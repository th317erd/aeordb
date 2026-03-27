pub mod api_key;
pub mod jwt;
pub mod middleware;

pub use api_key::{ApiKeyRecord, generate_api_key, hash_api_key, verify_api_key};
pub use jwt::{JwtManager, TokenClaims};
pub use middleware::auth_middleware;

use crate::storage::RedbStorage;

/// Bootstrap a root API key if no keys exist yet.
///
/// Returns the plaintext key ONLY on first startup (when no keys exist).
/// Returns None if any API key records are already present.
pub fn bootstrap_root_key(
  storage: &RedbStorage,
  _jwt_manager: &JwtManager,
) -> Option<String> {
  let existing_keys = storage
    .list_system_api_keys()
    .unwrap_or_default();

  if !existing_keys.is_empty() {
    return None;
  }

  let plaintext_key = generate_api_key();
  let key_hash = hash_api_key(&plaintext_key)
    .expect("failed to hash root API key");

  let record = ApiKeyRecord {
    key_id: uuid::Uuid::new_v4(),
    key_hash,
    roles: vec!["admin".to_string()],
    created_at: chrono::Utc::now(),
    is_revoked: false,
  };

  storage
    .store_api_key(&record)
    .expect("failed to store root API key");

  Some(plaintext_key)
}
