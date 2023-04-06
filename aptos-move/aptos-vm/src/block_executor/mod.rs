// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

pub(crate) mod vm_wrapper;

use crate::{
    adapter_common::{preprocess_transaction, PreprocessedTransaction},
    block_executor::vm_wrapper::AptosExecutorTask,
    counters::{
        BLOCK_EXECUTOR_CONCURRENCY, BLOCK_EXECUTOR_EXECUTE_BLOCK_SECONDS,
        BLOCK_EXECUTOR_SIGNATURE_VERIFICATION_SECONDS,
    },
    AptosVM,
};
use aptos_block_executor::{
    errors::Error,
    executor::{BlockExecutor, RAYON_EXEC_POOL},
    task::{
        Transaction as BlockExecutorTransaction,
        TransactionOutput as BlockExecutorTransactionOutput,
    },
};
use aptos_types::{
    state_store::state_key::StateKey,
    transaction::{Transaction, TransactionOutput, TransactionStatus},
};
use aptos_vm_logging::{flush_speculative_logs, init_speculative_logs};
use move_core_types::vm_status::VMStatus;
use rayon::prelude::*;
use std::time::Instant;
use aptos_vm_types::change_set::{AptosChangeSet, ChangeSet};
use aptos_vm_types::delta::DeltaOp;
use aptos_vm_types::remote_cache::StateViewWithRemoteCache;
use aptos_vm_types::transaction_output::VMTransactionOutput;
use aptos_vm_types::write::{AptosWrite, Op};

impl BlockExecutorTransaction for PreprocessedTransaction {
    type Key = StateKey;
    type Value = Op<AptosWrite>;
}

// Wrapper to avoid orphan rule
#[derive(PartialEq, Debug)]
pub(crate) struct AptosTransactionOutput(VMTransactionOutput);

impl AptosTransactionOutput {
    pub fn new(output: VMTransactionOutput) -> Self {
        Self(output)
    }

    pub fn into(self) -> VMTransactionOutput {
        self.0
    }
}

impl BlockExecutorTransactionOutput for AptosTransactionOutput {
    type Txn = PreprocessedTransaction;

    // TODO can we avoid clones here?
    fn get_writes(&self) -> Vec<(StateKey, Op<AptosWrite>)> {
        self.0
            .writes()
            .iter()
            .map(|(key, op)| (key.clone(), op.clone()))
            .collect()
    }

    fn get_deltas(&self) -> Vec<(StateKey, DeltaOp)> {
        self.0
            .deltas()
            .iter()
            .map(|(key, op)| (key.clone(), *op))
            .collect()
    }

    /// Execution output for transactions that comes after SkipRest signal.
    fn skip_output() -> Self {
        Self(VMTransactionOutput::new(
            ChangeSet::empty(),
            ChangeSet::empty(),
            vec![],
            0,
            TransactionStatus::Retry,
        ))
    }
}

pub struct BlockAptosVM();

impl BlockAptosVM {
    pub fn execute_block<S: StateViewWithRemoteCache + Sync>(
        transactions: Vec<Transaction>,
        state_view: &S,
        concurrency_level: usize,
    ) -> Result<Vec<TransactionOutput>, VMStatus> {
        let _timer = BLOCK_EXECUTOR_EXECUTE_BLOCK_SECONDS.start_timer();
        // Verify the signatures of all the transactions in parallel.
        // This is time consuming so don't wait and do the checking
        // sequentially while executing the transactions.
        let signature_verification_timer =
            BLOCK_EXECUTOR_SIGNATURE_VERIFICATION_SECONDS.start_timer();
        let signature_verified_block: Vec<PreprocessedTransaction> =
            RAYON_EXEC_POOL.install(|| {
                transactions
                    .into_par_iter()
                    .with_min_len(25)
                    .map(preprocess_transaction::<AptosVM>)
                    .collect()
            });
        drop(signature_verification_timer);

        init_speculative_logs(signature_verified_block.len());

        BLOCK_EXECUTOR_CONCURRENCY.set(concurrency_level as i64);
        let executor = BlockExecutor::<PreprocessedTransaction, AptosExecutorTask<S>, S>::new(
            concurrency_level,
        );

        let ret = executor
            .execute_block(state_view, signature_verified_block, state_view)
            .map(|results| {
                // Process the outputs in parallel, combining delta writes with other writes.
                RAYON_EXEC_POOL.install(|| {
                    results
                        .into_par_iter()
                        .map(|(output, delta_writes)| {
                            let (mut writes, deltas, events, gas_used, status) = output.into().unpack();

                            // We should have a delta write for every delta in the output.
                            assert_eq!(deltas.len(), delta_writes.len());

                            AptosChangeSet::extend_with_writes(&mut writes, &mut ChangeSet::empty(), ChangeSet::new(delta_writes)).expect("should not fail");
                            TransactionOutput::new(
                                AptosChangeSet::into_write_set(writes).expect("should not fail"),
                                events,
                                gas_used,
                                status,
                            )
                        })
                        .collect()
                })
            });

        flush_speculative_logs();

        match ret {
            Ok(outputs) => Ok(outputs),
            Err(Error::ModulePathReadWrite) => {
                unreachable!("[Execution]: Must be handled by sequential fallback")
            },
            Err(Error::UserError(err)) => Err(err),
        }
    }

    pub fn execute_block_benchmark<S: StateViewWithRemoteCache + Sync>(
        transactions: Vec<Transaction>,
        state_view: &S,
        concurrency_level: usize,
    ) -> (usize, usize) {
        // Verify the signatures of all the transactions in parallel.
        // This is time consuming so don't wait and do the checking
        // sequentially while executing the transactions.
        let signature_verified_block: Vec<PreprocessedTransaction> =
            RAYON_EXEC_POOL.install(|| {
                transactions
                    .clone()
                    .into_par_iter()
                    .with_min_len(25)
                    .map(preprocess_transaction::<AptosVM>)
                    .collect()
            });
        let signature_verified_block_for_seq: Vec<PreprocessedTransaction> = RAYON_EXEC_POOL
            .install(|| {
                transactions
                    .into_par_iter()
                    .with_min_len(25)
                    .map(preprocess_transaction::<AptosVM>)
                    .collect()
            });
        let block_size = signature_verified_block.len();

        init_speculative_logs(signature_verified_block.len());

        BLOCK_EXECUTOR_CONCURRENCY.set(concurrency_level as i64);
        let executor = BlockExecutor::<PreprocessedTransaction, AptosExecutorTask<S>, S>::new(
            concurrency_level,
        );
        println!("Parallel execution starts...");
        let timer = Instant::now();
        let ret = executor.execute_block(state_view, signature_verified_block, state_view);
        let exec_t = timer.elapsed();
        println!(
            "Parallel execution finishes, TPS = {}",
            block_size * 1000 / exec_t.as_millis() as usize
        );

        flush_speculative_logs();

        // sequentially execute the block and check if the results match
        let seq_executor =
            BlockExecutor::<PreprocessedTransaction, AptosExecutorTask<S>, S>::new(1);
        println!("Sequential execution starts...");
        let seq_timer = Instant::now();
        let seq_ret =
            seq_executor.execute_block(state_view, signature_verified_block_for_seq, state_view);
        let seq_exec_t = seq_timer.elapsed();
        println!(
            "Sequential execution finishes, TPS = {}",
            block_size * 1000 / seq_exec_t.as_millis() as usize
        );

        assert_eq!(ret, seq_ret);

        drop(ret);
        drop(seq_ret);

        (
            block_size * 1000 / exec_t.as_millis() as usize,
            block_size * 1000 / seq_exec_t.as_millis() as usize,
        )
    }
}
