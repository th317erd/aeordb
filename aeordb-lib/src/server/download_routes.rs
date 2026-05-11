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
    active_key_rules: Option<Extension<crate::auth::permission_middleware::ActiveKeyRules>>,
    Json(body): Json<DownloadRequest>,
) -> Response {
    if body.paths.is_empty() {
        return ErrorResponse::new("At least one path is required in the 'paths' array")
            .with_status(StatusCode::BAD_REQUEST)
            .into_response();
    }

    // Scoped-key check: every requested path must be readable by the key.
    // We use 'r' for files and 'l' for directories (the matching crudlify
    // flag). If the key's rules don't permit a path, return 404 so the
    // caller cannot enumerate the tree by probing.
    if let Some(Extension(rules)) = active_key_rules.as_ref() {
        use crate::engine::api_key_rules::{match_rules, check_operation_permitted};
        for raw_path in &body.paths {
            let normalized = normalize_path(raw_path);
            // Probe the path: check 'r' (file) OR 'l' (directory listing).
            let permitted = match match_rules(&rules.0, &normalized) {
                Some(rule) => {
                    check_operation_permitted(&rule.permitted, 'r')
                        || check_operation_permitted(&rule.permitted, 'l')
                }
                None => false,
            };
            if !permitted {
                return ErrorResponse::new(format!("Not found: {}", raw_path))
                    .with_status(StatusCode::NOT_FOUND)
                    .into_response();
            }
        }
    }

    const MAX_ZIP_SIZE: u64 = 2_147_483_648; // 2 GB

    let ops = DirectoryOps::new(&state.engine);
    let mut zip_buffer = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut cumulative_size: u64 = 0;

    // Compute common path prefix so ZIP entries are relative to the user's
    // browsing context, not the DB root. E.g. selecting /docs/readme.md and
    // /docs/notes.txt produces readme.md and notes.txt, not docs/readme.md.
    let normalized_paths: Vec<String> = body.paths.iter()
        .map(|p| normalize_path(p))
        .collect();
    let common_prefix = compute_common_prefix(&normalized_paths);

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
                    cumulative_size += data.len() as u64;
                    if cumulative_size > MAX_ZIP_SIZE {
                        return ErrorResponse::new(
                            "Download exceeds the 2 GB size limit. Select fewer files or download individually."
                        )
                            .with_status(StatusCode::PAYLOAD_TOO_LARGE)
                            .into_response();
                    }
                    let zip_entry_name = strip_prefix(&normalized, &common_prefix);
                    if zip_writer.start_file(&zip_entry_name, options).is_ok() {
                        let _ = zip_writer.write_all(&data);
                    }
                }
                Err(crate::engine::errors::EngineError::NotFound(_)) => {
                    // Not a file — try as directory
                    if add_directory_to_zip(&ops, &normalized, &common_prefix, &mut zip_writer, options, &mut skipped, &mut cumulative_size, MAX_ZIP_SIZE).is_err() {
                        if cumulative_size > MAX_ZIP_SIZE {
                            return ErrorResponse::new(
                                "Download exceeds the 2 GB size limit. Select fewer files or download individually."
                            )
                                .with_status(StatusCode::PAYLOAD_TOO_LARGE)
                                .into_response();
                        }
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
    common_prefix: &str,
    zip_writer: &mut zip::ZipWriter<std::io::Cursor<&mut Vec<u8>>>,
    options: zip::write::SimpleFileOptions,
    skipped: &mut Vec<String>,
    cumulative_size: &mut u64,
    max_size: u64,
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

        if entry.entry_type == EntryType::DirectoryIndex.to_u8() {
            let _ = add_directory_to_zip(ops, &normalized, common_prefix, zip_writer, options, skipped, cumulative_size, max_size);
        } else if entry.entry_type == EntryType::FileRecord.to_u8() {
            if let Ok(data) = ops.read_file(&normalized) {
                *cumulative_size += data.len() as u64;
                if *cumulative_size > max_size {
                    return Err(());
                }
                let zip_entry_name = strip_prefix(&normalized, common_prefix);
                if zip_writer.start_file(&zip_entry_name, options).is_ok() {
                    let _ = zip_writer.write_all(&data);
                }
            }
        }
    }

    Ok(())
}

/// Compute the longest common directory prefix from a list of paths.
/// E.g. ["/docs/readme.md", "/docs/notes.txt"] → "/docs/"
/// E.g. ["/docs/readme.md", "/images/logo.svg"] → "/"
fn compute_common_prefix(paths: &[String]) -> String {
    if paths.is_empty() {
        return "/".to_string();
    }

    // Split each path into directory segments
    let first = paths[0].as_str();
    let first_parent = match first.rfind('/') {
        Some(0) => "/",
        Some(idx) => &first[..idx + 1],
        None => "/",
    };

    let mut prefix = first_parent.to_string();

    for path in &paths[1..] {
        // Shorten prefix until it matches this path
        while !prefix.is_empty() && prefix != "/" {
            if path.starts_with(&prefix) {
                break;
            }
            // Remove last segment
            let trimmed = prefix.trim_end_matches('/');
            match trimmed.rfind('/') {
                Some(0) => { prefix = "/".to_string(); break; }
                Some(idx) => { prefix = trimmed[..idx + 1].to_string(); }
                None => { prefix = "/".to_string(); break; }
            }
        }
    }

    prefix
}

/// Strip the common prefix from a path to get the ZIP entry name.
/// "/docs/readme.md" with prefix "/docs/" → "readme.md"
/// "/readme.md" with prefix "/" → "readme.md"
fn strip_prefix(path: &str, prefix: &str) -> String {
    let stripped = if prefix == "/" {
        path.trim_start_matches('/')
    } else if path.starts_with(prefix) {
        &path[prefix.len()..]
    } else {
        path.trim_start_matches('/')
    };
    stripped.to_string()
}
