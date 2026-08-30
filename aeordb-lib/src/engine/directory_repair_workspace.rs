use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use crate::engine::directory_entry::ChildEntry;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::memory_coordinator::{
  AdmissionClass, CriticalMemoryPurpose, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation,
};
use crate::engine::path_utils::normalize_path;

const RUN_MAGIC: &[u8; 8] = b"AEORDR1\0";
const RUN_VERSION: u16 = 1;
const RUN_HEADER_BYTES: usize = 32;
const FRAME_FIXED_BYTES: usize = 12;
const FRAME_CRC_BYTES: usize = 4;
const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
const SORT_WINDOW_BYTES: usize = 4 * 1024 * 1024;
const BASE_MEMORY_BYTES: u64 = 32 * 1024 * 1024;
const RUN_IO_BUFFER_BYTES: usize = 64 * 1024;
const MERGE_FANOUT: usize = 8;
const MAX_OPEN_RAW_FILES: usize = 16;

#[derive(Debug, Clone, PartialEq)]
struct RepairRecord {
  directory: String,
  child: Option<ChildEntry>,
}

impl RepairRecord {
  fn marker(directory: &str) -> EngineResult<Self> {
    Ok(Self { directory: validate_directory_path(directory)?, child: None })
  }

  fn child(directory: &str, child: ChildEntry, hash_length: usize) -> EngineResult<Self> {
    let directory = validate_directory_path(directory)?;
    if child.name.is_empty() || child.name.contains('/') || child.hash.len() != hash_length {
      return Err(workspace_corruption(format!(
        "invalid directory-repair child name/hash for {directory}: name={:?}, hash_length={}",
        child.name,
        child.hash.len()
      )));
    }
    Ok(Self { directory, child: Some(child) })
  }

  fn sort_cmp(&self, other: &Self) -> Ordering {
    self.directory.cmp(&other.directory).then_with(|| match (&self.child, &other.child) {
      (None, None) => Ordering::Equal,
      (None, Some(_)) => Ordering::Less,
      (Some(_), None) => Ordering::Greater,
      (Some(left), Some(right)) => left
        .name
        .cmp(&right.name)
        .then_with(|| left.entry_type.cmp(&right.entry_type))
        .then_with(|| left.hash.cmp(&right.hash))
        .then_with(|| left.total_size.cmp(&right.total_size))
        .then_with(|| left.created_at.cmp(&right.created_at))
        .then_with(|| left.updated_at.cmp(&right.updated_at))
        .then_with(|| left.content_type.cmp(&right.content_type))
        .then_with(|| left.virtual_time.cmp(&right.virtual_time))
        .then_with(|| left.node_id.cmp(&right.node_id)),
    })
  }

  fn estimated_bytes(&self) -> usize {
    let child = self.child.as_ref().map_or(0, estimated_child_bytes);
    std::mem::size_of::<Self>().saturating_add(self.directory.len()).saturating_add(child)
  }
}

#[derive(Debug)]
struct RunFile {
  path: PathBuf,
  records: u64,
}

pub(crate) struct DirectoryRepairWorkspace {
  directory: tempfile::TempDir,
  hash_algo: HashAlgorithm,
  hash_length: usize,
  raw_files: BTreeMap<usize, File>,
  max_depth: usize,
  next_run_id: u64,
  cancellation: Arc<AtomicBool>,
  memory: MemoryReservation,
  group_reserved: u64,
}

impl DirectoryRepairWorkspace {
  pub(crate) fn new(
    database_path: &Path,
    hash_algo: HashAlgorithm,
    coordinator: &MemoryCoordinator,
    cancellation: Arc<AtomicBool>,
  ) -> EngineResult<Self> {
    check_cancelled(&cancellation)?;
    let memory = coordinator
      .reserve(MemoryOwner::Repair, BASE_MEMORY_BYTES, AdmissionClass::Critical(CriticalMemoryPurpose::BoundedRecovery))
      .map_err(workspace_memory_error)?;
    let parent = database_path.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let prefix = workspace_prefix(database_path);
    cleanup_stale_workspaces(parent, &prefix)?;
    let directory = tempfile::Builder::new().prefix(&prefix).tempdir_in(parent)?;
    let hash_length = hash_algo.hash_length();
    let mut workspace = Self {
      directory,
      hash_algo,
      hash_length,
      raw_files: BTreeMap::new(),
      max_depth: 0,
      next_run_id: 0,
      cancellation,
      memory,
      group_reserved: 0,
    };
    workspace.push_marker("/")?;
    Ok(workspace)
  }

  pub(crate) const fn max_depth(&self) -> usize {
    self.max_depth
  }

  pub(crate) fn push_child(&mut self, directory: &str, child: ChildEntry) -> EngineResult<()> {
    let record = RepairRecord::child(directory, child, self.hash_length)?;
    self.append_record(record)
  }

  pub(crate) fn finish_depth(&mut self, depth: usize) -> EngineResult<DirectoryRepairCursor> {
    check_cancelled(&self.cancellation)?;
    if let Some(file) = self.raw_files.remove(&depth) {
      drop(file);
    }
    let raw_path = self.raw_path(depth);
    let mut records = Vec::new();
    let mut record_bytes = 0usize;
    let mut runs = Vec::new();

    if raw_path.exists() {
      let mut reader = BufReader::with_capacity(RUN_IO_BUFFER_BYTES, File::open(&raw_path)?);
      while let Some(record) = read_frame(&mut reader, &raw_path, self.hash_length)? {
        check_cancelled(&self.cancellation)?;
        if directory_depth(&record.directory) != depth {
          return Err(workspace_corruption(format!("directory-repair record for {} appeared in depth {depth}", record.directory)));
        }
        record_bytes = record_bytes.saturating_add(record.estimated_bytes());
        records.push(record);
        if record_bytes >= SORT_WINDOW_BYTES {
          runs.push(self.write_sorted_run(&mut records)?);
          record_bytes = 0;
        }
      }
      std::fs::remove_file(&raw_path)?;
    }
    if !records.is_empty() {
      runs.push(self.write_sorted_run(&mut records)?);
    }
    if runs.is_empty() {
      runs.push(self.write_run(&[])?);
    }

    while runs.len() > 1 {
      check_cancelled(&self.cancellation)?;
      let mut next = Vec::new();
      while !runs.is_empty() {
        let take = runs.len().min(MERGE_FANOUT);
        let group: Vec<_> = runs.drain(..take).collect();
        next.push(self.merge_runs(group)?);
      }
      runs = next;
    }
    let run = runs.pop().ok_or_else(|| workspace_corruption("directory-repair run planner produced no final run".to_string()))?;
    DirectoryRepairCursor::open(run, self.hash_algo, self.hash_length, Arc::clone(&self.cancellation))
  }

  pub(crate) fn release_group(&mut self) -> EngineResult<()> {
    if self.group_reserved == 0 {
      return Ok(());
    }
    let bytes = std::mem::take(&mut self.group_reserved);
    self.memory.shrink(bytes).map_err(workspace_memory_error)
  }

  fn push_marker(&mut self, directory: &str) -> EngineResult<()> {
    self.append_record(RepairRecord::marker(directory)?)
  }

  fn append_record(&mut self, record: RepairRecord) -> EngineResult<()> {
    check_cancelled(&self.cancellation)?;
    let depth = directory_depth(&record.directory);
    self.max_depth = self.max_depth.max(depth);
    let frame = encode_frame(&record, self.hash_length)?;
    if !self.raw_files.contains_key(&depth) {
      while self.raw_files.len() >= MAX_OPEN_RAW_FILES {
        self.raw_files.pop_first();
      }
      let path = self.raw_path(depth);
      let file = OpenOptions::new().create(true).append(true).open(path)?;
      self.raw_files.insert(depth, file);
    }
    let raw_file = self
      .raw_files
      .get_mut(&depth)
      .ok_or_else(|| workspace_corruption(format!("directory-repair depth file {depth} disappeared before append")))?;
    raw_file.write_all(&frame)?;
    Ok(())
  }

  fn reserve_group(&mut self, bytes: u64) -> EngineResult<()> {
    self.memory.grow(bytes).map_err(workspace_memory_error)?;
    self.group_reserved = self
      .group_reserved
      .checked_add(bytes)
      .ok_or_else(|| EngineError::ResourceExhausted("directory-repair group accounting overflow".to_string()))?;
    Ok(())
  }

  fn raw_path(&self, depth: usize) -> PathBuf {
    self.directory.path().join(format!("depth-{depth:08x}.raw"))
  }

  fn next_run_path(&mut self) -> EngineResult<PathBuf> {
    let id = self.next_run_id;
    self.next_run_id =
      self.next_run_id.checked_add(1).ok_or_else(|| EngineError::ResourceExhausted("directory-repair run ID overflow".to_string()))?;
    Ok(self.directory.path().join(format!("run-{id:016x}.bin")))
  }

  fn write_sorted_run(&mut self, records: &mut Vec<RepairRecord>) -> EngineResult<RunFile> {
    records.sort_unstable_by(RepairRecord::sort_cmp);
    let run = self.write_run(records)?;
    records.clear();
    Ok(run)
  }

  fn write_run(&mut self, records: &[RepairRecord]) -> EngineResult<RunFile> {
    let path = self.next_run_path()?;
    let count =
      u64::try_from(records.len()).map_err(|_| EngineError::ResourceExhausted("directory-repair run count overflow".to_string()))?;
    let mut writer = RunWriter::create(&path, self.hash_algo, self.hash_length, count)?;
    for record in records {
      writer.write_record(record)?;
    }
    writer.finish()?;
    Ok(RunFile { path, records: count })
  }

  fn merge_runs(&mut self, runs: Vec<RunFile>) -> EngineResult<RunFile> {
    if runs.len() == 1 {
      return runs.into_iter().next().ok_or_else(|| workspace_corruption("directory-repair merge lost its sole input run".to_string()));
    }
    let count = runs.iter().try_fold(0u64, |total, run| {
      total.checked_add(run.records).ok_or_else(|| EngineError::ResourceExhausted("directory-repair merge count overflow".to_string()))
    })?;
    let output = self.next_run_path()?;
    let mut readers = runs.iter().map(|run| RunReader::open(run, self.hash_algo, self.hash_length)).collect::<EngineResult<Vec<_>>>()?;
    let mut heap = BinaryHeap::new();
    for (index, reader) in readers.iter_mut().enumerate() {
      if let Some(record) = reader.next_record()? {
        heap.push(HeapRecord { record, reader: index });
      }
    }
    let mut writer = RunWriter::create(&output, self.hash_algo, self.hash_length, count)?;
    let mut written = 0u64;
    while let Some(item) = heap.pop() {
      if written.is_multiple_of(4_096) {
        check_cancelled(&self.cancellation)?;
      }
      writer.write_record(&item.record)?;
      written = written.saturating_add(1);
      if let Some(record) = readers[item.reader].next_record()? {
        heap.push(HeapRecord { record, reader: item.reader });
      }
    }
    writer.finish()?;
    for run in runs {
      std::fs::remove_file(run.path)?;
    }
    Ok(RunFile { path: output, records: count })
  }
}

pub(crate) struct DirectoryRepairCursor {
  reader: RunReader,
  pending: Option<RepairRecord>,
  cancellation: Arc<AtomicBool>,
}

impl DirectoryRepairCursor {
  fn open(run: RunFile, hash_algo: HashAlgorithm, hash_length: usize, cancellation: Arc<AtomicBool>) -> EngineResult<Self> {
    Ok(Self { reader: RunReader::open(&run, hash_algo, hash_length)?, pending: None, cancellation })
  }

  pub(crate) fn next_group(&mut self, workspace: &mut DirectoryRepairWorkspace) -> EngineResult<Option<(String, Vec<ChildEntry>)>> {
    workspace.release_group()?;
    check_cancelled(&self.cancellation)?;
    let first = match self.pending.take() {
      Some(record) => Some(record),
      None => self.reader.next_record()?,
    };
    let Some(first) = first else {
      return Ok(None);
    };
    let directory = first.directory.clone();
    let path_reservation = u64::try_from(directory.len().saturating_mul(2).saturating_add(128))
      .map_err(|_| EngineError::ResourceExhausted("directory-repair path accounting overflow".to_string()))?;
    workspace.reserve_group(path_reservation)?;
    let mut children = Vec::new();
    let mut record = Some(first);

    loop {
      let current = match record.take() {
        Some(record) => Some(record),
        None => self.reader.next_record()?,
      };
      let Some(current) = current else {
        break;
      };
      if current.directory != directory {
        self.pending = Some(current);
        break;
      }
      let Some(child) = current.child else {
        continue;
      };
      if let Some(existing) = children.last().filter(|existing: &&ChildEntry| existing.name == child.name) {
        if *existing == child {
          continue;
        }
        return Err(workspace_corruption(format!(
          "conflicting authoritative children named {:?} while rebuilding {directory}",
          child.name
        )));
      }
      let child_bytes = estimated_child_bytes(&child).saturating_mul(2).saturating_add(std::mem::size_of::<ChildEntry>());
      workspace.reserve_group(u64::try_from(child_bytes).unwrap_or(u64::MAX))?;
      children
        .try_reserve_exact(1)
        .map_err(|error| EngineError::ResourceExhausted(format!("directory-repair child allocation failed: {error}")))?;
      children.push(child);
    }

    Ok(Some((directory, children)))
  }
}

#[derive(Debug)]
struct HeapRecord {
  record: RepairRecord,
  reader: usize,
}

impl PartialEq for HeapRecord {
  fn eq(&self, other: &Self) -> bool {
    self.record.sort_cmp(&other.record) == Ordering::Equal && self.reader == other.reader
  }
}

impl Eq for HeapRecord {}

impl Ord for HeapRecord {
  fn cmp(&self, other: &Self) -> Ordering {
    other.record.sort_cmp(&self.record).then_with(|| other.reader.cmp(&self.reader))
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
  expected: u64,
  written: u64,
}

impl RunWriter {
  fn create(path: &Path, hash_algo: HashAlgorithm, hash_length: usize, expected: u64) -> EngineResult<Self> {
    let file = OpenOptions::new().create_new(true).write(true).open(path)?;
    let mut writer = BufWriter::with_capacity(RUN_IO_BUFFER_BYTES, file);
    writer.write_all(&encode_run_header(hash_algo, hash_length, expected)?)?;
    Ok(Self { writer, hash_length, expected, written: 0 })
  }

  fn write_record(&mut self, record: &RepairRecord) -> EngineResult<()> {
    self.writer.write_all(&encode_frame(record, self.hash_length)?)?;
    self.written =
      self.written.checked_add(1).ok_or_else(|| EngineError::ResourceExhausted("directory-repair written count overflow".to_string()))?;
    Ok(())
  }

  fn finish(mut self) -> EngineResult<()> {
    if self.written != self.expected {
      return Err(workspace_corruption(format!("directory-repair run expected {} records but wrote {}", self.expected, self.written)));
    }
    self.writer.flush()?;
    Ok(())
  }
}

struct RunReader {
  reader: BufReader<File>,
  path: PathBuf,
  hash_length: usize,
  expected: u64,
  read: u64,
  eof_checked: bool,
}

impl RunReader {
  fn open(run: &RunFile, hash_algo: HashAlgorithm, hash_length: usize) -> EngineResult<Self> {
    let mut reader = BufReader::with_capacity(RUN_IO_BUFFER_BYTES, File::open(&run.path)?);
    let mut header = [0u8; RUN_HEADER_BYTES];
    reader.read_exact(&mut header).map_err(|error| workspace_io(&run.path, "run header", error))?;
    let expected = decode_run_header(&header, hash_algo, hash_length)?;
    if expected != run.records {
      return Err(workspace_corruption(format!(
        "directory-repair run {} count {expected} disagrees with manifest {}",
        run.path.display(),
        run.records
      )));
    }
    Ok(Self { reader, path: run.path.clone(), hash_length, expected, read: 0, eof_checked: false })
  }

  fn next_record(&mut self) -> EngineResult<Option<RepairRecord>> {
    if self.read == self.expected {
      if !self.eof_checked {
        let mut trailing = [0u8; 1];
        match self.reader.read(&mut trailing) {
          Ok(0) => self.eof_checked = true,
          Ok(_) => return Err(workspace_corruption(format!("directory-repair run {} has trailing bytes", self.path.display()))),
          Err(error) => return Err(workspace_io(&self.path, "run closure", error)),
        }
      }
      return Ok(None);
    }
    let record = read_frame(&mut self.reader, &self.path, self.hash_length)?
      .ok_or_else(|| workspace_corruption(format!("directory-repair run {} ended before its declared count", self.path.display())))?;
    self.read += 1;
    Ok(Some(record))
  }
}

fn encode_frame(record: &RepairRecord, hash_length: usize) -> EngineResult<Vec<u8>> {
  let directory = record.directory.as_bytes();
  let child = record.child.as_ref().map(|child| child.serialize(hash_length)).transpose()?.unwrap_or_default();
  let frame_length = FRAME_FIXED_BYTES
    .checked_add(directory.len())
    .and_then(|length| length.checked_add(child.len()))
    .and_then(|length| length.checked_add(FRAME_CRC_BYTES))
    .ok_or_else(|| EngineError::ResourceExhausted("directory-repair frame length overflow".to_string()))?;
  if frame_length > MAX_FRAME_BYTES || directory.len() > u32::MAX as usize || child.len() > u32::MAX as usize {
    return Err(EngineError::ResourceExhausted(format!("directory-repair frame is too large: {frame_length} bytes")));
  }
  let mut frame = Vec::new();
  frame
    .try_reserve_exact(4 + frame_length)
    .map_err(|error| EngineError::ResourceExhausted(format!("directory-repair frame allocation failed: {error}")))?;
  frame.extend_from_slice(&(frame_length as u32).to_le_bytes());
  frame.push(u8::from(record.child.is_some()));
  frame.extend_from_slice(&[0u8; 3]);
  frame.extend_from_slice(&(directory.len() as u32).to_le_bytes());
  frame.extend_from_slice(&(child.len() as u32).to_le_bytes());
  frame.extend_from_slice(directory);
  frame.extend_from_slice(&child);
  let checksum = crc32fast::hash(&frame[4..]);
  frame.extend_from_slice(&checksum.to_le_bytes());
  Ok(frame)
}

fn workspace_u16_at(bytes: &[u8], offset: usize, field: &str) -> EngineResult<u16> {
  let end = offset.checked_add(2).ok_or_else(|| workspace_corruption(format!("directory-repair {field} offset overflow")))?;
  let raw =
    bytes.get(offset..end).ok_or_else(|| workspace_corruption(format!("directory-repair {field} is truncated at offset {offset}")))?;
  let mut value = [0u8; 2];
  value.copy_from_slice(raw);
  Ok(u16::from_le_bytes(value))
}

fn workspace_u32_at(bytes: &[u8], offset: usize, field: &str) -> EngineResult<u32> {
  let end = offset.checked_add(4).ok_or_else(|| workspace_corruption(format!("directory-repair {field} offset overflow")))?;
  let raw =
    bytes.get(offset..end).ok_or_else(|| workspace_corruption(format!("directory-repair {field} is truncated at offset {offset}")))?;
  let mut value = [0u8; 4];
  value.copy_from_slice(raw);
  Ok(u32::from_le_bytes(value))
}

fn workspace_u64_at(bytes: &[u8], offset: usize, field: &str) -> EngineResult<u64> {
  let end = offset.checked_add(8).ok_or_else(|| workspace_corruption(format!("directory-repair {field} offset overflow")))?;
  let raw =
    bytes.get(offset..end).ok_or_else(|| workspace_corruption(format!("directory-repair {field} is truncated at offset {offset}")))?;
  let mut value = [0u8; 8];
  value.copy_from_slice(raw);
  Ok(u64::from_le_bytes(value))
}

fn read_frame<R: Read>(reader: &mut R, path: &Path, hash_length: usize) -> EngineResult<Option<RepairRecord>> {
  let mut length = [0u8; 4];
  match reader.read(&mut length[..1]) {
    Ok(0) => return Ok(None),
    Ok(1) => {}
    Ok(read) => {
      return Err(workspace_corruption(format!("directory-repair reader returned invalid byte count {read} for a one-byte buffer")))
    }
    Err(error) => return Err(workspace_io(path, "record prefix", error)),
  }
  reader.read_exact(&mut length[1..]).map_err(|error| workspace_io(path, "record prefix", error))?;
  let frame_length = u32::from_le_bytes(length) as usize;
  if !(FRAME_FIXED_BYTES + FRAME_CRC_BYTES..=MAX_FRAME_BYTES).contains(&frame_length) {
    return Err(workspace_corruption(format!("directory-repair record in {} has invalid frame length {frame_length}", path.display())));
  }
  let mut frame = Vec::new();
  frame
    .try_reserve_exact(frame_length)
    .map_err(|error| EngineError::ResourceExhausted(format!("directory-repair record allocation failed: {error}")))?;
  frame.resize(frame_length, 0);
  reader.read_exact(&mut frame).map_err(|error| workspace_io(path, "record body", error))?;
  let checksum_offset = frame_length - FRAME_CRC_BYTES;
  let stored = workspace_u32_at(&frame, checksum_offset, "record checksum")?;
  if stored != crc32fast::hash(&frame[..checksum_offset]) {
    return Err(workspace_corruption(format!("directory-repair record checksum mismatch in {}", path.display())));
  }
  let kind = frame[0];
  if kind > 1 || frame[1..4] != [0u8; 3] {
    return Err(workspace_corruption(format!("directory-repair record flags are invalid in {}", path.display())));
  }
  let directory_length = workspace_u32_at(&frame, 4, "record directory length")? as usize;
  let child_length = workspace_u32_at(&frame, 8, "record child length")? as usize;
  let expected = FRAME_FIXED_BYTES
    .checked_add(directory_length)
    .and_then(|length| length.checked_add(child_length))
    .ok_or_else(|| workspace_corruption("directory-repair record dimensions overflow".to_string()))?;
  if expected != checksum_offset || (kind == 0) != (child_length == 0) {
    return Err(workspace_corruption(format!("directory-repair record dimensions are invalid in {}", path.display())));
  }
  let directory_end = FRAME_FIXED_BYTES + directory_length;
  let directory = std::str::from_utf8(&frame[FRAME_FIXED_BYTES..directory_end])
    .map_err(|_| workspace_corruption(format!("directory-repair path is not UTF-8 in {}", path.display())))?;
  let directory = validate_directory_path(directory)?;
  let child = if kind == 1 {
    let (child, consumed) = ChildEntry::deserialize(&frame[directory_end..checksum_offset], hash_length, 0)?;
    if consumed != child_length || child.hash.len() != hash_length || child.name.is_empty() || child.name.contains('/') {
      return Err(workspace_corruption(format!("directory-repair child framing is invalid in {}", path.display())));
    }
    Some(child)
  } else {
    None
  };
  Ok(Some(RepairRecord { directory, child }))
}

fn encode_run_header(hash_algo: HashAlgorithm, hash_length: usize, records: u64) -> EngineResult<[u8; RUN_HEADER_BYTES]> {
  let hash_length =
    u16::try_from(hash_length).map_err(|_| EngineError::InvalidInput("directory-repair hash length exceeds u16".to_string()))?;
  let mut header = [0u8; RUN_HEADER_BYTES];
  header[..8].copy_from_slice(RUN_MAGIC);
  header[8..10].copy_from_slice(&RUN_VERSION.to_le_bytes());
  header[10..12].copy_from_slice(&hash_algo.to_u16().to_le_bytes());
  header[12..14].copy_from_slice(&hash_length.to_le_bytes());
  header[16..24].copy_from_slice(&records.to_le_bytes());
  let checksum = crc32fast::hash(&header[..28]);
  header[28..32].copy_from_slice(&checksum.to_le_bytes());
  Ok(header)
}

fn decode_run_header(header: &[u8; RUN_HEADER_BYTES], hash_algo: HashAlgorithm, hash_length: usize) -> EngineResult<u64> {
  let stored = workspace_u32_at(header, 28, "run-header checksum")?;
  if &header[..8] != RUN_MAGIC || stored != crc32fast::hash(&header[..28]) {
    return Err(workspace_corruption("directory-repair run header magic/checksum is invalid".to_string()));
  }
  let version = workspace_u16_at(header, 8, "run-header version")?;
  let algorithm = workspace_u16_at(header, 10, "run-header algorithm")?;
  let stored_hash_length = workspace_u16_at(header, 12, "run-header hash length")? as usize;
  if version != RUN_VERSION
    || algorithm != hash_algo.to_u16()
    || stored_hash_length != hash_length
    || header[14..16] != [0; 2]
    || header[24..28] != [0; 4]
  {
    return Err(workspace_corruption("directory-repair run header contract mismatch".to_string()));
  }
  workspace_u64_at(header, 16, "run-header record count")
}

fn validate_directory_path(path: &str) -> EngineResult<String> {
  if path.is_empty() || !path.starts_with('/') || path.as_bytes().contains(&0) {
    return Err(workspace_corruption(format!("invalid directory-repair path {path:?}")));
  }
  let normalized = normalize_path(path);
  if normalized != path {
    return Err(workspace_corruption(format!("directory-repair path is not canonical: {path:?}")));
  }
  Ok(normalized)
}

fn directory_depth(path: &str) -> usize {
  if path == "/" {
    0
  } else {
    path.split('/').filter(|segment| !segment.is_empty()).count()
  }
}

fn estimated_child_bytes(child: &ChildEntry) -> usize {
  std::mem::size_of::<ChildEntry>()
    .saturating_add(child.name.len())
    .saturating_add(child.hash.len())
    .saturating_add(child.content_type.as_ref().map_or(0, String::len))
}

fn check_cancelled(cancellation: &AtomicBool) -> EngineResult<()> {
  if cancellation.load(AtomicOrdering::Acquire) {
    Err(EngineError::ShuttingDown)
  } else {
    Ok(())
  }
}

fn workspace_prefix(database_path: &Path) -> String {
  let identity = blake3::hash(database_path.to_string_lossy().as_bytes());
  format!(".aeordb-directory-repair-{}-", &identity.to_hex()[..16])
}

fn cleanup_stale_workspaces(parent: &Path, prefix: &str) -> EngineResult<()> {
  for entry in std::fs::read_dir(parent)? {
    let entry = entry?;
    if !entry.file_name().to_string_lossy().starts_with(prefix) {
      continue;
    }
    let metadata = std::fs::symlink_metadata(entry.path())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
      return Err(EngineError::InvalidInput(format!(
        "suspicious directory-repair workspace artifact is not a private directory: {}",
        entry.path().display()
      )));
    }
    std::fs::remove_dir_all(entry.path())?;
  }
  Ok(())
}

fn workspace_memory_error(error: MemoryCoordinatorError) -> EngineError {
  match error {
    MemoryCoordinatorError::PolicyUnavailable
    | MemoryCoordinatorError::HardLimitExceeded { .. }
    | MemoryCoordinatorError::SoftPressureDeferred { .. }
    | MemoryCoordinatorError::EmergencyReserveExceeded { .. } => {
      EngineError::ResourceExhausted(format!("directory-repair memory admission failed: {error}"))
    }
    _ => EngineError::IoError(std::io::Error::other(format!("directory-repair memory accounting failed: {error}"))),
  }
}

fn workspace_io(path: &Path, operation: &str, error: std::io::Error) -> EngineError {
  EngineError::IoError(std::io::Error::new(
    error.kind(),
    format!("directory-repair scratch {operation} failed for {}: {error}", path.display()),
  ))
}

fn workspace_corruption(reason: String) -> EngineError {
  EngineError::CorruptEntry { offset: 0, reason }
}

#[cfg(test)]
#[path = "../../spec/engine/directory_repair_workspace_panic_internal_spec.rs"]
mod directory_repair_workspace_panic_internal_spec;

#[cfg(test)]
mod tests {
  use std::io::{Cursor, Write};

  use super::*;
  use crate::engine::entry_type::EntryType;
  use crate::engine::memory_coordinator::MemoryPolicy;

  fn coordinator() -> MemoryCoordinator {
    MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 64 * 1024 * 1024).unwrap())
  }

  fn workspace(temp: &tempfile::TempDir, cancellation: Arc<AtomicBool>) -> DirectoryRepairWorkspace {
    let database = temp.path().join("test.aeordb");
    File::create(&database).unwrap();
    DirectoryRepairWorkspace::new(&database, HashAlgorithm::Blake3_256, &coordinator(), cancellation).unwrap()
  }

  fn child(name: &str, byte: u8) -> ChildEntry {
    ChildEntry {
      entry_type: EntryType::FileRecord.to_u8(),
      hash: vec![byte; HashAlgorithm::Blake3_256.hash_length()],
      total_size: byte as u64,
      created_at: byte as i64,
      updated_at: byte as i64,
      name: name.to_string(),
      content_type: Some("text/plain".to_string()),
      virtual_time: 0,
      node_id: 0,
    }
  }

  #[test]
  fn depth_spools_group_children_in_stable_order_and_release_memory() {
    let temp = tempfile::tempdir().unwrap();
    let cancellation = Arc::new(AtomicBool::new(false));
    let coordinator = coordinator();
    let database = temp.path().join("test.aeordb");
    File::create(&database).unwrap();
    let mut workspace =
      DirectoryRepairWorkspace::new(&database, HashAlgorithm::Blake3_256, &coordinator, Arc::clone(&cancellation)).unwrap();
    workspace.push_marker("/a/b").unwrap();
    workspace.push_child("/a/b", child("z.txt", 2)).unwrap();
    workspace.push_child("/a/b", child("a.txt", 1)).unwrap();
    workspace.push_child("/a/b", child("a.txt", 1)).unwrap();

    let mut cursor = workspace.finish_depth(2).unwrap();
    let (path, children) = cursor.next_group(&mut workspace).unwrap().unwrap();
    assert_eq!(path, "/a/b");
    assert_eq!(children.iter().map(|child| child.name.as_str()).collect::<Vec<_>>(), ["a.txt", "z.txt"]);
    assert!(cursor.next_group(&mut workspace).unwrap().is_none());
    workspace.release_group().unwrap();
    drop(workspace);

    let owner = coordinator.snapshot().unwrap().owner(MemoryOwner::Repair).unwrap().clone();
    assert_eq!(owner.reserved_bytes, 0);
    assert_eq!(owner.active_reservations, 0);
  }

  #[test]
  fn merged_runs_preserve_total_order_and_reject_conflicting_children() {
    let temp = tempfile::tempdir().unwrap();
    let mut workspace = workspace(&temp, Arc::new(AtomicBool::new(false)));
    let run_a = workspace
      .write_run(&[
        RepairRecord::child("/a", child("same", 1), workspace.hash_length).unwrap(),
        RepairRecord::child("/a", child("z", 3), workspace.hash_length).unwrap(),
      ])
      .unwrap();
    let run_b = workspace
      .write_run(&[RepairRecord::marker("/a").unwrap(), RepairRecord::child("/a", child("same", 2), workspace.hash_length).unwrap()])
      .unwrap();
    let merged = workspace.merge_runs(vec![run_a, run_b]).unwrap();
    let mut cursor =
      DirectoryRepairCursor::open(merged, workspace.hash_algo, workspace.hash_length, Arc::clone(&workspace.cancellation)).unwrap();

    let error = cursor.next_group(&mut workspace).unwrap_err();
    assert!(matches!(error, EngineError::CorruptEntry { .. }));
  }

  #[test]
  fn run_reader_rejects_bytes_after_declared_record_count() {
    let temp = tempfile::tempdir().unwrap();
    let mut workspace = workspace(&temp, Arc::new(AtomicBool::new(false)));
    let run = workspace.write_run(&[RepairRecord::marker("/a").unwrap()]).unwrap();
    OpenOptions::new().append(true).open(&run.path).unwrap().write_all(b"unexpected").unwrap();

    let mut reader = RunReader::open(&run, workspace.hash_algo, workspace.hash_length).unwrap();
    assert!(reader.next_record().unwrap().is_some());
    let error = reader.next_record().unwrap_err();
    assert!(matches!(error, EngineError::CorruptEntry { .. }));
  }

  #[test]
  fn malformed_frames_and_stale_workspace_lookalikes_fail_closed() {
    let record = RepairRecord::child("/a", child("file", 1), HashAlgorithm::Blake3_256.hash_length()).unwrap();
    let mut encoded = encode_frame(&record, HashAlgorithm::Blake3_256.hash_length()).unwrap();
    *encoded.last_mut().unwrap() ^= 0xff;
    let error = read_frame(&mut Cursor::new(encoded), Path::new("corrupt-frame"), HashAlgorithm::Blake3_256.hash_length()).unwrap_err();
    assert!(matches!(error, EngineError::CorruptEntry { .. }));

    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("test.aeordb");
    File::create(&database).unwrap();
    let lookalike = temp.path().join(format!("{}file", workspace_prefix(&database)));
    File::create(&lookalike).unwrap();
    let error = DirectoryRepairWorkspace::new(&database, HashAlgorithm::Blake3_256, &coordinator(), Arc::new(AtomicBool::new(false)))
      .err()
      .expect("non-directory stale artifact must fail closed");
    assert!(matches!(error, EngineError::InvalidInput(_)));
    assert!(lookalike.exists());
  }

  #[test]
  fn cancellation_stops_spooling_and_drop_removes_scratch() {
    let temp = tempfile::tempdir().unwrap();
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut workspace = workspace(&temp, Arc::clone(&cancellation));
    workspace.push_child("/a", child("file", 1)).unwrap();
    let scratch = workspace.directory.path().to_path_buf();

    cancellation.store(true, AtomicOrdering::Release);
    assert!(matches!(workspace.push_child("/a", child("later", 2)), Err(EngineError::ShuttingDown)));
    assert!(matches!(workspace.finish_depth(1), Err(EngineError::ShuttingDown)));
    drop(workspace);
    assert!(!scratch.exists());
  }

  #[test]
  fn deeply_nested_repairs_keep_raw_spool_descriptors_bounded() {
    let temp = tempfile::tempdir().unwrap();
    let mut workspace = workspace(&temp, Arc::new(AtomicBool::new(false)));

    for depth in 1..=64 {
      let path = format!("/{}", vec!["level"; depth].join("/"));
      workspace.push_marker(&path).unwrap();
    }

    assert!(workspace.raw_files.len() <= MAX_OPEN_RAW_FILES, "open raw spool files must remain bounded independently of tree depth");
  }
}
