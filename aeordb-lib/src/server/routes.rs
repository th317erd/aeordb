use axum::{
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
use crate::storage::redb_backend::StorageError;

/// Build a fully qualified table name from the database and table path segments.
fn build_table_name(database: &str, table: &str) -> String {
  format!("{}:{}", database, table)
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
  let table_name = build_table_name(&database, &table);

  let content_type = headers
    .get("content-type")
    .and_then(|value| value.to_str().ok())
    .map(|value| value.to_string());

  let document = match state.storage.create_document(&table_name, body.to_vec(), content_type) {
    Ok(document) => document,
    Err(error) => {
      tracing::error!("Failed to create document: {}", error);
      return (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
          error: format!("Failed to create document: {}", error),
        }),
      )
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
      return (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse {
          error: format!("Invalid document ID: {}", document_id),
        }),
      )
        .into_response();
    }
  };

  let table_name = build_table_name(&database, &table);

  let document = match state.storage.get_document(&table_name, parsed_id) {
    Ok(Some(document)) => document,
    Ok(None) => {
      return (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
          error: format!("Document not found: {}", parsed_id),
        }),
      )
        .into_response();
    }
    Err(error) => {
      tracing::error!("Failed to get document: {}", error);
      return (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
          error: format!("Failed to get document: {}", error),
        }),
      )
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
      return (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse {
          error: format!("Invalid document ID: {}", document_id),
        }),
      )
        .into_response();
    }
  };

  let table_name = build_table_name(&database, &table);

  let document = match state.storage.update_document(&table_name, parsed_id, body.to_vec()) {
    Ok(document) => document,
    Err(StorageError::DocumentNotFound(id)) => {
      return (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
          error: format!("Document not found: {}", id),
        }),
      )
        .into_response();
    }
    Err(error) => {
      tracing::error!("Failed to update document: {}", error);
      return (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
          error: format!("Failed to update document: {}", error),
        }),
      )
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
      return (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse {
          error: format!("Invalid document ID: {}", document_id),
        }),
      )
        .into_response();
    }
  };

  let table_name = build_table_name(&database, &table);

  match state.storage.delete_document(&table_name, parsed_id) {
    Ok(()) => {
      let response_body = DeleteDocumentResponse {
        deleted: true,
        document_id: parsed_id,
      };
      (StatusCode::OK, Json(response_body)).into_response()
    }
    Err(StorageError::DocumentNotFound(id)) => (
      StatusCode::NOT_FOUND,
      Json(ErrorResponse {
        error: format!("Document not found: {}", id),
      }),
    )
      .into_response(),
    Err(error) => {
      tracing::error!("Failed to delete document: {}", error);
      (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
          error: format!("Failed to delete document: {}", error),
        }),
      )
        .into_response()
    }
  }
}

pub async fn list_documents(
  State(state): State<AppState>,
  Path((database, table)): Path<(String, String)>,
  Query(query): Query<ListDocumentsQuery>,
) -> Response {
  let table_name = build_table_name(&database, &table);
  let include_deleted = query.include_deleted.unwrap_or(false);

  match state.storage.list_documents(&table_name, include_deleted) {
    Ok(documents) => {
      let metadata: Vec<DocumentMetadataResponse> =
        documents.iter().map(DocumentMetadataResponse::from).collect();
      (StatusCode::OK, Json(metadata)).into_response()
    }
    Err(error) => {
      tracing::error!("Failed to list documents: {}", error);
      (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
          error: format!("Failed to list documents: {}", error),
        }),
      )
        .into_response()
    }
  }
}
