//! Real native consumers retain their authority and leases on decode pressure.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::retained_active_pointer_resource_spec::authority_fixture::{DATABASE_ID, create_publisher_for, retirement_owner_for};
use super::retained_definition_read_resource_spec::manifest_chain_with_selector_segments;
use super::retained_semantic_catalog_resource_spec::{Ordinals, graph_from_definitions};
use super::{ALGORITHM, measure_nth};
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::permission_resolver::CrudlifyOp;
use aeordb::engine::v4::admission::BinaryCapabilityProfileV1;
use aeordb::engine::v4::first_authority::*;
use aeordb::engine::v4::gc_retirement::RetirementJournalOwnerV1;
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_artifact::*;
use aeordb::engine::v4::index_compaction_runtime::*;
use aeordb::engine::v4::index_coverage_planner::IndexCoverageGenerationHealthV1;
use aeordb::engine::v4::index_coverage_registry::*;
use aeordb::engine::v4::index_native_compaction::{NativeIndexCompactionExecutorV1, NativeIndexCompactionOptionsV1};
use aeordb::engine::v4::index_native_semantic_source::FirstAuthorityIndexSemanticObjectReadSourceV1;
use aeordb::engine::v4::index_page::OrderedIndexRoleV1;
use aeordb::engine::v4::index_producer_source::IndexSemanticScopeLimitsV1;
use aeordb::engine::v4::index_semantic_source::CatalogIndexSemanticScopeSourceV1;
use aeordb::engine::v4::namespace::EncodedSemanticObjectV1;
use aeordb::engine::v4::read_view::*;
use aeordb::engine::v4::read_view_authorization::*;
use aeordb::engine::v4::read_view_native::*;
use aeordb::engine::v4::source_selector::JsonPathSegmentV1;
use aeordb::engine::v4::value_store::decode_value_store_definition;
use tokio_util::sync::CancellationToken;

const SELECTOR_ALLOCATION: usize = 19 * std::mem::size_of::<JsonPathSegmentV1<'static>>();

struct NativeFixture {
  _directory: tempfile::TempDir,
  path: PathBuf,
  publisher: Arc<V4FirstAuthorityPublisher>,
  memory: Arc<MemoryCoordinator>,
  retirement: Arc<Mutex<RetirementJournalOwnerV1>>,
  field_id: Vec<u8>,
  field_name: String,
}

impl NativeFixture {
  fn new() -> Self {
    let chain = manifest_chain_with_selector_segments(19)
      .map(|value| EncodedImmutableIndexArtifactV1 { key: decode_index_manifest(&value, ALGORITHM).unwrap().key, value });
    let scope = decode_index_manifest(&chain[0].value, ALGORITHM).unwrap();
    let value = decode_index_manifest(&chain[1].value, ALGORITHM).unwrap();
    let field = decode_index_manifest(&chain[2].value, ALGORITHM).unwrap();
    let IndexManifestBodyV1::ScopeCatalog(scope_body) = scope.details else {
      panic!("scope")
    };
    let IndexManifestBodyV1::ValueStore(value_body) = value.details else {
      panic!("value")
    };
    let IndexManifestBodyV1::FieldIndex(field_body) = field.details else {
      panic!("field")
    };
    let field_name = decode_value_store_definition(value_body.value_store_definition, ALGORITHM).unwrap().field_name.to_owned();
    let (objects, state_id) = graph_from_definitions(
      scope_body.scope_definition.to_vec(),
      value_body.value_store_definition.to_vec(),
      field_body.field_index_definition.to_vec(),
    );
    let state = EncodedSemanticObjectV1 { object_id: state_id.clone(), value: objects.0[&(1, state_id)].clone() };
    let objects: Vec<_> = objects.0.into_iter().map(|((_, object_id), value)| EncodedSemanticObjectV1 { object_id, value }).collect();
    let (directory, path, publisher) = create_publisher_for(ALGORITHM);
    let authority = publisher.load_selected_semantic_authority().unwrap();
    publisher
      .publish_immutable_semantic_objects(ImmutableSemanticObjectBatchPublicationRequestV1 {
        database_id: &DATABASE_ID,
        objects: &objects,
        publication_timestamp_ms: 1_700_000_000_200,
      })
      .unwrap();
    publisher
      .publish_successor_authority(&SuccessorAuthorityPublicationRequestV1 {
        database_id: DATABASE_ID,
        transaction_id: [0x65; 16],
        created_at_ms: 1_700_000_000_250,
        expected_head_hash: authority.root_hash,
        namespace_tree: PreparedNamespaceTreeV0 { root_hash: authority.namespace_tree_root, stored_value: Vec::new() },
        semantic_state: state,
        required_capabilities: [0; 32],
        typed_closure_digest: digest_parts(ALGORITHM, &[b"retained native resource fixture"]),
        authority_identity: b"HEAD".to_vec(),
      })
      .unwrap();
    publisher
      .publish_index_artifacts(IndexArtifactBatchPublicationRequestV1 {
        database_id: &DATABASE_ID,
        artifacts: &[&chain[0], &chain[1], &chain[2]],
        publication_timestamp_ms: 1_700_000_000_300,
      })
      .unwrap();
    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 1, 64 << 20).unwrap()));
    let cancellation = CancellationToken::new();
    let mut retirement = retirement_owner_for(ALGORITHM, &cancellation, &memory);
    let pointer = encode_active_pointer(&ActivePointerWriteV1 {
      kind: ActivePointerKindV1::FieldIndex,
      hash_algorithm: ALGORITHM,
      generation: field.generation,
      owner_id: field.owner_id,
      slot: 0,
      sequence: 1,
      target_manifest_hash: &chain[2].key,
    })
    .unwrap();
    publisher
      .publish_index_active_pointer(
        IndexActivePointerPublicationRequestV1 {
          database_id: &DATABASE_ID,
          pointer: &pointer,
          publication_timestamp_ms: 1_700_000_000_350,
          monotonic_now_ms: 1_700_000_000_350,
        },
        &mut retirement,
      )
      .unwrap();
    Self {
      _directory: directory,
      path,
      publisher: Arc::new(publisher),
      memory,
      retirement: Arc::new(Mutex::new(retirement)),
      field_id: field.owner_id.to_vec(),
      field_name,
    }
  }

  fn registry(&self) -> IndexCoverageRegistryV1 {
    IndexCoverageRegistryV1::new(
      ALGORITHM,
      DATABASE_ID,
      BinaryCapabilityProfileV1::current().supported_reader_capabilities,
      IndexCoverageRegistryOptionsV1::new(8, 2 << 20).unwrap(),
      Arc::clone(&self.memory),
    )
    .unwrap()
  }

  fn requests(&self) -> [IndexCoverageRegistryOwnerRequestV1; 1] {
    [IndexCoverageRegistryOwnerRequestV1::new(
      IndexCoverageRegistryOwnerKindV1::FieldIndex,
      self.field_id.clone(),
      IndexCoverageGenerationHealthV1::Healthy,
    )
    .unwrap()]
  }
}

#[test]
fn native_coverage_refresh_preserves_snapshot_on_each_definition_allocation_failure() {
  let fixture = NativeFixture::new();
  let registry = fixture.registry();
  let requests = fixture.requests();
  let cancellation = CancellationToken::new();
  let mut source = FirstAuthorityIndexCoverageRegistrySourceV1::new(Arc::clone(&fixture.publisher)).unwrap();
  let (warm, measured) = measure_nth(SELECTOR_ALLOCATION, usize::MAX, || registry.refresh(&mut source, &requests, &cancellation));
  let selected = warm.unwrap();
  assert!(measured.matching_requests >= 4, "{measured:?}");
  let reserved = fixture.memory.snapshot().unwrap().reserved_bytes;
  let before = fixture.publisher.observe().unwrap();
  for occurrence in 1..=measured.matching_requests {
    let (result, allocation) = measure_nth(SELECTOR_ALLOCATION, occurrence, || registry.refresh(&mut source, &requests, &cancellation));
    assert!(allocation.injected_failure, "{allocation:?}");
    let error = result.unwrap_err();
    assert!(error.to_string().contains("selector_decode_allocation"), "{error}");
    assert!(
      matches!(
        error,
        IndexCoverageRegistryErrorV1::Allocation(_)
          | IndexCoverageRegistryErrorV1::Source(IndexCoverageRegistrySourceErrorV1::Unavailable { .. })
      ),
      "{error}"
    );
    assert!(Arc::ptr_eq(&selected, &registry.snapshot().unwrap()));
    assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, reserved);
    assert_eq!(fixture.publisher.observe().unwrap(), before);
  }
  drop(selected);
  drop(registry.refresh(&mut source, &requests, &cancellation).unwrap());
  assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, reserved);
}

fn with_reader(action: impl FnOnce(&NativeFixture, &NativeSelectedNamespaceReaderV1<'_>)) {
  let fixture = NativeFixture::new();
  let source = Arc::new(NativeReadViewSourceV1::new(Arc::clone(&fixture.publisher), Arc::clone(&fixture.memory), 86_400_000));
  let pins = RootReadPinCoordinatorV1::new(Arc::clone(&fixture.memory), ALGORITHM, 8, 16).unwrap();
  let current = CurrentReadAuthorizationV1::new(
    CurrentPathAuthorizationV1::for_root("/", CrudlifyOp::List),
    ReadViewCredentialKindV1::Ordinary,
    ReadViewConcealmentV1::Conceal,
  );
  let authorizer = ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), source.as_ref().clone());
  let resolver = ReadViewResolverV1::new(source.clone(), pins.clone(), BinaryCapabilityProfileV1::current());
  let before = fixture.memory.snapshot().unwrap().reserved_bytes;
  let view = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap();
  let reader = source
    .selected_namespace_reader(&view, NativeSelectedNamespaceLimitsV1::new(16, 1 << 20, u16::MAX as usize, 32, 100_000, 10_000).unwrap())
    .unwrap();
  action(&fixture, &reader);
  drop(reader);
  drop(view);
  assert_eq!(pins.active_pin_count().unwrap(), 0);
  assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, before);
}

#[test]
fn native_selected_catalog_preserves_real_definition_allocation_failure() {
  with_reader(|fixture, reader| {
    let fields = [fixture.field_name.as_str()];
    let load = || reader.load_planner_catalogs("/", &fields, default_native_selected_semantic_limits_v1());
    let (warm, measured) = measure_nth(SELECTOR_ALLOCATION, usize::MAX, load);
    drop(warm.unwrap());
    assert!(measured.matching_requests > 0, "{measured:?}");
    let reserved = fixture.memory.snapshot().unwrap().reserved_bytes;
    for occurrence in 1..=measured.matching_requests {
      let (result, allocation) = measure_nth(SELECTOR_ALLOCATION, occurrence, load);
      assert!(allocation.injected_failure, "{allocation:?}");
      let error = match result {
        Err(error) => error,
        Ok(_) => panic!("selected catalog swallowed allocation failure"),
      };
      assert_eq!(error.code(), "selector_decode_allocation");
      assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::ResourceLimit);
      assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, reserved);
      drop(load().unwrap());
    }
  });
}

#[test]
fn native_selected_artifact_preserves_nested_and_chain_allocation_failures() {
  with_reader(|fixture, reader| {
    let registry = fixture.registry();
    let mut source = FirstAuthorityIndexCoverageRegistrySourceV1::new(Arc::clone(&fixture.publisher)).unwrap();
    let snapshot = registry.refresh(&mut source, &fixture.requests(), &CancellationToken::new()).unwrap();
    let mut catalog = reader.load_planner_catalogs("/", &[&fixture.field_name], default_native_selected_semantic_limits_v1()).unwrap();
    reader.bind_planner_coverage(&mut catalog, &snapshot).unwrap();
    let field = &catalog.catalogs()[0];
    let scope = &field.scopes[0];
    let request = NativeSelectedArtifactRootRequestV1 {
      catalog: field,
      scope_id: &scope.scope_id,
      selected_generation: scope.indexes[0].selected_generation.as_ref().unwrap(),
      role: OrderedIndexRoleV1::Posting,
    };
    let load = || reader.load_index_artifact_root(&request);
    let (warm, measured) = measure_nth(SELECTOR_ALLOCATION, usize::MAX, load);
    assert!(warm.unwrap().is_none());
    assert_eq!(measured.matching_requests, 2);
    let reserved = fixture.memory.snapshot().unwrap().reserved_bytes;
    for occurrence in 1..=measured.matching_requests {
      let (result, allocation) = measure_nth(SELECTOR_ALLOCATION, occurrence, load);
      assert!(allocation.injected_failure, "{allocation:?}");
      let error = match result {
        Err(error) => error,
        Ok(_) => panic!("selected artifact swallowed allocation failure"),
      };
      assert!(error.to_string().contains("selector_decode_allocation"), "{error}");
      assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::ResourceLimit);
      assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, reserved);
      assert!(load().unwrap().is_none());
    }
  });
}

#[test]
fn native_compaction_definition_allocation_failures_retry_without_publication() {
  let fixture = NativeFixture::new();
  let objects = FirstAuthorityIndexSemanticObjectReadSourceV1::new(Arc::clone(&fixture.publisher));
  let ordinals = Ordinals::default();
  let source = CatalogIndexSemanticScopeSourceV1::new(ALGORITHM, fixture.memory.as_ref().clone(), &objects, &ordinals);
  let executor = NativeIndexCompactionExecutorV1::new(
    DATABASE_ID,
    ALGORITHM,
    Arc::clone(&fixture.publisher),
    Arc::clone(&fixture.retirement),
    Arc::clone(&fixture.memory),
    &source,
    NativeIndexCompactionOptionsV1::engine_default(IndexSemanticScopeLimitsV1::new(8, 16, 32, 2 << 20).unwrap()).unwrap(),
  )
  .unwrap();
  let authority = fixture.publisher.load_selected_semantic_authority().unwrap();
  let request = IndexArtifactCompactionExecutionRequestV1 {
    operation_id: [0x81; 16],
    publication_sequence: authority.root_publication_sequence,
    namespace_root: &authority.root_hash,
    semantic_state_root: &authority.semantic_state.object_id,
    scope: "/",
    now_ms: 1_700_000_000_500,
    is_cancelled: &|| false,
  };
  let complete = IndexArtifactCompactionExecutionOutcomeV1::Complete { published_owners: 0, publication_bytes: 0 };
  let (warm, measured) = measure_nth(SELECTOR_ALLOCATION, usize::MAX, || executor.execute(request));
  assert_eq!(warm.unwrap(), complete);
  assert!(measured.matching_requests >= 4, "{measured:?}");
  let before = fixture.publisher.observe().unwrap();
  let before_bytes = std::fs::read(&fixture.path).unwrap();
  let reserved = fixture.memory.snapshot().unwrap().reserved_bytes;
  for occurrence in 1..=measured.matching_requests {
    let (result, allocation) = measure_nth(SELECTOR_ALLOCATION, occurrence, || executor.execute(request));
    assert!(allocation.injected_failure, "{allocation:?}");
    let error = result.unwrap_err();
    assert_eq!(error.code(), "selector_decode_allocation");
    assert_eq!(error.class(), IndexRuntimeCompactionErrorClassV1::RetryableBeforeSelection);
    assert_eq!(fixture.publisher.observe().unwrap(), before);
    assert_eq!(std::fs::read(&fixture.path).unwrap(), before_bytes);
    assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, reserved);
    assert_eq!(executor.execute(request).unwrap(), complete);
  }
}
