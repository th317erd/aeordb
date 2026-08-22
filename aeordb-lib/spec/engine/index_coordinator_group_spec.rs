use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_coordinator::{
  IndexCoordinatorErrorV1, IndexCoordinatorOptionsV1, IndexCoordinatorV1, IndexFlushReasonV1, IndexGroupMutationRequestV1,
  IndexMembershipOwnerClassV1, IndexMembershipStateV1, IndexMembershipTransitionRequestV1, IndexMutationGroupRequestV1,
  IndexMutationOperationV1, IndexMutationRequestV1,
};
use aeordb::engine::v4::index_page::OrderedIndexRoleV1;
use aeordb::engine::v4::index_record::{ScopeReverseRecordV1, encode_scope_reverse_record};

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

#[test]
fn semantic_group_freezes_retries_and_publishes_as_one_indivisible_unit() {
  let memory = memory(200_000);
  let mut coordinator = coordinator(memory.clone(), 20_000);
  let owner = owner(b"scope");
  let first = reverse_record(3, b"first");
  let second = reverse_record(3, b"second");
  let mutations = [
    grouped(&owner, 11, 0x11, &first, IndexMutationOperationV1::RemoveExisting),
    grouped(&owner, 11, 0x11, &second, IndexMutationOperationV1::Upsert),
  ];
  let transition = transition(&owner, 3, 11, 0x11, state(true, false), state(true, false));

  coordinator.admit_group(IndexMutationGroupRequestV1 { transition, mutations: &mutations }, 1_001).unwrap();
  let batch = coordinator.begin_flush(1_002, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
  assert_eq!(batch.records().len(), 2);
  assert_eq!(batch.transitions().len(), 1);
  assert!(batch.records().iter().any(|record| record.operation() == IndexMutationOperationV1::RemoveExisting));
  assert!(batch.records().iter().any(|record| record.operation() == IndexMutationOperationV1::Upsert));
  assert_eq!(batch.transitions()[0].before(), state(true, false));
  assert_eq!(batch.transitions()[0].after(), state(true, false));

  let retry = coordinator.retry_frozen(false).unwrap();
  assert_eq!(retry.records(), batch.records());
  assert_eq!(retry.transitions(), batch.transitions());
  assert!(matches!(coordinator.complete_success(&batch), Err(IndexCoordinatorErrorV1::StaleBatch)));
  coordinator.complete_success(&retry).unwrap();
  drop(batch);
  drop(retry);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes, 0);
}

#[test]
fn publication_limit_and_pressure_refuse_the_whole_group_without_partial_state() {
  let owner = owner(b"scope");
  let first = reverse_record(3, b"first");
  let second = reverse_record(3, b"second");
  let mutations = [
    grouped(&owner, 10, 0x10, &first, IndexMutationOperationV1::Upsert),
    grouped(&owner, 10, 0x10, &second, IndexMutationOperationV1::Upsert),
  ];
  let transition = transition(&owner, 3, 10, 0x10, state(false, false), state(true, false));

  for (memory_limit, publication_limit) in [(200_000, 1), (300, 20_000)] {
    let memory = memory(memory_limit);
    let mut coordinator = coordinator(memory.clone(), publication_limit);
    assert!(matches!(
      coordinator.admit_group(IndexMutationGroupRequestV1 { transition, mutations: &mutations }, 1_001),
      Err(IndexCoordinatorErrorV1::SpillRequired { .. })
    ));
    let snapshot = coordinator.snapshot();
    assert_eq!(snapshot.active_records, 0);
    assert_eq!(snapshot.active_groups, 0);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes, 0);
  }
}

#[test]
fn failed_flush_composes_frozen_and_newer_active_group_state() {
  let memory = memory(300_000);
  let mut coordinator = coordinator(memory.clone(), 30_000);
  let owner = owner(b"scope");
  let first = reverse_record(3, b"first");
  let second = reverse_record(3, b"second");
  let initial = [grouped(&owner, 10, 0x10, &first, IndexMutationOperationV1::Upsert)];
  coordinator
    .admit_group(
      IndexMutationGroupRequestV1 {
        transition: transition(&owner, 3, 10, 0x10, state(false, false), state(true, false)),
        mutations: &initial,
      },
      1_001,
    )
    .unwrap();
  let frozen = coordinator.begin_flush(1_002, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();

  let successor = [
    grouped(&owner, 11, 0x11, &first, IndexMutationOperationV1::RemoveExisting),
    grouped(&owner, 11, 0x11, &second, IndexMutationOperationV1::Upsert),
  ];
  coordinator
    .admit_group(
      IndexMutationGroupRequestV1 {
        transition: transition(&owner, 3, 11, 0x11, state(true, false), state(true, false)),
        mutations: &successor,
      },
      1_003,
    )
    .unwrap();
  let successor_bytes = coordinator.snapshot().active_bytes;
  coordinator.complete_failure(&frozen, 1_004).unwrap();
  drop(frozen);
  assert_eq!(
    coordinator.snapshot().active_bytes,
    successor_bytes + std::mem::size_of::<aeordb::engine::memory_coordinator::MemoryReservation>() as u64,
    "failed-flush composition retained bytes for a superseded mutation or duplicate group shell"
  );

  let restored = coordinator.begin_flush(1_005, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
  assert_eq!(restored.records().len(), 2);
  assert_eq!(restored.transitions().len(), 1);
  assert_eq!(restored.transitions()[0].before(), state(false, false));
  assert_eq!(restored.transitions()[0].after(), state(true, false));
  assert_eq!(restored.transitions()[0].publication_sequence(), 11);
  assert!(restored.records().iter().any(|record| record.operation() == IndexMutationOperationV1::RemoveExisting));
}

#[test]
fn same_footprint_group_replacement_uses_the_final_buffer_bound_not_clone_headroom() {
  let owner = owner(b"scope");
  let encoded = reverse_record(3, b"first");
  let initial_mutations = [grouped(&owner, 10, 0x10, &encoded, IndexMutationOperationV1::Upsert)];
  let generous_memory = memory(200_000);
  let mut probe = coordinator(generous_memory, 20_000);
  probe
    .admit_group(
      IndexMutationGroupRequestV1 {
        transition: transition(&owner, 3, 10, 0x10, state(false, false), state(true, false)),
        mutations: &initial_mutations,
      },
      1_001,
    )
    .unwrap();
  let exact_final_bytes = probe.snapshot().active_bytes;

  let memory = memory(200_000);
  let mut coordinator = coordinator_with_buffer(memory, 20_000, exact_final_bytes);
  coordinator
    .admit_group(
      IndexMutationGroupRequestV1 {
        transition: transition(&owner, 3, 10, 0x10, state(false, false), state(true, false)),
        mutations: &initial_mutations,
      },
      1_001,
    )
    .unwrap();
  let replacement_mutations = [grouped(&owner, 11, 0x11, &encoded, IndexMutationOperationV1::Upsert)];
  coordinator
    .admit_group(
      IndexMutationGroupRequestV1 {
        transition: transition(&owner, 3, 11, 0x11, state(true, false), state(true, false)),
        mutations: &replacement_mutations,
      },
      1_002,
    )
    .unwrap();
  assert_eq!(coordinator.snapshot().active_bytes, exact_final_bytes);
}

#[test]
fn transition_chain_mismatch_and_mixed_legacy_state_fail_without_changes() {
  let memory = memory(200_000);
  let mut grouped_coordinator = coordinator(memory.clone(), 20_000);
  let owner = owner(b"scope");
  let encoded = reverse_record(3, b"first");
  let mutations = [grouped(&owner, 10, 0x10, &encoded, IndexMutationOperationV1::Upsert)];
  let request = IndexMutationGroupRequestV1 {
    transition: transition(&owner, 3, 10, 0x10, state(false, false), state(true, false)),
    mutations: &mutations,
  };
  grouped_coordinator.admit_group(request, 1_001).unwrap();
  let before = grouped_coordinator.snapshot();
  let mismatch =
    IndexMutationGroupRequestV1 { transition: transition(&owner, 3, 11, 0x11, state(false, false), state(true, false)), mutations: &[] };
  assert!(matches!(grouped_coordinator.admit_group(mismatch, 1_002), Err(IndexCoordinatorErrorV1::ConflictingGroupTransition { .. })));
  assert_eq!(grouped_coordinator.snapshot(), before);
  assert!(matches!(grouped_coordinator.admit(legacy(&owner, 12, 0x12, &encoded), 1_003), Err(IndexCoordinatorErrorV1::MixedAdmissionMode)));

  let mut legacy_coordinator = coordinator(memory, 20_000);
  legacy_coordinator.admit(legacy(&owner, 10, 0x10, &encoded), 1_001).unwrap();
  assert!(matches!(
    legacy_coordinator.admit_group(
      IndexMutationGroupRequestV1 { transition: transition(&owner, 3, 11, 0x11, state(true, false), state(true, false)), mutations: &[] },
      1_002,
    ),
    Err(IndexCoordinatorErrorV1::MixedAdmissionMode)
  ));
}

#[test]
fn transition_only_group_is_durable_work_and_duplicate_delivery_is_stable() {
  let memory = memory(100_000);
  let mut coordinator = coordinator(memory, 20_000);
  let owner = owner(b"value");
  let request = IndexMutationGroupRequestV1 {
    transition: IndexMembershipTransitionRequestV1 {
      owner_id: &owner,
      owner_class: IndexMembershipOwnerClassV1::ValueStore,
      publication_sequence: 9,
      operation_id: [9; 16],
      document_ordinal: 5,
      before: state(false, false),
      after: state(false, true),
    },
    mutations: &[],
  };
  coordinator.admit_group(request, 1_001).unwrap();
  let before = coordinator.snapshot();
  assert!(coordinator.admit_group(request, 1_002).unwrap().is_duplicate());
  assert_eq!(coordinator.snapshot(), before);
  let batch = coordinator.begin_flush(1_003, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
  assert!(batch.records().is_empty());
  assert_eq!(batch.transitions().len(), 1);
}

#[test]
fn duplicate_delivery_requires_the_exact_complete_semantic_group() {
  let memory = memory(100_000);
  let mut coordinator = coordinator(memory, 20_000);
  let owner = owner(b"scope");
  let first = reverse_record(5, b"first");
  let second = reverse_record(5, b"second");
  let mutations = [
    grouped(&owner, 9, 9, &first, IndexMutationOperationV1::RemoveExisting),
    grouped(&owner, 9, 9, &second, IndexMutationOperationV1::Upsert),
  ];
  let transition = transition(&owner, 5, 9, 9, state(true, false), state(true, false));
  coordinator.admit_group(IndexMutationGroupRequestV1 { transition, mutations: &mutations }, 1_001).unwrap();
  let before = coordinator.snapshot();

  assert!(matches!(
    coordinator.admit_group(IndexMutationGroupRequestV1 { transition, mutations: &mutations[..1] }, 1_002),
    Err(IndexCoordinatorErrorV1::ConflictingGroupTransition { publication_sequence: 9 })
  ));
  let duplicated_key = [mutations[0], mutations[0]];
  assert!(matches!(
    coordinator.admit_group(IndexMutationGroupRequestV1 { transition, mutations: &duplicated_key }, 1_003),
    Err(IndexCoordinatorErrorV1::ConflictingGroupTransition { publication_sequence: 9 })
  ));
  assert_eq!(coordinator.snapshot(), before);
}

fn coordinator(memory: MemoryCoordinator, publication_batch_max_bytes: u64) -> IndexCoordinatorV1 {
  coordinator_with_buffer(memory, publication_batch_max_bytes, 100_000)
}

fn coordinator_with_buffer(
  memory: MemoryCoordinator,
  publication_batch_max_bytes: u64,
  mutation_buffer_max_bytes: u64,
) -> IndexCoordinatorV1 {
  IndexCoordinatorV1::new(
    [7; 16],
    ALGORITHM,
    memory,
    IndexCoordinatorOptionsV1::new(mutation_buffer_max_bytes, 1, 1_000, publication_batch_max_bytes).unwrap(),
    1_000,
  )
  .unwrap()
}

fn memory(hard_limit_bytes: u64) -> MemoryCoordinator {
  let emergency = (hard_limit_bytes / 3).max(1);
  MemoryCoordinator::new(MemoryPolicy::new(hard_limit_bytes - emergency - 1, hard_limit_bytes, 1, emergency).unwrap())
}

fn owner(label: &[u8]) -> Vec<u8> {
  digest_parts(ALGORITHM, &[b"owner:", label])
}

fn reverse_record(document_ordinal: u64, label: &[u8]) -> Vec<u8> {
  let file_key = digest_parts(ALGORITHM, &[b"file:", label]);
  encode_scope_reverse_record(&ScopeReverseRecordV1 { document_ordinal, file_key: &file_key }, ALGORITHM).unwrap()
}

fn grouped<'a>(
  owner_id: &'a [u8],
  publication_sequence: u64,
  operation_byte: u8,
  encoded_record: &'a [u8],
  operation: IndexMutationOperationV1,
) -> IndexGroupMutationRequestV1<'a> {
  IndexGroupMutationRequestV1 { operation, mutation: legacy(owner_id, publication_sequence, operation_byte, encoded_record) }
}

fn legacy<'a>(owner_id: &'a [u8], publication_sequence: u64, operation_byte: u8, encoded_record: &'a [u8]) -> IndexMutationRequestV1<'a> {
  IndexMutationRequestV1 {
    index_id: owner_id,
    role: OrderedIndexRoleV1::ScopeReverse,
    publication_sequence,
    operation_id: [operation_byte; 16],
    encoded_record,
  }
}

fn transition<'a>(
  owner_id: &'a [u8],
  document_ordinal: u64,
  publication_sequence: u64,
  operation_byte: u8,
  before: IndexMembershipStateV1,
  after: IndexMembershipStateV1,
) -> IndexMembershipTransitionRequestV1<'a> {
  IndexMembershipTransitionRequestV1 {
    owner_id,
    owner_class: IndexMembershipOwnerClassV1::ScopeCatalog,
    publication_sequence,
    operation_id: [operation_byte; 16],
    document_ordinal,
    before,
    after,
  }
}

const fn state(live: bool, unindexable: bool) -> IndexMembershipStateV1 {
  IndexMembershipStateV1 { live, unindexable }
}
