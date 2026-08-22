use axum::{
  Extension,
  body::Body,
  extract::{Path, Query as AxumQuery, State},
  http::{header, HeaderMap, StatusCode},
  response::{IntoResponse, Response},
  Json,
};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use super::blocking::run_engine_blocking;
use super::route_permissions::{reject_share_key, require_generic_data_engine_path, require_generic_data_path, RoutePermissionChecker};
use super::responses::{engine_error_response, EngineFileResponse, ErrorResponse};
use super::search_locators::{
  broad_query_terms, terms_from_query_node, try_generate_locators_with_budget, LocatorOptions, LocatorOptionsRequest, LocatorTerm,
};
use super::state::AppState;
use crate::auth::TokenClaims;
use crate::auth::permission_middleware::ActiveKeyRules;
use crate::engine::api_key_rules::{match_rules, check_operation_permitted};
use crate::engine::{DirectoryOps, RequestContext, SearchResult, StorageEngine, TaskStatus, VersionManager, is_root};
use crate::engine::directory_listing::list_directory_recursive_strict;
use crate::engine::directory_ops::{
  read_chunk_reserved, reserve_streaming_read, stream_hash_inventory_bytes, streaming_memory_error, file_content_hash, ReservedReadChunk,
};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::directory_ops::StreamingReadReservation;
use crate::engine::ChunkReadLocation;
use crate::engine::index_config::PathIndexConfig;
use crate::engine::permission_resolver::CrudlifyOp;
use crate::engine::query_engine::{
  parse_where_clause, AggregateQuery, ExplainMode, Query, QueryEngine, QueryMeta, QueryNode, QueryResult, QueryStrategy, SortDirection,
  SortField, DEFAULT_QUERY_LIMIT,
};
use crate::engine::system_family_policy::GenericDataPathSelection;
use crate::engine::symlink_resolver::{resolve_symlink, ResolvedTarget};
use crate::engine::SystemFamilyPolicyResolver;

/// Check if a file path is deleted and the user lacks delete permission.
/// Deleted files are invisible/inaccessible to users without 'd' permission.
fn is_deleted_and_forbidden(state: &AppState, claims: &TokenClaims, path: &str) -> EngineResult<bool> {
  use crate::engine::directory_ops::file_path_hash;

  // Check if the file is deleted in the KV store
  let algo = state.engine.hash_algo();
  let normalized = crate::engine::path_utils::normalize_path(path);
  let file_key = file_path_hash(&normalized, &algo)?;

  let is_deleted = state.engine.is_entry_deleted(&file_key)?;
  if !is_deleted {
    return Ok(false);
  }

  // Share credentials have no user-level Delete grant. A deleted path is
  // therefore always concealed even when the key's path rule still matches.
  if claims.sub.starts_with("share:") {
    return Ok(true);
  }

  let user_id = Uuid::parse_str(&claims.sub).map_err(|error| EngineError::InvalidInput(format!("invalid user identity: {error}")))?;

  // Root can see everything.
  if is_root(&user_id) {
    return Ok(false);
  }

  // File is deleted — check if user has 'd' permission
  let has_delete = RoutePermissionChecker::for_user(state, user_id).has_permission(&normalized, CrudlifyOp::Delete)?;

  Ok(!has_delete)
}

/// Query parameters for GET /files/*path (version access + directory listing).
#[derive(Deserialize, Default)]
pub struct EngineGetQuery {
  pub snapshot: Option<String>,
  pub version: Option<String>,
  pub depth: Option<i32>,
  pub glob: Option<String>,
  pub nofollow: Option<bool>,
  pub limit: Option<usize>,
  pub offset: Option<usize>,
  /// Sort field: "name", "size", "created_at", "updated_at" (default: "name")
  pub sort: Option<String>,
  /// Sort order: "asc" or "desc" (default: "asc")
  pub order: Option<String>,
}

/// Filter a listing of JSON entries based on active API key rules.
/// Entries whose "path" field is denied (no matching rule, or matched rule
/// forbids the given operation) are silently removed.
fn filter_listing_by_key_rules(entries: &mut Vec<serde_json::Value>, rules: &[crate::engine::api_key_rules::KeyRule], operation: char) {
  entries.retain_mut(|entry| {
    let path = entry["path"].as_str().unwrap_or("").to_string();

    // Order of precedence:
    // 1. If the item matches an explicit rule (not the catch-all `**`),
    //    that rule decides: drop unless the rule grants the operation.
    //    This is the case that the old code got wrong — it would route
    //    "denied" matches into the shared-path branch and keep them.
    // 2. Otherwise, if the item is an ANCESTOR of any rule's target
    //    (e.g. `/foo/` when the rule is on `/foo/bar/*`), allow it for
    //    navigation only with `-r--l---` perms.
    // 3. Otherwise, drop.
    match match_rules(rules, &path) {
      Some(rule) if rule.glob != "**" => {
        if check_operation_permitted(&rule.permitted, operation) {
          if let Some(obj) = entry.as_object_mut() {
            obj.insert("effective_permissions".to_string(), serde_json::Value::String(rule.permitted.clone()));
          }
          true
        } else {
          false
        }
      }
      _ => {
        // No explicit rule (or only the catch-all matched). Allow
        // navigation if this is an ancestor of a scoped target.
        if crate::engine::api_key_rules::is_item_on_shared_path(rules, &path) {
          if let Some(obj) = entry.as_object_mut() {
            obj.insert("effective_permissions".to_string(), serde_json::Value::String("-r--l---".to_string()));
          }
          true
        } else {
          false
        }
      }
    }
  });
}

/// Apply limit/offset pagination to a listing and return a JSON response
/// with `items`, `total`, `limit`, and `offset` fields.
fn paginated_listing_response(
  mut listing: Vec<serde_json::Value>,
  limit: Option<usize>,
  offset: Option<usize>,
  sort: Option<&str>,
  order: Option<&str>,
) -> Response {
  // Sort before pagination
  let sort_field = sort.unwrap_or("name");
  let descending = order.map(|o| o == "desc").unwrap_or(false);

  listing.sort_by(|a, b| {
    let a_is_dir = a["entry_type"].as_u64().map(|entry_type| entry_type == EntryType::DirectoryIndex.to_u8() as u64).unwrap_or(false);
    let b_is_dir = b["entry_type"].as_u64().map(|entry_type| entry_type == EntryType::DirectoryIndex.to_u8() as u64).unwrap_or(false);

    let category_cmp = match (a_is_dir, b_is_dir) {
      (true, false) => std::cmp::Ordering::Less,
      (false, true) => std::cmp::Ordering::Greater,
      _ => std::cmp::Ordering::Equal,
    };
    if category_cmp != std::cmp::Ordering::Equal {
      return category_cmp;
    }

    let cmp = match sort_field {
      "size" => {
        let a_size = a["size"].as_u64().unwrap_or(0);
        let b_size = b["size"].as_u64().unwrap_or(0);
        a_size.cmp(&b_size)
      }
      "created_at" => {
        let a_ts = a["created_at"].as_i64().or_else(|| a["created_at"].as_u64().map(|v| v as i64)).unwrap_or(0);
        let b_ts = b["created_at"].as_i64().or_else(|| b["created_at"].as_u64().map(|v| v as i64)).unwrap_or(0);
        a_ts.cmp(&b_ts)
      }
      "updated_at" => {
        let a_ts = a["updated_at"].as_i64().or_else(|| a["updated_at"].as_u64().map(|v| v as i64)).unwrap_or(0);
        let b_ts = b["updated_at"].as_i64().or_else(|| b["updated_at"].as_u64().map(|v| v as i64)).unwrap_or(0);
        a_ts.cmp(&b_ts)
      }
      _ => {
        // Default: sort by name (case-insensitive)
        let a_name = a["name"].as_str().unwrap_or("").to_lowercase();
        let b_name = b["name"].as_str().unwrap_or("").to_lowercase();
        a_name.cmp(&b_name)
      }
    };
    if descending {
      cmp.reverse()
    } else {
      cmp
    }
  });

  let total = listing.len();
  let off = offset.unwrap_or(0).min(total);
  listing = listing.split_off(off);
  if let Some(lim) = limit {
    listing.truncate(lim);
  }
  (
    StatusCode::OK,
    Json(serde_json::json!({
      "items": listing,
      "total": total,
      "limit": limit,
      "offset": off,
    })),
  )
    .into_response()
}

// ---------------------------------------------------------------------------
// Engine file routes
// ---------------------------------------------------------------------------

// Upload streaming: the PUT handler reads the body in 256KB chunks and stores
// each chunk individually. The full file is never in memory at once.

// ---------------------------------------------------------------------------
// POST /files/mkdir — create an empty directory
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct MkdirRequest {
  pub path: String,
}

pub async fn mkdir(State(state): State<AppState>, Extension(claims): Extension<TokenClaims>, Json(body): Json<MkdirRequest>) -> Response {
  let normalized = crate::engine::path_utils::normalize_path(&body.path);

  if let Err(response) = require_generic_data_path(&state, &normalized) {
    return response;
  }

  if normalized == "/" {
    return ErrorResponse::new("Cannot create root directory").with_status(StatusCode::BAD_REQUEST).into_response();
  }

  // User/group permission check: /files/mkdir is exempt from path-aware
  // middleware, so without this every authenticated user could create
  // directories anywhere. Required: Create on the parent directory.
  // Share keys (claims.sub starts with "share:") fall back to their own
  // key-rule enforcement upstream and don't carry user permissions; we
  // refuse them here.
  if let Err(response) = reject_share_key(&claims, "Share keys cannot create directories") {
    return response;
  }
  let permissions = match RoutePermissionChecker::from_claims(&state, &claims, "Invalid user identity") {
    Ok(permissions) => permissions,
    Err(response) => return response,
  };
  if !permissions.is_root() {
    let parent = crate::engine::path_utils::parent_path(&normalized).unwrap_or_else(|| "/".to_string());
    let permitted = match permissions.has_path_permission(&parent, CrudlifyOp::Create) {
      Ok(permitted) => permitted,
      Err(error) => return engine_error_response("Failed to check create permission", &error),
    };
    if !permitted {
      return ErrorResponse::new("Permission denied").with_status(StatusCode::FORBIDDEN).into_response();
    }
  }

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());

  let engine = state.engine.clone();
  let normalized_for_blocking = normalized.clone();
  let result = run_engine_blocking("create_directory", "Failed to create directory", move || {
    let ops = DirectoryOps::new(&engine);
    ops.create_directory(&ctx, &normalized_for_blocking)
  })
  .await;

  match result {
    Ok(()) => (
      StatusCode::CREATED,
      Json(serde_json::json!({
        "path": normalized,
        "entry_type": 3,
        "created": true,
      })),
    )
      .into_response(),
    Err(response) => response,
  }
}

/// PUT /engine/*path -- store a file via the custom storage engine.
///
/// Accepts the request body as a stream and buffers up to
/// The body is streamed in 256KB chunks and stored individually —
/// the full file is never buffered in memory. Supports files up to
/// the router-level body limit (10 GB).
pub async fn engine_store_file(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(path): Path<String>,
  headers: HeaderMap,
  body: Body,
) -> Response {
  if let Err(response) = require_generic_data_path(&state, &path) {
    return response;
  }

  // Stream the body in 256KB chunks — each chunk is stored to disk as it
  // arrives. Only the 32-byte hash is kept in memory, not the chunk data.
  // Memory usage: ~32 bytes per chunk regardless of file size.
  let chunk_size = crate::engine::directory_ops::DEFAULT_CHUNK_SIZE;
  let directory_ops = DirectoryOps::new(&state.engine);
  let mut chunk_hashes: Vec<Vec<u8>> = Vec::new();
  let mut buffer = Vec::with_capacity(chunk_size);
  let mut first_bytes = Vec::new();
  let mut total_size: u64 = 0;
  let mut data_stream = body.into_data_stream();

  while let Some(chunk_result) = data_stream.next().await {
    match chunk_result {
      Ok(data) => {
        // Capture first bytes for content-type detection
        if first_bytes.len() < 8192 {
          let need = (8192 - first_bytes.len()).min(data.len());
          first_bytes.extend_from_slice(&data[..need]);
        }

        let mut offset = 0;
        while offset < data.len() {
          let space = chunk_size - buffer.len();
          let take = space.min(data.len() - offset);
          buffer.extend_from_slice(&data[offset..offset + take]);
          offset += take;

          if buffer.len() >= chunk_size {
            total_size += buffer.len() as u64;
            let filled = std::mem::replace(&mut buffer, Vec::with_capacity(chunk_size));
            match directory_ops.store_chunk(&filled) {
              Ok(hash) => chunk_hashes.push(hash),
              Err(error) => {
                tracing::error!("Failed to store chunk: {}", error);
                return ErrorResponse::new("Failed to store upload chunk").with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response();
              }
            }
          }
        }
      }
      Err(_error) => {
        return ErrorResponse::new("Failed to read request body: the upload stream was interrupted or contained invalid data")
          .with_status(StatusCode::BAD_REQUEST)
          .into_response();
      }
    }
  }

  // Flush remaining buffer as the last chunk
  if !buffer.is_empty() {
    total_size += buffer.len() as u64;
    match directory_ops.store_chunk(&buffer) {
      Ok(hash) => chunk_hashes.push(hash),
      Err(error) => {
        tracing::error!("Failed to store final chunk: {}", error);
        return ErrorResponse::new("Failed to store upload chunk").with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response();
      }
    }
  }

  let content_type = headers.get("content-type").and_then(|value| value.to_str().ok()).map(|s| s.to_string());

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());

  // Move the fsync-heavy finalize off the async runtime so we don't block other
  // requests sharing this worker thread while we wait for disk.
  let engine_for_blocking = state.engine.clone();
  let path_for_blocking = path.clone();
  let ctx_for_blocking = ctx.clone();
  let first_bytes_owned = first_bytes;
  let chunk_hashes_owned = chunk_hashes;
  let file_record = match run_engine_blocking("finalize_file", "Failed to store file", move || {
    let ops = DirectoryOps::new(&engine_for_blocking);
    ops.finalize_file(&ctx_for_blocking, &path_for_blocking, chunk_hashes_owned, total_size, content_type.as_deref(), &first_bytes_owned)
  })
  .await
  {
    Ok(record) => record,
    Err(response) => return response,
  };

  // Auto-trigger reindex when indexes.json is stored. The file mutation is
  // already durably acknowledged, so follow-up failures are soft evidence and
  // must never turn the committed write into a retryable HTTP failure.
  if path.ends_with("/.aeordb-config/indexes.json") {
    let scheduling_engine = state.engine.clone();
    let scheduling_queue = state.task_queue.clone();
    let scheduling_path = path.clone();
    if let Err(error) = tokio::task::spawn_blocking(move || {
      schedule_automatic_reindex_after_commit(&scheduling_engine, scheduling_queue.as_deref(), &scheduling_path);
    })
    .await
    {
      crate::metrics::record_system_soft_failure("automatic_reindex", "worker_join", &path, error);
    }
  }

  let algo = state.engine.hash_algo();
  let response_body = match engine_file_response_with_hash(&file_record, algo) {
    Ok(response) => response,
    Err(error) => {
      tracing::error!(path, %error, "Stored file but could not construct its HTTP response hash");
      return ErrorResponse::new(
        "The file was saved, but its response hash could not be constructed; inspect server health before retrying".to_string(),
      )
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response();
    }
  };

  (StatusCode::CREATED, Json(response_body)).into_response()
}

fn schedule_automatic_reindex_after_commit(engine: &StorageEngine, queue: Option<&crate::engine::TaskQueue>, config_path: &str) {
  let Some(queue) = queue else {
    crate::metrics::record_system_soft_failure("automatic_reindex", "queue_unavailable", config_path, "task queue is unavailable");
    return;
  };

  let parent = config_path.trim_end_matches("/.aeordb-config/indexes.json");
  let parent = if parent.is_empty() { "/" } else { parent };
  let reindex_path = format!("/{}", parent.trim_start_matches('/'));

  match queue.list_tasks() {
    Ok(tasks) => {
      for task in tasks {
        if task.task_type == "reindex"
          && task.args.get("path").and_then(serde_json::Value::as_str) == Some(&reindex_path)
          && (task.status == TaskStatus::Pending || task.status == TaskStatus::Running)
        {
          if let Err(error) = queue.cancel(&task.id) {
            crate::metrics::record_system_soft_failure(
              "automatic_reindex",
              "task_cancel",
              format_args!("config={config_path}, task={}", task.id),
              error,
            );
          }
        }
      }
    }
    Err(error) => crate::metrics::record_system_soft_failure("automatic_reindex", "task_list", config_path, error),
  }

  let metadata_only = match DirectoryOps::new(engine).read_file_buffered(config_path) {
    Ok(data) => match PathIndexConfig::deserialize(&data) {
      Ok(config) => config.indexes.iter().all(|field| field.name.starts_with('@')),
      Err(error) => {
        crate::metrics::record_system_soft_failure("automatic_reindex", "config_decode", config_path, error);
        false
      }
    },
    Err(error) => {
      crate::metrics::record_system_soft_failure("automatic_reindex", "config_read", config_path, error);
      false
    }
  };

  if let Err(error) = queue.enqueue("reindex", serde_json::json!({"path": reindex_path, "metadata_only": metadata_only})) {
    crate::metrics::record_system_soft_failure("automatic_reindex", "task_enqueue", config_path, error);
  }
}

// ---------------------------------------------------------------------------
// engine_get helper functions
// ---------------------------------------------------------------------------

/// Build a streaming HTTP response from a file's chunk hashes.
///
/// Constructs the standard response with X-AeorDB-Path, X-AeorDB-Size,
/// X-AeorDB-Created, X-AeorDB-Updated headers. If `symlink_target` is
/// provided, adds an X-AeorDB-Link-Target header as well.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HttpByteRange {
  start: u64,
  end: u64,
}

impl HttpByteRange {
  fn len(self) -> u64 {
    self.end.saturating_sub(self.start).saturating_add(1)
  }
}

fn parse_range_header(headers: &HeaderMap, total_size: u64) -> Result<Option<HttpByteRange>, ()> {
  let Some(value) = headers.get(header::RANGE) else {
    return Ok(None);
  };
  let value = value.to_str().map_err(|_| ())?.trim();
  let Some(spec) = value.strip_prefix("bytes=") else {
    return Ok(None);
  };
  if total_size == 0 || spec.contains(',') {
    return Err(());
  }

  let (start_spec, end_spec) = spec.split_once('-').ok_or(())?;
  let last_byte = total_size - 1;

  let (start, end) = if start_spec.is_empty() {
    let suffix_len: u64 = end_spec.parse().map_err(|_| ())?;
    if suffix_len == 0 {
      return Err(());
    }
    let start = total_size.saturating_sub(suffix_len);
    (start, last_byte)
  } else {
    let start: u64 = start_spec.parse().map_err(|_| ())?;
    if start > last_byte {
      return Err(());
    }
    let end = if end_spec.is_empty() {
      last_byte
    } else {
      let requested_end: u64 = end_spec.parse().map_err(|_| ())?;
      if requested_end < start {
        return Err(());
      }
      requested_end.min(last_byte)
    };
    (start, end)
  };

  Ok(Some(HttpByteRange { start, end }))
}

fn range_not_satisfiable_response(total_size: u64) -> Response {
  axum::http::Response::builder()
    .status(StatusCode::RANGE_NOT_SATISFIABLE)
    .header(header::ACCEPT_RANGES, "bytes")
    .header(header::CONTENT_RANGE, format!("bytes */{}", total_size))
    .body(Body::empty())
    .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response").into_response())
}

const RANGE_READ_SPAN_MAX_GAP_BYTES: u64 = 256 * 1024;
const DEFAULT_READ_PREFETCH_BYTES: u64 = 2_621_440;
const DEFAULT_READ_COALESCE_MAX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy)]
struct ReadStreamLimits {
  prefetch_bytes: u64,
  coalesce_max_bytes: u64,
}

impl ReadStreamLimits {
  fn resolve(engine: &StorageEngine) -> EngineResult<Self> {
    let prefetch_bytes = engine.resolved_unsigned_config("io.read_prefetch_bytes").unwrap_or(DEFAULT_READ_PREFETCH_BYTES);
    let coalesce_max_bytes = engine.resolved_unsigned_config("io.read_coalesce_max_bytes").unwrap_or(DEFAULT_READ_COALESCE_MAX_BYTES);
    if prefetch_bytes == 0 || coalesce_max_bytes < prefetch_bytes {
      return Err(EngineError::InvalidInput(format!(
        "Invalid streaming read limits: prefetch={}, coalesce={}",
        prefetch_bytes, coalesce_max_bytes
      )));
    }
    usize::try_from(prefetch_bytes)
      .and_then(|_| usize::try_from(coalesce_max_bytes))
      .map_err(|_| EngineError::ResourceExhausted("streaming read limits exceed this platform's address space".to_string()))?;
    Ok(Self { prefetch_bytes, coalesce_max_bytes })
  }
}

struct LegacyEngineByteRangeStream {
  chunk_hashes: Vec<Vec<u8>>,
  engine: std::sync::Arc<StorageEngine>,
  include_deleted: bool,
  current_index: usize,
  cursor: u64,
  range_start: u64,
  range_end_exclusive: u64,
  _inventory_reservation: StreamingReadReservation,
}

impl LegacyEngineByteRangeStream {
  fn new(
    chunk_hashes: Vec<Vec<u8>>,
    engine: std::sync::Arc<StorageEngine>,
    include_deleted: bool,
    range: HttpByteRange,
  ) -> EngineResult<Self> {
    let inventory_bytes = stream_hash_inventory_bytes(&chunk_hashes, chunk_hashes.capacity())?;
    let inventory_reservation = reserve_streaming_read(&engine, inventory_bytes, "range stream inventory admission failed")?;
    Ok(Self {
      chunk_hashes,
      engine,
      include_deleted,
      current_index: 0,
      cursor: 0,
      range_start: range.start,
      range_end_exclusive: range.end.saturating_add(1),
      _inventory_reservation: inventory_reservation,
    })
  }

  fn chunk_metadata_len(&self, hash: &[u8]) -> EngineResult<Option<u64>> {
    self
      .engine
      .get_chunk_stream_metadata(hash, self.include_deleted)?
      .map(|metadata| metadata.raw_value_length)
      .ok_or_else(|| EngineError::NotFound(format!("Chunk not found: {}", hex::encode(hash))))
  }

  fn reserve_slice(&self, chunk: ReservedReadChunk, start: usize, end: usize, offset: u64) -> EngineResult<ReservedReadChunk> {
    if start == 0 && end == chunk.len() {
      return Ok(chunk);
    }
    let output_len = end.checked_sub(start).ok_or_else(|| EngineError::InvalidInput("Streaming range slice underflowed".to_string()))?;
    let admitted_bytes = u64::try_from(output_len)
      .ok()
      .and_then(|bytes| bytes.checked_add(std::mem::size_of::<ReservedReadChunk>() as u64))
      .ok_or_else(|| EngineError::ResourceExhausted("streaming range output estimate overflow".to_string()))?;
    let mut reservation = reserve_streaming_read(&self.engine, admitted_bytes, "range output admission failed")?;
    let mut data = Vec::with_capacity(output_len);
    data.extend_from_slice(&chunk.as_ref()[start..end]);
    drop(chunk);

    let retained_bytes = u64::try_from(data.capacity())
      .ok()
      .and_then(|bytes| bytes.checked_add(std::mem::size_of::<ReservedReadChunk>() as u64))
      .ok_or_else(|| EngineError::ResourceExhausted("streaming range output accounting overflow".to_string()))?;
    if retained_bytes > reservation.bytes() {
      return Err(EngineError::CorruptEntry {
        offset,
        reason: format!("Range output allocation {} exceeds admitted {} bytes", retained_bytes, reservation.bytes()),
      });
    }
    if retained_bytes < reservation.bytes() {
      reservation
        .shrink(reservation.bytes() - retained_bytes)
        .map_err(|error| streaming_memory_error("range output accounting failed", error))?;
    }
    Ok(ReservedReadChunk::from_admitted(data, reservation))
  }
}

impl Iterator for LegacyEngineByteRangeStream {
  type Item = EngineResult<ReservedReadChunk>;

  fn next(&mut self) -> Option<Self::Item> {
    while self.current_index < self.chunk_hashes.len() && self.cursor < self.range_end_exclusive {
      let hash = &self.chunk_hashes[self.current_index];
      self.current_index += 1;
      let chunk_start = self.cursor;
      let mut decoded_chunk = None;
      let chunk_len = match self.chunk_metadata_len(hash) {
        Ok(Some(chunk_len)) => chunk_len,
        Ok(None) => match read_chunk_reserved(&self.engine, hash, self.include_deleted) {
          Ok(chunk) => {
            let chunk_len = chunk.len() as u64;
            decoded_chunk = Some(chunk);
            chunk_len
          }
          Err(error) => return Some(Err(error)),
        },
        Err(error) => return Some(Err(error)),
      };
      let chunk_end = match chunk_start.checked_add(chunk_len) {
        Some(end) => end,
        None => {
          return Some(Err(EngineError::InvalidInput("File chunk offsets overflowed while serving byte range".to_string())));
        }
      };
      self.cursor = chunk_end;

      if chunk_end <= self.range_start {
        continue;
      }
      if chunk_start >= self.range_end_exclusive {
        return None;
      }

      let start_in_chunk = self.range_start.saturating_sub(chunk_start).min(chunk_len) as usize;
      let end_in_chunk = self.range_end_exclusive.saturating_sub(chunk_start).min(chunk_len) as usize;
      if start_in_chunk >= end_in_chunk {
        continue;
      }

      let chunk = match decoded_chunk {
        Some(chunk) => chunk,
        None => match read_chunk_reserved(&self.engine, hash, self.include_deleted) {
          Ok(chunk) => chunk,
          Err(error) => return Some(Err(error)),
        },
      };
      if chunk.len() as u64 != chunk_len {
        return Some(Err(EngineError::CorruptEntry {
          offset: chunk_start,
          reason: format!("Chunk length changed while serving range: metadata {}, decoded {}", chunk_len, chunk.len()),
        }));
      }
      return Some(self.reserve_slice(chunk, start_in_chunk, end_in_chunk, chunk_start));
    }

    None
  }
}

#[derive(Debug, Clone)]
struct PlannedRangeChunk {
  hash: Vec<u8>,
  file_start: u64,
  file_end: u64,
  wal_offset: u64,
  wal_total_length: u32,
}

impl PlannedRangeChunk {
  fn file_len(&self) -> u64 {
    self.file_end.saturating_sub(self.file_start)
  }

  fn wal_end(&self) -> EngineResult<u64> {
    self
      .wal_offset
      .checked_add(self.wal_total_length as u64)
      .ok_or_else(|| EngineError::InvalidInput("Chunk WAL offset overflowed while planning byte range".to_string()))
  }

  fn location(&self) -> ChunkReadLocation {
    ChunkReadLocation { hash: self.hash.clone(), offset: self.wal_offset, total_length: self.wal_total_length }
  }
}

struct CoalescedEngineByteRangeStream {
  chunks: Vec<PlannedRangeChunk>,
  engine: std::sync::Arc<StorageEngine>,
  next_index: usize,
  range_start: u64,
  range_end_exclusive: u64,
  limits: ReadStreamLimits,
  _inventory_reservation: StreamingReadReservation,
}

enum CoalescedStreamBuild {
  Ready(CoalescedEngineByteRangeStream),
  Legacy(Vec<Vec<u8>>),
}

impl CoalescedEngineByteRangeStream {
  fn new(
    chunk_hashes: Vec<Vec<u8>>,
    engine: std::sync::Arc<StorageEngine>,
    range: HttpByteRange,
    total_size: u64,
    limits: ReadStreamLimits,
  ) -> EngineResult<CoalescedStreamBuild> {
    let range_start = range.start;
    let range_end_exclusive = range.end.saturating_add(1);
    let inventory_bytes = stream_hash_inventory_bytes(&chunk_hashes, chunk_hashes.capacity())?;
    let mut inventory_reservation = reserve_streaming_read(&engine, inventory_bytes, "range stream inventory admission failed")?;
    let plan_bytes = chunk_hashes
      .len()
      .checked_mul(std::mem::size_of::<PlannedRangeChunk>())
      .and_then(|bytes| chunk_hashes.iter().try_fold(bytes, |total, hash| total.checked_add(hash.len())))
      .and_then(|bytes| u64::try_from(bytes).ok())
      .ok_or_else(|| EngineError::ResourceExhausted("range plan estimate overflow".to_string()))?;
    inventory_reservation.grow(plan_bytes).map_err(|error| streaming_memory_error("range plan admission failed", error))?;
    let mut chunks = Vec::with_capacity(chunk_hashes.len());
    let mut cursor = 0u64;

    for hash in &chunk_hashes {
      if cursor >= range_end_exclusive {
        break;
      }
      let metadata = engine
        .get_chunk_stream_metadata(hash, false)?
        .ok_or_else(|| EngineError::NotFound(format!("Chunk not found: {}", hex::encode(hash))))?;
      let Some(chunk_len) = metadata.raw_value_length else {
        drop(inventory_reservation);
        return Ok(CoalescedStreamBuild::Legacy(chunk_hashes));
      };
      let chunk_start = cursor;
      let chunk_end = chunk_start
        .checked_add(chunk_len)
        .ok_or_else(|| EngineError::InvalidInput("File chunk offsets overflowed while planning byte range".to_string()))?;
      if chunk_end > total_size {
        return Err(EngineError::CorruptEntry {
          offset: metadata.offset,
          reason: format!("Chunk metadata extends past declared file size: {} > {}", chunk_end, total_size),
        });
      }
      cursor = chunk_end;

      if chunk_end <= range_start {
        continue;
      }
      chunks.push(PlannedRangeChunk {
        hash: hash.clone(),
        file_start: chunk_start,
        file_end: chunk_end,
        wal_offset: metadata.offset,
        wal_total_length: metadata.total_length,
      });
    }

    if cursor < range_end_exclusive || chunks.is_empty() {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("Chunk metadata ended at {} before requested byte {}", cursor, range_end_exclusive),
      });
    }
    drop(chunk_hashes);

    let retained_bytes = chunks
      .capacity()
      .checked_mul(std::mem::size_of::<PlannedRangeChunk>())
      .and_then(|bytes| chunks.iter().try_fold(bytes, |total, chunk| total.checked_add(chunk.hash.capacity())))
      .and_then(|bytes| bytes.checked_add(std::mem::size_of::<CoalescedEngineByteRangeStream>()))
      .and_then(|bytes| u64::try_from(bytes).ok())
      .ok_or_else(|| EngineError::ResourceExhausted("range plan retained accounting overflow".to_string()))?;
    if retained_bytes > inventory_reservation.bytes() {
      return Err(EngineError::ResourceExhausted(format!(
        "range plan retained {} bytes exceeds admitted {} bytes",
        retained_bytes,
        inventory_reservation.bytes()
      )));
    }
    if retained_bytes < inventory_reservation.bytes() {
      inventory_reservation
        .shrink(inventory_reservation.bytes() - retained_bytes)
        .map_err(|error| streaming_memory_error("range plan accounting failed", error))?;
    }

    Ok(CoalescedStreamBuild::Ready(Self {
      chunks,
      engine,
      next_index: 0,
      range_start,
      range_end_exclusive,
      limits,
      _inventory_reservation: inventory_reservation,
    }))
  }

  fn selected_len(&self, chunk: &PlannedRangeChunk) -> u64 {
    let start = self.range_start.saturating_sub(chunk.file_start).min(chunk.file_len());
    let end = self.range_end_exclusive.saturating_sub(chunk.file_start).min(chunk.file_len());
    end.saturating_sub(start)
  }

  fn load_next_span(&mut self) -> EngineResult<ReservedReadChunk> {
    if self.next_index >= self.chunks.len() {
      return Err(EngineError::InvalidInput("Range stream is exhausted".to_string()));
    }

    let start_index = self.next_index;
    let first = &self.chunks[start_index];
    let span_start = first.wal_offset;
    let mut span_end = first.wal_end()?;
    let mut end_index = start_index + 1;
    let mut output_len = self.selected_len(first);
    let mut decoded_len = first.file_len();

    while end_index < self.chunks.len() {
      let next = &self.chunks[end_index];
      if next.wal_offset < span_end {
        break;
      }
      let gap = next.wal_offset - span_end;
      let next_end = next.wal_end()?;
      let span_len = next_end
        .checked_sub(span_start)
        .ok_or_else(|| EngineError::InvalidInput("Chunk WAL span underflowed while planning byte range".to_string()))?;
      let next_output_len = self.selected_len(next);
      let combined_output_len = output_len
        .checked_add(next_output_len)
        .ok_or_else(|| EngineError::ResourceExhausted("streaming output length overflow".to_string()))?;
      if gap > RANGE_READ_SPAN_MAX_GAP_BYTES
        || span_len > self.limits.coalesce_max_bytes
        || combined_output_len > self.limits.prefetch_bytes
      {
        break;
      }
      span_end = next_end;
      end_index += 1;
      output_len = combined_output_len;
      decoded_len = decoded_len
        .checked_add(next.file_len())
        .ok_or_else(|| EngineError::ResourceExhausted("decoded span length overflow".to_string()))?;
    }

    let span_chunks = &self.chunks[start_index..end_index];
    let span_len = span_end
      .checked_sub(span_start)
      .ok_or_else(|| EngineError::InvalidInput("Chunk WAL span underflowed while admitting byte range".to_string()))?;
    let locations_bytes = span_chunks
      .len()
      .checked_mul(std::mem::size_of::<ChunkReadLocation>())
      .and_then(|bytes| span_chunks.iter().try_fold(bytes, |total, chunk| total.checked_add(chunk.hash.len())))
      .and_then(|bytes| u64::try_from(bytes).ok())
      .ok_or_else(|| EngineError::ResourceExhausted("chunk location estimate overflow".to_string()))?;
    let decoded_outer_bytes = span_chunks
      .len()
      .checked_mul(std::mem::size_of::<Vec<u8>>())
      .and_then(|bytes| u64::try_from(bytes).ok())
      .ok_or_else(|| EngineError::ResourceExhausted("decoded chunk inventory estimate overflow".to_string()))?;
    let scratch_bytes = span_len
      .checked_add(decoded_len)
      .and_then(|bytes| bytes.checked_add(output_len))
      .and_then(|bytes| bytes.checked_add(locations_bytes))
      .and_then(|bytes| bytes.checked_add(decoded_outer_bytes))
      .and_then(|bytes| bytes.checked_add(std::mem::size_of::<ReservedReadChunk>() as u64))
      .ok_or_else(|| EngineError::ResourceExhausted("coalesced streaming span estimate overflow".to_string()))?;
    let mut reservation = reserve_streaming_read(&self.engine, scratch_bytes, "coalesced read span admission failed")?;
    let locations: Vec<ChunkReadLocation> = span_chunks.iter().map(PlannedRangeChunk::location).collect();
    let values = self.engine.read_chunk_span_verified(&locations)?;
    if values.len() != span_chunks.len() {
      return Err(EngineError::InvalidInput(format!(
        "Chunk span returned {} values for {} planned chunks",
        values.len(),
        span_chunks.len()
      )));
    }

    let output_capacity: usize = output_len
      .try_into()
      .map_err(|_| EngineError::ResourceExhausted(format!("streaming output too large for this platform: {}", output_len)))?;
    let mut output = Vec::with_capacity(output_capacity);
    for (chunk, data) in span_chunks.iter().zip(values.iter()) {
      let expected_len = chunk.file_len();
      if data.len() as u64 != expected_len {
        return Err(EngineError::CorruptEntry {
          offset: chunk.wal_offset,
          reason: format!("Chunk length mismatch: expected {}, decoded {}", expected_len, data.len()),
        });
      }

      let start_in_chunk = self.range_start.saturating_sub(chunk.file_start).min(expected_len) as usize;
      let end_in_chunk = self.range_end_exclusive.saturating_sub(chunk.file_start).min(expected_len) as usize;
      if start_in_chunk < end_in_chunk {
        output.extend_from_slice(&data[start_in_chunk..end_in_chunk]);
      }
    }
    if output.len() as u64 != output_len {
      return Err(EngineError::CorruptEntry {
        offset: span_start,
        reason: format!("Coalesced range produced {} bytes, expected {}", output.len(), output_len),
      });
    }
    drop(values);
    drop(locations);

    let retained_bytes = u64::try_from(output.capacity())
      .ok()
      .and_then(|bytes| bytes.checked_add(std::mem::size_of::<ReservedReadChunk>() as u64))
      .ok_or_else(|| EngineError::ResourceExhausted("coalesced output accounting overflow".to_string()))?;
    if retained_bytes > reservation.bytes() {
      return Err(EngineError::CorruptEntry {
        offset: span_start,
        reason: format!("Coalesced output allocation {} exceeds admitted {} bytes", retained_bytes, reservation.bytes()),
      });
    }
    if retained_bytes < reservation.bytes() {
      reservation
        .shrink(reservation.bytes() - retained_bytes)
        .map_err(|error| streaming_memory_error("coalesced output accounting failed", error))?;
    }
    self.next_index = end_index;

    Ok(ReservedReadChunk::from_admitted(output, reservation))
  }
}

impl Iterator for CoalescedEngineByteRangeStream {
  type Item = EngineResult<ReservedReadChunk>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.next_index >= self.chunks.len() {
      None
    } else {
      Some(self.load_next_span())
    }
  }
}

enum EngineByteRangeStream {
  Coalesced(CoalescedEngineByteRangeStream),
  Legacy(LegacyEngineByteRangeStream),
}

impl EngineByteRangeStream {
  fn new(
    chunk_hashes: Vec<Vec<u8>>,
    engine: std::sync::Arc<StorageEngine>,
    include_deleted: bool,
    range: HttpByteRange,
    total_size: u64,
  ) -> EngineResult<Self> {
    let limits = ReadStreamLimits::resolve(&engine)?;
    if include_deleted {
      Ok(Self::Legacy(LegacyEngineByteRangeStream::new(chunk_hashes, engine, true, range)?))
    } else {
      match CoalescedEngineByteRangeStream::new(chunk_hashes, std::sync::Arc::clone(&engine), range, total_size, limits)? {
        CoalescedStreamBuild::Ready(stream) => Ok(Self::Coalesced(stream)),
        CoalescedStreamBuild::Legacy(chunk_hashes) => {
          Ok(Self::Legacy(LegacyEngineByteRangeStream::new(chunk_hashes, engine, false, range)?))
        }
      }
    }
  }
}

impl Iterator for EngineByteRangeStream {
  type Item = EngineResult<ReservedReadChunk>;

  fn next(&mut self) -> Option<Self::Item> {
    match self {
      EngineByteRangeStream::Coalesced(stream) => stream.next(),
      EngineByteRangeStream::Legacy(stream) => stream.next(),
    }
  }
}

fn engine_stream_body(mut stream: EngineByteRangeStream) -> Body {
  let (sender, receiver) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(1);
  tokio::task::spawn_blocking(move || {
    for result in &mut stream {
      let is_error = result.is_err();
      let result = result.map(axum::body::Bytes::from_owner).map_err(|error| std::io::Error::other(error.to_string()));
      if sender.blocking_send(result).is_err() || is_error {
        break;
      }
    }
  });
  Body::from_stream(ReceiverStream::new(receiver))
}

fn read_raw_entry_reserved(
  engine: &StorageEngine,
  hash: &[u8],
  expected_value_length: u32,
  expected_total_length: u32,
) -> EngineResult<ReservedReadChunk> {
  let admitted_bytes = u64::from(expected_total_length)
    .checked_add(std::mem::size_of::<ReservedReadChunk>() as u64)
    .ok_or_else(|| EngineError::ResourceExhausted("raw entry memory estimate overflow".to_string()))?;
  let mut reservation = reserve_streaming_read(engine, admitted_bytes, "raw entry admission failed")?;
  let entry = engine.get_entry_verified_bounded(hash, expected_value_length).map_err(|error| match error {
    EngineError::InvalidInput(reason) => {
      EngineError::CorruptEntry { offset: 0, reason: format!("Raw entry exceeds its preflight metadata: {reason}") }
    }
    other => other,
  })?;
  let (header, _key, value) = entry.ok_or_else(|| EngineError::NotFound(format!("Entry not found: {}", hex::encode(hash))))?;
  if header.value_length != expected_value_length || header.total_length != expected_total_length {
    return Err(EngineError::CorruptEntry {
      offset: 0,
      reason: format!(
        "Raw entry metadata changed while reading: expected value/total {expected_value_length}/{expected_total_length}, found {}/{}",
        header.value_length, header.total_length
      ),
    });
  }
  let retained_bytes = u64::try_from(value.capacity())
    .ok()
    .and_then(|bytes| bytes.checked_add(std::mem::size_of::<ReservedReadChunk>() as u64))
    .ok_or_else(|| EngineError::ResourceExhausted("raw entry retained memory estimate overflow".to_string()))?;
  if retained_bytes > reservation.bytes() {
    return Err(EngineError::CorruptEntry {
      offset: 0,
      reason: format!("Raw entry allocation {retained_bytes} exceeds admitted {} bytes", reservation.bytes()),
    });
  }
  if retained_bytes < reservation.bytes() {
    reservation
      .shrink(reservation.bytes() - retained_bytes)
      .map_err(|error| streaming_memory_error("raw entry accounting failed", error))?;
  }
  Ok(ReservedReadChunk::from_admitted(value, reservation))
}

async fn build_file_streaming_response(
  engine: &std::sync::Arc<StorageEngine>,
  mut file_record: FileRecord,
  symlink_target: Option<&str>,
  request_headers: &HeaderMap,
  include_deleted: bool,
  extra_headers: &[(&'static str, String)],
) -> Response {
  let range = match parse_range_header(request_headers, file_record.total_size) {
    Ok(range) => range,
    Err(()) => return range_not_satisfiable_response(file_record.total_size),
  };

  let (status, selected_range, served_len, content_range) = if let Some(range) = range {
    (StatusCode::PARTIAL_CONTENT, Some(range), range.len(), Some(format!("bytes {}-{}/{}", range.start, range.end, file_record.total_size)))
  } else if file_record.total_size == 0 {
    (StatusCode::OK, None, 0, None)
  } else {
    (StatusCode::OK, Some(HttpByteRange { start: 0, end: file_record.total_size - 1 }), file_record.total_size, None)
  };

  let body = if let Some(selected_range) = selected_range {
    let stream_engine = std::sync::Arc::clone(engine);
    let chunk_hashes = std::mem::take(&mut file_record.chunk_hashes);
    let total_size = file_record.total_size;
    let stream = match run_engine_blocking("prepare_file_stream", "Failed to prepare file stream", move || {
      EngineByteRangeStream::new(chunk_hashes, stream_engine, include_deleted, selected_range, total_size)
    })
    .await
    {
      Ok(stream) => stream,
      Err(response) => return response,
    };
    engine_stream_body(stream)
  } else {
    Body::empty()
  };

  engine.counters().record_read(served_len);

  let safe_path = file_record.path.replace(['\n', '\r'], "");
  let mut response_builder = axum::http::Response::builder()
    .status(status)
    .header(header::ACCEPT_RANGES, "bytes")
    .header(header::CONTENT_LENGTH, served_len.to_string())
    .header("X-AeorDB-Path", safe_path)
    .header("X-AeorDB-Size", file_record.total_size.to_string())
    .header("X-AeorDB-Created", file_record.created_at.to_string())
    .header("X-AeorDB-Updated", file_record.updated_at.to_string());

  if let Some(value) = content_range {
    response_builder = response_builder.header(header::CONTENT_RANGE, value);
  }

  if let Some(target) = symlink_target {
    response_builder = response_builder.header("X-AeorDB-Link-Target", target.replace(['\n', '\r'], ""));
  }

  if let Some(ref content_type) = file_record.content_type {
    response_builder = response_builder.header(header::CONTENT_TYPE, content_type.as_str());
  }

  for (name, value) in extra_headers {
    response_builder = response_builder.header(*name, value.as_str());
  }

  response_builder.body(body).unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response").into_response())
}

/// Convert a flat directory listing (ChildEntry vec) to JSON values.
///
/// Each entry is enriched with its full path and, for symlink entries,
/// the symlink target is included.
fn build_directory_listing(
  entries: &[crate::engine::ChildEntry],
  base_path: &str,
  directory_ops: &DirectoryOps,
) -> EngineResult<Vec<serde_json::Value>> {
  let normalized = crate::engine::path_utils::normalize_path(base_path);
  let mut listing = Vec::with_capacity(entries.len());
  for child in entries {
    let child_path = if normalized == "/" { format!("/{}", child.name) } else { format!("{}/{}", normalized, child.name) };
    let mut entry_json = serde_json::json!({
      "path": child_path,
      "name": child.name,
      "entry_type": child.entry_type,
      "hash": hex::encode(&child.hash),
      "size": child.total_size,
      "created_at": child.created_at,
      "updated_at": child.updated_at,
      "content_type": child.content_type,
    });

    if child.entry_type == crate::engine::entry_type::EntryType::Symlink.to_u8() {
      let symlink_record = directory_ops.get_symlink(&child_path)?.ok_or_else(|| EngineError::CorruptEntry {
        offset: 0,
        reason: format!("directory names missing symlink record at {child_path}"),
      })?;
      entry_json["target"] = serde_json::json!(symlink_record.target);
    }

    listing.push(entry_json);
  }
  Ok(listing)
}

/// Apply API key rules and system-path filtering to a listing.
///
/// Returns `Err(Response)` if the user identity is invalid; otherwise mutates
/// the listing in place and returns `Ok(())`.
/// Filter a result set down to entries the user can directly Read.
/// Used by recursive listings, query results, and search results when the
/// caller reached the request path via ancestor navigation: a simple
/// allowed-children intersection is insufficient because each child may
/// itself have only partial grants (e.g. a file-pattern share). Per-entry
/// resolver walks correctly honor inheritance and file-pattern matching.
fn filter_results_by_direct_read(
  results: &mut Vec<serde_json::Value>,
  user_id_str: &str,
  engine: &std::sync::Arc<crate::engine::StorageEngine>,
  group_cache: &std::sync::Arc<crate::engine::cache::Cache<crate::engine::cache_loaders::GroupLoader>>,
) -> EngineResult<()> {
  use crate::engine::permission_resolver::{CrudlifyOp, PermissionResolver};

  let user_id = uuid::Uuid::parse_str(user_id_str).map_err(|error| EngineError::InvalidInput(format!("invalid user identity: {error}")))?;
  if crate::engine::is_root(&user_id) {
    return Ok(());
  }
  let resolver = PermissionResolver::new(engine, group_cache);
  let mut decisions = Vec::with_capacity(results.len());
  for entry in results.iter() {
    let Some(path) = entry["path"].as_str() else {
      decisions.push(false);
      continue;
    };
    decisions.push(resolver.check_direct_permission(&user_id, path, CrudlifyOp::Read)?);
  }
  let mut decisions = decisions.into_iter();
  results.retain(|_| decisions.next().unwrap_or(false));
  Ok(())
}

fn enrich_query_items_with_locators(
  engine: &StorageEngine,
  query_results: &[QueryResult],
  items: &mut [serde_json::Value],
  terms: &[LocatorTerm],
  options: &LocatorOptions,
  request_budget: &crate::engine::query_runtime::QueryRequestBudget,
) -> EngineResult<()> {
  for item in items {
    let Some(path) = json_item_path(item).map(str::to_string) else {
      return Err(EngineError::CorruptEntry { offset: 0, reason: "query result JSON is missing its path".to_string() });
    };
    let Some(result) = query_results.iter().find(|result| result.file_record.path == path) else {
      return Err(EngineError::CorruptEntry { offset: 0, reason: format!("query result JSON has no source FileRecord for '{path}'") });
    };
    add_locator_fields(engine, item, &result.file_record, terms, options, request_budget)?;
  }
  Ok(())
}

fn enrich_search_items_with_locators(
  engine: &StorageEngine,
  search_results: &[SearchResult],
  items: &mut [serde_json::Value],
  query: Option<&str>,
  query_node: Option<&QueryNode>,
  options: &LocatorOptions,
  request_budget: &crate::engine::query_runtime::QueryRequestBudget,
) -> EngineResult<()> {
  let structured_terms = query_node.map(terms_from_query_node);
  let ops = DirectoryOps::new(engine);

  for item in items {
    let Some(path) = json_item_path(item).map(str::to_string) else {
      return Err(EngineError::CorruptEntry { offset: 0, reason: "search result JSON is missing its path".to_string() });
    };
    let Some(search_result) = search_results.iter().find(|result| result.path == path) else {
      return Err(EngineError::CorruptEntry { offset: 0, reason: format!("search result JSON has no source result for '{path}'") });
    };
    let terms = match query {
      Some(query_text) => broad_query_terms(query_text, &search_result.matched_fields),
      None => structured_terms.clone().unwrap_or_default(),
    };
    let file_record = ops.get_metadata(&path)?.ok_or_else(|| EngineError::CorruptEntry {
      offset: 0,
      reason: format!("search result '{path}' disappeared before locator generation"),
    })?;
    add_locator_fields(engine, item, &file_record, &terms, options, request_budget)?;
  }
  Ok(())
}

fn add_locator_fields(
  engine: &StorageEngine,
  item: &mut serde_json::Value,
  file_record: &FileRecord,
  terms: &[LocatorTerm],
  options: &LocatorOptions,
  request_budget: &crate::engine::query_runtime::QueryRequestBudget,
) -> EngineResult<()> {
  let Some(object) = item.as_object_mut() else {
    return Err(EngineError::CorruptEntry { offset: 0, reason: "locator enrichment target is not a JSON object".to_string() });
  };

  if !file_record.content_hash.is_empty() {
    object.insert("content_hash".to_string(), serde_json::json!(file_record.content_hash_hex()));
  }

  let generation = try_generate_locators_with_budget(engine, file_record, terms, options, request_budget)?;
  let matches = serde_json::to_value(&generation.matches)
    .map_err(|error| EngineError::CorruptEntry { offset: 0, reason: format!("locator response serialization failed: {error}") })?;
  object.insert("matches".to_string(), matches);
  object.insert("matches_truncated".to_string(), serde_json::json!(generation.matches_truncated));
  object.insert("locator_status".to_string(), serde_json::json!(generation.locator_status));
  Ok(())
}

fn required_dispatch_value<T>(value: Option<T>, entry_type: &str, hash: &str) -> Result<T, Response> {
  match value {
    Some(value) => Ok(value),
    None => {
      tracing::error!(entry_type, hash, "entry hash dispatch state is incomplete");
      Err(ErrorResponse::new("Failed to retrieve entry").with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response())
    }
  }
}

fn serialize_response_value<T: serde::Serialize>(value: &T, context: &str) -> Result<serde_json::Value, Response> {
  serde_json::to_value(value).map_err(|error| {
    tracing::error!(context, %error, "HTTP response serialization failed");
    ErrorResponse::new(format!("{context} serialization failed")).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response()
  })
}

fn engine_file_response_with_hash(file_record: &FileRecord, algorithm: crate::engine::HashAlgorithm) -> EngineResult<EngineFileResponse> {
  let file_value = file_record.serialize(algorithm.hash_length())?;
  let content_hash = file_content_hash(&file_value, &algorithm)?;
  let mut response = EngineFileResponse::from(file_record);
  response.hash = Some(hex::encode(content_hash));
  Ok(response)
}

fn json_item_path(item: &serde_json::Value) -> Option<&str> {
  item.get("path").and_then(|value| value.as_str())
}

fn apply_listing_filters(
  engine: &StorageEngine,
  listing: &mut Vec<serde_json::Value>,
  key_rules: Option<&[crate::engine::api_key_rules::KeyRule]>,
  _user_id_str: &str,
  filtered_listing: Option<&crate::auth::permission_middleware::FilteredListing>,
) -> Result<(), Response> {
  if let Some(rules) = key_rules {
    if !rules.is_empty() {
      filter_listing_by_key_rules(listing, rules, 'l');
    }
  }

  filter_generic_data_items(engine, listing).map_err(|error| engine_error_response("Failed to classify listing paths", &error))?;

  // Ancestor-navigation filter: when the user reached this directory by
  // virtue of having a grant somewhere below, only show the children that
  // either ARE the grant target or are next-segment ancestors of one.
  if let Some(filter) = filtered_listing {
    listing.retain(|entry| {
      let name = entry["name"].as_str().unwrap_or("");
      filter.allowed_children.contains(name)
    });
  }

  Ok(())
}

fn filter_generic_data_items(engine: &StorageEngine, items: &mut Vec<serde_json::Value>) -> EngineResult<()> {
  let resolver = SystemFamilyPolicyResolver::new(engine.hash_algo())?;
  let mut retained = Vec::with_capacity(items.len());

  for item in items.drain(..) {
    let Some(path) = item.get("path").and_then(|value| value.as_str()) else {
      retained.push(item);
      continue;
    };
    match resolver.generic_data_path_selection(path)? {
      GenericDataPathSelection::Include => retained.push(item),
      GenericDataPathSelection::Conceal | GenericDataPathSelection::StructuralContainer => {}
    }
  }

  *items = retained;
  Ok(())
}

/// Compute effective_permissions for each listing item using the permission
/// resolver. Only runs for non-root users when items don't already have
/// effective_permissions (i.e., regular user/group shares, not scoped API keys).
fn attach_effective_permissions(
  listing: &mut [serde_json::Value],
  user_id: &Uuid,
  engine: &std::sync::Arc<crate::engine::StorageEngine>,
  group_cache: &std::sync::Arc<crate::engine::cache::Cache<crate::engine::cache_loaders::GroupLoader>>,
) -> EngineResult<()> {
  use crate::engine::permission_resolver::{CrudlifyOp, PermissionResolver};

  if crate::engine::is_root(user_id) {
    return Ok(());
  }

  let resolver = PermissionResolver::new(engine, group_cache);
  let ops = [
    ('c', CrudlifyOp::Create),
    ('r', CrudlifyOp::Read),
    ('u', CrudlifyOp::Update),
    ('d', CrudlifyOp::Delete),
    ('l', CrudlifyOp::List),
    ('i', CrudlifyOp::Invoke),
    ('f', CrudlifyOp::Deploy),
    ('y', CrudlifyOp::Configure),
  ];

  for entry in listing.iter_mut() {
    // Skip items that already have effective_permissions (set by key rules filter)
    if entry.get("effective_permissions").is_some() {
      continue;
    }

    let raw_path = match entry["path"].as_str() {
      Some(p) => p.to_string(),
      None => continue,
    };
    // Directories need a trailing slash so path_levels walks INTO them and
    // reads their .aeordb-permissions — otherwise a directory's own grants
    // are silently ignored when it appears as a listing entry.
    let is_directory =
      entry["entry_type"].as_u64().map(|t| t == crate::engine::entry_type::EntryType::DirectoryIndex.to_u8() as u64).unwrap_or(false);
    let path = if is_directory && !raw_path.ends_with('/') { format!("{}/", raw_path) } else { raw_path };

    let mut flags = ['-'; 8];
    for (i, (ch, op)) in ops.iter().enumerate() {
      if resolver.check_permission(user_id, &path, *op)? {
        flags[i] = *ch;
      }
    }
    let perm_str: String = flags.iter().collect();
    if let Some(obj) = entry.as_object_mut() {
      obj.insert("effective_permissions".to_string(), serde_json::Value::String(perm_str));
    }
  }
  Ok(())
}

/// Handle a symlink path: resolve and produce the appropriate file or
/// directory response, or return an error for dangling / cyclic symlinks.
async fn handle_symlink_resolution(
  engine: &std::sync::Arc<crate::engine::StorageEngine>,
  path: &str,
  symlink_target: &str,
  request_headers: &HeaderMap,
  user_id_str: &str,
  key_rules: Option<&[crate::engine::api_key_rules::KeyRule]>,
  filtered_listing: Option<&crate::auth::permission_middleware::FilteredListing>,
  limit: Option<usize>,
  offset: Option<usize>,
) -> Response {
  let directory_ops = DirectoryOps::new(engine);

  match resolve_symlink(engine, path) {
    Ok(ResolvedTarget::File(file_record)) => {
      if let Err(response) = require_generic_data_engine_path(engine, &file_record.path) {
        return response;
      }

      // Check if the resolved target path is allowed by API key rules
      if let Some(rules) = key_rules {
        if !rules.is_empty() {
          let target_path = &file_record.path;
          let normalized_target = if target_path.starts_with('/') { target_path.to_string() } else { format!("/{}", target_path) };
          match match_rules(rules, &normalized_target) {
            Some(rule) => {
              if !check_operation_permitted(&rule.permitted, 'r') {
                return ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response();
              }
            }
            None => {
              return ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response();
            }
          }
        }
      }

      build_file_streaming_response(engine, file_record, Some(symlink_target), request_headers, false, &[]).await
    }
    Ok(ResolvedTarget::Directory(dir_path)) => {
      if let Err(response) = require_generic_data_engine_path(engine, &dir_path) {
        return response;
      }

      match directory_ops.list_directory_strict(&dir_path) {
        Ok(entries) => {
          let mut listing = match build_directory_listing(&entries, &dir_path, &directory_ops) {
            Ok(listing) => listing,
            Err(error) => return engine_error_response("Failed to build resolved directory listing", &error),
          };
          match apply_listing_filters(engine, &mut listing, key_rules, user_id_str, filtered_listing) {
            Ok(()) => paginated_listing_response(listing, limit, offset, None, None),
            Err(response) => response,
          }
        }
        Err(error) => {
          tracing::error!("Engine: failed to list resolved directory: {}", error);
          ErrorResponse::new(format!(
            "Failed to list directory after resolving symlink '{}'. If this persists, check GET /system/health for system status",
            path
          ))
          .with_status(StatusCode::INTERNAL_SERVER_ERROR)
          .into_response()
        }
      }
    }
    Err(EngineError::NotFound(msg)) => {
      ErrorResponse::new(format!("Dangling symlink: {}", msg)).with_status(StatusCode::NOT_FOUND).into_response()
    }
    Err(EngineError::CyclicSymlink(msg)) => {
      ErrorResponse::new(format!("Symlink cycle detected: {}", msg)).with_status(StatusCode::BAD_REQUEST).into_response()
    }
    Err(EngineError::SymlinkDepthExceeded(msg)) => ErrorResponse::new(msg).with_status(StatusCode::BAD_REQUEST).into_response(),
    Err(error) => {
      tracing::error!("Engine: failed to resolve symlink '{}': {}", path, error);
      ErrorResponse::new(format!(
        "Failed to resolve symlink '{}'. The symlink or its target may be corrupted — contact your administrator",
        path
      ))
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response()
    }
  }
}

/// Handle a direct file read: stream the file content as an HTTP response.
async fn handle_file_response(
  engine: &std::sync::Arc<crate::engine::StorageEngine>,
  file_record: FileRecord,
  request_headers: &HeaderMap,
) -> Response {
  build_file_streaming_response(engine, file_record, None, request_headers, false, &[]).await
}

/// Handle recursive directory listing with depth and/or glob parameters.
fn handle_recursive_listing(
  engine: &std::sync::Arc<crate::engine::StorageEngine>,
  path: &str,
  version_query: &EngineGetQuery,
  key_rules: Option<&[crate::engine::api_key_rules::KeyRule]>,
  user_id_str: &str,
  filtered_listing: Option<&crate::auth::permission_middleware::FilteredListing>,
  state: Option<&AppState>,
) -> Response {
  let directory_ops = DirectoryOps::new(engine);

  let depth = version_query.depth.unwrap_or(0);
  // M17: Clamp recursive listing depth to prevent runaway traversals.
  let depth = if depth < 0 { -1 } else { depth.min(256) };
  let glob = version_query.glob.as_deref();

  match list_directory_recursive_strict(engine, path, depth, glob, None) {
    Ok(entries) => {
      let mut listing = Vec::with_capacity(entries.len());
      for entry in &entries {
        let mut entry_json = serde_json::json!({
          "path": entry.path,
          "name": entry.name,
          "entry_type": entry.entry_type,
          "hash": hex::encode(&entry.hash),
          "size": entry.total_size,
          "created_at": entry.created_at,
          "updated_at": entry.updated_at,
          "content_type": entry.content_type,
        });

        if entry.entry_type == crate::engine::entry_type::EntryType::Symlink.to_u8() {
          let symlink_record = match directory_ops.get_symlink(&entry.path) {
            Ok(Some(record)) => record,
            Ok(None) => {
              return engine_error_response(
                "Failed to build recursive directory listing",
                &EngineError::CorruptEntry { offset: 0, reason: format!("directory names missing symlink record at {}", entry.path) },
              )
            }
            Err(error) => return engine_error_response("Failed to build recursive directory listing", &error),
          };
          entry_json["target"] = serde_json::json!(symlink_record.target);
        }

        listing.push(entry_json);
      }

      match apply_listing_filters(engine, &mut listing, key_rules, user_id_str, None) {
        Ok(()) => {
          if filtered_listing.is_some() {
            if let Some(st) = state {
              if let Err(error) = filter_results_by_direct_read(&mut listing, user_id_str, &st.engine, &st.group_cache) {
                return engine_error_response("Failed to filter recursive listing permissions", &error);
              }
            }
          }
          paginated_listing_response(
            listing,
            version_query.limit,
            version_query.offset,
            version_query.sort.as_deref(),
            version_query.order.as_deref(),
          )
        }
        Err(response) => response,
      }
    }
    Err(EngineError::NotFound(_)) => ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response(),
    Err(error) => {
      tracing::error!("Engine: failed to list directory '{}': {}", path, error);
      ErrorResponse::new(format!(
        "Failed to list directory '{}' with recursive traversal. If this persists, check GET /system/health for system status",
        path
      ))
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response()
    }
  }
}

/// Pagination + sort options for a directory listing. Bundled to keep the
/// downstream signatures short — these always travel together.
struct ListingPagination<'a> {
  limit: Option<usize>,
  offset: Option<usize>,
  sort: Option<&'a str>,
  order: Option<&'a str>,
}

/// Handle default (flat) directory listing without depth/glob parameters.
fn handle_directory_listing(
  engine: &std::sync::Arc<crate::engine::StorageEngine>,
  path: &str,
  key_rules: Option<&[crate::engine::api_key_rules::KeyRule]>,
  user_id_str: &str,
  pagination: ListingPagination<'_>,
  state: Option<&AppState>,
  filtered_listing: Option<&crate::auth::permission_middleware::FilteredListing>,
) -> Response {
  let ListingPagination { limit, offset, sort, order } = pagination;
  let directory_ops = DirectoryOps::new(engine);

  match directory_ops.list_directory_strict(path) {
    Ok(entries) => {
      let mut listing = match build_directory_listing(&entries, path, &directory_ops) {
        Ok(listing) => listing,
        Err(error) => return engine_error_response("Failed to build directory listing", &error),
      };
      match apply_listing_filters(engine, &mut listing, key_rules, user_id_str, filtered_listing) {
        Ok(()) => {
          // Attach effective_permissions for non-root users
          if let Some(st) = state {
            if let Ok(uid) = uuid::Uuid::parse_str(user_id_str) {
              if let Err(error) = attach_effective_permissions(&mut listing, &uid, &st.engine, &st.group_cache) {
                return engine_error_response("Failed to resolve listing permissions", &error);
              }
            } else if !user_id_str.starts_with("share:") {
              return ErrorResponse::new("Invalid user identity").with_status(StatusCode::FORBIDDEN).into_response();
            }
          }
          paginated_listing_response(listing, limit, offset, sort, order)
        }
        Err(response) => response,
      }
    }
    Err(EngineError::NotFound(_)) => ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response(),
    Err(error) => {
      tracing::error!("Engine: failed to list directory '{}': {}", path, error);
      ErrorResponse::new(format!("Failed to list directory '{}'. If this persists, check GET /system/health for system status", path))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

// ---------------------------------------------------------------------------
// engine_get: dispatcher
// ---------------------------------------------------------------------------

/// GET /engine/*path -- read a file (streaming) or list a directory.
/// GET /files or /files/ — root directory listing (no wildcard path param).
pub async fn engine_get_root(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  active_key_rules: Option<Extension<ActiveKeyRules>>,
  filtered_listing: Option<Extension<crate::auth::permission_middleware::FilteredListing>>,
  headers: HeaderMap,
  AxumQuery(version_query): AxumQuery<EngineGetQuery>,
) -> Response {
  engine_get(State(state), Extension(claims), active_key_rules, filtered_listing, headers, Path("/".to_string()), AxumQuery(version_query))
    .await
}

pub async fn engine_get(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  active_key_rules: Option<Extension<ActiveKeyRules>>,
  filtered_listing: Option<Extension<crate::auth::permission_middleware::FilteredListing>>,
  headers: HeaderMap,
  Path(path): Path<String>,
  AxumQuery(version_query): AxumQuery<EngineGetQuery>,
) -> Response {
  if let Err(response) = require_generic_data_path(&state, &path) {
    return response;
  }

  // Deleted files are invisible to users without 'd' permission
  match is_deleted_and_forbidden(&state, &claims, &path) {
    Ok(true) => return ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response(),
    Ok(false) => {}
    Err(error) => return engine_error_response("Failed to check deleted-file access", &error),
  }

  // If snapshot or version query param is present, read from historical version
  if version_query.snapshot.is_some() || version_query.version.is_some() {
    return engine_get_at_version(&state, &path, &version_query, &headers).await;
  }

  // Extract key rules slice for helpers (avoids passing axum Extension around)
  let key_rules: Option<&[crate::engine::api_key_rules::KeyRule]> = active_key_rules.as_ref().map(|Extension(rules)| rules.0.as_slice());
  let filter_ref: Option<&crate::auth::permission_middleware::FilteredListing> = filtered_listing.as_ref().map(|Extension(f)| f);

  let directory_ops = DirectoryOps::new(&state.engine);

  // Check for symlink first
  let symlink_record = match directory_ops.get_symlink(&path) {
    Ok(record) => record,
    Err(error) => return engine_error_response("Failed to inspect symlink", &error),
  };
  if let Some(symlink_record) = symlink_record {
    // nofollow: return symlink metadata without resolving
    if version_query.nofollow == Some(true) {
      return (
        StatusCode::OK,
        Json(serde_json::json!({
          "path": symlink_record.path,
          "target": symlink_record.target,
          "entry_type": 8,
          "created_at": symlink_record.created_at,
          "updated_at": symlink_record.updated_at,
        })),
      )
        .into_response();
    }

    return handle_symlink_resolution(
      &state.engine,
      &path,
      &symlink_record.target,
      &headers,
      &claims.sub,
      key_rules,
      filter_ref,
      version_query.limit,
      version_query.offset,
    )
    .await;
  }

  // Try as file first
  match directory_ops.get_metadata(&path) {
    Ok(Some(file_record)) => {
      return handle_file_response(&state.engine, file_record, &headers).await;
    }
    Ok(None) => {
      // Not a file -- fall through to directory listing
    }
    Err(error) => {
      tracing::error!("Engine: failed to get metadata for '{}': {}", path, error);
      return ErrorResponse::new(format!("Failed to read path '{}'. If this persists, check GET /system/health for system status", path))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  }

  // Try as directory -- recursive listing if depth/glob specified
  if version_query.depth.is_some() || version_query.glob.is_some() {
    return handle_recursive_listing(&state.engine, &path, &version_query, key_rules, &claims.sub, filter_ref, Some(&state));
  }

  // Default flat directory listing
  handle_directory_listing(
    &state.engine,
    &path,
    key_rules,
    &claims.sub,
    ListingPagination {
      limit: version_query.limit,
      offset: version_query.offset,
      sort: version_query.sort.as_deref(),
      order: version_query.order.as_deref(),
    },
    Some(&state),
    filter_ref,
  )
}

/// Read a file at a historical version (snapshot or explicit root hash).
async fn engine_get_at_version(state: &AppState, path: &str, version_query: &EngineGetQuery, request_headers: &HeaderMap) -> Response {
  let vm = VersionManager::new(&state.engine);

  // Resolve root hash: snapshot takes precedence
  let root_hash = if let Some(ref snapshot_name) = version_query.snapshot {
    match vm.resolve_root_hash(Some(snapshot_name)) {
      Ok(hash) => hash,
      Err(_) => {
        return ErrorResponse::new(format!("Snapshot '{}' not found", snapshot_name)).with_status(StatusCode::NOT_FOUND).into_response();
      }
    }
  } else if let Some(ref version_hex) = version_query.version {
    match hex::decode(version_hex) {
      Ok(hash) => hash,
      Err(_) => {
        return ErrorResponse::new("Invalid version hash: value is not valid hex. Use the root_hash from a snapshot or version response")
          .with_status(StatusCode::BAD_REQUEST)
          .into_response();
      }
    }
  } else {
    return ErrorResponse::new(
      "No snapshot or version specified. Use ?snapshot=<name> or ?version=<hex_hash> to read a historical version",
    )
    .with_status(StatusCode::BAD_REQUEST)
    .into_response();
  };

  // Resolve the file at this version
  let (_file_hash, file_record) = match crate::engine::version_access::resolve_file_at_version(&state.engine, &root_hash, path) {
    Ok(result) => result,
    Err(crate::engine::errors::EngineError::NotFound(msg)) => {
      return ErrorResponse::new(msg).with_status(StatusCode::NOT_FOUND).into_response();
    }
    Err(error) => {
      tracing::error!("Engine: failed to read file '{}' at version: {}", path, error);
      return ErrorResponse::new(format!(
        "Failed to read file '{}' at historical version. If this persists, check GET /system/health for system status",
        path
      ))
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response();
    }
  };

  build_file_streaming_response(&state.engine, file_record, None, request_headers, true, &[]).await
}

/// DELETE /engine/*path -- delete a file via the custom storage engine.
pub async fn engine_delete_file(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(path): Path<String>,
) -> Response {
  if let Err(response) = require_generic_data_path(&state, &path) {
    return response;
  }

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let engine = state.engine.clone();
  let path_for_blocking = path.clone();

  // Dispatch + delete all happen on a blocking thread. The kind ("symlink" /
  // "file" / "directory") flows back to the response.
  let result = tokio::task::spawn_blocking(move || -> EngineResult<&'static str> {
    let ops = DirectoryOps::new(&engine);
    if ops.get_symlink(&path_for_blocking)?.is_some() {
      ops.delete_symlink(&ctx, &path_for_blocking)?;
      return Ok("symlink");
    }
    match ops.delete_file(&ctx, &path_for_blocking) {
      Ok(()) => Ok("file"),
      Err(EngineError::NotFound(_)) => {
        ops.delete_directory(&ctx, &path_for_blocking)?;
        Ok("directory")
      }
      Err(other) => Err(other),
    }
  })
  .await;

  match result {
    Ok(Ok(kind)) => {
      let mut body = serde_json::json!({ "deleted": true, "path": path });
      if kind != "file" {
        body["entry_type"] = serde_json::json!(kind);
      }
      (StatusCode::OK, Json(body)).into_response()
    }
    Ok(Err(EngineError::NotFound(_))) => {
      ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response()
    }
    Ok(Err(error)) => {
      tracing::error!("Engine: failed to delete '{}': {}", path, error);
      engine_error_response("Failed to delete", &error)
    }
    Err(join_error) => {
      tracing::error!("delete task panicked: {}", join_error);
      ErrorResponse::new("Failed to delete: internal task error").with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response()
    }
  }
}

/// HEAD /engine/*path -- return metadata as headers.
/// Restore a deleted file.
/// POST /files/restore { "path": "/some/file.txt" }
pub async fn restore_deleted_file(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Json(body): Json<serde_json::Value>,
) -> Response {
  let path = match body.get("path").and_then(|v| v.as_str()) {
    Some(p) => p.to_string(),
    None => {
      return ErrorResponse::new("Missing 'path' field").with_status(StatusCode::BAD_REQUEST).into_response();
    }
  };

  if let Err(response) = require_generic_data_path(&state, &path) {
    return response;
  }

  // User/group permission check: /files/restore is exempt from path-aware
  // middleware. Restoring a file is an inverse Delete operation — require
  // the 'd' (Delete) permission on the path, matching list_deleted_files.
  if let Err(response) = reject_share_key(&claims, "Share keys cannot restore deleted files") {
    return response;
  };
  let permissions = match RoutePermissionChecker::from_claims(&state, &claims, "Invalid user identity") {
    Ok(permissions) => permissions,
    Err(response) => return response,
  };
  if !permissions.is_root() {
    let permitted = match permissions.has_path_permission(&path, CrudlifyOp::Delete) {
      Ok(permitted) => permitted,
      Err(error) => return engine_error_response("Failed to check restore permission", &error),
    };
    if !permitted {
      return ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response();
    }
  }

  let ctx = crate::engine::RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let ops = DirectoryOps::new(&state.engine);

  match ops.restore_deleted_file(&ctx, &path) {
    Ok(()) => (
      StatusCode::OK,
      Json(serde_json::json!({
        "restored": true,
        "path": path,
      })),
    )
      .into_response(),
    Err(e) => ErrorResponse::new(format!("Restore failed: {}", e)).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response(),
  }
}

/// List deleted files in a directory.
/// GET /files/deleted?path=/some/dir/
pub async fn list_deleted_files(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  AxumQuery(params): AxumQuery<std::collections::HashMap<String, String>>,
) -> Response {
  let dir_path = params.get("path").map(|s| s.as_str()).unwrap_or("/");

  if let Err(response) = require_generic_data_path(&state, dir_path) {
    return response;
  }

  // Deleted files require 'd' permission — check on the directory
  let permissions = match RoutePermissionChecker::from_claims(&state, &claims, "Invalid user ID") {
    Ok(permissions) => permissions,
    Err(response) => return response,
  };
  if !permissions.is_root() {
    let permitted = match permissions.has_permission(dir_path, CrudlifyOp::Delete) {
      Ok(permitted) => permitted,
      Err(error) => return engine_error_response("Failed to check deleted-file listing permission", &error),
    };
    if !permitted {
      return (
        StatusCode::OK,
        Json(serde_json::json!({
          "items": [],
          "total": 0,
        })),
      )
        .into_response();
    }
  }

  let ops = DirectoryOps::new(&state.engine);

  match ops.list_deleted(dir_path) {
    Ok(records) => {
      let items: Vec<serde_json::Value> = records
        .iter()
        .map(|r| {
          let name = crate::engine::path_utils::file_name(&r.path).unwrap_or("").to_string();
          serde_json::json!({
            "path": r.path,
            "name": name,
            "deleted_at": r.deleted_at,
            "reason": r.reason,
          })
        })
        .collect();
      (
        StatusCode::OK,
        Json(serde_json::json!({
          "items": items,
          "total": items.len(),
        })),
      )
        .into_response()
    }
    Err(e) => {
      ErrorResponse::new(format!("Failed to list deleted files: {}", e)).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response()
    }
  }
}

pub async fn engine_head(State(state): State<AppState>, Extension(claims): Extension<TokenClaims>, Path(path): Path<String>) -> Response {
  if let Err(response) = require_generic_data_path(&state, &path) {
    return response;
  }

  // Deleted files are invisible to users without 'd' permission
  match is_deleted_and_forbidden(&state, &claims, &path) {
    Ok(true) => return ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response(),
    Ok(false) => {}
    Err(error) => return engine_error_response("Failed to check deleted-file access", &error),
  }

  let directory_ops = DirectoryOps::new(&state.engine);

  // Check symlink first
  let symlink_record = match directory_ops.get_symlink(&path) {
    Ok(record) => record,
    Err(error) => return engine_error_response("Failed to inspect symlink", &error),
  };
  if let Some(symlink_record) = symlink_record {
    return axum::http::Response::builder()
      .status(StatusCode::OK)
      .header("X-AeorDB-Type", "symlink")
      .header("X-AeorDB-Link-Target", symlink_record.target.replace(['\n', '\r'], ""))
      .header("X-AeorDB-Path", path.replace(['\n', '\r'], ""))
      .header("X-AeorDB-Created", symlink_record.created_at.to_string())
      .header("X-AeorDB-Updated", symlink_record.updated_at.to_string())
      .body(Body::empty())
      .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
  }

  match directory_ops.get_metadata(&path) {
    Ok(Some(file_record)) => {
      let safe_path = file_record.path.replace(['\n', '\r'], "");
      let mut response_builder = axum::http::Response::builder()
        .status(StatusCode::OK)
        .header("X-AeorDB-Type", "file")
        .header("X-AeorDB-Path", safe_path)
        .header("X-AeorDB-Size", file_record.total_size.to_string())
        .header("X-AeorDB-Created", file_record.created_at.to_string())
        .header("X-AeorDB-Updated", file_record.updated_at.to_string());

      if let Some(ref content_type) = file_record.content_type {
        response_builder = response_builder.header("content-type", content_type.as_str());
      }

      response_builder.body(Body::empty()).unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }
    Ok(None) => {
      // Check if it is a directory
      match directory_ops.list_directory_strict(&path) {
        Ok(_) => {
          let safe_path = path.replace(['\n', '\r'], "");
          axum::http::Response::builder()
            .status(StatusCode::OK)
            .header("X-AeorDB-Type", "directory")
            .header("X-AeorDB-Path", safe_path)
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(EngineError::NotFound(_)) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => engine_error_response("Failed to inspect directory", &error),
      }
    }
    Err(error) => {
      tracing::error!("Engine: failed to get metadata for '{}': {}", path, error);
      StatusCode::INTERNAL_SERVER_ERROR.into_response()
    }
  }
}

// ---------------------------------------------------------------------------
// Hash-based retrieval
// ---------------------------------------------------------------------------

/// GET /engine/_hash/{hex_hash} -- retrieve an entry by its content-addressed hash.
///
/// For FileRecords: streams the reconstructed file content (same as GET /engine/{path}).
/// For Chunks: returns raw decompressed chunk data.
/// For DirectoryIndex: returns the raw directory data.
/// Other types: returns raw bytes.
///
/// Scoped-key enforcement: a key with rules (ActiveKeyRules extension) can
/// only fetch FileRecords whose path is permitted with 'r' by the rules.
/// Other entry types (raw chunks, directory indexes) are denied for scoped
/// keys because there's no path to check — a chunk hash can be shared by
/// many files. Root and unscoped keys retain full access.
pub async fn engine_get_by_hash(
  State(state): State<AppState>,
  Extension(_claims): Extension<TokenClaims>,
  active_key_rules: Option<Extension<crate::auth::permission_middleware::ActiveKeyRules>>,
  headers: HeaderMap,
  Path(hex_hash): Path<String>,
) -> Response {
  let hash_bytes = match hex::decode(&hex_hash) {
    Ok(bytes) => bytes,
    Err(_) => {
      return ErrorResponse::new(format!("Invalid hex hash '{}': must be a valid hexadecimal string", hex_hash))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  };

  let header = {
    let engine = std::sync::Arc::clone(&state.engine);
    let hash = hash_bytes.clone();
    match run_engine_blocking("get_entry_header_by_hash", "Failed to retrieve entry header", move || {
      engine.get_entry_header(&hash)?.ok_or_else(|| EngineError::NotFound(format!("Entry not found: {}", hex::encode(hash))))
    })
    .await
    {
      Ok(header) => header,
      Err(response) => return response,
    }
  };

  // Block ALL access to system-flagged entries via API — system data is only
  // accessible through the internal system_store module, never through HTTP.
  if header.is_system_entry() {
    return ErrorResponse::new(format!("Entry not found: {}", hex_hash)).with_status(StatusCode::NOT_FOUND).into_response();
  }

  let mut file_record = if header.entry_type == EntryType::FileRecord {
    let engine = std::sync::Arc::clone(&state.engine);
    let hash = hash_bytes.clone();
    let hash_length = state.engine.hash_algo().hash_length();
    let entry_version = header.entry_version;
    match run_engine_blocking("get_file_record_by_hash", "Failed to retrieve file record", move || {
      let (_, _, value) =
        engine.get_entry_verified(&hash)?.ok_or_else(|| EngineError::NotFound(format!("Entry not found: {}", hex::encode(&hash))))?;
      FileRecord::deserialize(&value, hash_length, entry_version)
    })
    .await
    {
      Ok(record) => Some(record),
      Err(response) => return response,
    }
  } else {
    None
  };

  // Scoped-key check. ActiveKeyRules is only inserted by the permission
  // middleware when the key is scoped (rules non-empty). Root keys and
  // unscoped keys skip this entirely.
  if let Some(Extension(rules)) = active_key_rules.as_ref() {
    use crate::engine::api_key_rules::{match_rules, check_operation_permitted};
    match header.entry_type {
      EntryType::FileRecord => {
        let path = match file_record.as_ref() {
          Some(record) => &record.path,
          None => return ErrorResponse::new(format!("Entry not found: {}", hex_hash)).with_status(StatusCode::NOT_FOUND).into_response(),
        };
        let allowed = match match_rules(&rules.0, path) {
          Some(rule) => check_operation_permitted(&rule.permitted, 'r'),
          None => false,
        };
        if !allowed {
          // Use 404 (not 403) so scoped keys cannot enumerate forbidden
          // paths by probing hashes.
          return ErrorResponse::new(format!("Entry not found: {}", hex_hash)).with_status(StatusCode::NOT_FOUND).into_response();
        }
      }
      // For raw chunks and other non-path entries, we can't tie the hash
      // back to a path the scoped key is permitted to access. Deny.
      _ => {
        return ErrorResponse::new(format!("Entry not found: {}", hex_hash)).with_status(StatusCode::NOT_FOUND).into_response();
      }
    }
  }

  let raw_value = if matches!(header.entry_type, EntryType::FileRecord | EntryType::Chunk) {
    None
  } else {
    let engine = std::sync::Arc::clone(&state.engine);
    let hash = hash_bytes.clone();
    let value_length = header.value_length;
    let total_length = header.total_length;
    match run_engine_blocking("get_raw_entry_by_hash", "Failed to retrieve entry", move || {
      read_raw_entry_reserved(&engine, &hash, value_length, total_length)
    })
    .await
    {
      Ok(value) => Some(value),
      Err(response) => return response,
    }
  };

  match header.entry_type {
    EntryType::FileRecord => {
      let file_record = match required_dispatch_value(file_record.take(), "FileRecord", &hex_hash) {
        Ok(file_record) => file_record,
        Err(response) => return response,
      };

      build_file_streaming_response(
        &state.engine,
        file_record,
        None,
        &headers,
        false,
        &[("X-AeorDB-Type", header.entry_type.to_u8().to_string()), ("X-AeorDB-Hash", hex_hash.clone())],
      )
      .await
    }

    EntryType::Chunk => {
      let data = {
        let engine = std::sync::Arc::clone(&state.engine);
        let hash = hash_bytes.clone();
        match run_engine_blocking("get_chunk_by_hash", "Failed to retrieve chunk", move || read_chunk_reserved(&engine, &hash, false)).await
        {
          Ok(data) => data,
          Err(response) => return response,
        }
      };
      state.engine.counters().record_read(data.len() as u64);

      axum::http::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .header("X-AeorDB-Type", header.entry_type.to_u8().to_string())
        .header("X-AeorDB-Hash", &hex_hash)
        .body(Body::from(axum::body::Bytes::from_owner(data)))
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response").into_response())
    }

    EntryType::DirectoryIndex => {
      let value = match required_dispatch_value(raw_value, "DirectoryIndex", &hex_hash) {
        Ok(value) => value,
        Err(response) => return response,
      };
      state.engine.counters().record_read(value.len() as u64);
      axum::http::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .header("X-AeorDB-Type", header.entry_type.to_u8().to_string())
        .header("X-AeorDB-Hash", &hex_hash)
        .body(Body::from(axum::body::Bytes::from_owner(value)))
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response").into_response())
    }

    _ => {
      // Other types: return raw value bytes.
      let value = match required_dispatch_value(raw_value, "raw", &hex_hash) {
        Ok(value) => value,
        Err(response) => return response,
      };
      axum::http::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .header("X-AeorDB-Type", header.entry_type.to_u8().to_string())
        .header("X-AeorDB-Hash", &hex_hash)
        .body(Body::from(axum::body::Bytes::from_owner(value)))
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response").into_response())
    }
  }
}

// Snapshot + fork handlers moved to `server::version_routes`.

// ---------------------------------------------------------------------------
// Query endpoint
// ---------------------------------------------------------------------------

/// Raw query request — accepts `where` as either an array (legacy) or
/// an object (boolean logic). Deserialized as raw JSON so we can detect
/// the format at runtime.
#[derive(Debug, Deserialize)]
pub struct QueryRequest {
  pub path: String,
  pub r#where: serde_json::Value,
  pub limit: Option<usize>,
  pub offset: Option<usize>,
  pub order_by: Option<Vec<SortFieldRequest>>,
  pub after: Option<String>,
  pub before: Option<String>,
  pub include_total: Option<bool>,
  pub aggregate: Option<AggregateRequestData>,
  pub select: Option<Vec<String>>,
  pub explain: Option<serde_json::Value>,
  #[serde(flatten)]
  pub locators: LocatorOptionsRequest,
}

#[derive(Debug, Deserialize)]
pub struct AggregateRequestData {
  #[serde(default)]
  pub count: bool,
  #[serde(default)]
  pub sum: Vec<String>,
  #[serde(default)]
  pub avg: Vec<String>,
  #[serde(default)]
  pub min: Vec<String>,
  #[serde(default)]
  pub max: Vec<String>,
  #[serde(default)]
  pub group_by: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct SortFieldRequest {
  pub field: String,
  pub direction: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WhereClause {
  pub field: String,
  pub op: String,
  pub value: serde_json::Value,
  pub value2: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Projection helpers
// ---------------------------------------------------------------------------

/// Map virtual `@`-prefixed field names to their actual JSON keys.
fn map_select_fields(select: &[String]) -> Vec<String> {
  select
    .iter()
    .map(|s| match s.as_str() {
      "@path" => "path".to_string(),
      "@score" => "score".to_string(),
      "@size" => "size".to_string(),
      "@content_type" => "content_type".to_string(),
      "@created_at" => "created_at".to_string(),
      "@updated_at" => "updated_at".to_string(),
      "@content_hash" => "content_hash".to_string(),
      "@matched_by" => "matched_by".to_string(),
      "@matches" => "matches".to_string(),
      other => other.to_string(),
    })
    .collect()
}

/// Filter a JSON response to include only selected fields.
/// For arrays of objects (results), filters each object.
/// For objects with an "items" array (envelope), filters each item inside.
/// Envelope fields (has_more, next_cursor, etc.) are never stripped.
fn apply_projection(response: &mut serde_json::Value, select: &[String]) {
  if select.is_empty() {
    return;
  }

  // Build the set of allowed keys
  let allowed: std::collections::HashSet<&str> = select.iter().map(|s| s.as_str()).collect();

  if let Some(obj) = response.as_object_mut() {
    // Check if this is an envelope with "items" array
    if let Some(results) = obj.get_mut("items") {
      if let Some(arr) = results.as_array_mut() {
        for item in arr.iter_mut() {
          filter_object(item, &allowed);
        }
      }
    }
    // else: flat object (e.g., aggregation result) — don't filter it
  } else if let Some(arr) = response.as_array_mut() {
    // Flat array of results
    for item in arr.iter_mut() {
      filter_object(item, &allowed);
    }
  }
}

fn filter_object(value: &mut serde_json::Value, allowed: &std::collections::HashSet<&str>) {
  if let Some(obj) = value.as_object_mut() {
    let keys: Vec<String> = obj.keys().cloned().collect();
    for key in keys {
      if !allowed.contains(key.as_str()) {
        obj.remove(&key);
      }
    }
  }
}

/// POST /query -- execute an index query and return matching file metadata.
/// Supports both legacy array format and nested boolean object format.
/// Always returns paginated envelope: { results, has_more, next_cursor?, prev_cursor?, total? }
pub async fn query_endpoint(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  active_key_rules: Option<Extension<ActiveKeyRules>>,
  Json(body): Json<QueryRequest>,
) -> Response {
  if let Err(response) = require_generic_data_path(&state, &body.path) {
    return response;
  }
  let family_policy = match SystemFamilyPolicyResolver::new(state.engine.hash_algo()) {
    Ok(policy) => policy,
    Err(error) => return engine_error_response("Failed to load generic data policy", &error),
  };

  // Parse the where clause into a QueryNode tree.
  let query_node = match parse_where_clause(&body.r#where) {
    Ok(node) => node,
    Err(message) => {
      return ErrorResponse::new(message).with_status(StatusCode::BAD_REQUEST).into_response();
    }
  };

  // Check for empty where clause (AND with no children).
  let is_empty = matches!(&query_node, QueryNode::And(children) if children.is_empty());

  // Parse order_by
  let order_by: Vec<SortField> = body
    .order_by
    .as_ref()
    .map(|fields| {
      fields
        .iter()
        .map(|f| SortField {
          field: f.field.clone(),
          direction: match f.direction.as_deref() {
            Some("desc") => SortDirection::Desc,
            _ => SortDirection::Asc,
          },
        })
        .collect()
    })
    .unwrap_or_default();

  // Determine explain mode
  let explain_mode = match body.explain.as_ref() {
    Some(v) if v == "analyze" || v == &serde_json::json!("analyze") => ExplainMode::Analyze,
    Some(v) if v.as_bool().unwrap_or(false) || v == "plan" || v == &serde_json::json!("plan") => ExplainMode::Plan,
    _ => ExplainMode::Off,
  };
  let request_budget = match state.engine.start_query_request_budget() {
    Ok(request_budget) => request_budget,
    Err(error) => return engine_error_response("Query admission failed", &error),
  };

  // Handle EXPLAIN mode -- short-circuits normal response path
  if explain_mode != ExplainMode::Off {
    let agg = body.aggregate.as_ref().map(|agg_data| AggregateQuery {
      count: agg_data.count,
      sum: agg_data.sum.clone(),
      avg: agg_data.avg.clone(),
      min: agg_data.min.clone(),
      max: agg_data.max.clone(),
      group_by: agg_data.group_by.clone(),
    });

    let query = Query {
      path: body.path.clone(),
      field_queries: Vec::new(),
      node: if is_empty { None } else { Some(query_node.clone()) },
      limit: body.limit,
      offset: body.offset,
      order_by: order_by.clone(),
      after: body.after.clone(),
      before: body.before.clone(),
      include_total: body.include_total.unwrap_or(false),
      strategy: QueryStrategy::Full,
      aggregate: agg,
      explain: explain_mode,
    };

    let query_engine = QueryEngine::with_request_budget(&state.engine, request_budget.clone());
    match query_engine.execute_explain_filtered(&query, |result| family_policy.generic_data_path_is_visible(&result.file_record.path)) {
      Ok(result) => {
        let response = match serialize_response_value(&result, "Explain response") {
          Ok(response) => response,
          Err(response) => return response,
        };
        return (StatusCode::OK, Json(response)).into_response();
      }
      Err(error) => return engine_error_response("Explain failed", &error),
    }
  }

  // If aggregate query, use execute_aggregate
  if let Some(ref agg_data) = body.aggregate {
    let agg_query = AggregateQuery {
      count: agg_data.count,
      sum: agg_data.sum.clone(),
      avg: agg_data.avg.clone(),
      min: agg_data.min.clone(),
      max: agg_data.max.clone(),
      group_by: agg_data.group_by.clone(),
    };

    let query = Query {
      path: body.path.clone(),
      field_queries: Vec::new(),
      node: if is_empty { None } else { Some(query_node) },
      limit: body.limit,
      offset: body.offset,
      order_by,
      after: body.after.clone(),
      before: body.before.clone(),
      include_total: body.include_total.unwrap_or(false),
      strategy: QueryStrategy::Full,
      aggregate: Some(agg_query),
      explain: ExplainMode::Off,
    };

    let query_engine = QueryEngine::with_request_budget(&state.engine, request_budget.clone());
    match query_engine.execute_aggregate_filtered(&query, |result| family_policy.generic_data_path_is_visible(&result.file_record.path)) {
      Ok(result) => {
        let mut response_value = match serialize_response_value(&result, "Aggregation response") {
          Ok(response) => response,
          Err(response) => return response,
        };
        // Apply projection if select is specified
        if let Some(ref select) = body.select {
          if !select.is_empty() {
            let mapped = map_select_fields(select);
            apply_projection(&mut response_value, &mapped);
          }
        }
        return (StatusCode::OK, Json(response_value)).into_response();
      }
      Err(EngineError::NotFound(msg)) => {
        return ErrorResponse::new(msg).with_status(StatusCode::BAD_REQUEST).into_response();
      }
      Err(error @ (EngineError::ShuttingDown | EngineError::Cancelled(_) | EngineError::ResourceExhausted(_))) => {
        return engine_error_response("Aggregation failed", &error);
      }
      Err(e) => {
        return ErrorResponse::new(format!("Aggregation failed: {}", e)).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response();
      }
    }
  }

  let query = Query {
    path: body.path.clone(),
    field_queries: Vec::new(),
    node: if is_empty { None } else { Some(query_node.clone()) },
    limit: body.limit,
    offset: body.offset,
    order_by,
    after: body.after.clone(),
    before: body.before.clone(),
    include_total: body.include_total.unwrap_or(false),
    strategy: QueryStrategy::Full,
    aggregate: None,
    explain: ExplainMode::Off,
  };

  let query_engine = QueryEngine::with_request_budget(&state.engine, request_budget.clone());
  match query_engine.execute_paginated_filtered(&query, |result| family_policy.generic_data_path_is_visible(&result.file_record.path)) {
    Ok(paginated) => {
      let response_items: Vec<serde_json::Value> = paginated
        .results
        .iter()
        .map(|result| {
          serde_json::json!({
            "path": result.file_record.path,
            "size": result.file_record.total_size,
            "content_type": result.file_record.content_type,
            "created_at": result.file_record.created_at,
            "updated_at": result.file_record.updated_at,
            "score": result.score,
            "matched_by": result.matched_by,
          })
        })
        .collect();

      // Filter query results by API key rules — denied paths are silently omitted
      let mut response_items = if let Some(Extension(ref rules)) = active_key_rules {
        if !rules.0.is_empty() {
          let mut items = response_items;
          items.retain(|item| {
            let path = item["path"].as_str().unwrap_or("");
            let normalized = if path.starts_with('/') { path.to_string() } else { format!("/{}", path) };
            match match_rules(&rules.0, &normalized) {
              Some(rule) => check_operation_permitted(&rule.permitted, 'r'),
              None => false,
            }
          });
          items
        } else {
          response_items
        }
      } else {
        response_items
      };

      // Filter query results by user/group permissions. Query is exempt from
      // path-level middleware, so authorization happens here: a user only
      // sees files they have direct Read on (grants + grant inheritance).
      // Root short-circuits; share keys are handled by the key_rules branch
      // above.
      if !claims.sub.starts_with("share:") {
        if let Err(error) = filter_results_by_direct_read(&mut response_items, &claims.sub, &state.engine, &state.group_cache) {
          return engine_error_response("Failed to filter query permissions", &error);
        }
      }

      let locator_options = LocatorOptions::from_request(&body.locators);
      if locator_options.include_matches {
        let locator_terms = if is_empty { Vec::new() } else { terms_from_query_node(&query_node) };
        if let Err(error) = enrich_query_items_with_locators(
          state.engine.as_ref(),
          &paginated.results,
          &mut response_items,
          &locator_terms,
          &locator_options,
          &request_budget,
        ) {
          return engine_error_response("Query locator generation failed", &error);
        }
      }

      let mut response = serde_json::json!({
        "items": response_items,
        "has_more": paginated.has_more,
      });

      if let Some(total) = paginated.total_count {
        response["total"] = serde_json::json!(total);
      }
      if let Some(ref cursor) = paginated.next_cursor {
        response["next_cursor"] = serde_json::json!(cursor);
      }
      if let Some(ref cursor) = paginated.prev_cursor {
        response["prev_cursor"] = serde_json::json!(cursor);
      }
      if paginated.default_limit_hit {
        response["default_limit_hit"] = serde_json::json!(true);
        response["default_limit"] = serde_json::json!(DEFAULT_QUERY_LIMIT);
      }

      // Add reindex meta if a reindex is active for the query path
      let meta = state.task_queue.as_ref().and_then(|queue| {
        queue.get_reindex_progress_for_path(&body.path).map(|info| QueryMeta {
          reindexing: Some(info.progress),
          reindexing_eta: info.eta_ms,
          reindexing_indexed: Some(info.indexed_count),
          reindexing_total: Some(info.total_count),
          reindexing_stale_since: info.stale_since,
        })
      });
      if let Some(ref meta) = meta {
        response["meta"] = match serialize_response_value(meta, "Query metadata response") {
          Ok(meta) => meta,
          Err(response) => return response,
        };
      }

      // Apply projection if select is specified
      if let Some(ref select) = body.select {
        if !select.is_empty() {
          let mapped = map_select_fields(select);
          apply_projection(&mut response, &mapped);
        }
      }

      (StatusCode::OK, Json(response)).into_response()
    }
    Err(EngineError::NotFound(message)) => ErrorResponse::new(message).with_status(StatusCode::NOT_FOUND).into_response(),
    Err(EngineError::JsonParseError(message)) => ErrorResponse::new(message).with_status(StatusCode::BAD_REQUEST).into_response(),
    Err(EngineError::RangeQueryNotSupported(converter_name)) => {
      ErrorResponse::new(format!("Range query not supported for converter '{}'", converter_name,))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response()
    }
    Err(error @ (EngineError::ShuttingDown | EngineError::Cancelled(_) | EngineError::ResourceExhausted(_))) => {
      engine_error_response("Query failed", &error)
    }
    Err(error) => {
      tracing::error!("Query execution failed: {}", error);
      ErrorResponse::new(format!("Query failed: {}", error)).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response()
    }
  }
}

// ---------------------------------------------------------------------------
// Rename / move
// ---------------------------------------------------------------------------

/// Request body for POST /engine-rename/{*path}.
#[derive(Deserialize)]
pub struct RenameRequest {
  pub to: Option<String>,
}

/// POST /engine-rename/{*path} -- rename (move) a file or symlink.
/// Maximum merge-patch input/stored size — both the incoming body and
/// the on-disk file have to fit in memory simultaneously for the
/// read-merge-write cycle.
const MAX_MERGE_PATCH_BYTES: usize = 10 * 1024 * 1024;

#[derive(Deserialize, Default)]
pub struct MergePatchQuery {
  /// Signed merge depth.
  ///   * `None`          → strict RFC 7396 (unbounded recursion).
  ///   * `Some(0)`       → wholesale document replace (PUT semantics).
  ///   * `Some(N > 0)`   → merge N levels deep; object values beyond
  ///                       that boundary REPLACE the target subtree.
  ///   * `Some(N < 0)`   → merge |N| levels deep; object values beyond
  ///                       that boundary PRESERVE the existing target
  ///                       subtree (patch's deeper objects ignored).
  /// Scalars and `null` patch values always behave the same regardless
  /// of sign — `null` deletes, scalars insert/replace at the merge level.
  depth: Option<i64>,
}

/// PATCH /files/{*path} — dispatcher.
///
/// PATCH on a file is overloaded by `Content-Type`:
///   * `application/merge-patch+json` → RFC 7396 JSON merge into the
///     stored file. Body must be JSON; stored file must be JSON (or
///     absent). Optional `?depth=N` bounds the merge recursion.
///   * anything else → legacy rename behavior. Body is parsed as
///     `{"to": "/new/path"}` and the file/symlink is moved.
pub async fn engine_patch(
  state: State<AppState>,
  claims: Extension<TokenClaims>,
  AxumQuery(merge_q): AxumQuery<MergePatchQuery>,
  path: Path<String>,
  headers: HeaderMap,
  body: Body,
) -> Response {
  let content_type =
    headers.get("content-type").and_then(|v| v.to_str().ok()).map(|s| s.split(';').next().unwrap_or(s).trim().to_lowercase());

  if content_type.as_deref() == Some("application/merge-patch+json") {
    return do_merge_patch(state, claims, path, merge_q, body).await;
  }
  do_rename(state, claims, path, body).await
}

async fn do_merge_patch(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(path): Path<String>,
  merge_q: MergePatchQuery,
  body: Body,
) -> Response {
  use crate::engine::merge_patch::MergeDepth;

  if let Err(response) = require_generic_data_path(&state, &path) {
    return response;
  }

  let depth = match merge_q.depth {
    None => MergeDepth::Unbounded,
    Some(0) => MergeDepth::FullReplace,
    Some(n) if n > 0 => MergeDepth::ReplaceBeyond(n as u32),
    Some(n) => MergeDepth::PreserveBeyond(n.unsigned_abs() as u32),
  };

  // Read and validate the patch body.
  let body_bytes = match axum::body::to_bytes(body, MAX_MERGE_PATCH_BYTES).await {
    Ok(b) => b,
    Err(_) => {
      return ErrorResponse::new(format!("Patch body exceeds {} bytes or could not be read", MAX_MERGE_PATCH_BYTES))
        .with_status(StatusCode::PAYLOAD_TOO_LARGE)
        .into_response();
    }
  };
  let patch_value: serde_json::Value = match serde_json::from_slice(&body_bytes) {
    Ok(v) => v,
    Err(e) => {
      return ErrorResponse::new(format!("Patch body is not valid JSON: {}", e))
        .with_status(StatusCode::UNSUPPORTED_MEDIA_TYPE)
        .into_response();
    }
  };

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let engine = state.engine.clone();
  let path_for_blocking = path.clone();

  // Run the complete read-merge-write operation on one namespace authority
  // inside a blocking worker so concurrent patches cannot lose updates.
  let result = tokio::task::spawn_blocking(move || -> EngineResult<(FileRecord, bool)> {
    let ops = DirectoryOps::new(&engine);
    let merged = ops.merge_json_file_bounded(&ctx, &path_for_blocking, patch_value, depth, Some(MAX_MERGE_PATCH_BYTES))?;
    Ok((merged.file_record, !merged.created))
  })
  .await;

  let (file_record, existed) = match result {
    Ok(Ok(v)) => v,
    Ok(Err(EngineError::InvalidInput(msg))) => {
      // Differentiate "stored file isn't JSON" (415) from "stored too big" (413).
      let status = if msg.contains("exceeds") && msg.contains("byte merge cap") {
        StatusCode::PAYLOAD_TOO_LARGE
      } else if msg.contains("not valid JSON") {
        StatusCode::UNSUPPORTED_MEDIA_TYPE
      } else {
        StatusCode::BAD_REQUEST
      };
      return ErrorResponse::new(msg).with_status(status).into_response();
    }
    Ok(Err(error)) => {
      tracing::error!("Engine: failed merge-patch at '{}': {}", path, error);
      return engine_error_response("Merge-patch failed", &error);
    }
    Err(join_error) => {
      tracing::error!("merge-patch task panicked: {}", join_error);
      return ErrorResponse::new("Merge-patch failed: internal task error").with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
  };

  let algo = state.engine.hash_algo();
  let response_body = match engine_file_response_with_hash(&file_record, algo) {
    Ok(response) => response,
    Err(error) => {
      tracing::error!(path, %error, "Merged file but could not construct its HTTP response hash");
      return ErrorResponse::new(
        "The merge was saved, but its response hash could not be constructed; inspect server health before retrying".to_string(),
      )
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response();
    }
  };

  let status = if existed { StatusCode::OK } else { StatusCode::CREATED };
  (status, Json(response_body)).into_response()
}

async fn do_rename(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(path): Path<String>,
  body: Body,
) -> Response {
  // Buffer the body to JSON-parse it (axum's Json<T> extractor isn't
  // usable inside the dispatcher because we already consumed headers
  // separately).
  let body_bytes = match axum::body::to_bytes(body, 64 * 1024).await {
    Ok(b) => b,
    Err(_) => {
      return ErrorResponse::new("Rename request body too large or unreadable").with_status(StatusCode::BAD_REQUEST).into_response();
    }
  };
  let payload: RenameRequest = match serde_json::from_slice(&body_bytes) {
    Ok(v) => v,
    Err(e) => {
      return ErrorResponse::new(format!("Rename body must be JSON {{\"to\": ...}}: {}", e))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  };

  let destination = match payload.to {
    Some(ref t) if !t.is_empty() => t.as_str(),
    _ => {
      return ErrorResponse::new("Request must include non-empty 'to' field. Rename requires {\"to\": \"/new/path\"}")
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  };

  if let Err(response) = require_generic_data_path(&state, &path) {
    return response;
  }
  if let Err(response) = require_generic_data_path(&state, destination) {
    return response;
  }

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let engine = state.engine.clone();
  let path_for_blocking = path.clone();
  let destination_owned = destination.to_string();

  let result = run_engine_blocking("rename", "Rename failed", move || -> EngineResult<&'static str> {
    let ops = DirectoryOps::new(&engine);
    if ops.get_symlink(&path_for_blocking)?.is_some() {
      ops.rename_symlink(&ctx, &path_for_blocking, &destination_owned)?;
      Ok("symlink")
    } else {
      ops.rename_file(&ctx, &path_for_blocking, &destination_owned)?;
      Ok("file")
    }
  })
  .await;

  match result {
    Ok(kind) => {
      let from_normalized = crate::engine::path_utils::normalize_path(&path);
      let to_normalized = crate::engine::path_utils::normalize_path(destination);
      (
        StatusCode::OK,
        Json(serde_json::json!({
          "from": from_normalized,
          "to": to_normalized,
          "entry_type": kind,
        })),
      )
        .into_response()
    }
    Err(response) => response,
  }
}

// ---------------------------------------------------------------------------
// System repair
// ---------------------------------------------------------------------------

/// POST /system/repair — trigger a KV index rebuild from the append log.
pub async fn repair_kv(State(state): State<AppState>, Extension(claims): Extension<TokenClaims>) -> Response {
  let caller_id = match uuid::Uuid::parse_str(&claims.sub) {
    Ok(id) => id,
    Err(_) => return ErrorResponse::new("Invalid token").with_status(StatusCode::UNAUTHORIZED).into_response(),
  };

  if !crate::engine::user::is_root(&caller_id) {
    return ErrorResponse::new("Root access required for repair operations").with_status(StatusCode::FORBIDDEN).into_response();
  }

  let source_operation_id = uuid::Uuid::new_v4().into_bytes();
  match state.engine.repair_kv_and_admit_index_maintenance_v1(source_operation_id) {
    Ok(()) => (
      StatusCode::OK,
      Json(serde_json::json!({
          "status": "ok",
          "message": "KV index rebuilt successfully",
      })),
    )
      .into_response(),
    Err(e) => ErrorResponse::new(format!("Repair failed: {}", e)).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response(),
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// engine_error_status / sanitize_engine_error live in server::responses now;
// import them at the top of this file. Keep this section header for navigation.

// ---------------------------------------------------------------------------
// POST /files/copy — copy one or more files/directories to a destination
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CopyRequest {
  pub paths: Vec<String>,
  pub destination: String,
}

pub async fn copy_files(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Json(payload): Json<CopyRequest>,
) -> Response {
  let dest_normalized = crate::engine::path_utils::normalize_path(&payload.destination);

  if let Err(response) = require_generic_data_path(&state, &dest_normalized) {
    return response;
  }
  for path in &payload.paths {
    if let Err(response) = require_generic_data_path(&state, path) {
      return response;
    }
  }

  // User/group permission check: /files/copy is exempt from path-aware
  // middleware, so without this every authenticated user could copy any
  // file to any location. Required: Read on each source AND Create on
  // the destination directory.
  if let Err(response) = reject_share_key(&claims, "Share keys cannot copy files") {
    return response;
  };
  let permissions = match RoutePermissionChecker::from_claims(&state, &claims, "Invalid user identity") {
    Ok(permissions) => permissions,
    Err(response) => return response,
  };
  if !permissions.is_root() {
    // Source check first so a 404 on an unauthorized source isn't masked
    // by a 403 on an unauthorized destination.
    for raw_path in &payload.paths {
      let normalized = crate::engine::path_utils::normalize_path(raw_path);
      let permitted = match permissions.has_any_path_permission(&normalized, &[CrudlifyOp::Read, CrudlifyOp::List]) {
        Ok(permitted) => permitted,
        Err(error) => return engine_error_response("Failed to check copy source permission", &error),
      };
      if !permitted {
        return ErrorResponse::new(format!("Not found: {}", raw_path)).with_status(StatusCode::NOT_FOUND).into_response();
      }
    }
    let destination_permitted = match permissions.has_path_permission(&dest_normalized, CrudlifyOp::Create) {
      Ok(permitted) => permitted,
      Err(error) => return engine_error_response("Failed to check copy destination permission", &error),
    };
    if !destination_permitted {
      return ErrorResponse::new("Permission denied").with_status(StatusCode::FORBIDDEN).into_response();
    }
  }

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let engine = state.engine.clone();
  let paths = payload.paths.clone();
  let dest_for_blocking = dest_normalized.clone();

  let copied = match tokio::task::spawn_blocking(move || {
    let ops = DirectoryOps::new(&engine);
    ops.copy_paths(&ctx, &paths, &dest_for_blocking)
  })
  .await
  {
    Ok(Ok(copied)) => copied,
    Ok(Err(error)) => return engine_error_response("Copy failed", &error),
    Err(join_error) => {
      tracing::error!("copy task panicked: {}", join_error);
      return ErrorResponse::new("Copy failed: internal task error").with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
  };

  (StatusCode::OK, Json(serde_json::json!({ "copied": copied }))).into_response()
}

// ---------------------------------------------------------------------------
// POST /files/search — global cross-directory search
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct GlobalSearchRequest {
  pub query: Option<String>,
  #[serde(rename = "where")]
  pub where_clause: Option<serde_json::Value>,
  pub path: Option<String>,
  pub limit: Option<usize>,
  pub offset: Option<usize>,
  #[serde(flatten)]
  pub locators: LocatorOptionsRequest,
}

pub async fn global_search_endpoint(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Json(payload): Json<GlobalSearchRequest>,
) -> Response {
  if payload.query.is_none() && payload.where_clause.is_none() {
    return ErrorResponse::new("At least one of 'query' or 'where' is required").with_status(StatusCode::BAD_REQUEST).into_response();
  }

  let query_node = match payload.where_clause.as_ref() {
    Some(value) => match parse_where_clause(value) {
      Ok(node) => Some(node),
      Err(msg) => return ErrorResponse::new(msg).with_status(StatusCode::BAD_REQUEST).into_response(),
    },
    None => None,
  };

  let base_path = payload.path.as_deref().unwrap_or("/");
  if let Err(response) = require_generic_data_path(&state, base_path) {
    return response;
  }
  let limit = payload.limit.map(|l| l.min(1000));
  let offset = payload.offset;
  let request_budget = match state.engine.start_query_request_budget() {
    Ok(request_budget) => request_budget,
    Err(error) => return engine_error_response("Search admission failed", &error),
  };

  match crate::engine::search::global_search_with_budget(
    &state.engine,
    base_path,
    payload.query.as_deref(),
    query_node.as_ref(),
    limit,
    offset,
    &request_budget,
  ) {
    Ok(results) => {
      let mut items: Vec<serde_json::Value> = results
        .results
        .iter()
        .map(|r| {
          serde_json::json!({
            "path": r.path,
            "score": r.score,
            "matched_by": r.matched_by,
            "source": r.source_dir,
            "size": r.size,
            "content_type": r.content_type,
            "created_at": r.created_at,
            "updated_at": r.updated_at,
          })
        })
        .collect();

      if let Err(error) = filter_generic_data_items(&state.engine, &mut items) {
        return engine_error_response("Failed to classify search results", &error);
      }

      // Filter search results by user/group permissions. Search is exempt
      // from path-level middleware, so authorization happens here: a user
      // only sees files they have direct Read on (grants + inheritance).
      if !claims.sub.starts_with("share:") {
        if let Err(error) = filter_results_by_direct_read(&mut items, &claims.sub, &state.engine, &state.group_cache) {
          return engine_error_response("Failed to filter search permissions", &error);
        }
      }

      let locator_options = LocatorOptions::from_request(&payload.locators);
      if locator_options.include_matches {
        if let Err(error) = enrich_search_items_with_locators(
          state.engine.as_ref(),
          &results.results,
          &mut items,
          payload.query.as_deref(),
          query_node.as_ref(),
          &locator_options,
          &request_budget,
        ) {
          return engine_error_response("Search locator generation failed", &error);
        }
      }

      let mut response = serde_json::json!({
        "results": items,
        "has_more": results.has_more,
      });
      if let Some(total) = results.total_count {
        response["total_count"] = serde_json::json!(total);
      }
      (StatusCode::OK, Json(response)).into_response()
    }
    Err(error @ (EngineError::ShuttingDown | EngineError::Cancelled(_) | EngineError::ResourceExhausted(_))) => {
      engine_error_response("Search failed", &error)
    }
    Err(error) => {
      tracing::error!("Global search failed: {}", error);
      ErrorResponse::new(format!("Search failed: {}", error)).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response()
    }
  }
}

#[cfg(test)]
#[path = "../../spec/server/engine_routes_error_internal_spec.rs"]
mod engine_routes_error_internal_spec;
