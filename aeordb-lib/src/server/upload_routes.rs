use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::Extension;
use serde::{Deserialize, Serialize};

use crate::auth::TokenClaims;
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
