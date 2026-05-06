use std::collections::HashMap;
use std::hash::Hash;
use std::sync::RwLock;

use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::storage_engine::StorageEngine;

/// Trait for loading a cache entry on miss. Implementors define the key/value
/// types and how to fetch a value from the engine when it's not cached.
pub trait CacheLoader: Send + Sync {
    type Key: Eq + Hash + Clone + Send + Sync;
    type Value: Clone + Send + Sync;

    fn load(&self, key: &Self::Key, engine: &StorageEngine) -> EngineResult<Self::Value>;
}

/// Generic eviction-based cache. No TTL — entries live until explicitly evicted.
/// Uses RwLock for concurrent reads on the hot path.
pub struct Cache<L: CacheLoader> {
    entries: RwLock<HashMap<L::Key, L::Value>>,
    loader: L,
}

impl<L: CacheLoader> Cache<L> {
    /// Create a new cache with the given loader.
    pub fn new(loader: L) -> Self {
        Cache {
            entries: RwLock::new(HashMap::new()),
            loader,
        }
    }

    /// Get a value by key. Returns the cached value if present, otherwise
    /// calls the loader, caches the result, and returns it.
    /// Errors from the loader are propagated (not cached).
    pub fn get(&self, key: &L::Key, engine: &StorageEngine) -> EngineResult<L::Value> {
        // Fast path: read lock
        {
            let entries = self.entries.read().map_err(|e| {
                EngineError::IoError(std::io::Error::other(format!("Cache read lock poisoned: {}", e)))
            })?;
            if let Some(value) = entries.get(key) {
                return Ok(value.clone());
            }
        }

        // Cache miss: load from source
        let value = self.loader.load(key, engine)?;

        // Store in cache (write lock)
        {
            let mut entries = self.entries.write().map_err(|e| {
                EngineError::IoError(std::io::Error::other(format!("Cache write lock poisoned: {}", e)))
            })?;
            entries.insert(key.clone(), value.clone());
        }

        Ok(value)
    }

    /// Evict a single entry by key.
    pub fn evict(&self, key: &L::Key) {
        if let Ok(mut entries) = self.entries.write() {
            entries.remove(key);
        }
    }

    /// Flush the entire cache.
    pub fn evict_all(&self) {
        if let Ok(mut entries) = self.entries.write() {
            entries.clear();
        }
    }
}
