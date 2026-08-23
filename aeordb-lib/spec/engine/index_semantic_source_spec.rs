use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use aeordb::engine::file_record::FileRecord;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::index_producer_source::{
  IndexSemanticScopeLimitsV1, IndexSemanticScopeReadErrorClassV1, IndexSemanticScopeReadRequestV1, IndexSemanticScopeResolutionV1,
  IndexSemanticScopeSourceV1, ResolvedIndexDocumentTransitionV1, ResolvedIndexDocumentV1,
};
use aeordb::engine::v4::index_semantic_source::{
  CatalogIndexSemanticScopeSourceV1, IndexCompactionSemanticInventoryRequestV1, IndexScopeOrdinalAuthorityV1,
  IndexScopeOrdinalClaimErrorClassV1, IndexScopeOrdinalClaimErrorV1, IndexScopeOrdinalClaimObservationV1, IndexScopeOrdinalClaimPlanV1,
  IndexScopeOrdinalClaimRequestV1, IndexSemanticObjectReadSourceV1, StoredIndexSemanticObjectReadSourceV1, plan_scope_ordinal_claim,
};
use aeordb::engine::v4::field_definition::decode_field_index_definition;
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, decode_semantic_object, encode_semantic_state_object};
use aeordb::engine::v4::scope::decode_scope_definition;
use aeordb::engine::v4::semantic_store::V4SemanticObjectStore;
use aeordb::engine::v4::value_store::decode_value_store_definition;
use aeordb::engine::{HashAlgorithm, StorageEngine};

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;
const SEMANTIC_ROOT: [u8; 32] = [0x51; 32];
const SCOPE_ID: [u8; 32] = [0x63; 32];

fn fixture(name: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/semantic-object-v1/asem-blake3-256-{name}.bin", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn definition_fixture(folder: &str, name: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/{folder}/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn semantic_envelope(kind_id: u16, item_count: u64, body: &[u8]) -> Vec<u8> {
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
  decode_semantic_object(&bytes, ALGORITHM).unwrap();
  bytes
}

fn semantic_definition(class: u16, semantic_id: &[u8], definition: &[u8]) -> Vec<u8> {
  let mut body = vec![0; 16 + 32 + definition.len()];
  body[..2].copy_from_slice(&class.to_le_bytes());
  body[2..4].copy_from_slice(&1u16.to_le_bytes());
  body[8..40].copy_from_slice(semantic_id);
  body[40..44].copy_from_slice(&u32::try_from(definition.len()).unwrap().to_le_bytes());
  body[48..].copy_from_slice(definition);
  semantic_envelope(0x0004, 1, &body)
}

fn scope_definition(owner_path: &str, glob: Option<&str>) -> Vec<u8> {
  let glob = glob.unwrap_or("");
  let mut bytes = vec![0; 64 + owner_path.len() + glob.len()];
  let total_length = u32::try_from(bytes.len()).unwrap();
  bytes[..4].copy_from_slice(b"ASCP");
  bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
  bytes[6..8].copy_from_slice(&32u16.to_le_bytes());
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
  decode_scope_definition(&bytes, ALGORITHM).unwrap();
  bytes
}

#[derive(Clone)]
struct CatalogBinding {
  kind: u16,
  semantic_id: Vec<u8>,
  definition_object_id: Vec<u8>,
  owner_key: Vec<u8>,
  lookup_digest: Vec<u8>,
}

fn catalog_leaf(bindings: &[CatalogBinding]) -> Vec<u8> {
  let mut sorted = bindings.to_vec();
  sorted.sort_by(|left, right| (left.kind, &left.owner_key).cmp(&(right.kind, &right.owner_key)));
  assert!(sorted.iter().all(|binding| binding.lookup_digest == sorted[0].lookup_digest));
  let records_length: usize = sorted.iter().map(|binding| 8 + 64 + binding.owner_key.len()).sum();
  let mut body = vec![0; 48 + records_length];
  body[4..8].copy_from_slice(&u32::try_from(sorted.len()).unwrap().to_le_bytes());
  body[8..40].copy_from_slice(&sorted[0].lookup_digest);
  body[40..44].copy_from_slice(&u32::try_from(records_length).unwrap().to_le_bytes());
  let mut cursor = 48;
  for binding in sorted {
    body[cursor..cursor + 2].copy_from_slice(&binding.kind.to_le_bytes());
    body[cursor + 4..cursor + 8].copy_from_slice(&u32::try_from(binding.owner_key.len()).unwrap().to_le_bytes());
    body[cursor + 8..cursor + 40].copy_from_slice(&binding.semantic_id);
    body[cursor + 40..cursor + 72].copy_from_slice(&binding.definition_object_id);
    body[cursor + 72..cursor + 72 + binding.owner_key.len()].copy_from_slice(&binding.owner_key);
    cursor += 72 + binding.owner_key.len();
  }
  semantic_envelope(0x0002, bindings.len() as u64, &body)
}

fn build_catalog_node(bindings: &[CatalogBinding], depth: usize, objects: &mut Objects, node_count: &mut u64) -> (Vec<u8>, u64) {
  if bindings.iter().all(|binding| binding.lookup_digest == bindings[0].lookup_digest) {
    let bytes = catalog_leaf(bindings);
    let object = decode_semantic_object(&bytes, ALGORITHM).unwrap();
    objects.values.insert((object.kind_id, object.object_id.clone()), bytes);
    *node_count += 1;
    return (object.object_id, bindings.len() as u64);
  }

  let mut prefix_length = 0;
  while depth + prefix_length < 32
    && bindings.iter().all(|binding| binding.lookup_digest[depth + prefix_length] == bindings[0].lookup_digest[depth + prefix_length])
  {
    prefix_length += 1;
  }
  let edge_offset = depth + prefix_length;
  let mut groups: BTreeMap<u8, Vec<CatalogBinding>> = BTreeMap::new();
  for binding in bindings {
    groups.entry(binding.lookup_digest[edge_offset]).or_default().push(binding.clone());
  }
  assert!(groups.len() >= 2);
  let mut children = Vec::new();
  let mut subtree_records = 0u64;
  for (edge, group) in groups {
    let (object_id, record_count) = build_catalog_node(&group, edge_offset + 1, objects, node_count);
    subtree_records += record_count;
    children.push((edge, record_count, object_id));
  }
  let mut body = vec![0; 20 + prefix_length + children.len() * 44];
  body[4..6].copy_from_slice(&u16::try_from(depth).unwrap().to_le_bytes());
  body[6..8].copy_from_slice(&u16::try_from(prefix_length).unwrap().to_le_bytes());
  body[8..10].copy_from_slice(&u16::try_from(children.len()).unwrap().to_le_bytes());
  body[12..20].copy_from_slice(&subtree_records.to_le_bytes());
  body[20..20 + prefix_length].copy_from_slice(&bindings[0].lookup_digest[depth..edge_offset]);
  for (index, (edge, record_count, object_id)) in children.into_iter().enumerate() {
    let cursor = 20 + prefix_length + index * 44;
    body[cursor] = edge;
    body[cursor + 4..cursor + 12].copy_from_slice(&record_count.to_le_bytes());
    body[cursor + 12..cursor + 44].copy_from_slice(&object_id);
  }
  let bytes = semantic_envelope(0x0003, (body.len() - 20 - prefix_length) as u64 / 44, &body);
  let object = decode_semantic_object(&bytes, ALGORITHM).unwrap();
  objects.values.insert((object.kind_id, object.object_id.clone()), bytes);
  *node_count += 1;
  (object.object_id, subtree_records)
}

struct CompleteGraph {
  objects: Objects,
  state_root: Vec<u8>,
  catalog_root: Vec<u8>,
  catalog_record_count: u64,
  catalog_node_count: u64,
  definition_bytes: u64,
  scope_id: Vec<u8>,
  value_store_id: Vec<u8>,
  field_index_id: Vec<u8>,
}

fn complete_graph() -> CompleteGraph {
  let scope = definition_fixture("scope-definition-v1", "ascp-blake3-256-root-direct-valid.bin");
  let scope_id = decode_scope_definition(&scope, ALGORITHM).unwrap().scope_id;
  let mut value_store = definition_fixture("value-store-definition-v1", "avst-blake3-256-metadata-hash-corrected-valid.bin");
  value_store[32..64].copy_from_slice(&scope_id);
  let value_store_id = decode_value_store_definition(&value_store, ALGORITHM).unwrap().value_store_id;
  let mut field_index = definition_fixture("field-index-definition-v1", "afix-blake3-256-typed_exact_blake3_v1-valid.bin");
  field_index[32..64].copy_from_slice(&value_store_id);
  let field_index_id = decode_field_index_definition(&field_index, ALGORITHM).unwrap().index_id;

  let definitions = vec![(3, scope_id.clone(), scope), (4, value_store_id.clone(), value_store), (5, field_index_id.clone(), field_index)];
  build_complete_graph(definitions, scope_id, value_store_id, field_index_id)
}

fn build_complete_graph(
  definitions: Vec<(u16, Vec<u8>, Vec<u8>)>,
  scope_id: Vec<u8>,
  value_store_id: Vec<u8>,
  field_index_id: Vec<u8>,
) -> CompleteGraph {
  let definition_count = definitions.len() as u64;
  let definition_bytes = definitions.iter().map(|(_, _, definition)| definition.len() as u64).sum();
  let mut objects = Objects::default();
  let mut bindings = Vec::new();
  for (kind, semantic_id, definition) in definitions {
    let bytes = semantic_definition(kind, &semantic_id, &definition);
    let object = decode_semantic_object(&bytes, ALGORITHM).unwrap();
    objects.values.insert((object.kind_id, object.object_id.clone()), bytes);
    let kind_bytes = kind.to_le_bytes();
    bindings.push(CatalogBinding {
      kind,
      semantic_id: semantic_id.clone(),
      definition_object_id: object.object_id,
      owner_key: semantic_id.clone(),
      lookup_digest: digest_parts(ALGORITHM, &[b"aeordb.semantic-catalog-key.v1\0", &kind_bytes, &semantic_id]),
    });
  }
  let mut node_count = 0;
  let (catalog_root, record_count) = build_catalog_node(&bindings, 0, &mut objects, &mut node_count);
  let state = encode_semantic_state_object(
    &SemanticStateWriteV1 {
      required_capabilities: [0; 32],
      availability: SemanticAvailabilityV1::Complete {
        compiler_fingerprint: vec![0x11; 32],
        semantic_registry_fingerprint: vec![0x22; 32],
        catalog_root: catalog_root.clone(),
        catalog_record_count: record_count,
        catalog_node_count: node_count,
        definition_count,
        dependency_count: 0,
      },
    },
    ALGORITHM,
  )
  .unwrap();
  let state_root = state.object_id.clone();
  objects = objects.with(state.value);
  CompleteGraph {
    objects,
    state_root,
    catalog_root,
    catalog_record_count: record_count,
    catalog_node_count: node_count,
    definition_bytes,
    scope_id,
    value_store_id,
    field_index_id,
  }
}

fn aggregate_limit_graph() -> (CompleteGraph, Vec<Vec<u8>>, Vec<Vec<u8>>, Vec<Vec<u8>>) {
  let scope_one = scope_definition("/", None);
  let scope_one_id = decode_scope_definition(&scope_one, ALGORITHM).unwrap().scope_id;
  let scope_two = scope_definition("/", Some("*.json"));
  let scope_two_id = decode_scope_definition(&scope_two, ALGORITHM).unwrap().scope_id;

  let mut value_one = definition_fixture("value-store-definition-v1", "avst-blake3-256-metadata-hash-corrected-valid.bin");
  value_one[32..64].copy_from_slice(&scope_one_id);
  let value_one_id = decode_value_store_definition(&value_one, ALGORITHM).unwrap().value_store_id;
  let mut value_two = definition_fixture("value-store-definition-v1", "avst-blake3-256-json-corrected-valid.bin");
  value_two[32..64].copy_from_slice(&scope_one_id);
  let value_two_id = decode_value_store_definition(&value_two, ALGORITHM).unwrap().value_store_id;

  let mut field_one = definition_fixture("field-index-definition-v1", "afix-blake3-256-typed_exact_blake3_v1-valid.bin");
  field_one[32..64].copy_from_slice(&value_one_id);
  let field_one_id = decode_field_index_definition(&field_one, ALGORITHM).unwrap().index_id;
  let mut field_two = definition_fixture("field-index-definition-v1", "afix-blake3-256-bool_order_v1-valid.bin");
  field_two[32..64].copy_from_slice(&value_one_id);
  let field_two_id = decode_field_index_definition(&field_two, ALGORITHM).unwrap().index_id;

  let definitions = vec![
    (3, scope_one_id.clone(), scope_one),
    (3, scope_two_id.clone(), scope_two),
    (4, value_one_id.clone(), value_one),
    (4, value_two_id.clone(), value_two),
    (5, field_one_id.clone(), field_one),
    (5, field_two_id.clone(), field_two),
  ];
  let graph = build_complete_graph(definitions, scope_one_id.clone(), value_one_id.clone(), field_one_id.clone());
  (graph, vec![scope_one_id, scope_two_id], vec![value_one_id, value_two_id], vec![field_one_id, field_two_id])
}

fn replace_state_counts(graph: &mut CompleteGraph, record_count: u64, node_count: u64, definition_count: u64, dependency_count: u64) {
  let state = encode_semantic_state_object(
    &SemanticStateWriteV1 {
      required_capabilities: [0; 32],
      availability: SemanticAvailabilityV1::Complete {
        compiler_fingerprint: vec![0x11; 32],
        semantic_registry_fingerprint: vec![0x22; 32],
        catalog_root: graph.catalog_root.clone(),
        catalog_record_count: record_count,
        catalog_node_count: node_count,
        definition_count,
        dependency_count,
      },
    },
    ALGORITHM,
  )
  .unwrap();
  graph.state_root = state.object_id.clone();
  graph.objects.values.insert((1, state.object_id), state.value);
}

fn memory(bytes: u64) -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(bytes - bytes / 4, bytes, 1, bytes / 8).unwrap())
}

fn transition() -> ResolvedIndexDocumentTransitionV1 {
  transition_at("/doc.json")
}

fn transition_at(path: &str) -> ResolvedIndexDocumentTransitionV1 {
  ResolvedIndexDocumentTransitionV1 {
    before: None,
    after: Some(ResolvedIndexDocumentV1 {
      namespace_root: vec![1; 32],
      revision_hash: vec![2; 32],
      file_record: FileRecord {
        path: path.to_string(),
        content_type: Some("application/json".to_string()),
        total_size: 2,
        created_at: 1,
        updated_at: 2,
        metadata: Vec::new(),
        content_hash: vec![3; 32],
        chunk_hashes: vec![vec![4; 32]],
      },
    }),
  }
}

fn ordinal_request<'request>(
  transition: &'request ResolvedIndexDocumentTransitionV1,
  before_in_scope: bool,
  after_in_scope: bool,
  is_cancelled: &'request dyn Fn() -> bool,
) -> IndexScopeOrdinalClaimRequestV1<'request> {
  IndexScopeOrdinalClaimRequestV1 {
    operation_id: [0x41; 16],
    source_publication_sequence: 7,
    semantic_state_root: &SEMANTIC_ROOT,
    scope_id: &SCOPE_ID,
    transition,
    before_in_scope,
    after_in_scope,
    is_cancelled,
  }
}

fn observation(
  prior_operation_claim: Option<u64>,
  before_live_ordinal: Option<u64>,
  after_live_ordinal: Option<u64>,
  next_document_ordinal: u64,
) -> IndexScopeOrdinalClaimObservationV1 {
  IndexScopeOrdinalClaimObservationV1 { prior_operation_claim, before_live_ordinal, after_live_ordinal, next_document_ordinal }
}

#[test]
fn ordinal_planner_allocates_once_for_a_new_scope_member_and_advances_high_water() {
  let transition = transition();

  let plan = plan_scope_ordinal_claim(ordinal_request(&transition, false, true, &|| false), observation(None, None, None, 17)).unwrap();

  assert_eq!(plan, IndexScopeOrdinalClaimPlanV1::Allocate { document_ordinal: 17, next_document_ordinal: 18 });
}

#[test]
fn ordinal_planner_reuses_the_exact_durable_operation_claim_on_retry() {
  let transition = transition();

  let plan = plan_scope_ordinal_claim(ordinal_request(&transition, false, true, &|| false), observation(Some(23), None, None, 24)).unwrap();

  assert_eq!(plan, IndexScopeOrdinalClaimPlanV1::Reuse { document_ordinal: 23 });
}

#[test]
fn ordinal_planner_preserves_scope_local_identity_for_updates_moves_and_deletes() {
  let transition = transition();
  let cases = [(true, true, Some(31), Some(31)), (true, true, Some(31), None), (true, false, Some(31), None)];

  for (before_in_scope, after_in_scope, before_live, after_live) in cases {
    let plan = plan_scope_ordinal_claim(
      ordinal_request(&transition, before_in_scope, after_in_scope, &|| false),
      observation(None, before_live, after_live, 50),
    )
    .unwrap();
    assert_eq!(plan, IndexScopeOrdinalClaimPlanV1::Reuse { document_ordinal: 31 });
  }
}

#[test]
fn ordinal_planner_treats_cross_scope_move_as_source_reuse_and_destination_allocation() {
  let transition = transition();

  let source =
    plan_scope_ordinal_claim(ordinal_request(&transition, true, false, &|| false), observation(None, Some(7), None, 19)).unwrap();
  let destination =
    plan_scope_ordinal_claim(ordinal_request(&transition, false, true, &|| false), observation(None, None, None, 19)).unwrap();

  assert_eq!(source, IndexScopeOrdinalClaimPlanV1::Reuse { document_ordinal: 7 });
  assert_eq!(destination, IndexScopeOrdinalClaimPlanV1::Allocate { document_ordinal: 19, next_document_ordinal: 20 });
}

#[test]
fn ordinal_planner_never_reuses_a_deleted_identity_for_a_later_recreate() {
  let transition = transition();

  let deleted =
    plan_scope_ordinal_claim(ordinal_request(&transition, true, false, &|| false), observation(None, Some(11), None, 12)).unwrap();
  let recreated =
    plan_scope_ordinal_claim(ordinal_request(&transition, false, true, &|| false), observation(None, None, None, 12)).unwrap();

  assert_eq!(deleted, IndexScopeOrdinalClaimPlanV1::Reuse { document_ordinal: 11 });
  assert_eq!(recreated, IndexScopeOrdinalClaimPlanV1::Allocate { document_ordinal: 12, next_document_ordinal: 13 });
}

#[test]
fn ordinal_planner_reuses_an_existing_after_mapping_during_recovery_or_reindex() {
  let transition = transition();

  let plan = plan_scope_ordinal_claim(ordinal_request(&transition, false, true, &|| false), observation(None, None, Some(37), 38)).unwrap();

  assert_eq!(plan, IndexScopeOrdinalClaimPlanV1::Reuse { document_ordinal: 37 });
}

#[test]
fn ordinal_planner_rejects_invalid_membership_mappings_and_high_water() {
  let transition = transition();
  let cases = [
    (false, false, observation(None, None, None, 1), "scope_ordinal_membership"),
    (false, true, observation(Some(0), None, None, 1), "scope_ordinal_zero"),
    (true, false, observation(None, Some(0), None, 1), "scope_ordinal_zero"),
    (false, true, observation(None, None, Some(0), 1), "scope_ordinal_zero"),
    (false, true, observation(None, None, None, 0), "scope_ordinal_high_water"),
    (true, false, observation(None, None, None, 1), "scope_ordinal_before_missing"),
    (true, true, observation(None, Some(4), Some(5), 6), "scope_ordinal_conflict"),
    (false, true, observation(None, None, None, u64::MAX), "scope_ordinal_exhausted"),
  ];

  for (before_in_scope, after_in_scope, observed, expected_code) in cases {
    let error = plan_scope_ordinal_claim(ordinal_request(&transition, before_in_scope, after_in_scope, &|| false), observed).unwrap_err();
    assert_eq!(error.class(), IndexScopeOrdinalClaimErrorClassV1::Corrupt);
    assert_eq!(error.code(), expected_code);
  }
}

#[test]
fn ordinal_planner_observes_cancellation_before_state_validation() {
  let transition = transition();

  let error = plan_scope_ordinal_claim(ordinal_request(&transition, false, false, &|| true), observation(None, None, None, 0)).unwrap_err();

  assert_eq!(error.class(), IndexScopeOrdinalClaimErrorClassV1::Cancelled);
  assert_eq!(error.code(), "scope_ordinal_cancelled");
}

#[derive(Default)]
struct Objects {
  values: BTreeMap<(u16, Vec<u8>), Vec<u8>>,
  loads: AtomicUsize,
}

impl Objects {
  fn with(mut self, bytes: Vec<u8>) -> Self {
    let object = decode_semantic_object(&bytes, ALGORITHM).unwrap();
    self.values.insert((object.kind_id, object.object_id), bytes);
    self
  }
}

impl IndexSemanticObjectReadSourceV1 for Objects {
  fn load_semantic_object(
    &self,
    kind_id: u16,
    object_id: &[u8],
  ) -> Result<Option<Vec<u8>>, aeordb::engine::v4::index_producer_source::IndexSemanticScopeReadErrorV1> {
    self.loads.fetch_add(1, Ordering::SeqCst);
    Ok(self.values.get(&(kind_id, object_id.to_vec())).cloned())
  }
}

fn task_reserved_bytes(memory: &MemoryCoordinator) -> u64 {
  memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes
}

struct UnexpectedOrdinals;

impl IndexScopeOrdinalAuthorityV1 for UnexpectedOrdinals {
  fn claim_scope_ordinal(&self, _request: IndexScopeOrdinalClaimRequestV1<'_>) -> Result<u64, IndexScopeOrdinalClaimErrorV1> {
    panic!("content-only semantic state must not resolve an index ordinal")
  }
}

struct RecordingOrdinals {
  calls: Mutex<Vec<([u8; 16], Vec<u8>, bool, bool)>>,
  ordinal: u64,
}

struct FailingOrdinals {
  error: IndexScopeOrdinalClaimErrorV1,
}

impl IndexScopeOrdinalAuthorityV1 for FailingOrdinals {
  fn claim_scope_ordinal(&self, _request: IndexScopeOrdinalClaimRequestV1<'_>) -> Result<u64, IndexScopeOrdinalClaimErrorV1> {
    Err(self.error.clone())
  }
}

struct MappedOrdinals {
  ordinals: BTreeMap<Vec<u8>, u64>,
  calls: Mutex<Vec<Vec<u8>>>,
}

impl IndexScopeOrdinalAuthorityV1 for MappedOrdinals {
  fn claim_scope_ordinal(&self, request: IndexScopeOrdinalClaimRequestV1<'_>) -> Result<u64, IndexScopeOrdinalClaimErrorV1> {
    assert_eq!(request.source_publication_sequence, 7);
    self.calls.lock().unwrap().push(request.scope_id.to_vec());
    self
      .ordinals
      .get(request.scope_id)
      .copied()
      .ok_or_else(|| IndexScopeOrdinalClaimErrorV1::corrupt("test_scope_missing", "test ordinal mapping has no requested scope"))
  }
}

impl IndexScopeOrdinalAuthorityV1 for RecordingOrdinals {
  fn claim_scope_ordinal(&self, request: IndexScopeOrdinalClaimRequestV1<'_>) -> Result<u64, IndexScopeOrdinalClaimErrorV1> {
    assert!(!(request.is_cancelled)());
    assert_eq!(request.source_publication_sequence, 7);
    self.calls.lock().unwrap().push((request.operation_id, request.scope_id.to_vec(), request.before_in_scope, request.after_in_scope));
    Ok(self.ordinal)
  }
}

fn limits() -> IndexSemanticScopeLimitsV1 {
  IndexSemanticScopeLimitsV1::new(8, 16, 32, 2 * 1_024 * 1_024).unwrap()
}

#[test]
fn content_only_state_is_resolved_without_catalog_or_ordinal_access() {
  let state_bytes = fixture("state-content-only");
  let state = decode_semantic_object(&state_bytes, ALGORITHM).unwrap();
  let objects = Objects::default().with(state_bytes);
  let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, memory(16 * 1_024 * 1_024), &objects, &UnexpectedOrdinals);
  let transition = transition();

  let read = source
    .resolve_scopes(IndexSemanticScopeReadRequestV1 {
      operation_id: [9; 16],
      source_publication_sequence: 7,
      semantic_state_root: &state.object_id,
      transition: &transition,
      limits: limits(),
      is_cancelled: &|| false,
    })
    .unwrap();

  assert_eq!(read.resolution(), &IndexSemanticScopeResolutionV1::ContentOnly { semantic_state_root: state.object_id });
}

#[test]
fn compaction_inventory_reads_complete_owner_relationships_without_claiming_ordinals() {
  let graph = complete_graph();
  let memory = memory(16 * 1_024 * 1_024);
  let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, memory.clone(), &graph.objects, &UnexpectedOrdinals);

  let inventory = source
    .resolve_compaction_inventory(IndexCompactionSemanticInventoryRequestV1 {
      semantic_state_root: &graph.state_root,
      maintenance_scope: "/docs",
      limits: limits(),
      is_cancelled: &|| false,
    })
    .unwrap();

  assert_eq!(inventory.semantic_state_root(), graph.state_root);
  assert_eq!(inventory.scopes().len(), 1);
  assert_eq!(inventory.scopes()[0].scope_id(), graph.scope_id);
  assert_eq!(inventory.scopes()[0].value_stores().len(), 1);
  assert_eq!(inventory.scopes()[0].value_stores()[0].value_store_id(), graph.value_store_id);
  assert_eq!(inventory.scopes()[0].value_stores()[0].field_index_ids(), &[graph.field_index_id]);
  assert!(task_reserved_bytes(&memory) > 0);
  drop(inventory);
  assert_eq!(task_reserved_bytes(&memory), 0);
}

#[test]
fn compaction_inventory_returns_content_only_as_empty_and_rejects_cancelled_or_bounded_inputs() {
  let state_bytes = fixture("state-content-only");
  let state = decode_semantic_object(&state_bytes, ALGORITHM).unwrap();
  let objects = Objects::default().with(state_bytes);
  let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, memory(16 * 1_024 * 1_024), &objects, &UnexpectedOrdinals);
  let request = IndexCompactionSemanticInventoryRequestV1 {
    semantic_state_root: &state.object_id,
    maintenance_scope: "/",
    limits: limits(),
    is_cancelled: &|| false,
  };
  assert!(source.resolve_compaction_inventory(request).unwrap().scopes().is_empty());

  let cancelled = IndexCompactionSemanticInventoryRequestV1 { is_cancelled: &|| true, ..request };
  let error = source.resolve_compaction_inventory(cancelled).unwrap_err();
  assert_eq!(error.class(), IndexSemanticScopeReadErrorClassV1::Cancelled);

  let graph = complete_graph();
  let complete = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, memory(16 * 1_024 * 1_024), &graph.objects, &UnexpectedOrdinals);
  let bounded = complete
    .resolve_compaction_inventory(IndexCompactionSemanticInventoryRequestV1 {
      semantic_state_root: &graph.state_root,
      maintenance_scope: "/",
      limits: IndexSemanticScopeLimitsV1::new(1, 1, 1, graph.definition_bytes - 1).unwrap(),
      is_cancelled: &|| false,
    })
    .unwrap_err();
  assert_eq!(bounded.code(), "semantic_limit_exceeded");
}

#[test]
fn missing_semantic_state_is_corruption_not_empty_or_content_only_success() {
  let objects = Objects::default();
  let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, memory(16 * 1_024 * 1_024), &objects, &UnexpectedOrdinals);
  let transition = transition();
  let error = source
    .resolve_scopes(IndexSemanticScopeReadRequestV1 {
      operation_id: [9; 16],
      source_publication_sequence: 7,
      semantic_state_root: &[7; 32],
      transition: &transition,
      limits: limits(),
      is_cancelled: &|| false,
    })
    .unwrap_err();

  assert_eq!(error.code(), "semantic_state_missing");
}

#[test]
fn complete_catalog_resolves_exact_definition_closure_and_scope_local_ordinal() {
  let graph = complete_graph();
  let ordinals = RecordingOrdinals { calls: Mutex::new(Vec::new()), ordinal: 41 };
  let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, memory(16 * 1_024 * 1_024), &graph.objects, &ordinals);
  let transition = transition();

  let read = source
    .resolve_scopes(IndexSemanticScopeReadRequestV1 {
      operation_id: [0x44; 16],
      source_publication_sequence: 7,
      semantic_state_root: &graph.state_root,
      transition: &transition,
      limits: limits(),
      is_cancelled: &|| false,
    })
    .unwrap();
  let IndexSemanticScopeResolutionV1::Complete { semantic_state_root, scope_work } = read.resolution() else {
    panic!("complete semantic state resolved as content-only")
  };

  assert_eq!(semantic_state_root, &graph.state_root);
  assert_eq!(scope_work.len(), 1);
  assert_eq!(scope_work[0].document_ordinal, 41);
  assert_eq!(scope_work[0].scope.scope_id, graph.scope_id);
  assert_eq!(scope_work[0].scope.value_stores.len(), 1);
  assert_eq!(scope_work[0].scope.value_stores[0].value_store_id, graph.value_store_id);
  assert_eq!(scope_work[0].scope.value_stores[0].field_indexes.len(), 1);
  assert_eq!(scope_work[0].scope.value_stores[0].field_indexes[0].index_id, graph.field_index_id);
  assert_eq!(ordinals.calls.lock().unwrap().as_slice(), &[([0x44; 16], graph.scope_id, false, true)]);
}

#[test]
fn concrete_scope_resolution_forwards_exact_before_and_after_membership() {
  for (before_path, after_path, expected_membership) in [
    (Some("/nested/before.json"), Some("/after.json"), (false, true)),
    (Some("/before.json"), Some("/nested/after.json"), (true, false)),
    (Some("/before.json"), Some("/after.json"), (true, true)),
  ] {
    let graph = complete_graph();
    let ordinals = RecordingOrdinals { calls: Mutex::new(Vec::new()), ordinal: 41 };
    let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, memory(16 * 1_024 * 1_024), &graph.objects, &ordinals);
    let before = before_path.map(|path| transition_at(path).after.unwrap());
    let after = after_path.map(|path| transition_at(path).after.unwrap());
    let transition = ResolvedIndexDocumentTransitionV1 { before, after };

    source
      .resolve_scopes(IndexSemanticScopeReadRequestV1 {
        operation_id: [0x45; 16],
        source_publication_sequence: 7,
        semantic_state_root: &graph.state_root,
        transition: &transition,
        limits: limits(),
        is_cancelled: &|| false,
      })
      .unwrap();

    let calls = ordinals.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!((calls[0].2, calls[0].3), expected_membership);
  }
}

#[test]
fn out_of_scope_complete_catalog_is_an_explicit_empty_complete_result() {
  let graph = complete_graph();
  let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, memory(16 * 1_024 * 1_024), &graph.objects, &UnexpectedOrdinals);
  let transition = transition_at("/nested/doc.json");

  let read = source
    .resolve_scopes(IndexSemanticScopeReadRequestV1 {
      operation_id: [0x45; 16],
      source_publication_sequence: 7,
      semantic_state_root: &graph.state_root,
      transition: &transition,
      limits: limits(),
      is_cancelled: &|| false,
    })
    .unwrap();

  assert_eq!(
    read.resolution(),
    &IndexSemanticScopeResolutionV1::Complete { semantic_state_root: graph.state_root, scope_work: Vec::new() }
  );
}

#[test]
fn semantic_memory_admission_precedes_the_first_object_read_and_releases_on_refusal() {
  let graph = complete_graph();
  let task_memory = memory(4 * 1_024 * 1_024);
  let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, task_memory.clone(), &graph.objects, &UnexpectedOrdinals);
  let transition = transition();

  let error = source
    .resolve_scopes(IndexSemanticScopeReadRequestV1 {
      operation_id: [0x46; 16],
      source_publication_sequence: 7,
      semantic_state_root: &graph.state_root,
      transition: &transition,
      limits: limits(),
      is_cancelled: &|| false,
    })
    .unwrap_err();

  assert_eq!(error.class(), IndexSemanticScopeReadErrorClassV1::Retryable);
  assert_eq!(error.code(), "semantic_memory_pressure");
  assert_eq!(graph.objects.loads.load(Ordering::SeqCst), 0);
  assert_eq!(task_reserved_bytes(&task_memory), 0);
}

#[test]
fn cancellation_during_catalog_resolution_releases_task_memory() {
  let graph = complete_graph();
  let task_memory = memory(16 * 1_024 * 1_024);
  let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, task_memory.clone(), &graph.objects, &UnexpectedOrdinals);
  let transition = transition();
  let cancelled = || graph.objects.loads.load(Ordering::SeqCst) >= 2;

  let error = source
    .resolve_scopes(IndexSemanticScopeReadRequestV1 {
      operation_id: [0x47; 16],
      source_publication_sequence: 7,
      semantic_state_root: &graph.state_root,
      transition: &transition,
      limits: limits(),
      is_cancelled: &cancelled,
    })
    .unwrap_err();

  assert_eq!(error.class(), IndexSemanticScopeReadErrorClassV1::Cancelled);
  assert_eq!(error.code(), "semantic_cancelled");
  assert!(graph.objects.loads.load(Ordering::SeqCst) >= 2);
  assert_eq!(task_reserved_bytes(&task_memory), 0);
}

#[test]
fn successful_semantic_read_retains_and_then_releases_its_task_reservation() {
  let graph = complete_graph();
  let task_memory = memory(16 * 1_024 * 1_024);
  let ordinals = RecordingOrdinals { calls: Mutex::new(Vec::new()), ordinal: 5 };
  let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, task_memory.clone(), &graph.objects, &ordinals);
  let transition = transition();

  let read = source
    .resolve_scopes(IndexSemanticScopeReadRequestV1 {
      operation_id: [0x48; 16],
      source_publication_sequence: 7,
      semantic_state_root: &graph.state_root,
      transition: &transition,
      limits: limits(),
      is_cancelled: &|| false,
    })
    .unwrap();

  assert!(task_reserved_bytes(&task_memory) > 0);
  drop(read);
  assert_eq!(task_reserved_bytes(&task_memory), 0);
}

#[test]
fn semantic_state_catalog_count_mismatches_fail_closed() {
  for case in 0..5 {
    let mut graph = complete_graph();
    let mut record_count = graph.catalog_record_count;
    let mut node_count = graph.catalog_node_count;
    let mut definition_count = 3;
    let mut dependency_count = 0;
    match case {
      0 => record_count += 1,
      1 => node_count += 1,
      2 => definition_count -= 1,
      3 => dependency_count += 1,
      4 => node_count = 1,
      _ => unreachable!(),
    }
    replace_state_counts(&mut graph, record_count, node_count, definition_count, dependency_count);
    let task_memory = memory(16 * 1_024 * 1_024);
    let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, task_memory.clone(), &graph.objects, &UnexpectedOrdinals);
    let transition = transition();

    let error = source
      .resolve_scopes(IndexSemanticScopeReadRequestV1 {
        operation_id: [0x49; 16],
        source_publication_sequence: 7,
        semantic_state_root: &graph.state_root,
        transition: &transition,
        limits: limits(),
        is_cancelled: &|| false,
      })
      .unwrap_err();

    assert!(matches!(error.code(), "semantic_catalog_counts" | "semantic_state_counts"));
    assert_eq!(error.class(), IndexSemanticScopeReadErrorClassV1::Corrupt);
    if case == 4 {
      assert_eq!(graph.objects.loads.load(Ordering::SeqCst), 3, "catalog traversal did not stop at the selected root's exact node bound");
    }
    assert_eq!(task_reserved_bytes(&task_memory), 0);
  }
}

#[test]
fn missing_and_ambiguous_catalog_roots_are_corruption() {
  for expected_code in ["semantic_catalog_missing", "semantic_catalog_ambiguous"] {
    let mut graph = complete_graph();
    let original_kind = if graph.objects.values.contains_key(&(2, graph.catalog_root.clone())) { 2 } else { 3 };
    let other_kind = if original_kind == 2 { 3 } else { 2 };
    let bytes = graph.objects.values.get(&(original_kind, graph.catalog_root.clone())).unwrap().clone();
    if expected_code == "semantic_catalog_missing" {
      graph.objects.values.remove(&(original_kind, graph.catalog_root.clone()));
    } else {
      graph.objects.values.insert((other_kind, graph.catalog_root.clone()), bytes);
    }
    let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, memory(16 * 1_024 * 1_024), &graph.objects, &UnexpectedOrdinals);
    let transition = transition();

    let error = source
      .resolve_scopes(IndexSemanticScopeReadRequestV1 {
        operation_id: [0x4a; 16],
        source_publication_sequence: 7,
        semantic_state_root: &graph.state_root,
        transition: &transition,
        limits: limits(),
        is_cancelled: &|| false,
      })
      .unwrap_err();

    assert_eq!(error.code(), expected_code);
  }
}

#[test]
fn missing_or_substituted_definition_objects_fail_closed() {
  for expected_code in ["semantic_definition_missing", "semantic_definition_closure"] {
    let mut graph = complete_graph();
    let definition_keys = graph.objects.values.keys().filter(|(kind, _)| *kind == 4).cloned().collect::<Vec<_>>();
    assert!(definition_keys.len() >= 2);
    if expected_code == "semantic_definition_missing" {
      for key in definition_keys {
        graph.objects.values.remove(&key);
      }
    } else {
      let substitute = graph.objects.values.get(&definition_keys[1]).unwrap().clone();
      for key in definition_keys {
        graph.objects.values.insert(key, substitute.clone());
      }
    }
    let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, memory(16 * 1_024 * 1_024), &graph.objects, &UnexpectedOrdinals);
    let transition = transition();

    let error = source
      .resolve_scopes(IndexSemanticScopeReadRequestV1 {
        operation_id: [0x4b; 16],
        source_publication_sequence: 7,
        semantic_state_root: &graph.state_root,
        transition: &transition,
        limits: limits(),
        is_cancelled: &|| false,
      })
      .unwrap_err();

    assert_eq!(error.code(), expected_code);
  }
}

#[test]
fn definition_byte_limit_accepts_exact_fit_and_rejects_one_byte_less() {
  for (limit, succeeds) in [(None, true), (Some(1), false)] {
    let graph = complete_graph();
    let max_definition_bytes = graph.definition_bytes - limit.unwrap_or(0);
    let configured_limits = IndexSemanticScopeLimitsV1::new(1, 1, 1, max_definition_bytes).unwrap();
    let ordinals = RecordingOrdinals { calls: Mutex::new(Vec::new()), ordinal: 13 };
    let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, memory(16 * 1_024 * 1_024), &graph.objects, &ordinals);
    let transition = transition();
    let result = source.resolve_scopes(IndexSemanticScopeReadRequestV1 {
      operation_id: [0x4c; 16],
      source_publication_sequence: 7,
      semantic_state_root: &graph.state_root,
      transition: &transition,
      limits: configured_limits,
      is_cancelled: &|| false,
    });

    if succeeds {
      assert!(result.is_ok());
    } else {
      let error = result.unwrap_err();
      assert_eq!(error.code(), "semantic_limit_exceeded");
    }
  }
}

#[test]
fn overlapping_scopes_resolve_independent_ordinals_and_exact_aggregate_limits() {
  let (graph, scope_ids, value_store_ids, field_index_ids) = aggregate_limit_graph();
  let expected_ordinals = BTreeMap::from([(scope_ids[0].clone(), 81), (scope_ids[1].clone(), 82)]);
  let ordinals = MappedOrdinals { ordinals: expected_ordinals.clone(), calls: Mutex::new(Vec::new()) };
  let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, memory(16 * 1_024 * 1_024), &graph.objects, &ordinals);
  let transition = transition();

  let read = source
    .resolve_scopes(IndexSemanticScopeReadRequestV1 {
      operation_id: [0x4c; 16],
      source_publication_sequence: 7,
      semantic_state_root: &graph.state_root,
      transition: &transition,
      limits: IndexSemanticScopeLimitsV1::new(2, 2, 2, graph.definition_bytes).unwrap(),
      is_cancelled: &|| false,
    })
    .unwrap();
  let IndexSemanticScopeResolutionV1::Complete { scope_work, .. } = read.resolution() else {
    panic!("aggregate graph resolved as content-only")
  };

  assert_eq!(scope_work.len(), 2);
  for work in scope_work {
    assert_eq!(Some(&work.document_ordinal), expected_ordinals.get(&work.scope.scope_id));
  }
  let total_value_stores = scope_work.iter().map(|work| work.scope.value_stores.len()).sum::<usize>();
  let total_field_indexes =
    scope_work.iter().flat_map(|work| &work.scope.value_stores).map(|value_store| value_store.field_indexes.len()).sum::<usize>();
  assert_eq!(total_value_stores, value_store_ids.len());
  assert_eq!(total_field_indexes, field_index_ids.len());
  let mut calls = ordinals.calls.lock().unwrap().clone();
  calls.sort();
  let mut expected_calls = scope_ids;
  expected_calls.sort();
  assert_eq!(calls, expected_calls);
}

#[test]
fn concrete_catalog_enforces_scope_value_store_and_field_index_limits_independently() {
  for (max_scopes, max_value_stores, max_field_indexes, expected_resource) in
    [(1, 2, 2, "scopes"), (2, 1, 2, "value stores"), (2, 2, 1, "field indexes")]
  {
    let (graph, scope_ids, _, _) = aggregate_limit_graph();
    let ordinals =
      MappedOrdinals { ordinals: BTreeMap::from([(scope_ids[0].clone(), 81), (scope_ids[1].clone(), 82)]), calls: Mutex::new(Vec::new()) };
    let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, memory(16 * 1_024 * 1_024), &graph.objects, &ordinals);
    let transition = transition();

    let error = source
      .resolve_scopes(IndexSemanticScopeReadRequestV1 {
        operation_id: [0x4d; 16],
        source_publication_sequence: 7,
        semantic_state_root: &graph.state_root,
        transition: &transition,
        limits: IndexSemanticScopeLimitsV1::new(max_scopes, max_value_stores, max_field_indexes, graph.definition_bytes).unwrap(),
        is_cancelled: &|| false,
      })
      .unwrap_err();

    assert_eq!(error.code(), "semantic_limit_exceeded");
    assert!(error.context().contains(expected_resource), "unexpected limit error: {error}");
    assert!(ordinals.calls.lock().unwrap().is_empty(), "ordinal authority must not observe a partially validated scope set");
  }
}

#[test]
fn ordinal_failures_preserve_their_typed_error_class_and_zero_is_corruption() {
  let cases = [
    (IndexScopeOrdinalClaimErrorV1::cancelled("ordinal_cancelled", "cancelled"), IndexSemanticScopeReadErrorClassV1::Cancelled),
    (IndexScopeOrdinalClaimErrorV1::retryable("ordinal_retryable", "retryable"), IndexSemanticScopeReadErrorClassV1::Retryable),
    (IndexScopeOrdinalClaimErrorV1::corrupt("ordinal_corrupt", "corrupt"), IndexSemanticScopeReadErrorClassV1::Corrupt),
  ];

  for (ordinal_error, expected_class) in cases {
    let graph = complete_graph();
    let expected_code = ordinal_error.code();
    let ordinals = FailingOrdinals { error: ordinal_error };
    let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, memory(16 * 1_024 * 1_024), &graph.objects, &ordinals);
    let transition = transition();

    let error = source
      .resolve_scopes(IndexSemanticScopeReadRequestV1 {
        operation_id: [0x4d; 16],
        source_publication_sequence: 7,
        semantic_state_root: &graph.state_root,
        transition: &transition,
        limits: limits(),
        is_cancelled: &|| false,
      })
      .unwrap_err();

    assert_eq!(error.class(), expected_class);
    assert_eq!(error.code(), expected_code);
  }

  let graph = complete_graph();
  let ordinals = RecordingOrdinals { calls: Mutex::new(Vec::new()), ordinal: 0 };
  let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, memory(16 * 1_024 * 1_024), &graph.objects, &ordinals);
  let transition = transition();
  let error = source
    .resolve_scopes(IndexSemanticScopeReadRequestV1 {
      operation_id: [0x4e; 16],
      source_publication_sequence: 7,
      semantic_state_root: &graph.state_root,
      transition: &transition,
      limits: limits(),
      is_cancelled: &|| false,
    })
    .unwrap_err();
  assert_eq!(error.class(), IndexSemanticScopeReadErrorClassV1::Corrupt);
  assert_eq!(error.code(), "scope_ordinal_zero");
}

#[test]
fn file_backed_semantic_store_resolves_the_complete_catalog_through_the_production_adapter() {
  let graph = complete_graph();
  let temporary = tempfile::tempdir().unwrap();
  let database_path = temporary.path().join("semantic-source.aeordb");
  let engine = StorageEngine::create(database_path.to_str().unwrap()).unwrap();
  let store = V4SemanticObjectStore::new(&engine);
  for ((_, object_id), bytes) in &graph.objects.values {
    store.publish(object_id, bytes).unwrap();
  }
  let objects = StoredIndexSemanticObjectReadSourceV1::new(&engine);
  let ordinals = RecordingOrdinals { calls: Mutex::new(Vec::new()), ordinal: 73 };
  let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, engine.memory_coordinator().as_ref().clone(), &objects, &ordinals);
  let transition = transition();

  let read = source
    .resolve_scopes(IndexSemanticScopeReadRequestV1 {
      operation_id: [0x4f; 16],
      source_publication_sequence: 7,
      semantic_state_root: &graph.state_root,
      transition: &transition,
      limits: limits(),
      is_cancelled: &|| false,
    })
    .unwrap();
  let IndexSemanticScopeResolutionV1::Complete { scope_work, .. } = read.resolution() else {
    panic!("stored complete semantic state resolved as content-only")
  };
  assert_eq!(scope_work.len(), 1);
  assert_eq!(scope_work[0].document_ordinal, 73);
  assert_eq!(scope_work[0].scope.scope_id, graph.scope_id);
  drop(read);
  drop(source);
  drop(objects);
  engine.shutdown().unwrap();
}

#[test]
fn production_semantic_store_adapter_classifies_invalid_identity_and_shutdown_without_squelching() {
  let temporary = tempfile::tempdir().unwrap();
  let database_path = temporary.path().join("semantic-source-errors.aeordb");
  let engine = StorageEngine::create(database_path.to_str().unwrap()).unwrap();
  let objects = StoredIndexSemanticObjectReadSourceV1::new(&engine);

  let corrupt = objects.load_semantic_object(1, &[1]).unwrap_err();
  assert_eq!(corrupt.class(), IndexSemanticScopeReadErrorClassV1::Corrupt);
  assert_eq!(corrupt.code(), "semantic_store_corrupt");

  engine.shutdown().unwrap();
  let retryable = objects.load_semantic_object(1, &[1; 32]).unwrap_err();
  assert_eq!(retryable.class(), IndexSemanticScopeReadErrorClassV1::Retryable);
  assert_eq!(retryable.code(), "semantic_store_retryable");
}
