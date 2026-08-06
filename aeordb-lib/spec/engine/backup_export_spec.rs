use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::sync::Arc;

use aeordb::engine::backup::{
  create_patch, export_full, export_snapshot, export_version, export_version_with_cancellation, import_backup, ExportResult,
};
use aeordb::engine::directory_entry::{serialize_child_entries, ChildEntry};
use aeordb::engine::entry_header::FLAG_SYSTEM;
use aeordb::engine::errors::EngineError;
use aeordb::engine::memory_coordinator::{AdmissionClass, MemoryOwner};
use aeordb::engine::tree_walker::walk_version_tree;
use aeordb::engine::version_manager::VersionManager;
use aeordb::engine::{is_btree_format, BufferedFile, DirectoryOps, EntryType, StorageEngine, BTREE_CONVERSION_THRESHOLD};
use aeordb::engine::RequestContext;
use aeordb::server::create_temp_engine_for_tests;
use tokio_util::sync::CancellationToken;

// ─── Helpers ────────────────────────────────────────────────────────────

fn setup_engine_with_files() -> (Arc<StorageEngine>, tempfile::TempDir) {
  let ctx = RequestContext::system();
  let (engine, temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/docs/hello.txt", b"Hello World", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/docs/goodbye.txt", b"Goodbye World", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/images/photo.jpg", b"fake jpg data", Some("image/jpeg")).unwrap();

  (engine, temp)
}

fn output_path(temp: &tempfile::TempDir) -> String {
  temp.path().join("export.aeordb").to_str().unwrap().to_string()
}

fn assert_no_partial_artifacts(output: &str) {
  let output = std::path::Path::new(output);
  let parent = output.parent().unwrap();
  let prefix = format!("{}.part-", output.file_name().unwrap().to_string_lossy());
  let artifacts = std::fs::read_dir(parent)
    .unwrap()
    .filter_map(Result::ok)
    .map(|entry| entry.file_name())
    .filter(|name| name.to_string_lossy().starts_with(&prefix))
    .collect::<Vec<_>>();
  assert!(artifacts.is_empty(), "partial export artifacts remain: {artifacts:?}");
}

// ─── 1. test_export_head ────────────────────────────────────────────────

#[test]
fn test_export_head() {
  let (source, _source_temp) = setup_engine_with_files();
  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  let head = source.head_hash().unwrap();
  let result = export_version(&source, &head, &out, false).unwrap();

  assert_no_partial_artifacts(&out);

  assert_eq!(result.files_written, 3);
  assert!(result.chunks_written >= 3, "expected at least 3 chunks, got {}", result.chunks_written);
  assert!(result.directories_written >= 3, "expected at least 3 dirs (/, /docs, /images), got {}", result.directories_written);

  // Verify exported file can be opened and has the files
  let exported = StorageEngine::open(&out).unwrap();
  let ops = DirectoryOps::new(&exported);
  let content = ops.read_file_buffered("/docs/hello.txt").unwrap();
  assert_eq!(content, b"Hello World");
}

// ─── 2. test_export_snapshot ────────────────────────────────────────────

#[test]
fn test_export_snapshot() {
  let ctx = RequestContext::system();
  let (source, _source_temp) = setup_engine_with_files();
  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  // Create a snapshot
  let vm = VersionManager::new(&source);
  vm.create_snapshot(&ctx, "v1.0", HashMap::new()).unwrap();

  // Export the snapshot by name
  let result = export_snapshot(&source, Some("v1.0"), &out, false).unwrap();

  // The snapshot's root_hash should be used for backup metadata
  let vm2 = VersionManager::new(&source);
  let snap_hash = vm2.get_snapshot_hash("v1.0").unwrap();
  assert_eq!(result.version_hash, snap_hash, "should export the snapshot's root hash");

  // Verify exported file is openable and has the original files
  let exported = StorageEngine::open(&out).unwrap();
  let export_ops = DirectoryOps::new(&exported);
  assert_eq!(export_ops.read_file_buffered("/docs/hello.txt").unwrap(), b"Hello World");
  assert_eq!(export_ops.read_file_buffered("/images/photo.jpg").unwrap(), b"fake jpg data");

  // Verify backup hashes match the snapshot
  let (btype, base, target) = exported.backup_info().unwrap();
  assert_eq!(btype, 1);
  assert_eq!(base, snap_hash);
  assert_eq!(target, snap_hash);
}

// ─── 3. test_export_is_usable ───────────────────────────────────────────

#[test]
fn test_export_is_usable() {
  let (source, _source_temp) = setup_engine_with_files();
  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  let head = source.head_hash().unwrap();
  export_version(&source, &head, &out, false).unwrap();

  // Should be openable as a normal database (backup_type=1 is allowed)
  let exported = StorageEngine::open(&out).unwrap();

  // Can read files
  let ops = DirectoryOps::new(&exported);
  let content = ops.read_file_buffered("/images/photo.jpg").unwrap();
  assert_eq!(content, b"fake jpg data");

  // Can list directories
  let children = ops.list_directory("/").unwrap();
  assert!(!children.is_empty());
}

// ─── 4. test_export_has_correct_backup_type ─────────────────────────────

#[test]
fn test_export_has_correct_backup_type() {
  let (source, _source_temp) = setup_engine_with_files();
  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  let head = source.head_hash().unwrap();
  export_version(&source, &head, &out, false).unwrap();

  let exported = StorageEngine::open(&out).unwrap();
  let (backup_type, _base, _target) = exported.backup_info().unwrap();
  assert_eq!(backup_type, 1, "backup_type should be 1 (full export)");
}

// ─── 5. test_export_has_correct_hashes ──────────────────────────────────

#[test]
fn test_export_has_correct_hashes() {
  let (source, _source_temp) = setup_engine_with_files();
  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  let head = source.head_hash().unwrap();
  let result = export_version(&source, &head, &out, false).unwrap();

  let exported = StorageEngine::open(&out).unwrap();
  let (backup_type, base_hash, target_hash) = exported.backup_info().unwrap();

  assert_eq!(backup_type, 1);
  assert_eq!(base_hash, head, "base_hash should equal version_hash");
  assert_eq!(target_hash, head, "target_hash should equal version_hash");
  assert_eq!(result.version_hash, head);

  // HEAD in the exported file should also match
  let exported_head = exported.head_hash().unwrap();
  assert_eq!(exported_head, head, "exported HEAD should match version_hash");
}

// ─── 6. test_export_no_voids ────────────────────────────────────────────

#[test]
fn test_export_no_voids() {
  let ctx = RequestContext::system();
  let (source, _source_temp) = setup_engine_with_files();

  // Create some churn in the source to generate voids
  let ops = DirectoryOps::new(&source);
  ops.store_file_buffered(&ctx, "/temp/file1.txt", b"temporary", Some("text/plain")).unwrap();
  ops.delete_file(&ctx, "/temp/file1.txt").unwrap();

  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  let head = source.head_hash().unwrap();
  export_version(&source, &head, &out, false).unwrap();

  // The exported database should have zero voids
  let exported = StorageEngine::open(&out).unwrap();
  let stats = exported.stats().unwrap();
  assert_eq!(stats.void_count, 0, "exported database should have no voids");
}

// ─── 7. test_export_no_deletion_records ─────────────────────────────────

#[test]
fn test_export_no_deletion_records() {
  let ctx = RequestContext::system();
  let (source, _source_temp) = setup_engine_with_files();

  // Create and delete a file to produce deletion records
  let ops = DirectoryOps::new(&source);
  ops.store_file_buffered(&ctx, "/temp/doomed.txt", b"going away", Some("text/plain")).unwrap();
  ops.delete_file(&ctx, "/temp/doomed.txt").unwrap();

  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  let head = source.head_hash().unwrap();
  export_version(&source, &head, &out, false).unwrap();

  // Walk the exported tree -- no deletion records should appear
  let exported = StorageEngine::open(&out).unwrap();
  let exported_head = exported.head_hash().unwrap();
  let tree = walk_version_tree(&exported, &exported_head).unwrap();

  // The deleted file should not be in the tree
  assert!(!tree.files.contains_key("/temp/doomed.txt"), "deleted file should not appear in export");
}

// ─── 8. test_export_preserves_file_content ──────────────────────────────

#[test]
fn test_export_preserves_file_content() {
  let (source, _source_temp) = setup_engine_with_files();
  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  let head = source.head_hash().unwrap();
  export_version(&source, &head, &out, false).unwrap();

  let exported = StorageEngine::open(&out).unwrap();
  let ops = DirectoryOps::new(&exported);

  assert_eq!(ops.read_file_buffered("/docs/hello.txt").unwrap(), b"Hello World");
  assert_eq!(ops.read_file_buffered("/docs/goodbye.txt").unwrap(), b"Goodbye World");
  assert_eq!(ops.read_file_buffered("/images/photo.jpg").unwrap(), b"fake jpg data");
}

// ─── 9. test_export_nonexistent_snapshot ────────────────────────────────

#[test]
fn test_export_nonexistent_snapshot() {
  let (source, _source_temp) = setup_engine_with_files();
  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  let result = export_snapshot(&source, Some("nonexistent_snapshot"), &out, false);
  assert!(result.is_err(), "should fail for nonexistent snapshot");

  let err_msg = format!("{}", result.unwrap_err());
  assert!(err_msg.contains("not found") || err_msg.contains("Not found"), "error should mention not found, got: {}", err_msg);

  // Output file should not exist (create failed before writing)
  assert!(!std::path::Path::new(&out).exists(), "output file should not exist after failed export");
}

// ─── 10. test_export_empty_database ─────────────────────────────────────

#[test]
fn test_export_empty_database() {
  let (source, _source_temp) = create_temp_engine_for_tests();
  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  let head = source.head_hash().unwrap();
  let result = export_version(&source, &head, &out, false).unwrap();

  assert_eq!(result.files_written, 0);
  assert_eq!(result.chunks_written, 0);
  // At minimum the root directory should be exported
  assert!(result.directories_written >= 1, "should export at least root directory");

  // Should be openable
  let exported = StorageEngine::open(&out).unwrap();
  let ops = DirectoryOps::new(&exported);
  let children = ops.list_directory("/").unwrap();
  assert!(children.is_empty(), "empty database export should have empty root");
}

// ─── 11. test_export_nested_directories ─────────────────────────────────

#[test]
fn test_export_nested_directories() {
  let ctx = RequestContext::system();
  let (source, _source_temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&source);

  // Create deeply nested files
  ops.store_file_buffered(&ctx, "/a/b/c/d/deep.txt", b"deep content", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/a/b/shallow.txt", b"shallow content", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/a/b/c/mid.txt", b"mid content", Some("text/plain")).unwrap();

  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  let head = source.head_hash().unwrap();
  let result = export_version(&source, &head, &out, false).unwrap();

  assert_eq!(result.files_written, 3);

  let exported = StorageEngine::open(&out).unwrap();
  let export_ops = DirectoryOps::new(&exported);

  assert_eq!(export_ops.read_file_buffered("/a/b/c/d/deep.txt").unwrap(), b"deep content");
  assert_eq!(export_ops.read_file_buffered("/a/b/shallow.txt").unwrap(), b"shallow content");
  assert_eq!(export_ops.read_file_buffered("/a/b/c/mid.txt").unwrap(), b"mid content");

  // Verify intermediate directories exist
  let exported_head = exported.head_hash().unwrap();
  let tree = walk_version_tree(&exported, &exported_head).unwrap();
  assert!(tree.directories.contains_key("/"));
  assert!(tree.directories.contains_key("/a"));
  assert!(tree.directories.contains_key("/a/b"));
  assert!(tree.directories.contains_key("/a/b/c"));
  assert!(tree.directories.contains_key("/a/b/c/d"));
}

// ─── 12. test_export_result_display ─────────────────────────────────────

#[test]
fn test_export_result_display() {
  let result = ExportResult {
    chunks_written: 10,
    files_written: 5,
    directories_written: 3,
    version_hash: vec![0xAB, 0xCD, 0xEF],
    snapshots_written: 0,
  };

  let display = format!("{}", result);
  assert!(display.contains("Export complete."), "should contain header");
  assert!(display.contains("Files: 5"), "should show file count");
  assert!(display.contains("Chunks: 10"), "should show chunk count");
  assert!(display.contains("Directories: 3"), "should show directory count");
  assert!(display.contains("abcdef"), "should show hex-encoded version hash");
}

// ─── 13. test_export_head_via_export_snapshot_none ──────────────────────

#[test]
fn test_export_head_via_export_snapshot_none() {
  let (source, _source_temp) = setup_engine_with_files();
  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  // export_snapshot with None should export HEAD
  let result = export_snapshot(&source, None, &out, false).unwrap();

  let head = source.head_hash().unwrap();
  assert_eq!(result.version_hash, head, "should export HEAD when snapshot is None");
  assert_eq!(result.files_written, 3);
}

// ─── 14. test_export_output_already_exists ──────────────────────────────

#[test]
fn test_export_output_already_exists() {
  let (source, _source_temp) = setup_engine_with_files();
  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  // First export succeeds
  let head = source.head_hash().unwrap();
  export_version(&source, &head, &out, false).unwrap();

  // Second export to same path should fail (StorageEngine::create uses create_new)
  let result = export_version(&source, &head, &out, false);
  assert!(result.is_err(), "should fail when output already exists");
}

// ─── 15. test_export_large_file_multiple_chunks ─────────────────────────

#[test]
fn test_export_large_file_multiple_chunks() {
  let ctx = RequestContext::system();
  let (source, _source_temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&source);

  // Create a file larger than the default chunk size (256KB)
  let large_data = vec![0x42u8; 300_000];
  ops.store_file_buffered(&ctx, "/big/large.bin", &large_data, Some("application/octet-stream")).unwrap();

  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  let head = source.head_hash().unwrap();
  let result = export_version(&source, &head, &out, false).unwrap();

  assert_eq!(result.files_written, 1);
  // A 300KB file with 256KB chunks should produce 2 chunks
  assert!(result.chunks_written >= 2, "large file should have multiple chunks, got {}", result.chunks_written);

  // Verify content round-trips
  let exported = StorageEngine::open(&out).unwrap();
  let export_ops = DirectoryOps::new(&exported);
  let read_back = export_ops.read_file_buffered("/big/large.bin").unwrap();
  assert_eq!(read_back.len(), 300_000);
  assert_eq!(read_back, large_data);
}

// ─── 16. test_export_overwritten_file_only_latest ───────────────────────

#[test]
fn test_export_overwritten_file_only_latest() {
  let ctx = RequestContext::system();
  let (source, _source_temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&source);

  // Write, then overwrite the same file
  ops.store_file_buffered(&ctx, "/docs/file.txt", b"version 1", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/docs/file.txt", b"version 2", Some("text/plain")).unwrap();

  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  let head = source.head_hash().unwrap();
  let result = export_version(&source, &head, &out, false).unwrap();

  // Should only have 1 file (the latest version)
  assert_eq!(result.files_written, 1);

  let exported = StorageEngine::open(&out).unwrap();
  let export_ops = DirectoryOps::new(&exported);
  let content = export_ops.read_file_buffered("/docs/file.txt").unwrap();
  assert_eq!(content, b"version 2", "export should contain latest version");
}

// ─── 17. test_export_invalid_version_hash ───────────────────────────────

#[test]
fn test_export_invalid_version_hash() {
  let (source, _source_temp) = setup_engine_with_files();
  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  // Use a bogus hash that doesn't correspond to any version
  let bogus_hash = vec![0xFF; 32];
  let result = export_version(&source, &bogus_hash, &out, false);

  // The walk should succeed but find nothing (empty tree from missing root)
  // or it may succeed with 0 entries - either way it should not panic
  match result {
    Ok(r) => {
      // Empty tree is acceptable for a nonexistent root hash
      assert_eq!(r.files_written, 0);
    }
    Err(_) => {
      // Also acceptable if the engine errors out
    }
  }
}

#[test]
fn test_export_memory_pressure_fails_before_creating_output() {
  let (source, _source_temp) = setup_engine_with_files();
  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);
  let coordinator = source.memory_coordinator();
  let snapshot = coordinator.snapshot().unwrap();
  let policy = snapshot.policy.unwrap();
  let remaining = policy.ordinary_limit_bytes().saturating_sub(snapshot.accounted_bytes);
  let pressure_bytes = remaining.saturating_sub(2 * 1024).max(1);
  let _pressure = coordinator.reserve(MemoryOwner::Task, pressure_bytes, AdmissionClass::Workload).unwrap();

  let error = export_version(&source, &source.head_hash().unwrap(), &out, false).unwrap_err();

  assert!(matches!(error, EngineError::ResourceExhausted(_)), "unexpected error: {error}");
  assert!(!std::path::Path::new(&out).exists(), "failed admission must not create the destination");
  assert_no_partial_artifacts(&out);
  let owner = coordinator.snapshot().unwrap().owner(MemoryOwner::BackupRestore).unwrap().clone();
  assert_eq!(owner.reserved_bytes, 0);
  assert_eq!(owner.active_reservations, 0);
}

#[test]
fn test_full_export_rejects_malformed_existing_system_record() {
  let (source, _source_temp) = setup_engine_with_files();
  let key = aeordb::engine::directory_ops::file_path_hash("/.aeordb-system/email-config.json", &source.hash_algo()).unwrap();
  source.store_entry_with_flags(EntryType::FileRecord, &key, b"not-a-file-record", FLAG_SYSTEM).unwrap();
  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  let error = export_full(&source, &out, true).unwrap_err();

  assert!(error.to_string().contains("FileRecord"), "unexpected error: {error}");
  assert!(!std::path::Path::new(&out).exists());
  assert_no_partial_artifacts(&out);
}

#[test]
fn test_cancelled_export_creates_no_artifact_and_releases_memory() {
  let (source, _source_temp) = setup_engine_with_files();
  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);
  let cancellation = CancellationToken::new();
  cancellation.cancel();

  let error = export_version_with_cancellation(&source, &source.head_hash().unwrap(), &out, false, &cancellation).unwrap_err();

  assert!(matches!(error, EngineError::Cancelled(_)), "unexpected error: {error}");
  assert!(!std::path::Path::new(&out).exists());
  assert_no_partial_artifacts(&out);
  let owner = source.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::BackupRestore).unwrap().clone();
  assert_eq!(owner.reserved_bytes, 0);
  assert_eq!(owner.active_reservations, 0);
}

#[test]
fn test_export_rejects_directory_cycle_instead_of_omitting_branch() {
  let (source, _source_temp) = create_temp_engine_for_tests();
  let root_hash = source.head_hash().unwrap();
  let cycle = ChildEntry {
    entry_type: EntryType::DirectoryIndex.to_u8(),
    hash: root_hash.clone(),
    total_size: 0,
    created_at: 0,
    updated_at: 0,
    name: "cycle".to_string(),
    content_type: None,
    virtual_time: 0,
    node_id: 0,
  };
  let data = serialize_child_entries(&[cycle], source.hash_algo().hash_length()).unwrap();
  source.store_entry(EntryType::DirectoryIndex, &root_hash, &data).unwrap();
  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  let error = export_version(&source, &root_hash, &out, false).unwrap_err();

  assert!(error.to_string().contains("cycle"), "unexpected error: {error}");
  assert!(!std::path::Path::new(&out).exists());
  assert_no_partial_artifacts(&out);
}

#[test]
fn test_full_export_rejects_malformed_snapshot_record() {
  let (source, _source_temp) = setup_engine_with_files();
  let key = vec![0xA7; source.hash_algo().hash_length()];
  source.store_entry(EntryType::Snapshot, &key, b"not-a-snapshot").unwrap();
  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  let error = export_full(&source, &out, true).unwrap_err();

  assert!(matches!(error, EngineError::CorruptEntry { .. } | EngineError::UnexpectedEof), "unexpected error: {error}");
  assert!(!std::path::Path::new(&out).exists());
  assert_no_partial_artifacts(&out);
}

#[test]
fn test_export_rejects_corrupt_referenced_entry_body() {
  let (source, source_temp) = setup_engine_with_files();
  let tree = walk_version_tree(&source, &source.head_hash().unwrap()).unwrap();
  let key = tree.files.get("/docs/goodbye.txt").unwrap().1.chunk_hashes[0].clone();
  let entry = source.get_kv_entry(&key).unwrap().unwrap();
  let header = source.get_entry_header_including_deleted(&key).unwrap().unwrap();
  let value_offset = entry.offset + header.header_size() as u64 + u64::from(header.key_length);
  let mut file = OpenOptions::new().write(true).open(source_temp.path().join("test.aeordb")).unwrap();
  file.seek(SeekFrom::Start(value_offset)).unwrap();
  file.write_all(&[0xFF]).unwrap();
  file.sync_data().unwrap();
  drop(file);
  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  let error = export_version(&source, &source.head_hash().unwrap(), &out, false).unwrap_err();

  assert!(matches!(error, EngineError::CorruptEntry { .. }), "unexpected error: {error}");
  assert!(!std::path::Path::new(&out).exists());
  assert_no_partial_artifacts(&out);
}

#[test]
fn test_backup_artifacts_preserve_large_directory_btree_nodes() {
  let (source, _source_temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let files = (0..BTREE_CONVERSION_THRESHOLD - 1)
    .map(|index| BufferedFile {
      path: format!("/large/file-{index:04}.json"),
      data: format!("{{\"index\":{index}}}").into_bytes(),
      content_type: Some("application/json".to_string()),
    })
    .collect();
  DirectoryOps::new(&source).store_files_buffered_batch(&context, files).unwrap();
  let operations = DirectoryOps::new(&source);
  for index in BTREE_CONVERSION_THRESHOLD - 1..BTREE_CONVERSION_THRESHOLD + 8 {
    operations
      .store_file_buffered(
        &context,
        &format!("/large/file-{index:04}.json"),
        format!("{{\"index\":{index}}}").as_bytes(),
        Some("application/json"),
      )
      .unwrap();
  }
  let directory_key = aeordb::engine::directory_path_hash("/large", &source.hash_algo()).unwrap();
  let directory_link = source.get_entry(&directory_key).unwrap().unwrap().2;
  let directory_data = source.get_entry(&directory_link).unwrap().unwrap().2;
  assert!(is_btree_format(&directory_data), "source fixture must exercise B-tree export");
  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  export_version(&source, &source.head_hash().unwrap(), &out, false).unwrap();
  let exported = StorageEngine::open_for_import(&out).unwrap();
  let children = DirectoryOps::new(&exported).list_directory("/large/").unwrap();
  assert_eq!(children.len(), BTREE_CONVERSION_THRESHOLD + 8);
  drop(exported);

  let (restored, _restored_temp) = create_temp_engine_for_tests();
  import_backup(&context, &restored, &out, false, true, false).unwrap();
  assert_eq!(DirectoryOps::new(&restored).list_directory("/large/").unwrap().len(), BTREE_CONVERSION_THRESHOLD + 8);

  let patch_path = output_temp.path().join("large-directory.patch.aeordb").to_string_lossy().into_owned();
  let empty_hash = vec![0xA3; source.hash_algo().hash_length()];
  create_patch(&source, &empty_hash, &source.head_hash().unwrap(), &patch_path).unwrap();
  let patch = StorageEngine::open_for_import(&patch_path).unwrap();
  assert_eq!(DirectoryOps::new(&patch).list_directory("/large/").unwrap().len(), BTREE_CONVERSION_THRESHOLD + 8);
}

#[test]
fn test_export_accepts_distinct_empty_directories_with_shared_content_hash() {
  let (source, _source_temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let operations = DirectoryOps::new(&source);
  operations.create_directory(&context, "/first-empty").unwrap();
  operations.create_directory(&context, "/second-empty").unwrap();
  let output_temp = tempfile::tempdir().unwrap();
  let out = output_path(&output_temp);

  export_version(&source, &source.head_hash().unwrap(), &out, false).unwrap();
  let exported = StorageEngine::open_for_import(&out).unwrap();

  assert!(DirectoryOps::new(&exported).list_directory("/first-empty/").unwrap().is_empty());
  assert!(DirectoryOps::new(&exported).list_directory("/second-empty/").unwrap().is_empty());
}
