use axum::{
  Extension,
  body::Body,
  extract::{Path, Query as AxumQuery, State},
  http::{HeaderMap, StatusCode},
  response::{IntoResponse, Response},
  Json,
};
use futures_util::{stream, StreamExt};
use serde::Deserialize;

use uuid::Uuid;
use super::responses::{engine_error_response, EngineFileResponse, ErrorResponse};
use super::state::AppState;
use crate::auth::TokenClaims;
use crate::auth::permission_middleware::ActiveKeyRules;
use crate::engine::api_key_rules::{match_rules, check_operation_permitted};
use crate::engine::{DirectoryOps, RequestContext, TaskStatus, VersionManager, is_root};
use crate::engine::directory_listing::list_directory_recursive;
use crate::engine::compression::{CompressionAlgorithm, decompress};
use crate::engine::directory_ops::{is_system_path, EngineFileStream, file_content_hash};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::query_engine::{QueryEngine, QueryMeta, Query, QueryNode, FieldQuery, QueryOp, QueryStrategy, FuzzyOptions, Fuzziness, FuzzyAlgorithm, SortField, SortDirection, DEFAULT_QUERY_LIMIT, AggregateQuery, ExplainMode};
use crate::engine::symlink_resolver::{resolve_symlink, ResolvedTarget};

/// Check if a file path is deleted and the user lacks delete permission.
/// Deleted files are invisible/inaccessible to users without 'd' permission.
fn is_deleted_and_forbidden(state: &AppState, claims: &TokenClaims, path: &str) -> bool {
    use crate::engine::permission_resolver::{CrudlifyOp, PermissionResolver};
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
    let resolver = PermissionResolver::new(&state.engine, &state.group_cache);
    let has_delete = resolver.check_permission(&user_id, &normalized, CrudlifyOp::Delete).unwrap_or(false);

    !has_delete
}

/// Evict cache entries when a system file is written, deleted, or renamed.
fn evict_caches_for_path(state: &AppState, path: &str) {
    let normalized = crate::engine::path_utils::normalize_path(path);

    if normalized.ends_with("/.aeordb-permissions") || normalized == "/.aeordb-permissions" {
        let parent = crate::engine::path_utils::parent_path(&normalized)
            .unwrap_or_else(|| "/".to_string());
        state.engine.permissions_cache.evict(&parent);
    }

    if normalized.ends_with("/.aeordb-config/indexes.json") {
        if let Some(dir) = normalized.strip_suffix("/.aeordb-config/indexes.json") {
            let key = if dir.is_empty() { "/".to_string() } else { dir.to_string() };
            state.engine.index_config_cache.evict(&key);
        }
    }

    if normalized.starts_with("/.aeordb-system/api-keys/") {
        if let Some(key_id) = crate::engine::path_utils::file_name(&normalized) {
            state.api_key_cache.evict(&key_id.to_string());
        }
    }

    if normalized.starts_with("/.aeordb-system/groups/")
        || normalized.starts_with("/.aeordb-system/users/")
    {
        state.group_cache.evict_all();
    }
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
                        obj.insert(
                            "effective_permissions".to_string(),
                            serde_json::Value::String(rule.permitted.clone()),
                        );
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
                        obj.insert(
                            "effective_permissions".to_string(),
                            serde_json::Value::String("-r--l---".to_string()),
                        );
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
    if descending { cmp.reverse() } else { cmp }
  });

  let total = listing.len();
  let off = offset.unwrap_or(0).min(total);
  listing = listing.split_off(off);
  if let Some(lim) = limit {
    listing.truncate(lim);
  }
  (StatusCode::OK, Json(serde_json::json!({
    "items": listing,
    "total": total,
    "limit": limit,
    "offset": off,
  }))).into_response()
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

pub async fn mkdir(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Json(body): Json<MkdirRequest>,
) -> Response {
  let normalized = crate::engine::path_utils::normalize_path(&body.path);

  if is_system_path(&normalized) {
    return ErrorResponse::new(format!("Not found: {}", body.path))
      .with_status(StatusCode::NOT_FOUND)
      .into_response();
  }

  if normalized == "/" {
    return ErrorResponse::new("Cannot create root directory")
      .with_status(StatusCode::BAD_REQUEST)
      .into_response();
  }

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());

  let engine = state.engine.clone();
  let normalized_for_blocking = normalized.clone();
  let result = tokio::task::spawn_blocking(move || {
    let ops = DirectoryOps::new(&engine);
    ops.create_directory(&ctx, &normalized_for_blocking)
  })
  .await;

  match result {
    Ok(Ok(())) => (StatusCode::CREATED, Json(serde_json::json!({
      "path": normalized,
      "entry_type": 3,
      "created": true,
    }))).into_response(),
    Ok(Err(error)) => {
      tracing::error!("Failed to create directory '{}': {}", normalized, error);
      engine_error_response("Failed to create directory", &error)
    }
    Err(join_error) => {
      tracing::error!("create_directory task panicked: {}", join_error);
      ErrorResponse::new("Failed to create directory: internal task error")
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
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
    return ErrorResponse::new(format!("Not found: {}", path))
      .with_status(StatusCode::NOT_FOUND)
      .into_response();
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
                return ErrorResponse::new("Failed to store upload chunk")
                  .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                  .into_response();
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
        return ErrorResponse::new("Failed to store upload chunk")
          .with_status(StatusCode::INTERNAL_SERVER_ERROR)
          .into_response();
      }
    }
  }

  let content_type = headers
    .get("content-type")
    .and_then(|value| value.to_str().ok())
    .map(|s| s.to_string());

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());

  // Move the fsync-heavy finalize off the async runtime so we don't block other
  // requests sharing this worker thread while we wait for disk.
  let engine_for_blocking = state.engine.clone();
  let path_for_blocking = path.clone();
  let ctx_for_blocking = ctx.clone();
  let first_bytes_owned = first_bytes;
  let chunk_hashes_owned = chunk_hashes;
  let file_record = match tokio::task::spawn_blocking(move || {
    let ops = DirectoryOps::new(&engine_for_blocking);
    ops.finalize_file(
      &ctx_for_blocking,
      &path_for_blocking,
      chunk_hashes_owned,
      total_size,
      content_type.as_deref(),
      &first_bytes_owned,
    )
  })
  .await
  {
    Ok(Ok(record)) => record,
    Ok(Err(error)) => {
      tracing::error!("Engine: failed to store file at '{}': {}", path, error);
      return engine_error_response("Failed to store file", &error);
    }
    Err(join_error) => {
      tracing::error!("Engine: finalize_file task panicked: {}", join_error);
      return ErrorResponse::new("Failed to store file: internal task error")
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  // Auto-trigger reindex when indexes.json is stored
  if path.ends_with("/.aeordb-config/indexes.json") || path.ends_with(".config/indexes.json") {
    if let Some(ref queue) = state.task_queue {
      let parent = path.trim_end_matches("/.aeordb-config/indexes.json")
        .trim_end_matches(".config/indexes.json");
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

      // Enqueue new reindex
      let _ = queue.enqueue("reindex", serde_json::json!({"path": reindex_path}));
    }
  }

  let mut response_body = EngineFileResponse::from(&file_record);

  // Compute the content-addressed hash so the caller can fetch by hash.
  let algo = state.engine.hash_algo();
  let hash_length = algo.hash_length();
  let file_value = match file_record.serialize(hash_length) {
    Ok(v) => v,
    Err(_) => return ErrorResponse::new("Failed to serialize file record after storing. The file was saved but the response could not be built — contact your administrator".to_string())
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
fn build_file_streaming_response(
  engine: &std::sync::Arc<crate::engine::StorageEngine>,
  file_record: &FileRecord,
  symlink_target: Option<&str>,
) -> Response {
  let file_stream = match EngineFileStream::from_chunk_hashes(
    file_record.chunk_hashes.clone(), engine,
  ) {
    Ok(s) => s,
    Err(error) => {
      tracing::error!("Engine: failed to stream file '{}': {}", file_record.path, error);
      return ErrorResponse::new(format!("Failed to stream file '{}': the file data may be corrupted. Contact your administrator", file_record.path))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  let chunk_stream = stream::iter(file_stream.map(|chunk_result| {
    chunk_result
      .map(axum::body::Bytes::from)
      .map_err(|error| std::io::Error::other(error.to_string()))
  }));

  let body = Body::from_stream(chunk_stream);

  let safe_path = file_record.path.replace(['\n', '\r'], "");
  let mut response_builder = axum::http::Response::builder()
    .status(StatusCode::OK)
    .header("X-AeorDB-Path", safe_path)
    .header("X-AeorDB-Size", file_record.total_size.to_string())
    .header("X-AeorDB-Created", file_record.created_at.to_string())
    .header("X-AeorDB-Updated", file_record.updated_at.to_string());

  if let Some(target) = symlink_target {
    response_builder = response_builder
      .header("X-AeorDB-Link-Target", target.replace(['\n', '\r'], ""));
  }

  if let Some(ref content_type) = file_record.content_type {
    response_builder = response_builder.header("content-type", content_type.as_str());
  }

  response_builder
    .body(body)
    .unwrap_or_else(|_| {
      (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response").into_response()
    })
}

/// Convert a flat directory listing (ChildEntry vec) to JSON values.
///
/// Each entry is enriched with its full path and, for symlink entries,
/// the symlink target is included.
fn build_directory_listing(
  entries: &[crate::engine::ChildEntry],
  base_path: &str,
  directory_ops: &DirectoryOps,
) -> Vec<serde_json::Value> {
  let normalized = crate::engine::path_utils::normalize_path(base_path);
  entries
    .iter()
    .map(|child| {
      let child_path = if normalized == "/" {
        format!("/{}", child.name)
      } else {
        format!("{}/{}", normalized, child.name)
      };
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
fn apply_listing_filters(
  listing: &mut Vec<serde_json::Value>,
  key_rules: Option<&[crate::engine::api_key_rules::KeyRule]>,
  _user_id_str: &str,
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

  if crate::engine::is_root(user_id) { return; }

  let resolver = PermissionResolver::new(engine, group_cache);
  let ops = [
    ('c', CrudlifyOp::Create), ('r', CrudlifyOp::Read), ('u', CrudlifyOp::Update),
    ('d', CrudlifyOp::Delete), ('l', CrudlifyOp::List), ('i', CrudlifyOp::Invoke),
    ('f', CrudlifyOp::Deploy), ('y', CrudlifyOp::Configure),
  ];

  for entry in listing.iter_mut() {
    // Skip items that already have effective_permissions (set by key rules filter)
    if entry.get("effective_permissions").is_some() { continue; }

    let path = match entry["path"].as_str() {
      Some(p) => p.to_string(),
      None => continue,
    };

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
  user_id_str: &str,
  key_rules: Option<&[crate::engine::api_key_rules::KeyRule]>,
  limit: Option<usize>,
  offset: Option<usize>,
) -> Response {
  let directory_ops = DirectoryOps::new(engine);

  match resolve_symlink(engine, path) {
    Ok(ResolvedTarget::File(ref file_record)) => {
      // Block ALL access to symlinks resolving to /.aeordb-system/ paths — system
      // data is invisible through the API for all users, including root.
      if is_system_path(&file_record.path) {
        return ErrorResponse::new(format!("Not found: {}", path))
          .with_status(StatusCode::NOT_FOUND)
          .into_response();
      }

      // Check if the resolved target path is allowed by API key rules
      if let Some(rules) = key_rules {
        if !rules.is_empty() {
          let target_path = &file_record.path;
          let normalized_target = if target_path.starts_with('/') {
            target_path.to_string()
          } else {
            format!("/{}", target_path)
          };
          match match_rules(rules, &normalized_target) {
            Some(rule) => {
              if !check_operation_permitted(&rule.permitted, 'r') {
                return ErrorResponse::new(format!("Not found: {}", path))
                  .with_status(StatusCode::NOT_FOUND)
                  .into_response();
              }
            }
            None => {
              return ErrorResponse::new(format!("Not found: {}", path))
                .with_status(StatusCode::NOT_FOUND)
                .into_response();
            }
          }
        }
      }

      build_file_streaming_response(engine, file_record, Some(symlink_target))
    }
    Ok(ResolvedTarget::Directory(dir_path)) => {
      // Block ALL access to symlinks resolving to /.aeordb-system/ directories —
      // system data is invisible through the API for all users, including root.
      if is_system_path(&dir_path) {
        return ErrorResponse::new(format!("Not found: {}", path))
          .with_status(StatusCode::NOT_FOUND)
          .into_response();
      }

      match directory_ops.list_directory(&dir_path) {
        Ok(entries) => {
          let mut listing = build_directory_listing(&entries, &dir_path, &directory_ops);
          match apply_listing_filters(&mut listing, key_rules, user_id_str) {
            Ok(()) => paginated_listing_response(listing, limit, offset, None, None),
            Err(response) => response,
          }
        }
        Err(error) => {
          tracing::error!("Engine: failed to list resolved directory: {}", error);
          ErrorResponse::new(format!("Failed to list directory after resolving symlink '{}'. If this persists, check GET /system/health for system status", path))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response()
        }
      }
    }
    Err(EngineError::NotFound(msg)) => {
      ErrorResponse::new(format!("Dangling symlink: {}", msg))
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Err(EngineError::CyclicSymlink(msg)) => {
      ErrorResponse::new(format!("Symlink cycle detected: {}", msg))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response()
    }
    Err(EngineError::SymlinkDepthExceeded(msg)) => {
      ErrorResponse::new(msg)
        .with_status(StatusCode::BAD_REQUEST)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to resolve symlink '{}': {}", path, error);
      ErrorResponse::new(format!("Failed to resolve symlink '{}'. The symlink or its target may be corrupted — contact your administrator", path))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// Handle a direct file read: stream the file content as an HTTP response.
fn handle_file_response(
  engine: &std::sync::Arc<crate::engine::StorageEngine>,
  path: &str,
) -> Response {
  let directory_ops = DirectoryOps::new(engine);

  let file_record = match directory_ops.get_metadata(path) {
    Ok(Some(record)) => record,
    Ok(None) => {
      return ErrorResponse::new(format!("Not found: {}", path))
        .with_status(StatusCode::NOT_FOUND)
        .into_response();
    }
    Err(error) => {
      tracing::error!("Engine: failed to get metadata for '{}': {}", path, error);
      return ErrorResponse::new(format!("Failed to read metadata for '{}'. The file may be corrupted — contact your administrator", path))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  // Use read_file_streaming for direct file reads (reads via path, not chunk hashes)
  let file_stream = match directory_ops.read_file_streaming(path) {
    Ok(s) => s,
    Err(error) => {
      tracing::error!("Engine: failed to read file '{}': {}", path, error);
      return ErrorResponse::new(format!("Failed to read file '{}'. The file data may be corrupted — contact your administrator", path))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  let chunk_stream = stream::iter(file_stream.map(|chunk_result| {
    chunk_result
      .map(axum::body::Bytes::from)
      .map_err(|error| std::io::Error::other(error.to_string()))
  }));

  let body = Body::from_stream(chunk_stream);

  let safe_path = file_record.path.replace(['\n', '\r'], "");
  let mut response_builder = axum::http::Response::builder()
    .status(StatusCode::OK)
    .header("X-AeorDB-Path", safe_path)
    .header("X-AeorDB-Size", file_record.total_size.to_string())
    .header("X-AeorDB-Created", file_record.created_at.to_string())
    .header("X-AeorDB-Updated", file_record.updated_at.to_string());

  if let Some(ref content_type) = file_record.content_type {
    response_builder = response_builder.header("content-type", content_type.as_str());
  }

  response_builder
    .body(body)
    .unwrap_or_else(|_| {
      (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response").into_response()
    })
}

/// Handle recursive directory listing with depth and/or glob parameters.
fn handle_recursive_listing(
  engine: &std::sync::Arc<crate::engine::StorageEngine>,
  path: &str,
  version_query: &EngineGetQuery,
  key_rules: Option<&[crate::engine::api_key_rules::KeyRule]>,
  user_id_str: &str,
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

      match apply_listing_filters(&mut listing, key_rules, user_id_str) {
        Ok(()) => paginated_listing_response(listing, version_query.limit, version_query.offset, version_query.sort.as_deref(), version_query.order.as_deref()),
        Err(response) => response,
      }
    }
    Err(EngineError::NotFound(_)) => {
      ErrorResponse::new(format!("Not found: {}", path))
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to list directory '{}': {}", path, error);
      ErrorResponse::new(format!("Failed to list directory '{}' with recursive traversal. If this persists, check GET /system/health for system status", path))
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
) -> Response {
  let ListingPagination { limit, offset, sort, order } = pagination;
  let directory_ops = DirectoryOps::new(engine);

  match directory_ops.list_directory(path) {
    Ok(entries) => {
      let mut listing = build_directory_listing(&entries, path, &directory_ops);
      match apply_listing_filters(&mut listing, key_rules, user_id_str) {
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
    Err(EngineError::NotFound(_)) => {
      ErrorResponse::new(format!("Not found: {}", path))
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
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
  AxumQuery(version_query): AxumQuery<EngineGetQuery>,
) -> Response {
  engine_get(State(state), Extension(claims), active_key_rules, Path("/".to_string()), AxumQuery(version_query)).await
}

pub async fn engine_get(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  active_key_rules: Option<Extension<ActiveKeyRules>>,
  Path(path): Path<String>,
  AxumQuery(version_query): AxumQuery<EngineGetQuery>,
) -> Response {
  // Block ALL access to /.aeordb-system/ via API — system data is only accessible
  // through the internal system_store module, never through HTTP endpoints.
  if is_system_path(&path) {
    return ErrorResponse::new(format!("Not found: {}", path))
      .with_status(StatusCode::NOT_FOUND)
      .into_response();
  }

  // Deleted files are invisible to users without 'd' permission
  if is_deleted_and_forbidden(&state, &claims, &path) {
    return ErrorResponse::new(format!("Not found: {}", path))
      .with_status(StatusCode::NOT_FOUND)
      .into_response();
  }

  // If snapshot or version query param is present, read from historical version
  if version_query.snapshot.is_some() || version_query.version.is_some() {
    return engine_get_at_version(&state, &path, &version_query).await;
  }

  // Extract key rules slice for helpers (avoids passing axum Extension around)
  let key_rules: Option<&[crate::engine::api_key_rules::KeyRule]> =
    active_key_rules.as_ref().map(|Extension(rules)| rules.0.as_slice());

  let directory_ops = DirectoryOps::new(&state.engine);

  // Check for symlink first
  if let Ok(Some(symlink_record)) = directory_ops.get_symlink(&path) {
    // nofollow: return symlink metadata without resolving
    if version_query.nofollow == Some(true) {
      return (StatusCode::OK, Json(serde_json::json!({
        "path": symlink_record.path,
        "target": symlink_record.target,
        "entry_type": 8,
        "created_at": symlink_record.created_at,
        "updated_at": symlink_record.updated_at,
      }))).into_response();
    }

    return handle_symlink_resolution(
      &state.engine, &path, &symlink_record.target, &claims.sub, key_rules,
      version_query.limit, version_query.offset,
    );
  }

  // Try as file first
  match directory_ops.get_metadata(&path) {
    Ok(Some(_file_record)) => {
      return handle_file_response(&state.engine, &path);
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
    return handle_recursive_listing(
      &state.engine, &path, &version_query, key_rules, &claims.sub,
    );
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
  )
}


/// Read a file at a historical version (snapshot or explicit root hash).
async fn engine_get_at_version(
  state: &AppState,
  path: &str,
  version_query: &EngineGetQuery,
) -> Response {
  let vm = VersionManager::new(&state.engine);

  // Resolve root hash: snapshot takes precedence
  let root_hash = if let Some(ref snapshot_name) = version_query.snapshot {
    match vm.resolve_root_hash(Some(snapshot_name)) {
      Ok(hash) => hash,
      Err(_) => {
        return ErrorResponse::new(format!("Snapshot '{}' not found", snapshot_name))
          .with_status(StatusCode::NOT_FOUND)
          .into_response();
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
    return ErrorResponse::new("No snapshot or version specified. Use ?snapshot=<name> or ?version=<hex_hash> to read a historical version")
      .with_status(StatusCode::BAD_REQUEST)
      .into_response();
  };

  // Resolve the file at this version
  let (_file_hash, file_record) = match crate::engine::version_access::resolve_file_at_version(
    &state.engine, &root_hash, path,
  ) {
    Ok(result) => result,
    Err(crate::engine::errors::EngineError::NotFound(msg)) => {
      return ErrorResponse::new(msg)
        .with_status(StatusCode::NOT_FOUND)
        .into_response();
    }
    Err(error) => {
      tracing::error!("Engine: failed to read file '{}' at version: {}", path, error);
      return ErrorResponse::new(format!("Failed to read file '{}' at historical version. If this persists, check GET /system/health for system status", path))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  // Stream the file content from historical chunks (include deleted —
  // chunks may have been marked deleted after the snapshot was taken)
  let file_stream = match EngineFileStream::from_chunk_hashes_including_deleted(
    file_record.chunk_hashes.clone(), &state.engine,
  ) {
    Ok(stream) => stream,
    Err(error) => {
      tracing::error!("Engine: failed to stream file '{}' at version: {}", path, error);
      return ErrorResponse::new(format!("Failed to stream file '{}' from historical version. The file data may be corrupted — contact your administrator", path))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  let chunk_stream = stream::iter(file_stream.map(|chunk_result| {
    chunk_result
      .map(axum::body::Bytes::from)
      .map_err(|error| std::io::Error::other(error.to_string()))
  }));

  let body = Body::from_stream(chunk_stream);

  let mut response_builder = axum::http::Response::builder()
    .status(StatusCode::OK)
    .header("X-AeorDB-Path", path.replace(['\n', '\r'], ""))
    .header("X-AeorDB-Size", file_record.total_size.to_string())
    .header("X-AeorDB-Created", file_record.created_at.to_string())
    .header("X-AeorDB-Updated", file_record.updated_at.to_string());

  if let Some(ref content_type) = file_record.content_type {
    response_builder = response_builder.header("content-type", content_type.as_str());
  }

  response_builder
    .body(body)
    .unwrap_or_else(|_| {
      (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response").into_response()
    })
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
    return ErrorResponse::new(format!("Not found: {}", path))
      .with_status(StatusCode::NOT_FOUND)
      .into_response();
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
      ErrorResponse::new(format!("Not found: {}", path))
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Ok(Err(error)) => {
      tracing::error!("Engine: failed to delete '{}': {}", path, error);
      engine_error_response("Failed to delete", &error)
    }
    Err(join_error) => {
      tracing::error!("delete task panicked: {}", join_error);
      ErrorResponse::new("Failed to delete: internal task error")
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
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
      return ErrorResponse::new("Missing 'path' field")
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  };

  if is_system_path(&path) {
    return ErrorResponse::new(format!("Not found: {}", path))
      .with_status(StatusCode::NOT_FOUND)
      .into_response();
  }

  let ctx = crate::engine::RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let ops = DirectoryOps::new(&state.engine);

  match ops.restore_deleted_file(&ctx, &path) {
    Ok(()) => {
      (StatusCode::OK, Json(serde_json::json!({
        "restored": true,
        "path": path,
      }))).into_response()
    }
    Err(e) => {
      ErrorResponse::new(format!("Restore failed: {}", e))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
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
    return ErrorResponse::new(format!("Not found: {}", dir_path))
      .with_status(StatusCode::NOT_FOUND)
      .into_response();
  }

  // Deleted files require 'd' permission — check on the directory
  {
    use crate::engine::permission_resolver::{CrudlifyOp, PermissionResolver};
    let user_id = match Uuid::parse_str(&claims.sub) {
      Ok(id) => id,
      Err(_) => {
        return ErrorResponse::new("Invalid user ID")
          .with_status(StatusCode::FORBIDDEN)
          .into_response();
      }
    };
    if !is_root(&user_id) {
      let resolver = PermissionResolver::new(&state.engine, &state.group_cache);
      let has_delete = resolver.check_permission(&user_id, dir_path, CrudlifyOp::Delete).unwrap_or(false);
      if !has_delete {
        return (StatusCode::OK, Json(serde_json::json!({
          "items": [],
          "total": 0,
        }))).into_response();
      }
    }
  }

  let ops = DirectoryOps::new(&state.engine);

  match ops.list_deleted(dir_path) {
    Ok(records) => {
      let items: Vec<serde_json::Value> = records.iter().map(|r| {
        let name = crate::engine::path_utils::file_name(&r.path).unwrap_or("").to_string();
        serde_json::json!({
          "path": r.path,
          "name": name,
          "deleted_at": r.deleted_at,
          "reason": r.reason,
        })
      }).collect();
      (StatusCode::OK, Json(serde_json::json!({
        "items": items,
        "total": items.len(),
      }))).into_response()
    }
    Err(e) => {
      ErrorResponse::new(format!("Failed to list deleted files: {}", e))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

pub async fn engine_head(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(path): Path<String>,
) -> Response {
  if is_system_path(&path) {
    return ErrorResponse::new(format!("Not found: {}", path))
      .with_status(StatusCode::NOT_FOUND)
      .into_response();
  }

  // Deleted files are invisible to users without 'd' permission
  if is_deleted_and_forbidden(&state, &claims, &path) {
    return ErrorResponse::new(format!("Not found: {}", path))
      .with_status(StatusCode::NOT_FOUND)
      .into_response();
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

      response_builder
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
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
      return ErrorResponse::new(format!("Entry not found: {}", hex_hash))
        .with_status(StatusCode::NOT_FOUND)
        .into_response();
    }
    Err(e) => {
      tracing::error!("Engine: failed to retrieve entry by hash '{}': {}", hex_hash, e);
      return ErrorResponse::new(format!("Failed to retrieve entry: {}", e))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  // Block ALL access to system-flagged entries via API — system data is only
  // accessible through the internal system_store module, never through HTTP.
  if header.is_system_entry() {
    return ErrorResponse::new(format!("Entry not found: {}", hex_hash))
      .with_status(StatusCode::NOT_FOUND)
      .into_response();
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
            return ErrorResponse::new(format!("Entry not found: {}", hex_hash))
              .with_status(StatusCode::NOT_FOUND)
              .into_response();
          }
        };
        let allowed = match match_rules(&rules.0, &path) {
          Some(rule) => check_operation_permitted(&rule.permitted, 'r'),
          None => false,
        };
        if !allowed {
          // Use 404 (not 403) so scoped keys cannot enumerate forbidden
          // paths by probing hashes.
          return ErrorResponse::new(format!("Entry not found: {}", hex_hash))
            .with_status(StatusCode::NOT_FOUND)
            .into_response();
        }
      }
      // For raw chunks and other non-path entries, we can't tie the hash
      // back to a path the scoped key is permitted to access. Deny.
      _ => {
        return ErrorResponse::new(format!("Entry not found: {}", hex_hash))
          .with_status(StatusCode::NOT_FOUND)
          .into_response();
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
          return ErrorResponse::new(format!("Corrupt or unreadable file record at hash '{}'. The entry may need to be re-uploaded — contact your administrator", hex_hash))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
        }
      };

      let file_stream = match EngineFileStream::from_chunk_hashes(file_record.chunk_hashes, &state.engine) {
        Ok(s) => s,
        Err(e) => {
          tracing::error!("Engine: failed to read chunks for hash '{}': {}", hex_hash, e);
          return ErrorResponse::new(format!("Failed to read file chunks for hash '{}'. The file data may be corrupted — contact your administrator", hex_hash))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
        }
      };

      let chunk_stream = stream::iter(file_stream.map(|chunk_result| {
        chunk_result
          .map(axum::body::Bytes::from)
          .map_err(|error| std::io::Error::other(error.to_string()))
      }));

      let body = Body::from_stream(chunk_stream);

      let mut response_builder = axum::http::Response::builder()
        .status(StatusCode::OK)
        .header("X-AeorDB-Type", header.entry_type.to_u8().to_string())
        .header("X-AeorDB-Hash", &hex_hash)
        .header("X-AeorDB-Size", file_record.total_size.to_string());

      if let Some(ref ct) = file_record.content_type {
        response_builder = response_builder.header("content-type", ct.as_str());
      }

      response_builder
        .body(body)
        .unwrap_or_else(|_| {
          (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response").into_response()
        })
    }

    EntryType::Chunk => {
      // Decompress if needed and return raw chunk bytes.
      let data = if header.compression_algo != CompressionAlgorithm::None {
        match decompress(&value, header.compression_algo) {
          Ok(decompressed) => decompressed,
          Err(_) => value,
        }
      } else {
        value
      };

      axum::http::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .header("X-AeorDB-Type", header.entry_type.to_u8().to_string())
        .header("X-AeorDB-Hash", &hex_hash)
        .body(Body::from(data))
        .unwrap_or_else(|_| {
          (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response").into_response()
        })
    }

    EntryType::DirectoryIndex => {
      axum::http::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .header("X-AeorDB-Type", header.entry_type.to_u8().to_string())
        .header("X-AeorDB-Hash", &hex_hash)
        .body(Body::from(value))
        .unwrap_or_else(|_| {
          (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response").into_response()
        })
    }

    _ => {
      // Other types: return raw value bytes.
      axum::http::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .header("X-AeorDB-Type", header.entry_type.to_u8().to_string())
        .header("X-AeorDB-Hash", &hex_hash)
        .body(Body::from(value))
        .unwrap_or_else(|_| {
          (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response").into_response()
        })
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

/// Convert a JSON value to the byte representation used by converters.
/// Numbers -> u64 big-endian bytes.
/// Strings -> UTF-8 bytes.
/// Booleans -> single byte 0 or 1.
fn json_value_to_bytes(value: &serde_json::Value) -> Result<Vec<u8>, String> {
  match value {
    serde_json::Value::Number(number) => {
      if let Some(unsigned) = number.as_u64() {
        Ok(unsigned.to_be_bytes().to_vec())
      } else if let Some(signed) = number.as_i64() {
        Ok((signed as u64).to_be_bytes().to_vec())
      } else if let Some(float) = number.as_f64() {
        Ok((float as u64).to_be_bytes().to_vec())
      } else {
        Err("Unsupported number format".to_string())
      }
    }
    serde_json::Value::String(text) => Ok(text.as_bytes().to_vec()),
    serde_json::Value::Bool(flag) => Ok(vec![if *flag { 1 } else { 0 }]),
    other => Err(format!("Unsupported value type: {}", other)),
  }
}

/// Parse a single field-level where clause JSON object into a QueryNode::Field.
fn parse_single_field_query(value: &serde_json::Value) -> Result<QueryNode, String> {
  let field = value.get("field")
    .and_then(|v| v.as_str())
    .ok_or_else(|| "Missing 'field' in where clause".to_string())?;
  let op = value.get("op")
    .and_then(|v| v.as_str())
    .ok_or_else(|| format!("Missing 'op' in where clause for field '{}'", field))?;
  let raw_value = value.get("value")
    .ok_or_else(|| format!("Missing 'value' in where clause for field '{}'", field))?;

  let operation = match op {
    "eq" => {
      let bytes = json_value_to_bytes(raw_value)
        .map_err(|message| format!("Invalid value for field '{}': {}", field, message))?;
      QueryOp::Eq(bytes)
    }
    "gt" => {
      let bytes = json_value_to_bytes(raw_value)
        .map_err(|message| format!("Invalid value for field '{}': {}", field, message))?;
      QueryOp::Gt(bytes)
    }
    "lt" => {
      let bytes = json_value_to_bytes(raw_value)
        .map_err(|message| format!("Invalid value for field '{}': {}", field, message))?;
      QueryOp::Lt(bytes)
    }
    "between" => {
      let bytes = json_value_to_bytes(raw_value)
        .map_err(|message| format!("Invalid value for field '{}': {}", field, message))?;
      let raw_value2 = value.get("value2")
        .ok_or_else(|| format!("Missing value2 for 'between' operation on field '{}'", field))?;
      let bytes2 = json_value_to_bytes(raw_value2)
        .map_err(|message| format!("Invalid value2 for field '{}': {}", field, message))?;
      QueryOp::Between(bytes, bytes2)
    }
    "in" => {
      let array = raw_value.as_array()
        .ok_or_else(|| format!("'in' operation requires array value for field '{}'", field))?;
      let mut byte_values = Vec::with_capacity(array.len());
      for item in array {
        let bytes = json_value_to_bytes(item)
          .map_err(|message| format!("Invalid value in 'in' array for field '{}': {}", field, message))?;
        byte_values.push(bytes);
      }
      QueryOp::In(byte_values)
    }
    "contains" => {
      let s = raw_value.as_str()
        .ok_or_else(|| format!("'contains' requires string value for field '{}'", field))?;
      QueryOp::Contains(s.to_string())
    }
    "similar" => {
      let s = raw_value.as_str()
        .ok_or_else(|| format!("'similar' requires string value for field '{}'", field))?;
      let threshold = value.get("threshold")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.3);
      QueryOp::Similar(s.to_string(), threshold)
    }
    "phonetic" => {
      let s = raw_value.as_str()
        .ok_or_else(|| format!("'phonetic' requires string value for field '{}'", field))?;
      QueryOp::Phonetic(s.to_string())
    }
    "fuzzy" => {
      let s = raw_value.as_str()
        .ok_or_else(|| format!("'fuzzy' requires string value for field '{}'", field))?;

      let fuzziness = match value.get("fuzziness") {
        Some(v) if v.is_string() && v.as_str() == Some("auto") => Fuzziness::Auto,
        Some(v) if v.is_u64() => Fuzziness::Fixed(v.as_u64().unwrap() as usize),
        Some(v) if v.is_i64() => Fuzziness::Fixed(v.as_i64().unwrap().max(0) as usize),
        _ => Fuzziness::Auto,
      };

      let algorithm = match value.get("algorithm").and_then(|v| v.as_str()) {
        Some("jaro_winkler") => FuzzyAlgorithm::JaroWinkler,
        _ => FuzzyAlgorithm::DamerauLevenshtein,
      };

      QueryOp::Fuzzy(s.to_string(), FuzzyOptions { fuzziness, algorithm })
    }
    "match" => {
      let s = raw_value.as_str()
        .ok_or_else(|| format!("'match' requires string value for field '{}'", field))?;
      QueryOp::Match(s.to_string())
    }
    unknown => {
      return Err(format!("Unknown operation: '{}'", unknown));
    }
  };

  Ok(QueryNode::Field(FieldQuery {
    field_name: field.to_string(),
    operation,
  }))
}

/// Recursively parse a where clause JSON value into a QueryNode tree.
/// Supports:
///   - Array: legacy format, sugar for AND of field clauses
///   - Object with "and": AND of child clauses
///   - Object with "or": OR of child clauses
///   - Object with "not": NOT of a single child clause
///   - Object with "field": leaf field query
/// Maximum allowed nesting depth for where-clause parsing.
/// Prevents stack overflow from adversarial deeply-nested queries.
const MAX_WHERE_CLAUSE_DEPTH: usize = 32;

fn parse_where_clause(value: &serde_json::Value) -> Result<QueryNode, String> {
  parse_where_clause_inner(value, 0)
}

fn parse_where_clause_inner(value: &serde_json::Value, depth: usize) -> Result<QueryNode, String> {
  if depth > MAX_WHERE_CLAUSE_DEPTH {
    return Err(format!(
      "Query nesting too deep (max {} levels). Simplify the where clause",
      MAX_WHERE_CLAUSE_DEPTH,
    ));
  }

  if value.is_array() {
    let array = value.as_array().unwrap();
    let children: Result<Vec<QueryNode>, String> = array.iter()
      .map(|v| parse_where_clause_inner(v, depth + 1))
      .collect();
    return Ok(QueryNode::And(children?));
  }

  if let Some(and_array) = value.get("and") {
    let array = and_array.as_array()
      .ok_or_else(|| "'and' must be an array".to_string())?;
    let children: Result<Vec<QueryNode>, String> = array.iter()
      .map(|v| parse_where_clause_inner(v, depth + 1))
      .collect();
    return Ok(QueryNode::And(children?));
  }

  if let Some(or_array) = value.get("or") {
    let array = or_array.as_array()
      .ok_or_else(|| "'or' must be an array".to_string())?;
    let children: Result<Vec<QueryNode>, String> = array.iter()
      .map(|v| parse_where_clause_inner(v, depth + 1))
      .collect();
    return Ok(QueryNode::Or(children?));
  }

  if let Some(not_value) = value.get("not") {
    let child = parse_where_clause_inner(not_value, depth + 1)?;
    return Ok(QueryNode::Not(Box::new(child)));
  }

  if value.get("field").is_some() {
    return parse_single_field_query(value);
  }

  Err(format!("Invalid where clause structure: {}", value))
}

// ---------------------------------------------------------------------------
// Projection helpers
// ---------------------------------------------------------------------------

/// Map virtual `@`-prefixed field names to their actual JSON keys.
fn map_select_fields(select: &[String]) -> Vec<String> {
  select.iter().map(|s| {
    match s.as_str() {
      "@path" => "path".to_string(),
      "@score" => "score".to_string(),
      "@size" => "size".to_string(),
      "@content_type" => "content_type".to_string(),
      "@created_at" => "created_at".to_string(),
      "@updated_at" => "updated_at".to_string(),
      "@matched_by" => "matched_by".to_string(),
      other => other.to_string(),
    }
  }).collect()
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
  Extension(_claims): Extension<TokenClaims>,
  active_key_rules: Option<Extension<ActiveKeyRules>>,
  Json(body): Json<QueryRequest>,
) -> Response {
  // Parse the where clause into a QueryNode tree.
  let query_node = match parse_where_clause(&body.r#where) {
    Ok(node) => node,
    Err(message) => {
      return ErrorResponse::new(message)
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  };

  // Check for empty where clause (AND with no children).
  let is_empty = matches!(&query_node, QueryNode::And(children) if children.is_empty());

  // Parse order_by
  let order_by: Vec<SortField> = body.order_by.as_ref()
    .map(|fields| fields.iter().map(|f| SortField {
      field: f.field.clone(),
      direction: match f.direction.as_deref() {
        Some("desc") => SortDirection::Desc,
        _ => SortDirection::Asc,
      },
    }).collect())
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
        return ErrorResponse::new(format!("Explain failed: {}", e))
          .with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response();
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
        return ErrorResponse::new(format!("Aggregation failed: {}", e))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response();
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
      let response_items: Vec<serde_json::Value> = paginated.results
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
      let response_items = if let Some(Extension(ref rules)) = active_key_rules {
        if !rules.0.is_empty() {
          let mut items = response_items;
          items.retain(|item| {
            let path = item["path"].as_str().unwrap_or("");
            let normalized = if path.starts_with('/') {
              path.to_string()
            } else {
              format!("/{}", path)
            };
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
        queue.get_reindex_progress_for_path(&body.path).map(|info| {
          QueryMeta {
            reindexing: Some(info.progress),
            reindexing_eta: info.eta_ms,
            reindexing_indexed: Some(info.indexed_count),
            reindexing_total: Some(info.total_count),
            reindexing_stale_since: info.stale_since,
          }
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
    Err(EngineError::NotFound(message)) => {
      ErrorResponse::new(message)
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Err(EngineError::JsonParseError(message)) => {
      ErrorResponse::new(message)
        .with_status(StatusCode::BAD_REQUEST)
        .into_response()
    }
    Err(EngineError::RangeQueryNotSupported(converter_name)) => {
      ErrorResponse::new(format!(
        "Range query not supported for converter '{}'",
        converter_name,
      ))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Query execution failed: {}", error);
      ErrorResponse::new(format!("Query failed: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
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
pub async fn engine_rename(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(path): Path<String>,
  Json(payload): Json<RenameRequest>,
) -> Response {
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
    return ErrorResponse::new(format!("Not found: {}", path))
      .with_status(StatusCode::NOT_FOUND)
      .into_response();
  }

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let engine = state.engine.clone();
  let path_for_blocking = path.clone();
  let destination_owned = destination.to_string();

  let result = tokio::task::spawn_blocking(move || -> EngineResult<&'static str> {
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
    Ok(Ok(kind)) => {
      evict_caches_for_path(&state, &path);
      evict_caches_for_path(&state, destination);
      let from_normalized = crate::engine::path_utils::normalize_path(&path);
      let to_normalized = crate::engine::path_utils::normalize_path(destination);
      (StatusCode::OK, Json(serde_json::json!({
        "from": from_normalized,
        "to": to_normalized,
        "entry_type": kind,
      })))
        .into_response()
    }
    Ok(Err(error)) => {
      tracing::error!("Engine: failed to rename '{}': {}", path, error);
      engine_error_response("Rename failed", &error)
    }
    Err(join_error) => {
      tracing::error!("rename task panicked: {}", join_error);
      ErrorResponse::new("Rename failed: internal task error")
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

// ---------------------------------------------------------------------------
// System repair
// ---------------------------------------------------------------------------

/// POST /system/repair — trigger a KV index rebuild from the append log.
pub async fn repair_kv(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
) -> Response {
    let caller_id = match uuid::Uuid::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(_) => return ErrorResponse::new("Invalid token")
            .with_status(StatusCode::UNAUTHORIZED).into_response(),
    };

    if !crate::engine::user::is_root(&caller_id) {
        return ErrorResponse::new("Root access required for repair operations")
            .with_status(StatusCode::FORBIDDEN).into_response();
    }

    match state.engine.rebuild_kv() {
        Ok(()) => {
            (StatusCode::OK, Json(serde_json::json!({
                "status": "ok",
                "message": "KV index rebuilt successfully",
            }))).into_response()
        }
        Err(e) => {
            ErrorResponse::new(format!("Repair failed: {}", e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response()
        }
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
    return ErrorResponse::new("Not found")
      .with_status(StatusCode::NOT_FOUND)
      .into_response();
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
      let name = crate::engine::path_utils::file_name(&from_normalized)
        .unwrap_or("").to_string();
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
      return ErrorResponse::new("Copy failed: internal task error")
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
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
}

pub async fn global_search_endpoint(
  State(state): State<AppState>,
  Extension(_claims): Extension<TokenClaims>,
  Json(payload): Json<GlobalSearchRequest>,
) -> Response {
  if payload.query.is_none() && payload.where_clause.is_none() {
    return ErrorResponse::new("At least one of 'query' or 'where' is required")
      .with_status(StatusCode::BAD_REQUEST)
      .into_response();
  }

  // Parse the where clause into a FieldQuery, if provided.
  let field_query = match payload.where_clause.as_ref() {
    Some(value) => {
      match parse_single_field_query(value) {
        Ok(QueryNode::Field(fq)) => Some(fq),
        Ok(_) => {
          return ErrorResponse::new("'where' must be a single field query (field, op, value)")
            .with_status(StatusCode::BAD_REQUEST)
            .into_response();
        }
        Err(msg) => {
          return ErrorResponse::new(msg)
            .with_status(StatusCode::BAD_REQUEST)
            .into_response();
        }
      }
    }
    None => None,
  };

  let base_path = payload.path.as_deref().unwrap_or("/");
  let limit = payload.limit.map(|l| l.min(1000));
  let offset = payload.offset;

  match crate::engine::search::global_search(
    &state.engine,
    base_path,
    payload.query.as_deref(),
    field_query.as_ref(),
    limit,
    offset,
  ) {
    Ok(results) => {
      let items: Vec<serde_json::Value> = results.results.iter().map(|r| {
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
      }).collect();

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
      ErrorResponse::new(format!("Search failed: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}
