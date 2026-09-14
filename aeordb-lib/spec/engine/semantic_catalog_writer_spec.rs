//! Round 10/16 catalog encoders: independent wire bytes, all registered hashes,
//! malformed requests and pre-allocation count/byte limits. Tree closure and
//! canonical COW shape are separate integration obligations.
use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::namespace::{
  SemanticCatalogChildV1, SemanticCatalogRecordV1, decode_semantic_catalog_node, encode_semantic_catalog_internal,
  encode_semantic_catalog_leaf,
};
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

fn envelope(kind: u16, count: u64, body: &[u8]) -> Vec<u8> {
  let mut value = vec![0; body.len() + 36];
  let total = value.len() as u32;
  value[..4].copy_from_slice(b"ASEM");
  for (offset, field) in [(4, 1u16), (6, kind), (8, 32)] {
    value[offset..offset + 2].copy_from_slice(&field.to_le_bytes());
  }
  value[12..16].copy_from_slice(&total.to_le_bytes());
  value[16..20].copy_from_slice(&(body.len() as u32).to_le_bytes());
  value[20..28].copy_from_slice(&count.to_le_bytes());
  value[32..32 + body.len()].copy_from_slice(body);
  let end = value.len() - 4;
  let checksum = crc32fast::hash(&value[..end]);
  value[end..].copy_from_slice(&checksum.to_le_bytes());
  value
}

fn leaf_oracle(algorithm: HashAlgorithm, record: SemanticCatalogRecordV1<'_>) -> Vec<u8> {
  let width = algorithm.hash_length();
  let record_length = 8 + 2 * width + record.owner_key.len();
  let lookup =
    digest(algorithm, &[b"aeordb.semantic-catalog-key.v1\0".as_slice(), &record.record_kind.to_le_bytes(), record.owner_key].concat());
  let mut body = vec![0; 16 + width + record_length];
  body[4..8].copy_from_slice(&1u32.to_le_bytes());
  body[8..8 + width].copy_from_slice(&lookup);
  body[8 + width..12 + width].copy_from_slice(&(record_length as u32).to_le_bytes());
  let offset = 16 + width;
  body[offset..offset + 2].copy_from_slice(&record.record_kind.to_le_bytes());
  body[offset + 4..offset + 8].copy_from_slice(&(record.owner_key.len() as u32).to_le_bytes());
  body[offset + 8..offset + 8 + width].copy_from_slice(record.semantic_id);
  body[offset + 8 + width..offset + 8 + 2 * width].copy_from_slice(record.definition_object_id);
  body[offset + 8 + 2 * width..].copy_from_slice(record.owner_key);
  envelope(2, 1, &body)
}

fn internal_oracle(depth: u16, prefix: &[u8], children: &[SemanticCatalogChildV1<'_>]) -> Vec<u8> {
  let mut body = vec![0; 20];
  body[4..6].copy_from_slice(&depth.to_le_bytes());
  body[6..8].copy_from_slice(&(prefix.len() as u16).to_le_bytes());
  body[8..10].copy_from_slice(&(children.len() as u16).to_le_bytes());
  body[12..20].copy_from_slice(&children.iter().map(|child| child.record_count).sum::<u64>().to_le_bytes());
  body.extend_from_slice(prefix);
  for child in children {
    body.extend_from_slice(&[child.edge, 0, 0, 0]);
    body.extend_from_slice(&child.record_count.to_le_bytes());
    body.extend_from_slice(child.object_id);
  }
  envelope(3, children.len() as u64, &body)
}

fn assert_encoded(algorithm: HashAlgorithm, kind: u16, expected: &[u8], value: &[u8], identity: &[u8]) {
  assert_eq!(value, expected);
  let expected_id = digest(algorithm, &[b"aeordb.semantic-object.immutable.v1\0".as_slice(), &kind.to_le_bytes(), expected].concat());
  assert_eq!(identity, expected_id);
  assert_eq!(identity.len(), algorithm.hash_length());
  assert_eq!(decode_semantic_catalog_node(value, algorithm).unwrap().object_id(), identity);
}

#[test]
fn leaf_bytes_and_ids_match_independent_layout_for_all_classes_and_algorithms() {
  for algorithm in ALGORITHMS {
    let identity = vec![0x41; algorithm.hash_length()];
    let definition = vec![0x62; algorithm.hash_length()];
    for class in 1..=7 {
      let owner = if class <= 2 { "\x02\x00/config/日本語 x\\y".as_bytes().to_vec() } else { identity.clone() };
      let record =
        SemanticCatalogRecordV1 { record_kind: class, semantic_id: &identity, definition_object_id: &definition, owner_key: &owner };
      let encoded = encode_semantic_catalog_leaf(&[record], algorithm).unwrap();
      assert_encoded(algorithm, 2, &leaf_oracle(algorithm, record), &encoded.value, &encoded.object_id);
    }
  }
}

#[test]
fn catalog_writers_preserve_frozen_leaf_and_internal_fixture_bytes_at_both_widths() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let width = algorithm.hash_length();
    let root = format!("{}/spec/fixtures/v4/semantic-object-v1", env!("CARGO_MANIFEST_DIR"));
    let leaf = std::fs::read(format!("{root}/asem-{profile}-catalog-leaf-valid.bin")).unwrap();
    let offset = 48 + width;
    let record = SemanticCatalogRecordV1 {
      record_kind: 2,
      semantic_id: &leaf[offset + 8..offset + 8 + width],
      definition_object_id: &leaf[offset + 8 + width..offset + 8 + 2 * width],
      owner_key: b"\x02\x00/.aeordb-config/parsers.json",
    };
    let encoded = encode_semantic_catalog_leaf(&[record], algorithm).unwrap();
    assert_encoded(algorithm, 2, &leaf, &encoded.value, &encoded.object_id);
    let internal = std::fs::read(format!("{root}/asem-{profile}-catalog-internal-valid.bin")).unwrap();
    let children: Vec<_> = internal[52..internal.len() - 4]
      .chunks_exact(12 + width)
      .map(|bytes| SemanticCatalogChildV1 { edge: bytes[0], record_count: 1, object_id: &bytes[12..] })
      .collect();
    assert_eq!(children.len(), 2);
    let encoded = encode_semantic_catalog_internal(0, &[], &children, algorithm).unwrap();
    assert_encoded(algorithm, 3, &internal, &encoded.value, &encoded.object_id);
  }
}

#[test]
fn leaf_rejects_empty_and_excessive_record_counts_before_reading_records() {
  let invalid = SemanticCatalogRecordV1 { record_kind: 0, semantic_id: &[], definition_object_id: &[], owner_key: &[] };
  for algorithm in ALGORITHMS {
    for records in [Vec::new(), vec![invalid; 4097]] {
      let error = encode_semantic_catalog_leaf(&records, algorithm).unwrap_err();
      assert_eq!(error.code(), "catalog_leaf_count");
      assert_eq!(error.class(), MalformedInputClass::AllocationAmplification);
    }
  }
}

#[test]
fn leaf_rejects_oversized_complete_bytes_before_hashing_or_allocating_output() {
  let owner = vec![0; 65_537];
  let records = vec![SemanticCatalogRecordV1 { record_kind: 1, semantic_id: &[], definition_object_id: &[], owner_key: &owner }; 17];
  for algorithm in ALGORITHMS {
    let error = encode_semantic_catalog_leaf(&records, algorithm).unwrap_err();
    assert_eq!(error.code(), "catalog_leaf_exceeds_cap");
    assert_eq!(error.class(), MalformedInputClass::AllocationAmplification);
  }
}

#[test]
fn leaf_rejects_unknown_classes_and_invalid_identity_widths_or_zeroes() {
  for algorithm in ALGORITHMS {
    let width = algorithm.hash_length();
    let identity = vec![0x41; width];
    let record = SemanticCatalogRecordV1 { record_kind: 3, semantic_id: &identity, definition_object_id: &identity, owner_key: &identity };
    for class in [0, 8, u16::MAX] {
      assert!(encode_semantic_catalog_leaf(&[SemanticCatalogRecordV1 { record_kind: class, ..record }], algorithm).is_err());
    }
    for bytes in [Vec::new(), vec![0; width], vec![1; width - 1], vec![1; width + 1]] {
      assert!(encode_semantic_catalog_leaf(&[SemanticCatalogRecordV1 { semantic_id: &bytes, ..record }], algorithm).is_err());
      assert!(encode_semantic_catalog_leaf(&[SemanticCatalogRecordV1 { definition_object_id: &bytes, ..record }], algorithm).is_err());
    }
  }
}

#[test]
fn leaf_validates_every_owner_class_and_dependency_identity() {
  for algorithm in ALGORITHMS {
    let width = algorithm.hash_length();
    let identity = vec![0x41; width];
    for class in 1..=7 {
      for owner in [
        Vec::new(),
        vec![1],
        vec![1; width + 1],
        b"\0\0/".to_vec(),
        b"\x02\0relative".to_vec(),
        b"\x02\0/a/../b".to_vec(),
        b"\x02\0/\xff".to_vec(),
      ] {
        let record =
          SemanticCatalogRecordV1 { record_kind: class, semantic_id: &identity, definition_object_id: &identity, owner_key: &owner };
        assert!(encode_semantic_catalog_leaf(&[record], algorithm).is_err(), "{algorithm:?} class {class}, {owner:?}");
      }
    }
    for class in [6, 7] {
      let owner = vec![0x42; width];
      let record =
        SemanticCatalogRecordV1 { record_kind: class, semantic_id: &identity, definition_object_id: &identity, owner_key: &owner };
      assert_eq!(encode_semantic_catalog_leaf(&[record], algorithm).unwrap_err().code(), "catalog_leaf_dependency_identity");
    }
  }
}

#[test]
fn leaf_control_owner_byte_limit_and_root_path_are_preserved() {
  for algorithm in ALGORITHMS {
    let identity = vec![0x41; algorithm.hash_length()];
    let mut maximum = vec![b'a'; 65_537];
    maximum[..3].copy_from_slice(b"\x02\0/");
    for owner in [b"\x02\0/".as_slice(), maximum.as_slice()] {
      let record = SemanticCatalogRecordV1 { record_kind: 1, semantic_id: &identity, definition_object_id: &identity, owner_key: owner };
      assert!(encode_semantic_catalog_leaf(&[record], algorithm).is_ok());
    }
    maximum.push(b'a');
    let record = SemanticCatalogRecordV1 { record_kind: 1, semantic_id: &identity, definition_object_id: &identity, owner_key: &maximum };
    assert!(encode_semantic_catalog_leaf(&[record], algorithm).is_err());
  }
}

#[test]
fn leaf_rejects_duplicate_keys_and_distinct_lookup_digests() {
  for algorithm in ALGORITHMS {
    let identity = vec![0x41; algorithm.hash_length()];
    let other = vec![0x42; algorithm.hash_length()];
    let record = SemanticCatalogRecordV1 { record_kind: 3, semantic_id: &identity, definition_object_id: &identity, owner_key: &identity };
    assert_eq!(encode_semantic_catalog_leaf(&[record, record], algorithm).unwrap_err().code(), "catalog_leaf_order");
    let second = SemanticCatalogRecordV1 { owner_key: &other, ..record };
    assert_eq!(encode_semantic_catalog_leaf(&[record, second], algorithm).unwrap_err().code(), "catalog_leaf_lookup_digest");
  }
}

#[test]
fn internal_bytes_counts_and_identity_match_independent_layout_for_every_algorithm() {
  for algorithm in ALGORITHMS {
    let identity = vec![0x41; algorithm.hash_length()];
    let other = vec![0x42; algorithm.hash_length()];
    let children = [
      SemanticCatalogChildV1 { edge: 0, record_count: 3, object_id: &identity },
      SemanticCatalogChildV1 { edge: 255, record_count: 7, object_id: &other },
    ];
    let encoded = encode_semantic_catalog_internal(1, &[2, 3, 4], &children, algorithm).unwrap();
    assert_encoded(algorithm, 3, &internal_oracle(1, &[2, 3, 4], &children), &encoded.value, &encoded.object_id);
  }
}

#[test]
fn internal_accepts_full_fanout_and_last_digest_byte_depth() {
  for algorithm in ALGORITHMS {
    let identity = vec![0x41; algorithm.hash_length()];
    let children: Vec<_> = (0..=255).map(|edge| SemanticCatalogChildV1 { edge, record_count: 1, object_id: &identity }).collect();
    let depth = algorithm.hash_length() as u16 - 1;
    let encoded = encode_semantic_catalog_internal(depth, &[], &children, algorithm).unwrap();
    assert_encoded(algorithm, 3, &internal_oracle(depth, &[], &children), &encoded.value, &encoded.object_id);
    assert!(encoded.value.len() < 65_536);
  }
}

#[test]
fn internal_rejects_invalid_child_count_before_accessing_children() {
  let invalid = SemanticCatalogChildV1 { edge: 0, record_count: 0, object_id: &[] };
  for algorithm in ALGORITHMS {
    for length in [0, 1, 257] {
      assert_eq!(
        encode_semantic_catalog_internal(0, &[], &vec![invalid; length], algorithm).unwrap_err().code(),
        "catalog_internal_metadata"
      );
    }
  }
}

#[test]
fn internal_rejects_unreachable_prefix_depth_and_oversized_prefix_before_copy() {
  for algorithm in ALGORITHMS {
    let identity = vec![0x41; algorithm.hash_length()];
    let children = [
      SemanticCatalogChildV1 { edge: 0, record_count: 1, object_id: &identity },
      SemanticCatalogChildV1 { edge: 1, record_count: 1, object_id: &identity },
    ];
    for (depth, length) in
      [(algorithm.hash_length() as u16, 0), (0, algorithm.hash_length()), (1, algorithm.hash_length() - 1), (u16::MAX, 65_536)]
    {
      assert_eq!(
        encode_semantic_catalog_internal(depth, &vec![1; length], &children, algorithm).unwrap_err().code(),
        "catalog_internal_metadata"
      );
    }
  }
}

#[test]
fn internal_rejects_duplicate_or_unordered_edges() {
  for algorithm in ALGORITHMS {
    let identity = vec![0x41; algorithm.hash_length()];
    for edges in [[1, 1], [1, 0]] {
      let children = edges.map(|edge| SemanticCatalogChildV1 { edge, record_count: 1, object_id: &identity });
      assert_eq!(encode_semantic_catalog_internal(0, &[], &children, algorithm).unwrap_err().code(), "catalog_internal_child");
    }
  }
}

#[test]
fn internal_rejects_invalid_child_hashes_and_zero_counts() {
  for algorithm in ALGORITHMS {
    let width = algorithm.hash_length();
    let identity = vec![0x41; width];
    let first = SemanticCatalogChildV1 { edge: 0, record_count: 1, object_id: &identity };
    for bytes in [Vec::new(), vec![0; width], vec![1; width - 1], vec![1; width + 1]] {
      let second = SemanticCatalogChildV1 { edge: 1, record_count: 1, object_id: &bytes };
      assert!(encode_semantic_catalog_internal(0, &[], &[first, second], algorithm).is_err());
    }
    let second = SemanticCatalogChildV1 { edge: 1, record_count: 0, object_id: &identity };
    assert_eq!(encode_semantic_catalog_internal(0, &[], &[first, second], algorithm).unwrap_err().code(), "catalog_internal_zero_count");
  }
}

#[test]
fn internal_checked_count_sum_accepts_u64_maximum_and_rejects_overflow() {
  for algorithm in ALGORITHMS {
    let identity = vec![0x41; algorithm.hash_length()];
    let first = SemanticCatalogChildV1 { edge: 0, record_count: u64::MAX - 1, object_id: &identity };
    let second = SemanticCatalogChildV1 { edge: 1, record_count: 1, object_id: &identity };
    let encoded = encode_semantic_catalog_internal(0, &[], &[first, second], algorithm).unwrap();
    assert_encoded(algorithm, 3, &internal_oracle(0, &[], &[first, second]), &encoded.value, &encoded.object_id);
    let overflow = SemanticCatalogChildV1 { record_count: 2, ..second };
    assert!(encode_semantic_catalog_internal(0, &[], &[first, overflow], algorithm).is_err());
  }
}
