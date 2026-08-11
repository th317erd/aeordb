use super::*;

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::engine::hot_tail::{HotTailPayload, read_hot_tail_checked};
use crate::engine::kv_stages::initial_block_size;
use crate::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use crate::engine::native_durability::{sync_file_all_native, sync_file_data_native};
use crate::engine::v4::database_header::{
  DATABASE_HEADER_V4_DATA_OFFSET, DATABASE_HEADER_V4_REGION_LENGTH, DATABASE_HEADER_V4_SLOT_LENGTH, encode_database_header_slot,
};
use crate::engine::v4::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1, RetirementJournalRecordWriteV1};
use crate::engine::v4::header_publication::HeaderPublicationIo;
use crate::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, encode_semantic_state_object};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug)]
enum FirstAuthorityFailurePoint {
  DataBarrier,
  HeaderWriteBefore,
  HeaderWriteAfter,
  FullBarrier,
  Verify,
}

#[derive(Debug)]
struct FaultingNativeHeaderPublicationIo {
  failure: FirstAuthorityFailurePoint,
}

impl FaultingNativeHeaderPublicationIo {
  fn injected(operation: NativeDurabilityOperation) -> NativeDurabilityError {
    NativeDurabilityError::operation_io(operation, std::io::Error::other("injected first-authority publication failure"))
  }
}

impl HeaderPublicationIo for FaultingNativeHeaderPublicationIo {
  fn read_observation(&self, file: &File) -> Result<DatabaseHeaderObservationV4, DatabaseHeaderPublicationErrorV4> {
    observe_database_header_v4(file)
  }

  fn data_barrier(&self, file: &File) -> Result<(), NativeDurabilityError> {
    if matches!(self.failure, FirstAuthorityFailurePoint::DataBarrier) {
      return Err(Self::injected(NativeDurabilityOperation::DataBarrier));
    }
    sync_file_data_native(file)
  }

  fn write_slot(&self, file: &File, slot: usize, bytes: &[u8; DATABASE_HEADER_V4_SLOT_LENGTH]) -> Result<(), NativeDurabilityError> {
    if matches!(self.failure, FirstAuthorityFailurePoint::HeaderWriteBefore) {
      return Err(Self::injected(NativeDurabilityOperation::WriteAt));
    }
    write_file_at_native(file, (slot * DATABASE_HEADER_V4_SLOT_LENGTH) as u64, bytes)?;
    if matches!(self.failure, FirstAuthorityFailurePoint::HeaderWriteAfter) {
      return Err(Self::injected(NativeDurabilityOperation::WriteAt));
    }
    Ok(())
  }

  fn full_barrier(&self, file: &File) -> Result<(), NativeDurabilityError> {
    if matches!(self.failure, FirstAuthorityFailurePoint::FullBarrier) {
      return Err(Self::injected(NativeDurabilityOperation::FileBarrier));
    }
    sync_file_all_native(file)
  }

  fn verify_region(&self, file: &File, expected: &[u8; DATABASE_HEADER_V4_REGION_LENGTH]) -> Result<(), NativeDurabilityError> {
    if matches!(self.failure, FirstAuthorityFailurePoint::Verify) {
      return Err(Self::injected(NativeDurabilityOperation::ReadBack));
    }
    verify_file_bytes_native(file, 0, expected)
  }
}

struct VisibilityObserver {
  called: bool,
}

impl FirstAuthorityDependencyObserverV1 for VisibilityObserver {
  fn staged(&mut self, kv: &DiskKVStore, entities: &[PreparedWholeEntityV1]) -> Result<(), NativeDurabilityError> {
    self.called = true;
    let snapshot = kv.snapshot_handle().load();
    for entity in entities {
      assert!(snapshot.get(&entity.key).unwrap().is_none());
      assert!(kv.get_buffered(&entity.key).is_some());
    }
    assert_eq!(kv.hot_buffer_len(), FIRST_AUTHORITY_ENTITY_COUNT);
    Ok(())
  }
}

struct FailingVisibilityObserver;

impl FirstAuthorityDependencyObserverV1 for FailingVisibilityObserver {
  fn staged(&mut self, kv: &DiskKVStore, entities: &[PreparedWholeEntityV1]) -> Result<(), NativeDurabilityError> {
    let snapshot = kv.snapshot_handle().load();
    assert!(entities.iter().all(|entity| snapshot.get(&entity.key).unwrap().is_none()));
    Err(NativeDurabilityError::invalid(NativeDurabilityOperation::ReadBack, "injected failure after hidden dependency staging"))
  }
}

struct FailingPostCommitObserver;

impl FirstAuthorityDependencyObserverV1 for FailingPostCommitObserver {
  fn staged(&mut self, _kv: &DiskKVStore, _entities: &[PreparedWholeEntityV1]) -> Result<(), NativeDurabilityError> {
    Ok(())
  }

  fn authority_committed(
    &mut self,
    _kv: &DiskKVStore,
    _entities: &[PreparedWholeEntityV1],
  ) -> Result<(), FirstAuthorityPublicationErrorV1> {
    Err(FirstAuthorityPublicationErrorV1::invalid("injected_post_commit_failure", "injected failure after authority linearization"))
  }
}

#[derive(Clone, Copy, Debug)]
enum DependencyFailurePhase {
  BeforeEntity,
  EntityWritten,
  EntityStaged,
}

struct FailingDependencyObserver {
  phase: DependencyFailurePhase,
  entity_index: usize,
}

impl FirstAuthorityDependencyObserverV1 for FailingDependencyObserver {
  fn before_entity(&mut self, index: usize, _entity: &PreparedWholeEntityV1) -> Result<(), NativeDurabilityError> {
    self.fail_at(DependencyFailurePhase::BeforeEntity, index)
  }

  fn entity_written(&mut self, index: usize, _entity: &PreparedWholeEntityV1) -> Result<(), NativeDurabilityError> {
    self.fail_at(DependencyFailurePhase::EntityWritten, index)
  }

  fn entity_staged(&mut self, index: usize, _entity: &PreparedWholeEntityV1) -> Result<(), NativeDurabilityError> {
    self.fail_at(DependencyFailurePhase::EntityStaged, index)
  }

  fn staged(&mut self, _kv: &DiskKVStore, _entities: &[PreparedWholeEntityV1]) -> Result<(), NativeDurabilityError> {
    Ok(())
  }
}

impl FailingDependencyObserver {
  fn fail_at(&self, phase: DependencyFailurePhase, index: usize) -> Result<(), NativeDurabilityError> {
    if std::mem::discriminant(&self.phase) == std::mem::discriminant(&phase) && self.entity_index == index {
      return Err(NativeDurabilityError::invalid(
        NativeDurabilityOperation::WriteAt,
        format!("injected {phase:?} failure for first-authority entity {index}"),
      ));
    }
    Ok(())
  }
}

#[derive(Debug)]
struct CapturedRetirementSegmentV1 {
  segment_ordinal: u64,
  generation: u64,
  first_replacement_sequence: u64,
  last_replacement_sequence: u64,
  record_count: u32,
  artifact_key: Vec<u8>,
  value: Vec<u8>,
}

impl CapturedRetirementSegmentV1 {
  fn prepared(&self) -> PreparedRetirementJournalSegmentV1<'_> {
    PreparedRetirementJournalSegmentV1 {
      segment_ordinal: self.segment_ordinal,
      generation: self.generation,
      first_replacement_sequence: self.first_replacement_sequence,
      last_replacement_sequence: self.last_replacement_sequence,
      record_count: self.record_count,
      artifact_key: &self.artifact_key,
      value: &self.value,
    }
  }
}

#[derive(Default)]
struct CapturingRetirementSinkV1 {
  captured: Option<CapturedRetirementSegmentV1>,
}

impl RetirementJournalDurableSinkV1 for CapturingRetirementSinkV1 {
  fn publish_synced(
    &mut self,
    segment: &PreparedRetirementJournalSegmentV1<'_>,
  ) -> Result<RetirementJournalDurabilityReceiptV1, RetirementJournalSinkErrorV1> {
    self.captured = Some(CapturedRetirementSegmentV1 {
      segment_ordinal: segment.segment_ordinal,
      generation: segment.generation,
      first_replacement_sequence: segment.first_replacement_sequence,
      last_replacement_sequence: segment.last_replacement_sequence,
      record_count: segment.record_count,
      artifact_key: segment.artifact_key.to_vec(),
      value: segment.value.to_vec(),
    });
    Ok(RetirementJournalDurabilityReceiptV1 {
      artifact_key: segment.artifact_key.to_vec(),
      stored_value_length: segment.value.len() as u32,
      hard_publication_sequence: 1,
    })
  }
}

fn create_environment(
  name: &str,
  failure: Option<FirstAuthorityFailurePoint>,
) -> (tempfile::TempDir, PathBuf, Arc<DurabilityCoordinator>, V4FirstAuthorityPublisher) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join(format!("{name}.aeordb"));
  let mut file = std::fs::OpenOptions::new().create_new(true).read(true).write(true).open(&path).unwrap();
  let algorithm = HashAlgorithm::Blake3_256;
  let kv_block_length = initial_block_size() as u64;
  let header = DatabaseHeaderV4 {
    hash_algorithm: algorithm,
    slot_sequence: 1,
    created_at_ms: 1_700_000_000_000,
    updated_at_ms: 1_700_000_000_000,
    database_id: [0x31; 16],
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
    head_hash: vec![0; algorithm.hash_length()],
    base_hash: vec![0; algorithm.hash_length()],
    target_hash: vec![0; algorithm.hash_length()],
    required_writer_capabilities: [0; 32],
    system_family_registry_version: 1,
    system_family_registry_fingerprint: vec![0x41; algorithm.hash_length()],
    writer_fence_epoch: 1,
    physical_instance_id: [0x51; 16],
  };
  let slot = encode_database_header_slot(&header).unwrap();
  file.seek(SeekFrom::Start(0)).unwrap();
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
  let publisher = if let Some(failure) = failure {
    let publisher_file = kv.clone_database_file().unwrap();
    let observation = observe_database_header_v4(&publisher_file).unwrap();
    validate_kv_header_alignment(&kv, &observation.selected.header).unwrap();
    V4FirstAuthorityPublisher {
      file: publisher_file,
      kv: Mutex::new(kv),
      header_publisher: DatabaseHeaderPublisherV4::with_io(coordinator.clone(), Arc::new(FaultingNativeHeaderPublicationIo { failure })),
      root_state: Mutex::new(()),
    }
  } else {
    V4FirstAuthorityPublisher::new(kv, coordinator.clone()).unwrap()
  };
  (directory, path, coordinator, publisher)
}

fn environment(name: &str) -> (tempfile::TempDir, Arc<DurabilityCoordinator>, V4FirstAuthorityPublisher) {
  let (directory, _path, coordinator, publisher) = create_environment(name, None);
  (directory, coordinator, publisher)
}

fn reopen(path: &Path) -> (Arc<DurabilityCoordinator>, V4FirstAuthorityPublisher) {
  let mut file = std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
  let observation = observe_database_header_v4(&file).unwrap();
  let header = &observation.selected.header;
  let hot_tail = if header.head_hash.iter().any(|byte| *byte != 0) {
    read_hot_tail_checked(&mut file, header.hot_tail_offset, header.hash_algorithm.hash_length()).unwrap()
  } else {
    HotTailPayload::default()
  };
  let coordinator = Arc::new(DurabilityCoordinator::new());
  let kv = DiskKVStore::open_with_layout_and_coordinator(
    file.try_clone().unwrap(),
    header.hash_algorithm,
    header.kv_block_offset,
    header.kv_block_length,
    header.hot_tail_offset,
    header.kv_block_stage as usize,
    hot_tail.writes,
    hot_tail.voids,
    header.kv_block_version,
    coordinator.clone(),
  )
  .unwrap();
  let publisher = V4FirstAuthorityPublisher::new(kv, coordinator.clone()).unwrap();
  (coordinator, publisher)
}

fn seed_namespace_tree_collision(publisher: &V4FirstAuthorityPublisher, request: &FirstAuthorityPublicationRequestV1) {
  let mut observation = publisher.observe().unwrap();
  let sequence = observation.selected.header.write_sequence_high_water + 1;
  let entity = encode_entity(
    EntryTypeV4::DirectoryIndex,
    0,
    observation.selected.header.hash_algorithm,
    request.created_at_ms,
    sequence,
    &request.namespace_tree.root_hash,
    &request.namespace_tree.stored_value,
  )
  .unwrap();
  let offset = observation.selected.header.hot_tail_offset;
  write_file_at_native(&publisher.file, offset, &entity).unwrap();

  let mut kv = publisher.kv.lock().unwrap();
  let hot_tail_offset = offset + entity.len() as u64;
  kv.set_hot_tail_offset(hot_tail_offset);
  kv.insert(KVEntry {
    type_flags: KV_TYPE_DIRECTORY,
    hash: request.namespace_tree.root_hash.clone(),
    offset,
    total_length: entity.len() as u32,
  })
  .unwrap();
  kv.force_flush_hot_buffer().unwrap();
  drop(kv);

  observation.selected.header.updated_at_ms += 1;
  observation.selected.header.write_sequence_high_water = sequence;
  observation.selected.header.hot_tail_offset = hot_tail_offset;
  observation.selected.header.entry_count = 1;
  let slot = encode_database_header_slot(&observation.selected.header).unwrap();
  write_file_at_native(&publisher.file, 0, &slot).unwrap();
  write_file_at_native(&publisher.file, DATABASE_HEADER_V4_SLOT_LENGTH as u64, &slot).unwrap();
  sync_file_all_native(&publisher.file).unwrap();
}

fn request() -> FirstAuthorityPublicationRequestV1 {
  let algorithm = HashAlgorithm::Blake3_256;
  FirstAuthorityPublicationRequestV1 {
    database_id: [0x31; 16],
    transaction_id: [0x61; 16],
    created_at_ms: 1_700_000_000_100,
    namespace_tree: PreparedNamespaceTreeV0 { root_hash: digest_parts(algorithm, &[b"dirc:"]), stored_value: Vec::new() },
    semantic_state: encode_semantic_state_object(
      &SemanticStateWriteV1 {
        required_capabilities: [0; 32],
        availability: SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured },
      },
      algorithm,
    )
    .unwrap(),
    required_capabilities: [0; 32],
    typed_closure_digest: digest_parts(algorithm, &[b"typed test closure"]),
    authority_identity: b"HEAD".to_vec(),
  }
}

fn captured_retirement_segment(database_id: [u8; 16]) -> CapturedRetirementSegmentV1 {
  captured_retirement_segment_with_timestamp(database_id, None)
}

fn captured_retirement_segment_with_timestamp(database_id: [u8; 16], retired_at_ms: Option<u64>) -> CapturedRetirementSegmentV1 {
  let algorithm = HashAlgorithm::Blake3_256;
  let fixture_path =
    Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1/agca-blake3-256-retirement-journal-segment-valid.bin");
  let fixture = std::fs::read(fixture_path).unwrap();
  let decoded = decode_retirement_journal_segment_v1(&fixture, algorithm).unwrap();
  let record = retirement_journal_records_v1(&decoded, algorithm).unwrap().next().unwrap().unwrap();
  let physical_length = 24 + 2 * algorithm.hash_length();
  let old_start = 24;
  let replacement_start = old_start + physical_length;
  let replacement_end = replacement_start + physical_length;
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 * 1024 * 1024, 64 * 1024 * 1024, 1, 8 * 1024 * 1024).unwrap());
  let mut owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id,
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let mut sink = CapturingRetirementSinkV1::default();
  owner
    .append(
      RetirementJournalRecordWriteV1 {
        reason: record.reason,
        replacement_publication_sequence: record.replacement_publication_sequence,
        retired_at_ms: retired_at_ms.unwrap_or(record.retired_at_ms),
        old_incarnation: &record.encoded[old_start..replacement_start],
        replacement_incarnation: &record.encoded[replacement_start..replacement_end],
      },
      1,
      &mut sink,
    )
    .unwrap();
  sink.captured.unwrap()
}

fn publish_first_authority(publisher: &V4FirstAuthorityPublisher) {
  publisher.publish(&request()).unwrap();
}

fn write_redundant_header(publisher: &V4FirstAuthorityPublisher, header: &DatabaseHeaderV4) {
  let encoded = encode_database_header_slot(header).unwrap();
  write_file_at_native(&publisher.file, 0, &encoded).unwrap();
  write_file_at_native(&publisher.file, DATABASE_HEADER_V4_SLOT_LENGTH as u64, &encoded).unwrap();
  sync_file_all_native(&publisher.file).unwrap();
}

#[test]
fn staged_first_authority_entities_are_absent_from_every_published_snapshot() {
  let (_directory, _coordinator, publisher) = environment("hidden");
  let mut observer = VisibilityObserver { called: false };

  let receipt = publisher.publish_with_observer(&request(), &mut observer).unwrap();

  assert!(observer.called);
  assert!(publisher.locator(&receipt.namespace_root.root_hash).unwrap().is_some());
}

#[test]
fn failure_after_dependency_staging_restores_the_old_view_and_hot_tail_frontier() {
  let (_directory, coordinator, publisher) = environment("abort");
  let request = request();
  let before = publisher.observe().unwrap();
  let old_hot_tail_offset = publisher.kv.lock().unwrap().hot_tail_offset();
  let root =
    prepare_namespace_root(&request, before.selected.header.hash_algorithm, before.selected.header.write_sequence_high_water).unwrap();
  let mut observer = FailingVisibilityObserver;

  let error = publisher.publish_with_observer(&request, &mut observer).unwrap_err();

  assert_eq!(error.code(), "durability_failure");
  assert_eq!(publisher.observe().unwrap(), before);
  assert!(publisher.locator(&root.root_hash).unwrap().is_none());
  let kv = publisher.kv.lock().unwrap();
  assert_eq!(kv.hot_tail_offset(), old_hot_tail_offset);
  assert_eq!(kv.write_buffer_len(), 0);
  assert_eq!(kv.hot_buffer_len(), 0);
  assert!(coordinator.hard_failure().unwrap().is_some());
}

#[test]
fn post_commit_failure_returns_the_exact_committed_receipt_and_retry_is_idempotent() {
  let (_directory, coordinator, publisher) = environment("post-commit-failure");
  let request = request();
  let mut observer = FailingPostCommitObserver;

  let error = publisher.publish_with_observer(&request, &mut observer).unwrap_err();

  assert_eq!(error.code(), "first_authority_committed_postcondition_failure");
  let committed = error.committed_receipt().expect("post-commit failure must retain the exact receipt");
  let committed_sequence = committed.publication_sequence;
  let committed_root = committed.namespace_root.root_hash.clone();
  assert!(!committed.idempotent);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, committed_sequence);

  let retry = publisher.publish(&request).unwrap();
  assert!(retry.idempotent);
  assert_eq!(retry.publication_sequence, committed_sequence);
  assert_eq!(retry.namespace_root.root_hash, committed_root);
  assert_eq!(coordinator.snapshot().unwrap().next_sequence, committed_sequence + 1);
}

#[test]
fn retirement_post_commit_failure_retries_the_exact_selected_entity_without_republication() {
  let (_directory, _path, coordinator, mut publisher) = create_environment("retirement-post-commit", None);
  publish_first_authority(&publisher);
  let segment = captured_retirement_segment([0x31; 16]);
  let mut observer = FailingPostCommitObserver;

  let error = publisher.publish_retirement_journal_segment(&segment.prepared(), &mut observer).unwrap_err();

  assert_eq!(error.code(), "retirement_journal_committed_postcondition");
  let committed = publisher.observe().unwrap();
  let committed_frontier = coordinator.snapshot().unwrap().hard_frontier;
  let retry = publisher.publish_synced(&segment.prepared()).unwrap();
  assert_eq!(retry.hard_publication_sequence, committed.selected.header.write_sequence_high_water);
  assert_eq!(publisher.observe().unwrap(), committed);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, committed_frontier);
}

#[test]
fn retirement_exact_retry_survives_later_header_timestamp_advancement() {
  let (_directory, _path, coordinator, mut publisher) = create_environment("retirement-later-header", None);
  publish_first_authority(&publisher);
  let first_segment = captured_retirement_segment([0x31; 16]);
  let first = publisher.publish_synced(&first_segment.prepared()).unwrap();
  let first_entity_timestamp = publisher.observe().unwrap().selected.header.updated_at_ms;
  let later_segment = captured_retirement_segment_with_timestamp([0x31; 16], Some(first_entity_timestamp + 10_000));
  publisher.publish_synced(&later_segment.prepared()).unwrap();
  let later_header = publisher.observe().unwrap();
  let later_frontier = coordinator.snapshot().unwrap().hard_frontier;

  let retry = publisher.publish_synced(&first_segment.prepared()).unwrap();

  assert_eq!(retry, first);
  assert_eq!(publisher.observe().unwrap(), later_header);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, later_frontier);
}

#[test]
fn every_retirement_dependency_failure_keeps_the_old_selected_hot_tail_restartable() {
  let phases = [DependencyFailurePhase::BeforeEntity, DependencyFailurePhase::EntityWritten, DependencyFailurePhase::EntityStaged];
  for phase in phases {
    let (_directory, path, coordinator, publisher) = create_environment(&format!("retirement-dependency-{phase:?}"), None);
    publish_first_authority(&publisher);
    let segment = captured_retirement_segment([0x31; 16]);
    let before = publisher.observe().unwrap();
    let mut observer = FailingDependencyObserver { phase, entity_index: 0 };

    let error = publisher.publish_retirement_journal_segment(&segment.prepared(), &mut observer).unwrap_err();

    assert_eq!(error.code(), "durability_failure", "phase {phase:?}");
    assert_eq!(publisher.observe().unwrap(), before, "phase {phase:?}");
    assert!(publisher.locator(&segment.artifact_key).unwrap().is_none(), "phase {phase:?}");
    assert!(coordinator.hard_failure().unwrap().is_some(), "phase {phase:?}");
    drop(publisher);

    let (_restart_coordinator, mut reopened) = reopen(&path);
    assert_eq!(reopened.observe().unwrap(), before, "phase {phase:?}");
    assert!(reopened.locator(&segment.artifact_key).unwrap().is_none(), "phase {phase:?}");
    reopened.publish_synced(&segment.prepared()).unwrap();
    assert!(reopened.locator(&segment.artifact_key).unwrap().is_some(), "phase {phase:?}");
  }
}

#[test]
fn every_retirement_header_failure_reopens_as_old_or_one_complete_selected_entity() {
  let failures = [
    FirstAuthorityFailurePoint::DataBarrier,
    FirstAuthorityFailurePoint::HeaderWriteBefore,
    FirstAuthorityFailurePoint::HeaderWriteAfter,
    FirstAuthorityFailurePoint::FullBarrier,
    FirstAuthorityFailurePoint::Verify,
  ];
  for failure in failures {
    let (_directory, path, coordinator, publisher) = create_environment(&format!("retirement-header-{failure:?}"), None);
    publish_first_authority(&publisher);
    let segment = captured_retirement_segment([0x31; 16]);
    let before = publisher.observe().unwrap();
    let publisher = V4FirstAuthorityPublisher {
      file: publisher.file,
      kv: publisher.kv,
      header_publisher: DatabaseHeaderPublisherV4::with_io(coordinator.clone(), Arc::new(FaultingNativeHeaderPublicationIo { failure })),
      root_state: publisher.root_state,
    };

    let error = publisher.publish_retirement_journal_segment(&segment.prepared(), &mut NoopFirstAuthorityDependencyObserverV1).unwrap_err();

    assert_eq!(error.code(), "durability_failure", "failure point {failure:?}");
    assert!(coordinator.hard_failure().unwrap().is_some(), "failure point {failure:?}");
    let interrupted = publisher.observe().unwrap();
    drop(publisher);

    let (restart_coordinator, mut reopened) = reopen(&path);
    assert_eq!(reopened.observe().unwrap(), interrupted, "failure point {failure:?}");
    let selected_new_entity = interrupted.selected.header.write_sequence_high_water > before.selected.header.write_sequence_high_water;
    assert_eq!(reopened.locator(&segment.artifact_key).unwrap().is_some(), selected_new_entity, "failure point {failure:?}");
    let frontier_before_retry = restart_coordinator.snapshot().unwrap().hard_frontier;
    let retry = reopened.publish_synced(&segment.prepared()).unwrap();
    assert!(reopened.locator(&segment.artifact_key).unwrap().is_some(), "failure point {failure:?}");
    if selected_new_entity {
      assert_eq!(retry.hard_publication_sequence, interrupted.selected.header.write_sequence_high_water, "failure point {failure:?}");
      assert_eq!(restart_coordinator.snapshot().unwrap().hard_frontier, frontier_before_retry, "failure point {failure:?}");
    } else {
      assert!(restart_coordinator.snapshot().unwrap().hard_frontier > frontier_before_retry, "failure point {failure:?}");
    }
  }
}

#[test]
fn retirement_authority_preconditions_refuse_before_append_or_ticket_reservation() {
  let segment = captured_retirement_segment([0x31; 16]);

  let (_missing_directory, missing_path, missing_coordinator, mut missing) = create_environment("retirement-missing-authority", None);
  let missing_before = missing.observe().unwrap();
  let missing_length = std::fs::metadata(&missing_path).unwrap().len();
  let missing_sequence = missing_coordinator.snapshot().unwrap().next_sequence;
  let error = missing.publish_synced(&segment.prepared()).unwrap_err();
  assert_eq!(error.code(), "retirement_journal_missing_authority");
  assert_eq!(missing.observe().unwrap(), missing_before);
  assert_eq!(std::fs::metadata(&missing_path).unwrap().len(), missing_length);
  assert_eq!(missing_coordinator.snapshot().unwrap().next_sequence, missing_sequence);

  let (_mismatch_directory, mismatch_path, mismatch_coordinator, mut mismatch) = create_environment("retirement-database-mismatch", None);
  publish_first_authority(&mismatch);
  let other_database_segment = captured_retirement_segment([0x32; 16]);
  let mismatch_before = mismatch.observe().unwrap();
  let mismatch_length = std::fs::metadata(&mismatch_path).unwrap().len();
  let mismatch_sequence = mismatch_coordinator.snapshot().unwrap().next_sequence;
  let error = mismatch.publish_synced(&other_database_segment.prepared()).unwrap_err();
  assert_eq!(error.code(), "retirement_journal_database_mismatch");
  assert_eq!(mismatch.observe().unwrap(), mismatch_before);
  assert_eq!(std::fs::metadata(&mismatch_path).unwrap().len(), mismatch_length);
  assert_eq!(mismatch_coordinator.snapshot().unwrap().next_sequence, mismatch_sequence);
}

#[test]
fn degraded_or_exhausted_retirement_authority_refuses_without_flushing_baseline_state() {
  let segment = captured_retirement_segment([0x31; 16]);

  let (_degraded_directory, degraded_path, degraded_coordinator, mut degraded) = create_environment("retirement-degraded", None);
  publish_first_authority(&degraded);
  let selected = degraded.observe().unwrap().selected;
  let invalid_slot_offset = ((1 - selected.selected_slot) * DATABASE_HEADER_V4_SLOT_LENGTH) as u64;
  write_file_at_native(&degraded.file, invalid_slot_offset, &[0; DATABASE_HEADER_V4_SLOT_LENGTH]).unwrap();
  sync_file_all_native(&degraded.file).unwrap();
  let degraded_before = degraded.observe().unwrap();
  assert!(degraded_before.selected.redundancy_degraded);
  let degraded_length = std::fs::metadata(&degraded_path).unwrap().len();
  let degraded_sequence = degraded_coordinator.snapshot().unwrap().next_sequence;
  let error = degraded.publish_synced(&segment.prepared()).unwrap_err();
  assert_eq!(error.code(), "retirement_journal_degraded_header");
  assert_eq!(degraded.observe().unwrap(), degraded_before);
  assert_eq!(std::fs::metadata(&degraded_path).unwrap().len(), degraded_length);
  assert_eq!(degraded_coordinator.snapshot().unwrap().next_sequence, degraded_sequence);

  let (_directory, path, coordinator, mut publisher) = create_environment("retirement-exhausted-write-sequence", None);
  publish_first_authority(&publisher);
  let mut header = publisher.observe().unwrap().selected.header;
  header.write_sequence_high_water = u64::MAX;
  write_redundant_header(&publisher, &header);
  let before = publisher.observe().unwrap();
  let before_length = std::fs::metadata(&path).unwrap().len();
  let before_sequence = coordinator.snapshot().unwrap().next_sequence;

  let error = publisher.publish_synced(&segment.prepared()).unwrap_err();

  assert_eq!(error.code(), "retirement_journal_write_sequence_exhausted");
  assert_eq!(publisher.observe().unwrap(), before);
  assert_eq!(std::fs::metadata(&path).unwrap().len(), before_length);
  assert_eq!(coordinator.snapshot().unwrap().next_sequence, before_sequence);
}

#[test]
fn retirement_identity_collisions_refuse_before_flushing_or_header_mutation() {
  for type_flags in [KV_TYPE_DIRECTORY, kv_tag::GC_ARTIFACT] {
    let (_directory, path, coordinator, mut publisher) = create_environment(&format!("retirement-collision-{type_flags}"), None);
    publish_first_authority(&publisher);
    let segment = captured_retirement_segment([0x31; 16]);
    {
      let mut kv = publisher.kv.lock().unwrap();
      kv.insert(KVEntry { type_flags, hash: segment.artifact_key.clone(), offset: 0, total_length: 1 }).unwrap();
    }
    let mut aligned_header = publisher.observe().unwrap().selected.header;
    aligned_header.entry_count += 1;
    write_redundant_header(&publisher, &aligned_header);
    let before = publisher.observe().unwrap();
    let before_length = std::fs::metadata(&path).unwrap().len();
    let before_frontier = coordinator.snapshot().unwrap().hard_frontier;

    let error = publisher.publish_synced(&segment.prepared()).unwrap_err();

    if type_flags == KV_TYPE_DIRECTORY {
      assert_eq!(error.code(), "retirement_journal_identity_collision");
    } else {
      assert_eq!(error.code(), "truncated_entity_prefix");
    }
    assert_eq!(publisher.observe().unwrap(), before);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before_length);
    assert_eq!(coordinator.snapshot().unwrap().hard_frontier, before_frontier);
  }
}

#[test]
fn every_dependency_record_failure_prefix_remains_unadmitted_after_restart() {
  let phases = [DependencyFailurePhase::BeforeEntity, DependencyFailurePhase::EntityWritten, DependencyFailurePhase::EntityStaged];
  for phase in phases {
    for entity_index in 0..FIRST_AUTHORITY_ENTITY_COUNT {
      let (_directory, path, coordinator, publisher) = create_environment(&format!("dependency-{phase:?}-{entity_index}"), None);
      let request = request();
      let before = publisher.observe().unwrap();
      let expected_root =
        prepare_namespace_root(&request, before.selected.header.hash_algorithm, before.selected.header.write_sequence_high_water).unwrap();
      let mut observer = FailingDependencyObserver { phase, entity_index };

      let error = publisher.publish_with_observer(&request, &mut observer).unwrap_err();

      assert_eq!(error.code(), "durability_failure", "phase {phase:?}, entity {entity_index}");
      assert_eq!(publisher.observe().unwrap(), before, "phase {phase:?}, entity {entity_index}");
      assert!(publisher.locator(&expected_root.root_hash).unwrap().is_none(), "phase {phase:?}, entity {entity_index}");
      assert!(coordinator.hard_failure().unwrap().is_some(), "phase {phase:?}, entity {entity_index}");
      drop(publisher);

      let (_restart_coordinator, reopened) = reopen(&path);
      assert_eq!(reopened.observe().unwrap(), before, "phase {phase:?}, entity {entity_index}");
      assert!(reopened.locator(&expected_root.root_hash).unwrap().is_none(), "phase {phase:?}, entity {entity_index}");
      assert!(!reopened.publish(&request).unwrap().idempotent, "phase {phase:?}, entity {entity_index}");
    }
  }
}

#[test]
fn every_header_failure_prefix_reopens_as_old_or_one_complete_selected_authority() {
  let failures = [
    FirstAuthorityFailurePoint::DataBarrier,
    FirstAuthorityFailurePoint::HeaderWriteBefore,
    FirstAuthorityFailurePoint::HeaderWriteAfter,
    FirstAuthorityFailurePoint::FullBarrier,
    FirstAuthorityFailurePoint::Verify,
  ];

  for failure in failures {
    let (_directory, path, coordinator, publisher) = create_environment(&format!("prefix-{failure:?}"), Some(failure));
    let request = request();
    let initial = publisher.observe().unwrap();
    let expected_root =
      prepare_namespace_root(&request, initial.selected.header.hash_algorithm, initial.selected.header.write_sequence_high_water).unwrap();

    let error = publisher.publish(&request).unwrap_err();
    assert_eq!(error.code(), "durability_failure", "failure point {failure:?}");
    assert!(coordinator.hard_failure().unwrap().is_some(), "failure point {failure:?}");
    let interrupted = publisher.observe().unwrap();
    drop(publisher);

    let (restart_coordinator, reopened) = reopen(&path);
    let selected_after_restart = reopened.observe().unwrap();
    assert_eq!(selected_after_restart, interrupted, "failure point {failure:?}");
    let selected_new_authority = selected_after_restart.selected.header.head_hash == expected_root.root_hash;
    if selected_new_authority {
      assert!(reopened.locator(&expected_root.root_hash).unwrap().is_some(), "failure point {failure:?}");
      assert!(reopened.admission_locator(&expected_root.root_hash).unwrap().is_some(), "failure point {failure:?}");
    } else {
      assert!(selected_after_restart.selected.header.head_hash.iter().all(|byte| *byte == 0), "failure point {failure:?}");
      assert!(reopened.locator(&expected_root.root_hash).unwrap().is_none(), "failure point {failure:?}");
      assert!(reopened.admission_locator(&expected_root.root_hash).unwrap().is_none(), "failure point {failure:?}");
    }

    let frontier_before_retry = restart_coordinator.snapshot().unwrap().hard_frontier;
    let retry = reopened.publish(&request).unwrap();
    assert_eq!(retry.idempotent, selected_new_authority, "failure point {failure:?}");
    assert_eq!(retry.namespace_root.root_hash, expected_root.root_hash, "failure point {failure:?}");
    assert!(reopened.locator(&expected_root.root_hash).unwrap().is_some(), "failure point {failure:?}");
    assert!(reopened.admission_locator(&expected_root.root_hash).unwrap().is_some(), "failure point {failure:?}");
    let frontier_after_retry = restart_coordinator.snapshot().unwrap().hard_frontier;
    if selected_new_authority {
      assert_eq!(frontier_after_retry, frontier_before_retry, "failure point {failure:?}");
    } else {
      assert!(frontier_after_retry > frontier_before_retry, "failure point {failure:?}");
    }
  }
}

#[test]
fn malformed_requests_and_oversized_roots_refuse_before_ticket_or_file_mutation() {
  let (_directory, coordinator, publisher) = environment("malformed");
  let before_header = publisher.observe().unwrap();
  let before_file_length = publisher.file.metadata().unwrap().len();
  let before_sequence = coordinator.snapshot().unwrap().next_sequence;
  let valid = request();
  let mut cases = Vec::new();

  let mut database_mismatch = valid.clone();
  database_mismatch.database_id[0] ^= 0xFF;
  cases.push(("database mismatch", database_mismatch));
  let mut zero_transaction = valid.clone();
  zero_transaction.transaction_id = [0; 16];
  cases.push(("zero transaction", zero_transaction));
  let mut timestamp_overflow = valid.clone();
  timestamp_overflow.created_at_ms = i64::MAX as u64 + 1;
  cases.push(("timestamp overflow", timestamp_overflow));
  let mut semantic_identity_mismatch = valid.clone();
  semantic_identity_mismatch.semantic_state.object_id[0] ^= 0xFF;
  cases.push(("semantic identity mismatch", semantic_identity_mismatch));
  let mut closure_width_mismatch = valid.clone();
  closure_width_mismatch.typed_closure_digest.pop();
  cases.push(("closure width mismatch", closure_width_mismatch));
  let mut empty_authority = valid.clone();
  empty_authority.authority_identity.clear();
  cases.push(("empty authority", empty_authority));
  let mut tree_identity_mismatch = valid.clone();
  tree_identity_mismatch.namespace_tree.root_hash[0] ^= 0xFF;
  cases.push(("tree identity mismatch", tree_identity_mismatch));

  for (case, invalid) in cases {
    assert!(publisher.publish(&invalid).is_err(), "case {case}");
    assert_eq!(publisher.observe().unwrap(), before_header, "case {case}");
    assert_eq!(publisher.file.metadata().unwrap().len(), before_file_length, "case {case}");
    assert_eq!(coordinator.snapshot().unwrap().next_sequence, before_sequence, "case {case}");
    assert!(coordinator.hard_failure().unwrap().is_none(), "case {case}");
  }

  let mut oversized_tree = valid.clone();
  oversized_tree.namespace_tree.stored_value = vec![0x61; FIRST_AUTHORITY_NAMESPACE_TREE_CAP + 1];
  let error = publisher.publish(&oversized_tree).unwrap_err();
  assert_eq!(error.code(), "first_authority_namespace_tree_exceeds_cap");
  assert_eq!(publisher.observe().unwrap(), before_header);
  assert_eq!(publisher.file.metadata().unwrap().len(), before_file_length);
  assert_eq!(coordinator.snapshot().unwrap().next_sequence, before_sequence);
  assert!(coordinator.hard_failure().unwrap().is_none());

  assert!(!publisher.publish(&valid).unwrap().idempotent, "invalid requests must leave authority reusable");
}

#[test]
fn existing_package_identity_refuses_before_hard_admission_or_new_bytes() {
  let (_directory, coordinator, publisher) = environment("collision");
  let request = request();
  seed_namespace_tree_collision(&publisher, &request);
  let before_header = publisher.observe().unwrap();
  let before_file_length = publisher.file.metadata().unwrap().len();
  let before_sequence = coordinator.snapshot().unwrap().next_sequence;

  let error = publisher.publish(&request).unwrap_err();

  assert_eq!(error.code(), "first_authority_identity_collision");
  assert_eq!(publisher.observe().unwrap(), before_header);
  assert_eq!(publisher.file.metadata().unwrap().len(), before_file_length);
  assert_eq!(coordinator.snapshot().unwrap().next_sequence, before_sequence);
  assert!(coordinator.hard_failure().unwrap().is_none());
}

#[test]
fn concurrent_exact_attempts_publish_once_and_every_retry_observes_the_same_witness() {
  let (_directory, coordinator, publisher) = environment("concurrent");
  let publisher = Arc::new(publisher);
  let request = Arc::new(request());
  let start = Arc::new(std::sync::Barrier::new(16));
  let mut workers = Vec::new();
  for _ in 0..16 {
    let publisher = publisher.clone();
    let request = request.clone();
    let start = start.clone();
    workers.push(std::thread::spawn(move || {
      start.wait();
      publisher.publish(&request).unwrap()
    }));
  }
  let receipts: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();

  assert_eq!(receipts.iter().filter(|receipt| !receipt.idempotent).count(), 1);
  let first = &receipts[0];
  for receipt in &receipts[1..] {
    assert_eq!(receipt.namespace_root, first.namespace_root);
    assert_eq!(receipt.admission_control, first.admission_control);
    assert_eq!(receipt.publication_sequence, first.publication_sequence);
    assert_eq!(receipt.observation, first.observation);
  }
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, first.publication_sequence);
}

#[test]
fn clean_restart_loads_the_exact_witness_without_another_hard_publication() {
  let (_directory, path, _coordinator, publisher) = create_environment("restart", None);
  let request = request();
  let first = publisher.publish(&request).unwrap();
  drop(publisher);
  let (restart_coordinator, reopened) = reopen(&path);
  let before = restart_coordinator.snapshot().unwrap();

  let retry = reopened.publish(&request).unwrap();

  assert!(retry.idempotent);
  assert_eq!(retry.namespace_root, first.namespace_root);
  assert_eq!(retry.admission_control, first.admission_control);
  assert_eq!(retry.publication_sequence, first.publication_sequence);
  assert_eq!(restart_coordinator.snapshot().unwrap(), before);
}

#[test]
fn retry_rejects_oversized_locator_metadata_before_allocation() {
  let (_directory, coordinator, publisher) = environment("locator-cap");
  let request = request();
  let receipt = publisher.publish(&request).unwrap();
  let path =
    system_control_path(SystemControlKindV1::RootAdmissionCommit, &receipt.namespace_root.root_hash, SystemControlSlotV1::Immutable)
      .unwrap();
  let key = first_authority_file_path_hash(&path, HashAlgorithm::Blake3_256);
  let before_frontier = coordinator.snapshot().unwrap().hard_frontier;
  let mut kv = publisher.kv.lock().unwrap();
  let mut locator = kv.get(&key).unwrap().unwrap();
  locator.total_length = u32::MAX;
  kv.insert(locator).unwrap();
  drop(kv);

  let error = publisher.publish(&request).unwrap_err();

  assert_eq!(error.code(), "first_authority_locator_exceeds_cap");
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, before_frontier);
}

#[test]
fn selected_root_rejects_a_different_transaction_without_republication() {
  let (_directory, coordinator, publisher) = environment("retry-mismatch");
  let request = request();
  publisher.publish(&request).unwrap();
  let before = coordinator.snapshot().unwrap();
  let mut different = request.clone();
  different.transaction_id[0] ^= 0xFF;

  let error = publisher.publish(&different).unwrap_err();

  assert_eq!(error.code(), "first_authority_witness_mismatch");
  assert_eq!(coordinator.snapshot().unwrap(), before);
}
