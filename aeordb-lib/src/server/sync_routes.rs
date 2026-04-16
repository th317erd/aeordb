use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use base64::Engine as _;

use super::responses::ErrorResponse;
use super::state::AppState;
use crate::engine::api_key_rules::{check_operation_permitted, match_rules, KeyRule};
use crate::engine::compression::{decompress, CompressionAlgorithm};
use crate::engine::file_record::FileRecord;
use crate::engine::symlink_record::SymlinkRecord;
use crate::engine::system_store;
use crate::engine::tree_walker::{diff_trees, walk_version_tree, TreeDiff, VersionTree};
use crate::engine::version_manager::VersionManager;
use crate::engine::storage_engine::StorageEngine;

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SyncDiffRequest {
    pub since_root_hash: Option<String>,
    pub current_root_hash: Option<String>,
    pub paths: Option<Vec<String>>,
    pub node_id: Option<u64>,
    pub virtual_time: Option<u64>,
}

#[derive(Serialize)]
pub struct SyncDiffResponse {
    pub root_hash: String,
    pub changes: SyncChanges,
    pub chunk_hashes_needed: Vec<String>,
}

#[derive(Serialize)]
pub struct SyncChanges {
    pub files_added: Vec<SyncFileEntry>,
    pub files_modified: Vec<SyncFileEntry>,
    pub files_deleted: Vec<SyncDeletedEntry>,
    pub symlinks_added: Vec<SyncSymlinkEntry>,
    pub symlinks_modified: Vec<SyncSymlinkEntry>,
    pub symlinks_deleted: Vec<SyncDeletedEntry>,
}

#[derive(Serialize)]
pub struct SyncFileEntry {
    pub path: String,
    pub hash: String,
    pub size: u64,
    pub content_type: Option<String>,
    pub chunk_hashes: Vec<String>,
}

#[derive(Serialize)]
pub struct SyncSymlinkEntry {
    pub path: String,
    pub hash: String,
    pub target: String,
}

#[derive(Serialize)]
pub struct SyncDeletedEntry {
    pub path: String,
}

#[derive(Deserialize)]
pub struct SyncChunksRequest {
    pub hashes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Caller identity for sync operations
// ---------------------------------------------------------------------------

/// Describes who is calling the sync endpoint and what access they have.
pub enum SyncCaller {
    /// Cluster secret auth -- full access including /.system/.
    Peer,
    /// Root JWT (nil UUID) -- full access including /.system/.
    RootUser,
    /// Non-root JWT -- /.system/ filtered out, API key rules applied.
    ScopedUser {
        #[allow(dead_code)]
        user_id: String,
        key_rules: Vec<KeyRule>,
    },
}

impl SyncCaller {
    /// Whether this caller should see /.system/ entries.
    fn include_system(&self) -> bool {
        matches!(self, SyncCaller::Peer | SyncCaller::RootUser)
    }

    /// API key rules for path-level filtering (empty = no restrictions).
    fn key_rules(&self) -> &[KeyRule] {
        match self {
            SyncCaller::ScopedUser { key_rules, .. } => key_rules,
            _ => &[],
        }
    }
}

// ---------------------------------------------------------------------------
// Cluster secret validation
// ---------------------------------------------------------------------------

fn validate_cluster_secret(headers: &HeaderMap, engine: &StorageEngine) -> bool {
    let secret = match headers.get("X-Cluster-Secret") {
        Some(v) => match v.to_str() {
            Ok(s) => s,
            Err(_) => return false,
        },
        None => return false,
    };

    let provided_hash = blake3::hash(secret.as_bytes());

    match system_store::get_cluster_secret_hash(engine) {
        Ok(Some(stored_hash)) => provided_hash.as_bytes().to_vec() == stored_hash,
        _ => false,
    }
}

/// Determine the caller identity from request headers.
/// Tries cluster secret first, then JWT. Returns 401 if neither works.
fn determine_sync_caller(
    headers: &HeaderMap,
    state: &AppState,
) -> Result<SyncCaller, Response> {
    // 1. Try cluster secret first (peer mode).
    if validate_cluster_secret(headers, &state.engine) {
        return Ok(SyncCaller::Peer);
    }

    // 2. Try JWT Bearer token.
    if let Some(auth_header) = headers.get("authorization") {
        let token = auth_header
            .to_str()
            .ok()
            .and_then(|s| s.strip_prefix("Bearer "))
            .ok_or_else(|| {
                ErrorResponse::new("Invalid authorization header")
                    .with_status(StatusCode::UNAUTHORIZED)
                    .into_response()
            })?;

        let claims = state.jwt_manager.verify_token(token).map_err(|_| {
            ErrorResponse::new("Invalid or expired JWT")
                .with_status(StatusCode::UNAUTHORIZED)
                .into_response()
        })?;

        let user_id = uuid::Uuid::parse_str(&claims.sub).map_err(|_| {
            ErrorResponse::new("Invalid user ID in token")
                .with_status(StatusCode::UNAUTHORIZED)
                .into_response()
        })?;

        if crate::engine::user::is_root(&user_id) {
            return Ok(SyncCaller::RootUser);
        }

        // Non-root: check API key scoping if key_id is present.
        let key_rules = if let Some(ref key_id) = claims.key_id {
            match state.api_key_cache.get_key(key_id, &state.engine) {
                Ok(Some(key_record)) => {
                    if key_record.is_revoked {
                        return Err(ErrorResponse::new("API key revoked")
                            .with_status(StatusCode::UNAUTHORIZED)
                            .into_response());
                    }
                    if key_record.expires_at <= chrono::Utc::now().timestamp_millis() {
                        return Err(ErrorResponse::new("API key expired")
                            .with_status(StatusCode::UNAUTHORIZED)
                            .into_response());
                    }
                    key_record.rules
                }
                Ok(None) => {
                    return Err(ErrorResponse::new("API key not found")
                        .with_status(StatusCode::UNAUTHORIZED)
                        .into_response());
                }
                Err(_) => {
                    return Err(ErrorResponse::new("Failed to look up API key")
                        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                        .into_response());
                }
            }
        } else {
            vec![]
        };

        return Ok(SyncCaller::ScopedUser {
            user_id: claims.sub,
            key_rules,
        });
    }

    Err(ErrorResponse::new("Authentication required")
        .with_status(StatusCode::UNAUTHORIZED)
        .into_response())
}

// ---------------------------------------------------------------------------
// Helpers: convert tree structures to sync response types
// ---------------------------------------------------------------------------

fn file_record_to_sync_entry(path: &str, hash: &[u8], record: &FileRecord) -> SyncFileEntry {
    SyncFileEntry {
        path: path.to_string(),
        hash: hex::encode(hash),
        size: record.total_size,
        content_type: record.content_type.clone(),
        chunk_hashes: record.chunk_hashes.iter().map(|h| hex::encode(h)).collect(),
    }
}

fn symlink_record_to_sync_entry(path: &str, hash: &[u8], record: &SymlinkRecord) -> SyncSymlinkEntry {
    SyncSymlinkEntry {
        path: path.to_string(),
        hash: hex::encode(hash),
        target: record.target.clone(),
    }
}

fn path_matches_filter(path: &str, patterns: &[String]) -> bool {
    for pattern in patterns {
        if glob_match::glob_match(pattern, path) {
            return true;
        }
    }
    false
}

/// Check if a path is readable according to API key rules.
/// Empty rules = full access (no restrictions).
fn path_allowed_by_key_rules(path: &str, rules: &[KeyRule]) -> bool {
    if rules.is_empty() {
        return true; // no rules = no path-level restrictions
    }
    match match_rules(rules, path) {
        Some(rule) => check_operation_permitted(&rule.permitted, 'r'),
        None => false,
    }
}

/// Post-process SyncChanges to apply API key rule filtering.
fn filter_changes_by_key_rules(changes: &mut SyncChanges, rules: &[KeyRule]) {
    if rules.is_empty() {
        return;
    }

    changes.files_added.retain(|e| path_allowed_by_key_rules(&e.path, rules));
    changes.files_modified.retain(|e| path_allowed_by_key_rules(&e.path, rules));
    changes.files_deleted.retain(|e| path_allowed_by_key_rules(&e.path, rules));
    changes.symlinks_added.retain(|e| path_allowed_by_key_rules(&e.path, rules));
    changes.symlinks_modified.retain(|e| path_allowed_by_key_rules(&e.path, rules));
    changes.symlinks_deleted.retain(|e| path_allowed_by_key_rules(&e.path, rules));
}

/// Build a full sync response (no since_root_hash -- everything is "added").
/// When `include_system` is false, entries under /.system/ are excluded.
fn build_full_sync_response(
    tree: &VersionTree,
    path_filter: &Option<Vec<String>>,
    include_system: bool,
) -> (SyncChanges, Vec<String>) {
    let mut files_added = Vec::new();
    let mut symlinks_added = Vec::new();
    let mut chunk_hashes: Vec<String> = Vec::new();

    for (path, (hash, record)) in &tree.files {
        if !include_system && crate::engine::directory_ops::is_system_path(path) {
            continue;
        }
        if let Some(ref patterns) = path_filter {
            if !path_matches_filter(path, patterns) {
                continue;
            }
        }
        let entry = file_record_to_sync_entry(path, hash, record);
        chunk_hashes.extend(entry.chunk_hashes.iter().cloned());
        files_added.push(entry);
    }

    for (path, (hash, record)) in &tree.symlinks {
        if !include_system && crate::engine::directory_ops::is_system_path(path) {
            continue;
        }
        if let Some(ref patterns) = path_filter {
            if !path_matches_filter(path, patterns) {
                continue;
            }
        }
        symlinks_added.push(symlink_record_to_sync_entry(path, hash, record));
    }

    // Sort for deterministic output
    files_added.sort_by(|a, b| a.path.cmp(&b.path));
    symlinks_added.sort_by(|a, b| a.path.cmp(&b.path));
    chunk_hashes.sort();
    chunk_hashes.dedup();

    let changes = SyncChanges {
        files_added,
        files_modified: Vec::new(),
        files_deleted: Vec::new(),
        symlinks_added,
        symlinks_modified: Vec::new(),
        symlinks_deleted: Vec::new(),
    };

    (changes, chunk_hashes)
}

/// Build a diff-based sync response from a TreeDiff.
/// When `include_system` is false, entries under /.system/ are excluded.
fn build_sync_response_from_diff(
    diff: &TreeDiff,
    _current_tree: &VersionTree,
    path_filter: &Option<Vec<String>>,
    include_system: bool,
) -> (SyncChanges, Vec<String>) {
    let mut files_added = Vec::new();
    let mut files_modified = Vec::new();
    let mut files_deleted = Vec::new();
    let mut symlinks_added = Vec::new();
    let mut symlinks_modified = Vec::new();
    let mut symlinks_deleted = Vec::new();
    let mut chunk_hashes: Vec<String> = Vec::new();

    for (path, (hash, record)) in &diff.added {
        if !include_system && crate::engine::directory_ops::is_system_path(path) {
            continue;
        }
        if let Some(ref patterns) = path_filter {
            if !path_matches_filter(path, patterns) {
                continue;
            }
        }
        let entry = file_record_to_sync_entry(path, hash, record);
        chunk_hashes.extend(entry.chunk_hashes.iter().cloned());
        files_added.push(entry);
    }

    for (path, (hash, record)) in &diff.modified {
        if !include_system && crate::engine::directory_ops::is_system_path(path) {
            continue;
        }
        if let Some(ref patterns) = path_filter {
            if !path_matches_filter(path, patterns) {
                continue;
            }
        }
        let entry = file_record_to_sync_entry(path, hash, record);
        chunk_hashes.extend(entry.chunk_hashes.iter().cloned());
        files_modified.push(entry);
    }

    for path in &diff.deleted {
        if !include_system && crate::engine::directory_ops::is_system_path(path) {
            continue;
        }
        if let Some(ref patterns) = path_filter {
            if !path_matches_filter(path, patterns) {
                continue;
            }
        }
        files_deleted.push(SyncDeletedEntry { path: path.clone() });
    }

    for (path, (hash, record)) in &diff.symlinks_added {
        if !include_system && crate::engine::directory_ops::is_system_path(path) {
            continue;
        }
        if let Some(ref patterns) = path_filter {
            if !path_matches_filter(path, patterns) {
                continue;
            }
        }
        symlinks_added.push(symlink_record_to_sync_entry(path, hash, record));
    }

    for (path, (hash, record)) in &diff.symlinks_modified {
        if !include_system && crate::engine::directory_ops::is_system_path(path) {
            continue;
        }
        if let Some(ref patterns) = path_filter {
            if !path_matches_filter(path, patterns) {
                continue;
            }
        }
        symlinks_modified.push(symlink_record_to_sync_entry(path, hash, record));
    }

    for path in &diff.symlinks_deleted {
        if !include_system && crate::engine::directory_ops::is_system_path(path) {
            continue;
        }
        if let Some(ref patterns) = path_filter {
            if !path_matches_filter(path, patterns) {
                continue;
            }
        }
        symlinks_deleted.push(SyncDeletedEntry { path: path.clone() });
    }

    // Sort for deterministic output
    files_added.sort_by(|a, b| a.path.cmp(&b.path));
    files_modified.sort_by(|a, b| a.path.cmp(&b.path));
    files_deleted.sort_by(|a, b| a.path.cmp(&b.path));
    symlinks_added.sort_by(|a, b| a.path.cmp(&b.path));
    symlinks_modified.sort_by(|a, b| a.path.cmp(&b.path));
    symlinks_deleted.sort_by(|a, b| a.path.cmp(&b.path));
    chunk_hashes.sort();
    chunk_hashes.dedup();

    let changes = SyncChanges {
        files_added,
        files_modified,
        files_deleted,
        symlinks_added,
        symlinks_modified,
        symlinks_deleted,
    };

    (changes, chunk_hashes)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// POST /sync/diff -- compute and return tree differences.
pub async fn sync_diff(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<SyncDiffRequest>,
) -> Response {
    let caller = match determine_sync_caller(&headers, &state) {
        Ok(c) => c,
        Err(response) => return response,
    };

    let include_system = caller.include_system();

    let vm = VersionManager::new(&state.engine);

    let head_hash = match vm.get_head_hash() {
        Ok(hash) => hash,
        Err(e) => {
            return ErrorResponse::new(format!("Failed to get HEAD: {}", e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    };

    let (mut changes, chunk_hashes) = if let Some(ref since_hex) = payload.since_root_hash {
        let since_hash = match hex::decode(since_hex) {
            Ok(h) => h,
            Err(_) => {
                return ErrorResponse::new("Invalid since_root_hash hex")
                    .with_status(StatusCode::BAD_REQUEST)
                    .into_response()
            }
        };

        let base_tree = match walk_version_tree(&state.engine, &since_hash) {
            Ok(t) => t,
            Err(e) => {
                return ErrorResponse::new(format!("Failed to walk base tree: {}", e))
                    .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                    .into_response()
            }
        };

        let current_tree = match walk_version_tree(&state.engine, &head_hash) {
            Ok(t) => t,
            Err(e) => {
                return ErrorResponse::new(format!("Failed to walk current tree: {}", e))
                    .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                    .into_response()
            }
        };

        let diff = diff_trees(&base_tree, &current_tree);
        build_sync_response_from_diff(&diff, &current_tree, &payload.paths, include_system)
    } else {
        let tree = match walk_version_tree(&state.engine, &head_hash) {
            Ok(t) => t,
            Err(e) => {
                return ErrorResponse::new(format!("Failed to walk tree: {}", e))
                    .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                    .into_response()
            }
        };
        build_full_sync_response(&tree, &payload.paths, include_system)
    };

    // Apply API key rule filtering for scoped users.
    filter_changes_by_key_rules(&mut changes, caller.key_rules());

    let response = SyncDiffResponse {
        root_hash: hex::encode(&head_hash),
        changes,
        chunk_hashes_needed: chunk_hashes,
    };

    (StatusCode::OK, Json(response)).into_response()
}

/// POST /sync/chunks -- batch chunk transfer.
pub async fn sync_chunks(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<SyncChunksRequest>,
) -> Response {
    let caller = match determine_sync_caller(&headers, &state) {
        Ok(c) => c,
        Err(response) => return response,
    };

    let filter_system = !caller.include_system();

    let mut chunks: Vec<serde_json::Value> = Vec::new();

    for hex_hash in &payload.hashes {
        let hash = match hex::decode(hex_hash) {
            Ok(h) => h,
            Err(_) => continue,
        };

        if let Ok(Some((header, _key, value))) = state.engine.get_entry(&hash) {
            // Skip system entries for non-root/non-peer callers.
            if filter_system && header.is_system_entry() {
                continue;
            }

            let data = if header.compression_algo != CompressionAlgorithm::None {
                match decompress(&value, header.compression_algo) {
                    Ok(d) => d,
                    Err(_) => continue,
                }
            } else {
                value
            };

            chunks.push(serde_json::json!({
                "hash": hex_hash,
                "data": base64::engine::general_purpose::STANDARD.encode(&data),
                "size": data.len(),
            }));
        }
    }

    (StatusCode::OK, Json(serde_json::json!({ "chunks": chunks }))).into_response()
}
