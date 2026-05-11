use std::collections::HashMap;
use std::sync::Arc;

use aeordb::engine::backup::{export_version, export_snapshot, ExportResult};
use aeordb::engine::tree_walker::walk_version_tree;
use aeordb::engine::version_manager::VersionManager;
use aeordb::engine::{DirectoryOps, StorageEngine};
use aeordb::engine::RequestContext;
use aeordb::server::create_temp_engine_for_tests;

// ─── Helpers ────────────────────────────────────────────────────────────

fn setup_engine_with_files() -> (Arc<StorageEngine>, tempfile::TempDir) {
  let ctx = RequestContext::system();
    let (engine, temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);

    ops.store_file(&ctx, "/docs/hello.txt", b"Hello World", Some("text/plain")).unwrap();
    ops.store_file(&ctx, "/docs/goodbye.txt", b"Goodbye World", Some("text/plain")).unwrap();
    ops.store_file(&ctx, "/images/photo.jpg", b"fake jpg data", Some("image/jpeg")).unwrap();

    (engine, temp)
}

fn output_path(temp: &tempfile::TempDir) -> String {
    temp.path().join("export.aeordb").to_str().unwrap().to_string()
}

// ─── 1. test_export_head ────────────────────────────────────────────────

#[test]
fn test_export_head() {
    let (source, _source_temp) = setup_engine_with_files();
    let output_temp = tempfile::tempdir().unwrap();
    let out = output_path(&output_temp);

    let head = source.head_hash().unwrap();
    let result = export_version(&source, &head, &out, false).unwrap();

    assert_eq!(result.files_written, 3);
    assert!(result.chunks_written >= 3, "expected at least 3 chunks, got {}", result.chunks_written);
    assert!(result.directories_written >= 3, "expected at least 3 dirs (/, /docs, /images), got {}", result.directories_written);

    // Verify exported file can be opened and has the files
    let exported = StorageEngine::open(&out).unwrap();
    let ops = DirectoryOps::new(&exported);
    let content = ops.read_file("/docs/hello.txt").unwrap();
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
    assert_eq!(export_ops.read_file("/docs/hello.txt").unwrap(), b"Hello World");
    assert_eq!(export_ops.read_file("/images/photo.jpg").unwrap(), b"fake jpg data");

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
    let content = ops.read_file("/images/photo.jpg").unwrap();
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
    ops.store_file(&ctx, "/temp/file1.txt", b"temporary", Some("text/plain")).unwrap();
    ops.delete_file(&ctx, "/temp/file1.txt").unwrap();

    let output_temp = tempfile::tempdir().unwrap();
    let out = output_path(&output_temp);

    let head = source.head_hash().unwrap();
    export_version(&source, &head, &out, false).unwrap();

    // The exported database should have zero voids
    let exported = StorageEngine::open(&out).unwrap();
    let stats = exported.stats();
    assert_eq!(stats.void_count, 0, "exported database should have no voids");
}

// ─── 7. test_export_no_deletion_records ─────────────────────────────────

#[test]
fn test_export_no_deletion_records() {
  let ctx = RequestContext::system();
    let (source, _source_temp) = setup_engine_with_files();

    // Create and delete a file to produce deletion records
    let ops = DirectoryOps::new(&source);
    ops.store_file(&ctx, "/temp/doomed.txt", b"going away", Some("text/plain")).unwrap();
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
    assert!(
        !tree.files.contains_key("/temp/doomed.txt"),
        "deleted file should not appear in export"
    );
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

    assert_eq!(ops.read_file("/docs/hello.txt").unwrap(), b"Hello World");
    assert_eq!(ops.read_file("/docs/goodbye.txt").unwrap(), b"Goodbye World");
    assert_eq!(ops.read_file("/images/photo.jpg").unwrap(), b"fake jpg data");
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
    assert!(
        err_msg.contains("not found") || err_msg.contains("Not found"),
        "error should mention not found, got: {}",
        err_msg
    );

    // Output file should not exist (create failed before writing)
    assert!(
        !std::path::Path::new(&out).exists(),
        "output file should not exist after failed export"
    );
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
    ops.store_file(&ctx, "/a/b/c/d/deep.txt", b"deep content", Some("text/plain")).unwrap();
    ops.store_file(&ctx, "/a/b/shallow.txt", b"shallow content", Some("text/plain")).unwrap();
    ops.store_file(&ctx, "/a/b/c/mid.txt", b"mid content", Some("text/plain")).unwrap();

    let output_temp = tempfile::tempdir().unwrap();
    let out = output_path(&output_temp);

    let head = source.head_hash().unwrap();
    let result = export_version(&source, &head, &out, false).unwrap();

    assert_eq!(result.files_written, 3);

    let exported = StorageEngine::open(&out).unwrap();
    let export_ops = DirectoryOps::new(&exported);

    assert_eq!(export_ops.read_file("/a/b/c/d/deep.txt").unwrap(), b"deep content");
    assert_eq!(export_ops.read_file("/a/b/shallow.txt").unwrap(), b"shallow content");
    assert_eq!(export_ops.read_file("/a/b/c/mid.txt").unwrap(), b"mid content");

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
    ops.store_file(&ctx, "/big/large.bin", &large_data, Some("application/octet-stream")).unwrap();

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
    let read_back = export_ops.read_file("/big/large.bin").unwrap();
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
    ops.store_file(&ctx, "/docs/file.txt", b"version 1", Some("text/plain")).unwrap();
    ops.store_file(&ctx, "/docs/file.txt", b"version 2", Some("text/plain")).unwrap();

    let output_temp = tempfile::tempdir().unwrap();
    let out = output_path(&output_temp);

    let head = source.head_hash().unwrap();
    let result = export_version(&source, &head, &out, false).unwrap();

    // Should only have 1 file (the latest version)
    assert_eq!(result.files_written, 1);

    let exported = StorageEngine::open(&out).unwrap();
    let export_ops = DirectoryOps::new(&exported);
    let content = export_ops.read_file("/docs/file.txt").unwrap();
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
