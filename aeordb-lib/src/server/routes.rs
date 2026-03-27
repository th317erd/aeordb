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
};
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
