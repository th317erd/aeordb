use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Default magic link expiry in seconds (10 minutes).
pub const DEFAULT_MAGIC_LINK_EXPIRY_SECONDS: i64 = 600;

/// Record stored for each magic link.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MagicLinkRecord {
  pub code_hash: String,
  pub email: String,
  pub created_at: DateTime<Utc>,
  pub expires_at: DateTime<Utc>,
  pub is_used: bool,
}

/// Generate a cryptographically random magic link code (32 bytes, hex encoded).
pub fn generate_magic_link_code() -> String {
  let mut bytes = [0u8; 32];
  rand::rngs::OsRng.fill_bytes(&mut bytes);
  hex::encode(&bytes)
}

/// Hash a magic link code using SHA-256.
///
/// Magic links are short-lived, so SHA-256 is sufficient (no need for argon2).
pub fn hash_magic_link_code(code: &str) -> String {
  let mut hasher = Sha256::new();
  hasher.update(code.as_bytes());
  let result = hasher.finalize();
  hex::encode(&result)
}
