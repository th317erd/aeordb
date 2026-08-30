use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::hot_tail::read_hot_tail_checked;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::contract_generated::kv_tag;
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::entity::{EntryTypeV4, WHOLE_ENTITY_V1_FLAG_SYSTEM, decode_whole_entity};
use aeordb::engine::v4::first_authority::{FirstAuthorityPublicationRequestV1, PreparedNamespaceTreeV0, V4FirstAuthorityPublisher};
use aeordb::engine::v4::gc::immutable_gc_artifact_key;
use aeordb::engine::v4::gc_audit::{
  CorruptGcEvidenceDurabilityReceiptV1, CorruptGcEvidenceDurableSinkV1, CorruptGcEvidenceSinkErrorV1, decode_audit_artifact,
};
use aeordb::engine::v4::gc_lineage_recovery::{
  RetirementLineageRecoveryContextV1, RetirementLineageRecoveryDispositionV1, RetirementLineageRecoveryGroupV1,
  RetirementLineageRecoveryObservationV1, RetirementLineageRecoveryReconcilerV1,
};
use aeordb::engine::v4::gc_mark::{
  GcMarkArtifactV1, MarkMutationJournalSegmentWriteV1, MarkMutationRecordWriteV1, decode_gc_mark_artifact,
  encode_mark_mutation_journal_segment, mark_mutation_journal_records_v1,
};
use aeordb::engine::v4::gc_mark_convergence::{
  MarkMutationJournalBufferOptionsV1, MarkMutationJournalChainStartV1, MarkMutationJournalDurableSinkV1, MarkMutationJournalOwnerV1,
  PreparedMarkMutationJournalSegmentV1,
};
use aeordb::engine::v4::gc_retirement::{
  PreparedRetirementJournalSegmentV1, RetirementJournalBufferOptionsV1, RetirementJournalDurabilityReceiptV1,
  RetirementJournalDurableSinkV1, RetirementJournalOwnerV1, RetirementJournalRecordWriteV1, RetirementJournalSinkErrorV1,
};
use aeordb::engine::v4::gc_state::{RetirementReasonV1, decode_retirement_journal_segment_v1};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, encode_semantic_state_object};
use aeordb::engine::{DiskKVStore, HashAlgorithm};
use tokio_util::sync::CancellationToken;

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1")
}

fn fixture(algorithm: HashAlgorithm) -> Vec<u8> {
  let name = match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => unreachable!("the v4 contract has two fixture widths"),
  };
  fs::read(fixture_root().join(format!("agca-{name}-retirement-journal-segment-valid.bin"))).unwrap()
}

fn evidence_fixture(algorithm: HashAlgorithm) -> Vec<u8> {
  fs::read(fixture_root().join(format!("agca-{}-corrupt-gc-evidence.bin", algorithm_name(algorithm)))).unwrap()
}

fn mark_mutation_fixture(algorithm: HashAlgorithm) -> Vec<u8> {
  fs::read(fixture_root().join(format!("agca-{}-mark-mutation-journal-reset.bin", algorithm_name(algorithm)))).unwrap()
}

fn algorithm_name(algorithm: HashAlgorithm) -> &'static str {
  match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => unreachable!("the v4 contract has two fixture widths"),
  }
}

fn database_id() -> [u8; 16] {
  (0x31u8..=0x40).collect::<Vec<_>>().try_into().unwrap()
}

fn initial_header(algorithm: HashAlgorithm, kv_block_length: u64) -> DatabaseHeaderV4 {
  let hash_width = algorithm.hash_length();
  DatabaseHeaderV4 {
    hash_algorithm: algorithm,
    slot_sequence: 1,
    created_at_ms: 1_700_000_000_000,
    updated_at_ms: 1_700_000_000_000,
    database_id: database_id(),
    write_sequence_high_water: 1,
    required_reader_capabilities: [0; 32],
    kv_block_offset: DATABASE_HEADER_V4_DATA_OFFSET,
    kv_block_length,
    kv_block_version: DiskKVStore::CURRENT_KV_BLOCK_VERSION,
    kv_block_stage: 0,
    resize_in_progress: false,
    resize_target_stage: 0,
    nvt_offset: DATABASE_HEADER_V4_DATA_OFFSET + kv_block_length,
    nvt_length: 0,
    nvt_version: 1,
    backup_type: 0,
    hot_tail_offset: DATABASE_HEADER_V4_DATA_OFFSET + kv_block_length,
    buffer_kvs_offset: 0,
    buffer_nvt_offset: 0,
    entry_count: 0,
    head_hash: vec![0; hash_width],
    base_hash: vec![0; hash_width],
    target_hash: vec![0; hash_width],
    required_writer_capabilities: [0; 32],
    system_family_registry_version: 1,
    system_family_registry_fingerprint: vec![0x41; hash_width],
    writer_fence_epoch: 1,
    physical_instance_id: [0x51; 16],
  }
}

fn request(algorithm: HashAlgorithm) -> FirstAuthorityPublicationRequestV1 {
  let semantic_state = encode_semantic_state_object(
    &SemanticStateWriteV1 {
      required_capabilities: [0; 32],
      availability: SemanticAvailabilityV1::ContentOnly {
        reason: aeordb::engine::v4::namespace::SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured,
      },
    },
    algorithm,
  )
  .unwrap();
  FirstAuthorityPublicationRequestV1 {
    database_id: database_id(),
    transaction_id: [0x61; 16],
    created_at_ms: 1_700_000_000_100,
    namespace_tree: PreparedNamespaceTreeV0 { root_hash: digest_parts(algorithm, &[b"dirc:"]), stored_value: Vec::new() },
    semantic_state,
    required_capabilities: [0; 32],
    typed_closure_digest: digest_parts(algorithm, &[b"typed retirement storage closure"]),
    authority_identity: b"HEAD".to_vec(),
  }
}

fn create_publisher(algorithm: HashAlgorithm) -> (tempfile::TempDir, PathBuf, Arc<DurabilityCoordinator>, V4FirstAuthorityPublisher) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("retirement-storage.aeordb");
  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(&path).unwrap();
  let header = initial_header(algorithm, initial_block_size());
  let slot = encode_database_header_slot(&header).unwrap();
  file.write_all(&slot).unwrap();
  file.write_all(&slot).unwrap();
  let coordinator = Arc::new(DurabilityCoordinator::new());
  let kv = DiskKVStore::create_with_coordinator(
    file.try_clone().unwrap(),
    algorithm,
    header.kv_block_offset,
    header.hot_tail_offset,
    0,
    coordinator.clone(),
  )
  .unwrap();
  file.sync_all().unwrap();
  let publisher = V4FirstAuthorityPublisher::new(kv, coordinator.clone()).unwrap();
  publisher.publish(&request(algorithm)).unwrap();
  (directory, path, coordinator, publisher)
}

fn reopen(path: &Path) -> (Arc<DurabilityCoordinator>, V4FirstAuthorityPublisher) {
  let mut file = OpenOptions::new().read(true).write(true).open(path).unwrap();
  let observation = aeordb::engine::v4::header_publication::observe_database_header_v4(&file).unwrap();
  let header = &observation.selected.header;
  let hot_tail = read_hot_tail_checked(&mut file, header.hot_tail_offset, header.hash_algorithm.hash_length()).unwrap();
  let coordinator = Arc::new(DurabilityCoordinator::new());
  let kv = DiskKVStore::open_with_coordinator(
    file.try_clone().unwrap(),
    header.hash_algorithm,
    header.kv_block_offset,
    header.hot_tail_offset,
    header.kv_block_stage as usize,
    hot_tail.writes,
    hot_tail.voids,
    header.kv_block_version,
    coordinator.clone(),
  )
  .unwrap();
  (coordinator.clone(), V4FirstAuthorityPublisher::new(kv, coordinator).unwrap())
}

fn prepared_fixture<'a>(bytes: &'a [u8], artifact_key: &'a [u8], algorithm: HashAlgorithm) -> PreparedRetirementJournalSegmentV1<'a> {
  let segment = decode_retirement_journal_segment_v1(bytes, algorithm).unwrap();
  PreparedRetirementJournalSegmentV1 {
    segment_ordinal: segment.segment_ordinal,
    generation: segment.generation,
    first_replacement_sequence: segment.first_replacement_sequence,
    last_replacement_sequence: segment.last_replacement_sequence,
    record_count: segment.record_count,
    artifact_key,
    value: bytes,
  }
}

fn prepared_mark_mutation_fixture<'a>(
  bytes: &'a [u8],
  artifact_key: &'a [u8],
  algorithm: HashAlgorithm,
) -> PreparedMarkMutationJournalSegmentV1<'a> {
  let GcMarkArtifactV1::MutationJournal(segment) = decode_gc_mark_artifact(bytes, algorithm).unwrap() else {
    panic!("expected mark-mutation fixture");
  };
  PreparedMarkMutationJournalSegmentV1 {
    segment_ordinal: segment.segment_sequence,
    generation: segment.generation,
    first_publication_sequence: segment.first_sequence,
    last_publication_sequence: segment.last_sequence,
    record_count: segment.record_count,
    artifact_key,
    value: bytes,
  }
}

fn fixture_record<'a>(bytes: &'a [u8], algorithm: HashAlgorithm) -> RetirementJournalRecordWriteV1<'a> {
  let hash_width = algorithm.hash_length();
  let record_start = 32 + 24 + 32 + hash_width;
  let physical_length = 24 + 2 * hash_width;
  RetirementJournalRecordWriteV1 {
    reason: RetirementReasonV1::StableKeyReplace,
    replacement_publication_sequence: u64::from_le_bytes(bytes[record_start + 8..record_start + 16].try_into().unwrap()),
    retired_at_ms: u64::from_le_bytes(bytes[record_start + 16..record_start + 24].try_into().unwrap()),
    old_incarnation: &bytes[record_start + 24..record_start + 24 + physical_length],
    replacement_incarnation: &bytes[record_start + 24 + physical_length..record_start + 24 + 2 * physical_length],
  }
}

fn read_entity(path: &Path, offset: u64, length: u32) -> Vec<u8> {
  let mut file = OpenOptions::new().read(true).open(path).unwrap();
  file.seek(SeekFrom::Start(offset)).unwrap();
  let mut bytes = vec![0; length as usize];
  file.read_exact(&mut bytes).unwrap();
  bytes
}

fn physical_incarnation(algorithm: HashAlgorithm, logical_key_byte: u8, digest_byte: u8, wal_offset: u64, write_sequence: u64) -> Vec<u8> {
  let hash_width = algorithm.hash_length();
  let mut bytes = vec![0u8; 24 + 2 * hash_width];
  bytes[..hash_width].fill(logical_key_byte);
  bytes[hash_width..2 * hash_width].fill(digest_byte);
  bytes[2 * hash_width..2 * hash_width + 8].copy_from_slice(&wal_offset.to_le_bytes());
  bytes[2 * hash_width + 8..2 * hash_width + 16].copy_from_slice(&write_sequence.to_le_bytes());
  bytes[2 * hash_width + 16..2 * hash_width + 20].copy_from_slice(&128u32.to_le_bytes());
  bytes[2 * hash_width + 20] = 3;
  bytes[2 * hash_width + 21] = 1;
  bytes
}

struct EvidenceFirstJournalFailureSink<'a> {
  publisher: &'a mut V4FirstAuthorityPublisher,
  evidence_key: Option<Vec<u8>>,
  journal_key: Option<Vec<u8>>,
  fail_journal_once: bool,
}

impl CorruptGcEvidenceDurableSinkV1 for EvidenceFirstJournalFailureSink<'_> {
  fn publish_corrupt_evidence_synced(
    &mut self,
    artifact_key: &[u8],
    value: &[u8],
  ) -> Result<CorruptGcEvidenceDurabilityReceiptV1, CorruptGcEvidenceSinkErrorV1> {
    self.evidence_key = Some(artifact_key.to_vec());
    self.publisher.publish_corrupt_evidence_synced(artifact_key, value)
  }
}

impl RetirementJournalDurableSinkV1 for EvidenceFirstJournalFailureSink<'_> {
  fn publish_synced(
    &mut self,
    segment: &PreparedRetirementJournalSegmentV1<'_>,
  ) -> Result<RetirementJournalDurabilityReceiptV1, RetirementJournalSinkErrorV1> {
    self.journal_key = Some(segment.artifact_key.to_vec());
    if self.fail_journal_once {
      self.fail_journal_once = false;
      return Err(RetirementJournalSinkErrorV1::new(
        "injected_recovery_journal_failure",
        std::io::Error::other("injected failure after durable recovery evidence"),
      ));
    }
    RetirementJournalDurableSinkV1::publish_synced(self.publisher, segment)
  }
}

#[test]
fn bounded_owner_publishes_one_exact_gc_entity_through_the_shared_hard_authority_at_both_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let expected = fixture(algorithm);
    let (_directory, path, coordinator, mut publisher) = create_publisher(algorithm);
    let before = publisher.observe().unwrap();
    let before_frontier = coordinator.snapshot().unwrap().hard_frontier;
    let cancellation = CancellationToken::new();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(64 * 1024 * 1024, 96 * 1024 * 1024, 1, 8 * 1024 * 1024).unwrap());
    let mut owner = RetirementJournalOwnerV1::new_chain(
      algorithm,
      database_id(),
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &cancellation,
      &memory,
    )
    .unwrap();

    owner.append(fixture_record(&expected, algorithm), 1, &mut publisher).unwrap();

    let after = publisher.observe().unwrap();
    let key = immutable_gc_artifact_key(algorithm, aeordb::engine::v4::gc::GcArtifactKindV1::RetirementJournalSegment, &expected);
    let locator = publisher.locator(&key).unwrap().unwrap();
    assert_eq!(locator.type_flags, kv_tag::GC_ARTIFACT);
    assert_eq!(after.selected.header.slot_sequence, before.selected.header.slot_sequence + 1);
    assert_eq!(after.selected.header.write_sequence_high_water, before.selected.header.write_sequence_high_water + 1);
    assert_eq!(after.selected.header.entry_count, before.selected.header.entry_count + 1);
    assert_eq!(after.selected.header.head_hash, before.selected.header.head_hash);
    assert!(coordinator.snapshot().unwrap().hard_frontier > before_frontier);
    assert_eq!(owner.status().last_hard_publication_sequence, after.selected.header.write_sequence_high_water);

    let entity_bytes = read_entity(&path, locator.offset, locator.total_length);
    let entity = decode_whole_entity(&entity_bytes, algorithm, after.selected.header.write_sequence_high_water).unwrap();
    assert_eq!(entity.entry_type, EntryTypeV4::GcArtifact);
    assert_eq!(entity.flags, WHOLE_ENTITY_V1_FLAG_SYSTEM);
    assert_eq!(entity.key, key);
    assert_eq!(entity.stored_value, expected);
  }
}

#[test]
fn exact_retry_after_reopen_returns_the_original_durable_entity_sequence_without_republication() {
  let algorithm = HashAlgorithm::Blake3_256;
  let expected = fixture(algorithm);
  let (_directory, path, _coordinator, mut publisher) = create_publisher(algorithm);
  let artifact_key = immutable_gc_artifact_key(algorithm, aeordb::engine::v4::gc::GcArtifactKindV1::RetirementJournalSegment, &expected);
  let prepared = prepared_fixture(&expected, &artifact_key, algorithm);
  let first = RetirementJournalDurableSinkV1::publish_synced(&mut publisher, &prepared).unwrap();
  let selected = publisher.observe().unwrap();
  drop(publisher);

  let (coordinator, mut reopened) = reopen(&path);
  let before_frontier = coordinator.snapshot().unwrap().hard_frontier;
  let retry = RetirementJournalDurableSinkV1::publish_synced(&mut reopened, &prepared).unwrap();

  assert_eq!(retry, first);
  assert_eq!(reopened.observe().unwrap(), selected);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, before_frontier);
}

#[test]
fn bounded_mark_mutation_owner_publishes_through_shared_hard_authority_at_both_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let expected = mark_mutation_fixture(algorithm);
    let GcMarkArtifactV1::MutationJournal(journal) = decode_gc_mark_artifact(&expected, algorithm).unwrap() else {
      panic!("expected mark-mutation fixture");
    };
    let records = mark_mutation_journal_records_v1(&journal, algorithm).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    let database_id: [u8; 16] = journal.database_id.try_into().unwrap();
    let run_id: [u8; 16] = journal.run_id.try_into().unwrap();
    let (_directory, path, coordinator, mut publisher) = create_publisher(algorithm);
    let before = publisher.observe().unwrap();
    let before_frontier = coordinator.snapshot().unwrap().hard_frontier;
    let cancellation = CancellationToken::new();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(64 * 1024 * 1024, 96 * 1024 * 1024, 1, 8 * 1024 * 1024).unwrap());
    let options = MarkMutationJournalBufferOptionsV1::new(2, 1024 * 1024, 2 * 1024 * 1024, 30_000).unwrap();
    let mut owner = MarkMutationJournalOwnerV1::new_chain(
      MarkMutationJournalChainStartV1 {
        algorithm,
        database_id,
        run_id,
        generation: journal.generation,
        captured_publication_sequence: journal.first_sequence - 1,
        options,
        cancellation: &cancellation,
      },
      &memory,
    )
    .unwrap();
    for (index, record) in records.iter().enumerate() {
      let observation = owner.observe_committed(
        MarkMutationRecordWriteV1 {
          publication_sequence: record.publication_sequence,
          mutation_id: record.mutation_id,
          root_before: record.root_before,
          root_after: record.root_after,
          published_logical_key: record.published_logical_key,
          new_incarnation: record.new_incarnation_bytes,
          operation: record.operation,
        },
        u64::try_from(index + 1).unwrap(),
      );
      assert!(matches!(observation, aeordb::engine::v4::gc_mark_convergence::MarkMutationObservationV1::Buffered { .. }));
    }
    assert!(owner.flush(&mut publisher).unwrap());

    let after = publisher.observe().unwrap();
    let key = immutable_gc_artifact_key(algorithm, aeordb::engine::v4::gc::GcArtifactKindV1::MarkMutationJournalSegment, &expected);
    let locator = publisher.locator(&key).unwrap().unwrap();
    assert_eq!(locator.type_flags, kv_tag::GC_ARTIFACT);
    assert_eq!(after.selected.header.slot_sequence, before.selected.header.slot_sequence + 1);
    assert_eq!(after.selected.header.write_sequence_high_water, before.selected.header.write_sequence_high_water + 1);
    assert_eq!(after.selected.header.entry_count, before.selected.header.entry_count + 1);
    assert_eq!(after.selected.header.head_hash, before.selected.header.head_hash);
    assert!(coordinator.snapshot().unwrap().hard_frontier > before_frontier);
    assert_eq!(owner.status().last_hard_publication_sequence, after.selected.header.write_sequence_high_water);

    let entity_bytes = read_entity(&path, locator.offset, locator.total_length);
    let entity = decode_whole_entity(&entity_bytes, algorithm, after.selected.header.write_sequence_high_water).unwrap();
    assert_eq!(entity.entry_type, EntryTypeV4::GcArtifact);
    assert_eq!(entity.flags, WHOLE_ENTITY_V1_FLAG_SYSTEM);
    assert_eq!(entity.key, key);
    assert_eq!(entity.stored_value, expected);
  }
}

#[test]
fn mark_mutation_exact_retry_reopens_without_republication_and_mismatch_is_nonmutating() {
  let algorithm = HashAlgorithm::Blake3_256;
  let expected = mark_mutation_fixture(algorithm);
  let artifact_key = immutable_gc_artifact_key(algorithm, aeordb::engine::v4::gc::GcArtifactKindV1::MarkMutationJournalSegment, &expected);
  let valid = prepared_mark_mutation_fixture(&expected, &artifact_key, algorithm);
  let (_directory, path, coordinator, mut publisher) = create_publisher(algorithm);
  let before = publisher.observe().unwrap();
  let before_length = fs::metadata(&path).unwrap().len();
  let before_frontier = coordinator.snapshot().unwrap().hard_frontier;
  let mut wrong_key = valid.artifact_key.to_vec();
  wrong_key[0] ^= 0x80;
  let mut malformed_value = valid.value.to_vec();
  let malformed_last = malformed_value.len() - 1;
  malformed_value[malformed_last] ^= 0x80;
  let checkpoint_value = fs::read(fixture_root().join("agca-blake3-256-mark-run-checkpoint-embedded.bin")).unwrap();
  let checkpoint_key = immutable_gc_artifact_key(algorithm, aeordb::engine::v4::gc::GcArtifactKindV1::MarkRunCheckpoint, &checkpoint_value);
  let cases = [
    PreparedMarkMutationJournalSegmentV1 { artifact_key: &wrong_key, ..valid },
    PreparedMarkMutationJournalSegmentV1 { segment_ordinal: valid.segment_ordinal + 1, ..valid },
    PreparedMarkMutationJournalSegmentV1 { generation: valid.generation + 1, ..valid },
    PreparedMarkMutationJournalSegmentV1 { first_publication_sequence: valid.first_publication_sequence + 1, ..valid },
    PreparedMarkMutationJournalSegmentV1 { last_publication_sequence: valid.last_publication_sequence + 1, ..valid },
    PreparedMarkMutationJournalSegmentV1 { record_count: valid.record_count + 1, ..valid },
    PreparedMarkMutationJournalSegmentV1 { value: &malformed_value, ..valid },
    PreparedMarkMutationJournalSegmentV1 { artifact_key: &checkpoint_key, value: &checkpoint_value, ..valid },
  ];
  for mismatched in cases {
    assert!(MarkMutationJournalDurableSinkV1::publish_mark_mutation_segment_synced(&mut publisher, &mismatched).is_err());
    assert_eq!(publisher.observe().unwrap(), before);
    assert_eq!(fs::metadata(&path).unwrap().len(), before_length);
    assert_eq!(coordinator.snapshot().unwrap().hard_frontier, before_frontier);
  }

  let GcMarkArtifactV1::MutationJournal(journal) = decode_gc_mark_artifact(&expected, algorithm).unwrap() else {
    panic!("expected mark-mutation fixture");
  };
  let decoded_records = mark_mutation_journal_records_v1(&journal, algorithm).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
  let records = decoded_records
    .iter()
    .map(|record| MarkMutationRecordWriteV1 {
      publication_sequence: record.publication_sequence,
      mutation_id: record.mutation_id,
      root_before: record.root_before,
      root_after: record.root_after,
      published_logical_key: record.published_logical_key,
      new_incarnation: record.new_incarnation_bytes,
      operation: record.operation,
    })
    .collect::<Vec<_>>();
  let wrong_database_id = [0x91; 16];
  let run_id: [u8; 16] = journal.run_id.try_into().unwrap();
  let other_database = encode_mark_mutation_journal_segment(&MarkMutationJournalSegmentWriteV1 {
    hash_algorithm: algorithm,
    database_id: &wrong_database_id,
    run_id: &run_id,
    generation: journal.generation,
    segment_ordinal: journal.segment_sequence,
    previous_segment_hash: None,
    records: &records,
  })
  .unwrap();
  let wrong_database_prepared = prepared_mark_mutation_fixture(&other_database.value, &other_database.key, algorithm);
  assert!(MarkMutationJournalDurableSinkV1::publish_mark_mutation_segment_synced(&mut publisher, &wrong_database_prepared).is_err());
  assert_eq!(publisher.observe().unwrap(), before);
  assert_eq!(fs::metadata(&path).unwrap().len(), before_length);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, before_frontier);

  let first = MarkMutationJournalDurableSinkV1::publish_mark_mutation_segment_synced(&mut publisher, &valid).unwrap();
  let selected = publisher.observe().unwrap();
  drop(publisher);
  let (reopen_coordinator, mut reopened) = reopen(&path);
  let reopen_frontier = reopen_coordinator.snapshot().unwrap().hard_frontier;
  let retry = MarkMutationJournalDurableSinkV1::publish_mark_mutation_segment_synced(&mut reopened, &valid).unwrap();
  assert_eq!(retry, first);
  assert_eq!(reopened.observe().unwrap(), selected);
  assert_eq!(reopen_coordinator.snapshot().unwrap().hard_frontier, reopen_frontier);
}

#[test]
fn malformed_or_mismatched_prepared_segments_refuse_before_mutating_header_or_file() {
  let algorithm = HashAlgorithm::Blake3_256;
  let expected = fixture(algorithm);
  let (_directory, path, coordinator, mut publisher) = create_publisher(algorithm);
  let before = publisher.observe().unwrap();
  let before_length = fs::metadata(&path).unwrap().len();
  let before_frontier = coordinator.snapshot().unwrap().hard_frontier;
  let artifact_key = immutable_gc_artifact_key(algorithm, aeordb::engine::v4::gc::GcArtifactKindV1::RetirementJournalSegment, &expected);
  let valid = prepared_fixture(&expected, &artifact_key, algorithm);
  let mut wrong_key = valid.artifact_key.to_vec();
  wrong_key[0] ^= 0x80;
  let cases = [
    PreparedRetirementJournalSegmentV1 { artifact_key: &wrong_key, ..valid },
    PreparedRetirementJournalSegmentV1 { segment_ordinal: valid.segment_ordinal + 1, ..valid },
    PreparedRetirementJournalSegmentV1 { generation: valid.generation + 1, ..valid },
    PreparedRetirementJournalSegmentV1 { record_count: valid.record_count + 1, ..valid },
  ];

  for prepared in cases {
    assert!(RetirementJournalDurableSinkV1::publish_synced(&mut publisher, &prepared).is_err());
    assert_eq!(publisher.observe().unwrap(), before);
    assert_eq!(fs::metadata(&path).unwrap().len(), before_length);
    assert_eq!(coordinator.snapshot().unwrap().hard_frontier, before_frontier);
  }
}

#[test]
fn corrupt_evidence_uses_the_same_hard_immutable_gc_path_and_retries_after_reopen() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let value = evidence_fixture(algorithm);
    let artifact = decode_audit_artifact(&value, algorithm).unwrap();
    let key = artifact.key().to_vec();
    let (_directory, path, coordinator, mut publisher) = create_publisher(algorithm);
    let before = publisher.observe().unwrap();
    let before_frontier = coordinator.snapshot().unwrap().hard_frontier;

    let first = publisher.publish_corrupt_evidence_synced(&key, &value).unwrap();

    let after = publisher.observe().unwrap();
    assert_eq!(after.selected.header.slot_sequence, before.selected.header.slot_sequence + 1);
    assert_eq!(after.selected.header.write_sequence_high_water, before.selected.header.write_sequence_high_water + 1);
    assert_eq!(after.selected.header.entry_count, before.selected.header.entry_count + 1);
    assert_eq!(after.selected.header.head_hash, before.selected.header.head_hash);
    assert!(coordinator.snapshot().unwrap().hard_frontier > before_frontier);
    assert_eq!(first.artifact_key, key);
    assert_eq!(first.stored_value_length, value.len() as u32);
    assert_eq!(first.hard_publication_sequence, after.selected.header.write_sequence_high_water);
    let locator = publisher.locator(&key).unwrap().unwrap();
    let entity_bytes = read_entity(&path, locator.offset, locator.total_length);
    let entity = decode_whole_entity(&entity_bytes, algorithm, after.selected.header.write_sequence_high_water).unwrap();
    assert_eq!(entity.entry_type, EntryTypeV4::GcArtifact);
    assert_eq!(entity.flags, WHOLE_ENTITY_V1_FLAG_SYSTEM);
    assert_eq!(entity.key, key);
    assert_eq!(entity.stored_value, value);
    drop(publisher);

    let (reopen_coordinator, mut reopened) = reopen(&path);
    let reopen_frontier = reopen_coordinator.snapshot().unwrap().hard_frontier;
    let retry = reopened.publish_corrupt_evidence_synced(&key, &value).unwrap();
    assert_eq!(retry, first);
    assert_eq!(reopened.observe().unwrap(), after);
    assert_eq!(reopen_coordinator.snapshot().unwrap().hard_frontier, reopen_frontier);
  }
}

#[test]
fn corrupt_evidence_mismatch_refuses_before_mutating_shared_authority() {
  let algorithm = HashAlgorithm::Blake3_256;
  let value = evidence_fixture(algorithm);
  let artifact = decode_audit_artifact(&value, algorithm).unwrap();
  let mut wrong_key = artifact.key().to_vec();
  wrong_key[0] ^= 0x80;
  let (_directory, path, coordinator, mut publisher) = create_publisher(algorithm);
  let before = publisher.observe().unwrap();
  let before_length = fs::metadata(&path).unwrap().len();
  let before_frontier = coordinator.snapshot().unwrap().hard_frontier;

  assert!(publisher.publish_corrupt_evidence_synced(&wrong_key, &value).is_err());
  assert_eq!(publisher.observe().unwrap(), before);
  assert_eq!(fs::metadata(&path).unwrap().len(), before_length);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, before_frontier);
}

#[test]
fn recovery_uses_one_real_publisher_for_evidence_and_retirement_then_reopens_both() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, coordinator, mut publisher) = create_publisher(algorithm);
  let before = publisher.observe().unwrap();
  let before_frontier = coordinator.snapshot().unwrap().hard_frontier;
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(64 * 1024 * 1024, 96 * 1024 * 1024, 1, 8 * 1024 * 1024).unwrap());
  let mut owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id(),
    1,
    401,
    RetirementJournalBufferOptionsV1::new(4_096, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let old = physical_incarnation(algorithm, 0x41, 0x11, 10_000, 7);
  let selected = physical_incarnation(algorithm, 0x41, 0x12, 20_000, 8);
  let observations = [
    RetirementLineageRecoveryObservationV1 { incarnation: &old, retirement_present: false },
    RetirementLineageRecoveryObservationV1 { incarnation: &selected, retirement_present: false },
  ];
  let mut recovery = RetirementLineageRecoveryReconcilerV1::new(
    algorithm,
    RetirementLineageRecoveryContextV1 {
      database_id: database_id(),
      run_id: [0x71; 16],
      generation: 500,
      detected_at_ms: 1_700_000_500_000,
      recovery_publication_sequence: 9_000,
    },
    &cancellation,
  )
  .unwrap();

  let outcome = recovery
    .recover_group(
      RetirementLineageRecoveryGroupV1 { selected_incarnation: &selected, observations: &observations },
      100,
      &mut owner,
      &mut publisher,
    )
    .unwrap();

  assert_eq!(outcome.disposition, RetirementLineageRecoveryDispositionV1::Synthesized { record_count: 1 });
  assert!(!outcome.authorizes_reclaim());
  let evidence_key = outcome.evidence_receipt.unwrap().artifact_key;
  let journal_key = owner.status().last_segment_hash;
  assert!(publisher.locator(&evidence_key).unwrap().is_some());
  assert!(publisher.locator(&journal_key).unwrap().is_some());
  let after = publisher.observe().unwrap();
  assert_eq!(after.selected.header.write_sequence_high_water, before.selected.header.write_sequence_high_water + 2);
  assert_eq!(after.selected.header.entry_count, before.selected.header.entry_count + 2);
  assert!(coordinator.snapshot().unwrap().hard_frontier > before_frontier);
  drop(publisher);

  let (_reopen_coordinator, reopened) = reopen(&path);
  assert_eq!(reopened.observe().unwrap(), after);
  assert!(reopened.locator(&evidence_key).unwrap().is_some());
  assert!(reopened.locator(&journal_key).unwrap().is_some());
}

#[test]
fn evidence_first_journal_failure_reopens_protected_and_exact_retry_finishes_once() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, mut publisher) = create_publisher(algorithm);
  let before = publisher.observe().unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(64 * 1024 * 1024, 96 * 1024 * 1024, 1, 8 * 1024 * 1024).unwrap());
  let mut owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id(),
    1,
    401,
    RetirementJournalBufferOptionsV1::new(4_096, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let old = physical_incarnation(algorithm, 0x41, 0x11, 10_000, 7);
  let selected = physical_incarnation(algorithm, 0x41, 0x12, 20_000, 8);
  let observations = [
    RetirementLineageRecoveryObservationV1 { incarnation: &old, retirement_present: false },
    RetirementLineageRecoveryObservationV1 { incarnation: &selected, retirement_present: false },
  ];
  let context = RetirementLineageRecoveryContextV1 {
    database_id: database_id(),
    run_id: [0x71; 16],
    generation: 500,
    detected_at_ms: 1_700_000_500_000,
    recovery_publication_sequence: 9_000,
  };
  let mut recovery = RetirementLineageRecoveryReconcilerV1::new(algorithm, context, &cancellation).unwrap();
  let mut sink =
    EvidenceFirstJournalFailureSink { publisher: &mut publisher, evidence_key: None, journal_key: None, fail_journal_once: true };

  let error = recovery
    .recover_group(
      RetirementLineageRecoveryGroupV1 { selected_incarnation: &selected, observations: &observations },
      100,
      &mut owner,
      &mut sink,
    )
    .unwrap_err();
  assert_eq!(error.code(), "retirement_journal_sink");
  assert_eq!(error.admitted_records(), 1);
  let evidence_key = sink.evidence_key.take().unwrap();
  let journal_key = sink.journal_key.take().unwrap();
  drop(sink);
  assert!(publisher.locator(&evidence_key).unwrap().is_some());
  assert!(publisher.locator(&journal_key).unwrap().is_none());
  let evidence_only = publisher.observe().unwrap();
  assert_eq!(evidence_only.selected.header.write_sequence_high_water, before.selected.header.write_sequence_high_water + 1);
  drop(owner);
  drop(recovery);
  drop(publisher);

  let (_reopen_coordinator, mut reopened) = reopen(&path);
  assert_eq!(reopened.observe().unwrap(), evidence_only);
  assert!(reopened.locator(&evidence_key).unwrap().is_some());
  assert!(reopened.locator(&journal_key).unwrap().is_none());
  let mut retry_owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id(),
    1,
    401,
    RetirementJournalBufferOptionsV1::new(4_096, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let mut retry_recovery = RetirementLineageRecoveryReconcilerV1::new(algorithm, context, &cancellation).unwrap();
  let outcome = retry_recovery
    .recover_group(
      RetirementLineageRecoveryGroupV1 { selected_incarnation: &selected, observations: &observations },
      100,
      &mut retry_owner,
      &mut reopened,
    )
    .unwrap();
  assert_eq!(outcome.disposition, RetirementLineageRecoveryDispositionV1::Synthesized { record_count: 1 });
  assert!(!outcome.authorizes_reclaim());
  assert_eq!(outcome.evidence_receipt.unwrap().artifact_key, evidence_key);
  assert_eq!(retry_owner.status().last_segment_hash, journal_key);
  assert!(reopened.locator(&evidence_key).unwrap().is_some());
  assert!(reopened.locator(&journal_key).unwrap().is_some());
  assert_eq!(
    reopened.observe().unwrap().selected.header.write_sequence_high_water,
    evidence_only.selected.header.write_sequence_high_water + 1
  );
}
