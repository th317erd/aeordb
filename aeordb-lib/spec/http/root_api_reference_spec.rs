use std::collections::{BTreeMap, BTreeSet};

use axum::http::StatusCode;
use serde_json::json;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::read_view::{ReadViewRootMetadataV1, ReadableRootStateV1};
use aeordb::server::root_api::{
  AuthorizationOwnerV1, HttpMethodV1, ReadViewProofV1, RequestedRootSelectorV1, RootApiErrorV1, RootResponseShapeV1, RootRouteClassV1,
  RootSelectorFieldsV1, RootSelectorLocationV1, parse_root_selector_v1, root_error_response_v1, root_response_headers_v1, root_response_v1,
  route_root_contracts_v1,
};

fn contract(path: &str, method: HttpMethodV1) -> &'static aeordb::server::root_api::RouteRootOperationContractV1 {
  route_root_contracts_v1()
    .iter()
    .find(|registration| registration.path == path)
    .and_then(|registration| registration.operations.iter().find(|operation| operation.method == method))
    .unwrap_or_else(|| panic!("missing route-root contract for {method:?} {path}"))
}

fn source_route_operations() -> BTreeMap<String, BTreeSet<HttpMethodV1>> {
  let source = include_str!("../../src/server/mod.rs");
  let bytes = source.as_bytes();
  let mut registrations = BTreeMap::new();
  let mut cursor = 0;

  while let Some(relative_start) = source[cursor..].find(".route(") {
    let open = cursor + relative_start + ".route".len();
    let close = matching_paren(bytes, open).expect("every route registration must have balanced parentheses");
    let arguments = &source[open + 1..close];
    let path_start = arguments.find('"').expect("route path must be a string literal");
    let path_end = quoted_string_end(arguments.as_bytes(), path_start).expect("route path must be terminated");
    let path = arguments[path_start + 1..path_end].to_string();
    let handler = &arguments[path_end + 1..];
    let methods = method_constructors(handler);
    assert!(!methods.is_empty(), "route source has no recognized HTTP method: {path}");
    assert!(registrations.insert(path.clone(), methods).is_none(), "duplicate Axum route registration: {path}");
    cursor = close + 1;
  }

  registrations
}

fn matching_paren(source: &[u8], open: usize) -> Option<usize> {
  let mut depth = 0_usize;
  let mut index = open;
  let mut in_string = false;
  let mut escaped = false;
  while index < source.len() {
    let byte = source[index];
    if in_string {
      if escaped {
        escaped = false;
      } else if byte == b'\\' {
        escaped = true;
      } else if byte == b'"' {
        in_string = false;
      }
    } else if byte == b'"' {
      in_string = true;
    } else if byte == b'(' {
      depth += 1;
    } else if byte == b')' {
      depth = depth.checked_sub(1)?;
      if depth == 0 {
        return Some(index);
      }
    }
    index += 1;
  }
  None
}

fn quoted_string_end(source: &[u8], open: usize) -> Option<usize> {
  let mut index = open + 1;
  let mut escaped = false;
  while index < source.len() {
    let byte = source[index];
    if escaped {
      escaped = false;
    } else if byte == b'\\' {
      escaped = true;
    } else if byte == b'"' {
      return Some(index);
    }
    index += 1;
  }
  None
}

fn method_constructors(source: &str) -> BTreeSet<HttpMethodV1> {
  let mut methods = BTreeSet::new();
  let bytes = source.as_bytes();
  let mut index = 0;
  while index < bytes.len() {
    if bytes[index].is_ascii_alphabetic() || bytes[index] == b'_' {
      let start = index;
      index += 1;
      while index < bytes.len() && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_') {
        index += 1;
      }
      let identifier = &source[start..index];
      let mut next = index;
      while next < bytes.len() && bytes[next].is_ascii_whitespace() {
        next += 1;
      }
      if next < bytes.len() && bytes[next] == b'(' {
        let method = match identifier {
          "get" => Some(HttpMethodV1::Get),
          "post" => Some(HttpMethodV1::Post),
          "put" => Some(HttpMethodV1::Put),
          "patch" => Some(HttpMethodV1::Patch),
          "delete" => Some(HttpMethodV1::Delete),
          "head" => Some(HttpMethodV1::Head),
          _ => None,
        };
        if let Some(method) = method {
          assert!(methods.insert(method), "duplicate HTTP method constructor in route source: {identifier}");
        }
      }
    } else {
      index += 1;
    }
  }
  methods
}

#[test]
fn selectors_are_mutually_exclusive_bounded_and_canonical() {
  let algorithm = HashAlgorithm::Blake3_256;
  assert_eq!(parse_root_selector_v1(&RootSelectorFieldsV1::default(), algorithm).unwrap(), RequestedRootSelectorV1::CurrentHead);

  let root = vec![0xab; algorithm.hash_length()];
  assert_eq!(
    parse_root_selector_v1(
      &RootSelectorFieldsV1 { root_hash: Some(hex::encode(&root).to_uppercase()), snapshot: None, version: None },
      algorithm,
    )
    .unwrap(),
    RequestedRootSelectorV1::ExplicitRoot(root.clone())
  );
  assert_eq!(
    parse_root_selector_v1(
      &RootSelectorFieldsV1 { root_hash: None, snapshot: Some("before-import".to_string()), version: None },
      algorithm,
    )
    .unwrap(),
    RequestedRootSelectorV1::Snapshot("before-import".to_string())
  );
  assert_eq!(
    parse_root_selector_v1(&RootSelectorFieldsV1 { root_hash: None, snapshot: None, version: Some(hex::encode(&root)) }, algorithm,)
      .unwrap(),
    RequestedRootSelectorV1::VersionRoot(root)
  );

  for fields in [
    RootSelectorFieldsV1 { root_hash: Some("11".repeat(32)), snapshot: Some("s".to_string()), version: None },
    RootSelectorFieldsV1 { root_hash: None, snapshot: Some("s".to_string()), version: Some("22".repeat(32)) },
    RootSelectorFieldsV1 { root_hash: Some("11".repeat(32)), snapshot: None, version: Some("22".repeat(32)) },
  ] {
    assert_eq!(parse_root_selector_v1(&fields, algorithm).unwrap_err(), RootApiErrorV1::InvalidRootSelector);
  }

  for invalid in ["", "00", &"00".repeat(32), &"zz".repeat(32), &"11".repeat(64)] {
    let fields = RootSelectorFieldsV1 { root_hash: Some(invalid.to_string()), snapshot: None, version: None };
    assert_eq!(parse_root_selector_v1(&fields, algorithm).unwrap_err(), RootApiErrorV1::InvalidRootHash);
  }

  let oversized = RootSelectorFieldsV1 { root_hash: None, snapshot: Some("s".repeat(4097)), version: None };
  assert_eq!(parse_root_selector_v1(&oversized, algorithm).unwrap_err(), RootApiErrorV1::InvalidRootSelector);
  let empty = RootSelectorFieldsV1 { root_hash: None, snapshot: Some(String::new()), version: None };
  assert_eq!(parse_root_selector_v1(&empty, algorithm).unwrap_err(), RootApiErrorV1::InvalidRootSelector);
}

#[test]
fn every_successful_root_shape_uses_exact_lowercase_metadata() {
  let algorithm = HashAlgorithm::Blake3_256;
  let hash = vec![0xcd; algorithm.hash_length()];
  let live = ReadViewRootMetadataV1 { hash: hash.clone(), state: ReadableRootStateV1::Live, expires_at_ms: None };
  let live_response = root_response_v1(&live, algorithm).unwrap();
  assert_eq!(
    serde_json::to_value(&live_response).unwrap(),
    json!({
      "hash": hex::encode(&hash),
      "state": "live",
      "expires_at": null
    })
  );

  let pending = ReadViewRootMetadataV1 {
    hash: hash.clone(),
    state: ReadableRootStateV1::PendingDelete { pending_since_ms: 1_700_000_000_000, expires_at_ms: 1_700_086_400_000 },
    expires_at_ms: Some(1_700_086_400_000),
  };
  let pending_response = root_response_v1(&pending, algorithm).unwrap();
  assert_eq!(
    serde_json::to_value(&pending_response).unwrap(),
    json!({
      "hash": hex::encode(&hash),
      "state": "pending_delete",
      "expires_at": 1_700_086_400_000_i64
    })
  );

  let headers = root_response_headers_v1(&pending, algorithm).unwrap();
  assert_eq!(headers.get("x-aeordb-root-hash").unwrap().to_str().unwrap(), hex::encode(&hash));
  assert_eq!(headers.get("x-aeordb-root-state").unwrap(), "pending_delete");
  assert_eq!(headers.get("x-aeordb-root-expires-at").unwrap(), "1700086400000");

  let malformed = ReadViewRootMetadataV1 { hash: vec![0xcd; 31], state: ReadableRootStateV1::Live, expires_at_ms: None };
  assert_eq!(root_response_v1(&malformed, algorithm).unwrap_err(), RootApiErrorV1::InvalidNamespaceRoot);

  let inconsistent = ReadViewRootMetadataV1 {
    hash,
    state: ReadableRootStateV1::PendingDelete { pending_since_ms: 1, expires_at_ms: 10 },
    expires_at_ms: Some(11),
  };
  assert_eq!(root_response_v1(&inconsistent, algorithm).unwrap_err(), RootApiErrorV1::DatabaseCorruption);
}

#[test]
fn root_errors_have_stable_codes_statuses_and_concealment() {
  let cases = [
    (RootApiErrorV1::InvalidRootHash, StatusCode::BAD_REQUEST, "INVALID_ROOT_HASH"),
    (RootApiErrorV1::InvalidRootSelector, StatusCode::BAD_REQUEST, "INVALID_ROOT_SELECTOR"),
    (RootApiErrorV1::InvalidPagination, StatusCode::BAD_REQUEST, "INVALID_PAGINATION"),
    (RootApiErrorV1::InvalidPositionCursor, StatusCode::BAD_REQUEST, "INVALID_POSITION_CURSOR"),
    (RootApiErrorV1::PositionRootMismatch, StatusCode::BAD_REQUEST, "POSITION_ROOT_MISMATCH"),
    (RootApiErrorV1::PositionOrderMismatch, StatusCode::BAD_REQUEST, "POSITION_ORDER_MISMATCH"),
    (RootApiErrorV1::RootExpired, StatusCode::GONE, "ROOT_EXPIRED"),
    (RootApiErrorV1::InvalidNamespaceRoot, StatusCode::NOT_FOUND, "INVALID_NAMESPACE_ROOT"),
    (RootApiErrorV1::HistoricalViewUnavailable, StatusCode::SERVICE_UNAVAILABLE, "HISTORICAL_VIEW_UNAVAILABLE"),
    (RootApiErrorV1::DatabaseCorruption, StatusCode::INTERNAL_SERVER_ERROR, "DATABASE_CORRUPTION"),
  ];
  for (error, expected_status, expected_code) in cases {
    let response = root_error_response_v1(error, false);
    assert_eq!(response.status, expected_status);
    assert_eq!(response.body.code.as_deref(), Some(expected_code));
    assert!(!response.body.error.is_empty());

    let concealed = root_error_response_v1(error, true);
    assert_eq!(concealed.status, StatusCode::NOT_FOUND);
    assert_eq!(concealed.body.code.as_deref(), Some("NOT_FOUND"));
    assert_eq!(concealed.body.error, "Not found");
  }
}

#[test]
fn route_matrix_covers_every_registration_and_method_once() {
  let registrations = route_root_contracts_v1();
  assert_eq!(registrations.len(), 95, "the route-root registry must cover every Axum registration");
  assert_eq!(registrations.iter().map(|registration| registration.operations.len()).sum::<usize>(), 124);

  let mut paths = BTreeSet::new();
  let mut operations = BTreeSet::new();
  for registration in registrations {
    assert!(paths.insert(registration.path), "duplicate route registration contract: {}", registration.path);
    assert!(!registration.operations.is_empty(), "route registration has no method contract: {}", registration.path);
    for operation in registration.operations {
      assert!(
        operations.insert((registration.path, operation.method)),
        "duplicate method contract: {:?} {}",
        operation.method,
        registration.path
      );
      match operation.class {
        RootRouteClassV1::SingleRootNamespace | RootRouteClassV1::HashRetrieval => {
          assert_ne!(operation.selector, RootSelectorLocationV1::None);
          assert_ne!(operation.response, RootResponseShapeV1::None);
          assert!(matches!(operation.proof, ReadViewProofV1::ResolvedReadView | ReadViewProofV1::PluginHost));
        }
        RootRouteClassV1::MultiRoot => assert_eq!(operation.proof, ReadViewProofV1::MultiRootResolver),
        RootRouteClassV1::ContentStaging => assert_eq!(operation.proof, ReadViewProofV1::ContentTransport),
        RootRouteClassV1::Mutation => assert_eq!(operation.proof, ReadViewProofV1::MutationRejectsGenericRoot),
        RootRouteClassV1::OperationalSystem => {
          assert!(matches!(operation.proof, ReadViewProofV1::NoNamespace | ReadViewProofV1::PluginHost))
        }
      }
      if operation.authorization == AuthorizationOwnerV1::Public {
        assert!(!matches!(operation.class, RootRouteClassV1::SingleRootNamespace | RootRouteClassV1::HashRetrieval));
      }
    }
  }
}

#[test]
fn route_matrix_matches_the_actual_axum_source_surface() {
  let source = source_route_operations();
  let registry = route_root_contracts_v1()
    .iter()
    .map(|registration| {
      (registration.path.to_string(), registration.operations.iter().map(|operation| operation.method).collect::<BTreeSet<_>>())
    })
    .collect::<BTreeMap<_, _>>();

  assert_eq!(source, registry, "Axum route registrations and root-policy contracts must change together");
}

#[test]
fn route_matrix_freezes_high_risk_read_write_and_transport_boundaries() {
  let listing = contract("/files", HttpMethodV1::Get);
  assert_eq!(listing.class, RootRouteClassV1::SingleRootNamespace);
  assert_eq!(listing.selector, RootSelectorLocationV1::Query);
  assert_eq!(listing.response, RootResponseShapeV1::JsonEnvelope);
  assert_eq!(listing.authorization, AuthorizationOwnerV1::CurrentThenSelectedPath);
  assert_eq!(listing.proof, ReadViewProofV1::ResolvedReadView);

  let query = contract("/files/query", HttpMethodV1::Post);
  assert_eq!(query.class, RootRouteClassV1::SingleRootNamespace);
  assert_eq!(query.selector, RootSelectorLocationV1::JsonBody);
  assert_eq!(query.response, RootResponseShapeV1::JsonEnvelope);

  let download = contract("/files/download", HttpMethodV1::Post);
  assert_eq!(download.class, RootRouteClassV1::SingleRootNamespace);
  assert_eq!(download.response, RootResponseShapeV1::Headers);

  let file_write = contract("/files/{*path}", HttpMethodV1::Put);
  assert_eq!(file_write.class, RootRouteClassV1::Mutation);
  assert_eq!(file_write.proof, ReadViewProofV1::MutationRejectsGenericRoot);

  let fetch_by_hash = contract("/blobs/{hex_hash}", HttpMethodV1::Get);
  assert_eq!(fetch_by_hash.class, RootRouteClassV1::HashRetrieval);
  assert_eq!(fetch_by_hash.response, RootResponseShapeV1::Headers);

  let chunk_commit = contract("/blobs/commit", HttpMethodV1::Post);
  assert_eq!(chunk_commit.class, RootRouteClassV1::ContentStaging);
  assert_eq!(chunk_commit.proof, ReadViewProofV1::ContentTransport);

  let diff = contract("/versions/diff", HttpMethodV1::Post);
  assert_eq!(diff.class, RootRouteClassV1::MultiRoot);
  assert_eq!(diff.response, RootResponseShapeV1::RootSet);

  let plugin = contract("/plugins/{name}/invoke", HttpMethodV1::Post);
  assert_eq!(plugin.class, RootRouteClassV1::OperationalSystem);
  assert_eq!(plugin.proof, ReadViewProofV1::PluginHost);

  let health = contract("/system/health", HttpMethodV1::Get);
  assert_eq!(health.class, RootRouteClassV1::OperationalSystem);
  assert_eq!(health.selector, RootSelectorLocationV1::None);
  assert_eq!(health.authorization, AuthorizationOwnerV1::Public);
}
