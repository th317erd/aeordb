# Pre-Hashed Client Uploads Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement a four-phase upload protocol (negotiate → dedup check → parallel chunk upload → atomic multi-file commit) that eliminates redundant hashing and bandwidth waste.

**Architecture:** Four new HTTP endpoints under `/upload/`. The config and check endpoints are lightweight KV lookups. The chunk upload stores verified content-addressed blobs. The commit creates FileRecords and directories in a single WriteBatch pass. All existing upload paths (`PUT /engine/{path}`) continue working unchanged.

**Tech Stack:** Rust, axum, serde_json, blake3, hex

**Spec:** `bot-docs/plan/pre-hashed-uploads.md`

---

## File Structure

| Action | File | Responsibility |
|--------|------|----------------|
| Create | `aeordb-lib/src/server/upload_routes.rs` | All 4 upload endpoints: config, check, chunks, commit |
| Create | `aeordb-lib/src/engine/batch_commit.rs` | `commit_files` — atomic multi-file commit with single-pass directory propagation |
| Create | `aeordb-lib/spec/http/upload_spec.rs` | Tests for config, check, chunk upload endpoints |
| Create | `aeordb-lib/spec/http/upload_commit_spec.rs` | Tests for atomic commit endpoint |
| Create | `aeordb-lib/spec/http/upload_e2e_spec.rs` | Full round-trip integration tests |
| Modify | `aeordb-lib/src/server/mod.rs` | Add `pub mod upload_routes;` + route registration |
| Modify | `aeordb-lib/src/engine/mod.rs` | Add `pub mod batch_commit;` + re-exports |
| Modify | `aeordb-lib/src/engine/directory_ops.rs` | Export `chunk_content_hash`, `DEFAULT_CHUNK_SIZE`, `update_parent_directories` |
| Modify | `aeordb-lib/Cargo.toml` | Add test entries |

---

### Task 1: Upload Config + Dedup Check Endpoints

**Files:**
- Create: `aeordb-lib/src/server/upload_routes.rs`
- Modify: `aeordb-lib/src/server/mod.rs`
- Create: `aeordb-lib/spec/http/upload_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

This task implements the first two endpoints: `GET /upload/config` (public, no auth) and `POST /upload/check` (auth required).

- [ ] **Step 1: Create upload_routes.rs with config endpoint**

Create `aeordb-lib/src/server/upload_routes.rs`:

```rust
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::Extension;
use serde::{Deserialize, Serialize};

use crate::auth::TokenClaims;
use crate::engine::RequestContext;
use crate::server::state::AppState;

/// GET /upload/config — returns hash algorithm, chunk size, and hash prefix.
/// Public endpoint (no auth required).
pub async fn upload_config(
  State(state): State<AppState>,
) -> Response {
  let hash_algo = state.engine.hash_algo();
  let config = UploadConfig {
    hash_algorithm: format!("{:?}", hash_algo).to_lowercase(),
    chunk_size: 262_144, // DEFAULT_CHUNK_SIZE
    chunk_hash_prefix: "chunk:".to_string(),
  };
  (StatusCode::OK, Json(config)).into_response()
}

#[derive(Serialize)]
pub struct UploadConfig {
  pub hash_algorithm: String,
  pub chunk_size: usize,
  pub chunk_hash_prefix: String,
}

/// POST /upload/check — accepts a list of chunk hashes, returns which ones
/// the server already has and which it needs.
pub async fn upload_check(
  State(state): State<AppState>,
  Extension(_claims): Extension<TokenClaims>,
  Json(body): Json<CheckRequest>,
) -> Response {
  let mut have = Vec::new();
  let mut needed = Vec::new();

  for hash_hex in &body.hashes {
    let hash_bytes = match hex::decode(hash_hex) {
      Ok(bytes) => bytes,
      Err(_) => {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
          "error": format!("Invalid hex hash: {}", hash_hex)
        }))).into_response();
      }
    };

    match state.engine.has_entry(&hash_bytes) {
      Ok(true) => have.push(hash_hex.clone()),
      Ok(false) => needed.push(hash_hex.clone()),
      Err(_) => needed.push(hash_hex.clone()),
    }
  }

  (StatusCode::OK, Json(CheckResponse { have, needed })).into_response()
}

#[derive(Deserialize)]
pub struct CheckRequest {
  pub hashes: Vec<String>,
}

#[derive(Serialize)]
pub struct CheckResponse {
  pub have: Vec<String>,
  pub needed: Vec<String>,
}
```

- [ ] **Step 2: Register routes in server/mod.rs**

Add `pub mod upload_routes;` after `pub mod sse_routes;` in `aeordb-lib/src/server/mod.rs`.

Add the config endpoint to the **public routes** section (no auth):
```rust
    .route("/upload/config", get(upload_routes::upload_config))
```

Add the check endpoint to the **protected routes** section (requires auth):
```rust
    .route("/upload/check", post(upload_routes::upload_check))
```

- [ ] **Step 3: Add test entries to Cargo.toml**

```toml
[[test]]
name = "upload_spec"
path = "spec/http/upload_spec.rs"

[[test]]
name = "upload_commit_spec"
path = "spec/http/upload_commit_spec.rs"

[[test]]
name = "upload_e2e_spec"
path = "spec/http/upload_e2e_spec.rs"
```

- [ ] **Step 4: Write config + check tests**

Create `aeordb-lib/spec/http/upload_spec.rs`:

```rust
use std::sync::Arc;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine, EntryType};
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

fn root_bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: "00000000-0000-0000-0000-000000000000".to_string(),
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
  serde_json::from_slice(&bytes).expect("valid JSON")
}

// Helper: compute a chunk hash the same way the server does
fn compute_chunk_hash(data: &[u8]) -> String {
  let mut input = Vec::with_capacity(6 + data.len());
  input.extend_from_slice(b"chunk:");
  input.extend_from_slice(data);
  let hash = blake3::hash(&input);
  hex::encode(hash.as_bytes())
}

// ─── Config endpoint ────────────────────────────────────────────────────────

#[tokio::test]
async fn test_config_returns_hash_algo_and_chunk_size() {
  let (app, _, _, _temp) = test_app();

  let response = app
    .oneshot(Request::get("/upload/config").body(Body::empty()).unwrap())
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::OK);
  let json = body_json(response.into_body()).await;
  assert_eq!(json["hash_algorithm"], "blake3_256");
  assert_eq!(json["chunk_size"], 262144);
  assert_eq!(json["chunk_hash_prefix"], "chunk:");
}

#[tokio::test]
async fn test_config_no_auth_required() {
  let (app, _, _, _temp) = test_app();

  // No Authorization header
  let response = app
    .oneshot(Request::get("/upload/config").body(Body::empty()).unwrap())
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::OK);
}

// ─── Check endpoint ─────────────────────────────────────────────────────────

#[tokio::test]
async fn test_check_identifies_existing_chunks() {
  let (app, jwt, engine, _temp) = test_app();
  let token = root_bearer_token(&jwt);

  // Store a chunk directly so the server "has" it
  let chunk_data = b"hello world chunk data";
  let chunk_hash = compute_chunk_hash(chunk_data);
  let hash_bytes = hex::decode(&chunk_hash).unwrap();
  engine.store_entry(EntryType::Chunk, &hash_bytes, chunk_data).unwrap();

  // Check: server should have this hash
  let app = rebuild_app(&jwt, &engine);
  let body = serde_json::json!({ "hashes": [chunk_hash] });
  let response = app
    .oneshot(
      Request::post("/upload/check")
        .header("Authorization", &token)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::OK);
  let json = body_json(response.into_body()).await;
  assert!(json["have"].as_array().unwrap().contains(&serde_json::json!(chunk_hash)));
  assert!(json["needed"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_check_identifies_missing_chunks() {
  let (app, jwt, _, _temp) = test_app();
  let token = root_bearer_token(&jwt);

  let fake_hash = hex::encode(blake3::hash(b"nonexistent").as_bytes());
  let body = serde_json::json!({ "hashes": [fake_hash] });
  let response = app
    .oneshot(
      Request::post("/upload/check")
        .header("Authorization", &token)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::OK);
  let json = body_json(response.into_body()).await;
  assert!(json["have"].as_array().unwrap().is_empty());
  assert!(json["needed"].as_array().unwrap().contains(&serde_json::json!(fake_hash)));
}

#[tokio::test]
async fn test_check_mixed_have_and_needed() {
  let (app, jwt, engine, _temp) = test_app();
  let token = root_bearer_token(&jwt);

  // Store one chunk
  let existing_data = b"existing chunk";
  let existing_hash = compute_chunk_hash(existing_data);
  let hash_bytes = hex::decode(&existing_hash).unwrap();
  engine.store_entry(EntryType::Chunk, &hash_bytes, existing_data).unwrap();

  let missing_hash = hex::encode(blake3::hash(b"missing").as_bytes());

  let app = rebuild_app(&jwt, &engine);
  let body = serde_json::json!({ "hashes": [existing_hash, missing_hash] });
  let response = app
    .oneshot(
      Request::post("/upload/check")
        .header("Authorization", &token)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::OK);
  let json = body_json(response.into_body()).await;
  assert_eq!(json["have"].as_array().unwrap().len(), 1);
  assert_eq!(json["needed"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn test_check_empty_hash_list() {
  let (app, jwt, _, _temp) = test_app();
  let token = root_bearer_token(&jwt);

  let body = serde_json::json!({ "hashes": [] });
  let response = app
    .oneshot(
      Request::post("/upload/check")
        .header("Authorization", &token)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::OK);
  let json = body_json(response.into_body()).await;
  assert!(json["have"].as_array().unwrap().is_empty());
  assert!(json["needed"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_check_requires_auth() {
  let (app, _, _, _temp) = test_app();

  let body = serde_json::json!({ "hashes": [] });
  let response = app
    .oneshot(
      Request::post("/upload/check")
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
```

- [ ] **Step 5: Run tests**

Run: `cd /home/wyatt/Projects/aeordb && cargo test --test upload_spec -- --test-threads=1`
Expected: All 6 tests pass

- [ ] **Step 6: Commit**

```bash
cd /home/wyatt/Projects/aeordb
git add aeordb-lib/src/server/upload_routes.rs aeordb-lib/src/server/mod.rs aeordb-lib/spec/http/upload_spec.rs aeordb-lib/Cargo.toml
git commit -m "Upload Phase 1: config + dedup check endpoints — 6 tests"
```

---

### Task 2: Chunk Upload Endpoint

**Files:**
- Modify: `aeordb-lib/src/server/upload_routes.rs`
- Modify: `aeordb-lib/spec/http/upload_spec.rs`

- [ ] **Step 1: Add chunk upload endpoint to upload_routes.rs**

Add to `aeordb-lib/src/server/upload_routes.rs`:

```rust
use axum::extract::Path as AxumPath;
use crate::engine::{EntryType, should_compress, CompressionAlgorithm, compress};

/// PUT /upload/chunks/{hash} — upload a single chunk with hash verification.
pub async fn upload_chunk(
  State(state): State<AppState>,
  Extension(_claims): Extension<TokenClaims>,
  AxumPath(hash_hex): AxumPath<String>,
  body: axum::body::Bytes,
) -> Response {
  let chunk_size: usize = 262_144; // DEFAULT_CHUNK_SIZE

  // Validate size
  if body.len() > chunk_size {
    return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
      "error": "Chunk exceeds maximum size",
      "max": chunk_size,
      "got": body.len()
    }))).into_response();
  }

  // Decode expected hash
  let expected_bytes = match hex::decode(&hash_hex) {
    Ok(bytes) => bytes,
    Err(_) => {
      return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
        "error": "Invalid hex hash in URL"
      }))).into_response();
    }
  };

  // Compute actual hash: blake3("chunk:" + data)
  let mut hash_input = Vec::with_capacity(6 + body.len());
  hash_input.extend_from_slice(b"chunk:");
  hash_input.extend_from_slice(&body);
  let computed = blake3::hash(&hash_input);
  let computed_bytes = computed.as_bytes().to_vec();

  if computed_bytes != expected_bytes {
    return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
      "error": "Hash mismatch",
      "expected": hash_hex,
      "got": hex::encode(&computed_bytes)
    }))).into_response();
  }

  // Dedup: check if already exists
  match state.engine.has_entry(&computed_bytes) {
    Ok(true) => {
      return (StatusCode::OK, Json(serde_json::json!({
        "status": "exists",
        "hash": hash_hex
      }))).into_response();
    }
    _ => {}
  }

  // Store the chunk (with optional compression)
  let store_result = if should_compress(None, body.len()) {
    match compress(&body, CompressionAlgorithm::Zstd) {
      Ok(compressed) => state.engine.store_entry_compressed(
        EntryType::Chunk,
        &computed_bytes,
        &compressed,
        CompressionAlgorithm::Zstd,
      ),
      Err(_) => state.engine.store_entry(
        EntryType::Chunk,
        &computed_bytes,
        &body,
      ),
    }
  } else {
    state.engine.store_entry(
      EntryType::Chunk,
      &computed_bytes,
      &body,
    )
  };

  match store_result {
    Ok(_) => {
      (StatusCode::CREATED, Json(serde_json::json!({
        "status": "created",
        "hash": hash_hex
      }))).into_response()
    }
    Err(e) => {
      (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
        "error": format!("Failed to store chunk: {}", e)
      }))).into_response()
    }
  }
}
```

- [ ] **Step 2: Register chunk route**

In `aeordb-lib/src/server/mod.rs`, add to the protected routes:
```rust
    .route("/upload/chunks/{hash}", put(upload_routes::upload_chunk))
```

- [ ] **Step 3: Write chunk upload tests**

Add to `aeordb-lib/spec/http/upload_spec.rs`:

```rust
// ─── Chunk upload ───────────────────────────────────────────────────────────

#[tokio::test]
async fn test_chunk_upload_valid_hash() {
  let (app, jwt, _, _temp) = test_app();
  let token = root_bearer_token(&jwt);

  let chunk_data = b"test chunk data for upload";
  let chunk_hash = compute_chunk_hash(chunk_data);

  let response = app
    .oneshot(
      Request::put(&format!("/upload/chunks/{}", chunk_hash))
        .header("Authorization", &token)
        .header("Content-Type", "application/octet-stream")
        .body(Body::from(chunk_data.to_vec()))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::CREATED);
  let json = body_json(response.into_body()).await;
  assert_eq!(json["status"], "created");
}

#[tokio::test]
async fn test_chunk_upload_hash_mismatch() {
  let (app, jwt, _, _temp) = test_app();
  let token = root_bearer_token(&jwt);

  let fake_hash = hex::encode(blake3::hash(b"wrong").as_bytes());
  let response = app
    .oneshot(
      Request::put(&format!("/upload/chunks/{}", fake_hash))
        .header("Authorization", &token)
        .header("Content-Type", "application/octet-stream")
        .body(Body::from(b"actual data".to_vec()))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
  let json = body_json(response.into_body()).await;
  assert!(json["error"].as_str().unwrap().contains("Hash mismatch"));
}

#[tokio::test]
async fn test_chunk_upload_too_large() {
  let (app, jwt, _, _temp) = test_app();
  let token = root_bearer_token(&jwt);

  let oversized = vec![0u8; 262_145]; // 1 byte over limit
  let chunk_hash = compute_chunk_hash(&oversized);

  let response = app
    .oneshot(
      Request::put(&format!("/upload/chunks/{}", chunk_hash))
        .header("Authorization", &token)
        .header("Content-Type", "application/octet-stream")
        .body(Body::from(oversized))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
  let json = body_json(response.into_body()).await;
  assert!(json["error"].as_str().unwrap().contains("maximum size"));
}

#[tokio::test]
async fn test_chunk_upload_dedup() {
  let (app, jwt, engine, _temp) = test_app();
  let token = root_bearer_token(&jwt);

  let chunk_data = b"dedup test data";
  let chunk_hash = compute_chunk_hash(chunk_data);

  // First upload: 201
  let response = app
    .oneshot(
      Request::put(&format!("/upload/chunks/{}", chunk_hash))
        .header("Authorization", &token)
        .header("Content-Type", "application/octet-stream")
        .body(Body::from(chunk_data.to_vec()))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Second upload: 200 (already exists)
  let app = rebuild_app(&jwt, &engine);
  let response = app
    .oneshot(
      Request::put(&format!("/upload/chunks/{}", chunk_hash))
        .header("Authorization", &token)
        .header("Content-Type", "application/octet-stream")
        .body(Body::from(chunk_data.to_vec()))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let json = body_json(response.into_body()).await;
  assert_eq!(json["status"], "exists");
}

#[tokio::test]
async fn test_chunk_upload_empty_chunk() {
  let (app, jwt, _, _temp) = test_app();
  let token = root_bearer_token(&jwt);

  let chunk_data = b"";
  let chunk_hash = compute_chunk_hash(chunk_data);

  let response = app
    .oneshot(
      Request::put(&format!("/upload/chunks/{}", chunk_hash))
        .header("Authorization", &token)
        .header("Content-Type", "application/octet-stream")
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn test_chunk_upload_requires_auth() {
  let (app, _, _, _temp) = test_app();

  let chunk_data = b"no auth";
  let chunk_hash = compute_chunk_hash(chunk_data);

  let response = app
    .oneshot(
      Request::put(&format!("/upload/chunks/{}", chunk_hash))
        .header("Content-Type", "application/octet-stream")
        .body(Body::from(chunk_data.to_vec()))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
```

- [ ] **Step 4: Run tests**

Run: `cd /home/wyatt/Projects/aeordb && cargo test --test upload_spec -- --test-threads=1`
Expected: All 12 tests pass (6 from Task 1 + 6 new)

- [ ] **Step 5: Commit**

```bash
cd /home/wyatt/Projects/aeordb
git add aeordb-lib/src/server/upload_routes.rs aeordb-lib/src/server/mod.rs aeordb-lib/spec/http/upload_spec.rs
git commit -m "Upload Phase 2: chunk upload with hash verification — 12 tests"
```

---

### Task 3: Batch Commit Engine Function

**Files:**
- Create: `aeordb-lib/src/engine/batch_commit.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`
- Modify: `aeordb-lib/src/engine/directory_ops.rs` (export helpers)

This task builds the core commit logic as an engine function — no HTTP yet. The HTTP endpoint (Task 4) calls this.

- [ ] **Step 1: Export chunk_content_hash and DEFAULT_CHUNK_SIZE from directory_ops**

In `aeordb-lib/src/engine/directory_ops.rs`, change `chunk_content_hash` from `fn` to `pub fn` and `DEFAULT_CHUNK_SIZE` from `const` to `pub const`.

- [ ] **Step 2: Create batch_commit.rs**

Create `aeordb-lib/src/engine/batch_commit.rs`:

```rust
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::engine::directory_entry::{ChildEntry, deserialize_child_entries, serialize_child_entries};
use crate::engine::directory_ops::{directory_content_hash, directory_path_hash, file_path_hash};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::path_utils::{normalize_path, parent_path, file_name};
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::{StorageEngine, WriteBatch};
use crate::engine::engine_event::{EntryEventData, EVENT_ENTRIES_CREATED};

/// A single file in a commit changeset.
#[derive(Debug, Clone, Deserialize)]
pub struct CommitFile {
  pub path: String,
  pub chunks: Vec<String>,  // hex-encoded chunk hashes
  #[serde(default)]
  pub content_type: Option<String>,
}

/// Result of a successful commit.
#[derive(Debug, Clone, Serialize)]
pub struct CommitResult {
  pub committed: usize,
  pub files: Vec<CommittedFile>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommittedFile {
  pub path: String,
  pub size: u64,
}

/// Atomically commit multiple files from pre-uploaded chunks.
///
/// 1. Validate all chunk hashes exist.
/// 2. Create FileRecords from chunk hash lists.
/// 3. Update directories in a single pass (each dir updated once).
/// 4. Update HEAD once.
///
/// Uses WriteBatch for atomicity — everything lands or nothing does.
pub fn commit_files(
  engine: &StorageEngine,
  ctx: &RequestContext,
  files: Vec<CommitFile>,
) -> EngineResult<CommitResult> {
  if files.is_empty() {
    return Err(EngineError::InvalidInput("No files in commit".to_string()));
  }

  let algo = engine.hash_algo();
  let hash_length = algo.hash_length();

  // Phase 1: Validate all chunk hashes exist and compute sizes
  let mut file_records: Vec<(String, FileRecord, Vec<Vec<u8>>)> = Vec::new();
  let mut missing_chunks: Vec<String> = Vec::new();

  for file in &files {
    let normalized = normalize_path(&file.path);
    let mut chunk_hashes: Vec<Vec<u8>> = Vec::new();
    let mut total_size: u64 = 0;

    for chunk_hex in &file.chunks {
      let chunk_bytes = hex::decode(chunk_hex)
        .map_err(|_| EngineError::InvalidInput(format!("Invalid hex hash: {}", chunk_hex)))?;

      // Verify chunk exists
      match engine.get_entry(&chunk_bytes)? {
        Some((header, _key, value)) => {
          total_size += value.len() as u64;
          chunk_hashes.push(chunk_bytes);
        }
        None => {
          missing_chunks.push(chunk_hex.clone());
        }
      }
    }

    if missing_chunks.is_empty() {
      // Detect content type from first chunk if not provided
      let content_type = match &file.content_type {
        Some(ct) => ct.clone(),
        None => {
          // Read first chunk to detect content type
          if let Some(first_hash) = chunk_hashes.first() {
            if let Some((_h, _k, value)) = engine.get_entry(first_hash)? {
              crate::engine::content_type::detect_content_type(&value, None)
            } else {
              "application/octet-stream".to_string()
            }
          } else {
            "application/octet-stream".to_string()
          }
        }
      };

      // Check for existing file (preserve created_at on overwrite)
      let file_key = file_path_hash(&normalized, &algo)?;
      let existing_created_at = match engine.get_entry(&file_key)? {
        Some((_h, _k, value)) => {
          let existing = FileRecord::deserialize(&value, hash_length)?;
          Some(existing.created_at)
        }
        None => None,
      };

      let mut record = FileRecord::new(
        normalized.clone(),
        Some(content_type),
        total_size,
        chunk_hashes.clone(),
      );

      if let Some(created_at) = existing_created_at {
        record.created_at = created_at;
      }

      file_records.push((normalized, record, chunk_hashes));
    }
  }

  if !missing_chunks.is_empty() {
    return Err(EngineError::InvalidInput(
      format!("Missing chunks: {}", missing_chunks.join(", "))
    ));
  }

  // Phase 2: Store FileRecords
  let mut batch = WriteBatch::new();
  let mut committed_files: Vec<CommittedFile> = Vec::new();

  // Collect all child entries grouped by parent directory
  let mut dir_children: HashMap<String, Vec<ChildEntry>> = HashMap::new();

  for (normalized, record, _chunk_hashes) in &file_records {
    let file_key = file_path_hash(normalized, &algo)?;
    let file_value = record.serialize(hash_length);

    batch.add(EntryType::FileRecord, file_key.clone(), file_value);

    committed_files.push(CommittedFile {
      path: normalized.clone(),
      size: record.total_size,
    });

    // Build child entry for directory
    let child = ChildEntry {
      entry_type: EntryType::FileRecord.to_u8(),
      hash: file_key,
      total_size: record.total_size,
      created_at: record.created_at,
      updated_at: record.updated_at,
      name: file_name(normalized).unwrap_or("").to_string(),
      content_type: record.content_type.clone(),
    };

    let parent = parent_path(normalized).unwrap_or("/".to_string());
    dir_children.entry(parent).or_default().push(child);
  }

  // Phase 3: Flush FileRecords batch first so they exist in KV
  engine.flush_batch(batch)?;

  // Phase 4: Single-pass directory propagation
  // Process directories bottom-up (deepest first)
  let mut dir_paths: Vec<String> = dir_children.keys().cloned().collect();
  dir_paths.sort_by(|a, b| b.matches('/').count().cmp(&a.matches('/').count()));

  for dir_path in &dir_paths {
    let children = dir_children.get(dir_path).unwrap();
    let dir_key = directory_path_hash(dir_path, &algo)?;

    // Read existing directory
    let existing = engine.get_entry(&dir_key)?;

    let (dir_value, content_key) = match existing {
      Some((_h, _k, value)) if !value.is_empty() && crate::engine::btree::is_btree_format(&value) => {
        // B-tree: insert each child
        let mut current_data = value;
        let mut current_hash = dir_key.clone(); // will be overwritten
        for child in children {
          let (new_hash, new_data) = crate::engine::btree::btree_insert_batched(
            engine, &current_data, child.clone(), hash_length, &algo
          )?;
          current_data = new_data;
          current_hash = new_hash;
        }
        (current_data, current_hash)
      }
      Some((_h, _k, value)) => {
        // Flat: merge children
        let mut existing_children = if value.is_empty() {
          Vec::new()
        } else {
          deserialize_child_entries(&value, hash_length)?
        };

        for child in children {
          if let Some(existing) = existing_children.iter_mut().find(|c| c.name == child.name) {
            *existing = child.clone();
          } else {
            existing_children.push(child.clone());
          }
        }

        // Check B-tree conversion
        if existing_children.len() >= crate::engine::btree::BTREE_CONVERSION_THRESHOLD {
          let root_hash = crate::engine::btree::btree_from_entries(
            engine, existing_children, hash_length, &algo
          )?;
          let root_entry = engine.get_entry(&root_hash)?
            .ok_or_else(|| EngineError::NotFound("B-tree root not found".to_string()))?;
          (root_entry.2, root_hash)
        } else {
          let dir_value = serialize_child_entries(&existing_children, hash_length);
          let content_key = directory_content_hash(&dir_value, &algo)?;
          engine.store_entry(EntryType::DirectoryIndex, &content_key, &dir_value)?;
          (dir_value, content_key)
        }
      }
      None => {
        // New directory
        let dir_value = serialize_child_entries(children, hash_length);
        let content_key = directory_content_hash(&dir_value, &algo)?;
        engine.store_entry(EntryType::DirectoryIndex, &content_key, &dir_value)?;
        (dir_value, content_key)
      }
    };

    // Store at path key
    engine.store_entry(EntryType::DirectoryIndex, &dir_key, &dir_value)?;

    // If root, update HEAD
    if dir_path == "/" {
      engine.update_head(&content_key)?;
    } else {
      // Propagate this directory as a child of its parent
      let dir_child = ChildEntry {
        entry_type: EntryType::DirectoryIndex.to_u8(),
        hash: dir_key.clone(),
        total_size: 0,
        created_at: chrono::Utc::now().timestamp_millis(),
        updated_at: chrono::Utc::now().timestamp_millis(),
        name: file_name(dir_path).unwrap_or("").to_string(),
        content_type: None,
      };

      let grandparent = parent_path(dir_path).unwrap_or("/".to_string());
      dir_children.entry(grandparent.clone()).or_default().push(dir_child);

      // If grandparent wasn't already in our list, add it for processing
      if !dir_paths.contains(&grandparent) {
        dir_paths.push(grandparent);
      }
    }
  }

  // Emit event
  let event_entries: Vec<serde_json::Value> = committed_files.iter().map(|f| {
    serde_json::json!({
      "path": f.path,
      "entry_type": "file",
      "size": f.size,
    })
  }).collect();
  ctx.emit(EVENT_ENTRIES_CREATED, serde_json::json!({"entries": event_entries}));

  Ok(CommitResult {
    committed: committed_files.len(),
    files: committed_files,
  })
}
```

- [ ] **Step 3: Add to mod.rs**

Add `pub mod batch_commit;` in `aeordb-lib/src/engine/mod.rs`.

Add re-exports:
```rust
pub use batch_commit::{commit_files, CommitFile, CommitResult, CommittedFile};
```

- [ ] **Step 4: Add InvalidInput error variant**

Check if `EngineError::InvalidInput` exists. If not, add to `aeordb-lib/src/engine/errors.rs`:
```rust
  InvalidInput(String),
```

And add the Display match arm.

- [ ] **Step 5: Run compilation check**

Run: `cd /home/wyatt/Projects/aeordb && cargo check -p aeordb`
Expected: Compiles (no tests yet for this module — tests come in Task 4 via HTTP)

- [ ] **Step 6: Commit**

```bash
cd /home/wyatt/Projects/aeordb
git add aeordb-lib/src/engine/batch_commit.rs aeordb-lib/src/engine/mod.rs aeordb-lib/src/engine/directory_ops.rs aeordb-lib/src/engine/errors.rs
git commit -m "Upload Phase 3: batch_commit engine function — atomic multi-file commit"
```

---

### Task 4: Commit HTTP Endpoint + Tests

**Files:**
- Modify: `aeordb-lib/src/server/upload_routes.rs`
- Modify: `aeordb-lib/src/server/mod.rs`
- Create: `aeordb-lib/spec/http/upload_commit_spec.rs`

- [ ] **Step 1: Add commit endpoint to upload_routes.rs**

Add to `aeordb-lib/src/server/upload_routes.rs`:

```rust
use crate::engine::batch_commit::{commit_files, CommitFile};
use crate::engine::is_root;

#[derive(Deserialize)]
pub struct CommitRequest {
  pub files: Vec<CommitFile>,
}

/// POST /upload/commit — atomically commit multiple files from pre-uploaded chunks.
pub async fn upload_commit(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Json(body): Json<CommitRequest>,
) -> Response {
  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());

  let engine = state.engine.clone();
  let result = tokio::task::spawn_blocking(move || {
    commit_files(&engine, &ctx, body.files)
  }).await;

  match result {
    Ok(Ok(commit_result)) => {
      (StatusCode::OK, Json(serde_json::json!(commit_result))).into_response()
    }
    Ok(Err(e)) => {
      let status = match &e {
        EngineError::InvalidInput(_) => StatusCode::BAD_REQUEST,
        EngineError::PermissionDenied(_) => StatusCode::FORBIDDEN,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
      };
      let error_msg = e.to_string();

      // If missing chunks, extract the list
      if error_msg.starts_with("Missing chunks:") {
        let chunks: Vec<&str> = error_msg
          .trim_start_matches("Missing chunks: ")
          .split(", ")
          .collect();
        return (status, Json(serde_json::json!({
          "error": "Missing chunks",
          "missing": chunks
        }))).into_response();
      }

      (status, Json(serde_json::json!({ "error": error_msg }))).into_response()
    }
    Err(e) => {
      (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
        "error": format!("Commit task panicked: {}", e)
      }))).into_response()
    }
  }
}
```

Add the EngineError import at the top of the file:
```rust
use crate::engine::errors::EngineError;
```

- [ ] **Step 2: Register commit route**

In `aeordb-lib/src/server/mod.rs`, add to the protected routes:
```rust
    .route("/upload/commit", post(upload_routes::upload_commit))
```

- [ ] **Step 3: Write commit tests**

Create `aeordb-lib/spec/http/upload_commit_spec.rs`:

```rust
use std::sync::Arc;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine, EntryType};
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

fn root_bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: "00000000-0000-0000-0000-000000000000".to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + DEFAULT_EXPIRY_SECONDS,
    scope: None,
    permissions: None,
  };
  format!("Bearer {}", jwt_manager.create_token(&claims).unwrap())
}

async fn body_json(body: Body) -> serde_json::Value {
  let bytes = body.collect().await.unwrap().to_bytes().to_vec();
  serde_json::from_slice(&bytes).expect("valid JSON")
}

fn compute_chunk_hash(data: &[u8]) -> String {
  let mut input = Vec::with_capacity(6 + data.len());
  input.extend_from_slice(b"chunk:");
  input.extend_from_slice(data);
  hex::encode(blake3::hash(&input).as_bytes())
}

/// Upload a chunk via the API and return its hash.
async fn upload_chunk(app: axum::Router, token: &str, data: &[u8]) -> String {
  let hash = compute_chunk_hash(data);
  let response = app
    .oneshot(
      Request::put(&format!("/upload/chunks/{}", hash))
        .header("Authorization", token)
        .header("Content-Type", "application/octet-stream")
        .body(Body::from(data.to_vec()))
        .unwrap(),
    )
    .await
    .unwrap();
  assert!(response.status().is_success(), "chunk upload failed: {}", response.status());
  hash
}

// ─── Commit tests ───────────────────────────────────────────────────────────

#[tokio::test]
async fn test_commit_single_file() {
  let (app, jwt, engine, _temp) = test_app();
  let token = root_bearer_token(&jwt);

  // Upload chunks
  let h1 = upload_chunk(rebuild_app(&jwt, &engine), &token, b"chunk one data").await;
  let h2 = upload_chunk(rebuild_app(&jwt, &engine), &token, b"chunk two data").await;

  // Commit
  let body = serde_json::json!({
    "files": [{ "path": "/test.txt", "chunks": [h1, h2], "content_type": "text/plain" }]
  });

  let app = rebuild_app(&jwt, &engine);
  let response = app
    .oneshot(
      Request::post("/upload/commit")
        .header("Authorization", &token)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::OK);
  let json = body_json(response.into_body()).await;
  assert_eq!(json["committed"], 1);

  // Verify file is readable via normal GET
  let app = rebuild_app(&jwt, &engine);
  let response = app
    .oneshot(
      Request::get("/engine/test.txt")
        .header("Authorization", &token)
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_commit_multiple_files() {
  let (app, jwt, engine, _temp) = test_app();
  let token = root_bearer_token(&jwt);

  let h1 = upload_chunk(rebuild_app(&jwt, &engine), &token, b"file a data").await;
  let h2 = upload_chunk(rebuild_app(&jwt, &engine), &token, b"file b data").await;
  let h3 = upload_chunk(rebuild_app(&jwt, &engine), &token, b"file c data").await;

  let body = serde_json::json!({
    "files": [
      { "path": "/data/a.txt", "chunks": [h1], "content_type": "text/plain" },
      { "path": "/data/b.txt", "chunks": [h2], "content_type": "text/plain" },
      { "path": "/data/c.txt", "chunks": [h3], "content_type": "text/plain" }
    ]
  });

  let app = rebuild_app(&jwt, &engine);
  let response = app
    .oneshot(
      Request::post("/upload/commit")
        .header("Authorization", &token)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::OK);
  let json = body_json(response.into_body()).await;
  assert_eq!(json["committed"], 3);
}

#[tokio::test]
async fn test_commit_missing_chunks() {
  let (app, jwt, _, _temp) = test_app();
  let token = root_bearer_token(&jwt);

  let fake_hash = hex::encode(blake3::hash(b"nonexistent").as_bytes());
  let body = serde_json::json!({
    "files": [{ "path": "/test.txt", "chunks": [fake_hash], "content_type": "text/plain" }]
  });

  let response = app
    .oneshot(
      Request::post("/upload/commit")
        .header("Authorization", &token)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
  let json = body_json(response.into_body()).await;
  assert!(json["error"].as_str().unwrap().contains("Missing chunks"));
}

#[tokio::test]
async fn test_commit_empty_files_list() {
  let (app, jwt, _, _temp) = test_app();
  let token = root_bearer_token(&jwt);

  let body = serde_json::json!({ "files": [] });
  let response = app
    .oneshot(
      Request::post("/upload/commit")
        .header("Authorization", &token)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_commit_empty_file_zero_chunks() {
  let (app, jwt, engine, _temp) = test_app();
  let token = root_bearer_token(&jwt);

  let body = serde_json::json!({
    "files": [{ "path": "/empty.txt", "chunks": [], "content_type": "text/plain" }]
  });

  let app = rebuild_app(&jwt, &engine);
  let response = app
    .oneshot(
      Request::post("/upload/commit")
        .header("Authorization", &token)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::OK);
  let json = body_json(response.into_body()).await;
  assert_eq!(json["committed"], 1);
}

#[tokio::test]
async fn test_commit_preserves_chunk_order() {
  let (app, jwt, engine, _temp) = test_app();
  let token = root_bearer_token(&jwt);

  // Upload chunks with distinct content
  let h1 = upload_chunk(rebuild_app(&jwt, &engine), &token, b"AAAA first chunk").await;
  let h2 = upload_chunk(rebuild_app(&jwt, &engine), &token, b"BBBB second chunk").await;

  // Commit with specific order
  let body = serde_json::json!({
    "files": [{ "path": "/ordered.bin", "chunks": [h1, h2], "content_type": "application/octet-stream" }]
  });

  let app = rebuild_app(&jwt, &engine);
  let response = app
    .oneshot(
      Request::post("/upload/commit")
        .header("Authorization", &token)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  // Read the file back and verify content order
  let app = rebuild_app(&jwt, &engine);
  let response = app
    .oneshot(
      Request::get("/engine/ordered.bin")
        .header("Authorization", &token)
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
  assert!(body_bytes.starts_with(b"AAAA"), "first chunk should come first");
}

#[tokio::test]
async fn test_commit_requires_auth() {
  let (app, _, _, _temp) = test_app();

  let body = serde_json::json!({ "files": [] });
  let response = app
    .oneshot(
      Request::post("/upload/commit")
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
```

- [ ] **Step 4: Run tests**

Run: `cd /home/wyatt/Projects/aeordb && cargo test --test upload_commit_spec -- --test-threads=1`
Expected: All 7 tests pass

- [ ] **Step 5: Run full suite**

Run: `cd /home/wyatt/Projects/aeordb && cargo test -- --test-threads=4`
Expected: All tests pass (no regressions)

- [ ] **Step 6: Commit**

```bash
cd /home/wyatt/Projects/aeordb
git add aeordb-lib/src/server/upload_routes.rs aeordb-lib/src/server/mod.rs aeordb-lib/spec/http/upload_commit_spec.rs
git commit -m "Upload Phase 4: atomic commit endpoint — 7 tests"
```

---

### Task 5: E2E Integration Tests

**Files:**
- Create: `aeordb-lib/spec/http/upload_e2e_spec.rs`

- [ ] **Step 1: Write full round-trip tests**

Create `aeordb-lib/spec/http/upload_e2e_spec.rs`:

```rust
use std::sync::Arc;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine};
use aeordb::engine::gc::run_gc;
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

fn root_bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: "00000000-0000-0000-0000-000000000000".to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + DEFAULT_EXPIRY_SECONDS,
    scope: None,
    permissions: None,
  };
  format!("Bearer {}", jwt_manager.create_token(&claims).unwrap())
}

async fn body_json(body: Body) -> serde_json::Value {
  let bytes = body.collect().await.unwrap().to_bytes().to_vec();
  serde_json::from_slice(&bytes).expect("valid JSON")
}

fn compute_chunk_hash(data: &[u8]) -> String {
  let mut input = Vec::with_capacity(6 + data.len());
  input.extend_from_slice(b"chunk:");
  input.extend_from_slice(data);
  hex::encode(blake3::hash(&input).as_bytes())
}

async fn upload_chunk(app: axum::Router, token: &str, data: &[u8]) -> String {
  let hash = compute_chunk_hash(data);
  let resp = app.oneshot(
    Request::put(&format!("/upload/chunks/{}", hash))
      .header("Authorization", token)
      .header("Content-Type", "application/octet-stream")
      .body(Body::from(data.to_vec())).unwrap()
  ).await.unwrap();
  assert!(resp.status().is_success());
  hash
}

// ─── Full round-trip ────────────────────────────────────────────────────────

#[tokio::test]
async fn test_full_round_trip_config_check_upload_commit_read() {
  let (app, jwt, engine, _temp) = test_app();
  let token = root_bearer_token(&jwt);

  // 1. Config
  let resp = rebuild_app(&jwt, &engine).oneshot(
    Request::get("/upload/config").body(Body::empty()).unwrap()
  ).await.unwrap();
  let config = body_json(resp.into_body()).await;
  assert_eq!(config["chunk_hash_prefix"], "chunk:");

  // 2. Client hashes locally (simulated)
  let data_a = b"Hello, this is file A content!";
  let data_b = b"And this is file B content!";
  let hash_a = compute_chunk_hash(data_a);
  let hash_b = compute_chunk_hash(data_b);

  // 3. Check
  let check_body = serde_json::json!({ "hashes": [hash_a, hash_b] });
  let resp = rebuild_app(&jwt, &engine).oneshot(
    Request::post("/upload/check")
      .header("Authorization", &token)
      .header("Content-Type", "application/json")
      .body(Body::from(serde_json::to_string(&check_body).unwrap())).unwrap()
  ).await.unwrap();
  let check = body_json(resp.into_body()).await;
  assert_eq!(check["needed"].as_array().unwrap().len(), 2);

  // 4a. Upload missing chunks
  upload_chunk(rebuild_app(&jwt, &engine), &token, data_a).await;
  upload_chunk(rebuild_app(&jwt, &engine), &token, data_b).await;

  // 4b. Commit
  let commit_body = serde_json::json!({
    "files": [
      { "path": "/docs/a.txt", "chunks": [hash_a], "content_type": "text/plain" },
      { "path": "/docs/b.txt", "chunks": [hash_b], "content_type": "text/plain" }
    ]
  });
  let resp = rebuild_app(&jwt, &engine).oneshot(
    Request::post("/upload/commit")
      .header("Authorization", &token)
      .header("Content-Type", "application/json")
      .body(Body::from(serde_json::to_string(&commit_body).unwrap())).unwrap()
  ).await.unwrap();
  assert_eq!(resp.status(), StatusCode::OK);
  let result = body_json(resp.into_body()).await;
  assert_eq!(result["committed"], 2);

  // 5. Read back via normal GET
  let resp = rebuild_app(&jwt, &engine).oneshot(
    Request::get("/engine/docs/a.txt")
      .header("Authorization", &token)
      .body(Body::empty()).unwrap()
  ).await.unwrap();
  assert_eq!(resp.status(), StatusCode::OK);
  let body = resp.into_body().collect().await.unwrap().to_bytes();
  assert_eq!(&body[..], data_a);
}

#[tokio::test]
async fn test_incremental_upload_only_new_chunks() {
  let (app, jwt, engine, _temp) = test_app();
  let token = root_bearer_token(&jwt);

  // Upload and commit a file
  let shared_chunk = b"shared chunk data";
  let h_shared = upload_chunk(rebuild_app(&jwt, &engine), &token, shared_chunk).await;

  let commit_body = serde_json::json!({
    "files": [{ "path": "/v1.txt", "chunks": [h_shared], "content_type": "text/plain" }]
  });
  let resp = rebuild_app(&jwt, &engine).oneshot(
    Request::post("/upload/commit")
      .header("Authorization", &token)
      .header("Content-Type", "application/json")
      .body(Body::from(serde_json::to_string(&commit_body).unwrap())).unwrap()
  ).await.unwrap();
  assert_eq!(resp.status(), StatusCode::OK);

  // Now "upload v2" that shares a chunk
  let new_chunk = b"new chunk for v2";
  let h_new = compute_chunk_hash(new_chunk);

  // Check: server should already have h_shared
  let check_body = serde_json::json!({ "hashes": [h_shared, h_new] });
  let resp = rebuild_app(&jwt, &engine).oneshot(
    Request::post("/upload/check")
      .header("Authorization", &token)
      .header("Content-Type", "application/json")
      .body(Body::from(serde_json::to_string(&check_body).unwrap())).unwrap()
  ).await.unwrap();
  let check = body_json(resp.into_body()).await;
  assert_eq!(check["have"].as_array().unwrap().len(), 1, "should have the shared chunk");
  assert_eq!(check["needed"].as_array().unwrap().len(), 1, "should need only the new chunk");

  // Upload only the new chunk
  upload_chunk(rebuild_app(&jwt, &engine), &token, new_chunk).await;

  // Commit v2 using both chunks
  let commit_body = serde_json::json!({
    "files": [{ "path": "/v2.txt", "chunks": [h_shared, h_new], "content_type": "text/plain" }]
  });
  let resp = rebuild_app(&jwt, &engine).oneshot(
    Request::post("/upload/commit")
      .header("Authorization", &token)
      .header("Content-Type", "application/json")
      .body(Body::from(serde_json::to_string(&commit_body).unwrap())).unwrap()
  ).await.unwrap();
  assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_gc_collects_uncommitted_chunks() {
  let (_app, jwt, engine, _temp) = test_app();
  let token = root_bearer_token(&jwt);

  // Upload a chunk but never commit it
  let orphan_data = b"orphaned chunk data";
  upload_chunk(rebuild_app(&jwt, &engine), &token, orphan_data).await;

  // Run GC
  let ctx = RequestContext::system();
  let result = run_gc(&engine, &ctx, false).unwrap();

  // The orphan chunk should be collected as garbage
  assert!(result.garbage_entries > 0, "GC should collect the orphaned chunk");
}

#[tokio::test]
async fn test_commit_file_matches_regular_put() {
  let (_app, jwt, engine, _temp) = test_app();
  let token = root_bearer_token(&jwt);

  let file_content = b"identical content test";

  // Upload via regular PUT
  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();
  ops.store_file(&ctx, "/regular.txt", file_content, Some("text/plain")).unwrap();

  // Upload via chunked protocol
  let h1 = upload_chunk(rebuild_app(&jwt, &engine), &token, file_content).await;
  let commit_body = serde_json::json!({
    "files": [{ "path": "/chunked.txt", "chunks": [h1], "content_type": "text/plain" }]
  });
  let resp = rebuild_app(&jwt, &engine).oneshot(
    Request::post("/upload/commit")
      .header("Authorization", &token)
      .header("Content-Type", "application/json")
      .body(Body::from(serde_json::to_string(&commit_body).unwrap())).unwrap()
  ).await.unwrap();
  assert_eq!(resp.status(), StatusCode::OK);

  // Read both back
  let regular = ops.read_file("/regular.txt").unwrap();
  let chunked = ops.read_file("/chunked.txt").unwrap();
  assert_eq!(regular, chunked, "regular PUT and chunked commit should produce identical content");
}
```

- [ ] **Step 2: Run E2E tests**

Run: `cd /home/wyatt/Projects/aeordb && cargo test --test upload_e2e_spec -- --test-threads=1`
Expected: All 4 tests pass

- [ ] **Step 3: Run full suite**

Run: `cd /home/wyatt/Projects/aeordb && cargo test -- --test-threads=4`
Expected: All tests pass

- [ ] **Step 4: Commit**

```bash
cd /home/wyatt/Projects/aeordb
git add aeordb-lib/spec/http/upload_e2e_spec.rs
git commit -m "Upload Phase 5: E2E integration tests — 4 tests"
```

---

## Post-Implementation Checklist

- [ ] Update `.claude/TODO.md` — add "Completed: Pre-Hashed Client Uploads" with test count
- [ ] Update `.claude/DETAILS.md` — add upload_routes.rs and batch_commit.rs to key files
- [ ] Run: `cargo test -- --test-threads=4` — all tests pass
- [ ] Run: `cargo build -p aeordb-cli` — CLI compiles
- [ ] E2E curl test against real server (config → check → upload → commit → read)
