//! System store: typed system data operations backed by `DirectoryOps`.
//!
//! All data is stored as regular files under `/.system/` in the directory
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
    let path = format!("/.system/config/{}", key);
    ops.store_file(ctx, &path, value, Some("application/octet-stream"))?;
    Ok(())
}

/// Retrieve a config value by key.
pub fn get_config(engine: &StorageEngine, key: &str) -> EngineResult<Option<Vec<u8>>> {
    let ops = DirectoryOps::new(engine);
    let path = format!("/.system/config/{}", key);
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
    validate_user_id(&record.user_id)?;
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

/// Internal: store an API key record without user_id validation.
fn store_api_key_unchecked(
    engine: &StorageEngine,
    ctx: &RequestContext,
    record: &ApiKeyRecord,
) -> EngineResult<()> {
    let ops = DirectoryOps::new(engine);
    let path = format!("/.system/apikeys/{}", record.key_id);
    let json = serde_json::to_vec(record)
        .map_err(|error| EngineError::JsonParseError(error.to_string()))?;
    ops.store_file(ctx, &path, &json, Some("application/json"))?;
    Ok(())
}

/// Look up a single API key record by key_id prefix (first 16 hex chars
/// of the UUID, no dashes).
pub fn get_api_key_by_prefix(
    engine: &StorageEngine,
    key_id_prefix: &str,
) -> EngineResult<Option<ApiKeyRecord>> {
    let ops = DirectoryOps::new(engine);
    let entries = match ops.list_directory("/.system/apikeys") {
        Ok(entries) => entries,
        Err(EngineError::NotFound(_)) => return Ok(None),
        Err(error) => return Err(error),
    };

    for entry in &entries {
        let uuid = match Uuid::parse_str(&entry.name) {
            Ok(uuid) => uuid,
            Err(_) => continue, // skip non-UUID filenames
        };

        let simple = uuid.simple().to_string();
        let record_prefix = &simple[..16];
        if record_prefix != key_id_prefix {
            continue;
        }

        let path = format!("/.system/apikeys/{}", entry.name);
        if let Ok(data) = ops.read_file(&path) {
            if let Ok(record) = serde_json::from_slice::<ApiKeyRecord>(&data) {
                if !record.is_revoked {
                    return Ok(Some(record));
                }
            }
        }
    }
    Ok(None)
}

/// List all API key records.
pub fn list_api_keys(engine: &StorageEngine) -> EngineResult<Vec<ApiKeyRecord>> {
    let ops = DirectoryOps::new(engine);
    let entries = match ops.list_directory("/.system/apikeys") {
        Ok(entries) => entries,
        Err(EngineError::NotFound(_)) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let mut records = Vec::new();
    for entry in &entries {
        let path = format!("/.system/apikeys/{}", entry.name);
        if let Ok(data) = ops.read_file(&path) {
            if let Ok(record) = serde_json::from_slice::<ApiKeyRecord>(&data) {
                records.push(record);
            }
        }
    }
    Ok(records)
}

/// Revoke an API key by setting is_revoked = true.
/// Returns true if the key was found, false otherwise.
pub fn revoke_api_key(
    engine: &StorageEngine,
    ctx: &RequestContext,
    key_id: Uuid,
) -> EngineResult<bool> {
    let ops = DirectoryOps::new(engine);
    let path = format!("/.system/apikeys/{}", key_id);
    let data = match ops.read_file(&path) {
        Ok(data) => data,
        Err(EngineError::NotFound(_)) => return Ok(false),
        Err(error) => return Err(error),
    };

    let mut record: ApiKeyRecord = serde_json::from_slice(&data)
        .map_err(|error| EngineError::JsonParseError(error.to_string()))?;
    record.is_revoked = true;

    let json = serde_json::to_vec(&record)
        .map_err(|error| EngineError::JsonParseError(error.to_string()))?;
    ops.store_file(ctx, &path, &json, Some("application/json"))?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Users
// ---------------------------------------------------------------------------

/// Store a user. Validates user_id != nil UUID.
/// Automatically creates a per-user auto-group `user:{user_id}`.
pub fn store_user(
    engine: &StorageEngine,
    ctx: &RequestContext,
    user: &User,
) -> EngineResult<()> {
    validate_user_id(&user.user_id)?;

    let ops = DirectoryOps::new(engine);
    let path = format!("/.system/users/{}", user.user_id);
    let json = serde_json::to_vec(user)
        .map_err(|error| EngineError::JsonParseError(error.to_string()))?;
    ops.store_file(ctx, &path, &json, Some("application/json"))?;

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
    let ops = DirectoryOps::new(engine);
    let path = format!("/.system/users/{}", user_id);
    match ops.read_file(&path) {
        Ok(data) => {
            let user: User = serde_json::from_slice(&data)
                .map_err(|error| EngineError::JsonParseError(error.to_string()))?;
            Ok(Some(user))
        }
        Err(EngineError::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// List all users.
pub fn list_users(engine: &StorageEngine) -> EngineResult<Vec<User>> {
    let ops = DirectoryOps::new(engine);
    let entries = match ops.list_directory("/.system/users") {
        Ok(entries) => entries,
        Err(EngineError::NotFound(_)) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let mut users = Vec::new();
    for entry in &entries {
        let path = format!("/.system/users/{}", entry.name);
        if let Ok(data) = ops.read_file(&path) {
            if let Ok(user) = serde_json::from_slice::<User>(&data) {
                users.push(user);
            }
        }
    }
    Ok(users)
}

/// Delete a user. Also deletes the per-user auto-group.
/// Returns true if the user existed, false otherwise.
pub fn delete_user(
    engine: &StorageEngine,
    ctx: &RequestContext,
    user_id: &Uuid,
) -> EngineResult<bool> {
    let ops = DirectoryOps::new(engine);
    let path = format!("/.system/users/{}", user_id);
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

/// Store a group.
pub fn store_group(
    engine: &StorageEngine,
    ctx: &RequestContext,
    group: &Group,
) -> EngineResult<()> {
    let ops = DirectoryOps::new(engine);
    let path = format!("/.system/groups/{}", group.name);
    let json = serde_json::to_vec(group)
        .map_err(|error| EngineError::JsonParseError(error.to_string()))?;
    ops.store_file(ctx, &path, &json, Some("application/json"))?;
    Ok(())
}

/// Retrieve a group by name.
pub fn get_group(engine: &StorageEngine, name: &str) -> EngineResult<Option<Group>> {
    let ops = DirectoryOps::new(engine);
    let path = format!("/.system/groups/{}", name);
    match ops.read_file(&path) {
        Ok(data) => {
            let group: Group = serde_json::from_slice(&data)
                .map_err(|error| EngineError::JsonParseError(error.to_string()))?;
            Ok(Some(group))
        }
        Err(EngineError::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// List all groups.
pub fn list_groups(engine: &StorageEngine) -> EngineResult<Vec<Group>> {
    let ops = DirectoryOps::new(engine);
    let entries = match ops.list_directory("/.system/groups") {
        Ok(entries) => entries,
        Err(EngineError::NotFound(_)) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let mut groups = Vec::new();
    for entry in &entries {
        let path = format!("/.system/groups/{}", entry.name);
        if let Ok(data) = ops.read_file(&path) {
            if let Ok(group) = serde_json::from_slice::<Group>(&data) {
                groups.push(group);
            }
        }
    }
    Ok(groups)
}

/// Delete a group.
/// Returns true if the group existed, false otherwise.
pub fn delete_group(
    engine: &StorageEngine,
    ctx: &RequestContext,
    name: &str,
) -> EngineResult<bool> {
    let ops = DirectoryOps::new(engine);
    let path = format!("/.system/groups/{}", name);
    match ops.delete_file(ctx, &path) {
        Ok(()) => Ok(true),
        Err(EngineError::NotFound(_)) => Ok(false),
        Err(error) => Err(error),
    }
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
    let store_path = format!("/.system/permissions/{}", path_hash.to_hex());
    ops.store_file(ctx, &store_path, permissions_json, Some("application/json"))?;
    Ok(())
}

/// Retrieve permissions for a path.
pub fn get_permissions(engine: &StorageEngine, path: &str) -> EngineResult<Option<Vec<u8>>> {
    let ops = DirectoryOps::new(engine);
    let path_hash = blake3::hash(path.as_bytes());
    let store_path = format!("/.system/permissions/{}", path_hash.to_hex());
    match ops.read_file(&store_path) {
        Ok(data) => Ok(Some(data)),
        Err(EngineError::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

// ---------------------------------------------------------------------------
// Magic Links
// ---------------------------------------------------------------------------

/// Store a magic link record.
pub fn store_magic_link(
    engine: &StorageEngine,
    ctx: &RequestContext,
    record: &MagicLinkRecord,
) -> EngineResult<()> {
    let ops = DirectoryOps::new(engine);
    let path = format!("/.system/magic-links/{}", record.code_hash);
    let json = serde_json::to_vec(record)
        .map_err(|error| EngineError::JsonParseError(error.to_string()))?;
    ops.store_file(ctx, &path, &json, Some("application/json"))?;
    Ok(())
}

/// Retrieve a magic link record by code_hash.
pub fn get_magic_link(
    engine: &StorageEngine,
    code_hash: &str,
) -> EngineResult<Option<MagicLinkRecord>> {
    let ops = DirectoryOps::new(engine);
    let path = format!("/.system/magic-links/{}", code_hash);
    match ops.read_file(&path) {
        Ok(data) => {
            let record: MagicLinkRecord = serde_json::from_slice(&data)
                .map_err(|error| EngineError::JsonParseError(error.to_string()))?;
            Ok(Some(record))
        }
        Err(EngineError::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

// ---------------------------------------------------------------------------
// Refresh Tokens
// ---------------------------------------------------------------------------

/// Store a refresh token record.
pub fn store_refresh_token(
    engine: &StorageEngine,
    ctx: &RequestContext,
    record: &RefreshTokenRecord,
) -> EngineResult<()> {
    let ops = DirectoryOps::new(engine);
    let path = format!("/.system/refresh-tokens/{}", record.token_hash);
    let json = serde_json::to_vec(record)
        .map_err(|error| EngineError::JsonParseError(error.to_string()))?;
    ops.store_file(ctx, &path, &json, Some("application/json"))?;
    Ok(())
}

/// Retrieve a refresh token record by token_hash.
pub fn get_refresh_token(
    engine: &StorageEngine,
    token_hash: &str,
) -> EngineResult<Option<RefreshTokenRecord>> {
    let ops = DirectoryOps::new(engine);
    let path = format!("/.system/refresh-tokens/{}", token_hash);
    match ops.read_file(&path) {
        Ok(data) => {
            let record: RefreshTokenRecord = serde_json::from_slice(&data)
                .map_err(|error| EngineError::JsonParseError(error.to_string()))?;
            Ok(Some(record))
        }
        Err(EngineError::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Revoke a refresh token by setting is_revoked = true.
/// Returns true if the token was found, false otherwise.
pub fn revoke_refresh_token(
    engine: &StorageEngine,
    ctx: &RequestContext,
    token_hash: &str,
) -> EngineResult<bool> {
    let ops = DirectoryOps::new(engine);
    let path = format!("/.system/refresh-tokens/{}", token_hash);
    let data = match ops.read_file(&path) {
        Ok(data) => data,
        Err(EngineError::NotFound(_)) => return Ok(false),
        Err(error) => return Err(error),
    };

    let mut record: RefreshTokenRecord = serde_json::from_slice(&data)
        .map_err(|error| EngineError::JsonParseError(error.to_string()))?;
    record.is_revoked = true;

    let json = serde_json::to_vec(&record)
        .map_err(|error| EngineError::JsonParseError(error.to_string()))?;
    ops.store_file(ctx, &path, &json, Some("application/json"))?;
    Ok(true)
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
    ops.store_file(ctx, "/.system/cluster/node_id", &value, Some("application/octet-stream"))?;
    Ok(())
}

/// Load the persisted node identifier, if any.
pub fn get_node_id(engine: &StorageEngine) -> EngineResult<Option<u64>> {
    let ops = DirectoryOps::new(engine);
    match ops.read_file("/.system/cluster/node_id") {
        Ok(data) if data.len() == 8 => {
            Ok(Some(u64::from_le_bytes(data[..8].try_into().unwrap())))
        }
        Ok(_) => Ok(None), // wrong length — treat as missing
        Err(EngineError::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Persist the full set of peer configurations.
pub fn store_peer_configs(
    engine: &StorageEngine,
    ctx: &RequestContext,
    peers: &[PeerConfig],
) -> EngineResult<()> {
    let ops = DirectoryOps::new(engine);
    let json = serde_json::to_vec(peers)
        .map_err(|error| EngineError::JsonParseError(error.to_string()))?;
    ops.store_file(ctx, "/.system/cluster/peers", &json, Some("application/json"))?;
    Ok(())
}

/// Load persisted peer configurations.
pub fn get_peer_configs(engine: &StorageEngine) -> EngineResult<Vec<PeerConfig>> {
    let ops = DirectoryOps::new(engine);
    match ops.read_file("/.system/cluster/peers") {
        Ok(data) => {
            let peers: Vec<PeerConfig> = serde_json::from_slice(&data)
                .map_err(|error| EngineError::JsonParseError(error.to_string()))?;
            Ok(peers)
        }
        Err(EngineError::NotFound(_)) => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// Deploy (or overwrite) a plugin at the given key.
pub fn store_plugin(
    engine: &StorageEngine,
    ctx: &RequestContext,
    key: &str,
    encoded: &[u8],
) -> EngineResult<()> {
    let ops = DirectoryOps::new(engine);
    let path = format!("/.system/plugins/{}", key);
    ops.store_file(ctx, &path, encoded, Some("application/octet-stream"))?;
    Ok(())
}

/// Retrieve a plugin by key.
pub fn get_plugin(engine: &StorageEngine, key: &str) -> EngineResult<Option<Vec<u8>>> {
    let ops = DirectoryOps::new(engine);
    let path = format!("/.system/plugins/{}", key);
    match ops.read_file(&path) {
        Ok(data) => Ok(Some(data)),
        Err(EngineError::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// List all plugins, returning (key, encoded_bytes) for each.
pub fn list_plugins(engine: &StorageEngine) -> EngineResult<Vec<(String, Vec<u8>)>> {
    let ops = DirectoryOps::new(engine);
    let entries = match ops.list_directory("/.system/plugins") {
        Ok(entries) => entries,
        Err(EngineError::NotFound(_)) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let mut results = Vec::new();
    for entry in &entries {
        let path = format!("/.system/plugins/{}", entry.name);
        if let Ok(data) = ops.read_file(&path) {
            results.push((entry.name.clone(), data));
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
    let path = format!("/.system/plugins/{}", key);
    match ops.delete_file(ctx, &path) {
        Ok(()) => Ok(true),
        Err(EngineError::NotFound(_)) => Ok(false),
        Err(error) => Err(error),
    }
}
