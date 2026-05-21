use std::collections::HashMap;
use std::sync::Arc;

use aeordb::engine::{
  DirectoryOps, StorageEngine,
  directory_content_hash,
};
use aeordb::engine::directory_entry::deserialize_child_entries;
use aeordb::engine::entry_type::EntryType;
use aeordb::engine::tree_walker::{walk_version_tree, diff_trees};
use aeordb::engine::version_manager::VersionManager;
use aeordb::engine::RequestContext;
use aeordb::server::create_temp_engine_for_tests;

fn setup() -> (Arc<StorageEngine>, tempfile::TempDir) {
  create_temp_engine_for_tests()
}

// ─── 1. HEAD changes on file store ────────────────────────────────────────

#[test]
fn test_head_changes_on_file_store() {
  let ctx = RequestContext::system();
  let (engine, _temp) = setup();
  let ops = DirectoryOps::new(&engine);

  let head_before = engine.head_hash().unwrap();

  ops.store_file_buffered(&ctx, "/test.txt", b"hello", None).unwrap();

  let head_after = engine.head_hash().unwrap();
  assert_ne!(head_before, head_after, "HEAD must change after storing a file");
}

// ─── 2. HEAD differs between states ───────────────────────────────────────

#[test]
fn test_head_differs_between_states() {
  let ctx = RequestContext::system();
  let (engine, _temp) = setup();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/a.txt", b"file A", None).unwrap();
  let head_a = engine.head_hash().unwrap();

  ops.store_file_buffered(&ctx, "/b.txt", b"file B", None).unwrap();
  let head_b = engine.head_hash().unwrap();

  assert_ne!(head_a, head_b, "HEAD must differ after storing different files");
}

// ─── 3. Snapshot preserves directory structure (new files not visible) ─────

#[test]
fn test_snapshot_preserves_state() {
  let ctx = RequestContext::system();
  let (engine, _temp) = setup();
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  // Store initial files
  ops.store_file_buffered(&ctx, "/original.txt", b"original content", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/keep.txt", b"keep this", Some("text/plain")).unwrap();

  // Snapshot the current state
  let snapshot = vm.create_snapshot(&ctx, "v1", HashMap::new()).unwrap();

  // Store more files after the snapshot
  ops.store_file_buffered(&ctx, "/added-later.txt", b"added later", Some("text/plain")).unwrap();

  // Walk the snapshot tree -- files added later must NOT appear
  let tree = walk_version_tree(&engine, &snapshot.root_hash).unwrap();

  assert!(tree.files.contains_key("/original.txt"), "snapshot must contain /original.txt");
  assert!(tree.files.contains_key("/keep.txt"), "snapshot must contain /keep.txt");
  assert!(
    !tree.files.contains_key("/added-later.txt"),
    "snapshot must NOT contain /added-later.txt which was added after the snapshot"
  );
}

// ─── 4. Two snapshots have different trees ────────────────────────────────

#[test]
fn test_two_snapshots_different_trees() {
  let ctx = RequestContext::system();
  let (engine, _temp) = setup();
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  ops.store_file_buffered(&ctx, "/first.txt", b"first", None).unwrap();
  let snap_a = vm.create_snapshot(&ctx, "snap-a", HashMap::new()).unwrap();

  ops.store_file_buffered(&ctx, "/second.txt", b"second", None).unwrap();
  let snap_b = vm.create_snapshot(&ctx, "snap-b", HashMap::new()).unwrap();

  assert_ne!(snap_a.root_hash, snap_b.root_hash, "snapshots must have different root hashes");

  let tree_a = walk_version_tree(&engine, &snap_a.root_hash).unwrap();
  let tree_b = walk_version_tree(&engine, &snap_b.root_hash).unwrap();

  assert_eq!(tree_a.files.len(), 1, "snap-a tree should have 1 file");
  assert_eq!(tree_b.files.len(), 2, "snap-b tree should have 2 files");

  assert!(tree_a.files.contains_key("/first.txt"));
  assert!(!tree_a.files.contains_key("/second.txt"));

  assert!(tree_b.files.contains_key("/first.txt"));
  assert!(tree_b.files.contains_key("/second.txt"));
}

// ─── 5. Content hash entry exists alongside path entry ────────────────────

#[test]
fn test_content_hash_entry_exists() {
  let ctx = RequestContext::system();
  let (engine, _temp) = setup();
  let ops = DirectoryOps::new(&engine);
  let algo = engine.hash_algo();

  ops.store_file_buffered(&ctx, "/foo.txt", b"foo data", None).unwrap();

  // The path-based key for root "/" should exist
  let path_key = algo.compute_hash(b"dir:/").unwrap();
  assert!(engine.has_entry(&path_key).unwrap(), "path-based root dir entry must exist");

  // The content-based key should also exist
  let head = engine.head_hash().unwrap();
  assert!(engine.has_entry(&head).unwrap(), "content-hashed root dir entry must exist (HEAD points to it)");

  // HEAD should NOT equal the path key (it should be the content hash)
  assert_ne!(head, path_key, "HEAD must be a content hash, not the path hash");
}

// ─── 6. ChildEntry uses content hash for directories ──────────────────────

#[test]
fn test_child_entry_uses_content_hash() {
  let ctx = RequestContext::system();
  let (engine, _temp) = setup();
  let ops = DirectoryOps::new(&engine);
  let algo = engine.hash_algo();
  let hash_length = algo.hash_length();

  // Create a subdirectory by storing a file inside it
  ops.store_file_buffered(&ctx, "/subdir/file.txt", b"in subdir", None).unwrap();

  // Read the root directory to get ChildEntry for "subdir". The path-keyed
  // entry may be a hard link (32-byte content hash) to the actual data;
  // follow the link if so.
  let root_path_key = algo.compute_hash(b"dir:/").unwrap();
  let (_h, _k, raw) = engine.get_entry(&root_path_key).unwrap().unwrap();
  let root_value = if raw.len() == hash_length {
    engine.get_entry(&raw).unwrap().unwrap().2
  } else {
    raw
  };
  let children = deserialize_child_entries(&root_value, hash_length, 0).unwrap();

  let subdir_child = children.iter().find(|c| c.name == "subdir").expect("must find subdir child");
  assert_eq!(subdir_child.entry_type, EntryType::DirectoryIndex.to_u8());

  // The hash should be a content hash, not the path hash
  let subdir_path_key = algo.compute_hash(b"dir:/subdir").unwrap();
  assert_ne!(
    subdir_child.hash, subdir_path_key,
    "ChildEntry.hash for directory must be content hash, not path hash"
  );

  // Verify the content hash entry exists and contains the same data
  let content_entry = engine.get_entry(&subdir_child.hash).unwrap();
  assert!(content_entry.is_some(), "content-hashed directory entry must be retrievable");
}

// ─── 7. Snapshot directory tree is immutable (delete removes from current, not snapshot) ──

#[test]
fn test_snapshot_directory_tree_immutable_after_delete() {
  let ctx = RequestContext::system();
  let (engine, _temp) = setup();
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  // Store files
  ops.store_file_buffered(&ctx, "/alpha.txt", b"alpha", None).unwrap();
  ops.store_file_buffered(&ctx, "/beta.txt", b"beta", None).unwrap();

  // Snapshot
  let snapshot = vm.create_snapshot(&ctx, "before-delete", HashMap::new()).unwrap();

  // Delete a file
  ops.delete_file(&ctx, "/alpha.txt").unwrap();

  // Current HEAD tree should NOT have alpha in its directory structure
  let current_head = engine.head_hash().unwrap();
  let current_tree = walk_version_tree(&engine, &current_head).unwrap();
  assert!(
    !current_tree.files.contains_key("/alpha.txt"),
    "deleted file must not appear in current tree"
  );
  assert!(current_tree.files.contains_key("/beta.txt"));

  // The snapshot's directory structure is immutable -- it still lists alpha
  // (The directory index at the snapshot's root hash still contains the ChildEntry)
  let snapshot_tree = walk_version_tree(&engine, &snapshot.root_hash).unwrap();
  // The snapshot directory still references alpha in its directory entries
  let root_dir = snapshot_tree.directories.get("/").unwrap();
  let hash_length = engine.hash_algo().hash_length();
  let children = deserialize_child_entries(&root_dir.1, hash_length, 0).unwrap();
  let has_alpha = children.iter().any(|c| c.name == "alpha.txt");
  assert!(has_alpha, "snapshot directory must still list alpha.txt as a child entry");
}

// ─── 8. Diff between snapshots (add only) ────────────────────────────────

#[test]
fn test_diff_between_snapshots_add_only() {
  let ctx = RequestContext::system();
  let (engine, _temp) = setup();
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  ops.store_file_buffered(&ctx, "/existing.txt", b"original", None).unwrap();
  let snap_a = vm.create_snapshot(&ctx, "a", HashMap::new()).unwrap();

  // Add a new file (don't overwrite -- file records are path-addressed and overwrite)
  ops.store_file_buffered(&ctx, "/new-file.txt", b"brand new", None).unwrap();
  let snap_b = vm.create_snapshot(&ctx, "b", HashMap::new()).unwrap();

  let tree_a = walk_version_tree(&engine, &snap_a.root_hash).unwrap();
  let tree_b = walk_version_tree(&engine, &snap_b.root_hash).unwrap();

  let diff = diff_trees(&tree_a, &tree_b);

  assert!(diff.added.contains_key("/new-file.txt"), "diff must show /new-file.txt as added");
  assert!(diff.deleted.is_empty(), "no files should be deleted");
}

// ─── 9. HEAD is a content hash, not a path hash ──────────────────────────

#[test]
fn test_head_is_content_hash_not_path_hash() {
  let ctx = RequestContext::system();
  let (engine, _temp) = setup();
  let ops = DirectoryOps::new(&engine);
  let algo = engine.hash_algo();

  ops.store_file_buffered(&ctx, "/test.txt", b"data", None).unwrap();

  let head = engine.head_hash().unwrap();
  let root_path_hash = algo.compute_hash(b"dir:/").unwrap();

  assert_ne!(head, root_path_hash, "HEAD must NOT be the root path hash");

  // HEAD should be resolvable as a directory entry
  let entry = engine.get_entry(&head).unwrap();
  assert!(entry.is_some(), "HEAD must point to a valid entry");
}

// ─── 10. Content hash is deterministic ────────────────────────────────────

#[test]
fn test_content_hash_is_deterministic() {
  let algo = aeordb::engine::HashAlgorithm::Blake3_256;
  let data = b"some directory data";

  let hash_1 = directory_content_hash(data, &algo).unwrap();
  let hash_2 = directory_content_hash(data, &algo).unwrap();

  assert_eq!(hash_1, hash_2, "content hash must be deterministic");
}

// ─── 11. Content hash differs from path hash ─────────────────────────────

#[test]
fn test_content_hash_differs_from_path_hash() {
  let algo = aeordb::engine::HashAlgorithm::Blake3_256;

  let path_hash = algo.compute_hash(b"dir:/").unwrap();
  let content_hash = directory_content_hash(&[], &algo).unwrap();

  assert_ne!(
    path_hash, content_hash,
    "content hash (dirc: prefix) must differ from path hash (dir: prefix)"
  );
}

// ─── 12. Empty root directory initial HEAD is content hash ────────────────

#[test]
fn test_empty_root_directory_head_is_content_hash() {
  let (engine, _temp) = setup();
  let algo = engine.hash_algo();

  // Before any files are stored, HEAD should be the content hash of empty dir
  let head = engine.head_hash().unwrap();
  let expected_content_hash = directory_content_hash(&[], &algo).unwrap();

  assert_eq!(head, expected_content_hash, "initial HEAD must be content hash of empty root dir");
}

// ─── 13. Multiple file stores produce unique HEADs ───────────────────────

#[test]
fn test_multiple_stores_produce_unique_heads() {
  let ctx = RequestContext::system();
  let (engine, _temp) = setup();
  let ops = DirectoryOps::new(&engine);

  let mut seen_heads = std::collections::HashSet::new();
  seen_heads.insert(engine.head_hash().unwrap());

  for i in 0..5 {
    ops.store_file_buffered(&ctx, &format!("/file-{}.txt", i), format!("content-{}", i).as_bytes(), None).unwrap();
    let head = engine.head_hash().unwrap();
    assert!(seen_heads.insert(head.clone()), "HEAD must be unique after each store (iteration {})", i);
  }
}

// ─── 14. Snapshot restore sets HEAD to historical content hash ────────────

#[test]
fn test_snapshot_restore_sets_head_to_historical_content_hash() {
  let ctx = RequestContext::system();
  let (engine, _temp) = setup();
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  ops.store_file_buffered(&ctx, "/initial.txt", b"initial", None).unwrap();
  let snapshot = vm.create_snapshot(&ctx, "checkpoint", HashMap::new()).unwrap();
  let snapshot_head = engine.head_hash().unwrap();

  // Add more files to move HEAD forward
  ops.store_file_buffered(&ctx, "/later.txt", b"later", None).unwrap();
  let moved_head = engine.head_hash().unwrap();
  assert_ne!(snapshot_head, moved_head);

  // Restore snapshot
  vm.restore_snapshot(&ctx, "checkpoint").unwrap();
  let restored_head = engine.head_hash().unwrap();

  assert_eq!(restored_head, snapshot.root_hash, "restored HEAD must match snapshot root hash");
  assert_eq!(restored_head, snapshot_head, "restored HEAD must match original HEAD at snapshot time");

  // Walk the restored HEAD -- should only see initial file
  let tree = walk_version_tree(&engine, &restored_head).unwrap();
  assert!(tree.files.contains_key("/initial.txt"));
  assert!(!tree.files.contains_key("/later.txt"));
}

// ─── 15. Deeply nested directory content hashes propagate ─────────────────

#[test]
fn test_deep_nesting_content_hash_propagation() {
  let ctx = RequestContext::system();
  let (engine, _temp) = setup();
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  ops.store_file_buffered(&ctx, "/a/b/c/deep.txt", b"deep content", None).unwrap();
  let snap = vm.create_snapshot(&ctx, "deep-snap", HashMap::new()).unwrap();

  ops.store_file_buffered(&ctx, "/a/b/c/another.txt", b"another", None).unwrap();

  // Walk the snapshot -- should only contain the one deep file
  let tree = walk_version_tree(&engine, &snap.root_hash).unwrap();
  assert!(tree.files.contains_key("/a/b/c/deep.txt"));
  assert!(
    !tree.files.contains_key("/a/b/c/another.txt"),
    "file added after snapshot must not appear in snapshot tree"
  );
}

// ─── 16. Snapshot after add reflects only post-add state ──────────────────

#[test]
fn test_snapshot_after_add_reflects_new_files() {
  let ctx = RequestContext::system();
  let (engine, _temp) = setup();
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  ops.store_file_buffered(&ctx, "/stays.txt", b"permanent", None).unwrap();
  let snap_before = vm.create_snapshot(&ctx, "before-add", HashMap::new()).unwrap();

  ops.store_file_buffered(&ctx, "/added.txt", b"new file", None).unwrap();
  let snap_after = vm.create_snapshot(&ctx, "after-add", HashMap::new()).unwrap();

  let tree_before = walk_version_tree(&engine, &snap_before.root_hash).unwrap();
  let tree_after = walk_version_tree(&engine, &snap_after.root_hash).unwrap();

  assert_eq!(tree_before.files.len(), 1, "before snapshot should have 1 file");
  assert_eq!(tree_after.files.len(), 2, "after snapshot should have 2 files");

  assert!(!tree_before.files.contains_key("/added.txt"), "before snapshot must NOT have /added.txt");
  assert!(tree_after.files.contains_key("/added.txt"), "after snapshot must have /added.txt");
}

// ─── 17. File store with different sizes changes HEAD ─────────────────────

#[test]
fn test_file_overwrite_different_size_changes_head() {
  let ctx = RequestContext::system();
  let (engine, _temp) = setup();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/mutable.txt", b"short", None).unwrap();
  let head_v1 = engine.head_hash().unwrap();

  // Sleep to ensure updated_at differs (millisecond resolution)
  std::thread::sleep(std::time::Duration::from_millis(2));

  ops.store_file_buffered(&ctx, "/mutable.txt", b"a much longer version of the content", None).unwrap();
  let head_v2 = engine.head_hash().unwrap();

  // ChildEntry total_size changed, so directory content hash must differ
  assert_ne!(head_v1, head_v2, "HEAD must change when a file is overwritten with different size");
}

// ─── 18. Both path and content entries have same data ─────────────────────

#[test]
fn test_path_and_content_entries_have_same_data() {
  let ctx = RequestContext::system();
  let (engine, _temp) = setup();
  let ops = DirectoryOps::new(&engine);
  let algo = engine.hash_algo();

  ops.store_file_buffered(&ctx, "/test.txt", b"test data", None).unwrap();

  // Get the path-based entry. May be a hard-link (content-hash bytes);
  // follow it.
  let hash_length = algo.hash_length();
  let path_key = algo.compute_hash(b"dir:/").unwrap();
  let (_h1, _k1, raw) = engine.get_entry(&path_key).unwrap().unwrap();
  let path_value = if raw.len() == hash_length {
    engine.get_entry(&raw).unwrap().unwrap().2
  } else {
    raw
  };

  // Get the content-based entry (HEAD)
  let head = engine.head_hash().unwrap();
  let (_h2, _k2, content_value) = engine.get_entry(&head).unwrap().unwrap();

  assert_eq!(path_value, content_value, "path-based and content-based entries must have identical data");
}

// ─── 19. list_directory still works via path key ──────────────────────────

#[test]
fn test_list_directory_still_works() {
  let ctx = RequestContext::system();
  let (engine, _temp) = setup();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/docs/readme.txt", b"readme", None).unwrap();
  ops.store_file_buffered(&ctx, "/docs/notes.txt", b"notes", None).unwrap();
  ops.store_file_buffered(&ctx, "/images/pic.png", b"png data", None).unwrap();

  let root_children = ops.list_directory("/").unwrap();
  let root_names: Vec<&str> = root_children.iter().map(|c| c.name.as_str()).collect();
  assert!(root_names.contains(&"docs"), "root must list 'docs' directory");
  assert!(root_names.contains(&"images"), "root must list 'images' directory");

  let docs_children = ops.list_directory("/docs").unwrap();
  let doc_names: Vec<&str> = docs_children.iter().map(|c| c.name.as_str()).collect();
  assert!(doc_names.contains(&"readme.txt"));
  assert!(doc_names.contains(&"notes.txt"));
}

// ─── 20. Create directory then snapshot ───────────────────────────────────

#[test]
fn test_create_directory_then_snapshot() {
  let ctx = RequestContext::system();
  let (engine, _temp) = setup();
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  ops.create_directory(&ctx, "/empty-dir").unwrap();
  ops.store_file_buffered(&ctx, "/file.txt", b"data", None).unwrap();

  let snapshot = vm.create_snapshot(&ctx, "with-empty-dir", HashMap::new()).unwrap();

  ops.store_file_buffered(&ctx, "/empty-dir/new-file.txt", b"new", None).unwrap();

  // Snapshot tree should have the empty directory but not the file added after
  let tree = walk_version_tree(&engine, &snapshot.root_hash).unwrap();
  assert!(tree.directories.contains_key("/empty-dir"), "snapshot must contain /empty-dir");
  assert!(tree.files.contains_key("/file.txt"));
  assert!(
    !tree.files.contains_key("/empty-dir/new-file.txt"),
    "file added after snapshot must not appear"
  );
}

// ─── 21. Content hash uses dirc: prefix (collision avoidance) ─────────────

#[test]
fn test_content_hash_uses_distinct_prefix() {
  let algo = aeordb::engine::HashAlgorithm::Blake3_256;

  // Ensure "dirc:" prefix is used (not "dir:") to avoid collisions
  // with path hashes that look like "dir:/some/path"
  let content_of_slash = b"/";  // data that starts with "/"
  let content_hash = directory_content_hash(content_of_slash, &algo).unwrap();
  let path_hash = algo.compute_hash(b"dir:/").unwrap();

  // "dirc:/" != "dir:/" so these must differ
  assert_ne!(content_hash, path_hash, "content hash with dirc: prefix must not collide with dir: path hash");
}

// ─── 22. Walk HEAD tree matches current state ─────────────────────────────

#[test]
fn test_walk_head_tree_matches_current_state() {
  let ctx = RequestContext::system();
  let (engine, _temp) = setup();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/a.txt", b"a", None).unwrap();
  ops.store_file_buffered(&ctx, "/b.txt", b"b", None).unwrap();
  ops.store_file_buffered(&ctx, "/sub/c.txt", b"c", None).unwrap();

  let head = engine.head_hash().unwrap();
  let tree = walk_version_tree(&engine, &head).unwrap();

  assert!(tree.files.contains_key("/a.txt"));
  assert!(tree.files.contains_key("/b.txt"));
  assert!(tree.files.contains_key("/sub/c.txt"));
  assert_eq!(tree.files.len(), 3);
}

// ─── 23. Snapshot root hash is retrievable via get_entry ──────────────────

#[test]
fn test_snapshot_root_hash_is_valid_entry() {
  let ctx = RequestContext::system();
  let (engine, _temp) = setup();
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  ops.store_file_buffered(&ctx, "/test.txt", b"data", None).unwrap();
  let snapshot = vm.create_snapshot(&ctx, "snap", HashMap::new()).unwrap();

  // The snapshot root hash should be directly retrievable
  let entry = engine.get_entry(&snapshot.root_hash).unwrap();
  assert!(entry.is_some(), "snapshot root hash must be a valid, retrievable entry");
}

// ─── 24. Multiple snapshots with same content have same root hash ─────────

#[test]
fn test_identical_content_produces_same_root_hash() {
  let ctx = RequestContext::system();
  let (engine, _temp) = setup();
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  // Store a file, take a snapshot
  ops.store_file_buffered(&ctx, "/test.txt", b"static content", None).unwrap();
  let snap_1 = vm.create_snapshot(&ctx, "first", HashMap::new()).unwrap();

  // Without changing anything, take another snapshot
  // (Note: HEAD hasn't changed, so root_hash should be the same)
  let snap_2 = vm.create_snapshot(&ctx, "second", HashMap::new()).unwrap();

  assert_eq!(
    snap_1.root_hash, snap_2.root_hash,
    "snapshots of identical state must have the same root hash"
  );
}

// ─── 25. Content hash entry is immutable across directory mutations ───────

#[test]
fn test_content_hash_entry_immutable() {
  let ctx = RequestContext::system();
  let (engine, _temp) = setup();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/first.txt", b"first", None).unwrap();

  // Capture the current HEAD (content hash of root)
  let head_1 = engine.head_hash().unwrap();
  let (_h, _k, data_1) = engine.get_entry(&head_1).unwrap().unwrap();

  // Mutate the directory by adding another file
  ops.store_file_buffered(&ctx, "/second.txt", b"second", None).unwrap();

  // The old content hash entry should still exist with the same data
  let entry = engine.get_entry(&head_1).unwrap();
  assert!(entry.is_some(), "old content hash entry must still exist after mutation");
  let (_h, _k, data_1_after) = entry.unwrap();
  assert_eq!(data_1, data_1_after, "old content hash entry data must not change");
}
