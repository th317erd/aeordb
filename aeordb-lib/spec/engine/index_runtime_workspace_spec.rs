use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_coordinator::{IndexCoordinatorOptionsV1, IndexCoordinatorV1, IndexFlushReasonV1, IndexMutationRequestV1};
use aeordb::engine::v4::index_page::OrderedIndexRoleV1;
use aeordb::engine::v4::index_producer_coordinator::{IndexProducerTaskKindV1, IndexProducerTaskRequestV1};
use aeordb::engine::v4::index_record::{ScopeReverseRecordV1, encode_scope_reverse_record};
use aeordb::engine::v4::index_runtime_workspace::{
  IndexWorkspaceManifestWriteV1, IndexWorkspaceObjectKindV1, IndexWorkspaceObjectWriteV1, decode_index_workspace_manifest_v1,
  decode_index_workspace_object_v1, decode_index_workspace_producer_task_payload_v1, decode_index_workspace_runtime_batch_payload_v1,
  encode_index_workspace_manifest_v1, encode_index_workspace_object_v1, encode_index_workspace_producer_task_payload_v1,
  encode_index_workspace_runtime_batch_payload_v1,
};

const HASH_ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

#[test]
fn runtime_batch_payload_is_ordered_retry_stable_and_lossless() {
  let memory = MemoryCoordinator::new(MemoryPolicy::new(4_000_000, 5_000_000, 1, 1_000_000).unwrap());
  let options = IndexCoordinatorOptionsV1::new(1_000_000, 100, 1_000, 1_000_000).unwrap();
  let mut coordinator = IndexCoordinatorV1::new([0x77; 16], HASH_ALGORITHM, memory, options, 1_000).unwrap();
  let first_id = digest_parts(HASH_ALGORITHM, &[b"first-index"]);
  let second_id = digest_parts(HASH_ALGORITHM, &[b"second-index"]);
  let first_record = scope_reverse_record(3);
  let second_record = scope_reverse_record(7);
  coordinator
    .admit(
      IndexMutationRequestV1 {
        index_id: &second_id,
        role: OrderedIndexRoleV1::ScopeReverse,
        publication_sequence: 12,
        operation_id: [0x12; 16],
        encoded_record: &second_record,
      },
      1_001,
    )
    .unwrap();
  coordinator
    .admit(
      IndexMutationRequestV1 {
        index_id: &first_id,
        role: OrderedIndexRoleV1::ScopeReverse,
        publication_sequence: 11,
        operation_id: [0x11; 16],
        encoded_record: &first_record,
      },
      1_002,
    )
    .unwrap();
  let batch = coordinator.begin_flush(1_003, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
  let encoded = encode_index_workspace_runtime_batch_payload_v1(&batch, HASH_ALGORITHM).unwrap();
  let decoded = decode_index_workspace_runtime_batch_payload_v1(&encoded, HASH_ALGORITHM).unwrap();
  assert_eq!(decoded.coordinator_id, [0x77; 16]);
  assert_eq!(decoded.batch_id, batch.batch_id());
  assert_eq!(decoded.reason, IndexFlushReasonV1::Explicit);
  assert_eq!(decoded.records.len(), 2);
  assert!(decoded.records[0].index_id < decoded.records[1].index_id);
  assert_eq!(decoded.records[0].order_key, batch.records()[0].order_key());
  assert_eq!(decoded.records[1].encoded_record, batch.records()[1].encoded_record());

  let retry = coordinator.retry_frozen(false).unwrap();
  assert_ne!(batch.attempt_id(), retry.attempt_id());
  assert_eq!(encoded, encode_index_workspace_runtime_batch_payload_v1(&retry, HASH_ALGORITHM).unwrap());
}

#[test]
fn producer_task_payload_preserves_journal_or_scope_without_file_bodies() {
  let before = [0x11; 32];
  let after = [0x22; 32];
  let semantic = [0x33; 32];
  let journal = [0x44; 32];
  let journal_task = IndexProducerTaskRequestV1 {
    operation_id: [0x55; 16],
    kind: IndexProducerTaskKindV1::MutationWindow,
    publication_sequence: 77,
    namespace_root_before: &before,
    namespace_root_after: &after,
    semantic_state_root: &semantic,
    journal_head: Some(&journal),
    scope: None,
  };
  let encoded = encode_index_workspace_producer_task_payload_v1(&journal_task, HASH_ALGORITHM).unwrap();
  let decoded = decode_index_workspace_producer_task_payload_v1(&encoded, HASH_ALGORITHM).unwrap();
  assert_eq!(decoded.operation_id, journal_task.operation_id);
  assert_eq!(decoded.kind, journal_task.kind);
  assert_eq!(decoded.publication_sequence, 77);
  assert_eq!(decoded.namespace_root_before, before);
  assert_eq!(decoded.namespace_root_after, after);
  assert_eq!(decoded.semantic_state_root, semantic);
  assert_eq!(decoded.journal_head, Some(journal.as_slice()));
  assert_eq!(decoded.scope, None);

  let maintenance_task = IndexProducerTaskRequestV1 {
    operation_id: [0x66; 16],
    kind: IndexProducerTaskKindV1::Rebuild,
    publication_sequence: 78,
    namespace_root_before: &after,
    namespace_root_after: &after,
    semantic_state_root: &semantic,
    journal_head: None,
    scope: Some("/docs/reference"),
  };
  let encoded = encode_index_workspace_producer_task_payload_v1(&maintenance_task, HASH_ALGORITHM).unwrap();
  let decoded = decode_index_workspace_producer_task_payload_v1(&encoded, HASH_ALGORITHM).unwrap();
  assert_eq!(decoded.kind, IndexProducerTaskKindV1::Rebuild);
  assert_eq!(decoded.journal_head, None);
  assert_eq!(decoded.scope, Some("/docs/reference"));
  assert!(!encoded.windows(5).any(|window| window == b"body:"));
}

#[test]
fn runtime_batch_payload_rejects_truncation_amplification_and_noncanonical_records() {
  let valid = sample_runtime_batch_payload(HASH_ALGORITHM);
  for cut in 0..valid.len() {
    assert!(decode_index_workspace_runtime_batch_payload_v1(&valid[..cut], HASH_ALGORITHM).is_err(), "accepted runtime cut {cut}");
  }
  let mut trailing = valid.clone();
  trailing.push(0);
  assert!(decode_index_workspace_runtime_batch_payload_v1(&trailing, HASH_ALGORITHM).is_err());

  for offset in [0usize, 4, 6, 8, 40, 42, 48, 64, 68, 69, 70, 96, 100] {
    let mut corrupt = valid.clone();
    corrupt[offset] ^= 0xff;
    assert!(decode_index_workspace_runtime_batch_payload_v1(&corrupt, HASH_ALGORITHM).is_err(), "accepted runtime byte {offset}");
  }
  let mut amplified = valid.clone();
  amplified[44..48].copy_from_slice(&1_048_576u32.to_le_bytes());
  assert!(decode_index_workspace_runtime_batch_payload_v1(&amplified, HASH_ALGORITHM).is_err());
  for range in [16..32, 32..40, 72..80, 80..96, 104..136] {
    let mut corrupt = valid.clone();
    corrupt[range].fill(0);
    assert!(decode_index_workspace_runtime_batch_payload_v1(&corrupt, HASH_ALGORITHM).is_err());
  }

  let frame_length = u32::from_le_bytes(valid[64..68].try_into().unwrap()) as usize;
  let mut duplicate = valid.clone();
  let first = valid[64..64 + frame_length].to_vec();
  duplicate[64 + frame_length..64 + 2 * frame_length].copy_from_slice(&first);
  assert!(decode_index_workspace_runtime_batch_payload_v1(&duplicate, HASH_ALGORITHM).is_err());

  let mut mismatched_order_key = valid;
  let first_order_key = 64 + 40 + HASH_ALGORITHM.hash_length();
  mismatched_order_key[first_order_key] ^= 0x01;
  assert!(decode_index_workspace_runtime_batch_payload_v1(&mismatched_order_key, HASH_ALGORITHM).is_err());
}

#[test]
fn producer_task_payload_rejects_every_invalid_presence_and_root_closure() {
  let valid = sample_journal_task_payload(HASH_ALGORITHM, 42);
  for cut in 0..valid.len() {
    assert!(decode_index_workspace_producer_task_payload_v1(&valid[..cut], HASH_ALGORITHM).is_err(), "accepted task cut {cut}");
  }
  let mut trailing = valid.clone();
  trailing.push(0);
  assert!(decode_index_workspace_producer_task_payload_v1(&trailing, HASH_ALGORITHM).is_err());
  for offset in [0usize, 4, 6, 8, 32, 34, 44, 46, 48, 52] {
    let mut corrupt = valid.clone();
    corrupt[offset] ^= 0xff;
    assert!(decode_index_workspace_producer_task_payload_v1(&corrupt, HASH_ALGORITHM).is_err(), "accepted task byte {offset}");
  }
  let hash_width = HASH_ALGORITHM.hash_length();
  for range in [
    16..32,
    36..44,
    56..56 + hash_width,
    56 + hash_width..56 + 2 * hash_width,
    56 + 2 * hash_width..56 + 3 * hash_width,
    56 + 3 * hash_width..56 + 4 * hash_width,
  ] {
    let mut corrupt = valid.clone();
    corrupt[range].fill(0);
    assert!(decode_index_workspace_producer_task_payload_v1(&corrupt, HASH_ALGORITHM).is_err());
  }
  let mut equal_roots = valid.clone();
  let before = equal_roots[56..56 + hash_width].to_vec();
  equal_roots[56 + hash_width..56 + 2 * hash_width].copy_from_slice(&before);
  assert!(decode_index_workspace_producer_task_payload_v1(&equal_roots, HASH_ALGORITHM).is_err());

  let root = [0x33; 32];
  let invalid_scope = IndexProducerTaskRequestV1 {
    operation_id: [1; 16],
    kind: IndexProducerTaskKindV1::Build,
    publication_sequence: 1,
    namespace_root_before: &root,
    namespace_root_after: &root,
    semantic_state_root: &root,
    journal_head: None,
    scope: Some("/docs/../private"),
  };
  assert!(encode_index_workspace_producer_task_payload_v1(&invalid_scope, HASH_ALGORITHM).is_err());
}

fn scope_reverse_record(document_ordinal: u64) -> Vec<u8> {
  let file_key = digest_parts(HASH_ALGORITHM, &[b"file", &document_ordinal.to_le_bytes()]);
  encode_scope_reverse_record(&ScopeReverseRecordV1 { document_ordinal, file_key: &file_key }, HASH_ALGORITHM).unwrap()
}

#[test]
fn index_workspace_codecs_match_independent_cross_profile_fixtures() {
  for (hash_algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let runtime_payload = sample_runtime_batch_payload(hash_algorithm);
    let runtime_object = encode_index_workspace_object_v1(&IndexWorkspaceObjectWriteV1 {
      kind: IndexWorkspaceObjectKindV1::RuntimeBatch,
      hash_algorithm,
      database_id: [0x11; 16],
      destination_physical_instance_id: [0x22; 16],
      workspace_id: [0x33; 16],
      runtime_id: [0x44; 16],
      object_id: [0x55; 16],
      object_sequence: 9,
      created_at_ms: 1_725_000_000_123,
      logical_record_count: 2,
      minimum_publication_sequence: 40,
      maximum_publication_sequence: 41,
      payload: &runtime_payload,
    })
    .unwrap();
    let runtime_fixture = std::fs::read(format!(
      "{}/spec/fixtures/v4/index-runtime-workspace-object-v1/aiwo-{profile}-runtime-batch-valid.bin",
      env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    assert_eq!(runtime_object, runtime_fixture, "runtime object profile {profile}");

    let task_payload = sample_journal_task_payload(hash_algorithm, 42);
    let task_object = encode_index_workspace_object_v1(&IndexWorkspaceObjectWriteV1 {
      kind: IndexWorkspaceObjectKindV1::ProducerTask,
      hash_algorithm,
      database_id: [0x11; 16],
      destination_physical_instance_id: [0x22; 16],
      workspace_id: [0x33; 16],
      runtime_id: [0x44; 16],
      object_id: [0x55; 16],
      object_sequence: 10,
      created_at_ms: 1_725_000_000_123,
      logical_record_count: 1,
      minimum_publication_sequence: 42,
      maximum_publication_sequence: 42,
      payload: &task_payload,
    })
    .unwrap();
    let task_fixture = std::fs::read(format!(
      "{}/spec/fixtures/v4/index-runtime-workspace-object-v1/aiwo-{profile}-producer-task-valid.bin",
      env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    assert_eq!(task_object, task_fixture, "producer task profile {profile}");

    let manifest = encode_index_workspace_manifest_v1(&IndexWorkspaceManifestWriteV1 {
      database_id: [0x11; 16],
      destination_physical_instance_id: [0x22; 16],
      workspace_id: [0x33; 16],
      runtime_id: [0x44; 16],
      manifest_sequence: 1,
      previous_manifest_digest: [0; 32],
      object_kind: IndexWorkspaceObjectKindV1::RuntimeBatch,
      object_id: [0x55; 16],
      object_digest: *blake3::hash(&runtime_object).as_bytes(),
      object_stored_bytes: runtime_object.len() as u64,
      cumulative_object_count: 1,
      cumulative_stored_bytes: runtime_object.len() as u64,
      created_at_ms: 1_725_000_000_123,
    })
    .unwrap();
    let manifest_fixture = std::fs::read(format!(
      "{}/spec/fixtures/v4/index-runtime-workspace-manifest-v1/aiwm-{profile}-runtime-head-valid.bin",
      env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    assert_eq!(manifest.as_slice(), manifest_fixture, "manifest profile {profile}");
  }
}

#[test]
fn index_workspace_manifest_has_one_canonical_checked_layout() {
  let encoded = encode_index_workspace_manifest_v1(&IndexWorkspaceManifestWriteV1 {
    database_id: [0x11; 16],
    destination_physical_instance_id: [0x22; 16],
    workspace_id: [0x33; 16],
    runtime_id: [0x44; 16],
    manifest_sequence: 7,
    previous_manifest_digest: [0x55; 32],
    object_kind: IndexWorkspaceObjectKindV1::RuntimeBatch,
    object_id: [0x66; 16],
    object_digest: [0x77; 32],
    object_stored_bytes: 4096,
    cumulative_object_count: 3,
    cumulative_stored_bytes: 8192,
    created_at_ms: 1_725_000_000_123,
  })
  .unwrap();

  assert_eq!(encoded.len(), 208);
  assert_eq!(&encoded[..4], b"AIWM");
  let decoded = decode_index_workspace_manifest_v1(&encoded).unwrap();
  assert_eq!(decoded.database_id, [0x11; 16]);
  assert_eq!(decoded.destination_physical_instance_id, [0x22; 16]);
  assert_eq!(decoded.workspace_id, [0x33; 16]);
  assert_eq!(decoded.runtime_id, [0x44; 16]);
  assert_eq!(decoded.manifest_sequence, 7);
  assert_eq!(decoded.previous_manifest_digest, [0x55; 32]);
  assert_eq!(decoded.object_kind, IndexWorkspaceObjectKindV1::RuntimeBatch);
  assert_eq!(decoded.object_id, [0x66; 16]);
  assert_eq!(decoded.object_digest, [0x77; 32]);
  assert_eq!(decoded.object_stored_bytes, 4096);
  assert_eq!(decoded.cumulative_object_count, 3);
  assert_eq!(decoded.cumulative_stored_bytes, 8192);
  assert_eq!(decoded.created_at_ms, 1_725_000_000_123);
}

#[test]
fn index_workspace_object_has_one_canonical_checked_layout() {
  let payload = sample_runtime_batch_payload(HashAlgorithm::Blake3_256);
  let encoded = encode_index_workspace_object_v1(&IndexWorkspaceObjectWriteV1 {
    kind: IndexWorkspaceObjectKindV1::RuntimeBatch,
    hash_algorithm: HashAlgorithm::Blake3_256,
    database_id: [0x11; 16],
    destination_physical_instance_id: [0x22; 16],
    workspace_id: [0x33; 16],
    runtime_id: [0x44; 16],
    object_id: [0x55; 16],
    object_sequence: 9,
    created_at_ms: 1_725_000_000_123,
    logical_record_count: 2,
    minimum_publication_sequence: 40,
    maximum_publication_sequence: 41,
    payload: &payload,
  })
  .unwrap();

  assert_eq!(encoded.len(), 184 + payload.len() + 4);
  let decoded = decode_index_workspace_object_v1(&encoded).unwrap();
  assert_eq!(decoded.kind, IndexWorkspaceObjectKindV1::RuntimeBatch);
  assert_eq!(decoded.hash_algorithm, HashAlgorithm::Blake3_256);
  assert_eq!(decoded.object_sequence, 9);
  assert_eq!(decoded.logical_record_count, 2);
  assert_eq!(decoded.minimum_publication_sequence, 40);
  assert_eq!(decoded.maximum_publication_sequence, 41);
  assert_eq!(decoded.payload, payload);

  for cut in 0..encoded.len() {
    assert!(decode_index_workspace_object_v1(&encoded[..cut]).is_err(), "accepted object truncation at {cut}");
  }
  let mut trailing = encoded.clone();
  trailing.push(0);
  assert!(decode_index_workspace_object_v1(&trailing).is_err());
  for offset in [0usize, 4, 6, 8, 10, 12, 20, 88, 152, 184, encoded.len() - 1] {
    let mut corrupt = encoded.clone();
    corrupt[offset] ^= 0xff;
    assert!(decode_index_workspace_object_v1(&corrupt).is_err(), "accepted object corruption at {offset}");
  }
}

#[test]
fn index_workspace_manifest_rejects_every_malformed_boundary() {
  let valid = encode_index_workspace_manifest_v1(&IndexWorkspaceManifestWriteV1 {
    database_id: [1; 16],
    destination_physical_instance_id: [2; 16],
    workspace_id: [3; 16],
    runtime_id: [4; 16],
    manifest_sequence: 1,
    previous_manifest_digest: [0; 32],
    object_kind: IndexWorkspaceObjectKindV1::ProducerTask,
    object_id: [5; 16],
    object_digest: [6; 32],
    object_stored_bytes: 1,
    cumulative_object_count: 1,
    cumulative_stored_bytes: 1,
    created_at_ms: 1,
  })
  .unwrap();

  for cut in 0..valid.len() {
    assert!(decode_index_workspace_manifest_v1(&valid[..cut]).is_err(), "accepted truncation at {cut}");
  }
  let mut trailing = valid.to_vec();
  trailing.push(0);
  assert!(decode_index_workspace_manifest_v1(&trailing).is_err());
  for offset in [0usize, 4, 6, 8, 12, 120, 122, 204] {
    let mut corrupt = valid;
    corrupt[offset] ^= 0xff;
    assert!(decode_index_workspace_manifest_v1(&corrupt).is_err(), "accepted corruption at {offset}");
  }
}

#[test]
fn index_workspace_manifest_rejects_semantic_corruption_even_with_a_valid_crc() {
  let request = IndexWorkspaceManifestWriteV1 {
    database_id: [1; 16],
    destination_physical_instance_id: [2; 16],
    workspace_id: [3; 16],
    runtime_id: [4; 16],
    manifest_sequence: 2,
    previous_manifest_digest: [5; 32],
    object_kind: IndexWorkspaceObjectKindV1::RuntimeBatch,
    object_id: [6; 16],
    object_digest: [7; 32],
    object_stored_bytes: 256,
    cumulative_object_count: 2,
    cumulative_stored_bytes: 512,
    created_at_ms: 1,
  };
  let valid = encode_index_workspace_manifest_v1(&request).unwrap();

  for range in [16..32, 32..48, 48..64, 64..80, 124..140, 140..172] {
    let mut corrupt = valid;
    corrupt[range].fill(0);
    repair_manifest_crc(&mut corrupt);
    assert!(decode_index_workspace_manifest_v1(&corrupt).is_err());
  }
  for (offset, value) in [(80, 0u64), (172, 0), (180, 0), (188, 1), (196, 0)] {
    let mut corrupt = valid;
    corrupt[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    repair_manifest_crc(&mut corrupt);
    assert!(decode_index_workspace_manifest_v1(&corrupt).is_err(), "accepted invalid u64 at {offset}");
  }
  for (sequence, predecessor) in [(1u64, [5u8; 32]), (2, [0; 32])] {
    let mut corrupt = valid;
    corrupt[80..88].copy_from_slice(&sequence.to_le_bytes());
    corrupt[88..120].copy_from_slice(&predecessor);
    repair_manifest_crc(&mut corrupt);
    assert!(decode_index_workspace_manifest_v1(&corrupt).is_err());
  }
  let mut unknown_kind = valid;
  unknown_kind[120..122].copy_from_slice(&99u16.to_le_bytes());
  repair_manifest_crc(&mut unknown_kind);
  assert!(decode_index_workspace_manifest_v1(&unknown_kind).is_err());
}

#[test]
fn index_workspace_object_rejects_semantic_corruption_even_with_valid_checksums() {
  let payload = sample_journal_task_payload(HashAlgorithm::Blake3_256, 4);
  let valid = encode_index_workspace_object_v1(&IndexWorkspaceObjectWriteV1 {
    kind: IndexWorkspaceObjectKindV1::ProducerTask,
    hash_algorithm: HashAlgorithm::Blake3_256,
    database_id: [1; 16],
    destination_physical_instance_id: [2; 16],
    workspace_id: [3; 16],
    runtime_id: [4; 16],
    object_id: [5; 16],
    object_sequence: 2,
    created_at_ms: 3,
    logical_record_count: 1,
    minimum_publication_sequence: 4,
    maximum_publication_sequence: 4,
    payload: &payload,
  })
  .unwrap();

  for range in [24..40, 40..56, 56..72, 72..88, 88..104] {
    let mut corrupt = valid.clone();
    corrupt[range].fill(0);
    repair_object_crc(&mut corrupt);
    assert!(decode_index_workspace_object_v1(&corrupt).is_err());
  }
  for (offset, value) in [(104, 0u64), (112, 0), (128, 0), (136, 0), (144, 3)] {
    let mut corrupt = valid.clone();
    corrupt[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    repair_object_crc(&mut corrupt);
    assert!(decode_index_workspace_object_v1(&corrupt).is_err(), "accepted invalid u64 at {offset}");
  }
  for (offset, value) in [(6, 99u16), (10, 99u16)] {
    let mut corrupt = valid.clone();
    corrupt[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    repair_object_crc(&mut corrupt);
    assert!(decode_index_workspace_object_v1(&corrupt).is_err(), "accepted invalid u16 at {offset}");
  }
}

#[test]
fn index_workspace_encoders_reject_invalid_requests_before_allocation() {
  let manifest = IndexWorkspaceManifestWriteV1 {
    database_id: [1; 16],
    destination_physical_instance_id: [2; 16],
    workspace_id: [3; 16],
    runtime_id: [4; 16],
    manifest_sequence: 1,
    previous_manifest_digest: [0; 32],
    object_kind: IndexWorkspaceObjectKindV1::RuntimeBatch,
    object_id: [5; 16],
    object_digest: [6; 32],
    object_stored_bytes: 1,
    cumulative_object_count: 1,
    cumulative_stored_bytes: 1,
    created_at_ms: 1,
  };
  let mut invalid = manifest;
  invalid.database_id = [0; 16];
  assert!(encode_index_workspace_manifest_v1(&invalid).is_err());
  invalid = manifest;
  invalid.manifest_sequence = 0;
  assert!(encode_index_workspace_manifest_v1(&invalid).is_err());
  invalid = manifest;
  invalid.previous_manifest_digest = [1; 32];
  assert!(encode_index_workspace_manifest_v1(&invalid).is_err());
  invalid = manifest;
  invalid.object_digest = [0; 32];
  assert!(encode_index_workspace_manifest_v1(&invalid).is_err());
  invalid = manifest;
  invalid.cumulative_stored_bytes = 0;
  assert!(encode_index_workspace_manifest_v1(&invalid).is_err());

  let payload = sample_journal_task_payload(HashAlgorithm::Blake3_256, 1);
  let object = IndexWorkspaceObjectWriteV1 {
    kind: IndexWorkspaceObjectKindV1::ProducerTask,
    hash_algorithm: HashAlgorithm::Blake3_256,
    database_id: [1; 16],
    destination_physical_instance_id: [2; 16],
    workspace_id: [3; 16],
    runtime_id: [4; 16],
    object_id: [5; 16],
    object_sequence: 1,
    created_at_ms: 1,
    logical_record_count: 1,
    minimum_publication_sequence: 1,
    maximum_publication_sequence: 1,
    payload: &payload,
  };
  let mut invalid = object;
  invalid.runtime_id = [0; 16];
  assert!(encode_index_workspace_object_v1(&invalid).is_err());
  invalid = object;
  invalid.object_sequence = 0;
  assert!(encode_index_workspace_object_v1(&invalid).is_err());
  invalid = object;
  invalid.logical_record_count = 0;
  assert!(encode_index_workspace_object_v1(&invalid).is_err());
  invalid = object;
  invalid.maximum_publication_sequence = 0;
  assert!(encode_index_workspace_object_v1(&invalid).is_err());
  invalid = object;
  invalid.payload = b"";
  assert!(encode_index_workspace_object_v1(&invalid).is_err());
}

fn repair_manifest_crc(bytes: &mut [u8; 208]) {
  let checksum = crc32fast::hash(&bytes[..204]);
  bytes[204..208].copy_from_slice(&checksum.to_le_bytes());
}

fn repair_object_crc(bytes: &mut [u8]) {
  let checksum_offset = bytes.len() - 4;
  let checksum = crc32fast::hash(&bytes[..checksum_offset]);
  bytes[checksum_offset..].copy_from_slice(&checksum.to_le_bytes());
}

fn sample_runtime_batch_payload(hash_algorithm: HashAlgorithm) -> Vec<u8> {
  let memory = MemoryCoordinator::new(MemoryPolicy::new(4_000_000, 5_000_000, 1, 1_000_000).unwrap());
  let options = IndexCoordinatorOptionsV1::new(1_000_000, 100, 1_000, 1_000_000).unwrap();
  let mut coordinator = IndexCoordinatorV1::new([0x77; 16], hash_algorithm, memory, options, 1_000).unwrap();
  let hash_width = hash_algorithm.hash_length();
  for (ordinal, (index_byte, file_byte, operation_byte, publication_sequence)) in
    [(0u64, (0x21, 0x31, 0x11, 40u64)), (1, (0x22, 0x32, 0x12, 41))]
  {
    let index_id = vec![index_byte; hash_width];
    let file_key = vec![file_byte; hash_width];
    let encoded_record =
      encode_scope_reverse_record(&ScopeReverseRecordV1 { document_ordinal: ordinal + 3, file_key: &file_key }, hash_algorithm).unwrap();
    coordinator
      .admit(
        IndexMutationRequestV1 {
          index_id: &index_id,
          role: OrderedIndexRoleV1::ScopeReverse,
          publication_sequence,
          operation_id: [operation_byte; 16],
          encoded_record: &encoded_record,
        },
        1_001 + ordinal,
      )
      .unwrap();
  }
  let batch = coordinator.begin_flush(1_010, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
  encode_index_workspace_runtime_batch_payload_v1(&batch, hash_algorithm).unwrap()
}

fn sample_journal_task_payload(hash_algorithm: HashAlgorithm, publication_sequence: u64) -> Vec<u8> {
  let hash_width = hash_algorithm.hash_length();
  let before = vec![0x61; hash_width];
  let after = vec![0x62; hash_width];
  let semantic = vec![0x63; hash_width];
  let journal = vec![0x64; hash_width];
  encode_index_workspace_producer_task_payload_v1(
    &IndexProducerTaskRequestV1 {
      operation_id: [0x71; 16],
      kind: IndexProducerTaskKindV1::MutationWindow,
      publication_sequence,
      namespace_root_before: &before,
      namespace_root_after: &after,
      semantic_state_root: &semantic,
      journal_head: Some(&journal),
      scope: None,
    },
    hash_algorithm,
  )
  .unwrap()
}
