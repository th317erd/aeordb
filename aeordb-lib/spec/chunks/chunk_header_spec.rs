use std::thread;
use std::time::Duration;

use aeordb::storage::{
  Chunk, ChunkConfig, ChunkHeader, ChunkStorage,
  HEADER_SIZE, hash_data,
};
use aeordb::engine::EngineChunkStorage;

// ---------------------------------------------------------------------------
// ChunkHeader construction
// ---------------------------------------------------------------------------

#[test]
fn test_header_default_format_version_is_1() {
  let header = ChunkHeader::new();
  assert_eq!(header.format_version, 1);
}

#[test]
fn test_header_created_at_set_on_creation() {
  let before = chrono::Utc::now().timestamp_millis();
  let header = ChunkHeader::new();
  let after = chrono::Utc::now().timestamp_millis();

  assert!(header.created_at >= before);
  assert!(header.created_at <= after);
}

#[test]
fn test_header_updated_at_set_on_creation() {
  let before = chrono::Utc::now().timestamp_millis();
  let header = ChunkHeader::new();
  let after = chrono::Utc::now().timestamp_millis();

  assert!(header.updated_at >= before);
  assert!(header.updated_at <= after);
  // On creation, updated_at equals created_at.
  assert_eq!(header.created_at, header.updated_at);
}

#[test]
fn test_header_reserved_is_zeros() {
  let header = ChunkHeader::new();
  assert_eq!(header.reserved, [0u8; 16]);
}

// ---------------------------------------------------------------------------
// Serialization / deserialization
// ---------------------------------------------------------------------------

#[test]
fn test_header_serialize_deserialize_roundtrip() {
  let header = ChunkHeader::new();
  let bytes = header.serialize();
  let deserialized = ChunkHeader::deserialize(&bytes).unwrap();

  assert_eq!(deserialized.format_version, header.format_version);
  assert_eq!(deserialized.created_at, header.created_at);
  assert_eq!(deserialized.updated_at, header.updated_at);
  assert_eq!(deserialized.reserved, header.reserved);
  assert_eq!(deserialized, header);
}

#[test]
fn test_header_size_is_33_bytes() {
  assert_eq!(HEADER_SIZE, 33);

  let header = ChunkHeader::new();
  let bytes = header.serialize();
  assert_eq!(bytes.len(), 33);
}

#[test]
fn test_header_deserialize_from_too_short_slice() {
  let short_bytes = [0u8; 10];
  let result = ChunkHeader::deserialize_from_slice(&short_bytes);
  assert!(result.is_err());
}

#[test]
fn test_header_deserialize_unsupported_version_zero() {
  let mut bytes = [0u8; HEADER_SIZE];
  bytes[0] = 0; // format_version = 0 is unsupported
  let result = ChunkHeader::deserialize(&bytes);
  assert!(result.is_err());
}

#[test]
fn test_header_deserialize_future_version_accepted() {
  // Versions > 1 should still deserialize (forward compatibility).
  let header = ChunkHeader {
    format_version: 2,
    created_at: 12345,
    updated_at: 12345,
    reserved: [0u8; 16],
  };
  let bytes = header.serialize();
  let deserialized = ChunkHeader::deserialize(&bytes).unwrap();
  assert_eq!(deserialized.format_version, 2);
}

#[test]
fn test_header_serialization_layout() {
  let header = ChunkHeader {
    format_version: 1,
    created_at: 0x0102030405060708,
    updated_at: 0x090A0B0C0D0E0F10,
    reserved: [0u8; 16],
  };
  let bytes = header.serialize();

  // Byte 0: format_version.
  assert_eq!(bytes[0], 1);
  // Bytes 1..9: created_at big-endian.
  assert_eq!(&bytes[1..9], &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
  // Bytes 9..17: updated_at big-endian.
  assert_eq!(&bytes[9..17], &[0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10]);
  // Bytes 17..33: reserved (zeros).
  assert_eq!(&bytes[17..33], &[0u8; 16]);
}

// ---------------------------------------------------------------------------
// Chunk hash excludes header
// ---------------------------------------------------------------------------

#[test]
fn test_chunk_hash_excludes_header() {
  let data = b"hash should only cover this data".to_vec();
  let expected_hash = hash_data(&data);

  let chunk = Chunk::new(data.clone());

  // The chunk hash should match the data-only hash.
  assert_eq!(chunk.hash, expected_hash);
  // Verify passes because it hashes data only.
  assert!(chunk.verify());

  // Creating a second chunk with the same data should produce the same hash
  // even though the headers may have different timestamps.
  thread::sleep(Duration::from_millis(2));
  let chunk_b = Chunk::new(data);
  assert_eq!(chunk.hash, chunk_b.hash);
}

// ---------------------------------------------------------------------------
// Data capacity
// ---------------------------------------------------------------------------

#[test]
fn test_data_capacity_is_chunk_size_minus_header() {
  let config = ChunkConfig::new(64).unwrap();
  assert_eq!(config.data_capacity(), 64 - HEADER_SIZE);
  assert_eq!(config.data_capacity(), 31);

  let config_256 = ChunkConfig::new(256).unwrap();
  assert_eq!(config_256.data_capacity(), 256 - HEADER_SIZE);
  assert_eq!(config_256.data_capacity(), 223);

  let config_large = ChunkConfig::new(262144).unwrap();
  assert_eq!(config_large.data_capacity(), 262144 - HEADER_SIZE);
}

#[test]
fn test_chunk_size_too_small_for_header_rejected() {
  // Sizes <= HEADER_SIZE (33) should be rejected.
  assert!(ChunkConfig::new(1).is_err());
  assert!(ChunkConfig::new(2).is_err());
  assert!(ChunkConfig::new(4).is_err());
  assert!(ChunkConfig::new(8).is_err());
  assert!(ChunkConfig::new(16).is_err());
  assert!(ChunkConfig::new(32).is_err());

  // 64 is the smallest valid power of two > 33.
  assert!(ChunkConfig::new(64).is_ok());
}

// ---------------------------------------------------------------------------
// Engine chunk storage with headers
// ---------------------------------------------------------------------------

#[test]
fn test_engine_chunk_header_preserved_through_storage_roundtrip() {
  let temp_dir = tempfile::tempdir().unwrap();
  let engine_path = temp_dir.path().join("test.aeordb");
  let storage = EngineChunkStorage::create(engine_path.to_str().unwrap()).unwrap();

  let data = b"roundtrip with header".to_vec();
  let chunk = Chunk::new(data);

  let original_header = chunk.header.clone();
  storage.store_chunk(&chunk).unwrap();

  let retrieved = storage.get_chunk(&chunk.hash).unwrap().unwrap();
  assert_eq!(retrieved.header, original_header);
  assert_eq!(retrieved.data, chunk.data);
  assert_eq!(retrieved.hash, chunk.hash);
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[test]
fn test_empty_data_chunk_has_header() {
  let temp_dir = tempfile::tempdir().unwrap();
  let engine_path = temp_dir.path().join("test.aeordb");
  let storage = EngineChunkStorage::create(engine_path.to_str().unwrap()).unwrap();

  let chunk = Chunk::new(Vec::new());
  assert_eq!(chunk.header.format_version, 1);
  assert!(chunk.data.is_empty());
  assert!(chunk.verify());

  storage.store_chunk(&chunk).unwrap();
  let retrieved = storage.get_chunk(&chunk.hash).unwrap().unwrap();
  assert_eq!(retrieved.header.format_version, 1);
  assert!(retrieved.data.is_empty());
}

#[test]
fn test_chunk_dedup_preserves_original_header() {
  let temp_dir = tempfile::tempdir().unwrap();
  let engine_path = temp_dir.path().join("test.aeordb");
  let storage = EngineChunkStorage::create(engine_path.to_str().unwrap()).unwrap();

  let data = b"dedup test".to_vec();

  let chunk_a = Chunk::new(data.clone());
  storage.store_chunk(&chunk_a).unwrap();

  // Sleep to get a different timestamp.
  thread::sleep(Duration::from_millis(2));
  let chunk_b = Chunk::new(data);
  assert_eq!(chunk_a.hash, chunk_b.hash);

  // Store again -- should be a no-op (dedup).
  storage.store_chunk(&chunk_b).unwrap();

  // Retrieved chunk should have the original header (first write wins).
  let retrieved = storage.get_chunk(&chunk_a.hash).unwrap().unwrap();
  assert_eq!(retrieved.header.created_at, chunk_a.header.created_at);
}
