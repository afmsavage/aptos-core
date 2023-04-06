// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{executor::RAYON_EXEC_POOL, task::Transaction};
use aptos_aggregator::delta_change_set::{deserialize};
use aptos_mvhashmap::versioned_data::VersionedData;
use aptos_vm_types::{remote_cache::TStateViewWithRemoteCache, write::{AptosWrite, Op}};

pub(crate) struct OutputDeltaResolver<T: Transaction> {
    versioned_outputs: VersionedData<T::Key, T::Value>,
}

impl<T: Transaction> OutputDeltaResolver<T> {
    pub fn new(versioned_outputs: VersionedData<T::Key, T::Value>) -> Self {
        Self { versioned_outputs }
    }

    /// Takes Self, vector of all involved aggregator keys (each with at least one
    /// delta to resolve in the output), resolved values from storage for each key,
    /// and blocksize, and returns a Vec of materialized deltas per transaction index.
    pub(crate) fn resolve(
        self,
        base_view: &impl TStateViewWithRemoteCache<CommonKey = T::Key>,
        block_size: usize,
    ) -> Vec<Vec<(T::Key, Op<AptosWrite>)>> {
        let mut ret: Vec<Vec<(T::Key, Op<AptosWrite>)>> = vec![vec![]; block_size];

        // TODO: with more deltas, re-use executor threads and process in parallel.
        for key in self.versioned_outputs.take_aggregator_keys() {
            for (idx, value) in self.versioned_outputs.take_materialized_deltas(
                &key,
                base_view
                    .get_state_value_bytes(&key)
                    .ok() // Was anything found in storage
                    .and_then(|value| value.map(|bytes| deserialize(&bytes))),
            ) {
                ret[idx as usize].push((key.clone(), Op::Modification(AptosWrite::AggregatorValue(value))));
            }
        }

        RAYON_EXEC_POOL.spawn(move || drop(self));

        ret
    }
}
