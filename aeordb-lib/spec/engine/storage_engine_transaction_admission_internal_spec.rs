use super::{EngineError, StorageEngine, TransactionGuard};
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::time::{Duration, Instant};

#[test]
fn pre_admitted_top_level_transaction_excludes_late_legacy_joiners() {
  let temporary = tempfile::tempdir().unwrap();
  let database = temporary.path().join("exclusive-pre-admission.aeordb");
  let engine = StorageEngine::create(database.to_str().unwrap()).unwrap();
  let namespace = engine.namespace_write_guard().unwrap();
  let transaction = TransactionGuard::new_top_level(&engine, 0).unwrap();
  let admitted_sequence = engine.durability_snapshot().unwrap().next_sequence - 1;

  assert!(matches!(TransactionGuard::new(&engine), Err(EngineError::ResourceExhausted(_))));

  let receipt = transaction.commit_top_level_after(namespace).unwrap();
  assert_eq!(receipt.sequence, admitted_sequence);
  assert_eq!(engine.durability_snapshot().unwrap().pending_hard, 0);
  TransactionGuard::new(&engine).unwrap().commit().unwrap();
}

#[test]
fn exact_receipt_survives_post_commit_kv_expansion_preflight_failure() {
  let temporary = tempfile::tempdir().unwrap();
  let database = temporary.path().join("post-commit-expansion-failure.aeordb");
  let engine = StorageEngine::create(database.to_str().unwrap()).unwrap();
  let corrupt_offset = engine.store_entry(super::EntryType::Chunk, &[0x31; 32], b"expansion boundary").unwrap();
  {
    let mut file = OpenOptions::new().write(true).open(&database).unwrap();
    file.seek(SeekFrom::Start(corrupt_offset)).unwrap();
    file.write_all(&[0u8; 4]).unwrap();
    crate::engine::native_durability::sync_file_all_native(&file).unwrap();
  }

  let namespace = engine.namespace_write_guard().unwrap();
  let transaction = TransactionGuard::new_top_level(&engine, 0).unwrap();
  let admitted_sequence = engine.durability_snapshot().unwrap().next_sequence - 1;
  engine.kv_writer.lock().unwrap().needs_expansion = Some(1);

  let receipt = transaction
    .commit_top_level_after(namespace)
    .expect("post-commit maintenance must not convert an exact hard receipt into retryable failure");

  assert_eq!(receipt.sequence, admitted_sequence);
  assert!(engine.durability_snapshot().unwrap().hard_frontier >= admitted_sequence);
  assert_eq!(engine.kv_writer.lock().unwrap().needs_expansion, Some(1));
  assert!(engine.durability_failure().is_none());
}

#[test]
fn snapshot_contention_defers_expansion_without_stalling_the_committed_write() {
  let temporary = tempfile::tempdir().unwrap();
  let database = temporary.path().join("snapshot-contention-expansion.aeordb");
  let engine = StorageEngine::create(database.to_str().unwrap()).unwrap();
  let key = [0x41; 32];
  engine.store_entry(super::EntryType::Chunk, &key, b"committed before expansion contention").unwrap();
  let retained_reader = engine.kv_snapshot.load_full();
  engine.kv_writer.lock().unwrap().needs_expansion = Some(1);

  let started = Instant::now();
  engine
    .run_ready_kv_expansion()
    .expect("snapshot contention should defer a pre-mutation expansion instead of failing the committed write");

  assert!(started.elapsed() < Duration::from_millis(100), "snapshot contention stalled the committed writer");
  assert_eq!(engine.kv_writer.lock().unwrap().needs_expansion, Some(1));
  assert_eq!(engine.writer.read().unwrap().file_header().kv_block_stage, 0);
  assert!(engine.get_entry(&key).unwrap().is_some());

  drop(retained_reader);
  engine.run_ready_kv_expansion().unwrap();
  assert_eq!(engine.kv_writer.lock().unwrap().needs_expansion, None);
  assert_eq!(engine.writer.read().unwrap().file_header().kv_block_stage, 1);
  assert!(engine.get_entry(&key).unwrap().is_some());
}
