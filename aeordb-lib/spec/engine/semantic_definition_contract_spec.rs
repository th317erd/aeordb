//! Ratified definition contracts, independent bytes and execution boundaries.
use aeordb::engine::v4::source_selector::{decode_source_selector, encode_source_selector, JsonPathSegmentV1, SourceSelectorWriteV1};
use aeordb::engine::v4::value_store::decode_value_store_definition;
use aeordb::engine::HashAlgorithm;

fn fixture(profile: &str, suffix: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/value-store-definition-v1/avst-{profile}-{suffix}-valid.bin", env!("CARGO_MANIFEST_DIR")))
    .unwrap()
}

fn independent_key_selector(length: usize) -> Vec<u8> {
  assert!(length >= 41);
  let mut bytes = vec![0; length];
  bytes[..2].copy_from_slice(&1u16.to_le_bytes());
  bytes[2..4].copy_from_slice(&2u16.to_le_bytes());
  bytes[4..8].copy_from_slice(&(length as u32).to_le_bytes());
  bytes[12..16].copy_from_slice(&1u32.to_le_bytes());
  bytes[16..18].copy_from_slice(&1u16.to_le_bytes());
  bytes[32] = 1;
  bytes[36..40].copy_from_slice(&((length - 40) as u32).to_le_bytes());
  bytes[40..].fill(b'k');
  bytes
}

#[test]
fn current_native_registry_and_existing_small_selector_remain_valid() {
  use aeordb::engine::v4::native_semantics::NativeSemanticComponentV1;
  assert_eq!(NativeSemanticComponentV1::RegexSelector.fingerprint()[..4], [0xea, 0xe5, 0x08, 0x00]);
  assert!(decode_source_selector(&independent_key_selector(4096)).is_ok());
  let mut malformed = independent_key_selector(4096);
  malformed[20] = 1;
  assert!(decode_source_selector(&malformed).is_err());
}

#[test]
fn selector_reader_accepts_the_ratified_64_kib_boundary() {
  for length in [4096, 4097, 65536] {
    let bytes = independent_key_selector(length);
    let decoded = decode_source_selector(&bytes).unwrap_or_else(|error| panic!("{length}: {error}"));
    assert_eq!(decoded.item_count, 1);
    assert!(matches!(decoded.segments.as_slice(), [JsonPathSegmentV1::ObjectKey(key)] if key.len() == length - 40));
  }
  assert!(decode_source_selector(&independent_key_selector(65537)).is_err());
}

#[test]
fn selector_writer_emits_exact_independent_64_kib_bytes() {
  for length in [4096, 4097, 65536] {
    let key = "k".repeat(length - 40);
    let segments = [JsonPathSegmentV1::ObjectKey(&key)];
    let encoded =
      encode_source_selector(SourceSelectorWriteV1::JsonPath { segments: &segments }).unwrap_or_else(|error| panic!("{length}: {error}"));
    assert_eq!(encoded, independent_key_selector(length));
  }
  let key = "k".repeat(65537 - 40);
  assert!(encode_source_selector(SourceSelectorWriteV1::JsonPath { segments: &[JsonPathSegmentV1::ObjectKey(&key)] }).is_err());
}

#[test]
fn value_store_reader_accepts_a_complete_64_kib_selector_child() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let old = fixture(profile, "json-corrected");
    let fixed = 32 + algorithm.hash_length();
    let field_length = u32::from_le_bytes(old[fixed..fixed + 4].try_into().unwrap()) as usize;
    let old_selector_length = u32::from_le_bytes(old[fixed + 4..fixed + 8].try_into().unwrap()) as usize;
    let selector_start = fixed + 80 + field_length;
    let mut bytes = old[..selector_start].to_vec();
    bytes.extend_from_slice(&independent_key_selector(65536));
    bytes.extend_from_slice(&old[selector_start + old_selector_length..]);
    let total = bytes.len() as u32;
    bytes[8..12].copy_from_slice(&total.to_le_bytes());
    bytes[fixed + 4..fixed + 8].copy_from_slice(&65536u32.to_le_bytes());
    assert!(decode_value_store_definition(&bytes, algorithm).is_ok(), "{profile}: ratified selector child rejected");
  }
}

fn independent_always_missing(algorithm: HashAlgorithm) -> Vec<u8> {
  // Round 8A AVST body plus Round 9 canonical none plan and empty ADPT.
  let field_name = b"legacy_missing";
  let fixed = 32 + algorithm.hash_length();
  let field_start = fixed + 80;
  let selector_start = field_start + field_name.len();
  let parser_start = selector_start + 32;
  let dependencies_start = parser_start + 48;
  let total = dependencies_start + 32;
  let mut bytes = vec![0; total];
  bytes[..4].copy_from_slice(b"AVST");
  bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
  bytes[6..8].copy_from_slice(&32u16.to_le_bytes());
  bytes[8..12].copy_from_slice(&(total as u32).to_le_bytes());
  bytes[32..fixed].fill(0x41);
  for (offset, value) in [(0, field_name.len() as u32), (4, 32), (8, 48), (12, 32), (36, 1)] {
    bytes[fixed + offset..fixed + offset + 4].copy_from_slice(&value.to_le_bytes());
  }
  for (offset, value) in [(16, 2u16), (20, 1), (22, 2), (24, 1), (26, 2), (28, 1), (30, 1), (32, 1), (34, 1)] {
    bytes[fixed + offset..fixed + offset + 2].copy_from_slice(&value.to_le_bytes());
  }
  bytes[fixed + 48..fixed + 56].copy_from_slice(&1u64.to_le_bytes());
  bytes[field_start..selector_start].copy_from_slice(field_name);
  bytes[selector_start..selector_start + 2].copy_from_slice(&1u16.to_le_bytes());
  bytes[selector_start + 2..selector_start + 4].copy_from_slice(&4u16.to_le_bytes());
  bytes[selector_start + 4..selector_start + 8].copy_from_slice(&32u32.to_le_bytes());
  bytes[parser_start..parser_start + 4].copy_from_slice(b"APRP");
  bytes[parser_start + 4..parser_start + 6].copy_from_slice(&1u16.to_le_bytes());
  bytes[parser_start + 6..parser_start + 8].copy_from_slice(&48u16.to_le_bytes());
  bytes[parser_start + 8..parser_start + 12].copy_from_slice(&48u32.to_le_bytes());
  bytes[parser_start + 16..parser_start + 18].copy_from_slice(&1u16.to_le_bytes());
  bytes[dependencies_start..dependencies_start + 4].copy_from_slice(b"ADPT");
  bytes[dependencies_start + 4..dependencies_start + 6].copy_from_slice(&1u16.to_le_bytes());
  bytes[dependencies_start + 6..dependencies_start + 8].copy_from_slice(&32u16.to_le_bytes());
  bytes[dependencies_start + 8..dependencies_start + 12].copy_from_slice(&32u32.to_le_bytes());
  bytes
}

#[test]
fn always_missing_requires_the_round_9_none_plan_without_content_work() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let bytes = independent_always_missing(algorithm);
    let definition = decode_value_store_definition(&bytes, algorithm).unwrap_or_else(|error| panic!("{algorithm:?}: {error}"));
    assert!(definition.parser_plan.candidates.is_empty());
    assert!(definition.dependencies.records.is_empty());
    assert_eq!(definition.max_document_input_bytes, 0);
  }
}

#[test]
fn always_missing_does_not_admit_the_superseded_round_8a_parser_pipeline() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let bytes = fixture(profile, "always-missing-legacy");
    assert!(decode_value_store_definition(&bytes, algorithm).is_err(), "{profile}: superseded parser pipeline admitted");
  }
}

#[test]
fn selector_segment_count_is_independent_of_the_larger_byte_cap() {
  for count in [508usize, 509, 1024, 1025] {
    let segments = vec![JsonPathSegmentV1::FanOut; count];
    let total = 32 + 8 * count;
    let mut independent = vec![0; total];
    independent[..2].copy_from_slice(&1u16.to_le_bytes());
    independent[2..4].copy_from_slice(&2u16.to_le_bytes());
    independent[4..8].copy_from_slice(&(total as u32).to_le_bytes());
    independent[12..16].copy_from_slice(&(count as u32).to_le_bytes());
    independent[16..18].copy_from_slice(&1u16.to_le_bytes());
    for offset in (32..total).step_by(8) {
      independent[offset] = 3;
    }
    if count <= 1024 {
      assert_eq!(decode_source_selector(&independent).unwrap().item_count as usize, count);
      assert_eq!(encode_source_selector(SourceSelectorWriteV1::JsonPath { segments: &segments }).unwrap(), independent);
    } else {
      use aeordb::engine::v4::reader::MalformedInputClass;
      assert_eq!(decode_source_selector(&independent).unwrap_err().class(), MalformedInputClass::AllocationAmplification);
      assert_eq!(
        encode_source_selector(SourceSelectorWriteV1::JsonPath { segments: &segments }).unwrap_err().class(),
        MalformedInputClass::AllocationAmplification
      );
    }
  }
}

#[test]
fn larger_selector_cap_preserves_framing_utf8_counts_and_reserve_validation() {
  let original = independent_key_selector(65536);
  decode_source_selector(&original).unwrap();
  for mutation in 0..10 {
    let mut bytes = original.clone();
    match mutation {
      0 => bytes[0] = 2,
      1 => bytes[2] = 0,
      2 => bytes[4..8].copy_from_slice(&4096u32.to_le_bytes()),
      3 => bytes[8] = 1,
      4 => bytes[20] = 1,
      5 => bytes[12..16].copy_from_slice(&2u32.to_le_bytes()),
      6 => bytes[32] = 0,
      7 => bytes[34] = 1,
      8 => bytes[36..40].copy_from_slice(&u32::MAX.to_le_bytes()),
      9 => bytes[65535] = 0xff,
      _ => unreachable!(),
    }
    assert!(decode_source_selector(&bytes).is_err(), "mutation {mutation}");
  }
  for length in [0, 1, 31, 32, 39, 40, 4096, 65535] {
    assert!(decode_source_selector(&original[..length]).is_err(), "truncation {length}");
  }
}

#[test]
fn canonical_always_missing_needs_no_document_parser_or_mapper_work() {
  use aeordb::engine::file_record::FileRecord;
  use aeordb::engine::v4::index_source::{SourceDocumentV1, SourceExtractionV1, SourceOperationalErrorClassV1, ValueStoreRuntimeV1};
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let bytes = independent_always_missing(algorithm);
    let runtime = ValueStoreRuntimeV1::from_encoded(&bytes, algorithm).unwrap();
    let mut record = FileRecord::new("/missing.json".to_string(), Some("application/json".to_string()), 0, Vec::new());
    record.total_size = u64::MAX;
    assert!(matches!(
      runtime.extract(SourceDocumentV1 { file_record: &record, parsed_value: None }, None, &|| false).unwrap(),
      SourceExtractionV1::Missing
    ));
    assert_eq!(
      runtime.extract(SourceDocumentV1 { file_record: &record, parsed_value: None }, None, &|| true).unwrap_err().class(),
      SourceOperationalErrorClassV1::Cancelled
    );
  }
}

#[test]
fn canonical_always_missing_rejects_inapplicable_content_limits_and_corrected_family() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let original = independent_always_missing(algorithm);
    decode_value_store_definition(&original, algorithm).unwrap();
    let fixed = 32 + algorithm.hash_length();
    for offset in [56, 64, 72] {
      for value in [1u64, u64::MAX] {
        let mut bytes = original.clone();
        bytes[fixed + offset..fixed + offset + 8].copy_from_slice(&value.to_le_bytes());
        assert!(decode_value_store_definition(&bytes, algorithm).is_err(), "offset {offset}, value {value}");
      }
    }
    let mut bytes = original.clone();
    for offset in [16, 22, 26] {
      bytes[fixed + offset..fixed + offset + 2].copy_from_slice(&1u16.to_le_bytes());
    }
    assert!(decode_value_store_definition(&bytes, algorithm).is_err(), "always-missing is migration only");
    let mut bytes = original.clone();
    bytes[fixed + 18..fixed + 20].copy_from_slice(&2u16.to_le_bytes());
    assert!(decode_value_store_definition(&bytes, algorithm).is_err(), "metadata semantics are inapplicable");
    let mut bytes = original;
    bytes[fixed + 80] = b'@';
    assert!(decode_value_store_definition(&bytes, algorithm).is_err(), "metadata field names are inapplicable");
  }
}

#[test]
fn corrected_dependency_records_reject_migration_only_identity_flags() {
  use aeordb::engine::v4::dependency::decode_dependency_table;
  for profile in ["blake3-256", "sha512"] {
    let original =
      std::fs::read(format!("{}/spec/fixtures/v4/dependency-table-v1/adpt-{profile}-wasm-mapper-valid.bin", env!("CARGO_MANIFEST_DIR"),))
        .unwrap();
    decode_dependency_table(&original).unwrap();
    // The opaque-ID flag is migration-only even if the ID happens to remain
    // canonical. Test known corrected ABI, known pure executor, and both.
    for (abi, executor) in [(4u16, 2u16), (4, u16::MAX), (u16::MAX, 2)] {
      let mut bytes = original.clone();
      bytes[40..44].copy_from_slice(&6u32.to_le_bytes());
      bytes[44..46].copy_from_slice(&abi.to_le_bytes());
      bytes[46..48].copy_from_slice(&executor.to_le_bytes());
      assert!(decode_dependency_table(&bytes).is_err(), "{profile}: corrected ABI={abi}/executor={executor} accepts opaque-v0 flag");
    }
    let mut legacy = original;
    legacy[40..44].copy_from_slice(&6u32.to_le_bytes());
    legacy[44..46].copy_from_slice(&2u16.to_le_bytes());
    legacy[46..48].copy_from_slice(&3u16.to_le_bytes());
    assert!(decode_dependency_table(&legacy).is_ok(), "explicit legacy identity must remain retainable");
  }
}

#[test]
fn corrected_dependency_closure_rejects_opaque_v0_flags_even_with_unknown_executors() {
  use aeordb::engine::v4::dependency::decode_dependency_table;
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let mut bytes = fixture(profile, "mapper-corrected");
    let fixed = 32 + algorithm.hash_length();
    let read_u32 = |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
    let table = fixed + 80 + read_u32(fixed) + read_u32(fixed + 4) + read_u32(fixed + 8);
    let record = table + 32;
    assert_eq!(&bytes[table..table + 4], b"ADPT");
    assert_eq!(u16::from_le_bytes(bytes[record + 4..record + 6].try_into().unwrap()), 1);
    bytes[record + 8..record + 12].copy_from_slice(&6u32.to_le_bytes());
    bytes[record + 12..record + 14].copy_from_slice(&u16::MAX.to_le_bytes());
    bytes[record + 14..record + 16].copy_from_slice(&u16::MAX.to_le_bytes());
    assert!(decode_dependency_table(&bytes[table..]).is_ok(), "unknown standalone executor retains structurally valid legacy flags");
    assert!(
      decode_value_store_definition(&bytes, algorithm).is_err(),
      "{profile}: a corrected closure cannot contain migration-only identity flags"
    );
  }
}
