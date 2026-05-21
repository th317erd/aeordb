use aeordb::engine::compression::CompressionAlgorithm;
use aeordb::engine::entry_header::{
    EntryHeader, CURRENT_ENTRY_VERSION, ENTRY_MAGIC, FLAG_SYSTEM,
};
use aeordb::engine::entry_type::EntryType;
use aeordb::engine::hash_algorithm::HashAlgorithm;

/// Build a header with reasonable defaults for testing.
fn make_header(
    entry_type: EntryType,
    flags: u8,
    key_len: u32,
    val_len: u32,
) -> EntryHeader {
    let hash_algo = HashAlgorithm::Blake3_256;
    let hash = vec![0xAA; hash_algo.hash_length()];
    EntryHeader {
        entry_version: CURRENT_ENTRY_VERSION,
        entry_type,
        flags,
        hash_algo,
        compression_algo: CompressionAlgorithm::None,
        encryption_algo: 0,
        key_length: key_len,
        value_length: val_len,
        timestamp: 1_700_000_000_000,
        total_length: EntryHeader::compute_total_length(hash_algo, key_len as usize, val_len as usize).unwrap(),
        hash,
    }
}

// ===========================================================================
// Serialize / Deserialize round-trip
// ===========================================================================

#[test]
fn roundtrip_basic() {
    let header = make_header(EntryType::Chunk, 0, 10, 200);
    let bytes = header.serialize();
    let restored = EntryHeader::deserialize(&mut &bytes[..]).unwrap();

    assert_eq!(restored.entry_version, header.entry_version);
    assert_eq!(restored.entry_type, header.entry_type);
    assert_eq!(restored.flags, header.flags);
    assert_eq!(restored.hash_algo, header.hash_algo);
    assert_eq!(restored.compression_algo, header.compression_algo);
    assert_eq!(restored.encryption_algo, header.encryption_algo);
    assert_eq!(restored.key_length, header.key_length);
    assert_eq!(restored.value_length, header.value_length);
    assert_eq!(restored.timestamp, header.timestamp);
    assert_eq!(restored.total_length, header.total_length);
    assert_eq!(restored.hash, header.hash);
}

#[test]
fn roundtrip_all_entry_types() {
    let types = [
        EntryType::Chunk,
        EntryType::FileRecord,
        EntryType::DirectoryIndex,
        EntryType::DeletionRecord,
        EntryType::Snapshot,
        EntryType::Void,
        EntryType::Fork,
        EntryType::Symlink,
    ];
    for entry_type in types {
        let header = make_header(entry_type, 0, 5, 50);
        let bytes = header.serialize();
        let restored = EntryHeader::deserialize(&mut &bytes[..]).unwrap();
        assert_eq!(restored.entry_type, entry_type, "failed for {:?}", entry_type);
    }
}

#[test]
fn roundtrip_with_compression_zstd() {
    let hash_algo = HashAlgorithm::Blake3_256;
    let header = EntryHeader {
        entry_version: CURRENT_ENTRY_VERSION,
        entry_type: EntryType::Chunk,
        flags: 0,
        hash_algo,
        compression_algo: CompressionAlgorithm::Zstd,
        encryption_algo: 0,
        key_length: 8,
        value_length: 1024,
        timestamp: 1_700_000_000_000,
        total_length: EntryHeader::compute_total_length(hash_algo, 8, 1024).unwrap(),
        hash: vec![0xBB; hash_algo.hash_length()],
    };
    let bytes = header.serialize();
    let restored = EntryHeader::deserialize(&mut &bytes[..]).unwrap();
    assert_eq!(restored.compression_algo, CompressionAlgorithm::Zstd);
}

#[test]
fn roundtrip_zero_key_and_value() {
    let header = make_header(EntryType::Snapshot, 0, 0, 0);
    let bytes = header.serialize();
    let restored = EntryHeader::deserialize(&mut &bytes[..]).unwrap();
    assert_eq!(restored.key_length, 0);
    assert_eq!(restored.value_length, 0);
}

#[test]
fn roundtrip_large_key_and_value() {
    // Use a large but in-bounds value. MAX_KEY_OR_VALUE_BYTES = 1 GiB
    // (introduced in audit fix to prevent u32 overflow in
    // compute_total_length); larger values are explicitly rejected at the
    // append layer.
    let large = EntryHeader::MAX_KEY_OR_VALUE_BYTES;
    let header = make_header(EntryType::Chunk, 0, large, large);
    let bytes = header.serialize();
    let restored = EntryHeader::deserialize(&mut &bytes[..]).unwrap();
    assert_eq!(restored.key_length, large);
    assert_eq!(restored.value_length, large);
}

#[test]
fn roundtrip_negative_timestamp() {
    let hash_algo = HashAlgorithm::Blake3_256;
    let header = EntryHeader {
        entry_version: CURRENT_ENTRY_VERSION,
        entry_type: EntryType::Chunk,
        flags: 0,
        hash_algo,
        compression_algo: CompressionAlgorithm::None,
        encryption_algo: 0,
        key_length: 1,
        value_length: 1,
        timestamp: -9999,
        total_length: EntryHeader::compute_total_length(hash_algo, 1, 1).unwrap(),
        hash: vec![0x00; hash_algo.hash_length()],
    };
    let bytes = header.serialize();
    let restored = EntryHeader::deserialize(&mut &bytes[..]).unwrap();
    assert_eq!(restored.timestamp, -9999);
}

// ===========================================================================
// FLAG_SYSTEM and is_system_entry
// ===========================================================================

#[test]
fn is_system_entry_when_flag_set() {
    let header = make_header(EntryType::FileRecord, FLAG_SYSTEM, 5, 100);
    assert!(header.is_system_entry());
}

#[test]
fn is_not_system_entry_when_flag_clear() {
    let header = make_header(EntryType::FileRecord, 0, 5, 100);
    assert!(!header.is_system_entry());
}

#[test]
fn system_flag_survives_roundtrip() {
    let header = make_header(EntryType::FileRecord, FLAG_SYSTEM, 5, 100);
    let bytes = header.serialize();
    let restored = EntryHeader::deserialize(&mut &bytes[..]).unwrap();
    assert!(restored.is_system_entry());
}

#[test]
fn other_flags_preserved() {
    // Use flags 0xFF — all bits set. FLAG_SYSTEM is bit 0, but all should survive.
    let header = make_header(EntryType::Chunk, 0xFF, 5, 100);
    let bytes = header.serialize();
    let restored = EntryHeader::deserialize(&mut &bytes[..]).unwrap();
    assert_eq!(restored.flags, 0xFF);
    assert!(restored.is_system_entry());
}

// ===========================================================================
// header_size and compute_total_length
// ===========================================================================

#[test]
fn header_size_blake3() {
    let header = make_header(EntryType::Chunk, 0, 10, 20);
    // Fixed 31 + Blake3 hash length 32 = 63
    assert_eq!(header.header_size(), 63);
}

#[test]
fn compute_total_length_consistency() {
    let algo = HashAlgorithm::Blake3_256;
    let key_len = 15u32;
    let val_len = 500u32;
    let total = EntryHeader::compute_total_length(algo, key_len as usize, val_len as usize).unwrap();
    // total = header_size + key + value
    let expected = (31 + algo.hash_length()) as u32 + key_len + val_len;
    assert_eq!(total, expected);
}

// ===========================================================================
// compute_hash and verify
// ===========================================================================

#[test]
fn compute_hash_deterministic() {
    let h1 = EntryHeader::compute_hash(
        EntryType::Chunk,
        b"key",
        b"value",
        HashAlgorithm::Blake3_256,
    )
    .unwrap();
    let h2 = EntryHeader::compute_hash(
        EntryType::Chunk,
        b"key",
        b"value",
        HashAlgorithm::Blake3_256,
    )
    .unwrap();
    assert_eq!(h1, h2);
    assert_eq!(h1.len(), 32);
}

#[test]
fn compute_hash_differs_for_different_key() {
    let h1 = EntryHeader::compute_hash(
        EntryType::Chunk,
        b"key1",
        b"value",
        HashAlgorithm::Blake3_256,
    )
    .unwrap();
    let h2 = EntryHeader::compute_hash(
        EntryType::Chunk,
        b"key2",
        b"value",
        HashAlgorithm::Blake3_256,
    )
    .unwrap();
    assert_ne!(h1, h2);
}

#[test]
fn compute_hash_differs_for_different_entry_type() {
    let h1 = EntryHeader::compute_hash(
        EntryType::Chunk,
        b"key",
        b"value",
        HashAlgorithm::Blake3_256,
    )
    .unwrap();
    let h2 = EntryHeader::compute_hash(
        EntryType::FileRecord,
        b"key",
        b"value",
        HashAlgorithm::Blake3_256,
    )
    .unwrap();
    assert_ne!(h1, h2);
}

#[test]
fn verify_returns_true_for_matching_hash() {
    let key = b"test_key";
    let value = b"test_value";
    let algo = HashAlgorithm::Blake3_256;
    let hash = EntryHeader::compute_hash(EntryType::Chunk, key, value, algo).unwrap();
    let header = EntryHeader {
        entry_version: CURRENT_ENTRY_VERSION,
        entry_type: EntryType::Chunk,
        flags: 0,
        hash_algo: algo,
        compression_algo: CompressionAlgorithm::None,
        encryption_algo: 0,
        key_length: key.len() as u32,
        value_length: value.len() as u32,
        timestamp: 0,
        total_length: 0,
        hash,
    };
    assert!(header.verify(key, value));
}

#[test]
fn verify_returns_false_for_tampered_value() {
    let key = b"test_key";
    let value = b"test_value";
    let algo = HashAlgorithm::Blake3_256;
    let hash = EntryHeader::compute_hash(EntryType::Chunk, key, value, algo).unwrap();
    let header = EntryHeader {
        entry_version: CURRENT_ENTRY_VERSION,
        entry_type: EntryType::Chunk,
        flags: 0,
        hash_algo: algo,
        compression_algo: CompressionAlgorithm::None,
        encryption_algo: 0,
        key_length: key.len() as u32,
        value_length: value.len() as u32,
        timestamp: 0,
        total_length: 0,
        hash,
    };
    assert!(!header.verify(key, b"TAMPERED"));
}

#[test]
fn verify_returns_false_for_tampered_key() {
    let key = b"test_key";
    let value = b"test_value";
    let algo = HashAlgorithm::Blake3_256;
    let hash = EntryHeader::compute_hash(EntryType::Chunk, key, value, algo).unwrap();
    let header = EntryHeader {
        entry_version: CURRENT_ENTRY_VERSION,
        entry_type: EntryType::Chunk,
        flags: 0,
        hash_algo: algo,
        compression_algo: CompressionAlgorithm::None,
        encryption_algo: 0,
        key_length: key.len() as u32,
        value_length: value.len() as u32,
        timestamp: 0,
        total_length: 0,
        hash,
    };
    assert!(!header.verify(b"WRONG_KEY", value));
}

// ===========================================================================
// Deserialization error paths
// ===========================================================================

#[test]
fn deserialize_empty_input_fails() {
    let result = EntryHeader::deserialize(&mut &[][..]);
    assert!(result.is_err());
}

#[test]
fn deserialize_truncated_input_fails() {
    // Only 10 bytes -- not enough for the fixed header (31 bytes).
    let data = [0u8; 10];
    let result = EntryHeader::deserialize(&mut &data[..]);
    assert!(result.is_err());
}

#[test]
fn deserialize_bad_magic_fails() {
    let header = make_header(EntryType::Chunk, 0, 1, 1);
    let mut bytes = header.serialize();
    // Corrupt the magic bytes.
    bytes[0] = 0xFF;
    bytes[1] = 0xFF;
    let result = EntryHeader::deserialize(&mut &bytes[..]);
    assert!(result.is_err());
}

#[test]
fn deserialize_invalid_entry_type_fails() {
    let header = make_header(EntryType::Chunk, 0, 1, 1);
    let mut bytes = header.serialize();
    // EntryType is at fixed_buffer[5]. Set it to an invalid value.
    bytes[5] = 0xFF;
    let result = EntryHeader::deserialize(&mut &bytes[..]);
    assert!(result.is_err());
}

#[test]
fn deserialize_invalid_hash_algorithm_fails() {
    let header = make_header(EntryType::Chunk, 0, 1, 1);
    let mut bytes = header.serialize();
    // Hash algorithm is at bytes[7..9] (u16 LE). Set to invalid.
    bytes[7] = 0xFF;
    bytes[8] = 0xFF;
    let result = EntryHeader::deserialize(&mut &bytes[..]);
    assert!(result.is_err());
}

#[test]
fn deserialize_invalid_compression_algo_fails() {
    let header = make_header(EntryType::Chunk, 0, 1, 1);
    let mut bytes = header.serialize();
    // Compression algorithm is at fixed_buffer[9].
    bytes[9] = 0xFF;
    let result = EntryHeader::deserialize(&mut &bytes[..]);
    assert!(result.is_err());
}

#[test]
fn deserialize_truncated_hash_fails() {
    let header = make_header(EntryType::Chunk, 0, 1, 1);
    let bytes = header.serialize();
    // Truncate so the hash portion is incomplete.
    // Fixed header is 31 bytes; Blake3 hash is 32 bytes. Cut it at 40.
    let truncated = &bytes[..40];
    let result = EntryHeader::deserialize(&mut &truncated[..]);
    assert!(result.is_err());
}

// ===========================================================================
// Serialized size
// ===========================================================================

#[test]
fn serialize_produces_correct_length() {
    let header = make_header(EntryType::Chunk, 0, 10, 20);
    let bytes = header.serialize();
    assert_eq!(bytes.len(), header.header_size());
}

#[test]
fn magic_bytes_at_start() {
    let header = make_header(EntryType::Chunk, 0, 1, 1);
    let bytes = header.serialize();
    let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    assert_eq!(magic, ENTRY_MAGIC);
}

#[test]
fn entry_version_at_byte_4() {
    let header = make_header(EntryType::Chunk, 0, 1, 1);
    let bytes = header.serialize();
    assert_eq!(bytes[4], CURRENT_ENTRY_VERSION);
}
