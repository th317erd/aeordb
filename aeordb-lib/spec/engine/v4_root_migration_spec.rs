use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::v4::entity::{EntryTypeV4, WHOLE_ENTITY_V1_FLAG_SYSTEM};
use aeordb::engine::v4::namespace::{
  EncodedNamespaceRootV1, EncodedSemanticObjectV1, NamespaceRootWriteV1, NamespaceTreeEdgeV0, NamespaceTreeLayoutV0,
  SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, decode_namespace_tree_root_v0, encode_namespace_root,
  encode_semantic_state_object,
};
use aeordb::engine::v4::root_authority::{
  ImmutableNamespaceAuthorityInputV1, RootAdmissionCommitV1, RootAuthorityKindV1, RootAuthorityReferenceRoleV1, RootPublicationPrepareV1,
  decode_immutable_namespace_authority, encode_root_admission_commit_control, encode_root_publication_prepare_control,
};
use aeordb::engine::{CompressionAlgorithm, EntryType as LegacyEntryType, HashAlgorithm, StorageEngine};
use sha2::{Digest, Sha512};

const INITIAL_CAPABILITIES: [u8; 32] = [
  0x7f, 0x00, 0x6c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
  0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

struct AuthorityBundle {
  algorithm: HashAlgorithm,
  database_id: [u8; 16],
  root_hash: Vec<u8>,
  root_value: Vec<u8>,
  root_entity: Vec<u8>,
  namespace_tree_hash: Vec<u8>,
  namespace_tree_entity: Vec<u8>,
  semantic_state_id: Vec<u8>,
  semantic_state_object: Vec<u8>,
  admission_control: Vec<u8>,
}

#[derive(Clone, Copy)]
struct IndependentEntityWrite<'a> {
  algorithm: HashAlgorithm,
  entity_version: u8,
  entry_type: EntryTypeV4,
  flags: u8,
  compression_algorithm: CompressionAlgorithm,
  timestamp_ms: u64,
  write_sequence: u64,
  key: &'a [u8],
  stored_value: &'a [u8],
}

#[test]
fn namespace_and_semantic_root_writers_match_independent_fixtures_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let hash_width = algorithm.hash_length();
    let algorithm_name = algorithm_fixture_name(algorithm);
    let expected_root = fixture(&format!("directory-index-v1/adir-{algorithm_name}-namespace-root-valid.bin"));
    let root = encode_namespace_root(
      &NamespaceRootWriteV1 {
        required_capabilities: expected_root[36..68].try_into().unwrap(),
        namespace_tree_root: expected_root[72..72 + hash_width].to_vec(),
        semantic_state_root: expected_root[72 + hash_width..72 + 2 * hash_width].to_vec(),
      },
      algorithm,
    )
    .unwrap();
    assert_eq!(
      root,
      EncodedNamespaceRootV1 {
        root_hash: independent_digest(algorithm, &[b"aeordb.directory-index.immutable.v1\0", &3u16.to_le_bytes(), &expected_root]),
        value: expected_root,
      }
    );

    for fixture_name in ["state-complete", "state-content-only"] {
      let expected = fixture(&format!("semantic-object-v1/asem-{algorithm_name}-{fixture_name}.bin"));
      let body_offset = 32;
      let availability = if fixture_name == "state-complete" {
        let hashes_offset = body_offset + 48;
        let counts_offset = hashes_offset + 3 * hash_width;
        SemanticAvailabilityV1::Complete {
          compiler_fingerprint: expected[hashes_offset..hashes_offset + hash_width].to_vec(),
          semantic_registry_fingerprint: expected[hashes_offset + hash_width..hashes_offset + 2 * hash_width].to_vec(),
          catalog_root: expected[hashes_offset + 2 * hash_width..hashes_offset + 3 * hash_width].to_vec(),
          catalog_record_count: fixture_u64(&expected, counts_offset),
          catalog_node_count: fixture_u64(&expected, counts_offset + 8),
          definition_count: fixture_u64(&expected, counts_offset + 16),
          dependency_count: fixture_u64(&expected, counts_offset + 24),
        }
      } else {
        SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured }
      };
      let encoded = encode_semantic_state_object(
        &SemanticStateWriteV1 { required_capabilities: expected[body_offset + 4..body_offset + 36].try_into().unwrap(), availability },
        algorithm,
      )
      .unwrap();
      assert_eq!(
        encoded,
        EncodedSemanticObjectV1 {
          object_id: independent_digest(algorithm, &[b"aeordb.semantic-object.immutable.v1\0", &1u16.to_le_bytes(), &expected]),
          value: expected,
        }
      );
    }
  }
}

#[test]
fn root_prepare_and_admission_writers_match_independent_fixtures_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let hash_width = algorithm.hash_length();
    let algorithm_name = algorithm_fixture_name(algorithm);
    let expected_prepare = fixture(&format!("system-control-v1/control-{algorithm_name}-root-publication-prepare-valid.bin"));
    let prepare_body = &expected_prepare[32..expected_prepare.len() - 4];
    let prepare = RootPublicationPrepareV1 {
      database_id: prepare_body[..16].try_into().unwrap(),
      transaction_id: prepare_body[16..32].try_into().unwrap(),
      created_at_ms: fixture_i64(prepare_body, 32),
      target_namespace_root: prepare_body[40..40 + hash_width].to_vec(),
      target_semantic_state: prepare_body[40 + hash_width..40 + 2 * hash_width].to_vec(),
      typed_closure_digest: prepare_body[40 + 2 * hash_width..40 + 3 * hash_width].to_vec(),
      authority_kind: RootAuthorityKindV1::Head,
      authority_identity: prepare_body[64 + 5 * hash_width..].to_vec(),
      expected_authority_before: prepare_body[48 + 3 * hash_width..48 + 4 * hash_width].to_vec(),
      expected_authority_after: prepare_body[48 + 4 * hash_width..48 + 5 * hash_width].to_vec(),
      intended_header_slot_sequence: fixture_u64(prepare_body, 48 + 5 * hash_width),
      intended_publication_sequence: fixture_u64(prepare_body, 56 + 5 * hash_width),
    };
    assert_eq!(encode_root_publication_prepare_control(&prepare, algorithm).unwrap(), expected_prepare);

    let expected_commit = fixture(&format!("system-control-v1/control-{algorithm_name}-root-admission-commit-valid.bin"));
    let commit_body = &expected_commit[32..expected_commit.len() - 4];
    let commit = RootAdmissionCommitV1 {
      database_id: commit_body[..16].try_into().unwrap(),
      namespace_root: commit_body[16..16 + hash_width].to_vec(),
      transaction_id: commit_body[16 + hash_width..32 + hash_width].try_into().unwrap(),
      publication_started_at_ms: fixture_i64(commit_body, 32 + hash_width),
      authority_kind: RootAuthorityKindV1::Head,
      recovered_from_selected_authority: false,
      authority_identity_digest: commit_body[48 + hash_width..48 + 2 * hash_width].to_vec(),
      authority_after: commit_body[48 + 2 * hash_width..48 + 3 * hash_width].to_vec(),
      selected_header_slot_sequence: fixture_u64(commit_body, 48 + 3 * hash_width),
      publication_sequence: fixture_u64(commit_body, 56 + 3 * hash_width),
      prepare_payload_hash: commit_body[64 + 3 * hash_width..64 + 4 * hash_width].to_vec(),
    };
    assert_eq!(encode_root_admission_commit_control(&commit, algorithm).unwrap(), expected_commit);
  }
}

#[test]
fn root_authority_writers_reject_every_invalid_input_class() {
  let algorithm = HashAlgorithm::Blake3_256;
  let hash_width = algorithm.hash_length();

  let valid_namespace = NamespaceRootWriteV1 {
    required_capabilities: INITIAL_CAPABILITIES,
    namespace_tree_root: vec![1; hash_width],
    semantic_state_root: vec![2; hash_width],
  };
  let mut invalid_namespace = valid_namespace.clone();
  invalid_namespace.required_capabilities[3] = 1;
  assert_eq!(encode_namespace_root(&invalid_namespace, algorithm).unwrap_err().code(), "unsupported_required_capability");
  let mut invalid_namespace = valid_namespace.clone();
  invalid_namespace.namespace_tree_root.pop();
  assert_eq!(encode_namespace_root(&invalid_namespace, algorithm).unwrap_err().code(), "namespace_root_zero_edge");
  let mut invalid_namespace = valid_namespace;
  invalid_namespace.semantic_state_root.fill(0);
  assert_eq!(encode_namespace_root(&invalid_namespace, algorithm).unwrap_err().code(), "namespace_root_zero_edge");

  let valid_semantic = SemanticStateWriteV1 {
    required_capabilities: INITIAL_CAPABILITIES,
    availability: SemanticAvailabilityV1::Complete {
      compiler_fingerprint: vec![3; hash_width],
      semantic_registry_fingerprint: vec![4; hash_width],
      catalog_root: vec![5; hash_width],
      catalog_record_count: 1,
      catalog_node_count: 1,
      definition_count: 0,
      dependency_count: 0,
    },
  };
  let mut invalid_semantic = valid_semantic.clone();
  invalid_semantic.required_capabilities[3] = 1;
  assert_eq!(encode_semantic_state_object(&invalid_semantic, algorithm).unwrap_err().code(), "unsupported_required_capability");
  let mut invalid_semantic = valid_semantic.clone();
  if let SemanticAvailabilityV1::Complete { compiler_fingerprint, .. } = &mut invalid_semantic.availability {
    compiler_fingerprint.fill(0);
  }
  assert_eq!(encode_semantic_state_object(&invalid_semantic, algorithm).unwrap_err().code(), "semantic_state_hash_width");
  let mut invalid_semantic = valid_semantic.clone();
  if let SemanticAvailabilityV1::Complete { semantic_registry_fingerprint, .. } = &mut invalid_semantic.availability {
    semantic_registry_fingerprint.pop();
  }
  assert_eq!(encode_semantic_state_object(&invalid_semantic, algorithm).unwrap_err().code(), "semantic_state_hash_width");
  let mut invalid_semantic = valid_semantic.clone();
  if let SemanticAvailabilityV1::Complete { catalog_root, .. } = &mut invalid_semantic.availability {
    catalog_root.fill(0);
  }
  assert_eq!(encode_semantic_state_object(&invalid_semantic, algorithm).unwrap_err().code(), "semantic_state_hash_width");
  let mut invalid_semantic = valid_semantic.clone();
  if let SemanticAvailabilityV1::Complete { catalog_record_count, .. } = &mut invalid_semantic.availability {
    *catalog_record_count = 0;
  }
  assert_eq!(encode_semantic_state_object(&invalid_semantic, algorithm).unwrap_err().code(), "semantic_state_complete_invariant");
  let mut invalid_semantic = valid_semantic;
  if let SemanticAvailabilityV1::Complete { catalog_node_count, .. } = &mut invalid_semantic.availability {
    *catalog_node_count = 0;
  }
  assert_eq!(encode_semantic_state_object(&invalid_semantic, algorithm).unwrap_err().code(), "semantic_state_complete_invariant");

  let valid_prepare = RootPublicationPrepareV1 {
    database_id: [1; 16],
    transaction_id: [2; 16],
    created_at_ms: 1,
    target_namespace_root: vec![3; hash_width],
    target_semantic_state: vec![4; hash_width],
    typed_closure_digest: vec![5; hash_width],
    authority_kind: RootAuthorityKindV1::Head,
    authority_identity: vec![6; 12],
    expected_authority_before: vec![0; hash_width],
    expected_authority_after: vec![7; hash_width],
    intended_header_slot_sequence: 1,
    intended_publication_sequence: 1,
  };
  let mut invalid_prepare = valid_prepare.clone();
  invalid_prepare.database_id.fill(0);
  assert_eq!(encode_root_publication_prepare_control(&invalid_prepare, algorithm).unwrap_err().code(), "system_control_database_id");
  let mut invalid_prepare = valid_prepare.clone();
  invalid_prepare.transaction_id.fill(0);
  assert_eq!(encode_root_publication_prepare_control(&invalid_prepare, algorithm).unwrap_err().code(), "root_prepare_identity");
  let mut invalid_prepare = valid_prepare.clone();
  invalid_prepare.created_at_ms = -1;
  assert_eq!(encode_root_publication_prepare_control(&invalid_prepare, algorithm).unwrap_err().code(), "root_prepare_hashes");
  let mut invalid_prepare = valid_prepare.clone();
  invalid_prepare.target_namespace_root.pop();
  assert_eq!(encode_root_publication_prepare_control(&invalid_prepare, algorithm).unwrap_err().code(), "root_prepare_hashes");
  let mut invalid_prepare = valid_prepare.clone();
  invalid_prepare.target_semantic_state.fill(0);
  assert_eq!(encode_root_publication_prepare_control(&invalid_prepare, algorithm).unwrap_err().code(), "root_prepare_hashes");
  let mut invalid_prepare = valid_prepare.clone();
  invalid_prepare.typed_closure_digest.fill(0);
  assert_eq!(encode_root_publication_prepare_control(&invalid_prepare, algorithm).unwrap_err().code(), "root_prepare_hashes");
  let mut invalid_prepare = valid_prepare.clone();
  invalid_prepare.expected_authority_before.pop();
  assert_eq!(encode_root_publication_prepare_control(&invalid_prepare, algorithm).unwrap_err().code(), "root_prepare_hashes");
  let mut invalid_prepare = valid_prepare.clone();
  invalid_prepare.expected_authority_after.fill(0);
  assert_eq!(encode_root_publication_prepare_control(&invalid_prepare, algorithm).unwrap_err().code(), "root_prepare_hashes");
  let mut invalid_prepare = valid_prepare.clone();
  invalid_prepare.authority_identity.clear();
  assert_eq!(encode_root_publication_prepare_control(&invalid_prepare, algorithm).unwrap_err().code(), "root_prepare_authority_length");
  let mut invalid_prepare = valid_prepare.clone();
  invalid_prepare.authority_identity.resize(4_097, 6);
  assert_eq!(encode_root_publication_prepare_control(&invalid_prepare, algorithm).unwrap_err().code(), "root_prepare_authority_length");
  let mut invalid_prepare = valid_prepare.clone();
  invalid_prepare.intended_header_slot_sequence = 0;
  assert_eq!(encode_root_publication_prepare_control(&invalid_prepare, algorithm).unwrap_err().code(), "root_prepare_sequences");
  let mut invalid_prepare = valid_prepare;
  invalid_prepare.intended_publication_sequence = 0;
  assert_eq!(encode_root_publication_prepare_control(&invalid_prepare, algorithm).unwrap_err().code(), "root_prepare_sequences");

  let valid_commit = RootAdmissionCommitV1 {
    database_id: [1; 16],
    namespace_root: vec![3; hash_width],
    transaction_id: [2; 16],
    publication_started_at_ms: 1,
    authority_kind: RootAuthorityKindV1::Head,
    recovered_from_selected_authority: false,
    authority_identity_digest: vec![4; hash_width],
    authority_after: vec![5; hash_width],
    selected_header_slot_sequence: 1,
    publication_sequence: 1,
    prepare_payload_hash: vec![6; hash_width],
  };
  let mut invalid_commit = valid_commit.clone();
  invalid_commit.database_id.fill(0);
  assert_eq!(encode_root_admission_commit_control(&invalid_commit, algorithm).unwrap_err().code(), "system_control_database_id");
  let mut invalid_commit = valid_commit.clone();
  invalid_commit.transaction_id.fill(0);
  assert_eq!(encode_root_admission_commit_control(&invalid_commit, algorithm).unwrap_err().code(), "root_commit_identity");
  let mut invalid_commit = valid_commit.clone();
  invalid_commit.publication_started_at_ms = -1;
  assert_eq!(encode_root_admission_commit_control(&invalid_commit, algorithm).unwrap_err().code(), "root_commit_time");
  let mut invalid_commit = valid_commit.clone();
  invalid_commit.namespace_root.pop();
  assert_eq!(encode_root_admission_commit_control(&invalid_commit, algorithm).unwrap_err().code(), "root_commit_identity");
  let mut invalid_commit = valid_commit.clone();
  invalid_commit.authority_identity_digest.fill(0);
  assert_eq!(encode_root_admission_commit_control(&invalid_commit, algorithm).unwrap_err().code(), "root_commit_identity");
  let mut invalid_commit = valid_commit.clone();
  invalid_commit.authority_after.fill(0);
  assert_eq!(encode_root_admission_commit_control(&invalid_commit, algorithm).unwrap_err().code(), "root_commit_identity");
  let mut invalid_commit = valid_commit.clone();
  invalid_commit.prepare_payload_hash.fill(0);
  assert_eq!(encode_root_admission_commit_control(&invalid_commit, algorithm).unwrap_err().code(), "root_commit_identity");
  let mut invalid_commit = valid_commit.clone();
  invalid_commit.selected_header_slot_sequence = 0;
  assert_eq!(encode_root_admission_commit_control(&invalid_commit, algorithm).unwrap_err().code(), "root_commit_sequences");
  let mut invalid_commit = valid_commit;
  invalid_commit.publication_sequence = 0;
  assert_eq!(encode_root_admission_commit_control(&invalid_commit, algorithm).unwrap_err().code(), "root_commit_sequences");
}

#[test]
fn immutable_authority_resolves_complete_and_content_only_semantics_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let complete = authority_bundle(algorithm, false);
    let decoded = decode_bundle(&complete).unwrap();
    assert_eq!(decoded.root.root_hash, complete.root_hash);
    assert_eq!(decoded.namespace_tree.root_hash, decoded.root.namespace_tree_root);
    assert_eq!(decoded.semantic_state.object_id, decoded.root.semantic_state_root);
    assert_eq!(decoded.admission.namespace_root, decoded.root.root_hash);
    assert_eq!(decoded.root.required_capabilities, INITIAL_CAPABILITIES);
    assert_eq!(decoded.root.namespace_tree_codec, 1);
    assert_eq!(decoded.root.semantic_state_codec, 1);
    assert_eq!(decoded.semantic_state.required_capabilities, INITIAL_CAPABILITIES);
    assert_eq!(decoded.semantic_state.semantic_catalog_codec, 1);
    assert_eq!(decoded.semantic_state.semantic_definition_codec, 1);
    assert_eq!(decoded.semantic_state.compiler_profile_version, 1);
    let algorithm_name = algorithm_fixture_name(algorithm);
    let catalog = fixture(&format!("semantic-object-v1/asem-{algorithm_name}-catalog-internal-valid.bin"));
    let expected_catalog_root = independent_digest(algorithm, &[b"aeordb.semantic-object.immutable.v1\0", &3u16.to_le_bytes(), &catalog]);
    assert_eq!(
      decoded.semantic_state.availability,
      SemanticAvailabilityV1::Complete {
        compiler_fingerprint: independent_digest(algorithm, &[b"aeordb-v4-reference-compiler-profile-v1"]),
        semantic_registry_fingerprint: independent_digest(algorithm, &[b"aeordb-v4-reference-semantic-registry-v1"]),
        catalog_root: expected_catalog_root,
        catalog_record_count: 2,
        catalog_node_count: 3,
        definition_count: 1,
        dependency_count: 0,
      }
    );

    let content_only = authority_bundle(algorithm, true);
    let decoded = decode_bundle(&content_only).unwrap();
    assert_eq!(
      decoded.semantic_state.availability,
      SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured }
    );
  }
}

#[test]
fn namespace_tree_reader_returns_validated_flat_leaf_and_internal_edges_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let hash_width = algorithm.hash_length();
    let first_edge = vec![0x11; hash_width];
    let second_edge = vec![0x22; hash_width];
    let entries = vec![
      independent_child_entry("alpha", &first_edge, EntryTypeV4::FileRecord.to_u8()),
      independent_child_entry("bravo", &second_edge, EntryTypeV4::FileRecord.to_u8()),
    ];

    let flat_value = independent_flat_entries(&entries);
    let flat = decode_tree_value(algorithm, b"dirc:", &flat_value).unwrap();
    assert_eq!(flat.layout, NamespaceTreeLayoutV0::Flat { child_count: 2 });
    assert_eq!(
      flat.edges,
      vec![
        NamespaceTreeEdgeV0::Entry { name: "alpha".to_string(), entry_type: LegacyEntryType::FileRecord, identity: first_edge.clone() },
        NamespaceTreeEdgeV0::Entry { name: "bravo".to_string(), entry_type: LegacyEntryType::FileRecord, identity: second_edge.clone() },
      ]
    );

    let leaf_value = independent_btree_leaf(&entries);
    let leaf = decode_tree_value(algorithm, b"btree:", &leaf_value).unwrap();
    assert_eq!(leaf.layout, NamespaceTreeLayoutV0::BTreeLeaf { child_count: 2 });
    assert_eq!(
      leaf.edges,
      vec![
        NamespaceTreeEdgeV0::Entry { name: "alpha".to_string(), entry_type: LegacyEntryType::FileRecord, identity: first_edge.clone() },
        NamespaceTreeEdgeV0::Entry { name: "bravo".to_string(), entry_type: LegacyEntryType::FileRecord, identity: second_edge.clone() },
      ]
    );

    let internal_value = independent_btree_internal(&["bravo"], &[first_edge.clone(), second_edge.clone()]);
    let internal = decode_tree_value(algorithm, b"btree:", &internal_value).unwrap();
    assert_eq!(internal.layout, NamespaceTreeLayoutV0::BTreeInternal { separator_count: 1, child_count: 2 });
    assert_eq!(
      internal.edges,
      vec![NamespaceTreeEdgeV0::BTreeNode { identity: first_edge }, NamespaceTreeEdgeV0::BTreeNode { identity: second_edge }]
    );
  }
}

#[test]
fn namespace_tree_reader_enforces_outer_entity_identity_and_hash_width() {
  let algorithm = HashAlgorithm::Blake3_256;
  let value = Vec::new();
  let root_hash = independent_digest(algorithm, &[b"dirc:", &value]);
  let wrong_hash = vec![0x77; algorithm.hash_length()];
  let cases = [
    (
      wrap_tree_entity(algorithm, &value, &root_hash, EntryTypeV4::FileRecord, 0, CompressionAlgorithm::None),
      root_hash.as_slice(),
      "namespace_tree_entity_type",
    ),
    (
      wrap_tree_entity(algorithm, &value, &root_hash, EntryTypeV4::DirectoryIndex, WHOLE_ENTITY_V1_FLAG_SYSTEM, CompressionAlgorithm::None),
      root_hash.as_slice(),
      "namespace_tree_entity_representation",
    ),
    (
      wrap_tree_entity(algorithm, &value, &root_hash, EntryTypeV4::DirectoryIndex, 0, CompressionAlgorithm::Zstd),
      root_hash.as_slice(),
      "namespace_tree_entity_representation",
    ),
    (
      wrap_tree_entity(algorithm, &value, &wrong_hash, EntryTypeV4::DirectoryIndex, 0, CompressionAlgorithm::None),
      root_hash.as_slice(),
      "namespace_tree_entity_key",
    ),
    (
      wrap_tree_entity(algorithm, &value, &wrong_hash, EntryTypeV4::DirectoryIndex, 0, CompressionAlgorithm::None),
      wrong_hash.as_slice(),
      "namespace_tree_content_identity",
    ),
  ];
  for (entity, expected_hash, expected_code) in cases {
    let error = decode_namespace_tree_root_v0(&entity, expected_hash, algorithm, u64::MAX).unwrap_err();
    assert_eq!(error.code(), expected_code);
  }

  let wrong_version = independent_whole_entity(&IndependentEntityWrite {
    algorithm,
    entity_version: 1,
    entry_type: EntryTypeV4::DirectoryIndex,
    flags: 0,
    compression_algorithm: CompressionAlgorithm::None,
    timestamp_ms: 1,
    write_sequence: 1,
    key: &root_hash,
    stored_value: &value,
  });
  let error = decode_namespace_tree_root_v0(&wrong_version, &root_hash, algorithm, u64::MAX).unwrap_err();
  assert_eq!(error.code(), "namespace_tree_entity_type");

  let entity = wrap_tree_entity(algorithm, &value, &root_hash, EntryTypeV4::DirectoryIndex, 0, CompressionAlgorithm::None);
  let short_hash = &root_hash[..root_hash.len() - 1];
  let error = decode_namespace_tree_root_v0(&entity, short_hash, algorithm, u64::MAX).unwrap_err();
  assert_eq!(error.code(), "namespace_tree_hash_width");
}

#[test]
fn namespace_tree_reader_rejects_noncanonical_children_and_edges() {
  let algorithm = HashAlgorithm::Blake3_256;
  let hash_width = algorithm.hash_length();
  let first_edge = vec![0x11; hash_width];
  let second_edge = vec![0x22; hash_width];
  let zero_edge = vec![0; hash_width];

  let unordered_flat = independent_flat_entries(&[
    independent_child_entry("bravo", &first_edge, EntryTypeV4::FileRecord.to_u8()),
    independent_child_entry("alpha", &second_edge, EntryTypeV4::FileRecord.to_u8()),
  ]);
  let invalid_type_flat = independent_flat_entries(&[independent_child_entry("alpha", &first_edge, 0xff)]);
  let zero_edge_leaf = independent_btree_leaf(&[independent_child_entry("alpha", &zero_edge, EntryTypeV4::FileRecord.to_u8())]);
  let unordered_internal =
    independent_btree_internal(&["bravo", "alpha"], &[first_edge.clone(), second_edge.clone(), vec![0x33; hash_width]]);
  let zero_edge_internal = independent_btree_internal(&["bravo"], &[first_edge, zero_edge]);

  for (domain, value) in [
    (b"dirc:".as_slice(), unordered_flat),
    (b"dirc:".as_slice(), invalid_type_flat),
    (b"btree:".as_slice(), zero_edge_leaf),
    (b"btree:".as_slice(), unordered_internal),
    (b"btree:".as_slice(), zero_edge_internal),
  ] {
    let error = decode_tree_value(algorithm, domain, &value).unwrap_err();
    assert_eq!(error.code(), "namespace_tree_payload");
  }
}

#[test]
fn immutable_authority_rejects_valid_but_cross_root_semantic_and_admission_objects() {
  let complete = authority_bundle(HashAlgorithm::Blake3_256, false);
  let content_only = authority_bundle(HashAlgorithm::Blake3_256, true);

  let mut input = authority_input(&complete);
  input.semantic_state_object = Some(&content_only.semantic_state_object);
  let error = decode_immutable_namespace_authority(input, complete.algorithm, u64::MAX).unwrap_err();
  assert_eq!(error.role(), RootAuthorityReferenceRoleV1::SemanticStateRoot);
  assert_eq!(error.code(), "semantic_state_identity_mismatch");
  assert_eq!(error.identity(), complete.semantic_state_id);

  let mut input = authority_input(&complete);
  input.admission_control = Some(&content_only.admission_control);
  let error = decode_immutable_namespace_authority(input, complete.algorithm, u64::MAX).unwrap_err();
  assert_eq!(error.role(), RootAuthorityReferenceRoleV1::RootAdmissionCommit);
  assert_eq!(error.code(), "root_admission_identity_mismatch");
  assert_eq!(error.identity(), complete.root_hash);
}

#[test]
fn immutable_authority_enforces_namespace_root_outer_entity_contract() {
  let bundle = authority_bundle(HashAlgorithm::Blake3_256, false);
  let wrong_key = vec![0x9a; bundle.algorithm.hash_length()];
  let cases = [
    (
      wrap_root_entity(&bundle, EntryTypeV4::FileRecord, WHOLE_ENTITY_V1_FLAG_SYSTEM, CompressionAlgorithm::None, &bundle.root_hash),
      "namespace_root_entity_type",
    ),
    (
      wrap_root_entity(&bundle, EntryTypeV4::DirectoryIndex, 0, CompressionAlgorithm::None, &bundle.root_hash),
      "namespace_root_entity_representation",
    ),
    (
      wrap_root_entity(&bundle, EntryTypeV4::DirectoryIndex, WHOLE_ENTITY_V1_FLAG_SYSTEM, CompressionAlgorithm::Zstd, &bundle.root_hash),
      "namespace_root_entity_representation",
    ),
    (
      wrap_root_entity(&bundle, EntryTypeV4::DirectoryIndex, WHOLE_ENTITY_V1_FLAG_SYSTEM, CompressionAlgorithm::None, &wrong_key),
      "namespace_root_entity_key",
    ),
  ];

  for (root_entity, expected_code) in cases {
    let mut input = authority_input(&bundle);
    input.root_entity = Some(&root_entity);
    let error = decode_immutable_namespace_authority(input, bundle.algorithm, u64::MAX).unwrap_err();
    assert_eq!(error.role(), RootAuthorityReferenceRoleV1::NamespaceRoot);
    assert_eq!(error.code(), expected_code);
    assert_eq!(error.identity(), bundle.root_hash);
  }

  let wrong_version = independent_whole_entity(&IndependentEntityWrite {
    algorithm: bundle.algorithm,
    entity_version: 0,
    entry_type: EntryTypeV4::DirectoryIndex,
    flags: WHOLE_ENTITY_V1_FLAG_SYSTEM,
    compression_algorithm: CompressionAlgorithm::None,
    timestamp_ms: 2,
    write_sequence: 2,
    key: &bundle.root_hash,
    stored_value: &bundle.root_value,
  });
  let mut input = authority_input(&bundle);
  input.root_entity = Some(&wrong_version);
  let error = decode_immutable_namespace_authority(input, bundle.algorithm, u64::MAX).unwrap_err();
  assert_eq!(error.role(), RootAuthorityReferenceRoleV1::NamespaceRoot);
  assert_eq!(error.code(), "namespace_root_entity_type");

  let short_root_hash = &bundle.root_hash[..bundle.root_hash.len() - 1];
  let mut input = authority_input(&bundle);
  input.expected_root_hash = short_root_hash;
  let error = decode_immutable_namespace_authority(input, bundle.algorithm, u64::MAX).unwrap_err();
  assert_eq!(error.role(), RootAuthorityReferenceRoleV1::NamespaceRoot);
  assert_eq!(error.code(), "namespace_root_hash_width");
  assert_eq!(error.identity(), short_root_hash);
}

#[test]
fn immutable_authority_rejects_malformed_tree_wrong_semantic_kind_and_database() {
  let bundle = authority_bundle(HashAlgorithm::Blake3_256, false);
  let malformed_tree_value = [0x01, 0x02, 0x03];
  let malformed_tree = independent_whole_entity(&IndependentEntityWrite {
    algorithm: bundle.algorithm,
    entity_version: 0,
    entry_type: EntryTypeV4::DirectoryIndex,
    flags: 0,
    compression_algorithm: CompressionAlgorithm::None,
    timestamp_ms: 1,
    write_sequence: 1,
    key: &bundle.namespace_tree_hash,
    stored_value: &malformed_tree_value,
  });
  let mut input = authority_input(&bundle);
  input.namespace_tree_entity = Some(&malformed_tree);
  let error = decode_immutable_namespace_authority(input, bundle.algorithm, u64::MAX).unwrap_err();
  assert_eq!(error.role(), RootAuthorityReferenceRoleV1::NamespaceTreeRoot);
  assert_eq!(error.code(), "namespace_tree_payload");
  assert_eq!(error.identity(), bundle.namespace_tree_hash);

  let definition = authority_bundle_with_semantic(HashAlgorithm::Blake3_256, "definition-valid");
  let error = decode_bundle(&definition).unwrap_err();
  assert_eq!(error.role(), RootAuthorityReferenceRoleV1::SemanticStateRoot);
  assert_eq!(error.code(), "semantic_state_kind_mismatch");
  assert_eq!(error.identity(), definition.semantic_state_id);

  let mut wrong_database_id = bundle.database_id;
  wrong_database_id[0] ^= 0xff;
  let mut input = authority_input(&bundle);
  input.expected_database_id = &wrong_database_id;
  let error = decode_immutable_namespace_authority(input, bundle.algorithm, u64::MAX).unwrap_err();
  assert_eq!(error.role(), RootAuthorityReferenceRoleV1::RootAdmissionCommit);
  assert_eq!(error.code(), "root_admission_database_mismatch");
  assert_eq!(error.identity(), bundle.root_hash);
}

#[test]
fn root_admission_commit_exposes_the_frozen_typed_fields() {
  let bundle = authority_bundle(HashAlgorithm::Blake3_256, false);
  let admission = decode_bundle(&bundle).unwrap().admission;
  assert_eq!(admission.database_id, std::array::from_fn(|index| 0x10 + index as u8));
  assert_eq!(admission.namespace_root, bundle.root_hash);
  assert_eq!(admission.transaction_id, std::array::from_fn(|index| 0x30 + index as u8));
  assert_eq!(admission.publication_started_at_ms, 1_700_000_014_000);
  assert_eq!(admission.authority_kind, RootAuthorityKindV1::Head);
  assert!(!admission.recovered_from_selected_authority);
  assert_eq!(admission.authority_identity_digest, byte_range(0x40, 32));
  assert_eq!(admission.authority_after, byte_range(0x50, 32));
  assert_eq!(admission.selected_header_slot_sequence, 14);
  assert_eq!(admission.publication_sequence, 100);
  assert_eq!(admission.prepare_payload_hash, byte_range(0x60, 32));
}

#[test]
fn immutable_authority_resolves_exact_bytes_after_real_storage_restart() {
  let bundle = authority_bundle(HashAlgorithm::Blake3_256, false);
  let directory = tempfile::Builder::new().prefix("aeordb-v4-root-reader-").tempdir().unwrap();
  let database_path = directory.path().join("reader.aeordb");
  let database_path = database_path.to_str().unwrap();
  let stored_values = [&bundle.root_entity, &bundle.namespace_tree_entity, &bundle.semantic_state_object, &bundle.admission_control];
  let storage_keys: Vec<_> = stored_values.iter().map(|value| independent_digest(bundle.algorithm, &[value])).collect();

  let engine = StorageEngine::create(database_path).unwrap();
  for (key, value) in storage_keys.iter().zip(stored_values) {
    engine.store_entry(LegacyEntryType::Chunk, key, value).unwrap();
  }
  engine.shutdown().unwrap();
  drop(engine);

  let reopened = StorageEngine::open(database_path).unwrap();
  let persisted_values: Vec<_> = storage_keys
    .iter()
    .map(|key| reopened.get_entry_verified(key).unwrap().expect("stored authority object must survive restart").2)
    .collect();
  let decoded = decode_immutable_namespace_authority(
    ImmutableNamespaceAuthorityInputV1 {
      expected_root_hash: &bundle.root_hash,
      expected_database_id: &bundle.database_id,
      root_entity: Some(&persisted_values[0]),
      namespace_tree_entity: Some(&persisted_values[1]),
      semantic_state_object: Some(&persisted_values[2]),
      admission_control: Some(&persisted_values[3]),
    },
    bundle.algorithm,
    u64::MAX,
  )
  .unwrap();
  assert_eq!(decoded.root.root_hash, bundle.root_hash);
  assert_eq!(decoded.admission.database_id, bundle.database_id);
  reopened.shutdown().unwrap();
}

#[test]
fn immutable_authority_reports_each_missing_reference_without_guessing() {
  let bundle = authority_bundle(HashAlgorithm::Blake3_256, false);
  let cases = [
    RootAuthorityReferenceRoleV1::NamespaceRoot,
    RootAuthorityReferenceRoleV1::NamespaceTreeRoot,
    RootAuthorityReferenceRoleV1::SemanticStateRoot,
    RootAuthorityReferenceRoleV1::RootAdmissionCommit,
  ];

  for role in cases {
    let mut input = authority_input(&bundle);
    match role {
      RootAuthorityReferenceRoleV1::NamespaceRoot => input.root_entity = None,
      RootAuthorityReferenceRoleV1::NamespaceTreeRoot => input.namespace_tree_entity = None,
      RootAuthorityReferenceRoleV1::SemanticStateRoot => input.semantic_state_object = None,
      RootAuthorityReferenceRoleV1::RootAdmissionCommit => input.admission_control = None,
    }
    let error = decode_immutable_namespace_authority(input, bundle.algorithm, u64::MAX).unwrap_err();
    assert_eq!(error.role(), role);
    assert_eq!(error.code(), "missing_immutable_reference");
    let expected_identity = match role {
      RootAuthorityReferenceRoleV1::NamespaceRoot | RootAuthorityReferenceRoleV1::RootAdmissionCommit => &bundle.root_hash,
      RootAuthorityReferenceRoleV1::NamespaceTreeRoot => &bundle.namespace_tree_hash,
      RootAuthorityReferenceRoleV1::SemanticStateRoot => &bundle.semantic_state_id,
    };
    assert_eq!(error.identity(), expected_identity);
  }
}

#[test]
fn immutable_authority_codecs_are_disconnected_from_storage_and_service_authority() {
  let root = repository_root();
  let sources = ["namespace.rs", "root_authority.rs"]
    .into_iter()
    .map(|name| fs::read_to_string(root.join("aeordb-lib/src/engine/v4").join(name)).unwrap())
    .collect::<Vec<_>>()
    .join("\n");
  for forbidden in [
    "StorageEngine",
    "DirectoryOps",
    "NamespaceMutationCoordinator",
    "update_head",
    "publish_namespace_root",
    "V4ControlStore",
    "admit_v4_header",
  ] {
    assert!(!sources.contains(forbidden), "P3b-2a codec unexpectedly contains {forbidden}");
  }

  let production_sources = read_rust_sources(&root.join("aeordb-lib/src"));
  let first_authority = fs::read_to_string(root.join("aeordb-lib/src/engine/v4/first_authority.rs")).unwrap();
  let migration_destination = fs::read_to_string(root.join("aeordb-lib/src/engine/v4/migration_destination.rs")).unwrap();
  for (encoder, expected_production_occurrences, expected_first_authority_occurrences, expected_migration_destination_occurrences) in [
    ("encode_namespace_root", 3, 2, 0),
    ("encode_semantic_state_object", 3, 0, 2),
    ("encode_root_publication_prepare_control", 4, 3, 0),
    ("encode_root_admission_commit_control", 4, 3, 0),
  ] {
    assert_eq!(
      production_sources.matches(encoder).count(),
      expected_production_occurrences,
      "P3b root encoder {encoder} escaped its reviewed codec, first-authority, and migration-destination owners"
    );
    assert_eq!(
      first_authority.matches(encoder).count(),
      expected_first_authority_occurrences,
      "P3b root encoder {encoder} has an unexpected first-authority call shape"
    );
    assert_eq!(
      migration_destination.matches(encoder).count(),
      expected_migration_destination_occurrences,
      "P3b root encoder {encoder} has an unexpected migration-destination call shape"
    );
  }

  let reference_source = fs::read_to_string(root.join("aeordb-lib/spec/engine/v4_root_migration_spec.rs")).unwrap();
  for production_encoder_name in
    [["encode_whole", "entity"].join("_"), ["serialize", "child_entries"].join("_"), ["BTreeNode", "serialize"].join("::")]
  {
    assert!(
      !reference_source.contains(&production_encoder_name),
      "reference model must not use production encoder {production_encoder_name}"
    );
  }
}

fn decode_bundle(
  bundle: &AuthorityBundle,
) -> Result<aeordb::engine::v4::root_authority::ImmutableNamespaceAuthorityV1, aeordb::engine::v4::root_authority::RootAuthorityReadError> {
  decode_immutable_namespace_authority(authority_input(bundle), bundle.algorithm, u64::MAX)
}

fn authority_input(bundle: &AuthorityBundle) -> ImmutableNamespaceAuthorityInputV1<'_> {
  ImmutableNamespaceAuthorityInputV1 {
    expected_root_hash: &bundle.root_hash,
    expected_database_id: &bundle.database_id,
    root_entity: Some(&bundle.root_entity),
    namespace_tree_entity: Some(&bundle.namespace_tree_entity),
    semantic_state_object: Some(&bundle.semantic_state_object),
    admission_control: Some(&bundle.admission_control),
  }
}

fn authority_bundle(algorithm: HashAlgorithm, content_only: bool) -> AuthorityBundle {
  let semantic_name = if content_only { "state-content-only" } else { "state-complete" };
  authority_bundle_with_semantic(algorithm, semantic_name)
}

fn authority_bundle_with_semantic(algorithm: HashAlgorithm, semantic_name: &str) -> AuthorityBundle {
  let hash_width = algorithm.hash_length();
  let algorithm_name = algorithm_fixture_name(algorithm);
  let semantic_kind = match semantic_name {
    "state-complete" | "state-content-only" => 1u16,
    "definition-valid" => 4u16,
    other => panic!("unsupported semantic fixture {other}"),
  };
  let semantic_state_object = fixture(&format!("semantic-object-v1/asem-{algorithm_name}-{semantic_name}.bin"));
  let semantic_state_id =
    independent_digest(algorithm, &[b"aeordb.semantic-object.immutable.v1\0", &semantic_kind.to_le_bytes(), &semantic_state_object]);

  let namespace_tree_value = Vec::new();
  let namespace_tree_hash = independent_digest(algorithm, &[b"dirc:", &namespace_tree_value]);
  let namespace_tree_entity = independent_whole_entity(&IndependentEntityWrite {
    algorithm,
    entity_version: 0,
    entry_type: EntryTypeV4::DirectoryIndex,
    flags: 0,
    compression_algorithm: CompressionAlgorithm::None,
    timestamp_ms: 1,
    write_sequence: 1,
    key: &namespace_tree_hash,
    stored_value: &namespace_tree_value,
  });

  let mut root_value = fixture(&format!("directory-index-v1/adir-{algorithm_name}-namespace-root-valid.bin"));
  root_value[72..72 + hash_width].copy_from_slice(&namespace_tree_hash);
  root_value[72 + hash_width..72 + 2 * hash_width].copy_from_slice(&semantic_state_id);
  repair_trailing_crc(&mut root_value);
  let root_hash = independent_digest(algorithm, &[b"aeordb.directory-index.immutable.v1\0", &3u16.to_le_bytes(), &root_value]);
  let root_entity = independent_whole_entity(&IndependentEntityWrite {
    algorithm,
    entity_version: 1,
    entry_type: EntryTypeV4::DirectoryIndex,
    flags: WHOLE_ENTITY_V1_FLAG_SYSTEM,
    compression_algorithm: CompressionAlgorithm::None,
    timestamp_ms: 2,
    write_sequence: 2,
    key: &root_hash,
    stored_value: &root_value,
  });

  let mut admission_control = fixture(&format!("system-control-v1/control-{algorithm_name}-root-admission-commit-valid.bin"));
  admission_control[48..48 + hash_width].copy_from_slice(&root_hash);
  repair_trailing_crc(&mut admission_control);
  let database_id = admission_control[32..48].try_into().unwrap();

  AuthorityBundle {
    algorithm,
    database_id,
    root_hash,
    root_value,
    root_entity,
    namespace_tree_hash,
    namespace_tree_entity,
    semantic_state_id,
    semantic_state_object,
    admission_control,
  }
}

fn algorithm_fixture_name(algorithm: HashAlgorithm) -> &'static str {
  match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    other => panic!("unsupported test algorithm {other:?}"),
  }
}

fn wrap_root_entity(
  bundle: &AuthorityBundle,
  entry_type: EntryTypeV4,
  flags: u8,
  compression_algorithm: CompressionAlgorithm,
  key: &[u8],
) -> Vec<u8> {
  independent_whole_entity(&IndependentEntityWrite {
    algorithm: bundle.algorithm,
    entity_version: 1,
    entry_type,
    flags,
    compression_algorithm,
    timestamp_ms: 2,
    write_sequence: 2,
    key,
    stored_value: &bundle.root_value,
  })
}

fn decode_tree_value(
  algorithm: HashAlgorithm,
  identity_domain: &[u8],
  value: &[u8],
) -> Result<aeordb::engine::v4::namespace::NamespaceTreeRootV0, aeordb::engine::v4::reader::FormatError> {
  let root_hash = independent_digest(algorithm, &[identity_domain, value]);
  let entity = wrap_tree_entity(algorithm, value, &root_hash, EntryTypeV4::DirectoryIndex, 0, CompressionAlgorithm::None);
  decode_namespace_tree_root_v0(&entity, &root_hash, algorithm, u64::MAX)
}

fn wrap_tree_entity(
  algorithm: HashAlgorithm,
  value: &[u8],
  key: &[u8],
  entry_type: EntryTypeV4,
  flags: u8,
  compression_algorithm: CompressionAlgorithm,
) -> Vec<u8> {
  independent_whole_entity(&IndependentEntityWrite {
    algorithm,
    entity_version: 0,
    entry_type,
    flags,
    compression_algorithm,
    timestamp_ms: 1,
    write_sequence: 1,
    key,
    stored_value: value,
  })
}

fn independent_child_entry(name: &str, hash: &[u8], entry_type: u8) -> Vec<u8> {
  let name = name.as_bytes();
  let content_type = b"text/plain";
  let mut entry = Vec::with_capacity(1 + hash.len() + 8 * 5 + 4 + name.len() + content_type.len());
  entry.push(entry_type);
  entry.extend_from_slice(hash);
  entry.extend_from_slice(&1u64.to_le_bytes());
  entry.extend_from_slice(&1i64.to_le_bytes());
  entry.extend_from_slice(&1i64.to_le_bytes());
  entry.extend_from_slice(&(name.len() as u16).to_le_bytes());
  entry.extend_from_slice(name);
  entry.extend_from_slice(&(content_type.len() as u16).to_le_bytes());
  entry.extend_from_slice(content_type);
  entry.extend_from_slice(&1u64.to_le_bytes());
  entry.extend_from_slice(&1u64.to_le_bytes());
  entry
}

fn independent_flat_entries(entries: &[Vec<u8>]) -> Vec<u8> {
  let mut value = Vec::new();
  for entry in entries {
    value.extend_from_slice(entry);
  }
  value
}

fn independent_btree_leaf(entries: &[Vec<u8>]) -> Vec<u8> {
  let mut value = vec![0x00];
  value.extend_from_slice(&(entries.len() as u16).to_le_bytes());
  value.extend_from_slice(&independent_flat_entries(entries));
  value
}

fn independent_btree_internal(keys: &[&str], children: &[Vec<u8>]) -> Vec<u8> {
  let mut value = vec![0x01];
  value.extend_from_slice(&(keys.len() as u16).to_le_bytes());
  for key in keys {
    value.extend_from_slice(&(key.len() as u16).to_le_bytes());
    value.extend_from_slice(key.as_bytes());
  }
  for child in children {
    value.extend_from_slice(child);
  }
  value
}

fn byte_range(first: u8, length: usize) -> Vec<u8> {
  (0..length).map(|index| first + index as u8).collect()
}

fn independent_whole_entity(fields: &IndependentEntityWrite<'_>) -> Vec<u8> {
  let fields = *fields;
  let algorithm = fields.algorithm;
  let key = fields.key;
  let stored_value = fields.stored_value;
  let hash_width = algorithm.hash_length();
  let header_length = 77 + hash_width;
  let total_length = header_length + key.len() + stored_value.len();
  let mut entity = vec![0u8; total_length];
  put_u32(&mut entity, 0, 0x0ae0_12db);
  entity[4] = fields.entity_version;
  entity[5] = fields.entry_type as u8;
  put_u16(&mut entity, 6, header_length as u16);
  put_u32(&mut entity, 8, total_length as u32);
  entity[12] = fields.flags;
  put_u16(
    &mut entity,
    13,
    match algorithm {
      HashAlgorithm::Blake3_256 => 0x0001,
      HashAlgorithm::Sha512 => 0x0003,
      other => panic!("unsupported test algorithm {other:?}"),
    },
  );
  entity[15] = match fields.compression_algorithm {
    CompressionAlgorithm::None => 0x00,
    CompressionAlgorithm::Zstd => 0x01,
  };
  put_u32(&mut entity, 17, key.len() as u32);
  put_u32(&mut entity, 21, stored_value.len() as u32);
  put_u64(&mut entity, 25, fields.timestamp_ms);
  put_u64(&mut entity, 33, fields.write_sequence);
  let integrity = independent_digest(
    algorithm,
    &[b"aeordb-entry-v1\0", &entity[4..6], &entity[12..13], &entity[13..17], &entity[17..25], key, stored_value],
  );
  entity[41..41 + hash_width].copy_from_slice(&integrity);
  let checksum_offset = header_length - 4;
  let checksum = crc32fast::hash(&entity[..checksum_offset]);
  put_u32(&mut entity, checksum_offset, checksum);
  let key_end = header_length + key.len();
  entity[header_length..key_end].copy_from_slice(key);
  entity[key_end..].copy_from_slice(stored_value);
  entity
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
  bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
  bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn fixture_u64(bytes: &[u8], offset: usize) -> u64 {
  u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn fixture_i64(bytes: &[u8], offset: usize) -> i64 {
  i64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn independent_digest(algorithm: HashAlgorithm, parts: &[&[u8]]) -> Vec<u8> {
  match algorithm {
    HashAlgorithm::Blake3_256 => {
      let mut hasher = blake3::Hasher::new();
      for part in parts {
        hasher.update(part);
      }
      hasher.finalize().as_bytes().to_vec()
    }
    HashAlgorithm::Sha512 => {
      let mut hasher = Sha512::new();
      for part in parts {
        hasher.update(part);
      }
      hasher.finalize().to_vec()
    }
    other => panic!("unsupported test algorithm {other:?}"),
  }
}

fn repair_trailing_crc(bytes: &mut [u8]) {
  let checksum_offset = bytes.len() - 4;
  let checksum = crc32fast::hash(&bytes[..checksum_offset]);
  bytes[checksum_offset..].copy_from_slice(&checksum.to_le_bytes());
}

fn fixture(relative: &str) -> Vec<u8> {
  fs::read(fixture_root().join(relative)).unwrap()
}

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4")
}

fn repository_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn read_rust_sources(path: &Path) -> String {
  let mut sources = String::new();
  for entry in fs::read_dir(path).unwrap() {
    let path = entry.unwrap().path();
    if path.is_dir() {
      sources.push_str(&read_rust_sources(&path));
    } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
      sources.push_str(&fs::read_to_string(path).unwrap());
    }
  }
  sources
}
