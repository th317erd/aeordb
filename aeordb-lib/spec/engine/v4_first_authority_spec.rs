use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::sync::Arc;

use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::first_authority::{FirstAuthorityPublicationRequestV1, PreparedNamespaceTreeV0, V4FirstAuthorityPublisher};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, encode_semantic_state_object};
use aeordb::engine::v4::root_authority::decode_root_admission_commit;
use aeordb::engine::{DiskKVStore, HashAlgorithm};

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

fn publisher(algorithm: HashAlgorithm) -> (tempfile::TempDir, Arc<DurabilityCoordinator>, V4FirstAuthorityPublisher) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("first-authority.aeordb");
  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(path).unwrap();
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
  let publisher = V4FirstAuthorityPublisher::new(kv, coordinator.clone()).unwrap();
  (directory, coordinator, publisher)
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
  let namespace_tree_root = digest_parts(algorithm, &[b"dirc:"]);
  FirstAuthorityPublicationRequestV1 {
    database_id: [0x31; 16],
    transaction_id: [0x61; 16],
    created_at_ms: 1_700_000_000_100,
    namespace_tree: PreparedNamespaceTreeV0 { root_hash: namespace_tree_root, stored_value: Vec::new() },
    semantic_state,
    required_capabilities: [0; 32],
    typed_closure_digest: digest_parts(algorithm, &[b"typed test closure"]),
    authority_identity: b"HEAD".to_vec(),
  }
}

#[test]
fn first_authority_publishes_one_exact_root_and_witness_at_the_header_boundary() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, coordinator, publisher) = publisher(algorithm);
  let request = request(algorithm);
  let before = publisher.observe().unwrap();
  let expected_publication_sequence = coordinator.snapshot().unwrap().next_sequence;

  let receipt = publisher.publish(&request).unwrap();

  assert!(!receipt.idempotent);
  assert_eq!(receipt.publication_sequence, expected_publication_sequence);
  assert_eq!(receipt.observation.selected.header.head_hash, receipt.namespace_root.root_hash);
  assert_eq!(receipt.observation.selected.header.slot_sequence, before.selected.header.slot_sequence + 1);
  assert_eq!(receipt.observation.selected.header.write_sequence_high_water, before.selected.header.write_sequence_high_water + 8);
  assert_eq!(receipt.observation.selected.header.entry_count, before.selected.header.entry_count + 8);
  assert_eq!(receipt.observation.selected.header.required_reader_capabilities, before.selected.header.required_reader_capabilities);
  assert_eq!(receipt.observation.selected.header.required_writer_capabilities, before.selected.header.required_writer_capabilities);
  let admission = decode_root_admission_commit(&receipt.admission_control, algorithm).unwrap();
  assert_eq!(admission.namespace_root, receipt.namespace_root.root_hash);
  assert_eq!(admission.publication_sequence, receipt.publication_sequence);
  assert_eq!(admission.selected_header_slot_sequence, receipt.observation.selected.header.slot_sequence);
  assert!(publisher.locator(&receipt.namespace_root.root_hash).unwrap().is_some());
  assert!(publisher.admission_locator(&receipt.namespace_root.root_hash).unwrap().is_some());
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, receipt.publication_sequence);
}

#[test]
fn exact_retry_returns_the_selected_first_authority_without_another_publication() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, coordinator, publisher) = publisher(algorithm);
  let request = request(algorithm);
  let first = publisher.publish(&request).unwrap();
  let frontier = coordinator.snapshot().unwrap().hard_frontier;

  let retry = publisher.publish(&request).unwrap();

  assert!(retry.idempotent);
  assert_eq!(retry.namespace_root, first.namespace_root);
  assert_eq!(retry.admission_control, first.admission_control);
  assert_eq!(retry.publication_sequence, first.publication_sequence);
  assert_eq!(retry.observation, first.observation);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, frontier);
}

#[test]
fn first_authority_supports_the_frozen_sha512_identity_width() {
  let algorithm = HashAlgorithm::Sha512;
  let (_directory, coordinator, publisher) = publisher(algorithm);
  let request = request(algorithm);

  let receipt = publisher.publish(&request).unwrap();
  let admission = decode_root_admission_commit(&receipt.admission_control, algorithm).unwrap();

  assert_eq!(receipt.namespace_root.root_hash.len(), algorithm.hash_length());
  assert_eq!(receipt.observation.selected.header.head_hash, receipt.namespace_root.root_hash);
  assert_eq!(admission.namespace_root, receipt.namespace_root.root_hash);
  assert_eq!(admission.publication_sequence, receipt.publication_sequence);
  assert!(publisher.publish(&request).unwrap().idempotent);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, receipt.publication_sequence);
}

#[test]
fn first_authority_allows_only_reviewed_disconnected_owners_and_exclusively_owns_atomic_root_publication() {
  fn collect_rust_files(directory: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(directory).unwrap() {
      let path = entry.unwrap().path();
      if path.is_dir() {
        collect_rust_files(&path, files);
      } else if path.extension().is_some_and(|extension| extension == "rs") {
        files.push(path);
      }
    }
  }

  let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let first_authority_path = source_root.join("engine/v4/first_authority.rs");
  let index_recovery_store_path = source_root.join("engine/v4/index_recovery_store.rs");
  let migration_base_clone_execution_path = source_root.join("engine/v4/migration_base_clone_execution.rs");
  let migration_capture_replay_path = source_root.join("engine/v4/migration_capture_replay.rs");
  let migration_destination_path = source_root.join("engine/v4/migration_destination.rs");
  let migration_final_reconciliation_path = source_root.join("engine/v4/migration_final_reconciliation.rs");
  let migration_owner_path = source_root.join("engine/v4/migration_owner.rs");
  let disk_kv_path = source_root.join("engine/disk_kv_store.rs");
  let header_publication_path = source_root.join("engine/v4/header_publication.rs");
  let mut files = Vec::new();
  collect_rust_files(&source_root, &mut files);

  let mut publisher_callers: Vec<_> = files
    .iter()
    .filter(|path| *path != &first_authority_path)
    .filter(|path| std::fs::read_to_string(path).unwrap().contains("V4FirstAuthorityPublisher"))
    .collect();
  publisher_callers.sort();
  assert_eq!(
    publisher_callers,
    vec![
      &index_recovery_store_path,
      &migration_base_clone_execution_path,
      &migration_capture_replay_path,
      &migration_destination_path,
      &migration_final_reconciliation_path,
      &migration_owner_path,
    ],
    "first-authority publisher escaped the reviewed disconnected owners: {publisher_callers:?}"
  );
  for owner_path in [
    &index_recovery_store_path,
    &migration_base_clone_execution_path,
    &migration_capture_replay_path,
    &migration_destination_path,
    &migration_final_reconciliation_path,
    &migration_owner_path,
  ] {
    let owner_source = std::fs::read_to_string(owner_path).unwrap();
    for forbidden in ["DirectoryOps", "crate::server", "tokio::spawn"] {
      assert!(!owner_source.contains(forbidden), "disconnected owner {owner_path:?} gained live activation token {forbidden}");
    }
    if owner_path == &migration_final_reconciliation_path {
      assert!(owner_source.contains("MigrationSourceWriteFreezeV1"));
      assert!(owner_source.contains("StorageEngine"));
    } else {
      assert!(!owner_source.contains("StorageEngine"), "disconnected owner {owner_path:?} gained direct v3 engine ownership");
    }
  }

  for method in ["begin_atomic_visibility_batch", "publish_atomic_visibility_after_authority", "admit_inactive_slot_with_dependency_bytes"]
  {
    let owners: Vec<_> = files
      .iter()
      .filter(|path| *path != &disk_kv_path && *path != &header_publication_path)
      .filter(|path| std::fs::read_to_string(path).unwrap().contains(method))
      .collect();
    assert_eq!(owners, vec![&first_authority_path], "{method} escaped first-authority ownership: {owners:?}");
  }
}
