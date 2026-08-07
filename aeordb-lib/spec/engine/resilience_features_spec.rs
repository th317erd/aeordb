//! Tests for resilience features: GC auto-snapshot, verify, and verify --repair.

use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::gc::run_gc;
use aeordb::engine::memory_coordinator::{AdmissionClass, CriticalMemoryPurpose, MemoryOwner};
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::task_queue::TaskQueue;
use aeordb::engine::verify;
use aeordb::engine::version_manager::VersionManager;
use aeordb::engine::RequestContext;
use aeordb::server::create_temp_engine_for_tests;

/// Inject garbage bytes at the given offset in the database file.
fn inject_corruption(db_path: &str, offset: u64, size: usize) {
  use std::io::{Seek, SeekFrom, Write};
  let mut file = std::fs::OpenOptions::new().write(true).open(db_path).unwrap();
  file.seek(SeekFrom::Start(offset)).unwrap();
  let garbage: Vec<u8> = (0..size).map(|i| (i as u8).wrapping_mul(0x37)).collect();
  file.write_all(&garbage).unwrap();
  file.sync_all().unwrap();
}

/// Store a few test files into the engine.
fn store_test_files(engine: &StorageEngine) {
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(engine);
  ops.store_file_buffered(&ctx, "/docs/a.txt", b"file-a-content", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/docs/b.txt", b"file-b-content", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/images/photo.jpg", b"jpeg-data-here", Some("image/jpeg")).unwrap();
}

// =========================================================================
// Auto-snapshot before GC
// =========================================================================

#[test]
fn gc_creates_pre_gc_snapshot() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  store_test_files(&engine);

  // Delete a file so GC has something to collect
  let ops = DirectoryOps::new(&engine);
  ops.delete_file(&ctx, "/docs/a.txt").unwrap();

  // Run GC (not dry run)
  run_gc(&engine, &ctx, false).unwrap();

  // Check for pre-GC snapshot
  let vm = VersionManager::new(&engine);
  let snapshots = vm.list_snapshots().unwrap();
  let pre_gc = snapshots.iter().find(|s| s.name.starts_with("_aeordb_pre_gc_"));
  assert!(pre_gc.is_some(), "Pre-GC snapshot should exist");
}

#[test]
fn gc_dry_run_does_not_create_snapshot() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  store_test_files(&engine);

  let ops = DirectoryOps::new(&engine);
  ops.delete_file(&ctx, "/docs/a.txt").unwrap();

  // Dry run -- no snapshot
  run_gc(&engine, &ctx, true).unwrap();

  let vm = VersionManager::new(&engine);
  let snapshots = vm.list_snapshots().unwrap();
  let pre_gc = snapshots.iter().find(|s| s.name.starts_with("_aeordb_pre_gc_"));
  assert!(pre_gc.is_none(), "Dry run should not create snapshot");
}

#[test]
fn gc_keeps_only_last_3_pre_gc_snapshots() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();

  for i in 0..5 {
    // Store and delete a file to create garbage
    let ops = DirectoryOps::new(&engine);
    let path = format!("/temp_{}.txt", i);
    ops.store_file_buffered(&ctx, &path, format!("content-{}", i).as_bytes(), Some("text/plain")).unwrap();
    ops.delete_file(&ctx, &path).unwrap();

    // Sleep 1.1s so each GC gets a unique timestamp (chrono::Utc::now().timestamp()
    // has 1-second resolution)
    std::thread::sleep(std::time::Duration::from_millis(1100));

    // Run GC
    run_gc(&engine, &ctx, false).unwrap();
  }

  let vm = VersionManager::new(&engine);
  let snapshots = vm.list_snapshots().unwrap();
  let pre_gc_count = snapshots.iter().filter(|s| s.name.starts_with("_aeordb_pre_gc_")).count();

  assert!(pre_gc_count <= 3, "Should keep at most 3 pre-GC snapshots, got {}", pre_gc_count);
}

// =========================================================================
// aeordb verify
// =========================================================================

#[test]
fn verify_clean_database_reports_no_issues() {
  let (engine, temp) = create_temp_engine_for_tests();
  store_test_files(&engine);

  let db_path = temp.path().join("test.aeordb");
  let report = verify::verify(&engine, db_path.to_str().unwrap());

  // Core integrity: no corruption, no missing children
  assert_eq!(report.corrupt_hash, 0, "Should have no corrupt hashes");
  assert_eq!(report.corrupt_header, 0, "Should have no corrupt headers");
  assert!(report.missing_children.is_empty(), "Should have no missing children: {:?}", report.missing_children,);

  // Entry counts should be populated
  assert!(report.total_entries > 0, "Should have scanned entries");
  assert!(report.chunks > 0, "Should have chunks");
  assert!(report.file_records > 0, "Should have file records");
  assert!(report.directory_indexes > 0, "Should have directory indexes");
  assert!(report.valid_entries > 0, "Should have valid entries");
  assert!(report.verification_errors.is_empty(), "clean verification should not hide operational errors");
}

#[test]
fn verify_accepts_a_fresh_embedded_database_before_lazy_root_creation() {
  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("fresh.aeordb");
  let engine = StorageEngine::create(db_path.to_str().unwrap()).unwrap();

  let report = verify::verify_checked(&engine, db_path.to_str().unwrap()).unwrap();

  assert!(!report.has_issues(), "fresh database without a user root was reported damaged: {report:?}");
  assert_eq!(report.directories_checked, 1);
}

#[test]
fn verify_does_not_materialize_user_files_or_record_user_reads() {
  let (engine, temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let content = vec![0x5a; 2 * 1024 * 1024];
  ops.store_file_buffered(&ctx, "/large/payload.bin", &content, Some("application/octet-stream")).unwrap();
  drop(content);

  let before = engine.counters().snapshot();
  let db_path = temp.path().join("test.aeordb");
  let report = verify::verify_checked(&engine, db_path.to_str().unwrap()).unwrap();
  let after = engine.counters().snapshot();

  assert!(!report.has_issues(), "clean large-file verification reported issues: {report:?}");
  assert_eq!(after.reads_total, before.reads_total, "verification was counted as a user file read");
  assert_eq!(after.bytes_read_total, before.bytes_read_total, "verification materialized user file content");
}

#[test]
fn verify_checked_fails_closed_under_repair_memory_pressure_and_releases_reservations() {
  let (engine, temp) = create_temp_engine_for_tests();
  store_test_files(&engine);
  let db_path = temp.path().join("test.aeordb");
  let db_path = db_path.to_str().unwrap();

  let coordinator = engine.memory_coordinator();
  let snapshot = coordinator.snapshot().unwrap();
  let policy = snapshot.policy.unwrap();
  let remaining_critical = policy.emergency_reserve_bytes.checked_sub(snapshot.critical_reserved_bytes).unwrap();
  let pressure =
    coordinator.reserve(MemoryOwner::Repair, remaining_critical, AdmissionClass::Critical(CriticalMemoryPurpose::BoundedRecovery)).unwrap();

  let error = verify::verify_checked(&engine, db_path).unwrap_err();
  assert!(matches!(error, aeordb::engine::errors::EngineError::ResourceExhausted(_)));

  let compatibility_report = verify::verify(&engine, db_path);
  assert!(compatibility_report.has_issues());
  assert_eq!(compatibility_report.verification_errors.len(), 1);

  drop(pressure);
  let report = verify::verify_checked(&engine, db_path).unwrap();
  assert!(report.verification_errors.is_empty());
  let owner = coordinator.snapshot().unwrap().owner(MemoryOwner::Repair).unwrap().clone();
  assert_eq!(owner.active_reservations, 0);
  assert_eq!(owner.reserved_bytes, 0);
}

#[test]
fn verify_reports_storage_metrics() {
  let (engine, temp) = create_temp_engine_for_tests();
  store_test_files(&engine);

  let db_path = temp.path().join("test.aeordb");
  let report = verify::verify(&engine, db_path.to_str().unwrap());

  assert!(report.file_size > 0, "File size should be > 0");
  assert!(report.chunk_data_size > 0, "Chunk data should be > 0");
  assert!(!report.hash_algorithm.is_empty(), "Hash algorithm should be reported");
}

#[test]
fn verify_reports_current_head_file_bytes_instead_of_serialized_file_records() {
  let (engine, temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let path = format!("/docs/{}/versioned.txt", "long-directory-name".repeat(8));
  let historical = vec![b'h'; 300_123];
  let current = b"current";

  let historical_record = ops.store_file_buffered(&ctx, &path, &historical, Some("text/plain")).unwrap();
  VersionManager::new(&engine).create_snapshot(&ctx, "before-overwrite", std::collections::HashMap::new()).unwrap();
  let current_record = ops.store_file_buffered(&ctx, &path, current, Some("text/plain")).unwrap();

  let db_path = temp.path().join("test.aeordb");
  let report = verify::verify_checked(&engine, db_path.to_str().unwrap()).unwrap();

  assert_eq!(
    report.logical_data_size,
    current.len() as u64,
    "logical data must describe current HEAD file content, not serialized FileRecord payloads or retained versions"
  );
  assert_eq!(report.retained_file_versions, 2, "the current and snapshotted file identities should each be counted once");
  assert_eq!(report.retained_logical_data_size, (historical.len() + current.len()) as u64);
  assert_eq!(report.non_head_retained_logical_data_size, historical.len() as u64);
  let hash_length = engine.hash_algo().hash_length();
  let expected_file_record_payload_size =
    3 * (historical_record.serialize(hash_length).unwrap().len() as u64 + current_record.serialize(hash_length).unwrap().len() as u64);
  assert_eq!(
    report.file_record_payload_size, expected_file_record_payload_size,
    "raw WAL payload accounting should remain available separately and include all three FileRecord aliases"
  );
}

#[test]
fn verify_separates_deleted_snapshot_history_from_an_empty_current_head() {
  let (engine, temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let historical = b"retained only by the snapshot";

  ops.store_file_buffered(&ctx, "/history/only.txt", historical, Some("text/plain")).unwrap();
  VersionManager::new(&engine).create_snapshot(&ctx, "retained-history", std::collections::HashMap::new()).unwrap();
  ops.delete_file(&ctx, "/history/only.txt").unwrap();

  let db_path = temp.path().join("test.aeordb");
  let report = verify::verify_checked(&engine, db_path.to_str().unwrap()).unwrap();

  assert_eq!(report.logical_data_size, 0);
  assert_eq!(report.retained_file_versions, 1);
  assert_eq!(report.retained_logical_data_size, historical.len() as u64);
  assert_eq!(report.non_head_retained_logical_data_size, historical.len() as u64);
}

#[test]
fn verify_counts_current_logical_bytes_through_btree_directories() {
  let (engine, temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let mut expected = 0u64;

  for index in 0..=aeordb::engine::btree::BTREE_CONVERSION_THRESHOLD {
    let content = vec![b'x'; index % 17];
    expected += content.len() as u64;
    ops.store_file_buffered(&ctx, &format!("/many/file-{index:04}.txt"), &content, Some("text/plain")).unwrap();
  }

  let db_path = temp.path().join("test.aeordb");
  let report = verify::verify_checked(&engine, db_path.to_str().unwrap()).unwrap();

  assert_eq!(report.logical_data_size, expected);
  assert_eq!(report.retained_file_versions, (aeordb::engine::btree::BTREE_CONVERSION_THRESHOLD + 1) as u64);
  assert_eq!(report.retained_logical_data_size, expected);
  assert_eq!(report.non_head_retained_logical_data_size, 0);
}

#[test]
fn verify_uses_content_then_path_aliases_for_legacy_retained_version_accounting() {
  let (engine, temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let path = "/legacy/layout.txt";
  let content = b"legacy alias fallback";
  let record = ops.store_file_buffered(&ctx, path, content, Some("text/plain")).unwrap();
  let algo = engine.hash_algo();
  let identity_key = aeordb::engine::file_identity_hash(path, record.content_type.as_deref(), &record.chunk_hashes, &algo).unwrap();
  let content_key = aeordb::engine::file_content_hash(&record.serialize(algo.hash_length()).unwrap(), &algo).unwrap();
  let db_path = temp.path().join("test.aeordb");

  engine.remove_kv_entry(&identity_key).unwrap();
  let content_fallback = verify::verify_checked(&engine, db_path.to_str().unwrap()).unwrap();
  assert_eq!(content_fallback.retained_file_versions, 1);
  assert_eq!(content_fallback.retained_logical_data_size, content.len() as u64);

  engine.remove_kv_entry(&content_key).unwrap();
  let path_fallback = verify::verify_checked(&engine, db_path.to_str().unwrap()).unwrap();
  assert_eq!(path_fallback.retained_file_versions, 1);
  assert_eq!(path_fallback.retained_logical_data_size, content.len() as u64);
}

#[test]
fn verify_checked_detects_a_live_wal_entry_missing_from_kv() {
  let (engine, temp) = create_temp_engine_for_tests();
  let key = engine.compute_hash(b"verify-missing-kv").unwrap();
  engine.store_entry(aeordb::engine::EntryType::Chunk, &key, b"payload").unwrap();
  engine.remove_kv_entry(&key).unwrap();

  let db_path = temp.path().join("test.aeordb");
  let report = verify::verify_checked(&engine, db_path.to_str().unwrap()).unwrap();

  assert_eq!(report.missing_kv_entries, 1);
  assert_eq!(report.missing_kv_details.len(), 1);
  assert!(report.has_issues());
}

#[test]
fn verify_and_repair_checked_returns_final_state_and_keeps_engine_available() {
  let (engine, temp) = create_temp_engine_for_tests();
  let missing_key = engine.compute_hash(b"verify-repair-missing-kv").unwrap();
  engine.store_entry(aeordb::engine::EntryType::Chunk, &missing_key, b"payload").unwrap();
  engine.remove_kv_entry(&missing_key).unwrap();

  let db_path = temp.path().join("test.aeordb");
  let db_path = db_path.to_str().unwrap();
  let report = verify::verify_and_repair_checked(&engine, db_path).unwrap();

  assert!(!report.has_issues(), "checked repair returned stale or unresolved diagnostics: {report:?}");
  assert!(report.repairs.iter().any(|repair| repair.contains("KV index rebuilt")), "successful repair was not reported: {report:?}");

  let post_repair_key = engine.compute_hash(b"verify-repair-engine-remains-available").unwrap();
  engine.store_entry(aeordb::engine::EntryType::Chunk, &post_repair_key, b"still-writable").unwrap();
  drop(engine);

  let reopened = StorageEngine::open(db_path).unwrap();
  let reopened_report = verify::verify_checked(&reopened, db_path).unwrap();
  assert!(!reopened_report.has_issues(), "repaired database did not reopen cleanly: {reopened_report:?}");
  assert!(reopened.has_entry(&missing_key).unwrap());
  assert!(reopened.has_entry(&post_repair_key).unwrap());
}

#[test]
fn verify_and_repair_checked_republishes_a_corrupt_hot_tail() {
  use std::io::{Seek, SeekFrom, Write};

  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("test.aeordb");
  let db_path = db_path.to_str().unwrap();
  {
    let engine = StorageEngine::create(db_path).unwrap();
    store_test_files(&engine);
  }

  let engine = StorageEngine::open(db_path).unwrap();
  let hot_tail_offset = engine.writer_read_lock().unwrap().file_header().hot_tail_offset;
  let mut file = std::fs::OpenOptions::new().read(true).write(true).open(db_path).unwrap();
  file.seek(SeekFrom::Start(hot_tail_offset)).unwrap();
  file.write_all(&[0]).unwrap();
  file.sync_data().unwrap();
  drop(file);

  let damaged = verify::verify_checked(&engine, db_path).unwrap();
  assert!(!damaged.invalid_hot_tail_voids.is_empty(), "test setup did not damage the hot tail");

  let repaired = verify::verify_and_repair_checked(&engine, db_path).unwrap();
  assert!(!repaired.has_issues(), "hot-tail repair did not produce a clean final report: {repaired:?}");
  assert!(repaired.repairs.iter().any(|repair| repair.contains("Hot-tail void snapshot republished")));
  drop(engine);

  let reopened = StorageEngine::open(db_path).unwrap();
  let reopened_report = verify::verify_checked(&reopened, db_path).unwrap();
  assert!(!reopened_report.has_issues(), "republished hot tail did not survive reopen: {reopened_report:?}");
}

#[test]
fn verify_disk_workspace_preserves_overwrite_and_delete_recreate_chronology() {
  let (engine, temp) = create_temp_engine_for_tests();
  let raw_key = engine.compute_hash(b"mutable-verify-key").unwrap();
  engine.store_entry(aeordb::engine::EntryType::Chunk, &raw_key, b"old").unwrap();
  engine.store_entry(aeordb::engine::EntryType::Chunk, &raw_key, b"new").unwrap();

  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&ctx, "/chronology/item.txt", b"first", Some("text/plain")).unwrap();
  ops.delete_file(&ctx, "/chronology/item.txt").unwrap();
  ops.store_file_buffered(&ctx, "/chronology/item.txt", b"second", Some("text/plain")).unwrap();

  let db_path = temp.path().join("test.aeordb");
  let report = verify::verify_checked(&engine, db_path.to_str().unwrap()).unwrap();

  assert_eq!(report.missing_kv_entries, 0, "healthy chronology produced missing KV rows: {:?}", report.missing_kv_details);
  assert_eq!(report.stale_kv_entries, 0, "healthy chronology produced stale KV rows: {:?}", report.stale_kv_details);
}

#[test]
fn verify_directory_walk_reports_children_hidden_by_normal_live_filtering() {
  let (engine, temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&ctx, "/docs/orphan.txt", b"payload", Some("text/plain")).unwrap();
  let path_key = aeordb::engine::directory_ops::file_path_hash("/docs/orphan.txt", &engine.hash_algo()).unwrap();
  engine.remove_kv_entry(&path_key).unwrap();

  let db_path = temp.path().join("test.aeordb");
  let report = verify::verify_checked(&engine, db_path.to_str().unwrap()).unwrap();

  assert!(report.missing_children.iter().any(|detail| detail.contains("/docs/orphan.txt")), "dead child was filtered out of verification");
}

#[test]
fn verify_checked_does_not_hide_malformed_snapshot_metadata() {
  let (engine, temp) = create_temp_engine_for_tests();
  let key = engine.compute_hash(b"malformed-snapshot-metadata").unwrap();
  engine
    .store_entry_typed(aeordb::engine::EntryType::Snapshot, &key, b"not-a-snapshot-record", aeordb::engine::kv_store::KV_TYPE_SNAPSHOT)
    .unwrap();

  let db_path = temp.path().join("test.aeordb");
  let report = verify::verify_checked(&engine, db_path.to_str().unwrap()).unwrap();

  assert!(report.has_issues(), "malformed snapshot metadata was reported as a clean database");
  assert!(
    !report.broken_snapshots.is_empty() || !report.verification_errors.is_empty(),
    "malformed snapshot metadata produced no diagnostic"
  );
}

#[test]
fn verify_checked_reports_a_corrupt_snapshot_tree_body_without_aborting_the_scan() {
  let (engine, temp) = create_temp_engine_for_tests();
  store_test_files(&engine);
  let snapshot =
    VersionManager::new(&engine).create_snapshot(&RequestContext::system(), "corrupt-tree", std::collections::HashMap::new()).unwrap();
  let root_entry = engine.get_kv_entry(&snapshot.root_hash).unwrap().unwrap();
  let root_header = engine.get_entry_header_including_deleted(&snapshot.root_hash).unwrap().unwrap();
  let value_offset = root_entry.offset + root_header.header_size() as u64 + u64::from(root_header.key_length);
  inject_corruption(temp.path().join("test.aeordb").to_str().unwrap(), value_offset, 1);

  let db_path = temp.path().join("test.aeordb");
  let report =
    verify::verify_checked(&engine, db_path.to_str().unwrap()).expect("structural corruption belongs in the verification report");

  assert!(report.has_issues());
  assert!(report.corrupt_hash > 0);
  assert!(
    report.broken_snapshots.iter().any(|detail| detail.contains("corrupt-tree") || detail.contains("corrupt")),
    "snapshot tree corruption produced no bounded snapshot diagnostic: {report:?}"
  );
}

#[test]
fn verify_checked_walks_clean_snapshot_roots_without_buffering_snapshot_inventory() {
  let (engine, temp) = create_temp_engine_for_tests();
  store_test_files(&engine);
  VersionManager::new(&engine).create_snapshot(&RequestContext::system(), "clean-snapshot", std::collections::HashMap::new()).unwrap();

  let db_path = temp.path().join("test.aeordb");
  let report = verify::verify_checked(&engine, db_path.to_str().unwrap()).unwrap();

  assert_eq!(report.snapshots_checked, 1);
  assert!(report.broken_snapshots.is_empty(), "clean snapshot was reported damaged: {:?}", report.broken_snapshots);
}

#[test]
fn verify_checked_does_not_hide_malformed_file_record_metadata() {
  let (engine, temp) = create_temp_engine_for_tests();
  let key = engine.compute_hash(b"malformed-file-record-metadata").unwrap();
  engine
    .store_entry_typed(aeordb::engine::EntryType::FileRecord, &key, b"not-a-file-record", aeordb::engine::kv_store::KV_TYPE_FILE_RECORD)
    .unwrap();

  let db_path = temp.path().join("test.aeordb");
  let report = verify::verify_checked(&engine, db_path.to_str().unwrap()).unwrap();

  assert!(report.has_issues(), "malformed FileRecord metadata was reported as a clean database");
  assert!(report.verification_errors.iter().any(|error| error.contains("FileRecord")), "malformed FileRecord produced no diagnostic");
}

#[test]
fn verify_checked_distinguishes_valid_task_storage_from_file_records() {
  let (engine, temp) = create_temp_engine_for_tests();
  TaskQueue::new(engine.clone()).enqueue("verification-proof", serde_json::json!({ "scope": "/docs" })).unwrap();

  let db_path = temp.path().join("test.aeordb");
  let report = verify::verify_checked(&engine, db_path.to_str().unwrap()).unwrap();

  assert!(!report.has_issues(), "valid task storage was misclassified as malformed file metadata: {report:?}");
}

#[test]
fn verify_checked_reports_a_malformed_task_registry() {
  let (engine, temp) = create_temp_engine_for_tests();
  let registry_key = blake3::hash(b"::aeordb:task:_registry").as_bytes().to_vec();
  engine.store_entry(aeordb::engine::EntryType::FileRecord, &registry_key, b"not-json").unwrap();

  let db_path = temp.path().join("test.aeordb");
  let report = verify::verify_checked(&engine, db_path.to_str().unwrap()).unwrap();

  assert!(report.has_issues());
  assert!(
    report.verification_errors.iter().any(|error| error.contains("task registry is malformed")),
    "missing task diagnostic: {report:?}"
  );
}

#[test]
fn verify_reports_voids() {
  let (engine, temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  store_test_files(&engine);

  // Delete a file to create garbage, then GC to create voids
  let ops = DirectoryOps::new(&engine);
  ops.delete_file(&ctx, "/docs/a.txt").unwrap();
  run_gc(&engine, &ctx, false).unwrap();

  let db_path = temp.path().join("test.aeordb");
  let report = verify::verify(&engine, db_path.to_str().unwrap());

  assert!(report.voids > 0, "Should have voids after GC");
  assert!(report.void_bytes > 0, "Void bytes should be > 0");
}

#[test]
fn verify_and_repair_rebuilds_kv() {
  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("test.aeordb");
  let db_str = db_path.to_str().unwrap();

  {
    let engine = StorageEngine::create(db_str).unwrap();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.ensure_root_directory(&ctx).unwrap();
    store_test_files(&engine);
  }

  // Delete KV to force rebuild on open
  let kv_path = format!("{}.kv", db_str);
  let _ = std::fs::remove_file(&kv_path);

  let engine = StorageEngine::open(db_str).unwrap();
  let report = verify::verify_and_repair(&engine, db_str);

  // After KV rebuild on open, the database should have entries
  assert!(report.total_entries > 0, "Should have scanned entries after KV rebuild");
}

#[test]
fn verify_reports_corrupt_entries() {
  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("test.aeordb");
  let db_str = db_path.to_str().unwrap();

  {
    let engine = StorageEngine::create(db_str).unwrap();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.ensure_root_directory(&ctx).unwrap();
    store_test_files(&engine);
  }

  // Inject corruption at ~33% of file
  let file_size = std::fs::metadata(db_str).unwrap().len();
  inject_corruption(db_str, file_size / 3, 64);

  // Delete KV to force rebuild
  let kv_path = format!("{}.kv", db_str);
  let _ = std::fs::remove_file(&kv_path);

  let engine = StorageEngine::open(db_str).unwrap();
  let report = verify::verify(&engine, db_str);

  // Should have scanned entries (some may be corrupt, some may survive)
  assert!(report.total_entries > 0, "Should have scanned some entries despite corruption");
}

#[test]
fn verify_entry_counts_match_stored_data() {
  let (engine, temp) = create_temp_engine_for_tests();
  store_test_files(&engine);

  let db_path = temp.path().join("test.aeordb");
  let report = verify::verify(&engine, db_path.to_str().unwrap());

  // We stored 3 files, so at minimum 3 file records and 3 chunks
  assert!(report.file_records >= 3, "Should have at least 3 file records, got {}", report.file_records);
  assert!(report.chunks >= 3, "Should have at least 3 chunks, got {}", report.chunks);
  // Directories: / + /docs + /images = at least 3
  assert!(report.directory_indexes >= 3, "Should have at least 3 directory indexes, got {}", report.directory_indexes);
  // Valid entries should equal total (clean database)
  assert_eq!(report.valid_entries, report.total_entries, "All entries should be valid in a clean database");
}
