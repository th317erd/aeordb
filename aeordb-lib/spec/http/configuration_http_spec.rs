use std::sync::Arc;

use aeordb::auth::jwt::{DEFAULT_EXPIRY_SECONDS, JwtManager, TokenClaims};
use aeordb::engine::config_resolver::{ConfigurationFamily, MAX_CONFIG_DOCUMENT_BYTES};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::StorageEngine;
use aeordb::server::{create_app_with_jwt_and_engine, create_temp_engine_for_tests};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, temporary_directory) = create_temp_engine_for_tests();
  let application = create_app_with_jwt_and_engine(Arc::clone(&jwt_manager), Arc::clone(&engine));
  (application, jwt_manager, engine, temporary_directory)
}

fn bearer_token(jwt_manager: &JwtManager, subject: &str) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: subject.to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + DEFAULT_EXPIRY_SECONDS,
    scope: None,
    permissions: None,
    key_id: None,
  };
  format!("Bearer {}", jwt_manager.create_token(&claims).expect("create token"))
}

fn root_bearer_token(jwt_manager: &JwtManager) -> String {
  bearer_token(jwt_manager, "00000000-0000-0000-0000-000000000000")
}

fn non_root_bearer_token(jwt_manager: &JwtManager) -> String {
  bearer_token(jwt_manager, "11111111-1111-4111-8111-111111111111")
}

fn request(method: &str, uri: &str, authorization: &str, body: impl Into<Body>) -> Request<Body> {
  Request::builder()
    .method(method)
    .uri(uri)
    .header("authorization", authorization)
    .header("content-type", "application/json")
    .body(body.into())
    .expect("request")
}

async fn response_json(response: axum::response::Response) -> Value {
  let bytes = response.into_body().collect().await.expect("collect response").to_bytes();
  serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("JSON response: {error}; body={}", String::from_utf8_lossy(&bytes)))
}

#[tokio::test]
async fn root_get_returns_complete_runtime_and_lifecycle_envelopes() {
  let (application, jwt_manager, engine, _temporary_directory) = test_app();
  let authorization = root_bearer_token(&jwt_manager);

  let lifecycle_response =
    application.clone().oneshot(request("GET", "/system/lifecycle", &authorization, Body::empty())).await.expect("lifecycle response");
  assert_eq!(lifecycle_response.status(), StatusCode::OK);
  let lifecycle = response_json(lifecycle_response).await;
  assert_eq!(lifecycle["config"]["schema_version"], 1);
  assert_eq!(lifecycle["config"]["snapshot_writes_enabled"], true);
  assert_eq!(lifecycle["config"]["snapshot_retention"], json!({"auto_months": 0, "manual_months": 0}));
  assert_eq!(lifecycle["config"]["garbage_collection"]["pending_delete_grace_seconds"], 86_400);
  assert_eq!(lifecycle["invariants"]["required_complete_marks"], 2);
  assert_eq!(lifecycle["status"]["effective_valid"], true);
  assert_eq!(lifecycle["status"]["stored"]["state"], "missing");
  assert_eq!(lifecycle["status"]["sources"]["lifecycle.garbage_collection_pending_delete_grace_seconds"], "default");
  assert_eq!(lifecycle["status"]["disabled_capabilities"], json!(["configuration_recovery_controls"]));

  let runtime_response =
    application.oneshot(request("GET", "/system/runtime", &authorization, Body::empty())).await.expect("runtime response");
  assert_eq!(runtime_response.status(), StatusCode::OK);
  let runtime = response_json(runtime_response).await;
  assert_eq!(runtime["config"]["schema_version"], 1);
  assert_eq!(
    runtime["config"]["memory"]["hard_limit_bytes"],
    engine.configuration_snapshot().resolved_unsigned("memory.hard_limit_bytes").unwrap()
  );
  assert_eq!(runtime["status"]["sources"]["memory.hard_limit_bytes"], "default");
  assert_eq!(runtime["status"]["stored"]["state"], "missing");
  assert_eq!(runtime["status"]["degraded"], true);
  assert_eq!(runtime["status"]["disabled_capabilities"], json!(["query_plan_cache", "configuration_recovery_controls"]));
}

#[tokio::test]
async fn unavailable_dynamic_owner_remains_pending_and_reports_its_error() {
  let (application, jwt_manager, engine, _temporary_directory) = test_app();
  let authorization = root_bearer_token(&jwt_manager);
  let previous = engine.configuration_snapshot().resolved_unsigned("cache.query_plan_max_bytes").unwrap();

  let response = application
    .oneshot(request(
      "PUT",
      "/system/runtime",
      &authorization,
      Body::from(r#"{"schema_version":1,"cache":{"query_plan_max_bytes":8388608}}"#),
    ))
    .await
    .expect("runtime response");
  assert_eq!(response.status(), StatusCode::OK);
  let runtime = response_json(response).await;

  assert_eq!(runtime["config"]["cache"]["query_plan_max_bytes"], previous);
  assert_eq!(runtime["status"]["desired_config"]["cache"]["query_plan_max_bytes"], 8_388_608);
  assert_eq!(runtime["status"]["pending_convergence"]["cache.query_plan_max_bytes"], 8_388_608);
  assert!(runtime["status"]["convergence_errors"]["cache.query_plan_max_bytes"]
    .as_str()
    .unwrap_or("")
    .contains("query-plan cache owner is not implemented"));
  assert_eq!(runtime["status"]["degraded"], true);
}

#[tokio::test]
async fn configuration_routes_are_root_only_for_every_method() {
  let (application, jwt_manager, _engine, _temporary_directory) = test_app();
  let authorization = non_root_bearer_token(&jwt_manager);
  for route in ["/system/runtime", "/system/lifecycle"] {
    for (method, body) in [("GET", ""), ("PUT", r#"{"schema_version":1}"#), ("PATCH", r#"{}"#)] {
      let response =
        application.clone().oneshot(request(method, route, &authorization, Body::from(body))).await.expect("configuration response");
      assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {route}");
    }
  }
}

#[tokio::test]
async fn lifecycle_put_is_strict_and_returns_the_complete_effective_envelope() {
  let (application, jwt_manager, engine, _temporary_directory) = test_app();
  let authorization = root_bearer_token(&jwt_manager);
  let original_generation = engine.configuration_snapshot().generation;

  let duplicate = application
    .clone()
    .oneshot(request(
      "PUT",
      "/system/lifecycle",
      &authorization,
      Body::from(r#"{"schema_version":1,"snapshot_writes_enabled":true,"snapshot_writes_enabled":false}"#),
    ))
    .await
    .expect("duplicate response");
  assert_eq!(duplicate.status(), StatusCode::BAD_REQUEST);
  assert_eq!(engine.configuration_snapshot().generation, original_generation);

  let unknown = application
    .clone()
    .oneshot(request("PUT", "/system/lifecycle", &authorization, Body::from(r#"{"schema_version":1,"snapshop_writes_enabled":false}"#)))
    .await
    .expect("unknown response");
  assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);
  assert_eq!(engine.configuration_snapshot().generation, original_generation);

  let document = r#"{"schema_version":1,"snapshot_retention":{"auto_months":7}}"#;
  let response =
    application.oneshot(request("PUT", "/system/lifecycle", &authorization, Body::from(document))).await.expect("valid response");
  assert_eq!(response.status(), StatusCode::OK);
  let envelope = response_json(response).await;
  assert_eq!(envelope["config"]["snapshot_writes_enabled"], true);
  assert_eq!(envelope["config"]["snapshot_retention"], json!({"auto_months": 7, "manual_months": 0}));
  assert_eq!(envelope["config"]["garbage_collection"]["pending_delete_grace_seconds"], 86_400);
  assert_eq!(envelope["status"]["sources"]["lifecycle.snapshot_retention_auto_months"], "stored_lifecycle_v1");
  assert_eq!(envelope["status"]["sources"]["lifecycle.snapshot_retention_manual_months"], "default");
  assert_eq!(DirectoryOps::new(&engine).read_file_buffered(ConfigurationFamily::Lifecycle.path()).unwrap(), document.as_bytes());
}

#[tokio::test]
async fn configuration_writes_require_an_explicit_json_media_type() {
  let (application, jwt_manager, engine, _temporary_directory) = test_app();
  let authorization = root_bearer_token(&jwt_manager);
  let non_root_authorization = non_root_bearer_token(&jwt_manager);
  let generation = engine.configuration_snapshot().generation;

  let unauthorized = application
    .clone()
    .oneshot(
      Request::builder()
        .method("PUT")
        .uri("/system/lifecycle")
        .header("authorization", non_root_authorization)
        .header("content-type", "text/plain")
        .body(Body::from(r#"{"schema_version":1}"#))
        .unwrap(),
    )
    .await
    .expect("unauthorized media type response");
  assert_eq!(unauthorized.status(), StatusCode::FORBIDDEN);
  assert_eq!(engine.configuration_snapshot().generation, generation);

  for (method, content_type) in [("PUT", None), ("PUT", Some("text/plain")), ("PATCH", Some("text/plain"))] {
    let mut builder = Request::builder().method(method).uri("/system/lifecycle").header("authorization", &authorization);
    if let Some(content_type) = content_type {
      builder = builder.header("content-type", content_type);
    }
    let response =
      application.clone().oneshot(builder.body(Body::from(r#"{"schema_version":1}"#)).unwrap()).await.expect("media type response");
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE, "{method} {content_type:?}");
    assert_eq!(engine.configuration_snapshot().generation, generation);
  }

  let initialize = application
    .clone()
    .oneshot(
      Request::builder()
        .method("PUT")
        .uri("/system/lifecycle")
        .header("authorization", &authorization)
        .header("content-type", "Application/JSON; charset=utf-8")
        .body(Body::from(r#"{"schema_version":1}"#))
        .unwrap(),
    )
    .await
    .expect("initialize response");
  assert_eq!(initialize.status(), StatusCode::OK);
  let response = application
    .oneshot(
      Request::builder()
        .method("PATCH")
        .uri("/system/lifecycle")
        .header("authorization", &authorization)
        .header("content-type", "application/merge-patch+json; charset=utf-8")
        .body(Body::from(r#"{"snapshot_writes_enabled":false}"#))
        .unwrap(),
    )
    .await
    .expect("merge patch media type response");
  assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn lifecycle_patch_preserves_siblings_and_serializes_concurrent_updates() {
  let (application, jwt_manager, engine, _temporary_directory) = test_app();
  let authorization = root_bearer_token(&jwt_manager);
  let initial = r#"{"schema_version":1,"snapshot_writes_enabled":false,"snapshot_retention":{"auto_months":1,"manual_months":2}}"#;
  let response =
    application.clone().oneshot(request("PUT", "/system/lifecycle", &authorization, Body::from(initial))).await.expect("initial response");
  assert_eq!(response.status(), StatusCode::OK);

  let first = application.clone().oneshot(request(
    "PATCH",
    "/system/lifecycle",
    &authorization,
    Body::from(r#"{"snapshot_retention":{"auto_months":10}}"#),
  ));
  let second = application.clone().oneshot(request(
    "PATCH",
    "/system/lifecycle",
    &authorization,
    Body::from(r#"{"snapshot_retention":{"manual_months":20}}"#),
  ));
  let (first, second) = tokio::join!(first, second);
  assert_eq!(first.expect("first patch").status(), StatusCode::OK);
  assert_eq!(second.expect("second patch").status(), StatusCode::OK);

  let response = application.oneshot(request("GET", "/system/lifecycle", &authorization, Body::empty())).await.expect("final response");
  let envelope = response_json(response).await;
  assert_eq!(envelope["config"]["snapshot_writes_enabled"], false);
  assert_eq!(envelope["config"]["snapshot_retention"], json!({"auto_months": 10, "manual_months": 20}));
  let persisted: Value = serde_json::from_slice(
    &DirectoryOps::new(&engine).read_file_buffered(ConfigurationFamily::Lifecycle.path()).expect("persisted lifecycle"),
  )
  .expect("valid persisted lifecycle");
  assert_eq!(persisted["schema_version"], 1);
  assert_eq!(persisted["snapshot_retention"], json!({"auto_months": 10, "manual_months": 20}));
}

#[tokio::test]
async fn lifecycle_patch_rejects_noncanonical_or_invalid_results_without_mutation() {
  let (application, jwt_manager, engine, _temporary_directory) = test_app();
  let authorization = root_bearer_token(&jwt_manager);
  let initial = r#"{"schema_version":1,"snapshot_writes_enabled":false,"snapshot_retention":{"auto_months":1}}"#;
  let response =
    application.clone().oneshot(request("PUT", "/system/lifecycle", &authorization, Body::from(initial))).await.expect("initial response");
  assert_eq!(response.status(), StatusCode::OK);
  let generation = engine.configuration_snapshot().generation;

  for patch in
    [r#"{"snapshot_writes_enabled":true,"snapshot_writes_enabled":false}"#, r#"{} trailing"#, r#"[]"#, r#"{"unknown_policy":true}"#]
  {
    let response = application
      .clone()
      .oneshot(request("PATCH", "/system/lifecycle", &authorization, Body::from(patch)))
      .await
      .expect("invalid patch response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "patch={patch}");
    assert_eq!(engine.configuration_snapshot().generation, generation, "patch={patch}");
    assert_eq!(DirectoryOps::new(&engine).read_file_buffered(ConfigurationFamily::Lifecycle.path()).unwrap(), initial.as_bytes());
  }

  let response = application
    .oneshot(request("PATCH", "/system/lifecycle", &authorization, Body::from(r#"{"schema_version":null}"#)))
    .await
    .expect("schema repair response");
  assert_eq!(response.status(), StatusCode::OK);
  let persisted: Value = serde_json::from_slice(
    &DirectoryOps::new(&engine).read_file_buffered(ConfigurationFamily::Lifecycle.path()).expect("persisted lifecycle"),
  )
  .expect("valid persisted lifecycle");
  assert_eq!(persisted["schema_version"], 1);
}

#[tokio::test]
async fn patch_requires_a_valid_current_or_last_known_good_base() {
  let (application, jwt_manager, engine, _temporary_directory) = test_app();
  let authorization = root_bearer_token(&jwt_manager);
  let generation = engine.configuration_snapshot().generation;
  let response = application
    .oneshot(request("PATCH", "/system/lifecycle", &authorization, Body::from(r#"{"snapshot_writes_enabled":false}"#)))
    .await
    .expect("patch response");
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
  let error = response_json(response).await;
  assert!(error["error"].as_str().unwrap().contains("complete PUT"));
  assert_eq!(engine.configuration_snapshot().generation, generation);
}

#[tokio::test]
async fn startup_bound_put_reports_active_and_desired_values_without_persisting_overrides() {
  let (application, jwt_manager, engine, _temporary_directory) = test_app();
  let authorization = root_bearer_token(&jwt_manager);
  let active_hard_limit = engine.configuration_snapshot().resolved_unsigned("memory.hard_limit_bytes").unwrap();
  let desired_hard_limit =
    if active_hard_limit == 4_u64 * 1024 * 1024 * 1024 { 5_u64 * 1024 * 1024 * 1024 } else { 4_u64 * 1024 * 1024 * 1024 };
  let document = format!(r#"{{"schema_version":1,"memory":{{"hard_limit_bytes":{desired_hard_limit}}}}}"#);

  let response = application
    .oneshot(request("PUT", "/system/runtime", &authorization, Body::from(document.clone())))
    .await
    .expect("runtime put response");
  assert_eq!(response.status(), StatusCode::OK);
  let envelope = response_json(response).await;
  assert_eq!(envelope["config"]["memory"]["hard_limit_bytes"], active_hard_limit);
  assert_eq!(envelope["status"]["desired_config"]["memory"]["hard_limit_bytes"], desired_hard_limit);
  assert_eq!(envelope["status"]["pending_restart"]["memory.hard_limit_bytes"], desired_hard_limit);
  assert_eq!(DirectoryOps::new(&engine).read_file_buffered(ConfigurationFamily::Runtime.path()).unwrap(), document.as_bytes());
}

#[tokio::test]
async fn configuration_documents_cannot_be_accessed_through_generic_file_routes() {
  let (application, jwt_manager, _engine, _temporary_directory) = test_app();
  let authorization = root_bearer_token(&jwt_manager);
  for path in ["/.aeordb-config/runtime.json", "/.aeordb-config/lifecycle.json"] {
    let uri = format!("/files{path}");
    for (method, body) in [("GET", ""), ("HEAD", ""), ("PUT", r#"{"schema_version":1}"#), ("PATCH", r#"{}"#), ("DELETE", "")] {
      let response =
        application.clone().oneshot(request(method, &uri, &authorization, Body::from(body))).await.expect("generic route response");
      assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {uri}");
    }
  }

  let response = application
    .oneshot(request(
      "POST",
      "/blobs/commit",
      &authorization,
      Body::from(r#"{"files":[{"path":"/.aeordb-config/runtime.json","chunks":[],"content_type":"application/json"}]}"#),
    ))
    .await
    .expect("blob commit response");
  assert!(!response.status().is_success());
}

#[tokio::test]
async fn configuration_routes_reject_documents_above_the_frozen_bound() {
  let (application, jwt_manager, _engine, _temporary_directory) = test_app();
  let authorization = root_bearer_token(&jwt_manager);
  let oversized = vec![b' '; MAX_CONFIG_DOCUMENT_BYTES + 1];
  let response =
    application.oneshot(request("PUT", "/system/runtime", &authorization, Body::from(oversized))).await.expect("oversized response");
  assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}
