use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use aeordb::engine::StorageEngine;
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
  };
  let token = jwt_manager.create_token(&claims).expect("create token");
  format!("Bearer {}", token)
}

async fn body_json(body: Body) -> serde_json::Value {
  let bytes = body.collect().await.unwrap().to_bytes().to_vec();
  serde_json::from_slice(&bytes).expect("valid JSON response body")
}

fn store_index_config(engine: &StorageEngine, parent_path: &str, config: &PathIndexConfig) {
  let ops = DirectoryOps::new(engine);
  let config_path = if parent_path.ends_with('/') {
    format!("{}.config/indexes.json", parent_path)
  } else {
    format!("{}/.config/indexes.json", parent_path)
  };
  let config_data = config.serialize();
  ops.store_file(&config_path, &config_data, Some("application/json")).unwrap();
}

/// Set up a /people/ path with string + trigram + soundex + dmetaphone indexes on `name`.
fn setup_fuzzy_users(engine: &StorageEngine) {
  let ops = DirectoryOps::new(engine);

  let config = PathIndexConfig {
    indexes: vec![
      IndexFieldConfig {
        field_name: "name".to_string(),
        converter_type: "string".to_string(),
        min: None,
        max: None,
      },
      IndexFieldConfig {
        field_name: "name".to_string(),
        converter_type: "trigram".to_string(),
        min: None,
        max: None,
      },
      IndexFieldConfig {
        field_name: "name".to_string(),
        converter_type: "soundex".to_string(),
        min: None,
        max: None,
      },
      IndexFieldConfig {
        field_name: "name".to_string(),
        converter_type: "dmetaphone".to_string(),
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(engine, "/people", &config);

  let users = vec![
    ("smith.json", r#"{"name":"Smith","age":30}"#),
    ("smythe.json", r#"{"name":"Smythe","age":28}"#),
    ("schmidt.json", r#"{"name":"Schmidt","age":45}"#),
    ("martha.json", r#"{"name":"Martha","age":25}"#),
    ("restaurant.json", r#"{"name":"restaurant","age":0}"#),
    ("johnson.json", r#"{"name":"Johnson","age":50}"#),
  ];

  for (filename, json) in &users {
    ops.store_file_with_indexing(
      &format!("/people/{}", filename),
      json.as_bytes(),
      Some("application/json"),
    ).unwrap();
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_contains_query_http() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "contains",
      "value": "smi"
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json.as_array().unwrap();

  // "Smith" and "Schmidt" both contain "smi" (case-insensitive)
  let paths: Vec<&str> = results.iter().map(|r| r["path"].as_str().unwrap()).collect();
  assert!(paths.contains(&"/people/smith.json"), "should match Smith, got: {:?}", paths);
}

#[tokio::test]
async fn test_similar_query_http() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "similar",
      "value": "Smyth",
      "threshold": 0.3
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json.as_array().unwrap();
  assert!(!results.is_empty(), "similar query should return results");

  // Each result should have a score
  for result in results {
    let score = result["score"].as_f64().unwrap();
    assert!(score > 0.0, "score should be positive, got {}", score);
  }
}

#[tokio::test]
async fn test_phonetic_query_http() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "phonetic",
      "value": "Smith"
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json.as_array().unwrap();

  let paths: Vec<&str> = results.iter().map(|r| r["path"].as_str().unwrap()).collect();
  // Smith and Smythe should have similar phonetic codes
  assert!(paths.contains(&"/people/smith.json"), "should match Smith, got: {:?}", paths);
  assert!(paths.contains(&"/people/smythe.json"), "should match Smythe phonetically, got: {:?}", paths);
}

#[tokio::test]
async fn test_fuzzy_query_http_dl() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // "restarant" is 1 edit away from "restaurant"
  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "fuzzy",
      "value": "restarant"
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json.as_array().unwrap();

  let paths: Vec<&str> = results.iter().map(|r| r["path"].as_str().unwrap()).collect();
  assert!(paths.contains(&"/people/restaurant.json"), "should match restaurant, got: {:?}", paths);
}

#[tokio::test]
async fn test_fuzzy_query_http_jw() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // "Marhta" -> "Martha" via Jaro-Winkler (transposition)
  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "fuzzy",
      "value": "Marhta",
      "algorithm": "jaro_winkler"
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json.as_array().unwrap();

  let paths: Vec<&str> = results.iter().map(|r| r["path"].as_str().unwrap()).collect();
  assert!(paths.contains(&"/people/martha.json"), "should match Martha via JW, got: {:?}", paths);
}

#[tokio::test]
async fn test_match_query_http() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // "Schmidt" should find exact match + phonetic + trigram candidates
  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "match",
      "value": "Schmidt"
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json.as_array().unwrap();
  assert!(!results.is_empty(), "match query should return results");

  let paths: Vec<&str> = results.iter().map(|r| r["path"].as_str().unwrap()).collect();
  assert!(paths.contains(&"/people/schmidt.json"), "should match Schmidt exactly, got: {:?}", paths);

  // The Schmidt result should have matched_by with multiple strategies
  let schmidt_result = results.iter()
    .find(|r| r["path"].as_str().unwrap() == "/people/schmidt.json")
    .unwrap();
  let matched_by = schmidt_result["matched_by"].as_array().unwrap();
  assert!(!matched_by.is_empty(), "matched_by should not be empty for exact match");
  let strategies: Vec<&str> = matched_by.iter().map(|v| v.as_str().unwrap()).collect();
  assert!(strategies.contains(&"exact"), "should include 'exact' strategy, got: {:?}", strategies);
}

#[tokio::test]
async fn test_response_includes_score() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "contains",
      "value": "Smith"
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json.as_array().unwrap();
  assert!(!results.is_empty(), "should have at least one result");

  for result in results {
    assert!(result.get("score").is_some(), "response should include 'score' field");
    let score = result["score"].as_f64().unwrap();
    assert!(score > 0.0, "score should be positive");
    assert!(score <= 1.0, "score should be at most 1.0, got {}", score);
  }
}

#[tokio::test]
async fn test_response_includes_matched_by() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "phonetic",
      "value": "Smith"
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json.as_array().unwrap();
  assert!(!results.is_empty(), "should have at least one result");

  for result in results {
    assert!(result.get("matched_by").is_some(), "response should include 'matched_by' field");
    let matched_by = result["matched_by"].as_array().unwrap();
    assert!(!matched_by.is_empty(), "matched_by should not be empty for a match");
    // Each entry should be a string
    for entry in matched_by {
      assert!(entry.is_string(), "matched_by entries should be strings, got: {:?}", entry);
    }
  }
}

#[tokio::test]
async fn test_results_sorted_by_score() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // Use match to get results with varying scores
  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "match",
      "value": "Smith"
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json.as_array().unwrap();

  if results.len() >= 2 {
    let scores: Vec<f64> = results.iter()
      .map(|r| r["score"].as_f64().unwrap())
      .collect();
    for window in scores.windows(2) {
      assert!(
        window[0] >= window[1],
        "results should be sorted by score descending: {:?}",
        scores,
      );
    }
  }
}

#[tokio::test]
async fn test_unknown_op_returns_400() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "foobar",
      "value": "test"
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Edge case and failure path tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_contains_no_match() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "contains",
      "value": "zzzzz"
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json.as_array().unwrap();
  assert!(results.is_empty(), "no results expected for nonsense substring");
}

#[tokio::test]
async fn test_contains_requires_string_value() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "contains",
      "value": 123
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_similar_requires_string_value() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "similar",
      "value": 42
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_phonetic_requires_string_value() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "phonetic",
      "value": true
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_fuzzy_requires_string_value() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "fuzzy",
      "value": [1, 2, 3]
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_match_requires_string_value() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "match",
      "value": null
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_fuzzy_with_fixed_fuzziness() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "fuzzy",
      "value": "Smit",
      "fuzziness": 1
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json.as_array().unwrap();

  let paths: Vec<&str> = results.iter().map(|r| r["path"].as_str().unwrap()).collect();
  assert!(paths.contains(&"/people/smith.json"), "should match Smith with fuzziness 1, got: {:?}", paths);
}

#[tokio::test]
async fn test_similar_default_threshold() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // No explicit threshold — should default to 0.3
  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "similar",
      "value": "Smith"
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json.as_array().unwrap();
  assert!(!results.is_empty(), "similar with default threshold should find results");
}

#[tokio::test]
async fn test_match_no_match_returns_empty() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "match",
      "value": "xyzzyplugh"
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json.as_array().unwrap();
  assert!(results.is_empty(), "nonsense match query should return empty results");
}

#[tokio::test]
async fn test_score_and_matched_by_on_non_fuzzy_query() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // Exact eq query — score should be 1.0 and matched_by empty
  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "eq",
      "value": "Smith"
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json.as_array().unwrap();
  assert_eq!(results.len(), 1);
  assert_eq!(results[0]["score"].as_f64().unwrap(), 1.0);
  assert!(results[0]["matched_by"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_fuzzy_auto_fuzziness_string() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // fuzziness: "auto" should be accepted
  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "fuzzy",
      "value": "Smit",
      "fuzziness": "auto"
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_match_composite_returns_multiple_strategies() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_fuzzy_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // "Smith" should match Smith exactly + phonetically + trigram etc.
  let body = serde_json::json!({
    "path": "/people",
    "where": {
      "field": "name",
      "op": "match",
      "value": "Smith"
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json.as_array().unwrap();

  let smith_result = results.iter()
    .find(|r| r["path"].as_str().unwrap() == "/people/smith.json")
    .expect("Smith should be in results");

  let matched_by = smith_result["matched_by"].as_array().unwrap();
  // An exact match for "Smith" should trigger multiple strategies
  assert!(matched_by.len() >= 2,
    "exact match should trigger multiple strategies, got: {:?}", matched_by);
}
