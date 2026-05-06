use axum::{
  Extension,
  body::Body,
  extract::{Path, Query, State},
  http::StatusCode,
  response::{IntoResponse, Response},
  Json,
};
use serde::Deserialize;
use uuid::Uuid;

use super::responses::ErrorResponse;
use super::state::AppState;
use crate::engine::RequestContext;
use crate::auth::{
  TokenClaims, generate_api_key, hash_api_key, parse_api_key, verify_api_key, ApiKeyRecord, DEFAULT_EXPIRY_DAYS,
  generate_magic_link_code, hash_magic_link_code,
  generate_refresh_token, hash_refresh_token,
};
use crate::auth::magic_link::MagicLinkRecord;
use crate::auth::refresh::{RefreshTokenRecord, DEFAULT_REFRESH_EXPIRY_SECONDS};
use crate::engine::system_store;

pub async fn health_check(
  State(state): State<AppState>,
) -> impl IntoResponse {
  let report = crate::engine::health::full_health_check(
    &state.engine,
    &state.db_path,
    &state.peer_manager,
    state.startup_time,
  );
  // SECURITY: Only expose the top-level status publicly. Detailed checks
  // (engine stats, disk info, peer counts, auth mode) leak internal state
  // that aids attackers. Load balancers only need the status string.
  Json(serde_json::json!({
    "status": report.status,
    "version": env!("CARGO_PKG_VERSION"),
  }))
}

// ---------------------------------------------------------------------------
// Plugin routes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DeployPluginQuery {
  pub name: Option<String>,
  pub plugin_type: Option<String>,
}

/// PUT /plugins/:name — deploy a plugin.
///
/// Accepts the WASM binary as the raw request body.
/// Plugin name comes from the URL path segment.
/// Plugin type comes from the `plugin_type` query parameter (defaults to "wasm").
pub async fn deploy_plugin(
  State(state): State<AppState>,
  Path(name): Path<String>,
  Query(query): Query<DeployPluginQuery>,
  body: axum::body::Bytes,
) -> Response {
  let plugin_path = name.clone();
  let plugin_name = query.name.unwrap_or_else(|| name.clone());

  let plugin_type_string = query.plugin_type.unwrap_or_else(|| "wasm".to_string());
  let plugin_type: crate::plugins::PluginType = match plugin_type_string.parse() {
    Ok(parsed) => parsed,
    Err(error) => {
      return ErrorResponse::new(format!("Invalid plugin type: {}", error))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  };

  if body.is_empty() {
    return ErrorResponse::new("Plugin body must not be empty".to_string())
      .with_status(StatusCode::BAD_REQUEST)
      .into_response();
  }

  match state
    .plugin_manager
    .deploy_plugin(&plugin_name, &plugin_path, plugin_type, body.to_vec())
  {
    Ok(record) => {
      let metadata = record.to_metadata();
      match serde_json::to_value(metadata) {
        Ok(value) => (StatusCode::OK, Json(value)).into_response(),
        Err(e) => ErrorResponse::new(format!("Failed to serialize plugin metadata: {}", e))
          .with_status(StatusCode::INTERNAL_SERVER_ERROR)
          .into_response(),
      }
    }
    Err(crate::plugins::plugin_manager::PluginManagerError::InvalidPlugin(message)) => {
      ErrorResponse::new(format!("Invalid plugin: {}", message))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Failed to deploy plugin: {}", error);
      ErrorResponse::new(format!("Failed to deploy plugin: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// POST /plugins/:name/invoke — invoke a deployed plugin.
///
/// Wraps the raw request body in a `PluginRequest` envelope with metadata,
/// passes it through the WASM runtime with engine context, then deserializes
/// the `PluginResponse` to map status code, content type and headers back
/// to the HTTP response. Falls back to raw bytes if the plugin returns a
/// non-PluginResponse payload (backward compatibility).
pub async fn invoke_plugin(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(name): Path<String>,
  body: axum::body::Bytes,
) -> Response {
  let plugin_path = name.clone();

  // Build a PluginRequest envelope with metadata about the invocation.
  let plugin_request = aeordb_plugin_sdk::PluginRequest {
    arguments: body.to_vec(),
    metadata: {
      let mut meta = std::collections::HashMap::new();
      meta.insert("name".to_string(), name.clone());
      meta.insert(
        "path".to_string(),
        format!("/plugins/{}", name),
      );
      meta.insert("plugin_path".to_string(), plugin_path.clone());
      meta
    },
  };
  let request_bytes = serde_json::to_vec(&plugin_request).unwrap_or_default();

  // Create a RequestContext from the authenticated caller's claims.
  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());

  match state.plugin_manager.invoke_wasm_plugin_with_context(
    &plugin_path,
    &request_bytes,
    state.engine.clone(),
    ctx,
  ) {
    Ok(response_bytes) => {
      // Try to deserialize as a PluginResponse envelope.
      match serde_json::from_slice::<aeordb_plugin_sdk::PluginResponse>(&response_bytes) {
        Ok(plugin_response) => {
          let status = StatusCode::from_u16(plugin_response.status_code)
            .unwrap_or(StatusCode::OK);
          let content_type = plugin_response
            .content_type
            .unwrap_or_else(|| "application/octet-stream".to_string());

          // Allowlist of safe header prefixes/names from plugins.
          // Prevents plugins from setting security-sensitive headers like
          // Set-Cookie, Authorization, Host, etc.
          const SAFE_PLUGIN_HEADERS: &[&str] = &[
            "x-", "cache-control", "etag", "last-modified", "content-disposition",
            "content-language", "content-encoding", "vary",
          ];

          let mut response_builder = axum::http::Response::builder()
            .status(status)
            .header("content-type", content_type);

          for (key, value) in &plugin_response.headers {
            let key_lower = key.to_lowercase();
            let is_safe = SAFE_PLUGIN_HEADERS.iter().any(|prefix| key_lower.starts_with(prefix));
            if is_safe {
              response_builder = response_builder.header(key.as_str(), value.as_str());
            }
          }

          response_builder
            .body(axum::body::Body::from(plugin_response.body))
            .unwrap_or_else(|_| {
              (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response").into_response()
            })
        }
        Err(_) => {
          // Fallback: return raw bytes for backward compatibility with old plugins.
          axum::http::Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/octet-stream")
            .body(axum::body::Body::from(response_bytes))
            .unwrap_or_else(|_| {
              (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response").into_response()
            })
        }
      }
    }
    Err(crate::plugins::plugin_manager::PluginManagerError::NotFound(path)) => {
      ErrorResponse::new(format!("Plugin not found: {}", path))
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Plugin invocation failed: {}", error);
      ErrorResponse::new(format!("Plugin invocation failed: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// GET /plugins — list all deployed plugins.
pub async fn list_plugins(
  State(state): State<AppState>,
) -> Response {
  match state.plugin_manager.list_plugins() {
    Ok(plugins) => match serde_json::to_value(plugins) {
      Ok(value) => (StatusCode::OK, Json(serde_json::json!({"items": value}))).into_response(),
      Err(e) => {
        tracing::error!("Failed to serialize plugins: {}", e);
        ErrorResponse::new(format!("Failed to serialize plugins: {}", e))
          .with_status(StatusCode::INTERNAL_SERVER_ERROR)
          .into_response()
      }
    },
    Err(error) => {
      tracing::error!("Failed to list plugins: {}", error);
      ErrorResponse::new(format!("Failed to list plugins: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// DELETE /plugins/:name — remove a deployed plugin.
pub async fn remove_plugin(
  State(state): State<AppState>,
  Path(name): Path<String>,
) -> Response {
  let plugin_path = name.clone();

  match state.plugin_manager.remove_plugin(&plugin_path) {
    Ok(true) => (
      StatusCode::OK,
      Json(serde_json::json!({ "removed": true, "path": plugin_path })),
    )
      .into_response(),
    Ok(false) => ErrorResponse::new(format!("Plugin not found: {}", plugin_path))
      .with_status(StatusCode::NOT_FOUND)
      .into_response(),
    Err(error) => {
      tracing::error!("Failed to remove plugin: {}", error);
      ErrorResponse::new(format!("Failed to remove plugin: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

// ---------------------------------------------------------------------------
// Auth routes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AuthTokenRequest {
  pub api_key: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateApiKeyRequest {
  pub user_id: Option<String>,
}

/// POST /auth/token -- exchange an API key for a JWT.
/// Parses the key_id from the submitted key for O(1) lookup instead of
/// iterating all keys.
pub async fn auth_token(
  State(state): State<AppState>,
  Json(payload): Json<AuthTokenRequest>,
) -> Response {
  let (key_id_prefix, _full_key) = match parse_api_key(&payload.api_key) {
    Ok(parsed) => parsed,
    Err(_) => {
      metrics::counter!(crate::metrics::definitions::AUTH_TOKEN_EXCHANGES_TOTAL, "result" => "invalid_key").increment(1);
      return ErrorResponse::new("Invalid API key: the key format is not recognized. Keys use the format 'aeor_<key_id>_<secret>'".to_string())
        .with_status(StatusCode::UNAUTHORIZED)
        .into_response();
    }
  };

  let record = match state.auth_provider.get_api_key_by_prefix(&key_id_prefix) {
    Ok(Some(record)) => record,
    Ok(None) => {
      metrics::counter!(crate::metrics::definitions::AUTH_TOKEN_EXCHANGES_TOTAL, "result" => "not_found").increment(1);
      return ErrorResponse::new("Invalid API key: no key found matching the provided key ID prefix".to_string())
        .with_status(StatusCode::UNAUTHORIZED)
        .into_response();
    }
    Err(error) => {
      tracing::error!("Failed to look up API key: {}", error);
      metrics::counter!(crate::metrics::definitions::AUTH_TOKEN_EXCHANGES_TOTAL, "result" => "error").increment(1);
      return ErrorResponse::new("An unexpected error occurred while looking up the API key. If this persists, check GET /system/health for system status".to_string())
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  if record.is_revoked {
    metrics::counter!(crate::metrics::definitions::AUTH_TOKEN_EXCHANGES_TOTAL, "result" => "revoked").increment(1);
    return ErrorResponse::new("Invalid API key: this key has been revoked".to_string())
      .with_status(StatusCode::UNAUTHORIZED)
      .into_response();
  }

  let key_valid = match verify_api_key(&payload.api_key, &record.key_hash) {
    Ok(valid) => valid,
    Err(_) => {
      metrics::counter!(crate::metrics::definitions::AUTH_TOKEN_EXCHANGES_TOTAL, "result" => "invalid_key").increment(1);
      return ErrorResponse::new("Invalid API key: key verification failed".to_string())
        .with_status(StatusCode::UNAUTHORIZED)
        .into_response();
    }
  };

  if !key_valid {
    metrics::counter!(crate::metrics::definitions::AUTH_TOKEN_EXCHANGES_TOTAL, "result" => "invalid_key").increment(1);
    return ErrorResponse::new("Invalid API key: the secret does not match. Check that you are using the full key string".to_string())
      .with_status(StatusCode::UNAUTHORIZED)
      .into_response();
  }

  // Check API key expiry.
  let now_millis = chrono::Utc::now().timestamp_millis();
  if record.expires_at <= now_millis {
    metrics::counter!(crate::metrics::definitions::AUTH_TOKEN_EXCHANGES_TOTAL, "result" => "expired").increment(1);
    return ErrorResponse::new("API key expired. Create a new key via POST /auth/api-keys".to_string())
      .with_status(StatusCode::UNAUTHORIZED)
      .into_response();
  }

  let now = chrono::Utc::now().timestamp();
  // Cap JWT expiry to the lesser of DEFAULT_EXPIRY_SECONDS and the key's remaining lifetime.
  let key_expires_seconds = (record.expires_at / 1000) - now;
  let jwt_expiry = std::cmp::min(crate::auth::jwt::DEFAULT_EXPIRY_SECONDS, key_expires_seconds.max(0));
  let claims = TokenClaims {
    sub: match record.user_id {
      Some(uid) => uid.to_string(),
      None => format!("share:{}", record.key_id),
    },
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + jwt_expiry,
    scope: None,
    permissions: None,
    key_id: Some(record.key_id.to_string()),
  };

  let token = match state.jwt_manager.create_token(&claims) {
    Ok(token) => token,
    Err(error) => {
      tracing::error!("Failed to create JWT: {}", error);
      return ErrorResponse::new("Failed to create JWT during token exchange. If this persists, check GET /system/health for system status".to_string())
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  // Generate a refresh token alongside the JWT.
  let refresh_token_plaintext = generate_refresh_token();
  let refresh_token_hash = hash_refresh_token(&refresh_token_plaintext);
  let refresh_expires_at =
    chrono::Utc::now() + chrono::Duration::seconds(DEFAULT_REFRESH_EXPIRY_SECONDS);

  let ctx = RequestContext::with_bus(state.event_bus.clone());
  let refresh_record = RefreshTokenRecord {
    token_hash: refresh_token_hash,
    user_subject: match record.user_id {
      Some(uid) => uid.to_string(),
      None => format!("share:{}", record.key_id),
    },
    created_at: chrono::Utc::now(),
    expires_at: refresh_expires_at,
    is_revoked: false,
  };
  if let Err(error) = system_store::store_refresh_token(&state.engine, &ctx, &refresh_record) {
    tracing::error!("Failed to store refresh token: {}", error);
    return ErrorResponse::new("Failed to store refresh token during token exchange. If this persists, check GET /system/health for system status".to_string())
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response();
  }

  metrics::counter!(crate::metrics::definitions::AUTH_TOKEN_EXCHANGES_TOTAL, "result" => "success").increment(1);

  (
    StatusCode::OK,
    Json(serde_json::json!({
      "token": token,
      "expires_in": jwt_expiry,
      "refresh_token": refresh_token_plaintext,
    })),
  )
    .into_response()
}

/// POST /admin/api-keys -- create a new API key (requires root).
pub async fn create_api_key(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Json(payload): Json<CreateApiKeyRequest>,
) -> Response {
  if !crate::engine::is_root(&Uuid::parse_str(&claims.sub).unwrap_or(Uuid::new_v4())) {
    return ErrorResponse::new("root access required. Only the root user can create admin API keys via POST /admin/api-keys".to_string())
      .with_status(StatusCode::FORBIDDEN)
      .into_response();
  }

  // Determine which user this key is for.
  let target_user_id = match payload.user_id {
    Some(ref id_string) => match Uuid::parse_str(id_string) {
      Ok(id) => id,
      Err(_) => {
        return ErrorResponse::new(format!("Invalid user_id '{}': must be a valid UUID", id_string))
          .with_status(StatusCode::BAD_REQUEST)
          .into_response();
      }
    },
    None => {
      // Default to the calling user's identity.
      match Uuid::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(_) => {
          return ErrorResponse::new("Invalid sub claim in JWT: token 'sub' is not a valid UUID".to_string())
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
        }
      }
    }
  };

  let key_id = Uuid::new_v4();
  let plaintext_key = generate_api_key(key_id);
  let key_hash = match hash_api_key(&plaintext_key) {
    Ok(hash) => hash,
    Err(error) => {
      tracing::error!("Failed to hash API key: {}", error);
      return ErrorResponse::new("Failed to create API key: could not hash the generated key. If this persists, contact your administrator".to_string())
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  let record = ApiKeyRecord {
    key_id,
    key_hash,
    user_id: Some(target_user_id),
    created_at: chrono::Utc::now(),
    is_revoked: false,
    expires_at: chrono::Utc::now().timestamp_millis()
      + (DEFAULT_EXPIRY_DAYS * 24 * 60 * 60 * 1000),
    label: None,
    rules: vec![],
  };

  if let Err(error) = state.auth_provider.store_api_key(&record) {
    tracing::error!("Failed to store API key: {}", error);
    return ErrorResponse::new("Failed to store API key: could not persist to storage. If this persists, check GET /system/health for system status".to_string())
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response();
  }

  (
    StatusCode::CREATED,
    Json(serde_json::json!({
      "key_id": record.key_id,
      "api_key": plaintext_key,
      "user_id": record.user_id,
      "created_at": record.created_at.to_rfc3339(),
    })),
  )
    .into_response()
}

/// GET /admin/api-keys -- list all API key metadata (no secrets).
pub async fn list_api_keys(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
) -> Response {
  if !crate::engine::is_root(&Uuid::parse_str(&claims.sub).unwrap_or(Uuid::new_v4())) {
    return ErrorResponse::new("root access required. Only the root user can list admin API keys via GET /admin/api-keys".to_string())
      .with_status(StatusCode::FORBIDDEN)
      .into_response();
  }

  match state.auth_provider.list_api_keys() {
    Ok(keys) => {
      // Build a user_id → username cache to avoid repeated lookups
      let mut username_cache: std::collections::HashMap<uuid::Uuid, String> = std::collections::HashMap::new();
      username_cache.insert(
        uuid::Uuid::nil(),
        "root".to_string(),
      );

      let metadata: Vec<serde_json::Value> = keys
        .iter()
        .map(|record| {
          let username = if let Some(uid) = record.user_id {
            username_cache
              .entry(uid)
              .or_insert_with(|| {
                crate::engine::system_store::get_user(&state.engine, &uid)
                  .ok()
                  .flatten()
                  .map(|u| u.username)
                  .unwrap_or_else(|| uid.to_string())
              })
              .clone()
          } else {
            "share-key".to_string()
          };

          serde_json::json!({
            "key_id": record.key_id,
            "user_id": record.user_id,
            "username": username,
            "created_at": record.created_at.to_rfc3339(),
            "is_revoked": record.is_revoked,
            "expires_at": record.expires_at,
            "label": record.label,
            "rules": record.rules,
          })
        })
        .collect();
      (StatusCode::OK, Json(serde_json::json!({"items": metadata}))).into_response()
    }
    Err(error) => {
      tracing::error!("Failed to list API keys: {}", error);
      ErrorResponse::new("Failed to list API keys: could not read from storage. If this persists, check GET /system/health for system status".to_string())
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// DELETE /admin/api-keys/:key_id -- revoke an API key.
pub async fn revoke_api_key(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(key_id): Path<String>,
) -> Response {
  if !crate::engine::is_root(&Uuid::parse_str(&claims.sub).unwrap_or(Uuid::new_v4())) {
    return ErrorResponse::new("root access required. Only the root user can revoke admin API keys via DELETE /admin/api-keys/:key_id".to_string())
      .with_status(StatusCode::FORBIDDEN)
      .into_response();
  }

  let parsed_key_id = match Uuid::parse_str(&key_id) {
    Ok(id) => id,
    Err(_) => {
      return ErrorResponse::new(format!("Invalid key ID '{}': must be a valid UUID", key_id))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  };

  match state.auth_provider.revoke_api_key(parsed_key_id) {
    Ok(true) => {
      state.api_key_cache.evict(&parsed_key_id.to_string());
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
      ErrorResponse::new("Failed to revoke API key: could not persist revocation to storage. If this persists, check GET /system/health for system status".to_string())
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

// ---------------------------------------------------------------------------
// Magic link routes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct MagicLinkRequest {
  pub email: String,
}

#[derive(Debug, Deserialize)]
pub struct VerifyMagicLinkQuery {
  pub code: String,
}

/// POST /auth/magic-link — request a magic link for the given email.
///
/// Always returns 200 to prevent email enumeration. In dev mode, the magic link
/// URL is logged via tracing (no email is actually sent).
pub async fn request_magic_link(
  State(state): State<AppState>,
  Json(payload): Json<MagicLinkRequest>,
) -> Response {
  // Rate-limit by email.
  if let Err(error) = state.rate_limiter.check_rate_limit(&payload.email) {
    metrics::counter!(crate::metrics::definitions::AUTH_RATE_LIMIT_HITS_TOTAL).increment(1);
    return ErrorResponse::new(error.to_string())
      .with_status(StatusCode::TOO_MANY_REQUESTS)
      .into_response();
  }

  let code = generate_magic_link_code();
  let code_hash = hash_magic_link_code(&code);
  let expires_at = chrono::Utc::now()
    + chrono::Duration::seconds(
      crate::auth::magic_link::DEFAULT_MAGIC_LINK_EXPIRY_SECONDS,
    );

  let ctx = RequestContext::with_bus(state.event_bus.clone());
  let record = MagicLinkRecord {
    code_hash: code_hash.clone(),
    email: payload.email.clone(),
    created_at: chrono::Utc::now(),
    expires_at,
    is_used: false,
  };
  if let Err(error) = system_store::store_magic_link(&state.engine, &ctx, &record) {
    tracing::error!("Failed to store magic link: {}", error);
    // Still return 200 to prevent enumeration.
  }

  // Log the magic link URL at debug level only — the code is a secret.
  tracing::debug!(
    email = %payload.email,
    magic_link_url = %format!("/auth/magic-link/verify?code={}", code),
    "Magic link generated (dev mode — not emailed)"
  );

  (
    StatusCode::OK,
    Json(serde_json::json!({
      "message": "If an account exists, a login link has been sent."
    })),
  )
    .into_response()
}

/// GET /auth/magic-link/verify?code=... — verify a magic link code.
///
/// On success, returns a JWT. On any failure, returns 401.
pub async fn verify_magic_link(
  State(state): State<AppState>,
  Query(query): Query<VerifyMagicLinkQuery>,
) -> Response {
  let code_hash = hash_magic_link_code(&query.code);

  let record = match system_store::get_magic_link(&state.engine, &code_hash) {
    Ok(Some(record)) => record,
    Ok(None) => {
      return ErrorResponse::new("Invalid or expired magic link. Request a new one via POST /auth/magic-link".to_string())
        .with_status(StatusCode::UNAUTHORIZED)
        .into_response();
    }
    Err(error) => {
      tracing::error!("Failed to look up magic link: {}", error);
      return ErrorResponse::new("Failed to verify magic link. Request a new one via POST /auth/magic-link".to_string())
        .with_status(StatusCode::UNAUTHORIZED)
        .into_response();
    }
  };

  if record.is_used {
    return ErrorResponse::new("Magic link already used. Each link can only be used once — request a new one via POST /auth/magic-link".to_string())
      .with_status(StatusCode::UNAUTHORIZED)
      .into_response();
  }

  if record.expires_at < chrono::Utc::now() {
    return ErrorResponse::new("Magic link expired. Request a new one via POST /auth/magic-link".to_string())
      .with_status(StatusCode::UNAUTHORIZED)
      .into_response();
  }

  // Mark as used.
  let ctx = RequestContext::with_bus(state.event_bus.clone());
  if let Err(error) = system_store::mark_magic_link_used(&state.engine, &ctx, &code_hash) {
    tracing::error!("Failed to mark magic link as used: {}", error);
    return ErrorResponse::new("An unexpected error occurred while processing the magic link. If this persists, check GET /system/health for system status".to_string())
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response();
  }

  // Issue a JWT for this email.
  // Look up the user by email so we can use their UUID as `sub`.
  // Permission middleware expects a UUID — using the raw email would fail auth.
  let sub = match system_store::get_user_by_username(&state.engine, &record.email) {
    Ok(Some(user)) => user.user_id.to_string(),
    Ok(None) => {
      tracing::warn!("Magic link verified for '{}' but no user record found", record.email);
      return ErrorResponse::new("No user account exists for this email address. Contact your administrator to create an account".to_string())
        .with_status(StatusCode::UNAUTHORIZED)
        .into_response();
    }
    Err(error) => {
      tracing::error!("Failed to look up user by email '{}': {}", record.email, error);
      return ErrorResponse::new("An unexpected error occurred while looking up the user account. If this persists, check GET /system/health for system status".to_string())
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub,
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + crate::auth::jwt::DEFAULT_EXPIRY_SECONDS,
    scope: None,
    permissions: None,
    key_id: None,
  };

  match state.jwt_manager.create_token(&claims) {
    Ok(token) => (
      StatusCode::OK,
      Json(serde_json::json!({
        "token": token,
        "expires_in": crate::auth::jwt::DEFAULT_EXPIRY_SECONDS,
      })),
    )
      .into_response(),
    Err(error) => {
      tracing::error!("Failed to create JWT: {}", error);
      ErrorResponse::new("Failed to create JWT after magic link verification. If this persists, check GET /system/health for system status".to_string())
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

// ---------------------------------------------------------------------------
// Refresh token routes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RefreshTokenRequest {
  pub refresh_token: String,
}

/// POST /auth/refresh — exchange a refresh token for a new JWT + new refresh token.
///
/// Implements token rotation: the old refresh token is revoked and a new one is issued.
pub async fn refresh_token(
  State(state): State<AppState>,
  Json(payload): Json<RefreshTokenRequest>,
) -> Response {
  let old_token_hash = hash_refresh_token(&payload.refresh_token);

  let record = match system_store::get_refresh_token(&state.engine, &old_token_hash) {
    Ok(Some(record)) => record,
    Ok(None) => {
      return ErrorResponse::new("Invalid refresh token: no matching token found. Re-authenticate via POST /auth/token".to_string())
        .with_status(StatusCode::UNAUTHORIZED)
        .into_response();
    }
    Err(error) => {
      tracing::error!("Failed to look up refresh token: {}", error);
      return ErrorResponse::new("Failed to look up refresh token. Re-authenticate via POST /auth/token".to_string())
        .with_status(StatusCode::UNAUTHORIZED)
        .into_response();
    }
  };

  if record.is_revoked {
    return ErrorResponse::new("Refresh token has been revoked. Re-authenticate via POST /auth/token".to_string())
      .with_status(StatusCode::UNAUTHORIZED)
      .into_response();
  }

  if record.expires_at < chrono::Utc::now() {
    return ErrorResponse::new("Refresh token expired. Re-authenticate via POST /auth/token".to_string())
      .with_status(StatusCode::UNAUTHORIZED)
      .into_response();
  }

  // Revoke the old refresh token (rotation).
  let ctx = RequestContext::with_bus(state.event_bus.clone());
  if let Err(error) = system_store::revoke_refresh_token(&state.engine, &ctx, &old_token_hash) {
    tracing::error!("Failed to revoke old refresh token: {}", error);
    return ErrorResponse::new("An unexpected error occurred while rotating the refresh token. If this persists, check GET /system/health for system status".to_string())
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response();
  }

  // Issue a new JWT.
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: record.user_subject.clone(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + crate::auth::jwt::DEFAULT_EXPIRY_SECONDS,
    scope: None,
    permissions: None,
    key_id: None,
  };

  let token = match state.jwt_manager.create_token(&claims) {
    Ok(token) => token,
    Err(error) => {
      tracing::error!("Failed to create JWT: {}", error);
      return ErrorResponse::new("Failed to create JWT during token refresh. If this persists, check GET /system/health for system status".to_string())
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  // Issue a new refresh token.
  let new_refresh_token = generate_refresh_token();
  let new_refresh_hash = hash_refresh_token(&new_refresh_token);
  let refresh_expires_at =
    chrono::Utc::now() + chrono::Duration::seconds(DEFAULT_REFRESH_EXPIRY_SECONDS);

  let new_refresh_record = RefreshTokenRecord {
    token_hash: new_refresh_hash,
    user_subject: record.user_subject,
    created_at: chrono::Utc::now(),
    expires_at: refresh_expires_at,
    is_revoked: false,
  };
  if let Err(error) = system_store::store_refresh_token(&state.engine, &ctx, &new_refresh_record) {
    tracing::error!("Failed to store new refresh token: {}", error);
    return ErrorResponse::new("Failed to store new refresh token during rotation. If this persists, check GET /system/health for system status".to_string())
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response();
  }

  (
    StatusCode::OK,
    Json(serde_json::json!({
      "token": token,
      "expires_in": crate::auth::jwt::DEFAULT_EXPIRY_SECONDS,
      "refresh_token": new_refresh_token,
    })),
  )
    .into_response()
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// GET /admin/metrics -- render Prometheus metrics.
pub async fn metrics_endpoint(
  State(state): State<AppState>,
) -> Response {
  let output = state.prometheus_handle.render();
  Response::builder()
    .status(StatusCode::OK)
    .header("content-type", "text/plain; version=0.0.4; charset=utf-8")
    .body(Body::from(output))
    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}
