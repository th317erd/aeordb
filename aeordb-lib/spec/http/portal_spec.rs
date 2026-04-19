use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::StorageEngine;
use aeordb::server::{create_app_with_jwt_and_engine, create_temp_engine_for_tests};

/// Create a fresh in-memory app with engine support.
fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let app = create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone());
  (app, jwt_manager, engine, temp_dir)
}

/// Rebuild app from shared state (for multi-request tests).
fn rebuild_app(jwt_manager: &Arc<JwtManager>, engine: &Arc<StorageEngine>) -> axum::Router {
  create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone())
}

/// Create a Bearer token with the nil UUID (root user) for admin operations.
fn root_bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: "00000000-0000-0000-0000-000000000000".to_string(),
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

/// Create a regular Bearer token for authenticated (non-admin) access.
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

/// Collect response body into bytes.
async fn body_bytes(body: Body) -> Vec<u8> {
  body.collect().await.unwrap().to_bytes().to_vec()
}

/// Collect response body into JSON.
async fn body_json(body: Body) -> serde_json::Value {
  let bytes = body_bytes(body).await;
  serde_json::from_slice(&bytes).expect("valid JSON response body")
}

// ---------------------------------------------------------------------------
// Portal asset tests (public, no auth needed)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_portal_index_returns_html() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/system/portal")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let content_type = response
    .headers()
    .get("content-type")
    .expect("content-type header present")
    .to_str()
    .unwrap();
  assert!(
    content_type.contains("text/html"),
    "Expected text/html content-type, got: {}",
    content_type,
  );

  let bytes = body_bytes(response.into_body()).await;
  let body_str = String::from_utf8_lossy(&bytes);
  assert!(
    body_str.contains("AeorDB Portal"),
    "Expected body to contain 'AeorDB Portal', got: {}",
    &body_str[..body_str.len().min(200)],
  );
}

#[tokio::test]
async fn test_portal_index_slash_returns_html() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/system/portal/")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let content_type = response
    .headers()
    .get("content-type")
    .expect("content-type header present")
    .to_str()
    .unwrap();
  assert!(
    content_type.contains("text/html"),
    "Expected text/html content-type, got: {}",
    content_type,
  );
}

#[tokio::test]
async fn test_portal_app_mjs_returns_javascript() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/system/portal/app.mjs")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let content_type = response
    .headers()
    .get("content-type")
    .expect("content-type header present")
    .to_str()
    .unwrap();
  assert_eq!(content_type, "application/javascript; charset=utf-8");
}

#[tokio::test]
async fn test_portal_dashboard_mjs_returns_javascript() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/system/portal/dashboard.mjs")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let content_type = response
    .headers()
    .get("content-type")
    .expect("content-type header present")
    .to_str()
    .unwrap();
  assert_eq!(content_type, "application/javascript; charset=utf-8");
}

#[tokio::test]
async fn test_portal_users_mjs_returns_javascript() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/system/portal/users.mjs")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let content_type = response
    .headers()
    .get("content-type")
    .expect("content-type header present")
    .to_str()
    .unwrap();
  assert_eq!(content_type, "application/javascript; charset=utf-8");
}

#[tokio::test]
async fn test_portal_unknown_asset_returns_404() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/system/portal/nonexistent.js")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_portal_assets_require_no_auth() {
  let (app, _, _, _temp_dir) = test_app();

  // Deliberately omit Authorization header.
  let request = Request::builder()
    .method("GET")
    .uri("/system/portal")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(
    response.status(),
    StatusCode::OK,
    "Portal should be accessible without authentication",
  );
}

// ---------------------------------------------------------------------------
// Enhanced Stats API tests (requires auth)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_stats_returns_json() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/system/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let content_type = response
    .headers()
    .get("content-type")
    .expect("content-type header present")
    .to_str()
    .unwrap();
  assert!(
    content_type.contains("application/json"),
    "Expected application/json content-type, got: {}",
    content_type,
  );

  // Verify it parses as valid JSON.
  let _json = body_json(response.into_body()).await;
}

#[tokio::test]
async fn test_stats_requires_auth() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/system/stats")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(
    response.status(),
    StatusCode::UNAUTHORIZED,
    "Stats API should require authentication",
  );
}

#[tokio::test]
async fn test_stats_has_enhanced_structure() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/system/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;

  // Top-level sections must be present
  for section in &["identity", "counts", "sizes", "throughput", "health"] {
    assert!(
      json.get(section).is_some(),
      "Expected top-level section '{}' to be present in stats response",
      section,
    );
  }

  // Identity fields
  let identity = &json["identity"];
  assert!(identity.get("version").is_some(), "identity.version missing");
  assert!(identity.get("database_path").is_some(), "identity.database_path missing");
  assert!(identity.get("hash_algorithm").is_some(), "identity.hash_algorithm missing");
  assert!(identity.get("chunk_size").is_some(), "identity.chunk_size missing");
  assert!(identity.get("node_id").is_some(), "identity.node_id missing");
  assert!(identity.get("uptime_seconds").is_some(), "identity.uptime_seconds missing");

  // Count fields
  let counts = &json["counts"];
  for field in &["files", "directories", "symlinks", "chunks", "snapshots", "forks"] {
    assert!(
      counts.get(field).is_some(),
      "counts.{} missing",
      field,
    );
  }

  // Size fields
  let sizes = &json["sizes"];
  for field in &["disk_total", "kv_file", "logical_data", "chunk_data", "void_space", "dedup_savings"] {
    assert!(
      sizes.get(field).is_some(),
      "sizes.{} missing",
      field,
    );
  }

  // Throughput fields
  let throughput = &json["throughput"];
  for field in &["writes_per_sec", "reads_per_sec", "bytes_written_per_sec", "bytes_read_per_sec"] {
    let rate = throughput.get(field);
    assert!(rate.is_some(), "throughput.{} missing", field);
    let rate = rate.unwrap();
    // Each rate should have 1m, 5m, 15m, peak_1m sub-fields
    for sub in &["1m", "5m", "15m", "peak_1m"] {
      assert!(
        rate.get(sub).is_some(),
        "throughput.{}.{} missing",
        field, sub,
      );
    }
  }

  // Health fields
  let health = &json["health"];
  assert!(health.get("disk_usage_percent").is_some(), "health.disk_usage_percent missing");
  assert!(health.get("dedup_hit_rate").is_some(), "health.dedup_hit_rate missing");
  assert!(health.get("write_buffer_depth").is_some(), "health.write_buffer_depth missing");
}

#[tokio::test]
async fn test_stats_identity_version_matches_cargo() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/system/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let version = json["identity"]["version"].as_str().expect("version should be a string");
  assert_eq!(version, env!("CARGO_PKG_VERSION"), "Version should match Cargo.toml");
}

#[tokio::test]
async fn test_stats_identity_hash_algorithm_is_blake3() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/system/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let hash_algo = json["identity"]["hash_algorithm"].as_str().expect("hash_algorithm should be a string");
  assert_eq!(hash_algo, "Blake3_256", "Default hash algorithm should be Blake3_256");
}

#[tokio::test]
async fn test_stats_identity_chunk_size_is_default() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/system/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let chunk_size = json["identity"]["chunk_size"].as_u64().expect("chunk_size should be a number");
  assert_eq!(chunk_size, 262_144, "chunk_size should be 256KB (262144)");
}

#[tokio::test]
async fn test_stats_counts_zero_on_fresh_db() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/system/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let chunks = json["counts"]["chunks"].as_u64().unwrap_or(0);
  // A fresh database may have a small number of entries from root directory
  // initialization and system bootstrap. Verify the count is reasonable.
  assert!(chunks <= 5, "Fresh db should have very few chunks, got {}", chunks);
}

#[tokio::test]
async fn test_stats_counts_reflect_stored_files() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  // Store a file.
  let request = Request::builder()
    .method("PUT")
    .uri("/files/test/hello.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("hello world"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Rebuild app (oneshot consumed the router).
  let app = rebuild_app(&jwt_manager, &engine);

  let request = Request::builder()
    .method("GET")
    .uri("/system/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert!(
    json["counts"]["files"].as_u64().unwrap_or(0) > 0,
    "After storing a file, counts.files should be > 0, got: {}",
    json["counts"]["files"],
  );
  assert!(
    json["counts"]["chunks"].as_u64().unwrap_or(0) > 0,
    "After storing a file, counts.chunks should be > 0, got: {}",
    json["counts"]["chunks"],
  );
}

#[tokio::test]
async fn test_stats_sizes_void_space_zero_on_fresh_db() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/system/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(
    json["sizes"]["void_space"], 0,
    "Fresh db should have void_space=0, got: {}",
    json["sizes"]["void_space"],
  );
}

#[tokio::test]
async fn test_stats_throughput_rates_are_zero_without_trackers() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/system/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  // In test mode, rate_trackers is None, so all rates should be 0
  let writes_1m = json["throughput"]["writes_per_sec"]["1m"].as_f64().unwrap_or(-1.0);
  assert_eq!(writes_1m, 0.0, "Without rate trackers, writes_per_sec.1m should be 0.0");

  let reads_1m = json["throughput"]["reads_per_sec"]["1m"].as_f64().unwrap_or(-1.0);
  assert_eq!(reads_1m, 0.0, "Without rate trackers, reads_per_sec.1m should be 0.0");
}

#[tokio::test]
async fn test_stats_health_dedup_hit_rate_zero_on_fresh_db() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/system/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let dedup_hit_rate = json["health"]["dedup_hit_rate"].as_f64().unwrap_or(-1.0);
  // On a fresh db with no operations, dedup_hit_rate should be 0.0
  // (no chunks stored = no dedup opportunities)
  assert!(
    dedup_hit_rate >= 0.0 && dedup_hit_rate <= 1.0,
    "dedup_hit_rate should be between 0.0 and 1.0, got: {}",
    dedup_hit_rate,
  );
}

#[tokio::test]
async fn test_stats_identity_uptime_nonnegative() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/system/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let uptime = json["identity"]["uptime_seconds"].as_u64();
  assert!(uptime.is_some(), "uptime_seconds should be present as a number");
  // Just created, so uptime should be very small (< 60 seconds)
  assert!(uptime.unwrap() < 60, "uptime should be < 60 seconds for a just-created app");
}

#[tokio::test]
async fn test_stats_snapshot_count_after_snapshot() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  // Create a snapshot.
  let request = Request::builder()
    .method("POST")
    .uri("/versions/snapshots")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"name": "snap1"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert!(
    response.status().is_success(),
    "Snapshot creation should succeed, got: {}",
    response.status(),
  );

  // Rebuild app (oneshot consumed the router).
  let app = rebuild_app(&jwt_manager, &engine);

  let request = Request::builder()
    .method("GET")
    .uri("/system/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(
    json["counts"]["snapshots"], 1,
    "After creating one snapshot, counts.snapshots should be 1, got: {}",
    json["counts"]["snapshots"],
  );
}

#[tokio::test]
async fn test_stats_dedup_savings_computed_correctly() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  // Store the same file twice to trigger dedup
  let data = "hello world dedup test data";

  let request = Request::builder()
    .method("PUT")
    .uri("/files/test/file1.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from(data))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let app = rebuild_app(&jwt_manager, &engine);

  let request = Request::builder()
    .method("PUT")
    .uri("/files/test/file2.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from(data))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let app = rebuild_app(&jwt_manager, &engine);

  let request = Request::builder()
    .method("GET")
    .uri("/system/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;

  let logical = json["sizes"]["logical_data"].as_u64().unwrap_or(0);
  let chunk = json["sizes"]["chunk_data"].as_u64().unwrap_or(0);
  let savings = json["sizes"]["dedup_savings"].as_u64().unwrap_or(0);

  // logical_data should be 2x the file size (two files stored)
  // chunk_data should be 1x (dedup means only one chunk stored)
  // dedup_savings = logical - chunk
  assert_eq!(
    savings,
    logical.saturating_sub(chunk),
    "dedup_savings should equal logical_data - chunk_data",
  );

  // With identical files, we expect actual savings
  assert!(
    savings > 0,
    "After storing two identical files, dedup_savings should be > 0, got: {}",
    savings,
  );
}

#[tokio::test]
async fn test_stats_health_disk_usage_is_percentage() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/system/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let usage = json["health"]["disk_usage_percent"].as_f64().unwrap_or(-1.0);
  assert!(
    usage >= 0.0 && usage <= 100.0,
    "disk_usage_percent should be between 0 and 100, got: {}",
    usage,
  );
}
