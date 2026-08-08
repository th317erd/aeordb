use std::collections::HashMap;

use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::version_manager::VersionManager;
use aeordb::engine::RequestContext;

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
  let ctx = RequestContext::system();
  let path = dir.path().join("test.aeor");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  let ops = DirectoryOps::new(&engine);
  ops.ensure_root_directory(&ctx).unwrap();
  engine
}

// --- Snapshot tests ---

#[test]
fn test_create_snapshot() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let snapshot = vm.create_snapshot(&ctx, "v1", HashMap::new()).unwrap();
  assert_eq!(snapshot.name, "v1");
  assert!(!snapshot.root_hash.is_empty());
  assert!(snapshot.created_at > 0);
}

#[test]
fn test_create_snapshot_stores_metadata() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let mut metadata = HashMap::new();
  metadata.insert("author".to_string(), "test-user".to_string());
  metadata.insert("description".to_string(), "initial release".to_string());

  let snapshot = vm.create_snapshot(&ctx, "v1", metadata.clone()).unwrap();
  // create_snapshot auto-injects "type"=manual for callers that don't specify;
  // user-provided fields are preserved verbatim.
  assert_eq!(snapshot.metadata.get("author").unwrap(), "test-user");
  assert_eq!(snapshot.metadata.get("description").unwrap(), "initial release");
  assert_eq!(snapshot.metadata.get("type").unwrap(), "manual");

  // Verify it persists through listing
  let listed = vm.list_snapshots().unwrap();
  assert_eq!(listed.len(), 1);
  assert_eq!(listed[0].metadata.get("author").unwrap(), "test-user");
  assert_eq!(listed[0].metadata.get("description").unwrap(), "initial release");
}

#[test]
fn test_create_snapshot_captures_head_hash() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  // Store a file to change HEAD
  ops.store_file_buffered(&ctx, "/test.txt", b"hello", None).unwrap();
  let head_hash = vm.get_head_hash().unwrap();

  let snapshot = vm.create_snapshot(&ctx, "v1", HashMap::new()).unwrap();
  assert_eq!(snapshot.root_hash, head_hash);
}

#[test]
fn test_restore_snapshot() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  // Take a snapshot of current HEAD
  let original_head = vm.get_head_hash().unwrap();
  vm.create_snapshot(&ctx, "before-change", HashMap::new()).unwrap();

  // Change HEAD to something different
  let new_root = engine.compute_hash(b"new-state").unwrap();
  engine.update_head(&new_root).unwrap();

  let changed_head = vm.get_head_hash().unwrap();
  assert_ne!(original_head, changed_head);

  // Restore snapshot — HEAD should revert
  vm.restore_snapshot(&ctx, "before-change").unwrap();
  let restored_head = vm.get_head_hash().unwrap();
  assert_eq!(restored_head, original_head);
}

#[test]
fn test_restore_snapshot_rolls_back_state() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  // Capture initial HEAD (root dir hash)
  let initial_head = vm.get_head_hash().unwrap();
  vm.create_snapshot(&ctx, "checkpoint", HashMap::new()).unwrap();

  // Simulate state change by moving HEAD
  let state_a = engine.compute_hash(b"state-a").unwrap();
  engine.update_head(&state_a).unwrap();
  assert_ne!(vm.get_head_hash().unwrap(), initial_head);

  // Another change
  let state_b = engine.compute_hash(b"state-b").unwrap();
  engine.update_head(&state_b).unwrap();
  assert_ne!(vm.get_head_hash().unwrap(), state_a);

  // Restore the checkpoint — HEAD should revert to initial
  vm.restore_snapshot(&ctx, "checkpoint").unwrap();
  assert_eq!(vm.get_head_hash().unwrap(), initial_head);
}

#[test]
fn restored_head_is_authoritative_for_current_file_reads() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  ops.store_file_buffered(&ctx, "/restored.txt", b"snapshot bytes", Some("text/plain")).unwrap();
  let expected = ops.get_metadata("/restored.txt").unwrap().unwrap();
  vm.create_snapshot(&ctx, "file-version", HashMap::new()).unwrap();
  ops.store_file_buffered(&ctx, "/restored.txt", b"newer bytes", Some("text/plain")).unwrap();

  vm.restore_snapshot(&ctx, "file-version").unwrap();

  assert_eq!(ops.read_file_buffered("/restored.txt").unwrap(), b"snapshot bytes");
  assert_eq!(ops.get_metadata("/restored.txt").unwrap().unwrap().content_hash, expected.content_hash);
}

#[test]
fn restored_head_is_authoritative_for_current_symlink_reads() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  ops.store_symlink(&ctx, "/restored-link", "/first-target").unwrap();
  vm.create_snapshot(&ctx, "symlink-version", HashMap::new()).unwrap();
  ops.store_symlink(&ctx, "/restored-link", "/second-target").unwrap();

  vm.restore_snapshot(&ctx, "symlink-version").unwrap();

  assert_eq!(ops.get_symlink("/restored-link").unwrap().unwrap().target, "/first-target");
}

#[test]
fn restored_head_reconciles_all_live_namespace_counters() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  ops.store_file_buffered(&ctx, "/selected.txt", b"selected", Some("text/plain")).unwrap();
  ops.store_symlink(&ctx, "/selected-link", "/selected.txt").unwrap();
  let expected = engine.counters().snapshot();
  vm.create_snapshot(&ctx, "counter-version", HashMap::new()).unwrap();
  ops.store_file_buffered(&ctx, "/later.txt", b"later", Some("text/plain")).unwrap();
  ops.create_directory(&ctx, "/later-directory").unwrap();

  vm.restore_snapshot(&ctx, "counter-version").unwrap();
  let actual = engine.counters().snapshot();

  assert_eq!(actual.files, expected.files);
  assert_eq!(actual.directories, expected.directories);
  assert_eq!(actual.symlinks, expected.symlinks);
  assert_eq!(actual.logical_data_size, expected.logical_data_size);
}

#[test]
fn restored_head_is_authoritative_for_exists() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  ops.store_file_buffered(&ctx, "/before.txt", b"before", Some("text/plain")).unwrap();
  ops.create_directory(&ctx, "/before-directory").unwrap();
  vm.create_snapshot(&ctx, "exists-version", HashMap::new()).unwrap();

  ops.delete_file(&ctx, "/before.txt").unwrap();
  ops.delete_directory(&ctx, "/before-directory").unwrap();
  ops.store_file_buffered(&ctx, "/after.txt", b"after", Some("text/plain")).unwrap();
  ops.create_directory(&ctx, "/after-directory").unwrap();

  vm.restore_snapshot(&ctx, "exists-version").unwrap();

  assert!(ops.exists("/before.txt").unwrap());
  assert!(ops.exists("/before-directory").unwrap());
  assert!(!ops.exists("/after.txt").unwrap());
  assert!(!ops.exists("/after-directory").unwrap());
}

#[test]
fn restored_head_is_authoritative_for_nested_listing_without_a_live_path_locator() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  ops.store_file_buffered(&ctx, "/nested/snapshot.txt", b"snapshot", Some("text/plain")).unwrap();
  vm.create_snapshot(&ctx, "nested-version", HashMap::new()).unwrap();
  ops.store_file_buffered(&ctx, "/nested/newer.txt", b"newer", Some("text/plain")).unwrap();
  vm.restore_snapshot(&ctx, "nested-version").unwrap();

  let locator = aeordb::engine::directory_path_hash("/nested", &engine.hash_algo()).unwrap();
  engine.mark_entry_deleted(&locator).unwrap();

  let children = ops.list_directory("/nested").expect("HEAD-selected directory must not depend on its derived locator");
  assert_eq!(children.iter().map(|child| child.name.as_str()).collect::<Vec<_>>(), vec!["snapshot.txt"]);
}

#[test]
fn restored_head_is_authoritative_for_file_rename_and_copy_sources() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  ops.store_file_buffered(&ctx, "/rename-source.txt", b"snapshot rename", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/copy-source.txt", b"snapshot copy", Some("text/plain")).unwrap();
  vm.create_snapshot(&ctx, "source-version", HashMap::new()).unwrap();
  ops.store_file_buffered(&ctx, "/rename-source.txt", b"newer rename", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/copy-source.txt", b"newer copy", Some("text/plain")).unwrap();
  vm.restore_snapshot(&ctx, "source-version").unwrap();

  ops.rename_file(&ctx, "/rename-source.txt", "/renamed.txt").unwrap();
  ops.copy_file(&ctx, "/copy-source.txt", "/copied.txt").unwrap();

  assert_eq!(ops.read_file_buffered("/renamed.txt").unwrap(), b"snapshot rename");
  assert_eq!(ops.read_file_buffered("/copied.txt").unwrap(), b"snapshot copy");
}

#[test]
fn restored_head_is_authoritative_for_rename_destination_existence() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  ops.store_file_buffered(&ctx, "/source.txt", b"source", Some("text/plain")).unwrap();
  vm.create_snapshot(&ctx, "destination-version", HashMap::new()).unwrap();
  ops.store_file_buffered(&ctx, "/future.txt", b"not selected", Some("text/plain")).unwrap();
  vm.restore_snapshot(&ctx, "destination-version").unwrap();

  ops.rename_file(&ctx, "/source.txt", "/future.txt").expect("a derived locator from a newer root must not reserve an absent destination");
  assert_eq!(ops.read_file_buffered("/future.txt").unwrap(), b"source");
}

#[test]
fn restored_head_is_authoritative_for_symlink_rename_source() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  ops.store_symlink(&ctx, "/source-link", "/snapshot-target").unwrap();
  vm.create_snapshot(&ctx, "symlink-rename-version", HashMap::new()).unwrap();
  ops.store_symlink(&ctx, "/source-link", "/newer-target").unwrap();
  vm.restore_snapshot(&ctx, "symlink-rename-version").unwrap();

  ops.rename_symlink(&ctx, "/source-link", "/renamed-link").unwrap();
  assert_eq!(ops.get_symlink("/renamed-link").unwrap().unwrap().target, "/snapshot-target");
}

#[test]
fn test_list_snapshots() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  // Snapshot dedup: back-to-back snapshots with no writes between them
  // get deduplicated to the prior snapshot. Write something between each
  // to force HEAD changes.
  ops.store_file_buffered(&ctx, "/a.txt", b"a", None).unwrap();
  vm.create_snapshot(&ctx, "v1", HashMap::new()).unwrap();
  ops.store_file_buffered(&ctx, "/b.txt", b"b", None).unwrap();
  vm.create_snapshot(&ctx, "v2", HashMap::new()).unwrap();
  ops.store_file_buffered(&ctx, "/c.txt", b"c", None).unwrap();
  vm.create_snapshot(&ctx, "v3", HashMap::new()).unwrap();

  let snapshots = vm.list_snapshots().unwrap();
  assert_eq!(snapshots.len(), 3);
}

#[test]
fn test_list_snapshots_ordered_by_time() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let ops = DirectoryOps::new(&engine);
  // Writes between snapshots prevent dedup; small sleeps keep timestamps distinct.
  ops.store_file_buffered(&ctx, "/a.txt", b"a", None).unwrap();
  vm.create_snapshot(&ctx, "alpha", HashMap::new()).unwrap();
  std::thread::sleep(std::time::Duration::from_millis(2));
  ops.store_file_buffered(&ctx, "/b.txt", b"b", None).unwrap();
  vm.create_snapshot(&ctx, "beta", HashMap::new()).unwrap();
  std::thread::sleep(std::time::Duration::from_millis(2));
  ops.store_file_buffered(&ctx, "/c.txt", b"c", None).unwrap();
  vm.create_snapshot(&ctx, "gamma", HashMap::new()).unwrap();

  let snapshots = vm.list_snapshots().unwrap();
  assert_eq!(snapshots.len(), 3);
  assert!(snapshots[0].created_at <= snapshots[1].created_at);
  assert!(snapshots[1].created_at <= snapshots[2].created_at);
  assert_eq!(snapshots[0].name, "alpha");
  assert_eq!(snapshots[1].name, "beta");
  assert_eq!(snapshots[2].name, "gamma");
}

#[test]
fn test_delete_snapshot() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  vm.create_snapshot(&ctx, "to-delete", HashMap::new()).unwrap();
  assert_eq!(vm.list_snapshots().unwrap().len(), 1);

  vm.delete_snapshot(&ctx, "to-delete").unwrap();
  assert_eq!(vm.list_snapshots().unwrap().len(), 0);
}

// --- Fork tests ---

#[test]
fn test_create_fork() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let fork = vm.create_fork(&ctx, "feature-branch", None).unwrap();
  assert_eq!(fork.name, "feature-branch");
  assert!(!fork.root_hash.is_empty());
  assert!(fork.created_at > 0);
}

#[test]
fn test_create_fork_from_head() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  ops.store_file_buffered(&ctx, "/base.txt", b"base content", None).unwrap();
  let head_hash = vm.get_head_hash().unwrap();

  let fork = vm.create_fork(&ctx, "from-head", Some("HEAD")).unwrap();
  assert_eq!(fork.root_hash, head_hash);
}

#[test]
fn test_create_fork_from_snapshot() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  ops.store_file_buffered(&ctx, "/v1-file.txt", b"v1 content", None).unwrap();
  let snapshot = vm.create_snapshot(&ctx, "v1", HashMap::new()).unwrap();

  // Move HEAD forward
  ops.store_file_buffered(&ctx, "/v2-file.txt", b"v2 content", None).unwrap();

  // Fork from the snapshot, not HEAD
  let fork = vm.create_fork(&ctx, "from-v1", Some("v1")).unwrap();
  assert_eq!(fork.root_hash, snapshot.root_hash);
}

#[test]
fn test_fork_isolation() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  // Establish baseline HEAD
  ops.store_file_buffered(&ctx, "/shared.txt", b"shared", None).unwrap();
  let head_before_fork = vm.get_head_hash().unwrap();

  // Create a fork
  let _fork = vm.create_fork(&ctx, "isolated", None).unwrap();

  // Retain two real namespace roots, restore the first, then point the fork at
  // the second. Updating the fork must not move current HEAD.
  ops.store_file_buffered(&ctx, "/head-b.txt", b"head b", None).unwrap();
  vm.create_snapshot(&ctx, "head-b", HashMap::new()).unwrap();
  ops.store_file_buffered(&ctx, "/head-c.txt", b"head c", None).unwrap();
  let new_root = vm.get_head_hash().unwrap();
  vm.restore_snapshot(&ctx, "head-b").unwrap();
  let head_before_update = vm.get_head_hash().unwrap();
  vm.update_fork_hash("isolated", &new_root).unwrap();

  // HEAD should remain unchanged
  let head_after = vm.get_head_hash().unwrap();
  assert_eq!(head_before_update, head_after);
  assert_ne!(head_before_fork, head_after);

  // The fork's hash should differ from HEAD
  let fork_hash = vm.get_fork_hash("isolated").unwrap().unwrap();
  assert_ne!(fork_hash, head_after);
}

#[test]
fn test_promote_fork() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  vm.create_fork(&ctx, "to-promote", None).unwrap();
  let new_root = vm.get_fork_hash("to-promote").unwrap().unwrap();
  DirectoryOps::new(&engine).store_file_buffered(&ctx, "/after-fork.txt", b"advance HEAD", None).unwrap();
  assert_ne!(vm.get_head_hash().unwrap(), new_root);

  vm.promote_fork(&ctx, "to-promote").unwrap();

  // HEAD should now be the fork's hash
  let head = vm.get_head_hash().unwrap();
  assert_eq!(head, new_root);

  // Fork should no longer exist
  assert!(vm.get_fork_hash("to-promote").unwrap().is_none());
}

#[test]
fn test_promote_fork_updates_head() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  ops.store_file_buffered(&ctx, "/before.txt", b"before", None).unwrap();
  let original_head = vm.get_head_hash().unwrap();

  let fork = vm.create_fork(&ctx, "update-head", None).unwrap();
  assert_eq!(fork.root_hash, original_head);

  let diverged_root = fork.root_hash;
  ops.store_file_buffered(&ctx, "/after-fork.txt", b"advance HEAD", None).unwrap();
  let advanced_head = vm.get_head_hash().unwrap();
  assert_ne!(advanced_head, diverged_root);

  vm.promote_fork(&ctx, "update-head").unwrap();

  let new_head = vm.get_head_hash().unwrap();
  assert_eq!(new_head, diverged_root);
  assert_eq!(new_head, original_head);
  assert_ne!(new_head, advanced_head);
}

#[test]
fn test_abandon_fork() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  vm.create_fork(&ctx, "throwaway", None).unwrap();
  assert_eq!(vm.list_forks().unwrap().len(), 1);

  vm.abandon_fork(&ctx, "throwaway").unwrap();
  assert_eq!(vm.list_forks().unwrap().len(), 0);

  // Fork hash should return None
  assert!(vm.get_fork_hash("throwaway").unwrap().is_none());
}

#[test]
fn test_list_forks() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  vm.create_fork(&ctx, "fork-a", None).unwrap();
  vm.create_fork(&ctx, "fork-b", None).unwrap();
  vm.create_fork(&ctx, "fork-c", None).unwrap();

  let forks = vm.list_forks().unwrap();
  assert_eq!(forks.len(), 3);

  let names: Vec<&str> = forks.iter().map(|f| f.name.as_str()).collect();
  assert!(names.contains(&"fork-a"));
  assert!(names.contains(&"fork-b"));
  assert!(names.contains(&"fork-c"));
}

// --- Auto-snapshot naming ---

#[test]
fn test_auto_snapshot_naming() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
  let auto_name = format!("auto-{}", timestamp);

  let snapshot = vm.create_snapshot(&ctx, &auto_name, HashMap::new()).unwrap();
  assert!(snapshot.name.starts_with("auto-"));

  // Verify it can be looked up
  let snapshots = vm.list_snapshots().unwrap();
  assert_eq!(snapshots.len(), 1);
  assert!(snapshots[0].name.starts_with("auto-"));
}

// --- resolve_root_hash tests ---

#[test]
fn test_resolve_root_hash_head() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let head = vm.get_head_hash().unwrap();

  // None resolves to HEAD
  let resolved_none = vm.resolve_root_hash(None).unwrap();
  assert_eq!(resolved_none, head);

  // "HEAD" resolves to HEAD
  let resolved_head = vm.resolve_root_hash(Some("HEAD")).unwrap();
  assert_eq!(resolved_head, head);
}

#[test]
fn test_resolve_root_hash_fork() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let fork = vm.create_fork(&ctx, "my-fork", None).unwrap();

  let resolved = vm.resolve_root_hash(Some("my-fork")).unwrap();
  assert_eq!(resolved, fork.root_hash);
}

#[test]
fn test_resolve_root_hash_snapshot() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let snapshot = vm.create_snapshot(&ctx, "my-snap", HashMap::new()).unwrap();

  let resolved = vm.resolve_root_hash(Some("my-snap")).unwrap();
  assert_eq!(resolved, snapshot.root_hash);
}

// --- Error cases ---

#[test]
fn test_duplicate_snapshot_name_error() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  vm.create_snapshot(&ctx, "unique", HashMap::new()).unwrap();
  let result = vm.create_snapshot(&ctx, "unique", HashMap::new());

  assert!(result.is_err());
  let error_message = format!("{}", result.unwrap_err());
  assert!(error_message.contains("Already exists"));
}

#[test]
fn test_nonexistent_snapshot_error() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let result = vm.restore_snapshot(&ctx, "ghost");
  assert!(result.is_err());
  let error_message = format!("{}", result.unwrap_err());
  assert!(error_message.contains("Not found"));

  let result = vm.delete_snapshot(&ctx, "ghost");
  assert!(result.is_err());

  let result = vm.get_snapshot_hash("ghost");
  assert!(result.is_err());
}

#[test]
fn test_nonexistent_fork_error() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let result = vm.promote_fork(&ctx, "phantom");
  assert!(result.is_err());
  let error_message = format!("{}", result.unwrap_err());
  assert!(error_message.contains("Not found"));

  let result = vm.abandon_fork(&ctx, "phantom");
  assert!(result.is_err());

  // get_fork_hash returns Ok(None) for nonexistent, not an error
  let result = vm.get_fork_hash("phantom").unwrap();
  assert!(result.is_none());
}

// --- Edge cases and failure paths ---

#[test]
fn test_delete_snapshot_then_recreate() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  vm.create_snapshot(&ctx, "recycled", HashMap::new()).unwrap();
  vm.delete_snapshot(&ctx, "recycled").unwrap();

  // Should be able to recreate after deletion
  let snapshot = vm.create_snapshot(&ctx, "recycled", HashMap::new()).unwrap();
  assert_eq!(snapshot.name, "recycled");
}

#[test]
fn test_abandon_fork_then_recreate() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  vm.create_fork(&ctx, "temp", None).unwrap();
  vm.abandon_fork(&ctx, "temp").unwrap();

  // Should be able to recreate
  let fork = vm.create_fork(&ctx, "temp", None).unwrap();
  assert_eq!(fork.name, "temp");
}

#[test]
fn test_duplicate_fork_name_error() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  vm.create_fork(&ctx, "unique-fork", None).unwrap();
  let result = vm.create_fork(&ctx, "unique-fork", None);

  assert!(result.is_err());
  let error_message = format!("{}", result.unwrap_err());
  assert!(error_message.contains("Already exists"));
}

#[test]
fn test_create_fork_from_nonexistent_snapshot_error() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let result = vm.create_fork(&ctx, "bad-base", Some("no-such-snapshot"));
  assert!(result.is_err());
  let error_message = format!("{}", result.unwrap_err());
  assert!(error_message.contains("Not found"));
}

#[test]
fn test_resolve_root_hash_nonexistent_name_error() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let result = vm.resolve_root_hash(Some("nothing"));
  assert!(result.is_err());
}

#[test]
fn test_update_fork_hash() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let fork = vm.create_fork(&ctx, "mutable", None).unwrap();
  let original_hash = fork.root_hash.clone();

  DirectoryOps::new(&engine).store_file_buffered(&ctx, "/updated-root.txt", b"valid root", None).unwrap();
  let new_hash = engine.head_hash().unwrap();
  let durability_before = engine.durability_snapshot().unwrap();
  let writes_before = engine.counters().snapshot().writes_total;
  vm.update_fork_hash("mutable", &new_hash).unwrap();
  let durability_after = engine.durability_snapshot().unwrap();
  assert_eq!(durability_after.next_sequence, durability_before.next_sequence + 1);
  assert_eq!(durability_after.hard_frontier, durability_before.next_sequence);
  assert_eq!(engine.counters().snapshot().writes_total, writes_before + 1);

  let fetched = vm.get_fork_hash("mutable").unwrap().unwrap();
  assert_eq!(fetched, new_hash);
  assert_ne!(fetched, original_hash);
}

#[test]
fn test_update_nonexistent_fork_error() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let result = vm.update_fork_hash("ghost", &[0u8; 32]);
  assert!(result.is_err());
}

#[test]
fn test_update_fork_rejects_missing_or_wrong_type_namespace_roots() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);
  let original = vm.create_fork(&ctx, "validated", None).unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let writes_before = engine.counters().snapshot().writes_total;

  let missing = vec![0xA7; engine.hash_algo().hash_length()];
  assert!(vm.update_fork_hash("validated", &missing).is_err());

  let wrong_type = vec![0xA8; engine.hash_algo().hash_length()];
  engine.store_entry(aeordb::engine::EntryType::FileRecord, &wrong_type, b"not a directory").unwrap();
  assert!(vm.update_fork_hash("validated", &wrong_type).is_err());

  assert_eq!(vm.get_fork_hash("validated").unwrap().unwrap(), original.root_hash);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert_eq!(engine.counters().snapshot().writes_total, writes_before);
}

#[test]
fn test_version_locators_reject_masquerading_entry_types() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let root_hash = engine.head_hash().unwrap();
  let hash_length = engine.hash_algo().hash_length();

  let fork_key = engine.compute_hash(b"::aeordb:fork:masquerade").unwrap();
  let fork_value =
    aeordb::engine::version_manager::ForkInfo { name: "masquerade".to_string(), root_hash: root_hash.clone(), created_at: 1 }
      .serialize(hash_length)
      .unwrap();
  engine.store_entry_typed(aeordb::engine::EntryType::FileRecord, &fork_key, &fork_value, aeordb::engine::kv_store::KV_TYPE_FORK).unwrap();

  let snapshot_key = engine.compute_hash(b"snap:masquerade").unwrap();
  let snapshot_value = aeordb::engine::version_manager::SnapshotInfo {
    name: "masquerade".to_string(),
    root_hash: root_hash.clone(),
    created_at: 1,
    metadata: HashMap::new(),
  }
  .serialize(hash_length)
  .unwrap();
  engine
    .store_entry_typed(aeordb::engine::EntryType::FileRecord, &snapshot_key, &snapshot_value, aeordb::engine::kv_store::KV_TYPE_SNAPSHOT)
    .unwrap();

  let vm = VersionManager::new(&engine);
  assert!(vm.get_fork_hash("masquerade").is_err());
  assert!(vm.get_snapshot_hash("masquerade").is_err());
  let head_before = engine.head_hash().unwrap();
  assert!(vm.promote_fork(&ctx, "masquerade").is_err());
  assert_eq!(engine.head_hash().unwrap(), head_before);
}

#[test]
fn test_snapshot_serialization_roundtrip() {
  let mut metadata = HashMap::new();
  metadata.insert("key".to_string(), "value".to_string());

  let original = aeordb::engine::version_manager::SnapshotInfo {
    name: "test-snap".to_string(),
    root_hash: vec![0xAB; 32],
    created_at: 1234567890000,
    metadata,
  };

  let serialized = original.serialize(32).unwrap();
  let deserialized = aeordb::engine::version_manager::SnapshotInfo::deserialize(&serialized, 32, 0).unwrap();

  assert_eq!(deserialized.name, original.name);
  assert_eq!(deserialized.root_hash, original.root_hash);
  assert_eq!(deserialized.created_at, original.created_at);
  assert_eq!(deserialized.metadata.get("key").unwrap(), "value");
}

#[test]
fn test_fork_serialization_roundtrip() {
  let original =
    aeordb::engine::version_manager::ForkInfo { name: "test-fork".to_string(), root_hash: vec![0xCD; 32], created_at: 9876543210000 };

  let serialized = original.serialize(32).unwrap();
  let deserialized = aeordb::engine::version_manager::ForkInfo::deserialize(&serialized, 32, 0).unwrap();

  assert_eq!(deserialized.name, original.name);
  assert_eq!(deserialized.root_hash, original.root_hash);
  assert_eq!(deserialized.created_at, original.created_at);
}

#[test]
fn test_snapshot_deserialize_corrupt_data() {
  // Too short
  let result = aeordb::engine::version_manager::SnapshotInfo::deserialize(&[0], 32, 0);
  assert!(result.is_err());

  // Empty
  let result = aeordb::engine::version_manager::SnapshotInfo::deserialize(&[], 32, 0);
  assert!(result.is_err());
}

#[test]
fn test_fork_deserialize_corrupt_data() {
  let result = aeordb::engine::version_manager::ForkInfo::deserialize(&[0], 32, 0);
  assert!(result.is_err());

  let result = aeordb::engine::version_manager::ForkInfo::deserialize(&[], 32, 0);
  assert!(result.is_err());
}

#[test]
fn test_snapshot_with_empty_metadata() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let snapshot = vm.create_snapshot(&ctx, "no-meta", HashMap::new()).unwrap();
  // Even with no user metadata, the engine injects "type"=manual so lifecycle
  // retention has the information it needs.
  assert_eq!(snapshot.metadata.len(), 1);
  assert_eq!(snapshot.metadata.get("type").unwrap(), "manual");

  let listed = vm.list_snapshots().unwrap();
  assert_eq!(listed.len(), 1);
  assert_eq!(listed[0].metadata.get("type").unwrap(), "manual");
}

#[test]
fn test_multiple_forks_independent_hashes() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let fork_a = vm.create_fork(&ctx, "fork-a", None).unwrap();
  let fork_b = vm.create_fork(&ctx, "fork-b", None).unwrap();

  // Both start from HEAD so same initial hash
  assert_eq!(fork_a.root_hash, fork_b.root_hash);

  // Update fork-a only
  DirectoryOps::new(&engine).store_file_buffered(&ctx, "/fork-a-data.txt", b"valid root", None).unwrap();
  let new_hash = engine.head_hash().unwrap();
  vm.update_fork_hash("fork-a", &new_hash).unwrap();

  // fork-a changed, fork-b unchanged
  let hash_a = vm.get_fork_hash("fork-a").unwrap().unwrap();
  let hash_b = vm.get_fork_hash("fork-b").unwrap().unwrap();
  assert_eq!(hash_a, new_hash);
  assert_ne!(hash_a, hash_b);
}

#[test]
fn test_resolve_prefers_fork_over_snapshot_with_same_name() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  // Create a snapshot named "shared"
  let _snapshot = vm.create_snapshot(&ctx, "shared", HashMap::new()).unwrap();

  // Create a fork named "shared" — fork key uses a different hash prefix
  // so no collision in KV store
  vm.create_fork(&ctx, "shared", None).unwrap();

  // Update fork's hash to something distinct
  DirectoryOps::new(&engine).store_file_buffered(&ctx, "/fork-wins.txt", b"valid root", None).unwrap();
  let fork_root = engine.head_hash().unwrap();
  vm.update_fork_hash("shared", &fork_root).unwrap();

  // resolve_root_hash should prefer fork
  let resolved = vm.resolve_root_hash(Some("shared")).unwrap();
  assert_eq!(resolved, fork_root);
}
