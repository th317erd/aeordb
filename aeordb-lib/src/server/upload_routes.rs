use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::Extension;
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::auth::TokenClaims;
use crate::engine::batch_commit::{commit_files, CommitFile};
use crate::engine::errors::EngineError;
use crate::engine::RequestContext;
use crate::engine::EntryType;
use crate::server::blocking::run_engine_blocking;
use crate::server::state::AppState;

const MAX_CONCURRENT_BLOB_COMMITS: usize = 2;

fn blob_commit_semaphore() -> &'static Arc<Semaphore> {
  static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
  SEMAPHORE.get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_BLOB_COMMITS)))
}

fn in_flight_blob_commits() -> &'static Mutex<HashSet<u64>> {
  static IN_FLIGHT: OnceLock<Mutex<HashSet<u64>>> = OnceLock::new();
  IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()))
}

struct BlobCommitInFlightGuard {
  signature: u64,
}

impl BlobCommitInFlightGuard {
  fn try_acquire(signature: u64) -> bool {
    match in_flight_blob_commits().lock() {
      Ok(mut in_flight) => in_flight.insert(signature),
      Err(_) => false,
    }
  }

  fn new(signature: u64) -> Self {
    BlobCommitInFlightGuard { signature }
  }
}

impl Drop for BlobCommitInFlightGuard {
  fn drop(&mut self) {
    if let Ok(mut in_flight) = in_flight_blob_commits().lock() {
      in_flight.remove(&self.signature);
    }
  }
}

fn blob_commit_signature(files: &[CommitFile]) -> u64 {
  let mut hasher = std::collections::hash_map::DefaultHasher::new();
  files.len().hash(&mut hasher);
  for file in files {
    file.path.hash(&mut hasher);
    file.content_type.hash(&mut hasher);
    file.content_hash.hash(&mut hasher);
    file.size.hash(&mut hasher);
    file.chunks.len().hash(&mut hasher);
    for chunk in &file.chunks {
      chunk.hash(&mut hasher);
    }
  }
  hasher.finish()
}

/// GET /upload/config — returns hash algorithm, chunk size, and hash prefix.
pub async fn upload_config(State(state): State<AppState>) -> Response {
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
  let handler_start = std::time::Instant::now();
  let hash_count = body.hashes.len();
  let engine = state.engine.clone();
  let result = run_engine_blocking("blob_check", "Blob check failed", move || -> crate::engine::EngineResult<CheckResponse> {
    let mut have = Vec::new();
    let mut needed = Vec::new();

    for hash_hex in &body.hashes {
      let hash_bytes = hex::decode(hash_hex).map_err(|_| EngineError::InvalidInput(format!("Invalid hex hash: {}", hash_hex)))?;

      match engine.has_entry(&hash_bytes) {
        Ok(true) => have.push(hash_hex.clone()),
        Ok(false) => needed.push(hash_hex.clone()),
        Err(_) => needed.push(hash_hex.clone()),
      }
    }

    Ok(CheckResponse { have, needed })
  })
  .await;

  match result {
    Ok(response) => {
      let elapsed_ms = handler_start.elapsed().as_millis();
      if hash_count >= 1000 || elapsed_ms >= 500 {
        tracing::info!(hashes = hash_count, have = response.have.len(), needed = response.needed.len(), elapsed_ms, "blob check completed");
      } else {
        tracing::debug!(
          hashes = hash_count,
          have = response.have.len(),
          needed = response.needed.len(),
          elapsed_ms,
          "blob check completed"
        );
      }
      (StatusCode::OK, Json(response)).into_response()
    }
    Err(response) => response,
  }
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
  let handler_start = std::time::Instant::now();
  let chunk_size: usize = 262_144;
  let body_bytes = body.len();

  if body_bytes > chunk_size {
    return (
      StatusCode::BAD_REQUEST,
      Json(serde_json::json!({
        "error": "Chunk exceeds maximum size",
        "max": chunk_size,
        "got": body_bytes
      })),
    )
      .into_response();
  }

  let expected_bytes = match hex::decode(&hash_hex) {
    Ok(bytes) => bytes,
    Err(_) => {
      return (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
          "error": "Invalid hex hash in URL"
        })),
      )
        .into_response();
    }
  };

  // Compute: blake3("chunk:" + data)
  let hash_verify_start = std::time::Instant::now();
  let mut hash_input = Vec::with_capacity(6 + body.len());
  hash_input.extend_from_slice(b"chunk:");
  hash_input.extend_from_slice(&body);
  let computed = blake3::hash(&hash_input);
  let computed_bytes = computed.as_bytes().to_vec();
  let hash_verify_ms = hash_verify_start.elapsed().as_millis();

  if computed_bytes != expected_bytes {
    return (
      StatusCode::BAD_REQUEST,
      Json(serde_json::json!({
        "error": "Hash mismatch",
        "expected": hash_hex,
        "got": hex::encode(&computed_bytes)
      })),
    )
      .into_response();
  }

  // Move the I/O+fsync work to a blocking pool so we don't pin a tokio
  // worker. The engine uses std::sync locks and per-write fsync; left in an
  // async handler, multiple concurrent uploads can starve the runtime.
  let engine = state.engine.clone();
  let body_vec = body.to_vec();
  let engine_store_start = std::time::Instant::now();
  let store_result = run_engine_blocking("upload_chunk", "Failed to store chunk", move || {
    if engine.has_entry(&computed_bytes)? {
      engine.counters().record_chunk_deduped();
      return Ok("exists");
    }
    let chunk_size = body_vec.len() as u64;
    // Blob staging only knows raw chunk bytes, not the eventual file MIME
    // type. Blind compression here burns CPU on already-compressed media and
    // makes upload throughput unpredictable. Higher-level file writes that
    // have content-type/config context remain free to opt into compression.
    engine.store_entry(EntryType::Chunk, &computed_bytes, &body_vec)?;
    engine.counters().record_chunk_stored(chunk_size);
    engine.counters().record_write(chunk_size);
    Ok("created")
  })
  .await;
  let engine_store_ms = engine_store_start.elapsed().as_millis();

  match store_result {
    Ok(status) => {
      let elapsed_ms = handler_start.elapsed().as_millis();
      if elapsed_ms >= 500 {
        tracing::info!(bytes = body_bytes, status, hash_verify_ms, engine_store_ms, elapsed_ms, "blob chunk upload completed");
      } else {
        tracing::debug!(bytes = body_bytes, status, hash_verify_ms, engine_store_ms, elapsed_ms, "blob chunk upload completed");
      }
      let http_status = if status == "exists" { StatusCode::OK } else { StatusCode::CREATED };
      (
        http_status,
        Json(serde_json::json!({
          "status": status, "hash": hash_hex
        })),
      )
        .into_response()
    }
    Err(response) => response,
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
  let handler_start = std::time::Instant::now();
  let file_count = body.files.len();
  let total_chunk_refs: usize = body.files.iter().map(|file| file.chunks.len()).sum();
  let supplied_content_hash_files = body.files.iter().filter(|file| file.content_hash.is_some()).count();
  let supplied_size_files = body.files.iter().filter(|file| file.size.is_some()).count();
  let supplied_logical_file_bytes: u64 = body.files.iter().filter_map(|file| file.size).sum();
  // Beta-audit P0: a scoped key must not be able to commit files at paths
  // outside its scope, even via /blobs/commit. The path-level permission
  // middleware doesn't run for /blobs/* routes — enforce inline.
  if let Some(Extension(rules)) = active_key_rules.as_ref() {
    use crate::engine::api_key_rules::{match_rules, check_operation_permitted};
    for file in &body.files {
      let normalized = if file.path.starts_with('/') { file.path.clone() } else { format!("/{}", file.path) };
      let permitted = match match_rules(&rules.0, &normalized) {
        Some(rule) => check_operation_permitted(&rule.permitted, 'c') || check_operation_permitted(&rule.permitted, 'u'),
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

  let commit_permit: OwnedSemaphorePermit = match Arc::clone(blob_commit_semaphore()).try_acquire_owned() {
    Ok(permit) => permit,
    Err(_) => {
      tracing::warn!(
        files = file_count,
        total_chunk_refs,
        supplied_content_hash_files,
        supplied_size_files,
        supplied_logical_file_bytes,
        max_concurrent_blob_commits = MAX_CONCURRENT_BLOB_COMMITS,
        "blob commit rejected because commit workers are saturated"
      );
      return (
        StatusCode::TOO_MANY_REQUESTS,
        Json(serde_json::json!({
          "error": "Too many blob commits are already in progress; retry shortly",
          "retryable": true
        })),
      )
        .into_response();
    }
  };

  let commit_signature = blob_commit_signature(&body.files);
  if !BlobCommitInFlightGuard::try_acquire(commit_signature) {
    tracing::warn!(
      files = file_count,
      total_chunk_refs,
      supplied_content_hash_files,
      supplied_size_files,
      supplied_logical_file_bytes,
      "duplicate blob commit rejected while identical request is still in progress"
    );
    return (
      StatusCode::TOO_MANY_REQUESTS,
      Json(serde_json::json!({
        "error": "An identical blob commit is already in progress; retry after it completes",
        "retryable": true
      })),
    )
      .into_response();
  }
  let commit_guard = BlobCommitInFlightGuard::new(commit_signature);

  let engine = state.engine.clone();
  let result = run_engine_blocking("upload_commit", "Commit failed", move || {
    let _commit_permit = commit_permit;
    let _commit_guard = commit_guard;
    commit_files(&engine, &ctx, body.files)
  })
  .await;

  match result {
    Ok(commit_result) => {
      tracing::info!(
        files = file_count,
        total_chunk_refs,
        supplied_content_hash_files,
        supplied_size_files,
        supplied_logical_file_bytes,
        committed = commit_result.committed,
        elapsed_ms = handler_start.elapsed().as_millis(),
        "blob commit request completed"
      );
      (StatusCode::OK, Json(serde_json::json!(commit_result))).into_response()
    }
    Err(response) => {
      tracing::warn!(
        files = file_count,
        total_chunk_refs,
        supplied_content_hash_files,
        supplied_size_files,
        supplied_logical_file_bytes,
        elapsed_ms = handler_start.elapsed().as_millis(),
        "blob commit request failed"
      );
      response
    }
  }
}
