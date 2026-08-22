use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::first_authority::{
  FirstAuthorityPublicationRequestV1, IndexActivePointerPublicationRequestV1, IndexArtifactBatchPublicationRequestV1,
  PreparedNamespaceTreeV0, V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_compaction_runtime::{
  IndexArtifactCompactionExecutionOutcomeV1, IndexArtifactCompactionExecutionRequestV1, IndexRuntimeCompactionErrorClassV1,
  IndexRuntimeCompactionExecutorV1,
};
use aeordb::engine::v4::index_artifact::{
  ActivePointerKindV1, ActivePointerWriteV1, EncodedImmutableIndexArtifactV1, IndexManifestWriteV1, decode_index_manifest,
  encode_active_pointer, encode_index_manifest,
};
use aeordb::engine::v4::index_manifest::{CoverageVersionV1, IndexManifestBodyV1, ScopeCatalogManifestBodyV1};
use aeordb::engine::v4::index_native_compaction::{NativeIndexCompactionExecutorV1, NativeIndexCompactionOptionsV1};
use aeordb::engine::v4::index_native_semantic_source::FirstAuthorityIndexSemanticObjectReadSourceV1;
use aeordb::engine::v4::index_producer_source::IndexSemanticScopeLimitsV1;
use aeordb::engine::v4::index_semantic_source::{
  CatalogIndexSemanticScopeSourceV1, IndexScopeOrdinalAuthorityV1, IndexScopeOrdinalClaimErrorV1, IndexScopeOrdinalClaimRequestV1,
  IndexSemanticObjectReadSourceV1,
};
use aeordb::engine::v4::index_page::{
  ArtifactDirectoryEntryWriteV1, ArtifactDirectoryWriteV1, OrderedIndexRoleV1, OrderedPageWriteV1, PhysicalHintV1, decode_ordered_page,
  encode_artifact_directory, encode_ordered_page,
};
use aeordb::engine::v4::index_record::{ScopeDocumentRecordV1, ScopeReverseRecordV1, encode_scope_document_record, encode_scope_reverse_record};
use aeordb::engine::v4::namespace::{
  SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, decode_semantic_object, encode_semantic_state_object,
};
use aeordb::engine::v4::scope::decode_scope_definition;
use aeordb::engine::{DiskKVStore, HashAlgorithm};
use tokio_util::sync::CancellationToken;

const DATABASE_ID: [u8; 16] = [0x31; 16];
const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

#[test]
fn native_compaction_options_freeze_one_bounded_working_set() {
  let semantic = semantic_limits();
  let options = NativeIndexCompactionOptionsV1::engine_default(semantic).unwrap();

  assert_eq!(options.semantic_limits(), semantic);
  assert!(options.maximum_working_bytes() >= 160 * 1_024 * 1_024);
  assert!(options.maximum_working_bytes() <= 192 * 1_024 * 1_024);
}

#[test]
fn content_only_authority_completes_without_inventing_compaction_candidates_and_releases_memory() {
  let (_directory, _path, publisher) = create_publisher(ALGORITHM);
  let publisher = Arc::new(publisher);
  let memory = Arc::new(generous_memory());
  let semantic_objects = FirstAuthorityIndexSemanticObjectReadSourceV1::new(Arc::clone(&publisher));
  let semantic_source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, memory.as_ref().clone(), &semantic_objects, &NoOrdinals);
  let cancellation = CancellationToken::new();
  let retirement = Arc::new(Mutex::new(retirement_owner(ALGORITHM, &cancellation, memory.as_ref())));
  let executor = NativeIndexCompactionExecutorV1::new(
    DATABASE_ID,
    ALGORITHM,
    Arc::clone(&publisher),
    retirement,
    Arc::clone(&memory),
    &semantic_source,
    NativeIndexCompactionOptionsV1::engine_default(semantic_limits()).unwrap(),
  )
  .unwrap();
  let selected = publisher.load_selected_semantic_authority().unwrap();
  let before = memory.snapshot().unwrap().reserved_bytes;

  let outcome = executor.execute(execution_request(&selected.semantic_state.object_id, false)).unwrap();

  assert_eq!(outcome, IndexArtifactCompactionExecutionOutcomeV1::Complete { published_owners: 0, publication_bytes: 0 });
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, before);
}

#[test]
fn cancellation_and_memory_pressure_refuse_before_semantic_or_index_reads() {
  let (_directory, _path, publisher) = create_publisher(ALGORITHM);
  let publisher = Arc::new(publisher);
  let semantic_memory = generous_memory();
  let semantic_objects = FirstAuthorityIndexSemanticObjectReadSourceV1::new(Arc::clone(&publisher));
  let semantic_source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, semantic_memory.clone(), &semantic_objects, &NoOrdinals);
  let cancellation = CancellationToken::new();
  let retirement = Arc::new(Mutex::new(retirement_owner(ALGORITHM, &cancellation, &semantic_memory)));
  let constrained = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap()));
  let executor = NativeIndexCompactionExecutorV1::new(
    DATABASE_ID,
    ALGORITHM,
    Arc::clone(&publisher),
    retirement,
    constrained,
    &semantic_source,
    NativeIndexCompactionOptionsV1::engine_default(semantic_limits()).unwrap(),
  )
  .unwrap();
  let selected = publisher.load_selected_semantic_authority().unwrap();

  let cancelled = executor.execute(execution_request(&selected.semantic_state.object_id, true)).unwrap_err();
  assert_eq!(cancelled.class(), IndexRuntimeCompactionErrorClassV1::CancelledBeforeSelection);
  assert_eq!(cancelled.code(), "native_compaction_cancelled");

  let pressure = executor.execute(execution_request(&selected.semantic_state.object_id, false)).unwrap_err();
  assert_eq!(pressure.class(), IndexRuntimeCompactionErrorClassV1::RetryableBeforeSelection);
  assert_eq!(pressure.code(), "native_compaction_memory_pressure");
}

#[test]
fn populated_scope_compaction_merges_two_pages_selects_one_successor_and_then_completes_for_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    run_populated_scope_compaction(algorithm);
  }
}

fn run_populated_scope_compaction(algorithm: HashAlgorithm) {
  let (_directory, path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let memory = Arc::new(generous_memory());
  let cancellation = CancellationToken::new();
  let retirement = Arc::new(Mutex::new(retirement_owner(algorithm, &cancellation, memory.as_ref())));
  let fixture = scope_index_fixture(algorithm);
  let artifacts = fixture.artifacts.iter().collect::<Vec<_>>();
  publisher
    .publish_index_artifacts(IndexArtifactBatchPublicationRequestV1 {
      database_id: &DATABASE_ID,
      artifacts: &artifacts,
      publication_timestamp_ms: 1_700_000_000_200,
    })
    .unwrap();
  let pointer = encode_active_pointer(&ActivePointerWriteV1 {
    kind: ActivePointerKindV1::ScopeCatalog,
    hash_algorithm: algorithm,
    generation: 10,
    owner_id: &fixture.scope_id,
    slot: 0,
    sequence: 1,
    target_manifest_hash: &fixture.manifest.key,
  })
  .unwrap();
  publisher
    .publish_index_active_pointer(
      IndexActivePointerPublicationRequestV1 {
        database_id: &DATABASE_ID,
        pointer: &pointer,
        publication_timestamp_ms: 1_700_000_000_300,
        monotonic_now_ms: 1_700_000_000_300,
      },
      &mut retirement.lock().unwrap(),
    )
    .unwrap();

  let semantic = scope_semantic_graph(algorithm, &fixture.scope_definition, &fixture.scope_id);
  let semantic_source = CatalogIndexSemanticScopeSourceV1::new(algorithm, memory.as_ref().clone(), &semantic.objects, &NoOrdinals);
  let executor = NativeIndexCompactionExecutorV1::new(
    DATABASE_ID,
    algorithm,
    Arc::clone(&publisher),
    Arc::clone(&retirement),
    Arc::clone(&memory),
    &semantic_source,
    NativeIndexCompactionOptionsV1::engine_default(semantic_limits()).unwrap(),
  )
  .unwrap();

  let progress = executor.execute(execution_request(&semantic.state_root, false)).unwrap();
  let IndexArtifactCompactionExecutionOutcomeV1::Progress { published_owners, publication_bytes } = progress else {
    panic!("eligible two-page scope index did not publish compaction progress")
  };
  assert_eq!(published_owners, 1);
  assert!(publication_bytes > 0);
  let selected =
    publisher.load_index_active_pointer_pair(&DATABASE_ID, ActivePointerKindV1::ScopeCatalog, &fixture.scope_id).unwrap().selected.unwrap();
  assert_eq!(selected.generation, 11);
  assert_ne!(selected.target_manifest_hash, fixture.manifest.key);

  drop(executor);
  drop(publisher);
  let publisher = Arc::new(V4FirstAuthorityPublisher::open(&path).unwrap());
  let selected =
    publisher.load_index_active_pointer_pair(&DATABASE_ID, ActivePointerKindV1::ScopeCatalog, &fixture.scope_id).unwrap().selected.unwrap();
  assert_eq!(selected.generation, 11);
  let retirement = Arc::new(Mutex::new(retirement_owner(algorithm, &cancellation, memory.as_ref())));
  let semantic_source = CatalogIndexSemanticScopeSourceV1::new(algorithm, memory.as_ref().clone(), &semantic.objects, &NoOrdinals);
  let executor = NativeIndexCompactionExecutorV1::new(
    DATABASE_ID,
    algorithm,
    Arc::clone(&publisher),
    retirement,
    Arc::clone(&memory),
    &semantic_source,
    NativeIndexCompactionOptionsV1::engine_default(semantic_limits()).unwrap(),
  )
  .unwrap();
  assert_eq!(
    executor.execute(execution_request(&semantic.state_root, false)).unwrap(),
    IndexArtifactCompactionExecutionOutcomeV1::Complete { published_owners: 0, publication_bytes: 0 }
  );
}

fn execution_request(semantic_state_root: &[u8], cancelled: bool) -> IndexArtifactCompactionExecutionRequestV1<'_> {
  IndexArtifactCompactionExecutionRequestV1 {
    operation_id: [0x81; 16],
    publication_sequence: 7,
    namespace_root: &[0x91; 32],
    semantic_state_root,
    scope: "/",
    now_ms: 1_700_000_000_500,
    is_cancelled: if cancelled { &|| true } else { &|| false },
  }
}

fn semantic_limits() -> IndexSemanticScopeLimitsV1 {
  IndexSemanticScopeLimitsV1::new(8, 16, 32, 2 * 1_024 * 1_024).unwrap()
}

fn generous_memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 1, 64 << 20).unwrap())
}

struct NoOrdinals;

impl IndexScopeOrdinalAuthorityV1 for NoOrdinals {
  fn claim_scope_ordinal(&self, _request: IndexScopeOrdinalClaimRequestV1<'_>) -> Result<u64, IndexScopeOrdinalClaimErrorV1> {
    panic!("content-only compaction must not claim document ordinals")
  }
}

struct ScopeIndexFixture {
  scope_id: Vec<u8>,
  scope_definition: Vec<u8>,
  manifest: EncodedImmutableIndexArtifactV1,
  artifacts: Vec<EncodedImmutableIndexArtifactV1>,
}

fn scope_index_fixture(algorithm: HashAlgorithm) -> ScopeIndexFixture {
  let empty = std::fs::read(format!(
    "{}/spec/fixtures/v4/index-artifact-v1/aidx-blake3-256-scope-catalog-manifest-empty.bin",
    env!("CARGO_MANIFEST_DIR")
  ))
  .unwrap();
  let decoded = decode_index_manifest(&empty, HashAlgorithm::Blake3_256).unwrap();
  let IndexManifestBodyV1::ScopeCatalog(body) = decoded.details else {
    panic!("scope fixture decoded as another manifest kind")
  };
  let scope_definition = body.scope_definition.to_vec();
  let scope_id = decode_scope_definition(&scope_definition, algorithm).unwrap().scope_id;

  let documents = [(1, "/a.json"), (2, "/b.json")]
    .into_iter()
    .map(|(ordinal, path)| {
      let file_key = digest_parts(algorithm, &[b"file:", path.as_bytes()]);
      encode_scope_document_record(
        &ScopeDocumentRecordV1 {
          tombstone: false,
          document_ordinal: ordinal,
          file_key: &file_key,
          record_revision_hash: &digest_parts(algorithm, &[b"revision", &ordinal.to_le_bytes()]),
          path,
        },
        algorithm,
      )
      .unwrap()
    })
    .collect::<Vec<_>>();
  let ordinal_pages = documents
    .iter()
    .map(|record| ordered_page(algorithm, &scope_id, OrderedIndexRoleV1::ScopeOrdinal, &[record.as_slice()]))
    .collect::<Vec<_>>();
  let ordinal_root = leaf_directory(algorithm, &scope_id, OrderedIndexRoleV1::ScopeOrdinal, &ordinal_pages);

  let mut reverse = documents
    .iter()
    .enumerate()
    .map(|(index, _)| {
      let path = if index == 0 { "/a.json" } else { "/b.json" };
      let file_key = digest_parts(algorithm, &[b"file:", path.as_bytes()]);
      let encoded = encode_scope_reverse_record(
        &ScopeReverseRecordV1 { document_ordinal: u64::try_from(index).unwrap() + 1, file_key: &file_key },
        algorithm,
      )
      .unwrap();
      (file_key, encoded)
    })
    .collect::<Vec<_>>();
  reverse.sort_by(|left, right| left.0.cmp(&right.0));
  let reverse_refs = reverse.iter().map(|(_, encoded)| encoded.as_slice()).collect::<Vec<_>>();
  let reverse_page = ordered_page(algorithm, &scope_id, OrderedIndexRoleV1::ScopeReverse, &reverse_refs);
  let reverse_root = leaf_directory(algorithm, &scope_id, OrderedIndexRoleV1::ScopeReverse, std::slice::from_ref(&reverse_page));
  let source_namespace_root = digest_parts(algorithm, &[b"native compaction source namespace"]);
  let coverage_epoch_id = [0x77; 16];
  let manifest = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm: algorithm,
    generation: 10,
    owner_id: &scope_id,
    body: IndexManifestBodyV1::ScopeCatalog(ScopeCatalogManifestBodyV1 {
      coverage: CoverageVersionV1 {
        source_namespace_root: &source_namespace_root,
        coverage_epoch_id: &coverage_epoch_id,
        coverage_publication_sequence: body.coverage.coverage_publication_sequence,
      },
      ordinal_directory_root: Some(&ordinal_root.key),
      reverse_directory_root: Some(&reverse_root.key),
      next_document_ordinal: 3,
      live_document_count: 2,
      retained_tombstone_count: 0,
      ordinal_page_count: 2,
      reverse_page_count: 1,
      ..body
    }),
  })
  .unwrap();
  let mut artifacts = ordinal_pages;
  artifacts.push(ordinal_root);
  artifacts.push(reverse_page);
  artifacts.push(reverse_root);
  artifacts.push(manifest.clone());
  ScopeIndexFixture { scope_id, scope_definition, manifest, artifacts }
}

fn ordered_page(algorithm: HashAlgorithm, owner_id: &[u8], role: OrderedIndexRoleV1, records: &[&[u8]]) -> EncodedImmutableIndexArtifactV1 {
  encode_ordered_page(&OrderedPageWriteV1 {
    hash_algorithm: algorithm,
    role,
    owner_id,
    generation: 10,
    page_id: 0,
    previous_page_id: 0,
    next_page_id: 0,
    records,
  })
  .unwrap()
}

fn leaf_directory(
  algorithm: HashAlgorithm,
  owner_id: &[u8],
  role: OrderedIndexRoleV1,
  pages: &[EncodedImmutableIndexArtifactV1],
) -> EncodedImmutableIndexArtifactV1 {
  let decoded = pages.iter().map(|page| decode_ordered_page(&page.value, algorithm).unwrap()).collect::<Vec<_>>();
  let entries = decoded
    .iter()
    .map(|page| ArtifactDirectoryEntryWriteV1 {
      lower_fence: page.lower_fence,
      upper_fence: page.upper_fence,
      child_hash: &page.key,
      child_generation: page.generation,
      live_count: u64::from(page.live_count),
      tombstone_count: u64::from(page.tombstone_count),
      page_count: 1,
      logical_bytes: page.logical_live_bytes,
      minimum_page_id: 0,
      maximum_page_id: 0,
      physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    })
    .collect::<Vec<_>>();
  encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role,
    owner_id,
    generation: 10,
    level: 0,
    entries: &entries,
  })
  .unwrap()
}

struct ScopeSemanticGraph {
  objects: MemorySemanticObjects,
  state_root: Vec<u8>,
}

#[derive(Default)]
struct MemorySemanticObjects {
  values: BTreeMap<(u16, Vec<u8>), Vec<u8>>,
}

impl IndexSemanticObjectReadSourceV1 for MemorySemanticObjects {
  fn load_semantic_object(
    &self,
    kind_id: u16,
    object_id: &[u8],
  ) -> Result<Option<Vec<u8>>, aeordb::engine::v4::index_producer_source::IndexSemanticScopeReadErrorV1> {
    Ok(self.values.get(&(kind_id, object_id.to_vec())).cloned())
  }
}

fn scope_semantic_graph(algorithm: HashAlgorithm, scope_definition: &[u8], scope_id: &[u8]) -> ScopeSemanticGraph {
  let mut objects = MemorySemanticObjects::default();
  let definition = semantic_definition(algorithm, 3, scope_id, scope_definition);
  let definition_object = decode_semantic_object(&definition, algorithm).unwrap();
  objects.values.insert((definition_object.kind_id, definition_object.object_id.clone()), definition);
  let lookup_digest = digest_parts(algorithm, &[b"aeordb.semantic-catalog-key.v1\0", &3u16.to_le_bytes(), scope_id]);
  let catalog = semantic_catalog_leaf(algorithm, scope_id, &definition_object.object_id, &lookup_digest);
  let catalog_object = decode_semantic_object(&catalog, algorithm).unwrap();
  objects.values.insert((catalog_object.kind_id, catalog_object.object_id.clone()), catalog);
  let state = encode_semantic_state_object(
    &SemanticStateWriteV1 {
      required_capabilities: [0; 32],
      availability: SemanticAvailabilityV1::Complete {
        compiler_fingerprint: vec![0x11; algorithm.hash_length()],
        semantic_registry_fingerprint: vec![0x22; algorithm.hash_length()],
        catalog_root: catalog_object.object_id,
        catalog_record_count: 1,
        catalog_node_count: 1,
        definition_count: 1,
        dependency_count: 0,
      },
    },
    algorithm,
  )
  .unwrap();
  let state_root = state.object_id.clone();
  objects.values.insert((0x0001, state.object_id), state.value);
  ScopeSemanticGraph { objects, state_root }
}

fn semantic_definition(algorithm: HashAlgorithm, class: u16, semantic_id: &[u8], definition: &[u8]) -> Vec<u8> {
  let hash_width = algorithm.hash_length();
  let mut body = vec![0; 16 + hash_width + definition.len()];
  body[..2].copy_from_slice(&class.to_le_bytes());
  body[2..4].copy_from_slice(&1u16.to_le_bytes());
  body[8..8 + hash_width].copy_from_slice(semantic_id);
  body[8 + hash_width..12 + hash_width].copy_from_slice(&u32::try_from(definition.len()).unwrap().to_le_bytes());
  body[16 + hash_width..].copy_from_slice(definition);
  semantic_envelope(algorithm, 0x0004, 1, &body)
}

fn semantic_catalog_leaf(algorithm: HashAlgorithm, scope_id: &[u8], definition_object_id: &[u8], lookup_digest: &[u8]) -> Vec<u8> {
  let hash_width = algorithm.hash_length();
  let record_length = 8 + 2 * hash_width + scope_id.len();
  let records_start = 16 + hash_width;
  let mut body = vec![0; records_start + record_length];
  body[4..8].copy_from_slice(&1u32.to_le_bytes());
  body[8..8 + hash_width].copy_from_slice(lookup_digest);
  body[8 + hash_width..12 + hash_width].copy_from_slice(&u32::try_from(record_length).unwrap().to_le_bytes());
  body[records_start..records_start + 2].copy_from_slice(&3u16.to_le_bytes());
  body[records_start + 4..records_start + 8].copy_from_slice(&u32::try_from(scope_id.len()).unwrap().to_le_bytes());
  body[records_start + 8..records_start + 8 + hash_width].copy_from_slice(scope_id);
  body[records_start + 8 + hash_width..records_start + 8 + 2 * hash_width].copy_from_slice(definition_object_id);
  body[records_start + 8 + 2 * hash_width..].copy_from_slice(scope_id);
  semantic_envelope(algorithm, 0x0002, 1, &body)
}

fn semantic_envelope(algorithm: HashAlgorithm, kind_id: u16, item_count: u64, body: &[u8]) -> Vec<u8> {
  let mut bytes = vec![0; 32 + body.len() + 4];
  bytes[..4].copy_from_slice(b"ASEM");
  bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
  bytes[6..8].copy_from_slice(&kind_id.to_le_bytes());
  bytes[8..10].copy_from_slice(&32u16.to_le_bytes());
  let total_length = u32::try_from(bytes.len()).unwrap();
  bytes[12..16].copy_from_slice(&total_length.to_le_bytes());
  bytes[16..20].copy_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
  bytes[20..28].copy_from_slice(&item_count.to_le_bytes());
  bytes[32..32 + body.len()].copy_from_slice(body);
  let checksum_offset = bytes.len() - 4;
  let checksum = crc32fast::hash(&bytes[..checksum_offset]);
  bytes[checksum_offset..].copy_from_slice(&checksum.to_le_bytes());
  decode_semantic_object(&bytes, algorithm).unwrap();
  bytes
}

fn create_publisher(algorithm: HashAlgorithm) -> (tempfile::TempDir, PathBuf, V4FirstAuthorityPublisher) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("native-index-compaction.aeordb");
  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(&path).unwrap();
  let header = initial_header(algorithm);
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
  publisher.publish(&first_authority_request(algorithm)).unwrap();
  (directory, path, publisher)
}

fn initial_header(algorithm: HashAlgorithm) -> DatabaseHeaderV4 {
  let kv_block_length = initial_block_size();
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

fn first_authority_request(algorithm: HashAlgorithm) -> FirstAuthorityPublicationRequestV1 {
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
    typed_closure_digest: digest_parts(algorithm, &[b"typed native compaction closure"]),
    authority_identity: b"HEAD".to_vec(),
  }
}

fn retirement_owner(algorithm: HashAlgorithm, cancellation: &CancellationToken, memory: &MemoryCoordinator) -> RetirementJournalOwnerV1 {
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
