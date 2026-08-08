use axum::{
  Extension,
  extract::State,
  http::{StatusCode, header},
  response::{IntoResponse, Response},
  Json,
};
use serde::Deserialize;
use std::io::Write;

use super::responses::{engine_error_response, ErrorResponse};
use super::route_permissions::RoutePermissionChecker;
use super::state::AppState;
use super::temp_response::{body_from_tempfile, tempfile_for_engine, ResponseBuildCancellation, ResponseBuildGuard};
use crate::auth::TokenClaims;
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::EngineError;
use crate::engine::path_utils::normalize_path;
use crate::engine::permission_resolver::CrudlifyOp;
use crate::engine::SystemFamilyPolicyResolver;

#[derive(Deserialize)]
pub struct DownloadRequest {
  pub paths: Vec<String>,
}

/// POST /files/download — bundle requested paths into a ZIP archive.
pub async fn download_zip(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  active_key_rules: Option<Extension<crate::auth::permission_middleware::ActiveKeyRules>>,
  Json(body): Json<DownloadRequest>,
) -> Response {
  if body.paths.is_empty() {
    return ErrorResponse::new("At least one path is required in the 'paths' array").with_status(StatusCode::BAD_REQUEST).into_response();
  }

  // Scoped-key check: every requested path must be readable by the key.
  // We use 'r' for files and 'l' for directories (the matching crudlify
  // flag). If the key's rules don't permit a path, return 404 so the
  // caller cannot enumerate the tree by probing.
  if let Some(Extension(rules)) = active_key_rules.as_ref() {
    use crate::engine::api_key_rules::{match_rules, check_operation_permitted};
    for raw_path in &body.paths {
      let normalized = normalize_path(raw_path);
      // Probe the path: check 'r' (file) OR 'l' (directory listing).
      let permitted = match match_rules(&rules.0, &normalized) {
        Some(rule) => check_operation_permitted(&rule.permitted, 'r') || check_operation_permitted(&rule.permitted, 'l'),
        None => false,
      };
      if !permitted {
        return ErrorResponse::new(format!("Not found: {}", raw_path)).with_status(StatusCode::NOT_FOUND).into_response();
      }
    }
  }

  // User/group permission check: every requested path must be readable
  // by the calling user. Path-aware middleware exempts /files/download,
  // so without this, a non-root user with no API-key rules could ZIP up
  // any path in the database. 404 (not 403) so callers can't enumerate
  // existence by probing.
  if !claims.sub.starts_with("share:") {
    if let Ok(user_id) = uuid::Uuid::parse_str(&claims.sub) {
      let permissions = RoutePermissionChecker::for_user(&state, user_id);
      if !permissions.is_root() {
        for raw_path in &body.paths {
          let normalized = normalize_path(raw_path);
          if !permissions.has_any_path_permission(&normalized, &[CrudlifyOp::Read, CrudlifyOp::List]) {
            return ErrorResponse::new(format!("Not found: {}", raw_path)).with_status(StatusCode::NOT_FOUND).into_response();
          }
        }
      }
    }
  }

  const MAX_ZIP_SIZE: u64 = 2_147_483_648; // 2 GB

  // Compute common path prefix so ZIP entries are relative to the user's
  // browsing context, not the DB root. E.g. selecting /docs/readme.md and
  // /docs/notes.txt produces readme.md and notes.txt, not docs/readme.md.
  let normalized_paths: Vec<String> = body.paths.iter().map(|p| normalize_path(p)).collect();
  let common_prefix = compute_common_prefix(&normalized_paths);
  let engine = std::sync::Arc::clone(&state.engine);
  let paths = body.paths;
  let mut build_guard = ResponseBuildGuard::new();
  let cancellation = build_guard.cancellation();
  let build = tokio::task::spawn_blocking(move || build_zip(&engine, &paths, &common_prefix, MAX_ZIP_SIZE, &cancellation)).await;
  build_guard.disarm();
  let zip = match build {
    Ok(Ok(zip)) => zip,
    Ok(Err(ZipBuildError::TooLarge)) => {
      return ErrorResponse::new("Download exceeds the 2 GB size limit. Select fewer files or download individually.")
        .with_status(StatusCode::PAYLOAD_TOO_LARGE)
        .into_response();
    }
    Ok(Err(ZipBuildError::Engine(error))) => return engine_error_response("Failed to create ZIP archive", &error),
    Ok(Err(error)) => {
      tracing::error!("Failed to create ZIP archive: {}", error);
      return ErrorResponse::new("Failed to create ZIP archive").with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    Err(error) => {
      tracing::error!("ZIP builder task panicked: {}", error);
      return ErrorResponse::new("Failed to create ZIP archive: internal task error")
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  let (response_body, content_length) = match body_from_tempfile(zip.file, std::sync::Arc::clone(&state.engine)) {
    Ok(response) => response,
    Err(error) => return engine_error_response("Failed to stream ZIP archive", &error),
  };

  let mut builder = axum::http::Response::builder()
    .status(StatusCode::OK)
    .header(header::CONTENT_TYPE, "application/zip")
    .header(header::CONTENT_LENGTH, content_length.to_string())
    .header(header::CONTENT_DISPOSITION, "attachment; filename=\"aeordb-download.zip\"");

  if !zip.skipped.is_empty() {
    builder = builder.header(header::HeaderName::from_static("x-aeordb-skipped"), zip.skipped.join(", "));
  }

  builder.body(response_body).unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build ZIP response").into_response())
}

struct ZipBuildOutput {
  file: tempfile::NamedTempFile,
  skipped: Vec<String>,
}

fn build_zip(
  engine: &crate::engine::StorageEngine,
  paths: &[String],
  common_prefix: &str,
  max_size: u64,
  cancellation: &ResponseBuildCancellation,
) -> Result<ZipBuildOutput, ZipBuildError> {
  cancellation.check().map_err(ZipBuildError::Engine)?;
  let ops = DirectoryOps::new(engine);
  let family_policy = SystemFamilyPolicyResolver::new(engine.hash_algo()).map_err(ZipBuildError::Engine)?;
  let mut file = tempfile_for_engine(engine, "zip").map_err(|error| ZipBuildError::Write(error.to_string()))?;
  let mut skipped = Vec::new();
  let mut cumulative_size = 0u64;
  {
    let mut zip_writer = zip::ZipWriter::new(file.as_file_mut());
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for raw_path in paths {
      cancellation.check().map_err(ZipBuildError::Engine)?;
      let normalized = normalize_path(raw_path);
      if !family_policy.generic_data_path_is_visible(&normalized).map_err(ZipBuildError::Engine)? {
        skipped.push(raw_path.clone());
        continue;
      }

      match ops.get_metadata(&normalized) {
        Ok(Some(record)) => {
          let walk = ZipWalk { ops: &ops, family_policy, common_prefix, options, max_size, cancellation };
          let mut state = ZipState { writer: &mut zip_writer, skipped: &mut skipped, cumulative_size: &mut cumulative_size };
          add_file_to_zip(&walk, &normalized, record.total_size, &mut state)?;
        }
        Ok(None) | Err(EngineError::NotFound(_)) => {
          let walk = ZipWalk { ops: &ops, family_policy, common_prefix, options, max_size, cancellation };
          let mut state = ZipState { writer: &mut zip_writer, skipped: &mut skipped, cumulative_size: &mut cumulative_size };
          if let Err(error) = add_directory_to_zip(&walk, &normalized, &mut state) {
            if matches!(error, ZipBuildError::SourceUnavailable) {
              skipped.push(raw_path.clone());
            } else {
              return Err(error);
            }
          }
        }
        Err(EngineError::ResourceExhausted(error)) => return Err(ZipBuildError::Engine(EngineError::ResourceExhausted(error))),
        Err(_) => skipped.push(raw_path.clone()),
      }
    }
    zip_writer.finish().map_err(|error| ZipBuildError::Write(error.to_string()))?;
  }
  Ok(ZipBuildOutput { file, skipped })
}

/// Walk-invariant arguments for the recursive ZIP builder.
struct ZipWalk<'a> {
  ops: &'a DirectoryOps<'a>,
  family_policy: SystemFamilyPolicyResolver,
  common_prefix: &'a str,
  options: zip::write::SimpleFileOptions,
  max_size: u64,
  cancellation: &'a ResponseBuildCancellation,
}

/// Mutable state threaded through the recursive ZIP builder. Bundled so the
/// recursion signature stays short.
struct ZipState<'a, 'b> {
  writer: &'a mut zip::ZipWriter<&'b mut std::fs::File>,
  skipped: &'a mut Vec<String>,
  cumulative_size: &'a mut u64,
}

#[derive(Debug)]
enum ZipBuildError {
  SourceUnavailable,
  TooLarge,
  Write(String),
  Engine(EngineError),
}

impl std::fmt::Display for ZipBuildError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      ZipBuildError::SourceUnavailable => write!(formatter, "source unavailable"),
      ZipBuildError::TooLarge => write!(formatter, "download exceeds size limit"),
      ZipBuildError::Write(error) => write!(formatter, "zip write failed: {}", error),
      ZipBuildError::Engine(error) => write!(formatter, "engine read failed: {}", error),
    }
  }
}

fn add_file_to_zip(walk: &ZipWalk<'_>, path: &str, size: u64, state: &mut ZipState<'_, '_>) -> Result<(), ZipBuildError> {
  walk.cancellation.check().map_err(ZipBuildError::Engine)?;
  let next_size = state.cumulative_size.checked_add(size).ok_or(ZipBuildError::TooLarge)?;
  if next_size > walk.max_size {
    return Err(ZipBuildError::TooLarge);
  }
  let zip_entry_name = strip_prefix(path, walk.common_prefix);
  state.writer.start_file(&zip_entry_name, walk.options).map_err(|error| ZipBuildError::Write(error.to_string()))?;
  let mut stream = walk.ops.read_file_streaming(path).map_err(ZipBuildError::Engine)?;
  while let Some(chunk) = stream.next_reserved() {
    walk.cancellation.check().map_err(ZipBuildError::Engine)?;
    let chunk = chunk.map_err(ZipBuildError::Engine)?;
    state.writer.write_all(chunk.as_ref()).map_err(|error| ZipBuildError::Write(error.to_string()))?;
  }
  *state.cumulative_size = next_size;
  Ok(())
}

fn add_directory_to_zip(walk: &ZipWalk<'_>, dir_path: &str, state: &mut ZipState<'_, '_>) -> Result<(), ZipBuildError> {
  walk.cancellation.check().map_err(ZipBuildError::Engine)?;
  let entries = walk.ops.list_directory(dir_path).map_err(|_| ZipBuildError::SourceUnavailable)?;

  for entry in entries {
    walk.cancellation.check().map_err(ZipBuildError::Engine)?;
    let child_path = if dir_path == "/" { format!("/{}", entry.name) } else { format!("{}/{}", dir_path, entry.name) };

    let normalized = normalize_path(&child_path);

    if !walk.family_policy.generic_data_path_is_visible(&normalized).map_err(ZipBuildError::Engine)? {
      state.skipped.push(child_path);
      continue;
    }

    if entry.entry_type == EntryType::DirectoryIndex.to_u8() {
      add_directory_to_zip(walk, &normalized, state)?;
    } else if entry.entry_type == EntryType::FileRecord.to_u8() {
      match walk.ops.get_metadata(&normalized) {
        Ok(Some(record)) => add_file_to_zip(walk, &normalized, record.total_size, state)?,
        Ok(None) | Err(EngineError::NotFound(_)) => state.skipped.push(normalized),
        Err(error) => return Err(ZipBuildError::Engine(error)),
      }
    }
  }

  Ok(())
}

/// Compute the longest common directory prefix from a list of paths.
/// E.g. ["/docs/readme.md", "/docs/notes.txt"] → "/docs/"
/// E.g. ["/docs/readme.md", "/images/logo.svg"] → "/"
fn compute_common_prefix(paths: &[String]) -> String {
  if paths.is_empty() {
    return "/".to_string();
  }

  // Split each path into directory segments
  let first = paths[0].as_str();
  let first_parent = match first.rfind('/') {
    Some(0) => "/",
    Some(idx) => &first[..idx + 1],
    None => "/",
  };

  let mut prefix = first_parent.to_string();

  for path in &paths[1..] {
    // Shorten prefix until it matches this path
    while !prefix.is_empty() && prefix != "/" {
      if path.starts_with(&prefix) {
        break;
      }
      // Remove last segment
      let trimmed = prefix.trim_end_matches('/');
      match trimmed.rfind('/') {
        Some(0) => {
          prefix = "/".to_string();
          break;
        }
        Some(idx) => {
          prefix = trimmed[..idx + 1].to_string();
        }
        None => {
          prefix = "/".to_string();
          break;
        }
      }
    }
  }

  prefix
}

/// Strip the common prefix from a path to get the ZIP entry name.
/// "/docs/readme.md" with prefix "/docs/" → "readme.md"
/// "/readme.md" with prefix "/" → "readme.md"
fn strip_prefix(path: &str, prefix: &str) -> String {
  let stripped = if prefix == "/" {
    path.trim_start_matches('/')
  } else if path.starts_with(prefix) {
    &path[prefix.len()..]
  } else {
    path.trim_start_matches('/')
  };
  stripped.to_string()
}
