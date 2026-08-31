use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::engine::durability_coordinator::{CommitClass, DurabilityCommitReceipt, DurabilityCoordinator, NativeFileBarrierKind};
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::hot_tail;
use crate::engine::kv_pages::*;
use crate::engine::kv_nvt::KvNvt;
use crate::engine::kv_page_provider::{KvPageProvider, KvPageProviderStats};
use crate::engine::kv_snapshot::{KvPageSet, KvTypeCounts, ReadSnapshot};
use crate::engine::kv_stages::{KV_STAGE_SIZES, stage_params};
use crate::engine::kv_store::{KVEntry, KV_FLAG_DELETED};
use crate::engine::memory_coordinator::{AdmissionClass, CriticalMemoryPurpose, HostMemorySample, MemoryCoordinator, MemoryOwner, MemoryPolicy};

/// Number of buffered writes before auto-flush to KV bucket pages.
const WRITE_BUFFER_THRESHOLD: usize = 512;

/// Number of entries buffered before flushing to the hot tail.
/// Safety net for burst writes between 100ms timer flushes.
/// The timer is the primary durability mechanism — this threshold
/// handles bursts that exceed what 100ms can cover.
const HOT_BUFFER_THRESHOLD: usize = 512;

/// Temporary hard bound for retained page generations before the strict
/// process policy is available. Clean pages are still zero-retention; this
/// budget only permits safe prepare-before-overwrite publication.
const BOOTSTRAP_GENERATION_SOFT_BYTES: u64 = 4 * 1024 * 1024;
const BOOTSTRAP_GENERATION_HARD_BYTES: u64 = 8 * 1024 * 1024;
const BOOTSTRAP_GENERATION_EMERGENCY_BYTES: u64 = 1024 * 1024;
pub(crate) const KV_SNAPSHOT_DRAIN_BUSY_REASON: &str = "timed out waiting for KV snapshots before exclusive page writes";

/// A disk-resident KV store using NVT-indexed bucket pages inside the main
/// database file. No sidecar files — the KV block lives at the head of the
/// .aeordb file and the hot tail dangles off the end.
///
/// Lookup flow: write_buffer → NVT bucket → disk page scan.
pub struct DiskKVStore {
  /// NVT for O(1) bucket lookup from hash bytes.
  nvt: KvNvt,
  /// Write buffer: absorbs recent inserts before flushing to disk.
  write_buffer: HashMap<Vec<u8>, KVEntry>,
  /// File handle for the main .aeordb database file.
  /// KV pages are at kv_block_offset; hot tail at hot_tail_offset.
  db_file: File,
  durability_coordinator: Arc<DurabilityCoordinator>,
  /// Offset of the KV block within the database file.
  kv_block_offset: u64,
  /// Size of the KV block in bytes (pages must fit within this).
  kv_block_length: u64,
  /// Offset of the hot tail within the database file.
  hot_tail_offset: u64,
  /// Whether the hot tail is enabled (false for temp stores during resize).
  hot_tail_enabled: bool,
  /// Current stage in the KV_STAGE_SIZES table.
  stage: usize,
  /// Hash algorithm (determines hash_length for page layout).
  hash_algo: HashAlgorithm,
  /// Total entry count (disk + buffer, minus deleted).
  entry_count: usize,
  /// Number of buckets at the current stage.
  bucket_count: usize,
  /// Micro-buffer of entries pending write to the hot tail.
  hot_buffer: Vec<KVEntry>,
  /// Shared snapshot for lock-free readers. Updated after every mutation.
  snapshot: Arc<ArcSwap<ReadSnapshot>>,
  /// Lazy, reservation-owned page authority activated after bootstrap and
  /// recovery have completed. `None` is retained only for construction,
  /// rebuild, and compatibility tests that instantiate DiskKVStore directly.
  page_provider: Option<KvPageProvider>,
  bounded_page_config: Option<BoundedPageConfig>,
  /// Authoritative live type counts for flushed pages. Keeping this beside the
  /// writer lets bounded snapshots publish without rescanning every page.
  page_type_counts: KvTypeCounts,
  /// Shared NVT wrapped in Arc — re-cloned only on flush/resize.
  shared_nvt: Arc<KvNvt>,
  /// Set to true when a corrupt KV page requires an authoritative WAL rebuild.
  pub needs_rebuild: bool,
  /// Set to Some(target_stage) when the KV block needs expansion.
  /// StorageEngine reads this after flush and performs the expansion.
  pub needs_expansion: Option<usize>,
  /// Transaction nesting depth. When > 0, flush() skips clearing the hot tail.
  pub transaction_depth: u32,
  /// A namespace mutation with a pre-admitted hard ticket owns transaction
  /// depth exclusively; legacy guards must retry after it completes.
  pub(crate) pre_admitted_transaction_active: bool,
  /// A short first-authority transaction may stage a bounded KV delta without
  /// exposing it through the shared read snapshot. Only the matching token can
  /// publish or abort the batch.
  atomic_visibility_state: Option<AtomicKvVisibilityState>,
  next_atomic_visibility_id: u64,
  /// Current snapshot of the void_manager state, included in every hot tail
  /// flush. The engine syncs this from its VoidManager whenever voids change
  /// (GC sweep, void consumption, etc.). Hot tail load on startup populates
  /// this from disk; runtime register/consume operations on void_manager
  /// also update this field via the engine.
  pub pending_voids: Vec<crate::engine::hot_tail::VoidRecord>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AtomicKvVisibilityBatch {
  id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AtomicKvVisibilityState {
  id: u64,
  maximum_unique_entries: usize,
  baseline_entry_count: usize,
  baseline_hot_tail_offset: u64,
  expected_authority_sequence: u64,
}

struct PreparedPageFlush {
  replacements: Vec<(usize, Arc<[u8]>)>,
  overflow_entries: Vec<KVEntry>,
  page_type_counts: KvTypeCounts,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageFlushDurability {
  Immediate,
  DeferredToBulkCheckpoint,
}

#[derive(Clone)]
struct BoundedPageConfig {
  coordinator: MemoryCoordinator,
  max_resident_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DiskKvMemoryStats {
  pub write_buffer_bytes: u64,
  pub hot_buffer_bytes: u64,
  pub pending_void_bytes: u64,
  pub mutable_nvt_bytes: u64,
}

#[derive(Debug, Default)]
pub struct HotTailReplay {
  pub entries: Vec<KVEntry>,
  pub voids: Vec<crate::engine::hot_tail::VoidRecord>,
}

#[derive(Debug, Default)]
pub struct KvExpansionEntries {
  pub pending_voids: Vec<crate::engine::hot_tail::VoidRecord>,
  pub all_entries: Vec<KVEntry>,
}

impl DiskKvMemoryStats {
  pub fn total_bytes(self) -> u64 {
    self
      .write_buffer_bytes
      .saturating_add(self.hot_buffer_bytes)
      .saturating_add(self.pending_void_bytes)
      .saturating_add(self.mutable_nvt_bytes)
  }
}

impl DiskKVStore {
  pub(crate) const MAX_ATOMIC_VISIBILITY_ENTRIES: usize =
    if WRITE_BUFFER_THRESHOLD < HOT_BUFFER_THRESHOLD { WRITE_BUFFER_THRESHOLD - 1 } else { HOT_BUFFER_THRESHOLD - 1 };

  pub(crate) fn memory_stats(&self) -> DiskKvMemoryStats {
    let write_buffer_slots = self
      .write_buffer
      .capacity()
      .saturating_mul(std::mem::size_of::<(Vec<u8>, KVEntry)>().saturating_add(2 * std::mem::size_of::<usize>()));
    let write_buffer_payload = self
      .write_buffer
      .iter()
      .fold(0usize, |total, (key, entry)| total.saturating_add(key.capacity()).saturating_add(entry.hash.capacity()));
    let hot_buffer_payload = self.hot_buffer.iter().fold(0usize, |total, entry| total.saturating_add(entry.hash.capacity()));
    DiskKvMemoryStats {
      write_buffer_bytes: std::mem::size_of::<HashMap<Vec<u8>, KVEntry>>()
        .saturating_add(write_buffer_slots)
        .saturating_add(write_buffer_payload) as u64,
      hot_buffer_bytes: self.hot_buffer.capacity().saturating_mul(std::mem::size_of::<KVEntry>()).saturating_add(hot_buffer_payload) as u64,
      pending_void_bytes: self.pending_voids.capacity().saturating_mul(std::mem::size_of::<crate::engine::hot_tail::VoidRecord>()) as u64,
      mutable_nvt_bytes: self.nvt.estimated_memory_bytes(),
    }
  }

  pub(crate) fn emergency_hot_tail_payload_memory_bytes(&mut self) -> u64 {
    self.sanitize_pending_voids("emergency hot-tail memory estimate");
    let writes = self.write_buffer.values().fold(std::mem::size_of::<Vec<KVEntry>>() as u64, |total, entry| {
      total.saturating_add(std::mem::size_of::<KVEntry>() as u64).saturating_add(entry.hash.len() as u64)
    });
    let voids = (std::mem::size_of::<Vec<crate::engine::hot_tail::VoidRecord>>() as u64)
      .saturating_add((self.pending_voids.len() as u64).saturating_mul(std::mem::size_of::<crate::engine::hot_tail::VoidRecord>() as u64));
    writes.saturating_add(voids)
  }

  pub(crate) fn shutdown_flush_workspace_bytes(&self) -> u64 {
    let page_bytes = crate::engine::kv_pages::page_size(self.hash_algo.hash_length()) as u64;
    let modified_pages = (self.write_buffer.len().min(self.bucket_count) as u64).max(1);
    let memory = self.memory_stats();
    memory
      .write_buffer_bytes
      .saturating_add(memory.pending_void_bytes)
      .saturating_add(modified_pages.saturating_mul(page_bytes).saturating_mul(3))
  }

  fn sync_data_barrier(&self, estimated_bytes: u64) -> EngineResult<()> {
    self
      .durability_coordinator
      .execute_recoverable_file_barrier(&self.db_file, NativeFileBarrierKind::Data, estimated_bytes)
      .map_err(|error| EngineError::DurabilityFailure(error.to_string()))
  }

  fn page_arc(page_data: Vec<u8>) -> Arc<[u8]> {
    Arc::<[u8]>::from(page_data.into_boxed_slice())
  }

  fn bootstrap_page_memory_coordinator() -> EngineResult<MemoryCoordinator> {
    let policy =
      MemoryPolicy::new(BOOTSTRAP_GENERATION_SOFT_BYTES, BOOTSTRAP_GENERATION_HARD_BYTES, 1, BOOTSTRAP_GENERATION_EMERGENCY_BYTES)
        .map_err(|error| EngineError::InvalidInput(format!("invalid bootstrap KV memory policy: {error}")))?;
    let coordinator = MemoryCoordinator::new(policy);
    coordinator
      .update_host_sample(HostMemorySample { host_available_bytes: Some(u64::MAX), ..Default::default() })
      .map_err(|error| EngineError::InvalidInput(format!("cannot initialize bootstrap KV memory policy: {error}")))?;
    Ok(coordinator)
  }

  fn valid_void_range_for_layout(kv_block_offset: u64, kv_block_length: u64, hot_tail_offset: u64, offset: u64, size: u32) -> bool {
    if size == 0 {
      return false;
    }
    let wal_start = kv_block_offset.saturating_add(kv_block_length);
    let Some(end) = offset.checked_add(size as u64) else {
      return false;
    };
    offset >= wal_start && end <= hot_tail_offset
  }

  fn sanitize_pending_voids(&mut self, context: &str) {
    let before = self.pending_voids.len();
    let kv_block_offset = self.kv_block_offset;
    let kv_block_length = self.kv_block_length;
    let hot_tail_offset = self.hot_tail_offset;
    self
      .pending_voids
      .retain(|void| Self::valid_void_range_for_layout(kv_block_offset, kv_block_length, hot_tail_offset, void.offset, void.size));
    let dropped = before.saturating_sub(self.pending_voids.len());
    if dropped > 0 {
      tracing::warn!(
        context,
        dropped,
        wal_start = self.kv_block_offset.saturating_add(self.kv_block_length),
        hot_tail_offset = self.hot_tail_offset,
        "Dropped invalid pending void records before hot-tail persistence"
      );
    }
  }

  /// Create a new in-file KV store. Writes empty bucket pages at kv_block_offset.
  ///
  /// `db_file` is a clone of the main .aeordb file handle.
  /// `kv_block_offset` is where the KV block starts (typically 256, after file header).
  /// `hot_tail_offset` is where the hot tail lives (end of the file).
  pub fn create(db_file: File, hash_algo: HashAlgorithm, kv_block_offset: u64, hot_tail_offset: u64, stage: usize) -> EngineResult<Self> {
    Self::create_with_coordinator(db_file, hash_algo, kv_block_offset, hot_tail_offset, stage, Arc::new(DurabilityCoordinator::new()))
  }

  pub fn create_with_coordinator(
    db_file: File,
    hash_algo: HashAlgorithm,
    kv_block_offset: u64,
    hot_tail_offset: u64,
    stage: usize,
    durability_coordinator: Arc<DurabilityCoordinator>,
  ) -> EngineResult<Self> {
    let stage = stage.min(KV_STAGE_SIZES.len() - 1);
    let hash_length = hash_algo.hash_length();
    let psize = page_size(hash_length);
    let (kv_block_length, _) = stage_params(stage, psize);
    Self::create_with_layout_and_coordinator(
      db_file,
      hash_algo,
      kv_block_offset,
      kv_block_length,
      hot_tail_offset,
      stage,
      durability_coordinator,
    )
  }

  #[allow(clippy::too_many_arguments)]
  pub(crate) fn create_with_layout_and_coordinator(
    mut db_file: File,
    hash_algo: HashAlgorithm,
    kv_block_offset: u64,
    kv_block_length: u64,
    hot_tail_offset: u64,
    stage: usize,
    durability_coordinator: Arc<DurabilityCoordinator>,
  ) -> EngineResult<Self> {
    let stage = stage.min(KV_STAGE_SIZES.len() - 1);
    let hash_length = hash_algo.hash_length();
    let psize = page_size(hash_length);
    let (minimum_block_size, bucket_count) = stage_params(stage, psize);
    if kv_block_length < minimum_block_size {
      return Err(EngineError::InvalidInput(format!(
        "KV block length {kv_block_length} is smaller than the {minimum_block_size}-byte minimum for stage {stage}"
      )));
    }
    let kv_block_end =
      kv_block_offset.checked_add(kv_block_length).ok_or_else(|| EngineError::InvalidInput("KV block end overflows u64".to_string()))?;
    if hot_tail_offset != 0 && hot_tail_offset < kv_block_end {
      return Err(EngineError::InvalidInput(format!(
        "hot tail offset {hot_tail_offset} begins before the KV block ends at {kv_block_end}"
      )));
    }

    tracing::debug!(
      kv_block_offset,
      kv_block_length,
      hot_tail_offset,
      stage,
      bucket_count,
      psize,
      pages_bytes = bucket_count * psize,
      max_entries = bucket_count * MAX_ENTRIES_PER_PAGE,
      "DiskKVStore::create"
    );

    // Write empty pages for all buckets at kv_block_offset
    let empty_page = vec![0u8; psize];
    db_file.seek(SeekFrom::Start(kv_block_offset))?;
    for _ in 0..bucket_count {
      db_file.write_all(&empty_page)?;
    }
    durability_coordinator
      .execute_recoverable_file_barrier(&db_file, NativeFileBarrierKind::Data, (bucket_count * psize) as u64)
      .map_err(|error| EngineError::DurabilityFailure(error.to_string()))?;

    let nvt = KvNvt::new(bucket_count);
    let shared_nvt = Arc::new(nvt.clone());
    let bootstrap_provider = KvPageProvider::new(
      db_file.try_clone()?,
      kv_block_offset,
      hash_algo,
      bucket_count,
      0,
      Some(Self::bootstrap_page_memory_coordinator()?),
    )?;
    let bootstrap_pages = bootstrap_provider.snapshot()?;
    let initial_snapshot = ReadSnapshot::from_bounded_pages_with_type_counts(
      HashMap::new(),
      Arc::clone(&shared_nvt),
      bucket_count,
      hash_algo,
      0,
      bootstrap_pages,
      [0; 16],
    );
    let snapshot = Arc::new(ArcSwap::new(Arc::new(initial_snapshot)));

    Ok(DiskKVStore {
      nvt,
      write_buffer: HashMap::new(),
      db_file,
      durability_coordinator,
      kv_block_offset,
      kv_block_length,
      hot_tail_offset,
      hot_tail_enabled: true,
      stage,
      hash_algo,
      entry_count: 0,
      bucket_count,
      hot_buffer: Vec::new(),
      snapshot,
      page_provider: Some(bootstrap_provider),
      bounded_page_config: None,
      page_type_counts: [0; 16],
      shared_nvt,
      needs_rebuild: false,
      needs_expansion: None,
      transaction_depth: 0,
      pre_admitted_transaction_active: false,
      atomic_visibility_state: None,
      next_atomic_visibility_id: 1,
      pending_voids: Vec::new(),
    })
  }

  /// Open an existing in-file KV store by reading bucket pages from the database file.
  ///
  /// `db_file` is a clone of the main .aeordb file handle.
  /// `kv_block_offset` and `hot_tail_offset` come from the file header.
  /// `stage` comes from the file header's `kv_block_stage`.
  /// `hot_entries` are entries loaded from the hot tail (passed in by StorageEngine).
  /// Current on-disk layout version for the KV-pages block (the page array
  /// between `kv_block_offset` and `hot_tail_offset`). Stored in the
  /// [`FileHeader`](crate::engine::file_header::FileHeader) and re-validated
  /// on every open. Bump alongside a new layout migration in `open`.
  pub const CURRENT_KV_BLOCK_VERSION: u8 = 1;

  pub fn open(
    db_file: File,
    hash_algo: HashAlgorithm,
    kv_block_offset: u64,
    hot_tail_offset: u64,
    stage: usize,
    replay: HotTailReplay,
    kv_block_version: u8,
  ) -> EngineResult<Self> {
    let HotTailReplay { entries: hot_entries, voids: hot_voids } = replay;
    Self::open_with_coordinator(
      db_file,
      hash_algo,
      kv_block_offset,
      hot_tail_offset,
      stage,
      hot_entries,
      hot_voids,
      kv_block_version,
      Arc::new(DurabilityCoordinator::new()),
    )
  }

  #[allow(clippy::too_many_arguments)]
  pub fn open_with_coordinator(
    db_file: File,
    hash_algo: HashAlgorithm,
    kv_block_offset: u64,
    hot_tail_offset: u64,
    stage: usize,
    hot_entries: Vec<KVEntry>,
    hot_voids: Vec<crate::engine::hot_tail::VoidRecord>,
    kv_block_version: u8,
    durability_coordinator: Arc<DurabilityCoordinator>,
  ) -> EngineResult<Self> {
    let hash_length = hash_algo.hash_length();
    let (kv_block_length, _) = stage_params(stage.min(KV_STAGE_SIZES.len() - 1), page_size(hash_length));
    Self::open_with_layout_and_coordinator(
      db_file,
      hash_algo,
      kv_block_offset,
      kv_block_length,
      hot_tail_offset,
      stage,
      hot_entries,
      hot_voids,
      kv_block_version,
      durability_coordinator,
    )
  }

  #[allow(clippy::too_many_arguments)]
  pub(crate) fn open_with_layout_and_coordinator(
    db_file: File,
    hash_algo: HashAlgorithm,
    kv_block_offset: u64,
    kv_block_length: u64,
    hot_tail_offset: u64,
    stage: usize,
    hot_entries: Vec<KVEntry>,
    hot_voids: Vec<crate::engine::hot_tail::VoidRecord>,
    kv_block_version: u8,
    durability_coordinator: Arc<DurabilityCoordinator>,
  ) -> EngineResult<Self> {
    if kv_block_version != Self::CURRENT_KV_BLOCK_VERSION {
      // The KV-pages-on-disk layout we know how to read is v1. A future
      // bump will add the matching deserializer here; today, any other
      // value means the file was written by a newer engine and we
      // refuse to risk silent corruption.
      return Err(EngineError::InvalidEntryVersion(kv_block_version));
    }
    let hot_entry_count = hot_entries.len();
    let stage = stage.min(KV_STAGE_SIZES.len() - 1);
    let hash_length = hash_algo.hash_length();
    let psize = page_size(hash_length);
    let (minimum_block_size, bucket_count) = stage_params(stage, psize);
    if kv_block_length < minimum_block_size {
      return Err(EngineError::CorruptEntry {
        offset: kv_block_offset,
        reason: format!("KV block length {kv_block_length} is smaller than the {minimum_block_size}-byte minimum for stage {stage}"),
      });
    }
    let kv_block_end = kv_block_offset
      .checked_add(kv_block_length)
      .ok_or_else(|| EngineError::CorruptEntry { offset: kv_block_offset, reason: "KV block end overflows u64".to_string() })?;
    if hot_tail_offset < kv_block_end {
      return Err(EngineError::CorruptEntry {
        offset: hot_tail_offset,
        reason: format!("hot tail begins before the KV block ends at {kv_block_end}"),
      });
    }

    tracing::debug!(kv_block_offset, kv_block_length, hot_tail_offset, stage, bucket_count, hot_entry_count, "DiskKVStore::open");

    let nvt = KvNvt::new(bucket_count);
    let shared_nvt = Arc::new(nvt.clone());

    // Pre-populate write buffer with hot entries (not yet flushed to pages)
    let mut write_buffer = HashMap::new();
    for entry in hot_entries {
      write_buffer.insert(entry.hash.clone(), entry);
    }

    // Hot-tail voids are durable tombstones for WAL ranges reclaimed by GC.
    // Bucket pages may still contain old live KV entries for those ranges if
    // the process crashed after the hot tail was flushed but before the KV
    // write buffer was flushed to pages. Mask those page entries with deleted
    // buffer entries so clean startup never serves data from reusable space.
    let merged_hot_void_ranges = build_merged_void_ranges(&hot_voids);
    let mut effective_entry_count = 0usize;
    let mut page_type_counts = [0usize; 16];
    let mut detected_page_corruption = false;
    let bootstrap_provider = KvPageProvider::new(
      db_file.try_clone()?,
      kv_block_offset,
      hash_algo,
      bucket_count,
      0,
      Some(Self::bootstrap_page_memory_coordinator()?),
    )?;
    let bootstrap_pages = bootstrap_provider.snapshot()?;

    // Validate and account one page at a time without retaining the complete
    // block. Corrupt pages trigger a WAL rebuild before startup consumers run;
    // exact I/O failures still abort because truncation may also destroy WAL.
    for bucket in 0..bucket_count {
      let page_data = match bootstrap_pages.read_page(bucket) {
        Ok(page) => page,
        Err(error @ EngineError::CorruptEntry { .. }) => {
          tracing::warn!(bucket, ?error, "KV bucket page failed validation on open — triggering dirty startup");
          detected_page_corruption = true;
          continue;
        }
        Err(error) => return Err(error),
      };
      let entries = deserialize_page(&page_data, hash_length)?;
      let counts = live_type_counts_in_page(&page_data, hash_length)?;
      for (index, count) in counts.into_iter().enumerate() {
        page_type_counts[index] = page_type_counts[index].saturating_add(count);
      }
      for entry in entries {
        if entry.is_deleted() || write_buffer.contains_key(&entry.hash) {
          continue;
        }
        if !hot_voids.is_empty() {
          let entry_end = entry.offset.saturating_add(entry.total_length as u64);
          if entry_overlaps_void_ranges(entry.offset, entry_end, &merged_hot_void_ranges) {
            let mut deleted = entry;
            deleted.type_flags |= KV_FLAG_DELETED;
            write_buffer.insert(deleted.hash.clone(), deleted);
            continue;
          }
        }
        effective_entry_count = effective_entry_count.saturating_add(1);
      }
    }

    effective_entry_count = effective_entry_count.saturating_add(write_buffer.values().filter(|entry| !entry.is_deleted()).count());
    let initial_snapshot = ReadSnapshot::from_bounded_pages_with_type_counts(
      write_buffer.clone(),
      Arc::clone(&shared_nvt),
      bucket_count,
      hash_algo,
      effective_entry_count,
      bootstrap_pages,
      page_type_counts,
    );
    let snapshot = Arc::new(ArcSwap::new(Arc::new(initial_snapshot)));

    Ok(DiskKVStore {
      nvt,
      write_buffer,
      db_file,
      durability_coordinator,
      kv_block_offset,
      kv_block_length,
      hot_tail_offset,
      hot_tail_enabled: true,
      stage,
      hash_algo,
      entry_count: effective_entry_count,
      bucket_count,
      hot_buffer: Vec::new(),
      snapshot,
      page_provider: Some(bootstrap_provider),
      bounded_page_config: None,
      page_type_counts,
      shared_nvt,
      needs_rebuild: detected_page_corruption,
      needs_expansion: None,
      transaction_depth: 0,
      pre_admitted_transaction_active: false,
      atomic_visibility_state: None,
      next_atomic_visibility_id: 1,
      pending_voids: hot_voids,
    })
  }

  /// Create a temporary KV store for resize operations. No hot tail.
  pub fn create_temp(db_file: File, hash_algo: HashAlgorithm, kv_block_offset: u64, stage: usize) -> EngineResult<Self> {
    let mut store = Self::create(db_file, hash_algo, kv_block_offset, 0, stage)?;
    store.hot_tail_enabled = false;
    Ok(store)
  }

  /// Replace the bootstrap snapshot backend with the configured bounded page
  /// provider. Activation is atomic from readers' perspective: the new
  /// generation lease is acquired before ArcSwap releases the prior view.
  pub fn activate_bounded_pages(&mut self, coordinator: MemoryCoordinator, max_resident_bytes: u64) -> EngineResult<()> {
    if self.bounded_page_config.is_some() && self.page_provider.is_some() {
      return Err(EngineError::InvalidInput("bounded KV pages are already active".to_string()));
    }
    let provider = KvPageProvider::new(
      self.db_file.try_clone()?,
      self.kv_block_offset,
      self.hash_algo,
      self.bucket_count,
      max_resident_bytes,
      Some(coordinator.clone()),
    )?;
    self.publish_through_page_provider(provider)?;
    self.bounded_page_config = Some(BoundedPageConfig { coordinator, max_resident_bytes });
    Ok(())
  }

  /// Publish a fresh page provider with a new clean-residency bound.
  /// Existing snapshots retain their old provider until their leases drain;
  /// new readers atomically observe the replacement generation.
  pub fn reconfigure_bounded_pages(&mut self, max_resident_bytes: u64) -> EngineResult<()> {
    let config =
      self.bounded_page_config.clone().ok_or_else(|| EngineError::InvalidInput("bounded KV pages are not active".to_string()))?;
    let provider = KvPageProvider::new(
      self.db_file.try_clone()?,
      self.kv_block_offset,
      self.hash_algo,
      self.bucket_count,
      max_resident_bytes,
      Some(config.coordinator.clone()),
    )?;
    self.publish_through_page_provider(provider)?;
    self.bounded_page_config = Some(BoundedPageConfig { coordinator: config.coordinator, max_resident_bytes });
    Ok(())
  }

  pub(crate) fn activate_bootstrap_page_provider(&mut self) -> EngineResult<()> {
    if self.page_provider.is_some() {
      return Ok(());
    }
    let provider = KvPageProvider::new(
      self.db_file.try_clone()?,
      self.kv_block_offset,
      self.hash_algo,
      self.bucket_count,
      0,
      Some(Self::bootstrap_page_memory_coordinator()?),
    )?;
    self.publish_through_page_provider(provider)
  }

  fn publish_through_page_provider(&mut self, provider: KvPageProvider) -> EngineResult<()> {
    let pages = provider.snapshot()?;
    let snapshot = ReadSnapshot::from_bounded_pages_with_type_counts(
      self.write_buffer.clone(),
      Arc::clone(&self.shared_nvt),
      self.bucket_count,
      self.hash_algo,
      self.entry_count,
      pages,
      self.page_type_counts,
    );
    self.page_provider = Some(provider);
    self.snapshot.store(Arc::new(snapshot));
    Ok(())
  }

  pub fn kv_page_provider_stats(&self) -> EngineResult<Option<KvPageProviderStats>> {
    self.page_provider.as_ref().map(KvPageProvider::stats).transpose()
  }

  pub(crate) fn bounded_page_configuration(&self) -> Option<(MemoryCoordinator, u64)> {
    self.bounded_page_config.as_ref().map(|config| (config.coordinator.clone(), config.max_resident_bytes))
  }

  /// Revoke the published provider-backed view and wait for every prior
  /// snapshot lease to drain while keeping the provider installed. Exclusive
  /// rebuild owners use this before repeated in-place page replacement: the
  /// provider still supplies prepare-before-overwrite safety, but no reader
  /// requires historical page generations to be retained.
  ///
  /// A timeout restores a fresh readable view before returning an error.
  pub(crate) fn quiesce_bounded_page_snapshots_for_exclusive_write(&mut self, timeout: std::time::Duration) -> EngineResult<bool> {
    let Some(provider) = self.page_provider.clone() else {
      return Ok(false);
    };
    let placeholder = ReadSnapshot::unavailable(
      self.write_buffer.clone(),
      Arc::clone(&self.shared_nvt),
      self.bucket_count,
      self.hash_algo,
      self.entry_count,
      self.page_type_counts,
      "KV page publication is quiesced for exclusive writes",
    );
    self.snapshot.store(Arc::new(placeholder));
    match provider.wait_for_no_snapshots(timeout) {
      Ok(true) => return Ok(true),
      Ok(false) => {}
      Err(wait_error) => {
        return match self.publish_full_snapshot() {
          Ok(()) => Err(wait_error),
          Err(restore_error) => Err(EngineError::DurabilityFailure(format!(
            "failed to drain KV snapshots ({wait_error}) and failed to restore publication ({restore_error})"
          ))),
        };
      }
    }

    self.publish_full_snapshot()?;
    Err(EngineError::ResourceExhausted(KV_SNAPSHOT_DRAIN_BUSY_REASON.to_string()))
  }

  /// Remove the provider-backed snapshot from publication and wait for every
  /// previously loaded view to drain before a complete page-layout rewrite.
  /// A timeout restores a fresh bounded view before returning an error.
  pub(crate) fn suspend_bounded_pages_for_layout_rewrite(&mut self, timeout: std::time::Duration) -> EngineResult<bool> {
    if !self.quiesce_bounded_page_snapshots_for_exclusive_write(timeout)? {
      return Ok(false);
    }
    self.page_provider.take();
    Ok(true)
  }

  fn reactivate_bounded_pages_after_layout_rewrite(&mut self) -> EngineResult<bool> {
    if self.page_provider.is_some() {
      return Ok(false);
    }
    if let Some(config) = self.bounded_page_config.clone() {
      self.activate_bounded_pages(config.coordinator, config.max_resident_bytes)?;
    } else {
      self.activate_bootstrap_page_provider()?;
    }
    Ok(true)
  }

  // ========================================================================
  // Core KV operations
  // ========================================================================

  /// Look up an entry by hash.
  /// Search order: write_buffer → disk page.
  pub fn get(&self, hash: &[u8]) -> EngineResult<Option<KVEntry>> {
    self.snapshot.load().get(hash)
  }

  /// Insert or update an entry.
  pub fn insert(&mut self, entry: KVEntry) -> EngineResult<()> {
    self.require_no_atomic_visibility("ordinary KV insertion")?;
    let is_new = !self.write_buffer.contains_key(&entry.hash) && !self.entry_exists_on_disk(&entry.hash)?;

    self.write_buffer.insert(entry.hash.clone(), entry.clone());

    if is_new {
      self.entry_count += 1;
    }

    // Journal to hot buffer
    if self.hot_tail_enabled {
      self.hot_buffer.push(entry);
      if self.hot_buffer.len() >= HOT_BUFFER_THRESHOLD {
        self.flush_hot_buffer()?;
      }
    }

    let did_flush = if self.write_buffer.len() >= WRITE_BUFFER_THRESHOLD {
      self.flush()?;
      true
    } else {
      false
    };

    if !did_flush {
      self.publish_buffer_only();
    }

    Ok(())
  }

  /// Verify that one already validated set of new keys can be flushed into the
  /// current fixed KV layout. The caller must hold the writer and provide a
  /// clean baseline so this read-only result stays valid until publication.
  pub(crate) fn preflight_new_keys_fit_current_layout(&mut self, keys: &[&[u8]]) -> EngineResult<bool> {
    self.require_no_atomic_visibility("KV capacity preflight")?;
    if !self.write_buffer.is_empty() || !self.hot_buffer.is_empty() {
      return Err(EngineError::ResourceExhausted(
        "KV capacity preflight requires an explicitly flushed write and hot-buffer baseline".to_string(),
      ));
    }

    let hash_length = self.hash_algo.hash_length();
    let mut unique = HashSet::new();
    unique
      .try_reserve(keys.len())
      .map_err(|error| EngineError::ResourceExhausted(format!("KV capacity preflight identity allocation failed: {error}")))?;
    let mut keys_by_bucket: BTreeMap<usize, Vec<&[u8]>> = BTreeMap::new();
    for key in keys {
      if key.len() != hash_length {
        return Err(EngineError::InvalidInput(format!(
          "KV capacity preflight key length {} does not match active hash length {hash_length}",
          key.len()
        )));
      }
      if !unique.insert(*key) {
        return Err(EngineError::InvalidInput("KV capacity preflight contains a duplicate key".to_string()));
      }
      keys_by_bucket.entry(self.nvt.bucket_for_value(key)).or_default().push(*key);
    }

    for (bucket, new_keys) in keys_by_bucket {
      let page = self.current_page(bucket)?;
      let existing = deserialize_page(&page, hash_length)?;
      let additional = new_keys.iter().filter(|key| !existing.iter().any(|entry| entry.hash.as_slice() == **key)).count();
      let projected = existing
        .len()
        .checked_add(additional)
        .ok_or_else(|| EngineError::ResourceExhausted("KV capacity preflight entry count overflowed".to_string()))?;
      if projected > MAX_ENTRIES_PER_PAGE {
        return Ok(false);
      }
    }
    Ok(true)
  }

  /// Begin one bounded batch whose entries remain absent from every published
  /// read snapshot until a matching hard-authority receipt is supplied.
  ///
  /// The caller owns the KV writer exclusively for the batch lifetime. Dirty
  /// baseline buffers are refused instead of being folded into the authority
  /// transition or flushed implicitly.
  pub(crate) fn begin_atomic_visibility_batch(
    &mut self,
    maximum_unique_entries: usize,
    expected_authority_sequence: u64,
  ) -> EngineResult<AtomicKvVisibilityBatch> {
    if self.atomic_visibility_state.is_some() {
      return Err(EngineError::ResourceExhausted("an atomic KV visibility batch is already active".to_string()));
    }
    if self.transaction_depth != 0 || self.pre_admitted_transaction_active {
      return Err(EngineError::ResourceExhausted(
        "atomic KV visibility requires exclusive ownership outside another transaction".to_string(),
      ));
    }
    if !self.write_buffer.is_empty() || !self.hot_buffer.is_empty() {
      return Err(EngineError::ResourceExhausted(
        "atomic KV visibility requires an explicitly flushed write and hot-buffer baseline".to_string(),
      ));
    }
    if maximum_unique_entries == 0 || maximum_unique_entries > Self::MAX_ATOMIC_VISIBILITY_ENTRIES {
      return Err(EngineError::InvalidInput(format!(
        "atomic KV visibility entry bound must be in 1..={}",
        Self::MAX_ATOMIC_VISIBILITY_ENTRIES
      )));
    }
    if expected_authority_sequence == 0 {
      return Err(EngineError::InvalidInput("atomic KV visibility requires a nonzero expected authority sequence".to_string()));
    }
    let id = self.next_atomic_visibility_id;
    self.next_atomic_visibility_id = self
      .next_atomic_visibility_id
      .checked_add(1)
      .ok_or_else(|| EngineError::ResourceExhausted("atomic KV visibility token space is exhausted".to_string()))?;
    self.atomic_visibility_state = Some(AtomicKvVisibilityState {
      id,
      maximum_unique_entries,
      baseline_entry_count: self.entry_count,
      baseline_hot_tail_offset: self.hot_tail_offset,
      expected_authority_sequence,
    });
    Ok(AtomicKvVisibilityBatch { id })
  }

  /// Stage one KV entry without page flush, hot-tail flush, or snapshot
  /// publication. Physical WAL append ownership remains with the caller.
  pub(crate) fn stage_atomic_visibility_entry(&mut self, batch: AtomicKvVisibilityBatch, entry: KVEntry) -> EngineResult<()> {
    let state = self.require_atomic_visibility_batch(batch)?;
    let new_staged_key = !self.write_buffer.contains_key(&entry.hash);
    if !new_staged_key {
      return Err(EngineError::InvalidInput("atomic KV visibility cannot stage one key more than once".to_string()));
    }
    if self.write_buffer.len() >= state.maximum_unique_entries {
      return Err(EngineError::ResourceExhausted(format!(
        "atomic KV visibility batch exceeds its {} unique-entry bound",
        state.maximum_unique_entries
      )));
    }
    let is_new = !self.entry_exists_on_disk(&entry.hash)?;
    let next_entry_count = if is_new {
      self
        .entry_count
        .checked_add(1)
        .ok_or_else(|| EngineError::ResourceExhausted("atomic KV visibility entry count overflow".to_string()))?
    } else {
      self.entry_count
    };
    self.write_buffer.insert(entry.hash.clone(), entry.clone());
    self.entry_count = next_entry_count;
    self.hot_buffer.push(entry);
    Ok(())
  }

  /// Discard the volatile KV delta. Appended WAL bytes remain unreachable raw
  /// evidence for quarantine/recovery and are not inserted into the snapshot.
  pub(crate) fn abort_atomic_visibility_batch(&mut self, batch: AtomicKvVisibilityBatch) -> EngineResult<()> {
    let state = self.require_atomic_visibility_batch(batch)?;
    self.write_buffer.clear();
    self.hot_buffer.clear();
    self.entry_count = state.baseline_entry_count;
    self.hot_tail_offset = state.baseline_hot_tail_offset;
    self.atomic_visibility_state = None;
    Ok(())
  }

  /// Publish the staged KV delta only after the authority coordinator proves
  /// that the exact hard sequence reached its durability frontier.
  pub(crate) fn publish_atomic_visibility_after_authority(
    &mut self,
    batch: AtomicKvVisibilityBatch,
    receipt: &DurabilityCommitReceipt,
  ) -> EngineResult<()> {
    self.require_atomic_visibility_batch(batch)?;
    let state = self.require_atomic_visibility_batch(batch)?;
    if !self.hot_buffer.is_empty() {
      return Err(EngineError::DurabilityFailure(
        "atomic KV visibility cannot publish before its dependency payload is completed".to_string(),
      ));
    }
    if receipt.class != CommitClass::HardAuthority
      || receipt.sequence != state.expected_authority_sequence
      || receipt.hard_frontier < receipt.sequence
    {
      return Err(EngineError::DurabilityFailure(
        "atomic KV visibility requires its exact completed hard-authority receipt at or below the proven frontier".to_string(),
      ));
    }
    self.atomic_visibility_state = None;
    self.hot_buffer.clear();
    self.publish_buffer_only();
    Ok(())
  }

  fn require_atomic_visibility_batch(&self, batch: AtomicKvVisibilityBatch) -> EngineResult<AtomicKvVisibilityState> {
    match self.atomic_visibility_state {
      Some(state) if state.id == batch.id => Ok(state),
      Some(_) => Err(EngineError::InvalidInput("atomic KV visibility token does not own the active batch".to_string())),
      None => Err(EngineError::InvalidInput("no atomic KV visibility batch is active".to_string())),
    }
  }

  fn require_no_atomic_visibility(&self, operation: &str) -> EngineResult<()> {
    if self.atomic_visibility_state.is_some() {
      return Err(EngineError::ResourceExhausted(format!(
        "{operation} is unavailable while an atomic visibility batch owns the KV writer"
      )));
    }
    Ok(())
  }

  /// Bulk insert without public snapshot publication or per-window barriers.
  ///
  /// This is only for exclusive rebuild/expansion owners. The caller must
  /// finish with a durable page/hot-tail checkpoint before publishing the
  /// resulting layout. Intermediate pages are deliberately recoverable soft
  /// state so a large rebuild does not issue one barrier per 512 records.
  pub fn bulk_insert(&mut self, entries: &[KVEntry]) -> EngineResult<()> {
    self.require_no_atomic_visibility("bulk KV insertion")?;
    for entry in entries {
      if !self.write_buffer.contains_key(&entry.hash) && !self.entry_exists_on_current_layout(&entry.hash)? {
        self.entry_count += 1;
      }
      self.write_buffer.insert(entry.hash.clone(), entry.clone());

      if self.write_buffer.len() >= WRITE_BUFFER_THRESHOLD {
        self.flush_no_snapshot()?;
      }
    }
    Ok(())
  }

  fn flush_no_snapshot(&mut self) -> EngineResult<()> {
    if self.write_buffer.is_empty() {
      return Ok(());
    }
    let prepared = self.prepare_page_flush()?;
    self.apply_prepared_page_flush(
      prepared,
      PageFlushDurability::DeferredToBulkCheckpoint,
      MemoryOwner::KvSnapshotGenerations,
      AdmissionClass::Workload,
    )?;
    Ok(())
  }

  fn current_page(&mut self, bucket: usize) -> EngineResult<Arc<[u8]>> {
    if let Some(provider) = &self.page_provider {
      provider.read_page(bucket)
    } else {
      if bucket >= self.bucket_count {
        return Err(EngineError::InvalidInput(format!(
          "KV bucket {bucket} is outside the current layout of {} buckets",
          self.bucket_count
        )));
      }
      let hash_length = self.hash_algo.hash_length();
      let offset = self.kv_block_offset + bucket_page_offset(bucket, hash_length);
      let mut page = vec![0u8; page_size(hash_length)];
      self.db_file.seek(SeekFrom::Start(offset))?;
      self.db_file.read_exact(&mut page)?;
      live_type_counts_in_page(&page, hash_length).map_err(|error| match error {
        EngineError::CorruptEntry { reason, .. } => EngineError::CorruptEntry { offset, reason },
        other => other,
      })?;
      Ok(Self::page_arc(page))
    }
  }

  fn entry_exists_on_current_layout(&mut self, hash: &[u8]) -> EngineResult<bool> {
    let bucket = self.nvt.bucket_for_value(hash);
    let page = self.current_page(bucket)?;
    Ok(find_entry_in_page_data(&page, self.hash_algo.hash_length(), hash, true)?.is_some())
  }

  /// Build every replacement and detect overflow before any on-disk page is
  /// touched. Corruption is rebuild evidence, never an invitation to zero the
  /// page and continue with a partial index.
  fn prepare_page_flush(&mut self) -> EngineResult<PreparedPageFlush> {
    let hash_length = self.hash_algo.hash_length();
    let mut by_bucket: BTreeMap<usize, Vec<KVEntry>> = BTreeMap::new();
    for entry in self.write_buffer.values().cloned() {
      by_bucket.entry(self.nvt.bucket_for_value(&entry.hash)).or_default().push(entry);
    }

    let mut replacements = Vec::with_capacity(by_bucket.len());
    let mut overflow_entries = Vec::new();
    let mut next_type_counts = self.page_type_counts;
    for (bucket, new_entries) in by_bucket {
      let offset = self.kv_block_offset + bucket_page_offset(bucket, hash_length);
      let page = match self.current_page(bucket) {
        Ok(page) => page,
        Err(error) => {
          if matches!(error, EngineError::CorruptEntry { .. }) {
            self.needs_rebuild = true;
          }
          return Err(error);
        }
      };
      let old_counts = live_type_counts_in_page(&page, hash_length).map_err(|error| match error {
        EngineError::CorruptEntry { reason, .. } => EngineError::CorruptEntry { offset, reason },
        other => other,
      });
      let old_counts = match old_counts {
        Ok(counts) => counts,
        Err(error) => {
          self.needs_rebuild = true;
          return Err(error);
        }
      };
      let mut existing = match deserialize_page(&page, hash_length) {
        Ok(entries) => entries,
        Err(error) => {
          self.needs_rebuild = true;
          return Err(match error {
            EngineError::CorruptEntry { reason, .. } => EngineError::CorruptEntry { offset, reason },
            other => other,
          });
        }
      };
      for entry in new_entries {
        if !upsert_in_page(&mut existing, entry.clone()) {
          overflow_entries.push(entry);
        }
      }
      let replacement = Self::page_arc(serialize_page(&existing, hash_length));
      let new_counts = live_type_counts_in_page(&replacement, hash_length)?;
      for index in 0..next_type_counts.len() {
        next_type_counts[index] = next_type_counts[index].saturating_sub(old_counts[index]).saturating_add(new_counts[index]);
      }
      replacements.push((bucket, replacement));
    }
    Ok(PreparedPageFlush { replacements, overflow_entries, page_type_counts: next_type_counts })
  }

  /// Apply one completely prepared replacement set. Provider retention is
  /// admitted before `mark_overwrite_started`; the buffer and publication
  /// state remain untouched on every pre-overwrite failure.
  fn apply_prepared_page_flush(
    &mut self,
    prepared: PreparedPageFlush,
    durability: PageFlushDurability,
    generation_owner: MemoryOwner,
    generation_admission: AdmissionClass,
  ) -> EngineResult<Vec<(usize, Arc<[u8]>)>> {
    let buckets = prepared.replacements.iter().map(|(bucket, _)| *bucket).collect::<Vec<_>>();
    let mut update = self
      .page_provider
      .as_ref()
      .map(|provider| provider.begin_update_with_admission(&buckets, generation_owner, generation_admission))
      .transpose()?;
    let hash_length = self.hash_algo.hash_length();
    let mut overwrite_started = false;
    for (bucket, page) in &prepared.replacements {
      let offset = self.kv_block_offset + bucket_page_offset(*bucket, hash_length);
      self.db_file.seek(SeekFrom::Start(offset)).map_err(|error| {
        if overwrite_started {
          EngineError::PostMutationDurabilityFailure(format!("cannot seek to KV page {bucket} after another page overwrite began: {error}"))
        } else {
          EngineError::from(error)
        }
      })?;
      if !overwrite_started {
        if let Some(update) = update.as_mut() {
          update.mark_overwrite_started()?;
        }
        overwrite_started = true;
      }
      self
        .db_file
        .write_all(page)
        .map_err(|error| EngineError::PostMutationDurabilityFailure(format!("KV page {bucket} overwrite failed: {error}")))?;
    }
    if durability == PageFlushDurability::Immediate {
      self
        .sync_data_barrier(prepared.replacements.iter().map(|(_, page)| page.len() as u64).sum())
        .map_err(|error| EngineError::PostMutationDurabilityFailure(format!("KV page overwrite barrier failed: {error}")))?;
    }
    if let Some(update) = update {
      update
        .commit(prepared.replacements.clone())
        .map_err(|error| EngineError::PostMutationDurabilityFailure(format!("KV page generation publication failed: {error}")))?;
    }

    self.page_type_counts = prepared.page_type_counts;
    self.write_buffer = prepared.overflow_entries.into_iter().map(|entry| (entry.hash.clone(), entry)).collect();
    Ok(prepared.replacements)
  }

  fn entry_exists_on_disk(&self, hash: &[u8]) -> EngineResult<bool> {
    let current = self.snapshot.load();
    Ok(current.get_raw(hash)?.is_some())
  }

  /// Flush the write buffer to KV bucket pages.
  pub fn flush(&mut self) -> EngineResult<()> {
    self.flush_with_generation_admission(MemoryOwner::KvSnapshotGenerations, AdmissionClass::Workload)
  }

  pub(crate) fn flush_for_shutdown(&mut self) -> EngineResult<()> {
    self.flush_with_generation_admission(MemoryOwner::Shutdown, AdmissionClass::Critical(CriticalMemoryPurpose::Shutdown))
  }

  fn flush_with_generation_admission(&mut self, generation_owner: MemoryOwner, generation_admission: AdmissionClass) -> EngineResult<()> {
    self.require_no_atomic_visibility("KV page flush")?;
    if self.write_buffer.is_empty() {
      return Ok(());
    }
    let timer_start = std::time::Instant::now();
    tracing::debug!(
      write_buffer_len = self.write_buffer.len(),
      bucket_count = self.bucket_count,
      stage = self.stage,
      kv_block_offset = self.kv_block_offset,
      kv_block_length = self.kv_block_length,
      "flush: starting"
    );

    let prepared = self.prepare_page_flush()?;
    let overflow_entries = prepared.overflow_entries.clone();
    let replacements = self.apply_prepared_page_flush(prepared, PageFlushDurability::Immediate, generation_owner, generation_admission)?;
    let modified_buckets = replacements.iter().map(|(bucket, _)| *bucket).collect::<Vec<_>>();

    tracing::debug!(overflow_count = overflow_entries.len(), modified_buckets = modified_buckets.len(), "flush: pages written");

    if !overflow_entries.is_empty() {
      // Publish snapshot BEFORE resize so iter_all sees flushed entries
      self.publish_snapshot_incremental(&replacements)?;
      let old_stage = self.stage;
      self.resize_to_next_stage()?;
      if self.stage > old_stage {
        // Resize succeeded — re-insert overflow and flush again
        for entry in overflow_entries {
          self.write_buffer.insert(entry.hash.clone(), entry);
        }
        return self.flush_with_generation_admission(generation_owner, generation_admission);
      } else {
        // Resize blocked (block too small) — keep overflow in write buffer.
        // They're queryable via snapshot and will be persisted in the hot tail.
        // Write overflow entries to hot tail for crash recovery
        if self.hot_tail_enabled {
          let hash_length = self.hash_algo.hash_length();
          let all_hot: Vec<KVEntry> = self.write_buffer.values().cloned().collect();
          tracing::debug!(
            overflow_count = all_hot.len(),
            hot_tail_offset = self.hot_tail_offset,
            "flush: writing overflow entries to hot tail (resize blocked)"
          );
          self.sanitize_pending_voids("resize-blocked hot-tail flush");
          let payload = hot_tail::HotTailPayload { writes: all_hot, voids: self.pending_voids.clone() };
          let end = hot_tail::write_hot_tail(&mut self.db_file, self.hot_tail_offset, &payload, hash_length)?;
          self.db_file.set_len(end)?; // Truncate stale trailing data
          self.sync_data_barrier(0)?;
        }
        self.publish_snapshot_incremental(&replacements)?;
        self.publish_buffer_only();
        let elapsed = timer_start.elapsed().as_secs_f64();
        metrics::histogram!(crate::metrics::definitions::KV_FLUSH_DURATION).record(elapsed);
        return Ok(());
      }
    }

    // All entries flushed to pages — clear hot tail
    tracing::debug!(hot_tail_offset = self.hot_tail_offset, "flush: all entries fit in pages, clearing hot tail");
    self.flush_hot_buffer()?;
    if self.transaction_depth == 0 && self.hot_tail_enabled {
      let hash_length = self.hash_algo.hash_length();
      // Propagate write errors. If this fails (EIO / disk full), the
      // on-disk hot tail still has the OLD entries while hot_buffer
      // has been cleared — recovery would later load stale entries
      // pointing at WAL offsets that have since been overwritten.
      // Even when all KV writes flushed to pages, we keep the void
      // snapshot in the hot tail so void state survives.
      self.sanitize_pending_voids("kv flush page-hot-tail update");
      let payload = hot_tail::HotTailPayload { writes: Vec::new(), voids: self.pending_voids.clone() };
      let end = hot_tail::write_hot_tail(&mut self.db_file, self.hot_tail_offset, &payload, hash_length)
        .map_err(|e| EngineError::IoError(std::io::Error::other(format!("Failed to write hot tail after page flush: {}", e))))?;
      self.db_file.set_len(end)?;
      self.sync_data_barrier(0)?;
    }

    self.publish_snapshot_incremental(&replacements)?;

    let elapsed = timer_start.elapsed().as_secs_f64();
    metrics::histogram!(crate::metrics::definitions::KV_FLUSH_DURATION).record(elapsed);

    Ok(())
  }

  /// Request a coordinated layout change to the next stage.
  ///
  /// DiskKVStore does not own the file header, WAL relocation, or persistent
  /// durability latch, so it must never reinterpret page offsets itself.
  /// StorageEngine consumes `needs_expansion` after releasing the writer/KV
  /// locks and performs the only authorized full-layout mutation.
  pub fn resize_to_next_stage(&mut self) -> EngineResult<()> {
    let new_stage = (self.stage + 1).min(KV_STAGE_SIZES.len() - 1);
    if new_stage == self.stage {
      return Err(EngineError::IoError(std::io::Error::other("KV store at maximum stage — cannot resize further")));
    }
    tracing::info!(current_stage = self.stage, target_stage = new_stage, "KV layout change requested from StorageEngine");
    self.needs_expansion = Some(new_stage);
    Ok(())
  }

  pub fn contains(&self, hash: &[u8]) -> EngineResult<bool> {
    Ok(self.get(hash)?.is_some())
  }

  pub fn mark_deleted(&mut self, hash: &[u8]) -> EngineResult<bool> {
    self.require_no_atomic_visibility("KV deletion")?;
    if let Some(mut entry) = self.get(hash)? {
      entry.type_flags |= KV_FLAG_DELETED;
      self.write_buffer.insert(hash.to_vec(), entry);
      self.entry_count = self.entry_count.saturating_sub(1);
      self.publish_buffer_only();
      Ok(true)
    } else {
      Ok(false)
    }
  }

  pub fn mark_deleted_batch(&mut self, hashes: &[Vec<u8>]) -> EngineResult<()> {
    self.require_no_atomic_visibility("batched KV deletion")?;
    // Resolve each hash from the published snapshot (or current write_buffer)
    // without taking the KV writer lock again for each candidate. The snapshot
    // backend owns any required resident-page or bounded-provider access.
    let snapshot = self.snapshot.load();
    for hash in hashes {
      let mut entry = if let Some(buffered) = self.write_buffer.get(hash) {
        if buffered.is_deleted() {
          continue;
        }
        buffered.clone()
      } else if let Some(snap_entry) = snapshot.get_raw(hash)? {
        if snap_entry.is_deleted() {
          continue;
        }
        snap_entry
      } else {
        continue; // unknown hash — nothing to mark
      };
      entry.type_flags |= KV_FLAG_DELETED;
      self.write_buffer.insert(hash.clone(), entry);
      self.entry_count = self.entry_count.saturating_sub(1);
    }
    drop(snapshot);

    // **Deferred flush.** The DELETED-flag updates live only in the in-memory
    // write_buffer + the next published ReadSnapshot. We do NOT flush them
    // to disk bucket pages here, because the bulk DeletionRecord written to
    // the WAL just before this call IS the durable record. On crash, the
    // rebuilder replays that record and re-applies the deletions.
    //
    // The buffered flags will be flushed eventually — either when the
    // write_buffer hits WRITE_BUFFER_THRESHOLD during normal writes, or
    // on clean shutdown. This amortizes the bucket-page-rewrite cost
    // across subsequent writes instead of stalling the GC caller.
    self.publish_buffer_only();
    Ok(())
  }

  pub fn iter_all(&self) -> EngineResult<Vec<KVEntry>> {
    self.snapshot.load().iter_all()
  }

  pub fn len(&self) -> usize {
    self.entry_count
  }
  pub fn is_empty(&self) -> bool {
    self.entry_count == 0
  }
  pub fn write_buffer_len(&self) -> usize {
    self.write_buffer.len()
  }

  /// Snapshot volatile KV state for emergency preservation after a serious
  /// durability failure. The caller writes this outside the DB file.
  pub fn emergency_hot_tail_payload(&mut self) -> hot_tail::HotTailPayload {
    let all_hot: Vec<KVEntry> = self.write_buffer.values().cloned().collect();
    self.sanitize_pending_voids("emergency hot-tail spill");
    hot_tail::HotTailPayload { writes: all_hot, voids: self.pending_voids.clone() }
  }

  /// Look up an entry in the write buffer only (no disk read).
  pub fn get_buffered(&self, hash: &[u8]) -> Option<&KVEntry> {
    self.write_buffer.get(hash)
  }

  /// Clear the write buffer without flushing. Used before dropping a KV
  /// store that is being replaced (e.g., after rebuild_kv) to prevent
  /// the Drop impl from overwriting newly-rebuilt pages with stale data.
  pub fn clear_write_buffer(&mut self) {
    self.write_buffer.clear();
  }

  /// Insert an entry into the write buffer without triggering auto-flush
  /// or hot buffer journaling. Used by rebuild_kv to accumulate all entries
  /// before a single flush, preventing page clobbering across flush cycles.
  pub fn buffer_only(&mut self, entry: KVEntry) -> EngineResult<()> {
    self.require_no_atomic_visibility("unpublished KV buffering")?;
    let is_new = !self.write_buffer.contains_key(&entry.hash);
    self.write_buffer.insert(entry.hash.clone(), entry);
    if is_new {
      self.entry_count += 1;
    }
    Ok(())
  }

  pub fn update_flags(&mut self, hash: &[u8], new_flags: u8) -> EngineResult<bool> {
    self.require_no_atomic_visibility("KV flag update")?;
    if let Some(mut entry) = self.get(hash)? {
      let entry_type = entry.type_flags & 0x0F;
      entry.type_flags = entry_type | (new_flags & 0xF0);
      self.write_buffer.insert(hash.to_vec(), entry);
      self.publish_buffer_only();
      Ok(true)
    } else {
      Ok(false)
    }
  }

  pub fn update_offset(&mut self, hash: &[u8], new_offset: u64) -> EngineResult<bool> {
    self.require_no_atomic_visibility("KV offset update")?;
    if let Some(mut entry) = self.get(hash)? {
      entry.offset = new_offset;
      self.write_buffer.insert(hash.to_vec(), entry);
      self.publish_buffer_only();
      Ok(true)
    } else {
      Ok(false)
    }
  }

  pub fn stage(&self) -> usize {
    self.stage
  }
  pub fn bucket_count(&self) -> usize {
    self.bucket_count
  }
  pub fn hash_algo(&self) -> HashAlgorithm {
    self.hash_algo
  }
  pub fn hot_tail_offset(&self) -> u64 {
    self.hot_tail_offset
  }

  pub(crate) fn kv_block_offset(&self) -> u64 {
    self.kv_block_offset
  }

  pub(crate) fn kv_block_length(&self) -> u64 {
    self.kv_block_length
  }

  pub(crate) fn clone_database_file(&self) -> EngineResult<File> {
    Ok(self.db_file.try_clone()?)
  }

  pub(crate) fn shares_durability_coordinator(&self, coordinator: &Arc<DurabilityCoordinator>) -> bool {
    Arc::ptr_eq(&self.durability_coordinator, coordinator)
  }

  /// Update the hot tail offset (called by StorageEngine after a WAL append).
  /// Update the hot tail offset. Called by StorageEngine after each WAL append
  /// so the KV store knows where the hot tail should be written.
  /// CRITICAL: this must be called after every WAL write to prevent the hot
  /// tail from being written over live WAL data.
  pub fn set_hot_tail_offset(&mut self, offset: u64) {
    // Never move the hot tail backward — that would overwrite WAL entries
    if offset > self.hot_tail_offset {
      self.hot_tail_offset = offset;
    }
  }

  // ========================================================================
  // Snapshot publishing
  // ========================================================================

  fn read_all_pages(&mut self) -> EngineResult<KvPageSet> {
    let hash_length = self.hash_algo.hash_length();
    let psize = page_size(hash_length);
    let mut pages = Vec::with_capacity(self.bucket_count);
    for bucket in 0..self.bucket_count {
      let offset = self.kv_block_offset + bucket_page_offset(bucket, hash_length);
      let mut page_data = vec![0u8; psize];
      self.db_file.seek(SeekFrom::Start(offset))?;
      self.db_file.read_exact(&mut page_data)?;
      live_type_counts_in_page(&page_data, hash_length).map_err(|error| match error {
        EngineError::CorruptEntry { reason, .. } => EngineError::CorruptEntry { offset, reason },
        other => other,
      })?;
      pages.push(Self::page_arc(page_data));
    }
    Ok(Arc::new(pages))
  }

  fn publish_buffer_only(&mut self) {
    let snapshot = {
      let current = self.snapshot.load();
      current.republish_with_buffer(
        self.write_buffer.clone(),
        Arc::clone(&self.shared_nvt),
        self.bucket_count,
        self.hash_algo,
        self.entry_count,
      )
    };
    self.snapshot.store(Arc::new(snapshot));
  }

  fn publish_full_snapshot(&mut self) -> EngineResult<()> {
    if let Some(provider) = &self.page_provider {
      let pages = provider.snapshot()?;
      let snapshot = ReadSnapshot::from_bounded_pages_with_type_counts(
        self.write_buffer.clone(),
        Arc::clone(&self.shared_nvt),
        self.bucket_count,
        self.hash_algo,
        self.entry_count,
        pages,
        self.page_type_counts,
      );
      self.snapshot.store(Arc::new(snapshot));
      return Ok(());
    }
    let pages = self.read_all_pages()?;
    let snapshot = ReadSnapshot::new(
      self.write_buffer.clone(),
      Arc::clone(&self.shared_nvt),
      self.bucket_count,
      self.hash_algo,
      self.entry_count,
      pages,
    )?;
    self.snapshot.store(Arc::new(snapshot));
    Ok(())
  }

  fn publish_snapshot_incremental(&mut self, replacements: &[(usize, Arc<[u8]>)]) -> EngineResult<()> {
    if self.shared_nvt.bucket_count() != self.nvt.bucket_count() {
      self.shared_nvt = Arc::new(self.nvt.clone());
    }

    if let Some(provider) = &self.page_provider {
      let pages = provider.snapshot()?;
      let snapshot = ReadSnapshot::from_bounded_pages_with_type_counts(
        self.write_buffer.clone(),
        Arc::clone(&self.shared_nvt),
        self.bucket_count,
        self.hash_algo,
        self.entry_count,
        pages,
        self.page_type_counts,
      );
      self.snapshot.store(Arc::new(snapshot));
      return Ok(());
    }

    let current = self.snapshot.load();
    let old_pages = current
      .resident_pages()
      .ok_or_else(|| EngineError::InvalidInput("resident incremental publisher cannot update bounded KV pages".to_string()))?;
    let mut new_pages = (**old_pages).clone();
    for (bucket, page) in replacements {
      if *bucket < new_pages.len() {
        new_pages[*bucket] = Arc::clone(page);
      }
    }

    let snapshot = ReadSnapshot::new_with_page_type_counts(
      self.write_buffer.clone(),
      Arc::clone(&self.shared_nvt),
      self.bucket_count,
      self.hash_algo,
      self.entry_count,
      Arc::new(new_pages),
      self.page_type_counts,
    );
    self.snapshot.store(Arc::new(snapshot));
    Ok(())
  }

  fn publish_full_snapshot_with_new_nvt(&mut self) -> EngineResult<()> {
    self.shared_nvt = Arc::new(self.nvt.clone());
    if self.reactivate_bounded_pages_after_layout_rewrite()? {
      Ok(())
    } else {
      self.publish_full_snapshot()
    }
  }

  pub fn snapshot_handle(&self) -> &Arc<ArcSwap<ReadSnapshot>> {
    &self.snapshot
  }

  /// Make this KV store publish future lock-free snapshots through an existing
  /// engine-owned snapshot handle. Used when `StorageEngine::rebuild_kv`
  /// swaps in a newly-created store without replacing the engine's shared
  /// `ArcSwap` handle that readers already hold.
  pub fn adopt_snapshot_handle(&mut self, snapshot: Arc<ArcSwap<ReadSnapshot>>) -> EngineResult<()> {
    self.snapshot = snapshot;
    self.publish_full_snapshot()
  }

  /// Finalize KV block expansion after StorageEngine has relocated WAL data.
  /// Adjusts entry offsets, zeroes new pages, rehashes into new bucket layout,
  /// updates header, and publishes new snapshot.
  ///
  /// `target_stage`: the new KV stage to expand to
  /// `old_kv_end`: the old end of the KV block (where growth zone started)
  /// `relocation_end`: the end of the old WAL region relocated out of the
  /// expanded KV block. This can extend beyond the new KV block when an entry
  /// straddles the boundary.
  /// `offset_delta`: how much relocated entries' offsets shifted (copy_dst - old_kv_end)
  /// `new_hot_tail`: the new hot tail offset after relocation
  /// `pending_voids`: adjusted void snapshot for the new WAL layout
  pub fn finalize_expansion(
    &mut self,
    target_stage: usize,
    old_kv_end: u64,
    relocation_end: u64,
    offset_delta: i64,
    new_hot_tail: u64,
    entries: KvExpansionEntries,
  ) -> EngineResult<()> {
    let KvExpansionEntries { pending_voids, all_entries } = entries;
    let target_block_length = stage_params(target_stage, page_size(self.hash_algo.hash_length())).0;
    self.finalize_expansion_with_block_length(
      target_stage,
      target_block_length,
      old_kv_end,
      relocation_end,
      offset_delta,
      new_hot_tail,
      pending_voids,
      all_entries,
    )
  }

  #[allow(clippy::too_many_arguments)]
  pub(crate) fn finalize_expansion_with_block_length(
    &mut self,
    target_stage: usize,
    new_block_length: u64,
    old_kv_end: u64,
    relocation_end: u64,
    offset_delta: i64,
    new_hot_tail: u64,
    pending_voids: Vec<crate::engine::hot_tail::VoidRecord>,
    all_entries: Vec<KVEntry>,
  ) -> EngineResult<()> {
    let hash_length = self.hash_algo.hash_length();
    let psize = page_size(hash_length);
    let (minimum_block_size, new_bucket_count) = stage_params(target_stage, psize);
    if new_block_length < minimum_block_size {
      return Err(EngineError::InvalidInput(format!(
        "expanded KV block length {new_block_length} is smaller than the {minimum_block_size}-byte stage {target_stage} minimum"
      )));
    }
    let new_pages_size = (new_bucket_count as u64) * (psize as u64);

    // Zero-fill all KV bucket pages in the expanded region
    let empty_page = vec![0u8; psize];
    for bucket in 0..new_bucket_count {
      let offset = self.kv_block_offset + bucket_page_offset(bucket, hash_length);
      self.db_file.seek(SeekFrom::Start(offset))?;
      self.db_file.write_all(&empty_page)?;
    }
    if new_block_length > new_pages_size {
      let slack_offset = self.kv_block_offset + new_pages_size;
      let mut slack_len = new_block_length - new_pages_size;
      let zeroes = vec![0u8; 64 * 1024];
      self.db_file.seek(SeekFrom::Start(slack_offset))?;
      while slack_len > 0 {
        let write_len = usize::try_from(slack_len.min(zeroes.len() as u64))
          .map_err(|_| EngineError::InvalidInput("KV expansion slack chunk cannot be represented as usize".to_string()))?;
        self.db_file.write_all(&zeroes[..write_len])?;
        slack_len -= write_len as u64;
      }
    }
    self.sync_data_barrier(0)?;

    // Update internal state
    self.kv_block_length = new_block_length;
    self.stage = target_stage;
    self.bucket_count = new_bucket_count;
    self.nvt = KvNvt::new(new_bucket_count);
    self.hot_tail_offset = new_hot_tail;
    self.entry_count = 0;
    self.pending_voids = pending_voids;
    self.write_buffer.clear();
    self.hot_buffer.clear();

    // Adjust offsets for relocated entries.
    let adjusted: Vec<KVEntry> = all_entries
      .into_iter()
      .map(|mut e| {
        if e.offset >= old_kv_end && e.offset < relocation_end {
          let shifted = i128::from(e.offset) + i128::from(offset_delta);
          e.offset = u64::try_from(shifted)
            .map_err(|_| EngineError::InvalidInput(format!("relocated KV entry offset {shifted} cannot be represented as u64")))?;
        }
        Ok(e)
      })
      .collect::<EngineResult<_>>()?;

    // Rehash into new bucket layout
    self.bulk_insert(&adjusted)?;
    self.flush_no_snapshot()?;
    self.entry_count = adjusted.len();

    // Most entries are now durable in the expanded pages. A concentrated
    // bucket can still overflow the target layout, so the residual buffer is
    // part of the authoritative hot tail until a later stage can absorb it.
    // These are adjusted entries produced above, never stale pre-expansion
    // writes that point into the relocated growth zone.
    self.sanitize_pending_voids("finalize expansion hot-tail publish");
    let payload = hot_tail::HotTailPayload { writes: self.write_buffer.values().cloned().collect(), voids: self.pending_voids.clone() };
    let end = hot_tail::write_hot_tail(&mut self.db_file, self.hot_tail_offset, &payload, hash_length)?;
    self.db_file.set_len(end)?;
    self.sync_data_barrier(0)?;

    // Header authority belongs to StorageEngine. It publishes the final v3
    // A/B slot only after this method has made the relocated pages and hot
    // tail durable; publishing here as well creates two competing writer
    // views over one selector.

    // Publish new snapshot
    self.publish_full_snapshot_with_new_nvt()?;
    self.needs_expansion = if self.write_buffer.is_empty() {
      None
    } else if target_stage + 1 < KV_STAGE_SIZES.len() {
      Some(target_stage + 1)
    } else {
      tracing::warn!(
        target_stage,
        overflow_entries = self.write_buffer.len(),
        "KV target layout remains overfull at the maximum stage; overflow remains recoverable in the hot tail"
      );
      None
    };

    tracing::info!(
      target_stage,
      new_bucket_count,
      new_block_length,
      new_pages_size,
      overflow_entries = self.write_buffer.len(),
      next_expansion_stage = self.needs_expansion,
      "KV block expansion finalized"
    );

    Ok(())
  }

  // ========================================================================
  // Hot tail (replaces hot file)
  // ========================================================================

  /// Replace the snapshot of voids to include on the next hot tail flush.
  /// Called by the engine whenever VoidManager state changes (GC sweep,
  /// void consumption during writes, etc.).
  pub fn set_pending_voids(&mut self, voids: Vec<crate::engine::hot_tail::VoidRecord>) {
    self.pending_voids = voids;
    self.sanitize_pending_voids("set pending voids");
  }

  /// Force a hot tail flush even if hot_buffer is below threshold. Used by
  /// the engine after operations that change void state but don't touch
  /// the KV write_buffer (e.g., GC sweep that only registers voids).
  pub fn force_flush_hot_buffer(&mut self) -> EngineResult<()> {
    if self.prepare_hot_tail_dependency(true)? {
      self.sync_data_barrier(self.pending_hot_tail_bytes()?)?;
      self.complete_hot_tail_dependency();
    }
    Ok(())
  }

  /// Flush the hot buffer to the hot tail at the end of the database file.
  pub fn flush_hot_buffer(&mut self) -> EngineResult<()> {
    if self.prepare_hot_tail_dependency(false)? {
      self.sync_data_barrier(self.pending_hot_tail_bytes()?)?;
      self.complete_hot_tail_dependency();
    }

    Ok(())
  }

  pub(crate) fn pending_hot_tail_bytes(&self) -> EngineResult<u64> {
    if !self.hot_tail_enabled {
      return Ok(0);
    }
    u64::try_from(hot_tail::serialized_size(self.write_buffer.len(), self.pending_voids.len(), self.hash_algo.hash_length())?)
      .map_err(|_| EngineError::ResourceExhausted("hot-tail serialized length exceeds u64".to_string()))
  }

  pub(crate) fn prepare_hot_tail_dependency(&mut self, force: bool) -> std::io::Result<bool> {
    if !self.hot_tail_enabled || (!force && self.hot_buffer.is_empty()) {
      return Ok(false);
    }

    self.sanitize_pending_voids(if force { "force hot-tail dependency" } else { "hot-buffer dependency" });
    let payload = hot_tail::HotTailPayload { writes: self.write_buffer.values().cloned().collect(), voids: self.pending_voids.clone() };
    let serialized_length = hot_tail::serialized_size(payload.writes.len(), payload.voids.len(), self.hash_algo.hash_length())
      .and_then(|length| {
        u64::try_from(length).map_err(|_| EngineError::ResourceExhausted("hot-tail serialized length exceeds u64".to_string()))
      })
      .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string()))?;
    let end = self
      .hot_tail_offset
      .checked_add(serialized_length)
      .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "hot-tail end offset overflowed"))?;
    self.db_file.seek(SeekFrom::Start(self.hot_tail_offset))?;
    hot_tail::write_hot_tail_payload(&mut self.db_file, &payload, self.hash_algo.hash_length()).map_err(|error| match error {
      EngineError::IoError(error) => error,
      other => std::io::Error::other(other.to_string()),
    })?;
    self.db_file.set_len(end)?;
    Ok(true)
  }

  pub(crate) fn complete_hot_tail_dependency(&mut self) {
    self.hot_buffer.clear();
  }

  /// Number of entries in the hot buffer.
  pub fn hot_buffer_len(&self) -> usize {
    self.hot_buffer.len()
  }
}

impl Drop for DiskKVStore {
  fn drop(&mut self) {
    if let Some(state) = self.atomic_visibility_state.take() {
      self.write_buffer.clear();
      self.hot_buffer.clear();
      self.entry_count = state.baseline_entry_count;
      self.hot_tail_offset = state.baseline_hot_tail_offset;
      tracing::error!(atomic_visibility_id = state.id, "DiskKVStore discarded an uncommitted atomic visibility batch during drop");
      return;
    }
    if !self.write_buffer.is_empty() {
      if let Err(e) = self.flush() {
        tracing::error!("DiskKVStore: failed to flush on drop: {}", e);
      }
    }
  }
}

fn build_merged_void_ranges(voids: &[crate::engine::hot_tail::VoidRecord]) -> Vec<(u64, u64)> {
  let mut ranges: Vec<(u64, u64)> = voids
    .iter()
    .filter_map(|void| {
      if void.size == 0 {
        return None;
      }
      let end = void.offset.checked_add(void.size as u64)?;
      if end <= void.offset {
        return None;
      }
      Some((void.offset, end))
    })
    .collect();

  ranges.sort_unstable_by_key(|(start, end)| (*start, *end));

  let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
  for (start, end) in ranges {
    if let Some(last) = merged.last_mut() {
      if start <= last.1 {
        last.1 = last.1.max(end);
        continue;
      }
    }
    merged.push((start, end));
  }
  merged
}

fn entry_overlaps_void_ranges(entry_start: u64, entry_end: u64, void_ranges: &[(u64, u64)]) -> bool {
  if entry_start >= entry_end || void_ranges.is_empty() {
    return false;
  }

  let idx = void_ranges.partition_point(|(_, void_end)| *void_end <= entry_start);
  idx < void_ranges.len() && void_ranges[idx].0 < entry_end
}

#[cfg(test)]
mod internal_tests {
  use super::*;
  use crate::engine::hot_tail::VoidRecord;
  use tempfile::tempdir;

  #[test]
  fn explicit_layout_creation_refuses_invalid_spans_before_mutation() {
    let directory = tempdir().unwrap();
    let minimum = crate::engine::kv_stages::initial_block_size();

    let undersized_path = directory.path().join("undersized.aeordb");
    let undersized = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&undersized_path).unwrap();
    let error = match DiskKVStore::create_with_layout_and_coordinator(
      undersized,
      HashAlgorithm::Blake3_256,
      256,
      minimum - 1,
      256 + minimum,
      0,
      Arc::new(DurabilityCoordinator::new()),
    ) {
      Ok(_) => panic!("undersized explicit layout must be rejected"),
      Err(error) => error,
    };
    assert!(error.to_string().contains("smaller than"));
    assert_eq!(std::fs::metadata(undersized_path).unwrap().len(), 0);

    let overlap_path = directory.path().join("overlap.aeordb");
    let overlap = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&overlap_path).unwrap();
    let error = match DiskKVStore::create_with_layout_and_coordinator(
      overlap,
      HashAlgorithm::Blake3_256,
      256,
      minimum + 4096,
      256 + minimum,
      0,
      Arc::new(DurabilityCoordinator::new()),
    ) {
      Ok(_) => panic!("overlapping explicit layout must be rejected"),
      Err(error) => error,
    };
    assert!(error.to_string().contains("before the KV block ends"));
    assert_eq!(std::fs::metadata(overlap_path).unwrap().len(), 0);
  }

  #[test]
  fn exclusive_bulk_rebuild_does_not_retain_historical_page_generations() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("exclusive-bulk-rebuild.aeordb");
    let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(path).unwrap();
    let stage = 3;
    let (block_size, _) = stage_params(stage, page_size(HashAlgorithm::Blake3_256.hash_length()));
    let mut store = DiskKVStore::create(file, HashAlgorithm::Blake3_256, 256, 256 + block_size, stage).unwrap();

    assert!(store.quiesce_bounded_page_snapshots_for_exclusive_write(std::time::Duration::from_secs(1)).unwrap());
    assert_eq!(store.kv_page_provider_stats().unwrap().unwrap().active_snapshots, 0);

    for index in 0..6_144u64 {
      let hash = blake3::hash(&index.to_le_bytes()).as_bytes().to_vec();
      store
        .bulk_insert(&[KVEntry { type_flags: crate::engine::kv_store::KV_TYPE_CHUNK, hash, offset: index + 1, total_length: 1 }])
        .unwrap();
    }

    let stats = store.kv_page_provider_stats().unwrap().unwrap();
    assert_eq!(stats.active_snapshots, 0);
    assert_eq!(stats.historical_pages, 0, "exclusive rebuilds have no readers that require historical page generations");
    assert_eq!(stats.historical_bytes, 0);

    store.flush().unwrap();
    store.publish_full_snapshot().unwrap();
    assert_eq!(store.kv_page_provider_stats().unwrap().unwrap().active_snapshots, 1);
    for index in [0u64, 511, 512, 4_095, 6_143] {
      let hash = blake3::hash(&index.to_le_bytes()).as_bytes().to_vec();
      assert_eq!(store.get(&hash).unwrap().unwrap().offset, index + 1);
    }
  }

  #[test]
  fn exclusive_write_quiesce_timeout_restores_readable_publication() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("exclusive-write-timeout.aeordb");
    let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(path).unwrap();
    let block_size = crate::engine::kv_stages::initial_block_size();
    let mut store = DiskKVStore::create(file, HashAlgorithm::Blake3_256, 256, 256 + block_size, 0).unwrap();
    let retained_reader = store.snapshot_handle().load_full();

    let error = store.quiesce_bounded_page_snapshots_for_exclusive_write(std::time::Duration::ZERO).unwrap_err();

    assert!(matches!(error, EngineError::ResourceExhausted(_)));
    assert!(error.to_string().contains("timed out waiting for KV snapshots"));
    assert!(store.snapshot_handle().load().bounded_pages().is_some(), "a pre-overwrite timeout must restore a readable provider view");
    assert!(store.kv_page_provider_stats().unwrap().is_some());
    drop(retained_reader);
  }

  #[test]
  fn merged_void_ranges_drop_invalid_and_merge_overlaps() {
    let merged = build_merged_void_ranges(&[
      VoidRecord { offset: 100, size: 10 },
      VoidRecord { offset: 105, size: 10 },
      VoidRecord { offset: 200, size: 0 },
      VoidRecord { offset: u64::MAX - 1, size: 4 },
      VoidRecord { offset: 115, size: 5 },
      VoidRecord { offset: 300, size: 1 },
    ]);

    assert_eq!(merged, vec![(100, 120), (300, 301)]);
  }

  #[test]
  fn entry_overlap_uses_half_open_ranges() {
    let ranges = build_merged_void_ranges(&[VoidRecord { offset: 100, size: 20 }, VoidRecord { offset: 200, size: 20 }]);

    assert!(!entry_overlaps_void_ranges(80, 100, &ranges));
    assert!(entry_overlaps_void_ranges(80, 101, &ranges));
    assert!(entry_overlaps_void_ranges(100, 101, &ranges));
    assert!(entry_overlaps_void_ranges(119, 121, &ranges));
    assert!(!entry_overlaps_void_ranges(120, 199, &ranges));
    assert!(entry_overlaps_void_ranges(199, 201, &ranges));
    assert!(!entry_overlaps_void_ranges(220, 230, &ranges));
  }
}

#[cfg(test)]
#[path = "../../spec/engine/disk_kv_visibility_internal_spec.rs"]
mod disk_kv_visibility_internal_spec;
