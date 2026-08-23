use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use aeordb::engine::v4::database_header::{SelectedDatabaseHeaderV4, decode_header_region};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::namespace::{NamespaceRootV1, NamespaceTreeLayoutV0, NamespaceTreeRootV0, SemanticAvailabilityV1, SemanticStateV1};
use aeordb::engine::v4::position::{
  LogicalPositionWriteV1, PositionComparatorV1, PositionComponentWriteV1, PositionRouteV1, PositionSortDirectionV1, encode_logical_position,
};
use aeordb::engine::v4::position_order::{
  DirectoryOrderFieldV1, LogicalOrderComponentOwnedV1, LogicalOrderRowOwnedV1, compile_aggregate_group_order_v1,
  compile_directory_listing_order_v1, compile_global_search_order_v1, compile_query_order_v1,
};
use aeordb::engine::v4::position_resolver::{
  PositionResolutionErrorClassV1, PositionResolutionLimitsV1, PositionUniverseLookupRequestV1, PositionUniverseLookupResultV1,
  PositionUniverseSourceErrorV1, PositionUniverseSourceV1, resolve_position_bound_v1,
};
use aeordb::engine::v4::read_view::{
  CurrentReadAuthorizationV1, LoadedReadAuthorityV1, ReadViewAuthoritySourceV1, ReadViewAuthorizationErrorV1,
  ReadViewAuthorizationFailureV1, ReadViewAuthorizerV1, ReadViewConcealmentV1, ReadViewCredentialKindV1, ReadViewLifecycleErrorV1,
  ReadViewResolverV1, ReadViewSelectorV1, ReadViewSourceErrorV1, ResolvedReadViewV1, RootLifecycleObservationV1, RootReadPinCoordinatorV1,
};
use aeordb::engine::v4::root_authority::{ImmutableNamespaceAuthorityV1, RootAdmissionCommitV1, RootAuthorityKindV1};
use tokio_util::sync::CancellationToken;

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/database-header-v4")
}

fn hash(algorithm: HashAlgorithm, byte: u8) -> Vec<u8> {
  vec![byte; algorithm.hash_length()]
}

fn selected_header(algorithm: HashAlgorithm, head_hash: Vec<u8>) -> SelectedDatabaseHeaderV4 {
  let name = match algorithm {
    HashAlgorithm::Blake3_256 => "header-blake3-256-valid-ab.bin",
    HashAlgorithm::Sha512 => "header-sha512-valid-ab.bin",
    _ => panic!("position resolution tests use frozen BLAKE3-256 and SHA-512 headers"),
  };
  let mut selected = decode_header_region(&fs::read(fixture_root().join(name)).unwrap()).unwrap();
  selected.header.head_hash = head_hash;
  selected.header.slot_sequence = 20;
  selected.header.write_sequence_high_water = 200;
  selected
}

fn authority(header: &SelectedDatabaseHeaderV4, root_hash: Vec<u8>) -> ImmutableNamespaceAuthorityV1 {
  let algorithm = header.header.hash_algorithm;
  let namespace_tree_root = hash(algorithm, 0x22);
  let semantic_state_root = hash(algorithm, 0x33);
  ImmutableNamespaceAuthorityV1 {
    root: NamespaceRootV1 {
      root_hash: root_hash.clone(),
      required_capabilities: [0; 32],
      namespace_tree_codec: 0,
      semantic_state_codec: 1,
      namespace_tree_root: namespace_tree_root.clone(),
      semantic_state_root: semantic_state_root.clone(),
    },
    namespace_tree: NamespaceTreeRootV0 { root_hash: namespace_tree_root, layout: NamespaceTreeLayoutV0::Empty, edges: Vec::new() },
    semantic_state: SemanticStateV1 {
      object_id: semantic_state_root,
      required_capabilities: [0; 32],
      semantic_catalog_codec: 1,
      semantic_definition_codec: 1,
      compiler_profile_version: 1,
      availability: SemanticAvailabilityV1::Complete {
        compiler_fingerprint: hash(algorithm, 0x41),
        semantic_registry_fingerprint: hash(algorithm, 0x42),
        catalog_root: hash(algorithm, 0x43),
        catalog_record_count: 1,
        catalog_node_count: 1,
        definition_count: 1,
        dependency_count: 0,
      },
    },
    admission: RootAdmissionCommitV1 {
      database_id: header.header.database_id,
      namespace_root: root_hash,
      transaction_id: [0x55; 16],
      publication_started_at_ms: 1_700_000_000_000,
      authority_kind: RootAuthorityKindV1::Head,
      recovered_from_selected_authority: false,
      authority_identity_digest: hash(algorithm, 0x56),
      authority_after: hash(algorithm, 0x57),
      selected_header_slot_sequence: 19,
      publication_sequence: 199,
      prepare_payload_hash: hash(algorithm, 0x58),
    },
  }
}

struct FakeAuthoritySource {
  header: SelectedDatabaseHeaderV4,
  root: Vec<u8>,
  authority: ImmutableNamespaceAuthorityV1,
}

impl ReadViewAuthoritySourceV1 for FakeAuthoritySource {
  fn capture_header(&self, _cancellation: &CancellationToken) -> Result<SelectedDatabaseHeaderV4, ReadViewSourceErrorV1> {
    Ok(self.header.clone())
  }

  fn load_verified_authority(
    &self,
    _header: &SelectedDatabaseHeaderV4,
    root_hash: &[u8],
    _cancellation: &CancellationToken,
  ) -> Result<LoadedReadAuthorityV1, ReadViewSourceErrorV1> {
    if root_hash != self.root {
      return Err(ReadViewSourceErrorV1::RootNotAdmitted);
    }
    Ok(LoadedReadAuthorityV1::new(self.authority.clone(), None))
  }

  fn observe_lifecycle(
    &self,
    _header: &SelectedDatabaseHeaderV4,
    _root_hash: &[u8],
    _cancellation: &CancellationToken,
  ) -> Result<RootLifecycleObservationV1, ReadViewLifecycleErrorV1> {
    Ok(RootLifecycleObservationV1::Live)
  }
}

struct AllowAuthorizer;

impl ReadViewAuthorizerV1 for AllowAuthorizer {
  type CurrentAuthorization = ();
  type ResolvedAuthorization = ();

  fn authorize_current(
    &self,
    _cancellation: &CancellationToken,
  ) -> Result<CurrentReadAuthorizationV1<Self::CurrentAuthorization>, ReadViewAuthorizationErrorV1> {
    Ok(CurrentReadAuthorizationV1::new((), ReadViewCredentialKindV1::Ordinary, ReadViewConcealmentV1::Reveal))
  }

  fn restrict_to_selected_root(
    &self,
    _current: &Self::CurrentAuthorization,
    _header: &SelectedDatabaseHeaderV4,
    _authority: &LoadedReadAuthorityV1,
    _cancellation: &CancellationToken,
  ) -> Result<Self::ResolvedAuthorization, ReadViewAuthorizationFailureV1> {
    Ok(())
  }
}

fn resolved_view(algorithm: HashAlgorithm, root: Vec<u8>, explicit: bool) -> ResolvedReadViewV1<()> {
  let header = selected_header(algorithm, root.clone());
  let source = Arc::new(FakeAuthoritySource { authority: authority(&header, root.clone()), header, root: root.clone() });
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(8 * 1_024 * 1_024, 16 * 1_024 * 1_024, 1, 1_024 * 1_024).unwrap()));
  let pins = RootReadPinCoordinatorV1::new(memory, algorithm, 8, 16).unwrap();
  let capabilities = CapabilitySetV1::from_bits(0..24).unwrap();
  let resolver = ReadViewResolverV1::new(source, pins, BinaryCapabilityProfileV1::new(capabilities, capabilities));
  let cancellation = CancellationToken::new();
  let selector = if explicit { ReadViewSelectorV1::ExplicitRoot(&root) } else { ReadViewSelectorV1::CurrentHead };
  resolver.resolve(selector, &AllowAuthorizer, &cancellation).unwrap()
}

fn present(comparator: PositionComparatorV1, payload: impl Into<Vec<u8>>) -> LogicalOrderComponentOwnedV1 {
  LogicalOrderComponentOwnedV1::present(comparator, payload.into())
}

fn file_row(
  algorithm: HashAlgorithm,
  route: PositionRouteV1,
  components: Vec<LogicalOrderComponentOwnedV1>,
  path: &str,
  revision: u8,
) -> LogicalOrderRowOwnedV1 {
  LogicalOrderRowOwnedV1 {
    route,
    components,
    file_key_tie: digest_parts(algorithm, &[b"file:", path.as_bytes()]),
    record_revision_tie: hash(algorithm, revision),
  }
}

fn query_row(algorithm: HashAlgorithm, path: &str, key: u8) -> LogicalOrderRowOwnedV1 {
  file_row(algorithm, PositionRouteV1::Query, vec![present(PositionComparatorV1::Utf8Binary, path.as_bytes())], path, key)
}

fn aggregate_row(algorithm: HashAlgorithm, input_root: &[u8], count: u64, group_tuple: &[u8]) -> LogicalOrderRowOwnedV1 {
  LogicalOrderRowOwnedV1 {
    route: PositionRouteV1::AggregateGroups,
    components: vec![present(PositionComparatorV1::U64, count.to_le_bytes()), present(PositionComparatorV1::BytesBinary, group_tuple)],
    file_key_tie: digest_parts(algorithm, &[group_tuple]),
    record_revision_tie: input_root.to_vec(),
  }
}

fn encode_row(order: &aeordb::engine::v4::position::CompiledRouteOrderV1, root: &[u8], row: &LogicalOrderRowOwnedV1) -> Vec<u8> {
  let components: Vec<_> = row
    .components
    .iter()
    .map(|component| PositionComponentWriteV1 { comparator: component.comparator, state: component.state, payload: &component.payload })
    .collect();
  encode_logical_position(&LogicalPositionWriteV1 {
    order,
    namespace_root: root,
    file_key_tie: &row.file_key_tie,
    record_revision_tie: &row.record_revision_tie,
    components: &components,
  })
  .unwrap()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ObservedLookup {
  database_id: [u8; 16],
  physical_instance_id: [u8; 16],
  root: Vec<u8>,
  route: PositionRouteV1,
  order_fingerprint: Vec<u8>,
  file_key: Vec<u8>,
  revision: Vec<u8>,
  maximum_row_bytes: u64,
}

#[derive(Clone)]
enum ModelResponse {
  Found(LogicalOrderRowOwnedV1),
  Absent,
  Unavailable,
  Resource,
  Corrupt,
  Cancelled,
}

struct ModelUniverseSource {
  response: ModelResponse,
  cancel_during_lookup: bool,
  calls: Vec<ObservedLookup>,
}

impl ModelUniverseSource {
  fn found(row: LogicalOrderRowOwnedV1) -> Self {
    Self { response: ModelResponse::Found(row), cancel_during_lookup: false, calls: Vec::new() }
  }
}

impl PositionUniverseSourceV1 for ModelUniverseSource {
  fn resolve_position(
    &mut self,
    request: PositionUniverseLookupRequestV1<'_>,
    cancellation: &CancellationToken,
  ) -> Result<PositionUniverseLookupResultV1, PositionUniverseSourceErrorV1> {
    self.calls.push(ObservedLookup {
      database_id: request.database_id(),
      physical_instance_id: request.physical_instance_id(),
      root: request.selected_root().to_vec(),
      route: request.route(),
      order_fingerprint: request.order_fingerprint().to_vec(),
      file_key: request.file_key_tie().to_vec(),
      revision: request.record_revision_tie().to_vec(),
      maximum_row_bytes: request.maximum_row_bytes(),
    });
    if self.cancel_during_lookup {
      cancellation.cancel();
    }
    match &self.response {
      ModelResponse::Found(row) => Ok(PositionUniverseLookupResultV1::Found(row.clone())),
      ModelResponse::Absent => Ok(PositionUniverseLookupResultV1::Absent),
      ModelResponse::Unavailable => Err(PositionUniverseSourceErrorV1::unavailable("model unavailable")),
      ModelResponse::Resource => Err(PositionUniverseSourceErrorV1::resource("model pressure")),
      ModelResponse::Corrupt => Err(PositionUniverseSourceErrorV1::corrupt("model corrupt")),
      ModelResponse::Cancelled => Err(PositionUniverseSourceErrorV1::cancelled()),
    }
  }
}

fn limits() -> PositionResolutionLimitsV1 {
  PositionResolutionLimitsV1::new(256 * 1024).unwrap()
}

#[test]
fn exact_file_positions_bind_the_authorized_view_and_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let root = hash(algorithm, 0x11);
    let view = resolved_view(algorithm, root.clone(), true);
    let order = compile_query_order_v1(algorithm, &[]).unwrap();
    let row = query_row(algorithm, "/docs/a.txt", 7);
    let token = encode_row(&order, &root, &row);
    let mut source = ModelUniverseSource::found(row.clone());

    let resolved = resolve_position_bound_v1(&token, &order, &view, limits(), &mut source).unwrap();
    assert_eq!(resolved.row(), &row);
    assert_eq!(source.calls.len(), 1);
    assert_eq!(
      source.calls[0],
      ObservedLookup {
        database_id: view.database_id(),
        physical_instance_id: view.physical_instance_id(),
        root,
        route: PositionRouteV1::Query,
        order_fingerprint: order.fingerprint().to_vec(),
        file_key: row.file_key_tie,
        revision: row.record_revision_tie,
        maximum_row_bytes: 256 * 1024,
      }
    );
    assert_eq!(source.calls[0].order_fingerprint, order.fingerprint());
    assert_eq!(view.hash_algorithm(), algorithm);
  }
}

#[test]
fn directory_search_query_and_synthetic_aggregate_positions_share_one_resolver() {
  let algorithm = HashAlgorithm::Blake3_256;
  let root = hash(algorithm, 0x11);
  let view = resolved_view(algorithm, root.clone(), true);
  let cases = [
    (
      compile_directory_listing_order_v1(algorithm, DirectoryOrderFieldV1::Name, PositionSortDirectionV1::Ascending).unwrap(),
      file_row(
        algorithm,
        PositionRouteV1::DirectoryListing,
        vec![
          present(PositionComparatorV1::U64, 0u64.to_le_bytes()),
          present(PositionComparatorV1::Utf8Binary, b"docs"),
          present(PositionComparatorV1::Utf8Binary, b"Docs"),
          present(PositionComparatorV1::Utf8Binary, b"/Docs"),
        ],
        "/Docs",
        1,
      ),
    ),
    (compile_query_order_v1(algorithm, &[]).unwrap(), query_row(algorithm, "/query", 2)),
    (
      compile_global_search_order_v1(algorithm).unwrap(),
      file_row(
        algorithm,
        PositionRouteV1::GlobalSearch,
        vec![present(PositionComparatorV1::FiniteF64, 0.75f64.to_le_bytes()), present(PositionComparatorV1::Utf8Binary, b"/search")],
        "/search",
        3,
      ),
    ),
    (compile_aggregate_group_order_v1(algorithm, &[]).unwrap(), aggregate_row(algorithm, &root, 42, b"group-a")),
  ];

  for (order, row) in cases {
    let token = encode_row(&order, &root, &row);
    let mut source = ModelUniverseSource::found(row.clone());
    assert_eq!(resolve_position_bound_v1(&token, &order, &view, limits(), &mut source).unwrap().row(), &row);
    assert_eq!(source.calls[0].route, row.route);
  }
}

#[test]
fn malformed_foreign_or_implicit_positions_fail_before_universe_access() {
  let algorithm = HashAlgorithm::Blake3_256;
  let root = hash(algorithm, 0x11);
  let explicit = resolved_view(algorithm, root.clone(), true);
  let implicit = resolved_view(algorithm, root.clone(), false);
  let order = compile_query_order_v1(algorithm, &[]).unwrap();
  let base_row = query_row(algorithm, "/a", 1);
  let token = encode_row(&order, &root, &base_row);

  let mut source = ModelUniverseSource::found(base_row.clone());
  assert_eq!(
    resolve_position_bound_v1(b"not+base64", &order, &explicit, limits(), &mut source).unwrap_err().class(),
    PositionResolutionErrorClassV1::InvalidPosition
  );
  assert!(source.calls.is_empty());

  assert_eq!(
    resolve_position_bound_v1(&token, &order, &implicit, limits(), &mut source).unwrap_err().class(),
    PositionResolutionErrorClassV1::InvalidPosition
  );
  assert!(source.calls.is_empty());

  let foreign_root = hash(algorithm, 0x44);
  let foreign_token = encode_row(&order, &foreign_root, &base_row);
  assert_eq!(
    resolve_position_bound_v1(&foreign_token, &order, &explicit, limits(), &mut source).unwrap_err().class(),
    PositionResolutionErrorClassV1::RootMismatch
  );
  assert!(source.calls.is_empty());

  let ordered_fields = [aeordb::engine::v4::position_order::PositionOrderFieldV1 {
    field: "rank",
    direction: PositionSortDirectionV1::Ascending,
    comparator: PositionComparatorV1::U64,
  }];
  let other_order = compile_query_order_v1(algorithm, &ordered_fields).unwrap();
  let other_row = file_row(
    algorithm,
    PositionRouteV1::Query,
    vec![present(PositionComparatorV1::U64, 1u64.to_le_bytes()), present(PositionComparatorV1::Utf8Binary, b"/a")],
    "/a",
    1,
  );
  let other_token = encode_row(&other_order, &root, &other_row);
  assert_eq!(
    resolve_position_bound_v1(&other_token, &order, &explicit, limits(), &mut source).unwrap_err().class(),
    PositionResolutionErrorClassV1::OrderMismatch
  );
  assert!(source.calls.is_empty());

  let search_order = compile_global_search_order_v1(algorithm).unwrap();
  let search_row = file_row(
    algorithm,
    PositionRouteV1::GlobalSearch,
    vec![present(PositionComparatorV1::FiniteF64, 1.0f64.to_le_bytes()), present(PositionComparatorV1::Utf8Binary, b"/a")],
    "/a",
    1,
  );
  let search_token = encode_row(&search_order, &root, &search_row);
  assert_eq!(
    resolve_position_bound_v1(&search_token, &order, &explicit, limits(), &mut source).unwrap_err().class(),
    PositionResolutionErrorClassV1::InvalidPosition
  );
  assert!(source.calls.is_empty());

  let mut invalid_file_identity = base_row.clone();
  invalid_file_identity.file_key_tie = hash(algorithm, 0x66);
  let invalid_file_token = encode_row(&order, &root, &invalid_file_identity);
  assert_eq!(
    resolve_position_bound_v1(&invalid_file_token, &order, &explicit, limits(), &mut source).unwrap_err().class(),
    PositionResolutionErrorClassV1::InvalidPosition
  );
  assert!(source.calls.is_empty());

  let aggregate_order = compile_aggregate_group_order_v1(algorithm, &[]).unwrap();
  let aggregate = aggregate_row(algorithm, &root, 2, b"group-b");
  for invalid in [
    LogicalOrderRowOwnedV1 { file_key_tie: hash(algorithm, 0x77), ..aggregate.clone() },
    LogicalOrderRowOwnedV1 { record_revision_tie: hash(algorithm, 0x78), ..aggregate.clone() },
  ] {
    let invalid_token = encode_row(&aggregate_order, &root, &invalid);
    assert_eq!(
      resolve_position_bound_v1(&invalid_token, &aggregate_order, &explicit, limits(), &mut source).unwrap_err().class(),
      PositionResolutionErrorClassV1::InvalidPosition
    );
    assert!(source.calls.is_empty());
  }

  let foreign_algorithm_order = compile_query_order_v1(HashAlgorithm::Sha512, &[]).unwrap();
  let foreign_algorithm = resolve_position_bound_v1(&token, &foreign_algorithm_order, &explicit, limits(), &mut source).unwrap_err();
  assert_eq!((foreign_algorithm.class(), foreign_algorithm.code()), (PositionResolutionErrorClassV1::Corrupt, "database_corruption"));
  assert!(source.calls.is_empty());

  let cancelled = resolved_view(algorithm, root, true);
  cancelled.cancellation().cancel();
  assert_eq!(
    resolve_position_bound_v1(&token, &order, &cancelled, limits(), &mut source).unwrap_err().class(),
    PositionResolutionErrorClassV1::Cancelled
  );
  assert!(source.calls.is_empty());
}

#[test]
fn stale_forged_and_dishonest_universe_results_fail_closed() {
  let algorithm = HashAlgorithm::Blake3_256;
  let root = hash(algorithm, 0x11);
  let view = resolved_view(algorithm, root.clone(), true);
  let order = compile_query_order_v1(algorithm, &[]).unwrap();
  let actual = query_row(algorithm, "/actual", 1);
  let ranked_fields = [aeordb::engine::v4::position_order::PositionOrderFieldV1 {
    field: "rank",
    direction: PositionSortDirectionV1::Ascending,
    comparator: PositionComparatorV1::U64,
  }];
  let ranked_order = compile_query_order_v1(algorithm, &ranked_fields).unwrap();
  let ranked_actual = file_row(
    algorithm,
    PositionRouteV1::Query,
    vec![present(PositionComparatorV1::U64, 1u64.to_le_bytes()), present(PositionComparatorV1::Utf8Binary, b"/same")],
    "/same",
    7,
  );
  let mut ranked_forged = ranked_actual.clone();
  ranked_forged.components[0] = present(PositionComparatorV1::U64, 2u64.to_le_bytes());
  let token = encode_row(&ranked_order, &root, &ranked_forged);

  let mut source = ModelUniverseSource::found(ranked_actual);
  assert_eq!(
    resolve_position_bound_v1(&token, &ranked_order, &view, limits(), &mut source).unwrap_err().class(),
    PositionResolutionErrorClassV1::InvalidPosition
  );

  let actual_token = encode_row(&order, &root, &actual);
  let mut absent = ModelUniverseSource { response: ModelResponse::Absent, cancel_during_lookup: false, calls: Vec::new() };
  assert_eq!(
    resolve_position_bound_v1(&actual_token, &order, &view, limits(), &mut absent).unwrap_err().class(),
    PositionResolutionErrorClassV1::InvalidPosition
  );

  let mut foreign = actual.clone();
  foreign.file_key_tie = hash(algorithm, 9);
  let mut source = ModelUniverseSource::found(foreign);
  assert_eq!(
    resolve_position_bound_v1(&actual_token, &order, &view, limits(), &mut source).unwrap_err().class(),
    PositionResolutionErrorClassV1::Corrupt
  );

  let mut malformed = actual.clone();
  malformed.route = PositionRouteV1::GlobalSearch;
  let mut source = ModelUniverseSource::found(malformed);
  assert_eq!(
    resolve_position_bound_v1(&actual_token, &order, &view, limits(), &mut source).unwrap_err().class(),
    PositionResolutionErrorClassV1::Corrupt
  );

  let oversized = query_row(algorithm, &format!("/{}", "x".repeat(8 * 1024)), 2);
  let oversized_token = encode_row(&order, &root, &oversized);
  let tiny = PositionResolutionLimitsV1::new(128).unwrap();
  let mut source = ModelUniverseSource::found(oversized);
  assert_eq!(
    resolve_position_bound_v1(&oversized_token, &order, &view, tiny, &mut source).unwrap_err().class(),
    PositionResolutionErrorClassV1::ResourceLimit
  );
}

#[test]
fn source_failures_and_cancellation_keep_their_non_success_class() {
  let algorithm = HashAlgorithm::Blake3_256;
  let root = hash(algorithm, 0x11);
  let view = resolved_view(algorithm, root.clone(), true);
  let order = compile_query_order_v1(algorithm, &[]).unwrap();
  let row = query_row(algorithm, "/a", 1);
  let token = encode_row(&order, &root, &row);

  for (response, expected) in [
    (ModelResponse::Unavailable, PositionResolutionErrorClassV1::Unavailable),
    (ModelResponse::Resource, PositionResolutionErrorClassV1::ResourceLimit),
    (ModelResponse::Corrupt, PositionResolutionErrorClassV1::Corrupt),
    (ModelResponse::Cancelled, PositionResolutionErrorClassV1::Cancelled),
  ] {
    let mut source = ModelUniverseSource { response, cancel_during_lookup: false, calls: Vec::new() };
    assert_eq!(resolve_position_bound_v1(&token, &order, &view, limits(), &mut source).unwrap_err().class(), expected);
  }

  let view = resolved_view(algorithm, root, true);
  let mut source = ModelUniverseSource::found(row);
  source.cancel_during_lookup = true;
  assert_eq!(
    resolve_position_bound_v1(&token, &order, &view, limits(), &mut source).unwrap_err().class(),
    PositionResolutionErrorClassV1::Cancelled
  );

  assert_eq!(PositionResolutionLimitsV1::new(0).unwrap_err().class(), PositionResolutionErrorClassV1::ResourceLimit);
}

#[test]
fn resolver_exposes_the_frozen_client_error_codes() {
  let algorithm = HashAlgorithm::Blake3_256;
  let root = hash(algorithm, 0x11);
  let view = resolved_view(algorithm, root.clone(), true);
  let order = compile_query_order_v1(algorithm, &[]).unwrap();
  let row = query_row(algorithm, "/a", 1);
  let mut source = ModelUniverseSource::found(row.clone());

  let malformed = resolve_position_bound_v1(b"malformed", &order, &view, limits(), &mut source).unwrap_err();
  assert_eq!((malformed.class(), malformed.code()), (PositionResolutionErrorClassV1::InvalidPosition, "invalid_position_cursor"));

  let foreign = encode_row(&order, &hash(algorithm, 0x22), &row);
  let mismatch = resolve_position_bound_v1(&foreign, &order, &view, limits(), &mut source).unwrap_err();
  assert_eq!((mismatch.class(), mismatch.code()), (PositionResolutionErrorClassV1::RootMismatch, "position_root_mismatch"));

  let mut unavailable = ModelUniverseSource { response: ModelResponse::Unavailable, cancel_during_lookup: false, calls: Vec::new() };
  let token = encode_row(&order, &root, &row);
  let unavailable = resolve_position_bound_v1(&token, &order, &view, limits(), &mut unavailable).unwrap_err();
  assert_eq!((unavailable.class(), unavailable.code()), (PositionResolutionErrorClassV1::Unavailable, "historical_view_unavailable"));
}

#[test]
fn resolver_is_unique_storage_neutral_and_disconnected_from_legacy_queries() {
  let package = Path::new(env!("CARGO_MANIFEST_DIR"));
  let source = fs::read_to_string(package.join("src/engine/v4/position_resolver.rs")).unwrap();
  let modules = fs::read_to_string(package.join("src/engine/v4/mod.rs")).unwrap();
  assert_eq!(source.matches("pub fn resolve_position_bound_v1").count(), 1);
  assert_eq!(modules.matches("pub mod position_resolver;").count(), 1);
  for forbidden in [
    "StorageEngine",
    "DirectoryOps",
    "crate::server",
    "server::",
    "QueryEngine",
    "legacy",
    "tokio::spawn",
    "thread::spawn",
    "read_to_end",
    ".sort_by(",
  ] {
    assert!(!source.contains(forbidden), "position resolver contains forbidden dependency {forbidden:?}");
  }
}
