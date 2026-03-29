use aeordb::filesystem::{VersionError, VersionManager};
use redb::{Database, ReadableTable, TableDefinition};
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::NamedTempFile;

/// Helper: create a VersionManager backed by a temporary on-disk database.
/// Persistent savepoints require a real file — in-memory backends do not
/// support them.
fn create_test_manager() -> (VersionManager, Arc<Database>, NamedTempFile) {
  let tempfile = NamedTempFile::new().expect("failed to create temp file");
  let database = Arc::new(
    Database::create(tempfile.path()).expect("failed to create database"),
  );
  let manager = VersionManager::new(database.clone());
  (manager, database, tempfile)
}

/// Helper: insert a row into a user-facing table so we can verify restore
/// behavior.
fn insert_test_data(database: &Database, table_name: &str, key: u64, value: &[u8]) {
  let table_definition: TableDefinition<u64, &[u8]> =
    TableDefinition::new(table_name);
  let write_transaction = database.begin_write().unwrap();
  {
    let mut table = write_transaction.open_table(table_definition).unwrap();
    table.insert(key, value).unwrap();
  }
  write_transaction.commit().unwrap();
}

/// Helper: read a row from a user-facing table.
fn read_test_data(database: &Database, table_name: &str, key: u64) -> Option<Vec<u8>> {
  let table_definition: TableDefinition<u64, &[u8]> =
    TableDefinition::new(table_name);
  let read_transaction = database.begin_read().unwrap();
  let table = match read_transaction.open_table(table_definition) {
    Ok(table) => table,
    Err(redb::TableError::TableDoesNotExist(_)) => return None,
    Err(error) => panic!("unexpected table error: {error}"),
  };
  table.get(key).unwrap().map(|guard| guard.value().to_vec())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_create_version() {
  let (manager, _database, _tempfile) = create_test_manager();
  let metadata = HashMap::new();
  let version = manager
    .create_version("v1", metadata)
    .expect("create_version should succeed");
  assert_eq!(version.name, "v1");
}

#[test]
fn test_create_version_has_name_and_timestamp() {
  let (manager, _database, _tempfile) = create_test_manager();
  let before = chrono::Utc::now();
  let version = manager
    .create_version("snapshot-alpha", HashMap::new())
    .expect("create_version should succeed");
  let after = chrono::Utc::now();

  assert_eq!(version.name, "snapshot-alpha");
  assert!(
    version.created_at >= before && version.created_at <= after,
    "created_at should be between before and after"
  );
}

#[test]
fn test_create_version_stores_metadata() {
  let (manager, _database, _tempfile) = create_test_manager();
  let mut metadata = HashMap::new();
  metadata.insert("author".to_string(), "wyatt".to_string());
  metadata.insert("message".to_string(), "initial snapshot".to_string());

  let version = manager
    .create_version("v1", metadata)
    .expect("create_version should succeed");

  assert_eq!(version.metadata.get("author").unwrap(), "wyatt");
  assert_eq!(
    version.metadata.get("message").unwrap(),
    "initial snapshot"
  );
}

#[test]
fn test_get_version_by_name() {
  let (manager, _database, _tempfile) = create_test_manager();
  let created = manager
    .create_version("release-1.0", HashMap::new())
    .expect("create_version should succeed");

  let fetched = manager
    .get_version("release-1.0")
    .expect("get_version should succeed")
    .expect("version should exist");

  assert_eq!(fetched.name, created.name);
  assert_eq!(fetched.savepoint_id, created.savepoint_id);
}

#[test]
fn test_get_version_returns_none_for_missing() {
  let (manager, _database, _tempfile) = create_test_manager();
  let result = manager
    .get_version("nonexistent")
    .expect("get_version should succeed");
  assert!(result.is_none());
}

#[test]
fn test_list_versions_ordered_by_created_at() {
  let (manager, _database, _tempfile) = create_test_manager();

  manager
    .create_version("first", HashMap::new())
    .expect("create first");
  manager
    .create_version("second", HashMap::new())
    .expect("create second");
  manager
    .create_version("third", HashMap::new())
    .expect("create third");

  let versions = manager
    .list_versions()
    .expect("list_versions should succeed");

  assert_eq!(versions.len(), 3);
  assert_eq!(versions[0].name, "first");
  assert_eq!(versions[1].name, "second");
  assert_eq!(versions[2].name, "third");

  // Verify ordering invariant.
  assert!(versions[0].created_at <= versions[1].created_at);
  assert!(versions[1].created_at <= versions[2].created_at);
}

#[test]
fn test_list_versions_empty() {
  let (manager, _database, _tempfile) = create_test_manager();
  let versions = manager
    .list_versions()
    .expect("list_versions should succeed");
  assert!(versions.is_empty());
}

#[test]
fn test_delete_version() {
  let (manager, _database, _tempfile) = create_test_manager();
  manager
    .create_version("doomed", HashMap::new())
    .expect("create_version should succeed");

  manager
    .delete_version("doomed")
    .expect("delete_version should succeed");

  let result = manager
    .get_version("doomed")
    .expect("get_version should succeed");
  assert!(result.is_none());
}

#[test]
fn test_delete_nonexistent_version_returns_error() {
  let (manager, _database, _tempfile) = create_test_manager();
  let result = manager.delete_version("ghost");
  assert!(result.is_err());
  match result.unwrap_err() {
    VersionError::VersionNotFound(name) => assert_eq!(name, "ghost"),
    other => panic!("expected VersionNotFound, got: {other}"),
  }
}

#[test]
fn test_duplicate_version_name_returns_error() {
  let (manager, _database, _tempfile) = create_test_manager();
  manager
    .create_version("v1", HashMap::new())
    .expect("first create should succeed");

  let result = manager.create_version("v1", HashMap::new());
  assert!(result.is_err());
  match result.unwrap_err() {
    VersionError::VersionAlreadyExists(name) => assert_eq!(name, "v1"),
    other => panic!("expected VersionAlreadyExists, got: {other}"),
  }
}

#[test]
fn test_latest_version() {
  let (manager, _database, _tempfile) = create_test_manager();
  manager
    .create_version("old", HashMap::new())
    .expect("create old");
  manager
    .create_version("new", HashMap::new())
    .expect("create new");

  let latest = manager
    .latest_version()
    .expect("latest_version should succeed")
    .expect("should have a latest version");
  assert_eq!(latest.name, "new");
}

#[test]
fn test_latest_version_returns_none_when_empty() {
  let (manager, _database, _tempfile) = create_test_manager();
  let result = manager
    .latest_version()
    .expect("latest_version should succeed");
  assert!(result.is_none());
}

#[test]
fn test_restore_version_rolls_back_state() {
  let (manager, database, _tempfile) = create_test_manager();

  // Insert initial data.
  insert_test_data(&database, "users", 1, b"alice");

  // Create a version capturing this state.
  manager
    .create_version("before-bob", HashMap::new())
    .expect("create_version should succeed");

  // Insert more data AFTER the version.
  insert_test_data(&database, "users", 2, b"bob");

  // Verify bob exists before restore.
  assert!(read_test_data(&database, "users", 2).is_some());

  // Restore to the version.
  manager
    .restore_version("before-bob")
    .expect("restore_version should succeed");

  // Alice should still be there (she was present at savepoint time).
  assert_eq!(
    read_test_data(&database, "users", 1),
    Some(b"alice".to_vec())
  );

  // Bob should be gone (added after the savepoint).
  assert!(read_test_data(&database, "users", 2).is_none());
}

#[test]
fn test_restore_preserves_restored_version_metadata() {
  let (manager, database, _tempfile) = create_test_manager();

  insert_test_data(&database, "data", 1, b"original");

  let mut metadata = HashMap::new();
  metadata.insert("note".to_string(), "important".to_string());
  manager
    .create_version("checkpoint", metadata)
    .expect("create_version should succeed");

  // Mutate data after the version.
  insert_test_data(&database, "data", 2, b"extra");

  // Restore.
  manager
    .restore_version("checkpoint")
    .expect("restore should succeed");

  // The restored version's own metadata should still be available because
  // restore_version re-inserts it.
  let version = manager
    .get_version("checkpoint")
    .expect("get_version should succeed")
    .expect("version should exist after restore");
  assert_eq!(version.name, "checkpoint");
  assert_eq!(version.metadata.get("note").unwrap(), "important");
}

#[test]
fn test_restore_removes_later_version_metadata() {
  let (manager, database, _tempfile) = create_test_manager();

  insert_test_data(&database, "data", 1, b"initial");

  manager
    .create_version("v1", HashMap::new())
    .expect("create v1");

  insert_test_data(&database, "data", 2, b"added-after-v1");

  manager
    .create_version("v2", HashMap::new())
    .expect("create v2");

  // Restore to v1 — v2's metadata should be gone.
  manager
    .restore_version("v1")
    .expect("restore should succeed");

  let versions = manager
    .list_versions()
    .expect("list_versions should succeed");

  // Only v1 should remain (v2 was created after the v1 savepoint).
  assert_eq!(versions.len(), 1);
  assert_eq!(versions[0].name, "v1");

  // v2 should not be retrievable.
  assert!(manager.get_version("v2").expect("should not error").is_none());
}

#[test]
fn test_multiple_versions_and_restore() {
  let (manager, database, _tempfile) = create_test_manager();

  // State 1: just alice.
  insert_test_data(&database, "users", 1, b"alice");
  manager
    .create_version("v1", HashMap::new())
    .expect("create v1");

  // State 2: alice + bob.
  insert_test_data(&database, "users", 2, b"bob");
  manager
    .create_version("v2", HashMap::new())
    .expect("create v2");

  // State 3: alice + bob + charlie.
  insert_test_data(&database, "users", 3, b"charlie");

  // Restore to v1 — should have only alice.
  manager
    .restore_version("v1")
    .expect("restore to v1 should succeed");

  assert_eq!(
    read_test_data(&database, "users", 1),
    Some(b"alice".to_vec())
  );
  assert!(read_test_data(&database, "users", 2).is_none());
  assert!(read_test_data(&database, "users", 3).is_none());

  // Only v1 should remain in the versions list.
  let versions = manager.list_versions().expect("list_versions");
  assert_eq!(versions.len(), 1);
  assert_eq!(versions[0].name, "v1");
}

#[test]
fn test_restore_nonexistent_version_returns_error() {
  let (manager, _database, _tempfile) = create_test_manager();
  let result = manager.restore_version("nope");
  assert!(result.is_err());
  match result.unwrap_err() {
    VersionError::VersionNotFound(name) => assert_eq!(name, "nope"),
    other => panic!("expected VersionNotFound, got: {other}"),
  }
}

#[test]
fn test_delete_then_create_same_name() {
  let (manager, _database, _tempfile) = create_test_manager();

  manager
    .create_version("recycled", HashMap::new())
    .expect("first create");

  manager
    .delete_version("recycled")
    .expect("delete should succeed");

  // Re-creating with the same name should succeed.
  let version = manager
    .create_version("recycled", HashMap::new())
    .expect("re-create should succeed");
  assert_eq!(version.name, "recycled");
}

#[test]
fn test_create_version_with_empty_name() {
  let (manager, _database, _tempfile) = create_test_manager();
  // Empty string is a valid (if odd) key — no reason to reject it.
  let version = manager
    .create_version("", HashMap::new())
    .expect("empty name should be allowed");
  assert_eq!(version.name, "");
}

#[test]
fn test_version_savepoint_id_is_unique() {
  let (manager, _database, _tempfile) = create_test_manager();
  let v1 = manager
    .create_version("v1", HashMap::new())
    .expect("create v1");
  let v2 = manager
    .create_version("v2", HashMap::new())
    .expect("create v2");

  assert_ne!(
    v1.savepoint_id, v2.savepoint_id,
    "different versions must have different savepoint IDs"
  );
}

#[test]
fn test_list_versions_after_delete() {
  let (manager, _database, _tempfile) = create_test_manager();
  manager.create_version("a", HashMap::new()).expect("create a");
  manager.create_version("b", HashMap::new()).expect("create b");
  manager.create_version("c", HashMap::new()).expect("create c");

  manager.delete_version("b").expect("delete b");

  let versions = manager.list_versions().expect("list");
  assert_eq!(versions.len(), 2);
  let names: Vec<&str> = versions.iter().map(|v| v.name.as_str()).collect();
  assert!(names.contains(&"a"));
  assert!(names.contains(&"c"));
  assert!(!names.contains(&"b"));
}
