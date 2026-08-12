use super::*;

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Barrier, mpsc};
use std::time::Duration;

use crate::engine::hot_tail::{HotTailPayload, read_hot_tail_checked};
use crate::engine::kv_stages::initial_block_size;
use crate::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use crate::engine::native_durability::{sync_file_all_native, sync_file_data_native};
use crate::engine::v4::database_header::{
  DATABASE_HEADER_V4_DATA_OFFSET, DATABASE_HEADER_V4_REGION_LENGTH, DATABASE_HEADER_V4_SLOT_LENGTH, encode_database_header_slot,
};
use crate::engine::v4::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1, RetirementJournalRecordWriteV1};
use crate::engine::v4::gc::{
  EncodedGcActiveControlV1, EncodedImmutableGcArtifactV1, GcActiveControlWriteV1, GcArtifactKindV1, encode_gc_active_control,
};
use crate::engine::v4::gc_lifecycle::{
  RootExpiryManifestWriteV1, RootExpiryRecordWriteV1, RootLifecycleManifestWriteV1, RootLifecycleSupportClosureBuilderV1,
  RootLifecycleSupportLimitsV1, RootRetirementCommitWriteV1, decode_root_expiry_manifest_v1, decode_root_lifecycle_manifest_v1,
  decode_root_retirement_commit_v1, encode_root_expiry_manifest_v1, encode_root_expiry_record_v1, encode_root_lifecycle_manifest_v1,
  encode_root_retirement_commit_v1,
};
use crate::engine::v4::gc_mark::{MarkRunCheckpointWriteV1, encode_mark_run_checkpoint};
use crate::engine::v4::gc_mark_workspace::{
  DurableMarkWorkspaceClosureV1, DurableMarkWorkspaceV1, MarkWorkspaceBasisV1, MarkWorkspaceIdentityV1, MarkWorkspaceOptionsV1,
};
use crate::engine::v4::header_publication::HeaderPublicationIo;
use crate::engine::v4::gc_state::{
  GcDirectoryRoleV1, GcPhysicalHintV1, GcStateArtifactV1, GcStateDirectoryEntryWriteV1, GcStateDirectoryWriteV1, GcStatePageWriteV1,
  RootExpiryStateV1, decode_gc_state_artifact, encode_gc_state_directory_v1, encode_gc_state_page_v1,
};
use crate::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, encode_semantic_state_object};
use crate::engine::v4::read_view::RootLifecycleObservationV1;
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

#[derive(Debug)]
struct NthHeaderPublicationFaultIo {
  failure: FirstAuthorityFailurePoint,
  target_publication: usize,
  current_publication: AtomicUsize,
}

impl NthHeaderPublicationFaultIo {
  fn new(failure: FirstAuthorityFailurePoint, target_publication: usize) -> Self {
    Self { failure, target_publication, current_publication: AtomicUsize::new(0) }
  }

  fn is_target(&self) -> bool {
    self.current_publication.load(AtomicOrdering::SeqCst) == self.target_publication
  }

  fn injected(operation: NativeDurabilityOperation) -> NativeDurabilityError {
    NativeDurabilityError::operation_io(operation, std::io::Error::other("injected final-selector publication failure"))
  }
}

impl HeaderPublicationIo for NthHeaderPublicationFaultIo {
  fn read_observation(&self, file: &File) -> Result<DatabaseHeaderObservationV4, DatabaseHeaderPublicationErrorV4> {
    observe_database_header_v4(file)
  }

  fn data_barrier(&self, file: &File) -> Result<(), NativeDurabilityError> {
    let publication = self.current_publication.fetch_add(1, AtomicOrdering::SeqCst) + 1;
    if publication == self.target_publication && matches!(self.failure, FirstAuthorityFailurePoint::DataBarrier) {
      return Err(Self::injected(NativeDurabilityOperation::DataBarrier));
    }
    sync_file_data_native(file)
  }

  fn write_slot(&self, file: &File, slot: usize, bytes: &[u8; DATABASE_HEADER_V4_SLOT_LENGTH]) -> Result<(), NativeDurabilityError> {
    if self.is_target() && matches!(self.failure, FirstAuthorityFailurePoint::HeaderWriteBefore) {
      return Err(Self::injected(NativeDurabilityOperation::WriteAt));
    }
    write_file_at_native(file, (slot * DATABASE_HEADER_V4_SLOT_LENGTH) as u64, bytes)?;
    if self.is_target() && matches!(self.failure, FirstAuthorityFailurePoint::HeaderWriteAfter) {
      return Err(Self::injected(NativeDurabilityOperation::WriteAt));
    }
    Ok(())
  }

  fn full_barrier(&self, file: &File) -> Result<(), NativeDurabilityError> {
    if self.is_target() && matches!(self.failure, FirstAuthorityFailurePoint::FullBarrier) {
      return Err(Self::injected(NativeDurabilityOperation::FileBarrier));
    }
    sync_file_all_native(file)
  }

  fn verify_region(&self, file: &File, expected: &[u8; DATABASE_HEADER_V4_REGION_LENGTH]) -> Result<(), NativeDurabilityError> {
    if self.is_target() && matches!(self.failure, FirstAuthorityFailurePoint::Verify) {
      return Err(Self::injected(NativeDurabilityOperation::ReadBack));
    }
    verify_file_bytes_native(file, 0, expected)
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

struct CancelRetirementAfterCommitObserver {
  cancellation: CancellationToken,
}

impl FirstAuthorityDependencyObserverV1 for CancelRetirementAfterCommitObserver {
  fn staged(&mut self, _kv: &DiskKVStore, _entities: &[PreparedWholeEntityV1]) -> Result<(), NativeDurabilityError> {
    Ok(())
  }

  fn authority_committed(
    &mut self,
    _kv: &DiskKVStore,
    _entities: &[PreparedWholeEntityV1],
  ) -> Result<(), FirstAuthorityPublicationErrorV1> {
    self.cancellation.cancel();
    Ok(())
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
  let kv_block_length = initial_block_size();
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

struct PreparedGuardedRootRetirementV1 {
  target_root_hash: Vec<u8>,
  prior_lifecycle_manifest_key: Vec<u8>,
  intent: RootRetirementIntentV1,
  support_closure: RootLifecycleSupportClosureV1,
  retirement_commit: EncodedImmutableGcArtifactV1,
  expiry_manifest: EncodedImmutableGcArtifactV1,
  lifecycle_manifest: EncodedImmutableGcArtifactV1,
  lifecycle_control: EncodedGcActiveControlV1,
  pin_coordinator: RootReadPinCoordinatorV1,
}

impl PreparedGuardedRootRetirementV1 {
  fn request<'a>(&'a self, cancellation: &'a CancellationToken) -> RootRetirementPublicationRequestV1<'a> {
    RootRetirementPublicationRequestV1 {
      hash_algorithm: HashAlgorithm::Blake3_256,
      intent: &self.intent,
      support_closure: &self.support_closure,
      retirement_commit: &self.retirement_commit,
      expiry_manifest: &self.expiry_manifest,
      lifecycle_manifest: &self.lifecycle_manifest,
      lifecycle_control: &self.lifecycle_control,
      publication_timestamp_ms: 1_700_000_100_001,
      monotonic_now_ms: 1_700_000_100_001,
      cancellation,
      pin_coordinator: &self.pin_coordinator,
    }
  }
}

struct ExactRootRetirementAuthorityVerifierV1 {
  called: bool,
  expected_root_hash: Vec<u8>,
  expected_authority_root_set_digest: Vec<u8>,
  returned_authority_root_set_digest: Option<Vec<u8>>,
  target_is_authoritative: bool,
}

struct BlockingRootRetirementAuthorityVerifierV1 {
  entered: Arc<Barrier>,
  release: Arc<Barrier>,
  expected_root_hash: Vec<u8>,
  expected_authority_root_set_digest: Vec<u8>,
}

struct CleanupFailingRootRetirementAuthorityVerifierV1 {
  pin_coordinator: RootReadPinCoordinatorV1,
  expected_authority_root_set_digest: Vec<u8>,
}

impl RootRetirementAuthorityVerifierV1 for CleanupFailingRootRetirementAuthorityVerifierV1 {
  fn recheck_authority_roots(
    &mut self,
    request: RootRetirementAuthorityRecheckRequestV1<'_>,
  ) -> Result<RootRetirementAuthoritySnapshotV1, RootRetirementAuthorityRecheckErrorV1> {
    assert_eq!(request.expected_authority_root_set_digest, self.expected_authority_root_set_digest);
    self.pin_coordinator.fail_next_cleanup_for_test();
    Ok(RootRetirementAuthoritySnapshotV1 {
      target_is_authoritative: false,
      authority_root_set_digest: self.expected_authority_root_set_digest.clone(),
    })
  }
}

impl RootRetirementAuthorityVerifierV1 for BlockingRootRetirementAuthorityVerifierV1 {
  fn recheck_authority_roots(
    &mut self,
    request: RootRetirementAuthorityRecheckRequestV1<'_>,
  ) -> Result<RootRetirementAuthoritySnapshotV1, RootRetirementAuthorityRecheckErrorV1> {
    assert_eq!(request.namespace_root_hash, self.expected_root_hash);
    assert_eq!(request.expected_authority_root_set_digest, self.expected_authority_root_set_digest);
    self.entered.wait();
    self.release.wait();
    Ok(RootRetirementAuthoritySnapshotV1 {
      target_is_authoritative: false,
      authority_root_set_digest: self.expected_authority_root_set_digest.clone(),
    })
  }
}

impl RootRetirementAuthorityVerifierV1 for ExactRootRetirementAuthorityVerifierV1 {
  fn recheck_authority_roots(
    &mut self,
    request: RootRetirementAuthorityRecheckRequestV1<'_>,
  ) -> Result<RootRetirementAuthoritySnapshotV1, RootRetirementAuthorityRecheckErrorV1> {
    self.called = true;
    assert_eq!(request.hash_algorithm, HashAlgorithm::Blake3_256);
    assert_eq!(request.database_id, [0x31; 16]);
    assert_eq!(request.namespace_root_hash, self.expected_root_hash);
    assert_eq!(request.expected_authority_root_set_digest, self.expected_authority_root_set_digest);
    assert_eq!(request.final_mark_generation, 5);
    Ok(RootRetirementAuthoritySnapshotV1 {
      target_is_authoritative: self.target_is_authoritative,
      authority_root_set_digest: self
        .returned_authority_root_set_digest
        .clone()
        .unwrap_or_else(|| self.expected_authority_root_set_digest.clone()),
    })
  }
}

struct FailingRootRetirementAuthorityVerifierV1 {
  called: bool,
}

impl RootRetirementAuthorityVerifierV1 for FailingRootRetirementAuthorityVerifierV1 {
  fn recheck_authority_roots(
    &mut self,
    _request: RootRetirementAuthorityRecheckRequestV1<'_>,
  ) -> Result<RootRetirementAuthoritySnapshotV1, RootRetirementAuthorityRecheckErrorV1> {
    self.called = true;
    Err(RootRetirementAuthorityRecheckErrorV1::new("root_authority_source_unavailable", "injected caller-owned authority source failure"))
  }
}

fn publish_empty_lifecycle_authority(
  publisher: &V4FirstAuthorityPublisher,
  retirement_owner: &mut RetirementJournalOwnerV1<'_>,
  slot: u8,
  sequence: u64,
  generation: u64,
  timestamp_ms: u64,
) -> EncodedImmutableGcArtifactV1 {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x31; 16];
  let authority_root_set_digest = digest_parts(algorithm, &[b"prior complete authority roots", &generation.to_le_bytes()]);
  let manifest = encode_root_lifecycle_manifest_v1(&RootLifecycleManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    generation,
    published_at_ms: i64::try_from(timestamp_ms).unwrap(),
    source_complete_mark_generation: generation,
    authority_root_set_digest: &authority_root_set_digest,
    candidate_directory_hash: None,
    root_expiry_manifest_hash: None,
    next_page_id: 1,
    candidate_count: 0,
    pending_count: 0,
    retired_evidence_count: 0,
    candidate_bytes: 0,
    expiry_bytes: 0,
  })
  .unwrap();
  publisher
    .publish_immutable_gc_artifact(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::RootLifecycleManifest,
        database_id: &database_id,
        artifact_key: &manifest.key,
        value: &manifest.value,
        minimum_timestamp_ms: timestamp_ms,
        committed_postcondition_code: "root_lifecycle_manifest_committed_postcondition",
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
    .unwrap();
  let control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::RootLifecycleActiveControl,
    hash_algorithm: algorithm,
    database_id: &database_id,
    slot,
    sequence,
    generation,
    target_manifest_hash: &manifest.key,
  })
  .unwrap();
  let outcome = publisher
    .publish_gc_active_control(
      GcControlPublicationRequestV1 {
        expected_control_kind: GcArtifactKindV1::RootLifecycleActiveControl,
        encoded_control: &control,
        publication_timestamp_ms: timestamp_ms,
        monotonic_now_ms: timestamp_ms,
      },
      retirement_owner,
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
    .unwrap();
  let GcControlPublicationOutcomeV1::Complete(publication) = outcome else {
    panic!("prior lifecycle control unexpectedly reported a committed failure");
  };
  assert_eq!(publication.control_slot, slot);
  assert!(!publication.idempotent);
  manifest
}

fn prepare_guarded_root_retirement(
  publisher: &mut V4FirstAuthorityPublisher,
  retirement_owner: &mut RetirementJournalOwnerV1<'_>,
  cancellation: &CancellationToken,
  memory: &Arc<MemoryCoordinator>,
  publish_support: bool,
) -> PreparedGuardedRootRetirementV1 {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x31; 16];
  let first_authority = publisher.publish(&request()).unwrap();
  let target_root_hash = first_authority.namespace_root.root_hash;
  let admission_commit_payload_hash = digest_parts(algorithm, &[&first_authority.admission_control]);
  publish_empty_lifecycle_authority(publisher, retirement_owner, 0, 1, 3, 1_700_000_050_000);
  let prior_lifecycle_manifest = publish_empty_lifecycle_authority(publisher, retirement_owner, 1, 2, 4, 1_700_000_060_000);
  assert_eq!(retirement_owner.status().pending_records, 0);

  let authority_root_set_digest = digest_parts(algorithm, &[b"complete authority roots after final mark"]);
  let committed_at_ms = 1_700_000_100_000i64;
  let retirement_commit = encode_root_retirement_commit_v1(&RootRetirementCommitWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    namespace_root_hash: &target_root_hash,
    retirement_id: &[0x81; 16],
    committed_at_ms,
    pending_since_ms: committed_at_ms - 86_400_000,
    grace_at_pending_ms: 86_400_000,
    final_mark_generation: 5,
    reason: 1,
    prior_lifecycle_manifest_hash: &prior_lifecycle_manifest.key,
    authority_root_set_digest: &authority_root_set_digest,
    admission_commit_payload_hash: &admission_commit_payload_hash,
  })
  .unwrap();
  let expiry_record = encode_root_expiry_record_v1(&RootExpiryRecordWriteV1 {
    hash_algorithm: algorithm,
    namespace_root_hash: &target_root_hash,
    retired_at_ms: committed_at_ms,
    last_pending_since_ms: committed_at_ms - 86_400_000,
    final_mark_generation: 5,
    reason: 1,
    state: RootExpiryStateV1::LogicallyRetired,
    retirement_commit_hash: &retirement_commit.key,
    root_object_reclaim_proof_hash: None,
    evidence_expires_at_ms: None,
  })
  .unwrap();
  let expiry_page = encode_gc_state_page_v1(&GcStatePageWriteV1 {
    hash_algorithm: algorithm,
    role: GcDirectoryRoleV1::RootExpiry,
    database_id: &database_id,
    catalog_id: &[0x71; 16],
    generation: 6,
    page_id: 1,
    records: &[&expiry_record],
  })
  .unwrap();
  let GcStateArtifactV1::Page(decoded_page) = decode_gc_state_artifact(&expiry_page.value, algorithm).unwrap() else {
    unreachable!();
  };
  let directory_entries = [GcStateDirectoryEntryWriteV1 {
    lower_fence: decoded_page.lower_fence,
    upper_fence: decoded_page.upper_fence,
    child_hash: &expiry_page.key,
    child_generation: decoded_page.generation,
    live_count: u64::from(decoded_page.record_count),
    tombstone_count: 0,
    page_count: 1,
    logical_bytes: decoded_page.logical_bytes,
    minimum_page_id: decoded_page.page_id,
    maximum_page_id: decoded_page.page_id,
    physical_hint: GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
  }];
  let expiry_directory = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role: GcDirectoryRoleV1::RootExpiry,
    database_id: &database_id,
    catalog_id: decoded_page.catalog_id,
    generation: 6,
    level: 0,
    entries: &directory_entries,
  })
  .unwrap();
  if publish_support {
    for artifact in [&expiry_page, &expiry_directory] {
      publisher
        .publish_root_lifecycle_support_artifact(RootLifecycleSupportPublicationRequestV1 {
          database_id: &database_id,
          artifact,
          publication_timestamp_ms: 1_700_000_100_001,
        })
        .unwrap();
    }
  }

  let logical_bytes = u64::try_from(expiry_record.len()).unwrap();
  let expiry_manifest = encode_root_expiry_manifest_v1(&RootExpiryManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    generation: 6,
    retention_ms: 30 * 24 * 60 * 60 * 1_000,
    optional_byte_budget: 256 * 1024 * 1024,
    directory_root_hash: Some(&expiry_directory.key),
    next_page_id: 2,
    record_count: 1,
    logical_bytes,
    mandatory_count: 1,
    mandatory_bytes: logical_bytes,
    optional_count: 0,
    optional_bytes: 0,
    oldest_retired_at_ms: Some(committed_at_ms),
    newest_retired_at_ms: Some(committed_at_ms),
  })
  .unwrap();
  let lifecycle_manifest = encode_root_lifecycle_manifest_v1(&RootLifecycleManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    generation: 6,
    published_at_ms: committed_at_ms + 1,
    source_complete_mark_generation: 5,
    authority_root_set_digest: &authority_root_set_digest,
    candidate_directory_hash: None,
    root_expiry_manifest_hash: Some(&expiry_manifest.key),
    next_page_id: 1,
    candidate_count: 0,
    pending_count: 0,
    retired_evidence_count: 1,
    candidate_bytes: 0,
    expiry_bytes: logical_bytes,
  })
  .unwrap();
  let retirement = decode_root_retirement_commit_v1(&retirement_commit.value, algorithm).unwrap();
  let expiry = decode_root_expiry_manifest_v1(&expiry_manifest.value, algorithm).unwrap();
  let lifecycle = decode_root_lifecycle_manifest_v1(&lifecycle_manifest.value, algorithm).unwrap();
  let mut closure_builder = RootLifecycleSupportClosureBuilderV1::new_for_retirement(
    &lifecycle,
    &expiry,
    &retirement,
    algorithm,
    cancellation,
    RootLifecycleSupportLimitsV1 { maximum_candidate_records: 0, maximum_expiry_records: 1, maximum_support_artifacts: 2 },
    memory,
  )
  .unwrap();
  closure_builder.observe_encoded(&expiry_page.value).unwrap();
  closure_builder.observe_encoded(&expiry_directory.value).unwrap();
  let support_closure = closure_builder.finish().unwrap();
  let lifecycle_control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::RootLifecycleActiveControl,
    hash_algorithm: algorithm,
    database_id: &database_id,
    slot: 0,
    sequence: 3,
    generation: 6,
    target_manifest_hash: &lifecycle_manifest.key,
  })
  .unwrap();
  let intent = RootRetirementIntentV1 {
    namespace_root_hash: target_root_hash.clone(),
    committed_at_ms,
    pending_since_ms: committed_at_ms - 86_400_000,
    grace_at_pending_ms: 86_400_000,
    final_mark_generation: 5,
    reason: 1,
    prior_lifecycle_manifest_hash: prior_lifecycle_manifest.key.clone(),
    authority_root_set_digest,
    admission_commit_payload_hash,
  };

  let observation = publisher.observe().unwrap();
  let mut successor = observation.selected.header.clone();
  successor.updated_at_ms += 1;
  successor.head_hash = digest_parts(algorithm, &[b"new current namespace root"]);
  publisher.header_publisher.publish_inactive_slot(&publisher.file, &observation, successor).unwrap();
  let pin_coordinator = RootReadPinCoordinatorV1::new(memory.clone(), algorithm, 16, 16).unwrap();

  PreparedGuardedRootRetirementV1 {
    target_root_hash,
    prior_lifecycle_manifest_key: prior_lifecycle_manifest.key,
    intent,
    support_closure,
    retirement_commit,
    expiry_manifest,
    lifecycle_manifest,
    lifecycle_control,
    pin_coordinator,
  }
}

fn selected_root_lifecycle_manifest_key(publisher: &V4FirstAuthorityPublisher) -> Vec<u8> {
  let observation = publisher.observe().unwrap();
  let kv = publisher.kv.lock().unwrap();
  select_root_lifecycle_control(&publisher.file, &kv, &observation.selected.header).unwrap().target_manifest_hash
}

fn corrupt_last_entity_byte(publisher: &V4FirstAuthorityPublisher, key: &[u8]) {
  let locator = publisher.locator(key).unwrap().expect("corruption target must be durably published");
  let offset = locator.offset + u64::from(locator.total_length) - 1;
  let mut file = publisher.file.try_clone().unwrap();
  file.seek(SeekFrom::Start(offset)).unwrap();
  let mut byte = [0u8; 1];
  file.read_exact(&mut byte).unwrap();
  byte[0] ^= 0x80;
  file.seek(SeekFrom::Start(offset)).unwrap();
  file.write_all(&byte).unwrap();
  file.sync_all().unwrap();
}

struct PreparedMarkCheckpointV1 {
  closure: DurableMarkWorkspaceClosureV1,
  checkpoint: EncodedImmutableGcArtifactV1,
  control: EncodedGcActiveControlV1,
}

fn prepare_mark_checkpoint(
  database_path: &Path,
  scratch_root: &Path,
  memory: &MemoryCoordinator,
  run_byte: u8,
  generation: u64,
  checkpoint_sequence: u64,
) -> PreparedMarkCheckpointV1 {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x31; 16];
  let run_id = [run_byte; 16];
  let identity = MarkWorkspaceIdentityV1::new(database_id, run_id, generation, checkpoint_sequence, algorithm).unwrap();
  let basis = MarkWorkspaceBasisV1::new(
    1,
    1_700_000_100_000 + checkpoint_sequence,
    1_700_000_100_500 + checkpoint_sequence,
    vec![0x51; algorithm.hash_length()],
    vec![0x11; algorithm.hash_length()],
    [0x71; 32],
  )
  .unwrap();
  let mut workspace = DurableMarkWorkspaceV1::create(
    database_path,
    identity,
    basis,
    MarkWorkspaceOptionsV1::new(Some(scratch_root.to_path_buf()), 64 * 1024 * 1024, 0).unwrap(),
    CancellationToken::new(),
    memory,
  )
  .unwrap();
  let closure = workspace.complete().unwrap();
  let mut capabilities = [0u8; 32];
  for bit in [12usize, 13, 14, 15, 17] {
    capabilities[bit / 8] |= 1 << (bit % 8);
  }
  let checkpoint = encode_mark_run_checkpoint(&MarkRunCheckpointWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    run_id: &run_id,
    generation,
    checkpoint_sequence,
    state: 1,
    phase: 1,
    resumable: true,
    canceled: false,
    capabilities,
    started_at_ms: 1_700_000_100_000 + checkpoint_sequence,
    updated_at_ms: 1_700_000_100_500 + checkpoint_sequence,
    authority_root_set_digest: &[0x11; 32],
    semantic_state_digest: &[0x31; 32],
    kv_layout_fingerprint: &[0x51; 32],
    effective_policy_fingerprint: [0x71; 32],
    system_family_registry_fingerprint: [0x91; 32],
    captured_header_sequence: 17,
    captured_write_high_water: 900,
    reconciled_through_sequence: 801,
    active_bitmap_bit_count: 512,
    kv_bucket_count: 8,
    kv_slots_per_bucket: 64,
    workspace_path: &closure.checkpoint_workspace_path().unwrap(),
    workspace_id: [run_byte.wrapping_add(0x20); 16],
    workspace_manifest_digest: closure.manifest_digest(),
    mutation_journal_head: &[0xB1; 32],
    checkpoint_logical_work: checkpoint_sequence * 1024,
    total_logical_work_hint: 64 * 1024 * 1024,
  })
  .unwrap();
  let control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::MarkRunActiveControl,
    hash_algorithm: algorithm,
    database_id: &database_id,
    slot: u8::try_from((checkpoint_sequence - 1) % 2).unwrap(),
    sequence: checkpoint_sequence,
    generation,
    target_manifest_hash: &checkpoint.key,
  })
  .unwrap();
  PreparedMarkCheckpointV1 { closure, checkpoint, control }
}

fn publish_mark_checkpoint(
  publisher: &mut V4FirstAuthorityPublisher,
  owner: &mut RetirementJournalOwnerV1<'_>,
  prepared: &PreparedMarkCheckpointV1,
  timestamp_ms: u64,
) -> MarkRunCheckpointPublicationReceiptV1 {
  publisher
    .publish_mark_run_checkpoint(
      MarkRunCheckpointPublicationRequestV1 {
        hash_algorithm: HashAlgorithm::Blake3_256,
        checkpoint: &prepared.checkpoint,
        control: &prepared.control,
        workspace: &prepared.closure,
        publication_timestamp_ms: timestamp_ms,
        monotonic_now_ms: timestamp_ms,
      },
      owner,
    )
    .unwrap()
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
fn root_lifecycle_and_mark_controls_share_one_kind_scoped_replacement_path() {
  let (_directory, _path, _coordinator, publisher) = create_environment("shared-gc-control", None);
  publish_first_authority(&publisher);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap());
  let cancellation = CancellationToken::new();
  let database_id = [0x31; 16];
  let mut owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    database_id,
    1,
    401,
    RetirementJournalBufferOptionsV1::new(8, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();

  let mut last_control = None;
  for sequence in 1..=3u64 {
    let generation = 500 + sequence;
    let timestamp_ms = 1_700_000_500_000 + sequence;
    let manifest = encode_root_lifecycle_manifest_v1(&RootLifecycleManifestWriteV1 {
      hash_algorithm: HashAlgorithm::Blake3_256,
      database_id: &database_id,
      generation,
      published_at_ms: i64::try_from(timestamp_ms).unwrap(),
      source_complete_mark_generation: generation,
      authority_root_set_digest: &[0x41; 32],
      candidate_directory_hash: None,
      root_expiry_manifest_hash: None,
      next_page_id: 1,
      candidate_count: 0,
      pending_count: 0,
      retired_evidence_count: 0,
      candidate_bytes: 0,
      expiry_bytes: 0,
    })
    .unwrap();
    publisher
      .publish_immutable_gc_artifact(
        ImmutableGcArtifactPublicationV1 {
          kind: GcArtifactKindV1::RootLifecycleManifest,
          database_id: &database_id,
          artifact_key: &manifest.key,
          value: &manifest.value,
          minimum_timestamp_ms: timestamp_ms,
          committed_postcondition_code: "root_lifecycle_manifest_committed_postcondition",
        },
        &mut NoopFirstAuthorityDependencyObserverV1,
      )
      .unwrap();
    let control = encode_gc_active_control(&GcActiveControlWriteV1 {
      kind: GcArtifactKindV1::RootLifecycleActiveControl,
      hash_algorithm: HashAlgorithm::Blake3_256,
      database_id: &database_id,
      slot: u8::try_from((sequence - 1) % 2).unwrap(),
      sequence,
      generation,
      target_manifest_hash: &manifest.key,
    })
    .unwrap();
    let outcome = publisher
      .publish_gc_active_control(
        GcControlPublicationRequestV1 {
          expected_control_kind: GcArtifactKindV1::RootLifecycleActiveControl,
          encoded_control: &control,
          publication_timestamp_ms: timestamp_ms,
          monotonic_now_ms: timestamp_ms,
        },
        &mut owner,
        &mut NoopFirstAuthorityDependencyObserverV1,
      )
      .unwrap();
    let GcControlPublicationOutcomeV1::Complete(publication) = outcome else {
      panic!("lifecycle control publication unexpectedly reported a committed failure");
    };
    assert_eq!(publication.control_slot, u8::try_from((sequence - 1) % 2).unwrap());
    assert_eq!(publication.replaced_control, sequence == 3);
    assert!(!publication.idempotent);
    last_control = Some((control, timestamp_ms));
  }

  assert_eq!(owner.status().pending_records, 1);
  let mark_control_key = gc_active_control_key(HashAlgorithm::Blake3_256, GcArtifactKindV1::MarkRunActiveControl, &database_id, 0).unwrap();
  assert!(publisher.locator(&mark_control_key).unwrap().is_none());
  let (last_control, timestamp_ms) = last_control.unwrap();
  let retry = publisher
    .publish_gc_active_control(
      GcControlPublicationRequestV1 {
        expected_control_kind: GcArtifactKindV1::RootLifecycleActiveControl,
        encoded_control: &last_control,
        publication_timestamp_ms: timestamp_ms,
        monotonic_now_ms: timestamp_ms,
      },
      &mut owner,
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
    .unwrap();
  let GcControlPublicationOutcomeV1::Complete(retry) = retry else {
    panic!("exact lifecycle control retry unexpectedly reported a committed failure");
  };
  assert!(retry.idempotent);
  assert_eq!(owner.status().pending_records, 1);

  let before_wrong_kind = publisher.observe().unwrap();
  let wrong_kind = publisher
    .publish_gc_active_control(
      GcControlPublicationRequestV1 {
        expected_control_kind: GcArtifactKindV1::MarkRunActiveControl,
        encoded_control: &last_control,
        publication_timestamp_ms: timestamp_ms,
        monotonic_now_ms: timestamp_ms,
      },
      &mut owner,
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
    .unwrap_err();
  assert_eq!(wrong_kind.code(), "gc_control_kind");
  assert_eq!(publisher.observe().unwrap(), before_wrong_kind);
}

#[test]
fn guarded_root_retirement_selects_control_last_and_exact_retry_does_not_recheck_stale_authority() {
  let (_directory, path, coordinator, mut publisher) = create_environment("guarded-root-retirement", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
  let target_locator = publisher.locator(&prepared.target_root_hash).unwrap().unwrap();
  let target_admission_locator = publisher.admission_locator(&prepared.target_root_hash).unwrap().unwrap();
  let file_length_before = std::fs::metadata(&path).unwrap().len();
  let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
    called: false,
    expected_root_hash: prepared.target_root_hash.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
    returned_authority_root_set_digest: None,
    target_is_authoritative: false,
  };

  let receipt = publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap();

  assert!(authority_verifier.called);
  assert!(!receipt.idempotent);
  assert_eq!(receipt.lifecycle_control_slot, 0);
  assert!(receipt.retirement_commit_write_sequence < receipt.expiry_manifest_write_sequence);
  assert!(receipt.expiry_manifest_write_sequence < receipt.lifecycle_manifest_write_sequence);
  assert!(receipt.lifecycle_manifest_write_sequence < receipt.lifecycle_control_write_sequence);
  assert!(matches!(receipt.lineage_state, RootRetirementLineageStateV1::HardPublished { .. }));
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.lifecycle_manifest.key);
  assert_eq!(publisher.locator(&prepared.target_root_hash).unwrap().unwrap(), target_locator);
  assert_eq!(publisher.admission_locator(&prepared.target_root_hash).unwrap().unwrap(), target_admission_locator);
  assert!(publisher.locator(&prepared.retirement_commit.key).unwrap().is_some());
  assert!(publisher.locator(&prepared.expiry_manifest.key).unwrap().is_some());
  assert!(publisher.locator(&prepared.lifecycle_manifest.key).unwrap().is_some());
  assert!(publisher.locator(&prepared.lifecycle_control.key).unwrap().is_some());
  assert!(std::fs::metadata(&path).unwrap().len() >= file_length_before);
  assert_eq!(retirement_owner.status().pending_records, 0);
  assert_eq!(retirement_owner.status().durable_records, 1);

  let before_retry = publisher.observe().unwrap();
  let before_retry_frontier = coordinator.snapshot().unwrap().hard_frontier;
  let before_retry_locators = [
    publisher.locator(&prepared.retirement_commit.key).unwrap().unwrap(),
    publisher.locator(&prepared.expiry_manifest.key).unwrap().unwrap(),
    publisher.locator(&prepared.lifecycle_manifest.key).unwrap().unwrap(),
    publisher.locator(&prepared.lifecycle_control.key).unwrap().unwrap(),
  ];
  authority_verifier.called = false;
  authority_verifier.target_is_authoritative = true;
  let retry = publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap();

  assert!(retry.idempotent);
  assert!(!authority_verifier.called, "exact selected retry must not depend on stale caller authority");
  assert!(matches!(retry.lineage_state, RootRetirementLineageStateV1::NotRequired));
  assert_eq!(publisher.observe().unwrap(), before_retry);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, before_retry_frontier);
  assert_eq!(
    [
      publisher.locator(&prepared.retirement_commit.key).unwrap().unwrap(),
      publisher.locator(&prepared.expiry_manifest.key).unwrap().unwrap(),
      publisher.locator(&prepared.lifecycle_manifest.key).unwrap().unwrap(),
      publisher.locator(&prepared.lifecycle_control.key).unwrap().unwrap(),
    ],
    before_retry_locators,
  );
}

#[test]
fn guarded_root_retirement_requires_the_exact_support_closure_to_be_durable_before_exclusion() {
  let (_directory, _path, _coordinator, mut publisher) = create_environment("root-retirement-missing-support", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, false);
  let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
    called: false,
    expected_root_hash: prepared.target_root_hash.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
    returned_authority_root_set_digest: None,
    target_is_authoritative: false,
  };

  let error =
    publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap_err();

  assert_eq!(error.code(), "root_retirement_support_missing");
  assert!(!authority_verifier.called);
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.prior_lifecycle_manifest_key);
  assert!(publisher.locator(&prepared.retirement_commit.key).unwrap().is_none());
  assert!(publisher.locator(&prepared.expiry_manifest.key).unwrap().is_none());
  assert!(publisher.locator(&prepared.lifecycle_manifest.key).unwrap().is_none());
}

#[test]
fn guarded_root_retirement_refuses_active_read_pins_before_final_authority_recheck() {
  let (_directory, _path, _coordinator, mut publisher) = create_environment("root-retirement-active-pin", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
  let read = prepared
    .pin_coordinator
    .admit_read(&prepared.target_root_hash, &cancellation, || {
      Ok(RootLifecycleObservationV1::PendingDelete {
        pending_since_ms: prepared.intent.pending_since_ms,
        grace_at_pending_ms: prepared.intent.grace_at_pending_ms,
        current_configured_grace_ms: prepared.intent.grace_at_pending_ms,
      })
    })
    .unwrap();
  let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
    called: false,
    expected_root_hash: prepared.target_root_hash.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
    returned_authority_root_set_digest: None,
    target_is_authoritative: false,
  };

  let error =
    publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap_err();

  assert_eq!(error.code(), "root_pinned");
  assert!(!authority_verifier.called);
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.prior_lifecycle_manifest_key);
  assert!(publisher.locator(&prepared.retirement_commit.key).unwrap().is_none());
  assert!(publisher.locator(&prepared.lifecycle_control.key).unwrap().is_some());
  drop(read);
  assert_eq!(prepared.pin_coordinator.active_pin_count().unwrap(), 0);
  assert_eq!(prepared.pin_coordinator.tracked_root_count().unwrap(), 0);
}

#[test]
fn guarded_root_retirement_refuses_authoritative_or_changed_root_sets_before_publication() {
  for case in ["target-authoritative", "authority-digest-changed"] {
    let (_directory, _path, _coordinator, mut publisher) = create_environment(case, None);
    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
    let cancellation = CancellationToken::new();
    let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      [0x31; 16],
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &cancellation,
      &memory,
    )
    .unwrap();
    let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
    let returned_authority_root_set_digest = if case == "authority-digest-changed" {
      digest_parts(HashAlgorithm::Blake3_256, &[b"changed caller authority roots"])
    } else {
      prepared.intent.authority_root_set_digest.clone()
    };
    let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
      called: false,
      expected_root_hash: prepared.target_root_hash.clone(),
      expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
      returned_authority_root_set_digest: Some(returned_authority_root_set_digest),
      target_is_authoritative: case == "target-authoritative",
    };

    let error =
      publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap_err();

    assert_eq!(error.code(), "root_retirement_authority_changed", "case {case}");
    assert!(authority_verifier.called, "case {case}");
    assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.prior_lifecycle_manifest_key, "case {case}");
    assert!(publisher.locator(&prepared.retirement_commit.key).unwrap().is_none(), "case {case}");
    assert!(publisher.locator(&prepared.expiry_manifest.key).unwrap().is_none(), "case {case}");
    assert!(publisher.locator(&prepared.lifecycle_manifest.key).unwrap().is_none(), "case {case}");
  }
}

#[test]
fn guarded_root_retirement_propagates_authority_source_failure_without_selecting_retirement() {
  let (_directory, _path, _coordinator, mut publisher) = create_environment("root-retirement-authority-failure", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
  let mut authority_verifier = FailingRootRetirementAuthorityVerifierV1 { called: false };

  let error =
    publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap_err();

  assert_eq!(error.code(), "root_authority_source_unavailable");
  assert!(authority_verifier.called);
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.prior_lifecycle_manifest_key);
  assert!(publisher.locator(&prepared.retirement_commit.key).unwrap().is_none());
  assert!(publisher.locator(&prepared.lifecycle_control.key).unwrap().is_some());
}

#[test]
fn guarded_root_retirement_cancellation_refuses_before_support_scan_or_authority_callback() {
  let (_directory, _path, _coordinator, mut publisher) = create_environment("root-retirement-canceled", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
  let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
    called: false,
    expected_root_hash: prepared.target_root_hash.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
    returned_authority_root_set_digest: None,
    target_is_authoritative: false,
  };
  cancellation.cancel();

  let error =
    publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap_err();

  assert_eq!(error.code(), "root_retirement_canceled");
  assert!(!authority_verifier.called);
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.prior_lifecycle_manifest_key);
  assert!(publisher.locator(&prepared.retirement_commit.key).unwrap().is_none());
}

#[test]
fn guarded_root_retirement_refuses_when_selected_prior_lifecycle_advances() {
  let (_directory, _path, _coordinator, mut publisher) = create_environment("root-retirement-prior-advanced", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
  let advanced = publish_empty_lifecycle_authority(&publisher, &mut retirement_owner, 0, 3, 5, 1_700_000_090_000);
  retirement_owner.flush(&mut publisher).unwrap();
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), advanced.key);
  let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
    called: false,
    expected_root_hash: prepared.target_root_hash.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
    returned_authority_root_set_digest: None,
    target_is_authoritative: false,
  };

  let error =
    publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap_err();

  assert_eq!(error.code(), "root_retirement_prior_lifecycle_changed");
  assert!(!authority_verifier.called);
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), advanced.key);
  assert!(publisher.locator(&prepared.retirement_commit.key).unwrap().is_none());
  assert!(publisher.locator(&prepared.lifecycle_manifest.key).unwrap().is_none());
}

#[test]
fn root_retirement_failure_before_selector_keeps_prior_lifecycle_selected_across_restart() {
  for phase in [DependencyFailurePhase::BeforeEntity, DependencyFailurePhase::EntityWritten, DependencyFailurePhase::EntityStaged] {
    let (_directory, path, coordinator, mut publisher) = create_environment(&format!("root-retirement-before-selector-{phase:?}"), None);
    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
    let cancellation = CancellationToken::new();
    let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      [0x31; 16],
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &cancellation,
      &memory,
    )
    .unwrap();
    let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
    let old_control_locator = publisher.locator(&prepared.lifecycle_control.key).unwrap().unwrap();
    let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
      called: false,
      expected_root_hash: prepared.target_root_hash.clone(),
      expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
      returned_authority_root_set_digest: None,
      target_is_authoritative: false,
    };
    let mut observer = FailingDependencyObserver { phase, entity_index: 0 };

    let error = publisher
      .publish_root_retirement_with_control_observer(
        prepared.request(&cancellation),
        &mut authority_verifier,
        &mut retirement_owner,
        &mut observer,
      )
      .unwrap_err();

    assert_eq!(error.code(), "durability_failure", "phase {phase:?}");
    assert!(error.committed_receipt().is_none(), "phase {phase:?}");
    assert!(authority_verifier.called, "phase {phase:?}");
    assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.prior_lifecycle_manifest_key, "phase {phase:?}");
    assert_eq!(publisher.locator(&prepared.lifecycle_control.key).unwrap().unwrap(), old_control_locator, "phase {phase:?}");
    assert!(publisher.locator(&prepared.retirement_commit.key).unwrap().is_some(), "phase {phase:?}");
    assert!(publisher.locator(&prepared.expiry_manifest.key).unwrap().is_some(), "phase {phase:?}");
    assert!(publisher.locator(&prepared.lifecycle_manifest.key).unwrap().is_some(), "phase {phase:?}");
    assert_eq!(retirement_owner.status().pending_records, 0, "phase {phase:?}");
    assert_eq!(retirement_owner.status().durable_records, 0, "phase {phase:?}");
    assert!(coordinator.hard_failure().unwrap().is_some(), "phase {phase:?}");
    drop(retirement_owner);
    drop(publisher);

    let (_restart_coordinator, mut reopened) = reopen(&path);
    assert_eq!(selected_root_lifecycle_manifest_key(&reopened), prepared.prior_lifecycle_manifest_key, "phase {phase:?}");
    let retry_cancellation = CancellationToken::new();
    let mut retry_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      [0x31; 16],
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &retry_cancellation,
      &memory,
    )
    .unwrap();
    authority_verifier.called = false;
    let retry = reopened.publish_root_retirement(prepared.request(&retry_cancellation), &mut authority_verifier, &mut retry_owner).unwrap();
    assert!(authority_verifier.called, "phase {phase:?}");
    assert!(!retry.idempotent, "phase {phase:?}");
    assert_eq!(selected_root_lifecycle_manifest_key(&reopened), prepared.lifecycle_manifest.key, "phase {phase:?}");
    assert!(matches!(retry.lineage_state, RootRetirementLineageStateV1::HardPublished { .. }), "phase {phase:?}");
  }
}

#[test]
fn every_final_selector_header_failure_restarts_as_exactly_pending_or_retired_and_retains_uncertain_lineage() {
  let failures = [
    FirstAuthorityFailurePoint::DataBarrier,
    FirstAuthorityFailurePoint::HeaderWriteBefore,
    FirstAuthorityFailurePoint::HeaderWriteAfter,
    FirstAuthorityFailurePoint::FullBarrier,
    FirstAuthorityFailurePoint::Verify,
  ];
  for failure in failures {
    let (_directory, path, coordinator, mut publisher) = create_environment(&format!("root-retirement-selector-{failure:?}"), None);
    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
    let cancellation = CancellationToken::new();
    let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      [0x31; 16],
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &cancellation,
      &memory,
    )
    .unwrap();
    let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
    publisher = V4FirstAuthorityPublisher {
      file: publisher.file,
      kv: publisher.kv,
      header_publisher: DatabaseHeaderPublisherV4::with_io(coordinator.clone(), Arc::new(NthHeaderPublicationFaultIo::new(failure, 5))),
      root_state: publisher.root_state,
    };
    let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
      called: false,
      expected_root_hash: prepared.target_root_hash.clone(),
      expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
      returned_authority_root_set_digest: None,
      target_is_authoritative: false,
    };

    let error =
      publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap_err();

    assert!(authority_verifier.called, "failure {failure:?}");
    assert!(coordinator.hard_failure().unwrap().is_some(), "failure {failure:?}");
    let selector_may_have_committed = matches!(
      failure,
      FirstAuthorityFailurePoint::HeaderWriteAfter | FirstAuthorityFailurePoint::FullBarrier | FirstAuthorityFailurePoint::Verify
    );
    if selector_may_have_committed {
      let receipt = error.committed_receipt().expect("a selected uncertain lifecycle control needs an exact committed receipt");
      assert_eq!(receipt.lifecycle_manifest_key, prepared.lifecycle_manifest.key, "failure {failure:?}");
      assert!(matches!(receipt.lineage_state, RootRetirementLineageStateV1::BufferedAfterFlushFailure { .. }), "failure {failure:?}");
      assert_eq!(retirement_owner.status().pending_records, 1, "failure {failure:?}");
    } else {
      assert!(error.committed_receipt().is_none(), "failure {failure:?}");
      assert_eq!(retirement_owner.status().pending_records, 0, "failure {failure:?}");
    }
    drop(retirement_owner);
    drop(publisher);

    let (_restart_coordinator, mut reopened) = reopen(&path);
    let selected_manifest = selected_root_lifecycle_manifest_key(&reopened);
    let expected_manifest =
      if selector_may_have_committed { &prepared.lifecycle_manifest.key } else { &prepared.prior_lifecycle_manifest_key };
    assert_eq!(&selected_manifest, expected_manifest, "failure {failure:?}");
    let retry_cancellation = CancellationToken::new();
    let mut retry_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      [0x31; 16],
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &retry_cancellation,
      &memory,
    )
    .unwrap();
    authority_verifier.called = false;
    let retry = reopened.publish_root_retirement(prepared.request(&retry_cancellation), &mut authority_verifier, &mut retry_owner).unwrap();
    assert_eq!(retry.idempotent, selector_may_have_committed, "failure {failure:?}");
    assert_eq!(authority_verifier.called, !selector_may_have_committed, "failure {failure:?}");
    assert_eq!(selected_root_lifecycle_manifest_key(&reopened), prepared.lifecycle_manifest.key, "failure {failure:?}");
  }
}

#[test]
fn racing_read_pin_cannot_enter_until_retirement_selects_the_new_lifecycle() {
  let (_directory, _path, _coordinator, mut publisher) = create_environment("root-retirement-racing-pin", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
  let verifier_entered = Arc::new(Barrier::new(2));
  let verifier_release = Arc::new(Barrier::new(2));
  let mut authority_verifier = BlockingRootRetirementAuthorityVerifierV1 {
    entered: verifier_entered.clone(),
    release: verifier_release.clone(),
    expected_root_hash: prepared.target_root_hash.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
  };
  let pin_started = Arc::new(Barrier::new(2));
  let (lifecycle_callback_sender, lifecycle_callback_receiver) = mpsc::channel();

  std::thread::scope(|scope| {
    let retirement =
      scope.spawn(|| publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner));
    verifier_entered.wait();

    let pin_coordinator = prepared.pin_coordinator.clone();
    let pin_root = prepared.target_root_hash.clone();
    let pin_started_thread = pin_started.clone();
    let pin_cancellation = CancellationToken::new();
    let pin = scope.spawn(move || {
      pin_started_thread.wait();
      pin_coordinator.admit_read(&pin_root, &pin_cancellation, || {
        lifecycle_callback_sender.send(()).unwrap();
        Ok(RootLifecycleObservationV1::LogicallyRetired)
      })
    });
    pin_started.wait();
    assert!(
      matches!(lifecycle_callback_receiver.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Timeout)),
      "a new read reached lifecycle admission while retirement held the root exclusion"
    );

    verifier_release.wait();
    let retirement_receipt = retirement.join().unwrap().unwrap();
    lifecycle_callback_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
    let pin_error = pin.join().unwrap().unwrap_err();
    assert_eq!(pin_error.code(), "root_expired");
    assert_eq!(retirement_receipt.lifecycle_manifest_key, prepared.lifecycle_manifest.key);
  });

  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.lifecycle_manifest.key);
  assert_eq!(prepared.pin_coordinator.active_pin_count().unwrap(), 0);
  assert_eq!(prepared.pin_coordinator.tracked_root_count().unwrap(), 0);
}

#[test]
fn post_selector_lineage_failure_returns_the_exact_committed_retirement_receipt() {
  let (_directory, path, _coordinator, mut publisher) = create_environment("root-retirement-buffered-lineage", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
  let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
    called: false,
    expected_root_hash: prepared.target_root_hash.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
    returned_authority_root_set_digest: None,
    target_is_authoritative: false,
  };
  let mut observer = CancelRetirementAfterCommitObserver { cancellation: cancellation.clone() };

  let error = publisher
    .publish_root_retirement_with_control_observer(
      prepared.request(&cancellation),
      &mut authority_verifier,
      &mut retirement_owner,
      &mut observer,
    )
    .unwrap_err();

  assert_eq!(error.code(), "root_retirement_committed_lineage");
  let receipt = error.committed_receipt().expect("selected lifecycle authority requires a committed receipt");
  assert_eq!(receipt.lifecycle_manifest_key, prepared.lifecycle_manifest.key);
  assert!(matches!(
    receipt.lineage_state,
    RootRetirementLineageStateV1::BufferedAfterFlushFailure { code: "retirement_journal_cancelled", .. }
  ));
  assert!(authority_verifier.called);
  assert_eq!(retirement_owner.status().pending_records, 1);
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.lifecycle_manifest.key);
  drop(retirement_owner);
  drop(publisher);

  let (_restart_coordinator, mut reopened) = reopen(&path);
  assert_eq!(selected_root_lifecycle_manifest_key(&reopened), prepared.lifecycle_manifest.key);
  let retry_cancellation = CancellationToken::new();
  let mut retry_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &retry_cancellation,
    &memory,
  )
  .unwrap();
  authority_verifier.called = false;
  let retry = reopened.publish_root_retirement(prepared.request(&retry_cancellation), &mut authority_verifier, &mut retry_owner).unwrap();
  assert!(retry.idempotent);
  assert!(!authority_verifier.called);
}

#[test]
fn post_selector_pin_cleanup_failure_returns_the_exact_committed_retirement_receipt() {
  let (_directory, path, _coordinator, mut publisher) = create_environment("root-retirement-pin-cleanup", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
  let mut authority_verifier = CleanupFailingRootRetirementAuthorityVerifierV1 {
    pin_coordinator: prepared.pin_coordinator.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
  };

  let error =
    publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap_err();

  assert_eq!(error.code(), "root_retirement_committed_pin_cleanup");
  let receipt = error.committed_receipt().expect("pin cleanup failure happened after lifecycle selection");
  assert_eq!(receipt.lifecycle_manifest_key, prepared.lifecycle_manifest.key);
  assert!(matches!(receipt.lineage_state, RootRetirementLineageStateV1::HardPublished { .. }));
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.lifecycle_manifest.key);
  drop(retirement_owner);
  drop(publisher);

  let (_restart_coordinator, reopened) = reopen(&path);
  assert_eq!(selected_root_lifecycle_manifest_key(&reopened), prepared.lifecycle_manifest.key);
}

#[test]
fn selector_uncertainty_and_pin_cleanup_failure_preserve_the_receipt_and_both_diagnostics() {
  let (_directory, path, coordinator, mut publisher) = create_environment("root-retirement-selector-and-pin-cleanup", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
  publisher = V4FirstAuthorityPublisher {
    file: publisher.file,
    kv: publisher.kv,
    header_publisher: DatabaseHeaderPublisherV4::with_io(
      coordinator,
      Arc::new(NthHeaderPublicationFaultIo::new(FirstAuthorityFailurePoint::Verify, 5)),
    ),
    root_state: publisher.root_state,
  };
  let mut authority_verifier = CleanupFailingRootRetirementAuthorityVerifierV1 {
    pin_coordinator: prepared.pin_coordinator.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
  };

  let error =
    publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap_err();

  assert_eq!(error.code(), "gc_control_committed_authority_uncertain");
  let receipt = error.committed_receipt().expect("combined post-selector failures must preserve the committed retirement receipt");
  assert_eq!(receipt.lifecycle_manifest_key, prepared.lifecycle_manifest.key);
  assert!(matches!(receipt.lineage_state, RootRetirementLineageStateV1::BufferedAfterFlushFailure { .. }));
  assert!(error.to_string().contains("releasing the root retirement exclusion also failed"));
  drop(retirement_owner);
  drop(publisher);

  let (_restart_coordinator, reopened) = reopen(&path);
  assert_eq!(selected_root_lifecycle_manifest_key(&reopened), prepared.lifecycle_manifest.key);
}

#[test]
fn corrupt_prior_control_manifest_or_support_locator_cannot_advance_logical_retirement() {
  for case in ["control", "manifest", "support"] {
    let (_directory, _path, _coordinator, mut publisher) = create_environment(&format!("root-retirement-corrupt-{case}"), None);
    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
    let cancellation = CancellationToken::new();
    let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      [0x31; 16],
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &cancellation,
      &memory,
    )
    .unwrap();
    let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
    let corrupt_key = match case {
      "control" => gc_active_control_key(HashAlgorithm::Blake3_256, GcArtifactKindV1::RootLifecycleActiveControl, &[0x31; 16], 1).unwrap(),
      "manifest" => prepared.prior_lifecycle_manifest_key.clone(),
      "support" => prepared.support_closure.expiry_directory_hash().unwrap().to_vec(),
      _ => unreachable!(),
    };
    corrupt_last_entity_byte(&publisher, &corrupt_key);
    let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
      called: false,
      expected_root_hash: prepared.target_root_hash.clone(),
      expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
      returned_authority_root_set_digest: None,
      target_is_authoritative: false,
    };

    let error =
      publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap_err();

    assert!(error.committed_receipt().is_none(), "case {case}");
    assert!(!authority_verifier.called, "case {case}");
    assert!(publisher.locator(&prepared.retirement_commit.key).unwrap().is_none(), "case {case}");
    assert!(publisher.locator(&prepared.expiry_manifest.key).unwrap().is_none(), "case {case}");
    assert!(publisher.locator(&prepared.lifecycle_manifest.key).unwrap().is_none(), "case {case}");
    assert_eq!(retirement_owner.status().pending_records, 0, "case {case}");
  }
}

#[test]
fn root_retirement_failure_after_selector_reports_committed_and_restarts_as_retired() {
  let (_directory, path, coordinator, mut publisher) = create_environment("root-retirement-after-selector", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
  let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
    called: false,
    expected_root_hash: prepared.target_root_hash.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
    returned_authority_root_set_digest: None,
    target_is_authoritative: false,
  };
  let mut observer = FailingPostCommitObserver;

  let error = publisher
    .publish_root_retirement_with_control_observer(
      prepared.request(&cancellation),
      &mut authority_verifier,
      &mut retirement_owner,
      &mut observer,
    )
    .unwrap_err();

  assert_eq!(error.code(), "gc_control_committed_postcondition_failure");
  let committed = error.committed_receipt().expect("selected lifecycle control must return an exact committed receipt");
  assert!(authority_verifier.called);
  assert!(!committed.idempotent);
  assert!(matches!(committed.lineage_state, RootRetirementLineageStateV1::HardPublished { .. }));
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.lifecycle_manifest.key);
  assert_eq!(retirement_owner.status().pending_records, 0);
  assert_eq!(retirement_owner.status().durable_records, 1);
  assert!(coordinator.hard_failure().unwrap().is_none());
  drop(retirement_owner);
  drop(publisher);

  let (_restart_coordinator, mut reopened) = reopen(&path);
  assert_eq!(selected_root_lifecycle_manifest_key(&reopened), prepared.lifecycle_manifest.key);
  let retry_cancellation = CancellationToken::new();
  let mut retry_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    2,
    402,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &retry_cancellation,
    &memory,
  )
  .unwrap();
  authority_verifier.called = false;
  authority_verifier.target_is_authoritative = true;
  let retry = reopened.publish_root_retirement(prepared.request(&retry_cancellation), &mut authority_verifier, &mut retry_owner).unwrap();
  assert!(retry.idempotent);
  assert!(!authority_verifier.called);
}

#[test]
fn mark_control_post_commit_failure_returns_exact_receipt_and_hard_lineage() {
  let (directory, path, _coordinator, mut publisher) = create_environment("mark-control-post-commit", None);
  publish_first_authority(&publisher);
  let scratch_root = directory.path().join("mark-scratch");
  std::fs::create_dir(&scratch_root).unwrap();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap());
  let cancellation = CancellationToken::new();
  let mut owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let first = prepare_mark_checkpoint(&path, &scratch_root, &memory, 0x51, 101, 1);
  let _first_receipt = publish_mark_checkpoint(&mut publisher, &mut owner, &first, 1_700_000_200_001);
  let second = prepare_mark_checkpoint(&path, &scratch_root, &memory, 0x52, 102, 2);
  let _second_receipt = publish_mark_checkpoint(&mut publisher, &mut owner, &second, 1_700_000_200_002);
  let replacement = prepare_mark_checkpoint(&path, &scratch_root, &memory, 0x53, 103, 3);
  let mut observer = FailingPostCommitObserver;

  let error = publisher
    .publish_mark_run_checkpoint_with_control_observer(
      MarkRunCheckpointPublicationRequestV1 {
        hash_algorithm: HashAlgorithm::Blake3_256,
        checkpoint: &replacement.checkpoint,
        control: &replacement.control,
        workspace: &replacement.closure,
        publication_timestamp_ms: 1_700_000_200_003,
        monotonic_now_ms: 1_700_000_200_003,
      },
      &mut owner,
      &mut observer,
    )
    .unwrap_err();

  assert_eq!(error.code(), "mark_checkpoint_control_committed_postcondition_failure");
  let committed = error.committed_receipt().expect("selected mark control must return its exact committed receipt");
  assert_eq!(committed.control_slot, 0);
  assert!(committed.replaced_control);
  assert!(!committed.idempotent);
  assert!(matches!(committed.lineage_state, MarkRunCheckpointLineageStateV1::HardPublished { .. }));
  assert_eq!(owner.status().pending_records, 0);
  assert_eq!(owner.status().durable_records, 1);
  let committed_locator = publisher.locator(&replacement.control.key).unwrap().unwrap();
  assert_eq!(committed_locator.type_flags, kv_tag::GC_ARTIFACT);
  assert!(committed_locator.offset < committed.observation.selected.header.hot_tail_offset);

  let before_retry = publisher.observe().unwrap();
  let retry = publish_mark_checkpoint(&mut publisher, &mut owner, &replacement, 1_700_000_200_003);
  assert!(retry.idempotent);
  assert_eq!(publisher.observe().unwrap(), before_retry);
}

#[test]
fn mark_control_activation_failure_discards_soft_lineage_and_keeps_old_control_selected() {
  let (directory, path, coordinator, mut publisher) = create_environment("mark-control-pre-commit", None);
  publish_first_authority(&publisher);
  let scratch_root = directory.path().join("mark-scratch");
  std::fs::create_dir(&scratch_root).unwrap();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap());
  let cancellation = CancellationToken::new();
  let mut owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let first = prepare_mark_checkpoint(&path, &scratch_root, &memory, 0x61, 201, 1);
  let _first_receipt = publish_mark_checkpoint(&mut publisher, &mut owner, &first, 1_700_000_300_001);
  let second = prepare_mark_checkpoint(&path, &scratch_root, &memory, 0x62, 202, 2);
  let _second_receipt = publish_mark_checkpoint(&mut publisher, &mut owner, &second, 1_700_000_300_002);
  let replacement = prepare_mark_checkpoint(&path, &scratch_root, &memory, 0x63, 203, 3);
  let old_locator = publisher.locator(&first.control.key).unwrap().unwrap();
  let mut observer = FailingDependencyObserver { phase: DependencyFailurePhase::BeforeEntity, entity_index: 0 };

  let error = publisher
    .publish_mark_run_checkpoint_with_control_observer(
      MarkRunCheckpointPublicationRequestV1 {
        hash_algorithm: HashAlgorithm::Blake3_256,
        checkpoint: &replacement.checkpoint,
        control: &replacement.control,
        workspace: &replacement.closure,
        publication_timestamp_ms: 1_700_000_300_003,
        monotonic_now_ms: 1_700_000_300_003,
      },
      &mut owner,
      &mut observer,
    )
    .unwrap_err();

  assert_eq!(error.code(), "durability_failure");
  assert!(error.committed_receipt().is_none());
  assert_eq!(publisher.locator(&replacement.control.key).unwrap().unwrap(), old_locator);
  assert_eq!(owner.status().pending_records, 0);
  assert_eq!(owner.status().durable_records, 0);
  assert!(coordinator.hard_failure().unwrap().is_some());
  let interrupted = publisher.observe().unwrap();
  let interrupted_length = std::fs::metadata(&path).unwrap().len();
  assert!(interrupted_length > interrupted.selected.header.hot_tail_offset);
  let reserved_write_sequence = interrupted.selected.header.write_sequence_high_water;
  drop(owner);
  drop(publisher);

  let (_restart_coordinator, mut reopened) = reopen(&path);
  assert_eq!(reopened.locator(&replacement.control.key).unwrap().unwrap(), old_locator);
  let retry_cancellation = CancellationToken::new();
  let mut retry_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &retry_cancellation,
    &memory,
  )
  .unwrap();
  let retry = publish_mark_checkpoint(&mut reopened, &mut retry_owner, &replacement, 1_700_000_300_003);
  assert_eq!(retry.control_write_sequence, reserved_write_sequence + 1);
  assert!(reopened.locator(&replacement.control.key).unwrap().unwrap().offset >= interrupted_length);
  assert_eq!(retry_owner.status().pending_records, 0);
  assert_eq!(retry_owner.status().durable_records, 1);
}

#[test]
fn mark_control_surfaces_buffered_lineage_when_immediate_post_commit_flush_fails() {
  let (directory, path, _coordinator, mut publisher) = create_environment("mark-control-buffered-lineage", None);
  publish_first_authority(&publisher);
  let scratch_root = directory.path().join("mark-scratch");
  std::fs::create_dir(&scratch_root).unwrap();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap());
  let cancellation = CancellationToken::new();
  let mut owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let first = prepare_mark_checkpoint(&path, &scratch_root, &memory, 0x71, 301, 1);
  let _first_receipt = publish_mark_checkpoint(&mut publisher, &mut owner, &first, 1_700_000_400_001);
  let second = prepare_mark_checkpoint(&path, &scratch_root, &memory, 0x72, 302, 2);
  let _second_receipt = publish_mark_checkpoint(&mut publisher, &mut owner, &second, 1_700_000_400_002);
  let replacement = prepare_mark_checkpoint(&path, &scratch_root, &memory, 0x73, 303, 3);
  let mut observer = CancelRetirementAfterCommitObserver { cancellation: cancellation.clone() };

  let receipt = publisher
    .publish_mark_run_checkpoint_with_control_observer(
      MarkRunCheckpointPublicationRequestV1 {
        hash_algorithm: HashAlgorithm::Blake3_256,
        checkpoint: &replacement.checkpoint,
        control: &replacement.control,
        workspace: &replacement.closure,
        publication_timestamp_ms: 1_700_000_400_003,
        monotonic_now_ms: 1_700_000_400_003,
      },
      &mut owner,
      &mut observer,
    )
    .unwrap();

  assert!(receipt.replaced_control);
  assert!(matches!(
    receipt.lineage_state,
    MarkRunCheckpointLineageStateV1::BufferedAfterFlushFailure { code: "retirement_journal_cancelled", .. }
  ));
  assert_eq!(owner.status().pending_records, 1);
  assert_eq!(owner.status().durable_records, 0);
  assert_eq!(publisher.locator(&replacement.control.key).unwrap().unwrap().type_flags, kv_tag::GC_ARTIFACT);
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
