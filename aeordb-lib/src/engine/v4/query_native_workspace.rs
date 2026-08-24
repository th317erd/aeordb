//! Disposable bounded ordering workspace for native authoritative queries.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::engine::emergency_spill::open_regular_file_no_follow;
use crate::engine::file_record::FileRecord;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};
use crate::engine::v4::hash::digest_parts;
use crate::engine::v4::private_workspace::{
  PrivateWorkspaceErrorV1, create_private_regular_file, secure_platform_private_directory, validate_existing_directory,
  validate_private_regular_file,
};
use crate::engine::v4::scope::validate_canonical_absolute_path;
use crate::engine::HashAlgorithm;

const MAXIMUM_HASH_LENGTH: usize = 64;
const MAXIMUM_ENCODED_FILE_RECORD_BYTES: usize = 4 * 1024 * 1024;
const MAXIMUM_DECODE_WORKSPACE_BYTES: usize = 16 * 1024 * 1024;
const MAXIMUM_WORKSPACE_BYTES: u64 = 16 * 1024 * 1024 * 1024 * 1024;
const MAXIMUM_RECORDS: u64 = 1_000_000_000;
const MAXIMUM_RECORDS_PER_RUN: usize = 1_000_000;
const MAXIMUM_MERGE_FAN_IN: usize = 64;
const MAXIMUM_RUN_LEVELS: usize = 64;
const RUN_HEADER_LENGTH: usize = 32;
const RUN_MAGIC: &[u8; 8] = b"AEQORUN1";
const RUN_VERSION: u16 = 1;
const DATA_HEADER_LENGTH: usize = 16;
const DATA_MAGIC: &[u8; 4] = b"AQDF";
const DATA_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeQueryOrderingWorkspaceErrorClassV1 {
  Invalid,
  Resource,
  Unavailable,
  Corrupt,
  Cancelled,
}

#[derive(Debug)]
pub(crate) struct NativeQueryOrderingWorkspaceErrorV1 {
  class: NativeQueryOrderingWorkspaceErrorClassV1,
  code: &'static str,
  context: String,
}

impl NativeQueryOrderingWorkspaceErrorV1 {
  pub(crate) const fn class(&self) -> NativeQueryOrderingWorkspaceErrorClassV1 {
    self.class
  }

  pub(crate) const fn code(&self) -> &'static str {
    self.code
  }

  pub(crate) fn context(&self) -> &str {
    &self.context
  }

  fn invalid(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: NativeQueryOrderingWorkspaceErrorClassV1::Invalid, code, context: context.into() }
  }

  fn resource(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: NativeQueryOrderingWorkspaceErrorClassV1::Resource, code, context: context.into() }
  }

  fn unavailable(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: NativeQueryOrderingWorkspaceErrorClassV1::Unavailable, code, context: context.into() }
  }

  fn corrupt(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: NativeQueryOrderingWorkspaceErrorClassV1::Corrupt, code, context: context.into() }
  }

  fn cancelled() -> Self {
    Self {
      class: NativeQueryOrderingWorkspaceErrorClassV1::Cancelled,
      code: "native_query_order_workspace_cancelled",
      context: "native query ordering workspace work was cancelled".to_string(),
    }
  }
}

impl fmt::Display for NativeQueryOrderingWorkspaceErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.context)
  }
}

impl Error for NativeQueryOrderingWorkspaceErrorV1 {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NativeQueryOrderingWorkspaceLimitsV1 {
  maximum_records: u64,
  maximum_workspace_bytes: u64,
  maximum_sort_bytes: u64,
  maximum_records_per_run: usize,
  merge_fan_in: usize,
  maximum_run_levels: usize,
}

impl NativeQueryOrderingWorkspaceLimitsV1 {
  pub(crate) fn new(
    maximum_records: u64,
    maximum_workspace_bytes: u64,
    maximum_sort_bytes: u64,
    maximum_records_per_run: usize,
    merge_fan_in: usize,
  ) -> Result<Self, NativeQueryOrderingWorkspaceErrorV1> {
    if maximum_records == 0
      || maximum_records > MAXIMUM_RECORDS
      || maximum_workspace_bytes == 0
      || maximum_workspace_bytes > MAXIMUM_WORKSPACE_BYTES
      || maximum_records_per_run == 0
      || maximum_records_per_run > MAXIMUM_RECORDS_PER_RUN
      || !(2..=MAXIMUM_MERGE_FAN_IN).contains(&merge_fan_in)
    {
      return Err(NativeQueryOrderingWorkspaceErrorV1::invalid(
        "native_query_order_limits",
        "native query ordering limits are zero, exceed fixed safety maxima, or have an invalid merge fan-in",
      ));
    }
    let maximum_run_levels = maximum_run_levels(maximum_records, maximum_records_per_run, merge_fan_in)?;
    let maximum_retained_runs = maximum_run_levels
      .checked_mul(merge_fan_in)
      .ok_or_else(|| NativeQueryOrderingWorkspaceErrorV1::invalid("native_query_order_limits", "retained run bound overflowed"))?;
    let reference_bytes = maximum_records_per_run
      .checked_mul(size_of::<WorkspaceReferenceV1>())
      .ok_or_else(|| NativeQueryOrderingWorkspaceErrorV1::invalid("native_query_order_limits", "sort-reference bound overflowed"))?;
    let run_descriptor_bytes = maximum_retained_runs
      .checked_mul(size_of::<RunFileV1>())
      .and_then(|bytes| bytes.checked_mul(3))
      .ok_or_else(|| NativeQueryOrderingWorkspaceErrorV1::invalid("native_query_order_limits", "run-descriptor bound overflowed"))?;
    let merge_bytes = merge_fan_in
      .checked_mul(size_of::<RunReaderV1>() + size_of::<MergeHeadV1>() + size_of::<RunFileV1>())
      .ok_or_else(|| NativeQueryOrderingWorkspaceErrorV1::invalid("native_query_order_limits", "merge-window bound overflowed"))?;
    let required_sort_bytes = reference_bytes
      .checked_add(run_descriptor_bytes)
      .and_then(|bytes| bytes.checked_add(merge_bytes))
      .and_then(|bytes| bytes.checked_add(MAXIMUM_ENCODED_FILE_RECORD_BYTES + DATA_HEADER_LENGTH + MAXIMUM_HASH_LENGTH))
      .ok_or_else(|| {
        NativeQueryOrderingWorkspaceErrorV1::invalid(
          "native_query_order_limits",
          "native query ordering sort-memory bound overflows this platform",
        )
      })?;
    let required_sort_bytes = u64::try_from(required_sort_bytes).map_err(|error| {
      NativeQueryOrderingWorkspaceErrorV1::invalid(
        "native_query_order_limits",
        format!("native query ordering sort-memory bound exceeds u64: {error}"),
      )
    })?;
    if maximum_sort_bytes < required_sort_bytes {
      return Err(NativeQueryOrderingWorkspaceErrorV1::invalid(
        "native_query_order_limits",
        "native query ordering sort memory cannot hold its bounded references, run tiers, and merge window",
      ));
    }
    Ok(Self { maximum_records, maximum_workspace_bytes, maximum_sort_bytes, maximum_records_per_run, merge_fan_in, maximum_run_levels })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkspaceReferenceV1 {
  file_key: [u8; MAXIMUM_HASH_LENGTH],
  scope_id: [u8; MAXIMUM_HASH_LENGTH],
  hash_length: u8,
  data_offset: u64,
  data_length: u32,
  data_checksum: u32,
}

impl WorkspaceReferenceV1 {
  fn new(
    file_key: &[u8],
    scope_id: Option<&[u8]>,
    data_offset: u64,
    data_length: u32,
    data_checksum: u32,
  ) -> Result<Self, NativeQueryOrderingWorkspaceErrorV1> {
    if !matches!(file_key.len(), 32 | 64)
      || file_key.iter().all(|byte| *byte == 0)
      || scope_id.is_some_and(|scope_id| scope_id.len() != file_key.len() || scope_id.iter().all(|byte| *byte == 0))
    {
      return Err(NativeQueryOrderingWorkspaceErrorV1::invalid(
        "native_query_order_identity",
        "workspace FileKey and any effective ScopeId must be nonzero frozen-width identities",
      ));
    }
    let mut retained_file_key = [0; MAXIMUM_HASH_LENGTH];
    retained_file_key[..file_key.len()].copy_from_slice(file_key);
    let mut retained_scope_id = [0; MAXIMUM_HASH_LENGTH];
    if let Some(scope_id) = scope_id {
      retained_scope_id[..scope_id.len()].copy_from_slice(scope_id);
    }
    Ok(Self {
      file_key: retained_file_key,
      scope_id: retained_scope_id,
      hash_length: file_key.len() as u8,
      data_offset,
      data_length,
      data_checksum,
    })
  }

  fn file_key(&self) -> &[u8] {
    &self.file_key[..self.hash_length as usize]
  }
}

#[derive(Clone, Debug)]
struct RunFileV1 {
  run_id: u64,
  record_count: u64,
  byte_length: u64,
}

pub(crate) struct NativeQueryOrderingWorkspaceBuilderV1 {
  directory: tempfile::TempDir,
  data_path: PathBuf,
  data: File,
  hash_algorithm: HashAlgorithm,
  hash_length: usize,
  memory: Arc<MemoryCoordinator>,
  cancellation: CancellationToken,
  limits: NativeQueryOrderingWorkspaceLimitsV1,
  records: Vec<WorkspaceReferenceV1>,
  run_levels: Vec<Vec<RunFileV1>>,
  next_run_id: u64,
  record_count: u64,
  data_length: u64,
  workspace_bytes: u64,
  failed: bool,
  _sort_memory: MemoryReservation,
}

impl NativeQueryOrderingWorkspaceBuilderV1 {
  pub(crate) fn new(
    parent: &Path,
    hash_algorithm: HashAlgorithm,
    memory: Arc<MemoryCoordinator>,
    cancellation: CancellationToken,
    limits: NativeQueryOrderingWorkspaceLimitsV1,
  ) -> Result<Self, NativeQueryOrderingWorkspaceErrorV1> {
    require_not_cancelled(&cancellation)?;
    if !matches!(hash_algorithm, HashAlgorithm::Blake3_256 | HashAlgorithm::Sha512) {
      return Err(NativeQueryOrderingWorkspaceErrorV1::invalid(
        "native_query_order_hash_algorithm",
        "native query ordering requires a frozen v4 hash width",
      ));
    }
    validate_existing_directory(parent, "native query workspace parent").map_err(map_private_workspace_error)?;
    let sort_memory = memory
      .reserve(MemoryOwner::Query, limits.maximum_sort_bytes, AdmissionClass::Workload)
      .map_err(|error| NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_memory", error.to_string()))?;
    let directory = tempfile::Builder::new()
      .prefix(".aeordb-query-order-")
      .tempdir_in(parent)
      .map_err(|error| NativeQueryOrderingWorkspaceErrorV1::unavailable("native_query_order_workspace_create", error.to_string()))?;
    secure_platform_private_directory(directory.path()).map_err(map_private_workspace_error)?;
    let data_path = directory.path().join("rows.aqdf");
    let data = create_private_regular_file(&data_path, "native query row spool").map_err(map_private_workspace_error)?;
    let mut records = Vec::new();
    records.try_reserve_exact(limits.maximum_records_per_run).map_err(|error| {
      NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_allocation", format!("sort-reference allocation failed: {error}"))
    })?;
    let mut run_levels = Vec::new();
    run_levels.try_reserve_exact(limits.maximum_run_levels).map_err(|error| {
      NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_allocation", format!("run-level allocation failed: {error}"))
    })?;
    Ok(Self {
      directory,
      data_path,
      data,
      hash_algorithm,
      hash_length: hash_algorithm.hash_length(),
      memory,
      cancellation,
      limits,
      records,
      run_levels,
      next_run_id: 0,
      record_count: 0,
      data_length: 0,
      workspace_bytes: 0,
      failed: false,
      _sort_memory: sort_memory,
    })
  }

  pub(crate) fn append_parts(
    &mut self,
    file_key: &[u8],
    scope_id: Option<&[u8]>,
    record_revision: &[u8],
    entity_version: u8,
    file_record: &FileRecord,
  ) -> Result<(), NativeQueryOrderingWorkspaceErrorV1> {
    self.require_usable()?;
    validate_identity(file_key, self.hash_length, "FileKey")?;
    if let Some(scope_id) = scope_id {
      validate_identity(scope_id, self.hash_length, "ScopeId")?;
    }
    validate_identity(record_revision, self.hash_length, "RecordRevision")?;
    if !matches!(entity_version, 0 | 1) {
      return Err(NativeQueryOrderingWorkspaceErrorV1::invalid(
        "native_query_order_file_version",
        "workspace FileRecord version is not readable",
      ));
    }
    let derived_file_key = digest_parts(self.hash_algorithm, &[b"file:", file_record.path.as_bytes()]);
    if derived_file_key != file_key {
      return Err(NativeQueryOrderingWorkspaceErrorV1::corrupt(
        "native_query_order_file_identity",
        "workspace FileKey does not match the canonical FileRecord path",
      ));
    }
    if self.record_count >= self.limits.maximum_records {
      return Err(NativeQueryOrderingWorkspaceErrorV1::resource(
        "native_query_order_record_limit",
        "native query ordering workspace exceeds its admitted document count",
      ));
    }
    if self.records.len() == self.limits.maximum_records_per_run {
      if let Err(error) = self.flush_initial_run() {
        self.failed = true;
        return Err(error);
      }
    }
    let encoded = file_record.serialize_for_version(self.hash_length, entity_version).map_err(|error| {
      NativeQueryOrderingWorkspaceErrorV1::corrupt("native_query_order_file_record", format!("cannot encode captured FileRecord: {error}"))
    })?;
    validate_canonical_absolute_path(&file_record.path)
      .map_err(|error| NativeQueryOrderingWorkspaceErrorV1::corrupt("native_query_order_file_path", error.to_string()))?;
    if digest_parts(self.hash_algorithm, &[b"filec:", &encoded]) != record_revision {
      return Err(NativeQueryOrderingWorkspaceErrorV1::corrupt(
        "native_query_order_record_revision",
        "workspace RecordRevision does not match the captured FileRecord bytes",
      ));
    }
    if encoded.len() > MAXIMUM_ENCODED_FILE_RECORD_BYTES {
      return Err(NativeQueryOrderingWorkspaceErrorV1::resource(
        "native_query_order_file_record_limit",
        "captured FileRecord exceeds the workspace frame bound",
      ));
    }
    let payload_length = self
      .hash_length
      .checked_add(encoded.len())
      .ok_or_else(|| NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_frame_length", "row payload length overflowed"))?;
    let frame_length = DATA_HEADER_LENGTH
      .checked_add(payload_length)
      .ok_or_else(|| NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_frame_length", "row frame length overflowed"))?;
    let frame_length = u32::try_from(frame_length).map_err(|error| {
      NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_frame_length", format!("row frame length exceeds u32: {error}"))
    })?;
    self.admit_workspace_bytes(u64::from(frame_length))?;
    let mut checksum = crc32fast::Hasher::new();
    checksum.update(record_revision);
    checksum.update(&encoded);
    let checksum = checksum.finalize();
    let mut header = [0; DATA_HEADER_LENGTH];
    header[..4].copy_from_slice(DATA_MAGIC);
    header[4..6].copy_from_slice(&DATA_VERSION.to_le_bytes());
    header[6] = entity_version;
    header[7] = self.hash_length as u8;
    header[8..12].copy_from_slice(&frame_length.to_le_bytes());
    header[12..16].copy_from_slice(&checksum.to_le_bytes());
    let data_offset = self.data_length;
    let write_result =
      self.data.write_all(&header).and_then(|_| self.data.write_all(record_revision)).and_then(|_| self.data.write_all(&encoded));
    if let Err(error) = write_result {
      self.failed = true;
      return Err(NativeQueryOrderingWorkspaceErrorV1::unavailable("native_query_order_data_write", error.to_string()));
    }
    let next_data_length = self
      .data_length
      .checked_add(u64::from(frame_length))
      .ok_or_else(|| NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_frame_length", "row data length overflowed"))?;
    let next_workspace_bytes = self.workspace_bytes.checked_add(u64::from(frame_length)).ok_or_else(|| {
      NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_workspace_bytes", "workspace byte count overflowed")
    })?;
    let reference = WorkspaceReferenceV1::new(file_key, scope_id, data_offset, frame_length, checksum)?;
    let next_record_count = self.record_count.checked_add(1).ok_or_else(|| {
      NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_record_limit", "workspace record count overflowed")
    })?;
    self.data_length = next_data_length;
    self.workspace_bytes = next_workspace_bytes;
    self.records.push(reference);
    self.record_count = next_record_count;
    Ok(())
  }

  pub(crate) fn finish(mut self) -> Result<NativeQueryOrderingWorkspaceV1, NativeQueryOrderingWorkspaceErrorV1> {
    self.require_usable()?;
    if let Err(error) = self.data.flush() {
      self.failed = true;
      return Err(NativeQueryOrderingWorkspaceErrorV1::unavailable("native_query_order_data_flush", error.to_string()));
    }
    self.flush_initial_run()?;
    let mut runs = self.take_retained_runs()?;
    if runs.is_empty() {
      runs.push(self.write_sorted_run(&[])?);
    }
    let final_run = self.merge_all_runs(runs)?;
    if final_run.record_count != self.record_count {
      return Err(NativeQueryOrderingWorkspaceErrorV1::corrupt(
        "native_query_order_record_count",
        "final ordering run does not contain every admitted document",
      ));
    }
    let data_length = self
      .data
      .metadata()
      .map_err(|error| NativeQueryOrderingWorkspaceErrorV1::unavailable("native_query_order_data_metadata", error.to_string()))?
      .len();
    if data_length != self.data_length {
      return Err(NativeQueryOrderingWorkspaceErrorV1::corrupt(
        "native_query_order_data_length",
        "row spool physical length disagrees with its completed frame frontier",
      ));
    }
    Ok(NativeQueryOrderingWorkspaceV1 {
      directory: self.directory,
      data_path: self.data_path,
      order_run: final_run,
      hash_algorithm: self.hash_algorithm,
      hash_length: self.hash_length,
      memory: self.memory,
      cancellation: self.cancellation,
      record_count: self.record_count,
      data_length,
      workspace_bytes: self.workspace_bytes,
    })
  }

  fn require_usable(&self) -> Result<(), NativeQueryOrderingWorkspaceErrorV1> {
    require_not_cancelled(&self.cancellation)?;
    if self.failed {
      return Err(NativeQueryOrderingWorkspaceErrorV1::unavailable(
        "native_query_order_workspace_failed",
        "native query ordering workspace cannot continue after an uncertain scratch write",
      ));
    }
    Ok(())
  }

  fn admit_workspace_bytes(&self, additional: u64) -> Result<(), NativeQueryOrderingWorkspaceErrorV1> {
    let total = self.workspace_bytes.checked_add(additional).ok_or_else(|| {
      NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_workspace_bytes", "workspace byte count overflowed")
    })?;
    if total > self.limits.maximum_workspace_bytes {
      return Err(NativeQueryOrderingWorkspaceErrorV1::resource(
        "native_query_order_workspace_bytes",
        "native query ordering workspace exceeds its admitted disk-byte bound",
      ));
    }
    Ok(())
  }

  fn flush_initial_run(&mut self) -> Result<(), NativeQueryOrderingWorkspaceErrorV1> {
    if self.records.is_empty() {
      return Ok(());
    }
    require_not_cancelled(&self.cancellation)?;
    self.records.sort_unstable_by(|left, right| left.file_key().cmp(right.file_key()));
    reject_duplicate_references(&self.records)?;
    let records = std::mem::take(&mut self.records);
    let run = self.write_sorted_run(&records);
    self.records = records;
    self.records.clear();
    let run = run?;
    self.retain_compacted_run(run)
  }

  fn write_sorted_run(&mut self, records: &[WorkspaceReferenceV1]) -> Result<RunFileV1, NativeQueryOrderingWorkspaceErrorV1> {
    let record_count = u64::try_from(records.len()).map_err(|error| {
      NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_record_count", format!("run record count exceeds u64: {error}"))
    })?;
    let (run, path, mut file) = self.create_run(record_count)?;
    let header = encode_run_header(self.hash_length, record_count)?;
    if let Err(error) = file.write_all(&header) {
      self.failed = true;
      return Err(NativeQueryOrderingWorkspaceErrorV1::unavailable("native_query_order_run_write", error.to_string()));
    }
    for record in records {
      require_not_cancelled(&self.cancellation)?;
      let encoded = encode_reference(record)?;
      if let Err(error) = file.write_all(&encoded) {
        self.failed = true;
        return Err(NativeQueryOrderingWorkspaceErrorV1::unavailable("native_query_order_run_write", error.to_string()));
      }
    }
    finish_run_file(&path, &mut file, run.byte_length)?;
    self.workspace_bytes = self
      .workspace_bytes
      .checked_add(run.byte_length)
      .ok_or_else(|| NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_workspace_bytes", "run byte count overflowed"))?;
    Ok(run)
  }

  fn create_run(&mut self, record_count: u64) -> Result<(RunFileV1, PathBuf, File), NativeQueryOrderingWorkspaceErrorV1> {
    let byte_length = run_byte_length(self.hash_length, record_count)?;
    self.admit_workspace_bytes(byte_length)?;
    let run_id = self.next_run_id;
    self.next_run_id = self
      .next_run_id
      .checked_add(1)
      .ok_or_else(|| NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_run_count", "run identity overflowed"))?;
    let path = run_path(self.directory.path(), run_id);
    let file = create_private_regular_file(&path, "native query ordering run").map_err(map_private_workspace_error)?;
    Ok((RunFileV1 { run_id, record_count, byte_length }, path, file))
  }

  fn retain_compacted_run(&mut self, mut run: RunFileV1) -> Result<(), NativeQueryOrderingWorkspaceErrorV1> {
    let mut level = 0usize;
    loop {
      require_not_cancelled(&self.cancellation)?;
      if level >= self.limits.maximum_run_levels {
        return Err(NativeQueryOrderingWorkspaceErrorV1::resource(
          "native_query_order_run_levels",
          "native query ordering exceeded its calculated run-level bound",
        ));
      }
      while self.run_levels.len() <= level {
        let mut runs = Vec::new();
        runs.try_reserve_exact(self.limits.merge_fan_in).map_err(|error| {
          NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_allocation", format!("run-tier allocation failed: {error}"))
        })?;
        self.run_levels.push(runs);
      }
      self.run_levels[level].push(run);
      if self.run_levels[level].len() < self.limits.merge_fan_in {
        return Ok(());
      }
      let mut group = Vec::new();
      std::mem::swap(&mut group, &mut self.run_levels[level]);
      self.run_levels[level].try_reserve_exact(self.limits.merge_fan_in).map_err(|error| {
        NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_allocation", format!("run-tier allocation failed: {error}"))
      })?;
      run = self.merge_run_group(&group)?;
      self.remove_runs(&group)?;
      level = level
        .checked_add(1)
        .ok_or_else(|| NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_run_levels", "run level overflowed"))?;
    }
  }

  fn take_retained_runs(&mut self) -> Result<Vec<RunFileV1>, NativeQueryOrderingWorkspaceErrorV1> {
    let mut runs = Vec::new();
    let retained_count = self.run_levels.iter().try_fold(0usize, |total, level| {
      total
        .checked_add(level.len())
        .ok_or_else(|| NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_run_count", "retained run count overflowed"))
    })?;
    runs.try_reserve_exact(retained_count).map_err(|error| {
      NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_allocation", format!("retained-run allocation failed: {error}"))
    })?;
    for level in &mut self.run_levels {
      runs.append(level);
    }
    Ok(runs)
  }

  fn merge_all_runs(&mut self, mut current: Vec<RunFileV1>) -> Result<RunFileV1, NativeQueryOrderingWorkspaceErrorV1> {
    while current.len() > 1 {
      require_not_cancelled(&self.cancellation)?;
      let mut next = Vec::new();
      next.try_reserve_exact(current.len().div_ceil(self.limits.merge_fan_in)).map_err(|error| {
        NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_allocation", format!("merge-run allocation failed: {error}"))
      })?;
      let mut pending = current.into_iter();
      loop {
        let mut group = Vec::new();
        group.try_reserve_exact(self.limits.merge_fan_in).map_err(|error| {
          NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_allocation", format!("merge-group allocation failed: {error}"))
        })?;
        for _ in 0..self.limits.merge_fan_in {
          let Some(run) = pending.next() else { break };
          group.push(run);
        }
        if group.is_empty() {
          break;
        }
        if group.len() == 1 {
          next.push(group.pop().ok_or_else(|| {
            NativeQueryOrderingWorkspaceErrorV1::corrupt("native_query_order_merge_group", "validated singleton merge group is empty")
          })?);
          continue;
        }
        let merged = self.merge_run_group(&group)?;
        self.remove_runs(&group)?;
        next.push(merged);
      }
      current = next;
    }
    current.pop().ok_or_else(|| {
      NativeQueryOrderingWorkspaceErrorV1::corrupt("native_query_order_final_run", "native query ordering produced no final run")
    })
  }

  fn merge_run_group(&mut self, group: &[RunFileV1]) -> Result<RunFileV1, NativeQueryOrderingWorkspaceErrorV1> {
    let mut record_count = 0u64;
    let mut readers = Vec::new();
    readers.try_reserve_exact(group.len()).map_err(|error| {
      NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_allocation", format!("merge-reader allocation failed: {error}"))
    })?;
    for run in group {
      require_not_cancelled(&self.cancellation)?;
      record_count = record_count.checked_add(run.record_count).ok_or_else(|| {
        NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_record_count", "merged run record count overflowed")
      })?;
      readers.push(RunReaderV1::open(self.directory.path(), run, self.hash_length)?);
    }
    let (run, path, mut output) = self.create_run(record_count)?;
    let header = encode_run_header(self.hash_length, record_count)?;
    output
      .write_all(&header)
      .map_err(|error| NativeQueryOrderingWorkspaceErrorV1::unavailable("native_query_order_run_write", error.to_string()))?;
    let mut heap = BinaryHeap::new();
    heap.try_reserve(group.len()).map_err(|error| {
      NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_allocation", format!("merge-heap allocation failed: {error}"))
    })?;
    for (reader_index, reader) in readers.iter_mut().enumerate() {
      if let Some(record) = reader.next_reference()? {
        heap.push(MergeHeadV1 { record, reader_index });
      }
    }
    let mut prior_file_key: Option<[u8; MAXIMUM_HASH_LENGTH]> = None;
    let mut written = 0u64;
    while let Some(head) = heap.pop() {
      require_not_cancelled(&self.cancellation)?;
      if prior_file_key.as_ref().is_some_and(|prior| &prior[..self.hash_length] == head.record.file_key()) {
        return Err(NativeQueryOrderingWorkspaceErrorV1::corrupt(
          "native_query_order_duplicate_file_key",
          "native query ordering encountered the same FileKey in multiple selected namespace rows",
        ));
      }
      let mut retained = [0; MAXIMUM_HASH_LENGTH];
      retained[..self.hash_length].copy_from_slice(head.record.file_key());
      prior_file_key = Some(retained);
      let encoded = encode_reference(&head.record)?;
      output
        .write_all(&encoded)
        .map_err(|error| NativeQueryOrderingWorkspaceErrorV1::unavailable("native_query_order_run_write", error.to_string()))?;
      written = written.checked_add(1).ok_or_else(|| {
        NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_record_count", "merged output count overflowed")
      })?;
      if let Some(record) = readers[head.reader_index].next_reference()? {
        heap.push(MergeHeadV1 { record, reader_index: head.reader_index });
      }
    }
    if written != record_count {
      return Err(NativeQueryOrderingWorkspaceErrorV1::corrupt(
        "native_query_order_record_count",
        "merged output count disagrees with its input run headers",
      ));
    }
    finish_run_file(&path, &mut output, run.byte_length)?;
    self.workspace_bytes = self
      .workspace_bytes
      .checked_add(run.byte_length)
      .ok_or_else(|| NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_workspace_bytes", "merged run bytes overflowed"))?;
    Ok(run)
  }

  fn remove_runs(&mut self, runs: &[RunFileV1]) -> Result<(), NativeQueryOrderingWorkspaceErrorV1> {
    for run in runs {
      let path = run_path(self.directory.path(), run.run_id);
      fs::remove_file(&path).map_err(|error| {
        NativeQueryOrderingWorkspaceErrorV1::unavailable("native_query_order_run_remove", format!("{}: {error}", path.display()))
      })?;
      self.workspace_bytes = self.workspace_bytes.checked_sub(run.byte_length).ok_or_else(|| {
        NativeQueryOrderingWorkspaceErrorV1::corrupt("native_query_order_workspace_bytes", "removed run exceeds workspace accounting")
      })?;
    }
    Ok(())
  }
}

pub(crate) struct NativeQueryOrderingWorkspaceV1 {
  directory: tempfile::TempDir,
  data_path: PathBuf,
  order_run: RunFileV1,
  hash_algorithm: HashAlgorithm,
  hash_length: usize,
  memory: Arc<MemoryCoordinator>,
  cancellation: CancellationToken,
  record_count: u64,
  data_length: u64,
  workspace_bytes: u64,
}

impl NativeQueryOrderingWorkspaceV1 {
  pub(crate) const fn record_count(&self) -> u64 {
    self.record_count
  }

  pub(crate) const fn workspace_bytes(&self) -> u64 {
    self.workspace_bytes
  }

  pub(crate) fn open_cursor(&self) -> Result<NativeQueryOrderingCursorV1, NativeQueryOrderingWorkspaceErrorV1> {
    let (run_file, run_count, data, decode_memory) = self.open_access()?;
    Ok(NativeQueryOrderingCursorV1 {
      run: RunReaderV1 { file: run_file, hash_length: self.hash_length, remaining: run_count },
      data,
      hash_algorithm: self.hash_algorithm,
      hash_length: self.hash_length,
      data_length: self.data_length,
      cancellation: self.cancellation.clone(),
      row: NativeQueryOrderingRowV1::empty(),
      _decode_memory: decode_memory,
    })
  }

  pub(crate) fn open_lookup(&self) -> Result<NativeQueryOrderingLookupV1, NativeQueryOrderingWorkspaceErrorV1> {
    let (run, record_count, data, decode_memory) = self.open_access()?;
    Ok(NativeQueryOrderingLookupV1 {
      run,
      data,
      hash_algorithm: self.hash_algorithm,
      hash_length: self.hash_length,
      record_count,
      data_length: self.data_length,
      cancellation: self.cancellation.clone(),
      row: NativeQueryOrderingRowV1::empty(),
      _decode_memory: decode_memory,
    })
  }

  fn open_access(&self) -> Result<(File, u64, File, MemoryReservation), NativeQueryOrderingWorkspaceErrorV1> {
    require_not_cancelled(&self.cancellation)?;
    let (run, record_count) = open_validated_run_file(self.directory.path(), &self.order_run, self.hash_length)?;
    let data = open_regular_file_no_follow(&self.data_path)
      .map_err(|error| NativeQueryOrderingWorkspaceErrorV1::unavailable("native_query_order_data_open", error.to_string()))?;
    validate_private_regular_file(&self.data_path, &data, "native query row spool").map_err(map_private_workspace_error)?;
    let observed = data
      .metadata()
      .map_err(|error| NativeQueryOrderingWorkspaceErrorV1::unavailable("native_query_order_data_metadata", error.to_string()))?
      .len();
    if observed != self.data_length {
      return Err(NativeQueryOrderingWorkspaceErrorV1::corrupt(
        "native_query_order_data_length",
        "row spool length changed after ordering completion",
      ));
    }
    let decode_bytes = u64::try_from(MAXIMUM_DECODE_WORKSPACE_BYTES).map_err(|error| {
      NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_decode_memory", format!("decode reservation exceeds u64: {error}"))
    })?;
    let decode_memory = self
      .memory
      .reserve(MemoryOwner::Query, decode_bytes, AdmissionClass::Workload)
      .map_err(|error| NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_decode_memory", error.to_string()))?;
    Ok((run, record_count, data, decode_memory))
  }

  #[cfg(test)]
  fn directory_path(&self) -> &Path {
    self.directory.path()
  }
}

pub(crate) struct NativeQueryOrderingCursorV1 {
  run: RunReaderV1,
  data: File,
  hash_algorithm: HashAlgorithm,
  hash_length: usize,
  data_length: u64,
  cancellation: CancellationToken,
  row: NativeQueryOrderingRowV1,
  _decode_memory: MemoryReservation,
}

impl NativeQueryOrderingCursorV1 {
  pub(crate) fn next_row(
    &mut self,
    cancellation: &CancellationToken,
  ) -> Result<Option<&NativeQueryOrderingRowV1>, NativeQueryOrderingWorkspaceErrorV1> {
    require_not_cancelled(cancellation)?;
    require_not_cancelled(&self.cancellation)?;
    let Some(reference) = self.run.next_reference()? else {
      return Ok(None);
    };
    read_ordered_row(&mut self.data, self.data_length, self.hash_algorithm, self.hash_length, &reference, &mut self.row)?;
    Ok(Some(&self.row))
  }
}

pub(crate) struct NativeQueryOrderingLookupV1 {
  run: File,
  data: File,
  hash_algorithm: HashAlgorithm,
  hash_length: usize,
  record_count: u64,
  data_length: u64,
  cancellation: CancellationToken,
  row: NativeQueryOrderingRowV1,
  _decode_memory: MemoryReservation,
}

impl NativeQueryOrderingLookupV1 {
  pub(crate) fn find_row(
    &mut self,
    file_key: &[u8],
    cancellation: &CancellationToken,
  ) -> Result<Option<&NativeQueryOrderingRowV1>, NativeQueryOrderingWorkspaceErrorV1> {
    require_not_cancelled(cancellation)?;
    require_not_cancelled(&self.cancellation)?;
    validate_identity(file_key, self.hash_length, "lookup FileKey")?;
    let mut lower = 0u64;
    let mut upper = self.record_count;
    while lower < upper {
      require_not_cancelled(cancellation)?;
      require_not_cancelled(&self.cancellation)?;
      let middle = lower + (upper - lower) / 2;
      let reference = read_reference_at(&mut self.run, middle, self.hash_length, self.record_count)?;
      match reference.file_key().cmp(file_key) {
        Ordering::Less => lower = middle + 1,
        Ordering::Greater => upper = middle,
        Ordering::Equal => return self.load_reference(reference).map(Some),
      }
    }
    let reference = self.scan_absent_target(file_key, cancellation)?;
    match reference {
      Some(reference) => self.load_reference(reference).map(Some),
      None => Ok(None),
    }
  }

  fn scan_absent_target(
    &mut self,
    file_key: &[u8],
    cancellation: &CancellationToken,
  ) -> Result<Option<WorkspaceReferenceV1>, NativeQueryOrderingWorkspaceErrorV1> {
    let mut prior = None;
    let mut found = None;
    for index in 0..self.record_count {
      require_not_cancelled(cancellation)?;
      require_not_cancelled(&self.cancellation)?;
      let reference = read_reference_at(&mut self.run, index, self.hash_length, self.record_count)?;
      if prior.as_ref().is_some_and(|prior: &WorkspaceReferenceV1| prior.file_key() >= reference.file_key()) {
        return Err(NativeQueryOrderingWorkspaceErrorV1::corrupt(
          "native_query_order_lookup_order",
          "ordering run is not in strict FileKey order",
        ));
      }
      if reference.file_key() == file_key {
        found = Some(reference.clone());
      }
      prior = Some(reference);
    }
    Ok(found)
  }

  fn load_reference(&mut self, reference: WorkspaceReferenceV1) -> Result<&NativeQueryOrderingRowV1, NativeQueryOrderingWorkspaceErrorV1> {
    read_ordered_row(&mut self.data, self.data_length, self.hash_algorithm, self.hash_length, &reference, &mut self.row)?;
    Ok(&self.row)
  }
}

pub(crate) struct NativeQueryOrderingRowV1 {
  file_key: [u8; MAXIMUM_HASH_LENGTH],
  scope_id: [u8; MAXIMUM_HASH_LENGTH],
  hash_length: usize,
  hash_algorithm: HashAlgorithm,
  entity_version: u8,
  frame: Vec<u8>,
}

impl NativeQueryOrderingRowV1 {
  fn empty() -> Self {
    Self {
      file_key: [0; MAXIMUM_HASH_LENGTH],
      scope_id: [0; MAXIMUM_HASH_LENGTH],
      hash_length: 0,
      hash_algorithm: HashAlgorithm::Blake3_256,
      entity_version: 0,
      frame: Vec::new(),
    }
  }

  pub(crate) fn file_key(&self) -> &[u8] {
    &self.file_key[..self.hash_length]
  }

  pub(crate) fn scope_id(&self) -> Option<&[u8]> {
    let scope_id = &self.scope_id[..self.hash_length];
    (!scope_id.iter().all(|byte| *byte == 0)).then_some(scope_id)
  }

  pub(crate) fn record_revision(&self) -> &[u8] {
    &self.frame[DATA_HEADER_LENGTH..DATA_HEADER_LENGTH + self.hash_length]
  }

  pub(crate) const fn entity_version(&self) -> u8 {
    self.entity_version
  }

  pub(crate) fn encoded_file_record(&self) -> &[u8] {
    &self.frame[DATA_HEADER_LENGTH + self.hash_length..]
  }

  #[cfg(test)]
  pub(crate) const fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }
}

struct RunReaderV1 {
  file: File,
  hash_length: usize,
  remaining: u64,
}

impl RunReaderV1 {
  fn open(directory: &Path, run: &RunFileV1, hash_length: usize) -> Result<Self, NativeQueryOrderingWorkspaceErrorV1> {
    let (file, count) = open_validated_run_file(directory, run, hash_length)?;
    Ok(Self { file, hash_length, remaining: count })
  }

  fn next_reference(&mut self) -> Result<Option<WorkspaceReferenceV1>, NativeQueryOrderingWorkspaceErrorV1> {
    if self.remaining == 0 {
      return Ok(None);
    }
    let length = reference_length(self.hash_length)?;
    let mut bytes = [0; 2 * MAXIMUM_HASH_LENGTH + 20];
    read_exact_classified(&mut self.file, &mut bytes[..length], "native_query_order_run_record")?;
    let reference = decode_reference(&bytes[..length], self.hash_length)?;
    self.remaining -= 1;
    Ok(Some(reference))
  }
}

fn open_validated_run_file(
  directory: &Path,
  run: &RunFileV1,
  hash_length: usize,
) -> Result<(File, u64), NativeQueryOrderingWorkspaceErrorV1> {
  let path = run_path(directory, run.run_id);
  let mut file = open_regular_file_no_follow(&path)
    .map_err(|error| NativeQueryOrderingWorkspaceErrorV1::unavailable("native_query_order_run_open", error.to_string()))?;
  validate_private_regular_file(&path, &file, "native query ordering run").map_err(map_private_workspace_error)?;
  let observed = file
    .metadata()
    .map_err(|error| NativeQueryOrderingWorkspaceErrorV1::unavailable("native_query_order_run_metadata", error.to_string()))?
    .len();
  if observed != run.byte_length {
    return Err(NativeQueryOrderingWorkspaceErrorV1::corrupt(
      "native_query_order_run_length",
      "ordering run physical length changed after completion",
    ));
  }
  let mut header = [0; RUN_HEADER_LENGTH];
  read_exact_classified(&mut file, &mut header, "native_query_order_run_header")?;
  let count = decode_run_header(&header, hash_length)?;
  if count != run.record_count || run_byte_length(hash_length, count)? != observed {
    return Err(NativeQueryOrderingWorkspaceErrorV1::corrupt(
      "native_query_order_run_header",
      "ordering run header disagrees with its completed receipt",
    ));
  }
  Ok((file, count))
}

fn read_reference_at(
  file: &mut File,
  index: u64,
  hash_length: usize,
  record_count: u64,
) -> Result<WorkspaceReferenceV1, NativeQueryOrderingWorkspaceErrorV1> {
  if index >= record_count {
    return Err(NativeQueryOrderingWorkspaceErrorV1::corrupt(
      "native_query_order_lookup_index",
      "ordering lookup index exceeds its completed run",
    ));
  }
  let length = u64::try_from(reference_length(hash_length)?).map_err(|error| {
    NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_lookup_offset", format!("reference length exceeds u64: {error}"))
  })?;
  let offset = index
    .checked_mul(length)
    .and_then(|offset| offset.checked_add(RUN_HEADER_LENGTH as u64))
    .ok_or_else(|| NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_lookup_offset", "run offset overflowed"))?;
  file
    .seek(SeekFrom::Start(offset))
    .map_err(|error| NativeQueryOrderingWorkspaceErrorV1::unavailable("native_query_order_lookup_seek", error.to_string()))?;
  let mut bytes = [0; 2 * MAXIMUM_HASH_LENGTH + 20];
  read_exact_classified(file, &mut bytes[..length as usize], "native_query_order_lookup_read")?;
  decode_reference(&bytes[..length as usize], hash_length)
}

fn read_ordered_row(
  data: &mut File,
  data_length: u64,
  hash_algorithm: HashAlgorithm,
  hash_length: usize,
  reference: &WorkspaceReferenceV1,
  row: &mut NativeQueryOrderingRowV1,
) -> Result<(), NativeQueryOrderingWorkspaceErrorV1> {
  let frame_end = reference
    .data_offset
    .checked_add(u64::from(reference.data_length))
    .ok_or_else(|| NativeQueryOrderingWorkspaceErrorV1::corrupt("native_query_order_data_reference", "row frame range overflowed"))?;
  if frame_end > data_length || reference.data_length as usize > MAXIMUM_ENCODED_FILE_RECORD_BYTES + DATA_HEADER_LENGTH + hash_length {
    return Err(NativeQueryOrderingWorkspaceErrorV1::corrupt(
      "native_query_order_data_reference",
      "ordering run references a row frame outside the completed spool",
    ));
  }
  data
    .seek(SeekFrom::Start(reference.data_offset))
    .map_err(|error| NativeQueryOrderingWorkspaceErrorV1::unavailable("native_query_order_data_seek", error.to_string()))?;
  row.frame.clear();
  row.frame.try_reserve_exact(reference.data_length as usize).map_err(|error| {
    NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_decode_allocation", format!("row-frame allocation failed: {error}"))
  })?;
  row.frame.resize(reference.data_length as usize, 0);
  read_exact_classified(data, &mut row.frame, "native_query_order_data_read")?;
  validate_data_frame(&row.frame, hash_length, reference.data_checksum)?;
  row.file_key = reference.file_key;
  row.scope_id = reference.scope_id;
  row.hash_length = hash_length;
  row.entity_version = row.frame[6];
  row.hash_algorithm = hash_algorithm;
  Ok(())
}

#[derive(Eq, PartialEq)]
struct MergeHeadV1 {
  record: WorkspaceReferenceV1,
  reader_index: usize,
}

impl Ord for MergeHeadV1 {
  fn cmp(&self, other: &Self) -> Ordering {
    other.record.file_key().cmp(self.record.file_key()).then_with(|| other.reader_index.cmp(&self.reader_index))
  }
}

impl PartialOrd for MergeHeadV1 {
  fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

fn require_not_cancelled(cancellation: &CancellationToken) -> Result<(), NativeQueryOrderingWorkspaceErrorV1> {
  if cancellation.is_cancelled() {
    Err(NativeQueryOrderingWorkspaceErrorV1::cancelled())
  } else {
    Ok(())
  }
}

fn validate_identity(identity: &[u8], hash_length: usize, role: &str) -> Result<(), NativeQueryOrderingWorkspaceErrorV1> {
  if identity.len() != hash_length || identity.iter().all(|byte| *byte == 0) {
    return Err(NativeQueryOrderingWorkspaceErrorV1::invalid(
      "native_query_order_identity",
      format!("{role} has the wrong width or is all zero"),
    ));
  }
  Ok(())
}

fn reject_duplicate_references(records: &[WorkspaceReferenceV1]) -> Result<(), NativeQueryOrderingWorkspaceErrorV1> {
  if records.windows(2).any(|pair| pair[0].file_key() == pair[1].file_key()) {
    return Err(NativeQueryOrderingWorkspaceErrorV1::corrupt(
      "native_query_order_duplicate_file_key",
      "native query ordering encountered the same FileKey in multiple selected namespace rows",
    ));
  }
  Ok(())
}

fn maximum_run_levels(
  maximum_records: u64,
  maximum_records_per_run: usize,
  merge_fan_in: usize,
) -> Result<usize, NativeQueryOrderingWorkspaceErrorV1> {
  let records_per_run = u64::try_from(maximum_records_per_run).map_err(|error| {
    NativeQueryOrderingWorkspaceErrorV1::invalid("native_query_order_limits", format!("records per run exceed u64: {error}"))
  })?;
  let fan_in = u64::try_from(merge_fan_in).map_err(|error| {
    NativeQueryOrderingWorkspaceErrorV1::invalid("native_query_order_limits", format!("merge fan-in exceeds u64: {error}"))
  })?;
  let mut runs = maximum_records.div_ceil(records_per_run);
  let mut levels = 1usize;
  while runs > 1 {
    runs = runs.div_ceil(fan_in);
    levels = levels
      .checked_add(1)
      .ok_or_else(|| NativeQueryOrderingWorkspaceErrorV1::invalid("native_query_order_limits", "run-level count overflowed"))?;
  }
  if levels > MAXIMUM_RUN_LEVELS {
    return Err(NativeQueryOrderingWorkspaceErrorV1::invalid(
      "native_query_order_limits",
      "native query ordering run-level count exceeds its fixed safety maximum",
    ));
  }
  Ok(levels)
}

fn run_path(directory: &Path, run_id: u64) -> PathBuf {
  directory.join(format!("run-{run_id:020}.aqor"))
}

fn reference_length(hash_length: usize) -> Result<usize, NativeQueryOrderingWorkspaceErrorV1> {
  hash_length
    .checked_mul(2)
    .and_then(|length| length.checked_add(20))
    .ok_or_else(|| NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_reference_length", "reference length overflowed"))
}

fn run_byte_length(hash_length: usize, record_count: u64) -> Result<u64, NativeQueryOrderingWorkspaceErrorV1> {
  let reference_length = u64::try_from(reference_length(hash_length)?).map_err(|error| {
    NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_run_length", format!("reference length exceeds u64: {error}"))
  })?;
  record_count
    .checked_mul(reference_length)
    .and_then(|length| length.checked_add(RUN_HEADER_LENGTH as u64))
    .ok_or_else(|| NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_run_length", "run length overflowed"))
}

fn encode_run_header(hash_length: usize, record_count: u64) -> Result<[u8; RUN_HEADER_LENGTH], NativeQueryOrderingWorkspaceErrorV1> {
  let record_length = u32::try_from(reference_length(hash_length)?).map_err(|error| {
    NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_run_header", format!("reference length exceeds u32: {error}"))
  })?;
  let mut header = [0; RUN_HEADER_LENGTH];
  header[..8].copy_from_slice(RUN_MAGIC);
  header[8..10].copy_from_slice(&RUN_VERSION.to_le_bytes());
  header[10] = hash_length as u8;
  header[12..16].copy_from_slice(&record_length.to_le_bytes());
  header[16..24].copy_from_slice(&record_count.to_le_bytes());
  let checksum = crc32fast::hash(&header[..28]);
  header[28..32].copy_from_slice(&checksum.to_le_bytes());
  Ok(header)
}

fn decode_run_header(header: &[u8; RUN_HEADER_LENGTH], hash_length: usize) -> Result<u64, NativeQueryOrderingWorkspaceErrorV1> {
  let version = u16::from_le_bytes([header[8], header[9]]);
  let record_length = u32::from_le_bytes([header[12], header[13], header[14], header[15]]) as usize;
  let count = u64::from_le_bytes([header[16], header[17], header[18], header[19], header[20], header[21], header[22], header[23]]);
  let checksum = u32::from_le_bytes([header[28], header[29], header[30], header[31]]);
  if &header[..8] != RUN_MAGIC
    || version != RUN_VERSION
    || header[10] as usize != hash_length
    || header[11] != 0
    || header[24..28].iter().any(|byte| *byte != 0)
    || record_length != reference_length(hash_length)?
    || checksum != crc32fast::hash(&header[..28])
  {
    return Err(NativeQueryOrderingWorkspaceErrorV1::corrupt(
      "native_query_order_run_header",
      "ordering run header magic, version, width, reserve, length, or checksum is invalid",
    ));
  }
  Ok(count)
}

fn encode_reference(reference: &WorkspaceReferenceV1) -> Result<Vec<u8>, NativeQueryOrderingWorkspaceErrorV1> {
  let hash_length = reference.hash_length as usize;
  let length = reference_length(hash_length)?;
  let mut bytes = Vec::new();
  bytes.try_reserve_exact(length).map_err(|error| {
    NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_allocation", format!("reference encoding allocation failed: {error}"))
  })?;
  bytes.extend_from_slice(reference.file_key());
  bytes.extend_from_slice(&reference.scope_id[..hash_length]);
  bytes.extend_from_slice(&reference.data_offset.to_le_bytes());
  bytes.extend_from_slice(&reference.data_length.to_le_bytes());
  bytes.extend_from_slice(&reference.data_checksum.to_le_bytes());
  let checksum = crc32fast::hash(&bytes);
  bytes.extend_from_slice(&checksum.to_le_bytes());
  Ok(bytes)
}

fn decode_reference(bytes: &[u8], hash_length: usize) -> Result<WorkspaceReferenceV1, NativeQueryOrderingWorkspaceErrorV1> {
  if bytes.len() != reference_length(hash_length)? {
    return Err(NativeQueryOrderingWorkspaceErrorV1::corrupt(
      "native_query_order_reference_length",
      "ordering run reference has the wrong fixed length",
    ));
  }
  let payload_end = bytes.len() - 4;
  let checksum = u32::from_le_bytes([bytes[payload_end], bytes[payload_end + 1], bytes[payload_end + 2], bytes[payload_end + 3]]);
  if checksum != crc32fast::hash(&bytes[..payload_end]) {
    return Err(NativeQueryOrderingWorkspaceErrorV1::corrupt(
      "native_query_order_reference_checksum",
      "ordering run reference checksum is invalid",
    ));
  }
  let scalar_offset = hash_length * 2;
  let data_offset = u64::from_le_bytes([
    bytes[scalar_offset],
    bytes[scalar_offset + 1],
    bytes[scalar_offset + 2],
    bytes[scalar_offset + 3],
    bytes[scalar_offset + 4],
    bytes[scalar_offset + 5],
    bytes[scalar_offset + 6],
    bytes[scalar_offset + 7],
  ]);
  let data_length =
    u32::from_le_bytes([bytes[scalar_offset + 8], bytes[scalar_offset + 9], bytes[scalar_offset + 10], bytes[scalar_offset + 11]]);
  let data_checksum =
    u32::from_le_bytes([bytes[scalar_offset + 12], bytes[scalar_offset + 13], bytes[scalar_offset + 14], bytes[scalar_offset + 15]]);
  let encoded_scope_id = &bytes[hash_length..scalar_offset];
  let scope_id = (!encoded_scope_id.iter().all(|byte| *byte == 0)).then_some(encoded_scope_id);
  match WorkspaceReferenceV1::new(&bytes[..hash_length], scope_id, data_offset, data_length, data_checksum) {
    Ok(reference) => Ok(reference),
    Err(error) => Err(NativeQueryOrderingWorkspaceErrorV1::corrupt(error.code, error.context)),
  }
}

fn validate_data_frame(frame: &[u8], hash_length: usize, expected_checksum: u32) -> Result<(), NativeQueryOrderingWorkspaceErrorV1> {
  if frame.len() < DATA_HEADER_LENGTH + hash_length {
    return Err(NativeQueryOrderingWorkspaceErrorV1::corrupt("native_query_order_data_frame", "row spool frame is truncated"));
  }
  let version = u16::from_le_bytes([frame[4], frame[5]]);
  let total_length = u32::from_le_bytes([frame[8], frame[9], frame[10], frame[11]]) as usize;
  let checksum = u32::from_le_bytes([frame[12], frame[13], frame[14], frame[15]]);
  if &frame[..4] != DATA_MAGIC
    || version != DATA_VERSION
    || !matches!(frame[6], 0 | 1)
    || frame[7] as usize != hash_length
    || total_length != frame.len()
    || checksum != expected_checksum
    || checksum != crc32fast::hash(&frame[DATA_HEADER_LENGTH..])
  {
    return Err(NativeQueryOrderingWorkspaceErrorV1::corrupt(
      "native_query_order_data_frame",
      "row spool frame magic, version, width, length, or checksum is invalid",
    ));
  }
  let revision = &frame[DATA_HEADER_LENGTH..DATA_HEADER_LENGTH + hash_length];
  if revision.iter().all(|byte| *byte == 0) {
    return Err(NativeQueryOrderingWorkspaceErrorV1::corrupt(
      "native_query_order_data_frame",
      "row spool frame contains an all-zero RecordRevision",
    ));
  }
  Ok(())
}

fn finish_run_file(path: &Path, file: &mut File, expected_length: u64) -> Result<(), NativeQueryOrderingWorkspaceErrorV1> {
  file.flush().map_err(|error| NativeQueryOrderingWorkspaceErrorV1::unavailable("native_query_order_run_flush", error.to_string()))?;
  let observed = file
    .metadata()
    .map_err(|error| NativeQueryOrderingWorkspaceErrorV1::unavailable("native_query_order_run_metadata", error.to_string()))?
    .len();
  if observed != expected_length {
    return Err(NativeQueryOrderingWorkspaceErrorV1::corrupt(
      "native_query_order_run_length",
      format!("ordering run {} wrote {observed} bytes instead of {expected_length}", path.display()),
    ));
  }
  Ok(())
}

fn read_exact_classified(file: &mut File, bytes: &mut [u8], code: &'static str) -> Result<(), NativeQueryOrderingWorkspaceErrorV1> {
  file.read_exact(bytes).map_err(|error| {
    if error.kind() == std::io::ErrorKind::UnexpectedEof {
      NativeQueryOrderingWorkspaceErrorV1::corrupt(code, error.to_string())
    } else {
      NativeQueryOrderingWorkspaceErrorV1::unavailable(code, error.to_string())
    }
  })
}

fn map_private_workspace_error(error: PrivateWorkspaceErrorV1) -> NativeQueryOrderingWorkspaceErrorV1 {
  match error {
    PrivateWorkspaceErrorV1::Path(context) => NativeQueryOrderingWorkspaceErrorV1::invalid("native_query_order_private_path", context),
    PrivateWorkspaceErrorV1::Capacity(context) => {
      NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_private_capacity", context)
    }
    #[cfg(windows)]
    PrivateWorkspaceErrorV1::Allocation(context) => {
      NativeQueryOrderingWorkspaceErrorV1::resource("native_query_order_private_allocation", context)
    }
    #[cfg(windows)]
    PrivateWorkspaceErrorV1::State(context) => {
      NativeQueryOrderingWorkspaceErrorV1::unavailable("native_query_order_private_state", context)
    }
    PrivateWorkspaceErrorV1::Io { operation, source } => {
      NativeQueryOrderingWorkspaceErrorV1::unavailable("native_query_order_private_io", format!("{operation}: {source}"))
    }
    PrivateWorkspaceErrorV1::Durability(source) => {
      NativeQueryOrderingWorkspaceErrorV1::unavailable("native_query_order_private_durability", source.to_string())
    }
  }
}

#[cfg(test)]
mod tests {
  use std::collections::BTreeMap;
  use std::fs::OpenOptions;
  use std::io::{Read, Seek, SeekFrom, Write};

  #[cfg(unix)]
  use std::os::unix::fs::PermissionsExt;

  use crate::engine::memory_coordinator::{AdmissionClass, MemoryOwner, MemoryPolicy};

  use super::*;

  fn limits(maximum_workspace_bytes: u64, maximum_records_per_run: usize) -> NativeQueryOrderingWorkspaceLimitsV1 {
    NativeQueryOrderingWorkspaceLimitsV1::new(32, maximum_workspace_bytes, 8 * 1024 * 1024, maximum_records_per_run, 2).unwrap()
  }

  fn memory() -> Arc<MemoryCoordinator> {
    Arc::new(MemoryCoordinator::new(MemoryPolicy::new(32 * 1024 * 1024, 64 * 1024 * 1024, 1, 1024).unwrap()))
  }

  fn record_parts(algorithm: HashAlgorithm, path: &str) -> (Vec<u8>, Vec<u8>, FileRecord) {
    let mut record = FileRecord::new(path.to_string(), Some("application/json".to_string()), 0, Vec::new());
    record.content_hash = digest_parts(algorithm, &[b""]);
    let encoded = record.serialize_for_version(algorithm.hash_length(), 1).unwrap();
    let record_revision = digest_parts(algorithm, &[b"filec:", &encoded]);
    let file_key = digest_parts(algorithm, &[b"file:", path.as_bytes()]);
    (file_key, record_revision, record)
  }

  fn append_record(builder: &mut NativeQueryOrderingWorkspaceBuilderV1, algorithm: HashAlgorithm, scope_id: &[u8], path: &str) {
    let (file_key, record_revision, record) = record_parts(algorithm, path);
    builder.append_parts(&file_key, Some(scope_id), &record_revision, 1, &record).unwrap();
  }

  fn one_record_workspace(
    parent: &Path,
    algorithm: HashAlgorithm,
    memory: Arc<MemoryCoordinator>,
    cancellation: CancellationToken,
  ) -> NativeQueryOrderingWorkspaceV1 {
    let mut builder =
      NativeQueryOrderingWorkspaceBuilderV1::new(parent, algorithm, memory, cancellation, limits(16 * 1024 * 1024, 2)).unwrap();
    let scope_id = digest_parts(algorithm, &[b"scope"]);
    append_record(&mut builder, algorithm, &scope_id, "/docs/one.json");
    builder.finish().unwrap()
  }

  #[test]
  fn external_runs_merge_to_global_file_key_order_and_support_independent_readers() {
    for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
      let directory = tempfile::tempdir().unwrap();
      let memory = memory();
      let cancellation = CancellationToken::new();
      let mut builder = NativeQueryOrderingWorkspaceBuilderV1::new(
        directory.path(),
        algorithm,
        Arc::clone(&memory),
        cancellation.clone(),
        limits(16 * 1024 * 1024, 2),
      )
      .unwrap();
      let scope_id = digest_parts(algorithm, &[b"scope"]);
      let mut expected = BTreeMap::new();
      for name in ["a.json", "b.json", "c.json", "d.json", "e.json", "f.json", "g.json"] {
        let path = format!("/docs/{name}");
        let (file_key, record_revision, record) = record_parts(algorithm, &path);
        builder.append_parts(&file_key, Some(&scope_id), &record_revision, 1, &record).unwrap();
        expected.insert(file_key, (scope_id.clone(), record_revision, path));
      }

      let workspace = builder.finish().unwrap();
      assert_eq!(workspace.record_count(), 7);
      assert!(workspace.workspace_bytes() > 0);
      assert!(workspace.directory_path().starts_with(directory.path()));
      #[cfg(unix)]
      {
        assert_eq!(workspace.directory.path().metadata().unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(workspace.data_path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(
          run_path(workspace.directory.path(), workspace.order_run.run_id).metadata().unwrap().permissions().mode() & 0o777,
          0o600
        );
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);

      for _ in 0..2 {
        let mut cursor = workspace.open_cursor().unwrap();
        let mut observed = Vec::new();
        while let Some(row) = cursor.next_row(&cancellation).unwrap() {
          let record =
            FileRecord::deserialize(row.encoded_file_record(), row.hash_algorithm().hash_length(), row.entity_version()).unwrap();
          observed.push((row.file_key().to_vec(), row.scope_id().unwrap().to_vec(), row.record_revision().to_vec(), record.path));
        }
        let expected_rows = expected
          .iter()
          .map(|(file_key, (scope_id, revision, path))| (file_key.clone(), scope_id.clone(), revision.clone(), path.clone()))
          .collect::<Vec<_>>();
        assert_eq!(observed, expected_rows);
        drop(cursor);
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
      }
      let workspace_path = workspace.directory_path().to_path_buf();
      drop(workspace);
      assert!(!workspace_path.exists());
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    }
  }

  #[test]
  fn exact_lookup_finds_both_hash_widths_and_proves_absence_without_scanning_database_state() {
    for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
      let directory = tempfile::tempdir().unwrap();
      let memory = memory();
      let cancellation = CancellationToken::new();
      let mut builder = NativeQueryOrderingWorkspaceBuilderV1::new(
        directory.path(),
        algorithm,
        Arc::clone(&memory),
        cancellation.clone(),
        limits(16 * 1024 * 1024, 2),
      )
      .unwrap();
      let scope_id = digest_parts(algorithm, &[b"scope"]);
      let mut expected = BTreeMap::new();
      for name in ["g.json", "a.json", "e.json", "b.json", "f.json", "c.json", "d.json"] {
        let path = format!("/docs/{name}");
        let (file_key, record_revision, record) = record_parts(algorithm, &path);
        builder.append_parts(&file_key, Some(&scope_id), &record_revision, 1, &record).unwrap();
        expected.insert(file_key, (record_revision, path));
      }
      let workspace = builder.finish().unwrap();
      let mut lookup = workspace.open_lookup().unwrap();

      for (file_key, (record_revision, path)) in expected.iter().rev() {
        let row = lookup.find_row(file_key, &cancellation).unwrap().unwrap();
        assert_eq!(row.file_key(), file_key);
        assert_eq!(row.record_revision(), record_revision);
        let record = FileRecord::deserialize(row.encoded_file_record(), algorithm.hash_length(), row.entity_version()).unwrap();
        assert_eq!(&record.path, path);
      }
      let absent = digest_parts(algorithm, &[b"absent FileKey"]);
      assert!(lookup.find_row(&absent, &cancellation).unwrap().is_none());
      drop(lookup);
      drop(workspace);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    }
  }

  #[test]
  fn incremental_compaction_keeps_run_metadata_logarithmically_bounded() {
    let algorithm = HashAlgorithm::Blake3_256;
    let directory = tempfile::tempdir().unwrap();
    let memory = memory();
    let cancellation = CancellationToken::new();
    let limits = NativeQueryOrderingWorkspaceLimitsV1::new(128, 64 * 1024 * 1024, 8 * 1024 * 1024, 1, 2).unwrap();
    let maximum_run_levels = limits.maximum_run_levels;
    let mut builder =
      NativeQueryOrderingWorkspaceBuilderV1::new(directory.path(), algorithm, Arc::clone(&memory), cancellation, limits).unwrap();
    let scope_id = digest_parts(algorithm, &[b"scope"]);
    for index in 0..128 {
      append_record(&mut builder, algorithm, &scope_id, &format!("/docs/{index:04}.json"));
      let retained_runs = builder.run_levels.iter().map(Vec::len).sum::<usize>();
      assert!(retained_runs <= maximum_run_levels);
    }
    let workspace = builder.finish().unwrap();
    assert_eq!(workspace.record_count(), 128);
    drop(workspace);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }

  #[test]
  fn documents_without_an_effective_scope_round_trip_explicitly() {
    let algorithm = HashAlgorithm::Blake3_256;
    let directory = tempfile::tempdir().unwrap();
    let memory = memory();
    let cancellation = CancellationToken::new();
    let mut builder = NativeQueryOrderingWorkspaceBuilderV1::new(
      directory.path(),
      algorithm,
      Arc::clone(&memory),
      cancellation.clone(),
      limits(16 * 1024 * 1024, 2),
    )
    .unwrap();
    let (file_key, record_revision, record) = record_parts(algorithm, "/docs/unconfigured.json");
    builder.append_parts(&file_key, None, &record_revision, 1, &record).unwrap();
    let workspace = builder.finish().unwrap();
    let mut cursor = workspace.open_cursor().unwrap();
    let row = cursor.next_row(&cancellation).unwrap().unwrap();
    assert_eq!(row.file_key(), file_key);
    assert_eq!(row.scope_id(), None);
    assert_eq!(row.record_revision(), record_revision);
    assert!(cursor.next_row(&cancellation).unwrap().is_none());
    drop(cursor);
    drop(workspace);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }

  #[test]
  fn duplicate_file_keys_across_runs_fail_closed_and_cleanup() {
    let algorithm = HashAlgorithm::Blake3_256;
    let directory = tempfile::tempdir().unwrap();
    let memory = memory();
    let cancellation = CancellationToken::new();
    let mut builder = NativeQueryOrderingWorkspaceBuilderV1::new(
      directory.path(),
      algorithm,
      Arc::clone(&memory),
      cancellation,
      limits(16 * 1024 * 1024, 1),
    )
    .unwrap();
    let workspace_path = builder.directory.path().to_path_buf();
    let scope_id = digest_parts(algorithm, &[b"scope"]);
    append_record(&mut builder, algorithm, &scope_id, "/docs/repeated.json");
    append_record(&mut builder, algorithm, &scope_id, "/docs/repeated.json");
    let error = builder.finish().err().unwrap();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Corrupt);
    assert_eq!(error.code(), "native_query_order_duplicate_file_key");
    assert!(!workspace_path.exists());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }

  #[test]
  fn workspace_refuses_invalid_inputs_before_mutating_the_spool() {
    let algorithm = HashAlgorithm::Blake3_256;
    let directory = tempfile::tempdir().unwrap();
    let memory = memory();
    let cancellation = CancellationToken::new();
    let mut builder = NativeQueryOrderingWorkspaceBuilderV1::new(
      directory.path(),
      algorithm,
      Arc::clone(&memory),
      cancellation,
      limits(16 * 1024 * 1024, 2),
    )
    .unwrap();
    let scope_id = digest_parts(algorithm, &[b"scope"]);
    let (file_key, record_revision, record) = record_parts(algorithm, "/docs/invalid.json");

    let error = builder.append_parts(&file_key[..31], Some(&scope_id), &record_revision, 1, &record).unwrap_err();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Invalid);
    assert_eq!(error.code(), "native_query_order_identity");
    let error = builder.append_parts(&file_key, Some(&scope_id), &record_revision, 2, &record).unwrap_err();
    assert_eq!(error.code(), "native_query_order_file_version");
    let mut wrong_revision = record_revision.clone();
    wrong_revision[0] ^= 0xff;
    let error = builder.append_parts(&file_key, Some(&scope_id), &wrong_revision, 1, &record).unwrap_err();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Corrupt);
    assert_eq!(error.code(), "native_query_order_record_revision");
    let mut noncanonical = record.clone();
    noncanonical.path = "/docs/../invalid.json".to_string();
    let noncanonical_key = digest_parts(algorithm, &[b"file:", noncanonical.path.as_bytes()]);
    let noncanonical_encoded = noncanonical.serialize_for_version(algorithm.hash_length(), 1).unwrap();
    let noncanonical_revision = digest_parts(algorithm, &[b"filec:", &noncanonical_encoded]);
    let error = builder.append_parts(&noncanonical_key, Some(&scope_id), &noncanonical_revision, 1, &noncanonical).unwrap_err();
    assert_eq!(error.code(), "native_query_order_file_path");
    assert_eq!(builder.data.metadata().unwrap().len(), 0);
    assert_eq!(builder.record_count, 0);
    drop(builder);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);

    let error = NativeQueryOrderingWorkspaceBuilderV1::new(
      directory.path(),
      HashAlgorithm::Sha256,
      memory,
      CancellationToken::new(),
      limits(16 * 1024 * 1024, 2),
    )
    .err()
    .unwrap();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Invalid);
    assert_eq!(error.code(), "native_query_order_hash_algorithm");
  }

  #[test]
  fn disk_and_memory_admission_fail_as_typed_resource_limits() {
    let algorithm = HashAlgorithm::Blake3_256;
    let directory = tempfile::tempdir().unwrap();
    let memory = memory();
    let mut builder =
      NativeQueryOrderingWorkspaceBuilderV1::new(directory.path(), algorithm, Arc::clone(&memory), CancellationToken::new(), limits(1, 2))
        .unwrap();
    let scope_id = digest_parts(algorithm, &[b"scope"]);
    let (file_key, record_revision, record) = record_parts(algorithm, "/docs/full.json");
    let error = builder.append_parts(&file_key, Some(&scope_id), &record_revision, 1, &record).unwrap_err();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Resource);
    assert_eq!(error.code(), "native_query_order_workspace_bytes");
    assert_eq!(builder.data.metadata().unwrap().len(), 0);
    drop(builder);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);

    let constrained = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(512 * 1024, 1024 * 1024, 1, 512 * 1024).unwrap()));
    let error = NativeQueryOrderingWorkspaceBuilderV1::new(
      directory.path(),
      algorithm,
      Arc::clone(&constrained),
      CancellationToken::new(),
      limits(16 * 1024 * 1024, 2),
    )
    .err()
    .unwrap();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Resource);
    assert_eq!(error.code(), "native_query_order_memory");
    assert_eq!(constrained.snapshot().unwrap().reserved_bytes, 0);

    let cursor_memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(39 * 1024 * 1024, 40 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
    let workspace = one_record_workspace(directory.path(), algorithm, Arc::clone(&cursor_memory), CancellationToken::new());
    let held = cursor_memory.reserve(MemoryOwner::Query, 24 * 1024 * 1024, AdmissionClass::Workload).unwrap();
    let error = workspace.open_cursor().err().unwrap();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Resource);
    assert_eq!(error.code(), "native_query_order_decode_memory");
    let error = workspace.open_lookup().err().unwrap();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Resource);
    assert_eq!(error.code(), "native_query_order_decode_memory");
    drop(held);
    drop(workspace.open_cursor().unwrap());
    drop(workspace.open_lookup().unwrap());
    drop(workspace);
    assert_eq!(cursor_memory.snapshot().unwrap().reserved_bytes, 0);
  }

  #[test]
  fn cancellation_fails_at_builder_finish_open_and_cursor_boundaries() {
    let algorithm = HashAlgorithm::Blake3_256;
    let directory = tempfile::tempdir().unwrap();
    let memory = memory();
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let error =
      NativeQueryOrderingWorkspaceBuilderV1::new(directory.path(), algorithm, Arc::clone(&memory), cancelled, limits(16 * 1024 * 1024, 2))
        .err()
        .unwrap();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Cancelled);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);

    let shared = CancellationToken::new();
    let mut builder = NativeQueryOrderingWorkspaceBuilderV1::new(
      directory.path(),
      algorithm,
      Arc::clone(&memory),
      shared.clone(),
      limits(16 * 1024 * 1024, 2),
    )
    .unwrap();
    let workspace_path = builder.directory.path().to_path_buf();
    let scope_id = digest_parts(algorithm, &[b"scope"]);
    append_record(&mut builder, algorithm, &scope_id, "/docs/cancelled.json");
    shared.cancel();
    let error = builder.finish().err().unwrap();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Cancelled);
    assert!(!workspace_path.exists());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);

    let shared = CancellationToken::new();
    let workspace = one_record_workspace(directory.path(), algorithm, Arc::clone(&memory), shared.clone());
    shared.cancel();
    let error = workspace.open_cursor().err().unwrap();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Cancelled);
    let error = workspace.open_lookup().err().unwrap();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Cancelled);
    drop(workspace);

    let workspace = one_record_workspace(directory.path(), algorithm, Arc::clone(&memory), CancellationToken::new());
    let mut cursor = workspace.open_cursor().unwrap();
    let request = CancellationToken::new();
    request.cancel();
    let error = cursor.next_row(&request).err().unwrap();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Cancelled);
    drop(cursor);
    let mut lookup = workspace.open_lookup().unwrap();
    let file_key = digest_parts(algorithm, &[b"file:", b"/docs/one.json"]);
    let error = lookup.find_row(&file_key, &request).err().unwrap();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Cancelled);
    drop(lookup);
    drop(workspace);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }

  #[test]
  fn corrupt_or_truncated_run_and_data_artifacts_fail_closed() {
    let algorithm = HashAlgorithm::Blake3_256;
    let directory = tempfile::tempdir().unwrap();
    let memory = memory();

    let workspace = one_record_workspace(directory.path(), algorithm, Arc::clone(&memory), CancellationToken::new());
    let run = run_path(workspace.directory.path(), workspace.order_run.run_id);
    let mut file = OpenOptions::new().write(true).open(&run).unwrap();
    file.seek(SeekFrom::Start(8)).unwrap();
    file.write_all(&0xffffu16.to_le_bytes()).unwrap();
    file.flush().unwrap();
    let error = workspace.open_cursor().err().unwrap();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Corrupt);
    assert_eq!(error.code(), "native_query_order_run_header");
    let error = workspace.open_lookup().err().unwrap();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Corrupt);
    assert_eq!(error.code(), "native_query_order_run_header");
    drop(workspace);

    let workspace = one_record_workspace(directory.path(), algorithm, Arc::clone(&memory), CancellationToken::new());
    let run = run_path(workspace.directory.path(), workspace.order_run.run_id);
    OpenOptions::new().write(true).open(run).unwrap().set_len(RUN_HEADER_LENGTH as u64).unwrap();
    let error = workspace.open_cursor().err().unwrap();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Corrupt);
    assert_eq!(error.code(), "native_query_order_run_length");
    let error = workspace.open_lookup().err().unwrap();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Corrupt);
    assert_eq!(error.code(), "native_query_order_run_length");
    drop(workspace);

    let workspace = one_record_workspace(directory.path(), algorithm, Arc::clone(&memory), CancellationToken::new());
    let mut data = OpenOptions::new().read(true).write(true).open(&workspace.data_path).unwrap();
    data.seek(SeekFrom::Start((DATA_HEADER_LENGTH + algorithm.hash_length()) as u64)).unwrap();
    let mut byte = [0; 1];
    data.read_exact(&mut byte).unwrap();
    data.seek(SeekFrom::Current(-1)).unwrap();
    byte[0] ^= 0xff;
    data.write_all(&byte).unwrap();
    data.flush().unwrap();
    let mut cursor = workspace.open_cursor().unwrap();
    let error = cursor.next_row(&CancellationToken::new()).err().unwrap();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Corrupt);
    assert_eq!(error.code(), "native_query_order_data_frame");
    drop(cursor);
    let mut lookup = workspace.open_lookup().unwrap();
    let file_key = digest_parts(algorithm, &[b"file:", b"/docs/one.json"]);
    let error = lookup.find_row(&file_key, &CancellationToken::new()).err().unwrap();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Corrupt);
    assert_eq!(error.code(), "native_query_order_data_frame");
    drop(lookup);
    drop(workspace);

    let workspace = one_record_workspace(directory.path(), algorithm, Arc::clone(&memory), CancellationToken::new());
    OpenOptions::new().write(true).open(&workspace.data_path).unwrap().set_len(1).unwrap();
    let error = workspace.open_cursor().err().unwrap();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Corrupt);
    assert_eq!(error.code(), "native_query_order_data_length");
    let error = workspace.open_lookup().err().unwrap();
    assert_eq!(error.class(), NativeQueryOrderingWorkspaceErrorClassV1::Corrupt);
    assert_eq!(error.code(), "native_query_order_data_length");
    drop(workspace);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}
