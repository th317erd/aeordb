use std::sync::Arc;
use std::thread;

use aeordb::storage::{
  Chunk, ChunkConfig, ChunkHash, ChunkStorage, ChunkStore, ChunkStoreError,
  InMemoryChunkStorage, chunk_hash_from_hex, chunk_hash_to_hex, hash_data,
};

// ---------------------------------------------------------------------------
// Chunk-level tests
// ---------------------------------------------------------------------------

#[test]
fn test_store_and_retrieve_chunk() {
  let store = ChunkStore::new_in_memory();
  let data = b"hello, content-addressed world!";
  let map = store.store(data).unwrap();
  let retrieved = store.retrieve(&map).unwrap();
  assert_eq!(retrieved, data);
}

#[test]
fn test_chunk_hash_is_deterministic() {
  let data = b"deterministic hashing";
  let hash_a = hash_data(data);
  let hash_b = hash_data(data);
  assert_eq!(hash_a, hash_b);

  // Different data produces different hash.
  let hash_c = hash_data(b"different data");
  assert_ne!(hash_a, hash_c);
}

#[test]
fn test_duplicate_chunk_not_stored_twice() {
  let store = ChunkStore::new_in_memory();
  let data = b"duplicate me";

  // Store the same data twice.
  let map_a = store.store(data).unwrap();
  let map_b = store.store(data).unwrap();

  // Both maps should reference the same chunk(s).
  assert_eq!(map_a.chunk_hashes, map_b.chunk_hashes);

  // Total chunk count should be 1, not 2.
  let stats = store.stats().unwrap();
  assert_eq!(stats.total_chunks, 1);
}

#[test]
fn test_chunk_integrity_verified_on_read() {
  let store = ChunkStore::new_in_memory();
  let data = b"verify my integrity";
  let map = store.store(data).unwrap();

  // Verify integrity should return empty list (no corruption).
  let corrupt = store.verify_integrity(&map).unwrap();
  assert!(corrupt.is_empty());
}

#[test]
fn test_corrupt_chunk_detected() {
  // Create a chunk with mismatched hash and data.
  let chunk = Chunk::new(b"original data".to_vec());
  assert!(chunk.verify());

  // Create a chunk with wrong data for its hash.
  let corrupt_chunk = Chunk {
    hash: chunk.hash,
    data: b"tampered data".to_vec(),
  };
  assert!(!corrupt_chunk.verify());
}

// ---------------------------------------------------------------------------
// Data store/retrieve tests
// ---------------------------------------------------------------------------

#[test]
fn test_store_and_retrieve_data() {
  let store = ChunkStore::new_in_memory();
  let data = b"store and retrieve this data";
  let map = store.store(data).unwrap();
  let retrieved = store.retrieve(&map).unwrap();
  assert_eq!(retrieved, data);
}

#[test]
fn test_large_data_split_into_chunks() {
  // Use small chunk size (64 bytes) to force splitting.
  let config = ChunkConfig::new(64).unwrap();
  let store = ChunkStore::new_in_memory_with_config(config);

  // 200 bytes of data should produce ceil(200/64) = 4 chunks.
  let data = vec![0xABu8; 200];
  let map = store.store(&data).unwrap();
  assert_eq!(map.chunk_hashes.len(), 4);
  assert_eq!(map.total_size, 200);

  let retrieved = store.retrieve(&map).unwrap();
  assert_eq!(retrieved, data);
}

#[test]
fn test_data_reconstruction_matches_original() {
  let config = ChunkConfig::new(32).unwrap();
  let store = ChunkStore::new_in_memory_with_config(config);

  // Mixed data pattern.
  let data: Vec<u8> = (0..=255).cycle().take(500).collect();
  let map = store.store(&data).unwrap();
  let retrieved = store.retrieve(&map).unwrap();
  assert_eq!(retrieved, data);
}

#[test]
fn test_empty_data_handled() {
  let store = ChunkStore::new_in_memory();
  let data = b"";
  let map = store.store(data).unwrap();

  assert!(map.chunk_hashes.is_empty());
  assert_eq!(map.total_size, 0);

  let retrieved = store.retrieve(&map).unwrap();
  assert!(retrieved.is_empty());
}

#[test]
fn test_single_byte_data() {
  let store = ChunkStore::new_in_memory();
  let data = b"\x42";
  let map = store.store(data).unwrap();

  assert_eq!(map.chunk_hashes.len(), 1);
  assert_eq!(map.total_size, 1);

  let retrieved = store.retrieve(&map).unwrap();
  assert_eq!(retrieved, data);
}

#[test]
fn test_exact_chunk_size_data() {
  let config = ChunkConfig::new(64).unwrap();
  let store = ChunkStore::new_in_memory_with_config(config);

  let data = vec![0xFFu8; 64];
  let map = store.store(&data).unwrap();

  assert_eq!(map.chunk_hashes.len(), 1);
  assert_eq!(map.total_size, 64);

  let retrieved = store.retrieve(&map).unwrap();
  assert_eq!(retrieved, data);
}

#[test]
fn test_chunk_size_plus_one_data() {
  let config = ChunkConfig::new(64).unwrap();
  let store = ChunkStore::new_in_memory_with_config(config);

  let data = vec![0xFFu8; 65];
  let map = store.store(&data).unwrap();

  // 65 bytes = 1 full chunk (64) + 1 partial chunk (1 byte).
  assert_eq!(map.chunk_hashes.len(), 2);
  assert_eq!(map.total_size, 65);

  let retrieved = store.retrieve(&map).unwrap();
  assert_eq!(retrieved, data);
}

#[test]
fn test_configurable_chunk_size() {
  // Power of two sizes should work.
  for size in [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024] {
    let config = ChunkConfig::new(size).unwrap();
    assert_eq!(config.chunk_size, size);
  }

  // Non-power-of-two should fail.
  assert!(ChunkConfig::new(3).is_err());
  assert!(ChunkConfig::new(5).is_err());
  assert!(ChunkConfig::new(100).is_err());
  assert!(ChunkConfig::new(0).is_err());
}

// ---------------------------------------------------------------------------
// Garbage collection tests
// ---------------------------------------------------------------------------

#[test]
fn test_garbage_collection_removes_unreferenced_chunks() {
  let config = ChunkConfig::new(64).unwrap();
  let store = ChunkStore::new_in_memory_with_config(config);

  // Store two different data blobs.
  let data_a = vec![0xAAu8; 100];
  let data_b = vec![0xBBu8; 100];
  let map_a = store.store(&data_a).unwrap();
  let _map_b = store.store(&data_b).unwrap();

  // Both maps use 2 chunks each (ceil(100/64) = 2), all distinct.
  let initial_stats = store.stats().unwrap();
  assert_eq!(initial_stats.total_chunks, 4);

  // GC keeping only map_a as live.
  let removed = store.garbage_collect(&[map_a.clone()]).unwrap();
  assert_eq!(removed, 2);

  // Verify map_a still works.
  let retrieved = store.retrieve(&map_a).unwrap();
  assert_eq!(retrieved, data_a);

  // map_b's chunks are gone.
  let remaining_stats = store.stats().unwrap();
  assert_eq!(remaining_stats.total_chunks, 2);
}

#[test]
fn test_garbage_collection_preserves_referenced_chunks() {
  let config = ChunkConfig::new(64).unwrap();
  let store = ChunkStore::new_in_memory_with_config(config);

  let data = vec![0xCCu8; 100];
  let map = store.store(&data).unwrap();

  // GC with the map as live should remove nothing.
  let removed = store.garbage_collect(&[map.clone()]).unwrap();
  assert_eq!(removed, 0);

  // Data still retrievable.
  let retrieved = store.retrieve(&map).unwrap();
  assert_eq!(retrieved, data);
}

// ---------------------------------------------------------------------------
// Stats tests
// ---------------------------------------------------------------------------

#[test]
fn test_stats_accurate() {
  let config = ChunkConfig::new(64).unwrap();
  let store = ChunkStore::new_in_memory_with_config(config);

  // Empty store.
  let stats = store.stats().unwrap();
  assert_eq!(stats.total_chunks, 0);
  assert_eq!(stats.total_bytes, 0);
  assert_eq!(stats.chunk_size, 64);

  // Store 100 bytes = 2 chunks (64 + 36).
  let data = vec![0xAAu8; 100];
  store.store(&data).unwrap();

  let stats = store.stats().unwrap();
  assert_eq!(stats.total_chunks, 2);
  assert_eq!(stats.total_bytes, 100);
  assert_eq!(stats.chunk_size, 64);
}

// ---------------------------------------------------------------------------
// Concurrency tests
// ---------------------------------------------------------------------------

#[test]
fn test_concurrent_chunk_reads() {
  let store = Arc::new(ChunkStore::new_in_memory());
  let data = b"concurrent read test data that is interesting";
  let map = store.store(data).unwrap();

  let handles: Vec<_> = (0..10)
    .map(|_| {
      let store = store.clone();
      let map = map.clone();
      thread::spawn(move || store.retrieve(&map).unwrap())
    })
    .collect();

  for handle in handles {
    let retrieved = handle.join().unwrap();
    assert_eq!(retrieved, data);
  }
}

#[test]
fn test_concurrent_chunk_writes() {
  let store = Arc::new(ChunkStore::new_in_memory());

  let handles: Vec<_> = (0..10)
    .map(|index| {
      let store = store.clone();
      thread::spawn(move || {
        let data = vec![index as u8; 100];
        store.store(&data).unwrap()
      })
    })
    .collect();

  let maps: Vec<_> = handles
    .into_iter()
    .map(|handle| handle.join().unwrap())
    .collect();

  // Verify each map's data is correct.
  for (index, map) in maps.iter().enumerate() {
    let retrieved = store.retrieve(map).unwrap();
    assert_eq!(retrieved, vec![index as u8; 100]);
  }
}

// ---------------------------------------------------------------------------
// Integrity verification tests
// ---------------------------------------------------------------------------

#[test]
fn test_verify_integrity_passes_for_valid_data() {
  let store = ChunkStore::new_in_memory();
  let data = b"integrity check passes for valid data";
  let map = store.store(data).unwrap();

  let corrupt = store.verify_integrity(&map).unwrap();
  assert!(corrupt.is_empty());
}

// ---------------------------------------------------------------------------
// Hash hex roundtrip
// ---------------------------------------------------------------------------

#[test]
fn test_chunk_hash_to_hex_roundtrip() {
  let data = b"roundtrip test";
  let hash = hash_data(data);
  let hex_string = chunk_hash_to_hex(&hash);
  let parsed = chunk_hash_from_hex(&hex_string).unwrap();
  assert_eq!(parsed, hash);

  // Invalid hex should fail.
  assert!(chunk_hash_from_hex("not_hex").is_err());

  // Wrong length should fail.
  assert!(chunk_hash_from_hex("aabb").is_err());

  // Empty string should fail.
  assert!(chunk_hash_from_hex("").is_err());
}

// ---------------------------------------------------------------------------
// Retrieve missing chunk
// ---------------------------------------------------------------------------

#[test]
fn test_retrieve_missing_chunk_returns_error() {
  let store = ChunkStore::new_in_memory();
  let data = b"some data";
  let mut map = store.store(data).unwrap();

  // Corrupt the map to reference a nonexistent chunk.
  map.chunk_hashes[0] = [0xFFu8; 32];

  let result = store.retrieve(&map);
  assert!(result.is_err());
  match result.unwrap_err() {
    ChunkStoreError::ChunkNotFound(_) => {}
    other => panic!("expected ChunkNotFound, got: {other}"),
  }
}

// ---------------------------------------------------------------------------
// ChunkConfig edge cases
// ---------------------------------------------------------------------------

#[test]
fn test_chunk_config_default() {
  let config = ChunkConfig::default();
  assert_eq!(config.chunk_size, 262144);
}

#[test]
fn test_chunk_config_index_and_offset() {
  let config = ChunkConfig::new(64).unwrap();

  assert_eq!(config.chunk_index(0), 0);
  assert_eq!(config.chunk_index(63), 0);
  assert_eq!(config.chunk_index(64), 1);
  assert_eq!(config.chunk_index(128), 2);

  assert_eq!(config.offset_within_chunk(0), 0);
  assert_eq!(config.offset_within_chunk(63), 63);
  assert_eq!(config.offset_within_chunk(64), 0);
  assert_eq!(config.offset_within_chunk(65), 1);

  assert!(config.is_chunk_boundary(0));
  assert!(config.is_chunk_boundary(64));
  assert!(config.is_chunk_boundary(128));
  assert!(!config.is_chunk_boundary(1));
  assert!(!config.is_chunk_boundary(63));
}

// ---------------------------------------------------------------------------
// InMemoryChunkStorage direct tests
// ---------------------------------------------------------------------------

#[test]
fn test_in_memory_storage_has_chunk() {
  let storage = InMemoryChunkStorage::new();
  let chunk = Chunk::new(b"test".to_vec());

  assert!(!storage.has_chunk(&chunk.hash).unwrap());
  storage.store_chunk(&chunk).unwrap();
  assert!(storage.has_chunk(&chunk.hash).unwrap());
}

#[test]
fn test_in_memory_storage_remove_chunk() {
  let storage = InMemoryChunkStorage::new();
  let chunk = Chunk::new(b"removable".to_vec());

  storage.store_chunk(&chunk).unwrap();
  assert!(storage.remove_chunk(&chunk.hash).unwrap());
  assert!(!storage.has_chunk(&chunk.hash).unwrap());

  // Removing nonexistent chunk returns false.
  assert!(!storage.remove_chunk(&chunk.hash).unwrap());
}

#[test]
fn test_in_memory_storage_get_nonexistent() {
  let storage = InMemoryChunkStorage::new();
  let hash: ChunkHash = [0u8; 32];
  assert!(storage.get_chunk(&hash).unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Redb-backed chunk store
// ---------------------------------------------------------------------------

#[test]
fn test_redb_backed_store_and_retrieve() {
  let backend = redb::backends::InMemoryBackend::new();
  let database = redb::Database::builder()
    .create_with_backend(backend)
    .unwrap();
  let database = Arc::new(database);

  let config = ChunkConfig::new(64).unwrap();
  let store = ChunkStore::new_with_redb_and_config(database, config);

  // Use varied data to avoid dedup across chunks.
  let data: Vec<u8> = (0..200).map(|index| index as u8).collect();
  let map = store.store(&data).unwrap();
  let retrieved = store.retrieve(&map).unwrap();
  assert_eq!(retrieved, data);

  let stats = store.stats().unwrap();
  assert_eq!(stats.total_chunks, 4); // ceil(200/64) = 4
  assert_eq!(stats.total_bytes, 200);
}

#[test]
fn test_redb_backed_garbage_collection() {
  let backend = redb::backends::InMemoryBackend::new();
  let database = redb::Database::builder()
    .create_with_backend(backend)
    .unwrap();
  let database = Arc::new(database);

  let config = ChunkConfig::new(64).unwrap();
  let store = ChunkStore::new_with_redb_and_config(database, config);

  let data_a = vec![0xAAu8; 100];
  let data_b = vec![0xBBu8; 100];
  let map_a = store.store(&data_a).unwrap();
  let _map_b = store.store(&data_b).unwrap();

  let removed = store.garbage_collect(&[map_a.clone()]).unwrap();
  assert_eq!(removed, 2);

  let retrieved = store.retrieve(&map_a).unwrap();
  assert_eq!(retrieved, data_a);
}
