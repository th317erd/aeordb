use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Prefix for all aeordb refresh tokens.
const REFRESH_TOKEN_PREFIX: &str = "aeor_r_";

/// Default refresh token expiry in seconds (30 days).
pub const DEFAULT_REFRESH_EXPIRY_SECONDS: i64 = 30 * 24 * 3600;

/// Record stored for each refresh token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshTokenRecord {
  pub token_hash: String,
  pub user_subject: String,
  pub created_at: DateTime<Utc>,
  pub expires_at: DateTime<Utc>,
  pub is_revoked: bool,
  /// The API key that issued this refresh token, when known. Refresh
  /// requests verify the key is still active and unrevoked — otherwise
  /// a revoked key's outstanding refresh tokens would still mint fresh
  /// JWTs. Older refresh records (pre-2026-05) have `None`; for those we
  /// fall back to "trust unless explicitly revoked," matching legacy
  /// behavior so existing sessions don't break on upgrade.
  #[serde(default)]
  pub key_id: Option<String>,
}

/// Generate a cryptographically random refresh token with the `aeor_r_` prefix.
pub fn generate_refresh_token() -> String {
  let mut bytes = [0u8; 32];
  rand::rngs::OsRng.fill_bytes(&mut bytes);
  format!("{}{}", REFRESH_TOKEN_PREFIX, hex::encode(&bytes))
}

/// Hash a refresh token using SHA-256.
pub fn hash_refresh_token(token: &str) -> String {
  let mut hasher = Sha256::new();
  hasher.update(token.as_bytes());
  let result = hasher.finalize();
  hex::encode(&result)
}
