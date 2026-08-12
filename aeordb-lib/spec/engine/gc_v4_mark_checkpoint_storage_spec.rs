use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::hot_tail::read_hot_tail_checked;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::entity::{EntryTypeV4, WHOLE_ENTITY_V1_FLAG_SYSTEM, WholeEntityWriteV1, decode_whole_entity, encode_whole_entity};
use aeordb::engine::v4::first_authority::{
  FirstAuthorityPublicationRequestV1, MarkRunCheckpointLineageStateV1, MarkRunCheckpointPublicationRequestV1,
  MarkRunCheckpointSelectionRequestV1, MarkRunCheckpointSelectionV1, PreparedNamespaceTreeV0, V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::gc::{GcActiveControlWriteV1, GcArtifactKindV1, decode_gc_active_control, encode_gc_active_control};
use aeordb::engine::v4::gc_mark::{
  GcMarkArtifactV1, MarkResumeContextV1, MarkRunCheckpointV1, MarkRunCheckpointWriteV1, decode_gc_mark_artifact, encode_mark_run_checkpoint,
};
use aeordb::engine::v4::gc_mark_workspace::{
  DurableMarkWorkspaceClosureV1, DurableMarkWorkspaceV1, MarkWorkspaceBasisV1, MarkWorkspaceIdentityV1, MarkWorkspaceOptionsV1,
  MarkWorkspaceReopenOptionsV1,
};
use aeordb::engine::v4::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1};
use aeordb::engine::v4::gc_state::{RetirementReasonV1, decode_retirement_journal_segment_v1, retirement_journal_records_v1};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, encode_semantic_state_object};
use aeordb::engine::{CompressionAlgorithm, DiskKVStore, HashAlgorithm};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

fn sequence<const N: usize>(start: u8) -> [u8; N] {
  let mut bytes = [0u8; N];
  for (index, byte) in bytes.iter_mut().enumerate() {
    *byte = start.wrapping_add(u8::try_from(index).unwrap());
  }
  bytes
}

fn sequence_vec(start: u8, length: usize) -> Vec<u8> {
  (0..length).map(|index| start.wrapping_add(u8::try_from(index).unwrap())).collect()
}

fn capabilities() -> [u8; 32] {
  let mut capabilities = [0u8; 32];
  for bit in [12usize, 13, 14, 15, 17] {
    capabilities[bit / 8] |= 1 << (bit % 8);
  }
  capabilities
}

fn database_id() -> [u8; 16] {
  sequence(0x31)
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

fn first_authority_request(algorithm: HashAlgorithm) -> FirstAuthorityPublicationRequestV1 {
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
    typed_closure_digest: digest_parts(algorithm, &[b"typed mark checkpoint storage closure"]),
    authority_identity: b"HEAD".to_vec(),
  }
}

fn create_publisher(algorithm: HashAlgorithm) -> (TempDir, PathBuf, V4FirstAuthorityPublisher) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("mark-checkpoint-storage.aeordb");
  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(&path).unwrap();
  let header = initial_header(algorithm, initial_block_size() as u64);
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
  let publisher = V4FirstAuthorityPublisher::new(kv, coordinator).unwrap();
  publisher.publish(&first_authority_request(algorithm)).unwrap();
  (directory, path, publisher)
}

fn reopen(path: &Path) -> V4FirstAuthorityPublisher {
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
  V4FirstAuthorityPublisher::new(kv, coordinator).unwrap()
}

fn memory_coordinator() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap())
}

struct PreparedCheckpoint {
  closure: DurableMarkWorkspaceClosureV1,
  checkpoint: aeordb::engine::v4::gc::EncodedImmutableGcArtifactV1,
  control: aeordb::engine::v4::gc::EncodedGcActiveControlV1,
}

fn decoded_checkpoint<'a>(algorithm: HashAlgorithm, prepared: &'a PreparedCheckpoint) -> Box<MarkRunCheckpointV1<'a>> {
  let GcMarkArtifactV1::Checkpoint(checkpoint) = decode_gc_mark_artifact(&prepared.checkpoint.value, algorithm).unwrap() else {
    panic!("prepared artifact is not a mark checkpoint");
  };
  checkpoint
}

fn resume_context<'a>(algorithm: HashAlgorithm, checkpoint: &'a MarkRunCheckpointV1<'a>) -> MarkResumeContextV1<'a> {
  MarkResumeContextV1 {
    hash_algorithm: algorithm,
    database_id: checkpoint.database_id,
    run_id: checkpoint.run_id,
    generation: checkpoint.generation,
    checkpoint_sequence: checkpoint.checkpoint_sequence,
    workspace_path: checkpoint.workspace_path,
    workspace_id: checkpoint.workspace_id,
    authority_root_set_digest: checkpoint.authority_root_set_digest,
    semantic_state_digest: checkpoint.semantic_state_digest,
    kv_layout_fingerprint: checkpoint.kv_layout_fingerprint,
    effective_policy_fingerprint: checkpoint.effective_policy_fingerprint,
    system_family_registry_fingerprint: checkpoint.system_family_registry_fingerprint,
    captured_header_sequence: checkpoint.captured_header_sequence,
    captured_write_high_water: checkpoint.captured_write_high_water,
    reconciled_through_sequence: checkpoint.reconciled_through_sequence,
    active_bitmap_bit_count: checkpoint.active_bitmap_bit_count,
    kv_bucket_count: checkpoint.kv_bucket_count,
    kv_slots_per_bucket: checkpoint.kv_slots_per_bucket,
  }
}

fn selection_request<'a>(
  resume_contexts: &'a [MarkResumeContextV1<'a>],
  cancellation: &'a CancellationToken,
  memory: &'a MemoryCoordinator,
) -> MarkRunCheckpointSelectionRequestV1<'a> {
  MarkRunCheckpointSelectionRequestV1 {
    resume_contexts,
    workspace_options: MarkWorkspaceReopenOptionsV1::new(64 * 1024 * 1024).unwrap(),
    cancellation,
    memory,
  }
}

fn prepare_checkpoint(
  database_path: &Path,
  scratch_root: &Path,
  memory: &MemoryCoordinator,
  algorithm: HashAlgorithm,
  run_byte: u8,
  generation: u64,
  checkpoint_sequence: u64,
  control_slot: u8,
  control_sequence: u64,
) -> PreparedCheckpoint {
  let run_id = [run_byte; 16];
  let identity = MarkWorkspaceIdentityV1::new(database_id(), run_id, generation, checkpoint_sequence, algorithm).unwrap();
  let basis = MarkWorkspaceBasisV1::new(
    1,
    1_700_000_100_000 + checkpoint_sequence,
    1_700_000_100_500 + checkpoint_sequence,
    sequence_vec(0x51, algorithm.hash_length()),
    sequence_vec(0x11, algorithm.hash_length()),
    sequence(0x71),
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
  let workspace_path = closure.checkpoint_workspace_path().unwrap();
  let checkpoint = encode_mark_run_checkpoint(&MarkRunCheckpointWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id(),
    run_id: &run_id,
    generation,
    checkpoint_sequence,
    state: 1,
    phase: 1,
    resumable: true,
    canceled: false,
    capabilities: capabilities(),
    started_at_ms: 1_700_000_100_000 + checkpoint_sequence,
    updated_at_ms: 1_700_000_100_500 + checkpoint_sequence,
    authority_root_set_digest: &sequence_vec(0x11, algorithm.hash_length()),
    semantic_state_digest: &sequence_vec(0x31, algorithm.hash_length()),
    kv_layout_fingerprint: &sequence_vec(0x51, algorithm.hash_length()),
    effective_policy_fingerprint: sequence(0x71),
    system_family_registry_fingerprint: sequence(0x91),
    captured_header_sequence: 17,
    captured_write_high_water: 900,
    reconciled_through_sequence: 801,
    active_bitmap_bit_count: 512,
    kv_bucket_count: 8,
    kv_slots_per_bucket: 64,
    workspace_path: &workspace_path,
    workspace_id: [run_byte.wrapping_add(0x20); 16],
    workspace_manifest_digest: closure.manifest_digest(),
    mutation_journal_head: &sequence_vec(0xB1, algorithm.hash_length()),
    checkpoint_logical_work: checkpoint_sequence * 1024,
    total_logical_work_hint: 64 * 1024 * 1024,
  })
  .unwrap();
  let control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::MarkRunActiveControl,
    hash_algorithm: algorithm,
    database_id: &database_id(),
    slot: control_slot,
    sequence: control_sequence,
    generation,
    target_manifest_hash: &checkpoint.key,
  })
  .unwrap();
  PreparedCheckpoint { closure, checkpoint, control }
}

fn read_entity(path: &Path, offset: u64, length: u32) -> Vec<u8> {
  let mut file = OpenOptions::new().read(true).open(path).unwrap();
  file.seek(SeekFrom::Start(offset)).unwrap();
  let mut bytes = vec![0; length as usize];
  file.read_exact(&mut bytes).unwrap();
  bytes
}

fn publish(
  publisher: &mut V4FirstAuthorityPublisher,
  owner: &mut RetirementJournalOwnerV1<'_>,
  algorithm: HashAlgorithm,
  prepared: &PreparedCheckpoint,
  timestamp: u64,
) -> aeordb::engine::v4::first_authority::MarkRunCheckpointPublicationReceiptV1 {
  publisher
    .publish_mark_run_checkpoint(
      MarkRunCheckpointPublicationRequestV1 {
        hash_algorithm: algorithm,
        checkpoint: &prepared.checkpoint,
        control: &prepared.control,
        workspace: &prepared.closure,
        publication_timestamp_ms: timestamp,
        monotonic_now_ms: timestamp,
      },
      owner,
    )
    .unwrap()
}

#[test]
fn whole_entity_authority_publishes_a_b_a_and_retires_the_prior_control_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (directory, path, mut publisher) = create_publisher(algorithm);
    let scratch = directory.path().join("scratch");
    fs::create_dir(&scratch).unwrap();
    let memory = memory_coordinator();
    let cancellation = CancellationToken::new();
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

    let a1 = prepare_checkpoint(&path, &scratch, &memory, algorithm, 0x51, 101, 1, 0, 1);
    let a1_receipt = publish(&mut publisher, &mut owner, algorithm, &a1, 1_700_000_200_001);
    assert_eq!(a1_receipt.control_slot, 0);
    assert!(!a1_receipt.replaced_control);
    assert_eq!(a1_receipt.lineage_state.code(), "not_required");
    let old_a_locator = publisher.locator(&a1.control.key).unwrap().unwrap();

    let b2 = prepare_checkpoint(&path, &scratch, &memory, algorithm, 0x52, 102, 2, 1, 2);
    let b2_receipt = publish(&mut publisher, &mut owner, algorithm, &b2, 1_700_000_200_002);
    assert_eq!(b2_receipt.control_slot, 1);
    assert!(!b2_receipt.replaced_control);

    let a3 = prepare_checkpoint(&path, &scratch, &memory, algorithm, 0x53, 103, 3, 0, 3);
    let a3_receipt = publish(&mut publisher, &mut owner, algorithm, &a3, 1_700_000_200_003);
    assert_eq!(a3_receipt.control_slot, 0);
    assert!(a3_receipt.replaced_control);
    assert!(matches!(a3_receipt.lineage_state, MarkRunCheckpointLineageStateV1::HardPublished { .. }));
    assert_eq!(owner.status().pending_records, 0);
    assert_eq!(owner.status().durable_records, 1);

    let new_a_locator = publisher.locator(&a3.control.key).unwrap().unwrap();
    assert_ne!(new_a_locator.offset, old_a_locator.offset);
    let a_bytes = read_entity(&path, new_a_locator.offset, new_a_locator.total_length);
    let a_entity = decode_whole_entity(&a_bytes, algorithm, a3_receipt.observation.selected.header.write_sequence_high_water).unwrap();
    let a_control = decode_gc_active_control(a_entity.stored_value, algorithm).unwrap();
    assert_eq!(a_control.sequence, 3);
    assert_eq!(a_control.target_manifest_hash, a3.checkpoint.key);
    assert!(publisher.locator(&a3.checkpoint.key).unwrap().is_some());
    assert!(publisher.locator(&b2.checkpoint.key).unwrap().is_some());

    let journal_key = owner.status().last_segment_hash;
    let journal_locator = publisher.locator(&journal_key).unwrap().unwrap();
    let journal_entity_bytes = read_entity(&path, journal_locator.offset, journal_locator.total_length);
    let journal_entity =
      decode_whole_entity(&journal_entity_bytes, algorithm, a3_receipt.observation.selected.header.write_sequence_high_water).unwrap();
    let journal = decode_retirement_journal_segment_v1(journal_entity.stored_value, algorithm).unwrap();
    let records: Vec<_> = retirement_journal_records_v1(&journal, algorithm).unwrap().map(|record| record.unwrap()).collect();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].reason, RetirementReasonV1::PointerOrControlReplace);
    assert_eq!(records[0].replacement_publication_sequence, a3_receipt.control_write_sequence);
    let old = records[0].old;
    let replacement = records[0].replacement;
    assert_eq!(old.wal_offset, old_a_locator.offset);
    assert_eq!(replacement.wal_offset, new_a_locator.offset);
    assert_eq!(replacement.write_sequence, a3_receipt.control_write_sequence);

    let before_retry = publisher.observe().unwrap();
    let retry = publish(&mut publisher, &mut owner, algorithm, &a3, 1_700_000_200_003);
    assert!(retry.idempotent);
    assert_eq!(publisher.observe().unwrap(), before_retry);
    drop(publisher);

    let reopened = reopen(&path);
    assert!(reopened.locator(&a3.checkpoint.key).unwrap().is_some());
    assert_eq!(reopened.locator(&a3.control.key).unwrap().unwrap(), new_a_locator);
    assert!(reopened.locator(&journal_key).unwrap().is_some());
  }
}

#[test]
fn workspace_mismatch_and_canceled_retirement_owner_refuse_before_checkpoint_authority_changes() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (directory, path, mut publisher) = create_publisher(algorithm);
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory_coordinator();
  let prepared = prepare_checkpoint(&path, &scratch, &memory, algorithm, 0x61, 201, 1, 0, 1);
  let other = prepare_checkpoint(&path, &scratch, &memory, algorithm, 0x62, 202, 2, 0, 2);
  let cancellation = CancellationToken::new();
  let mut owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id(),
    1,
    401,
    RetirementJournalBufferOptionsV1::default(),
    &cancellation,
    &memory,
  )
  .unwrap();
  let before = publisher.observe().unwrap();
  let error = publisher
    .publish_mark_run_checkpoint(
      MarkRunCheckpointPublicationRequestV1 {
        hash_algorithm: algorithm,
        checkpoint: &prepared.checkpoint,
        control: &prepared.control,
        workspace: &other.closure,
        publication_timestamp_ms: 1_700_000_300_001,
        monotonic_now_ms: 1,
      },
      &mut owner,
    )
    .unwrap_err();
  assert_eq!(error.code(), "mark_checkpoint_workspace_closure");
  assert_eq!(publisher.observe().unwrap(), before);

  cancellation.cancel();
  let error = publisher
    .publish_mark_run_checkpoint(
      MarkRunCheckpointPublicationRequestV1 {
        hash_algorithm: algorithm,
        checkpoint: &prepared.checkpoint,
        control: &prepared.control,
        workspace: &prepared.closure,
        publication_timestamp_ms: 1_700_000_300_001,
        monotonic_now_ms: 1,
      },
      &mut owner,
    )
    .unwrap_err();
  assert_eq!(error.code(), "retirement_journal_cancelled");
  assert_eq!(publisher.observe().unwrap(), before);
}

#[test]
fn restart_selection_reports_absence_then_selects_the_newest_complete_checkpoint_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (directory, path, mut publisher) = create_publisher(algorithm);
    let scratch = directory.path().join("scratch");
    fs::create_dir(&scratch).unwrap();
    let memory = memory_coordinator();
    let cancellation = CancellationToken::new();
    assert!(matches!(
      publisher.select_mark_run_checkpoint(selection_request(&[], &cancellation, &memory)).unwrap(),
      MarkRunCheckpointSelectionV1::Absent
    ));

    let mut owner = RetirementJournalOwnerV1::new_chain(
      algorithm,
      database_id(),
      1,
      401,
      RetirementJournalBufferOptionsV1::default(),
      &cancellation,
      &memory,
    )
    .unwrap();
    let a1 = prepare_checkpoint(&path, &scratch, &memory, algorithm, 0x71, 301, 1, 0, 1);
    let _ = publish(&mut publisher, &mut owner, algorithm, &a1, 1_700_000_400_001);
    let b2 = prepare_checkpoint(&path, &scratch, &memory, algorithm, 0x72, 302, 2, 1, 2);
    let _ = publish(&mut publisher, &mut owner, algorithm, &b2, 1_700_000_400_002);
    let a_checkpoint = decoded_checkpoint(algorithm, &a1);
    let b_checkpoint = decoded_checkpoint(algorithm, &b2);
    let contexts = [resume_context(algorithm, &a_checkpoint), resume_context(algorithm, &b_checkpoint)];

    let selected = match publisher.select_mark_run_checkpoint(selection_request(&contexts, &cancellation, &memory)).unwrap() {
      MarkRunCheckpointSelectionV1::Selected(selected) => selected,
      MarkRunCheckpointSelectionV1::Absent => panic!("published mark controls were reported absent"),
    };
    assert_eq!(selected.control_slot, 1);
    assert_eq!(selected.control_sequence, 2);
    assert_eq!(selected.checkpoint_key, b2.checkpoint.key);
    assert!(selected.degraded_slots.is_empty());
    assert_eq!(selected.workspace().manifest_digest(), b2.closure.manifest_digest());
    assert_eq!(selected.checkpoint().unwrap().checkpoint_sequence, 2);
    drop(selected);
    drop(publisher);

    let reopened = reopen(&path);
    let selected = match reopened.select_mark_run_checkpoint(selection_request(&contexts, &cancellation, &memory)).unwrap() {
      MarkRunCheckpointSelectionV1::Selected(selected) => selected,
      MarkRunCheckpointSelectionV1::Absent => panic!("reopened mark controls were reported absent"),
    };
    assert_eq!(selected.control_slot, 1);
    assert_eq!(selected.checkpoint_key, b2.checkpoint.key);
  }
}

#[test]
fn selection_falls_back_from_a_tampered_newer_workspace_and_refuses_when_both_are_invalid() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (directory, path, mut publisher) = create_publisher(algorithm);
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory_coordinator();
  let cancellation = CancellationToken::new();
  let mut owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id(),
    1,
    401,
    RetirementJournalBufferOptionsV1::default(),
    &cancellation,
    &memory,
  )
  .unwrap();
  let a1 = prepare_checkpoint(&path, &scratch, &memory, algorithm, 0x73, 303, 1, 0, 1);
  let _ = publish(&mut publisher, &mut owner, algorithm, &a1, 1_700_000_500_001);
  let b2 = prepare_checkpoint(&path, &scratch, &memory, algorithm, 0x74, 304, 2, 1, 2);
  let _ = publish(&mut publisher, &mut owner, algorithm, &b2, 1_700_000_500_002);
  let a_checkpoint = decoded_checkpoint(algorithm, &a1);
  let b_checkpoint = decoded_checkpoint(algorithm, &b2);
  let contexts = [resume_context(algorithm, &a_checkpoint), resume_context(algorithm, &b_checkpoint)];

  let mut b_manifest = fs::read(b2.closure.manifest_path()).unwrap();
  b_manifest[0] ^= 1;
  fs::write(b2.closure.manifest_path(), b_manifest).unwrap();
  let selected = match publisher.select_mark_run_checkpoint(selection_request(&contexts, &cancellation, &memory)).unwrap() {
    MarkRunCheckpointSelectionV1::Selected(selected) => selected,
    MarkRunCheckpointSelectionV1::Absent => panic!("prior complete checkpoint was hidden by a tampered replacement"),
  };
  assert_eq!(selected.control_slot, 0);
  assert_eq!(selected.checkpoint_key, a1.checkpoint.key);
  assert_eq!(selected.degraded_slots.len(), 1);
  assert_eq!(selected.degraded_slots[0].slot, 1);
  assert_eq!(selected.degraded_slots[0].code, "mark_workspace_format");
  drop(selected);

  let mut a_manifest = fs::read(a1.closure.manifest_path()).unwrap();
  a_manifest[0] ^= 1;
  fs::write(a1.closure.manifest_path(), a_manifest).unwrap();
  let error = publisher.select_mark_run_checkpoint(selection_request(&contexts, &cancellation, &memory)).unwrap_err();
  assert_eq!(error.code(), "mark_checkpoint_selection_no_valid_closure");
  assert!(error.to_string().contains("slot: 0"));
  assert!(error.to_string().contains("slot: 1"));
}

#[test]
fn corrupt_control_falls_back_but_equal_sequence_disagreement_remains_ambiguous() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (directory, path, mut publisher) = create_publisher(algorithm);
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory_coordinator();
  let cancellation = CancellationToken::new();
  let mut owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id(),
    1,
    401,
    RetirementJournalBufferOptionsV1::default(),
    &cancellation,
    &memory,
  )
  .unwrap();
  let a1 = prepare_checkpoint(&path, &scratch, &memory, algorithm, 0x75, 305, 1, 0, 1);
  let _ = publish(&mut publisher, &mut owner, algorithm, &a1, 1_700_000_600_001);
  let b2 = prepare_checkpoint(&path, &scratch, &memory, algorithm, 0x76, 306, 2, 1, 2);
  let _ = publish(&mut publisher, &mut owner, algorithm, &b2, 1_700_000_600_002);
  let a_checkpoint = decoded_checkpoint(algorithm, &a1);
  let b_checkpoint = decoded_checkpoint(algorithm, &b2);
  let contexts = [resume_context(algorithm, &a_checkpoint), resume_context(algorithm, &b_checkpoint)];
  let b_locator = publisher.locator(&b2.control.key).unwrap().unwrap();
  let b_bytes = read_entity(&path, b_locator.offset, b_locator.total_length);
  let mut file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
  file.seek(SeekFrom::Start(b_locator.offset + u64::from(b_locator.total_length) - 1)).unwrap();
  file.write_all(&[b_bytes[b_bytes.len() - 1] ^ 1]).unwrap();
  file.sync_all().unwrap();
  let selected = match publisher.select_mark_run_checkpoint(selection_request(&contexts, &cancellation, &memory)).unwrap() {
    MarkRunCheckpointSelectionV1::Selected(selected) => selected,
    MarkRunCheckpointSelectionV1::Absent => panic!("valid A control was hidden by corrupt B"),
  };
  assert_eq!(selected.control_slot, 0);
  assert_eq!(selected.degraded_slots.len(), 1);
  assert_eq!(selected.degraded_slots[0].slot, 1);
  drop(selected);
  drop(file);

  let (directory, path, mut publisher) = create_publisher(algorithm);
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory_coordinator();
  let cancellation = CancellationToken::new();
  let mut owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id(),
    1,
    401,
    RetirementJournalBufferOptionsV1::default(),
    &cancellation,
    &memory,
  )
  .unwrap();
  let a1 = prepare_checkpoint(&path, &scratch, &memory, algorithm, 0x77, 307, 1, 0, 1);
  let _ = publish(&mut publisher, &mut owner, algorithm, &a1, 1_700_000_700_001);
  let b2 = prepare_checkpoint(&path, &scratch, &memory, algorithm, 0x78, 308, 2, 1, 2);
  let _ = publish(&mut publisher, &mut owner, algorithm, &b2, 1_700_000_700_002);
  let b_locator = publisher.locator(&b2.control.key).unwrap().unwrap();
  let high_water = publisher.observe().unwrap().selected.header.write_sequence_high_water;
  let old_bytes = read_entity(&path, b_locator.offset, b_locator.total_length);
  let old_entity = decode_whole_entity(&old_bytes, algorithm, high_water).unwrap();
  let ambiguous_control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::MarkRunActiveControl,
    hash_algorithm: algorithm,
    database_id: &database_id(),
    slot: 1,
    sequence: 1,
    generation: 308,
    target_manifest_hash: &b2.checkpoint.key,
  })
  .unwrap();
  let replacement = encode_whole_entity(&WholeEntityWriteV1 {
    entry_type: EntryTypeV4::GcArtifact,
    flags: WHOLE_ENTITY_V1_FLAG_SYSTEM,
    hash_algorithm: algorithm,
    compression_algorithm: CompressionAlgorithm::None,
    timestamp_ms: old_entity.timestamp_ms,
    write_sequence: old_entity.write_sequence,
    key: &b2.control.key,
    stored_value: &ambiguous_control.value,
  })
  .unwrap();
  assert_eq!(replacement.len(), old_bytes.len());
  let mut file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
  file.seek(SeekFrom::Start(b_locator.offset)).unwrap();
  file.write_all(&replacement).unwrap();
  file.sync_all().unwrap();
  let a_checkpoint = decoded_checkpoint(algorithm, &a1);
  let b_checkpoint = decoded_checkpoint(algorithm, &b2);
  let contexts = [resume_context(algorithm, &a_checkpoint), resume_context(algorithm, &b_checkpoint)];
  let error = publisher.select_mark_run_checkpoint(selection_request(&contexts, &cancellation, &memory)).unwrap_err();
  assert_eq!(error.code(), "gc_control_pair_ambiguous");
}

#[test]
fn selection_cancellation_memory_pressure_and_context_amplification_fail_before_resume() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (directory, path, mut publisher) = create_publisher(algorithm);
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory_coordinator();
  let cancellation = CancellationToken::new();
  let mut owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id(),
    1,
    401,
    RetirementJournalBufferOptionsV1::default(),
    &cancellation,
    &memory,
  )
  .unwrap();
  let a1 = prepare_checkpoint(&path, &scratch, &memory, algorithm, 0x79, 309, 1, 0, 1);
  let _ = publish(&mut publisher, &mut owner, algorithm, &a1, 1_700_000_800_001);
  let checkpoint = decoded_checkpoint(algorithm, &a1);
  let context = resume_context(algorithm, &checkpoint);

  let canceled = CancellationToken::new();
  canceled.cancel();
  assert_eq!(
    publisher.select_mark_run_checkpoint(selection_request(&[context], &canceled, &memory)).unwrap_err().code(),
    "mark_checkpoint_selection_cancelled"
  );
  let constrained = MemoryCoordinator::new(MemoryPolicy::new(1, 1024, 1, 1).unwrap());
  assert_eq!(
    publisher.select_mark_run_checkpoint(selection_request(&[context], &cancellation, &constrained)).unwrap_err().code(),
    "mark_checkpoint_selection_memory"
  );
  assert_eq!(constrained.snapshot().unwrap().reserved_bytes, 0);
  assert_eq!(
    publisher.select_mark_run_checkpoint(selection_request(&[context, context, context], &cancellation, &memory)).unwrap_err().code(),
    "mark_checkpoint_selection_contexts"
  );
}

#[test]
fn control_with_a_missing_checkpoint_does_not_hide_its_complete_peer() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (directory, path, mut publisher) = create_publisher(algorithm);
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory_coordinator();
  let cancellation = CancellationToken::new();
  let mut owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id(),
    1,
    401,
    RetirementJournalBufferOptionsV1::default(),
    &cancellation,
    &memory,
  )
  .unwrap();
  let a1 = prepare_checkpoint(&path, &scratch, &memory, algorithm, 0x7A, 310, 1, 0, 1);
  let _ = publish(&mut publisher, &mut owner, algorithm, &a1, 1_700_000_900_001);
  let b2 = prepare_checkpoint(&path, &scratch, &memory, algorithm, 0x7B, 311, 2, 1, 2);
  let _ = publish(&mut publisher, &mut owner, algorithm, &b2, 1_700_000_900_002);

  let b_locator = publisher.locator(&b2.control.key).unwrap().unwrap();
  let high_water = publisher.observe().unwrap().selected.header.write_sequence_high_water;
  let old_bytes = read_entity(&path, b_locator.offset, b_locator.total_length);
  let old_entity = decode_whole_entity(&old_bytes, algorithm, high_water).unwrap();
  let missing_target = vec![0xE1; algorithm.hash_length()];
  let missing_control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::MarkRunActiveControl,
    hash_algorithm: algorithm,
    database_id: &database_id(),
    slot: 1,
    sequence: 2,
    generation: 311,
    target_manifest_hash: &missing_target,
  })
  .unwrap();
  let replacement = encode_whole_entity(&WholeEntityWriteV1 {
    entry_type: EntryTypeV4::GcArtifact,
    flags: WHOLE_ENTITY_V1_FLAG_SYSTEM,
    hash_algorithm: algorithm,
    compression_algorithm: CompressionAlgorithm::None,
    timestamp_ms: old_entity.timestamp_ms,
    write_sequence: old_entity.write_sequence,
    key: &b2.control.key,
    stored_value: &missing_control.value,
  })
  .unwrap();
  assert_eq!(replacement.len(), old_bytes.len());
  let mut file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
  file.seek(SeekFrom::Start(b_locator.offset)).unwrap();
  file.write_all(&replacement).unwrap();
  file.sync_all().unwrap();

  let a_checkpoint = decoded_checkpoint(algorithm, &a1);
  let contexts = [resume_context(algorithm, &a_checkpoint)];
  let selected = match publisher.select_mark_run_checkpoint(selection_request(&contexts, &cancellation, &memory)).unwrap() {
    MarkRunCheckpointSelectionV1::Selected(selected) => selected,
    MarkRunCheckpointSelectionV1::Absent => panic!("missing B checkpoint hid complete A"),
  };
  assert_eq!(selected.control_slot, 0);
  assert_eq!(selected.degraded_slots.len(), 1);
  assert_eq!(selected.degraded_slots[0].slot, 1);
  assert_eq!(selected.degraded_slots[0].code, "mark_checkpoint_entity_missing");
}

fn rust_sources(root: &Path, sources: &mut Vec<PathBuf>) {
  for entry in fs::read_dir(root).unwrap() {
    let path = entry.unwrap().path();
    if path.is_dir() {
      rust_sources(&path, sources);
    } else if path.extension().is_some_and(|extension| extension == "rs") {
      sources.push(path);
    }
  }
}

#[test]
fn checkpoint_selection_remains_disconnected_from_live_service_and_destructive_gc() {
  let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let owner_path = source_root.join("engine/v4/first_authority.rs");
  let mut sources = Vec::new();
  rust_sources(&source_root, &mut sources);
  let callers: Vec<_> = sources
    .into_iter()
    .filter(|path| path != &owner_path)
    .filter(|path| fs::read_to_string(path).unwrap_or_default().contains(".select_mark_run_checkpoint("))
    .map(|path| path.strip_prefix(&source_root).unwrap().to_owned())
    .collect();
  assert!(callers.is_empty(), "P4-3 checkpoint selection activated before the runtime owner exists: {callers:?}");
}
