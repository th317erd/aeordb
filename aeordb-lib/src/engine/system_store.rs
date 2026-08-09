//! System store: typed system data operations backed by `DirectoryOps`.
//!
//! All data is stored as regular files under `/.aeordb-system/` in the directory
//! tree, which means system data automatically participates in replication
//! and versioning.
//!
//! This module is the Phase 2 replacement for `system_tables.rs`, which
//! uses loose KV entries with BLAKE3-hashed domain-prefixed keys.

use uuid::Uuid;

use crate::auth::api_key::{ApiKeyRecord, ApiKeyRevokePolicy, ApiKeyRevokeResult};
use crate::auth::magic_link::{MagicLinkConsumeResult, MagicLinkRecord};
use crate::auth::refresh::{RefreshTokenRecord, RefreshTokenRotationResult};
use crate::engine::batch_commit::{BufferedFile, commit_buffered_files_with_kind};
use crate::engine::directory_ops::{BufferedFileTransform, DirectoryOps, FileDeletionRequest, SystemFileAliasMigrationOutcome};
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::group::Group;
use crate::engine::json_store::{JsonDoc, JsonStore, JsonStoreMutation};
use crate::engine::namespace_mutation::NamespaceMutationKind;
use crate::engine::peer_connection::PeerConfig;
use crate::engine::request_context::RequestContext;
use crate::engine::schema_version::JsonVersioned;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::user::{ROOT_USER_ID, User, validate_user_id};

const CREDENTIAL_RECORD_MAX_BYTES: u64 = 1024 * 1024;
const LEGACY_CONFIG_KEY_MAX_BYTES: usize = 255;
const LEGACY_CONFIG_VALUE_MAX_BYTES: u64 = 1024 * 1024;
const JWT_SIGNING_KEY_BYTES: usize = 32;
const PEER_CONFIG_DOCUMENT_MAX_BYTES: u64 = 1024 * 1024;
const NODE_ID_PATH: &str = "/.aeordb-system/cluster/node_id";

fn ensure_credential_record_bounded<T: JsonVersioned>(record: &T, role: &str) -> EngineResult<()> {
  let serialized_bytes = record.serialize_versioned().len() as u64;
  if serialized_bytes > CREDENTIAL_RECORD_MAX_BYTES {
    return Err(EngineError::ResourceExhausted(format!(
      "{role} record is {serialized_bytes} bytes, exceeding the {CREDENTIAL_RECORD_MAX_BYTES}-byte credential limit"
    )));
  }
  Ok(())
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

fn validate_legacy_config_key(key: &str) -> EngineResult<()> {
  if key.is_empty() {
    return Err(EngineError::InvalidInput("config key must not be empty".to_string()));
  }
  if key.len() > LEGACY_CONFIG_KEY_MAX_BYTES {
    return Err(EngineError::ResourceExhausted(format!(
      "config key is {} bytes, exceeding the {LEGACY_CONFIG_KEY_MAX_BYTES}-byte limit",
      key.len(),
    )));
  }
  if key == "." || key == ".." || key.contains('/') || key.contains('\\') || key.contains("::") || key.chars().any(char::is_control) {
    return Err(EngineError::InvalidInput(format!("config key '{key}' contains reserved path characters")));
  }
  Ok(())
}

fn validate_legacy_config_value(value: &[u8]) -> EngineResult<()> {
  if value.len() as u64 > LEGACY_CONFIG_VALUE_MAX_BYTES {
    return Err(EngineError::ResourceExhausted(format!(
      "config value is {} bytes, exceeding the {LEGACY_CONFIG_VALUE_MAX_BYTES}-byte limit",
      value.len(),
    )));
  }
  Ok(())
}

fn legacy_config_path(key: &str) -> EngineResult<String> {
  validate_legacy_config_key(key)?;
  Ok(format!("/.aeordb-system/config/{key}"))
}

/// Store a config value by key.
pub fn store_config(engine: &StorageEngine, ctx: &RequestContext, key: &str, value: &[u8]) -> EngineResult<()> {
  validate_legacy_config_value(value)?;
  if key == "jwt_signing_key" {
    validate_jwt_signing_key(value, false)?;
  }
  let ops = DirectoryOps::new(engine);
  let path = legacy_config_path(key)?;
  ops.store_file_buffered(ctx, &path, value, Some("application/octet-stream"))?;
  Ok(())
}

/// Retrieve a config value by key.
pub fn get_config(engine: &StorageEngine, key: &str) -> EngineResult<Option<Vec<u8>>> {
  let ops = DirectoryOps::new(engine);
  let path = legacy_config_path(key)?;
  match ops.read_file_buffered_bounded(&path, LEGACY_CONFIG_VALUE_MAX_BYTES) {
    Ok(data) => Ok(Some(data)),
    Err(EngineError::NotFound(_)) => Ok(None),
    Err(error) => Err(error),
  }
}

fn validate_jwt_signing_key(key: &[u8], persisted: bool) -> EngineResult<()> {
  if key.len() == JWT_SIGNING_KEY_BYTES {
    return Ok(());
  }
  let reason = format!("JWT signing key must be exactly {JWT_SIGNING_KEY_BYTES} bytes, found {}", key.len());
  if persisted {
    Err(EngineError::CorruptEntry { offset: 0, reason })
  } else {
    Err(EngineError::InvalidInput(reason))
  }
}

/// Read the exact persisted Ed25519 JWT seed, failing closed on malformed
/// authority instead of treating it as absent.
pub fn get_jwt_signing_key(engine: &StorageEngine) -> EngineResult<Option<Vec<u8>>> {
  let Some(key) = get_config(engine, "jwt_signing_key")? else {
    return Ok(None);
  };
  validate_jwt_signing_key(&key, true)?;
  Ok(Some(key))
}

/// Replace the persisted Ed25519 JWT seed after exact validation.
pub fn store_jwt_signing_key(engine: &StorageEngine, ctx: &RequestContext, key: &[u8]) -> EngineResult<()> {
  validate_jwt_signing_key(key, false)?;
  store_config(engine, ctx, "jwt_signing_key", key)
}

/// Atomically install the first JWT seed or return the already-persisted
/// winner. Concurrent first-run providers therefore cannot acknowledge
/// different in-memory signing authority.
pub fn initialize_jwt_signing_key(engine: &StorageEngine, ctx: &RequestContext, candidate: &[u8]) -> EngineResult<Vec<u8>> {
  validate_jwt_signing_key(candidate, false)?;
  let candidate = candidate.to_vec();
  let path = legacy_config_path("jwt_signing_key")?;
  DirectoryOps::new(engine).transform_file_buffered(
    ctx,
    &path,
    Some("application/octet-stream"),
    LEGACY_CONFIG_VALUE_MAX_BYTES,
    NamespaceMutationKind::SystemWrite,
    move |current| {
      if let Some(current) = current {
        validate_jwt_signing_key(current, true)?;
        return Ok(BufferedFileTransform::Keep(current.to_vec()));
      }
      Ok(BufferedFileTransform::Replace { data: candidate.clone(), output: candidate })
    },
  )
}

// ---------------------------------------------------------------------------
// API Keys
// ---------------------------------------------------------------------------

/// Store a non-root user or share-link API key record.
/// Root-owned keys require an explicitly root-authorized caller.
pub fn store_api_key(engine: &StorageEngine, ctx: &RequestContext, record: &ApiKeyRecord) -> EngineResult<()> {
  if let Some(ref uid) = record.user_id {
    validate_user_id(uid)?;
  }
  store_api_key_unchecked(engine, ctx, record)
}

/// Store a root-owned API key after the caller has authenticated root
/// authority. This rejects non-root records so the bypass cannot accidentally
/// weaken ordinary user-ID validation.
pub fn store_api_key_with_root_authority(engine: &StorageEngine, ctx: &RequestContext, record: &ApiKeyRecord) -> EngineResult<()> {
  if record.user_id != Some(ROOT_USER_ID) {
    return Err(EngineError::InvalidInput("root-authorized API-key storage requires the root user ID".to_string()));
  }
  store_api_key_unchecked(engine, ctx, record)
}

/// Compatibility entry point for initial database bootstrap and legacy
/// internal fixture/setup callers. This preserves its historical unchecked
/// behavior; authenticated routes are source-gated onto
/// `store_api_key_with_root_authority` instead.
pub fn store_api_key_for_bootstrap(engine: &StorageEngine, ctx: &RequestContext, record: &ApiKeyRecord) -> EngineResult<()> {
  store_api_key_unchecked(engine, ctx, record)
}

static API_KEY_STORE: JsonStore<ApiKeyRecord> = JsonStore::new("/.aeordb-system/api-keys");

/// Internal: store an API key record without user_id validation.
fn store_api_key_unchecked(engine: &StorageEngine, ctx: &RequestContext, record: &ApiKeyRecord) -> EngineResult<()> {
  ensure_credential_record_bounded(record, "API key")?;
  API_KEY_STORE.put(engine, ctx, &record.key_id.to_string(), record)
}

/// Look up a single API key record by key_id prefix (first 16 hex chars
/// of the UUID, no dashes). Scan-based secondary lookup — no index yet.
pub fn get_api_key_by_prefix(engine: &StorageEngine, key_id_prefix: &str) -> EngineResult<Option<ApiKeyRecord>> {
  for record in API_KEY_STORE.list(engine)? {
    let simple = record.key_id.simple().to_string();
    if &simple[..16] == key_id_prefix {
      return Ok(Some(record));
    }
  }
  Ok(None)
}

/// Get an API key record by its UUID. Returns `Ok(None)` if no such key.
pub fn get_api_key(engine: &StorageEngine, key_id: Uuid) -> EngineResult<Option<ApiKeyRecord>> {
  API_KEY_STORE.get(engine, &key_id.to_string())
}

/// List all API key records.
pub fn list_api_keys(engine: &StorageEngine) -> EngineResult<Vec<ApiKeyRecord>> {
  API_KEY_STORE.list(engine)
}

/// Revoke an API key by setting is_revoked = true.
/// Returns true if the key was found, false otherwise.
pub fn revoke_api_key(engine: &StorageEngine, ctx: &RequestContext, key_id: Uuid) -> EngineResult<bool> {
  Ok(revoke_api_key_with_policy(engine, ctx, key_id, ApiKeyRevokePolicy::Any)?.is_revoked())
}

/// Revoke one key only when the stored record matches the caller's explicit
/// policy. Lookup, policy validation, idempotence, and replacement share one
/// namespace authority window.
pub fn revoke_api_key_with_policy(
  engine: &StorageEngine,
  ctx: &RequestContext,
  key_id: Uuid,
  policy: ApiKeyRevokePolicy,
) -> EngineResult<ApiKeyRevokeResult> {
  API_KEY_STORE.transform(engine, ctx, &key_id.to_string(), CREDENTIAL_RECORD_MAX_BYTES, move |current| {
    let Some(mut record) = current else {
      return Ok(JsonStoreMutation::Keep(ApiKeyRevokeResult::NotFound));
    };
    if !policy.accepts(&record) {
      return Ok(JsonStoreMutation::Keep(ApiKeyRevokeResult::PolicyMismatch));
    }
    if record.is_revoked {
      return Ok(JsonStoreMutation::Keep(ApiKeyRevokeResult::AlreadyRevoked));
    }
    record.is_revoked = true;
    Ok(JsonStoreMutation::Replace { value: record, output: ApiKeyRevokeResult::Revoked })
  })
}

/// Update one API-key label through typed versioned authority.
/// `None` preserves the current label and performs no write.
pub fn update_api_key_label(
  engine: &StorageEngine,
  ctx: &RequestContext,
  key_id: Uuid,
  label: Option<String>,
) -> EngineResult<Option<ApiKeyRecord>> {
  API_KEY_STORE.transform(engine, ctx, &key_id.to_string(), CREDENTIAL_RECORD_MAX_BYTES, move |current| {
    let Some(mut record) = current else {
      return Ok(JsonStoreMutation::Keep(None));
    };
    let Some(label) = label else {
      return Ok(JsonStoreMutation::Keep(Some(record)));
    };
    if record.label.as_ref() == Some(&label) {
      return Ok(JsonStoreMutation::Keep(Some(record)));
    }
    record.label = Some(label);
    Ok(JsonStoreMutation::Replace { value: record.clone(), output: Some(record) })
  })
}

// ---------------------------------------------------------------------------
// Users
// ---------------------------------------------------------------------------

static USER_STORE: JsonStore<User> = JsonStore::new("/.aeordb-system/users");

/// Store a user. Validates user_id != nil UUID.
/// Automatically creates a per-user auto-group `user:{user_id}`.
pub fn store_user(engine: &StorageEngine, ctx: &RequestContext, user: &User) -> EngineResult<()> {
  validate_user_id(&user.user_id)?;
  let group_name = format!("user:{}", user.user_id);
  let auto_group = Group::new(&group_name, "crudlify", "........", "user_id", "eq", &user.user_id.to_string())?;
  commit_buffered_files_with_kind(
    engine,
    ctx,
    vec![
      BufferedFile {
        path: format!("/.aeordb-system/users/{}", user.user_id),
        data: user.serialize_versioned(),
        content_type: Some("application/json".to_string()),
      },
      BufferedFile {
        path: format!("/.aeordb-system/groups/{group_name}"),
        data: auto_group.serialize_versioned(),
        content_type: Some("application/json".to_string()),
      },
    ],
    NamespaceMutationKind::SystemWrite,
  )?;
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
pub fn update_user(engine: &StorageEngine, ctx: &RequestContext, user: &User) -> EngineResult<()> {
  validate_user_id(&user.user_id)?;
  USER_STORE.put(engine, ctx, &user.user_id.to_string(), user)
}

/// Count all users.
pub fn count_users(engine: &StorageEngine) -> EngineResult<u64> {
  let ops = DirectoryOps::new(engine);
  let entries = match ops.list_directory_strict("/.aeordb-system/users") {
    Ok(entries) => entries,
    Err(EngineError::NotFound(_)) => return Ok(0),
    Err(error) => return Err(error),
  };
  Ok(entries.len() as u64)
}

/// Delete a user. Also deletes the per-user auto-group.
/// Returns true if the user existed, false otherwise.
pub fn delete_user(engine: &StorageEngine, ctx: &RequestContext, user_id: &Uuid) -> EngineResult<bool> {
  let ops = DirectoryOps::new(engine);
  let user_path = format!("/.aeordb-system/users/{}", user_id);
  let group_name = format!("user:{}", user_id);
  let group_path = format!("/.aeordb-system/groups/{group_name}");
  let deleted_paths = ops.delete_files_batch_with_kind(
    ctx,
    vec![FileDeletionRequest::primary(user_path.clone()), FileDeletionRequest::optional(group_path)],
    NamespaceMutationKind::SystemWrite,
  )?;
  Ok(deleted_paths.iter().any(|path| path == &user_path))
}

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

static GROUP_STORE: JsonStore<Group> = JsonStore::new("/.aeordb-system/groups");

/// Store a group.
pub fn store_group(engine: &StorageEngine, ctx: &RequestContext, group: &Group) -> EngineResult<()> {
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
pub fn update_group(engine: &StorageEngine, ctx: &RequestContext, group: &Group) -> EngineResult<()> {
  store_group(engine, ctx, group)
}

/// Delete a group. Returns `Ok(true)` if it existed, `Ok(false)` if not.
pub fn delete_group(engine: &StorageEngine, ctx: &RequestContext, name: &str) -> EngineResult<bool> {
  GROUP_STORE.delete(engine, ctx, name)
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

/// Store permissions for a path. The path is BLAKE3-hashed to avoid nested
/// directory issues from arbitrary path strings.
pub fn store_permissions(engine: &StorageEngine, ctx: &RequestContext, path: &str, permissions_json: &[u8]) -> EngineResult<()> {
  let ops = DirectoryOps::new(engine);
  let path_hash = blake3::hash(path.as_bytes());
  let store_path = format!("/.aeordb-system/permissions/{}", path_hash.to_hex());
  ops.store_file_buffered(ctx, &store_path, permissions_json, Some("application/json"))?;
  Ok(())
}

/// Retrieve permissions for a path.
pub fn get_permissions(engine: &StorageEngine, path: &str) -> EngineResult<Option<Vec<u8>>> {
  let ops = DirectoryOps::new(engine);
  let path_hash = blake3::hash(path.as_bytes());
  let store_path = format!("/.aeordb-system/permissions/{}", path_hash.to_hex());
  match ops.read_file_buffered(&store_path) {
    Ok(data) => Ok(Some(data)),
    Err(EngineError::NotFound(_)) => Ok(None),
    Err(error) => Err(error),
  }
}

// ---------------------------------------------------------------------------
// Magic Links
// ---------------------------------------------------------------------------

static MAGIC_LINK_STORE: JsonStore<MagicLinkRecord> = JsonStore::new("/.aeordb-system/magic-links");

/// Store a magic link record.
pub fn store_magic_link(engine: &StorageEngine, ctx: &RequestContext, record: &MagicLinkRecord) -> EngineResult<()> {
  ensure_credential_record_bounded(record, "magic link")?;
  MAGIC_LINK_STORE.put(engine, ctx, &record.code_hash, record)
}

/// Retrieve a magic link record by code_hash.
pub fn get_magic_link(engine: &StorageEngine, code_hash: &str) -> EngineResult<Option<MagicLinkRecord>> {
  MAGIC_LINK_STORE.get(engine, code_hash)
}

/// Mark a magic link as used.
pub fn mark_magic_link_used(engine: &StorageEngine, ctx: &RequestContext, code_hash: &str) -> EngineResult<()> {
  let requested_hash = code_hash.to_string();
  MAGIC_LINK_STORE.transform(engine, ctx, code_hash, CREDENTIAL_RECORD_MAX_BYTES, move |current| {
    let Some(mut record) = current else {
      return Err(EngineError::NotFound(format!("magic link not found: {requested_hash}")));
    };
    if record.is_used {
      return Ok(JsonStoreMutation::Keep(()));
    }
    record.is_used = true;
    Ok(JsonStoreMutation::Replace { value: record, output: () })
  })
}

/// Atomically consume an active magic link. Only the caller that changes the
/// stored record from unused to used receives `Consumed`; concurrent or later
/// callers receive `AlreadyUsed` without another write.
pub fn consume_magic_link(
  engine: &StorageEngine,
  ctx: &RequestContext,
  code_hash: &str,
  now: chrono::DateTime<chrono::Utc>,
) -> EngineResult<MagicLinkConsumeResult> {
  MAGIC_LINK_STORE.transform(engine, ctx, code_hash, CREDENTIAL_RECORD_MAX_BYTES, move |current| {
    let Some(mut record) = current else {
      return Ok(JsonStoreMutation::Keep(MagicLinkConsumeResult::NotFound));
    };
    if record.is_used {
      return Ok(JsonStoreMutation::Keep(MagicLinkConsumeResult::AlreadyUsed));
    }
    if record.expires_at < now {
      return Ok(JsonStoreMutation::Keep(MagicLinkConsumeResult::Expired));
    }
    record.is_used = true;
    Ok(JsonStoreMutation::Replace { value: record.clone(), output: MagicLinkConsumeResult::Consumed(record) })
  })
}

// ---------------------------------------------------------------------------
// Refresh Tokens
// ---------------------------------------------------------------------------

static REFRESH_TOKEN_STORE: JsonStore<RefreshTokenRecord> = JsonStore::new("/.aeordb-system/refresh-tokens");

/// Store a refresh token record.
pub fn store_refresh_token(engine: &StorageEngine, ctx: &RequestContext, record: &RefreshTokenRecord) -> EngineResult<()> {
  ensure_credential_record_bounded(record, "refresh token")?;
  REFRESH_TOKEN_STORE.put(engine, ctx, &record.token_hash, record)
}

/// Retrieve a refresh token record by token_hash.
pub fn get_refresh_token(engine: &StorageEngine, token_hash: &str) -> EngineResult<Option<RefreshTokenRecord>> {
  REFRESH_TOKEN_STORE.get(engine, token_hash)
}

/// Revoke a refresh token by setting is_revoked = true.
/// Returns true if the token was found, false otherwise.
pub fn revoke_refresh_token(engine: &StorageEngine, ctx: &RequestContext, token_hash: &str) -> EngineResult<bool> {
  REFRESH_TOKEN_STORE.transform(engine, ctx, token_hash, CREDENTIAL_RECORD_MAX_BYTES, |current| {
    let Some(mut record) = current else {
      return Ok(JsonStoreMutation::Keep(false));
    };
    if record.is_revoked {
      return Ok(JsonStoreMutation::Keep(true));
    }
    record.is_revoked = true;
    Ok(JsonStoreMutation::Replace { value: record, output: true })
  })
}

/// Atomically claim an active refresh token for rotation. Exactly one caller
/// can change the token from active to revoked and receive its record.
pub fn claim_refresh_token_rotation(
  engine: &StorageEngine,
  ctx: &RequestContext,
  token_hash: &str,
  now: chrono::DateTime<chrono::Utc>,
) -> EngineResult<RefreshTokenRotationResult> {
  REFRESH_TOKEN_STORE.transform(engine, ctx, token_hash, CREDENTIAL_RECORD_MAX_BYTES, move |current| {
    let Some(mut record) = current else {
      return Ok(JsonStoreMutation::Keep(RefreshTokenRotationResult::NotFound));
    };
    if record.is_revoked {
      return Ok(JsonStoreMutation::Keep(RefreshTokenRotationResult::AlreadyRevoked));
    }
    if record.expires_at < now {
      return Ok(JsonStoreMutation::Keep(RefreshTokenRotationResult::Expired));
    }
    record.is_revoked = true;
    Ok(JsonStoreMutation::Replace { value: record.clone(), output: RefreshTokenRotationResult::Claimed(record) })
  })
}

// ---------------------------------------------------------------------------
// Cleanup: Expired Tokens & Used/Expired Magic Links
// ---------------------------------------------------------------------------

/// Clean up expired/revoked refresh tokens and used/expired magic links.
/// Returns `(tokens_cleaned, links_cleaned)`.
///
/// This function is idempotent and safe to run concurrently — each iteration
/// independently scans the directory and deletes qualifying entries.
pub fn cleanup_expired_tokens(engine: &StorageEngine, ctx: &RequestContext) -> EngineResult<(usize, usize)> {
  let ops = DirectoryOps::new(engine);
  let now = chrono::Utc::now();
  let mut tokens_cleaned = 0;
  let mut links_cleaned = 0;

  // Clean up refresh tokens
  let token_entries = match ops.list_directory_strict("/.aeordb-system/refresh-tokens") {
    Ok(entries) => entries,
    Err(EngineError::NotFound(_)) => Vec::new(),
    Err(e) => return Err(e),
  };

  for entry in &token_entries {
    let path = format!("/.aeordb-system/refresh-tokens/{}", entry.name);
    let data = ops.read_file_buffered(&path)?;
    let record = RefreshTokenRecord::deserialize_versioned(&data)?;
    if record.is_revoked || record.expires_at < now {
      ops.delete_file(ctx, &path)?;
      tokens_cleaned += 1;
    }
  }

  // Clean up magic links
  let link_entries = match ops.list_directory_strict("/.aeordb-system/magic-links") {
    Ok(entries) => entries,
    Err(EngineError::NotFound(_)) => Vec::new(),
    Err(e) => return Err(e),
  };

  for entry in &link_entries {
    let path = format!("/.aeordb-system/magic-links/{}", entry.name);
    let data = ops.read_file_buffered(&path)?;
    let record = MagicLinkRecord::deserialize_versioned(&data)?;
    if record.is_used || record.expires_at < now {
      ops.delete_file(ctx, &path)?;
      links_cleaned += 1;
    }
  }

  if tokens_cleaned > 0 || links_cleaned > 0 {
    tracing::info!(
      tokens_cleaned = tokens_cleaned,
      links_cleaned = links_cleaned,
      "Cleaned up expired tokens and used/expired magic links",
    );
  }

  metrics::counter!(crate::metrics::definitions::CLEANUP_TOKENS_TOTAL).increment(tokens_cleaned as u64);
  metrics::counter!(crate::metrics::definitions::CLEANUP_LINKS_TOTAL).increment(links_cleaned as u64);

  Ok((tokens_cleaned, links_cleaned))
}

// ---------------------------------------------------------------------------
// Cluster / Replication
// ---------------------------------------------------------------------------

/// Persist this node's unique identifier.
pub fn store_node_id(engine: &StorageEngine, ctx: &RequestContext, node_id: u64) -> EngineResult<()> {
  let selected = initialize_node_id(engine, ctx, node_id)?;
  if selected != node_id {
    return Err(EngineError::AlreadyExists(format!("cluster node_id is already initialized as {selected}")));
  }
  Ok(())
}

/// Initialize this node's stable identifier exactly once.
///
/// Concurrent callers all receive the first acknowledged nonzero value. An
/// existing malformed value fails closed instead of being treated as absent.
pub fn initialize_node_id(engine: &StorageEngine, ctx: &RequestContext, candidate_node_id: u64) -> EngineResult<u64> {
  if candidate_node_id == 0 {
    return Err(EngineError::InvalidInput("cluster node_id 0 is reserved for uninitialized state".to_string()));
  }
  let ops = DirectoryOps::new(engine);
  ops.transform_file_buffered(ctx, NODE_ID_PATH, Some("application/octet-stream"), 8, NamespaceMutationKind::SystemWrite, move |current| {
    match current {
      Some(bytes) if bytes.len() == 8 => {
        let selected = u64::from_le_bytes(bytes.try_into().expect("length checked"));
        if selected == 0 {
          return Err(EngineError::CorruptEntry { offset: 0, reason: "cluster node_id contains reserved value 0".to_string() });
        }
        Ok(crate::engine::directory_ops::BufferedFileTransform::Keep(selected))
      }
      Some(bytes) => Err(EngineError::CorruptEntry { offset: 0, reason: format!("cluster node_id has {} bytes; expected 8", bytes.len()) }),
      None => Ok(crate::engine::directory_ops::BufferedFileTransform::Replace {
        data: candidate_node_id.to_le_bytes().to_vec(),
        output: candidate_node_id,
      }),
    }
  })
}

/// Load the persisted node identifier, if any.
pub fn get_node_id(engine: &StorageEngine) -> EngineResult<Option<u64>> {
  let ops = DirectoryOps::new(engine);
  match ops.read_file_buffered_bounded(NODE_ID_PATH, 8) {
    Ok(data) if data.len() == 8 => {
      let node_id = u64::from_le_bytes(data[..8].try_into().expect("length checked"));
      if node_id == 0 {
        return Err(EngineError::CorruptEntry { offset: 0, reason: "cluster node_id contains reserved value 0".to_string() });
      }
      Ok(Some(node_id))
    }
    Ok(data) => Err(EngineError::CorruptEntry { offset: 0, reason: format!("cluster node_id has {} bytes; expected 8", data.len()) }),
    Err(EngineError::NotFound(_)) => Ok(None),
    Err(error) => Err(error),
  }
}

/// Wrapper around `Vec<PeerConfig>` so the persisted JSON is an object
/// rather than a bare array — JSON-versioning requires the top level to
/// be an object (so a `$v` field can be injected).
#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
struct PeerConfigList {
  peers: Vec<PeerConfig>,
}

crate::impl_json_versioned_v0!(PeerConfigList);

static PEER_CONFIGS_DOC: JsonDoc<PeerConfigList> = JsonDoc::new("/.aeordb-system/cluster/peers");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerAddressConflictPolicy {
  AllowDuplicate,
  KeepExisting,
  ReplaceExisting,
}

#[derive(Debug, Clone)]
pub struct NewPeerConfig {
  pub address: String,
  pub label: Option<String>,
  pub sync_paths: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct PeerConfigTransition {
  pub peer: PeerConfig,
  pub changed: bool,
  pub retired_node_ids: Vec<u64>,
}

/// Typed authority for the singleton peer-configuration document.
pub struct PeerConfigStore<'a> {
  engine: &'a StorageEngine,
}

impl<'a> PeerConfigStore<'a> {
  pub fn new(engine: &'a StorageEngine) -> Self {
    Self { engine }
  }

  pub fn list(&self) -> EngineResult<Vec<PeerConfig>> {
    let peers = PEER_CONFIGS_DOC.get_or_default_bounded(self.engine, PeerConfigList::default(), PEER_CONFIG_DOCUMENT_MAX_BYTES)?.peers;
    validate_peer_configs(&peers)?;
    Ok(peers)
  }

  pub fn replace_all(&self, ctx: &RequestContext, peers: Vec<PeerConfig>) -> EngineResult<bool> {
    validate_peer_configs(&peers)?;
    PEER_CONFIGS_DOC.transform(self.engine, ctx, PEER_CONFIG_DOCUMENT_MAX_BYTES, move |current| {
      if let Some(current) = &current {
        validate_peer_configs(&current.peers)?;
        if current.peers == peers {
          return Ok(JsonStoreMutation::Keep(false));
        }
      }
      Ok(JsonStoreMutation::Replace { value: PeerConfigList { peers }, output: true })
    })
  }

  pub fn create(
    &self,
    ctx: &RequestContext,
    local_node_id: Option<u64>,
    new_peer: NewPeerConfig,
    address_policy: PeerAddressConflictPolicy,
  ) -> EngineResult<PeerConfigTransition> {
    validate_new_peer_config(&new_peer)?;
    PEER_CONFIGS_DOC.transform(self.engine, ctx, PEER_CONFIG_DOCUMENT_MAX_BYTES, move |current| {
      let mut peers = current.unwrap_or_default().peers;
      validate_peer_configs(&peers)?;
      if address_policy == PeerAddressConflictPolicy::KeepExisting {
        if let Some(existing) = peers.iter().find(|peer| peer.address == new_peer.address).cloned() {
          return Ok(JsonStoreMutation::Keep(PeerConfigTransition { peer: existing, changed: false, retired_node_ids: Vec::new() }));
        }
      }

      let mut retired_node_ids = Vec::new();
      if address_policy == PeerAddressConflictPolicy::ReplaceExisting {
        peers.retain(|peer| {
          let retain = peer.address != new_peer.address;
          if !retain {
            retired_node_ids.push(peer.node_id);
          }
          retain
        });
      }
      let node_id = fresh_peer_node_id(&peers, local_node_id);
      let peer = PeerConfig {
        node_id,
        address: new_peer.address,
        label: new_peer.label,
        sync_paths: new_peer.sync_paths,
        last_clock_offset_ms: None,
        last_wire_time_ms: None,
        last_jitter_ms: None,
        clock_state_at: None,
      };
      peers.push(peer.clone());
      validate_peer_configs(&peers)?;
      Ok(JsonStoreMutation::Replace {
        value: PeerConfigList { peers },
        output: PeerConfigTransition { peer, changed: true, retired_node_ids },
      })
    })
  }

  /// Ensure a set of startup peer addresses exists through one bounded
  /// document transition. Existing addresses are no-write results.
  pub fn ensure_addresses(
    &self,
    ctx: &RequestContext,
    local_node_id: Option<u64>,
    addresses: Vec<String>,
  ) -> EngineResult<Vec<PeerConfigTransition>> {
    let mut unique_addresses = Vec::new();
    let mut seen_addresses = std::collections::HashSet::with_capacity(addresses.len());
    for address in addresses {
      let peer = NewPeerConfig { address, label: None, sync_paths: None };
      validate_new_peer_config(&peer)?;
      if seen_addresses.insert(peer.address.clone()) {
        unique_addresses.push(peer.address);
      }
    }

    PEER_CONFIGS_DOC.transform(self.engine, ctx, PEER_CONFIG_DOCUMENT_MAX_BYTES, move |current| {
      let mut peers = current.unwrap_or_default().peers;
      validate_peer_configs(&peers)?;
      let mut transitions = Vec::with_capacity(unique_addresses.len());
      let mut changed = false;
      for address in unique_addresses {
        if let Some(existing) = peers.iter().find(|peer| peer.address == address).cloned() {
          transitions.push(PeerConfigTransition { peer: existing, changed: false, retired_node_ids: Vec::new() });
          continue;
        }
        let peer = PeerConfig {
          node_id: fresh_peer_node_id(&peers, local_node_id),
          address,
          label: None,
          sync_paths: None,
          last_clock_offset_ms: None,
          last_wire_time_ms: None,
          last_jitter_ms: None,
          clock_state_at: None,
        };
        peers.push(peer.clone());
        transitions.push(PeerConfigTransition { peer, changed: true, retired_node_ids: Vec::new() });
        changed = true;
      }
      validate_peer_configs(&peers)?;
      if !changed {
        return Ok(JsonStoreMutation::Keep(transitions));
      }
      Ok(JsonStoreMutation::Replace { value: PeerConfigList { peers }, output: transitions })
    })
  }

  pub fn upsert(
    &self,
    ctx: &RequestContext,
    peer: PeerConfig,
    address_policy: PeerAddressConflictPolicy,
  ) -> EngineResult<PeerConfigTransition> {
    validate_peer_config(&peer)?;
    PEER_CONFIGS_DOC.transform(self.engine, ctx, PEER_CONFIG_DOCUMENT_MAX_BYTES, move |current| {
      let original_peers = current.unwrap_or_default().peers;
      let peers = original_peers.clone();
      validate_peer_configs(&peers)?;
      if address_policy == PeerAddressConflictPolicy::KeepExisting {
        if let Some(existing) = peers.iter().find(|current| current.address == peer.address).cloned() {
          return Ok(JsonStoreMutation::Keep(PeerConfigTransition { peer: existing, changed: false, retired_node_ids: Vec::new() }));
        }
      }

      let mut retired_node_ids = Vec::new();
      let mut replacement = Vec::with_capacity(peers.len().saturating_add(1));
      let mut inserted = false;
      for current in peers {
        let same_node = current.node_id == peer.node_id;
        let same_address = current.address == peer.address;
        let retire = same_node || (address_policy == PeerAddressConflictPolicy::ReplaceExisting && same_address);
        if retire && current.node_id != peer.node_id {
          retired_node_ids.push(current.node_id);
        }
        if retire {
          if !inserted {
            replacement.push(peer.clone());
            inserted = true;
          }
          continue;
        }
        replacement.push(current);
      }
      if !inserted {
        replacement.push(peer.clone());
      }
      validate_peer_configs(&replacement)?;
      let changed = original_peers != replacement;
      if !changed {
        return Ok(JsonStoreMutation::Keep(PeerConfigTransition { peer, changed: false, retired_node_ids: Vec::new() }));
      }
      Ok(JsonStoreMutation::Replace {
        value: PeerConfigList { peers: replacement },
        output: PeerConfigTransition { peer, changed: true, retired_node_ids },
      })
    })
  }

  pub fn remove(&self, ctx: &RequestContext, node_id: u64) -> EngineResult<Option<PeerConfig>> {
    PEER_CONFIGS_DOC.transform(self.engine, ctx, PEER_CONFIG_DOCUMENT_MAX_BYTES, move |current| {
      let Some(mut current) = current else {
        return Ok(JsonStoreMutation::Keep(None));
      };
      validate_peer_configs(&current.peers)?;
      let Some(index) = current.peers.iter().position(|peer| peer.node_id == node_id) else {
        return Ok(JsonStoreMutation::Keep(None));
      };
      let removed = current.peers.remove(index);
      Ok(JsonStoreMutation::Replace { value: current, output: Some(removed) })
    })
  }
}

fn validate_new_peer_config(peer: &NewPeerConfig) -> EngineResult<()> {
  if peer.address.trim().is_empty() {
    return Err(EngineError::InvalidInput("peer address cannot be empty".to_string()));
  }
  Ok(())
}

fn validate_peer_config(peer: &PeerConfig) -> EngineResult<()> {
  if peer.node_id == 0 {
    return Err(EngineError::InvalidInput("peer node_id 0 is reserved".to_string()));
  }
  validate_new_peer_config(&NewPeerConfig { address: peer.address.clone(), label: peer.label.clone(), sync_paths: peer.sync_paths.clone() })
}

fn validate_peer_configs(peers: &[PeerConfig]) -> EngineResult<()> {
  let mut node_ids = std::collections::HashSet::with_capacity(peers.len());
  for peer in peers {
    validate_peer_config(peer)?;
    if !node_ids.insert(peer.node_id) {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("peer configuration contains duplicate node_id {}", peer.node_id),
      });
    }
  }
  let serialized = PeerConfigList { peers: peers.to_vec() }.serialize_versioned();
  if serialized.len() as u64 > PEER_CONFIG_DOCUMENT_MAX_BYTES {
    return Err(EngineError::ResourceExhausted(format!(
      "peer configuration is {} bytes, exceeding the {PEER_CONFIG_DOCUMENT_MAX_BYTES}-byte limit",
      serialized.len()
    )));
  }
  Ok(())
}

fn fresh_peer_node_id(existing_peers: &[PeerConfig], local_node_id: Option<u64>) -> u64 {
  loop {
    let candidate: u64 = rand::random();
    if candidate != 0 && Some(candidate) != local_node_id && !existing_peers.iter().any(|peer| peer.node_id == candidate) {
      return candidate;
    }
  }
}

/// Persist the full set of peer configurations.
pub fn store_peer_configs(engine: &StorageEngine, ctx: &RequestContext, peers: &[PeerConfig]) -> EngineResult<()> {
  PeerConfigStore::new(engine).replace_all(ctx, peers.to_vec()).map(|_| ())
}

/// Load persisted peer configurations.
pub fn get_peer_configs(engine: &StorageEngine) -> EngineResult<Vec<PeerConfig>> {
  PeerConfigStore::new(engine).list()
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

pub(crate) fn plugin_storage_path(key: &str) -> String {
  format!("/.aeordb-system/plugins/{}", encode_plugin_key(key))
}

pub fn store_plugin(engine: &StorageEngine, ctx: &RequestContext, key: &str, encoded: &[u8]) -> EngineResult<()> {
  let ops = DirectoryOps::new(engine);
  let path = plugin_storage_path(key);
  ops.store_file_buffered(ctx, &path, encoded, Some("application/octet-stream"))?;
  Ok(())
}

/// Retrieve a plugin by key.
pub fn get_plugin(engine: &StorageEngine, key: &str) -> EngineResult<Option<Vec<u8>>> {
  let ops = DirectoryOps::new(engine);
  let path = plugin_storage_path(key);
  match ops.read_file_buffered(&path) {
    Ok(data) => Ok(Some(data)),
    Err(EngineError::NotFound(_)) => Ok(None),
    Err(error) => Err(error),
  }
}

/// List all plugins, returning (key, encoded_bytes) for each.
/// Plugin keys are encoded with '::' replacing '/' for flat storage.
pub fn list_plugins(engine: &StorageEngine) -> EngineResult<Vec<(String, Vec<u8>)>> {
  let mut results = Vec::new();
  let mut offset = 0usize;
  loop {
    let (keys, has_more) = list_plugin_keys_window(engine, offset, 64)?;
    if keys.is_empty() {
      break;
    }
    offset = offset.saturating_add(keys.len());
    for key in keys {
      let data = get_plugin(engine, &key)?.ok_or_else(|| EngineError::NotFound(format!("plugin disappeared during enumeration: {key}")))?;
      results.push((key, data));
    }
    if !has_more {
      break;
    }
  }
  Ok(results)
}

/// Return one bounded window of decoded plugin keys without reading plugin bodies.
pub fn list_plugin_keys_window(engine: &StorageEngine, offset: usize, limit: usize) -> EngineResult<(Vec<String>, bool)> {
  let ops = DirectoryOps::new(engine);
  let window = match ops.list_directory_window_strict("/.aeordb-system/plugins", offset, limit) {
    Ok(window) => window,
    Err(EngineError::NotFound(_)) => return Ok((Vec::new(), false)),
    Err(error) => return Err(error),
  };
  Ok((window.entries.into_iter().map(|entry| decode_plugin_key(&entry.name)).collect(), window.has_more))
}

/// Remove a plugin by key.
/// Returns true if the plugin existed, false otherwise.
pub fn remove_plugin(engine: &StorageEngine, ctx: &RequestContext, key: &str) -> EngineResult<bool> {
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

static PEER_SYNC_STATE_STORE: JsonStore<crate::engine::sync_engine::PeerSyncState> = JsonStore::new("/.aeordb-system/sync-peers");

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
pub fn get_peer_sync_state(engine: &StorageEngine, peer_node_id: u64) -> EngineResult<Option<crate::engine::sync_engine::PeerSyncState>> {
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
/// This function is idempotent: absent legacy paths are skipped and exact
/// duplicate aliases converge to the canonical path. Divergent aliases fail
/// startup without changing either file.
pub fn migrate_system_paths(engine: &StorageEngine) -> EngineResult<()> {
  let ops = DirectoryOps::new(engine);
  let ctx = RequestContext::system();

  migrate_directory(&ops, &ctx, "/.aeordb-system/apikeys", "/.aeordb-system/api-keys")?;
  migrate_directory(&ops, &ctx, "/.aeordb-system/cluster/sync", "/.aeordb-system/sync-peers")?;

  Ok(())
}

/// Move all entries from `old_dir` to `new_dir`, preserving filenames and
/// complete FileRecord metadata.
fn migrate_directory(ops: &DirectoryOps, ctx: &RequestContext, old_dir: &str, new_dir: &str) -> EngineResult<()> {
  let entries = match ops.list_directory_strict(old_dir) {
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
    match ops.migrate_system_file_alias(ctx, &old_path, &new_path)? {
      SystemFileAliasMigrationOutcome::SourceMissing => {
        tracing::debug!(old = %old_path, new = %new_path, "Legacy system entry disappeared before migration authority was acquired");
      }
      SystemFileAliasMigrationOutcome::Moved => {
        tracing::info!(old = %old_path, new = %new_path, "Migrated system entry");
      }
      SystemFileAliasMigrationOutcome::IdenticalAliasRetired => {
        tracing::info!(old = %old_path, new = %new_path, "Retired identical legacy system entry alias");
      }
    }
  }

  Ok(())
}
