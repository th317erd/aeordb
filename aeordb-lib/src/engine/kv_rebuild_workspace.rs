use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use crate::engine::directory_ops::{directory_path_hash, file_path_hash};
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::kv_store::{KVEntry, KV_FLAG_DELETED, KV_TYPE_DIRECTORY};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::path_utils::normalize_path;
use crate::engine::symlink_record::symlink_path_hash;

const RUN_MAGIC: &[u8; 8] = b"AEORKVR1";
const RUN_VERSION: u16 = 1;
const RUN_HEADER_LENGTH: usize = 32;
const RUN_RECORD_FIXED_LENGTH: usize = 28;
const RUN_RECORD_CRC_LENGTH: usize = 4;
const MAX_HASH_LENGTH: usize = 64;
const DEFAULT_RECORD_BUFFER_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_MERGE_FANOUT: usize = 8;
const RUN_IO_BUFFER_BYTES: usize = 64 * 1024;
const WORKSPACE_MEMORY_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RebuildOrder {
  pub(crate) timestamp: i64,
  pub(crate) offset: u64,
}

impl RebuildOrder {
  pub(crate) fn is_after(self, other: Self) -> bool {
    (self.timestamp, self.offset) > (other.timestamp, other.offset)
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedKvRecord {
  pub(crate) type_flags: u8,
  pub(crate) hash: Vec<u8>,
  pub(crate) offset: u64,
  pub(crate) value_length: u32,
  pub(crate) total_length: u32,
  pub(crate) order: RebuildOrder,
}

impl ResolvedKvRecord {
  pub(crate) fn is_deleted(&self) -> bool {
    self.type_flags & KV_FLAG_DELETED != 0
  }

  pub(crate) fn to_kv_entry(&self) -> KVEntry {
    KVEntry { type_flags: self.type_flags, hash: self.hash.clone(), offset: self.offset, total_length: self.total_length }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceAction {
  Value = 0,
  Delete = 1,
}

impl WorkspaceAction {
  fn from_u8(value: u8) -> EngineResult<Self> {
    match value {
      0 => Ok(Self::Value),
      1 => Ok(Self::Delete),
      _ => Err(scratch_corruption(format!("unknown rebuild action {value}"))),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkspaceRecord {
  action: WorkspaceAction,
  type_flags: u8,
  hash: [u8; MAX_HASH_LENGTH],
  hash_length: u8,
  offset: u64,
  value_length: u32,
  total_length: u32,
  order: RebuildOrder,
}

impl WorkspaceRecord {
  fn value(type_flags: u8, hash: &[u8], offset: u64, value_length: u32, total_length: u32, order: RebuildOrder) -> EngineResult<Self> {
    Self::new(WorkspaceAction::Value, type_flags, hash, offset, value_length, total_length, order)
  }

  fn deletion(hash: &[u8], order: RebuildOrder) -> EngineResult<Self> {
    Self::new(WorkspaceAction::Delete, 0, hash, 0, 0, 0, order)
  }

  fn new(
    action: WorkspaceAction,
    type_flags: u8,
    hash: &[u8],
    offset: u64,
    value_length: u32,
    total_length: u32,
    order: RebuildOrder,
  ) -> EngineResult<Self> {
    if hash.is_empty() || hash.len() > MAX_HASH_LENGTH {
      return Err(EngineError::InvalidInput(format!("rebuild workspace hash length {} is unsupported", hash.len())));
    }
    let mut stored_hash = [0u8; MAX_HASH_LENGTH];
    stored_hash[..hash.len()].copy_from_slice(hash);
    Ok(Self { action, type_flags, hash: stored_hash, hash_length: hash.len() as u8, offset, value_length, total_length, order })
  }

  fn hash(&self) -> &[u8] {
    &self.hash[..self.hash_length as usize]
  }

  fn sort_cmp(&self, other: &Self) -> Ordering {
    self
      .hash()
      .cmp(other.hash())
      .then_with(|| (self.action as u8).cmp(&(other.action as u8)))
      .then_with(|| self.order.timestamp.cmp(&other.order.timestamp))
      .then_with(|| self.order.offset.cmp(&other.order.offset))
      .then_with(|| self.type_flags.cmp(&other.type_flags))
      .then_with(|| self.value_length.cmp(&other.value_length))
      .then_with(|| self.total_length.cmp(&other.total_length))
  }
}

#[derive(Debug)]
struct RunFile {
  path: PathBuf,
  record_count: u64,
}

pub(crate) struct KvRebuildWorkspace {
  directory: tempfile::TempDir,
  hash_algo: HashAlgorithm,
  hash_length: usize,
  record_length: usize,
  records: Vec<WorkspaceRecord>,
  record_capacity: usize,
  merge_fanout: usize,
  levels: Vec<Vec<RunFile>>,
  next_run_id: u64,
  raw_record_count: u64,
  final_run: Option<RunFile>,
  resolved_record_count: Option<u64>,
  cancellation: Option<Arc<AtomicBool>>,
  _memory: Option<MemoryReservation>,
}

impl KvRebuildWorkspace {
  pub(crate) fn new(
    database_path: &Path,
    hash_algo: HashAlgorithm,
    memory_coordinator: Option<&MemoryCoordinator>,
    admission_class: AdmissionClass,
    cancellation: Option<Arc<AtomicBool>>,
  ) -> EngineResult<Self> {
    Self::new_for_purpose(database_path, "rebuild", hash_algo, memory_coordinator, admission_class, cancellation)
  }

  pub(crate) fn new_for_purpose(
    database_path: &Path,
    purpose: &str,
    hash_algo: HashAlgorithm,
    memory_coordinator: Option<&MemoryCoordinator>,
    admission_class: AdmissionClass,
    cancellation: Option<Arc<AtomicBool>>,
  ) -> EngineResult<Self> {
    let record_capacity = (DEFAULT_RECORD_BUFFER_BYTES / std::mem::size_of::<WorkspaceRecord>()).max(1);
    Self::new_with_limits_for_purpose(
      database_path,
      purpose,
      hash_algo,
      memory_coordinator,
      admission_class,
      record_capacity,
      DEFAULT_MERGE_FANOUT,
      cancellation,
    )
  }

  #[cfg(test)]
  fn new_with_limits(
    database_path: &Path,
    hash_algo: HashAlgorithm,
    memory_coordinator: Option<&MemoryCoordinator>,
    admission_class: AdmissionClass,
    record_capacity: usize,
    merge_fanout: usize,
  ) -> EngineResult<Self> {
    Self::new_with_limits_for_purpose(
      database_path,
      "rebuild",
      hash_algo,
      memory_coordinator,
      admission_class,
      record_capacity,
      merge_fanout,
      None,
    )
  }

  #[allow(clippy::too_many_arguments)]
  #[cfg(test)]
  fn new_with_limits_and_cancellation(
    database_path: &Path,
    hash_algo: HashAlgorithm,
    memory_coordinator: Option<&MemoryCoordinator>,
    admission_class: AdmissionClass,
    record_capacity: usize,
    merge_fanout: usize,
    cancellation: Option<Arc<AtomicBool>>,
  ) -> EngineResult<Self> {
    Self::new_with_limits_for_purpose(
      database_path,
      "rebuild",
      hash_algo,
      memory_coordinator,
      admission_class,
      record_capacity,
      merge_fanout,
      cancellation,
    )
  }

  #[allow(clippy::too_many_arguments)]
  fn new_with_limits_for_purpose(
    database_path: &Path,
    purpose: &str,
    hash_algo: HashAlgorithm,
    memory_coordinator: Option<&MemoryCoordinator>,
    admission_class: AdmissionClass,
    record_capacity: usize,
    merge_fanout: usize,
    cancellation: Option<Arc<AtomicBool>>,
  ) -> EngineResult<Self> {
    if record_capacity == 0 || merge_fanout < 2 {
      return Err(EngineError::InvalidInput("rebuild workspace requires a nonzero record window and fanout of at least two".to_string()));
    }
    if purpose.is_empty()
      || purpose.len() > 32
      || !purpose.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
      return Err(EngineError::InvalidInput(format!("invalid rebuild workspace purpose {purpose:?}")));
    }
    let parent = database_path.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let prefix = workspace_prefix_for_purpose(database_path, purpose);
    cleanup_stale_workspaces(parent, &prefix)?;
    let directory = tempfile::Builder::new().prefix(&prefix).tempdir_in(parent)?;
    let memory = memory_coordinator
      .map(|coordinator| coordinator.reserve(MemoryOwner::Repair, WORKSPACE_MEMORY_BYTES, admission_class))
      .transpose()
      .map_err(workspace_memory_error)?;
    let mut records = Vec::new();
    records
      .try_reserve_exact(record_capacity)
      .map_err(|error| EngineError::ResourceExhausted(format!("rebuild workspace record allocation failed: {error}")))?;
    let hash_length = hash_algo.hash_length();
    let record_length = RUN_RECORD_FIXED_LENGTH
      .checked_add(hash_length)
      .and_then(|length| length.checked_add(RUN_RECORD_CRC_LENGTH))
      .ok_or_else(|| EngineError::InvalidInput("rebuild workspace record length overflow".to_string()))?;
    Ok(Self {
      directory,
      hash_algo,
      hash_length,
      record_length,
      records,
      record_capacity,
      merge_fanout,
      levels: Vec::new(),
      next_run_id: 0,
      raw_record_count: 0,
      final_run: None,
      resolved_record_count: None,
      cancellation,
      _memory: memory,
    })
  }

  pub(crate) fn push_value(
    &mut self,
    type_flags: u8,
    hash: &[u8],
    offset: u64,
    value_length: u32,
    total_length: u32,
    order: RebuildOrder,
  ) -> EngineResult<()> {
    self.push_record(WorkspaceRecord::value(type_flags, hash, offset, value_length, total_length, order)?)
  }

  pub(crate) fn push_deletion_path(&mut self, path: &str, order: RebuildOrder) -> EngineResult<()> {
    let normalized = normalize_path(path);
    for hash in [
      file_path_hash(&normalized, &self.hash_algo)?,
      directory_path_hash(&normalized, &self.hash_algo)?,
      symlink_path_hash(&normalized, &self.hash_algo)?,
      self.hash_algo.compute_hash(path.as_bytes())?,
    ] {
      self.push_record(WorkspaceRecord::deletion(&hash, order)?)?;
    }
    Ok(())
  }

  fn push_record(&mut self, record: WorkspaceRecord) -> EngineResult<()> {
    self.check_cancelled()?;
    if record.hash_length as usize != self.hash_length {
      return Err(EngineError::InvalidInput(format!(
        "rebuild workspace expected {}-byte hashes, got {}",
        self.hash_length, record.hash_length
      )));
    }
    if self.final_run.is_some() {
      return Err(EngineError::InvalidInput("cannot append to a finalized rebuild workspace".to_string()));
    }
    self.records.push(record);
    self.raw_record_count = self
      .raw_record_count
      .checked_add(1)
      .ok_or_else(|| EngineError::ResourceExhausted("rebuild workspace record count overflow".to_string()))?;
    if self.records.len() >= self.record_capacity {
      self.flush_record_window()?;
    }
    Ok(())
  }

  pub(crate) fn finish(&mut self) -> EngineResult<()> {
    self.check_cancelled()?;
    if self.final_run.is_some() {
      return Ok(());
    }
    self.flush_record_window()?;
    self.records = Vec::new();

    let mut consolidated = Vec::new();
    for level in std::mem::take(&mut self.levels) {
      if level.is_empty() {
        continue;
      }
      consolidated.push(self.merge_runs(level)?);
    }
    while consolidated.len() > 1 {
      self.check_cancelled()?;
      let take = consolidated.len().min(self.merge_fanout);
      let group: Vec<RunFile> = consolidated.drain(..take).collect();
      consolidated.push(self.merge_runs(group)?);
    }
    let final_run = match consolidated.pop() {
      Some(run) => run,
      None => self.write_run(&[])?,
    };
    validate_run(&final_run, self.hash_algo, self.hash_length, self.record_length)?;
    self.final_run = Some(final_run);
    self.resolved_record_count = Some(self.count_resolved_records()?);
    Ok(())
  }

  pub(crate) fn raw_record_count(&self) -> u64 {
    self.raw_record_count
  }

  pub(crate) fn resolved_record_count(&self) -> EngineResult<u64> {
    self
      .resolved_record_count
      .ok_or_else(|| EngineError::InvalidInput("rebuild workspace must be finalized before reading its resolved count".to_string()))
  }

  pub(crate) fn visit_resolved<F>(&self, mut visitor: F) -> EngineResult<()>
  where
    F: FnMut(ResolvedKvRecord) -> EngineResult<()>,
  {
    let mut cursor = self.resolved_cursor()?;
    while let Some(record) = cursor.next_record()? {
      visitor(record)?;
    }
    Ok(())
  }

  pub(crate) fn resolved_cursor(&self) -> EngineResult<ResolvedRecordCursor> {
    let final_run = self
      .final_run
      .as_ref()
      .ok_or_else(|| EngineError::InvalidInput("rebuild workspace must be finalized before iteration".to_string()))?;
    Ok(ResolvedRecordCursor {
      reader: RunReader::open(final_run, self.hash_algo, self.hash_length, self.record_length)?,
      pending: None,
      cancellation: self.cancellation.clone(),
    })
  }

  fn count_resolved_records(&self) -> EngineResult<u64> {
    let mut count = 0u64;
    self.visit_resolved(|_| {
      count = count.checked_add(1).ok_or_else(|| EngineError::ResourceExhausted("rebuild resolved record count overflow".to_string()))?;
      Ok(())
    })?;
    Ok(count)
  }

  fn flush_record_window(&mut self) -> EngineResult<()> {
    self.check_cancelled()?;
    if self.records.is_empty() {
      return Ok(());
    }
    self.records.sort_unstable_by(WorkspaceRecord::sort_cmp);
    let records = std::mem::take(&mut self.records);
    let run = self.write_run(&records)?;
    self.records = Vec::new();
    self
      .records
      .try_reserve_exact(self.record_capacity)
      .map_err(|error| EngineError::ResourceExhausted(format!("rebuild workspace record allocation failed: {error}")))?;
    self.add_run(0, run)
  }

  fn add_run(&mut self, level: usize, run: RunFile) -> EngineResult<()> {
    if self.levels.len() <= level {
      self.levels.resize_with(level + 1, Vec::new);
    }
    self.levels[level].push(run);
    if self.levels[level].len() < self.merge_fanout {
      return Ok(());
    }
    let group = std::mem::take(&mut self.levels[level]);
    let merged = self.merge_runs(group)?;
    self.add_run(level + 1, merged)
  }

  fn write_run(&mut self, records: &[WorkspaceRecord]) -> EngineResult<RunFile> {
    let path = self.next_run_path()?;
    let record_count =
      u64::try_from(records.len()).map_err(|_| EngineError::ResourceExhausted("rebuild run record count exceeds u64".to_string()))?;
    let mut writer = RunWriter::create(&path, self.hash_algo, self.hash_length, self.record_length, record_count)?;
    for record in records {
      writer.write_record(record)?;
    }
    writer.finish()?;
    Ok(RunFile { path, record_count })
  }

  fn merge_runs(&mut self, runs: Vec<RunFile>) -> EngineResult<RunFile> {
    self.check_cancelled()?;
    if runs.len() == 1 {
      return Ok(runs.into_iter().next().expect("one run"));
    }
    let output_count = runs.iter().try_fold(0u64, |count, run| {
      count.checked_add(run.record_count).ok_or_else(|| EngineError::ResourceExhausted("rebuild merge record count overflow".to_string()))
    })?;
    let output_path = self.next_run_path()?;
    let mut readers = runs
      .iter()
      .map(|run| RunReader::open(run, self.hash_algo, self.hash_length, self.record_length))
      .collect::<EngineResult<Vec<_>>>()?;
    let mut heap = BinaryHeap::new();
    for (reader_index, reader) in readers.iter_mut().enumerate() {
      if let Some(record) = reader.next_record()? {
        heap.push(HeapRecord { record, reader_index });
      }
    }
    let mut writer = RunWriter::create(&output_path, self.hash_algo, self.hash_length, self.record_length, output_count)?;
    let mut merged_count = 0u64;
    while let Some(item) = heap.pop() {
      if merged_count.is_multiple_of(4_096) {
        self.check_cancelled()?;
      }
      writer.write_record(&item.record)?;
      merged_count = merged_count.saturating_add(1);
      if let Some(record) = readers[item.reader_index].next_record()? {
        heap.push(HeapRecord { record, reader_index: item.reader_index });
      }
    }
    writer.finish()?;
    for run in runs {
      std::fs::remove_file(run.path)?;
    }
    Ok(RunFile { path: output_path, record_count: output_count })
  }

  fn next_run_path(&mut self) -> EngineResult<PathBuf> {
    let run_id = self.next_run_id;
    self.next_run_id =
      self.next_run_id.checked_add(1).ok_or_else(|| EngineError::ResourceExhausted("rebuild workspace run ID overflow".to_string()))?;
    Ok(self.directory.path().join(format!("run-{run_id:016x}.bin")))
  }

  fn check_cancelled(&self) -> EngineResult<()> {
    if self.cancellation.as_ref().is_some_and(|cancelled| cancelled.load(AtomicOrdering::Acquire)) {
      return Err(EngineError::ShuttingDown);
    }
    Ok(())
  }
}

#[cfg(test)]
fn workspace_prefix(database_path: &Path) -> String {
  workspace_prefix_for_purpose(database_path, "rebuild")
}

fn workspace_prefix_for_purpose(database_path: &Path, purpose: &str) -> String {
  let identity = blake3::hash(database_path.to_string_lossy().as_bytes());
  if purpose == "rebuild" {
    format!(".aeordb-rebuild-{}-", &identity.to_hex()[..16])
  } else {
    format!(".aeordb-rebuild-{}-{purpose}-", &identity.to_hex()[..16])
  }
}

/// The caller owns AeorDB's exclusive database lock before constructing a
/// workspace, so any prior directory for this database identity is a crash
/// remnant rather than a live peer. Refuse non-directory lookalikes instead of
/// following or deleting an attacker-controlled symlink.
fn cleanup_stale_workspaces(parent: &Path, prefix: &str) -> EngineResult<()> {
  for entry in std::fs::read_dir(parent)? {
    let entry = entry?;
    if !entry.file_name().to_string_lossy().starts_with(prefix) {
      continue;
    }
    let metadata = std::fs::symlink_metadata(entry.path())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
      return Err(EngineError::InvalidInput(format!(
        "suspicious rebuild workspace artifact is not a private directory: {}",
        entry.path().display()
      )));
    }
    std::fs::remove_dir_all(entry.path())?;
  }
  Ok(())
}

fn should_replace_value(existing: &WorkspaceRecord, candidate: &WorkspaceRecord) -> bool {
  let existing_type = existing.type_flags & 0x0f;
  let candidate_type = candidate.type_flags & 0x0f;
  if existing_type == KV_TYPE_DIRECTORY && candidate_type == KV_TYPE_DIRECTORY {
    if candidate.value_length == 0 && existing.value_length > 0 {
      return false;
    }
    if candidate.value_length > 0 && existing.value_length == 0 {
      return true;
    }
  }
  candidate.order.is_after(existing.order)
}

fn resolve_record(selected: Option<WorkspaceRecord>, deletion: Option<RebuildOrder>) -> Option<ResolvedKvRecord> {
  let mut selected = selected?;
  if deletion.is_some_and(|order| order.is_after(selected.order)) {
    selected.type_flags = (selected.type_flags & 0x0f) | KV_FLAG_DELETED;
  }
  Some(ResolvedKvRecord {
    type_flags: selected.type_flags,
    hash: selected.hash().to_vec(),
    offset: selected.offset,
    value_length: selected.value_length,
    total_length: selected.total_length,
    order: selected.order,
  })
}

pub(crate) struct ResolvedRecordCursor {
  reader: RunReader,
  pending: Option<WorkspaceRecord>,
  cancellation: Option<Arc<AtomicBool>>,
}

impl ResolvedRecordCursor {
  pub(crate) fn next_record(&mut self) -> EngineResult<Option<ResolvedKvRecord>> {
    loop {
      self.check_cancelled()?;
      let first = match self.pending.take() {
        Some(record) => Some(record),
        None => self.reader.next_record()?,
      };
      let Some(first) = first else {
        return Ok(None);
      };
      let group_hash = first.hash().to_vec();
      let mut selected = None;
      let mut latest_deletion = None;
      update_resolved_group(first, &mut selected, &mut latest_deletion);

      loop {
        self.check_cancelled()?;
        match self.reader.next_record()? {
          Some(record) if record.hash() == group_hash => update_resolved_group(record, &mut selected, &mut latest_deletion),
          Some(record) => {
            self.pending = Some(record);
            break;
          }
          None => break,
        }
      }

      if let Some(record) = resolve_record(selected, latest_deletion) {
        return Ok(Some(record));
      }
    }
  }

  fn check_cancelled(&self) -> EngineResult<()> {
    if self.cancellation.as_ref().is_some_and(|cancelled| cancelled.load(AtomicOrdering::Acquire)) {
      return Err(EngineError::ShuttingDown);
    }
    Ok(())
  }
}

fn update_resolved_group(record: WorkspaceRecord, selected: &mut Option<WorkspaceRecord>, latest_deletion: &mut Option<RebuildOrder>) {
  match record.action {
    WorkspaceAction::Value => {
      if selected.as_ref().map(|existing| should_replace_value(existing, &record)).unwrap_or(true) {
        *selected = Some(record);
      }
    }
    WorkspaceAction::Delete => {
      if latest_deletion.map(|existing| record.order.is_after(existing)).unwrap_or(true) {
        *latest_deletion = Some(record.order);
      }
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeapRecord {
  record: WorkspaceRecord,
  reader_index: usize,
}

impl Ord for HeapRecord {
  fn cmp(&self, other: &Self) -> Ordering {
    other.record.sort_cmp(&self.record).then_with(|| other.reader_index.cmp(&self.reader_index))
  }
}

impl PartialOrd for HeapRecord {
  fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

struct RunWriter {
  writer: BufWriter<File>,
  hash_length: usize,
  record_length: usize,
  expected_count: u64,
  written_count: u64,
}

impl RunWriter {
  fn create(path: &Path, hash_algo: HashAlgorithm, hash_length: usize, record_length: usize, record_count: u64) -> EngineResult<Self> {
    let file = OpenOptions::new().create_new(true).write(true).open(path)?;
    let mut writer = BufWriter::with_capacity(RUN_IO_BUFFER_BYTES, file);
    writer.write_all(&encode_run_header(hash_algo, hash_length, record_length, record_count)?)?;
    Ok(Self { writer, hash_length, record_length, expected_count: record_count, written_count: 0 })
  }

  fn write_record(&mut self, record: &WorkspaceRecord) -> EngineResult<()> {
    if record.hash_length as usize != self.hash_length {
      return Err(EngineError::InvalidInput("rebuild run record hash length changed while writing".to_string()));
    }
    let mut bytes = [0u8; RUN_RECORD_FIXED_LENGTH + MAX_HASH_LENGTH + RUN_RECORD_CRC_LENGTH];
    bytes[0] = record.action as u8;
    bytes[1] = record.type_flags;
    bytes[4..12].copy_from_slice(&record.order.timestamp.to_le_bytes());
    bytes[12..20].copy_from_slice(&record.offset.to_le_bytes());
    bytes[20..24].copy_from_slice(&record.value_length.to_le_bytes());
    bytes[24..28].copy_from_slice(&record.total_length.to_le_bytes());
    bytes[28..28 + self.hash_length].copy_from_slice(record.hash());
    let checksum_offset = self.record_length - RUN_RECORD_CRC_LENGTH;
    let checksum = crc32fast::hash(&bytes[..checksum_offset]);
    bytes[checksum_offset..self.record_length].copy_from_slice(&checksum.to_le_bytes());
    self.writer.write_all(&bytes[..self.record_length])?;
    self.written_count =
      self.written_count.checked_add(1).ok_or_else(|| EngineError::ResourceExhausted("rebuild run writer count overflow".to_string()))?;
    Ok(())
  }

  fn finish(mut self) -> EngineResult<()> {
    if self.written_count != self.expected_count {
      return Err(EngineError::InvalidInput(format!(
        "rebuild run expected {} records but wrote {}",
        self.expected_count, self.written_count
      )));
    }
    self.writer.flush()?;
    Ok(())
  }
}

struct RunReader {
  reader: BufReader<File>,
  path: PathBuf,
  hash_length: usize,
  record_length: usize,
  record_count: u64,
  read_count: u64,
  finished: bool,
}

impl RunReader {
  fn open(run: &RunFile, hash_algo: HashAlgorithm, hash_length: usize, record_length: usize) -> EngineResult<Self> {
    let file = File::open(&run.path)?;
    let mut reader = BufReader::with_capacity(RUN_IO_BUFFER_BYTES, file);
    let mut header = [0u8; RUN_HEADER_LENGTH];
    reader.read_exact(&mut header).map_err(|error| scratch_io(&run.path, "header", error))?;
    let count = decode_run_header(&header, hash_algo, hash_length, record_length)?;
    if count != run.record_count {
      return Err(scratch_corruption(format!(
        "rebuild run {} count {} disagrees with manifest {}",
        run.path.display(),
        count,
        run.record_count
      )));
    }
    Ok(Self { reader, path: run.path.clone(), hash_length, record_length, record_count: count, read_count: 0, finished: false })
  }

  fn next_record(&mut self) -> EngineResult<Option<WorkspaceRecord>> {
    if self.read_count == self.record_count {
      self.verify_eof()?;
      return Ok(None);
    }
    let mut bytes = [0u8; RUN_RECORD_FIXED_LENGTH + MAX_HASH_LENGTH + RUN_RECORD_CRC_LENGTH];
    self.reader.read_exact(&mut bytes[..self.record_length]).map_err(|error| scratch_io(&self.path, "record", error))?;
    let checksum_offset = self.record_length - RUN_RECORD_CRC_LENGTH;
    let stored_checksum = u32::from_le_bytes(bytes[checksum_offset..self.record_length].try_into().expect("four-byte checksum"));
    let computed_checksum = crc32fast::hash(&bytes[..checksum_offset]);
    if stored_checksum != computed_checksum {
      return Err(scratch_corruption(format!("rebuild run {} record checksum mismatch", self.path.display())));
    }
    if bytes[2] != 0 || bytes[3] != 0 {
      return Err(scratch_corruption(format!("rebuild run {} record reserved bytes are nonzero", self.path.display())));
    }
    let action = WorkspaceAction::from_u8(bytes[0])?;
    let order = RebuildOrder {
      timestamp: i64::from_le_bytes(bytes[4..12].try_into().expect("eight-byte timestamp")),
      offset: u64::from_le_bytes(bytes[12..20].try_into().expect("eight-byte offset")),
    };
    let value_length = u32::from_le_bytes(bytes[20..24].try_into().expect("four-byte value length"));
    let total_length = u32::from_le_bytes(bytes[24..28].try_into().expect("four-byte total length"));
    let record =
      WorkspaceRecord::new(action, bytes[1], &bytes[28..28 + self.hash_length], order.offset, value_length, total_length, order)?;
    self.read_count += 1;
    Ok(Some(record))
  }

  fn verify_eof(&mut self) -> EngineResult<()> {
    if self.finished {
      return Ok(());
    }
    let mut trailing = [0u8; 1];
    match self.reader.read(&mut trailing) {
      Ok(0) => {
        self.finished = true;
        Ok(())
      }
      Ok(_) => Err(scratch_corruption(format!("rebuild run {} has trailing bytes", self.path.display()))),
      Err(error) => Err(scratch_io(&self.path, "trailing-byte check", error)),
    }
  }
}

fn validate_run(run: &RunFile, hash_algo: HashAlgorithm, hash_length: usize, record_length: usize) -> EngineResult<()> {
  let mut reader = RunReader::open(run, hash_algo, hash_length, record_length)?;
  while reader.next_record()?.is_some() {}
  Ok(())
}

fn encode_run_header(
  hash_algo: HashAlgorithm,
  hash_length: usize,
  record_length: usize,
  record_count: u64,
) -> EngineResult<[u8; RUN_HEADER_LENGTH]> {
  let hash_length = u16::try_from(hash_length).map_err(|_| EngineError::InvalidInput("rebuild hash length exceeds u16".to_string()))?;
  let record_length =
    u16::try_from(record_length).map_err(|_| EngineError::InvalidInput("rebuild record length exceeds u16".to_string()))?;
  let mut header = [0u8; RUN_HEADER_LENGTH];
  header[..8].copy_from_slice(RUN_MAGIC);
  header[8..10].copy_from_slice(&RUN_VERSION.to_le_bytes());
  header[10..12].copy_from_slice(&hash_algo.to_u16().to_le_bytes());
  header[12..14].copy_from_slice(&hash_length.to_le_bytes());
  header[14..16].copy_from_slice(&record_length.to_le_bytes());
  header[16..24].copy_from_slice(&record_count.to_le_bytes());
  let checksum = crc32fast::hash(&header[..28]);
  header[28..32].copy_from_slice(&checksum.to_le_bytes());
  Ok(header)
}

fn decode_run_header(
  header: &[u8; RUN_HEADER_LENGTH],
  hash_algo: HashAlgorithm,
  hash_length: usize,
  record_length: usize,
) -> EngineResult<u64> {
  let stored_checksum = u32::from_le_bytes(header[28..32].try_into().expect("four-byte checksum"));
  if &header[..8] != RUN_MAGIC || stored_checksum != crc32fast::hash(&header[..28]) {
    return Err(scratch_corruption("rebuild run header magic or checksum is invalid".to_string()));
  }
  let version = u16::from_le_bytes(header[8..10].try_into().expect("two-byte version"));
  let stored_algo = u16::from_le_bytes(header[10..12].try_into().expect("two-byte algorithm"));
  let stored_hash_length = u16::from_le_bytes(header[12..14].try_into().expect("two-byte hash length")) as usize;
  let stored_record_length = u16::from_le_bytes(header[14..16].try_into().expect("two-byte record length")) as usize;
  if version != RUN_VERSION
    || stored_algo != hash_algo.to_u16()
    || stored_hash_length != hash_length
    || stored_record_length != record_length
    || header[24..28] != [0u8; 4]
  {
    return Err(scratch_corruption("rebuild run header contract mismatch".to_string()));
  }
  Ok(u64::from_le_bytes(header[16..24].try_into().expect("eight-byte record count")))
}

fn scratch_io(path: &Path, operation: &str, error: std::io::Error) -> EngineError {
  EngineError::IoError(std::io::Error::new(error.kind(), format!("rebuild scratch {} failed for {}: {error}", operation, path.display())))
}

fn scratch_corruption(reason: String) -> EngineError {
  EngineError::CorruptEntry { offset: 0, reason }
}

fn workspace_memory_error(error: MemoryCoordinatorError) -> EngineError {
  match error {
    MemoryCoordinatorError::PolicyUnavailable
    | MemoryCoordinatorError::HardLimitExceeded { .. }
    | MemoryCoordinatorError::SoftPressureDeferred { .. }
    | MemoryCoordinatorError::EmergencyReserveExceeded { .. } => {
      EngineError::ResourceExhausted(format!("rebuild workspace memory admission failed: {error}"))
    }
    _ => EngineError::IoError(std::io::Error::other(format!("rebuild workspace memory admission failed: {error}"))),
  }
}

#[cfg(test)]
mod tests {
  use std::io::{Seek, SeekFrom};

  use super::*;

  fn workspace(temp: &tempfile::TempDir) -> KvRebuildWorkspace {
    let database_path = temp.path().join("test.aeordb");
    File::create(&database_path).unwrap();
    KvRebuildWorkspace::new_with_limits(&database_path, HashAlgorithm::Blake3_256, None, AdmissionClass::Maintenance, 3, 2).unwrap()
  }

  fn hash(byte: u8) -> Vec<u8> {
    vec![byte; HashAlgorithm::Blake3_256.hash_length()]
  }

  #[test]
  fn external_runs_resolve_latest_values_across_multiple_merge_levels() {
    let temp = tempfile::tempdir().unwrap();
    let mut workspace = workspace(&temp);
    for index in (0..25u8).rev() {
      workspace.push_value(1, &hash(index), index as u64, 10, 20, RebuildOrder { timestamp: index as i64, offset: index as u64 }).unwrap();
    }
    workspace.push_value(1, &hash(7), 700, 11, 21, RebuildOrder { timestamp: 700, offset: 700 }).unwrap();
    workspace.finish().unwrap();

    assert_eq!(workspace.raw_record_count(), 26);
    assert_eq!(workspace.resolved_record_count().unwrap(), 25);
    let mut resolved = Vec::new();
    workspace
      .visit_resolved(|record| {
        resolved.push(record);
        Ok(())
      })
      .unwrap();
    assert!(resolved.windows(2).all(|pair| pair[0].hash < pair[1].hash));
    assert_eq!(resolved.iter().find(|record| record.hash == hash(7)).unwrap().offset, 700);
  }

  #[test]
  fn directory_nonempty_preference_and_deletion_order_match_legacy_rebuild() {
    let temp = tempfile::tempdir().unwrap();
    let mut workspace = workspace(&temp);
    let directory_hash = hash(1);
    workspace.push_value(KV_TYPE_DIRECTORY, &directory_hash, 10, 100, 120, RebuildOrder { timestamp: 10, offset: 10 }).unwrap();
    workspace.push_value(KV_TYPE_DIRECTORY, &directory_hash, 20, 0, 20, RebuildOrder { timestamp: 20, offset: 20 }).unwrap();
    let deleted_hash = file_path_hash("/deleted", &HashAlgorithm::Blake3_256).unwrap();
    workspace.push_value(1, &deleted_hash, 30, 10, 20, RebuildOrder { timestamp: 30, offset: 30 }).unwrap();
    workspace.push_deletion_path("/deleted", RebuildOrder { timestamp: 40, offset: 40 }).unwrap();
    let recreated_hash = file_path_hash("/recreated", &HashAlgorithm::Blake3_256).unwrap();
    workspace.push_deletion_path("/recreated", RebuildOrder { timestamp: 50, offset: 50 }).unwrap();
    workspace.push_value(1, &recreated_hash, 60, 10, 20, RebuildOrder { timestamp: 60, offset: 60 }).unwrap();
    workspace.finish().unwrap();

    let mut resolved = Vec::new();
    workspace
      .visit_resolved(|record| {
        resolved.push(record);
        Ok(())
      })
      .unwrap();
    let directory = resolved.iter().find(|record| record.hash == directory_hash).unwrap();
    assert_eq!(directory.offset, 10, "nonempty directory wins over a newer empty rewrite");
    assert!(resolved.iter().find(|record| record.hash == deleted_hash).unwrap().is_deleted());
    assert!(!resolved.iter().find(|record| record.hash == recreated_hash).unwrap().is_deleted());
  }

  #[test]
  fn malformed_hashes_and_post_finish_appends_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let mut workspace = workspace(&temp);
    let error = workspace.push_value(1, &[1u8; 31], 1, 1, 1, RebuildOrder { timestamp: 1, offset: 1 }).unwrap_err();
    assert!(matches!(error, EngineError::InvalidInput(_)));
    workspace.finish().unwrap();
    let error = workspace.push_value(1, &hash(1), 1, 1, 1, RebuildOrder { timestamp: 1, offset: 1 }).unwrap_err();
    assert!(matches!(error, EngineError::InvalidInput(_)));
  }

  #[test]
  fn checksum_corruption_and_trailing_bytes_fail_closed() {
    for trailing in [false, true] {
      let temp = tempfile::tempdir().unwrap();
      let mut workspace = workspace(&temp);
      workspace.push_value(1, &hash(1), 1, 1, 1, RebuildOrder { timestamp: 1, offset: 1 }).unwrap();
      workspace.finish().unwrap();
      let run = workspace.final_run.as_ref().unwrap();
      let mut file = OpenOptions::new().read(true).write(true).open(&run.path).unwrap();
      if trailing {
        file.seek(SeekFrom::End(0)).unwrap();
        file.write_all(&[0xff]).unwrap();
      } else {
        file.seek(SeekFrom::Start((RUN_HEADER_LENGTH + 1) as u64)).unwrap();
        file.write_all(&[0xff]).unwrap();
      }
      file.flush().unwrap();
      let error = workspace.visit_resolved(|_| Ok(())).unwrap_err();
      assert!(matches!(error, EngineError::CorruptEntry { .. }));
    }
  }

  #[test]
  fn workspace_files_are_removed_on_drop() {
    let temp = tempfile::tempdir().unwrap();
    let path = {
      let workspace = workspace(&temp);
      workspace.directory.path().to_path_buf()
    };
    assert!(!path.exists());
  }

  #[test]
  fn memory_pressure_is_retryable_but_coordinator_failures_are_not() {
    let pressure = workspace_memory_error(MemoryCoordinatorError::PolicyUnavailable);
    assert!(matches!(pressure, EngineError::ResourceExhausted(_)));

    let accounting = workspace_memory_error(MemoryCoordinatorError::AccountingOverflow { owner: MemoryOwner::Repair });
    assert!(matches!(accounting, EngineError::IoError(_)));
  }

  #[test]
  fn construction_removes_only_stale_workspaces_for_the_same_database() {
    let temp = tempfile::tempdir().unwrap();
    let database_path = temp.path().join("test.aeordb");
    File::create(&database_path).unwrap();
    let prefix = workspace_prefix(&database_path);
    let stale = temp.path().join(format!("{prefix}stale"));
    std::fs::create_dir(&stale).unwrap();
    File::create(stale.join("run.bin")).unwrap();
    let unrelated = temp.path().join(".aeordb-rebuild-unrelated-stale");
    std::fs::create_dir(&unrelated).unwrap();

    let workspace =
      KvRebuildWorkspace::new_with_limits(&database_path, HashAlgorithm::Blake3_256, None, AdmissionClass::Maintenance, 3, 2).unwrap();

    assert!(!stale.exists());
    assert!(unrelated.exists());
    assert!(workspace.directory.path().file_name().unwrap().to_string_lossy().starts_with(&prefix));
  }

  #[test]
  fn suspicious_stale_workspace_lookalike_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let database_path = temp.path().join("test.aeordb");
    File::create(&database_path).unwrap();
    let lookalike = temp.path().join(format!("{}file", workspace_prefix(&database_path)));
    File::create(&lookalike).unwrap();

    let error = KvRebuildWorkspace::new_with_limits(&database_path, HashAlgorithm::Blake3_256, None, AdmissionClass::Maintenance, 3, 2)
      .err()
      .expect("a non-directory workspace lookalike must be rejected");

    assert!(matches!(error, EngineError::InvalidInput(_)));
    assert!(lookalike.exists());
  }

  #[test]
  fn cancellation_stops_append_and_merge_work_and_cleans_scratch() {
    let temp = tempfile::tempdir().unwrap();
    let database_path = temp.path().join("test.aeordb");
    File::create(&database_path).unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut workspace = KvRebuildWorkspace::new_with_limits_and_cancellation(
      &database_path,
      HashAlgorithm::Blake3_256,
      None,
      AdmissionClass::Maintenance,
      2,
      2,
      Some(Arc::clone(&cancelled)),
    )
    .unwrap();
    workspace.push_value(1, &hash(1), 1, 1, 1, RebuildOrder { timestamp: 1, offset: 1 }).unwrap();
    workspace.push_value(1, &hash(2), 2, 1, 1, RebuildOrder { timestamp: 2, offset: 2 }).unwrap();
    let workspace_path = workspace.directory.path().to_path_buf();

    cancelled.store(true, AtomicOrdering::Release);
    assert!(matches!(workspace.push_value(1, &hash(3), 3, 1, 1, RebuildOrder { timestamp: 3, offset: 3 }), Err(EngineError::ShuttingDown)));
    assert!(matches!(workspace.finish(), Err(EngineError::ShuttingDown)));
    drop(workspace);
    assert!(!workspace_path.exists());
  }

  #[test]
  fn independent_workspace_purposes_coexist_without_stale_cleanup_collisions() {
    let temp = tempfile::tempdir().unwrap();
    let database_path = temp.path().join("test.aeordb");
    File::create(&database_path).unwrap();
    let expected = KvRebuildWorkspace::new_for_purpose(
      &database_path,
      "verify-expected",
      HashAlgorithm::Blake3_256,
      None,
      AdmissionClass::Maintenance,
      None,
    )
    .unwrap();
    let expected_path = expected.directory.path().to_path_buf();
    let actual = KvRebuildWorkspace::new_for_purpose(
      &database_path,
      "verify-actual",
      HashAlgorithm::Blake3_256,
      None,
      AdmissionClass::Maintenance,
      None,
    )
    .unwrap();

    assert!(expected_path.exists(), "creating a separate purpose must not delete a live workspace");
    assert!(actual.directory.path().exists());
  }

  #[test]
  fn resolved_cursor_skips_deletion_only_groups_and_yields_hash_order() {
    let temp = tempfile::tempdir().unwrap();
    let mut workspace = workspace(&temp);
    workspace.push_deletion_path("/absent", RebuildOrder { timestamp: 1, offset: 1 }).unwrap();
    workspace.push_value(1, &hash(9), 90, 1, 2, RebuildOrder { timestamp: 9, offset: 90 }).unwrap();
    workspace.push_value(1, &hash(2), 20, 1, 2, RebuildOrder { timestamp: 2, offset: 20 }).unwrap();
    workspace.finish().unwrap();

    let mut cursor = workspace.resolved_cursor().unwrap();
    let first = cursor.next_record().unwrap().unwrap();
    let second = cursor.next_record().unwrap().unwrap();
    assert!(first.hash < second.hash);
    assert!(cursor.next_record().unwrap().is_none());
  }
}
