use axum::{
  extract::State,
  http::{HeaderMap, StatusCode},
  response::{IntoResponse, Response},
  Json,
};
use serde::{Deserialize, Serialize};

use base64::Engine as _;

use super::responses::ErrorResponse;
use super::state::AppState;
use crate::engine::api_key_rules::{check_operation_permitted, match_rules, KeyRule};
use crate::engine::sync_api::{compute_sync_diff, SyncDiff as EngineSyncDiff};

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SyncDiffRequest {
  pub since_root_hash: Option<String>,
  pub current_root_hash: Option<String>,
  pub paths: Option<Vec<String>>,
  pub node_id: Option<u64>,
  pub virtual_time: Option<u64>,
}

#[derive(Serialize)]
pub struct SyncDiffResponse {
  pub root_hash: String,
  pub changes: SyncChanges,
  pub chunk_hashes_needed: Vec<String>,
}

#[derive(Serialize)]
pub struct SyncChanges {
  pub files_added: Vec<SyncFileEntry>,
  pub files_modified: Vec<SyncFileEntry>,
  pub files_deleted: Vec<SyncDeletedEntry>,
  pub symlinks_added: Vec<SyncSymlinkEntry>,
  pub symlinks_modified: Vec<SyncSymlinkEntry>,
  pub symlinks_deleted: Vec<SyncDeletedEntry>,
}

#[derive(Serialize)]
pub struct SyncFileEntry {
  pub path: String,
  pub hash: String,
  pub size: u64,
  pub content_type: Option<String>,
  pub chunk_hashes: Vec<String>,
}

#[derive(Serialize)]
pub struct SyncSymlinkEntry {
  pub path: String,
  pub hash: String,
  pub target: String,
}

#[derive(Serialize)]
pub struct SyncDeletedEntry {
  pub path: String,
}

#[derive(Deserialize)]
pub struct SyncChunksRequest {
  pub hashes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Caller identity for sync operations
// ---------------------------------------------------------------------------

/// Describes who is calling the sync endpoint and what access they have.
///
/// A replication peer calls with a JWT minted by
/// `SyncEngine::mint_sync_token`, which has `sub: ROOT_USER_ID` and
/// `scope: "sync"`. Those calls receive only registry families selected by
/// peer-replication policy. Everyone else receives client-sync policy before
/// ordinary path authorization.
pub enum SyncCaller {
  /// Replication peer: `sub: ROOT_USER_ID` + `scope: "sync"`.
  Peer,
  /// Root JWT (nil UUID), no sync scope — admin tool, not a peer.
  /// Receives client-sync policy; use backup for complete protected closure.
  RootUser,
  /// Non-root JWT — client-sync policy followed by API-key/user rules.
  ScopedUser {
    // TODO: Use for per-user sync audit logging and rate limiting.
    #[allow(dead_code)]
    user_id: String,
    key_rules: Vec<KeyRule>,
  },
}

impl SyncCaller {
  /// Whether this caller selects registry peer-replication policy.
  fn include_system(&self) -> bool {
    matches!(self, SyncCaller::Peer)
  }

  /// API key rules for path-level filtering (empty = no restrictions).
  fn key_rules(&self) -> &[KeyRule] {
    match self {
      SyncCaller::ScopedUser { key_rules, .. } => key_rules,
      _ => &[],
    }
  }
}

/// Determine the caller identity from request headers.
/// Verifies JWT Bearer token. Returns 401 if no valid auth is present.
fn determine_sync_caller(headers: &HeaderMap, state: &AppState) -> Result<SyncCaller, Response> {
  // 0. If auth is disabled (dev mode), select peer-replication policy to
  //    preserve the pre-auth-disabled sync behavior.
  if !state.auth_provider.is_enabled() {
    return Ok(SyncCaller::Peer);
  }

  // 1. Try JWT Bearer token.
  if let Some(auth_header) = headers.get("authorization") {
    let token = auth_header.to_str().ok().and_then(|s| s.strip_prefix("Bearer ")).ok_or_else(|| {
      ErrorResponse::new("Invalid authorization header: expected 'Bearer <token>' format")
        .with_status(StatusCode::UNAUTHORIZED)
        .into_response()
    })?;

    let claims = state.jwt_manager.verify_token(token).map_err(|_| {
      ErrorResponse::new("Invalid or expired JWT. Re-authenticate via POST /auth/token")
        .with_status(StatusCode::UNAUTHORIZED)
        .into_response()
    })?;

    let user_id = uuid::Uuid::parse_str(&claims.sub).map_err(|_| {
      ErrorResponse::new("Invalid user ID in token: 'sub' claim is not a valid UUID").with_status(StatusCode::UNAUTHORIZED).into_response()
    })?;

    if crate::engine::user::is_root(&user_id) {
      // A root JWT with `scope: "sync"` is a replication peer
      // (minted by SyncEngine::mint_sync_token); it receives registry-approved
      // portable peer state. A root JWT without that scope is an admin tool and
      // receives client-sync policy; use a root-key backup for complete closure.
      if claims.scope.as_deref() == Some("sync") {
        return Ok(SyncCaller::Peer);
      }
      return Ok(SyncCaller::RootUser);
    }

    // Non-root: check API key scoping if key_id is present.
    let key_rules = if let Some(ref key_id) = claims.key_id {
      match state.api_key_cache.get(&key_id.to_string(), &state.engine) {
        Ok(Some(key_record)) => {
          if key_record.is_revoked {
            return Err(
              ErrorResponse::new("API key has been revoked. Create a new key via POST /auth/api-keys")
                .with_status(StatusCode::UNAUTHORIZED)
                .into_response(),
            );
          }
          if key_record.expires_at <= chrono::Utc::now().timestamp_millis() {
            return Err(
              ErrorResponse::new("API key expired. Create a new key via POST /auth/api-keys")
                .with_status(StatusCode::UNAUTHORIZED)
                .into_response(),
            );
          }
          key_record.rules
        }
        Ok(None) => {
          return Err(
            ErrorResponse::new("API key not found: the key referenced in the token no longer exists")
              .with_status(StatusCode::UNAUTHORIZED)
              .into_response(),
          );
        }
        Err(_) => {
          return Err(
            ErrorResponse::new(
              "Failed to look up API key: could not read from storage. If this persists, check GET /system/health for system status",
            )
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response(),
          );
        }
      }
    } else {
      vec![]
    };

    return Ok(SyncCaller::ScopedUser { user_id: claims.sub, key_rules });
  }

  Err(
    ErrorResponse::new("Authentication required. Provide a Bearer token via the Authorization header")
      .with_status(StatusCode::UNAUTHORIZED)
      .into_response(),
  )
}

// ---------------------------------------------------------------------------
// Helpers: convert the embedded sync result to the stable HTTP schema
// ---------------------------------------------------------------------------

fn sync_changes_from_engine(diff: EngineSyncDiff) -> SyncChanges {
  let convert_file = |entry: crate::engine::sync_api::SyncFileEntry| SyncFileEntry {
    path: entry.path,
    hash: hex::encode(entry.hash),
    size: entry.size,
    content_type: entry.content_type,
    chunk_hashes: entry.chunk_hashes.into_iter().map(hex::encode).collect(),
  };
  let convert_symlink = |entry: crate::engine::sync_api::SyncSymlinkEntry| SyncSymlinkEntry {
    path: entry.path,
    hash: hex::encode(entry.hash),
    target: entry.target,
  };
  let convert_deleted = |entry: crate::engine::sync_api::SyncDeletedEntry| SyncDeletedEntry { path: entry.path };

  SyncChanges {
    files_added: diff.files_added.into_iter().map(convert_file).collect(),
    files_modified: diff.files_modified.into_iter().map(convert_file).collect(),
    files_deleted: diff.files_deleted.into_iter().map(convert_deleted).collect(),
    symlinks_added: diff.symlinks_added.into_iter().map(convert_symlink).collect(),
    symlinks_modified: diff.symlinks_modified.into_iter().map(convert_symlink).collect(),
    symlinks_deleted: diff.symlinks_deleted.into_iter().map(convert_deleted).collect(),
  }
}

/// Check if a path is readable according to API key rules.
/// Empty rules = full access (no restrictions).
fn path_allowed_by_key_rules(path: &str, rules: &[KeyRule]) -> bool {
  if rules.is_empty() {
    return true; // no rules = no path-level restrictions
  }
  match match_rules(rules, path) {
    Some(rule) => check_operation_permitted(&rule.permitted, 'r'),
    None => false,
  }
}

/// Post-process SyncChanges to apply API key rule filtering.
fn filter_changes_by_key_rules(changes: &mut SyncChanges, rules: &[KeyRule]) {
  if rules.is_empty() {
    return;
  }

  changes.files_added.retain(|e| path_allowed_by_key_rules(&e.path, rules));
  changes.files_modified.retain(|e| path_allowed_by_key_rules(&e.path, rules));
  changes.files_deleted.retain(|e| path_allowed_by_key_rules(&e.path, rules));
  changes.symlinks_added.retain(|e| path_allowed_by_key_rules(&e.path, rules));
  changes.symlinks_modified.retain(|e| path_allowed_by_key_rules(&e.path, rules));
  changes.symlinks_deleted.retain(|e| path_allowed_by_key_rules(&e.path, rules));
}

/// Drop every entry the user can't directly Read. Used for non-peer
/// callers so /sync/diff never leaks paths outside the user's grants.
/// Without this, a user with only directory-level shares (and no API
/// key rules) would receive the full path list — a metadata leak even
/// though GET /files/{path} would correctly 403 on the content.
///
/// SystemFamily client-sync policy has already removed ineligible protected,
/// derived, secret, and node-local entries before this authorization pass.
fn filter_changes_by_user_permissions(
  changes: &mut SyncChanges,
  user_id_str: &str,
  state: &AppState,
) -> crate::engine::errors::EngineResult<()> {
  use crate::engine::permission_resolver::CrudlifyOp;

  // Root short-circuits in the resolver, but we want belt-and-suspenders:
  // root callers never reach this path (Peer/RootUser handled separately).
  let user_id = uuid::Uuid::parse_str(user_id_str)
    .map_err(|error| crate::engine::errors::EngineError::InvalidInput(format!("invalid sync user identity: {error}")))?;
  let resolver = crate::engine::permission_resolver::PermissionResolver::new(&state.engine, &state.group_cache);
  let mut allowed = std::collections::HashSet::new();
  for path in changes
    .files_added
    .iter()
    .map(|entry| entry.path.as_str())
    .chain(changes.files_modified.iter().map(|entry| entry.path.as_str()))
    .chain(changes.files_deleted.iter().map(|entry| entry.path.as_str()))
    .chain(changes.symlinks_added.iter().map(|entry| entry.path.as_str()))
    .chain(changes.symlinks_modified.iter().map(|entry| entry.path.as_str()))
    .chain(changes.symlinks_deleted.iter().map(|entry| entry.path.as_str()))
  {
    if resolver.check_path_permission(&user_id, path, CrudlifyOp::Read)? {
      allowed.insert(path.to_string());
    }
  }

  changes.files_added.retain(|entry| allowed.contains(&entry.path));
  changes.files_modified.retain(|entry| allowed.contains(&entry.path));
  changes.files_deleted.retain(|entry| allowed.contains(&entry.path));
  changes.symlinks_added.retain(|entry| allowed.contains(&entry.path));
  changes.symlinks_modified.retain(|entry| allowed.contains(&entry.path));
  changes.symlinks_deleted.retain(|entry| allowed.contains(&entry.path));
  Ok(())
}

fn user_has_grant_scope(user_id_str: &str, state: &AppState) -> crate::engine::errors::EngineResult<bool> {
  use crate::engine::permission_resolver::PermissionResolver;

  let user_id = uuid::Uuid::parse_str(user_id_str)
    .map_err(|error| crate::engine::errors::EngineError::InvalidInput(format!("invalid sync user identity: {error}")))?;

  let resolver = PermissionResolver::new(&state.engine, &state.group_cache);
  resolver.has_descendant_grants(&user_id, "/")
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// POST /sync/diff -- compute and return tree differences.
pub async fn sync_diff(State(state): State<AppState>, headers: HeaderMap, Json(payload): Json<SyncDiffRequest>) -> Response {
  // M3: Cap the number of path filters to prevent abuse.
  if let Some(ref paths) = payload.paths {
    if paths.len() > 100 {
      return ErrorResponse::new("Too many path filters (max 100). Reduce the number of entries in the 'paths' array")
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  }

  let caller = match determine_sync_caller(&headers, &state) {
    Ok(c) => c,
    Err(response) => return response,
  };

  let include_system = caller.include_system();
  let since_hash = match payload.since_root_hash.as_deref() {
    Some(value) => match hex::decode(value) {
      Ok(hash) => Some(hash),
      Err(_) => {
        return ErrorResponse::new("Invalid since_root_hash: value is not valid hex. Use the root_hash from a previous sync response")
          .with_status(StatusCode::BAD_REQUEST)
          .into_response()
      }
    },
    None => None,
  };
  let diff = match compute_sync_diff(&state.engine, since_hash.as_deref(), payload.paths.as_deref(), include_system) {
    Ok(diff) => diff,
    Err(error) => {
      return ErrorResponse::new(format!("Failed to compute sync diff: {error}"))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  };
  let root_hash = hex::encode(&diff.root_hash);
  let mut changes = sync_changes_from_engine(diff);

  // Apply API key rule filtering for scoped users.
  filter_changes_by_key_rules(&mut changes, caller.key_rules());

  // Apply user/group permission filtering only for plain JWT callers that
  // actually have grant-scoped access. API-key rules are explicit sync
  // scope and are applied above; non-root JWTs with no grants keep the
  // existing client-sync contract after registry policy.
  if let SyncCaller::ScopedUser { user_id, key_rules } = &caller {
    if key_rules.is_empty() {
      match user_has_grant_scope(user_id, &state) {
        Ok(true) => {
          if let Err(error) = filter_changes_by_user_permissions(&mut changes, user_id, &state) {
            return ErrorResponse::new(format!("Failed to apply sync permissions: {error}"))
              .with_status(StatusCode::INTERNAL_SERVER_ERROR)
              .into_response();
          }
        }
        Ok(false) => {}
        Err(error) => {
          return ErrorResponse::new(format!("Failed to resolve sync permission scope: {error}"))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
        }
      }
    }
  }

  // H4: Rebuild chunk hashes from the FILTERED changes so scoped users
  // don't receive chunk hashes for files they can't access.
  let filtered_chunk_hashes: Vec<String> = {
    let mut hashes: Vec<String> =
      changes.files_added.iter().chain(changes.files_modified.iter()).flat_map(|e| e.chunk_hashes.iter().cloned()).collect();
    hashes.sort();
    hashes.dedup();
    hashes
  };

  let response = SyncDiffResponse { root_hash, changes, chunk_hashes_needed: filtered_chunk_hashes };

  (StatusCode::OK, Json(response)).into_response()
}

/// POST /sync/chunks -- batch chunk transfer.
///
/// Caps both the number of hashes (10,000) and the total response payload
/// (512 MB) to bound peer memory usage. A worker hitting either limit returns
/// 413 Payload Too Large; the caller should split the request and retry.
const SYNC_CHUNKS_MAX_RESPONSE_BYTES: usize = 512 * 1024 * 1024;
const SYNC_CHUNKS_MAX_HASHES: usize = 10_000;

pub async fn sync_chunks(State(state): State<AppState>, headers: HeaderMap, Json(payload): Json<SyncChunksRequest>) -> Response {
  if payload.hashes.len() > SYNC_CHUNKS_MAX_HASHES {
    return ErrorResponse::new(format!("Too many chunk hashes (max {}). Split the request into multiple batches", SYNC_CHUNKS_MAX_HASHES))
      .with_status(StatusCode::BAD_REQUEST)
      .into_response();
  }

  let caller = match determine_sync_caller(&headers, &state) {
    Ok(c) => c,
    Err(response) => return response,
  };

  let filter_system = !caller.include_system();

  // Scoped-key enforcement: a key with rules (non-empty key_rules) could
  // exfiltrate chunks outside its scope by guessing hashes, because
  // /sync/chunks identifies content by hash and provides no path
  // context. Refuse explicitly. Non-root callers WITHOUT rules (regular
  // client sync) are still allowed — they only see non-system chunks
  // via the `filter_system` gate below.
  if let SyncCaller::ScopedUser { key_rules, .. } = &caller {
    if !key_rules.is_empty() {
      return ErrorResponse::new(
        "Scoped API keys (with path rules) cannot use /sync/chunks. \
                 Use /files/{path} for path-aware content access.",
      )
      .with_status(StatusCode::FORBIDDEN)
      .into_response();
    }
  }

  let mut chunks: Vec<serde_json::Value> = Vec::new();
  // Track accumulated payload size so we don't OOM building a 3.4 GB JSON
  // response from a worst-case 10k * 256 KB request. base64 expands 4/3.
  let mut accumulated_payload: usize = 0;

  for hex_hash in &payload.hashes {
    let hash = match hex::decode(hex_hash) {
      Ok(h) => h,
      Err(_) => continue,
    };

    if let Ok(Some((header, _key, _value))) = state.engine.get_entry_including_deleted(&hash) {
      // Skip system entries for non-root/non-peer callers.
      if filter_system && header.is_system_entry() {
        continue;
      }

      let data = match state.engine.read_chunk_including_deleted(&hash) {
        Ok(Some(data)) => data,
        _ => continue,
      };
      state.engine.counters().record_read(data.len() as u64);

      // Project base64-encoded size + JSON overhead. Bail before pushing.
      let projected = data.len().saturating_mul(4) / 3 + hex_hash.len() + 64;
      if accumulated_payload.saturating_add(projected) > SYNC_CHUNKS_MAX_RESPONSE_BYTES {
        return (
          StatusCode::PAYLOAD_TOO_LARGE,
          Json(serde_json::json!({
              "error": format!(
                  "Response would exceed {} bytes; split the request into smaller batches",
                  SYNC_CHUNKS_MAX_RESPONSE_BYTES
              ),
              "chunks_so_far": chunks.len(),
          })),
        )
          .into_response();
      }
      accumulated_payload += projected;

      chunks.push(serde_json::json!({
          "hash": hex_hash,
          "data": base64::engine::general_purpose::STANDARD.encode(&data),
          "size": data.len(),
      }));
    }
  }

  (StatusCode::OK, Json(serde_json::json!({ "chunks": chunks }))).into_response()
}
