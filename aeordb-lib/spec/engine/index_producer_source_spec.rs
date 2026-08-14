use std::collections::BTreeMap;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::file_record::FileRecord;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::index_producer_admission::{admit_mutation_journal_tasks, derive_mutation_operation_id};
use aeordb::engine::v4::index_producer_coordinator::{
  IndexProducerCoordinatorOptionsV1, IndexProducerCoordinatorV1, IndexProducerSpillErrorV1, IndexProducerSpillReasonV1,
  IndexProducerSpillReceiptV1, IndexProducerSpillStoreV1, IndexProducerTaskViewV1,
};
use aeordb::engine::v4::index_producer_source::{
  IndexFileRevisionReadErrorV1, IndexFileRevisionSourceV1, IndexProducerSourceErrorV1, LoadedIndexFileRevisionV1,
  resolve_leased_mutation_record, resolve_mutation_document_transition,
};
use aeordb::engine::v4::index_task::{
  JournalOwnerKindV1, MutationJournalWriteV1, MutationKindV1, MutationRecordWriteV1, MutationSideWriteV1, decode_mutation_journal,
  encode_mutation_journal,
};

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

fn hash(label: &[u8]) -> Vec<u8> {
  aeordb::engine::v4::hash::digest_parts(ALGORITHM, &[b"producer-source:", label])
}

fn file(path: &str, content: u8) -> FileRecord {
  FileRecord {
    path: path.to_string(),
    content_type: Some("application/json".to_string()),
    total_size: 16,
    created_at: 100,
    updated_at: 101,
    metadata: Vec::new(),
    content_hash: vec![content; 32],
    chunk_hashes: vec![vec![content.wrapping_add(1); 32]],
  }
}

fn encoded_journal(path: &str) -> Vec<u8> {
  let before_root = hash(b"before-root");
  let after_root = hash(b"after-root");
  let semantic = hash(b"semantic");
  let mutation = hash(b"mutation");
  let before_revision = hash(b"before-revision");
  let after_revision = hash(b"after-revision");
  encode_mutation_journal(&MutationJournalWriteV1 {
    hash_algorithm: ALGORITHM,
    owner_id: [0x31; 16],
    owner_kind: JournalOwnerKindV1::Task,
    generation: 1,
    segment_ordinal: 0,
    chain_reset: true,
    previous_segment: &[0; 32],
    semantic_state_root: &semantic,
    runtime_boot_id: [0x32; 16],
    records: &[MutationRecordWriteV1 {
      kind: MutationKindV1::Update,
      sequence: 7,
      mutation_id: &mutation,
      batch_ordinal: 0,
      batch_count: 1,
      root_before: &before_root,
      root_after: &after_root,
      before: Some(MutationSideWriteV1 { path, revision: &before_revision }),
      after: Some(MutationSideWriteV1 { path, revision: &after_revision }),
      committed_at_ms: 100,
    }],
  })
  .unwrap()
  .value
}

fn producer() -> IndexProducerCoordinatorV1 {
  let memory = MemoryCoordinator::new(MemoryPolicy::new(6 * 1_024 * 1_024, 8 * 1_024 * 1_024, 1, 1 * 1_024 * 1_024).unwrap());
  IndexProducerCoordinatorV1::new(
    ALGORITHM,
    memory,
    IndexProducerCoordinatorOptionsV1::new(8, 2 * 1_024 * 1_024, 3, 10, 1_000, 16, 256, 2 * 1_024 * 1_024).unwrap(),
  )
  .unwrap()
}

struct Spill;

impl IndexProducerSpillStoreV1 for Spill {
  fn spill(
    &mut self,
    task: IndexProducerTaskViewV1<'_>,
    _reason: IndexProducerSpillReasonV1,
  ) -> Result<IndexProducerSpillReceiptV1, IndexProducerSpillErrorV1> {
    IndexProducerSpillReceiptV1::new(task.operation_id(), hash(b"spill"))
  }
}

#[derive(Default)]
struct RevisionSource {
  records: BTreeMap<(Vec<u8>, String), LoadedIndexFileRevisionV1>,
  failure: Option<IndexFileRevisionReadErrorV1>,
}

impl IndexFileRevisionSourceV1 for RevisionSource {
  fn load_file_revision(
    &self,
    namespace_root: &[u8],
    path: &str,
  ) -> Result<Option<LoadedIndexFileRevisionV1>, IndexFileRevisionReadErrorV1> {
    if let Some(error) = &self.failure {
      return Err(error.clone());
    }
    Ok(self.records.get(&(namespace_root.to_vec(), path.to_string())).cloned())
  }
}

fn leased_fixture(
  path: &str,
) -> (Vec<u8>, IndexProducerCoordinatorV1, aeordb::engine::v4::index_producer_coordinator::IndexProducerLeaseV1) {
  let encoded = encoded_journal(path);
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let mut producer = producer();
  admit_mutation_journal_tasks(ALGORITHM, &mut producer, &journal, 200, &|| false, &mut Spill).unwrap();
  let lease = producer.lease_next(200, false).unwrap().unwrap();
  (encoded, producer, lease)
}

#[test]
fn leased_task_resolves_exactly_one_matching_journal_record() {
  let (encoded, producer, lease) = leased_fixture("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let record = resolve_leased_mutation_record(ALGORITHM, &producer, &lease, &journal, &|| false).unwrap();
  let expected = derive_mutation_operation_id(ALGORITHM, record.mutation_id, record.batch_ordinal).unwrap();
  assert_eq!(lease.operation_id(), expected);
  assert_eq!(record.before_path, Some("/doc.json"));
  assert_eq!(record.after_path, Some("/doc.json"));
}

#[test]
fn exact_before_and_after_revisions_become_a_borrowable_collector_transition() {
  let (encoded, producer, lease) = leased_fixture("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let record = resolve_leased_mutation_record(ALGORITHM, &producer, &lease, &journal, &|| false).unwrap();
  let mut source = RevisionSource::default();
  source.records.insert(
    (record.root_before.to_vec(), "/doc.json".to_string()),
    LoadedIndexFileRevisionV1 { revision_hash: record.before_revision.unwrap().to_vec(), file_record: file("/doc.json", 0x41) },
  );
  source.records.insert(
    (record.root_after.to_vec(), "/doc.json".to_string()),
    LoadedIndexFileRevisionV1 { revision_hash: record.after_revision.unwrap().to_vec(), file_record: file("/doc.json", 0x42) },
  );

  let resolved = resolve_mutation_document_transition(ALGORITHM, &record, &source, &|| false).unwrap();
  let borrowed = resolved.as_collector_transition();
  assert_eq!(borrowed.before.unwrap().file_record.content_hash, vec![0x41; 32]);
  assert_eq!(borrowed.after.unwrap().file_record.content_hash, vec![0x42; 32]);
  assert_eq!(borrowed.before.unwrap().namespace_root, record.root_before);
  assert_eq!(borrowed.after.unwrap().record_revision_hash, record.after_revision.unwrap());
}

#[test]
fn wrong_journal_or_missing_revision_never_falls_through_to_an_empty_transition() {
  let (encoded, producer, lease) = leased_fixture("/doc.json");
  let other = encoded_journal("/other.json");
  let other_journal = decode_mutation_journal(&other, ALGORITHM).unwrap();
  assert!(matches!(
    resolve_leased_mutation_record(ALGORITHM, &producer, &lease, &other_journal, &|| false),
    Err(IndexProducerSourceErrorV1::TaskMismatch(_))
  ));

  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let record = resolve_leased_mutation_record(ALGORITHM, &producer, &lease, &journal, &|| false).unwrap();
  assert!(matches!(
    resolve_mutation_document_transition(ALGORITHM, &record, &RevisionSource::default(), &|| false),
    Err(IndexProducerSourceErrorV1::MissingRevision { .. })
  ));
}

#[test]
fn revision_identity_or_path_mismatch_is_corruption_not_success() {
  let (encoded, producer, lease) = leased_fixture("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let record = resolve_leased_mutation_record(ALGORITHM, &producer, &lease, &journal, &|| false).unwrap();
  let mut source = RevisionSource::default();
  source.records.insert(
    (record.root_before.to_vec(), "/doc.json".to_string()),
    LoadedIndexFileRevisionV1 { revision_hash: hash(b"wrong"), file_record: file("/other.json", 0x41) },
  );
  assert!(matches!(
    resolve_mutation_document_transition(ALGORITHM, &record, &source, &|| false),
    Err(IndexProducerSourceErrorV1::RevisionMismatch { .. }) | Err(IndexProducerSourceErrorV1::PathMismatch { .. })
  ));
}

#[test]
fn cancellation_and_reader_failures_remain_typed() {
  let (encoded, producer, lease) = leased_fixture("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  assert_eq!(
    resolve_leased_mutation_record(ALGORITHM, &producer, &lease, &journal, &|| true).unwrap_err(),
    IndexProducerSourceErrorV1::Cancelled
  );
  let record = resolve_leased_mutation_record(ALGORITHM, &producer, &lease, &journal, &|| false).unwrap();
  let source = RevisionSource {
    records: BTreeMap::new(),
    failure: Some(IndexFileRevisionReadErrorV1::retryable("storage_busy", "injected source outage")),
  };
  assert!(matches!(
    resolve_mutation_document_transition(ALGORITHM, &record, &source, &|| false),
    Err(IndexProducerSourceErrorV1::RevisionRead(error)) if error.code() == "storage_busy"
  ));
  let source = RevisionSource {
    records: BTreeMap::new(),
    failure: Some(IndexFileRevisionReadErrorV1::cancelled("shutdown", "injected worker shutdown")),
  };
  assert_eq!(
    resolve_mutation_document_transition(ALGORITHM, &record, &source, &|| false).unwrap_err(),
    IndexProducerSourceErrorV1::Cancelled
  );
}
