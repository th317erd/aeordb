use axum::{
    Extension,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use uuid::Uuid;

use super::responses::ErrorResponse;
use super::state::AppState;
use crate::auth::TokenClaims;
use crate::auth::api_key::{generate_api_key, hash_api_key, ApiKeyRecord, NO_EXPIRY_SENTINEL, MAX_EXPIRY_DAYS};
use crate::engine::api_key_rules::KeyRule;
use crate::engine::path_utils::normalize_path;
use crate::engine::user::is_root;

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateShareLinkRequest {
    pub paths: Vec<String>,
    pub permissions: String,
    pub expires_in_days: Option<i64>,
    pub base_url: Option<String>,
}

#[derive(Deserialize)]
pub struct ShareLinksQuery {
    pub path: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Minimal percent-encoding for URL query parameter values.
fn simple_url_encode(input: &str) -> String {
    let mut result = String::with_capacity(input.len() * 2);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                result.push(byte as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    result
}

fn permission_label(perms: &str) -> &str {
    match perms {
        "-r--l---" => "View only",
        "crudl..." => "Can edit",
        "crudlify" => "Full access",
        _ => "Custom",
    }
}

// ---------------------------------------------------------------------------
// POST /files/share-link
// ---------------------------------------------------------------------------

/// Create a share link by minting an API key with path-scoped rules and
/// returning a pre-authenticated portal URL.
pub async fn create_share_link(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Json(body): Json<CreateShareLinkRequest>,
) -> Response {
    // 1. Validate caller is root.
    let caller_id = match Uuid::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(_) => {
            return ErrorResponse::new("Invalid user identity")
                .with_status(StatusCode::FORBIDDEN)
                .into_response();
        }
    };

    if !is_root(&caller_id) {
        return ErrorResponse::new("Only root can create share links")
            .with_status(StatusCode::FORBIDDEN)
            .into_response();
    }

    // 2. Validate inputs.
    if body.paths.is_empty() {
        return ErrorResponse::new("At least one path is required")
            .with_status(StatusCode::BAD_REQUEST)
            .into_response();
    }

    if body.permissions.len() != 8 {
        return ErrorResponse::new("permissions must be exactly 8 characters (crudlify pattern)")
            .with_status(StatusCode::BAD_REQUEST)
            .into_response();
    }

    // 2b. Block sharing of system paths.
    for raw_path in &body.paths {
        let normalized = normalize_path(raw_path);
        if normalized.starts_with("/.system") {
            return ErrorResponse::new("Cannot share system paths")
                .with_status(StatusCode::BAD_REQUEST)
                .into_response();
        }
    }

    // 3. Build rules: one rule per shared path, then deny-all fallback.
    //    The permission middleware uses ancestor-aware matching for share keys,
    //    so parent directories are automatically navigable without explicit rules.
    let mut rules: Vec<KeyRule> = Vec::with_capacity(body.paths.len() + 1);
    for path in &body.paths {
        if path.ends_with('/') {
            // Directory: allow everything inside + the directory itself
            rules.push(KeyRule {
                glob: format!("{}**", path),
                permitted: body.permissions.clone(),
            });
        } else {
            // File: just the exact file
            rules.push(KeyRule {
                glob: path.clone(),
                permitted: body.permissions.clone(),
            });
        }
    }
    // Deny-all fallback.
    rules.push(KeyRule {
        glob: "**".to_string(),
        permitted: "--------".to_string(),
    });

    // 4. Calculate expiration.
    let now_millis = chrono::Utc::now().timestamp_millis();
    let expires_at = match body.expires_in_days {
        None => NO_EXPIRY_SENTINEL,
        Some(days) => {
            let clamped = days.clamp(1, MAX_EXPIRY_DAYS);
            now_millis + (clamped * 24 * 60 * 60 * 1000)
        }
    };

    // 5. Create API key record.
    let key_id = Uuid::new_v4();
    let plaintext_key = generate_api_key(key_id);
    let key_hash = match hash_api_key(&plaintext_key) {
        Ok(hash) => hash,
        Err(error) => {
            tracing::error!("Failed to hash share link API key: {}", error);
            return ErrorResponse::new("Failed to create share link: could not hash the generated key")
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
    };

    let first_path = body.paths.first().cloned().unwrap_or_default();
    let perm_label = permission_label(&body.permissions);

    let record = ApiKeyRecord {
        key_id,
        key_hash,
        user_id: None,
        created_at: chrono::Utc::now(),
        is_revoked: false,
        expires_at,
        label: Some(format!("Share: {} ({})", first_path, perm_label)),
        rules: rules.clone(),
    };

    // 6. Store the key. user_id is None so store_api_key works fine
    //    (the nil-UUID guard only fires for Some(uid)).
    if let Err(error) = state.auth_provider.store_api_key(&record) {
        tracing::error!("Failed to store share link API key: {}", error);
        return ErrorResponse::new("Failed to create share link: could not persist the key to storage")
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
    }

    // 7. Build JWT claims.
    let now_seconds = chrono::Utc::now().timestamp();
    let jwt_exp = if expires_at == NO_EXPIRY_SENTINEL {
        // Cap to ~100 years in seconds from now — effectively never expires.
        now_seconds + (100 * 365 * 24 * 3600)
    } else {
        expires_at / 1000
    };

    let jwt_claims = TokenClaims {
        sub: format!("share:{}", key_id),
        iss: "aeordb".to_string(),
        iat: now_seconds,
        exp: jwt_exp,
        scope: None,
        permissions: None,
        key_id: Some(key_id.to_string()),
    };

    // 8. Create token.
    let token = match state.jwt_manager.create_token(&jwt_claims) {
        Ok(token) => token,
        Err(error) => {
            tracing::error!("Failed to create JWT for share link: {}", error);
            return ErrorResponse::new("Failed to create share link: could not generate token")
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
    };

    // 9. Build URL.
    let base = body.base_url.unwrap_or_default();
    let encoded_path = simple_url_encode(&first_path);
    let url = format!(
        "{}/system/portal/?token={}&path={}&perm={}",
        base.trim_end_matches('/'),
        token,
        encoded_path,
        body.permissions,
    );

    // 10. Return response.
    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "url": url,
            "token": token,
            "key_id": key_id,
            "permissions": body.permissions,
            "expires_at": expires_at,
            "paths": body.paths,
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// GET /files/share-links?path=...
// ---------------------------------------------------------------------------

/// List active share links, optionally filtered by path.
pub async fn list_share_links(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    axum::extract::Query(query): axum::extract::Query<ShareLinksQuery>,
) -> Response {
    // Validate caller is root.
    let caller_id = match Uuid::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(_) => {
            return ErrorResponse::new("Invalid user identity")
                .with_status(StatusCode::FORBIDDEN)
                .into_response();
        }
    };

    if !is_root(&caller_id) {
        return ErrorResponse::new("Only root can list share links")
            .with_status(StatusCode::FORBIDDEN)
            .into_response();
    }

    // List all API keys and filter to share keys (user_id is None, not revoked).
    let all_keys = match state.auth_provider.list_api_keys() {
        Ok(keys) => keys,
        Err(error) => {
            tracing::error!("Failed to list API keys for share links: {}", error);
            return ErrorResponse::new("Failed to list share links: could not read from storage")
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
    };

    let now_millis = chrono::Utc::now().timestamp_millis();

    let share_keys: Vec<&ApiKeyRecord> = all_keys
        .iter()
        .filter(|k| k.user_id.is_none() && !k.is_revoked && k.expires_at > now_millis)
        .collect();

    // Optionally filter by path: include keys whose rules contain a glob
    // that matches or contains the query path.
    let filtered: Vec<&ApiKeyRecord> = if let Some(ref path) = query.path {
        share_keys
            .into_iter()
            .filter(|k| {
                k.rules.iter().any(|rule| {
                    // Skip the deny-all fallback rule.
                    if rule.permitted == "--------" {
                        return false;
                    }
                    // Check if the rule's glob starts with the query path
                    // or the path is a prefix of the glob's base.
                    rule.glob.starts_with(path.as_str())
                        || path.starts_with(rule.glob.trim_end_matches("/**").trim_end_matches("**"))
                })
            })
            .collect()
    } else {
        share_keys
    };

    let links: Vec<serde_json::Value> = filtered
        .iter()
        .map(|k| {
            let permissions = k
                .rules
                .first()
                .map(|r| r.permitted.as_str())
                .unwrap_or("--------");
            let paths: Vec<&str> = k
                .rules
                .iter()
                .filter(|r| r.permitted != "--------" && r.glob.ends_with("**"))
                .map(|r| r.glob.trim_end_matches("/**").trim_end_matches("**"))
                .collect();

            serde_json::json!({
                "key_id": k.key_id,
                "label": k.label,
                "permissions": permissions,
                "expires_at": k.expires_at,
                "created_at": k.created_at.to_rfc3339(),
                "paths": paths,
            })
        })
        .collect();

    let response_path = query.path.unwrap_or_default();

    Json(serde_json::json!({
        "path": response_path,
        "links": links,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// DELETE /files/share-links/{key_id}
// ---------------------------------------------------------------------------

/// Revoke a share link by revoking its backing API key.
pub async fn revoke_share_link(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Path(key_id): Path<String>,
) -> Response {
    // 1. Validate caller is root.
    let caller_id = match Uuid::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(_) => {
            return ErrorResponse::new("Invalid user identity")
                .with_status(StatusCode::FORBIDDEN)
                .into_response();
        }
    };

    if !is_root(&caller_id) {
        return ErrorResponse::new("Only root can revoke share links")
            .with_status(StatusCode::FORBIDDEN)
            .into_response();
    }

    // 2. Parse key_id.
    let parsed_key_id = match Uuid::parse_str(&key_id) {
        Ok(id) => id,
        Err(_) => {
            return ErrorResponse::new(format!("Invalid key ID '{}': must be a valid UUID", key_id))
                .with_status(StatusCode::BAD_REQUEST)
                .into_response();
        }
    };

    // 3. Verify it is a share key (user_id is None).
    let all_keys = match state.auth_provider.list_api_keys() {
        Ok(keys) => keys,
        Err(error) => {
            tracing::error!("Failed to list API keys: {}", error);
            return ErrorResponse::new("Failed to revoke share link: could not read from storage")
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
    };

    let target = all_keys.iter().find(|k| k.key_id == parsed_key_id);
    match target {
        None => {
            return ErrorResponse::new(format!("Share link not found: {}", parsed_key_id))
                .with_status(StatusCode::NOT_FOUND)
                .into_response();
        }
        Some(record) => {
            if record.user_id.is_some() {
                return ErrorResponse::new("This key is not a share link; use DELETE /auth/keys/{key_id} instead")
                    .with_status(StatusCode::BAD_REQUEST)
                    .into_response();
            }
        }
    }

    // 4. Revoke.
    match state.auth_provider.revoke_api_key(parsed_key_id) {
        Ok(true) => {
            // 5. Invalidate cache.
            state.api_key_cache.invalidate(&parsed_key_id.to_string());

            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "revoked": true,
                    "key_id": parsed_key_id,
                })),
            )
                .into_response()
        }
        Ok(false) => {
            ErrorResponse::new(format!("Share link not found: {}", parsed_key_id))
                .with_status(StatusCode::NOT_FOUND)
                .into_response()
        }
        Err(error) => {
            tracing::error!("Failed to revoke share link: {}", error);
            ErrorResponse::new("Failed to revoke share link: could not persist revocation to storage")
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    }
}
