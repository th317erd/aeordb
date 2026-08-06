use axum::{
  Extension,
  body::Body,
  extract::State,
  http::{header::CONTENT_LENGTH, HeaderMap, StatusCode},
  response::{IntoResponse, Response},
  Json,
};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

use super::blocking::run_engine_blocking;
use super::responses::{engine_error_response, error_codes, ErrorResponse, require_root};
use super::state::AppState;
use crate::auth::TokenClaims;
use crate::engine::memory_coordinator::{AdmissionClass, CriticalMemoryPurpose, MemoryOwner, MemoryReservation};
use crate::engine::operation_memory::OperationMemoryBudget;
use crate::engine::{EngineError, RequestContext, StorageEngine};

const BACKUP_STREAM_BUFFER_BYTES: u64 = 64 * 1024;
pub(super) const BACKUP_UPLOAD_LIMIT_BYTES: usize = 10 * 1024 * 1024 * 1024;

struct TemporaryFileStream {
  inner: Option<ReaderStream<tokio::fs::File>>,
  artifact: Option<TemporaryBackupArtifact>,
  _reservation: MemoryReservation,
}

impl Stream for TemporaryFileStream {
  type Item = Result<axum::body::Bytes, std::io::Error>;

  fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
    match self.inner.as_mut() {
      Some(inner) => Pin::new(inner).poll_next(context),
      None => Poll::Ready(None),
    }
  }
}

impl Drop for TemporaryFileStream {
  fn drop(&mut self) {
    drop(self.inner.take());
    drop(self.artifact.take());
  }
}

struct TemporaryBackupArtifact {
  path: PathBuf,
  owned: bool,
}

impl TemporaryBackupArtifact {
  fn path_string(&self) -> String {
    self.path.to_string_lossy().into_owned()
  }

  fn mark_owned(&mut self) {
    self.owned = true;
  }

  async fn create_upload_file(&mut self) -> std::io::Result<tokio::fs::File> {
    let file = tokio::fs::OpenOptions::new().write(true).create_new(true).open(&self.path).await?;
    self.mark_owned();
    Ok(file)
  }
}

impl Drop for TemporaryBackupArtifact {
  fn drop(&mut self) {
    if self.owned {
      crate::engine::backup::cleanup_backup_artifact(&self.path);
    }
  }
}

fn unique_temp_artifact(engine: &StorageEngine, prefix: &str) -> Result<TemporaryBackupArtifact, std::io::Error> {
  let directory =
    engine.database_path().parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| std::path::Path::new("."));
  let temp_file = tempfile::Builder::new().prefix(&format!("{}-", prefix)).suffix(".aeordb").tempfile_in(directory)?;
  let path = temp_file.path().to_path_buf();
  // Unlink the placeholder while retaining its collision-resistant path.
  drop(temp_file.into_temp_path());
  Ok(TemporaryBackupArtifact { path, owned: false })
}

fn checked_upload_total(current: u64, frame_bytes: usize, limit: u64) -> Result<u64, ()> {
  let frame_bytes = u64::try_from(frame_bytes).map_err(|_| ())?;
  let next = current.checked_add(frame_bytes).ok_or(())?;
  if next > limit {
    return Err(());
  }
  Ok(next)
}

/// POST /admin/export -- export a version as .aeordb
/// Query params: snapshot=name, hash=hex (default: HEAD)
pub async fn export_backup(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  params: axum::extract::Query<ExportParams>,
) -> Response {
  let _user_id = match require_root(&claims) {
    Ok(id) => id,
    Err(response) => return response,
  };

  let mut artifact = match unique_temp_artifact(&state.engine, "aeordb-export") {
    Ok(artifact) => artifact,
    Err(e) => {
      return ErrorResponse::new(format!("Failed to create temporary file for backup operation: {}. Check disk space and permissions", e))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  };

  let hash_bytes = match params.hash.as_deref() {
    Some(hash) => match hex::decode(hash) {
      Ok(bytes) if bytes.len() == state.engine.hash_algo().hash_length() => Some(bytes),
      Ok(bytes) => {
        return ErrorResponse::new(format!(
          "Invalid hash length: expected {} bytes, got {}",
          state.engine.hash_algo().hash_length(),
          bytes.len()
        ))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response()
      }
      Err(e) => return ErrorResponse::new(format!("Invalid hash: {}", e)).with_status(StatusCode::BAD_REQUEST).into_response(),
    },
    None => None,
  };
  let snapshot = params.snapshot.clone();
  let engine = state.engine.clone();
  let work_path = artifact.path_string();
  let result = run_engine_blocking("backup export", "Export failed", move || {
    // HTTP exports never include system data. Full system backups require the
    // CLI root-key flow.
    let result = match hash_bytes {
      Some(hash) => crate::engine::backup::export_version(&engine, &hash, &work_path, false),
      None => crate::engine::backup::export_snapshot(&engine, snapshot.as_deref(), &work_path, false),
    }?;
    artifact.mark_owned();
    Ok((result, artifact))
  })
  .await;

  match result {
    Ok((export_result, artifact)) => {
      let hash_hex = hex::encode(&export_result.version_hash);
      let hash_prefix = if hash_hex.len() >= 8 { &hash_hex[..8] } else { &hash_hex };
      let filename = format!("export-{}.aeordb", hash_prefix);
      stream_temporary_backup(state.engine.clone(), artifact, filename).await
    }
    Err(response) => response,
  }
}

/// POST /admin/diff -- create a patch between two versions
pub async fn diff_backup(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  params: axum::extract::Query<DiffParams>,
) -> Response {
  let _user_id = match require_root(&claims) {
    Ok(id) => id,
    Err(response) => return response,
  };

  let mut artifact = match unique_temp_artifact(&state.engine, "aeordb-patch") {
    Ok(artifact) => artifact,
    Err(e) => {
      return ErrorResponse::new(format!("Failed to create temporary file for backup operation: {}. Check disk space and permissions", e))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  };

  let engine = state.engine.clone();
  let from = params.from.clone();
  let to = params.to.clone();
  let work_path = artifact.path_string();
  let result = run_engine_blocking("backup diff", "Diff failed", move || {
    let result = crate::engine::backup::create_patch_from_references(&engine, &from, to.as_deref(), &work_path)?;
    artifact.mark_owned();
    Ok((result, artifact))
  })
  .await;

  match result {
    Ok((patch_result, artifact)) => {
      let hash_hex = hex::encode(&patch_result.to_hash);
      let hash_prefix = if hash_hex.len() >= 8 { &hash_hex[..8] } else { &hash_hex };
      let filename = format!("patch-{}.aeordb", hash_prefix);
      stream_temporary_backup(state.engine.clone(), artifact, filename).await
    }
    Err(response) => response,
  }
}

/// POST /admin/import -- import a backup file
pub async fn import_backup(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  params: axum::extract::Query<ImportParams>,
  headers: HeaderMap,
  body: Body,
) -> Response {
  let _user_id = match require_root(&claims) {
    Ok(id) => id,
    Err(response) => return response,
  };

  let mode = match crate::engine::backup::ImportMode::parse(params.mode.as_deref()) {
    Ok(mode) => mode,
    Err(error) => return engine_error_response("Invalid import mode", &error),
  };

  if let Some(value) = headers.get(CONTENT_LENGTH) {
    let declared_bytes = match value.to_str().ok().and_then(|value| value.parse::<u64>().ok()) {
      Some(bytes) => bytes,
      None => return ErrorResponse::new("Invalid Content-Length header").with_status(StatusCode::BAD_REQUEST).into_response(),
    };
    if declared_bytes > BACKUP_UPLOAD_LIMIT_BYTES as u64 {
      return ErrorResponse::new(format!("Import upload exceeds the {} byte limit", BACKUP_UPLOAD_LIMIT_BYTES))
        .with_status(StatusCode::PAYLOAD_TOO_LARGE)
        .into_response();
    }
  }

  let mut artifact = match unique_temp_artifact(&state.engine, "aeordb-import") {
    Ok(artifact) => artifact,
    Err(e) => {
      return ErrorResponse::new(format!("Failed to create temporary file for backup operation: {}. Check disk space and permissions", e))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  };

  let mut stream_budget = match OperationMemoryBudget::new(
    &state.engine,
    "backup upload",
    MemoryOwner::BackupRestore,
    AdmissionClass::Maintenance,
    4 * 1024,
    None,
  ) {
    Ok(budget) => budget,
    Err(error) => return engine_error_response("Import upload refused", &error),
  };

  let mut file = match artifact.create_upload_file().await {
    Ok(file) => file,
    Err(error) => {
      return ErrorResponse::new(format!("Failed to create uploaded backup temporary file: {}. Check disk space and permissions", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  };
  let mut data_stream = body.into_data_stream();
  let mut received_data = false;
  let mut received_bytes = 0_u64;
  while let Some(frame) = data_stream.next().await {
    let bytes = match frame {
      Ok(bytes) => bytes,
      Err(error) => {
        drop(file);
        return ErrorResponse::new(format!("Failed to read import upload stream: {}", error))
          .with_status(StatusCode::BAD_REQUEST)
          .into_response();
      }
    };
    received_data |= !bytes.is_empty();
    received_bytes = match checked_upload_total(received_bytes, bytes.len(), BACKUP_UPLOAD_LIMIT_BYTES as u64) {
      Ok(total) => total,
      Err(()) => {
        drop(file);
        return ErrorResponse::new(format!("Import upload exceeds the {} byte limit", BACKUP_UPLOAD_LIMIT_BYTES))
          .with_status(StatusCode::PAYLOAD_TOO_LARGE)
          .into_response();
      }
    };
    let frame_bytes = match u64::try_from(bytes.len()) {
      Ok(bytes) => bytes,
      Err(_) => {
        drop(file);
        return ErrorResponse::new("Import upload frame is too large").with_status(StatusCode::PAYLOAD_TOO_LARGE).into_response();
      }
    };
    if let Err(error) = stream_budget.reserve(frame_bytes, "upload frame admission failed") {
      drop(file);
      return engine_error_response("Import upload refused", &error);
    }
    if let Err(error) = file.write_all(&bytes).await {
      drop(file);
      return ErrorResponse::new(format!("Failed to write uploaded backup temporary file: {}. Check disk space", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
    if let Err(error) = stream_budget.release(frame_bytes, "upload frame release failed") {
      drop(file);
      return engine_error_response("Import upload accounting failed", &error);
    }
  }
  if !received_data {
    drop(file);
    return ErrorResponse::new("Import upload body is empty").with_status(StatusCode::BAD_REQUEST).into_response();
  }
  if let Err(error) = file.sync_data().await {
    drop(file);
    return ErrorResponse::new(format!("Failed to synchronize uploaded backup temporary file: {}", error))
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response();
  }
  drop(file);
  drop(stream_budget);

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let engine = state.engine.clone();
  let work_path = artifact.path_string();
  let force = params.force.unwrap_or(false);
  let promote = params.promote.unwrap_or(false);
  let result = match tokio::task::spawn_blocking(move || {
    // HTTP imports never accept system data. System restore requires the CLI
    // root-key flow.
    let result = crate::engine::backup::import_backup_with_mode(&ctx, &engine, &work_path, force, promote, false, mode);
    (result, artifact)
  })
  .await
  {
    Ok((Ok(result), _artifact)) => Ok(result),
    Ok((Err(error), _artifact)) => {
      tracing::error!(%error, "backup import failed");
      Err(import_error_response(&error))
    }
    Err(error) => {
      tracing::error!(%error, "backup import task panicked");
      Err(ErrorResponse::new("Import failed: internal task error").with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response())
    }
  };

  match result {
    Ok(import_result) => (
      StatusCode::OK,
      Json(serde_json::json!({
          "status": "success",
          "backup_type": match import_result.backup_type { 1 => "export", 2 => "patch", _ => "unknown" },
          "entries_imported": import_result.entries_imported,
          "chunks_imported": import_result.chunks_imported,
          "files_imported": import_result.files_imported,
          "directories_imported": import_result.directories_imported,
          "deletions_applied": import_result.deletions_applied,
          "version_hash": hex::encode(&import_result.version_hash),
          "head_promoted": import_result.head_promoted,
      })),
    )
      .into_response(),
    Err(response) => response,
  }
}

fn import_error_response(error: &EngineError) -> Response {
  if matches!(
    error,
    EngineError::InvalidMagic
      | EngineError::InvalidEntryVersion(_)
      | EngineError::InvalidEntryType(_)
      | EngineError::InvalidHashAlgorithm(_)
      | EngineError::CorruptEntry { .. }
      | EngineError::UnexpectedEof
      | EngineError::PatchDatabase(_)
  ) {
    return ErrorResponse::new("Import failed: uploaded file is not a valid AeorDB backup")
      .with_code(error_codes::INVALID_INPUT)
      .with_status(StatusCode::BAD_REQUEST)
      .into_response();
  }
  engine_error_response("Import failed", error)
}

async fn stream_temporary_backup(engine: Arc<StorageEngine>, artifact: TemporaryBackupArtifact, filename: String) -> Response {
  let file_size = match tokio::fs::metadata(&artifact.path).await {
    Ok(metadata) => metadata.len(),
    Err(error) => {
      return ErrorResponse::new(format!("Failed to stat generated backup file: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };
  let reservation = match engine.memory_coordinator().reserve(
    MemoryOwner::StreamingRead,
    BACKUP_STREAM_BUFFER_BYTES,
    AdmissionClass::Critical(CriticalMemoryPurpose::StreamingRead),
  ) {
    Ok(reservation) => reservation,
    Err(error) => {
      let engine_error = EngineError::ResourceExhausted(format!("backup download buffer admission failed: {}", error));
      return engine_error_response("Backup download refused", &engine_error);
    }
  };
  let file = match tokio::fs::File::open(&artifact.path).await {
    Ok(file) => file,
    Err(error) => {
      return ErrorResponse::new(format!("Failed to open generated backup file: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };
  let stream = TemporaryFileStream {
    inner: Some(ReaderStream::with_capacity(file, BACKUP_STREAM_BUFFER_BYTES as usize)),
    artifact: Some(artifact),
    _reservation: reservation,
  };
  axum::http::Response::builder()
    .status(StatusCode::OK)
    .header("content-type", "application/octet-stream")
    .header("content-disposition", format!("attachment; filename=\"{}\"", filename))
    .header("content-length", file_size.to_string())
    .body(Body::from_stream(stream))
    .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build backup response").into_response())
}

/// POST /admin/promote -- promote a version hash to HEAD
pub async fn promote_head(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  params: axum::extract::Query<PromoteParams>,
) -> Response {
  let _user_id = match require_root(&claims) {
    Ok(id) => id,
    Err(response) => return response,
  };

  let hash_bytes = match hex::decode(&params.hash) {
    Ok(b) => b,
    Err(e) => return ErrorResponse::new(format!("Invalid hash: {}", e)).with_status(StatusCode::BAD_REQUEST).into_response(),
  };

  match state.engine.has_entry(&hash_bytes) {
    Ok(true) => {}
    Ok(false) => {
      return ErrorResponse::new(format!(
        "Version hash '{}' not found. Use GET /versions/snapshots to find valid version hashes",
        params.hash
      ))
      .with_status(StatusCode::NOT_FOUND)
      .into_response()
    }
    Err(e) => {
      return ErrorResponse::new(format!("Failed to verify version hash '{}': {}", params.hash, e))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }

  match state.engine.update_head(&hash_bytes) {
    Ok(()) => (
      StatusCode::OK,
      Json(serde_json::json!({
          "status": "success",
          "head": hex::encode(&hash_bytes),
      })),
    )
      .into_response(),
    Err(e) => ErrorResponse::new(format!("Promote failed: {}", e)).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response(),
  }
}

#[derive(Debug, Deserialize)]
pub struct ExportParams {
  pub snapshot: Option<String>,
  pub hash: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DiffParams {
  pub from: String,
  pub to: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ImportParams {
  pub force: Option<bool>,
  pub promote: Option<bool>,
  /// "restore" or "merge". Restore refuses to write into a non-empty target
  /// (unless `force=true`); merge unions the backup into the target. Defaults
  /// to "merge" for compatibility with existing callers.
  pub mode: Option<String>,
}

#[cfg(test)]
mod upload_limit_tests {
  use super::*;

  #[test]
  fn upload_total_accepts_exact_limit() {
    let limit = BACKUP_UPLOAD_LIMIT_BYTES as u64;
    assert_eq!(checked_upload_total(limit - 7, 7, limit), Ok(limit));
  }

  #[test]
  fn upload_total_rejects_one_byte_over_limit() {
    let limit = BACKUP_UPLOAD_LIMIT_BYTES as u64;
    assert_eq!(checked_upload_total(limit, 1, limit), Err(()));
  }

  #[test]
  fn upload_total_rejects_arithmetic_overflow() {
    assert_eq!(checked_upload_total(u64::MAX, 1, u64::MAX), Err(()));
  }

  #[tokio::test]
  async fn unowned_temporary_artifact_does_not_remove_a_substituted_path() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("substituted.aeordb");
    let mut artifact = TemporaryBackupArtifact { path: path.clone(), owned: false };
    std::fs::write(&path, b"not ours").unwrap();

    let error = artifact.create_upload_file().await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);

    drop(artifact);

    assert_eq!(std::fs::read(path).unwrap(), b"not ours");
  }
}

#[derive(Debug, Deserialize)]
pub struct PromoteParams {
  pub hash: String,
}
