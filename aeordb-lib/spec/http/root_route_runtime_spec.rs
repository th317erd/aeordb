use std::fs;
use std::path::Path;
use std::sync::Arc;

use aeordb::auth::jwt::JwtManager;
use aeordb::server::root_api::{
  HttpMethodV1, RootResponseShapeV1, RootRouteClassV1, RouteRootContractWitnessV1, root_contract_middleware,
  route_root_operation_contract_v1,
};
use aeordb::server::{create_app_with_jwt_and_engine, create_temp_engine_for_tests};
use axum::body::{Body, to_bytes};
use axum::extract::{Extension, Request};
use axum::http::{Method, StatusCode};
use axum::middleware::from_fn;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use tower::ServiceExt;

async fn witnessed(Extension(witness): Extension<RouteRootContractWitnessV1>) -> Response {
  let body = format!("{}:{:?}", witness.path, witness.operation.method);
  Response::builder().status(StatusCode::OK).body(Body::from(body)).unwrap()
}

async fn witnessed_body(Extension(_witness): Extension<RouteRootContractWitnessV1>, request: Request) -> Response {
  let bytes = to_bytes(request.into_body(), 1024).await.unwrap();
  Response::builder().status(StatusCode::OK).body(Body::from(bytes)).unwrap()
}

#[test]
fn runtime_lookup_covers_explicit_methods_and_axum_implicit_head() {
  let query = route_root_operation_contract_v1("/files/query", &Method::POST).unwrap();
  assert_eq!(query.method, HttpMethodV1::Post);
  assert_eq!(query.class, RootRouteClassV1::SingleRootNamespace);
  assert_eq!(query.response, RootResponseShapeV1::JsonEnvelope);

  let explicit_head = route_root_operation_contract_v1("/files/{*path}", &Method::HEAD).unwrap();
  assert_eq!(explicit_head.method, HttpMethodV1::Head);
  assert_eq!(explicit_head.class, RootRouteClassV1::SingleRootNamespace);
  assert_eq!(explicit_head.response, RootResponseShapeV1::Headers);

  let implicit_head = route_root_operation_contract_v1("/files", &Method::HEAD).unwrap();
  assert_eq!(implicit_head.method, HttpMethodV1::Head);
  assert_eq!(implicit_head.class, RootRouteClassV1::SingleRootNamespace);
  assert_eq!(implicit_head.response, RootResponseShapeV1::JsonEnvelope);

  assert!(route_root_operation_contract_v1("/files/query", &Method::HEAD).is_none());
  assert!(route_root_operation_contract_v1("/files", &Method::CONNECT).is_none());
  assert!(route_root_operation_contract_v1("/not-registered", &Method::GET).is_none());
}

#[tokio::test]
async fn middleware_exposes_the_exact_witness_to_handler_and_response() {
  let router = Router::new().route("/files", get(witnessed)).layer(from_fn(root_contract_middleware));

  let response = router.oneshot(Request::builder().method(Method::GET).uri("/files").body(Body::empty()).unwrap()).await.unwrap();

  assert_eq!(response.status(), StatusCode::OK);
  let witness = response.extensions().get::<RouteRootContractWitnessV1>().copied().unwrap();
  assert_eq!(witness.path, "/files");
  assert_eq!(witness.operation.method, HttpMethodV1::Get);
  assert_eq!(to_bytes(response.into_body(), 1024).await.unwrap(), "/files:Get");
}

#[tokio::test]
async fn middleware_classifies_implicit_head_without_changing_axum_method_behavior() {
  let router = Router::new().route("/system/health", get(witnessed)).layer(from_fn(root_contract_middleware));

  let response = router.oneshot(Request::builder().method(Method::HEAD).uri("/system/health").body(Body::empty()).unwrap()).await.unwrap();

  assert_eq!(response.status(), StatusCode::OK);
  let witness = response.extensions().get::<RouteRootContractWitnessV1>().copied().unwrap();
  assert_eq!(witness.path, "/system/health");
  assert_eq!(witness.operation.method, HttpMethodV1::Head);
  assert!(to_bytes(response.into_body(), 1024).await.unwrap().is_empty());
}

#[tokio::test]
async fn middleware_leaves_undeclared_methods_and_unknown_paths_to_axum() {
  let router = Router::new().route("/files/query", post(witnessed)).layer(from_fn(root_contract_middleware));

  let method_response =
    router.clone().oneshot(Request::builder().method(Method::GET).uri("/files/query").body(Body::empty()).unwrap()).await.unwrap();
  assert_eq!(method_response.status(), StatusCode::METHOD_NOT_ALLOWED);
  assert!(method_response.extensions().get::<RouteRootContractWitnessV1>().is_none());

  let missing_response = router.oneshot(Request::builder().method(Method::GET).uri("/missing").body(Body::empty()).unwrap()).await.unwrap();
  assert_eq!(missing_response.status(), StatusCode::NOT_FOUND);
  assert!(missing_response.extensions().get::<RouteRootContractWitnessV1>().is_none());
}

#[tokio::test]
async fn middleware_does_not_buffer_replace_or_truncate_request_bodies() {
  let router = Router::new().route("/files/query", post(witnessed_body)).layer(from_fn(root_contract_middleware));
  let expected = b"streamed request bytes remain exact";

  let response = router
    .oneshot(Request::builder().method(Method::POST).uri("/files/query").body(Body::from(expected.as_slice())).unwrap())
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::OK);
  assert_eq!(to_bytes(response.into_body(), 1024).await.unwrap(), expected.as_slice());
}

#[tokio::test]
async fn assembled_app_witness_wraps_public_and_protected_routes() {
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let app = create_app_with_jwt_and_engine(jwt_manager, engine);

  let health =
    app.clone().oneshot(Request::builder().method(Method::GET).uri("/system/health").body(Body::empty()).unwrap()).await.unwrap();
  assert_eq!(health.status(), StatusCode::OK);
  let health_witness = health.extensions().get::<RouteRootContractWitnessV1>().copied().unwrap();
  assert_eq!(health_witness.path, "/system/health");
  assert_eq!(health_witness.operation.method, HttpMethodV1::Get);
  assert_eq!(health_witness.operation.class, RootRouteClassV1::OperationalSystem);

  let protected = app.oneshot(Request::builder().method(Method::GET).uri("/files").body(Body::empty()).unwrap()).await.unwrap();
  assert_eq!(protected.status(), StatusCode::UNAUTHORIZED);
  let protected_witness = protected.extensions().get::<RouteRootContractWitnessV1>().copied().unwrap();
  assert_eq!(protected_witness.path, "/files");
  assert_eq!(protected_witness.operation.method, HttpMethodV1::Get);
  assert_eq!(protected_witness.operation.class, RootRouteClassV1::SingleRootNamespace);
}

#[test]
fn assembled_server_installs_exactly_one_whole_router_contract_middleware() {
  let source = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server/mod.rs")).unwrap();
  assert_eq!(source.matches("from_fn(root_api::root_contract_middleware)").count(), 1);

  let root_source = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server/root_api.rs")).unwrap();
  for forbidden in ["DirectoryOps", "StorageEngine", "ReadViewResolverV1::new", "NativeReadViewSourceV1::new"] {
    assert!(!root_source.contains(forbidden), "route witness unexpectedly activates or depends on storage: {forbidden}");
  }
}
