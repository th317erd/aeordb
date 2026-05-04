use std::collections::HashMap;

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
use super::responses::{EngineFileResponse, ErrorResponse, ForkResponse, SnapshotResponse};
use super::state::AppState;
use crate::auth::TokenClaims;
use crate::auth::permission_middleware::ActiveKeyRules;
use crate::engine::api_key_rules::{match_rules, check_operation_permitted};
use crate::engine::{DirectoryOps, RequestContext, TaskStatus, VersionManager, is_root};
use crate::engine::directory_listing::list_directory_recursive;
use crate::engine::compression::{CompressionAlgorithm, decompress};
use crate::engine::directory_ops::{is_system_path, EngineFileStream, file_content_hash};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::EngineError;
use crate::engine::file_record::FileRecord;
use crate::engine::query_engine::{QueryEngine, QueryMeta, Query, QueryNode, FieldQuery, QueryOp, QueryStrategy, FuzzyOptions, Fuzziness, FuzzyAlgorithm, SortField, SortDirection, DEFAULT_QUERY_LIMIT, AggregateQuery, ExplainMode};
use crate::engine::symlink_resolver::{resolve_symlink, ResolvedTarget};

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
        // Check ancestor path first — items on the path to a scoped target
        // must be visible for directory tree navigation, regardless of the
        // deny-all fallback rule that would otherwise hide them.
        if crate::engine::api_key_rules::is_item_on_shared_path(rules, &path) {
            // Items on the ancestor path get read+list only for navigation.
            // Items that directly match a rule get that rule's permissions.
            let effective = match match_rules(rules, &path) {
                Some(rule) if rule.glob != "**" => rule.permitted.clone(),
                _ => "-r--l---".to_string(),
            };
            if let Some(obj) = entry.as_object_mut() {
                obj.insert("effective_permissions".to_string(), serde_json::Value::String(effective));
            }
            return true;
        }
        match match_rules(rules, &path) {
            Some(rule) => {
                if check_operation_permitted(&rule.permitted, operation) {
                    if let Some(obj) = entry.as_object_mut() {
                        obj.insert("effective_permissions".to_string(), serde_json::Value::String(rule.permitted.clone()));
                    }
                    true
                } else {
                    false
                }
            }
            None => false,
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
  let ops = DirectoryOps::new(&state.engine);

  match ops.create_directory(&ctx, &normalized) {
    Ok(()) => {
      (StatusCode::CREATED, Json(serde_json::json!({
        "path": normalized,
        "entry_type": 3,
        "created": true,
      }))).into_response()
    }
    Err(error) => {
      tracing::error!("Failed to create directory '{}': {}", normalized, error);
      ErrorResponse::new(format!("Failed to create directory: {}", error))
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
    .and_then(|value| value.to_str().ok());

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());

  let file_record = match directory_ops.finalize_file(
    &ctx, &path, chunk_hashes, total_size, content_type, &first_bytes,
  ) {
    Ok(record) => record,
    Err(error) => {
      tracing::error!("Engine: failed to store file at '{}': {}", path, error);
      let status = engine_error_status(&error);
      return ErrorResponse::new(sanitize_engine_error("Failed to store file", &error))
        .with_status(status)
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

  let safe_path = file_record.path.replace('\n', "").replace('\r', "");
  let mut response_builder = axum::http::Response::builder()
    .status(StatusCode::OK)
    .header("X-AeorDB-Path", safe_path)
    .header("X-AeorDB-Size", file_record.total_size.to_string())
    .header("X-AeorDB-Created", file_record.created_at.to_string())
    .header("X-AeorDB-Updated", file_record.updated_at.to_string());

  if let Some(target) = symlink_target {
    response_builder = response_builder
      .header("X-AeorDB-Link-Target", target.replace('\n', "").replace('\r', ""));
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
  group_cache: &std::sync::Arc<crate::engine::GroupCache>,
  permissions_cache: &std::sync::Arc<crate::engine::PermissionsCache>,
) {
  use crate::engine::permission_resolver::{CrudlifyOp, PermissionResolver};

  if crate::engine::is_root(user_id) { return; }

  let resolver = PermissionResolver::new(engine, group_cache, permissions_cache);
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

  let safe_path = file_record.path.replace('\n', "").replace('\r', "");
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

/// Handle default (flat) directory listing without depth/glob parameters.
fn handle_directory_listing(
  engine: &std::sync::Arc<crate::engine::StorageEngine>,
  path: &str,
  key_rules: Option<&[crate::engine::api_key_rules::KeyRule]>,
  user_id_str: &str,
  limit: Option<usize>,
  offset: Option<usize>,
  sort: Option<&str>,
  order: Option<&str>,
  state: Option<&AppState>,
) -> Response {
  let directory_ops = DirectoryOps::new(engine);

  match directory_ops.list_directory(path) {
    Ok(entries) => {
      let mut listing = build_directory_listing(&entries, path, &directory_ops);
      match apply_listing_filters(&mut listing, key_rules, user_id_str) {
        Ok(()) => {
          // Attach effective_permissions for non-root users
          if let Some(st) = state {
            if let Ok(uid) = uuid::Uuid::parse_str(user_id_str) {
              attach_effective_permissions(&mut listing, &uid, &st.engine, &st.group_cache, &st.permissions_cache);
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
pub async fn engine_get(
  State(state): State<AppState>,
  Extension(_claims): Extension<TokenClaims>,
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
      &state.engine, &path, &symlink_record.target, &_claims.sub, key_rules,
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
      &state.engine, &path, &version_query, key_rules, &_claims.sub,
    );
  }

  // Default flat directory listing
  handle_directory_listing(&state.engine, &path, key_rules, &_claims.sub, version_query.limit, version_query.offset, version_query.sort.as_deref(), version_query.order.as_deref(), Some(&state))
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
    .header("X-AeorDB-Path", path.replace('\n', "").replace('\r', ""))
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
  let directory_ops = DirectoryOps::new(&state.engine);

  // Check if it's a symlink first
  if directory_ops.get_symlink(&path).ok().flatten().is_some() {
    return match directory_ops.delete_symlink(&ctx, &path) {
      Ok(()) => {
        (StatusCode::OK, Json(serde_json::json!({ "deleted": true, "path": path, "entry_type": "symlink" })))
          .into_response()
      }
      Err(error) => {
        tracing::error!("Engine: failed to delete symlink '{}': {}", path, error);
        ErrorResponse::new(format!("Failed to delete symlink '{}'. If this persists, check GET /system/health for system status", path))
          .with_status(StatusCode::INTERNAL_SERVER_ERROR)
          .into_response()
      }
    };
  }

  match directory_ops.delete_file_with_indexing(&ctx, &path) {
    Ok(()) => {
      (
        StatusCode::OK,
        Json(serde_json::json!({ "deleted": true, "path": path })),
      )
        .into_response()
    }
    Err(EngineError::NotFound(_)) => {
      // File not found — try as an empty directory
      match directory_ops.delete_directory(&ctx, &path) {
        Ok(()) => {
          (StatusCode::OK, Json(serde_json::json!({ "deleted": true, "path": path, "entry_type": "directory" })))
            .into_response()
        }
        Err(EngineError::NotFound(_)) => {
          ErrorResponse::new(format!("Not found: {}", path))
            .with_status(StatusCode::NOT_FOUND)
            .into_response()
        }
        Err(EngineError::InvalidInput(msg)) => {
          ErrorResponse::new(msg)
            .with_status(StatusCode::BAD_REQUEST)
            .into_response()
        }
        Err(error) => {
          tracing::error!("Engine: failed to delete directory '{}': {}", path, error);
          ErrorResponse::new(format!("Failed to delete: {}", error))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response()
        }
      }
    }
    Err(error) => {
      tracing::error!("Engine: failed to delete file '{}': {}", path, error);
      ErrorResponse::new(format!("Failed to delete file '{}'. If this persists, check GET /system/health for system status", path))
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
  Extension(_claims): Extension<TokenClaims>,
  AxumQuery(params): AxumQuery<std::collections::HashMap<String, String>>,
) -> Response {
  let dir_path = params.get("path").map(|s| s.as_str()).unwrap_or("/");

  if is_system_path(dir_path) {
    return ErrorResponse::new(format!("Not found: {}", dir_path))
      .with_status(StatusCode::NOT_FOUND)
      .into_response();
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
  Extension(_claims): Extension<TokenClaims>,
  Path(path): Path<String>,
) -> Response {
  // Block ALL access to /.aeordb-system/ via API — system data is only accessible
  // through the internal system_store module, never through HTTP endpoints.
  if is_system_path(&path) {
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
      .header("X-AeorDB-Link-Target", symlink_record.target.replace('\n', "").replace('\r', ""))
      .header("X-AeorDB-Path", path.replace('\n', "").replace('\r', ""))
      .header("X-AeorDB-Created", symlink_record.created_at.to_string())
      .header("X-AeorDB-Updated", symlink_record.updated_at.to_string())
      .body(Body::empty())
      .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
  }

  match directory_ops.get_metadata(&path) {
    Ok(Some(file_record)) => {
      let safe_path = file_record.path.replace('\n', "").replace('\r', "");
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
          let safe_path = path.replace('\n', "").replace('\r', "");
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
pub async fn engine_get_by_hash(
  State(state): State<AppState>,
  Extension(_claims): Extension<TokenClaims>,
  Path(hex_hash): Path<String>,
) -> Response {
  // Any authenticated user can fetch by hash. System-flagged entries
  // return 404 (system hashes use a separate domain, can't be guessed).
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

// ---------------------------------------------------------------------------
// Snapshot routes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateSnapshotRequest {
  pub name: String,
  #[serde(default)]
  pub metadata: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct RestoreSnapshotRequest {
  /// Snapshot ID (hex root hash) — authoritative identifier.
  pub id: Option<String>,
  /// Snapshot name — fallback for backward compatibility.
  pub name: Option<String>,
}

/// POST /version/snapshot -- create a named snapshot of the current HEAD.
pub async fn snapshot_create(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Json(payload): Json<CreateSnapshotRequest>,
) -> Response {
  // Manual snapshot throttle: 1 per 60 seconds (own lane, doesn't
  // interfere with auto-snapshots from delete/restore operations)
  {
    use std::sync::atomic::Ordering;
    let now = chrono::Utc::now().timestamp_millis();
    let last = state.engine.last_manual_snapshot.load(Ordering::Relaxed);
    let elapsed = now - last;
    if elapsed < 60_000 && last > 0 {
      let remaining = (60_000 - elapsed) / 1000;
      return ErrorResponse::new(format!(
        "Snapshot rate limited. Try again in {} seconds.", remaining
      ))
        .with_status(StatusCode::TOO_MANY_REQUESTS)
        .into_response();
    }
    // CAS to claim the slot
    let _ = state.engine.last_manual_snapshot
      .compare_exchange(last, now, Ordering::SeqCst, Ordering::Relaxed);
  }

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let version_manager = VersionManager::new(&state.engine);

  match version_manager.create_snapshot(&ctx, &payload.name, payload.metadata) {
    Ok(snapshot_info) => {
      // If the returned snapshot has a different name than requested,
      // it was deduplicated (HEAD unchanged since that snapshot).
      let is_duplicate = snapshot_info.name != payload.name;
      let status = if is_duplicate { StatusCode::OK } else { StatusCode::CREATED };
      let mut response_body = serde_json::to_value(SnapshotResponse::from(&snapshot_info))
        .unwrap_or_default();
      if is_duplicate {
        response_body["duplicate"] = serde_json::json!(true);
        // Don't consume the rate limit slot for no-ops
        state.engine.last_manual_snapshot.store(0, std::sync::atomic::Ordering::Relaxed);
      }
      (status, Json(response_body)).into_response()
    }
    Err(EngineError::AlreadyExists(message)) => {
      ErrorResponse::new(message)
        .with_status(StatusCode::CONFLICT)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to create snapshot: {}", error);
      ErrorResponse::new(format!("Failed to create snapshot: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// GET /version/snapshots -- list all snapshots.
pub async fn snapshot_list(
  State(state): State<AppState>,
  Extension(_claims): Extension<TokenClaims>,
) -> Response {
  let version_manager = VersionManager::new(&state.engine);

  match version_manager.list_snapshots() {
    Ok(snapshots) => {
      let listing: Vec<SnapshotResponse> = snapshots
        .iter()
        .map(SnapshotResponse::from)
        .collect();
      (StatusCode::OK, Json(serde_json::json!({"items": listing}))).into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to list snapshots: {}", error);
      ErrorResponse::new(format!("Failed to list snapshots: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// POST /version/restore -- restore a named snapshot (requires root).
pub async fn snapshot_restore(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Json(payload): Json<RestoreSnapshotRequest>,
) -> Response {
  let user_id = match uuid::Uuid::parse_str(&claims.sub) {
    Ok(id) => id,
    Err(_) => {
      return (StatusCode::FORBIDDEN, Json(serde_json::json!({
        "error": "Invalid user ID"
      }))).into_response();
    }
  };
  if !is_root(&user_id) {
    return (StatusCode::FORBIDDEN, Json(serde_json::json!({
      "error": "Only root user can restore snapshots"
    }))).into_response();
  }

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let version_manager = VersionManager::new(&state.engine);

  // Resolve snapshot: id takes precedence over name
  let identifier = payload.id.as_deref()
    .or(payload.name.as_deref())
    .unwrap_or("");

  let snapshot = match version_manager.resolve_snapshot(identifier) {
    Ok(s) => s,
    Err(_) => {
      return ErrorResponse::new(format!("Snapshot not found: '{}'. Use GET /versions/snapshots to list available snapshots", identifier))
        .with_status(StatusCode::NOT_FOUND)
        .into_response();
    }
  };

  match version_manager.restore_snapshot(&ctx, &snapshot.name) {
    Ok(()) => {
      (
        StatusCode::OK,
        Json(serde_json::json!({ "restored": true, "id": snapshot.id(), "name": snapshot.name })),
      )
        .into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to restore snapshot '{}': {}", snapshot.name, error);
      ErrorResponse::new(format!("Failed to restore snapshot: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// DELETE /version/snapshot/:id_or_name -- delete a snapshot (requires root).
/// Accepts either a snapshot ID (hex root hash) or name.
pub async fn snapshot_delete(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(id_or_name): Path<String>,
) -> Response {
  let user_id = match uuid::Uuid::parse_str(&claims.sub) {
    Ok(id) => id,
    Err(_) => {
      return (StatusCode::FORBIDDEN, Json(serde_json::json!({
        "error": "Invalid user ID"
      }))).into_response();
    }
  };
  if !is_root(&user_id) {
    return (StatusCode::FORBIDDEN, Json(serde_json::json!({
      "error": "Only root user can delete snapshots"
    }))).into_response();
  }

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let version_manager = VersionManager::new(&state.engine);

  let snapshot = match version_manager.resolve_snapshot(&id_or_name) {
    Ok(s) => s,
    Err(_) => {
      return ErrorResponse::new(format!("Snapshot not found: '{}'", id_or_name))
        .with_status(StatusCode::NOT_FOUND)
        .into_response();
    }
  };

  let snap_id = snapshot.id();
  let snap_name = snapshot.name.clone();
  match version_manager.delete_snapshot(&ctx, &snap_name) {
    Ok(()) => {
      (
        StatusCode::OK,
        Json(serde_json::json!({ "deleted": true, "id": snap_id, "name": snap_name })),
      )
        .into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to delete snapshot '{}': {}", snap_name, error);
      ErrorResponse::new(format!("Failed to delete snapshot: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

// ---------------------------------------------------------------------------
// Fork routes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateForkRequest {
  pub name: String,
  pub base: Option<String>,
}

/// POST /version/fork -- create a named fork.
pub async fn fork_create(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Json(payload): Json<CreateForkRequest>,
) -> Response {
  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let version_manager = VersionManager::new(&state.engine);

  match version_manager.create_fork(&ctx, &payload.name, payload.base.as_deref()) {
    Ok(fork_info) => {
      let response_body = ForkResponse::from(&fork_info);
      (StatusCode::CREATED, Json(response_body)).into_response()
    }
    Err(EngineError::AlreadyExists(message)) => {
      ErrorResponse::new(message)
        .with_status(StatusCode::CONFLICT)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to create fork: {}", error);
      ErrorResponse::new(format!("Failed to create fork: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// GET /version/forks -- list all active forks.
pub async fn fork_list(
  State(state): State<AppState>,
  Extension(_claims): Extension<TokenClaims>,
) -> Response {
  let version_manager = VersionManager::new(&state.engine);

  match version_manager.list_forks() {
    Ok(forks) => {
      let listing: Vec<ForkResponse> = forks
        .iter()
        .map(ForkResponse::from)
        .collect();
      (StatusCode::OK, Json(serde_json::json!({"items": listing}))).into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to list forks: {}", error);
      ErrorResponse::new(format!("Failed to list forks: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// POST /version/fork/:name/promote -- promote a fork to HEAD (requires root).
pub async fn fork_promote(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(name): Path<String>,
) -> Response {
  let user_id = match uuid::Uuid::parse_str(&claims.sub) {
    Ok(id) => id,
    Err(_) => {
      return (StatusCode::FORBIDDEN, Json(serde_json::json!({
        "error": "Invalid user ID"
      }))).into_response();
    }
  };
  if !is_root(&user_id) {
    return (StatusCode::FORBIDDEN, Json(serde_json::json!({
      "error": "Only root user can promote forks"
    }))).into_response();
  }

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let version_manager = VersionManager::new(&state.engine);

  match version_manager.promote_fork(&ctx, &name) {
    Ok(()) => {
      (
        StatusCode::OK,
        Json(serde_json::json!({ "promoted": true, "name": name })),
      )
        .into_response()
    }
    Err(EngineError::NotFound(_)) => {
      ErrorResponse::new(format!("Fork not found: '{}'. Use GET /versions/forks to list active forks", name))
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to promote fork '{}': {}", name, error);
      ErrorResponse::new(format!("Failed to promote fork: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// DELETE /version/fork/:name -- abandon a fork (requires root).
pub async fn fork_abandon(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(name): Path<String>,
) -> Response {
  let user_id = match uuid::Uuid::parse_str(&claims.sub) {
    Ok(id) => id,
    Err(_) => {
      return (StatusCode::FORBIDDEN, Json(serde_json::json!({
        "error": "Invalid user ID"
      }))).into_response();
    }
  };
  if !is_root(&user_id) {
    return (StatusCode::FORBIDDEN, Json(serde_json::json!({
      "error": "Only root user can abandon forks"
    }))).into_response();
  }

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let version_manager = VersionManager::new(&state.engine);

  match version_manager.abandon_fork(&ctx, &name) {
    Ok(()) => {
      (
        StatusCode::OK,
        Json(serde_json::json!({ "abandoned": true, "name": name })),
      )
        .into_response()
    }
    Err(EngineError::NotFound(_)) => {
      ErrorResponse::new(format!("Fork not found: '{}'. Use GET /versions/forks to list active forks", name))
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to abandon fork '{}': {}", name, error);
      ErrorResponse::new(format!("Failed to abandon fork: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

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
  let ops = DirectoryOps::new(&state.engine);

  // Try symlink first, then file
  if ops.get_symlink(&path).ok().flatten().is_some() {
    return match ops.rename_symlink(&ctx, &path, destination) {
      Ok(_record) => {
        let from_normalized = crate::engine::path_utils::normalize_path(&path);
        let to_normalized = crate::engine::path_utils::normalize_path(destination);
        (StatusCode::OK, Json(serde_json::json!({
          "from": from_normalized,
          "to": to_normalized,
          "entry_type": "symlink",
        }))).into_response()
      }
      Err(ref error) => {
        let status = engine_error_status(error);
        let message = sanitize_engine_error("Rename failed", error);
        tracing::error!("Engine: failed to rename symlink '{}': {}", path, error);
        ErrorResponse::new(message)
          .with_status(status)
          .into_response()
      }
    };
  }

  match ops.rename_file(&ctx, &path, destination) {
    Ok(_record) => {
      let from_normalized = crate::engine::path_utils::normalize_path(&path);
      let to_normalized = crate::engine::path_utils::normalize_path(destination);
      (StatusCode::OK, Json(serde_json::json!({
        "from": from_normalized,
        "to": to_normalized,
        "entry_type": "file",
      }))).into_response()
    }
    Err(ref error) => {
      let status = engine_error_status(error);
      let message = sanitize_engine_error("Rename failed", error);
      tracing::error!("Engine: failed to rename file '{}': {}", path, error);
      ErrorResponse::new(message)
        .with_status(status)
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

fn engine_error_status(error: &EngineError) -> StatusCode {
  match error {
    EngineError::NotFound(_) => StatusCode::NOT_FOUND,
    EngineError::AlreadyExists(_) => StatusCode::CONFLICT,
    EngineError::InvalidInput(_) => StatusCode::BAD_REQUEST,
    _ => StatusCode::INTERNAL_SERVER_ERROR,
  }
}

/// L12: Return a safe error message for client responses.
/// Passes through user-facing validation messages (InvalidInput, NotFound,
/// AlreadyExists) but suppresses internal details for all other error variants.
fn sanitize_engine_error(prefix: &str, error: &EngineError) -> String {
  match error {
    EngineError::InvalidInput(msg) => format!("{}: {}", prefix, msg),
    EngineError::NotFound(msg) => format!("{}: {}", prefix, msg),
    EngineError::AlreadyExists(msg) => format!("{}: {}", prefix, msg),
    _ => prefix.to_string(),
  }
}

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
  let ops = DirectoryOps::new(&state.engine);
  let mut copied = Vec::new();
  let mut errors = Vec::new();

  for path in &payload.paths {
    let from_normalized = crate::engine::path_utils::normalize_path(path);
    let name = crate::engine::path_utils::file_name(&from_normalized)
      .unwrap_or("").to_string();
    let to_path = format!("{}/{}", dest_normalized.trim_end_matches('/'), name);

    match ops.copy_path(&ctx, &from_normalized, &to_path) {
      Ok(paths) => copied.extend(paths),
      Err(error) => errors.push(format!("{}: {}", from_normalized, error)),
    }
  }

  let mut response = serde_json::json!({ "copied": copied });
  if !errors.is_empty() {
    response["errors"] = serde_json::json!(errors);
  }

  (StatusCode::OK, Json(response)).into_response()
}
