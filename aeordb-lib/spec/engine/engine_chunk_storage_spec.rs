use aeordb::engine::engine_chunk_storage::EngineChunkStorage;
use aeordb::storage::chunk::Chunk;
use aeordb::storage::chunk_storage::ChunkStorage;
use tempfile::TempDir;

fn create_test_db() -> (TempDir, EngineChunkStorage) {
  let temp_dir = TempDir::new().expect("failed to create temp dir");
  let db_path = temp_dir.path().join("test.aeor");
  let storage = EngineChunkStorage::create(db_path.to_str().unwrap())
    .expect("failed to create database");
  (temp_dir, storage)
}

fn db_path(temp_dir: &TempDir) -> String {
  temp_dir.path().join("test.aeor").to_str().unwrap().to_string()
}

#[test]
fn test_create_new_database() {
  let (_temp_dir, storage) = create_test_db();

  assert_eq!(storage.chunk_count().unwrap(), 0);
  assert!(storage.list_chunk_hashes().unwrap().is_empty());
}

#[test]
fn test_store_and_retrieve_chunk() {
  let (_temp_dir, storage) = create_test_db();

  let chunk = Chunk::new(b"hello world".to_vec());
  storage.store_chunk(&chunk).unwrap();

  let retrieved = storage.get_chunk(&chunk.hash).unwrap();
  assert!(retrieved.is_some());

  let retrieved = retrieved.unwrap();
  assert_eq!(retrieved.hash, chunk.hash);
  assert_eq!(retrieved.data, chunk.data);
}

#[test]
fn test_store_duplicate_chunk_dedup() {
  let (_temp_dir, storage) = create_test_db();

  let chunk = Chunk::new(b"deduplicated data".to_vec());
  storage.store_chunk(&chunk).unwrap();
  storage.store_chunk(&chunk).unwrap(); // Should be a no-op

  assert_eq!(storage.chunk_count().unwrap(), 1);
}

#[test]
fn test_has_chunk() {
  let (_temp_dir, storage) = create_test_db();

  let chunk = Chunk::new(b"existence check".to_vec());

  assert!(!storage.has_chunk(&chunk.hash).unwrap());

  storage.store_chunk(&chunk).unwrap();

  assert!(storage.has_chunk(&chunk.hash).unwrap());
}

#[test]
fn test_has_chunk_nonexistent() {
  let (_temp_dir, storage) = create_test_db();

  let fake_hash = [0xFFu8; 32];
  assert!(!storage.has_chunk(&fake_hash).unwrap());
}

#[test]
fn test_remove_chunk() {
  let (_temp_dir, storage) = create_test_db();

  let chunk = Chunk::new(b"to be removed".to_vec());
  storage.store_chunk(&chunk).unwrap();
  assert!(storage.has_chunk(&chunk.hash).unwrap());

  let removed = storage.remove_chunk(&chunk.hash).unwrap();
  assert!(removed);

  // Should no longer be findable
  assert!(!storage.has_chunk(&chunk.hash).unwrap());
  assert_eq!(storage.chunk_count().unwrap(), 0);

  // get_chunk should return None for deleted chunk
  assert!(storage.get_chunk(&chunk.hash).unwrap().is_none());
}

#[test]
fn test_remove_chunk_nonexistent() {
  let (_temp_dir, storage) = create_test_db();

  let fake_hash = [0xAAu8; 32];
  let removed = storage.remove_chunk(&fake_hash).unwrap();
  assert!(!removed);
}

#[test]
fn test_remove_chunk_already_deleted() {
  let (_temp_dir, storage) = create_test_db();

  let chunk = Chunk::new(b"double delete".to_vec());
  storage.store_chunk(&chunk).unwrap();

  assert!(storage.remove_chunk(&chunk.hash).unwrap());
  // Second removal should return false
  assert!(!storage.remove_chunk(&chunk.hash).unwrap());
}

#[test]
fn test_chunk_count() {
  let (_temp_dir, storage) = create_test_db();

  assert_eq!(storage.chunk_count().unwrap(), 0);

  storage.store_chunk(&Chunk::new(b"one".to_vec())).unwrap();
  assert_eq!(storage.chunk_count().unwrap(), 1);

  storage.store_chunk(&Chunk::new(b"two".to_vec())).unwrap();
  assert_eq!(storage.chunk_count().unwrap(), 2);

  storage.store_chunk(&Chunk::new(b"three".to_vec())).unwrap();
  assert_eq!(storage.chunk_count().unwrap(), 3);
}

#[test]
fn test_list_chunk_hashes() {
  let (_temp_dir, storage) = create_test_db();

  let chunk_a = Chunk::new(b"alpha".to_vec());
  let chunk_b = Chunk::new(b"beta".to_vec());
  let chunk_c = Chunk::new(b"gamma".to_vec());

  storage.store_chunk(&chunk_a).unwrap();
  storage.store_chunk(&chunk_b).unwrap();
  storage.store_chunk(&chunk_c).unwrap();

  let hashes = storage.list_chunk_hashes().unwrap();
  assert_eq!(hashes.len(), 3);
  assert!(hashes.contains(&chunk_a.hash));
  assert!(hashes.contains(&chunk_b.hash));
  assert!(hashes.contains(&chunk_c.hash));
}

#[test]
fn test_list_chunk_hashes_excludes_deleted() {
  let (_temp_dir, storage) = create_test_db();

  let chunk_a = Chunk::new(b"alpha".to_vec());
  let chunk_b = Chunk::new(b"beta".to_vec());

  storage.store_chunk(&chunk_a).unwrap();
  storage.store_chunk(&chunk_b).unwrap();
  storage.remove_chunk(&chunk_a.hash).unwrap();

  let hashes = storage.list_chunk_hashes().unwrap();
  assert_eq!(hashes.len(), 1);
  assert!(!hashes.contains(&chunk_a.hash));
  assert!(hashes.contains(&chunk_b.hash));
}

#[test]
fn test_store_many_chunks() {
  let (_temp_dir, storage) = create_test_db();

  let count = 100;
  let mut stored_hashes = Vec::new();

  for index in 0..count {
    let data = format!("chunk data number {}", index);
    let chunk = Chunk::new(data.into_bytes());
    stored_hashes.push(chunk.hash);
    storage.store_chunk(&chunk).unwrap();
  }

  assert_eq!(storage.chunk_count().unwrap(), count as u64);

  // Verify all chunks are retrievable
  for hash in &stored_hashes {
    assert!(storage.has_chunk(hash).unwrap());
    let retrieved = storage.get_chunk(hash).unwrap();
    assert!(retrieved.is_some());
  }
}

#[test]
fn test_open_existing_database() {
  let temp_dir = TempDir::new().expect("failed to create temp dir");
  let path = db_path(&temp_dir);

  // Create and populate
  {
    let storage = EngineChunkStorage::create(&path).unwrap();
    storage.store_chunk(&Chunk::new(b"persist me".to_vec())).unwrap();
  }

  // Re-open
  let storage = EngineChunkStorage::open(&path).unwrap();
  assert_eq!(storage.chunk_count().unwrap(), 1);
}

#[test]
fn test_chunk_persists_across_reopen() {
  let temp_dir = TempDir::new().expect("failed to create temp dir");
  let path = db_path(&temp_dir);

  let chunk = Chunk::new(b"persistent data".to_vec());
  let chunk_hash = chunk.hash;

  // Store
  {
    let storage = EngineChunkStorage::create(&path).unwrap();
    storage.store_chunk(&chunk).unwrap();
  }

  // Re-open and verify data integrity
  {
    let storage = EngineChunkStorage::open(&path).unwrap();
    assert!(storage.has_chunk(&chunk_hash).unwrap());

    let retrieved = storage.get_chunk(&chunk_hash).unwrap().unwrap();
    assert_eq!(retrieved.data, b"persistent data");
    assert_eq!(retrieved.hash, chunk_hash);
  }
}

#[test]
fn test_large_chunk_storage() {
  let (_temp_dir, storage) = create_test_db();

  // 1 MB chunk
  let large_data = vec![0xABu8; 1_000_000];
  let chunk = Chunk::new(large_data.clone());

  storage.store_chunk(&chunk).unwrap();

  let retrieved = storage.get_chunk(&chunk.hash).unwrap().unwrap();
  assert_eq!(retrieved.data.len(), 1_000_000);
  assert_eq!(retrieved.data, large_data);
}

#[test]
fn test_chunk_integrity_verification() {
  let (_temp_dir, storage) = create_test_db();

  let chunk = Chunk::new(b"integrity test data".to_vec());
  storage.store_chunk(&chunk).unwrap();

  // Retrieve and verify
  let retrieved = storage.get_chunk(&chunk.hash).unwrap().unwrap();
  assert!(retrieved.verify());
}

#[test]
fn test_empty_chunk() {
  let (_temp_dir, storage) = create_test_db();

  let chunk = Chunk::new(Vec::new());
  storage.store_chunk(&chunk).unwrap();

  let retrieved = storage.get_chunk(&chunk.hash).unwrap().unwrap();
  assert!(retrieved.data.is_empty());
  assert!(retrieved.verify());
}

#[test]
fn test_multiple_reopens() {
  let temp_dir = TempDir::new().expect("failed to create temp dir");
  let path = db_path(&temp_dir);

  let chunk_a = Chunk::new(b"first".to_vec());
  let chunk_b = Chunk::new(b"second".to_vec());
  let chunk_c = Chunk::new(b"third".to_vec());

  // Create and add first chunk
  {
    let storage = EngineChunkStorage::create(&path).unwrap();
    storage.store_chunk(&chunk_a).unwrap();
  }

  // Reopen and add second
  {
    let storage = EngineChunkStorage::open(&path).unwrap();
    assert_eq!(storage.chunk_count().unwrap(), 1);
    storage.store_chunk(&chunk_b).unwrap();
  }

  // Reopen and add third
  {
    let storage = EngineChunkStorage::open(&path).unwrap();
    assert_eq!(storage.chunk_count().unwrap(), 2);
    storage.store_chunk(&chunk_c).unwrap();
  }

  // Final reopen — all three should be present
  {
    let storage = EngineChunkStorage::open(&path).unwrap();
    assert_eq!(storage.chunk_count().unwrap(), 3);
    assert!(storage.has_chunk(&chunk_a.hash).unwrap());
    assert!(storage.has_chunk(&chunk_b.hash).unwrap());
    assert!(storage.has_chunk(&chunk_c.hash).unwrap());
  }
}

#[test]
fn test_get_chunk_returns_none_for_missing() {
  let (_temp_dir, storage) = create_test_db();

  let missing_hash = [0x00u8; 32];
  let result = storage.get_chunk(&missing_hash).unwrap();
  assert!(result.is_none());
}
