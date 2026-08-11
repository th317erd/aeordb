use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::hot_tail::read_hot_tail_checked;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::contract_generated::kv_tag;
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::entity::{EntryTypeV4, WHOLE_ENTITY_V1_FLAG_SYSTEM, decode_whole_entity};
use aeordb::engine::v4::first_authority::{FirstAuthorityPublicationRequestV1, PreparedNamespaceTreeV0, V4FirstAuthorityPublisher};
use aeordb::engine::v4::gc::immutable_gc_artifact_key;
use aeordb::engine::v4::gc_retirement::{
  PreparedRetirementJournalSegmentV1, RetirementJournalBufferOptionsV1, RetirementJournalDurableSinkV1, RetirementJournalOwnerV1,
  RetirementJournalRecordWriteV1,
};
use aeordb::engine::v4::gc_state::{RetirementReasonV1, decode_retirement_journal_segment_v1};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, encode_semantic_state_object};
use aeordb::engine::{DiskKVStore, HashAlgorithm};
use tokio_util::sync::CancellationToken;

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1")
}

fn fixture(algorithm: HashAlgorithm) -> Vec<u8> {
  let name = match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => unreachable!("the v4 contract has two fixture widths"),
  };
  fs::read(fixture_root().join(format!("agca-{name}-retirement-journal-segment-valid.bin"))).unwrap()
}

fn database_id() -> [u8; 16] {
  (0x31u8..=0x40).collect::<Vec<_>>().try_into().unwrap()
}

fn initial_header(algorithm: HashAlgorithm, kv_block_length: u64) -> DatabaseHeaderV4 {
  let hash_width = algorithm.hash_length();
  DatabaseHeaderV4 {
    hash_algorithm: algorithm,
    slot_sequence: 1,
    created_at_ms: 1_700_000_000_000,
    updated_at_ms: 1_700_000_000_000,
    database_id: database_id(),
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
  FirstAuthorityPublicationRequestV1 {
    database_id: database_id(),
    transaction_id: [0x61; 16],
    created_at_ms: 1_700_000_000_100,
    namespace_tree: PreparedNamespaceTreeV0 { root_hash: digest_parts(algorithm, &[b"dirc:"]), stored_value: Vec::new() },
    semantic_state,
    required_capabilities: [0; 32],
    typed_closure_digest: digest_parts(algorithm, &[b"typed retirement storage closure"]),
    authority_identity: b"HEAD".to_vec(),
  }
}

fn create_publisher(algorithm: HashAlgorithm) -> (tempfile::TempDir, PathBuf, Arc<DurabilityCoordinator>, V4FirstAuthorityPublisher) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("retirement-storage.aeordb");
  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(&path).unwrap();
  let header = initial_header(algorithm, initial_block_size() as u64);
  let slot = encode_database_header_slot(&header).unwrap();
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
  publisher.publish(&request(algorithm)).unwrap();
  (directory, path, coordinator, publisher)
}

fn reopen(path: &Path) -> (Arc<DurabilityCoordinator>, V4FirstAuthorityPublisher) {
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
  (coordinator.clone(), V4FirstAuthorityPublisher::new(kv, coordinator).unwrap())
}

fn prepared_fixture<'a>(bytes: &'a [u8], artifact_key: &'a [u8], algorithm: HashAlgorithm) -> PreparedRetirementJournalSegmentV1<'a> {
  let segment = decode_retirement_journal_segment_v1(bytes, algorithm).unwrap();
  PreparedRetirementJournalSegmentV1 {
    segment_ordinal: segment.segment_ordinal,
    generation: segment.generation,
    first_replacement_sequence: segment.first_replacement_sequence,
    last_replacement_sequence: segment.last_replacement_sequence,
    record_count: segment.record_count,
    artifact_key,
    value: bytes,
  }
}

fn fixture_record<'a>(bytes: &'a [u8], algorithm: HashAlgorithm) -> RetirementJournalRecordWriteV1<'a> {
  let hash_width = algorithm.hash_length();
  let record_start = 32 + 24 + 32 + hash_width;
  let physical_length = 24 + 2 * hash_width;
  RetirementJournalRecordWriteV1 {
    reason: RetirementReasonV1::StableKeyReplace,
    replacement_publication_sequence: u64::from_le_bytes(bytes[record_start + 8..record_start + 16].try_into().unwrap()),
    retired_at_ms: u64::from_le_bytes(bytes[record_start + 16..record_start + 24].try_into().unwrap()),
    old_incarnation: &bytes[record_start + 24..record_start + 24 + physical_length],
    replacement_incarnation: &bytes[record_start + 24 + physical_length..record_start + 24 + 2 * physical_length],
  }
}

fn read_entity(path: &Path, offset: u64, length: u32) -> Vec<u8> {
  let mut file = OpenOptions::new().read(true).open(path).unwrap();
  file.seek(SeekFrom::Start(offset)).unwrap();
  let mut bytes = vec![0; length as usize];
  file.read_exact(&mut bytes).unwrap();
  bytes
}

#[test]
fn bounded_owner_publishes_one_exact_gc_entity_through_the_shared_hard_authority_at_both_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let expected = fixture(algorithm);
    let (_directory, path, coordinator, mut publisher) = create_publisher(algorithm);
    let before = publisher.observe().unwrap();
    let before_frontier = coordinator.snapshot().unwrap().hard_frontier;
    let cancellation = CancellationToken::new();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(64 * 1024 * 1024, 96 * 1024 * 1024, 1, 8 * 1024 * 1024).unwrap());
    let mut owner = RetirementJournalOwnerV1::new_chain(
      algorithm,
      database_id(),
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &cancellation,
      &memory,
    )
    .unwrap();

    owner.append(fixture_record(&expected, algorithm), 1, &mut publisher).unwrap();

    let after = publisher.observe().unwrap();
    let key = immutable_gc_artifact_key(algorithm, aeordb::engine::v4::gc::GcArtifactKindV1::RetirementJournalSegment, &expected);
    let locator = publisher.locator(&key).unwrap().unwrap();
    assert_eq!(locator.type_flags, kv_tag::GC_ARTIFACT);
    assert_eq!(after.selected.header.slot_sequence, before.selected.header.slot_sequence + 1);
    assert_eq!(after.selected.header.write_sequence_high_water, before.selected.header.write_sequence_high_water + 1);
    assert_eq!(after.selected.header.entry_count, before.selected.header.entry_count + 1);
    assert_eq!(after.selected.header.head_hash, before.selected.header.head_hash);
    assert!(coordinator.snapshot().unwrap().hard_frontier > before_frontier);
    assert_eq!(owner.status().last_hard_publication_sequence, after.selected.header.write_sequence_high_water);

    let entity_bytes = read_entity(&path, locator.offset, locator.total_length);
    let entity = decode_whole_entity(&entity_bytes, algorithm, after.selected.header.write_sequence_high_water).unwrap();
    assert_eq!(entity.entry_type, EntryTypeV4::GcArtifact);
    assert_eq!(entity.flags, WHOLE_ENTITY_V1_FLAG_SYSTEM);
    assert_eq!(entity.key, key);
    assert_eq!(entity.stored_value, expected);
  }
}

#[test]
fn exact_retry_after_reopen_returns_the_original_durable_entity_sequence_without_republication() {
  let algorithm = HashAlgorithm::Blake3_256;
  let expected = fixture(algorithm);
  let (_directory, path, _coordinator, mut publisher) = create_publisher(algorithm);
  let artifact_key = immutable_gc_artifact_key(algorithm, aeordb::engine::v4::gc::GcArtifactKindV1::RetirementJournalSegment, &expected);
  let prepared = prepared_fixture(&expected, &artifact_key, algorithm);
  let first = publisher.publish_synced(&prepared).unwrap();
  let selected = publisher.observe().unwrap();
  drop(publisher);

  let (coordinator, mut reopened) = reopen(&path);
  let before_frontier = coordinator.snapshot().unwrap().hard_frontier;
  let retry = reopened.publish_synced(&prepared).unwrap();

  assert_eq!(retry, first);
  assert_eq!(reopened.observe().unwrap(), selected);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, before_frontier);
}

#[test]
fn malformed_or_mismatched_prepared_segments_refuse_before_mutating_header_or_file() {
  let algorithm = HashAlgorithm::Blake3_256;
  let expected = fixture(algorithm);
  let (_directory, path, coordinator, mut publisher) = create_publisher(algorithm);
  let before = publisher.observe().unwrap();
  let before_length = fs::metadata(&path).unwrap().len();
  let before_frontier = coordinator.snapshot().unwrap().hard_frontier;
  let artifact_key = immutable_gc_artifact_key(algorithm, aeordb::engine::v4::gc::GcArtifactKindV1::RetirementJournalSegment, &expected);
  let valid = prepared_fixture(&expected, &artifact_key, algorithm);
  let mut wrong_key = valid.artifact_key.to_vec();
  wrong_key[0] ^= 0x80;
  let cases = [
    PreparedRetirementJournalSegmentV1 { artifact_key: &wrong_key, ..valid },
    PreparedRetirementJournalSegmentV1 { segment_ordinal: valid.segment_ordinal + 1, ..valid },
    PreparedRetirementJournalSegmentV1 { generation: valid.generation + 1, ..valid },
    PreparedRetirementJournalSegmentV1 { record_count: valid.record_count + 1, ..valid },
  ];

  for prepared in cases {
    assert!(publisher.publish_synced(&prepared).is_err());
    assert_eq!(publisher.observe().unwrap(), before);
    assert_eq!(fs::metadata(&path).unwrap().len(), before_length);
    assert_eq!(coordinator.snapshot().unwrap().hard_frontier, before_frontier);
  }
}
