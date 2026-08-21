use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, encode_canonical_value};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_coordinator::{IndexCoordinatorOptionsV1, IndexCoordinatorV1, IndexFlushReasonV1, IndexMutationRequestV1};
use aeordb::engine::v4::index_maintenance_scan::derive_index_maintenance_document_operation_id_v1;
use aeordb::engine::v4::index_page::{OrderedIndexRoleV1, PostingRecordV1, encode_posting_record};
use aeordb::engine::v4::index_producer_coordinator::{
  IndexProducerAdmissionV1, IndexProducerCompletionV1, IndexProducerCoordinatorErrorV1, IndexProducerCoordinatorOptionsV1,
  IndexProducerCoordinatorV1, IndexProducerDurableTaskStoreV1, IndexProducerFallbackModeV1, IndexProducerMaintenanceDocumentRequestV1,
  IndexProducerMaintenanceProgressV1, IndexProducerMutationV1, IndexProducerOwnerDispositionV1, IndexProducerOwnerOutcomeV1,
  IndexProducerReportV1, IndexProducerSpillErrorV1, IndexProducerSpillReasonV1, IndexProducerSpillReceiptV1, IndexProducerSpillStoreV1,
  IndexProducerTaskKindV1, IndexProducerTaskRequestV1, IndexProducerTaskViewV1,
};
use aeordb::engine::v4::index_record::{
  CanonicalValueRecordV1, ScopeReverseRecordV1, encode_canonical_value_record, encode_scope_reverse_record,
};

const HASH_ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

fn memory(hard_limit_bytes: u64) -> MemoryCoordinator {
  let emergency = (hard_limit_bytes / 4).max(1);
  MemoryCoordinator::new(MemoryPolicy::new(hard_limit_bytes - emergency - 1, hard_limit_bytes, 1, emergency).unwrap())
}

fn options(max_tasks: u32, max_bytes: u64, max_attempts: u16) -> IndexProducerCoordinatorOptionsV1 {
  IndexProducerCoordinatorOptionsV1::new(max_tasks, max_bytes, max_attempts, 10, 1_000, 16, 128, 64 * 1024).unwrap()
}

fn hash(label: &[u8]) -> Vec<u8> {
  digest_parts(HASH_ALGORITHM, &[b"producer:", label])
}

fn task<'a>(
  operation_id: [u8; 16],
  kind: IndexProducerTaskKindV1,
  sequence: u64,
  before: &'a [u8],
  after: &'a [u8],
  semantic: &'a [u8],
  journal: Option<&'a [u8]>,
  scope: Option<&'a str>,
) -> IndexProducerTaskRequestV1<'a> {
  IndexProducerTaskRequestV1 {
    operation_id,
    kind,
    publication_sequence: sequence,
    namespace_root_before: before,
    namespace_root_after: after,
    semantic_state_root: semantic,
    journal_head: journal,
    scope,
  }
}

fn mutation(owner_id: &[u8], ordinal: u64) -> IndexProducerMutationV1 {
  let file_key = hash(&ordinal.to_le_bytes());
  let encoded =
    encode_scope_reverse_record(&ScopeReverseRecordV1 { document_ordinal: ordinal, file_key: &file_key }, HASH_ALGORITHM).unwrap();
  IndexProducerMutationV1 { owner_id: owner_id.to_vec(), role: OrderedIndexRoleV1::ScopeReverse, encoded_record: encoded }
}

fn value_mutation(owner_id: &[u8], ordinal: u64) -> IndexProducerMutationV1 {
  let revision = hash(b"revision");
  let canonical = encode_canonical_value(&CanonicalConfigValueV1::String("value".to_string()), CanonicalValueBounds::SOURCE_VALUE).unwrap();
  let encoded = encode_canonical_value_record(
    &CanonicalValueRecordV1 {
      tombstone: false,
      document_ordinal: ordinal,
      source_value_ordinal: 0,
      record_revision_hash: &revision,
      canonical_value: Some(&canonical),
    },
    HASH_ALGORITHM,
  )
  .unwrap();
  IndexProducerMutationV1 { owner_id: owner_id.to_vec(), role: OrderedIndexRoleV1::Value, encoded_record: encoded }
}

fn posting_mutation(owner_id: &[u8], ordinal: u64) -> IndexProducerMutationV1 {
  let encoded = encode_posting_record(&PostingRecordV1 {
    tombstone: false,
    coordinate: 17,
    document_ordinal: ordinal,
    source_value_ordinal: 0,
    expansion_ordinal: 0,
    posting_key: b"posting",
  })
  .unwrap();
  IndexProducerMutationV1 { owner_id: owner_id.to_vec(), role: OrderedIndexRoleV1::Posting, encoded_record: encoded }
}

fn mutation_coordinator(memory: MemoryCoordinator) -> IndexCoordinatorV1 {
  IndexCoordinatorV1::new(
    [0x44; 16],
    HASH_ALGORITHM,
    memory,
    IndexCoordinatorOptionsV1::new(512 * 1024, 262_144, 30_000, 256 * 1024).unwrap(),
    1,
  )
  .unwrap()
}

#[derive(Default)]
struct SpillStore {
  calls: Vec<([u8; 16], IndexProducerSpillReasonV1)>,
  persisted: Vec<[u8; 16]>,
  fail: bool,
  malformed_receipt: bool,
  dishonest_receipt: bool,
}

impl IndexProducerDurableTaskStoreV1 for SpillStore {
  fn persist_task(&mut self, task: IndexProducerTaskViewV1<'_>) -> Result<IndexProducerSpillReceiptV1, IndexProducerSpillErrorV1> {
    self.persisted.push(task.operation_id());
    if self.fail {
      return Err(IndexProducerSpillErrorV1::new("persist_refused", "injected durable-task refusal"));
    }
    Ok(IndexProducerSpillReceiptV1::new(task.operation_id(), hash(b"durable-task")).unwrap())
  }
}

impl IndexProducerSpillStoreV1 for SpillStore {
  fn spill(
    &mut self,
    task: IndexProducerTaskViewV1<'_>,
    reason: IndexProducerSpillReasonV1,
  ) -> Result<IndexProducerSpillReceiptV1, IndexProducerSpillErrorV1> {
    self.calls.push((task.operation_id(), reason));
    if self.fail {
      return Err(IndexProducerSpillErrorV1::new("spill_refused", "injected spill refusal"));
    }
    if self.malformed_receipt {
      return Ok(IndexProducerSpillReceiptV1::new(task.operation_id(), vec![0x77; 31]).unwrap());
    }
    if self.dishonest_receipt {
      return Ok(IndexProducerSpillReceiptV1::new([0x55; 16], hash(b"spill")).unwrap());
    }
    Ok(IndexProducerSpillReceiptV1::new(task.operation_id(), hash(b"spill")).unwrap())
  }
}

#[test]
fn options_and_exact_source_identity_fail_closed_without_retention() {
  assert!(IndexProducerCoordinatorOptionsV1::new(0, 1, 1, 1, 1, 1, 1, 1).is_err());
  assert!(IndexProducerCoordinatorOptionsV1::new(1, 0, 1, 1, 1, 1, 1, 1).is_err());
  assert!(IndexProducerCoordinatorOptionsV1::new(1, 1, 0, 1, 1, 1, 1, 1).is_err());
  assert!(IndexProducerCoordinatorOptionsV1::new(1, 1, 1, 0, 1, 1, 1, 1).is_err());
  assert!(IndexProducerCoordinatorOptionsV1::new(1, 1, 1, 2, 1, 1, 1, 1).is_err());
  assert!(IndexProducerCoordinatorOptionsV1::new(1, 1, 1, 1, 1, 0, 1, 1).is_err());
  assert!(IndexProducerCoordinatorOptionsV1::new(1, 1, 1, 1, 1, 1, 0, 1).is_err());
  assert!(IndexProducerCoordinatorOptionsV1::new(1, 1, 1, 1, 1, 1, 1, 0).is_err());

  let memory = memory(100_000);
  let mut coordinator = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory.clone(), options(4, 20_000, 3)).unwrap();
  let before = hash(b"before");
  let after = hash(b"after");
  let semantic = hash(b"semantic");
  let journal = hash(b"journal");
  for invalid in [
    task([0; 16], IndexProducerTaskKindV1::MutationWindow, 1, &before, &after, &semantic, Some(&journal), None),
    task([1; 16], IndexProducerTaskKindV1::MutationWindow, 0, &before, &after, &semantic, Some(&journal), None),
    task([1; 16], IndexProducerTaskKindV1::MutationWindow, 1, &before[..31], &after, &semantic, Some(&journal), None),
    task([1; 16], IndexProducerTaskKindV1::MutationWindow, 1, &before, &after, &semantic, None, None),
    task([1; 16], IndexProducerTaskKindV1::Rebuild, 1, &after, &after, &semantic, Some(&journal), Some("/docs")),
    task([1; 16], IndexProducerTaskKindV1::Rebuild, 1, &after, &after, &semantic, None, None),
    task([1; 16], IndexProducerTaskKindV1::Rebuild, 1, &after, &after, &semantic, None, Some("//docs")),
    task([1; 16], IndexProducerTaskKindV1::Rebuild, 1, &after, &after, &semantic, None, Some("/docs/../private")),
  ] {
    assert!(coordinator.admit(invalid, 10).is_err());
    assert_eq!(coordinator.snapshot().pending_tasks, 0);
  }
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
}

#[test]
fn admission_is_bounded_body_free_deduplicated_and_canonical() {
  let memory = memory(100_000);
  let mut coordinator = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory.clone(), options(2, 20_000, 3)).unwrap();
  let before = hash(b"before");
  let after = hash(b"after");
  let semantic = hash(b"semantic");
  let journal = hash(b"journal");
  let request = task([2; 16], IndexProducerTaskKindV1::MutationWindow, 2, &before, &after, &semantic, Some(&journal), None);
  assert_eq!(coordinator.admit(request, 10).unwrap(), IndexProducerAdmissionV1::Queued);
  let retained = coordinator.snapshot();
  assert_eq!(retained.pending_tasks, 1);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, retained.pending_bytes);

  assert_eq!(coordinator.admit(request, 11).unwrap(), IndexProducerAdmissionV1::Duplicate);
  assert_eq!(coordinator.snapshot(), retained);
  let conflicting = task([2; 16], IndexProducerTaskKindV1::MutationWindow, 3, &before, &after, &semantic, Some(&journal), None);
  assert!(matches!(coordinator.admit(conflicting, 12), Err(IndexProducerCoordinatorErrorV1::ConflictingTask { .. })));

  let earlier = task([1; 16], IndexProducerTaskKindV1::MutationWindow, 1, &before, &after, &semantic, Some(&journal), None);
  coordinator.admit(earlier, 13).unwrap();
  let lease = coordinator.lease_next(13, false).unwrap().expect("earliest canonical task");
  assert_eq!(lease.operation_id(), [1; 16]);
  assert_eq!(coordinator.snapshot().leased_tasks, 1);
  assert!(coordinator.lease_next(13, false).unwrap().is_none(), "one owner leases producer work");
  coordinator.cancel(&lease).unwrap();
  assert_eq!(coordinator.snapshot().pending_tasks, 2);

  let overflow = task([3; 16], IndexProducerTaskKindV1::MutationWindow, 3, &before, &after, &semantic, Some(&journal), None);
  assert!(matches!(coordinator.admit(overflow, 14), Err(IndexProducerCoordinatorErrorV1::SpillRequired { .. })));
  assert_eq!(coordinator.snapshot().pending_tasks, 2);
}

#[test]
fn durable_admission_persists_before_queueing_duplicate_or_pressure_outcomes() {
  let mut coordinator = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(100_000), options(1, 20_000, 3)).unwrap();
  let before = hash(b"before");
  let after = hash(b"after");
  let semantic = hash(b"semantic");
  let journal = hash(b"journal");
  let first = task([0x21; 16], IndexProducerTaskKindV1::MutationWindow, 1, &before, &after, &semantic, Some(&journal), None);
  let second = task([0x22; 16], IndexProducerTaskKindV1::MutationWindow, 2, &before, &after, &semantic, Some(&journal), None);
  let mut store = SpillStore::default();

  assert_eq!(coordinator.admit_durable_or_spill(first, 10, &mut store).unwrap(), IndexProducerAdmissionV1::Queued);
  assert_eq!(store.persisted, vec![[0x21; 16]]);
  assert_eq!(coordinator.admit_durable_or_spill(first, 11, &mut store).unwrap(), IndexProducerAdmissionV1::Duplicate);
  assert_eq!(store.persisted, vec![[0x21; 16], [0x21; 16]]);

  let pressure = coordinator.admit_durable_or_spill(second, 12, &mut store).unwrap();
  assert!(matches!(pressure, IndexProducerAdmissionV1::Spilled { .. }));
  assert_eq!(store.persisted, vec![[0x21; 16], [0x21; 16], [0x22; 16]]);
  assert_eq!(store.calls, vec![([0x22; 16], IndexProducerSpillReasonV1::AdmissionPressure)]);
  assert_eq!(coordinator.snapshot().pending_tasks, 1);

  store.fail = true;
  let third = task([0x23; 16], IndexProducerTaskKindV1::MutationWindow, 3, &before, &after, &semantic, Some(&journal), None);
  assert!(matches!(coordinator.admit_durable_or_spill(third, 13, &mut store), Err(IndexProducerCoordinatorErrorV1::SpillFailed { .. })));
  assert_eq!(coordinator.snapshot().pending_tasks, 1, "durability refusal must precede queue admission");

  let invalid = task([0; 16], IndexProducerTaskKindV1::MutationWindow, 4, &before, &after, &semantic, Some(&journal), None);
  let persisted_before = store.persisted.len();
  assert!(matches!(coordinator.admit_durable_or_spill(invalid, 14, &mut store), Err(IndexProducerCoordinatorErrorV1::InvalidTask(_))));
  assert_eq!(store.persisted.len(), persisted_before, "invalid tasks must not reach durable storage");
}

#[test]
fn maintenance_document_progress_is_memory_bounded_and_uses_the_document_operation_identity() {
  let task_memory = memory(200_000);
  let mut producer = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, task_memory.clone(), options(4, 100_000, 3)).unwrap();
  let mut mutations = mutation_coordinator(memory(500_000));
  let root = hash(b"root");
  let semantic = hash(b"semantic");
  let revision = hash(b"revision-a");
  let parent_operation_id = [0x31; 16];
  producer
    .admit(task(parent_operation_id, IndexProducerTaskKindV1::Rebuild, 9, &root, &root, &semantic, None, Some("/docs")), 100)
    .unwrap();
  let retained_before = producer.snapshot().pending_bytes;
  let lease = producer.lease_next(100, false).unwrap().unwrap();
  assert_eq!(producer.leased_maintenance_resume_after(&lease).unwrap(), None);
  let owner_id = hash(b"field-index");
  let expected_operation_id = derive_index_maintenance_document_operation_id_v1(
    HASH_ALGORITHM,
    parent_operation_id,
    IndexProducerTaskKindV1::Rebuild,
    &root,
    &revision,
    "/docs/a.json",
  )
  .unwrap();

  let progress = producer
    .advance_maintenance_document(
      &lease,
      IndexProducerMaintenanceDocumentRequestV1 {
        revision_hash: &revision,
        path: "/docs/a.json",
        report: IndexProducerReportV1 {
          outcomes: vec![IndexProducerOwnerOutcomeV1::ready(owner_id.clone(), vec![mutation(&owner_id, 1)])],
        },
      },
      &mut mutations,
      101,
      false,
      &mut SpillStore::default(),
    )
    .unwrap();
  assert!(matches!(
    progress,
    IndexProducerMaintenanceProgressV1::Advanced { document_operation_id, .. }
      if document_operation_id == expected_operation_id
  ));
  assert_eq!(producer.leased_maintenance_resume_after(&lease).unwrap(), Some("/docs/a.json"));
  assert!(producer.snapshot().pending_bytes > retained_before);
  assert_eq!(task_memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, producer.snapshot().pending_bytes);

  let batch = mutations.begin_flush(102, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
  assert_eq!(batch.records().len(), 1);
  assert_eq!(batch.records()[0].operation_id(), expected_operation_id);
  producer.cancel(&lease).unwrap();
  assert_eq!(
    producer
      .admit(task(parent_operation_id, IndexProducerTaskKindV1::Rebuild, 9, &root, &root, &semantic, None, Some("/docs")), 102,)
      .unwrap(),
    IndexProducerAdmissionV1::Duplicate,
    "duplicate durable replay must not reset in-memory progress",
  );
  let resumed = producer.lease_next(102, false).unwrap().unwrap();
  assert_eq!(producer.leased_maintenance_resume_after(&resumed).unwrap(), Some("/docs/a.json"));
  producer
    .complete(&resumed, IndexProducerReportV1 { outcomes: Vec::new() }, &mut mutations, 103, false, &mut SpillStore::default())
    .unwrap();
  assert_eq!(task_memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
}

#[test]
fn maintenance_cursor_does_not_advance_past_partial_mutation_admission_and_replay_is_idempotent() {
  let mut producer = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(200_000), options(4, 100_000, 3)).unwrap();
  let mut mutations = mutation_coordinator(memory(500_000));
  let root = hash(b"root");
  let semantic = hash(b"semantic");
  let revision = hash(b"revision-a");
  let parent_operation_id = [0x32; 16];
  producer
    .admit(task(parent_operation_id, IndexProducerTaskKindV1::Rebuild, 9, &root, &root, &semantic, None, Some("/docs")), 100)
    .unwrap();
  let lease = producer.lease_next(100, false).unwrap().unwrap();
  let first_owner = vec![0x10; HASH_ALGORITHM.hash_length()];
  let conflicting_owner = vec![0x20; HASH_ALGORITHM.hash_length()];
  let conflicting_mutation = mutation(&conflicting_owner, 2);
  mutations
    .admit(
      IndexMutationRequestV1 {
        index_id: &conflicting_owner,
        role: conflicting_mutation.role,
        publication_sequence: 9,
        operation_id: [0x99; 16],
        encoded_record: &conflicting_mutation.encoded_record,
      },
      100,
    )
    .unwrap();

  for now_ms in [101, 102] {
    let report = IndexProducerReportV1 {
      outcomes: vec![
        IndexProducerOwnerOutcomeV1::ready(first_owner.clone(), vec![mutation(&first_owner, 1)]),
        IndexProducerOwnerOutcomeV1::ready(conflicting_owner.clone(), vec![conflicting_mutation.clone()]),
      ],
    };
    assert!(matches!(
      producer.advance_maintenance_document(
        &lease,
        IndexProducerMaintenanceDocumentRequestV1 { revision_hash: &revision, path: "/docs/a.json", report },
        &mut mutations,
        now_ms,
        false,
        &mut SpillStore::default(),
      ),
      Err(IndexProducerCoordinatorErrorV1::MutationAdmission { .. })
    ));
    assert_eq!(producer.leased_maintenance_resume_after(&lease).unwrap(), None);
    assert_eq!(mutations.snapshot().active_records, 2, "the first mutation is inserted once and is a duplicate on replay");
  }
  producer.cancel(&lease).unwrap();
}

#[test]
fn maintenance_restart_replays_from_scope_without_duplicating_document_mutations() {
  let mut mutations = mutation_coordinator(memory(500_000));
  let root = hash(b"root");
  let semantic = hash(b"semantic");
  let revision = hash(b"revision-a");
  let owner = hash(b"field-index");
  let operation_id = [0x33; 16];

  let mut first = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(200_000), options(4, 100_000, 3)).unwrap();
  first.admit(task(operation_id, IndexProducerTaskKindV1::Rebuild, 9, &root, &root, &semantic, None, Some("/docs")), 100).unwrap();
  let lease = first.lease_next(100, false).unwrap().unwrap();
  let report = || IndexProducerReportV1 { outcomes: vec![IndexProducerOwnerOutcomeV1::ready(owner.clone(), vec![mutation(&owner, 1)])] };
  first
    .advance_maintenance_document(
      &lease,
      IndexProducerMaintenanceDocumentRequestV1 { revision_hash: &revision, path: "/docs/a.json", report: report() },
      &mut mutations,
      101,
      false,
      &mut SpillStore::default(),
    )
    .unwrap();
  assert_eq!(mutations.snapshot().active_records, 1);
  drop(first);

  let mut recovered = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(200_000), options(4, 100_000, 3)).unwrap();
  recovered.admit(task(operation_id, IndexProducerTaskKindV1::Rebuild, 9, &root, &root, &semantic, None, Some("/docs")), 200).unwrap();
  let lease = recovered.lease_next(200, false).unwrap().unwrap();
  assert_eq!(recovered.leased_maintenance_resume_after(&lease).unwrap(), None, "the restart cursor is intentionally not durable");
  recovered
    .advance_maintenance_document(
      &lease,
      IndexProducerMaintenanceDocumentRequestV1 { revision_hash: &revision, path: "/docs/a.json", report: report() },
      &mut mutations,
      201,
      false,
      &mut SpillStore::default(),
    )
    .unwrap();
  assert_eq!(recovered.leased_maintenance_resume_after(&lease).unwrap(), Some("/docs/a.json"));
  assert_eq!(mutations.snapshot().active_records, 1);
  assert_eq!(mutations.snapshot().active_mutations, 1);
  recovered.cancel(&lease).unwrap();
}

#[test]
fn maintenance_progress_rejects_wrong_mode_revision_scope_order_and_lease_before_admission() {
  let root = hash(b"root");
  let before = hash(b"before");
  let semantic = hash(b"semantic");
  let journal = hash(b"journal");
  let revision = hash(b"revision-a");
  let mut mutations = mutation_coordinator(memory(500_000));

  for (ordinal, request) in [
    task([0x41; 16], IndexProducerTaskKindV1::MutationWindow, 1, &before, &root, &semantic, Some(&journal), None),
    task([0x42; 16], IndexProducerTaskKindV1::Compact, 1, &root, &root, &semantic, None, Some("/docs")),
  ]
  .into_iter()
  .enumerate()
  {
    let mut producer = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(200_000), options(2, 100_000, 3)).unwrap();
    producer.admit(request, ordinal as u64 + 1).unwrap();
    let lease = producer.lease_next(ordinal as u64 + 1, false).unwrap().unwrap();
    assert!(matches!(
      producer.leased_maintenance_resume_after(&lease),
      Err(IndexProducerCoordinatorErrorV1::InvalidMaintenanceDocument(_))
    ));
    assert!(matches!(
      producer.advance_maintenance_document(
        &lease,
        IndexProducerMaintenanceDocumentRequestV1 {
          revision_hash: &revision,
          path: "/docs/a.json",
          report: IndexProducerReportV1 { outcomes: Vec::new() },
        },
        &mut mutations,
        ordinal as u64 + 10,
        false,
        &mut SpillStore::default(),
      ),
      Err(IndexProducerCoordinatorErrorV1::InvalidMaintenanceDocument(_))
    ));
    assert_eq!(mutations.snapshot().active_records, 0);
    producer.cancel(&lease).unwrap();
  }

  let mut producer = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(200_000), options(2, 100_000, 3)).unwrap();
  producer.admit(task([0x43; 16], IndexProducerTaskKindV1::Rebuild, 9, &root, &root, &semantic, None, Some("/docs")), 100).unwrap();
  let lease = producer.lease_next(100, false).unwrap().unwrap();
  let oversized = format!("/docs/{}", "a".repeat(16 * 1024));
  for (revision_hash, path) in [
    (&revision[..31], "/docs/a.json"),
    (&[0; 32][..], "/docs/a.json"),
    (revision.as_slice(), "//docs/a.json"),
    (revision.as_slice(), "/private/a.json"),
    (revision.as_slice(), oversized.as_str()),
  ] {
    assert!(matches!(
      producer.advance_maintenance_document(
        &lease,
        IndexProducerMaintenanceDocumentRequestV1 { revision_hash, path, report: IndexProducerReportV1 { outcomes: Vec::new() } },
        &mut mutations,
        101,
        false,
        &mut SpillStore::default(),
      ),
      Err(IndexProducerCoordinatorErrorV1::InvalidMaintenanceDocument(_))
    ));
    assert_eq!(producer.leased_maintenance_resume_after(&lease).unwrap(), None);
  }
  producer
    .advance_maintenance_document(
      &lease,
      IndexProducerMaintenanceDocumentRequestV1 {
        revision_hash: &revision,
        path: "/docs/b.json",
        report: IndexProducerReportV1 { outcomes: Vec::new() },
      },
      &mut mutations,
      101,
      false,
      &mut SpillStore::default(),
    )
    .unwrap();
  for path in ["/docs/b.json", "/docs/a.json"] {
    assert!(matches!(
      producer.advance_maintenance_document(
        &lease,
        IndexProducerMaintenanceDocumentRequestV1 {
          revision_hash: &revision,
          path,
          report: IndexProducerReportV1 { outcomes: Vec::new() },
        },
        &mut mutations,
        102,
        false,
        &mut SpillStore::default(),
      ),
      Err(IndexProducerCoordinatorErrorV1::InvalidMaintenanceDocument(_))
    ));
    assert_eq!(producer.leased_maintenance_resume_after(&lease).unwrap(), Some("/docs/b.json"));
  }
  let owner = hash(b"index");
  let wrong_owner = hash(b"wrong-index");
  assert!(matches!(
    producer.advance_maintenance_document(
      &lease,
      IndexProducerMaintenanceDocumentRequestV1 {
        revision_hash: &revision,
        path: "/docs/c.json",
        report: IndexProducerReportV1 { outcomes: vec![IndexProducerOwnerOutcomeV1::ready(owner, vec![mutation(&wrong_owner, 1)])] },
      },
      &mut mutations,
      102,
      false,
      &mut SpillStore::default(),
    ),
    Err(IndexProducerCoordinatorErrorV1::InvalidReport(_))
  ));
  assert_eq!(producer.leased_maintenance_resume_after(&lease).unwrap(), Some("/docs/b.json"));
  assert_eq!(mutations.snapshot().active_records, 0);

  let mut other = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(200_000), options(2, 100_000, 3)).unwrap();
  other.admit(task([0x44; 16], IndexProducerTaskKindV1::Rebuild, 10, &root, &root, &semantic, None, Some("/docs")), 100).unwrap();
  let foreign = other.lease_next(100, false).unwrap().unwrap();
  assert!(matches!(
    producer.advance_maintenance_document(
      &foreign,
      IndexProducerMaintenanceDocumentRequestV1 {
        revision_hash: &revision,
        path: "/docs/c.json",
        report: IndexProducerReportV1 { outcomes: Vec::new() },
      },
      &mut mutations,
      103,
      false,
      &mut SpillStore::default(),
    ),
    Err(IndexProducerCoordinatorErrorV1::ForeignLease)
  ));
  producer.cancel(&lease).unwrap();
  assert!(matches!(producer.leased_maintenance_resume_after(&lease), Err(IndexProducerCoordinatorErrorV1::StaleLease)));
  other.cancel(&foreign).unwrap();
}

#[test]
fn maintenance_continuation_pressure_fails_before_mutation_or_progress() {
  let root = hash(b"root");
  let semantic = hash(b"semantic");
  let request = task([0x45; 16], IndexProducerTaskKindV1::Rebuild, 9, &root, &root, &semantic, None, Some("/docs"));
  let mut probe = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(200_000), options(2, 100_000, 3)).unwrap();
  probe.admit(request, 100).unwrap();
  let base_bytes = probe.snapshot().pending_bytes;
  drop(probe);

  let task_memory = memory(200_000);
  let mut producer = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, task_memory.clone(), options(2, base_bytes, 3)).unwrap();
  let mut mutations = mutation_coordinator(memory(500_000));
  producer.admit(request, 100).unwrap();
  let lease = producer.lease_next(100, false).unwrap().unwrap();
  let owner = hash(b"index");
  assert!(matches!(
    producer.advance_maintenance_document(
      &lease,
      IndexProducerMaintenanceDocumentRequestV1 {
        revision_hash: &hash(b"revision"),
        path: "/docs/a.json",
        report: IndexProducerReportV1 { outcomes: vec![IndexProducerOwnerOutcomeV1::ready(owner.clone(), vec![mutation(&owner, 1)])] },
      },
      &mut mutations,
      101,
      false,
      &mut SpillStore::default(),
    ),
    Err(IndexProducerCoordinatorErrorV1::SpillRequired { .. })
  ));
  assert_eq!(producer.leased_maintenance_resume_after(&lease).unwrap(), None);
  assert_eq!(mutations.snapshot().active_records, 0);
  assert_eq!(producer.snapshot().pending_bytes, base_bytes);
  assert_eq!(task_memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, base_bytes);
  producer.cancel(&lease).unwrap();

  let constrained_memory = MemoryCoordinator::new(MemoryPolicy::new(base_bytes, base_bytes + 1, 1, 1).unwrap());
  let mut producer = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, constrained_memory.clone(), options(2, 100_000, 3)).unwrap();
  producer.admit(request, 200).unwrap();
  let lease = producer.lease_next(200, false).unwrap().unwrap();
  assert!(matches!(
    producer.advance_maintenance_document(
      &lease,
      IndexProducerMaintenanceDocumentRequestV1 {
        revision_hash: &hash(b"revision"),
        path: "/docs/a.json",
        report: IndexProducerReportV1 { outcomes: Vec::new() },
      },
      &mut mutations,
      201,
      false,
      &mut SpillStore::default(),
    ),
    Err(IndexProducerCoordinatorErrorV1::SpillRequired { .. })
  ));
  assert_eq!(producer.leased_maintenance_resume_after(&lease).unwrap(), None);
  assert_eq!(producer.snapshot().pending_bytes, base_bytes);
  assert_eq!(constrained_memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, base_bytes);
  producer.cancel(&lease).unwrap();
}

#[test]
fn maintenance_retry_keeps_prior_progress_and_success_resets_the_task_attempt_budget() {
  let root = hash(b"root");
  let semantic = hash(b"semantic");
  let revision_a = hash(b"revision-a");
  let revision_b = hash(b"revision-b");
  let revision_c = hash(b"revision-c");
  let owner = hash(b"field-index");
  let retry_owner = hash(b"retry-owner");
  let operation_id = [0x46; 16];
  let mut producer = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(200_000), options(2, 100_000, 3)).unwrap();
  let mut mutations = mutation_coordinator(memory(500_000));
  producer.admit(task(operation_id, IndexProducerTaskKindV1::Rebuild, 9, &root, &root, &semantic, None, Some("/docs")), 100).unwrap();
  let lease = producer.lease_next(100, false).unwrap().unwrap();
  producer
    .advance_maintenance_document(
      &lease,
      IndexProducerMaintenanceDocumentRequestV1 {
        revision_hash: &revision_a,
        path: "/docs/a.json",
        report: IndexProducerReportV1 { outcomes: Vec::new() },
      },
      &mut mutations,
      101,
      false,
      &mut SpillStore::default(),
    )
    .unwrap();

  let retrying_report = |ordinal| IndexProducerReportV1 {
    outcomes: vec![
      IndexProducerOwnerOutcomeV1::ready(owner.clone(), vec![mutation(&owner, ordinal)]),
      IndexProducerOwnerOutcomeV1::retryable(retry_owner.clone(), 11, 25, IndexProducerFallbackModeV1::AuthoritativeScan, None),
    ],
  };
  let progress = producer
    .advance_maintenance_document(
      &lease,
      IndexProducerMaintenanceDocumentRequestV1 { revision_hash: &revision_b, path: "/docs/b.json", report: retrying_report(2) },
      &mut mutations,
      102,
      false,
      &mut SpillStore::default(),
    )
    .unwrap();
  assert!(matches!(progress, IndexProducerMaintenanceProgressV1::RetryScheduled { attempt: 1, next_retry_at_ms: 127, .. }));
  assert_eq!(mutations.snapshot().active_records, 1, "successful owners remain admitted while another owner retries");
  assert!(producer.lease_next(126, false).unwrap().is_none());
  let lease = producer.lease_next(127, false).unwrap().unwrap();
  assert_eq!(producer.leased_maintenance_resume_after(&lease).unwrap(), Some("/docs/a.json"));
  producer
    .advance_maintenance_document(
      &lease,
      IndexProducerMaintenanceDocumentRequestV1 {
        revision_hash: &revision_b,
        path: "/docs/b.json",
        report: IndexProducerReportV1 { outcomes: vec![IndexProducerOwnerOutcomeV1::ready(owner.clone(), vec![mutation(&owner, 2)])] },
      },
      &mut mutations,
      127,
      false,
      &mut SpillStore::default(),
    )
    .unwrap();
  assert_eq!(producer.leased_maintenance_resume_after(&lease).unwrap(), Some("/docs/b.json"));
  assert_eq!(mutations.snapshot().active_records, 1, "the successful retry replays as an exact duplicate");

  let progress = producer
    .advance_maintenance_document(
      &lease,
      IndexProducerMaintenanceDocumentRequestV1 { revision_hash: &revision_c, path: "/docs/c.json", report: retrying_report(3) },
      &mut mutations,
      128,
      false,
      &mut SpillStore::default(),
    )
    .unwrap();
  assert!(matches!(progress, IndexProducerMaintenanceProgressV1::RetryScheduled { attempt: 1, next_retry_at_ms: 153, .. }));
  let lease = producer.lease_next(153, false).unwrap().unwrap();
  assert_eq!(producer.leased_maintenance_resume_after(&lease).unwrap(), Some("/docs/b.json"));
  producer.cancel(&lease).unwrap();
}

#[test]
fn maintenance_cancellation_and_spill_refusal_retain_the_last_completed_document() {
  let task_memory = memory(200_000);
  let root = hash(b"root");
  let semantic = hash(b"semantic");
  let revision_a = hash(b"revision-a");
  let revision_b = hash(b"revision-b");
  let retry_owner = hash(b"retry-owner");
  let mut producer = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, task_memory.clone(), options(2, 100_000, 1)).unwrap();
  let mut mutations = mutation_coordinator(memory(500_000));
  producer.admit(task([0x47; 16], IndexProducerTaskKindV1::Rebuild, 9, &root, &root, &semantic, None, Some("/docs")), 100).unwrap();
  let lease = producer.lease_next(100, false).unwrap().unwrap();
  producer
    .advance_maintenance_document(
      &lease,
      IndexProducerMaintenanceDocumentRequestV1 {
        revision_hash: &revision_a,
        path: "/docs/a.json",
        report: IndexProducerReportV1 { outcomes: Vec::new() },
      },
      &mut mutations,
      101,
      false,
      &mut SpillStore::default(),
    )
    .unwrap();
  assert!(matches!(
    producer.advance_maintenance_document(
      &lease,
      IndexProducerMaintenanceDocumentRequestV1 {
        revision_hash: &revision_b,
        path: "/docs/b.json",
        report: IndexProducerReportV1 { outcomes: Vec::new() },
      },
      &mut mutations,
      102,
      true,
      &mut SpillStore::default(),
    ),
    Err(IndexProducerCoordinatorErrorV1::Cancelled)
  ));
  let lease = producer.lease_next(102, false).unwrap().unwrap();
  assert_eq!(producer.leased_maintenance_resume_after(&lease).unwrap(), Some("/docs/a.json"));

  let retry_report = IndexProducerReportV1 {
    outcomes: vec![IndexProducerOwnerOutcomeV1::retryable(retry_owner, 11, 25, IndexProducerFallbackModeV1::AuthoritativeScan, None)],
  };
  let mut spills = SpillStore { fail: true, ..SpillStore::default() };
  assert!(matches!(
    producer.advance_maintenance_document(
      &lease,
      IndexProducerMaintenanceDocumentRequestV1 { revision_hash: &revision_b, path: "/docs/b.json", report: retry_report },
      &mut mutations,
      103,
      false,
      &mut spills,
    ),
    Err(IndexProducerCoordinatorErrorV1::SpillFailed { .. })
  ));
  let lease = producer.lease_next(1_103, false).unwrap().unwrap();
  assert_eq!(producer.leased_maintenance_resume_after(&lease).unwrap(), Some("/docs/a.json"));
  spills.fail = false;
  let progress = producer
    .advance_maintenance_document(
      &lease,
      IndexProducerMaintenanceDocumentRequestV1 {
        revision_hash: &revision_b,
        path: "/docs/b.json",
        report: IndexProducerReportV1 {
          outcomes: vec![IndexProducerOwnerOutcomeV1::retryable(
            hash(b"retry-owner"),
            11,
            25,
            IndexProducerFallbackModeV1::AuthoritativeScan,
            None,
          )],
        },
      },
      &mut mutations,
      1_104,
      false,
      &mut spills,
    )
    .unwrap();
  assert!(matches!(progress, IndexProducerMaintenanceProgressV1::Spilled { .. }));
  assert_eq!(producer.snapshot().pending_tasks, 0);
  assert_eq!(task_memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
}

#[test]
fn ready_and_frozen_outcomes_feed_the_single_mutation_coordinator() {
  let task_memory = memory(200_000);
  let mutation_memory = memory(500_000);
  let mut producer = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, task_memory, options(4, 100_000, 3)).unwrap();
  let mut mutations = mutation_coordinator(mutation_memory);
  let before = hash(b"before");
  let after = hash(b"after");
  let semantic = hash(b"semantic");
  let journal = hash(b"journal");
  producer.admit(task([3; 16], IndexProducerTaskKindV1::MutationWindow, 7, &before, &after, &semantic, Some(&journal), None), 100).unwrap();
  let lease = producer.lease_next(100, false).unwrap().unwrap();
  let ready_id = hash(b"ready-index");
  let frozen_id = hash(b"frozen-index");
  let report = IndexProducerReportV1 {
    outcomes: vec![
      IndexProducerOwnerOutcomeV1::ready(ready_id.clone(), vec![mutation(&ready_id, 1)]),
      IndexProducerOwnerOutcomeV1::frozen_unindexable(frozen_id.clone(), 1, 3, None, vec![mutation(&frozen_id, 1)]),
    ],
  };
  let completion = producer.complete(&lease, report, &mut mutations, 101, false, &mut SpillStore::default()).unwrap();
  let IndexProducerCompletionV1::Completed { outcomes } = completion else {
    panic!("task should complete");
  };
  assert_eq!(outcomes.len(), 2);
  assert_eq!(mutations.snapshot().active_records, 2);
  assert_eq!(mutations.snapshot().active_mutations, 2);
  assert_eq!(producer.snapshot().pending_tasks, 0);
}

#[test]
fn mixed_operational_failure_retries_without_losing_successful_index_work() {
  let mut producer = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(200_000), options(4, 100_000, 3)).unwrap();
  let mut mutations = mutation_coordinator(memory(500_000));
  let root = hash(b"root");
  let semantic = hash(b"semantic");
  let scope = "/docs";
  producer.admit(task([4; 16], IndexProducerTaskKindV1::Rebuild, 9, &root, &root, &semantic, None, Some(scope)), 100).unwrap();
  let lease = producer.lease_next(100, false).unwrap().unwrap();
  let scope_id = hash(b"scope");
  let value_store_id = hash(b"value-store");
  let field_index_id = hash(b"field-index");
  let retry_id = hash(b"retry");
  let report = IndexProducerReportV1 {
    outcomes: vec![
      IndexProducerOwnerOutcomeV1::ready(scope_id.clone(), vec![mutation(&scope_id, 1)]),
      IndexProducerOwnerOutcomeV1::ready(value_store_id.clone(), vec![value_mutation(&value_store_id, 1)]),
      IndexProducerOwnerOutcomeV1::ready(field_index_id.clone(), vec![posting_mutation(&field_index_id, 1)]),
      IndexProducerOwnerOutcomeV1::retryable(retry_id, 11, 25, IndexProducerFallbackModeV1::AuthoritativeScan, None),
    ],
  };
  let completion = producer.complete(&lease, report, &mut mutations, 101, false, &mut SpillStore::default()).unwrap();
  assert!(matches!(completion, IndexProducerCompletionV1::RetryScheduled { attempt: 1, next_retry_at_ms: 126, .. }));
  assert_eq!(mutations.snapshot().active_records, 3);
  assert!(producer.lease_next(125, false).unwrap().is_none());
  let retry_lease = producer.lease_next(126, false).unwrap().unwrap();
  assert_eq!(retry_lease.operation_id(), [4; 16]);
  producer.cancel(&retry_lease).unwrap();
}

#[test]
fn retry_exhaustion_spills_and_spill_failure_retains_recoverable_work() {
  let root = hash(b"root");
  let semantic = hash(b"semantic");
  let owner_id = hash(b"retry");
  let report = || IndexProducerReportV1 {
    outcomes: vec![IndexProducerOwnerOutcomeV1::retryable(owner_id.clone(), 11, 1, IndexProducerFallbackModeV1::AuthoritativeScan, None)],
  };
  let mut producer = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(200_000), options(4, 100_000, 1)).unwrap();
  let mut mutations = mutation_coordinator(memory(500_000));
  producer.admit(task([5; 16], IndexProducerTaskKindV1::Rebuild, 10, &root, &root, &semantic, None, Some("/")), 1).unwrap();
  let lease = producer.lease_next(1, false).unwrap().unwrap();
  let mut spills = SpillStore::default();
  let completion = producer.complete(&lease, report(), &mut mutations, 2, false, &mut spills).unwrap();
  assert!(matches!(completion, IndexProducerCompletionV1::Spilled { .. }));
  assert_eq!(spills.calls, vec![([5; 16], IndexProducerSpillReasonV1::RetryExhausted)]);
  assert_eq!(producer.snapshot().pending_tasks, 0);

  producer.admit(task([6; 16], IndexProducerTaskKindV1::Rebuild, 11, &root, &root, &semantic, None, Some("/")), 3).unwrap();
  let lease = producer.lease_next(3, false).unwrap().unwrap();
  spills.fail = true;
  let error = producer.complete(&lease, report(), &mut mutations, 4, false, &mut spills).unwrap_err();
  assert!(matches!(error, IndexProducerCoordinatorErrorV1::SpillFailed { .. }));
  assert_eq!(producer.snapshot().pending_tasks, 1);
  assert_eq!(producer.snapshot().leased_tasks, 0);

  let retry_lease = producer.lease_next(1_004, false).unwrap().expect("spill failure schedules retained work");
  producer.cancel(&retry_lease).unwrap();

  let mut malformed_producer = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(200_000), options(4, 100_000, 1)).unwrap();
  malformed_producer.admit(task([7; 16], IndexProducerTaskKindV1::Rebuild, 12, &root, &root, &semantic, None, Some("/")), 1_005).unwrap();
  let lease = malformed_producer.lease_next(1_005, false).unwrap().unwrap();
  assert_eq!(lease.operation_id(), [7; 16]);
  spills.fail = false;
  spills.malformed_receipt = true;
  let error = malformed_producer.complete(&lease, report(), &mut mutations, 1_006, false, &mut spills).unwrap_err();
  assert!(matches!(error, IndexProducerCoordinatorErrorV1::SpillFailed { code: "spill_artifact_width", .. }));
  assert_eq!(malformed_producer.snapshot().pending_tasks, 1);
  assert_eq!(malformed_producer.snapshot().leased_tasks, 0, "a malformed spill receipt cannot strand the active lease");
  let retry_lease = malformed_producer.lease_next(2_006, false).unwrap().expect("malformed spill receipt retains retryable work");
  spills.malformed_receipt = false;
  spills.dishonest_receipt = true;
  let error = malformed_producer.complete(&retry_lease, report(), &mut mutations, 2_007, false, &mut spills).unwrap_err();
  assert!(matches!(error, IndexProducerCoordinatorErrorV1::SpillFailed { code: "spill_identity_mismatch", .. }));
  assert_eq!(malformed_producer.snapshot().pending_tasks, 1);
  assert_eq!(malformed_producer.snapshot().leased_tasks, 0, "dishonest spill receipt cannot release or strand retained work");
}

#[test]
fn task_level_retry_uses_the_same_backoff_and_exhaustion_path() {
  let root = hash(b"root");
  let semantic = hash(b"semantic");
  let mut producer = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(200_000), options(4, 100_000, 3)).unwrap();
  producer.admit(task([0x51; 16], IndexProducerTaskKindV1::Rebuild, 10, &root, &root, &semantic, None, Some("/")), 100).unwrap();
  let lease = producer.lease_next(100, false).unwrap().unwrap();
  let mut spills = SpillStore::default();

  assert!(matches!(producer.retry_task(&lease, 0, 101, false, &mut spills), Err(IndexProducerCoordinatorErrorV1::InvalidTask(_))));
  assert_eq!(producer.snapshot().leased_tasks, 1, "invalid caller input cannot silently release a lease");
  let completion = producer.retry_task(&lease, 25, 101, false, &mut spills).unwrap();
  assert!(
    matches!(completion, IndexProducerCompletionV1::RetryScheduled { attempt: 1, next_retry_at_ms: 126, ref outcomes } if outcomes.is_empty())
  );
  assert!(producer.lease_next(125, false).unwrap().is_none());

  let lease = producer.lease_next(126, false).unwrap().unwrap();
  let completion = producer.retry_task(&lease, 25, 126, false, &mut spills).unwrap();
  assert!(matches!(completion, IndexProducerCompletionV1::RetryScheduled { attempt: 2, next_retry_at_ms: 151, .. }));
  let lease = producer.lease_next(151, false).unwrap().unwrap();
  let completion = producer.retry_task(&lease, 25, 151, false, &mut spills).unwrap();
  assert!(matches!(completion, IndexProducerCompletionV1::Spilled { ref outcomes, .. } if outcomes.is_empty()));
  assert_eq!(spills.calls, vec![([0x51; 16], IndexProducerSpillReasonV1::RetryExhausted)]);
  assert_eq!(producer.snapshot().pending_tasks, 0);
  assert_eq!(producer.snapshot().scheduled_retries, 2);
}

#[test]
fn task_level_retry_cancellation_and_spill_refusal_retain_recoverable_work() {
  let root = hash(b"root");
  let semantic = hash(b"semantic");
  let mut producer = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(200_000), options(4, 100_000, 1)).unwrap();
  producer.admit(task([0x52; 16], IndexProducerTaskKindV1::Rebuild, 10, &root, &root, &semantic, None, Some("/")), 100).unwrap();
  let lease = producer.lease_next(100, false).unwrap().unwrap();
  assert!(matches!(
    producer.retry_task(&lease, 25, 101, true, &mut SpillStore::default()),
    Err(IndexProducerCoordinatorErrorV1::Cancelled)
  ));
  assert_eq!(producer.snapshot().pending_tasks, 1);
  assert_eq!(producer.snapshot().leased_tasks, 0);

  let lease = producer.lease_next(101, false).unwrap().unwrap();
  let mut spills = SpillStore { fail: true, ..SpillStore::default() };
  assert!(matches!(
    producer.retry_task(&lease, 25, 102, false, &mut spills),
    Err(IndexProducerCoordinatorErrorV1::SpillFailed { code: "spill_refused", .. })
  ));
  assert_eq!(producer.snapshot().pending_tasks, 1);
  assert_eq!(producer.snapshot().leased_tasks, 0);
  assert!(producer.lease_next(1_102, false).unwrap().is_some());
}

#[test]
fn retry_deadline_overflow_releases_the_lease_and_retains_recoverable_work() {
  let root = hash(b"root");
  let semantic = hash(b"semantic");
  let mut producer = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(200_000), options(4, 100_000, 3)).unwrap();
  producer.admit(task([0x53; 16], IndexProducerTaskKindV1::Rebuild, 10, &root, &root, &semantic, None, Some("/")), u64::MAX).unwrap();
  let lease = producer.lease_next(u64::MAX, false).unwrap().unwrap();

  assert!(matches!(
    producer.retry_task(&lease, 25, u64::MAX, false, &mut SpillStore::default()),
    Err(IndexProducerCoordinatorErrorV1::AccountingOverflow("retry deadline"))
  ));
  assert_eq!(producer.snapshot().pending_tasks, 1);
  assert_eq!(producer.snapshot().leased_tasks, 0);
  assert_eq!(producer.snapshot().scheduled_retries, 1);
  assert!(producer.lease_next(u64::MAX, false).unwrap().is_some());
}

#[test]
fn pressure_can_spill_before_cloning_and_cancellation_never_consumes_a_task() {
  let root = hash(b"root");
  let semantic = hash(b"semantic");
  let request = task([7; 16], IndexProducerTaskKindV1::Rebuild, 12, &root, &root, &semantic, None, Some("/large-scope"));
  let mut producer = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(100_000), options(1, 1, 2)).unwrap();
  let mut spills = SpillStore::default();
  let admission = producer.admit_or_spill(request, 1, &mut spills).unwrap();
  assert!(matches!(admission, IndexProducerAdmissionV1::Spilled { .. }));
  assert_eq!(spills.calls, vec![([7; 16], IndexProducerSpillReasonV1::AdmissionPressure)]);
  assert_eq!(producer.snapshot().pending_tasks, 0);

  let mut producer = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(100_000), options(1, 20_000, 2)).unwrap();
  producer.admit(request, 1).unwrap();
  assert!(matches!(producer.lease_next(1, true), Err(IndexProducerCoordinatorErrorV1::Cancelled)));
  assert_eq!(producer.snapshot().pending_tasks, 1);
  assert_eq!(producer.snapshot().leased_tasks, 0);
}

#[test]
fn malformed_or_oversized_reports_fail_before_mutating_either_coordinator() {
  let mut producer = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(200_000), options(4, 100_000, 3)).unwrap();
  let mut mutations = mutation_coordinator(memory(500_000));
  let root = hash(b"root");
  let semantic = hash(b"semantic");
  producer.admit(task([8; 16], IndexProducerTaskKindV1::Rebuild, 13, &root, &root, &semantic, None, Some("/")), 1).unwrap();
  let lease = producer.lease_next(1, false).unwrap().unwrap();
  let id = hash(b"index");
  let wrong_id = hash(b"wrong");
  let malformed = IndexProducerReportV1 { outcomes: vec![IndexProducerOwnerOutcomeV1::ready(id, vec![mutation(&wrong_id, 1)])] };
  assert!(matches!(
    producer.complete(&lease, malformed, &mut mutations, 2, false, &mut SpillStore::default()),
    Err(IndexProducerCoordinatorErrorV1::InvalidReport(_))
  ));
  assert_eq!(mutations.snapshot().active_records, 0);
  assert_eq!(producer.snapshot().leased_tasks, 1);

  let id = hash(b"index");
  let malformed_record =
    IndexProducerMutationV1 { owner_id: id.clone(), role: OrderedIndexRoleV1::ScopeReverse, encoded_record: b"bad".to_vec() };
  let later_malformed =
    IndexProducerReportV1 { outcomes: vec![IndexProducerOwnerOutcomeV1::ready(id.clone(), vec![mutation(&id, 1), malformed_record])] };
  assert!(matches!(
    producer.complete(&lease, later_malformed, &mut mutations, 3, false, &mut SpillStore::default()),
    Err(IndexProducerCoordinatorErrorV1::InvalidReport(_))
  ));
  assert_eq!(mutations.snapshot().active_records, 0, "all report records are decoded before the first admission");

  let retry_with_mutation = IndexProducerReportV1 {
    outcomes: vec![IndexProducerOwnerOutcomeV1 {
      owner_id: id.clone(),
      disposition: IndexProducerOwnerDispositionV1::Retryable {
        stable_reason: 9,
        retry_after_ms: 1,
        fallback_mode: IndexProducerFallbackModeV1::AuthoritativeScan,
        evidence_hash: None,
      },
      mutations: vec![mutation(&id, 1)],
    }],
  };
  assert!(matches!(
    producer.complete(&lease, retry_with_mutation, &mut mutations, 4, false, &mut SpillStore::default()),
    Err(IndexProducerCoordinatorErrorV1::InvalidReport(_))
  ));
  assert_eq!(mutations.snapshot().active_records, 0);

  let too_many_outcomes = IndexProducerReportV1 {
    outcomes: (0..17)
      .map(|ordinal| {
        let owner_id = hash(&[ordinal]);
        IndexProducerOwnerOutcomeV1::ready(owner_id, Vec::new())
      })
      .collect(),
  };
  assert!(matches!(
    producer.complete(&lease, too_many_outcomes, &mut mutations, 5, false, &mut SpillStore::default()),
    Err(IndexProducerCoordinatorErrorV1::InvalidReport(_))
  ));

  let duplicate_id = hash(b"duplicate");
  let duplicate_outcomes = IndexProducerReportV1 {
    outcomes: vec![
      IndexProducerOwnerOutcomeV1::ready(duplicate_id.clone(), Vec::new()),
      IndexProducerOwnerOutcomeV1::ready(duplicate_id, Vec::new()),
    ],
  };
  assert!(matches!(
    producer.complete(&lease, duplicate_outcomes, &mut mutations, 6, false, &mut SpillStore::default()),
    Err(IndexProducerCoordinatorErrorV1::InvalidReport(_))
  ));

  let too_many_mutations = IndexProducerReportV1 {
    outcomes: vec![IndexProducerOwnerOutcomeV1::ready(id.clone(), (1..=129).map(|ordinal| mutation(&id, ordinal)).collect())],
  };
  assert!(matches!(
    producer.complete(&lease, too_many_mutations, &mut mutations, 7, false, &mut SpillStore::default()),
    Err(IndexProducerCoordinatorErrorV1::InvalidReport(_))
  ));
  assert_eq!(mutations.snapshot().active_records, 0, "report limits are checked before mutation admission");
  producer.cancel(&lease).unwrap();
}

#[test]
fn outcome_state_classes_are_validated_against_their_frozen_registries() {
  let mut producer = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(200_000), options(4, 100_000, 3)).unwrap();
  let mut mutations = mutation_coordinator(memory(500_000));
  let root = hash(b"root");
  let semantic = hash(b"semantic");
  producer.admit(task([11; 16], IndexProducerTaskKindV1::Rebuild, 14, &root, &root, &semantic, None, Some("/")), 1).unwrap();
  let lease = producer.lease_next(1, false).unwrap().unwrap();
  let owner_id = hash(b"index");

  let invalid = [
    IndexProducerOwnerDispositionV1::FrozenUnindexable { stage: 7, reason: 1, evidence_hash: None },
    IndexProducerOwnerDispositionV1::FrozenUnindexable { stage: 5, reason: 1, evidence_hash: None },
    IndexProducerOwnerDispositionV1::Retryable {
      stable_reason: 25,
      retry_after_ms: 1,
      fallback_mode: IndexProducerFallbackModeV1::AuthoritativeScan,
      evidence_hash: None,
    },
    IndexProducerOwnerDispositionV1::Degraded {
      stable_reason: 25,
      fallback_mode: IndexProducerFallbackModeV1::AuthoritativeScan,
      evidence_hash: None,
    },
  ];
  for (ordinal, disposition) in invalid.into_iter().enumerate() {
    let report = IndexProducerReportV1 {
      outcomes: vec![IndexProducerOwnerOutcomeV1 { owner_id: owner_id.clone(), disposition, mutations: vec![mutation(&owner_id, 1)] }],
    };
    assert!(matches!(
      producer.complete(&lease, report, &mut mutations, ordinal as u64 + 2, false, &mut SpillStore::default()),
      Err(IndexProducerCoordinatorErrorV1::InvalidReport(_))
    ));
    assert_eq!(mutations.snapshot().active_records, 0, "invalid outcome metadata must fail before mutation admission");
  }
  producer.cancel(&lease).unwrap();
}

#[test]
fn lease_identity_and_cancellation_fail_closed() {
  let root = hash(b"root");
  let semantic = hash(b"semantic");
  let mut first = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(100_000), options(2, 20_000, 2)).unwrap();
  let mut second = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(100_000), options(2, 20_000, 2)).unwrap();
  first.admit(task([9; 16], IndexProducerTaskKindV1::Build, 1, &root, &root, &semantic, None, Some("/")), 1).unwrap();
  second.admit(task([10; 16], IndexProducerTaskKindV1::Build, 1, &root, &root, &semantic, None, Some("/")), 1).unwrap();
  let lease = first.lease_next(1, false).unwrap().unwrap();
  let foreign = second.lease_next(1, false).unwrap().unwrap();
  assert!(matches!(second.cancel(&lease), Err(IndexProducerCoordinatorErrorV1::ForeignLease)));

  let mut mutations = mutation_coordinator(memory(500_000));
  assert!(matches!(
    first.complete(&foreign, IndexProducerReportV1 { outcomes: Vec::new() }, &mut mutations, 1_000, false, &mut SpillStore::default()),
    Err(IndexProducerCoordinatorErrorV1::ForeignLease)
  ));
  assert!(matches!(
    first.complete(&lease, IndexProducerReportV1 { outcomes: Vec::new() }, &mut mutations, 2, false, &mut SpillStore::default()),
    Ok(IndexProducerCompletionV1::Completed { .. })
  ));
  second.cancel(&foreign).unwrap();

  first.admit(task([12; 16], IndexProducerTaskKindV1::Build, 2, &root, &root, &semantic, None, Some("/")), 3).unwrap();
  let lease = first.lease_next(3, false).unwrap().unwrap();
  assert!(matches!(
    first.complete(&lease, IndexProducerReportV1 { outcomes: Vec::new() }, &mut mutations, 1, true, &mut SpillStore::default()),
    Err(IndexProducerCoordinatorErrorV1::Cancelled)
  ));
  assert_eq!(first.snapshot().pending_tasks, 1);
  assert_eq!(first.snapshot().leased_tasks, 0);
  assert!(matches!(first.cancel(&lease), Err(IndexProducerCoordinatorErrorV1::StaleLease)));
}

#[test]
fn stale_completion_cannot_consume_or_cancel_a_replacement_lease() {
  let mut coordinator = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(500_000), options(2, 20_000, 2)).unwrap();
  let root = hash(b"root");
  let semantic = hash(b"semantic");
  coordinator.admit(task([11; 16], IndexProducerTaskKindV1::Build, 1, &root, &root, &semantic, None, Some("/")), 1).unwrap();
  let stale = coordinator.lease_next(1, false).unwrap().unwrap();
  coordinator.cancel(&stale).unwrap();
  let replacement = coordinator.lease_next(2, false).unwrap().unwrap();
  let mut mutations = mutation_coordinator(memory(500_000));

  assert!(matches!(
    coordinator.complete(&stale, IndexProducerReportV1 { outcomes: Vec::new() }, &mut mutations, 2, false, &mut SpillStore::default(),),
    Err(IndexProducerCoordinatorErrorV1::StaleLease)
  ));
  assert_eq!(coordinator.snapshot().leased_tasks, 1);
  assert!(matches!(
    coordinator.complete(
      &replacement,
      IndexProducerReportV1 { outcomes: Vec::new() },
      &mut mutations,
      3,
      false,
      &mut SpillStore::default(),
    ),
    Ok(IndexProducerCompletionV1::Completed { .. })
  ));
}

#[test]
fn every_producer_kind_uses_the_same_leased_path() {
  let root = hash(b"root");
  let before = hash(b"before");
  let semantic = hash(b"semantic");
  let journal = hash(b"journal");
  let kinds = [
    (IndexProducerTaskKindV1::MutationWindow, Some(journal.as_slice()), None),
    (IndexProducerTaskKindV1::Reconcile, Some(journal.as_slice()), None),
    (IndexProducerTaskKindV1::Build, None, Some("/")),
    (IndexProducerTaskKindV1::Rebuild, None, Some("/")),
    (IndexProducerTaskKindV1::Retire, None, Some("/")),
    (IndexProducerTaskKindV1::Compact, None, Some("/")),
    (IndexProducerTaskKindV1::Repair, None, Some("/")),
    (IndexProducerTaskKindV1::ExplicitMutation, None, Some("/")),
    (IndexProducerTaskKindV1::LegacyMigration, None, Some("/")),
  ];
  let mut producer = IndexProducerCoordinatorV1::new(HASH_ALGORITHM, memory(500_000), options(16, 200_000, 3)).unwrap();
  for (ordinal, (kind, journal, scope)) in kinds.into_iter().enumerate() {
    let operation_id = [u8::try_from(ordinal + 1).unwrap(); 16];
    let (task_before, task_after) = if matches!(kind, IndexProducerTaskKindV1::MutationWindow | IndexProducerTaskKindV1::Reconcile) {
      (before.as_slice(), root.as_slice())
    } else {
      (root.as_slice(), root.as_slice())
    };
    producer
      .admit(task(operation_id, kind, ordinal as u64 + 1, task_before, task_after, &semantic, journal, scope), ordinal as u64)
      .unwrap();
  }
  let mut now = 100u64;
  for expected in 1u8..=9 {
    let lease = producer.lease_next(now, false).unwrap().unwrap();
    assert_eq!(lease.operation_id(), [expected; 16]);
    producer.cancel(&lease).unwrap();
    let lease = producer.lease_next(now, false).unwrap().unwrap();
    assert_eq!(lease.operation_id(), [expected; 16]);
    let mut mutations = mutation_coordinator(memory(500_000));
    let completion = producer
      .complete(&lease, IndexProducerReportV1 { outcomes: Vec::new() }, &mut mutations, now + 1, false, &mut SpillStore::default())
      .unwrap();
    assert!(matches!(completion, IndexProducerCompletionV1::Completed { .. }));
    now += 1;
  }
  assert_eq!(producer.snapshot().pending_tasks, 0);
}
