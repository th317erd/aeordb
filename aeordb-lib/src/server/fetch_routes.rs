use axum::{
  Extension,
  extract::State,
  http::StatusCode,
  response::{IntoResponse, Response},
  Json,
};
use serde::{Deserialize, Serialize};
use std::io::Write;

use super::engine_routes::{
  attach_root_metadata_headers, current_path_read_is_authorized, legacy_root_adapter_error_response, reject_historical_share_selector,
  require_legacy_root_plan, root_api_error_response, selected_path_is_authorized, RouteResponseError,
};
use super::legacy_v3_root_adapter::{LegacyV3RootAdapterErrorV1, LegacyV3SelectedRootAdapterV1};
use super::responses::{engine_error_response, ErrorResponse};
use super::root_api::{RequestedRootSelectorV1, RootRequestAdapterV1, RootSelectorFieldsV1, RouteRootRequestPlanV1, parse_root_selector_v1};
use super::state::AppState;
use super::temp_response::{body_from_tempfile, tempfile_for_engine, ResponseBuildCancellation, ResponseBuildGuard};
use crate::auth::permission_middleware::ActiveKeyRules;
use crate::auth::TokenClaims;
use crate::engine::directory_ops::{EngineFileStream, reserve_streaming_read};
use crate::engine::errors::EngineError;
use crate::engine::path_utils::{file_name, normalize_path};
use crate::engine::permission_resolver::CrudlifyOp;
use crate::engine::range_extract::RangeExtractionRequest;

const MAX_BATCH_FETCH_FILES: usize = 10_000;
const MAX_BATCH_FETCH_RESPONSE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Deserialize)]
pub struct BatchFetchRequest {
  pub paths: Option<Vec<String>>,
  pub items: Option<Vec<BatchFetchItem>>,
  pub max_bytes: Option<u64>,
  pub continue_on_error: Option<bool>,
  pub root_hash: Option<String>,
  pub snapshot: Option<String>,
  pub version: Option<String>,
}

#[derive(Clone, Deserialize)]
pub struct BatchFetchItem {
  pub id: Option<String>,
  pub path: String,
  pub if_content_hash: Option<String>,
  pub if_updated_at: Option<i64>,
  pub range: RangeExtractionRequest,
  pub max_bytes: Option<usize>,
}

#[derive(Debug, Serialize)]
struct BatchFetchRangeError {
  #[serde(skip_serializing_if = "Option::is_none")]
  id: Option<String>,
  path: String,
  status: &'static str,
  message: String,
}

/// POST /files/fetch — fetch multiple file bodies as a JSON object keyed by path.
pub async fn batch_fetch(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Extension(root_plan): Extension<RouteRootRequestPlanV1>,
  active_key_rules: Option<Extension<ActiveKeyRules>>,
  Json(body): Json<BatchFetchRequest>,
) -> Response {
  if let Err(error) = require_legacy_root_plan(root_plan, RootRequestAdapterV1::ResolveSingleRoot) {
    return error.into_response();
  }
  let selector_fields = RootSelectorFieldsV1 { root_hash: body.root_hash, snapshot: body.snapshot, version: body.version };
  match (body.paths, body.items) {
    (Some(_), Some(_)) => {
      return ErrorResponse::new("Provide either 'paths' for whole-file fetch or 'items' for range fetch, not both")
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
    (Some(paths), None) => {
      return batch_fetch_paths(state, claims, active_key_rules.map(|Extension(rules)| rules), paths, body.max_bytes, selector_fields).await
    }
    (None, Some(items)) => {
      return batch_fetch_range_items(
        state,
        claims,
        active_key_rules.map(|Extension(rules)| rules),
        items,
        body.max_bytes,
        body.continue_on_error.unwrap_or(false),
        selector_fields,
      )
      .await;
    }
    (None, None) => {
      return ErrorResponse::new("Provide either a non-empty 'paths' array or a non-empty 'items' array")
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  }
}

async fn batch_fetch_paths(
  state: AppState,
  claims: TokenClaims,
  active_key_rules: Option<ActiveKeyRules>,
  paths: Vec<String>,
  max_bytes: Option<u64>,
  selector_fields: RootSelectorFieldsV1,
) -> Response {
  if paths.is_empty() {
    return ErrorResponse::new("At least one path is required in the 'paths' array").with_status(StatusCode::BAD_REQUEST).into_response();
  }

  if paths.len() > MAX_BATCH_FETCH_FILES {
    return ErrorResponse::new(format!("Too many paths (max {}). Split the request into multiple batches", MAX_BATCH_FETCH_FILES))
      .with_status(StatusCode::BAD_REQUEST)
      .into_response();
  }

  let key_rules = active_key_rules.as_ref().map(|rules| rules.0.as_slice());
  for path in &paths {
    if let Err(error) = require_current_fetch_path(&state, &claims, key_rules, path) {
      return error.into_response();
    }
  }
  let selector = match parse_root_selector_v1(&selector_fields, state.engine.hash_algo()) {
    Ok(selector) => selector,
    Err(error) => return root_api_error_response(error, false),
  };
  if let Err(error) = reject_historical_share_selector(&claims, &selector) {
    return error.into_response();
  }

  let max_response_bytes = max_bytes.unwrap_or(MAX_BATCH_FETCH_RESPONSE_BYTES).min(MAX_BATCH_FETCH_RESPONSE_BYTES);
  let build_state = state.clone();
  let mut build_guard = ResponseBuildGuard::new();
  let cancellation = build_guard.cancellation();
  let build = tokio::task::spawn_blocking(move || {
    build_batch_fetch_paths(&build_state, &claims, &paths, max_response_bytes, &cancellation, &selector)
  })
  .await;
  build_guard.disarm();
  let output = match build {
    Ok(Ok(output)) => output,
    Ok(Err(error)) => return fetch_build_error_response(error),
    Err(error) => {
      tracing::error!("Batch fetch builder task panicked: {}", error);
      return ErrorResponse::new("Failed to build batch fetch response: internal task error")
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };
  stream_fetch_file(output, &state)
}

async fn batch_fetch_range_items(
  state: AppState,
  claims: TokenClaims,
  active_key_rules: Option<ActiveKeyRules>,
  items: Vec<BatchFetchItem>,
  max_bytes: Option<u64>,
  continue_on_error: bool,
  selector_fields: RootSelectorFieldsV1,
) -> Response {
  if items.is_empty() {
    return ErrorResponse::new("At least one item is required in the 'items' array").with_status(StatusCode::BAD_REQUEST).into_response();
  }

  if items.len() > MAX_BATCH_FETCH_FILES {
    return ErrorResponse::new(format!("Too many items (max {}). Split the request into multiple batches", MAX_BATCH_FETCH_FILES))
      .with_status(StatusCode::BAD_REQUEST)
      .into_response();
  }

  let key_rules = active_key_rules.as_ref().map(|rules| rules.0.as_slice());
  let mut current_path_authorizations = Vec::with_capacity(items.len());
  let mut has_currently_authorized_path = false;
  for item in &items {
    match current_fetch_path_is_authorized(&state, &claims, key_rules, &item.path) {
      Ok(true) => {
        current_path_authorizations.push(true);
        has_currently_authorized_path = true;
      }
      Ok(false) if continue_on_error => current_path_authorizations.push(false),
      Ok(false) => {
        return ErrorResponse::new(format!("Not found: {}", item.path)).with_status(StatusCode::NOT_FOUND).into_response();
      }
      Err(error) => return error.into_response(),
    }
  }
  if !has_currently_authorized_path {
    return ErrorResponse::new(format!("Not found: {}", items[0].path)).with_status(StatusCode::NOT_FOUND).into_response();
  }
  let selector = match parse_root_selector_v1(&selector_fields, state.engine.hash_algo()) {
    Ok(selector) => selector,
    Err(error) => return root_api_error_response(error, false),
  };
  if let Err(error) = reject_historical_share_selector(&claims, &selector) {
    return error.into_response();
  }

  let max_response_bytes = max_bytes.unwrap_or(MAX_BATCH_FETCH_RESPONSE_BYTES).min(MAX_BATCH_FETCH_RESPONSE_BYTES);
  let build_state = state.clone();
  let mut build_guard = ResponseBuildGuard::new();
  let cancellation = build_guard.cancellation();
  let build = tokio::task::spawn_blocking(move || {
    build_batch_fetch_ranges(BatchFetchRangeBuildRequest {
      state: &build_state,
      claims: &claims,
      items: &items,
      current_path_authorizations: &current_path_authorizations,
      max_response_bytes,
      continue_on_error,
      cancellation: &cancellation,
      selector: &selector,
    })
  })
  .await;
  build_guard.disarm();
  let output = match build {
    Ok(Ok(output)) => output,
    Ok(Err(error)) => return fetch_build_error_response(error),
    Err(error) => {
      tracing::error!("Batch range fetch builder task panicked: {}", error);
      return ErrorResponse::new("Failed to build batch range response: internal task error")
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };
  stream_fetch_file(output, &state)
}

fn require_current_fetch_path(
  state: &AppState,
  claims: &TokenClaims,
  key_rules: Option<&[crate::engine::api_key_rules::KeyRule]>,
  raw_path: &str,
) -> Result<(), RouteResponseError> {
  match current_fetch_path_is_authorized(state, claims, key_rules, raw_path)? {
    true => Ok(()),
    false => Err(ErrorResponse::new(format!("Not found: {raw_path}")).with_status(StatusCode::NOT_FOUND).into_response().into()),
  }
}

fn current_fetch_path_is_authorized(
  state: &AppState,
  claims: &TokenClaims,
  key_rules: Option<&[crate::engine::api_key_rules::KeyRule]>,
  raw_path: &str,
) -> Result<bool, RouteResponseError> {
  let normalized = normalize_path(raw_path);
  match current_path_read_is_authorized(state, claims, key_rules, &normalized) {
    Ok(authorized) => Ok(authorized),
    Err(error) => Err(engine_error_response("Failed to authorize batch fetch path", &error).into()),
  }
}

struct BuiltFetchResponse {
  file: tempfile::NamedTempFile,
  root: crate::engine::v4::read_view::ReadViewRootMetadataV1,
}

#[derive(Debug)]
enum FetchBuildError {
  Engine(EngineError),
  Root(LegacyV3RootAdapterErrorV1),
  Response(StatusCode, String),
  Io(String),
}

impl From<EngineError> for FetchBuildError {
  fn from(error: EngineError) -> Self {
    Self::Engine(error)
  }
}

fn build_batch_fetch_paths(
  state: &AppState,
  claims: &TokenClaims,
  paths: &[String],
  max_response_bytes: u64,
  cancellation: &ResponseBuildCancellation,
  selector: &RequestedRootSelectorV1,
) -> Result<BuiltFetchResponse, FetchBuildError> {
  cancellation.check()?;
  let selected = LegacyV3SelectedRootAdapterV1::resolve(&state.engine, selector).map_err(FetchBuildError::Root)?;
  let mut file = tempfile_for_engine(&state.engine, "fetch").map_err(|error| FetchBuildError::Io(error.to_string()))?;
  file.write_all(b"{").map_err(fetch_io)?;
  let mut cumulative_size = 0u64;
  let mut first = true;

  for raw_path in paths {
    cancellation.check()?;
    let normalized = normalize_path(raw_path);
    if !selected_path_is_authorized(state, claims, &selected, &normalized, CrudlifyOp::Read)? {
      return Err(FetchBuildError::Response(StatusCode::NOT_FOUND, format!("Not found: {raw_path}")));
    }
    let selected_file = match selected.file(&normalized) {
      Ok(selected_file) => selected_file,
      Err(EngineError::NotFound(_)) => {
        return Err(FetchBuildError::Response(StatusCode::NOT_FOUND, format!("Not found: {raw_path}")));
      }
      Err(error) => return Err(FetchBuildError::Engine(error)),
    };
    let record = &selected_file.record;
    cumulative_size = cumulative_size
      .checked_add(record.total_size)
      .ok_or_else(|| FetchBuildError::Response(StatusCode::PAYLOAD_TOO_LARGE, "Batch fetch response size overflowed".to_string()))?;
    if cumulative_size > max_response_bytes {
      return Err(FetchBuildError::Response(
        StatusCode::PAYLOAD_TOO_LARGE,
        format!("Batch fetch response would exceed {max_response_bytes} bytes. Split the request into smaller batches"),
      ));
    }

    if !first {
      file.write_all(b",").map_err(fetch_io)?;
    }
    first = false;
    serde_json::to_writer(&mut file, &record.path).map_err(fetch_json)?;
    file.write_all(b":{\"path\":").map_err(fetch_io)?;
    serde_json::to_writer(&mut file, &record.path).map_err(fetch_json)?;
    file.write_all(b",\"name\":").map_err(fetch_io)?;
    serde_json::to_writer(&mut file, file_name(&record.path).unwrap_or("")).map_err(fetch_json)?;
    write!(file, ",\"size\":{},\"created_at\":{},\"updated_at\":{}", record.total_size, record.created_at, record.updated_at)
      .map_err(fetch_io)?;
    file.write_all(b",\"content_type\":").map_err(fetch_io)?;
    serde_json::to_writer(&mut file, &record.content_type).map_err(fetch_json)?;
    file.write_all(b",\"content\":\"").map_err(fetch_io)?;
    let stream = selected.file_stream(&selected_file)?;
    write_lossy_json_content(&mut file, stream, &state.engine, cancellation)?;
    file.write_all(b"\"}").map_err(fetch_io)?;
  }
  file.write_all(b"}").map_err(fetch_io)?;
  Ok(BuiltFetchResponse { file, root: selected.root_metadata().clone() })
}

struct BatchFetchRangeBuildRequest<'request> {
  state: &'request AppState,
  claims: &'request TokenClaims,
  items: &'request [BatchFetchItem],
  current_path_authorizations: &'request [bool],
  max_response_bytes: u64,
  continue_on_error: bool,
  cancellation: &'request ResponseBuildCancellation,
  selector: &'request RequestedRootSelectorV1,
}

fn build_batch_fetch_ranges(request: BatchFetchRangeBuildRequest<'_>) -> Result<BuiltFetchResponse, FetchBuildError> {
  let BatchFetchRangeBuildRequest {
    state,
    claims,
    items,
    current_path_authorizations,
    max_response_bytes,
    continue_on_error,
    cancellation,
    selector,
  } = request;
  cancellation.check()?;
  if current_path_authorizations.len() != items.len() {
    return Err(FetchBuildError::Response(
      StatusCode::INTERNAL_SERVER_ERROR,
      "Batch range authorization result count did not match the item count".to_string(),
    ));
  }
  let selected = LegacyV3SelectedRootAdapterV1::resolve(&state.engine, selector).map_err(FetchBuildError::Root)?;
  let mut file = tempfile_for_engine(&state.engine, "fetch-range").map_err(|error| FetchBuildError::Io(error.to_string()))?;
  file.write_all(b"{\"items\":[").map_err(fetch_io)?;
  let mut cumulative_size = 0u64;
  let mut has_errors = false;

  for (index, (item, currently_authorized)) in items.iter().zip(current_path_authorizations).enumerate() {
    cancellation.check()?;
    if index > 0 {
      file.write_all(b",").map_err(fetch_io)?;
    }
    match fetch_one_range_item(state, claims, &selected, item, *currently_authorized) {
      Ok(value) => {
        let content_len = value.content_len();
        if cumulative_size.saturating_add(content_len) > max_response_bytes {
          let error = range_error_value(
            item,
            "too_large",
            format!("Batch fetch response would exceed {max_response_bytes} bytes. Split the request into smaller batches"),
          );
          if !continue_on_error {
            return Err(FetchBuildError::Response(
              StatusCode::PAYLOAD_TOO_LARGE,
              error["message"].as_str().unwrap_or("Range fetch response too large").to_string(),
            ));
          }
          has_errors = true;
          serde_json::to_writer(&mut file, &error).map_err(fetch_json)?;
        } else {
          cumulative_size += content_len;
          value.write_json(&mut file)?;
        }
      }
      Err(RangeFetchError::Response(status, error)) => {
        if !continue_on_error {
          return Err(FetchBuildError::Response(status, error.message));
        }
        has_errors = true;
        serde_json::to_writer(&mut file, &error).map_err(fetch_json)?;
      }
      Err(RangeFetchError::Engine(error)) => return Err(FetchBuildError::Engine(error)),
    }
    cancellation.check()?;
  }
  write!(file, "],\"has_errors\":{has_errors}}}").map_err(fetch_io)?;
  Ok(BuiltFetchResponse { file, root: selected.root_metadata().clone() })
}

fn write_lossy_json_content(
  writer: &mut impl Write,
  mut stream: EngineFileStream<'_>,
  engine: &crate::engine::StorageEngine,
  cancellation: &ResponseBuildCancellation,
) -> Result<(), FetchBuildError> {
  let mut pending = Vec::with_capacity(3);
  while let Some(chunk) = stream.next_reserved() {
    cancellation.check()?;
    let chunk = chunk?;
    let combined_len = pending
      .len()
      .checked_add(chunk.len())
      .ok_or_else(|| EngineError::ResourceExhausted("batch UTF-8 conversion length overflow".to_string()))?;
    let conversion_bytes = u64::try_from(combined_len)
      .ok()
      .and_then(|bytes| bytes.checked_mul(4))
      .ok_or_else(|| EngineError::ResourceExhausted("batch UTF-8 conversion estimate overflow".to_string()))?;
    let _conversion = reserve_streaming_read(engine, conversion_bytes, "batch UTF-8 conversion admission failed")?;
    let mut combined = Vec::with_capacity(combined_len);
    combined.extend_from_slice(&pending);
    combined.extend_from_slice(chunk.as_ref());
    let carry_len = incomplete_utf8_suffix_len(&combined);
    let complete_len = combined.len() - carry_len;
    let text = String::from_utf8_lossy(&combined[..complete_len]);
    write_json_escaped(writer, &text).map_err(fetch_io)?;
    pending.clear();
    pending.extend_from_slice(&combined[complete_len..]);
  }
  if !pending.is_empty() {
    let text = String::from_utf8_lossy(&pending);
    write_json_escaped(writer, &text).map_err(fetch_io)?;
  }
  Ok(())
}

fn incomplete_utf8_suffix_len(bytes: &[u8]) -> usize {
  if bytes.is_empty() || bytes[bytes.len() - 1].is_ascii() {
    return 0;
  }
  let mut lead_index = bytes.len() - 1;
  let mut continuation = 0usize;
  while bytes[lead_index] & 0b1100_0000 == 0b1000_0000 {
    continuation += 1;
    if continuation == 3 || lead_index == 0 {
      break;
    }
    lead_index -= 1;
  }
  let lead = bytes[lead_index];
  let expected = match lead {
    0xC2..=0xDF => 2,
    0xE0..=0xEF => 3,
    0xF0..=0xF4 => 4,
    _ => return 0,
  };
  let available = bytes.len() - lead_index;
  (available < expected).then_some(available).unwrap_or(0)
}

fn write_json_escaped(writer: &mut impl Write, text: &str) -> std::io::Result<()> {
  for character in text.chars() {
    match character {
      '"' => writer.write_all(br#"\""#)?,
      '\\' => writer.write_all(br#"\\"#)?,
      '\u{08}' => writer.write_all(br#"\b"#)?,
      '\u{0C}' => writer.write_all(br#"\f"#)?,
      '\n' => writer.write_all(br#"\n"#)?,
      '\r' => writer.write_all(br#"\r"#)?,
      '\t' => writer.write_all(br#"\t"#)?,
      character if character <= '\u{1F}' => write!(writer, "\\u{:04x}", character as u32)?,
      character => {
        let mut encoded = [0u8; 4];
        writer.write_all(character.encode_utf8(&mut encoded).as_bytes())?;
      }
    }
  }
  Ok(())
}

fn stream_fetch_file(output: BuiltFetchResponse, state: &AppState) -> Response {
  let (body, content_length) = match body_from_tempfile(output.file, std::sync::Arc::clone(&state.engine)) {
    Ok(response) => response,
    Err(error) => return engine_error_response("Failed to stream batch fetch response", &error),
  };
  let response = axum::http::Response::builder()
    .status(StatusCode::OK)
    .header("content-type", "application/json")
    .header("content-length", content_length.to_string())
    .body(body)
    .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build batch fetch response").into_response());
  attach_root_metadata_headers(response, &output.root, state.engine.hash_algo())
}

fn fetch_build_error_response(error: FetchBuildError) -> Response {
  match error {
    FetchBuildError::Engine(error) => engine_error_response("Failed to build batch fetch response", &error),
    FetchBuildError::Root(error) => legacy_root_adapter_error_response(error),
    FetchBuildError::Response(status, message) => ErrorResponse::new(message).with_status(status).into_response(),
    FetchBuildError::Io(error) => {
      tracing::error!("Failed to build batch fetch response: {}", error);
      ErrorResponse::new("Failed to build batch fetch response").with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response()
    }
  }
}

fn fetch_io(error: std::io::Error) -> FetchBuildError {
  FetchBuildError::Io(error.to_string())
}

fn fetch_json(error: serde_json::Error) -> FetchBuildError {
  FetchBuildError::Io(error.to_string())
}

#[derive(Debug)]
enum RangeFetchError {
  Response(StatusCode, BatchFetchRangeError),
  Engine(EngineError),
}

struct FetchedRangeItem {
  id: Option<String>,
  path: String,
  name: String,
  size: u64,
  created_at: i64,
  updated_at: i64,
  content_hash: String,
  extracted: crate::engine::range_extract::ExtractedRange,
}

#[derive(Serialize)]
struct FetchedRangeWire<'a> {
  id: &'a Option<String>,
  path: &'a str,
  name: &'a str,
  size: u64,
  created_at: i64,
  updated_at: i64,
  content_hash: &'a str,
  content_type: &'a str,
  range: FetchedRangeDescriptor<'a>,
  source_size: u64,
  content: &'a str,
  truncated: bool,
  status: &'static str,
}

#[derive(Serialize)]
struct FetchedRangeDescriptor<'a> {
  mode: &'a str,
  start: Option<u64>,
  end: Option<u64>,
  pointer: Option<&'a str>,
}

impl FetchedRangeItem {
  fn content_len(&self) -> u64 {
    self.extracted.content.len() as u64
  }

  fn write_json(&self, writer: &mut impl Write) -> Result<(), FetchBuildError> {
    let wire = FetchedRangeWire {
      id: &self.id,
      path: &self.path,
      name: &self.name,
      size: self.size,
      created_at: self.created_at,
      updated_at: self.updated_at,
      content_hash: &self.content_hash,
      content_type: &self.extracted.content_type,
      range: FetchedRangeDescriptor {
        mode: self.extracted.mode.as_str(),
        start: self.extracted.start,
        end: self.extracted.end,
        pointer: self.extracted.pointer.as_deref(),
      },
      source_size: self.extracted.source_size,
      content: &self.extracted.content,
      truncated: self.extracted.truncated,
      status: "ok",
    };
    serde_json::to_writer(writer, &wire).map_err(fetch_json)
  }
}

fn fetch_one_range_item(
  state: &AppState,
  claims: &TokenClaims,
  selected: &LegacyV3SelectedRootAdapterV1<'_>,
  item: &BatchFetchItem,
  currently_authorized: bool,
) -> Result<FetchedRangeItem, RangeFetchError> {
  let normalized = normalize_path(&item.path);

  if !currently_authorized {
    return Err(RangeFetchError::Response(StatusCode::NOT_FOUND, range_error(item, "not_found", format!("Not found: {}", item.path))));
  }
  if !selected_path_is_authorized(state, claims, selected, &normalized, CrudlifyOp::Read).map_err(RangeFetchError::Engine)? {
    return Err(RangeFetchError::Response(StatusCode::NOT_FOUND, range_error(item, "not_found", format!("Not found: {}", item.path))));
  }

  let selected_file = match selected.file(&normalized) {
    Ok(selected_file) => selected_file,
    Err(EngineError::NotFound(_)) => {
      return Err(RangeFetchError::Response(StatusCode::NOT_FOUND, range_error(item, "not_found", format!("Not found: {}", item.path))));
    }
    Err(error) => {
      tracing::error!("Batch range fetch: failed to get metadata for '{}': {}", normalized, error);
      return Err(RangeFetchError::Response(
        StatusCode::INTERNAL_SERVER_ERROR,
        range_error(
          item,
          "error",
          format!("Failed to read metadata for '{}'. The file may be corrupted — contact your administrator", item.path),
        ),
      ));
    }
  };
  let file_record = &selected_file.record;

  let content_hash = file_record.content_hash_hex();
  if let Some(expected) = item.if_content_hash.as_deref() {
    if expected != content_hash {
      return Err(RangeFetchError::Response(StatusCode::CONFLICT, range_error(item, "stale", "File content hash changed".to_string())));
    }
  }

  if let Some(expected_updated_at) = item.if_updated_at {
    if expected_updated_at != file_record.updated_at {
      return Err(RangeFetchError::Response(StatusCode::CONFLICT, range_error(item, "stale", "File updated_at changed".to_string())));
    }
  }

  let mut range_request = item.range.clone();
  if item.max_bytes.is_some() {
    range_request.max_bytes = item.max_bytes;
  }

  let extracted = match selected.extract_range(&selected_file, &range_request) {
    Ok(extracted) => extracted,
    Err(EngineError::NotFound(_)) => {
      return Err(RangeFetchError::Response(StatusCode::NOT_FOUND, range_error(item, "not_found", format!("Not found: {}", item.path))));
    }
    Err(EngineError::InvalidInput(message)) | Err(EngineError::JsonParseError(message)) => {
      return Err(RangeFetchError::Response(StatusCode::BAD_REQUEST, range_error(item, "invalid", message)));
    }
    Err(error @ EngineError::ResourceExhausted(_)) => return Err(RangeFetchError::Engine(error)),
    Err(error) => {
      tracing::error!("Batch range fetch: failed to read range for '{}': {}", normalized, error);
      return Err(RangeFetchError::Response(
        StatusCode::INTERNAL_SERVER_ERROR,
        range_error(
          item,
          "error",
          format!("Failed to read file '{}'. The file data may be corrupted — contact your administrator", item.path),
        ),
      ));
    }
  };

  let name = file_name(&file_record.path).unwrap_or("").to_string();
  Ok(FetchedRangeItem {
    id: item.id.clone(),
    path: file_record.path.clone(),
    name,
    size: file_record.total_size,
    created_at: file_record.created_at,
    updated_at: file_record.updated_at,
    content_hash,
    extracted,
  })
}

fn range_error(item: &BatchFetchItem, status: &'static str, message: String) -> BatchFetchRangeError {
  BatchFetchRangeError { id: item.id.clone(), path: normalize_path(&item.path), status, message }
}

fn range_error_value(item: &BatchFetchItem, status: &'static str, message: String) -> serde_json::Value {
  serde_json::to_value(range_error(item, status, message)).unwrap_or_else(|_| serde_json::json!({"status": status}))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::engine::directory_ops::DirectoryOps;
  use crate::engine::memory_coordinator::MemoryOwner;
  use crate::engine::range_extract::{RangeExtractionRequest, RangeMode, extract_range_from_record};
  use crate::engine::request_context::RequestContext;
  use crate::server::create_temp_engine_for_tests;

  struct ReservationInspectingWriter<'a> {
    engine: &'a crate::engine::StorageEngine,
    baseline_reservations: u64,
    observed_reservation: bool,
    bytes: Vec<u8>,
  }

  impl Write for ReservationInspectingWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
      let owner = self.engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().clone();
      self.observed_reservation |= owner.active_reservations > self.baseline_reservations;
      self.bytes.extend_from_slice(bytes);
      Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
      Ok(())
    }
  }

  #[test]
  fn range_item_serialization_retains_extraction_memory_until_writer_completion() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&RequestContext::system(), "/range.txt", &vec![b'x'; 512 * 1024], Some("text/plain")).unwrap();
    let record = ops.get_metadata("/range.txt").unwrap().unwrap();
    let baseline = engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().clone();
    let extracted = extract_range_from_record(
      &engine,
      &record,
      &RangeExtractionRequest { mode: RangeMode::Bytes, start: Some(0), end: None, pointer: None, max_bytes: Some(128 * 1024) },
    )
    .unwrap();
    let value = FetchedRangeItem {
      id: Some("proof".to_string()),
      path: record.path.clone(),
      name: "range.txt".to_string(),
      size: record.total_size,
      created_at: record.created_at,
      updated_at: record.updated_at,
      content_hash: record.content_hash_hex(),
      extracted,
    };
    let mut writer = ReservationInspectingWriter {
      engine: &engine,
      baseline_reservations: baseline.active_reservations,
      observed_reservation: false,
      bytes: Vec::new(),
    };

    value.write_json(&mut writer).unwrap();
    assert!(writer.observed_reservation, "range output reservation was released before serialization completed");
    let json: serde_json::Value = serde_json::from_slice(&writer.bytes).unwrap();
    assert_eq!(json["status"], "ok");
    assert_eq!(json["content"].as_str().unwrap().len(), 128 * 1024);
    drop(value);

    let released = engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().clone();
    assert_eq!(released.active_reservations, baseline.active_reservations);
    assert_eq!(released.reserved_bytes, baseline.reserved_bytes);
  }
}
