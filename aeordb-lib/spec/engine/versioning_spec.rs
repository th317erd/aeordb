use std::collections::HashMap;

use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::version_manager::VersionManager;

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
  let path = dir.path().join("test.aeor");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  let ops = DirectoryOps::new(&engine);
  ops.ensure_root_directory().unwrap();
  engine
}

// --- Snapshot tests ---

#[test]
fn test_create_snapshot() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let snapshot = vm.create_snapshot("v1", HashMap::new()).unwrap();
  assert_eq!(snapshot.name, "v1");
  assert!(!snapshot.root_hash.is_empty());
  assert!(snapshot.created_at > 0);
}

#[test]
fn test_create_snapshot_stores_metadata() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let mut metadata = HashMap::new();
  metadata.insert("author".to_string(), "test-user".to_string());
  metadata.insert("description".to_string(), "initial release".to_string());

  let snapshot = vm.create_snapshot("v1", metadata.clone()).unwrap();
  assert_eq!(snapshot.metadata, metadata);

  // Verify it persists through listing
  let listed = vm.list_snapshots().unwrap();
  assert_eq!(listed.len(), 1);
  assert_eq!(listed[0].metadata.get("author").unwrap(), "test-user");
  assert_eq!(listed[0].metadata.get("description").unwrap(), "initial release");
}

#[test]
fn test_create_snapshot_captures_head_hash() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  // Store a file to change HEAD
  ops.store_file("/test.txt", b"hello", None).unwrap();
  let head_hash = vm.get_head_hash().unwrap();

  let snapshot = vm.create_snapshot("v1", HashMap::new()).unwrap();
  assert_eq!(snapshot.root_hash, head_hash);
}

#[test]
fn test_restore_snapshot() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  // Take a snapshot of current HEAD
  let original_head = vm.get_head_hash().unwrap();
  vm.create_snapshot("before-change", HashMap::new()).unwrap();

  // Change HEAD to something different
  let new_root = engine.compute_hash(b"new-state").unwrap();
  engine.update_head(&new_root).unwrap();

  let changed_head = vm.get_head_hash().unwrap();
  assert_ne!(original_head, changed_head);

  // Restore snapshot — HEAD should revert
  vm.restore_snapshot("before-change").unwrap();
  let restored_head = vm.get_head_hash().unwrap();
  assert_eq!(restored_head, original_head);
}

#[test]
fn test_restore_snapshot_rolls_back_state() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  // Capture initial HEAD (root dir hash)
  let initial_head = vm.get_head_hash().unwrap();
  vm.create_snapshot("checkpoint", HashMap::new()).unwrap();

  // Simulate state change by moving HEAD
  let state_a = engine.compute_hash(b"state-a").unwrap();
  engine.update_head(&state_a).unwrap();
  assert_ne!(vm.get_head_hash().unwrap(), initial_head);

  // Another change
  let state_b = engine.compute_hash(b"state-b").unwrap();
  engine.update_head(&state_b).unwrap();
  assert_ne!(vm.get_head_hash().unwrap(), state_a);

  // Restore the checkpoint — HEAD should revert to initial
  vm.restore_snapshot("checkpoint").unwrap();
  assert_eq!(vm.get_head_hash().unwrap(), initial_head);
}

#[test]
fn test_list_snapshots() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  vm.create_snapshot("v1", HashMap::new()).unwrap();
  vm.create_snapshot("v2", HashMap::new()).unwrap();
  vm.create_snapshot("v3", HashMap::new()).unwrap();

  let snapshots = vm.list_snapshots().unwrap();
  assert_eq!(snapshots.len(), 3);
}

#[test]
fn test_list_snapshots_ordered_by_time() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  vm.create_snapshot("alpha", HashMap::new()).unwrap();
  // Small delay to ensure distinct timestamps
  std::thread::sleep(std::time::Duration::from_millis(2));
  vm.create_snapshot("beta", HashMap::new()).unwrap();
  std::thread::sleep(std::time::Duration::from_millis(2));
  vm.create_snapshot("gamma", HashMap::new()).unwrap();

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
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  vm.create_snapshot("to-delete", HashMap::new()).unwrap();
  assert_eq!(vm.list_snapshots().unwrap().len(), 1);

  vm.delete_snapshot("to-delete").unwrap();
  assert_eq!(vm.list_snapshots().unwrap().len(), 0);
}

// --- Fork tests ---

#[test]
fn test_create_fork() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let fork = vm.create_fork("feature-branch", None).unwrap();
  assert_eq!(fork.name, "feature-branch");
  assert!(!fork.root_hash.is_empty());
  assert!(fork.created_at > 0);
}

#[test]
fn test_create_fork_from_head() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  ops.store_file("/base.txt", b"base content", None).unwrap();
  let head_hash = vm.get_head_hash().unwrap();

  let fork = vm.create_fork("from-head", Some("HEAD")).unwrap();
  assert_eq!(fork.root_hash, head_hash);
}

#[test]
fn test_create_fork_from_snapshot() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  ops.store_file("/v1-file.txt", b"v1 content", None).unwrap();
  let snapshot = vm.create_snapshot("v1", HashMap::new()).unwrap();

  // Move HEAD forward
  ops.store_file("/v2-file.txt", b"v2 content", None).unwrap();

  // Fork from the snapshot, not HEAD
  let fork = vm.create_fork("from-v1", Some("v1")).unwrap();
  assert_eq!(fork.root_hash, snapshot.root_hash);
}

#[test]
fn test_fork_isolation() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  // Establish baseline HEAD
  ops.store_file("/shared.txt", b"shared", None).unwrap();
  let head_before_fork = vm.get_head_hash().unwrap();

  // Create a fork
  let _fork = vm.create_fork("isolated", None).unwrap();

  // Update the fork's hash (simulating a write to the fork)
  let new_root = engine.compute_hash(b"fake-fork-root").unwrap();
  vm.update_fork_hash("isolated", &new_root).unwrap();

  // HEAD should remain unchanged
  let head_after = vm.get_head_hash().unwrap();
  assert_eq!(head_before_fork, head_after);

  // The fork's hash should differ from HEAD
  let fork_hash = vm.get_fork_hash("isolated").unwrap().unwrap();
  assert_ne!(fork_hash, head_after);
}

#[test]
fn test_promote_fork() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  vm.create_fork("to-promote", None).unwrap();

  // Update fork hash to something distinct
  let new_root = engine.compute_hash(b"promoted-root").unwrap();
  vm.update_fork_hash("to-promote", &new_root).unwrap();

  vm.promote_fork("to-promote").unwrap();

  // HEAD should now be the fork's hash
  let head = vm.get_head_hash().unwrap();
  assert_eq!(head, new_root);

  // Fork should no longer exist
  assert!(vm.get_fork_hash("to-promote").unwrap().is_none());
}

#[test]
fn test_promote_fork_updates_head() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  ops.store_file("/before.txt", b"before", None).unwrap();
  let original_head = vm.get_head_hash().unwrap();

  let fork = vm.create_fork("update-head", None).unwrap();
  assert_eq!(fork.root_hash, original_head);

  // Simulate a fork diverging by updating its hash
  let diverged_root = engine.compute_hash(b"diverged-content").unwrap();
  vm.update_fork_hash("update-head", &diverged_root).unwrap();

  vm.promote_fork("update-head").unwrap();

  let new_head = vm.get_head_hash().unwrap();
  assert_eq!(new_head, diverged_root);
  assert_ne!(new_head, original_head);
}

#[test]
fn test_abandon_fork() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  vm.create_fork("throwaway", None).unwrap();
  assert_eq!(vm.list_forks().unwrap().len(), 1);

  vm.abandon_fork("throwaway").unwrap();
  assert_eq!(vm.list_forks().unwrap().len(), 0);

  // Fork hash should return None
  assert!(vm.get_fork_hash("throwaway").unwrap().is_none());
}

#[test]
fn test_list_forks() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  vm.create_fork("fork-a", None).unwrap();
  vm.create_fork("fork-b", None).unwrap();
  vm.create_fork("fork-c", None).unwrap();

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
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
  let auto_name = format!("auto-{}", timestamp);

  let snapshot = vm.create_snapshot(&auto_name, HashMap::new()).unwrap();
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
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let fork = vm.create_fork("my-fork", None).unwrap();

  let resolved = vm.resolve_root_hash(Some("my-fork")).unwrap();
  assert_eq!(resolved, fork.root_hash);
}

#[test]
fn test_resolve_root_hash_snapshot() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let snapshot = vm.create_snapshot("my-snap", HashMap::new()).unwrap();

  let resolved = vm.resolve_root_hash(Some("my-snap")).unwrap();
  assert_eq!(resolved, snapshot.root_hash);
}

// --- Error cases ---

#[test]
fn test_duplicate_snapshot_name_error() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  vm.create_snapshot("unique", HashMap::new()).unwrap();
  let result = vm.create_snapshot("unique", HashMap::new());

  assert!(result.is_err());
  let error_message = format!("{}", result.unwrap_err());
  assert!(error_message.contains("Already exists"));
}

#[test]
fn test_nonexistent_snapshot_error() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let result = vm.restore_snapshot("ghost");
  assert!(result.is_err());
  let error_message = format!("{}", result.unwrap_err());
  assert!(error_message.contains("Not found"));

  let result = vm.delete_snapshot("ghost");
  assert!(result.is_err());

  let result = vm.get_snapshot_hash("ghost");
  assert!(result.is_err());
}

#[test]
fn test_nonexistent_fork_error() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let result = vm.promote_fork("phantom");
  assert!(result.is_err());
  let error_message = format!("{}", result.unwrap_err());
  assert!(error_message.contains("Not found"));

  let result = vm.abandon_fork("phantom");
  assert!(result.is_err());

  // get_fork_hash returns Ok(None) for nonexistent, not an error
  let result = vm.get_fork_hash("phantom").unwrap();
  assert!(result.is_none());
}

// --- Edge cases and failure paths ---

#[test]
fn test_delete_snapshot_then_recreate() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  vm.create_snapshot("recycled", HashMap::new()).unwrap();
  vm.delete_snapshot("recycled").unwrap();

  // Should be able to recreate after deletion
  let snapshot = vm.create_snapshot("recycled", HashMap::new()).unwrap();
  assert_eq!(snapshot.name, "recycled");
}

#[test]
fn test_abandon_fork_then_recreate() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  vm.create_fork("temp", None).unwrap();
  vm.abandon_fork("temp").unwrap();

  // Should be able to recreate
  let fork = vm.create_fork("temp", None).unwrap();
  assert_eq!(fork.name, "temp");
}

#[test]
fn test_duplicate_fork_name_error() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  vm.create_fork("unique-fork", None).unwrap();
  let result = vm.create_fork("unique-fork", None);

  assert!(result.is_err());
  let error_message = format!("{}", result.unwrap_err());
  assert!(error_message.contains("Already exists"));
}

#[test]
fn test_create_fork_from_nonexistent_snapshot_error() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let result = vm.create_fork("bad-base", Some("no-such-snapshot"));
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
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let fork = vm.create_fork("mutable", None).unwrap();
  let original_hash = fork.root_hash.clone();

  let new_hash = engine.compute_hash(b"updated-root").unwrap();
  vm.update_fork_hash("mutable", &new_hash).unwrap();

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
fn test_snapshot_serialization_roundtrip() {
  let mut metadata = HashMap::new();
  metadata.insert("key".to_string(), "value".to_string());

  let original = aeordb::engine::version_manager::SnapshotInfo {
    name: "test-snap".to_string(),
    root_hash: vec![0xAB; 32],
    created_at: 1234567890000,
    metadata,
  };

  let serialized = original.serialize(32);
  let deserialized = aeordb::engine::version_manager::SnapshotInfo::deserialize(&serialized, 32).unwrap();

  assert_eq!(deserialized.name, original.name);
  assert_eq!(deserialized.root_hash, original.root_hash);
  assert_eq!(deserialized.created_at, original.created_at);
  assert_eq!(deserialized.metadata.get("key").unwrap(), "value");
}

#[test]
fn test_fork_serialization_roundtrip() {
  let original = aeordb::engine::version_manager::ForkInfo {
    name: "test-fork".to_string(),
    root_hash: vec![0xCD; 32],
    created_at: 9876543210000,
  };

  let serialized = original.serialize(32);
  let deserialized = aeordb::engine::version_manager::ForkInfo::deserialize(&serialized, 32).unwrap();

  assert_eq!(deserialized.name, original.name);
  assert_eq!(deserialized.root_hash, original.root_hash);
  assert_eq!(deserialized.created_at, original.created_at);
}

#[test]
fn test_snapshot_deserialize_corrupt_data() {
  // Too short
  let result = aeordb::engine::version_manager::SnapshotInfo::deserialize(&[0], 32);
  assert!(result.is_err());

  // Empty
  let result = aeordb::engine::version_manager::SnapshotInfo::deserialize(&[], 32);
  assert!(result.is_err());
}

#[test]
fn test_fork_deserialize_corrupt_data() {
  let result = aeordb::engine::version_manager::ForkInfo::deserialize(&[0], 32);
  assert!(result.is_err());

  let result = aeordb::engine::version_manager::ForkInfo::deserialize(&[], 32);
  assert!(result.is_err());
}

#[test]
fn test_snapshot_with_empty_metadata() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let snapshot = vm.create_snapshot("no-meta", HashMap::new()).unwrap();
  assert!(snapshot.metadata.is_empty());

  let listed = vm.list_snapshots().unwrap();
  assert_eq!(listed.len(), 1);
  assert!(listed[0].metadata.is_empty());
}

#[test]
fn test_multiple_forks_independent_hashes() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let fork_a = vm.create_fork("fork-a", None).unwrap();
  let fork_b = vm.create_fork("fork-b", None).unwrap();

  // Both start from HEAD so same initial hash
  assert_eq!(fork_a.root_hash, fork_b.root_hash);

  // Update fork-a only
  let new_hash = engine.compute_hash(b"fork-a-data").unwrap();
  vm.update_fork_hash("fork-a", &new_hash).unwrap();

  // fork-a changed, fork-b unchanged
  let hash_a = vm.get_fork_hash("fork-a").unwrap().unwrap();
  let hash_b = vm.get_fork_hash("fork-b").unwrap().unwrap();
  assert_eq!(hash_a, new_hash);
  assert_ne!(hash_a, hash_b);
}

#[test]
fn test_resolve_prefers_fork_over_snapshot_with_same_name() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  // Create a snapshot named "shared"
  let _snapshot = vm.create_snapshot("shared", HashMap::new()).unwrap();

  // Create a fork named "shared" — fork key uses a different hash prefix
  // so no collision in KV store
  vm.create_fork("shared", None).unwrap();

  // Update fork's hash to something distinct
  let fork_root = engine.compute_hash(b"fork-wins").unwrap();
  vm.update_fork_hash("shared", &fork_root).unwrap();

  // resolve_root_hash should prefer fork
  let resolved = vm.resolve_root_hash(Some("shared")).unwrap();
  assert_eq!(resolved, fork_root);
}
