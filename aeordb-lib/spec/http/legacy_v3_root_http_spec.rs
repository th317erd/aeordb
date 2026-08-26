use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{DEFAULT_EXPIRY_SECONDS, JwtManager, TokenClaims};
use aeordb::engine::directory_ops::file_content_hash;
use aeordb::engine::permissions::{PathPermissions, PermissionLink};
use aeordb::engine::version_access::resolve_file_at_version;
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

  fn store(&self, path: &str, bytes: &[u8]) {
    DirectoryOps::new(&self.engine).store_file_buffered(&RequestContext::system(), path, bytes, Some("application/octet-stream")).unwrap();
  }

  async fn request(&self, method: Method, uri: impl AsRef<str>, token: &str, range: Option<&str>) -> HttpResult {
    let mut builder = Request::builder().method(method).uri(uri.as_ref()).header("authorization", token);
    if let Some(range) = range {
      builder = builder.header("range", range);
    }
    let response = self.app().oneshot(builder.body(Body::empty()).unwrap()).await.unwrap();
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

fn assert_root_json(value: &serde_json::Value, root_hash: &[u8], state: &str) {
  assert_eq!(value["root"]["hash"], hex::encode(root_hash));
  assert_eq!(value["root"]["state"], state);
  assert!(value["root"]["expires_at"].is_null());
}

#[tokio::test]
async fn current_file_list_symlink_and_head_responses_identify_one_live_root() {
  let context = TestContext::new();
  let root_token = context.root_token();
  context.store("/docs/report.txt", b"current report");
  DirectoryOps::new(&context.engine).store_symlink(&RequestContext::system(), "/docs/latest", "/docs/report.txt").unwrap();
  let root = context.engine.head_hash().unwrap();
  let (record_hash, _) = resolve_file_at_version(&context.engine, &root, "/docs/report.txt").unwrap();

  let file = context.request(Method::GET, "/files/docs/report.txt", &root_token, None).await;
  assert_eq!(file.status, StatusCode::OK);
  assert_eq!(file.bytes, b"current report");
  assert_root_headers(&file.headers, &root, "live");

  let listing = context.request(Method::GET, "/files/docs/", &root_token, None).await;
  assert_eq!(listing.status, StatusCode::OK);
  let listing = listing.json();
  assert_root_json(&listing, &root, "live");
  assert!(listing["items"].as_array().unwrap().iter().any(|item| item["path"] == "/docs/latest" && item["target"] == "/docs/report.txt"));

  let link = context.request(Method::GET, "/links/docs/latest", &root_token, None).await;
  assert_eq!(link.status, StatusCode::OK);
  let link = link.json();
  assert_root_json(&link, &root, "live");
  assert_eq!(link["target"], "/docs/report.txt");

  for path in [
    "/files/docs/report.txt".to_string(),
    "/files/docs/".to_string(),
    "/files/docs/latest".to_string(),
    "/links/docs/latest".to_string(),
    format!("/blobs/{}", hex::encode(record_hash)),
  ] {
    let head = context.request(Method::HEAD, &path, &root_token, None).await;
    assert_eq!(head.status, StatusCode::OK, "HEAD {path}");
    assert!(head.bytes.is_empty());
    assert_root_headers(&head.headers, &root, "live");
  }
}

#[tokio::test]
async fn explicit_snapshot_and_version_selectors_preserve_historical_file_range_list_and_symlink_after_head_advances() {
  let context = TestContext::new();
  let root_token = context.root_token();
  context.store("/docs/report.txt", b"historical-report");
  context.store("/docs/sub/old.txt", b"old");
  DirectoryOps::new(&context.engine).store_symlink(&RequestContext::system(), "/docs/latest", "/docs/report.txt").unwrap();
  let snapshot = VersionManager::new(&context.engine).create_snapshot(&RequestContext::system(), "before", HashMap::new()).unwrap();
  let root_hex = hex::encode(&snapshot.root_hash);
  context.store("/docs/report.txt", b"successor-report");
  context.store("/docs/sub/new.txt", b"new");

  for selector in [format!("root_hash={root_hex}"), "snapshot=before".to_string(), format!("version={root_hex}")] {
    let file = context.request(Method::GET, format!("/files/docs/report.txt?{selector}"), &root_token, Some("bytes=2-8")).await;
    assert_eq!(file.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(file.bytes, b"storica");
    assert_root_headers(&file.headers, &snapshot.root_hash, "retained");

    let listing = context.request(Method::GET, format!("/files/docs/?{selector}"), &root_token, None).await;
    assert_eq!(listing.status, StatusCode::OK);
    let listing = listing.json();
    assert_root_json(&listing, &snapshot.root_hash, "retained");
    assert!(listing["items"].as_array().unwrap().iter().any(|item| item["path"] == "/docs/report.txt"));

    let recursive = context.request(Method::GET, format!("/files/docs/?depth=-1&{selector}"), &root_token, None).await;
    assert_eq!(recursive.status, StatusCode::OK);
    let recursive = recursive.json();
    assert_root_json(&recursive, &snapshot.root_hash, "retained");
    let paths = recursive["items"].as_array().unwrap().iter().map(|item| item["path"].as_str().unwrap()).collect::<Vec<_>>();
    assert!(paths.contains(&"/docs/sub/old.txt"));
    assert!(!paths.contains(&"/docs/sub/new.txt"));

    let nofollow = context.request(Method::GET, format!("/files/docs/latest?nofollow=true&{selector}"), &root_token, None).await;
    assert_eq!(nofollow.status, StatusCode::OK);
    let nofollow = nofollow.json();
    assert_root_json(&nofollow, &snapshot.root_hash, "retained");
    assert_eq!(nofollow["target"], "/docs/report.txt");

    let link = context.request(Method::GET, format!("/links/docs/latest?{selector}"), &root_token, None).await;
    assert_eq!(link.status, StatusCode::OK);
    assert_root_json(&link.json(), &snapshot.root_hash, "retained");

    let head = context.request(Method::HEAD, format!("/files/docs/report.txt?{selector}"), &root_token, None).await;
    assert_eq!(head.status, StatusCode::OK);
    assert_root_headers(&head.headers, &snapshot.root_hash, "retained");
  }
}

#[tokio::test]
async fn selectors_are_mutually_exclusive_and_supplied_roots_never_fall_back_to_head() {
  let context = TestContext::new();
  let root_token = context.root_token();
  context.store("/value.txt", b"current");
  let root = context.engine.head_hash().unwrap();

  let conflicting =
    context.request(Method::GET, format!("/files/value.txt?root_hash={}&snapshot=anything", hex::encode(&root)), &root_token, None).await;
  assert_eq!(conflicting.status, StatusCode::BAD_REQUEST);
  assert_eq!(conflicting.json()["code"], "INVALID_ROOT_SELECTOR");

  let malformed = context.request(Method::GET, "/files/value.txt?root_hash=xyz", &root_token, None).await;
  assert_eq!(malformed.status, StatusCode::BAD_REQUEST);
  assert_eq!(malformed.json()["code"], "INVALID_ROOT_HASH");

  let unavailable = context.request(Method::GET, format!("/files/value.txt?root_hash={}", "a5".repeat(32)), &root_token, None).await;
  assert_eq!(unavailable.status, StatusCode::SERVICE_UNAVAILABLE);
  assert_eq!(unavailable.json()["code"], "HISTORICAL_VIEW_UNAVAILABLE");
}

#[tokio::test]
async fn current_path_denial_precedes_selected_root_resolution() {
  let context = TestContext::new();
  let root_token = context.root_token();
  context.store("/private/value.txt", b"secret");
  let denied_token = context.token_for(uuid::Uuid::new_v4().to_string());

  let response = context.request(Method::GET, format!("/files/private/value.txt?root_hash={}", "a6".repeat(32)), &denied_token, None).await;
  assert_eq!(response.status, StatusCode::FORBIDDEN);
  assert_ne!(response.json()["code"], "HISTORICAL_VIEW_UNAVAILABLE");

  let root_listing = context.request(Method::GET, format!("/files?root_hash={}", "a6".repeat(32)), &denied_token, None).await;
  assert_eq!(root_listing.status, StatusCode::FORBIDDEN);
  assert_ne!(root_listing.json()["code"], "HISTORICAL_VIEW_UNAVAILABLE");

  let root_response = context.request(Method::GET, "/files/private/value.txt", &root_token, None).await;
  assert_eq!(root_response.status, StatusCode::OK);
}

#[tokio::test]
async fn selected_permission_documents_can_restrict_but_not_expand_current_user_authority() {
  let context = TestContext::new();
  let root_token = context.root_token();
  context.store("/secure/value.txt", b"selected secret");
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
  context.store(permission_path, &selected_denial.serialize());
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
  context.store(permission_path, &current_grant.serialize());
  let user_token = context.token_for(uuid::Uuid::new_v4().to_string());

  let current = context.request(Method::GET, "/files/secure/value.txt", &user_token, None).await;
  assert_eq!(current.status, StatusCode::OK);
  assert_eq!(current.bytes, b"selected secret");

  let historical =
    context.request(Method::GET, format!("/files/secure/value.txt?root_hash={}", hex::encode(&selected_root)), &user_token, None).await;
  assert_eq!(historical.status, StatusCode::FORBIDDEN);

  let root_historical =
    context.request(Method::GET, format!("/files/secure/value.txt?root_hash={}", hex::encode(&selected_root)), &root_token, None).await;
  assert_eq!(root_historical.status, StatusCode::OK);
}

#[tokio::test]
async fn file_record_hash_requires_selected_root_reachability_and_current_path_authority() {
  let context = TestContext::new();
  let root_token = context.root_token();
  context.store("/docs/value.txt", b"old value");
  let selected_root = context.engine.head_hash().unwrap();
  let (old_record_hash, old_record) = resolve_file_at_version(&context.engine, &selected_root, "/docs/value.txt").unwrap();
  let old_record_version = context.engine.get_entry_header_including_deleted(&old_record_hash).unwrap().unwrap().entry_version;
  let old_content_hash = file_content_hash(
    &old_record.serialize_for_version(context.engine.hash_algo().hash_length(), old_record_version).unwrap(),
    &context.engine.hash_algo(),
  )
  .unwrap();
  let chunk_hash = old_record.chunk_hashes[0].clone();
  context.store("/docs/value.txt", b"new value");
  let current_root = context.engine.head_hash().unwrap();
  let (current_record_hash, current_record) = resolve_file_at_version(&context.engine, &current_root, "/docs/value.txt").unwrap();
  let current_record_version = context.engine.get_entry_header_including_deleted(&current_record_hash).unwrap().unwrap().entry_version;
  let current_content_hash = file_content_hash(
    &current_record.serialize_for_version(context.engine.hash_algo().hash_length(), current_record_version).unwrap(),
    &context.engine.hash_algo(),
  )
  .unwrap();

  for reachable_hash in [&old_record_hash, &old_content_hash] {
    let selected = context
      .request(Method::GET, format!("/blobs/{}?root_hash={}", hex::encode(reachable_hash), hex::encode(&selected_root)), &root_token, None)
      .await;
    assert_eq!(selected.status, StatusCode::OK);
    assert_eq!(selected.bytes, b"old value");
    assert_root_headers(&selected.headers, &selected_root, "retained");
  }

  for unreachable_hash in [&current_record_hash, &current_content_hash] {
    let unreachable = context
      .request(
        Method::GET,
        format!("/blobs/{}?root_hash={}", hex::encode(unreachable_hash), hex::encode(&selected_root)),
        &root_token,
        None,
      )
      .await;
    assert_eq!(unreachable.status, StatusCode::NOT_FOUND);
  }

  let denied_token = context.token_for(uuid::Uuid::new_v4().to_string());
  let denied = context
    .request(Method::GET, format!("/blobs/{}?root_hash={}", hex::encode(&old_record_hash), "a7".repeat(32)), &denied_token, None)
    .await;
  assert_eq!(denied.status, StatusCode::NOT_FOUND);
  let denied_malformed =
    context.request(Method::GET, format!("/blobs/{}?root_hash=xyz", hex::encode(&old_record_hash)), &denied_token, None).await;
  assert_eq!(denied_malformed.status, StatusCode::NOT_FOUND);

  let raw_chunk = context
    .request(Method::GET, format!("/blobs/{}?root_hash={}", hex::encode(&chunk_hash), hex::encode(&selected_root)), &root_token, None)
    .await;
  assert_eq!(raw_chunk.status, StatusCode::OK);
  assert_eq!(raw_chunk.bytes, b"old value");
  assert_root_headers(&raw_chunk.headers, &selected_root, "retained");

  let denied_raw = context
    .request(Method::GET, format!("/blobs/{}?root_hash={}", hex::encode(&chunk_hash), hex::encode(&selected_root)), &denied_token, None)
    .await;
  assert_eq!(denied_raw.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn share_credentials_remain_current_only_across_file_list_link_and_hash_reads() {
  let context = TestContext::new();
  let root_token = context.root_token();
  context.store("/shared/value.txt", b"shared value");
  DirectoryOps::new(&context.engine).store_symlink(&RequestContext::system(), "/shared/latest", "/shared/value.txt").unwrap();

  let create_body = serde_json::json!({
    "paths": ["/shared/"],
    "permissions": "-r--l---",
    "expires_in_days": 1,
  });
  let create_response = context
    .app()
    .oneshot(
      Request::builder()
        .method(Method::POST)
        .uri("/files/share-link")
        .header("authorization", &root_token)
        .header("content-type", "application/json")
        .body(Body::from(create_body.to_string()))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(create_response.status(), StatusCode::CREATED);
  let create_bytes = create_response.into_body().collect().await.unwrap().to_bytes();
  let create_json: serde_json::Value = serde_json::from_slice(&create_bytes).unwrap();
  let share_token = format!("Bearer {}", create_json["token"].as_str().unwrap());

  let current_root = context.engine.head_hash().unwrap();
  let (record_hash, _) = resolve_file_at_version(&context.engine, &current_root, "/shared/value.txt").unwrap();
  for uri in [
    "/files/shared/value.txt".to_string(),
    "/files/shared/".to_string(),
    "/links/shared/latest".to_string(),
    format!("/blobs/{}", hex::encode(&record_hash)),
  ] {
    let current = context.request(Method::GET, &uri, &share_token, None).await;
    assert_eq!(current.status, StatusCode::OK, "current share read {uri}");

    let separator = if uri.contains('?') { '&' } else { '?' };
    let historical_uri = format!("{uri}{separator}root_hash={}", hex::encode(&current_root));
    let historical = context.request(Method::GET, historical_uri, &share_token, None).await;
    assert_eq!(historical.status, StatusCode::NOT_FOUND, "historical share read {uri}");
    assert_eq!(historical.json()["code"], "NOT_FOUND");
  }
}
