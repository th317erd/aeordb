use argon2::{
  Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
  password_hash::SaltString,
};
use chrono::{DateTime, Utc};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::engine::api_key_rules::KeyRule;

/// Prefix for all aeordb API keys.
const API_KEY_PREFIX: &str = "aeor_k_";

/// Default expiry for API keys: 2 years (in days).
pub const DEFAULT_EXPIRY_DAYS: i64 = 730;
/// Maximum expiry for API keys: 10 years (in days).
pub const MAX_EXPIRY_DAYS: i64 = 3650;
/// Sentinel value for "never expires" share keys. Year 2200 in milliseconds.
pub const NO_EXPIRY_SENTINEL: i64 = 7_258_118_400_000;

/// Metadata record for a stored API key (never contains the plaintext key).
/// The `user_id` field links this key to its owning user. For the root
/// bootstrap key, `user_id` is the nil UUID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyRecord {
  pub key_id: Uuid,
  pub key_hash: String,
  pub user_id: Option<Uuid>,
  pub created_at: DateTime<Utc>,
  pub is_revoked: bool,
  /// Milliseconds since epoch. Mandatory — old records without this field
  /// will fail deserialization intentionally.
  pub expires_at: i64,
  /// Human-friendly label for the key.
  #[serde(default)]
  pub label: Option<String>,
  /// Path-to-permission rules. Empty vec means no path-level restrictions.
  #[serde(default)]
  pub rules: Vec<KeyRule>,
}

/// Generate a new API key with the format `aeor_k_{key_id_prefix}_{random_hex}`
/// where key_id_prefix is the first 16 hex chars of the key_id UUID (no dashes).
pub fn generate_api_key(key_id: Uuid) -> String {
  let mut random_bytes = [0u8; 32];
  rand::RngCore::fill_bytes(&mut OsRng, &mut random_bytes);
  let hex_string = hex::encode(&random_bytes);
  let key_id_prefix = &key_id.simple().to_string()[..16];
  format!("{}{}_{}", API_KEY_PREFIX, key_id_prefix, hex_string)
}

/// Parse an API key, extracting the key_id prefix and the full key string.
/// Returns (key_id_prefix, full_key) on success.
pub fn parse_api_key(key: &str) -> Result<(String, String), String> {
  let without_prefix = key
    .strip_prefix(API_KEY_PREFIX)
    .ok_or_else(|| "API key missing aeor_k_ prefix".to_string())?;

  let underscore_position = without_prefix
    .find('_')
    .ok_or_else(|| "API key missing key_id separator".to_string())?;

  let key_id_prefix = without_prefix[..underscore_position].to_string();
  if key_id_prefix.len() != 16 {
    return Err(format!(
      "key_id prefix must be 16 hex chars, got {}",
      key_id_prefix.len()
    ));
  }

  Ok((key_id_prefix, key.to_string()))
}

/// Hash an API key using argon2id.
pub fn hash_api_key(key: &str) -> Result<String, argon2::password_hash::Error> {
  let salt = SaltString::generate(&mut OsRng);
  let argon2 = Argon2::default();
  let password_hash = argon2.hash_password(key.as_bytes(), &salt)?;
  Ok(password_hash.to_string())
}

/// Verify a plaintext API key against an argon2id hash.
pub fn verify_api_key(key: &str, hash: &str) -> Result<bool, argon2::password_hash::Error> {
  let parsed_hash = PasswordHash::new(hash)?;
  let argon2 = Argon2::default();
  match argon2.verify_password(key.as_bytes(), &parsed_hash) {
    Ok(()) => Ok(true),
    Err(argon2::password_hash::Error::Password) => Ok(false),
    Err(error) => Err(error),
  }
}

/// Validate that a key is the root API key for the given engine.
/// Returns Ok(true) if the key matches the root key in the engine,
/// Ok(false) if the key is valid but not root, or doesn't match any key.
/// Used by CLI tools (export/import) to authorize system data access.
pub fn validate_root_key(
  engine: &crate::engine::StorageEngine,
  key: &str,
) -> Result<bool, String> {
  let (key_id_prefix, full_key) = parse_api_key(key)?;

  let record = crate::engine::system_store::get_api_key_by_prefix(engine, &key_id_prefix)
    .map_err(|e| format!("failed to read api key record: {}", e))?
    .ok_or_else(|| "no api key found matching the provided key".to_string())?;

  // Root key has user_id == None (no user — bootstrap key)
  if record.user_id.is_some() {
    return Ok(false);
  }

  // Verify the hash
  match verify_api_key(&full_key, &record.key_hash) {
    Ok(matches) => Ok(matches),
    Err(e) => Err(format!("hash verification failed: {}", e)),
  }
}
