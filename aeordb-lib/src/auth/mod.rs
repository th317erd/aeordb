pub mod api_key;
pub mod auth_uri;
pub mod jwt;
pub mod magic_link;
pub mod middleware;
pub mod permission_middleware;
pub mod provider;
pub mod rate_limiter;
pub mod refresh;

pub use api_key::{ApiKeyRecord, generate_api_key, hash_api_key, parse_api_key, verify_api_key, validate_root_key, DEFAULT_EXPIRY_DAYS, MAX_EXPIRY_DAYS, NO_EXPIRY_SENTINEL};
pub use auth_uri::{AuthMode, parse_auth_uri, resolve_auth_mode, expand_tilde};
pub use jwt::{JwtManager, TokenClaims};
pub use magic_link::{MagicLinkRecord, generate_magic_link_code, hash_magic_link_code};
pub use middleware::auth_middleware;
pub use permission_middleware::{permission_middleware, ActiveKeyRules};
pub use provider::{AuthProvider, AuthProviderError, FileAuthProvider, NoAuthProvider};
pub use rate_limiter::RateLimiter;
pub use refresh::{RefreshTokenRecord, generate_refresh_token, hash_refresh_token};

use crate::engine::{RequestContext, StorageEngine, ROOT_USER_ID};
use crate::engine::system_store;

/// Bootstrap a root API key if no keys exist yet.
///
/// Returns `Ok(Some(plaintext_key))` on success, `Ok(None)` if keys already
/// exist, or `Err` if hashing or storage fails.
pub fn bootstrap_root_key(
  engine: &StorageEngine,
) -> Result<Option<String>, crate::engine::errors::EngineError> {
  let existing_keys = system_store::list_api_keys(engine)
    .unwrap_or_default();

  if !existing_keys.is_empty() {
    return Ok(None);
  }

  let key_id = uuid::Uuid::new_v4();
  let plaintext_key = generate_api_key(key_id);
  let key_hash = hash_api_key(&plaintext_key)
    .map_err(|error| crate::engine::errors::EngineError::IoError(
      std::io::Error::other(format!("failed to hash root API key: {}", error)),
    ))?;

  let record = ApiKeyRecord {
    key_id,
    key_hash,
    user_id: Some(ROOT_USER_ID),
    created_at: chrono::Utc::now(),
    is_revoked: false,
    expires_at: chrono::Utc::now().timestamp_millis()
      + (DEFAULT_EXPIRY_DAYS * 24 * 60 * 60 * 1000),
    label: Some("root-bootstrap".to_string()),
    rules: vec![],
  };

  let ctx = RequestContext::system();
  system_store::store_api_key_for_bootstrap(engine, &ctx, &record)?;

  Ok(Some(plaintext_key))
}
