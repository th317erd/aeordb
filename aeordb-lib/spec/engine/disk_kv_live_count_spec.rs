//! Live counts describe effective visibility, not presence of tombstone keys.
use super::*;
use crate::engine::kv_store::{KV_FLAG_DELETED, KV_FLAG_PENDING};
use tokio_util::sync::CancellationToken;

fn assert_snapshot_count(store: &DiskKVStore, expected: usize) {
  let snapshot = store.snapshot_handle().load_full();
  assert_eq!(snapshot.len(), expected);
  let summary = snapshot.visit_captured_entries(&CancellationToken::new(), 10_000, |_| Ok(true)).unwrap();
  assert!(summary.complete);
  assert_eq!(summary.visited_entries, expected as u64);
}

fn prepare(store: &mut DiskKVStore, prior: u8, flushed: bool) {
  if prior > 0 {
    store.insert(entry(0x31, 3_100)).unwrap();
    if prior == 2 {
      assert!(store.mark_deleted(&[0x31; 32]).unwrap());
    }
  }
  if flushed {
    store.flush().unwrap();
  }
  assert_eq!(store.len(), usize::from(prior == 1));
  assert_snapshot_count(store, usize::from(prior == 1));
}

#[test]
fn native_semantic_task_retention_kv_count_insert_tracks_live_transitions() {
  for prior in 0..=2 {
    for flushed in [false, true] {
      for deleted in [false, true] {
        let (mut store, _directory) = test_store("insert-live-count");
        prepare(&mut store, prior, flushed);
        let old = store.snapshot_handle().load_full();
        let mut incoming = entry(0x31, 4_100);
        if deleted {
          incoming.type_flags |= KV_FLAG_DELETED;
        }
        store.insert(incoming.clone()).unwrap();
        store.insert(incoming).unwrap();
        let expected = usize::from(!deleted);
        assert_eq!(store.len(), expected, "prior={prior} flushed={flushed} deleted={deleted}");
        assert_snapshot_count(&store, expected);
        store.flush().unwrap();
        assert_snapshot_count(&store, expected);
        assert_eq!(old.len(), usize::from(prior == 1));
        assert_eq!(old.get(&[0x31; 32]).unwrap().is_some(), prior == 1);
      }
    }
  }
}

#[test]
fn native_semantic_task_retention_kv_count_bulk_tracks_live_transitions_without_publishing() {
  for prior in 0..=2 {
    for flushed in [false, true] {
      for deleted in [false, true] {
        let (mut store, _directory) = test_store("bulk-live-count");
        prepare(&mut store, prior, flushed);
        let mut incoming = entry(0x31, 4_100);
        if deleted {
          incoming.type_flags |= KV_FLAG_DELETED;
        }
        store.bulk_insert(&[incoming.clone(), incoming]).unwrap();
        let expected = usize::from(!deleted);
        assert_eq!(store.len(), expected, "prior={prior} flushed={flushed} deleted={deleted}");
        assert_snapshot_count(&store, usize::from(prior == 1));
        store.flush().unwrap();
        assert_snapshot_count(&store, expected);
      }
    }
  }
}

#[test]
fn native_semantic_task_retention_kv_count_buffer_only_counts_live_rebuild_rows() {
  for prior in 0..=2 {
    for deleted in [false, true] {
      let (mut store, _directory) = test_store("buffer-only-live-count");
      if prior > 0 {
        let mut initial = entry(0x31, 3_100);
        if prior == 2 {
          initial.type_flags |= KV_FLAG_DELETED;
        }
        store.buffer_only(initial).unwrap();
        assert_eq!(store.len(), usize::from(prior == 1));
      }
      let mut incoming = entry(0x31, 4_100);
      if deleted {
        incoming.type_flags |= KV_FLAG_DELETED;
      }
      store.buffer_only(incoming.clone()).unwrap();
      store.buffer_only(incoming).unwrap();
      let expected = usize::from(!deleted);
      assert_eq!(store.len(), expected, "prior={prior} deleted={deleted}");
      assert_snapshot_count(&store, 0);
      store.flush().unwrap();
      assert_snapshot_count(&store, expected);
    }
  }
}

#[test]
fn native_semantic_task_retention_kv_count_atomic_tracks_live_transitions_and_abort() {
  for prior in 0..=2 {
    for deleted in [false, true] {
      for commit in [false, true] {
        let (mut store, _directory) = test_store("atomic-live-count");
        prepare(&mut store, prior, true);
        let prior_count = usize::from(prior == 1);
        let mut incoming = entry(0x31, 4_100);
        if deleted {
          incoming.type_flags |= KV_FLAG_DELETED;
        }
        let batch = store.begin_atomic_visibility_batch(2, 1).unwrap();
        store.stage_atomic_visibility_entry(batch, incoming).unwrap();
        assert_eq!(store.len(), usize::from(!deleted), "prior={prior} deleted={deleted}");
        assert_snapshot_count(&store, prior_count);
        if commit {
          store.complete_hot_tail_dependency();
          store.publish_atomic_visibility_after_authority(batch, &hard_receipt(1)).unwrap();
          assert_snapshot_count(&store, usize::from(!deleted));
        } else {
          store.abort_atomic_visibility_batch(batch).unwrap();
          assert_eq!(store.len(), prior_count);
          assert_snapshot_count(&store, prior_count);
        }
      }
    }
  }
}

#[test]
fn native_semantic_task_retention_kv_count_flag_deletion_updates_live_visibility() {
  for flushed in [false, true] {
    let (mut store, _directory) = test_store("flag-live-count");
    prepare(&mut store, 1, flushed);
    assert!(store.update_flags(&[0x31; 32], KV_FLAG_PENDING).unwrap());
    assert_eq!(store.len(), 1);
    assert_snapshot_count(&store, 1);
    assert!(store.update_flags(&[0x31; 32], KV_FLAG_PENDING | KV_FLAG_DELETED).unwrap());
    assert_eq!(store.len(), 0);
    assert_snapshot_count(&store, 0);
    assert!(!store.update_flags(&[0x31; 32], KV_FLAG_PENDING).unwrap());
    assert_eq!(store.len(), 0);
    store.flush().unwrap();
    assert_snapshot_count(&store, 0);
  }
}

#[test]
fn native_semantic_task_retention_kv_count_reopen_agrees_for_all_hash_algorithms() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let directory = tempdir().unwrap();
    let path = directory.path().join("count-reopen.aeordb");
    let tail = 256 + stage_params(0, page_size(algorithm.hash_length())).0;
    let live = KVEntry { type_flags: KV_TYPE_CHUNK, hash: vec![0x31; algorithm.hash_length()], offset: tail + 512, total_length: 64 };
    {
      let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path).unwrap();
      let mut store = DiskKVStore::create(file, algorithm, 256, tail, 0).unwrap();
      store.insert(live.clone()).unwrap();
      store.flush().unwrap();
      assert!(store.mark_deleted(&live.hash).unwrap());
      store.flush().unwrap();
      let deleted_snapshot = store.snapshot_handle().load_full();
      store.insert(live.clone()).unwrap();
      assert_eq!(store.len(), 1);
      assert_snapshot_count(&store, 1);
      store.flush().unwrap();
      assert_eq!(deleted_snapshot.len(), 0);
      assert!(deleted_snapshot.get(&live.hash).unwrap().is_none());
    }
    let file = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
    let reopened = DiskKVStore::open(
      file,
      algorithm,
      256,
      tail,
      0,
      HotTailReplay { entries: vec![], voids: vec![] },
      DiskKVStore::CURRENT_KV_BLOCK_VERSION,
    )
    .unwrap();
    assert_eq!(reopened.len(), 1);
    assert_eq!(reopened.get(&live.hash).unwrap(), Some(live));
    assert_snapshot_count(&reopened, 1);
  }
}

#[test]
fn native_semantic_task_retention_kv_count_overflow_refuses_before_any_insert_mutation() {
  for mode in 0..4 {
    let (mut store, _directory) = test_store("live-count-overflow");
    let before = std::fs::read(_directory.path().join("live-count-overflow.aeordb")).unwrap();
    let incoming = entry(0x31, 4_100);
    let batch = if mode == 3 { Some(store.begin_atomic_visibility_batch(2, 1).unwrap()) } else { None };
    store.entry_count = usize::MAX;
    let result = match mode {
      0 => store.insert(incoming),
      1 => store.bulk_insert(&[incoming]),
      2 => store.buffer_only(incoming),
      _ => store.stage_atomic_visibility_entry(batch.unwrap(), incoming),
    };
    assert!(matches!(result, Err(EngineError::ResourceExhausted(_))));
    assert_eq!(store.entry_count, usize::MAX);
    assert_eq!(store.write_buffer_len(), 0);
    assert_eq!(store.hot_buffer_len(), 0);
    assert_snapshot_count(&store, 0);
    assert_eq!(std::fs::read(_directory.path().join("live-count-overflow.aeordb")).unwrap(), before);
    if let Some(batch) = batch {
      store.abort_atomic_visibility_batch(batch).unwrap();
    }
    store.entry_count = 0;
  }
}

#[test]
fn native_semantic_task_retention_kv_count_underflow_refuses_before_deleting_live_entry() {
  for mode in 0..7 {
    let (mut store, directory) = test_store("live-count-underflow");
    prepare(&mut store, 1, mode == 3);
    let before = std::fs::read(directory.path().join("live-count-underflow.aeordb")).unwrap();
    let before_buffer = store.write_buffer.clone();
    let before_hot = store.hot_buffer.clone();
    let mut incoming = entry(0x31, 4_100);
    incoming.type_flags |= KV_FLAG_DELETED;
    let batch = if mode == 3 { Some(store.begin_atomic_visibility_batch(2, 1).unwrap()) } else { None };
    store.entry_count = 0;
    let result = match mode {
      0 => store.insert(incoming),
      1 => store.bulk_insert(&[incoming]),
      2 => store.buffer_only(incoming),
      3 => store.stage_atomic_visibility_entry(batch.unwrap(), incoming),
      4 => store.update_flags(&[0x31; 32], KV_FLAG_DELETED).map(|changed| assert!(changed)),
      5 => store.mark_deleted(&[0x31; 32]).map(|changed| assert!(changed)),
      _ => store.mark_deleted_batch(&[vec![0x31; 32]]),
    };
    assert!(matches!(result, Err(EngineError::CorruptEntry { .. })), "mode={mode}: {result:?}");
    assert_eq!(store.entry_count, 0);
    assert_eq!(store.write_buffer, before_buffer);
    assert_eq!(store.hot_buffer, before_hot);
    assert_snapshot_count(&store, 1);
    assert_eq!(std::fs::read(directory.path().join("live-count-underflow.aeordb")).unwrap(), before);
    if let Some(batch) = batch {
      store.abort_atomic_visibility_batch(batch).unwrap();
    }
    store.entry_count = 1;
  }
}
