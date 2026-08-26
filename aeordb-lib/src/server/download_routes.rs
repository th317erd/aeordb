use axum::{
  Extension,
  extract::State,
  http::{StatusCode, header},
  response::{IntoResponse, Response},
  Json,
};
use serde::Deserialize;
use std::io::Write;

use super::engine_routes::{
  attach_root_metadata_headers, current_path_is_authorized, legacy_root_adapter_error_response, reject_historical_share_selector,
  require_legacy_root_plan, root_api_error_response, selected_path_is_authorized,
};
use super::legacy_v3_root_adapter::{LegacyV3RootAdapterErrorV1, LegacyV3SelectedFileV1, LegacyV3SelectedPathV1, LegacyV3SelectedRootAdapterV1};
use super::responses::{engine_error_response, ErrorResponse};
use super::root_api::{RequestedRootSelectorV1, RootRequestAdapterV1, RootSelectorFieldsV1, RouteRootRequestPlanV1, parse_root_selector_v1};
use super::state::AppState;
use super::temp_response::{body_from_tempfile, tempfile_for_engine, ResponseBuildCancellation, ResponseBuildGuard};
use crate::auth::TokenClaims;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::EngineError;
use crate::engine::path_utils::normalize_path;
use crate::engine::permission_resolver::CrudlifyOp;
use crate::engine::system_family_policy::GenericDataPathSelection;
use crate::engine::SystemFamilyPolicyResolver;

#[derive(Deserialize)]
pub struct DownloadRequest {
  pub paths: Vec<String>,
  pub root_hash: Option<String>,
  pub snapshot: Option<String>,
  pub version: Option<String>,
}

/// POST /files/download — bundle requested paths into a ZIP archive.
pub async fn download_zip(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Extension(root_plan): Extension<RouteRootRequestPlanV1>,
  active_key_rules: Option<Extension<crate::auth::permission_middleware::ActiveKeyRules>>,
  Json(body): Json<DownloadRequest>,
) -> Response {
  if let Err(response) = require_legacy_root_plan(root_plan, RootRequestAdapterV1::ResolveSingleRoot) {
    return response;
  }
  if body.paths.is_empty() {
    return ErrorResponse::new("At least one path is required in the 'paths' array").with_status(StatusCode::BAD_REQUEST).into_response();
  }

  let key_rules = active_key_rules.as_ref().map(|Extension(rules)| rules.0.as_slice());
  let family_policy = match SystemFamilyPolicyResolver::new(state.engine.hash_algo()) {
    Ok(family_policy) => family_policy,
    Err(error) => return engine_error_response("Failed to classify download paths", &error),
  };
  let mut selected_paths = Vec::with_capacity(body.paths.len());
  let mut pre_skipped = Vec::new();
  for raw_path in &body.paths {
    let normalized = normalize_path(raw_path);
    match family_policy.generic_data_path_selection(&normalized) {
      Ok(GenericDataPathSelection::Include) => {}
      Ok(GenericDataPathSelection::Conceal | GenericDataPathSelection::StructuralContainer) => {
        pre_skipped.push(raw_path.clone());
        continue;
      }
      Err(error) => return engine_error_response("Failed to classify download path", &error),
    }
    let permitted = current_path_is_authorized(&state, &claims, key_rules, &normalized, CrudlifyOp::Read).and_then(|readable| {
      if readable {
        Ok(true)
      } else {
        current_path_is_authorized(&state, &claims, key_rules, &normalized, CrudlifyOp::List)
      }
    });
    match permitted {
      Ok(true) => {}
      Ok(false) => {
        return ErrorResponse::new(format!("Not found: {raw_path}")).with_status(StatusCode::NOT_FOUND).into_response();
      }
      Err(error) => return engine_error_response("Failed to check download permission", &error),
    }
    selected_paths.push(raw_path.clone());
  }
  if selected_paths.is_empty() {
    return ErrorResponse::new(format!("Not found: {}", body.paths[0])).with_status(StatusCode::NOT_FOUND).into_response();
  }

  let selector_fields = RootSelectorFieldsV1 { root_hash: body.root_hash, snapshot: body.snapshot, version: body.version };
  let selector = match parse_root_selector_v1(&selector_fields, state.engine.hash_algo()) {
    Ok(selector) => selector,
    Err(error) => return root_api_error_response(error, false),
  };
  if let Err(response) = reject_historical_share_selector(&claims, &selector) {
    return response;
  }

  const MAX_ZIP_SIZE: u64 = 2_147_483_648; // 2 GB

  // Compute common path prefix so ZIP entries are relative to the user's
  // browsing context, not the DB root. E.g. selecting /docs/readme.md and
  // /docs/notes.txt produces readme.md and notes.txt, not docs/readme.md.
  let normalized_paths: Vec<String> = body.paths.iter().map(|p| normalize_path(p)).collect();
  let common_prefix = compute_common_prefix(&normalized_paths);
  let build_state = state.clone();
  let build_claims = claims.clone();
  let build_key_rules = active_key_rules.map(|Extension(rules)| rules);
  let paths = selected_paths;
  let mut build_guard = ResponseBuildGuard::new();
  let cancellation = build_guard.cancellation();
  let build = tokio::task::spawn_blocking(move || {
    build_zip(
      &build_state,
      &build_claims,
      build_key_rules.as_ref().map(|rules| rules.0.as_slice()),
      &selector,
      &paths,
      pre_skipped,
      &common_prefix,
      MAX_ZIP_SIZE,
      &cancellation,
    )
  })
  .await;
  build_guard.disarm();
  let zip = match build {
    Ok(Ok(zip)) => zip,
    Ok(Err(ZipBuildError::TooLarge)) => {
      return ErrorResponse::new("Download exceeds the 2 GB size limit. Select fewer files or download individually.")
        .with_status(StatusCode::PAYLOAD_TOO_LARGE)
        .into_response();
    }
    Ok(Err(ZipBuildError::Concealed(path))) => {
      return ErrorResponse::new(format!("Not found: {path}")).with_status(StatusCode::NOT_FOUND).into_response();
    }
    Ok(Err(ZipBuildError::Root(error))) => return legacy_root_adapter_error_response(error),
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

  let response =
    builder.body(response_body).unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build ZIP response").into_response());
  attach_root_metadata_headers(response, &zip.root, state.engine.hash_algo()).unwrap_or_else(|response| response)
}

struct ZipBuildOutput {
  file: tempfile::NamedTempFile,
  skipped: Vec<String>,
  root: crate::engine::v4::read_view::ReadViewRootMetadataV1,
}

fn build_zip(
  state: &AppState,
  claims: &TokenClaims,
  key_rules: Option<&[crate::engine::api_key_rules::KeyRule]>,
  selector: &RequestedRootSelectorV1,
  paths: &[String],
  pre_skipped: Vec<String>,
  common_prefix: &str,
  max_size: u64,
  cancellation: &ResponseBuildCancellation,
) -> Result<ZipBuildOutput, ZipBuildError> {
  cancellation.check().map_err(ZipBuildError::Engine)?;
  let selected = LegacyV3SelectedRootAdapterV1::resolve(&state.engine, selector).map_err(ZipBuildError::Root)?;
  let mut file = tempfile_for_engine(&state.engine, "zip").map_err(|error| ZipBuildError::Write(error.to_string()))?;
  let mut skipped = pre_skipped;
  let mut cumulative_size = 0u64;
  {
    let mut zip_writer = zip::ZipWriter::new(file.as_file_mut());
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for raw_path in paths {
      cancellation.check().map_err(ZipBuildError::Engine)?;
      let normalized = normalize_path(raw_path);
      match selected.path(&normalized) {
        Ok(LegacyV3SelectedPathV1::File(selected_file)) => {
          let walk = ZipWalk { state, claims, key_rules, selected: &selected, common_prefix, options, max_size, cancellation };
          if !zip_descendant_is_authorized(&walk, &normalized, CrudlifyOp::Read)? {
            return Err(ZipBuildError::Concealed(raw_path.clone()));
          }
          let mut state = ZipState { writer: &mut zip_writer, skipped: &mut skipped, cumulative_size: &mut cumulative_size };
          add_file_to_zip(&walk, &normalized, selected_file, &mut state)?;
        }
        Ok(LegacyV3SelectedPathV1::Directory(_)) => {
          let walk = ZipWalk { state, claims, key_rules, selected: &selected, common_prefix, options, max_size, cancellation };
          if !zip_descendant_is_authorized(&walk, &normalized, CrudlifyOp::List)? {
            return Err(ZipBuildError::Concealed(raw_path.clone()));
          }
          let mut state = ZipState { writer: &mut zip_writer, skipped: &mut skipped, cumulative_size: &mut cumulative_size };
          if let Err(error) = add_directory_to_zip(&walk, &normalized, &mut state) {
            if matches!(error, ZipBuildError::SourceUnavailable) {
              skipped.push(raw_path.clone());
            } else {
              return Err(error);
            }
          }
        }
        Ok(LegacyV3SelectedPathV1::Symlink(_)) | Err(EngineError::NotFound(_)) => skipped.push(raw_path.clone()),
        Err(error) => return Err(ZipBuildError::Engine(error)),
      }
    }
    zip_writer.finish().map_err(|error| ZipBuildError::Write(error.to_string()))?;
  }
  Ok(ZipBuildOutput { file, skipped, root: selected.root_metadata().clone() })
}

/// Walk-invariant arguments for the recursive ZIP builder.
struct ZipWalk<'walk, 'engine> {
  state: &'walk AppState,
  claims: &'walk TokenClaims,
  key_rules: Option<&'walk [crate::engine::api_key_rules::KeyRule]>,
  selected: &'walk LegacyV3SelectedRootAdapterV1<'engine>,
  common_prefix: &'walk str,
  options: zip::write::SimpleFileOptions,
  max_size: u64,
  cancellation: &'walk ResponseBuildCancellation,
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
  Concealed(String),
  TooLarge,
  Write(String),
  Engine(EngineError),
  Root(LegacyV3RootAdapterErrorV1),
}

impl From<EngineError> for ZipBuildError {
  fn from(error: EngineError) -> Self {
    Self::Engine(error)
  }
}

impl std::fmt::Display for ZipBuildError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      ZipBuildError::SourceUnavailable => write!(formatter, "source unavailable"),
      ZipBuildError::Concealed(path) => write!(formatter, "source is concealed: {path}"),
      ZipBuildError::TooLarge => write!(formatter, "download exceeds size limit"),
      ZipBuildError::Write(error) => write!(formatter, "zip write failed: {}", error),
      ZipBuildError::Engine(error) => write!(formatter, "engine read failed: {}", error),
      ZipBuildError::Root(error) => write!(formatter, "selected root failed: {error}"),
    }
  }
}

fn add_file_to_zip(
  walk: &ZipWalk<'_, '_>,
  path: &str,
  selected_file: LegacyV3SelectedFileV1,
  state: &mut ZipState<'_, '_>,
) -> Result<(), ZipBuildError> {
  walk.cancellation.check().map_err(ZipBuildError::Engine)?;
  let next_size = state.cumulative_size.checked_add(selected_file.record.total_size).ok_or(ZipBuildError::TooLarge)?;
  if next_size > walk.max_size {
    return Err(ZipBuildError::TooLarge);
  }
  let zip_entry_name = strip_prefix(path, walk.common_prefix);
  state.writer.start_file(&zip_entry_name, walk.options).map_err(|error| ZipBuildError::Write(error.to_string()))?;
  let mut stream = walk.selected.file_stream(&selected_file).map_err(ZipBuildError::Engine)?;
  while let Some(chunk) = stream.next_reserved() {
    walk.cancellation.check().map_err(ZipBuildError::Engine)?;
    let chunk = chunk.map_err(ZipBuildError::Engine)?;
    state.writer.write_all(chunk.as_ref()).map_err(|error| ZipBuildError::Write(error.to_string()))?;
  }
  *state.cumulative_size = next_size;
  Ok(())
}

fn add_directory_to_zip(walk: &ZipWalk<'_, '_>, dir_path: &str, state: &mut ZipState<'_, '_>) -> Result<(), ZipBuildError> {
  walk.cancellation.check().map_err(ZipBuildError::Engine)?;
  let entries = match walk.selected.list_directory(dir_path) {
    Ok(entries) => entries,
    Err(EngineError::NotFound(_)) => return Err(ZipBuildError::SourceUnavailable),
    Err(error) => return Err(ZipBuildError::Engine(error)),
  };

  for entry in entries {
    walk.cancellation.check().map_err(ZipBuildError::Engine)?;
    let normalized = entry.path;
    let operation = if entry.entry_type == EntryType::DirectoryIndex.to_u8() { CrudlifyOp::List } else { CrudlifyOp::Read };
    if !zip_descendant_is_authorized(walk, &normalized, operation)? {
      state.skipped.push(normalized);
      continue;
    }

    if entry.entry_type == EntryType::DirectoryIndex.to_u8() {
      add_directory_to_zip(walk, &normalized, state)?;
    } else if entry.entry_type == EntryType::FileRecord.to_u8() {
      match walk.selected.file(&normalized) {
        Ok(selected_file) => add_file_to_zip(walk, &normalized, selected_file, state)?,
        Err(EngineError::NotFound(_)) => state.skipped.push(normalized),
        Err(error) => return Err(ZipBuildError::Engine(error)),
      }
    }
  }

  Ok(())
}

fn zip_descendant_is_authorized(walk: &ZipWalk<'_, '_>, path: &str, operation: CrudlifyOp) -> Result<bool, ZipBuildError> {
  if !current_path_is_authorized(walk.state, walk.claims, walk.key_rules, path, operation).map_err(ZipBuildError::Engine)? {
    return Ok(false);
  }
  selected_path_is_authorized(walk.state, walk.claims, walk.selected, path, operation).map_err(ZipBuildError::Engine)
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
