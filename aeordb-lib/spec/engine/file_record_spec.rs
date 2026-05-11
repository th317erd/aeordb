use aeordb::engine::file_record::FileRecord;

const BLAKE3_HASH_LEN: usize = 32;

/// Build a FileRecord with explicit timestamps for deterministic testing.
fn make_record(path: &str, content_type: Option<&str>, total_size: u64, chunks: Vec<Vec<u8>>) -> FileRecord {
    let mut rec = FileRecord::new(
        path.to_string(),
        content_type.map(|s| s.to_string()),
        total_size,
        chunks,
    );
    // Pin timestamps for deterministic round-trips.
    rec.created_at = 1_700_000_000_000;
    rec.updated_at = 1_700_000_001_000;
    rec
}

// ===========================================================================
// Serialize / Deserialize round-trip — happy paths
// ===========================================================================

#[test]
fn roundtrip_basic() {
    let hash = vec![0xAA; BLAKE3_HASH_LEN];
    let record = make_record("/hello.txt", Some("text/plain"), 42, vec![hash]);
    let data = record.serialize(BLAKE3_HASH_LEN).unwrap();
    let restored = FileRecord::deserialize(&data, BLAKE3_HASH_LEN, 0).unwrap();
    assert_eq!(restored, record);
}

#[test]
fn roundtrip_no_content_type() {
    let hash = vec![0xBB; BLAKE3_HASH_LEN];
    let record = make_record("/data.bin", None, 1024, vec![hash]);
    let data = record.serialize(BLAKE3_HASH_LEN).unwrap();
    let restored = FileRecord::deserialize(&data, BLAKE3_HASH_LEN, 0).unwrap();
    assert_eq!(restored.content_type, None);
    assert_eq!(restored, record);
}

#[test]
fn roundtrip_multiple_chunks() {
    let hashes: Vec<Vec<u8>> = (0..5).map(|i| vec![i; BLAKE3_HASH_LEN]).collect();
    let record = make_record("/multi.dat", Some("application/octet-stream"), 5000, hashes.clone());
    let data = record.serialize(BLAKE3_HASH_LEN).unwrap();
    let restored = FileRecord::deserialize(&data, BLAKE3_HASH_LEN, 0).unwrap();
    assert_eq!(restored.chunk_hashes.len(), 5);
    assert_eq!(restored, record);
}

#[test]
fn roundtrip_empty_chunks() {
    let record = make_record("/empty_chunks.txt", Some("text/plain"), 0, vec![]);
    let data = record.serialize(BLAKE3_HASH_LEN).unwrap();
    let restored = FileRecord::deserialize(&data, BLAKE3_HASH_LEN, 0).unwrap();
    assert!(restored.chunk_hashes.is_empty());
    assert_eq!(restored, record);
}

#[test]
fn roundtrip_empty_metadata() {
    let record = make_record("/meta.txt", None, 10, vec![vec![0x11; BLAKE3_HASH_LEN]]);
    assert!(record.metadata.is_empty());
    let data = record.serialize(BLAKE3_HASH_LEN).unwrap();
    let restored = FileRecord::deserialize(&data, BLAKE3_HASH_LEN, 0).unwrap();
    assert!(restored.metadata.is_empty());
}

#[test]
fn roundtrip_with_metadata() {
    let mut record = make_record("/meta.txt", None, 10, vec![vec![0x11; BLAKE3_HASH_LEN]]);
    record.metadata = b"some extra metadata bytes".to_vec();
    let data = record.serialize(BLAKE3_HASH_LEN).unwrap();
    let restored = FileRecord::deserialize(&data, BLAKE3_HASH_LEN, 0).unwrap();
    assert_eq!(restored.metadata, b"some extra metadata bytes");
    assert_eq!(restored, record);
}

#[test]
fn roundtrip_unicode_path() {
    let record = make_record(
        "/\u{30D5}\u{30A1}\u{30A4}\u{30EB}/\u{30C6}\u{30B9}\u{30C8}.json",
        Some("application/json"),
        77,
        vec![vec![0xCC; BLAKE3_HASH_LEN]],
    );
    let data = record.serialize(BLAKE3_HASH_LEN).unwrap();
    let restored = FileRecord::deserialize(&data, BLAKE3_HASH_LEN, 0).unwrap();
    assert_eq!(restored.path, record.path);
}

#[test]
fn roundtrip_total_size_zero() {
    let record = make_record("/zero.txt", Some("text/plain"), 0, vec![]);
    let data = record.serialize(BLAKE3_HASH_LEN).unwrap();
    let restored = FileRecord::deserialize(&data, BLAKE3_HASH_LEN, 0).unwrap();
    assert_eq!(restored.total_size, 0);
}

#[test]
fn roundtrip_total_size_max_u64() {
    let record = make_record("/huge.bin", None, u64::MAX, vec![vec![0xFF; BLAKE3_HASH_LEN]]);
    let data = record.serialize(BLAKE3_HASH_LEN).unwrap();
    let restored = FileRecord::deserialize(&data, BLAKE3_HASH_LEN, 0).unwrap();
    assert_eq!(restored.total_size, u64::MAX);
}

// ===========================================================================
// Path length validation (C2 fix)
// ===========================================================================

#[test]
fn serialize_rejects_path_exceeding_u16_max() {
    let long_path = "/".to_string() + &"x".repeat(u16::MAX as usize + 1);
    let record = make_record(&long_path, None, 0, vec![]);
    let result = record.serialize(BLAKE3_HASH_LEN);
    assert!(result.is_err());
}

#[test]
fn serialize_accepts_path_at_u16_max() {
    // Path of exactly u16::MAX bytes (including leading /)
    let path = "/".to_string() + &"a".repeat(u16::MAX as usize - 1);
    assert_eq!(path.len(), u16::MAX as usize);
    let record = make_record(&path, None, 0, vec![]);
    let result = record.serialize(BLAKE3_HASH_LEN);
    assert!(result.is_ok());
}

#[test]
fn serialize_rejects_content_type_exceeding_u16_max() {
    let long_ct = "x".repeat(u16::MAX as usize + 1);
    let record = make_record("/f.txt", Some(&long_ct), 0, vec![]);
    let result = record.serialize(BLAKE3_HASH_LEN);
    assert!(result.is_err());
}

// ===========================================================================
// Deserialization error paths
// ===========================================================================

#[test]
fn deserialize_empty_data_fails() {
    let result = FileRecord::deserialize(&[], BLAKE3_HASH_LEN, 0);
    assert!(result.is_err());
}

#[test]
fn deserialize_truncated_path_length_fails() {
    // Only 1 byte -- need at least 2 for path_length u16.
    let result = FileRecord::deserialize(&[0x00], BLAKE3_HASH_LEN, 0);
    assert!(result.is_err());
}

#[test]
fn deserialize_truncated_path_data_fails() {
    // Path length says 10, but only 2 bytes follow.
    let mut data = Vec::new();
    data.extend_from_slice(&10u16.to_le_bytes()); // path_length = 10
    data.extend_from_slice(&[0x41, 0x42]); // only 2 bytes of path
    let result = FileRecord::deserialize(&data, BLAKE3_HASH_LEN, 0);
    assert!(result.is_err());
}

#[test]
fn deserialize_truncated_after_path_fails() {
    // Valid path, but no content_type_length follows.
    let mut data = Vec::new();
    let path = b"/test";
    data.extend_from_slice(&(path.len() as u16).to_le_bytes());
    data.extend_from_slice(path);
    // No more data.
    let result = FileRecord::deserialize(&data, BLAKE3_HASH_LEN, 0);
    assert!(result.is_err());
}

#[test]
fn deserialize_invalid_utf8_path_fails() {
    let mut data = Vec::new();
    let bad_bytes: &[u8] = &[0xFF, 0xFE]; // invalid UTF-8
    data.extend_from_slice(&(bad_bytes.len() as u16).to_le_bytes());
    data.extend_from_slice(bad_bytes);
    let result = FileRecord::deserialize(&data, BLAKE3_HASH_LEN, 0);
    assert!(result.is_err());
}

#[test]
fn deserialize_truncated_chunk_hashes_fails() {
    // Serialize a valid record, then truncate in the chunk hash area.
    let record = make_record("/f.txt", None, 10, vec![vec![0xAA; BLAKE3_HASH_LEN], vec![0xBB; BLAKE3_HASH_LEN]]);
    let full_data = record.serialize(BLAKE3_HASH_LEN).unwrap();
    // Remove last 10 bytes so the second hash is incomplete.
    let truncated = &full_data[..full_data.len() - 10];
    let result = FileRecord::deserialize(truncated, BLAKE3_HASH_LEN, 0);
    assert!(result.is_err());
}

// ===========================================================================
// new() constructor
// ===========================================================================

#[test]
fn new_sets_timestamps() {
    let record = FileRecord::new("/now.txt".to_string(), None, 0, vec![]);
    // Timestamps should be recent (within last 5 seconds).
    let now_ms = chrono::Utc::now().timestamp_millis();
    assert!((now_ms - record.created_at).abs() < 5000);
    assert!((now_ms - record.updated_at).abs() < 5000);
    assert_eq!(record.created_at, record.updated_at);
}

#[test]
fn new_metadata_is_empty() {
    let record = FileRecord::new("/m.txt".to_string(), None, 0, vec![]);
    assert!(record.metadata.is_empty());
}

// ===========================================================================
// Version dispatch
// ===========================================================================

#[test]
fn deserialize_version_0_works() {
    let record = make_record("/v0.txt", Some("text/plain"), 100, vec![vec![0x12; BLAKE3_HASH_LEN]]);
    let data = record.serialize(BLAKE3_HASH_LEN).unwrap();
    let restored = FileRecord::deserialize(&data, BLAKE3_HASH_LEN, 0).unwrap();
    assert_eq!(restored, record);
}

#[test]
fn deserialize_unknown_version_returns_error() {
    // Unknown entry versions must produce a hard error, not silently fall
    // back to v0 deserialization (which would risk misinterpreting future
    // on-disk layouts).
    let record = make_record("/vX.txt", None, 50, vec![vec![0x34; BLAKE3_HASH_LEN]]);
    let data = record.serialize(BLAKE3_HASH_LEN).unwrap();
    let err = FileRecord::deserialize(&data, BLAKE3_HASH_LEN, 99).unwrap_err();
    assert!(
        matches!(err, aeordb::engine::errors::EngineError::InvalidEntryVersion(99)),
        "expected InvalidEntryVersion(99), got {:?}",
        err
    );
}

// ===========================================================================
// Different hash lengths
// ===========================================================================

#[test]
fn roundtrip_with_sha512_hash_length() {
    let sha512_len = 64;
    let hash = vec![0xDD; sha512_len];
    let mut record = make_record("/sha512.txt", Some("text/plain"), 256, vec![hash]);
    // Ensure metadata is pinned too.
    record.metadata = vec![];
    let data = record.serialize(sha512_len).unwrap();
    let restored = FileRecord::deserialize(&data, sha512_len, 0).unwrap();
    assert_eq!(restored, record);
    assert_eq!(restored.chunk_hashes[0].len(), 64);
}
