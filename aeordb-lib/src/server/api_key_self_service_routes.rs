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
use crate::auth::api_key::{generate_api_key, hash_api_key, ApiKeyRecord, DEFAULT_EXPIRY_DAYS, MAX_EXPIRY_DAYS};
use crate::engine::api_key_rules::{parse_rules_from_json, validate_rules};
use crate::engine::user::is_root;

#[derive(Deserialize)]
pub struct CreateKeyRequest {
  pub label: Option<String>,
  pub expires_in_days: Option<i64>,
  pub rules: Option<serde_json::Value>,
  /// Root-only: create a key for another user.
  pub user_id: Option<String>,
}

/// POST /api-keys -- create an API key for the calling user (or another user if root).
pub async fn create_own_key(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Json(payload): Json<CreateKeyRequest>,
) -> Response {
  let caller_id = match Uuid::parse_str(&claims.sub) {
    Ok(id) => id,
    Err(_) => {
      return ErrorResponse::new("Invalid sub claim in JWT: token 'sub' is not a valid UUID. Re-authenticate via POST /auth/token")
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  // Determine the target user for this key.
  let target_user_id = if let Some(ref uid_string) = payload.user_id {
    // Only root can create keys for other users.
    if !is_root(&caller_id) {
      return ErrorResponse::new("Only root can create keys for other users. Omit the 'user_id' field to create a key for yourself")
        .with_status(StatusCode::FORBIDDEN)
        .into_response();
    }
    match Uuid::parse_str(uid_string) {
      Ok(id) => id,
      Err(_) => {
        return ErrorResponse::new(format!("Invalid user_id '{}': must be a valid UUID", uid_string))
          .with_status(StatusCode::BAD_REQUEST)
          .into_response();
      }
    }
  } else {
    caller_id
  };

  // Parse and validate rules if present.
  let rules = if let Some(ref rules_json) = payload.rules {
    match parse_rules_from_json(rules_json) {
      Ok(parsed) => {
        if let Err(err) = validate_rules(&parsed) {
          return ErrorResponse::new(format!("Invalid rules: {}. Rules must include 'path_pattern' and 'permitted' fields", err))
            .with_status(StatusCode::BAD_REQUEST)
            .into_response();
        }
        parsed
      }
      Err(err) => {
        return ErrorResponse::new(format!("Invalid rules JSON: {}. Expected an array of {{\"path_pattern\": \"...\", \"permitted\": \"rwld\"}}", err))
          .with_status(StatusCode::BAD_REQUEST)
          .into_response();
      }
    }
  } else {
    vec![]
  };

  // Clamp expiry to MAX_EXPIRY_DAYS, default to DEFAULT_EXPIRY_DAYS.
  let days = payload.expires_in_days.unwrap_or(DEFAULT_EXPIRY_DAYS);
  let clamped_days = days.clamp(1, MAX_EXPIRY_DAYS);

  let now_millis = chrono::Utc::now().timestamp_millis();
  let expires_at = now_millis + (clamped_days * 24 * 60 * 60 * 1000);

  let key_id = Uuid::new_v4();
  let plaintext_key = generate_api_key(key_id);
  let key_hash = match hash_api_key(&plaintext_key) {
    Ok(hash) => hash,
    Err(error) => {
      tracing::error!("Failed to hash API key: {}", error);
      return ErrorResponse::new("Failed to create API key: could not hash the generated key. If this persists, contact your administrator")
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  let record = ApiKeyRecord {
    key_id,
    key_hash,
    user_id: target_user_id,
    created_at: chrono::Utc::now(),
    is_revoked: false,
    expires_at,
    label: payload.label.clone(),
    rules: rules.clone(),
  };

  // Root users need the bootstrap path to bypass nil-UUID validation.
  let store_result = if is_root(&target_user_id) {
    state.auth_provider.store_api_key_for_bootstrap(&record)
  } else {
    state.auth_provider.store_api_key(&record)
  };

  if let Err(error) = store_result {
    tracing::error!("Failed to store API key: {}", error);
    return ErrorResponse::new("Failed to store API key: could not persist to storage. If this persists, check GET /system/health for system status")
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response();
  }

  let rules_json = serde_json::to_value(&rules).unwrap_or(serde_json::json!([]));

  (
    StatusCode::CREATED,
    Json(serde_json::json!({
      "key_id": record.key_id,
      "key": plaintext_key,
      "label": record.label,
      "expires_at": record.expires_at,
      "rules": rules_json,
      "user_id": record.user_id,
    })),
  )
    .into_response()
}

/// GET /api-keys -- list the calling user's own API keys.
pub async fn list_own_keys(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
) -> Response {
  let caller_id = match Uuid::parse_str(&claims.sub) {
    Ok(id) => id,
    Err(_) => {
      return ErrorResponse::new("Invalid sub claim in JWT: token 'sub' is not a valid UUID. Re-authenticate via POST /auth/token")
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  match state.auth_provider.list_api_keys() {
    Ok(keys) => {
      // Resolve caller's username once
      let caller_username = crate::engine::system_store::get_user(&state.engine, &caller_id)
        .ok()
        .flatten()
        .map(|u| u.username)
        .unwrap_or_else(|| {
          if crate::engine::user::is_root(&caller_id) { "root".to_string() }
          else { caller_id.to_string() }
        });

      let own_keys: Vec<serde_json::Value> = keys
        .iter()
        .filter(|record| record.user_id == caller_id)
        .map(|record| {
          serde_json::json!({
            "key_id": record.key_id,
            "user_id": record.user_id,
            "username": caller_username,
            "created_at": record.created_at.to_rfc3339(),
            "is_revoked": record.is_revoked,
            "expires_at": record.expires_at,
            "label": record.label,
            "rules": record.rules,
          })
        })
        .collect();
      (StatusCode::OK, Json(own_keys)).into_response()
    }
    Err(error) => {
      tracing::error!("Failed to list API keys: {}", error);
      ErrorResponse::new("Failed to list API keys: could not read from storage. If this persists, check GET /system/health for system status")
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// DELETE /api-keys/{key_id} -- revoke one of the calling user's own API keys.
pub async fn revoke_own_key(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(key_id): Path<String>,
) -> Response {
  let caller_id = match Uuid::parse_str(&claims.sub) {
    Ok(id) => id,
    Err(_) => {
      return ErrorResponse::new("Invalid sub claim in JWT: token 'sub' is not a valid UUID. Re-authenticate via POST /auth/token")
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  let parsed_key_id = match Uuid::parse_str(&key_id) {
    Ok(id) => id,
    Err(_) => {
      return ErrorResponse::new(format!("Invalid key ID '{}': must be a valid UUID", key_id))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  };

  // Load all keys and find the matching one to verify ownership.
  let keys = match state.auth_provider.list_api_keys() {
    Ok(keys) => keys,
    Err(error) => {
      tracing::error!("Failed to list API keys: {}", error);
      return ErrorResponse::new("Failed to look up API key: could not read from storage. If this persists, check GET /system/health for system status")
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  let target_key = keys.iter().find(|record| record.key_id == parsed_key_id);

  match target_key {
    None => {
      ErrorResponse::new(format!("API key not found: {}", parsed_key_id))
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Some(record) => {
      // Non-root users can only revoke their own keys.
      if record.user_id != caller_id && !is_root(&caller_id) {
        return ErrorResponse::new("Cannot revoke another user's key. You can only revoke keys you own")
          .with_status(StatusCode::FORBIDDEN)
          .into_response();
      }

      match state.auth_provider.revoke_api_key(parsed_key_id) {
        Ok(true) => {
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
          ErrorResponse::new(format!("API key not found: {}", parsed_key_id))
            .with_status(StatusCode::NOT_FOUND)
            .into_response()
        }
        Err(error) => {
          tracing::error!("Failed to revoke API key: {}", error);
          ErrorResponse::new("Failed to revoke API key: could not persist revocation to storage. If this persists, check GET /system/health for system status")
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response()
        }
      }
    }
  }
}

/// GET /auth/keys/users — list users the caller can create keys for.
///
/// Root: returns all users (user_id + username only — minimal data).
/// Non-root: returns only the caller (they can only create keys for themselves).
pub async fn list_key_assignable_users(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
) -> Response {
  let caller_id = match Uuid::parse_str(&claims.sub) {
    Ok(id) => id,
    Err(_) => {
      return ErrorResponse::new("Invalid sub claim in JWT")
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  if is_root(&caller_id) {
    // Root sees all users
    match crate::engine::system_store::list_users(&state.engine) {
      Ok(users) => {
        let mut items: Vec<serde_json::Value> = users
          .iter()
          .map(|u| serde_json::json!({
            "user_id": u.user_id,
            "username": u.username,
          }))
          .collect();

        // Include root itself (not in the user store)
        items.insert(0, serde_json::json!({
          "user_id": Uuid::nil(),
          "username": "root",
        }));

        (StatusCode::OK, Json(serde_json::json!({ "items": items }))).into_response()
      }
      Err(e) => {
        tracing::error!("Failed to list users for key assignment: {}", e);
        ErrorResponse::new("Failed to list users")
          .with_status(StatusCode::INTERNAL_SERVER_ERROR)
          .into_response()
      }
    }
  } else {
    // Non-root sees only themselves
    let username = crate::engine::system_store::get_user(&state.engine, &caller_id)
      .ok()
      .flatten()
      .map(|u| u.username)
      .unwrap_or_else(|| caller_id.to_string());

    (StatusCode::OK, Json(serde_json::json!({
      "items": [{
        "user_id": caller_id,
        "username": username,
      }]
    }))).into_response()
  }
}
