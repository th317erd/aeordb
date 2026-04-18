use aeordb::engine::deletion_record::DeletionRecord;

/// Build a DeletionRecord with a fixed timestamp for deterministic round-trips.
fn make_record(path: &str, reason: Option<&str>) -> DeletionRecord {
    let mut rec = DeletionRecord::new(path.to_string(), reason.map(|s| s.to_string()));
    rec.deleted_at = 1_700_000_000_000;
    rec
}

// ===========================================================================
// new() constructor
// ===========================================================================

#[test]
fn new_sets_path_and_reason() {
    let rec = DeletionRecord::new("/to_delete.txt".to_string(), Some("expired".to_string()));
    assert_eq!(rec.path, "/to_delete.txt");
    assert_eq!(rec.reason, Some("expired".to_string()));
}

#[test]
fn new_with_no_reason() {
    let rec = DeletionRecord::new("/gone.txt".to_string(), None);
    assert_eq!(rec.path, "/gone.txt");
    assert_eq!(rec.reason, None);
}

#[test]
fn new_sets_deleted_at_to_current_time() {
    let rec = DeletionRecord::new("/timed.txt".to_string(), None);
    let now_ms = chrono::Utc::now().timestamp_millis();
    assert!((now_ms - rec.deleted_at).abs() < 5000, "deleted_at should be within 5 seconds of now");
}

// ===========================================================================
// Serialize / Deserialize round-trip — happy paths
// ===========================================================================

#[test]
fn roundtrip_with_reason() {
    let record = make_record("/deleted.txt", Some("user request"));
    let data = record.serialize();
    let restored = DeletionRecord::deserialize(&data).unwrap();
    assert_eq!(restored, record);
}

#[test]
fn roundtrip_without_reason() {
    let record = make_record("/deleted.txt", None);
    let data = record.serialize();
    let restored = DeletionRecord::deserialize(&data).unwrap();
    assert_eq!(restored, record);
    assert_eq!(restored.reason, None);
}

#[test]
fn roundtrip_empty_reason_becomes_none() {
    // An empty reason string serializes to a zero-length reason,
    // which deserializes as None.
    let mut record = make_record("/x.txt", Some(""));
    // With Some(""), serialize writes 0-length reason bytes.
    let data = record.serialize();
    let restored = DeletionRecord::deserialize(&data).unwrap();
    // Empty reason deserializes as None per the implementation.
    assert_eq!(restored.reason, None);
    // Adjust for equality check.
    record.reason = None;
    assert_eq!(restored, record);
}

#[test]
fn roundtrip_unicode_path() {
    let record = make_record(
        "/\u{30D5}\u{30A1}\u{30A4}\u{30EB}/\u{524A}\u{9664}.txt",
        Some("\u{671F}\u{9650}\u{5207}\u{308C}"),
    );
    let data = record.serialize();
    let restored = DeletionRecord::deserialize(&data).unwrap();
    assert_eq!(restored, record);
}

#[test]
fn roundtrip_long_reason() {
    let reason = "x".repeat(10_000);
    let record = make_record("/big_reason.txt", Some(&reason));
    let data = record.serialize();
    let restored = DeletionRecord::deserialize(&data).unwrap();
    assert_eq!(restored.reason.as_deref(), Some(reason.as_str()));
}

#[test]
fn roundtrip_preserves_deleted_at() {
    let mut record = make_record("/ts.txt", None);
    record.deleted_at = 9_876_543_210;
    let data = record.serialize();
    let restored = DeletionRecord::deserialize(&data).unwrap();
    assert_eq!(restored.deleted_at, 9_876_543_210);
}

#[test]
fn roundtrip_negative_timestamp() {
    let mut record = make_record("/neg.txt", None);
    record.deleted_at = -1_000_000;
    let data = record.serialize();
    let restored = DeletionRecord::deserialize(&data).unwrap();
    assert_eq!(restored.deleted_at, -1_000_000);
}

#[test]
fn roundtrip_root_path() {
    let record = make_record("/", None);
    let data = record.serialize();
    let restored = DeletionRecord::deserialize(&data).unwrap();
    assert_eq!(restored.path, "/");
}

#[test]
fn roundtrip_deeply_nested_path() {
    let path = "/a/b/c/d/e/f/g/h/i/j/k/l/m/n.txt";
    let record = make_record(path, Some("cleanup"));
    let data = record.serialize();
    let restored = DeletionRecord::deserialize(&data).unwrap();
    assert_eq!(restored.path, path);
}

// ===========================================================================
// Deserialization error paths
// ===========================================================================

#[test]
fn deserialize_empty_data_fails() {
    let result = DeletionRecord::deserialize(&[]);
    assert!(result.is_err());
}

#[test]
fn deserialize_single_byte_fails() {
    let result = DeletionRecord::deserialize(&[0x00]);
    assert!(result.is_err());
}

#[test]
fn deserialize_truncated_path_fails() {
    // Path length says 20, but only 3 bytes of path present.
    let mut data = Vec::new();
    data.extend_from_slice(&20u16.to_le_bytes());
    data.extend_from_slice(b"abc");
    let result = DeletionRecord::deserialize(&data);
    assert!(result.is_err());
}

#[test]
fn deserialize_truncated_after_path_fails() {
    // Valid path, but no deleted_at field follows.
    let mut data = Vec::new();
    let path = b"/test";
    data.extend_from_slice(&(path.len() as u16).to_le_bytes());
    data.extend_from_slice(path);
    let result = DeletionRecord::deserialize(&data);
    assert!(result.is_err());
}

#[test]
fn deserialize_truncated_deleted_at_fails() {
    // Valid path, partial deleted_at (only 4 bytes of needed 8).
    let mut data = Vec::new();
    let path = b"/test";
    data.extend_from_slice(&(path.len() as u16).to_le_bytes());
    data.extend_from_slice(path);
    data.extend_from_slice(&[0x00; 4]); // only 4 bytes of i64
    let result = DeletionRecord::deserialize(&data);
    assert!(result.is_err());
}

#[test]
fn deserialize_truncated_reason_length_fails() {
    // Valid path + deleted_at, but no reason_length.
    let mut data = Vec::new();
    let path = b"/test";
    data.extend_from_slice(&(path.len() as u16).to_le_bytes());
    data.extend_from_slice(path);
    data.extend_from_slice(&0i64.to_le_bytes());
    // No reason_length follows.
    let result = DeletionRecord::deserialize(&data);
    assert!(result.is_err());
}

#[test]
fn deserialize_truncated_reason_data_fails() {
    // Valid path + deleted_at + reason_length=10, but only 3 bytes of reason.
    let mut data = Vec::new();
    let path = b"/test";
    data.extend_from_slice(&(path.len() as u16).to_le_bytes());
    data.extend_from_slice(path);
    data.extend_from_slice(&0i64.to_le_bytes());
    data.extend_from_slice(&10u16.to_le_bytes()); // reason_length = 10
    data.extend_from_slice(b"abc"); // only 3 bytes
    let result = DeletionRecord::deserialize(&data);
    assert!(result.is_err());
}

#[test]
fn deserialize_invalid_utf8_path_fails() {
    let mut data = Vec::new();
    let bad_bytes: &[u8] = &[0xFF, 0xFE];
    data.extend_from_slice(&(bad_bytes.len() as u16).to_le_bytes());
    data.extend_from_slice(bad_bytes);
    // Even if we add the rest, the path should fail UTF-8 validation.
    data.extend_from_slice(&0i64.to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    let result = DeletionRecord::deserialize(&data);
    assert!(result.is_err());
}

#[test]
fn deserialize_invalid_utf8_reason_fails() {
    let mut data = Vec::new();
    let path = b"/ok";
    data.extend_from_slice(&(path.len() as u16).to_le_bytes());
    data.extend_from_slice(path);
    data.extend_from_slice(&0i64.to_le_bytes());
    let bad_reason: &[u8] = &[0xFF, 0xFE, 0x80];
    data.extend_from_slice(&(bad_reason.len() as u16).to_le_bytes());
    data.extend_from_slice(bad_reason);
    let result = DeletionRecord::deserialize(&data);
    assert!(result.is_err());
}

// ===========================================================================
// Serialized buffer structure
// ===========================================================================

#[test]
fn serialize_capacity_is_exact() {
    let record = make_record("/exact.txt", Some("reason"));
    let data = record.serialize();
    let path_bytes = record.path.as_bytes();
    let reason_bytes = record.reason.as_deref().unwrap_or("").as_bytes();
    let expected_len = 2 + path_bytes.len() + 8 + 2 + reason_bytes.len();
    assert_eq!(data.len(), expected_len);
}

#[test]
fn serialize_no_reason_capacity_is_exact() {
    let record = make_record("/noreason.txt", None);
    let data = record.serialize();
    let path_bytes = record.path.as_bytes();
    let expected_len = 2 + path_bytes.len() + 8 + 2; // reason_length = 0, no reason bytes
    assert_eq!(data.len(), expected_len);
}
