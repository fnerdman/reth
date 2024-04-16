#[cfg(not(feature = "optimism"))]
use revm::DatabaseCommit;
use revm::{
    db::StateDBBox,
    inspector_handle_register,
    interpreter::Host,
    primitives::{CfgEnvWithHandlerCfg, ResultAndState},
    Evm, State,
};
use std::{sync::Arc, time::Instant};
#[cfg(not(feature = "optimism"))]
use tracing::{debug, trace};

use reth_evm::ConfigureEvm;
use reth_interfaces::executor::{BlockExecutionError, BlockValidationError};
#[cfg(feature = "optimism")]
use reth_primitives::revm::env::fill_op_tx_env;
#[cfg(not(feature = "optimism"))]
use reth_primitives::revm::env::fill_tx_env;
use reth_primitives::{
    Address, Block, BlockNumber, BlockWithSenders, Bloom, ChainSpec, GotExpected, Hardfork, Header,
    PruneModes, Receipt, ReceiptWithBloom, Receipts, TransactionSigned, Withdrawals, B256, U256,
};
#[cfg(not(feature = "optimism"))]
use reth_provider::BundleStateWithReceipts;
use reth_provider::{
    BlockExecutor, BlockExecutorStats, ProviderError, PrunableBlockExecutor, StateProvider,
};

use crate::{
    batch::BlockBatchRecord,
    database::StateProviderDatabase,
    eth_dao_fork::{DAO_HARDFORK_BENEFICIARY, DAO_HARDKFORK_ACCOUNTS},
    stack::{InspectorStack, InspectorStackConfig},
    state_change::{apply_beacon_root_contract_call, post_block_balance_increments},
};

/// EVMProcessor is a block executor that uses revm to execute blocks or multiple blocks.
///
/// Output is obtained by calling `take_output_state` function.
///
/// It is capable of pruning the data that will be written to the database
/// and implemented [PrunableBlockExecutor] traits.
///
/// It implemented the [BlockExecutor] that give it the ability to take block
/// apply pre state (Cancun system contract call), execute transaction and apply
/// state change and then apply post execution changes (block reward, withdrawals, irregular DAO
/// hardfork state change). And if `execute_and_verify_receipt` is called it will verify the
/// receipt.
///
/// InspectorStack are used for optional inspecting execution. And it contains
/// various duration of parts of execution.
#[allow(missing_debug_implementations)]
pub struct EVMProcessor<'a, EvmConfig> {
    /// The configured chain-spec
    pub(crate) chain_spec: Arc<ChainSpec>,
    /// revm instance that contains database and env environment.
    pub(crate) evm: Evm<'a, InspectorStack, StateDBBox<'a, ProviderError>>,
    /// Keeps track of the recorded receipts and pruning configuration.
    pub(crate) batch_record: BlockBatchRecord,
    /// Execution stats
    pub(crate) stats: BlockExecutorStats,
    /// The type that is able to configure the EVM environment.
    _evm_config: EvmConfig,
}

impl<'a, EvmConfig> EVMProcessor<'a, EvmConfig>
where
    EvmConfig: ConfigureEvm,
{
    /// Return chain spec.
    pub fn chain_spec(&self) -> &Arc<ChainSpec> {
        &self.chain_spec
    }

    /// Creates a new executor from the given chain spec and database.
    pub fn new_with_db<DB: StateProvider + 'a>(
        chain_spec: Arc<ChainSpec>,
        db: StateProviderDatabase<DB>,
        evm_config: EvmConfig,
    ) -> Self {
        let state = State::builder()
            .with_database_boxed(Box::new(db))
            .with_bundle_update()
            .without_state_clear()
            .build();
        EVMProcessor::new_with_state(chain_spec, state, evm_config)
    }

    /// Create a new EVM processor with the given revm state.
    pub fn new_with_state(
        chain_spec: Arc<ChainSpec>,
        revm_state: StateDBBox<'a, ProviderError>,
        evm_config: EvmConfig,
    ) -> Self {
        let stack = InspectorStack::new(InspectorStackConfig::default());
        let evm = evm_config.evm_with_inspector(revm_state, stack);
        EVMProcessor {
            chain_spec,
            evm,
            batch_record: BlockBatchRecord::default(),
            stats: BlockExecutorStats::default(),
            _evm_config: evm_config,
        }
    }

    /// Configures the executor with the given inspectors.
    pub fn set_stack(&mut self, stack: InspectorStack) {
        self.evm.context.external = stack;
    }

    /// Configure the executor with the given block.
    pub fn set_first_block(&mut self, num: BlockNumber) {
        self.batch_record.set_first_block(num);
    }

    /// Saves the receipts to the batch record.
    pub fn save_receipts(&mut self, receipts: Vec<Receipt>) -> Result<(), BlockExecutionError> {
        self.batch_record.save_receipts(receipts)
    }

    /// Returns the recorded receipts.
    pub fn receipts(&self) -> &Receipts {
        self.batch_record.receipts()
    }

    /// Returns a reference to the database
    pub fn db_mut(&mut self) -> &mut StateDBBox<'a, ProviderError> {
        &mut self.evm.context.evm.db
    }

    /// Initializes the config and block env.
    pub(crate) fn init_env(&mut self, header: &Header, total_difficulty: U256) {
        // Set state clear flag.
        let state_clear_flag =
            self.chain_spec.fork(Hardfork::SpuriousDragon).active_at_block(header.number);

        self.db_mut().set_state_clear_flag(state_clear_flag);

        let mut cfg =
            CfgEnvWithHandlerCfg::new_with_spec_id(self.evm.cfg().clone(), self.evm.spec_id());
        EvmConfig::fill_cfg_and_block_env(
            &mut cfg,
            self.evm.block_mut(),
            &self.chain_spec,
            header,
            total_difficulty,
        );
        *self.evm.cfg_mut() = cfg.cfg_env;

        // This will update the spec in case it changed
        self.evm.modify_spec_id(cfg.handler_cfg.spec_id);
    }

    /// Applies the pre-block call to the EIP-4788 beacon block root contract.
    ///
    /// If cancun is not activated or the block is the genesis block, then this is a no-op, and no
    /// state changes are made.
    fn apply_beacon_root_contract_call(
        &mut self,
        block: &Block,
    ) -> Result<(), BlockExecutionError> {
        apply_beacon_root_contract_call(
            &self.chain_spec,
            block.timestamp,
            block.number,
            block.parent_beacon_block_root,
            &mut self.evm,
        )?;
        Ok(())
    }

    /// Apply post execution state changes, including block rewards, withdrawals, and irregular DAO
    /// hardfork state change.
    pub fn apply_post_execution_state_change(
        &mut self,
        block: &Block,
        total_difficulty: U256,
    ) -> Result<(), BlockExecutionError> {
        let mut balance_increments = post_block_balance_increments(
            &self.chain_spec,
            block.number,
            block.difficulty,
            block.beneficiary,
            block.timestamp,
            total_difficulty,
            &block.ommers,
            block.withdrawals.as_ref().map(Withdrawals::as_ref),
        );

        // Irregular state change at Ethereum DAO hardfork
        if self.chain_spec.fork(Hardfork::Dao).transitions_at_block(block.number) {
            // drain balances from hardcoded addresses.
            let drained_balance: u128 = self
                .db_mut()
                .drain_balances(DAO_HARDKFORK_ACCOUNTS)
                .map_err(|_| BlockValidationError::IncrementBalanceFailed)?
                .into_iter()
                .sum();

            // return balance to DAO beneficiary.
            *balance_increments.entry(DAO_HARDFORK_BENEFICIARY).or_default() += drained_balance;
        }
        // increment balances
        self.db_mut()
            .increment_balances(balance_increments)
            .map_err(|_| BlockValidationError::IncrementBalanceFailed)?;

        Ok(())
    }

    /// Runs a single transaction in the configured environment and proceeds
    /// to return the result and state diff (without applying it).
    ///
    /// Assumes the rest of the block environment has been filled via `init_block_env`.
    pub fn transact(
        &mut self,
        transaction: &TransactionSigned,
        sender: Address,
    ) -> Result<ResultAndState, BlockExecutionError> {
        // Fill revm structure.
        #[cfg(not(feature = "optimism"))]
        fill_tx_env(self.evm.tx_mut(), transaction, sender);

        #[cfg(feature = "optimism")]
        {
            let mut envelope_buf = Vec::with_capacity(transaction.length_without_header());
            transaction.encode_enveloped(&mut envelope_buf);
            fill_op_tx_env(self.evm.tx_mut(), transaction, sender, envelope_buf.into());
        }

        let hash = transaction.hash_ref();
        let should_inspect = self.evm.context.external.should_inspect(self.evm.env(), hash);
        let out = if should_inspect {
            // push inspector handle register.
            self.evm.handler.append_handler_register_plain(inspector_handle_register);
            let output = self.evm.transact();
            tracing::trace!(
                target: "evm",
                %hash, ?output, ?transaction, env = ?self.evm.context.evm.env,
                "Executed transaction"
            );
            // pop last handle register
            self.evm.handler.pop_handle_register();
            output
        } else {
            // Main execution without needing the hash
            self.evm.transact()
        };

        out.map_err(move |e| {
            // Ensure hash is calculated for error log, if not already done
            BlockValidationError::EVM { hash: transaction.recalculate_hash(), error: e.into() }
                .into()
        })
    }

    /// Execute the block, verify gas usage and apply post-block state changes.
    pub(crate) fn execute_inner(
        &mut self,
        block: &BlockWithSenders,
        total_difficulty: U256,
    ) -> Result<Vec<Receipt>, BlockExecutionError> {
        self.init_env(&block.header, total_difficulty);
        self.apply_beacon_root_contract_call(block)?;
        let (receipts, cumulative_gas_used) = self.execute_transactions(block, total_difficulty)?;

        // Check if gas used matches the value set in header.
        if block.gas_used != cumulative_gas_used {
            let receipts = Receipts::from_block_receipt(receipts);
            return Err(BlockValidationError::BlockGasUsed {
                gas: GotExpected { got: cumulative_gas_used, expected: block.gas_used },
                gas_spent_by_tx: receipts.gas_spent_by_tx()?,
            }
            .into())
        }
        let time = Instant::now();
        self.apply_post_execution_state_change(block, total_difficulty)?;
        self.stats.apply_post_execution_state_changes_duration += time.elapsed();

        let time = Instant::now();
        let retention = self.batch_record.bundle_retention(block.number);
        self.db_mut().merge_transitions(retention);
        self.stats.merge_transitions_duration += time.elapsed();

        if self.batch_record.first_block().is_none() {
            self.batch_record.set_first_block(block.number);
        }

        Ok(receipts)
    }
}

/// Default Ethereum implementation of the [BlockExecutor] trait for the [EVMProcessor].
#[cfg(not(feature = "optimism"))]
impl<'a, EvmConfig> BlockExecutor for EVMProcessor<'a, EvmConfig>
where
    EvmConfig: ConfigureEvm,
{
    type Error = BlockExecutionError;

    fn execute_and_verify_receipt(
        &mut self,
        block: &BlockWithSenders,
        total_difficulty: U256,
    ) -> Result<(), BlockExecutionError> {
        // execute block
        let receipts = self.execute_inner(block, total_difficulty)?;

        // TODO Before Byzantium, receipts contained state root that would mean that expensive
        // operation as hashing that is needed for state root got calculated in every
        // transaction This was replaced with is_success flag.
        // See more about EIP here: https://eips.ethereum.org/EIPS/eip-658
        if self.chain_spec.fork(Hardfork::Byzantium).active_at_block(block.header.number) {
            let time = Instant::now();
            if let Err(error) =
                verify_receipt(block.header.receipts_root, block.header.logs_bloom, receipts.iter())
            {
                debug!(target: "evm", %error, ?receipts, "receipts verification failed");
                return Err(error)
            };
            self.stats.receipt_root_duration += time.elapsed();
        }

        self.batch_record.save_receipts(receipts)?;
        Ok(())
    }

    fn execute_transactions(
        &mut self,
        block: &BlockWithSenders,
        total_difficulty: U256,
    ) -> Result<(Vec<Receipt>, u64), BlockExecutionError> {
        self.init_env(&block.header, total_difficulty);

        // perf: do not execute empty blocks
        if block.body.is_empty() {
            return Ok((Vec::new(), 0))
        }

        let mut cumulative_gas_used = 0;
        let mut receipts = Vec::with_capacity(block.body.len());
        for (sender, transaction) in block.transactions_with_sender() {
            let time = Instant::now();
            // The sum of the transaction’s gas limit, Tg, and the gas utilized in this block prior,
            // must be no greater than the block’s gasLimit.
            let block_available_gas = block.header.gas_limit - cumulative_gas_used;
            if transaction.gas_limit() > block_available_gas {
                return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                    transaction_gas_limit: transaction.gas_limit(),
                    block_available_gas,
                }
                .into())
            }
            // Execute transaction.
            let ResultAndState { result, state } = self.transact(transaction, *sender)?;
            trace!(
                target: "evm",
                ?transaction, ?result, ?state,
                "Executed transaction"
            );
            self.stats.execution_duration += time.elapsed();
            let time = Instant::now();

            self.db_mut().commit(state);

            self.stats.apply_state_duration += time.elapsed();

            // append gas used
            cumulative_gas_used += result.gas_used();

            // Push transaction changeset and calculate header bloom filter for receipt.
            receipts.push(Receipt {
                tx_type: transaction.tx_type(),
                // Success flag was added in `EIP-658: Embedding transaction status code in
                // receipts`.
                success: result.is_success(),
                cumulative_gas_used,
                // convert to reth log
                logs: result.into_logs().into_iter().map(Into::into).collect(),
            });
        }

        Ok((receipts, cumulative_gas_used))
    }

    fn take_output_state(&mut self) -> BundleStateWithReceipts {
        self.stats.log_debug();
        BundleStateWithReceipts::new(
            self.evm.context.evm.db.take_bundle(),
            self.batch_record.take_receipts(),
            self.batch_record.first_block().unwrap_or_default(),
        )
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.evm.context.evm.db.bundle_size_hint())
    }
}

impl<'a, EvmConfig> PrunableBlockExecutor for EVMProcessor<'a, EvmConfig>
where
    EvmConfig: ConfigureEvm,
{
    fn set_tip(&mut self, tip: BlockNumber) {
        self.batch_record.set_tip(tip);
    }

    fn set_prune_modes(&mut self, prune_modes: PruneModes) {
        self.batch_record.set_prune_modes(prune_modes);
    }
}

/// Calculate the receipts root, and copmare it against against the expected receipts root and logs
/// bloom.
pub fn verify_receipt<'a>(
    expected_receipts_root: B256,
    expected_logs_bloom: Bloom,
    receipts: impl Iterator<Item = &'a Receipt> + Clone,
) -> Result<(), BlockExecutionError> {
    // Calculate receipts root.
    let receipts_with_bloom = receipts.map(|r| r.clone().into()).collect::<Vec<ReceiptWithBloom>>();
    let receipts_root = reth_primitives::proofs::calculate_receipt_root(&receipts_with_bloom);

    // Create header log bloom.
    let logs_bloom = receipts_with_bloom.iter().fold(Bloom::ZERO, |bloom, r| bloom | r.bloom);

    compare_receipts_root_and_logs_bloom(
        receipts_root,
        logs_bloom,
        expected_receipts_root,
        expected_logs_bloom,
    )?;

    Ok(())
}

/// Compare the calculated receipts root with the expected receipts root, also copmare
/// the calculated logs bloom with the expected logs bloom.
pub fn compare_receipts_root_and_logs_bloom(
    calculated_receipts_root: B256,
    calculated_logs_bloom: Bloom,
    expected_receipts_root: B256,
    expected_logs_bloom: Bloom,
) -> Result<(), BlockExecutionError> {
    if calculated_receipts_root != expected_receipts_root {
        return Err(BlockValidationError::ReceiptRootDiff(
            GotExpected { got: calculated_receipts_root, expected: expected_receipts_root }.into(),
        )
        .into())
    }

    if calculated_logs_bloom != expected_logs_bloom {
        return Err(BlockValidationError::BloomLogDiff(
            GotExpected { got: calculated_logs_bloom, expected: expected_logs_bloom }.into(),
        )
        .into())
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use revm::{Database, TransitionState};

    use reth_primitives::{
        bytes,
        constants::{BEACON_ROOTS_ADDRESS, EIP1559_INITIAL_BASE_FEE, SYSTEM_ADDRESS},
        keccak256, Account, Bytes, ChainSpecBuilder, ForkCondition, Signature, Transaction,
        TransactionKind, TxEip1559, MAINNET,
    };

    use crate::test_utils::{StateProviderTest, TestEvmConfig};

    use super::*;

    static BEACON_ROOT_CONTRACT_CODE: Bytes = bytes!("3373fffffffffffffffffffffffffffffffffffffffe14604d57602036146024575f5ffd5b5f35801560495762001fff810690815414603c575f5ffd5b62001fff01545f5260205ff35b5f5ffd5b62001fff42064281555f359062001fff015500");

    fn create_state_provider_with_beacon_root_contract() -> StateProviderTest {
        let mut db = StateProviderTest::default();

        let beacon_root_contract_account = Account {
            balance: U256::ZERO,
            bytecode_hash: Some(keccak256(BEACON_ROOT_CONTRACT_CODE.clone())),
            nonce: 1,
        };

        db.insert_account(
            BEACON_ROOTS_ADDRESS,
            beacon_root_contract_account,
            Some(BEACON_ROOT_CONTRACT_CODE.clone()),
            HashMap::new(),
        );

        db
    }

    #[test]
    fn eip_4788_non_genesis_call() {
        let mut header =
            Header { timestamp: 1, number: 1, excess_blob_gas: Some(0), ..Header::default() };

        let db = create_state_provider_with_beacon_root_contract();

        let chain_spec = Arc::new(
            ChainSpecBuilder::from(&*MAINNET)
                .shanghai_activated()
                .with_fork(Hardfork::Cancun, ForkCondition::Timestamp(1))
                .build(),
        );

        // execute invalid header (no parent beacon block root)
        let mut executor = EVMProcessor::new_with_db(
            chain_spec,
            StateProviderDatabase::new(db),
            TestEvmConfig::default(),
        );

        // attempt to execute a block without parent beacon block root, expect err
        let err = executor
            .execute_and_verify_receipt(
                &BlockWithSenders {
                    block: Block {
                        header: header.clone(),
                        body: vec![],
                        ommers: vec![],
                        withdrawals: None,
                    },
                    senders: vec![],
                },
                U256::ZERO,
            )
            .expect_err(
                "Executing cancun block without parent beacon block root field should fail",
            );
        assert_eq!(
            err,
            BlockExecutionError::Validation(BlockValidationError::MissingParentBeaconBlockRoot)
        );

        // fix header, set a gas limit
        header.parent_beacon_block_root = Some(B256::with_last_byte(0x69));

        // Now execute a block with the fixed header, ensure that it does not fail
        executor
            .execute_and_verify_receipt(
                &BlockWithSenders {
                    block: Block {
                        header: header.clone(),
                        body: vec![],
                        ommers: vec![],
                        withdrawals: None,
                    },
                    senders: vec![],
                },
                U256::ZERO,
            )
            .unwrap();

        // check the actual storage of the contract - it should be:
        // * The storage value at header.timestamp % HISTORY_BUFFER_LENGTH should be
        // header.timestamp
        // * The storage value at header.timestamp % HISTORY_BUFFER_LENGTH + HISTORY_BUFFER_LENGTH
        // should be parent_beacon_block_root
        let history_buffer_length = 8191u64;
        let timestamp_index = header.timestamp % history_buffer_length;
        let parent_beacon_block_root_index =
            timestamp_index % history_buffer_length + history_buffer_length;

        // get timestamp storage and compare
        let timestamp_storage =
            executor.db_mut().storage(BEACON_ROOTS_ADDRESS, U256::from(timestamp_index)).unwrap();
        assert_eq!(timestamp_storage, U256::from(header.timestamp));

        // get parent beacon block root storage and compare
        let parent_beacon_block_root_storage = executor
            .db_mut()
            .storage(BEACON_ROOTS_ADDRESS, U256::from(parent_beacon_block_root_index))
            .expect("storage value should exist");
        assert_eq!(parent_beacon_block_root_storage, U256::from(0x69));
    }

    #[test]
    fn eip_4788_no_code_cancun() {
        // This test ensures that we "silently fail" when cancun is active and there is no code at
        // BEACON_ROOTS_ADDRESS
        let header = Header {
            timestamp: 1,
            number: 1,
            parent_beacon_block_root: Some(B256::with_last_byte(0x69)),
            excess_blob_gas: Some(0),
            ..Header::default()
        };

        let db = StateProviderTest::default();

        // DON'T deploy the contract at genesis
        let chain_spec = Arc::new(
            ChainSpecBuilder::from(&*MAINNET)
                .shanghai_activated()
                .with_fork(Hardfork::Cancun, ForkCondition::Timestamp(1))
                .build(),
        );

        let mut executor = EVMProcessor::new_with_db(
            chain_spec,
            StateProviderDatabase::new(db),
            TestEvmConfig::default(),
        );
        executor.init_env(&header, U256::ZERO);

        // get the env
        let previous_env = executor.evm.context.evm.env.clone();

        // attempt to execute an empty block with parent beacon block root, this should not fail
        executor
            .execute_and_verify_receipt(
                &BlockWithSenders {
                    block: Block {
                        header: header.clone(),
                        body: vec![],
                        ommers: vec![],
                        withdrawals: None,
                    },
                    senders: vec![],
                },
                U256::ZERO,
            )
            .expect(
                "Executing a block with no transactions while cancun is active should not fail",
            );

        // ensure that the env has not changed
        assert_eq!(executor.evm.context.evm.env, previous_env);
    }

    #[test]
    fn eip_4788_empty_account_call() {
        // This test ensures that we do not increment the nonce of an empty SYSTEM_ADDRESS account
        // during the pre-block call

        let mut db = create_state_provider_with_beacon_root_contract();

        // insert an empty SYSTEM_ADDRESS
        db.insert_account(SYSTEM_ADDRESS, Account::default(), None, HashMap::new());

        let chain_spec = Arc::new(
            ChainSpecBuilder::from(&*MAINNET)
                .shanghai_activated()
                .with_fork(Hardfork::Cancun, ForkCondition::Timestamp(1))
                .build(),
        );

        let mut executor = EVMProcessor::new_with_db(
            chain_spec,
            StateProviderDatabase::new(db),
            TestEvmConfig::default(),
        );

        // construct the header for block one
        let header = Header {
            timestamp: 1,
            number: 1,
            parent_beacon_block_root: Some(B256::with_last_byte(0x69)),
            excess_blob_gas: Some(0),
            ..Header::default()
        };

        executor.init_env(&header, U256::ZERO);

        // attempt to execute an empty block with parent beacon block root, this should not fail
        executor
            .execute_and_verify_receipt(
                &BlockWithSenders {
                    block: Block {
                        header: header.clone(),
                        body: vec![],
                        ommers: vec![],
                        withdrawals: None,
                    },
                    senders: vec![],
                },
                U256::ZERO,
            )
            .expect(
                "Executing a block with no transactions while cancun is active should not fail",
            );

        // ensure that the nonce of the system address account has not changed
        let nonce = executor.db_mut().basic(SYSTEM_ADDRESS).unwrap().unwrap().nonce;
        assert_eq!(nonce, 0);
    }

    #[test]
    fn eip_4788_genesis_call() {
        let db = create_state_provider_with_beacon_root_contract();

        // activate cancun at genesis
        let chain_spec = Arc::new(
            ChainSpecBuilder::from(&*MAINNET)
                .shanghai_activated()
                .with_fork(Hardfork::Cancun, ForkCondition::Timestamp(0))
                .build(),
        );

        let mut header = chain_spec.genesis_header();

        let mut executor = EVMProcessor::new_with_db(
            chain_spec,
            StateProviderDatabase::new(db),
            TestEvmConfig::default(),
        );
        executor.init_env(&header, U256::ZERO);

        // attempt to execute the genesis block with non-zero parent beacon block root, expect err
        header.parent_beacon_block_root = Some(B256::with_last_byte(0x69));
        let _err = executor
            .execute_and_verify_receipt(
                &BlockWithSenders {
                    block: Block {
                        header: header.clone(),
                        body: vec![],
                        ommers: vec![],
                        withdrawals: None,
                    },
                    senders: vec![],
                },
                U256::ZERO,
            )
            .expect_err(
                "Executing genesis cancun block with non-zero parent beacon block root field should fail",
            );

        // fix header
        header.parent_beacon_block_root = Some(B256::ZERO);

        // now try to process the genesis block again, this time ensuring that a system contract
        // call does not occur
        executor
            .execute_and_verify_receipt(
                &BlockWithSenders {
                    block: Block {
                        header: header.clone(),
                        body: vec![],
                        ommers: vec![],
                        withdrawals: None,
                    },
                    senders: vec![],
                },
                U256::ZERO,
            )
            .unwrap();

        // there is no system contract call so there should be NO STORAGE CHANGES
        // this means we'll check the transition state
        let state = executor.evm.context.evm.inner.db;
        let transition_state =
            state.transition_state.expect("the evm should be initialized with bundle updates");

        // assert that it is the default (empty) transition state
        assert_eq!(transition_state, TransitionState::default());
    }

    #[test]
    fn eip_4788_high_base_fee() {
        // This test ensures that if we have a base fee, then we don't return an error when the
        // system contract is called, due to the gas price being less than the base fee.
        let header = Header {
            timestamp: 1,
            number: 1,
            parent_beacon_block_root: Some(B256::with_last_byte(0x69)),
            base_fee_per_gas: Some(u64::MAX),
            excess_blob_gas: Some(0),
            ..Header::default()
        };

        let db = create_state_provider_with_beacon_root_contract();

        let chain_spec = Arc::new(
            ChainSpecBuilder::from(&*MAINNET)
                .shanghai_activated()
                .with_fork(Hardfork::Cancun, ForkCondition::Timestamp(1))
                .build(),
        );

        // execute header
        let mut executor = EVMProcessor::new_with_db(
            chain_spec,
            StateProviderDatabase::new(db),
            TestEvmConfig::default(),
        );
        executor.init_env(&header, U256::ZERO);

        // ensure that the env is configured with a base fee
        assert_eq!(executor.evm.block().basefee, U256::from(u64::MAX));

        // Now execute a block with the fixed header, ensure that it does not fail
        executor
            .execute_and_verify_receipt(
                &BlockWithSenders {
                    block: Block {
                        header: header.clone(),
                        body: vec![],
                        ommers: vec![],
                        withdrawals: None,
                    },
                    senders: vec![],
                },
                U256::ZERO,
            )
            .unwrap();

        // check the actual storage of the contract - it should be:
        // * The storage value at header.timestamp % HISTORY_BUFFER_LENGTH should be
        // header.timestamp
        // * The storage value at header.timestamp % HISTORY_BUFFER_LENGTH + HISTORY_BUFFER_LENGTH
        // should be parent_beacon_block_root
        let history_buffer_length = 8191u64;
        let timestamp_index = header.timestamp % history_buffer_length;
        let parent_beacon_block_root_index =
            timestamp_index % history_buffer_length + history_buffer_length;

        // get timestamp storage and compare
        let timestamp_storage =
            executor.db_mut().storage(BEACON_ROOTS_ADDRESS, U256::from(timestamp_index)).unwrap();
        assert_eq!(timestamp_storage, U256::from(header.timestamp));

        // get parent beacon block root storage and compare
        let parent_beacon_block_root_storage = executor
            .db_mut()
            .storage(BEACON_ROOTS_ADDRESS, U256::from(parent_beacon_block_root_index))
            .unwrap();
        assert_eq!(parent_beacon_block_root_storage, U256::from(0x69));
    }

    #[test]
    fn test_transact_error_includes_correct_hash() {
        let chain_spec = Arc::new(
            ChainSpecBuilder::from(&*MAINNET)
                .shanghai_activated()
                .with_fork(Hardfork::Cancun, ForkCondition::Timestamp(1))
                .build(),
        );

        let db = StateProviderTest::default();
        let chain_id = chain_spec.chain.id();

        // execute header
        let mut executor = EVMProcessor::new_with_db(
            chain_spec,
            StateProviderDatabase::new(db),
            TestEvmConfig::default(),
        );

        // Create a test transaction that gonna fail
        let transaction = TransactionSigned::from_transaction_and_signature(
            Transaction::Eip1559(TxEip1559 {
                chain_id,
                nonce: 1,
                gas_limit: 21_000,
                to: TransactionKind::Call(Address::ZERO),
                max_fee_per_gas: EIP1559_INITIAL_BASE_FEE as u128,
                ..Default::default()
            }),
            Signature::default(),
        );

        let result = executor.transact(&transaction, Address::random());

        let expected_hash = transaction.recalculate_hash();

        // Check the error
        match result {
            Err(BlockExecutionError::Validation(BlockValidationError::EVM { hash, error: _ })) => {
                    assert_eq!(hash, expected_hash, "The EVM error does not include the correct transaction hash.");
            },
            _ => panic!("Expected a BlockExecutionError::Validation error, but transaction did not fail as expected."),
        }
    }
}
