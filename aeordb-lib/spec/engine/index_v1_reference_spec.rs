use std::any::TypeId;
use std::sync::atomic::{AtomicUsize, Ordering};

use aeordb::engine::field_index_v0_nvt::FieldIndexV0Nvt;
use aeordb::engine::file_record::FileRecord;
use aeordb::engine::index_store::FieldIndex;
use aeordb::engine::kv_nvt::KvNvt;
use aeordb::engine::kv_store::KVStore;
use aeordb::engine::legacy_nvt_v1::LegacyNvtV1;
use aeordb::engine::nvt::NormalizedVectorTable;
use aeordb::engine::scalar_converter::{HashConverter, StringConverter};
use aeordb::engine::v4::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, decode_canonical_value, encode_canonical_value};
use aeordb::engine::v4::field_definition::{decode_converter_definition, decode_field_index_definition};
use aeordb::engine::v4::index_converter::{ConverterRuntimeV1, IndexSemanticErrorClassV1};
use aeordb::engine::v4::index_definition_runtime::{IndexDefinitionErrorClassV1, IndexDefinitionRuntimeV1};
use aeordb::engine::v4::index_artifact::{
  ImmutableIndexArtifactKindV1, IndexManifestBodyV1, IndexManifestWriteV1, decode_index_manifest, encode_index_manifest,
  validate_correctness_manifest_chain,
};
use aeordb::engine::v4::index_page::{
  ArtifactDirectoryEntryWriteV1, ArtifactDirectoryWriteV1, OrderedIndexRoleV1, OrderedPageWriteV1, PostingRecordV1,
  decode_artifact_directory, decode_ordered_page, decode_posting_record, encode_artifact_directory, encode_ordered_page,
  encode_posting_record, validate_posting_page_link,
};
use aeordb::engine::v4::index_record::{
  DocumentStateOwnerV1, DocumentStateRecordV1, decode_canonical_value_record, decode_document_state_record, decode_scope_document_record,
  decode_scope_reverse_record, encode_canonical_value_record, encode_document_state_record, encode_scope_document_record,
  encode_scope_reverse_record,
};
use aeordb::engine::v4::reader::MalformedInputClass;
use aeordb::engine::v4::index_semantic_registry::{
  SOURCE_TYPE_BYTES, SOURCE_TYPE_I64, SOURCE_TYPE_NULL, SOURCE_TYPE_U64, SOURCE_TYPE_UTF8, converter_registry, metadata_source_registry,
  source_selector_registry, strategy_registry,
};
use aeordb::engine::v4::index_source::{
  PluginMapperExecutorV1, PluginMapperOutcomeV1, PluginMapperRequestV1, SourceDocumentV1, SourceExtractionV1,
  SourceOperationalErrorClassV1, SourceOperationalErrorV1, SourceOperationalResultV1, ValueStoreRuntimeV1,
};
use aeordb::engine::v4::value_store::decode_value_store_definition;
use aeordb::engine::HashAlgorithm;

fn index_artifact_fixture(name: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/index-artifact-v1/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn legacy_hash_nvt_fixture(bucket_count: u32, buckets: &[(u64, u32)]) -> Vec<u8> {
  assert_eq!(buckets.len(), bucket_count as usize);
  let mut bytes = Vec::new();
  bytes.push(1);
  bytes.extend_from_slice(&1u32.to_le_bytes());
  bytes.push(1);
  bytes.extend_from_slice(&bucket_count.to_le_bytes());
  for (offset, entry_count) in buckets {
    bytes.extend_from_slice(&offset.to_le_bytes());
    bytes.extend_from_slice(&entry_count.to_le_bytes());
  }
  bytes
}

fn empty_kv_store_fixture(bucket_count: u32) -> Vec<u8> {
  let nvt = legacy_hash_nvt_fixture(bucket_count, &vec![(0, 0); bucket_count as usize]);
  let mut bytes = Vec::new();
  bytes.push(1);
  bytes.extend_from_slice(&1u16.to_le_bytes());
  bytes.extend_from_slice(&0u64.to_le_bytes());
  bytes.extend_from_slice(&(nvt.len() as u32).to_le_bytes());
  bytes.extend_from_slice(&nvt);
  bytes
}

fn populated_kv_store_fixture() -> Vec<u8> {
  let hash = vec![0; 32];
  let nvt = legacy_hash_nvt_fixture(2, &[(0, 1), (0, 0)]);
  let mut bytes = Vec::new();
  bytes.push(1);
  bytes.extend_from_slice(&1u16.to_le_bytes());
  bytes.extend_from_slice(&1u64.to_le_bytes());
  bytes.push(0);
  bytes.extend_from_slice(&hash);
  bytes.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
  bytes.extend_from_slice(&96u32.to_le_bytes());
  bytes.extend_from_slice(&(nvt.len() as u32).to_le_bytes());
  bytes.extend_from_slice(&nvt);
  bytes
}

fn empty_v0_field_index_fixture(field_name: &str, bucket_count: u32) -> Vec<u8> {
  let nvt = legacy_hash_nvt_fixture(bucket_count, &vec![(0, 0); bucket_count as usize]);
  let mut bytes = Vec::new();
  bytes.push(0);
  bytes.extend_from_slice(&(field_name.len() as u16).to_le_bytes());
  bytes.extend_from_slice(field_name.as_bytes());
  bytes.extend_from_slice(&1u32.to_le_bytes());
  bytes.push(1);
  bytes.extend_from_slice(&(nvt.len() as u32).to_le_bytes());
  bytes.extend_from_slice(&nvt);
  bytes.extend_from_slice(&0u32.to_le_bytes());
  bytes.extend_from_slice(&0u32.to_le_bytes());
  bytes
}

fn populated_v0_field_index_fixture(field_name: &str, bucket_count: u32) -> Vec<u8> {
  let mut buckets = vec![(0, 0); bucket_count as usize];
  buckets[0] = (0, 1);
  let nvt = legacy_hash_nvt_fixture(bucket_count, &buckets);
  let mut bytes = Vec::new();
  bytes.push(0);
  bytes.extend_from_slice(&(field_name.len() as u16).to_le_bytes());
  bytes.extend_from_slice(field_name.as_bytes());
  bytes.extend_from_slice(&1u32.to_le_bytes());
  bytes.push(1);
  bytes.extend_from_slice(&(nvt.len() as u32).to_le_bytes());
  bytes.extend_from_slice(&nvt);
  bytes.extend_from_slice(&1u32.to_le_bytes());
  bytes.extend_from_slice(&0f64.to_le_bytes());
  bytes.extend_from_slice(&[0; 32]);
  bytes.extend_from_slice(&0u32.to_le_bytes());
  bytes
}

#[test]
fn legacy_kv_and_v0_field_nvt_have_distinct_owners() {
  assert_ne!(TypeId::of::<LegacyNvtV1>(), TypeId::of::<KvNvt>());
  assert_ne!(TypeId::of::<LegacyNvtV1>(), TypeId::of::<FieldIndexV0Nvt>());
  assert_ne!(TypeId::of::<KvNvt>(), TypeId::of::<FieldIndexV0Nvt>());
}

#[test]
fn public_normalized_vector_table_facade_preserves_legacy_bytes() {
  let mut legacy = LegacyNvtV1::new(Box::new(HashConverter), 2);
  legacy.update_bucket(0, 0x0102_0304_0506_0708, 3);
  legacy.update_bucket(1, 16, 1);

  let expected = legacy_hash_nvt_fixture(2, &[(0x0102_0304_0506_0708, 3), (16, 1)]);
  assert_eq!(legacy.serialize(), expected);

  let facade = NormalizedVectorTable::deserialize(&expected).unwrap();
  assert_eq!(facade.serialize(), expected);
}

#[test]
fn kv_nvt_preserves_legacy_payload_and_complete_kv_store_bytes() {
  let mut nvt = KvNvt::new(2);
  nvt.update_bucket(0, 0x0102_0304_0506_0708, 3);
  nvt.update_bucket(1, 16, 1);
  let expected = legacy_hash_nvt_fixture(2, &[(0x0102_0304_0506_0708, 3), (16, 1)]);
  assert_eq!(nvt.serialize(), expected);
  assert_eq!(KvNvt::deserialize(&expected).unwrap().serialize(), expected);

  let store = KVStore::new(HashAlgorithm::Blake3_256, 2);
  assert_eq!(store.serialize(), empty_kv_store_fixture(2));

  let mut populated = KVStore::new(HashAlgorithm::Blake3_256, 2);
  populated.insert(aeordb::engine::KVEntry {
    type_flags: aeordb::engine::KV_TYPE_CHUNK,
    hash: vec![0; 32],
    offset: 0x0102_0304_0506_0708,
    total_length: 96,
  });
  assert_eq!(populated.serialize(), populated_kv_store_fixture());
}

#[test]
fn field_index_v0_nvt_preserves_legacy_payload_and_complete_index_bytes() {
  let mut nvt = FieldIndexV0Nvt::new(Box::new(HashConverter), 2);
  nvt.update_bucket(0, 0x0102_0304_0506_0708, 3);
  nvt.update_bucket(1, 16, 1);
  let expected = legacy_hash_nvt_fixture(2, &[(0x0102_0304_0506_0708, 3), (16, 1)]);
  assert_eq!(nvt.serialize(), expected);
  assert_eq!(FieldIndexV0Nvt::deserialize(&expected).unwrap().serialize(), expected);

  let index = FieldIndex::new("hash".to_string(), Box::new(HashConverter));
  assert_eq!(index.serialize(32), empty_v0_field_index_fixture("hash", 1_024));

  let mut populated = FieldIndex::new("hash".to_string(), Box::new(HashConverter));
  populated.insert(&[0; 32], vec![0; 32]);
  populated.ensure_nvt_current();
  assert_eq!(populated.serialize(32), populated_v0_field_index_fixture("hash", 1_024));
}

#[test]
fn kv_wrapper_rejects_non_hash_legacy_payload_without_weakening_v0_field_compatibility() {
  let bytes = LegacyNvtV1::new(Box::new(StringConverter::new(32)), 2).serialize();
  assert!(KvNvt::deserialize(&bytes).is_err());
  assert_eq!(FieldIndexV0Nvt::deserialize(&bytes).unwrap().serialize(), bytes);
}

fn converter_fixture(name: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/converter-definition-v1/acnv-blake3-256-{name}-valid.bin", env!("CARGO_MANIFEST_DIR")))
    .unwrap()
}

fn field_index_fixture(name: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/field-index-definition-v1/afix-blake3-256-{name}-valid.bin", env!("CARGO_MANIFEST_DIR")))
    .unwrap()
}

fn value_store_fixture(name: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/value-store-definition-v1/{name}.bin", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn corrected_json_value_store_with_selector(segments: &[Vec<u8>]) -> Vec<u8> {
  let mut selector = Vec::new();
  selector.extend_from_slice(&1u16.to_le_bytes());
  selector.extend_from_slice(&2u16.to_le_bytes());
  let selector_length = 32usize + segments.iter().map(Vec::len).sum::<usize>();
  selector.extend_from_slice(&(selector_length as u32).to_le_bytes());
  selector.extend_from_slice(&0u32.to_le_bytes());
  selector.extend_from_slice(&(segments.len() as u32).to_le_bytes());
  selector.extend_from_slice(&1u16.to_le_bytes());
  selector.extend_from_slice(&0u16.to_le_bytes());
  selector.extend_from_slice(&[0; 12]);
  for segment in segments {
    selector.extend_from_slice(segment);
  }
  assert_eq!(selector.len(), selector_length);

  let mut value_store = value_store_fixture("avst-blake3-256-json-corrected-valid");
  let old_selector_length = u32::from_le_bytes(value_store[68..72].try_into().unwrap()) as usize;
  value_store.splice(152..152 + old_selector_length, selector);
  let total_length = value_store.len() as u32;
  value_store[8..12].copy_from_slice(&total_length.to_le_bytes());
  value_store[68..72].copy_from_slice(&(selector_length as u32).to_le_bytes());
  value_store
}

fn object_key_segment(key: &str) -> Vec<u8> {
  let mut segment = Vec::new();
  segment.extend_from_slice(&1u16.to_le_bytes());
  segment.extend_from_slice(&0u16.to_le_bytes());
  segment.extend_from_slice(&(key.len() as u32).to_le_bytes());
  segment.extend_from_slice(key.as_bytes());
  segment
}

fn numeric_index_segment(index: u64) -> Vec<u8> {
  let mut segment = Vec::new();
  segment.extend_from_slice(&2u16.to_le_bytes());
  segment.extend_from_slice(&0u16.to_le_bytes());
  segment.extend_from_slice(&8u32.to_le_bytes());
  segment.extend_from_slice(&index.to_le_bytes());
  segment
}

fn regex_segment(pattern: &str) -> Vec<u8> {
  let mut segment = Vec::new();
  segment.extend_from_slice(&4u16.to_le_bytes());
  segment.extend_from_slice(&0u16.to_le_bytes());
  segment.extend_from_slice(&(pattern.len() as u32).to_le_bytes());
  segment.extend_from_slice(pattern.as_bytes());
  segment
}

fn corrected_runtime(name: &str) -> ConverterRuntimeV1<'static> {
  let bytes = Box::leak(converter_fixture(name).into_boxed_slice());
  ConverterRuntimeV1::from_encoded(bytes, HashAlgorithm::Blake3_256).unwrap()
}

fn token_coordinate_reference(posting_key: &[u8]) -> u64 {
  let mut hasher = blake3::Hasher::new();
  hasher.update(b"aeordb.index.token-coordinate.v1\0");
  hasher.update(posting_key);
  let digest = hasher.finalize();
  u64::from_be_bytes(digest.as_bytes()[..8].try_into().unwrap())
}

fn migration_scalar_coordinate_reference(scalar: f64) -> Option<u64> {
  if scalar <= 0.0 {
    return Some(0);
  }
  if scalar >= 1.0 {
    return Some(u64::MAX);
  }
  if !scalar.is_finite() {
    return None;
  }

  let bits = scalar.to_bits();
  let exponent_bits = ((bits >> 52) & 0x7ff) as i32;
  if exponent_bits == 0 {
    return Some(0);
  }
  let significand = (1u128 << 52) | u128::from(bits & ((1u64 << 52) - 1));
  let shift = exponent_bits - 1023 + 12;
  let coordinate = if shift >= 0 { significand << shift } else { significand >> -shift };
  u64::try_from(coordinate).ok()
}

#[test]
fn v1_converter_and_strategy_registries_are_closed_and_match_frozen_definitions() {
  let converters = converter_registry();
  let strategies = strategy_registry();
  assert_eq!(converters.len(), 25);
  assert_eq!(strategies.len(), 12);
  assert_eq!(converters.iter().filter(|entry| entry.corrected).count(), 12);
  assert_eq!(strategies.iter().filter(|entry| entry.corrected).count(), 6);

  for converter in converters {
    let bytes = converter_fixture(converter.name);
    let decoded = decode_converter_definition(&bytes, HashAlgorithm::Blake3_256).unwrap();
    assert_eq!(decoded.converter_id, converter.id);
    assert_eq!(decoded.source_type_mask, converter.source_type_mask);
    assert_eq!(decoded.corrected, converter.corrected);
    assert_eq!(decoded.name, converter.name);

    let field_bytes = field_index_fixture(converter.name);
    let field = decode_field_index_definition(&field_bytes, HashAlgorithm::Blake3_256).unwrap();
    assert_eq!(field.strategy_id, converter.strategy_id);
    assert_eq!(
      field.strategy_name,
      strategies.iter().find(|entry| entry.id == converter.strategy_id && entry.corrected == converter.corrected).unwrap().name
    );
  }
}

#[test]
fn v1_source_registries_are_closed_and_typed_without_alias_duplication() {
  let selectors = source_selector_registry();
  assert_eq!(selectors.len(), 4);
  assert!(selectors[..3].iter().all(|entry| entry.corrected && entry.migration));
  assert_eq!(selectors[3].name, "always_missing_v0");
  assert!(!selectors[3].corrected);

  let metadata = metadata_source_registry();
  assert_eq!(metadata.len(), 8);
  assert_eq!(metadata[0].field_name, "@path");
  assert_eq!(metadata[0].corrected_source_type_mask, SOURCE_TYPE_UTF8);
  assert_eq!(metadata[3].corrected_source_type_mask, SOURCE_TYPE_NULL | SOURCE_TYPE_UTF8);
  assert_eq!(metadata[4].corrected_source_type_mask, SOURCE_TYPE_U64);
  assert_eq!(metadata[5].corrected_source_type_mask, SOURCE_TYPE_I64);
  assert_eq!(metadata[7].corrected_source_type_mask, SOURCE_TYPE_BYTES);
  assert!(!metadata.iter().any(|entry| entry.field_name == "@file_name"));
}

#[test]
fn corrected_scalar_converters_share_canonical_source_and_query_compilation() {
  let cases = [
    ("bytes_binary_order_v1", CanonicalConfigValueV1::Bytes(vec![0x00, 0xff]), vec![0x00, 0xff], 0x00ff_0000_0000_0000),
    ("utf8_binary_order_v1", CanonicalConfigValueV1::String("A".to_string()), b"A".to_vec(), 0x4100_0000_0000_0000),
    ("u64_order_v1", CanonicalConfigValueV1::Unsigned(u64::MAX), u64::MAX.to_le_bytes().to_vec(), u64::MAX),
    ("i64_order_v1", CanonicalConfigValueV1::Signed(i64::MIN), i64::MIN.to_le_bytes().to_vec(), 0),
    ("bool_order_v1", CanonicalConfigValueV1::Boolean(true), vec![1], u64::MAX),
  ];

  for (name, value, expected_key, expected_coordinate) in cases {
    let runtime = corrected_runtime(name);
    let source = runtime.compile_source_value(&value).unwrap();
    let query = runtime.compile_query_literal(&value).unwrap();
    assert_eq!(source, query);
    assert_eq!(source.postings.len(), 1);
    assert_eq!(source.postings[0].posting_key, expected_key, "{name}");
    assert_eq!(source.postings[0].coordinate, expected_coordinate, "{name}");
    assert_eq!(source.postings[0].expansion_ordinal, 0);
  }
}

#[test]
fn corrected_token_converters_match_ratified_classes_coordinates_order_and_deduplication() {
  let cases = [
    (
      "unicode_trigram_v1",
      "A.B",
      vec![
        (hex::decode("01202061").unwrap(), 0x4b8b_1a4f_6b2a_2862),
        (hex::decode("01206120").unwrap(), 0x8f1b_5ce7_7914_2e26),
        (hex::decode("01202062").unwrap(), 0x14a7_ef9a_a466_e426),
        (hex::decode("01206220").unwrap(), 0x2796_91f7_fe5b_feaf),
        (hex::decode("02612e62").unwrap(), 0x36d1_4394_6826_8084),
      ],
    ),
    ("soundex_ascii_v1", "Robert-Rupert Robert", vec![(hex::decode("0352313633").unwrap(), 0x0dc0_93c8_c500_2637)]),
    ("double_metaphone_primary_ascii_v1", "Smith Smith", vec![(hex::decode("04534d30").unwrap(), 0x4efb_65f8_2095_b1de)]),
    ("double_metaphone_alt_ascii_v1", "Smith Schmidt Schmidt", vec![(hex::decode("05534d5454").unwrap(), 0x07a7_6593_4571_acdd)]),
  ];

  for (name, source, expected) in cases {
    let runtime = corrected_runtime(name);
    let value = CanonicalConfigValueV1::String(source.to_string());
    let compiled = runtime.compile_source_value(&value).unwrap();
    assert_eq!(compiled, runtime.compile_query_literal(&value).unwrap(), "{name}");
    assert_eq!(compiled.postings.len(), expected.len(), "{name}");
    for (ordinal, (posting, (expected_key, expected_coordinate))) in compiled.postings.iter().zip(expected).enumerate() {
      assert_eq!(posting.posting_key, expected_key, "{name} posting {ordinal}");
      assert_eq!(posting.coordinate, expected_coordinate, "{name} posting {ordinal}");
      assert_eq!(posting.coordinate, token_coordinate_reference(&posting.posting_key), "{name} posting {ordinal}");
      assert_eq!(posting.expansion_ordinal, ordinal as u32, "{name} posting {ordinal}");
    }
  }
}

#[test]
fn corrected_trigram_preserves_first_occurrence_across_word_and_substring_classes() {
  let runtime = corrected_runtime("unicode_trigram_v1");
  let compiled = runtime.compile_source_value(&CanonicalConfigValueV1::String("aaaa".to_string())).unwrap();
  let expected = ["01202061", "01206161", "01616161", "01616120", "02616161"];
  assert_eq!(compiled.postings.len(), expected.len());
  for (ordinal, (posting, expected)) in compiled.postings.iter().zip(expected).enumerate() {
    assert_eq!(posting.posting_key, hex::decode(expected).unwrap());
    assert_eq!(posting.expansion_ordinal, ordinal as u32);
  }
}

#[test]
fn corrected_tokens_cover_empty_short_unicode_and_strict_ascii_phonetic_edges() {
  for name in ["unicode_trigram_v1", "soundex_ascii_v1", "double_metaphone_primary_ascii_v1", "double_metaphone_alt_ascii_v1"] {
    let runtime = corrected_runtime(name);
    let value = CanonicalConfigValueV1::String(String::new());
    assert!(runtime.compile_source_value(&value).unwrap().postings.is_empty(), "{name}");
    assert_eq!(runtime.compile_source_value(&value).unwrap(), runtime.compile_query_literal(&value).unwrap(), "{name}");
  }

  let trigram = corrected_runtime("unicode_trigram_v1");
  for (source, expected) in [
    (".", Vec::<&str>::new()),
    ("ab", vec!["01202061", "01206162", "01616220"]),
    ("é", vec!["012020c3a9", "0120c3a920"]),
    ("İ", vec!["01202069", "01206920"]),
  ] {
    let value = CanonicalConfigValueV1::String(source.to_string());
    let compiled = trigram.compile_source_value(&value).unwrap();
    assert_eq!(compiled, trigram.compile_query_literal(&value).unwrap(), "{source:?}");
    assert_eq!(compiled.postings.len(), expected.len(), "{source:?}");
    for (posting, expected) in compiled.postings.iter().zip(expected) {
      assert_eq!(posting.posting_key, hex::decode(expected).unwrap(), "{source:?}");
      assert_eq!(posting.coordinate, token_coordinate_reference(&posting.posting_key), "{source:?}");
    }
  }

  for name in ["soundex_ascii_v1", "double_metaphone_primary_ascii_v1", "double_metaphone_alt_ascii_v1"] {
    let runtime = corrected_runtime(name);
    let non_ascii = CanonicalConfigValueV1::String("K".to_string());
    assert!(runtime.compile_source_value(&non_ascii).unwrap().postings.is_empty(), "{name} must retain source ASCII letters only");
  }
}

#[test]
fn token_compilation_is_independent_of_prior_source_order() {
  let source_values = ["A.B", "Schmidt", "Robert-Rupert", "aaaa", "İstanbul"];
  for name in ["unicode_trigram_v1", "soundex_ascii_v1", "double_metaphone_primary_ascii_v1", "double_metaphone_alt_ascii_v1"] {
    let runtime = corrected_runtime(name);
    let forward = source_values
      .iter()
      .map(|source| runtime.compile_source_value(&CanonicalConfigValueV1::String((*source).to_string())).unwrap())
      .collect::<Vec<_>>();
    let reverse = source_values
      .iter()
      .rev()
      .map(|source| runtime.compile_source_value(&CanonicalConfigValueV1::String((*source).to_string())).unwrap())
      .collect::<Vec<_>>();
    assert_eq!(forward, reverse.into_iter().rev().collect::<Vec<_>>(), "{name}");
  }
}

#[test]
fn corrected_token_converters_reject_malformed_posting_keys_and_fail_limits_atomically() {
  let trigram = corrected_runtime("unicode_trigram_v1");
  for malformed in [vec![], vec![3, b'a', b'b', b'c'], vec![1, 0xff], vec![1, b'a', b'b']] {
    let error = trigram.compare_posting_keys(&malformed, &[1, b'a', b'b', b'c']).unwrap_err();
    assert_eq!(error.class(), IndexSemanticErrorClassV1::MalformedPostingKey);
  }
  for (name, malformed, valid) in [
    ("soundex_ascii_v1", vec![3, b'R', b'1'], vec![3, b'R', b'1', b'6', b'3']),
    ("double_metaphone_primary_ascii_v1", vec![5, b'S'], vec![4, b'S', b'M', b'0']),
    ("double_metaphone_alt_ascii_v1", vec![5, b's'], vec![5, b'S', b'M', b'T', b'T']),
  ] {
    let error = corrected_runtime(name).compare_posting_keys(&malformed, &valid).unwrap_err();
    assert_eq!(error.class(), IndexSemanticErrorClassV1::MalformedPostingKey, "{name}");
  }

  let mut count_limited = converter_fixture("unicode_trigram_v1");
  count_limited[72..76].copy_from_slice(&1u32.to_le_bytes());
  let runtime = ConverterRuntimeV1::from_encoded(Box::leak(count_limited.into_boxed_slice()), HashAlgorithm::Blake3_256).unwrap();
  let error = runtime.compile_source_value(&CanonicalConfigValueV1::String("A.B".to_string())).unwrap_err();
  assert_eq!(error.class(), IndexSemanticErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "converter_output_count_limit");

  let mut total_limited = converter_fixture("soundex_ascii_v1");
  total_limited[80..88].copy_from_slice(&4u64.to_le_bytes());
  let runtime = ConverterRuntimeV1::from_encoded(Box::leak(total_limited.into_boxed_slice()), HashAlgorithm::Blake3_256).unwrap();
  let error = runtime.compile_source_value(&CanonicalConfigValueV1::String("Robert".to_string())).unwrap_err();
  assert_eq!(error.class(), IndexSemanticErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "converter_total_output_limit");
}

#[test]
fn migration_v0_adapters_use_the_ratified_fixed_coordinate_mapping_and_share_query_compilation() {
  let cases = [
    ("hash_v0", vec![0xff; 8]),
    ("u8_v0", vec![128]),
    ("u16_v0", 32_768u16.to_be_bytes().to_vec()),
    ("u32_v0", 1u32.to_be_bytes().to_vec()),
    ("u64_v0", 1u64.to_be_bytes().to_vec()),
    ("i64_v0", 0i64.to_be_bytes().to_vec()),
    ("f64_v0", 0.5f64.to_be_bytes().to_vec()),
    ("string_v0", b"middle".to_vec()),
    ("timestamp_v0", 2_051_222_400_000i64.to_be_bytes().to_vec()),
    ("trigram_v0", b"alpha".to_vec()),
    ("soundex_v0", b"Robert".to_vec()),
    ("dmetaphone_primary_v0", b"Smith".to_vec()),
    ("dmetaphone_alt_v0", b"Schmidt".to_vec()),
  ];

  for (name, bytes) in cases {
    let runtime = corrected_runtime(name);
    let value = CanonicalConfigValueV1::Bytes(bytes);
    let source = runtime.compile_source_value(&value).unwrap();
    assert_eq!(source, runtime.compile_query_literal(&value).unwrap(), "{name}");
    assert!(!source.postings.is_empty(), "{name}");
  }

  let hash = corrected_runtime("hash_v0");
  assert_eq!(
    hash.compile_source_value(&CanonicalConfigValueV1::Bytes(Vec::new())).unwrap().postings[0].coordinate,
    migration_scalar_coordinate_reference(0.0).unwrap()
  );
  assert_eq!(
    hash.compile_source_value(&CanonicalConfigValueV1::Bytes(vec![0xff; 8])).unwrap().postings[0].coordinate,
    migration_scalar_coordinate_reference(1.0).unwrap()
  );
  let float = corrected_runtime("f64_v0");
  assert_eq!(
    float.compile_source_value(&CanonicalConfigValueV1::Bytes(0.5f64.to_be_bytes().to_vec())).unwrap().postings[0].coordinate,
    migration_scalar_coordinate_reference(0.5).unwrap()
  );
  assert_eq!(
    float.compile_source_value(&CanonicalConfigValueV1::Bytes(0.1f64.to_be_bytes().to_vec())).unwrap().postings[0].coordinate,
    migration_scalar_coordinate_reference(0.1).unwrap()
  );
}

#[test]
fn migration_scalar_reference_freezes_ieee_boundaries_without_float_to_integer_casts() {
  assert_eq!(migration_scalar_coordinate_reference(-1.0), Some(0));
  assert_eq!(migration_scalar_coordinate_reference(0.0), Some(0));
  assert_eq!(migration_scalar_coordinate_reference(f64::from_bits(1)), Some(0));
  assert_eq!(migration_scalar_coordinate_reference(0.5), Some(1u64 << 63));
  assert_eq!(migration_scalar_coordinate_reference(f64::from_bits(1.0f64.to_bits() - 1)), Some(u64::MAX - 2_047));
  assert_eq!(migration_scalar_coordinate_reference(1.0), Some(u64::MAX));
  assert_eq!(migration_scalar_coordinate_reference(f64::INFINITY), Some(u64::MAX));
  assert_eq!(migration_scalar_coordinate_reference(f64::NAN), None);
}

#[test]
fn migration_v0_adapter_rejects_a_nonfinite_derived_scalar_instead_of_assigning_a_coordinate() {
  let mut definition = converter_fixture("f64_v0");
  definition[120..128].copy_from_slice(&f64::NAN.to_le_bytes());
  definition[128..136].copy_from_slice(&1.0f64.to_le_bytes());
  let runtime = ConverterRuntimeV1::from_encoded(Box::leak(definition.into_boxed_slice()), HashAlgorithm::Blake3_256).unwrap();
  let error = runtime.compile_source_value(&CanonicalConfigValueV1::Bytes(0.5f64.to_be_bytes().to_vec())).unwrap_err();
  assert_eq!(error.class(), IndexSemanticErrorClassV1::InvalidSourceValue);
  assert_eq!(error.code(), "legacy_scalar_nonfinite");
}

#[test]
fn migration_v0_adapter_preserves_invalid_utf8_reversed_ranges_and_atomic_limits() {
  for name in ["trigram_v0", "soundex_v0", "dmetaphone_primary_v0", "dmetaphone_alt_v0"] {
    let runtime = corrected_runtime(name);
    let value = CanonicalConfigValueV1::Bytes(vec![0xff]);
    assert!(runtime.compile_source_value(&value).unwrap().postings.is_empty(), "{name}");
  }

  let mut unsigned_definition = converter_fixture("u16_v0");
  unsigned_definition[120..122].copy_from_slice(&2u16.to_le_bytes());
  unsigned_definition[122..124].copy_from_slice(&1u16.to_le_bytes());
  let unsigned = ConverterRuntimeV1::from_encoded(Box::leak(unsigned_definition.into_boxed_slice()), HashAlgorithm::Blake3_256).unwrap();
  let unsigned_posting =
    unsigned.compile_source_value(&CanonicalConfigValueV1::Bytes(3u16.to_be_bytes().to_vec())).unwrap().postings.remove(0);
  assert_eq!(unsigned_posting.coordinate, migration_scalar_coordinate_reference(1.0 / 65_535.0).unwrap());

  let mut float_definition = converter_fixture("f64_v0");
  float_definition[120..128].copy_from_slice(&1.0f64.to_le_bytes());
  float_definition[128..136].copy_from_slice(&0.0f64.to_le_bytes());
  let float = ConverterRuntimeV1::from_encoded(Box::leak(float_definition.into_boxed_slice()), HashAlgorithm::Blake3_256).unwrap();
  let float_posting =
    float.compile_source_value(&CanonicalConfigValueV1::Bytes(0.25f64.to_be_bytes().to_vec())).unwrap().postings.remove(0);
  assert_eq!(float_posting.coordinate, migration_scalar_coordinate_reference(0.75).unwrap());

  let mut count_limited = converter_fixture("trigram_v0");
  count_limited[72..76].copy_from_slice(&1u32.to_le_bytes());
  let runtime = ConverterRuntimeV1::from_encoded(Box::leak(count_limited.into_boxed_slice()), HashAlgorithm::Blake3_256).unwrap();
  let error = runtime.compile_source_value(&CanonicalConfigValueV1::Bytes(b"abc".to_vec())).unwrap_err();
  assert_eq!(error.class(), IndexSemanticErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "converter_output_count_limit");

  let mut truncated_parameters = converter_fixture("u64_v0");
  truncated_parameters.pop();
  let truncated_length = truncated_parameters.len() as u32;
  truncated_parameters[8..12].copy_from_slice(&truncated_length.to_le_bytes());
  truncated_parameters[56..60].copy_from_slice(&15u32.to_le_bytes());
  let error = ConverterRuntimeV1::from_encoded(Box::leak(truncated_parameters.into_boxed_slice()), HashAlgorithm::Blake3_256).unwrap_err();
  assert_eq!(error.class(), IndexSemanticErrorClassV1::UnsupportedDefinition);
  assert_eq!(error.code(), "converter_definition_invalid");
}

#[test]
fn corrected_numeric_comparison_decodes_little_endian_keys_instead_of_sorting_bytes() {
  let runtime = corrected_runtime("u64_order_v1");
  let low = runtime.compile_source_value(&CanonicalConfigValueV1::Unsigned(255)).unwrap().postings.remove(0);
  let high = runtime.compile_source_value(&CanonicalConfigValueV1::Unsigned(256)).unwrap().postings.remove(0);
  assert!(low.posting_key > high.posting_key, "little-endian bytes deliberately disagree with numeric order");
  assert_eq!(runtime.compare_posting_keys(&low.posting_key, &high.posting_key).unwrap(), std::cmp::Ordering::Less);
  assert!(low.coordinate < high.coordinate);
}

#[test]
fn corrected_ordered_converter_coordinates_and_comparators_match_an_independent_boundary_model() {
  let unsigned = corrected_runtime("u64_order_v1");
  let unsigned_values = [0, 1, 255, 256, 1u64 << 53, 1u64 << 63, u64::MAX];
  let unsigned_postings = unsigned_values
    .iter()
    .map(|value| unsigned.compile_source_value(&CanonicalConfigValueV1::Unsigned(*value)).unwrap().postings.remove(0))
    .collect::<Vec<_>>();
  for (index, posting) in unsigned_postings.iter().enumerate() {
    assert_eq!(posting.coordinate, unsigned_values[index]);
    if index > 0 {
      assert_eq!(
        unsigned.compare_posting_keys(&unsigned_postings[index - 1].posting_key, &posting.posting_key).unwrap(),
        std::cmp::Ordering::Less
      );
      assert!(unsigned_postings[index - 1].coordinate < posting.coordinate);
    }
  }

  let signed = corrected_runtime("i64_order_v1");
  let signed_values = [i64::MIN, -1, 0, 1, i64::MAX];
  let signed_postings = signed_values
    .iter()
    .map(|value| signed.compile_source_value(&CanonicalConfigValueV1::Signed(*value)).unwrap().postings.remove(0))
    .collect::<Vec<_>>();
  for (index, posting) in signed_postings.iter().enumerate() {
    assert_eq!(posting.coordinate, (signed_values[index] as u64) ^ (1u64 << 63));
    if index > 0 {
      assert_eq!(
        signed.compare_posting_keys(&signed_postings[index - 1].posting_key, &posting.posting_key).unwrap(),
        std::cmp::Ordering::Less
      );
      assert!(signed_postings[index - 1].coordinate < posting.coordinate);
    }
  }

  let float = corrected_runtime("f64_finite_order_v1");
  let float_values = [-f64::MAX, -1.0, 0.0, 1.0, f64::MAX];
  let float_postings = float_values
    .iter()
    .map(|value| float.compile_source_value(&CanonicalConfigValueV1::FloatBits(value.to_bits())).unwrap().postings.remove(0))
    .collect::<Vec<_>>();
  for (index, posting) in float_postings.iter().enumerate() {
    let bits = float_values[index].to_bits();
    let expected_coordinate = if bits & (1u64 << 63) == 0 { bits ^ (1u64 << 63) } else { !bits };
    assert_eq!(posting.coordinate, expected_coordinate);
    if index > 0 {
      assert_eq!(
        float.compare_posting_keys(&float_postings[index - 1].posting_key, &posting.posting_key).unwrap(),
        std::cmp::Ordering::Less
      );
      assert!(float_postings[index - 1].coordinate < posting.coordinate);
    }
  }
}

#[test]
fn corrected_converter_rejects_malformed_persisted_keys_and_enforces_every_scalar_byte_limit() {
  let exact = corrected_runtime("typed_exact_blake3_v1");
  let error = exact.compare_posting_keys(&[0x07; 32], &[0x07; 33]).unwrap_err();
  assert_eq!(error.class(), IndexSemanticErrorClassV1::MalformedPostingKey);
  let error = exact.compare_posting_keys(&[0xff; 33], &[0x07; 33]).unwrap_err();
  assert_eq!(error.class(), IndexSemanticErrorClassV1::MalformedPostingKey);

  let utf8 = corrected_runtime("utf8_binary_order_v1");
  let error = utf8.compare_posting_keys(&[0xff], b"valid").unwrap_err();
  assert_eq!(error.class(), IndexSemanticErrorClassV1::MalformedPostingKey);

  let unsigned = corrected_runtime("u64_order_v1");
  let error = unsigned.compare_posting_keys(&[0; 7], &[0; 8]).unwrap_err();
  assert_eq!(error.class(), IndexSemanticErrorClassV1::MalformedPostingKey);

  let boolean = corrected_runtime("bool_order_v1");
  let error = boolean.compare_posting_keys(&[2], &[1]).unwrap_err();
  assert_eq!(error.class(), IndexSemanticErrorClassV1::MalformedPostingKey);

  let float = corrected_runtime("f64_finite_order_v1");
  let error = float.compare_posting_keys(&(-0.0f64).to_bits().to_le_bytes(), &0.0f64.to_bits().to_le_bytes()).unwrap_err();
  assert_eq!(error.class(), IndexSemanticErrorClassV1::MalformedPostingKey);

  let mut input_limited = converter_fixture("utf8_binary_order_v1");
  input_limited[64..72].copy_from_slice(&5u64.to_le_bytes());
  let input_limited = Box::leak(input_limited.into_boxed_slice());
  let runtime = ConverterRuntimeV1::from_encoded(input_limited, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(
    runtime.compile_source_value(&CanonicalConfigValueV1::String("x".to_string())).unwrap_err().class(),
    IndexSemanticErrorClassV1::ResourceLimit
  );

  let mut output_limited = converter_fixture("utf8_binary_order_v1");
  output_limited[76..80].copy_from_slice(&1u32.to_le_bytes());
  let output_limited = Box::leak(output_limited.into_boxed_slice());
  let runtime = ConverterRuntimeV1::from_encoded(output_limited, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(
    runtime.compile_source_value(&CanonicalConfigValueV1::String("ab".to_string())).unwrap_err().class(),
    IndexSemanticErrorClassV1::ResourceLimit
  );

  let mut total_limited = converter_fixture("utf8_binary_order_v1");
  total_limited[80..88].copy_from_slice(&1u64.to_le_bytes());
  let total_limited = Box::leak(total_limited.into_boxed_slice());
  let runtime = ConverterRuntimeV1::from_encoded(total_limited, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(
    runtime.compile_source_value(&CanonicalConfigValueV1::String("ab".to_string())).unwrap_err().class(),
    IndexSemanticErrorClassV1::ResourceLimit
  );
}

#[test]
fn converter_coercion_never_rewrites_the_authoritative_canonical_source_value() {
  let runtime = corrected_runtime("u64_order_v1");
  let source = CanonicalConfigValueV1::Signed(5);
  let compiled = runtime.compile_source_value(&source).unwrap();
  assert_eq!(decode_canonical_value(&compiled.canonical_value, CanonicalValueBounds::SOURCE_VALUE).unwrap(), source);
  assert_eq!(compiled.postings[0].posting_key, 5u64.to_le_bytes());
}

#[test]
fn corrected_float_and_timestamp_converters_reject_ambiguous_values_and_share_exact_coordinates() {
  let float = corrected_runtime("f64_finite_order_v1");
  let negative_zero = float.compile_source_value(&CanonicalConfigValueV1::FloatBits((-0.0f64).to_bits())).unwrap();
  assert_eq!(negative_zero.postings[0].posting_key, 0.0f64.to_bits().to_le_bytes());
  assert_eq!(negative_zero.postings[0].coordinate, 1 << 63);
  assert!(float.compile_source_value(&CanonicalConfigValueV1::FloatBits(f64::NAN.to_bits())).is_err());
  assert!(float.compile_source_value(&CanonicalConfigValueV1::Unsigned(9_007_199_254_740_993)).is_err());
  assert!(float.compile_source_value(&CanonicalConfigValueV1::Unsigned(u64::MAX)).is_err());
  assert!(float.compile_source_value(&CanonicalConfigValueV1::Signed(i64::MAX)).is_err());
  assert!(float.compile_source_value(&CanonicalConfigValueV1::Signed(i64::MIN)).is_ok());
  assert!(float.compile_source_value(&CanonicalConfigValueV1::Unsigned(1u64 << 63)).is_ok());

  let timestamp = corrected_runtime("timestamp_ms_order_v1");
  let utc = timestamp.compile_source_value(&CanonicalConfigValueV1::String("1970-01-01T00:00:00Z".to_string())).unwrap();
  let offset = timestamp.compile_query_literal(&CanonicalConfigValueV1::String("1970-01-01T01:00:00+01:00".to_string())).unwrap();
  assert_eq!(utc.postings[0].posting_key, 0i64.to_le_bytes());
  assert_eq!(utc.postings[0].coordinate, 1 << 63);
  assert_eq!(utc.postings, offset.postings);
  assert!(timestamp.compile_query_literal(&CanonicalConfigValueV1::String("1970-01-01 00:00:00".to_string())).is_err());
  assert!(timestamp.compile_query_literal(&CanonicalConfigValueV1::String("0".to_string())).is_err());
}

#[test]
fn typed_exact_candidate_key_never_replaces_complete_value_recheck() {
  let runtime = corrected_runtime("typed_exact_blake3_v1");
  let value = CanonicalConfigValueV1::String("candidate".to_string());
  let canonical = encode_canonical_value(&value, CanonicalValueBounds::SOURCE_VALUE).unwrap();
  let posting = runtime.compile_source_value(&value).unwrap().postings.remove(0);

  let digest = blake3::hash(&[b"aeordb.typed-exact-posting.v1\0".as_slice(), canonical.as_slice()].concat());
  let mut expected_key = vec![canonical[0]];
  expected_key.extend_from_slice(digest.as_bytes());
  assert_eq!(posting.posting_key, expected_key);

  let different =
    encode_canonical_value(&CanonicalConfigValueV1::String("different".to_string()), CanonicalValueBounds::SOURCE_VALUE).unwrap();
  assert!(runtime.exact_values_equal(&canonical, &canonical).unwrap());
  assert!(!runtime.exact_values_equal(&canonical, &different).unwrap());
}

#[test]
fn corrected_metadata_source_emits_typed_full_content_hash_without_reading_file_bytes() {
  let bytes = Box::leak(value_store_fixture("avst-blake3-256-metadata-hash-corrected-valid").into_boxed_slice());
  let runtime = ValueStoreRuntimeV1::from_encoded(bytes, HashAlgorithm::Blake3_256).unwrap();
  let record = FileRecord {
    path: "/docs/a.json".to_string(),
    content_type: Some("application/json".to_string()),
    total_size: 99,
    created_at: -5,
    updated_at: 7,
    metadata: Vec::new(),
    content_hash: vec![0xab; 32],
    chunk_hashes: vec![vec![0xcd; 32]],
  };

  let extracted = runtime.extract(SourceDocumentV1 { file_record: &record, parsed_value: None }, None, &|| false).unwrap();
  let SourceExtractionV1::Values(values) = extracted else {
    panic!("metadata hash must produce one value");
  };
  assert_eq!(values.len(), 1);
  assert_eq!(
    decode_canonical_value(&values[0], CanonicalValueBounds::SOURCE_VALUE).unwrap(),
    CanonicalConfigValueV1::Bytes(vec![0xab; 32])
  );

  let mut missing_hash = record;
  missing_hash.content_hash.clear();
  let extracted = runtime.extract(SourceDocumentV1 { file_record: &missing_hash, parsed_value: None }, None, &|| false).unwrap();
  assert!(matches!(extracted, SourceExtractionV1::DeterministicUnindexable { code: "file_record_migration_required", .. }));
}

#[test]
fn corrected_metadata_hash_uses_the_selected_database_hash_profile_width() {
  let bytes = Box::leak(value_store_fixture("avst-sha512-metadata-hash-corrected-valid").into_boxed_slice());
  let definition = decode_value_store_definition(bytes, HashAlgorithm::Sha512).unwrap();
  assert_eq!(definition.value_store_id.len(), 64);
  let runtime = ValueStoreRuntimeV1::from_encoded(bytes, HashAlgorithm::Sha512).unwrap();
  let record = FileRecord {
    path: "/docs/wide.bin".to_string(),
    content_type: None,
    total_size: 64,
    created_at: 0,
    updated_at: 0,
    metadata: Vec::new(),
    content_hash: vec![0xab; 64],
    chunk_hashes: vec![vec![0xcd; 64]],
  };
  let SourceExtractionV1::Values(values) =
    runtime.extract(SourceDocumentV1 { file_record: &record, parsed_value: None }, None, &|| false).unwrap()
  else {
    panic!("64-byte metadata hash must produce one value");
  };
  assert_eq!(
    decode_canonical_value(&values[0], CanonicalValueBounds::SOURCE_VALUE).unwrap(),
    CanonicalConfigValueV1::Bytes(vec![0xab; 64])
  );
}

#[test]
fn corrected_json_selector_preserves_order_duplicates_and_never_publishes_partial_values() {
  let bytes = Box::leak(value_store_fixture("avst-blake3-256-json-corrected-valid").into_boxed_slice());
  let runtime = ValueStoreRuntimeV1::from_encoded(bytes, HashAlgorithm::Blake3_256).unwrap();
  let parsed = CanonicalConfigValueV1::Map(std::collections::BTreeMap::from([(
    "messages".to_string(),
    CanonicalConfigValueV1::Array(vec![
      CanonicalConfigValueV1::Map(std::collections::BTreeMap::from([(
        "user".to_string(),
        CanonicalConfigValueV1::String("first".to_string()),
      )])),
      CanonicalConfigValueV1::Map(std::collections::BTreeMap::from([(
        "assistant".to_string(),
        CanonicalConfigValueV1::String("skip".to_string()),
      )])),
      CanonicalConfigValueV1::Map(std::collections::BTreeMap::from([(
        "user".to_string(),
        CanonicalConfigValueV1::String("first".to_string()),
      )])),
    ]),
  )]));
  let record = FileRecord::new("/messages.json".to_string(), Some("application/json".to_string()), 0, Vec::new());

  let extracted = runtime.extract(SourceDocumentV1 { file_record: &record, parsed_value: Some(&parsed) }, None, &|| false).unwrap();
  let SourceExtractionV1::Values(values) = extracted else {
    panic!("selector must produce ordered values");
  };
  let decoded = values.iter().map(|value| decode_canonical_value(value, CanonicalValueBounds::SOURCE_VALUE).unwrap()).collect::<Vec<_>>();
  assert_eq!(decoded, vec![CanonicalConfigValueV1::String("first".to_string()), CanonicalConfigValueV1::String("first".to_string())]);

  let cancellation = runtime.extract(SourceDocumentV1 { file_record: &record, parsed_value: Some(&parsed) }, None, &|| true).unwrap_err();
  assert_eq!(cancellation.class(), SourceOperationalErrorClassV1::Cancelled);
}

#[test]
fn corrected_json_selector_handles_numeric_array_and_object_indices() {
  let bytes = Box::leak(
    corrected_json_value_store_with_selector(&[object_key_segment("m"), numeric_index_segment(0), regex_segment("use")]).into_boxed_slice(),
  );
  let runtime = ValueStoreRuntimeV1::from_encoded(bytes, HashAlgorithm::Blake3_256).unwrap();
  let record = FileRecord::new("/numeric.json".to_string(), Some("application/json".to_string()), 0, Vec::new());

  for parsed in [
    CanonicalConfigValueV1::Map(std::collections::BTreeMap::from([(
      "m".to_string(),
      CanonicalConfigValueV1::Array(vec![CanonicalConfigValueV1::Map(std::collections::BTreeMap::from([(
        "use".to_string(),
        CanonicalConfigValueV1::String("array".to_string()),
      )]))]),
    )])),
    CanonicalConfigValueV1::Map(std::collections::BTreeMap::from([(
      "m".to_string(),
      CanonicalConfigValueV1::Map(std::collections::BTreeMap::from([(
        "0".to_string(),
        CanonicalConfigValueV1::Map(std::collections::BTreeMap::from([(
          "use".to_string(),
          CanonicalConfigValueV1::String("object".to_string()),
        )])),
      )])),
    )])),
  ] {
    let SourceExtractionV1::Values(values) =
      runtime.extract(SourceDocumentV1 { file_record: &record, parsed_value: Some(&parsed) }, None, &|| false).unwrap()
    else {
      panic!("numeric selector must produce one value");
    };
    assert_eq!(values.len(), 1);
  }
}

#[test]
fn corrected_array_regex_evaluates_prior_candidates_first_and_preserves_output_order() {
  let bytes = Box::leak(corrected_json_value_store_with_selector(&[object_key_segment("m"), regex_segment("a")]).into_boxed_slice());
  let runtime = ValueStoreRuntimeV1::from_encoded(bytes, HashAlgorithm::Blake3_256).unwrap();
  let record = FileRecord::new("/array-regex.json".to_string(), Some("application/json".to_string()), 0, Vec::new());
  let matching_map = CanonicalConfigValueV1::Map(std::collections::BTreeMap::from([("a".to_string(), CanonicalConfigValueV1::Signed(1))]));
  let parsed = CanonicalConfigValueV1::Map(std::collections::BTreeMap::from([(
    "m".to_string(),
    CanonicalConfigValueV1::Array(vec![matching_map.clone(), CanonicalConfigValueV1::String("alpha".to_string())]),
  )]));
  let SourceExtractionV1::Values(values) =
    runtime.extract(SourceDocumentV1 { file_record: &record, parsed_value: Some(&parsed) }, None, &|| false).unwrap()
  else {
    panic!("array regex must produce ordered values");
  };
  let decoded = values.iter().map(|value| decode_canonical_value(value, CanonicalValueBounds::SOURCE_VALUE).unwrap()).collect::<Vec<_>>();
  assert_eq!(decoded, vec![matching_map, CanonicalConfigValueV1::String("alpha".to_string())]);

  let mut bytes = corrected_json_value_store_with_selector(&[object_key_segment("m"), regex_segment("a")]);
  bytes[136..144].copy_from_slice(&1u64.to_le_bytes());
  let bytes = Box::leak(bytes.into_boxed_slice());
  let runtime = ValueStoreRuntimeV1::from_encoded(bytes, HashAlgorithm::Blake3_256).unwrap();
  let parsed = CanonicalConfigValueV1::Map(std::collections::BTreeMap::from([(
    "m".to_string(),
    CanonicalConfigValueV1::Array(vec![CanonicalConfigValueV1::Bytes(vec![0]), CanonicalConfigValueV1::String("large".to_string())]),
  )]));
  let outcome = runtime.extract(SourceDocumentV1 { file_record: &record, parsed_value: Some(&parsed) }, None, &|| false).unwrap();
  assert!(matches!(outcome, SourceExtractionV1::DeterministicUnindexable { code: "selector_regex_value", .. }));

  let escaped_value = "line\n\"quoted\"";
  let expected_json = serde_json::to_string(&serde_json::json!({ "a": escaped_value })).unwrap();
  let bytes = Box::leak(
    corrected_json_value_store_with_selector(&[object_key_segment("m"), regex_segment(&regex::escape(&expected_json))]).into_boxed_slice(),
  );
  let runtime = ValueStoreRuntimeV1::from_encoded(bytes, HashAlgorithm::Blake3_256).unwrap();
  let escaped_map = CanonicalConfigValueV1::Map(std::collections::BTreeMap::from([(
    "a".to_string(),
    CanonicalConfigValueV1::String(escaped_value.to_string()),
  )]));
  let parsed = CanonicalConfigValueV1::Map(std::collections::BTreeMap::from([(
    "m".to_string(),
    CanonicalConfigValueV1::Array(vec![escaped_map.clone()]),
  )]));
  let SourceExtractionV1::Values(values) =
    runtime.extract(SourceDocumentV1 { file_record: &record, parsed_value: Some(&parsed) }, None, &|| false).unwrap()
  else {
    panic!("compact JSON escaping must agree with the independent serde JSON oracle");
  };
  assert_eq!(decode_canonical_value(&values[0], CanonicalValueBounds::SOURCE_VALUE).unwrap(), escaped_map);
}

#[test]
fn corrected_json_selector_limits_fail_the_whole_document_without_partial_values() {
  let mut work_limited = value_store_fixture("avst-blake3-256-json-corrected-valid");
  work_limited[128..136].copy_from_slice(&4u64.to_le_bytes());
  let work_limited = Box::leak(work_limited.into_boxed_slice());
  let runtime = ValueStoreRuntimeV1::from_encoded(work_limited, HashAlgorithm::Blake3_256).unwrap();
  let parsed = CanonicalConfigValueV1::Map(std::collections::BTreeMap::from([(
    "messages".to_string(),
    CanonicalConfigValueV1::Array(vec![
      CanonicalConfigValueV1::Map(std::collections::BTreeMap::from([(
        "user".to_string(),
        CanonicalConfigValueV1::String("first".to_string()),
      )])),
      CanonicalConfigValueV1::Map(std::collections::BTreeMap::from([(
        "user".to_string(),
        CanonicalConfigValueV1::String("second".to_string()),
      )])),
    ]),
  )]));
  let record = FileRecord::new("/limited.json".to_string(), Some("application/json".to_string()), 0, Vec::new());
  let outcome = runtime.extract(SourceDocumentV1 { file_record: &record, parsed_value: Some(&parsed) }, None, &|| false).unwrap();
  assert!(matches!(outcome, SourceExtractionV1::DeterministicUnindexable { code: "selector_work_limit", .. }));

  let mut count_limited = value_store_fixture("avst-blake3-256-json-corrected-valid");
  count_limited[100..104].copy_from_slice(&1u32.to_le_bytes());
  let count_limited = Box::leak(count_limited.into_boxed_slice());
  let runtime = ValueStoreRuntimeV1::from_encoded(count_limited, HashAlgorithm::Blake3_256).unwrap();
  let outcome = runtime.extract(SourceDocumentV1 { file_record: &record, parsed_value: Some(&parsed) }, None, &|| false).unwrap();
  assert!(matches!(outcome, SourceExtractionV1::DeterministicUnindexable { code: "source_value_count_limit", .. }));

  let mut bytes_limited = value_store_fixture("avst-blake3-256-json-corrected-valid");
  bytes_limited[112..120].copy_from_slice(&1u64.to_le_bytes());
  let bytes_limited = Box::leak(bytes_limited.into_boxed_slice());
  let runtime = ValueStoreRuntimeV1::from_encoded(bytes_limited, HashAlgorithm::Blake3_256).unwrap();
  let outcome = runtime.extract(SourceDocumentV1 { file_record: &record, parsed_value: Some(&parsed) }, None, &|| false).unwrap();
  assert!(matches!(outcome, SourceExtractionV1::DeterministicUnindexable { code: "source_value_bytes_limit", .. }));

  let mut examined_limited = value_store_fixture("avst-blake3-256-json-corrected-valid");
  examined_limited[136..144].copy_from_slice(&3u64.to_le_bytes());
  let examined_limited = Box::leak(examined_limited.into_boxed_slice());
  let runtime = ValueStoreRuntimeV1::from_encoded(examined_limited, HashAlgorithm::Blake3_256).unwrap();
  let outcome = runtime.extract(SourceDocumentV1 { file_record: &record, parsed_value: Some(&parsed) }, None, &|| false).unwrap();
  assert!(matches!(outcome, SourceExtractionV1::DeterministicUnindexable { code: "selector_examined_bytes_limit", .. }));

  let mut document_limited = value_store_fixture("avst-blake3-256-json-corrected-valid");
  document_limited[120..128].copy_from_slice(&1u64.to_le_bytes());
  let document_limited = Box::leak(document_limited.into_boxed_slice());
  let runtime = ValueStoreRuntimeV1::from_encoded(document_limited, HashAlgorithm::Blake3_256).unwrap();
  let mut oversized_record = record;
  oversized_record.total_size = 2;
  let outcome = runtime.extract(SourceDocumentV1 { file_record: &oversized_record, parsed_value: Some(&parsed) }, None, &|| false).unwrap();
  assert!(matches!(outcome, SourceExtractionV1::DeterministicUnindexable { code: "source_document_input_limit", .. }));
}

#[test]
fn corrected_plugin_mapper_without_exact_executor_is_operational_not_a_negative_match() {
  let bytes = Box::leak(value_store_fixture("avst-blake3-256-mapper-corrected-valid").into_boxed_slice());
  let runtime = ValueStoreRuntimeV1::from_encoded(bytes, HashAlgorithm::Blake3_256).unwrap();
  let parsed = CanonicalConfigValueV1::Map(std::collections::BTreeMap::new());
  let record = FileRecord::new("/mapper.json".to_string(), Some("application/json".to_string()), 0, Vec::new());
  let error = runtime.extract(SourceDocumentV1 { file_record: &record, parsed_value: Some(&parsed) }, None, &|| false).unwrap_err();
  assert_eq!(error.class(), SourceOperationalErrorClassV1::DependencyUnavailable);
}

struct DuplicateMapper;

impl PluginMapperExecutorV1 for DuplicateMapper {
  fn invoke(&self, request: PluginMapperRequestV1<'_>) -> SourceOperationalResultV1<PluginMapperOutcomeV1> {
    assert!(request.dependency_ordinal > 0);
    decode_canonical_value(request.arguments, CanonicalValueBounds::CONFIG).unwrap();
    let value = encode_canonical_value(&CanonicalConfigValueV1::String("same".to_string()), CanonicalValueBounds::SOURCE_VALUE).unwrap();
    Ok(PluginMapperOutcomeV1::Values(vec![value.clone(), value]))
  }
}

struct InvalidMapper;

impl PluginMapperExecutorV1 for InvalidMapper {
  fn invoke(&self, _request: PluginMapperRequestV1<'_>) -> SourceOperationalResultV1<PluginMapperOutcomeV1> {
    Ok(PluginMapperOutcomeV1::Values(vec![vec![0xff]]))
  }
}

struct FailingMapper;

impl PluginMapperExecutorV1 for FailingMapper {
  fn invoke(&self, _request: PluginMapperRequestV1<'_>) -> SourceOperationalResultV1<PluginMapperOutcomeV1> {
    Err(SourceOperationalErrorV1::host_failure("mapper_host_failed", "synthetic host failure"))
  }
}

struct EmptyMapper;

impl PluginMapperExecutorV1 for EmptyMapper {
  fn invoke(&self, _request: PluginMapperRequestV1<'_>) -> SourceOperationalResultV1<PluginMapperOutcomeV1> {
    Ok(PluginMapperOutcomeV1::Values(Vec::new()))
  }
}

struct RejectingMapper;

impl PluginMapperExecutorV1 for RejectingMapper {
  fn invoke(&self, _request: PluginMapperRequestV1<'_>) -> SourceOperationalResultV1<PluginMapperOutcomeV1> {
    Ok(PluginMapperOutcomeV1::DeterministicRejection { code: "mapper_rejected", context: "synthetic rejection".to_string() })
  }
}

struct CountingMapper<'a> {
  calls: &'a AtomicUsize,
}

impl PluginMapperExecutorV1 for CountingMapper<'_> {
  fn invoke(&self, _request: PluginMapperRequestV1<'_>) -> SourceOperationalResultV1<PluginMapperOutcomeV1> {
    self.calls.fetch_add(1, Ordering::SeqCst);
    Ok(PluginMapperOutcomeV1::Missing)
  }
}

#[test]
fn corrected_plugin_mapper_preserves_valid_duplicates_but_separates_contract_and_host_failures() {
  let bytes = Box::leak(value_store_fixture("avst-blake3-256-mapper-corrected-valid").into_boxed_slice());
  let runtime = ValueStoreRuntimeV1::from_encoded(bytes, HashAlgorithm::Blake3_256).unwrap();
  let parsed = CanonicalConfigValueV1::Map(std::collections::BTreeMap::new());
  let record = FileRecord::new("/mapper.json".to_string(), Some("application/json".to_string()), 0, Vec::new());
  let document = SourceDocumentV1 { file_record: &record, parsed_value: Some(&parsed) };

  let outcome = runtime.extract(document, Some(&DuplicateMapper), &|| false).unwrap();
  let SourceExtractionV1::Values(values) = outcome else {
    panic!("valid mapper must return values");
  };
  assert_eq!(values.len(), 2);
  assert_eq!(values[0], values[1]);

  let outcome = runtime.extract(document, Some(&InvalidMapper), &|| false).unwrap();
  assert!(matches!(outcome, SourceExtractionV1::DeterministicUnindexable { code: "plugin_mapper_invalid_value", .. }));

  let error = runtime.extract(document, Some(&FailingMapper), &|| false).unwrap_err();
  assert_eq!(error.class(), SourceOperationalErrorClassV1::HostFailure);
  assert_eq!(error.code(), "mapper_host_failed");

  let outcome = runtime.extract(document, Some(&EmptyMapper), &|| false).unwrap();
  assert!(matches!(outcome, SourceExtractionV1::DeterministicUnindexable { code: "plugin_mapper_empty_values", .. }));

  let outcome = runtime.extract(document, Some(&RejectingMapper), &|| false).unwrap();
  assert!(matches!(outcome, SourceExtractionV1::DeterministicUnindexable { code: "mapper_rejected", .. }));

  let calls = AtomicUsize::new(0);
  let cancellation_checks = AtomicUsize::new(0);
  let error = runtime
    .extract(document, Some(&CountingMapper { calls: &calls }), &|| cancellation_checks.fetch_add(1, Ordering::SeqCst) >= 2)
    .unwrap_err();
  assert_eq!(calls.load(Ordering::SeqCst), 1);
  assert_eq!(error.class(), SourceOperationalErrorClassV1::Cancelled);
}

#[test]
fn public_runtime_constructors_redecode_complete_definition_bytes_and_reject_corruption() {
  let mut converter = converter_fixture("typed_exact_blake3_v1");
  converter[32..34].copy_from_slice(&0xffffu16.to_le_bytes());
  let error = ConverterRuntimeV1::from_encoded(&converter, HashAlgorithm::Blake3_256).unwrap_err();
  assert_eq!(error.class(), IndexSemanticErrorClassV1::UnsupportedDefinition);
  assert_eq!(error.code(), "converter_definition_invalid");

  let mut value_store = value_store_fixture("avst-blake3-256-json-corrected-valid");
  value_store[12] = 1;
  let error = ValueStoreRuntimeV1::from_encoded(&value_store, HashAlgorithm::Blake3_256).unwrap_err();
  assert_eq!(error.class(), SourceOperationalErrorClassV1::HostFailure);
  assert_eq!(error.code(), "value_store_definition_invalid");

  let valid_value_store = value_store_fixture("avst-blake3-256-metadata-hash-corrected-valid");
  let value_definition = decode_value_store_definition(&valid_value_store, HashAlgorithm::Blake3_256).unwrap();
  let mut field = field_index_fixture("typed_exact_blake3_v1");
  field[32..64].copy_from_slice(&value_definition.value_store_id);
  field[64..66].copy_from_slice(&0xffffu16.to_le_bytes());
  let error = IndexDefinitionRuntimeV1::from_encoded(&valid_value_store, &field, HashAlgorithm::Blake3_256).unwrap_err();
  assert_eq!(error.class(), IndexDefinitionErrorClassV1::UnsupportedDefinition);
  assert_eq!(error.code(), "index_field_definition_invalid");
}

#[test]
fn executable_index_definition_binds_the_complete_value_store_identity_and_strategy() {
  let value_bytes = Box::leak(value_store_fixture("avst-blake3-256-metadata-hash-corrected-valid").into_boxed_slice());
  let value = decode_value_store_definition(value_bytes, HashAlgorithm::Blake3_256).unwrap();
  let mut field_bytes = field_index_fixture("typed_exact_blake3_v1");
  field_bytes[32..64].copy_from_slice(&value.value_store_id);
  let field_bytes = Box::leak(field_bytes.into_boxed_slice());
  let runtime = IndexDefinitionRuntimeV1::from_encoded(value_bytes, field_bytes, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(runtime.index_id(), runtime.field_definition().index_id);
  assert_eq!(runtime.value_store_id(), value.value_store_id);
  assert!(runtime.supports_operation(0));
  assert!(runtime.supports_operation(1));
  assert!(!runtime.supports_operation(2));

  let record = FileRecord {
    path: "/identity.bin".to_string(),
    content_type: None,
    total_size: 1,
    created_at: 0,
    updated_at: 0,
    metadata: Vec::new(),
    content_hash: vec![0x44; 32],
    chunk_hashes: vec![vec![0x55; 32]],
  };
  let extracted = runtime.value_store().extract(SourceDocumentV1 { file_record: &record, parsed_value: None }, None, &|| false).unwrap();
  let SourceExtractionV1::Values(values) = extracted else {
    panic!("hash source must exist");
  };
  let compiled = runtime.compile_source_values(&values).unwrap();
  assert_eq!(compiled.values.len(), 1);
  assert_eq!(compiled.values[0].source_value_ordinal, 0);
  assert_eq!(compiled.posting_count, 1);
  assert_eq!(compiled.values[0].postings[0].posting_key.len(), 33);

  let mismatched_value_bytes = Box::leak(value_store_fixture("avst-blake3-256-json-corrected-valid").into_boxed_slice());
  let error = IndexDefinitionRuntimeV1::from_encoded(mismatched_value_bytes, field_bytes, HashAlgorithm::Blake3_256).unwrap_err();
  assert_eq!(error.class(), IndexDefinitionErrorClassV1::IdentityMismatch);
}

#[test]
fn executable_index_definition_preserves_resource_failures_and_enforces_document_aggregate_limits() {
  let value_bytes = Box::leak(value_store_fixture("avst-blake3-256-metadata-hash-corrected-valid").into_boxed_slice());
  let value = decode_value_store_definition(value_bytes, HashAlgorithm::Blake3_256).unwrap();
  let canonical_hash = encode_canonical_value(&CanonicalConfigValueV1::Bytes(vec![0x44; 32]), CanonicalValueBounds::SOURCE_VALUE).unwrap();

  let mut converter_limited = field_index_fixture("typed_exact_blake3_v1");
  converter_limited[32..64].copy_from_slice(&value.value_store_id);
  let strategy_name_length = u16::from_le_bytes(converter_limited[104..106].try_into().unwrap()) as usize;
  let converter_start = 168 + strategy_name_length;
  converter_limited[converter_start + 64..converter_start + 72].copy_from_slice(&1u64.to_le_bytes());
  let converter_limited = Box::leak(converter_limited.into_boxed_slice());
  let runtime = IndexDefinitionRuntimeV1::from_encoded(value_bytes, converter_limited, HashAlgorithm::Blake3_256).unwrap();
  let error = runtime.compile_source_values(std::slice::from_ref(&canonical_hash)).unwrap_err();
  assert_eq!(error.class(), IndexDefinitionErrorClassV1::ResourceLimit);

  let mut posting_limited = field_index_fixture("typed_exact_blake3_v1");
  posting_limited[32..64].copy_from_slice(&value.value_store_id);
  posting_limited[120..128].copy_from_slice(&1u64.to_le_bytes());
  let posting_limited = Box::leak(posting_limited.into_boxed_slice());
  let runtime = IndexDefinitionRuntimeV1::from_encoded(value_bytes, posting_limited, HashAlgorithm::Blake3_256).unwrap();
  let error = runtime.compile_source_values(std::slice::from_ref(&canonical_hash)).unwrap_err();
  assert_eq!(error.class(), IndexDefinitionErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "index_posting_bytes_limit");

  let mut recheck_limited = field_index_fixture("typed_exact_blake3_v1");
  recheck_limited[32..64].copy_from_slice(&value.value_store_id);
  recheck_limited[128..136].copy_from_slice(&1u64.to_le_bytes());
  let recheck_limited = Box::leak(recheck_limited.into_boxed_slice());
  let runtime = IndexDefinitionRuntimeV1::from_encoded(value_bytes, recheck_limited, HashAlgorithm::Blake3_256).unwrap();
  let error = runtime.compile_source_values(&[canonical_hash]).unwrap_err();
  assert_eq!(error.class(), IndexDefinitionErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "index_recheck_bytes_limit");
}

#[test]
fn executable_index_definition_rechecks_value_store_count_before_allocating_caller_input() {
  let value_bytes = Box::leak(value_store_fixture("avst-blake3-256-metadata-hash-corrected-valid").into_boxed_slice());
  let value = decode_value_store_definition(value_bytes, HashAlgorithm::Blake3_256).unwrap();
  let maximum_values = value.max_source_values_per_document as usize;
  let mut field_bytes = field_index_fixture("typed_exact_blake3_v1");
  field_bytes[32..64].copy_from_slice(&value.value_store_id);
  let field_bytes = Box::leak(field_bytes.into_boxed_slice());
  let runtime = IndexDefinitionRuntimeV1::from_encoded(value_bytes, field_bytes, HashAlgorithm::Blake3_256).unwrap();
  let canonical_hash = encode_canonical_value(&CanonicalConfigValueV1::Bytes(vec![0x44; 32]), CanonicalValueBounds::SOURCE_VALUE).unwrap();
  let values = vec![canonical_hash; maximum_values + 1];
  let error = runtime.compile_source_values(&values).unwrap_err();
  assert_eq!(error.class(), IndexDefinitionErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "index_source_value_count_limit");
}

#[test]
fn typed_manifest_codecs_round_trip_every_independent_manifest_fixture() {
  for (profile, hash_algorithm) in [("blake3-256", HashAlgorithm::Blake3_256), ("sha512", HashAlgorithm::Sha512)] {
    for kind in ["scope-catalog", "value-store", "field-index", "field-nvt"] {
      for population in ["empty", "populated"] {
        let name = format!("aidx-{profile}-{kind}-manifest-{population}.bin");
        let expected = index_artifact_fixture(&name);
        let decoded = decode_index_manifest(&expected, hash_algorithm).unwrap();
        let encoded = encode_index_manifest(&IndexManifestWriteV1 {
          hash_algorithm,
          generation: decoded.generation,
          owner_id: decoded.owner_id,
          body: decoded.details.clone(),
        })
        .unwrap();
        assert_eq!(encoded.value, expected, "{name}");
        assert_eq!(encoded.key, decoded.key, "{name}");
      }
    }
  }
}

#[test]
fn typed_scope_value_and_state_records_round_trip_both_width_page_fixtures() {
  for (profile, hash_algorithm) in [("blake3-256", HashAlgorithm::Blake3_256), ("sha512", HashAlgorithm::Sha512)] {
    for (fixture_kind, expected_role) in [
      ("scope-ordinal", OrderedIndexRoleV1::ScopeOrdinal),
      ("scope-reverse", OrderedIndexRoleV1::ScopeReverse),
      ("value", OrderedIndexRoleV1::Value),
      ("value-document-state", OrderedIndexRoleV1::ValueDocumentState),
      ("posting", OrderedIndexRoleV1::Posting),
      ("index-document-state", OrderedIndexRoleV1::IndexDocumentState),
    ] {
      let name = format!("aidx-{profile}-{fixture_kind}-page-valid.bin");
      let fixture = index_artifact_fixture(&name);
      let page = decode_ordered_page(&fixture, hash_algorithm).unwrap();
      assert_eq!(page.role, expected_role, "{name}");
      for record in page.records.iter() {
        let record = record.unwrap();
        let encoded = match expected_role {
          OrderedIndexRoleV1::ScopeOrdinal => {
            let decoded = decode_scope_document_record(record.encoded, hash_algorithm).unwrap();
            encode_scope_document_record(&decoded, hash_algorithm).unwrap()
          }
          OrderedIndexRoleV1::ScopeReverse => {
            let decoded = decode_scope_reverse_record(record.encoded, hash_algorithm).unwrap();
            encode_scope_reverse_record(&decoded, hash_algorithm).unwrap()
          }
          OrderedIndexRoleV1::Value => {
            let decoded = decode_canonical_value_record(record.encoded, hash_algorithm).unwrap();
            encode_canonical_value_record(&decoded, hash_algorithm).unwrap()
          }
          OrderedIndexRoleV1::ValueDocumentState | OrderedIndexRoleV1::IndexDocumentState => {
            let owner = if expected_role == OrderedIndexRoleV1::ValueDocumentState {
              DocumentStateOwnerV1::ValueStore
            } else {
              DocumentStateOwnerV1::FieldIndex
            };
            let decoded = decode_document_state_record(record.encoded, owner, hash_algorithm).unwrap();
            encode_document_state_record(&decoded, owner, hash_algorithm).unwrap()
          }
          OrderedIndexRoleV1::Posting => encode_posting_record(&decode_posting_record(record.encoded).unwrap()).unwrap(),
          OrderedIndexRoleV1::NvtTile => unreachable!("fixture role is fixed above"),
        };
        assert_eq!(encoded, record.encoded, "{name}");
      }
    }
  }
}

#[test]
fn typed_page_and_directory_writers_match_every_independent_fixture() {
  for (profile, hash_algorithm) in [("blake3-256", HashAlgorithm::Blake3_256), ("sha512", HashAlgorithm::Sha512)] {
    for role_name in ["scope-ordinal", "scope-reverse", "value", "value-document-state", "posting", "index-document-state"] {
      let page_name = format!("aidx-{profile}-{role_name}-page-valid.bin");
      let expected_page = index_artifact_fixture(&page_name);
      let page = decode_ordered_page(&expected_page, hash_algorithm).unwrap();
      let records = page.records.iter().map(|record| record.unwrap().encoded).collect::<Vec<_>>();
      let encoded_page = encode_ordered_page(&OrderedPageWriteV1 {
        hash_algorithm,
        role: page.role,
        owner_id: page.owner_id,
        generation: page.generation,
        page_id: page.page_id,
        previous_page_id: page.previous_page_id,
        next_page_id: page.next_page_id,
        records: &records,
      })
      .unwrap();
      assert_eq!(encoded_page.value, expected_page, "{page_name}");
      assert_eq!(encoded_page.key, page.key, "{page_name}");

      let directory_name = format!("aidx-{profile}-{role_name}-directory-leaf-valid.bin");
      let expected_directory = index_artifact_fixture(&directory_name);
      assert_directory_writer_matches(&expected_directory, hash_algorithm, &directory_name);
    }

    let internal_name = format!("aidx-{profile}-posting-directory-internal-valid.bin");
    let expected_internal = index_artifact_fixture(&internal_name);
    assert_directory_writer_matches(&expected_internal, hash_algorithm, &internal_name);

    let nvt_name = format!("aidx-{profile}-nvt-tile-directory-leaf-valid.bin");
    let expected_nvt = index_artifact_fixture(&nvt_name);
    assert_directory_writer_matches(&expected_nvt, hash_algorithm, &nvt_name);
  }
}

#[test]
fn ordered_index_roles_close_owner_child_codec_and_page_id_contract() {
  let expected = [
    (OrderedIndexRoleV1::ScopeOrdinal, 1, ImmutableIndexArtifactKindV1::ScopeCatalogPage, 1, false),
    (OrderedIndexRoleV1::ScopeReverse, 1, ImmutableIndexArtifactKindV1::ScopeCatalogPage, 2, false),
    (OrderedIndexRoleV1::Value, 2, ImmutableIndexArtifactKindV1::ValuePage, 3, true),
    (OrderedIndexRoleV1::ValueDocumentState, 2, ImmutableIndexArtifactKindV1::DocumentStatePage, 1, true),
    (OrderedIndexRoleV1::Posting, 3, ImmutableIndexArtifactKindV1::PostingPage, 4, true),
    (OrderedIndexRoleV1::IndexDocumentState, 3, ImmutableIndexArtifactKindV1::DocumentStatePage, 1, true),
    (OrderedIndexRoleV1::NvtTile, 3, ImmutableIndexArtifactKindV1::NvtTile, 5, false),
  ];
  for (role, owner_class, child_kind, key_codec, uses_page_id) in expected {
    assert_eq!(role.owner_class(), owner_class, "{role:?}");
    assert_eq!(role.child_kind(), child_kind, "{role:?}");
    assert_eq!(role.key_codec(), key_codec, "{role:?}");
    assert_eq!(role.uses_page_id(), uses_page_id, "{role:?}");
  }
}

fn assert_directory_writer_matches(expected: &[u8], hash_algorithm: HashAlgorithm, name: &str) {
  let directory = decode_artifact_directory(expected, hash_algorithm).unwrap();
  let entries = directory
    .entries
    .iter()
    .map(|entry| ArtifactDirectoryEntryWriteV1 {
      lower_fence: entry.lower_fence,
      upper_fence: entry.upper_fence,
      child_hash: entry.child_hash,
      child_generation: entry.child_generation,
      live_count: entry.live_count,
      tombstone_count: entry.tombstone_count,
      page_count: entry.page_count,
      logical_bytes: entry.logical_bytes,
      minimum_page_id: entry.minimum_page_id,
      maximum_page_id: entry.maximum_page_id,
      physical_hint: entry.physical_hint,
    })
    .collect::<Vec<_>>();
  let encoded = encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm,
    role: directory.role,
    owner_id: directory.owner_id,
    generation: directory.generation,
    level: directory.level,
    entries: &entries,
  })
  .unwrap();
  assert_eq!(encoded.value, expected, "{name}");
  assert_eq!(encoded.key, directory.key, "{name}");
}

#[test]
fn posting_page_writer_exposes_and_validates_adjacent_links_without_collecting_a_chain() {
  let fixture = index_artifact_fixture("aidx-blake3-256-posting-page-valid.bin");
  let page = decode_ordered_page(&fixture, HashAlgorithm::Blake3_256).unwrap();
  let records = page.records.iter().map(|record| record.unwrap().encoded).collect::<Vec<_>>();
  assert_eq!(records.len(), 2);

  let left_bytes = encode_ordered_page(&OrderedPageWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    role: OrderedIndexRoleV1::Posting,
    owner_id: page.owner_id,
    generation: page.generation,
    page_id: page.page_id,
    previous_page_id: 0,
    next_page_id: page.page_id + 1,
    records: &records[..1],
  })
  .unwrap();
  let right_bytes = encode_ordered_page(&OrderedPageWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    role: OrderedIndexRoleV1::Posting,
    owner_id: page.owner_id,
    generation: page.generation + 1,
    page_id: page.page_id + 1,
    previous_page_id: page.page_id,
    next_page_id: 0,
    records: &records[1..],
  })
  .unwrap();
  let left = decode_ordered_page(&left_bytes.value, HashAlgorithm::Blake3_256).unwrap();
  let right = decode_ordered_page(&right_bytes.value, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!((left.previous_page_id, left.next_page_id), (0, right.page_id));
  assert_eq!((right.previous_page_id, right.next_page_id), (left.page_id, 0));
  validate_posting_page_link(&left, &right, HashAlgorithm::Blake3_256).unwrap();

  let detached_bytes = encode_ordered_page(&OrderedPageWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    role: OrderedIndexRoleV1::Posting,
    owner_id: page.owner_id,
    generation: page.generation + 1,
    page_id: page.page_id + 1,
    previous_page_id: 0,
    next_page_id: 0,
    records: &records[1..],
  })
  .unwrap();
  let detached = decode_ordered_page(&detached_bytes.value, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(
    validate_posting_page_link(&left, &detached, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let overlap_bytes = encode_ordered_page(&OrderedPageWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    role: OrderedIndexRoleV1::Posting,
    owner_id: page.owner_id,
    generation: page.generation + 1,
    page_id: page.page_id + 1,
    previous_page_id: page.page_id,
    next_page_id: 0,
    records: &records[..1],
  })
  .unwrap();
  let overlap = decode_ordered_page(&overlap_bytes.value, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(
    validate_posting_page_link(&left, &overlap, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );

  let value_fixture = index_artifact_fixture("aidx-blake3-256-value-page-valid.bin");
  let value_page = decode_ordered_page(&value_fixture, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(
    validate_posting_page_link(&value_page, &right, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
}

#[test]
fn typed_page_and_directory_writers_reject_noncanonical_inputs_before_publication() {
  let fixture = index_artifact_fixture("aidx-blake3-256-posting-page-valid.bin");
  let page = decode_ordered_page(&fixture, HashAlgorithm::Blake3_256).unwrap();
  let records = page.records.iter().map(|record| record.unwrap().encoded).collect::<Vec<_>>();
  let valid_page = OrderedPageWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    role: page.role,
    owner_id: page.owner_id,
    generation: page.generation,
    page_id: page.page_id,
    previous_page_id: 0,
    next_page_id: 0,
    records: &records,
  };

  let zero_owner = vec![0u8; 32];
  assert_eq!(
    encode_ordered_page(&OrderedPageWriteV1 { owner_id: &zero_owner, ..valid_page }).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  assert_eq!(
    encode_ordered_page(&OrderedPageWriteV1 { generation: 0, ..valid_page }).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  assert_eq!(
    encode_ordered_page(&OrderedPageWriteV1 { page_id: 0, ..valid_page }).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  assert_eq!(
    encode_ordered_page(&OrderedPageWriteV1 { role: OrderedIndexRoleV1::NvtTile, page_id: 0, ..valid_page }).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );
  assert_eq!(
    encode_ordered_page(&OrderedPageWriteV1 { previous_page_id: page.page_id, ..valid_page }).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  assert_eq!(
    encode_ordered_page(&OrderedPageWriteV1 { records: &[], ..valid_page }).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let reversed = [records[1], records[0]];
  assert_eq!(
    encode_ordered_page(&OrderedPageWriteV1 { records: &reversed, ..valid_page }).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );
  let mut trailing_record = records[0].to_vec();
  trailing_record.push(0);
  assert_eq!(
    encode_ordered_page(&OrderedPageWriteV1 { records: &[&trailing_record], ..valid_page }).unwrap_err().class(),
    MalformedInputClass::TruncationOrTrailingBytes
  );

  let non_posting_links = OrderedPageWriteV1 { role: OrderedIndexRoleV1::Value, previous_page_id: 1, ..valid_page };
  assert_eq!(encode_ordered_page(&non_posting_links).unwrap_err().class(), MalformedInputClass::NonzeroReservedOrPadding);

  let duplicate_records = OrderedPageWriteV1 { role: OrderedIndexRoleV1::Posting, records: &[records[0], records[0]], ..valid_page };
  assert_eq!(encode_ordered_page(&duplicate_records).unwrap_err().class(), MalformedInputClass::NoncanonicalOrderOrDuplicate);

  let scope_fixture = index_artifact_fixture("aidx-blake3-256-scope-ordinal-page-valid.bin");
  let scope_page = decode_ordered_page(&scope_fixture, HashAlgorithm::Blake3_256).unwrap();
  let scope_records = scope_page.records.iter().map(|record| record.unwrap().encoded).collect::<Vec<_>>();
  let invalid_scope_page = OrderedPageWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    role: scope_page.role,
    owner_id: scope_page.owner_id,
    generation: scope_page.generation,
    page_id: 1,
    previous_page_id: 0,
    next_page_id: 0,
    records: &scope_records,
  };
  assert_eq!(encode_ordered_page(&invalid_scope_page).unwrap_err().class(), MalformedInputClass::IdentityKeyOrGenerationMismatch);

  let huge_key_a = vec![0x41; 1_024 * 1_024];
  let huge_key_b = vec![0x42; 1_024 * 1_024];
  let huge_record_a = encode_posting_record(&PostingRecordV1 {
    tombstone: false,
    coordinate: 1,
    document_ordinal: 1,
    source_value_ordinal: 0,
    expansion_ordinal: 0,
    posting_key: &huge_key_a,
  })
  .unwrap();
  let huge_record_b = encode_posting_record(&PostingRecordV1 {
    tombstone: false,
    coordinate: 2,
    document_ordinal: 2,
    source_value_ordinal: 0,
    expansion_ordinal: 0,
    posting_key: &huge_key_b,
  })
  .unwrap();
  assert_eq!(
    encode_ordered_page(&OrderedPageWriteV1 { records: &[&huge_record_a, &huge_record_b], ..valid_page }).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let directory_fixture = index_artifact_fixture("aidx-blake3-256-posting-directory-leaf-valid.bin");
  let directory = decode_artifact_directory(&directory_fixture, HashAlgorithm::Blake3_256).unwrap();
  let child = &directory.entries[0];
  let valid_child = ArtifactDirectoryEntryWriteV1 {
    lower_fence: child.lower_fence,
    upper_fence: child.upper_fence,
    child_hash: child.child_hash,
    child_generation: child.child_generation,
    live_count: child.live_count,
    tombstone_count: child.tombstone_count,
    page_count: child.page_count,
    logical_bytes: child.logical_bytes,
    minimum_page_id: child.minimum_page_id,
    maximum_page_id: child.maximum_page_id,
    physical_hint: child.physical_hint,
  };
  let valid_directory = ArtifactDirectoryWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    role: directory.role,
    owner_id: directory.owner_id,
    generation: directory.generation,
    level: directory.level,
    entries: &[valid_child],
  };
  assert_eq!(
    encode_artifact_directory(&ArtifactDirectoryWriteV1 { owner_id: &zero_owner, ..valid_directory }).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  assert_eq!(
    encode_artifact_directory(&ArtifactDirectoryWriteV1 { generation: 0, ..valid_directory }).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  assert_eq!(
    encode_artifact_directory(&ArtifactDirectoryWriteV1 { level: 16, ..valid_directory }).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );
  assert_eq!(
    encode_artifact_directory(&ArtifactDirectoryWriteV1 { entries: &[], ..valid_directory }).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );
  assert_eq!(
    encode_artifact_directory(&ArtifactDirectoryWriteV1 {
      entries: &[ArtifactDirectoryEntryWriteV1 { child_generation: directory.generation + 1, ..valid_child }],
      ..valid_directory
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  assert_eq!(
    encode_artifact_directory(&ArtifactDirectoryWriteV1 {
      entries: &[ArtifactDirectoryEntryWriteV1 { live_count: 0, tombstone_count: 0, ..valid_child }],
      ..valid_directory
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  assert_eq!(
    encode_artifact_directory(&ArtifactDirectoryWriteV1 {
      entries: &[ArtifactDirectoryEntryWriteV1 { logical_bytes: 0, ..valid_child }],
      ..valid_directory
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  assert_eq!(
    encode_artifact_directory(&ArtifactDirectoryWriteV1 {
      entries: &[ArtifactDirectoryEntryWriteV1 { page_count: 2, ..valid_child }],
      ..valid_directory
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  assert_eq!(
    encode_artifact_directory(&ArtifactDirectoryWriteV1 {
      entries: &[ArtifactDirectoryEntryWriteV1 { minimum_page_id: 0, maximum_page_id: 0, ..valid_child }],
      ..valid_directory
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  assert_eq!(
    encode_artifact_directory(&ArtifactDirectoryWriteV1 {
      level: 1,
      entries: &[ArtifactDirectoryEntryWriteV1 { page_count: 0, ..valid_child }],
      ..valid_directory
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  assert_eq!(
    encode_artifact_directory(&ArtifactDirectoryWriteV1 { entries: &[valid_child, valid_child], ..valid_directory }).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );

  let zero_hash = vec![0u8; 32];
  assert_eq!(
    encode_artifact_directory(&ArtifactDirectoryWriteV1 {
      entries: &[ArtifactDirectoryEntryWriteV1 { child_hash: &zero_hash, ..valid_child }],
      ..valid_directory
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );

  let too_many_entries = vec![valid_child; 65_537];
  assert_eq!(
    encode_artifact_directory(&ArtifactDirectoryWriteV1 { entries: &too_many_entries, ..valid_directory }).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let mut overflow_fence = child.upper_fence.to_vec();
  overflow_fence[..8].copy_from_slice(&u64::MAX.to_le_bytes());
  let overflow_entries = [
    ArtifactDirectoryEntryWriteV1 { live_count: u64::MAX, ..valid_child },
    ArtifactDirectoryEntryWriteV1 { lower_fence: &overflow_fence, upper_fence: &overflow_fence, live_count: 1, ..valid_child },
  ];
  assert_eq!(
    encode_artifact_directory(&ArtifactDirectoryWriteV1 { entries: &overflow_entries, ..valid_directory }).unwrap_err().class(),
    MalformedInputClass::LengthCountOrArithmeticOverflow
  );

  let scope_directory_fixture = index_artifact_fixture("aidx-blake3-256-scope-ordinal-directory-leaf-valid.bin");
  let scope_directory = decode_artifact_directory(&scope_directory_fixture, HashAlgorithm::Blake3_256).unwrap();
  let scope_child = &scope_directory.entries[0];
  let invalid_scope_child = ArtifactDirectoryEntryWriteV1 {
    lower_fence: scope_child.lower_fence,
    upper_fence: scope_child.upper_fence,
    child_hash: scope_child.child_hash,
    child_generation: scope_child.child_generation,
    live_count: scope_child.live_count,
    tombstone_count: scope_child.tombstone_count,
    page_count: scope_child.page_count,
    logical_bytes: scope_child.logical_bytes,
    minimum_page_id: 1,
    maximum_page_id: 1,
    physical_hint: scope_child.physical_hint,
  };
  assert_eq!(
    encode_artifact_directory(&ArtifactDirectoryWriteV1 {
      hash_algorithm: HashAlgorithm::Blake3_256,
      role: scope_directory.role,
      owner_id: scope_directory.owner_id,
      generation: scope_directory.generation,
      level: scope_directory.level,
      entries: &[invalid_scope_child],
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let oversized_fence = vec![0u8; 1_024 * 1_024 + 1];
  assert_eq!(
    encode_artifact_directory(&ArtifactDirectoryWriteV1 {
      entries: &[ArtifactDirectoryEntryWriteV1 { lower_fence: &oversized_fence, upper_fence: &oversized_fence, ..valid_child }],
      ..valid_directory
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::AllocationAmplification
  );

  let mut first_fence = vec![0u8; 25];
  first_fence[8] = 1;
  let mut middle_oversized_fence = vec![0u8; 1_024 * 1_024 + 1];
  middle_oversized_fence[..8].copy_from_slice(&1u64.to_le_bytes());
  middle_oversized_fence[8] = 1;
  let mut last_fence = vec![0u8; 25];
  last_fence[..8].copy_from_slice(&2u64.to_le_bytes());
  last_fence[8] = 1;
  let middle_oversized_entries = [
    ArtifactDirectoryEntryWriteV1 { lower_fence: &first_fence, upper_fence: &first_fence, ..valid_child },
    ArtifactDirectoryEntryWriteV1 { lower_fence: &middle_oversized_fence, upper_fence: &middle_oversized_fence, ..valid_child },
    ArtifactDirectoryEntryWriteV1 { lower_fence: &last_fence, upper_fence: &last_fence, ..valid_child },
  ];
  assert_eq!(
    encode_artifact_directory(&ArtifactDirectoryWriteV1 { entries: &middle_oversized_entries, ..valid_directory }).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let maximum_fence = vec![0u8; 1_024 * 1_024];
  assert_eq!(
    encode_artifact_directory(&ArtifactDirectoryWriteV1 {
      entries: &[ArtifactDirectoryEntryWriteV1 { lower_fence: &maximum_fence, upper_fence: &maximum_fence, ..valid_child }],
      ..valid_directory
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::AllocationAmplification
  );
}

#[test]
fn typed_posting_record_codec_rejects_identity_framing_and_size_errors() {
  let fixture = index_artifact_fixture("aidx-blake3-256-posting-page-valid.bin");
  let page = decode_ordered_page(&fixture, HashAlgorithm::Blake3_256).unwrap();
  let encoded = page.records.iter().next().unwrap().unwrap().encoded;
  let decoded = decode_posting_record(encoded).unwrap();

  let zero_ordinal = PostingRecordV1 { document_ordinal: 0, ..decoded.clone() };
  assert_eq!(encode_posting_record(&zero_ordinal).unwrap_err().class(), MalformedInputClass::IdentityKeyOrGenerationMismatch);

  let empty_key = PostingRecordV1 { posting_key: &[], ..decoded.clone() };
  assert_eq!(encode_posting_record(&empty_key).unwrap_err().class(), MalformedInputClass::AllocationAmplification);

  let mut trailing = encoded.to_vec();
  trailing.push(0);
  assert_eq!(decode_posting_record(&trailing).unwrap_err().class(), MalformedInputClass::TruncationOrTrailingBytes);

  let mut reserved = encoded.to_vec();
  reserved[1] = 1;
  assert_eq!(decode_posting_record(&reserved).unwrap_err().class(), MalformedInputClass::NonzeroReservedOrPadding);

  let mut zero_ordinal_bytes = encoded.to_vec();
  zero_ordinal_bytes[16..24].copy_from_slice(&0u64.to_le_bytes());
  assert_eq!(decode_posting_record(&zero_ordinal_bytes).unwrap_err().class(), MalformedInputClass::IdentityKeyOrGenerationMismatch);

  let oversized_key = vec![0u8; 1_024 * 1_024 + 1];
  let oversized = PostingRecordV1 { posting_key: &oversized_key, ..decoded };
  assert_eq!(encode_posting_record(&oversized).unwrap_err().class(), MalformedInputClass::AllocationAmplification);
}

#[test]
fn typed_manifest_chain_requires_exact_references_coverage_and_semantic_owners() {
  for (profile, hash_algorithm) in [("blake3-256", HashAlgorithm::Blake3_256), ("sha512", HashAlgorithm::Sha512)] {
    let scope_empty_bytes = index_artifact_fixture(&format!("aidx-{profile}-scope-catalog-manifest-empty.bin"));
    let scope_populated_bytes = index_artifact_fixture(&format!("aidx-{profile}-scope-catalog-manifest-populated.bin"));
    let value_empty_bytes = index_artifact_fixture(&format!("aidx-{profile}-value-store-manifest-empty.bin"));
    let value_populated_bytes = index_artifact_fixture(&format!("aidx-{profile}-value-store-manifest-populated.bin"));
    let field_empty_bytes = index_artifact_fixture(&format!("aidx-{profile}-field-index-manifest-empty.bin"));
    let field_populated_bytes = index_artifact_fixture(&format!("aidx-{profile}-field-index-manifest-populated.bin"));
    let scope_empty = decode_index_manifest(&scope_empty_bytes, hash_algorithm).unwrap();
    let scope_populated = decode_index_manifest(&scope_populated_bytes, hash_algorithm).unwrap();
    let value_empty = decode_index_manifest(&value_empty_bytes, hash_algorithm).unwrap();
    let value_populated = decode_index_manifest(&value_populated_bytes, hash_algorithm).unwrap();
    let field_empty = decode_index_manifest(&field_empty_bytes, hash_algorithm).unwrap();
    let field_populated = decode_index_manifest(&field_populated_bytes, hash_algorithm).unwrap();

    validate_correctness_manifest_chain(&scope_empty, &value_empty, &field_empty, hash_algorithm).unwrap();
    validate_correctness_manifest_chain(&scope_populated, &value_populated, &field_populated, hash_algorithm).unwrap();
    assert_eq!(
      validate_correctness_manifest_chain(&scope_populated, &value_empty, &field_empty, hash_algorithm).unwrap_err().class(),
      MalformedInputClass::CrossRecordClosureMismatch
    );
    assert_eq!(
      validate_correctness_manifest_chain(&scope_populated, &value_populated, &field_empty, hash_algorithm).unwrap_err().class(),
      MalformedInputClass::CrossRecordClosureMismatch
    );

    let mut changed_details = field_populated.details.clone();
    let IndexManifestBodyV1::FieldIndex(changed_field) = &mut changed_details else {
      panic!("fixture is a field manifest");
    };
    changed_field.coverage.coverage_publication_sequence = changed_field.coverage.coverage_publication_sequence.checked_add(1).unwrap();
    let changed = encode_index_manifest(&IndexManifestWriteV1 {
      hash_algorithm,
      generation: field_populated.generation,
      owner_id: field_populated.owner_id,
      body: changed_details,
    })
    .unwrap();
    let changed = decode_index_manifest(&changed.value, hash_algorithm).unwrap();
    assert_eq!(
      validate_correctness_manifest_chain(&scope_populated, &value_populated, &changed, hash_algorithm).unwrap_err().class(),
      MalformedInputClass::CrossRecordClosureMismatch
    );
  }
}

#[test]
fn typed_record_codecs_reject_reserved_ordinals_owner_confusion_and_unbounded_evidence() {
  for (profile, hash_algorithm) in [("blake3-256", HashAlgorithm::Blake3_256), ("sha512", HashAlgorithm::Sha512)] {
    let scope_bytes = index_artifact_fixture(&format!("aidx-{profile}-scope-ordinal-page-valid.bin"));
    let scope_page = decode_ordered_page(&scope_bytes, hash_algorithm).unwrap();
    let mut scope_record = scope_page.records.iter().next().unwrap().unwrap().encoded.to_vec();
    scope_record[8..16].copy_from_slice(&0u64.to_le_bytes());
    assert_eq!(
      decode_scope_document_record(&scope_record, hash_algorithm).unwrap_err().class(),
      MalformedInputClass::IdentityKeyOrGenerationMismatch
    );

    let reverse_bytes = index_artifact_fixture(&format!("aidx-{profile}-scope-reverse-page-valid.bin"));
    let reverse_page = decode_ordered_page(&reverse_bytes, hash_algorithm).unwrap();
    let mut reverse_record = reverse_page.records.iter().next().unwrap().unwrap().encoded.to_vec();
    reverse_record[0] = 1;
    assert_eq!(
      decode_scope_reverse_record(&reverse_record, hash_algorithm).unwrap_err().class(),
      MalformedInputClass::CrossRecordClosureMismatch
    );

    let value_bytes = index_artifact_fixture(&format!("aidx-{profile}-value-page-valid.bin"));
    let value_page = decode_ordered_page(&value_bytes, hash_algorithm).unwrap();
    let mut value_record = value_page.records.iter().next().unwrap().unwrap().encoded.to_vec();
    value_record.push(0);
    assert_eq!(
      decode_canonical_value_record(&value_record, hash_algorithm).unwrap_err().class(),
      MalformedInputClass::TruncationOrTrailingBytes
    );
    let decoded_value = decode_canonical_value_record(&value_page.records.iter().next().unwrap().unwrap().encoded, hash_algorithm).unwrap();
    let ambiguous_tombstone = aeordb::engine::v4::index_record::CanonicalValueRecordV1 {
      tombstone: true,
      document_ordinal: decoded_value.document_ordinal,
      source_value_ordinal: decoded_value.source_value_ordinal,
      record_revision_hash: decoded_value.record_revision_hash,
      canonical_value: Some(&[]),
    };
    assert_eq!(
      encode_canonical_value_record(&ambiguous_tombstone, hash_algorithm).unwrap_err().class(),
      MalformedInputClass::CrossRecordClosureMismatch
    );

    let state_bytes = index_artifact_fixture(&format!("aidx-{profile}-value-document-state-page-valid.bin"));
    let state_page = decode_ordered_page(&state_bytes, hash_algorithm).unwrap();
    let state_record = state_page.records.iter().next().unwrap().unwrap();
    assert_eq!(
      decode_document_state_record(state_record.encoded, DocumentStateOwnerV1::FieldIndex, hash_algorithm).unwrap_err().class(),
      MalformedInputClass::CrossRecordClosureMismatch
    );
    let decoded = decode_document_state_record(state_record.encoded, DocumentStateOwnerV1::ValueStore, hash_algorithm).unwrap();
    let oversized_evidence = vec![0u8; 4 * 1_024 + 1];
    let oversized = DocumentStateRecordV1 {
      tombstone: decoded.tombstone,
      stage: decoded.stage,
      reason: decoded.reason,
      document_ordinal: decoded.document_ordinal,
      record_revision_hash: decoded.record_revision_hash,
      observed_value_count: decoded.observed_value_count,
      observed_canonical_bytes: decoded.observed_canonical_bytes,
      observed_work_units: decoded.observed_work_units,
      dependency_ordinal: decoded.dependency_ordinal,
      evidence: &oversized_evidence,
    };
    assert_eq!(
      encode_document_state_record(&oversized, DocumentStateOwnerV1::ValueStore, hash_algorithm).unwrap_err().class(),
      MalformedInputClass::AllocationAmplification
    );
  }
}

#[test]
fn typed_manifest_writers_reject_identity_capability_and_root_count_corruption() {
  for (profile, hash_algorithm) in [("blake3-256", HashAlgorithm::Blake3_256), ("sha512", HashAlgorithm::Sha512)] {
    let scope_bytes = index_artifact_fixture(&format!("aidx-{profile}-scope-catalog-manifest-populated.bin"));
    let scope = decode_index_manifest(&scope_bytes, hash_algorithm).unwrap();

    let zero_generation = IndexManifestWriteV1 { hash_algorithm, generation: 0, owner_id: scope.owner_id, body: scope.details.clone() };
    assert_eq!(encode_index_manifest(&zero_generation).unwrap_err().class(), MalformedInputClass::IdentityKeyOrGenerationMismatch);

    let wrong_width_owner = IndexManifestWriteV1 {
      hash_algorithm,
      generation: scope.generation,
      owner_id: &scope.owner_id[..scope.owner_id.len() - 1],
      body: scope.details.clone(),
    };
    assert_eq!(encode_index_manifest(&wrong_width_owner).unwrap_err().class(), MalformedInputClass::IdentityKeyOrGenerationMismatch);

    let mut unknown_capability = scope.details.clone();
    let IndexManifestBodyV1::ScopeCatalog(body) = &mut unknown_capability else {
      panic!("fixture is a scope manifest");
    };
    body.required_reader_capabilities[3] = 1;
    assert_eq!(
      encode_index_manifest(&IndexManifestWriteV1 {
        hash_algorithm,
        generation: scope.generation,
        owner_id: scope.owner_id,
        body: unknown_capability,
      })
      .unwrap_err()
      .class(),
      MalformedInputClass::UnknownRequiredCapability
    );

    let mut broken_scope_counts = scope.details.clone();
    let IndexManifestBodyV1::ScopeCatalog(body) = &mut broken_scope_counts else {
      panic!("fixture is a scope manifest");
    };
    body.ordinal_page_count = 0;
    assert_eq!(
      encode_index_manifest(&IndexManifestWriteV1 {
        hash_algorithm,
        generation: scope.generation,
        owner_id: scope.owner_id,
        body: broken_scope_counts,
      })
      .unwrap_err()
      .class(),
      MalformedInputClass::CrossRecordClosureMismatch
    );

    let value_bytes = index_artifact_fixture(&format!("aidx-{profile}-value-store-manifest-populated.bin"));
    let value = decode_index_manifest(&value_bytes, hash_algorithm).unwrap();
    let mut zero_next_page = value.details.clone();
    let IndexManifestBodyV1::ValueStore(body) = &mut zero_next_page else {
      panic!("fixture is a value-store manifest");
    };
    body.next_page_id = 0;
    assert_eq!(
      encode_index_manifest(&IndexManifestWriteV1 {
        hash_algorithm,
        generation: value.generation,
        owner_id: value.owner_id,
        body: zero_next_page,
      })
      .unwrap_err()
      .class(),
      MalformedInputClass::IdentityKeyOrGenerationMismatch
    );
  }
}

#[test]
fn typed_record_writers_reject_wrong_hash_width_and_file_key_mismatch() {
  for (profile, hash_algorithm) in [("blake3-256", HashAlgorithm::Blake3_256), ("sha512", HashAlgorithm::Sha512)] {
    let scope_bytes = index_artifact_fixture(&format!("aidx-{profile}-scope-ordinal-page-valid.bin"));
    let scope_page = decode_ordered_page(&scope_bytes, hash_algorithm).unwrap();
    let encoded_record = scope_page.records.iter().next().unwrap().unwrap().encoded;
    let mut record = decode_scope_document_record(encoded_record, hash_algorithm).unwrap();

    let wrong_width_revision = vec![0x44; hash_algorithm.hash_length() - 1];
    record.record_revision_hash = &wrong_width_revision;
    assert_eq!(
      encode_scope_document_record(&record, hash_algorithm).unwrap_err().class(),
      MalformedInputClass::IdentityKeyOrGenerationMismatch
    );

    record = decode_scope_document_record(encoded_record, hash_algorithm).unwrap();
    let mismatched_file_key = vec![0x55; hash_algorithm.hash_length()];
    record.file_key = &mismatched_file_key;
    assert_eq!(
      encode_scope_document_record(&record, hash_algorithm).unwrap_err().class(),
      MalformedInputClass::IdentityKeyOrGenerationMismatch
    );
  }
}
