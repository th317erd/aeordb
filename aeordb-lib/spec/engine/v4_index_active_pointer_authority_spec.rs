use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aeordb::engine::durability_coordinator::{CommitClass, DurabilityCoordinator};
use aeordb::engine::hot_tail::read_hot_tail_checked;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::first_authority::{
  FirstAuthorityPublicationRequestV1, IndexActivePointerPublicationRequestV1, IndexArtifactBatchPublicationRequestV1,
  PreparedNamespaceTreeV0, V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_artifact::{
  ActivePointerKindV1, ActivePointerWriteV1, EncodedImmutableIndexArtifactV1, decode_index_manifest, encode_active_pointer,
};
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, encode_semantic_state_object};
use aeordb::engine::{DiskKVStore, HashAlgorithm};
use tokio_util::sync::CancellationToken;

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;
const DATABASE_ID: [u8; 16] = [0x31; 16];

#[test]
fn first_authority_selects_retries_replaces_and_reopens_one_active_pointer_pair() {
  let (_directory, path, publisher) = create_publisher();
  let scope_manifest = immutable_fixture("aidx-blake3-256-scope-catalog-manifest-empty.bin");
  let value_manifest = immutable_fixture("aidx-blake3-256-value-store-manifest-empty.bin");
  let manifest = immutable_fixture("aidx-blake3-256-field-index-manifest-empty.bin");
  let manifest_view = decode_index_manifest(&manifest.value, ALGORITHM).unwrap();
  publisher
    .publish_index_artifacts(IndexArtifactBatchPublicationRequestV1 {
      database_id: &DATABASE_ID,
      artifacts: &[&scope_manifest, &value_manifest, &manifest],
      publication_timestamp_ms: 1_700_000_000_200,
    })
    .unwrap();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let mut retirement = retirement_owner(&cancellation, &memory);

  let pointer_a = encode_active_pointer(&ActivePointerWriteV1 {
    kind: ActivePointerKindV1::FieldIndex,
    hash_algorithm: ALGORITHM,
    generation: manifest_view.generation,
    owner_id: manifest_view.owner_id,
    slot: 0,
    sequence: 1,
    target_manifest_hash: &manifest.key,
  })
  .unwrap();
  let first = publisher
    .publish_index_active_pointer(
      IndexActivePointerPublicationRequestV1 {
        database_id: &DATABASE_ID,
        pointer: &pointer_a,
        publication_timestamp_ms: 1_700_000_000_300,
        monotonic_now_ms: 1_700_000_000_300,
      },
      &mut retirement,
    )
    .unwrap();
  assert_eq!(first.pointer_sequence, 1);
  assert_eq!(first.selected_slot, 0);
  assert!(!first.idempotent);

  let retry = publisher
    .publish_index_active_pointer(
      IndexActivePointerPublicationRequestV1 {
        database_id: &DATABASE_ID,
        pointer: &pointer_a,
        publication_timestamp_ms: 1_700_000_000_301,
        monotonic_now_ms: 1_700_000_000_301,
      },
      &mut retirement,
    )
    .unwrap();
  assert!(retry.idempotent);
  assert_eq!(retry.observation, first.observation);

  let pointer_b = encode_active_pointer(&ActivePointerWriteV1 {
    kind: ActivePointerKindV1::FieldIndex,
    hash_algorithm: ALGORITHM,
    generation: manifest_view.generation,
    owner_id: manifest_view.owner_id,
    slot: 1,
    sequence: 2,
    target_manifest_hash: &manifest.key,
  })
  .unwrap();
  publisher
    .publish_index_active_pointer(
      IndexActivePointerPublicationRequestV1 {
        database_id: &DATABASE_ID,
        pointer: &pointer_b,
        publication_timestamp_ms: 1_700_000_000_400,
        monotonic_now_ms: 1_700_000_000_400,
      },
      &mut retirement,
    )
    .unwrap();

  let pointer_a_replacement = encode_active_pointer(&ActivePointerWriteV1 {
    kind: ActivePointerKindV1::FieldIndex,
    hash_algorithm: ALGORITHM,
    generation: manifest_view.generation,
    owner_id: manifest_view.owner_id,
    slot: 0,
    sequence: 3,
    target_manifest_hash: &manifest.key,
  })
  .unwrap();
  let replacement = publisher
    .publish_index_active_pointer(
      IndexActivePointerPublicationRequestV1 {
        database_id: &DATABASE_ID,
        pointer: &pointer_a_replacement,
        publication_timestamp_ms: 1_700_000_000_500,
        monotonic_now_ms: 1_700_000_000_500,
      },
      &mut retirement,
    )
    .unwrap();
  assert_eq!(replacement.pointer_sequence, 3);
  assert_eq!(replacement.selected_slot, 0);
  assert!(replacement.replaced_slot);
  assert!(replacement.retirement_hard_publication_sequence.is_some());

  drop(publisher);
  let reopened = reopen(&path);
  let pair = reopened.load_index_active_pointer_pair(&DATABASE_ID, ActivePointerKindV1::FieldIndex, manifest_view.owner_id).unwrap();
  let selected = pair.selected.unwrap();
  assert_eq!(selected.bytes, pointer_a_replacement.value);
  assert_eq!(selected.pointer_sequence, 3);
  assert_eq!(selected.target_manifest_hash, manifest.key);
  assert_eq!(pair.slots[0].as_ref().unwrap().pointer_sequence, 3);
  assert_eq!(pair.slots[1].as_ref().unwrap().pointer_sequence, 2);
}

#[test]
fn index_hard_barrier_returns_real_durability_evidence_without_moving_semantic_authority() {
  let (_directory, path, publisher) = create_publisher();
  let before = publisher.observe().unwrap();
  let receipt = publisher.publish_index_hard_barrier(&DATABASE_ID, 1_700_000_000_200).unwrap();

  assert_eq!(receipt.durability.class, CommitClass::HardAuthority);
  assert_ne!(receipt.durability.sequence, 0);
  assert_eq!(receipt.durability.hard_frontier, receipt.durability.sequence);
  assert_eq!(receipt.observation.selected.header.slot_sequence, before.selected.header.slot_sequence + 1);
  assert_eq!(receipt.observation.selected.header.updated_at_ms, 1_700_000_000_200);
  assert_eq!(receipt.observation.selected.header.write_sequence_high_water, before.selected.header.write_sequence_high_water);
  assert_eq!(receipt.observation.selected.header.hot_tail_offset, before.selected.header.hot_tail_offset);
  assert_eq!(receipt.observation.selected.header.entry_count, before.selected.header.entry_count);
  assert_eq!(receipt.observation.selected.header.head_hash, before.selected.header.head_hash);

  let error = publisher.publish_index_hard_barrier(&DATABASE_ID, 0).unwrap_err();
  assert_eq!(error.code(), "index_hard_barrier_time");
  let error = publisher.publish_index_hard_barrier(&[0x99; 16], 1_700_000_000_201).unwrap_err();
  assert_eq!(error.code(), "index_active_pointer_database_mismatch");

  drop(publisher);
  let reopened = reopen(&path).observe().unwrap();
  assert_eq!(reopened, receipt.observation);
}

#[test]
fn active_pointer_refuses_incomplete_foreign_and_noncanonical_publication_without_moving_authority() {
  let (_directory, _path, publisher) = create_publisher();
  let scope_manifest = immutable_fixture("aidx-blake3-256-scope-catalog-manifest-empty.bin");
  let value_manifest = immutable_fixture("aidx-blake3-256-value-store-manifest-empty.bin");
  let field_manifest = immutable_fixture("aidx-blake3-256-field-index-manifest-empty.bin");
  let field = decode_index_manifest(&field_manifest.value, ALGORITHM).unwrap();
  publisher
    .publish_index_artifacts(IndexArtifactBatchPublicationRequestV1 {
      database_id: &DATABASE_ID,
      artifacts: &[&field_manifest],
      publication_timestamp_ms: 1_700_000_000_200,
    })
    .unwrap();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let mut retirement = retirement_owner(&cancellation, &memory);
  let pointer = encode_active_pointer(&ActivePointerWriteV1 {
    kind: ActivePointerKindV1::FieldIndex,
    hash_algorithm: ALGORITHM,
    generation: field.generation,
    owner_id: field.owner_id,
    slot: 0,
    sequence: 1,
    target_manifest_hash: &field_manifest.key,
  })
  .unwrap();
  let before = publisher.observe().unwrap();
  let error = publisher
    .publish_index_active_pointer(
      IndexActivePointerPublicationRequestV1 {
        database_id: &DATABASE_ID,
        pointer: &pointer,
        publication_timestamp_ms: 1_700_000_000_300,
        monotonic_now_ms: 1_700_000_000_300,
      },
      &mut retirement,
    )
    .unwrap_err();
  assert_eq!(error.code(), "index_active_pointer_target_closure");
  assert_eq!(publisher.observe().unwrap(), before);
  assert!(publisher
    .load_index_active_pointer_pair(&DATABASE_ID, ActivePointerKindV1::FieldIndex, field.owner_id)
    .unwrap()
    .selected
    .is_none());

  publisher
    .publish_index_artifacts(IndexArtifactBatchPublicationRequestV1 {
      database_id: &DATABASE_ID,
      artifacts: &[&scope_manifest, &value_manifest],
      publication_timestamp_ms: 1_700_000_000_400,
    })
    .unwrap();
  let ready = publisher.observe().unwrap();
  let wrong_slot = encode_active_pointer(&ActivePointerWriteV1 { slot: 1, ..active_pointer_write(&field_manifest, 0, 1) }).unwrap();
  let error = publisher
    .publish_index_active_pointer(
      IndexActivePointerPublicationRequestV1 {
        database_id: &DATABASE_ID,
        pointer: &wrong_slot,
        publication_timestamp_ms: 1_700_000_000_500,
        monotonic_now_ms: 1_700_000_000_500,
      },
      &mut retirement,
    )
    .unwrap_err();
  assert_eq!(error.code(), "index_active_pointer_rewrite_plan");
  assert_eq!(publisher.observe().unwrap(), ready);

  let foreign_database = [0x99; 16];
  let error = publisher
    .publish_index_active_pointer(
      IndexActivePointerPublicationRequestV1 {
        database_id: &foreign_database,
        pointer: &pointer,
        publication_timestamp_ms: 1_700_000_000_501,
        monotonic_now_ms: 1_700_000_000_501,
      },
      &mut retirement,
    )
    .unwrap_err();
  assert_eq!(error.code(), "index_active_pointer_database_mismatch");
  assert_eq!(publisher.observe().unwrap(), ready);

  let mut bad_key = pointer.clone();
  bad_key.key[0] ^= 1;
  let error = publisher
    .publish_index_active_pointer(
      IndexActivePointerPublicationRequestV1 {
        database_id: &DATABASE_ID,
        pointer: &bad_key,
        publication_timestamp_ms: 1_700_000_000_502,
        monotonic_now_ms: 1_700_000_000_502,
      },
      &mut retirement,
    )
    .unwrap_err();
  assert_eq!(error.code(), "index_active_pointer_prepared_mismatch");
  assert_eq!(publisher.observe().unwrap(), ready);
}

#[test]
fn active_pointer_authority_round_trips_the_widest_hash_profile() {
  let algorithm = HashAlgorithm::Sha512;
  let (_directory, path, publisher) = create_publisher_for(algorithm);
  let manifest = immutable_fixture_for(algorithm, "aidx-sha512-scope-catalog-manifest-empty.bin");
  let manifest_view = decode_index_manifest(&manifest.value, algorithm).unwrap();
  publisher
    .publish_index_artifacts(IndexArtifactBatchPublicationRequestV1 {
      database_id: &DATABASE_ID,
      artifacts: &[&manifest],
      publication_timestamp_ms: 1_700_000_000_200,
    })
    .unwrap();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let mut retirement = retirement_owner_for(algorithm, &cancellation, &memory);
  let pointer = encode_active_pointer(&ActivePointerWriteV1 {
    kind: ActivePointerKindV1::ScopeCatalog,
    hash_algorithm: algorithm,
    generation: manifest_view.generation,
    owner_id: manifest_view.owner_id,
    slot: 0,
    sequence: 1,
    target_manifest_hash: &manifest.key,
  })
  .unwrap();
  publisher
    .publish_index_active_pointer(
      IndexActivePointerPublicationRequestV1 {
        database_id: &DATABASE_ID,
        pointer: &pointer,
        publication_timestamp_ms: 1_700_000_000_300,
        monotonic_now_ms: 1_700_000_000_300,
      },
      &mut retirement,
    )
    .unwrap();
  drop(publisher);
  let selected = reopen(&path)
    .load_index_active_pointer_pair(&DATABASE_ID, ActivePointerKindV1::ScopeCatalog, manifest_view.owner_id)
    .unwrap()
    .selected
    .unwrap();
  assert_eq!(selected.bytes, pointer.value);
  assert_eq!(selected.owner_id.len(), 64);
}

fn create_publisher() -> (tempfile::TempDir, PathBuf, V4FirstAuthorityPublisher) {
  create_publisher_for(ALGORITHM)
}

fn create_publisher_for(algorithm: HashAlgorithm) -> (tempfile::TempDir, PathBuf, V4FirstAuthorityPublisher) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("index-active-pointer.aeordb");
  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(&path).unwrap();
  let header = initial_header(algorithm, initial_block_size());
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

fn initial_header(algorithm: HashAlgorithm, kv_block_length: u64) -> DatabaseHeaderV4 {
  DatabaseHeaderV4 {
    hash_algorithm: algorithm,
    slot_sequence: 1,
    created_at_ms: 1_700_000_000_000,
    updated_at_ms: 1_700_000_000_000,
    database_id: DATABASE_ID,
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
  }
}

fn first_authority_request(algorithm: HashAlgorithm) -> FirstAuthorityPublicationRequestV1 {
  FirstAuthorityPublicationRequestV1 {
    database_id: DATABASE_ID,
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
    typed_closure_digest: digest_parts(algorithm, &[b"typed index-pointer closure"]),
    authority_identity: b"HEAD".to_vec(),
  }
}

fn retirement_owner(cancellation: &CancellationToken, memory: &MemoryCoordinator) -> RetirementJournalOwnerV1 {
  retirement_owner_for(ALGORITHM, cancellation, memory)
}

fn retirement_owner_for(
  algorithm: HashAlgorithm,
  cancellation: &CancellationToken,
  memory: &MemoryCoordinator,
) -> RetirementJournalOwnerV1 {
  RetirementJournalOwnerV1::new_chain(
    algorithm,
    DATABASE_ID,
    1,
    901,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    cancellation,
    memory,
  )
  .unwrap()
}

fn immutable_fixture(name: &str) -> EncodedImmutableIndexArtifactV1 {
  immutable_fixture_for(ALGORITHM, name)
}

fn immutable_fixture_for(algorithm: HashAlgorithm, name: &str) -> EncodedImmutableIndexArtifactV1 {
  let value = fs::read(fixture_root().join(name)).unwrap();
  let key = decode_index_manifest(&value, algorithm).unwrap().key;
  EncodedImmutableIndexArtifactV1 { key, value }
}

fn active_pointer_write(manifest: &EncodedImmutableIndexArtifactV1, slot: u8, sequence: u64) -> ActivePointerWriteV1<'_> {
  let manifest_view = decode_index_manifest(&manifest.value, ALGORITHM).unwrap();
  ActivePointerWriteV1 {
    kind: ActivePointerKindV1::FieldIndex,
    hash_algorithm: ALGORITHM,
    generation: manifest_view.generation,
    owner_id: manifest_view.owner_id,
    slot,
    sequence,
    target_manifest_hash: &manifest.key,
  }
}

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/index-artifact-v1")
}
