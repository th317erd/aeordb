use std::sync::Arc;

use aeordb::engine::backup::{
    create_patch, export_version, import_backup, ExportResult, ImportResult,
};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::tree_walker::walk_version_tree;
use aeordb::engine::RequestContext;
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

    ops.store_file(&ctx, "/docs/hello.txt", b"Hello World", Some("text/plain"))
        .unwrap();
    ops.store_file(&ctx, "/docs/goodbye.txt", b"Goodbye World", Some("text/plain"))
        .unwrap();
    ops.store_file(&ctx, "/images/photo.jpg", b"fake jpg data", Some("image/jpeg"))
        .unwrap();

    (engine, temp)
}

fn export_to_path(engine: &StorageEngine, path: &str) -> ExportResult {
    let head = engine.head_hash().unwrap();
    export_version(engine, &head, path).unwrap()
}

// ─── 1. test_import_full_export ─────────────────────────────────────────

#[test]
fn test_import_full_export() {
    let (source, source_temp) = setup_engine_with_files();
    let export_path = db_path(&source_temp, "export.aeordb");
    export_to_path(&source, &export_path);

    // Create a fresh target database
    let (target, _target_temp) = create_temp_engine_for_tests();

    let result = import_backup(&target, &export_path, false, false).unwrap();

    assert_eq!(result.backup_type, 1);
    assert!(result.files_imported >= 3, "expected at least 3 files imported, got {}", result.files_imported);
    assert!(result.chunks_imported >= 3, "expected at least 3 chunks imported, got {}", result.chunks_imported);
    assert!(result.directories_imported >= 3, "expected at least 3 dirs imported, got {}", result.directories_imported);
    assert_eq!(result.deletions_applied, 0);
}

// ─── 2. test_import_preserves_content ───────────────────────────────────

#[test]
fn test_import_preserves_content() {
    let (source, source_temp) = setup_engine_with_files();
    let export_path = db_path(&source_temp, "export.aeordb");
    export_to_path(&source, &export_path);

    let (target, _target_temp) = create_temp_engine_for_tests();
    import_backup(&target, &export_path, false, true).unwrap();

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

    let result = import_backup(&target, &export_path, false, false).unwrap();

    assert!(!result.head_promoted, "HEAD should NOT be promoted");
    let current_head = target.head_hash().unwrap();
    assert_eq!(
        current_head, original_head,
        "HEAD should remain unchanged when promote=false"
    );
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

    let result = import_backup(&target, &export_path, false, true).unwrap();

    assert!(result.head_promoted, "HEAD should be promoted");
    let current_head = target.head_hash().unwrap();
    assert_eq!(
        current_head, export_result.version_hash,
        "HEAD should equal the imported version hash"
    );
}

// ─── 5. test_import_patch_matching_base ─────────────────────────────────

#[test]
fn test_import_patch_matching_base() {
    let (source, source_temp) = setup_engine_with_files();

    // Create a base hash (bogus), then patch from bogus -> HEAD
    let bogus = vec![0xDE; 32];
    let head = source.head_hash().unwrap();
    let patch_path = db_path(&source_temp, "patch.aeordb");
    create_patch(&source, &bogus, &head, &patch_path).unwrap();

    // Create a target whose HEAD matches the bogus base
    let (target, _target_temp) = create_temp_engine_for_tests();
    target.update_head(&bogus).unwrap();

    let result = import_backup(&target, &patch_path, false, true).unwrap();
    assert_eq!(result.backup_type, 2);
    assert!(result.head_promoted);
    assert_eq!(target.head_hash().unwrap(), head);
}

// ─── 6. test_import_patch_wrong_base ────────────────────────────────────

#[test]
fn test_import_patch_wrong_base() {
    let (source, source_temp) = setup_engine_with_files();

    let bogus = vec![0xDE; 32];
    let head = source.head_hash().unwrap();
    let patch_path = db_path(&source_temp, "patch.aeordb");
    create_patch(&source, &bogus, &head, &patch_path).unwrap();

    // Target HEAD is different from the patch base
    let (target, _target_temp) = create_temp_engine_for_tests();
    let different_hash = vec![0xFF; 32];
    target.update_head(&different_hash).unwrap();

    let result = import_backup(&target, &patch_path, false, false);
    assert!(result.is_err(), "should fail when target HEAD doesn't match patch base");

    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("does not match"),
        "error should mention mismatch, got: {}",
        err_msg
    );
}

// ─── 7. test_import_patch_wrong_base_force ──────────────────────────────

#[test]
fn test_import_patch_wrong_base_force() {
    let (source, source_temp) = setup_engine_with_files();

    let bogus = vec![0xDE; 32];
    let head = source.head_hash().unwrap();
    let patch_path = db_path(&source_temp, "patch.aeordb");
    create_patch(&source, &bogus, &head, &patch_path).unwrap();

    // Target HEAD is different, but we use force=true
    let (target, _target_temp) = create_temp_engine_for_tests();
    let different_hash = vec![0xFF; 32];
    target.update_head(&different_hash).unwrap();

    let result = import_backup(&target, &patch_path, true, false);
    assert!(result.is_ok(), "should succeed with force=true, got: {:?}", result.err());
}

// ─── 8. test_import_patch_applies_deletions ─────────────────────────────

#[test]
fn test_import_patch_applies_deletions() {
  let ctx = RequestContext::system();
    // Create two engines to simulate diff with deletions
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    let ops_a = DirectoryOps::new(&engine_a);
    ops_a.store_file(&ctx, "/keep.txt", b"keep", Some("text/plain")).unwrap();
    ops_a.store_file(&ctx, "/remove.txt", b"remove me", Some("text/plain")).unwrap();
    let tree_a = walk_version_tree(&engine_a, &engine_a.head_hash().unwrap()).unwrap();

    let (engine_b, _temp_b) = create_temp_engine_for_tests();
    let ops_b = DirectoryOps::new(&engine_b);
    ops_b.store_file(&ctx, "/keep.txt", b"keep", Some("text/plain")).unwrap();
    let tree_b = walk_version_tree(&engine_b, &engine_b.head_hash().unwrap()).unwrap();

    let diff = aeordb::engine::tree_walker::diff_trees(&tree_a, &tree_b);

    // Verify our setup: /remove.txt should be in deleted
    assert!(
        diff.deleted.contains(&"/remove.txt".to_string()),
        "expected /remove.txt in deleted set"
    );
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
    let result = import_backup(&source, &export_path, false, false).unwrap();

    assert_eq!(
        result.chunks_imported, 0,
        "no chunks should be imported since they already exist"
    );
}

// ─── 11. test_round_trip_export_import ──────────────────────────────────

#[test]
fn test_round_trip_export_import() {
    let (source, source_temp) = setup_engine_with_files();
    let export_path = db_path(&source_temp, "export.aeordb");
    export_to_path(&source, &export_path);

    // Import into fresh target with promote
    let (target, _target_temp) = create_temp_engine_for_tests();
    let import_result = import_backup(&target, &export_path, false, true).unwrap();

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

    let result = import_backup(&target, "/nonexistent/path/backup.aeordb", false, false);
    assert!(result.is_err(), "should fail for nonexistent backup file");
}

// ─── 13. test_import_full_export_type_1 ─────────────────────────────────

#[test]
fn test_import_full_export_type_1() {
    let (source, source_temp) = setup_engine_with_files();
    let export_path = db_path(&source_temp, "export.aeordb");
    export_to_path(&source, &export_path);

    let (target, _target_temp) = create_temp_engine_for_tests();
    let result = import_backup(&target, &export_path, false, false).unwrap();

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
    let result = import_backup(&target, &export_path, false, true).unwrap();

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
    let import_result = import_backup(&target, &export_path, false, false).unwrap();

    assert_eq!(
        import_result.version_hash, export_result.version_hash,
        "import result version_hash should match export version_hash"
    );
}

// ─── 16. test_import_patch_base_check_skipped_for_full_export ───────────

#[test]
fn test_import_full_export_skips_base_check() {
    let (source, source_temp) = setup_engine_with_files();
    let export_path = db_path(&source_temp, "export.aeordb");
    export_to_path(&source, &export_path);

    // Target has a totally different HEAD, but full exports don't check base
    let (target, _target_temp) = create_temp_engine_for_tests();
    target.update_head(&vec![0xFF; 32]).unwrap();

    let result = import_backup(&target, &export_path, false, false);
    assert!(
        result.is_ok(),
        "full export import should not check base version, got: {:?}",
        result.err()
    );
}

// ─── 17. test_import_entries_total_count ─────────────────────────────────

#[test]
fn test_import_entries_total_count() {
    let (source, source_temp) = setup_engine_with_files();
    let export_path = db_path(&source_temp, "export.aeordb");
    export_to_path(&source, &export_path);

    let (target, _target_temp) = create_temp_engine_for_tests();
    let result = import_backup(&target, &export_path, false, false).unwrap();

    // entries_imported should equal sum of chunks + files + dirs + deletions
    assert_eq!(
        result.entries_imported,
        result.chunks_imported + result.files_imported + result.directories_imported + result.deletions_applied,
        "entries_imported should be the sum of all sub-counts"
    );
}
