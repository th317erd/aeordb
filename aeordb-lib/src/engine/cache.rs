use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, RwLock};

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
/// Uses RwLock for concurrent reads on the hot path. A per-key in-flight map
/// provides singleflight semantics so a burst of cold misses for the same key
/// invokes the loader once, not N times.
pub struct Cache<L: CacheLoader> {
    entries: RwLock<HashMap<L::Key, L::Value>>,
    in_flight: Mutex<HashMap<L::Key, Arc<Mutex<()>>>>,
    loader: L,
}

impl<L: CacheLoader> Cache<L> {
    /// Create a new cache with the given loader.
    pub fn new(loader: L) -> Self {
        Cache {
            entries: RwLock::new(HashMap::new()),
            in_flight: Mutex::new(HashMap::new()),
            loader,
        }
    }

    /// Get a value by key. Returns the cached value if present, otherwise
    /// calls the loader, caches the result, and returns it. Concurrent cold
    /// misses for the same key wait on a single in-flight load.
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

        // Cache miss: arrange singleflight. Either we become the loader for this
        // key (insert a fresh Mutex, hold its guard) or we wait on an existing
        // loader (clone the Arc, lock it after dropping in_flight, then re-check
        // the cache once the original loader has finished).
        let lock = {
            let mut in_flight = self.in_flight.lock().map_err(|e| {
                EngineError::IoError(std::io::Error::other(format!("Cache in_flight poisoned: {}", e)))
            })?;
            if let Some(existing) = in_flight.get(key) {
                let lock = existing.clone();
                drop(in_flight);
                let _wait = lock.lock().map_err(|e| {
                    EngineError::IoError(std::io::Error::other(format!("Cache loader mutex poisoned: {}", e)))
                })?;
                // Loader finished — re-check the cache.
                let entries = self.entries.read().map_err(|e| {
                    EngineError::IoError(std::io::Error::other(format!("Cache read lock poisoned: {}", e)))
                })?;
                if let Some(value) = entries.get(key) {
                    return Ok(value.clone());
                }
                // The original loader errored; fall through and try again ourselves.
                drop(entries);
                Arc::new(Mutex::new(()))
            } else {
                let lock = Arc::new(Mutex::new(()));
                in_flight.insert(key.clone(), lock.clone());
                lock
            }
        };

        // Hold the per-key mutex for the duration of the load so any waiter
        // blocks until we've populated the cache.
        let guard = lock.lock().map_err(|e| {
            EngineError::IoError(std::io::Error::other(format!("Cache loader mutex poisoned: {}", e)))
        })?;

        let result = self.loader.load(key, engine);

        match &result {
            Ok(value) => {
                if let Ok(mut entries) = self.entries.write() {
                    entries.insert(key.clone(), value.clone());
                }
            }
            Err(_) => {
                // Don't cache errors. Waiters will fall through and retry.
            }
        }

        // Remove the in_flight entry BEFORE releasing the per-key mutex so a new
        // caller arriving after we drop the guard creates a fresh entry rather
        // than waiting on our already-completed mutex.
        if let Ok(mut in_flight) = self.in_flight.lock() {
            in_flight.remove(key);
        }
        drop(guard);

        result
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

    /// Current number of cached entries. Best-effort: returns 0 if the read
    /// lock is poisoned. Used by soak-test instrumentation to attribute RSS
    /// growth to specific caches.
    pub fn len(&self) -> usize {
        self.entries.read().map(|m| m.len()).unwrap_or(0)
    }

    /// True if the cache currently holds zero entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
