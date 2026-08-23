use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use aeordb::auth::jwt::JwtManager;
use aeordb::server::root_api::{
  AuthorizationOwnerV1, HttpMethodV1, ReadViewProofV1, RootRequestAdapterV1, RootRequestPlanErrorV1, RootResponseShapeV1, RootRouteClassV1,
  RootSelectorLocationV1, RootServiceActivationV1, RootServiceModeV1, RouteRootContractWitnessV1, RouteRootOperationContractV1,
  RouteRootRequestPlanV1, root_contract_middleware, route_root_contracts_v1, route_root_operation_contract_v1, route_root_request_plan_v1,
};
use aeordb::server::{create_app_with_jwt_and_engine, create_temp_engine_for_tests};
use axum::body::Body;
use axum::extract::{Extension, Request};
use axum::http::{Method, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use tower::ServiceExt;

fn expected_adapter(class: RootRouteClassV1) -> RootRequestAdapterV1 {
  match class {
    RootRouteClassV1::SingleRootNamespace => RootRequestAdapterV1::ResolveSingleRoot,
    RootRouteClassV1::MultiRoot => RootRequestAdapterV1::ResolveMultipleRoots,
    RootRouteClassV1::ContentStaging => RootRequestAdapterV1::TransportContent,
    RootRouteClassV1::HashRetrieval => RootRequestAdapterV1::RetrieveHashFromSelectedRoot,
    RootRouteClassV1::OperationalSystem => RootRequestAdapterV1::ExecuteOperational,
    RootRouteClassV1::Mutation => RootRequestAdapterV1::PublishCurrentMutation,
  }
}

fn operation(class: RootRouteClassV1, proof: ReadViewProofV1) -> RouteRootOperationContractV1 {
  RouteRootOperationContractV1 {
    method: HttpMethodV1::Get,
    class,
    selector: RootSelectorLocationV1::None,
    response: RootResponseShapeV1::None,
    authorization: AuthorizationOwnerV1::Middleware,
    proof,
  }
}

#[test]
fn every_registered_and_inherited_operation_has_one_six_class_request_plan() {
  let activation = RootServiceActivationV1::inactive_v4();
  assert_eq!(activation.mode(), RootServiceModeV1::LegacyV3Compatibility);
  let mut adapters = BTreeSet::new();

  for registration in route_root_contracts_v1() {
    let has_explicit_head = registration.operations.iter().any(|operation| operation.method == HttpMethodV1::Head);
    for operation in registration.operations {
      let witness = RouteRootContractWitnessV1 { path: registration.path, operation: *operation };
      let plan = route_root_request_plan_v1(witness, activation).unwrap();
      assert_eq!(plan.witness, witness);
      assert_eq!(plan.service_mode, RootServiceModeV1::LegacyV3Compatibility);
      assert_eq!(plan.adapter, expected_adapter(operation.class));
      adapters.insert(plan.adapter);

      if operation.method == HttpMethodV1::Get && !has_explicit_head {
        let inherited = route_root_operation_contract_v1(registration.path, &Method::HEAD).unwrap();
        let inherited_witness = RouteRootContractWitnessV1 { path: registration.path, operation: inherited };
        let inherited_plan = route_root_request_plan_v1(inherited_witness, activation).unwrap();
        assert_eq!(inherited_plan.adapter, plan.adapter);
        assert_eq!(inherited_plan.witness.operation.method, HttpMethodV1::Head);
      }
    }
  }

  assert_eq!(adapters.len(), 6);
}

#[test]
fn inconsistent_class_and_proof_pairs_fail_closed() {
  let activation = RootServiceActivationV1::inactive_v4();
  let invalid = [
    (RootRouteClassV1::SingleRootNamespace, ReadViewProofV1::NoNamespace),
    (RootRouteClassV1::MultiRoot, ReadViewProofV1::ResolvedReadView),
    (RootRouteClassV1::ContentStaging, ReadViewProofV1::NoNamespace),
    (RootRouteClassV1::HashRetrieval, ReadViewProofV1::ContentTransport),
    (RootRouteClassV1::OperationalSystem, ReadViewProofV1::ResolvedReadView),
    (RootRouteClassV1::Mutation, ReadViewProofV1::MultiRootResolver),
  ];

  for (class, proof) in invalid {
    let witness = RouteRootContractWitnessV1 { path: "/invalid", operation: operation(class, proof) };
    assert_eq!(route_root_request_plan_v1(witness, activation), Err(RootRequestPlanErrorV1::ClassProofMismatch { class, proof }));
  }
}

async fn planned(Extension(_plan): Extension<RouteRootRequestPlanV1>) -> Response {
  Response::builder().status(StatusCode::OK).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn whole_router_middleware_attaches_each_semantic_plan_before_dispatch() {
  let activation = RootServiceActivationV1::inactive_v4();
  let router = Router::new()
    .route("/files", get(planned))
    .route("/versions/diff", post(planned))
    .route("/blobs/config", get(planned))
    .route("/blobs/{hex_hash}", get(planned))
    .route("/system/health", get(planned))
    .route("/files/mkdir", post(planned))
    .layer(from_fn_with_state(activation, root_contract_middleware));
  let cases = [
    (Method::GET, "/files", RootRequestAdapterV1::ResolveSingleRoot),
    (Method::POST, "/versions/diff", RootRequestAdapterV1::ResolveMultipleRoots),
    (Method::GET, "/blobs/config", RootRequestAdapterV1::TransportContent),
    (Method::GET, "/blobs/abc", RootRequestAdapterV1::RetrieveHashFromSelectedRoot),
    (Method::GET, "/system/health", RootRequestAdapterV1::ExecuteOperational),
    (Method::POST, "/files/mkdir", RootRequestAdapterV1::PublishCurrentMutation),
  ];

  for (method, uri, expected) in cases {
    let response = router.clone().oneshot(Request::builder().method(method).uri(uri).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let plan = response.extensions().get::<RouteRootRequestPlanV1>().copied().unwrap();
    assert_eq!(plan.adapter, expected);
    assert_eq!(plan.service_mode, RootServiceModeV1::LegacyV3Compatibility);
  }
}

#[tokio::test]
async fn assembled_app_uses_the_inactive_v4_plan_for_public_and_protected_routes() {
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let app = create_app_with_jwt_and_engine(jwt_manager, engine);

  let health =
    app.clone().oneshot(Request::builder().method(Method::GET).uri("/system/health").body(Body::empty()).unwrap()).await.unwrap();
  assert_eq!(health.status(), StatusCode::OK);
  let health_plan = health.extensions().get::<RouteRootRequestPlanV1>().copied().unwrap();
  assert_eq!(health_plan.adapter, RootRequestAdapterV1::ExecuteOperational);
  assert_eq!(health_plan.service_mode, RootServiceModeV1::LegacyV3Compatibility);

  let protected = app.oneshot(Request::builder().method(Method::GET).uri("/files").body(Body::empty()).unwrap()).await.unwrap();
  assert_eq!(protected.status(), StatusCode::UNAUTHORIZED);
  let protected_plan = protected.extensions().get::<RouteRootRequestPlanV1>().copied().unwrap();
  assert_eq!(protected_plan.adapter, RootRequestAdapterV1::ResolveSingleRoot);
  assert_eq!(protected_plan.service_mode, RootServiceModeV1::LegacyV3Compatibility);
}

#[test]
fn source_has_one_inactive_activation_boundary_and_no_handler_class_inference() {
  let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
  let server = fs::read_to_string(manifest.join("src/server/mod.rs")).unwrap();
  assert_eq!(server.matches("RootServiceActivationV1::inactive_v4()").count(), 1);
  assert_eq!(server.matches("from_fn_with_state(root_service_activation, root_api::root_contract_middleware)").count(), 1);

  let root_source = fs::read_to_string(manifest.join("src/server/root_api.rs")).unwrap();
  assert!(!root_source.contains("fn active_v4"));
  for forbidden in ["DirectoryOps", "StorageEngine", "ReadViewResolverV1::new", "NativeReadViewSourceV1::new"] {
    assert!(!root_source.contains(forbidden), "request adapter unexpectedly activates or depends on storage: {forbidden}");
  }

  for entry in fs::read_dir(manifest.join("src/server")).unwrap() {
    let path = entry.unwrap().path();
    if path.extension().and_then(|extension| extension.to_str()) != Some("rs") || path.ends_with("root_api.rs") {
      continue;
    }
    let source = fs::read_to_string(&path).unwrap();
    assert!(!source.contains("RootRouteClassV1::"), "handler inferred a root class outside the frozen adapter: {}", path.display());
  }
}
