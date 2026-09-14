//! Canonical definition writer regressions against independent frozen bytes.
use aeordb::engine::v4::reader::MalformedInputClass;
use aeordb::engine::v4::value_store::{
  decode_value_store_definition, encode_value_store_definition, ValueStoreDefinitionWriteV1, ValueStoreSemanticFamily,
};
use aeordb::engine::HashAlgorithm;
use sha2::Digest;

fn fixture(profile: &str, suffix: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/value-store-definition-v1/avst-{profile}-{suffix}-valid.bin", env!("CARGO_MANIFEST_DIR")))
    .unwrap()
}

fn independent_request(bytes: &[u8], algorithm: HashAlgorithm) -> ValueStoreDefinitionWriteV1<'_> {
  let read_u16 = |offset| u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap());
  let read_u32 = |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
  let read_u64 = |offset| u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
  let fixed = 32 + algorithm.hash_length();
  let field_start = fixed + 80;
  let field_end = field_start + read_u32(fixed) as usize;
  let selector_end = field_end + read_u32(fixed + 4) as usize;
  let parser_end = selector_end + read_u32(fixed + 8) as usize;
  assert_eq!(parser_end + read_u32(fixed + 12) as usize, bytes.len());
  let semantic_family = match read_u16(fixed + 16) {
    1 => ValueStoreSemanticFamily::CorrectedV1,
    2 => ValueStoreSemanticFamily::MigrationV0,
    other => panic!("unknown fixture family {other}"),
  };
  ValueStoreDefinitionWriteV1 {
    scope_id: &bytes[32..fixed],
    field_name: std::str::from_utf8(&bytes[field_start..field_end]).unwrap(),
    semantic_family,
    max_source_values_per_document: read_u32(fixed + 36),
    max_canonical_source_bytes_per_document: read_u64(fixed + 48),
    max_document_input_bytes: read_u64(fixed + 56),
    max_selector_work_items_per_document: read_u64(fixed + 64),
    max_selector_examined_bytes_per_document: read_u64(fixed + 72),
    selector: &bytes[field_end..selector_end],
    parser_plan: &bytes[selector_end..parser_end],
    dependencies: &bytes[parser_end..],
  }
}

fn independent_identity(algorithm: HashAlgorithm, bytes: &[u8]) -> Vec<u8> {
  let mut input = b"aeordb.index.value-store-definition.v1\0".to_vec();
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
fn value_store_writer_matches_independent_metadata_json_and_mapper_fixtures() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    for suffix in [
      "metadata-hash-corrected",
      "metadata-created-at-legacy",
      "json-corrected",
      "json-legacy",
      "mapper-corrected",
      "mapper-legacy",
      "always-missing-none",
    ] {
      let expected = fixture(profile, suffix);
      let encoded = encode_value_store_definition(independent_request(&expected, algorithm), algorithm).unwrap();
      assert_eq!(encoded.value, expected, "{profile}/{suffix}");
      assert_eq!(encoded.value_store_id, independent_identity(algorithm, &expected));
      assert_eq!(decode_value_store_definition(&encoded.value, algorithm).unwrap().value_store_id, encoded.value_store_id);
    }
  }
}

#[test]
fn value_store_writer_uses_each_database_hash_for_identity_with_exact_scope_width() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let expected = fixture(if algorithm.hash_length() == 64 { "sha512" } else { "blake3-256" }, "json-corrected");
    let encoded = encode_value_store_definition(independent_request(&expected, algorithm), algorithm).unwrap();
    assert_eq!(encoded.value, expected);
    assert_eq!(encoded.value_store_id, independent_identity(algorithm, &expected));
    for scope in [vec![], vec![1; algorithm.hash_length() - 1], vec![1; algorithm.hash_length() + 1], vec![0; algorithm.hash_length()]] {
      let mut request = independent_request(&expected, algorithm);
      request.scope_id = &scope;
      assert!(encode_value_store_definition(request, algorithm).is_err());
    }
  }
}

#[test]
fn value_store_writer_rejects_invalid_or_oversized_names_and_children() {
  let algorithm = HashAlgorithm::Blake3_256;
  let bytes = fixture("blake3-256", "json-corrected");
  for name in ["".to_string(), "a\0b".to_string(), "@unknown".to_string(), "a".repeat(4097)] {
    let mut request = independent_request(&bytes, algorithm);
    request.field_name = &name;
    assert!(encode_value_store_definition(request, algorithm).is_err());
  }
  for (field, maximum) in [(0, 64 * 1024), (1, 128 * 1024), (2, 256 * 1024)] {
    for length in [0, maximum + 1] {
      let child = vec![0u8; length];
      let mut request = independent_request(&bytes, algorithm);
      match field {
        0 => request.selector = &child,
        1 => request.parser_plan = &child,
        2 => request.dependencies = &child,
        _ => unreachable!(),
      }
      let error = encode_value_store_definition(request, algorithm).unwrap_err();
      if length > maximum {
        assert_eq!(error.class(), MalformedInputClass::AllocationAmplification);
      }
    }
  }
}

#[test]
fn value_store_writer_rejects_zero_and_corrected_unlimited_semantic_limits() {
  let algorithm = HashAlgorithm::Blake3_256;
  let bytes = fixture("blake3-256", "json-corrected");
  for field in 0..5 {
    for limit in [0, u64::MAX] {
      let mut request = independent_request(&bytes, algorithm);
      match field {
        0 => request.max_source_values_per_document = limit as u32,
        1 => request.max_canonical_source_bytes_per_document = limit,
        2 => request.max_document_input_bytes = limit,
        3 => request.max_selector_work_items_per_document = limit,
        4 => request.max_selector_examined_bytes_per_document = limit,
        _ => unreachable!(),
      }
      assert!(encode_value_store_definition(request, algorithm).is_err(), "field {field}, limit {limit}");
    }
  }
}

#[test]
fn value_store_writer_fingerprints_every_meaningful_fixed_input_change() {
  let algorithm = HashAlgorithm::Blake3_256;
  let bytes = fixture("blake3-256", "json-corrected");
  let mut identities = std::collections::BTreeSet::new();
  let scope = vec![0x77; algorithm.hash_length()];
  for field in 0..8 {
    let mut request = independent_request(&bytes, algorithm);
    match field {
      0 => {}
      1 => request.scope_id = &scope,
      2 => request.field_name = "different-field",
      3 => request.max_source_values_per_document += 1,
      4 => request.max_canonical_source_bytes_per_document += 1,
      5 => request.max_document_input_bytes += 1,
      6 => request.max_selector_work_items_per_document += 1,
      7 => request.max_selector_examined_bytes_per_document += 1,
      _ => unreachable!(),
    }
    let encoded = encode_value_store_definition(request, algorithm).unwrap();
    assert!(identities.insert(encoded.value_store_id.clone()), "field {field}");
    assert_eq!(encoded.value_store_id, independent_identity(algorithm, &encoded.value));
  }
}

#[test]
fn value_store_writer_fingerprints_each_complete_child_not_just_its_selector_kind() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let bytes = fixture(profile, "json-corrected");
    let original = independent_request(&bytes, algorithm);
    let mut selector = original.selector.to_vec();
    assert_eq!(selector[32], 1);
    selector[40] = b'M'; // Case-sensitive object key, same selector kind and size.
    let mut parser = original.parser_plan.to_vec();
    let match_length = u32::from_le_bytes(parser[64..68].try_into().unwrap()) as usize;
    let policy = 48 + 32 + match_length;
    assert_eq!(&parser[policy..policy + 4], b"AIVP");
    let fuel = u64::from_le_bytes(parser[policy + 48..policy + 56].try_into().unwrap());
    parser[policy + 48..policy + 56].copy_from_slice(&(fuel + 1).to_le_bytes());
    let mut dependencies = original.dependencies.to_vec();
    dependencies[32 + 40] ^= 1; // Different executable identity, identical role and ABI.
    let mut identities = std::collections::BTreeSet::new();
    for child in 0..4 {
      let mut request = independent_request(&bytes, algorithm);
      match child {
        0 => {}
        1 => request.selector = &selector,
        2 => request.parser_plan = &parser,
        3 => request.dependencies = &dependencies,
        _ => unreachable!(),
      }
      let encoded = encode_value_store_definition(request, algorithm).unwrap();
      assert_eq!(encoded.value_store_id, independent_identity(algorithm, &encoded.value));
      assert!(identities.insert(encoded.value_store_id), "{profile}: child {child}");
    }
  }
}

#[test]
fn value_store_writer_rejects_nested_framing_family_and_dependency_closure_errors() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let bytes = fixture(profile, "json-corrected");
    let original = independent_request(&bytes, algorithm);
    for child in 0..3 {
      let source = match child {
        0 => original.selector,
        1 => original.parser_plan,
        2 => original.dependencies,
        _ => unreachable!(),
      };
      for malformed in [source[..source.len() - 1].to_vec(), {
        let mut reserved = source.to_vec();
        reserved[24] ^= 1;
        reserved
      }] {
        let mut request = independent_request(&bytes, algorithm);
        match child {
          0 => request.selector = &malformed,
          1 => request.parser_plan = &malformed,
          2 => request.dependencies = &malformed,
          _ => unreachable!(),
        }
        assert!(encode_value_store_definition(request, algorithm).is_err(), "{profile}: child {child}");
      }
    }
    let mut wrong_family = independent_request(&bytes, algorithm);
    wrong_family.semantic_family = ValueStoreSemanticFamily::MigrationV0;
    assert!(encode_value_store_definition(wrong_family, algorithm).is_err());
    let metadata = fixture(profile, "metadata-hash-corrected");
    let empty = independent_request(&metadata, algorithm);
    for child in 0..2 {
      let mut request = independent_request(&bytes, algorithm);
      if child == 0 {
        request.parser_plan = empty.parser_plan;
      } else {
        request.dependencies = empty.dependencies;
      }
      assert!(encode_value_store_definition(request, algorithm).is_err());
    }
  }
}

#[test]
fn value_store_writer_enforces_parser_free_migration_only_always_missing() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let canonical = fixture(profile, "always-missing-none");
    assert_eq!(encode_value_store_definition(independent_request(&canonical, algorithm), algorithm).unwrap().value, canonical);
    let obsolete = fixture(profile, "always-missing-legacy");
    assert!(encode_value_store_definition(independent_request(&obsolete, algorithm), algorithm).is_err());
    let parsed = fixture(profile, "json-legacy");
    let pipeline = independent_request(&parsed, algorithm);
    for mutation in 0..6 {
      let mut request = independent_request(&canonical, algorithm);
      match mutation {
        0 => request.semantic_family = ValueStoreSemanticFamily::CorrectedV1,
        1 => request.max_document_input_bytes = 1,
        2 => request.max_selector_work_items_per_document = 1,
        3 => request.max_selector_examined_bytes_per_document = 1,
        4 => request.dependencies = pipeline.dependencies,
        5 => request.parser_plan = pipeline.parser_plan,
        _ => unreachable!(),
      }
      assert!(encode_value_store_definition(request, algorithm).is_err(), "{profile}: mutation {mutation}");
    }
  }
}
