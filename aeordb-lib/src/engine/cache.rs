use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};

use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::storage_engine::StorageEngine;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct CleanCacheStats {
  pub entries: usize,
  pub resident_bytes: u64,
  pub max_bytes: Option<u64>,
  pub hits: u64,
  pub misses: u64,
  pub evictions: u64,
  pub evicted_entries: u64,
  pub evicted_bytes: u64,
  pub admission_skips: u64,
}

#[derive(Clone)]
struct CleanCachePolicy {
  coordinator: MemoryCoordinator,
  owner: MemoryOwner,
  max_bytes: u64,
}

struct CleanCacheEntry<V> {
  value: V,
  weight: u64,
  last_access: AtomicU64,
  _reservation: Option<MemoryReservation>,
}

struct CleanCacheState<K, V> {
  entries: HashMap<K, CleanCacheEntry<V>>,
  policy: Option<CleanCachePolicy>,
  resident_bytes: u64,
}

impl<K, V> Default for CleanCacheState<K, V> {
  fn default() -> Self {
    Self { entries: HashMap::new(), policy: None, resident_bytes: 0 }
  }
}

/// A clean, rebuildable cache whose retained bytes own memory reservations.
///
/// Values are never correctness authority. If admission fails, insertion is
/// skipped and the caller keeps using the value it already loaded. A zero-byte
/// limit is therefore a deliberate no-retention policy rather than an error.
pub struct CleanCache<K, V> {
  state: RwLock<CleanCacheState<K, V>>,
  access_clock: AtomicU64,
  hits: AtomicU64,
  misses: AtomicU64,
  evictions: AtomicU64,
  evicted_entries: AtomicU64,
  evicted_bytes: AtomicU64,
  admission_skips: AtomicU64,
}

impl<K, V> Default for CleanCache<K, V> {
  fn default() -> Self {
    Self::new()
  }
}

impl<K, V> CleanCache<K, V> {
  pub fn new() -> Self {
    Self::from_state(CleanCacheState::default())
  }

  pub fn new_bounded(coordinator: MemoryCoordinator, owner: MemoryOwner, max_bytes: u64) -> Self {
    let mut state = CleanCacheState::default();
    state.policy = Some(CleanCachePolicy { coordinator, owner, max_bytes });
    Self::from_state(state)
  }

  fn from_state(state: CleanCacheState<K, V>) -> Self {
    Self {
      state: RwLock::new(state),
      access_clock: AtomicU64::new(1),
      hits: AtomicU64::new(0),
      misses: AtomicU64::new(0),
      evictions: AtomicU64::new(0),
      evicted_entries: AtomicU64::new(0),
      evicted_bytes: AtomicU64::new(0),
      admission_skips: AtomicU64::new(0),
    }
  }

  fn next_access(&self) -> u64 {
    self.access_clock.fetch_add(1, Ordering::Relaxed)
  }

  pub fn activate_bounded(&self, coordinator: MemoryCoordinator, owner: MemoryOwner, max_bytes: u64) -> EngineResult<()> {
    let mut state = self
      .state
      .write()
      .map_err(|error| EngineError::IoError(std::io::Error::other(format!("Clean cache write lock poisoned: {error}"))))?;
    state.entries.clear();
    state.resident_bytes = 0;
    state.policy = Some(CleanCachePolicy { coordinator, owner, max_bytes });
    Ok(())
  }

  pub fn stats(&self) -> EngineResult<CleanCacheStats> {
    let state =
      self.state.read().map_err(|error| EngineError::IoError(std::io::Error::other(format!("Clean cache read lock poisoned: {error}"))))?;
    Ok(CleanCacheStats {
      entries: state.entries.len(),
      resident_bytes: state.resident_bytes,
      max_bytes: state.policy.as_ref().map(|policy| policy.max_bytes),
      hits: self.hits.load(Ordering::Relaxed),
      misses: self.misses.load(Ordering::Relaxed),
      evictions: self.evictions.load(Ordering::Relaxed),
      evicted_entries: self.evicted_entries.load(Ordering::Relaxed),
      evicted_bytes: self.evicted_bytes.load(Ordering::Relaxed),
      admission_skips: self.admission_skips.load(Ordering::Relaxed),
    })
  }

  pub fn len(&self) -> usize {
    self.state.read().map(|state| state.entries.len()).unwrap_or(0)
  }

  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }

  pub fn clear(&self) -> EngineResult<()> {
    let mut state = self
      .state
      .write()
      .map_err(|error| EngineError::IoError(std::io::Error::other(format!("Clean cache write lock poisoned: {error}"))))?;
    state.entries.clear();
    state.resident_bytes = 0;
    Ok(())
  }
}

impl<K, V> CleanCache<K, V>
where
  K: Eq + Hash + Clone,
  V: Clone,
{
  pub fn reconfigure_max_bytes(&self, max_bytes: u64) -> EngineResult<()> {
    let mut state = self
      .state
      .write()
      .map_err(|error| EngineError::IoError(std::io::Error::other(format!("Clean cache write lock poisoned: {error}"))))?;
    let Some(policy) = state.policy.as_mut() else {
      return Err(EngineError::InvalidInput("cannot reconfigure an unbounded clean cache".to_string()));
    };
    policy.max_bytes = max_bytes;
    while state.resident_bytes > max_bytes {
      if !self.evict_lru_locked(&mut state, None) {
        return Err(EngineError::ResourceExhausted(format!(
          "clean cache retains {} bytes after applying a {max_bytes}-byte limit",
          state.resident_bytes
        )));
      }
    }
    Ok(())
  }

  pub fn get(&self, key: &K) -> EngineResult<Option<V>> {
    let state =
      self.state.read().map_err(|error| EngineError::IoError(std::io::Error::other(format!("Clean cache read lock poisoned: {error}"))))?;
    if let Some(entry) = state.entries.get(key) {
      entry.last_access.store(self.next_access(), Ordering::Relaxed);
      let value = entry.value.clone();
      self.hits.fetch_add(1, Ordering::Relaxed);
      Ok(Some(value))
    } else {
      self.misses.fetch_add(1, Ordering::Relaxed);
      Ok(None)
    }
  }

  pub fn insert_with_weight(&self, key: K, value: V, weight: u64) -> EngineResult<bool> {
    let mut state = self
      .state
      .write()
      .map_err(|error| EngineError::IoError(std::io::Error::other(format!("Clean cache write lock poisoned: {error}"))))?;

    let Some(policy) = state.policy.clone() else {
      if let Some(previous) = state.entries.remove(&key) {
        state.resident_bytes = state.resident_bytes.saturating_sub(previous.weight);
      }
      state.resident_bytes = state.resident_bytes.saturating_add(weight);
      state.entries.insert(key, CleanCacheEntry { value, weight, last_access: AtomicU64::new(self.next_access()), _reservation: None });
      return Ok(true);
    };

    if policy.max_bytes == 0 || weight > policy.max_bytes {
      self.admission_skips.fetch_add(1, Ordering::Relaxed);
      return Ok(false);
    }

    let replacing = state.entries.contains_key(&key);
    let previous_weight = state.entries.get(&key).map_or(0, |entry| entry.weight);
    while state.resident_bytes.saturating_sub(previous_weight).saturating_add(weight) > policy.max_bytes {
      if !self.evict_lru_locked(&mut state, Some(&key)) {
        self.admission_skips.fetch_add(1, Ordering::Relaxed);
        return Ok(false);
      }
    }

    if replacing {
      let resize_result = loop {
        let result = {
          let entry = state.entries.get_mut(&key).expect("replacement cache entry remains admitted");
          let reservation = entry
            ._reservation
            .as_mut()
            .ok_or_else(|| EngineError::IoError(std::io::Error::other("bounded clean cache replacement has no memory reservation")))?;
          if weight > previous_weight {
            reservation.grow(weight - previous_weight)
          } else if weight < previous_weight {
            reservation.shrink(previous_weight - weight)
          } else {
            Ok(())
          }
        };
        match result {
          Ok(()) => break Ok(()),
          Err(error) if is_cache_admission_refusal(&error) && weight > previous_weight && self.evict_lru_locked(&mut state, Some(&key)) => {
          }
          Err(error) => break Err(error),
        }
      };
      match resize_result {
        Ok(()) => {
          let entry = state.entries.get_mut(&key).expect("replacement cache entry remains admitted after resize");
          entry.value = value;
          entry.weight = weight;
          entry.last_access.store(self.next_access(), Ordering::Relaxed);
          state.resident_bytes = state.resident_bytes.saturating_sub(previous_weight).saturating_add(weight);
          return Ok(true);
        }
        Err(error) if is_cache_admission_refusal(&error) => {
          self.admission_skips.fetch_add(1, Ordering::Relaxed);
          return Ok(false);
        }
        Err(error) => {
          return Err(EngineError::IoError(std::io::Error::other(format!("Clean cache memory accounting failed: {error}"))));
        }
      }
    }

    let mut reservation = policy.coordinator.reserve(policy.owner, weight, AdmissionClass::Cache);
    if reservation.is_err() {
      while self.evict_lru_locked(&mut state, None) {
        reservation = policy.coordinator.reserve(policy.owner, weight, AdmissionClass::Cache);
        if reservation.is_ok() {
          break;
        }
      }
    }
    let reservation = match reservation {
      Ok(reservation) => reservation,
      Err(error) if is_cache_admission_refusal(&error) => {
        self.admission_skips.fetch_add(1, Ordering::Relaxed);
        return Ok(false);
      }
      Err(error) => {
        return Err(EngineError::IoError(std::io::Error::other(format!("Clean cache memory accounting failed: {error}"))));
      }
    };

    state.resident_bytes = state.resident_bytes.saturating_add(weight);
    state
      .entries
      .insert(key, CleanCacheEntry { value, weight, last_access: AtomicU64::new(self.next_access()), _reservation: Some(reservation) });
    Ok(true)
  }

  pub fn remove(&self, key: &K) -> EngineResult<bool> {
    let mut state = self
      .state
      .write()
      .map_err(|error| EngineError::IoError(std::io::Error::other(format!("Clean cache write lock poisoned: {error}"))))?;
    let Some(entry) = state.entries.remove(key) else {
      return Ok(false);
    };
    state.resident_bytes = state.resident_bytes.saturating_sub(entry.weight);
    Ok(true)
  }

  pub fn remove_where<F>(&self, mut predicate: F) -> EngineResult<usize>
  where
    F: FnMut(&K) -> bool,
  {
    let mut state = self
      .state
      .write()
      .map_err(|error| EngineError::IoError(std::io::Error::other(format!("Clean cache write lock poisoned: {error}"))))?;
    let mut removed_entries = 0usize;
    let mut removed_bytes = 0u64;
    state.entries.retain(|key, entry| {
      if !predicate(key) {
        return true;
      }
      removed_entries = removed_entries.saturating_add(1);
      removed_bytes = removed_bytes.saturating_add(entry.weight);
      false
    });
    state.resident_bytes = state.resident_bytes.saturating_sub(removed_bytes);
    Ok(removed_entries)
  }

  fn evict_lru_locked(&self, state: &mut CleanCacheState<K, V>, excluded: Option<&K>) -> bool {
    let Some(key) = state
      .entries
      .iter()
      .filter(|(key, _)| excluded != Some(*key))
      .min_by_key(|(_, entry)| entry.last_access.load(Ordering::Relaxed))
      .map(|(key, _)| key.clone())
    else {
      return false;
    };
    let Some(entry) = state.entries.remove(&key) else {
      return false;
    };
    state.resident_bytes = state.resident_bytes.saturating_sub(entry.weight);
    self.evictions.fetch_add(1, Ordering::Relaxed);
    self.evicted_entries.fetch_add(1, Ordering::Relaxed);
    self.evicted_bytes.fetch_add(entry.weight, Ordering::Relaxed);
    true
  }
}

fn is_cache_admission_refusal(error: &MemoryCoordinatorError) -> bool {
  matches!(
    error,
    MemoryCoordinatorError::PolicyUnavailable
      | MemoryCoordinatorError::HardLimitExceeded { .. }
      | MemoryCoordinatorError::SoftPressureDeferred { .. }
      | MemoryCoordinatorError::EmergencyReserveExceeded { .. }
  )
}

/// Trait for loading a cache entry on miss. Implementors define the key/value
/// types and how to fetch a value from the engine when it's not cached.
pub trait CacheLoader: Send + Sync {
  type Key: Eq + Hash + Clone + Send + Sync;
  type Value: Clone + Send + Sync;

  fn load(&self, key: &Self::Key, engine: &StorageEngine) -> EngineResult<Self::Value>;

  fn estimated_entry_bytes(&self, _key: &Self::Key, _value: &Self::Value) -> u64 {
    std::mem::size_of::<Self::Key>().saturating_add(std::mem::size_of::<Self::Value>()) as u64
  }
}

enum CacheLoadState<V> {
  Loading,
  Loaded(V),
  Failed,
}

struct CacheLoad<V> {
  state: Mutex<CacheLoadState<V>>,
  ready: Condvar,
}

impl<V> CacheLoad<V> {
  fn new() -> Self {
    Self { state: Mutex::new(CacheLoadState::Loading), ready: Condvar::new() }
  }
}

/// Generic eviction-based cache. No TTL — entries live until explicitly evicted.
/// Uses RwLock for concurrent reads on the hot path. A per-key in-flight map
/// provides singleflight semantics so a burst of cold misses for the same key
/// invokes the loader once, not N times.
pub struct Cache<L: CacheLoader> {
  entries: CleanCache<L::Key, L::Value>,
  in_flight: Mutex<HashMap<L::Key, Arc<CacheLoad<L::Value>>>>,
  loader: L,
}

impl<L: CacheLoader> Cache<L> {
  /// Create a new cache with the given loader.
  pub fn new(loader: L) -> Self {
    Cache { entries: CleanCache::new(), in_flight: Mutex::new(HashMap::new()), loader }
  }

  pub fn new_bounded(loader: L, coordinator: MemoryCoordinator, max_bytes: u64) -> Self {
    Cache {
      entries: CleanCache::new_bounded(coordinator, MemoryOwner::ServerCaches, max_bytes),
      in_flight: Mutex::new(HashMap::new()),
      loader,
    }
  }

  pub fn activate_bounded(&self, coordinator: MemoryCoordinator, max_bytes: u64) -> EngineResult<()> {
    self.entries.activate_bounded(coordinator, MemoryOwner::ServerCaches, max_bytes)
  }

  /// Get a value by key. Returns the cached value if present, otherwise
  /// calls the loader, caches the result, and returns it. Concurrent cold
  /// misses for the same key wait on a single in-flight load.
  /// Errors from the loader are propagated (not cached).
  pub fn get(&self, key: &L::Key, engine: &StorageEngine) -> EngineResult<L::Value> {
    loop {
      if let Some(value) = self.entries.get(key)? {
        return Ok(value);
      }

      let (load, is_owner) = {
        let mut in_flight =
          self.in_flight.lock().map_err(|e| EngineError::IoError(std::io::Error::other(format!("Cache in_flight poisoned: {}", e))))?;
        match in_flight.get(key) {
          Some(existing) => (Arc::clone(existing), false),
          None => {
            let load = Arc::new(CacheLoad::new());
            in_flight.insert(key.clone(), Arc::clone(&load));
            (load, true)
          }
        }
      };

      if !is_owner {
        let mut state =
          load.state.lock().map_err(|error| EngineError::IoError(std::io::Error::other(format!("Cache load state poisoned: {error}"))))?;
        while matches!(*state, CacheLoadState::Loading) {
          state = load
            .ready
            .wait(state)
            .map_err(|error| EngineError::IoError(std::io::Error::other(format!("Cache load wait poisoned: {error}"))))?;
        }
        match &*state {
          CacheLoadState::Loaded(value) => return Ok(value.clone()),
          CacheLoadState::Failed => continue,
          CacheLoadState::Loading => unreachable!("cache load wait returned before completion"),
        }
      }

      let result = self.loader.load(key, engine);
      let retention_result = match &result {
        Ok(value) => {
          let weight = self.loader.estimated_entry_bytes(key, value);
          self.entries.insert_with_weight(key.clone(), value.clone(), weight).map(|_| ())
        }
        // Don't cache errors. Waiters retry through a new singleflight owner.
        Err(_) => Ok(()),
      };
      let shared_value = match (&result, &retention_result) {
        (Ok(value), Ok(())) => Some(value.clone()),
        _ => None,
      };

      {
        let mut state =
          load.state.lock().map_err(|error| EngineError::IoError(std::io::Error::other(format!("Cache load state poisoned: {error}"))))?;
        *state = match shared_value {
          Some(value) => CacheLoadState::Loaded(value),
          None => CacheLoadState::Failed,
        };
        load.ready.notify_all();
      }
      {
        let mut in_flight = self
          .in_flight
          .lock()
          .map_err(|error| EngineError::IoError(std::io::Error::other(format!("Cache in_flight cleanup poisoned: {error}"))))?;
        if in_flight.get(key).is_some_and(|current| Arc::ptr_eq(current, &load)) {
          in_flight.remove(key);
        }
      }
      retention_result?;
      return result;
    }
  }

  /// Evict a single entry by key.
  pub fn evict(&self, key: &L::Key) -> EngineResult<bool> {
    self.entries.remove(key)
  }

  /// Flush the entire cache.
  pub fn evict_all(&self) -> EngineResult<()> {
    self.entries.clear()
  }

  /// Current number of cached entries. Best-effort: returns 0 if the read
  /// lock is poisoned. Used by soak-test instrumentation to attribute RSS
  /// growth to specific caches.
  pub fn len(&self) -> usize {
    self.entries.len()
  }

  /// True if the cache currently holds zero entries.
  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }

  /// Estimate container-owned bytes. Heap allocations retained inside a
  /// loader-specific key or value remain part of RSS remainder until that
  /// loader supplies a deeper accounting adapter.
  pub fn estimated_container_bytes(&self) -> EngineResult<u64> {
    let entry_bytes = self.entries.stats()?.resident_bytes;
    let in_flight = self
      .in_flight
      .lock()
      .map_err(|error| EngineError::IoError(std::io::Error::other(format!("Cache in_flight lock poisoned: {error}"))))?;
    let in_flight_bytes = in_flight
      .capacity()
      .saturating_mul(std::mem::size_of::<(L::Key, Arc<CacheLoad<L::Value>>)>().saturating_add(2 * std::mem::size_of::<usize>()));
    Ok((std::mem::size_of::<Self>() as u64).saturating_add(entry_bytes).saturating_add(in_flight_bytes as u64))
  }

  pub fn stats(&self) -> EngineResult<CleanCacheStats> {
    self.entries.stats()
  }
}
