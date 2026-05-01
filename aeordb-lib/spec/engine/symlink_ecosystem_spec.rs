use std::collections::HashMap;
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::directory_listing::list_directory_recursive;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::version_manager::VersionManager;
use aeordb::engine::tree_walker::{walk_version_tree, diff_trees};
use aeordb::engine::gc::run_gc;
use aeordb::engine::entry_type::EntryType;
use aeordb::engine::RequestContext;

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
    let ctx = RequestContext::system();
    let path = dir.path().join("test.aeor");
    let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
    let ops = DirectoryOps::new(&engine);
    ops.ensure_root_directory(&ctx).unwrap();
    engine
}

fn store_file(engine: &StorageEngine, path: &str, content: &[u8]) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_file(&ctx, path, content, None).unwrap();
}

fn store_symlink(engine: &StorageEngine, path: &str, target: &str) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_symlink(&ctx, path, target).unwrap();
}

// ============================================================================
// GC Tests
// ============================================================================

#[test]
fn test_gc_preserves_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);

    // Store a file and a symlink
    store_file(&engine, "/data.txt", b"hello world");
    store_symlink(&engine, "/link", "/data.txt");

    // Verify symlink exists before GC
    let before = ops.get_symlink("/link").unwrap();
    assert!(before.is_some(), "symlink should exist before GC");
    assert_eq!(before.unwrap().target, "/data.txt");

    // Run GC (not dry run)
    let _result = run_gc(&engine, &ctx, false).unwrap();

    // Verify symlink still readable after GC
    let after = ops.get_symlink("/link").unwrap();
    assert!(after.is_some(), "symlink should survive GC");
    assert_eq!(after.unwrap().target, "/data.txt");

    // Verify file also survives
    let file_data = ops.read_file("/data.txt").unwrap();
    assert_eq!(file_data, b"hello world", "file should survive GC");
}

#[test]
fn test_gc_collects_orphaned_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);

    // Store and then delete a symlink to create an orphan
    store_symlink(&engine, "/link", "/target.txt");
    ops.delete_symlink(&ctx, "/link").unwrap();

    // The path-hash entry is now a deletion record, but the content-hash
    // entry for the old symlink data is orphaned. GC should collect it.
    let result = run_gc(&engine, &ctx, false).unwrap();
    // At minimum, the old content-hash symlink entry should be garbage
    assert!(result.garbage_entries > 0, "deleted symlink content should be collected");
}

// ============================================================================
// Tree Walker Tests
// ============================================================================

#[test]
fn test_tree_walker_includes_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ctx = RequestContext::system();
    let vm = VersionManager::new(&engine);

    store_symlink(&engine, "/link", "/target.txt");

    let snapshot = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();
    let tree = walk_version_tree(&engine, &snapshot.root_hash).unwrap();

    assert!(tree.symlinks.contains_key("/link"), "tree should contain the symlink");
    let (_hash, record) = &tree.symlinks["/link"];
    assert_eq!(record.target, "/target.txt");
}

#[test]
fn test_tree_walker_includes_multiple_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ctx = RequestContext::system();
    let vm = VersionManager::new(&engine);

    store_file(&engine, "/real.txt", b"content");
    store_symlink(&engine, "/link1", "/real.txt");
    store_symlink(&engine, "/link2", "/real.txt");

    let snapshot = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();
    let tree = walk_version_tree(&engine, &snapshot.root_hash).unwrap();

    assert_eq!(tree.symlinks.len(), 2);
    assert!(tree.symlinks.contains_key("/link1"));
    assert!(tree.symlinks.contains_key("/link2"));
    assert!(tree.files.contains_key("/real.txt"));
}

#[test]
fn test_tree_walker_symlink_in_subdirectory() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ctx = RequestContext::system();
    let vm = VersionManager::new(&engine);

    store_file(&engine, "/a/target.txt", b"data");
    store_symlink(&engine, "/a/link", "/a/target.txt");

    let snapshot = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();
    let tree = walk_version_tree(&engine, &snapshot.root_hash).unwrap();

    assert!(tree.symlinks.contains_key("/a/link"), "symlink in subdirectory should be found");
    assert_eq!(tree.symlinks["/a/link"].1.target, "/a/target.txt");
}

// ============================================================================
// Tree Diff Tests
// ============================================================================

#[test]
fn test_tree_diff_symlink_added() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ctx = RequestContext::system();
    let vm = VersionManager::new(&engine);

    // Snapshot 1: no symlink
    store_file(&engine, "/file.txt", b"hello");
    let snap1 = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();

    // Add symlink
    store_symlink(&engine, "/link", "/file.txt");
    let snap2 = vm.create_snapshot(&ctx, "snap2", HashMap::new()).unwrap();

    let tree1 = walk_version_tree(&engine, &snap1.root_hash).unwrap();
    let tree2 = walk_version_tree(&engine, &snap2.root_hash).unwrap();
    let diff = diff_trees(&tree1, &tree2);

    assert!(diff.symlinks_added.contains_key("/link"), "symlink should be in added");
    assert_eq!(diff.symlinks_added["/link"].1.target, "/file.txt");
    assert!(diff.symlinks_modified.is_empty());
    assert!(diff.symlinks_deleted.is_empty());
}

#[test]
fn test_tree_diff_symlink_modified() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ctx = RequestContext::system();
    let vm = VersionManager::new(&engine);

    // Snapshot 1: symlink -> /old
    store_symlink(&engine, "/link", "/old");
    let snap1 = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();

    // Change target
    store_symlink(&engine, "/link", "/new");
    let snap2 = vm.create_snapshot(&ctx, "snap2", HashMap::new()).unwrap();

    let tree1 = walk_version_tree(&engine, &snap1.root_hash).unwrap();
    let tree2 = walk_version_tree(&engine, &snap2.root_hash).unwrap();
    let diff = diff_trees(&tree1, &tree2);

    assert!(diff.symlinks_modified.contains_key("/link"), "symlink should be in modified");
    assert_eq!(diff.symlinks_modified["/link"].1.target, "/new");
    assert!(diff.symlinks_added.is_empty());
    assert!(diff.symlinks_deleted.is_empty());
}

#[test]
fn test_tree_diff_symlink_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ctx = RequestContext::system();
    let vm = VersionManager::new(&engine);
    let ops = DirectoryOps::new(&engine);

    // Snapshot 1: has symlink
    store_symlink(&engine, "/link", "/target");
    let snap1 = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();

    // Delete symlink
    ops.delete_symlink(&ctx, "/link").unwrap();
    let snap2 = vm.create_snapshot(&ctx, "snap2", HashMap::new()).unwrap();

    let tree1 = walk_version_tree(&engine, &snap1.root_hash).unwrap();
    let tree2 = walk_version_tree(&engine, &snap2.root_hash).unwrap();
    let diff = diff_trees(&tree1, &tree2);

    assert!(diff.symlinks_deleted.contains(&"/link".to_string()), "symlink should be in deleted");
    assert!(diff.symlinks_added.is_empty());
    assert!(diff.symlinks_modified.is_empty());
}

#[test]
fn test_tree_diff_symlink_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ctx = RequestContext::system();
    let vm = VersionManager::new(&engine);

    // Both snapshots have the same symlink
    store_symlink(&engine, "/link", "/target");
    let snap1 = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();

    // Modify a file but leave symlink alone
    store_file(&engine, "/other.txt", b"data");
    let snap2 = vm.create_snapshot(&ctx, "snap2", HashMap::new()).unwrap();

    let tree1 = walk_version_tree(&engine, &snap1.root_hash).unwrap();
    let tree2 = walk_version_tree(&engine, &snap2.root_hash).unwrap();
    let diff = diff_trees(&tree1, &tree2);

    assert!(diff.symlinks_added.is_empty(), "unchanged symlink should not appear as added");
    assert!(diff.symlinks_modified.is_empty(), "unchanged symlink should not appear as modified");
    assert!(diff.symlinks_deleted.is_empty(), "unchanged symlink should not appear as deleted");
}

#[test]
fn test_tree_diff_is_empty_with_only_symlink_changes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ctx = RequestContext::system();
    let vm = VersionManager::new(&engine);

    let snap1 = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();
    store_symlink(&engine, "/link", "/target");
    let snap2 = vm.create_snapshot(&ctx, "snap2", HashMap::new()).unwrap();

    let tree1 = walk_version_tree(&engine, &snap1.root_hash).unwrap();
    let tree2 = walk_version_tree(&engine, &snap2.root_hash).unwrap();
    let diff = diff_trees(&tree1, &tree2);

    // Diff should NOT be empty because we added a symlink
    assert!(!diff.is_empty(), "diff with symlink addition should not be empty");
}

// ============================================================================
// Directory Listing Tests
// ============================================================================

#[test]
fn test_listing_includes_symlink_target() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_symlink(&engine, "/link.txt", "/target.txt");

    let entries = list_directory_recursive(&engine, "/", 0, None, None).unwrap();
    let symlink_entry = entries.iter().find(|e| e.name == "link.txt");
    assert!(symlink_entry.is_some(), "symlink should appear in listing");

    let entry = symlink_entry.unwrap();
    assert_eq!(entry.entry_type, EntryType::Symlink.to_u8());
    assert_eq!(entry.target, Some("/target.txt".to_string()));
}

#[test]
fn test_listing_recursive_includes_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_file(&engine, "/a/file.txt", b"data");
    store_symlink(&engine, "/a/link", "/a/file.txt");

    let entries = list_directory_recursive(&engine, "/", -1, None, None).unwrap();
    let symlink_entry = entries.iter().find(|e| e.name == "link");
    assert!(symlink_entry.is_some(), "symlink should appear in recursive listing");

    let entry = symlink_entry.unwrap();
    assert_eq!(entry.path, "/a/link");
    assert_eq!(entry.target, Some("/a/file.txt".to_string()));
}

#[test]
fn test_listing_glob_filters_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_symlink(&engine, "/link.txt", "/target1");
    store_symlink(&engine, "/link.psd", "/target2");

    let entries = list_directory_recursive(&engine, "/", 0, Some("*.txt"), None).unwrap();
    assert_eq!(entries.len(), 1, "glob should filter symlinks");
    assert_eq!(entries[0].name, "link.txt");
    assert_eq!(entries[0].target, Some("/target1".to_string()));
}

#[test]
fn test_listing_files_have_no_target() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_file(&engine, "/file.txt", b"hello");

    let entries = list_directory_recursive(&engine, "/", 0, None, None).unwrap();
    let file_entry = entries.iter().find(|e| e.name == "file.txt").unwrap();
    assert_eq!(file_entry.target, None, "file entries should have target=None");
}

#[test]
fn test_listing_mixed_files_and_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_file(&engine, "/file.txt", b"hello");
    store_symlink(&engine, "/link", "/file.txt");

    let entries = list_directory_recursive(&engine, "/", 0, None, None).unwrap();
    assert_eq!(entries.len(), 2);

    let file_entry = entries.iter().find(|e| e.name == "file.txt").unwrap();
    assert_eq!(file_entry.target, None);
    assert_eq!(file_entry.entry_type, EntryType::FileRecord.to_u8());

    let link_entry = entries.iter().find(|e| e.name == "link").unwrap();
    assert_eq!(link_entry.target, Some("/file.txt".to_string()));
    assert_eq!(link_entry.entry_type, EntryType::Symlink.to_u8());
}

// ============================================================================
// Version Access Tests
// ============================================================================

#[test]
fn test_version_access_rejects_symlink_as_file() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ctx = RequestContext::system();
    let vm = VersionManager::new(&engine);

    store_symlink(&engine, "/link", "/target");
    let snap = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();

    // Attempting to resolve a symlink as a file should fail with a descriptive error
    let result = aeordb::engine::version_access::resolve_file_at_version(
        &engine, &snap.root_hash, "/link",
    );
    assert!(result.is_err());
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("symlink"),
        "error should mention symlink, got: {}",
        err_msg
    );
}

#[test]
fn test_version_access_file_still_works() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ctx = RequestContext::system();
    let vm = VersionManager::new(&engine);

    store_file(&engine, "/file.txt", b"hello");
    let snap = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();

    // Normal file resolution should still work
    let result = aeordb::engine::version_access::resolve_file_at_version(
        &engine, &snap.root_hash, "/file.txt",
    );
    assert!(result.is_ok(), "file resolution should still work");
}

// ============================================================================
// Edge Cases
// ============================================================================

#[test]
fn test_gc_with_symlink_and_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ctx = RequestContext::system();
    let vm = VersionManager::new(&engine);
    let ops = DirectoryOps::new(&engine);

    // Create symlink and snapshot it
    store_symlink(&engine, "/link", "/target");
    let _snap = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();

    // Delete symlink from HEAD (but snapshot still references it)
    ops.delete_symlink(&ctx, "/link").unwrap();

    // GC should NOT collect the symlink because snapshot still references it
    let _result = run_gc(&engine, &ctx, false).unwrap();

    // The snapshot-referenced symlink content should survive
    let tree = walk_version_tree(&engine, &_snap.root_hash).unwrap();
    assert!(tree.symlinks.contains_key("/link"), "snapshot symlink should survive GC");
}

#[test]
fn test_tree_walker_empty_tree_has_no_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ctx = RequestContext::system();
    let vm = VersionManager::new(&engine);

    let snap = vm.create_snapshot(&ctx, "empty", HashMap::new()).unwrap();
    let tree = walk_version_tree(&engine, &snap.root_hash).unwrap();

    assert!(tree.symlinks.is_empty(), "empty tree should have no symlinks");
}
