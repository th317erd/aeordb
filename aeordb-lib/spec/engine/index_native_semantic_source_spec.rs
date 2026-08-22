use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};

use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::first_authority::{
  FirstAuthorityPublicationRequestV1, IndexArtifactBatchPublicationRequestV1, PreparedNamespaceTreeV0, V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_artifact::ImmutableIndexArtifactKindV1;
use aeordb::engine::v4::index_coordinator_recovery::IndexRecoveryOptionsV1;
use aeordb::engine::v4::index_native_semantic_source::{
  FirstAuthorityIndexSemanticObjectReadSourceV1, NativeIndexOperationDescriptorCatalogV1, NativeIndexScopeOrdinalAuthorityV1,
  NativeIndexSemanticSourceErrorV1,
};
use aeordb::engine::v4::index_native_journal_source::FirstAuthorityIndexProducerJournalSourceV1;
use aeordb::engine::v4::index_operation_control::IndexOperationKindV1;
use aeordb::engine::v4::index_recovery_store::{
  IndexScopeOrdinalStoreRegistryOptionsV1, IndexScopeOrdinalStoreRegistryV1, NativeIndexOperationDescriptorV1,
};
use aeordb::engine::v4::index_scope_ordinal_authority::IndexScopeOrdinalStateOptionsV1;
use aeordb::engine::v4::index_semantic_source::{
  IndexScopeOrdinalAuthorityV1, IndexScopeOrdinalClaimErrorClassV1, IndexScopeOrdinalClaimRequestV1, IndexSemanticObjectReadSourceV1,
};
use aeordb::engine::v4::index_producer_source::{ResolvedIndexDocumentTransitionV1, ResolvedIndexDocumentV1};
use aeordb::engine::v4::index_producer_journal_source::{
  IndexProducerJournalReadErrorClassV1, IndexProducerJournalReadRequestV1, IndexProducerJournalSourceV1,
};
use aeordb::engine::v4::namespace::{
  SemanticAvailabilityV1, SemanticUnavailableReasonV1, decode_semantic_object, encode_semantic_state_object, SemanticStateWriteV1,
};
use aeordb::engine::v4::index_task::{
  JournalOwnerKindV1, MutationJournalWriteV1, MutationKindV1, MutationRecordWriteV1, MutationSideWriteV1, encode_mutation_journal,
};
use aeordb::engine::{DiskKVStore, FileRecord, HashAlgorithm, MockClock, VirtualClock};
use tokio_util::sync::CancellationToken;

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;
const DATABASE_ID: [u8; 16] = [0x31; 16];

fn memory() -> Arc<MemoryCoordinator> {
  Arc::new(MemoryCoordinator::new(MemoryPolicy::new(64 * 1_024 * 1_024, 96 * 1_024 * 1_024, 1, 8 * 1_024 * 1_024).unwrap()))
}

fn descriptor(scope_fill: u8, operation_fill: u8) -> NativeIndexOperationDescriptorV1 {
  descriptor_for_database(DATABASE_ID, scope_fill, operation_fill)
}

fn descriptor_for_database(database_id: [u8; 16], scope_fill: u8, operation_fill: u8) -> NativeIndexOperationDescriptorV1 {
  NativeIndexOperationDescriptorV1::new(
    ALGORITHM,
    database_id,
    vec![scope_fill; ALGORITHM.hash_length()],
    [operation_fill; 16],
    IndexOperationKindV1::Build,
    vec![scope_fill.wrapping_add(1); ALGORITHM.hash_length()],
    None,
    None,
  )
  .unwrap()
}

fn initial_header_for_algorithm(kv_block_length: u64, hash_algorithm: HashAlgorithm) -> DatabaseHeaderV4 {
  DatabaseHeaderV4 {
    hash_algorithm,
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
    head_hash: vec![0; hash_algorithm.hash_length()],
    base_hash: vec![0; hash_algorithm.hash_length()],
    target_hash: vec![0; hash_algorithm.hash_length()],
    required_writer_capabilities: [0; 32],
    system_family_registry_version: 1,
    system_family_registry_fingerprint: vec![0x41; hash_algorithm.hash_length()],
    writer_fence_epoch: 1,
    physical_instance_id: [0x51; 16],
  }
}

fn publisher() -> (tempfile::TempDir, Arc<V4FirstAuthorityPublisher>, Vec<u8>) {
  publisher_for_algorithm(ALGORITHM)
}

fn publisher_for_algorithm(hash_algorithm: HashAlgorithm) -> (tempfile::TempDir, Arc<V4FirstAuthorityPublisher>, Vec<u8>) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("native-semantic-source.aeordb");
  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(path).unwrap();
  let kv_block_length = initial_block_size();
  let header = initial_header_for_algorithm(kv_block_length, hash_algorithm);
  let slot = encode_database_header_slot(&header).unwrap();
  file.seek(SeekFrom::Start(0)).unwrap();
  file.write_all(&slot).unwrap();
  file.write_all(&slot).unwrap();
  let coordinator = Arc::new(DurabilityCoordinator::new());
  let kv = DiskKVStore::create_with_coordinator(
    file.try_clone().unwrap(),
    hash_algorithm,
    header.kv_block_offset,
    header.hot_tail_offset,
    0,
    Arc::clone(&coordinator),
  )
  .unwrap();
  file.sync_all().unwrap();
  let publisher = Arc::new(V4FirstAuthorityPublisher::new(kv, coordinator).unwrap());
  let semantic_state = encode_semantic_state_object(
    &SemanticStateWriteV1 {
      required_capabilities: [0; 32],
      availability: SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured },
    },
    hash_algorithm,
  )
  .unwrap();
  let semantic_root = semantic_state.object_id.clone();
  publisher
    .publish(&FirstAuthorityPublicationRequestV1 {
      database_id: DATABASE_ID,
      transaction_id: [0x61; 16],
      created_at_ms: 1_700_000_000_100,
      namespace_tree: PreparedNamespaceTreeV0 { root_hash: digest_parts(hash_algorithm, &[b"dirc:"]), stored_value: Vec::new() },
      semantic_state,
      required_capabilities: [0; 32],
      typed_closure_digest: digest_parts(hash_algorithm, &[b"native semantic source closure"]),
      authority_identity: b"HEAD".to_vec(),
    })
    .unwrap();
  (directory, publisher, semantic_root)
}

fn mutation_journal() -> aeordb::engine::v4::index_artifact::EncodedImmutableIndexArtifactV1 {
  mutation_journal_for_algorithm(ALGORITHM)
}

fn mutation_journal_for_algorithm(hash_algorithm: HashAlgorithm) -> aeordb::engine::v4::index_artifact::EncodedImmutableIndexArtifactV1 {
  let hash = |label: &[u8]| digest_parts(hash_algorithm, &[b"native journal source:", label]);
  let previous_segment = vec![0; hash_algorithm.hash_length()];
  encode_mutation_journal(&MutationJournalWriteV1 {
    hash_algorithm,
    owner_id: [0x71; 16],
    owner_kind: JournalOwnerKindV1::Task,
    generation: 1,
    segment_ordinal: 0,
    chain_reset: true,
    previous_segment: &previous_segment,
    semantic_state_root: &hash(b"semantic"),
    runtime_boot_id: [0x72; 16],
    records: &[MutationRecordWriteV1 {
      kind: MutationKindV1::Create,
      sequence: 1,
      mutation_id: &hash(b"mutation"),
      batch_ordinal: 0,
      batch_count: 1,
      root_before: &hash(b"root before"),
      root_after: &hash(b"root after"),
      before: None,
      after: Some(MutationSideWriteV1 { path: "/docs/native.json", revision: &hash(b"revision") }),
      committed_at_ms: 1_700_000_000_200,
    }],
  })
  .unwrap()
}

fn maximum_length_mutation_journal() -> aeordb::engine::v4::index_artifact::EncodedImmutableIndexArtifactV1 {
  let baseline = mutation_journal();
  let maximum_length = ImmutableIndexArtifactKindV1::MutationJournalSegment.maximum_encoded_length();
  let extra_path_bytes = maximum_length.checked_sub(baseline.value.len()).unwrap();
  let path = format!("/{}", "a".repeat("/docs/native.json".len() - 1 + extra_path_bytes));
  let hash = |label: &[u8]| digest_parts(ALGORITHM, &[b"native maximum journal source:", label]);
  let previous_segment = vec![0; ALGORITHM.hash_length()];
  let artifact = encode_mutation_journal(&MutationJournalWriteV1 {
    hash_algorithm: ALGORITHM,
    owner_id: [0x73; 16],
    owner_kind: JournalOwnerKindV1::Task,
    generation: 1,
    segment_ordinal: 0,
    chain_reset: true,
    previous_segment: &previous_segment,
    semantic_state_root: &hash(b"semantic"),
    runtime_boot_id: [0x74; 16],
    records: &[MutationRecordWriteV1 {
      kind: MutationKindV1::Create,
      sequence: 1,
      mutation_id: &hash(b"mutation"),
      batch_ordinal: 0,
      batch_count: 1,
      root_before: &hash(b"root before"),
      root_after: &hash(b"root after"),
      before: None,
      after: Some(MutationSideWriteV1 { path: &path, revision: &hash(b"revision") }),
      committed_at_ms: 1_700_000_000_302,
    }],
  })
  .unwrap();
  assert_eq!(artifact.value.len(), maximum_length);
  artifact
}

#[test]
fn first_authority_semantic_reader_loads_the_exact_shadow_object() {
  let (_directory, publisher, semantic_root) = publisher();
  let source = FirstAuthorityIndexSemanticObjectReadSourceV1::new(Arc::clone(&publisher));

  let bytes = source.load_semantic_object(0x0001, &semantic_root).unwrap().unwrap();
  let decoded = decode_semantic_object(&bytes, ALGORITHM).unwrap();
  assert_eq!(decoded.object_id, semantic_root);
  assert!(matches!(decoded.kind, aeordb::engine::v4::namespace::SemanticObjectKind::State { .. }));
  assert_eq!(source.load_semantic_object(0x0002, &semantic_root).unwrap(), None);

  let error = source.load_semantic_object(0x0001, &[0x77]).unwrap_err();
  assert_eq!(error.class(), aeordb::engine::v4::index_producer_source::IndexSemanticScopeReadErrorClassV1::Corrupt);
}

#[test]
fn first_authority_journal_reader_is_exact_bounded_cancelable_and_releases_task_memory() {
  let (_directory, publisher, _semantic_root) = publisher();
  let artifact = mutation_journal();
  publisher
    .publish_index_artifacts(IndexArtifactBatchPublicationRequestV1 {
      database_id: &DATABASE_ID,
      artifacts: &[&artifact],
      publication_timestamp_ms: 1_700_000_000_300,
    })
    .unwrap();
  let task_memory = memory();
  let source = FirstAuthorityIndexProducerJournalSourceV1::new(Arc::clone(&publisher), Arc::clone(&task_memory), ALGORITHM);
  let baseline = task_memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes;

  let read = source
    .load_journal(IndexProducerJournalReadRequestV1 { hash_algorithm: ALGORITHM, journal_head: &artifact.key, is_cancelled: &|| false })
    .unwrap();
  assert_eq!(read.encoded(), artifact.value);
  assert_eq!(read.decode_journal(ALGORITHM, &artifact.key).unwrap().key, artifact.key);
  assert!(task_memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes > baseline);
  drop(read);
  assert_eq!(task_memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, baseline);

  let missing = digest_parts(ALGORITHM, &[b"missing native journal"]);
  let error = source
    .load_journal(IndexProducerJournalReadRequestV1 { hash_algorithm: ALGORITHM, journal_head: &missing, is_cancelled: &|| false })
    .unwrap_err();
  assert_eq!(error.class(), IndexProducerJournalReadErrorClassV1::Corrupt);
  assert_eq!(error.code(), "native_journal_missing");
  let error = source
    .load_journal(IndexProducerJournalReadRequestV1 { hash_algorithm: ALGORITHM, journal_head: &artifact.key, is_cancelled: &|| true })
    .unwrap_err();
  assert_eq!(error.class(), IndexProducerJournalReadErrorClassV1::Cancelled);
  assert_eq!(task_memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, baseline);

  let cancellation_checks = AtomicUsize::new(0);
  let cancel_after_probe = || cancellation_checks.fetch_add(1, Ordering::AcqRel) >= 1;
  let error = source
    .load_journal(IndexProducerJournalReadRequestV1 {
      hash_algorithm: ALGORITHM,
      journal_head: &artifact.key,
      is_cancelled: &cancel_after_probe,
    })
    .unwrap_err();
  assert_eq!(error.class(), IndexProducerJournalReadErrorClassV1::Cancelled);
  assert_eq!(error.code(), "native_journal_cancelled_after_probe");
  assert_eq!(task_memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, baseline);

  for (cancel_after_checks, expected_code) in [(2, "native_journal_cancelled_after_admission"), (3, "native_journal_cancelled_after_read")]
  {
    let cancellation_checks = AtomicUsize::new(0);
    let cancel_at_boundary = || cancellation_checks.fetch_add(1, Ordering::AcqRel) >= cancel_after_checks;
    let error = source
      .load_journal(IndexProducerJournalReadRequestV1 {
        hash_algorithm: ALGORITHM,
        journal_head: &artifact.key,
        is_cancelled: &cancel_at_boundary,
      })
      .unwrap_err();
    assert_eq!(error.class(), IndexProducerJournalReadErrorClassV1::Cancelled);
    assert_eq!(error.code(), expected_code);
    assert_eq!(task_memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, baseline);
  }

  let pressured_memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(1, 2, 1, 1).unwrap()));
  let pressured = FirstAuthorityIndexProducerJournalSourceV1::new(publisher, Arc::clone(&pressured_memory), ALGORITHM);
  let error = pressured
    .load_journal(IndexProducerJournalReadRequestV1 { hash_algorithm: ALGORITHM, journal_head: &artifact.key, is_cancelled: &|| false })
    .unwrap_err();
  assert_eq!(error.class(), IndexProducerJournalReadErrorClassV1::Retryable);
  assert_eq!(pressured_memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
}

#[test]
fn first_authority_journal_reader_preserves_the_sha512_profile() {
  let hash_algorithm = HashAlgorithm::Sha512;
  let (_directory, publisher, _semantic_root) = publisher_for_algorithm(hash_algorithm);
  let artifact = mutation_journal_for_algorithm(hash_algorithm);
  publisher
    .publish_index_artifacts(IndexArtifactBatchPublicationRequestV1 {
      database_id: &DATABASE_ID,
      artifacts: &[&artifact],
      publication_timestamp_ms: 1_700_000_000_301,
    })
    .unwrap();
  let task_memory = memory();
  let source = FirstAuthorityIndexProducerJournalSourceV1::new(publisher, Arc::clone(&task_memory), hash_algorithm);

  let read = source
    .load_journal(IndexProducerJournalReadRequestV1 { hash_algorithm, journal_head: &artifact.key, is_cancelled: &|| false })
    .unwrap();
  let decoded = read.decode_journal(hash_algorithm, &artifact.key).unwrap();
  assert_eq!(decoded.key.len(), hash_algorithm.hash_length());
  assert_eq!(decoded.key, artifact.key);
  drop(read);
  assert_eq!(task_memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
}

#[test]
fn first_authority_journal_reader_accounts_for_the_whole_entity_allocation_at_the_frozen_limit() {
  let (_directory, publisher, _semantic_root) = publisher();
  let artifact = maximum_length_mutation_journal();
  publisher
    .publish_index_artifacts(IndexArtifactBatchPublicationRequestV1 {
      database_id: &DATABASE_ID,
      artifacts: &[&artifact],
      publication_timestamp_ms: 1_700_000_000_303,
    })
    .unwrap();
  let task_memory = memory();
  let source = FirstAuthorityIndexProducerJournalSourceV1::new(publisher, Arc::clone(&task_memory), ALGORITHM);

  let read = source
    .load_journal(IndexProducerJournalReadRequestV1 { hash_algorithm: ALGORITHM, journal_head: &artifact.key, is_cancelled: &|| false })
    .unwrap();
  assert_eq!(read.encoded().len(), ImmutableIndexArtifactKindV1::MutationJournalSegment.maximum_encoded_length());
  assert!(read.reserved_bytes() > u64::try_from(read.encoded().len()).unwrap());
  assert_eq!(read.decode_journal(ALGORITHM, &artifact.key).unwrap().key, artifact.key);
  drop(read);
  assert_eq!(task_memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
}

#[test]
fn descriptor_catalog_is_sorted_unique_bounded_and_memory_accounted() {
  let catalog_memory = memory();
  let before = catalog_memory.snapshot().unwrap().owner(MemoryOwner::IndexCleanCache).unwrap().reserved_bytes;
  let second = descriptor(0x22, 0x42);
  let first = descriptor(0x11, 0x41);
  let catalog = NativeIndexOperationDescriptorCatalogV1::new(
    ALGORITHM,
    DATABASE_ID,
    &[second.clone(), first.clone()],
    2,
    64 * 1_024,
    Arc::clone(&catalog_memory),
    &|| false,
  )
  .unwrap();
  assert_eq!(catalog.len(), 2);
  assert_eq!(catalog.descriptor(first.index_id()).unwrap(), &first);
  assert_eq!(catalog.descriptor(second.index_id()).unwrap(), &second);
  assert!(catalog.retained_bytes() > 0);
  assert_eq!(
    catalog_memory.snapshot().unwrap().owner(MemoryOwner::IndexCleanCache).unwrap().reserved_bytes - before,
    catalog.retained_bytes()
  );
  drop(catalog);
  assert_eq!(catalog_memory.snapshot().unwrap().owner(MemoryOwner::IndexCleanCache).unwrap().reserved_bytes, before);

  let duplicate = descriptor(0x11, 0x43);
  assert!(NativeIndexOperationDescriptorCatalogV1::new(
    ALGORITHM,
    DATABASE_ID,
    &[first.clone(), duplicate],
    2,
    64 * 1_024,
    Arc::clone(&catalog_memory),
    &|| false,
  )
  .is_err());
  assert!(NativeIndexOperationDescriptorCatalogV1::new(
    ALGORITHM,
    DATABASE_ID,
    &[first.clone(), second],
    1,
    64 * 1_024,
    Arc::clone(&catalog_memory),
    &|| false,
  )
  .is_err());
  assert!(NativeIndexOperationDescriptorCatalogV1::new(ALGORITHM, DATABASE_ID, &[first], 2, 1, catalog_memory, &|| false,).is_err());

  let memory = memory();
  let before = memory.snapshot().unwrap().owner(MemoryOwner::IndexCleanCache).unwrap().reserved_bytes;
  let foreign = descriptor_for_database([0x91; 16], 0x33, 0x44);
  assert!(matches!(
    NativeIndexOperationDescriptorCatalogV1::new(ALGORITHM, DATABASE_ID, &[foreign], 2, 64 * 1_024, Arc::clone(&memory), &|| false,),
    Err(NativeIndexSemanticSourceErrorV1::Invalid { code: "native_scope_descriptor_authority", .. })
  ));
  assert!(matches!(
    NativeIndexOperationDescriptorCatalogV1::new(
      ALGORITHM,
      DATABASE_ID,
      &[descriptor(0x44, 0x45)],
      2,
      64 * 1_024,
      Arc::clone(&memory),
      &|| true,
    ),
    Err(NativeIndexSemanticSourceErrorV1::Cancelled)
  ));
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::IndexCleanCache).unwrap().reserved_bytes, before);
}

#[test]
fn native_scope_ordinal_authority_routes_exact_scopes_and_preserves_cancellation() {
  let (_directory, publisher, _semantic_root) = publisher();
  let memory = memory();
  let cancellation = CancellationToken::new();
  let retirement = Arc::new(Mutex::new(
    RetirementJournalOwnerV1::new_chain(
      ALGORITHM,
      DATABASE_ID,
      1,
      901,
      RetirementJournalBufferOptionsV1::new(1, 1_024 * 1_024, 30_000),
      &cancellation,
      &memory,
    )
    .unwrap(),
  ));
  let clock: Arc<dyn VirtualClock> = Arc::new(MockClock::new(1, 1_700_000_000_200));
  let registry = Arc::new(
    IndexScopeOrdinalStoreRegistryV1::new(
      IndexScopeOrdinalStoreRegistryOptionsV1::new(4, 1_024 * 1_024).unwrap(),
      ALGORITHM,
      DATABASE_ID,
      IndexRecoveryOptionsV1::new(8, 1_024 * 1_024, 8, 1_024 * 1_024).unwrap(),
      publisher,
      retirement,
      Arc::clone(&memory),
      cancellation.clone(),
      clock,
    )
    .unwrap(),
  );
  let descriptor = descriptor(0x11, 0x41);
  let known_scope = descriptor.index_id().to_vec();
  let catalog = Arc::new(
    NativeIndexOperationDescriptorCatalogV1::new(ALGORITHM, DATABASE_ID, &[descriptor], 4, 64 * 1_024, memory, &|| false).unwrap(),
  );
  assert!(matches!(
    NativeIndexScopeOrdinalAuthorityV1::new(
      HashAlgorithm::Sha512,
      Arc::clone(&catalog),
      Arc::clone(&registry),
      IndexScopeOrdinalStateOptionsV1::new(2, 8).unwrap(),
    ),
    Err(NativeIndexSemanticSourceErrorV1::Invalid { code: "native_scope_ordinal_authority", .. })
  ));
  let authority =
    NativeIndexScopeOrdinalAuthorityV1::new(ALGORITHM, catalog, registry, IndexScopeOrdinalStateOptionsV1::new(2, 8).unwrap()).unwrap();
  let transition = ResolvedIndexDocumentTransitionV1 {
    before: None,
    after: Some(ResolvedIndexDocumentV1 {
      namespace_root: vec![0xa1; ALGORITHM.hash_length()],
      revision_hash: vec![0xa2; ALGORITHM.hash_length()],
      file_record: FileRecord::new("/docs/a.txt".to_string(), None, 1, Vec::new()),
    }),
  };
  let unknown = vec![0x99; ALGORITHM.hash_length()];
  let error = authority
    .claim_scope_ordinal(IndexScopeOrdinalClaimRequestV1 {
      operation_id: [0x71; 16],
      source_publication_sequence: 7,
      semantic_state_root: &[0x81; 32],
      scope_id: &unknown,
      transition: &transition,
      before_in_scope: false,
      after_in_scope: true,
      is_cancelled: &|| false,
    })
    .unwrap_err();
  assert_eq!(error.class(), IndexScopeOrdinalClaimErrorClassV1::Corrupt);
  assert_eq!(error.code(), "native_scope_descriptor_missing");

  let error = authority
    .claim_scope_ordinal(IndexScopeOrdinalClaimRequestV1 {
      operation_id: [0x70; 16],
      source_publication_sequence: 6,
      semantic_state_root: &[0x81; 32],
      scope_id: &known_scope,
      transition: &transition,
      before_in_scope: false,
      after_in_scope: true,
      is_cancelled: &|| false,
    })
    .unwrap_err();
  assert_eq!(error.class(), IndexScopeOrdinalClaimErrorClassV1::Corrupt);
  assert_ne!(error.code(), "native_scope_descriptor_missing");
  assert_eq!(authority.registry().snapshot().unwrap().entries, 1);

  cancellation.cancel();
  let error = authority
    .claim_scope_ordinal(IndexScopeOrdinalClaimRequestV1 {
      operation_id: [0x72; 16],
      source_publication_sequence: 8,
      semantic_state_root: &[0x81; 32],
      scope_id: &known_scope,
      transition: &transition,
      before_in_scope: false,
      after_in_scope: true,
      is_cancelled: &|| false,
    })
    .unwrap_err();
  assert_eq!(error.class(), IndexScopeOrdinalClaimErrorClassV1::Cancelled);
  assert_eq!(error.code(), "native_scope_registry_cancelled");
}
