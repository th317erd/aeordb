//! Independent Round 16 dependency identity and dynamic-width contract proof.
use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::dependency::decode_dependency_record_bytes;
use aeordb::engine::v4::namespace::{
  decode_semantic_catalog_node, decode_semantic_definition_record, SemanticCatalogNodeV1, SemanticCatalogRecordV1,
};
use aeordb::engine::v4::semantic_catalog::{
  SemanticCatalogObjectSourceV1, SemanticCatalogReadErrorClassV1, SemanticCatalogReadErrorV1, SemanticCatalogReaderV1,
};
use sha2::Digest;

const ALGORITHMS: [HashAlgorithm; 5] =
  [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512];

fn digest(algorithm: HashAlgorithm, bytes: &[u8]) -> Vec<u8> {
  match algorithm {
    HashAlgorithm::Blake3_256 => blake3::hash(bytes).as_bytes().to_vec(),
    HashAlgorithm::Sha256 => sha2::Sha256::digest(bytes).to_vec(),
    HashAlgorithm::Sha512 => sha2::Sha512::digest(bytes).to_vec(),
    HashAlgorithm::Sha3_256 => sha3::Sha3_256::digest(bytes).to_vec(),
    HashAlgorithm::Sha3_512 => sha3::Sha3_512::digest(bytes).to_vec(),
  }
}

fn dependency(class: u16, role: u16, profile: u16) -> Vec<u8> {
  let name = b"/org/aeordev/tests/combined";
  let version = b"1.0.0";
  let mut bytes = vec![0; 96 + name.len() + version.len()];
  let length = bytes.len() as u32;
  bytes[..4].copy_from_slice(&length.to_le_bytes());
  let native = class == 7;
  for (offset, value) in [
    (4, if native { 2u16 } else { 1 }),
    (6, role),
    (12, if native { 0 } else { role + 2 }),
    (14, profile),
    (16, if native { 2 } else { 1 }),
    (18, if native { 0 } else { 1 }),
  ] {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
  }
  bytes[8..12].copy_from_slice(&(if native { 0u32 } else { 4 }).to_le_bytes());
  bytes[20..24].copy_from_slice(&(name.len() as u32).to_le_bytes());
  bytes[24..28].copy_from_slice(&(version.len() as u32).to_le_bytes());
  bytes[32..40].copy_from_slice(&(if native { 0u64 } else { 128 }).to_le_bytes());
  bytes[40..72].fill(0x5a);
  bytes[96..96 + name.len()].copy_from_slice(name);
  bytes[96 + name.len()..].copy_from_slice(version);
  bytes
}

fn dependency_id(algorithm: HashAlgorithm, class: u16, bytes: &[u8]) -> Vec<u8> {
  let domain: &[u8] = if class == 6 {
    b"aeordb.semantic.executable-dependency-definition.v1\0"
  } else {
    b"aeordb.semantic.native-dependency-definition.v1\0"
  };
  digest(algorithm, &[domain, bytes].concat())
}

fn object(kind: u16, body: &[u8]) -> Vec<u8> {
  let mut bytes = vec![0; 36 + body.len()];
  let length = bytes.len() as u32;
  bytes[..4].copy_from_slice(b"ASEM");
  bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
  bytes[6..8].copy_from_slice(&kind.to_le_bytes());
  bytes[8..10].copy_from_slice(&32u16.to_le_bytes());
  bytes[12..16].copy_from_slice(&length.to_le_bytes());
  bytes[16..20].copy_from_slice(&(body.len() as u32).to_le_bytes());
  bytes[20..28].copy_from_slice(&1u64.to_le_bytes());
  bytes[32..32 + body.len()].copy_from_slice(body);
  let checksum_offset = bytes.len() - 4;
  let checksum = crc32fast::hash(&bytes[..checksum_offset]);
  bytes[checksum_offset..].copy_from_slice(&checksum.to_le_bytes());
  bytes
}

fn object_id(algorithm: HashAlgorithm, kind: u16, bytes: &[u8]) -> Vec<u8> {
  digest(algorithm, &[b"aeordb.semantic-object.immutable.v1\0".as_slice(), &kind.to_le_bytes(), bytes].concat())
}

fn definition(class: u16, semantic_id: &[u8], dependency: &[u8]) -> Vec<u8> {
  let width = semantic_id.len();
  let mut body = vec![0; 16 + width + dependency.len()];
  body[..2].copy_from_slice(&class.to_le_bytes());
  body[2..4].copy_from_slice(&1u16.to_le_bytes());
  body[8..8 + width].copy_from_slice(semantic_id);
  body[8 + width..12 + width].copy_from_slice(&(dependency.len() as u32).to_le_bytes());
  body[16 + width..].copy_from_slice(dependency);
  object(4, &body)
}

fn leaf(algorithm: HashAlgorithm, class: u16, semantic_id: &[u8], definition_id: &[u8], key: &[u8]) -> Vec<u8> {
  let width = algorithm.hash_length();
  let record_length = 8 + 2 * width + key.len();
  let mut body = vec![0; 16 + width + record_length];
  body[4..8].copy_from_slice(&1u32.to_le_bytes());
  let lookup = digest(algorithm, &[b"aeordb.semantic-catalog-key.v1\0".as_slice(), &class.to_le_bytes(), key].concat());
  body[8..8 + width].copy_from_slice(&lookup);
  body[8 + width..12 + width].copy_from_slice(&(record_length as u32).to_le_bytes());
  let record = 16 + width;
  body[record..record + 2].copy_from_slice(&class.to_le_bytes());
  body[record + 4..record + 8].copy_from_slice(&(key.len() as u32).to_le_bytes());
  body[record + 8..record + 8 + width].copy_from_slice(semantic_id);
  body[record + 8 + width..record + 8 + 2 * width].copy_from_slice(definition_id);
  body[record + 8 + 2 * width..].copy_from_slice(key);
  object(2, &body)
}

#[test]
fn dependency_catalog_accepts_complete_ids_for_every_registered_database_hash() {
  for algorithm in ALGORITHMS {
    for class in [6, 7] {
      let raw = dependency(class, 1, if class == 6 { 2 } else { 1 });
      let semantic_id = dependency_id(algorithm, class, &raw);
      let value = definition(class, &semantic_id, &raw);
      let decoded = decode_semantic_definition_record(&value, algorithm).unwrap();
      assert_eq!(decoded.semantic_id, semantic_id);
      assert_eq!(semantic_id.len(), algorithm.hash_length());
      let catalog = leaf(algorithm, class, &semantic_id, &decoded.object_id, &semantic_id);
      let SemanticCatalogNodeV1::Leaf(decoded) = decode_semantic_catalog_node(&catalog, algorithm).unwrap() else {
        panic!("expected leaf")
      };
      let record = decoded.records().next().unwrap().unwrap();
      assert_eq!(record.owner_key, semantic_id);
      assert_eq!(record.semantic_id, semantic_id);
      assert_eq!(raw[40..72], [0x5a; 32]);
    }
  }
}

#[test]
fn same_artifact_parser_mapper_and_runtime_bindings_have_distinct_complete_keys() {
  for algorithm in ALGORITHMS {
    let records = [dependency(6, 1, 2), dependency(6, 2, 2), dependency(6, 1, u16::MAX)];
    let ids: Vec<_> = records.iter().map(|raw| dependency_id(algorithm, 6, raw)).collect();
    for (index, raw) in records.iter().enumerate() {
      assert_eq!(raw[40..72], records[0][40..72]);
      for previous in &ids[..index] {
        assert_ne!(&ids[index], previous);
      }
      let value = definition(6, &ids[index], raw);
      let decoded = decode_semantic_definition_record(&value, algorithm).unwrap();
      assert!(decode_semantic_catalog_node(&leaf(algorithm, 6, &ids[index], &decoded.object_id, &ids[index]), algorithm).is_ok());
    }
  }
}

#[test]
fn dependency_catalog_rejects_same_width_owner_semantic_id_disagreement() {
  for algorithm in ALGORITHMS {
    for class in [6, 7] {
      let raw = dependency(class, 1, if class == 6 { 2 } else { 1 });
      let semantic_id = dependency_id(algorithm, class, &raw);
      let mut key = semantic_id.clone();
      key[0] ^= 1;
      let encoded = leaf(algorithm, class, &semantic_id, &vec![0x22; algorithm.hash_length()], &key);
      assert!(decode_semantic_catalog_node(&encoded, algorithm).is_err(), "{algorithm:?}, class {class}: mismatched owner accepted");
    }
  }
}

#[test]
fn raw_artifact_fingerprint_is_not_a_catalog_key_even_at_matching_width() {
  for algorithm in ALGORITHMS {
    for class in [6, 7] {
      let raw = dependency(class, 1, if class == 6 { 2 } else { 1 });
      let semantic_id = dependency_id(algorithm, class, &raw);
      let encoded = leaf(algorithm, class, &semantic_id, &vec![0x22; algorithm.hash_length()], &raw[40..72]);
      assert!(decode_semantic_catalog_node(&encoded, algorithm).is_err(), "{algorithm:?}, class {class}: artifact key accepted");
    }
  }
}

#[test]
fn dependency_key_width_is_exact_not_an_arbitrary_variable_blob() {
  for algorithm in ALGORITHMS {
    for class in [6, 7] {
      let width = algorithm.hash_length();
      for length in [0, 1, width - 1, width + 1, 65] {
        let encoded = leaf(algorithm, class, &vec![0x11; width], &vec![0x22; width], &vec![0x11; length]);
        assert!(decode_semantic_catalog_node(&encoded, algorithm).is_err(), "{algorithm:?}, class {class}, length {length}");
      }
    }
  }
}

#[test]
fn dependency_definition_rejects_stale_semantic_ids_and_cross_class_domains() {
  for algorithm in ALGORITHMS {
    for class in [6, 7] {
      let raw = dependency(class, 1, if class == 6 { 2 } else { 1 });
      let mut stale = dependency_id(algorithm, class, &raw);
      stale[0] ^= 1;
      assert!(
        decode_semantic_definition_record(&definition(class, &stale, &raw), algorithm).is_err(),
        "{algorithm:?}, class {class}: stale ID accepted"
      );
      let other = dependency_id(algorithm, if class == 6 { 7 } else { 6 }, &raw);
      assert!(decode_semantic_definition_record(&definition(class, &other, &raw), algorithm).is_err());
    }
  }
}

#[test]
fn correctly_hashed_malformed_or_wrong_class_dependencies_are_rejected() {
  for algorithm in ALGORITHMS {
    for class in [6, 7] {
      let original = dependency(class, 1, if class == 6 { 2 } else { 1 });
      let mut invalid =
        vec![Vec::new(), original[..95].to_vec(), dependency(if class == 6 { 7 } else { 6 }, 1, if class == 6 { 1 } else { 2 })];
      let mut reserved = original.clone();
      reserved[72] = 1;
      invalid.push(reserved);
      let mut trailing = original.clone();
      trailing.push(0);
      invalid.push(trailing);
      for raw in invalid {
        let semantic_id = dependency_id(algorithm, class, &raw);
        assert!(
          decode_semantic_definition_record(&definition(class, &semantic_id, &raw), algorithm).is_err(),
          "{algorithm:?}, class {class}: malformed payload accepted"
        );
      }
    }
  }
}

struct DefinitionSource(Vec<u8>);
impl SemanticCatalogObjectSourceV1 for DefinitionSource {
  fn load_semantic_object(&self, kind_id: u16, _object_id: &[u8]) -> Result<Option<Vec<u8>>, SemanticCatalogReadErrorV1> {
    assert_eq!(kind_id, 4);
    Ok(Some(self.0.clone()))
  }
}

#[test]
fn direct_definition_resolution_cannot_bypass_dependency_owner_identity() {
  for algorithm in ALGORITHMS {
    for class in [6, 7] {
      let raw = dependency(class, 1, if class == 6 { 2 } else { 1 });
      let semantic_id = dependency_id(algorithm, class, &raw);
      let value = definition(class, &semantic_id, &raw);
      let definition_id = object_id(algorithm, 4, &value);
      let source = DefinitionSource(value);
      let reader = SemanticCatalogReaderV1::new(algorithm, &source);
      let mut owner = semantic_id.clone();
      owner[0] ^= 1;
      let record =
        SemanticCatalogRecordV1 { record_kind: class, semantic_id: &semantic_id, definition_object_id: &definition_id, owner_key: &owner };
      let visited = std::cell::Cell::new(false);
      let result = reader.with_definition(record, &|| false, |_| {
        visited.set(true);
        Ok(())
      });
      assert!(result.is_err(), "{algorithm:?}, class {class}: callback received a mismatched binding");
      assert!(!visited.get());
    }
  }
}

struct ObservedSource {
  value: Result<Option<Vec<u8>>, SemanticCatalogReadErrorV1>,
  loads: std::cell::Cell<usize>,
}

impl SemanticCatalogObjectSourceV1 for ObservedSource {
  fn load_semantic_object(&self, kind_id: u16, _object_id: &[u8]) -> Result<Option<Vec<u8>>, SemanticCatalogReadErrorV1> {
    assert_eq!(kind_id, 4);
    self.loads.set(self.loads.get() + 1);
    self.value.clone()
  }
}

#[test]
fn direct_resolution_accepts_complete_dynamic_width_bindings_and_returns_callback_results() {
  for algorithm in ALGORITHMS {
    for class in [6, 7] {
      let raw = dependency(class, 1, if class == 6 { 2 } else { 1 });
      let semantic_id = dependency_id(algorithm, class, &raw);
      let value = definition(class, &semantic_id, &raw);
      let definition_id = object_id(algorithm, 4, &value);
      let source = DefinitionSource(value);
      let reader = SemanticCatalogReaderV1::new(algorithm, &source);
      let record = SemanticCatalogRecordV1 {
        record_kind: class,
        semantic_id: &semantic_id,
        definition_object_id: &definition_id,
        owner_key: &semantic_id,
      };
      assert_eq!(reader.with_definition(record, &|| false, |bytes| Ok(bytes.to_vec())).unwrap(), raw);
      let callback_error = SemanticCatalogReadErrorV1::resource("test_callback_resource", "caller allocation refused");
      let result: Result<(), _> = reader.with_definition(record, &|| false, |_| Err(callback_error.clone()));
      assert_eq!(result.unwrap_err(), callback_error);
    }
  }
}

#[test]
fn cancellation_before_or_during_definition_load_prevents_callback_execution() {
  for algorithm in ALGORITHMS {
    for class in [6, 7] {
      for cancel_before in [true, false] {
        let raw = dependency(class, 1, if class == 6 { 2 } else { 1 });
        let semantic_id = dependency_id(algorithm, class, &raw);
        let value = definition(class, &semantic_id, &raw);
        let definition_id = object_id(algorithm, 4, &value);
        let source = ObservedSource { value: Ok(Some(value)), loads: std::cell::Cell::new(0) };
        let reader = SemanticCatalogReaderV1::new(algorithm, &source);
        let record = SemanticCatalogRecordV1 {
          record_kind: class,
          semantic_id: &semantic_id,
          definition_object_id: &definition_id,
          owner_key: &semantic_id,
        };
        let visited = std::cell::Cell::new(false);
        let result = reader.with_definition(record, &|| cancel_before || source.loads.get() > 0, |_| {
          visited.set(true);
          Ok(())
        });
        let error = result.expect_err("a cancelled definition load must not reach its callback");
        assert_eq!(error.class(), SemanticCatalogReadErrorClassV1::Cancelled);
        assert_eq!(error.code(), "semantic_cancelled");
        assert_eq!(source.loads.get(), usize::from(!cancel_before));
        assert!(!visited.get());
      }
    }
  }
}

#[test]
fn direct_resolution_preserves_missing_source_errors_and_rejects_substituted_bindings() {
  for algorithm in ALGORITHMS {
    for class in [6, 7] {
      let raw = dependency(class, 1, if class == 6 { 2 } else { 1 });
      let semantic_id = dependency_id(algorithm, class, &raw);
      let value = definition(class, &semantic_id, &raw);
      let definition_id = object_id(algorithm, 4, &value);
      let record = SemanticCatalogRecordV1 {
        record_kind: class,
        semantic_id: &semantic_id,
        definition_object_id: &definition_id,
        owner_key: &semantic_id,
      };
      for source_result in [
        Ok(None),
        Err(SemanticCatalogReadErrorV1::unavailable("test_source_unavailable", "source is offline")),
        Err(SemanticCatalogReadErrorV1::resource("test_source_resource", "source allocation refused")),
      ] {
        let expected = source_result.clone().err();
        let source = ObservedSource { value: source_result, loads: std::cell::Cell::new(0) };
        let reader = SemanticCatalogReaderV1::new(algorithm, &source);
        let result: Result<(), _> = reader.with_definition(record, &|| false, |_| panic!("failed load reached callback"));
        let error = result.unwrap_err();
        if let Some(expected) = expected {
          assert_eq!(error, expected);
        } else {
          assert_eq!(error.class(), SemanticCatalogReadErrorClassV1::Corrupt);
          assert_eq!(error.code(), "semantic_definition_missing");
        }
      }
      let source = DefinitionSource(value);
      let reader = SemanticCatalogReaderV1::new(algorithm, &source);
      let wrong_id = vec![0x77; algorithm.hash_length()];
      for substituted in [
        SemanticCatalogRecordV1 { record_kind: if class == 6 { 7 } else { 6 }, ..record },
        SemanticCatalogRecordV1 { semantic_id: &wrong_id, ..record },
        SemanticCatalogRecordV1 { definition_object_id: &wrong_id, ..record },
      ] {
        let result: Result<(), _> = reader.with_definition(substituted, &|| false, |_| panic!("substituted binding reached callback"));
        let error = result.unwrap_err();
        assert_eq!(error.class(), SemanticCatalogReadErrorClassV1::Corrupt);
        assert_eq!(error.code(), "semantic_definition_closure");
      }
    }
  }
}

#[test]
fn dependency_definition_rejects_all_truncations_and_malformed_record_fields() {
  for algorithm in ALGORITHMS {
    for class in [6, 7] {
      let original = dependency(class, 1, if class == 6 { 2 } else { 1 });
      let mut invalid: Vec<Vec<u8>> = (0..original.len()).map(|length| original[..length].to_vec()).collect();
      for (offset, bytes) in [
        (0, u32::MAX.to_le_bytes().to_vec()),
        (4, 3u16.to_le_bytes().to_vec()),
        (6, 0u16.to_le_bytes().to_vec()),
        (8, u32::MAX.to_le_bytes().to_vec()),
        (20, 0u32.to_le_bytes().to_vec()),
        (20, u32::MAX.to_le_bytes().to_vec()),
        (24, u32::MAX.to_le_bytes().to_vec()),
        (28, 1u32.to_le_bytes().to_vec()),
        (40, vec![0; 32]),
        (96, vec![0xff]),
        (original.len() - 1, vec![0xff]),
      ] {
        let mut bytes_mutated = original.clone();
        bytes_mutated[offset..offset + bytes.len()].copy_from_slice(&bytes);
        invalid.push(bytes_mutated);
      }
      for (case, raw) in invalid.into_iter().enumerate() {
        let semantic_id = dependency_id(algorithm, class, &raw);
        assert!(
          decode_semantic_definition_record(&definition(class, &semantic_id, &raw), algorithm).is_err(),
          "{algorithm:?}, class {class}, malformed case {case}"
        );
      }
    }
  }
}

#[test]
fn canonical_single_record_decoder_borrows_bounded_fields_and_rejects_oversize_components() {
  for class in [6, 7] {
    for (id_length, version_length, accepted) in [(4096, 256, true), (4097, 256, false), (4096, 257, false)] {
      let mut raw = dependency(class, 1, if class == 6 { 2 } else { 1 });
      raw.truncate(96);
      let name = format!("/{}", "a".repeat(id_length - 1));
      let version = format!("1.0.0+{}", "a".repeat(version_length - 6));
      raw.extend_from_slice(name.as_bytes());
      raw.extend_from_slice(version.as_bytes());
      let length = raw.len() as u32;
      raw[..4].copy_from_slice(&length.to_le_bytes());
      raw[20..24].copy_from_slice(&(id_length as u32).to_le_bytes());
      raw[24..28].copy_from_slice(&(version_length as u32).to_le_bytes());
      if accepted {
        let decoded = decode_dependency_record_bytes(&raw).unwrap();
        assert_eq!(decoded.dependency_id.as_ptr(), raw[96..].as_ptr());
        assert_eq!(decoded.version.as_ptr(), raw[96 + id_length..].as_ptr());
        assert_eq!(decoded.dependency_id, name);
        assert_eq!(decoded.version, version);
      } else {
        assert_eq!(decode_dependency_record_bytes(&raw).unwrap_err().code(), "dependency_record_component_length");
      }
      for algorithm in ALGORITHMS {
        let semantic_id = dependency_id(algorithm, class, &raw);
        assert_eq!(decode_semantic_definition_record(&definition(class, &semantic_id, &raw), algorithm).is_ok(), accepted);
      }
    }
  }
}

#[test]
fn dependency_ids_cannot_mix_same_width_hash_algorithms_or_definition_object_ids() {
  for algorithm in ALGORITHMS {
    for class in [6, 7] {
      let raw = dependency(class, 1, if class == 6 { 2 } else { 1 });
      for other in ALGORITHMS {
        if other == algorithm || other.hash_length() != algorithm.hash_length() {
          continue;
        }
        let other_id = dependency_id(other, class, &raw);
        let error = decode_semantic_definition_record(&definition(class, &other_id, &raw), algorithm).unwrap_err();
        assert_eq!(error.code(), "semantic_dependency_definition_identity");
      }
      let semantic_id = dependency_id(algorithm, class, &raw);
      let value = definition(class, &semantic_id, &raw);
      let definition_id = object_id(algorithm, 4, &value);
      assert_ne!(definition_id, semantic_id);
      assert_eq!(
        decode_semantic_catalog_node(&leaf(algorithm, class, &semantic_id, &definition_id, &definition_id), algorithm).unwrap_err().code(),
        "catalog_leaf_dependency_identity"
      );
    }
  }
}

#[test]
fn cancellation_after_definition_validation_still_prevents_inspection() {
  let algorithm = HashAlgorithm::Sha512;
  let raw = dependency(7, 1, 1);
  let semantic_id = dependency_id(algorithm, 7, &raw);
  let value = definition(7, &semantic_id, &raw);
  let definition_id = object_id(algorithm, 4, &value);
  let source = DefinitionSource(value);
  let reader = SemanticCatalogReaderV1::new(algorithm, &source);
  let record =
    SemanticCatalogRecordV1 { record_kind: 7, semantic_id: &semantic_id, definition_object_id: &definition_id, owner_key: &semantic_id };
  let polls = std::cell::Cell::new(0);
  let result: Result<(), _> = reader.with_definition(
    record,
    &|| {
      polls.set(polls.get() + 1);
      polls.get() == 3
    },
    |_| panic!("cancelled inspection reached callback"),
  );
  assert_eq!(result.unwrap_err().class(), SemanticCatalogReadErrorClassV1::Cancelled);
  assert_eq!(polls.get(), 3);
}

struct CatalogFixtureSource(std::collections::BTreeMap<(u16, Vec<u8>), Vec<u8>>);

impl SemanticCatalogObjectSourceV1 for CatalogFixtureSource {
  fn load_semantic_object(&self, kind_id: u16, object_id: &[u8]) -> Result<Option<Vec<u8>>, SemanticCatalogReadErrorV1> {
    Ok(self.0.get(&(kind_id, object_id.to_vec())).cloned())
  }
}

#[test]
fn independent_dependency_fixtures_resolve_through_catalog_traversal_and_definition_closure() {
  use aeordb::engine::v4::semantic_catalog::SemanticCatalogTraversalBoundsV1;
  let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/semantic-object-v1");
  for (algorithm, label) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let mut wasm_identities = Vec::new();
    let mut wasm_fingerprints = Vec::new();
    for name in ["wasm-parser", "wasm-mapper", "native-dependency"] {
      let definition_bytes = std::fs::read(root.join(format!("asem-{label}-{name}-definition-valid.bin"))).unwrap();
      let leaf_bytes = std::fs::read(root.join(format!("asem-{label}-{name}-binding-valid.bin"))).unwrap();
      let definition_id = object_id(algorithm, 4, &definition_bytes);
      let root_id = object_id(algorithm, 2, &leaf_bytes);
      let expected = decode_semantic_definition_record(&definition_bytes, algorithm).unwrap();
      let expected_class = expected.class;
      let expected_payload = expected.definition.to_vec();
      let raw_dependency = decode_dependency_record_bytes(&expected_payload).unwrap();
      if expected_class == 6 {
        wasm_identities.push(expected.semantic_id.to_vec());
        wasm_fingerprints.push(raw_dependency.fingerprint);
      }
      assert_eq!(expected.semantic_id.len(), algorithm.hash_length());
      let source = CatalogFixtureSource(std::collections::BTreeMap::from([
        ((4, definition_id), definition_bytes),
        ((2, root_id.clone()), leaf_bytes),
      ]));
      let reader = SemanticCatalogReaderV1::new(algorithm, &source);
      let mut inspected = 0;
      let stats = reader
        .walk_catalog(&root_id, SemanticCatalogTraversalBoundsV1::new(1, 1).unwrap(), &|| false, |record| {
          assert_eq!(record.record_kind, expected_class);
          assert_eq!(record.owner_key.len(), algorithm.hash_length());
          assert_eq!(record.owner_key, record.semantic_id);
          reader.with_definition(record, &|| false, |payload| {
            assert_eq!(payload, expected_payload);
            inspected += 1;
            Ok(())
          })
        })
        .unwrap();
      assert_eq!(inspected, 1);
      assert_eq!(stats.records, 1);
      assert_eq!(stats.nodes, 1);
      assert_eq!(stats.class_counts[expected_class as usize], 1);
    }
    assert_eq!(wasm_identities.len(), 2);
    assert_ne!(wasm_identities[0], wasm_identities[1]);
    assert_eq!(wasm_fingerprints[0], wasm_fingerprints[1]);
  }
}
