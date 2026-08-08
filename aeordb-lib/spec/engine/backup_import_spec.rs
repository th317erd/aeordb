use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::sync::Arc;

use aeordb::engine::backup::{
  backup_contains_system_data, create_patch, export_full, export_version, import_backup, import_backup_with_mode, ExportResult, ImportMode,
  ImportResult,
};
use aeordb::engine::deletion_record::DeletionRecord;
use aeordb::engine::directory_entry::{deserialize_child_entries, serialize_child_entries, ChildEntry};
use aeordb::engine::entry_header::FLAG_SYSTEM;
use aeordb::engine::directory_ops::{file_path_hash, DirectoryOps};
use aeordb::engine::errors::EngineError;
use aeordb::engine::file_record::FileRecord;
use aeordb::engine::kv_store::KV_TYPE_FILE_RECORD;
use aeordb::engine::memory_coordinator::{AdmissionClass, MemoryOwner};
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::{btree_from_entries, btree_list_from_node, directory_path_hash, is_btree_format, EntryType};
use aeordb::engine::tree_walker::walk_version_tree;
use aeordb::engine::RequestContext;
use aeordb::engine::VersionManager;
use aeordb::server::create_temp_engine_for_tests;
use tempfile::TempDir;

// ─── Helpers ────────────────────────────────────────────────────────────

fn db_path(dir: &TempDir, name: &str) -> String {
  dir.path().join(name).to_str().unwrap().to_string()
}

fn setup_engine_with_files() -> (Arc<StorageEngine>, TempDir) {
  let ctx = RequestContext::system();
  let (engine, temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/docs/hello.txt", b"Hello World", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/docs/goodbye.txt", b"Goodbye World", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/images/photo.jpg", b"fake jpg data", Some("image/jpeg")).unwrap();

  (engine, temp)
}

fn export_to_path(engine: &StorageEngine, path: &str) -> ExportResult {
  let head = engine.head_hash().unwrap();
  export_version(engine, &head, path, false).unwrap()
}

fn add_file_to_backup(path: &str, file_path: &str, content: &[u8]) {
  let backup = StorageEngine::open_for_import(path).unwrap();
  let (backup_type, base_hash, _target_hash) = backup.backup_info().unwrap();
  let ops = DirectoryOps::new(&backup);
  ops.store_file_buffered(&RequestContext::system(), file_path, content, Some("application/octet-stream")).unwrap();
  let head = backup.head_hash().unwrap();
  let effective_base = if backup_type == 1 { &head } else { &base_hash };
  backup.set_backup_info(backup_type, effective_base, &head).unwrap();
}

fn install_legacy_system_root(source: &StorageEngine) -> Vec<u8> {
  let clean_root = source.head_hash().unwrap();
  let clean_root_entry = source.get_entry(&clean_root).unwrap().unwrap();
  let mut root_children = if is_btree_format(&clean_root_entry.2) {
    btree_list_from_node(&clean_root_entry.2, source, source.hash_algo().hash_length(), true).unwrap()
  } else {
    deserialize_child_entries(&clean_root_entry.2, source.hash_algo().hash_length(), clean_root_entry.0.entry_version).unwrap()
  };
  let system_path_hash = directory_path_hash("/.aeordb-system", &source.hash_algo()).unwrap();
  let system_hash = source.get_entry(&system_path_hash).unwrap().unwrap().2;
  root_children.push(ChildEntry {
    entry_type: EntryType::DirectoryIndex.to_u8(),
    hash: system_hash,
    total_size: 0,
    created_at: 1,
    updated_at: 1,
    name: ".aeordb-system".to_string(),
    content_type: None,
    virtual_time: 1,
    node_id: 1,
  });

  let legacy_root = if is_btree_format(&clean_root_entry.2) {
    btree_from_entries(source, root_children, source.hash_algo().hash_length(), &source.hash_algo()).unwrap()
  } else {
    root_children.sort_by(|left, right| left.name.cmp(&right.name));
    let root_data = serialize_child_entries(&root_children, source.hash_algo().hash_length()).unwrap();
    let root_hash = aeordb::engine::directory_content_hash(&root_data, &source.hash_algo()).unwrap();
    source.store_entry(EntryType::DirectoryIndex, &root_hash, &root_data).unwrap();
    root_hash
  };
  source.update_head(&legacy_root).unwrap();
  legacy_root
}

fn rewrite_artifact_file_record_path(artifact_path: &str, old_path: &str, new_path: &str) {
  let artifact = StorageEngine::open_for_import(artifact_path).unwrap();
  for (key, value) in artifact.entries_by_type(KV_TYPE_FILE_RECORD).unwrap() {
    let header = artifact.get_entry_header_including_deleted(&key).unwrap().unwrap();
    let mut record = FileRecord::deserialize(&value, artifact.hash_algo().hash_length(), header.entry_version).unwrap();
    if record.path == old_path {
      record.path = new_path.to_string();
      let rewritten = record.serialize_for_version(artifact.hash_algo().hash_length(), header.entry_version).unwrap();
      artifact.store_entry_with_flags_and_version(EntryType::FileRecord, &key, &rewritten, header.flags, header.entry_version).unwrap();
    }
  }
}

// ─── 1. test_import_full_export ─────────────────────────────────────────

#[test]
fn test_import_full_export() {
  let (source, source_temp) = setup_engine_with_files();
  let export_path = db_path(&source_temp, "export.aeordb");
  export_to_path(&source, &export_path);

  // Create a fresh target database
  let (target, _target_temp) = create_temp_engine_for_tests();

  let ctx = RequestContext::system();
  let result = import_backup(&ctx, &target, &export_path, false, false, false).unwrap();

  assert_eq!(result.backup_type, 1);
  assert!(result.files_imported >= 3, "expected at least 3 files imported, got {}", result.files_imported);
  assert!(result.chunks_imported >= 3, "expected at least 3 chunks imported, got {}", result.chunks_imported);
  assert!(result.directories_imported >= 3, "expected at least 3 dirs imported, got {}", result.directories_imported);
  assert_eq!(result.deletions_applied, 0);
}

#[test]
fn test_full_import_records_logical_write_metrics() {
  let (source, source_temp) = setup_engine_with_files();
  let export_path = db_path(&source_temp, "metrics-export.aeordb");
  export_to_path(&source, &export_path);

  let (target, _target_temp) = create_temp_engine_for_tests();
  let before = target.counters().snapshot();
  let result = import_backup(&RequestContext::system(), &target, &export_path, false, false, false).unwrap();
  let after = target.counters().snapshot();

  let expected_writes = result.chunks_imported.saturating_add(result.files_imported).saturating_add(result.directories_imported);
  assert_eq!(after.writes_total - before.writes_total, expected_writes);
  assert!(after.bytes_written_total > before.bytes_written_total, "imported chunks must contribute write throughput bytes");
}

// ─── 2. test_import_preserves_content ───────────────────────────────────

#[test]
fn test_import_preserves_content() {
  let (source, source_temp) = setup_engine_with_files();
  let export_path = db_path(&source_temp, "export.aeordb");
  export_to_path(&source, &export_path);

  let (target, _target_temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  import_backup(&ctx, &target, &export_path, false, true, false).unwrap();

  // After import+promote, we should be able to read the files via tree walking
  let target_head = target.head_hash().unwrap();
  let tree = walk_version_tree(&target, &target_head).unwrap();

  // Verify file paths exist in the tree
  assert!(tree.files.contains_key("/docs/hello.txt"), "hello.txt should exist after import");
  assert!(tree.files.contains_key("/docs/goodbye.txt"), "goodbye.txt should exist after import");
  assert!(tree.files.contains_key("/images/photo.jpg"), "photo.jpg should exist after import");
}

// ─── 3. test_import_does_not_promote_head ───────────────────────────────

#[test]
fn test_import_does_not_promote_head() {
  let (source, source_temp) = setup_engine_with_files();
  let export_path = db_path(&source_temp, "export.aeordb");
  let export_result = export_to_path(&source, &export_path);

  let (target, _target_temp) = create_temp_engine_for_tests();
  let original_head = target.head_hash().unwrap();

  let ctx = RequestContext::system();
  let result = import_backup(&ctx, &target, &export_path, false, false, false).unwrap();

  assert!(!result.head_promoted, "HEAD should NOT be promoted");
  let current_head = target.head_hash().unwrap();
  assert_eq!(current_head, original_head, "HEAD should remain unchanged when promote=false");
  // The version hash in the result should be the exported version
  assert_eq!(result.version_hash, export_result.version_hash);
}

// ─── 4. test_import_with_promote ────────────────────────────────────────

#[test]
fn test_import_with_promote() {
  let (source, source_temp) = setup_engine_with_files();
  let export_path = db_path(&source_temp, "export.aeordb");
  let export_result = export_to_path(&source, &export_path);

  let (target, _target_temp) = create_temp_engine_for_tests();

  let ctx = RequestContext::system();
  let result = import_backup(&ctx, &target, &export_path, false, true, false).unwrap();

  assert!(result.head_promoted, "HEAD should be promoted");
  let current_head = target.head_hash().unwrap();
  assert_eq!(current_head, export_result.version_hash, "HEAD should equal the imported version hash");
}

// ─── 5. test_import_patch_matching_base ─────────────────────────────────

#[test]
fn test_import_patch_matching_base() {
  let (source, source_temp) = setup_engine_with_files();
  let (target, _target_temp) = create_temp_engine_for_tests();
  let base = target.head_hash().unwrap();

  let head = source.head_hash().unwrap();
  let patch_path = db_path(&source_temp, "patch.aeordb");
  create_patch(&source, &base, &head, &patch_path).unwrap();

  let ctx = RequestContext::system();
  let result = import_backup(&ctx, &target, &patch_path, false, true, false).unwrap();
  assert_eq!(result.backup_type, 2);
  assert!(result.head_promoted);
  assert_eq!(target.head_hash().unwrap(), head);
}

// ─── 6. test_import_patch_wrong_base ────────────────────────────────────

#[test]
fn test_import_patch_wrong_base() {
  let (source, source_temp) = setup_engine_with_files();
  let (target, _target_temp) = create_temp_engine_for_tests();
  let base = target.head_hash().unwrap();

  let head = source.head_hash().unwrap();
  let patch_path = db_path(&source_temp, "patch.aeordb");
  create_patch(&source, &base, &head, &patch_path).unwrap();

  // Target HEAD is valid but semantically different from the patch base.
  DirectoryOps::new(&target).store_file_buffered(&RequestContext::system(), "/diverged.txt", b"diverged", Some("text/plain")).unwrap();

  let ctx = RequestContext::system();
  let result = import_backup(&ctx, &target, &patch_path, false, false, false);
  assert!(result.is_err(), "should fail when target HEAD doesn't match patch base");

  let err_msg = format!("{}", result.unwrap_err());
  assert!(err_msg.contains("does not match"), "error should mention mismatch, got: {}", err_msg);
}

// ─── 7. test_import_patch_wrong_base_force ──────────────────────────────

#[test]
fn test_import_patch_wrong_base_force() {
  let (source, source_temp) = setup_engine_with_files();
  let (target, _target_temp) = create_temp_engine_for_tests();
  let base = target.head_hash().unwrap();

  let head = source.head_hash().unwrap();
  let patch_path = db_path(&source_temp, "patch.aeordb");
  create_patch(&source, &base, &head, &patch_path).unwrap();

  // Target HEAD is different, but we use force=true
  let different_hash = vec![0xFF; 32];
  target.update_head(&different_hash).unwrap();

  let ctx = RequestContext::system();
  let result = import_backup(&ctx, &target, &patch_path, true, false, false);
  assert!(result.is_ok(), "should succeed with force=true, got: {:?}", result.err());
}

// ─── 8. test_import_patch_applies_deletions ─────────────────────────────

#[test]
fn test_import_patch_applies_deletions() {
  let ctx = RequestContext::system();
  // Create two engines to simulate diff with deletions
  let (engine_a, _temp_a) = create_temp_engine_for_tests();
  let ops_a = DirectoryOps::new(&engine_a);
  ops_a.store_file_buffered(&ctx, "/keep.txt", b"keep", Some("text/plain")).unwrap();
  ops_a.store_file_buffered(&ctx, "/remove.txt", b"remove me", Some("text/plain")).unwrap();
  let tree_a = walk_version_tree(&engine_a, &engine_a.head_hash().unwrap()).unwrap();

  let (engine_b, _temp_b) = create_temp_engine_for_tests();
  let ops_b = DirectoryOps::new(&engine_b);
  ops_b.store_file_buffered(&ctx, "/keep.txt", b"keep", Some("text/plain")).unwrap();
  let tree_b = walk_version_tree(&engine_b, &engine_b.head_hash().unwrap()).unwrap();

  let diff = aeordb::engine::tree_walker::diff_trees(&tree_a, &tree_b);

  // Verify our setup: /remove.txt should be in deleted
  assert!(diff.deleted.contains(&"/remove.txt".to_string()), "expected /remove.txt in deleted set");
}

// ─── 9. test_import_result_display ──────────────────────────────────────

#[test]
fn test_import_result_display() {
  let result = ImportResult {
    backup_type: 1,
    entries_imported: 15,
    chunks_imported: 10,
    files_imported: 3,
    directories_imported: 2,
    deletions_applied: 0,
    version_hash: vec![0xAB, 0xCD, 0xEF],
    head_promoted: false,
  };

  let display = format!("{}", result);
  assert!(display.contains("Full export imported."), "should contain header, got: {}", display);
  assert!(display.contains("Entries: 15"), "should show entries count");
  assert!(display.contains("Chunks: 10"), "should show chunks count");
  assert!(display.contains("Files: 3"), "should show files count");
  assert!(display.contains("Directories: 2"), "should show dirs count");
  assert!(display.contains("Deletions: 0"), "should show deletions count");
  assert!(display.contains("abcdef"), "should show hex version hash");
  assert!(display.contains("has NOT been changed"), "should indicate HEAD not changed");
  assert!(display.contains("aeordb promote"), "should suggest promote command");
}

#[test]
fn test_import_result_display_promoted() {
  let result = ImportResult {
    backup_type: 2,
    entries_imported: 5,
    chunks_imported: 2,
    files_imported: 1,
    directories_imported: 1,
    deletions_applied: 1,
    version_hash: vec![0x11, 0x22],
    head_promoted: true,
  };

  let display = format!("{}", result);
  assert!(display.contains("Patch imported."), "should say Patch");
  assert!(display.contains("has been promoted."), "should indicate HEAD promoted");
  assert!(display.contains("Deletions: 1"), "should show deletions");
}

// ─── 10. test_import_chunk_dedup ────────────────────────────────────────

#[test]
fn test_import_chunk_dedup() {
  let (source, source_temp) = setup_engine_with_files();
  let export_path = db_path(&source_temp, "export.aeordb");
  export_to_path(&source, &export_path);

  // Import into source itself (which already has the chunks)
  let ctx = RequestContext::system();
  let result = import_backup(&ctx, &source, &export_path, false, false, false).unwrap();

  assert_eq!(result.chunks_imported, 0, "no chunks should be imported since they already exist");
}

// ─── 11. test_round_trip_export_import ──────────────────────────────────

#[test]
fn test_round_trip_export_import() {
  let (source, source_temp) = setup_engine_with_files();
  let export_path = db_path(&source_temp, "export.aeordb");
  export_to_path(&source, &export_path);

  // Import into fresh target with promote
  let (target, _target_temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let import_result = import_backup(&ctx, &target, &export_path, false, true, false).unwrap();

  assert!(import_result.head_promoted);

  // Walk source and target trees, compare file sets
  let source_head = source.head_hash().unwrap();
  let source_tree = walk_version_tree(&source, &source_head).unwrap();

  let target_head = target.head_hash().unwrap();
  let target_tree = walk_version_tree(&target, &target_head).unwrap();

  // Same files should exist
  let mut source_paths: Vec<String> = source_tree.files.keys().cloned().collect();
  let mut target_paths: Vec<String> = target_tree.files.keys().cloned().collect();
  source_paths.sort();
  target_paths.sort();

  assert_eq!(source_paths, target_paths, "exported and imported file sets should match");

  // Same directories should exist
  let mut source_dirs: Vec<String> = source_tree.directories.keys().cloned().collect();
  let mut target_dirs: Vec<String> = target_tree.directories.keys().cloned().collect();
  source_dirs.sort();
  target_dirs.sort();

  assert_eq!(source_dirs, target_dirs, "exported and imported directory sets should match");
}

// ─── 12. test_import_nonexistent_file ───────────────────────────────────

#[test]
fn test_import_nonexistent_file() {
  let (target, _target_temp) = create_temp_engine_for_tests();

  let ctx = RequestContext::system();
  let result = import_backup(&ctx, &target, "/nonexistent/path/backup.aeordb", false, false, false);
  assert!(result.is_err(), "should fail for nonexistent backup file");
}

// ─── 13. test_import_full_export_type_1 ─────────────────────────────────

#[test]
fn test_import_full_export_type_1() {
  let (source, source_temp) = setup_engine_with_files();
  let export_path = db_path(&source_temp, "export.aeordb");
  export_to_path(&source, &export_path);

  let (target, _target_temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let result = import_backup(&ctx, &target, &export_path, false, false, false).unwrap();

  // Full export should not attempt deletion processing
  assert_eq!(result.deletions_applied, 0);
  assert_eq!(result.backup_type, 1);
}

// ─── 14. test_import_empty_export ───────────────────────────────────────

#[test]
fn test_import_empty_export() {
  let (source, _source_temp) = create_temp_engine_for_tests();
  let export_temp = tempfile::tempdir().unwrap();
  let export_path = db_path(&export_temp, "empty_export.aeordb");
  export_to_path(&source, &export_path);

  let (target, _target_temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let result = import_backup(&ctx, &target, &export_path, false, true, false).unwrap();

  assert_eq!(result.files_imported, 0);
  assert_eq!(result.chunks_imported, 0);
  // May have directory entries for root
  assert!(result.head_promoted);
}

// ─── 15. test_import_version_hash_in_result ─────────────────────────────

#[test]
fn test_import_version_hash_in_result() {
  let (source, source_temp) = setup_engine_with_files();
  let export_path = db_path(&source_temp, "export.aeordb");
  let export_result = export_to_path(&source, &export_path);

  let (target, _target_temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let import_result = import_backup(&ctx, &target, &export_path, false, false, false).unwrap();

  assert_eq!(import_result.version_hash, export_result.version_hash, "import result version_hash should match export version_hash");
}

// ─── 16. test_import_patch_base_check_skipped_for_full_export ───────────

#[test]
fn test_import_full_export_skips_base_check() {
  let (source, source_temp) = setup_engine_with_files();
  let export_path = db_path(&source_temp, "export.aeordb");
  export_to_path(&source, &export_path);

  // Target has a totally different HEAD, but full exports don't check base
  let (target, _target_temp) = create_temp_engine_for_tests();
  target.update_head(&[0xFF; 32]).unwrap();

  let ctx = RequestContext::system();
  let result = import_backup(&ctx, &target, &export_path, false, false, false);
  assert!(result.is_ok(), "full export import should not check base version, got: {:?}", result.err());
}

// ─── 17. test_import_entries_total_count ─────────────────────────────────

#[test]
fn test_import_entries_total_count() {
  let (source, source_temp) = setup_engine_with_files();
  let export_path = db_path(&source_temp, "export.aeordb");
  export_to_path(&source, &export_path);

  let (target, _target_temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let result = import_backup(&ctx, &target, &export_path, false, false, false).unwrap();

  // entries_imported should equal sum of chunks + files + dirs + deletions
  assert_eq!(
    result.entries_imported,
    result.chunks_imported + result.files_imported + result.directories_imported + result.deletions_applied,
    "entries_imported should be the sum of all sub-counts"
  );
}

#[test]
fn test_backup_system_data_inspection_propagates_entry_read_failure() {
  let temp = tempfile::tempdir().unwrap();
  let path = db_path(&temp, "corrupt-inspection.aeordb");
  let backup = StorageEngine::create(&path).unwrap();
  let key = vec![0xA5; backup.hash_algo().hash_length()];
  backup.store_entry_with_flags(EntryType::FileRecord, &key, b"system-record", FLAG_SYSTEM).unwrap();
  let entry = backup.get_kv_entry(&key).unwrap().unwrap();
  let mut file = OpenOptions::new().write(true).open(&path).unwrap();
  file.seek(SeekFrom::Start(entry.offset)).unwrap();
  file.write_all(&0u32.to_le_bytes()).unwrap();
  file.sync_data().unwrap();

  let error = backup_contains_system_data(&backup).unwrap_err();

  assert!(error.to_string().contains("magic") || error.to_string().contains("Magic"), "unexpected error: {error}");
}

#[test]
fn test_backup_system_data_inspection_uses_registry_instead_of_header_flag() {
  let (source, source_temp) = setup_engine_with_files();
  let export_path = db_path(&source_temp, "unflagged-portable-system.aeordb");
  export_to_path(&source, &export_path);
  add_file_to_backup(&export_path, "/.aeordb-system/users/user.json", br#"{"name":"portable"}"#);

  let backup = StorageEngine::open_for_import(&export_path).unwrap();
  for (key, value) in backup.entries_by_type(KV_TYPE_FILE_RECORD).unwrap() {
    let header = backup.get_entry_header_including_deleted(&key).unwrap().unwrap();
    let record = FileRecord::deserialize(&value, backup.hash_algo().hash_length(), header.entry_version).unwrap();
    if record.path == "/.aeordb-system/users/user.json" {
      backup.store_entry_with_flags_and_version(EntryType::FileRecord, &key, &value, 0, header.entry_version).unwrap();
    }
  }

  assert!(backup_contains_system_data(&backup).unwrap(), "portable protected state must be identified from the registry path policy");
}

#[test]
fn test_import_memory_pressure_fails_before_target_mutation() {
  let (source, source_temp) = setup_engine_with_files();
  let export_path = db_path(&source_temp, "pressure-import.aeordb");
  export_to_path(&source, &export_path);
  let (target, _target_temp) = create_temp_engine_for_tests();
  let head_before = target.head_hash().unwrap();
  let entries_before = target.kv_entry_count().unwrap();
  let coordinator = target.memory_coordinator();
  let snapshot = coordinator.snapshot().unwrap();
  let policy = snapshot.policy.unwrap();
  let remaining = policy.ordinary_limit_bytes().saturating_sub(snapshot.accounted_bytes);
  let _pressure = coordinator.reserve(MemoryOwner::Task, remaining.saturating_sub(4 * 1024), AdmissionClass::Workload).unwrap();

  let error = import_backup(&RequestContext::system(), &target, &export_path, false, true, false).unwrap_err();

  assert!(matches!(error, EngineError::ResourceExhausted(_)), "unexpected error: {error}");
  assert_eq!(target.head_hash().unwrap(), head_before);
  assert_eq!(target.kv_entry_count().unwrap(), entries_before);
  let owner = coordinator.snapshot().unwrap().owner(MemoryOwner::BackupRestore).unwrap().clone();
  assert_eq!(owner.reserved_bytes, 0);
  assert_eq!(owner.active_reservations, 0);
}

#[test]
fn test_sparse_patch_memory_pressure_fails_before_target_mutation() {
  let (source, source_temp) = setup_engine_with_files();
  let (target, _target_temp) = create_temp_engine_for_tests();
  let patch_path = db_path(&source_temp, "pressure-patch-import.aeordb");
  create_patch(&source, &target.head_hash().unwrap(), &source.head_hash().unwrap(), &patch_path).unwrap();
  let head_before = target.head_hash().unwrap();
  let entries_before = target.kv_entry_count().unwrap();
  let coordinator = target.memory_coordinator();
  let snapshot = coordinator.snapshot().unwrap();
  let policy = snapshot.policy.unwrap();
  let remaining = policy.ordinary_limit_bytes().saturating_sub(snapshot.accounted_bytes);
  let _pressure = coordinator.reserve(MemoryOwner::Task, remaining.saturating_sub(4 * 1024), AdmissionClass::Workload).unwrap();

  let error = import_backup(&RequestContext::system(), &target, &patch_path, false, true, false).unwrap_err();

  assert!(matches!(error, EngineError::ResourceExhausted(_)), "unexpected error: {error}");
  assert_eq!(target.head_hash().unwrap(), head_before);
  assert_eq!(target.kv_entry_count().unwrap(), entries_before);
  let owner = coordinator.snapshot().unwrap().owner(MemoryOwner::BackupRestore).unwrap().clone();
  assert_eq!(owner.reserved_bytes, 0);
  assert_eq!(owner.active_reservations, 0);
}

#[test]
fn test_import_rejects_unknown_backup_type_before_target_mutation() {
  let (source, source_temp) = setup_engine_with_files();
  let export_path = db_path(&source_temp, "unknown-type.aeordb");
  export_to_path(&source, &export_path);
  {
    let backup = StorageEngine::open_for_import(&export_path).unwrap();
    let (_, base_hash, target_hash) = backup.backup_info().unwrap();
    backup.set_backup_info(3, &base_hash, &target_hash).unwrap();
  }
  let (target, _target_temp) = create_temp_engine_for_tests();
  let head_before = target.head_hash().unwrap();
  let entries_before = target.kv_entry_count().unwrap();

  let error = import_backup(&RequestContext::system(), &target, &export_path, false, true, false).unwrap_err();

  assert!(matches!(error, EngineError::InvalidInput(_)), "unexpected error: {error}");
  assert_eq!(target.head_hash().unwrap(), head_before);
  assert_eq!(target.kv_entry_count().unwrap(), entries_before);
}

#[test]
fn test_import_rejects_corrupt_entry_body_before_target_mutation() {
  let (source, source_temp) = setup_engine_with_files();
  let export_path = db_path(&source_temp, "corrupt-body.aeordb");
  export_to_path(&source, &export_path);
  let corrupt_key = {
    let backup = StorageEngine::open_for_import(&export_path).unwrap();
    aeordb::engine::directory_ops::file_path_hash("/docs/goodbye.txt", &backup.hash_algo()).unwrap()
  };
  let (entry, header) = {
    let backup = StorageEngine::open_for_import(&export_path).unwrap();
    (backup.get_kv_entry(&corrupt_key).unwrap().unwrap(), backup.get_entry_header_including_deleted(&corrupt_key).unwrap().unwrap())
  };
  let value_offset = entry.offset + header.header_size() as u64 + u64::from(header.key_length);
  let mut file = OpenOptions::new().write(true).open(&export_path).unwrap();
  file.seek(SeekFrom::Start(value_offset)).unwrap();
  file.write_all(&[0xFF]).unwrap();
  file.sync_data().unwrap();
  drop(file);

  let (target, _target_temp) = create_temp_engine_for_tests();
  let head_before = target.head_hash().unwrap();
  let entries_before = target.kv_entry_count().unwrap();

  let error = import_backup(&RequestContext::system(), &target, &export_path, false, true, false).unwrap_err();

  assert!(matches!(error, EngineError::CorruptEntry { .. }), "unexpected error: {error}");
  assert_eq!(target.head_hash().unwrap(), head_before);
  assert_eq!(target.kv_entry_count().unwrap(), entries_before, "corrupt backup must fail before importing earlier entries");
}

#[test]
fn test_import_rejects_unknown_protected_path_before_target_mutation() {
  let (source, source_temp) = setup_engine_with_files();
  let export_path = db_path(&source_temp, "unknown-protected.aeordb");
  export_to_path(&source, &export_path);
  add_file_to_backup(&export_path, "/.aeordb-future/secret.bin", b"must not cross import boundary");

  let (target, _target_temp) = create_temp_engine_for_tests();
  let head_before = target.head_hash().unwrap();
  let entries_before = target.kv_entry_count().unwrap();

  let error = import_backup(&RequestContext::system(), &target, &export_path, false, true, false).unwrap_err();

  assert!(matches!(error, EngineError::SystemFamilyPolicy { .. }), "unexpected error: {error}");
  assert_eq!(target.head_hash().unwrap(), head_before);
  assert_eq!(target.kv_entry_count().unwrap(), entries_before, "policy failure must precede every target write");
}

#[test]
fn test_privileged_import_omits_node_local_credentials() {
  let (source, source_temp) = setup_engine_with_files();
  let export_path = db_path(&source_temp, "node-local-credential.aeordb");
  export_to_path(&source, &export_path);
  add_file_to_backup(&export_path, "/.aeordb-system/api-keys/secret.bin", b"node-local credential");

  let (target, _target_temp) = create_temp_engine_for_tests();
  import_backup(&RequestContext::system(), &target, &export_path, false, true, true).unwrap();

  let ops = DirectoryOps::new(&target);
  assert_eq!(ops.read_file_buffered("/docs/hello.txt").unwrap(), b"Hello World");
  assert!(matches!(ops.read_file_buffered("/.aeordb-system/api-keys/secret.bin"), Err(EngineError::NotFound(_))));
}

#[test]
fn test_privileged_full_import_preserves_portable_state_and_snapshots() {
  let (source, source_temp) = setup_engine_with_files();
  let context = RequestContext::system();
  DirectoryOps::new(&source)
    .store_file_buffered(&context, "/.aeordb-system/users/portable-user.json", br#"{"name":"portable"}"#, Some("application/json"))
    .unwrap();
  VersionManager::new(&source).create_snapshot(&context, "before-update", HashMap::new()).unwrap();
  DirectoryOps::new(&source).store_file_buffered(&context, "/docs/after.txt", b"after", Some("text/plain")).unwrap();
  let export_path = db_path(&source_temp, "portable-full.aeordb");
  export_full(&source, &export_path, true).unwrap();

  let backup = StorageEngine::open_for_import(&export_path).unwrap();
  assert!(backup_contains_system_data(&backup).unwrap());
  drop(backup);

  let (target, _target_temp) = create_temp_engine_for_tests();
  import_backup(&context, &target, &export_path, false, true, true).unwrap();

  assert_eq!(DirectoryOps::new(&target).read_file_buffered("/.aeordb-system/users/portable-user.json").unwrap(), br#"{"name":"portable"}"#,);
  let snapshots = VersionManager::new(&target).list_snapshots().unwrap();
  let snapshot = snapshots.iter().find(|snapshot| snapshot.name == "before-update").expect("snapshot must be imported");
  let snapshot_tree = walk_version_tree(&target, &snapshot.root_hash).unwrap();
  assert!(snapshot_tree.files.contains_key("/docs/hello.txt"));
  assert!(!snapshot_tree.files.contains_key("/docs/after.txt"));
}

#[test]
fn test_sparse_patch_reads_unchanged_entries_from_target_overlay() {
  let context = RequestContext::system();
  let (source, source_temp) = create_temp_engine_for_tests();
  let (target, _target_temp) = create_temp_engine_for_tests();
  let source_ops = DirectoryOps::new(&source);
  let target_ops = DirectoryOps::new(&target);
  source_ops.store_file_buffered(&context, "/keep.txt", b"keep", Some("text/plain")).unwrap();
  let base = source.head_hash().unwrap();
  let base_export_path = db_path(&source_temp, "overlay-base.aeordb");
  export_version(&source, &base, &base_export_path, false).unwrap();
  import_backup(&context, &target, &base_export_path, false, true, false).unwrap();
  assert_eq!(target.head_hash().unwrap(), base);
  source_ops.store_file_buffered(&context, "/added.txt", b"added", Some("text/plain")).unwrap();
  let patch_path = db_path(&source_temp, "overlay.aeordb");
  create_patch(&source, &base, &source.head_hash().unwrap(), &patch_path).unwrap();

  let result = import_backup(&context, &target, &patch_path, false, true, false).unwrap();

  assert!(result.head_promoted);
  assert_eq!(target_ops.read_file_buffered("/keep.txt").unwrap(), b"keep");
  assert_eq!(target_ops.read_file_buffered("/added.txt").unwrap(), b"added");
}

#[test]
fn test_sparse_patch_accepts_logically_equivalent_base_after_filtered_full_import() {
  let context = RequestContext::system();
  let (source, source_temp) = setup_engine_with_files();
  let source_operations = DirectoryOps::new(&source);
  source_operations.store_file_buffered(&context, "/.aeordb-system/api-keys/local.json", b"node-local", Some("application/json")).unwrap();
  let raw_base = install_legacy_system_root(&source);
  let base_export = db_path(&source_temp, "filtered-patch-base.aeordb");
  export_version(&source, &raw_base, &base_export, false).unwrap();

  let (target, _target_temp) = create_temp_engine_for_tests();
  let base_import = import_backup(&context, &target, &base_export, false, true, false).unwrap();
  assert_ne!(base_import.version_hash, raw_base, "filtered export must reproduce the logical/raw root mismatch");
  assert_eq!(target.head_hash().unwrap(), base_import.version_hash);

  source_operations.store_file_buffered(&context, "/docs/after.txt", b"after", Some("text/plain")).unwrap();
  let patch_path = db_path(&source_temp, "filtered-base.patch.aeordb");
  create_patch(&source, &raw_base, &source.head_hash().unwrap(), &patch_path).unwrap();

  let result = import_backup(&context, &target, &patch_path, false, true, false).unwrap();

  assert!(result.head_promoted);
  assert_eq!(DirectoryOps::new(&target).read_file_buffered("/docs/hello.txt").unwrap(), b"Hello World");
  assert_eq!(DirectoryOps::new(&target).read_file_buffered("/docs/after.txt").unwrap(), b"after");
}

#[test]
fn test_sparse_patch_deletion_remains_verifiable_after_restart() {
  let context = RequestContext::system();
  let (source, source_temp) = create_temp_engine_for_tests();
  let source_operations = DirectoryOps::new(&source);
  source_operations.store_file_buffered(&context, "/keep.txt", b"keep", Some("text/plain")).unwrap();
  source_operations.store_file_buffered(&context, "/remove.txt", b"remove", Some("text/plain")).unwrap();
  let base = source.head_hash().unwrap();
  let base_export = db_path(&source_temp, "deletion-base.aeordb");
  export_version(&source, &base, &base_export, false).unwrap();

  let (target, target_temp) = create_temp_engine_for_tests();
  let target_path = db_path(&target_temp, "test.aeordb");
  import_backup(&context, &target, &base_export, false, true, false).unwrap();
  source_operations.delete_file(&context, "/remove.txt").unwrap();
  let patch_path = db_path(&source_temp, "deletion.patch.aeordb");
  create_patch(&source, &base, &source.head_hash().unwrap(), &patch_path).unwrap();

  import_backup(&context, &target, &patch_path, false, true, false).unwrap();
  assert!(matches!(DirectoryOps::new(&target).read_file_buffered("/remove.txt"), Err(EngineError::NotFound(_))));
  target.shutdown().unwrap();
  drop(target);

  let reopened = StorageEngine::open(&target_path).unwrap();
  let report = aeordb::engine::verify::verify_checked(&reopened, &target_path).unwrap();
  assert_eq!(report.missing_kv_entries, 0, "patch deletion must leave durable replay evidence");
  assert!(!report.has_issues(), "reopened patch target must verify cleanly: {report:?}");
}

#[test]
fn test_sparse_patch_preserves_namespace_permissions() {
  let context = RequestContext::system();
  let (source, source_temp) = create_temp_engine_for_tests();
  let (target, _target_temp) = create_temp_engine_for_tests();
  let base = source.head_hash().unwrap();
  DirectoryOps::new(&source)
    .store_file_buffered(&context, "/docs/.aeordb-permissions", br#"{"inherit":true}"#, Some("application/json"))
    .unwrap();
  let patch_path = db_path(&source_temp, "permissions.aeordb");
  create_patch(&source, &base, &source.head_hash().unwrap(), &patch_path).unwrap();

  import_backup(&context, &target, &patch_path, false, true, false).unwrap();

  assert_eq!(DirectoryOps::new(&target).read_file_buffered("/docs/.aeordb-permissions").unwrap(), br#"{"inherit":true}"#);
}

#[test]
fn test_sparse_patch_does_not_rewrite_unchanged_overlay_files() {
  let context = RequestContext::system();
  let (source, source_temp) = create_temp_engine_for_tests();
  let source_operations = DirectoryOps::new(&source);
  for index in 0..32 {
    source_operations
      .store_file_buffered(&context, &format!("/bulk/file-{index:02}.txt"), format!("value-{index}").as_bytes(), Some("text/plain"))
      .unwrap();
  }
  let base = source.head_hash().unwrap();
  let base_export = db_path(&source_temp, "sparse-metrics-base.aeordb");
  export_version(&source, &base, &base_export, false).unwrap();
  let (target, _target_temp) = create_temp_engine_for_tests();
  import_backup(&context, &target, &base_export, false, true, false).unwrap();
  source_operations.store_file_buffered(&context, "/bulk/added.txt", b"added", Some("text/plain")).unwrap();
  let patch_path = db_path(&source_temp, "sparse-metrics.aeordb");
  create_patch(&source, &base, &source.head_hash().unwrap(), &patch_path).unwrap();
  let before = target.counters().snapshot();

  let result = import_backup(&context, &target, &patch_path, false, true, false).unwrap();
  let after = target.counters().snapshot();

  assert_eq!(result.files_imported, 1, "only the added file should mutate its path alias");
  let expected_writes = result
    .chunks_imported
    .saturating_add(result.files_imported)
    .saturating_add(result.directories_imported)
    .saturating_add(result.deletions_applied)
    .saturating_add(u64::from(result.head_promoted));
  assert_eq!(after.writes_total - before.writes_total, expected_writes);
  assert!(expected_writes < 10, "one-file patch unexpectedly rewrote the 32-file base: {expected_writes} writes");
}

#[test]
fn test_sparse_patch_omits_rooted_node_local_payload_and_rebuilds_root() {
  let context = RequestContext::system();
  let (source, source_temp) = create_temp_engine_for_tests();
  let (target, _target_temp) = create_temp_engine_for_tests();
  let base = source.head_hash().unwrap();
  DirectoryOps::new(&source).store_file_buffered(&context, "/ordinary.txt", b"ordinary", Some("text/plain")).unwrap();
  let patch_path = db_path(&source_temp, "rooted-node-local.aeordb");
  create_patch(&source, &base, &source.head_hash().unwrap(), &patch_path).unwrap();
  add_file_to_backup(&patch_path, "/.aeordb-system/api-keys/patch-only.json", b"node-local");
  let patch = StorageEngine::open_for_import(&patch_path).unwrap();
  let (_, patch_base, _) = patch.backup_info().unwrap();
  let legacy_root = install_legacy_system_root(&patch);
  patch.set_backup_info(2, &patch_base, &legacy_root).unwrap();
  drop(patch);

  let result = import_backup(&context, &target, &patch_path, false, true, true).unwrap();

  assert!(result.head_promoted);
  assert_eq!(DirectoryOps::new(&target).read_file_buffered("/ordinary.txt").unwrap(), b"ordinary");
  assert!(matches!(
    DirectoryOps::new(&target).read_file_buffered("/.aeordb-system/api-keys/patch-only.json"),
    Err(EngineError::NotFound(_))
  ));
  let tree = walk_version_tree(&target, &target.head_hash().unwrap()).unwrap();
  assert!(!tree.files.contains_key("/.aeordb-system/api-keys/patch-only.json"));
}

#[test]
fn test_sparse_patch_rejects_unknown_leaf_before_target_mutation() {
  let context = RequestContext::system();
  let (source, source_temp) = create_temp_engine_for_tests();
  let (target, _target_temp) = create_temp_engine_for_tests();
  let base = source.head_hash().unwrap();
  DirectoryOps::new(&source).store_file_buffered(&context, "/ordinary.txt", b"ordinary", Some("text/plain")).unwrap();
  let patch_path = db_path(&source_temp, "unknown-leaf.aeordb");
  create_patch(&source, &base, &source.head_hash().unwrap(), &patch_path).unwrap();
  add_file_to_backup(&patch_path, "/.aeordb-future/unknown.bin", b"unknown");
  let head_before = target.head_hash().unwrap();
  let entries_before = target.kv_entry_count().unwrap();

  let error = import_backup(&context, &target, &patch_path, false, true, true).unwrap_err();

  assert!(matches!(error, EngineError::SystemFamilyPolicy { .. }), "unexpected error: {error}");
  assert_eq!(target.head_hash().unwrap(), head_before);
  assert_eq!(target.kv_entry_count().unwrap(), entries_before);
}

#[test]
fn test_sparse_patch_rejects_structural_leaf_before_target_mutation() {
  assert_sparse_patch_rejects_rewritten_record_path("/.aeordb-system", "system_family_structural_leaf");
}

#[test]
fn test_sparse_patch_rejects_embedded_path_mismatch_before_target_mutation() {
  assert_sparse_patch_rejects_rewritten_record_path("/different.txt", "does not match traversed path");
}

fn assert_sparse_patch_rejects_rewritten_record_path(rewritten_path: &str, expected_error: &str) {
  let context = RequestContext::system();
  let (source, source_temp) = create_temp_engine_for_tests();
  let (target, _target_temp) = create_temp_engine_for_tests();
  let base = source.head_hash().unwrap();
  DirectoryOps::new(&source).store_file_buffered(&context, "/ordinary.txt", b"ordinary", Some("text/plain")).unwrap();
  let patch_path = db_path(&source_temp, "rewritten-record.aeordb");
  create_patch(&source, &base, &source.head_hash().unwrap(), &patch_path).unwrap();
  rewrite_artifact_file_record_path(&patch_path, "/ordinary.txt", rewritten_path);
  let head_before = target.head_hash().unwrap();
  let entries_before = target.kv_entry_count().unwrap();

  let error = import_backup(&context, &target, &patch_path, false, true, true).unwrap_err();

  assert!(error.to_string().contains(expected_error), "unexpected error: {error}");
  assert_eq!(target.head_hash().unwrap(), head_before);
  assert_eq!(target.kv_entry_count().unwrap(), entries_before);
}

#[test]
fn test_sparse_patch_omits_node_local_deletion() {
  let context = RequestContext::system();
  let (source, source_temp) = create_temp_engine_for_tests();
  let (target, _target_temp) = create_temp_engine_for_tests();
  let base = source.head_hash().unwrap();
  let node_local_path = "/.aeordb-system/api-keys/keep-local.json";
  DirectoryOps::new(&source).store_file_buffered(&context, "/ordinary.txt", b"ordinary", Some("text/plain")).unwrap();
  DirectoryOps::new(&target).store_file_buffered(&context, node_local_path, b"keep-local", Some("application/json")).unwrap();
  assert_eq!(target.head_hash().unwrap(), base, "detached node-local state must not alter the namespace root");
  let patch_path = db_path(&source_temp, "node-local-deletion.aeordb");
  create_patch(&source, &base, &source.head_hash().unwrap(), &patch_path).unwrap();
  let patch = StorageEngine::open_for_import(&patch_path).unwrap();
  let deletion = DeletionRecord::new(node_local_path.to_string(), Some("malicious".to_string()));
  patch
    .store_entry(EntryType::DeletionRecord, &file_path_hash(node_local_path, &patch.hash_algo()).unwrap(), &deletion.serialize())
    .unwrap();
  drop(patch);

  import_backup(&context, &target, &patch_path, false, true, true).unwrap();

  assert_eq!(DirectoryOps::new(&target).read_file_buffered(node_local_path).unwrap(), b"keep-local");
}

#[test]
fn test_sparse_patch_rejects_mismatched_deletion_key_before_target_mutation() {
  let context = RequestContext::system();
  let (source, source_temp) = create_temp_engine_for_tests();
  let (target, _target_temp) = create_temp_engine_for_tests();
  let base = source.head_hash().unwrap();
  DirectoryOps::new(&source).store_file_buffered(&context, "/ordinary.txt", b"ordinary", Some("text/plain")).unwrap();
  let patch_path = db_path(&source_temp, "bad-deletion-key.aeordb");
  create_patch(&source, &base, &source.head_hash().unwrap(), &patch_path).unwrap();
  let patch = StorageEngine::open_for_import(&patch_path).unwrap();
  let deletion = DeletionRecord::new("/victim.txt".to_string(), Some("malicious".to_string()));
  patch.store_entry(EntryType::DeletionRecord, &[0xA5; 32], &deletion.serialize()).unwrap();
  drop(patch);
  let head_before = target.head_hash().unwrap();
  let entries_before = target.kv_entry_count().unwrap();

  let error = import_backup(&context, &target, &patch_path, false, true, true).unwrap_err();

  assert!(error.to_string().contains("DeletionRecord key does not match"), "unexpected error: {error}");
  assert_eq!(target.head_hash().unwrap(), head_before);
  assert_eq!(target.kv_entry_count().unwrap(), entries_before);
}

#[test]
fn test_sparse_patch_rejects_retained_and_deleted_path_before_target_mutation() {
  let context = RequestContext::system();
  let (source, source_temp) = create_temp_engine_for_tests();
  let (target, _target_temp) = create_temp_engine_for_tests();
  let base = source.head_hash().unwrap();
  let path = "/ordinary.txt";
  DirectoryOps::new(&source).store_file_buffered(&context, path, b"ordinary", Some("text/plain")).unwrap();
  let patch_path = db_path(&source_temp, "contradictory-deletion.aeordb");
  create_patch(&source, &base, &source.head_hash().unwrap(), &patch_path).unwrap();
  let patch = StorageEngine::open_for_import(&patch_path).unwrap();
  let deletion = DeletionRecord::new(path.to_string(), Some("malicious".to_string()));
  patch.store_entry(EntryType::DeletionRecord, &file_path_hash(path, &patch.hash_algo()).unwrap(), &deletion.serialize()).unwrap();
  drop(patch);
  let head_before = target.head_hash().unwrap();
  let entries_before = target.kv_entry_count().unwrap();

  let error = import_backup(&context, &target, &patch_path, false, true, true).unwrap_err();

  assert!(error.to_string().contains("both retains and deletes"), "unexpected error: {error}");
  assert_eq!(target.head_hash().unwrap(), head_before);
  assert_eq!(target.kv_entry_count().unwrap(), entries_before);
}

#[test]
fn test_sparse_patch_restore_mode_rejects_nonempty_target_before_mutation() {
  let context = RequestContext::system();
  let (source, source_temp) = create_temp_engine_for_tests();
  let (target, _target_temp) = create_temp_engine_for_tests();
  let base = source.head_hash().unwrap();
  DirectoryOps::new(&source).store_file_buffered(&context, "/incoming.txt", b"incoming", Some("text/plain")).unwrap();
  let patch_path = db_path(&source_temp, "restore-mode.aeordb");
  create_patch(&source, &base, &source.head_hash().unwrap(), &patch_path).unwrap();
  DirectoryOps::new(&target).store_file_buffered(&context, "/existing.txt", b"existing", Some("text/plain")).unwrap();
  let head_before = target.head_hash().unwrap();
  let entries_before = target.kv_entry_count().unwrap();

  let error = import_backup_with_mode(&context, &target, &patch_path, false, true, false, ImportMode::Restore).unwrap_err();

  assert!(error.to_string().contains("target database is not empty"), "unexpected error: {error}");
  assert_eq!(target.head_hash().unwrap(), head_before);
  assert_eq!(target.kv_entry_count().unwrap(), entries_before);
}

#[test]
fn restore_mode_target_emptiness_uses_registry_data_export_policy() {
  let context = RequestContext::system();
  let (source, source_temp) = create_temp_engine_for_tests();
  DirectoryOps::new(&source).store_file_buffered(&context, "/incoming.txt", b"incoming", Some("text/plain")).unwrap();
  let export_path = db_path(&source_temp, "registry-empty-target.aeordb");
  export_full(&source, &export_path, false).unwrap();

  let (concealed_target, _concealed_temp) = create_temp_engine_for_tests();
  DirectoryOps::new(&concealed_target)
    .store_file_buffered(&context, "/.aeordb-conflicts/item.json", b"conflict evidence", Some("application/json"))
    .unwrap();
  import_backup_with_mode(&context, &concealed_target, &export_path, false, true, false, ImportMode::Restore).unwrap();
  assert_eq!(DirectoryOps::new(&concealed_target).read_file_buffered("/incoming.txt").unwrap(), b"incoming");

  let (portable_target, _portable_temp) = create_temp_engine_for_tests();
  DirectoryOps::new(&portable_target)
    .store_file_buffered(&context, "/.aeordb-permissions", br#"{"links":[]}"#, Some("application/json"))
    .unwrap();
  let head_before = portable_target.head_hash().unwrap();
  let error = import_backup_with_mode(&context, &portable_target, &export_path, false, true, false, ImportMode::Restore).unwrap_err();
  assert!(error.to_string().contains("target database is not empty"), "unexpected error: {error}");
  assert_eq!(portable_target.head_hash().unwrap(), head_before);
}
