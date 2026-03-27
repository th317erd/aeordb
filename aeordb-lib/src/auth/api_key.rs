use argon2::{
  Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
  password_hash::SaltString,
};
use chrono::{DateTime, Utc};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Prefix for all aeordb API keys.
const API_KEY_PREFIX: &str = "aeor_k_";

/// Metadata record for a stored API key (never contains the plaintext key).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyRecord {
  pub key_id: Uuid,
  pub key_hash: String,
  pub roles: Vec<String>,
  pub created_at: DateTime<Utc>,
  pub is_revoked: bool,
}

/// Generate a new API key with the format `aeor_k_{key_id_prefix}_{random_hex}`
/// where key_id_prefix is the first 16 hex chars of the key_id UUID (no dashes).
pub fn generate_api_key(key_id: Uuid) -> String {
  let mut random_bytes = [0u8; 32];
  rand::RngCore::fill_bytes(&mut OsRng, &mut random_bytes);
  let hex_string = hex_encode(&random_bytes);
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

/// Simple hex encoder (avoids pulling in the `hex` crate).
fn hex_encode(bytes: &[u8]) -> String {
  let mut output = String::with_capacity(bytes.len() * 2);
  for byte in bytes {
    output.push_str(&format!("{:02x}", byte));
  }
  output
}
