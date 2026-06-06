use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

use crate::engine::file_record::FileRecord;
use crate::engine::kv_store::{KV_TYPE_CHUNK, KV_TYPE_FILE_RECORD, KV_TYPE_DIRECTORY, KV_TYPE_SNAPSHOT, KV_TYPE_FORK, KV_TYPE_SYMLINK};
use crate::engine::storage_engine::StorageEngine;

/// Atomic counters for O(1) database statistics.
///
/// Maintained in-memory only -- initialized from KV snapshot on startup,
/// incremented/decremented during operations, reconciled by GC.
/// All operations use Relaxed ordering (approximate counts are fine,
/// GC reconciles periodically).
pub struct EngineCounters {
  // Counts
  pub files: AtomicU64,
  pub directories: AtomicU64,
  pub symlinks: AtomicU64,
  pub chunks: AtomicU64,
  pub snapshots: AtomicU64,
  pub forks: AtomicU64,

  // Sizes (bytes)
  pub logical_data_size: AtomicU64,
  pub chunk_data_size: AtomicU64,
  pub void_space: AtomicU64,

  // Throughput (monotonic, never decremented)
  pub writes_total: AtomicU64,
  pub reads_total: AtomicU64,
  pub bytes_written_total: AtomicU64,
  pub bytes_read_total: AtomicU64,

  // Dedup tracking
  pub chunks_deduped_total: AtomicU64,

  // Write buffer
  pub write_buffer_depth: AtomicU64,

  // Void tracking — count + total bytes of reusable space.
  pub void_count: AtomicU64,
}

/// A plain (non-atomic) snapshot of all counter values at a point in time.
/// Used by the stats API and metrics pulse for serialization.
#[derive(Debug, Clone, Serialize)]
pub struct CountersSnapshot {
  pub files: u64,
  pub directories: u64,
  pub symlinks: u64,
  pub chunks: u64,
  pub snapshots: u64,
  pub forks: u64,
  pub logical_data_size: u64,
  pub chunk_data_size: u64,
  pub void_space: u64,
  pub writes_total: u64,
  pub reads_total: u64,
  pub bytes_written_total: u64,
  pub bytes_read_total: u64,
  pub chunks_deduped_total: u64,
  pub write_buffer_depth: u64,
  pub void_count: u64,
}

impl EngineCounters {
  /// Create new counters with all fields initialized to zero.
  pub fn new() -> Self {
    EngineCounters {
      files: AtomicU64::new(0),
      directories: AtomicU64::new(0),
      symlinks: AtomicU64::new(0),
      chunks: AtomicU64::new(0),
      snapshots: AtomicU64::new(0),
      forks: AtomicU64::new(0),
      logical_data_size: AtomicU64::new(0),
      chunk_data_size: AtomicU64::new(0),
      void_space: AtomicU64::new(0),
      writes_total: AtomicU64::new(0),
      reads_total: AtomicU64::new(0),
      bytes_written_total: AtomicU64::new(0),
      bytes_read_total: AtomicU64::new(0),
      chunks_deduped_total: AtomicU64::new(0),
      write_buffer_depth: AtomicU64::new(0),
      void_count: AtomicU64::new(0),
    }
  }

  // ── Count increment/decrement helpers ────────────────────────────────

  pub fn increment_files(&self) {
    self.files.fetch_add(1, Ordering::Relaxed);
  }

  pub fn decrement_files(&self) {
    saturating_sub(&self.files, 1);
  }

  pub fn increment_directories(&self) {
    self.directories.fetch_add(1, Ordering::Relaxed);
  }

  pub fn decrement_directories(&self) {
    saturating_sub(&self.directories, 1);
  }

  pub fn increment_symlinks(&self) {
    self.symlinks.fetch_add(1, Ordering::Relaxed);
  }

  pub fn decrement_symlinks(&self) {
    saturating_sub(&self.symlinks, 1);
  }

  pub fn increment_chunks(&self) {
    self.chunks.fetch_add(1, Ordering::Relaxed);
  }

  pub fn decrement_chunks(&self) {
    saturating_sub(&self.chunks, 1);
  }

  pub fn increment_snapshots(&self) {
    self.snapshots.fetch_add(1, Ordering::Relaxed);
  }

  pub fn decrement_snapshots(&self) {
    saturating_sub(&self.snapshots, 1);
  }

  pub fn increment_forks(&self) {
    self.forks.fetch_add(1, Ordering::Relaxed);
  }

  pub fn decrement_forks(&self) {
    saturating_sub(&self.forks, 1);
  }

  // ── Size helpers (add/subtract bytes) ────────────────────────────────

  pub fn add_logical_data_size(&self, bytes: u64) {
    self.logical_data_size.fetch_add(bytes, Ordering::Relaxed);
  }

  pub fn sub_logical_data_size(&self, bytes: u64) {
    saturating_sub(&self.logical_data_size, bytes);
  }

  pub fn add_chunk_data_size(&self, bytes: u64) {
    self.chunk_data_size.fetch_add(bytes, Ordering::Relaxed);
  }

  pub fn sub_chunk_data_size(&self, bytes: u64) {
    saturating_sub(&self.chunk_data_size, bytes);
  }

  pub fn add_void_space(&self, bytes: u64) {
    self.void_space.fetch_add(bytes, Ordering::Relaxed);
  }

  pub fn sub_void_space(&self, bytes: u64) {
    saturating_sub(&self.void_space, bytes);
  }

  // ── Throughput helpers (monotonic, never decremented) ─────────────────

  pub fn increment_writes(&self) {
    self.writes_total.fetch_add(1, Ordering::Relaxed);
  }

  pub fn increment_reads(&self) {
    self.reads_total.fetch_add(1, Ordering::Relaxed);
  }

  pub fn add_bytes_written(&self, bytes: u64) {
    self.bytes_written_total.fetch_add(bytes, Ordering::Relaxed);
  }

  pub fn add_bytes_read(&self, bytes: u64) {
    self.bytes_read_total.fetch_add(bytes, Ordering::Relaxed);
  }

  /// Record one logical write operation.
  pub fn record_write(&self, bytes: u64) {
    self.increment_writes();
    self.add_bytes_written(bytes);
  }

  /// Record one logical read operation.
  pub fn record_read(&self, bytes: u64) {
    self.increment_reads();
    self.add_bytes_read(bytes);
  }

  /// Record a newly persisted chunk.
  pub fn record_chunk_stored(&self, bytes: u64) {
    self.increment_chunks();
    self.add_chunk_data_size(bytes);
  }

  /// Record a deduplicated chunk hit.
  pub fn record_chunk_deduped(&self) {
    self.increment_chunks_deduped();
  }

  /// Record a file write and keep live file/logical-size counters in sync.
  pub fn record_file_write(&self, previous_size: Option<u64>, new_size: u64, throughput_bytes: u64) {
    self.record_write(throughput_bytes);
    match previous_size {
      None => {
        self.increment_files();
        self.add_logical_data_size(new_size);
      }
      Some(old_size) if new_size >= old_size => {
        self.add_logical_data_size(new_size - old_size);
      }
      Some(old_size) => {
        self.sub_logical_data_size(old_size - new_size);
      }
    }
  }

  /// Record a file deletion mutation and keep live counters in sync.
  pub fn record_file_delete(&self, size: u64) {
    self.record_write(0);
    self.decrement_files();
    self.sub_logical_data_size(size);
  }

  /// Record a file becoming live again without re-writing its payload bytes.
  pub fn record_file_restore(&self, size: u64) {
    self.record_write(0);
    self.increment_files();
    self.add_logical_data_size(size);
  }

  pub fn record_directory_create(&self) {
    self.record_write(0);
    self.increment_directories();
  }

  pub fn record_directory_delete(&self) {
    self.record_write(0);
    self.decrement_directories();
  }

  pub fn record_symlink_write(&self, existed: bool) {
    self.record_write(0);
    if !existed {
      self.increment_symlinks();
    }
  }

  pub fn record_symlink_delete(&self) {
    self.record_write(0);
    self.decrement_symlinks();
  }

  // ── Dedup tracking ───────────────────────────────────────────────────

  pub fn increment_chunks_deduped(&self) {
    self.chunks_deduped_total.fetch_add(1, Ordering::Relaxed);
  }

  // ── Write buffer ─────────────────────────────────────────────────────

  pub fn set_write_buffer_depth(&self, depth: u64) {
    self.write_buffer_depth.store(depth, Ordering::Relaxed);
  }

  pub fn set_void_stats(&self, count: u64, total_bytes: u64) {
    self.void_count.store(count, Ordering::Relaxed);
    self.void_space.store(total_bytes, Ordering::Relaxed);
  }

  // ── Snapshot / reconcile ─────────────────────────────────────────────

  /// Capture a point-in-time snapshot of all counter values.
  /// Used by the stats API and metrics pulse for serialization.
  pub fn snapshot(&self) -> CountersSnapshot {
    CountersSnapshot {
      files: self.files.load(Ordering::Relaxed),
      directories: self.directories.load(Ordering::Relaxed),
      symlinks: self.symlinks.load(Ordering::Relaxed),
      chunks: self.chunks.load(Ordering::Relaxed),
      snapshots: self.snapshots.load(Ordering::Relaxed),
      forks: self.forks.load(Ordering::Relaxed),
      logical_data_size: self.logical_data_size.load(Ordering::Relaxed),
      chunk_data_size: self.chunk_data_size.load(Ordering::Relaxed),
      void_space: self.void_space.load(Ordering::Relaxed),
      writes_total: self.writes_total.load(Ordering::Relaxed),
      reads_total: self.reads_total.load(Ordering::Relaxed),
      bytes_written_total: self.bytes_written_total.load(Ordering::Relaxed),
      bytes_read_total: self.bytes_read_total.load(Ordering::Relaxed),
      chunks_deduped_total: self.chunks_deduped_total.load(Ordering::Relaxed),
      write_buffer_depth: self.write_buffer_depth.load(Ordering::Relaxed),
      void_count: self.void_count.load(Ordering::Relaxed),
    }
  }

  /// Overwrite all count and size atomics from authoritative values.
  /// Called by GC after sweep to reconcile counters with ground truth.
  /// Throughput counters (writes_total, reads_total, bytes_written_total,
  /// bytes_read_total, chunks_deduped_total) are NOT reconciled because
  /// they are monotonic and not derivable from a KV scan.
  pub fn reconcile(&self, snapshot: &CountersSnapshot) {
    self.files.store(snapshot.files, Ordering::Relaxed);
    self.directories.store(snapshot.directories, Ordering::Relaxed);
    self.symlinks.store(snapshot.symlinks, Ordering::Relaxed);
    self.chunks.store(snapshot.chunks, Ordering::Relaxed);
    self.snapshots.store(snapshot.snapshots, Ordering::Relaxed);
    self.forks.store(snapshot.forks, Ordering::Relaxed);
    self.logical_data_size.store(snapshot.logical_data_size, Ordering::Relaxed);
    self.chunk_data_size.store(snapshot.chunk_data_size, Ordering::Relaxed);
    self.void_space.store(snapshot.void_space, Ordering::Relaxed);
    self.void_count.store(snapshot.void_count, Ordering::Relaxed);
    self.write_buffer_depth.store(snapshot.write_buffer_depth, Ordering::Relaxed);
  }

  /// Create a new EngineCounters by scanning the KV snapshot once.
  ///
  /// This is the one-time O(n) startup cost. Counts entries by type,
  /// sums file sizes by reading file records from the WAL, and captures
  /// void space from the void manager.
  pub fn initialize_from_kv(engine: &StorageEngine) -> Self {
    let counters = EngineCounters::new();

    let kv_snapshot = engine.kv_snapshot.load();
    let all_entries = kv_snapshot.iter_all().unwrap_or_default();
    let hash_length = engine.hash_algo().hash_length();

    // Reading every FileRecord and Chunk payload off the WAL just to sum
    // logical_data_size and chunk_data_size is multi-GB of disk I/O for
    // a real DB. Gate the size accumulation on AEORDB_INIT_COUNTERS_FULL
    // for callers that need accurate sizes at startup; default skip.
    let accumulate_sizes = std::env::var("AEORDB_INIT_COUNTERS_FULL").map(|v| !v.is_empty()).unwrap_or(false);

    let mut logical_size: u64 = 0;
    let mut chunk_size: u64 = 0;

    for entry in &all_entries {
      match entry.entry_type() {
        KV_TYPE_FILE_RECORD => {
          // Note: KV_TYPE_FILE_RECORD entries include every historical
          // revision (each FileRecord write is content-addressed and
          // creates a new KV entry). The `files` counter tracks LIVE
          // files — set below from the HEAD tree walker instead.
          if accumulate_sizes {
            if let Ok(Some((_header, _key, value))) = engine.get_entry(&entry.hash) {
              if let Ok(record) = FileRecord::deserialize(&value, hash_length, 0) {
                logical_size += record.total_size;
              }
            }
          }
        }
        KV_TYPE_DIRECTORY => {
          // Same as FileRecord: every directory mutation creates a
          // new KV entry (parent + ancestors all change). Live count
          // comes from the HEAD walker below.
        }
        KV_TYPE_SYMLINK => {
          counters.symlinks.fetch_add(1, Ordering::Relaxed);
        }
        KV_TYPE_CHUNK => {
          counters.chunks.fetch_add(1, Ordering::Relaxed);
          if accumulate_sizes {
            if let Ok(Some((_header, _key, value))) = engine.get_entry(&entry.hash) {
              chunk_size += value.len() as u64;
            }
          }
        }
        KV_TYPE_SNAPSHOT => {
          counters.snapshots.fetch_add(1, Ordering::Relaxed);
        }
        KV_TYPE_FORK => {
          counters.forks.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
      }
    }

    // Files + directories are LIVE counts (reachable from HEAD), not
    // total KV-entry counts. Walks the current tree once at startup;
    // runtime increment_files/decrement_files keep it in sync after.
    match crate::engine::directory_listing::count_live_tree(engine) {
      Ok((live_files, live_dirs)) => {
        counters.files.store(live_files, Ordering::Relaxed);
        counters.directories.store(live_dirs, Ordering::Relaxed);
      }
      Err(err) => {
        tracing::warn!(
            error = %err,
            "count_live_tree failed at startup; live file/dir counts default to 0"
        );
      }
    }

    counters.logical_data_size.store(logical_size, Ordering::Relaxed);
    counters.chunk_data_size.store(chunk_size, Ordering::Relaxed);

    // Capture void space from the void manager
    if let Ok(void_manager) = engine.void_manager.read() {
      counters.void_space.store(void_manager.total_void_space(), Ordering::Relaxed);
    }

    counters
  }
}

/// Saturating subtraction for AtomicU64: if `current < amount`, stores 0
/// instead of wrapping around to u64::MAX.
fn saturating_sub(atomic: &AtomicU64, amount: u64) {
  let mut current = atomic.load(Ordering::Relaxed);
  loop {
    let new_value = current.saturating_sub(amount);
    match atomic.compare_exchange_weak(current, new_value, Ordering::Relaxed, Ordering::Relaxed) {
      Ok(_) => break,
      Err(actual) => current = actual,
    }
  }
}
