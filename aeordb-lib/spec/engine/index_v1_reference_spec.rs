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
  assert!(matches!(outcome, SourceExtractionV1::DeterministicUnindexable { code: "source_value_limit", .. }));

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
