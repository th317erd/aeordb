use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aeordb::engine::directory_entry::{ChildEntry, serialize_child_entries};
use aeordb::engine::btree::{BTreeNode, LeafNode};
use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::file_record::FileRecord;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::entity::EntryTypeV4;
use aeordb::engine::v4::first_authority::{
  FirstAuthorityPublicationRequestV1, ImmutableEntityBatchPublicationRequestV1, ImmutableEntityWriteV1, PreparedNamespaceTreeV0,
  SuccessorAuthorityPublicationRequestV1, V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, decode_namespace_root, encode_semantic_state_object};
use aeordb::engine::v4::read_view::{
  CurrentReadAuthorizationV1, ReadViewAuthorizationErrorV1, ReadViewConcealmentV1, ReadViewCredentialKindV1, ReadViewResolverV1,
  ReadViewSelectorV1, ReadableRootStateV1, RootLifecycleObservationV1, RootPinCoordinatorErrorV1, RootReadPinCoordinatorV1,
};
use aeordb::engine::v4::read_view_authorization::{
  CapturedCurrentPathAuthorizationSourceV1, CurrentPathAuthorizationV1, PathAuthorizationDecisionV1, ReadViewPermissionAuthorizerV1,
};
use aeordb::engine::v4::read_view_native::NativeReadViewSourceV1;
use aeordb::engine::v4::system_family::embedded_system_family_registry;
use aeordb::engine::v4::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use aeordb::engine::permission_resolver::CrudlifyOp;
use aeordb::engine::permissions::{PathPermissions, PermissionLink};
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
    required_reader_capabilities: CapabilitySetV1::v4_baseline().into_bytes(),
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
    required_writer_capabilities: CapabilitySetV1::v4_baseline().into_bytes(),
    system_family_registry_version: 1,
    system_family_registry_fingerprint: embedded_system_family_registry(algorithm).unwrap().operational_fingerprint.clone(),
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

fn all_capabilities_profile() -> BinaryCapabilityProfileV1 {
  let all = CapabilitySetV1::from_bits(0..24).unwrap();
  BinaryCapabilityProfileV1::new(all, all)
}

fn publish_permission_tree(
  publisher: &V4FirstAuthorityPublisher,
  algorithm: HashAlgorithm,
  expected_head_hash: Vec<u8>,
  file_record_version: u8,
  btree_permissions_directory: bool,
  btree_extra_entries: usize,
  chunk_repetitions: usize,
) -> (Vec<u8>, Vec<u8>) {
  let timestamp = 1_700_000_000_300;
  let permission_path = "/docs/.aeordb-permissions";
  let permission_bytes = PathPermissions {
    links: vec![PermissionLink {
      group: "current-editors".to_string(),
      allow: "....l...".to_string(),
      deny: "........".to_string(),
      others_allow: None,
      others_deny: None,
      path_pattern: None,
    }],
  }
  .serialize();
  let chunk_hash = digest_parts(algorithm, &[b"chunk:", &permission_bytes]);
  let mut record = FileRecord {
    path: permission_path.to_string(),
    content_type: Some("application/json".to_string()),
    total_size: permission_bytes.len() as u64,
    created_at: timestamp,
    updated_at: timestamp,
    metadata: Vec::new(),
    content_hash: Vec::new(),
    chunk_hashes: vec![chunk_hash.clone(); chunk_repetitions],
  };
  if file_record_version == 1 {
    record.content_hash = digest_parts(algorithm, &[&permission_bytes]);
  }
  let record_bytes = record.serialize_for_version(algorithm.hash_length(), file_record_version).unwrap();
  let file_hash = digest_parts(algorithm, &[b"filec:", &record_bytes]);
  let permission_entry = ChildEntry {
    entry_type: EntryTypeV4::FileRecord.to_u8(),
    hash: file_hash.clone(),
    total_size: permission_bytes.len() as u64,
    created_at: timestamp,
    updated_at: timestamp,
    name: ".aeordb-permissions".to_string(),
    content_type: Some("application/json".to_string()),
    virtual_time: 1,
    node_id: 1,
  };
  let docs_value = if btree_permissions_directory {
    let mut entries = vec![permission_entry];
    for index in 0..btree_extra_entries {
      entries.push(ChildEntry {
        entry_type: EntryTypeV4::FileRecord.to_u8(),
        hash: file_hash.clone(),
        total_size: permission_bytes.len() as u64,
        created_at: timestamp,
        updated_at: timestamp,
        name: format!("z-extra-{index:04}"),
        content_type: Some("application/json".to_string()),
        virtual_time: 1,
        node_id: index as u64 + 2,
      });
    }
    BTreeNode::Leaf(LeafNode { entries }).serialize(algorithm.hash_length()).unwrap()
  } else {
    serialize_child_entries(&[permission_entry], algorithm.hash_length()).unwrap()
  };
  let docs_domain = if btree_permissions_directory { b"btree:".as_slice() } else { b"dirc:".as_slice() };
  let docs_hash = digest_parts(algorithm, &[docs_domain, &docs_value]);
  let root_value = serialize_child_entries(
    &[ChildEntry {
      entry_type: EntryTypeV4::DirectoryIndex.to_u8(),
      hash: docs_hash.clone(),
      total_size: docs_value.len() as u64,
      created_at: timestamp,
      updated_at: timestamp,
      name: "docs".to_string(),
      content_type: None,
      virtual_time: 1,
      node_id: 1,
    }],
    algorithm.hash_length(),
  )
  .unwrap();
  let root_hash = digest_parts(algorithm, &[b"dirc:", &root_value]);
  let entities = [
    ImmutableEntityWriteV1 {
      entity_version: 0,
      entry_type: EntryTypeV4::Chunk,
      flags: 0,
      key: &chunk_hash,
      stored_value: &permission_bytes,
    },
    ImmutableEntityWriteV1 {
      entity_version: file_record_version,
      entry_type: EntryTypeV4::FileRecord,
      flags: 0,
      key: &file_hash,
      stored_value: &record_bytes,
    },
    ImmutableEntityWriteV1 {
      entity_version: 0,
      entry_type: EntryTypeV4::DirectoryIndex,
      flags: 0,
      key: &docs_hash,
      stored_value: &docs_value,
    },
    ImmutableEntityWriteV1 {
      entity_version: 0,
      entry_type: EntryTypeV4::DirectoryIndex,
      flags: 0,
      key: &root_hash,
      stored_value: &root_value,
    },
  ];
  publisher
    .publish_immutable_entity_batch(ImmutableEntityBatchPublicationRequestV1 {
      database_id: &[0x31; 16],
      entities: &entities,
      publication_timestamp_ms: timestamp as u64,
    })
    .unwrap();
  let namespace_root = publisher
    .publish_successor_authority(&SuccessorAuthorityPublicationRequestV1 {
      database_id: [0x31; 16],
      transaction_id: [0x63; 16],
      created_at_ms: timestamp as u64 + 1,
      expected_head_hash,
      namespace_tree: PreparedNamespaceTreeV0 { root_hash: root_hash.clone(), stored_value: root_value },
      semantic_state: semantic_state(algorithm, aeordb::engine::v4::namespace::SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured),
      required_capabilities: [0; 32],
      typed_closure_digest: digest_parts(algorithm, &[b"read view permission closure"]),
      authority_identity: b"HEAD".to_vec(),
    })
    .unwrap()
    .namespace_root
    .root_hash;
  (namespace_root, chunk_hash)
}

#[test]
fn native_resolver_owns_real_authority_memory_and_pin_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, _path, publisher) = publisher(algorithm);
    let receipt = publisher.publish(&first_request(algorithm)).unwrap();
    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(256 * 1024 * 1024, 512 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
    let grace = if algorithm == HashAlgorithm::Blake3_256 { 0 } else { 86_400_000 };
    let source = Arc::new(NativeReadViewSourceV1::new(Arc::new(publisher), Arc::clone(&memory), grace));
    let pins = RootReadPinCoordinatorV1::new(Arc::clone(&memory), algorithm, 8, 16).unwrap();
    let current = CurrentReadAuthorizationV1::new(
      CurrentPathAuthorizationV1::for_root("/", CrudlifyOp::List),
      ReadViewCredentialKindV1::Ordinary,
      ReadViewConcealmentV1::Conceal,
    );
    let authorizer =
      ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), source.as_ref().clone());
    let resolver = ReadViewResolverV1::new(Arc::clone(&source), pins.clone(), all_capabilities_profile());

    let view = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap();

    assert_eq!(view.root_metadata().hash, receipt.namespace_root.root_hash);
    assert_eq!(pins.active_pin_count().unwrap(), 1);
    assert!(memory.snapshot().unwrap().reserved_bytes > 0);
    let mut retirement_ran = false;
    let retirement_error = pins
      .with_retirement_exclusion(view.root_metadata().hash.as_slice(), &CancellationToken::new(), || {
        retirement_ran = true;
        Ok(())
      })
      .unwrap_err();
    assert!(matches!(retirement_error, RootPinCoordinatorErrorV1::RootPinned));
    assert!(!retirement_ran);
    drop(view);
    assert_eq!(pins.active_pin_count().unwrap(), 0);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn native_selected_permissions_read_flat_and_btree_v0_v1_files_at_both_hash_widths() {
  for (algorithm, version, btree) in [(HashAlgorithm::Blake3_256, 0, false), (HashAlgorithm::Sha512, 1, true)] {
    let (_directory, _path, publisher) = publisher(algorithm);
    let first = publisher.publish(&first_request(algorithm)).unwrap();
    let (expected_root, _) = publish_permission_tree(&publisher, algorithm, first.namespace_root.root_hash, version, btree, 0, 1);
    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(256 * 1024 * 1024, 512 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
    let source = Arc::new(NativeReadViewSourceV1::new(Arc::new(publisher), Arc::clone(&memory), 86_400_000));
    let pins = RootReadPinCoordinatorV1::new(Arc::clone(&memory), algorithm, 8, 16).unwrap();
    let current = CurrentReadAuthorizationV1::new(
      CurrentPathAuthorizationV1::for_user(
        "/docs/",
        CrudlifyOp::List,
        vec!["current-editors".to_string()],
        PathAuthorizationDecisionV1::direct(),
      ),
      ReadViewCredentialKindV1::Ordinary,
      ReadViewConcealmentV1::Conceal,
    );
    let authorizer =
      ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), source.as_ref().clone());
    let resolver = ReadViewResolverV1::new(Arc::clone(&source), pins.clone(), all_capabilities_profile());

    let view = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap();

    assert_eq!(view.root_metadata().hash, expected_root);
    assert!(view.authorization().is_direct());
    drop(view);
    assert_eq!(pins.active_pin_count().unwrap(), 0);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn native_selected_ancestor_navigation_intersects_current_child_names() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = publisher(algorithm);
  let first = publisher.publish(&first_request(algorithm)).unwrap();
  publish_permission_tree(&publisher, algorithm, first.namespace_root.root_hash, 1, true, 0, 1);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(256 * 1024 * 1024, 512 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
  let source = Arc::new(NativeReadViewSourceV1::new(Arc::new(publisher), Arc::clone(&memory), 86_400_000));
  let pins = RootReadPinCoordinatorV1::new(Arc::clone(&memory), algorithm, 8, 16).unwrap();
  let current_children = ["docs".to_string(), "current-only".to_string()].into_iter().collect();
  let current = CurrentReadAuthorizationV1::new(
    CurrentPathAuthorizationV1::for_user(
      "/",
      CrudlifyOp::List,
      vec!["current-editors".to_string()],
      PathAuthorizationDecisionV1::ancestor_navigation(current_children).unwrap(),
    ),
    ReadViewCredentialKindV1::Ordinary,
    ReadViewConcealmentV1::Conceal,
  );
  let authorizer = ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), source.as_ref().clone());
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), pins.clone(), all_capabilities_profile());

  let view = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap();

  assert_eq!(view.authorization().allowed_children().unwrap().iter().cloned().collect::<Vec<_>>(), ["docs"]);
  drop(view);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn native_current_denial_and_authority_pressure_release_every_resource() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = publisher(algorithm);
  publisher.publish(&first_request(algorithm)).unwrap();
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(512 * 1024, 1024 * 1024, 1, 64 * 1024).unwrap()));
  let source = Arc::new(NativeReadViewSourceV1::new(Arc::new(publisher), Arc::clone(&memory), 86_400_000));
  let pins = RootReadPinCoordinatorV1::new(Arc::clone(&memory), algorithm, 8, 16).unwrap();
  let denied = CapturedCurrentPathAuthorizationSourceV1::new(Err(ReadViewAuthorizationErrorV1::denied(ReadViewConcealmentV1::Conceal)));
  let denied_authorizer = ReadViewPermissionAuthorizerV1::new(denied, source.as_ref().clone());
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), pins.clone(), all_capabilities_profile());

  let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &denied_authorizer, &CancellationToken::new()).unwrap_err();
  assert_eq!(error.code(), "read_authorization_denied");
  assert_eq!(pins.active_pin_count().unwrap(), 0);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);

  let current = CurrentReadAuthorizationV1::new(
    CurrentPathAuthorizationV1::for_root("/", CrudlifyOp::List),
    ReadViewCredentialKindV1::Ordinary,
    ReadViewConcealmentV1::Conceal,
  );
  let pressure_authorizer =
    ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), source.as_ref().clone());
  let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &pressure_authorizer, &CancellationToken::new()).unwrap_err();
  assert_eq!(error.code(), "read_view_memory_admission");
  assert_eq!(pins.active_pin_count().unwrap(), 0);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn native_permission_corruption_fails_closed_and_releases_pin_and_memory() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, publisher) = publisher(algorithm);
  let first = publisher.publish(&first_request(algorithm)).unwrap();
  let (_, chunk_hash) = publish_permission_tree(&publisher, algorithm, first.namespace_root.root_hash, 1, false, 0, 1);
  let chunk = publisher.locator(&chunk_hash).unwrap().unwrap();
  let mut file = OpenOptions::new().read(true).write(true).open(path).unwrap();
  file.seek(SeekFrom::Start(chunk.offset + u64::from(chunk.total_length) - 1)).unwrap();
  file.write_all(&[0x7f]).unwrap();
  file.sync_all().unwrap();
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(256 * 1024 * 1024, 512 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
  let source = Arc::new(NativeReadViewSourceV1::new(Arc::new(publisher), Arc::clone(&memory), 86_400_000));
  let pins = RootReadPinCoordinatorV1::new(Arc::clone(&memory), algorithm, 8, 16).unwrap();
  let current = CurrentReadAuthorizationV1::new(
    CurrentPathAuthorizationV1::for_user(
      "/docs/",
      CrudlifyOp::List,
      vec!["current-editors".to_string()],
      PathAuthorizationDecisionV1::direct(),
    ),
    ReadViewCredentialKindV1::Ordinary,
    ReadViewConcealmentV1::Conceal,
  );
  let authorizer = ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), source.as_ref().clone());
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), pins.clone(), all_capabilities_profile());

  let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap_err();

  assert_eq!(error.code(), "read_authorization_corrupt");
  assert_eq!(pins.active_pin_count().unwrap(), 0);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn native_selected_permissions_reject_noncanonical_fanout_and_chunk_amplification() {
  for (btree, extra_entries, chunk_repetitions, expected_message) in
    [(true, 40, 1, "canonical fanout"), (false, 0, 65, "chunk-count bound")]
  {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, _path, publisher) = publisher(algorithm);
    let first = publisher.publish(&first_request(algorithm)).unwrap();
    publish_permission_tree(&publisher, algorithm, first.namespace_root.root_hash, 1, btree, extra_entries, chunk_repetitions);
    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(256 * 1024 * 1024, 512 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
    let source = Arc::new(NativeReadViewSourceV1::new(Arc::new(publisher), Arc::clone(&memory), 86_400_000));
    let pins = RootReadPinCoordinatorV1::new(Arc::clone(&memory), algorithm, 8, 16).unwrap();
    let current = CurrentReadAuthorizationV1::new(
      CurrentPathAuthorizationV1::for_user(
        "/docs/",
        CrudlifyOp::List,
        vec!["current-editors".to_string()],
        PathAuthorizationDecisionV1::direct(),
      ),
      ReadViewCredentialKindV1::Ordinary,
      ReadViewConcealmentV1::Conceal,
    );
    let authorizer =
      ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), source.as_ref().clone());
    let resolver = ReadViewResolverV1::new(Arc::clone(&source), pins.clone(), all_capabilities_profile());

    let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap_err();

    assert_eq!(error.code(), "read_authorization_corrupt");
    assert!(error.to_string().contains(expected_message), "unexpected error: {error}");
    assert_eq!(pins.active_pin_count().unwrap(), 0);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn native_resolver_reads_an_admitted_historical_root_after_head_advances() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = publisher(algorithm);
  let first = publisher.publish(&first_request(algorithm)).unwrap();
  publisher.publish_successor_authority(&successor_request(algorithm, first.namespace_root.root_hash.clone())).unwrap();
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(256 * 1024 * 1024, 512 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
  let source = Arc::new(NativeReadViewSourceV1::new(Arc::new(publisher), Arc::clone(&memory), 86_400_000));
  let pins = RootReadPinCoordinatorV1::new(Arc::clone(&memory), algorithm, 8, 16).unwrap();
  let current = CurrentReadAuthorizationV1::new(
    CurrentPathAuthorizationV1::for_root("/", CrudlifyOp::List),
    ReadViewCredentialKindV1::Ordinary,
    ReadViewConcealmentV1::Conceal,
  );
  let authorizer = ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), source.as_ref().clone());
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), pins.clone(), all_capabilities_profile());

  let view =
    resolver.resolve(ReadViewSelectorV1::ExplicitRoot(&first.namespace_root.root_hash), &authorizer, &CancellationToken::new()).unwrap();

  assert_eq!(view.root_metadata().hash, first.namespace_root.root_hash);
  assert_eq!(view.root_metadata().state, ReadableRootStateV1::Retained);
  drop(view);
  assert_eq!(pins.active_pin_count().unwrap(), 0);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn native_read_view_has_one_production_source_and_no_service_or_v3_storage_bypass() {
  fn collect_rust_files(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).unwrap() {
      let path = entry.unwrap().path();
      if path.is_dir() {
        collect_rust_files(&path, files);
      } else if path.extension().is_some_and(|extension| extension == "rs") {
        files.push(path);
      }
    }
  }

  let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let mut files = Vec::new();
  collect_rust_files(&source_root, &mut files);
  let source_text = files.iter().map(|path| fs::read_to_string(path).unwrap()).collect::<Vec<_>>();
  assert_eq!(source_text.iter().map(|source| source.matches("impl ReadViewAuthoritySourceV1 for").count()).sum::<usize>(), 1,);
  assert_eq!(source_text.iter().map(|source| source.matches("impl SelectedRootPermissionSourceV1 for").count()).sum::<usize>(), 1,);
  assert_eq!(source_text.iter().map(|source| source.matches("load_immutable_entity_at_captured_header(").count()).sum::<usize>(), 2,);
  let native = fs::read_to_string(source_root.join("engine/v4/read_view_native.rs")).unwrap();
  for forbidden in ["crate::server", "DirectoryOps", "StorageEngine", "axum::", "Router<", "route("] {
    assert!(!native.contains(forbidden), "native read-view adapter gained a forbidden service/v3 dependency: {forbidden}");
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
