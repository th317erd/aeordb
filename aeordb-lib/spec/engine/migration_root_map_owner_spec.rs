use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::hot_tail::read_hot_tail_checked;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::entity::{EntryTypeV4, WHOLE_ENTITY_V1_FLAG_SYSTEM};
use aeordb::engine::directory_entry::{ChildEntry, serialize_child_entries};
use aeordb::engine::v4::first_authority::{
  FirstAuthorityPublicationRequestV1, ImmutableEntityBatchPublicationRequestV1, ImmutableEntityWriteV1,
  ImmutableSystemControlBatchPublicationRequestV1, ImmutableSystemControlWriteV1, MutableSystemControlExpectationV1,
  MutableSystemControlPublicationRequestV1, PreparedNamespaceTreeV0, V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::migration_base_clone_execution::{
  MigrationBaseCloneSeedKindV1, MigrationBaseCloneSeedResultSinkV1, MigrationBaseCloneSeedV1,
};
use aeordb::engine::v4::migration_capture_replay::{MigrationCaptureReplayAuthorityTemplateV1, MigrationCaptureReplayRootSinkV1};
use aeordb::engine::v4::migration_final_authority_reconciliation::{
  MigrationFinalAuthoritySeedCountsV1, MigrationFinalRootMappingClosureV1, MigrationFinalRootMappingSinkV1, MigrationFinalRootMappingV1,
};
use aeordb::engine::v4::migration_preflight::AuthorityInventoryCountsV1;
use aeordb::engine::v4::migration_root_map::{
  LegacyRootMapControlBodyV1, LegacyRootMapPageBodyV1, LegacyRootMapRowV1, LegacyRootSemanticAvailabilityV1,
  encode_legacy_root_map_control, encode_legacy_root_map_page, legacy_root_map_page_identity_hash,
};
use aeordb::engine::v4::migration_root_map_owner::{
  LegacyRootMapOwnerV1, LegacyRootMapProducerSinkV1, LegacyRootMapPublicationRequestV1, LegacyRootMapStagingWorkspaceV1,
  LegacyRootMapWorkspaceIdentityV1, LegacyRootMapWorkspaceOptionsV1, LegacyRootMapWorkspaceReopenOptionsV1, VerifiedLegacyRootMapReaderV1,
};
use aeordb::engine::v4::namespace::{
  EncodedNamespaceRootV1, NamespaceRootWriteV1, SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1,
  decode_namespace_root, decode_semantic_object, encode_namespace_root, encode_semantic_state_object,
};
use aeordb::engine::v4::root_authority::{RootAuthorityKindV1, decode_root_admission_commit, decode_root_publication_prepare};
use aeordb::engine::v4::system_control::SystemControlKindV1;
use aeordb::engine::{DiskKVStore, EntryType, HashAlgorithm};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

const DATABASE_ID: [u8; 16] = [0x31; 16];
const MIGRATION_ID: [u8; 16] = [0x71; 16];
const SOURCE_PHYSICAL_ID: [u8; 16] = [0x41; 16];
const DESTINATION_PHYSICAL_ID: [u8; 16] = [0x51; 16];

fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(128 << 20, 192 << 20, 1, 32 << 20).unwrap())
}

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
  create_publisher_with_availability(
    algorithm,
    SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured },
  )
}

fn create_publisher_with_availability(
  algorithm: HashAlgorithm,
  availability: SemanticAvailabilityV1,
) -> (tempfile::TempDir, PathBuf, V4FirstAuthorityPublisher) {
  let directory = tempdir().unwrap();
  let path = directory.path().join("root-map-owner.aeordb");
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
  publisher.publish(&first_authority_request_for(algorithm, availability)).unwrap();
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

fn first_authority_request_for(algorithm: HashAlgorithm, availability: SemanticAvailabilityV1) -> FirstAuthorityPublicationRequestV1 {
  FirstAuthorityPublicationRequestV1 {
    database_id: DATABASE_ID,
    transaction_id: [0x61; 16],
    created_at_ms: 1_700_000_000_100,
    namespace_tree: PreparedNamespaceTreeV0 { root_hash: digest_parts(algorithm, &[b"dirc:"]), stored_value: Vec::new() },
    semantic_state: encode_semantic_state_object(&SemanticStateWriteV1 { required_capabilities: [0; 32], availability }, algorithm)
      .unwrap(),
    required_capabilities: [0; 32],
    typed_closure_digest: digest_parts(algorithm, &[b"typed root-map-owner closure"]),
    authority_identity: b"HEAD".to_vec(),
  }
}

fn authority_for(algorithm: HashAlgorithm, availability: SemanticAvailabilityV1) -> MigrationCaptureReplayAuthorityTemplateV1 {
  MigrationCaptureReplayAuthorityTemplateV1 {
    base_predecessor_head: vec![0x17; algorithm.hash_length()],
    semantic_state: encode_semantic_state_object(&SemanticStateWriteV1 { required_capabilities: [0; 32], availability }, algorithm)
      .unwrap(),
    required_capabilities: [0; 32],
    typed_closure_context: vec![0x18; algorithm.hash_length()],
    authority_identity: b"HEAD".to_vec(),
    publication_timestamp_floor_ms: 1_700_000_000_100,
    monotonic_timestamp_floor_ms: 1_700_000_000_100,
  }
}

fn namespace_for(
  algorithm: HashAlgorithm,
  authority: &MigrationCaptureReplayAuthorityTemplateV1,
  tree_root: &[u8],
) -> EncodedNamespaceRootV1 {
  encode_namespace_root(
    &NamespaceRootWriteV1 {
      required_capabilities: authority.required_capabilities,
      namespace_tree_root: tree_root.to_vec(),
      semantic_state_root: authority.semantic_state.object_id.clone(),
    },
    algorithm,
  )
  .unwrap()
}

fn publish_tree(publisher: &V4FirstAuthorityPublisher, algorithm: HashAlgorithm, marker: u8) -> Vec<u8> {
  let value = serialize_child_entries(
    &[ChildEntry {
      entry_type: EntryTypeV4::FileRecord.to_u8(),
      hash: digest_parts(algorithm, &[b"filec:", &[marker]]),
      total_size: 1,
      created_at: 1_700_000_000_200,
      updated_at: 1_700_000_000_200,
      name: format!("child-{marker:02x}"),
      content_type: Some("application/octet-stream".to_string()),
      virtual_time: u64::from(marker),
      node_id: u64::from(marker),
    }],
    algorithm.hash_length(),
  )
  .unwrap();
  let key = digest_parts(algorithm, &[b"dirc:", &value]);
  publisher
    .publish_immutable_entity_batch(ImmutableEntityBatchPublicationRequestV1 {
      database_id: &DATABASE_ID,
      entities: &[ImmutableEntityWriteV1 {
        entity_version: 0,
        entry_type: EntryTypeV4::DirectoryIndex,
        flags: 0,
        key: &key,
        stored_value: &value,
      }],
      publication_timestamp_ms: 1_700_000_000_200 + u64::from(marker),
    })
    .unwrap();
  key
}

fn identity(algorithm: HashAlgorithm) -> LegacyRootMapWorkspaceIdentityV1 {
  LegacyRootMapWorkspaceIdentityV1::new(DATABASE_ID, MIGRATION_ID, DATABASE_ID, SOURCE_PHYSICAL_ID, DESTINATION_PHYSICAL_ID, 1, algorithm)
    .unwrap()
}

fn options(root: &Path) -> LegacyRootMapWorkspaceOptionsV1 {
  LegacyRootMapWorkspaceOptionsV1::new(Some(root.to_path_buf()), 64 << 20, 1_000, 0, 2 << 10, 3, 2, 2 << 20).unwrap()
}

fn reopen_options() -> LegacyRootMapWorkspaceReopenOptionsV1 {
  LegacyRootMapWorkspaceReopenOptionsV1::new(64 << 20, 1_000, 0, 2 << 10, 3, 2, 2 << 20).unwrap()
}

fn staged_mapping(
  algorithm: HashAlgorithm,
  authority: &MigrationCaptureReplayAuthorityTemplateV1,
  key: u8,
  destination_tree_root: &[u8],
  sequence: u64,
) -> (LegacyRootMapRowV1, EncodedNamespaceRootV1) {
  let namespace = namespace_for(algorithm, authority, destination_tree_root);
  let semantic = decode_semantic_object(&authority.semantic_state.value, algorithm).unwrap().semantic_state.unwrap();
  let semantic_availability = match semantic.availability {
    SemanticAvailabilityV1::Complete { .. } => LegacyRootSemanticAvailabilityV1::Complete,
    SemanticAvailabilityV1::ContentOnly { reason } => LegacyRootSemanticAvailabilityV1::ContentOnly { reason },
  };
  let row = LegacyRootMapRowV1 {
    legacy_root_hash: vec![key; algorithm.hash_length()],
    namespace_root_v1_hash: namespace.root_hash.clone(),
    semantic_availability,
    captured_source_write_sequence: sequence,
  };
  (row, namespace)
}

fn retirement_owner(algorithm: HashAlgorithm, cancellation: &CancellationToken, memory: &MemoryCoordinator) -> RetirementJournalOwnerV1 {
  RetirementJournalOwnerV1::new_chain(
    algorithm,
    DATABASE_ID,
    1,
    901,
    RetirementJournalBufferOptionsV1::new(1, 1 << 20, 30_000),
    cancellation,
    memory,
  )
  .unwrap()
}

fn private_scratch(parent: &Path) -> PathBuf {
  let scratch = parent.join("scratch");
  std::fs::create_dir(&scratch).unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o700)).unwrap();
  }
  scratch
}

fn selected_root_map_is_absent(publisher: &V4FirstAuthorityPublisher) -> bool {
  publisher.load_mutable_system_control(SystemControlKindV1::LegacyRootMapControl, &DATABASE_ID, &MIGRATION_ID).unwrap().is_none()
}

#[test]
fn bounded_root_map_owner_sorts_deduplicates_publishes_reopens_and_looks_up() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (directory, database_path, publisher) = create_publisher_for(algorithm);
    let cancellation = CancellationToken::new();
    let memory = memory();
    let scratch = directory.path().join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut workspace = LegacyRootMapStagingWorkspaceV1::create(
      &database_path,
      identity(algorithm),
      1_700_000_000_300,
      options(&scratch),
      cancellation.clone(),
      &memory,
    )
    .unwrap();
    let authority =
      authority_for(algorithm, SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured });
    let tree_60 = publish_tree(&publisher, algorithm, 0x60);
    let tree_30 = publish_tree(&publisher, algorithm, 0x30);
    let tree_50 = publish_tree(&publisher, algorithm, 0x50);
    let tree_20 = publish_tree(&publisher, algorithm, 0x20);
    for (row, namespace) in [
      staged_mapping(algorithm, &authority, 0x50, &tree_60, 5),
      staged_mapping(algorithm, &authority, 0x20, &tree_30, 2),
      staged_mapping(algorithm, &authority, 0x40, &tree_50, 4),
    ] {
      workspace.stage_mapping(&row, &namespace).unwrap();
    }
    let (duplicate, duplicate_namespace) = staged_mapping(algorithm, &authority, 0x20, &tree_30, 9);
    workspace.stage_mapping(&duplicate, &duplicate_namespace).unwrap();
    let (first, first_namespace) = staged_mapping(algorithm, &authority, 0x10, &tree_20, 1);
    workspace.stage_mapping(&first, &first_namespace).unwrap();
    let destination_head = publisher.observe().unwrap().selected.header.head_hash.clone();
    workspace.seal(5, 0, [0xa1; 32], [0xb1; 32], &destination_head).unwrap();
    let workspace_path = workspace.workspace_path().to_path_buf();
    drop(workspace);

    let reopened =
      LegacyRootMapStagingWorkspaceV1::reopen(&workspace_path, identity(algorithm), reopen_options(), cancellation.clone(), &memory)
        .unwrap();
    let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
    let receipt = LegacyRootMapOwnerV1::new(&publisher)
      .publish(
        LegacyRootMapPublicationRequestV1 {
          workspace: reopened,
          retirement_owner: &mut retirement,
          cancellation: &cancellation,
          monotonic_now_ms: 1_700_000_000_400,
        },
        &memory,
      )
      .unwrap();
    assert_eq!(receipt.record_count, 4);
    assert_eq!(receipt.page_count, 2);
    assert!(receipt.maximum_run_rows <= 8);
    assert!(receipt.maximum_open_runs <= 3);

    let reopened_publisher = reopen(&database_path);
    let reader = VerifiedLegacyRootMapReaderV1::open(&reopened_publisher, DATABASE_ID, MIGRATION_ID, &cancellation, &memory).unwrap();
    assert_eq!(reader.record_count(), 4);
    assert_eq!(reader.lookup(&vec![0x20; algorithm.hash_length()], &cancellation).unwrap().unwrap().captured_source_write_sequence, 9);
    assert!(reader.lookup(&vec![0x30; algorithm.hash_length()], &cancellation).unwrap().is_none());

    let retry_workspace =
      LegacyRootMapStagingWorkspaceV1::reopen(&workspace_path, identity(algorithm), reopen_options(), cancellation.clone(), &memory)
        .unwrap();
    let retry = LegacyRootMapOwnerV1::new(&reopened_publisher)
      .publish(
        LegacyRootMapPublicationRequestV1 {
          workspace: retry_workspace,
          retirement_owner: &mut retirement,
          cancellation: &cancellation,
          monotonic_now_ms: 1_700_000_000_500,
        },
        &memory,
      )
      .unwrap();
    assert!(retry.idempotent);

    let selected = reopened_publisher
      .load_mutable_system_control(SystemControlKindV1::LegacyRootMapControl, &DATABASE_ID, &MIGRATION_ID)
      .unwrap()
      .unwrap();
    let decoded = aeordb::engine::v4::migration_root_map::decode_legacy_root_map_control(&selected.bytes, algorithm).unwrap();
    let advanced = encode_legacy_root_map_control(decoded.sequence + 1, &decoded.body, algorithm).unwrap();
    reopened_publisher
      .publish_mutable_system_control(
        MutableSystemControlPublicationRequestV1 {
          database_id: &DATABASE_ID,
          kind: SystemControlKindV1::LegacyRootMapControl,
          identity: &MIGRATION_ID,
          expected: Some(MutableSystemControlExpectationV1 {
            selected_slot: selected.selected_slot,
            control_sequence: selected.control_sequence,
            control_digest: selected.control_digest,
          }),
          guards: &[],
          encoded_control: &advanced,
          publication_timestamp_ms: 1_700_000_000_600,
          monotonic_now_ms: 1_700_000_000_600,
        },
        &mut retirement,
      )
      .unwrap();
    let error = reader.lookup(&vec![0x20; algorithm.hash_length()], &cancellation).unwrap_err();
    assert_eq!(error.code(), "migration_root_map_selected_changed");
  }
}

#[test]
fn conflicting_selected_map_is_rejected_before_destination_authority_changes() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (directory, database_path, publisher) = create_publisher_for(algorithm);
  let cancellation = CancellationToken::new();
  let memory = memory();
  let authority =
    authority_for(algorithm, SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured });
  let destination_head = publisher.observe().unwrap().selected.header.head_hash.clone();
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);

  let first_scratch = directory.path().join("first-scratch");
  std::fs::create_dir(&first_scratch).unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&first_scratch, std::fs::Permissions::from_mode(0o700)).unwrap();
  }
  let first_tree = publish_tree(&publisher, algorithm, 0x31);
  let (first_row, first_namespace) = staged_mapping(algorithm, &authority, 0x21, &first_tree, 1);
  let mut first_workspace = LegacyRootMapStagingWorkspaceV1::create(
    &database_path,
    identity(algorithm),
    1_700_000_000_300,
    options(&first_scratch),
    cancellation.clone(),
    &memory,
  )
  .unwrap();
  first_workspace.stage_mapping(&first_row, &first_namespace).unwrap();
  first_workspace.seal(1, 0, [0xa1; 32], [0xb1; 32], &destination_head).unwrap();
  LegacyRootMapOwnerV1::new(&publisher)
    .publish(
      LegacyRootMapPublicationRequestV1 {
        workspace: first_workspace,
        retirement_owner: &mut retirement,
        cancellation: &cancellation,
        monotonic_now_ms: 1_700_000_000_400,
      },
      &memory,
    )
    .unwrap();

  let second_scratch = directory.path().join("second-scratch");
  std::fs::create_dir(&second_scratch).unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&second_scratch, std::fs::Permissions::from_mode(0o700)).unwrap();
  }
  let second_tree = publish_tree(&publisher, algorithm, 0x32);
  let (second_row, second_namespace) = staged_mapping(algorithm, &authority, 0x22, &second_tree, 2);
  assert!(publisher.load_immutable_entity_bounded(&second_namespace.root_hash, 1 << 20).unwrap().is_none());
  let mut second_workspace = LegacyRootMapStagingWorkspaceV1::create(
    &database_path,
    identity(algorithm),
    1_700_000_000_500,
    options(&second_scratch),
    cancellation.clone(),
    &memory,
  )
  .unwrap();
  second_workspace.stage_mapping(&second_row, &second_namespace).unwrap();
  second_workspace.seal(1, 0, [0xa2; 32], [0xb2; 32], &destination_head).unwrap();
  let before = publisher.observe().unwrap().selected.header;

  let error = LegacyRootMapOwnerV1::new(&publisher)
    .publish(
      LegacyRootMapPublicationRequestV1 {
        workspace: second_workspace,
        retirement_owner: &mut retirement,
        cancellation: &cancellation,
        monotonic_now_ms: 1_700_000_000_600,
      },
      &memory,
    )
    .unwrap_err();
  assert_eq!(error.code(), "migration_root_map_selected_collision");
  assert_eq!(publisher.observe().unwrap().selected.header, before);
  assert!(publisher.load_immutable_entity_bounded(&second_namespace.root_hash, 1 << 20).unwrap().is_none());
  assert!(publisher
    .load_immutable_system_control(SystemControlKindV1::RootAdmissionCommit, &DATABASE_ID, &second_namespace.root_hash)
    .unwrap()
    .is_none());
}

#[test]
fn root_map_sort_refuses_conflicting_duplicate_source_roots() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (directory, database_path, publisher) = create_publisher_for(algorithm);
  let cancellation = CancellationToken::new();
  let memory = memory();
  let scratch = directory.path().join("scratch");
  std::fs::create_dir(&scratch).unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o700)).unwrap();
  }
  let mut workspace = LegacyRootMapStagingWorkspaceV1::create(
    &database_path,
    identity(algorithm),
    1_700_000_000_300,
    options(&scratch),
    cancellation.clone(),
    &memory,
  )
  .unwrap();
  let authority =
    authority_for(algorithm, SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured });
  let first_tree = publish_tree(&publisher, algorithm, 0x30);
  let second_tree = publish_tree(&publisher, algorithm, 0x31);
  let (first, first_namespace) = staged_mapping(algorithm, &authority, 0x20, &first_tree, 1);
  let (second, second_namespace) = staged_mapping(algorithm, &authority, 0x20, &second_tree, 2);
  workspace.stage_mapping(&first, &first_namespace).unwrap();
  workspace.stage_mapping(&second, &second_namespace).unwrap();
  let destination_head = publisher.observe().unwrap().selected.header.head_hash.clone();
  workspace.seal(2, 0, [0xa1; 32], [0xb1; 32], &destination_head).unwrap();
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let error = LegacyRootMapOwnerV1::new(&publisher)
    .publish(
      LegacyRootMapPublicationRequestV1 {
        workspace,
        retirement_owner: &mut retirement,
        cancellation: &cancellation,
        monotonic_now_ms: 1_700_000_000_400,
      },
      &memory,
    )
    .unwrap_err();
  assert_eq!(error.code(), "migration_root_map_conflicting_mapping");
  assert!(
    publisher
      .load_mutable_system_control(
        aeordb::engine::v4::system_control::SystemControlKindV1::LegacyRootMapControl,
        &DATABASE_ID,
        &MIGRATION_ID,
      )
      .unwrap()
      .is_none()
  );
}

#[test]
fn one_root_map_sink_adapts_base_replay_and_final_streams_and_omits_detached_paths() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (directory, database_path, publisher) = create_publisher_for(algorithm);
  let cancellation = CancellationToken::new();
  let memory = memory();
  let scratch = directory.path().join("scratch");
  std::fs::create_dir(&scratch).unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o700)).unwrap();
  }
  let authority =
    authority_for(algorithm, SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured });
  let base_tree = publish_tree(&publisher, algorithm, 0x21);
  let replay_tree = publish_tree(&publisher, algorithm, 0x22);
  let final_tree = publish_tree(&publisher, algorithm, 0x23);
  let mut workspace = LegacyRootMapStagingWorkspaceV1::create(
    &database_path,
    identity(algorithm),
    1_700_000_000_300,
    options(&scratch),
    cancellation.clone(),
    &memory,
  )
  .unwrap();
  let destination_head = publisher.observe().unwrap().selected.header.head_hash.clone();
  {
    let mut sink = LegacyRootMapProducerSinkV1::new(&mut workspace, &authority, 7).unwrap();
    sink
      .record_seed_result(
        &MigrationBaseCloneSeedV1 {
          kind: MigrationBaseCloneSeedKindV1::Snapshot,
          path: "/".to_string(),
          entry_type: EntryType::DirectoryIndex,
          hash: vec![0x11; algorithm.hash_length()],
        },
        Some(&base_tree),
      )
      .unwrap();
    sink
      .record_seed_result(
        &MigrationBaseCloneSeedV1 {
          kind: MigrationBaseCloneSeedKindV1::DetachedProtectedPath,
          path: "/.aeordb-system/local".to_string(),
          entry_type: EntryType::FileRecord,
          hash: vec![0x99; algorithm.hash_length()],
        },
        Some(&vec![0xa9; algorithm.hash_length()]),
      )
      .unwrap();
    let replay_namespace = namespace_for(algorithm, &authority, &replay_tree);
    MigrationCaptureReplayRootSinkV1::record_root_mapping(
      &mut sink,
      8,
      &vec![0x12; algorithm.hash_length()],
      &replay_namespace.root_hash,
      &replay_tree,
    )
    .unwrap();
    let final_namespace = namespace_for(algorithm, &authority, &final_tree);
    MigrationFinalRootMappingSinkV1::record_root_mapping(
      &mut sink,
      &MigrationFinalRootMappingV1 {
        kind: MigrationBaseCloneSeedKindV1::Maintenance,
        authority_identity: b"maintenance".to_vec(),
        source_write_sequence: 9,
        source_path: "/".to_string(),
        source_entry_type: EntryType::DirectoryIndex,
        source_root: vec![0x13; algorithm.hash_length()],
        system_family_id: None,
        destination_entity: Some(vec![0x23; algorithm.hash_length()]),
        destination_namespace_root: Some(final_namespace.root_hash),
        destination_tree_root: Some(final_tree),
        reused: false,
      },
    )
    .unwrap();
    MigrationFinalRootMappingSinkV1::record_root_mapping(
      &mut sink,
      &MigrationFinalRootMappingV1 {
        kind: MigrationBaseCloneSeedKindV1::DetachedProtectedPath,
        authority_identity: b"destination-local".to_vec(),
        source_write_sequence: 10,
        source_path: "/.aeordb-system/local".to_string(),
        source_entry_type: EntryType::FileRecord,
        source_root: vec![0x98; algorithm.hash_length()],
        system_family_id: Some(1),
        destination_entity: None,
        destination_namespace_root: None,
        destination_tree_root: None,
        reused: false,
      },
    )
    .unwrap();
    MigrationFinalRootMappingSinkV1::finish_root_mappings(
      &mut sink,
      &MigrationFinalRootMappingClosureV1 {
        database_id: DATABASE_ID,
        migration_id: MIGRATION_ID,
        source_physical_instance_id: SOURCE_PHYSICAL_ID,
        destination_physical_instance_id: DESTINATION_PHYSICAL_ID,
        source_header_sequence: 7,
        frozen_source_root: vec![0x17; algorithm.hash_length()],
        frozen_source_publication_sequence: 10,
        destination_header_sequence: publisher.observe().unwrap().selected.header.slot_sequence,
        destination_namespace_root: destination_head.clone(),
        destination_tree_root: digest_parts(algorithm, &[b"dirc:"]),
        source_authority_counts: AuthorityInventoryCountsV1::default(),
        seed_counts: MigrationFinalAuthoritySeedCountsV1 { maintenance: 1, detached_protected: 1, ..Default::default() },
        mapping_count: 2,
        omitted_mapping_count: 1,
        authority_digest: [0xa2; 32],
        mapping_digest: [0xb2; 32],
        system_family_registry_fingerprint: vec![0x41; algorithm.hash_length()],
      },
    )
    .unwrap();
  }

  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  LegacyRootMapOwnerV1::new(&publisher)
    .publish(
      LegacyRootMapPublicationRequestV1 {
        workspace,
        retirement_owner: &mut retirement,
        cancellation: &cancellation,
        monotonic_now_ms: 1_700_000_000_400,
      },
      &memory,
    )
    .unwrap();
  assert_eq!(publisher.observe().unwrap().selected.header.head_hash, destination_head);
  let reader = VerifiedLegacyRootMapReaderV1::open(&publisher, DATABASE_ID, MIGRATION_ID, &cancellation, &memory).unwrap();
  assert_eq!(reader.record_count(), 3);
  for (source, sequence) in [(0x11, 7), (0x12, 8), (0x13, 9)] {
    let row = reader.lookup(&vec![source; algorithm.hash_length()], &cancellation).unwrap().unwrap();
    assert_eq!(row.captured_source_write_sequence, sequence);
    assert_eq!(
      row.semantic_availability,
      LegacyRootSemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured }
    );
    let target = publisher.load_immutable_entity_bounded(&row.namespace_root_v1_hash, 1 << 20).unwrap().unwrap();
    assert_eq!(target.entity_version, 1);
    assert_eq!(target.entry_type, EntryTypeV4::DirectoryIndex);
    assert_eq!(target.flags, WHOLE_ENTITY_V1_FLAG_SYSTEM);
    let admission = publisher
      .load_immutable_system_control(SystemControlKindV1::RootAdmissionCommit, &DATABASE_ID, &row.namespace_root_v1_hash)
      .unwrap()
      .unwrap();
    let decoded_admission = decode_root_admission_commit(&admission.bytes, algorithm).unwrap();
    assert_eq!(decoded_admission.namespace_root, row.namespace_root_v1_hash);
    assert_eq!(decoded_admission.authority_kind, RootAuthorityKindV1::MigrationMap);
    let prepare = publisher
      .load_immutable_system_control(SystemControlKindV1::RootPublicationPrepare, &DATABASE_ID, &decoded_admission.transaction_id)
      .unwrap()
      .unwrap();
    let decoded_prepare = decode_root_publication_prepare(&prepare.bytes, algorithm).unwrap();
    assert_eq!(decoded_prepare.target_namespace_root, row.namespace_root_v1_hash);
    assert_eq!(decoded_prepare.authority_kind, RootAuthorityKindV1::MigrationMap);
    assert_eq!(decoded_admission.prepare_payload_hash, digest_parts(algorithm, &[&prepare.bytes]));
    let decoded_namespace = decode_namespace_root(&target.stored_value, algorithm).unwrap();
    assert_eq!(
      decoded_prepare.typed_closure_digest,
      digest_parts(
        algorithm,
        &[
          b"aeordb.migration-map.root-admission.closure.v1\0",
          &DATABASE_ID,
          &MIGRATION_ID,
          &1u64.to_le_bytes(),
          &vec![source; algorithm.hash_length()],
          &sequence.to_le_bytes(),
          &row.namespace_root_v1_hash,
          &decoded_namespace.namespace_tree_root,
          &decoded_namespace.semantic_state_root,
        ],
      )
    );
  }
  assert!(reader.lookup(&vec![0x98; algorithm.hash_length()], &cancellation).unwrap().is_none());
  assert!(reader.lookup(&vec![0x99; algorithm.hash_length()], &cancellation).unwrap().is_none());
}

#[test]
fn root_map_sink_refuses_malformed_semantic_authority_before_staging() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (directory, database_path, _publisher) = create_publisher_for(algorithm);
  let cancellation = CancellationToken::new();
  let memory = memory();
  let scratch = directory.path().join("scratch");
  std::fs::create_dir(&scratch).unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o700)).unwrap();
  }
  let mut authority =
    authority_for(algorithm, SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured });
  authority.semantic_state.value[0] ^= 0xff;
  let mut workspace = LegacyRootMapStagingWorkspaceV1::create(
    &database_path,
    identity(algorithm),
    1_700_000_000_300,
    options(&scratch),
    cancellation,
    &memory,
  )
  .unwrap();
  let error = LegacyRootMapProducerSinkV1::new(&mut workspace, &authority, 1).unwrap_err();
  assert_eq!(error.code(), "crc_mismatch");
}

#[test]
fn stage_reopen_truncates_only_a_torn_suffix_and_rejects_interior_corruption() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (directory, database_path, publisher) = create_publisher_for(algorithm);
  let cancellation = CancellationToken::new();
  let memory = memory();
  let scratch = private_scratch(directory.path());
  let authority =
    authority_for(algorithm, SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured });
  let tree = publish_tree(&publisher, algorithm, 0x31);
  let mut workspace = LegacyRootMapStagingWorkspaceV1::create(
    &database_path,
    identity(algorithm),
    1_700_000_000_300,
    options(&scratch),
    cancellation.clone(),
    &memory,
  )
  .unwrap();
  let (row, namespace) = staged_mapping(algorithm, &authority, 0x21, &tree, 1);
  workspace.stage_mapping(&row, &namespace).unwrap();
  let destination_head = publisher.observe().unwrap().selected.header.head_hash.clone();
  workspace.seal(1, 0, [0xa1; 32], [0xb1; 32], &destination_head).unwrap();
  let workspace_path = workspace.workspace_path().to_path_buf();
  let stage_path = workspace_path.join("rows.stage");
  let complete_length = std::fs::metadata(&stage_path).unwrap().len();
  let complete_bytes = std::fs::read(&stage_path).unwrap();
  drop(workspace);

  OpenOptions::new().append(true).open(&stage_path).unwrap().write_all(&[0xaa, 0xbb, 0xcc]).unwrap();
  let reopened =
    LegacyRootMapStagingWorkspaceV1::reopen(&workspace_path, identity(algorithm), reopen_options(), cancellation.clone(), &memory).unwrap();
  assert_eq!(std::fs::metadata(&stage_path).unwrap().len(), complete_length);
  drop(reopened);

  OpenOptions::new().append(true).open(&stage_path).unwrap().write_all(&complete_bytes).unwrap();
  let error =
    LegacyRootMapStagingWorkspaceV1::reopen(&workspace_path, identity(algorithm), reopen_options(), cancellation.clone(), &memory)
      .unwrap_err();
  assert_eq!(error.code(), "migration_root_map_stage_seal");

  std::fs::write(&stage_path, &complete_bytes).unwrap();
  OpenOptions::new().write(true).open(&stage_path).unwrap().set_len(0).unwrap();
  let error =
    LegacyRootMapStagingWorkspaceV1::reopen(&workspace_path, identity(algorithm), reopen_options(), cancellation.clone(), &memory)
      .unwrap_err();
  assert_eq!(error.code(), "migration_root_map_stage_seal");

  std::fs::write(&stage_path, &complete_bytes).unwrap();
  let mut digest_mismatch = complete_bytes.clone();
  digest_mismatch[STAGE_CORRUPTION_OFFSET] ^= 0x40;
  let crc_offset = digest_mismatch.len() - 4;
  let repaired_crc = crc32fast::hash(&digest_mismatch[..crc_offset]);
  digest_mismatch[crc_offset..].copy_from_slice(&repaired_crc.to_le_bytes());
  std::fs::write(&stage_path, digest_mismatch).unwrap();
  let error =
    LegacyRootMapStagingWorkspaceV1::reopen(&workspace_path, identity(algorithm), reopen_options(), cancellation.clone(), &memory)
      .unwrap_err();
  assert_eq!(error.code(), "migration_root_map_stage_seal");

  std::fs::write(&stage_path, &complete_bytes).unwrap();
  let mut bytes = std::fs::read(&stage_path).unwrap();
  bytes[STAGE_CORRUPTION_OFFSET] ^= 0x80;
  std::fs::write(&stage_path, bytes).unwrap();
  let error =
    LegacyRootMapStagingWorkspaceV1::reopen(&workspace_path, identity(algorithm), reopen_options(), cancellation, &memory).unwrap_err();
  assert_eq!(error.code(), "migration_root_map_stage_frame");
}

const STAGE_CORRUPTION_OFFSET: usize = 24;

#[test]
fn publication_revalidates_the_sealed_stage_before_derived_or_authority_writes() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (directory, database_path, publisher) = create_publisher_for(algorithm);
  let cancellation = CancellationToken::new();
  let memory = memory();
  let scratch = private_scratch(directory.path());
  let authority =
    authority_for(algorithm, SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured });
  let tree = publish_tree(&publisher, algorithm, 0x31);
  let mut workspace = LegacyRootMapStagingWorkspaceV1::create(
    &database_path,
    identity(algorithm),
    1_700_000_000_300,
    options(&scratch),
    cancellation.clone(),
    &memory,
  )
  .unwrap();
  let (row, namespace) = staged_mapping(algorithm, &authority, 0x21, &tree, 1);
  workspace.stage_mapping(&row, &namespace).unwrap();
  let destination_head = publisher.observe().unwrap().selected.header.head_hash.clone();
  workspace.seal(1, 0, [0xa1; 32], [0xb1; 32], &destination_head).unwrap();
  let stage_path = workspace.workspace_path().join("rows.stage");
  let mut bytes = std::fs::read(&stage_path).unwrap();
  bytes[STAGE_CORRUPTION_OFFSET] ^= 0x40;
  let crc_offset = bytes.len() - 4;
  let repaired_crc = crc32fast::hash(&bytes[..crc_offset]);
  bytes[crc_offset..].copy_from_slice(&repaired_crc.to_le_bytes());
  std::fs::write(&stage_path, bytes).unwrap();
  let before = publisher.observe().unwrap().selected.header;
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);

  let error = LegacyRootMapOwnerV1::new(&publisher)
    .publish(
      LegacyRootMapPublicationRequestV1 {
        workspace,
        retirement_owner: &mut retirement,
        cancellation: &cancellation,
        monotonic_now_ms: 1_700_000_000_400,
      },
      &memory,
    )
    .unwrap_err();
  assert_eq!(error.code(), "migration_root_map_stage_seal");
  assert_eq!(publisher.observe().unwrap().selected.header, before);
  assert!(publisher.load_immutable_entity_bounded(&namespace.root_hash, 1 << 20).unwrap().is_none());
  assert!(selected_root_map_is_absent(&publisher));
}

#[test]
fn reopen_removes_only_owned_pending_files_and_refuses_unknown_entries() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (directory, database_path, _publisher) = create_publisher_for(algorithm);
  let cancellation = CancellationToken::new();
  let memory = memory();
  let scratch = private_scratch(directory.path());
  let workspace = LegacyRootMapStagingWorkspaceV1::create(
    &database_path,
    identity(algorithm),
    1_700_000_000_300,
    options(&scratch),
    cancellation.clone(),
    &memory,
  )
  .unwrap();
  let workspace_path = workspace.workspace_path().to_path_buf();
  drop(workspace);

  let pending_names = [
    workspace_path.join(".root-map-00000000000000000000000000000001.pending"),
    workspace_path.join("runs/.root-map-00000000000000000000000000000002.pending"),
    workspace_path.join("pages/.root-map-00000000000000000000000000000003.pending"),
  ];
  for (ordinal, path) in pending_names.iter().enumerate() {
    let private_source = LegacyRootMapStagingWorkspaceV1::create(
      &database_path,
      LegacyRootMapWorkspaceIdentityV1::new(
        DATABASE_ID,
        [0x80 + u8::try_from(ordinal).unwrap(); 16],
        DATABASE_ID,
        SOURCE_PHYSICAL_ID,
        DESTINATION_PHYSICAL_ID,
        1,
        algorithm,
      )
      .unwrap(),
      1_700_000_000_400 + u64::try_from(ordinal).unwrap(),
      options(&scratch),
      cancellation.clone(),
      &memory,
    )
    .unwrap();
    let private_stage = private_source.workspace_path().join("rows.stage");
    drop(private_source);
    std::fs::rename(private_stage, path).unwrap();
  }
  let reopened =
    LegacyRootMapStagingWorkspaceV1::reopen(&workspace_path, identity(algorithm), reopen_options(), cancellation.clone(), &memory).unwrap();
  assert!(pending_names.iter().all(|path| !path.exists()));
  drop(reopened);

  std::fs::write(workspace_path.join("unexpected.data"), b"foreign").unwrap();
  let error =
    LegacyRootMapStagingWorkspaceV1::reopen(&workspace_path, identity(algorithm), reopen_options(), cancellation, &memory).unwrap_err();
  assert_eq!(error.code(), "migration_root_map_workspace");
}

#[cfg(unix)]
#[test]
fn reopen_refuses_an_owned_pending_name_that_is_a_symlink() {
  use std::os::unix::fs::symlink;

  let algorithm = HashAlgorithm::Blake3_256;
  let (directory, database_path, _publisher) = create_publisher_for(algorithm);
  let cancellation = CancellationToken::new();
  let memory = memory();
  let scratch = private_scratch(directory.path());
  let workspace = LegacyRootMapStagingWorkspaceV1::create(
    &database_path,
    identity(algorithm),
    1_700_000_000_300,
    options(&scratch),
    cancellation.clone(),
    &memory,
  )
  .unwrap();
  let workspace_path = workspace.workspace_path().to_path_buf();
  drop(workspace);
  symlink(workspace_path.join("rows.stage"), workspace_path.join(".root-map-00000000000000000000000000000004.pending")).unwrap();

  let error =
    LegacyRootMapStagingWorkspaceV1::reopen(&workspace_path, identity(algorithm), reopen_options(), cancellation, &memory).unwrap_err();
  assert_eq!(error.code(), "migration_root_map_workspace");
}

#[test]
fn missing_destination_tree_and_prepublication_cancellation_never_select_a_map() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (directory, database_path, publisher) = create_publisher_for(algorithm);
  let cancellation = CancellationToken::new();
  let memory = memory();
  let scratch = private_scratch(directory.path());
  let authority =
    authority_for(algorithm, SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured });
  let missing_tree = vec![0xe1; algorithm.hash_length()];
  let mut workspace = LegacyRootMapStagingWorkspaceV1::create(
    &database_path,
    identity(algorithm),
    1_700_000_000_300,
    options(&scratch),
    cancellation.clone(),
    &memory,
  )
  .unwrap();
  let (row, namespace) = staged_mapping(algorithm, &authority, 0x21, &missing_tree, 1);
  workspace.stage_mapping(&row, &namespace).unwrap();
  let destination_head = publisher.observe().unwrap().selected.header.head_hash.clone();
  workspace.seal(1, 0, [0xa1; 32], [0xb1; 32], &destination_head).unwrap();
  let before = publisher.observe().unwrap().selected.header;
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let error = LegacyRootMapOwnerV1::new(&publisher)
    .publish(
      LegacyRootMapPublicationRequestV1 {
        workspace,
        retirement_owner: &mut retirement,
        cancellation: &cancellation,
        monotonic_now_ms: 1_700_000_000_400,
      },
      &memory,
    )
    .unwrap_err();
  assert_eq!(error.code(), "migration_map_authority_tree_missing");
  assert_eq!(publisher.observe().unwrap().selected.header, before);
  assert!(selected_root_map_is_absent(&publisher));
  assert!(publisher.load_immutable_entity_bounded(&namespace.root_hash, 1 << 20).unwrap().is_none());

  let canceled = CancellationToken::new();
  canceled.cancel();
  let second_scratch = directory.path().join("second-scratch");
  std::fs::create_dir(&second_scratch).unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&second_scratch, std::fs::Permissions::from_mode(0o700)).unwrap();
  }
  let error = LegacyRootMapStagingWorkspaceV1::create(
    &database_path,
    identity(algorithm),
    1_700_000_000_300,
    options(&second_scratch),
    canceled,
    &memory,
  )
  .unwrap_err();
  assert_eq!(error.code(), "migration_root_map_cancelled");
  assert!(selected_root_map_is_absent(&publisher));
}

#[test]
fn staging_enforces_namespace_row_disk_free_space_and_batch_bounds() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (directory, database_path, publisher) = create_publisher_for(algorithm);
  let cancellation = CancellationToken::new();
  let memory = memory();
  let authority =
    authority_for(algorithm, SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured });
  let scratch = private_scratch(directory.path());

  let too_small = LegacyRootMapWorkspaceOptionsV1::new(Some(scratch.clone()), 1, 1, 0, 2 << 10, 2, 1, 2 << 20).unwrap();
  let error = LegacyRootMapStagingWorkspaceV1::create(
    &database_path,
    identity(algorithm),
    1_700_000_000_300,
    too_small,
    cancellation.clone(),
    &memory,
  )
  .unwrap_err();
  assert_eq!(error.code(), "migration_root_map_capacity");

  let no_free_space = LegacyRootMapWorkspaceOptionsV1::new(Some(scratch.clone()), 64 << 20, 1, u64::MAX, 2 << 10, 2, 1, 2 << 20).unwrap();
  let error = LegacyRootMapStagingWorkspaceV1::create(
    &database_path,
    identity(algorithm),
    1_700_000_000_300,
    no_free_space,
    cancellation.clone(),
    &memory,
  )
  .unwrap_err();
  assert_eq!(error.code(), "migration_root_map_capacity");

  let one_row = LegacyRootMapWorkspaceOptionsV1::new(Some(scratch.clone()), 64 << 20, 1, 0, 2 << 10, 2, 1, 512).unwrap();
  let mut workspace =
    LegacyRootMapStagingWorkspaceV1::create(&database_path, identity(algorithm), 1_700_000_000_300, one_row, cancellation.clone(), &memory)
      .unwrap();
  let tree_a = publish_tree(&publisher, algorithm, 0x31);
  let tree_b = publish_tree(&publisher, algorithm, 0x32);
  let (row_a, namespace_a) = staged_mapping(algorithm, &authority, 0x21, &tree_a, 1);
  let (row_b, namespace_b) = staged_mapping(algorithm, &authority, 0x22, &tree_b, 2);
  let mut mismatched = row_a.clone();
  mismatched.namespace_root_v1_hash = namespace_b.root_hash.clone();
  let error = workspace.stage_mapping(&mismatched, &namespace_a).unwrap_err();
  assert_eq!(error.code(), "migration_root_map_namespace_identity");
  workspace.stage_mapping(&row_a, &namespace_a).unwrap();
  let error = workspace.stage_mapping(&row_b, &namespace_b).unwrap_err();
  assert_eq!(error.code(), "migration_root_map_capacity");

  let destination_head = publisher.observe().unwrap().selected.header.head_hash.clone();
  workspace.seal(1, 0, [0xa1; 32], [0xb1; 32], &destination_head).unwrap();
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let error = LegacyRootMapOwnerV1::new(&publisher)
    .publish(
      LegacyRootMapPublicationRequestV1 {
        workspace,
        retirement_owner: &mut retirement,
        cancellation: &cancellation,
        monotonic_now_ms: 1_700_000_000_400,
      },
      &memory,
    )
    .unwrap_err();
  assert_eq!(error.code(), "migration_map_authority_entity_bytes");
  assert!(selected_root_map_is_absent(&publisher));
}

#[test]
fn empty_map_and_multi_pass_complete_semantic_map_publish_canonically() {
  let algorithm = HashAlgorithm::Blake3_256;
  let complete = SemanticAvailabilityV1::Complete {
    compiler_fingerprint: vec![0x11; algorithm.hash_length()],
    semantic_registry_fingerprint: vec![0x12; algorithm.hash_length()],
    catalog_root: vec![0x13; algorithm.hash_length()],
    catalog_record_count: 1,
    catalog_node_count: 1,
    definition_count: 0,
    dependency_count: 0,
  };
  let (directory, database_path, publisher) = create_publisher_with_availability(algorithm, complete.clone());
  let cancellation = CancellationToken::new();
  let memory = memory();
  let scratch = private_scratch(directory.path());
  let authority = authority_for(algorithm, complete);

  let empty_scratch = directory.path().join("empty-scratch");
  std::fs::create_dir(&empty_scratch).unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&empty_scratch, std::fs::Permissions::from_mode(0o700)).unwrap();
  }
  let mut empty = LegacyRootMapStagingWorkspaceV1::create(
    &database_path,
    LegacyRootMapWorkspaceIdentityV1::new(DATABASE_ID, [0x72; 16], DATABASE_ID, SOURCE_PHYSICAL_ID, DESTINATION_PHYSICAL_ID, 1, algorithm)
      .unwrap(),
    1_700_000_000_300,
    options(&empty_scratch),
    cancellation.clone(),
    &memory,
  )
  .unwrap();
  let destination_head = publisher.observe().unwrap().selected.header.head_hash.clone();
  empty.seal(0, 0, [0xa1; 32], [0xb1; 32], &destination_head).unwrap();
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let empty_receipt = LegacyRootMapOwnerV1::new(&publisher)
    .publish(
      LegacyRootMapPublicationRequestV1 {
        workspace: empty,
        retirement_owner: &mut retirement,
        cancellation: &cancellation,
        monotonic_now_ms: 1_700_000_000_400,
      },
      &memory,
    )
    .unwrap();
  assert_eq!((empty_receipt.page_count, empty_receipt.record_count, empty_receipt.merge_passes), (0, 0, 0));
  assert_eq!(VerifiedLegacyRootMapReaderV1::open(&publisher, DATABASE_ID, [0x72; 16], &cancellation, &memory).unwrap().record_count(), 0);

  let bounded = LegacyRootMapWorkspaceOptionsV1::new(Some(scratch), 64 << 20, 1_000, 0, 1_200, 2, 5, 2 << 20).unwrap();
  let mut workspace =
    LegacyRootMapStagingWorkspaceV1::create(&database_path, identity(algorithm), 1_700_000_000_500, bounded, cancellation.clone(), &memory)
      .unwrap();
  for key in (1..=60).rev() {
    let tree = publish_tree(&publisher, algorithm, key);
    let (row, namespace) = staged_mapping(algorithm, &authority, key, &tree, u64::from(key));
    workspace.stage_mapping(&row, &namespace).unwrap();
  }
  let destination_head = publisher.observe().unwrap().selected.header.head_hash.clone();
  workspace.seal(60, 0, [0xa2; 32], [0xb2; 32], &destination_head).unwrap();
  let receipt = LegacyRootMapOwnerV1::new(&publisher)
    .publish(
      LegacyRootMapPublicationRequestV1 {
        workspace,
        retirement_owner: &mut retirement,
        cancellation: &cancellation,
        monotonic_now_ms: 1_700_000_000_600,
      },
      &memory,
    )
    .unwrap();
  assert_eq!(receipt.record_count, 60);
  assert!(receipt.merge_passes >= 2);
  assert!(receipt.maximum_open_runs <= 2);
  let tiny_memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 10, 64 << 10, 1, 8 << 10).unwrap());
  assert_eq!(
    VerifiedLegacyRootMapReaderV1::open(&publisher, DATABASE_ID, MIGRATION_ID, &cancellation, &tiny_memory).unwrap_err().code(),
    "migration_root_map_memory"
  );
  let reader_memory = MemoryCoordinator::new(MemoryPolicy::new(8 << 20, 16 << 20, 1, 2 << 20).unwrap());
  let reader = VerifiedLegacyRootMapReaderV1::open(&publisher, DATABASE_ID, MIGRATION_ID, &cancellation, &reader_memory).unwrap();
  let open_memory = reader_memory.snapshot().unwrap().owner(MemoryOwner::Migration).unwrap().clone();
  assert_eq!(open_memory.reserved_bytes, 512 << 10);
  assert_eq!(open_memory.peak_reserved_bytes, (512 << 10) + (2 << 20) + (128 << 10));
  assert_eq!(
    reader.lookup(&vec![0x20; algorithm.hash_length()], &cancellation).unwrap().unwrap().semantic_availability,
    LegacyRootSemanticAvailabilityV1::Complete
  );
  drop(reader);
  assert_eq!(reader_memory.snapshot().unwrap().owner(MemoryOwner::Migration).unwrap().reserved_bytes, 0);
}

#[test]
fn selected_reader_memory_is_admitted_before_root_map_selection() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (directory, database_path, publisher) = create_publisher_for(algorithm);
  let cancellation = CancellationToken::new();
  let memory = memory();
  let scratch = private_scratch(directory.path());
  let authority =
    authority_for(algorithm, SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured });
  let tree = publish_tree(&publisher, algorithm, 0x43);
  let (row, namespace) = staged_mapping(algorithm, &authority, 0x43, &tree, 7);
  let bounded = LegacyRootMapWorkspaceOptionsV1::new(Some(scratch), 64 << 20, 100, 0, 1_200, 2, 8, 4 << 10).unwrap();
  let mut workspace =
    LegacyRootMapStagingWorkspaceV1::create(&database_path, identity(algorithm), 1_700_000_000_300, bounded, cancellation.clone(), &memory)
      .unwrap();
  workspace.stage_mapping(&row, &namespace).unwrap();
  let destination_head = publisher.observe().unwrap().selected.header.head_hash.clone();
  workspace.seal(1, 0, [0xa4; 32], [0xb4; 32], &destination_head).unwrap();
  let workspace_path = workspace.workspace_path().to_path_buf();
  let constrained = MemoryCoordinator::new(MemoryPolicy::new(512 << 10, 1 << 20, 1, 128 << 10).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);

  let error = LegacyRootMapOwnerV1::new(&publisher)
    .publish(
      LegacyRootMapPublicationRequestV1 {
        workspace,
        retirement_owner: &mut retirement,
        cancellation: &cancellation,
        monotonic_now_ms: 1_700_000_000_400,
      },
      &constrained,
    )
    .unwrap_err();
  assert_eq!(error.code(), "migration_root_map_memory");
  assert!(error.committed_receipt().is_none());
  assert!(selected_root_map_is_absent(&publisher));

  let reopened = LegacyRootMapStagingWorkspaceV1::reopen(
    &workspace_path,
    identity(algorithm),
    LegacyRootMapWorkspaceReopenOptionsV1::new(64 << 20, 100, 0, 1_200, 2, 8, 4 << 10).unwrap(),
    cancellation.clone(),
    &memory,
  )
  .unwrap();
  let receipt = LegacyRootMapOwnerV1::new(&publisher)
    .publish(
      LegacyRootMapPublicationRequestV1 {
        workspace: reopened,
        retirement_owner: &mut retirement,
        cancellation: &cancellation,
        monotonic_now_ms: 1_700_000_000_500,
      },
      &memory,
    )
    .unwrap();
  assert!(!receipt.idempotent);
  assert!(!selected_root_map_is_absent(&publisher));

  let committed_retry = LegacyRootMapStagingWorkspaceV1::reopen(
    &workspace_path,
    identity(algorithm),
    LegacyRootMapWorkspaceReopenOptionsV1::new(64 << 20, 100, 0, 1_200, 2, 8, 4 << 10).unwrap(),
    cancellation.clone(),
    &constrained,
  )
  .unwrap();
  let error = LegacyRootMapOwnerV1::new(&publisher)
    .publish(
      LegacyRootMapPublicationRequestV1 {
        workspace: committed_retry,
        retirement_owner: &mut retirement,
        cancellation: &cancellation,
        monotonic_now_ms: 1_700_000_000_600,
      },
      &constrained,
    )
    .unwrap_err();
  assert_eq!(error.code(), "migration_root_map_selection_committed");
  let committed = error.committed_receipt().unwrap();
  assert!(committed.idempotent);
  assert_eq!(committed.control_sequence, receipt.control_sequence);
  assert_eq!(committed.control_payload_hash, receipt.control_payload_hash);
}

#[test]
fn migration_root_authority_memory_refusal_never_moves_destination_or_selects_map() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (directory, database_path, publisher) = create_publisher_for(algorithm);
  let cancellation = CancellationToken::new();
  let memory = memory();
  let scratch = private_scratch(directory.path());
  let authority =
    authority_for(algorithm, SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured });
  let tree = publish_tree(&publisher, algorithm, 0x41);
  let (row, namespace) = staged_mapping(algorithm, &authority, 0x41, &tree, 7);
  let mut workspace = LegacyRootMapStagingWorkspaceV1::create(
    &database_path,
    identity(algorithm),
    1_700_000_000_300,
    LegacyRootMapWorkspaceOptionsV1::new(Some(scratch), 64 << 20, 100, 0, 1 << 20, 2, 8, 2 << 20).unwrap(),
    cancellation.clone(),
    &memory,
  )
  .unwrap();
  workspace.stage_mapping(&row, &namespace).unwrap();
  let destination_head = publisher.observe().unwrap().selected.header.head_hash.clone();
  workspace.seal(1, 0, [0xa3; 32], [0xb3; 32], &destination_head).unwrap();
  let before = publisher.observe().unwrap().selected.header.clone();
  let constrained = MemoryCoordinator::new(MemoryPolicy::new(5 << 20, 10 << 20, 1, 1 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);

  let error = LegacyRootMapOwnerV1::new(&publisher)
    .publish(
      LegacyRootMapPublicationRequestV1 {
        workspace,
        retirement_owner: &mut retirement,
        cancellation: &cancellation,
        monotonic_now_ms: 1_700_000_000_400,
      },
      &constrained,
    )
    .unwrap_err();
  assert_eq!(error.code(), "migration_map_authority_memory");
  assert_eq!(publisher.observe().unwrap().selected.header, before);
  assert!(selected_root_map_is_absent(&publisher));
}

#[test]
fn mapping_the_selected_head_reuses_its_existing_head_admission() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (directory, database_path, publisher) = create_publisher_for(algorithm);
  let cancellation = CancellationToken::new();
  let memory = memory();
  let scratch = private_scratch(directory.path());
  let authority =
    authority_for(algorithm, SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured });
  let destination_tree = digest_parts(algorithm, &[b"dirc:"]);
  let (row, namespace) = staged_mapping(algorithm, &authority, 0x42, &destination_tree, 7);
  let destination_head = publisher.observe().unwrap().selected.header.head_hash.clone();
  assert_eq!(namespace.root_hash, destination_head);
  let original_admission =
    publisher.load_immutable_system_control(SystemControlKindV1::RootAdmissionCommit, &DATABASE_ID, &destination_head).unwrap().unwrap();
  assert_eq!(decode_root_admission_commit(&original_admission.bytes, algorithm).unwrap().authority_kind, RootAuthorityKindV1::Head);

  let mut workspace = LegacyRootMapStagingWorkspaceV1::create(
    &database_path,
    identity(algorithm),
    1_700_000_000_300,
    options(&scratch),
    cancellation.clone(),
    &memory,
  )
  .unwrap();
  workspace.stage_mapping(&row, &namespace).unwrap();
  workspace.seal(1, 0, [0xa1; 32], [0xb1; 32], &destination_head).unwrap();
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  LegacyRootMapOwnerV1::new(&publisher)
    .publish(
      LegacyRootMapPublicationRequestV1 {
        workspace,
        retirement_owner: &mut retirement,
        cancellation: &cancellation,
        monotonic_now_ms: 1_700_000_000_400,
      },
      &memory,
    )
    .unwrap();

  let retained_admission =
    publisher.load_immutable_system_control(SystemControlKindV1::RootAdmissionCommit, &DATABASE_ID, &destination_head).unwrap().unwrap();
  assert_eq!(retained_admission.bytes, original_admission.bytes);
  assert_eq!(decode_root_admission_commit(&retained_admission.bytes, algorithm).unwrap().authority_kind, RootAuthorityKindV1::Head);
}

#[test]
fn selected_reader_rejects_missing_inconsistent_and_foreign_incarnation_authority() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _database_path, publisher) = create_publisher_for(algorithm);
  let cancellation = CancellationToken::new();
  let memory = memory();
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let page_hash = legacy_root_map_page_identity_hash(algorithm, DATABASE_ID, MIGRATION_ID, 0).unwrap();
  let control_body = LegacyRootMapControlBodyV1 {
    database_id: DATABASE_ID,
    migration_id: MIGRATION_ID,
    logical_database_id: DATABASE_ID,
    source_physical_instance_id: SOURCE_PHYSICAL_ID,
    destination_physical_instance_id: DESTINATION_PHYSICAL_ID,
    map_generation: 1,
    page_count: 1,
    record_count: 1,
    first_page_hash: page_hash.clone(),
    last_page_hash: page_hash,
    complete_map_digest: vec![0x44; algorithm.hash_length()],
  };
  let control = encode_legacy_root_map_control(1, &control_body, algorithm).unwrap();
  publisher
    .publish_mutable_system_control(
      MutableSystemControlPublicationRequestV1 {
        database_id: &DATABASE_ID,
        kind: SystemControlKindV1::LegacyRootMapControl,
        identity: &MIGRATION_ID,
        expected: None,
        guards: &[],
        encoded_control: &control,
        publication_timestamp_ms: 1_700_000_000_300,
        monotonic_now_ms: 1_700_000_000_300,
      },
      &mut retirement,
    )
    .unwrap();
  let error = VerifiedLegacyRootMapReaderV1::open(&publisher, DATABASE_ID, MIGRATION_ID, &cancellation, &memory)
    .err()
    .expect("missing selected page must refuse the reader");
  assert_eq!(error.code(), "migration_root_map_selected_page_missing");

  let page_identity = [MIGRATION_ID.as_slice(), &0u64.to_le_bytes()].concat();
  let page = encode_legacy_root_map_page(
    &LegacyRootMapPageBodyV1 {
      database_id: DATABASE_ID,
      migration_id: MIGRATION_ID,
      logical_database_id: DATABASE_ID,
      source_physical_instance_id: SOURCE_PHYSICAL_ID,
      destination_physical_instance_id: DESTINATION_PHYSICAL_ID,
      page_ordinal: 0,
      previous_page_hash: vec![0; algorithm.hash_length()],
      next_page_hash: vec![0; algorithm.hash_length()],
      rows: vec![LegacyRootMapRowV1 {
        legacy_root_hash: vec![0x21; algorithm.hash_length()],
        namespace_root_v1_hash: vec![0x31; algorithm.hash_length()],
        semantic_availability: LegacyRootSemanticAvailabilityV1::ContentOnly {
          reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured,
        },
        captured_source_write_sequence: 1,
      }],
    },
    algorithm,
  )
  .unwrap();
  publisher
    .publish_immutable_system_controls(ImmutableSystemControlBatchPublicationRequestV1 {
      database_id: &DATABASE_ID,
      controls: &[ImmutableSystemControlWriteV1 {
        kind: SystemControlKindV1::LegacyRootMapPage,
        identity: &page_identity,
        encoded_control: &page,
      }],
      publication_timestamp_ms: 1_700_000_000_400,
    })
    .unwrap();
  let error = VerifiedLegacyRootMapReaderV1::open(&publisher, DATABASE_ID, MIGRATION_ID, &cancellation, &memory)
    .err()
    .expect("inconsistent selected page chain must refuse the reader");
  assert_eq!(error.code(), "legacy_root_map_chain_digest");

  let foreign_migration_id = [0x73; 16];
  let foreign_control = encode_legacy_root_map_control(
    1,
    &LegacyRootMapControlBodyV1 {
      database_id: DATABASE_ID,
      migration_id: foreign_migration_id,
      logical_database_id: DATABASE_ID,
      source_physical_instance_id: SOURCE_PHYSICAL_ID,
      destination_physical_instance_id: [0x52; 16],
      map_generation: 1,
      page_count: 0,
      record_count: 0,
      first_page_hash: vec![0; algorithm.hash_length()],
      last_page_hash: vec![0; algorithm.hash_length()],
      complete_map_digest: vec![0; algorithm.hash_length()],
    },
    algorithm,
  )
  .unwrap();
  publisher
    .publish_mutable_system_control(
      MutableSystemControlPublicationRequestV1 {
        database_id: &DATABASE_ID,
        kind: SystemControlKindV1::LegacyRootMapControl,
        identity: &foreign_migration_id,
        expected: None,
        guards: &[],
        encoded_control: &foreign_control,
        publication_timestamp_ms: 1_700_000_000_500,
        monotonic_now_ms: 1_700_000_000_500,
      },
      &mut retirement,
    )
    .unwrap();
  let error = VerifiedLegacyRootMapReaderV1::open(&publisher, DATABASE_ID, foreign_migration_id, &cancellation, &memory)
    .err()
    .expect("foreign destination incarnation must refuse the reader");
  assert_eq!(error.code(), "migration_root_map_selected_identity");
}
