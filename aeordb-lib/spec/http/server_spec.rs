use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::StorageEngine;
use aeordb::server::{create_app_with_jwt, create_temp_engine_for_tests};
use aeordb::storage::RedbStorage;

/// Create a fresh in-memory app with a shared JwtManager.
fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<RedbStorage>, Arc<StorageEngine>, tempfile::TempDir) {
  let storage = Arc::new(RedbStorage::new_in_memory().expect("in-memory storage"));
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let app = create_app_with_jwt(storage.clone(), jwt_manager.clone(), engine.clone());
  (app, jwt_manager, storage, engine, temp_dir)
}

/// Helper: build app from shared storage + jwt_manager.
fn rebuild_app(storage: &Arc<RedbStorage>, jwt_manager: &Arc<JwtManager>, engine: &Arc<StorageEngine>) -> axum::Router {
  create_app_with_jwt(storage.clone(), jwt_manager.clone(), engine.clone())
}

/// Create an admin Bearer token value (including "Bearer " prefix).
fn bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: "test-admin".to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + DEFAULT_EXPIRY_SECONDS,
    roles: vec!["admin".to_string()],
    scope: None,
    permissions: None,
  };
  let token = jwt_manager.create_token(&claims).expect("create token");
  format!("Bearer {}", token)
}

/// Helper to collect the response body into bytes.
async fn body_bytes(body: Body) -> Vec<u8> {
  body.collect().await.unwrap().to_bytes().to_vec()
}

/// Helper to collect the response body into a JSON value.
async fn body_json(body: Body) -> serde_json::Value {
  let bytes = body_bytes(body).await;
  serde_json::from_slice(&bytes).expect("valid JSON response body")
}

// ---------------------------------------------------------------------------
// Health check
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_health_check_returns_200() {
  let (app, _, _, _, _temp_dir) = test_app();
  let request = Request::builder()
    .uri("/admin/health")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["status"], "ok");
}

// ---------------------------------------------------------------------------
// Create document
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_document_returns_201_with_id() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);
  let request = Request::builder()
    .method("POST")
    .uri("/mydb/users")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("hello world"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  assert!(json["document_id"].is_string());
  // Validate the document_id is a valid UUID
  let id_str = json["document_id"].as_str().unwrap();
  uuid::Uuid::parse_str(id_str).expect("document_id should be a valid UUID");
}

#[tokio::test]
async fn test_create_document_returns_body_with_mandatory_fields() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);
  let request = Request::builder()
    .method("POST")
    .uri("/mydb/users")
    .header("content-type", "application/octet-stream")
    .header("authorization", &auth)
    .body(Body::from("some data"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  assert!(json["document_id"].is_string(), "missing document_id");
  assert!(json["created_at"].is_string(), "missing created_at");
  assert!(json["updated_at"].is_string(), "missing updated_at");
}

#[tokio::test]
async fn test_create_document_preserves_content_type() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let app = rebuild_app(&storage, &jwt_manager, &engine);
  let request = Request::builder()
    .method("POST")
    .uri("/mydb/users")
    .header("content-type", "image/png")
    .header("authorization", &auth)
    .body(Body::from(vec![0x89, 0x50, 0x4e, 0x47]))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  let document_id = json["document_id"].as_str().unwrap();

  // Now fetch it and confirm the content-type was preserved
  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let get_request = Request::builder()
    .uri(format!("/mydb/users/{}", document_id))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let get_response = app2.oneshot(get_request).await.unwrap();
  assert_eq!(get_response.status(), StatusCode::OK);

  let content_type = get_response
    .headers()
    .get("content-type")
    .expect("content-type header should be present")
    .to_str()
    .unwrap();
  assert_eq!(content_type, "image/png");
}

#[tokio::test]
async fn test_create_with_json_content_type() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let app = rebuild_app(&storage, &jwt_manager, &engine);
  let json_body = r#"{"name":"alice","age":30}"#;
  let request = Request::builder()
    .method("POST")
    .uri("/mydb/users")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(json_body))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  let document_id = json["document_id"].as_str().unwrap();

  // Fetch back and verify raw data integrity
  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let get_request = Request::builder()
    .uri(format!("/mydb/users/{}", document_id))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let get_response = app2.oneshot(get_request).await.unwrap();
  assert_eq!(get_response.status(), StatusCode::OK);

  let ct = get_response.headers().get("content-type").unwrap().to_str().unwrap();
  assert_eq!(ct, "application/json");

  let raw_body = body_bytes(get_response.into_body()).await;
  assert_eq!(String::from_utf8(raw_body).unwrap(), json_body);
}

#[tokio::test]
async fn test_create_with_binary_content_type() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let app = rebuild_app(&storage, &jwt_manager, &engine);
  let binary_data: Vec<u8> = (0u8..=255).collect();
  let request = Request::builder()
    .method("POST")
    .uri("/mydb/blobs")
    .header("content-type", "application/octet-stream")
    .header("authorization", &auth)
    .body(Body::from(binary_data.clone()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  let document_id = json["document_id"].as_str().unwrap();

  // Fetch back and verify binary data roundtrip
  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let get_request = Request::builder()
    .uri(format!("/mydb/blobs/{}", document_id))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let get_response = app2.oneshot(get_request).await.unwrap();
  let raw = body_bytes(get_response.into_body()).await;
  assert_eq!(raw, binary_data);
}

// ---------------------------------------------------------------------------
// Get document
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_get_document_returns_200_with_data() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let app = rebuild_app(&storage, &jwt_manager, &engine);
  // Create a document first
  let request = Request::builder()
    .method("POST")
    .uri("/mydb/users")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("hello"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let json = body_json(response.into_body()).await;
  let document_id = json["document_id"].as_str().unwrap().to_string();

  // Get it
  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let get_request = Request::builder()
    .uri(format!("/mydb/users/{}", document_id))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let get_response = app2.oneshot(get_request).await.unwrap();
  assert_eq!(get_response.status(), StatusCode::OK);

  let body = body_bytes(get_response.into_body()).await;
  assert_eq!(body, b"hello");
}

#[tokio::test]
async fn test_get_document_returns_correct_content_type() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let app = rebuild_app(&storage, &jwt_manager, &engine);
  let request = Request::builder()
    .method("POST")
    .uri("/mydb/users")
    .header("content-type", "application/xml")
    .header("authorization", &auth)
    .body(Body::from("<root/>"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let json = body_json(response.into_body()).await;
  let document_id = json["document_id"].as_str().unwrap().to_string();

  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let get_request = Request::builder()
    .uri(format!("/mydb/users/{}", document_id))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let get_response = app2.oneshot(get_request).await.unwrap();
  assert_eq!(get_response.status(), StatusCode::OK);

  let content_type = get_response
    .headers()
    .get("content-type")
    .unwrap()
    .to_str()
    .unwrap();
  assert_eq!(content_type, "application/xml");

  // Also verify custom metadata headers
  assert!(get_response.headers().get("X-Document-Id").is_some());
  assert!(get_response.headers().get("X-Created-At").is_some());
  assert!(get_response.headers().get("X-Updated-At").is_some());
}

#[tokio::test]
async fn test_get_document_returns_404_for_missing() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);
  let fake_id = uuid::Uuid::new_v4();
  let request = Request::builder()
    .uri(format!("/mydb/users/{}", fake_id))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Update document
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_update_document_returns_200() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let app = rebuild_app(&storage, &jwt_manager, &engine);
  // Create
  let request = Request::builder()
    .method("POST")
    .uri("/mydb/users")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("original"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let json = body_json(response.into_body()).await;
  let document_id = json["document_id"].as_str().unwrap().to_string();

  // Update
  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let update_request = Request::builder()
    .method("PATCH")
    .uri(format!("/mydb/users/{}", document_id))
    .header("authorization", &auth)
    .body(Body::from("updated"))
    .unwrap();

  let update_response = app2.oneshot(update_request).await.unwrap();
  assert_eq!(update_response.status(), StatusCode::OK);

  let update_json = body_json(update_response.into_body()).await;
  assert_eq!(update_json["document_id"].as_str().unwrap(), document_id);
  assert!(update_json["created_at"].is_string());
  assert!(update_json["updated_at"].is_string());

  // Verify data was actually updated
  let app3 = rebuild_app(&storage, &jwt_manager, &engine);
  let get_request = Request::builder()
    .uri(format!("/mydb/users/{}", document_id))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let get_response = app3.oneshot(get_request).await.unwrap();
  let body = body_bytes(get_response.into_body()).await;
  assert_eq!(body, b"updated");
}

#[tokio::test]
async fn test_update_document_returns_404_for_missing() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);
  let fake_id = uuid::Uuid::new_v4();
  let request = Request::builder()
    .method("PATCH")
    .uri(format!("/mydb/users/{}", fake_id))
    .header("authorization", &auth)
    .body(Body::from("data"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Delete document
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_delete_document_returns_200() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let app = rebuild_app(&storage, &jwt_manager, &engine);
  // Create
  let request = Request::builder()
    .method("POST")
    .uri("/mydb/users")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("deleteme"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let json = body_json(response.into_body()).await;
  let document_id = json["document_id"].as_str().unwrap().to_string();

  // Delete
  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let delete_request = Request::builder()
    .method("DELETE")
    .uri(format!("/mydb/users/{}", document_id))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let delete_response = app2.oneshot(delete_request).await.unwrap();
  assert_eq!(delete_response.status(), StatusCode::OK);

  let delete_json = body_json(delete_response.into_body()).await;
  assert_eq!(delete_json["deleted"], true);
  assert_eq!(delete_json["document_id"].as_str().unwrap(), document_id);

  // Verify the document is actually gone
  let app3 = rebuild_app(&storage, &jwt_manager, &engine);
  let get_request = Request::builder()
    .uri(format!("/mydb/users/{}", document_id))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let get_response = app3.oneshot(get_request).await.unwrap();
  assert_eq!(get_response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_document_returns_404_for_already_deleted_or_missing() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Completely missing document
  let app = rebuild_app(&storage, &jwt_manager, &engine);
  let fake_id = uuid::Uuid::new_v4();
  let request = Request::builder()
    .method("DELETE")
    .uri(format!("/mydb/users/{}", fake_id))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);

  // Create then delete, then try to delete again
  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let create_request = Request::builder()
    .method("POST")
    .uri("/mydb/users")
    .header("authorization", &auth)
    .body(Body::from("temp"))
    .unwrap();

  let create_response = app2.oneshot(create_request).await.unwrap();
  let create_json = body_json(create_response.into_body()).await;
  let document_id = create_json["document_id"].as_str().unwrap().to_string();

  // First delete succeeds
  let app3 = rebuild_app(&storage, &jwt_manager, &engine);
  let delete_request = Request::builder()
    .method("DELETE")
    .uri(format!("/mydb/users/{}", document_id))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let delete_response = app3.oneshot(delete_request).await.unwrap();
  assert_eq!(delete_response.status(), StatusCode::OK);

  // Second delete should return 404 (already deleted)
  let app4 = rebuild_app(&storage, &jwt_manager, &engine);
  let delete_request2 = Request::builder()
    .method("DELETE")
    .uri(format!("/mydb/users/{}", document_id))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let delete_response2 = app4.oneshot(delete_request2).await.unwrap();
  assert_eq!(
    delete_response2.status(),
    StatusCode::NOT_FOUND,
    "Deleting an already-deleted document should return 404"
  );
}

// ---------------------------------------------------------------------------
// List documents
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_list_documents_returns_200_with_array() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let app = rebuild_app(&storage, &jwt_manager, &engine);
  // Create two documents
  let request1 = Request::builder()
    .method("POST")
    .uri("/mydb/users")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("alice"))
    .unwrap();
  app.oneshot(request1).await.unwrap();

  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let request2 = Request::builder()
    .method("POST")
    .uri("/mydb/users")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("bob"))
    .unwrap();
  app2.oneshot(request2).await.unwrap();

  // List
  let app3 = rebuild_app(&storage, &jwt_manager, &engine);
  let list_request = Request::builder()
    .uri("/mydb/users")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let list_response = app3.oneshot(list_request).await.unwrap();
  assert_eq!(list_response.status(), StatusCode::OK);

  let json = body_json(list_response.into_body()).await;
  let array = json.as_array().expect("response should be an array");
  assert_eq!(array.len(), 2);

  // Each item should have metadata fields
  for item in array {
    assert!(item["document_id"].is_string());
    assert!(item["created_at"].is_string());
    assert!(item["updated_at"].is_string());
  }
}

#[tokio::test]
async fn test_list_documents_empty_table_returns_empty_array() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);
  let request = Request::builder()
    .uri("/mydb/emptytable")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let array = json.as_array().expect("response should be an array");
  assert!(array.is_empty());
}

// ---------------------------------------------------------------------------
// Route matching / 404
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_nonexistent_route_returns_404() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);
  let request = Request::builder()
    .uri("/this/does/not/exist/at/all")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_empty_table_name_handling() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Axum won't match "/{database}/{table}" if table is empty, so
  // a URI like "/mydb/" should 404 (no route match).
  let request = Request::builder()
    .method("POST")
    .uri("/mydb/")
    .header("authorization", &auth)
    .body(Body::from("test"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  // This should be 404 because "/{database}/{table}" requires a non-empty table segment
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_get_document_with_invalid_uuid_returns_400() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);
  let request = Request::builder()
    .uri("/mydb/users/not-a-uuid")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);

  let json = body_json(response.into_body()).await;
  assert!(json["error"].as_str().unwrap().contains("Invalid document ID"));
}

#[tokio::test]
async fn test_update_document_with_invalid_uuid_returns_400() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);
  let request = Request::builder()
    .method("PATCH")
    .uri("/mydb/users/garbage")
    .header("authorization", &auth)
    .body(Body::from("data"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_delete_document_with_invalid_uuid_returns_400() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);
  let request = Request::builder()
    .method("DELETE")
    .uri("/mydb/users/not-valid")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_create_document_without_content_type() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let app = rebuild_app(&storage, &jwt_manager, &engine);
  // No content-type header
  let request = Request::builder()
    .method("POST")
    .uri("/mydb/users")
    .header("authorization", &auth)
    .body(Body::from("raw bytes"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  let document_id = json["document_id"].as_str().unwrap();

  // Fetch and verify no content-type is set
  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let get_request = Request::builder()
    .uri(format!("/mydb/users/{}", document_id))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let get_response = app2.oneshot(get_request).await.unwrap();
  assert_eq!(get_response.status(), StatusCode::OK);
  // When no content_type was stored, we shouldn't set one
  assert!(
    get_response.headers().get("content-type").is_none(),
    "content-type should not be set when none was stored"
  );
}

#[tokio::test]
async fn test_create_empty_body() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let app = rebuild_app(&storage, &jwt_manager, &engine);
  let request = Request::builder()
    .method("POST")
    .uri("/mydb/users")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  let document_id = json["document_id"].as_str().unwrap();

  // Fetch and verify empty body
  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let get_request = Request::builder()
    .uri(format!("/mydb/users/{}", document_id))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let get_response = app2.oneshot(get_request).await.unwrap();
  let body = body_bytes(get_response.into_body()).await;
  assert!(body.is_empty());
}

// ---------------------------------------------------------------------------
// FIX 8: Table/database name validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_invalid_table_name_rejected() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/mydb/inv@lid!")
    .header("authorization", &auth)
    .body(Body::from("data"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);

  let json = body_json(response.into_body()).await;
  assert!(
    json["error"].as_str().unwrap().contains("Invalid table name"),
    "Error should mention invalid table name, got: {}",
    json["error"]
  );
}

#[tokio::test]
async fn test_path_traversal_rejected() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/mydb/..%2F..%2Fetc")
    .header("authorization", &auth)
    .body(Body::from("data"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_system_table_prefix_rejected() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/mydb/_system")
    .header("authorization", &auth)
    .body(Body::from("data"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);

  let json = body_json(response.into_body()).await;
  assert!(
    json["error"].as_str().unwrap().contains("reserved"),
    "Error should mention reserved prefix, got: {}",
    json["error"]
  );
}

#[tokio::test]
async fn test_system_database_prefix_rejected() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/_internal/users")
    .header("authorization", &auth)
    .body(Body::from("data"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);

  let json = body_json(response.into_body()).await;
  assert!(
    json["error"].as_str().unwrap().contains("reserved"),
    "Error should mention reserved prefix, got: {}",
    json["error"]
  );
}

// ---------------------------------------------------------------------------
// FIX 12: Concurrent requests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_concurrent_requests_handled() {
  let storage = Arc::new(RedbStorage::new_in_memory().expect("in-memory storage"));
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let auth = bearer_token(&jwt_manager);

  // Create a document first
  let app = create_app_with_jwt(storage.clone(), jwt_manager.clone(), engine.clone());
  let request = Request::builder()
    .method("POST")
    .uri("/mydb/concurrent")
    .header("authorization", &auth)
    .header("content-type", "text/plain")
    .body(Body::from("seed"))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Spawn multiple concurrent create requests
  let mut handles = Vec::new();
  for i in 0..5 {
    let storage_clone = storage.clone();
    let jwt_clone = jwt_manager.clone();
    let engine_clone = engine.clone();
    let auth_clone = auth.clone();
    handles.push(tokio::spawn(async move {
      let app = create_app_with_jwt(storage_clone, jwt_clone, engine_clone);
      let request = Request::builder()
        .method("POST")
        .uri("/mydb/concurrent")
        .header("authorization", &auth_clone)
        .header("content-type", "text/plain")
        .body(Body::from(format!("concurrent-{}", i)))
        .unwrap();
      let response = app.oneshot(request).await.unwrap();
      response.status()
    }));
  }

  let mut success_count = 0;
  for handle in handles {
    let status = handle.await.unwrap();
    if status == StatusCode::CREATED {
      success_count += 1;
    }
  }

  assert_eq!(success_count, 5, "All concurrent requests should succeed");

  // Verify all documents exist
  let app_final = create_app_with_jwt(storage.clone(), jwt_manager.clone(), engine.clone());
  let list_request = Request::builder()
    .uri("/mydb/concurrent")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let list_response = app_final.oneshot(list_request).await.unwrap();
  let list_json = body_json(list_response.into_body()).await;
  let array = list_json.as_array().unwrap();
  assert_eq!(array.len(), 6, "Should have seed + 5 concurrent docs");
}
