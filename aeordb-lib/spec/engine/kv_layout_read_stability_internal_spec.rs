use std::sync::{mpsc, Arc};
use std::time::Duration;

use super::*;
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::RequestContext;

#[test]
fn stable_kv_wal_visit_blocks_layout_expansion_until_header_reads_finish() {
  let directory = tempfile::tempdir().unwrap();
  let database = directory.path().join("stable-kv-wal-visit.aeordb");
  let engine = Arc::new(StorageEngine::create(database.to_str().unwrap()).unwrap());
  let context = RequestContext::system();
  DirectoryOps::new(&engine).store_file_buffered(&context, "/live.txt", b"live", Some("text/plain")).unwrap();

  let current_stage = engine.writer_read_lock().unwrap().file_header().kv_block_stage as usize;
  let target_stage = current_stage + 1;
  let (visitor_entered_tx, visitor_entered_rx) = mpsc::channel();
  let (release_visitor_tx, release_visitor_rx) = mpsc::channel();
  let visiting_engine = Arc::clone(&engine);
  let visitor = std::thread::spawn(move || {
    let mut held_first_entry = false;
    visiting_engine.visit_kv_entries_with_stable_wal(|entry, writer| {
      if !held_first_entry {
        held_first_entry = true;
        visitor_entered_tx.send(()).unwrap();
        release_visitor_rx.recv_timeout(Duration::from_secs(5)).expect("stable visitor release timed out");
      }
      let header = writer.read_entry_header_at_shared(entry.offset)?;
      assert_eq!(header.total_length, entry.total_length, "stable KV row and WAL header diverged");
      Ok(true)
    })
  });

  visitor_entered_rx.recv_timeout(Duration::from_secs(5)).expect("stable visitor never reached a KV row");
  let (expansion_attempted_tx, expansion_attempted_rx) = mpsc::channel();
  let (expansion_done_tx, expansion_done_rx) = mpsc::channel();
  let expanding_engine = Arc::clone(&engine);
  let expansion = std::thread::spawn(move || {
    expansion_attempted_tx.send(()).unwrap();
    let result = expanding_engine.expand_kv_block_online(target_stage);
    expansion_done_tx.send(result).unwrap();
  });

  expansion_attempted_rx.recv_timeout(Duration::from_secs(5)).expect("expansion worker never started");
  assert!(
    expansion_done_rx.recv_timeout(Duration::from_millis(100)).is_err(),
    "KV expansion published while a stable KV/WAL visit still held the old layout"
  );

  release_visitor_tx.send(()).unwrap();
  assert!(visitor.join().unwrap().unwrap(), "stable visit must inspect the complete KV view");
  expansion_done_rx.recv_timeout(Duration::from_secs(5)).expect("KV expansion did not resume after the stable visit released").unwrap();
  expansion.join().unwrap();
}

#[test]
fn stable_kv_wal_visit_releases_layout_after_visitor_failure() {
  let directory = tempfile::tempdir().unwrap();
  let database = directory.path().join("failed-stable-kv-wal-visit.aeordb");
  let engine = Arc::new(StorageEngine::create(database.to_str().unwrap()).unwrap());
  let context = RequestContext::system();
  DirectoryOps::new(&engine).store_file_buffered(&context, "/live.txt", b"live", Some("text/plain")).unwrap();

  let error = engine
    .visit_kv_entries_with_stable_wal(|_, _| Err(EngineError::Cancelled("injected stable KV/WAL visitor failure".to_string())))
    .expect_err("visitor failure must propagate");
  assert!(matches!(error, EngineError::Cancelled(message) if message == "injected stable KV/WAL visitor failure"));

  let current_stage = engine.writer_read_lock().unwrap().file_header().kv_block_stage as usize;
  let target_stage = current_stage + 1;
  let (expansion_done_tx, expansion_done_rx) = mpsc::channel();
  let expanding_engine = Arc::clone(&engine);
  let expansion = std::thread::spawn(move || {
    expansion_done_tx.send(expanding_engine.expand_kv_block_online(target_stage)).unwrap();
  });
  expansion_done_rx.recv_timeout(Duration::from_secs(5)).expect("visitor failure leaked a stable-layout lock or snapshot lease").unwrap();
  expansion.join().unwrap();
}
