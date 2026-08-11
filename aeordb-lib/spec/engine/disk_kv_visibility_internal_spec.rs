use super::*;

use crate::engine::durability_coordinator::{CommitClass, DurabilityCommitReceipt};
use crate::engine::kv_store::KV_TYPE_CHUNK;
use tempfile::tempdir;

fn test_store(name: &str) -> (DiskKVStore, tempfile::TempDir) {
  let directory = tempdir().unwrap();
  let path = directory.path().join(format!("{name}.aeordb"));
  let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(path).unwrap();
  let block_size = crate::engine::kv_stages::initial_block_size();
  let store = DiskKVStore::create(file, HashAlgorithm::Blake3_256, 256, 256 + block_size, 0).unwrap();
  (store, directory)
}

fn entry(seed: u8, offset: u64) -> KVEntry {
  KVEntry { type_flags: KV_TYPE_CHUNK, hash: vec![seed; 32], offset, total_length: 64 }
}

fn hard_receipt(sequence: u64) -> DurabilityCommitReceipt {
  DurabilityCommitReceipt { sequence, class: CommitClass::HardAuthority, hard_frontier: sequence }
}

#[test]
fn atomic_visibility_batch_keeps_staged_entries_hidden_until_hard_authority() {
  let (mut store, _directory) = test_store("hidden-until-authority");
  let prior_snapshot = store.snapshot_handle().load_full();
  let staged = entry(0x41, 4_096);
  let batch = store.begin_atomic_visibility_batch(2, 1).unwrap();

  store.stage_atomic_visibility_entry(batch, staged.clone()).unwrap();

  assert!(prior_snapshot.get(&staged.hash).unwrap().is_none());
  assert!(store.snapshot_handle().load().get(&staged.hash).unwrap().is_none());
  assert_eq!(store.get_buffered(&staged.hash), Some(&staged));
  assert!(store.publish_atomic_visibility_after_authority(batch, &hard_receipt(1)).is_err());
  store.complete_hot_tail_dependency();
  let incomplete = DurabilityCommitReceipt { sequence: 1, class: CommitClass::HardAuthority, hard_frontier: 0 };
  assert!(store.publish_atomic_visibility_after_authority(batch, &incomplete).is_err());
  assert!(store.snapshot_handle().load().get(&staged.hash).unwrap().is_none());

  store.publish_atomic_visibility_after_authority(batch, &hard_receipt(1)).unwrap();

  assert_eq!(store.snapshot_handle().load().get(&staged.hash).unwrap(), Some(staged));
  assert!(prior_snapshot.get(&vec![0x41; 32]).unwrap().is_none(), "an already captured read view must remain exact");
}

#[test]
fn atomic_visibility_batch_aborts_to_the_exact_prior_view() {
  let (mut store, _directory) = test_store("abort-prior-view");
  let existing = entry(0x51, 5_100);
  store.insert(existing.clone()).unwrap();
  store.flush().unwrap();
  let prior_count = store.len();
  let prior_hot_tail_offset = store.hot_tail_offset();
  let prior_snapshot = store.snapshot_handle().load_full();
  let staged = entry(0x52, 5_200);
  let batch = store.begin_atomic_visibility_batch(2, 2).unwrap();

  store.stage_atomic_visibility_entry(batch, staged.clone()).unwrap();
  store.set_hot_tail_offset(prior_hot_tail_offset + 8_192);
  store.abort_atomic_visibility_batch(batch).unwrap();

  assert_eq!(store.len(), prior_count);
  assert_eq!(store.write_buffer_len(), 0);
  assert_eq!(store.hot_buffer_len(), 0);
  assert_eq!(store.hot_tail_offset(), prior_hot_tail_offset);
  assert_eq!(store.snapshot_handle().load().get(&existing.hash).unwrap(), Some(existing));
  assert!(store.snapshot_handle().load().get(&staged.hash).unwrap().is_none());
  assert!(prior_snapshot.get(&staged.hash).unwrap().is_none());
}

#[test]
fn atomic_visibility_batch_rejects_dirty_entry_state_capacity_overflow_and_writer_bypass() {
  let (mut store, _directory) = test_store("bounds-and-bypass");
  let dirty = entry(0x61, 6_100);
  store.insert(dirty).unwrap();
  assert!(store.begin_atomic_visibility_batch(1, 3).is_err());
  store.flush().unwrap();
  assert!(store.begin_atomic_visibility_batch(0, 3).is_err());
  assert!(store.begin_atomic_visibility_batch(1, 0).is_err());
  assert!(store.begin_atomic_visibility_batch(WRITE_BUFFER_THRESHOLD, 3).is_err());

  let batch = store.begin_atomic_visibility_batch(1, 3).unwrap();
  let staged = entry(0x62, 6_200);
  store.stage_atomic_visibility_entry(batch, staged.clone()).unwrap();
  assert!(store.stage_atomic_visibility_entry(batch, staged.clone()).is_err());
  assert!(store.stage_atomic_visibility_entry(batch, entry(0x63, 6_300)).is_err());
  assert!(store.insert(entry(0x64, 6_400)).is_err());
  assert!(store.mark_deleted(&staged.hash).is_err());
  assert!(store.flush().is_err());
  assert!(store.snapshot_handle().load().get(&staged.hash).unwrap().is_none());
  store.abort_atomic_visibility_batch(batch).unwrap();
}

#[test]
fn atomic_visibility_batch_rejects_wrong_tokens_and_non_hard_receipts_without_revealing_state() {
  let (mut store, _directory) = test_store("token-and-receipt");
  let batch = store.begin_atomic_visibility_batch(1, 4).unwrap();
  let wrong = AtomicKvVisibilityBatch { id: batch.id.wrapping_add(1) };
  let staged = entry(0x71, 7_100);

  assert!(store.stage_atomic_visibility_entry(wrong, staged.clone()).is_err());
  store.stage_atomic_visibility_entry(batch, staged.clone()).unwrap();
  store.complete_hot_tail_dependency();
  let soft = DurabilityCommitReceipt { sequence: 4, class: CommitClass::RecoverableSoftState, hard_frontier: 4 };
  assert!(store.publish_atomic_visibility_after_authority(batch, &soft).is_err());
  assert!(store.publish_atomic_visibility_after_authority(batch, &hard_receipt(3)).is_err());
  assert!(store.snapshot_handle().load().get(&staged.hash).unwrap().is_none());
  assert!(store.abort_atomic_visibility_batch(wrong).is_err());
  store.abort_atomic_visibility_batch(batch).unwrap();
}
