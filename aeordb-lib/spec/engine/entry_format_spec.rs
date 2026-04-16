use std::io::Cursor;

use aeordb::engine::compression::CompressionAlgorithm;
use aeordb::engine::entry_header::{EntryHeader, ENTRY_MAGIC, CURRENT_ENTRY_VERSION};
use aeordb::engine::entry_type::EntryType;
use aeordb::engine::file_header::{FileHeader, FILE_HEADER_SIZE, FILE_MAGIC};
use aeordb::engine::hash_algorithm::HashAlgorithm;

#[test]
fn test_entry_magic_is_correct() {
  assert_eq!(ENTRY_MAGIC, 0x0AE012DB);
}

#[test]
fn test_entry_header_serialize_deserialize_roundtrip() {
  let key = b"test-key";
  let value = b"test-value";
  let hash_algo = HashAlgorithm::Blake3_256;
  let entry_type = EntryType::Chunk;

  let hash = EntryHeader::compute_hash(entry_type, key, value, hash_algo)
    .expect("Failed to compute hash");
  let total_length =
    EntryHeader::compute_total_length(hash_algo, key.len() as u32, value.len() as u32);

  let header = EntryHeader {
    entry_version: CURRENT_ENTRY_VERSION,
    entry_type,
    flags: 0,
    hash_algo,
    compression_algo: CompressionAlgorithm::None,
    encryption_algo: 0,
    key_length: key.len() as u32,
    value_length: value.len() as u32,
    timestamp: 1700000000000,
    total_length,
    hash,
  };

  let serialized = header.serialize();
  let mut cursor = Cursor::new(&serialized);
  let deserialized = EntryHeader::deserialize(&mut cursor)
    .expect("Failed to deserialize header");

  assert_eq!(deserialized.entry_version, header.entry_version);
  assert_eq!(deserialized.entry_type, header.entry_type);
  assert_eq!(deserialized.flags, header.flags);
  assert_eq!(deserialized.hash_algo, header.hash_algo);
  assert_eq!(deserialized.key_length, header.key_length);
  assert_eq!(deserialized.value_length, header.value_length);
  assert_eq!(deserialized.timestamp, header.timestamp);
  assert_eq!(deserialized.total_length, header.total_length);
  assert_eq!(deserialized.hash, header.hash);
}

#[test]
fn test_entry_header_with_blake3() {
  let key = b"hello";
  let value = b"world";
  let hash_algo = HashAlgorithm::Blake3_256;

  let hash = EntryHeader::compute_hash(EntryType::Chunk, key, value, hash_algo)
    .expect("Failed to compute hash");

  assert_eq!(hash.len(), 32);
  // Hash should be deterministic
  let hash_again = EntryHeader::compute_hash(EntryType::Chunk, key, value, hash_algo)
    .expect("Failed to compute hash");
  assert_eq!(hash, hash_again);
}

#[test]
fn test_entry_header_hash_verification_passes() {
  let key = b"test-key";
  let value = b"test-value";
  let hash_algo = HashAlgorithm::Blake3_256;

  let hash = EntryHeader::compute_hash(EntryType::FileRecord, key, value, hash_algo)
    .expect("Failed to compute hash");

  let header = EntryHeader {
    entry_version: CURRENT_ENTRY_VERSION,
    entry_type: EntryType::FileRecord,
    flags: 0,
    hash_algo,
    compression_algo: CompressionAlgorithm::None,
    encryption_algo: 0,
    key_length: key.len() as u32,
    value_length: value.len() as u32,
    timestamp: 1700000000000,
    total_length: EntryHeader::compute_total_length(
      hash_algo,
      key.len() as u32,
      value.len() as u32,
    ),
    hash,
  };

  assert!(header.verify(key, value));
}

#[test]
fn test_entry_header_hash_verification_fails_on_tamper() {
  let key = b"test-key";
  let value = b"test-value";
  let hash_algo = HashAlgorithm::Blake3_256;

  let hash = EntryHeader::compute_hash(EntryType::Chunk, key, value, hash_algo)
    .expect("Failed to compute hash");

  let header = EntryHeader {
    entry_version: CURRENT_ENTRY_VERSION,
    entry_type: EntryType::Chunk,
    flags: 0,
    hash_algo,
    compression_algo: CompressionAlgorithm::None,
    encryption_algo: 0,
    key_length: key.len() as u32,
    value_length: value.len() as u32,
    timestamp: 1700000000000,
    total_length: EntryHeader::compute_total_length(
      hash_algo,
      key.len() as u32,
      value.len() as u32,
    ),
    hash,
  };

  // Tampered value
  assert!(!header.verify(key, b"tampered-value"));
  // Tampered key
  assert!(!header.verify(b"tampered-key", value));
  // Both tampered
  assert!(!header.verify(b"tampered-key", b"tampered-value"));
}

#[test]
fn test_entry_type_roundtrip() {
  let types = [
    (0x01, EntryType::Chunk),
    (0x02, EntryType::FileRecord),
    (0x03, EntryType::DirectoryIndex),
    (0x04, EntryType::DeletionRecord),
    (0x05, EntryType::Snapshot),
    (0x06, EntryType::Void),
    (0x07, EntryType::Fork),
  ];

  for (byte_value, expected_type) in types {
    let parsed = EntryType::from_u8(byte_value).expect("Failed to parse entry type");
    assert_eq!(parsed, expected_type);
    assert_eq!(parsed.to_u8(), byte_value);
  }
}

#[test]
fn test_entry_type_invalid_value() {
  assert!(EntryType::from_u8(0x00).is_err());
  assert!(EntryType::from_u8(0x09).is_err());  // 0x08 is Symlink (valid)
  assert!(EntryType::from_u8(0xFF).is_err());
}

#[test]
fn test_hash_algorithm_lengths() {
  assert_eq!(HashAlgorithm::Blake3_256.hash_length(), 32);
  assert_eq!(HashAlgorithm::Sha256.hash_length(), 32);
  assert_eq!(HashAlgorithm::Sha512.hash_length(), 64);
  assert_eq!(HashAlgorithm::Sha3_256.hash_length(), 32);
  assert_eq!(HashAlgorithm::Sha3_512.hash_length(), 64);
}

#[test]
fn test_hash_algorithm_from_u16() {
  assert_eq!(HashAlgorithm::from_u16(0x0001), Some(HashAlgorithm::Blake3_256));
  assert_eq!(HashAlgorithm::from_u16(0x0002), Some(HashAlgorithm::Sha256));
  assert_eq!(HashAlgorithm::from_u16(0x0003), Some(HashAlgorithm::Sha512));
  assert_eq!(HashAlgorithm::from_u16(0x0004), Some(HashAlgorithm::Sha3_256));
  assert_eq!(HashAlgorithm::from_u16(0x0005), Some(HashAlgorithm::Sha3_512));
  assert_eq!(HashAlgorithm::from_u16(0x0000), None);
  assert_eq!(HashAlgorithm::from_u16(0x0006), None);
  assert_eq!(HashAlgorithm::from_u16(0xFFFF), None);
}

#[test]
fn test_file_header_serialize_deserialize_roundtrip() {
  let header = FileHeader::new(HashAlgorithm::Blake3_256);
  let serialized = header.serialize();
  let deserialized = FileHeader::deserialize(&serialized)
    .expect("Failed to deserialize file header");

  assert_eq!(deserialized.header_version, header.header_version);
  assert_eq!(deserialized.hash_algo, header.hash_algo);
  assert_eq!(deserialized.created_at, header.created_at);
  assert_eq!(deserialized.updated_at, header.updated_at);
  assert_eq!(deserialized.kv_block_offset, header.kv_block_offset);
  assert_eq!(deserialized.kv_block_length, header.kv_block_length);
  assert_eq!(deserialized.kv_block_version, header.kv_block_version);
  assert_eq!(deserialized.nvt_offset, header.nvt_offset);
  assert_eq!(deserialized.nvt_length, header.nvt_length);
  assert_eq!(deserialized.nvt_version, header.nvt_version);
  assert_eq!(deserialized.head_hash, header.head_hash);
  assert_eq!(deserialized.entry_count, header.entry_count);
  assert_eq!(deserialized.resize_in_progress, header.resize_in_progress);
  assert_eq!(deserialized.buffer_kvs_offset, header.buffer_kvs_offset);
  assert_eq!(deserialized.buffer_nvt_offset, header.buffer_nvt_offset);
}

#[test]
fn test_file_header_is_exactly_256_bytes() {
  let header = FileHeader::new(HashAlgorithm::Blake3_256);
  let serialized = header.serialize();
  assert_eq!(serialized.len(), FILE_HEADER_SIZE);
  assert_eq!(FILE_HEADER_SIZE, 256);
}

#[test]
fn test_file_header_magic_validation() {
  let mut bytes = [0u8; FILE_HEADER_SIZE];
  // No magic — should fail
  let result = FileHeader::deserialize(&bytes);
  assert!(result.is_err());

  // Wrong magic
  bytes[0..4].copy_from_slice(b"NOPE");
  let result = FileHeader::deserialize(&bytes);
  assert!(result.is_err());

  // Correct magic but invalid hash algo
  bytes[0..4].copy_from_slice(FILE_MAGIC);
  bytes[4] = 1; // header_version
  bytes[5] = 0xFF; // invalid hash_algo high byte
  bytes[6] = 0xFF; // invalid hash_algo low byte
  let result = FileHeader::deserialize(&bytes);
  assert!(result.is_err());
}

#[test]
fn test_entry_header_total_length_correct() {
  let hash_algo = HashAlgorithm::Blake3_256;
  let key_length: u32 = 16;
  let value_length: u32 = 128;

  let total = EntryHeader::compute_total_length(hash_algo, key_length, value_length);
  // 31 (fixed) + 32 (blake3 hash) + 16 (key) + 128 (value) = 207
  let expected = 31 + 32 + 16 + 128;
  assert_eq!(total, expected);
}

#[test]
fn test_void_entry_format() {
  let key = b"";
  let value = vec![0u8; 100];
  let hash_algo = HashAlgorithm::Blake3_256;

  let hash = EntryHeader::compute_hash(EntryType::Void, key, &value, hash_algo)
    .expect("Failed to compute hash");
  let total_length =
    EntryHeader::compute_total_length(hash_algo, key.len() as u32, value.len() as u32);

  let header = EntryHeader {
    entry_version: CURRENT_ENTRY_VERSION,
    entry_type: EntryType::Void,
    flags: 0,
    hash_algo,
    compression_algo: CompressionAlgorithm::None,
    encryption_algo: 0,
    key_length: 0,
    value_length: value.len() as u32,
    timestamp: 1700000000000,
    total_length,
    hash,
  };

  assert_eq!(header.entry_type, EntryType::Void);
  assert_eq!(header.key_length, 0);
  assert!(header.verify(key, &value));

  // Roundtrip
  let serialized = header.serialize();
  let mut cursor = Cursor::new(&serialized);
  let deserialized = EntryHeader::deserialize(&mut cursor)
    .expect("Failed to deserialize void entry header");
  assert_eq!(deserialized.entry_type, EntryType::Void);
  assert_eq!(deserialized.key_length, 0);
  assert_eq!(deserialized.value_length, 100);
}

#[test]
fn test_different_hash_algorithms_produce_different_lengths() {
  // Blake3 and Sha256 both produce 32-byte hashes
  assert_eq!(HashAlgorithm::Blake3_256.hash_length(), 32);
  assert_eq!(HashAlgorithm::Sha256.hash_length(), 32);
  // Sha512 produces 64-byte hashes
  assert_eq!(HashAlgorithm::Sha512.hash_length(), 64);

  // Total length differs when hash length differs
  let total_blake3 = EntryHeader::compute_total_length(HashAlgorithm::Blake3_256, 10, 10);
  let total_sha512 = EntryHeader::compute_total_length(HashAlgorithm::Sha512, 10, 10);
  assert_ne!(total_blake3, total_sha512);
  // sha512 total should be 32 bytes larger (64 - 32)
  assert_eq!(total_sha512 - total_blake3, 32);
}

#[test]
fn test_entry_header_deserialize_invalid_magic() {
  let mut data = vec![0xDE, 0xAD, 0xBE, 0xEF]; // wrong magic
  data.extend_from_slice(&[0u8; 60]); // padding
  let mut cursor = Cursor::new(&data);
  let result = EntryHeader::deserialize(&mut cursor);
  assert!(result.is_err());
}

#[test]
fn test_entry_header_deserialize_version_zero_is_valid() {
  // Version 0 is the current format — it must be accepted.
  assert_eq!(CURRENT_ENTRY_VERSION, 0);

  let key = b"test-key";
  let value = b"test-value";
  let hash_algo = HashAlgorithm::Blake3_256;
  let entry_type = EntryType::Chunk;

  let hash = EntryHeader::compute_hash(entry_type, key, value, hash_algo)
    .expect("Failed to compute hash");
  let total_length =
    EntryHeader::compute_total_length(hash_algo, key.len() as u32, value.len() as u32);

  let header = EntryHeader {
    entry_version: 0,
    entry_type,
    flags: 0,
    hash_algo,
    compression_algo: CompressionAlgorithm::None,
    encryption_algo: 0,
    key_length: key.len() as u32,
    value_length: value.len() as u32,
    timestamp: 1234567890,
    total_length,
    hash,
  };

  let serialized = header.serialize();
  let mut cursor = Cursor::new(&serialized);
  let deserialized = EntryHeader::deserialize(&mut cursor).expect("Version 0 must deserialize");
  assert_eq!(deserialized.entry_version, 0);
}

#[test]
fn test_entry_header_deserialize_invalid_entry_type() {
  let mut data = Vec::new();
  data.extend_from_slice(&ENTRY_MAGIC.to_le_bytes());
  data.push(CURRENT_ENTRY_VERSION); // valid entry_version
  data.push(0xFF); // invalid entry_type
  data.extend_from_slice(&[0u8; 60]); // padding
  let mut cursor = Cursor::new(&data);
  let result = EntryHeader::deserialize(&mut cursor);
  assert!(result.is_err());
}

#[test]
fn test_entry_header_deserialize_truncated_data() {
  let data = vec![0u8; 10]; // too short for even the fixed header
  let mut cursor = Cursor::new(&data);
  let result = EntryHeader::deserialize(&mut cursor);
  assert!(result.is_err());
}

#[test]
fn test_hash_algorithm_compute_unsupported() {
  // Only Blake3_256 is implemented; others should return errors
  let data = b"test data";
  assert!(HashAlgorithm::Sha256.compute_hash(data).is_err());
  assert!(HashAlgorithm::Sha512.compute_hash(data).is_err());
  assert!(HashAlgorithm::Sha3_256.compute_hash(data).is_err());
  assert!(HashAlgorithm::Sha3_512.compute_hash(data).is_err());
}

#[test]
fn test_entry_header_different_entry_types_produce_different_hashes() {
  let key = b"same-key";
  let value = b"same-value";
  let hash_algo = HashAlgorithm::Blake3_256;

  let hash_chunk = EntryHeader::compute_hash(EntryType::Chunk, key, value, hash_algo)
    .expect("Failed to compute hash");
  let hash_file = EntryHeader::compute_hash(EntryType::FileRecord, key, value, hash_algo)
    .expect("Failed to compute hash");

  // Different entry types should produce different hashes because entry_type
  // is included in the hash input
  assert_ne!(hash_chunk, hash_file);
}

#[test]
fn test_file_header_with_nonzero_fields() {
  let mut header = FileHeader::new(HashAlgorithm::Blake3_256);
  header.kv_block_offset = 1024;
  header.kv_block_length = 4096;
  header.nvt_offset = 5120;
  header.nvt_length = 2048;
  header.entry_count = 42;
  header.resize_in_progress = true;
  header.buffer_kvs_offset = 8192;
  header.buffer_nvt_offset = 9216;
  header.head_hash = vec![0xAB; 32];

  let serialized = header.serialize();
  let deserialized = FileHeader::deserialize(&serialized)
    .expect("Failed to deserialize file header");

  assert_eq!(deserialized.kv_block_offset, 1024);
  assert_eq!(deserialized.kv_block_length, 4096);
  assert_eq!(deserialized.nvt_offset, 5120);
  assert_eq!(deserialized.nvt_length, 2048);
  assert_eq!(deserialized.entry_count, 42);
  assert!(deserialized.resize_in_progress);
  assert_eq!(deserialized.buffer_kvs_offset, 8192);
  assert_eq!(deserialized.buffer_nvt_offset, 9216);
  assert_eq!(deserialized.head_hash, vec![0xAB; 32]);
}
