//! Disposable bounded path ordering. Membership and durable capture belong to callers.
use super::super::parser_registry_compiler::SemanticCompilationErrorV1;
use super::super::private_workspace::{
  PrivateWorkspaceErrorV1, create_private_regular_file, ensure_capacity, secure_platform_private_directory, validate_existing_directory,
  validate_private_regular_file,
};
use super::super::scope::validate_canonical_absolute_path;
use crate::engine::emergency_spill::open_regular_file_no_follow;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio_util::sync::CancellationToken;

type Result<T> = std::result::Result<T, SemanticCompilationErrorV1>;
const ROLE: &str = "<semantic-source-path-workspace>";
const MAGIC: &[u8; 8] = b"ASPRUN01";
const HEADER: u64 = 32;
const LEVELS: usize = 64;

#[derive(Clone, Copy, Debug)]
pub struct SemanticSourcePathWorkspaceBoundsV1 {
  pub maximum_input_paths: u64,
  pub maximum_path_bytes: usize,
  pub maximum_sort_bytes: u64,
  pub maximum_stored_bytes: u64,
  pub maximum_io_bytes: u64,
  pub maximum_paths_per_run: usize,
  pub merge_fan_in: usize,
  pub minimum_free_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SemanticSourcePathWorkspaceStatisticsV1 {
  pub input_paths: u64,
  pub initial_runs: u64,
  pub peak_retained_runs: usize,
  pub peak_open_inputs: usize,
  pub peak_stored_bytes: u64,
  pub io_bytes: u64,
}

struct Context {
  bounds: SemanticSourcePathWorkspaceBoundsV1,
  memory: MemoryCoordinator,
  cancellation: CancellationToken,
  io: AtomicU64,
}

impl Context {
  fn check(&self) -> Result<()> {
    if self.cancellation.is_cancelled() {
      return Err(SemanticCompilationErrorV1::Cancelled);
    }
    self.memory.check_admission(MemoryOwner::Task, AdmissionClass::Workload).map_err(resource)
  }

  fn reserve(&self, bytes: usize) -> Result<MemoryReservation> {
    self.check()?;
    self.memory.reserve(MemoryOwner::Task, bytes as u64, AdmissionClass::Workload).map_err(resource)
  }

  fn charge_io(&self, bytes: u64) -> Result<()> {
    self.check()?;
    self
      .io
      .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |prior| {
        prior.checked_add(bytes).filter(|total| *total <= self.bounds.maximum_io_bytes)
      })
      .map_err(|prior| {
        resource(format!("cumulative scratch I/O limit exceeded: used={prior}, requested={bytes}, limit={}", self.bounds.maximum_io_bytes))
      })?;
    Ok(())
  }

  fn path(&self, text: &str) -> Result<SemanticSourcePathV1> {
    validate_path(text, self.bounds.maximum_path_bytes)?;
    let reservation = self.reserve(text.len() + size_of::<SemanticSourcePathV1>())?;
    let mut path = String::new();
    path.try_reserve_exact(text.len()).map_err(resource)?;
    path.push_str(text);
    Ok(SemanticSourcePathV1 { path, _reservation: reservation })
  }
}

#[derive(Clone, Copy)]
struct Run {
  id: u64,
  count: u64,
  bytes: u64,
  level: usize,
}

pub struct SemanticSourcePathWorkspaceBuilderV1 {
  context: Context,
  directory: tempfile::TempDir,
  paths: Vec<SemanticSourcePathV1>,
  runs: Vec<Run>,
  window: usize,
  next_id: u64,
  stored: u64,
  statistics: SemanticSourcePathWorkspaceStatisticsV1,
  failed: bool,
  _reservation: MemoryReservation,
}

impl SemanticSourcePathWorkspaceBuilderV1 {
  pub fn new(
    parent: &Path,
    bounds: SemanticSourcePathWorkspaceBoundsV1,
    memory: &MemoryCoordinator,
    cancellation: &CancellationToken,
  ) -> Result<Self> {
    let (window, metadata) = validate_bounds(bounds)?;
    let context = Context { bounds, memory: memory.clone(), cancellation: cancellation.clone(), io: AtomicU64::new(0) };
    let reservation = context.reserve(metadata)?;
    validate_existing_directory(parent, "semantic source workspace parent").map_err(private_error)?;
    ensure_capacity(parent, HEADER, bounds.minimum_free_bytes).map_err(private_error)?;
    let directory = tempfile::Builder::new().prefix("aeordb-source-paths-").tempdir_in(parent).map_err(operational)?;
    secure_platform_private_directory(directory.path()).map_err(private_error)?;
    let mut paths = Vec::new();
    paths.try_reserve_exact(window).map_err(resource)?;
    let mut runs = Vec::new();
    runs.try_reserve_exact(LEVELS * bounds.merge_fan_in).map_err(resource)?;
    context.check()?;
    Ok(Self {
      context,
      directory,
      paths,
      runs,
      window,
      next_id: 0,
      stored: 0,
      statistics: SemanticSourcePathWorkspaceStatisticsV1::default(),
      failed: false,
      _reservation: reservation,
    })
  }

  pub fn append_path(&mut self, path: &str) -> Result<()> {
    if self.failed {
      return Err(operational("workspace builder is unusable after failure"));
    }
    let result = self.append_inner(path);
    match result {
      Ok(value) => Ok(value),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }

  fn append_inner(&mut self, path: &str) -> Result<()> {
    self.context.check()?;
    if self.statistics.input_paths >= self.context.bounds.maximum_input_paths {
      return Err(resource("input path count limit exceeded"));
    }
    let path = self.context.path(path)?;
    self.paths.push(path);
    self.statistics.input_paths += 1;
    if self.paths.len() == self.window {
      self.flush()?;
    }
    self.context.check()
  }

  fn flush(&mut self) -> Result<()> {
    if self.paths.is_empty() {
      return Ok(());
    }
    self.context.check()?;
    self.paths.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    self.paths.dedup_by(|left, right| left.path == right.path);
    let bytes = self
      .paths
      .iter()
      .try_fold(HEADER, |total, path| total.checked_add(8 + path.path.len() as u64).ok_or_else(|| resource("run length overflow")))?;
    let run = self.allocate_run(bytes, 0)?;
    let mut output = self.output(run)?;
    for path in &self.paths {
      write_path(&self.context, &mut output, &path.path)?;
    }
    let run = finish_run(&self.context, output, run, self.paths.len() as u64, bytes)?;
    self.paths.clear();
    self.statistics.initial_runs += 1;
    self.retain(run)
  }

  fn allocate_run(&mut self, bytes: u64, level: usize) -> Result<Run> {
    self.context.check()?;
    let stored = self
      .stored
      .checked_add(bytes)
      .filter(|total| *total <= self.context.bounds.maximum_stored_bytes)
      .ok_or_else(|| resource("simultaneous scratch storage limit exceeded"))?;
    ensure_capacity(self.directory.path(), bytes, self.context.bounds.minimum_free_bytes).map_err(private_error)?;
    let id = self.next_id;
    self.next_id = id.checked_add(1).ok_or_else(|| resource("run identity overflow"))?;
    self.stored = stored;
    self.statistics.peak_stored_bytes = self.statistics.peak_stored_bytes.max(stored);
    Ok(Run { id, count: 0, bytes, level })
  }

  fn output(&self, run: Run) -> Result<File> {
    let mut file = create_private_regular_file(&run_path(self.directory.path(), run.id), "source path run").map_err(private_error)?;
    self.context.charge_io(HEADER)?;
    file.write_all(&[0; HEADER as usize]).map_err(operational)?;
    Ok(file)
  }

  fn retain(&mut self, mut run: Run) -> Result<()> {
    loop {
      self.context.check()?;
      if run.level >= LEVELS {
        return Err(resource("run level limit exceeded"));
      }
      self.runs.push(run);
      self.statistics.peak_retained_runs = self.statistics.peak_retained_runs.max(self.runs.len());
      let count = self.runs.iter().filter(|other| other.level == run.level).count();
      if count < self.context.bounds.merge_fan_in {
        return Ok(());
      }
      let mut group = Vec::new();
      group.try_reserve_exact(count).map_err(resource)?;
      let mut index = 0;
      while index < self.runs.len() {
        if self.runs[index].level == run.level {
          group.push(self.runs.remove(index));
        } else {
          index += 1;
        }
      }
      run = self.merge(&group, run.level + 1)?;
    }
  }

  fn merge(&mut self, group: &[Run], level: usize) -> Result<Run> {
    let maximum_bytes =
      group.iter().try_fold(HEADER, |total, run| total.checked_add(run.bytes - HEADER).ok_or_else(|| resource("merge length overflow")))?;
    let run = self.allocate_run(maximum_bytes, level)?;
    let mut readers = Vec::new();
    readers.try_reserve_exact(group.len()).map_err(resource)?;
    let mut heads = Vec::new();
    heads.try_reserve_exact(group.len()).map_err(resource)?;
    for input in group {
      let mut reader = Reader::open(&self.context, self.directory.path(), *input)?;
      heads.push(reader.next(&self.context)?);
      readers.push(reader);
    }
    self.statistics.peak_open_inputs = self.statistics.peak_open_inputs.max(readers.len());
    let mut output = self.output(run)?;
    let mut previous: Option<SemanticSourcePathV1> = None;
    let mut count = 0u64;
    let mut bytes = HEADER;
    loop {
      self.context.check()?;
      let next = heads
        .iter()
        .enumerate()
        .filter_map(|(index, head)| head.as_ref().map(|path| (index, path)))
        .min_by(|left, right| left.1.path.cmp(&right.1.path))
        .map(|(index, _)| index);
      let Some(index) = next else {
        break;
      };
      let path = heads[index].take().ok_or_else(|| operational("missing merge head"))?;
      if previous.as_ref().is_none_or(|prior| prior.path != path.path) {
        write_path(&self.context, &mut output, &path.path)?;
        count += 1;
        bytes += 8 + path.path.len() as u64;
        previous = Some(path);
      }
      heads[index] = readers[index].next(&self.context)?;
    }
    let completed = finish_run(&self.context, output, run, count, bytes)?;
    drop(readers);
    self.stored -= maximum_bytes - bytes;
    for input in group {
      self.context.check()?;
      fs::remove_file(run_path(self.directory.path(), input.id)).map_err(operational)?;
      self.stored -= input.bytes;
    }
    Ok(completed)
  }

  pub fn finish(mut self) -> Result<SemanticSourcePathWorkspaceV1> {
    if self.failed {
      return Err(operational("workspace builder is unusable after failure"));
    }
    self.context.check()?;
    self.flush()?;
    while self.runs.len() > 1 {
      let count = self.runs.len().min(self.context.bounds.merge_fan_in);
      let mut group = Vec::new();
      group.try_reserve_exact(count).map_err(resource)?;
      for _ in 0..count {
        group.push(self.runs.pop().ok_or_else(|| operational("missing retained run"))?);
      }
      let run = self.merge(&group, 0)?;
      self.runs.push(run);
    }
    let run = self.runs.pop();
    if let Some(run) = run {
      let mut reader = Reader::open(&self.context, self.directory.path(), run)?;
      while reader.next(&self.context)?.is_some() {}
    }
    self.context.check()?;
    let reservation = self.context.reserve(size_of::<SemanticSourcePathWorkspaceV1>())?;
    Ok(SemanticSourcePathWorkspaceV1 {
      context: self.context,
      directory: self.directory,
      run,
      statistics: self.statistics,
      _reservation: reservation,
    })
  }
}

pub struct SemanticSourcePathWorkspaceV1 {
  context: Context,
  directory: tempfile::TempDir,
  run: Option<Run>,
  statistics: SemanticSourcePathWorkspaceStatisticsV1,
  _reservation: MemoryReservation,
}

impl SemanticSourcePathWorkspaceV1 {
  pub fn path_count(&self) -> u64 {
    self.run.map_or(0, |run| run.count)
  }

  pub fn statistics(&self) -> SemanticSourcePathWorkspaceStatisticsV1 {
    SemanticSourcePathWorkspaceStatisticsV1 { io_bytes: self.context.io.load(Ordering::Relaxed), ..self.statistics }
  }

  pub fn open_cursor(&self) -> Result<SemanticSourcePathCursorV1<'_>> {
    self.context.check()?;
    let reader = self.run.map(|run| Reader::open(&self.context, self.directory.path(), run)).transpose()?;
    self.context.check()?;
    Ok(SemanticSourcePathCursorV1 { workspace: self, reader, failed: false })
  }
}

pub struct SemanticSourcePathCursorV1<'a> {
  workspace: &'a SemanticSourcePathWorkspaceV1,
  reader: Option<Reader>,
  failed: bool,
}

impl SemanticSourcePathCursorV1<'_> {
  /// Rows are provisional until this cursor reports a successful end of stream.
  pub fn next_path(&mut self) -> Result<Option<SemanticSourcePathV1>> {
    if self.failed {
      return Err(operational("workspace cursor is unusable after failure"));
    }
    let result = (|| {
      self.workspace.context.check()?;
      let path = match &mut self.reader {
        Some(reader) => reader.next(&self.workspace.context)?,
        None => None,
      };
      self.workspace.context.check()?;
      Ok(path)
    })();
    match result {
      Ok(value) => Ok(value),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }
}

pub struct SemanticSourcePathV1 {
  path: String,
  _reservation: MemoryReservation,
}

impl SemanticSourcePathV1 {
  pub fn as_str(&self) -> &str {
    &self.path
  }
}

struct Reader {
  file: File,
  run: Run,
  remaining: u64,
  position: u64,
  previous: Vec<u8>,
  ended: bool,
  _reservation: MemoryReservation,
}

impl Reader {
  fn open(context: &Context, directory: &Path, run: Run) -> Result<Self> {
    let reservation = context.reserve(context.bounds.maximum_path_bytes + size_of::<Self>())?;
    let path = run_path(directory, run.id);
    let mut file = open_regular_file_no_follow(&path).map_err(operational)?;
    validate_private_regular_file(&path, &file, "source path run").map_err(private_error)?;
    if file.metadata().map_err(operational)?.len() != run.bytes {
      return Err(operational("scratch run length mismatch"));
    }
    let mut header = [0u8; HEADER as usize];
    context.charge_io(HEADER)?;
    file.read_exact(&mut header).map_err(operational)?;
    if header != encode_header(run.count, run.bytes) {
      return Err(operational("scratch run header mismatch"));
    }
    let mut previous = Vec::new();
    previous.try_reserve_exact(context.bounds.maximum_path_bytes).map_err(resource)?;
    Ok(Self { file, run, remaining: run.count, position: HEADER, previous, ended: false, _reservation: reservation })
  }

  fn next(&mut self, context: &Context) -> Result<Option<SemanticSourcePathV1>> {
    context.check()?;
    if self.ended {
      return Ok(None);
    }
    if self.remaining == 0 {
      if self.position != self.run.bytes {
        return Err(operational("scratch run count/length mismatch"));
      }
      context.charge_io(1)?;
      if self.file.read(&mut [0]).map_err(operational)? != 0 {
        return Err(operational("scratch run trailing bytes"));
      }
      context.check()?;
      self.ended = true;
      return Ok(None);
    }
    if self.position.checked_add(8).is_none_or(|end| end > self.run.bytes) {
      return Err(operational("scratch frame is truncated"));
    }
    let mut frame = [0u8; 8];
    context.charge_io(8)?;
    self.file.read_exact(&mut frame).map_err(operational)?;
    let length = u32::from_le_bytes(frame[..4].try_into().map_err(operational)?) as usize;
    let checksum = u32::from_le_bytes(frame[4..].try_into().map_err(operational)?);
    if length == 0 || length > context.bounds.maximum_path_bytes {
      return Err(operational("scratch path length is invalid"));
    }
    let end = self
      .position
      .checked_add(8 + length as u64)
      .filter(|end| *end <= self.run.bytes)
      .ok_or_else(|| operational("scratch path is truncated"))?;
    let reservation = context.reserve(length + size_of::<SemanticSourcePathV1>())?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length).map_err(resource)?;
    bytes.resize(length, 0);
    context.charge_io(length as u64)?;
    self.file.read_exact(&mut bytes).map_err(operational)?;
    if crc32fast::hash(&bytes) != checksum {
      return Err(operational("scratch path checksum mismatch"));
    }
    if !self.previous.is_empty() && self.previous >= bytes {
      return Err(operational("scratch path order is not strictly increasing"));
    }
    let path = String::from_utf8(bytes).map_err(operational)?;
    validate_path(&path, context.bounds.maximum_path_bytes).map_err(operational)?;
    self.previous.clear();
    self.previous.extend_from_slice(path.as_bytes());
    self.position = end;
    self.remaining -= 1;
    context.check()?;
    Ok(Some(SemanticSourcePathV1 { path, _reservation: reservation }))
  }
}

fn encode_header(count: u64, bytes: u64) -> [u8; HEADER as usize] {
  let mut header = [0; HEADER as usize];
  header[..8].copy_from_slice(MAGIC);
  header[8..16].copy_from_slice(&count.to_le_bytes());
  header[16..24].copy_from_slice(&bytes.to_le_bytes());
  let checksum = crc32fast::hash(&header[..24]);
  header[24..28].copy_from_slice(&checksum.to_le_bytes());
  header
}

fn write_path(context: &Context, file: &mut impl Write, path: &str) -> Result<()> {
  context.charge_io(8 + path.len() as u64)?;
  file.write_all(&(path.len() as u32).to_le_bytes()).map_err(operational)?;
  file.write_all(&crc32fast::hash(path.as_bytes()).to_le_bytes()).map_err(operational)?;
  file.write_all(path.as_bytes()).map_err(operational)
}

fn finish_run(context: &Context, mut file: File, run: Run, count: u64, bytes: u64) -> Result<Run> {
  context.charge_io(HEADER)?;
  if file.stream_position().map_err(operational)? != bytes {
    return Err(operational("scratch output length mismatch"));
  }
  file.seek(SeekFrom::Start(0)).map_err(operational)?;
  file.write_all(&encode_header(count, bytes)).map_err(operational)?;
  file.flush().map_err(operational)?;
  if file.metadata().map_err(operational)?.len() != bytes {
    return Err(operational("scratch output framing mismatch"));
  }
  context.check()?;
  Ok(Run { count, bytes, ..run })
}

fn validate_bounds(bounds: SemanticSourcePathWorkspaceBoundsV1) -> Result<(usize, usize)> {
  if bounds.maximum_input_paths == 0
    || bounds.maximum_input_paths > 1_000_000_000
    || !(1..=65_535).contains(&bounds.maximum_path_bytes)
    || !(1..=1_000_000).contains(&bounds.maximum_paths_per_run)
    || !(2..=64).contains(&bounds.merge_fan_in)
    || bounds.maximum_sort_bytes == 0
    || bounds.maximum_sort_bytes > 64 << 20
    || bounds.maximum_stored_bytes < HEADER
    || bounds.maximum_stored_bytes > 16 << 40
    || bounds.maximum_io_bytes == 0
    || bounds.maximum_io_bytes > 1 << 60
  {
    return Err(invalid("workspace bounds are outside supported limits"));
  }
  // The vector retains its capacity during merging. Each owned row separately
  // reserves its payload/guard, and each reader reserves its previous-path
  // window. Account for both phases without reserving those windows twice.
  let row_metadata = size_of::<SemanticSourcePathV1>();
  let fixed = 4096
    + LEVELS * bounds.merge_fan_in * size_of::<Run>() * 3
    + bounds.merge_fan_in * (size_of::<Reader>() + size_of::<Option<SemanticSourcePathV1>>());
  let merge = bounds.merge_fan_in * (bounds.maximum_path_bytes + size_of::<Reader>())
    + (bounds.merge_fan_in + 2) * (bounds.maximum_path_bytes + row_metadata);
  let available = bounds.maximum_sort_bytes.checked_sub(fixed as u64).ok_or_else(|| invalid("sort memory cannot hold run metadata"))?;
  let merge_available = available.checked_sub(merge as u64).ok_or_else(|| invalid("sort memory cannot hold merge state"))?;
  let window = bounds
    .maximum_paths_per_run
    .min(bounds.maximum_input_paths as usize)
    .min((available / (bounds.maximum_path_bytes + 2 * row_metadata) as u64) as usize)
    .min((merge_available / row_metadata as u64) as usize);
  if window == 0 {
    return Err(invalid("sort memory cannot hold one path"));
  }
  Ok((window, fixed + window * row_metadata))
}

fn validate_path(path: &str, maximum: usize) -> Result<()> {
  if path.len() > maximum {
    return Err(invalid("source path exceeds byte bound"));
  }
  validate_canonical_absolute_path(path).map_err(invalid)
}

fn run_path(directory: &Path, id: u64) -> PathBuf {
  directory.join(format!("run-{id:016x}.paths"))
}

fn private_error(error: PrivateWorkspaceErrorV1) -> SemanticCompilationErrorV1 {
  match error {
    PrivateWorkspaceErrorV1::Capacity(message) => resource(message),
    #[cfg(windows)]
    PrivateWorkspaceErrorV1::Allocation(message) => resource(message),
    other => operational(other),
  }
}
fn invalid(message: impl std::fmt::Display) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::InvalidSource { path: ROLE, message: message.to_string() }
}
fn resource(message: impl std::fmt::Display) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::Resource { path: ROLE, message: message.to_string() }
}
fn operational(message: impl std::fmt::Display) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::Operational { path: ROLE, message: message.to_string() }
}

#[cfg(test)]
#[path = "../../../spec/engine/semantic_source_path_workspace_spec.rs"]
mod spec;
