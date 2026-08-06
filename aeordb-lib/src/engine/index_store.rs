use std::collections::{HashMap, HashSet};
use std::mem::size_of;
use std::time::{Duration, Instant};

use base64::Engine as _;

use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::index_config::{create_converter_from_config, IndexFieldConfig, PathIndexConfig};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::nvt::NormalizedVectorTable;
use crate::engine::path_utils::normalize_path;
use crate::engine::request_context::RequestContext;
use crate::engine::scalar_converter::{
  deserialize_converter, serialize_converter, ScalarConverter, CONVERTER_TYPE_PHONETIC, CONVERTER_TYPE_TRIGRAM,
};
use crate::engine::storage_engine::StorageEngine;

/// Default number of NVT buckets for a new FieldIndex.
const DEFAULT_NVT_BUCKET_COUNT: usize = 1024;
/// Default number of in-memory index mutations before buffered index flush.
pub const DEFAULT_INDEX_BUFFER_FLUSH_WRITES: usize = 262_144;
/// Default maximum age of unflushed buffered index mutations.
pub const DEFAULT_INDEX_BUFFER_FLUSH_INTERVAL: Duration = Duration::from_secs(30);
/// Default upper bound for clean in-memory index cache entries.
pub const DEFAULT_INDEX_CACHE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Default idle age before a clean in-memory index may be evicted.
pub const DEFAULT_INDEX_CACHE_CLEAN_TTL: Duration = Duration::from_secs(5 * 60);
/// Bounded headroom for NVT rebuild and nested converter/NVT serialization
/// allocations that coexist with the final publication buffer.
const INDEX_FLUSH_TRANSIENT_OVERHEAD_BYTES: u64 = 64 * 1024;
const INDEX_LOAD_FIXED_OVERHEAD_BYTES: u64 = 8 * 1024 * 1024;
const INDEX_LOAD_SERIALIZED_AMPLIFICATION: u64 = 8;

/// A single entry in a field index: maps a scalar to a file's hash.
#[derive(Debug, Clone)]
pub struct IndexEntry {
  pub scalar: f64,
  pub file_hash: Vec<u8>,
}

/// A field index: converter + sorted entries + NVT bucket index.
pub struct FieldIndex {
  pub field_name: String,
  pub converter: Box<dyn ScalarConverter>,
  pub entries: Vec<IndexEntry>,
  pub nvt: NormalizedVectorTable,
  /// Raw field values keyed by file_hash. Used by fuzzy query recheck
  /// to avoid re-loading files from storage.
  ///
  /// NOTE: The values HashMap grows with every indexed file and is never
  /// pruned. At 1M files with 100-byte values, this is ~100MB per index field.
  /// Consider capping or implementing lazy loading from disk during recheck.
  pub values: HashMap<Vec<u8>, Vec<u8>>,
  values_cover_entries: bool,
  dirty: bool,
}

/// Flush policy for buffered index updates.
#[derive(Debug, Clone, Copy)]
pub struct IndexWriteBufferOptions {
  /// Flush after this many index mutations. One mutation is one file update
  /// against one field/strategy index.
  pub flush_after_writes: usize,
  /// Flush when unpersisted mutations have lived at least this long.
  pub flush_after: Duration,
}

impl Default for IndexWriteBufferOptions {
  fn default() -> Self {
    Self { flush_after_writes: DEFAULT_INDEX_BUFFER_FLUSH_WRITES, flush_after: DEFAULT_INDEX_BUFFER_FLUSH_INTERVAL }
  }
}

impl IndexWriteBufferOptions {
  pub fn new(flush_after_writes: usize, flush_after: Duration) -> Self {
    Self { flush_after_writes: flush_after_writes.max(1), flush_after }
  }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct IndexWriteBufferStats {
  pub mutations: usize,
  pub flushes: usize,
  pub flushed_indexes: usize,
  pub evictions: usize,
  pub evicted_indexes: usize,
  pub evicted_bytes: u64,
  pub cached_indexes: usize,
  pub dirty_indexes: usize,
  pub deleted_indexes: usize,
  pub pending_mutations: usize,
  pub entries: usize,
  pub values: usize,
  pub estimated_bytes: u64,
  pub estimated_clean_bytes: u64,
  pub estimated_dirty_bytes: u64,
  pub clean_reserved_bytes: u64,
  pub dirty_reserved_bytes: u64,
  pub flush_reserved_bytes: u64,
  pub flushing_indexes: usize,
  pub max_bytes: u64,
  pub mutation_max_bytes: u64,
  pub publication_batch_max_bytes: u64,
  pub clean_ttl_ms: u64,
  pub reservation_owned: bool,
  pub top_cached_indexes: Vec<CachedIndexMemoryStats>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CachedIndexMemoryStats {
  pub parent: String,
  pub field_name: String,
  pub strategy: String,
  pub entries: usize,
  pub values: usize,
  pub estimated_bytes: u64,
  pub dirty: bool,
  pub last_access_age_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct BufferedIndexKey {
  parent: String,
  field_name: String,
  strategy: String,
}

impl BufferedIndexKey {
  fn estimated_memory_bytes(&self) -> u64 {
    size_of::<Self>()
      .saturating_add(self.parent.capacity())
      .saturating_add(self.field_name.capacity())
      .saturating_add(self.strategy.capacity()) as u64
  }
}

pub(crate) struct IndexFlushSnapshot {
  saves: Vec<(BufferedIndexKey, Vec<u8>)>,
  deletes: Vec<BufferedIndexKey>,
  mutation_counts: Vec<(BufferedIndexKey, usize)>,
  pending_mutations: usize,
}

#[derive(Clone)]
pub(crate) struct IndexMemoryPolicy {
  coordinator: MemoryCoordinator,
  clean_max_bytes: u64,
  mutation_max_bytes: u64,
  publication_batch_max_bytes: u64,
  clean_ttl: Duration,
}

impl IndexMemoryPolicy {
  pub(crate) fn new(
    coordinator: MemoryCoordinator,
    clean_max_bytes: u64,
    mutation_max_bytes: u64,
    publication_batch_max_bytes: u64,
    clean_ttl: Duration,
  ) -> Self {
    Self { coordinator, clean_max_bytes, mutation_max_bytes, publication_batch_max_bytes, clean_ttl }
  }
}

/// Shared in-memory index mutation buffer.
///
/// Indexes are recoverable from file records, so every code path that mutates
/// indexes should update this shared in-memory state first and let the flush
/// policy persist dirty field/strategy indexes in batches.
pub(crate) struct SharedIndexWriteBuffer {
  options: IndexWriteBufferOptions,
  indexes: HashMap<BufferedIndexKey, FieldIndex>,
  last_access: HashMap<BufferedIndexKey, Instant>,
  dirty_keys: HashSet<BufferedIndexKey>,
  flushing_keys: HashSet<BufferedIndexKey>,
  deleted_keys: HashSet<BufferedIndexKey>,
  pending_by_key: HashMap<BufferedIndexKey, usize>,
  clean_reservations: HashMap<BufferedIndexKey, MemoryReservation>,
  dirty_reservations: HashMap<BufferedIndexKey, MemoryReservation>,
  flush_reservations: HashMap<BufferedIndexKey, MemoryReservation>,
  memory_policy: Option<IndexMemoryPolicy>,
  pending_mutations: usize,
  total_mutations: usize,
  flush_count: usize,
  flushed_indexes: usize,
  eviction_count: usize,
  evicted_indexes: usize,
  evicted_bytes: u64,
  last_flush: Instant,
}

impl Default for SharedIndexWriteBuffer {
  fn default() -> Self {
    Self::new(IndexWriteBufferOptions::default())
  }
}

impl SharedIndexWriteBuffer {
  pub(crate) fn new(options: IndexWriteBufferOptions) -> Self {
    SharedIndexWriteBuffer {
      options,
      indexes: HashMap::new(),
      last_access: HashMap::new(),
      dirty_keys: HashSet::new(),
      flushing_keys: HashSet::new(),
      deleted_keys: HashSet::new(),
      pending_by_key: HashMap::new(),
      clean_reservations: HashMap::new(),
      dirty_reservations: HashMap::new(),
      flush_reservations: HashMap::new(),
      memory_policy: None,
      pending_mutations: 0,
      total_mutations: 0,
      flush_count: 0,
      flushed_indexes: 0,
      eviction_count: 0,
      evicted_indexes: 0,
      evicted_bytes: 0,
      last_flush: Instant::now(),
    }
  }

  fn options(&self) -> IndexWriteBufferOptions {
    self.options
  }

  pub(crate) fn activate_memory_policy(&mut self, policy: IndexMemoryPolicy, options: IndexWriteBufferOptions) -> EngineResult<()> {
    if !self.indexes.is_empty()
      || !self.dirty_keys.is_empty()
      || !self.flushing_keys.is_empty()
      || !self.deleted_keys.is_empty()
      || !self.pending_by_key.is_empty()
      || !self.clean_reservations.is_empty()
      || !self.dirty_reservations.is_empty()
      || !self.flush_reservations.is_empty()
    {
      return Err(EngineError::InvalidInput("cannot activate index memory policy after index state has been admitted".to_string()));
    }
    self.options = options;
    self.memory_policy = Some(policy);
    Ok(())
  }

  fn effective_options(&self, override_options: Option<IndexWriteBufferOptions>) -> IndexWriteBufferOptions {
    override_options.unwrap_or_else(|| self.options())
  }

  fn should_flush(&self, override_options: Option<IndexWriteBufferOptions>) -> bool {
    let options = self.effective_options(override_options);
    self.pending_mutations > 0 && (self.pending_mutations >= options.flush_after_writes || self.last_flush.elapsed() >= options.flush_after)
  }

  fn touch(&mut self, key: &BufferedIndexKey) {
    self.last_access.insert(key.clone(), Instant::now());
  }

  fn record_mutation(&mut self, key: &BufferedIndexKey) {
    *self.pending_by_key.entry(key.clone()).or_insert(0) += 1;
    self.pending_mutations = self.pending_mutations.saturating_add(1);
    self.total_mutations = self.total_mutations.saturating_add(1);
  }

  fn memory_error(context: &str, error: MemoryCoordinatorError) -> EngineError {
    match error {
      MemoryCoordinatorError::HardLimitExceeded { .. }
      | MemoryCoordinatorError::SoftPressureDeferred { .. }
      | MemoryCoordinatorError::EmergencyReserveExceeded { .. }
      | MemoryCoordinatorError::PolicyUnavailable => EngineError::ResourceExhausted(format!("{context}: {error}")),
      _ => EngineError::IoError(std::io::Error::other(format!("{context}: {error}"))),
    }
  }

  fn is_clean_admission_refusal(error: &MemoryCoordinatorError) -> bool {
    matches!(
      error,
      MemoryCoordinatorError::PolicyUnavailable
        | MemoryCoordinatorError::HardLimitExceeded { .. }
        | MemoryCoordinatorError::SoftPressureDeferred { .. }
        | MemoryCoordinatorError::EmergencyReserveExceeded { .. }
    )
  }

  fn reservation_bytes(reservations: &HashMap<BufferedIndexKey, MemoryReservation>) -> u64 {
    reservations.values().fold(0u64, |total, reservation| total.saturating_add(reservation.bytes()))
  }

  fn reserve_dirty_exact(&mut self, key: &BufferedIndexKey, required_bytes: u64) -> EngineResult<()> {
    let Some(policy) = self.memory_policy.clone() else {
      return Ok(());
    };
    let previous_bytes = self.dirty_reservations.get(key).map_or(0, MemoryReservation::bytes);
    let projected = Self::reservation_bytes(&self.dirty_reservations).saturating_sub(previous_bytes).saturating_add(required_bytes);
    if projected > policy.mutation_max_bytes {
      return Err(EngineError::ResourceExhausted(format!(
        "index mutation buffer requires {projected} bytes but index.mutation_buffer_max_bytes is {}",
        policy.mutation_max_bytes
      )));
    }

    if let Some(reservation) = self.dirty_reservations.get_mut(key) {
      if required_bytes > previous_bytes {
        reservation
          .grow(required_bytes - previous_bytes)
          .map_err(|error| Self::memory_error("index mutation memory admission failed", error))?;
      } else if required_bytes < previous_bytes {
        reservation
          .shrink(previous_bytes - required_bytes)
          .map_err(|error| Self::memory_error("index mutation memory accounting failed", error))?;
      }
      return Ok(());
    }

    let reservation = policy
      .coordinator
      .reserve(MemoryOwner::IndexDirtyBuffers, required_bytes, AdmissionClass::Workload)
      .map_err(|error| Self::memory_error("index mutation memory admission failed", error))?;
    self.clean_reservations.remove(key);
    self.dirty_reservations.insert(key.clone(), reservation);
    Ok(())
  }

  fn prepare_dirty_reservation(&self, key: &BufferedIndexKey, required_bytes: u64) -> EngineResult<Option<MemoryReservation>> {
    let Some(policy) = self.memory_policy.as_ref() else {
      return Ok(None);
    };
    let previous_bytes = self.dirty_reservations.get(key).map_or(0, MemoryReservation::bytes);
    let projected = Self::reservation_bytes(&self.dirty_reservations).saturating_sub(previous_bytes).saturating_add(required_bytes);
    if projected > policy.mutation_max_bytes {
      return Err(EngineError::ResourceExhausted(format!(
        "index mutation buffer requires {projected} bytes but index.mutation_buffer_max_bytes is {}",
        policy.mutation_max_bytes
      )));
    }
    policy
      .coordinator
      .reserve(MemoryOwner::IndexDirtyBuffers, required_bytes, AdmissionClass::Workload)
      .map(Some)
      .map_err(|error| Self::memory_error("index mutation memory admission failed", error))
  }

  fn adopt_dirty_reservation(
    &mut self,
    key: &BufferedIndexKey,
    required_bytes: u64,
    prepared: Option<MemoryReservation>,
  ) -> EngineResult<()> {
    let Some(policy) = self.memory_policy.as_ref() else {
      return Ok(());
    };
    let mut reservation =
      prepared.ok_or_else(|| EngineError::IoError(std::io::Error::other("bounded index insertion has no reservation")))?;
    if reservation.bytes() < required_bytes {
      return Err(EngineError::IoError(std::io::Error::other("prepared index reservation is smaller than retained index state")));
    }
    if reservation.bytes() > required_bytes {
      reservation
        .shrink(reservation.bytes() - required_bytes)
        .map_err(|error| Self::memory_error("prepared index memory accounting failed", error))?;
    }
    let previous_bytes = self.dirty_reservations.get(key).map_or(0, MemoryReservation::bytes);
    let projected = Self::reservation_bytes(&self.dirty_reservations).saturating_sub(previous_bytes).saturating_add(reservation.bytes());
    if projected > policy.mutation_max_bytes {
      return Err(EngineError::ResourceExhausted(format!(
        "index mutation buffer changed during admission and now requires {projected} bytes but index.mutation_buffer_max_bytes is {}",
        policy.mutation_max_bytes
      )));
    }
    self.clean_reservations.remove(key);
    self.dirty_reservations.insert(key.clone(), reservation);
    Ok(())
  }

  fn release_key_reservations(&mut self, key: &BufferedIndexKey) {
    self.clean_reservations.remove(key);
    self.dirty_reservations.remove(key);
  }

  fn mutation_reservation_target(index: &FieldIndex, field_values: &[Vec<u8>], file_key: &[u8]) -> u64 {
    let current = index.estimated_memory_bytes();
    let value_bytes = field_values.iter().fold(0u64, |total, value| total.saturating_add(value.len() as u64));
    let maximum_expansions = field_values.iter().fold(0u64, |total, value| total.saturating_add(value.len().max(1) as u64));
    let entry_bytes = maximum_expansions.saturating_mul((size_of::<IndexEntry>() + file_key.len() + size_of::<Vec<u8>>()) as u64);
    current
      .saturating_mul(2)
      .saturating_add(value_bytes.saturating_mul(2))
      .saturating_add(entry_bytes.saturating_mul(2))
      .saturating_add(64 * 1024)
  }

  pub(crate) fn stats(&self) -> IndexWriteBufferStats {
    let now = Instant::now();
    let mut top_cached_indexes: Vec<CachedIndexMemoryStats> = self
      .indexes
      .iter()
      .map(|(key, index)| CachedIndexMemoryStats {
        parent: key.parent.clone(),
        field_name: key.field_name.clone(),
        strategy: key.strategy.clone(),
        entries: index.entries.len(),
        values: index.values.len(),
        estimated_bytes: index.estimated_memory_bytes(),
        dirty: self.dirty_keys.contains(key) || self.flushing_keys.contains(key),
        last_access_age_ms: self.last_access.get(key).map(|instant| now.saturating_duration_since(*instant).as_millis() as u64),
      })
      .collect();
    top_cached_indexes.sort_by(|left, right| right.estimated_bytes.cmp(&left.estimated_bytes));
    top_cached_indexes.truncate(8);

    let entries = self.indexes.values().map(|index| index.entries.len()).sum();
    let values = self.indexes.values().map(|index| index.values.len()).sum();
    let estimated_bytes = self.indexes.values().fold(0u64, |total, index| total.saturating_add(index.estimated_memory_bytes()));
    let estimated_dirty_bytes = self
      .indexes
      .iter()
      .filter(|(key, _)| self.dirty_keys.contains(*key) || self.flushing_keys.contains(*key))
      .fold(0u64, |total, (_, index)| total.saturating_add(index.estimated_memory_bytes()));
    let estimated_clean_bytes = estimated_bytes.saturating_sub(estimated_dirty_bytes);
    let clean_reserved_bytes: u64 = self.clean_reservations.values().map(MemoryReservation::bytes).sum();
    let dirty_reserved_bytes: u64 = self.dirty_reservations.values().map(MemoryReservation::bytes).sum();
    let flush_reserved_bytes: u64 = self.flush_reservations.values().map(MemoryReservation::bytes).sum();

    IndexWriteBufferStats {
      mutations: self.total_mutations,
      flushes: self.flush_count,
      flushed_indexes: self.flushed_indexes,
      evictions: self.eviction_count,
      evicted_indexes: self.evicted_indexes,
      evicted_bytes: self.evicted_bytes,
      cached_indexes: self.indexes.len(),
      dirty_indexes: self.dirty_keys.union(&self.flushing_keys).count(),
      deleted_indexes: self.deleted_keys.len(),
      pending_mutations: self.pending_mutations,
      entries,
      values,
      estimated_bytes,
      estimated_clean_bytes,
      estimated_dirty_bytes,
      clean_reserved_bytes,
      dirty_reserved_bytes,
      flush_reserved_bytes,
      flushing_indexes: self.flushing_keys.len(),
      max_bytes: self.memory_policy.as_ref().map_or(DEFAULT_INDEX_CACHE_MAX_BYTES, |policy| policy.clean_max_bytes),
      mutation_max_bytes: self.memory_policy.as_ref().map_or(u64::MAX, |policy| policy.mutation_max_bytes),
      publication_batch_max_bytes: self.memory_policy.as_ref().map_or(u64::MAX, |policy| policy.publication_batch_max_bytes),
      clean_ttl_ms: self.memory_policy.as_ref().map_or(DEFAULT_INDEX_CACHE_CLEAN_TTL, |policy| policy.clean_ttl).as_millis() as u64,
      reservation_owned: self.memory_policy.is_some(),
      top_cached_indexes,
    }
  }

  fn get_index_clone(&mut self, key: &BufferedIndexKey, hash_length: usize) -> EngineResult<Option<FieldIndex>> {
    if self.deleted_keys.contains(key) {
      return Ok(None);
    }
    match self.indexes.get(key) {
      Some(index) => {
        let clone = index.deep_clone(hash_length)?;
        self.touch(key);
        Ok(Some(clone))
      }
      None => Ok(None),
    }
  }

  fn index_clone_estimate(&self, key: &BufferedIndexKey) -> Option<u64> {
    if self.deleted_keys.contains(key) {
      return None;
    }
    self.indexes.get(key).map(FieldIndex::estimated_memory_bytes)
  }

  fn list_index_names(&self, parent: &str) -> Vec<String> {
    let mut names = Vec::new();
    for key in self.indexes.keys() {
      if key.parent == parent && !self.deleted_keys.contains(key) {
        names.push(format!("{}.{}", key.field_name, key.strategy));
      }
    }
    names
  }

  fn indexed_parents(&self) -> Vec<String> {
    let mut parents = Vec::new();
    for key in self.indexes.keys() {
      if !self.deleted_keys.contains(key) {
        parents.push(key.parent.clone());
      }
    }
    parents
  }

  fn put_index(&mut self, key: BufferedIndexKey, index: FieldIndex, prepared: Option<MemoryReservation>) -> EngineResult<()> {
    self.adopt_dirty_reservation(&key, index.estimated_memory_bytes(), prepared)?;
    self.deleted_keys.remove(&key);
    self.indexes.insert(key.clone(), index);
    self.touch(&key);
    self.dirty_keys.insert(key.clone());
    self.record_mutation(&key);
    Ok(())
  }

  fn update_index(
    &mut self,
    key: BufferedIndexKey,
    initial_index: Option<FieldIndex>,
    create_index: impl FnOnce() -> EngineResult<FieldIndex>,
    field_values: &[Vec<u8>],
    file_key: &[u8],
  ) -> EngineResult<()> {
    if !self.indexes.contains_key(&key) {
      let index = match initial_index {
        Some(index) => index,
        None => create_index()?,
      };
      let reservation_target = Self::mutation_reservation_target(&index, field_values, file_key);
      self.reserve_dirty_exact(&key, reservation_target)?;
      self.indexes.insert(key.clone(), index);
    } else {
      let reservation_target =
        Self::mutation_reservation_target(self.indexes.get(&key).expect("buffered index exists before mutation"), field_values, file_key);
      self.reserve_dirty_exact(&key, reservation_target)?;
    }
    self.deleted_keys.remove(&key);

    let index = self.indexes.get_mut(&key).expect("buffered index exists after insertion");
    index.remove(file_key);
    for value in field_values {
      index.insert_expanded(value, file_key.to_vec());
    }
    let actual_bytes = index.estimated_memory_bytes();
    self.dirty_keys.insert(key.clone());
    self.touch(&key);
    self.record_mutation(&key);
    self.reserve_dirty_exact(&key, actual_bytes)?;
    Ok(())
  }

  fn remove_file_from_index(&mut self, key: BufferedIndexKey, initial_index: Option<FieldIndex>, file_key: &[u8]) -> EngineResult<()> {
    if self.deleted_keys.contains(&key) {
      return Ok(());
    }
    if !self.indexes.contains_key(&key) {
      if let Some(index) = initial_index {
        self.reserve_dirty_exact(&key, index.estimated_memory_bytes())?;
        self.indexes.insert(key.clone(), index);
        self.touch(&key);
      } else {
        return Ok(());
      }
    }

    let Some(index) = self.indexes.get_mut(&key) else {
      return Ok(());
    };
    let before_entries = index.len();
    let before_values = index.values.len();
    index.remove(file_key);
    if index.len() != before_entries || index.values.len() != before_values {
      let actual_bytes = index.estimated_memory_bytes();
      self.dirty_keys.insert(key.clone());
      self.touch(&key);
      self.record_mutation(&key);
      self.reserve_dirty_exact(&key, actual_bytes)?;
    }
    Ok(())
  }

  fn delete_index(&mut self, key: BufferedIndexKey) -> EngineResult<()> {
    self.reserve_dirty_exact(&key, key.estimated_memory_bytes())?;
    self.indexes.remove(&key);
    self.last_access.remove(&key);
    self.dirty_keys.remove(&key);
    self.deleted_keys.insert(key.clone());
    self.record_mutation(&key);
    Ok(())
  }

  fn snapshot_flush(&mut self, hash_length: usize) -> EngineResult<IndexFlushSnapshot> {
    if !self.flushing_keys.is_empty() {
      return Err(EngineError::ResourceExhausted("an index flush generation is already in progress".to_string()));
    }
    let publication_max_bytes = self.memory_policy.as_ref().map_or(u64::MAX, |policy| policy.publication_batch_max_bytes);
    let candidate_count = self.deleted_keys.union(&self.dirty_keys).count();
    let candidate_bytes = self
      .deleted_keys
      .union(&self.dirty_keys)
      .fold(size_of::<Vec<BufferedIndexKey>>() as u64, |total, key| total.saturating_add(key.estimated_memory_bytes()));
    let _candidate_reservation = match self.memory_policy.as_ref() {
      Some(policy) => Some(
        policy
          .coordinator
          .reserve(
            MemoryOwner::IndexDirtyBuffers,
            candidate_bytes,
            AdmissionClass::Critical(crate::engine::memory_coordinator::CriticalMemoryPurpose::DurableWrite),
          )
          .map_err(|error| Self::memory_error("index flush key-generation memory admission failed", error))?,
      ),
      None => None,
    };
    let mut candidates: Vec<BufferedIndexKey> = Vec::with_capacity(candidate_count);
    candidates.extend(self.deleted_keys.union(&self.dirty_keys).cloned());
    candidates.sort_by(|left, right| {
      left.parent.cmp(&right.parent).then_with(|| left.field_name.cmp(&right.field_name)).then_with(|| left.strategy.cmp(&right.strategy))
    });
    let mut saves = Vec::new();
    let mut deletes = Vec::new();
    let mut mutation_counts = Vec::new();
    let mut pending_mutations = 0usize;
    let mut publication_bytes = 0u64;

    for key in candidates {
      let delete = self.deleted_keys.contains(&key);
      let serialized_size = if delete {
        key.estimated_memory_bytes()
      } else {
        let Some(index) = self.indexes.get(&key) else {
          let partial = IndexFlushSnapshot { saves, deletes, mutation_counts, pending_mutations };
          self.restore_failed_flush(&partial);
          return Err(EngineError::IoError(std::io::Error::other(format!(
            "dirty index state has no cached index for {}.{} at {}",
            key.field_name, key.strategy, key.parent
          ))));
        };
        index.serialized_size(hash_length) as u64
      };
      if serialized_size > publication_max_bytes {
        let partial = IndexFlushSnapshot { saves, deletes, mutation_counts, pending_mutations };
        self.restore_failed_flush(&partial);
        return Err(EngineError::ResourceExhausted(format!(
          "index {}.{} at {} serializes to {serialized_size} bytes, exceeding index.publication_batch_max_bytes={publication_max_bytes}",
          key.field_name, key.strategy, key.parent
        )));
      }
      if (!saves.is_empty() || !deletes.is_empty()) && publication_bytes.saturating_add(serialized_size) > publication_max_bytes {
        break;
      }

      if let Some(policy) = self.memory_policy.as_ref() {
        let scratch_bytes = if delete {
          key.estimated_memory_bytes()
        } else {
          let Some(scratch_bytes) = serialized_size.checked_add(INDEX_FLUSH_TRANSIENT_OVERHEAD_BYTES) else {
            let partial = IndexFlushSnapshot { saves, deletes, mutation_counts, pending_mutations };
            self.restore_failed_flush(&partial);
            return Err(EngineError::ResourceExhausted(format!(
              "index {}.{} at {} has an overflowing flush-memory requirement",
              key.field_name, key.strategy, key.parent
            )));
          };
          scratch_bytes
        };
        let reservation = match policy.coordinator.reserve(
          MemoryOwner::IndexDirtyBuffers,
          scratch_bytes,
          AdmissionClass::Critical(crate::engine::memory_coordinator::CriticalMemoryPurpose::DurableWrite),
        ) {
          Ok(reservation) => reservation,
          Err(error) => {
            let partial = IndexFlushSnapshot { saves, deletes, mutation_counts, pending_mutations };
            self.restore_failed_flush(&partial);
            return Err(Self::memory_error("index flush serialization memory admission failed", error));
          }
        };
        self.flush_reservations.insert(key.clone(), reservation);
      }
      if !delete {
        let index = self.indexes.get_mut(&key).expect("selected dirty index remains cached");
        index.ensure_nvt_current();
        let data = index.serialize(hash_length);
        debug_assert_eq!(data.len() as u64, serialized_size);
        saves.push((key.clone(), data));
        self.dirty_keys.remove(&key);
      } else {
        deletes.push(key.clone());
        self.deleted_keys.remove(&key);
      }
      self.flushing_keys.insert(key.clone());
      let key_mutations = self.pending_by_key.remove(&key).unwrap_or(0);
      mutation_counts.push((key, key_mutations));
      pending_mutations = pending_mutations.saturating_add(key_mutations);
      self.pending_mutations = self.pending_mutations.saturating_sub(key_mutations);
      publication_bytes = publication_bytes.saturating_add(serialized_size);
    }

    if saves.is_empty() && deletes.is_empty() && self.pending_mutations > 0 {
      return Err(EngineError::IoError(std::io::Error::other("index pending-mutation accounting has no dirty or deleted key")));
    }

    Ok(IndexFlushSnapshot { saves, deletes, mutation_counts, pending_mutations })
  }

  fn restore_failed_flush(&mut self, snapshot: &IndexFlushSnapshot) {
    for (key, _) in &snapshot.saves {
      self.flushing_keys.remove(key);
      self.flush_reservations.remove(key);
      if self.indexes.contains_key(key) && !self.deleted_keys.contains(key) {
        self.dirty_keys.insert(key.clone());
      }
    }
    for key in &snapshot.deletes {
      self.flushing_keys.remove(key);
      self.flush_reservations.remove(key);
      if !self.indexes.contains_key(key) {
        self.deleted_keys.insert(key.clone());
      }
    }
    for (key, count) in &snapshot.mutation_counts {
      let restored = self.pending_by_key.entry(key.clone()).or_insert(0);
      *restored = restored.saturating_add(*count);
    }
    self.pending_mutations = self.pending_mutations.saturating_add(snapshot.pending_mutations);
  }

  fn finish_successful_flush(&mut self, snapshot: &IndexFlushSnapshot) -> EngineResult<()> {
    let mut first_error = None;
    for (key, _) in &snapshot.saves {
      self.flushing_keys.remove(key);
      self.flush_reservations.remove(key);
      if self.dirty_keys.contains(key) || self.deleted_keys.contains(key) {
        continue;
      }
      if let Err(error) = self.transition_persisted_index_to_clean(key) {
        first_error.get_or_insert(error);
      }
    }
    for key in &snapshot.deletes {
      self.flushing_keys.remove(key);
      self.flush_reservations.remove(key);
      if self.dirty_keys.contains(key) || self.deleted_keys.contains(key) || self.indexes.contains_key(key) {
        continue;
      }
      self.release_key_reservations(key);
    }
    self.flush_count = self.flush_count.saturating_add(1);
    self.flushed_indexes = self.flushed_indexes.saturating_add(snapshot.saves.len());
    self.last_flush = Instant::now();
    match first_error {
      Some(error) => Err(error),
      None => Ok(()),
    }
  }

  fn transition_persisted_index_to_clean(&mut self, key: &BufferedIndexKey) -> EngineResult<()> {
    let Some(policy) = self.memory_policy.clone() else {
      self.dirty_reservations.remove(key);
      return Ok(());
    };
    let Some(index_bytes) = self.indexes.get(key).map(FieldIndex::estimated_memory_bytes) else {
      self.release_key_reservations(key);
      return Ok(());
    };

    self.evict_clean_to_fit(policy.clean_max_bytes, index_bytes, Some(key));
    if policy.clean_max_bytes == 0 || index_bytes > policy.clean_max_bytes {
      self.remove_clean_index(key);
      self.dirty_reservations.remove(key);
      return Ok(());
    }
    let reservation = policy.coordinator.reserve(MemoryOwner::IndexCleanCache, index_bytes, AdmissionClass::Cache);
    match reservation {
      Ok(reservation) => {
        self.clean_reservations.insert(key.clone(), reservation);
        self.dirty_reservations.remove(key);
        Ok(())
      }
      Err(error) if Self::is_clean_admission_refusal(&error) => {
        tracing::debug!(parent = %key.parent, field = %key.field_name, strategy = %key.strategy, %error, "Clean index retention skipped after durable flush");
        self.remove_clean_index(key);
        self.dirty_reservations.remove(key);
        Ok(())
      }
      Err(error) => {
        self.remove_clean_index(key);
        self.dirty_reservations.remove(key);
        Err(Self::memory_error("clean index memory accounting failed after durable flush", error))
      }
    }
  }

  fn remove_clean_index(&mut self, key: &BufferedIndexKey) -> Option<u64> {
    let removed_bytes = self.indexes.remove(key).map(|index| index.estimated_memory_bytes());
    self.last_access.remove(key);
    self.clean_reservations.remove(key);
    removed_bytes
  }

  fn evict_clean_to_fit(&mut self, max_bytes: u64, additional_bytes: u64, excluded: Option<&BufferedIndexKey>) {
    let mut evicted = 0usize;
    while Self::reservation_bytes(&self.clean_reservations).saturating_add(additional_bytes) > max_bytes {
      let candidate = self
        .last_access
        .iter()
        .filter(|(key, _)| {
          excluded != Some(*key)
            && self.clean_reservations.contains_key(*key)
            && !self.dirty_keys.contains(*key)
            && !self.flushing_keys.contains(*key)
            && !self.deleted_keys.contains(*key)
        })
        .min_by_key(|(_, accessed)| **accessed)
        .map(|(key, _)| key.clone());
      let Some(candidate) = candidate else {
        break;
      };
      let removed_bytes = self.remove_clean_index(&candidate).unwrap_or(0);
      self.evicted_indexes = self.evicted_indexes.saturating_add(1);
      self.evicted_bytes = self.evicted_bytes.saturating_add(removed_bytes);
      evicted = evicted.saturating_add(1);
    }
    if evicted > 0 {
      self.eviction_count = self.eviction_count.saturating_add(1);
    }
  }

  fn evict_clean_indexes(&mut self, max_bytes: u64, clean_ttl: Duration) -> usize {
    if self.indexes.is_empty() {
      return 0;
    }

    let now = Instant::now();
    let mut total_bytes: u64 = self
      .indexes
      .iter()
      .filter(|(key, _)| !self.dirty_keys.contains(*key) && !self.flushing_keys.contains(*key) && !self.deleted_keys.contains(*key))
      .map(|(_, index)| index.estimated_memory_bytes())
      .sum();
    let mut to_evict: HashSet<BufferedIndexKey> = HashSet::new();

    for key in self.indexes.keys() {
      if self.dirty_keys.contains(key) || self.flushing_keys.contains(key) || self.deleted_keys.contains(key) {
        continue;
      }
      let idle_for = self.last_access.get(key).map(|last| now.saturating_duration_since(*last)).unwrap_or(clean_ttl);
      if idle_for >= clean_ttl {
        to_evict.insert(key.clone());
      }
    }

    let ttl_bytes: u64 = to_evict.iter().filter_map(|key| self.indexes.get(key)).map(|index| index.estimated_memory_bytes()).sum();
    total_bytes = total_bytes.saturating_sub(ttl_bytes);

    if max_bytes > 0 && total_bytes > max_bytes {
      let mut candidates: Vec<(BufferedIndexKey, Instant, u64)> = self
        .indexes
        .iter()
        .filter_map(|(key, index)| {
          if self.dirty_keys.contains(key) || self.flushing_keys.contains(key) || self.deleted_keys.contains(key) || to_evict.contains(key)
          {
            return None;
          }
          let last_access = self.last_access.get(key).copied().unwrap_or(now);
          Some((key.clone(), last_access, index.estimated_memory_bytes()))
        })
        .collect();

      candidates.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| right.2.cmp(&left.2)));

      for (key, _last_access, estimated_bytes) in candidates {
        if total_bytes <= max_bytes {
          break;
        }
        total_bytes = total_bytes.saturating_sub(estimated_bytes);
        to_evict.insert(key);
      }
    }

    if to_evict.is_empty() {
      return 0;
    }

    let mut evicted_bytes = 0u64;
    let mut evicted = 0usize;
    for key in to_evict {
      if let Some(bytes) = self.remove_clean_index(&key) {
        evicted_bytes = evicted_bytes.saturating_add(bytes);
        evicted += 1;
      }
    }

    if evicted > 0 {
      self.eviction_count += 1;
      self.evicted_indexes += evicted;
      self.evicted_bytes = self.evicted_bytes.saturating_add(evicted_bytes);
      tracing::debug!(
        evicted,
        evicted_bytes,
        cached_indexes = self.indexes.len(),
        remaining_estimated_bytes = self.indexes.values().map(|index| index.estimated_memory_bytes()).sum::<u64>(),
        max_bytes,
        clean_ttl_ms = clean_ttl.as_millis(),
        "Evicted clean in-memory index cache entries"
      );
    }

    evicted
  }

  pub(crate) fn emergency_snapshot_json(&mut self, hash_length: usize) -> EngineResult<serde_json::Value> {
    let mut saves = Vec::new();
    for key in &self.dirty_keys {
      if self.deleted_keys.contains(key) {
        continue;
      }
      if let Some(index) = self.indexes.get_mut(key) {
        index.ensure_nvt_current();
        let data = index.serialize(hash_length);
        saves.push(serde_json::json!({
          "parent": &key.parent,
          "field_name": &key.field_name,
          "strategy": &key.strategy,
          "bytes_base64": {
            "encoding": "base64",
            "data": base64::engine::general_purpose::STANDARD.encode(data),
          },
        }));
      }
    }

    let deletes: Vec<serde_json::Value> = self
      .deleted_keys
      .iter()
      .map(|key| {
        serde_json::json!({
          "parent": &key.parent,
          "field_name": &key.field_name,
          "strategy": &key.strategy,
        })
      })
      .collect();

    Ok(serde_json::json!({
      "format": "aeordb-index-buffer-spill-v1",
      "pending_mutations": self.pending_mutations,
      "dirty_saves": saves,
      "deletes": deletes,
    }))
  }
}

/// Compatibility handle for bulk callers.
///
/// The handle no longer owns a separate cache. It forwards updates to the
/// engine-wide shared index buffer so live writes, reindexing, delete cleanup,
/// and manual index operations all use the same mutation/flush path.
pub struct IndexWriteBuffer<'a> {
  manager: IndexManager<'a>,
  options: IndexWriteBufferOptions,
  pending_mutations: usize,
  total_mutations: usize,
  flush_count: usize,
  flushed_indexes: usize,
  last_flush: Instant,
}

impl<'a> IndexWriteBuffer<'a> {
  pub fn new(engine: &'a StorageEngine, options: IndexWriteBufferOptions) -> Self {
    Self {
      manager: IndexManager::new(engine),
      options,
      pending_mutations: 0,
      total_mutations: 0,
      flush_count: 0,
      flushed_indexes: 0,
      last_flush: Instant::now(),
    }
  }

  pub fn update_index(
    &mut self,
    parent: &str,
    field_name: &str,
    field_config: &IndexFieldConfig,
    field_values: &[Vec<u8>],
    file_key: &[u8],
  ) -> EngineResult<()> {
    self.manager.update_index_with_options(parent, field_name, field_config, field_values, file_key, Some(self.options))?;
    self.pending_mutations += 1;
    self.total_mutations += 1;
    Ok(())
  }

  pub fn should_flush(&self) -> bool {
    self.pending_mutations > 0
      && (self.pending_mutations >= self.options.flush_after_writes || self.last_flush.elapsed() >= self.options.flush_after)
  }

  pub fn flush_if_due(&mut self) -> EngineResult<bool> {
    if self.should_flush() {
      self.flush_all()?;
      Ok(true)
    } else {
      Ok(false)
    }
  }

  pub fn flush_all(&mut self) -> EngineResult<usize> {
    if self.pending_mutations == 0 {
      return Ok(0);
    }

    let flushed = self.manager.flush_buffered_indexes()?;
    self.pending_mutations = 0;
    self.flush_count += 1;
    self.flushed_indexes += flushed;
    self.last_flush = Instant::now();
    Ok(flushed)
  }

  pub fn stats(&self) -> EngineResult<IndexWriteBufferStats> {
    let manager_stats = self.manager.buffered_index_stats()?;
    Ok(IndexWriteBufferStats {
      mutations: self.total_mutations,
      flushes: self.flush_count,
      flushed_indexes: self.flushed_indexes,
      evictions: manager_stats.evictions,
      evicted_indexes: manager_stats.evicted_indexes,
      evicted_bytes: manager_stats.evicted_bytes,
      cached_indexes: manager_stats.cached_indexes,
      dirty_indexes: manager_stats.dirty_indexes,
      deleted_indexes: manager_stats.deleted_indexes,
      pending_mutations: self.pending_mutations,
      entries: manager_stats.entries,
      values: manager_stats.values,
      estimated_bytes: manager_stats.estimated_bytes,
      estimated_clean_bytes: manager_stats.estimated_clean_bytes,
      estimated_dirty_bytes: manager_stats.estimated_dirty_bytes,
      clean_reserved_bytes: manager_stats.clean_reserved_bytes,
      dirty_reserved_bytes: manager_stats.dirty_reserved_bytes,
      flush_reserved_bytes: manager_stats.flush_reserved_bytes,
      flushing_indexes: manager_stats.flushing_indexes,
      max_bytes: manager_stats.max_bytes,
      mutation_max_bytes: manager_stats.mutation_max_bytes,
      publication_batch_max_bytes: manager_stats.publication_batch_max_bytes,
      clean_ttl_ms: manager_stats.clean_ttl_ms,
      reservation_owned: manager_stats.reservation_owned,
      top_cached_indexes: manager_stats.top_cached_indexes,
    })
  }
}

impl FieldIndex {
  /// Create an empty index with the given converter.
  pub fn new(field_name: String, converter: Box<dyn ScalarConverter>) -> Self {
    let nvt_converter = deserialize_converter(&converter.serialize()).expect("converter roundtrip for NVT should never fail");
    let nvt = NormalizedVectorTable::new(nvt_converter, DEFAULT_NVT_BUCKET_COUNT);
    FieldIndex { field_name, converter, entries: Vec::new(), nvt, values: HashMap::new(), values_cover_entries: true, dirty: false }
  }

  /// Convert value to scalar and insert in sorted position. Marks NVT dirty.
  pub fn insert(&mut self, value: &[u8], file_hash: Vec<u8>) {
    self.values_cover_entries = false;
    let scalar = self.converter.to_scalar(value);
    let entry = IndexEntry { scalar, file_hash };
    let position = self
      .entries
      .binary_search_by(|probe| probe.scalar.partial_cmp(&scalar).unwrap_or(std::cmp::Ordering::Equal))
      .unwrap_or_else(|position| position);
    self.entries.insert(position, entry);
    self.dirty = true;
  }

  /// Expand a value via the converter's expand_value, then insert each expanded
  /// value as a separate index entry. For default converters this inserts one entry.
  /// For trigram/phonetic converters this inserts multiple entries.
  pub fn insert_expanded(&mut self, value: &[u8], file_hash: Vec<u8>) {
    // Store the raw value for recheck lookups
    self.values.insert(file_hash.clone(), value.to_vec());

    let expanded = self.converter.expand_value(value);
    for entry_value in expanded {
      let scalar = self.converter.to_scalar(&entry_value);
      let entry = IndexEntry { scalar, file_hash: file_hash.clone() };
      let position = self
        .entries
        .binary_search_by(|probe| probe.scalar.partial_cmp(&scalar).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or_else(|position| position);
      self.entries.insert(position, entry);
      self.dirty = true;
    }
  }

  /// Remove all entries for a given file hash. Marks NVT dirty.
  pub fn remove(&mut self, file_hash: &[u8]) {
    let had_value = self.values.remove(file_hash).is_some();
    if self.values_cover_entries && !had_value {
      return;
    }

    let original_length = self.entries.len();
    self.entries.retain(|entry| entry.file_hash != file_hash);
    if self.entries.len() != original_length {
      self.dirty = true;
    }
  }

  /// Get the raw field value for a file hash (for fuzzy query recheck).
  pub fn get_value(&self, file_hash: &[u8]) -> Option<&[u8]> {
    self.values.get(file_hash).map(|v| v.as_slice())
  }

  pub fn estimated_memory_bytes(&self) -> u64 {
    let entry_payload_bytes = self.entries.iter().fold(0usize, |total, entry| total.saturating_add(entry.file_hash.capacity()));
    let entries_bytes = self.entries.capacity().saturating_mul(size_of::<IndexEntry>()).saturating_add(entry_payload_bytes);
    let values_payload_bytes =
      self.values.iter().fold(0usize, |total, (key, value)| total.saturating_add(key.capacity()).saturating_add(value.capacity()));
    let values_bytes = self.values.capacity().saturating_mul(size_of::<(Vec<u8>, Vec<u8>)>()).saturating_add(values_payload_bytes);
    let converter_bytes = self.converter.serialize().len();
    size_of::<FieldIndex>()
      .saturating_add(self.field_name.capacity())
      .saturating_add(entries_bytes)
      .saturating_add(values_bytes)
      .saturating_add(converter_bytes)
      .saturating_add(self.nvt.estimated_memory_bytes() as usize) as u64
  }

  /// Return true when this index stores one scalar for the complete raw field
  /// value, making scalar `lookup_exact` a valid exact-match accelerator.
  pub fn supports_scalar_exact_lookup(&self) -> bool {
    !matches!(self.converter.type_tag(), CONVERTER_TYPE_TRIGRAM | CONVERTER_TYPE_PHONETIC)
  }

  /// Find file hashes whose stored raw field value exactly matches one of the
  /// requested values.
  ///
  /// This is the correctness path for exact equality on tokenizing indexes
  /// such as trigram and phonetic. Those indexes store entries for expanded
  /// tokens, so scalar lookup with the full query string is not meaningful.
  /// The raw values map preserves the original field value for exact recheck.
  pub fn lookup_stored_values_exact(&self, values: &[Vec<u8>]) -> Vec<Vec<u8>> {
    self
      .values
      .iter()
      .filter(|(_file_hash, stored_value)| values.iter().any(|value| stored_value.as_slice() == value.as_slice()))
      .map(|(file_hash, _stored_value)| file_hash.clone())
      .collect()
  }

  /// Returns true if entries have changed since the last NVT rebuild.
  pub fn is_dirty(&self) -> bool {
    self.dirty
  }

  /// Ensure the NVT reflects the current entries. Called before any lookup.
  pub fn ensure_nvt_current(&mut self) {
    if !self.dirty {
      return;
    }
    self.rebuild_nvt();
  }

  /// Rebuild the NVT from the sorted entries, assigning bucket ranges.
  pub fn rebuild_nvt(&mut self) {
    let bucket_count = self.nvt.bucket_count();

    // Reset all buckets to empty.
    for bucket_index in 0..bucket_count {
      self.nvt.update_bucket(bucket_index, 0, 0);
    }

    if self.entries.is_empty() {
      self.dirty = false;
      return;
    }

    // Walk sorted entries and assign each to its NVT bucket.
    // Track the start_index and count for each bucket.
    let mut bucket_start_indices: Vec<Option<usize>> = vec![None; bucket_count];
    let mut bucket_counts: Vec<u32> = vec![0; bucket_count];

    for (entry_index, entry) in self.entries.iter().enumerate() {
      let bucket_index = self.scalar_to_bucket(entry.scalar);
      if bucket_start_indices[bucket_index].is_none() {
        bucket_start_indices[bucket_index] = Some(entry_index);
      }
      bucket_counts[bucket_index] += 1;
    }

    for bucket_index in 0..bucket_count {
      let start = bucket_start_indices[bucket_index].unwrap_or(0) as u64;
      let count = bucket_counts[bucket_index];
      self.nvt.update_bucket(bucket_index, start, count);
    }

    self.dirty = false;
  }

  /// Find entries matching the exact value.
  /// Uses NVT for bucket-level lookup, then verifies using raw byte comparison
  /// from the values map (falling back to scalar comparison for entries without
  /// stored values). This avoids false positives when many distinct values map
  /// to the same f64 scalar (e.g. small u64 values in a [0, u64::MAX] range).
  pub fn lookup_exact(&mut self, value: &[u8]) -> Vec<&IndexEntry> {
    self.ensure_nvt_current();

    let target_scalar = self.converter.to_scalar(value);
    let bucket_index = self.nvt.bucket_for_value(value);
    let bucket = self.nvt.get_bucket(bucket_index);

    if bucket.entry_count == 0 {
      return Vec::new();
    }

    let start = bucket.kv_block_offset as usize;
    let end = (start + bucket.entry_count as usize).min(self.entries.len());

    self.entries[start..end]
      .iter()
      .filter(|entry| {
        // First check: scalar must be close (bucket-level filtering)
        if (entry.scalar - target_scalar).abs() >= f64::EPSILON {
          return false;
        }
        // Second check: if we have stored raw values, verify exact byte match.
        // This prevents false positives when distinct values map to the same
        // f64 scalar (e.g. u64 values 100 and 101 with a [0, u64::MAX] range
        // both produce scalars indistinguishable at f64 precision).
        if let Some(stored_value) = self.values.get(&entry.file_hash) {
          stored_value.as_slice() == value
        } else {
          // No stored value — fall back to scalar-only match (legacy indexes
          // without the values map, or indexes where values are not stored).
          true
        }
      })
      .collect()
  }

  /// Range query: find entries with values between min and max.
  /// Uses NVT for bucket-level lookup across the range, then verifies using
  /// raw byte comparison from the values map (falling back to scalar comparison
  /// for entries without stored values). This avoids false negatives when many
  /// distinct values map to nearly identical f64 scalars (e.g. small u64 values
  /// in a [0, u64::MAX] range where scalars are indistinguishable).
  pub fn lookup_range(&mut self, min_value: &[u8], max_value: &[u8]) -> EngineResult<Vec<&IndexEntry>> {
    if !self.converter.is_order_preserving() {
      return Err(EngineError::RangeQueryNotSupported(self.converter.name().to_string()));
    }
    self.ensure_nvt_current();

    let min_scalar = self.converter.to_scalar(min_value);
    let max_scalar = self.converter.to_scalar(max_value);
    let start_bucket = self.scalar_to_bucket(min_scalar);
    let end_bucket = self.scalar_to_bucket(max_scalar);

    // Detect if we have stored raw values for byte-level comparison.
    // When scalars are indistinguishable (e.g. small u64 with [0, u64::MAX] range),
    // the scalar-based filter would reject valid entries. In that case, use raw
    // byte comparison which preserves ordering for big-endian numeric types.
    let has_values = !self.values.is_empty();

    let mut results = Vec::new();
    for bucket_index in start_bucket..=end_bucket {
      if bucket_index >= self.nvt.bucket_count() {
        break;
      }
      let bucket = self.nvt.get_bucket(bucket_index);
      if bucket.entry_count == 0 {
        continue;
      }
      let start = bucket.kv_block_offset as usize;
      let end = (start + bucket.entry_count as usize).min(self.entries.len());
      for entry in &self.entries[start..end] {
        if has_values {
          if let Some(stored_value) = self.values.get(&entry.file_hash) {
            // Raw byte comparison: big-endian bytes preserve numeric ordering.
            if stored_value.as_slice() >= min_value && stored_value.as_slice() <= max_value {
              results.push(entry);
            }
            continue;
          }
        }
        // Fallback: scalar comparison (for entries without stored values).
        if entry.scalar >= min_scalar && entry.scalar <= max_scalar {
          results.push(entry);
        }
      }
    }

    Ok(results)
  }

  /// Greater than query. Uses NVT to skip buckets below the threshold.
  /// When raw values are available, uses byte comparison to avoid f64 precision
  /// issues (same fix as lookup_exact and lookup_range).
  pub fn lookup_gt(&mut self, value: &[u8]) -> EngineResult<Vec<&IndexEntry>> {
    if !self.converter.is_order_preserving() {
      return Err(EngineError::RangeQueryNotSupported(self.converter.name().to_string()));
    }
    self.ensure_nvt_current();

    let target_scalar = self.converter.to_scalar(value);
    let start_bucket = self.scalar_to_bucket(target_scalar);
    let has_values = !self.values.is_empty();

    let mut results = Vec::new();
    for bucket_index in start_bucket..self.nvt.bucket_count() {
      let bucket = self.nvt.get_bucket(bucket_index);
      if bucket.entry_count == 0 {
        continue;
      }
      let start = bucket.kv_block_offset as usize;
      let end = (start + bucket.entry_count as usize).min(self.entries.len());
      for entry in &self.entries[start..end] {
        if has_values {
          if let Some(stored_value) = self.values.get(&entry.file_hash) {
            if stored_value.as_slice() > value {
              results.push(entry);
            }
            continue;
          }
        }
        if entry.scalar > target_scalar {
          results.push(entry);
        }
      }
    }

    Ok(results)
  }

  /// Less than query. Uses NVT to skip buckets above the threshold.
  /// When raw values are available, uses byte comparison to avoid f64 precision
  /// issues (same fix as lookup_exact and lookup_range).
  pub fn lookup_lt(&mut self, value: &[u8]) -> EngineResult<Vec<&IndexEntry>> {
    if !self.converter.is_order_preserving() {
      return Err(EngineError::RangeQueryNotSupported(self.converter.name().to_string()));
    }
    self.ensure_nvt_current();

    let target_scalar = self.converter.to_scalar(value);
    let end_bucket = self.scalar_to_bucket(target_scalar);
    let has_values = !self.values.is_empty();

    let mut results = Vec::new();
    for bucket_index in 0..=end_bucket {
      if bucket_index >= self.nvt.bucket_count() {
        break;
      }
      let bucket = self.nvt.get_bucket(bucket_index);
      if bucket.entry_count == 0 {
        continue;
      }
      let start = bucket.kv_block_offset as usize;
      let end = (start + bucket.entry_count as usize).min(self.entries.len());
      for entry in &self.entries[start..end] {
        if has_values {
          if let Some(stored_value) = self.values.get(&entry.file_hash) {
            if stored_value.as_slice() < value {
              results.push(entry);
            }
            continue;
          }
        }
        if entry.scalar < target_scalar {
          results.push(entry);
        }
      }
    }

    Ok(results)
  }

  /// Direct scalar lookup — O(1) bucket identification, scan within bucket.
  /// Used for Tier 1 simple queries. Bypasses the NVT value conversion.
  pub fn lookup_by_scalar(&mut self, scalar: f64) -> Vec<&IndexEntry> {
    self.ensure_nvt_current();
    let bucket_index = self.nvt.bucket_for_scalar(scalar);
    let bucket = self.nvt.get_bucket(bucket_index);

    if bucket.entry_count == 0 {
      return Vec::new();
    }

    let start = bucket.kv_block_offset as usize;
    let end = (start + bucket.entry_count as usize).min(self.entries.len());

    self.entries[start..end].iter().filter(|entry| (entry.scalar - scalar).abs() < 1e-10).collect()
  }

  /// Direct scalar range — mark start/end buckets, return entries in range.
  /// Used for Tier 1 simple queries. Bypasses the NVT value conversion.
  pub fn lookup_by_scalar_range(&mut self, min_scalar: f64, max_scalar: f64) -> Vec<&IndexEntry> {
    self.ensure_nvt_current();

    let start_bucket = self.scalar_to_bucket(min_scalar);
    let end_bucket = self.scalar_to_bucket(max_scalar);

    let mut results = Vec::new();
    for bucket_index in start_bucket..=end_bucket {
      if bucket_index >= self.nvt.bucket_count() {
        break;
      }
      let bucket = self.nvt.get_bucket(bucket_index);
      if bucket.entry_count == 0 {
        continue;
      }
      let start = bucket.kv_block_offset as usize;
      let end = (start + bucket.entry_count as usize).min(self.entries.len());
      for entry in &self.entries[start..end] {
        if entry.scalar >= min_scalar && entry.scalar <= max_scalar {
          results.push(entry);
        }
      }
    }

    results
  }

  /// Return the number of entries.
  pub fn len(&self) -> usize {
    self.entries.len()
  }

  /// Check if the index is empty.
  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  /// Clone through the stable serialized representation so the boxed
  /// converter and NVT state are copied without sharing mutable query state.
  pub fn deep_clone(&self, hash_length: usize) -> EngineResult<Self> {
    FieldIndex::deserialize(&self.serialize(hash_length), hash_length)
  }

  pub fn serialized_size(&self, hash_length: usize) -> usize {
    let converter_length = serialize_converter(self.converter.as_ref()).len();
    let values_size =
      self.values.iter().fold(0usize, |total, (key, value)| total.saturating_add(key.len()).saturating_add(4).saturating_add(value.len()));
    1usize
      .saturating_add(2)
      .saturating_add(self.field_name.len())
      .saturating_add(4)
      .saturating_add(converter_length)
      .saturating_add(4)
      .saturating_add(self.nvt.serialized_size())
      .saturating_add(4)
      .saturating_add(self.entries.len().saturating_mul(8usize.saturating_add(hash_length)))
      .saturating_add(4)
      .saturating_add(values_size)
  }

  /// Current on-disk schema version for a FieldIndex blob. Bump alongside
  /// a new `deserialize_v{n}` arm when the layout changes.
  pub const SCHEMA_VERSION: u8 = 0;

  /// Serialize the index: 1-byte schema version + converter state + NVT data +
  /// entry count + sorted entries. Each entry is: f64 scalar (8 bytes LE) +
  /// file_hash (hash_length bytes).
  pub fn serialize(&self, hash_length: usize) -> Vec<u8> {
    let converter_data = serialize_converter(self.converter.as_ref());
    let nvt_data = self.nvt.serialize();
    let field_name_bytes = self.field_name.as_bytes();

    let capacity = self.serialized_size(hash_length);
    let mut buffer = Vec::with_capacity(capacity);

    // Schema version byte at offset 0. The reader checks this FIRST and
    // dispatches; everything below is the v0 layout.
    buffer.push(Self::SCHEMA_VERSION);

    // Field name
    buffer.extend_from_slice(&(field_name_bytes.len() as u16).to_le_bytes());
    buffer.extend_from_slice(field_name_bytes);

    // Converter section
    buffer.extend_from_slice(&(converter_data.len() as u32).to_le_bytes());
    buffer.extend_from_slice(&converter_data);

    // NVT section
    buffer.extend_from_slice(&(nvt_data.len() as u32).to_le_bytes());
    buffer.extend_from_slice(&nvt_data);

    // Entry count
    buffer.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());

    // Entries: scalar (f64 LE) + file_hash
    for entry in &self.entries {
      buffer.extend_from_slice(&entry.scalar.to_le_bytes());
      buffer.extend_from_slice(&entry.file_hash);
    }

    // Values map: count (u32) + each: file_hash (hash_length bytes) + value_length (u32) + value bytes
    buffer.extend_from_slice(&(self.values.len() as u32).to_le_bytes());
    for (file_hash, value) in &self.values {
      buffer.extend_from_slice(file_hash);
      buffer.extend_from_slice(&(value.len() as u32).to_le_bytes());
      buffer.extend_from_slice(value);
    }

    buffer
  }

  /// Deserialize an index from bytes. Reads the schema version byte at
  /// offset 0 and dispatches to the matching `deserialize_v{n}`.
  pub fn deserialize(data: &[u8], hash_length: usize) -> EngineResult<Self> {
    if data.is_empty() {
      return Err(EngineError::CorruptEntry { offset: 0, reason: "FieldIndex data is empty".to_string() });
    }
    let version = data[0];
    match version {
      0 => Self::deserialize_v0(&data[1..], hash_length),
      _ => Err(EngineError::InvalidEntryVersion(version)),
    }
  }

  fn deserialize_v0(data: &[u8], hash_length: usize) -> EngineResult<Self> {
    let mut cursor = 0;

    // Field name
    if data.len() < cursor + 2 {
      return Err(EngineError::CorruptEntry { offset: 0, reason: "FieldIndex data too short for field name length".to_string() });
    }
    let field_name_length = u16::from_le_bytes([data[cursor], data[cursor + 1]]) as usize;
    cursor += 2;

    if data.len() < cursor + field_name_length {
      return Err(EngineError::CorruptEntry { offset: 0, reason: "FieldIndex data too short for field name".to_string() });
    }
    let field_name = String::from_utf8(data[cursor..cursor + field_name_length].to_vec())
      .map_err(|error| EngineError::CorruptEntry { offset: cursor as u64, reason: format!("Invalid UTF-8 field name: {}", error) })?;
    cursor += field_name_length;

    // Converter section
    if data.len() < cursor + 4 {
      return Err(EngineError::CorruptEntry { offset: 0, reason: "FieldIndex data too short for converter length".to_string() });
    }
    let converter_length = u32::from_le_bytes([data[cursor], data[cursor + 1], data[cursor + 2], data[cursor + 3]]) as usize;
    cursor += 4;

    if data.len() < cursor + converter_length {
      return Err(EngineError::CorruptEntry { offset: 0, reason: "FieldIndex data too short for converter data".to_string() });
    }
    let converter = deserialize_converter(&data[cursor..cursor + converter_length])?;
    cursor += converter_length;

    // NVT section (optional for backward compatibility with old format)
    let nvt = if data.len() >= cursor + 4 {
      let nvt_length = u32::from_le_bytes([data[cursor], data[cursor + 1], data[cursor + 2], data[cursor + 3]]) as usize;

      // Heuristic: if nvt_length is unreasonably large or would exceed remaining data
      // minus what we need for entries, this is likely the old format where this u32
      // is actually the entry count. In the new format, NVT data starts with a version
      // byte (>= 1) followed by converter data, so nvt_length will be reasonable.
      // We peek at what follows: if cursor+4+nvt_length leaves room for an entry_count u32,
      // and the data at cursor+4 starts with a valid NVT version byte, it's NVT data.
      let has_nvt_section = nvt_length > 0
        && data.len() >= cursor + 4 + nvt_length + 4
        && data[cursor + 4] >= 1 // valid NVT version byte
        && data[cursor + 4] < 128; // not a huge version number

      if has_nvt_section {
        cursor += 4;
        let nvt_result = NormalizedVectorTable::deserialize(&data[cursor..cursor + nvt_length]);
        cursor += nvt_length;
        nvt_result.ok()
      } else {
        None
      }
    } else {
      None
    };

    // Entry count
    if data.len() < cursor + 4 {
      return Err(EngineError::CorruptEntry { offset: 0, reason: "FieldIndex data too short for entry count".to_string() });
    }
    let entry_count = u32::from_le_bytes([data[cursor], data[cursor + 1], data[cursor + 2], data[cursor + 3]]) as usize;
    cursor += 4;

    let entry_size = 8 + hash_length;
    if data.len() < cursor + entry_count * entry_size {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!(
          "FieldIndex data too short for {} entries (need {} bytes, have {})",
          entry_count,
          entry_count * entry_size,
          data.len() - cursor,
        ),
      });
    }

    let mut entries = Vec::with_capacity(entry_count);
    for _ in 0..entry_count {
      let scalar = f64::from_le_bytes([
        data[cursor],
        data[cursor + 1],
        data[cursor + 2],
        data[cursor + 3],
        data[cursor + 4],
        data[cursor + 5],
        data[cursor + 6],
        data[cursor + 7],
      ]);
      cursor += 8;

      let file_hash = data[cursor..cursor + hash_length].to_vec();
      cursor += hash_length;

      entries.push(IndexEntry { scalar, file_hash });
    }

    // Read values map (backward compatible — empty if no data remains)
    let mut values = HashMap::new();
    if data.len() > cursor + 4 {
      let value_count = u32::from_le_bytes([data[cursor], data[cursor + 1], data[cursor + 2], data[cursor + 3]]) as usize;
      cursor += 4;

      for _ in 0..value_count {
        if data.len() < cursor + hash_length + 4 {
          break; // truncated data, stop reading
        }
        let file_hash = data[cursor..cursor + hash_length].to_vec();
        cursor += hash_length;

        let value_length = u32::from_le_bytes([data[cursor], data[cursor + 1], data[cursor + 2], data[cursor + 3]]) as usize;
        cursor += 4;

        if data.len() < cursor + value_length {
          break;
        }
        let value = data[cursor..cursor + value_length].to_vec();
        cursor += value_length;

        values.insert(file_hash, value);
      }
    }

    // Use deserialized NVT if available (preserves bucket count), otherwise build fresh.
    let resolved_nvt = match nvt {
      Some(deserialized_nvt) => deserialized_nvt,
      None => {
        let nvt_converter = deserialize_converter(&converter.serialize()).expect("converter roundtrip for NVT should never fail");
        NormalizedVectorTable::new(nvt_converter, DEFAULT_NVT_BUCKET_COUNT)
      }
    };

    let values_cover_entries = Self::values_cover_all_entries(&entries, &values);

    // Always rebuild NVT from entries on deserialize, since the serialized NVT
    // may be stale (entries modified after last NVT rebuild before serialization).
    let mut index = FieldIndex { field_name, converter, entries, nvt: resolved_nvt, values, values_cover_entries, dirty: true };
    index.rebuild_nvt();
    Ok(index)
  }

  // --- Private helpers ---

  fn values_cover_all_entries(entries: &[IndexEntry], values: &HashMap<Vec<u8>, Vec<u8>>) -> bool {
    entries.iter().all(|entry| values.contains_key(&entry.file_hash))
  }

  /// Map a scalar in [0.0, 1.0] to a bucket index.
  fn scalar_to_bucket(&self, scalar: f64) -> usize {
    let bucket_count = self.nvt.bucket_count();
    let index = (scalar * bucket_count as f64).floor() as usize;
    index.min(bucket_count.saturating_sub(1))
  }
}

/// Manages indexes for paths in the storage engine.
pub struct IndexManager<'a> {
  engine: &'a StorageEngine,
}

impl<'a> IndexManager<'a> {
  pub fn new(engine: &'a StorageEngine) -> Self {
    IndexManager { engine }
  }

  /// Build the index file path for a field at a given path (old format, no strategy).
  fn index_file_path_legacy(path: &str, field_name: &str) -> String {
    let base = if path.ends_with('/') { path.to_string() } else { format!("{}/", path) };
    format!("{}.aeordb-indexes/{}.idx", base, field_name)
  }

  /// Build the index file path for a field at a given path with strategy.
  fn index_file_path(path: &str, field_name: &str, strategy: &str) -> String {
    let base = if path.ends_with('/') { path.to_string() } else { format!("{}/", path) };
    format!("{}.aeordb-indexes/{}.{}.idx", base, field_name, strategy)
  }

  /// Build the indexes directory path for a given path.
  fn indexes_dir_path(path: &str) -> String {
    let base = if path.ends_with('/') { path.to_string() } else { format!("{}/", path) };
    format!("{}.aeordb-indexes", base)
  }

  fn buffer_key(path: &str, field_name: &str, strategy: &str) -> BufferedIndexKey {
    BufferedIndexKey { parent: normalize_path(path), field_name: field_name.to_string(), strategy: strategy.to_string() }
  }

  fn split_index_name(index_name: &str) -> Option<(&str, &str)> {
    index_name.rsplit_once('.')
  }

  fn lock_buffer(&self) -> EngineResult<std::sync::MutexGuard<'_, SharedIndexWriteBuffer>> {
    self.engine.index_write_buffer.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))
  }

  fn lock_flush_guard(&self) -> EngineResult<std::sync::MutexGuard<'_, ()>> {
    self.engine.index_flush_guard.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))
  }

  fn grow_accounted_load(reservation: &mut MemoryReservation, bytes: u64, context: &str) -> EngineResult<()> {
    reservation.grow(bytes).map_err(|error| SharedIndexWriteBuffer::memory_error(context, error))
  }

  fn shrink_accounted_load(reservation: &mut MemoryReservation, bytes: u64, context: &str) -> EngineResult<()> {
    reservation.shrink(bytes).map_err(|error| SharedIndexWriteBuffer::memory_error(context, error))
  }

  fn buffered_index_clone_accounted(
    &self,
    key: &BufferedIndexKey,
    hash_length: usize,
    reservation: &mut MemoryReservation,
  ) -> EngineResult<Option<FieldIndex>> {
    let mut reserved_for_clone = 0u64;
    loop {
      let required = {
        let buffer = self.lock_buffer()?;
        buffer.index_clone_estimate(key)
      };
      let Some(required) = required else {
        if reserved_for_clone > 0 {
          Self::shrink_accounted_load(reservation, reserved_for_clone, "index clone accounting failed")?;
        }
        return Ok(None);
      };

      if required > reserved_for_clone {
        Self::grow_accounted_load(reservation, required - reserved_for_clone, "index clone admission failed")?;
        reserved_for_clone = required;
      }

      let mut buffer = self.lock_buffer()?;
      let Some(current_required) = buffer.index_clone_estimate(key) else {
        drop(buffer);
        Self::shrink_accounted_load(reservation, reserved_for_clone, "index clone accounting failed")?;
        return Ok(None);
      };
      if current_required > reserved_for_clone {
        continue;
      }
      return buffer.get_index_clone(key, hash_length);
    }
  }

  fn disk_index_clone_accounted(&self, index_path: &str, reservation: &mut MemoryReservation) -> EngineResult<Option<FieldIndex>> {
    let ops = DirectoryOps::new(self.engine);
    let Some(record) = ops.get_metadata(index_path)? else {
      return Ok(None);
    };
    let temporary_bytes = record
      .total_size
      .checked_mul(INDEX_LOAD_SERIALIZED_AMPLIFICATION)
      .and_then(|bytes| bytes.checked_add(INDEX_LOAD_FIXED_OVERHEAD_BYTES))
      .ok_or_else(|| EngineError::ResourceExhausted("index load memory estimate overflow".to_string()))?;
    Self::grow_accounted_load(reservation, temporary_bytes, "index disk load admission failed")?;

    let result = match ops.read_file_buffered(index_path) {
      Ok(data) => {
        let hash_length = self.engine.hash_algo().hash_length();
        Some(FieldIndex::deserialize(&data, hash_length)?)
      }
      Err(EngineError::NotFound(_)) => None,
      Err(error) => return Err(error),
    };

    let retained_bytes = result.as_ref().map_or(0, FieldIndex::estimated_memory_bytes);
    if retained_bytes > temporary_bytes {
      Self::grow_accounted_load(reservation, retained_bytes - temporary_bytes, "index retained memory admission failed")?;
    } else if retained_bytes < temporary_bytes {
      Self::shrink_accounted_load(reservation, temporary_bytes - retained_bytes, "index load accounting failed")?;
    }
    Ok(result)
  }

  fn load_index_legacy_from_disk(&self, path: &str, field_name: &str) -> EngineResult<Option<FieldIndex>> {
    let old_path = Self::index_file_path_legacy(path, field_name);
    let ops = DirectoryOps::new(self.engine);

    match ops.read_file_buffered(&old_path) {
      Ok(data) => {
        let hash_length = self.engine.hash_algo().hash_length();
        Ok(Some(FieldIndex::deserialize(&data, hash_length)?))
      }
      Err(EngineError::NotFound(_)) => Ok(None),
      Err(error) => Err(error),
    }
  }

  fn load_index_by_strategy_from_disk(&self, path: &str, field_name: &str, strategy: &str) -> EngineResult<Option<FieldIndex>> {
    let index_path = Self::index_file_path(path, field_name, strategy);
    let ops = DirectoryOps::new(self.engine);

    match ops.read_file_buffered(&index_path) {
      Ok(data) => {
        let hash_length = self.engine.hash_algo().hash_length();
        Ok(Some(FieldIndex::deserialize(&data, hash_length)?))
      }
      Err(EngineError::NotFound(_)) => Ok(None),
      Err(error) => Err(error),
    }
  }

  fn list_indexes_from_disk(&self, path: &str) -> EngineResult<Vec<String>> {
    let indexes_dir = Self::indexes_dir_path(path);
    let ops = DirectoryOps::new(self.engine);

    match ops.list_directory(&indexes_dir) {
      Ok(children) => {
        let field_names: Vec<String> = children
          .iter()
          .filter_map(|child| {
            if child.name.ends_with(".idx") {
              // New format: field.strategy.idx -> return "field.strategy"
              // Old format: field.idx -> return "field"
              Some(child.name.trim_end_matches(".idx").to_string())
            } else {
              None
            }
          })
          .collect();
        Ok(field_names)
      }
      Err(EngineError::NotFound(_)) => Ok(Vec::new()),
      Err(error) => Err(error),
    }
  }

  fn save_index_bytes_to_disk(&self, key: &BufferedIndexKey, data: &[u8]) -> EngineResult<()> {
    let index_path = Self::index_file_path(&key.parent, &key.field_name, &key.strategy);
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(self.engine);
    ops.store_file_buffered(&ctx, &index_path, data, Some("application/octet-stream"))?;
    Ok(())
  }

  fn delete_index_from_disk(&self, key: &BufferedIndexKey) -> EngineResult<()> {
    let ctx = RequestContext::system();
    let index_path = Self::index_file_path(&key.parent, &key.field_name, &key.strategy);
    let ops = DirectoryOps::new(self.engine);
    match ops.delete_file(&ctx, &index_path) {
      Ok(()) | Err(EngineError::NotFound(_)) => Ok(()),
      Err(error) => Err(error),
    }
  }

  /// Load an index for a field at the given path.
  /// Tries the old naming format ({field_name}.idx) first for backward compatibility,
  /// then scans for new-format files ({field_name}.{strategy}.idx).
  pub fn load_index(&self, path: &str, field_name: &str) -> EngineResult<Option<FieldIndex>> {
    // Try old format first: {field_name}.idx
    if let Some(index) = self.load_index_legacy_from_disk(path, field_name)? {
      return Ok(Some(index));
    }

    // Try new format: scan for {field_name}.{strategy}.idx
    let indexes = self.list_indexes(path)?;
    for index_name in &indexes {
      if index_name.starts_with(&format!("{}.", field_name)) {
        let strategy = index_name.rsplit_once('.').map(|x| x.1).unwrap_or("string");
        return self.load_index_by_strategy(path, field_name, strategy);
      }
    }

    Ok(None)
  }

  /// Load an index while charging every retained clone to an existing workload reservation.
  pub fn load_index_accounted(
    &self,
    path: &str,
    field_name: &str,
    reservation: &mut MemoryReservation,
  ) -> EngineResult<Option<FieldIndex>> {
    let legacy_path = Self::index_file_path_legacy(path, field_name);
    if let Some(index) = self.disk_index_clone_accounted(&legacy_path, reservation)? {
      return Ok(Some(index));
    }

    let indexes = self.list_indexes(path)?;
    for index_name in &indexes {
      if index_name.starts_with(&format!("{}.", field_name)) {
        let strategy = index_name.rsplit_once('.').map(|pair| pair.1).unwrap_or("string");
        return self.load_index_by_strategy_accounted(path, field_name, strategy, reservation);
      }
    }
    Ok(None)
  }

  /// Load an index by field name and strategy.
  pub fn load_index_by_strategy(&self, path: &str, field_name: &str, strategy: &str) -> EngineResult<Option<FieldIndex>> {
    let key = Self::buffer_key(path, field_name, strategy);
    let hash_length = self.engine.hash_algo().hash_length();
    {
      let mut buffer = self.lock_buffer()?;
      if let Some(index) = buffer.get_index_clone(&key, hash_length)? {
        return Ok(Some(index));
      }
      if buffer.deleted_keys.contains(&key) {
        return Ok(None);
      }
    }
    self.load_index_by_strategy_from_disk(&key.parent, field_name, strategy)
  }

  /// Load a strategy index while reserving before either an in-memory deep clone
  /// or the buffered read/deserialization amplification of an on-disk index.
  pub fn load_index_by_strategy_accounted(
    &self,
    path: &str,
    field_name: &str,
    strategy: &str,
    reservation: &mut MemoryReservation,
  ) -> EngineResult<Option<FieldIndex>> {
    let key = Self::buffer_key(path, field_name, strategy);
    let hash_length = self.engine.hash_algo().hash_length();
    if let Some(index) = self.buffered_index_clone_accounted(&key, hash_length, reservation)? {
      return Ok(Some(index));
    }
    {
      let buffer = self.lock_buffer()?;
      if buffer.deleted_keys.contains(&key) {
        return Ok(None);
      }
    }
    let index_path = Self::index_file_path(&key.parent, field_name, strategy);
    self.disk_index_clone_accounted(&index_path, reservation)
  }

  /// Buffer an index save to `.indexes/{field_name}.{strategy}.idx` at the given path.
  pub fn save_index(&self, path: &str, index: &FieldIndex) -> EngineResult<()> {
    self.engine.ensure_writable()?;
    let strategy = index.converter.strategy();
    let key = Self::buffer_key(path, &index.field_name, strategy);
    let hash_length = self.engine.hash_algo().hash_length();
    let reservation_bytes = index.estimated_memory_bytes().max(index.serialized_size(hash_length) as u64);
    let prepared = {
      let buffer = self.lock_buffer()?;
      buffer.prepare_dirty_reservation(&key, reservation_bytes)?
    };
    let index_clone = index.deep_clone(hash_length)?;
    {
      let mut buffer = self.lock_buffer()?;
      buffer.put_index(key, index_clone, prepared)?;
    }
    self.flush_buffered_indexes_if_due_with_options(None).map(|_| ())
  }

  /// List index names at this path.
  /// Returns names in the format "field.strategy" (new format) or "field" (old format).
  pub fn list_indexes(&self, path: &str) -> EngineResult<Vec<String>> {
    let parent = normalize_path(path);
    let mut names = self.list_indexes_from_disk(&parent)?;
    {
      let buffer = self.lock_buffer()?;
      names.extend(buffer.list_index_names(&parent));
      names.retain(|name| {
        if let Some((field_name, strategy)) = Self::split_index_name(name) {
          let key = Self::buffer_key(&parent, field_name, strategy);
          !buffer.deleted_keys.contains(&key)
        } else {
          true
        }
      });
    }
    names.sort();
    names.dedup();
    Ok(names)
  }

  /// Delete an index for a field and strategy at the given path.
  pub fn delete_index(&self, path: &str, field_name: &str, strategy: &str) -> EngineResult<()> {
    self.engine.ensure_writable()?;
    let key = Self::buffer_key(path, field_name, strategy);
    let exists_in_buffer = {
      let buffer = self.lock_buffer()?;
      buffer.indexes.contains_key(&key) && !buffer.deleted_keys.contains(&key)
    };
    if !exists_in_buffer && self.load_index_by_strategy_from_disk(&key.parent, field_name, strategy)?.is_none() {
      return Err(EngineError::NotFound(Self::index_file_path(&key.parent, field_name, strategy)));
    }
    {
      let mut buffer = self.lock_buffer()?;
      buffer.delete_index(key)?;
    }
    self.flush_buffered_indexes_if_due_with_options(None).map(|_| ())
  }

  /// Delete an index using the legacy path format (no strategy).
  pub fn delete_index_legacy(&self, path: &str, field_name: &str) -> EngineResult<()> {
    self.engine.ensure_writable()?;
    let ctx = RequestContext::system();
    let index_path = Self::index_file_path_legacy(path, field_name);
    let ops = DirectoryOps::new(self.engine);
    ops.delete_file(&ctx, &index_path)
  }

  /// Delete field/strategy indexes at `path` that are not present in the
  /// current index config. This retires indexes removed by config changes so a
  /// reindex does not keep carrying obsolete cached or on-disk index files.
  pub fn delete_indexes_not_in_config(&self, path: &str, config: &PathIndexConfig) -> EngineResult<usize> {
    let parent = normalize_path(path);
    let mut desired = HashSet::new();

    for field_config in &config.indexes {
      let converter = create_converter_from_config(field_config)?;
      desired.insert(format!("{}.{}", field_config.name, converter.strategy()));
    }

    let existing = self.list_indexes(&parent)?;
    let mut deleted = 0usize;

    for index_name in existing {
      if desired.contains(&index_name) {
        continue;
      }

      let delete_result = if let Some((field_name, strategy)) = Self::split_index_name(&index_name) {
        self.delete_index(&parent, field_name, strategy)
      } else {
        self.delete_index_legacy(&parent, &index_name)
      };

      match delete_result {
        Ok(()) => deleted += 1,
        Err(EngineError::NotFound(_)) => {}
        Err(error) => return Err(error),
      }
    }

    if deleted > 0 {
      self.flush_buffered_indexes()?;
    }

    Ok(deleted)
  }

  /// Create an empty index for a field at the given path.
  pub fn create_index(&self, path: &str, field_name: &str, converter: Box<dyn ScalarConverter>) -> EngineResult<FieldIndex> {
    let index = FieldIndex::new(field_name.to_string(), converter);
    self.save_index(path, &index)?;
    Ok(index)
  }

  /// Update one field/strategy index through the shared buffered write path.
  pub fn update_index(
    &self,
    parent: &str,
    field_name: &str,
    field_config: &IndexFieldConfig,
    field_values: &[Vec<u8>],
    file_key: &[u8],
  ) -> EngineResult<()> {
    self.update_index_with_options(parent, field_name, field_config, field_values, file_key, None)
  }

  pub(crate) fn update_index_with_options(
    &self,
    parent: &str,
    field_name: &str,
    field_config: &IndexFieldConfig,
    field_values: &[Vec<u8>],
    file_key: &[u8],
    options: Option<IndexWriteBufferOptions>,
  ) -> EngineResult<()> {
    self.engine.ensure_writable()?;
    let converter = create_converter_from_config(field_config)?;
    let strategy = converter.strategy().to_string();
    let key = Self::buffer_key(parent, field_name, &strategy);

    let needs_load = {
      let buffer = self.lock_buffer()?;
      !buffer.indexes.contains_key(&key)
    };
    let initial_index = if needs_load { self.load_index_by_strategy_from_disk(&key.parent, field_name, &strategy)? } else { None };

    {
      let mut buffer = self.lock_buffer()?;
      let create_index = || {
        let converter = create_converter_from_config(field_config)?;
        Ok(FieldIndex::new(field_name.to_string(), converter))
      };
      buffer.update_index(key, initial_index, create_index, field_values, file_key)?;
    }

    self.flush_buffered_indexes_if_due_with_options(options).map(|_| ())
  }

  /// Remove a file hash from an index name returned by `list_indexes`, using
  /// the same buffered mutation path as inserts and saves.
  pub fn remove_file_from_index_name(&self, parent: &str, index_name: &str, file_key: &[u8]) -> EngineResult<()> {
    self.engine.ensure_writable()?;
    if let Some((field_name, strategy)) = Self::split_index_name(index_name) {
      self.remove_file_from_index(parent, field_name, strategy, file_key)
    } else {
      let Some(index) = self.load_index(parent, index_name)? else {
        return Ok(());
      };
      let strategy = index.converter.strategy().to_string();
      let key = Self::buffer_key(parent, &index.field_name, &strategy);
      {
        let mut buffer = self.lock_buffer()?;
        buffer.remove_file_from_index(key, Some(index), file_key)?;
      }
      self.flush_buffered_indexes_if_due_with_options(None).map(|_| ())
    }
  }

  pub fn remove_file_from_index(&self, parent: &str, field_name: &str, strategy: &str, file_key: &[u8]) -> EngineResult<()> {
    self.engine.ensure_writable()?;
    let key = Self::buffer_key(parent, field_name, strategy);
    let needs_load = {
      let buffer = self.lock_buffer()?;
      !buffer.indexes.contains_key(&key) && !buffer.deleted_keys.contains(&key)
    };
    let initial_index = if needs_load { self.load_index_by_strategy_from_disk(&key.parent, field_name, strategy)? } else { None };
    {
      let mut buffer = self.lock_buffer()?;
      buffer.remove_file_from_index(key, initial_index, file_key)?;
    }
    self.flush_buffered_indexes_if_due_with_options(None).map(|_| ())
  }

  pub fn buffered_index_stats(&self) -> EngineResult<IndexWriteBufferStats> {
    Ok(self.lock_buffer()?.stats())
  }

  pub fn evict_clean_indexes(&self) -> EngineResult<usize> {
    let mut buffer = self.lock_buffer()?;
    let (max_bytes, clean_ttl) = buffer
      .memory_policy
      .as_ref()
      .map_or((DEFAULT_INDEX_CACHE_MAX_BYTES, DEFAULT_INDEX_CACHE_CLEAN_TTL), |policy| (policy.clean_max_bytes, policy.clean_ttl));
    Ok(buffer.evict_clean_indexes(max_bytes, clean_ttl))
  }

  pub fn evict_clean_indexes_with_policy(&self, max_bytes: u64, clean_ttl: Duration) -> EngineResult<usize> {
    Ok(self.lock_buffer()?.evict_clean_indexes(max_bytes, clean_ttl))
  }

  pub fn flush_buffered_indexes_if_due(&self) -> EngineResult<bool> {
    self.flush_buffered_indexes_if_due_with_options(None)
  }

  pub(crate) fn flush_buffered_indexes_if_due_with_options(&self, options: Option<IndexWriteBufferOptions>) -> EngineResult<bool> {
    self.engine.ensure_writable()?;
    let _flush_guard = self.lock_flush_guard()?;
    match self.take_flush_snapshot(false, options)? {
      Some(snapshot) => {
        self.write_flush_snapshot(snapshot)?;
        Ok(true)
      }
      None => Ok(false),
    }
  }

  pub fn flush_buffered_indexes(&self) -> EngineResult<usize> {
    self.engine.ensure_writable()?;
    let _flush_guard = self.lock_flush_guard()?;
    let mut target_keys = {
      let buffer = self.lock_buffer()?;
      buffer.pending_by_key.keys().cloned().collect::<HashSet<_>>()
    };
    let mut flushed_indexes = 0usize;
    while !target_keys.is_empty() {
      let Some(snapshot) = self.take_flush_snapshot(true, None)? else {
        break;
      };
      let completed_keys = snapshot.mutation_counts.iter().map(|(key, _)| key.clone()).collect::<Vec<_>>();
      flushed_indexes = flushed_indexes.saturating_add(self.write_flush_snapshot(snapshot)?);
      for key in completed_keys {
        target_keys.remove(&key);
      }
    }
    Ok(flushed_indexes)
  }

  fn take_flush_snapshot(&self, force: bool, options: Option<IndexWriteBufferOptions>) -> EngineResult<Option<IndexFlushSnapshot>> {
    let hash_length = self.engine.hash_algo().hash_length();
    let snapshot = {
      let mut buffer = self.lock_buffer()?;
      if force && buffer.pending_mutations == 0 && buffer.dirty_keys.is_empty() && buffer.deleted_keys.is_empty() {
        return Ok(None);
      }
      if !force && !buffer.should_flush(options) {
        return Ok(None);
      }
      let effective_options = buffer.effective_options(options);
      tracing::debug!(
        force,
        pending_mutations = buffer.pending_mutations,
        dirty_indexes = buffer.dirty_keys.len(),
        deleted_indexes = buffer.deleted_keys.len(),
        cached_indexes = buffer.indexes.len(),
        elapsed_ms = buffer.last_flush.elapsed().as_millis(),
        flush_after_writes = effective_options.flush_after_writes,
        flush_after_ms = effective_options.flush_after.as_millis(),
        "Taking buffered index flush snapshot"
      );
      buffer.snapshot_flush(hash_length)?
    };
    Ok(Some(snapshot))
  }

  fn write_flush_snapshot(&self, snapshot: IndexFlushSnapshot) -> EngineResult<usize> {
    let mut result = Ok(());
    for (key, data) in &snapshot.saves {
      if let Err(error) = self.save_index_bytes_to_disk(key, data) {
        result = Err(error);
        break;
      }
    }
    if result.is_ok() {
      for key in &snapshot.deletes {
        if let Err(error) = self.delete_index_from_disk(key) {
          result = Err(error);
          break;
        }
      }
    }

    match result {
      Ok(()) => {
        let flushed = snapshot.saves.len();
        let finish_result = {
          let mut buffer = self.lock_buffer()?;
          buffer.finish_successful_flush(&snapshot)
        };
        let eviction_result = self.evict_clean_indexes();
        finish_result?;
        eviction_result?;
        Ok(flushed)
      }
      Err(error) => match self.lock_buffer() {
        Ok(mut buffer) => {
          buffer.restore_failed_flush(&snapshot);
          Err(error)
        }
        Err(restore_error) => Err(EngineError::IoError(std::io::Error::other(format!(
          "index publication failed ({error}) and its dirty generation could not be restored ({restore_error})"
        )))),
      },
    }
  }

  /// Discover all directories that contain indexes under `base_path`.
  ///
  /// Scans `base_path` recursively for files whose path includes
  /// `/.aeordb-indexes/`, extracts the parent directory of each
  /// `.aeordb-indexes` segment, and returns a deduplicated, sorted list.
  pub fn discover_indexed_directories(&self, base_path: &str) -> EngineResult<Vec<String>> {
    use std::collections::BTreeSet;
    use crate::engine::directory_listing::list_directory_recursive;

    let mut indexed_dirs = BTreeSet::new();

    // Check base_path itself: if list_indexes returns any results, include it.
    let base_indexes = self.list_indexes(base_path)?;
    if !base_indexes.is_empty() {
      let normalized = crate::engine::path_utils::normalize_path(base_path);
      indexed_dirs.insert(normalized);
    }

    // Recursively list all files. Files inside .aeordb-indexes directories have
    // paths like `/some/dir/.aeordb-indexes/field.trigram.idx`. We extract
    // `/some/dir` from those paths. If the recursive walk fails (e.g., malformed
    // directory entry after KV expansion), log the error and return whatever we
    // found from the base path scan — partial results are better than a total failure.
    match list_directory_recursive(self.engine, base_path, -1, None, None) {
      Ok(entries) => {
        for entry in &entries {
          if let Some(idx_pos) = entry.path.find("/.aeordb-indexes/") {
            let parent = &entry.path[..idx_pos];
            let dir = if parent.is_empty() { "/" } else { parent };
            indexed_dirs.insert(dir.to_string());
          }
        }
      }
      Err(e) => {
        tracing::warn!(base_path, "discover_indexed_directories: recursive scan failed ({}). Returning base-path results only.", e,);
      }
    }

    if let Ok(buffer) = self.lock_buffer() {
      for parent in buffer.indexed_parents() {
        let normalized_base = crate::engine::path_utils::normalize_path(base_path);
        if parent == normalized_base || parent.starts_with(&format!("{}/", normalized_base.trim_end_matches('/'))) {
          indexed_dirs.insert(parent);
        }
      }
    }

    Ok(indexed_dirs.into_iter().collect())
  }

  /// Load all indexes for a field across all strategies.
  pub fn load_indexes_for_field(&self, path: &str, field_name: &str) -> EngineResult<Vec<FieldIndex>> {
    let indexes = self.list_indexes(path)?;
    let mut result = Vec::new();

    for index_name in &indexes {
      // index_name is either "field" (old) or "field.strategy" (new)
      let is_match = index_name == field_name || index_name.starts_with(&format!("{}.", field_name));

      if is_match {
        // Determine strategy from the name
        let strategy = if index_name.contains('.') {
          index_name.split_once('.').map(|x| x.1).unwrap_or("string")
        } else {
          "string" // old format
        };

        if let Some(idx) = self.load_index_by_strategy(path, field_name, strategy)? {
          result.push(idx);
        } else if strategy == "string" {
          // Try old format (no strategy in filename)
          if let Some(idx) = self.load_index(path, field_name)? {
            result.push(idx);
          }
        }
      }
    }

    Ok(result)
  }

  /// Load all strategies for one field while charging each materialized index
  /// to an existing workload reservation before allocation.
  pub fn load_indexes_for_field_accounted(
    &self,
    path: &str,
    field_name: &str,
    reservation: &mut MemoryReservation,
  ) -> EngineResult<Vec<FieldIndex>> {
    let indexes = self.list_indexes(path)?;
    let mut result = Vec::new();

    for index_name in &indexes {
      let is_match = index_name == field_name || index_name.starts_with(&format!("{}.", field_name));
      if !is_match {
        continue;
      }
      let strategy = if index_name.contains('.') { index_name.split_once('.').map(|pair| pair.1).unwrap_or("string") } else { "string" };
      if let Some(index) = self.load_index_by_strategy_accounted(path, field_name, strategy, reservation)? {
        result.push(index);
      } else if strategy == "string" {
        if let Some(index) = self.load_index_accounted(path, field_name, reservation)? {
          result.push(index);
        }
      }
    }
    Ok(result)
  }
}
