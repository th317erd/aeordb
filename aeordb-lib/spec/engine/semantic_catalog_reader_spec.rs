use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::namespace::{SemanticCatalogNodeV1, decode_semantic_catalog_node, decode_semantic_definition_record};

fn fixture(name: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/semantic-object-v1/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

#[test]
fn definition_reader_exposes_exact_frozen_class_identity_and_payload_at_both_hash_widths() {
  for (algorithm, profile, expected_id) in [
    (HashAlgorithm::Blake3_256, "blake3-256", "56c3037d9064e1b4de44bc118ee660a1f58da4181a9c3a8018d98df0cbb178a2"),
    (
      HashAlgorithm::Sha512,
      "sha512",
      "93642ec072974f4e206a04b4ee92d12d12861c41f47cde6804fc06e6c4aaa2af1e99b60faa4f0f6b35dd71c08cc33e230c48b7e55390fa676e2885765429f6cb",
    ),
  ] {
    let bytes = fixture(&format!("asem-{profile}-definition-valid.bin"));
    let definition = decode_semantic_definition_record(&bytes, algorithm).unwrap();

    assert_eq!(definition.class, 2);
    assert_eq!(hex::encode(definition.semantic_id), expected_id);
    assert_eq!(definition.definition, b"\x01\x00canonical-parser-registry");
  }
}

#[test]
fn catalog_leaf_reader_exposes_each_exact_binding_without_materializing_a_record_vector() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let bytes = fixture(&format!("asem-{profile}-catalog-leaf-valid.bin"));
    let definition_bytes = fixture(&format!("asem-{profile}-definition-valid.bin"));
    let definition = decode_semantic_definition_record(&definition_bytes, algorithm).unwrap();
    let SemanticCatalogNodeV1::Leaf(leaf) = decode_semantic_catalog_node(&bytes, algorithm).unwrap() else {
      panic!("catalog leaf fixture decoded as an internal node");
    };

    assert_eq!(leaf.record_count(), 1);
    let records = leaf.records().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].record_kind, 2);
    assert_eq!(records[0].semantic_id, definition.semantic_id);
    assert_eq!(records[0].definition_object_id, definition.object_id);
    assert_eq!(records[0].owner_key, b"\x02\x00/.aeordb-config/parsers.json");
  }
}

#[test]
fn catalog_internal_reader_exposes_radix_position_counts_and_ordered_children() {
  let bytes = fixture("asem-sha512-catalog-internal-valid.bin");
  let SemanticCatalogNodeV1::Internal(node) = decode_semantic_catalog_node(&bytes, HashAlgorithm::Sha512).unwrap() else {
    panic!("catalog internal fixture decoded as a leaf");
  };

  assert_eq!(node.depth(), 0);
  assert_eq!(node.prefix(), b"");
  assert_eq!(node.child_count(), 2);
  assert_eq!(node.subtree_record_count(), 2);
  let children = node.children().collect::<Result<Vec<_>, _>>().unwrap();
  assert_eq!(children.len(), 2);
  assert_eq!(children[0].edge, 0x83);
  assert_eq!(children[0].record_count, 1);
  assert_eq!(children[1].edge, 0xe1);
  assert_eq!(children[1].record_count, 1);
  assert!(children[0].object_id < children[1].object_id || children[0].edge < children[1].edge);
}

#[test]
fn typed_readers_reject_the_wrong_semantic_object_kind() {
  let leaf = fixture("asem-blake3-256-catalog-leaf-valid.bin");
  let definition = fixture("asem-blake3-256-definition-valid.bin");
  let state = fixture("asem-blake3-256-state-complete.bin");

  assert!(decode_semantic_definition_record(&leaf, HashAlgorithm::Blake3_256).is_err());
  assert!(decode_semantic_catalog_node(&definition, HashAlgorithm::Blake3_256).is_err());
  assert!(decode_semantic_catalog_node(&state, HashAlgorithm::Blake3_256).is_err());
}

#[test]
fn typed_readers_reject_corrupt_and_truncated_semantic_objects_before_exposing_views() {
  let mut leaf = fixture("asem-blake3-256-catalog-leaf-valid.bin");
  let final_byte = leaf.len() - 1;
  leaf[final_byte] ^= 0x80;
  let corrupt = decode_semantic_catalog_node(&leaf, HashAlgorithm::Blake3_256).unwrap_err();
  assert_eq!(corrupt.code(), "crc_mismatch");

  let mut definition = fixture("asem-sha512-definition-valid.bin");
  definition.truncate(definition.len() - 1);
  let truncated = decode_semantic_definition_record(&definition, HashAlgorithm::Sha512).unwrap_err();
  assert_eq!(truncated.code(), "semantic_total_length");
}
