//! HTTP-level tests for the `PATCH /files/{*path}` merge-patch mode
//! (RFC 7396 + optional `?depth=N` bound).
//!
//! Companion to `rename_http_spec.rs`, which covers the legacy rename
//! mode of the same endpoint. The dispatcher in `engine_routes.rs`
//! picks between them by `Content-Type` —
//! `application/merge-patch+json` lands here.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine};
use aeordb::server::{create_app_with_jwt_and_engine, create_temp_engine_for_tests};

fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let app = create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone());
  (app, jwt_manager, engine, temp_dir)
}

fn rebuild_app(jwt_manager: &Arc<JwtManager>, engine: &Arc<StorageEngine>) -> axum::Router {
  create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone())
}

fn bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: uuid::Uuid::nil().to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + DEFAULT_EXPIRY_SECONDS,
    scope: None,
    permissions: None,
    key_id: None,
  };
  let token = jwt_manager.create_token(&claims).expect("create token");
  format!("Bearer {}", token)
}

async fn body_json(body: Body) -> serde_json::Value {
  let bytes = body.collect().await.unwrap().to_bytes().to_vec();
  serde_json::from_slice(&bytes).expect("valid JSON")
}

fn seed_json(engine: &StorageEngine, path: &str, value: serde_json::Value) {
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(engine);
  let bytes = serde_json::to_vec(&value).unwrap();
  ops.store_file_buffered(&ctx, path, &bytes, Some("application/json")).unwrap();
}

fn read_json(engine: &StorageEngine, path: &str) -> serde_json::Value {
  let ops = DirectoryOps::new(engine);
  let bytes = ops.read_file_buffered(path).expect("file must exist");
  serde_json::from_slice(&bytes).expect("stored content must be JSON")
}

fn patch_req(uri: &str, auth: &str, body: serde_json::Value) -> Request<Body> {
  Request::builder()
    .method("PATCH")
    .uri(uri)
    .header("content-type", "application/merge-patch+json")
    .header("authorization", auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap()
}

// ─────────────────────────────────────────────────────────────────────────
// Happy path: RFC 7396 default behavior
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn merge_patch_default_recursive() {
  let (_, jwt, engine, _tmp) = test_app();
  let auth = bearer_token(&jwt);
  seed_json(&engine, "/doc.json", serde_json::json!({
    "name": "Alice",
    "prefs": {"theme": "dark", "lang": "en"},
    "tags": ["a", "b"],
  }));

  let resp = rebuild_app(&jwt, &engine)
    .oneshot(patch_req("/files/doc.json", &auth, serde_json::json!({
      "prefs": {"theme": "light"},
      "email": "a@x",
    })))
    .await
    .unwrap();
  assert_eq!(resp.status(), StatusCode::OK);

  let stored = read_json(&engine, "/doc.json");
  assert_eq!(stored, serde_json::json!({
    "name": "Alice",
    "prefs": {"theme": "light", "lang": "en"},
    "tags": ["a", "b"],
    "email": "a@x",
  }));
}

#[tokio::test]
async fn merge_patch_null_deletes_keys() {
  let (_, jwt, engine, _tmp) = test_app();
  let auth = bearer_token(&jwt);
  seed_json(&engine, "/doc.json", serde_json::json!({"a": 1, "b": 2, "c": 3}));

  let resp = rebuild_app(&jwt, &engine)
    .oneshot(patch_req("/files/doc.json", &auth, serde_json::json!({"b": null, "d": 4})))
    .await
    .unwrap();
  assert_eq!(resp.status(), StatusCode::OK);

  let stored = read_json(&engine, "/doc.json");
  assert_eq!(stored, serde_json::json!({"a": 1, "c": 3, "d": 4}));
}

#[tokio::test]
async fn merge_patch_arrays_replace_wholesale() {
  let (_, jwt, engine, _tmp) = test_app();
  let auth = bearer_token(&jwt);
  seed_json(&engine, "/doc.json", serde_json::json!({"items": [1, 2, 3, 4, 5]}));

  let resp = rebuild_app(&jwt, &engine)
    .oneshot(patch_req("/files/doc.json", &auth, serde_json::json!({"items": [10]})))
    .await
    .unwrap();
  assert_eq!(resp.status(), StatusCode::OK);

  // RFC 7396: arrays replace, no concat or merge by index.
  assert_eq!(read_json(&engine, "/doc.json"), serde_json::json!({"items": [10]}));
}

// ─────────────────────────────────────────────────────────────────────────
// Depth bound
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn merge_patch_depth_1_replaces_subtrees() {
  let (_, jwt, engine, _tmp) = test_app();
  let auth = bearer_token(&jwt);
  seed_json(&engine, "/doc.json", serde_json::json!({
    "user": {"name": "Alice", "prefs": {"theme": "dark"}},
    "session": "abc",
  }));

  let resp = rebuild_app(&jwt, &engine)
    .oneshot(patch_req("/files/doc.json?depth=1", &auth, serde_json::json!({
      "user": {"prefs": {"theme": "light"}},
    })))
    .await
    .unwrap();
  assert_eq!(resp.status(), StatusCode::OK);

  // depth=1 = top-level keys merge; object values replace wholesale.
  // `user` is replaced (name lost), `session` is preserved.
  assert_eq!(read_json(&engine, "/doc.json"), serde_json::json!({
    "user": {"prefs": {"theme": "light"}},
    "session": "abc",
  }));
}

#[tokio::test]
async fn merge_patch_depth_0_is_full_replace() {
  let (_, jwt, engine, _tmp) = test_app();
  let auth = bearer_token(&jwt);
  seed_json(&engine, "/doc.json", serde_json::json!({"a": 1, "b": 2, "nested": {"x": 1}}));

  let resp = rebuild_app(&jwt, &engine)
    .oneshot(patch_req("/files/doc.json?depth=0", &auth, serde_json::json!({"c": 3})))
    .await
    .unwrap();
  assert_eq!(resp.status(), StatusCode::OK);

  // depth=0 = wholesale replace. Original a/b/nested gone.
  assert_eq!(read_json(&engine, "/doc.json"), serde_json::json!({"c": 3}));
}

#[tokio::test]
async fn merge_patch_negative_depth_preserves_subtrees() {
  let (_, jwt, engine, _tmp) = test_app();
  let auth = bearer_token(&jwt);
  seed_json(&engine, "/doc.json", serde_json::json!({
    "user": {"name": "Alice", "prefs": {"theme": "dark"}},
    "scalar": "old",
  }));

  // depth=-1: top-level scalars merge as usual, but object values
  // (target.user) are PRESERVED — the patch's deeper object is ignored.
  let resp = rebuild_app(&jwt, &engine)
    .oneshot(patch_req("/files/doc.json?depth=-1", &auth, serde_json::json!({
      "user": {"prefs": {"theme": "light"}},
      "scalar": "new",
    })))
    .await
    .unwrap();
  assert_eq!(resp.status(), StatusCode::OK);

  assert_eq!(read_json(&engine, "/doc.json"), serde_json::json!({
    "user": {"name": "Alice", "prefs": {"theme": "dark"}},  // unchanged
    "scalar": "new",
  }));
}

#[tokio::test]
async fn merge_patch_negative_depth_does_not_create_new_subtrees() {
  let (_, jwt, engine, _tmp) = test_app();
  let auth = bearer_token(&jwt);
  seed_json(&engine, "/doc.json", serde_json::json!({"existing": "v"}));

  // Patch tries to add a NEW nested object. With preserve-beyond policy
  // at depth=-1, we don't touch the depths — so this isn't created.
  let resp = rebuild_app(&jwt, &engine)
    .oneshot(patch_req("/files/doc.json?depth=-1", &auth, serde_json::json!({
      "new_nested": {"key": "value"},
      "scalar_ok": 42,
    })))
    .await
    .unwrap();
  assert_eq!(resp.status(), StatusCode::OK);

  assert_eq!(read_json(&engine, "/doc.json"), serde_json::json!({
    "existing": "v",
    "scalar_ok": 42,
  }));
}

#[tokio::test]
async fn merge_patch_negative_depth_null_still_deletes_at_top_level() {
  let (_, jwt, engine, _tmp) = test_app();
  let auth = bearer_token(&jwt);
  seed_json(&engine, "/doc.json", serde_json::json!({
    "keep_object": {"deep": "untouched"},
    "delete_me": "x",
  }));

  let resp = rebuild_app(&jwt, &engine)
    .oneshot(patch_req("/files/doc.json?depth=-1", &auth, serde_json::json!({
      "keep_object": {"deep": "ignored"},
      "delete_me": null,
    })))
    .await
    .unwrap();
  assert_eq!(resp.status(), StatusCode::OK);

  assert_eq!(read_json(&engine, "/doc.json"), serde_json::json!({
    "keep_object": {"deep": "untouched"},
  }));
}

// ─────────────────────────────────────────────────────────────────────────
// File lifecycle
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn merge_patch_creates_when_file_absent() {
  let (_, jwt, engine, _tmp) = test_app();
  let auth = bearer_token(&jwt);

  let resp = rebuild_app(&jwt, &engine)
    .oneshot(patch_req("/files/new.json", &auth, serde_json::json!({"created": true})))
    .await
    .unwrap();
  assert_eq!(resp.status(), StatusCode::CREATED);

  assert_eq!(read_json(&engine, "/new.json"), serde_json::json!({"created": true}));
}

#[tokio::test]
async fn merge_patch_existing_returns_200() {
  let (_, jwt, engine, _tmp) = test_app();
  let auth = bearer_token(&jwt);
  seed_json(&engine, "/doc.json", serde_json::json!({"a": 1}));

  let resp = rebuild_app(&jwt, &engine)
    .oneshot(patch_req("/files/doc.json", &auth, serde_json::json!({"b": 2})))
    .await
    .unwrap();
  assert_eq!(resp.status(), StatusCode::OK);
}

// ─────────────────────────────────────────────────────────────────────────
// Validation failures
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn merge_patch_invalid_json_body_returns_415() {
  let (_, jwt, engine, _tmp) = test_app();
  let auth = bearer_token(&jwt);
  seed_json(&engine, "/doc.json", serde_json::json!({"a": 1}));

  let req = Request::builder()
    .method("PATCH")
    .uri("/files/doc.json")
    .header("content-type", "application/merge-patch+json")
    .header("authorization", &auth)
    .body(Body::from(b"this is not json".as_ref()))
    .unwrap();
  let resp = rebuild_app(&jwt, &engine).oneshot(req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

  // File untouched.
  assert_eq!(read_json(&engine, "/doc.json"), serde_json::json!({"a": 1}));
}

#[tokio::test]
async fn merge_patch_stored_not_json_returns_415() {
  let (_, jwt, engine, _tmp) = test_app();
  let auth = bearer_token(&jwt);

  // Seed the file as raw text rather than JSON.
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&ctx, "/blob.bin", b"this is binary garbage", Some("application/octet-stream")).unwrap();

  let resp = rebuild_app(&jwt, &engine)
    .oneshot(patch_req("/files/blob.bin", &auth, serde_json::json!({"key": "value"})))
    .await
    .unwrap();
  assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn merge_patch_system_path_returns_404() {
  let (_, jwt, engine, _tmp) = test_app();
  let auth = bearer_token(&jwt);

  let resp = rebuild_app(&jwt, &engine)
    .oneshot(patch_req("/files/.aeordb-system/foo", &auth, serde_json::json!({})))
    .await
    .unwrap();
  assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ─────────────────────────────────────────────────────────────────────────
// Dispatcher: content-type discriminates merge vs rename
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn patch_with_application_json_still_renames() {
  let (_, jwt, engine, _tmp) = test_app();
  let auth = bearer_token(&jwt);
  seed_json(&engine, "/from.json", serde_json::json!({"k": "v"}));

  let req = Request::builder()
    .method("PATCH")
    .uri("/files/from.json")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&serde_json::json!({"to": "/to.json"})).unwrap()))
    .unwrap();
  let resp = rebuild_app(&jwt, &engine).oneshot(req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::OK);

  let payload = body_json(resp.into_body()).await;
  assert_eq!(payload["from"], "/from.json");
  assert_eq!(payload["to"], "/to.json");

  // Old path is gone; new path has the data.
  let ops = DirectoryOps::new(&engine);
  assert!(ops.read_file_buffered("/from.json").is_err());
  assert_eq!(read_json(&engine, "/to.json"), serde_json::json!({"k": "v"}));
}

#[tokio::test]
async fn patch_with_merge_content_type_does_not_rename() {
  // Sanity: payload {"to": "..."} sent with merge-patch+json content-type
  // must NOT trigger a rename — it should be merged into the JSON file.
  let (_, jwt, engine, _tmp) = test_app();
  let auth = bearer_token(&jwt);
  seed_json(&engine, "/doc.json", serde_json::json!({"existing": true}));

  let resp = rebuild_app(&jwt, &engine)
    .oneshot(patch_req("/files/doc.json", &auth, serde_json::json!({"to": "/should-not-be-a-rename"})))
    .await
    .unwrap();
  assert_eq!(resp.status(), StatusCode::OK);

  let stored = read_json(&engine, "/doc.json");
  assert_eq!(stored, serde_json::json!({
    "existing": true,
    "to": "/should-not-be-a-rename",
  }));

  // Confirm rename did NOT happen.
  let ops = DirectoryOps::new(&engine);
  assert!(ops.read_file_buffered("/should-not-be-a-rename").is_err());
}
