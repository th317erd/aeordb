use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::Extension;
use serde::{Deserialize, Serialize};

use crate::auth::TokenClaims;
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

  // Dedup check
  match state.engine.has_entry(&computed_bytes) {
    Ok(true) => {
      return (StatusCode::OK, Json(serde_json::json!({
        "status": "exists",
        "hash": hash_hex
      }))).into_response();
    }
    _ => {}
  }

  // Store with optional compression
  let store_result = if should_compress(None, body.len()) {
    match compress(&body, CompressionAlgorithm::Zstd) {
      Ok(compressed) => state.engine.store_entry_compressed(
        EntryType::Chunk, &computed_bytes, &compressed, CompressionAlgorithm::Zstd,
      ),
      Err(_) => state.engine.store_entry(EntryType::Chunk, &computed_bytes, &body),
    }
  } else {
    state.engine.store_entry(EntryType::Chunk, &computed_bytes, &body)
  };

  match store_result {
    Ok(_) => (StatusCode::CREATED, Json(serde_json::json!({
      "status": "created", "hash": hash_hex
    }))).into_response(),
    Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
      "error": format!("Failed to store chunk: {}", e)
    }))).into_response(),
  }
}
