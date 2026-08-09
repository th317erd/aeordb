use std::sync::Arc;

use crate::auth::api_key::{ApiKeyRecord, ApiKeyRevokePolicy, ApiKeyRevokeResult};
use crate::auth::jwt::JwtManager;
use crate::engine::{RequestContext, StorageEngine, ROOT_USER_ID};
use crate::engine::system_store;

/// Error type for auth provider operations.
#[derive(Debug, thiserror::Error)]
pub enum AuthProviderError {
  #[error("system storage error: {0}")]
  SystemStore(#[from] crate::engine::errors::EngineError),

  #[error("auth disabled")]
  AuthDisabled,

  #[error("{0}")]
  Other(String),
}

pub type Result<T> = std::result::Result<T, AuthProviderError>;

/// Trait for authentication providers.
/// Different implementations handle different auth modes.
pub trait AuthProvider: Send + Sync {
  /// Validate an API key by its key_id prefix.
  /// Returns the matching record if found and not deleted.
  fn get_api_key_by_prefix(&self, key_id_prefix: &str) -> Result<Option<ApiKeyRecord>>;

  /// Get the JWT signing/verification manager.
  fn jwt_manager(&self) -> &JwtManager;

  /// Store a new API key.
  fn store_api_key(&self, record: &ApiKeyRecord) -> Result<()>;

  /// Store a new API key for bootstrap (allows nil UUID).
  fn store_api_key_for_bootstrap(&self, record: &ApiKeyRecord) -> Result<()>;

  /// Store a root-owned API key after the caller has authenticated root
  /// authority. Every provider must implement this explicitly and enforce
  /// that the record is actually root-owned.
  fn store_api_key_with_root_authority(&self, record: &ApiKeyRecord) -> Result<()>;

  /// List all API keys (metadata only).
  fn list_api_keys(&self) -> Result<Vec<ApiKeyRecord>>;

  /// Return the engine that owns authentication authority, when one exists.
  fn authority_engine(&self) -> Option<Arc<StorageEngine>>;

  /// Update one API-key label inside the provider's authority transaction.
  fn update_api_key_label(&self, key_id: uuid::Uuid, label: Option<String>) -> Result<Option<ApiKeyRecord>>;

  /// Revoke an API key only when it matches the caller's explicit policy.
  fn revoke_api_key_with_policy(&self, key_id: uuid::Uuid, policy: ApiKeyRevokePolicy) -> Result<ApiKeyRevokeResult>;

  /// Revoke an API key by key_id without an ownership restriction.
  fn revoke_api_key(&self, key_id: uuid::Uuid) -> Result<bool> {
    Ok(self.revoke_api_key_with_policy(key_id, ApiKeyRevokePolicy::Any)?.is_revoked())
  }

  /// Whether this provider allows auth operations (false for NoAuth).
  fn is_enabled(&self) -> bool {
    true
  }
}

/// No authentication. All requests are allowed as root.
/// Used with --auth=false for development mode.
pub struct NoAuthProvider {
  dummy_jwt_manager: JwtManager,
}

impl NoAuthProvider {
  pub fn new() -> Self {
    Self { dummy_jwt_manager: JwtManager::generate() }
  }
}

impl Default for NoAuthProvider {
  fn default() -> Self {
    Self::new()
  }
}

impl AuthProvider for NoAuthProvider {
  fn get_api_key_by_prefix(&self, _key_id_prefix: &str) -> Result<Option<ApiKeyRecord>> {
    // Return a fake root record for any key lookup.
    Ok(Some(ApiKeyRecord {
      key_id: uuid::Uuid::nil(),
      key_hash: String::new(),
      user_id: Some(ROOT_USER_ID),
      created_at: chrono::Utc::now(),
      is_revoked: false,
      expires_at: i64::MAX,
      label: None,
      rules: vec![],
    }))
  }

  fn jwt_manager(&self) -> &JwtManager {
    &self.dummy_jwt_manager
  }

  fn store_api_key(&self, _record: &ApiKeyRecord) -> Result<()> {
    Ok(())
  }

  fn store_api_key_for_bootstrap(&self, _record: &ApiKeyRecord) -> Result<()> {
    Ok(())
  }

  fn store_api_key_with_root_authority(&self, _record: &ApiKeyRecord) -> Result<()> {
    Ok(())
  }

  fn list_api_keys(&self) -> Result<Vec<ApiKeyRecord>> {
    Ok(Vec::new())
  }

  fn authority_engine(&self) -> Option<Arc<StorageEngine>> {
    None
  }

  fn update_api_key_label(&self, _key_id: uuid::Uuid, _label: Option<String>) -> Result<Option<ApiKeyRecord>> {
    Ok(None)
  }

  fn revoke_api_key_with_policy(&self, _key_id: uuid::Uuid, _policy: ApiKeyRevokePolicy) -> Result<ApiKeyRevokeResult> {
    Ok(ApiKeyRevokeResult::NotFound)
  }

  fn is_enabled(&self) -> bool {
    false
  }
}

/// File-based authentication. Keys and signing key stored in an engine file.
/// Handles --auth=self and --auth=file://path.
///
/// For --auth=self: uses the main database engine (same as current behavior).
/// For --auth=file://path: opens a SEPARATE .aeordb file at that path.
pub struct FileAuthProvider {
  engine: Arc<StorageEngine>,
  jwt_manager: JwtManager,
}

const SIGNING_KEY_CONFIG: &str = "jwt_signing_key";

impl FileAuthProvider {
  /// Create a provider while preserving initialization failures for callers
  /// that own process startup and shutdown policy.
  pub fn try_new(engine: Arc<StorageEngine>) -> std::result::Result<Self, String> {
    let jwt_manager = load_or_create_jwt_manager(&engine)?;
    Ok(Self { engine, jwt_manager })
  }

  /// Create a FileAuthProvider backed by the given engine.
  /// Loads or generates the JWT signing key from the engine's system store.
  /// Panics if the JWT signing key cannot be created or persisted -- this is
  /// a server-startup operation and the process cannot continue without it.
  pub fn new(engine: Arc<StorageEngine>) -> Self {
    Self::try_new(engine).expect("fatal: unable to load or create JWT signing key during initialization")
  }

  /// Create a FileAuthProvider for a separate identity file.
  /// If the file doesn't exist, it will be created and bootstrapped.
  pub fn from_identity_file(path: &str) -> std::result::Result<(Self, Option<String>), String> {
    let file_path = std::path::Path::new(path);

    // Create parent directory if needed.
    if let Some(parent) = file_path.parent() {
      if !parent.exists() {
        std::fs::create_dir_all(parent).map_err(|error| format!("Failed to create identity directory: {}", error))?;
      }
    }

    let engine = if file_path.exists() {
      StorageEngine::open(path).map_err(|error| format!("Failed to open identity file: {}", error))?
    } else {
      StorageEngine::create(path).map_err(|error| format!("Failed to create identity file: {}", error))?
    };

    let engine = Arc::new(engine);
    let provider = Self::try_new(engine.clone()).map_err(|error| format!("Failed to initialize identity provider: {error}"))?;

    // Bootstrap a root key if none exist.
    let bootstrap_key = crate::auth::bootstrap_root_key(&engine).map_err(|error| format!("Failed to bootstrap root key: {}", error))?;

    Ok((provider, bootstrap_key))
  }
}

impl AuthProvider for FileAuthProvider {
  fn get_api_key_by_prefix(&self, key_id_prefix: &str) -> Result<Option<ApiKeyRecord>> {
    Ok(system_store::get_api_key_by_prefix(&self.engine, key_id_prefix)?)
  }

  fn jwt_manager(&self) -> &JwtManager {
    &self.jwt_manager
  }

  fn store_api_key(&self, record: &ApiKeyRecord) -> Result<()> {
    let ctx = RequestContext::system();
    Ok(system_store::store_api_key(&self.engine, &ctx, record)?)
  }

  fn store_api_key_for_bootstrap(&self, record: &ApiKeyRecord) -> Result<()> {
    let ctx = RequestContext::system();
    Ok(system_store::store_api_key_for_bootstrap(&self.engine, &ctx, record)?)
  }

  fn store_api_key_with_root_authority(&self, record: &ApiKeyRecord) -> Result<()> {
    let ctx = RequestContext::system();
    Ok(system_store::store_api_key_with_root_authority(&self.engine, &ctx, record)?)
  }

  fn list_api_keys(&self) -> Result<Vec<ApiKeyRecord>> {
    Ok(system_store::list_api_keys(&self.engine)?)
  }

  fn authority_engine(&self) -> Option<Arc<StorageEngine>> {
    Some(self.engine.clone())
  }

  fn update_api_key_label(&self, key_id: uuid::Uuid, label: Option<String>) -> Result<Option<ApiKeyRecord>> {
    let ctx = RequestContext::system();
    Ok(system_store::update_api_key_label(&self.engine, &ctx, key_id, label)?)
  }

  fn revoke_api_key_with_policy(&self, key_id: uuid::Uuid, policy: ApiKeyRevokePolicy) -> Result<ApiKeyRevokeResult> {
    let ctx = RequestContext::system();
    Ok(system_store::revoke_api_key_with_policy(&self.engine, &ctx, key_id, policy)?)
  }
}

/// Load an existing signing key from config, or generate a new one and persist it.
///
/// Failure semantics — important for cluster correctness:
///   * If the key file IS NOT PRESENT (`Ok(None)`): generate a new one and
///     persist it. This is the normal first-run bootstrap.
///   * If the key file IS PRESENT but corrupt (e.g. wrong byte length): RETURN
///     AN ERROR so the operator can investigate. The previous behavior here
///     silently fell through to "generate new" — which invalidated every
///     outstanding JWT and refresh token on the cluster and could mask a
///     deeper corruption issue. Refusing to start surfaces the problem.
fn load_or_create_jwt_manager(engine: &StorageEngine) -> std::result::Result<JwtManager, String> {
  match system_store::get_config(engine, SIGNING_KEY_CONFIG) {
    Ok(Some(key_bytes)) => {
      // Key bytes ARE present. They MUST parse. Don't fall through.
      return JwtManager::from_bytes(&key_bytes).map_err(|e| {
        format!(
          "JWT signing key is present at /.aeordb-system/config/{} but failed to parse: {:?}. \
         This indicates corruption. Refusing to regenerate the key automatically — \
         doing so would invalidate every outstanding JWT and refresh token in the cluster. \
         If you intend to reset the signing key, run emergency-reset (which also wipes \
         /.aeordb-system/refresh-tokens) or remove the corrupt config entry manually \
         before restart.",
          SIGNING_KEY_CONFIG, e
        )
      });
    }
    Ok(None) => {
      // First-run bootstrap path — fall through to generate-and-persist.
    }
    Err(e) => {
      return Err(format!("Failed to read JWT signing key config: {}", e));
    }
  }

  let manager = JwtManager::generate();
  let key_bytes = manager.to_bytes();
  let ctx = RequestContext::system();
  system_store::store_config(engine, &ctx, SIGNING_KEY_CONFIG, &key_bytes)
    .map_err(|error| format!("failed to persist JWT signing key: {}", error))?;
  Ok(manager)
}
