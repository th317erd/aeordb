use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::index_producer_admission::{
  IndexProducerJournalAdmissionErrorV1, IndexProducerMaintenanceAdmissionErrorV1, IndexProducerMaintenanceIntentV1,
  IndexProducerMaintenanceClassV1, admit_mutation_journal_tasks, build_maintenance_task, derive_implicit_maintenance_source_operation_id,
  derive_mutation_operation_id,
};
use aeordb::engine::v4::index_producer_coordinator::{
  IndexProducerCoordinatorOptionsV1, IndexProducerCoordinatorV1, IndexProducerSpillErrorV1, IndexProducerSpillReasonV1,
  IndexProducerSpillReceiptV1, IndexProducerSpillStoreV1, IndexProducerTaskKindV1, IndexProducerTaskViewV1,
};
use aeordb::engine::v4::index_task::{
  JournalOwnerKindV1, MutationJournalWriteV1, MutationKindV1, MutationRecordWriteV1, MutationSideWriteV1, decode_mutation_journal,
  encode_mutation_journal,
};

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

fn hash(label: &[u8]) -> Vec<u8> {
  aeordb::engine::v4::hash::digest_parts(ALGORITHM, &[b"producer-admission:", label])
}

fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(6 * 1_024 * 1_024, 8 * 1_024 * 1_024, 1, 1 * 1_024 * 1_024).unwrap())
}

fn producer(max_pending_tasks: u32) -> IndexProducerCoordinatorV1 {
  IndexProducerCoordinatorV1::new(
    ALGORITHM,
    memory(),
    IndexProducerCoordinatorOptionsV1::new(max_pending_tasks, 2 * 1_024 * 1_024, 3, 10, 1_000, 16, 256, 2 * 1_024 * 1_024).unwrap(),
  )
  .unwrap()
}

fn encoded_journal(discontinuous: bool) -> Vec<u8> {
  let root_a = hash(b"root-a");
  let root_b = hash(b"root-b");
  let root_c = hash(b"root-c");
  let root_d = hash(b"root-d");
  let semantic = hash(b"semantic");
  let mutation_a = hash(b"mutation-a");
  let mutation_b = hash(b"mutation-b");
  let revision_a = hash(b"revision-a");
  let revision_b = hash(b"revision-b");
  let records = if discontinuous {
    vec![
      MutationRecordWriteV1 {
        kind: MutationKindV1::Create,
        sequence: 7,
        mutation_id: &mutation_a,
        batch_ordinal: 0,
        batch_count: 1,
        root_before: &root_a,
        root_after: &root_b,
        before: None,
        after: Some(MutationSideWriteV1 { path: "/a.json", revision: &revision_a }),
        committed_at_ms: 100,
      },
      MutationRecordWriteV1 {
        kind: MutationKindV1::Create,
        sequence: 8,
        mutation_id: &mutation_b,
        batch_ordinal: 0,
        batch_count: 1,
        root_before: &root_c,
        root_after: &root_d,
        before: None,
        after: Some(MutationSideWriteV1 { path: "/b.json", revision: &revision_b }),
        committed_at_ms: 101,
      },
    ]
  } else {
    vec![
      MutationRecordWriteV1 {
        kind: MutationKindV1::Create,
        sequence: 7,
        mutation_id: &mutation_a,
        batch_ordinal: 0,
        batch_count: 2,
        root_before: &root_a,
        root_after: &root_b,
        before: None,
        after: Some(MutationSideWriteV1 { path: "/a.json", revision: &revision_a }),
        committed_at_ms: 100,
      },
      MutationRecordWriteV1 {
        kind: MutationKindV1::Create,
        sequence: 7,
        mutation_id: &mutation_a,
        batch_ordinal: 1,
        batch_count: 2,
        root_before: &root_a,
        root_after: &root_b,
        before: None,
        after: Some(MutationSideWriteV1 { path: "/b.json", revision: &revision_b }),
        committed_at_ms: 100,
      },
    ]
  };
  encode_mutation_journal(&MutationJournalWriteV1 {
    hash_algorithm: ALGORITHM,
    owner_id: [0x55; 16],
    owner_kind: JournalOwnerKindV1::Task,
    generation: 1,
    segment_ordinal: 0,
    chain_reset: true,
    previous_segment: &[0; 32],
    semantic_state_root: &semantic,
    runtime_boot_id: [0x66; 16],
    records: &records,
  })
  .unwrap()
  .value
}

#[derive(Default)]
struct SpillStore {
  tasks: Vec<([u8; 16], IndexProducerSpillReasonV1)>,
}

impl IndexProducerSpillStoreV1 for SpillStore {
  fn spill(
    &mut self,
    task: IndexProducerTaskViewV1<'_>,
    reason: IndexProducerSpillReasonV1,
  ) -> Result<IndexProducerSpillReceiptV1, IndexProducerSpillErrorV1> {
    self.tasks.push((task.operation_id(), reason));
    IndexProducerSpillReceiptV1::new(task.operation_id(), hash(b"spill"))
  }
}

#[test]
fn exact_journal_admits_one_deterministic_body_free_task_per_source_record() {
  let encoded = encoded_journal(false);
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let records = journal.records.iter().collect::<Result<Vec<_>, _>>().unwrap();
  let mut producer = producer(8);
  let mut spill = SpillStore::default();

  let summary = admit_mutation_journal_tasks(ALGORITHM, &mut producer, &journal, 200, &|| false, &mut spill).unwrap();

  assert_eq!(summary.queued, 2);
  assert_eq!(summary.duplicates, 0);
  assert_eq!(summary.spilled, 0);
  assert_eq!(producer.snapshot().pending_tasks, 2);
  assert!(spill.tasks.is_empty());
  let first = derive_mutation_operation_id(ALGORITHM, records[0].mutation_id, records[0].batch_ordinal).unwrap();
  let second = derive_mutation_operation_id(ALGORITHM, records[1].mutation_id, records[1].batch_ordinal).unwrap();
  assert_ne!(first, second);

  let lease = producer.lease_next(200, false).unwrap().unwrap();
  let task = producer.leased_task(&lease).unwrap();
  assert_eq!(task.kind(), IndexProducerTaskKindV1::MutationWindow);
  assert_eq!(task.publication_sequence(), 7);
  assert_eq!(task.namespace_root_before(), journal.source_root_before);
  assert_eq!(task.namespace_root_after(), journal.source_root_after);
  assert_eq!(task.semantic_state_root(), journal.semantic_state_root);
  assert_eq!(task.journal_head(), Some(journal.key.as_slice()));
}

#[test]
fn retrying_the_same_journal_is_idempotent() {
  let encoded = encoded_journal(false);
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let mut producer = producer(8);
  let mut spill = SpillStore::default();

  let first = admit_mutation_journal_tasks(ALGORITHM, &mut producer, &journal, 200, &|| false, &mut spill).unwrap();
  let second = admit_mutation_journal_tasks(ALGORITHM, &mut producer, &journal, 200, &|| false, &mut spill).unwrap();

  assert_eq!((first.queued, first.duplicates), (2, 0));
  assert_eq!((second.queued, second.duplicates), (0, 2));
  assert_eq!(producer.snapshot().pending_tasks, 2);
}

#[test]
fn pressure_spills_each_unretained_task_through_the_same_boundary() {
  let encoded = encoded_journal(false);
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let mut producer = producer(1);
  let mut spill = SpillStore::default();

  let summary = admit_mutation_journal_tasks(ALGORITHM, &mut producer, &journal, 200, &|| false, &mut spill).unwrap();

  assert_eq!((summary.queued, summary.duplicates, summary.spilled), (1, 0, 1));
  assert_eq!(producer.snapshot().pending_tasks, 1);
  assert_eq!(spill.tasks.len(), 1);
  assert_eq!(spill.tasks[0].1, IndexProducerSpillReasonV1::AdmissionPressure);
}

#[test]
fn discontinuous_journal_fails_before_any_task_is_admitted() {
  let encoded = encoded_journal(true);
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let mut producer = producer(8);
  let mut spill = SpillStore::default();

  let error = admit_mutation_journal_tasks(ALGORITHM, &mut producer, &journal, 200, &|| false, &mut spill).unwrap_err();

  assert!(matches!(error, IndexProducerJournalAdmissionErrorV1::DiscontinuousRoots { .. }));
  assert_eq!(producer.snapshot().pending_tasks, 0);
  assert!(spill.tasks.is_empty());
}

#[test]
fn cancellation_is_checked_before_validation_and_between_task_admissions() {
  use std::cell::Cell;

  let encoded = encoded_journal(false);
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let mut producer = producer(8);
  let mut spill = SpillStore::default();
  let error = admit_mutation_journal_tasks(ALGORITHM, &mut producer, &journal, 200, &|| true, &mut spill).unwrap_err();
  assert_eq!(error, IndexProducerJournalAdmissionErrorV1::Cancelled);
  assert_eq!(producer.snapshot().pending_tasks, 0);

  let checks = Cell::new(0u32);
  let cancel_during_admission = || {
    let observed = checks.get();
    checks.set(observed + 1);
    observed >= 4
  };
  let error = admit_mutation_journal_tasks(ALGORITHM, &mut producer, &journal, 200, &cancel_during_admission, &mut spill).unwrap_err();
  assert_eq!(error, IndexProducerJournalAdmissionErrorV1::Cancelled);
  assert_eq!(producer.snapshot().pending_tasks, 1);

  let resumed = admit_mutation_journal_tasks(ALGORITHM, &mut producer, &journal, 200, &|| false, &mut spill).unwrap();
  assert_eq!((resumed.queued, resumed.duplicates), (1, 1));
  assert_eq!(producer.snapshot().pending_tasks, 2);
}

#[test]
fn operation_identity_rejects_wrong_width_and_supports_every_hash_profile() {
  assert!(matches!(
    derive_mutation_operation_id(ALGORITHM, &[0x11; 31], 0),
    Err(IndexProducerJournalAdmissionErrorV1::InvalidMutationIdentity { .. })
  ));
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let identity = vec![0x11; algorithm.hash_length()];
    let first = derive_mutation_operation_id(algorithm, &identity, 0).unwrap();
    let second = derive_mutation_operation_id(algorithm, &identity, 1).unwrap();
    assert_ne!(first, [0; 16]);
    assert_ne!(first, second);
  }
}

#[test]
fn maintenance_intent_builds_one_root_pinned_body_free_retry_stable_task() {
  let root = hash(b"maintenance-root");
  let semantic = hash(b"maintenance-semantic");
  let intent = IndexProducerMaintenanceIntentV1 {
    source_operation_id: [0x71; 16],
    class: IndexProducerMaintenanceClassV1::Reindex,
    publication_sequence: 42,
    namespace_root: &root,
    semantic_state_root: &semantic,
    scope: "/docs",
  };

  let first = build_maintenance_task(ALGORITHM, intent).unwrap();
  let retry = build_maintenance_task(ALGORITHM, intent).unwrap();
  assert_eq!(first.operation_id, retry.operation_id);
  assert_ne!(first.operation_id, [0; 16]);
  assert_eq!(first.kind, IndexProducerTaskKindV1::Rebuild);
  assert_eq!(first.publication_sequence, 42);
  assert_eq!(first.namespace_root_before, root);
  assert_eq!(first.namespace_root_after, root);
  assert_eq!(first.semantic_state_root, semantic);
  assert_eq!(first.journal_head, None);
  assert_eq!(first.scope, Some("/docs"));
}

#[test]
fn maintenance_identity_separates_every_authority_dimension() {
  let root = hash(b"maintenance-root");
  let other_root = hash(b"maintenance-root-other");
  let semantic = hash(b"maintenance-semantic");
  let other_semantic = hash(b"maintenance-semantic-other");
  let base = IndexProducerMaintenanceIntentV1 {
    source_operation_id: [0x72; 16],
    class: IndexProducerMaintenanceClassV1::Repair,
    publication_sequence: 42,
    namespace_root: &root,
    semantic_state_root: &semantic,
    scope: "/docs",
  };
  let base_id = build_maintenance_task(ALGORITHM, base).unwrap().operation_id;
  for changed in [
    IndexProducerMaintenanceIntentV1 { source_operation_id: [0x73; 16], ..base },
    IndexProducerMaintenanceIntentV1 { class: IndexProducerMaintenanceClassV1::Reindex, ..base },
    IndexProducerMaintenanceIntentV1 { publication_sequence: 43, ..base },
    IndexProducerMaintenanceIntentV1 { namespace_root: &other_root, ..base },
    IndexProducerMaintenanceIntentV1 { semantic_state_root: &other_semantic, ..base },
    IndexProducerMaintenanceIntentV1 { scope: "/docs/sub", ..base },
  ] {
    assert_ne!(build_maintenance_task(ALGORITHM, changed).unwrap().operation_id, base_id);
  }
}

#[test]
fn implicit_maintenance_source_identity_is_retry_stable_and_separates_class_scope_and_hash_profile() {
  let mut identities = Vec::new();
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let first = derive_implicit_maintenance_source_operation_id(algorithm, IndexProducerMaintenanceClassV1::Repair, "/docs").unwrap();
    let retry = derive_implicit_maintenance_source_operation_id(algorithm, IndexProducerMaintenanceClassV1::Repair, "/docs").unwrap();
    assert_eq!(first, retry);
    assert_ne!(first, [0; 16]);
    assert_ne!(
      first,
      derive_implicit_maintenance_source_operation_id(algorithm, IndexProducerMaintenanceClassV1::Reindex, "/docs").unwrap()
    );
    assert_ne!(
      first,
      derive_implicit_maintenance_source_operation_id(algorithm, IndexProducerMaintenanceClassV1::Repair, "/docs/sub").unwrap()
    );
    identities.push(first);
  }
  identities.sort();
  identities.dedup();
  assert_eq!(identities.len(), 5);
  assert!(derive_implicit_maintenance_source_operation_id(ALGORITHM, IndexProducerMaintenanceClassV1::Repair, "docs").is_err());
  assert!(derive_implicit_maintenance_source_operation_id(ALGORITHM, IndexProducerMaintenanceClassV1::Repair, "/docs/../private").is_err());
}

#[test]
fn maintenance_intent_rejects_malformed_authority_before_admission() {
  let root = hash(b"maintenance-root");
  let semantic = hash(b"maintenance-semantic");
  let valid = IndexProducerMaintenanceIntentV1 {
    source_operation_id: [0x74; 16],
    class: IndexProducerMaintenanceClassV1::ConfigurationRetirement,
    publication_sequence: 42,
    namespace_root: &root,
    semantic_state_root: &semantic,
    scope: "/docs",
  };
  let malformed = [
    IndexProducerMaintenanceIntentV1 { source_operation_id: [0; 16], ..valid },
    IndexProducerMaintenanceIntentV1 { publication_sequence: 0, ..valid },
    IndexProducerMaintenanceIntentV1 { namespace_root: &[0x11; 31], ..valid },
    IndexProducerMaintenanceIntentV1 { namespace_root: &[0; 32], ..valid },
    IndexProducerMaintenanceIntentV1 { semantic_state_root: &[0x11; 31], ..valid },
    IndexProducerMaintenanceIntentV1 { semantic_state_root: &[0; 32], ..valid },
    IndexProducerMaintenanceIntentV1 { scope: "docs", ..valid },
    IndexProducerMaintenanceIntentV1 { scope: "/docs/../private", ..valid },
  ];
  for intent in malformed {
    assert!(matches!(build_maintenance_task(ALGORITHM, intent), Err(IndexProducerMaintenanceAdmissionErrorV1::Invalid(_))));
  }
}

#[test]
fn maintenance_intent_supports_every_database_hash_profile() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let root = vec![0x81; algorithm.hash_length()];
    let semantic = vec![0x82; algorithm.hash_length()];
    let request = build_maintenance_task(
      algorithm,
      IndexProducerMaintenanceIntentV1 {
        source_operation_id: [0x75; 16],
        class: IndexProducerMaintenanceClassV1::LegacyMigration,
        publication_sequence: 1,
        namespace_root: &root,
        semantic_state_root: &semantic,
        scope: "/",
      },
    )
    .unwrap();
    assert_ne!(request.operation_id, [0; 16]);
  }
}

#[test]
fn maintenance_classes_freeze_every_live_producer_mapping_without_exposing_journal_kinds() {
  let expected = [
    (IndexProducerMaintenanceClassV1::DeleteCleanup, 1, IndexProducerTaskKindV1::Rebuild),
    (IndexProducerMaintenanceClassV1::ConfigurationRetirement, 2, IndexProducerTaskKindV1::Retire),
    (IndexProducerMaintenanceClassV1::Reindex, 3, IndexProducerTaskKindV1::Rebuild),
    (IndexProducerMaintenanceClassV1::Repair, 4, IndexProducerTaskKindV1::Repair),
    (IndexProducerMaintenanceClassV1::ExplicitLegacyMutation, 5, IndexProducerTaskKindV1::ExplicitMutation),
    (IndexProducerMaintenanceClassV1::LegacyMigration, 6, IndexProducerTaskKindV1::LegacyMigration),
    (IndexProducerMaintenanceClassV1::DefinitionBuild, 7, IndexProducerTaskKindV1::Build),
    (IndexProducerMaintenanceClassV1::Compaction, 8, IndexProducerTaskKindV1::Compact),
  ];
  for (class, id, kind) in expected {
    assert_eq!(class.id(), id);
    assert_eq!(class.task_kind(), kind);
    assert!(!class.task_kind().requires_journal());
  }
}
