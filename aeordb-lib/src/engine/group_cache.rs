use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::engine::errors::EngineResult;
use crate::engine::group::Group;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::system_store;
use crate::engine::user::User;

/// Cached group memberships for a single user.
struct CacheEntry {
  groups: Vec<String>,
  fetched_at: Instant,
}

/// In-memory cache mapping user_id to the list of group names they belong to.
/// Membership is evaluated lazily and cached with a TTL.
pub struct GroupCache {
  entries: RwLock<HashMap<Uuid, CacheEntry>>,
  ttl: Duration,
}

impl GroupCache {
  /// Create a new group cache with the given TTL.
  pub fn new(ttl: Duration) -> Self {
    GroupCache {
      entries: RwLock::new(HashMap::new()),
      ttl,
    }
  }

  /// Get the list of group names a user belongs to.
  /// Returns cached result if available and not expired; otherwise loads from engine.
  pub fn get_groups(&self, user_id: &Uuid, engine: &StorageEngine) -> EngineResult<Vec<String>> {
    // Check cache first (read lock).
    {
      let entries = self.entries.read().map_err(|error| {
        crate::engine::errors::EngineError::IoError(
          std::io::Error::other(format!("GroupCache read lock poisoned: {}", error)),
        )
      })?;

      if let Some(entry) = entries.get(user_id) {
        if entry.fetched_at.elapsed() < self.ttl {
          return Ok(entry.groups.clone());
        }
      }
    }

    // Cache miss or expired -- load from engine.
    let groups = self.load_user_groups(user_id, engine)?;

    // Store in cache (write lock).
    {
      let mut entries = self.entries.write().map_err(|error| {
        crate::engine::errors::EngineError::IoError(
          std::io::Error::other(format!("GroupCache write lock poisoned: {}", error)),
        )
      })?;

      entries.insert(*user_id, CacheEntry {
        groups: groups.clone(),
        fetched_at: Instant::now(),
      });
    }

    Ok(groups)
  }

  /// Evict a specific user's cached group memberships.
  pub fn evict_user(&self, user_id: &Uuid) {
    if let Ok(mut entries) = self.entries.write() {
      entries.remove(user_id);
    }
  }

  /// Flush the entire cache. Use when a group's query changes.
  pub fn evict_all(&self) {
    if let Ok(mut entries) = self.entries.write() {
      entries.clear();
    }
  }

  /// Load all groups from system_store and evaluate membership for a user.
  fn load_user_groups(&self, user_id: &Uuid, engine: &StorageEngine) -> EngineResult<Vec<String>> {
    // Load the user record.
    let user: User = match system_store::get_user(engine, user_id)? {
      Some(user) => user,
      None => return Ok(Vec::new()),
    };

    // Load all groups.
    let all_groups: Vec<Group> = system_store::list_groups(engine)?;

    // Evaluate membership for each group.
    let mut member_groups = Vec::new();
    for group in &all_groups {
      if group.evaluate_membership(&user) {
        member_groups.push(group.name.clone());
      }
    }

    Ok(member_groups)
  }
}
