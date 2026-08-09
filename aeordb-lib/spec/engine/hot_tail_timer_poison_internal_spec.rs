use std::collections::BTreeMap;
use std::ffi::OsString;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use super::*;

fn test_engine(temporary: &tempfile::TempDir, name: &str) -> Arc<StorageEngine> {
  let database_path = temporary.path().join(name);
  let spill_path = temporary.path().join(format!("{name}.spill"));
  let command_line = crate::engine::config_resolver::CommandLineConfigOverrides::from_registered(BTreeMap::from([(
    "--recovery-emergency-spill-dir".to_string(),
    OsString::from(spill_path),
  )]))
  .unwrap();
  Arc::new(StorageEngine::create_with_hot_dir_and_configuration_overrides(database_path.to_str().unwrap(), None, command_line).unwrap())
}

#[test]
fn poisoned_kv_probe_latches_the_timer_instead_of_looking_busy() {
  let temporary = tempfile::tempdir().unwrap();
  let engine = test_engine(&temporary, "poisoned-kv-probe.aeordb");
  let poison_engine = Arc::clone(&engine);
  let _ = std::thread::spawn(move || {
    let _guard = poison_engine.kv_writer.lock().unwrap();
    panic!("intentional KV writer poison");
  })
  .join();

  let result = catch_unwind(AssertUnwindSafe(|| engine.try_flush_hot_buffer()));

  assert!(result.is_ok(), "the timer must not unwind after KV lock poison");
  assert!(engine.durability_failure().is_some(), "KV lock poison must latch write admission read-only");
}

#[test]
fn poisoned_wal_writer_latches_the_timer_instead_of_looking_busy() {
  let temporary = tempfile::tempdir().unwrap();
  let engine = test_engine(&temporary, "poisoned-wal-writer.aeordb");
  engine.store_entry(EntryType::Chunk, &[0xA1; 32], b"pending-value").unwrap();
  let poison_engine = Arc::clone(&engine);
  let _ = std::thread::spawn(move || {
    let _guard = poison_engine.writer.write().unwrap();
    panic!("intentional WAL writer poison");
  })
  .join();

  let result = catch_unwind(AssertUnwindSafe(|| engine.try_flush_hot_buffer()));

  assert!(result.is_ok(), "the timer must not unwind after writer lock poison");
  assert!(engine.durability_failure().is_some(), "writer lock poison must latch write admission read-only");
  assert!(engine.kv_layout_metrics().is_err(), "diagnostics must not publish a fabricated zero-sized KV block after writer poison");
}

#[test]
fn healthy_lock_contention_still_defers_without_latching() {
  let temporary = tempfile::tempdir().unwrap();
  let engine = test_engine(&temporary, "healthy-contention.aeordb");
  engine.store_entry(EntryType::Chunk, &[0xA2; 32], b"pending-value").unwrap();

  {
    let kv_guard = engine.kv_writer.lock().unwrap();
    engine.try_flush_hot_buffer();
    assert!(engine.durability_failure().is_none());
    assert!(kv_guard.hot_buffer_len() > 0);
  }

  {
    let _writer_guard = engine.writer.write().unwrap();
    engine.try_flush_hot_buffer();
    assert!(engine.durability_failure().is_none());
  }

  engine.try_flush_hot_buffer();
  assert!(engine.durability_failure().is_none());
  assert_eq!(engine.kv_writer.lock().unwrap().hot_buffer_len(), 0);
}
