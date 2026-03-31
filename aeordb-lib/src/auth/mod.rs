pub mod api_key;
pub mod jwt;
pub mod magic_link;
pub mod middleware;
pub mod rate_limiter;
pub mod refresh;

pub use api_key::{ApiKeyRecord, generate_api_key, hash_api_key, parse_api_key, verify_api_key};
pub use jwt::{JwtManager, TokenClaims};
pub use magic_link::{MagicLinkRecord, generate_magic_link_code, hash_magic_link_code};
pub use middleware::auth_middleware;
pub use rate_limiter::RateLimiter;
pub use refresh::{RefreshTokenRecord, generate_refresh_token, hash_refresh_token};

use crate::engine::{StorageEngine, SystemTables};

/// Bootstrap a root API key if no keys exist yet.
///
/// Returns the plaintext key ONLY on first startup (when no keys exist).
/// Returns None if any API key records are already present.
pub fn bootstrap_root_key(
  engine: &StorageEngine,
) -> Option<String> {
  let system_tables = SystemTables::new(engine);

  let existing_keys = system_tables
    .list_system_api_keys()
    .unwrap_or_default();

  if !existing_keys.is_empty() {
    return None;
  }

  let key_id = uuid::Uuid::new_v4();
  let plaintext_key = generate_api_key(key_id);
  let key_hash = hash_api_key(&plaintext_key)
    .expect("failed to hash root API key");

  let record = ApiKeyRecord {
    key_id,
    key_hash,
    roles: vec!["admin".to_string()],
    created_at: chrono::Utc::now(),
    is_revoked: false,
  };

  system_tables
    .store_api_key(&record)
    .expect("failed to store root API key");

  Some(plaintext_key)
}
