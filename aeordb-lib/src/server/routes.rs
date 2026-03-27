use axum::{
  Extension,
  body::Body,
  extract::{Path, Query, State},
  http::{HeaderMap, StatusCode},
  response::{IntoResponse, Response},
  Json,
};
use serde::Deserialize;
use uuid::Uuid;

use super::responses::{
  CreateDocumentResponse, DeleteDocumentResponse, DocumentMetadataResponse, ErrorResponse,
};
use super::state::AppState;
use crate::auth::{
  TokenClaims, generate_api_key, hash_api_key, parse_api_key, verify_api_key, ApiKeyRecord,
  generate_magic_link_code, hash_magic_link_code,
  generate_refresh_token, hash_refresh_token,
};
use crate::auth::refresh::DEFAULT_REFRESH_EXPIRY_SECONDS;
use crate::storage::redb_backend::StorageError;

/// Validate a database or table name for safety.
/// Allows: alphanumeric, underscores, hyphens.
/// Rejects: empty, starts with underscore (reserved for system tables),
/// longer than 255 chars, path traversal characters.
fn validate_resource_name(name: &str) -> Result<(), String> {
  if name.is_empty() {
    return Err("Resource name must not be empty".to_string());
  }

  if name.len() > 255 {
    return Err("Resource name must not exceed 255 characters".to_string());
  }

  if name.starts_with('_') {
    return Err("Resource names starting with underscore are reserved for system use".to_string());
  }

  if name.contains('/') || name.contains('\\') || name.contains("..") {
    return Err("Resource name contains invalid path traversal characters".to_string());
  }

  if !name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
    return Err("Resource name must only contain alphanumeric characters, underscores, or hyphens".to_string());
  }

  Ok(())
}

/// Build a fully qualified table name from the database and table path segments.
fn build_table_name(database: &str, table: &str) -> Result<String, Box<Response>> {
  if let Err(message) = validate_resource_name(database) {
    return Err(Box::new(
      ErrorResponse::new(format!("Invalid database name: {}", message))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response(),
    ));
  }
  if let Err(message) = validate_resource_name(table) {
    return Err(Box::new(
      ErrorResponse::new(format!("Invalid table name: {}", message))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response(),
    ));
  }
  Ok(format!("{}:{}", database, table))
}

pub async fn health_check() -> impl IntoResponse {
  Json(serde_json::json!({ "status": "ok" }))
}

#[derive(Debug, Deserialize)]
pub struct ListDocumentsQuery {
  pub include_deleted: Option<bool>,
}

pub async fn create_document(
  State(state): State<AppState>,
  Path((database, table)): Path<(String, String)>,
  headers: HeaderMap,
  body: axum::body::Bytes,
) -> Response {
  let table_name = match build_table_name(&database, &table) {
    Ok(name) => name,
    Err(response) => return *response,
  };

  let content_type = headers
    .get("content-type")
    .and_then(|value| value.to_str().ok())
    .map(|value| value.to_string());

  let document = match state.storage.create_document(&table_name, body.to_vec(), content_type) {
    Ok(document) => document,
    Err(error) => {
      tracing::error!("Failed to create document: {}", error);
      return ErrorResponse::new(format!("Failed to create document: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  let response_body = CreateDocumentResponse::from(&document);
  (StatusCode::CREATED, Json(response_body)).into_response()
}

pub async fn get_document(
  State(state): State<AppState>,
  Path((database, table, document_id)): Path<(String, String, String)>,
) -> Response {
  let parsed_id = match Uuid::parse_str(&document_id) {
    Ok(id) => id,
    Err(_) => {
      return ErrorResponse::new(format!("Invalid document ID: {}", document_id))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  };

  let table_name = match build_table_name(&database, &table) {
    Ok(name) => name,
    Err(response) => return *response,
  };

  let document = match state.storage.get_document(&table_name, parsed_id) {
    Ok(Some(document)) => document,
    Ok(None) => {
      return ErrorResponse::new(format!("Document not found: {}", parsed_id))
        .with_status(StatusCode::NOT_FOUND)
        .into_response();
    }
    Err(error) => {
      tracing::error!("Failed to get document: {}", error);
      return ErrorResponse::new(format!("Failed to get document: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  let mut response_builder = Response::builder()
    .status(StatusCode::OK)
    .header("X-Document-Id", document.document_id.to_string())
    .header("X-Created-At", document.created_at.to_rfc3339())
    .header("X-Updated-At", document.updated_at.to_rfc3339());

  if let Some(ref content_type) = document.content_type {
    response_builder = response_builder.header("content-type", content_type.as_str());
  }

  response_builder
    .body(Body::from(document.data))
    .unwrap_or_else(|_| {
      (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Failed to build response",
      )
        .into_response()
    })
}

pub async fn update_document(
  State(state): State<AppState>,
  Path((database, table, document_id)): Path<(String, String, String)>,
  body: axum::body::Bytes,
) -> Response {
  let parsed_id = match Uuid::parse_str(&document_id) {
    Ok(id) => id,
    Err(_) => {
      return ErrorResponse::new(format!("Invalid document ID: {}", document_id))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  };

  let table_name = match build_table_name(&database, &table) {
    Ok(name) => name,
    Err(response) => return *response,
  };

  let document = match state.storage.update_document(&table_name, parsed_id, body.to_vec()) {
    Ok(document) => document,
    Err(StorageError::DocumentNotFound(id)) => {
      return ErrorResponse::new(format!("Document not found: {}", id))
        .with_status(StatusCode::NOT_FOUND)
        .into_response();
    }
    Err(error) => {
      tracing::error!("Failed to update document: {}", error);
      return ErrorResponse::new(format!("Failed to update document: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  let response_body = CreateDocumentResponse::from(&document);
  (StatusCode::OK, Json(response_body)).into_response()
}

pub async fn delete_document(
  State(state): State<AppState>,
  Path((database, table, document_id)): Path<(String, String, String)>,
) -> Response {
  let parsed_id = match Uuid::parse_str(&document_id) {
    Ok(id) => id,
    Err(_) => {
      return ErrorResponse::new(format!("Invalid document ID: {}", document_id))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  };

  let table_name = match build_table_name(&database, &table) {
    Ok(name) => name,
    Err(response) => return *response,
  };

  match state.storage.delete_document(&table_name, parsed_id) {
    Ok(()) => {
      let response_body = DeleteDocumentResponse {
        deleted: true,
        document_id: parsed_id,
      };
      (StatusCode::OK, Json(response_body)).into_response()
    }
    Err(StorageError::DocumentNotFound(id)) => {
      ErrorResponse::new(format!("Document not found: {}", id))
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Failed to delete document: {}", error);
      ErrorResponse::new(format!("Failed to delete document: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

pub async fn list_documents(
  State(state): State<AppState>,
  Path((database, table)): Path<(String, String)>,
  Query(query): Query<ListDocumentsQuery>,
) -> Response {
  let table_name = match build_table_name(&database, &table) {
    Ok(name) => name,
    Err(response) => return *response,
  };
  let include_deleted = query.include_deleted.unwrap_or(false);

  match state.storage.list_documents(&table_name, include_deleted) {
    Ok(documents) => {
      let metadata: Vec<DocumentMetadataResponse> =
        documents.iter().map(DocumentMetadataResponse::from).collect();
      (StatusCode::OK, Json(metadata)).into_response()
    }
    Err(error) => {
      tracing::error!("Failed to list documents: {}", error);
      ErrorResponse::new(format!("Failed to list documents: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

// ---------------------------------------------------------------------------
// Plugin routes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DeployPluginQuery {
  pub name: Option<String>,
  pub plugin_type: Option<String>,
}

/// PUT /:database/:schema/:table/_deploy — deploy a plugin.
///
/// Accepts the WASM binary as the raw request body.
/// Plugin name comes from the `name` query parameter (defaults to the table segment).
/// Plugin type comes from the `plugin_type` query parameter (defaults to "wasm").
pub async fn deploy_plugin(
  State(state): State<AppState>,
  Path((database, schema, table)): Path<(String, String, String)>,
  Query(query): Query<DeployPluginQuery>,
  body: axum::body::Bytes,
) -> Response {
  let plugin_path = format!("{}/{}/{}", database, schema, table);
  let plugin_name = query.name.unwrap_or_else(|| table.clone());

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
      (StatusCode::OK, Json(serde_json::to_value(metadata).unwrap())).into_response()
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

/// POST /:database/:schema/:table/:function_name/_invoke — invoke a deployed plugin.
pub async fn invoke_plugin(
  State(state): State<AppState>,
  Path((database, schema, table, function_name)): Path<(String, String, String, String)>,
  body: axum::body::Bytes,
) -> Response {
  let plugin_path = format!("{}/{}/{}", database, schema, table);

  // For now we ignore function_name — in the future it could select a specific
  // exported function within the plugin module.
  let _ = function_name;

  match state
    .plugin_manager
    .invoke_wasm_plugin(&plugin_path, &body)
  {
    Ok(response_bytes) => {
      let mut response_builder = axum::http::Response::builder().status(StatusCode::OK);
      response_builder = response_builder.header("content-type", "application/octet-stream");
      response_builder
        .body(axum::body::Body::from(response_bytes))
        .unwrap_or_else(|_| {
          (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response").into_response()
        })
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

/// GET /:database/_plugins — list all deployed plugins.
pub async fn list_plugins(
  State(state): State<AppState>,
  Path(_database): Path<String>,
) -> Response {
  match state.plugin_manager.list_plugins() {
    Ok(plugins) => (StatusCode::OK, Json(serde_json::to_value(plugins).unwrap())).into_response(),
    Err(error) => {
      tracing::error!("Failed to list plugins: {}", error);
      ErrorResponse::new(format!("Failed to list plugins: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// DELETE /:database/:schema/:table/:function_name/_remove — remove a deployed plugin.
pub async fn remove_plugin(
  State(state): State<AppState>,
  Path((database, schema, table, _function_name)): Path<(String, String, String, String)>,
) -> Response {
  let plugin_path = format!("{}/{}/{}", database, schema, table);

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
  pub roles: Option<Vec<String>>,
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
      return ErrorResponse::new("Invalid API key".to_string())
        .with_status(StatusCode::UNAUTHORIZED)
        .into_response();
    }
  };

  let record = match state.storage.get_system_api_key(&key_id_prefix) {
    Ok(Some(record)) => record,
    Ok(None) => {
      return ErrorResponse::new("Invalid API key".to_string())
        .with_status(StatusCode::UNAUTHORIZED)
        .into_response();
    }
    Err(error) => {
      tracing::error!("Failed to look up API key: {}", error);
      return ErrorResponse::new("Internal server error".to_string())
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  if record.is_revoked {
    return ErrorResponse::new("Invalid API key".to_string())
      .with_status(StatusCode::UNAUTHORIZED)
      .into_response();
  }

  let key_valid = match verify_api_key(&payload.api_key, &record.key_hash) {
    Ok(valid) => valid,
    Err(_) => {
      return ErrorResponse::new("Invalid API key".to_string())
        .with_status(StatusCode::UNAUTHORIZED)
        .into_response();
    }
  };

  if !key_valid {
    return ErrorResponse::new("Invalid API key".to_string())
      .with_status(StatusCode::UNAUTHORIZED)
      .into_response();
  }

  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: record.key_id.to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + crate::auth::jwt::DEFAULT_EXPIRY_SECONDS,
    roles: record.roles.clone(),
    scope: None,
    permissions: None,
  };

  let token = match state.jwt_manager.create_token(&claims) {
    Ok(token) => token,
    Err(error) => {
      tracing::error!("Failed to create JWT: {}", error);
      return ErrorResponse::new("Failed to create token".to_string())
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  // Generate a refresh token alongside the JWT.
  let refresh_token_plaintext = generate_refresh_token();
  let refresh_token_hash = hash_refresh_token(&refresh_token_plaintext);
  let refresh_expires_at =
    chrono::Utc::now() + chrono::Duration::seconds(DEFAULT_REFRESH_EXPIRY_SECONDS);

  if let Err(error) = state.storage.store_refresh_token(
    &refresh_token_hash,
    &record.key_id.to_string(),
    refresh_expires_at,
  ) {
    tracing::error!("Failed to store refresh token: {}", error);
    return ErrorResponse::new("Failed to create token".to_string())
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response();
  }

  (
    StatusCode::OK,
    Json(serde_json::json!({
      "token": token,
      "expires_in": crate::auth::jwt::DEFAULT_EXPIRY_SECONDS,
      "refresh_token": refresh_token_plaintext,
    })),
  )
    .into_response()
}

/// POST /admin/api-keys -- create a new API key (requires admin role).
pub async fn create_api_key(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Json(payload): Json<CreateApiKeyRequest>,
) -> Response {
  if !claims.roles.contains(&"admin".to_string()) {
    return ErrorResponse::new("Admin role required".to_string())
      .with_status(StatusCode::FORBIDDEN)
      .into_response();
  }

  let key_id = Uuid::new_v4();
  let plaintext_key = generate_api_key(key_id);
  let key_hash = match hash_api_key(&plaintext_key) {
    Ok(hash) => hash,
    Err(error) => {
      tracing::error!("Failed to hash API key: {}", error);
      return ErrorResponse::new("Failed to create API key".to_string())
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  let roles = payload.roles.unwrap_or_default();
  let record = ApiKeyRecord {
    key_id,
    key_hash,
    roles,
    created_at: chrono::Utc::now(),
    is_revoked: false,
  };

  if let Err(error) = state.storage.store_api_key(&record) {
    tracing::error!("Failed to store API key: {}", error);
    return ErrorResponse::new("Failed to store API key".to_string())
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response();
  }

  (
    StatusCode::CREATED,
    Json(serde_json::json!({
      "key_id": record.key_id,
      "api_key": plaintext_key,
      "roles": record.roles,
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
  if !claims.roles.contains(&"admin".to_string()) {
    return ErrorResponse::new("Admin role required".to_string())
      .with_status(StatusCode::FORBIDDEN)
      .into_response();
  }

  match state.storage.list_system_api_keys() {
    Ok(keys) => {
      let metadata: Vec<serde_json::Value> = keys
        .iter()
        .map(|record| {
          serde_json::json!({
            "key_id": record.key_id,
            "roles": record.roles,
            "created_at": record.created_at.to_rfc3339(),
            "is_revoked": record.is_revoked,
          })
        })
        .collect();
      (StatusCode::OK, Json(metadata)).into_response()
    }
    Err(error) => {
      tracing::error!("Failed to list API keys: {}", error);
      ErrorResponse::new("Failed to list API keys".to_string())
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
  if !claims.roles.contains(&"admin".to_string()) {
    return ErrorResponse::new("Admin role required".to_string())
      .with_status(StatusCode::FORBIDDEN)
      .into_response();
  }

  let parsed_key_id = match Uuid::parse_str(&key_id) {
    Ok(id) => id,
    Err(_) => {
      return ErrorResponse::new(format!("Invalid key ID: {}", key_id))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  };

  match state.storage.revoke_api_key(parsed_key_id) {
    Ok(true) => (
      StatusCode::OK,
      Json(serde_json::json!({
        "revoked": true,
        "key_id": parsed_key_id,
      })),
    )
      .into_response(),
    Ok(false) => {
      ErrorResponse::new(format!("API key not found: {}", parsed_key_id))
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Failed to revoke API key: {}", error);
      ErrorResponse::new("Failed to revoke API key".to_string())
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

  if let Err(error) = state
    .storage
    .store_magic_link(&code_hash, &payload.email, expires_at)
  {
    tracing::error!("Failed to store magic link: {}", error);
    // Still return 200 to prevent enumeration.
  }

  // In dev mode, log the magic link URL.
  tracing::info!(
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

  let record = match state.storage.get_magic_link(&code_hash) {
    Ok(Some(record)) => record,
    Ok(None) => {
      return ErrorResponse::new("Invalid or expired magic link".to_string())
        .with_status(StatusCode::UNAUTHORIZED)
        .into_response();
    }
    Err(error) => {
      tracing::error!("Failed to look up magic link: {}", error);
      return ErrorResponse::new("Invalid or expired magic link".to_string())
        .with_status(StatusCode::UNAUTHORIZED)
        .into_response();
    }
  };

  if record.is_used {
    return ErrorResponse::new("Magic link already used".to_string())
      .with_status(StatusCode::UNAUTHORIZED)
      .into_response();
  }

  if record.expires_at < chrono::Utc::now() {
    return ErrorResponse::new("Magic link expired".to_string())
      .with_status(StatusCode::UNAUTHORIZED)
      .into_response();
  }

  // Mark as used.
  if let Err(error) = state.storage.mark_magic_link_used(&code_hash) {
    tracing::error!("Failed to mark magic link as used: {}", error);
    return ErrorResponse::new("Internal server error".to_string())
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response();
  }

  // Issue a JWT for this email.
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: record.email.clone(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + crate::auth::jwt::DEFAULT_EXPIRY_SECONDS,
    roles: vec!["user".to_string()],
    scope: None,
    permissions: None,
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
      ErrorResponse::new("Failed to create token".to_string())
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

  let record = match state.storage.get_refresh_token(&old_token_hash) {
    Ok(Some(record)) => record,
    Ok(None) => {
      return ErrorResponse::new("Invalid refresh token".to_string())
        .with_status(StatusCode::UNAUTHORIZED)
        .into_response();
    }
    Err(error) => {
      tracing::error!("Failed to look up refresh token: {}", error);
      return ErrorResponse::new("Invalid refresh token".to_string())
        .with_status(StatusCode::UNAUTHORIZED)
        .into_response();
    }
  };

  if record.is_revoked {
    return ErrorResponse::new("Refresh token revoked".to_string())
      .with_status(StatusCode::UNAUTHORIZED)
      .into_response();
  }

  if record.expires_at < chrono::Utc::now() {
    return ErrorResponse::new("Refresh token expired".to_string())
      .with_status(StatusCode::UNAUTHORIZED)
      .into_response();
  }

  // Revoke the old refresh token (rotation).
  if let Err(error) = state.storage.revoke_refresh_token(&old_token_hash) {
    tracing::error!("Failed to revoke old refresh token: {}", error);
    return ErrorResponse::new("Internal server error".to_string())
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
    roles: vec!["admin".to_string()],
    scope: None,
    permissions: None,
  };

  let token = match state.jwt_manager.create_token(&claims) {
    Ok(token) => token,
    Err(error) => {
      tracing::error!("Failed to create JWT: {}", error);
      return ErrorResponse::new("Failed to create token".to_string())
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  // Issue a new refresh token.
  let new_refresh_token = generate_refresh_token();
  let new_refresh_hash = hash_refresh_token(&new_refresh_token);
  let refresh_expires_at =
    chrono::Utc::now() + chrono::Duration::seconds(DEFAULT_REFRESH_EXPIRY_SECONDS);

  if let Err(error) = state.storage.store_refresh_token(
    &new_refresh_hash,
    &record.user_subject,
    refresh_expires_at,
  ) {
    tracing::error!("Failed to store new refresh token: {}", error);
    return ErrorResponse::new("Internal server error".to_string())
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
