//! Definition-object writer proof. These are canonical structural payloads, not proof of
//! source-configuration compilation, semantic activation or executor availability.
use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::namespace::{decode_semantic_definition_record, encode_semantic_definition_object};
use aeordb::engine::v4::reader::MalformedInputClass;
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

fn domain(class: u16) -> &'static [u8] {
  match class {
    1 => b"aeordb.semantic.effective-index-config-projection.v1\0",
    2 => b"aeordb.semantic.parser-registry-projection.v1\0",
    3 => b"aeordb.index.scope-definition.v1\0",
    4 => b"aeordb.index.value-store-definition.v1\0",
    5 => b"aeordb.index.field-definition.v1\0",
    6 => b"aeordb.semantic.executable-dependency-definition.v1\0",
    7 => b"aeordb.semantic.native-dependency-definition.v1\0",
    _ => panic!("unknown test class"),
  }
}

fn payload(algorithm: HashAlgorithm, class: u16) -> Vec<u8> {
  if class <= 2 {
    // CanonicalConfigValueV1 empty map: structural codec characterization only.
    return vec![0x0a, 4, 0, 0, 0, 0, 0, 0, 0];
  }
  let profile = if algorithm.hash_length() == 32 { "blake3-256" } else { "sha512" };
  let name = match class {
    3 => format!("scope-definition-v1/ascp-{profile}-root-direct-valid.bin"),
    4 => format!("value-store-definition-v1/avst-{profile}-metadata-hash-corrected-valid.bin"),
    5 => format!("field-index-definition-v1/afix-{profile}-bool_order_v1-valid.bin"),
    6 => format!("semantic-object-v1/asem-{profile}-wasm-parser-definition-valid.bin"),
    7 => format!("semantic-object-v1/asem-{profile}-native-dependency-definition-valid.bin"),
    _ => panic!("unknown test class"),
  };
  let bytes = std::fs::read(format!("{}/spec/fixtures/v4/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap();
  if class <= 5 {
    return bytes;
  }
  bytes[48 + algorithm.hash_length()..bytes.len() - 4].to_vec()
}

fn expected_object(class: u16, semantic_id: &[u8], payload: &[u8]) -> Vec<u8> {
  let width = semantic_id.len();
  let mut bytes = vec![0; 52 + width + payload.len()];
  let length = bytes.len() as u32;
  bytes[..4].copy_from_slice(b"ASEM");
  for (offset, value) in [(4, 1u16), (6, 4), (8, 32), (32, class), (34, 1)] {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
  }
  bytes[12..16].copy_from_slice(&length.to_le_bytes());
  bytes[16..20].copy_from_slice(&(length - 36).to_le_bytes());
  bytes[20..28].copy_from_slice(&1u64.to_le_bytes());
  bytes[40..40 + width].copy_from_slice(semantic_id);
  bytes[40 + width..44 + width].copy_from_slice(&(payload.len() as u32).to_le_bytes());
  bytes[48 + width..48 + width + payload.len()].copy_from_slice(payload);
  let end = bytes.len() - 4;
  let checksum = crc32fast::hash(&bytes[..end]);
  bytes[end..].copy_from_slice(&checksum.to_le_bytes());
  bytes
}

#[test]
fn definition_object_bytes_and_both_identities_match_independent_domains_for_all_classes_and_hashes() {
  for algorithm in ALGORITHMS {
    for class in 1..=7 {
      let raw = payload(algorithm, class);
      let semantic_id = digest(algorithm, &[domain(class), &raw].concat());
      let expected = expected_object(class, &semantic_id, &raw);
      let encoded = encode_semantic_definition_object(class, &raw, algorithm).unwrap();
      assert_eq!(encoded.semantic_id, semantic_id);
      assert_eq!(encoded.object.value, expected);
      let object_id = digest(algorithm, &[b"aeordb.semantic-object.immutable.v1\0".as_slice(), &4u16.to_le_bytes(), &expected].concat());
      assert_eq!(encoded.object.object_id, object_id);
      assert_ne!(encoded.object.object_id, semantic_id);
      let decoded = decode_semantic_definition_record(&encoded.object.value, algorithm).unwrap();
      assert_eq!(decoded.class, class);
      assert_eq!(decoded.semantic_id, semantic_id);
      assert_eq!(decoded.definition, raw);
    }
  }
}

#[test]
fn definition_object_writer_rejects_every_truncated_payload_and_trailing_bytes() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for class in 1..=7 {
      let mut raw = payload(algorithm, class);
      for length in 0..raw.len() {
        assert!(
          encode_semantic_definition_object(class, &raw[..length], algorithm).is_err(),
          "{algorithm:?} class {class} prefix {length}"
        );
      }
      raw.push(0);
      assert!(encode_semantic_definition_object(class, &raw, algorithm).is_err());
    }
  }
}

#[test]
fn definition_object_writer_rejects_raw_json_and_opaque_historical_projection_payloads() {
  for class in [1, 2] {
    for raw in [b"{}".as_slice(), b"{\"fields\":[]}", b"\x01\0canonical-parser-registry"] {
      assert!(encode_semantic_definition_object(class, raw, HashAlgorithm::Blake3_256).is_err());
    }
  }
}

#[test]
fn definition_object_writer_refuses_unknown_classes_and_cross_kind_dependency_records() {
  let algorithm = HashAlgorithm::Blake3_256;
  for class in [0, 8, u16::MAX] {
    assert!(encode_semantic_definition_object(class, &payload(algorithm, 1), algorithm).is_err());
  }
  assert!(encode_semantic_definition_object(6, &payload(algorithm, 7), algorithm).is_err());
  assert!(encode_semantic_definition_object(7, &payload(algorithm, 6), algorithm).is_err());
}

#[test]
fn definition_object_writer_checks_complete_size_before_payload_validation() {
  let raw = vec![0; 1_048_576];
  for algorithm in ALGORITHMS {
    for class in 1..=7 {
      assert_eq!(encode_semantic_definition_object(class, &raw, algorithm).unwrap_err().code(), "semantic_definition_exceeds_cap");
    }
  }
}

#[test]
fn definition_object_writer_preserves_smaller_class_caps_before_decoding() {
  for (class, maximum) in [(1, 262_144), (2, 262_144), (3, 65_536), (4, 524_288), (5, 262_144)] {
    let bytes = vec![0; maximum + 1];
    let error = encode_semantic_definition_object(class, &bytes, HashAlgorithm::Blake3_256).unwrap_err();
    assert_eq!(error.class(), MalformedInputClass::AllocationAmplification, "class {class}: {error}");
  }
}

#[test]
fn definition_object_writer_checks_typed_payloads_instead_of_wrapping_opaque_bytes() {
  let algorithm = HashAlgorithm::Blake3_256;
  for class in 3..=5 {
    for wrong_class in 3..=5 {
      if wrong_class != class {
        assert!(encode_semantic_definition_object(class, &payload(algorithm, wrong_class), algorithm).is_err());
      }
    }
    let mut bytes = payload(algorithm, class);
    bytes[0] ^= 1;
    assert!(encode_semantic_definition_object(class, &bytes, algorithm).is_err());
  }
}

#[test]
fn definition_object_writer_retains_unknown_executor_profiles_without_claiming_availability() {
  for algorithm in ALGORITHMS {
    for class in [6, 7] {
      let mut bytes = payload(algorithm, class);
      bytes[14..16].copy_from_slice(&u16::MAX.to_le_bytes());
      let encoded = encode_semantic_definition_object(class, &bytes, algorithm).unwrap();
      assert_eq!(encoded.semantic_id, digest(algorithm, &[domain(class), &bytes].concat()));
      assert_eq!(decode_semantic_definition_record(&encoded.object.value, algorithm).unwrap().definition, bytes);
    }
  }
}
