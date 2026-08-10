use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_SLOT_LENGTH, DatabaseHeaderV4, decode_header_region, encode_database_header_slot};
use aeordb::engine::v4::entity::{
  WHOLE_ENTITY_V1_KEY_CAP, WHOLE_ENTITY_V1_VALUE_CAP, WholeEntityWriteV1, checked_whole_entity_encoded_length, decode_whole_entity,
  encode_whole_entity,
};
use aeordb::engine::v4::reader::MalformedInputClass;
use aeordb::engine::{CompressionAlgorithm, HashAlgorithm};

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4")
}

fn header_fixture(name: &str) -> Vec<u8> {
  fs::read(fixture_root().join("database-header-v4").join(name)).expect("independent header fixture")
}

fn entity_fixture(name: &str) -> Vec<u8> {
  fs::read(fixture_root().join("whole-entity-v1").join(name)).expect("independent entity fixture")
}

#[test]
fn database_header_writer_matches_both_independent_hash_width_fixtures() {
  for name in ["header-blake3-256-valid-ab.bin", "header-sha512-valid-ab.bin"] {
    let region = header_fixture(name);
    let selected = decode_header_region(&region).expect("valid independent header region");
    let expected =
      &region[selected.selected_slot * DATABASE_HEADER_V4_SLOT_LENGTH..(selected.selected_slot + 1) * DATABASE_HEADER_V4_SLOT_LENGTH];

    let encoded = encode_database_header_slot(&selected.header).expect("fixture-derived header must encode");
    assert_eq!(encoded.as_slice(), expected, "fixture {name}");
  }
}

#[test]
fn whole_entity_writer_matches_both_independent_hash_width_fixtures() {
  for (name, algorithm) in [
    ("entity-blake3-256-directory-root-valid.bin", HashAlgorithm::Blake3_256),
    ("entity-sha512-directory-root-valid.bin", HashAlgorithm::Sha512),
  ] {
    let expected = entity_fixture(name);
    let decoded = decode_whole_entity(&expected, algorithm, u64::MAX).expect("valid independent whole-entity fixture");
    let request = WholeEntityWriteV1 {
      entry_type: decoded.entry_type,
      flags: decoded.flags,
      hash_algorithm: decoded.hash_algorithm,
      compression_algorithm: decoded.compression_algorithm,
      timestamp_ms: decoded.timestamp_ms,
      write_sequence: decoded.write_sequence,
      key: decoded.key,
      stored_value: decoded.stored_value,
    };

    let encoded = encode_whole_entity(&request).expect("fixture-derived whole entity must encode");
    assert_eq!(encoded, expected, "fixture {name}");
  }
}

#[test]
fn database_header_writer_rejects_identity_capability_hash_and_region_errors() {
  let region = header_fixture("header-blake3-256-valid-ab.bin");
  let valid = decode_header_region(&region).unwrap().header;

  assert_header_error(mutate_header(&valid, |header| header.database_id = [0; 16]), "zero_identity");
  assert_header_error(mutate_header(&valid, |header| header.physical_instance_id = [0; 16]), "zero_identity");
  assert_header_error(mutate_header(&valid, |header| header.slot_sequence = 0), "zero_header_sequence");
  assert_header_error(mutate_header(&valid, |header| header.write_sequence_high_water = 0), "zero_header_sequence");
  assert_header_error(mutate_header(&valid, |header| header.writer_fence_epoch = 0), "zero_registry_or_fence");
  assert_header_error(mutate_header(&valid, |header| header.system_family_registry_version = 0), "zero_registry_or_fence");
  assert_header_error(mutate_header(&valid, |header| header.required_reader_capabilities[3] = 1), "unsupported_required_capability");
  assert_header_error(
    mutate_header(&valid, |header| {
      header.head_hash.pop();
    }),
    "hash_width",
  );
  assert_header_error(mutate_header(&valid, |header| header.kv_block_version = 2), "unsupported_region_version");
  assert_header_error(mutate_header(&valid, |header| header.nvt_version = 2), "unsupported_region_version");
  assert_header_error(mutate_header(&valid, |header| header.kv_block_stage = 10), "kv_stage");
  assert_header_error(mutate_header(&valid, |header| header.resize_target_stage = 10), "resize_target_stage");
  assert_header_error(mutate_header(&valid, |header| header.resize_target_stage = 1), "resize_state");
  assert_header_error(
    mutate_header(&valid, |header| {
      header.resize_in_progress = true;
      header.resize_target_stage = 0;
    }),
    "resize_state",
  );
  assert_header_error(mutate_header(&valid, |header| header.kv_block_length += 1), "kv_stage_length");
  assert_header_error(mutate_header(&valid, |header| header.backup_type = 3), "backup_type");
  assert_header_error(mutate_header(&valid, |header| header.kv_block_offset = 1), "region_overlap");
  assert_header_error(mutate_header(&valid, |header| header.kv_block_length = u64::MAX), "offset_overflow");
}

#[test]
fn database_header_reader_rejects_crc_valid_unchecked_stage_state() {
  for (offset, value, expected_code) in [(107, 10, "kv_stage"), (109, 10, "resize_target_stage"), (109, 1, "resize_state")] {
    let mut region = header_fixture("header-blake3-256-valid-ab.bin");
    for slot_index in 0..2 {
      let slot_start = slot_index * DATABASE_HEADER_V4_SLOT_LENGTH;
      region[slot_start + offset] = value;
      let crc_offset = slot_start + DATABASE_HEADER_V4_SLOT_LENGTH - 4;
      let crc = crc32fast::hash(&region[slot_start..crc_offset]);
      region[crc_offset..crc_offset + 4].copy_from_slice(&crc.to_le_bytes());
    }
    assert_eq!(decode_header_region(&region).unwrap_err().code(), expected_code);
  }
}

#[test]
fn whole_entity_writer_rejects_invalid_flags_and_unreserved_sequence() {
  let request = WholeEntityWriteV1 {
    entry_type: aeordb::engine::v4::entity::EntryTypeV4::FileRecord,
    flags: 0,
    hash_algorithm: HashAlgorithm::Blake3_256,
    compression_algorithm: CompressionAlgorithm::None,
    timestamp_ms: 1_700_000_000_000,
    write_sequence: 1,
    key: b"key",
    stored_value: b"value",
  };

  let mut invalid_flags = request.clone();
  invalid_flags.flags = 0x80;
  let error = encode_whole_entity(&invalid_flags).unwrap_err();
  assert_eq!(error.class(), MalformedInputClass::UnknownTypeKindOrEnum);
  assert_eq!(error.code(), "unknown_entity_flags");

  let mut zero_sequence = request.clone();
  zero_sequence.write_sequence = 0;
  let error = encode_whole_entity(&zero_sequence).unwrap_err();
  assert_eq!(error.class(), MalformedInputClass::IdentityKeyOrGenerationMismatch);
  assert_eq!(error.code(), "unreserved_write_sequence");

  let encoded = encode_whole_entity(&request).unwrap();
  let decoded = decode_whole_entity(&encoded, request.hash_algorithm, request.write_sequence).unwrap();
  assert_eq!(decoded.key, request.key);
  assert_eq!(decoded.stored_value, request.stored_value);
}

#[test]
fn whole_entity_length_preflight_is_exact_and_rejects_oversized_components_without_allocation() {
  assert_eq!(checked_whole_entity_encoded_length(HashAlgorithm::Blake3_256, 3, 5).unwrap(), 117);
  assert_eq!(checked_whole_entity_encoded_length(HashAlgorithm::Sha512, 3, 5).unwrap(), 149);

  let error = checked_whole_entity_encoded_length(HashAlgorithm::Blake3_256, WHOLE_ENTITY_V1_KEY_CAP + 1, 0).unwrap_err();
  assert_eq!(error.class(), MalformedInputClass::AllocationAmplification);
  assert_eq!(error.code(), "entity_component_exceeds_cap");

  let error = checked_whole_entity_encoded_length(HashAlgorithm::Blake3_256, 0, WHOLE_ENTITY_V1_VALUE_CAP + 1).unwrap_err();
  assert_eq!(error.class(), MalformedInputClass::AllocationAmplification);
  assert_eq!(error.code(), "entity_component_exceeds_cap");
}

#[test]
fn production_writer_surface_remains_disconnected_from_fixture_generation_and_service_activation() {
  let reference_manifest = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../tools/v4-reference/Cargo.toml")).unwrap();
  assert!(!reference_manifest.contains("aeordb-lib"));
  assert!(!reference_manifest.contains("path = \"../../aeordb-lib\""));

  let reference_sources = fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../tools/v4-reference/src"))
    .unwrap()
    .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
    .collect::<Vec<_>>()
    .join("\n");
  assert!(!reference_sources.contains("encode_database_header_slot"));
  assert!(!reference_sources.contains("encode_whole_entity"));

  let admission = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine/v4/admission.rs")).unwrap();
  assert!(admission.contains("Self::new(CapabilitySetV1(reader), CapabilitySetV1::empty())"));
  let storage_engine = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine/storage_engine.rs")).unwrap();
  assert!(!storage_engine.contains("encode_database_header_slot"));
  assert!(!storage_engine.contains("encode_whole_entity"));
}

fn mutate_header(header: &DatabaseHeaderV4, mutate: impl FnOnce(&mut DatabaseHeaderV4)) -> DatabaseHeaderV4 {
  let mut changed = header.clone();
  mutate(&mut changed);
  changed
}

fn assert_header_error(header: DatabaseHeaderV4, expected_code: &str) {
  let error = encode_database_header_slot(&header).unwrap_err();
  assert_eq!(error.code(), expected_code);
}
