use aeordb::engine::symlink_record::{SymlinkRecord, symlink_path_hash, symlink_content_hash};
use aeordb::engine::hash_algorithm::HashAlgorithm;

#[test]
fn test_serialize_deserialize_roundtrip() {
    let record = SymlinkRecord::new("/link".to_string(), "/target/file.txt".to_string());
    let data = record.serialize();
    let restored = SymlinkRecord::deserialize(&data).unwrap();
    assert_eq!(restored.path, record.path);
    assert_eq!(restored.target, record.target);
    assert_eq!(restored.created_at, record.created_at);
    assert_eq!(restored.updated_at, record.updated_at);
}

#[test]
fn test_field_preservation() {
    let mut record = SymlinkRecord::new("/my/link".to_string(), "/some/deep/target".to_string());
    record.created_at = 1000;
    record.updated_at = 2000;
    let data = record.serialize();
    let restored = SymlinkRecord::deserialize(&data).unwrap();
    assert_eq!(restored.path, "/my/link");
    assert_eq!(restored.target, "/some/deep/target");
    assert_eq!(restored.created_at, 1000);
    assert_eq!(restored.updated_at, 2000);
}

#[test]
fn test_empty_target() {
    let record = SymlinkRecord::new("/link".to_string(), "".to_string());
    let data = record.serialize();
    let restored = SymlinkRecord::deserialize(&data).unwrap();
    assert_eq!(restored.target, "");
}

#[test]
fn test_unicode_paths() {
    let record = SymlinkRecord::new("/\u{30EA}\u{30F3}\u{30AF}".to_string(), "/\u{76EE}\u{6A19}/\u{30D5}\u{30A1}\u{30A4}\u{30EB}.txt".to_string());
    let data = record.serialize();
    let restored = SymlinkRecord::deserialize(&data).unwrap();
    assert_eq!(restored.path, "/\u{30EA}\u{30F3}\u{30AF}");
    assert_eq!(restored.target, "/\u{76EE}\u{6A19}/\u{30D5}\u{30A1}\u{30A4}\u{30EB}.txt");
}

#[test]
fn test_hash_functions() {
    let algo = HashAlgorithm::Blake3_256;
    let hash1 = symlink_path_hash("/link", &algo).unwrap();
    let hash2 = symlink_content_hash(b"some data", &algo).unwrap();
    assert!(!hash1.is_empty());
    assert!(!hash2.is_empty());
    assert_ne!(hash1, hash2);
}

#[test]
fn test_deserialize_too_short() {
    // Less than 4 bytes should fail
    let result = SymlinkRecord::deserialize(&[0x00, 0x01]);
    assert!(result.is_err());
}

#[test]
fn test_deserialize_truncated_path() {
    // Claim path is 10 bytes but only provide 2
    let data = vec![0x0A, 0x00, 0x41, 0x42];
    let result = SymlinkRecord::deserialize(&data);
    assert!(result.is_err());
}

#[test]
fn test_deserialize_truncated_target_and_timestamps() {
    // Valid path (length 1, byte 'A'), then target length 1, byte 'B', but no timestamps
    let data = vec![
        0x01, 0x00, b'A',       // path: "A"
        0x01, 0x00, b'B',       // target: "B"
        // missing 16 bytes of timestamps
    ];
    let result = SymlinkRecord::deserialize(&data);
    assert!(result.is_err());
}

#[test]
fn test_path_hash_deterministic() {
    let algo = HashAlgorithm::Blake3_256;
    let h1 = symlink_path_hash("/foo", &algo).unwrap();
    let h2 = symlink_path_hash("/foo", &algo).unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn test_path_hash_differs_for_different_paths() {
    let algo = HashAlgorithm::Blake3_256;
    let h1 = symlink_path_hash("/foo", &algo).unwrap();
    let h2 = symlink_path_hash("/bar", &algo).unwrap();
    assert_ne!(h1, h2);
}

#[test]
fn test_content_hash_deterministic() {
    let algo = HashAlgorithm::Blake3_256;
    let h1 = symlink_content_hash(b"data", &algo).unwrap();
    let h2 = symlink_content_hash(b"data", &algo).unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn test_content_hash_differs_for_different_data() {
    let algo = HashAlgorithm::Blake3_256;
    let h1 = symlink_content_hash(b"data1", &algo).unwrap();
    let h2 = symlink_content_hash(b"data2", &algo).unwrap();
    assert_ne!(h1, h2);
}

#[test]
fn test_empty_path() {
    let record = SymlinkRecord::new("".to_string(), "/target".to_string());
    let data = record.serialize();
    let restored = SymlinkRecord::deserialize(&data).unwrap();
    assert_eq!(restored.path, "");
    assert_eq!(restored.target, "/target");
}

#[test]
fn test_max_u16_boundary_path() {
    // Path exactly at u16 max would be 65535 bytes — just test a reasonably large path
    let long_path = "/".to_string() + &"a".repeat(1000);
    let record = SymlinkRecord::new(long_path.clone(), "/t".to_string());
    let data = record.serialize();
    let restored = SymlinkRecord::deserialize(&data).unwrap();
    assert_eq!(restored.path, long_path);
}
