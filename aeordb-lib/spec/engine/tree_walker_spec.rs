use std::collections::HashMap;
use std::sync::Arc;
use aeordb::engine::{RequestContext, StorageEngine, DirectoryOps};
use aeordb::engine::tree_walker::{walk_version_tree, diff_trees};
use aeordb::server::create_temp_engine_for_tests;

// Helper to create a test engine with some files
fn setup_test_engine_with_files() -> (Arc<StorageEngine>, tempfile::TempDir) {
  let ctx = RequestContext::system();
  let (engine, temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/docs/hello.txt", b"Hello World", Some("text/plain")).unwrap();
  ops.store_file(&ctx, "/docs/goodbye.txt", b"Goodbye World", Some("text/plain")).unwrap();
  ops.store_file(&ctx, "/images/photo.jpg", b"fake jpg data", Some("image/jpeg")).unwrap();

  (engine, temp)
}

// ─── walk_version_tree ───────────────────────────────────────────────────

#[test]
fn test_walk_tree_finds_all_files() {
  let (engine, _temp) = setup_test_engine_with_files();
  let head = engine.head_hash().unwrap();
  let tree = walk_version_tree(&engine, &head).unwrap();

  assert!(tree.files.len() >= 3, "should have at least 3 files, got {}", tree.files.len());
  assert!(tree.files.contains_key("/docs/hello.txt"));
  assert!(tree.files.contains_key("/docs/goodbye.txt"));
  assert!(tree.files.contains_key("/images/photo.jpg"));
}

#[test]
fn test_walk_tree_finds_directories() {
  let (engine, _temp) = setup_test_engine_with_files();
  let head = engine.head_hash().unwrap();
  let tree = walk_version_tree(&engine, &head).unwrap();

  // Root directory must always be present
  assert!(tree.directories.contains_key("/"), "should have root directory");
  // Subdirectories should be present
  assert!(tree.directories.contains_key("/docs"), "should have /docs directory");
  assert!(tree.directories.contains_key("/images"), "should have /images directory");
}

#[test]
fn test_walk_tree_collects_chunks() {
  let (engine, _temp) = setup_test_engine_with_files();
  let head = engine.head_hash().unwrap();
  let tree = walk_version_tree(&engine, &head).unwrap();

  assert!(!tree.chunks.is_empty(), "should have collected chunk hashes");

  // Each file with non-empty content should contribute at least one chunk
  for (path, (_, record)) in &tree.files {
    if record.total_size > 0 {
      assert!(
        !record.chunk_hashes.is_empty(),
        "file {} with size {} should have chunks",
        path,
        record.total_size,
      );
      // Every chunk hash from a file record should be in tree.chunks
      for chunk in &record.chunk_hashes {
        assert!(
          tree.chunks.contains(chunk),
          "chunk from {} should be in tree.chunks",
          path,
        );
      }
    }
  }
}

#[test]
fn test_walk_empty_tree() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let head = engine.head_hash().unwrap();
  let tree = walk_version_tree(&engine, &head).unwrap();

  assert!(tree.files.is_empty(), "empty database should have no files");
  assert!(tree.chunks.is_empty(), "empty database should have no chunks");
  // Root directory should still be present
  assert!(tree.directories.contains_key("/"), "should have root directory");
}

#[test]
fn test_walk_nested_directories() {
  let ctx = RequestContext::system();
  let (engine, _temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/a/b/c/deep.txt", b"deep content", Some("text/plain")).unwrap();

  let head = engine.head_hash().unwrap();
  let tree = walk_version_tree(&engine, &head).unwrap();

  assert!(tree.files.contains_key("/a/b/c/deep.txt"), "should find deeply nested file");
  assert!(tree.directories.contains_key("/a"), "should have /a directory");
  assert!(tree.directories.contains_key("/a/b"), "should have /a/b directory");
  assert!(tree.directories.contains_key("/a/b/c"), "should have /a/b/c directory");
}

#[test]
fn test_walk_tree_with_nonexistent_root_hash() {
  let (engine, _temp) = create_temp_engine_for_tests();
  // A bogus hash that doesn't exist in the database
  let bogus_hash = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
  let tree = walk_version_tree(&engine, &bogus_hash).unwrap();

  // Should return an empty tree since root was not found
  assert!(tree.files.is_empty());
  assert!(tree.directories.is_empty());
  assert!(tree.chunks.is_empty());
}

#[test]
fn test_walk_tree_file_records_contain_correct_metadata() {
  let ctx = RequestContext::system();
  let (engine, _temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);

  let content = b"test content for metadata check";
  ops.store_file(&ctx, "/meta/test.txt", content, Some("text/plain")).unwrap();

  let head = engine.head_hash().unwrap();
  let tree = walk_version_tree(&engine, &head).unwrap();

  let (_, record) = tree.files.get("/meta/test.txt").expect("file should exist in tree");
  assert_eq!(record.total_size, content.len() as u64);
  assert_eq!(record.content_type.as_deref(), Some("text/plain"));
  assert_eq!(record.path, "/meta/test.txt");
}

// ─── diff_trees ──────────────────────────────────────────────────────────

// Note: AeorDB uses path-based (mutable) hashing for directories and files.
// Snapshots capture the root hash, but the underlying entries are overwritten
// in place. To test diffs, we use separate engines representing different states.

/// Helper: walk a fresh engine with specific files to get a VersionTree.
fn tree_from_files(files: &[(&str, &[u8])]) -> aeordb::engine::tree_walker::VersionTree {
  let ctx = RequestContext::system();
  let (engine, _temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);
  for (path, data) in files {
    ops.store_file(&ctx, path, data, Some("text/plain")).unwrap();
  }
  let head = engine.head_hash().unwrap();
  // _temp must stay alive until walk completes
  
  walk_version_tree(&engine, &head).unwrap()
}

#[test]
fn test_diff_added_files() {
  let ctx = RequestContext::system();
  // Base: empty, Target: two files
  let (engine_a, _temp_a) = create_temp_engine_for_tests();
  let head_a = engine_a.head_hash().unwrap();
  let tree_a = walk_version_tree(&engine_a, &head_a).unwrap();

  let (engine_b, _temp_b) = create_temp_engine_for_tests();
  let ops_b = DirectoryOps::new(&engine_b);
  ops_b.store_file(&ctx, "/data/file1.txt", b"content1", Some("text/plain")).unwrap();
  ops_b.store_file(&ctx, "/data/file2.txt", b"content2", Some("text/plain")).unwrap();
  let head_b = engine_b.head_hash().unwrap();
  let tree_b = walk_version_tree(&engine_b, &head_b).unwrap();

  let diff = diff_trees(&tree_a, &tree_b);

  assert_eq!(diff.added.len(), 2, "should have 2 added files");
  assert!(diff.added.contains_key("/data/file1.txt"));
  assert!(diff.added.contains_key("/data/file2.txt"));
  assert!(diff.modified.is_empty());
  assert!(diff.deleted.is_empty());
}

#[test]
fn test_diff_modified_files() {
  // Base: file with "original", Target: same file with "modified content"
  let tree_a = tree_from_files(&[("/data/file1.txt", b"original")]);
  let tree_b = tree_from_files(&[("/data/file1.txt", b"modified content")]);

  let diff = diff_trees(&tree_a, &tree_b);

  assert!(diff.modified.contains_key("/data/file1.txt"), "file1.txt should be modified");
  assert!(diff.added.is_empty());
  assert!(diff.deleted.is_empty());
}

#[test]
fn test_diff_deleted_files() {
  // Base: two files, Target: only one file
  let tree_a = tree_from_files(&[
    ("/data/file1.txt", b"content"),
    ("/data/file2.txt", b"content2"),
  ]);
  let tree_b = tree_from_files(&[("/data/file1.txt", b"content")]);

  let diff = diff_trees(&tree_a, &tree_b);

  assert!(
    diff.deleted.contains(&"/data/file2.txt".to_string()),
    "file2.txt should be deleted, got: {:?}",
    diff.deleted,
  );
}

#[test]
fn test_diff_new_chunks_only() {
  // Base: one file, Target: same file + a new file
  let tree_a = tree_from_files(&[("/data/file1.txt", b"shared content")]);
  let tree_b = tree_from_files(&[
    ("/data/file1.txt", b"shared content"),
    ("/data/file2.txt", b"new unique content"),
  ]);

  let diff = diff_trees(&tree_a, &tree_b);

  // new_chunks should include chunks from file2.txt only
  assert!(!diff.new_chunks.is_empty(), "should have new chunks from file2");

  // Verify file1's chunks are not in new_chunks (same content = same chunk hashes)
  let (_, file1_record) = tree_a.files.get("/data/file1.txt").unwrap();
  for chunk in &file1_record.chunk_hashes {
    assert!(
      !diff.new_chunks.contains(chunk),
      "file1 chunks should not be in new_chunks",
    );
  }
}

#[test]
fn test_diff_no_changes() {
  // Both engines have the same file with the same content
  let tree_a = tree_from_files(&[("/data/file1.txt", b"content")]);
  let tree_b = tree_from_files(&[("/data/file1.txt", b"content")]);

  let diff = diff_trees(&tree_a, &tree_b);

  assert!(diff.is_empty(), "no changes between identical trees");
}

#[test]
fn test_diff_changed_directories() {
  // Base: one file in /data, Target: two files in /data
  let tree_a = tree_from_files(&[("/data/file1.txt", b"content")]);
  let tree_b = tree_from_files(&[
    ("/data/file1.txt", b"content"),
    ("/data/file2.txt", b"content2"),
  ]);

  let diff = diff_trees(&tree_a, &tree_b);

  // The /data directory should have changed (different raw data due to extra child)
  assert!(
    diff.changed_directories.contains_key("/data"),
    "the /data directory should be in changed_directories, got: {:?}",
    diff.changed_directories.keys().collect::<Vec<_>>(),
  );
}

#[test]
fn test_diff_mixed_operations() {
  // Base: keep, modify, remove
  let tree_base = tree_from_files(&[
    ("/keep.txt", b"keep"),
    ("/modify.txt", b"original"),
    ("/remove.txt", b"going away"),
  ]);
  // Target: keep (same), modify (changed), remove (gone), added (new)
  let tree_target = tree_from_files(&[
    ("/keep.txt", b"keep"),
    ("/modify.txt", b"changed"),
    ("/added.txt", b"new file"),
  ]);

  let diff = diff_trees(&tree_base, &tree_target);

  assert!(diff.added.contains_key("/added.txt"), "added.txt should be added");
  assert!(diff.modified.contains_key("/modify.txt"), "modify.txt should be modified");
  assert!(diff.deleted.contains(&"/remove.txt".to_string()), "remove.txt should be deleted");
  // keep.txt should not appear in any diff category
  assert!(!diff.added.contains_key("/keep.txt"));
  assert!(!diff.modified.contains_key("/keep.txt"));
  assert!(!diff.deleted.contains(&"/keep.txt".to_string()));
}

#[test]
fn test_diff_empty_to_empty() {
  let (engine_a, _temp_a) = create_temp_engine_for_tests();
  let (engine_b, _temp_b) = create_temp_engine_for_tests();

  let tree_a = walk_version_tree(&engine_a, &engine_a.head_hash().unwrap()).unwrap();
  let tree_b = walk_version_tree(&engine_b, &engine_b.head_hash().unwrap()).unwrap();

  let diff = diff_trees(&tree_a, &tree_b);

  assert!(diff.is_empty());
  assert!(diff.new_chunks.is_empty());
}

#[test]
fn test_walk_tree_empty_file() {
  let ctx = RequestContext::system();
  let (engine, _temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);

  // Store an empty file (0 bytes)
  ops.store_file(&ctx, "/empty.txt", b"", Some("text/plain")).unwrap();

  let head = engine.head_hash().unwrap();
  let tree = walk_version_tree(&engine, &head).unwrap();

  assert!(tree.files.contains_key("/empty.txt"), "should find empty file");
  let (_, record) = tree.files.get("/empty.txt").unwrap();
  assert_eq!(record.total_size, 0);
  assert!(record.chunk_hashes.is_empty(), "empty file should have 0 chunks");
}

#[test]
fn test_walk_tree_multiple_files_same_directory() {
  let ctx = RequestContext::system();
  let (engine, _temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/dir/a.txt", b"aaa", Some("text/plain")).unwrap();
  ops.store_file(&ctx, "/dir/b.txt", b"bbb", Some("text/plain")).unwrap();
  ops.store_file(&ctx, "/dir/c.txt", b"ccc", Some("text/plain")).unwrap();

  let head = engine.head_hash().unwrap();
  let tree = walk_version_tree(&engine, &head).unwrap();

  assert!(tree.files.contains_key("/dir/a.txt"));
  assert!(tree.files.contains_key("/dir/b.txt"));
  assert!(tree.files.contains_key("/dir/c.txt"));
  assert_eq!(tree.files.len(), 3);
}

#[test]
fn test_version_tree_new_is_empty() {
  use aeordb::engine::tree_walker::VersionTree;

  let tree = VersionTree::new();
  assert!(tree.files.is_empty());
  assert!(tree.directories.is_empty());
  assert!(tree.chunks.is_empty());
}

#[test]
fn test_tree_diff_is_empty() {
  use aeordb::engine::tree_walker::TreeDiff;

  let diff = TreeDiff {
    added: HashMap::new(),
    modified: HashMap::new(),
    deleted: Vec::new(),
    new_chunks: std::collections::HashSet::new(),
    changed_directories: HashMap::new(),
    symlinks_added: HashMap::new(),
    symlinks_modified: HashMap::new(),
    symlinks_deleted: Vec::new(),
  };

  assert!(diff.is_empty());
}

#[test]
fn test_tree_diff_is_not_empty_with_added() {
  use aeordb::engine::tree_walker::TreeDiff;
  use aeordb::engine::FileRecord;

  let mut added = HashMap::new();
  added.insert(
    "/test.txt".to_string(),
    (vec![1, 2, 3], FileRecord::new("test.txt".to_string(), None, 0, vec![])),
  );

  let diff = TreeDiff {
    added,
    modified: HashMap::new(),
    deleted: Vec::new(),
    new_chunks: std::collections::HashSet::new(),
    changed_directories: HashMap::new(),
    symlinks_added: HashMap::new(),
    symlinks_modified: HashMap::new(),
    symlinks_deleted: Vec::new(),
  };

  assert!(!diff.is_empty());
}

#[test]
fn test_tree_diff_is_not_empty_with_deleted() {
  use aeordb::engine::tree_walker::TreeDiff;

  let diff = TreeDiff {
    added: HashMap::new(),
    modified: HashMap::new(),
    deleted: vec!["/gone.txt".to_string()],
    new_chunks: std::collections::HashSet::new(),
    changed_directories: HashMap::new(),
    symlinks_added: HashMap::new(),
    symlinks_modified: HashMap::new(),
    symlinks_deleted: Vec::new(),
  };

  assert!(!diff.is_empty());
}
