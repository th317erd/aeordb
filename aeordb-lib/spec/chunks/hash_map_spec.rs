use std::sync::Arc;

use aeordb::storage::{
  ChunkConfig, HashMapStore, InMemoryChunkStorage,
  ChunkStorage,
};

fn make_hash_map_store(chunk_size: usize) -> HashMapStore {
  let config = ChunkConfig::new(chunk_size).unwrap();
  let storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
  HashMapStore::new(storage, config)
}

// ---------------------------------------------------------------------------
// Basic hash map tests
// ---------------------------------------------------------------------------

#[test]
fn test_store_data_creates_hash_map() {
  let store = make_hash_map_store(64);
  let data = b"hash map creation test";
  let map = store.store_data(data).unwrap();

  assert!(!map.chunk_hashes.is_empty());
  assert_eq!(map.total_size, data.len() as u64);
}

#[test]
fn test_hash_map_resolves_to_original_data() {
  let store = make_hash_map_store(64);
  let data = b"resolve me back to the original bytes";
  let map = store.store_data(data).unwrap();
  let retrieved = store.retrieve_data(&map).unwrap();
  assert_eq!(retrieved, data);
}

#[test]
fn test_hash_map_hash_is_deterministic() {
  let store = make_hash_map_store(64);
  let data = b"deterministic map hash";

  let map_a = store.store_data(data).unwrap();
  let map_b = store.store_data(data).unwrap();

  // Same data produces same chunk hashes, therefore same map hash.
  assert_eq!(map_a.hash, map_b.hash);
  assert_eq!(map_a.chunk_hashes, map_b.chunk_hashes);
}

// ---------------------------------------------------------------------------
// Update / diff tests
// ---------------------------------------------------------------------------

#[test]
fn test_update_data_reuses_unchanged_chunks() {
  let config = ChunkConfig::new(64).unwrap();
  let storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
  let store = HashMapStore::new(storage.clone(), config);

  // Store 128 bytes (2 chunks of 64) with distinct content per chunk.
  let mut data_a = vec![0xAAu8; 128];
  data_a[0] = 0x01; // make first chunk distinct from second
  let map_a = store.store_data(&data_a).unwrap();
  assert_eq!(map_a.chunk_hashes.len(), 2);

  let initial_count = storage.chunk_count().unwrap();
  assert_eq!(initial_count, 2);

  // Update: change only the second half.
  let mut data_b = data_a.clone();
  data_b[64..].fill(0xBB);
  let map_b = store.update_data(&map_a, &data_b).unwrap();

  // First chunk hash should be unchanged.
  assert_eq!(map_a.chunk_hashes[0], map_b.chunk_hashes[0]);
  // Second chunk hash should differ.
  assert_ne!(map_a.chunk_hashes[1], map_b.chunk_hashes[1]);

  // Only one new chunk should have been stored (the changed one).
  let final_count = storage.chunk_count().unwrap();
  assert_eq!(final_count, 3); // 2 original + 1 new
}

#[test]
fn test_partial_update_creates_minimal_new_chunks() {
  let config = ChunkConfig::new(32).unwrap();
  let storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
  let store = HashMapStore::new(storage.clone(), config);

  // 160 bytes = 5 chunks of 32.
  let data_a = vec![0xAAu8; 160];
  let map_a = store.store_data(&data_a).unwrap();
  assert_eq!(map_a.chunk_hashes.len(), 5);
  assert_eq!(storage.chunk_count().unwrap(), 1); // all chunks identical (0xAA repeated)

  // Change only 1 byte in the middle (affects one chunk).
  let mut data_b = data_a.clone();
  data_b[50] = 0xBB; // byte 50 is in chunk index 1 (bytes 32-63)
  let map_b = store.update_data(&map_a, &data_b).unwrap();

  // Only the changed chunk should be new.
  assert_eq!(map_b.chunk_hashes.len(), 5);
  assert_ne!(map_b.chunk_hashes[1], map_a.chunk_hashes[1]);

  // Chunks 0, 2, 3, 4 should be unchanged.
  assert_eq!(map_b.chunk_hashes[0], map_a.chunk_hashes[0]);
  assert_eq!(map_b.chunk_hashes[2], map_a.chunk_hashes[2]);
  assert_eq!(map_b.chunk_hashes[3], map_a.chunk_hashes[3]);
  assert_eq!(map_b.chunk_hashes[4], map_a.chunk_hashes[4]);
}

#[test]
fn test_diff_shows_added_and_removed_chunks() {
  let store = make_hash_map_store(64);

  // Use distinct data per chunk to avoid dedup conflation.
  let mut data_a = vec![0xAAu8; 128];
  data_a[0] = 0x01; // make first chunk different from second
  let map_a = store.store_data(&data_a).unwrap();
  assert_eq!(map_a.chunk_hashes.len(), 2);

  let mut data_b = data_a.clone();
  data_b[64..].fill(0xBB);
  let map_b = store.store_data(&data_b).unwrap();
  assert_eq!(map_b.chunk_hashes.len(), 2);

  let diff = HashMapStore::diff_hash_maps(&map_a, &map_b);

  // First chunk is the same in both.
  assert_eq!(diff.unchanged.len(), 1);
  assert_eq!(diff.unchanged[0], map_a.chunk_hashes[0]);

  // Second chunk differs.
  assert_eq!(diff.added.len(), 1);
  assert_eq!(diff.removed.len(), 1);
  assert_eq!(diff.removed[0], map_a.chunk_hashes[1]);
  assert_eq!(diff.added[0], map_b.chunk_hashes[1]);
}

#[test]
fn test_diff_identical_maps_shows_no_changes() {
  let store = make_hash_map_store(64);

  let data = b"identical data for both maps";
  let map_a = store.store_data(data).unwrap();
  let map_b = store.store_data(data).unwrap();

  let diff = HashMapStore::diff_hash_maps(&map_a, &map_b);
  assert!(diff.added.is_empty());
  assert!(diff.removed.is_empty());
  assert_eq!(diff.unchanged.len(), map_a.chunk_hashes.len());
}

// ---------------------------------------------------------------------------
// Serialize / deserialize hash map
// ---------------------------------------------------------------------------

#[test]
fn test_store_and_load_hash_map() {
  let config = ChunkConfig::new(64).unwrap();
  let storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
  let store = HashMapStore::new(storage, config);

  let data = b"serialize and deserialize this hash map";
  let original_map = store.store_data(data).unwrap();

  // Store the hash map itself as a chunk.
  let map_hash = store.store_hash_map_as_chunk(&original_map).unwrap();

  // Load it back.
  let loaded_map = store.load_hash_map(&map_hash).unwrap();

  assert_eq!(loaded_map.hash, original_map.hash);
  assert_eq!(loaded_map.chunk_hashes, original_map.chunk_hashes);
  assert_eq!(loaded_map.total_size, original_map.total_size);

  // The data should still be retrievable through the loaded map.
  let retrieved = store.retrieve_data(&loaded_map).unwrap();
  assert_eq!(retrieved, data);
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[test]
fn test_hash_map_of_empty_data() {
  let store = make_hash_map_store(64);
  let map = store.store_data(b"").unwrap();

  assert!(map.chunk_hashes.is_empty());
  assert_eq!(map.total_size, 0);

  let retrieved = store.retrieve_data(&map).unwrap();
  assert!(retrieved.is_empty());

  // Hash of empty map should be deterministic.
  let map_b = store.store_data(b"").unwrap();
  assert_eq!(map.hash, map_b.hash);
}

#[test]
fn test_large_data_hash_map() {
  let config = ChunkConfig::new(1024).unwrap();
  let storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
  let store = HashMapStore::new(storage.clone(), config);

  // 1MB+ of varied data.
  let data: Vec<u8> = (0..=255u8).cycle().take(1_048_576 + 123).collect();
  let map = store.store_data(&data).unwrap();

  // Should have ceil((1048576 + 123) / 1024) = 1025 chunks.
  assert_eq!(map.chunk_hashes.len(), 1025);
  assert_eq!(map.total_size, 1_048_576 + 123);

  let retrieved = store.retrieve_data(&map).unwrap();
  assert_eq!(retrieved.len(), data.len());
  assert_eq!(retrieved, data);
}

// ---------------------------------------------------------------------------
// Diff edge cases
// ---------------------------------------------------------------------------

#[test]
fn test_diff_empty_to_nonempty() {
  let store = make_hash_map_store(64);

  let empty_map = store.store_data(b"").unwrap();
  let data_map = store.store_data(b"some data here").unwrap();

  let diff = HashMapStore::diff_hash_maps(&empty_map, &data_map);
  assert_eq!(diff.added.len(), data_map.chunk_hashes.len());
  assert!(diff.removed.is_empty());
  assert!(diff.unchanged.is_empty());
}

#[test]
fn test_diff_nonempty_to_empty() {
  let store = make_hash_map_store(64);

  let data_map = store.store_data(b"some data here").unwrap();
  let empty_map = store.store_data(b"").unwrap();

  let diff = HashMapStore::diff_hash_maps(&data_map, &empty_map);
  assert!(diff.added.is_empty());
  assert_eq!(diff.removed.len(), data_map.chunk_hashes.len());
  assert!(diff.unchanged.is_empty());
}

// ---------------------------------------------------------------------------
// Update edge cases
// ---------------------------------------------------------------------------

#[test]
fn test_update_to_empty() {
  let config = ChunkConfig::new(64).unwrap();
  let storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
  let store = HashMapStore::new(storage, config);

  let data = b"not empty";
  let map = store.store_data(data).unwrap();

  let updated = store.update_data(&map, b"").unwrap();
  assert!(updated.chunk_hashes.is_empty());
  assert_eq!(updated.total_size, 0);
}

#[test]
fn test_update_from_empty() {
  let config = ChunkConfig::new(64).unwrap();
  let storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
  let store = HashMapStore::new(storage, config);

  let empty_map = store.store_data(b"").unwrap();
  let updated = store.update_data(&empty_map, b"now has data").unwrap();

  assert!(!updated.chunk_hashes.is_empty());
  assert_eq!(updated.total_size, 12);

  let retrieved = store.retrieve_data(&updated).unwrap();
  assert_eq!(retrieved, b"now has data");
}

// ---------------------------------------------------------------------------
// Hash map serialization edge cases
// ---------------------------------------------------------------------------

#[test]
fn test_store_and_load_empty_hash_map() {
  let config = ChunkConfig::new(64).unwrap();
  let storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
  let store = HashMapStore::new(storage, config);

  let empty_map = store.store_data(b"").unwrap();
  let map_hash = store.store_hash_map_as_chunk(&empty_map).unwrap();
  let loaded = store.load_hash_map(&map_hash).unwrap();

  assert_eq!(loaded.hash, empty_map.hash);
  assert!(loaded.chunk_hashes.is_empty());
  assert_eq!(loaded.total_size, 0);
}

#[test]
fn test_load_nonexistent_hash_map_returns_error() {
  let config = ChunkConfig::new(64).unwrap();
  let storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
  let store = HashMapStore::new(storage, config);

  let fake_hash = [0xFFu8; 32];
  let result = store.load_hash_map(&fake_hash);
  assert!(result.is_err());
}
