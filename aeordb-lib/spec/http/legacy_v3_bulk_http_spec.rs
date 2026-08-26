use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{DEFAULT_EXPIRY_SECONDS, JwtManager, TokenClaims};
use aeordb::engine::permissions::{PathPermissions, PermissionLink};
use aeordb::engine::version_manager::VersionManager;
use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine};
use aeordb::server::{create_app_with_jwt_and_engine, create_temp_engine_for_tests};

struct TestContext {
  jwt_manager: Arc<JwtManager>,
  engine: Arc<StorageEngine>,
  _directory: tempfile::TempDir,
}

impl TestContext {
  fn new() -> Self {
    let jwt_manager = Arc::new(JwtManager::generate());
    let (engine, directory) = create_temp_engine_for_tests();
    Self { jwt_manager, engine, _directory: directory }
  }

  fn app(&self) -> axum::Router {
    create_app_with_jwt_and_engine(Arc::clone(&self.jwt_manager), Arc::clone(&self.engine))
  }

  fn token_for(&self, subject: impl Into<String>) -> String {
    let now = chrono::Utc::now().timestamp();
    let claims = TokenClaims {
      sub: subject.into(),
      iss: "aeordb".to_string(),
      iat: now,
      exp: now + DEFAULT_EXPIRY_SECONDS,
      scope: None,
      permissions: None,
      key_id: None,
    };
    format!("Bearer {}", self.jwt_manager.create_token(&claims).unwrap())
  }

  fn root_token(&self) -> String {
    self.token_for(uuid::Uuid::nil().to_string())
  }

  fn store(&self, path: &str, bytes: &[u8], content_type: &str) {
    DirectoryOps::new(&self.engine).store_file_buffered(&RequestContext::system(), path, bytes, Some(content_type)).unwrap();
  }

  async fn post(&self, uri: &str, token: &str, body: serde_json::Value) -> HttpResult {
    let response = self
      .app()
      .oneshot(
        Request::builder()
          .method(Method::POST)
          .uri(uri)
          .header("authorization", token)
          .header("content-type", "application/json")
          .body(Body::from(body.to_string()))
          .unwrap(),
      )
      .await
      .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes().to_vec();
    HttpResult { status, headers, bytes }
  }
}

struct HttpResult {
  status: StatusCode,
  headers: HeaderMap,
  bytes: Vec<u8>,
}

impl HttpResult {
  fn json(&self) -> serde_json::Value {
    serde_json::from_slice(&self.bytes).unwrap()
  }
}

fn assert_root_headers(headers: &HeaderMap, root_hash: &[u8], state: &str) {
  assert_eq!(headers["x-aeordb-root-hash"], hex::encode(root_hash));
  assert_eq!(headers["x-aeordb-root-state"], state);
  assert_eq!(headers["x-aeordb-root-expires-at"], "");
}

fn zip_files(bytes: Vec<u8>) -> HashMap<String, Vec<u8>> {
  let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).expect("valid ZIP");
  let mut files = HashMap::new();
  for index in 0..archive.len() {
    let mut entry = archive.by_index(index).unwrap();
    if entry.is_dir() {
      continue;
    }
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes).unwrap();
    files.insert(entry.name().to_string(), bytes);
  }
  files
}

#[tokio::test]
async fn current_fetch_and_download_preserve_legacy_shapes_with_one_live_root_header() {
  let context = TestContext::new();
  let token = context.root_token();
  context.store("/docs/value.txt", b"current value", "text/plain");
  let root = context.engine.head_hash().unwrap();

  let whole = context.post("/files/fetch", &token, serde_json::json!({ "paths": ["/docs/value.txt"] })).await;
  assert_eq!(whole.status, StatusCode::OK);
  assert_root_headers(&whole.headers, &root, "live");
  let whole_json = whole.json();
  assert_eq!(whole_json["/docs/value.txt"]["content"], "current value");
  assert!(whole_json.get("root").is_none(), "legacy keyed fetch body gained a root envelope");

  let ranges = context
    .post(
      "/files/fetch",
      &token,
      serde_json::json!({
        "items": [{ "id": "bytes", "path": "/docs/value.txt", "range": { "mode": "bytes", "start": 0, "end": 7 } }]
      }),
    )
    .await;
  assert_eq!(ranges.status, StatusCode::OK);
  assert_root_headers(&ranges.headers, &root, "live");
  let ranges_json = ranges.json();
  assert_eq!(ranges_json["items"][0]["content"], "current");
  assert!(ranges_json.get("root").is_none(), "legacy range body gained a root envelope");

  let download = context.post("/files/download", &token, serde_json::json!({ "paths": ["/docs/value.txt"] })).await;
  assert_eq!(download.status, StatusCode::OK);
  assert_root_headers(&download.headers, &root, "live");
  assert_eq!(zip_files(download.bytes)["value.txt"], b"current value");
}

#[tokio::test]
async fn historical_whole_range_json_pointer_and_recursive_zip_remain_exact_after_head_advances() {
  let context = TestContext::new();
  let token = context.root_token();
  context.store("/docs/value.txt", b"historical value", "text/plain");
  context.store("/docs/data.json", br#"{"name":"historical"}"#, "application/json");
  context.store("/docs/sub/old.txt", b"old child", "text/plain");
  let snapshot = VersionManager::new(&context.engine).create_snapshot(&RequestContext::system(), "before", HashMap::new()).unwrap();
  let root_hex = hex::encode(&snapshot.root_hash);
  context.store("/docs/value.txt", b"successor value", "text/plain");
  context.store("/docs/data.json", br#"{"name":"successor"}"#, "application/json");
  context.store("/docs/sub/new.txt", b"new child", "text/plain");

  for selector in [
    serde_json::json!({ "root_hash": root_hex }),
    serde_json::json!({ "snapshot": "before" }),
    serde_json::json!({ "version": hex::encode(&snapshot.root_hash) }),
  ] {
    let mut body = serde_json::json!({ "paths": ["/docs/value.txt"] });
    body.as_object_mut().unwrap().extend(selector.as_object().unwrap().clone());
    let whole = context.post("/files/fetch", &token, body).await;
    assert_eq!(whole.status, StatusCode::OK);
    assert_root_headers(&whole.headers, &snapshot.root_hash, "retained");
    assert_eq!(whole.json()["/docs/value.txt"]["content"], "historical value");
  }

  let ranges = context
    .post(
      "/files/fetch",
      &token,
      serde_json::json!({
        "root_hash": hex::encode(&snapshot.root_hash),
        "items": [
          { "id": "bytes", "path": "/docs/value.txt", "range": { "mode": "bytes", "start": 0, "end": 10 } },
          { "id": "chars", "path": "/docs/value.txt", "range": { "mode": "chars", "start": 0, "end": 10 } },
          { "id": "lines", "path": "/docs/value.txt", "range": { "mode": "lines", "start": 1, "end": 1 } },
          { "id": "json", "path": "/docs/data.json", "range": { "mode": "json_pointer", "pointer": "/name" } }
        ]
      }),
    )
    .await;
  assert_eq!(ranges.status, StatusCode::OK);
  assert_root_headers(&ranges.headers, &snapshot.root_hash, "retained");
  let ranges = ranges.json();
  assert_eq!(ranges["items"][0]["content"], "historical");
  assert_eq!(ranges["items"][1]["content"], "historical");
  assert_eq!(ranges["items"][2]["content"], "historical value");
  assert_eq!(ranges["items"][3]["content"], "historical");

  let download = context.post("/files/download", &token, serde_json::json!({ "paths": ["/docs"], "snapshot": "before" })).await;
  assert_eq!(download.status, StatusCode::OK);
  assert_root_headers(&download.headers, &snapshot.root_hash, "retained");
  let files = zip_files(download.bytes);
  assert_eq!(files["docs/value.txt"], b"historical value");
  assert_eq!(files["docs/data.json"], br#"{"name":"historical"}"#);
  assert_eq!(files["docs/sub/old.txt"], b"old child");
  assert!(!files.contains_key("docs/sub/new.txt"));
}

#[tokio::test]
async fn body_selectors_are_strict_and_unavailable_roots_never_fall_back_to_head() {
  let context = TestContext::new();
  let token = context.root_token();
  context.store("/value.txt", b"current", "text/plain");
  let root = context.engine.head_hash().unwrap();

  for route in ["/files/fetch", "/files/download"] {
    let conflict = context
      .post(route, &token, serde_json::json!({ "paths": ["/value.txt"], "root_hash": hex::encode(&root), "snapshot": "before" }))
      .await;
    assert_eq!(conflict.status, StatusCode::BAD_REQUEST, "{route} conflict");
    assert_eq!(conflict.json()["code"], "INVALID_ROOT_SELECTOR");

    let malformed = context.post(route, &token, serde_json::json!({ "paths": ["/value.txt"], "root_hash": "xyz" })).await;
    assert_eq!(malformed.status, StatusCode::BAD_REQUEST, "{route} malformed");
    assert_eq!(malformed.json()["code"], "INVALID_ROOT_HASH");

    let unavailable = context.post(route, &token, serde_json::json!({ "paths": ["/value.txt"], "root_hash": "a5".repeat(32) })).await;
    assert_eq!(unavailable.status, StatusCode::SERVICE_UNAVAILABLE, "{route} unavailable");
    assert_eq!(unavailable.json()["code"], "HISTORICAL_VIEW_UNAVAILABLE");
  }
}

#[tokio::test]
async fn current_denial_precedes_root_resolution_and_selected_documents_can_only_restrict() {
  let context = TestContext::new();
  let root_token = context.root_token();
  context.store("/secure/value.txt", b"secret", "text/plain");
  let denied_token = context.token_for(uuid::Uuid::new_v4().to_string());

  for route in ["/files/fetch", "/files/download"] {
    let denied =
      context.post(route, &denied_token, serde_json::json!({ "paths": ["/secure/value.txt"], "root_hash": "a6".repeat(32) })).await;
    assert_eq!(denied.status, StatusCode::NOT_FOUND, "{route} denial");
    assert_ne!(denied.json()["code"], "HISTORICAL_VIEW_UNAVAILABLE");
  }

  let permission_path = "/secure/.aeordb-permissions";
  let selected_denial = PathPermissions {
    links: vec![PermissionLink {
      group: "not-a-member".to_string(),
      allow: "--------".to_string(),
      deny: "--------".to_string(),
      others_allow: None,
      others_deny: Some("-r------".to_string()),
      path_pattern: None,
    }],
  };
  context.store(permission_path, &selected_denial.serialize(), "application/json");
  let selected_root = context.engine.head_hash().unwrap();
  let current_grant = PathPermissions {
    links: vec![PermissionLink {
      group: "not-a-member".to_string(),
      allow: "--------".to_string(),
      deny: "--------".to_string(),
      others_allow: Some("-r------".to_string()),
      others_deny: None,
      path_pattern: None,
    }],
  };
  context.store(permission_path, &current_grant.serialize(), "application/json");
  let user_token = context.token_for(uuid::Uuid::new_v4().to_string());

  for route in ["/files/fetch", "/files/download"] {
    let current = context.post(route, &user_token, serde_json::json!({ "paths": ["/secure/value.txt"] })).await;
    assert_eq!(current.status, StatusCode::OK, "{route} current grant");
    let historical = context
      .post(route, &user_token, serde_json::json!({ "paths": ["/secure/value.txt"], "root_hash": hex::encode(&selected_root) }))
      .await;
    assert_eq!(historical.status, StatusCode::NOT_FOUND, "{route} selected restriction");
  }

  let current_range = context
    .post(
      "/files/fetch",
      &user_token,
      serde_json::json!({
        "items": [{ "path": "/secure/value.txt", "range": { "mode": "bytes", "start": 0, "end": 3 } }]
      }),
    )
    .await;
  assert_eq!(current_range.status, StatusCode::OK);
  let historical_range = context
    .post(
      "/files/fetch",
      &user_token,
      serde_json::json!({
        "root_hash": hex::encode(&selected_root),
        "items": [{ "path": "/secure/value.txt", "range": { "mode": "bytes", "start": 0, "end": 3 } }]
      }),
    )
    .await;
  assert_eq!(historical_range.status, StatusCode::NOT_FOUND);

  let root_historical = context
    .post("/files/fetch", &root_token, serde_json::json!({ "paths": ["/secure/value.txt"], "root_hash": hex::encode(&selected_root) }))
    .await;
  assert_eq!(root_historical.status, StatusCode::OK);
}

#[tokio::test]
async fn mixed_range_batch_preserves_continue_on_error_without_resolving_an_all_denied_root() {
  let context = TestContext::new();
  context.store("/open/value.txt", b"open value", "text/plain");
  context.store("/secure/value.txt", b"secret value", "text/plain");
  let open_grant = PathPermissions {
    links: vec![PermissionLink {
      group: "not-a-member".to_string(),
      allow: "--------".to_string(),
      deny: "--------".to_string(),
      others_allow: Some("-r------".to_string()),
      others_deny: None,
      path_pattern: None,
    }],
  };
  context.store("/open/.aeordb-permissions", &open_grant.serialize(), "application/json");
  let root = context.engine.head_hash().unwrap();
  let user_token = context.token_for(uuid::Uuid::new_v4().to_string());

  let mixed = context
    .post(
      "/files/fetch",
      &user_token,
      serde_json::json!({
        "continue_on_error": true,
        "items": [
          { "id": "allowed", "path": "/open/value.txt", "range": { "mode": "bytes", "start": 0, "end": 4 } },
          { "id": "denied", "path": "/secure/value.txt", "range": { "mode": "bytes", "start": 0, "end": 6 } }
        ]
      }),
    )
    .await;
  assert_eq!(mixed.status, StatusCode::OK);
  assert_root_headers(&mixed.headers, &root, "live");
  let mixed = mixed.json();
  assert_eq!(mixed["items"][0]["id"], "allowed");
  assert_eq!(mixed["items"][0]["content"], "open");
  assert_eq!(mixed["items"][1]["id"], "denied");
  assert_eq!(mixed["items"][1]["status"], "not_found");
  assert_eq!(mixed["has_errors"], true);

  let denied = context
    .post(
      "/files/fetch",
      &user_token,
      serde_json::json!({
        "continue_on_error": true,
        "root_hash": "a6".repeat(32),
        "items": [{ "id": "denied", "path": "/secure/value.txt", "range": { "mode": "bytes", "start": 0, "end": 6 } }]
      }),
    )
    .await;
  assert_eq!(denied.status, StatusCode::NOT_FOUND);
  assert_ne!(denied.json()["code"], "HISTORICAL_VIEW_UNAVAILABLE");
}

#[tokio::test]
async fn recursive_zip_reauthorizes_each_selected_descendant() {
  let context = TestContext::new();
  let root_token = context.root_token();
  context.store("/bundle/open.txt", b"open", "text/plain");
  context.store("/bundle/restricted/hidden.txt", b"hidden", "text/plain");
  let top_grant = PathPermissions {
    links: vec![PermissionLink {
      group: "not-a-member".to_string(),
      allow: "--------".to_string(),
      deny: "--------".to_string(),
      others_allow: Some("-r--l---".to_string()),
      others_deny: None,
      path_pattern: None,
    }],
  };
  let child_denial = PathPermissions {
    links: vec![PermissionLink {
      group: "not-a-member".to_string(),
      allow: "--------".to_string(),
      deny: "--------".to_string(),
      others_allow: None,
      others_deny: Some("-r------".to_string()),
      path_pattern: None,
    }],
  };
  context.store("/bundle/.aeordb-permissions", &top_grant.serialize(), "application/json");
  context.store("/bundle/restricted/.aeordb-permissions", &child_denial.serialize(), "application/json");
  let root = context.engine.head_hash().unwrap();
  let user_token = context.token_for(uuid::Uuid::new_v4().to_string());

  let download = context.post("/files/download", &user_token, serde_json::json!({ "paths": ["/bundle"] })).await;
  assert_eq!(download.status, StatusCode::OK);
  assert_root_headers(&download.headers, &root, "live");
  assert!(download.headers["x-aeordb-skipped"].to_str().unwrap().contains("/bundle/restricted/hidden.txt"));
  let files = zip_files(download.bytes);
  assert_eq!(files["bundle/open.txt"], b"open");
  assert!(!files.contains_key("bundle/restricted/hidden.txt"));

  let root_download = context.post("/files/download", &root_token, serde_json::json!({ "paths": ["/bundle"] })).await;
  assert_eq!(root_download.status, StatusCode::OK);
  assert_eq!(zip_files(root_download.bytes)["bundle/restricted/hidden.txt"], b"hidden");
}

#[tokio::test]
async fn zip_requires_list_for_a_selected_directory_and_concealed_only_requests_never_resolve_a_root() {
  let context = TestContext::new();
  context.store("/read-only/child.txt", b"child", "text/plain");
  let read_only = PathPermissions {
    links: vec![PermissionLink {
      group: "not-a-member".to_string(),
      allow: "--------".to_string(),
      deny: "--------".to_string(),
      others_allow: Some("-r------".to_string()),
      others_deny: None,
      path_pattern: None,
    }],
  };
  context.store("/read-only/.aeordb-permissions", &read_only.serialize(), "application/json");
  context.store("/.aeordb-conflicts/evidence.json", b"evidence", "application/json");
  let user_token = context.token_for(uuid::Uuid::new_v4().to_string());

  let read_only_directory = context.post("/files/download", &user_token, serde_json::json!({ "paths": ["/read-only"] })).await;
  assert_eq!(read_only_directory.status, StatusCode::NOT_FOUND);

  let concealed_only = context
    .post(
      "/files/download",
      &user_token,
      serde_json::json!({ "paths": ["/.aeordb-conflicts/evidence.json"], "root_hash": "a6".repeat(32) }),
    )
    .await;
  assert_eq!(concealed_only.status, StatusCode::NOT_FOUND);
  assert_ne!(concealed_only.json()["code"], "HISTORICAL_VIEW_UNAVAILABLE");
}

#[tokio::test]
async fn share_credentials_remain_current_only_for_fetch_and_download() {
  let context = TestContext::new();
  let root_token = context.root_token();
  context.store("/shared/value.txt", b"shared", "text/plain");
  let create = context
    .post("/files/share-link", &root_token, serde_json::json!({ "paths": ["/shared/"], "permissions": "-r--l---", "expires_in_days": 1 }))
    .await;
  assert_eq!(create.status, StatusCode::CREATED);
  let share_token = format!("Bearer {}", create.json()["token"].as_str().unwrap());
  let root = context.engine.head_hash().unwrap();

  for route in ["/files/fetch", "/files/download"] {
    let current = context.post(route, &share_token, serde_json::json!({ "paths": ["/shared/value.txt"] })).await;
    assert_eq!(current.status, StatusCode::OK, "{route} current share");
    assert_root_headers(&current.headers, &root, "live");

    let historical =
      context.post(route, &share_token, serde_json::json!({ "paths": ["/shared/value.txt"], "root_hash": hex::encode(&root) })).await;
    assert_eq!(historical.status, StatusCode::NOT_FOUND, "{route} historical share");
    assert_eq!(historical.json()["code"], "NOT_FOUND");
  }
}
