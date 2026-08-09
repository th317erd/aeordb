use aeordb::engine::errors::EngineError;
use aeordb::engine::file_header::{FileHeader, FILE_HEADER_SIZE, write_initial_header};
use aeordb::engine::hash_algorithm::HashAlgorithm;
use aeordb::engine::storage_engine::StorageEngine;
use tempfile::TempDir;

fn db_path(dir: &TempDir, name: &str) -> String {
  dir.path().join(name).to_str().unwrap().to_string()
}

// ============================================================
// FileHeader backup field tests
// ============================================================

#[test]
fn test_file_header_backup_type_default() {
  let header = FileHeader::new(HashAlgorithm::Blake3_256);
  assert_eq!(header.backup_type, 0);
}

#[test]
fn test_file_header_base_hash_default() {
  let header = FileHeader::new(HashAlgorithm::Blake3_256);
  assert_eq!(header.base_hash, vec![0u8; 32]);
}

#[test]
fn test_file_header_target_hash_default() {
  let header = FileHeader::new(HashAlgorithm::Blake3_256);
  assert_eq!(header.target_hash, vec![0u8; 32]);
}

#[test]
fn test_file_header_serialize_deserialize_backup_fields() {
  let mut header = FileHeader::new(HashAlgorithm::Blake3_256);
  header.backup_type = 1;
  header.base_hash = vec![0xAA; 32];
  header.target_hash = vec![0xBB; 32];

  let serialized = header.serialize().unwrap();
  let deserialized = FileHeader::deserialize(&serialized).unwrap();

  assert_eq!(deserialized.backup_type, 1);
  assert_eq!(deserialized.base_hash, vec![0xAA; 32]);
  assert_eq!(deserialized.target_hash, vec![0xBB; 32]);
}

#[test]
fn test_file_header_serialize_deserialize_patch() {
  let mut header = FileHeader::new(HashAlgorithm::Blake3_256);
  header.backup_type = 2;
  header.base_hash = (0..32).collect::<Vec<u8>>();
  header.target_hash = (32..64).collect::<Vec<u8>>();

  let serialized = header.serialize().unwrap();
  let deserialized = FileHeader::deserialize(&serialized).unwrap();

  assert_eq!(deserialized.backup_type, 2);
  assert_eq!(deserialized.base_hash, (0..32).collect::<Vec<u8>>());
  assert_eq!(deserialized.target_hash, (32..64).collect::<Vec<u8>>());
}

#[test]
fn test_file_header_backward_compat() {
  // Simulate old format: serialize a default header, then zero out the
  // backup fields area (they're already zero by default, so just verify
  // deserialization produces the expected defaults).
  let header = FileHeader::new(HashAlgorithm::Blake3_256);
  let serialized = header.serialize().unwrap();
  let deserialized = FileHeader::deserialize(&serialized).unwrap();

  assert_eq!(deserialized.backup_type, 0);
  assert_eq!(deserialized.base_hash, vec![0u8; 32]);
  assert_eq!(deserialized.target_hash, vec![0u8; 32]);
}

#[test]
fn test_file_header_serialize_fits_in_256_bytes_blake3() {
  // With BLAKE3_256 (32 bytes), backup fields fit comfortably in 256.
  let mut header = FileHeader::new(HashAlgorithm::Blake3_256);
  header.backup_type = 2;
  header.base_hash = vec![0xFF; 32];
  header.target_hash = vec![0xEE; 32];

  let serialized = header.serialize().unwrap();
  assert_eq!(serialized.len(), FILE_HEADER_SIZE);

  // Round-trip
  let deserialized = FileHeader::deserialize(&serialized).unwrap();
  assert_eq!(deserialized.backup_type, 2);
  assert_eq!(deserialized.base_hash, vec![0xFF; 32]);
  assert_eq!(deserialized.target_hash, vec![0xEE; 32]);
}

#[test]
fn test_file_header_serialize_fits_in_256_bytes_sha256() {
  // SHA-256 also has 32-byte hashes, should fit fine.
  let mut header = FileHeader::new(HashAlgorithm::Sha256);
  header.backup_type = 1;
  header.base_hash = vec![0xAB; 32];
  header.target_hash = vec![0xCD; 32];

  let serialized = header.serialize().unwrap();
  assert_eq!(serialized.len(), FILE_HEADER_SIZE);

  let deserialized = FileHeader::deserialize(&serialized).unwrap();
  assert_eq!(deserialized.backup_type, 1);
  assert_eq!(deserialized.base_hash, vec![0xAB; 32]);
  assert_eq!(deserialized.target_hash, vec![0xCD; 32]);
}

#[test]
fn v3_file_header_rejects_hash_algorithms_that_do_not_fit_its_fixed_slot() {
  for algorithm in [HashAlgorithm::Sha512, HashAlgorithm::Sha3_512] {
    let error = FileHeader::new(algorithm).serialize().unwrap_err();
    assert!(error.to_string().contains("64-byte"), "{error}");
  }
}

#[test]
fn v3_file_header_serializer_rejects_non_current_layout_versions() {
  for version in [0, 1, 2, 4, u8::MAX] {
    let mut header = FileHeader::new(HashAlgorithm::Blake3_256);
    header.header_version = version;

    let error = header.serialize().unwrap_err();

    assert!(matches!(error, EngineError::InvalidEntryVersion(actual) if actual == version));
  }
}

#[test]
fn v3_header_publication_does_not_mutate_file_or_sequence_when_serialization_fails() {
  let mut header = FileHeader::new(HashAlgorithm::Sha512);
  let mut file = tempfile::tempfile().unwrap();

  let error = write_initial_header(&mut file, &mut header).unwrap_err();

  assert!(error.to_string().contains("64-byte"), "{error}");
  assert_eq!(header.sequence, 0);
  assert_eq!(file.metadata().unwrap().len(), 0);
}

#[test]
fn v3_file_header_rejects_wrong_sized_hash_fields_before_serialization() {
  for field in ["head", "base", "target"] {
    let mut header = FileHeader::new(HashAlgorithm::Blake3_256);
    match field {
      "head" => {
        header.head_hash.pop();
      }
      "base" => {
        header.base_hash.push(0);
      }
      "target" => {
        header.target_hash.clear();
      }
      _ => unreachable!(),
    }

    let error = header.serialize().unwrap_err();
    assert!(error.to_string().contains(field), "{error}");
  }
}

#[test]
fn v3_file_header_deserializer_rejects_a_valid_crc_with_an_unsupported_hash_width() {
  let mut bytes = FileHeader::new(HashAlgorithm::Blake3_256).serialize().unwrap();
  bytes[5..7].copy_from_slice(&HashAlgorithm::Sha512.to_u16().to_le_bytes());
  let checksum = crc32fast::hash(&bytes[..FILE_HEADER_SIZE - 4]);
  bytes[FILE_HEADER_SIZE - 4..].copy_from_slice(&checksum.to_le_bytes());

  let error = FileHeader::deserialize(&bytes).unwrap_err();

  assert!(error.to_string().contains("64-byte"), "{error}");
}

#[test]
fn test_file_header_preserves_existing_fields_with_backup() {
  let mut header = FileHeader::new(HashAlgorithm::Blake3_256);
  header.entry_count = 42;
  header.kv_block_offset = 1000;
  header.backup_type = 1;
  header.base_hash = vec![0x11; 32];
  header.target_hash = vec![0x22; 32];

  let serialized = header.serialize().unwrap();
  let deserialized = FileHeader::deserialize(&serialized).unwrap();

  assert_eq!(deserialized.entry_count, 42);
  assert_eq!(deserialized.kv_block_offset, 1000);
  assert_eq!(deserialized.backup_type, 1);
  assert_eq!(deserialized.base_hash, vec![0x11; 32]);
  assert_eq!(deserialized.target_hash, vec![0x22; 32]);
}

// ============================================================
// StorageEngine open guard tests
// ============================================================

#[test]
fn test_open_normal_database() {
  let dir = TempDir::new().unwrap();
  let path = db_path(&dir, "normal.aeor");

  let engine = StorageEngine::create(&path).unwrap();
  // backup_type defaults to 0
  drop(engine);

  let result = StorageEngine::open(&path);
  assert!(result.is_ok(), "Normal database (backup_type=0) should open fine");
}

#[test]
fn test_open_full_export() {
  let dir = TempDir::new().unwrap();
  let path = db_path(&dir, "full_export.aeor");

  let engine = StorageEngine::create(&path).unwrap();
  engine.set_backup_info(1, &[0xAA; 32], &[0xBB; 32]).unwrap();
  drop(engine);

  let result = StorageEngine::open(&path);
  assert!(result.is_ok(), "Full export (backup_type=1) should open fine");
}

#[test]
fn test_open_patch_fails() {
  let dir = TempDir::new().unwrap();
  let path = db_path(&dir, "patch.aeor");

  let engine = StorageEngine::create(&path).unwrap();
  engine.set_backup_info(2, &[0xAA; 32], &[0xBB; 32]).unwrap();
  drop(engine);

  let result = StorageEngine::open(&path);
  assert!(result.is_err(), "Patch database (backup_type=2) should fail to open");
}

#[test]
fn test_open_patch_error_message() {
  let dir = TempDir::new().unwrap();
  let path = db_path(&dir, "patch_msg.aeor");

  let base_hash = vec![0xAA; 32];
  let target_hash = vec![0xBB; 32];

  let engine = StorageEngine::create(&path).unwrap();
  engine.set_backup_info(2, &base_hash, &target_hash).unwrap();
  drop(engine);

  let err = match StorageEngine::open(&path) {
    Err(e) => e,
    Ok(_) => panic!("Expected error but open succeeded"),
  };
  let msg = format!("{}", err);

  assert!(msg.contains(&hex::encode(&base_hash)), "Error should contain base hash hex");
  assert!(msg.contains(&hex::encode(&target_hash)), "Error should contain target hash hex");
  assert!(msg.contains("patch"), "Error should mention patch");
  assert!(msg.contains("import"), "Error should mention import");
}

#[test]
fn test_open_patch_returns_patch_database_variant() {
  let dir = TempDir::new().unwrap();
  let path = db_path(&dir, "patch_variant.aeor");

  let engine = StorageEngine::create(&path).unwrap();
  engine.set_backup_info(2, &[0xCC; 32], &[0xDD; 32]).unwrap();
  drop(engine);

  let err = match StorageEngine::open(&path) {
    Err(e) => e,
    Ok(_) => panic!("Expected error but open succeeded"),
  };
  assert!(matches!(err, EngineError::PatchDatabase(_)), "Expected PatchDatabase error variant, got: {:?}", err);
}

#[test]
fn test_open_for_import_patch_succeeds() {
  let dir = TempDir::new().unwrap();
  let path = db_path(&dir, "import_patch.aeor");

  let engine = StorageEngine::create(&path).unwrap();
  engine.set_backup_info(2, &[0xAA; 32], &[0xBB; 32]).unwrap();
  drop(engine);

  let result = StorageEngine::open_for_import(&path);
  assert!(result.is_ok(), "open_for_import should accept patch databases");
}

#[test]
fn test_open_for_import_normal() {
  let dir = TempDir::new().unwrap();
  let path = db_path(&dir, "import_normal.aeor");

  let engine = StorageEngine::create(&path).unwrap();
  drop(engine);

  let result = StorageEngine::open_for_import(&path);
  assert!(result.is_ok(), "open_for_import should accept normal databases");
}

#[test]
fn test_backup_info_returns_correct_values() {
  let dir = TempDir::new().unwrap();
  let path = db_path(&dir, "backup_info.aeor");

  let base_hash = vec![0x11; 32];
  let target_hash = vec![0x22; 32];

  let engine = StorageEngine::create(&path).unwrap();
  engine.set_backup_info(1, &base_hash, &target_hash).unwrap();

  let (bt, bh, th) = engine.backup_info().unwrap();
  assert_eq!(bt, 1);
  assert_eq!(bh, base_hash);
  assert_eq!(th, target_hash);
}

#[test]
fn test_backup_info_defaults_on_new_db() {
  let dir = TempDir::new().unwrap();
  let path = db_path(&dir, "backup_defaults.aeor");

  let engine = StorageEngine::create(&path).unwrap();
  let (bt, bh, th) = engine.backup_info().unwrap();

  assert_eq!(bt, 0);
  assert_eq!(bh, vec![0u8; 32]);
  assert_eq!(th, vec![0u8; 32]);
}

#[test]
fn test_set_backup_info_persists_across_reopen() {
  let dir = TempDir::new().unwrap();
  let path = db_path(&dir, "persist.aeor");

  let base_hash = vec![0xDE; 32];
  let target_hash = vec![0xAD; 32];

  let engine = StorageEngine::create(&path).unwrap();
  engine.set_backup_info(1, &base_hash, &target_hash).unwrap();
  drop(engine);

  let engine = StorageEngine::open(&path).unwrap();
  let (bt, bh, th) = engine.backup_info().unwrap();
  assert_eq!(bt, 1);
  assert_eq!(bh, base_hash);
  assert_eq!(th, target_hash);
}

#[test]
fn test_open_for_import_preserves_backup_info() {
  let dir = TempDir::new().unwrap();
  let path = db_path(&dir, "import_info.aeor");

  let base_hash = vec![0xCA; 32];
  let target_hash = vec![0xFE; 32];

  let engine = StorageEngine::create(&path).unwrap();
  engine.set_backup_info(2, &base_hash, &target_hash).unwrap();
  drop(engine);

  let engine = StorageEngine::open_for_import(&path).unwrap();
  let (bt, bh, th) = engine.backup_info().unwrap();
  assert_eq!(bt, 2);
  assert_eq!(bh, base_hash);
  assert_eq!(th, target_hash);
}

#[test]
fn test_open_high_backup_type_also_rejected() {
  // backup_type=255 should also be rejected (anything > 1)
  let dir = TempDir::new().unwrap();
  let path = db_path(&dir, "high_bt.aeor");

  let engine = StorageEngine::create(&path).unwrap();
  engine.set_backup_info(255, &[0x00; 32], &[0x00; 32]).unwrap();
  drop(engine);

  let result = StorageEngine::open(&path);
  assert!(result.is_err(), "backup_type=255 should be rejected by open()");

  // But open_for_import should work
  let result = StorageEngine::open_for_import(&path);
  assert!(result.is_ok(), "backup_type=255 should be accepted by open_for_import()");
}
