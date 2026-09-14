//! Exercise the actual retained-catalog walkers under selector decode pressure.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{ALGORITHM, fixture, json_selector_definition, measure};
use aeordb::engine::file_record::FileRecord;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::dependency::encode_dependency_record;
use aeordb::engine::v4::field_definition::decode_field_index_definition;
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_producer_source::*;
use aeordb::engine::v4::index_semantic_source::*;
use aeordb::engine::v4::namespace::*;
use aeordb::engine::v4::scope::decode_scope_definition;
use aeordb::engine::v4::source_selector::JsonPathSegmentV1;
use aeordb::engine::v4::value_store::decode_value_store_definition;

#[derive(Default)]
pub(super) struct Objects(pub(super) BTreeMap<(u16, Vec<u8>), Vec<u8>>);

impl Objects {
  fn insert(&mut self, kind: u16, object: EncodedSemanticObjectV1) -> Vec<u8> {
    self.0.insert((kind, object.object_id.clone()), object.value);
    object.object_id
  }
}

impl IndexSemanticObjectReadSourceV1 for Objects {
  fn load_semantic_object(&self, kind: u16, identity: &[u8]) -> Result<Option<Vec<u8>>, IndexSemanticScopeReadErrorV1> {
    Ok(self.0.get(&(kind, identity.to_vec())).cloned())
  }
}

#[derive(Default)]
pub(super) struct Ordinals(AtomicUsize);

impl IndexScopeOrdinalAuthorityV1 for Ordinals {
  fn claim_scope_ordinal(&self, _: IndexScopeOrdinalClaimRequestV1<'_>) -> Result<u64, IndexScopeOrdinalClaimErrorV1> {
    self.0.fetch_add(1, Ordering::SeqCst);
    Ok(1)
  }
}

struct Binding {
  class: u16,
  semantic_id: Vec<u8>,
  definition_id: Vec<u8>,
  lookup: Vec<u8>,
}

fn catalog(bindings: &[&Binding], depth: usize, objects: &mut Objects, nodes: &mut u64) -> Vec<u8> {
  *nodes += 1;
  if bindings.len() == 1 {
    let binding = bindings[0];
    return objects.insert(
      2,
      encode_semantic_catalog_leaf(
        &[SemanticCatalogRecordV1 {
          record_kind: binding.class,
          owner_key: &binding.semantic_id,
          semantic_id: &binding.semantic_id,
          definition_object_id: &binding.definition_id,
        }],
        ALGORITHM,
      )
      .unwrap(),
    );
  }
  let first = bindings[0];
  let mut split = depth;
  while bindings.iter().all(|binding| binding.lookup[split] == first.lookup[split]) {
    split += 1;
    assert!(split < 32, "fixed test definitions unexpectedly share a complete lookup digest");
  }
  let mut groups: BTreeMap<u8, Vec<&Binding>> = BTreeMap::new();
  for binding in bindings {
    groups.entry(binding.lookup[split]).or_default().push(binding);
  }
  let children: Vec<_> =
    groups.into_iter().map(|(edge, group)| (edge, group.len() as u64, catalog(&group, split + 1, objects, nodes))).collect();
  objects.insert(
    3,
    encode_semantic_catalog_internal(
      depth as u16,
      &first.lookup[depth..split],
      &children
        .iter()
        .map(|(edge, count, identity)| SemanticCatalogChildV1 { edge: *edge, record_count: *count, object_id: identity })
        .collect::<Vec<_>>(),
      ALGORITHM,
    )
    .unwrap(),
  )
}

fn graph() -> (Objects, Vec<u8>) {
  let scope = fixture("scope-definition-v1", "ascp-blake3-256-root-direct-valid");
  let scope_id = decode_scope_definition(&scope, ALGORITHM).unwrap().scope_id;
  // Nineteen segments avoid the fixture's unrelated 408-byte catalog clone.
  let segments: [JsonPathSegmentV1<'_>; 19] = std::array::from_fn(|_| JsonPathSegmentV1::ObjectKey("x"));
  let mut value = json_selector_definition(false, &segments);
  value[32..64].copy_from_slice(&scope_id);
  let value_definition = decode_value_store_definition(&value, ALGORITHM).unwrap();
  let mut field = fixture("field-index-definition-v1", "afix-blake3-256-typed_exact_blake3_v1-valid");
  field[32..64].copy_from_slice(&value_definition.value_store_id);
  graph_from_definitions(scope, value, field)
}

pub(super) fn graph_from_definitions(scope: Vec<u8>, value: Vec<u8>, field: Vec<u8>) -> (Objects, Vec<u8>) {
  let value_definition = decode_value_store_definition(&value, ALGORITHM).unwrap();
  let mut definitions: Vec<_> = value_definition
    .dependencies
    .records
    .iter()
    .map(|record| (if record.kind == 1 { 6 } else { 7 }, encode_dependency_record(record).unwrap()))
    .collect();
  let dependency_count = definitions.len() as u64;
  decode_field_index_definition(&field, ALGORITHM).unwrap();
  definitions.extend([(3, scope), (4, value), (5, field)]);
  let mut objects = Objects::default();
  let bindings: Vec<_> = definitions
    .into_iter()
    .map(|(class, definition)| {
      let encoded = encode_semantic_definition_object(class, &definition, ALGORITHM).unwrap();
      let definition_id = objects.insert(4, encoded.object);
      Binding {
        class,
        lookup: digest_parts(ALGORITHM, &[b"aeordb.semantic-catalog-key.v1\0", &class.to_le_bytes(), &encoded.semantic_id]),
        semantic_id: encoded.semantic_id,
        definition_id,
      }
    })
    .collect();
  let mut nodes = 0;
  let root = catalog(&bindings.iter().collect::<Vec<_>>(), 0, &mut objects, &mut nodes);
  let state = encode_semantic_state_object(
    &SemanticStateWriteV1 {
      required_capabilities: [0; 32],
      availability: SemanticAvailabilityV1::Complete {
        compiler_fingerprint: vec![1; 32],
        semantic_registry_fingerprint: vec![2; 32],
        catalog_root: root,
        catalog_record_count: bindings.len() as u64,
        catalog_node_count: nodes,
        definition_count: bindings.len() as u64,
        dependency_count,
      },
    },
    ALGORITHM,
  )
  .unwrap();
  let state_id = objects.insert(1, state);
  (objects, state_id)
}

#[test]
fn semantic_scope_read_allocation_failure_retries_without_claiming_an_ordinal() {
  let (objects, state) = graph();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(12 << 20, 16 << 20, 1, 2 << 20).unwrap());
  let ordinals = Ordinals::default();
  let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, memory.clone(), &objects, &ordinals);
  let transition = ResolvedIndexDocumentTransitionV1 {
    before: None,
    after: Some(ResolvedIndexDocumentV1 {
      namespace_root: vec![1; 32],
      revision_hash: vec![2; 32],
      file_record: FileRecord::new("/doc.json".into(), None, 0, Vec::new()),
    }),
  };
  let request = IndexSemanticScopeReadRequestV1 {
    operation_id: [3; 16],
    source_publication_sequence: 1,
    semantic_state_root: &state,
    transition: &transition,
    limits: IndexSemanticScopeLimitsV1::new(8, 16, 32, 2 << 20).unwrap(),
    is_cancelled: &|| false,
  };
  drop(source.resolve_scopes(request).unwrap());
  ordinals.0.store(0, Ordering::SeqCst);
  let (result, allocations) = measure(19 * std::mem::size_of::<JsonPathSegmentV1<'_>>(), || source.resolve_scopes(request));
  assert!(allocations.injected_failure, "{allocations:?}");
  let error = result.unwrap_err();
  assert_eq!(error.code(), "selector_decode_allocation");
  assert_eq!(error.class(), IndexSemanticScopeReadErrorClassV1::Retryable);
  assert_eq!(ordinals.0.load(Ordering::SeqCst), 0);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  drop(source.resolve_scopes(request).unwrap());
  assert_eq!(ordinals.0.load(Ordering::SeqCst), 1);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn semantic_compaction_inventory_allocation_failure_is_retryable_and_releases_memory() {
  let (objects, state) = graph();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(12 << 20, 16 << 20, 1, 2 << 20).unwrap());
  let ordinals = Ordinals::default();
  let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, memory.clone(), &objects, &ordinals);
  let request = IndexCompactionSemanticInventoryRequestV1 {
    semantic_state_root: &state,
    maintenance_scope: "/",
    limits: IndexSemanticScopeLimitsV1::new(8, 16, 32, 2 << 20).unwrap(),
    is_cancelled: &|| false,
  };
  drop(source.resolve_compaction_inventory(request).unwrap());
  let (result, allocations) = measure(19 * std::mem::size_of::<JsonPathSegmentV1<'_>>(), || source.resolve_compaction_inventory(request));
  assert!(allocations.injected_failure, "{allocations:?}");
  let error = result.unwrap_err();
  assert_eq!(error.code(), "selector_decode_allocation");
  assert_eq!(error.class(), IndexSemanticScopeReadErrorClassV1::Retryable);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  drop(source.resolve_compaction_inventory(request).unwrap());
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  assert_eq!(ordinals.0.load(Ordering::SeqCst), 0);
}
