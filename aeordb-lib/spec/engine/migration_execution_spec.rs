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
  ImmutableSemanticObjectBatchPublicationRequestV1, PreparedNamespaceTreeV0, SuccessorAuthorityPublicationRequestV1,
  V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, encode_semantic_state_object};
use aeordb::engine::v4::namespace::{
  EncodedSemanticObjectV1, SemanticCatalogChildV1, SemanticCatalogNodeV1, SemanticCatalogRecordV1, decode_semantic_catalog_node,
  decode_semantic_definition_record, encode_semantic_catalog_internal, encode_semantic_catalog_leaf, encode_semantic_definition_object,
};
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
  let kv_block_length = initial_block_size();
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
fn immutable_semantic_objects_publish_canonically_and_refuse_invalid_batches_without_authority_change() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (directory, coordinator, publisher) = initialized_publisher(algorithm);
    let before = publisher.observe().unwrap();
    let semantic = encode_semantic_state_object(
      &SemanticStateWriteV1 {
        required_capabilities: [0; 32],
        availability: SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyDependencyCannotBeProven },
      },
      algorithm,
    )
    .unwrap();
    let request = ImmutableSemanticObjectBatchPublicationRequestV1 {
      database_id: &before.selected.header.database_id,
      objects: std::slice::from_ref(&semantic),
      publication_timestamp_ms: before.selected.header.updated_at_ms + 1,
    };

    let receipt = publisher.publish_immutable_semantic_objects(request).unwrap();

    assert!(!receipt.idempotent);
    assert_eq!(receipt.entities.len(), 2);
    assert!(receipt.entities.iter().all(|entity| !entity.idempotent));
    assert_eq!(receipt.observation.selected.header.head_hash, before.selected.header.head_hash);
    assert_eq!(receipt.observation.selected.header.entry_count, before.selected.header.entry_count + 2);
    assert_eq!(receipt.observation.selected.header.write_sequence_high_water, before.selected.header.write_sequence_high_water + 2);
    let selected_after_publication = receipt.observation.clone();
    let hard_frontier = coordinator.snapshot().unwrap().hard_frontier;

    let retry = publisher.publish_immutable_semantic_objects(request).unwrap();
    assert!(retry.idempotent);
    assert!(retry.entities.iter().all(|entity| entity.idempotent));
    assert_eq!(retry.observation, selected_after_publication);
    assert_eq!(coordinator.snapshot().unwrap().hard_frontier, hard_frontier);

    let timestamp_independent_retry = publisher
      .publish_immutable_semantic_objects(ImmutableSemanticObjectBatchPublicationRequestV1 {
        publication_timestamp_ms: request.publication_timestamp_ms + 1,
        ..request
      })
      .unwrap();
    assert!(timestamp_independent_retry.idempotent);
    assert!(timestamp_independent_retry.entities.iter().all(|entity| entity.idempotent));
    assert_eq!(timestamp_independent_retry.observation, selected_after_publication);
    assert_eq!(coordinator.snapshot().unwrap().hard_frontier, hard_frontier);

    let baseline = publisher.observe().unwrap();
    let wrong_database = [0x91; 16];
    assert_eq!(
      publisher
        .publish_immutable_semantic_objects(ImmutableSemanticObjectBatchPublicationRequestV1 { database_id: &wrong_database, ..request })
        .unwrap_err()
        .code(),
      "immutable_semantic_object_database_mismatch"
    );
    assert_eq!(
      publisher
        .publish_immutable_semantic_objects(ImmutableSemanticObjectBatchPublicationRequestV1 { objects: &[], ..request })
        .unwrap_err()
        .code(),
      "immutable_semantic_object_count"
    );
    let oversized_count = vec![semantic.clone(); 2_049];
    assert_eq!(
      publisher
        .publish_immutable_semantic_objects(ImmutableSemanticObjectBatchPublicationRequestV1 { objects: &oversized_count, ..request })
        .unwrap_err()
        .code(),
      "immutable_semantic_object_count"
    );
    let duplicates = [semantic.clone(), semantic.clone()];
    assert_eq!(
      publisher
        .publish_immutable_semantic_objects(ImmutableSemanticObjectBatchPublicationRequestV1 { objects: &duplicates, ..request })
        .unwrap_err()
        .code(),
      "immutable_semantic_object_duplicate"
    );
    let mut wrong_identity = semantic.clone();
    wrong_identity.object_id[0] ^= 0xff;
    assert_eq!(
      publisher
        .publish_immutable_semantic_objects(ImmutableSemanticObjectBatchPublicationRequestV1 {
          objects: std::slice::from_ref(&wrong_identity),
          ..request
        })
        .unwrap_err()
        .code(),
      "immutable_semantic_object_identity"
    );
    let mut malformed = semantic.clone();
    *malformed.value.last_mut().unwrap() ^= 0xff;
    assert!(publisher
      .publish_immutable_semantic_objects(ImmutableSemanticObjectBatchPublicationRequestV1 {
        objects: std::slice::from_ref(&malformed),
        ..request
      })
      .is_err());
    assert_eq!(
      publisher
        .publish_immutable_semantic_objects(ImmutableSemanticObjectBatchPublicationRequestV1 { publication_timestamp_ms: 0, ..request })
        .unwrap_err()
        .code(),
      "immutable_semantic_object_publication_time"
    );
    assert_eq!(publisher.observe().unwrap(), baseline);

    let path = directory.path().join("migration-execution.aeordb");
    drop(publisher);
    drop(coordinator);
    let reopened = V4FirstAuthorityPublisher::open(&path).unwrap();
    let reopened_receipt = reopened.publish_immutable_semantic_objects(request).unwrap();
    assert!(reopened_receipt.idempotent);
    assert_eq!(reopened_receipt.observation, selected_after_publication);
  }
}

#[test]
fn encoded_catalog_nodes_publish_and_reopen_through_the_existing_authority_without_selecting_a_root() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let (directory, coordinator, publisher) = initialized_publisher(algorithm);
    let before = publisher.observe().unwrap();
    let definitions: Vec<_> = ["parser", "mapper"]
      .iter()
      .map(|role| {
        let path =
          format!("{}/spec/fixtures/v4/semantic-object-v1/asem-{profile}-wasm-{role}-definition-valid.bin", env!("CARGO_MANIFEST_DIR"));
        let value = std::fs::read(path).unwrap();
        let object_id = decode_semantic_definition_record(&value, algorithm).unwrap().object_id;
        EncodedSemanticObjectV1 { object_id, value }
      })
      .collect();
    let leaves: Vec<_> = definitions
      .iter()
      .map(|definition| {
        let decoded = decode_semantic_definition_record(&definition.value, algorithm).unwrap();
        encode_semantic_catalog_leaf(
          &[SemanticCatalogRecordV1 {
            record_kind: 6,
            semantic_id: decoded.semantic_id,
            definition_object_id: &definition.object_id,
            owner_key: decoded.semantic_id,
          }],
          algorithm,
        )
        .unwrap()
      })
      .collect();
    let lookups: Vec<_> = leaves
      .iter()
      .map(|leaf| {
        let SemanticCatalogNodeV1::Leaf(decoded) = decode_semantic_catalog_node(&leaf.value, algorithm).unwrap() else {
          panic!("expected leaf");
        };
        decoded.lookup_digest().to_vec()
      })
      .collect();
    let common = lookups[0].iter().zip(&lookups[1]).take_while(|(left, right)| left == right).count();
    assert!(common < algorithm.hash_length());
    let mut children = [
      SemanticCatalogChildV1 { edge: lookups[0][common], record_count: 1, object_id: &leaves[0].object_id },
      SemanticCatalogChildV1 { edge: lookups[1][common], record_count: 1, object_id: &leaves[1].object_id },
    ];
    children.sort_by_key(|child| child.edge);
    let internal = encode_semantic_catalog_internal(0, &lookups[0][..common], &children, algorithm).unwrap();
    let publish = |objects: &[EncodedSemanticObjectV1]| {
      publisher
        .publish_immutable_semantic_objects(ImmutableSemanticObjectBatchPublicationRequestV1 {
          database_id: &before.selected.header.database_id,
          objects,
          publication_timestamp_ms: before.selected.header.updated_at_ms + 1,
        })
        .unwrap()
    };
    // Dependency-first pages use the real file/KV/header publication owner.
    // This does not advertise executable availability or activate semantics.
    assert!(!publish(&definitions).idempotent);
    assert!(!publish(&leaves).idempotent);
    assert!(!publish(std::slice::from_ref(&internal)).idempotent);
    let mut objects = definitions;
    objects.extend(leaves);
    objects.push(internal);
    let selected = publisher.observe().unwrap();
    assert_eq!(selected.selected.header.head_hash, before.selected.header.head_hash);
    let frontier = coordinator.snapshot().unwrap().hard_frontier;
    assert!(publish(&objects).idempotent);
    assert_eq!(publisher.observe().unwrap(), selected);
    assert_eq!(coordinator.snapshot().unwrap().hard_frontier, frontier);
    let mut malformed = objects.last().unwrap().clone();
    malformed.value[32] ^= 1;
    assert!(publisher
      .publish_immutable_semantic_objects(ImmutableSemanticObjectBatchPublicationRequestV1 {
        database_id: &before.selected.header.database_id,
        objects: &[malformed],
        publication_timestamp_ms: before.selected.header.updated_at_ms + 1,
      })
      .is_err());
    assert_eq!(publisher.observe().unwrap(), selected);
    drop(publisher);
    drop(coordinator);
    let reopened = V4FirstAuthorityPublisher::open(directory.path().join("migration-execution.aeordb")).unwrap();
    assert_eq!(reopened.observe().unwrap(), selected);
    for object in &objects {
      let kind = u16::from_le_bytes([object.value[6], object.value[7]]);
      assert_eq!(reopened.load_semantic_object(kind, &object.object_id).unwrap().as_deref(), Some(object.value.as_slice()));
    }
    assert!(
      reopened
        .publish_immutable_semantic_objects(ImmutableSemanticObjectBatchPublicationRequestV1 {
          database_id: &before.selected.header.database_id,
          objects: &objects,
          publication_timestamp_ms: before.selected.header.updated_at_ms + 2,
        })
        .unwrap()
        .idempotent
    );
    assert_eq!(reopened.observe().unwrap(), selected);
  }
}

#[test]
fn validated_definition_objects_publish_and_reopen_for_all_classes_and_registered_hashes() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (directory, coordinator, publisher) = initialized_publisher(algorithm);
    let before = publisher.observe().unwrap();
    let profile = if algorithm.hash_length() == 32 { "blake3-256" } else { "sha512" };
    let objects: Vec<_> = (1..=7)
      .map(|class| {
        let payload = if class <= 2 {
          // Structural projection payload only; no semantic compilation or root
          // activation is claimed by this physical-object storage test.
          vec![0x0a, 4, 0, 0, 0, 0, 0, 0, 0]
        } else {
          let name = match class {
            3 => format!("scope-definition-v1/ascp-{profile}-root-direct-valid.bin"),
            4 => format!("value-store-definition-v1/avst-{profile}-metadata-hash-corrected-valid.bin"),
            5 => format!("field-index-definition-v1/afix-{profile}-bool_order_v1-valid.bin"),
            6 => format!("semantic-object-v1/asem-{profile}-wasm-parser-definition-valid.bin"),
            7 => format!("semantic-object-v1/asem-{profile}-native-dependency-definition-valid.bin"),
            _ => unreachable!(),
          };
          let bytes = std::fs::read(format!("{}/spec/fixtures/v4/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap();
          if class <= 5 {
            bytes
          } else {
            bytes[48 + algorithm.hash_length()..bytes.len() - 4].to_vec()
          }
        };
        let encoded = encode_semantic_definition_object(class, &payload, algorithm).unwrap();
        assert_eq!(encoded.semantic_id.len(), algorithm.hash_length());
        assert_ne!(encoded.semantic_id, encoded.object.object_id);
        encoded.object
      })
      .collect();
    let request = ImmutableSemanticObjectBatchPublicationRequestV1 {
      database_id: &before.selected.header.database_id,
      objects: &objects,
      publication_timestamp_ms: before.selected.header.updated_at_ms + 1,
    };
    let receipt = publisher.publish_immutable_semantic_objects(request).unwrap();
    assert!(!receipt.idempotent);
    assert_eq!(receipt.entities.len(), 14);
    assert_eq!(receipt.observation.selected.header.head_hash, before.selected.header.head_hash);
    let selected = publisher.observe().unwrap();
    let frontier = coordinator.snapshot().unwrap().hard_frontier;
    assert!(publisher.publish_immutable_semantic_objects(request).unwrap().idempotent);
    assert_eq!(publisher.observe().unwrap(), selected);
    assert_eq!(coordinator.snapshot().unwrap().hard_frontier, frontier);
    let mut malformed = objects[0].clone();
    malformed.value[34] = 2;
    assert!(publisher
      .publish_immutable_semantic_objects(ImmutableSemanticObjectBatchPublicationRequestV1 { objects: &[malformed], ..request })
      .is_err());
    assert_eq!(publisher.observe().unwrap(), selected);
    drop(publisher);
    drop(coordinator);
    let reopened = V4FirstAuthorityPublisher::open(directory.path().join("migration-execution.aeordb")).unwrap();
    for (index, object) in objects.iter().enumerate() {
      let loaded = reopened.load_semantic_object(4, &object.object_id).unwrap().unwrap();
      assert_eq!(loaded, object.value);
      assert_eq!(decode_semantic_definition_record(&loaded, algorithm).unwrap().class, index as u16 + 1);
    }
    assert!(reopened.publish_immutable_semantic_objects(request).unwrap().idempotent);
    assert_eq!(reopened.observe().unwrap(), selected);
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
fn compiled_parser_registry_pins_publish_and_reopen_without_selecting_new_authority() {
  use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
  use aeordb::engine::v4::dependency::{DependencyRecordV1, decode_dependency_record_bytes};
  use aeordb::engine::v4::parser_registry_compiler::{
    ParserAliasSnapshotV1, ParserRegistryCompilationRequestV1, SemanticCompilationErrorV1, compile_parser_registry_v1,
  };

  struct Snapshot(Vec<u8>);
  impl ParserAliasSnapshotV1 for Snapshot {
    fn resolve_parser_alias(&self, _alias: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
      Ok(Some(decode_dependency_record_bytes(&self.0).unwrap()))
    }
  }

  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (directory, coordinator, publisher) = initialized_publisher(algorithm);
    let before = publisher.observe().unwrap();
    let profile = if algorithm.hash_length() == 32 { "blake3-256" } else { "sha512" };
    let fixture = std::fs::read(format!(
      "{}/spec/fixtures/v4/semantic-object-v1/asem-{profile}-wasm-parser-definition-valid.bin",
      env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let snapshot = Snapshot(fixture[48 + algorithm.hash_length()..fixture.len() - 4].to_vec());
    let memory = MemoryCoordinator::new(MemoryPolicy::new(96 * 1024 * 1024, 128 * 1024 * 1024, 32 * 1024 * 1024, 8 * 1024 * 1024).unwrap());
    let request = ParserRegistryCompilationRequestV1 {
      source: Some(br#"{"$v":1,"parsers":{"TEXT/PLAIN":"captured"}}"#),
      hash_algorithm: algorithm,
      maximum_source_bytes: 4096,
      maximum_workspace_bytes: 16 * 1024 * 1024,
    };
    let compiled = compile_parser_registry_v1(request, &snapshot, &memory, &|| false).unwrap();
    let dependency = encode_semantic_definition_object(6, compiled.entries()[0].dependency_bytes(), algorithm).unwrap();
    let projection = compiled.projection();
    let leaf = encode_semantic_catalog_leaf(
      &[SemanticCatalogRecordV1 {
        record_kind: 2,
        owner_key: b"\x02\x00/.aeordb-config/parsers.json",
        semantic_id: &projection.semantic_id,
        definition_object_id: &projection.object.object_id,
      }],
      algorithm,
    )
    .unwrap();
    let objects = [dependency.object, projection.object.clone(), leaf];
    let publication = ImmutableSemanticObjectBatchPublicationRequestV1 {
      database_id: &before.selected.header.database_id,
      objects: &objects,
      publication_timestamp_ms: before.selected.header.updated_at_ms + 1,
    };
    assert!(!publisher.publish_immutable_semantic_objects(publication).unwrap().idempotent);
    let selected = publisher.observe().unwrap();
    assert_eq!(selected.selected.header.head_hash, before.selected.header.head_hash);
    assert!(publisher.publish_immutable_semantic_objects(publication).unwrap().idempotent);
    let equivalent = compile_parser_registry_v1(
      ParserRegistryCompilationRequestV1 { source: Some(br#"{ "parsers": {"text/plain":"renamed"}, "$v":1 }"#), ..request },
      &snapshot,
      &memory,
      &|| false,
    )
    .unwrap();
    assert_eq!(equivalent.projection(), projection);
    assert!(compile_parser_registry_v1(
      ParserRegistryCompilationRequestV1 { source: Some(br#"{"$v":1,"parsers":null}"#), ..request },
      &snapshot,
      &memory,
      &|| false,
    )
    .is_err());
    assert_eq!(publisher.observe().unwrap(), selected);
    drop(publisher);
    drop(coordinator);
    let reopened = V4FirstAuthorityPublisher::open(directory.path().join("migration-execution.aeordb")).unwrap();
    assert_eq!(reopened.observe().unwrap(), selected);
    for object in &objects {
      let kind = u16::from_le_bytes([object.value[6], object.value[7]]);
      assert_eq!(reopened.load_semantic_object(kind, &object.object_id).unwrap().as_deref(), Some(object.value.as_slice()));
    }
    assert!(reopened.publish_immutable_semantic_objects(publication).unwrap().idempotent);
    assert_eq!(reopened.observe().unwrap(), selected);
  }
}

#[test]
fn compiled_parser_context_definitions_publish_and_reopen_without_selecting_new_authority() {
  use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
  use aeordb::engine::v4::dependency::{decode_dependency_record_bytes, encode_dependency_record};
  use aeordb::engine::v4::parser_context_compiler::{
    ParserContextCompilationRequestV1, ParserContextSourceV1, ParserSelectorDependencyV1, compile_parser_context_v1,
  };
  use aeordb::engine::v4::parser_plan::decode_parser_resolution_plan;
  use aeordb::engine::v4::scope::{ScopeDefinitionWriteV1, ScopeMatchingMode, encode_scope_definition};
  use aeordb::engine::v4::source_selector::{SourceSelectorWriteV1, encode_source_selector};
  use aeordb::engine::v4::value_store::{
    ValueStoreDefinitionWriteV1, ValueStoreSemanticFamily, decode_value_store_definition, encode_value_store_definition,
  };

  let fixtures = format!("{}/spec/fixtures/v4", env!("CARGO_MANIFEST_DIR"));
  let dependency_bytes = std::fs::read(format!("{fixtures}/semantic-object-v1/asem-blake3-256-wasm-parser-definition-valid.bin")).unwrap();
  let parser = decode_dependency_record_bytes(&dependency_bytes[80..dependency_bytes.len() - 4]).unwrap();
  let mut mapper = parser.clone();
  mapper.role = 2;
  mapper.abi = 4;
  let program = std::fs::read(format!("{fixtures}/parser-resolution-plan-v1/aprp-blake3-256-explicit-plugin-valid.bin")).unwrap();
  let mut policy = decode_parser_resolution_plan(&program).unwrap().candidates[0].policy.clone();
  policy.max_fuel = 10_000_000;
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (directory, coordinator, publisher) = initialized_publisher(algorithm);
    let before = publisher.observe().unwrap();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(96 << 20, 128 << 20, 32 << 20, 8 << 20).unwrap());
    let request = ParserContextCompilationRequestV1 {
      source: ParserContextSourceV1::Explicit { dependency: parser.clone(), policy: &policy },
      selector_dependency: ParserSelectorDependencyV1::Mapper(mapper.clone()),
      maximum_workspace_bytes: 16 << 20,
    };
    let context = compile_parser_context_v1(request.clone(), &memory, &|| false).unwrap();
    let scope = encode_scope_definition(
      ScopeDefinitionWriteV1 { mode: ScopeMatchingMode::DirectChildren, owner_path: "/documents", glob: None },
      algorithm,
    )
    .unwrap();
    let selector = encode_source_selector(SourceSelectorWriteV1::PluginMapper {
      dependency_ordinal: context.selector_dependency_ordinal().unwrap(),
      mapper_contract: 2,
      arguments: &[1, 0, 0, 0, 0], // Independent canonical Null frame, the omitted-args default.
      policy: &policy,
    })
    .unwrap();
    let value_request = ValueStoreDefinitionWriteV1 {
      scope_id: &scope.scope_id,
      field_name: "title",
      semantic_family: ValueStoreSemanticFamily::CorrectedV1,
      max_source_values_per_document: 1024,
      max_canonical_source_bytes_per_document: 8 << 20,
      max_document_input_bytes: 64 << 20,
      max_selector_work_items_per_document: 0,
      max_selector_examined_bytes_per_document: 0,
      selector: &selector,
      parser_plan: context.parser_plan(),
      dependencies: context.dependencies(),
    };
    for (work, examined) in [(1_000_000, 0), (0, 64 << 20), (1_000_000, 64 << 20)] {
      let invalid = ValueStoreDefinitionWriteV1 {
        max_selector_work_items_per_document: work,
        max_selector_examined_bytes_per_document: examined,
        ..value_request
      };
      assert_eq!(encode_value_store_definition(invalid, algorithm).unwrap_err().code(), "value_store_closure");
      assert_eq!(publisher.observe().unwrap(), before);
    }
    let value = encode_value_store_definition(value_request, algorithm).unwrap();
    let definitions = [
      (3, encode_semantic_definition_object(3, &scope.value, algorithm).unwrap()),
      (4, encode_semantic_definition_object(4, &value.value, algorithm).unwrap()),
      (6, encode_semantic_definition_object(6, &encode_dependency_record(&parser).unwrap(), algorithm).unwrap()),
      (6, encode_semantic_definition_object(6, &encode_dependency_record(&mapper).unwrap(), algorithm).unwrap()),
    ];
    let records: Vec<_> = definitions
      .iter()
      .map(|(kind, definition)| SemanticCatalogRecordV1 {
        record_kind: *kind,
        owner_key: &definition.semantic_id,
        semantic_id: &definition.semantic_id,
        definition_object_id: &definition.object.object_id,
      })
      .collect();
    // Each noncolliding key has its own leaf. This stages definitions and leaf
    // bindings, not a complete catalog or a selected namespace root.
    let mut objects = vec![
      definitions[2].1.object.clone(),
      definitions[3].1.object.clone(),
      definitions[0].1.object.clone(),
      definitions[1].1.object.clone(),
    ];
    for record in records {
      objects.push(encode_semantic_catalog_leaf(&[record], algorithm).unwrap());
    }
    let publication = ImmutableSemanticObjectBatchPublicationRequestV1 {
      database_id: &before.selected.header.database_id,
      objects: &objects,
      publication_timestamp_ms: before.selected.header.updated_at_ms + 1,
    };
    assert!(!publisher.publish_immutable_semantic_objects(publication).unwrap().idempotent);
    let selected = publisher.observe().unwrap();
    assert_eq!(selected.selected.header.head_hash, before.selected.header.head_hash);
    assert!(publisher.publish_immutable_semantic_objects(publication).unwrap().idempotent);
    assert!(compile_parser_context_v1(request, &memory, &|| true).is_err());
    assert_eq!(publisher.observe().unwrap(), selected);
    drop(context);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    drop(publisher);
    drop(coordinator);
    let reopened = V4FirstAuthorityPublisher::open(directory.path().join("migration-execution.aeordb")).unwrap();
    assert_eq!(reopened.observe().unwrap(), selected);
    for object in &objects {
      let kind = u16::from_le_bytes([object.value[6], object.value[7]]);
      assert_eq!(reopened.load_semantic_object(kind, &object.object_id).unwrap().as_deref(), Some(object.value.as_slice()));
    }
    let loaded = reopened.load_semantic_object(4, &definitions[1].1.object.object_id).unwrap().unwrap();
    let payload = decode_semantic_definition_record(&loaded, algorithm).unwrap();
    let decoded = decode_value_store_definition(payload.definition, algorithm).unwrap();
    assert_eq!(decoded.value_store_id, value.value_store_id);
    assert_eq!(decoded.dependencies.records, [parser.clone(), mapper.clone()]);
    assert!(reopened.publish_immutable_semantic_objects(publication).unwrap().idempotent);
    assert_eq!(reopened.observe().unwrap(), selected);
  }
}

#[test]
fn compiled_source_selectors_publish_complete_definition_objects_and_reopen() {
  use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
  use aeordb::engine::v4::dependency::{decode_dependency_record_bytes, decode_dependency_table, encode_dependency_record};
  use aeordb::engine::v4::parser_context_compiler::{
    ParserContextCompilationRequestV1, ParserContextSourceV1, ParserSelectorDependencyV1, compile_parser_context_v1,
  };
  use aeordb::engine::v4::parser_plan::decode_parser_resolution_plan;
  use aeordb::engine::v4::scope::{ScopeDefinitionWriteV1, ScopeMatchingMode, encode_scope_definition};
  use aeordb::engine::v4::source_selector_compiler::{SourceSelectorCompilationRequestV1, SourceSelectorInputV1, compile_source_selector_v1};
  use aeordb::engine::v4::value_store::{ValueStoreDefinitionWriteV1, ValueStoreSemanticFamily, encode_value_store_definition};

  let fixtures = format!("{}/spec/fixtures/v4", env!("CARGO_MANIFEST_DIR"));
  let dependency = std::fs::read(format!("{fixtures}/semantic-object-v1/asem-blake3-256-wasm-parser-definition-valid.bin")).unwrap();
  let parser = decode_dependency_record_bytes(&dependency[80..dependency.len() - 4]).unwrap();
  let program = std::fs::read(format!("{fixtures}/parser-resolution-plan-v1/aprp-blake3-256-explicit-plugin-valid.bin")).unwrap();
  let mut policy = decode_parser_resolution_plan(&program).unwrap().candidates[0].policy.clone();
  policy.max_fuel = 10_000_000;
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (directory, coordinator, publisher) = initialized_publisher(algorithm);
    let before = publisher.observe().unwrap();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(96 << 20, 128 << 20, 32 << 20, 8 << 20).unwrap());
    let context = compile_parser_context_v1(
      ParserContextCompilationRequestV1 {
        source: ParserContextSourceV1::Explicit { dependency: parser.clone(), policy: &policy },
        selector_dependency: ParserSelectorDependencyV1::JsonPath,
        maximum_workspace_bytes: 4 << 20,
      },
      &memory,
      &|| false,
    )
    .unwrap();
    let request = SourceSelectorCompilationRequestV1 {
      field_name: "/^title$/igg",
      source: SourceSelectorInputV1::JsonPath(None),
      maximum_source_bytes: 1 << 20,
      maximum_workspace_bytes: 64 << 20,
    };
    let selector = compile_source_selector_v1(request.clone(), &memory, &|| false).unwrap();
    let explicit = [serde_json::Value::String(request.field_name.into())];
    let same = compile_source_selector_v1(
      SourceSelectorCompilationRequestV1 { source: SourceSelectorInputV1::JsonPath(Some(&explicit)), ..request.clone() },
      &memory,
      &|| false,
    )
    .unwrap();
    assert_eq!(selector.selector(), same.selector());
    let scope = encode_scope_definition(
      ScopeDefinitionWriteV1 { mode: ScopeMatchingMode::DirectChildren, owner_path: "/documents", glob: None },
      algorithm,
    )
    .unwrap();
    let value = encode_value_store_definition(
      ValueStoreDefinitionWriteV1 {
        scope_id: &scope.scope_id,
        field_name: selector.field_name(),
        semantic_family: ValueStoreSemanticFamily::CorrectedV1,
        max_source_values_per_document: 1024,
        max_canonical_source_bytes_per_document: 8 << 20,
        max_document_input_bytes: 64 << 20,
        max_selector_work_items_per_document: 1_000_000,
        max_selector_examined_bytes_per_document: 64 << 20,
        selector: selector.selector(),
        parser_plan: context.parser_plan(),
        dependencies: context.dependencies(),
      },
      algorithm,
    )
    .unwrap();
    let mut definitions = Vec::new();
    for dependency in decode_dependency_table(context.dependencies()).unwrap().records {
      let class = if dependency.kind == 1 { 6 } else { 7 };
      definitions
        .push((class, encode_semantic_definition_object(class, &encode_dependency_record(&dependency).unwrap(), algorithm).unwrap()));
    }
    definitions.push((3, encode_semantic_definition_object(3, &scope.value, algorithm).unwrap()));
    definitions.push((4, encode_semantic_definition_object(4, &value.value, algorithm).unwrap()));
    let mut objects = Vec::new();
    for (_, definition) in &definitions {
      objects.push(definition.object.clone());
    }
    for (class, definition) in &definitions {
      objects.push(
        encode_semantic_catalog_leaf(
          &[SemanticCatalogRecordV1 {
            record_kind: *class,
            owner_key: &definition.semantic_id,
            semantic_id: &definition.semantic_id,
            definition_object_id: &definition.object.object_id,
          }],
          algorithm,
        )
        .unwrap(),
      );
    }
    let publication = ImmutableSemanticObjectBatchPublicationRequestV1 {
      database_id: &before.selected.header.database_id,
      objects: &objects,
      publication_timestamp_ms: before.selected.header.updated_at_ms + 1,
    };
    assert!(!publisher.publish_immutable_semantic_objects(publication).unwrap().idempotent);
    let selected = publisher.observe().unwrap();
    assert_eq!(selected.selected.header.head_hash, before.selected.header.head_hash);
    assert!(compile_source_selector_v1(request, &memory, &|| true).is_err());
    assert_eq!(publisher.observe().unwrap(), selected);
    drop(publisher);
    drop(coordinator);
    let reopened = V4FirstAuthorityPublisher::open(directory.path().join("migration-execution.aeordb")).unwrap();
    for object in &objects {
      let kind = u16::from_le_bytes([object.value[6], object.value[7]]);
      assert_eq!(reopened.load_semantic_object(kind, &object.object_id).unwrap().as_deref(), Some(object.value.as_slice()));
    }
    assert!(reopened.publish_immutable_semantic_objects(publication).unwrap().idempotent);
    assert_eq!(reopened.observe().unwrap(), selected);
  }
}

#[test]
fn compiled_default_index_definitions_publish_and_reopen_without_selecting_a_new_root() {
  use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
  use aeordb::engine::v4::database_header::SelectedDatabaseHeaderV4;
  use aeordb::engine::v4::index_definition_compiler::{
    ConverterDefinitionLimitsInputV1, CorrectedIndexInputV1, FieldDefinitionLimitsInputV1, IndexDefinitionCompilationRequestV1,
    SourceDefinitionLimitsInputV1, compile_index_definitions_v1, default_metadata_indexes_v1,
  };
  use aeordb::engine::v4::parser_context_compiler::{
    ParserContextCompilationRequestV1, ParserContextSourceV1, ParserSelectorDependencyV1, compile_parser_context_v1,
  };
  use aeordb::engine::v4::scope::{ScopeDefinitionWriteV1, ScopeMatchingMode, encode_scope_definition};
  use aeordb::engine::v4::semantic_catalog::{
    SemanticCatalogObjectSourceV1, SemanticCatalogReadErrorV1, SemanticCatalogReaderV1, SemanticCatalogTraversalBoundsV1,
  };
  use aeordb::engine::v4::semantic_catalog_mutation::{
    SemanticCatalogMutationRequestV1, SemanticCatalogMutationV1, SemanticCatalogSnapshotV1, plan_semantic_catalog_mutation_v1,
  };
  use aeordb::engine::v4::source_selector_compiler::{SourceSelectorCompilationRequestV1, SourceSelectorInputV1, compile_source_selector_v1};
  use tokio_util::sync::CancellationToken;

  struct CapturedObjects<'a> {
    publisher: &'a V4FirstAuthorityPublisher,
    header: &'a SelectedDatabaseHeaderV4,
    cancellation: &'a CancellationToken,
  }

  impl SemanticCatalogObjectSourceV1 for CapturedObjects<'_> {
    fn load_semantic_object(&self, kind: u16, identity: &[u8]) -> Result<Option<Vec<u8>>, SemanticCatalogReadErrorV1> {
      self
        .publisher
        .load_semantic_object_at_captured_header(self.header, kind, identity, self.cancellation)
        .map_err(|source| SemanticCatalogReadErrorV1::corrupt(source.code(), source.to_string()))
    }
  }

  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (directory, coordinator, publisher) = initialized_publisher(algorithm);
    let before = publisher.observe().unwrap();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(192 << 20, 256 << 20, 32 << 20, 16 << 20).unwrap());
    let scope =
      encode_scope_definition(ScopeDefinitionWriteV1 { mode: ScopeMatchingMode::DirectChildren, owner_path: "/", glob: None }, algorithm)
        .unwrap();
    let mut definitions = vec![(3, encode_semantic_definition_object(3, &scope.value, algorithm).unwrap())];
    for field in default_metadata_indexes_v1() {
      let context = compile_parser_context_v1(
        ParserContextCompilationRequestV1 {
          source: ParserContextSourceV1::Metadata,
          selector_dependency: ParserSelectorDependencyV1::None,
          maximum_workspace_bytes: 64 << 20,
        },
        &memory,
        &|| false,
      )
      .unwrap();
      let source = compile_source_selector_v1(
        SourceSelectorCompilationRequestV1 {
          field_name: field.field_name,
          source: SourceSelectorInputV1::Metadata,
          maximum_source_bytes: 1 << 20,
          maximum_workspace_bytes: 64 << 20,
        },
        &memory,
        &|| false,
      )
      .unwrap();
      let indexes: Vec<_> = field
        .converter_ids
        .iter()
        .map(|converter_id| CorrectedIndexInputV1 {
          converter_id: *converter_id,
          converter_limits: ConverterDefinitionLimitsInputV1::default(),
          field_limits: FieldDefinitionLimitsInputV1::default(),
        })
        .collect();
      let request = IndexDefinitionCompilationRequestV1 {
        scope_id: &scope.scope_id,
        source: &source,
        parser_context: &context,
        source_limits: SourceDefinitionLimitsInputV1::default(),
        indexes: &indexes,
        hash_algorithm: algorithm,
        maximum_workspace_bytes: 64 << 20,
      };
      assert!(compile_index_definitions_v1(request.clone(), &memory, &|| true).is_err());
      assert_eq!(publisher.observe().unwrap(), before);
      let compiled = compile_index_definitions_v1(request, &memory, &|| false).unwrap();
      definitions.push((4, encode_semantic_definition_object(4, &compiled.value_store().value, algorithm).unwrap()));
      for index in compiled.field_indexes() {
        definitions.push((5, encode_semantic_definition_object(5, &index.value, algorithm).unwrap()));
      }
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    assert_eq!(definitions.iter().filter(|(class, _)| *class == 4).count(), 8);
    assert_eq!(definitions.iter().filter(|(class, _)| *class == 5).count(), 13);
    definitions.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.semantic_id.cmp(&right.1.semantic_id)));
    let records: Vec<_> = definitions
      .iter()
      .map(|(class, definition)| SemanticCatalogRecordV1 {
        record_kind: *class,
        owner_key: &definition.semantic_id,
        semantic_id: &definition.semantic_id,
        definition_object_id: &definition.object.object_id,
      })
      .collect();
    let mut objects: Vec<_> = definitions.iter().map(|(_, definition)| definition.object.clone()).collect();
    publisher
      .publish_immutable_semantic_objects(ImmutableSemanticObjectBatchPublicationRequestV1 {
        database_id: &before.selected.header.database_id,
        objects: &objects,
        publication_timestamp_ms: before.selected.header.updated_at_ms + 1,
      })
      .unwrap();
    let cancellation = CancellationToken::new();
    let mut root = None;
    let mut record_count = 0;
    let mut node_count = 0;
    for record in records {
      let observed = publisher.observe().unwrap();
      let source = CapturedObjects { publisher: &publisher, header: &observed.selected, cancellation: &cancellation };
      let plan = plan_semantic_catalog_mutation_v1(
        SemanticCatalogMutationRequestV1 {
          hash_algorithm: algorithm,
          snapshot: SemanticCatalogSnapshotV1 { root_object_id: root.as_deref(), record_count, node_count },
          mutation: SemanticCatalogMutationV1::Upsert(record),
          maximum_workspace_bytes: 32 << 20,
        },
        &source,
        &memory,
        &|| false,
      )
      .unwrap();
      let publication = ImmutableSemanticObjectBatchPublicationRequestV1 {
        database_id: &observed.selected.header.database_id,
        objects: plan.objects(),
        publication_timestamp_ms: observed.selected.header.updated_at_ms + 1,
      };
      assert!(!publisher.publish_immutable_semantic_objects(publication).unwrap().idempotent);
      assert!(publisher.publish_immutable_semantic_objects(publication).unwrap().idempotent);
      for object in plan.objects() {
        if !objects.iter().any(|existing| existing.object_id == object.object_id) {
          objects.push(object.clone());
        }
      }
      root = plan.root_object_id().map(<[u8]>::to_vec);
      record_count = plan.record_count();
      node_count = plan.node_count();
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    assert_eq!(record_count, 22);
    let after = publisher.observe().unwrap();
    assert_eq!(after.selected.header.head_hash, before.selected.header.head_hash);
    drop(publisher);
    drop(coordinator);
    let reopened = V4FirstAuthorityPublisher::open(directory.path().join("migration-execution.aeordb")).unwrap();
    for object in &objects {
      let kind = u16::from_le_bytes([object.value[6], object.value[7]]);
      assert_eq!(reopened.load_semantic_object(kind, &object.object_id).unwrap().as_deref(), Some(object.value.as_slice()));
    }
    let source = CapturedObjects { publisher: &reopened, header: &after.selected, cancellation: &cancellation };
    let reader = SemanticCatalogReaderV1::new(algorithm, &source);
    let walked = reader
      .walk_catalog(
        root.as_deref().unwrap(),
        SemanticCatalogTraversalBoundsV1::new(record_count, node_count).unwrap(),
        &|| false,
        |record| {
          reader.with_definition(record, &|| false, |payload| {
            let definition = definitions.iter().find(|(_, definition)| definition.semantic_id == record.semantic_id).unwrap();
            assert_eq!(definition.0, record.record_kind);
            assert_eq!(payload, &definition.1.object.value[32 + 16 + algorithm.hash_length()..definition.1.object.value.len() - 4]);
            Ok(())
          })
        },
      )
      .unwrap();
    assert_eq!(walked.class_counts, [0, 0, 0, 1, 8, 13, 0, 0]);
    assert_eq!(walked.nodes, node_count);
    assert!(
      reopened
        .publish_immutable_semantic_objects(ImmutableSemanticObjectBatchPublicationRequestV1 {
          database_id: &after.selected.header.database_id,
          objects: &objects,
          publication_timestamp_ms: after.selected.header.updated_at_ms + 1,
        })
        .unwrap()
        .idempotent
    );
    assert_eq!(reopened.observe().unwrap(), after);
  }
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
fn complete_configuration_catalogs_publish_reopen_and_preserve_all_seven_classes_without_root_selection() {
  use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
  use aeordb::engine::v4::database_header::SelectedDatabaseHeaderV4;
  use aeordb::engine::v4::dependency::DependencyRecordV1;
  use aeordb::engine::v4::index_configuration_compiler::{
    IndexConfigurationAliasSnapshotV1, IndexConfigurationCompilationRequestV1, compile_index_configuration_v1,
    default_index_configuration_v1,
  };
  use aeordb::engine::v4::parser_registry_compiler::{
    ParserAliasSnapshotV1, ParserRegistryCompilationRequestV1, SemanticCompilationErrorV1, compile_parser_registry_v1,
  };
  use aeordb::engine::v4::semantic_catalog::{
    SemanticCatalogObjectSourceV1, SemanticCatalogReadErrorV1, SemanticCatalogReaderV1, SemanticCatalogTraversalBoundsV1,
  };
  use aeordb::engine::v4::semantic_catalog_mutation::{
    SemanticCatalogMutationRequestV1, SemanticCatalogMutationV1, SemanticCatalogSnapshotV1, plan_semantic_catalog_mutation_v1,
  };
  use tokio_util::sync::CancellationToken;

  struct Snapshot;
  impl ParserAliasSnapshotV1 for Snapshot {
    fn resolve_parser_alias(&self, _: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
      Ok(Some(dependency(1)))
    }
  }
  impl IndexConfigurationAliasSnapshotV1 for Snapshot {
    fn resolve_mapper_alias(&self, _: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
      Ok(Some(dependency(2)))
    }
  }
  fn dependency(role: u16) -> DependencyRecordV1<'static> {
    DependencyRecordV1 {
      kind: 1,
      role,
      flags: 4,
      abi: role + 2,
      executor_profile: 2,
      fingerprint_semantics: 1,
      artifact_kind: 1,
      artifact_length: 123,
      fingerprint: [0x42; 32],
      dependency_id: "/org/example/shared",
      version: "1.2.3",
    }
  }
  struct CapturedObjects<'a> {
    publisher: &'a V4FirstAuthorityPublisher,
    header: &'a SelectedDatabaseHeaderV4,
    cancellation: &'a CancellationToken,
  }
  impl SemanticCatalogObjectSourceV1 for CapturedObjects<'_> {
    fn load_semantic_object(&self, kind: u16, identity: &[u8]) -> Result<Option<Vec<u8>>, SemanticCatalogReadErrorV1> {
      self
        .publisher
        .load_semantic_object_at_captured_header(self.header, kind, identity, self.cancellation)
        .map_err(|source| SemanticCatalogReadErrorV1::corrupt(source.code(), source.to_string()))
    }
  }
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    for (source, counts) in [
      (default_index_configuration_v1(), [0, 1, 1, 1, 12, 18, 0, 4]),
      (
        br#"{"$v":1,"parser":"p","indexes":[{"name":"x","type":"typed_exact_blake3_v1","source":{"plugin":"m"}}]}"#.as_slice(),
        [0, 1, 1, 1, 1, 1, 2, 0],
      ),
      (br#"{"$v":1,"indexes":[]}"#.as_slice(), [0, 1, 1, 1, 0, 0, 0, 0]),
    ] {
      let (directory, coordinator, publisher) = initialized_publisher(algorithm);
      let before = publisher.observe().unwrap();
      let memory = MemoryCoordinator::new(MemoryPolicy::new(192 << 20, 256 << 20, 32 << 20, 16 << 20).unwrap());
      let registry = compile_parser_registry_v1(
        ParserRegistryCompilationRequestV1 {
          source: None,
          hash_algorithm: algorithm,
          maximum_source_bytes: 1 << 20,
          maximum_workspace_bytes: 64 << 20,
        },
        &Snapshot,
        &memory,
        &|| false,
      )
      .unwrap();
      let request = IndexConfigurationCompilationRequestV1 {
        source,
        owner_path: "/",
        registry: &registry,
        hash_algorithm: algorithm,
        maximum_source_bytes: 1 << 20,
        maximum_workspace_bytes: 128 << 20,
      };
      assert!(compile_index_configuration_v1(request, &Snapshot, &memory, &|| true).is_err());
      assert_eq!(publisher.observe().unwrap(), before);
      let compiled = compile_index_configuration_v1(request, &Snapshot, &memory, &|| false).unwrap();
      let mut definitions = vec![
        (1, compiled.projection().clone()),
        (2, registry.projection().clone()),
        (3, encode_semantic_definition_object(3, &compiled.scope().value, algorithm).unwrap()),
      ];
      for field in compiled.fields() {
        definitions.push((4, encode_semantic_definition_object(4, &field.value_store().value, algorithm).unwrap()));
        for index in field.field_indexes() {
          definitions.push((5, encode_semantic_definition_object(5, &index.value, algorithm).unwrap()));
        }
      }
      for dependency in compiled.dependencies() {
        let class = u16::from_le_bytes(dependency.object.value[32..34].try_into().unwrap());
        definitions.push((class, dependency.clone()));
      }
      drop(compiled);
      drop(registry);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
      let owners: Vec<_> = definitions
        .iter()
        .map(|(class, definition)| match class {
          1 => [1u16.to_le_bytes().as_slice(), b"/.aeordb-config/indexes.json"].concat(),
          2 => [2u16.to_le_bytes().as_slice(), b"/.aeordb-config/parsers.json"].concat(),
          _ => definition.semantic_id.clone(),
        })
        .collect();
      let objects: Vec<_> = definitions.iter().map(|(_, definition)| definition.object.clone()).collect();
      let publication = ImmutableSemanticObjectBatchPublicationRequestV1 {
        database_id: &before.selected.header.database_id,
        objects: &objects,
        publication_timestamp_ms: before.selected.header.updated_at_ms + 1,
      };
      assert!(!publisher.publish_immutable_semantic_objects(publication).unwrap().idempotent);
      assert!(publisher.publish_immutable_semantic_objects(publication).unwrap().idempotent);
      let cancellation = CancellationToken::new();
      let (mut root, mut record_count, mut node_count) = (None, 0, 0);
      for ((class, definition), owner) in definitions.iter().zip(&owners) {
        let observed = publisher.observe().unwrap();
        let source = CapturedObjects { publisher: &publisher, header: &observed.selected, cancellation: &cancellation };
        let plan = plan_semantic_catalog_mutation_v1(
          SemanticCatalogMutationRequestV1 {
            hash_algorithm: algorithm,
            snapshot: SemanticCatalogSnapshotV1 { root_object_id: root.as_deref(), record_count, node_count },
            mutation: SemanticCatalogMutationV1::Upsert(SemanticCatalogRecordV1 {
              record_kind: *class,
              owner_key: owner,
              semantic_id: &definition.semantic_id,
              definition_object_id: &definition.object.object_id,
            }),
            maximum_workspace_bytes: 32 << 20,
          },
          &source,
          &memory,
          &|| false,
        )
        .unwrap();
        let publication = ImmutableSemanticObjectBatchPublicationRequestV1 {
          database_id: &observed.selected.header.database_id,
          objects: plan.objects(),
          publication_timestamp_ms: observed.selected.header.updated_at_ms + 1,
        };
        assert!(!publisher.publish_immutable_semantic_objects(publication).unwrap().idempotent);
        assert!(publisher.publish_immutable_semantic_objects(publication).unwrap().idempotent);
        root = plan.root_object_id().map(<[u8]>::to_vec);
        record_count = plan.record_count();
        node_count = plan.node_count();
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
      assert_eq!(record_count, counts.iter().sum::<u64>());
      let after = publisher.observe().unwrap();
      assert_eq!(after.selected.header.head_hash, before.selected.header.head_hash);
      drop(publisher);
      drop(coordinator);
      let reopened = V4FirstAuthorityPublisher::open(directory.path().join("migration-execution.aeordb")).unwrap();
      let source = CapturedObjects { publisher: &reopened, header: &after.selected, cancellation: &cancellation };
      let reader = SemanticCatalogReaderV1::new(algorithm, &source);
      let walked = reader
        .walk_catalog(
          root.as_deref().unwrap(),
          SemanticCatalogTraversalBoundsV1::new(record_count, node_count).unwrap(),
          &|| false,
          |record| {
            reader.with_definition(record, &|| false, |payload| {
              let (_, definition) = definitions
                .iter()
                .find(|(class, definition)| *class == record.record_kind && definition.semantic_id == record.semantic_id)
                .unwrap();
              assert_eq!(payload, &definition.object.value[48 + algorithm.hash_length()..definition.object.value.len() - 4]);
              Ok(())
            })
          },
        )
        .unwrap();
      assert_eq!(walked.class_counts, counts);
      assert_eq!(walked.nodes, node_count);
      assert!(reopened.publish_immutable_semantic_objects(publication).unwrap().idempotent);
      assert_eq!(reopened.observe().unwrap(), after);
    }
  }
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
fn exact_successor_retry_survives_a_later_non_head_header_publication() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _coordinator, publisher) = initialized_publisher(algorithm);
  let before = publisher.observe().unwrap();
  let request = successor_request(algorithm, &before.selected.header, 0x7a, before.selected.header.updated_at_ms + 1, "retry-history.txt");
  let successor = publisher.publish_successor_authority(&request).unwrap();
  let chunk = b"header history after successor";
  let chunk_key = digest_parts(algorithm, &[b"chunk:", chunk]);

  let later = publisher
    .publish_immutable_entity_batch(ImmutableEntityBatchPublicationRequestV1 {
      database_id: &before.selected.header.database_id,
      entities: &[ImmutableEntityWriteV1 {
        entity_version: 0,
        entry_type: EntryTypeV4::Chunk,
        flags: 0,
        key: &chunk_key,
        stored_value: chunk,
      }],
      publication_timestamp_ms: successor.observation.selected.header.updated_at_ms + 1,
    })
    .unwrap();
  assert!(later.observation.selected.header.slot_sequence > successor.observation.selected.header.slot_sequence);
  assert_eq!(later.observation.selected.header.head_hash, successor.namespace_root.root_hash);
  let selected_authority = publisher.load_selected_semantic_authority().unwrap();
  assert_eq!(selected_authority.selected_header_slot_sequence, successor.observation.selected.header.slot_sequence);
  assert_eq!(selected_authority.root_hash, successor.namespace_root.root_hash);

  let retry = publisher.publish_successor_authority(&request).unwrap();
  assert!(retry.idempotent);
  assert_eq!(retry.namespace_root, successor.namespace_root);
  assert_eq!(retry.publication_sequence, successor.publication_sequence);
  assert_eq!(retry.observation, later.observation);
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

#[test]
fn catalog_cow_nodes_publish_reopen_and_traverse_through_the_captured_physical_authority() {
  use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
  use aeordb::engine::v4::database_header::SelectedDatabaseHeaderV4;
  use aeordb::engine::v4::semantic_catalog::{
    SemanticCatalogObjectSourceV1, SemanticCatalogReadErrorV1, SemanticCatalogReaderV1, SemanticCatalogTraversalBoundsV1,
  };
  use aeordb::engine::v4::semantic_catalog_mutation::{
    SemanticCatalogMutationRequestV1, SemanticCatalogMutationV1, SemanticCatalogSnapshotV1, plan_semantic_catalog_mutation_v1,
  };
  use tokio_util::sync::CancellationToken;

  struct CapturedObjects<'a> {
    publisher: &'a V4FirstAuthorityPublisher,
    header: &'a SelectedDatabaseHeaderV4,
    cancellation: &'a CancellationToken,
  }

  impl SemanticCatalogObjectSourceV1 for CapturedObjects<'_> {
    fn load_semantic_object(&self, kind: u16, identity: &[u8]) -> Result<Option<Vec<u8>>, SemanticCatalogReadErrorV1> {
      self
        .publisher
        .load_semantic_object_at_captured_header(self.header, kind, identity, self.cancellation)
        .map_err(|source| SemanticCatalogReadErrorV1::corrupt(source.code(), source.to_string()))
    }
  }

  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (directory, coordinator, mut publisher) = initialized_publisher(algorithm);
    drop(coordinator);
    let initial = publisher.observe().unwrap();
    // One structural projection shared by12 owner paths: catalog binding count
    // is intentionally NOT a distinct-definition-object count. No compiler or
    // selected semantic-root activation is claimed by this staging test.
    let definition = encode_semantic_definition_object(2, &[0x0a, 4, 0, 0, 0, 0, 0, 0, 0], algorithm).unwrap();
    publisher
      .publish_immutable_semantic_objects(ImmutableSemanticObjectBatchPublicationRequestV1 {
        database_id: &initial.selected.header.database_id,
        objects: std::slice::from_ref(&definition.object),
        publication_timestamp_ms: initial.selected.header.updated_at_ms + 1,
      })
      .unwrap();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(96 * 1024 * 1024, 128 * 1024 * 1024, 32 * 1024 * 1024, 8 * 1024 * 1024).unwrap());
    let cancellation = CancellationToken::new();
    let mut root = None;
    let mut records = 0;
    let mut nodes = 0;
    for operation in 0..24 {
      let index = operation % 12;
      let owner = [b"\x02\x00".as_slice(), format!("/controls/{index}.json").as_bytes()].concat();
      let mutation = if operation < 12 {
        SemanticCatalogMutationV1::Upsert(SemanticCatalogRecordV1 {
          record_kind: 2,
          owner_key: &owner,
          semantic_id: &definition.semantic_id,
          definition_object_id: &definition.object.object_id,
        })
      } else {
        SemanticCatalogMutationV1::Remove { record_kind: 2, owner_key: &owner }
      };
      let observed = publisher.observe().unwrap();
      let source = CapturedObjects { publisher: &publisher, header: &observed.selected, cancellation: &cancellation };
      let plan = plan_semantic_catalog_mutation_v1(
        SemanticCatalogMutationRequestV1 {
          hash_algorithm: algorithm,
          snapshot: SemanticCatalogSnapshotV1 { root_object_id: root.as_deref(), record_count: records, node_count: nodes },
          mutation,
          maximum_workspace_bytes: 32 * 1024 * 1024,
        },
        &source,
        &memory,
        &|| false,
      )
      .unwrap();
      assert!(!plan.is_unchanged());
      if !plan.objects().is_empty() {
        let publication = ImmutableSemanticObjectBatchPublicationRequestV1 {
          database_id: &observed.selected.header.database_id,
          objects: plan.objects(),
          publication_timestamp_ms: observed.selected.header.updated_at_ms + 1,
        };
        publisher.publish_immutable_semantic_objects(publication).unwrap();
        let after = publisher.observe().unwrap();
        assert!(publisher.publish_immutable_semantic_objects(publication).unwrap().idempotent);
        assert_eq!(publisher.observe().unwrap(), after);
      }
      root = plan.root_object_id().map(<[u8]>::to_vec);
      records = plan.record_count();
      nodes = plan.node_count();
      drop(plan);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
      if operation % 3 == 2 {
        drop(publisher);
        publisher = V4FirstAuthorityPublisher::open(directory.path().join("migration-execution.aeordb")).unwrap();
      }
      let observed = publisher.observe().unwrap();
      assert_eq!(observed.selected.header.head_hash, initial.selected.header.head_hash);
      let source = CapturedObjects { publisher: &publisher, header: &observed.selected, cancellation: &cancellation };
      if let Some(identity) = &root {
        let reader = SemanticCatalogReaderV1::new(algorithm, &source);
        let walked = reader
          .walk_catalog(identity, SemanticCatalogTraversalBoundsV1::new(records, nodes).unwrap(), &|| false, |record| {
            assert_eq!(record.record_kind, 2);
            assert_eq!(record.definition_object_id, definition.object.object_id);
            reader.with_definition(record, &|| false, |payload| {
              assert_eq!(payload, &[0x0a, 4, 0, 0, 0, 0, 0, 0, 0]);
              Ok(())
            })
          })
          .unwrap();
        assert_eq!(walked.class_counts[2], records);
        assert_eq!(walked.nodes, nodes);
      }
    }
    assert!(root.is_none());
    assert_eq!((records, nodes), (0, 0));
  }
}

#[test]
fn compiled_catalog_stages_through_native_authority_reopens_and_never_selects_head() {
  use aeordb::engine::v4::config_value::{CanonicalValueBounds, borrow_canonical_value};
  use aeordb::engine::v4::namespace::decode_semantic_object;
  use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
  use aeordb::engine::v4::index_configuration_compiler::{
    IndexConfigurationAliasSnapshotV1, IndexConfigurationCompilationRequestV1, compile_index_configuration_v1,
  };
  use aeordb::engine::v4::parser_registry_compiler::{
    ParserAliasSnapshotV1, ParserRegistryCompilationRequestV1, SemanticCompilationErrorV1, compile_parser_registry_v1,
  };
  use aeordb::engine::v4::semantic_catalog::{SemanticCatalogReaderV1, SemanticCatalogTraversalBoundsV1};
  use aeordb::engine::v4::semantic_catalog_compiler::{SemanticCatalogCompilationRequestV1, compile_semantic_catalog_v1};
  use aeordb::engine::v4::semantic_catalog_native::NativeSemanticCatalogStagingStoreV1;
  use tokio_util::sync::CancellationToken;

  struct Snapshot;
  impl ParserAliasSnapshotV1 for Snapshot {
    fn resolve_parser_alias(
      &self,
      _: &str,
    ) -> Result<Option<aeordb::engine::v4::dependency::DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
      Ok(None)
    }
  }
  impl IndexConfigurationAliasSnapshotV1 for Snapshot {
    fn resolve_mapper_alias(
      &self,
      _: &str,
    ) -> Result<Option<aeordb::engine::v4::dependency::DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
      Ok(None)
    }
  }

  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (directory, coordinator, publisher) = initialized_publisher(algorithm);
    drop(coordinator);
    let before = publisher.observe().unwrap();
    let cancellation = CancellationToken::new();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 1, 32 << 20).unwrap());
    let registry = compile_parser_registry_v1(
      ParserRegistryCompilationRequestV1 {
        source: None,
        hash_algorithm: algorithm,
        maximum_source_bytes: 1 << 20,
        maximum_workspace_bytes: 64 << 20,
      },
      &Snapshot,
      &memory,
      &|| false,
    )
    .unwrap();
    let request = SemanticCatalogCompilationRequestV1 {
      hash_algorithm: algorithm,
      expected_configuration_count: 2,
      required_capabilities: [0; 32],
      maximum_workspace_bytes: 64 << 20,
    };
    let compile = |publisher: &V4FirstAuthorityPublisher| {
      let mut store = NativeSemanticCatalogStagingStoreV1::new(
        publisher,
        before.selected.header.database_id,
        before.selected.header.updated_at_ms + 1,
        &cancellation,
      )
      .unwrap();
      let inputs = ["/", "/nested"].into_iter().map(|owner_path| {
        compile_index_configuration_v1(
          IndexConfigurationCompilationRequestV1 {
            source: br#"{"$v":1,"indexes":[{"name":"value","type":"typed_exact_blake3_v1"}]}"#,
            owner_path,
            registry: &registry,
            hash_algorithm: algorithm,
            maximum_source_bytes: 1 << 20,
            maximum_workspace_bytes: 128 << 20,
          },
          &Snapshot,
          &memory,
          &|| false,
        )
      });
      compile_semantic_catalog_v1(request, &registry, inputs, &mut store, &memory, &|| cancellation.is_cancelled()).unwrap()
    };
    let result = compile(&publisher);
    let after = publisher.observe().unwrap();
    assert_eq!(after.selected.header.head_hash, before.selected.header.head_hash);
    assert_eq!(after.selected.header.nvt_length, before.selected.header.nvt_length);
    assert!(after.selected.header.write_sequence_high_water > before.selected.header.write_sequence_high_water);
    assert_eq!(compile(&publisher).semantic_state(), result.semantic_state());
    assert_eq!(publisher.observe().unwrap(), after, "idempotent staging changed physical authority");
    drop(publisher);
    let reopened = V4FirstAuthorityPublisher::open(directory.path().join("migration-execution.aeordb")).unwrap();
    assert_eq!(reopened.observe().unwrap(), after);
    assert_eq!(compile(&reopened).semantic_state(), result.semantic_state());
    assert_eq!(reopened.observe().unwrap(), after);
    assert_eq!(reopened.load_semantic_object(1, &result.semantic_state().object_id).unwrap().unwrap(), result.semantic_state().value);
    let state = decode_semantic_object(&result.semantic_state().value, algorithm).unwrap().semantic_state.unwrap();
    let SemanticAvailabilityV1::Complete {
      catalog_root, catalog_record_count, catalog_node_count, definition_count, dependency_count, ..
    } = state.availability
    else {
      panic!("complete catalog")
    };
    assert_eq!((catalog_record_count, definition_count, dependency_count), (13, 13, 4));
    let store = NativeSemanticCatalogStagingStoreV1::new(
      &reopened,
      before.selected.header.database_id,
      before.selected.header.updated_at_ms + 1,
      &cancellation,
    )
    .unwrap();
    let reader = SemanticCatalogReaderV1::new(algorithm, &store);
    let bounds = SemanticCatalogTraversalBoundsV1::new(catalog_record_count, catalog_node_count).unwrap();
    let stats = reader
      .walk_catalog(&catalog_root, bounds, &|| false, |record| {
        reader.with_definition(record, &|| false, |payload| {
          if record.record_kind == 1 {
            let value = borrow_canonical_value(payload, CanonicalValueBounds::CONFIG).unwrap();
            let mut entries = value.map_entries().unwrap();
            let (key, fields) = entries.next().unwrap().unwrap();
            assert_eq!(key, "fields");
            let mut fields = fields.map_entries().unwrap();
            let (_, field) = fields.next().unwrap().unwrap();
            assert!(fields.next().is_none());
            let mut members = field.map_entries().unwrap();
            let (key, indexes) = members.next().unwrap().unwrap();
            assert_eq!(key, "indexes");
            let mut indexes = indexes.array_entries().unwrap();
            let index = indexes.next().unwrap().unwrap();
            assert_eq!(index.as_bytes().unwrap().len(), record.semantic_id.len());
            assert!(indexes.next().is_none());
            let (key, value_store) = members.next().unwrap().unwrap();
            assert_eq!(key, "value_store_id");
            assert_eq!(value_store.as_bytes().unwrap().len(), record.semantic_id.len());
            assert!(members.next().is_none());
            let (key, scope) = entries.next().unwrap().unwrap();
            assert_eq!(key, "scope_id");
            assert_eq!(scope.as_bytes().unwrap().len(), record.semantic_id.len());
            assert!(entries.next().is_none());
          } else if record.record_kind == 2 {
            let value = borrow_canonical_value(payload, CanonicalValueBounds::CONFIG).unwrap();
            assert!(value.map_entries().unwrap().next().is_none());
          }
          Ok(())
        })?;
        let matched = reader.with_record(&catalog_root, bounds, record.record_kind, record.owner_key, &|| false, |candidate| {
          Ok(candidate.semantic_id == record.semantic_id && candidate.definition_object_id == record.definition_object_id)
        })?;
        assert_eq!(matched, Some(true), "physical point lookup disagrees with captured catalog traversal");
        Ok(())
      })
      .unwrap();
    assert_eq!(stats.class_counts, [0, 2, 1, 2, 2, 2, 0, 4]);
    assert_eq!(
      reader.with_record(&catalog_root, bounds, 1, b"\x01\x00/missing/.aeordb-config/indexes.json", &|| false, |_| Ok(())).unwrap(),
      None
    );
    assert!(NativeSemanticCatalogStagingStoreV1::new(&reopened, [0; 16], 1, &cancellation).is_err());
    assert!(NativeSemanticCatalogStagingStoreV1::new(&reopened, before.selected.header.database_id, 0, &cancellation).is_err());
    cancellation.cancel();
    assert!(NativeSemanticCatalogStagingStoreV1::new(&reopened, before.selected.header.database_id, 1, &cancellation).is_err());
    assert_eq!(reopened.observe().unwrap(), after);
    drop(result);
    drop(registry);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}
