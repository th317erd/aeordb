use axum::{
    Extension,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use std::io::Write;

use super::responses::ErrorResponse;
use super::state::AppState;
use crate::auth::TokenClaims;
use crate::engine::directory_ops::{DirectoryOps, is_system_path};
use crate::engine::entry_type::EntryType;
use crate::engine::path_utils::normalize_path;

#[derive(Deserialize)]
pub struct DownloadRequest {
    pub paths: Vec<String>,
}

/// POST /files/download — bundle requested paths into a ZIP archive.
pub async fn download_zip(
    State(state): State<AppState>,
    Extension(_claims): Extension<TokenClaims>,
    Json(body): Json<DownloadRequest>,
) -> Response {
    if body.paths.is_empty() {
        return ErrorResponse::new("At least one path is required in the 'paths' array")
            .with_status(StatusCode::BAD_REQUEST)
            .into_response();
    }

    let ops = DirectoryOps::new(&state.engine);
    let mut zip_buffer = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    {
        let mut zip_writer = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_buffer));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

        for raw_path in &body.paths {
            let normalized = normalize_path(raw_path);

            // Skip .system/ paths
            if is_system_path(&normalized) {
                skipped.push(raw_path.clone());
                continue;
            }

            // Try as file first
            match ops.read_file(&normalized) {
                Ok(data) => {
                    let zip_entry_name = normalized.trim_start_matches('/');
                    if zip_writer.start_file(zip_entry_name, options).is_ok() {
                        let _ = zip_writer.write_all(&data);
                    }
                }
                Err(crate::engine::errors::EngineError::NotFound(_)) => {
                    // Not a file — try as directory
                    if add_directory_to_zip(&ops, &normalized, &mut zip_writer, options, &mut skipped).is_err() {
                        skipped.push(raw_path.clone());
                    }
                }
                Err(_) => {
                    skipped.push(raw_path.clone());
                }
            }
        }

        if let Err(error) = zip_writer.finish() {
            tracing::error!("Failed to finalize ZIP: {}", error);
            return ErrorResponse::new("Failed to create ZIP archive")
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
    }

    let mut builder = axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/zip")
        .header(header::CONTENT_DISPOSITION, "attachment; filename=\"aeordb-download.zip\"");

    if !skipped.is_empty() {
        builder = builder.header(
            header::HeaderName::from_static("x-aeordb-skipped"),
            skipped.join(", "),
        );
    }

    builder
        .body(axum::body::Body::from(zip_buffer))
        .unwrap_or_else(|_| {
            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build ZIP response").into_response()
        })
}

fn add_directory_to_zip(
    ops: &DirectoryOps,
    dir_path: &str,
    zip_writer: &mut zip::ZipWriter<std::io::Cursor<&mut Vec<u8>>>,
    options: zip::write::SimpleFileOptions,
    skipped: &mut Vec<String>,
) -> Result<(), ()> {
    let entries = ops.list_directory(dir_path).map_err(|_| ())?;

    for entry in entries {
        let child_path = if dir_path == "/" {
            format!("/{}", entry.name)
        } else {
            format!("{}/{}", dir_path, entry.name)
        };

        let normalized = normalize_path(&child_path);

        if is_system_path(&normalized) {
            skipped.push(child_path);
            continue;
        }

        // Check entry type — DirectoryIndex = directory, FileRecord = file
        if entry.entry_type == EntryType::DirectoryIndex.to_u8() {
            let _ = add_directory_to_zip(ops, &normalized, zip_writer, options, skipped);
        } else if entry.entry_type == EntryType::FileRecord.to_u8() {
            if let Ok(data) = ops.read_file(&normalized) {
                let zip_entry_name = normalized.trim_start_matches('/');
                if zip_writer.start_file(zip_entry_name, options).is_ok() {
                    let _ = zip_writer.write_all(&data);
                }
            }
        }
    }

    Ok(())
}
