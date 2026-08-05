use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use aeordb::engine::v4::database_header::{DatabaseHeaderVersion, decode_header_region, probe_header_version, read_header_region};
use aeordb::engine::v4::config_value::{CanonicalValueBounds, validate_canonical_value};
use aeordb::engine::v4::entity::decode_whole_entity;
use aeordb::engine::v4::dependency::{decode_dependency_table, decode_invocation_policy};
use aeordb::engine::v4::namespace::{SemanticObjectKind, decode_namespace_root, decode_semantic_object};
use aeordb::engine::v4::reader::{BoundedReader, MalformedInputClass};
use aeordb::engine::HashAlgorithm;
use serde::Deserialize;

#[derive(Deserialize)]
struct FixtureManifest {
  fixtures: Vec<FixtureRow>,
}

#[derive(Deserialize)]
struct FixtureRow {
  id: String,
  format_id: String,
  hash_algorithm: String,
  binary: String,
  canonical_key: Option<String>,
  expected: String,
}

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4")
}

fn manifest() -> FixtureManifest {
  serde_json::from_slice(&fs::read(fixture_root().join("format-fixture-manifest.json")).unwrap()).unwrap()
}

#[test]
fn every_database_header_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest().fixtures.into_iter().filter(|row| row.format_id == "database-header-v4").collect();
  assert_eq!(rows.len(), 10);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let observed = match decode_header_region(&bytes) {
      Ok(selected) if selected.redundancy_degraded => format!("selected:{}:redundancy-degraded", selected.header.slot_sequence),
      Ok(selected) => format!("selected:{}", selected.header.slot_sequence),
      Err(error) => format!("error:{}", error.code()),
    };
    assert_eq!(observed, row.expected, "fixture {}", row.id);
  }
}

#[test]
fn every_whole_entity_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest().fixtures.into_iter().filter(|row| row.format_id == "whole-entity-v1").collect();
  assert_eq!(rows.len(), 2);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let algorithm = hash_algorithm(&row.hash_algorithm);
    let entity = decode_whole_entity(&bytes, algorithm, u64::MAX).unwrap();
    assert_eq!(format!("entity:entry-type=0x{:02x}", entity.entry_type.to_u8()), row.expected, "fixture {}", row.id);
    assert_eq!(hex::encode(entity.key), row.canonical_key.unwrap(), "fixture {}", row.id);
  }
}

#[test]
fn every_namespace_and_semantic_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> =
    manifest().fixtures.into_iter().filter(|row| row.format_id == "directory-index-v1" || row.format_id == "semantic-object-v1").collect();
  assert_eq!(rows.len(), 12);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let algorithm = hash_algorithm(&row.hash_algorithm);
    let (observed, key) = if row.format_id == "directory-index-v1" {
      let root = decode_namespace_root(&bytes, algorithm).unwrap();
      ("directory:namespace-root".to_string(), root.root_hash)
    } else {
      let object = decode_semantic_object(&bytes, algorithm).unwrap();
      let summary = match object.kind {
        SemanticObjectKind::State { content_only_reason: None } => "semantic:state:complete".to_string(),
        SemanticObjectKind::State { content_only_reason: Some(reason) } => format!("semantic:state:content-only:reason={reason}"),
        SemanticObjectKind::CatalogLeaf { record_count } => format!("semantic:catalog-leaf:records={record_count}"),
        SemanticObjectKind::CatalogInternal { child_count } => format!("semantic:catalog-internal:children={child_count}"),
        SemanticObjectKind::Definition { class } => format!("semantic:definition:class={class}"),
      };
      (summary, object.object_id)
    };
    assert_eq!(observed, row.expected, "fixture {}", row.id);
    assert_eq!(hex::encode(key), row.canonical_key.unwrap(), "fixture {}", row.id);
  }
}

#[test]
fn every_canonical_config_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest().fixtures.into_iter().filter(|row| row.format_id == "canonical-config-value-v1").collect();
  assert_eq!(rows.len(), 6);
  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let summary = validate_canonical_value(&bytes, CanonicalValueBounds::CONFIG).unwrap();
    assert_eq!(format!("config:{}:{}={}", summary.tag_name, summary.detail_name, summary.detail), row.expected, "fixture {}", row.id);
  }
}

#[test]
fn every_invocation_and_dependency_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest()
    .fixtures
    .into_iter()
    .filter(|row| row.format_id == "invocation-policy-v1" || row.format_id == "dependency-table-v1")
    .collect();
  assert_eq!(rows.len(), 12);
  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let observed = if row.format_id == "invocation-policy-v1" {
      format!("policy:{}", decode_invocation_policy(&bytes).unwrap().name())
    } else {
      format!("dependencies:records={}", decode_dependency_table(&bytes).unwrap().records.len())
    };
    assert_eq!(observed, row.expected, "fixture {}", row.id);
  }
}

#[test]
fn invocation_and_dependency_readers_reject_context_and_count_corruption() {
  let root = fixture_root();
  let native = fs::read(root.join("invocation-policy-v1/aivp-blake3-256-native-valid.bin")).unwrap();
  let mut native_request = native;
  native_request[24..32].copy_from_slice(&1u64.to_le_bytes());
  assert_eq!(decode_invocation_policy(&native_request).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);

  let wasm = fs::read(root.join("invocation-policy-v1/aivp-blake3-256-pure-wasm-valid.bin")).unwrap();
  let mut unaligned_memory = wasm;
  unaligned_memory[40..48].copy_from_slice(&65_537u64.to_le_bytes());
  assert_eq!(decode_invocation_policy(&unaligned_memory).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);

  let empty = fs::read(root.join("dependency-table-v1/adpt-blake3-256-empty-valid.bin")).unwrap();
  let mut amplified = empty.clone();
  amplified[16..20].copy_from_slice(&1_025u32.to_le_bytes());
  assert_eq!(decode_dependency_table(&amplified).unwrap_err().class(), MalformedInputClass::AllocationAmplification);

  let mut trailing = empty;
  trailing.push(0);
  assert_eq!(decode_dependency_table(&trailing).unwrap_err().class(), MalformedInputClass::TruncationOrTrailingBytes);
}

#[test]
fn canonical_config_rejects_aliases_order_depth_and_amplification() {
  let small_u64 = canonical_frame(0x05, &1u64.to_le_bytes());
  assert_eq!(
    validate_canonical_value(&small_u64, CanonicalValueBounds::CONFIG).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let negative_zero = canonical_frame(0x06, &(-0.0f64).to_bits().to_le_bytes());
  assert_eq!(
    validate_canonical_value(&negative_zero, CanonicalValueBounds::CONFIG).unwrap_err().class(),
    MalformedInputClass::NoncanonicalBooleanOrOptionalPresence
  );

  let null = canonical_frame(0x01, &[]);
  let mut map = 2u32.to_le_bytes().to_vec();
  for key in [b"z".as_slice(), b"a".as_slice()] {
    map.extend_from_slice(&(key.len() as u32).to_le_bytes());
    map.extend_from_slice(key);
    map.extend_from_slice(&null);
  }
  assert_eq!(
    validate_canonical_value(&canonical_frame(0x0a, &map), CanonicalValueBounds::CONFIG).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );

  let oversized = vec![0u8; CanonicalValueBounds::CONFIG.maximum_value_length + 1];
  assert_eq!(
    validate_canonical_value(&oversized, CanonicalValueBounds::CONFIG).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );
}

#[test]
fn namespace_readers_reject_crc_identity_order_and_capability_corruption() {
  let directory = fs::read(fixture_root().join("directory-index-v1/adir-blake3-256-namespace-root-valid.bin")).unwrap();
  let mut bad_capability = directory.clone();
  bad_capability[36 + 3] = 1;
  repair_trailing_crc(&mut bad_capability);
  assert_eq!(
    decode_namespace_root(&bad_capability, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownRequiredCapability
  );

  let mut zero_edge = directory;
  zero_edge[72..104].fill(0);
  repair_trailing_crc(&mut zero_edge);
  assert_eq!(
    decode_namespace_root(&zero_edge, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::InvalidGraphEdgeOrCycle
  );

  let leaf = fs::read(fixture_root().join("semantic-object-v1/asem-blake3-256-catalog-leaf-valid.bin")).unwrap();
  let mut bad_crc = leaf.clone();
  *bad_crc.last_mut().unwrap() ^= 1;
  assert_eq!(
    decode_semantic_object(&bad_crc, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::ChecksumOrIntegrityMismatch
  );

  let mut trailing = leaf;
  trailing.push(0);
  assert_eq!(
    decode_semantic_object(&trailing, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::TruncationOrTrailingBytes
  );
}

#[test]
fn whole_entity_rejects_corrupt_framing_before_publication() {
  let source = fs::read(fixture_root().join("whole-entity-v1/entity-blake3-256-directory-root-valid.bin")).unwrap();

  let mut bad_crc = source.clone();
  bad_crc[105] ^= 0x80;
  assert_eq!(
    decode_whole_entity(&bad_crc, HashAlgorithm::Blake3_256, u64::MAX).unwrap_err().class(),
    MalformedInputClass::ChecksumOrIntegrityMismatch
  );

  let mut bad_integrity = source.clone();
  *bad_integrity.last_mut().unwrap() ^= 0x80;
  assert_eq!(decode_whole_entity(&bad_integrity, HashAlgorithm::Blake3_256, u64::MAX).unwrap_err().code(), "integrity_hash_mismatch");

  let mut unknown_type = source.clone();
  unknown_type[5] = 0xff;
  repair_entity_header_crc(&mut unknown_type, 32);
  assert_eq!(
    decode_whole_entity(&unknown_type, HashAlgorithm::Blake3_256, u64::MAX).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let mut zero_sequence = source.clone();
  zero_sequence[33..41].fill(0);
  repair_entity_header_crc(&mut zero_sequence, 32);
  assert_eq!(decode_whole_entity(&zero_sequence, HashAlgorithm::Blake3_256, u64::MAX).unwrap_err().code(), "unreserved_write_sequence");

  let mut bad_length = source.clone();
  bad_length[21..25].copy_from_slice(&u32::MAX.to_le_bytes());
  repair_entity_header_crc(&mut bad_length, 32);
  assert_eq!(
    decode_whole_entity(&bad_length, HashAlgorithm::Blake3_256, u64::MAX).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let mut trailing = source;
  trailing.push(0);
  assert_eq!(
    decode_whole_entity(&trailing, HashAlgorithm::Blake3_256, u64::MAX).unwrap_err().class(),
    MalformedInputClass::TruncationOrTrailingBytes
  );
}

#[test]
fn header_probe_distinguishes_v3_and_v4_without_writing() {
  let v4 = fs::read(fixture_root().join("database-header-v4/header-blake3-256-valid-ab.bin")).unwrap();
  assert_eq!(probe_header_version(&v4[..8]).unwrap(), DatabaseHeaderVersion::V4);

  let mut v3 = [0u8; 8];
  v3[..4].copy_from_slice(b"AEOR");
  v3[4] = 3;
  assert_eq!(probe_header_version(&v3).unwrap(), DatabaseHeaderVersion::V3);
}

#[test]
fn reading_a_header_region_does_not_modify_the_file() {
  let source = fs::read(fixture_root().join("database-header-v4/header-blake3-256-valid-ab.bin")).unwrap();
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("probe.aeordb");
  let mut file = fs::File::create(&path).unwrap();
  file.write_all(&source).unwrap();
  drop(file);

  let before = fs::read(&path).unwrap();
  let mut file = fs::File::open(&path).unwrap();
  let selected = read_header_region(&mut file).unwrap();
  assert_eq!(selected.header.slot_sequence, 42);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn bounded_reader_rejects_lengths_before_allocation() {
  let mut bytes = Vec::new();
  bytes.extend_from_slice(&u32::MAX.to_le_bytes());
  let mut reader = BoundedReader::new(&bytes, 1_024).unwrap();
  let error = reader.read_u32_length_prefixed(128).unwrap_err();
  assert_eq!(error.class(), MalformedInputClass::AllocationAmplification);
  assert_eq!(reader.allocated_bytes(), 0);
}

#[test]
fn bounded_reader_rejects_overflow_truncation_and_trailing_bytes() {
  assert_eq!(
    BoundedReader::checked_array_bytes(usize::MAX, 2, 1_024).unwrap_err().class(),
    MalformedInputClass::LengthCountOrArithmeticOverflow
  );

  let mut truncated = BoundedReader::new(&[1, 2, 3], 3).unwrap();
  assert_eq!(truncated.read_exact(4).unwrap_err().class(), MalformedInputClass::TruncationOrTrailingBytes);

  let mut trailing = BoundedReader::new(&[1, 2], 2).unwrap();
  trailing.read_u8().unwrap();
  assert_eq!(trailing.finish().unwrap_err().class(), MalformedInputClass::TruncationOrTrailingBytes);
}

#[test]
fn bounded_reader_accepts_exact_limits() {
  let bytes = [3u8, 0, 0, 0, b'a', b'b', b'c'];
  let mut reader = BoundedReader::new(&bytes, bytes.len()).unwrap();
  assert_eq!(reader.read_u32_length_prefixed(3).unwrap(), b"abc");
  reader.finish().unwrap();
  assert_eq!(reader.allocated_bytes(), 3);
}

fn hash_algorithm(name: &str) -> HashAlgorithm {
  match name {
    "blake3-256" => HashAlgorithm::Blake3_256,
    "sha512" => HashAlgorithm::Sha512,
    other => panic!("unsupported fixture hash algorithm {other}"),
  }
}

fn repair_entity_header_crc(entity: &mut [u8], hash_width: usize) {
  let header_length = 77 + hash_width;
  let crc_offset = header_length - 4;
  let crc = crc32fast::hash(&entity[..crc_offset]);
  entity[crc_offset..header_length].copy_from_slice(&crc.to_le_bytes());
}

fn repair_trailing_crc(value: &mut [u8]) {
  let crc_offset = value.len() - 4;
  let crc = crc32fast::hash(&value[..crc_offset]);
  value[crc_offset..].copy_from_slice(&crc.to_le_bytes());
}

fn canonical_frame(tag: u8, payload: &[u8]) -> Vec<u8> {
  let mut value = Vec::with_capacity(5 + payload.len());
  value.push(tag);
  value.extend_from_slice(&(payload.len() as u32).to_le_bytes());
  value.extend_from_slice(payload);
  value
}
