use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::hot_tail::read_hot_tail_checked;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::first_authority::{
  FirstAuthorityPublicationRequestV1, ImmutableSystemControlBatchPublicationRequestV1, ImmutableSystemControlWriteV1,
  PreparedNamespaceTreeV0, V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::migration_root_map::{
  LegacyRootMapPageBodyV1, LegacyRootMapRowV1, LegacyRootSemanticAvailabilityV1, encode_legacy_root_map_page,
  legacy_root_map_page_identity_hash,
};
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, encode_semantic_state_object};
use aeordb::engine::v4::system_control::SystemControlKindV1;
use aeordb::engine::{DiskKVStore, HashAlgorithm};

const DATABASE_ID: [u8; 16] = [0x31; 16];
const OTHER_DATABASE_ID: [u8; 16] = [0x32; 16];
const MIGRATION_ID: [u8; 16] = [0x71; 16];
const SOURCE_PHYSICAL_ID: [u8; 16] = [0x41; 16];
const DESTINATION_PHYSICAL_ID: [u8; 16] = [0x51; 16];

fn initial_header_for(algorithm: HashAlgorithm, kv_block_length: u64) -> DatabaseHeaderV4 {
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
    physical_instance_id: DESTINATION_PHYSICAL_ID,
  }
}

fn create_publisher_for(algorithm: HashAlgorithm) -> (tempfile::TempDir, PathBuf, V4FirstAuthorityPublisher) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("immutable-system-control.aeordb");
  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(&path).unwrap();
  let header = initial_header_for(algorithm, initial_block_size());
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
  publisher.publish(&first_authority_request_for(algorithm)).unwrap();
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

fn first_authority_request_for(algorithm: HashAlgorithm) -> FirstAuthorityPublicationRequestV1 {
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
    typed_closure_digest: digest_parts(algorithm, &[b"typed immutable-control closure"]),
    authority_identity: b"HEAD".to_vec(),
  }
}

fn page_identity(ordinal: u64) -> Vec<u8> {
  let mut identity = MIGRATION_ID.to_vec();
  identity.extend_from_slice(&ordinal.to_le_bytes());
  identity
}

fn page_control(algorithm: HashAlgorithm, ordinal: u64, previous: Vec<u8>, next: Vec<u8>, row_byte: u8) -> Vec<u8> {
  encode_legacy_root_map_page(
    &LegacyRootMapPageBodyV1 {
      database_id: DATABASE_ID,
      migration_id: MIGRATION_ID,
      logical_database_id: DATABASE_ID,
      source_physical_instance_id: SOURCE_PHYSICAL_ID,
      destination_physical_instance_id: DESTINATION_PHYSICAL_ID,
      page_ordinal: ordinal,
      previous_page_hash: previous,
      next_page_hash: next,
      rows: vec![LegacyRootMapRowV1 {
        legacy_root_hash: vec![row_byte; algorithm.hash_length()],
        namespace_root_v1_hash: vec![row_byte + 1; algorithm.hash_length()],
        semantic_availability: LegacyRootSemanticAvailabilityV1::Complete,
        captured_source_write_sequence: ordinal + 11,
      }],
    },
    algorithm,
  )
  .unwrap()
}

fn two_pages(algorithm: HashAlgorithm) -> (Vec<u8>, Vec<u8>) {
  let first_hash = legacy_root_map_page_identity_hash(algorithm, DATABASE_ID, MIGRATION_ID, 0).unwrap();
  let second_hash = legacy_root_map_page_identity_hash(algorithm, DATABASE_ID, MIGRATION_ID, 1).unwrap();
  (
    page_control(algorithm, 0, vec![0; algorithm.hash_length()], second_hash, 0x61),
    page_control(algorithm, 1, first_hash, vec![0; algorithm.hash_length()], 0x71),
  )
}

fn publication_request<'a>(
  controls: &'a [ImmutableSystemControlWriteV1<'a>],
  database_id: &'a [u8; 16],
  publication_timestamp_ms: u64,
) -> ImmutableSystemControlBatchPublicationRequestV1<'a> {
  ImmutableSystemControlBatchPublicationRequestV1 { database_id, controls, publication_timestamp_ms }
}

#[test]
fn immutable_system_controls_publish_in_one_authority_batch_retry_and_reopen() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, publisher) = create_publisher_for(algorithm);
    let (first, second) = two_pages(algorithm);
    let first_identity = page_identity(0);
    let second_identity = page_identity(1);
    let writes = [
      ImmutableSystemControlWriteV1 { kind: SystemControlKindV1::LegacyRootMapPage, identity: &first_identity, encoded_control: &first },
      ImmutableSystemControlWriteV1 { kind: SystemControlKindV1::LegacyRootMapPage, identity: &second_identity, encoded_control: &second },
    ];
    let request = ImmutableSystemControlBatchPublicationRequestV1 {
      database_id: &DATABASE_ID,
      controls: &writes,
      publication_timestamp_ms: 1_700_000_000_200,
    };

    let receipt = publisher.publish_immutable_system_controls(request).unwrap();
    assert_eq!(receipt.controls.len(), 2);
    assert!(!receipt.idempotent);
    assert!(receipt.controls.iter().all(|control| !control.idempotent && control.control_sequence == 1));
    let selected_header_sequence = receipt.observation.selected.header.slot_sequence;

    for (identity, bytes) in [(&first_identity, &first), (&second_identity, &second)] {
      let loaded =
        publisher.load_immutable_system_control(SystemControlKindV1::LegacyRootMapPage, &DATABASE_ID, identity).unwrap().unwrap();
      assert_eq!(loaded.control_sequence, 1);
      assert_eq!(&loaded.bytes, bytes);
      assert_eq!(loaded.payload_hash, digest_parts(algorithm, &[bytes]));
    }

    let retry = publisher.publish_immutable_system_controls(request).unwrap();
    assert!(retry.idempotent);
    assert!(retry.controls.iter().all(|control| control.idempotent));
    assert_eq!(retry.observation.selected.header.slot_sequence, selected_header_sequence);

    let reopened = reopen(&path);
    assert_eq!(
      reopened
        .load_immutable_system_control(SystemControlKindV1::LegacyRootMapPage, &DATABASE_ID, &second_identity)
        .unwrap()
        .unwrap()
        .bytes,
      second
    );
  }
}

#[test]
fn immutable_system_controls_reject_malformed_identity_kind_batch_and_collision() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = create_publisher_for(algorithm);
  let (first, _) = two_pages(algorithm);
  let identity = page_identity(0);
  let write = ImmutableSystemControlWriteV1 { kind: SystemControlKindV1::LegacyRootMapPage, identity: &identity, encoded_control: &first };
  assert!(publisher.load_immutable_system_control(SystemControlKindV1::LegacyRootMapPage, &DATABASE_ID, &identity).unwrap().is_none());
  assert_eq!(
    publisher.publish_immutable_system_controls(publication_request(&[], &DATABASE_ID, 1)).unwrap_err().code(),
    "immutable_system_control_count"
  );
  let oversized = vec![write; 256];
  assert_eq!(
    publisher.publish_immutable_system_controls(publication_request(&oversized, &DATABASE_ID, 1)).unwrap_err().code(),
    "immutable_system_control_count"
  );
  assert_eq!(
    publisher.publish_immutable_system_controls(publication_request(&[write], &DATABASE_ID, 0)).unwrap_err().code(),
    "immutable_system_control_publication_time"
  );
  assert_eq!(
    publisher.publish_immutable_system_controls(publication_request(&[write], &DATABASE_ID, i64::MAX as u64 + 1)).unwrap_err().code(),
    "immutable_system_control_publication_time"
  );
  assert_eq!(
    publisher.publish_immutable_system_controls(publication_request(&[write], &OTHER_DATABASE_ID, 1)).unwrap_err().code(),
    "immutable_system_control_database_mismatch"
  );
  assert_eq!(
    publisher.publish_immutable_system_controls(publication_request(&[write, write], &DATABASE_ID, 1)).unwrap_err().code(),
    "immutable_system_control_duplicate"
  );

  let mutable_kind = ImmutableSystemControlWriteV1 { kind: SystemControlKindV1::MigrationProgress, ..write };
  assert_eq!(
    publisher.publish_immutable_system_controls(publication_request(&[mutable_kind], &DATABASE_ID, 1)).unwrap_err().code(),
    "immutable_system_control_kind"
  );
  let wrong_identity = [0x99; 24];
  let mismatched = ImmutableSystemControlWriteV1 { identity: &wrong_identity, ..write };
  assert_eq!(
    publisher.publish_immutable_system_controls(publication_request(&[mismatched], &DATABASE_ID, 1)).unwrap_err().code(),
    "immutable_system_control_prepared_mismatch"
  );
  let mut corrupt = first.clone();
  *corrupt.last_mut().unwrap() ^= 1;
  let malformed = ImmutableSystemControlWriteV1 { encoded_control: &corrupt, ..write };
  assert_eq!(
    publisher.publish_immutable_system_controls(publication_request(&[malformed], &DATABASE_ID, 1)).unwrap_err().code(),
    "system_control_crc"
  );

  publisher.publish_immutable_system_controls(publication_request(&[write], &DATABASE_ID, 1_700_000_000_200)).unwrap();
  let changed = page_control(algorithm, 0, vec![0; algorithm.hash_length()], vec![0; algorithm.hash_length()], 0x62);
  let collision = ImmutableSystemControlWriteV1 { encoded_control: &changed, ..write };
  assert_eq!(
    publisher.publish_immutable_system_controls(publication_request(&[collision], &DATABASE_ID, 1_700_000_000_200)).unwrap_err().code(),
    "immutable_entity_identity_collision"
  );

  assert_eq!(
    publisher.load_immutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &identity).unwrap_err().code(),
    "immutable_system_control_kind"
  );
  assert_eq!(
    publisher.load_immutable_system_control(SystemControlKindV1::LegacyRootMapPage, &OTHER_DATABASE_ID, &identity).unwrap_err().code(),
    "immutable_system_control_database_mismatch"
  );
}

#[test]
fn immutable_system_control_authority_delegates_to_the_shared_stable_batch_path() {
  let source = std::fs::read_to_string(format!("{}/src/engine/v4/first_authority.rs", env!("CARGO_MANIFEST_DIR"))).unwrap();
  let method = source
    .split_once("fn publish_immutable_system_controls_with_observer(")
    .and_then(|(_, remainder)| remainder.split_once("pub fn ").map(|(method, _)| method))
    .expect("immutable system-control publisher");
  assert!(method.contains("publish_immutable_entity_batch_with_validation"));
  assert!(!method.contains("commit_stable_entity_dependency("));
}
