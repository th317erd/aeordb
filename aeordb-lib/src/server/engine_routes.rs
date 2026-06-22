use std::collections::VecDeque;

use axum::{
  Extension,
  body::Body,
  extract::{Path, Query as AxumQuery, State},
  http::{header, HeaderMap, StatusCode},
  response::{IntoResponse, Response},
  Json,
};
use futures_util::{stream, StreamExt};
use serde::Deserialize;
use uuid::Uuid;

use super::blocking::run_engine_blocking;
use super::cache_invalidation::{evict_caches_for_path, evict_caches_for_paths};
use super::route_permissions::{reject_share_key, RoutePermissionChecker};
use super::responses::{engine_error_response, EngineFileResponse, ErrorResponse};
use super::search_locators::{broad_query_terms, generate_locators, terms_from_query_node, LocatorOptions, LocatorOptionsRequest, LocatorTerm};
use super::state::AppState;
use crate::auth::TokenClaims;
use crate::auth::permission_middleware::ActiveKeyRules;
use crate::engine::api_key_rules::{match_rules, check_operation_permitted};
use crate::engine::{DirectoryOps, RequestContext, SearchResult, StorageEngine, TaskStatus, VersionManager, is_root};
use crate::engine::directory_listing::list_directory_recursive;
use crate::engine::directory_ops::{is_system_path, EngineFileStream, file_content_hash};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::ChunkReadLocation;
use crate::engine::index_config::PathIndexConfig;
use crate::engine::permission_resolver::CrudlifyOp;
use crate::engine::query_engine::{
  parse_where_clause, AggregateQuery, ExplainMode, Query, QueryEngine, QueryMeta, QueryNode, QueryResult, QueryStrategy, SortDirection,
  SortField, DEFAULT_QUERY_LIMIT,
};
use crate::engine::symlink_resolver::{resolve_symlink, ResolvedTarget};

/// Check if a file path is deleted and the user lacks delete permission.
/// Deleted files are invisible/inaccessible to users without 'd' permission.
fn is_deleted_and_forbidden(state: &AppState, claims: &TokenClaims, path: &str) -> bool {
  use crate::engine::directory_ops::file_path_hash;

  let user_id = match Uuid::parse_str(&claims.sub) {
    Ok(id) => id,
    Err(_) => return false,
  };

  // Root can see everything
  if is_root(&user_id) {
    return false;
  }

  // Check if the file is deleted in the KV store
  let algo = state.engine.hash_algo();
  let normalized = crate::engine::path_utils::normalize_path(path);
  let file_key = match file_path_hash(&normalized, &algo) {
    Ok(key) => key,
    Err(_) => return false,
  };

  let is_deleted = state.engine.is_entry_deleted(&file_key).unwrap_or(false);
  if !is_deleted {
    return false;
  }

  // File is deleted — check if user has 'd' permission
  let has_delete = RoutePermissionChecker::for_user(state, user_id).has_permission(&normalized, CrudlifyOp::Delete);

  !has_delete
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

  if is_system_path(&normalized) {
    return ErrorResponse::new(format!("Not found: {}", body.path)).with_status(StatusCode::NOT_FOUND).into_response();
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
    if !permissions.has_path_permission(&parent, CrudlifyOp::Create) {
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
  // Block ALL access to /.aeordb-system/ via API — system data is only accessible
  // through the internal system_store module, never through HTTP endpoints.
  if is_system_path(&path) {
    return ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response();
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

  // Auto-trigger reindex when indexes.json is stored
  if path.ends_with("/.aeordb-config/indexes.json") {
    if let Some(ref queue) = state.task_queue {
      let parent = path.trim_end_matches("/.aeordb-config/indexes.json");
      let parent = if parent.is_empty() { "/" } else { parent };
      let reindex_path = format!("/{}", parent.trim_start_matches('/'));

      // Cancel any existing reindex for this path
      if let Ok(tasks) = queue.list_tasks() {
        for task in &tasks {
          if task.task_type == "reindex"
            && task.args.get("path").and_then(|v| v.as_str()) == Some(&reindex_path)
            && (task.status == TaskStatus::Pending || task.status == TaskStatus::Running)
          {
            let _ = queue.cancel(&task.id);
          }
        }
      }

      let metadata_only = DirectoryOps::new(&state.engine)
        .read_file_buffered(&path)
        .ok()
        .and_then(|data| PathIndexConfig::deserialize(&data).ok())
        .map(|config| config.indexes.iter().all(|field| field.name.starts_with('@')))
        .unwrap_or(false);

      // Enqueue new reindex
      let _ = queue.enqueue("reindex", serde_json::json!({"path": reindex_path, "metadata_only": metadata_only}));
    }
  }

  let mut response_body = EngineFileResponse::from(&file_record);

  // Compute the content-addressed hash so the caller can fetch by hash.
  let algo = state.engine.hash_algo();
  let hash_length = algo.hash_length();
  let file_value = match file_record.serialize(hash_length) {
    Ok(v) => v,
    Err(_) => return ErrorResponse::new(
      "Failed to serialize file record after storing. The file was saved but the response could not be built — contact your administrator"
        .to_string(),
    )
    .with_status(StatusCode::INTERNAL_SERVER_ERROR)
    .into_response(),
  };
  if let Ok(content_hash) = file_content_hash(&file_value, &algo) {
    response_body.hash = Some(hex::encode(&content_hash));
  }

  evict_caches_for_path(&state, &path);

  (StatusCode::CREATED, Json(response_body)).into_response()
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

const RANGE_READ_SPAN_MAX_BYTES: u64 = 32 * 1024 * 1024;
const RANGE_READ_SPAN_MAX_GAP_BYTES: u64 = 256 * 1024;

struct LegacyEngineByteRangeStream {
  chunk_hashes: Vec<Vec<u8>>,
  engine: std::sync::Arc<crate::engine::StorageEngine>,
  include_deleted: bool,
  use_metadata_skip: bool,
  current_index: usize,
  cursor: u64,
  range_start: u64,
  range_end_exclusive: u64,
}

impl LegacyEngineByteRangeStream {
  fn new(
    chunk_hashes: Vec<Vec<u8>>,
    engine: std::sync::Arc<crate::engine::StorageEngine>,
    include_deleted: bool,
    use_metadata_skip: bool,
    range: HttpByteRange,
  ) -> Self {
    Self {
      chunk_hashes,
      engine,
      include_deleted,
      use_metadata_skip,
      current_index: 0,
      cursor: 0,
      range_start: range.start,
      range_end_exclusive: range.end.saturating_add(1),
    }
  }

  fn chunk_metadata_len(&self, hash: &[u8]) -> EngineResult<Option<u64>> {
    if self.include_deleted || !self.use_metadata_skip {
      return Ok(None);
    }
    Ok(self.engine.get_chunk_metadata(hash)?.and_then(|metadata| metadata.raw_value_length))
  }

  fn read_chunk(&self, hash: &[u8]) -> EngineResult<Vec<u8>> {
    let entry = if self.include_deleted {
      self.engine.read_chunk_verified_including_deleted(hash)?
    } else {
      self.engine.read_chunk_verified(hash)?
    };
    match entry {
      Some(value) => Ok(value),
      None => Err(EngineError::NotFound(format!("Chunk not found: {}", hex::encode(hash)))),
    }
  }
}

impl Iterator for LegacyEngineByteRangeStream {
  type Item = EngineResult<Vec<u8>>;

  fn next(&mut self) -> Option<Self::Item> {
    while self.current_index < self.chunk_hashes.len() && self.cursor < self.range_end_exclusive {
      let hash = self.chunk_hashes[self.current_index].clone();
      self.current_index += 1;

      match self.chunk_metadata_len(&hash) {
        Ok(Some(chunk_len)) if self.cursor.saturating_add(chunk_len) <= self.range_start => {
          self.cursor = self.cursor.saturating_add(chunk_len);
          continue;
        }
        Ok(_) => {}
        Err(error) => return Some(Err(error)),
      }

      let chunk_start = self.cursor;
      let data = match self.read_chunk(&hash) {
        Ok(data) => data,
        Err(error) => return Some(Err(error)),
      };
      let chunk_len = data.len() as u64;
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

      return Some(Ok(data[start_in_chunk..end_in_chunk].to_vec()));
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
  engine: std::sync::Arc<crate::engine::StorageEngine>,
  next_index: usize,
  pending: VecDeque<Vec<u8>>,
  range_start: u64,
  range_end_exclusive: u64,
}

impl CoalescedEngineByteRangeStream {
  fn new(
    chunk_hashes: Vec<Vec<u8>>,
    engine: std::sync::Arc<crate::engine::StorageEngine>,
    range: HttpByteRange,
    total_size: u64,
  ) -> EngineResult<Option<Self>> {
    let range_start = range.start;
    let range_end_exclusive = range.end.saturating_add(1);
    let mut chunks = Vec::new();
    let mut cursor = 0u64;
    let mut total_metadata_size = 0u64;

    for hash in chunk_hashes {
      let metadata =
        engine.get_chunk_metadata(&hash)?.ok_or_else(|| EngineError::NotFound(format!("Chunk not found: {}", hex::encode(&hash))))?;
      let chunk_len =
        metadata.raw_value_length.ok_or_else(|| EngineError::InvalidInput(format!("Chunk length unavailable: {}", hex::encode(&hash))))?;
      total_metadata_size = total_metadata_size
        .checked_add(chunk_len)
        .ok_or_else(|| EngineError::InvalidInput("File metadata chunk lengths overflowed while planning byte range".to_string()))?;
      let chunk_start = cursor;
      let chunk_end = chunk_start
        .checked_add(chunk_len)
        .ok_or_else(|| EngineError::InvalidInput("File chunk offsets overflowed while planning byte range".to_string()))?;
      cursor = chunk_end;

      if chunk_end <= range_start {
        continue;
      }
      if chunk_start >= range_end_exclusive {
        break;
      }

      chunks.push(PlannedRangeChunk {
        hash,
        file_start: chunk_start,
        file_end: chunk_end,
        wal_offset: metadata.offset,
        wal_total_length: metadata.total_length,
      });
    }

    if total_metadata_size != total_size {
      tracing::debug!(
        expected_size = total_size,
        metadata_size = total_metadata_size,
        "Range stream falling back to per-chunk reads because cheap chunk metadata does not match logical file size"
      );
      return Ok(None);
    }

    Ok(Some(Self { chunks, engine, next_index: 0, pending: VecDeque::new(), range_start, range_end_exclusive }))
  }

  fn load_next_span(&mut self) -> EngineResult<()> {
    if self.next_index >= self.chunks.len() {
      return Ok(());
    }

    let start_index = self.next_index;
    let first = &self.chunks[start_index];
    let span_start = first.wal_offset;
    let mut span_end = first.wal_end()?;
    self.next_index += 1;

    while self.next_index < self.chunks.len() {
      let next = &self.chunks[self.next_index];
      if next.wal_offset < span_end {
        break;
      }
      let gap = next.wal_offset - span_end;
      let next_end = next.wal_end()?;
      let span_len = next_end
        .checked_sub(span_start)
        .ok_or_else(|| EngineError::InvalidInput("Chunk WAL span underflowed while planning byte range".to_string()))?;
      if gap > RANGE_READ_SPAN_MAX_GAP_BYTES || span_len > RANGE_READ_SPAN_MAX_BYTES {
        break;
      }
      span_end = next_end;
      self.next_index += 1;
    }

    let span_chunks = &self.chunks[start_index..self.next_index];
    let locations: Vec<ChunkReadLocation> = span_chunks.iter().map(PlannedRangeChunk::location).collect();
    let values = self.engine.read_chunk_span_verified(&locations)?;
    if values.len() != span_chunks.len() {
      return Err(EngineError::InvalidInput(format!(
        "Chunk span returned {} values for {} planned chunks",
        values.len(),
        span_chunks.len()
      )));
    }

    for (chunk, data) in span_chunks.iter().zip(values) {
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
        self.pending.push_back(data[start_in_chunk..end_in_chunk].to_vec());
      }
    }

    Ok(())
  }
}

impl Iterator for CoalescedEngineByteRangeStream {
  type Item = EngineResult<Vec<u8>>;

  fn next(&mut self) -> Option<Self::Item> {
    loop {
      if let Some(data) = self.pending.pop_front() {
        return Some(Ok(data));
      }
      if self.next_index >= self.chunks.len() {
        return None;
      }
      if let Err(error) = self.load_next_span() {
        return Some(Err(error));
      }
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
    engine: std::sync::Arc<crate::engine::StorageEngine>,
    include_deleted: bool,
    range: HttpByteRange,
    total_size: u64,
  ) -> EngineResult<Self> {
    if include_deleted {
      Ok(Self::Legacy(LegacyEngineByteRangeStream::new(chunk_hashes, engine, true, false, range)))
    } else {
      match CoalescedEngineByteRangeStream::new(chunk_hashes.clone(), std::sync::Arc::clone(&engine), range, total_size)? {
        Some(stream) => Ok(Self::Coalesced(stream)),
        None => Ok(Self::Legacy(LegacyEngineByteRangeStream::new(chunk_hashes, engine, false, false, range))),
      }
    }
  }
}

impl Iterator for EngineByteRangeStream {
  type Item = EngineResult<Vec<u8>>;

  fn next(&mut self) -> Option<Self::Item> {
    match self {
      EngineByteRangeStream::Coalesced(stream) => stream.next(),
      EngineByteRangeStream::Legacy(stream) => stream.next(),
    }
  }
}

fn build_file_streaming_response(
  engine: &std::sync::Arc<crate::engine::StorageEngine>,
  file_record: &FileRecord,
  symlink_target: Option<&str>,
  request_headers: &HeaderMap,
  include_deleted: bool,
  extra_headers: &[(&'static str, String)],
) -> Response {
  let range = match parse_range_header(request_headers, file_record.total_size) {
    Ok(range) => range,
    Err(()) => return range_not_satisfiable_response(file_record.total_size),
  };

  let (status, body, served_len, content_range) = if let Some(range) = range {
    let range_stream = match EngineByteRangeStream::new(
      file_record.chunk_hashes.clone(),
      std::sync::Arc::clone(engine),
      include_deleted,
      range,
      file_record.total_size,
    ) {
      Ok(stream) => stream,
      Err(error) => {
        tracing::error!("Engine: failed to prepare range stream for '{}': {}", file_record.path, error);
        return ErrorResponse::new(format!(
          "Failed to stream file range '{}': the file data may be corrupted. Contact your administrator",
          file_record.path
        ))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
      }
    };
    let chunk_stream = stream::iter(
      range_stream.map(|chunk_result| chunk_result.map(axum::body::Bytes::from).map_err(|error| std::io::Error::other(error.to_string()))),
    );
    (
      StatusCode::PARTIAL_CONTENT,
      Body::from_stream(chunk_stream),
      range.len(),
      Some(format!("bytes {}-{}/{}", range.start, range.end, file_record.total_size)),
    )
  } else {
    let file_stream = if include_deleted {
      EngineFileStream::from_chunk_hashes_including_deleted_owned(file_record.chunk_hashes.clone(), std::sync::Arc::clone(engine))
    } else {
      EngineFileStream::from_chunk_hashes_owned(file_record.chunk_hashes.clone(), std::sync::Arc::clone(engine))
    };
    let file_stream = match file_stream {
      Ok(s) => s,
      Err(error) => {
        tracing::error!("Engine: failed to stream file '{}': {}", file_record.path, error);
        return ErrorResponse::new(format!(
          "Failed to stream file '{}': the file data may be corrupted. Contact your administrator",
          file_record.path
        ))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
      }
    };
    let chunk_stream = stream::iter(
      file_stream.map(|chunk_result| chunk_result.map(axum::body::Bytes::from).map_err(|error| std::io::Error::other(error.to_string()))),
    );
    (StatusCode::OK, Body::from_stream(chunk_stream), file_record.total_size, None)
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
fn build_directory_listing(entries: &[crate::engine::ChildEntry], base_path: &str, directory_ops: &DirectoryOps) -> Vec<serde_json::Value> {
  let normalized = crate::engine::path_utils::normalize_path(base_path);
  entries
    .iter()
    .map(|child| {
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

      // Include symlink target in listing
      if child.entry_type == crate::engine::entry_type::EntryType::Symlink.to_u8() {
        if let Ok(Some(symlink_record)) = directory_ops.get_symlink(&child_path) {
          entry_json["target"] = serde_json::json!(symlink_record.target);
        }
      }

      entry_json
    })
    .collect()
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
) {
  use crate::engine::permission_resolver::{CrudlifyOp, PermissionResolver};

  let Ok(user_id) = uuid::Uuid::parse_str(user_id_str) else {
    return;
  };
  if crate::engine::is_root(&user_id) {
    return;
  }
  let resolver = PermissionResolver::new(engine, group_cache);
  results.retain(|entry| {
    let Some(path) = entry["path"].as_str() else {
      return false;
    };
    resolver.check_direct_permission(&user_id, path, CrudlifyOp::Read).unwrap_or(false)
  });
}

fn enrich_query_items_with_locators(
  engine: &StorageEngine,
  query_results: &[QueryResult],
  items: &mut [serde_json::Value],
  terms: &[LocatorTerm],
  options: &LocatorOptions,
) {
  for item in items {
    let Some(path) = json_item_path(item).map(str::to_string) else {
      continue;
    };
    let Some(result) = query_results.iter().find(|result| result.file_record.path == path) else {
      continue;
    };
    add_locator_fields(engine, item, &result.file_record, terms, options);
  }
}

fn enrich_search_items_with_locators(
  engine: &StorageEngine,
  search_results: &[SearchResult],
  items: &mut [serde_json::Value],
  query: Option<&str>,
  query_node: Option<&QueryNode>,
  options: &LocatorOptions,
) {
  let structured_terms = query_node.map(terms_from_query_node);
  let ops = DirectoryOps::new(engine);

  for item in items {
    let Some(path) = json_item_path(item).map(str::to_string) else {
      continue;
    };
    let Some(search_result) = search_results.iter().find(|result| result.path == path) else {
      continue;
    };
    let terms = match query {
      Some(query_text) => broad_query_terms(query_text, &search_result.matched_fields),
      None => structured_terms.clone().unwrap_or_default(),
    };
    let Ok(Some(file_record)) = ops.get_metadata(&path) else {
      continue;
    };
    add_locator_fields(engine, item, &file_record, &terms, options);
  }
}

fn add_locator_fields(
  engine: &StorageEngine,
  item: &mut serde_json::Value,
  file_record: &FileRecord,
  terms: &[LocatorTerm],
  options: &LocatorOptions,
) {
  let Some(object) = item.as_object_mut() else {
    return;
  };

  if !file_record.content_hash.is_empty() {
    object.insert("content_hash".to_string(), serde_json::json!(file_record.content_hash_hex()));
  }

  let generation = generate_locators(engine, file_record, terms, options);
  object.insert("matches".to_string(), serde_json::to_value(generation.matches).unwrap_or_else(|_| serde_json::Value::Array(Vec::new())));
  object.insert("matches_truncated".to_string(), serde_json::json!(generation.matches_truncated));
  object.insert("locator_status".to_string(), serde_json::json!(generation.locator_status));
}

fn json_item_path(item: &serde_json::Value) -> Option<&str> {
  item.get("path").and_then(|value| value.as_str())
}

fn apply_listing_filters(
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

  // Filter /.aeordb-system/ from ALL listings — system data is invisible through
  // the API for all users, including root.
  listing.retain(|entry| {
    let path = entry["path"].as_str().unwrap_or("");
    !path.starts_with("/.aeordb-")
  });

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

/// Compute effective_permissions for each listing item using the permission
/// resolver. Only runs for non-root users when items don't already have
/// effective_permissions (i.e., regular user/group shares, not scoped API keys).
fn attach_effective_permissions(
  listing: &mut [serde_json::Value],
  user_id: &Uuid,
  engine: &std::sync::Arc<crate::engine::StorageEngine>,
  group_cache: &std::sync::Arc<crate::engine::cache::Cache<crate::engine::cache_loaders::GroupLoader>>,
) {
  use crate::engine::permission_resolver::{CrudlifyOp, PermissionResolver};

  if crate::engine::is_root(user_id) {
    return;
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
      if resolver.check_permission(user_id, &path, *op).unwrap_or(false) {
        flags[i] = *ch;
      }
    }
    let perm_str: String = flags.iter().collect();
    if let Some(obj) = entry.as_object_mut() {
      obj.insert("effective_permissions".to_string(), serde_json::Value::String(perm_str));
    }
  }
}

/// Handle a symlink path: resolve and produce the appropriate file or
/// directory response, or return an error for dangling / cyclic symlinks.
fn handle_symlink_resolution(
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
    Ok(ResolvedTarget::File(ref file_record)) => {
      // Block ALL access to symlinks resolving to /.aeordb-system/ paths — system
      // data is invisible through the API for all users, including root.
      if is_system_path(&file_record.path) {
        return ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response();
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

      build_file_streaming_response(engine, file_record, Some(symlink_target), request_headers, false, &[])
    }
    Ok(ResolvedTarget::Directory(dir_path)) => {
      // Block ALL access to symlinks resolving to /.aeordb-system/ directories —
      // system data is invisible through the API for all users, including root.
      if is_system_path(&dir_path) {
        return ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response();
      }

      match directory_ops.list_directory(&dir_path) {
        Ok(entries) => {
          let mut listing = build_directory_listing(&entries, &dir_path, &directory_ops);
          match apply_listing_filters(&mut listing, key_rules, user_id_str, filtered_listing) {
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
fn handle_file_response(engine: &std::sync::Arc<crate::engine::StorageEngine>, path: &str, request_headers: &HeaderMap) -> Response {
  let directory_ops = DirectoryOps::new(engine);

  let file_record = match directory_ops.get_metadata(path) {
    Ok(Some(record)) => record,
    Ok(None) => {
      return ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response();
    }
    Err(error) => {
      tracing::error!("Engine: failed to get metadata for '{}': {}", path, error);
      return ErrorResponse::new(format!("Failed to read metadata for '{}'. The file may be corrupted — contact your administrator", path))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  build_file_streaming_response(engine, &file_record, None, request_headers, false, &[])
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

  match list_directory_recursive(engine, path, depth, glob, None) {
    Ok(entries) => {
      let mut listing: Vec<serde_json::Value> = entries
        .iter()
        .map(|entry| {
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

          // Include symlink target in listing
          if entry.entry_type == crate::engine::entry_type::EntryType::Symlink.to_u8() {
            if let Ok(Some(symlink_record)) = directory_ops.get_symlink(&entry.path) {
              entry_json["target"] = serde_json::json!(symlink_record.target);
            }
          }

          entry_json
        })
        .collect();

      match apply_listing_filters(&mut listing, key_rules, user_id_str, None) {
        Ok(()) => {
          if filtered_listing.is_some() {
            if let Some(st) = state {
              filter_results_by_direct_read(&mut listing, user_id_str, &st.engine, &st.group_cache);
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

  match directory_ops.list_directory(path) {
    Ok(entries) => {
      let mut listing = build_directory_listing(&entries, path, &directory_ops);
      match apply_listing_filters(&mut listing, key_rules, user_id_str, filtered_listing) {
        Ok(()) => {
          // Attach effective_permissions for non-root users
          if let Some(st) = state {
            if let Ok(uid) = uuid::Uuid::parse_str(user_id_str) {
              attach_effective_permissions(&mut listing, &uid, &st.engine, &st.group_cache);
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
  // Block ALL access to /.aeordb-system/ via API — system data is only accessible
  // through the internal system_store module, never through HTTP endpoints.
  if is_system_path(&path) {
    return ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response();
  }

  // Deleted files are invisible to users without 'd' permission
  if is_deleted_and_forbidden(&state, &claims, &path) {
    return ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response();
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
  if let Ok(Some(symlink_record)) = directory_ops.get_symlink(&path) {
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
    );
  }

  // Try as file first
  match directory_ops.get_metadata(&path) {
    Ok(Some(_file_record)) => {
      return handle_file_response(&state.engine, &path, &headers);
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

  build_file_streaming_response(&state.engine, &file_record, None, request_headers, true, &[])
}

/// DELETE /engine/*path -- delete a file via the custom storage engine.
pub async fn engine_delete_file(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(path): Path<String>,
) -> Response {
  // Block ALL access to /.aeordb-system/ via API — system data is only accessible
  // through the internal system_store module, never through HTTP endpoints.
  if is_system_path(&path) {
    return ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response();
  }

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let engine = state.engine.clone();
  let path_for_blocking = path.clone();

  // Dispatch + delete all happen on a blocking thread. The kind ("symlink" /
  // "file" / "directory") flows back to the response. Cache eviction stays on
  // the async side since it touches Arc'd state.
  let result = tokio::task::spawn_blocking(move || -> EngineResult<&'static str> {
    let ops = DirectoryOps::new(&engine);
    if ops.get_symlink(&path_for_blocking).ok().flatten().is_some() {
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
      evict_caches_for_path(&state, &path);
      if kind == "file" {
        state.index_cleanup.queue(path.clone());
      }
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

  if is_system_path(&path) {
    return ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response();
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
  if !permissions.is_root() && !permissions.has_path_permission(&path, CrudlifyOp::Delete) {
    return ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response();
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

  if is_system_path(dir_path) {
    return ErrorResponse::new(format!("Not found: {}", dir_path)).with_status(StatusCode::NOT_FOUND).into_response();
  }

  // Deleted files require 'd' permission — check on the directory
  let permissions = match RoutePermissionChecker::from_claims(&state, &claims, "Invalid user ID") {
    Ok(permissions) => permissions,
    Err(response) => return response,
  };
  if !permissions.is_root() && !permissions.has_permission(dir_path, CrudlifyOp::Delete) {
    return (
      StatusCode::OK,
      Json(serde_json::json!({
        "items": [],
        "total": 0,
      })),
    )
      .into_response();
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
  if is_system_path(&path) {
    return ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response();
  }

  // Deleted files are invisible to users without 'd' permission
  if is_deleted_and_forbidden(&state, &claims, &path) {
    return ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response();
  }

  let directory_ops = DirectoryOps::new(&state.engine);

  // Check symlink first
  if let Ok(Some(symlink_record)) = directory_ops.get_symlink(&path) {
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
      match directory_ops.list_directory(&path) {
        Ok(_) => {
          let safe_path = path.replace(['\n', '\r'], "");
          axum::http::Response::builder()
            .status(StatusCode::OK)
            .header("X-AeorDB-Type", "directory")
            .header("X-AeorDB-Path", safe_path)
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
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

  let (header, _key, value) = match state.engine.get_entry(&hash_bytes) {
    Ok(Some(entry)) => entry,
    Ok(None) => {
      return ErrorResponse::new(format!("Entry not found: {}", hex_hash)).with_status(StatusCode::NOT_FOUND).into_response();
    }
    Err(e) => {
      tracing::error!("Engine: failed to retrieve entry by hash '{}': {}", hex_hash, e);
      return ErrorResponse::new(format!("Failed to retrieve entry: {}", e)).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
  };

  // Block ALL access to system-flagged entries via API — system data is only
  // accessible through the internal system_store module, never through HTTP.
  if header.is_system_entry() {
    return ErrorResponse::new(format!("Entry not found: {}", hex_hash)).with_status(StatusCode::NOT_FOUND).into_response();
  }

  // Scoped-key check. ActiveKeyRules is only inserted by the permission
  // middleware when the key is scoped (rules non-empty). Root keys and
  // unscoped keys skip this entirely.
  if let Some(Extension(rules)) = active_key_rules.as_ref() {
    use crate::engine::api_key_rules::{match_rules, check_operation_permitted};
    match header.entry_type {
      EntryType::FileRecord => {
        let algo = state.engine.hash_algo();
        let hash_length = algo.hash_length();
        let path = match FileRecord::deserialize(&value, hash_length, header.entry_version) {
          Ok(r) => r.path,
          Err(_) => {
            return ErrorResponse::new(format!("Entry not found: {}", hex_hash)).with_status(StatusCode::NOT_FOUND).into_response();
          }
        };
        let allowed = match match_rules(&rules.0, &path) {
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

  match header.entry_type {
    EntryType::FileRecord => {
      // Deserialize the FileRecord and stream its chunk data, just like engine_get.
      let algo = state.engine.hash_algo();
      let hash_length = algo.hash_length();

      let file_record = match FileRecord::deserialize(&value, hash_length, header.entry_version) {
        Ok(r) => r,
        Err(e) => {
          tracing::error!("Engine: corrupt FileRecord at hash '{}': {}", hex_hash, e);
          return ErrorResponse::new(format!(
            "Corrupt or unreadable file record at hash '{}'. The entry may need to be re-uploaded — contact your administrator",
            hex_hash
          ))
          .with_status(StatusCode::INTERNAL_SERVER_ERROR)
          .into_response();
        }
      };

      build_file_streaming_response(
        &state.engine,
        &file_record,
        None,
        &headers,
        false,
        &[("X-AeorDB-Type", header.entry_type.to_u8().to_string()), ("X-AeorDB-Hash", hex_hash.clone())],
      )
    }

    EntryType::Chunk => {
      let data = match state.engine.read_chunk(&hash_bytes) {
        Ok(Some(data)) => data,
        Ok(None) => {
          return ErrorResponse::new(format!("Entry not found: {}", hex_hash)).with_status(StatusCode::NOT_FOUND).into_response();
        }
        Err(error) => {
          tracing::error!("Engine: failed to read chunk by hash '{}': {}", hex_hash, error);
          return ErrorResponse::new(format!("Failed to retrieve entry: {}", error))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
        }
      };
      state.engine.counters().record_read(data.len() as u64);

      axum::http::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .header("X-AeorDB-Type", header.entry_type.to_u8().to_string())
        .header("X-AeorDB-Hash", &hex_hash)
        .body(Body::from(data))
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response").into_response())
    }

    EntryType::DirectoryIndex => {
      state.engine.counters().record_read(value.len() as u64);
      axum::http::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .header("X-AeorDB-Type", header.entry_type.to_u8().to_string())
        .header("X-AeorDB-Hash", &hex_hash)
        .body(Body::from(value))
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response").into_response())
    }

    _ => {
      // Other types: return raw value bytes.
      axum::http::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .header("X-AeorDB-Type", header.entry_type.to_u8().to_string())
        .header("X-AeorDB-Hash", &hex_hash)
        .body(Body::from(value))
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

    let query_engine = QueryEngine::new(&state.engine);
    match query_engine.execute_explain(&query) {
      Ok(result) => {
        return (StatusCode::OK, Json(serde_json::to_value(&result).unwrap())).into_response();
      }
      Err(e) => {
        return ErrorResponse::new(format!("Explain failed: {}", e)).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response();
      }
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

    let query_engine = QueryEngine::new(&state.engine);
    match query_engine.execute_aggregate(&query) {
      Ok(result) => {
        let mut response_value = serde_json::to_value(&result).unwrap();
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

  let query_engine = QueryEngine::new(&state.engine);
  match query_engine.execute_paginated(&query) {
    Ok(paginated) => {
      let response_items: Vec<serde_json::Value> = paginated
        .results
        .iter()
        // Filter /.aeordb-system/ paths from query results — system data is invisible
        // through the API for all users, including root.
        .filter(|result| !is_system_path(&result.file_record.path))
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
        filter_results_by_direct_read(&mut response_items, &claims.sub, &state.engine, &state.group_cache);
      }

      let locator_options = LocatorOptions::from_request(&body.locators);
      if locator_options.include_matches {
        let locator_terms = if is_empty { Vec::new() } else { terms_from_query_node(&query_node) };
        enrich_query_items_with_locators(state.engine.as_ref(), &paginated.results, &mut response_items, &locator_terms, &locator_options);
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
        response["meta"] = serde_json::to_value(meta).unwrap();
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
  use crate::engine::merge_patch::{apply_merge_patch, MergeDepth};

  if is_system_path(&path) {
    return ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response();
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

  // Read existing → merge → write. Run on a blocking worker so we don't
  // hold an async runtime thread through the disk-bound parts.
  let result = tokio::task::spawn_blocking(move || -> EngineResult<(FileRecord, bool)> {
    let ops = DirectoryOps::new(&engine);

    // Read existing (if any). Missing file → start from empty object.
    let (mut target, existed) = match ops.read_file_buffered(&path_for_blocking) {
      Ok(bytes) => {
        if bytes.len() > MAX_MERGE_PATCH_BYTES {
          return Err(EngineError::InvalidInput(format!(
            "stored file at {} is {} bytes, exceeds {} byte merge cap",
            path_for_blocking,
            bytes.len(),
            MAX_MERGE_PATCH_BYTES
          )));
        }
        if bytes.is_empty() {
          (serde_json::Value::Object(serde_json::Map::new()), true)
        } else {
          let parsed: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| EngineError::InvalidInput(format!("stored file at {} is not valid JSON: {}", path_for_blocking, e)))?;
          (parsed, true)
        }
      }
      Err(EngineError::NotFound(_)) => (serde_json::Value::Object(serde_json::Map::new()), false),
      Err(e) => return Err(e),
    };

    apply_merge_patch(&mut target, patch_value, depth);

    let serialized =
      serde_json::to_vec(&target).map_err(|e| EngineError::InvalidInput(format!("merged document failed to serialize: {}", e)))?;
    let record = ops.store_file_buffered(&ctx, &path_for_blocking, &serialized, Some("application/json"))?;
    Ok((record, existed))
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

  evict_caches_for_path(&state, &path);

  let mut response_body = EngineFileResponse::from(&file_record);
  let algo = state.engine.hash_algo();
  let hash_length = algo.hash_length();
  if let Ok(file_value) = file_record.serialize(hash_length) {
    if let Ok(content_hash) = file_content_hash(&file_value, &algo) {
      response_body.hash = Some(hex::encode(&content_hash));
    }
  }

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

  // Block ALL access to /.aeordb-system/ via API — system data is only accessible
  // through the internal system_store module, never through HTTP endpoints.
  if is_system_path(&path) || is_system_path(destination) {
    return ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response();
  }

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let engine = state.engine.clone();
  let path_for_blocking = path.clone();
  let destination_owned = destination.to_string();

  let result = run_engine_blocking("rename", "Rename failed", move || -> EngineResult<&'static str> {
    let ops = DirectoryOps::new(&engine);
    if ops.get_symlink(&path_for_blocking).ok().flatten().is_some() {
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
      evict_caches_for_paths(&state, [path.as_str(), destination]);
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

  match state.engine.rebuild_kv() {
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

  if is_system_path(&dest_normalized) {
    return ErrorResponse::new("Not found").with_status(StatusCode::NOT_FOUND).into_response();
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
      if !permissions.has_any_path_permission(&normalized, &[CrudlifyOp::Read, CrudlifyOp::List]) {
        return ErrorResponse::new(format!("Not found: {}", raw_path)).with_status(StatusCode::NOT_FOUND).into_response();
      }
    }
    if !permissions.has_path_permission(&dest_normalized, CrudlifyOp::Create) {
      return ErrorResponse::new("Permission denied").with_status(StatusCode::FORBIDDEN).into_response();
    }
  }

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let engine = state.engine.clone();
  let paths = payload.paths.clone();
  let dest_for_blocking = dest_normalized.clone();

  // All copies run sequentially on a blocking thread; errors are collected
  // per-source rather than aborting on the first failure (matches prior behavior).
  let (copied, errors) = match tokio::task::spawn_blocking(move || {
    let ops = DirectoryOps::new(&engine);
    let mut copied = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for path in &paths {
      let from_normalized = crate::engine::path_utils::normalize_path(path);
      let name = crate::engine::path_utils::file_name(&from_normalized).unwrap_or("").to_string();
      let to_path = format!("{}/{}", dest_for_blocking.trim_end_matches('/'), name);
      match ops.copy_path(&ctx, &from_normalized, &to_path) {
        Ok(paths) => copied.extend(paths),
        Err(error) => errors.push(format!("{}: {}", from_normalized, error)),
      }
    }
    (copied, errors)
  })
  .await
  {
    Ok(pair) => pair,
    Err(join_error) => {
      tracing::error!("copy task panicked: {}", join_error);
      return ErrorResponse::new("Copy failed: internal task error").with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
  };

  let mut response = serde_json::json!({ "copied": copied });
  if !errors.is_empty() {
    response["errors"] = serde_json::json!(errors);
  }

  (StatusCode::OK, Json(response)).into_response()
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
  let limit = payload.limit.map(|l| l.min(1000));
  let offset = payload.offset;

  match crate::engine::search::global_search(&state.engine, base_path, payload.query.as_deref(), query_node.as_ref(), limit, offset) {
    Ok(results) => {
      let mut items: Vec<serde_json::Value> = results
        .results
        .iter()
        .filter(|r| !is_system_path(&r.path))
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

      // Filter search results by user/group permissions. Search is exempt
      // from path-level middleware, so authorization happens here: a user
      // only sees files they have direct Read on (grants + inheritance).
      if !claims.sub.starts_with("share:") {
        filter_results_by_direct_read(&mut items, &claims.sub, &state.engine, &state.group_cache);
      }

      let locator_options = LocatorOptions::from_request(&payload.locators);
      if locator_options.include_matches {
        enrich_search_items_with_locators(
          state.engine.as_ref(),
          &results.results,
          &mut items,
          payload.query.as_deref(),
          query_node.as_ref(),
          &locator_options,
        );
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
    Err(error) => {
      tracing::error!("Global search failed: {}", error);
      ErrorResponse::new(format!("Search failed: {}", error)).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response()
    }
  }
}
