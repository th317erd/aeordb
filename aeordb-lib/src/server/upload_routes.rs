use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::Extension;
use serde::{Deserialize, Serialize};

use crate::auth::TokenClaims;
use crate::engine::batch_commit::{commit_files, CommitFile};
use crate::engine::errors::EngineError;
use crate::engine::RequestContext;
use crate::engine::{EntryType, should_compress, CompressionAlgorithm, compress};
use crate::server::state::AppState;

/// GET /upload/config — returns hash algorithm, chunk size, and hash prefix.
pub async fn upload_config(
  State(state): State<AppState>,
) -> Response {
  let hash_algo = state.engine.hash_algo();
  let config = UploadConfig {
    hash_algorithm: format!("{:?}", hash_algo).to_lowercase(),
    chunk_size: 262_144,
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

/// PUT /upload/chunks/{hash} — upload a single chunk with hash verification.
pub async fn upload_chunk(
  State(state): State<AppState>,
  Extension(_claims): Extension<TokenClaims>,
  AxumPath(hash_hex): AxumPath<String>,
  body: axum::body::Bytes,
) -> Response {
  let chunk_size: usize = 262_144;

  if body.len() > chunk_size {
    return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
      "error": "Chunk exceeds maximum size",
      "max": chunk_size,
      "got": body.len()
    }))).into_response();
  }

  let expected_bytes = match hex::decode(&hash_hex) {
    Ok(bytes) => bytes,
    Err(_) => {
      return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
        "error": "Invalid hex hash in URL"
      }))).into_response();
    }
  };

  // Compute: blake3("chunk:" + data)
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

  // Move the I/O+fsync work to a blocking pool so we don't pin a tokio
  // worker. The engine uses std::sync locks and per-write fsync; left in an
  // async handler, multiple concurrent uploads can starve the runtime.
  let engine = state.engine.clone();
  let body_vec = body.to_vec();
  let store_result = tokio::task::spawn_blocking(move || -> Result<&'static str, crate::engine::errors::EngineError> {
    if engine.has_entry(&computed_bytes)? {
      return Ok("exists");
    }
    if should_compress(None, body_vec.len()) {
      match compress(&body_vec, CompressionAlgorithm::Zstd) {
        Ok(compressed) => {
          engine.store_entry_compressed(
            EntryType::Chunk, &computed_bytes, &compressed, CompressionAlgorithm::Zstd,
          )?;
        }
        Err(_) => {
          engine.store_entry(EntryType::Chunk, &computed_bytes, &body_vec)?;
        }
      }
    } else {
      engine.store_entry(EntryType::Chunk, &computed_bytes, &body_vec)?;
    }
    Ok("created")
  }).await;

  match store_result {
    Ok(Ok("exists")) => (StatusCode::OK, Json(serde_json::json!({
      "status": "exists", "hash": hash_hex
    }))).into_response(),
    Ok(Ok(_)) => (StatusCode::CREATED, Json(serde_json::json!({
      "status": "created", "hash": hash_hex
    }))).into_response(),
    Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
      "error": format!("Failed to store chunk: {}", e)
    }))).into_response(),
    Err(join_err) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
      "error": format!("Chunk upload task panicked: {}", join_err)
    }))).into_response(),
  }
}

#[derive(Deserialize)]
pub struct CommitRequest {
  pub files: Vec<CommitFile>,
}

/// POST /upload/commit — atomically commit multiple files from pre-uploaded chunks.
pub async fn upload_commit(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  active_key_rules: Option<Extension<crate::auth::permission_middleware::ActiveKeyRules>>,
  Json(body): Json<CommitRequest>,
) -> Response {
  // Beta-audit P0: a scoped key must not be able to commit files at paths
  // outside its scope, even via /blobs/commit. The path-level permission
  // middleware doesn't run for /blobs/* routes — enforce inline.
  if let Some(Extension(rules)) = active_key_rules.as_ref() {
    use crate::engine::api_key_rules::{match_rules, check_operation_permitted};
    for file in &body.files {
      let normalized = if file.path.starts_with('/') {
        file.path.clone()
      } else {
        format!("/{}", file.path)
      };
      let permitted = match match_rules(&rules.0, &normalized) {
        Some(rule) => check_operation_permitted(&rule.permitted, 'c')
          || check_operation_permitted(&rule.permitted, 'u'),
        None => false,
      };
      if !permitted {
        return (
          StatusCode::NOT_FOUND,
          Json(serde_json::json!({
            "error": format!("Not found: {}", file.path),
          })),
        )
          .into_response();
      }
    }
  }

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
        _ => StatusCode::INTERNAL_SERVER_ERROR,
      };
      (status, Json(serde_json::json!({ "error": e.to_string() }))).into_response()
    }
    Err(e) => {
      (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
        "error": format!("Commit task panicked: {}", e)
      }))).into_response()
    }
  }
}
