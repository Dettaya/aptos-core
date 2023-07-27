// Copyright © Aptos Foundation

use std::collections::BTreeSet;
use std::iter::Chain;
use std::slice::Iter;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};
use dashmap::{DashMap, DashSet};
use itertools::Itertools;
use once_cell::sync::Lazy;
use rayon::prelude::{IntoParallelIterator, IntoParallelRefIterator, IntoParallelRefMutIterator};
use aptos_metrics_core::{HistogramVec, register_histogram_vec};
use aptos_types::block_executor::partitioner::{CrossShardDependencies, ShardedTxnIndex, SubBlock, SubBlocksForShard, TransactionWithDependencies};
use aptos_types::state_store::state_key::StateKey;
use aptos_types::transaction::analyzed_transaction::{AnalyzedTransaction, StorageLocation};
use aptos_types::transaction::Transaction;
use move_core_types::account_address::AccountAddress;
use crate::{add_edges, BlockPartitioner, get_anchor_shard_id};
use aptos_metrics_core::exponential_buckets;
use rayon::iter::ParallelIterator;
use rayon::{ThreadPool, ThreadPoolBuilder};
use std::cmp;
use std::ops::Deref;
use aptos_crypto::hash::CryptoHash;
use serde::{Deserialize, Serialize};
use storage_location_helper::StorageLocationHelper;

type Sender = Option<AccountAddress>;

mod storage_location_helper;

pub struct OmegaPartitioner {
    thread_pool: ThreadPool,
}

impl OmegaPartitioner {
    pub fn new(num_threads: usize) -> Self {
        Self {
            thread_pool: ThreadPoolBuilder::new().num_threads(num_threads).build().unwrap()
        }
    }

    fn add_edges(
        &self,
        txns: &Vec<Mutex<Option<AnalyzedTransaction>>>,
        txn_id_matrix: &Vec<Vec<Vec<usize>>>,
        helpers: &DashMap<usize, RwLock<StorageLocationHelper>>,
    ) -> Vec<SubBlocksForShard<AnalyzedTransaction>>{
        let num_txns = txns.len();
        let num_rounds = txn_id_matrix.len();
        let num_shards = txn_id_matrix.first().unwrap().len();

        let mut global_txn_counter: usize = 0;
        let mut new_indices: Vec<usize> = vec![0; num_txns];

        let mut start_index_matrix: Vec<Vec<usize>> = vec![vec![0; num_shards]; num_rounds];
        for (round_id, row) in txn_id_matrix.iter().enumerate() {
            for (shard_id, txn_ids) in row.iter().enumerate() {
                let num_txns_in_cur_sub_block = txn_ids.len();
                for (pos_inside_sub_block, txn_id) in txn_ids.iter().enumerate() {
                    let new_index = global_txn_counter + pos_inside_sub_block;
                    new_indices[*txn_id] = new_index;
                }
                start_index_matrix[round_id][shard_id] = global_txn_counter;
                global_txn_counter += num_txns_in_cur_sub_block;
            }
        }

        let mut sub_block_matrix: Vec<Vec<Mutex<Option<SubBlock<AnalyzedTransaction>>>>> = Vec::with_capacity(num_rounds);
        for _round_id in 0..num_rounds {
            let mut row = Vec::with_capacity(num_shards);
            for shard_id in 0..num_shards {
                row.push(Mutex::new(None));
            }
            sub_block_matrix.push(row);
        }

        self.thread_pool.install(||{
            (0..num_rounds).into_par_iter().for_each(|round_id| {
                (0..num_shards).into_par_iter().for_each(|shard_id| {
                    let cur_sub_block_size = txn_id_matrix[round_id][shard_id].len();
                    let mut twds: Vec<TransactionWithDependencies<AnalyzedTransaction>> = Vec::with_capacity(cur_sub_block_size);
                    (0..cur_sub_block_size).into_iter().for_each(|pos_in_sub_block|{
                        let txn_id = txn_id_matrix[round_id][shard_id][pos_in_sub_block];
                        let txn = txns[txn_id].lock().unwrap().take().unwrap();
                        let mut deps = CrossShardDependencies::default();
                        for loc in txn.write_hints.iter().chain(txn.read_hints.iter()) {
                            let loc_id = *loc.maybe_id_in_partition_session.as_ref().unwrap();
                            let helper_ref = helpers.get(&loc_id).unwrap();
                            let helper = helper_ref.read().unwrap();
                            if let Some(fat_id) = helper.promoted_writer_ids.range(..TxnFatId::new(round_id, shard_id, 0)).last() {
                                let src_txn_idx_fat = ShardedTxnIndex {
                                    txn_index: new_indices[fat_id.old_txn_idx],
                                    shard_id: fat_id.shard_id,
                                    round_id: fat_id.round_id,
                                };
                                deps.add_required_edge(src_txn_idx_fat, loc.clone());
                            }
                        }
                        for loc in txn.write_hints.iter() {
                            let loc_id = *loc.maybe_id_in_partition_session.as_ref().unwrap();
                            let helper_ref = helpers.get(&loc_id).unwrap();
                            let helper = helper_ref.read().unwrap();
                            let is_last_writer_in_cur_sub_block = helper.promoted_writer_ids.range(TxnFatId::new(round_id, shard_id, txn_id + 1)..TxnFatId::new(round_id, shard_id + 1, 0)).next().is_none();
                            if is_last_writer_in_cur_sub_block {
                                let mut end_id = TxnFatId::new(num_rounds, num_shards, 0); // Guaranteed to be invalid.
                                for follower_id in helper.promoted_txn_ids.range(TxnFatId::new(round_id, shard_id + 1, 0)..) {
                                    if *follower_id > end_id {
                                        break;
                                    }
                                    let dst_txn_idx_fat = ShardedTxnIndex {
                                        txn_index: new_indices[follower_id.old_txn_idx],
                                        shard_id: follower_id.shard_id,
                                        round_id: follower_id.round_id,
                                    };
                                    deps.add_dependent_edge(dst_txn_idx_fat, vec![loc.clone()]);
                                    if helper.writer_set.contains(&follower_id.old_txn_idx) {
                                        end_id = TxnFatId::new(follower_id.round_id, follower_id.shard_id + 1, 0);
                                    }
                                }
                            }
                        }
                        let twd = TransactionWithDependencies::new(txn, deps);
                        twds.push(twd);
                    });
                    let sub_block = SubBlock::new(start_index_matrix[round_id][shard_id], twds);
                    *sub_block_matrix[round_id][shard_id].lock().unwrap() = Some(sub_block);
                });
            });
        });

        let ret: Vec<SubBlocksForShard<AnalyzedTransaction>> = (0..num_shards).map(|shard_id|{
            let sub_blocks: Vec<SubBlock<AnalyzedTransaction>> = (0..num_rounds).map(|round_id|{
                sub_block_matrix[round_id][shard_id].lock().unwrap().take().unwrap()
            }).collect();
            SubBlocksForShard::new(shard_id, sub_blocks)
        }).collect();

        ret
    }

    fn discarding_round(
        &self,
        round_id: usize,
        txns: &Vec<AnalyzedTransaction>,
        txn_id_vecs: Vec<Vec<usize>>,
        loc_helpers: &DashMap<usize, RwLock<StorageLocationHelper>>,
        start_txn_id_by_shard_id: &Vec<usize>,
    ) -> (Vec<Vec<usize>>, Vec<Vec<usize>>) {
        println!("round_id={round_id}, txn_id_vecs={txn_id_vecs:?}");
        let num_shards = txn_id_vecs.len();

        let mut discarded: Vec<RwLock<Vec<usize>>> = Vec::with_capacity(num_shards);
        let mut potentially_accepted: Vec<RwLock<Vec<usize>>> = Vec::with_capacity(num_shards);
        let mut finally_accepted: Vec<RwLock<Vec<usize>>> = Vec::with_capacity(num_shards);
        for shard_id in 0..num_shards {
            potentially_accepted.push(RwLock::new(Vec::with_capacity(txn_id_vecs[shard_id].len())));
            finally_accepted.push(RwLock::new(Vec::with_capacity(txn_id_vecs[shard_id].len())));
            discarded.push(RwLock::new(Vec::with_capacity(txn_id_vecs[shard_id].len())));
        }

        let min_discarded_seq_nums_by_sender_id: DashMap<usize, AtomicUsize> = DashMap::new();
        let shard_id_and_txn_id_vec_pairs: Vec<(usize, Vec<usize>)> = txn_id_vecs.into_iter().enumerate().collect();

        self.thread_pool.install(||{
            shard_id_and_txn_id_vec_pairs.into_par_iter().for_each(|(my_shard_id, txn_ids)| {
                txn_ids.into_par_iter().for_each(|txn_id| {
                    let txn = txns.get(txn_id).unwrap();
                    let in_round_conflict_detected = txn.write_hints.iter().chain(txn.read_hints.iter()).any(|loc| {
                        let loc_id = *loc.maybe_id_in_partition_session.as_ref().unwrap();
                        let loc_helper = loc_helpers.get(&loc_id).unwrap();
                        let loc_helper_read = loc_helper.read().unwrap();
                        let anchor_shard_id = loc_helper_read.anchor_shard_id;
                        loc_helper_read.has_write_in_range(start_txn_id_by_shard_id[anchor_shard_id], start_txn_id_by_shard_id[my_shard_id])
                    });
                    if in_round_conflict_detected {
                        let sender_id = txn.maybe_sender_id_in_partition_session.unwrap();
                        min_discarded_seq_nums_by_sender_id.entry(sender_id).or_insert_with(|| AtomicUsize::new(usize::MAX)).value().fetch_min(txn_id, Ordering::SeqCst);
                        discarded[my_shard_id].write().unwrap().push(txn_id);
                    } else {
                        potentially_accepted[my_shard_id].write().unwrap().push(txn_id);
                    }
                });
            });

            (0..num_shards).into_par_iter().for_each(|shard_id|{
                potentially_accepted[shard_id].read().unwrap().par_iter().for_each(|txn_id|{
                    let txn = txns.get(*txn_id).unwrap();
                    let sender_id = txn.maybe_sender_id_in_partition_session.unwrap();
                    let min_discarded_txn_id = min_discarded_seq_nums_by_sender_id.entry(sender_id).or_insert_with(|| AtomicUsize::new(usize::MAX)).load(Ordering::SeqCst);
                    if *txn_id < min_discarded_txn_id {
                        for loc in txn.write_hints.iter().chain(txn.read_hints.iter()) {
                            let loc_id = *loc.maybe_id_in_partition_session.as_ref().unwrap();
                            loc_helpers.get(&loc_id).unwrap().write().unwrap().promote_txn_id(*txn_id, round_id, shard_id);
                        }
                        finally_accepted[shard_id].write().unwrap().push(*txn_id);
                    } else {
                        discarded[shard_id].write().unwrap().push(*txn_id);
                    }
                });
            });
        });

        (extract_and_sort(finally_accepted), extract_and_sort(discarded))
    }

}

impl BlockPartitioner for OmegaPartitioner {
    fn partition(&self, mut txns: Vec<AnalyzedTransaction>, num_executor_shards: usize) -> Vec<SubBlocksForShard<AnalyzedTransaction>> {
        let timer = OMEGA_PARTITIONER_MISC_TIMERS_SECONDS.with_label_values(&["preprocess"]).start_timer();
        let num_txns = txns.len();
        let mut num_senders = AtomicUsize::new(0);
        let mut num_keys = AtomicUsize::new(0);
        let shard_amount = std::env::var("OMEGA_PARTITIONER__DASHMAP_NUM_SHARDS").ok().map(|v|v.parse::<usize>().unwrap_or(256)).unwrap_or(256);
        let mut sender_ids_by_sender: DashMap<Sender, usize> = DashMap::with_shard_amount(shard_amount);
        let mut txn_counts_by_sender_id: DashMap<usize, AtomicUsize> = DashMap::with_shard_amount(shard_amount);
        let mut key_ids_by_key: DashMap<StateKey, usize> = DashMap::with_shard_amount(shard_amount);
        let mut helpers_by_key_id: DashMap<usize, RwLock<StorageLocationHelper>> = DashMap::with_shard_amount(shard_amount);
        for (txn_id, txn) in txns.iter_mut().enumerate() {
            txn.maybe_txn_id_in_partition_session = Some(txn_id);
        }
        txns.par_iter_mut().for_each(|mut txn| {
            let txn_id = *txn.maybe_txn_id_in_partition_session.as_ref().unwrap();
            let sender = txn.sender();
            let sender_id = *sender_ids_by_sender.entry(sender).or_insert_with(||{
                num_senders.fetch_add(1, Ordering::SeqCst)
            });
            txn_counts_by_sender_id.entry(sender_id).or_insert_with(|| AtomicUsize::new(0)).fetch_add(1, Ordering::SeqCst);
            txn.maybe_sender_id_in_partition_session = Some(sender_id);
            let num_writes = txn.write_hints.len();
            for (i, storage_location) in txn.write_hints.iter_mut().chain(txn.read_hints.iter_mut()).enumerate() {
                let key = storage_location.maybe_state_key().unwrap().clone();
                let key_id = *key_ids_by_key.entry(key).or_insert_with(|| {
                    num_keys.fetch_add(1, Ordering::SeqCst)
                });
                storage_location.maybe_id_in_partition_session = Some(key_id);
                let is_write = i < num_writes;
                helpers_by_key_id.entry(key_id).or_insert_with(|| {
                    let anchor_shard_id = get_anchor_shard_id(storage_location, num_executor_shards);
                    RwLock::new(StorageLocationHelper::new(anchor_shard_id))
                }).write().unwrap().add_candidate(txn_id, is_write);

            }
        });
        let duration = timer.stop_and_record();
        println!("omega_par/preprocess={duration:?}");

        print_storage_location_helper_summary(&key_ids_by_key, &helpers_by_key_id);

        let mut remaining_txns = uniform_partition(num_txns, num_executor_shards);
        let mut start_txn_ids_by_shard_id = vec![0; num_executor_shards];
        {
            for shard_id in 1..num_executor_shards {
                start_txn_ids_by_shard_id[shard_id] = start_txn_ids_by_shard_id[shard_id - 1] + remaining_txns[shard_id - 1].len();
            }
        }

        let timer = OMEGA_PARTITIONER_MISC_TIMERS_SECONDS.with_label_values(&["multi_rounds"]).start_timer();
        let num_rounds: usize = 2;
        let mut txn_id_matrix: Vec<Vec<Vec<usize>>> = Vec::new();
        for round_id in 0..(num_rounds - 1) {
            let timer = OMEGA_PARTITIONER_MISC_TIMERS_SECONDS.with_label_values(&["multi_rounds"]).start_timer();
            let (accepted, discarded) = self.discarding_round(round_id, &txns, remaining_txns, &helpers_by_key_id, &start_txn_ids_by_shard_id);
            txn_id_matrix.push(accepted);
            remaining_txns = discarded;
            let duration = timer.stop_and_record();
            println!("omega_par/multi_rounds/round_{round_id}={duration:?}");
        }

        for (round_id, row) in txn_id_matrix.iter().enumerate() {
            println!("RAW_MATRIX - round_id={round_id}, row={row:?}");
        }
        println!("RAW_MATRIX - last_round, row={remaining_txns:?}");

        let last_round_txns: Vec<usize> = remaining_txns.into_iter().flatten().collect();
        for txn_id in last_round_txns.iter() {
            let txn = &txns[*txn_id];
            for loc in txn.read_hints.iter().chain(txn.write_hints.iter()) {
                let loc_id = *loc.maybe_id_in_partition_session.as_ref().unwrap();
                let helper = helpers_by_key_id.get(&loc_id).unwrap();
                helper.write().unwrap().promote_txn_id(*txn_id, num_rounds - 1, num_executor_shards - 1);
            }
        }

        remaining_txns = vec![vec![]; num_executor_shards];
        remaining_txns[num_executor_shards - 1] = last_round_txns;
        txn_id_matrix.push(remaining_txns);
        let num_actual_rounds = txn_id_matrix.len();
        let duration = timer.stop_and_record();
        println!("omega_par/multi_rounds={duration:?}");

        print_storage_location_helper_summary(&key_ids_by_key, &helpers_by_key_id);

        let timer = OMEGA_PARTITIONER_MISC_TIMERS_SECONDS.with_label_values(&["add_edges"]).start_timer();
        let txns: Vec<Mutex<Option<AnalyzedTransaction>>> = txns.into_iter().map(|t|Mutex::new(Some(t))).collect();
        let ret = self.add_edges(&txns, &txn_id_matrix, &helpers_by_key_id);
        let duration = timer.stop_and_record();
        println!("omega_par/add_edges={duration:?}");

        ret
    }
}

fn print_storage_location_helper_summary(key_ids_by_key: &DashMap<StateKey, usize>, helpers_by_key_id: &DashMap<usize, RwLock<StorageLocationHelper>>) {
    for kv in key_ids_by_key.iter() {
        let key = kv.key();
        let key_id = *kv.value();
        let helper = helpers_by_key_id.get(&key_id).unwrap();
        println!("HELPER CHECK - key_id={}, key={}, helper={}", key_id, key.hash().to_hex(), helper.read().unwrap().brief());
    }
}

/// 18,5 -> [4,4,4,3,3]
fn uniform_partition(num_items: usize, num_chunks: usize) -> Vec<Vec<usize>> {
    let num_big_chunks = num_items % num_chunks;
    let small_chunk_size = num_items / num_chunks;
    let mut ret = Vec::with_capacity(num_chunks);
    let mut next_chunk_start = 0;
    for chunk_id in 0..num_chunks {
        let extra = if chunk_id < num_big_chunks { 1 } else { 0 };
        let next_chunk_end = next_chunk_start + small_chunk_size + extra;
        let chunk: Vec<usize> = (next_chunk_start..next_chunk_end).collect();
        next_chunk_start = next_chunk_end;
        ret.push(chunk);
    }
    ret
}

#[test]
fn test_uniform_partition() {
    let actual = uniform_partition(18, 5);
    assert_eq!(vec![4,4,4,3,3], actual.iter().map(|v|v.len()).collect::<Vec<usize>>());
    assert_eq!((0..18).collect::<Vec<usize>>(), actual.concat());

    let actual = uniform_partition(18, 3);
    assert_eq!(vec![6,6,6], actual.iter().map(|v|v.len()).collect::<Vec<usize>>());
    assert_eq!((0..18).collect::<Vec<usize>>(), actual.concat());
}


fn extract_and_sort(arr_2d: Vec<RwLock<Vec<usize>>>) -> Vec<Vec<usize>> {
    arr_2d.into_iter().map(|arr_1d|{
        let mut x = arr_1d.write().unwrap();
        let mut y = std::mem::replace(&mut *x, vec![]);
        y.sort();
        y
    }).collect::<Vec<_>>()
}

pub static OMEGA_PARTITIONER_MISC_TIMERS_SECONDS: Lazy<HistogramVec> = Lazy::new(|| {
    register_histogram_vec!(
        // metric name
        "omega_partitioner_misc_timers_seconds",
        // metric description
        "The time spent in seconds of miscellaneous phases of OmegaPartitioner.",
        &["name"],
        exponential_buckets(/*start=*/ 1e-3, /*factor=*/ 2.0, /*count=*/ 20).unwrap(),
    )
        .unwrap()
});

#[derive(Copy, Clone, Debug, Eq, Ord, PartialEq, Serialize, Deserialize)]
pub struct TxnFatId {
    pub round_id: usize,
    pub shard_id: usize,
    pub old_txn_idx: usize,
}

impl PartialOrd for TxnFatId {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        (self.round_id, self.shard_id, self.old_txn_idx).partial_cmp(&(other.round_id, other.shard_id, other.old_txn_idx))
    }
}

impl TxnFatId {
    pub fn new(round_id: usize, shard_id: usize, old_txn_idx: usize) -> Self {
        Self {
            round_id,
            shard_id,
            old_txn_idx,
        }
    }
}