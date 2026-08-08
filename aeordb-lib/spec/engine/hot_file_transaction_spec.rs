//! Comprehensive tests for hot file transaction guards, deadlock prevention,
//! and crash recovery.
//!
//! Validates that TransactionGuard correctly manages transaction depth,
//! fires on all exit paths (normal, error, panic), and that store/delete
//! operations are properly wrapped in transactions.

use aeordb::engine::storage_engine::{StorageEngine, TransactionGuard};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::entry_type::EntryType;
use aeordb::engine::errors::EngineError;
use aeordb::engine::RequestContext;
use serial_test::serial;

struct SpillEnvironment {
  spill_directory: Option<std::ffi::OsString>,
  spill_max_bytes: Option<std::ffi::OsString>,
  config_only: Option<std::ffi::OsString>,
}

impl SpillEnvironment {
  fn new(spill_directory: &std::path::Path) -> Self {
    let previous = Self {
      spill_directory: std::env::var_os("AEORDB_EMERGENCY_SPILL_DIR"),
      spill_max_bytes: std::env::var_os("AEORDB_EMERGENCY_WAL_SPILL_MAX_BYTES"),
      config_only: std::env::var_os("AEORDB_EMERGENCY_SPILL_TEST_CONFIG_ONLY"),
    };
    unsafe {
      std::env::set_var("AEORDB_EMERGENCY_SPILL_DIR", spill_directory);
      std::env::set_var("AEORDB_EMERGENCY_WAL_SPILL_MAX_BYTES", "1048576");
      std::env::set_var("AEORDB_EMERGENCY_SPILL_TEST_CONFIG_ONLY", "1");
    }
    previous
  }
}

impl Drop for SpillEnvironment {
  fn drop(&mut self) {
    unsafe {
      restore_environment("AEORDB_EMERGENCY_SPILL_DIR", self.spill_directory.take());
      restore_environment("AEORDB_EMERGENCY_WAL_SPILL_MAX_BYTES", self.spill_max_bytes.take());
      restore_environment("AEORDB_EMERGENCY_SPILL_TEST_CONFIG_ONLY", self.config_only.take());
    }
  }
}

unsafe fn restore_environment(name: &str, value: Option<std::ffi::OsString>) {
  match value {
    Some(value) => unsafe { std::env::set_var(name, value) },
    None => unsafe { std::env::remove_var(name) },
  }
}

/// Create a fresh test database with hot directory enabled.
fn create_test_db_with_hot_dir() -> (StorageEngine, tempfile::TempDir) {
  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("test.aeordb");
  let engine = StorageEngine::create_with_hot_dir(db_path.to_str().unwrap(), Some(temp.path())).unwrap();
  (engine, temp)
}

fn store_transaction_probe(engine: &StorageEngine, discriminator: u8) -> Vec<u8> {
  let key = vec![discriminator; engine.hash_algo().hash_length()];
  engine.store_entry(EntryType::Chunk, &key, &[discriminator]).unwrap();
  key
}

// =========================================================================
// Transaction depth management
// =========================================================================

#[test]
fn transaction_guard_increments_and_decrements_depth() {
  let (engine, _temp) = create_test_db_with_hot_dir();

  // Depth starts at 0
  {
    let _guard = TransactionGuard::new(&engine).unwrap();
    // Inside transaction -- depth is 1. Raw dependency appends remain valid;
    // namespace APIs own their own exclusive hard-authority transaction.
    store_transaction_probe(&engine, 0x11);
  }
  // Guard dropped -- depth back to 0, hot file truncated

  // Verify namespace authority is reusable after guard drop.
  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();
  ops.store_file_buffered(&ctx, "/test.txt", b"hello", Some("text/plain")).unwrap();
  let data = ops.read_file_buffered("/test.txt").unwrap();
  assert_eq!(data, b"hello");
}

#[test]
fn transaction_guard_fires_on_error() {
  let (engine, _temp) = create_test_db_with_hot_dir();

  let result: Result<(), String> = {
    let _guard = TransactionGuard::new(&engine).unwrap();
    // Simulate an error mid-transaction
    Err("simulated error".to_string())
  };

  assert!(result.is_err());
  // Guard should have dropped -- verify we can start a new transaction
  // without deadlocking
  let guard2 = TransactionGuard::new(&engine).unwrap();
  // If this doesn't deadlock, depth management is correct
  store_transaction_probe(&engine, 0x12);
  guard2.commit().unwrap();

  // Also verify namespace authority is reusable after the transaction.
  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();
  ops.store_file_buffered(&ctx, "/after-error.txt", b"recovered", Some("text/plain")).unwrap();
}

#[test]
#[serial]
fn transaction_result_latches_only_typed_post_mutation_durability_failures() {
  let temp = tempfile::tempdir().unwrap();
  let _spill_environment = SpillEnvironment::new(&temp.path().join("spill"));
  let db_path = temp.path().join("post-mutation-latch.aeordb");
  let engine = StorageEngine::create(db_path.to_str().unwrap()).unwrap();

  let ordinary = TransactionGuard::new(&engine)
    .unwrap()
    .finish::<()>(Err(EngineError::DurabilityFailure("pre-overwrite memory admission refused".to_string())))
    .unwrap_err();
  assert!(matches!(ordinary, EngineError::DurabilityFailure(_)));
  assert!(engine.durability_failure().is_none(), "pre-overwrite refusal must not latch the database");

  let serious = TransactionGuard::new(&engine)
    .unwrap()
    .finish::<()>(Err(EngineError::PostMutationDurabilityFailure("page barrier failed after overwrite".to_string())))
    .unwrap_err();
  assert!(matches!(serious, EngineError::DurabilityFailure(_)));
  assert!(engine.durability_failure().unwrap().contains("page barrier failed after overwrite"));
  assert!(engine.emergency_spill_report().is_some(), "the first serious failure must attempt an emergency spill");
  assert!(matches!(
    engine.store_entry(aeordb::engine::entry_type::EntryType::Chunk, b"blocked", b"blocked"),
    Err(EngineError::DurabilityFailure(_))
  ));
}

#[test]
fn transaction_guard_fires_on_panic() {
  let (engine, _temp) = create_test_db_with_hot_dir();

  let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    let _guard = TransactionGuard::new(&engine).unwrap();
    panic!("simulated panic inside transaction");
  }));

  assert!(result.is_err());
  // Guard should have dropped despite panic
  // Verify depth is back to 0 by successfully starting a new transaction
  let guard2 = TransactionGuard::new(&engine).unwrap();
  store_transaction_probe(&engine, 0x13);
  guard2.commit().unwrap();
  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();
  // This should work -- depth is 0, hot file can truncate
  ops.store_file_buffered(&ctx, "/after-panic.txt", b"recovered", Some("text/plain")).unwrap();

  // Verify the file is actually readable
  let data = ops.read_file_buffered("/after-panic.txt").unwrap();
  assert_eq!(data, b"recovered");
}

#[test]
fn transaction_depth_always_returns_to_zero() {
  let (engine, _temp) = create_test_db_with_hot_dir();

  // Multiple sequential transactions
  for i in 0..10 {
    let _guard = TransactionGuard::new(&engine).unwrap();
    store_transaction_probe(&engine, 0x20 + i);
  }

  // All guards dropped -- depth must be 0
  // Prove it by successfully storing another file (which triggers flush + truncate)
  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();
  ops.store_file_buffered(&ctx, "/final.txt", b"final", Some("text/plain")).unwrap();

  // Verify all raw transaction probes and the final namespace write survived.
  for i in 0..10 {
    let key = vec![0x20 + i; engine.hash_algo().hash_length()];
    assert_eq!(engine.get_entry(&key).unwrap().unwrap().2, vec![0x20 + i]);
  }
  let final_data = ops.read_file_buffered("/final.txt").unwrap();
  assert_eq!(final_data, b"final");
}

#[test]
fn nested_guards_increment_depth_correctly() {
  let (engine, _temp) = create_test_db_with_hot_dir();

  {
    let _guard1 = TransactionGuard::new(&engine).unwrap();
    {
      let _guard2 = TransactionGuard::new(&engine).unwrap();
      // Depth is 2 here -- raw dependency appends remain buffered.
      store_transaction_probe(&engine, 0x31);
    }
    // Depth is 1 here -- inner guard dropped
  }
  // Depth is 0 here -- outer guard dropped

  // Verify we can start fresh transactions
  let guard3 = TransactionGuard::new(&engine).unwrap();
  store_transaction_probe(&engine, 0x32);
  guard3.commit().unwrap();
  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();
  ops.store_file_buffered(&ctx, "/after-nested.txt", b"ok", Some("text/plain")).unwrap();
}

#[test]
fn mixed_success_and_error_transactions() {
  let (engine, _temp) = create_test_db_with_hot_dir();

  // Successful transaction
  {
    let _guard = TransactionGuard::new(&engine).unwrap();
    store_transaction_probe(&engine, 0x41);
  }

  // Failed transaction (error)
  let _: Result<(), String> = {
    let _guard = TransactionGuard::new(&engine).unwrap();
    Err("fail".to_string())
  };

  // Another successful transaction
  {
    let _guard = TransactionGuard::new(&engine).unwrap();
    store_transaction_probe(&engine, 0x42);
  }

  // Panicked transaction
  let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    let _guard = TransactionGuard::new(&engine).unwrap();
    panic!("boom");
  }));

  // Final successful transaction -- proves depth is still 0
  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();
  ops.store_file_buffered(&ctx, "/success3.txt", b"ok3", Some("text/plain")).unwrap();

  // Successful dependency transactions and the final namespace write survive.
  assert_eq!(engine.get_entry(&vec![0x41; engine.hash_algo().hash_length()]).unwrap().unwrap().2, vec![0x41]);
  assert_eq!(engine.get_entry(&vec![0x42; engine.hash_algo().hash_length()]).unwrap().unwrap().2, vec![0x42]);
  assert_eq!(ops.read_file_buffered("/success3.txt").unwrap(), b"ok3");
}

// =========================================================================
// store_file is transactional
// =========================================================================

#[test]
fn store_file_wraps_in_transaction() {
  let (engine, _temp) = create_test_db_with_hot_dir();

  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();

  // Store a file -- this should be wrapped in a transaction internally
  ops.store_file_buffered(&ctx, "/docs/readme.md", b"# Hello", Some("text/markdown")).unwrap();

  // Verify the file is listed in its parent directory
  let children = ops.list_directory("/docs").unwrap();
  assert!(children.iter().any(|c| c.name == "readme.md"), "file should be in parent listing");

  // Verify the file is readable
  let data = ops.read_file_buffered("/docs/readme.md").unwrap();
  assert_eq!(data, b"# Hello");
}

#[test]
fn store_multiple_files_each_transactional() {
  let (engine, _temp) = create_test_db_with_hot_dir();

  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();

  // Each store_file internally wraps in a transaction
  ops.store_file_buffered(&ctx, "/docs/a.txt", b"aaa", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/docs/b.txt", b"bbb", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/docs/c.txt", b"ccc", Some("text/plain")).unwrap();

  // All should be listed
  let children = ops.list_directory("/docs").unwrap();
  assert_eq!(children.len(), 3);
  let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
  assert!(names.contains(&"a.txt"));
  assert!(names.contains(&"b.txt"));
  assert!(names.contains(&"c.txt"));

  // All should be readable
  assert_eq!(ops.read_file_buffered("/docs/a.txt").unwrap(), b"aaa");
  assert_eq!(ops.read_file_buffered("/docs/b.txt").unwrap(), b"bbb");
  assert_eq!(ops.read_file_buffered("/docs/c.txt").unwrap(), b"ccc");
}

#[test]
fn concurrent_namespace_transactions_share_a_live_header_commit() {
  use std::sync::{Arc, Barrier};

  let (engine, temp) = create_test_db_with_hot_dir();
  let engine = Arc::new(engine);
  let db_path = temp.path().join("test.aeordb");
  let initial_sequence = {
    let mut file = std::fs::File::open(&db_path).unwrap();
    aeordb::engine::file_header::read_active_header(&mut file).unwrap().0.sequence
  };
  let workers = 8usize;
  let start = Arc::new(Barrier::new(workers));
  let handles: Vec<_> = (0..workers)
    .map(|index| {
      let engine = engine.clone();
      let start = start.clone();
      std::thread::spawn(move || {
        start.wait();
        let path = format!("/grouped/file-{index}.txt");
        DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), &path, b"group me", Some("text/plain"))
      })
    })
    .collect();
  for handle in handles {
    handle.join().unwrap().unwrap();
  }

  let final_sequence = {
    let mut file = std::fs::File::open(&db_path).unwrap();
    aeordb::engine::file_header::read_active_header(&mut file).unwrap().0.sequence
  };
  assert!(final_sequence - initial_sequence < workers as u64, "concurrent commits were still published as singletons");
  assert_eq!(DirectoryOps::new(&engine).list_directory("/grouped").unwrap().len(), workers);

  let engine = Arc::try_unwrap(engine).ok().expect("all grouped writer references should be released");
  drop(engine);
  let reopened = StorageEngine::open_with_hot_dir(db_path.to_str().unwrap(), Some(temp.path())).unwrap();
  let reopened_ops = DirectoryOps::new(&reopened);
  for index in 0..workers {
    let path = format!("/grouped/file-{index}.txt");
    assert_eq!(reopened_ops.read_file_buffered(&path).unwrap(), b"group me");
  }
}

// =========================================================================
// delete_file is transactional
// =========================================================================

#[test]
fn delete_file_wraps_in_transaction() {
  let (engine, _temp) = create_test_db_with_hot_dir();

  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();

  ops.store_file_buffered(&ctx, "/docs/to-delete.txt", b"delete me", Some("text/plain")).unwrap();

  // Verify it exists
  let children = ops.list_directory("/docs").unwrap();
  assert!(children.iter().any(|c| c.name == "to-delete.txt"));

  // Delete it
  ops.delete_file(&ctx, "/docs/to-delete.txt").unwrap();

  // Verify it's gone from listing
  let children = ops.list_directory("/docs").unwrap();
  assert!(!children.iter().any(|c| c.name == "to-delete.txt"), "file should be removed from listing");

  // Verify reading it returns NotFound
  let result = ops.read_file_buffered("/docs/to-delete.txt");
  assert!(result.is_err(), "deleted file should not be readable");
}

#[test]
fn delete_nonexistent_file_returns_error() {
  let (engine, _temp) = create_test_db_with_hot_dir();

  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();

  let result = ops.delete_file(&ctx, "/nonexistent.txt");
  assert!(result.is_err(), "deleting nonexistent file should fail");
}

// =========================================================================
// Recovery tests
// =========================================================================

#[test]
fn recovery_detects_orphaned_file_after_hot_replay() {
  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("test.aeordb");
  let db_str = db_path.to_str().unwrap();
  let hot_dir = temp.path();

  // Create DB and store files normally
  {
    let engine = StorageEngine::create_with_hot_dir(db_str, Some(hot_dir)).unwrap();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.store_file_buffered(&ctx, "/docs/existing.txt", b"exists", Some("text/plain")).unwrap();
  }

  // Reopen -- should have no recovery needed, data should be intact
  {
    let engine = StorageEngine::open_with_hot_dir(db_str, Some(hot_dir)).unwrap();
    let ops = DirectoryOps::new(&engine);
    let children = ops.list_directory("/docs").unwrap();
    assert!(children.iter().any(|c| c.name == "existing.txt"), "file should survive close/reopen cycle");

    // Verify the file data is intact
    let data = ops.read_file_buffered("/docs/existing.txt").unwrap();
    assert_eq!(data, b"exists");
  }
}

#[test]
fn recovery_preserves_multiple_files_across_restart() {
  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("test.aeordb");
  let db_str = db_path.to_str().unwrap();
  let hot_dir = temp.path();

  // Create and populate
  {
    let engine = StorageEngine::create_with_hot_dir(db_str, Some(hot_dir)).unwrap();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.store_file_buffered(&ctx, "/data/alpha.txt", b"alpha-data", Some("text/plain")).unwrap();
    ops.store_file_buffered(&ctx, "/data/beta.txt", b"beta-data", Some("text/plain")).unwrap();
    ops.store_file_buffered(&ctx, "/data/gamma.txt", b"gamma-data", Some("text/plain")).unwrap();
  }

  // Reopen and verify
  {
    let engine = StorageEngine::open_with_hot_dir(db_str, Some(hot_dir)).unwrap();
    let ops = DirectoryOps::new(&engine);

    let children = ops.list_directory("/data").unwrap();
    assert_eq!(children.len(), 3, "all three files should survive restart");

    assert_eq!(ops.read_file_buffered("/data/alpha.txt").unwrap(), b"alpha-data");
    assert_eq!(ops.read_file_buffered("/data/beta.txt").unwrap(), b"beta-data");
    assert_eq!(ops.read_file_buffered("/data/gamma.txt").unwrap(), b"gamma-data");
  }
}

#[test]
fn recovery_after_store_and_delete_across_restart() {
  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("test.aeordb");
  let db_str = db_path.to_str().unwrap();
  let hot_dir = temp.path();

  // Create, store, then delete a file
  {
    let engine = StorageEngine::create_with_hot_dir(db_str, Some(hot_dir)).unwrap();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.store_file_buffered(&ctx, "/docs/keep.txt", b"keep-me", Some("text/plain")).unwrap();
    ops.store_file_buffered(&ctx, "/docs/remove.txt", b"remove-me", Some("text/plain")).unwrap();
    ops.delete_file(&ctx, "/docs/remove.txt").unwrap();
  }

  // Reopen and verify the deletion persisted
  {
    let engine = StorageEngine::open_with_hot_dir(db_str, Some(hot_dir)).unwrap();
    let ops = DirectoryOps::new(&engine);

    let children = ops.list_directory("/docs").unwrap();
    assert!(children.iter().any(|c| c.name == "keep.txt"), "kept file should exist");
    assert!(!children.iter().any(|c| c.name == "remove.txt"), "deleted file should stay deleted");

    assert_eq!(ops.read_file_buffered("/docs/keep.txt").unwrap(), b"keep-me");
  }
}

// =========================================================================
// Edge cases
// =========================================================================

#[test]
fn empty_file_uses_its_owned_namespace_transaction() {
  let (engine, _temp) = create_test_db_with_hot_dir();

  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();

  ops.store_file_buffered(&ctx, "/empty.txt", b"", Some("text/plain")).unwrap();

  let data = ops.read_file_buffered("/empty.txt").unwrap();
  assert!(data.is_empty(), "empty file should read back as empty");
}

#[test]
fn large_file_uses_its_owned_namespace_transaction() {
  let (engine, _temp) = create_test_db_with_hot_dir();

  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();

  // Store a file larger than one chunk (>256KB) through its owned transaction.
  let large_data: Vec<u8> = (0..300_000).map(|i| (i % 256) as u8).collect();
  ops.store_file_buffered(&ctx, "/large.bin", &large_data, Some("application/octet-stream")).unwrap();

  let read_back = ops.read_file_buffered("/large.bin").unwrap();
  assert_eq!(read_back.len(), 300_000);
  assert_eq!(read_back, large_data);
}

#[test]
fn namespace_write_rejects_a_legacy_outer_transaction_without_publishing_authority() {
  let (engine, _temp) = create_test_db_with_hot_dir();
  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();
  let initial_head = engine.head_hash().unwrap();
  let initial_sequence = engine.durability_snapshot().unwrap().next_sequence;

  let guard = TransactionGuard::new(&engine).unwrap();
  let error = ops.store_file_buffered(&ctx, "/nested-refused.txt", b"refused", Some("text/plain")).unwrap_err();
  assert!(matches!(error, EngineError::InvalidInput(message) if message.contains("top-level namespace mutation")));
  assert!(ops.get_metadata("/nested-refused.txt").unwrap().is_none());
  assert_eq!(engine.head_hash().unwrap(), initial_head);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, initial_sequence);
  guard.commit().unwrap();
}

#[test]
fn overwrite_file_with_transaction() {
  let (engine, _temp) = create_test_db_with_hot_dir();

  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();

  // Store, then overwrite
  ops.store_file_buffered(&ctx, "/mutable.txt", b"version-1", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/mutable.txt", b"version-2", Some("text/plain")).unwrap();

  let data = ops.read_file_buffered("/mutable.txt").unwrap();
  assert_eq!(data, b"version-2", "overwritten file should have latest content");

  // Only one entry in parent listing
  let children = ops.list_directory("/").unwrap();
  let matches: Vec<_> = children.iter().filter(|c| c.name == "mutable.txt").collect();
  assert_eq!(matches.len(), 1, "should not duplicate listing on overwrite");
}
