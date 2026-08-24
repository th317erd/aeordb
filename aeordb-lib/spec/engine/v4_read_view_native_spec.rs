use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aeordb::engine::directory_entry::{ChildEntry, serialize_child_entries};
use aeordb::engine::btree::{BTreeNode, InternalNode, LeafNode};
use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::file_record::FileRecord;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, encode_canonical_value};
use aeordb::engine::v4::entity::EntryTypeV4;
use aeordb::engine::v4::first_authority::{
  FirstAuthorityPublicationRequestV1, ImmutableEntityBatchPublicationRequestV1, ImmutableEntityWriteV1,
  ImmutableSemanticObjectBatchPublicationRequestV1, IndexActivePointerPublicationRequestV1, IndexArtifactBatchPublicationRequestV1,
  PreparedNamespaceTreeV0, SuccessorAuthorityPublicationRequestV1, V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1};
use aeordb::engine::v4::field_definition::decode_field_index_definition;
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_artifact::{
  ActivePointerKindV1, ActivePointerWriteV1, CoverageVersionV1, EncodedImmutableIndexArtifactV1, FieldIndexManifestBodyV1,
  IndexManifestBodyV1, IndexManifestWriteV1, ScopeCatalogManifestBodyV1, ValueStoreManifestBodyV1, encode_active_pointer,
  decode_index_manifest, encode_index_manifest,
};
use aeordb::engine::v4::index_artifact_cursor::{
  ArtifactCursorReadErrorV1, ArtifactCursorSourceV1, ArtifactDirectoryRootSummaryV1, ArtifactPageCursorLimitsV1,
  ArtifactPageNeighborModeV1, ArtifactPageSeekV1,
};
use aeordb::engine::v4::index_coverage_planner::IndexCoverageGenerationHealthV1;
use aeordb::engine::v4::index_coverage_registry::{
  FirstAuthorityIndexCoverageRegistrySourceV1, IndexCoverageNvtDescriptorV1, IndexCoverageRegistryOptionsV1,
  IndexCoverageRegistryOwnerKindV1, IndexCoverageRegistryOwnerRequestV1, IndexCoverageRegistrySnapshotV1, IndexCoverageRegistryV1,
  field_definition_fingerprint, field_dependency_fingerprint,
};
use aeordb::engine::v4::index_manifest::FieldNvtManifestBodyV1;
use aeordb::engine::v4::index_partial_acceleration::IndexPartialSourceErrorClassV1;
use aeordb::engine::v4::index_nvt::{
  NvtBasisStatusV1, NvtEntryWriteV1, NvtTileWriteV1, encode_nvt_tile, pin_field_index_v1, validate_field_nvt_basis_v1,
};
use aeordb::engine::v4::index_page::{
  ArtifactDirectoryEntryWriteV1, ArtifactDirectoryWriteV1, OrderedIndexRoleV1, OrderedPageWriteV1, PhysicalHintV1, PostingRecordV1,
  compare_order_keys, decode_artifact_directory, decode_ordered_page, decode_ordered_record, encode_artifact_directory,
  encode_ordered_page, encode_posting_record, ordered_record_order_key,
};
use aeordb::engine::v4::index_record::{ScopeDocumentRecordV1, ScopeReverseRecordV1, encode_scope_document_record, encode_scope_reverse_record};
use aeordb::engine::v4::index_producer_collector::{
  IndexParserDeterministicFailureV1, IndexParserExecutionErrorV1, IndexParserExecutionRequestV1, IndexParserExecutorV1,
  IndexParserOutcomeV1,
};
use aeordb::engine::v4::index_source::{PluginMapperExecutorV1, PluginMapperOutcomeV1, PluginMapperRequestV1, SourceOperationalResultV1};
use aeordb::engine::v4::namespace::{
  EncodedSemanticObjectV1, SemanticAvailabilityV1, SemanticStateWriteV1, decode_namespace_root, decode_semantic_object,
  encode_semantic_state_object,
};
use aeordb::engine::v4::scope::decode_scope_definition;
use aeordb::engine::v4::read_view::{
  CurrentReadAuthorizationV1, ReadViewAuthorizationErrorV1, ReadViewConcealmentV1, ReadViewCredentialKindV1, ReadViewResolverV1,
  ReadViewSelectorV1, ReadableRootStateV1, ResolvedReadViewV1, RootLifecycleObservationV1, RootPinCoordinatorErrorV1,
  RootReadPinCoordinatorV1,
};
use aeordb::engine::v4::read_view_authorization::{
  CapturedCurrentPathAuthorizationSourceV1, CurrentPathAuthorizationV1, PathAuthorizationDecisionV1, ReadViewPermissionAuthorizerV1,
  ResolvedPathAuthorizationV1,
};
use aeordb::engine::v4::position::{PositionComponentStateV1, PositionRouteV1};
use aeordb::engine::v4::position_resolver::{
  PositionUniverseLookupRequestV1, PositionUniverseLookupResultV1, PositionUniverseSourceErrorV1, PositionUniverseSourceV1,
  resolve_position_universe_row_v1,
};
use aeordb::engine::v4::query_aggregate_execution::{
  CompiledQueryAggregateInputV1, QueryAggregateInputLimitsV1, QueryAggregateInputLookupRequestV1, QueryAggregateInputLookupResultV1,
  QueryAggregateInputSourceV1, resolve_query_aggregate_input_v1,
};
use aeordb::engine::v4::query_complete_candidate::{QueryCompletePostingRootRequestV1, QueryCompleteScopeRootRequestV1};
use aeordb::engine::v4::query_partial_candidate::{
  QueryPartialCandidateArtifactSourceV1, QueryPartialPostingRootRequestV1, QueryPartialScopeRootRequestV1,
};
use aeordb::engine::v4::query_planner::{
  CompiledRootAwareQueryPlanV1, QueryAggregateFieldV1, QueryAggregateKindV1, QueryExpressionV1, QueryPlanningContextV1,
  QueryPlanningCoverageGenerationV1, QueryPlanningRequestV1, QueryPredicateOperationV1, QueryPredicateV1, QuerySortDirectionV1,
  QuerySortFieldV1, RootAwareQueryFieldCatalogV1, default_query_planning_limits_v1, plan_root_aware_query_v1,
};
use aeordb::engine::v4::query_executor::{
  QueryAuthoritativeFieldPartitionSourceV1, QueryExecutionByteLimitsV1, QueryExecutionCountLimitsV1,
  QueryExecutionFieldPartitionOpenRequestV1, QueryExecutionFieldStateV1, QueryExecutionLimitsV1, QueryExecutionSourceErrorClassV1,
};
use aeordb::engine::v4::query_native_source::{
  NativeAuthoritativeAuxiliaryLimitsV1, NativeAuthoritativeFieldPartitionLimitsV1, NativeAuthoritativeFieldPartitionSourceV1,
};
use aeordb::engine::v4::query_order_execution::{QueryOrderedTopKLimitsV1, QueryOrderedTopKSinkV1};
use aeordb::engine::v4::read_view_native::{
  NativeReadViewSourceV1, NativeSelectedArtifactCursorRequestV1, NativeSelectedNamespaceFileRowV1, NativeSelectedNamespaceLimitsV1,
  NativeSelectedArtifactRootRequestV1, NativeSelectedNvtFallbackReasonV1, NativeSelectedNamespaceReadErrorClassV1,
  NativeSelectedNamespaceReadErrorV1, NativeSelectedNamespaceReaderV1, NativeSelectedPostingSeekRequestV1,
  NativeSelectedPostingSeekSourceV1, NativeSelectedSemanticByteLimitsV1, NativeSelectedSemanticCountLimitsV1,
  NativeSelectedSemanticLimitsV1, NativeSelectedSourceEvaluationV1, NativeSelectedSourceLimitsV1, NativeSelectedSourceOutcomeV1,
  NativeSelectedSourceParserV1, default_native_selected_semantic_limits_v1,
};
use aeordb::engine::v4::system_family::embedded_system_family_registry;
use aeordb::engine::v4::value_store::decode_value_store_definition;
use aeordb::engine::v4::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use aeordb::engine::permission_resolver::CrudlifyOp;
use aeordb::engine::permissions::{PathPermissions, PermissionLink};
use aeordb::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::{DiskKVStore, HashAlgorithm};
use tokio_util::sync::CancellationToken;

trait SelectedSourceTestExtV1 {
  #[allow(clippy::too_many_arguments)]
  fn evaluate_authoritative_source(
    &self,
    row: &NativeSelectedNamespaceFileRowV1,
    catalog: &RootAwareQueryFieldCatalogV1,
    scope_id: &[u8],
    parser: NativeSelectedSourceParserV1<'_>,
    mapper: Option<&dyn PluginMapperExecutorV1>,
    limits: NativeSelectedSourceLimitsV1,
  ) -> Result<NativeSelectedSourceEvaluationV1, NativeSelectedNamespaceReadErrorV1>;
}

impl SelectedSourceTestExtV1 for NativeSelectedNamespaceReaderV1<'_> {
  fn evaluate_authoritative_source(
    &self,
    row: &NativeSelectedNamespaceFileRowV1,
    catalog: &RootAwareQueryFieldCatalogV1,
    scope_id: &[u8],
    parser: NativeSelectedSourceParserV1<'_>,
    mapper: Option<&dyn PluginMapperExecutorV1>,
    limits: NativeSelectedSourceLimitsV1,
  ) -> Result<NativeSelectedSourceEvaluationV1, NativeSelectedNamespaceReadErrorV1> {
    self.prepare_authoritative_source(catalog, scope_id, limits)?.evaluate(row, parser, mapper)
  }
}

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

fn semantic_definition_fixture(algorithm: HashAlgorithm, folder: &str, prefix: &str, name: &str) -> Vec<u8> {
  let profile = match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => panic!("selected semantic graph fixtures only cover frozen v4 hash profiles"),
  };
  fs::read(format!("{}/spec/fixtures/v4/{folder}/{prefix}-{profile}-{name}-valid.bin", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn semantic_scope_definition(algorithm: HashAlgorithm, owner_path: &str, glob: Option<&str>) -> Vec<u8> {
  let glob = glob.unwrap_or("");
  let mut bytes = vec![0; 64 + owner_path.len() + glob.len()];
  bytes[..4].copy_from_slice(b"ASCP");
  bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
  bytes[6..8].copy_from_slice(&32u16.to_le_bytes());
  let total_length = u32::try_from(bytes.len()).unwrap();
  bytes[8..12].copy_from_slice(&total_length.to_le_bytes());
  bytes[32..36].copy_from_slice(&u32::try_from(owner_path.len()).unwrap().to_le_bytes());
  bytes[36..40].copy_from_slice(&u32::try_from(glob.len()).unwrap().to_le_bytes());
  bytes[40..42].copy_from_slice(&1u16.to_le_bytes());
  bytes[42..44].copy_from_slice(&(if glob.is_empty() { 1u16 } else { 2u16 }).to_le_bytes());
  for offset in [44, 46, 48, 50, 52, 54] {
    bytes[offset..offset + 2].copy_from_slice(&1u16.to_le_bytes());
  }
  let owner_end = 64 + owner_path.len();
  bytes[64..owner_end].copy_from_slice(owner_path.as_bytes());
  bytes[owner_end..].copy_from_slice(glob.as_bytes());
  decode_scope_definition(&bytes, algorithm).unwrap();
  bytes
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

fn semantic_definition_object(algorithm: HashAlgorithm, class: u16, semantic_id: &[u8], definition: &[u8]) -> EncodedSemanticObjectV1 {
  let hash_width = algorithm.hash_length();
  let mut body = vec![0; 16 + hash_width + definition.len()];
  body[..2].copy_from_slice(&class.to_le_bytes());
  body[2..4].copy_from_slice(&1u16.to_le_bytes());
  body[8..8 + hash_width].copy_from_slice(semantic_id);
  body[8 + hash_width..12 + hash_width].copy_from_slice(&u32::try_from(definition.len()).unwrap().to_le_bytes());
  body[16 + hash_width..].copy_from_slice(definition);
  let value = semantic_envelope(algorithm, 0x0004, 1, &body);
  let object = decode_semantic_object(&value, algorithm).unwrap();
  EncodedSemanticObjectV1 { object_id: object.object_id, value }
}

#[derive(Clone)]
struct SemanticCatalogBinding {
  kind: u16,
  semantic_id: Vec<u8>,
  definition_object_id: Vec<u8>,
  owner_key: Vec<u8>,
  lookup_digest: Vec<u8>,
}

fn semantic_catalog_leaf(algorithm: HashAlgorithm, bindings: &[SemanticCatalogBinding]) -> EncodedSemanticObjectV1 {
  let hash_width = algorithm.hash_length();
  let mut sorted = bindings.to_vec();
  sorted.sort_by(|left, right| (left.kind, &left.owner_key).cmp(&(right.kind, &right.owner_key)));
  assert!(sorted.iter().all(|binding| binding.lookup_digest == sorted[0].lookup_digest));
  let records_length: usize = sorted.iter().map(|binding| 8 + 2 * hash_width + binding.owner_key.len()).sum();
  let header_length = 16 + hash_width;
  let mut body = vec![0; header_length + records_length];
  body[4..8].copy_from_slice(&u32::try_from(sorted.len()).unwrap().to_le_bytes());
  body[8..8 + hash_width].copy_from_slice(&sorted[0].lookup_digest);
  body[8 + hash_width..12 + hash_width].copy_from_slice(&u32::try_from(records_length).unwrap().to_le_bytes());
  let mut cursor = header_length;
  for binding in sorted {
    body[cursor..cursor + 2].copy_from_slice(&binding.kind.to_le_bytes());
    body[cursor + 4..cursor + 8].copy_from_slice(&u32::try_from(binding.owner_key.len()).unwrap().to_le_bytes());
    body[cursor + 8..cursor + 8 + hash_width].copy_from_slice(&binding.semantic_id);
    body[cursor + 8 + hash_width..cursor + 8 + 2 * hash_width].copy_from_slice(&binding.definition_object_id);
    body[cursor + 8 + 2 * hash_width..cursor + 8 + 2 * hash_width + binding.owner_key.len()].copy_from_slice(&binding.owner_key);
    cursor += 8 + 2 * hash_width + binding.owner_key.len();
  }
  let value = semantic_envelope(algorithm, 0x0002, bindings.len() as u64, &body);
  let object = decode_semantic_object(&value, algorithm).unwrap();
  EncodedSemanticObjectV1 { object_id: object.object_id, value }
}

fn build_semantic_catalog_node(
  algorithm: HashAlgorithm,
  bindings: &[SemanticCatalogBinding],
  depth: usize,
  objects: &mut Vec<EncodedSemanticObjectV1>,
  node_count: &mut u64,
) -> (Vec<u8>, u64) {
  let hash_width = algorithm.hash_length();
  if bindings.iter().all(|binding| binding.lookup_digest == bindings[0].lookup_digest) {
    let leaf = semantic_catalog_leaf(algorithm, bindings);
    let root = leaf.object_id.clone();
    objects.push(leaf);
    *node_count += 1;
    return (root, bindings.len() as u64);
  }

  let mut prefix_length = 0;
  while depth + prefix_length < hash_width
    && bindings.iter().all(|binding| binding.lookup_digest[depth + prefix_length] == bindings[0].lookup_digest[depth + prefix_length])
  {
    prefix_length += 1;
  }
  let edge_offset = depth + prefix_length;
  let mut groups: BTreeMap<u8, Vec<SemanticCatalogBinding>> = BTreeMap::new();
  for binding in bindings {
    groups.entry(binding.lookup_digest[edge_offset]).or_default().push(binding.clone());
  }
  assert!(groups.len() >= 2);
  let mut children = Vec::new();
  let mut subtree_records = 0u64;
  for (edge, group) in groups {
    let (object_id, record_count) = build_semantic_catalog_node(algorithm, &group, edge_offset + 1, objects, node_count);
    subtree_records += record_count;
    children.push((edge, record_count, object_id));
  }
  let child_length = 12 + hash_width;
  let mut body = vec![0; 20 + prefix_length + children.len() * child_length];
  body[4..6].copy_from_slice(&u16::try_from(depth).unwrap().to_le_bytes());
  body[6..8].copy_from_slice(&u16::try_from(prefix_length).unwrap().to_le_bytes());
  body[8..10].copy_from_slice(&u16::try_from(children.len()).unwrap().to_le_bytes());
  body[12..20].copy_from_slice(&subtree_records.to_le_bytes());
  body[20..20 + prefix_length].copy_from_slice(&bindings[0].lookup_digest[depth..edge_offset]);
  for (index, (edge, record_count, object_id)) in children.into_iter().enumerate() {
    let cursor = 20 + prefix_length + index * child_length;
    body[cursor] = edge;
    body[cursor + 4..cursor + 12].copy_from_slice(&record_count.to_le_bytes());
    body[cursor + 12..cursor + 12 + hash_width].copy_from_slice(&object_id);
  }
  let value = semantic_envelope(algorithm, 0x0003, (body.len() - 20 - prefix_length) as u64 / child_length as u64, &body);
  let object = decode_semantic_object(&value, algorithm).unwrap();
  let root = object.object_id.clone();
  objects.push(EncodedSemanticObjectV1 { object_id: object.object_id, value });
  *node_count += 1;
  (root, subtree_records)
}

struct CompleteSemanticGraph {
  objects: Vec<EncodedSemanticObjectV1>,
  state: EncodedSemanticObjectV1,
  scope_id: Vec<u8>,
  value_store_id: Vec<u8>,
  field_index_id: Vec<u8>,
  scope_definition: Vec<u8>,
  value_store_definition: Vec<u8>,
  field_index_definition: Vec<u8>,
}

fn complete_semantic_graph(algorithm: HashAlgorithm) -> CompleteSemanticGraph {
  let scope = semantic_definition_fixture(algorithm, "scope-definition-v1", "ascp", "root-direct");
  complete_semantic_graph_with_scope(algorithm, scope)
}

fn complete_semantic_graph_with_scope(algorithm: HashAlgorithm, scope: Vec<u8>) -> CompleteSemanticGraph {
  complete_semantic_graph_with_definitions(algorithm, scope, "metadata-hash-corrected", "typed_exact_blake3_v1")
}

fn complete_semantic_graph_with_definitions(
  algorithm: HashAlgorithm,
  scope: Vec<u8>,
  value_store_fixture: &str,
  field_index_fixture: &str,
) -> CompleteSemanticGraph {
  complete_semantic_graph_with_extra_scopes(algorithm, scope, value_store_fixture, field_index_fixture, &[])
}

fn complete_semantic_graph_with_extra_scopes(
  algorithm: HashAlgorithm,
  scope: Vec<u8>,
  value_store_fixture: &str,
  field_index_fixture: &str,
  extra_scopes: &[Vec<u8>],
) -> CompleteSemanticGraph {
  let hash_width = algorithm.hash_length();
  let scope_id = decode_scope_definition(&scope, algorithm).unwrap().scope_id;
  let mut value_store = semantic_definition_fixture(algorithm, "value-store-definition-v1", "avst", value_store_fixture);
  value_store[32..32 + hash_width].copy_from_slice(&scope_id);
  let value_store_id = decode_value_store_definition(&value_store, algorithm).unwrap().value_store_id;
  let mut field_index = semantic_definition_fixture(algorithm, "field-index-definition-v1", "afix", field_index_fixture);
  field_index[32..32 + hash_width].copy_from_slice(&value_store_id);
  complete_semantic_graph_from_encoded(algorithm, scope, value_store, field_index, extra_scopes)
}

fn complete_size_semantic_graph_with_extra_scopes(
  algorithm: HashAlgorithm,
  scope: Vec<u8>,
  extra_scopes: &[Vec<u8>],
) -> CompleteSemanticGraph {
  let hash_width = algorithm.hash_length();
  let scope_id = decode_scope_definition(&scope, algorithm).unwrap().scope_id;
  let mut value_store = semantic_definition_fixture(algorithm, "value-store-definition-v1", "avst", "metadata-hash-corrected");
  value_store[32..32 + hash_width].copy_from_slice(&scope_id);
  let field_start = 112 + hash_width;
  let old_field_length = u32::from_le_bytes(value_store[32 + hash_width..36 + hash_width].try_into().unwrap()) as usize;
  value_store.splice(field_start..field_start + old_field_length, b"@size".iter().copied());
  let total_length = value_store.len() as u32;
  value_store[8..12].copy_from_slice(&total_length.to_le_bytes());
  value_store[32 + hash_width..36 + hash_width].copy_from_slice(&("@size".len() as u32).to_le_bytes());
  let selector_start = field_start + "@size".len();
  value_store[selector_start + 32..selector_start + 34].copy_from_slice(&5u16.to_le_bytes());
  let value_store_id = decode_value_store_definition(&value_store, algorithm).unwrap().value_store_id;
  let mut field_index = semantic_definition_fixture(algorithm, "field-index-definition-v1", "afix", "u64_order_v1");
  field_index[32..32 + hash_width].copy_from_slice(&value_store_id);
  complete_semantic_graph_from_encoded(algorithm, scope, value_store, field_index, extra_scopes)
}

fn complete_semantic_graph_from_encoded(
  algorithm: HashAlgorithm,
  scope: Vec<u8>,
  value_store: Vec<u8>,
  field_index: Vec<u8>,
  extra_scopes: &[Vec<u8>],
) -> CompleteSemanticGraph {
  let hash_width = algorithm.hash_length();
  let scope_id = decode_scope_definition(&scope, algorithm).unwrap().scope_id;
  let value_store_id = decode_value_store_definition(&value_store, algorithm).unwrap().value_store_id;
  let field_index_id = decode_field_index_definition(&field_index, algorithm).unwrap().index_id;
  let scope_definition = scope.clone();
  let value_store_definition = value_store.clone();
  let field_index_definition = field_index.clone();
  let mut definitions =
    vec![(3u16, scope_id.clone(), scope), (4u16, value_store_id.clone(), value_store), (5u16, field_index_id.clone(), field_index)];
  for extra_scope in extra_scopes {
    let extra_scope_id = decode_scope_definition(extra_scope, algorithm).unwrap().scope_id;
    definitions.push((3u16, extra_scope_id, extra_scope.clone()));
  }
  let definition_count = u64::try_from(definitions.len()).unwrap();
  let mut objects = Vec::new();
  let mut bindings = Vec::new();
  for (kind, semantic_id, definition) in definitions {
    let object = semantic_definition_object(algorithm, kind, &semantic_id, &definition);
    let kind_bytes = kind.to_le_bytes();
    let lookup_digest = digest_parts(algorithm, &[b"aeordb.semantic-catalog-key.v1\0", &kind_bytes, &semantic_id]);
    bindings.push(SemanticCatalogBinding {
      kind,
      semantic_id: semantic_id.clone(),
      definition_object_id: object.object_id.clone(),
      owner_key: semantic_id,
      lookup_digest,
    });
    objects.push(object);
  }
  let mut node_count = 0;
  let (catalog_root, record_count) = build_semantic_catalog_node(algorithm, &bindings, 0, &mut objects, &mut node_count);
  let state = encode_semantic_state_object(
    &SemanticStateWriteV1 {
      required_capabilities: [0; 32],
      availability: SemanticAvailabilityV1::Complete {
        compiler_fingerprint: vec![0x11; hash_width],
        semantic_registry_fingerprint: vec![0x22; hash_width],
        catalog_root,
        catalog_record_count: record_count,
        catalog_node_count: node_count,
        definition_count,
        dependency_count: 0,
      },
    },
    algorithm,
  )
  .unwrap();
  objects.push(state.clone());
  CompleteSemanticGraph {
    objects,
    state,
    scope_id,
    value_store_id,
    field_index_id,
    scope_definition,
    value_store_definition,
    field_index_definition,
  }
}

fn publish_content_file_tree(
  publisher: &V4FirstAuthorityPublisher,
  algorithm: HashAlgorithm,
  expected_head_hash: Vec<u8>,
  name: &str,
  content: &[u8],
  transaction_id: [u8; 16],
) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
  publish_content_file_tree_with_chunk(publisher, algorithm, expected_head_hash, name, content, transaction_id, true)
}

#[allow(clippy::too_many_arguments)]
fn publish_content_file_tree_with_chunk(
  publisher: &V4FirstAuthorityPublisher,
  algorithm: HashAlgorithm,
  expected_head_hash: Vec<u8>,
  name: &str,
  content: &[u8],
  transaction_id: [u8; 16],
  publish_chunk: bool,
) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
  let timestamp = 1_700_000_000_500;
  let path = format!("/docs/{name}");
  let chunk_hash = digest_parts(algorithm, &[b"chunk:", content]);
  let record = FileRecord {
    path,
    content_type: Some("application/json".to_string()),
    total_size: content.len() as u64,
    created_at: timestamp,
    updated_at: timestamp,
    metadata: Vec::new(),
    content_hash: digest_parts(algorithm, &[content]),
    chunk_hashes: vec![chunk_hash.clone()],
  };
  let record_bytes = record.serialize_for_version(algorithm.hash_length(), 1).unwrap();
  let record_revision = digest_parts(algorithm, &[b"filec:", &record_bytes]);
  let docs_value = serialize_child_entries(
    &[ChildEntry {
      entry_type: EntryTypeV4::FileRecord.to_u8(),
      hash: record_revision.clone(),
      total_size: content.len() as u64,
      created_at: timestamp,
      updated_at: timestamp,
      name: name.to_string(),
      content_type: Some("application/json".to_string()),
      virtual_time: 1,
      node_id: 1,
    }],
    algorithm.hash_length(),
  )
  .unwrap();
  let docs_hash = digest_parts(algorithm, &[b"dirc:", &docs_value]);
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
  let record_entity = ImmutableEntityWriteV1 {
    entity_version: 1,
    entry_type: EntryTypeV4::FileRecord,
    flags: 0,
    key: &record_revision,
    stored_value: &record_bytes,
  };
  let docs_entity = ImmutableEntityWriteV1 {
    entity_version: 0,
    entry_type: EntryTypeV4::DirectoryIndex,
    flags: 0,
    key: &docs_hash,
    stored_value: &docs_value,
  };
  let root_entity = ImmutableEntityWriteV1 {
    entity_version: 0,
    entry_type: EntryTypeV4::DirectoryIndex,
    flags: 0,
    key: &root_hash,
    stored_value: &root_value,
  };
  let chunk_entity =
    ImmutableEntityWriteV1 { entity_version: 0, entry_type: EntryTypeV4::Chunk, flags: 0, key: &chunk_hash, stored_value: content };
  let entities =
    if publish_chunk { vec![chunk_entity, record_entity, docs_entity, root_entity] } else { vec![record_entity, docs_entity, root_entity] };
  publisher
    .publish_immutable_entity_batch(ImmutableEntityBatchPublicationRequestV1 {
      database_id: &[0x31; 16],
      entities: &entities,
      publication_timestamp_ms: timestamp as u64,
    })
    .unwrap();
  let selected_root = publisher
    .publish_successor_authority(&SuccessorAuthorityPublicationRequestV1 {
      database_id: [0x31; 16],
      transaction_id,
      created_at_ms: timestamp as u64 + 1,
      expected_head_hash,
      namespace_tree: PreparedNamespaceTreeV0 { root_hash, stored_value: root_value },
      semantic_state: semantic_state(algorithm, aeordb::engine::v4::namespace::SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured),
      required_capabilities: [0; 32],
      typed_closure_digest: digest_parts(algorithm, &[b"selected source content closure"]),
      authority_identity: b"HEAD".to_vec(),
    })
    .unwrap()
    .namespace_root
    .root_hash;
  (selected_root, record_revision, chunk_hash)
}

fn publish_complete_semantic_root(
  publisher: &V4FirstAuthorityPublisher,
  algorithm: HashAlgorithm,
  current_root_value: &[u8],
  expected_head_hash: Vec<u8>,
  graph: &CompleteSemanticGraph,
) -> Vec<u8> {
  let current_root = decode_namespace_root(current_root_value, algorithm).unwrap();
  let namespace_tree =
    publisher.load_immutable_entity_bounded(&current_root.namespace_tree_root, 1024 * 1024).unwrap().expect("selected namespace tree");
  publisher
    .publish_immutable_semantic_objects(ImmutableSemanticObjectBatchPublicationRequestV1 {
      database_id: &[0x31; 16],
      objects: &graph.objects,
      publication_timestamp_ms: 1_700_000_000_250,
    })
    .unwrap();
  publisher
    .publish_successor_authority(&SuccessorAuthorityPublicationRequestV1 {
      database_id: [0x31; 16],
      transaction_id: [0x65; 16],
      created_at_ms: 1_700_000_000_260,
      expected_head_hash,
      namespace_tree: PreparedNamespaceTreeV0 { root_hash: current_root.namespace_tree_root, stored_value: namespace_tree.stored_value },
      semantic_state: graph.state.clone(),
      required_capabilities: [0; 32],
      typed_closure_digest: digest_parts(algorithm, &[b"selected semantic catalog closure"]),
      authority_identity: b"HEAD".to_vec(),
    })
    .unwrap()
    .namespace_root
    .root_hash
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

fn publish_file_tree(
  publisher: &V4FirstAuthorityPublisher,
  algorithm: HashAlgorithm,
  expected_head_hash: Vec<u8>,
  file_record_version: u8,
  btree: bool,
  names: &[&str],
  corruption: FileTreeCorruption,
) -> (Vec<u8>, Vec<(String, Vec<u8>)>) {
  publish_file_tree_with_sizes(
    publisher,
    algorithm,
    expected_head_hash,
    file_record_version,
    btree,
    names,
    &vec![0; names.len()],
    corruption,
  )
}

#[allow(clippy::too_many_arguments)]
fn publish_file_tree_with_sizes(
  publisher: &V4FirstAuthorityPublisher,
  algorithm: HashAlgorithm,
  expected_head_hash: Vec<u8>,
  file_record_version: u8,
  btree: bool,
  names: &[&str],
  sizes: &[u64],
  corruption: FileTreeCorruption,
) -> (Vec<u8>, Vec<(String, Vec<u8>)>) {
  assert_eq!(names.len(), sizes.len());
  let timestamp = 1_700_000_000_400;
  let mut file_entities = Vec::new();
  let mut entries = Vec::new();
  let mut identities = Vec::new();
  for (index, (name, total_size)) in names.iter().zip(sizes).enumerate() {
    let path = format!("/docs/{name}");
    let mut record = FileRecord {
      path: path.clone(),
      content_type: Some("application/json".to_string()),
      total_size: *total_size,
      created_at: timestamp,
      updated_at: timestamp,
      metadata: Vec::new(),
      content_hash: Vec::new(),
      chunk_hashes: Vec::new(),
    };
    if file_record_version == 1 {
      record.content_hash = digest_parts(algorithm, &[b""]);
    }
    let record_bytes = record.serialize_for_version(algorithm.hash_length(), file_record_version).unwrap();
    let record_revision = digest_parts(algorithm, &[b"filec:", &record_bytes]);
    entries.push(ChildEntry {
      entry_type: if corruption == FileTreeCorruption::LastRole && index + 1 == names.len() {
        EntryTypeV4::DeletionRecord.to_u8()
      } else {
        EntryTypeV4::FileRecord.to_u8()
      },
      hash: record_revision.clone(),
      total_size: *total_size,
      created_at: timestamp,
      updated_at: if corruption == FileTreeCorruption::LastMetadata && index + 1 == names.len() { timestamp + 1 } else { timestamp },
      name: (*name).to_string(),
      content_type: Some("application/json".to_string()),
      virtual_time: index as u64 + 1,
      node_id: index as u64 + 1,
    });
    identities.push((path, record_revision.clone()));
    file_entities.push((record_revision, record_bytes));
  }
  let mut nested_directory_entities = Vec::new();
  let docs_value = if btree && entries.len() > 2 {
    let split = entries.len() / 2;
    let left_value = BTreeNode::Leaf(LeafNode { entries: entries[..split].to_vec() }).serialize(algorithm.hash_length()).unwrap();
    let right_entries = entries[split..].to_vec();
    let right_value = BTreeNode::Leaf(LeafNode { entries: right_entries.clone() }).serialize(algorithm.hash_length()).unwrap();
    let left_hash = digest_parts(algorithm, &[b"btree:", &left_value]);
    let right_hash = digest_parts(algorithm, &[b"btree:", &right_value]);
    let value = BTreeNode::Internal(InternalNode {
      keys: vec![right_entries[0].name.clone()],
      children: vec![left_hash.clone(), right_hash.clone()],
    })
    .serialize(algorithm.hash_length())
    .unwrap();
    nested_directory_entities.push((left_hash, left_value));
    nested_directory_entities.push((right_hash, right_value));
    value
  } else if btree {
    BTreeNode::Leaf(LeafNode { entries }).serialize(algorithm.hash_length()).unwrap()
  } else {
    serialize_child_entries(&entries, algorithm.hash_length()).unwrap()
  };
  let docs_domain = if btree { b"btree:".as_slice() } else { b"dirc:".as_slice() };
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
  let mut entity_data = file_entities
    .iter()
    .map(|(key, value)| (file_record_version, EntryTypeV4::FileRecord, key.as_slice(), value.as_slice()))
    .collect::<Vec<_>>();
  for (key, value) in &nested_directory_entities {
    entity_data.push((0, EntryTypeV4::DirectoryIndex, key.as_slice(), value.as_slice()));
  }
  entity_data.push((0, EntryTypeV4::DirectoryIndex, docs_hash.as_slice(), docs_value.as_slice()));
  entity_data.push((0, EntryTypeV4::DirectoryIndex, root_hash.as_slice(), root_value.as_slice()));
  let entities = entity_data
    .iter()
    .map(|(entity_version, entry_type, key, stored_value)| ImmutableEntityWriteV1 {
      entity_version: *entity_version,
      entry_type: *entry_type,
      flags: 0,
      key,
      stored_value,
    })
    .collect::<Vec<_>>();
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
      transaction_id: [0x64; 16],
      created_at_ms: timestamp as u64 + 1,
      expected_head_hash,
      namespace_tree: PreparedNamespaceTreeV0 { root_hash, stored_value: root_value },
      semantic_state: semantic_state(algorithm, aeordb::engine::v4::namespace::SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured),
      required_capabilities: [0; 32],
      typed_closure_digest: digest_parts(algorithm, &[b"read view file closure"]),
      authority_identity: b"HEAD".to_vec(),
    })
    .unwrap()
    .namespace_root
    .root_hash;
  (namespace_root, identities)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileTreeCorruption {
  None,
  LastMetadata,
  LastRole,
}

struct SelectedSourceFixture {
  _directory: tempfile::TempDir,
  path: PathBuf,
  source: Arc<NativeReadViewSourceV1>,
  memory: Arc<MemoryCoordinator>,
  pins: RootReadPinCoordinatorV1,
  cancellation: CancellationToken,
  view: ResolvedReadViewV1<ResolvedPathAuthorizationV1>,
  graph: CompleteSemanticGraph,
  field_name: String,
  chunk_offset: Option<(u64, u32)>,
}

impl SelectedSourceFixture {
  fn reader(&self) -> NativeSelectedNamespaceReaderV1<'_> {
    self
      .source
      .selected_namespace_reader(
        &self.view,
        NativeSelectedNamespaceLimitsV1::new(16, 1024 * 1024, u16::MAX as usize, 32, 100_000, 10_000).unwrap(),
      )
      .unwrap()
  }

  fn assert_released(self) {
    let Self { source, memory, pins, view, .. } = self;
    drop(view);
    drop(source);
    assert_eq!(pins.active_pin_count().unwrap(), 0);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

struct SelectedArtifactFixture {
  _directory: tempfile::TempDir,
  path: PathBuf,
  source: Arc<NativeReadViewSourceV1>,
  memory: Arc<MemoryCoordinator>,
  pins: RootReadPinCoordinatorV1,
  cancellation: CancellationToken,
  view: ResolvedReadViewV1<ResolvedPathAuthorizationV1>,
  catalog: RootAwareQueryFieldCatalogV1,
  scope_id: Vec<u8>,
  generation: QueryPlanningCoverageGenerationV1,
  posting_page: EncodedImmutableIndexArtifactV1,
  posting_root: EncodedImmutableIndexArtifactV1,
  scope_ordinal_root: EncodedImmutableIndexArtifactV1,
  nvt_descriptor: Option<IndexCoverageNvtDescriptorV1>,
  nvt_tile: Option<EncodedImmutableIndexArtifactV1>,
  nvt_manifest: Option<EncodedImmutableIndexArtifactV1>,
  target_coordinate: Option<u64>,
  target_posting_position: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectedArtifactLayoutV1 {
  SinglePage,
  ValidNvtPageHint,
  StaleNvtPageHint,
  TwoLevelNvt,
  CorruptNvtParent,
  LargeNvt,
  CorruptPostingParent,
  BrokenPostingLink,
}

impl SelectedArtifactLayoutV1 {
  fn has_nvt(self) -> bool {
    self != Self::SinglePage
  }

  fn has_three_posting_pages(self) -> bool {
    matches!(self, Self::TwoLevelNvt | Self::CorruptNvtParent | Self::LargeNvt | Self::CorruptPostingParent | Self::BrokenPostingLink)
  }

  fn has_two_level_posting(self) -> bool {
    self.has_three_posting_pages()
  }

  fn has_two_level_nvt(self) -> bool {
    matches!(self, Self::TwoLevelNvt | Self::CorruptNvtParent | Self::CorruptPostingParent | Self::BrokenPostingLink)
  }
}

impl SelectedArtifactFixture {
  fn reader(&self) -> NativeSelectedNamespaceReaderV1<'_> {
    self
      .source
      .selected_namespace_reader(
        &self.view,
        NativeSelectedNamespaceLimitsV1::new(16, 1024 * 1024, u16::MAX as usize, 32, 100_000, 10_000).unwrap(),
      )
      .unwrap()
  }

  fn assert_released(self) {
    let Self { source, memory, pins, view, .. } = self;
    drop(view);
    drop(source);
    assert_eq!(pins.active_pin_count().unwrap(), 0);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

fn selected_artifact_cursor_request<'a>(
  catalog: &'a RootAwareQueryFieldCatalogV1,
  scope_id: &'a [u8],
  selected_generation: &'a QueryPlanningCoverageGenerationV1,
  role: OrderedIndexRoleV1,
  neighbors: ArtifactPageNeighborModeV1,
  limits: ArtifactPageCursorLimitsV1,
) -> NativeSelectedArtifactCursorRequestV1<'a> {
  NativeSelectedArtifactCursorRequestV1 {
    catalog,
    scope_id,
    selected_generation,
    role,
    seek: ArtifactPageSeekV1::PageOrdinal(0),
    neighbors,
    limits,
  }
}

fn selected_artifact_root_request<'a>(
  fixture: &'a SelectedArtifactFixture,
  role: OrderedIndexRoleV1,
) -> NativeSelectedArtifactRootRequestV1<'a> {
  NativeSelectedArtifactRootRequestV1 {
    catalog: &fixture.catalog,
    scope_id: &fixture.scope_id,
    selected_generation: &fixture.generation,
    role,
  }
}

fn selected_artifact_fixture(algorithm: HashAlgorithm) -> SelectedArtifactFixture {
  selected_artifact_fixture_with_options(
    algorithm,
    0,
    true,
    false,
    [0; 32],
    all_capabilities_profile(),
    SelectedArtifactLayoutV1::SinglePage,
  )
}

fn selected_artifact_fixture_with_layout(algorithm: HashAlgorithm, layout: SelectedArtifactLayoutV1) -> SelectedArtifactFixture {
  selected_artifact_fixture_with_options(algorithm, 0, true, false, [0; 32], all_capabilities_profile(), layout)
}

fn selected_artifact_fixture_with_manifest_live_delta(algorithm: HashAlgorithm, manifest_live_delta: u64) -> SelectedArtifactFixture {
  selected_artifact_fixture_with_options(
    algorithm,
    manifest_live_delta,
    true,
    false,
    [0; 32],
    all_capabilities_profile(),
    SelectedArtifactLayoutV1::SinglePage,
  )
}

fn selected_partial_artifact_fixture(algorithm: HashAlgorithm) -> SelectedArtifactFixture {
  selected_artifact_fixture_with_options(
    algorithm,
    0,
    true,
    true,
    [0; 32],
    all_capabilities_profile(),
    SelectedArtifactLayoutV1::SinglePage,
  )
}

fn selected_stale_nvt_artifact_fixture(algorithm: HashAlgorithm) -> SelectedArtifactFixture {
  selected_artifact_fixture_with_options(
    algorithm,
    0,
    true,
    false,
    [0; 32],
    all_capabilities_profile(),
    SelectedArtifactLayoutV1::StaleNvtPageHint,
  )
}

fn selected_valid_nvt_artifact_fixture(algorithm: HashAlgorithm) -> SelectedArtifactFixture {
  selected_artifact_fixture_with_options(
    algorithm,
    0,
    true,
    false,
    [0; 32],
    all_capabilities_profile(),
    SelectedArtifactLayoutV1::ValidNvtPageHint,
  )
}

fn selected_artifact_fixture_with_unadmitted_capability(algorithm: HashAlgorithm) -> SelectedArtifactFixture {
  let required_reader_capabilities = CapabilitySetV1::from_bits([23]).unwrap().into_bytes();
  let baseline = CapabilitySetV1::v4_baseline();
  selected_artifact_fixture_with_options(
    algorithm,
    0,
    true,
    false,
    required_reader_capabilities,
    BinaryCapabilityProfileV1::new(baseline, baseline),
    SelectedArtifactLayoutV1::SinglePage,
  )
}

fn publish_selected_artifact_pointers(
  publisher: &V4FirstAuthorityPublisher,
  algorithm: HashAlgorithm,
  manifests: &[(ActivePointerKindV1, &EncodedImmutableIndexArtifactV1)],
) {
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let mut retirement = RetirementJournalOwnerV1::new_chain(
    algorithm,
    [0x31; 16],
    1,
    902,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  for (index, (kind, manifest)) in manifests.iter().enumerate() {
    let decoded = decode_index_manifest(&manifest.value, algorithm).unwrap();
    let pointer = encode_active_pointer(&ActivePointerWriteV1 {
      kind: *kind,
      hash_algorithm: algorithm,
      generation: decoded.generation,
      owner_id: decoded.owner_id,
      slot: 0,
      sequence: 1,
      target_manifest_hash: &manifest.key,
    })
    .unwrap();
    let timestamp = 1_700_000_000_350 + u64::try_from(index).unwrap();
    publisher
      .publish_index_active_pointer(
        IndexActivePointerPublicationRequestV1 {
          database_id: &[0x31; 16],
          pointer: &pointer,
          publication_timestamp_ms: timestamp,
          monotonic_now_ms: timestamp,
        },
        &mut retirement,
      )
      .unwrap();
  }
}

fn selected_artifact_fixture_with_options(
  algorithm: HashAlgorithm,
  manifest_live_delta: u64,
  publish_posting_page: bool,
  advance_target_root: bool,
  field_required_reader_capabilities: [u8; 32],
  capability_profile: BinaryCapabilityProfileV1,
  layout: SelectedArtifactLayoutV1,
) -> SelectedArtifactFixture {
  let (directory, path, publisher) = publisher(algorithm);
  let first = publisher.publish(&first_request(algorithm)).unwrap();
  let graph = complete_semantic_graph(algorithm);
  let selected_root =
    publish_complete_semantic_root(&publisher, algorithm, &first.namespace_root.value, first.namespace_root.root_hash, &graph);
  let posting_coordinates: &[u64] = if layout.has_three_posting_pages() {
    &[17, 29, 41]
  } else if layout == SelectedArtifactLayoutV1::StaleNvtPageHint {
    &[17, 29]
  } else {
    &[17]
  };
  let posting_records = posting_coordinates
    .iter()
    .enumerate()
    .map(|(index, coordinate)| {
      encode_posting_record(&PostingRecordV1 {
        tombstone: false,
        coordinate: *coordinate,
        document_ordinal: u64::try_from(index).unwrap() + 1,
        source_value_ordinal: 0,
        expansion_ordinal: 0,
        posting_key: &coordinate.to_le_bytes(),
      })
      .unwrap()
    })
    .collect::<Vec<_>>();
  let posting_pages = posting_records
    .iter()
    .enumerate()
    .map(|(index, record)| {
      let page_id = u64::try_from(index).unwrap() + 1;
      let previous_page_id =
        if page_id == 1 || layout == SelectedArtifactLayoutV1::BrokenPostingLink && page_id == 2 { 0 } else { page_id - 1 };
      let next_page_id = if index + 1 == posting_records.len() { 0 } else { page_id + 1 };
      encode_ordered_page(&OrderedPageWriteV1 {
        hash_algorithm: algorithm,
        role: OrderedIndexRoleV1::Posting,
        owner_id: &graph.field_index_id,
        generation: 7,
        page_id,
        previous_page_id,
        next_page_id,
        records: &[record.as_slice()],
      })
      .unwrap()
    })
    .collect::<Vec<_>>();
  let decoded_pages = posting_pages.iter().map(|page| decode_ordered_page(&page.value, algorithm).unwrap()).collect::<Vec<_>>();
  let posting_entries = posting_pages
    .iter()
    .zip(&decoded_pages)
    .map(|(page, decoded)| ArtifactDirectoryEntryWriteV1 {
      lower_fence: decoded.lower_fence,
      upper_fence: decoded.upper_fence,
      child_hash: &page.key,
      child_generation: decoded.generation,
      live_count: u64::from(decoded.live_count),
      tombstone_count: u64::from(decoded.tombstone_count),
      page_count: 1,
      logical_bytes: decoded.logical_live_bytes,
      minimum_page_id: decoded.page_id,
      maximum_page_id: decoded.page_id,
      physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    })
    .collect::<Vec<_>>();
  let posting_children = if layout.has_two_level_posting() {
    posting_entries
      .iter()
      .map(|entry| {
        encode_artifact_directory(&ArtifactDirectoryWriteV1 {
          hash_algorithm: algorithm,
          role: OrderedIndexRoleV1::Posting,
          owner_id: &graph.field_index_id,
          generation: 7,
          level: 0,
          entries: std::slice::from_ref(entry),
        })
        .unwrap()
      })
      .collect::<Vec<_>>()
  } else {
    Vec::new()
  };
  let posting_root = if posting_children.is_empty() {
    encode_artifact_directory(&ArtifactDirectoryWriteV1 {
      hash_algorithm: algorithm,
      role: OrderedIndexRoleV1::Posting,
      owner_id: &graph.field_index_id,
      generation: 7,
      level: 0,
      entries: &posting_entries,
    })
    .unwrap()
  } else {
    let decoded_children =
      posting_children.iter().map(|child| decode_artifact_directory(&child.value, algorithm).unwrap()).collect::<Vec<_>>();
    let mut root_entries = posting_children
      .iter()
      .zip(&decoded_children)
      .map(|(child, decoded)| ArtifactDirectoryEntryWriteV1 {
        lower_fence: decoded.lower_fence,
        upper_fence: decoded.upper_fence,
        child_hash: &child.key,
        child_generation: decoded.generation,
        live_count: decoded.live_count,
        tombstone_count: decoded.tombstone_count,
        page_count: decoded.page_count,
        logical_bytes: decoded.logical_bytes,
        minimum_page_id: decoded.minimum_page_id,
        maximum_page_id: decoded.maximum_page_id,
        physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
      })
      .collect::<Vec<_>>();
    if layout == SelectedArtifactLayoutV1::CorruptPostingParent {
      root_entries[1].child_generation -= 1;
    }
    encode_artifact_directory(&ArtifactDirectoryWriteV1 {
      hash_algorithm: algorithm,
      role: OrderedIndexRoleV1::Posting,
      owner_id: &graph.field_index_id,
      generation: 7,
      level: 1,
      entries: &root_entries,
    })
    .unwrap()
  };
  let posting_page = posting_pages[0].clone();
  let decoded_page = &decoded_pages[0];
  let decoded_root = decode_artifact_directory(&posting_root.value, algorithm).unwrap();
  let scope_identities = posting_coordinates
    .iter()
    .enumerate()
    .map(|(index, _)| {
      let document_ordinal = u64::try_from(index).unwrap() + 1;
      let path = format!("/docs/{document_ordinal}.json");
      let file_key = digest_parts(algorithm, &[b"file:", path.as_bytes()]);
      let record_revision_hash = digest_parts(algorithm, &[b"revision:", path.as_bytes()]);
      (document_ordinal, path, file_key, record_revision_hash)
    })
    .collect::<Vec<_>>();
  let scope_records = scope_identities
    .iter()
    .map(|(document_ordinal, path, file_key, record_revision_hash)| {
      encode_scope_document_record(
        &ScopeDocumentRecordV1 { tombstone: false, document_ordinal: *document_ordinal, file_key, record_revision_hash, path },
        algorithm,
      )
      .unwrap()
    })
    .collect::<Vec<_>>();
  let scope_record_refs = scope_records.iter().map(Vec::as_slice).collect::<Vec<_>>();
  let scope_ordinal_page = encode_ordered_page(&OrderedPageWriteV1 {
    hash_algorithm: algorithm,
    role: OrderedIndexRoleV1::ScopeOrdinal,
    owner_id: &graph.scope_id,
    generation: 7,
    page_id: 0,
    previous_page_id: 0,
    next_page_id: 0,
    records: &scope_record_refs,
  })
  .unwrap();
  let decoded_scope_page = decode_ordered_page(&scope_ordinal_page.value, algorithm).unwrap();
  let scope_ordinal_root = encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role: OrderedIndexRoleV1::ScopeOrdinal,
    owner_id: &graph.scope_id,
    generation: 7,
    level: 0,
    entries: &[ArtifactDirectoryEntryWriteV1 {
      lower_fence: decoded_scope_page.lower_fence,
      upper_fence: decoded_scope_page.upper_fence,
      child_hash: &scope_ordinal_page.key,
      child_generation: decoded_scope_page.generation,
      live_count: u64::from(decoded_scope_page.live_count),
      tombstone_count: u64::from(decoded_scope_page.tombstone_count),
      page_count: 1,
      logical_bytes: decoded_scope_page.logical_live_bytes,
      minimum_page_id: 0,
      maximum_page_id: 0,
      physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    }],
  })
  .unwrap();
  let decoded_scope_root = decode_artifact_directory(&scope_ordinal_root.value, algorithm).unwrap();
  let mut scope_reverse_records = scope_identities
    .iter()
    .map(|(document_ordinal, _, file_key, _)| {
      encode_scope_reverse_record(&ScopeReverseRecordV1 { document_ordinal: *document_ordinal, file_key }, algorithm).unwrap()
    })
    .collect::<Vec<_>>();
  scope_reverse_records.sort_by(|left, right| {
    let left = decode_ordered_record(left, algorithm, OrderedIndexRoleV1::ScopeReverse).unwrap();
    let right = decode_ordered_record(right, algorithm, OrderedIndexRoleV1::ScopeReverse).unwrap();
    compare_order_keys(
      algorithm,
      OrderedIndexRoleV1::ScopeReverse,
      &ordered_record_order_key(&left).unwrap(),
      &ordered_record_order_key(&right).unwrap(),
    )
    .unwrap()
  });
  let scope_reverse_record_refs = scope_reverse_records.iter().map(Vec::as_slice).collect::<Vec<_>>();
  let scope_reverse_page = encode_ordered_page(&OrderedPageWriteV1 {
    hash_algorithm: algorithm,
    role: OrderedIndexRoleV1::ScopeReverse,
    owner_id: &graph.scope_id,
    generation: 7,
    page_id: 0,
    previous_page_id: 0,
    next_page_id: 0,
    records: &scope_reverse_record_refs,
  })
  .unwrap();
  let decoded_reverse_page = decode_ordered_page(&scope_reverse_page.value, algorithm).unwrap();
  let scope_reverse_root = encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role: OrderedIndexRoleV1::ScopeReverse,
    owner_id: &graph.scope_id,
    generation: 7,
    level: 0,
    entries: &[ArtifactDirectoryEntryWriteV1 {
      lower_fence: decoded_reverse_page.lower_fence,
      upper_fence: decoded_reverse_page.upper_fence,
      child_hash: &scope_reverse_page.key,
      child_generation: decoded_reverse_page.generation,
      live_count: u64::from(decoded_reverse_page.live_count),
      tombstone_count: u64::from(decoded_reverse_page.tombstone_count),
      page_count: 1,
      logical_bytes: decoded_reverse_page.logical_live_bytes,
      minimum_page_id: 0,
      maximum_page_id: 0,
      physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    }],
  })
  .unwrap();
  let decoded_reverse_root = decode_artifact_directory(&scope_reverse_root.value, algorithm).unwrap();
  let coverage_epoch_id = [0x41; 16];
  let coverage =
    CoverageVersionV1 { source_namespace_root: &selected_root, coverage_epoch_id: &coverage_epoch_id, coverage_publication_sequence: 7 };
  let scope_manifest = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm: algorithm,
    generation: 7,
    owner_id: &graph.scope_id,
    body: IndexManifestBodyV1::ScopeCatalog(ScopeCatalogManifestBodyV1 {
      required_reader_capabilities: [0; 32],
      coverage: coverage.clone(),
      next_document_ordinal: u64::try_from(scope_records.len()).unwrap() + 1,
      ordinal_directory_root: Some(&scope_ordinal_root.key),
      reverse_directory_root: Some(&scope_reverse_root.key),
      live_document_count: decoded_scope_root.live_count,
      retained_tombstone_count: 0,
      ordinal_page_count: decoded_scope_root.page_count,
      reverse_page_count: decoded_reverse_root.page_count,
      scope_definition: &graph.scope_definition,
    }),
  })
  .unwrap();
  let value_manifest = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm: algorithm,
    generation: 7,
    owner_id: &graph.value_store_id,
    body: IndexManifestBodyV1::ValueStore(ValueStoreManifestBodyV1 {
      required_reader_capabilities: [0; 32],
      coverage: coverage.clone(),
      scope_catalog_manifest: &scope_manifest.key,
      value_directory_root: None,
      document_state_directory_root: None,
      next_page_id: 1,
      value_page_count: 0,
      state_page_count: 0,
      value_document_count: 0,
      unindexable_document_count: 0,
      live_value_count: 0,
      value_tombstone_count: 0,
      state_tombstone_count: 0,
      live_canonical_value_bytes: 0,
      value_store_definition: &graph.value_store_definition,
    }),
  })
  .unwrap();
  let field_manifest = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm: algorithm,
    generation: 7,
    owner_id: &graph.field_index_id,
    body: IndexManifestBodyV1::FieldIndex(FieldIndexManifestBodyV1 {
      required_reader_capabilities: field_required_reader_capabilities,
      coverage,
      value_store_manifest: &value_manifest.key,
      posting_directory_root: Some(&posting_root.key),
      document_state_directory_root: None,
      first_page_id: decoded_page.page_id,
      last_page_id: decoded_pages.last().unwrap().page_id,
      next_page_id: decoded_pages.last().unwrap().page_id + 1,
      posting_page_count: decoded_root.page_count,
      state_page_count: 0,
      live_posting_count: decoded_root.live_count + manifest_live_delta,
      posting_tombstone_count: decoded_root.tombstone_count,
      posting_document_count: u64::try_from(posting_pages.len()).unwrap(),
      unindexable_document_count: 0,
      state_tombstone_count: 0,
      live_canonical_posting_bytes: decoded_root.logical_bytes,
      field_index_definition: &graph.field_index_definition,
    }),
  })
  .unwrap();
  let nvt_hint_page_id = if layout == SelectedArtifactLayoutV1::StaleNvtPageHint {
    2
  } else if layout.has_three_posting_pages() {
    2
  } else {
    1
  };
  let nvt_entries = if layout == SelectedArtifactLayoutV1::LargeNvt {
    (0..128u32)
      .map(|relative_cell| NvtEntryWriteV1 {
        relative_cell,
        predecessor_page_id: Some(nvt_hint_page_id),
        successor_page_id: None,
        approximate_live_postings: 1,
        sample_coordinate: if relative_cell == 0 { 17 } else { u64::from(relative_cell) << 54 },
      })
      .collect::<Vec<_>>()
  } else {
    vec![NvtEntryWriteV1 {
      relative_cell: 0,
      predecessor_page_id: Some(nvt_hint_page_id),
      successor_page_id: None,
      approximate_live_postings: 1,
      sample_coordinate: 17,
    }]
  };
  let nvt_tile = layout.has_nvt().then(|| {
    encode_nvt_tile(&NvtTileWriteV1 {
      hash_algorithm: algorithm,
      owner_id: &graph.field_index_id,
      generation: 8,
      resolution: 1024,
      tile_start_cell: 0,
      tile_cell_count: 1024,
      basis_posting_generation: 7,
      entries: &nvt_entries,
    })
    .unwrap()
  });
  let nvt_leaf_directory = nvt_tile.as_ref().filter(|_| layout.has_two_level_nvt()).map(|tile| {
    let tile_start = 0u64.to_le_bytes();
    encode_artifact_directory(&ArtifactDirectoryWriteV1 {
      hash_algorithm: algorithm,
      role: OrderedIndexRoleV1::NvtTile,
      owner_id: &graph.field_index_id,
      generation: 8,
      level: 0,
      entries: &[ArtifactDirectoryEntryWriteV1 {
        lower_fence: &tile_start,
        upper_fence: &tile_start,
        child_hash: &tile.key,
        child_generation: 8,
        live_count: u64::try_from(nvt_entries.len()).unwrap(),
        tombstone_count: 0,
        page_count: 1,
        logical_bytes: u64::try_from(tile.value.len()).unwrap(),
        minimum_page_id: 0,
        maximum_page_id: 0,
        physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
      }],
    })
    .unwrap()
  });
  let nvt_root = nvt_tile.as_ref().map(|tile| {
    let tile_start = 0u64.to_le_bytes();
    if let Some(child) = nvt_leaf_directory.as_ref() {
      let decoded = decode_artifact_directory(&child.value, algorithm).unwrap();
      let child_generation = decoded.generation - u64::from(layout == SelectedArtifactLayoutV1::CorruptNvtParent);
      encode_artifact_directory(&ArtifactDirectoryWriteV1 {
        hash_algorithm: algorithm,
        role: OrderedIndexRoleV1::NvtTile,
        owner_id: &graph.field_index_id,
        generation: 8,
        level: 1,
        entries: &[ArtifactDirectoryEntryWriteV1 {
          lower_fence: decoded.lower_fence,
          upper_fence: decoded.upper_fence,
          child_hash: &child.key,
          child_generation,
          live_count: decoded.live_count,
          tombstone_count: decoded.tombstone_count,
          page_count: decoded.page_count,
          logical_bytes: decoded.logical_bytes,
          minimum_page_id: decoded.minimum_page_id,
          maximum_page_id: decoded.maximum_page_id,
          physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
        }],
      })
      .unwrap()
    } else {
      encode_artifact_directory(&ArtifactDirectoryWriteV1 {
        hash_algorithm: algorithm,
        role: OrderedIndexRoleV1::NvtTile,
        owner_id: &graph.field_index_id,
        generation: 8,
        level: 0,
        entries: &[ArtifactDirectoryEntryWriteV1 {
          lower_fence: &tile_start,
          upper_fence: &tile_start,
          child_hash: &tile.key,
          child_generation: 8,
          live_count: u64::try_from(nvt_entries.len()).unwrap(),
          tombstone_count: 0,
          page_count: 1,
          logical_bytes: u64::try_from(tile.value.len()).unwrap(),
          minimum_page_id: 0,
          maximum_page_id: 0,
          physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
        }],
      })
      .unwrap()
    }
  });
  let nvt_manifest = nvt_root.as_ref().map(|root| {
    encode_index_manifest(&IndexManifestWriteV1 {
      hash_algorithm: algorithm,
      generation: 8,
      owner_id: &graph.field_index_id,
      body: IndexManifestBodyV1::FieldNvt(FieldNvtManifestBodyV1 {
        required_reader_capabilities: [0; 32],
        tile_cells: 1024,
        resolution: 1024,
        basis_posting_generation: 7,
        basis_source_head_hash: &selected_root,
        tile_directory_root: Some(&root.key),
        tile_count: 1,
        populated_cell_count: u64::try_from(nvt_entries.len()).unwrap(),
        approximate_live_posting_count: u64::try_from(nvt_entries.len()).unwrap(),
      }),
    })
    .unwrap()
  });
  let nvt_descriptor = nvt_manifest.as_ref().map(|manifest| {
    let field = pin_field_index_v1(&field_manifest.value, algorithm).unwrap();
    let NvtBasisStatusV1::Usable(basis) = validate_field_nvt_basis_v1(&field, Some(&manifest.value)) else {
      panic!("selected native NVT fixture must have a usable basis");
    };
    IndexCoverageNvtDescriptorV1::try_from_pinned(&basis).unwrap()
  });
  let target_coordinate = layout.has_nvt().then_some(if layout.has_three_posting_pages() { 35 } else { 20 });
  let target_posting_position = target_coordinate.map(|coordinate| {
    let target = encode_posting_record(&PostingRecordV1 {
      tombstone: false,
      coordinate,
      document_ordinal: u64::MAX,
      source_value_ordinal: u32::MAX,
      expansion_ordinal: u32::MAX,
      posting_key: &coordinate.to_le_bytes(),
    })
    .unwrap();
    let decoded = decode_ordered_record(&target, algorithm, OrderedIndexRoleV1::Posting).unwrap();
    ordered_record_order_key(&decoded).unwrap()
  });
  let mut artifacts = vec![
    &scope_manifest,
    &value_manifest,
    &field_manifest,
    &posting_root,
    &scope_ordinal_root,
    &scope_ordinal_page,
    &scope_reverse_root,
    &scope_reverse_page,
  ];
  artifacts.extend(posting_children.iter());
  if publish_posting_page {
    artifacts.extend(posting_pages.iter());
  }
  if let (Some(tile), Some(root), Some(manifest)) = (nvt_tile.as_ref(), nvt_root.as_ref(), nvt_manifest.as_ref()) {
    artifacts.push(tile);
    if let Some(child) = nvt_leaf_directory.as_ref() {
      artifacts.push(child);
    }
    artifacts.extend([root, manifest]);
  }
  publisher
    .publish_index_artifacts(IndexArtifactBatchPublicationRequestV1 {
      database_id: &[0x31; 16],
      artifacts: &artifacts,
      publication_timestamp_ms: 1_700_000_000_300,
    })
    .unwrap();
  let mut pointer_manifests =
    vec![(ActivePointerKindV1::ScopeCatalog, &scope_manifest), (ActivePointerKindV1::FieldIndex, &field_manifest)];
  if let Some(nvt_manifest) = nvt_manifest.as_ref() {
    pointer_manifests.push((ActivePointerKindV1::FieldNvt, nvt_manifest));
  }
  publish_selected_artifact_pointers(&publisher, algorithm, &pointer_manifests);
  let target_root = if advance_target_root {
    let timestamp = 1_700_000_000_450;
    let namespace_tree = serialize_child_entries(
      &[ChildEntry {
        entry_type: EntryTypeV4::DirectoryIndex.to_u8(),
        hash: digest_parts(algorithm, &[b"dirc:"]),
        total_size: 0,
        created_at: timestamp as i64,
        updated_at: timestamp as i64,
        name: "future".to_owned(),
        content_type: None,
        virtual_time: 1,
        node_id: 1,
      }],
      algorithm.hash_length(),
    )
    .unwrap();
    publisher
      .publish_successor_authority(&SuccessorAuthorityPublicationRequestV1 {
        database_id: [0x31; 16],
        transaction_id: [0x66; 16],
        created_at_ms: timestamp,
        expected_head_hash: selected_root.clone(),
        namespace_tree: PreparedNamespaceTreeV0 {
          root_hash: digest_parts(algorithm, &[b"dirc:", &namespace_tree]),
          stored_value: namespace_tree,
        },
        semantic_state: graph.state.clone(),
        required_capabilities: [0; 32],
        typed_closure_digest: digest_parts(algorithm, &[b"selected artifact partial target closure"]),
        authority_identity: b"HEAD".to_vec(),
      })
      .unwrap()
      .namespace_root
      .root_hash
  } else {
    selected_root.clone()
  };
  let generation = QueryPlanningCoverageGenerationV1 {
    generation: 7,
    owner_id: graph.field_index_id.clone(),
    manifest_hash: field_manifest.key.clone(),
    source_namespace_root: selected_root.clone(),
    coverage_epoch_id,
    coverage_publication_sequence: 7,
    definition_fingerprint: field_definition_fingerprint(algorithm, &graph.field_index_definition),
    dependency_fingerprint: field_dependency_fingerprint(algorithm, &graph.scope_id, &graph.value_store_id),
    health: IndexCoverageGenerationHealthV1::Healthy,
  };
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(256 * 1024 * 1024, 512 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
  let source = Arc::new(NativeReadViewSourceV1::new(Arc::new(publisher), Arc::clone(&memory), 86_400_000));
  let pins = RootReadPinCoordinatorV1::new(Arc::clone(&memory), algorithm, 8, 16).unwrap();
  let current = CurrentReadAuthorizationV1::new(
    CurrentPathAuthorizationV1::for_root("/", CrudlifyOp::List),
    ReadViewCredentialKindV1::Ordinary,
    ReadViewConcealmentV1::Conceal,
  );
  let authorizer = ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), source.as_ref().clone());
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), pins.clone(), capability_profile);
  let cancellation = CancellationToken::new();
  let view = resolver.resolve(ReadViewSelectorV1::ExplicitRoot(&target_root), &authorizer, &cancellation).unwrap();
  let reader = source
    .selected_namespace_reader(
      &view,
      NativeSelectedNamespaceLimitsV1::new(16, 1024 * 1024, u16::MAX as usize, 32, 100_000, 10_000).unwrap(),
    )
    .unwrap();
  let selected = reader.load_planner_catalogs("/", &["@hash"], default_native_selected_semantic_limits_v1()).unwrap();
  let mut catalog = selected.catalogs()[0].clone();
  let candidate = catalog.scopes[0]
    .indexes
    .iter_mut()
    .find(|candidate| candidate.index_id == generation.owner_id)
    .expect("selected semantic catalog contains the indexed @hash strategy");
  candidate.selected_generation = Some(generation.clone());
  drop(selected);
  drop(reader);
  SelectedArtifactFixture {
    _directory: directory,
    path,
    source,
    memory,
    pins,
    cancellation,
    view,
    catalog,
    scope_id: graph.scope_id,
    generation,
    posting_page,
    posting_root,
    scope_ordinal_root,
    nvt_descriptor,
    nvt_tile,
    nvt_manifest,
    target_coordinate,
    target_posting_position,
  }
}

fn corrupt_artifact_checksum(fixture: &SelectedArtifactFixture, key: &[u8]) {
  let locator = fixture.source.publisher().locator(key).unwrap().unwrap();
  let checksum_offset = locator.offset + u64::from(locator.total_length) - 1;
  let mut file = OpenOptions::new().read(true).write(true).open(&fixture.path).unwrap();
  file.seek(SeekFrom::Start(checksum_offset)).unwrap();
  let mut checksum_byte = [0; 1];
  file.read_exact(&mut checksum_byte).unwrap();
  checksum_byte[0] ^= 0xff;
  file.seek(SeekFrom::Start(checksum_offset)).unwrap();
  file.write_all(&checksum_byte).unwrap();
  file.sync_all().unwrap();
}

fn selected_artifact_coverage_snapshot(fixture: &SelectedArtifactFixture) -> Arc<IndexCoverageRegistrySnapshotV1> {
  let registry = IndexCoverageRegistryV1::new(
    fixture.view.hash_algorithm(),
    fixture.view.database_id(),
    CapabilitySetV1::from_bits(0..24).unwrap(),
    IndexCoverageRegistryOptionsV1::new(8, 8 * 1024 * 1024).unwrap(),
    Arc::clone(&fixture.memory),
  )
  .unwrap();
  let requests = [
    IndexCoverageRegistryOwnerRequestV1::new(
      IndexCoverageRegistryOwnerKindV1::ScopeCatalog,
      fixture.scope_id.clone(),
      IndexCoverageGenerationHealthV1::Healthy,
    )
    .unwrap(),
    IndexCoverageRegistryOwnerRequestV1::new(
      IndexCoverageRegistryOwnerKindV1::FieldIndex,
      fixture.generation.owner_id.clone(),
      IndexCoverageGenerationHealthV1::Healthy,
    )
    .unwrap(),
  ];
  let mut source = FirstAuthorityIndexCoverageRegistrySourceV1::new(Arc::clone(fixture.source.publisher())).unwrap();
  registry.refresh(&mut source, &requests, &fixture.cancellation).unwrap()
}

fn truncate_artifact_source(fixture: &SelectedArtifactFixture, key: &[u8]) {
  let locator = fixture.source.publisher().locator(key).unwrap().unwrap();
  let file = OpenOptions::new().write(true).open(&fixture.path).unwrap();
  file.set_len(locator.offset).unwrap();
  file.sync_all().unwrap();
}

fn assert_selected_nvt_exact_fallback(
  fixture: SelectedArtifactFixture,
  limits: ArtifactPageCursorLimitsV1,
  reason: NativeSelectedNvtFallbackReasonV1,
) {
  let reader = fixture.reader();
  let selected = reader
    .seek_posting_page(&NativeSelectedPostingSeekRequestV1 {
      catalog: &fixture.catalog,
      scope_id: &fixture.scope_id,
      selected_generation: &fixture.generation,
      nvt_descriptor: fixture.nvt_descriptor.as_ref(),
      target_coordinate: fixture.target_coordinate.unwrap(),
      target_posting_position: fixture.target_posting_position.as_deref().unwrap(),
      neighbors: ArtifactPageNeighborModeV1::Both,
      limits,
    })
    .unwrap()
    .unwrap();
  assert_eq!(selected.source(), NativeSelectedPostingSeekSourceV1::ExactDirectory);
  let fallback = selected.nvt_fallback().unwrap();
  assert_eq!(fallback.reason(), reason);
  assert!(fallback.diagnostic_code().is_some());
  assert_eq!(decode_ordered_page(selected.cursor().cursor().page(), fixture.view.hash_algorithm()).unwrap().page_id, 2);
  drop(selected);
  drop(reader);
  fixture.assert_released();
}

fn selected_source_fixture(value_store_fixture: &str, scope_glob: Option<&str>, body: &[u8], publish_chunk: bool) -> SelectedSourceFixture {
  selected_source_fixture_with_scopes(
    value_store_fixture,
    semantic_scope_definition(HashAlgorithm::Blake3_256, "/docs", scope_glob),
    &[],
    body,
    publish_chunk,
  )
}

fn selected_source_fixture_with_scopes(
  value_store_fixture: &str,
  primary_scope: Vec<u8>,
  extra_scopes: &[Vec<u8>],
  body: &[u8],
  publish_chunk: bool,
) -> SelectedSourceFixture {
  let algorithm = HashAlgorithm::Blake3_256;
  let (directory, path, publisher) = publisher(algorithm);
  let first = publisher.publish(&first_request(algorithm)).unwrap();
  let (content_root, _, chunk_hash) = publish_content_file_tree_with_chunk(
    &publisher,
    algorithm,
    first.namespace_root.root_hash,
    "messages.json",
    body,
    [0x68; 16],
    publish_chunk,
  );
  let chunk_offset = publisher.locator(&chunk_hash).unwrap().map(|locator| (locator.offset, locator.total_length));
  let content_root_value = publisher.load_immutable_entity_bounded(&content_root, 1024 * 1024).unwrap().unwrap().stored_value;
  let graph =
    complete_semantic_graph_with_extra_scopes(algorithm, primary_scope, value_store_fixture, "typed_exact_blake3_v1", extra_scopes);
  let field_name = match value_store_fixture {
    "metadata-hash-corrected" => "@hash",
    "json-corrected" => "messages",
    "mapper-corrected" => "summary",
    other => panic!("selected source test fixture has no field mapping for {other}"),
  }
  .to_string();
  let selected_root = publish_complete_semantic_root(&publisher, algorithm, &content_root_value, content_root, &graph);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(256 * 1024 * 1024, 512 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
  let source = Arc::new(NativeReadViewSourceV1::new(Arc::new(publisher), Arc::clone(&memory), 86_400_000));
  let pins = RootReadPinCoordinatorV1::new(Arc::clone(&memory), algorithm, 8, 16).unwrap();
  let current = CurrentReadAuthorizationV1::new(
    CurrentPathAuthorizationV1::for_root("/docs/", CrudlifyOp::List),
    ReadViewCredentialKindV1::Ordinary,
    ReadViewConcealmentV1::Conceal,
  );
  let authorizer = ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), source.as_ref().clone());
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), pins.clone(), all_capabilities_profile());
  let cancellation = CancellationToken::new();
  let view = resolver.resolve(ReadViewSelectorV1::ExplicitRoot(&selected_root), &authorizer, &cancellation).unwrap();
  SelectedSourceFixture { _directory: directory, path, source, memory, pins, cancellation, view, graph, field_name, chunk_offset }
}

struct NativePartitionFixture {
  _directory: tempfile::TempDir,
  source: NativeAuthoritativeFieldPartitionSourceV1,
  backing_source: Arc<NativeReadViewSourceV1>,
  memory: Arc<MemoryCoordinator>,
  pins: RootReadPinCoordinatorV1,
  cancellation: CancellationToken,
  selected_root: Vec<u8>,
  publication_sequence: u64,
  scope_id: Vec<u8>,
  field_name: String,
  maximum_documents: u64,
  plan: CompiledRootAwareQueryPlanV1,
}

impl NativePartitionFixture {
  fn open_cursor(
    &mut self,
    maximum_path_bytes: u64,
  ) -> Box<dyn aeordb::engine::v4::query_executor::QueryAuthoritativeFieldPartitionCursorV1> {
    let scope_ids = [self.scope_id.as_slice()];
    self
      .source
      .open_field_partition(QueryExecutionFieldPartitionOpenRequestV1 {
        selected_namespace_root: &self.selected_root,
        publication_sequence: self.publication_sequence,
        query_path: "/docs",
        field_name: &self.field_name,
        scope_ids: &scope_ids,
        maximum_documents: self.maximum_documents,
        maximum_values_per_document: 16,
        maximum_canonical_value_bytes_per_document: 1024 * 1024,
        maximum_path_bytes,
        cancellation: &self.cancellation,
      })
      .unwrap()
  }

  fn assert_released(self) {
    let Self { source, backing_source, memory, pins, .. } = self;
    drop(source);
    drop(backing_source);
    assert_eq!(pins.active_pin_count().unwrap(), 0);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

struct NativeCandidateArtifactFixture {
  _directory: tempfile::TempDir,
  path: PathBuf,
  source: NativeAuthoritativeFieldPartitionSourceV1,
  backing_source: Arc<NativeReadViewSourceV1>,
  memory: Arc<MemoryCoordinator>,
  pins: RootReadPinCoordinatorV1,
  cancellation: CancellationToken,
  plan: CompiledRootAwareQueryPlanV1,
  scope_id: Vec<u8>,
  generation: QueryPlanningCoverageGenerationV1,
  semantic_state_root: Vec<u8>,
  posting_page: EncodedImmutableIndexArtifactV1,
  posting_root: EncodedImmutableIndexArtifactV1,
  scope_ordinal_root: EncodedImmutableIndexArtifactV1,
}

impl NativeCandidateArtifactFixture {
  fn candidate(&self) -> &aeordb::engine::v4::query_planner::CompiledQueryIndexCandidateV1 {
    self.plan.predicates()[0].scopes()[0]
      .candidates()
      .iter()
      .find(|candidate| candidate.selected_generation().is_some())
      .expect("candidate artifact fixture has one planner-selected generation")
  }

  fn advance_mutable_authority(&self) {
    let namespace_tree = serialize_child_entries(
      &[ChildEntry {
        entry_type: EntryTypeV4::DirectoryIndex.to_u8(),
        hash: digest_parts(self.plan.hash_algorithm(), &[b"dirc:"]),
        total_size: 0,
        created_at: 1_700_000_000_710,
        updated_at: 1_700_000_000_710,
        name: "candidate-race".to_owned(),
        content_type: None,
        virtual_time: 1,
        node_id: 1,
      }],
      self.plan.hash_algorithm().hash_length(),
    )
    .unwrap();
    let namespace_tree_root = digest_parts(self.plan.hash_algorithm(), &[b"dirc:", &namespace_tree]);
    let semantic_state = self.backing_source.publisher().load_semantic_object(0x0001, &self.semantic_state_root).unwrap().unwrap();
    let successor = self
      .backing_source
      .publisher()
      .publish_successor_authority(&SuccessorAuthorityPublicationRequestV1 {
        database_id: [0x31; 16],
        transaction_id: [0x67; 16],
        created_at_ms: 1_700_000_000_710,
        expected_head_hash: self.plan.selected_namespace_root().to_vec(),
        namespace_tree: PreparedNamespaceTreeV0 { root_hash: namespace_tree_root, stored_value: namespace_tree },
        semantic_state: EncodedSemanticObjectV1 { object_id: self.semantic_state_root.clone(), value: semantic_state },
        required_capabilities: [0; 32],
        typed_closure_digest: digest_parts(self.plan.hash_algorithm(), &[b"candidate mutable authority advance"]),
        authority_identity: b"HEAD".to_vec(),
      })
      .unwrap();
    assert_ne!(successor.namespace_root.root_hash, self.plan.selected_namespace_root());
    let pointer = encode_active_pointer(&ActivePointerWriteV1 {
      kind: ActivePointerKindV1::FieldIndex,
      hash_algorithm: self.plan.hash_algorithm(),
      generation: self.generation.generation,
      owner_id: &self.generation.owner_id,
      slot: 1,
      sequence: 2,
      target_manifest_hash: &self.generation.manifest_hash,
    })
    .unwrap();
    let mut retirement = RetirementJournalOwnerV1::new_chain(
      self.plan.hash_algorithm(),
      [0x31; 16],
      1,
      903,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &self.cancellation,
      &self.memory,
    )
    .unwrap();
    self
      .backing_source
      .publisher()
      .publish_index_active_pointer(
        IndexActivePointerPublicationRequestV1 {
          database_id: &[0x31; 16],
          pointer: &pointer,
          publication_timestamp_ms: 1_700_000_000_720,
          monotonic_now_ms: 1_700_000_000_720,
        },
        &mut retirement,
      )
      .unwrap();
  }

  fn corrupt_artifact_checksum(&self, key: &[u8]) {
    let locator = self.backing_source.publisher().locator(key).unwrap().unwrap();
    let checksum_offset = locator.offset + u64::from(locator.total_length) - 1;
    let mut file = OpenOptions::new().read(true).write(true).open(&self.path).unwrap();
    file.seek(SeekFrom::Start(checksum_offset)).unwrap();
    let mut byte = [0; 1];
    file.read_exact(&mut byte).unwrap();
    byte[0] ^= 0xff;
    file.seek(SeekFrom::Start(checksum_offset)).unwrap();
    file.write_all(&byte).unwrap();
    file.sync_all().unwrap();
  }

  fn assert_released(self) {
    let Self { source, backing_source, memory, pins, .. } = self;
    drop(source);
    drop(backing_source);
    assert_eq!(pins.active_pin_count().unwrap(), 0);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

fn native_candidate_artifact_fixture(algorithm: HashAlgorithm, partial: bool) -> NativeCandidateArtifactFixture {
  let fixture = if partial { selected_partial_artifact_fixture(algorithm) } else { selected_artifact_fixture(algorithm) };
  let snapshot = selected_artifact_coverage_snapshot(&fixture);
  let reader = fixture.reader();
  let mut semantic_catalog = reader.load_planner_catalogs("/", &["@hash"], default_native_selected_semantic_limits_v1()).unwrap();
  reader.bind_planner_coverage(&mut semantic_catalog, &snapshot).unwrap();
  drop(reader);
  drop(snapshot);
  let context = QueryPlanningContextV1::from_resolved_view(&fixture.view).unwrap();
  let expression = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@hash".to_string(),
    operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("ab".repeat(algorithm.hash_length()))),
  });
  let plan = plan_root_aware_query_v1(&QueryPlanningRequestV1 {
    context: &context,
    query_path: "/",
    expression: &expression,
    catalogs: semantic_catalog.catalogs(),
    sort_fields: &[],
    aggregate_fields: &[],
    group_fields: &[],
    result_limit: 20,
    limits: default_query_planning_limits_v1(),
    is_cancelled: &|| false,
  })
  .unwrap();
  let semantic_state_root = fixture.view.authority().root.semantic_state_root.clone();
  let SelectedArtifactFixture {
    _directory,
    path,
    source: backing_source,
    memory,
    pins,
    cancellation,
    view,
    scope_id,
    generation,
    posting_page,
    posting_root,
    scope_ordinal_root,
    ..
  } = fixture;
  let limits = NativeAuthoritativeFieldPartitionLimitsV1::new(
    NativeSelectedNamespaceLimitsV1::new(16, 1024 * 1024, u16::MAX as usize, 32, 100_000, 10_000).unwrap(),
    8,
    64 * 1024 * 1024,
    8 * 1024 * 1024,
    2,
    2,
  )
  .unwrap();
  let source = NativeAuthoritativeFieldPartitionSourceV1::build(
    backing_source.as_ref().clone(),
    view,
    semantic_catalog,
    "/",
    _directory.path(),
    limits,
    &cancellation,
  )
  .unwrap();
  NativeCandidateArtifactFixture {
    _directory,
    path,
    source,
    backing_source,
    memory,
    pins,
    cancellation,
    plan,
    scope_id,
    generation,
    semantic_state_root,
    posting_page,
    posting_root,
    scope_ordinal_root,
  }
}

fn native_partition_fixture(
  value_store_fixture: &str,
  primary_scope: Vec<u8>,
  extra_scopes: &[Vec<u8>],
  body: &[u8],
) -> NativePartitionFixture {
  let fixture = selected_source_fixture_with_scopes(value_store_fixture, primary_scope, extra_scopes, body, true);
  let reader = fixture.reader();
  let semantic_catalog =
    reader.load_planner_catalogs("/docs", &[fixture.field_name.as_str()], default_native_selected_semantic_limits_v1()).unwrap();
  drop(reader);
  let SelectedSourceFixture { _directory, source: backing_source, memory, pins, cancellation, view, graph, field_name, .. } = fixture;
  let selected_root = view.root_metadata().hash.clone();
  let publication_sequence = view.authority().admission.publication_sequence;
  let context = QueryPlanningContextV1::from_resolved_view(&view).unwrap();
  let expression = QueryExpressionV1::And(Vec::new());
  let plan = plan_root_aware_query_v1(&QueryPlanningRequestV1 {
    context: &context,
    query_path: "/docs",
    expression: &expression,
    catalogs: &[],
    sort_fields: &[],
    aggregate_fields: &[],
    group_fields: &[],
    result_limit: 20,
    limits: default_query_planning_limits_v1(),
    is_cancelled: &|| false,
  })
  .unwrap();
  let limits = NativeAuthoritativeFieldPartitionLimitsV1::new(
    NativeSelectedNamespaceLimitsV1::new(1, 1024 * 1024, u16::MAX as usize, 32, 100_000, 10_000).unwrap(),
    8,
    64 * 1024 * 1024,
    8 * 1024 * 1024,
    2,
    2,
  )
  .unwrap();
  let source = NativeAuthoritativeFieldPartitionSourceV1::build(
    backing_source.as_ref().clone(),
    view,
    semantic_catalog,
    "/docs",
    _directory.path(),
    limits,
    &cancellation,
  )
  .unwrap();
  NativePartitionFixture {
    _directory,
    source,
    backing_source,
    memory,
    pins,
    cancellation,
    selected_root,
    publication_sequence,
    scope_id: graph.scope_id,
    field_name,
    maximum_documents: 8,
    plan,
  }
}

fn native_partition_many_fixture(
  algorithm: HashAlgorithm,
  file_record_version: u8,
  btree: bool,
  names: &[&str],
) -> (NativePartitionFixture, Vec<(Vec<u8>, Vec<u8>, String)>) {
  native_partition_many_fixture_configured(
    algorithm,
    file_record_version,
    btree,
    names,
    &vec![0; names.len()],
    "@hash",
    false,
    semantic_scope_definition(algorithm, "/docs", Some("*.json")),
    &[],
    None,
  )
}

fn native_auxiliary_size_fixture(
  algorithm: HashAlgorithm,
  names: &[&str],
  sizes: &[u64],
) -> (NativePartitionFixture, Vec<(Vec<u8>, Vec<u8>, String)>) {
  native_partition_many_fixture_configured(
    algorithm,
    1,
    true,
    names,
    sizes,
    "@size",
    true,
    semantic_scope_definition(algorithm, "/docs", Some("*.json")),
    &[],
    None,
  )
}

fn native_authoritative_size_fixture(
  algorithm: HashAlgorithm,
  names: &[&str],
  sizes: &[u64],
  minimum_size: u64,
) -> (NativePartitionFixture, Vec<(Vec<u8>, Vec<u8>, String)>) {
  native_partition_many_fixture_configured(
    algorithm,
    1,
    true,
    names,
    sizes,
    "@size",
    true,
    semantic_scope_definition(algorithm, "/docs", Some("*.json")),
    &[],
    Some(minimum_size),
  )
}

fn native_auxiliary_size_fieldless_fixture(algorithm: HashAlgorithm) -> (NativePartitionFixture, Vec<(Vec<u8>, Vec<u8>, String)>) {
  let nearer = [semantic_scope_definition(algorithm, "/docs", None)];
  native_partition_many_fixture_configured(
    algorithm,
    1,
    false,
    &["fieldless.json"],
    &[17],
    "@size",
    true,
    semantic_scope_definition(algorithm, "/", Some("**/*.json")),
    &nearer,
    None,
  )
}

#[allow(clippy::too_many_arguments)]
fn native_partition_many_fixture_configured(
  algorithm: HashAlgorithm,
  file_record_version: u8,
  btree: bool,
  names: &[&str],
  sizes: &[u64],
  field_name: &str,
  size_auxiliary: bool,
  primary_scope: Vec<u8>,
  extra_scopes: &[Vec<u8>],
  predicate_minimum: Option<u64>,
) -> (NativePartitionFixture, Vec<(Vec<u8>, Vec<u8>, String)>) {
  let (directory, _path, publisher) = publisher(algorithm);
  let first = publisher.publish(&first_request(algorithm)).unwrap();
  let (content_root, identities) = publish_file_tree_with_sizes(
    &publisher,
    algorithm,
    first.namespace_root.root_hash,
    file_record_version,
    btree,
    names,
    sizes,
    FileTreeCorruption::None,
  );
  let content_root_value = publisher.load_immutable_entity_bounded(&content_root, 1024 * 1024).unwrap().unwrap().stored_value;
  let graph = if size_auxiliary {
    complete_size_semantic_graph_with_extra_scopes(algorithm, primary_scope, extra_scopes)
  } else {
    complete_semantic_graph_with_extra_scopes(algorithm, primary_scope, "metadata-hash-corrected", "typed_exact_blake3_v1", extra_scopes)
  };
  let selected_root = publish_complete_semantic_root(&publisher, algorithm, &content_root_value, content_root, &graph);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(256 * 1024 * 1024, 512 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
  let backing_source = Arc::new(NativeReadViewSourceV1::new(Arc::new(publisher), Arc::clone(&memory), 86_400_000));
  let pins = RootReadPinCoordinatorV1::new(Arc::clone(&memory), algorithm, 8, 16).unwrap();
  let current = CurrentReadAuthorizationV1::new(
    CurrentPathAuthorizationV1::for_root("/docs/", CrudlifyOp::List),
    ReadViewCredentialKindV1::Ordinary,
    ReadViewConcealmentV1::Conceal,
  );
  let authorizer =
    ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), backing_source.as_ref().clone());
  let resolver = ReadViewResolverV1::new(Arc::clone(&backing_source), pins.clone(), all_capabilities_profile());
  let cancellation = CancellationToken::new();
  let view = resolver.resolve(ReadViewSelectorV1::ExplicitRoot(&selected_root), &authorizer, &cancellation).unwrap();
  let publication_sequence = view.authority().admission.publication_sequence;
  let reader = backing_source
    .selected_namespace_reader(&view, NativeSelectedNamespaceLimitsV1::new(1, 1024 * 1024, u16::MAX as usize, 32, 100_000, 10_000).unwrap())
    .unwrap();
  let semantic_catalog = reader.load_planner_catalogs("/docs", &[field_name], default_native_selected_semantic_limits_v1()).unwrap();
  drop(reader);
  let context = QueryPlanningContextV1::from_resolved_view(&view).unwrap();
  let expression = predicate_minimum.map_or_else(
    || QueryExpressionV1::And(Vec::new()),
    |minimum| {
      QueryExpressionV1::Field(QueryPredicateV1 {
        field_name: "@size".to_string(),
        operation: QueryPredicateOperationV1::Gt(CanonicalConfigValueV1::Unsigned(minimum)),
      })
    },
  );
  let plan = if size_auxiliary {
    let sort_fields = [QuerySortFieldV1 { field_name: "@size".to_string(), direction: QuerySortDirectionV1::Ascending }];
    let aggregate_fields = [
      QueryAggregateFieldV1 { field_name: "@size".to_string(), kind: QueryAggregateKindV1::Count },
      QueryAggregateFieldV1 { field_name: "@size".to_string(), kind: QueryAggregateKindV1::Sum },
    ];
    let group_fields = ["@size".to_string()];
    plan_root_aware_query_v1(&QueryPlanningRequestV1 {
      context: &context,
      query_path: "/docs",
      expression: &expression,
      catalogs: semantic_catalog.catalogs(),
      sort_fields: &sort_fields,
      aggregate_fields: &aggregate_fields,
      group_fields: &group_fields,
      result_limit: 20,
      limits: default_query_planning_limits_v1(),
      is_cancelled: &|| false,
    })
    .unwrap()
  } else {
    plan_root_aware_query_v1(&QueryPlanningRequestV1 {
      context: &context,
      query_path: "/docs",
      expression: &expression,
      catalogs: &[],
      sort_fields: &[],
      aggregate_fields: &[],
      group_fields: &[],
      result_limit: 20,
      limits: default_query_planning_limits_v1(),
      is_cancelled: &|| false,
    })
    .unwrap()
  };
  let maximum_documents = u64::try_from(names.len()).unwrap();
  let limits = NativeAuthoritativeFieldPartitionLimitsV1::new(
    NativeSelectedNamespaceLimitsV1::new(1, 1024 * 1024, u16::MAX as usize, 32, 100_000, 10_000).unwrap(),
    maximum_documents,
    64 * 1024 * 1024,
    8 * 1024 * 1024,
    2,
    2,
  )
  .unwrap();
  let source = NativeAuthoritativeFieldPartitionSourceV1::build(
    backing_source.as_ref().clone(),
    view,
    semantic_catalog,
    "/docs",
    directory.path(),
    limits,
    &cancellation,
  )
  .unwrap();
  let expected = identities
    .into_iter()
    .map(|(path, revision)| (digest_parts(algorithm, &[b"file:", path.as_bytes()]), revision, path))
    .collect::<Vec<_>>();
  (
    NativePartitionFixture {
      _directory: directory,
      source,
      backing_source,
      memory,
      pins,
      cancellation,
      selected_root,
      publication_sequence,
      scope_id: graph.scope_id,
      field_name: field_name.to_string(),
      maximum_documents,
      plan,
    },
    expected,
  )
}

struct ParsedNullParser;

impl IndexParserExecutorV1 for ParsedNullParser {
  fn parse(&self, _request: IndexParserExecutionRequestV1<'_>) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1> {
    Ok(IndexParserOutcomeV1::Parsed(CanonicalConfigValueV1::Null))
  }
}

struct DependencyUnavailableParser;

impl IndexParserExecutorV1 for DependencyUnavailableParser {
  fn parse(&self, _request: IndexParserExecutionRequestV1<'_>) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1> {
    Err(IndexParserExecutionErrorV1::dependency_unavailable(
      "selected_test_parser_dependency",
      "selected parser dependency is deliberately unavailable",
    ))
  }
}

struct DeterministicFailureParser;

impl IndexParserExecutorV1 for DeterministicFailureParser {
  fn parse(&self, _request: IndexParserExecutionRequestV1<'_>) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1> {
    Ok(IndexParserOutcomeV1::DeterministicUnindexable(IndexParserDeterministicFailureV1::malformed_document(
      b"selected deterministic parser evidence".to_vec(),
      1,
    )))
  }
}

struct CancelAfterParser<'token> {
  cancellation: &'token CancellationToken,
}

impl IndexParserExecutorV1 for CancelAfterParser<'_> {
  fn parse(&self, _request: IndexParserExecutionRequestV1<'_>) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1> {
    self.cancellation.cancel();
    Ok(IndexParserOutcomeV1::Parsed(CanonicalConfigValueV1::Null))
  }
}

struct SelectedValueMapper;

impl PluginMapperExecutorV1 for SelectedValueMapper {
  fn invoke(&self, request: PluginMapperRequestV1<'_>) -> SourceOperationalResultV1<PluginMapperOutcomeV1> {
    assert_eq!(request.dependency_ordinal, 2);
    assert_eq!(request.document, &CanonicalConfigValueV1::Null);
    let value = encode_canonical_value(&CanonicalConfigValueV1::String("mapped".to_string()), CanonicalValueBounds::SOURCE_VALUE).unwrap();
    Ok(PluginMapperOutcomeV1::Values(vec![value]))
  }
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
fn selected_semantic_catalog_uses_the_historical_root_and_query_memory_after_head_advances() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    assert_selected_semantic_catalog_uses_historical_root(algorithm);
  }
}

#[test]
fn selected_source_evaluation_reads_historical_chunks_and_retains_exact_query_memory() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, _path, publisher) = publisher(algorithm);
    let first = publisher.publish(&first_request(algorithm)).unwrap();
    let historical_body = br#"{"messages":[{"user":"historical"}]}"#;
    let current_body = br#"{"messages":[{"user":"current"}]}"#;
    let (content_root, historical_revision, _) =
      publish_content_file_tree(&publisher, algorithm, first.namespace_root.root_hash, "messages.json", historical_body, [0x66; 16]);
    let content_root_value = publisher.load_immutable_entity_bounded(&content_root, 1024 * 1024).unwrap().unwrap().stored_value;
    let scope = semantic_scope_definition(algorithm, "/docs", Some("*.json"));
    let graph = complete_semantic_graph_with_definitions(algorithm, scope, "json-corrected", "typed_exact_blake3_v1");
    let historical_root = publish_complete_semantic_root(&publisher, algorithm, &content_root_value, content_root, &graph);
    publish_content_file_tree(&publisher, algorithm, historical_root.clone(), "messages.json", current_body, [0x67; 16]);

    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(256 * 1024 * 1024, 512 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
    let source = Arc::new(NativeReadViewSourceV1::new(Arc::new(publisher), Arc::clone(&memory), 86_400_000));
    let pins = RootReadPinCoordinatorV1::new(Arc::clone(&memory), algorithm, 8, 16).unwrap();
    let current = CurrentReadAuthorizationV1::new(
      CurrentPathAuthorizationV1::for_root("/docs/", CrudlifyOp::List),
      ReadViewCredentialKindV1::Ordinary,
      ReadViewConcealmentV1::Conceal,
    );
    let authorizer =
      ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), source.as_ref().clone());
    let resolver = ReadViewResolverV1::new(Arc::clone(&source), pins.clone(), all_capabilities_profile());
    let view = resolver.resolve(ReadViewSelectorV1::ExplicitRoot(&historical_root), &authorizer, &CancellationToken::new()).unwrap();
    let namespace_limits = NativeSelectedNamespaceLimitsV1::new(16, 1024 * 1024, u16::MAX as usize, 32, 100_000, 10_000).unwrap();
    let reader = source.selected_namespace_reader(&view, namespace_limits).unwrap();
    let catalogs = reader.load_planner_catalogs("/docs", &["messages"], default_native_selected_semantic_limits_v1()).unwrap();
    let page = reader.scan_files("/docs", None).unwrap();
    let row = &page.rows()[0];
    assert_eq!(row.record_revision(), historical_revision);
    let before = memory.snapshot().unwrap().owner(aeordb::engine::memory_coordinator::MemoryOwner::Query).unwrap().reserved_bytes;

    let evaluation = reader
      .evaluate_authoritative_source(
        row,
        &catalogs.catalogs()[0],
        &graph.scope_id,
        NativeSelectedSourceParserV1::Native,
        None,
        NativeSelectedSourceLimitsV1::new(128 * 1024 * 1024).unwrap(),
      )
      .unwrap();

    let expected =
      encode_canonical_value(&CanonicalConfigValueV1::String("historical".to_string()), CanonicalValueBounds::SOURCE_VALUE).unwrap();
    let NativeSelectedSourceOutcomeV1::Values(values) = evaluation.outcome() else {
      panic!("historical selected source was not evaluated to values");
    };
    assert_eq!(values, &[expected]);
    assert_eq!(evaluation.selected_root(), historical_root);
    assert_eq!(evaluation.semantic_state_root(), graph.state.object_id);
    assert_eq!(evaluation.scope_id(), graph.scope_id);
    assert_eq!(evaluation.value_store_id(), graph.value_store_id);
    assert_eq!(evaluation.file_key(), row.file_key());
    assert_eq!(evaluation.record_revision(), historical_revision);
    let during = memory.snapshot().unwrap().owner(aeordb::engine::memory_coordinator::MemoryOwner::Query).unwrap().reserved_bytes;
    assert!(during > before);
    drop(evaluation);
    assert_eq!(memory.snapshot().unwrap().owner(aeordb::engine::memory_coordinator::MemoryOwner::Query).unwrap().reserved_bytes, before);

    drop(page);
    drop(catalogs);
    drop(reader);
    drop(view);
    assert_eq!(pins.active_pin_count().unwrap(), 0);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn selected_source_prepares_one_reusable_runtime_and_releases_its_query_memory() {
  let fixture = selected_source_fixture("json-corrected", Some("*.json"), br#"{"messages":[{"user":"prepared"}]}"#, true);
  let reader = fixture.reader();
  let catalogs =
    reader.load_planner_catalogs("/docs", &[fixture.field_name.as_str()], default_native_selected_semantic_limits_v1()).unwrap();
  let page = reader.scan_files("/docs", None).unwrap();
  let baseline = fixture.memory.snapshot().unwrap().reserved_bytes;
  let prepared = reader
    .prepare_authoritative_source(
      &catalogs.catalogs()[0],
      &fixture.graph.scope_id,
      NativeSelectedSourceLimitsV1::new(128 * 1024 * 1024).unwrap(),
    )
    .unwrap();
  let prepared_bytes = fixture.memory.snapshot().unwrap().reserved_bytes;
  assert!(prepared_bytes > baseline, "prepared runtime must retain its admitted compiled state");
  assert!(
    prepared_bytes - baseline >= 2 * 1024 * 1024,
    "the fixture's compiled regex NFA/DFA ceilings must remain admitted while the prepared runtime is reusable"
  );

  for _ in 0..2 {
    let evaluation = prepared.evaluate(&page.rows()[0], NativeSelectedSourceParserV1::Native, None).unwrap();
    assert!(matches!(evaluation.outcome(), NativeSelectedSourceOutcomeV1::Values(values) if values.len() == 1));
    assert!(fixture.memory.snapshot().unwrap().reserved_bytes > prepared_bytes);
    drop(evaluation);
    assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, prepared_bytes);
  }

  drop(prepared);
  assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, baseline);
  drop(page);
  drop(catalogs);
  drop(reader);
  fixture.assert_released();
}

#[test]
fn selected_source_metadata_and_scope_filter_never_read_missing_chunks() {
  let metadata = selected_source_fixture("metadata-hash-corrected", Some("*.json"), b"unpublished metadata body", false);
  let reader = metadata.reader();
  let catalogs =
    reader.load_planner_catalogs("/docs", &[metadata.field_name.as_str()], default_native_selected_semantic_limits_v1()).unwrap();
  let page = reader.scan_files("/docs", None).unwrap();
  let evaluation = reader
    .evaluate_authoritative_source(
      &page.rows()[0],
      &catalogs.catalogs()[0],
      &metadata.graph.scope_id,
      NativeSelectedSourceParserV1::Native,
      None,
      NativeSelectedSourceLimitsV1::new(128 * 1024 * 1024).unwrap(),
    )
    .unwrap();
  assert!(matches!(evaluation.outcome(), NativeSelectedSourceOutcomeV1::Values(values) if values.len() == 1));
  drop(evaluation);
  drop(page);
  drop(catalogs);
  drop(reader);
  metadata.assert_released();

  let filtered = selected_source_fixture("json-corrected", Some("*.txt"), b"unpublished filtered body", false);
  let reader = filtered.reader();
  let catalogs =
    reader.load_planner_catalogs("/docs", &[filtered.field_name.as_str()], default_native_selected_semantic_limits_v1()).unwrap();
  let page = reader.scan_files("/docs", None).unwrap();
  let evaluation = reader
    .evaluate_authoritative_source(
      &page.rows()[0],
      &catalogs.catalogs()[0],
      &filtered.graph.scope_id,
      NativeSelectedSourceParserV1::Native,
      None,
      NativeSelectedSourceLimitsV1::new(128 * 1024 * 1024).unwrap(),
    )
    .unwrap();
  assert_eq!(evaluation.outcome(), &NativeSelectedSourceOutcomeV1::OutOfScope);
  drop(evaluation);
  drop(page);
  drop(catalogs);
  drop(reader);
  filtered.assert_released();
}

#[test]
fn selected_source_missing_and_corrupt_chunks_are_corruption_not_missing() {
  let missing = selected_source_fixture("json-corrected", Some("*.json"), br#"{"messages":[{"user":"missing"}]}"#, false);
  let reader = missing.reader();
  let catalogs =
    reader.load_planner_catalogs("/docs", &[missing.field_name.as_str()], default_native_selected_semantic_limits_v1()).unwrap();
  let page = reader.scan_files("/docs", None).unwrap();
  let error = reader
    .evaluate_authoritative_source(
      &page.rows()[0],
      &catalogs.catalogs()[0],
      &missing.graph.scope_id,
      NativeSelectedSourceParserV1::Native,
      None,
      NativeSelectedSourceLimitsV1::new(128 * 1024 * 1024).unwrap(),
    )
    .unwrap_err();
  assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  assert_eq!(error.code(), "selected_source_corrupt_chunk_missing");
  drop(page);
  drop(catalogs);
  drop(reader);
  missing.assert_released();

  let corrupt = selected_source_fixture("json-corrected", Some("*.json"), br#"{"messages":[{"user":"corrupt"}]}"#, true);
  let (offset, total_length) = corrupt.chunk_offset.expect("published chunk must have a locator");
  let mut file = OpenOptions::new().read(true).write(true).open(&corrupt.path).unwrap();
  file.seek(SeekFrom::Start(offset + u64::from(total_length) - 1)).unwrap();
  file.write_all(&[0x7f]).unwrap();
  file.sync_all().unwrap();
  drop(file);
  let reader = corrupt.reader();
  let catalogs =
    reader.load_planner_catalogs("/docs", &[corrupt.field_name.as_str()], default_native_selected_semantic_limits_v1()).unwrap();
  let page = reader.scan_files("/docs", None).unwrap();
  let error = reader
    .evaluate_authoritative_source(
      &page.rows()[0],
      &catalogs.catalogs()[0],
      &corrupt.graph.scope_id,
      NativeSelectedSourceParserV1::Native,
      None,
      NativeSelectedSourceLimitsV1::new(128 * 1024 * 1024).unwrap(),
    )
    .unwrap_err();
  assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  assert!(error.code().starts_with("selected_source_corrupt_"));
  drop(page);
  drop(catalogs);
  drop(reader);
  corrupt.assert_released();
}

#[test]
fn selected_source_preserves_parser_dependency_and_deterministic_outcomes() {
  let fixture = selected_source_fixture("json-corrected", Some("*.json"), b"body is intentionally unavailable", false);
  let reader = fixture.reader();
  let catalogs =
    reader.load_planner_catalogs("/docs", &[fixture.field_name.as_str()], default_native_selected_semantic_limits_v1()).unwrap();
  let page = reader.scan_files("/docs", None).unwrap();
  let baseline = fixture.memory.snapshot().unwrap().reserved_bytes;

  let dependency = reader
    .evaluate_authoritative_source(
      &page.rows()[0],
      &catalogs.catalogs()[0],
      &fixture.graph.scope_id,
      NativeSelectedSourceParserV1::Explicit(&DependencyUnavailableParser),
      None,
      NativeSelectedSourceLimitsV1::new(128 * 1024 * 1024).unwrap(),
    )
    .unwrap_err();
  assert_eq!(dependency.class(), NativeSelectedNamespaceReadErrorClassV1::Unavailable);
  assert_eq!(dependency.code(), "selected_test_parser_dependency");
  assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, baseline);

  let evaluation = reader
    .evaluate_authoritative_source(
      &page.rows()[0],
      &catalogs.catalogs()[0],
      &fixture.graph.scope_id,
      NativeSelectedSourceParserV1::Explicit(&DeterministicFailureParser),
      None,
      NativeSelectedSourceLimitsV1::new(128 * 1024 * 1024).unwrap(),
    )
    .unwrap();
  assert!(
    matches!(evaluation.outcome(), NativeSelectedSourceOutcomeV1::ParserUnindexable(failure) if failure.evidence() == b"selected deterministic parser evidence")
  );
  assert!(fixture.memory.snapshot().unwrap().reserved_bytes > baseline);
  drop(evaluation);
  assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, baseline);
  drop(page);
  drop(catalogs);
  drop(reader);
  fixture.assert_released();

  let malformed = selected_source_fixture("json-corrected", Some("*.json"), br#"{"messages":[}"#, true);
  let reader = malformed.reader();
  let catalogs =
    reader.load_planner_catalogs("/docs", &[malformed.field_name.as_str()], default_native_selected_semantic_limits_v1()).unwrap();
  let page = reader.scan_files("/docs", None).unwrap();
  let evaluation = reader
    .evaluate_authoritative_source(
      &page.rows()[0],
      &catalogs.catalogs()[0],
      &malformed.graph.scope_id,
      NativeSelectedSourceParserV1::Native,
      None,
      NativeSelectedSourceLimitsV1::new(128 * 1024 * 1024).unwrap(),
    )
    .unwrap();
  assert!(matches!(evaluation.outcome(), NativeSelectedSourceOutcomeV1::ParserUnindexable(_)));
  drop(evaluation);
  drop(page);
  drop(catalogs);
  drop(reader);
  malformed.assert_released();
}

#[test]
fn selected_source_mapper_is_explicit_and_never_collapses_dependency_loss_to_missing() {
  let fixture = selected_source_fixture("mapper-corrected", Some("*.json"), b"mapper parser body is supplied by its exact executor", false);
  let reader = fixture.reader();
  let catalogs =
    reader.load_planner_catalogs("/docs", &[fixture.field_name.as_str()], default_native_selected_semantic_limits_v1()).unwrap();
  let page = reader.scan_files("/docs", None).unwrap();
  let baseline = fixture.memory.snapshot().unwrap().reserved_bytes;
  let unavailable = reader
    .evaluate_authoritative_source(
      &page.rows()[0],
      &catalogs.catalogs()[0],
      &fixture.graph.scope_id,
      NativeSelectedSourceParserV1::Explicit(&ParsedNullParser),
      None,
      NativeSelectedSourceLimitsV1::new(128 * 1024 * 1024).unwrap(),
    )
    .unwrap_err();
  assert_eq!(unavailable.class(), NativeSelectedNamespaceReadErrorClassV1::Unavailable);
  assert_eq!(unavailable.code(), "plugin_mapper_unavailable");
  assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, baseline);

  let evaluation = reader
    .evaluate_authoritative_source(
      &page.rows()[0],
      &catalogs.catalogs()[0],
      &fixture.graph.scope_id,
      NativeSelectedSourceParserV1::Explicit(&ParsedNullParser),
      Some(&SelectedValueMapper),
      NativeSelectedSourceLimitsV1::new(128 * 1024 * 1024).unwrap(),
    )
    .unwrap();
  let expected = encode_canonical_value(&CanonicalConfigValueV1::String("mapped".to_string()), CanonicalValueBounds::SOURCE_VALUE).unwrap();
  assert!(matches!(evaluation.outcome(), NativeSelectedSourceOutcomeV1::Values(values) if values == &[expected]));
  drop(evaluation);
  assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, baseline);
  drop(page);
  drop(catalogs);
  drop(reader);
  fixture.assert_released();
}

#[test]
fn selected_source_cancellation_pressure_and_cross_view_inputs_fail_closed_without_leaks() {
  let cancelled = selected_source_fixture("metadata-hash-corrected", Some("*.json"), b"cancelled", false);
  let reader = cancelled.reader();
  let catalogs =
    reader.load_planner_catalogs("/docs", &[cancelled.field_name.as_str()], default_native_selected_semantic_limits_v1()).unwrap();
  let page = reader.scan_files("/docs", None).unwrap();
  let baseline = cancelled.memory.snapshot().unwrap().reserved_bytes;
  cancelled.cancellation.cancel();
  let error = reader
    .evaluate_authoritative_source(
      &page.rows()[0],
      &catalogs.catalogs()[0],
      &cancelled.graph.scope_id,
      NativeSelectedSourceParserV1::Native,
      None,
      NativeSelectedSourceLimitsV1::new(128 * 1024 * 1024).unwrap(),
    )
    .unwrap_err();
  assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::Cancelled);
  assert_eq!(cancelled.memory.snapshot().unwrap().reserved_bytes, baseline);
  drop(page);
  drop(catalogs);
  drop(reader);
  cancelled.assert_released();

  let after_parser = selected_source_fixture("json-corrected", Some("*.json"), b"cancelled after parser", false);
  let reader = after_parser.reader();
  let catalogs =
    reader.load_planner_catalogs("/docs", &[after_parser.field_name.as_str()], default_native_selected_semantic_limits_v1()).unwrap();
  let page = reader.scan_files("/docs", None).unwrap();
  let baseline = after_parser.memory.snapshot().unwrap().reserved_bytes;
  let error = {
    let parser = CancelAfterParser { cancellation: &after_parser.cancellation };
    reader.evaluate_authoritative_source(
      &page.rows()[0],
      &catalogs.catalogs()[0],
      &after_parser.graph.scope_id,
      NativeSelectedSourceParserV1::Explicit(&parser),
      None,
      NativeSelectedSourceLimitsV1::new(128 * 1024 * 1024).unwrap(),
    )
  }
  .unwrap_err();
  assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::Cancelled);
  assert_eq!(after_parser.memory.snapshot().unwrap().reserved_bytes, baseline);
  drop(page);
  drop(catalogs);
  drop(reader);
  after_parser.assert_released();

  let pressured = selected_source_fixture("metadata-hash-corrected", Some("*.json"), b"pressured", false);
  let reader = pressured.reader();
  let catalogs =
    reader.load_planner_catalogs("/docs", &[pressured.field_name.as_str()], default_native_selected_semantic_limits_v1()).unwrap();
  let page = reader.scan_files("/docs", None).unwrap();
  let baseline = pressured.memory.snapshot().unwrap().reserved_bytes;
  let error = reader
    .evaluate_authoritative_source(
      &page.rows()[0],
      &catalogs.catalogs()[0],
      &pressured.graph.scope_id,
      NativeSelectedSourceParserV1::Native,
      None,
      NativeSelectedSourceLimitsV1::new(1).unwrap(),
    )
    .unwrap_err();
  assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "selected_source_retained_bytes");
  assert_eq!(pressured.memory.snapshot().unwrap().reserved_bytes, baseline);
  let ordinary_limit = 511 * 1024 * 1024;
  let blocker = pressured.memory.reserve(MemoryOwner::ServerCaches, ordinary_limit - baseline - 1, AdmissionClass::Workload).unwrap();
  let error = match reader.prepare_authoritative_source(
    &catalogs.catalogs()[0],
    &pressured.graph.scope_id,
    NativeSelectedSourceLimitsV1::new(128 * 1024 * 1024).unwrap(),
  ) {
    Ok(_) => panic!("runtime preparation must refuse hard query pressure"),
    Err(error) => error,
  };
  assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "selected_source_memory");
  drop(blocker);
  assert_eq!(pressured.memory.snapshot().unwrap().reserved_bytes, baseline);
  let prepared = reader
    .prepare_authoritative_source(
      &catalogs.catalogs()[0],
      &pressured.graph.scope_id,
      NativeSelectedSourceLimitsV1::new(128 * 1024 * 1024).unwrap(),
    )
    .unwrap();
  let prepared_baseline = pressured.memory.snapshot().unwrap().reserved_bytes;
  let blocker =
    pressured.memory.reserve(MemoryOwner::ServerCaches, ordinary_limit - prepared_baseline - 1, AdmissionClass::Workload).unwrap();
  let error = prepared.evaluate(&page.rows()[0], NativeSelectedSourceParserV1::Native, None).unwrap_err();
  assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "selected_source_receipt_memory");
  drop(blocker);
  assert_eq!(pressured.memory.snapshot().unwrap().reserved_bytes, prepared_baseline);
  let blocker =
    pressured.memory.reserve(MemoryOwner::ServerCaches, ordinary_limit - prepared_baseline - 8 * 1024, AdmissionClass::Workload).unwrap();
  let error = prepared.evaluate(&page.rows()[0], NativeSelectedSourceParserV1::Native, None).unwrap_err();
  assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "selected_source_memory");
  drop(blocker);
  assert_eq!(pressured.memory.snapshot().unwrap().reserved_bytes, prepared_baseline);
  drop(prepared);
  assert_eq!(pressured.memory.snapshot().unwrap().reserved_bytes, baseline);
  drop(page);
  drop(catalogs);
  drop(reader);
  pressured.assert_released();

  let left = selected_source_fixture("metadata-hash-corrected", Some("*.json"), b"left", false);
  let right = selected_source_fixture("metadata-hash-corrected", Some("*.json"), b"right and therefore a different root", false);
  let left_reader = left.reader();
  let right_reader = right.reader();
  let left_catalogs =
    left_reader.load_planner_catalogs("/docs", &[left.field_name.as_str()], default_native_selected_semantic_limits_v1()).unwrap();
  let right_catalogs =
    right_reader.load_planner_catalogs("/docs", &[right.field_name.as_str()], default_native_selected_semantic_limits_v1()).unwrap();
  let left_page = left_reader.scan_files("/docs", None).unwrap();
  let right_page = right_reader.scan_files("/docs", None).unwrap();
  let foreign_row = left_reader
    .evaluate_authoritative_source(
      &right_page.rows()[0],
      &left_catalogs.catalogs()[0],
      &left.graph.scope_id,
      NativeSelectedSourceParserV1::Native,
      None,
      NativeSelectedSourceLimitsV1::new(128 * 1024 * 1024).unwrap(),
    )
    .unwrap_err();
  assert_eq!(foreign_row.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  assert_eq!(foreign_row.code(), "selected_source_row_authority");
  let foreign_catalog = left_reader
    .evaluate_authoritative_source(
      &left_page.rows()[0],
      &right_catalogs.catalogs()[0],
      &left.graph.scope_id,
      NativeSelectedSourceParserV1::Native,
      None,
      NativeSelectedSourceLimitsV1::new(128 * 1024 * 1024).unwrap(),
    )
    .unwrap_err();
  assert_eq!(foreign_catalog.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  assert_eq!(foreign_catalog.code(), "selected_source_catalog_authority");
  let mut wrong_value_store = left_catalogs.catalogs()[0].clone();
  wrong_value_store.scopes[0].value_store_id[0] ^= 1;
  let wrong_value_store = left_reader
    .evaluate_authoritative_source(
      &left_page.rows()[0],
      &wrong_value_store,
      &left.graph.scope_id,
      NativeSelectedSourceParserV1::Native,
      None,
      NativeSelectedSourceLimitsV1::new(128 * 1024 * 1024).unwrap(),
    )
    .unwrap_err();
  assert_eq!(wrong_value_store.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  assert_eq!(wrong_value_store.code(), "authoritative_source_identity");
  drop(left_page);
  drop(right_page);
  drop(left_catalogs);
  drop(right_catalogs);
  drop(left_reader);
  drop(right_reader);
  left.assert_released();
  right.assert_released();
}

#[test]
fn selected_semantic_catalog_keeps_descendant_glob_scopes_for_directory_queries() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, _path, publisher) = publisher(algorithm);
    let first = publisher.publish(&first_request(algorithm)).unwrap();
    let scope = semantic_scope_definition(algorithm, "/docs", Some("*.json"));
    let graph = complete_semantic_graph_with_scope(algorithm, scope);
    let complete_root =
      publish_complete_semantic_root(&publisher, algorithm, &first.namespace_root.value, first.namespace_root.root_hash, &graph);

    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(256 * 1024 * 1024, 512 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
    let source = Arc::new(NativeReadViewSourceV1::new(Arc::new(publisher), Arc::clone(&memory), 86_400_000));
    let pins = RootReadPinCoordinatorV1::new(Arc::clone(&memory), algorithm, 8, 16).unwrap();
    let current = CurrentReadAuthorizationV1::new(
      CurrentPathAuthorizationV1::for_root("/docs/", CrudlifyOp::List),
      ReadViewCredentialKindV1::Ordinary,
      ReadViewConcealmentV1::Conceal,
    );
    let authorizer =
      ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), source.as_ref().clone());
    let resolver = ReadViewResolverV1::new(Arc::clone(&source), pins.clone(), all_capabilities_profile());
    let view = resolver.resolve(ReadViewSelectorV1::ExplicitRoot(&complete_root), &authorizer, &CancellationToken::new()).unwrap();
    let namespace_limits = NativeSelectedNamespaceLimitsV1::new(16, 1024 * 1024, u16::MAX as usize, 32, 100_000, 10_000).unwrap();
    let reader = source.selected_namespace_reader(&view, namespace_limits).unwrap();

    let selected = reader.load_planner_catalogs("/docs", &["@hash"], default_native_selected_semantic_limits_v1()).unwrap();

    assert_eq!(selected.catalogs().len(), 1);
    assert_eq!(selected.catalogs()[0].scopes.len(), 1);
    assert_eq!(selected.catalogs()[0].scopes[0].scope_id, graph.scope_id);
    drop(selected);
    drop(reader);
    drop(view);
    assert_eq!(pins.active_pin_count().unwrap(), 0);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn selected_semantic_catalog_retains_fieldless_nearer_scopes_for_effective_resolution() {
  let algorithm = HashAlgorithm::Blake3_256;
  let primary = semantic_scope_definition(algorithm, "/", Some("**/*.json"));
  let nearer = semantic_scope_definition(algorithm, "/docs", None);
  let primary_id = decode_scope_definition(&primary, algorithm).unwrap().scope_id;
  let nearer_id = decode_scope_definition(&nearer, algorithm).unwrap().scope_id;
  let fixture = selected_source_fixture_with_scopes("metadata-hash-corrected", primary, &[nearer], b"{}", true);
  let reader = fixture.reader();

  let selected = reader.load_planner_catalogs("/docs", &["@hash"], default_native_selected_semantic_limits_v1()).unwrap();

  assert_eq!(selected.catalogs().len(), 1);
  assert_eq!(selected.catalogs()[0].scopes.len(), 1);
  assert_eq!(selected.catalogs()[0].scopes[0].scope_id, primary_id);
  let retained = selected
    .scope_definitions()
    .iter()
    .map(|scope| (scope.scope_id().to_vec(), scope.encoded_definition().to_vec()))
    .collect::<BTreeMap<_, _>>();
  assert_eq!(retained.len(), 2);
  assert_eq!(retained.get(&primary_id).map(Vec::as_slice), Some(selected.catalogs()[0].scopes[0].encoded_scope_definition.as_slice()));
  assert!(retained.contains_key(&nearer_id));

  drop(selected);
  drop(reader);
  fixture.assert_released();
}

#[test]
fn native_authoritative_partition_streams_selected_root_values_and_exact_receipts() {
  let algorithm = HashAlgorithm::Blake3_256;
  let scope = semantic_scope_definition(algorithm, "/docs", Some("*.json"));
  let mut fixture = native_partition_fixture("json-corrected", scope, &[], br#"{"messages":[{"user":"selected"}]}"#);
  assert_eq!(fixture.source.document_count(), 1);
  assert!(fixture.source.workspace_bytes() > 0);

  let mut cursor = fixture.open_cursor(u16::MAX as u64);
  let document = cursor.next_document(&fixture.cancellation).unwrap().unwrap();
  assert_eq!(document.scope_id.as_deref(), Some(fixture.scope_id.as_slice()));
  assert_eq!(document.path, "/docs/messages.json");
  assert_eq!(document.state, QueryExecutionFieldStateV1::Values);
  assert_eq!(document.canonical_values.len(), 1);
  assert!(cursor.next_document(&fixture.cancellation).unwrap().is_none());
  let receipt = cursor.finish().unwrap();
  assert_eq!(receipt.selected_namespace_root, fixture.selected_root);
  assert_eq!(receipt.publication_sequence, fixture.publication_sequence);
  assert_eq!(receipt.field_name, fixture.field_name);
  assert_eq!(receipt.scope_ids, vec![fixture.scope_id.clone()]);
  assert_eq!(receipt.scope_document_counts, vec![1]);
  assert_eq!(receipt.unconfigured_document_count, 0);
  assert_eq!(receipt.document_count, 1);
  assert!(receipt.complete);
  drop(cursor);
  fixture.assert_released();
}

#[test]
fn native_authoritative_partition_reorders_one_document_pages_by_file_key() {
  let names = ["a.json", "b.json", "c.json", "d.json", "e.json", "f.json", "g.json"];
  let (mut fixture, mut expected) = native_partition_many_fixture(HashAlgorithm::Blake3_256, 1, true, &names);
  let path_page_order = expected.iter().map(|(file_key, _, _)| file_key.clone()).collect::<Vec<_>>();
  expected.sort_by(|left, right| left.0.cmp(&right.0));
  assert_ne!(path_page_order, expected.iter().map(|(file_key, _, _)| file_key.clone()).collect::<Vec<_>>());

  let mut cursor = fixture.open_cursor(u16::MAX as u64);
  let mut observed = Vec::new();
  while let Some(document) = cursor.next_document(&fixture.cancellation).unwrap() {
    observed.push((document.file_key, document.path));
  }
  assert_eq!(observed, expected.into_iter().map(|(file_key, _, path)| (file_key, path)).collect::<Vec<_>>());
  let receipt = cursor.finish().unwrap();
  assert_eq!(receipt.scope_document_counts, vec![names.len() as u64]);
  assert_eq!(receipt.unconfigured_document_count, 0);
  assert_eq!(receipt.document_count, names.len() as u64);
  drop(cursor);
  fixture.assert_released();
}

fn native_query_execution_limits() -> QueryExecutionLimitsV1 {
  QueryExecutionLimitsV1::new(
    QueryExecutionCountLimitsV1::new(64, 256, 64, 100_000).unwrap(),
    QueryExecutionByteLimitsV1::new(1024 * 1024, 8 * 1024 * 1024, 8 * 1024 * 1024).unwrap(),
  )
}

#[test]
fn native_authoritative_execution_facade_runs_the_selected_root_truth_path() {
  let names = ["a.json", "b.json", "c.json", "d.json"];
  let sizes = [3, 17, 9, 1];
  let (mut fixture, expected) = native_authoritative_size_fixture(HashAlgorithm::Blake3_256, &names, &sizes, 8);
  let mut expected = expected.into_iter().zip(sizes).filter_map(|(identity, size)| (size > 8).then_some(identity)).collect::<Vec<_>>();
  expected.sort_by(|left, right| left.0.cmp(&right.0));
  let advanced = fixture
    .backing_source
    .publisher()
    .publish_successor_authority(&successor_request(HashAlgorithm::Blake3_256, fixture.selected_root.clone()))
    .unwrap();
  assert_ne!(advanced.namespace_root.root_hash, fixture.selected_root);

  let execution =
    fixture.source.execute_authoritative_query_v1(&fixture.plan, &fixture.cancellation, native_query_execution_limits()).unwrap();

  assert_eq!(execution.selected_namespace_root(), fixture.selected_root);
  assert_eq!(execution.examined_documents(), names.len() as u64);
  assert_eq!(execution.examined_field_values(), names.len() as u64);
  assert_eq!(
    execution
      .matches()
      .iter()
      .map(|matched| (matched.file_key().to_vec(), matched.record_revision().to_vec(), matched.path().to_string()))
      .collect::<Vec<_>>(),
    expected
  );
  drop(execution);

  let mut auxiliary = fixture
    .source
    .open_auxiliary_source(&fixture.plan, NativeAuthoritativeAuxiliaryLimitsV1::new(16, 64, 1024 * 1024, u16::MAX as u64).unwrap())
    .unwrap();
  let mut sink = QueryOrderedTopKSinkV1::new(
    &fixture.plan,
    &mut auxiliary,
    fixture.memory.as_ref(),
    &fixture.cancellation,
    QueryOrderedTopKLimitsV1::new(1024 * 1024, 8 * 1024 * 1024).unwrap(),
  )
  .unwrap();
  let receipt = fixture
    .source
    .execute_authoritative_query_into_v1(&fixture.plan, &fixture.cancellation, native_query_execution_limits(), &mut sink)
    .unwrap();
  assert_eq!(receipt.match_count(), 2);
  let ordered = sink.finish().unwrap();
  assert_eq!(ordered.total_match_count(), 2);
  assert_eq!(ordered.rows().len(), 2);
  assert_eq!(ordered.rows()[0].components[0].payload, 9u64.to_le_bytes());
  assert_eq!(ordered.rows()[1].components[0].payload, 17u64.to_le_bytes());
  drop(ordered);
  drop(auxiliary);
  fixture.assert_released();
}

#[test]
fn native_auxiliary_source_resolves_selected_root_path_and_stale_revision() {
  let (fixture, expected) = native_partition_many_fixture(HashAlgorithm::Blake3_256, 1, true, &["a.json", "z.json"]);
  let mut source = fixture
    .source
    .open_auxiliary_source(&fixture.plan, NativeAuthoritativeAuxiliaryLimitsV1::new(16, 64, 1024 * 1024, u16::MAX as u64).unwrap())
    .unwrap();
  let (file_key, revision, path) = &expected[0];
  let request = PositionUniverseLookupRequestV1::new(
    fixture.plan.database_id(),
    fixture.plan.physical_instance_id(),
    fixture.plan.selected_namespace_root(),
    fixture.plan.query_order(),
    file_key,
    revision,
    1024 * 1024,
  );
  let PositionUniverseLookupResultV1::Found(row) = resolve_position_universe_row_v1(request, &mut source, &fixture.cancellation).unwrap()
  else {
    panic!("selected-root FileKey and RecordRevision must resolve")
  };
  assert_eq!(row.route, PositionRouteV1::Query);
  assert_eq!(row.components.len(), 1);
  assert_eq!(row.components[0].state, PositionComponentStateV1::Present);
  assert_eq!(row.components[0].payload, path.as_bytes());

  let stale_revision = digest_parts(HashAlgorithm::Blake3_256, &[b"stale native auxiliary revision"]);
  let stale = resolve_position_universe_row_v1(
    PositionUniverseLookupRequestV1::new(
      fixture.plan.database_id(),
      fixture.plan.physical_instance_id(),
      fixture.plan.selected_namespace_root(),
      fixture.plan.query_order(),
      file_key,
      &stale_revision,
      1024 * 1024,
    ),
    &mut source,
    &fixture.cancellation,
  )
  .unwrap();
  assert_eq!(stale, PositionUniverseLookupResultV1::Absent);
  drop(source);
  fixture.assert_released();
}

#[test]
fn native_auxiliary_source_reuses_selected_definitions_for_position_and_aggregate_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let names = ["large.json", "middle.json", "small.json"];
    let sizes = [90u64, 10, 50];
    let (fixture, expected) = native_auxiliary_size_fixture(algorithm, &names, &sizes);
    let aggregate =
      CompiledQueryAggregateInputV1::from_plan(&fixture.plan, QueryAggregateInputLimitsV1::new(8, 8, 8, 1024 * 1024).unwrap()).unwrap();
    let mut position_source = fixture
      .source
      .open_auxiliary_source(&fixture.plan, NativeAuthoritativeAuxiliaryLimitsV1::new(16, 64, 1024 * 1024, u16::MAX as u64).unwrap())
      .unwrap();
    let mut aggregate_source = fixture
      .source
      .open_auxiliary_source(&fixture.plan, NativeAuthoritativeAuxiliaryLimitsV1::new(16, 64, 1024 * 1024, u16::MAX as u64).unwrap())
      .unwrap();

    for ((file_key, revision, path), size) in expected.iter().zip(sizes) {
      let PositionUniverseLookupResultV1::Found(position) = resolve_position_universe_row_v1(
        PositionUniverseLookupRequestV1::new(
          fixture.plan.database_id(),
          fixture.plan.physical_instance_id(),
          fixture.plan.selected_namespace_root(),
          fixture.plan.query_order(),
          file_key,
          revision,
          1024 * 1024,
        ),
        &mut position_source,
        &fixture.cancellation,
      )
      .unwrap() else {
        panic!("selected-root size position must resolve")
      };
      assert_eq!(position.components.len(), 2);
      assert_eq!(position.components[0].payload, size.to_le_bytes());
      assert_eq!(position.components[1].payload, path.as_bytes());

      let QueryAggregateInputLookupResultV1::Found(row) = resolve_query_aggregate_input_v1(
        QueryAggregateInputLookupRequestV1::new(&aggregate, file_key, revision),
        &mut aggregate_source,
        &fixture.cancellation,
      )
      .unwrap() else {
        panic!("selected-root size aggregate must resolve")
      };
      assert_eq!(row.fields.len(), 1);
      assert_eq!(row.fields[0].scope_id.as_deref(), Some(fixture.scope_id.as_slice()));
      assert_eq!(row.fields[0].state, QueryExecutionFieldStateV1::Values);
      assert_eq!(row.fields[0].values.len(), 1);
      assert_eq!(row.fields[0].values[0].payload, size.to_le_bytes());
    }
    drop(position_source);
    drop(aggregate_source);
    fixture.assert_released();
  }
}

#[test]
fn native_auxiliary_source_retains_a_nearer_fieldless_scope_as_missing() {
  let (fixture, expected) = native_auxiliary_size_fieldless_fixture(HashAlgorithm::Blake3_256);
  let aggregate =
    CompiledQueryAggregateInputV1::from_plan(&fixture.plan, QueryAggregateInputLimitsV1::new(8, 8, 8, 1024 * 1024).unwrap()).unwrap();
  let limits = NativeAuthoritativeAuxiliaryLimitsV1::new(16, 64, 1024 * 1024, u16::MAX as u64).unwrap();
  let mut position_source = fixture.source.open_auxiliary_source(&fixture.plan, limits).unwrap();
  let mut aggregate_source = fixture.source.open_auxiliary_source(&fixture.plan, limits).unwrap();
  let (file_key, revision, path) = &expected[0];

  let PositionUniverseLookupResultV1::Found(position) = resolve_position_universe_row_v1(
    PositionUniverseLookupRequestV1::new(
      fixture.plan.database_id(),
      fixture.plan.physical_instance_id(),
      fixture.plan.selected_namespace_root(),
      fixture.plan.query_order(),
      file_key,
      revision,
      1024 * 1024,
    ),
    &mut position_source,
    &fixture.cancellation,
  )
  .unwrap() else {
    panic!("fieldless selected-root position must resolve")
  };
  assert_eq!(position.components[0].state, PositionComponentStateV1::Missing);
  assert_eq!(position.components[1].payload, path.as_bytes());

  let QueryAggregateInputLookupResultV1::Found(row) = resolve_query_aggregate_input_v1(
    QueryAggregateInputLookupRequestV1::new(&aggregate, file_key, revision),
    &mut aggregate_source,
    &fixture.cancellation,
  )
  .unwrap() else {
    panic!("fieldless selected-root aggregate must resolve")
  };
  assert_eq!(row.fields.len(), 1);
  assert_eq!(row.fields[0].scope_id, None);
  assert_eq!(row.fields[0].state, QueryExecutionFieldStateV1::Missing);
  assert!(row.fields[0].values.is_empty());
  drop(position_source);
  drop(aggregate_source);
  fixture.assert_released();
}

#[test]
fn native_auxiliary_source_fails_closed_on_binding_pressure_and_cancellation() {
  let (pressured, _) = native_auxiliary_size_fixture(HashAlgorithm::Blake3_256, &["record.json"], &[7]);
  let before = pressured.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes;
  let error = match pressured
    .source
    .open_auxiliary_source(&pressured.plan, NativeAuthoritativeAuxiliaryLimitsV1::new(16, 64, 1, u16::MAX as u64).unwrap())
  {
    Ok(_) => panic!("one byte cannot admit native auxiliary bindings"),
    Err(error) => error,
  };
  assert_eq!(error.class(), QueryExecutionSourceErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "native_auxiliary_binding_limit");
  assert_eq!(pressured.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, before);
  pressured.assert_released();

  let (cancelled, expected) = native_partition_many_fixture(HashAlgorithm::Blake3_256, 1, false, &["record.json"]);
  let mut source = cancelled
    .source
    .open_auxiliary_source(&cancelled.plan, NativeAuthoritativeAuxiliaryLimitsV1::new(16, 64, 1024 * 1024, u16::MAX as u64).unwrap())
    .unwrap();
  let request_cancellation = CancellationToken::new();
  request_cancellation.cancel();
  let (file_key, revision, _) = &expected[0];
  let error = resolve_position_universe_row_v1(
    PositionUniverseLookupRequestV1::new(
      cancelled.plan.database_id(),
      cancelled.plan.physical_instance_id(),
      cancelled.plan.selected_namespace_root(),
      cancelled.plan.query_order(),
      file_key,
      revision,
      1024 * 1024,
    ),
    &mut source,
    &request_cancellation,
  )
  .unwrap_err();
  assert!(matches!(error, aeordb::engine::v4::position_resolver::PositionUniverseSourceErrorV1::Cancelled));
  drop(source);
  cancelled.assert_released();
}

#[test]
fn native_auxiliary_source_rejects_an_oversized_position_row_before_return() {
  let (fixture, expected) = native_partition_many_fixture(HashAlgorithm::Blake3_256, 1, false, &["record.json"]);
  let mut source = fixture
    .source
    .open_auxiliary_source(&fixture.plan, NativeAuthoritativeAuxiliaryLimitsV1::new(16, 64, 1024 * 1024, u16::MAX as u64).unwrap())
    .unwrap();
  let before = fixture.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes;
  let (file_key, revision, _) = &expected[0];
  let error = source
    .resolve_position(
      PositionUniverseLookupRequestV1::new(
        fixture.plan.database_id(),
        fixture.plan.physical_instance_id(),
        fixture.plan.selected_namespace_root(),
        fixture.plan.query_order(),
        file_key,
        revision,
        1,
      ),
      &fixture.cancellation,
    )
    .unwrap_err();
  assert!(matches!(error, PositionUniverseSourceErrorV1::ResourceLimit(_)));
  assert_eq!(fixture.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, before);
  drop(source);
  fixture.assert_released();
}

#[test]
fn native_auxiliary_source_rejects_an_oversized_aggregate_row_before_return() {
  let (fixture, expected) = native_auxiliary_size_fixture(HashAlgorithm::Blake3_256, &["record.json"], &[7]);
  let input = CompiledQueryAggregateInputV1::from_plan(&fixture.plan, QueryAggregateInputLimitsV1::new(8, 8, 8, 1).unwrap()).unwrap();
  let mut source = fixture
    .source
    .open_auxiliary_source(&fixture.plan, NativeAuthoritativeAuxiliaryLimitsV1::new(16, 64, 1024 * 1024, u16::MAX as u64).unwrap())
    .unwrap();
  let before = fixture.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes;
  let (file_key, revision, _) = &expected[0];
  let error =
    source.resolve_aggregate_input(QueryAggregateInputLookupRequestV1::new(&input, file_key, revision), &fixture.cancellation).unwrap_err();
  assert_eq!(error.class(), QueryExecutionSourceErrorClassV1::ResourceLimit);
  assert_eq!(fixture.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, before);
  drop(source);
  fixture.assert_released();
}

#[test]
fn native_authoritative_partition_retains_fieldless_effective_scope_as_missing() {
  let algorithm = HashAlgorithm::Blake3_256;
  let primary = semantic_scope_definition(algorithm, "/", Some("**/*.json"));
  let nearer = semantic_scope_definition(algorithm, "/docs", None);
  let mut fixture = native_partition_fixture("metadata-hash-corrected", primary, &[nearer], b"{}");

  let mut cursor = fixture.open_cursor(u16::MAX as u64);
  let document = cursor.next_document(&fixture.cancellation).unwrap().unwrap();
  assert_eq!(document.scope_id, None);
  assert_eq!(document.path, "/docs/messages.json");
  assert_eq!(document.state, QueryExecutionFieldStateV1::Missing);
  assert!(document.canonical_values.is_empty());
  assert!(cursor.next_document(&fixture.cancellation).unwrap().is_none());
  let receipt = cursor.finish().unwrap();
  assert_eq!(receipt.scope_document_counts, vec![0]);
  assert_eq!(receipt.unconfigured_document_count, 1);
  assert_eq!(receipt.document_count, 1);
  drop(cursor);
  fixture.assert_released();
}

#[test]
fn native_authoritative_partition_poisoned_after_post_advance_failure() {
  let algorithm = HashAlgorithm::Blake3_256;
  let scope = semantic_scope_definition(algorithm, "/docs", Some("*.json"));
  let mut fixture = native_partition_fixture("metadata-hash-corrected", scope, &[], b"{}");

  let mut cursor = fixture.open_cursor(1);
  let first = cursor.next_document(&fixture.cancellation).unwrap_err();
  assert_eq!(first.class(), QueryExecutionSourceErrorClassV1::Corrupt);
  assert_eq!(first.code(), "native_partition_document_path");
  let retry = cursor.next_document(&fixture.cancellation).unwrap_err();
  assert_eq!(retry.class(), QueryExecutionSourceErrorClassV1::Internal);
  assert_eq!(retry.code(), "native_partition_cursor_failed");
  let finish = cursor.finish().unwrap_err();
  assert_eq!(finish.class(), QueryExecutionSourceErrorClassV1::Internal);
  assert_eq!(finish.code(), "native_partition_cursor_failed");
  drop(cursor);
  fixture.assert_released();
}

fn assert_selected_semantic_catalog_uses_historical_root(algorithm: HashAlgorithm) {
  let (_directory, _path, publisher) = publisher(algorithm);
  let first = publisher.publish(&first_request(algorithm)).unwrap();
  let graph = complete_semantic_graph(algorithm);
  let complete_root =
    publish_complete_semantic_root(&publisher, algorithm, &first.namespace_root.value, first.namespace_root.root_hash, &graph);
  publisher.publish_successor_authority(&successor_request(algorithm, complete_root.clone())).unwrap();

  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(256 * 1024 * 1024, 512 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
  let source = Arc::new(NativeReadViewSourceV1::new(Arc::new(publisher), Arc::clone(&memory), 86_400_000));
  let pins = RootReadPinCoordinatorV1::new(Arc::clone(&memory), algorithm, 8, 16).unwrap();
  let current = CurrentReadAuthorizationV1::new(
    CurrentPathAuthorizationV1::for_root("/docs/", CrudlifyOp::List),
    ReadViewCredentialKindV1::Ordinary,
    ReadViewConcealmentV1::Conceal,
  );
  let authorizer = ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), source.as_ref().clone());
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), pins.clone(), all_capabilities_profile());
  let view = resolver.resolve(ReadViewSelectorV1::ExplicitRoot(&complete_root), &authorizer, &CancellationToken::new()).unwrap();
  let namespace_limits = NativeSelectedNamespaceLimitsV1::new(16, 1024 * 1024, u16::MAX as usize, 32, 100_000, 10_000).unwrap();
  let reader = source.selected_namespace_reader(&view, namespace_limits).unwrap();
  let before = memory.snapshot().unwrap().owner(aeordb::engine::memory_coordinator::MemoryOwner::Query).unwrap().reserved_bytes;

  let selected = reader.load_planner_catalogs("/docs", &["@hash"], default_native_selected_semantic_limits_v1()).unwrap();

  let during = memory.snapshot().unwrap().owner(aeordb::engine::memory_coordinator::MemoryOwner::Query).unwrap().reserved_bytes;
  assert!(during > before);
  assert_eq!(selected.selected_root(), complete_root);
  assert_eq!(selected.semantic_state_root(), graph.state.object_id);
  assert_eq!(selected.catalogs().len(), 1);
  let catalog = &selected.catalogs()[0];
  assert_eq!(catalog.database_id, view.database_id());
  assert_eq!(catalog.physical_instance_id, view.physical_instance_id());
  assert_eq!(catalog.field_name, "@hash");
  assert!(catalog.complete);
  assert_eq!(catalog.scopes.len(), 1);
  let scope = &catalog.scopes[0];
  assert_eq!(scope.scope_id, graph.scope_id);
  assert_eq!(decode_value_store_definition(&scope.encoded_value_store_definition, algorithm).unwrap().value_store_id, graph.value_store_id);
  assert_eq!(scope.indexes.len(), 1);
  assert_eq!(scope.indexes[0].index_id, graph.field_index_id);
  assert!(scope.indexes[0].selected_generation.is_none());
  drop(selected);
  assert_eq!(memory.snapshot().unwrap().owner(aeordb::engine::memory_coordinator::MemoryOwner::Query).unwrap().reserved_bytes, before);
  drop(reader);
  drop(view);
  assert_eq!(pins.active_pin_count().unwrap(), 0);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn selected_semantic_catalog_binds_the_exact_real_registry_generation() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for (fixture, expected_nvt) in [(selected_artifact_fixture(algorithm), false), (selected_valid_nvt_artifact_fixture(algorithm), true)] {
      let snapshot = selected_artifact_coverage_snapshot(&fixture);
      let reader = fixture.reader();
      let before_selected = fixture.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes;
      let mut selected = reader.load_planner_catalogs("/", &["@hash"], default_native_selected_semantic_limits_v1()).unwrap();
      assert!(selected.catalogs()[0].scopes[0].indexes[0].selected_generation.is_none());
      let before_binding = fixture.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes;

      reader.bind_planner_coverage(&mut selected, snapshot.as_ref()).unwrap();

      assert!(fixture.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes > before_binding);
      let candidate =
        selected.catalogs()[0].scopes[0].indexes.iter().find(|candidate| candidate.index_id == fixture.generation.owner_id).unwrap();
      assert_eq!(candidate.selected_generation.as_ref(), Some(&fixture.generation));
      assert_eq!(candidate.nvt_hint_available, expected_nvt);
      let rebind = reader.bind_planner_coverage(&mut selected, snapshot.as_ref()).unwrap_err();
      assert_eq!(rebind.class(), NativeSelectedNamespaceReadErrorClassV1::InvalidRequest);
      assert_eq!(selected.catalogs()[0].scopes[0].indexes[0].selected_generation.as_ref(), Some(&fixture.generation));
      drop(selected);
      assert_eq!(fixture.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, before_selected);
      drop(reader);
      drop(snapshot);
      fixture.assert_released();
    }
  }
}

#[test]
fn selected_semantic_catalog_coverage_binding_is_atomic_and_absence_is_authoritative() {
  let fixture = selected_artifact_fixture(HashAlgorithm::Blake3_256);
  let reader = fixture.reader();
  let mut selected = reader.load_planner_catalogs("/", &["@hash"], default_native_selected_semantic_limits_v1()).unwrap();
  let before = fixture.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes;
  let foreign_memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap()));
  let foreign_registry = IndexCoverageRegistryV1::new(
    fixture.view.hash_algorithm(),
    [0x99; 16],
    CapabilitySetV1::from_bits(0..24).unwrap(),
    IndexCoverageRegistryOptionsV1::new(1, 1024 * 1024).unwrap(),
    Arc::clone(&foreign_memory),
  )
  .unwrap();
  let foreign = foreign_registry.snapshot().unwrap();
  let error = reader.bind_planner_coverage(&mut selected, foreign.as_ref()).unwrap_err();
  assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  assert!(selected.catalogs()[0].scopes[0].indexes[0].selected_generation.is_none());
  assert_eq!(fixture.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, before);
  let snapshot = selected_artifact_coverage_snapshot(&fixture);
  fixture.cancellation.cancel();
  let error = reader.bind_planner_coverage(&mut selected, snapshot.as_ref()).unwrap_err();
  assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::Cancelled);
  assert!(selected.catalogs()[0].scopes[0].indexes[0].selected_generation.is_none());
  assert_eq!(fixture.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, before);
  drop(snapshot);
  drop(foreign);
  drop(foreign_registry);
  assert_eq!(foreign_memory.snapshot().unwrap().reserved_bytes, 0);
  drop(selected);
  drop(reader);
  fixture.assert_released();

  let fixture = selected_artifact_fixture(HashAlgorithm::Blake3_256);
  let empty_registry = IndexCoverageRegistryV1::new(
    fixture.view.hash_algorithm(),
    fixture.view.database_id(),
    CapabilitySetV1::from_bits(0..24).unwrap(),
    IndexCoverageRegistryOptionsV1::new(1, 1024 * 1024).unwrap(),
    Arc::clone(&fixture.memory),
  )
  .unwrap();
  let empty = empty_registry.snapshot().unwrap();
  let reader = fixture.reader();
  let mut selected = reader.load_planner_catalogs("/", &["@hash"], default_native_selected_semantic_limits_v1()).unwrap();
  let before = fixture.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes;
  let ordinary_limit = 511 * 1024 * 1024;
  let memory_snapshot = fixture.memory.snapshot().unwrap();
  let blocker_bytes = ordinary_limit - memory_snapshot.accounted_bytes - 64;
  let blocker = fixture.memory.reserve(MemoryOwner::ServerCaches, blocker_bytes, AdmissionClass::Workload).unwrap();
  reader.bind_planner_coverage(&mut selected, empty.as_ref()).unwrap();
  let candidate = &selected.catalogs()[0].scopes[0].indexes[0];
  assert!(candidate.selected_generation.is_none());
  assert!(!candidate.nvt_hint_available);
  assert_eq!(fixture.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, before);
  drop(blocker);
  drop(selected);
  drop(reader);
  drop(empty);
  drop(empty_registry);
  fixture.assert_released();
}

#[test]
fn selected_semantic_catalog_rejects_invalid_limits_scope_escape_content_only_and_cancellation() {
  assert_eq!(
    NativeSelectedSemanticCountLimitsV1::new(0, 1, 1, 1, 1, 1).unwrap_err().class(),
    NativeSelectedNamespaceReadErrorClassV1::InvalidRequest,
  );
  assert_eq!(
    NativeSelectedSemanticByteLimitsV1::new(1024, 512).unwrap_err().class(),
    NativeSelectedNamespaceReadErrorClassV1::InvalidRequest,
  );
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = publisher(algorithm);
  publisher.publish(&first_request(algorithm)).unwrap();
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(256 * 1024 * 1024, 512 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
  let source = Arc::new(NativeReadViewSourceV1::new(Arc::new(publisher), Arc::clone(&memory), 86_400_000));
  let pins = RootReadPinCoordinatorV1::new(Arc::clone(&memory), algorithm, 8, 16).unwrap();
  let current = CurrentReadAuthorizationV1::new(
    CurrentPathAuthorizationV1::for_root("/docs/", CrudlifyOp::List),
    ReadViewCredentialKindV1::Ordinary,
    ReadViewConcealmentV1::Conceal,
  );
  let authorizer = ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), source.as_ref().clone());
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), pins.clone(), all_capabilities_profile());
  let cancellation = CancellationToken::new();
  let view = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &cancellation).unwrap();
  let namespace_limits = NativeSelectedNamespaceLimitsV1::new(16, 1024 * 1024, u16::MAX as usize, 32, 100_000, 10_000).unwrap();
  let reader = source.selected_namespace_reader(&view, namespace_limits).unwrap();

  let escaped = match reader.load_planner_catalogs("/", &["@hash"], default_native_selected_semantic_limits_v1()) {
    Ok(_) => panic!("selected semantic catalog escaped its authorized query path"),
    Err(error) => error,
  };
  assert_eq!(escaped.class(), NativeSelectedNamespaceReadErrorClassV1::InvalidRequest);
  let content_only = match reader.load_planner_catalogs("/docs", &["@hash"], default_native_selected_semantic_limits_v1()) {
    Ok(_) => panic!("content-only selected semantics became a complete planner catalog"),
    Err(error) => error,
  };
  assert_eq!(content_only.class(), NativeSelectedNamespaceReadErrorClassV1::Unavailable);
  assert_eq!(content_only.code(), "selected_semantic_content_only");
  cancellation.cancel();
  let cancelled = match reader.load_planner_catalogs("/docs", &["@hash"], default_native_selected_semantic_limits_v1()) {
    Ok(_) => panic!("cancelled selected semantic read continued"),
    Err(error) => error,
  };
  assert_eq!(cancelled.class(), NativeSelectedNamespaceReadErrorClassV1::Cancelled);

  drop(reader);
  drop(view);
  assert_eq!(pins.active_pin_count().unwrap(), 0);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn selected_semantic_catalog_reports_missing_fields_and_releases_memory_on_pressure() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = publisher(algorithm);
  let first = publisher.publish(&first_request(algorithm)).unwrap();
  let graph = complete_semantic_graph(algorithm);
  let complete_root =
    publish_complete_semantic_root(&publisher, algorithm, &first.namespace_root.value, first.namespace_root.root_hash, &graph);
  let publisher = Arc::new(publisher);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(256 * 1024 * 1024, 512 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
  let source = Arc::new(NativeReadViewSourceV1::new(Arc::clone(&publisher), Arc::clone(&memory), 86_400_000));
  let pins = RootReadPinCoordinatorV1::new(Arc::clone(&memory), algorithm, 8, 16).unwrap();
  let current = CurrentReadAuthorizationV1::new(
    CurrentPathAuthorizationV1::for_root("/docs/", CrudlifyOp::List),
    ReadViewCredentialKindV1::Ordinary,
    ReadViewConcealmentV1::Conceal,
  );
  let authorizer = ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), source.as_ref().clone());
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), pins.clone(), all_capabilities_profile());
  let view = resolver.resolve(ReadViewSelectorV1::ExplicitRoot(&complete_root), &authorizer, &CancellationToken::new()).unwrap();
  let namespace_limits = NativeSelectedNamespaceLimitsV1::new(16, 1024 * 1024, u16::MAX as usize, 32, 100_000, 10_000).unwrap();
  let reader = source.selected_namespace_reader(&view, namespace_limits).unwrap();
  let before = memory.snapshot().unwrap().reserved_bytes;

  let bounded_counts = NativeSelectedSemanticCountLimitsV1::new(1, 1, 1, 1, 1, 100).unwrap();
  let bounded_bytes = NativeSelectedSemanticByteLimitsV1::new(1024 * 1024, 2 * 1024 * 1024).unwrap();
  let bounded = NativeSelectedSemanticLimitsV1::new(bounded_counts, bounded_bytes);
  let count_error = match reader.load_planner_catalogs("/docs", &["@hash"], bounded) {
    Ok(_) => panic!("selected semantic catalog escaped its persisted-item bound"),
    Err(error) => error,
  };
  assert_eq!(count_error.class(), NativeSelectedNamespaceReadErrorClassV1::ResourceLimit);
  assert_eq!(count_error.code(), "selected_semantic_catalog_items");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, before);

  let definition_counts = NativeSelectedSemanticCountLimitsV1::new(1, 1, 1, 1, 100, 1000).unwrap();
  let definition_bytes = NativeSelectedSemanticByteLimitsV1::new(1, 1024).unwrap();
  let definition_limits = NativeSelectedSemanticLimitsV1::new(definition_counts, definition_bytes);
  let definition_error = match reader.load_planner_catalogs("/docs", &["@hash"], definition_limits) {
    Ok(_) => panic!("selected semantic catalog escaped its definition-byte bound"),
    Err(error) => error,
  };
  assert_eq!(definition_error.class(), NativeSelectedNamespaceReadErrorClassV1::ResourceLimit);
  assert_eq!(definition_error.code(), "selected_semantic_definition_bytes");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, before);

  let missing = match reader.load_planner_catalogs("/docs", &["@updated_at"], default_native_selected_semantic_limits_v1()) {
    Ok(_) => panic!("missing selected semantic field became an empty success"),
    Err(error) => error,
  };
  assert_eq!(missing.class(), NativeSelectedNamespaceReadErrorClassV1::InvalidRequest);
  assert_eq!(missing.code(), "selected_semantic_field_missing");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, before);
  drop(reader);
  drop(view);
  drop(source);
  assert_eq!(pins.active_pin_count().unwrap(), 0);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);

  let pressured_memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 160 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
  let pressured_source = Arc::new(NativeReadViewSourceV1::new(Arc::clone(&publisher), Arc::clone(&pressured_memory), 86_400_000));
  let pressured_pins = RootReadPinCoordinatorV1::new(Arc::clone(&pressured_memory), algorithm, 8, 16).unwrap();
  let current = CurrentReadAuthorizationV1::new(
    CurrentPathAuthorizationV1::for_root("/docs/", CrudlifyOp::List),
    ReadViewCredentialKindV1::Ordinary,
    ReadViewConcealmentV1::Conceal,
  );
  let authorizer =
    ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), pressured_source.as_ref().clone());
  let resolver = ReadViewResolverV1::new(Arc::clone(&pressured_source), pressured_pins.clone(), all_capabilities_profile());
  let view = resolver.resolve(ReadViewSelectorV1::ExplicitRoot(&complete_root), &authorizer, &CancellationToken::new()).unwrap();
  let reader = pressured_source.selected_namespace_reader(&view, namespace_limits).unwrap();
  let before = pressured_memory.snapshot().unwrap().reserved_bytes;
  let pressure = match reader.load_planner_catalogs("/docs", &["@hash"], default_native_selected_semantic_limits_v1()) {
    Ok(_) => panic!("selected semantic catalog escaped process-memory pressure"),
    Err(error) => error,
  };
  assert_eq!(pressure.class(), NativeSelectedNamespaceReadErrorClassV1::ResourceLimit);
  assert_eq!(pressured_memory.snapshot().unwrap().reserved_bytes, before);
  drop(reader);
  drop(view);
  assert_eq!(pressured_pins.active_pin_count().unwrap(), 0);
  assert_eq!(pressured_memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn selected_namespace_reader_binds_historical_rows_identity_and_query_memory_to_the_authorized_view() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = publisher(algorithm);
  let first = publisher.publish(&first_request(algorithm)).unwrap();
  let (selected_root, _) = publish_permission_tree(&publisher, algorithm, first.namespace_root.root_hash, 1, false, 0, 1);
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
  let view = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap();
  let captured_slot_sequence = view.header_slot_sequence();
  source.publisher().publish_successor_authority(&successor_request(algorithm, selected_root.clone())).unwrap();

  let limits = NativeSelectedNamespaceLimitsV1::new(16, 1024 * 1024, u16::MAX as usize, 32, 100_000, 10_000).unwrap();
  let reader = source.selected_namespace_reader(&view, limits).unwrap();
  let before_page = memory.snapshot().unwrap().owner(aeordb::engine::memory_coordinator::MemoryOwner::Query).unwrap().reserved_bytes;
  let page = reader.scan_files("/docs", None).unwrap();
  let during_page = memory.snapshot().unwrap().owner(aeordb::engine::memory_coordinator::MemoryOwner::Query).unwrap().reserved_bytes;

  assert_eq!(page.database_id(), view.database_id());
  assert_eq!(page.physical_instance_id(), view.physical_instance_id());
  assert_eq!(page.selected_root(), selected_root);
  assert_eq!(page.header_slot_sequence(), captured_slot_sequence);
  assert!(page.complete());
  assert!(page.next_resume_after().is_none());
  assert_eq!(page.rows().len(), 1);
  let row = &page.rows()[0];
  assert_eq!(row.path(), "/docs/.aeordb-permissions");
  assert_eq!(row.file_key(), digest_parts(algorithm, &[b"file:", row.path().as_bytes()]));
  assert_eq!(row.file_record().path, row.path());
  assert!(during_page > before_page);
  let file_key = row.file_key().to_vec();
  let revision = row.record_revision().to_vec();
  drop(page);
  assert_eq!(memory.snapshot().unwrap().owner(aeordb::engine::memory_coordinator::MemoryOwner::Query).unwrap().reserved_bytes, before_page);

  let identity = reader.resolve_file_identity("/docs", &file_key, &revision).unwrap();
  assert_eq!(identity.database_id(), view.database_id());
  assert_eq!(identity.physical_instance_id(), view.physical_instance_id());
  assert_eq!(identity.selected_root(), selected_root);
  assert_eq!(identity.namespace_tree_root(), view.authority().namespace_tree.root_hash);
  assert_eq!(identity.semantic_state_root(), view.authority().semantic_state.object_id);
  assert_eq!(identity.header_slot_sequence(), captured_slot_sequence);
  let resolved = identity.into_found().expect("selected historical identity was not found");
  assert_eq!(resolved.path(), "/docs/.aeordb-permissions");
  drop(resolved);
  assert_eq!(memory.snapshot().unwrap().owner(aeordb::engine::memory_coordinator::MemoryOwner::Query).unwrap().reserved_bytes, before_page);

  drop(reader);
  drop(view);
  assert_eq!(pins.active_pin_count().unwrap(), 0);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn selected_namespace_reader_scans_flat_and_btree_file_records_at_both_hash_widths() {
  for (algorithm, version, btree) in [(HashAlgorithm::Blake3_256, 0, false), (HashAlgorithm::Sha512, 1, true)] {
    let (_directory, _path, publisher) = publisher(algorithm);
    let first = publisher.publish(&first_request(algorithm)).unwrap();
    let names = if btree { vec!["a.json", "m.json", "z.json"] } else { vec!["record.json"] };
    let (selected_root, identities) =
      publish_file_tree(&publisher, algorithm, first.namespace_root.root_hash, version, btree, &names, FileTreeCorruption::None);
    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(256 * 1024 * 1024, 512 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
    let source = Arc::new(NativeReadViewSourceV1::new(Arc::new(publisher), Arc::clone(&memory), 86_400_000));
    let pins = RootReadPinCoordinatorV1::new(Arc::clone(&memory), algorithm, 8, 16).unwrap();
    let current = CurrentReadAuthorizationV1::new(
      CurrentPathAuthorizationV1::for_root("/docs/", CrudlifyOp::List),
      ReadViewCredentialKindV1::Ordinary,
      ReadViewConcealmentV1::Conceal,
    );
    let authorizer =
      ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), source.as_ref().clone());
    let resolver = ReadViewResolverV1::new(Arc::clone(&source), pins.clone(), all_capabilities_profile());
    let view = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap();
    let limits = NativeSelectedNamespaceLimitsV1::new(16, 1024 * 1024, u16::MAX as usize, 32, 100_000, 10_000).unwrap();
    let reader = source.selected_namespace_reader(&view, limits).unwrap();

    let page = reader.scan_files("/docs", None).unwrap();

    assert_eq!(page.selected_root(), selected_root);
    assert_eq!(page.rows().len(), identities.len());
    for (row, (path, revision)) in page.rows().iter().zip(&identities) {
      assert_eq!(row.path(), path);
      assert_eq!(row.record_revision(), revision);
      assert_eq!(row.file_key().len(), algorithm.hash_length());
      assert_eq!(row.record_revision().len(), algorithm.hash_length());
    }
    drop(page);
    let (last_path, last_revision) = identities.last().unwrap();
    let last_file_key = digest_parts(algorithm, &[b"file:", last_path.as_bytes()]);
    let resolved = reader.resolve_file_identity("/docs", &last_file_key, last_revision).unwrap().into_found().unwrap();
    assert_eq!(resolved.path(), last_path);
    drop(resolved);
    let outside = match reader.scan_files("/", None) {
      Ok(_) => panic!("selected namespace reader escaped its authorized request scope"),
      Err(error) => error,
    };
    assert_eq!(outside.class(), NativeSelectedNamespaceReadErrorClassV1::InvalidRequest);
    assert_eq!(outside.code(), "selected_namespace_authorization_scope");
    drop(reader);
    drop(view);
    assert_eq!(pins.active_pin_count().unwrap(), 0);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn selected_namespace_reader_pages_without_duplicates_and_never_turns_incomplete_identity_work_into_absence() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = publisher(algorithm);
  let first = publisher.publish(&first_request(algorithm)).unwrap();
  let (_selected_root, identities) = publish_file_tree(
    &publisher,
    algorithm,
    first.namespace_root.root_hash,
    1,
    false,
    &["a.json", "b.json", "c.json"],
    FileTreeCorruption::None,
  );
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
  let view = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap();
  let page_limits = NativeSelectedNamespaceLimitsV1::new(1, 1024 * 1024, u16::MAX as usize, 32, 100_000, 10_000).unwrap();
  let reader = source.selected_namespace_reader(&view, page_limits).unwrap();
  let mut resume = None;
  let mut paths = Vec::new();
  loop {
    let page = reader.scan_files("/docs", resume.as_deref()).unwrap();
    paths.extend(page.rows().iter().map(|row| row.path().to_string()));
    if page.complete() {
      break;
    }
    resume = Some(page.next_resume_after().unwrap().to_string());
  }
  assert_eq!(paths, ["/docs/a.json", "/docs/b.json", "/docs/c.json"]);
  let error = match reader.scan_files("/docs", Some("/docs/missing.json")) {
    Ok(_) => panic!("missing immutable resume path must not be accepted"),
    Err(error) => error,
  };
  assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  assert_eq!(error.code(), "selected_namespace_resume_missing");

  let file_key = digest_parts(algorithm, &[b"file:", identities[2].0.as_bytes()]);
  let wrong_revision = digest_parts(algorithm, &[b"wrong record revision"]);
  assert!(reader.resolve_file_identity("/docs", &file_key, &wrong_revision).unwrap().is_absent());
  let missing_file_key = digest_parts(algorithm, &[b"file:missing"]);
  assert!(reader.resolve_file_identity("/docs", &missing_file_key, &identities[2].1).unwrap().is_absent());

  let work_limits = NativeSelectedNamespaceLimitsV1::new(1, 1024 * 1024, u16::MAX as usize, 32, 1, 10_000).unwrap();
  let work_reader = source.selected_namespace_reader(&view, work_limits).unwrap();
  let error = match work_reader.scan_files("/docs", None) {
    Ok(_) => panic!("selected namespace B-tree work escaped the caller bound"),
    Err(error) => error,
  };
  assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "selected_namespace_work");

  let identity_limits = NativeSelectedNamespaceLimitsV1::new(1, 1024 * 1024, u16::MAX as usize, 32, 100_000, 1).unwrap();
  let bounded_reader = source.selected_namespace_reader(&view, identity_limits).unwrap();
  let error = match bounded_reader.resolve_file_identity("/docs", &file_key, &identities[2].1) {
    Ok(_) => panic!("bounded identity lookup must not claim a result after incomplete work"),
    Err(error) => error,
  };
  assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "selected_namespace_identity_count");

  drop(bounded_reader);
  drop(work_reader);
  drop(reader);
  drop(view);
  assert_eq!(pins.active_pin_count().unwrap(), 0);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn selected_namespace_reader_rejects_invalid_limits_cancellation_and_file_record_metadata_corruption() {
  let invalid = NativeSelectedNamespaceLimitsV1::new(0, 1, 1, 1, 1, 1).unwrap_err();
  assert_eq!(invalid.class(), NativeSelectedNamespaceReadErrorClassV1::InvalidRequest);
  let unaccounted_slots = NativeSelectedNamespaceLimitsV1::new(4096, 1, 1, 1, 1, 1).unwrap_err();
  assert_eq!(unaccounted_slots.class(), NativeSelectedNamespaceReadErrorClassV1::InvalidRequest);

  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = publisher(algorithm);
  let first = publisher.publish(&first_request(algorithm)).unwrap();
  publish_file_tree(&publisher, algorithm, first.namespace_root.root_hash, 1, false, &["bad.json"], FileTreeCorruption::LastMetadata);
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
  let cancellation = CancellationToken::new();
  let view = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &cancellation).unwrap();
  let limits = NativeSelectedNamespaceLimitsV1::new(16, 1024 * 1024, u16::MAX as usize, 32, 100_000, 10_000).unwrap();
  let reader = source.selected_namespace_reader(&view, limits).unwrap();

  let invalid_path = match reader.scan_files("docs", None) {
    Ok(_) => panic!("noncanonical scope must not be accepted"),
    Err(error) => error,
  };
  assert_eq!(invalid_path.class(), NativeSelectedNamespaceReadErrorClassV1::InvalidRequest);
  let corrupt = match reader.scan_files("/docs", None) {
    Ok(_) => panic!("mismatched FileRecord metadata must not be accepted"),
    Err(error) => error,
  };
  assert_eq!(corrupt.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  cancellation.cancel();
  let cancelled = match reader.scan_files("/docs", None) {
    Ok(_) => panic!("cancelled selected namespace read must not continue"),
    Err(error) => error,
  };
  assert_eq!(cancelled.class(), NativeSelectedNamespaceReadErrorClassV1::Cancelled);

  drop(reader);
  drop(view);
  assert_eq!(pins.active_pin_count().unwrap(), 0);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn selected_namespace_reader_refuses_workspace_pressure_without_leaking_its_page_reservation() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = publisher(algorithm);
  let first = publisher.publish(&first_request(algorithm)).unwrap();
  publish_file_tree(&publisher, algorithm, first.namespace_root.root_hash, 1, false, &["record.json"], FileTreeCorruption::None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 160 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
  let source = Arc::new(NativeReadViewSourceV1::new(Arc::new(publisher), Arc::clone(&memory), 86_400_000));
  let pins = RootReadPinCoordinatorV1::new(Arc::clone(&memory), algorithm, 8, 16).unwrap();
  let current = CurrentReadAuthorizationV1::new(
    CurrentPathAuthorizationV1::for_root("/docs/", CrudlifyOp::List),
    ReadViewCredentialKindV1::Ordinary,
    ReadViewConcealmentV1::Conceal,
  );
  let authorizer = ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), source.as_ref().clone());
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), pins.clone(), all_capabilities_profile());
  let view = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap();
  let limits = NativeSelectedNamespaceLimitsV1::new(1, 128 * 1024 * 1024, u16::MAX as usize, 32, 100_000, 10_000).unwrap();
  let reader = source.selected_namespace_reader(&view, limits).unwrap();
  let before = memory.snapshot().unwrap().reserved_bytes;

  let error = match reader.scan_files("/docs", None) {
    Ok(_) => panic!("selected namespace workspace exceeded the admitted process-memory limit"),
    Err(error) => error,
  };

  assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "selected_namespace_workspace_memory");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, before);
  drop(reader);
  drop(view);
  assert_eq!(pins.active_pin_count().unwrap(), 0);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn selected_namespace_reader_rejects_non_namespace_child_roles() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = publisher(algorithm);
  let first = publisher.publish(&first_request(algorithm)).unwrap();
  publish_file_tree(&publisher, algorithm, first.namespace_root.root_hash, 1, false, &["invalid-role.json"], FileTreeCorruption::LastRole);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(256 * 1024 * 1024, 512 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
  let source = Arc::new(NativeReadViewSourceV1::new(Arc::new(publisher), Arc::clone(&memory), 86_400_000));
  let pins = RootReadPinCoordinatorV1::new(Arc::clone(&memory), algorithm, 8, 16).unwrap();
  let current = CurrentReadAuthorizationV1::new(
    CurrentPathAuthorizationV1::for_root("/docs/", CrudlifyOp::List),
    ReadViewCredentialKindV1::Ordinary,
    ReadViewConcealmentV1::Conceal,
  );
  let authorizer = ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), source.as_ref().clone());
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), pins.clone(), all_capabilities_profile());
  let view = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap();
  let limits = NativeSelectedNamespaceLimitsV1::new(1, 1024 * 1024, u16::MAX as usize, 32, 100_000, 10_000).unwrap();
  let reader = source.selected_namespace_reader(&view, limits).unwrap();

  let error = match reader.scan_files("/docs", None) {
    Ok(_) => panic!("non-namespace child role must not disappear from the selected namespace scan"),
    Err(error) => error,
  };

  assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  assert_eq!(error.code(), "selected_namespace_child_role");
  drop(reader);
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
  assert_eq!(source_text.iter().map(|source| source.matches("load_index_artifact_at_captured_header(").count()).sum::<usize>(), 2,);
  let native = fs::read_to_string(source_root.join("engine/v4/read_view_native.rs")).unwrap();
  assert_eq!(native.matches("pub enum NativeSelectedSourceParserV1").count(), 1);
  assert_eq!(native.matches("pub fn prepare_authoritative_source").count(), 1);
  assert_eq!(native.matches("pub fn seek_posting_page").count(), 1);
  assert_eq!(native.matches("pub fn evaluate_authoritative_source(").count(), 0);
  assert_eq!(native.matches("impl NativeSelectedSourceEvaluatorV1").count(), 1);
  assert_eq!(native.matches("_prepared_memory: MemoryReservation").count(), 1);
  assert_eq!(native.matches("impl NativeIndexParserBodySourceV1 for CapturedSelectedParserBodySourceV1").count(), 1);
  for (needle, expected) in
    [("fn resolve_path(", 1), ("fn visit_directory_children", 1), ("fn load_file_record(", 1), ("FileRecord::deserialize(", 1)]
  {
    assert_eq!(native.matches(needle).count(), expected, "captured-header namespace primitive is not unique: {needle}");
  }
  for forbidden in ["crate::server", "DirectoryOps", "StorageEngine", "axum::", "Router<", "route("] {
    assert!(!native.contains(forbidden), "native read-view adapter gained a forbidden service/v3 dependency: {forbidden}");
  }
  let artifact = fs::read_to_string(source_root.join("engine/v4/index_artifact_native.rs")).unwrap();
  assert_eq!(artifact.matches("impl ArtifactCursorSourceV1 for CapturedNativeArtifactCursorSourceV1").count(), 1);
  assert_eq!(artifact.matches("load_artifact_page_cursor_v1(").count(), 1);
  assert_eq!(artifact.matches("load_artifact_leaf_cursor_v1(").count(), 1);
  assert_eq!(artifact.matches("load_index_artifact_at_captured_header(").count(), 1);
  assert!(!artifact.contains("BinaryCapabilityProfileV1::current()"));
  for forbidden in ["load_index_artifact_bounded(", "load_index_artifact(", "crate::server", "DirectoryOps", "StorageEngine"] {
    assert!(!artifact.contains(forbidden), "selected artifact source gained a current-authority or service bypass: {forbidden}");
  }
  let semantic = fs::read_to_string(source_root.join("engine/v4/semantic_catalog.rs")).unwrap();
  let producer = fs::read_to_string(source_root.join("engine/v4/index_semantic_source.rs")).unwrap();
  assert_eq!(semantic.matches("pub fn walk_catalog(").count(), 1);
  assert_eq!(semantic.matches("decode_semantic_catalog_node(").count(), 1);
  assert_eq!(semantic.matches("decode_semantic_definition_record(").count(), 1);
  for adapter in [&native, &producer] {
    assert!(!adapter.contains("decode_semantic_catalog_node("), "semantic adapter bypassed the shared bounded catalog walker");
    assert!(!adapter.contains("decode_semantic_definition_record("), "semantic adapter bypassed shared definition closure");
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
fn selected_artifact_cursor_uses_captured_authority_and_retains_query_memory_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let fixture = selected_artifact_fixture(algorithm);
    let later_record = encode_posting_record(&PostingRecordV1 {
      tombstone: false,
      coordinate: 29,
      document_ordinal: 2,
      source_value_ordinal: 0,
      expansion_ordinal: 0,
      posting_key: &29u64.to_le_bytes(),
    })
    .unwrap();
    let later_page = encode_ordered_page(&OrderedPageWriteV1 {
      hash_algorithm: algorithm,
      role: OrderedIndexRoleV1::Posting,
      owner_id: &fixture.generation.owner_id,
      generation: 8,
      page_id: 2,
      previous_page_id: 0,
      next_page_id: 0,
      records: &[later_record.as_slice()],
    })
    .unwrap();
    fixture
      .source
      .publisher()
      .publish_index_artifacts(IndexArtifactBatchPublicationRequestV1 {
        database_id: &[0x31; 16],
        artifacts: &[&later_page],
        publication_timestamp_ms: 1_700_000_000_400,
      })
      .unwrap();
    let captured_error = fixture
      .source
      .publisher()
      .load_index_artifact_at_captured_header(
        fixture.view.captured_header(),
        &later_page.key,
        later_page.value.len(),
        &fixture.cancellation,
      )
      .unwrap_err();
    assert_eq!(captured_error.code(), "immutable_index_locator_range");

    let before = fixture.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes;
    let reader = fixture.reader();
    let selected = reader
      .load_index_artifact_page_cursor(&selected_artifact_cursor_request(
        &fixture.catalog,
        &fixture.scope_id,
        &fixture.generation,
        OrderedIndexRoleV1::Posting,
        ArtifactPageNeighborModeV1::Both,
        ArtifactPageCursorLimitsV1::new(4, 2 * 1024 * 1024).unwrap(),
      ))
      .unwrap()
      .unwrap();
    assert_eq!(selected.selected_root(), fixture.view.root_metadata().hash);
    assert_eq!(selected.manifest_hash(), fixture.generation.manifest_hash);
    assert_eq!(selected.owner_id(), fixture.generation.owner_id);
    assert_eq!(selected.root_key(), fixture.posting_root.key);
    assert_eq!(selected.generation(), fixture.generation.generation);
    assert_eq!(selected.role(), OrderedIndexRoleV1::Posting);
    assert_eq!(selected.cursor().page(), fixture.posting_page.value);
    assert!(selected.cursor().previous_page().is_none());
    assert!(selected.cursor().next_page().is_none());
    assert!(fixture.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes > before);
    drop(selected);
    let empty_state = reader
      .load_index_artifact_page_cursor(&selected_artifact_cursor_request(
        &fixture.catalog,
        &fixture.scope_id,
        &fixture.generation,
        OrderedIndexRoleV1::IndexDocumentState,
        ArtifactPageNeighborModeV1::None,
        ArtifactPageCursorLimitsV1::new(4, 2 * 1024 * 1024).unwrap(),
      ))
      .unwrap();
    assert!(empty_state.is_none());
    let mut beyond = selected_artifact_cursor_request(
      &fixture.catalog,
      &fixture.scope_id,
      &fixture.generation,
      OrderedIndexRoleV1::Posting,
      ArtifactPageNeighborModeV1::None,
      ArtifactPageCursorLimitsV1::new(4, 2 * 1024 * 1024).unwrap(),
    );
    beyond.seek = ArtifactPageSeekV1::PageOrdinal(1);
    let beyond_error = reader.load_index_artifact_page_cursor(&beyond).unwrap_err();
    assert_eq!(beyond_error.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
    assert_eq!(beyond_error.code(), "artifact_cursor_rank");
    drop(reader);
    assert_eq!(fixture.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, before);
    fixture.assert_released();
  }

  let partial = selected_partial_artifact_fixture(HashAlgorithm::Blake3_256);
  assert_ne!(partial.generation.source_namespace_root, partial.view.root_metadata().hash);
  let reader = partial.reader();
  let selected = reader
    .load_index_artifact_page_cursor(&selected_artifact_cursor_request(
      &partial.catalog,
      &partial.scope_id,
      &partial.generation,
      OrderedIndexRoleV1::Posting,
      ArtifactPageNeighborModeV1::None,
      ArtifactPageCursorLimitsV1::new(4, 2 * 1024 * 1024).unwrap(),
    ))
    .unwrap()
    .unwrap();
  assert_eq!(selected.selected_root(), partial.view.root_metadata().hash);
  assert_eq!(selected.coverage_source_root(), partial.generation.source_namespace_root);
  assert_eq!(selected.cursor().page(), partial.posting_page.value);
  drop(selected);
  drop(reader);
  partial.assert_released();
}

#[test]
fn selected_artifact_root_adapter_resolves_posting_and_exact_dependent_scope_roots_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let fixture = selected_artifact_fixture(algorithm);
    let reader = fixture.reader();
    let before = fixture.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes;

    let posting = reader.load_index_artifact_root(&selected_artifact_root_request(&fixture, OrderedIndexRoleV1::Posting)).unwrap().unwrap();
    assert_eq!(posting.root_key(), fixture.posting_root.key);
    assert_eq!(posting.owner_id(), fixture.generation.owner_id);
    assert_eq!(posting.generation(), fixture.generation.generation);
    assert_eq!(posting.role(), OrderedIndexRoleV1::Posting);
    assert_eq!(
      posting.summary(),
      ArtifactDirectoryRootSummaryV1::from_directory(&decode_artifact_directory(&fixture.posting_root.value, algorithm).unwrap())
    );

    let scope =
      reader.load_index_artifact_root(&selected_artifact_root_request(&fixture, OrderedIndexRoleV1::ScopeOrdinal)).unwrap().unwrap();
    assert_eq!(scope.root_key(), fixture.scope_ordinal_root.key);
    assert_eq!(scope.owner_id(), fixture.scope_id);
    assert_eq!(scope.generation(), 7);
    assert_eq!(scope.role(), OrderedIndexRoleV1::ScopeOrdinal);
    assert_eq!(
      scope.summary(),
      ArtifactDirectoryRootSummaryV1::from_directory(&decode_artifact_directory(&fixture.scope_ordinal_root.value, algorithm).unwrap())
    );

    drop(posting);
    drop(scope);
    assert_eq!(fixture.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, before);
    drop(reader);
    fixture.assert_released();
  }

  let partial = selected_partial_artifact_fixture(HashAlgorithm::Blake3_256);
  assert_ne!(partial.generation.source_namespace_root, partial.view.root_metadata().hash);
  let reader = partial.reader();
  let scope =
    reader.load_index_artifact_root(&selected_artifact_root_request(&partial, OrderedIndexRoleV1::ScopeOrdinal)).unwrap().unwrap();
  assert_eq!(scope.root_key(), partial.scope_ordinal_root.key);
  assert_eq!(scope.owner_id(), partial.scope_id);
  drop(scope);
  drop(reader);
  partial.assert_released();
}

#[test]
fn selected_artifact_root_adapter_fails_closed_and_releases_memory_on_every_boundary() {
  let empty = selected_artifact_fixture(HashAlgorithm::Blake3_256);
  let reader = empty.reader();
  assert!(reader
    .load_index_artifact_root(&selected_artifact_root_request(&empty, OrderedIndexRoleV1::IndexDocumentState))
    .unwrap()
    .is_none());
  let role_error = reader.load_index_artifact_root(&selected_artifact_root_request(&empty, OrderedIndexRoleV1::Value)).unwrap_err();
  assert_eq!(role_error.class(), NativeSelectedNamespaceReadErrorClassV1::InvalidRequest);
  assert_eq!(role_error.code(), "selected_artifact_catalog_role");
  drop(reader);
  empty.assert_released();

  let mismatched = selected_artifact_fixture_with_manifest_live_delta(HashAlgorithm::Blake3_256, 1);
  let reader = mismatched.reader();
  let mismatch_error =
    reader.load_index_artifact_root(&selected_artifact_root_request(&mismatched, OrderedIndexRoleV1::Posting)).unwrap_err();
  assert_eq!(mismatch_error.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  assert_eq!(mismatch_error.code(), "selected_artifact_root_manifest_closure");
  drop(reader);
  mismatched.assert_released();

  let corrupt = selected_artifact_fixture(HashAlgorithm::Blake3_256);
  corrupt_artifact_checksum(&corrupt, &corrupt.posting_root.key);
  let reader = corrupt.reader();
  let corrupt_error = reader.load_index_artifact_root(&selected_artifact_root_request(&corrupt, OrderedIndexRoleV1::Posting)).unwrap_err();
  assert_eq!(corrupt_error.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  drop(reader);
  corrupt.assert_released();

  let truncated = selected_artifact_fixture(HashAlgorithm::Blake3_256);
  truncate_artifact_source(&truncated, &truncated.posting_root.key);
  let reader = truncated.reader();
  let truncated_error =
    reader.load_index_artifact_root(&selected_artifact_root_request(&truncated, OrderedIndexRoleV1::Posting)).unwrap_err();
  assert_eq!(truncated_error.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  drop(reader);
  truncated.assert_released();

  let cancelled = selected_artifact_fixture(HashAlgorithm::Blake3_256);
  cancelled.cancellation.cancel();
  let reader = cancelled.reader();
  let cancelled_error =
    reader.load_index_artifact_root(&selected_artifact_root_request(&cancelled, OrderedIndexRoleV1::Posting)).unwrap_err();
  assert_eq!(cancelled_error.class(), NativeSelectedNamespaceReadErrorClassV1::Cancelled);
  drop(reader);
  cancelled.assert_released();

  let pressured = selected_artifact_fixture(HashAlgorithm::Blake3_256);
  let baseline_query = pressured.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes;
  let snapshot = pressured.memory.snapshot().unwrap();
  let ordinary_limit = 511 * 1024 * 1024;
  let blocker_bytes = ordinary_limit - snapshot.accounted_bytes - 512 * 1024;
  let blocker = pressured.memory.reserve(MemoryOwner::ServerCaches, blocker_bytes, AdmissionClass::Workload).unwrap();
  let reader = pressured.reader();
  let pressure_error =
    reader.load_index_artifact_root(&selected_artifact_root_request(&pressured, OrderedIndexRoleV1::Posting)).unwrap_err();
  assert_eq!(pressure_error.class(), NativeSelectedNamespaceReadErrorClassV1::ResourceLimit);
  assert_eq!(pressure_error.code(), "selected_artifact_read_memory");
  assert_eq!(pressured.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, baseline_query);
  drop(reader);
  drop(blocker);
  pressured.assert_released();
}

#[test]
fn native_candidate_artifact_source_resolves_complete_and_partial_roots_through_captured_bytes() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let complete = native_candidate_artifact_fixture(algorithm, false);
    let mut source = complete.source.open_candidate_artifact_source();
    complete.advance_mutable_authority();
    let posting = source
      .resolve_complete_posting_root_v1(QueryCompletePostingRootRequestV1 {
        selected_namespace_root: complete.plan.selected_namespace_root(),
        publication_sequence: complete.plan.publication_sequence(),
        scope_id: &complete.scope_id,
        candidate: complete.candidate(),
        cancellation: &complete.cancellation,
      })
      .unwrap();
    assert!(posting.complete);
    assert_eq!(posting.root.as_ref().unwrap().root_key(), complete.posting_root.key);
    let scope = source
      .resolve_complete_scope_root_v1(QueryCompleteScopeRootRequestV1 {
        selected_namespace_root: complete.plan.selected_namespace_root(),
        publication_sequence: complete.plan.publication_sequence(),
        scope_id: &complete.scope_id,
        cancellation: &complete.cancellation,
      })
      .unwrap();
    assert!(scope.complete);
    assert_eq!(scope.root.as_ref().unwrap().root_key(), complete.scope_ordinal_root.key);
    let page = source.read_immutable_artifact(&complete.posting_page.key, 1024 * 1024).unwrap();
    assert_eq!(decode_ordered_page(page.bytes(), algorithm).unwrap().role, OrderedIndexRoleV1::Posting);
    drop(page);
    drop(source);
    complete.assert_released();

    let partial = native_candidate_artifact_fixture(algorithm, true);
    assert_ne!(partial.plan.selected_namespace_root(), partial.generation.source_namespace_root);
    let mut source = partial.source.open_candidate_artifact_source();
    let posting = source
      .resolve_partial_posting_root(QueryPartialPostingRootRequestV1 {
        target_namespace_root: partial.plan.selected_namespace_root(),
        target_publication_sequence: partial.plan.publication_sequence(),
        source_namespace_root: &partial.generation.source_namespace_root,
        source_publication_sequence: partial.generation.coverage_publication_sequence,
        scope_id: &partial.scope_id,
        candidate: partial.candidate(),
        cancellation: &partial.cancellation,
      })
      .unwrap();
    assert!(posting.complete);
    assert_eq!(posting.root.as_ref().unwrap().root_key(), partial.posting_root.key);
    let scope = source
      .resolve_partial_scope_root(QueryPartialScopeRootRequestV1 {
        source_namespace_root: &partial.generation.source_namespace_root,
        source_publication_sequence: partial.generation.coverage_publication_sequence,
        scope_id: &partial.scope_id,
        cancellation: &partial.cancellation,
      })
      .unwrap();
    assert!(scope.complete);
    assert_eq!(scope.root.as_ref().unwrap().root_key(), partial.scope_ordinal_root.key);
    drop(source);
    partial.assert_released();
  }
}

#[test]
fn native_candidate_artifact_source_fails_closed_and_retains_exact_byte_memory() {
  let substituted = native_candidate_artifact_fixture(HashAlgorithm::Blake3_256, false);
  let foreign = native_candidate_artifact_fixture(HashAlgorithm::Blake3_256, true);
  let mut source = substituted.source.open_candidate_artifact_source();
  let error = source
    .resolve_complete_posting_root_v1(QueryCompletePostingRootRequestV1 {
      selected_namespace_root: substituted.plan.selected_namespace_root(),
      publication_sequence: substituted.plan.publication_sequence(),
      scope_id: &substituted.scope_id,
      candidate: foreign.candidate(),
      cancellation: &substituted.cancellation,
    })
    .unwrap_err();
  assert_eq!(error.class(), QueryExecutionSourceErrorClassV1::Corrupt);
  assert_eq!(error.code(), "native_candidate_generation_interval");
  drop(source);
  substituted.assert_released();
  foreign.assert_released();

  let unavailable = native_candidate_artifact_fixture(HashAlgorithm::Blake3_256, true);
  let mut source = unavailable.source.open_candidate_artifact_source();
  let missing_root = vec![0x55; unavailable.plan.hash_algorithm().hash_length()];
  let error = source
    .resolve_partial_scope_root(QueryPartialScopeRootRequestV1 {
      source_namespace_root: &missing_root,
      source_publication_sequence: unavailable.generation.coverage_publication_sequence,
      scope_id: &unavailable.scope_id,
      cancellation: &unavailable.cancellation,
    })
    .unwrap_err();
  assert_eq!(error.class(), IndexPartialSourceErrorClassV1::Unavailable);
  assert_eq!(error.code(), "native_candidate_scope_generation_missing");
  drop(source);
  unavailable.assert_released();

  let corrupt = native_candidate_artifact_fixture(HashAlgorithm::Blake3_256, false);
  corrupt.corrupt_artifact_checksum(&corrupt.posting_root.key);
  let mut source = corrupt.source.open_candidate_artifact_source();
  let error = source
    .resolve_complete_posting_root_v1(QueryCompletePostingRootRequestV1 {
      selected_namespace_root: corrupt.plan.selected_namespace_root(),
      publication_sequence: corrupt.plan.publication_sequence(),
      scope_id: &corrupt.scope_id,
      candidate: corrupt.candidate(),
      cancellation: &corrupt.cancellation,
    })
    .unwrap_err();
  assert_eq!(error.class(), QueryExecutionSourceErrorClassV1::Corrupt);
  drop(source);
  corrupt.assert_released();

  let pressured = native_candidate_artifact_fixture(HashAlgorithm::Blake3_256, false);
  let snapshot = pressured.memory.snapshot().unwrap();
  let blocker_bytes = 511 * 1024 * 1024 - snapshot.accounted_bytes - 512 * 1024;
  let blocker = pressured.memory.reserve(MemoryOwner::ServerCaches, blocker_bytes, AdmissionClass::Workload).unwrap();
  let mut source = pressured.source.open_candidate_artifact_source();
  let error = source
    .resolve_complete_posting_root_v1(QueryCompletePostingRootRequestV1 {
      selected_namespace_root: pressured.plan.selected_namespace_root(),
      publication_sequence: pressured.plan.publication_sequence(),
      scope_id: &pressured.scope_id,
      candidate: pressured.candidate(),
      cancellation: &pressured.cancellation,
    })
    .unwrap_err();
  assert_eq!(error.class(), QueryExecutionSourceErrorClassV1::ResourceLimit);
  drop(source);
  drop(blocker);
  pressured.assert_released();

  let cancelled = native_candidate_artifact_fixture(HashAlgorithm::Blake3_256, false);
  let mut source = cancelled.source.open_candidate_artifact_source();
  cancelled.cancellation.cancel();
  let error = source
    .resolve_complete_scope_root_v1(QueryCompleteScopeRootRequestV1 {
      selected_namespace_root: cancelled.plan.selected_namespace_root(),
      publication_sequence: cancelled.plan.publication_sequence(),
      scope_id: &cancelled.scope_id,
      cancellation: &cancelled.cancellation,
    })
    .unwrap_err();
  assert_eq!(error.class(), QueryExecutionSourceErrorClassV1::Cancelled);
  assert!(matches!(
    source.read_immutable_artifact(&cancelled.posting_page.key, 1024 * 1024).unwrap_err(),
    ArtifactCursorReadErrorV1::Cancelled
  ));
  drop(source);
  cancelled.assert_released();

  let retained = native_candidate_artifact_fixture(HashAlgorithm::Blake3_256, false);
  let mut source = retained.source.open_candidate_artifact_source();
  let bytes = source.read_immutable_artifact(&retained.posting_page.key, 1024 * 1024).unwrap();
  assert!(matches!(
    source.read_immutable_artifact(&retained.posting_page.key, 1).unwrap_err(),
    ArtifactCursorReadErrorV1::ResourcePressure(_)
  ));
  drop(source);
  let NativeCandidateArtifactFixture { source, backing_source, memory, pins, .. } = retained;
  drop(source);
  drop(backing_source);
  assert_eq!(pins.active_pin_count().unwrap(), 0);
  assert!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes > 0);
  drop(bytes);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn selected_posting_seek_without_nvt_uses_exact_directory_and_rejects_a_malformed_target() {
  let fixture = selected_artifact_fixture(HashAlgorithm::Blake3_256);
  let target_posting_position = decode_ordered_page(&fixture.posting_page.value, HashAlgorithm::Blake3_256).unwrap().lower_fence.to_vec();
  let reader = fixture.reader();
  let request = NativeSelectedPostingSeekRequestV1 {
    catalog: &fixture.catalog,
    scope_id: &fixture.scope_id,
    selected_generation: &fixture.generation,
    nvt_descriptor: None,
    target_coordinate: 17,
    target_posting_position: &target_posting_position,
    neighbors: ArtifactPageNeighborModeV1::None,
    limits: ArtifactPageCursorLimitsV1::new(4, 2 * 1024 * 1024).unwrap(),
  };
  let selected = reader.seek_posting_page(&request).unwrap().unwrap();
  assert_eq!(selected.source(), NativeSelectedPostingSeekSourceV1::ExactDirectory);
  assert_eq!(selected.nvt_fallback().map(|fallback| fallback.reason()), Some(NativeSelectedNvtFallbackReasonV1::Absent));
  assert_eq!(selected.nvt_fallback().and_then(|fallback| fallback.diagnostic_code()), None);
  drop(selected);

  let malformed = NativeSelectedPostingSeekRequestV1 { target_posting_position: &[0], ..request };
  let error = reader.seek_posting_page(&malformed).unwrap_err();
  assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::InvalidRequest);
  assert_eq!(error.code(), "selected_posting_target");
  drop(reader);
  fixture.assert_released();
}

#[test]
fn selected_native_nvt_returns_a_validated_posting_start_without_exact_fallback() {
  let fixture = selected_valid_nvt_artifact_fixture(HashAlgorithm::Blake3_256);
  let reader = fixture.reader();
  let selected = reader
    .seek_posting_page(&NativeSelectedPostingSeekRequestV1 {
      catalog: &fixture.catalog,
      scope_id: &fixture.scope_id,
      selected_generation: &fixture.generation,
      nvt_descriptor: fixture.nvt_descriptor.as_ref(),
      target_coordinate: 20,
      target_posting_position: fixture.target_posting_position.as_deref().unwrap(),
      neighbors: ArtifactPageNeighborModeV1::Both,
      limits: ArtifactPageCursorLimitsV1::new(4, 2 * 1024 * 1024).unwrap(),
    })
    .unwrap()
    .unwrap();

  assert_eq!(selected.source(), NativeSelectedPostingSeekSourceV1::NvtHint);
  assert!(selected.nvt_fallback().is_none());
  assert_eq!(decode_ordered_page(selected.cursor().cursor().page(), HashAlgorithm::Blake3_256).unwrap().page_id, 1);
  drop(selected);
  drop(reader);
  fixture.assert_released();
}

#[test]
fn selected_native_nvt_rejects_a_structural_stale_page_and_uses_the_exact_posting_predecessor() {
  let fixture = selected_stale_nvt_artifact_fixture(HashAlgorithm::Blake3_256);
  let before = fixture.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes;
  let reader = fixture.reader();
  let selected = reader
    .seek_posting_page(&NativeSelectedPostingSeekRequestV1 {
      catalog: &fixture.catalog,
      scope_id: &fixture.scope_id,
      selected_generation: &fixture.generation,
      nvt_descriptor: fixture.nvt_descriptor.as_ref(),
      target_coordinate: 20,
      target_posting_position: fixture.target_posting_position.as_deref().unwrap(),
      neighbors: ArtifactPageNeighborModeV1::Both,
      limits: ArtifactPageCursorLimitsV1::new(4, 2 * 1024 * 1024).unwrap(),
    })
    .unwrap()
    .unwrap();

  assert_eq!(selected.source(), NativeSelectedPostingSeekSourceV1::ExactDirectory);
  assert_eq!(selected.nvt_fallback().map(|fallback| fallback.reason()), Some(NativeSelectedNvtFallbackReasonV1::StalePageHint));
  assert_eq!(selected.nvt_fallback().and_then(|fallback| fallback.diagnostic_code()), None);
  assert_eq!(decode_ordered_page(selected.cursor().cursor().page(), HashAlgorithm::Blake3_256).unwrap().page_id, 1);
  assert!(selected.cursor().cursor().previous_page().is_none());
  assert_eq!(decode_ordered_page(selected.cursor().cursor().next_page().unwrap(), HashAlgorithm::Blake3_256).unwrap().page_id, 2);
  assert!(fixture.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes > before);
  drop(selected);
  drop(reader);
  assert_eq!(fixture.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, before);
  fixture.assert_released();
}

#[test]
fn selected_native_nvt_two_level_bidirectional_start_is_complete_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let fixture = selected_artifact_fixture_with_layout(algorithm, SelectedArtifactLayoutV1::TwoLevelNvt);
    let reader = fixture.reader();
    let selected = reader
      .seek_posting_page(&NativeSelectedPostingSeekRequestV1 {
        catalog: &fixture.catalog,
        scope_id: &fixture.scope_id,
        selected_generation: &fixture.generation,
        nvt_descriptor: fixture.nvt_descriptor.as_ref(),
        target_coordinate: fixture.target_coordinate.unwrap(),
        target_posting_position: fixture.target_posting_position.as_deref().unwrap(),
        neighbors: ArtifactPageNeighborModeV1::Both,
        limits: ArtifactPageCursorLimitsV1::new(4, 2 * 1024 * 1024).unwrap(),
      })
      .unwrap()
      .unwrap();

    assert_eq!(selected.source(), NativeSelectedPostingSeekSourceV1::NvtHint);
    assert!(selected.nvt_fallback().is_none());
    let cursor = selected.cursor().cursor();
    let previous = decode_ordered_page(cursor.previous_page().unwrap(), algorithm).unwrap();
    let current = decode_ordered_page(cursor.page(), algorithm).unwrap();
    let next = decode_ordered_page(cursor.next_page().unwrap(), algorithm).unwrap();
    assert_eq!([previous.page_id, current.page_id, next.page_id], [1, 2, 3]);
    assert!(previous.upper_fence < current.lower_fence);
    assert!(current.upper_fence < next.lower_fence);
    assert_eq!(previous.next_page_id, current.page_id);
    assert_eq!(current.previous_page_id, previous.page_id);
    assert_eq!(current.next_page_id, next.page_id);
    assert_eq!(next.previous_page_id, current.page_id);
    drop(selected);
    drop(reader);
    fixture.assert_released();
  }
}

#[test]
fn selected_native_nvt_optional_failures_preserve_the_exact_posting_answer() {
  let corrupt_tile = selected_artifact_fixture_with_layout(HashAlgorithm::Blake3_256, SelectedArtifactLayoutV1::TwoLevelNvt);
  corrupt_artifact_checksum(&corrupt_tile, &corrupt_tile.nvt_tile.as_ref().unwrap().key);
  assert_selected_nvt_exact_fallback(
    corrupt_tile,
    ArtifactPageCursorLimitsV1::new(4, 2 * 1024 * 1024).unwrap(),
    NativeSelectedNvtFallbackReasonV1::Corrupt,
  );

  let truncated = selected_artifact_fixture_with_layout(HashAlgorithm::Blake3_256, SelectedArtifactLayoutV1::TwoLevelNvt);
  truncate_artifact_source(&truncated, &truncated.nvt_manifest.as_ref().unwrap().key);
  assert_selected_nvt_exact_fallback(
    truncated,
    ArtifactPageCursorLimitsV1::new(4, 2 * 1024 * 1024).unwrap(),
    NativeSelectedNvtFallbackReasonV1::Corrupt,
  );

  for (layout, limits, reason) in [
    (
      SelectedArtifactLayoutV1::CorruptNvtParent,
      ArtifactPageCursorLimitsV1::new(4, 2 * 1024 * 1024).unwrap(),
      NativeSelectedNvtFallbackReasonV1::Corrupt,
    ),
    (
      SelectedArtifactLayoutV1::LargeNvt,
      ArtifactPageCursorLimitsV1::new(4, 4 * 1024).unwrap(),
      NativeSelectedNvtFallbackReasonV1::ResourceLimit,
    ),
  ] {
    assert_selected_nvt_exact_fallback(selected_artifact_fixture_with_layout(HashAlgorithm::Blake3_256, layout), limits, reason);
  }
}

#[test]
fn selected_native_nvt_cancellation_is_never_converted_into_exact_fallback() {
  let fixture = selected_artifact_fixture_with_layout(HashAlgorithm::Blake3_256, SelectedArtifactLayoutV1::TwoLevelNvt);
  fixture.cancellation.cancel();
  let reader = fixture.reader();
  let error = reader
    .seek_posting_page(&NativeSelectedPostingSeekRequestV1 {
      catalog: &fixture.catalog,
      scope_id: &fixture.scope_id,
      selected_generation: &fixture.generation,
      nvt_descriptor: fixture.nvt_descriptor.as_ref(),
      target_coordinate: fixture.target_coordinate.unwrap(),
      target_posting_position: fixture.target_posting_position.as_deref().unwrap(),
      neighbors: ArtifactPageNeighborModeV1::Both,
      limits: ArtifactPageCursorLimitsV1::new(4, 2 * 1024 * 1024).unwrap(),
    })
    .unwrap_err();
  assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::Cancelled);
  drop(reader);
  fixture.assert_released();
}

#[test]
fn selected_native_nvt_never_masks_correctness_bearing_posting_corruption() {
  for layout in [SelectedArtifactLayoutV1::CorruptPostingParent, SelectedArtifactLayoutV1::BrokenPostingLink] {
    let fixture = selected_artifact_fixture_with_layout(HashAlgorithm::Blake3_256, layout);
    let reader = fixture.reader();
    let error = reader
      .seek_posting_page(&NativeSelectedPostingSeekRequestV1 {
        catalog: &fixture.catalog,
        scope_id: &fixture.scope_id,
        selected_generation: &fixture.generation,
        nvt_descriptor: fixture.nvt_descriptor.as_ref(),
        target_coordinate: fixture.target_coordinate.unwrap(),
        target_posting_position: fixture.target_posting_position.as_deref().unwrap(),
        neighbors: ArtifactPageNeighborModeV1::Both,
        limits: ArtifactPageCursorLimitsV1::new(4, 2 * 1024 * 1024).unwrap(),
      })
      .unwrap_err();
    assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
    drop(reader);
    fixture.assert_released();
  }
}

#[test]
fn selected_artifact_cursor_rejects_catalog_manifest_and_root_closure_drift() {
  let fixture = selected_artifact_fixture(HashAlgorithm::Blake3_256);
  let reader = fixture.reader();
  let limits = ArtifactPageCursorLimitsV1::new(4, 2 * 1024 * 1024).unwrap();
  let mut fingerprint_generation = fixture.generation.clone();
  fingerprint_generation.definition_fingerprint[0] ^= 0xff;
  let mut fingerprint_catalog = fixture.catalog.clone();
  fingerprint_catalog.scopes[0].indexes[0].selected_generation = Some(fingerprint_generation.clone());
  let fingerprint_error = reader
    .load_index_artifact_page_cursor(&selected_artifact_cursor_request(
      &fingerprint_catalog,
      &fixture.scope_id,
      &fingerprint_generation,
      OrderedIndexRoleV1::Posting,
      ArtifactPageNeighborModeV1::None,
      limits,
    ))
    .unwrap_err();
  assert_eq!(fingerprint_error.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  assert_eq!(fingerprint_error.code(), "selected_artifact_catalog_fingerprint");

  let mut missing_generation = fixture.generation.clone();
  missing_generation.manifest_hash = vec![0xa9; HashAlgorithm::Blake3_256.hash_length()];
  let mut missing_catalog = fixture.catalog.clone();
  missing_catalog.scopes[0].indexes[0].selected_generation = Some(missing_generation.clone());
  let missing_error = reader
    .load_index_artifact_page_cursor(&selected_artifact_cursor_request(
      &missing_catalog,
      &fixture.scope_id,
      &missing_generation,
      OrderedIndexRoleV1::Posting,
      ArtifactPageNeighborModeV1::None,
      limits,
    ))
    .unwrap_err();
  assert_eq!(missing_error.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  assert_eq!(missing_error.code(), "selected_artifact_manifest_missing");

  let catalog_error = reader
    .load_index_artifact_page_cursor(&selected_artifact_cursor_request(
      &fixture.catalog,
      &fixture.scope_id,
      &fingerprint_generation,
      OrderedIndexRoleV1::Posting,
      ArtifactPageNeighborModeV1::None,
      limits,
    ))
    .unwrap_err();
  assert_eq!(catalog_error.code(), "selected_artifact_generation_catalog");
  let mut semantic_catalog = fixture.catalog.clone();
  semantic_catalog.scopes[0].indexes[0].encoded_field_definition[0] ^= 0xff;
  let semantic_error = reader
    .load_index_artifact_page_cursor(&selected_artifact_cursor_request(
      &semantic_catalog,
      &fixture.scope_id,
      &fixture.generation,
      OrderedIndexRoleV1::Posting,
      ArtifactPageNeighborModeV1::None,
      limits,
    ))
    .unwrap_err();
  assert_eq!(semantic_error.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  assert_eq!(semantic_error.code(), "selected_artifact_catalog_fingerprint");
  let mut foreign_catalog = fixture.catalog.clone();
  foreign_catalog.selected_namespace_root[0] ^= 0xff;
  let foreign_error = reader
    .load_index_artifact_page_cursor(&selected_artifact_cursor_request(
      &foreign_catalog,
      &fixture.scope_id,
      &fixture.generation,
      OrderedIndexRoleV1::Posting,
      ArtifactPageNeighborModeV1::None,
      limits,
    ))
    .unwrap_err();
  assert_eq!(foreign_error.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  assert_eq!(foreign_error.code(), "selected_artifact_catalog_authority");
  let mut future_generation = fixture.generation.clone();
  future_generation.coverage_publication_sequence = fixture.catalog.publication_sequence + 1;
  let mut future_catalog = fixture.catalog.clone();
  future_catalog.scopes[0].indexes[0].selected_generation = Some(future_generation.clone());
  let future_error = reader
    .load_index_artifact_page_cursor(&selected_artifact_cursor_request(
      &future_catalog,
      &fixture.scope_id,
      &future_generation,
      OrderedIndexRoleV1::Posting,
      ArtifactPageNeighborModeV1::None,
      limits,
    ))
    .unwrap_err();
  assert_eq!(future_error.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  assert_eq!(future_error.code(), "selected_artifact_generation_interval");
  let mut foreign_source_generation = fixture.generation.clone();
  foreign_source_generation.source_namespace_root = vec![0xd7; HashAlgorithm::Blake3_256.hash_length()];
  let mut foreign_source_catalog = fixture.catalog.clone();
  foreign_source_catalog.scopes[0].indexes[0].selected_generation = Some(foreign_source_generation.clone());
  let foreign_source_error = reader
    .load_index_artifact_page_cursor(&selected_artifact_cursor_request(
      &foreign_source_catalog,
      &fixture.scope_id,
      &foreign_source_generation,
      OrderedIndexRoleV1::Posting,
      ArtifactPageNeighborModeV1::None,
      limits,
    ))
    .unwrap_err();
  assert_eq!(foreign_source_error.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  assert_eq!(foreign_source_error.code(), "selected_artifact_manifest_identity");
  let role_error = reader
    .load_index_artifact_page_cursor(&selected_artifact_cursor_request(
      &fixture.catalog,
      &fixture.scope_id,
      &fixture.generation,
      OrderedIndexRoleV1::Value,
      ArtifactPageNeighborModeV1::None,
      limits,
    ))
    .unwrap_err();
  assert_eq!(role_error.class(), NativeSelectedNamespaceReadErrorClassV1::InvalidRequest);
  assert_eq!(role_error.code(), "selected_artifact_catalog_role");
  drop(reader);
  fixture.assert_released();

  let degraded_partial = selected_partial_artifact_fixture(HashAlgorithm::Blake3_256);
  let mut degraded_generation = degraded_partial.generation.clone();
  degraded_generation.health = IndexCoverageGenerationHealthV1::Degraded;
  let mut degraded_catalog = degraded_partial.catalog.clone();
  degraded_catalog.scopes[0].indexes[0].selected_generation = Some(degraded_generation.clone());
  let reader = degraded_partial.reader();
  let degraded_error = reader
    .load_index_artifact_page_cursor(&selected_artifact_cursor_request(
      &degraded_catalog,
      &degraded_partial.scope_id,
      &degraded_generation,
      OrderedIndexRoleV1::Posting,
      ArtifactPageNeighborModeV1::None,
      limits,
    ))
    .unwrap_err();
  assert_eq!(degraded_error.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  assert_eq!(degraded_error.code(), "selected_artifact_generation_interval");
  drop(reader);
  degraded_partial.assert_released();

  let mismatched = selected_artifact_fixture_with_manifest_live_delta(HashAlgorithm::Blake3_256, 1);
  let reader = mismatched.reader();
  let root_error = reader
    .load_index_artifact_page_cursor(&selected_artifact_cursor_request(
      &mismatched.catalog,
      &mismatched.scope_id,
      &mismatched.generation,
      OrderedIndexRoleV1::Posting,
      ArtifactPageNeighborModeV1::None,
      limits,
    ))
    .unwrap_err();
  assert_eq!(root_error.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  assert_eq!(root_error.code(), "selected_artifact_root_manifest_closure");
  drop(reader);
  mismatched.assert_released();

  let missing_page = selected_artifact_fixture_with_options(
    HashAlgorithm::Blake3_256,
    0,
    false,
    false,
    [0; 32],
    all_capabilities_profile(),
    SelectedArtifactLayoutV1::SinglePage,
  );
  let reader = missing_page.reader();
  let missing_page_error = reader
    .load_index_artifact_page_cursor(&selected_artifact_cursor_request(
      &missing_page.catalog,
      &missing_page.scope_id,
      &missing_page.generation,
      OrderedIndexRoleV1::Posting,
      ArtifactPageNeighborModeV1::None,
      limits,
    ))
    .unwrap_err();
  assert_eq!(missing_page_error.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  assert_eq!(missing_page_error.code(), "selected_artifact_cursor_missing");
  drop(reader);
  missing_page.assert_released();

  let corrupt_page = selected_artifact_fixture(HashAlgorithm::Blake3_256);
  let locator = corrupt_page.source.publisher().locator(&corrupt_page.posting_page.key).unwrap().unwrap();
  let mut file = OpenOptions::new().read(true).write(true).open(&corrupt_page.path).unwrap();
  file.seek(SeekFrom::Start(locator.offset + u64::from(locator.total_length) - 1)).unwrap();
  let mut checksum_byte = [0; 1];
  file.read_exact(&mut checksum_byte).unwrap();
  checksum_byte[0] ^= 0xff;
  file.seek(SeekFrom::Start(locator.offset + u64::from(locator.total_length) - 1)).unwrap();
  file.write_all(&checksum_byte).unwrap();
  file.sync_all().unwrap();
  let reader = corrupt_page.reader();
  let corrupt_page_error = reader
    .load_index_artifact_page_cursor(&selected_artifact_cursor_request(
      &corrupt_page.catalog,
      &corrupt_page.scope_id,
      &corrupt_page.generation,
      OrderedIndexRoleV1::Posting,
      ArtifactPageNeighborModeV1::None,
      limits,
    ))
    .unwrap_err();
  assert_eq!(corrupt_page_error.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
  assert_eq!(corrupt_page_error.code(), "selected_artifact_cursor_source");
  drop(reader);
  corrupt_page.assert_released();
}

#[test]
fn selected_artifact_cursor_uses_the_capabilities_admitted_by_its_resolved_view() {
  let fixture = selected_artifact_fixture_with_unadmitted_capability(HashAlgorithm::Blake3_256);
  assert_eq!(fixture.view.supported_reader_capabilities(), CapabilitySetV1::v4_baseline());
  let reader = fixture.reader();

  let error = reader
    .load_index_artifact_page_cursor(&selected_artifact_cursor_request(
      &fixture.catalog,
      &fixture.scope_id,
      &fixture.generation,
      OrderedIndexRoleV1::Posting,
      ArtifactPageNeighborModeV1::None,
      ArtifactPageCursorLimitsV1::new(4, 2 * 1024 * 1024).unwrap(),
    ))
    .unwrap_err();

  assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::Unavailable);
  assert_eq!(error.code(), "selected_artifact_reader_capabilities");
  drop(reader);
  fixture.assert_released();
}

#[test]
fn selected_artifact_cursor_fails_closed_on_cancellation_and_query_memory_pressure() {
  let cancelled = selected_artifact_fixture(HashAlgorithm::Blake3_256);
  cancelled.cancellation.cancel();
  let reader = cancelled.reader();
  let error = reader
    .load_index_artifact_page_cursor(&selected_artifact_cursor_request(
      &cancelled.catalog,
      &cancelled.scope_id,
      &cancelled.generation,
      OrderedIndexRoleV1::Posting,
      ArtifactPageNeighborModeV1::None,
      ArtifactPageCursorLimitsV1::new(4, 1024 * 1024).unwrap(),
    ))
    .unwrap_err();
  assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::Cancelled);
  drop(reader);
  cancelled.assert_released();

  let pressured = selected_artifact_fixture(HashAlgorithm::Blake3_256);
  let snapshot = pressured.memory.snapshot().unwrap();
  let ordinary_limit = 511 * 1024 * 1024;
  let blocker_bytes = ordinary_limit - snapshot.accounted_bytes - 512 * 1024;
  let blocker = pressured.memory.reserve(MemoryOwner::ServerCaches, blocker_bytes, AdmissionClass::Workload).unwrap();
  let reader = pressured.reader();
  let error = reader
    .load_index_artifact_page_cursor(&selected_artifact_cursor_request(
      &pressured.catalog,
      &pressured.scope_id,
      &pressured.generation,
      OrderedIndexRoleV1::Posting,
      ArtifactPageNeighborModeV1::None,
      ArtifactPageCursorLimitsV1::new(4, 1024 * 1024).unwrap(),
    ))
    .unwrap_err();
  assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "selected_artifact_workspace_memory");
  drop(reader);
  drop(blocker);
  let baseline = pressured.memory.snapshot().unwrap();
  let baseline_query = baseline.owner(MemoryOwner::Query).unwrap().reserved_bytes;
  let blocker_bytes = ordinary_limit - baseline.accounted_bytes - 512 * 1024;
  let blocker = pressured.memory.reserve(MemoryOwner::ServerCaches, blocker_bytes, AdmissionClass::Workload).unwrap();
  let reader = pressured.reader();
  let error = reader
    .load_index_artifact_page_cursor(&selected_artifact_cursor_request(
      &pressured.catalog,
      &pressured.scope_id,
      &pressured.generation,
      OrderedIndexRoleV1::Posting,
      ArtifactPageNeighborModeV1::None,
      ArtifactPageCursorLimitsV1::new(4, 256 * 1024).unwrap(),
    ))
    .unwrap_err();
  assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "selected_artifact_read_memory");
  assert_eq!(pressured.memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, baseline_query);
  drop(reader);
  drop(blocker);
  pressured.assert_released();
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
