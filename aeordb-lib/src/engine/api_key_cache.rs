use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use crate::auth::api_key::ApiKeyRecord;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::storage_engine::StorageEngine;
use crate::engine::system_store;

/// Cached API key record with fetch timestamp.
struct CacheEntry {
    record: ApiKeyRecord,
    fetched_at: Instant,
}

/// In-memory cache mapping key_id strings to their ApiKeyRecord.
/// Entries are lazily loaded and cached with a TTL.
pub struct ApiKeyCache {
    entries: RwLock<HashMap<String, CacheEntry>>,
    ttl: Duration,
}

impl ApiKeyCache {
    /// Create a new API key cache with the given TTL.
    pub fn new(ttl: Duration) -> Self {
        ApiKeyCache {
            entries: RwLock::new(HashMap::new()),
            ttl,
        }
    }

    /// Get an API key record by key_id string.
    /// Returns cached result if available and not expired; otherwise loads from engine.
    pub fn get_key(&self, key_id: &str, engine: &StorageEngine) -> EngineResult<Option<ApiKeyRecord>> {
        // Check cache first (read lock).
        {
            let entries = self.entries.read().map_err(|error| {
                EngineError::IoError(
                    std::io::Error::other(format!("ApiKeyCache read lock poisoned: {}", error)),
                )
            })?;
            if let Some(entry) = entries.get(key_id) {
                if entry.fetched_at.elapsed() < self.ttl {
                    return Ok(Some(entry.record.clone()));
                }
            }
        }

        // Cache miss or expired — load from engine.
        let key_uuid = match uuid::Uuid::parse_str(key_id) {
            Ok(id) => id,
            Err(_) => return Ok(None),
        };

        let all_keys = system_store::list_api_keys(engine)?;

        let record = all_keys.into_iter().find(|k| k.key_id == key_uuid);

        // Store in cache (write lock).
        if let Some(ref rec) = record {
            let mut entries = self.entries.write().map_err(|error| {
                EngineError::IoError(
                    std::io::Error::other(format!("ApiKeyCache write lock poisoned: {}", error)),
                )
            })?;
            entries.insert(key_id.to_string(), CacheEntry {
                record: rec.clone(),
                fetched_at: Instant::now(),
            });
        }

        Ok(record)
    }

    /// Invalidate a cached key (call on revoke/delete).
    pub fn invalidate(&self, key_id: &str) {
        if let Ok(mut entries) = self.entries.write() {
            entries.remove(key_id);
        }
    }

    /// Flush the entire cache.
    pub fn evict_all(&self) {
        if let Ok(mut entries) = self.entries.write() {
            entries.clear();
        }
    }
}
