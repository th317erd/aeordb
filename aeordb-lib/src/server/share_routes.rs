use axum::{
    Extension,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::responses::ErrorResponse;
use super::state::AppState;
use crate::auth::TokenClaims;
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::permissions::{PathPermissions, PermissionLink};
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
pub async fn share(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Json(body): Json<ShareRequest>,
) -> Response {
    // Parse and validate caller identity
    let caller_id = match Uuid::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(_) => {
            return ErrorResponse::new("Invalid user identity")
                .with_status(StatusCode::FORBIDDEN)
                .into_response();
        }
    };

    // Resolve the caller's display name for notifications
    let sharer_name = if is_root(&caller_id) {
        "Root".to_string()
    } else {
        crate::engine::system_store::get_user(&state.engine, &caller_id)
            .ok().flatten()
            .map(|u| u.username)
            .unwrap_or_else(|| "Someone".to_string())
    };

    // Only root can share for now
    if !is_root(&caller_id) {
        return ErrorResponse::new("Only root can share files")
            .with_status(StatusCode::FORBIDDEN)
            .into_response();
    }

    if body.paths.is_empty() {
        return ErrorResponse::new("At least one path is required")
            .with_status(StatusCode::BAD_REQUEST)
            .into_response();
    }

    let has_users = body.users.as_ref().map_or(false, |u| !u.is_empty());
    let has_groups = body.groups.as_ref().map_or(false, |g| !g.is_empty());
    if !has_users && !has_groups {
        return ErrorResponse::new("At least one user or group is required")
            .with_status(StatusCode::BAD_REQUEST)
            .into_response();
    }

    // Validate the permissions string (must be 8 chars of crudlify pattern)
    if body.permissions.len() != 8 {
        return ErrorResponse::new("permissions must be exactly 8 characters (crudlify pattern)")
            .with_status(StatusCode::BAD_REQUEST)
            .into_response();
    }

    let ops = DirectoryOps::new(&state.engine);
    let ctx = RequestContext::system();

    let mut shared_count = 0usize;
    let mut shared_paths: Vec<String> = Vec::new();

    for raw_path in &body.paths {
        let normalized = normalize_path(raw_path);

        // Determine whether this is a file or directory.
        // Try reading as a file first; if NotFound, check as directory.
        let is_file = ops.read_file(&normalized).is_ok();
        let is_dir = if !is_file {
            ops.list_directory(&normalized).is_ok()
        } else {
            false
        };

        if !is_file && !is_dir {
            return ErrorResponse::new(format!("Path not found: {}", normalized))
                .with_status(StatusCode::NOT_FOUND)
                .into_response();
        }

        // For files: store permission on parent dir with path_pattern = filename
        // For dirs:  store permission on the dir itself with no path_pattern
        let (perm_dir, path_pattern) = if is_file {
            let parent = parent_path(&normalized).unwrap_or_else(|| "/".to_string());
            let fname = file_name(&normalized).unwrap_or("").to_string();
            (parent, Some(fname))
        } else {
            (normalized.clone(), None)
        };

        // Read existing .permissions or start empty
        let perm_file_path = if perm_dir == "/" || perm_dir.ends_with('/') {
            format!("{}.permissions", perm_dir)
        } else {
            format!("{}/.permissions", perm_dir)
        };

        let mut perms = match ops.read_file(&perm_file_path) {
            Ok(data) => match PathPermissions::deserialize(&data) {
                Ok(p) => p,
                Err(_) => PathPermissions { links: Vec::new() },
            },
            Err(_) => PathPermissions { links: Vec::new() },
        };

        // Build the list of groups to add links for
        let mut target_groups: Vec<String> = Vec::new();

        if let Some(ref users) = body.users {
            for user_id_str in users {
                target_groups.push(format!("user:{}", user_id_str));
            }
        }
        if let Some(ref groups) = body.groups {
            for group_name in groups {
                target_groups.push(group_name.clone());
            }
        }

        // Upsert links
        for group in &target_groups {
            let existing = perms.links.iter_mut().find(|link| {
                link.group == *group && link.path_pattern == path_pattern
            });

            match existing {
                Some(link) => {
                    // Update existing link
                    link.allow = body.permissions.clone();
                }
                None => {
                    // Insert new link
                    perms.links.push(PermissionLink {
                        group: group.clone(),
                        allow: body.permissions.clone(),
                        deny: "........".to_string(),
                        others_allow: None,
                        others_deny: None,
                        path_pattern: path_pattern.clone(),
                    });
                }
            }
        }

        // Write back the .permissions file
        let serialized = perms.serialize();
        if let Err(e) = ops.store_file(&ctx, &perm_file_path, &serialized, Some("application/json")) {
            return ErrorResponse::new(format!("Failed to store permissions: {}", e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }

        // Evict cache for this directory
        state.permissions_cache.evict_path(&perm_dir);

        shared_count += 1;
        shared_paths.push(normalized);
    }

    // Spawn background email notification (best-effort)
    let engine_clone = state.engine.clone();
    let notify_paths = body.paths.clone();
    let notify_permissions = body.permissions.clone();
    let notify_users: Vec<String> = body.users.clone().unwrap_or_default();
    let sharer = sharer_name.clone();
    tokio::spawn(async move {
        send_share_notifications(&engine_clone, &sharer, &notify_users, &notify_paths, &notify_permissions).await;
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
    Extension(_claims): Extension<TokenClaims>,
    axum::extract::Query(query): axum::extract::Query<SharesQuery>,
) -> Response {
    let normalized = normalize_path(&query.path);
    let ops = DirectoryOps::new(&state.engine);

    // Determine perm_dir: if path is a file, look at parent
    let is_file = ops.read_file(&normalized).is_ok();
    let perm_dir = if is_file {
        parent_path(&normalized).unwrap_or_else(|| "/".to_string())
    } else {
        normalized.clone()
    };

    let perm_file_path = if perm_dir == "/" || perm_dir.ends_with('/') {
        format!("{}.permissions", perm_dir)
    } else {
        format!("{}/.permissions", perm_dir)
    };

    let perms = match ops.read_file(&perm_file_path) {
        Ok(data) => match PathPermissions::deserialize(&data) {
            Ok(p) => p,
            Err(_) => PathPermissions { links: Vec::new() },
        },
        Err(_) => PathPermissions { links: Vec::new() },
    };

    // If the query is for a specific file, filter to links with matching path_pattern
    let file_filter = if is_file {
        file_name(&normalized).map(|s| s.to_string())
    } else {
        None
    };

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
                    _ => None,
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
            return ErrorResponse::new("Invalid user identity")
                .with_status(StatusCode::FORBIDDEN)
                .into_response();
        }
    };

    // Only root can unshare for now
    if !is_root(&caller_id) {
        return ErrorResponse::new("Only root can revoke shares")
            .with_status(StatusCode::FORBIDDEN)
            .into_response();
    }

    let normalized = normalize_path(&body.path);
    let ops = DirectoryOps::new(&state.engine);
    let ctx = RequestContext::system();

    // Determine perm_dir
    let is_file = ops.read_file(&normalized).is_ok();
    let perm_dir = if is_file {
        parent_path(&normalized).unwrap_or_else(|| "/".to_string())
    } else {
        normalized.clone()
    };

    let perm_file_path = if perm_dir == "/" || perm_dir.ends_with('/') {
        format!("{}.permissions", perm_dir)
    } else {
        format!("{}/.permissions", perm_dir)
    };

    let mut perms = match ops.read_file(&perm_file_path) {
        Ok(data) => match PathPermissions::deserialize(&data) {
            Ok(p) => p,
            Err(_) => {
                return ErrorResponse::new("No permissions found for this path")
                    .with_status(StatusCode::NOT_FOUND)
                    .into_response();
            }
        },
        Err(_) => {
            return ErrorResponse::new("No permissions found for this path")
                .with_status(StatusCode::NOT_FOUND)
                .into_response();
        }
    };

    let original_len = perms.links.len();
    perms.links.retain(|link| {
        !(link.group == body.group && link.path_pattern == body.path_pattern)
    });

    if perms.links.len() == original_len {
        return ErrorResponse::new("No matching permission link found")
            .with_status(StatusCode::NOT_FOUND)
            .into_response();
    }

    // Write back
    let serialized = perms.serialize();
    if let Err(e) = ops.store_file(&ctx, &perm_file_path, &serialized, Some("application/json")) {
        return ErrorResponse::new(format!("Failed to update permissions: {}", e))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
    }

    // Evict cache
    state.permissions_cache.evict_path(&perm_dir);

    Json(serde_json::json!({
        "revoked": true,
        "group": body.group,
    }))
    .into_response()
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
) {
    // Load email config — if not configured, silently skip
    let config = match crate::engine::email_config::load_email_config(engine) {
        Ok(Some(c)) => c,
        _ => return,
    };

    for uid_str in user_ids {
        let uid = match uuid::Uuid::parse_str(uid_str) {
            Ok(id) => id,
            Err(_) => continue,
        };
        let user = match crate::engine::system_store::get_user(engine, &uid) {
            Ok(Some(u)) => u,
            _ => continue,
        };
        let email = match user.email {
            Some(ref e) if !e.is_empty() => e.clone(),
            _ => continue,
        };

        let portal_url = format!(
            "/system/portal/?page=files&path={}",
            paths.first().map(|p| p.as_str()).unwrap_or("/"),
        );
        let (subject, html, text) = crate::engine::email_template::build_share_notification(
            sharer_name, paths, permissions, &portal_url,
        );

        if let Err(e) = crate::engine::email_sender::send_email(&config, &email, &subject, &html, &text).await {
            tracing::warn!("Failed to notify {}: {}", email, e);
        }
    }
}
