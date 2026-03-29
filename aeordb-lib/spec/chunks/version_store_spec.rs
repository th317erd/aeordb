use std::collections::HashMap;
use std::sync::Arc;

use aeordb::storage::{
  ChunkConfig, ChunkStorage, ContentHashMap, HashMapStore,
  InMemoryChunkStorage, InMemoryVersionStorage, VersionStore,
};

/// Build a VersionStore backed by in-memory storage with a small chunk size.
fn make_version_store() -> (VersionStore, Arc<dyn ChunkStorage>) {
  let chunk_storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
  let version_storage = Arc::new(InMemoryVersionStorage::new());
  let store = VersionStore::new(chunk_storage.clone(), version_storage);
  (store, chunk_storage)
}

/// Helper: store data into chunks and return the ContentHashMap.
fn store_data(chunk_storage: &Arc<dyn ChunkStorage>, data: &[u8]) -> ContentHashMap {
  let config = ChunkConfig::new(64).unwrap();
  let hash_map_store = HashMapStore::new(chunk_storage.clone(), config);
  hash_map_store.store_data(data).unwrap()
}

// ---------------------------------------------------------------------------
// Basic creation
// ---------------------------------------------------------------------------

#[test]
fn test_create_version() {
  let (version_store, chunk_storage) = make_version_store();
  let map = store_data(&chunk_storage, b"version one data");

  let version = version_store
    .create_version(&map, Some("v1".to_string()), HashMap::new())
    .unwrap();

  assert_eq!(version.name.as_deref(), Some("v1"));
  assert!(version.parent_version_id.is_none());
}

#[test]
fn test_create_version_sets_parent_to_previous_latest() {
  let (version_store, chunk_storage) = make_version_store();
  let map_a = store_data(&chunk_storage, b"first version");
  let map_b = store_data(&chunk_storage, b"second version");

  let version_a = version_store
    .create_version(&map_a, Some("v1".to_string()), HashMap::new())
    .unwrap();
  let version_b = version_store
    .create_version(&map_b, Some("v2".to_string()), HashMap::new())
    .unwrap();

  assert!(version_a.parent_version_id.is_none());
  assert_eq!(version_b.parent_version_id, Some(version_a.version_id));
}

// ---------------------------------------------------------------------------
// Retrieval
// ---------------------------------------------------------------------------

#[test]
fn test_get_version_by_id() {
  let (version_store, chunk_storage) = make_version_store();
  let map = store_data(&chunk_storage, b"get by id");

  let created = version_store
    .create_version(&map, Some("tagged".to_string()), HashMap::new())
    .unwrap();

  let fetched = version_store
    .get_version(&created.version_id)
    .unwrap()
    .expect("version should exist");

  assert_eq!(fetched.version_id, created.version_id);
  assert_eq!(fetched.name.as_deref(), Some("tagged"));
}

#[test]
fn test_get_version_by_name() {
  let (version_store, chunk_storage) = make_version_store();
  let map = store_data(&chunk_storage, b"named version");

  let created = version_store
    .create_version(&map, Some("release-1.0".to_string()), HashMap::new())
    .unwrap();

  let fetched = version_store
    .get_version_by_name("release-1.0")
    .unwrap()
    .expect("version should be found by name");

  assert_eq!(fetched.version_id, created.version_id);
}

#[test]
fn test_get_version_returns_none_for_missing() {
  let (version_store, _chunk_storage) = make_version_store();
  let missing_id = uuid::Uuid::new_v4();

  let result = version_store.get_version(&missing_id).unwrap();
  assert!(result.is_none());
}

#[test]
fn test_get_latest_version() {
  let (version_store, chunk_storage) = make_version_store();
  let map_a = store_data(&chunk_storage, b"first");
  let map_b = store_data(&chunk_storage, b"second");

  let _version_a = version_store
    .create_version(&map_a, Some("v1".to_string()), HashMap::new())
    .unwrap();
  let version_b = version_store
    .create_version(&map_b, Some("v2".to_string()), HashMap::new())
    .unwrap();

  let latest = version_store
    .get_latest_version()
    .unwrap()
    .expect("should have a latest version");

  assert_eq!(latest.version_id, version_b.version_id);
}

#[test]
fn test_get_latest_version_returns_none_when_empty() {
  let (version_store, _chunk_storage) = make_version_store();

  let latest = version_store.get_latest_version().unwrap();
  assert!(latest.is_none());
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

#[test]
fn test_list_versions_ordered_by_created_at() {
  let (version_store, chunk_storage) = make_version_store();
  let map_a = store_data(&chunk_storage, b"alpha");
  let map_b = store_data(&chunk_storage, b"beta");
  let map_c = store_data(&chunk_storage, b"gamma");

  let version_a = version_store
    .create_version(&map_a, Some("a".to_string()), HashMap::new())
    .unwrap();
  let version_b = version_store
    .create_version(&map_b, Some("b".to_string()), HashMap::new())
    .unwrap();
  let version_c = version_store
    .create_version(&map_c, Some("c".to_string()), HashMap::new())
    .unwrap();

  let versions = version_store.list_versions().unwrap();
  assert_eq!(versions.len(), 3);

  // Descending order: most recent first.
  assert_eq!(versions[0].version_id, version_c.version_id);
  assert_eq!(versions[1].version_id, version_b.version_id);
  assert_eq!(versions[2].version_id, version_a.version_id);
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

#[test]
fn test_restore_version_returns_hash_map() {
  let (version_store, chunk_storage) = make_version_store();
  let data = b"restorable data";
  let map = store_data(&chunk_storage, data);

  let version = version_store
    .create_version(&map, None, HashMap::new())
    .unwrap();

  let restored = version_store
    .restore_version(&version.version_id)
    .unwrap();

  assert_eq!(restored.chunk_hashes, map.chunk_hashes);
  assert_eq!(restored.total_size, map.total_size);
  assert_eq!(restored.hash, map.hash);
}

#[test]
fn test_restore_version_data_matches_original() {
  let (version_store, chunk_storage) = make_version_store();
  let data = b"the original payload bytes";
  let map = store_data(&chunk_storage, data);

  let version = version_store
    .create_version(&map, None, HashMap::new())
    .unwrap();

  let restored_map = version_store
    .restore_version(&version.version_id)
    .unwrap();

  // Retrieve the actual data through the restored map.
  let config = ChunkConfig::new(64).unwrap();
  let hash_map_store = HashMapStore::new(chunk_storage.clone(), config);
  let retrieved = hash_map_store.retrieve_data(&restored_map).unwrap();
  assert_eq!(retrieved, data);
}

// ---------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------

#[test]
fn test_diff_versions_shows_changes() {
  let (version_store, chunk_storage) = make_version_store();

  // With chunk_size=64 and header=33, data_capacity=31.
  // Create 2 chunks of distinct content (62 bytes total).
  let config = ChunkConfig::new(64).unwrap();
  let data_capacity = config.data_capacity(); // 31
  let total = data_capacity * 2; // 62
  let mut data_a = vec![0xAAu8; total];
  data_a[0] = 0x01; // make first chunk distinct from second
  let map_a = store_data(&chunk_storage, &data_a);
  assert_eq!(map_a.chunk_hashes.len(), 2);

  // Change only the second chunk.
  let mut data_b = data_a.clone();
  data_b[data_capacity..].fill(0xBB);
  let map_b = store_data(&chunk_storage, &data_b);
  assert_eq!(map_b.chunk_hashes.len(), 2);

  let version_a = version_store
    .create_version(&map_a, Some("a".to_string()), HashMap::new())
    .unwrap();
  let version_b = version_store
    .create_version(&map_b, Some("b".to_string()), HashMap::new())
    .unwrap();

  let diff = version_store
    .diff_versions(&version_a.version_id, &version_b.version_id)
    .unwrap();

  assert_eq!(diff.chunks_unchanged.len(), 1);
  assert_eq!(diff.chunks_added.len(), 1);
  assert_eq!(diff.chunks_removed.len(), 1);
  assert!(diff.data_added_bytes > 0);
  assert!(diff.data_removed_bytes > 0);
}

#[test]
fn test_diff_identical_versions_shows_no_changes() {
  let (version_store, chunk_storage) = make_version_store();
  let data = b"identical across versions";
  let map = store_data(&chunk_storage, data);

  let version_a = version_store
    .create_version(&map, Some("same-a".to_string()), HashMap::new())
    .unwrap();
  let version_b = version_store
    .create_version(&map, Some("same-b".to_string()), HashMap::new())
    .unwrap();

  let diff = version_store
    .diff_versions(&version_a.version_id, &version_b.version_id)
    .unwrap();

  assert!(diff.chunks_added.is_empty());
  assert!(diff.chunks_removed.is_empty());
  assert_eq!(diff.chunks_unchanged.len(), map.chunk_hashes.len());
  assert_eq!(diff.data_added_bytes, 0);
  assert_eq!(diff.data_removed_bytes, 0);
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

#[test]
fn test_delete_version() {
  let (version_store, chunk_storage) = make_version_store();
  let map = store_data(&chunk_storage, b"deletable");

  let version = version_store
    .create_version(&map, None, HashMap::new())
    .unwrap();

  version_store.delete_version(&version.version_id).unwrap();

  let fetched = version_store.get_version(&version.version_id).unwrap();
  assert!(fetched.is_none());
}

#[test]
fn test_delete_version_does_not_delete_chunks() {
  let (version_store, chunk_storage) = make_version_store();
  let data = b"chunks should survive version deletion";
  let map = store_data(&chunk_storage, data);

  let chunk_count_before = chunk_storage.chunk_count().unwrap();
  assert!(chunk_count_before > 0);

  let version = version_store
    .create_version(&map, None, HashMap::new())
    .unwrap();

  version_store.delete_version(&version.version_id).unwrap();

  // Chunks should still be present.
  let chunk_count_after = chunk_storage.chunk_count().unwrap();
  assert!(chunk_count_after >= chunk_count_before);

  // Data should still be retrievable via the original hash map.
  let config = ChunkConfig::new(64).unwrap();
  let hash_map_store = HashMapStore::new(chunk_storage.clone(), config);
  let retrieved = hash_map_store.retrieve_data(&map).unwrap();
  assert_eq!(retrieved, data);
}

// ---------------------------------------------------------------------------
// Tagging
// ---------------------------------------------------------------------------

#[test]
fn test_tag_version() {
  let (version_store, chunk_storage) = make_version_store();
  let map = store_data(&chunk_storage, b"taggable");

  let version = version_store
    .create_version(&map, None, HashMap::new())
    .unwrap();

  assert!(version.name.is_none());

  version_store
    .tag_version(&version.version_id, "release-2.0")
    .unwrap();

  let fetched = version_store
    .get_version_by_name("release-2.0")
    .unwrap()
    .expect("version should be findable by new tag");

  assert_eq!(fetched.version_id, version.version_id);
}

#[test]
fn test_tag_version_overwrites_existing_tag() {
  let (version_store, chunk_storage) = make_version_store();
  let map_a = store_data(&chunk_storage, b"first tagged");
  let map_b = store_data(&chunk_storage, b"second tagged");

  let version_a = version_store
    .create_version(&map_a, Some("latest".to_string()), HashMap::new())
    .unwrap();
  let version_b = version_store
    .create_version(&map_b, None, HashMap::new())
    .unwrap();

  // Move the "latest" tag to version_b.
  version_store
    .tag_version(&version_b.version_id, "latest")
    .unwrap();

  let fetched = version_store
    .get_version_by_name("latest")
    .unwrap()
    .expect("tag should resolve");

  assert_eq!(fetched.version_id, version_b.version_id);

  // version_a should no longer have the "latest" name.
  let fetched_a = version_store
    .get_version(&version_a.version_id)
    .unwrap()
    .expect("version a should still exist");
  assert!(fetched_a.name.is_none());
}

// ---------------------------------------------------------------------------
// History chain
// ---------------------------------------------------------------------------

#[test]
fn test_version_history_chain() {
  let (version_store, chunk_storage) = make_version_store();
  let map_a = store_data(&chunk_storage, b"chain-1");
  let map_b = store_data(&chunk_storage, b"chain-2");
  let map_c = store_data(&chunk_storage, b"chain-3");

  let version_a = version_store
    .create_version(&map_a, Some("v1".to_string()), HashMap::new())
    .unwrap();
  let version_b = version_store
    .create_version(&map_b, Some("v2".to_string()), HashMap::new())
    .unwrap();
  let version_c = version_store
    .create_version(&map_c, Some("v3".to_string()), HashMap::new())
    .unwrap();

  // Verify parent chain: c -> b -> a -> None.
  assert!(version_a.parent_version_id.is_none());
  assert_eq!(version_b.parent_version_id, Some(version_a.version_id));
  assert_eq!(version_c.parent_version_id, Some(version_b.version_id));
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

#[test]
fn test_version_metadata_stored() {
  let (version_store, chunk_storage) = make_version_store();
  let map = store_data(&chunk_storage, b"metadata test");

  let mut metadata = HashMap::new();
  metadata.insert("author".to_string(), "test-user".to_string());
  metadata.insert("message".to_string(), "initial commit".to_string());

  let version = version_store
    .create_version(&map, None, metadata.clone())
    .unwrap();

  let fetched = version_store
    .get_version(&version.version_id)
    .unwrap()
    .expect("version should exist");

  assert_eq!(fetched.metadata.get("author").unwrap(), "test-user");
  assert_eq!(fetched.metadata.get("message").unwrap(), "initial commit");
  assert_eq!(fetched.metadata.len(), 2);
}

// ---------------------------------------------------------------------------
// Restore after multiple updates
// ---------------------------------------------------------------------------

#[test]
fn test_restore_old_version_after_multiple_updates() {
  let (version_store, chunk_storage) = make_version_store();

  let data_v1 = b"original data for version 1";
  let data_v2 = b"updated data for version 2";
  let data_v3 = b"latest data for version 3";

  let map_v1 = store_data(&chunk_storage, data_v1);
  let map_v2 = store_data(&chunk_storage, data_v2);
  let map_v3 = store_data(&chunk_storage, data_v3);

  let version_1 = version_store
    .create_version(&map_v1, Some("v1".to_string()), HashMap::new())
    .unwrap();
  let _version_2 = version_store
    .create_version(&map_v2, Some("v2".to_string()), HashMap::new())
    .unwrap();
  let _version_3 = version_store
    .create_version(&map_v3, Some("v3".to_string()), HashMap::new())
    .unwrap();

  // Restore the original version.
  let restored_map = version_store
    .restore_version(&version_1.version_id)
    .unwrap();

  let config = ChunkConfig::new(64).unwrap();
  let hash_map_store = HashMapStore::new(chunk_storage.clone(), config);
  let retrieved = hash_map_store.retrieve_data(&restored_map).unwrap();
  assert_eq!(retrieved, data_v1);
}

// ---------------------------------------------------------------------------
// Error / edge cases
// ---------------------------------------------------------------------------

#[test]
fn test_restore_nonexistent_version_returns_error() {
  let (version_store, _chunk_storage) = make_version_store();
  let missing_id = uuid::Uuid::new_v4();

  let result = version_store.restore_version(&missing_id);
  assert!(result.is_err());
}

#[test]
fn test_delete_nonexistent_version_returns_error() {
  let (version_store, _chunk_storage) = make_version_store();
  let missing_id = uuid::Uuid::new_v4();

  let result = version_store.delete_version(&missing_id);
  assert!(result.is_err());
}

#[test]
fn test_diff_with_nonexistent_version_returns_error() {
  let (version_store, chunk_storage) = make_version_store();
  let map = store_data(&chunk_storage, b"exists");
  let version = version_store
    .create_version(&map, None, HashMap::new())
    .unwrap();

  let missing_id = uuid::Uuid::new_v4();
  let result = version_store.diff_versions(&version.version_id, &missing_id);
  assert!(result.is_err());
}

#[test]
fn test_tag_nonexistent_version_returns_error() {
  let (version_store, _chunk_storage) = make_version_store();
  let missing_id = uuid::Uuid::new_v4();

  let result = version_store.tag_version(&missing_id, "nope");
  assert!(result.is_err());
}

#[test]
fn test_get_version_by_name_returns_none_for_missing_name() {
  let (version_store, _chunk_storage) = make_version_store();

  let result = version_store.get_version_by_name("nonexistent").unwrap();
  assert!(result.is_none());
}

#[test]
fn test_create_version_with_empty_data() {
  let (version_store, chunk_storage) = make_version_store();
  let map = store_data(&chunk_storage, b"");

  let version = version_store
    .create_version(&map, Some("empty".to_string()), HashMap::new())
    .unwrap();

  let restored = version_store
    .restore_version(&version.version_id)
    .unwrap();

  assert!(restored.chunk_hashes.is_empty());
  assert_eq!(restored.total_size, 0);
}

#[test]
fn test_multiple_versions_same_data_share_chunks() {
  let (version_store, chunk_storage) = make_version_store();
  let data = b"shared content across versions";
  let map = store_data(&chunk_storage, data);

  let chunk_count_before = chunk_storage.chunk_count().unwrap();

  let _version_a = version_store
    .create_version(&map, Some("copy-a".to_string()), HashMap::new())
    .unwrap();
  let _version_b = version_store
    .create_version(&map, Some("copy-b".to_string()), HashMap::new())
    .unwrap();

  // No new data chunks should have been created (content-addressed dedup).
  // Only the serialized hash map chunk is stored once (same hash map = same chunk).
  let chunk_count_after = chunk_storage.chunk_count().unwrap();
  // The serialized hash map chunk is stored once, so at most 1 extra chunk.
  assert!(chunk_count_after <= chunk_count_before + 1);
}

#[test]
fn test_list_versions_empty() {
  let (version_store, _chunk_storage) = make_version_store();

  let versions = version_store.list_versions().unwrap();
  assert!(versions.is_empty());
}

#[test]
fn test_delete_version_then_list_excludes_deleted() {
  let (version_store, chunk_storage) = make_version_store();
  let map_a = store_data(&chunk_storage, b"keep me");
  let map_b = store_data(&chunk_storage, b"delete me");

  let version_a = version_store
    .create_version(&map_a, Some("keep".to_string()), HashMap::new())
    .unwrap();
  let version_b = version_store
    .create_version(&map_b, Some("delete".to_string()), HashMap::new())
    .unwrap();

  version_store.delete_version(&version_b.version_id).unwrap();

  let versions = version_store.list_versions().unwrap();
  assert_eq!(versions.len(), 1);
  assert_eq!(versions[0].version_id, version_a.version_id);
}
