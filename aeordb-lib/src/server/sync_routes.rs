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
use crate::engine::tree_walker::{diff_trees, walk_version_tree, TreeDiff, VersionTree};
use crate::engine::version_manager::VersionManager;

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
///
/// The distinction between `Peer` and `RootUser` matters for system-data
/// inclusion. A **replication peer** (another cluster node) calls sync
/// internally with a JWT minted by `SyncEngine::mint_sync_token`, which
/// has `sub: ROOT_USER_ID` AND `scope: "sync"`. Those calls MUST receive
/// `/.aeordb-system/` entries — that's how users, groups, refresh tokens,
/// etc. propagate across the cluster. Anyone else (root admin running
/// curl, scoped users) does **not** get system data through /sync — they
/// should use a backup with `--root-key` or path-based APIs.
pub enum SyncCaller {
    /// Replication peer: `sub: ROOT_USER_ID` + `scope: "sync"`. Full
    /// access INCLUDING `/.aeordb-system/` and `/.aeordb-config/`.
    Peer,
    /// Root JWT (nil UUID), no sync scope — admin tool, not a peer.
    /// `/.aeordb-system/` filtered out (use backup instead).
    RootUser,
    /// Non-root JWT — `/.aeordb-system/` filtered out, API key rules applied.
    ScopedUser {
        // TODO: Use for per-user sync audit logging and rate limiting.
        #[allow(dead_code)]
        user_id: String,
        key_rules: Vec<KeyRule>,
    },
}

impl SyncCaller {
    /// Whether this caller should see /.aeordb-system/ entries.
    /// Only peer replicas include system data; admin root users do NOT.
    fn include_system(&self) -> bool {
        matches!(self, SyncCaller::Peer)
    }

    /// API key rules for path-level filtering (empty = no restrictions).
    fn key_rules(&self) -> &[KeyRule] {
        match self {
            SyncCaller::ScopedUser { key_rules, .. } => key_rules,
            _ => &[],
        }
    }
}

/// Determine the caller identity from request headers.
/// Verifies JWT Bearer token. Returns 401 if no valid auth is present.
fn determine_sync_caller(
    headers: &HeaderMap,
    state: &AppState,
) -> Result<SyncCaller, Response> {
    // 0. If auth is disabled (dev mode), treat as a peer so dev sync flows
    //    see system data — matches the pre-auth-disabled behavior.
    if !state.auth_provider.is_enabled() {
        return Ok(SyncCaller::Peer);
    }

    // 1. Try JWT Bearer token.
    if let Some(auth_header) = headers.get("authorization") {
        let token = auth_header
            .to_str()
            .ok()
            .and_then(|s| s.strip_prefix("Bearer "))
            .ok_or_else(|| {
                ErrorResponse::new("Invalid authorization header: expected 'Bearer <token>' format")
                    .with_status(StatusCode::UNAUTHORIZED)
                    .into_response()
            })?;

        let claims = state.jwt_manager.verify_token(token).map_err(|_| {
            ErrorResponse::new("Invalid or expired JWT. Re-authenticate via POST /auth/token")
                .with_status(StatusCode::UNAUTHORIZED)
                .into_response()
        })?;

        let user_id = uuid::Uuid::parse_str(&claims.sub).map_err(|_| {
            ErrorResponse::new("Invalid user ID in token: 'sub' claim is not a valid UUID")
                .with_status(StatusCode::UNAUTHORIZED)
                .into_response()
        })?;

        if crate::engine::user::is_root(&user_id) {
            // A root JWT with `scope: "sync"` is a replication peer
            // (minted by SyncEngine::mint_sync_token); it MUST receive
            // system data. A root JWT without that scope is an admin tool
            // and must NOT receive system data through sync — use a
            // root-key backup for that purpose.
            if claims.scope.as_deref() == Some("sync") {
                return Ok(SyncCaller::Peer);
            }
            return Ok(SyncCaller::RootUser);
        }

        // Non-root: check API key scoping if key_id is present.
        let key_rules = if let Some(ref key_id) = claims.key_id {
            match state.api_key_cache.get(&key_id.to_string(), &state.engine) {
                Ok(Some(key_record)) => {
                    if key_record.is_revoked {
                        return Err(ErrorResponse::new("API key has been revoked. Create a new key via POST /auth/api-keys")
                            .with_status(StatusCode::UNAUTHORIZED)
                            .into_response());
                    }
                    if key_record.expires_at <= chrono::Utc::now().timestamp_millis() {
                        return Err(ErrorResponse::new("API key expired. Create a new key via POST /auth/api-keys")
                            .with_status(StatusCode::UNAUTHORIZED)
                            .into_response());
                    }
                    key_record.rules
                }
                Ok(None) => {
                    return Err(ErrorResponse::new("API key not found: the key referenced in the token no longer exists")
                        .with_status(StatusCode::UNAUTHORIZED)
                        .into_response());
                }
                Err(_) => {
                    return Err(ErrorResponse::new("Failed to look up API key: could not read from storage. If this persists, check GET /system/health for system status")
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

    Err(ErrorResponse::new("Authentication required. Provide a Bearer token via the Authorization header")
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
/// When `include_system` is false, entries under /.aeordb-system/ are excluded.
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

/// Filter entries from a diff source, applying system-path and glob-pattern checks.
/// Collects converted entries into `dest`. `path_fn` extracts the path from each item,
/// and `convert_fn` produces the output entry.
fn filter_and_collect<I, T, O>(
    source: I,
    include_system: bool,
    path_filter: &Option<Vec<String>>,
    path_fn: impl Fn(&T) -> &str,
    convert_fn: impl Fn(T) -> O,
    dest: &mut Vec<O>,
) where
    I: Iterator<Item = T>,
{
    for item in source {
        let path = path_fn(&item);
        if !include_system && crate::engine::directory_ops::is_system_path(path) {
            continue;
        }
        if let Some(ref patterns) = path_filter {
            if !path_matches_filter(path, patterns) {
                continue;
            }
        }
        dest.push(convert_fn(item));
    }
}

/// Build a diff-based sync response from a TreeDiff.
/// When `include_system` is false, entries under /.aeordb-system/ are excluded.
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

    // Files: added, modified
    filter_and_collect(
        diff.added.iter(),
        include_system, path_filter,
        |(path, _)| path.as_str(),
        |(path, (hash, record))| file_record_to_sync_entry(path, hash, record),
        &mut files_added,
    );
    filter_and_collect(
        diff.modified.iter(),
        include_system, path_filter,
        |(path, _)| path.as_str(),
        |(path, (hash, record))| file_record_to_sync_entry(path, hash, record),
        &mut files_modified,
    );

    // Files: deleted
    filter_and_collect(
        diff.deleted.iter(),
        include_system, path_filter,
        |path| path.as_str(),
        |path| SyncDeletedEntry { path: path.clone() },
        &mut files_deleted,
    );

    // Symlinks: added, modified
    filter_and_collect(
        diff.symlinks_added.iter(),
        include_system, path_filter,
        |(path, _)| path.as_str(),
        |(path, (hash, record))| symlink_record_to_sync_entry(path, hash, record),
        &mut symlinks_added,
    );
    filter_and_collect(
        diff.symlinks_modified.iter(),
        include_system, path_filter,
        |(path, _)| path.as_str(),
        |(path, (hash, record))| symlink_record_to_sync_entry(path, hash, record),
        &mut symlinks_modified,
    );

    // Symlinks: deleted
    filter_and_collect(
        diff.symlinks_deleted.iter(),
        include_system, path_filter,
        |path| path.as_str(),
        |path| SyncDeletedEntry { path: path.clone() },
        &mut symlinks_deleted,
    );

    // Collect chunk hashes from file entries
    let mut chunk_hashes: Vec<String> = Vec::new();
    for entry in files_added.iter().chain(files_modified.iter()) {
        chunk_hashes.extend(entry.chunk_hashes.iter().cloned());
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
    // M3: Cap the number of path filters to prevent abuse.
    if let Some(ref paths) = payload.paths {
        if paths.len() > 100 {
            return ErrorResponse::new("Too many path filters (max 100). Reduce the number of entries in the 'paths' array")
                .with_status(StatusCode::BAD_REQUEST)
                .into_response();
        }
    }

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

    let (mut changes, _unfiltered_chunks) = if let Some(ref since_hex) = payload.since_root_hash {
        let since_hash = match hex::decode(since_hex) {
            Ok(h) => h,
            Err(_) => {
                return ErrorResponse::new("Invalid since_root_hash: value is not valid hex. Use the root_hash from a previous sync response")
                    .with_status(StatusCode::BAD_REQUEST)
                    .into_response()
            }
        };

        let mut base_tree = match walk_version_tree(&state.engine, &since_hash) {
            Ok(t) => t,
            Err(e) => {
                return ErrorResponse::new(format!("Failed to walk base tree: {}", e))
                    .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                    .into_response()
            }
        };

        let mut current_tree = match walk_version_tree(&state.engine, &head_hash) {
            Ok(t) => t,
            Err(e) => {
                return ErrorResponse::new(format!("Failed to walk current tree: {}", e))
                    .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                    .into_response()
            }
        };

        // For replication peers, system subtrees aren't reachable from the
        // user-visible HEAD tree (by design — see tree_walker docs). Walk
        // current state into the current_tree only; base_tree intentionally
        // does NOT include system data, so the diff treats every system
        // file as "added" — the receiving peer dedupes by content hash. We
        // can't accurately reconstruct system state at the historical
        // `since_root_hash` because system data isn't versioned along
        // with HEAD.
        let _ = &mut base_tree; // keep the binding even when we don't augment it
        if include_system {
            crate::engine::tree_walker::augment_with_system_subtrees(&state.engine, &mut current_tree);
        }

        let diff = diff_trees(&base_tree, &current_tree);
        build_sync_response_from_diff(&diff, &current_tree, &payload.paths, include_system)
    } else {
        let mut tree = match walk_version_tree(&state.engine, &head_hash) {
            Ok(t) => t,
            Err(e) => {
                return ErrorResponse::new(format!("Failed to walk tree: {}", e))
                    .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                    .into_response()
            }
        };
        if include_system {
            crate::engine::tree_walker::augment_with_system_subtrees(&state.engine, &mut tree);
        }
        build_full_sync_response(&tree, &payload.paths, include_system)
    };

    // Apply API key rule filtering for scoped users.
    filter_changes_by_key_rules(&mut changes, caller.key_rules());

    // H4: Rebuild chunk hashes from the FILTERED changes so scoped users
    // don't receive chunk hashes for files they can't access.
    let filtered_chunk_hashes: Vec<String> = {
        let mut hashes: Vec<String> = changes.files_added.iter()
            .chain(changes.files_modified.iter())
            .flat_map(|e| e.chunk_hashes.iter().cloned())
            .collect();
        hashes.sort();
        hashes.dedup();
        hashes
    };

    let response = SyncDiffResponse {
        root_hash: hex::encode(&head_hash),
        changes,
        chunk_hashes_needed: filtered_chunk_hashes,
    };

    (StatusCode::OK, Json(response)).into_response()
}

/// POST /sync/chunks -- batch chunk transfer.
pub async fn sync_chunks(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<SyncChunksRequest>,
) -> Response {
    // M3: Cap the number of chunk hashes to prevent abuse.
    if payload.hashes.len() > 10_000 {
        return ErrorResponse::new("Too many chunk hashes (max 10000). Split the request into multiple batches")
            .with_status(StatusCode::BAD_REQUEST)
            .into_response();
    }

    let caller = match determine_sync_caller(&headers, &state) {
        Ok(c) => c,
        Err(response) => return response,
    };

    let filter_system = !caller.include_system();

    // Scoped-key enforcement: a key with rules (non-empty key_rules) could
    // exfiltrate chunks outside its scope by guessing hashes, because
    // /sync/chunks identifies content by hash and provides no path
    // context. Refuse explicitly. Non-root callers WITHOUT rules (regular
    // client sync) are still allowed — they only see non-system chunks
    // via the `filter_system` gate below.
    if let SyncCaller::ScopedUser { key_rules, .. } = &caller {
        if !key_rules.is_empty() {
            return ErrorResponse::new(
                "Scoped API keys (with path rules) cannot use /sync/chunks. \
                 Use /files/{path} for path-aware content access."
            )
                .with_status(StatusCode::FORBIDDEN)
                .into_response();
        }
    }

    let mut chunks: Vec<serde_json::Value> = Vec::new();

    for hex_hash in &payload.hashes {
        let hash = match hex::decode(hex_hash) {
            Ok(h) => h,
            Err(_) => continue,
        };

        if let Ok(Some((header, _key, value))) = state.engine.get_entry_including_deleted(&hash) {
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
