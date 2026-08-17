use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Barrier};

use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::directory_entry::{ChildEntry, serialize_child_entries};
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::entity::{
  EntryTypeV4, WHOLE_ENTITY_V1_FLAG_SYSTEM, WholeEntityWriteV1, checked_whole_entity_encoded_length, decode_whole_entity,
  encode_whole_entity,
};
use aeordb::engine::v4::first_authority::{
  FirstAuthorityPublicationReceiptV1, FirstAuthorityPublicationRequestV1, ImmutableEntityBatchPublicationRequestV1, ImmutableEntityWriteV1,
  PreparedNamespaceTreeV0, SuccessorAuthorityPublicationRequestV1, V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, encode_semantic_state_object};
use aeordb::engine::v4::root_authority::decode_root_admission_commit;
use aeordb::engine::hot_tail::read_hot_tail_checked;
use aeordb::engine::{CompressionAlgorithm, DiskKVStore, HashAlgorithm};

fn content_only_semantic_state(algorithm: HashAlgorithm) -> aeordb::engine::v4::namespace::EncodedSemanticObjectV1 {
  encode_semantic_state_object(
    &SemanticStateWriteV1 {
      required_capabilities: [0; 32],
      availability: SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured },
    },
    algorithm,
  )
  .unwrap()
}

fn initialized_publisher(algorithm: HashAlgorithm) -> (tempfile::TempDir, Arc<DurabilityCoordinator>, V4FirstAuthorityPublisher) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("migration-execution.aeordb");
  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(path).unwrap();
  let kv_block_length = initial_block_size() as u64;
  let hash_width = algorithm.hash_length();
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
    head_hash: vec![0; hash_width],
    base_hash: vec![0; hash_width],
    target_hash: vec![0; hash_width],
    required_writer_capabilities: [0; 32],
    system_family_registry_version: 1,
    system_family_registry_fingerprint: vec![0x41; hash_width],
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
  let publisher = V4FirstAuthorityPublisher::new(kv, coordinator.clone()).unwrap();
  let semantic_state = content_only_semantic_state(algorithm);
  publisher
    .publish(&FirstAuthorityPublicationRequestV1 {
      database_id: header.database_id,
      transaction_id: [0x61; 16],
      created_at_ms: header.created_at_ms + 1,
      namespace_tree: PreparedNamespaceTreeV0 { root_hash: digest_parts(algorithm, &[b"dirc:"]), stored_value: Vec::new() },
      semantic_state,
      required_capabilities: [0; 32],
      typed_closure_digest: digest_parts(algorithm, &[b"migration execution initial closure"]),
      authority_identity: b"HEAD".to_vec(),
    })
    .unwrap();
  (directory, coordinator, publisher)
}

fn reopen_publisher(path: &Path) -> V4FirstAuthorityPublisher {
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

fn read_published_entity(directory: &tempfile::TempDir, publisher: &V4FirstAuthorityPublisher, key: &[u8]) -> Vec<u8> {
  let locator = publisher.locator(key).unwrap().expect("published locator");
  let mut file = OpenOptions::new().read(true).open(directory.path().join("migration-execution.aeordb")).unwrap();
  file.seek(SeekFrom::Start(locator.offset)).unwrap();
  let mut bytes = vec![0; locator.total_length as usize];
  file.read_exact(&mut bytes).unwrap();
  bytes
}

fn successor_request(
  algorithm: HashAlgorithm,
  header: &DatabaseHeaderV4,
  transaction_byte: u8,
  created_at_ms: u64,
  child_name: &str,
) -> SuccessorAuthorityPublicationRequestV1 {
  let root_value = serialize_child_entries(
    &[ChildEntry {
      entry_type: EntryTypeV4::FileRecord.to_u8(),
      hash: digest_parts(algorithm, &[b"filec:", child_name.as_bytes()]),
      total_size: 1,
      created_at: created_at_ms as i64,
      updated_at: created_at_ms as i64,
      name: child_name.to_string(),
      content_type: Some("text/plain".to_string()),
      virtual_time: 1,
      node_id: 1,
    }],
    algorithm.hash_length(),
  )
  .unwrap();
  SuccessorAuthorityPublicationRequestV1 {
    database_id: header.database_id,
    transaction_id: [transaction_byte; 16],
    created_at_ms,
    expected_head_hash: header.head_hash.clone(),
    namespace_tree: PreparedNamespaceTreeV0 { root_hash: digest_parts(algorithm, &[b"dirc:", &root_value]), stored_value: root_value },
    semantic_state: content_only_semantic_state(algorithm),
    required_capabilities: [0; 32],
    typed_closure_digest: digest_parts(algorithm, &[b"migration successor closure", child_name.as_bytes()]),
    authority_identity: b"HEAD".to_vec(),
  }
}

#[test]
fn bounded_immutable_entity_batch_is_atomic_idempotent_and_preserves_head() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (directory, coordinator, publisher) = initialized_publisher(algorithm);
    let before = publisher.observe().unwrap();
    let chunk = b"migration chunk";
    let chunk_key = digest_parts(algorithm, &[b"chunk:", chunk]);
    let directory_value = serialize_child_entries(
      &[ChildEntry {
        entry_type: EntryTypeV4::FileRecord.to_u8(),
        hash: digest_parts(algorithm, &[b"filec:", b"unselected child"]),
        total_size: 0,
        created_at: 1_700_000_000_001,
        updated_at: 1_700_000_000_001,
        name: "child".to_string(),
        content_type: None,
        virtual_time: 1,
        node_id: 1,
      }],
      algorithm.hash_length(),
    )
    .unwrap();
    let directory_key = digest_parts(algorithm, &[b"dirc:", &directory_value]);
    let entities = [
      ImmutableEntityWriteV1 { entity_version: 0, entry_type: EntryTypeV4::Chunk, flags: 0, key: &chunk_key, stored_value: chunk },
      ImmutableEntityWriteV1 {
        entity_version: 0,
        entry_type: EntryTypeV4::DirectoryIndex,
        flags: 0,
        key: &directory_key,
        stored_value: &directory_value,
      },
    ];
    let request = ImmutableEntityBatchPublicationRequestV1 {
      database_id: &before.selected.header.database_id,
      entities: &entities,
      publication_timestamp_ms: before.selected.header.updated_at_ms + 1,
    };

    let receipt = publisher.publish_immutable_entity_batch(request).unwrap();

    assert!(!receipt.idempotent);
    assert_eq!(receipt.entities.len(), 2);
    assert!(receipt.entities.iter().all(|entity| !entity.idempotent));
    assert_eq!(receipt.observation.selected.header.head_hash, before.selected.header.head_hash);
    assert_eq!(receipt.observation.selected.header.write_sequence_high_water, before.selected.header.write_sequence_high_water + 2);
    assert_eq!(receipt.observation.selected.header.entry_count, before.selected.header.entry_count + 2);
    assert!(publisher.locator(&chunk_key).unwrap().is_some());
    assert!(publisher.locator(&directory_key).unwrap().is_some());
    let high_water = receipt.observation.selected.header.write_sequence_high_water;
    assert_eq!(
      decode_whole_entity(&read_published_entity(&directory, &publisher, &chunk_key), algorithm, high_water).unwrap().entity_version,
      0
    );
    assert_eq!(
      decode_whole_entity(&read_published_entity(&directory, &publisher, &directory_key), algorithm, high_water).unwrap().entity_version,
      0
    );
    let hard_frontier = coordinator.snapshot().unwrap().hard_frontier;

    let retry = publisher.publish_immutable_entity_batch(request).unwrap();

    assert!(retry.idempotent);
    assert!(retry.entities.iter().all(|entity| entity.idempotent));
    assert_eq!(retry.observation, receipt.observation);
    assert_eq!(coordinator.snapshot().unwrap().hard_frontier, hard_frontier);
  }
}

#[test]
fn bounded_immutable_entity_reader_enforces_key_and_allocation_limits() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, _coordinator, publisher) = initialized_publisher(algorithm);
    let observation = publisher.observe().unwrap();
    let chunk = b"bounded immutable read";
    let chunk_key = digest_parts(algorithm, &[b"chunk:", chunk]);
    publisher
      .publish_immutable_entity_batch(ImmutableEntityBatchPublicationRequestV1 {
        database_id: &observation.selected.header.database_id,
        entities: &[ImmutableEntityWriteV1 {
          entity_version: 0,
          entry_type: EntryTypeV4::Chunk,
          flags: 0,
          key: &chunk_key,
          stored_value: chunk,
        }],
        publication_timestamp_ms: observation.selected.header.updated_at_ms + 1,
      })
      .unwrap();

    assert_eq!(publisher.load_immutable_entity_bounded(&chunk_key, 0).unwrap_err().code(), "immutable_entity_read_bound");
    assert_eq!(
      publisher.load_immutable_entity_bounded(&chunk_key[..chunk_key.len() - 1], 1024).unwrap_err().code(),
      "immutable_entity_read_key"
    );
    assert_eq!(
      publisher.load_immutable_entity_bounded(&vec![0; algorithm.hash_length()], 1024).unwrap_err().code(),
      "immutable_entity_read_key"
    );
    let missing = digest_parts(algorithm, &[b"missing immutable entity"]);
    assert!(publisher.load_immutable_entity_bounded(&missing, 1024).unwrap().is_none());
    assert_eq!(publisher.load_immutable_entity_bounded(&chunk_key, 1).unwrap_err().code(), "first_authority_locator_exceeds_cap");

    let loaded = publisher.load_immutable_entity_bounded(&chunk_key, 1024).unwrap().unwrap();
    assert_eq!(loaded.entity_version, 0);
    assert_eq!(loaded.entry_type, EntryTypeV4::Chunk);
    assert_eq!(loaded.flags, 0);
    assert_eq!(loaded.compression_algorithm, CompressionAlgorithm::None);
    assert_eq!(loaded.key, chunk_key);
    assert_eq!(loaded.stored_value, chunk);
  }
}

#[test]
fn immutable_entity_batch_refuses_invalid_bounds_roles_and_collisions_without_publication() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _coordinator, publisher) = initialized_publisher(algorithm);
  let before = publisher.observe().unwrap();
  let database_id = before.selected.header.database_id;
  let chunk = b"exact chunk";
  let chunk_key = digest_parts(algorithm, &[b"chunk:", chunk]);
  let valid = ImmutableEntityWriteV1 { entity_version: 0, entry_type: EntryTypeV4::Chunk, flags: 0, key: &chunk_key, stored_value: chunk };

  let empty = ImmutableEntityBatchPublicationRequestV1 { database_id: &database_id, entities: &[], publication_timestamp_ms: 1 };
  assert_eq!(publisher.publish_immutable_entity_batch(empty).unwrap_err().code(), "immutable_entity_batch_count");

  let too_many = vec![valid; 512];
  let request = ImmutableEntityBatchPublicationRequestV1 { database_id: &database_id, entities: &too_many, publication_timestamp_ms: 1 };
  assert_eq!(publisher.publish_immutable_entity_batch(request).unwrap_err().code(), "immutable_entity_batch_count");

  let request = ImmutableEntityBatchPublicationRequestV1 { database_id: &[0x77; 16], entities: &[valid], publication_timestamp_ms: 1 };
  assert_eq!(publisher.publish_immutable_entity_batch(request).unwrap_err().code(), "immutable_entity_database_mismatch");
  let request = ImmutableEntityBatchPublicationRequestV1 { database_id: &database_id, entities: &[valid], publication_timestamp_ms: 0 };
  assert_eq!(publisher.publish_immutable_entity_batch(request).unwrap_err().code(), "immutable_entity_publication_time");
  let request = ImmutableEntityBatchPublicationRequestV1 {
    database_id: &database_id,
    entities: &[valid],
    publication_timestamp_ms: i64::MAX as u64 + 1,
  };
  assert_eq!(publisher.publish_immutable_entity_batch(request).unwrap_err().code(), "immutable_entity_publication_time");

  let short_key = &chunk_key[..chunk_key.len() - 1];
  let short = ImmutableEntityWriteV1 { entity_version: 0, entry_type: EntryTypeV4::Chunk, flags: 0, key: short_key, stored_value: chunk };
  let request = ImmutableEntityBatchPublicationRequestV1 { database_id: &database_id, entities: &[short], publication_timestamp_ms: 1 };
  assert_eq!(publisher.publish_immutable_entity_batch(request).unwrap_err().code(), "immutable_entity_key_width");

  let fabricated_key = vec![0x33; algorithm.hash_length()];
  let fabricated =
    ImmutableEntityWriteV1 { entity_version: 0, entry_type: EntryTypeV4::Chunk, flags: 0, key: &fabricated_key, stored_value: chunk };
  let request =
    ImmutableEntityBatchPublicationRequestV1 { database_id: &database_id, entities: &[fabricated], publication_timestamp_ms: 1 };
  assert_eq!(publisher.publish_immutable_entity_batch(request).unwrap_err().code(), "immutable_entity_content_identity");

  let duplicates = [valid, valid];
  let request = ImmutableEntityBatchPublicationRequestV1 { database_id: &database_id, entities: &duplicates, publication_timestamp_ms: 1 };
  assert_eq!(publisher.publish_immutable_entity_batch(request).unwrap_err().code(), "immutable_entity_duplicate");

  for entry_type in [
    EntryTypeV4::DeletionRecord,
    EntryTypeV4::Snapshot,
    EntryTypeV4::Void,
    EntryTypeV4::Fork,
    EntryTypeV4::IndexArtifact,
    EntryTypeV4::GcArtifact,
  ] {
    let specialized = ImmutableEntityWriteV1 { entity_version: 1, entry_type, flags: 1, key: &chunk_key, stored_value: chunk };
    let request =
      ImmutableEntityBatchPublicationRequestV1 { database_id: &database_id, entities: &[specialized], publication_timestamp_ms: 1 };
    assert_eq!(publisher.publish_immutable_entity_batch(request).unwrap_err().code(), "immutable_entity_specialized_type");
  }

  let system = ImmutableEntityWriteV1 {
    entity_version: 0,
    entry_type: EntryTypeV4::Chunk,
    flags: WHOLE_ENTITY_V1_FLAG_SYSTEM,
    key: &chunk_key,
    stored_value: chunk,
  };
  let request = ImmutableEntityBatchPublicationRequestV1 { database_id: &database_id, entities: &[system], publication_timestamp_ms: 1 };
  assert_eq!(publisher.publish_immutable_entity_batch(request).unwrap_err().code(), "immutable_entity_representation");

  let wrong_version = ImmutableEntityWriteV1 { entity_version: 1, ..valid };
  let request =
    ImmutableEntityBatchPublicationRequestV1 { database_id: &database_id, entities: &[wrong_version], publication_timestamp_ms: 1 };
  assert_eq!(publisher.publish_immutable_entity_batch(request).unwrap_err().code(), "immutable_entity_version");

  let oversized_value = vec![0; 64 * 1024 * 1024];
  let oversized =
    ImmutableEntityWriteV1 { entity_version: 0, entry_type: EntryTypeV4::Chunk, flags: 0, key: &chunk_key, stored_value: &oversized_value };
  let request = ImmutableEntityBatchPublicationRequestV1 { database_id: &database_id, entities: &[oversized], publication_timestamp_ms: 1 };
  assert_eq!(publisher.publish_immutable_entity_batch(request).unwrap_err().code(), "immutable_entity_batch_bytes");

  let unknown_flags =
    ImmutableEntityWriteV1 { entity_version: 0, entry_type: EntryTypeV4::Chunk, flags: 0x80, key: &chunk_key, stored_value: chunk };
  let request =
    ImmutableEntityBatchPublicationRequestV1 { database_id: &database_id, entities: &[unknown_flags], publication_timestamp_ms: 1 };
  assert_eq!(publisher.publish_immutable_entity_batch(request).unwrap_err().code(), "unknown_entity_flags");
  assert_eq!(publisher.observe().unwrap(), before);

  publisher
    .publish_immutable_entity_batch(ImmutableEntityBatchPublicationRequestV1 {
      database_id: &database_id,
      entities: &[valid],
      publication_timestamp_ms: before.selected.header.updated_at_ms + 1,
    })
    .unwrap();
  let collision =
    ImmutableEntityWriteV1 { entity_version: 0, entry_type: EntryTypeV4::Chunk, flags: 0, key: &chunk_key, stored_value: b"different" };
  let collision_before = publisher.observe().unwrap();
  let request = ImmutableEntityBatchPublicationRequestV1 {
    database_id: &database_id,
    entities: &[collision],
    publication_timestamp_ms: collision_before.selected.header.updated_at_ms + 1,
  };
  assert_eq!(publisher.publish_immutable_entity_batch(request).unwrap_err().code(), "immutable_entity_content_identity");
  assert_eq!(publisher.observe().unwrap(), collision_before);
}

#[test]
fn immutable_entity_batch_can_mix_existing_and_new_entities_without_rewriting_the_existing_identity() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _coordinator, publisher) = initialized_publisher(algorithm);
  let before = publisher.observe().unwrap();
  let empty_directory_key = digest_parts(algorithm, &[b"dirc:"]);
  let chunk = b"new alongside existing";
  let chunk_key = digest_parts(algorithm, &[b"chunk:", chunk]);
  let entities = [
    ImmutableEntityWriteV1 {
      entity_version: 0,
      entry_type: EntryTypeV4::DirectoryIndex,
      flags: 0,
      key: &empty_directory_key,
      stored_value: &[],
    },
    ImmutableEntityWriteV1 { entity_version: 0, entry_type: EntryTypeV4::Chunk, flags: 0, key: &chunk_key, stored_value: chunk },
  ];

  let receipt = publisher
    .publish_immutable_entity_batch(ImmutableEntityBatchPublicationRequestV1 {
      database_id: &before.selected.header.database_id,
      entities: &entities,
      publication_timestamp_ms: before.selected.header.updated_at_ms + 1,
    })
    .unwrap();

  assert!(!receipt.idempotent);
  assert!(receipt.entities[0].idempotent);
  assert!(!receipt.entities[1].idempotent);
  assert_eq!(receipt.observation.selected.header.write_sequence_high_water, before.selected.header.write_sequence_high_water + 1);
  assert_eq!(receipt.observation.selected.header.entry_count, before.selected.header.entry_count + 1);
}

#[test]
fn immutable_entity_batch_accepts_the_exact_count_and_encoded_byte_caps() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _coordinator, publisher) = initialized_publisher(algorithm);
  let before = publisher.observe().unwrap();
  let database_id = before.selected.header.database_id;
  let values = (0u32..511).map(u32::to_le_bytes).collect::<Vec<_>>();
  let keys = values.iter().map(|value| digest_parts(algorithm, &[b"chunk:", value])).collect::<Vec<_>>();
  let entities = keys
    .iter()
    .zip(&values)
    .map(|(key, value)| ImmutableEntityWriteV1 { entity_version: 0, entry_type: EntryTypeV4::Chunk, flags: 0, key, stored_value: value })
    .collect::<Vec<_>>();

  let receipt = publisher
    .publish_immutable_entity_batch(ImmutableEntityBatchPublicationRequestV1 {
      database_id: &database_id,
      entities: &entities,
      publication_timestamp_ms: before.selected.header.updated_at_ms + 1,
    })
    .unwrap();

  assert_eq!(receipt.entities.len(), 511);
  assert_eq!(receipt.observation.selected.header.entry_count, before.selected.header.entry_count + 511);

  let (_directory, _coordinator, publisher) = initialized_publisher(algorithm);
  let before = publisher.observe().unwrap();
  let key_width = algorithm.hash_length();
  let empty_encoded_length = checked_whole_entity_encoded_length(algorithm, key_width, 0).unwrap();
  let value = vec![0x5a; 64 * 1024 * 1024 - empty_encoded_length];
  let key = digest_parts(algorithm, &[b"chunk:", &value]);
  assert_eq!(checked_whole_entity_encoded_length(algorithm, key_width, value.len()).unwrap(), 64 * 1024 * 1024);
  let entities = [ImmutableEntityWriteV1 { entity_version: 0, entry_type: EntryTypeV4::Chunk, flags: 0, key: &key, stored_value: &value }];

  let receipt = publisher
    .publish_immutable_entity_batch(ImmutableEntityBatchPublicationRequestV1 {
      database_id: &before.selected.header.database_id,
      entities: &entities,
      publication_timestamp_ms: before.selected.header.updated_at_ms + 1,
    })
    .unwrap();

  assert_eq!(receipt.entities.len(), 1);
  assert!(publisher.locator(&key).unwrap().is_some());
}

#[test]
fn successor_authority_advances_head_atomically_reuses_semantics_and_retains_the_prior_root() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, coordinator, publisher) = initialized_publisher(algorithm);
    let initial = publisher.observe().unwrap();
    let prior_head = initial.selected.header.head_hash.clone();
    let request = successor_request(algorithm, &initial.selected.header, 0x72, initial.selected.header.updated_at_ms + 2, "cloned.txt");
    let root_hash = request.namespace_tree.root_hash.clone();
    publisher
      .publish_immutable_entity_batch(ImmutableEntityBatchPublicationRequestV1 {
        database_id: &initial.selected.header.database_id,
        entities: &[ImmutableEntityWriteV1 {
          entity_version: 0,
          entry_type: EntryTypeV4::DirectoryIndex,
          flags: 0,
          key: &root_hash,
          stored_value: &request.namespace_tree.stored_value,
        }],
        publication_timestamp_ms: initial.selected.header.updated_at_ms + 1,
      })
      .unwrap();
    let before = publisher.observe().unwrap();
    assert_eq!(request.expected_head_hash, prior_head);
    let minimum_publication_sequence = coordinator.snapshot().unwrap().next_sequence;

    let receipt = publisher.publish_successor_authority(&request).unwrap();

    assert!(!receipt.idempotent);
    assert!(receipt.publication_sequence >= minimum_publication_sequence);
    assert_eq!(coordinator.snapshot().unwrap().hard_frontier, receipt.publication_sequence);
    assert_eq!(receipt.observation.selected.header.head_hash, receipt.namespace_root.root_hash);
    let decoded_root = aeordb::engine::v4::namespace::decode_namespace_root(&receipt.namespace_root.value, algorithm).unwrap();
    assert_eq!(decoded_root.namespace_tree_root, root_hash);
    assert_eq!(receipt.observation.selected.header.write_sequence_high_water, before.selected.header.write_sequence_high_water + 5);
    assert_eq!(receipt.observation.selected.header.entry_count, before.selected.header.entry_count + 5);
    assert!(publisher.locator(&prior_head).unwrap().is_some());
    assert!(publisher.locator(&receipt.namespace_root.root_hash).unwrap().is_some());
    assert!(publisher.admission_locator(&receipt.namespace_root.root_hash).unwrap().is_some());

    let hard_frontier = coordinator.snapshot().unwrap().hard_frontier;
    let retry = publisher.publish_successor_authority(&request).unwrap();
    assert!(retry.idempotent);
    assert_eq!(retry.namespace_root, receipt.namespace_root);
    assert_eq!(retry.prepare_control, receipt.prepare_control);
    assert_eq!(retry.admission_control, receipt.admission_control);
    assert_eq!(retry.publication_sequence, receipt.publication_sequence);
    assert_eq!(retry.observation, receipt.observation);
    assert_eq!(coordinator.snapshot().unwrap().hard_frontier, hard_frontier);
  }
}

#[test]
fn successor_authority_can_atomically_supply_a_missing_target_root() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _coordinator, publisher) = initialized_publisher(algorithm);
  let before = publisher.observe().unwrap();
  let request = successor_request(algorithm, &before.selected.header, 0x73, before.selected.header.updated_at_ms + 1, "not-precloned.txt");

  let receipt = publisher.publish_successor_authority(&request).unwrap();

  assert_eq!(receipt.observation.selected.header.write_sequence_high_water, before.selected.header.write_sequence_high_water + 6);
  assert_eq!(receipt.observation.selected.header.entry_count, before.selected.header.entry_count + 6);
  assert!(publisher.locator(&request.namespace_tree.root_hash).unwrap().is_some());
}

#[test]
fn successor_authority_can_reselect_a_previously_admitted_root_and_retry_after_restart() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (directory, _coordinator, publisher) = initialized_publisher(algorithm);
    let initial = publisher.observe().unwrap();
    let initial_root = initial.selected.header.head_hash.clone();
    let initial_admission_locator = publisher.admission_locator(&initial_root).unwrap().unwrap();

    let away = successor_request(algorithm, &initial.selected.header, 0x73, initial.selected.header.updated_at_ms + 1, "away.txt");
    let away_receipt = publisher.publish_successor_authority(&away).unwrap();
    let return_request = SuccessorAuthorityPublicationRequestV1 {
      database_id: initial.selected.header.database_id,
      transaction_id: [0x74; 16],
      created_at_ms: away_receipt.observation.selected.header.updated_at_ms + 1,
      expected_head_hash: away_receipt.namespace_root.root_hash.clone(),
      namespace_tree: PreparedNamespaceTreeV0 { root_hash: digest_parts(algorithm, &[b"dirc:"]), stored_value: Vec::new() },
      semantic_state: content_only_semantic_state(algorithm),
      required_capabilities: [0; 32],
      typed_closure_digest: digest_parts(algorithm, &[b"migration execution return closure"]),
      authority_identity: b"HEAD".to_vec(),
    };
    let before_return = publisher.observe().unwrap();

    let returned = publisher.publish_successor_authority(&return_request).unwrap();

    assert!(!returned.idempotent);
    assert_eq!(returned.namespace_root.root_hash, initial_root);
    assert_eq!(returned.observation.selected.header.head_hash, initial_root);
    assert_eq!(returned.observation.selected.header.write_sequence_high_water, before_return.selected.header.write_sequence_high_water + 2);
    assert_eq!(returned.observation.selected.header.entry_count, before_return.selected.header.entry_count + 2);
    assert_eq!(publisher.admission_locator(&initial_root).unwrap().unwrap(), initial_admission_locator);
    let original_admission = decode_root_admission_commit(&returned.admission_control, algorithm).unwrap();
    assert_eq!(original_admission.transaction_id, [0x61; 16]);
    assert_ne!(returned.publication_sequence, original_admission.publication_sequence);

    let retry = publisher.publish_successor_authority(&return_request).unwrap();
    assert!(retry.idempotent);
    assert_eq!(retry, FirstAuthorityPublicationReceiptV1 { idempotent: true, ..returned.clone() });
    drop(publisher);

    let reopened = reopen_publisher(&directory.path().join("migration-execution.aeordb"));
    let reopened_retry = reopened.publish_successor_authority(&return_request).unwrap();
    assert!(reopened_retry.idempotent);
    assert_eq!(reopened_retry, retry);
  }
}

#[test]
fn successor_authority_refuses_invalid_authority_and_retry_inputs_without_moving_head() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _coordinator, publisher) = initialized_publisher(algorithm);
  let before = publisher.observe().unwrap();
  let base = successor_request(algorithm, &before.selected.header, 0x74, before.selected.header.updated_at_ms + 1, "validation.txt");
  let assert_rejected = |request: SuccessorAuthorityPublicationRequestV1, expected_code: &str| {
    let selected_before = publisher.observe().unwrap();
    let error = publisher.publish_successor_authority(&request).unwrap_err();
    assert_eq!(error.code(), expected_code);
    assert_eq!(publisher.observe().unwrap(), selected_before);
  };

  let mut request = base.clone();
  request.created_at_ms = 0;
  assert_rejected(request, "successor_authority_timestamp_range");
  let mut request = base.clone();
  request.created_at_ms = i64::MAX as u64 + 1;
  assert_rejected(request, "successor_authority_timestamp_range");
  let mut request = base.clone();
  request.database_id = [0x91; 16];
  assert_rejected(request, "successor_authority_database_mismatch");
  let mut request = base.clone();
  request.expected_head_hash = vec![0; algorithm.hash_length()];
  assert_rejected(request, "successor_authority_expected_head");
  let mut request = base.clone();
  request.expected_head_hash.pop();
  assert_rejected(request, "successor_authority_expected_head");
  let mut request = base.clone();
  request.expected_head_hash = digest_parts(algorithm, &[b"another selected head"]);
  assert_rejected(request, "successor_authority_stale_head");
  let mut request = base.clone();
  request.transaction_id = [0; 16];
  assert_rejected(request, "root_prepare_identity");
  let mut request = base.clone();
  request.authority_identity.clear();
  assert_rejected(request, "root_prepare_authority_length");
  let mut request = base.clone();
  request.typed_closure_digest.pop();
  assert_rejected(request, "root_prepare_hashes");
  let mut request = base.clone();
  request.namespace_tree.root_hash = digest_parts(algorithm, &[b"wrong root identity"]);
  assert_rejected(request, "namespace_tree_content_identity");

  let mut alternate_capabilities = [0; 32];
  alternate_capabilities[0] = 1;
  let unavailable_semantic = encode_semantic_state_object(
    &SemanticStateWriteV1 {
      required_capabilities: alternate_capabilities,
      availability: SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyDependencyCannotBeProven },
    },
    algorithm,
  )
  .unwrap();
  let mut request = base;
  request.semantic_state = unavailable_semantic;
  assert_rejected(request, "successor_authority_semantic_state_missing");
}

#[test]
fn successor_authority_refuses_precloned_identity_collisions_and_nonexact_retries() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (directory, _coordinator, publisher) = initialized_publisher(algorithm);
  let before = publisher.observe().unwrap();
  let request = successor_request(algorithm, &before.selected.header, 0x75, before.selected.header.updated_at_ms + 1, "collision.txt");
  publisher
    .publish_immutable_entity_batch(ImmutableEntityBatchPublicationRequestV1 {
      database_id: &before.selected.header.database_id,
      entities: &[ImmutableEntityWriteV1 {
        entity_version: 0,
        entry_type: EntryTypeV4::DirectoryIndex,
        flags: 0,
        key: &request.namespace_tree.root_hash,
        stored_value: &request.namespace_tree.stored_value,
      }],
      publication_timestamp_ms: before.selected.header.updated_at_ms + 1,
    })
    .unwrap();
  let locator = publisher.locator(&request.namespace_tree.root_hash).unwrap().unwrap();
  let path = directory.path().join("migration-execution.aeordb");
  let mut file = OpenOptions::new().read(true).write(true).open(path).unwrap();
  file.seek(SeekFrom::Start(locator.offset)).unwrap();
  let mut original = vec![0; locator.total_length as usize];
  file.read_exact(&mut original).unwrap();
  let decoded = decode_whole_entity(&original, algorithm, u64::MAX).unwrap();
  let mut conflicting_value = decoded.stored_value.to_vec();
  *conflicting_value.last_mut().expect("successor tree is nonempty") ^= 0x01;
  let conflicting = encode_whole_entity(&WholeEntityWriteV1 {
    entity_version: decoded.entity_version,
    entry_type: decoded.entry_type,
    flags: decoded.flags,
    hash_algorithm: algorithm,
    compression_algorithm: CompressionAlgorithm::None,
    timestamp_ms: decoded.timestamp_ms,
    write_sequence: decoded.write_sequence,
    key: decoded.key,
    stored_value: &conflicting_value,
  })
  .unwrap();
  assert_eq!(conflicting.len(), original.len());
  file.seek(SeekFrom::Start(locator.offset)).unwrap();
  file.write_all(&conflicting).unwrap();
  file.sync_all().unwrap();
  let collision_before = publisher.observe().unwrap();
  assert_eq!(publisher.publish_successor_authority(&request).unwrap_err().code(), "immutable_entity_identity_collision");
  assert_eq!(publisher.observe().unwrap(), collision_before);

  let (_directory, _coordinator, publisher) = initialized_publisher(algorithm);
  let before = publisher.observe().unwrap();
  let request = successor_request(algorithm, &before.selected.header, 0x76, before.selected.header.updated_at_ms + 1, "retry.txt");
  let receipt = publisher.publish_successor_authority(&request).unwrap();
  let selected = receipt.observation.clone();

  let mut changed_closure = request.clone();
  changed_closure.typed_closure_digest = digest_parts(algorithm, &[b"changed closure"]);
  assert_eq!(publisher.publish_successor_authority(&changed_closure).unwrap_err().code(), "successor_authority_retry_collision");
  assert_eq!(publisher.observe().unwrap(), selected);

  let mut changed_transaction = request;
  changed_transaction.transaction_id = [0x77; 16];
  assert_eq!(publisher.publish_successor_authority(&changed_transaction).unwrap_err().code(), "successor_authority_witness_mismatch");
  assert_eq!(publisher.observe().unwrap(), selected);
}

#[test]
fn concurrent_successor_publications_from_one_predecessor_have_exactly_one_winner() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _coordinator, publisher) = initialized_publisher(algorithm);
  let before = publisher.observe().unwrap();
  let first = successor_request(algorithm, &before.selected.header, 0x78, before.selected.header.updated_at_ms + 1, "first.txt");
  let second = successor_request(algorithm, &before.selected.header, 0x79, before.selected.header.updated_at_ms + 2, "second.txt");
  let publisher = Arc::new(publisher);
  let barrier = Arc::new(Barrier::new(3));
  let mut workers = Vec::new();
  for request in [first, second] {
    let publisher = Arc::clone(&publisher);
    let barrier = Arc::clone(&barrier);
    workers.push(std::thread::spawn(move || {
      barrier.wait();
      publisher.publish_successor_authority(&request)
    }));
  }
  barrier.wait();
  let results = workers.into_iter().map(|worker| worker.join().unwrap()).collect::<Vec<_>>();

  assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
  assert_eq!(
    results.iter().filter(|result| result.as_ref().is_err_and(|error| error.code() == "successor_authority_stale_head")).count(),
    1
  );
  let winner = results.iter().find_map(|result| result.as_ref().ok()).unwrap();
  let selected = publisher.observe().unwrap();
  assert_eq!(selected.selected.header.head_hash, winner.namespace_root.root_hash);
  assert!(publisher.locator(&before.selected.header.head_hash).unwrap().is_some());
}
