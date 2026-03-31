use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::auth::api_key::ApiKeyRecord;
use crate::auth::magic_link::MagicLinkRecord;
use crate::auth::refresh::RefreshTokenRecord;
use crate::engine::entry_type::EntryType;
use crate::engine::storage_engine::StorageEngine;

/// Error type for system table operations.
#[derive(Debug, thiserror::Error)]
pub enum SystemTableError {
  #[error("engine error: {0}")]
  Engine(#[from] crate::engine::errors::EngineError),

  #[error("serialization error: {0}")]
  Serialization(String),

  #[error("record not found")]
  NotFound,

  #[error("corrupt data")]
  CorruptData,
}

pub type Result<T> = std::result::Result<T, SystemTableError>;

/// Provides system table operations (config, API keys, magic links, refresh
/// tokens, plugins) backed by the custom StorageEngine instead of redb.
///
/// Keys are domain-prefixed and hashed with BLAKE3 so they land in the
/// engine's KV store alongside file/directory entries but in a separate
/// namespace.
pub struct SystemTables<'a> {
  engine: &'a StorageEngine,
}

// Domain prefixes for system table keys.
const PREFIX_CONFIG: &str = "::aeordb:config:";
const PREFIX_API_KEY: &str = "::aeordb:apikey:";
const PREFIX_API_KEY_REGISTRY: &str = "::aeordb:apikey:_registry";
const PREFIX_MAGIC_LINK: &str = "::aeordb:magiclink:";
const PREFIX_REFRESH_TOKEN: &str = "::aeordb:refresh:";
const PREFIX_PLUGIN: &str = "::aeordb:plugin:";
const PREFIX_PLUGIN_REGISTRY: &str = "::aeordb:plugin:_registry";

impl<'a> SystemTables<'a> {
  pub fn new(engine: &'a StorageEngine) -> Self {
    SystemTables { engine }
  }

  /// Compute a deterministic hash for a system-table key string.
  fn hash_key(&self, key_string: &str) -> Vec<u8> {
    blake3::hash(key_string.as_bytes()).as_bytes().to_vec()
  }

  // -------------------------------------------------------------------------
  // Config
  // -------------------------------------------------------------------------

  /// Store a config value by key.
  pub fn store_config(&self, key: &str, value: &[u8]) -> Result<()> {
    let hash = self.hash_key(&format!("{PREFIX_CONFIG}{key}"));
    self.engine.store_entry(EntryType::FileRecord, &hash, value)?;
    Ok(())
  }

  /// Retrieve a config value by key.
  pub fn get_config(&self, key: &str) -> Result<Option<Vec<u8>>> {
    let hash = self.hash_key(&format!("{PREFIX_CONFIG}{key}"));
    match self.engine.get_entry(&hash)? {
      Some((_header, _key, value)) => Ok(Some(value)),
      None => Ok(None),
    }
  }

  // -------------------------------------------------------------------------
  // API Keys
  // -------------------------------------------------------------------------

  /// Store an API key record.
  pub fn store_api_key(&self, record: &ApiKeyRecord) -> Result<()> {
    let key_id_string = record.key_id.to_string();
    let hash = self.hash_key(&format!("{PREFIX_API_KEY}{key_id_string}"));
    let encoded = serde_json::to_vec(record)
      .map_err(|error| SystemTableError::Serialization(error.to_string()))?;
    self.engine.store_entry(EntryType::FileRecord, &hash, &encoded)?;

    // Update the registry.
    let mut registry = self.load_api_key_registry()?;
    if !registry.contains(&key_id_string) {
      registry.push(key_id_string);
      self.save_api_key_registry(&registry)?;
    }

    Ok(())
  }

  /// Look up a single API key record by key_id prefix (first 16 hex chars
  /// of the UUID, no dashes).
  pub fn get_system_api_key(&self, key_id_prefix: &str) -> Result<Option<ApiKeyRecord>> {
    let registry = self.load_api_key_registry()?;
    for key_id_string in &registry {
      let simple = Uuid::parse_str(key_id_string)
        .map_err(|_| SystemTableError::CorruptData)?
        .simple()
        .to_string();
      let record_prefix = &simple[..16];
      if record_prefix == key_id_prefix {
        let hash = self.hash_key(&format!("{PREFIX_API_KEY}{key_id_string}"));
        if let Some((_header, _key, value)) = self.engine.get_entry(&hash)? {
          if self.engine.is_entry_deleted(&hash)? {
            continue;
          }
          let record: ApiKeyRecord = serde_json::from_slice(&value)
            .map_err(|_| SystemTableError::CorruptData)?;
          return Ok(Some(record));
        }
      }
    }
    Ok(None)
  }

  /// List all API key records.
  pub fn list_system_api_keys(&self) -> Result<Vec<ApiKeyRecord>> {
    let registry = self.load_api_key_registry()?;
    let mut records = Vec::new();
    for key_id_string in &registry {
      let hash = self.hash_key(&format!("{PREFIX_API_KEY}{key_id_string}"));
      if self.engine.is_entry_deleted(&hash)? {
        continue;
      }
      if let Some((_header, _key, value)) = self.engine.get_entry(&hash)? {
        let record: ApiKeyRecord = serde_json::from_slice(&value)
          .map_err(|_| SystemTableError::CorruptData)?;
        records.push(record);
      }
    }
    Ok(records)
  }

  /// Revoke an API key by setting is_revoked = true.
  /// Returns true if the key was found, false otherwise.
  pub fn revoke_api_key(&self, key_id: Uuid) -> Result<bool> {
    let key_id_string = key_id.to_string();
    let hash = self.hash_key(&format!("{PREFIX_API_KEY}{key_id_string}"));
    let entry = match self.engine.get_entry(&hash)? {
      Some((_header, _key, value)) => value,
      None => return Ok(false),
    };

    let mut record: ApiKeyRecord = serde_json::from_slice(&entry)
      .map_err(|_| SystemTableError::CorruptData)?;
    record.is_revoked = true;

    let encoded = serde_json::to_vec(&record)
      .map_err(|error| SystemTableError::Serialization(error.to_string()))?;
    self.engine.store_entry(EntryType::FileRecord, &hash, &encoded)?;
    Ok(true)
  }

  fn load_api_key_registry(&self) -> Result<Vec<String>> {
    let hash = self.hash_key(PREFIX_API_KEY_REGISTRY);
    match self.engine.get_entry(&hash)? {
      Some((_header, _key, value)) => {
        let registry: Vec<String> = serde_json::from_slice(&value)
          .map_err(|_| SystemTableError::CorruptData)?;
        Ok(registry)
      }
      None => Ok(Vec::new()),
    }
  }

  fn save_api_key_registry(&self, registry: &[String]) -> Result<()> {
    let hash = self.hash_key(PREFIX_API_KEY_REGISTRY);
    let encoded = serde_json::to_vec(registry)
      .map_err(|error| SystemTableError::Serialization(error.to_string()))?;
    self.engine.store_entry(EntryType::FileRecord, &hash, &encoded)?;
    Ok(())
  }

  // -------------------------------------------------------------------------
  // Magic Links
  // -------------------------------------------------------------------------

  /// Store a magic link record, keyed by code_hash.
  pub fn store_magic_link(
    &self,
    code_hash: &str,
    email: &str,
    expires_at: DateTime<Utc>,
  ) -> Result<()> {
    let record = MagicLinkRecord {
      code_hash: code_hash.to_string(),
      email: email.to_string(),
      created_at: Utc::now(),
      expires_at,
      is_used: false,
    };

    let hash = self.hash_key(&format!("{PREFIX_MAGIC_LINK}{code_hash}"));
    let encoded = serde_json::to_vec(&record)
      .map_err(|error| SystemTableError::Serialization(error.to_string()))?;
    self.engine.store_entry(EntryType::FileRecord, &hash, &encoded)?;
    Ok(())
  }

  /// Retrieve a magic link record by code_hash.
  pub fn get_magic_link(&self, code_hash: &str) -> Result<Option<MagicLinkRecord>> {
    let hash = self.hash_key(&format!("{PREFIX_MAGIC_LINK}{code_hash}"));
    match self.engine.get_entry(&hash)? {
      Some((_header, _key, value)) => {
        let record: MagicLinkRecord = serde_json::from_slice(&value)
          .map_err(|_| SystemTableError::CorruptData)?;
        Ok(Some(record))
      }
      None => Ok(None),
    }
  }

  /// Mark a magic link as used.
  pub fn mark_magic_link_used(&self, code_hash: &str) -> Result<()> {
    let hash = self.hash_key(&format!("{PREFIX_MAGIC_LINK}{code_hash}"));
    let entry = match self.engine.get_entry(&hash)? {
      Some((_header, _key, value)) => value,
      None => return Err(SystemTableError::NotFound),
    };

    let mut record: MagicLinkRecord = serde_json::from_slice(&entry)
      .map_err(|_| SystemTableError::CorruptData)?;
    record.is_used = true;

    let encoded = serde_json::to_vec(&record)
      .map_err(|error| SystemTableError::Serialization(error.to_string()))?;
    self.engine.store_entry(EntryType::FileRecord, &hash, &encoded)?;
    Ok(())
  }

  // -------------------------------------------------------------------------
  // Refresh Tokens
  // -------------------------------------------------------------------------

  /// Store a refresh token record, keyed by token_hash.
  pub fn store_refresh_token(
    &self,
    token_hash: &str,
    user_subject: &str,
    expires_at: DateTime<Utc>,
  ) -> Result<()> {
    let record = RefreshTokenRecord {
      token_hash: token_hash.to_string(),
      user_subject: user_subject.to_string(),
      created_at: Utc::now(),
      expires_at,
      is_revoked: false,
    };

    let hash = self.hash_key(&format!("{PREFIX_REFRESH_TOKEN}{token_hash}"));
    let encoded = serde_json::to_vec(&record)
      .map_err(|error| SystemTableError::Serialization(error.to_string()))?;
    self.engine.store_entry(EntryType::FileRecord, &hash, &encoded)?;
    Ok(())
  }

  /// Retrieve a refresh token record by token_hash.
  pub fn get_refresh_token(&self, token_hash: &str) -> Result<Option<RefreshTokenRecord>> {
    let hash = self.hash_key(&format!("{PREFIX_REFRESH_TOKEN}{token_hash}"));
    match self.engine.get_entry(&hash)? {
      Some((_header, _key, value)) => {
        let record: RefreshTokenRecord = serde_json::from_slice(&value)
          .map_err(|_| SystemTableError::CorruptData)?;
        Ok(Some(record))
      }
      None => Ok(None),
    }
  }

  /// Revoke a refresh token by setting is_revoked = true.
  pub fn revoke_refresh_token(&self, token_hash: &str) -> Result<()> {
    let hash = self.hash_key(&format!("{PREFIX_REFRESH_TOKEN}{token_hash}"));
    let entry = match self.engine.get_entry(&hash)? {
      Some((_header, _key, value)) => value,
      None => return Err(SystemTableError::NotFound),
    };

    let mut record: RefreshTokenRecord = serde_json::from_slice(&entry)
      .map_err(|_| SystemTableError::CorruptData)?;
    record.is_revoked = true;

    let encoded = serde_json::to_vec(&record)
      .map_err(|error| SystemTableError::Serialization(error.to_string()))?;
    self.engine.store_entry(EntryType::FileRecord, &hash, &encoded)?;
    Ok(())
  }

  // -------------------------------------------------------------------------
  // Plugins
  // -------------------------------------------------------------------------

  /// Deploy (or overwrite) a plugin at the given path.
  pub fn store_plugin(&self, path: &str, encoded: &[u8]) -> Result<()> {
    let hash = self.hash_key(&format!("{PREFIX_PLUGIN}{path}"));
    self.engine.store_entry(EntryType::FileRecord, &hash, encoded)?;

    // Update registry.
    let mut registry = self.load_plugin_registry()?;
    if !registry.contains(&path.to_string()) {
      registry.push(path.to_string());
      self.save_plugin_registry(&registry)?;
    }

    Ok(())
  }

  /// Retrieve a plugin record by path.
  pub fn get_plugin(&self, path: &str) -> Result<Option<Vec<u8>>> {
    let hash = self.hash_key(&format!("{PREFIX_PLUGIN}{path}"));
    match self.engine.get_entry(&hash)? {
      Some((_header, _key, value)) => {
        if self.engine.is_entry_deleted(&hash)? {
          return Ok(None);
        }
        Ok(Some(value))
      }
      None => Ok(None),
    }
  }

  /// List all plugin paths from the registry, returning (path, encoded_bytes)
  /// for each non-deleted plugin.
  pub fn list_plugins(&self) -> Result<Vec<(String, Vec<u8>)>> {
    let registry = self.load_plugin_registry()?;
    let mut results = Vec::new();
    for path in &registry {
      let hash = self.hash_key(&format!("{PREFIX_PLUGIN}{path}"));
      if self.engine.is_entry_deleted(&hash)? {
        continue;
      }
      if let Some((_header, _key, value)) = self.engine.get_entry(&hash)? {
        results.push((path.clone(), value));
      }
    }
    Ok(results)
  }

  /// Remove a plugin by path.
  /// Returns true if the plugin existed and was removed, false if not found.
  pub fn remove_plugin(&self, path: &str) -> Result<bool> {
    let hash = self.hash_key(&format!("{PREFIX_PLUGIN}{path}"));
    if !self.engine.has_entry(&hash)? {
      return Ok(false);
    }
    self.engine.mark_entry_deleted(&hash)?;

    // Update registry.
    let mut registry = self.load_plugin_registry()?;
    registry.retain(|registered_path| registered_path != path);
    self.save_plugin_registry(&registry)?;

    Ok(true)
  }

  fn load_plugin_registry(&self) -> Result<Vec<String>> {
    let hash = self.hash_key(PREFIX_PLUGIN_REGISTRY);
    match self.engine.get_entry(&hash)? {
      Some((_header, _key, value)) => {
        let registry: Vec<String> = serde_json::from_slice(&value)
          .map_err(|_| SystemTableError::CorruptData)?;
        Ok(registry)
      }
      None => Ok(Vec::new()),
    }
  }

  fn save_plugin_registry(&self, registry: &[String]) -> Result<()> {
    let hash = self.hash_key(PREFIX_PLUGIN_REGISTRY);
    let encoded = serde_json::to_vec(registry)
      .map_err(|error| SystemTableError::Serialization(error.to_string()))?;
    self.engine.store_entry(EntryType::FileRecord, &hash, &encoded)?;
    Ok(())
  }
}
