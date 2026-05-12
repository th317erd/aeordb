use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use aeordb::engine::StorageEngine;
use aeordb::engine::RequestContext;
use aeordb::server::{create_app_with_jwt_and_engine, create_temp_engine_for_tests};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let app = create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone());
  (app, jwt_manager, engine, temp_dir)
}

fn rebuild_app(
  jwt_manager: &Arc<JwtManager>,
  engine: &Arc<StorageEngine>,
) -> axum::Router {
  create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone())
}

fn bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: "test-admin".to_string(),
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
  serde_json::from_slice(&bytes).expect("valid JSON response body")
}

fn store_index_config(engine: &StorageEngine, parent_path: &str, config: &PathIndexConfig) {
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(engine);
  let config_path = if parent_path.ends_with('/') {
    format!("{}.config/indexes.json", parent_path)
  } else {
    format!("{}/.aeordb-config/indexes.json", parent_path)
  };
  let config_data = config.serialize();
  ops.store_file(&ctx, &config_path, &config_data, Some("application/json")).unwrap();
}

/// Create two separate directories (/people and /products) each with their own
/// indexes so global search can discover and search across both.
fn setup_multi_directory(engine: &StorageEngine) {
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(engine);

  // --- /people: name (string + trigram + soundex + dmetaphone) ---
  let people_config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,
    indexes: vec![
      IndexFieldConfig {
        name: "name".to_string(),
        index_type: "string".to_string(),
        source: None,
        min: None,
        max: None,
      },
      IndexFieldConfig {
        name: "name".to_string(),
        index_type: "trigram".to_string(),
        source: None,
        min: None,
        max: None,
      },
      IndexFieldConfig {
        name: "name".to_string(),
        index_type: "soundex".to_string(),
        source: None,
        min: None,
        max: None,
      },
      IndexFieldConfig {
        name: "name".to_string(),
        index_type: "dmetaphone".to_string(),
        source: None,
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(engine, "/people", &people_config);

  let people = vec![
    ("alice.json", r#"{"name":"Alice","age":30}"#),
    ("bob.json", r#"{"name":"Bob","age":25}"#),
    ("charlie.json", r#"{"name":"Charlie","age":40}"#),
  ];
  for (filename, json) in &people {
    ops.store_file_with_indexing(
      &ctx,
      &format!("/people/{}", filename),
      json.as_bytes(),
      Some("application/json"),
    ).unwrap();
  }

  // --- /products: title (string + trigram), price (u64) ---
  let products_config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,
    indexes: vec![
      IndexFieldConfig {
        name: "title".to_string(),
        index_type: "string".to_string(),
        source: None,
        min: None,
        max: None,
      },
      IndexFieldConfig {
        name: "title".to_string(),
        index_type: "trigram".to_string(),
        source: None,
        min: None,
        max: None,
      },
      IndexFieldConfig {
        name: "price".to_string(),
        index_type: "u64".to_string(),
        source: None,
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(engine, "/products", &products_config);

  let products = vec![
    ("widget.json", r#"{"title":"Alice Widget","price":100}"#),
    ("gadget.json", r#"{"title":"Super Gadget","price":500}"#),
    ("doohickey.json", r#"{"title":"Doohickey","price":50}"#),
  ];
  for (filename, json) in &products {
    ops.store_file_with_indexing(
      &ctx,
      &format!("/products/{}", filename),
      json.as_bytes(),
      Some("application/json"),
    ).unwrap();
  }
}

/// Helper: send a POST /files/search request and return (status, json body).
async fn search_request(
  app: axum::Router,
  auth: &str,
  body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
  let request = Request::builder()
    .method("POST")
    .uri("/files/search")
    .header("content-type", "application/json")
    .header("authorization", auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let status = response.status();
  let json = body_json(response.into_body()).await;
  (status, json)
}

// ===========================================================================
// Happy path tests
// ===========================================================================

#[tokio::test]
async fn test_broad_search_across_directories() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // "Alice" appears in /people/alice.json (name) AND /products/widget.json (title)
  let (status, json) = search_request(app, &auth, serde_json::json!({
    "query": "Alice",
    "limit": 50
  })).await;

  assert_eq!(status, StatusCode::OK);

  let results = json["results"].as_array().expect("results should be an array");
  assert!(results.len() >= 2, "should match across both directories, got {} results", results.len());

  let paths: Vec<&str> = results.iter().map(|r| r["path"].as_str().unwrap()).collect();
  assert!(paths.contains(&"/people/alice.json"), "should find Alice in /people, got: {:?}", paths);
  assert!(paths.contains(&"/products/widget.json"), "should find 'Alice Widget' in /products, got: {:?}", paths);
}

#[tokio::test]
async fn test_broad_search_returns_source_field() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let (status, json) = search_request(app, &auth, serde_json::json!({
    "query": "Alice"
  })).await;

  assert_eq!(status, StatusCode::OK);

  let results = json["results"].as_array().unwrap();
  for result in results {
    assert!(result.get("source").is_some(), "each result must include 'source' field, got: {:?}", result);
    let source = result["source"].as_str().unwrap();
    assert!(source.starts_with('/'), "source should be a path, got: {}", source);
  }
}

#[tokio::test]
async fn test_broad_search_response_shape() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let (status, json) = search_request(app, &auth, serde_json::json!({
    "query": "Alice"
  })).await;

  assert_eq!(status, StatusCode::OK);

  // Envelope fields
  assert!(json.get("results").is_some(), "response must have 'results'");
  assert!(json.get("has_more").is_some(), "response must have 'has_more'");
  assert!(json["has_more"].is_boolean(), "has_more must be boolean");

  // Each result has the documented fields
  let results = json["results"].as_array().unwrap();
  assert!(!results.is_empty());
  for result in results {
    assert!(result.get("path").is_some(), "missing 'path'");
    assert!(result.get("score").is_some(), "missing 'score'");
    assert!(result.get("matched_by").is_some(), "missing 'matched_by'");
    assert!(result.get("source").is_some(), "missing 'source'");
    assert!(result.get("size").is_some(), "missing 'size'");
    assert!(result.get("created_at").is_some(), "missing 'created_at'");
    assert!(result.get("updated_at").is_some(), "missing 'updated_at'");

    // Type checks
    assert!(result["path"].is_string());
    assert!(result["score"].is_f64() || result["score"].is_i64());
    assert!(result["matched_by"].is_array());
    assert!(result["source"].is_string());
    assert!(result["size"].is_number());
    assert!(result["created_at"].is_number());
    assert!(result["updated_at"].is_number());
  }
}

#[tokio::test]
async fn test_structured_search_across_directories() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // Structured search on "name" field — only /people has it indexed
  let (status, json) = search_request(app, &auth, serde_json::json!({
    "where": {"field": "name", "op": "eq", "value": "Alice"}
  })).await;

  assert_eq!(status, StatusCode::OK);

  let results = json["results"].as_array().unwrap();
  assert_eq!(results.len(), 1, "only one person named Alice");
  assert_eq!(results[0]["path"].as_str().unwrap(), "/people/alice.json");
}

#[tokio::test]
async fn test_structured_search_numeric_field() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // Search for products with price > 100
  let (status, json) = search_request(app, &auth, serde_json::json!({
    "where": {"field": "price", "op": "gt", "value": 100}
  })).await;

  assert_eq!(status, StatusCode::OK);

  let results = json["results"].as_array().unwrap();
  let paths: Vec<&str> = results.iter().map(|r| r["path"].as_str().unwrap()).collect();
  assert!(paths.contains(&"/products/gadget.json"), "gadget (price 500) should match, got: {:?}", paths);
  // widget (100) should NOT match "gt 100"
  assert!(!paths.contains(&"/products/widget.json"), "widget (price 100) should not match gt 100");
}

#[tokio::test]
async fn test_results_sorted_by_score_descending() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let (status, json) = search_request(app, &auth, serde_json::json!({
    "query": "Alice"
  })).await;

  assert_eq!(status, StatusCode::OK);

  let results = json["results"].as_array().unwrap();
  if results.len() >= 2 {
    let scores: Vec<f64> = results.iter()
      .map(|r| r["score"].as_f64().unwrap_or(0.0))
      .collect();
    for window in scores.windows(2) {
      assert!(
        window[0] >= window[1],
        "results must be sorted by score descending: {:?}",
        scores,
      );
    }
  }
}

#[tokio::test]
async fn test_total_count_present() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let (status, json) = search_request(app, &auth, serde_json::json!({
    "query": "Alice"
  })).await;

  assert_eq!(status, StatusCode::OK);

  // total_count should be present
  assert!(json.get("total_count").is_some(), "total_count should be present");
  let total = json["total_count"].as_u64().unwrap();
  let result_count = json["results"].as_array().unwrap().len() as u64;
  assert!(total >= result_count, "total_count ({}) should be >= result count ({})", total, result_count);
}

// ===========================================================================
// Pagination
// ===========================================================================

#[tokio::test]
async fn test_limit_restricts_results() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let (status, json) = search_request(app, &auth, serde_json::json!({
    "query": "Alice",
    "limit": 1
  })).await;

  assert_eq!(status, StatusCode::OK);

  let results = json["results"].as_array().unwrap();
  assert!(results.len() <= 1, "limit 1 should return at most 1 result, got {}", results.len());
}

#[tokio::test]
async fn test_offset_skips_results() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // First, get all results
  let app_clone = rebuild_app(&jwt_manager, &engine);
  let (_, full_json) = search_request(app_clone, &auth, serde_json::json!({
    "query": "Alice"
  })).await;
  let full_results = full_json["results"].as_array().unwrap();

  if full_results.len() >= 2 {
    // Now search with offset=1
    let (status, json) = search_request(app, &auth, serde_json::json!({
      "query": "Alice",
      "offset": 1
    })).await;

    assert_eq!(status, StatusCode::OK);
    let offset_results = json["results"].as_array().unwrap();
    assert_eq!(
      offset_results.len(),
      full_results.len() - 1,
      "offset 1 should skip one result"
    );
  }
}

#[tokio::test]
async fn test_has_more_when_more_results_exist() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // First check total count
  let app_clone = rebuild_app(&jwt_manager, &engine);
  let (_, full_json) = search_request(app_clone, &auth, serde_json::json!({
    "query": "Alice"
  })).await;
  let total = full_json["results"].as_array().unwrap().len();

  if total >= 2 {
    let (status, json) = search_request(app, &auth, serde_json::json!({
      "query": "Alice",
      "limit": 1
    })).await;

    assert_eq!(status, StatusCode::OK);
    assert!(json["has_more"].as_bool().unwrap(), "has_more should be true when more results exist");
  }
}

#[tokio::test]
async fn test_has_more_false_when_all_returned() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let (status, json) = search_request(app, &auth, serde_json::json!({
    "query": "Alice",
    "limit": 1000
  })).await;

  assert_eq!(status, StatusCode::OK);
  assert!(!json["has_more"].as_bool().unwrap(), "has_more should be false when all results fit in limit");
}

// ===========================================================================
// Path scoping
// ===========================================================================

#[tokio::test]
async fn test_path_scopes_to_subtree() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // Scope to /people — should NOT return /products results
  let (status, json) = search_request(app, &auth, serde_json::json!({
    "query": "Alice",
    "path": "/people"
  })).await;

  assert_eq!(status, StatusCode::OK);

  let results = json["results"].as_array().unwrap();
  for result in results {
    let source = result["source"].as_str().unwrap();
    assert!(
      source.starts_with("/people"),
      "scoped search should only return results from /people, got source: {}",
      source,
    );
  }
}

#[tokio::test]
async fn test_path_defaults_to_root() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // No path specified — should search everything
  let (status, json) = search_request(app, &auth, serde_json::json!({
    "query": "Alice"
  })).await;

  assert_eq!(status, StatusCode::OK);

  let results = json["results"].as_array().unwrap();
  let sources: Vec<&str> = results.iter().map(|r| r["source"].as_str().unwrap()).collect();
  // With our setup, "Alice" should match in at least the people directory
  assert!(!sources.is_empty(), "default path should search globally");
}

// ===========================================================================
// Limit clamping
// ===========================================================================

#[tokio::test]
async fn test_limit_clamped_to_max_1000() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // Request limit > 1000 — should not error, server clamps it
  let (status, _json) = search_request(app, &auth, serde_json::json!({
    "query": "Alice",
    "limit": 5000
  })).await;

  assert_eq!(status, StatusCode::OK, "limit above max should be clamped, not rejected");
}

// ===========================================================================
// Error / validation tests
// ===========================================================================

#[tokio::test]
async fn test_neither_query_nor_where_returns_400() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let (status, json) = search_request(app, &auth, serde_json::json!({})).await;

  assert_eq!(status, StatusCode::BAD_REQUEST);
  // Should have an error message
  let error_text = json.to_string();
  assert!(
    error_text.contains("query") || error_text.contains("where") || error_text.contains("required"),
    "error should mention missing query/where, got: {}",
    error_text,
  );
}

#[tokio::test]
async fn test_invalid_where_clause_returns_400() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // where clause with missing "op"
  let (status, _json) = search_request(app, &auth, serde_json::json!({
    "where": {"field": "name", "value": "Alice"}
  })).await;

  assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_invalid_json_body_returns_400() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/files/search")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from("not valid json"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert!(
    response.status() == StatusCode::BAD_REQUEST || response.status() == StatusCode::UNPROCESSABLE_ENTITY,
    "malformed JSON should be rejected, got: {}",
    response.status(),
  );
}

#[tokio::test]
async fn test_no_auth_returns_401() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);

  let request = Request::builder()
    .method("POST")
    .uri("/files/search")
    .header("content-type", "application/json")
    .body(Body::from(serde_json::to_vec(&serde_json::json!({"query": "test"})).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "missing auth should return 401");
}

#[tokio::test]
async fn test_expired_token_returns_401() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);

  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: "test-admin".to_string(),
    iss: "aeordb".to_string(),
    iat: now - 7200,
    exp: now - 3600, // expired 1 hour ago
    scope: None,
    permissions: None,
    key_id: None,
  };
  let token = jwt_manager.create_token(&claims).expect("create token");
  let auth = format!("Bearer {}", token);

  let (status, _json) = search_request(app, &auth, serde_json::json!({
    "query": "Alice"
  })).await;

  assert_eq!(status, StatusCode::UNAUTHORIZED, "expired token should return 401");
}

#[tokio::test]
async fn test_where_with_unknown_op_returns_400() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let (status, _json) = search_request(app, &auth, serde_json::json!({
    "where": {"field": "name", "op": "nonexistent_op", "value": "test"}
  })).await;

  assert_eq!(status, StatusCode::BAD_REQUEST, "unknown operator should return 400");
}

// ===========================================================================
// Empty / edge-case results
// ===========================================================================

#[tokio::test]
async fn test_no_matching_results_returns_empty() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let (status, json) = search_request(app, &auth, serde_json::json!({
    "query": "xyzzyplughnowaythismatches"
  })).await;

  assert_eq!(status, StatusCode::OK);

  let results = json["results"].as_array().unwrap();
  assert!(results.is_empty(), "nonsense query should return empty results");
  assert!(!json["has_more"].as_bool().unwrap());
}

#[tokio::test]
async fn test_no_indexed_directories_returns_empty() {
  let (_, jwt_manager, _engine, _temp_dir) = test_app();
  // Do NOT set up any directories — engine is fresh
  let app = rebuild_app(&jwt_manager, &_engine);
  let auth = bearer_token(&jwt_manager);

  let (status, json) = search_request(app, &auth, serde_json::json!({
    "query": "anything"
  })).await;

  assert_eq!(status, StatusCode::OK);

  let results = json["results"].as_array().unwrap();
  assert!(results.is_empty(), "fresh engine with no indexes should return empty");
}

#[tokio::test]
async fn test_structured_search_field_not_indexed_anywhere() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // "nonexistent_field" is not indexed in any directory
  let (status, json) = search_request(app, &auth, serde_json::json!({
    "where": {"field": "nonexistent_field", "op": "eq", "value": "anything"}
  })).await;

  assert_eq!(status, StatusCode::OK);

  let results = json["results"].as_array().unwrap();
  assert!(results.is_empty(), "querying a non-indexed field should return empty results");
}

// ===========================================================================
// Deduplication
// ===========================================================================

#[tokio::test]
async fn test_duplicate_paths_are_deduplicated() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let (status, json) = search_request(app, &auth, serde_json::json!({
    "query": "Alice"
  })).await;

  assert_eq!(status, StatusCode::OK);

  let results = json["results"].as_array().unwrap();
  let paths: Vec<&str> = results.iter().map(|r| r["path"].as_str().unwrap()).collect();

  // No path should appear more than once
  let mut unique_paths = paths.clone();
  unique_paths.sort();
  unique_paths.dedup();
  assert_eq!(
    paths.len(),
    unique_paths.len(),
    "results should not contain duplicate paths, got: {:?}",
    paths,
  );
}

// ===========================================================================
// Score and matched_by validation
// ===========================================================================

#[tokio::test]
async fn test_scores_are_positive() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let (status, json) = search_request(app, &auth, serde_json::json!({
    "query": "Alice"
  })).await;

  assert_eq!(status, StatusCode::OK);

  let results = json["results"].as_array().unwrap();
  for result in results {
    let score = result["score"].as_f64().unwrap_or(0.0);
    assert!(score > 0.0, "score should be positive, got {} for path {}", score, result["path"]);
  }
}

#[tokio::test]
async fn test_matched_by_contains_strings() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let (status, json) = search_request(app, &auth, serde_json::json!({
    "query": "Alice"
  })).await;

  assert_eq!(status, StatusCode::OK);

  let results = json["results"].as_array().unwrap();
  for result in results {
    let matched_by = result["matched_by"].as_array().unwrap();
    for entry in matched_by {
      assert!(entry.is_string(), "matched_by entries must be strings, got: {:?}", entry);
    }
  }
}

// ===========================================================================
// Combined query + where
// ===========================================================================

#[tokio::test]
async fn test_query_only_uses_broad_search() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // Only 'query' — should work via broad search (fuzzy indexes)
  let (status, json) = search_request(app, &auth, serde_json::json!({
    "query": "Bob"
  })).await;

  assert_eq!(status, StatusCode::OK);

  let results = json["results"].as_array().unwrap();
  let paths: Vec<&str> = results.iter().map(|r| r["path"].as_str().unwrap()).collect();
  assert!(paths.contains(&"/people/bob.json"), "broad search should find Bob, got: {:?}", paths);
}

#[tokio::test]
async fn test_where_only_uses_structured_search() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // Only 'where' — structured search
  let (status, json) = search_request(app, &auth, serde_json::json!({
    "where": {"field": "title", "op": "eq", "value": "Super Gadget"}
  })).await;

  assert_eq!(status, StatusCode::OK);

  let results = json["results"].as_array().unwrap();
  assert_eq!(results.len(), 1);
  assert_eq!(results[0]["path"].as_str().unwrap(), "/products/gadget.json");
}

// ===========================================================================
// Large offset beyond results
// ===========================================================================

#[tokio::test]
async fn test_offset_beyond_results_returns_empty() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let (status, json) = search_request(app, &auth, serde_json::json!({
    "query": "Alice",
    "offset": 9999
  })).await;

  assert_eq!(status, StatusCode::OK);

  let results = json["results"].as_array().unwrap();
  assert!(results.is_empty(), "offset beyond total should return empty results");
  assert!(!json["has_more"].as_bool().unwrap());
}

// ===========================================================================
// Empty string query
// ===========================================================================

#[tokio::test]
async fn test_empty_query_string() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_multi_directory(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // Empty string query — technically valid (query is provided), but should
  // not crash; behavior may be empty results or all results
  let (status, _json) = search_request(app, &auth, serde_json::json!({
    "query": ""
  })).await;

  // Should not error — either 200 with empty or 200 with results
  assert_eq!(status, StatusCode::OK, "empty query string should not cause an error");
}
