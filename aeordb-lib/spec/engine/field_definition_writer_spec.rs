//! Canonical definition writer regressions against independent frozen bytes.
use aeordb::engine::v4::field_definition::{
  decode_converter_definition, decode_field_index_definition, encode_converter_definition, encode_field_index_definition,
  ConverterDefinitionWriteV1, FieldIndexDefinitionWriteV1,
};
use aeordb::engine::v4::reader::MalformedInputClass;
use aeordb::engine::HashAlgorithm;
use sha2::Digest;

const CONVERTERS: [&str; 25] = [
  "typed_exact_blake3_v1",
  "bytes_binary_order_v1",
  "utf8_binary_order_v1",
  "u64_order_v1",
  "i64_order_v1",
  "f64_finite_order_v1",
  "timestamp_ms_order_v1",
  "bool_order_v1",
  "unicode_trigram_v1",
  "soundex_ascii_v1",
  "double_metaphone_primary_ascii_v1",
  "double_metaphone_alt_ascii_v1",
  "hash_v0",
  "u8_v0",
  "u16_v0",
  "u32_v0",
  "u64_v0",
  "i64_v0",
  "f64_v0",
  "string_v0",
  "timestamp_v0",
  "trigram_v0",
  "soundex_v0",
  "dmetaphone_primary_v0",
  "dmetaphone_alt_v0",
];

fn fixture(family: &str, prefix: &str, profile: &str, converter: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/{family}/{prefix}-{profile}-{converter}-valid.bin", env!("CARGO_MANIFEST_DIR"),)).unwrap()
}

fn unsigned16(bytes: &[u8], offset: usize) -> u16 {
  u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn unsigned32(bytes: &[u8], offset: usize) -> u32 {
  u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn unsigned64(bytes: &[u8], offset: usize) -> u64 {
  u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn converter_request(bytes: &[u8]) -> ConverterDefinitionWriteV1<'_> {
  // Round 11 fixed offsets, not the production decoder's interpretation.
  assert_eq!(&bytes[..4], b"ACNV");
  assert_eq!(bytes.len(), 120 + unsigned32(bytes, 56) as usize);
  ConverterDefinitionWriteV1 {
    converter_id: unsigned16(bytes, 32),
    max_input_bytes: unsigned64(bytes, 64),
    max_output_values: unsigned32(bytes, 72),
    max_output_value_bytes: unsigned32(bytes, 76),
    max_total_output_bytes: unsigned64(bytes, 80),
    parameters: &bytes[120..],
  }
}

fn field_request(bytes: &[u8], algorithm: HashAlgorithm) -> FieldIndexDefinitionWriteV1<'_> {
  let fixed = 32 + algorithm.hash_length();
  let converter_start = fixed + 104 + usize::from(unsigned16(bytes, fixed + 40));
  assert_eq!(&bytes[..4], b"AFIX");
  assert_eq!(bytes.len(), converter_start + unsigned32(bytes, fixed + 36) as usize);
  FieldIndexDefinitionWriteV1 {
    value_store_id: &bytes[32..fixed],
    converter_definition: &bytes[converter_start..],
    max_terms_per_document: unsigned32(bytes, fixed + 44),
    max_postings_per_document: unsigned32(bytes, fixed + 48),
    max_canonical_posting_bytes_per_document: unsigned64(bytes, fixed + 56),
    max_query_recheck_value_bytes: unsigned64(bytes, fixed + 64),
  }
}

fn independent_identity(algorithm: HashAlgorithm, domain: &[u8], bytes: &[u8]) -> Vec<u8> {
  let mut input = domain.to_vec();
  input.extend_from_slice(bytes);
  match algorithm {
    HashAlgorithm::Blake3_256 => blake3::hash(&input).as_bytes().to_vec(),
    HashAlgorithm::Sha256 => sha2::Sha256::digest(&input).to_vec(),
    HashAlgorithm::Sha512 => sha2::Sha512::digest(&input).to_vec(),
    HashAlgorithm::Sha3_256 => sha3::Sha3_256::digest(&input).to_vec(),
    HashAlgorithm::Sha3_512 => sha3::Sha3_512::digest(&input).to_vec(),
  }
}

#[test]
fn converter_writer_matches_all_fifty_independent_registry_fixtures() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    for converter in CONVERTERS {
      let expected = fixture("converter-definition-v1", "acnv", profile, converter);
      let encoded = encode_converter_definition(converter_request(&expected), algorithm).unwrap();
      assert_eq!(encoded.value, expected, "{profile}/{converter}");
      assert_eq!(encoded.converter_fingerprint, independent_identity(algorithm, b"aeordb.index.converter-definition.v1\0", &expected));
      assert_eq!(decode_converter_definition(&encoded.value, algorithm).unwrap().converter_fingerprint, encoded.converter_fingerprint);
    }
  }
}

#[test]
fn field_writer_derives_exact_registry_semantics_in_all_fifty_independent_fixtures() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    for converter in CONVERTERS {
      let expected = fixture("field-index-definition-v1", "afix", profile, converter);
      let encoded = encode_field_index_definition(field_request(&expected, algorithm), algorithm).unwrap();
      assert_eq!(encoded.value, expected, "{profile}/{converter}");
      assert_eq!(encoded.index_id, independent_identity(algorithm, b"aeordb.index.field-definition.v1\0", &expected));
      assert_eq!(decode_field_index_definition(&encoded.value, algorithm).unwrap().index_id, encoded.index_id);
    }
  }
}

#[test]
fn definition_writers_use_all_database_hash_algorithms_without_changing_semantic_bytes() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let profile = if algorithm.hash_length() == 64 { "sha512" } else { "blake3-256" };
    let converter = fixture("converter-definition-v1", "acnv", profile, "u8_v0");
    let encoded = encode_converter_definition(converter_request(&converter), algorithm).unwrap();
    assert_eq!(encoded.value, converter);
    assert_eq!(encoded.converter_fingerprint, independent_identity(algorithm, b"aeordb.index.converter-definition.v1\0", &converter));
    let field = fixture("field-index-definition-v1", "afix", profile, "u8_v0");
    let encoded = encode_field_index_definition(field_request(&field, algorithm), algorithm).unwrap();
    assert_eq!(encoded.value, field);
    assert_eq!(encoded.index_id, independent_identity(algorithm, b"aeordb.index.field-definition.v1\0", &field));
  }
}

#[test]
fn converter_writer_rejects_unknown_ids_invalid_parameters_and_limits() {
  let algorithm = HashAlgorithm::Blake3_256;
  let bytes = fixture("converter-definition-v1", "acnv", "blake3-256", "typed_exact_blake3_v1");
  for identifier in [0, 13, 0x8000, 0x800e, u16::MAX] {
    let mut request = converter_request(&bytes);
    request.converter_id = identifier;
    assert!(encode_converter_definition(request, algorithm).is_err());
  }
  for field in 0..4 {
    let mut request = converter_request(&bytes);
    match field {
      0 => request.max_input_bytes = 0,
      1 => request.max_output_values = 0,
      2 => request.max_output_value_bytes = 0,
      3 => request.max_total_output_bytes = 0,
      _ => unreachable!(),
    }
    assert_eq!(encode_converter_definition(request, algorithm).unwrap_err().class(), MalformedInputClass::AllocationAmplification);
  }
  for count in [65_536, 65_537] {
    let mut request = converter_request(&bytes);
    request.max_output_values = count;
    assert_eq!(encode_converter_definition(request, algorithm).is_ok(), count == 65_536);
  }
  for converter in CONVERTERS {
    let bytes = fixture("converter-definition-v1", "acnv", "blake3-256", converter);
    let original = converter_request(&bytes);
    let mut wrong_parameters = original.parameters.to_vec();
    wrong_parameters.push(0);
    let mut request = converter_request(&bytes);
    request.parameters = &wrong_parameters;
    assert!(encode_converter_definition(request, algorithm).is_err(), "{converter}");
  }
  let string = fixture("converter-definition-v1", "acnv", "blake3-256", "string_v0");
  let mut request = converter_request(&string);
  request.parameters = &[0; 4];
  assert!(encode_converter_definition(request, algorithm).is_err());
  let excessive = vec![0; 65_536 - 120 + 1];
  let mut request = converter_request(&bytes);
  request.parameters = &excessive;
  assert_eq!(encode_converter_definition(request, algorithm).unwrap_err().class(), MalformedInputClass::AllocationAmplification);
}

#[test]
fn field_writer_rejects_wrong_width_zero_identity_malformed_converter_and_excessive_bounds() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let bytes = fixture("field-index-definition-v1", "afix", profile, "typed_exact_blake3_v1");
    for identity in [vec![], vec![0; algorithm.hash_length()], vec![1; algorithm.hash_length() - 1], vec![1; algorithm.hash_length() + 1]] {
      let mut request = field_request(&bytes, algorithm);
      request.value_store_id = &identity;
      assert!(encode_field_index_definition(request, algorithm).is_err());
    }
    for converter in [vec![], vec![0; 119], vec![0; 120], vec![0; 65_537]] {
      let mut request = field_request(&bytes, algorithm);
      request.converter_definition = &converter;
      assert!(encode_field_index_definition(request, algorithm).is_err());
    }
    for field in 0..4 {
      let maximum = if field < 2 { 65_536 } else { 8 * 1_048_576 };
      for value in [0, maximum, maximum + 1] {
        let mut request = field_request(&bytes, algorithm);
        match field {
          0 => request.max_terms_per_document = value as u32,
          1 => request.max_postings_per_document = value as u32,
          2 => request.max_canonical_posting_bytes_per_document = value,
          3 => request.max_query_recheck_value_bytes = value,
          _ => unreachable!(),
        }
        assert_eq!(encode_field_index_definition(request, algorithm).is_ok(), value == maximum, "field {field}, value {value}");
      }
    }
  }
}

#[test]
fn converter_writer_identity_covers_every_bound_and_legacy_parameter() {
  let algorithm = HashAlgorithm::Blake3_256;
  let bytes = fixture("converter-definition-v1", "acnv", "blake3-256", "u8_v0");
  let mut identities = std::collections::BTreeSet::new();
  for field in 0..6 {
    let mut request = converter_request(&bytes);
    match field {
      0 => {}
      1 => request.max_input_bytes += 1,
      2 => request.max_output_values += 1,
      3 => request.max_output_value_bytes += 1,
      4 => request.max_total_output_bytes += 1,
      5 => request.parameters = &[1, 255],
      _ => unreachable!(),
    }
    let encoded = encode_converter_definition(request, algorithm).unwrap();
    assert!(identities.insert(encoded.converter_fingerprint.clone()), "field {field}");
    assert_eq!(encoded.converter_fingerprint, independent_identity(algorithm, b"aeordb.index.converter-definition.v1\0", &encoded.value));
  }
}

#[test]
fn field_writer_identity_covers_each_limit_binding_and_embedded_converter() {
  let algorithm = HashAlgorithm::Blake3_256;
  let bytes = fixture("field-index-definition-v1", "afix", "blake3-256", "typed_exact_blake3_v1");
  let alternate = fixture("converter-definition-v1", "acnv", "blake3-256", "bool_order_v1");
  let identity = [0x77; 32];
  let mut identities = std::collections::BTreeSet::new();
  for field in 0..7 {
    let mut request = field_request(&bytes, algorithm);
    match field {
      0 => {}
      1 => request.value_store_id = &identity,
      2 => request.max_terms_per_document -= 1,
      3 => request.max_postings_per_document -= 1,
      4 => request.max_canonical_posting_bytes_per_document -= 1,
      5 => request.max_query_recheck_value_bytes -= 1,
      6 => request.converter_definition = &alternate,
      _ => unreachable!(),
    }
    let encoded = encode_field_index_definition(request, algorithm).unwrap();
    assert!(identities.insert(encoded.index_id.clone()), "field {field}");
    assert_eq!(encoded.index_id, independent_identity(algorithm, b"aeordb.index.field-definition.v1\0", &encoded.value));
  }
}
