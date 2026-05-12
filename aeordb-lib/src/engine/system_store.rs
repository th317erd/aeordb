//! System store: typed system data operations backed by `DirectoryOps`.
//!
//! All data is stored as regular files under `/.aeordb-system/` in the directory
//! tree, which means system data automatically participates in replication
//! and versioning.
//!
//! This module is the Phase 2 replacement for `system_tables.rs`, which
//! uses loose KV entries with BLAKE3-hashed domain-prefixed keys.

use uuid::Uuid;

use crate::auth::api_key::ApiKeyRecord;
use crate::auth::magic_link::MagicLinkRecord;
use crate::auth::refresh::RefreshTokenRecord;
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::group::Group;
use crate::engine::json_store::{JsonDoc, JsonStore};
use crate::engine::peer_connection::PeerConfig;
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::user::{User, validate_user_id};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Store a config value by key.
pub fn store_config(
    engine: &StorageEngine,
    ctx: &RequestContext,
    key: &str,
    value: &[u8],
) -> EngineResult<()> {
    let ops = DirectoryOps::new(engine);
    let path = format!("/.aeordb-system/config/{}", key);
    ops.store_file(ctx, &path, value, Some("application/octet-stream"))?;
    Ok(())
}

/// Retrieve a config value by key.
pub fn get_config(engine: &StorageEngine, key: &str) -> EngineResult<Option<Vec<u8>>> {
    let ops = DirectoryOps::new(engine);
    let path = format!("/.aeordb-system/config/{}", key);
    match ops.read_file(&path) {
        Ok(data) => Ok(Some(data)),
        Err(EngineError::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

// ---------------------------------------------------------------------------
// API Keys
// ---------------------------------------------------------------------------

/// Store an API key record.
/// SECURITY: Validates that user_id is not the nil UUID (root).
/// Use `store_api_key_for_bootstrap` for the root bootstrap key only.
pub fn store_api_key(
    engine: &StorageEngine,
    ctx: &RequestContext,
    record: &ApiKeyRecord,
) -> EngineResult<()> {
    if let Some(ref uid) = record.user_id {
        validate_user_id(uid)?;
    }
    store_api_key_unchecked(engine, ctx, record)
}

/// SECURITY WARNING: This method allows storing an API key with the nil UUID
/// (root user_id). It exists SOLELY for the bootstrap process that creates
/// the initial root API key. NEVER expose this method to any external
/// interface (HTTP, WASM plugins, native plugins, admin paths).
pub fn store_api_key_for_bootstrap(
    engine: &StorageEngine,
    ctx: &RequestContext,
    record: &ApiKeyRecord,
) -> EngineResult<()> {
    store_api_key_unchecked(engine, ctx, record)
}

static API_KEY_STORE: JsonStore<ApiKeyRecord> =
    JsonStore::new("/.aeordb-system/api-keys");

/// Internal: store an API key record without user_id validation.
fn store_api_key_unchecked(
    engine: &StorageEngine,
    ctx: &RequestContext,
    record: &ApiKeyRecord,
) -> EngineResult<()> {
    API_KEY_STORE.put(engine, ctx, &record.key_id.to_string(), record)
}

/// Look up a single API key record by key_id prefix (first 16 hex chars
/// of the UUID, no dashes). Scan-based secondary lookup — no index yet.
pub fn get_api_key_by_prefix(
    engine: &StorageEngine,
    key_id_prefix: &str,
) -> EngineResult<Option<ApiKeyRecord>> {
    for record in API_KEY_STORE.list(engine)? {
        let simple = record.key_id.simple().to_string();
        if &simple[..16] == key_id_prefix {
            return Ok(Some(record));
        }
    }
    Ok(None)
}

/// Get an API key record by its UUID. Returns `Ok(None)` if no such key.
pub fn get_api_key(
    engine: &StorageEngine,
    key_id: Uuid,
) -> EngineResult<Option<ApiKeyRecord>> {
    API_KEY_STORE.get(engine, &key_id.to_string())
}

/// List all API key records.
pub fn list_api_keys(engine: &StorageEngine) -> EngineResult<Vec<ApiKeyRecord>> {
    API_KEY_STORE.list(engine)
}

/// Revoke an API key by setting is_revoked = true.
/// Returns true if the key was found, false otherwise.
pub fn revoke_api_key(
    engine: &StorageEngine,
    ctx: &RequestContext,
    key_id: Uuid,
) -> EngineResult<bool> {
    let mut record = match API_KEY_STORE.get(engine, &key_id.to_string())? {
        Some(record) => record,
        None => return Ok(false),
    };
    record.is_revoked = true;
    API_KEY_STORE.put(engine, ctx, &key_id.to_string(), &record)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Users
// ---------------------------------------------------------------------------

static USER_STORE: JsonStore<User> = JsonStore::new("/.aeordb-system/users");

/// Store a user. Validates user_id != nil UUID.
/// Automatically creates a per-user auto-group `user:{user_id}`.
pub fn store_user(
    engine: &StorageEngine,
    ctx: &RequestContext,
    user: &User,
) -> EngineResult<()> {
    validate_user_id(&user.user_id)?;
    USER_STORE.put(engine, ctx, &user.user_id.to_string(), user)?;

    // Create per-user auto-group.
    let group_name = format!("user:{}", user.user_id);
    let auto_group = Group::new(
        &group_name,
        "crudlify",
        "........",
        "user_id",
        "eq",
        &user.user_id.to_string(),
    )?;
    store_group(engine, ctx, &auto_group)?;

    Ok(())
}

/// Retrieve a user by user_id.
pub fn get_user(engine: &StorageEngine, user_id: &Uuid) -> EngineResult<Option<User>> {
    USER_STORE.get(engine, &user_id.to_string())
}

/// List all users.
pub fn list_users(engine: &StorageEngine) -> EngineResult<Vec<User>> {
    USER_STORE.list(engine)
}

/// Retrieve a user by username (scan-based; no secondary index).
pub fn get_user_by_username(engine: &StorageEngine, username: &str) -> EngineResult<Option<User>> {
    let users = list_users(engine)?;
    Ok(users.into_iter().find(|user| user.username == username))
}

/// Update an existing user. Validates user_id != nil UUID.
/// Does NOT recreate the auto-group (use store_user for initial creation).
pub fn update_user(
    engine: &StorageEngine,
    ctx: &RequestContext,
    user: &User,
) -> EngineResult<()> {
    validate_user_id(&user.user_id)?;
    USER_STORE.put(engine, ctx, &user.user_id.to_string(), user)
}

/// Count all users.
pub fn count_users(engine: &StorageEngine) -> EngineResult<u64> {
    let ops = DirectoryOps::new(engine);
    let entries = match ops.list_directory("/.aeordb-system/users") {
        Ok(entries) => entries,
        Err(EngineError::NotFound(_)) => return Ok(0),
        Err(error) => return Err(error),
    };
    Ok(entries.len() as u64)
}

/// Delete a user. Also deletes the per-user auto-group.
/// Returns true if the user existed, false otherwise.
pub fn delete_user(
    engine: &StorageEngine,
    ctx: &RequestContext,
    user_id: &Uuid,
) -> EngineResult<bool> {
    let ops = DirectoryOps::new(engine);
    let path = format!("/.aeordb-system/users/{}", user_id);
    match ops.delete_file(ctx, &path) {
        Ok(()) => {}
        Err(EngineError::NotFound(_)) => return Ok(false),
        Err(error) => return Err(error),
    }

    // Delete the auto-group (best-effort).
    let group_name = format!("user:{}", user_id);
    let _ = delete_group(engine, ctx, &group_name);

    Ok(true)
}

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

static GROUP_STORE: JsonStore<Group> = JsonStore::new("/.aeordb-system/groups");

/// Store a group.
pub fn store_group(
    engine: &StorageEngine,
    ctx: &RequestContext,
    group: &Group,
) -> EngineResult<()> {
    GROUP_STORE.put(engine, ctx, &group.name, group)
}

/// Retrieve a group by name.
pub fn get_group(engine: &StorageEngine, name: &str) -> EngineResult<Option<Group>> {
    GROUP_STORE.get(engine, name)
}

/// List all groups.
pub fn list_groups(engine: &StorageEngine) -> EngineResult<Vec<Group>> {
    GROUP_STORE.list(engine)
}

/// Update a group. Currently identical to `store_group`; kept distinct for
/// callers that want to express update intent.
pub fn update_group(
    engine: &StorageEngine,
    ctx: &RequestContext,
    group: &Group,
) -> EngineResult<()> {
    store_group(engine, ctx, group)
}

/// Delete a group. Returns `Ok(true)` if it existed, `Ok(false)` if not.
pub fn delete_group(
    engine: &StorageEngine,
    ctx: &RequestContext,
    name: &str,
) -> EngineResult<bool> {
    GROUP_STORE.delete(engine, ctx, name)
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

/// Store permissions for a path. The path is BLAKE3-hashed to avoid nested
/// directory issues from arbitrary path strings.
pub fn store_permissions(
    engine: &StorageEngine,
    ctx: &RequestContext,
    path: &str,
    permissions_json: &[u8],
) -> EngineResult<()> {
    let ops = DirectoryOps::new(engine);
    let path_hash = blake3::hash(path.as_bytes());
    let store_path = format!("/.aeordb-system/permissions/{}", path_hash.to_hex());
    ops.store_file(ctx, &store_path, permissions_json, Some("application/json"))?;
    Ok(())
}

/// Retrieve permissions for a path.
pub fn get_permissions(engine: &StorageEngine, path: &str) -> EngineResult<Option<Vec<u8>>> {
    let ops = DirectoryOps::new(engine);
    let path_hash = blake3::hash(path.as_bytes());
    let store_path = format!("/.aeordb-system/permissions/{}", path_hash.to_hex());
    match ops.read_file(&store_path) {
        Ok(data) => Ok(Some(data)),
        Err(EngineError::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

// ---------------------------------------------------------------------------
// Magic Links
// ---------------------------------------------------------------------------

static MAGIC_LINK_STORE: JsonStore<MagicLinkRecord> =
    JsonStore::new("/.aeordb-system/magic-links");

/// Store a magic link record.
pub fn store_magic_link(
    engine: &StorageEngine,
    ctx: &RequestContext,
    record: &MagicLinkRecord,
) -> EngineResult<()> {
    MAGIC_LINK_STORE.put(engine, ctx, &record.code_hash, record)
}

/// Retrieve a magic link record by code_hash.
pub fn get_magic_link(
    engine: &StorageEngine,
    code_hash: &str,
) -> EngineResult<Option<MagicLinkRecord>> {
    MAGIC_LINK_STORE.get(engine, code_hash)
}

/// Mark a magic link as used.
pub fn mark_magic_link_used(
    engine: &StorageEngine,
    ctx: &RequestContext,
    code_hash: &str,
) -> EngineResult<()> {
    let mut record = match get_magic_link(engine, code_hash)? {
        Some(record) => record,
        None => return Err(EngineError::NotFound(format!("magic link not found: {}", code_hash))),
    };
    record.is_used = true;
    store_magic_link(engine, ctx, &record)
}

// ---------------------------------------------------------------------------
// Refresh Tokens
// ---------------------------------------------------------------------------

static REFRESH_TOKEN_STORE: JsonStore<RefreshTokenRecord> =
    JsonStore::new("/.aeordb-system/refresh-tokens");

/// Store a refresh token record.
pub fn store_refresh_token(
    engine: &StorageEngine,
    ctx: &RequestContext,
    record: &RefreshTokenRecord,
) -> EngineResult<()> {
    REFRESH_TOKEN_STORE.put(engine, ctx, &record.token_hash, record)
}

/// Retrieve a refresh token record by token_hash.
pub fn get_refresh_token(
    engine: &StorageEngine,
    token_hash: &str,
) -> EngineResult<Option<RefreshTokenRecord>> {
    REFRESH_TOKEN_STORE.get(engine, token_hash)
}

/// Revoke a refresh token by setting is_revoked = true.
/// Returns true if the token was found, false otherwise.
pub fn revoke_refresh_token(
    engine: &StorageEngine,
    ctx: &RequestContext,
    token_hash: &str,
) -> EngineResult<bool> {
    let mut record = match REFRESH_TOKEN_STORE.get(engine, token_hash)? {
        Some(record) => record,
        None => return Ok(false),
    };
    record.is_revoked = true;
    REFRESH_TOKEN_STORE.put(engine, ctx, token_hash, &record)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Cleanup: Expired Tokens & Used/Expired Magic Links
// ---------------------------------------------------------------------------

/// Clean up expired/revoked refresh tokens and used/expired magic links.
/// Returns `(tokens_cleaned, links_cleaned)`.
///
/// This function is idempotent and safe to run concurrently — each iteration
/// independently scans the directory and deletes qualifying entries.
pub fn cleanup_expired_tokens(
    engine: &StorageEngine,
    ctx: &RequestContext,
) -> EngineResult<(usize, usize)> {
    let ops = DirectoryOps::new(engine);
    let now = chrono::Utc::now();
    let mut tokens_cleaned = 0;
    let mut links_cleaned = 0;

    // Clean up refresh tokens
    let token_entries = match ops.list_directory("/.aeordb-system/refresh-tokens") {
        Ok(entries) => entries,
        Err(EngineError::NotFound(_)) => Vec::new(),
        Err(e) => return Err(e),
    };

    for entry in &token_entries {
        let path = format!("/.aeordb-system/refresh-tokens/{}", entry.name);
        if let Ok(data) = ops.read_file(&path) {
            if let Ok(record) = serde_json::from_slice::<RefreshTokenRecord>(&data) {
                if record.is_revoked || record.expires_at < now {
                    let _ = ops.delete_file(ctx, &path);
                    tokens_cleaned += 1;
                }
            }
        }
    }

    // Clean up magic links
    let link_entries = match ops.list_directory("/.aeordb-system/magic-links") {
        Ok(entries) => entries,
        Err(EngineError::NotFound(_)) => Vec::new(),
        Err(e) => return Err(e),
    };

    for entry in &link_entries {
        let path = format!("/.aeordb-system/magic-links/{}", entry.name);
        if let Ok(data) = ops.read_file(&path) {
            if let Ok(record) = serde_json::from_slice::<MagicLinkRecord>(&data) {
                if record.is_used || record.expires_at < now {
                    let _ = ops.delete_file(ctx, &path);
                    links_cleaned += 1;
                }
            }
        }
    }

    if tokens_cleaned > 0 || links_cleaned > 0 {
        tracing::info!(
            tokens_cleaned = tokens_cleaned,
            links_cleaned = links_cleaned,
            "Cleaned up expired tokens and used/expired magic links",
        );
    }

    metrics::counter!(crate::metrics::definitions::CLEANUP_TOKENS_TOTAL)
        .increment(tokens_cleaned as u64);
    metrics::counter!(crate::metrics::definitions::CLEANUP_LINKS_TOTAL)
        .increment(links_cleaned as u64);

    Ok((tokens_cleaned, links_cleaned))
}

// ---------------------------------------------------------------------------
// Cluster / Replication
// ---------------------------------------------------------------------------

/// Persist this node's unique identifier.
pub fn store_node_id(
    engine: &StorageEngine,
    ctx: &RequestContext,
    node_id: u64,
) -> EngineResult<()> {
    let ops = DirectoryOps::new(engine);
    let value = node_id.to_le_bytes().to_vec();
    ops.store_file(ctx, "/.aeordb-system/cluster/node_id", &value, Some("application/octet-stream"))?;
    Ok(())
}

/// Load the persisted node identifier, if any.
pub fn get_node_id(engine: &StorageEngine) -> EngineResult<Option<u64>> {
    let ops = DirectoryOps::new(engine);
    match ops.read_file("/.aeordb-system/cluster/node_id") {
        Ok(data) if data.len() == 8 => {
            Ok(Some(u64::from_le_bytes(data[..8].try_into().unwrap())))
        }
        Ok(_) => Ok(None), // wrong length — treat as missing
        Err(EngineError::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

static PEER_CONFIGS_DOC: JsonDoc<Vec<PeerConfig>> =
    JsonDoc::new("/.aeordb-system/cluster/peers");

/// Persist the full set of peer configurations.
pub fn store_peer_configs(
    engine: &StorageEngine,
    ctx: &RequestContext,
    peers: &[PeerConfig],
) -> EngineResult<()> {
    PEER_CONFIGS_DOC.put(engine, ctx, &peers.to_vec())
}

/// Load persisted peer configurations.
pub fn get_peer_configs(engine: &StorageEngine) -> EngineResult<Vec<PeerConfig>> {
    PEER_CONFIGS_DOC.get_or_default(engine, Vec::new())
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// Deploy (or overwrite) a plugin at the given key.
/// Encode a plugin key for safe storage as a filename.
/// Replaces '/' with '::' to avoid creating nested directories.
fn encode_plugin_key(key: &str) -> String {
    key.replace('/', "::")
}

/// Decode a plugin key from the filename back to the original key.
fn decode_plugin_key(encoded: &str) -> String {
    encoded.replace("::", "/")
}

pub fn store_plugin(
    engine: &StorageEngine,
    ctx: &RequestContext,
    key: &str,
    encoded: &[u8],
) -> EngineResult<()> {
    let ops = DirectoryOps::new(engine);
    let safe_key = encode_plugin_key(key);
    let path = format!("/.aeordb-system/plugins/{}", safe_key);
    ops.store_file(ctx, &path, encoded, Some("application/octet-stream"))?;
    Ok(())
}

/// Retrieve a plugin by key.
pub fn get_plugin(engine: &StorageEngine, key: &str) -> EngineResult<Option<Vec<u8>>> {
    let ops = DirectoryOps::new(engine);
    let safe_key = encode_plugin_key(key);
    let path = format!("/.aeordb-system/plugins/{}", safe_key);
    match ops.read_file(&path) {
        Ok(data) => Ok(Some(data)),
        Err(EngineError::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// List all plugins, returning (key, encoded_bytes) for each.
/// Plugin keys are encoded with '::' replacing '/' for flat storage.
pub fn list_plugins(engine: &StorageEngine) -> EngineResult<Vec<(String, Vec<u8>)>> {
    let ops = DirectoryOps::new(engine);
    let entries = match ops.list_directory("/.aeordb-system/plugins") {
        Ok(entries) => entries,
        Err(EngineError::NotFound(_)) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let mut results = Vec::new();
    for entry in &entries {
        let path = format!("/.aeordb-system/plugins/{}", entry.name);
        if let Ok(data) = ops.read_file(&path) {
            let original_key = decode_plugin_key(&entry.name);
            results.push((original_key, data));
        }
    }
    Ok(results)
}

/// Remove a plugin by key.
/// Returns true if the plugin existed, false otherwise.
pub fn remove_plugin(
    engine: &StorageEngine,
    ctx: &RequestContext,
    key: &str,
) -> EngineResult<bool> {
    let ops = DirectoryOps::new(engine);
    let safe_key = encode_plugin_key(key);
    let path = format!("/.aeordb-system/plugins/{}", safe_key);
    match ops.delete_file(ctx, &path) {
        Ok(()) => Ok(true),
        Err(EngineError::NotFound(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

// ---------------------------------------------------------------------------
// Peer Sync State
// ---------------------------------------------------------------------------

static PEER_SYNC_STATE_STORE: JsonStore<crate::engine::sync_engine::PeerSyncState> =
    JsonStore::new("/.aeordb-system/sync-peers");

/// Persist sync state for a specific peer.
pub fn store_peer_sync_state(
    engine: &StorageEngine,
    ctx: &RequestContext,
    peer_node_id: u64,
    state: &crate::engine::sync_engine::PeerSyncState,
) -> EngineResult<()> {
    PEER_SYNC_STATE_STORE.put(engine, ctx, &peer_node_id.to_string(), state)
}

/// Load sync state for a specific peer.
pub fn get_peer_sync_state(
    engine: &StorageEngine,
    peer_node_id: u64,
) -> EngineResult<Option<crate::engine::sync_engine::PeerSyncState>> {
    PEER_SYNC_STATE_STORE.get(engine, &peer_node_id.to_string())
}

// ---------------------------------------------------------------------------
// Startup Migration: Rename legacy system paths
// ---------------------------------------------------------------------------

/// Migrate data from legacy system paths to their new canonical names.
///
/// Path renames:
///   `/.aeordb-system/apikeys/`       -> `/.aeordb-system/api-keys/`
///   `/.aeordb-system/cluster/sync/`  -> `/.aeordb-system/sync-peers/`
///
/// This function is idempotent: if the old path does not exist (or is empty),
/// it is silently skipped. Safe to call on every startup.
pub fn migrate_system_paths(engine: &StorageEngine) -> EngineResult<()> {
    let ops = DirectoryOps::new(engine);
    let ctx = RequestContext::system();

    migrate_directory(&ops, &ctx, "/.aeordb-system/apikeys", "/.aeordb-system/api-keys")?;
    migrate_directory(&ops, &ctx, "/.aeordb-system/cluster/sync", "/.aeordb-system/sync-peers")?;

    Ok(())
}

/// Move all entries from `old_dir` to `new_dir`, preserving filenames.
/// Skips entries that already exist at the new path (idempotent).
fn migrate_directory(
    ops: &DirectoryOps,
    ctx: &RequestContext,
    old_dir: &str,
    new_dir: &str,
) -> EngineResult<()> {
    let entries = match ops.list_directory(old_dir) {
        Ok(entries) => entries,
        Err(EngineError::NotFound(_)) => return Ok(()), // nothing to migrate
        Err(error) => return Err(error),
    };

    if entries.is_empty() {
        return Ok(());
    }

    tracing::info!(
        old_path = %old_dir,
        new_path = %new_dir,
        entry_count = entries.len(),
        "Migrating system path entries",
    );

    for entry in &entries {
        let old_path = format!("{}/{}", old_dir, entry.name);
        let new_path = format!("{}/{}", new_dir, entry.name);

        // Skip if the entry already exists at the new path (idempotent).
        match ops.read_file(&new_path) {
            Ok(_) => {
                tracing::info!(
                    old = %old_path,
                    new = %new_path,
                    "Entry already exists at new path, skipping",
                );
                continue;
            }
            Err(EngineError::NotFound(_)) => {} // expected — proceed with migration
            Err(error) => return Err(error),
        }

        let data = ops.read_file(&old_path)?;
        ops.store_file(ctx, &new_path, &data, Some("application/octet-stream"))?;
        ops.delete_file(ctx, &old_path)?;

        tracing::info!(
            old = %old_path,
            new = %new_path,
            "Migrated system entry",
        );
    }

    Ok(())
}
