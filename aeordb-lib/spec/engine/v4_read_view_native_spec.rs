use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;

use aeordb::engine::directory_entry::{ChildEntry, serialize_child_entries};
use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::entity::EntryTypeV4;
use aeordb::engine::v4::first_authority::{
  FirstAuthorityPublicationRequestV1, PreparedNamespaceTreeV0, SuccessorAuthorityPublicationRequestV1, V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, decode_namespace_root, encode_semantic_state_object};
use aeordb::engine::v4::read_view::RootLifecycleObservationV1;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::{DiskKVStore, HashAlgorithm};
use tokio_util::sync::CancellationToken;

fn initial_header(algorithm: HashAlgorithm, kv_block_length: u64) -> DatabaseHeaderV4 {
  let hash_width = algorithm.hash_length();
  DatabaseHeaderV4 {
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

fn publisher(algorithm: HashAlgorithm) -> (tempfile::TempDir, PathBuf, V4FirstAuthorityPublisher) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("read-view-native.aeordb");
  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(&path).unwrap();
  let kv_block_length = initial_block_size() as u64;
  let header = initial_header(algorithm, kv_block_length);
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
  (directory, path, publisher)
}

fn semantic_state(
  algorithm: HashAlgorithm,
  reason: aeordb::engine::v4::namespace::SemanticUnavailableReasonV1,
) -> aeordb::engine::v4::namespace::EncodedSemanticObjectV1 {
  encode_semantic_state_object(
    &SemanticStateWriteV1 { required_capabilities: [0; 32], availability: SemanticAvailabilityV1::ContentOnly { reason } },
    algorithm,
  )
  .unwrap()
}

fn first_request(algorithm: HashAlgorithm) -> FirstAuthorityPublicationRequestV1 {
  FirstAuthorityPublicationRequestV1 {
    database_id: [0x31; 16],
    transaction_id: [0x61; 16],
    created_at_ms: 1_700_000_000_100,
    namespace_tree: PreparedNamespaceTreeV0 { root_hash: digest_parts(algorithm, &[b"dirc:"]), stored_value: Vec::new() },
    semantic_state: semantic_state(algorithm, aeordb::engine::v4::namespace::SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured),
    required_capabilities: [0; 32],
    typed_closure_digest: digest_parts(algorithm, &[b"read view first closure"]),
    authority_identity: b"HEAD".to_vec(),
  }
}

fn successor_request(algorithm: HashAlgorithm, expected_head_hash: Vec<u8>) -> SuccessorAuthorityPublicationRequestV1 {
  let created_at_ms = 1_700_000_000_200;
  let root_value = serialize_child_entries(
    &[ChildEntry {
      entry_type: EntryTypeV4::FileRecord.to_u8(),
      hash: digest_parts(algorithm, &[b"filec:successor.txt"]),
      total_size: 1,
      created_at: created_at_ms,
      updated_at: created_at_ms,
      name: "successor.txt".to_string(),
      content_type: Some("text/plain".to_string()),
      virtual_time: 1,
      node_id: 1,
    }],
    algorithm.hash_length(),
  )
  .unwrap();
  SuccessorAuthorityPublicationRequestV1 {
    database_id: [0x31; 16],
    transaction_id: [0x62; 16],
    created_at_ms: created_at_ms as u64,
    expected_head_hash,
    namespace_tree: PreparedNamespaceTreeV0 { root_hash: digest_parts(algorithm, &[b"dirc:", &root_value]), stored_value: root_value },
    semantic_state: semantic_state(algorithm, aeordb::engine::v4::namespace::SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured),
    required_capabilities: [0; 32],
    typed_closure_digest: digest_parts(algorithm, &[b"read view successor closure"]),
    authority_identity: b"HEAD".to_vec(),
  }
}

#[test]
fn captured_header_reader_loads_exact_current_authority_at_both_frozen_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, _path, publisher) = publisher(algorithm);
    let receipt = publisher.publish(&first_request(algorithm)).unwrap();
    let captured = receipt.observation.selected;
    let encoded_root = decode_namespace_root(&receipt.namespace_root.value, algorithm).unwrap();

    let loaded = publisher
      .load_namespace_authority_at_captured_header(&captured, &receipt.namespace_root.root_hash, &CancellationToken::new())
      .unwrap()
      .unwrap();

    assert_eq!(loaded.root.root_hash, receipt.namespace_root.root_hash);
    assert_eq!(loaded.namespace_tree.root_hash, encoded_root.namespace_tree_root);
    assert_eq!(loaded.semantic_state.object_id, encoded_root.semantic_state_root);
    assert_eq!(loaded.admission.namespace_root, receipt.namespace_root.root_hash);
    assert_eq!(loaded.admission.database_id, captured.header.database_id);
  }
}

#[test]
fn captured_header_reader_keeps_historical_authority_exact_after_head_advances() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = publisher(algorithm);
  let first = publisher.publish(&first_request(algorithm)).unwrap();
  let captured_first = first.observation.selected.clone();
  let successor = publisher.publish_successor_authority(&successor_request(algorithm, first.namespace_root.root_hash.clone())).unwrap();
  assert_ne!(successor.namespace_root.root_hash, first.namespace_root.root_hash);

  let historical = publisher
    .load_namespace_authority_at_captured_header(&captured_first, &first.namespace_root.root_hash, &CancellationToken::new())
    .unwrap()
    .unwrap();

  assert_eq!(historical.root.root_hash, first.namespace_root.root_hash);
  assert_eq!(historical.admission.publication_sequence, first.publication_sequence);
  assert!(historical.admission.publication_sequence <= captured_first.header.write_sequence_high_water);
}

#[test]
fn captured_header_reader_distinguishes_unknown_root_from_corrupt_admitted_closure() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, publisher) = publisher(algorithm);
  let receipt = publisher.publish(&first_request(algorithm)).unwrap();
  let captured = receipt.observation.selected;
  let encoded_root = decode_namespace_root(&receipt.namespace_root.value, algorithm).unwrap();
  let unknown = vec![0x99; algorithm.hash_length()];

  assert!(publisher.load_namespace_authority_at_captured_header(&captured, &unknown, &CancellationToken::new()).unwrap().is_none());

  let tree_locator = publisher.locator(&encoded_root.namespace_tree_root).unwrap().unwrap();
  let mut file = OpenOptions::new().read(true).write(true).open(path).unwrap();
  file.seek(SeekFrom::Start(tree_locator.offset + u64::from(tree_locator.total_length) - 1)).unwrap();
  file.write_all(&[0x7f]).unwrap();
  file.sync_all().unwrap();

  let error = publisher
    .load_namespace_authority_at_captured_header(&captured, &receipt.namespace_root.root_hash, &CancellationToken::new())
    .unwrap_err();
  assert_ne!(error.code(), "captured_authority_root_not_admitted");
}

#[test]
fn captured_header_reader_rejects_foreign_authority_and_cancellation() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = publisher(algorithm);
  let receipt = publisher.publish(&first_request(algorithm)).unwrap();
  let captured = receipt.observation.selected;

  let mut foreign = captured.clone();
  foreign.header.physical_instance_id = [0xa5; 16];
  let error = publisher
    .load_namespace_authority_at_captured_header(&foreign, &receipt.namespace_root.root_hash, &CancellationToken::new())
    .unwrap_err();
  assert_eq!(error.code(), "captured_authority_physical_instance");

  let cancellation = CancellationToken::new();
  cancellation.cancel();
  let error =
    publisher.load_namespace_authority_at_captured_header(&captured, &receipt.namespace_root.root_hash, &cancellation).unwrap_err();
  assert_eq!(error.code(), "captured_authority_cancelled");
}

#[test]
fn captured_header_reader_never_exposes_entities_published_after_its_high_water() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = publisher(algorithm);
  let first = publisher.publish(&first_request(algorithm)).unwrap();
  let captured_first = first.observation.selected;
  let successor = publisher.publish_successor_authority(&successor_request(algorithm, first.namespace_root.root_hash)).unwrap();

  let error = publisher
    .load_namespace_authority_at_captured_header(&captured_first, &successor.namespace_root.root_hash, &CancellationToken::new())
    .unwrap_err();
  assert_eq!(error.code(), "unreserved_write_sequence");
}

#[test]
fn selected_lifecycle_point_reader_treats_current_head_as_live_and_absent_controls_as_retained() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, _path, publisher) = publisher(algorithm);
    let receipt = publisher.publish(&first_request(algorithm)).unwrap();
    let captured = receipt.observation.selected;
    let memory = MemoryCoordinator::new(MemoryPolicy::new(8 * 1024 * 1024, 16 * 1024 * 1024, 1, 1024 * 1024).unwrap());
    let cancellation = CancellationToken::new();

    assert_eq!(
      publisher
        .observe_root_lifecycle_at_captured_header(&captured, &receipt.namespace_root.root_hash, 86_400_000, &cancellation, &memory,)
        .unwrap(),
      RootLifecycleObservationV1::Live,
    );
    assert_eq!(
      publisher
        .observe_root_lifecycle_at_captured_header(
          &captured,
          &digest_parts(algorithm, &[b"admitted historical root without lifecycle state"]),
          86_400_000,
          &cancellation,
          &memory,
        )
        .unwrap(),
      RootLifecycleObservationV1::Retained,
    );

    let canceled = CancellationToken::new();
    canceled.cancel();
    assert_eq!(
      publisher
        .observe_root_lifecycle_at_captured_header(&captured, &receipt.namespace_root.root_hash, 86_400_000, &canceled, &memory,)
        .unwrap_err()
        .code(),
      "root_lifecycle_read_canceled",
    );
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}
