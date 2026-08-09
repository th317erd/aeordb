use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};

use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::kv_pages::{bucket_page_offset, live_type_counts_in_page, page_size};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};
use crate::engine::native_durability::read_exact_at_platform;

struct CachedPage {
  data: Arc<[u8]>,
  last_access: u64,
  _reservation: MemoryReservation,
}

struct HistoricalPage {
  data: Arc<[u8]>,
  _reservation: MemoryReservation,
}

struct PendingPage {
  old_generation: u64,
  data: Arc<[u8]>,
  reservation: MemoryReservation,
}

struct PendingUpdate {
  generation: u64,
  pages: HashMap<usize, PendingPage>,
  overwrite_started: bool,
}

#[derive(Default)]
struct PageCacheState {
  pages: HashMap<usize, CachedPage>,
  loading: HashSet<usize>,
  access_clock: u64,
  resident_bytes: u64,
  hits: u64,
  misses: u64,
  disk_reads: u64,
  evictions: u64,
  read_failures: u64,
  cache_deferrals: u64,
  committed_generation: u64,
  bucket_generations: HashMap<usize, u64>,
  active_generations: BTreeMap<u64, u64>,
  historical: HashMap<usize, BTreeMap<u64, HistoricalPage>>,
  pending: Option<PendingUpdate>,
  preparing_update: bool,
  poisoned_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KvPageProviderStats {
  pub resident_pages: u64,
  pub resident_bytes: u64,
  pub max_resident_bytes: u64,
  pub hits: u64,
  pub misses: u64,
  pub disk_reads: u64,
  pub evictions: u64,
  pub read_failures: u64,
  pub cache_deferrals: u64,
  pub historical_pages: u64,
  pub historical_bytes: u64,
  pub pending_pages: u64,
  pub pending_bytes: u64,
  pub committed_generation: u64,
  pub active_snapshots: u64,
}

struct KvPageProviderInner {
  file: File,
  kv_block_offset: u64,
  hash_length: usize,
  bucket_count: usize,
  page_size: usize,
  max_resident_bytes: u64,
  coordinator: Option<MemoryCoordinator>,
  state: Mutex<PageCacheState>,
  loaded: Condvar,
}

#[derive(Clone)]
pub struct KvPageProvider {
  inner: Arc<KvPageProviderInner>,
}

struct SnapshotLease {
  provider: Weak<KvPageProviderInner>,
  generation: u64,
}

impl Drop for SnapshotLease {
  fn drop(&mut self) {
    let Some(provider) = self.provider.upgrade() else {
      return;
    };
    let Ok(mut state) = provider.state.lock() else {
      return;
    };
    if let Some(active) = state.active_generations.get_mut(&self.generation) {
      *active = active.saturating_sub(1);
      if *active == 0 {
        state.active_generations.remove(&self.generation);
      }
    }
    prune_historical_pages(&mut state);
    provider.loaded.notify_all();
  }
}

#[derive(Clone)]
pub struct KvPageSnapshot {
  provider: KvPageProvider,
  lease: Arc<SnapshotLease>,
}

impl KvPageSnapshot {
  pub fn generation(&self) -> u64 {
    self.lease.generation
  }

  pub fn read_page(&self, bucket: usize) -> EngineResult<Arc<[u8]>> {
    self.provider.read_page_at(self.generation(), bucket)
  }
}

pub struct KvPageUpdate {
  provider: KvPageProvider,
  generation: u64,
  overwrite_started: bool,
  completed: bool,
}

impl KvPageProvider {
  pub fn new(
    file: File,
    kv_block_offset: u64,
    hash_algorithm: HashAlgorithm,
    bucket_count: usize,
    max_resident_bytes: u64,
    coordinator: Option<MemoryCoordinator>,
  ) -> EngineResult<Self> {
    if bucket_count == 0 {
      return Err(EngineError::InvalidInput("KV page provider requires at least one bucket".to_string()));
    }
    let hash_length = hash_algorithm.hash_length();
    let page_size = page_size(hash_length);
    let page_bytes = u64::try_from(page_size).map_err(|_| EngineError::InvalidInput("KV page size exceeds u64".to_string()))?;
    let pages_bytes = u64::try_from(bucket_count)
      .ok()
      .and_then(|count| count.checked_mul(page_bytes))
      .ok_or_else(|| EngineError::InvalidInput("KV page layout size overflows u64".to_string()))?;
    kv_block_offset.checked_add(pages_bytes).ok_or_else(|| EngineError::InvalidInput("KV page layout end overflows u64".to_string()))?;
    Ok(Self {
      inner: Arc::new(KvPageProviderInner {
        file,
        kv_block_offset,
        hash_length,
        bucket_count,
        page_size,
        max_resident_bytes,
        coordinator,
        state: Mutex::new(PageCacheState::default()),
        loaded: Condvar::new(),
      }),
    })
  }

  fn lock(&self) -> EngineResult<MutexGuard<'_, PageCacheState>> {
    self.inner.state.lock().map_err(|error| EngineError::IoError(std::io::Error::other(format!("KV page cache lock poisoned: {error}"))))
  }

  fn page_offset(&self, bucket: usize) -> EngineResult<u64> {
    if bucket >= self.inner.bucket_count {
      return Err(EngineError::InvalidInput(format!(
        "KV bucket {bucket} is outside the page-provider layout of {} buckets",
        self.inner.bucket_count
      )));
    }
    self
      .inner
      .kv_block_offset
      .checked_add(bucket_page_offset(bucket, self.inner.hash_length))
      .ok_or_else(|| EngineError::InvalidInput(format!("KV bucket {bucket} offset overflows u64")))
  }

  pub fn snapshot(&self) -> EngineResult<KvPageSnapshot> {
    let generation = {
      let mut state = self.lock()?;
      if let Some(reason) = &state.poisoned_reason {
        return Err(EngineError::DurabilityFailure(format!("KV page publication is poisoned: {reason}")));
      }
      let generation = state.committed_generation;
      let active = state.active_generations.entry(generation).or_default();
      *active = active
        .checked_add(1)
        .ok_or_else(|| EngineError::DurabilityFailure(format!("KV snapshot count overflow at generation {generation}")))?;
      generation
    };
    Ok(KvPageSnapshot { provider: self.clone(), lease: Arc::new(SnapshotLease { provider: Arc::downgrade(&self.inner), generation }) })
  }

  pub fn read_page(&self, bucket: usize) -> EngineResult<Arc<[u8]>> {
    let generation = {
      let state = self.lock()?;
      if let Some(reason) = &state.poisoned_reason {
        return Err(EngineError::DurabilityFailure(format!("KV page publication is poisoned: {reason}")));
      }
      state.committed_generation
    };
    self.read_page_at(generation, bucket)
  }

  fn read_page_at(&self, generation: u64, bucket: usize) -> EngineResult<Arc<[u8]>> {
    let offset = match self.page_offset(bucket) {
      Ok(offset) => offset,
      Err(error) => {
        let mut state = self.lock()?;
        state.read_failures = state.read_failures.saturating_add(1);
        return Err(error);
      }
    };

    loop {
      let mut state = self.lock()?;

      if let Some(pending) = state.pending.as_ref().and_then(|update| update.pages.get(&bucket)) {
        if generation < state.pending.as_ref().expect("pending update exists").generation {
          let data = Arc::clone(&pending.data);
          state.hits = state.hits.saturating_add(1);
          return Ok(data);
        }
      }

      let current_generation = state.bucket_generations.get(&bucket).copied().unwrap_or(0);
      if generation < current_generation {
        let data = state
          .historical
          .get(&bucket)
          .and_then(|versions| versions.range(..=generation).next_back())
          .map(|(_, page)| Arc::clone(&page.data))
          .ok_or_else(|| {
            EngineError::DurabilityFailure(format!(
              "KV page {bucket} has no retained bytes for snapshot generation {generation}; current generation is {current_generation}"
            ))
          })?;
        state.hits = state.hits.saturating_add(1);
        return Ok(data);
      }

      state.access_clock = state.access_clock.saturating_add(1);
      let access_clock = state.access_clock;
      if let Some(page) = state.pages.get_mut(&bucket) {
        page.last_access = access_clock;
        let data = Arc::clone(&page.data);
        state.hits = state.hits.saturating_add(1);
        return Ok(data);
      }
      if state.loading.contains(&bucket) {
        state = self
          .inner
          .loaded
          .wait(state)
          .map_err(|error| EngineError::IoError(std::io::Error::other(format!("KV page cache wait lock poisoned: {error}"))))?;
        drop(state);
        continue;
      }
      state.loading.insert(bucket);
      state.misses = state.misses.saturating_add(1);
      break;
    }

    let mut bytes = vec![0u8; self.inner.page_size];
    let read_result = read_exact_at_platform(&self.inner.file, offset, &mut bytes)
      .map_err(EngineError::from)
      .and_then(|()| live_type_counts_in_page(&bytes, self.inner.hash_length).map(|_| ()))
      .map(|()| Arc::<[u8]>::from(bytes.into_boxed_slice()))
      .map_err(|error| match error {
        EngineError::CorruptEntry { reason, .. } => EngineError::CorruptEntry { offset, reason },
        other => other,
      });

    let mut state = self.lock()?;
    state.disk_reads = state.disk_reads.saturating_add(1);
    let data = match read_result {
      Ok(data) => data,
      Err(error) => {
        state.loading.remove(&bucket);
        state.read_failures = state.read_failures.saturating_add(1);
        self.inner.loaded.notify_all();
        return Err(error);
      }
    };
    let page_bytes = data.len() as u64;
    drop(state);
    let reservation = if self.inner.max_resident_bytes >= page_bytes {
      self
        .inner
        .coordinator
        .as_ref()
        .and_then(|coordinator| coordinator.reserve(MemoryOwner::KvResidentPages, page_bytes, AdmissionClass::Cache).ok())
    } else {
      None
    };

    let mut state = self.lock()?;
    if let Some(reservation) = reservation {
      while state.resident_bytes.saturating_add(page_bytes) > self.inner.max_resident_bytes {
        if !evict_oldest_page(&mut state) {
          break;
        }
      }
      remove_cached_page(&mut state, bucket);
      state.access_clock = state.access_clock.saturating_add(1);
      let last_access = state.access_clock;
      state.resident_bytes = state.resident_bytes.saturating_add(page_bytes);
      state.pages.insert(bucket, CachedPage { data: Arc::clone(&data), last_access, _reservation: reservation });
    } else {
      state.cache_deferrals = state.cache_deferrals.saturating_add(1);
    }
    state.loading.remove(&bucket);
    self.inner.loaded.notify_all();
    Ok(data)
  }

  pub fn begin_update(&self, buckets: &[usize]) -> EngineResult<KvPageUpdate> {
    self.begin_update_with_admission(buckets, MemoryOwner::KvSnapshotGenerations, AdmissionClass::Workload)
  }

  pub(crate) fn begin_update_with_admission(
    &self,
    buckets: &[usize],
    owner: MemoryOwner,
    admission: AdmissionClass,
  ) -> EngineResult<KvPageUpdate> {
    if buckets.is_empty() {
      return Err(EngineError::InvalidInput("KV page update requires at least one bucket".to_string()));
    }
    let mut unique = HashSet::with_capacity(buckets.len());
    for &bucket in buckets {
      self.page_offset(bucket)?;
      if !unique.insert(bucket) {
        return Err(EngineError::InvalidInput(format!("KV page update repeats bucket {bucket}")));
      }
    }

    let old_generation = {
      let mut state = self.lock()?;
      if let Some(reason) = &state.poisoned_reason {
        return Err(EngineError::DurabilityFailure(format!("KV page publication is poisoned: {reason}")));
      }
      if state.preparing_update || state.pending.is_some() {
        return Err(EngineError::InvalidInput("another KV page update is already in progress".to_string()));
      }
      state.preparing_update = true;
      state.committed_generation
    };

    let preparation = (|| {
      let coordinator = self
        .inner
        .coordinator
        .as_ref()
        .ok_or_else(|| EngineError::DurabilityFailure("KV page updates require an active memory coordinator".to_string()))?;
      let mut pages = HashMap::with_capacity(buckets.len());
      for &bucket in buckets {
        let data = self.read_page_at(old_generation, bucket)?;
        {
          let mut state = self.lock()?;
          remove_cached_page(&mut state, bucket);
        }
        let reservation = coordinator
          .reserve(owner, data.len() as u64, admission)
          .map_err(|error| EngineError::DurabilityFailure(format!("cannot preserve KV page {bucket} before overwrite: {error}")))?;
        let bucket_generation = self.lock()?.bucket_generations.get(&bucket).copied().unwrap_or(0);
        pages.insert(bucket, PendingPage { old_generation: bucket_generation, data, reservation });
      }
      let generation =
        old_generation.checked_add(1).ok_or_else(|| EngineError::DurabilityFailure("KV page generation overflow".to_string()))?;
      Ok((generation, pages))
    })();

    let (generation, pages) = match preparation {
      Ok(prepared) => prepared,
      Err(error) => {
        let mut state = self.lock()?;
        state.preparing_update = false;
        return Err(error);
      }
    };

    let mut state = self.lock()?;
    if state.committed_generation != old_generation || state.pending.is_some() || state.poisoned_reason.is_some() {
      state.preparing_update = false;
      return Err(EngineError::DurabilityFailure("KV page generation changed while preparing an update".to_string()));
    }
    for &bucket in buckets {
      remove_cached_page(&mut state, bucket);
    }
    state.pending = Some(PendingUpdate { generation, pages, overwrite_started: false });
    Ok(KvPageUpdate { provider: self.clone(), generation, overwrite_started: false, completed: false })
  }

  pub fn is_poisoned(&self) -> EngineResult<bool> {
    Ok(self.lock()?.poisoned_reason.is_some())
  }

  /// Wait until every snapshot lease from this provider has been released.
  /// The caller must first remove the provider-backed view from its publication
  /// authority so no new lease can become visible while draining.
  pub fn wait_for_no_snapshots(&self, timeout: std::time::Duration) -> EngineResult<bool> {
    let deadline = std::time::Instant::now() + timeout;
    let mut state = self.lock()?;
    loop {
      let active: u64 = state.active_generations.values().copied().sum();
      if active == 0 {
        return Ok(true);
      }
      let now = std::time::Instant::now();
      if now >= deadline {
        return Ok(false);
      }
      let remaining = deadline.saturating_duration_since(now);
      let (next, result) = self
        .inner
        .loaded
        .wait_timeout(state, remaining)
        .map_err(|error| EngineError::IoError(std::io::Error::other(format!("KV snapshot drain lock poisoned: {error}"))))?;
      state = next;
      if result.timed_out() {
        return Ok(state.active_generations.values().copied().sum::<u64>() == 0);
      }
    }
  }

  pub fn stats(&self) -> EngineResult<KvPageProviderStats> {
    let state = self.lock()?;
    let historical_pages = state.historical.values().map(BTreeMap::len).sum::<usize>() as u64;
    let historical_bytes = state.historical.values().flat_map(BTreeMap::values).map(|page| page.data.len() as u64).sum();
    let (pending_pages, pending_bytes) = state
      .pending
      .as_ref()
      .map(|pending| (pending.pages.len() as u64, pending.pages.values().map(|page| page.data.len() as u64).sum()))
      .unwrap_or((0, 0));
    Ok(KvPageProviderStats {
      resident_pages: state.pages.len() as u64,
      resident_bytes: state.resident_bytes,
      max_resident_bytes: self.inner.max_resident_bytes,
      hits: state.hits,
      misses: state.misses,
      disk_reads: state.disk_reads,
      evictions: state.evictions,
      read_failures: state.read_failures,
      cache_deferrals: state.cache_deferrals,
      historical_pages,
      historical_bytes,
      pending_pages,
      pending_bytes,
      committed_generation: state.committed_generation,
      active_snapshots: state.active_generations.values().copied().sum(),
    })
  }
}

impl KvPageUpdate {
  pub const fn generation(&self) -> u64 {
    self.generation
  }

  pub fn mark_overwrite_started(&mut self) -> EngineResult<()> {
    let mut state = self.provider.lock()?;
    let pending = state.pending.as_mut().ok_or_else(|| EngineError::DurabilityFailure("KV page update state is missing".to_string()))?;
    if pending.generation != self.generation {
      return Err(EngineError::DurabilityFailure("KV page update generation does not match pending state".to_string()));
    }
    pending.overwrite_started = true;
    self.overwrite_started = true;
    Ok(())
  }

  pub fn commit(mut self, replacements: Vec<(usize, Arc<[u8]>)>) -> EngineResult<()> {
    if !self.overwrite_started {
      return Err(EngineError::InvalidInput("KV page update cannot commit before overwrite starts".to_string()));
    }
    let mut replacement_pages = HashMap::with_capacity(replacements.len());
    for (bucket, data) in replacements {
      let offset = self.provider.page_offset(bucket)?;
      if data.len() != self.provider.inner.page_size {
        return Err(EngineError::InvalidInput(format!(
          "KV replacement page {bucket} has {} bytes; expected {}",
          data.len(),
          self.provider.inner.page_size
        )));
      }
      live_type_counts_in_page(&data, self.provider.inner.hash_length).map_err(|error| match error {
        EngineError::CorruptEntry { reason, .. } => EngineError::CorruptEntry { offset, reason },
        other => other,
      })?;
      if replacement_pages.insert(bucket, data).is_some() {
        return Err(EngineError::InvalidInput(format!("KV update provides replacement bucket {bucket} more than once")));
      }
    }

    let mut state = self.provider.lock()?;
    let pending = state.pending.take().ok_or_else(|| EngineError::DurabilityFailure("KV page update state is missing".to_string()))?;
    if pending.generation != self.generation || !pending.overwrite_started {
      state.pending = Some(pending);
      return Err(EngineError::DurabilityFailure("KV page update state does not permit commit".to_string()));
    }
    let expected: HashSet<_> = pending.pages.keys().copied().collect();
    let actual: HashSet<_> = replacement_pages.keys().copied().collect();
    if actual != expected {
      state.pending = Some(pending);
      return Err(EngineError::InvalidInput(format!(
        "KV update replacement set does not match prepared buckets: expected {expected:?}, received {actual:?}"
      )));
    }

    for (bucket, old) in pending.pages {
      let previous = state
        .historical
        .entry(bucket)
        .or_default()
        .insert(old.old_generation, HistoricalPage { data: old.data, _reservation: old.reservation });
      if previous.is_some() {
        state.poisoned_reason = Some(format!("duplicate historical KV page generation {} for bucket {bucket}", old.old_generation));
        return Err(EngineError::DurabilityFailure(state.poisoned_reason.clone().expect("poison reason was set")));
      }
      state.bucket_generations.insert(bucket, self.generation);
    }
    state.committed_generation = self.generation;
    state.preparing_update = false;
    drop(replacement_pages);
    prune_historical_pages(&mut state);
    self.completed = true;
    Ok(())
  }

  pub fn abort_before_overwrite(mut self) -> EngineResult<()> {
    self.abort_before_overwrite_inner()?;
    self.completed = true;
    Ok(())
  }

  fn abort_before_overwrite_inner(&mut self) -> EngineResult<()> {
    let mut state = self.provider.lock()?;
    let pending = state.pending.as_ref().ok_or_else(|| EngineError::DurabilityFailure("KV page update state is missing".to_string()))?;
    if pending.generation != self.generation {
      return Err(EngineError::DurabilityFailure("KV page update generation does not match pending state".to_string()));
    }
    if self.overwrite_started || pending.overwrite_started {
      return Err(EngineError::DurabilityFailure("KV page update cannot abort after overwrite starts".to_string()));
    }
    state.pending.take();
    state.preparing_update = false;
    Ok(())
  }
}

impl Drop for KvPageUpdate {
  fn drop(&mut self) {
    if self.completed {
      return;
    }
    if !self.overwrite_started {
      if let Err(error) = self.abort_before_overwrite_inner() {
        tracing::error!(%error, generation = self.generation, "Failed to abort prepared KV page update");
      }
      return;
    }
    let Ok(mut state) = self.provider.inner.state.lock() else {
      return;
    };
    if state.pending.as_ref().is_some_and(|pending| pending.generation == self.generation) {
      state.poisoned_reason = Some(format!("KV page update generation {} was abandoned after overwrite started", self.generation));
      state.preparing_update = false;
    }
  }
}

fn remove_cached_page(state: &mut PageCacheState, bucket: usize) -> bool {
  let Some(evicted) = state.pages.remove(&bucket) else {
    return false;
  };
  state.resident_bytes = state.resident_bytes.saturating_sub(evicted.data.len() as u64);
  state.evictions = state.evictions.saturating_add(1);
  true
}

fn evict_oldest_page(state: &mut PageCacheState) -> bool {
  let Some(oldest) = state.pages.iter().min_by_key(|(_, page)| page.last_access).map(|(bucket, _)| *bucket) else {
    return false;
  };
  remove_cached_page(state, oldest)
}

fn prune_historical_pages(state: &mut PageCacheState) {
  let active_generations: Vec<u64> = state.active_generations.keys().copied().collect();
  let current_generations = &state.bucket_generations;
  state.historical.retain(|bucket, versions| {
    let current = current_generations.get(bucket).copied().unwrap_or(state.committed_generation);
    let ordered: Vec<u64> = versions.keys().copied().collect();
    versions.retain(|valid_from, _| {
      let next = ordered.iter().copied().find(|candidate| candidate > valid_from).unwrap_or(current);
      active_generations.iter().any(|generation| *valid_from <= *generation && *generation < next)
    });
    !versions.is_empty()
  });
}
