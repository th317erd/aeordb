use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::entry_type::EntryType;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::RequestContext;

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
  let ctx = RequestContext::system();
  let path = dir.path().join("test.aeor");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  let ops = DirectoryOps::new(&engine);
  ops.ensure_root_directory(&ctx).unwrap();
  engine
}

// --- Store and read ---

#[test]
fn test_store_and_read_symlink() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let record = ops.store_symlink(&ctx, "/link.txt", "/target.txt").unwrap();
  assert_eq!(record.path, "/link.txt");
  assert_eq!(record.target, "/target.txt");
  assert!(record.created_at > 0);
  assert!(record.updated_at > 0);
  assert_eq!(record.created_at, record.updated_at);

  // Read it back
  let fetched = ops.get_symlink("/link.txt").unwrap().unwrap();
  assert_eq!(fetched.path, "/link.txt");
  assert_eq!(fetched.target, "/target.txt");
  assert_eq!(fetched.created_at, record.created_at);
  assert_eq!(fetched.updated_at, record.updated_at);
}

// --- Update preserves created_at ---

#[test]
fn test_update_symlink_target() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let original = ops.store_symlink(&ctx, "/link", "/old-target").unwrap();
  let original_created_at = original.created_at;

  // Small delay to ensure updated_at differs
  std::thread::sleep(std::time::Duration::from_millis(5));

  let updated = ops.store_symlink(&ctx, "/link", "/new-target").unwrap();
  assert_eq!(updated.target, "/new-target");
  assert_eq!(updated.created_at, original_created_at, "created_at must be preserved on update");
  assert!(updated.updated_at > original.updated_at, "updated_at must advance on update");

  // Verify via get_symlink
  let fetched = ops.get_symlink("/link").unwrap().unwrap();
  assert_eq!(fetched.target, "/new-target");
  assert_eq!(fetched.created_at, original_created_at);
}

// --- Delete ---

#[test]
fn test_delete_symlink() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  ops.store_symlink(&ctx, "/link.txt", "/target.txt").unwrap();

  // Verify it exists
  assert!(ops.get_symlink("/link.txt").unwrap().is_some());

  // Delete it
  ops.delete_symlink(&ctx, "/link.txt").unwrap();

  // Should be gone
  assert!(ops.get_symlink("/link.txt").unwrap().is_none());
}

// --- Directory listing ---

#[test]
fn test_symlink_in_directory_listing() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  ops.store_symlink(&ctx, "/link.txt", "/target").unwrap();

  let children = ops.list_directory("/").unwrap();
  let symlink_child = children.iter().find(|c| c.name == "link.txt");
  assert!(symlink_child.is_some(), "symlink should appear in directory listing");

  let child = symlink_child.unwrap();
  assert_eq!(child.entry_type, EntryType::Symlink.to_u8());
  assert_eq!(child.total_size, 0);
}

// --- Nonexistent target is OK ---

#[test]
fn test_store_symlink_to_nonexistent() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Should succeed without validating target existence
  let record = ops.store_symlink(&ctx, "/link", "/nonexistent/path").unwrap();
  assert_eq!(record.target, "/nonexistent/path");
}

// --- Versioning (basic: current target reflects latest store) ---

#[test]
fn test_symlink_versioning_current_target() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  ops.store_symlink(&ctx, "/link", "/target-v1").unwrap();
  ops.store_symlink(&ctx, "/link", "/target-v2").unwrap();

  let current = ops.get_symlink("/link").unwrap().unwrap();
  assert_eq!(current.target, "/target-v2");
}

// --- Parent directory creation ---

#[test]
fn test_store_symlink_creates_parent_dirs() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  ops.store_symlink(&ctx, "/a/b/link", "/target").unwrap();

  // Root should contain "a"
  let root_children = ops.list_directory("/").unwrap();
  assert!(
    root_children.iter().any(|c| c.name == "a"),
    "root should contain directory 'a'"
  );

  // /a should contain "b"
  let a_children = ops.list_directory("/a").unwrap();
  assert!(
    a_children.iter().any(|c| c.name == "b"),
    "/a should contain directory 'b'"
  );

  // /a/b should contain "link" with Symlink type
  let b_children = ops.list_directory("/a/b").unwrap();
  let link_child = b_children.iter().find(|c| c.name == "link");
  assert!(link_child.is_some(), "/a/b should contain symlink 'link'");
  assert_eq!(link_child.unwrap().entry_type, EntryType::Symlink.to_u8());
}

// --- Delete nonexistent -> NotFound ---

#[test]
fn test_delete_nonexistent_symlink() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let result = ops.delete_symlink(&ctx, "/nonexistent");
  assert!(result.is_err());
  let err_msg = format!("{}", result.unwrap_err());
  assert!(err_msg.contains("Not found"), "expected NotFound error, got: {}", err_msg);
}

// --- Get nonexistent -> Ok(None) ---

#[test]
fn test_get_nonexistent_symlink() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let result = ops.get_symlink("/nonexistent").unwrap();
  assert!(result.is_none());
}

// --- Delete removes from parent listing ---

#[test]
fn test_delete_symlink_removes_from_parent() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  ops.store_symlink(&ctx, "/link.txt", "/target.txt").unwrap();

  // Verify in listing
  let children = ops.list_directory("/").unwrap();
  assert!(children.iter().any(|c| c.name == "link.txt"));

  // Delete
  ops.delete_symlink(&ctx, "/link.txt").unwrap();

  // Should be gone from listing
  let children = ops.list_directory("/").unwrap();
  assert!(
    !children.iter().any(|c| c.name == "link.txt"),
    "deleted symlink should not appear in directory listing"
  );
}

// --- Path normalization ---

#[test]
fn test_store_symlink_normalizes_paths() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Store with non-normalized path
  let record = ops.store_symlink(&ctx, "link.txt", "target.txt").unwrap();
  assert_eq!(record.path, "/link.txt");
  assert_eq!(record.target, "/target.txt");

  // Should be readable via normalized path
  let fetched = ops.get_symlink("/link.txt").unwrap();
  assert!(fetched.is_some());
}

// --- Multiple symlinks coexist ---

#[test]
fn test_multiple_symlinks_in_same_directory() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  ops.store_symlink(&ctx, "/link1", "/target1").unwrap();
  ops.store_symlink(&ctx, "/link2", "/target2").unwrap();
  ops.store_symlink(&ctx, "/link3", "/target3").unwrap();

  let children = ops.list_directory("/").unwrap();
  let symlink_names: Vec<&str> = children
    .iter()
    .filter(|c| c.entry_type == EntryType::Symlink.to_u8())
    .map(|c| c.name.as_str())
    .collect();

  assert!(symlink_names.contains(&"link1"));
  assert!(symlink_names.contains(&"link2"));
  assert!(symlink_names.contains(&"link3"));
}

// --- Symlink and file coexist in same directory ---

#[test]
fn test_symlink_and_file_coexist() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/real-file.txt", b"hello", None).unwrap();
  ops.store_symlink(&ctx, "/link-file.txt", "/real-file.txt").unwrap();

  let children = ops.list_directory("/").unwrap();
  assert_eq!(children.len(), 2);

  let file_child = children.iter().find(|c| c.name == "real-file.txt").unwrap();
  let symlink_child = children.iter().find(|c| c.name == "link-file.txt").unwrap();

  assert_eq!(file_child.entry_type, EntryType::FileRecord.to_u8());
  assert_eq!(symlink_child.entry_type, EntryType::Symlink.to_u8());
}

// --- Double delete ---

#[test]
fn test_double_delete_symlink_errors() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  ops.store_symlink(&ctx, "/link", "/target").unwrap();
  ops.delete_symlink(&ctx, "/link").unwrap();

  // Second delete should fail
  let result = ops.delete_symlink(&ctx, "/link");
  assert!(result.is_err());
  let err_msg = format!("{}", result.unwrap_err());
  assert!(err_msg.contains("Not found"), "expected NotFound, got: {}", err_msg);
}
