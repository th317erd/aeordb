use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{DEFAULT_EXPIRY_SECONDS, JwtManager, TokenClaims};
use aeordb::engine::directory_ops::{file_content_hash, file_path_hash};
use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use aeordb::engine::index_store::IndexManager;
use aeordb::engine::permissions::{PathPermissions, PermissionLink};
use aeordb::engine::v4::position::{PositionRouteV1, decode_logical_position};
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

  async fn post_json(&self, uri: &str, token: &str, body: serde_json::Value) -> HttpResult {
    let response = self
      .app()
      .oneshot(
        Request::builder()
          .method(Method::POST)
          .uri(uri)
          .header("authorization", token)
          .header("content-type", "application/json")
          .body(Body::from(serde_json::to_vec(&body).unwrap()))
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
async fn historical_query_uses_selected_root_indexes_and_returns_root_metadata_after_head_advances() {
  let context = TestContext::new();
  let root_token = context.root_token();
  let operations = DirectoryOps::new(&context.engine);
  let configuration = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,
    indexes: vec![IndexFieldConfig { name: "name".to_string(), index_type: "string".to_string(), source: None, min: None, max: None }],
  };
  context.store("/records/.aeordb-config/indexes.json", &configuration.serialize());
  operations
    .store_file_with_indexing(&RequestContext::system(), "/records/historical.json", br#"{"name":"Historical"}"#, Some("application/json"))
    .unwrap();
  IndexManager::new(&context.engine).flush_buffered_indexes().unwrap();
  let snapshot = VersionManager::new(&context.engine).create_snapshot(&RequestContext::system(), "query-before", HashMap::new()).unwrap();

  operations
    .store_file_with_indexing(&RequestContext::system(), "/records/historical.json", br#"{"name":"Successor"}"#, Some("application/json"))
    .unwrap();
  operations
    .store_file_with_indexing(&RequestContext::system(), "/records/successor.json", br#"{"name":"Historical"}"#, Some("application/json"))
    .unwrap();
  IndexManager::new(&context.engine).flush_buffered_indexes().unwrap();

  let response = context
    .post_json(
      "/files/query",
      &root_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": hex::encode(&snapshot.root_hash),
        "where": { "field": "name", "op": "eq", "value": "Historical" },
        "include_total": true,
        "limit": 10
      }),
    )
    .await;
  assert_eq!(response.status, StatusCode::OK, "query response: {}", String::from_utf8_lossy(&response.bytes));
  let response = response.json();
  assert_root_json(&response, &snapshot.root_hash, "retained");
  assert_eq!(response["total"], 1);
  assert_eq!(response["items"].as_array().unwrap().len(), 1);
  assert_eq!(response["items"][0]["path"], "/records/historical.json");
}

#[tokio::test]
async fn current_query_rechecks_the_selected_root_when_a_persisted_index_lags_acknowledged_writes() {
  let context = TestContext::new();
  let root_token = context.root_token();
  let operations = DirectoryOps::new(&context.engine);
  let configuration = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: Some("**/*.json".to_string()),
    indexes: vec![IndexFieldConfig { name: "kind".to_string(), index_type: "string".to_string(), source: None, min: None, max: None }],
  };
  context.store("/records/.aeordb-config/indexes.json", &configuration.serialize());
  for path in ["/records/alpha.json", "/records/beta.json"] {
    operations.store_file_with_indexing(&RequestContext::system(), path, br#"{"kind":"match"}"#, Some("application/json")).unwrap();
  }
  IndexManager::new(&context.engine).flush_buffered_indexes().unwrap();

  operations
    .store_file_with_indexing(&RequestContext::system(), "/records/alpha.json", br#"{"kind":"successor"}"#, Some("application/json"))
    .unwrap();
  operations
    .store_file_with_indexing(&RequestContext::system(), "/records/successor.json", br#"{"kind":"match"}"#, Some("application/json"))
    .unwrap();

  let selected_root = context.engine.head_hash().unwrap();
  let response = context
    .post_json(
      "/files/query",
      &root_token,
      serde_json::json!({
        "path": "/records",
        "where": { "field": "kind", "op": "eq", "value": "match" },
        "include_total": true,
        "limit": 10
      }),
    )
    .await;
  assert_eq!(response.status, StatusCode::OK, "query response: {}", String::from_utf8_lossy(&response.bytes));
  let response = response.json();
  assert_root_json(&response, &selected_root, "live");
  assert_eq!(response["total"], 2);
  let paths = response["items"].as_array().unwrap().iter().map(|item| item["path"].as_str().unwrap()).collect::<Vec<_>>();
  assert_eq!(paths, vec!["/records/beta.json", "/records/successor.json"]);
}

#[tokio::test]
async fn historical_query_emits_canonical_apos_and_resumes_after_exact_selected_row() {
  let context = TestContext::new();
  let root_token = context.root_token();
  let operations = DirectoryOps::new(&context.engine);
  let configuration = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: Some("**/*.json".to_string()),
    indexes: vec![IndexFieldConfig { name: "kind".to_string(), index_type: "string".to_string(), source: None, min: None, max: None }],
  };
  context.store("/records/.aeordb-config/indexes.json", &configuration.serialize());
  for path in ["/records/alpha.json", "/records/beta.json"] {
    operations.store_file_with_indexing(&RequestContext::system(), path, br#"{"kind":"match"}"#, Some("application/json")).unwrap();
  }
  IndexManager::new(&context.engine).flush_buffered_indexes().unwrap();
  let snapshot =
    VersionManager::new(&context.engine).create_snapshot(&RequestContext::system(), "query-position-before", HashMap::new()).unwrap();
  let root_hash = hex::encode(&snapshot.root_hash);

  operations
    .store_file_with_indexing(&RequestContext::system(), "/records/alpha.json", br#"{"kind":"successor"}"#, Some("application/json"))
    .unwrap();
  operations
    .store_file_with_indexing(&RequestContext::system(), "/records/successor.json", br#"{"kind":"match"}"#, Some("application/json"))
    .unwrap();
  IndexManager::new(&context.engine).flush_buffered_indexes().unwrap();

  let first = context
    .post_json(
      "/files/query",
      &root_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": root_hash,
        "where": { "field": "kind", "op": "eq", "value": "match" },
        "include_total": true,
        "limit": 1
      }),
    )
    .await;
  assert_eq!(first.status, StatusCode::OK, "first query page: {}", String::from_utf8_lossy(&first.bytes));
  let first = first.json();
  assert_root_json(&first, &snapshot.root_hash, "retained");
  assert_eq!(first["total"], 2);
  assert_eq!(first["items"][0]["path"], "/records/alpha.json");
  assert_eq!(first["has_more"], true);
  let next_cursor = first["next_cursor"].as_str().expect("first page must emit next_cursor");
  assert!(!next_cursor.contains('='), "APOS must use canonical unpadded base64url");
  let decoded = decode_logical_position(next_cursor.as_bytes(), context.engine.hash_algo()).unwrap();
  assert_eq!(decoded.route, PositionRouteV1::Query);
  assert_eq!(decoded.namespace_root(), snapshot.root_hash);

  let plan_with_position = context
    .post_json(
      "/files/query",
      &root_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": hex::encode(&snapshot.root_hash),
        "where": { "field": "kind", "op": "eq", "value": "match" },
        "explain": "plan",
        "limit": 1,
        "after": next_cursor
      }),
    )
    .await;
  assert_eq!(plan_with_position.status, StatusCode::BAD_REQUEST);
  assert_eq!(plan_with_position.json()["code"], "INVALID_POSITION_CURSOR");

  let analyze_first = context
    .post_json(
      "/files/query",
      &root_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": hex::encode(&snapshot.root_hash),
        "where": { "field": "kind", "op": "eq", "value": "match" },
        "explain": "analyze",
        "include_total": true,
        "limit": 1
      }),
    )
    .await;
  assert_eq!(analyze_first.status, StatusCode::OK, "first EXPLAIN ANALYZE page: {}", String::from_utf8_lossy(&analyze_first.bytes));
  let analyze_first = analyze_first.json();
  assert_root_json(&analyze_first, &snapshot.root_hash, "retained");
  assert_eq!(analyze_first["items"]["items"][0]["path"], "/records/alpha.json");
  assert_eq!(analyze_first["items"]["total"], 2);
  let analyze_cursor = analyze_first["items"]["next_cursor"].as_str().expect("EXPLAIN ANALYZE must emit canonical APOS");
  let decoded = decode_logical_position(analyze_cursor.as_bytes(), context.engine.hash_algo()).unwrap();
  assert_eq!(decoded.route, PositionRouteV1::Query);
  assert_eq!(decoded.namespace_root(), snapshot.root_hash);

  let analyze_second = context
    .post_json(
      "/files/query",
      &root_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": hex::encode(&snapshot.root_hash),
        "where": { "field": "kind", "op": "eq", "value": "match" },
        "explain": "analyze",
        "include_total": true,
        "limit": 1,
        "after": analyze_cursor
      }),
    )
    .await;
  assert_eq!(analyze_second.status, StatusCode::OK, "second EXPLAIN ANALYZE page: {}", String::from_utf8_lossy(&analyze_second.bytes));
  let analyze_second = analyze_second.json();
  assert_eq!(analyze_second["items"]["items"][0]["path"], "/records/beta.json");
  assert_eq!(analyze_second["items"]["has_more"], false);

  let second = context
    .post_json(
      "/files/query",
      &root_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": hex::encode(&snapshot.root_hash),
        "where": { "field": "kind", "op": "eq", "value": "match" },
        "include_total": true,
        "limit": 1,
        "after": next_cursor
      }),
    )
    .await;
  assert_eq!(second.status, StatusCode::OK, "second query page: {}", String::from_utf8_lossy(&second.bytes));
  let second = second.json();
  assert_root_json(&second, &snapshot.root_hash, "retained");
  assert_eq!(second["total"], 2);
  assert_eq!(second["items"].as_array().unwrap().len(), 1);
  assert_eq!(second["items"][0]["path"], "/records/beta.json");
  assert_eq!(second["has_more"], false);
  let previous_cursor = second["prev_cursor"].as_str().expect("second page must emit prev_cursor");

  let previous = context
    .post_json(
      "/files/query",
      &root_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": hex::encode(&snapshot.root_hash),
        "where": { "field": "kind", "op": "eq", "value": "match" },
        "include_total": true,
        "limit": 1,
        "before": previous_cursor
      }),
    )
    .await;
  assert_eq!(previous.status, StatusCode::OK, "previous query page: {}", String::from_utf8_lossy(&previous.bytes));
  let previous = previous.json();
  assert_eq!(previous["items"].as_array().unwrap().len(), 1);
  assert_eq!(previous["items"][0]["path"], "/records/alpha.json");
  assert_eq!(previous["has_more"], false);
}

#[tokio::test]
async fn selected_query_order_uses_typed_signed_values_and_apos_tuple_continuity() {
  let context = TestContext::new();
  let root_token = context.root_token();
  let operations = DirectoryOps::new(&context.engine);
  let configuration = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: Some("**/*.json".to_string()),
    indexes: vec![
      IndexFieldConfig { name: "kind".to_string(), index_type: "string".to_string(), source: None, min: None, max: None },
      IndexFieldConfig { name: "priority".to_string(), index_type: "i64".to_string(), source: None, min: Some(-10.0), max: Some(10.0) },
    ],
  };
  context.store("/records/.aeordb-config/indexes.json", &configuration.serialize());
  for (path, priority) in [("/records/negative.json", -1), ("/records/positive.json", 2)] {
    operations
      .store_file_with_indexing(
        &RequestContext::system(),
        path,
        serde_json::to_string(&serde_json::json!({ "kind": "match", "priority": priority })).unwrap().as_bytes(),
        Some("application/json"),
      )
      .unwrap();
  }
  IndexManager::new(&context.engine).flush_buffered_indexes().unwrap();
  let selected_root = context.engine.head_hash().unwrap();

  let first = context
    .post_json(
      "/files/query",
      &root_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": hex::encode(&selected_root),
        "where": { "field": "kind", "op": "eq", "value": "match" },
        "order_by": [{ "field": "priority", "direction": "asc" }],
        "limit": 1
      }),
    )
    .await;
  assert_eq!(first.status, StatusCode::OK, "signed order first page: {}", String::from_utf8_lossy(&first.bytes));
  let first = first.json();
  assert_eq!(first["items"][0]["path"], "/records/negative.json");
  let next_cursor = first["next_cursor"].as_str().expect("signed order must emit APOS");

  let second = context
    .post_json(
      "/files/query",
      &root_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": hex::encode(&selected_root),
        "where": { "field": "kind", "op": "eq", "value": "match" },
        "order_by": [{ "field": "priority", "direction": "asc" }],
        "limit": 1,
        "after": next_cursor
      }),
    )
    .await;
  assert_eq!(second.status, StatusCode::OK, "signed order second page: {}", String::from_utf8_lossy(&second.bytes));
  assert_eq!(second.json()["items"][0]["path"], "/records/positive.json");
}

#[tokio::test]
async fn historical_search_uses_selected_root_indexes_and_returns_root_metadata_after_head_advances() {
  let context = TestContext::new();
  let root_token = context.root_token();
  let operations = DirectoryOps::new(&context.engine);
  let configuration = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,
    indexes: vec![IndexFieldConfig { name: "name".to_string(), index_type: "string".to_string(), source: None, min: None, max: None }],
  };
  context.store("/records/.aeordb-config/indexes.json", &configuration.serialize());
  operations
    .store_file_with_indexing(&RequestContext::system(), "/records/historical.json", br#"{"name":"Historical"}"#, Some("application/json"))
    .unwrap();
  IndexManager::new(&context.engine).flush_buffered_indexes().unwrap();
  let snapshot = VersionManager::new(&context.engine).create_snapshot(&RequestContext::system(), "search-before", HashMap::new()).unwrap();

  operations
    .store_file_with_indexing(&RequestContext::system(), "/records/historical.json", br#"{"name":"Successor"}"#, Some("application/json"))
    .unwrap();
  operations
    .store_file_with_indexing(&RequestContext::system(), "/records/successor.json", br#"{"name":"Historical"}"#, Some("application/json"))
    .unwrap();
  IndexManager::new(&context.engine).flush_buffered_indexes().unwrap();

  let response = context
    .post_json(
      "/files/search",
      &root_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": hex::encode(&snapshot.root_hash),
        "where": { "field": "name", "op": "eq", "value": "Historical" },
        "limit": 10
      }),
    )
    .await;
  assert_eq!(response.status, StatusCode::OK, "search response: {}", String::from_utf8_lossy(&response.bytes));
  let response = response.json();
  assert_root_json(&response, &snapshot.root_hash, "retained");
  assert_eq!(response["total_count"], 1);
  assert_eq!(response["results"].as_array().unwrap().len(), 1);
  assert_eq!(response["results"][0]["path"], "/records/historical.json");
}

#[tokio::test]
async fn historical_non_json_search_fails_closed_when_the_detached_parser_registry_cannot_be_proven() {
  let context = TestContext::new();
  let root_token = context.root_token();
  let operations = DirectoryOps::new(&context.engine);
  let configuration = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: Some("**/*.txt".to_string()),
    indexes: vec![IndexFieldConfig {
      name: "text".to_string(),
      index_type: "trigram".to_string(),
      source: Some(serde_json::json!(["text"])),
      min: None,
      max: None,
    }],
  };
  context.store("/.aeordb-config/parsers.json", b"{}");
  context.store("/docs/.aeordb-config/indexes.json", &configuration.serialize());
  operations.store_file_with_indexing(&RequestContext::system(), "/docs/ambiguous.txt", b"needle", Some("text/plain")).unwrap();
  IndexManager::new(&context.engine).flush_buffered_indexes().unwrap();
  let snapshot = VersionManager::new(&context.engine).create_snapshot(&RequestContext::system(), "parser-before", HashMap::new()).unwrap();
  context.store("/advance.json", br#"{"state":"successor"}"#);

  let response = context
    .post_json(
      "/files/search",
      &root_token,
      serde_json::json!({
        "path": "/docs",
        "root_hash": hex::encode(&snapshot.root_hash),
        "query": "needle",
        "limit": 10
      }),
    )
    .await;
  assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE, "historical search response: {}", String::from_utf8_lossy(&response.bytes));
  assert_eq!(response.json()["code"], "HISTORICAL_VIEW_UNAVAILABLE");
}

#[tokio::test]
async fn historical_search_locators_fetch_exact_crlf_bytes_from_the_same_root_after_head_advances() {
  let context = TestContext::new();
  let root_token = context.root_token();
  let operations = DirectoryOps::new(&context.engine);
  let configuration = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: Some("**/*.txt".to_string()),
    indexes: vec![IndexFieldConfig {
      name: "text".to_string(),
      index_type: "trigram".to_string(),
      source: Some(serde_json::json!(["text"])),
      min: None,
      max: None,
    }],
  };
  context.store("/docs/.aeordb-config/indexes.json", &configuration.serialize());
  let historical_body = b"zero\r\nneedle here\r\nlast\r\n";
  operations.store_file_with_indexing(&RequestContext::system(), "/docs/crlf.txt", historical_body, Some("application/json")).unwrap();
  IndexManager::new(&context.engine).flush_buffered_indexes().unwrap();
  let snapshot = VersionManager::new(&context.engine).create_snapshot(&RequestContext::system(), "locator-before", HashMap::new()).unwrap();
  let (historical_record_revision, historical_record) =
    resolve_file_at_version(&context.engine, &snapshot.root_hash, "/docs/crlf.txt").unwrap();

  operations
    .store_file_with_indexing(&RequestContext::system(), "/docs/crlf.txt", b"evil\r\nmutant here\r\nlast\r\n", Some("application/json"))
    .unwrap();
  IndexManager::new(&context.engine).flush_buffered_indexes().unwrap();

  let response = context
    .post_json(
      "/files/search",
      &root_token,
      serde_json::json!({
        "path": "/docs",
        "root_hash": hex::encode(&snapshot.root_hash),
        "query": "needle",
        "include_matches": true,
        "max_matches_per_result": 1,
        "snippet_chars": 32,
        "limit": 10
      }),
    )
    .await;
  assert_eq!(response.status, StatusCode::OK, "historical search response: {}", String::from_utf8_lossy(&response.bytes));
  let response = response.json();
  assert_root_json(&response, &snapshot.root_hash, "retained");
  let result = &response["results"][0];
  assert_eq!(result["path"], "/docs/crlf.txt");
  assert_eq!(result["file_key"], hex::encode(file_path_hash("/docs/crlf.txt", &context.engine.hash_algo()).unwrap()));
  assert_eq!(result["record_revision"], hex::encode(&historical_record_revision));
  assert_eq!(result["content_hash"], historical_record.content_hash_hex());
  assert_eq!(result["locator_status"], "complete");
  let locator = &result["matches"][0];
  assert_eq!(locator["matched_text"], "needle");
  assert_eq!(
    locator["range"]["byte"],
    serde_json::json!({
      "start": 6,
      "end": 12,
      "unit": "utf8-byte",
      "basis": "stored-file"
    })
  );
  assert_eq!(
    locator["range"]["char"],
    serde_json::json!({
      "start": 5,
      "end": 11,
      "unit": "unicode-scalar",
      "basis": "stored-file-text"
    })
  );
  assert_eq!(
    locator["range"]["line"],
    serde_json::json!({
      "start": 2,
      "end": 2,
      "unit": "line",
      "basis": "stored-file-text"
    })
  );
  assert_eq!(
    locator["range"]["column"],
    serde_json::json!({
      "start": 0,
      "end": 6,
      "unit": "unicode-scalar",
      "basis": "line"
    })
  );

  let historical_fetch = context
    .post_json(
      "/files/fetch",
      &root_token,
      serde_json::json!({
        "root_hash": hex::encode(&snapshot.root_hash),
        "items": [{
          "id": "exact-match",
          "path": "/docs/crlf.txt",
          "if_content_hash": result["content_hash"],
          "range": { "mode": "bytes", "start": locator["range"]["byte"]["start"], "end": locator["range"]["byte"]["end"] }
        }]
      }),
    )
    .await;
  assert_eq!(historical_fetch.status, StatusCode::OK, "historical range fetch: {}", String::from_utf8_lossy(&historical_fetch.bytes));
  assert_root_headers(&historical_fetch.headers, &snapshot.root_hash, "retained");
  assert_eq!(historical_fetch.json()["items"][0]["content"], "needle");

  let current_fetch = context
    .post_json(
      "/files/fetch",
      &root_token,
      serde_json::json!({
        "items": [{
          "id": "same-range-at-head",
          "path": "/docs/crlf.txt",
          "range": { "mode": "bytes", "start": 6, "end": 12 }
        }]
      }),
    )
    .await;
  assert_eq!(current_fetch.status, StatusCode::OK, "current range fetch: {}", String::from_utf8_lossy(&current_fetch.bytes));
  assert_eq!(current_fetch.json()["items"][0]["content"], "mutant");
}

#[tokio::test]
async fn historical_search_emits_canonical_apos_and_resumes_after_exact_selected_row() {
  let context = TestContext::new();
  let root_token = context.root_token();
  let operations = DirectoryOps::new(&context.engine);
  let configuration = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: Some("**/*.json".to_string()),
    indexes: vec![IndexFieldConfig { name: "kind".to_string(), index_type: "string".to_string(), source: None, min: None, max: None }],
  };
  context.store("/records/.aeordb-config/indexes.json", &configuration.serialize());
  for path in ["/records/alpha.json", "/records/beta.json"] {
    operations.store_file_with_indexing(&RequestContext::system(), path, br#"{"kind":"match"}"#, Some("application/json")).unwrap();
  }
  IndexManager::new(&context.engine).flush_buffered_indexes().unwrap();
  let snapshot =
    VersionManager::new(&context.engine).create_snapshot(&RequestContext::system(), "search-position-before", HashMap::new()).unwrap();

  operations
    .store_file_with_indexing(&RequestContext::system(), "/records/alpha.json", br#"{"kind":"successor"}"#, Some("application/json"))
    .unwrap();
  operations
    .store_file_with_indexing(&RequestContext::system(), "/records/successor.json", br#"{"kind":"match"}"#, Some("application/json"))
    .unwrap();
  IndexManager::new(&context.engine).flush_buffered_indexes().unwrap();

  let first = context
    .post_json(
      "/files/search",
      &root_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": hex::encode(&snapshot.root_hash),
        "where": { "field": "kind", "op": "eq", "value": "match" },
        "limit": 1
      }),
    )
    .await;
  assert_eq!(first.status, StatusCode::OK, "first search page: {}", String::from_utf8_lossy(&first.bytes));
  let first = first.json();
  assert_root_json(&first, &snapshot.root_hash, "retained");
  assert_eq!(first["total_count"], 2);
  assert_eq!(first["results"][0]["path"], "/records/alpha.json");
  let next_cursor = first["next_cursor"].as_str().expect("first search page must emit next_cursor");
  let decoded = decode_logical_position(next_cursor.as_bytes(), context.engine.hash_algo()).unwrap();
  assert_eq!(decoded.route, PositionRouteV1::GlobalSearch);
  assert_eq!(decoded.namespace_root(), snapshot.root_hash);

  let second = context
    .post_json(
      "/files/search",
      &root_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": hex::encode(&snapshot.root_hash),
        "where": { "field": "kind", "op": "eq", "value": "match" },
        "limit": 1,
        "after": next_cursor
      }),
    )
    .await;
  assert_eq!(second.status, StatusCode::OK, "second search page: {}", String::from_utf8_lossy(&second.bytes));
  let second = second.json();
  assert_root_json(&second, &snapshot.root_hash, "retained");
  assert_eq!(second["total_count"], 2);
  assert_eq!(second["results"].as_array().unwrap().len(), 1);
  assert_eq!(second["results"][0]["path"], "/records/beta.json");
  assert_eq!(second["has_more"], false);
}

#[tokio::test]
async fn selected_search_intersects_text_and_structured_predicates_before_totals() {
  let context = TestContext::new();
  let root_token = context.root_token();
  let operations = DirectoryOps::new(&context.engine);
  let configuration = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: Some("**/*.json".to_string()),
    indexes: vec![
      IndexFieldConfig { name: "name".to_string(), index_type: "trigram".to_string(), source: None, min: None, max: None },
      IndexFieldConfig { name: "kind".to_string(), index_type: "string".to_string(), source: None, min: None, max: None },
    ],
  };
  context.store("/records/.aeordb-config/indexes.json", &configuration.serialize());
  for (path, kind) in [("/records/allowed.json", "allowed"), ("/records/denied.json", "denied")] {
    operations
      .store_file_with_indexing(
        &RequestContext::system(),
        path,
        serde_json::to_string(&serde_json::json!({ "name": "Alpha", "kind": kind })).unwrap().as_bytes(),
        Some("application/json"),
      )
      .unwrap();
  }
  IndexManager::new(&context.engine).flush_buffered_indexes().unwrap();
  let selected_root = context.engine.head_hash().unwrap();

  let response = context
    .post_json(
      "/files/search",
      &root_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": hex::encode(&selected_root),
        "query": "Alpha",
        "where": { "field": "kind", "op": "eq", "value": "allowed" },
        "limit": 10
      }),
    )
    .await;
  assert_eq!(response.status, StatusCode::OK, "combined search response: {}", String::from_utf8_lossy(&response.bytes));
  let response = response.json();
  assert_eq!(response["total_count"], 1);
  assert_eq!(response["results"].as_array().unwrap().len(), 1);
  assert_eq!(response["results"][0]["path"], "/records/allowed.json");
}

#[tokio::test]
async fn historical_aggregate_groups_emit_canonical_apos_and_resume_by_group_tuple() {
  let context = TestContext::new();
  let root_token = context.root_token();
  let operations = DirectoryOps::new(&context.engine);
  let configuration = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: Some("**/*.json".to_string()),
    indexes: vec![IndexFieldConfig { name: "kind".to_string(), index_type: "string".to_string(), source: None, min: None, max: None }],
  };
  context.store("/records/.aeordb-config/indexes.json", &configuration.serialize());
  for (path, kind) in [("/records/a1.json", "alpha"), ("/records/a2.json", "alpha"), ("/records/b1.json", "beta")] {
    operations
      .store_file_with_indexing(
        &RequestContext::system(),
        path,
        serde_json::to_string(&serde_json::json!({ "kind": kind })).unwrap().as_bytes(),
        Some("application/json"),
      )
      .unwrap();
  }
  IndexManager::new(&context.engine).flush_buffered_indexes().unwrap();
  let snapshot =
    VersionManager::new(&context.engine).create_snapshot(&RequestContext::system(), "aggregate-position-before", HashMap::new()).unwrap();
  operations
    .store_file_with_indexing(&RequestContext::system(), "/records/a1.json", br#"{"kind":"successor"}"#, Some("application/json"))
    .unwrap();
  IndexManager::new(&context.engine).flush_buffered_indexes().unwrap();

  let first = context
    .post_json(
      "/files/query",
      &root_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": hex::encode(&snapshot.root_hash),
        "where": { "field": "kind", "op": "in", "value": ["alpha", "beta"] },
        "aggregate": { "count": true, "group_by": ["kind"] },
        "limit": 1
      }),
    )
    .await;
  assert_eq!(first.status, StatusCode::OK, "first aggregate page: {}", String::from_utf8_lossy(&first.bytes));
  let first = first.json();
  assert_root_json(&first, &snapshot.root_hash, "retained");
  assert_eq!(first["count"], 3);
  assert_eq!(first["groups"].as_array().unwrap().len(), 1);
  assert_eq!(first["groups"][0]["key"]["kind"], "alpha");
  assert_eq!(first["groups"][0]["count"], 2);
  let next_cursor = first["next_cursor"].as_str().expect("first aggregate page must emit next_cursor");
  let decoded = decode_logical_position(next_cursor.as_bytes(), context.engine.hash_algo()).unwrap();
  assert_eq!(decoded.route, PositionRouteV1::AggregateGroups);
  assert_eq!(decoded.namespace_root(), snapshot.root_hash);
  assert_eq!(decoded.record_revision_tie(), snapshot.root_hash);

  let analyze_first = context
    .post_json(
      "/files/query",
      &root_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": hex::encode(&snapshot.root_hash),
        "where": { "field": "kind", "op": "in", "value": ["alpha", "beta"] },
        "aggregate": { "count": true, "group_by": ["kind"] },
        "explain": "analyze",
        "limit": 1
      }),
    )
    .await;
  assert_eq!(analyze_first.status, StatusCode::OK, "first aggregate EXPLAIN page: {}", String::from_utf8_lossy(&analyze_first.bytes));
  let analyze_first = analyze_first.json();
  assert_root_json(&analyze_first, &snapshot.root_hash, "retained");
  assert_eq!(analyze_first["items"]["groups"][0]["key"]["kind"], "alpha");
  let analyze_cursor = analyze_first["items"]["next_cursor"].as_str().expect("aggregate EXPLAIN must emit canonical APOS");
  let decoded = decode_logical_position(analyze_cursor.as_bytes(), context.engine.hash_algo()).unwrap();
  assert_eq!(decoded.route, PositionRouteV1::AggregateGroups);

  let analyze_second = context
    .post_json(
      "/files/query",
      &root_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": hex::encode(&snapshot.root_hash),
        "where": { "field": "kind", "op": "in", "value": ["alpha", "beta"] },
        "aggregate": { "count": true, "group_by": ["kind"] },
        "explain": "analyze",
        "limit": 1,
        "after": analyze_cursor
      }),
    )
    .await;
  assert_eq!(analyze_second.status, StatusCode::OK, "second aggregate EXPLAIN page: {}", String::from_utf8_lossy(&analyze_second.bytes));
  let analyze_second = analyze_second.json();
  assert_eq!(analyze_second["items"]["groups"][0]["key"]["kind"], "beta");
  assert_eq!(analyze_second["items"]["has_more"], false);

  let second = context
    .post_json(
      "/files/query",
      &root_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": hex::encode(&snapshot.root_hash),
        "where": { "field": "kind", "op": "in", "value": ["alpha", "beta"] },
        "aggregate": { "count": true, "group_by": ["kind"] },
        "limit": 1,
        "after": next_cursor
      }),
    )
    .await;
  assert_eq!(second.status, StatusCode::OK, "second aggregate page: {}", String::from_utf8_lossy(&second.bytes));
  let second = second.json();
  assert_eq!(second["groups"].as_array().unwrap().len(), 1);
  assert_eq!(second["groups"][0]["key"]["kind"], "beta");
  assert_eq!(second["groups"][0]["count"], 1);
  assert_eq!(second["has_more"], false);
}

#[tokio::test]
async fn selected_permissions_filter_query_search_aggregate_and_explain_before_observables() {
  let context = TestContext::new();
  let operations = DirectoryOps::new(&context.engine);
  let root_token = context.root_token();
  let user_token = context.token_for(uuid::Uuid::new_v4().to_string());
  let configuration = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: Some("**/*.json".to_string()),
    indexes: vec![IndexFieldConfig { name: "kind".to_string(), index_type: "string".to_string(), source: None, min: None, max: None }],
  };
  context.store("/records/.aeordb-config/indexes.json", &configuration.serialize());
  context.store(
    "/records/.aeordb-permissions",
    &PathPermissions {
      links: vec![PermissionLink {
        group: "not-a-member".to_string(),
        allow: "--------".to_string(),
        deny: "--------".to_string(),
        others_allow: Some("-r------".to_string()),
        others_deny: None,
        path_pattern: None,
      }],
    }
    .serialize(),
  );
  context.store(
    "/records/private/.aeordb-permissions",
    &PathPermissions {
      links: vec![PermissionLink {
        group: "not-a-member".to_string(),
        allow: "--------".to_string(),
        deny: "--------".to_string(),
        others_allow: None,
        others_deny: Some("-r------".to_string()),
        path_pattern: None,
      }],
    }
    .serialize(),
  );
  operations
    .store_file_with_indexing(&RequestContext::system(), "/records/public/visible.json", br#"{"kind":"match"}"#, Some("application/json"))
    .unwrap();
  operations
    .store_file_with_indexing(&RequestContext::system(), "/records/private/hidden.json", br#"{"kind":"match"}"#, Some("application/json"))
    .unwrap();
  IndexManager::new(&context.engine).flush_buffered_indexes().unwrap();
  let snapshot =
    VersionManager::new(&context.engine).create_snapshot(&RequestContext::system(), "permission-before", HashMap::new()).unwrap();

  context.store(
    "/records/private/.aeordb-permissions",
    &PathPermissions {
      links: vec![PermissionLink {
        group: "not-a-member".to_string(),
        allow: "--------".to_string(),
        deny: "--------".to_string(),
        others_allow: Some("-r------".to_string()),
        others_deny: None,
        path_pattern: None,
      }],
    }
    .serialize(),
  );
  let root_hash = hex::encode(&snapshot.root_hash);
  let base_query = serde_json::json!({
    "path": "/records",
    "root_hash": root_hash,
    "where": { "field": "kind", "op": "eq", "value": "match" },
    "include_total": true,
    "limit": 10
  });

  let root_page = context
    .post_json(
      "/files/query",
      &root_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": root_hash,
        "where": { "field": "kind", "op": "eq", "value": "match" },
        "limit": 1
      }),
    )
    .await;
  assert_eq!(root_page.status, StatusCode::OK, "root query page: {}", String::from_utf8_lossy(&root_page.bytes));
  let hidden_cursor = root_page.json()["next_cursor"].as_str().expect("root page must expose its own continuation").to_string();
  let denied_cursor = context
    .post_json(
      "/files/query",
      &user_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": hex::encode(&snapshot.root_hash),
        "where": { "field": "kind", "op": "eq", "value": "match" },
        "limit": 1,
        "after": hidden_cursor.clone()
      }),
    )
    .await;
  assert_eq!(denied_cursor.status, StatusCode::BAD_REQUEST);
  assert_eq!(denied_cursor.json()["code"], "INVALID_POSITION_CURSOR");

  let query = context.post_json("/files/query", &user_token, base_query.clone()).await;
  assert_eq!(query.status, StatusCode::OK, "query response: {}", String::from_utf8_lossy(&query.bytes));
  let query = query.json();
  assert_root_json(&query, &snapshot.root_hash, "retained");
  assert_eq!(query["total"], 1);
  assert_eq!(query["items"].as_array().unwrap().len(), 1);
  assert_eq!(query["items"][0]["path"], "/records/public/visible.json");

  let aggregate = context
    .post_json(
      "/files/query",
      &user_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": hex::encode(&snapshot.root_hash),
        "where": { "field": "kind", "op": "eq", "value": "match" },
        "aggregate": { "count": true },
        "limit": 10
      }),
    )
    .await;
  assert_eq!(aggregate.status, StatusCode::OK, "aggregate response: {}", String::from_utf8_lossy(&aggregate.bytes));
  let aggregate = aggregate.json();
  assert_root_json(&aggregate, &snapshot.root_hash, "retained");
  assert_eq!(aggregate["count"], 1);

  let explain = context
    .post_json(
      "/files/query",
      &user_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": hex::encode(&snapshot.root_hash),
        "where": { "field": "kind", "op": "eq", "value": "match" },
        "include_total": true,
        "explain": "analyze",
        "limit": 10
      }),
    )
    .await;
  assert_eq!(explain.status, StatusCode::OK, "explain response: {}", String::from_utf8_lossy(&explain.bytes));
  let explain = explain.json();
  assert_root_json(&explain, &snapshot.root_hash, "retained");
  assert_eq!(explain["execution"]["results_returned"], 1);
  assert_eq!(explain["items"]["items"].as_array().unwrap().len(), 1);
  let explain_text = serde_json::to_string(&explain["plan"]).unwrap();
  for hidden_detail in ["entries", "values_stored", "index_source", "index_field", "strategy", "indexes"] {
    assert!(!explain_text.contains(hidden_detail), "logical EXPLAIN leaked physical detail {hidden_detail}: {explain_text}");
  }

  let denied_explain_cursor = context
    .post_json(
      "/files/query",
      &user_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": hex::encode(&snapshot.root_hash),
        "where": { "field": "kind", "op": "eq", "value": "match" },
        "explain": "analyze",
        "limit": 1,
        "after": hidden_cursor
      }),
    )
    .await;
  assert_eq!(denied_explain_cursor.status, StatusCode::BAD_REQUEST);
  assert_eq!(denied_explain_cursor.json()["code"], "INVALID_POSITION_CURSOR");

  let search = context
    .post_json(
      "/files/search",
      &user_token,
      serde_json::json!({
        "path": "/records",
        "root_hash": hex::encode(&snapshot.root_hash),
        "where": { "field": "kind", "op": "eq", "value": "match" },
        "limit": 10
      }),
    )
    .await;
  assert_eq!(search.status, StatusCode::OK, "search response: {}", String::from_utf8_lossy(&search.bytes));
  let search = search.json();
  assert_root_json(&search, &snapshot.root_hash, "retained");
  assert_eq!(search["total_count"], 1);
  assert_eq!(search["results"].as_array().unwrap().len(), 1);
  assert_eq!(search["results"][0]["path"], "/records/public/visible.json");
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
