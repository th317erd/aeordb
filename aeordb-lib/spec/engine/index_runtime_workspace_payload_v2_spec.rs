use std::fs;
use std::path::PathBuf;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_coordinator::IndexFlushReasonV1;
use aeordb::engine::v4::index_page::OrderedIndexRoleV1;
use aeordb::engine::v4::index_record::{ScopeDocumentRecordV1, ScopeReverseRecordV1, encode_scope_document_record, encode_scope_reverse_record};
use aeordb::engine::v4::index_runtime_workspace::{
  IndexWorkspaceRuntimeBatchPayload, decode_index_workspace_object_v1, decode_index_workspace_runtime_batch_payload,
};
use aeordb::engine::v4::index_runtime_workspace_payload_v2::{
  IndexWorkspaceMembershipStateV2, IndexWorkspaceMembershipTransitionWriteV2, IndexWorkspaceMutationOperationV2,
  IndexWorkspaceOwnerClassV2, IndexWorkspaceRuntimeBatchWriteV2, IndexWorkspaceRuntimeMutationWriteV2,
  decode_index_workspace_runtime_batch_payload_v2, encode_index_workspace_runtime_batch_payload_v2,
};

const OBJECT_HEADER_LENGTH: usize = 184;

#[test]
fn airb_v2_writer_byte_matches_both_independent_fixtures() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let expected = fixture_payload(profile);
    let (records, transitions) = fixture_inputs(algorithm);
    let encoded = encode_index_workspace_runtime_batch_payload_v2(&IndexWorkspaceRuntimeBatchWriteV2 {
      hash_algorithm: algorithm,
      coordinator_id: [0x77; 16],
      batch_id: 2,
      reason: IndexFlushReasonV1::Explicit,
      mutations: &records,
      transitions: &transitions,
    })
    .unwrap();
    assert_eq!(encoded, expected, "AIRB v2 mismatch for {profile}");

    let decoded = decode_index_workspace_runtime_batch_payload_v2(&encoded, algorithm).unwrap();
    assert_eq!(decoded.coordinator_id, [0x77; 16]);
    assert_eq!(decoded.batch_id, 2);
    assert_eq!(decoded.mutations.len(), 2);
    assert_eq!(decoded.transitions.len(), 3);
    assert_eq!(decoded.mutations[0].operation, IndexWorkspaceMutationOperationV2::RemoveExisting);
    assert_eq!(decoded.mutations[1].operation, IndexWorkspaceMutationOperationV2::Upsert);
    assert_eq!(decoded.transitions[0].owner_class, IndexWorkspaceOwnerClassV2::ScopeCatalog);
    assert_eq!(decoded.transitions[1].owner_class, IndexWorkspaceOwnerClassV2::ValueStore);
    assert_eq!(decoded.transitions[2].owner_class, IndexWorkspaceOwnerClassV2::FieldIndex);
  }
}

#[test]
fn runtime_payload_dispatch_preserves_v1_and_accepts_v2_inside_the_v1_outer_object() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let v1_object = fixture_object(profile, "runtime-batch-valid");
    let v1 = decode_index_workspace_object_v1(&v1_object).unwrap();
    assert!(matches!(
      decode_index_workspace_runtime_batch_payload(v1.payload, algorithm).unwrap(),
      IndexWorkspaceRuntimeBatchPayload::V1(_)
    ));

    let v2_object = fixture_object(profile, "runtime-batch-v2-valid");
    let v2 = decode_index_workspace_object_v1(&v2_object).unwrap();
    assert!(matches!(
      decode_index_workspace_runtime_batch_payload(v2.payload, algorithm).unwrap(),
      IndexWorkspaceRuntimeBatchPayload::V2(_)
    ));
  }
}

#[test]
fn runtime_payload_dispatch_rejects_unknown_versions() {
  let mut payload = fixture_payload("blake3-256");
  payload[4..6].copy_from_slice(&3u16.to_le_bytes());
  assert!(decode_index_workspace_runtime_batch_payload(&payload, HashAlgorithm::Blake3_256).is_err());
}

#[test]
fn airb_v2_uses_the_exact_hash_profile_and_accepts_transition_only_batches() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (_, transitions) = fixture_inputs(algorithm);
    let encoded = encode_index_workspace_runtime_batch_payload_v2(&IndexWorkspaceRuntimeBatchWriteV2 {
      hash_algorithm: algorithm,
      coordinator_id: [0x77; 16],
      batch_id: 3,
      reason: IndexFlushReasonV1::Explicit,
      mutations: &[],
      transitions: &transitions,
    })
    .unwrap();
    let decoded = decode_index_workspace_runtime_batch_payload_v2(&encoded, algorithm).unwrap();
    assert!(decoded.mutations.is_empty());
    assert_eq!(decoded.transitions.len(), 3);
  }
}

#[test]
fn airb_v2_rejects_remove_existing_for_a_valid_non_reverse_record() {
  let algorithm = HashAlgorithm::Blake3_256;
  let owner = [0x21; 32];
  let path = "/docs/file.txt";
  let file_key = digest_parts(algorithm, &[b"file:", path.as_bytes()]);
  let revision = digest_parts(algorithm, &[b"revision"]);
  let record = encode_scope_document_record(
    &ScopeDocumentRecordV1 { tombstone: false, document_ordinal: 3, file_key: &file_key, record_revision_hash: &revision, path },
    algorithm,
  )
  .unwrap();
  let order_key = 3u64.to_le_bytes();
  let mutation = IndexWorkspaceRuntimeMutationWriteV2 {
    index_id: &owner,
    role: OrderedIndexRoleV1::ScopeOrdinal,
    operation: IndexWorkspaceMutationOperationV2::RemoveExisting,
    publication_sequence: 41,
    operation_id: [0x12; 16],
    order_key: &order_key,
    encoded_record: &record,
  };
  let transition = transition(&owner, IndexWorkspaceOwnerClassV2::ScopeCatalog, [0x12; 16], true, false, false, false);
  assert!(encode_index_workspace_runtime_batch_payload_v2(&IndexWorkspaceRuntimeBatchWriteV2 {
    hash_algorithm: algorithm,
    coordinator_id: [0x77; 16],
    batch_id: 4,
    reason: IndexFlushReasonV1::Explicit,
    mutations: &[mutation],
    transitions: &[transition],
  })
  .is_err());
}

#[test]
fn airb_v2_rejects_truncation_unknown_operations_and_illegal_membership_states() {
  let valid = fixture_payload("blake3-256");
  for cut in 0..valid.len() {
    assert!(decode_index_workspace_runtime_batch_payload_v2(&valid[..cut], HashAlgorithm::Blake3_256).is_err(), "accepted cut {cut}");
  }

  let mut unknown_operation = valid.clone();
  unknown_operation[69] = 0xff;
  assert!(decode_index_workspace_runtime_batch_payload_v2(&unknown_operation, HashAlgorithm::Blake3_256).is_err());

  let second_mutation = next_frame(&valid, 64);
  let transitions_start = next_frame(&valid, second_mutation);
  let mut scope_unindexable = valid.clone();
  scope_unindexable[transitions_start + 5] = 0b0101;
  assert!(decode_index_workspace_runtime_batch_payload_v2(&scope_unindexable, HashAlgorithm::Blake3_256).is_err());

  let second_transition = next_frame(&valid, transitions_start);
  let mut live_and_unindexable = valid.clone();
  live_and_unindexable[second_transition + 5] = 0b0101;
  assert!(decode_index_workspace_runtime_batch_payload_v2(&live_and_unindexable, HashAlgorithm::Blake3_256).is_err());

  let mut nonzero_reserved = valid;
  nonzero_reserved[52] = 1;
  assert!(decode_index_workspace_runtime_batch_payload_v2(&nonzero_reserved, HashAlgorithm::Blake3_256).is_err());
}

#[test]
fn airb_v2_rejects_unbound_mutations_bad_transition_order_and_future_transition_sequences() {
  let valid = fixture_payload("blake3-256");
  let second_mutation = next_frame(&valid, 64);
  let transitions_start = next_frame(&valid, second_mutation);

  let mut unbound = valid.clone();
  let first_owner = 64 + 40;
  unbound[first_owner] = 0x24;
  assert!(decode_index_workspace_runtime_batch_payload_v2(&unbound, HashAlgorithm::Blake3_256).is_err());

  let second_transition = next_frame(&valid, transitions_start);
  let mut duplicate_transition = valid.clone();
  let first_transition = valid[transitions_start..second_transition].to_vec();
  duplicate_transition[second_transition..second_transition + first_transition.len()].copy_from_slice(&first_transition);
  assert!(decode_index_workspace_runtime_batch_payload_v2(&duplicate_transition, HashAlgorithm::Blake3_256).is_err());

  let mut older_transition = valid;
  older_transition[transitions_start + 8..transitions_start + 16].copy_from_slice(&39u64.to_le_bytes());
  assert!(decode_index_workspace_runtime_batch_payload_v2(&older_transition, HashAlgorithm::Blake3_256).is_err());

  let mut wrong_owner_class = fixture_payload("blake3-256");
  wrong_owner_class[transitions_start + 4] = IndexWorkspaceOwnerClassV2::ValueStore.id();
  assert!(decode_index_workspace_runtime_batch_payload_v2(&wrong_owner_class, HashAlgorithm::Blake3_256).is_err());

  let mut unknown_flags = fixture_payload("blake3-256");
  unknown_flags[transitions_start + 5] |= 0x80;
  assert!(decode_index_workspace_runtime_batch_payload_v2(&unknown_flags, HashAlgorithm::Blake3_256).is_err());

  let mut amplified = fixture_payload("blake3-256");
  amplified[44..48].copy_from_slice(&u32::MAX.to_le_bytes());
  assert!(decode_index_workspace_runtime_batch_payload_v2(&amplified, HashAlgorithm::Blake3_256).is_err());

  let mut no_transitions = fixture_payload("blake3-256");
  no_transitions[48..52].copy_from_slice(&0u32.to_le_bytes());
  assert!(decode_index_workspace_runtime_batch_payload_v2(&no_transitions, HashAlgorithm::Blake3_256).is_err());
}

fn fixture_inputs(
  algorithm: HashAlgorithm,
) -> (Vec<IndexWorkspaceRuntimeMutationWriteV2<'static>>, Vec<IndexWorkspaceMembershipTransitionWriteV2<'static>>) {
  let width = algorithm.hash_length();
  let owner = leaked(vec![0x21; width]);
  let first_key = leaked(vec![0x31; width]);
  let second_key = leaked(vec![0x32; width]);
  let first_record =
    leaked(encode_scope_reverse_record(&ScopeReverseRecordV1 { document_ordinal: 3, file_key: first_key }, algorithm).unwrap());
  let second_record =
    leaked(encode_scope_reverse_record(&ScopeReverseRecordV1 { document_ordinal: 3, file_key: second_key }, algorithm).unwrap());
  let records = vec![
    IndexWorkspaceRuntimeMutationWriteV2 {
      index_id: owner,
      role: OrderedIndexRoleV1::ScopeReverse,
      operation: IndexWorkspaceMutationOperationV2::RemoveExisting,
      publication_sequence: 40,
      operation_id: [0x11; 16],
      order_key: first_key,
      encoded_record: first_record,
    },
    IndexWorkspaceRuntimeMutationWriteV2 {
      index_id: owner,
      role: OrderedIndexRoleV1::ScopeReverse,
      operation: IndexWorkspaceMutationOperationV2::Upsert,
      publication_sequence: 41,
      operation_id: [0x12; 16],
      order_key: second_key,
      encoded_record: second_record,
    },
  ];
  let transitions = vec![
    transition(owner, IndexWorkspaceOwnerClassV2::ScopeCatalog, [0x12; 16], true, true, false, false),
    transition(leaked(vec![0x22; width]), IndexWorkspaceOwnerClassV2::ValueStore, [0x22; 16], false, true, false, false),
    transition(leaked(vec![0x23; width]), IndexWorkspaceOwnerClassV2::FieldIndex, [0x23; 16], false, true, true, false),
  ];
  (records, transitions)
}

fn transition<'a>(
  owner_id: &'a [u8],
  owner_class: IndexWorkspaceOwnerClassV2,
  operation_id: [u8; 16],
  before_live: bool,
  after_live: bool,
  before_unindexable: bool,
  after_unindexable: bool,
) -> IndexWorkspaceMembershipTransitionWriteV2<'a> {
  IndexWorkspaceMembershipTransitionWriteV2 {
    owner_id,
    owner_class,
    publication_sequence: 41,
    operation_id,
    document_ordinal: 3,
    before: IndexWorkspaceMembershipStateV2 { live: before_live, unindexable: before_unindexable },
    after: IndexWorkspaceMembershipStateV2 { live: after_live, unindexable: after_unindexable },
  }
}

fn fixture_payload(profile: &str) -> Vec<u8> {
  let object = fixture_object(profile, "runtime-batch-v2-valid");
  object[OBJECT_HEADER_LENGTH..object.len() - 4].to_vec()
}

fn fixture_object(profile: &str, name: &str) -> Vec<u8> {
  let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("spec/fixtures/v4/index-runtime-workspace-object-v1")
    .join(format!("aiwo-{profile}-{name}.bin"));
  fs::read(path).unwrap()
}

fn next_frame(bytes: &[u8], start: usize) -> usize {
  start + u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap()) as usize
}

fn leaked(bytes: Vec<u8>) -> &'static [u8] {
  Box::leak(bytes.into_boxed_slice())
}
