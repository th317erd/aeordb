use axum::{
    Extension,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use super::responses::{ErrorResponse, require_root};
use super::state::AppState;
use crate::auth::TokenClaims;
use crate::engine::RequestContext;

fn unique_temp_path(prefix: &str) -> Result<String, std::io::Error> {
    let temp_file = tempfile::Builder::new()
        .prefix(&format!("{}-", prefix))
        .suffix(".aeordb")
        .tempfile()?;
    let path = temp_file.path().to_string_lossy().to_string();
    // Keep the path but drop the file handle so the caller can write to it
    let _ = temp_file.into_temp_path();
    Ok(path)
}

/// POST /admin/export -- export a version as .aeordb
/// Query params: snapshot=name, hash=hex (default: HEAD)
pub async fn export_backup(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    params: axum::extract::Query<ExportParams>,
) -> Response {
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

    let output_path = match unique_temp_path("aeordb-export") {
        Ok(path) => path,
        Err(e) => return ErrorResponse::new(format!("Failed to create temporary file for backup operation: {}. Check disk space and permissions", e))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response(),
    };

    let result = if let Some(ref hash) = params.hash {
        let hash_bytes = match hex::decode(hash) {
            Ok(b) => b,
            Err(e) => {
                return ErrorResponse::new(format!("Invalid hash: {}", e))
                    .with_status(StatusCode::BAD_REQUEST)
                    .into_response()
            }
        };
        crate::engine::backup::export_version(&state.engine, &hash_bytes, &output_path)
    } else {
        crate::engine::backup::export_snapshot(
            &state.engine,
            params.snapshot.as_deref(),
            &output_path,
        )
    };

    match result {
        Ok(export_result) => {
            // Read the file and stream it back
            match std::fs::read(&output_path) {
                Ok(data) => {
                    let _ = std::fs::remove_file(&output_path);
                    let hash_hex = hex::encode(&export_result.version_hash);
                    let hash_prefix = if hash_hex.len() >= 8 {
                        &hash_hex[..8]
                    } else {
                        &hash_hex
                    };
                    let filename = format!("export-{}.aeordb", hash_prefix);
                    (
                        StatusCode::OK,
                        [
                            ("content-type", "application/octet-stream".to_string()),
                            (
                                "content-disposition",
                                format!("attachment; filename=\"{}\"", filename),
                            ),
                        ],
                        data,
                    )
                        .into_response()
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&output_path);
                    ErrorResponse::new(format!("Failed to read exported backup file: {}. The export completed but the file could not be read", e))
                        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                        .into_response()
                }
            }
        }
        Err(e) => {
            let _ = std::fs::remove_file(&output_path);
            ErrorResponse::new(format!("Export failed: {}", e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    }
}

/// POST /admin/diff -- create a patch between two versions
pub async fn diff_backup(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    params: axum::extract::Query<DiffParams>,
) -> Response {
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

    let output_path = match unique_temp_path("aeordb-patch") {
        Ok(path) => path,
        Err(e) => return ErrorResponse::new(format!("Failed to create temporary file for backup operation: {}. Check disk space and permissions", e))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response(),
    };

    let result = crate::engine::backup::create_patch_from_snapshots(
        &state.engine,
        &params.from,
        params.to.as_deref(),
        &output_path,
    )
    .or_else(|_| {
        // Clean up partial file from first attempt
        let _ = std::fs::remove_file(&output_path);
        let from_bytes = hex::decode(&params.from).map_err(|e| {
            crate::engine::EngineError::NotFound(format!("Invalid 'from': {}", e))
        })?;
        let to_bytes = match &params.to {
            Some(h) => hex::decode(h).map_err(|e| {
                crate::engine::EngineError::NotFound(format!("Invalid 'to': {}", e))
            })?,
            None => state.engine.head_hash()?,
        };
        crate::engine::backup::create_patch(
            &state.engine,
            &from_bytes,
            &to_bytes,
            &output_path,
        )
    });

    match result {
        Ok(patch_result) => match std::fs::read(&output_path) {
            Ok(data) => {
                let _ = std::fs::remove_file(&output_path);
                let hash_hex = hex::encode(&patch_result.to_hash);
                let hash_prefix = if hash_hex.len() >= 8 {
                    &hash_hex[..8]
                } else {
                    &hash_hex
                };
                let filename = format!("patch-{}.aeordb", hash_prefix);
                (
                    StatusCode::OK,
                    [
                        ("content-type", "application/octet-stream".to_string()),
                        (
                            "content-disposition",
                            format!("attachment; filename=\"{}\"", filename),
                        ),
                    ],
                    data,
                )
                    .into_response()
            }
            Err(e) => {
                let _ = std::fs::remove_file(&output_path);
                ErrorResponse::new(format!("Failed to read generated patch file: {}. The diff completed but the file could not be read", e))
                    .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                    .into_response()
            }
        },
        Err(e) => {
            let _ = std::fs::remove_file(&output_path);
            ErrorResponse::new(format!("Diff failed: {}", e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    }
}

/// POST /admin/import -- import a backup file
pub async fn import_backup(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    params: axum::extract::Query<ImportParams>,
    body: axum::body::Bytes,
) -> Response {
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

    // Write body to temp file
    let temp_path = match unique_temp_path("aeordb-import") {
        Ok(path) => path,
        Err(e) => return ErrorResponse::new(format!("Failed to create temporary file for backup operation: {}. Check disk space and permissions", e))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response(),
    };

    if let Err(e) = std::fs::write(&temp_path, &body) {
        return ErrorResponse::new(format!("Failed to write uploaded backup to temp file: {}. Check disk space", e))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
    }

    let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
    let result = crate::engine::backup::import_backup(
        &ctx,
        &state.engine,
        &temp_path,
        params.force.unwrap_or(false),
        params.promote.unwrap_or(false),
    );

    let _ = std::fs::remove_file(&temp_path);

    match result {
        Ok(import_result) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "success",
                "backup_type": match import_result.backup_type { 1 => "export", 2 => "patch", _ => "unknown" },
                "entries_imported": import_result.entries_imported,
                "chunks_imported": import_result.chunks_imported,
                "files_imported": import_result.files_imported,
                "directories_imported": import_result.directories_imported,
                "deletions_applied": import_result.deletions_applied,
                "version_hash": hex::encode(&import_result.version_hash),
                "head_promoted": import_result.head_promoted,
            })),
        )
            .into_response(),
        Err(e) => ErrorResponse::new(format!("Import failed: {}", e))
            .with_status(StatusCode::BAD_REQUEST)
            .into_response(),
    }
}

/// POST /admin/promote -- promote a version hash to HEAD
pub async fn promote_head(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    params: axum::extract::Query<PromoteParams>,
) -> Response {
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

    let hash_bytes = match hex::decode(&params.hash) {
        Ok(b) => b,
        Err(e) => {
            return ErrorResponse::new(format!("Invalid hash: {}", e))
                .with_status(StatusCode::BAD_REQUEST)
                .into_response()
        }
    };

    match state.engine.has_entry(&hash_bytes) {
        Ok(true) => {}
        Ok(false) => {
            return ErrorResponse::new(format!("Version hash '{}' not found. Use GET /versions/snapshots to find valid version hashes", params.hash))
                .with_status(StatusCode::NOT_FOUND)
                .into_response()
        }
        Err(e) => {
            return ErrorResponse::new(format!("Failed to verify version hash '{}': {}", params.hash, e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    }

    match state.engine.update_head(&hash_bytes) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "success",
                "head": hex::encode(&hash_bytes),
            })),
        )
            .into_response(),
        Err(e) => ErrorResponse::new(format!("Promote failed: {}", e))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct ExportParams {
    pub snapshot: Option<String>,
    pub hash: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DiffParams {
    pub from: String,
    pub to: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ImportParams {
    pub force: Option<bool>,
    pub promote: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct PromoteParams {
    pub hash: String,
}
