use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::permissions::PathPermissions;
use crate::engine::storage_engine::StorageEngine;

/// Cached .permissions file for a path.
struct PermCacheEntry {
  /// None means no .permissions file exists at this path.
  permissions: Option<PathPermissions>,
  fetched_at: Instant,
}

/// In-memory cache for `.permissions` files at directory paths.
pub struct PermissionsCache {
  entries: RwLock<HashMap<String, PermCacheEntry>>,
  ttl: Duration,
}

impl PermissionsCache {
  /// Create a new permissions cache with the given TTL.
  pub fn new(ttl: Duration) -> Self {
    PermissionsCache {
      entries: RwLock::new(HashMap::new()),
      ttl,
    }
  }

  /// Get the permissions for a directory path.
  /// Returns cached result if available and not expired; otherwise loads from engine.
  /// Returns `None` if no `.permissions` file exists at this path.
  pub fn get_permissions(
    &self,
    path: &str,
    engine: &StorageEngine,
  ) -> EngineResult<Option<PathPermissions>> {
    // Check cache first (read lock).
    {
      let entries = self.entries.read().map_err(|error| {
        EngineError::IoError(
          std::io::Error::other(format!("PermissionsCache read lock poisoned: {}", error)),
        )
      })?;

      if let Some(entry) = entries.get(path) {
        if entry.fetched_at.elapsed() < self.ttl {
          return Ok(entry.permissions.clone());
        }
      }
    }

    // Cache miss or expired -- load from engine.
    let permissions = self.load_permissions(path, engine)?;

    // Store in cache (write lock).
    {
      let mut entries = self.entries.write().map_err(|error| {
        EngineError::IoError(
          std::io::Error::other(format!("PermissionsCache write lock poisoned: {}", error)),
        )
      })?;

      entries.insert(path.to_string(), PermCacheEntry {
        permissions: permissions.clone(),
        fetched_at: Instant::now(),
      });
    }

    Ok(permissions)
  }

  /// Evict a specific path from the cache. Call when .permissions is written at that path.
  pub fn evict_path(&self, path: &str) {
    if let Ok(mut entries) = self.entries.write() {
      entries.remove(path);
    }
  }

  /// Flush the entire cache.
  pub fn evict_all(&self) {
    if let Ok(mut entries) = self.entries.write() {
      entries.clear();
    }
  }

  /// Load a `.permissions` file from the engine via DirectoryOps.
  fn load_permissions(
    &self,
    path: &str,
    engine: &StorageEngine,
  ) -> EngineResult<Option<PathPermissions>> {
    let directory_ops = DirectoryOps::new(engine);

    let permissions_path = if path == "/" || path.ends_with('/') {
      format!("{}.aeordb-permissions", path)
    } else {
      format!("{}/.aeordb-permissions", path)
    };

    match directory_ops.read_file(&permissions_path) {
      Ok(data) => {
        let permissions = PathPermissions::deserialize(&data)?;
        Ok(Some(permissions))
      }
      Err(EngineError::NotFound(_)) => Ok(None),
      Err(error) => Err(error),
    }
  }
}
