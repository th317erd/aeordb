use std::collections::HashSet;

use axum::{
  Extension,
  extract::State,
  http::{HeaderMap, StatusCode},
  response::{IntoResponse, Response},
  Json,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::responses::{engine_error_response, ErrorResponse};
use super::route_permissions::{parse_user_id, reject_share_key, RoutePermissionChecker};
use super::state::AppState;
use crate::auth::TokenClaims;
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::permission_resolver::CrudlifyOp;
use crate::engine::permissions::{PathPermissions, PermissionRevokeResult, PermissionStore, validate_permission_flags};
use crate::engine::path_utils::{normalize_path, parent_path, file_name};
use crate::engine::request_context::RequestContext;
use crate::engine::user::is_root;

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ShareRequest {
  pub paths: Vec<String>,
  pub users: Option<Vec<String>>,
  pub groups: Option<Vec<String>>,
  pub permissions: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SharePathKind {
  File,
  Directory,
}

fn share_path_kind(ops: &DirectoryOps<'_>, path: &str) -> EngineResult<Option<SharePathKind>> {
  if ops.get_metadata(path)?.is_some() {
    return Ok(Some(SharePathKind::File));
  }

  match ops.list_directory_strict(path) {
    Ok(_) => Ok(Some(SharePathKind::Directory)),
    Err(EngineError::NotFound(_)) => Ok(None),
    Err(error) => Err(error),
  }
}

fn load_stored_permissions(ops: &DirectoryOps<'_>, path: &str) -> EngineResult<Option<PathPermissions>> {
  match ops.read_file_buffered(path) {
    Ok(data) => PathPermissions::deserialize_stored(&data, path).map(Some),
    Err(EngineError::NotFound(_)) => Ok(None),
    Err(error) => Err(error),
  }
}

#[derive(Deserialize)]
pub struct SharesQuery {
  pub path: String,
}

#[derive(Deserialize)]
pub struct UnshareRequest {
  pub path: String,
  pub group: String,
  #[serde(default)]
  pub path_pattern: Option<String>,
}

#[derive(Serialize)]
struct ShareInfo {
  group: String,
  allow: String,
  deny: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  path_pattern: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  username: Option<String>,
}

// ---------------------------------------------------------------------------
// POST /files/share
// ---------------------------------------------------------------------------

/// Share one or more paths with users and/or groups.
///
/// For each path:
///   - If the path is a file, permissions are stored on the parent directory
///     with a `path_pattern` matching the filename.
///   - If the path is a directory, permissions are stored on that directory
///     with no `path_pattern` (applies to everything inside).
///
/// Derive an absolute base URL (scheme + host + optional port, no path)
/// from the request headers. Used for embedding clickable links in
/// outbound emails — relative URLs render dead in most mail clients.
///
/// Precedence:
///   1. `X-Forwarded-Proto` + `X-Forwarded-Host` (proxy / load balancer).
///   2. `Host` header with assumed `http://` (direct connection).
///   3. Fallback: empty string — caller may decide to skip the email,
///      log a warning, or proceed with a relative URL.
fn resolve_public_base_url(headers: &HeaderMap) -> Option<String> {
  let host = headers.get("x-forwarded-host").and_then(|v| v.to_str().ok()).or_else(|| headers.get("host").and_then(|v| v.to_str().ok()))?;
  if host.is_empty() {
    return None;
  }
  let scheme =
    headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).map(|s| s.split(',').next().unwrap_or(s).trim()).unwrap_or_else(|| {
      // Heuristic: localhost without explicit X-Forwarded-Proto is dev.
      if host.starts_with("localhost") || host.starts_with("127.0.0.1") {
        "http"
      } else {
        "https"
      }
    });
  Some(format!("{}://{}", scheme, host))
}

pub async fn share(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  headers: HeaderMap,
  Json(body): Json<ShareRequest>,
) -> Response {
  // Parse and validate caller identity
  let caller_id = match Uuid::parse_str(&claims.sub) {
    Ok(id) => id,
    Err(_) => {
      return ErrorResponse::new("Invalid user identity").with_status(StatusCode::FORBIDDEN).into_response();
    }
  };

  // Only root can share for now
  if !is_root(&caller_id) {
    return ErrorResponse::new("Only root can share files").with_status(StatusCode::FORBIDDEN).into_response();
  }
  let sharer_name = "Root".to_string();

  if body.paths.is_empty() {
    return ErrorResponse::new("At least one path is required").with_status(StatusCode::BAD_REQUEST).into_response();
  }

  let has_users = body.users.as_ref().is_some_and(|u| !u.is_empty());
  let has_groups = body.groups.as_ref().is_some_and(|g| !g.is_empty());
  if !has_users && !has_groups {
    return ErrorResponse::new("At least one user or group is required").with_status(StatusCode::BAD_REQUEST).into_response();
  }

  if let Err(error) = validate_permission_flags(&body.permissions) {
    return ErrorResponse::new(error.to_string()).with_status(StatusCode::BAD_REQUEST).into_response();
  }

  let ctx = RequestContext::system();
  let mut direct_users = Vec::new();
  let mut seen_direct_users = HashSet::new();
  if let Some(users) = body.users.as_ref() {
    for user_id in users {
      if seen_direct_users.insert(user_id.clone()) {
        direct_users.push(user_id.clone());
      }
    }
  }
  let mut target_groups = Vec::new();
  target_groups.extend(direct_users.iter().map(|user_id| format!("user:{user_id}")));
  if let Some(groups) = body.groups.as_ref() {
    target_groups.extend(groups.iter().cloned());
  }
  let grant = match PermissionStore::new(&state.engine).grant_paths(&ctx, body.paths.clone(), target_groups, body.permissions.clone()) {
    Ok(grant) => grant,
    Err(error) => return engine_error_response("Failed to share paths", &error),
  };
  let changed_paths = grant.changed_paths;
  let shared_paths = grant.paths;
  let shared_count = shared_paths.len();

  if changed_paths.is_empty() {
    return Json(serde_json::json!({
        "shared": shared_count,
        "paths": shared_paths,
    }))
    .into_response();
  }

  // Emit per-recipient SSE events for live notification.
  // Each user receives one event per shared path (delivered via /events/me).
  for recipient_uid in &direct_users {
    for path in &changed_paths {
      let event = crate::engine::engine_event::EngineEvent::for_user(
        crate::engine::engine_event::EVENT_FILES_SHARED,
        &claims.sub,
        recipient_uid,
        serde_json::json!({
            "path": path,
            "permissions": body.permissions,
            "from": sharer_name,
        }),
      );
      state.event_bus.emit(event);
    }
  }

  // Spawn background email notification (best-effort)
  let engine_clone = state.engine.clone();
  let notify_paths = changed_paths.clone();
  let notify_permissions = body.permissions.clone();
  let notify_users: Vec<String> = direct_users;
  let sharer = sharer_name.clone();
  let public_base_url = resolve_public_base_url(&headers);
  tokio::spawn(async move {
    send_share_notifications(&engine_clone, &sharer, &notify_users, &notify_paths, &notify_permissions, public_base_url.as_deref()).await;
  });

  Json(serde_json::json!({
      "shared": shared_count,
      "paths": shared_paths,
  }))
  .into_response()
}

// ---------------------------------------------------------------------------
// GET /files/shares?path=...
// ---------------------------------------------------------------------------

/// List active shares for a path.
pub async fn list_shares(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  axum::extract::Query(query): axum::extract::Query<SharesQuery>,
) -> Response {
  // Share tokens cannot list shares
  if let Err(response) = reject_share_key(&claims, "Not available for share links") {
    return response.into_response();
  }
  let normalized = normalize_path(&query.path);

  // Require the caller to have at least Read access on the queried path
  // (or be root). Without this, a non-root user could probe arbitrary
  // paths to enumerate who else has been granted access — leaking
  // both path existence AND usernames of other grantees. 404 (not 403)
  // so the response shape doesn't reveal existence on its own.
  let permissions = match RoutePermissionChecker::from_claims(&state, &claims, "Invalid user identity") {
    Ok(permissions) => permissions,
    Err(response) => return response.into_response(),
  };
  if !permissions.is_root() {
    let permitted = match permissions.has_any_path_permission(&normalized, &[CrudlifyOp::Read, CrudlifyOp::List]) {
      Ok(permitted) => permitted,
      Err(error) => return engine_error_response("Failed to check share-listing permission", &error),
    };
    if !permitted {
      return ErrorResponse::new(format!("Not found: {}", normalized)).with_status(StatusCode::NOT_FOUND).into_response();
    }
  }

  let ops = DirectoryOps::new(&state.engine);

  // Determine perm_dir: if path is a file, look at parent.
  let is_file = match share_path_kind(&ops, &normalized) {
    Ok(Some(SharePathKind::File)) => true,
    Ok(Some(SharePathKind::Directory)) | Ok(None) => false,
    Err(error) => return engine_error_response("Failed to inspect shared path", &error),
  };
  let perm_dir = if is_file { parent_path(&normalized).unwrap_or_else(|| "/".to_string()) } else { normalized.clone() };

  let perm_file_path = if perm_dir == "/" || perm_dir.ends_with('/') {
    format!("{}.aeordb-permissions", perm_dir)
  } else {
    format!("{}/.aeordb-permissions", perm_dir)
  };

  let perms = match load_stored_permissions(&ops, &perm_file_path) {
    Ok(Some(permissions)) => permissions,
    Ok(None) => PathPermissions { links: Vec::new() },
    Err(error) => return engine_error_response("Failed to load permissions", &error),
  };

  // If the query is for a specific file, filter to links with matching path_pattern
  let file_filter = if is_file { file_name(&normalized).map(|s| s.to_string()) } else { None };

  let mut shares: Vec<ShareInfo> = Vec::new();
  for link in &perms.links {
    // If filtering for a specific file, only include matching path_pattern links
    if let Some(ref filter) = file_filter {
      match &link.path_pattern {
        Some(pp) if pp == filter => {}
        Some(_) => continue,
        None => {} // directory-wide link still applies
      }
    }

    // Resolve username for user:UUID groups
    let username = if link.group.starts_with("user:") {
      let uid_str = &link.group[5..];
      if let Ok(uid) = Uuid::parse_str(uid_str) {
        match crate::engine::system_store::get_user(&state.engine, &uid) {
          Ok(Some(user)) => Some(user.username),
          Ok(None) => None,
          Err(error) => return engine_error_response("Failed to resolve shared user", &error),
        }
      } else {
        None
      }
    } else {
      None
    };

    shares.push(ShareInfo {
      group: link.group.clone(),
      allow: link.allow.clone(),
      deny: link.deny.clone(),
      path_pattern: link.path_pattern.clone(),
      username,
    });
  }

  Json(serde_json::json!({
      "path": normalized,
      "shares": shares,
  }))
  .into_response()
}

// ---------------------------------------------------------------------------
// DELETE /files/shares
// ---------------------------------------------------------------------------

/// Revoke a share by removing a permission link.
pub async fn unshare(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Json(body): Json<UnshareRequest>,
) -> Response {
  // Parse and validate caller identity
  let caller_id = match Uuid::parse_str(&claims.sub) {
    Ok(id) => id,
    Err(_) => {
      return ErrorResponse::new("Invalid user identity").with_status(StatusCode::FORBIDDEN).into_response();
    }
  };

  // Only root can unshare for now
  if !is_root(&caller_id) {
    return ErrorResponse::new("Only root can revoke shares").with_status(StatusCode::FORBIDDEN).into_response();
  }

  let normalized = normalize_path(&body.path);
  let ctx = RequestContext::system();
  match PermissionStore::new(&state.engine).revoke_path(&ctx, &normalized, &body.group, body.path_pattern.as_deref()) {
    Ok(PermissionRevokeResult::Revoked) => {}
    Ok(PermissionRevokeResult::PermissionFileNotFound) => {
      return ErrorResponse::new("No permissions found for this path").with_status(StatusCode::NOT_FOUND).into_response();
    }
    Ok(PermissionRevokeResult::LinkNotFound) => {
      return ErrorResponse::new("No matching permission link found").with_status(StatusCode::NOT_FOUND).into_response();
    }
    Err(error) => return engine_error_response("Failed to update permissions", &error),
  }

  state.event_bus.emit(crate::engine::engine_event::EngineEvent::for_groups(
    crate::engine::engine_event::EVENT_FILES_UNSHARED,
    &claims.sub,
    vec![body.group.clone()],
    serde_json::json!({
      "path": normalized,
      "action": "refresh",
    }),
  ));

  Json(serde_json::json!({
      "revoked": true,
      "group": body.group,
  }))
  .into_response()
}

// ---------------------------------------------------------------------------
// GET /files/shared-with-me — find all paths where the user has permissions
// ---------------------------------------------------------------------------

/// Return paths where the calling user has at least one matching group.
/// Used by the file browser to discover accessible entry points for
/// non-root users.
pub async fn shared_with_me(State(state): State<AppState>, Extension(claims): Extension<TokenClaims>) -> Response {
  // Share tokens don't use .aeordb-permissions; they have scoped key rules.
  if let Err(response) = reject_share_key(&claims, "Not available for share links") {
    return response.into_response();
  }

  let caller_id = match parse_user_id(&claims, "Invalid identity") {
    Ok(id) => id,
    Err(response) => return response.into_response(),
  };

  // Root sees everything — no need for this endpoint
  if is_root(&caller_id) {
    return Json(serde_json::json!({ "paths": [] })).into_response();
  }

  // Get the user's group memberships
  let user_groups = match state.group_cache.get(&caller_id, &state.engine) {
    Ok(groups) => groups,
    Err(error) => return engine_error_response("Failed to load sharing authority", &error),
  };

  if user_groups.is_empty() {
    return Json(serde_json::json!({ "paths": [] })).into_response();
  }

  let ops = DirectoryOps::new(&state.engine);
  let grants_index = match state.engine.grants_index_cache.get(&(), &state.engine) {
    Ok(index) => index,
    Err(error) => return engine_error_response("Failed to load grants authority", &error),
  };

  let mut shared_paths: Vec<serde_json::Value> = Vec::new();

  // Collect EVERY grant matching the user's groups — one user may have
  // multiple shares in the same directory (e.g. share-file-A and
  // share-file-B with different path_patterns).
  for group in &user_groups {
    let Some(records) = grants_index.by_group.get(group) else {
      continue;
    };
    for grant in records {
      // For file-pattern shares, look up the file's metadata so the
      // client can render a real preview/listing entry instead of a
      // placeholder.
      let metadata = if let Some(ref pattern) = grant.path_pattern {
        let file_path = if grant.dir_path == "/" { format!("/{}", pattern) } else { format!("{}/{}", grant.dir_path, pattern) };
        match ops.get_metadata(&file_path) {
          Ok(Some(fr)) => Some(serde_json::json!({
              "size": fr.total_size,
              "created_at": fr.created_at,
              "updated_at": fr.updated_at,
              "content_type": fr.content_type,
          })),
          Ok(None) | Err(EngineError::NotFound(_)) => None,
          Err(error) => return engine_error_response("Failed to load shared-file metadata", &error),
        }
      } else {
        None
      };

      let mut entry_value = serde_json::json!({
          "path": grant.dir_path.clone(),
          "permissions": grant.allow.clone(),
          "path_pattern": grant.path_pattern.clone(),
      });
      if let Some(meta) = metadata {
        if let Some(obj) = entry_value.as_object_mut() {
          if let Some(meta_obj) = meta.as_object() {
            for (k, v) in meta_obj {
              obj.insert(k.clone(), v.clone());
            }
          }
        }
      }
      shared_paths.push(entry_value);
    }
  }

  Json(serde_json::json!({ "paths": shared_paths })).into_response()
}

// ---------------------------------------------------------------------------
// Background email notifications
// ---------------------------------------------------------------------------

async fn send_share_notifications(
  engine: &crate::engine::storage_engine::StorageEngine,
  sharer_name: &str,
  user_ids: &[String],
  paths: &[String],
  permissions: &str,
  public_base_url: Option<&str>,
) {
  // Load email config — if not configured, silently skip
  let config = match crate::engine::email_config::load_email_config(engine) {
    Ok(Some(c)) => c,
    Ok(None) => return,
    Err(error) => {
      tracing::error!("Failed to load share notification email configuration: {error}");
      return;
    }
  };

  // Build the View-Files link once. Without a resolved public base URL
  // the email link would be relative ("/?page=...") and would not work
  // in any mail client — log a warning so the deployment notices.
  let base = match public_base_url {
    Some(b) if !b.is_empty() => b.trim_end_matches('/').to_string(),
    _ => {
      tracing::warn!(
        "Share email link will be relative — could not derive a public base URL from request headers. \
                 Set X-Forwarded-Proto + X-Forwarded-Host at your reverse proxy, or ensure Host is reachable."
      );
      String::new()
    }
  };
  let first_path = paths.first().map(|p| p.as_str()).unwrap_or("/");
  let encoded_path = url_encode_path(first_path);
  let portal_url = format!("{}/?page=files&path={}", base, encoded_path);

  for uid_str in user_ids {
    let uid = match uuid::Uuid::parse_str(uid_str) {
      Ok(id) => id,
      Err(_) => continue,
    };
    let user = match crate::engine::system_store::get_user(engine, &uid) {
      Ok(Some(u)) => u,
      Ok(None) => continue,
      Err(error) => {
        tracing::error!(user_id = %uid, "Failed to load share notification user: {error}");
        continue;
      }
    };
    let email = match user.email {
      Some(ref e) if !e.is_empty() => e.clone(),
      _ => continue,
    };

    let (subject, html, text) = crate::engine::email_template::build_share_notification(sharer_name, paths, permissions, &portal_url);

    if let Err(e) = crate::engine::email_sender::send_email(&config, &email, &subject, &html, &text).await {
      tracing::warn!("Failed to notify {}: {}", email, e);
    }
  }
}

/// URL-encode a path's reserved characters so it survives a query-string
/// transit. Keeps `/`, `-`, `_`, `.`, `~` unencoded so the destination
/// portal can route on the slash structure; everything else gets
/// percent-encoded. (We don't pull in `url` or `percent-encoding` just
/// for this — it's a tiny set of bytes to handle.)
fn url_encode_path(s: &str) -> String {
  let mut out = String::with_capacity(s.len());
  for &b in s.as_bytes() {
    let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~');
    if unreserved {
      out.push(b as char);
    } else {
      out.push_str(&format!("%{:02X}", b));
    }
  }
  out
}

#[cfg(test)]
mod url_resolution_tests {
  use super::*;
  use axum::http::{HeaderName, HeaderValue};

  fn make_headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
      h.insert(HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
    }
    h
  }

  #[test]
  fn forwarded_headers_take_precedence() {
    let h = make_headers(&[("x-forwarded-proto", "https"), ("x-forwarded-host", "aeordb.example.com"), ("host", "internal:6830")]);
    assert_eq!(resolve_public_base_url(&h), Some("https://aeordb.example.com".to_string()));
  }

  #[test]
  fn host_only_assumes_https() {
    let h = make_headers(&[("host", "aeordb.example.com")]);
    assert_eq!(resolve_public_base_url(&h), Some("https://aeordb.example.com".to_string()));
  }

  #[test]
  fn localhost_host_assumes_http() {
    let h = make_headers(&[("host", "localhost:6830")]);
    assert_eq!(resolve_public_base_url(&h), Some("http://localhost:6830".to_string()));
  }

  #[test]
  fn forwarded_proto_first_value_wins_when_multiple() {
    let h = make_headers(&[("x-forwarded-proto", "https,http"), ("x-forwarded-host", "example.com")]);
    assert_eq!(resolve_public_base_url(&h), Some("https://example.com".to_string()));
  }

  #[test]
  fn no_headers_returns_none() {
    let h = HeaderMap::new();
    assert_eq!(resolve_public_base_url(&h), None);
  }

  #[test]
  fn encodes_spaces_and_special_chars_in_path() {
    assert_eq!(url_encode_path("/Pictures/My Folder"), "/Pictures/My%20Folder");
    assert_eq!(url_encode_path("/a&b"), "/a%26b");
    assert_eq!(url_encode_path("/q=?#"), "/q%3D%3F%23");
    assert_eq!(url_encode_path("/safe-chars_.~"), "/safe-chars_.~");
    assert_eq!(url_encode_path("/"), "/");
  }
}
