use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;

use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::hot_tail::read_hot_tail_checked;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::first_authority::{
  FirstAuthorityPublicationRequestV1, MutableSystemControlExpectationV1, MutableSystemControlGuardV1,
  MutableSystemControlPublicationRequestV1, PreparedNamespaceTreeV0, V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::migration_control::{MigrationLeaseBodyV1, MigrationLeaseStateV1, encode_migration_lease_control};
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, encode_semantic_state_object};
use aeordb::engine::v4::system_control::{SystemControlKindV1, SystemControlSlotV1, system_control_path};
use aeordb::engine::{DiskKVStore, HashAlgorithm};
use tokio_util::sync::CancellationToken;

const DATABASE_ID: [u8; 16] = [0x31; 16];
const MIGRATION_ID: [u8; 16] = [0x71; 16];
const OTHER_MIGRATION_ID: [u8; 16] = [0x72; 16];
const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

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
    physical_instance_id: [0x51; 16],
  }
}

fn create_publisher() -> (tempfile::TempDir, PathBuf, V4FirstAuthorityPublisher) {
  create_publisher_for(ALGORITHM)
}

fn create_publisher_for(algorithm: HashAlgorithm) -> (tempfile::TempDir, PathBuf, V4FirstAuthorityPublisher) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("mutable-control.aeordb");
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
    typed_closure_digest: digest_parts(algorithm, &[b"typed mutable-control closure"]),
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

fn lease_control(sequence: u64, renewed_at_ms: i64) -> Vec<u8> {
  lease_control_for(ALGORITHM, sequence, renewed_at_ms)
}

fn lease_control_for(algorithm: HashAlgorithm, sequence: u64, renewed_at_ms: i64) -> Vec<u8> {
  lease_control_for_identity(algorithm, MIGRATION_ID, sequence, renewed_at_ms)
}

fn lease_control_for_identity(algorithm: HashAlgorithm, migration_id: [u8; 16], sequence: u64, renewed_at_ms: i64) -> Vec<u8> {
  encode_migration_lease_control(
    sequence,
    &MigrationLeaseBodyV1 {
      database_id: DATABASE_ID,
      migration_id,
      source_physical_instance_id: [0x41; 16],
      destination_physical_instance_id: [0x51; 16],
      holder_boot_id: [0x61; 16],
      fencing_token: 1,
      acquired_at_ms: 1_700_000_000_200,
      renewed_at_ms,
      expires_at_ms: renewed_at_ms + 60_000,
      source_header_sequence: 41,
      state: MigrationLeaseStateV1::Held,
    },
    algorithm,
  )
  .unwrap()
}

fn expectation(selected: &aeordb::engine::v4::first_authority::LoadedMutableSystemControlV1) -> MutableSystemControlExpectationV1 {
  MutableSystemControlExpectationV1 {
    selected_slot: selected.selected_slot,
    control_sequence: selected.control_sequence,
    control_digest: selected.control_digest.clone(),
  }
}

fn publish_lease(
  publisher: &V4FirstAuthorityPublisher,
  retirement: &mut RetirementJournalOwnerV1,
  expected: Option<MutableSystemControlExpectationV1>,
  bytes: &[u8],
  timestamp: u64,
) -> Result<
  aeordb::engine::v4::first_authority::MutableSystemControlPublicationReceiptV1,
  aeordb::engine::v4::first_authority::MutableSystemControlPublicationErrorV1,
> {
  publish_lease_guarded(publisher, retirement, &MIGRATION_ID, expected, &[], bytes, timestamp)
}

fn publish_lease_guarded(
  publisher: &V4FirstAuthorityPublisher,
  retirement: &mut RetirementJournalOwnerV1,
  identity: &[u8],
  expected: Option<MutableSystemControlExpectationV1>,
  guards: &[MutableSystemControlGuardV1<'_>],
  bytes: &[u8],
  timestamp: u64,
) -> Result<
  aeordb::engine::v4::first_authority::MutableSystemControlPublicationReceiptV1,
  aeordb::engine::v4::first_authority::MutableSystemControlPublicationErrorV1,
> {
  publisher.publish_mutable_system_control(
    MutableSystemControlPublicationRequestV1 {
      database_id: &DATABASE_ID,
      kind: SystemControlKindV1::MigrationLease,
      identity,
      expected,
      guards,
      encoded_control: bytes,
      publication_timestamp_ms: timestamp,
      monotonic_now_ms: timestamp,
    },
    retirement,
  )
}

#[test]
fn mutable_control_authority_selects_a_b_a_with_lineage_and_reopens_exactly() {
  let (_directory, path, publisher) = create_publisher();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(&cancellation, &memory);
  let mut expected = None;

  for (sequence, slot) in [(1, SystemControlSlotV1::A), (2, SystemControlSlotV1::B), (3, SystemControlSlotV1::A)] {
    let bytes = lease_control(sequence, 1_700_000_000_200 + sequence as i64);
    let receipt = publisher
      .publish_mutable_system_control(
        MutableSystemControlPublicationRequestV1 {
          database_id: &DATABASE_ID,
          kind: SystemControlKindV1::MigrationLease,
          identity: &MIGRATION_ID,
          expected,
          guards: &[],
          encoded_control: &bytes,
          publication_timestamp_ms: 1_700_000_000_300 + sequence,
          monotonic_now_ms: 10_000 + sequence,
        },
        &mut retirement,
      )
      .unwrap();
    assert_eq!(receipt.selected_slot, slot);
    assert_eq!(receipt.control_sequence, sequence);
    assert_eq!(receipt.replaced_slot, sequence == 3);
    assert_eq!(receipt.retirement_hard_publication_sequence.is_some(), sequence == 3);
    let selected =
      publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
    assert_eq!(selected.bytes, bytes);
    expected = Some(MutableSystemControlExpectationV1 {
      selected_slot: selected.selected_slot,
      control_sequence: selected.control_sequence,
      control_digest: selected.control_digest,
    });
  }
  drop(publisher);

  let reopened = reopen(&path);
  let selected = reopened.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  assert_eq!(selected.selected_slot, SystemControlSlotV1::A);
  assert_eq!(selected.control_sequence, 3);
  assert_eq!(selected.bytes, lease_control(3, 1_700_000_000_203));
}

#[test]
fn mutable_control_authority_preserves_the_widest_hash_profile() {
  let algorithm = HashAlgorithm::Sha512;
  let (_directory, path, publisher) = create_publisher_for(algorithm);
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner_for(algorithm, &cancellation, &memory);
  let first = lease_control_for(algorithm, 1, 1_700_000_000_201);
  let first_receipt = publisher
    .publish_mutable_system_control(
      MutableSystemControlPublicationRequestV1 {
        database_id: &DATABASE_ID,
        kind: SystemControlKindV1::MigrationLease,
        identity: &MIGRATION_ID,
        expected: None,
        guards: &[],
        encoded_control: &first,
        publication_timestamp_ms: 1_700_000_000_301,
        monotonic_now_ms: 1,
      },
      &mut retirement,
    )
    .unwrap();
  assert_eq!(first_receipt.control_digest.len(), algorithm.hash_length());
  let second = lease_control_for(algorithm, 2, 1_700_000_000_202);
  let second_receipt = publisher
    .publish_mutable_system_control(
      MutableSystemControlPublicationRequestV1 {
        database_id: &DATABASE_ID,
        kind: SystemControlKindV1::MigrationLease,
        identity: &MIGRATION_ID,
        expected: Some(MutableSystemControlExpectationV1 {
          selected_slot: first_receipt.selected_slot,
          control_sequence: first_receipt.control_sequence,
          control_digest: first_receipt.control_digest,
        }),
        guards: &[],
        encoded_control: &second,
        publication_timestamp_ms: 1_700_000_000_302,
        monotonic_now_ms: 2,
      },
      &mut retirement,
    )
    .unwrap();
  assert_eq!(second_receipt.selected_slot, SystemControlSlotV1::B);
  assert_eq!(second_receipt.control_digest.len(), algorithm.hash_length());
  drop(publisher);

  let reopened = reopen(&path);
  let selected = reopened.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  assert_eq!(selected.control_digest.len(), algorithm.hash_length());
  assert_eq!(selected.bytes, second);
}

#[test]
fn mutable_control_authority_is_idempotent_and_rejects_stale_or_malformed_cas() {
  let (_directory, _path, publisher) = create_publisher();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(&cancellation, &memory);
  let first = lease_control(1, 1_700_000_000_201);
  let first_receipt = publish_lease(&publisher, &mut retirement, None, &first, 1_700_000_000_301).unwrap();
  let after_first =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();

  let retry = publish_lease(
    &publisher,
    &mut retirement,
    Some(MutableSystemControlExpectationV1 {
      selected_slot: SystemControlSlotV1::Immutable,
      control_sequence: 0,
      control_digest: Vec::new(),
    }),
    &first,
    1_700_000_000_302,
  )
  .unwrap();
  assert!(retry.idempotent);
  assert_eq!(retry.control_sequence, first_receipt.control_sequence);
  assert_eq!(retry.observation, first_receipt.observation);

  let second = lease_control(2, 1_700_000_000_202);
  for malformed in [
    MutableSystemControlExpectationV1 {
      selected_slot: SystemControlSlotV1::Immutable,
      control_sequence: after_first.control_sequence,
      control_digest: after_first.control_digest.clone(),
    },
    MutableSystemControlExpectationV1 {
      selected_slot: after_first.selected_slot,
      control_sequence: 0,
      control_digest: after_first.control_digest.clone(),
    },
    MutableSystemControlExpectationV1 {
      selected_slot: after_first.selected_slot,
      control_sequence: after_first.control_sequence,
      control_digest: vec![0; ALGORITHM.hash_length()],
    },
  ] {
    let error = publish_lease(&publisher, &mut retirement, Some(malformed), &second, 1_700_000_000_303).unwrap_err();
    assert_eq!(error.code(), "mutable_control_expectation");
  }
  let missing = publish_lease(&publisher, &mut retirement, None, &second, 1_700_000_000_304).unwrap_err();
  assert_eq!(missing.code(), "mutable_control_selector_conflict");

  let first_expectation = expectation(&after_first);
  publish_lease(&publisher, &mut retirement, Some(first_expectation.clone()), &second, 1_700_000_000_305).unwrap();
  let stale = publish_lease(&publisher, &mut retirement, Some(first_expectation), &lease_control(3, 1_700_000_000_203), 1_700_000_000_306)
    .unwrap_err();
  assert_eq!(stale.code(), "mutable_control_selector_conflict");
}

#[test]
fn mutable_control_authority_rejects_foreign_kind_database_identity_and_sequence() {
  let (_directory, _path, publisher) = create_publisher();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(&cancellation, &memory);
  let bytes = lease_control(1, 1_700_000_000_201);

  let wrong_database = publisher
    .publish_mutable_system_control(
      MutableSystemControlPublicationRequestV1 {
        database_id: &[0x99; 16],
        kind: SystemControlKindV1::MigrationLease,
        identity: &MIGRATION_ID,
        expected: None,
        guards: &[],
        encoded_control: &bytes,
        publication_timestamp_ms: 1_700_000_000_301,
        monotonic_now_ms: 1,
      },
      &mut retirement,
    )
    .unwrap_err();
  assert_eq!(wrong_database.code(), "mutable_control_database_mismatch");

  let immutable = publisher
    .publish_mutable_system_control(
      MutableSystemControlPublicationRequestV1 {
        database_id: &DATABASE_ID,
        kind: SystemControlKindV1::LegacyRootMapPage,
        identity: &MIGRATION_ID,
        expected: None,
        guards: &[],
        encoded_control: &bytes,
        publication_timestamp_ms: 1_700_000_000_301,
        monotonic_now_ms: 1,
      },
      &mut retirement,
    )
    .unwrap_err();
  assert_eq!(immutable.code(), "mutable_control_kind");

  let wrong_identity = publisher
    .publish_mutable_system_control(
      MutableSystemControlPublicationRequestV1 {
        database_id: &DATABASE_ID,
        kind: SystemControlKindV1::MigrationLease,
        identity: &[0x72; 16],
        expected: None,
        guards: &[],
        encoded_control: &bytes,
        publication_timestamp_ms: 1_700_000_000_301,
        monotonic_now_ms: 1,
      },
      &mut retirement,
    )
    .unwrap_err();
  assert_eq!(wrong_identity.code(), "mutable_control_prepared_mismatch");

  let skipped = publish_lease(&publisher, &mut retirement, None, &lease_control(2, 1_700_000_000_202), 1_700_000_000_302).unwrap_err();
  assert_eq!(skipped.code(), "mutable_control_sequence");
  assert!(publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().is_none());
}

#[test]
fn mutable_control_authority_rejects_out_of_range_time_before_any_durable_mutation() {
  let (_directory, path, publisher) = create_publisher();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(&cancellation, &memory);
  let before = std::fs::read(&path).unwrap();

  let error = publish_lease(&publisher, &mut retirement, None, &lease_control(1, 1_700_000_000_201), i64::MAX as u64 + 1).unwrap_err();

  assert_eq!(error.code(), "mutable_control_timestamp_range");
  assert_eq!(std::fs::read(&path).unwrap(), before, "invalid input must not reserve a header sequence or append WAL bytes");
  assert!(publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().is_none());
}

#[test]
fn mutable_control_authority_rejects_a_stale_cross_control_guard_without_target_publication() {
  let (_directory, _path, publisher) = create_publisher();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(&cancellation, &memory);
  let first = lease_control(1, 1_700_000_000_201);
  publish_lease(&publisher, &mut retirement, None, &first, 1_700_000_000_301).unwrap();
  let guarded = publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let stale_guard =
    MutableSystemControlGuardV1 { kind: SystemControlKindV1::MigrationLease, identity: &MIGRATION_ID, expected: expectation(&guarded) };

  let other_first = lease_control_for_identity(ALGORITHM, OTHER_MIGRATION_ID, 1, 1_700_000_000_201);
  publisher
    .publish_mutable_system_control(
      MutableSystemControlPublicationRequestV1 {
        database_id: &DATABASE_ID,
        kind: SystemControlKindV1::MigrationLease,
        identity: &OTHER_MIGRATION_ID,
        expected: None,
        guards: &[],
        encoded_control: &other_first,
        publication_timestamp_ms: 1_700_000_000_302,
        monotonic_now_ms: 2,
      },
      &mut retirement,
    )
    .unwrap();
  let other_selected =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &OTHER_MIGRATION_ID).unwrap().unwrap();

  let second = lease_control(2, 1_700_000_000_202);
  publish_lease(&publisher, &mut retirement, Some(expectation(&guarded)), &second, 1_700_000_000_303).unwrap();
  let other_second = lease_control_for_identity(ALGORITHM, OTHER_MIGRATION_ID, 2, 1_700_000_000_202);
  let error = publisher
    .publish_mutable_system_control(
      MutableSystemControlPublicationRequestV1 {
        database_id: &DATABASE_ID,
        kind: SystemControlKindV1::MigrationLease,
        identity: &OTHER_MIGRATION_ID,
        expected: Some(expectation(&other_selected)),
        guards: &[stale_guard],
        encoded_control: &other_second,
        publication_timestamp_ms: 1_700_000_000_304,
        monotonic_now_ms: 4,
      },
      &mut retirement,
    )
    .unwrap_err();

  assert_eq!(error.code(), "mutable_control_guard_conflict");
  let selected =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &OTHER_MIGRATION_ID).unwrap().unwrap();
  assert_eq!(selected.control_sequence, 1);
  assert_eq!(selected.bytes, other_first);
}

#[test]
fn mutable_control_authority_bounds_and_validates_cross_control_guards() {
  let (_directory, _path, publisher) = create_publisher();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(&cancellation, &memory);
  let first = lease_control(1, 1_700_000_000_201);
  publish_lease(&publisher, &mut retirement, None, &first, 1_700_000_000_301).unwrap();
  let guarded = publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let valid_guard =
    MutableSystemControlGuardV1 { kind: SystemControlKindV1::MigrationLease, identity: &MIGRATION_ID, expected: expectation(&guarded) };

  let other_first = lease_control_for_identity(ALGORITHM, OTHER_MIGRATION_ID, 1, 1_700_000_000_201);
  publish_lease_guarded(&publisher, &mut retirement, &OTHER_MIGRATION_ID, None, &[], &other_first, 1_700_000_000_302).unwrap();
  let target =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &OTHER_MIGRATION_ID).unwrap().unwrap();
  let other_second = lease_control_for_identity(ALGORITHM, OTHER_MIGRATION_ID, 2, 1_700_000_000_202);

  let malformed = MutableSystemControlGuardV1 {
    expected: MutableSystemControlExpectationV1 {
      selected_slot: guarded.selected_slot,
      control_sequence: guarded.control_sequence,
      control_digest: vec![0; ALGORITHM.hash_length()],
    },
    ..valid_guard.clone()
  };
  let error = publish_lease_guarded(
    &publisher,
    &mut retirement,
    &OTHER_MIGRATION_ID,
    Some(expectation(&target)),
    &[malformed],
    &other_second,
    1_700_000_000_303,
  )
  .unwrap_err();
  assert_eq!(error.code(), "mutable_control_guard_expectation");

  let duplicate = [valid_guard.clone(), valid_guard.clone()];
  let error = publish_lease_guarded(
    &publisher,
    &mut retirement,
    &OTHER_MIGRATION_ID,
    Some(expectation(&target)),
    &duplicate,
    &other_second,
    1_700_000_000_304,
  )
  .unwrap_err();
  assert_eq!(error.code(), "mutable_control_guard_duplicate");

  let excessive = vec![valid_guard.clone(); 9];
  let error = publish_lease_guarded(
    &publisher,
    &mut retirement,
    &OTHER_MIGRATION_ID,
    Some(expectation(&target)),
    &excessive,
    &other_second,
    1_700_000_000_305,
  )
  .unwrap_err();
  assert_eq!(error.code(), "mutable_control_guard_count");

  let immutable = MutableSystemControlGuardV1 { kind: SystemControlKindV1::LegacyRootMapPage, ..valid_guard.clone() };
  let error = publish_lease_guarded(
    &publisher,
    &mut retirement,
    &OTHER_MIGRATION_ID,
    Some(expectation(&target)),
    &[immutable],
    &other_second,
    1_700_000_000_306,
  )
  .unwrap_err();
  assert_eq!(error.code(), "mutable_control_kind");

  let retry = publish_lease_guarded(
    &publisher,
    &mut retirement,
    &OTHER_MIGRATION_ID,
    None,
    &[MutableSystemControlGuardV1 {
      expected: MutableSystemControlExpectationV1 {
        selected_slot: SystemControlSlotV1::Immutable,
        control_sequence: 0,
        control_digest: Vec::new(),
      },
      ..valid_guard
    }],
    &other_first,
    1_700_000_000_307,
  )
  .unwrap();
  assert!(retry.idempotent, "an already-selected target must remain an exact retry even if a guard subsequently changed");
  let selected =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &OTHER_MIGRATION_ID).unwrap().unwrap();
  assert_eq!(selected.control_sequence, 1);
  assert_eq!(selected.bytes, other_first);
}

#[test]
fn mutable_control_authority_serializes_concurrent_compare_and_swap_contenders() {
  let (_directory, _path, publisher) = create_publisher();
  let publisher = Arc::new(publisher);
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(&cancellation, &memory);
  let first = lease_control(1, 1_700_000_000_201);
  publish_lease(&publisher, &mut retirement, None, &first, 1_700_000_000_301).unwrap();
  let selected = publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let expected = expectation(&selected);
  let barrier = Arc::new(Barrier::new(3));
  let mut workers = Vec::new();

  for (renewed_at_ms, timestamp) in [(1_700_000_000_202, 1_700_000_000_302), (1_700_000_000_203, 1_700_000_000_303)] {
    let publisher = publisher.clone();
    let expected = expected.clone();
    let barrier = barrier.clone();
    workers.push(thread::spawn(move || {
      let cancellation = CancellationToken::new();
      let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
      let mut retirement = retirement_owner(&cancellation, &memory);
      let bytes = lease_control(2, renewed_at_ms);
      barrier.wait();
      let result = publish_lease(&publisher, &mut retirement, Some(expected), &bytes, timestamp)
        .map(|receipt| receipt.idempotent)
        .map_err(|error| error.code());
      (bytes, result)
    }));
  }
  barrier.wait();
  let results: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
  assert_eq!(results.iter().filter(|(_, result)| result.is_ok()).count(), 1);
  assert_eq!(
    results.iter().filter_map(|(_, result)| result.as_ref().err()).copied().collect::<Vec<_>>(),
    vec!["mutable_control_selector_conflict"]
  );
  let winner = results.iter().find(|(_, result)| result.is_ok()).unwrap();
  assert_eq!(winner.1, Ok(false));
  let selected = publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  assert_eq!(selected.control_sequence, 2);
  assert_eq!(selected.bytes, winner.0);
}

#[test]
fn mutable_control_authority_fails_closed_on_corrupt_persisted_slot() {
  let (_directory, path, publisher) = create_publisher();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(&cancellation, &memory);
  let bytes = lease_control(1, 1_700_000_000_201);
  publish_lease(&publisher, &mut retirement, None, &bytes, 1_700_000_000_301).unwrap();
  let slot_path = system_control_path(SystemControlKindV1::MigrationLease, &MIGRATION_ID, SystemControlSlotV1::A).unwrap();
  let path_key = digest_parts(ALGORITHM, &[b"file:", slot_path.as_bytes()]);
  let locator = publisher.locator(&path_key).unwrap().unwrap();
  drop(publisher);

  let mut file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
  file.seek(SeekFrom::Start(locator.offset)).unwrap();
  let mut byte = [0; 1];
  file.read_exact(&mut byte).unwrap();
  byte[0] ^= 0xff;
  file.seek(SeekFrom::Start(locator.offset)).unwrap();
  file.write_all(&byte).unwrap();
  file.sync_all().unwrap();
  drop(file);

  let reopened = reopen(&path);
  let error = reopened.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap_err();
  assert!(
    matches!(error.code(), "entity_magic_or_version" | "whole_entity_header_crc" | "whole_entity_integrity_hash"),
    "unexpected corruption code: {}",
    error.code()
  );
}
