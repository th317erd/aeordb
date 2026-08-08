use crate::engine::compression::{compress, CompressionAlgorithm};
use crate::engine::batch_commit::{commit_buffered_files, BufferedFile, CommitResult};
use crate::engine::deletion_record::DeletionRecord;
use crate::engine::directory_entry::{ChildEntry, deserialize_child_entries, serialize_child_entries};
use crate::engine::directory_repair_workspace::DirectoryRepairWorkspace;
use crate::engine::entry_header::FLAG_SYSTEM;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::{FileRecord, CURRENT_FILE_RECORD_VERSION};
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::symlink_record::{SymlinkRecord, symlink_path_hash, symlink_content_hash};
use crate::engine::index_config_resolver::IndexConfigResolver;
use crate::engine::merge_patch::{apply_merge_patch, MergeDepth};
use crate::engine::memory_coordinator::{AdmissionClass, CriticalMemoryPurpose, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::operation_memory::OperationMemoryBudget;
use crate::engine::engine_event::{EntryEventData, EVENT_ENTRIES_CREATED, EVENT_ENTRIES_DELETED};
use crate::engine::path_utils::{file_name, normalize_path, parent_path};
use crate::engine::request_context::RequestContext;
use crate::engine::rss_sampler::PhaseSampler;
use crate::engine::storage_engine::{StorageEngine, WriteBatch};
use crate::engine::system_family_policy::GenericDataPathSelection;
use crate::engine::traversal::{TraversalIntegrity, VisitorCompletion};
use crate::engine::SystemFamilyPolicyResolver;

/// Default chunk size for splitting file data (256 KB).
pub const DEFAULT_CHUNK_SIZE: usize = 262_144;

/// One bounded, live window from a directory listing.
pub struct DirectoryListWindow {
  pub entries: Vec<ChildEntry>,
  pub has_more: bool,
  pub warnings: Vec<crate::engine::btree::BTreeWalkWarning>,
  pub integrity: TraversalIntegrity,
  pub visitor_completion: VisitorCompletion,
}

/// One directory traversal with explicit structural-integrity evidence.
pub struct DirectoryTraversalResult {
  pub entries: Vec<ChildEntry>,
  pub issues: Vec<crate::engine::btree::BTreeWalkWarning>,
  pub integrity: TraversalIntegrity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChildPathState {
  Live,
  Deleted,
  Missing,
}

/// Compute the domain-prefixed hash for a file path.
pub fn file_path_hash(path: &str, algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
  algo.compute_hash(format!("file:{}", path).as_bytes())
}

/// Compute the domain-prefixed hash for a directory path.
pub fn directory_path_hash(path: &str, algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
  algo.compute_hash(format!("dir:{}", path).as_bytes())
}

/// Compute a content-addressed hash for directory data.
/// Uses the "dirc:" domain prefix + the actual serialized content,
/// distinct from the path-based "dir:" prefix to avoid collisions.
pub fn directory_content_hash(data: &[u8], algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
  let mut input = Vec::with_capacity(5 + data.len());
  input.extend_from_slice(b"dirc:");
  input.extend_from_slice(data);
  algo.compute_hash(&input)
}

/// Compute a content-addressed hash for a serialized FileRecord.
/// Uses the "filec:" domain prefix, distinct from the path-based "file:" prefix.
pub fn file_content_hash(data: &[u8], algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
  let mut input = Vec::with_capacity(6 + data.len());
  input.extend_from_slice(b"filec:");
  input.extend_from_slice(data);
  algo.compute_hash(&input)
}

/// Identity hash for a file — based on content-defining fields only.
/// Excludes timestamps, metadata, and total_size.
/// Two identical files stored at different times produce the SAME identity hash.
pub fn file_identity_hash(path: &str, content_type: Option<&str>, chunk_hashes: &[Vec<u8>], algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
  let mut input = Vec::new();
  input.extend_from_slice(b"fileid:");
  input.extend_from_slice(path.as_bytes());
  input.push(0); // separator
  input.extend_from_slice(content_type.unwrap_or("").as_bytes());
  input.push(0); // separator
  for hash in chunk_hashes {
    input.extend_from_slice(hash);
  }
  algo.compute_hash(&input)
}

/// Identity hash for a symlink — based on path and target only.
/// Excludes timestamps.
pub fn symlink_identity_hash(path: &str, target: &str, algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
  let mut input = Vec::new();
  input.extend_from_slice(b"symlinkid:");
  input.extend_from_slice(path.as_bytes());
  input.push(0); // separator
  input.extend_from_slice(target.as_bytes());
  algo.compute_hash(&input)
}

fn immediate_child_under(parent: &str, path: &str) -> Option<(String, bool)> {
  let parent = normalize_path(parent);
  let path = normalize_path(path);
  if path == parent {
    return None;
  }

  let rest = if parent == "/" {
    path.strip_prefix('/')?
  } else {
    let prefix = format!("{}/", parent.trim_end_matches('/'));
    path.strip_prefix(&prefix)?
  };

  if rest.is_empty() {
    return None;
  }

  let mut segments = rest.split('/').filter(|segment| !segment.is_empty());
  let first = segments.next()?.to_string();
  let direct = segments.next().is_none();
  Some((first, direct))
}

enum RepairWorkspaceChild {
  Child { parent: String, child: ChildEntry },
  SkippedProtected,
  SkippedNonPath,
  SkippedDangling,
  SkippedMalformed,
}

/// Compute the domain-prefixed hash for a chunk.
pub fn chunk_content_hash(data: &[u8], algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
  let mut input = Vec::with_capacity(6 + data.len());
  input.extend_from_slice(b"chunk:");
  input.extend_from_slice(data);
  algo.compute_hash(&input)
}

/// Compute the hash for a system chunk (/.aeordb-system/ data).
/// Uses "system::" domain prefix — cryptographically separated from user "chunk:" domain.
pub fn system_chunk_hash(data: &[u8], algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
  let mut input = Vec::with_capacity(8 + data.len());
  input.extend_from_slice(b"system::");
  input.extend_from_slice(data);
  algo.compute_hash(&input)
}

/// Compute the identity hash for a system file.
/// Uses "sysfileid:" domain prefix.
pub fn system_file_identity_hash(
  path: &str,
  content_type: Option<&str>,
  chunk_hashes: &[Vec<u8>],
  algo: &HashAlgorithm,
) -> EngineResult<Vec<u8>> {
  let mut input = Vec::new();
  input.extend_from_slice(b"sysfileid:");
  input.extend_from_slice(path.as_bytes());
  input.push(0);
  input.extend_from_slice(content_type.unwrap_or("").as_bytes());
  input.push(0);
  for hash in chunk_hashes {
    input.extend_from_slice(hash);
  }
  algo.compute_hash(&input)
}

/// Compute the raw whole-file content hash from unprefixed file bytes.
///
/// This is intentionally distinct from [`chunk_content_hash`] and
/// [`file_content_hash`]: chunk hashes use the `chunk:` domain prefix and
/// file content-addressed keys hash the serialized `FileRecord`. `@hash`
/// exposes this raw file-byte hash so clients can ask "where does this
/// exact content exist?" independent of path and MIME type.
pub fn whole_file_content_hash(data: &[u8], algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
  algo.compute_hash(data)
}

/// Compute the raw whole-file content hash by streaming stored chunks in
/// order. Used by commit paths that only receive chunk hashes.
pub fn whole_file_content_hash_from_chunks(engine: &StorageEngine, chunk_hashes: &[Vec<u8>]) -> EngineResult<Vec<u8>> {
  let algo = engine.hash_algo();
  let mut hasher = algo.incremental_hasher()?;

  for hash in chunk_hashes {
    let Some(chunk) = engine.read_chunk(hash)? else {
      return Err(EngineError::NotFound(format!("Chunk not found while computing file content hash: {}", hex::encode(hash))));
    };
    hasher.update(&chunk);
  }

  Ok(hasher.finalize())
}

fn ensure_file_record_content_hash(engine: &StorageEngine, record: &mut FileRecord) -> EngineResult<()> {
  let hash_length = engine.hash_algo().hash_length();
  if record.content_hash.len() == hash_length {
    return Ok(());
  }
  if !record.content_hash.is_empty() {
    return Err(EngineError::InvalidInput(format!(
      "FileRecord content hash length {} does not match expected hash length {}",
      record.content_hash.len(),
      hash_length,
    )));
  }
  record.content_hash = whole_file_content_hash_from_chunks(engine, &record.chunk_hashes)?;
  Ok(())
}

fn ensure_file_record_content_hash_for_migration(
  engine: &StorageEngine,
  record: &mut FileRecord,
  memory: &mut OperationMemoryBudget,
) -> EngineResult<()> {
  let hash_length = engine.hash_algo().hash_length();
  if record.content_hash.len() == hash_length {
    return Ok(());
  }
  if !record.content_hash.is_empty() {
    return Err(EngineError::InvalidInput(format!(
      "FileRecord content hash length {} does not match expected hash length {}",
      record.content_hash.len(),
      hash_length,
    )));
  }

  let mut hasher = engine.hash_algo().incremental_hasher()?;
  for chunk_hash in &record.chunk_hashes {
    memory.record_work(1)?;
    let metadata = engine
      .get_chunk_stream_metadata(chunk_hash, false)?
      .ok_or_else(|| EngineError::NotFound(format!("Chunk not found while computing file content hash: {}", hex::encode(chunk_hash))))?;
    let decoded_bound = metadata.raw_value_length.unwrap_or(DEFAULT_CHUNK_SIZE as u64);
    let chunk_workspace = u64::from(metadata.total_length)
      .checked_add(decoded_bound)
      .ok_or_else(|| EngineError::ResourceExhausted("file record migration chunk estimate overflow".to_string()))?;
    memory.reserve(chunk_workspace, "file record migration chunk admission failed")?;
    let decoded_bound_usize = usize::try_from(decoded_bound)
      .map_err(|_| EngineError::ResourceExhausted(format!("file record migration chunk bound exceeds this platform: {decoded_bound}")))?;
    let read_result = engine.read_chunk_verified_bounded(chunk_hash, decoded_bound_usize);
    let consume_result = match read_result {
      Ok(Some(chunk)) => {
        if metadata.raw_value_length.is_some_and(|length| length != chunk.len() as u64) {
          Err(EngineError::CorruptEntry {
            offset: metadata.offset,
            reason: format!("migration chunk length {} does not match stored raw length {:?}", chunk.len(), metadata.raw_value_length),
          })
        } else {
          hasher.update(&chunk);
          Ok(())
        }
      }
      Ok(None) => Err(EngineError::NotFound(format!("Chunk not found while computing file content hash: {}", hex::encode(chunk_hash)))),
      Err(error) => Err(error),
    };
    let release_result = memory.release(chunk_workspace, "file record migration chunk release failed");
    match (consume_result, release_result) {
      (Ok(()), Ok(())) => {}
      (Err(error), Ok(())) => return Err(error),
      (_, Err(error)) => return Err(error),
    }
  }
  record.content_hash = hasher.finalize();
  Ok(())
}

fn store_file_record_entry(engine: &StorageEngine, key: &[u8], value: &[u8], flags: u8, entry_version: u8) -> EngineResult<()> {
  if flags != 0 {
    engine.store_entry_with_flags_and_version(EntryType::FileRecord, key, value, flags, entry_version)?;
  } else {
    engine.store_entry_with_version(EntryType::FileRecord, key, value, entry_version)?;
  }
  Ok(())
}

#[derive(Debug, Clone)]
pub(crate) struct FileRecordKeys {
  pub identity_key: Vec<u8>,
}

pub(crate) fn materialize_file_record_entries(
  engine: &StorageEngine,
  normalized_path: &str,
  record: &mut FileRecord,
  flags: u8,
) -> EngineResult<FileRecordKeys> {
  materialize_file_record_entries_at_version(engine, normalized_path, record, flags, CURRENT_FILE_RECORD_VERSION)
}

fn materialize_file_record_entries_at_version(
  engine: &StorageEngine,
  normalized_path: &str,
  record: &mut FileRecord,
  flags: u8,
  entry_version: u8,
) -> EngineResult<FileRecordKeys> {
  let algo = engine.hash_algo();
  let hash_length = algo.hash_length();
  ensure_file_record_content_hash(engine, record)?;

  let file_value = record.serialize_for_version(hash_length, entry_version)?;
  let content_key = file_content_hash(&file_value, &algo)?;
  let identity_key = file_identity_hash(normalized_path, record.content_type.as_deref(), &record.chunk_hashes, &algo)?;
  let file_key = file_path_hash(normalized_path, &algo)?;

  store_file_record_entry(engine, &content_key, &file_value, flags, entry_version)?;
  store_file_record_entry(engine, &identity_key, &file_value, flags, entry_version)?;
  store_file_record_entry(engine, &file_key, &file_value, flags, entry_version)?;

  Ok(FileRecordKeys { identity_key })
}

#[derive(Debug, Clone)]
pub(crate) struct FileRecordPublishInput {
  pub normalized_path: String,
  pub content_type: Option<String>,
  pub total_size: u64,
  pub chunk_hashes: Vec<Vec<u8>>,
  pub content_hash: Vec<u8>,
  pub flags: u8,
  pub created_at_override: Option<i64>,
  pub prefer_existing_created_at: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct FileRecordPublishResult {
  pub normalized_path: String,
  pub file_record: FileRecord,
  pub child_entry: ChildEntry,
  pub event_entry: EntryEventData,
  pub existing_total_size: Option<u64>,
}

pub(crate) fn publish_file_record_entries(engine: &StorageEngine, input: FileRecordPublishInput) -> EngineResult<FileRecordPublishResult> {
  publish_file_record_entries_at_version(engine, input, CURRENT_FILE_RECORD_VERSION)
}

fn publish_file_record_entries_at_version(
  engine: &StorageEngine,
  input: FileRecordPublishInput,
  entry_version: u8,
) -> EngineResult<FileRecordPublishResult> {
  let normalized = normalize_path(&input.normalized_path);
  let algo = engine.hash_algo();
  let hash_length = algo.hash_length();
  let file_key = file_path_hash(&normalized, &algo)?;
  let (existing_created_at, existing_total_size) = match engine.get_entry(&file_key)? {
    Some((header, _key, value)) => {
      let existing = FileRecord::deserialize(&value, hash_length, header.entry_version)?;
      (Some(existing.created_at), Some(existing.total_size))
    }
    None => (None, None),
  };

  let mut file_record = FileRecord::new(normalized.clone(), input.content_type.clone(), input.total_size, input.chunk_hashes);
  file_record.content_hash = input.content_hash;

  if input.prefer_existing_created_at {
    if let Some(original_created_at) = existing_created_at {
      file_record.created_at = original_created_at;
    } else if let Some(created_at_override) = input.created_at_override {
      file_record.created_at = created_at_override;
    }
  } else if let Some(created_at_override) = input.created_at_override {
    file_record.created_at = created_at_override;
  }

  let keys = materialize_file_record_entries_at_version(engine, &normalized, &mut file_record, input.flags, entry_version)?;
  let content_type = file_record.content_type.clone();
  let child_entry = ChildEntry {
    entry_type: EntryType::FileRecord.to_u8(),
    hash: keys.identity_key,
    total_size: file_record.total_size,
    created_at: file_record.created_at,
    updated_at: file_record.updated_at,
    name: file_name(&normalized).unwrap_or("").to_string(),
    content_type: content_type.clone(),
    virtual_time: chrono::Utc::now().timestamp_millis() as u64,
    node_id: 0,
  };
  let event_entry = EntryEventData {
    path: normalized.clone(),
    entry_type: "file".to_string(),
    content_type,
    size: file_record.total_size,
    hash: file_record.content_hash_hex(),
    created_at: file_record.created_at,
    updated_at: file_record.updated_at,
    previous_hash: None,
  };

  Ok(FileRecordPublishResult { normalized_path: normalized, file_record, child_entry, event_entry, existing_total_size })
}

fn file_record_header_needs_migration(engine: &StorageEngine, key: &[u8]) -> EngineResult<bool> {
  match engine.get_entry(key)? {
    Some((header, _key, _value)) => Ok(header.entry_version < CURRENT_FILE_RECORD_VERSION),
    None => Ok(true),
  }
}

/// Evaluate the v0 detached-system storage layout used by legacy entry flags.
///
/// This is a byte-layout compatibility rule, not generic authorization or
/// SystemFamily policy. New policy consumers must use
/// [`SystemFamilyPolicyResolver`].
fn v0_path_uses_detached_system_storage(path: &str) -> bool {
  let normalized = crate::engine::path_utils::normalize_path(path);
  normalized == "/.aeordb-system"
    || normalized.starts_with("/.aeordb-system/")
    || normalized == "/.aeordb-config"
    || normalized.starts_with("/.aeordb-config/")
}

/// Return the legacy entry-header flags required by the v0 storage layout.
pub fn v0_system_entry_flags(path: &str) -> u8 {
  if v0_path_uses_detached_system_storage(path) {
    FLAG_SYSTEM
  } else {
    0
  }
}

/// Whether v0 namespace propagation treats this path as a detached root.
pub(crate) fn v0_is_detached_system_path(path: &str) -> bool {
  v0_path_uses_detached_system_storage(path)
}

/// Compute the domain-prefixed hash for a deletion record.
pub(crate) fn deletion_record_hash(path: &str, timestamp: i64, algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
  algo.compute_hash(format!("del:{}:{}", path, timestamp).as_bytes())
}

/// Internal handle that lets [`EngineFileStream`] satisfy both lifetime
/// regimes: a borrowed `&StorageEngine` for fast in-process calls
/// (e.g. CLI, soak-worker), and an owned `Arc<StorageEngine>` for cases
/// that need `'static` (e.g. axum HTTP body streams).
enum EngineHandle<'a> {
  Borrowed(&'a StorageEngine),
  Owned(std::sync::Arc<StorageEngine>),
}

impl<'a> EngineHandle<'a> {
  fn engine(&self) -> &StorageEngine {
    match self {
      EngineHandle::Borrowed(e) => e,
      EngineHandle::Owned(arc) => arc,
    }
  }
}

/// Lazy chunk stream over a file's admitted chunk-hash inventory.
///
/// Construction accounts the already-owned inventory but performs no chunk
/// I/O. Each call to `next()` fetches exactly one bounded chunk and keeps its
/// reservation until the next legacy iterator call or the returned HTTP frame
/// is dropped. Peak payload memory is one chunk for embedded iteration.
///
/// History: an earlier version of this struct loaded every chunk eagerly
/// in its constructor and just iterated a pre-populated `Vec`. That made
/// reads of large files (audiobooks, video files) spike RSS to file size.
/// Caught during the 2026-05-15 soak diagnostics.
pub struct EngineFileStream<'a> {
  chunk_hashes: Vec<Vec<u8>>,
  engine: EngineHandle<'a>,
  current_index: usize,
  include_deleted: bool,
  expected_total_size: Option<u64>,
  _inventory_reservation: StreamingReadReservation,
  legacy_chunk_reservation: Option<StreamingReadReservation>,
}

/// Streaming reads preserve the P2b shadow-mode compatibility contract: when
/// startup configuration is unresolved, the legacy engine remains readable
/// without pretending that a configured memory policy exists. Once a policy
/// is active, `inner` owns the real coordinator reservation and all admission
/// failures remain enforced.
pub(crate) struct StreamingReadReservation {
  inner: Option<MemoryReservation>,
  bytes: u64,
}

impl std::fmt::Debug for StreamingReadReservation {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.debug_struct("StreamingReadReservation").field("bytes", &self.bytes).field("tracked", &self.inner.is_some()).finish()
  }
}

impl StreamingReadReservation {
  fn admitted(reservation: MemoryReservation) -> Self {
    let bytes = reservation.bytes();
    Self { inner: Some(reservation), bytes }
  }

  fn legacy_untracked(bytes: u64) -> Self {
    Self { inner: None, bytes }
  }

  pub(crate) fn bytes(&self) -> u64 {
    self.bytes
  }

  pub(crate) fn grow(&mut self, additional_bytes: u64) -> Result<(), MemoryCoordinatorError> {
    let next_bytes =
      self.bytes.checked_add(additional_bytes).ok_or(MemoryCoordinatorError::AccountingOverflow { owner: MemoryOwner::StreamingRead })?;
    if let Some(reservation) = self.inner.as_mut() {
      reservation.grow(additional_bytes)?;
    }
    self.bytes = next_bytes;
    Ok(())
  }

  pub(crate) fn shrink(&mut self, bytes: u64) -> Result<(), MemoryCoordinatorError> {
    if bytes > self.bytes {
      return Err(MemoryCoordinatorError::InvalidShrink {
        owner: MemoryOwner::StreamingRead,
        requested_bytes: bytes,
        reserved_bytes: self.bytes,
      });
    }
    if let Some(reservation) = self.inner.as_mut() {
      reservation.shrink(bytes)?;
    }
    self.bytes -= bytes;
    Ok(())
  }
}

/// One decoded file chunk whose memory remains admitted until the chunk is
/// dropped by its final consumer. HTTP converts this owner directly into
/// `Bytes`, so socket backpressure and disconnects retain/release the exact
/// reservation with the payload rather than with the iterator cursor.
pub(crate) struct ReservedReadChunk {
  data: Vec<u8>,
  _reservation: StreamingReadReservation,
}

impl ReservedReadChunk {
  pub(crate) fn len(&self) -> usize {
    self.data.len()
  }

  pub(crate) fn from_admitted(data: Vec<u8>, reservation: StreamingReadReservation) -> Self {
    Self { data, _reservation: reservation }
  }

  fn into_parts(self) -> (Vec<u8>, StreamingReadReservation) {
    (self.data, self._reservation)
  }
}

impl AsRef<[u8]> for ReservedReadChunk {
  fn as_ref(&self) -> &[u8] {
    &self.data
  }
}

impl<'a> EngineFileStream<'a> {
  /// Build a stream from an explicit list of chunk hashes (public entry point
  /// for hash-based retrieval where we already have the FileRecord).
  pub fn from_chunk_hashes(chunk_hashes: Vec<Vec<u8>>, engine: &'a StorageEngine) -> EngineResult<Self> {
    Self::new(chunk_hashes, engine, false)
  }

  /// Like `from_chunk_hashes` but reads chunks even if they are marked deleted.
  /// Used for streaming files from historical snapshots.
  pub fn from_chunk_hashes_including_deleted(chunk_hashes: Vec<Vec<u8>>, engine: &'a StorageEngine) -> EngineResult<Self> {
    Self::new(chunk_hashes, engine, true)
  }

  pub(crate) fn new(chunk_hashes: Vec<Vec<u8>>, engine: &'a StorageEngine, include_deleted: bool) -> EngineResult<Self> {
    Self::new_with_expected_total_size(chunk_hashes, engine, include_deleted, None)
  }

  fn new_with_expected_total_size(
    chunk_hashes: Vec<Vec<u8>>,
    engine: &'a StorageEngine,
    include_deleted: bool,
    expected_total_size: Option<u64>,
  ) -> EngineResult<Self> {
    let inventory_bytes = stream_hash_inventory_bytes(&chunk_hashes, chunk_hashes.capacity())?;
    let inventory_reservation = reserve_streaming_read(engine, inventory_bytes, "file stream inventory admission failed")?;
    Ok(EngineFileStream {
      chunk_hashes,
      engine: EngineHandle::Borrowed(engine),
      current_index: 0,
      include_deleted,
      expected_total_size,
      _inventory_reservation: inventory_reservation,
      legacy_chunk_reservation: None,
    })
  }

  /// Number of chunks the stream will yield.
  pub fn chunk_count(&self) -> usize {
    self.chunk_hashes.len()
  }

  /// Collect all chunks into a single `Vec<u8>`. Materializes the full file
  /// in memory by definition — use only when the caller actually needs the
  /// whole content (e.g. small config files). Prefer iterating chunks for
  /// arbitrary-size reads.
  pub fn collect_to_vec(self) -> EngineResult<Vec<u8>> {
    let expected_size = self
      .expected_total_size
      .map(|size| {
        usize::try_from(size)
          .map_err(|_| EngineError::ResourceExhausted(format!("buffered file size exceeds this platform's address space: {size}")))
      })
      .transpose()?;
    let mut result = Vec::new();
    if let Some(expected_size) = expected_size {
      result
        .try_reserve_exact(expected_size)
        .map_err(|error| EngineError::ResourceExhausted(format!("buffered file allocation failed: {error}")))?;
    }
    for item in self {
      let chunk = item?;
      let next_size =
        result.len().checked_add(chunk.len()).ok_or_else(|| EngineError::ResourceExhausted("buffered file length overflow".to_string()))?;
      if expected_size.is_some_and(|expected| next_size > expected) {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("decoded file body exceeds declared total size: decoded={next_size}, declared={}", expected_size.unwrap_or(0)),
        });
      }
      result.extend_from_slice(&chunk);
    }
    if expected_size.is_some_and(|expected| result.len() != expected) {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!(
          "decoded file body does not match declared total size: decoded={}, declared={}",
          result.len(),
          expected_size.unwrap_or(0)
        ),
      });
    }
    Ok(result)
  }

  fn fetch_chunk_reserved(&self, hash: &[u8]) -> EngineResult<ReservedReadChunk> {
    read_chunk_reserved(self.engine.engine(), hash, self.include_deleted)
  }

  pub(crate) fn next_reserved(&mut self) -> Option<EngineResult<ReservedReadChunk>> {
    drop(self.legacy_chunk_reservation.take());
    if self.current_index >= self.chunk_hashes.len() {
      return None;
    }
    let index = self.current_index;
    self.current_index += 1;
    Some(self.fetch_chunk_reserved(&self.chunk_hashes[index]))
  }
}

impl EngineFileStream<'static> {
  /// Build a `'static` stream from an owned `Arc<StorageEngine>`. Required
  /// when the stream must outlive the calling stack frame (e.g. axum HTTP
  /// response bodies that demand `'static + Send`).
  pub fn from_chunk_hashes_owned(chunk_hashes: Vec<Vec<u8>>, engine: std::sync::Arc<StorageEngine>) -> EngineResult<Self> {
    Self::new_owned(chunk_hashes, engine, false)
  }

  /// Owned-Arc variant of [`from_chunk_hashes_including_deleted`].
  pub fn from_chunk_hashes_including_deleted_owned(
    chunk_hashes: Vec<Vec<u8>>,
    engine: std::sync::Arc<StorageEngine>,
  ) -> EngineResult<Self> {
    Self::new_owned(chunk_hashes, engine, true)
  }

  pub(crate) fn new_owned(chunk_hashes: Vec<Vec<u8>>, engine: std::sync::Arc<StorageEngine>, include_deleted: bool) -> EngineResult<Self> {
    let inventory_bytes = stream_hash_inventory_bytes(&chunk_hashes, chunk_hashes.capacity())?;
    let inventory_reservation = reserve_streaming_read(&engine, inventory_bytes, "file stream inventory admission failed")?;
    Ok(EngineFileStream {
      chunk_hashes,
      engine: EngineHandle::Owned(engine),
      current_index: 0,
      include_deleted,
      expected_total_size: None,
      _inventory_reservation: inventory_reservation,
      legacy_chunk_reservation: None,
    })
  }
}

impl<'a> Iterator for EngineFileStream<'a> {
  type Item = EngineResult<Vec<u8>>;

  fn next(&mut self) -> Option<Self::Item> {
    self.next_reserved().map(|result| {
      result.map(|chunk| {
        let (data, reservation) = chunk.into_parts();
        self.legacy_chunk_reservation = Some(reservation);
        data
      })
    })
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    let remaining = self.chunk_hashes.len() - self.current_index;
    (remaining, Some(remaining))
  }
}

impl<'a> ExactSizeIterator for EngineFileStream<'a> {}

pub(crate) fn stream_hash_inventory_bytes(chunk_hashes: &[Vec<u8>], outer_capacity: usize) -> EngineResult<u64> {
  let outer = outer_capacity
    .checked_mul(std::mem::size_of::<Vec<u8>>())
    .ok_or_else(|| EngineError::ResourceExhausted("file stream hash inventory estimate overflow".to_string()))?;
  let inner = chunk_hashes.iter().try_fold(0usize, |total, hash| {
    total
      .checked_add(hash.capacity())
      .ok_or_else(|| EngineError::ResourceExhausted("file stream hash inventory estimate overflow".to_string()))
  })?;
  outer
    .checked_add(inner)
    .and_then(|bytes| bytes.checked_add(std::mem::size_of::<EngineFileStream<'static>>()))
    .and_then(|bytes| u64::try_from(bytes).ok())
    .ok_or_else(|| EngineError::ResourceExhausted("file stream hash inventory estimate overflow".to_string()))
}

pub(crate) fn reserve_streaming_read(engine: &StorageEngine, bytes: u64, context: &str) -> EngineResult<StreamingReadReservation> {
  let Some(coordinator) = engine.memory_coordinator_if_initialized() else {
    return Ok(StreamingReadReservation::legacy_untracked(bytes));
  };
  match coordinator.reserve(MemoryOwner::StreamingRead, bytes, AdmissionClass::Critical(CriticalMemoryPurpose::StreamingRead)) {
    Ok(reservation) => Ok(StreamingReadReservation::admitted(reservation)),
    Err(MemoryCoordinatorError::PolicyUnavailable) => Ok(StreamingReadReservation::legacy_untracked(bytes)),
    Err(error) => Err(streaming_memory_error(context, error)),
  }
}

pub(crate) fn streaming_memory_error(context: &str, error: MemoryCoordinatorError) -> EngineError {
  match error {
    MemoryCoordinatorError::HardLimitExceeded { .. }
    | MemoryCoordinatorError::SoftPressureDeferred { .. }
    | MemoryCoordinatorError::EmergencyReserveExceeded { .. } => EngineError::ResourceExhausted(format!("{context}: {error}")),
    other => EngineError::IoError(std::io::Error::other(format!("{context}: {other}"))),
  }
}

pub(crate) fn read_chunk_reserved(engine: &StorageEngine, hash: &[u8], include_deleted: bool) -> EngineResult<ReservedReadChunk> {
  let metadata = engine
    .get_chunk_stream_metadata(hash, include_deleted)?
    .ok_or_else(|| EngineError::NotFound(format!("Chunk not found: {}", hex::encode(hash))))?;
  let decoded_bound = metadata.raw_value_length.unwrap_or(DEFAULT_CHUNK_SIZE as u64);
  let admitted_bytes = (metadata.total_length as u64)
    .checked_add(decoded_bound)
    .and_then(|bytes| bytes.checked_add(std::mem::size_of::<ReservedReadChunk>() as u64))
    .ok_or_else(|| EngineError::ResourceExhausted("streaming chunk memory estimate overflow".to_string()))?;
  let mut reservation = reserve_streaming_read(engine, admitted_bytes, "decoded chunk admission failed")?;
  let decoded_bound: usize = decoded_bound
    .try_into()
    .map_err(|_| EngineError::ResourceExhausted(format!("decoded chunk bound exceeds this platform's address space: {decoded_bound}")))?;
  let read_result = if include_deleted {
    engine.read_chunk_verified_including_deleted_bounded(hash, decoded_bound)
  } else {
    engine.read_chunk_verified_bounded(hash, decoded_bound)
  };
  let entry = read_result.map_err(|error| match error {
    EngineError::InvalidInput(reason) => {
      EngineError::CorruptEntry { offset: metadata.offset, reason: format!("Chunk exceeds its streaming contract: {reason}") }
    }
    other => other,
  })?;
  let data = entry.ok_or_else(|| EngineError::NotFound(format!("Chunk not found: {}", hex::encode(hash))))?;
  if metadata.raw_value_length.is_some_and(|decoded_bytes| data.len() as u64 != decoded_bytes) {
    return Err(EngineError::CorruptEntry {
      offset: metadata.offset,
      reason: format!(
        "Chunk length does not match admitted metadata: metadata {:?}, bound {}, decoded {}",
        metadata.raw_value_length,
        decoded_bound,
        data.len()
      ),
    });
  }
  let retained_bytes = u64::try_from(data.capacity())
    .ok()
    .and_then(|bytes| bytes.checked_add(std::mem::size_of::<ReservedReadChunk>() as u64))
    .ok_or_else(|| EngineError::ResourceExhausted("streaming chunk retained memory estimate overflow".to_string()))?;
  if retained_bytes > reservation.bytes() {
    return Err(EngineError::CorruptEntry {
      offset: metadata.offset,
      reason: format!("Decoded chunk allocation {} exceeds admitted {} bytes", retained_bytes, reservation.bytes()),
    });
  }
  if retained_bytes < reservation.bytes() {
    reservation
      .shrink(reservation.bytes() - retained_bytes)
      .map_err(|error| streaming_memory_error("decoded chunk accounting failed", error))?;
  }
  Ok(ReservedReadChunk::from_admitted(data, reservation))
}

/// Directory operations built on top of the StorageEngine.
///
/// Provides file storage, retrieval, deletion, directory listing,
/// and path-based navigation with automatic parent directory management.
pub struct DirectoryOps<'a> {
  engine: &'a StorageEngine,
}

#[derive(Debug, Clone)]
pub struct JsonMergeFileResult {
  pub file_record: FileRecord,
  pub created: bool,
}

#[derive(Debug, Clone)]
pub struct JsonMergeFilePatch {
  pub path: String,
  pub patch: serde_json::Value,
  pub depth: MergeDepth,
}

#[derive(Debug, Clone)]
pub struct JsonMergedFile {
  pub path: String,
  pub size: u64,
  pub created: bool,
}

#[derive(Debug, Clone)]
pub struct JsonMergeBatchResult {
  pub merged: usize,
  pub files: Vec<JsonMergedFile>,
}

impl<'a> DirectoryOps<'a> {
  /// Create a new `DirectoryOps` handle wrapping the given storage engine.
  pub fn new(engine: &'a StorageEngine) -> Self {
    DirectoryOps { engine }
  }

  /// Store a file at the given path from a fully-buffered byte slice.
  ///
  /// **WARNING — buffered, not streaming.** The entire file content must fit
  /// in `data`. This is convenient for small payloads (JSON configs, indexes,
  /// short logs) but a footgun for large files. For arbitrary-size files
  /// (e.g. user uploads), use [`store_file_from_reader`] which streams chunks
  /// from any `Read` source without buffering the whole file.
  pub fn store_file_buffered(&self, ctx: &RequestContext, path: &str, data: &[u8], content_type: Option<&str>) -> EngineResult<FileRecord> {
    self.store_file_internal(ctx, path, data, content_type, CompressionAlgorithm::None)
  }

  pub(crate) fn store_transition_control_v0(&self, path: &str, data: &[u8]) -> EngineResult<FileRecord> {
    let normalized = normalize_path(path);
    if !normalized.starts_with("/.aeordb-system/controls/v1/") {
      return Err(EngineError::InvalidInput("transition control writer requires a canonical ControlStore path".to_string()));
    }
    self.store_file_internal_inner(
      &RequestContext::system(),
      &normalized,
      data,
      Some("application/octet-stream"),
      CompressionAlgorithm::None,
      0,
      false,
    )
  }

  /// Store multiple small files from fully-buffered byte vectors.
  ///
  /// **WARNING — buffered, not streaming.** Every file body is already in
  /// memory. Use this for small trusted SDK writes (JSON buckets, configs,
  /// short text files), not arbitrary-size user uploads.
  pub fn store_files_buffered_batch(&self, ctx: &RequestContext, files: Vec<BufferedFile>) -> EngineResult<CommitResult> {
    commit_buffered_files(self.engine, ctx, files)
  }

  /// Apply an RFC 7396 JSON merge patch to one stored JSON file.
  ///
  /// Missing files start as `{}` and are created as `application/json`.
  /// Existing files must contain valid JSON; invalid stored JSON fails fast
  /// before any write occurs.
  pub fn merge_json_file(
    &self,
    ctx: &RequestContext,
    path: &str,
    patch: serde_json::Value,
    depth: MergeDepth,
  ) -> EngineResult<JsonMergeFileResult> {
    let (serialized, existed) = self.prepare_json_merge(path, patch, depth)?;
    let file_record = self.store_file_buffered(ctx, path, &serialized, Some("application/json"))?;
    Ok(JsonMergeFileResult { file_record, created: !existed })
  }

  /// Apply JSON merge patches to multiple small JSON files in one write batch.
  ///
  /// All target documents are read, parsed, and merged before the batch write
  /// starts, so invalid JSON in any existing file prevents every write in the
  /// batch.
  pub fn merge_json_files_batch(&self, ctx: &RequestContext, patches: Vec<JsonMergeFilePatch>) -> EngineResult<JsonMergeBatchResult> {
    if patches.is_empty() {
      return Err(EngineError::InvalidInput("No JSON merge patches provided".to_string()));
    }

    let mut seen_paths = std::collections::HashSet::with_capacity(patches.len());
    for patch in &patches {
      let normalized = normalize_path(&patch.path);
      if normalized == "/" {
        return Err(EngineError::InvalidInput("Cannot store at root path".to_string()));
      }
      if !seen_paths.insert(normalized.clone()) {
        return Err(EngineError::InvalidInput(format!("Duplicate batch path: {}", normalized)));
      }
    }

    let mut files = Vec::with_capacity(patches.len());
    let mut merged_files = Vec::with_capacity(patches.len());

    for patch in patches {
      let normalized = normalize_path(&patch.path);
      let (serialized, existed) = self.prepare_json_merge(&normalized, patch.patch, patch.depth)?;
      let size = serialized.len() as u64;
      files.push(BufferedFile { path: normalized.clone(), data: serialized, content_type: Some("application/json".to_string()) });
      merged_files.push(JsonMergedFile { path: normalized, size, created: !existed });
    }

    let result = self.store_files_buffered_batch(ctx, files)?;
    Ok(JsonMergeBatchResult { merged: result.committed, files: merged_files })
  }

  fn prepare_json_merge(&self, path: &str, patch: serde_json::Value, depth: MergeDepth) -> EngineResult<(Vec<u8>, bool)> {
    let normalized = normalize_path(path);
    if normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot store at root path".to_string()));
    }

    let (mut target, existed) = match self.read_file_buffered(&normalized) {
      Ok(bytes) => {
        if bytes.is_empty() {
          (serde_json::Value::Object(serde_json::Map::new()), true)
        } else {
          let parsed: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|error| EngineError::InvalidInput(format!("stored file at {} is not valid JSON: {}", normalized, error)))?;
          (parsed, true)
        }
      }
      Err(EngineError::NotFound(_)) => (serde_json::Value::Object(serde_json::Map::new()), false),
      Err(error) => return Err(error),
    };

    apply_merge_patch(&mut target, patch, depth);
    serde_json::to_vec(&target)
      .map(|serialized| (serialized, existed))
      .map_err(|error| EngineError::InvalidInput(format!("merged document failed to serialize: {}", error)))
  }

  /// Store a file at the given path by streaming chunks from a `Read` source.
  ///
  /// Memory usage is bounded to a single chunk buffer (`DEFAULT_CHUNK_SIZE`,
  /// 256 KB) regardless of file size. Use this for arbitrary-size uploads,
  /// bulk imports, and anything that might not fit in memory.
  pub fn store_file_from_reader<R: std::io::Read>(
    &self,
    ctx: &RequestContext,
    path: &str,
    mut reader: R,
    content_type: Option<&str>,
  ) -> EngineResult<FileRecord> {
    let _mem = PhaseSampler::start("store_file_from_reader", std::time::Duration::from_millis(50));
    let chunk_size = DEFAULT_CHUNK_SIZE;
    let mut chunk_hashes: Vec<Vec<u8>> = Vec::new();
    let mut first_bytes: Vec<u8> = Vec::with_capacity(8192);
    let mut total_size: u64 = 0;
    let mut buffer = vec![0u8; chunk_size];
    let mut filled = 0usize;
    let mut content_hasher = self.engine.hash_algo().incremental_hasher()?;

    loop {
      // Top up the buffer to a full chunk before emitting, so we end up with
      // uniformly-sized chunks (last one may be smaller).
      while filled < chunk_size {
        let n = reader.read(&mut buffer[filled..]).map_err(EngineError::IoError)?;
        if n == 0 {
          break;
        }
        filled += n;
      }
      if filled == 0 {
        break;
      }

      // Capture the first up-to-8 KB for content-type detection.
      if first_bytes.len() < 8192 {
        let need = (8192 - first_bytes.len()).min(filled);
        first_bytes.extend_from_slice(&buffer[..need]);
      }

      let hash = self.store_chunk(&buffer[..filled])?;
      content_hasher.update(&buffer[..filled]);
      chunk_hashes.push(hash);
      total_size += filled as u64;

      if filled < chunk_size {
        // Reader is exhausted (short read).
        break;
      }
      filled = 0;
    }

    let content_hash = content_hasher.finalize();
    let file_record =
      self.finalize_file_with_content_hash(ctx, path, chunk_hashes, total_size, content_type, &first_bytes, content_hash)?;
    self.index_metadata_after_streaming_store(ctx, path);
    Ok(file_record)
  }

  /// Store a single data chunk and return its hash. Deduplicates automatically.
  /// Used by streaming upload to store chunks as they arrive without buffering.
  pub fn store_chunk(&self, data: &[u8]) -> EngineResult<Vec<u8>> {
    let _mem = PhaseSampler::start("store_chunk", std::time::Duration::from_millis(50));
    let algo = self.engine.hash_algo();
    let chunk_key = chunk_content_hash(data, &algo)?;
    if !self.engine.has_entry(&chunk_key)? {
      self.engine.store_entry(EntryType::Chunk, &chunk_key, data)?;
      self.engine.counters().record_chunk_stored(data.len() as u64);
    } else {
      self.engine.counters().record_chunk_deduped();
    }
    Ok(chunk_key)
  }

  /// Finalize a file from pre-stored chunk hashes.
  /// Chunks must already be stored via `store_chunk()`. This method creates
  /// the FileRecord, updates directory indexes, and emits events.
  /// `first_bytes` is the first ≤8KB for content-type detection.
  pub fn finalize_file(
    &self,
    ctx: &RequestContext,
    path: &str,
    chunk_hashes: Vec<Vec<u8>>,
    total_size: u64,
    content_type: Option<&str>,
    first_bytes: &[u8],
  ) -> EngineResult<FileRecord> {
    let content_hash = whole_file_content_hash_from_chunks(self.engine, &chunk_hashes)?;
    let file_record = self.finalize_file_with_content_hash(ctx, path, chunk_hashes, total_size, content_type, first_bytes, content_hash)?;
    self.index_metadata_after_streaming_store(ctx, path);
    Ok(file_record)
  }

  fn index_metadata_after_streaming_store(&self, ctx: &RequestContext, path: &str) {
    let pipeline = crate::engine::indexing_pipeline::IndexingPipeline::new(self.engine);
    if let Err(error) = pipeline.run_metadata_only(ctx, path) {
      tracing::warn!("Metadata indexing failed for streamed file '{}': {}", path, error);
    }
  }

  fn finalize_file_with_content_hash(
    &self,
    ctx: &RequestContext,
    path: &str,
    chunk_hashes: Vec<Vec<u8>>,
    total_size: u64,
    content_type: Option<&str>,
    first_bytes: &[u8],
    content_hash: Vec<u8>,
  ) -> EngineResult<FileRecord> {
    let _mem = PhaseSampler::start("finalize_file", std::time::Duration::from_millis(50));
    let timer_start = std::time::Instant::now();
    let normalized = normalize_path(path);

    if normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot store at root path".to_string()));
    }

    let namespace = self.engine.namespace_write_guard()?;
    let txn = crate::engine::storage_engine::TransactionGuard::new(self.engine)?;

    let sys_flags = v0_system_entry_flags(&normalized);
    let detected_content_type = crate::engine::content_type::detect_content_type(first_bytes, content_type);

    let published = publish_file_record_entries(
      self.engine,
      FileRecordPublishInput {
        normalized_path: normalized,
        content_type: Some(detected_content_type),
        total_size,
        chunk_hashes,
        content_hash,
        flags: sys_flags,
        created_at_override: None,
        prefer_existing_created_at: true,
      },
    )?;

    self.update_parent_directories(&published.normalized_path, published.child_entry.clone())?;
    txn.commit_after(namespace)?;
    self.engine.counters().record_file_write(published.existing_total_size, total_size, total_size);
    ctx.emit(EVENT_ENTRIES_CREATED, serde_json::json!({"entries": [published.event_entry.clone()]}));

    let elapsed = timer_start.elapsed().as_secs_f64();
    metrics::histogram!(crate::metrics::definitions::FILE_STORE_DURATION).record(elapsed);

    Ok(published.file_record)
  }

  /// Store a file with compression at the given path, splitting data into chunks.
  /// Creates intermediate directories as needed and updates HEAD.
  /// Chunks are compressed individually using the specified algorithm.
  pub fn store_file_compressed(
    &self,
    ctx: &RequestContext,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
    compression_algo: CompressionAlgorithm,
  ) -> EngineResult<FileRecord> {
    self.store_file_internal(ctx, path, data, content_type, compression_algo)
  }

  /// Internal file storage with optional compression.
  ///
  /// **Atomicity (M15)**: This method stores chunks, a FileRecord, and
  /// updated directory entries as separate append-writer operations. If the
  /// process crashes mid-way, some chunks or the FileRecord may be written
  /// to disk without the directory tree pointing to them. These orphaned
  /// entries are harmless — they consume space but are unreachable — and
  /// will be reclaimed by the next GC sweep. The hot-file mechanism
  /// ensures the KV index is recovered on restart, and since the directory
  /// tree is only updated atomically at the end (single entry write),
  /// readers will never see a partially-stored file.
  fn store_file_internal(
    &self,
    ctx: &RequestContext,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
    compression_algo: CompressionAlgorithm,
  ) -> EngineResult<FileRecord> {
    let timer_start = std::time::Instant::now();
    let result = self.store_file_internal_inner(ctx, path, data, content_type, compression_algo, CURRENT_FILE_RECORD_VERSION, true);
    let elapsed = timer_start.elapsed().as_secs_f64();
    metrics::histogram!(crate::metrics::definitions::FILE_STORE_DURATION).record(elapsed);
    result
  }

  /// Inner implementation of store_file_internal, separated for timing.
  fn store_file_internal_inner(
    &self,
    ctx: &RequestContext,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
    compression_algo: CompressionAlgorithm,
    file_record_version: u8,
    emit_event: bool,
  ) -> EngineResult<FileRecord> {
    let normalized = normalize_path(path);

    // M15: Reject storing at root path — it would create a ghost entry.
    if normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot store at root path".to_string()));
    }

    let algo = self.engine.hash_algo();
    let sys_flags = v0_system_entry_flags(&normalized);

    // Detect content type from magic bytes when not explicitly provided
    let detected_content_type = crate::engine::content_type::detect_content_type(data, content_type);

    // Split data into chunks and store each one
    let mut chunk_hashes = Vec::new();
    let chunk_size = DEFAULT_CHUNK_SIZE;

    if data.is_empty() {
      // Even empty files get zero chunks — that's fine
    } else {
      let mut offset = 0;
      while offset < data.len() {
        let end = (offset + chunk_size).min(data.len());
        let chunk_data = &data[offset..end];

        // Hash is ALWAYS on uncompressed data (for dedup)
        let chunk_key = chunk_content_hash(chunk_data, &algo)?;

        // Dedup: only store if not already present
        if !self.engine.has_entry(&chunk_key)? {
          let stored_chunk_bytes;
          if compression_algo != CompressionAlgorithm::None {
            let compressed_data = compress(chunk_data, compression_algo)?;
            stored_chunk_bytes = compressed_data.len() as u64;
            if sys_flags != 0 {
              self.engine.store_entry_compressed_with_flags(EntryType::Chunk, &chunk_key, &compressed_data, sys_flags, compression_algo)?;
            } else {
              self.engine.store_entry_compressed(EntryType::Chunk, &chunk_key, &compressed_data, compression_algo)?;
            }
          } else if sys_flags != 0 {
            stored_chunk_bytes = chunk_data.len() as u64;
            self.engine.store_entry_with_flags(EntryType::Chunk, &chunk_key, chunk_data, sys_flags)?;
          } else {
            stored_chunk_bytes = chunk_data.len() as u64;
            self.engine.store_entry(EntryType::Chunk, &chunk_key, chunk_data)?;
          }
          self.engine.counters().record_chunk_stored(stored_chunk_bytes);
        } else {
          self.engine.counters().record_chunk_deduped();
        }

        chunk_hashes.push(chunk_key);
        offset = end;
      }
    }

    let namespace = self.engine.namespace_write_guard()?;
    let txn = crate::engine::storage_engine::TransactionGuard::new(self.engine)?;

    let total_size = data.len() as u64;
    let published = publish_file_record_entries_at_version(
      self.engine,
      FileRecordPublishInput {
        normalized_path: normalized,
        content_type: Some(detected_content_type),
        total_size,
        chunk_hashes,
        content_hash: whole_file_content_hash(data, &algo)?,
        flags: sys_flags,
        created_at_override: None,
        prefer_existing_created_at: true,
      },
      file_record_version,
    )?;

    self.update_parent_directories(&published.normalized_path, published.child_entry.clone())?;
    txn.commit_after(namespace)?;
    self.engine.counters().record_file_write(published.existing_total_size, total_size, total_size);
    if emit_event {
      ctx.emit(EVENT_ENTRIES_CREATED, serde_json::json!({"entries": [published.event_entry.clone()]}));
    }

    Ok(published.file_record)
  }

  /// Restore a file from an existing FileRecord without re-reading chunk data.
  /// The chunks must already exist in the database (e.g., from a historical snapshot).
  /// This avoids loading the entire file into memory for large file restores.
  pub fn restore_file_from_record(&self, ctx: &RequestContext, path: &str, source_record: &FileRecord) -> EngineResult<()> {
    let normalized = normalize_path(path);
    let namespace = self.engine.namespace_write_guard()?;
    let txn = crate::engine::storage_engine::TransactionGuard::new(self.engine)?;

    let content_type = source_record.content_type.as_deref().unwrap_or("application/octet-stream");
    let published = publish_file_record_entries(
      self.engine,
      FileRecordPublishInput {
        normalized_path: normalized,
        content_type: Some(content_type.to_string()),
        total_size: source_record.total_size,
        chunk_hashes: source_record.chunk_hashes.clone(),
        content_hash: source_record.content_hash.clone(),
        flags: 0,
        created_at_override: Some(source_record.created_at),
        prefer_existing_created_at: true,
      },
    );
    let published = published?;

    self.update_parent_directories(&published.normalized_path, published.child_entry.clone())?;
    txn.commit_after(namespace)?;
    self.engine.counters().record_file_write(published.existing_total_size, source_record.total_size, 0);
    ctx.emit(EVENT_ENTRIES_CREATED, serde_json::json!({"entries": [published.event_entry.clone()]}));

    Ok(())
  }

  /// Read a file as a streaming iterator of chunk data.
  pub fn read_file_streaming(&self, path: &str) -> EngineResult<EngineFileStream<'_>> {
    let timer_start = std::time::Instant::now();
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    let file_key = file_path_hash(&normalized, &algo)?;
    // User-facing read — verify hash integrity
    let entry = self.engine.get_entry_verified(&file_key)?.ok_or_else(|| EngineError::NotFound(normalized.clone()))?;

    let (header, _key, value) = entry;
    let file_record = FileRecord::deserialize(&value, hash_length, header.entry_version)?;

    self.engine.counters().record_read(file_record.total_size);

    let elapsed = timer_start.elapsed().as_secs_f64();
    metrics::histogram!(crate::metrics::definitions::FILE_READ_DURATION).record(elapsed);

    EngineFileStream::new_with_expected_total_size(file_record.chunk_hashes, self.engine, false, Some(file_record.total_size))
  }

  /// Read a file's full content into memory.
  ///
  /// **WARNING — buffered, not streaming.** The full decompressed file is
  /// materialized in a single `Vec<u8>`. Convenient for small payloads
  /// (JSON configs, sub-MB content) but a footgun for large files. For
  /// arbitrary-size reads, use [`read_file_streaming`] and iterate its
  /// `EngineFileStream` so memory stays bounded to one chunk.
  pub fn read_file_buffered(&self, path: &str) -> EngineResult<Vec<u8>> {
    let result = self.read_file_streaming(path)?.collect_to_vec()?;
    Ok(result)
  }

  /// Delete a file, storing a DeletionRecord and updating parent directories.
  /// Takes an auto-snapshot before delete (throttled to once per minute).
  pub fn delete_file(&self, ctx: &RequestContext, path: &str) -> EngineResult<()> {
    let normalized = normalize_path(path);

    let namespace = self.engine.namespace_write_guard()?;
    let txn = crate::engine::storage_engine::TransactionGuard::new(self.engine)?;
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();
    let sys_flags = v0_system_entry_flags(&normalized);

    // Verify the file exists FIRST — before auto-snapshot or any side-effect.
    // A delete of a nonexistent file must produce zero observable side-effects:
    // no auto-snapshot, no event, no counter changes.
    let file_key = file_path_hash(&normalized, &algo)?;
    let file_record_opt = match self.engine.get_entry(&file_key)? {
      Some((header, _key, value)) => Some(FileRecord::deserialize(&value, hash_length, header.entry_version)?),
      None => {
        return Err(EngineError::NotFound(normalized));
      }
    };

    // File confirmed to exist. Now take an auto-snapshot before mutating
    // (at most once per minute).
    if v0_system_entry_flags(&normalized) == 0 {
      self.auto_snapshot_before_delete(ctx);
    }

    // Store a DeletionRecord
    let deletion = DeletionRecord::new(normalized.clone(), None);
    let deletion_key = deletion_record_hash(&normalized, deletion.deleted_at, &algo)?;
    let deletion_value = deletion.serialize();
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::DeletionRecord, &deletion_key, &deletion_value, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::DeletionRecord, &deletion_key, &deletion_value)?;
    }

    // Mark the FileRecord as deleted in the KV store
    self.engine.mark_entry_deleted(&file_key)?;

    // Remove child from parent directory
    self.remove_from_parent_directory(&normalized)?;

    let deleted_entry = file_record_opt.map(|record| EntryEventData {
      path: normalized,
      entry_type: "file".to_string(),
      content_type: record.content_type.clone(),
      size: record.total_size,
      hash: record.content_hash_hex(),
      created_at: record.created_at,
      updated_at: record.updated_at,
      previous_hash: None,
    });

    txn.commit_after(namespace)?;

    // Publish process-local side effects only after the hard commit succeeds.
    if let Some(entry_data) = deleted_entry {
      self.engine.counters().record_file_delete(entry_data.size);
      ctx.emit(EVENT_ENTRIES_DELETED, serde_json::json!({"entries": [entry_data]}));
    }

    Ok(())
  }

  /// Delete an empty directory. Returns an error if the directory has children.
  ///
  /// **TOCTOU note**: The emptiness check is not fully atomic with the deletion.
  /// A TransactionGuard documents the atomicity boundary. After mark_entry_deleted
  /// and remove_from_parent_directory, we re-check the raw directory data for
  /// children. If a concurrent write sneaked in between the initial check and
  /// the deletion, those children are now orphaned -- but we log a warning so
  /// the condition is observable (and GC will eventually reclaim them).
  pub fn delete_directory(&self, ctx: &RequestContext, path: &str) -> EngineResult<()> {
    let normalized = normalize_path(path);
    let namespace = self.engine.namespace_write_guard()?;
    let txn = crate::engine::storage_engine::TransactionGuard::new(self.engine)?;
    let algo = self.engine.hash_algo();
    let sys_flags = v0_system_entry_flags(&normalized);

    if normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot delete root directory".to_string()));
    }

    // Verify the directory exists and is empty
    // Directory deletion retains the v3 compatibility behavior that treats a
    // stale child with no live path key as absent. P2d's mutation coordinator
    // will replace this TOCTOU cleanup contract with operation-ledger evidence.
    let children = self.list_directory(&normalized)?;
    if !children.is_empty() {
      return Err(EngineError::InvalidInput(format!("Directory '{}' is not empty ({} children)", normalized, children.len())));
    }

    let deletion = DeletionRecord::new(normalized.clone(), None);
    let deletion_key = deletion_record_hash(&normalized, deletion.deleted_at, &algo)?;
    let deletion_value = deletion.serialize();
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::DeletionRecord, &deletion_key, &deletion_value, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::DeletionRecord, &deletion_key, &deletion_value)?;
    }

    // Mark the directory index entry as deleted
    let dir_key = directory_path_hash(&normalized, &algo)?;
    self.engine.mark_entry_deleted(&dir_key)?;

    // Remove from parent listing
    self.remove_from_parent_directory(&normalized)?;

    // TOCTOU re-check: verify no children were added between our emptiness
    // check and the deletion. The directory entry is already marked deleted,
    // so use get_entry_including_deleted to read the raw data at that offset.
    if let Ok(Some((_header, _key, raw_value))) = self.engine.get_entry_including_deleted(&dir_key) {
      // Follow hard link if present (value == hash_length bytes)
      let hash_length = algo.hash_length();
      let value = if raw_value.len() == hash_length {
        self.engine.get_entry(&raw_value)?.map(|(_h, _k, v)| v).unwrap_or_default()
      } else {
        raw_value
      };
      if !value.is_empty() {
        let recheck_children = if crate::engine::btree::is_btree_format(&value) {
          crate::engine::btree::btree_list_from_node(&value, self.engine, hash_length, false).unwrap_or_default()
        } else {
          deserialize_child_entries(&value, hash_length, 0).unwrap_or_default()
        };
        if !recheck_children.is_empty() {
          tracing::warn!(
            path = %normalized,
            orphaned_children = recheck_children.len(),
            "TOCTOU race in delete_directory: children were added concurrently and are now orphaned"
          );
        }
      }
    }

    txn.commit_after(namespace)?;

    // Update process-local state only after the hard commit succeeds.
    self.engine.counters().record_directory_delete();

    ctx.emit(
      EVENT_ENTRIES_DELETED,
      serde_json::json!({"entries": [{
        "path": normalized,
        "entry_type": "directory",
      }]}),
    );

    Ok(())
  }

  /// List the children of a directory.
  ///
  /// B-tree directory damage is handled best-effort: readable branches are
  /// returned and damaged branches are logged. `NotFound` is still returned as
  /// an error when the directory genuinely doesn't exist.
  pub fn list_directory(&self, path: &str) -> EngineResult<Vec<ChildEntry>> {
    let normalized = normalize_path(path);
    let result = self.list_directory_with_traversal(&normalized)?;
    for warning in &result.issues {
      tracing::warn!(
        path = %normalized,
        node_hash = %warning.node_hash_hex().unwrap_or_else(|| "inline-root".to_string()),
        reason = %warning.reason,
        "Directory index is not completely readable; returning diagnostic listing result"
      );
    }
    Ok(result.entries)
  }

  /// List every live child, failing if any directory or path-key state needed
  /// to prove a complete result is malformed, missing, or stale.
  pub fn list_directory_strict(&self, path: &str) -> EngineResult<Vec<ChildEntry>> {
    let mut entries = Vec::new();
    self.visit_live_directory_children_strict(path, |child| {
      entries.push(child.clone());
      Ok(true)
    })?;
    Ok(entries)
  }

  /// Visit live directory children in key order without collecting a complete
  /// B-tree listing. Flat compatibility directories remain bounded by the
  /// conversion threshold and retain the existing all-or-empty corruption
  /// behavior.
  pub(crate) fn visit_live_directory_children<F>(&self, path: &str, mut visitor: F) -> EngineResult<bool>
  where
    F: FnMut(&ChildEntry) -> EngineResult<bool>,
  {
    self.visit_live_directory_children_with_mode(path, crate::engine::btree::BTreeWalkMode::BestEffort, &mut visitor)
  }

  pub(crate) fn visit_live_directory_children_strict<F>(&self, path: &str, mut visitor: F) -> EngineResult<bool>
  where
    F: FnMut(&ChildEntry) -> EngineResult<bool>,
  {
    self.visit_live_directory_children_with_mode(path, crate::engine::btree::BTreeWalkMode::Strict, &mut visitor)
  }

  fn visit_live_directory_children_with_mode<F>(
    &self,
    path: &str,
    mode: crate::engine::btree::BTreeWalkMode,
    visitor: &mut F,
  ) -> EngineResult<bool>
  where
    F: FnMut(&ChildEntry) -> EngineResult<bool>,
  {
    let normalized = normalize_path(path);
    let hash_length = self.engine.hash_algo().hash_length();
    let Some((header, value)) = self.load_directory_listing_data(&normalized)? else {
      return Err(EngineError::NotFound(normalized));
    };
    if value.is_empty() {
      return Ok(true);
    }

    if !crate::engine::btree::is_btree_format(&value) {
      let mut decoded = Vec::new();
      let decode_result = Self::visit_bounded_flat_children(&value, hash_length, header.entry_version, |child| {
        decoded.push(child.clone());
        Ok(true)
      });
      let children = match decode_result {
        Ok(_) => self.filter_live_children_with_mode(&normalized, decoded, mode)?,
        Err(error) => {
          if mode == crate::engine::btree::BTreeWalkMode::Strict {
            return Err(Self::corrupt_flat_directory_error(&normalized, error));
          }
          tracing::warn!(path = %normalized, %error, "Corrupt flat directory index; returning an empty listing");
          return Ok(true);
        }
      };
      for child in &children {
        if !visitor(child)? {
          return Ok(false);
        }
      }
      return Ok(true);
    }

    let mut visit_live_child = |child: &ChildEntry| -> EngineResult<bool> {
      if self.live_child_for_mode(&normalized, child, mode)? {
        return visitor(child);
      }
      Ok(true)
    };
    let result =
      crate::engine::btree::btree_visit_from_node_with_mode(&value, self.engine, hash_length, false, mode, &mut visit_live_child)?;
    for warning in result.warnings {
      tracing::warn!(
        path = %normalized,
        node_hash = %warning.node_hash_hex().unwrap_or_else(|| "inline-root".to_string()),
        reason = %warning.reason,
        "B-tree directory index partially unreadable; returning partial listing"
      );
    }
    Ok(result.visitor_completion.is_exhausted())
  }

  /// List a directory without discarding flat or B-tree corruption evidence.
  pub fn list_directory_with_traversal(&self, path: &str) -> EngineResult<DirectoryTraversalResult> {
    let normalized = normalize_path(path);
    let hash_length = self.engine.hash_algo().hash_length();
    let read_result = self.load_directory_listing_data(&normalized);
    match read_result {
      Ok(Some((header, value))) => {
        if normalized == "/" {
          tracing::debug!(
            value_len = value.len(),
            is_btree = if value.is_empty() { false } else { crate::engine::btree::is_btree_format(&value) },
            first_bytes = %if value.is_empty() { "empty".to_string() } else { hex::encode(&value[..value.len().min(16)]) },
            "list_directory: root entry"
          );
        }
        if value.is_empty() {
          return Ok(DirectoryTraversalResult { entries: Vec::new(), issues: Vec::new(), integrity: TraversalIntegrity::Complete });
        }
        if crate::engine::btree::is_btree_format(&value) {
          let result = crate::engine::btree::btree_list_from_node_with_mode(
            &value,
            self.engine,
            hash_length,
            false,
            crate::engine::btree::BTreeWalkMode::BestEffort,
          )?;
          let children = self.filter_live_children(&normalized, result.entries)?;
          return Ok(DirectoryTraversalResult { entries: children, issues: result.warnings, integrity: result.integrity });
        }
        match deserialize_child_entries(&value, hash_length, header.entry_version) {
          Ok(children) => Ok(DirectoryTraversalResult {
            entries: self.filter_live_children(&normalized, children)?,
            issues: Vec::new(),
            integrity: TraversalIntegrity::Complete,
          }),
          Err(error) => Ok(DirectoryTraversalResult {
            entries: Vec::new(),
            issues: vec![crate::engine::btree::BTreeWalkWarning {
              node_hash: None,
              reason: format!("Corrupt flat directory index at {normalized}: {error}"),
            }],
            integrity: TraversalIntegrity::Corrupt,
          }),
        }
      }
      Ok(None) => Err(EngineError::NotFound(normalized)),
      Err(error) => Err(error),
    }
  }

  /// List directory children and return any best-effort B-tree traversal
  /// warnings. This is used by verify so damaged B-tree branches are surfaced
  /// as repairable directory issues instead of disappearing behind the normal
  /// read-path fallback.
  pub fn list_directory_with_btree_warnings(
    &self,
    path: &str,
  ) -> EngineResult<(Vec<ChildEntry>, Vec<crate::engine::btree::BTreeWalkWarning>)> {
    let result = self.list_directory_with_traversal(path)?;
    Ok((result.entries, result.issues))
  }

  /// Return a bounded live window without materializing an entire B-tree directory.
  pub fn list_directory_window(&self, path: &str, offset: usize, limit: usize) -> EngineResult<DirectoryListWindow> {
    self.list_directory_window_inner(path, offset, limit, true, true, crate::engine::btree::BTreeWalkMode::BestEffort)
  }

  /// Return a bounded live window while rejecting malformed, missing, or stale
  /// state encountered before the visitor reaches the requested boundary.
  pub fn list_directory_window_strict(&self, path: &str, offset: usize, limit: usize) -> EngineResult<DirectoryListWindow> {
    self.list_directory_window_inner(path, offset, limit, true, true, crate::engine::btree::BTreeWalkMode::Strict)
  }

  /// Visit every raw child for verification without collecting a complete
  /// B-tree listing, filtering dead path keys, or healing stale directory
  /// state. Flat directories remain structurally bounded by the conversion
  /// threshold; B-tree directories are visited one leaf at a time.
  pub(crate) fn visit_directory_for_verification<F>(
    &self,
    path: &str,
    mut visitor: F,
  ) -> EngineResult<(Vec<crate::engine::btree::BTreeWalkWarning>, bool)>
  where
    F: FnMut(&ChildEntry) -> EngineResult<()>,
  {
    let normalized = normalize_path(path);
    let hash_length = self.engine.hash_algo().hash_length();
    let dir_key = directory_path_hash(&normalized, &self.engine.hash_algo())?;
    let (listing, recovered_stale_path_key) = match self.recover_directory_data_if_stale(&normalized, &dir_key)? {
      Some(pair) => (Some(pair), true),
      None => (self.read_directory_data(&dir_key)?, false),
    };
    let Some((header, value)) = listing else {
      if normalized == "/" {
        let head = self.engine.head_hash()?;
        if head.is_empty() || head.iter().all(|byte| *byte == 0) {
          return Ok((Vec::new(), false));
        }
      }
      return Err(EngineError::NotFound(normalized));
    };
    if value.is_empty() {
      return Ok((Vec::new(), recovered_stale_path_key));
    }
    if crate::engine::btree::is_btree_format(&value) {
      let mut visit = |child: &ChildEntry| -> EngineResult<bool> {
        visitor(child)?;
        Ok(true)
      };
      let result = crate::engine::btree::btree_visit_from_node_with_mode(
        &value,
        self.engine,
        hash_length,
        false,
        crate::engine::btree::BTreeWalkMode::BestEffort,
        &mut visit,
      )?;
      return Ok((result.warnings, recovered_stale_path_key));
    }

    Self::visit_bounded_flat_children(&value, hash_length, header.entry_version, |child| {
      visitor(&child)?;
      Ok(true)
    })?;
    Ok((Vec::new(), recovered_stale_path_key))
  }

  pub(crate) fn visit_bounded_flat_children<F>(data: &[u8], hash_length: usize, version: u8, mut visitor: F) -> EngineResult<bool>
  where
    F: FnMut(&ChildEntry) -> EngineResult<bool>,
  {
    let mut offset = 0usize;
    let mut count = 0usize;
    while offset < data.len() {
      if count >= crate::engine::btree::BTREE_CONVERSION_THRESHOLD {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!(
            "flat directory exceeds the bounded {}-entry compatibility limit",
            crate::engine::btree::BTREE_CONVERSION_THRESHOLD
          ),
        });
      }
      let (child, consumed) = ChildEntry::deserialize(&data[offset..], hash_length, version)?;
      if consumed == 0 {
        return Err(EngineError::CorruptEntry { offset: 0, reason: "flat directory child consumed zero bytes".to_string() });
      }
      offset = offset
        .checked_add(consumed)
        .ok_or_else(|| EngineError::CorruptEntry { offset: 0, reason: "flat directory offset overflow".to_string() })?;
      count = count.saturating_add(1);
      if !visitor(&child)? {
        return Ok(false);
      }
    }
    Ok(true)
  }

  fn list_directory_window_inner(
    &self,
    path: &str,
    offset: usize,
    limit: usize,
    filter_live: bool,
    heal_stale: bool,
    mode: crate::engine::btree::BTreeWalkMode,
  ) -> EngineResult<DirectoryListWindow> {
    let normalized = normalize_path(path);
    let hash_length = self.engine.hash_algo().hash_length();
    let Some((header, value)) = self.load_directory_listing_data_inner(&normalized, heal_stale)? else {
      return Err(EngineError::NotFound(normalized));
    };
    if value.is_empty() {
      return Ok(DirectoryListWindow {
        entries: Vec::new(),
        has_more: false,
        warnings: Vec::new(),
        integrity: TraversalIntegrity::Complete,
        visitor_completion: VisitorCompletion::Exhausted,
      });
    }

    if !crate::engine::btree::is_btree_format(&value) {
      let children = match deserialize_child_entries(&value, hash_length, header.entry_version) {
        Ok(children) if filter_live => self.filter_live_children_with_mode(&normalized, children, mode)?,
        Ok(children) => children,
        Err(error) => {
          if !filter_live {
            return Err(error);
          }
          if mode == crate::engine::btree::BTreeWalkMode::Strict {
            return Err(Self::corrupt_flat_directory_error(&normalized, error));
          }
          tracing::warn!(path = %normalized, error = %error, "Corrupt flat directory index; returning an empty listing window");
          let warning = crate::engine::btree::BTreeWalkWarning {
            node_hash: None,
            reason: format!("Corrupt flat directory index at {normalized}: {error}"),
          };
          return Ok(DirectoryListWindow {
            entries: Vec::new(),
            has_more: false,
            warnings: vec![warning],
            integrity: TraversalIntegrity::Corrupt,
            visitor_completion: VisitorCompletion::Exhausted,
          });
        }
      };
      let start = offset.min(children.len());
      let end = start.saturating_add(limit).min(children.len());
      let has_more = end < children.len();
      return Ok(DirectoryListWindow {
        entries: children[start..end].to_vec(),
        has_more,
        warnings: Vec::new(),
        integrity: TraversalIntegrity::Complete,
        visitor_completion: if has_more { VisitorCompletion::StoppedByVisitor } else { VisitorCompletion::Exhausted },
      });
    }

    let mut seen = 0usize;
    let mut entries = Vec::with_capacity(limit.min(crate::engine::btree::BTREE_MAX_LEAF_ENTRIES));
    let mut has_more = false;
    let mut visitor = |child: &ChildEntry| -> EngineResult<bool> {
      if filter_live && !self.live_child_for_mode(&normalized, child, mode)? {
        return Ok(true);
      }
      if seen < offset {
        seen = seen.saturating_add(1);
        return Ok(true);
      }
      if entries.len() < limit {
        entries.push(child.clone());
        return Ok(true);
      }
      has_more = true;
      Ok(false)
    };
    let visit = crate::engine::btree::btree_visit_from_node_with_mode(&value, self.engine, hash_length, false, mode, &mut visitor)?;
    Ok(DirectoryListWindow {
      entries,
      has_more,
      warnings: visit.warnings,
      integrity: visit.integrity,
      visitor_completion: visit.visitor_completion,
    })
  }

  fn load_directory_listing_data(&self, normalized: &str) -> EngineResult<Option<(crate::engine::entry_header::EntryHeader, Vec<u8>)>> {
    self.load_directory_listing_data_inner(normalized, true)
  }

  fn load_directory_listing_data_inner(
    &self,
    normalized: &str,
    heal_stale: bool,
  ) -> EngineResult<Option<(crate::engine::entry_header::EntryHeader, Vec<u8>)>> {
    let algo = self.engine.hash_algo();
    let dir_key = directory_path_hash(normalized, &algo)?;
    if normalized == "/" {
      let snapshot = self.engine.kv_snapshot.load();
      if let Some(kv_entry) = snapshot.get(&dir_key)? {
        tracing::debug!(kv_offset = kv_entry.offset, kv_type = kv_entry.type_flags, "list_directory: root KV entry");
      }
    }

    // HEAD/dir-key divergence is healed by the same path for full and bounded listings.
    if let Some(pair) = self.recover_directory_data_if_stale(normalized, &dir_key)? {
      if heal_stale {
        if let Err(error) = self.heal_stale_dir_keys_along_path(normalized) {
          tracing::warn!(path = %normalized, error = %error, "Ancestor dir_key heal failed; continuing with served canonical content");
        }
      }
      Ok(Some(pair))
    } else {
      self.read_directory_data(&dir_key)
    }
  }

  fn filter_live_children(&self, parent: &str, children: Vec<ChildEntry>) -> EngineResult<Vec<ChildEntry>> {
    self.filter_live_children_with_mode(parent, children, crate::engine::btree::BTreeWalkMode::BestEffort)
  }

  fn corrupt_flat_directory_error(path: &str, error: EngineError) -> EngineError {
    EngineError::CorruptEntry { offset: 0, reason: format!("Corrupt flat directory index at {path}: {error}") }
  }

  fn filter_live_children_with_mode(
    &self,
    parent: &str,
    children: Vec<ChildEntry>,
    mode: crate::engine::btree::BTreeWalkMode,
  ) -> EngineResult<Vec<ChildEntry>> {
    let mut live = Vec::with_capacity(children.len());

    for child in children {
      if self.live_child_for_mode(parent, &child, mode)? {
        live.push(child);
      }
    }

    Ok(live)
  }

  fn live_child_for_mode(&self, parent: &str, child: &ChildEntry, mode: crate::engine::btree::BTreeWalkMode) -> EngineResult<bool> {
    let algo = self.engine.hash_algo();
    let child_path = if parent == "/" { format!("/{}", child.name) } else { format!("{}/{}", parent, child.name) };
    let state = match EntryType::from_u8(child.entry_type) {
      Ok(EntryType::FileRecord) => self.child_path_state(&file_path_hash(&child_path, &algo)?),
      Ok(EntryType::DirectoryIndex) => self.child_path_state(&directory_path_hash(&child_path, &algo)?),
      Ok(EntryType::Symlink) => self.child_path_state(&symlink_path_hash(&child_path, &algo)?),
      Ok(_) => Ok(ChildPathState::Live),
      Err(error) => Err(error),
    };
    match state {
      Ok(ChildPathState::Live) => Ok(true),
      Ok(ChildPathState::Deleted) => {
        tracing::debug!(parent = %parent, child_path = %child_path, entry_type = child.entry_type, "Skipping directory child with an authoritative deletion tombstone");
        Ok(false)
      }
      Ok(ChildPathState::Missing) if mode == crate::engine::btree::BTreeWalkMode::Strict => Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("directory '{}' contains child '{}' with no path-key authority", parent, child_path),
      }),
      Ok(ChildPathState::Missing) => {
        tracing::warn!(parent = %parent, child_path = %child_path, entry_type = child.entry_type, "Skipping stale directory child whose path key is not live");
        Ok(false)
      }
      Err(error) if mode == crate::engine::btree::BTreeWalkMode::Strict => Err(error),
      Err(error) => {
        tracing::warn!(parent = %parent, child = %child.name, entry_type = child.entry_type, error = %error, "Skipping directory child with invalid entry type");
        Ok(false)
      }
    }
  }

  fn child_path_state(&self, path_key: &[u8]) -> EngineResult<ChildPathState> {
    Ok(match self.engine.get_kv_entry(path_key)? {
      Some(entry) if entry.is_deleted() => ChildPathState::Deleted,
      Some(_) => ChildPathState::Live,
      None => ChildPathState::Missing,
    })
  }

  /// Create an empty directory at the given path.
  pub fn create_directory(&self, ctx: &RequestContext, path: &str) -> EngineResult<()> {
    let normalized = normalize_path(path);
    let namespace = self.engine.namespace_write_guard()?;
    let txn = crate::engine::storage_engine::TransactionGuard::new(self.engine)?;
    let algo = self.engine.hash_algo();

    let dir_key = directory_path_hash(&normalized, &algo)?;

    // Store empty directory index at path-based key
    self.engine.store_entry(EntryType::DirectoryIndex, &dir_key, &[])?;

    // Also store at content-addressed key for immutable versioning
    let content_key = directory_content_hash(&[], &algo)?;
    self.engine.store_entry(EntryType::DirectoryIndex, &content_key, &[])?;

    // Update parent directory if this isn't root
    let now = chrono::Utc::now().timestamp_millis();
    if normalized != "/" {
      let child = ChildEntry {
        entry_type: EntryType::DirectoryIndex.to_u8(),
        hash: content_key, // content hash for tree walker
        total_size: 0,
        created_at: now,
        updated_at: now,
        name: file_name(&normalized).unwrap_or("").to_string(),
        content_type: None,
        virtual_time: now as u64,
        node_id: 0,
      };
      self.update_parent_directories(&normalized, child)?;
    }

    let entry_data = EntryEventData {
      path: normalized,
      entry_type: "directory".to_string(),
      content_type: None,
      size: 0,
      hash: String::new(),
      created_at: now,
      updated_at: now,
      previous_hash: None,
    };
    txn.commit_after(namespace)?;
    self.engine.counters().record_directory_create();
    ctx.emit(EVENT_ENTRIES_CREATED, serde_json::json!({"entries": [entry_data]}));

    Ok(())
  }

  /// Get the FileRecord metadata for a file path.
  pub fn get_metadata(&self, path: &str) -> EngineResult<Option<FileRecord>> {
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    let file_key = file_path_hash(&normalized, &algo)?;
    match self.engine.get_entry(&file_key)? {
      Some((header, _key, value)) => {
        let record = FileRecord::deserialize(&value, hash_length, header.entry_version)?;
        Ok(Some(record))
      }
      None => Ok(None),
    }
  }

  /// Rewrite a stored FileRecord to the current payload version if any of its
  /// materialized keys are missing or still point at an older entry version.
  ///
  /// This is a schema migration helper, not a user file write. It preserves the
  /// FileRecord timestamps, metadata, chunks, and parent directory entries while
  /// refreshing the path, identity, and current content-addressed FileRecord
  /// keys. Old content-addressed keys are left as garbage for normal GC.
  pub fn migrate_file_record_to_current_version(&self, path: &str) -> EngineResult<bool> {
    let mut memory =
      OperationMemoryBudget::new(self.engine, "file record migration", MemoryOwner::Migration, AdmissionClass::Maintenance, 0, None)?;
    self.migrate_file_record_to_current_version_with_memory(path, &mut memory)
  }

  pub(crate) fn migrate_file_record_to_current_version_with_memory(
    &self,
    path: &str,
    memory: &mut OperationMemoryBudget,
  ) -> EngineResult<bool> {
    let checkpoint = memory.checkpoint();
    let result = self.migrate_file_record_to_current_version_inner(path, memory);
    let release = memory.release_to(checkpoint, "file record migration workspace release failed");
    match (result, release) {
      (Ok(value), Ok(())) => Ok(value),
      (Err(error), Ok(())) => Err(error),
      (_, Err(error)) => Err(error),
    }
  }

  fn migrate_file_record_to_current_version_inner(&self, path: &str, memory: &mut OperationMemoryBudget) -> EngineResult<bool> {
    let path_workspace = path
      .len()
      .checked_mul(2)
      .and_then(|bytes| bytes.checked_add(512))
      .and_then(|bytes| u64::try_from(bytes).ok())
      .ok_or_else(|| EngineError::ResourceExhausted("file record migration path estimate overflow".to_string()))?;
    memory.reserve(path_workspace, "file record migration path admission failed")?;
    let normalized = normalize_path(path);
    let _namespace = self.engine.namespace_write_guard()?;
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();
    let file_key = file_path_hash(&normalized, &algo)?;

    let path_entry = self.engine.get_kv_entry(&file_key)?.ok_or_else(|| EngineError::NotFound(normalized.clone()))?;
    let record_workspace = u64::from(path_entry.total_length)
      .checked_mul(3)
      .and_then(|bytes| bytes.checked_add(std::mem::size_of::<FileRecord>() as u64))
      .ok_or_else(|| EngineError::ResourceExhausted("file record migration record estimate overflow".to_string()))?;
    memory.reserve(record_workspace, "file record migration record admission failed")?;
    let (path_header, _stored_key, value) = self.engine.get_entry(&file_key)?.ok_or_else(|| EngineError::NotFound(normalized.clone()))?;
    let mut record = FileRecord::deserialize(&value, hash_length, path_header.entry_version)?;

    let mut needs_migration = path_header.entry_version < CURRENT_FILE_RECORD_VERSION;
    if record.path != normalized {
      record.path = normalized.clone();
      needs_migration = true;
    }

    if record.content_hash.len() != hash_length {
      needs_migration = true;
    }
    ensure_file_record_content_hash_for_migration(self.engine, &mut record, memory)?;

    let identity_key = file_identity_hash(&normalized, record.content_type.as_deref(), &record.chunk_hashes, &algo)?;
    if file_record_header_needs_migration(self.engine, &identity_key)? {
      needs_migration = true;
    }

    let file_value = record.serialize(hash_length)?;
    let content_key = file_content_hash(&file_value, &algo)?;
    if file_record_header_needs_migration(self.engine, &content_key)? {
      needs_migration = true;
    }
    drop(file_value);
    drop(value);

    if !needs_migration {
      return Ok(false);
    }

    materialize_file_record_entries(self.engine, &normalized, &mut record, path_header.flags)?;

    Ok(true)
  }

  /// Check if a file or directory exists at the given path.
  pub fn exists(&self, path: &str) -> EngineResult<bool> {
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();

    let file_key = file_path_hash(&normalized, &algo)?;
    if self.engine.has_entry(&file_key)? {
      return Ok(true);
    }

    let dir_key = directory_path_hash(&normalized, &algo)?;
    self.engine.has_entry(&dir_key)
  }

  /// List deleted files whose paths are under the given directory.
  /// Returns a list of (path, deleted_at) tuples.
  pub fn list_deleted(&self, dir_path: &str) -> EngineResult<Vec<crate::engine::deletion_record::DeletionRecord>> {
    let normalized = normalize_path(dir_path);
    let prefix = if normalized == "/" { "/".to_string() } else { format!("{}/", normalized.trim_end_matches('/')) };
    let family_policy = SystemFamilyPolicyResolver::new(self.engine.hash_algo())?;

    let deletion_entries = self.engine.entries_by_type(crate::engine::kv_store::KV_TYPE_DELETION)?;

    let mut results = Vec::new();
    for (_hash, value) in &deletion_entries {
      // TODO: when a v1 DeletionRecord format ships, plumb header.entry_version
      // through entries_by_type (it currently exposes only hash+value, not the
      // surrounding EntryHeader). For now every entry on disk is v0.
      if let Ok(record) = crate::engine::deletion_record::DeletionRecord::deserialize(value, 0) {
        // Check if this deletion is a direct child of the requested directory
        if record.path.starts_with(&prefix) || (normalized == "/" && record.path.starts_with('/')) {
          let remainder = if normalized == "/" { &record.path[1..] } else { &record.path[prefix.len()..] };
          // Direct child: no further slashes in the remainder
          if !remainder.contains('/') && !remainder.is_empty() {
            match family_policy.generic_data_path_selection(&record.path)? {
              GenericDataPathSelection::Include => results.push(record),
              GenericDataPathSelection::Conceal | GenericDataPathSelection::StructuralContainer => {}
            }
          }
        }
      }
    }

    // Sort by deleted_at descending (most recent first)
    results.sort_by(|a, b| b.deleted_at.cmp(&a.deleted_at));
    // Deduplicate by path (keep most recent deletion)
    let mut seen = std::collections::HashSet::new();
    results.retain(|r| seen.insert(r.path.clone()));

    Ok(results)
  }

  /// Take an auto-snapshot before a destructive operation.
  /// Uses a per-lane AtomicI64 so delete/restore/manual snapshots
  /// don't block each other. Each lane throttles independently.
  fn auto_snapshot_throttled(&self, ctx: &RequestContext, lane: &std::sync::atomic::AtomicI64, throttle_ms: i64, prefix: &str) {
    if !crate::engine::lifecycle_config::snapshot_writes_enabled(self.engine) {
      tracing::debug!("Auto-snapshot ({}) skipped because snapshot writes are disabled", prefix);
      return;
    }

    use std::sync::atomic::Ordering;
    let now = chrono::Utc::now().timestamp_millis();
    let last = lane.load(Ordering::Relaxed);
    let elapsed = now - last;

    if elapsed < throttle_ms && last > 0 {
      return;
    }

    // Try to claim the slot (CAS prevents races)
    if lane.compare_exchange(last, now, Ordering::SeqCst, Ordering::Relaxed).is_err() {
      return; // another thread beat us
    }

    let vm = crate::engine::version_manager::VersionManager::new(self.engine);
    let dt = chrono::Utc::now();
    let name = format!(
      "{} {}-{}-{} {}:{}:{}.{:03}",
      prefix,
      dt.format("%Y"),
      dt.format("%m"),
      dt.format("%d"),
      dt.format("%H"),
      dt.format("%M"),
      dt.format("%S"),
      dt.timestamp_subsec_millis(),
    );

    // Use a system context so the auto-snapshot does NOT emit a public
    // `versions_created` event — auto-snapshots are an implementation
    // detail of the calling operation (delete/restore), not a user-visible
    // version mutation. The caller's own event (entries_deleted etc.)
    // remains the observable signal.
    let sys_ctx = crate::engine::request_context::RequestContext::system();
    let _ = ctx; // Keep ctx parameter for future use (currently unused after the suppression above)
    let mut metadata = std::collections::HashMap::new();
    metadata.insert(
      crate::engine::lifecycle_config::SNAPSHOT_TYPE_KEY.to_string(),
      crate::engine::lifecycle_config::SNAPSHOT_TYPE_AUTO.to_string(),
    );
    match vm.create_snapshot(&sys_ctx, &name, metadata) {
      Ok(_) => {
        tracing::info!(snapshot = %name, "Auto-snapshot ({})", prefix);
      }
      Err(e) => {
        tracing::warn!("Auto-snapshot ({}) failed: {}", prefix, e);
        lane.store(last, Ordering::Relaxed);
      }
    }
  }

  /// Auto-snapshot before delete — own lane, 60s throttle.
  fn auto_snapshot_before_delete(&self, ctx: &RequestContext) {
    self.auto_snapshot_throttled(ctx, &self.engine.last_auto_snapshot_delete, 60_000, "auto-pre-delete");
  }

  /// Auto-snapshot before restore — own lane, 60s throttle.
  pub fn auto_snapshot_before_restore(&self, ctx: &RequestContext) {
    self.auto_snapshot_throttled(ctx, &self.engine.last_auto_snapshot_restore, 60_000, "auto-pre-restore");
  }

  /// Restore a deleted file by un-marking it in the KV and re-adding
  /// it to its parent directory.
  pub fn restore_deleted_file(&self, ctx: &RequestContext, path: &str) -> EngineResult<()> {
    let normalized = normalize_path(path);
    let namespace = self.engine.namespace_write_guard()?;
    let txn = crate::engine::storage_engine::TransactionGuard::new(self.engine)?;
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    let file_key = file_path_hash(&normalized, &algo)?;

    // Try to read the file record even though it's marked deleted.
    // The engine read validates that the KV offset still points into the
    // current WAL region before touching disk.
    let mut file_record = {
      let (header, _key, value) = self
        .engine
        .get_entry_including_deleted(&file_key)?
        .ok_or_else(|| EngineError::NotFound(format!("No record found for deleted file: {}", normalized)))?;
      FileRecord::deserialize(&value, hash_length, header.entry_version)?
    };
    ensure_file_record_content_hash(self.engine, &mut file_record)?;

    let keys = materialize_file_record_entries(self.engine, &normalized, &mut file_record, 0)?;

    // Re-add to parent directory using identity_key (not file_key)
    let child = ChildEntry {
      name: crate::engine::path_utils::file_name(&normalized).unwrap_or("").to_string(),
      entry_type: EntryType::FileRecord.to_u8(),
      hash: keys.identity_key,
      total_size: file_record.total_size,
      content_type: file_record.content_type.clone(),
      created_at: file_record.created_at,
      updated_at: chrono::Utc::now().timestamp_millis(),
      virtual_time: 0,
      node_id: 0,
    };
    self.update_parent_directories(&normalized, child)?;

    let event = serde_json::json!({"entries": [{
      "path": normalized,
      "entry_type": "file",
      "content_type": file_record.content_type,
      "size": file_record.total_size,
    }]});

    txn.commit_after(namespace)?;
    self.engine.counters().record_file_restore(file_record.total_size);
    ctx.emit(crate::engine::engine_event::EVENT_ENTRIES_CREATED, event);

    Ok(())
  }

  /// Ensure the root directory exists. Called during database creation.
  pub fn ensure_root_directory(&self, _ctx: &RequestContext) -> EngineResult<()> {
    let _namespace = self.engine.direct_hard_authority_guard()?;
    let algo = self.engine.hash_algo();
    let dir_key = directory_path_hash("/", &algo)?;

    // If the root directory entry exists, leave it alone — even if it's
    // unreadable (e.g., dangling hard link). Overwriting an existing root
    // destroys all directory tree state. A KV rebuild (verify --repair)
    // is the correct recovery path, not silent recreation.
    if self.engine.has_entry(&dir_key)? {
      match self.list_directory("/") {
        Ok(children) if !children.is_empty() => return Ok(()),
        Ok(_) => {
          // Root exists but lists as empty — might be a hard link with
          // missing target. Log but DO NOT overwrite.
          tracing::warn!("Root directory exists but appears empty. Run 'aeordb verify --repair' if data is missing.");
          return Ok(());
        }
        Err(e) => {
          // Root exists but is unreadable — DO NOT overwrite.
          tracing::warn!("Root directory exists but is unreadable ({}). Run 'aeordb verify --repair' to recover.", e);
          return Ok(());
        }
      }
    }

    self.engine.store_entry(EntryType::DirectoryIndex, &dir_key, &[])?;

    // Also store at content-addressed key for immutable versioning
    let content_key = directory_content_hash(&[], &algo)?;
    self.engine.store_entry(EntryType::DirectoryIndex, &content_key, &[])?;

    // Update HEAD to point to content hash (immutable) instead of path hash
    self.engine.update_head(&content_key)?;

    Ok(())
  }

  fn repair_workspace_file_child(
    &self,
    entry: &crate::engine::kv_store::KVEntry,
    hash_length: usize,
    algo: &HashAlgorithm,
    family_policy: SystemFamilyPolicyResolver,
  ) -> EngineResult<RepairWorkspaceChild> {
    let Some((header, _key, value)) = self.engine.get_entry(&entry.hash)? else {
      return Ok(RepairWorkspaceChild::SkippedMalformed);
    };
    let record = match FileRecord::deserialize(&value, hash_length, header.entry_version) {
      Ok(record) => record,
      Err(_) => return Ok(RepairWorkspaceChild::SkippedMalformed),
    };
    let path = normalize_path(&record.path);
    if path == "/" || !family_policy.generic_data_path_is_visible(&path)? {
      return Ok(RepairWorkspaceChild::SkippedProtected);
    }
    let path_key = file_path_hash(&path, algo)?;
    if entry.hash != path_key {
      return Ok(RepairWorkspaceChild::SkippedNonPath);
    }
    if !self.file_record_chunks_live(&record)? {
      return Ok(RepairWorkspaceChild::SkippedDangling);
    }
    let name = file_name(&path).unwrap_or("");
    if name.is_empty() {
      return Ok(RepairWorkspaceChild::SkippedMalformed);
    }
    let identity_key = file_identity_hash(&path, record.content_type.as_deref(), &record.chunk_hashes, algo)?;
    let child_hash = if self.engine.has_entry(&identity_key)? { identity_key } else { path_key };
    let parent = parent_path(&path).unwrap_or_else(|| "/".to_string());
    Ok(RepairWorkspaceChild::Child {
      parent,
      child: ChildEntry {
        name: name.to_string(),
        entry_type: EntryType::FileRecord.to_u8(),
        hash: child_hash,
        total_size: record.total_size,
        content_type: record.content_type,
        created_at: record.created_at,
        updated_at: record.updated_at,
        virtual_time: 0,
        node_id: 0,
      },
    })
  }

  fn repair_workspace_symlink_child(
    &self,
    entry: &crate::engine::kv_store::KVEntry,
    algo: &HashAlgorithm,
    family_policy: SystemFamilyPolicyResolver,
  ) -> EngineResult<RepairWorkspaceChild> {
    let Some((header, _key, value)) = self.engine.get_entry(&entry.hash)? else {
      return Ok(RepairWorkspaceChild::SkippedMalformed);
    };
    let record = match SymlinkRecord::deserialize(&value, header.entry_version) {
      Ok(record) => record,
      Err(_) => return Ok(RepairWorkspaceChild::SkippedMalformed),
    };
    let path = normalize_path(&record.path);
    if path == "/" || !family_policy.generic_data_path_is_visible(&path)? {
      return Ok(RepairWorkspaceChild::SkippedProtected);
    }
    let path_key = symlink_path_hash(&path, algo)?;
    if entry.hash != path_key {
      return Ok(RepairWorkspaceChild::SkippedNonPath);
    }
    let name = file_name(&path).unwrap_or("");
    if name.is_empty() {
      return Ok(RepairWorkspaceChild::SkippedMalformed);
    }
    let identity_key = symlink_identity_hash(&path, &record.target, algo)?;
    let child_hash = if self.engine.has_entry(&identity_key)? { identity_key } else { path_key };
    let parent = parent_path(&path).unwrap_or_else(|| "/".to_string());
    Ok(RepairWorkspaceChild::Child {
      parent,
      child: ChildEntry {
        name: name.to_string(),
        entry_type: EntryType::Symlink.to_u8(),
        hash: child_hash,
        total_size: 0,
        content_type: None,
        created_at: record.created_at,
        updated_at: record.updated_at,
        virtual_time: 0,
        node_id: 0,
      },
    })
  }

  /// Rebuild the directory tree from current file and symlink path records.
  ///
  /// The authoritative path-record scan is page-wise and streams canonical
  /// children into a same-filesystem external workspace. Directories are then
  /// grouped and written bottom-up, so database-wide repair memory remains
  /// bounded without replaying historical content/identity copies.
  pub fn rebuild_directory_tree(&self, _ctx: &RequestContext) -> EngineResult<usize> {
    let algo = self.engine.hash_algo();
    let hash_length = self.engine.hash_algo().hash_length();
    let family_policy = SystemFamilyPolicyResolver::new(algo)?;
    let cancellation = self.engine.repair_cancellation();
    let _namespace = self.engine.direct_hard_authority_guard()?;
    let mut workspace =
      DirectoryRepairWorkspace::new(self.engine.database_path(), algo, self.engine.memory_coordinator().as_ref(), cancellation.clone())?;
    let mut memory = OperationMemoryBudget::new(
      self.engine,
      "directory tree repair scan",
      MemoryOwner::Repair,
      AdmissionClass::Critical(CriticalMemoryPurpose::BoundedRecovery),
      0,
      None,
    )?;
    let mut file_records_found = 0;
    let mut path_records_found = 0;
    let mut symlink_records_found = 0;
    let mut skipped_protected = 0;
    let mut skipped_non_path_key = 0;
    let mut skipped_dangling = 0;
    let mut skipped_error = 0;

    self.engine.visit_kv_entries_for_repair(|entry| {
      if cancellation.load(std::sync::atomic::Ordering::Acquire) {
        return Err(EngineError::ShuttingDown);
      }
      let entry_type = entry.entry_type();
      if !matches!(entry_type, crate::engine::kv_store::KV_TYPE_FILE_RECORD | crate::engine::kv_store::KV_TYPE_SYMLINK) {
        return Ok(true);
      }
      let checkpoint = memory.checkpoint();
      let record_memory = u64::from(entry.total_length)
        .checked_mul(3)
        .and_then(|bytes| bytes.checked_add(512))
        .ok_or_else(|| EngineError::ResourceExhausted("directory-repair record estimate overflow".to_string()))?;
      memory.reserve(record_memory, "directory-repair record admission failed")?;
      let result = (|| -> EngineResult<()> {
        match entry_type {
          crate::engine::kv_store::KV_TYPE_FILE_RECORD => {
            file_records_found += 1;
            match self.repair_workspace_file_child(entry, hash_length, &algo, family_policy)? {
              RepairWorkspaceChild::Child { parent, child } => {
                path_records_found += 1;
                workspace.push_child(&parent, child)?;
              }
              RepairWorkspaceChild::SkippedProtected => skipped_protected += 1,
              RepairWorkspaceChild::SkippedNonPath => skipped_non_path_key += 1,
              RepairWorkspaceChild::SkippedDangling => skipped_dangling += 1,
              RepairWorkspaceChild::SkippedMalformed => skipped_error += 1,
            }
            Ok(())
          }
          crate::engine::kv_store::KV_TYPE_SYMLINK => {
            symlink_records_found += 1;
            match self.repair_workspace_symlink_child(entry, &algo, family_policy)? {
              RepairWorkspaceChild::Child { parent, child } => {
                path_records_found += 1;
                workspace.push_child(&parent, child)?;
              }
              RepairWorkspaceChild::SkippedProtected => skipped_protected += 1,
              RepairWorkspaceChild::SkippedNonPath => skipped_non_path_key += 1,
              RepairWorkspaceChild::SkippedDangling => skipped_dangling += 1,
              RepairWorkspaceChild::SkippedMalformed => skipped_error += 1,
            }
            Ok(())
          }
          _ => unreachable!("entry type filtered above"),
        }
      })();
      let release = memory.release_to(checkpoint, "directory-repair record release failed");
      match (result, release) {
        (Ok(()), Ok(())) => Ok(true),
        (Err(error), Ok(())) => Err(error),
        (_, Err(error)) => Err(error),
      }
    })?;

    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut dirs_written = 0usize;
    for depth in (0..=workspace.max_depth()).rev() {
      let mut cursor = workspace.finish_depth(depth)?;
      while let Some((dir_path, mut children)) = cursor.next_group(&mut workspace)? {
        Self::sort_rebuilt_children(&mut children);
        let store_result = self.store_rebuilt_directory(&dir_path, children, hash_length, &algo);
        let release_result = workspace.release_group();
        let (content_key, dir_size) = match (store_result, release_result) {
          (Ok(stored), Ok(())) => stored,
          (Err(error), Ok(())) => return Err(error),
          (_, Err(error)) => return Err(error),
        };
        dirs_written += 1;

        if dir_path != "/" {
          let parent = parent_path(&dir_path).unwrap_or_else(|| "/".to_string());
          let child = ChildEntry {
            name: crate::engine::path_utils::file_name(&dir_path).unwrap_or("").to_string(),
            entry_type: crate::engine::entry_type::EntryType::DirectoryIndex.to_u8(),
            hash: content_key,
            total_size: dir_size,
            content_type: None,
            created_at: now_ms,
            updated_at: now_ms,
            virtual_time: now_ms as u64,
            node_id: 0,
          };
          workspace.push_child(&parent, child)?;
        }
      }
    }

    tracing::debug!(
      file_records_found,
      symlink_records_found,
      path_records_found,
      skipped_protected,
      skipped_non_path_key,
      skipped_dangling,
      skipped_error,
      dirs_written,
      "rebuild_directory_tree complete"
    );

    Ok(dirs_written)
  }

  /// Rebuild one directory index from authoritative live path records.
  ///
  /// This is narrower than [`rebuild_directory_tree`]: it repairs a damaged
  /// B-tree directory without rewriting the whole tree. It reconstructs direct
  /// files and symlinks from live path keys, restores child directories implied
  /// by descendant path records, and preserves any readable child directories
  /// already present in the damaged directory.
  pub fn repair_directory_index_from_path_records(&self, path: &str) -> EngineResult<usize> {
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();
    let family_policy = SystemFamilyPolicyResolver::new(algo)?;
    let _namespace = self.engine.direct_hard_authority_guard()?;
    let mut memory = OperationMemoryBudget::new(
      self.engine,
      "targeted directory repair",
      MemoryOwner::Repair,
      AdmissionClass::Critical(CriticalMemoryPurpose::BoundedRecovery),
      256 * 1024,
      None,
    )?;
    let mut children = self.collect_repair_children_for_directory(&normalized, hash_length, &algo, family_policy, &mut memory)?;
    Self::sort_rebuilt_children(&mut children);
    let (content_key, dir_size) = self.store_rebuilt_directory(&normalized, children, hash_length, &algo)?;

    if normalized != "/" {
      let now_ms = chrono::Utc::now().timestamp_millis();
      let child = ChildEntry {
        name: file_name(&normalized).unwrap_or("").to_string(),
        entry_type: EntryType::DirectoryIndex.to_u8(),
        hash: content_key,
        total_size: dir_size,
        content_type: None,
        created_at: now_ms,
        updated_at: now_ms,
        virtual_time: now_ms as u64,
        node_id: 0,
      };
      self.update_parent_directories(&normalized, child)?;
    }

    tracing::info!(path = %normalized, "Repaired directory index from live path records");
    Ok(1)
  }

  fn collect_repair_children_for_directory(
    &self,
    dir_path: &str,
    hash_length: usize,
    algo: &HashAlgorithm,
    family_policy: SystemFamilyPolicyResolver,
    memory: &mut OperationMemoryBudget,
  ) -> EngineResult<Vec<ChildEntry>> {
    let mut children: std::collections::BTreeMap<String, ChildEntry> = std::collections::BTreeMap::new();
    self.collect_existing_directory_children_for_repair(dir_path, hash_length, algo, family_policy, &mut children, memory)?;

    let cancellation = self.engine.repair_cancellation();
    self.engine.visit_kv_entries_for_repair(|entry| {
      if cancellation.load(std::sync::atomic::Ordering::Acquire) {
        return Err(EngineError::ShuttingDown);
      }
      let checkpoint = memory.checkpoint();
      let record_memory = u64::from(entry.total_length)
        .checked_mul(3)
        .and_then(|bytes| bytes.checked_add(512))
        .ok_or_else(|| EngineError::ResourceExhausted("targeted directory-repair record estimate overflow".to_string()))?;
      match entry.entry_type() {
        crate::engine::kv_store::KV_TYPE_FILE_RECORD => {
          memory.reserve(record_memory, "targeted FileRecord repair admission failed")?;
          let result = self.repair_child_from_file_record(entry, dir_path, hash_length, algo, family_policy);
          let release = memory.release_to(checkpoint, "targeted FileRecord repair release failed");
          match (result, release) {
            (Ok(Some(child)), Ok(())) => Self::insert_repair_child(&mut children, child, memory)?,
            (Ok(None), Ok(())) => {}
            (Err(error), Ok(())) => return Err(error),
            (_, Err(error)) => return Err(error),
          }
        }
        crate::engine::kv_store::KV_TYPE_SYMLINK => {
          memory.reserve(record_memory, "targeted symlink repair admission failed")?;
          let result = self.repair_child_from_symlink_record(entry, dir_path, algo, family_policy);
          let release = memory.release_to(checkpoint, "targeted symlink repair release failed");
          match (result, release) {
            (Ok(Some(child)), Ok(())) => Self::insert_repair_child(&mut children, child, memory)?,
            (Ok(None), Ok(())) => {}
            (Err(error), Ok(())) => return Err(error),
            (_, Err(error)) => return Err(error),
          }
        }
        _ => {}
      }
      Ok(true)
    })?;

    Ok(children.into_values().collect())
  }

  fn collect_existing_directory_children_for_repair(
    &self,
    dir_path: &str,
    hash_length: usize,
    algo: &HashAlgorithm,
    family_policy: SystemFamilyPolicyResolver,
    children: &mut std::collections::BTreeMap<String, ChildEntry>,
    memory: &mut OperationMemoryBudget,
  ) -> EngineResult<()> {
    let dir_key = directory_path_hash(dir_path, algo)?;
    let Some(path_entry) = self.engine.get_kv_entry(&dir_key)? else {
      return Ok(());
    };
    let path_memory = u64::from(path_entry.total_length)
      .checked_mul(3)
      .ok_or_else(|| EngineError::ResourceExhausted("targeted directory path-record estimate overflow".to_string()))?;
    memory.reserve(path_memory, "targeted directory path-record admission failed")?;
    let Some((mut header, _key, mut value)) = self.engine.get_entry(&dir_key)? else {
      return Ok(());
    };
    if value.len() == hash_length {
      let target_hash = value;
      let Some(target_entry) = self.engine.get_kv_entry(&target_hash)? else {
        return Ok(());
      };
      let target_memory = u64::from(target_entry.total_length)
        .checked_mul(3)
        .ok_or_else(|| EngineError::ResourceExhausted("targeted directory content estimate overflow".to_string()))?;
      memory.reserve(target_memory, "targeted directory content admission failed")?;
      let Some((target_header, _target_key, target_value)) = self.engine.get_entry(&target_hash)? else {
        return Ok(());
      };
      header = target_header;
      value = target_value;
    }

    let mut preserve = |child: &ChildEntry| -> EngineResult<bool> {
      if child.entry_type != EntryType::DirectoryIndex.to_u8() {
        return Ok(true);
      }
      let child_path =
        if dir_path == "/" { format!("/{}", child.name) } else { format!("{}/{}", dir_path.trim_end_matches('/'), child.name) };
      if !family_policy.generic_data_path_is_visible(&child_path)? {
        return Ok(true);
      }
      if self.engine.has_entry(&directory_path_hash(&child_path, algo)?)? {
        Self::insert_repair_child(children, child.clone(), memory)?;
      }
      Ok(true)
    };

    if !value.is_empty() && crate::engine::btree::is_btree_format(&value) {
      crate::engine::btree::btree_visit_from_node_with_mode(
        &value,
        self.engine,
        hash_length,
        false,
        crate::engine::btree::BTreeWalkMode::BestEffort,
        &mut preserve,
      )?;
      return Ok(());
    }

    let mut offset = 0usize;
    while offset < value.len() {
      let (child, consumed) = match ChildEntry::deserialize(&value[offset..], hash_length, header.entry_version) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(()),
      };
      if consumed == 0 {
        return Err(EngineError::CorruptEntry { offset: 0, reason: "directory child decoder made no progress".to_string() });
      }
      preserve(&child)?;
      offset = offset
        .checked_add(consumed)
        .ok_or_else(|| EngineError::CorruptEntry { offset: 0, reason: "directory child offset overflow".to_string() })?;
    }
    Ok(())
  }

  fn insert_repair_child(
    children: &mut std::collections::BTreeMap<String, ChildEntry>,
    child: ChildEntry,
    memory: &mut OperationMemoryBudget,
  ) -> EngineResult<()> {
    if children.get(&child.name).is_some_and(|existing| existing == &child) {
      return Ok(());
    }
    let bytes = std::mem::size_of::<ChildEntry>()
      .saturating_add(child.name.len().saturating_mul(3))
      .saturating_add(child.hash.len().saturating_mul(2))
      .saturating_add(child.content_type.as_ref().map_or(0, |content_type| content_type.len().saturating_mul(2)))
      .saturating_add(128);
    memory.reserve(u64::try_from(bytes).unwrap_or(u64::MAX), "targeted directory child retention failed")?;
    children.insert(child.name.clone(), child);
    Ok(())
  }

  fn repair_child_from_file_record(
    &self,
    entry: &crate::engine::kv_store::KVEntry,
    dir_path: &str,
    hash_length: usize,
    algo: &HashAlgorithm,
    family_policy: SystemFamilyPolicyResolver,
  ) -> EngineResult<Option<ChildEntry>> {
    let Some((header, _key, value)) = self.engine.get_entry(&entry.hash)? else {
      return Ok(None);
    };
    let record = match FileRecord::deserialize(&value, hash_length, header.entry_version) {
      Ok(record) => record,
      Err(_) => return Ok(None),
    };
    let path = normalize_path(&record.path);
    if path == "/" || !family_policy.generic_data_path_is_visible(&path)? {
      return Ok(None);
    }
    let path_key = file_path_hash(&path, algo)?;
    if entry.hash != path_key {
      return Ok(None);
    }
    if !self.file_record_chunks_live(&record)? {
      return Ok(None);
    }

    let Some((child_name, direct_child)) = immediate_child_under(dir_path, &path) else {
      return Ok(None);
    };
    if !direct_child {
      return self.implied_directory_child(dir_path, &child_name, algo, hash_length);
    }

    let identity_key = file_identity_hash(&path, record.content_type.as_deref(), &record.chunk_hashes, algo)?;
    let child_hash = if self.engine.has_entry(&identity_key)? { identity_key } else { path_key };
    Ok(Some(ChildEntry {
      name: child_name,
      entry_type: EntryType::FileRecord.to_u8(),
      hash: child_hash,
      total_size: record.total_size,
      content_type: record.content_type.clone(),
      created_at: record.created_at,
      updated_at: record.updated_at,
      virtual_time: 0,
      node_id: 0,
    }))
  }

  fn repair_child_from_symlink_record(
    &self,
    entry: &crate::engine::kv_store::KVEntry,
    dir_path: &str,
    algo: &HashAlgorithm,
    family_policy: SystemFamilyPolicyResolver,
  ) -> EngineResult<Option<ChildEntry>> {
    let Some((header, _key, value)) = self.engine.get_entry(&entry.hash)? else {
      return Ok(None);
    };
    let record = match SymlinkRecord::deserialize(&value, header.entry_version) {
      Ok(record) => record,
      Err(_) => return Ok(None),
    };
    let path = normalize_path(&record.path);
    if path == "/" || !family_policy.generic_data_path_is_visible(&path)? {
      return Ok(None);
    }
    let path_key = symlink_path_hash(&path, algo)?;
    if entry.hash != path_key {
      return Ok(None);
    }

    let Some((child_name, direct_child)) = immediate_child_under(dir_path, &path) else {
      return Ok(None);
    };
    if !direct_child {
      return self.implied_directory_child(dir_path, &child_name, algo, algo.hash_length());
    }

    let identity_key = symlink_identity_hash(&path, &record.target, algo)?;
    let child_hash = if self.engine.has_entry(&identity_key)? { identity_key } else { path_key };
    Ok(Some(ChildEntry {
      name: child_name,
      entry_type: EntryType::Symlink.to_u8(),
      hash: child_hash,
      total_size: 0,
      content_type: None,
      created_at: record.created_at,
      updated_at: record.updated_at,
      virtual_time: 0,
      node_id: 0,
    }))
  }

  fn implied_directory_child(
    &self,
    dir_path: &str,
    child_name: &str,
    algo: &HashAlgorithm,
    hash_length: usize,
  ) -> EngineResult<Option<ChildEntry>> {
    let child_path =
      if dir_path == "/" { format!("/{}", child_name) } else { format!("{}/{}", dir_path.trim_end_matches('/'), child_name) };
    let Some((child_hash, total_size, timestamp)) = self.directory_child_hash_for_path(&child_path, hash_length, algo)? else {
      return Ok(None);
    };
    Ok(Some(ChildEntry {
      name: child_name.to_string(),
      entry_type: EntryType::DirectoryIndex.to_u8(),
      hash: child_hash,
      total_size,
      content_type: None,
      created_at: timestamp,
      updated_at: timestamp,
      virtual_time: timestamp as u64,
      node_id: 0,
    }))
  }

  fn directory_child_hash_for_path(
    &self,
    path: &str,
    hash_length: usize,
    algo: &HashAlgorithm,
  ) -> EngineResult<Option<(Vec<u8>, u64, i64)>> {
    let dir_key = directory_path_hash(path, algo)?;
    let Some((header, _key, value)) = self.engine.get_entry(&dir_key)? else {
      return Ok(None);
    };
    if value.len() == hash_length {
      return match self.engine.get_entry(&value)? {
        Some((_target_header, _target_key, target_value)) => Ok(Some((value, target_value.len() as u64, header.timestamp))),
        None => Ok(None),
      };
    }
    let content_key = if !value.is_empty() && crate::engine::btree::is_btree_format(&value) {
      let root = crate::engine::btree::BTreeNode::deserialize(&value, hash_length, header.entry_version)?;
      root.content_hash(hash_length, algo)?
    } else {
      directory_content_hash(&value, algo)?
    };
    Ok(Some((content_key, value.len() as u64, header.timestamp)))
  }

  fn file_record_chunks_live(&self, record: &FileRecord) -> EngineResult<bool> {
    for chunk_hash in &record.chunk_hashes {
      if !self.engine.has_entry(chunk_hash)? {
        return Ok(false);
      }
    }
    Ok(true)
  }

  fn sort_rebuilt_children(children: &mut [ChildEntry]) {
    children.sort_by(|a, b| {
      let a_is_dir = a.entry_type == EntryType::DirectoryIndex.to_u8();
      let b_is_dir = b.entry_type == EntryType::DirectoryIndex.to_u8();
      match (a_is_dir, b_is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
      }
    });
  }

  fn store_rebuilt_directory(
    &self,
    dir_path: &str,
    children: Vec<ChildEntry>,
    hash_length: usize,
    algo: &HashAlgorithm,
  ) -> EngineResult<(Vec<u8>, u64)> {
    let dir_key = directory_path_hash(dir_path, algo)?;
    let mut batch = WriteBatch::new();

    let (dir_value, content_key) = if children.len() >= crate::engine::btree::BTREE_CONVERSION_THRESHOLD {
      let root_hash = crate::engine::btree::btree_from_entries(self.engine, children, hash_length, algo)?;
      let root_entry = self
        .engine
        .get_entry(&root_hash)?
        .ok_or_else(|| EngineError::NotFound("B-tree root not found after directory rebuild".to_string()))?;
      self.engine.cache_dir_content(root_hash.clone(), root_entry.2.clone())?;
      (root_entry.2, root_hash)
    } else {
      let dir_value = serialize_child_entries(&children, hash_length)?;
      let content_key = directory_content_hash(&dir_value, algo)?;
      batch.add(EntryType::DirectoryIndex, content_key.clone(), dir_value.clone());
      self.engine.cache_dir_content(content_key.clone(), dir_value.clone())?;
      (dir_value, content_key)
    };

    batch.add(EntryType::DirectoryIndex, dir_key, content_key.clone());
    if dir_path == "/" {
      self.engine.flush_batch_and_update_head(batch, &content_key)?;
    } else {
      self.engine.flush_batch(batch)?;
    }

    Ok((content_key, dir_value.len() as u64))
  }

  /// Detect the compression algorithm for a file based on its parent's index config.
  /// Reads `.aeordb-config/indexes.json` under the parent path; returns Zstd if
  /// configured and the content type/size pass the `should_compress` heuristic, else None.
  fn detect_compression(&self, path: &str, content_type: Option<&str>, data_length: usize) -> CompressionAlgorithm {
    IndexConfigResolver::new(self.engine).compression_for_path(path, content_type, data_length)
  }

  /// Store a file with automatic index updates and optional compression.
  /// After storing the file, checks for index config at `.config/indexes.json`
  /// under the parent path and updates relevant indexes.
  /// Compression is determined by config or auto-detection via `should_compress`.
  pub fn store_file_with_indexing(
    &self,
    ctx: &RequestContext,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
  ) -> EngineResult<FileRecord> {
    let compression_algo = self.detect_compression(path, content_type, data.len());
    let file_record = self.store_file_internal(ctx, path, data, content_type, compression_algo)?;

    // Delegate to indexing pipeline using the detected content type from the file record
    let pipeline = crate::engine::indexing_pipeline::IndexingPipeline::new(self.engine);
    let detected_ct = file_record.content_type.as_deref();
    if let Err(e) = pipeline.run(ctx, path, data, detected_ct) {
      tracing::warn!("Indexing pipeline failed for '{}': {}", path, e);
    }

    Ok(file_record)
  }

  /// Store a file with the full indexing pipeline including parser plugin support.
  pub fn store_file_with_full_pipeline(
    &self,
    ctx: &RequestContext,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
    plugin_manager: Option<&crate::plugins::PluginManager>,
  ) -> EngineResult<FileRecord> {
    let compression_algo = self.detect_compression(path, content_type, data.len());

    let file_record = self.store_file_internal(ctx, path, data, content_type, compression_algo)?;

    // Use full pipeline with plugin manager, passing detected content type
    let pipeline = match plugin_manager {
      Some(pm) => crate::engine::indexing_pipeline::IndexingPipeline::with_plugin_manager(self.engine, pm),
      None => crate::engine::indexing_pipeline::IndexingPipeline::new(self.engine),
    };
    let detected_ct = file_record.content_type.as_deref();
    if let Err(e) = pipeline.run(ctx, path, data, detected_ct) {
      tracing::warn!("Indexing pipeline failed for '{}': {}", path, e);
    }

    Ok(file_record)
  }

  /// Delete a file and remove its entries from all indexes at that path.
  pub fn delete_file_with_indexing(&self, ctx: &RequestContext, path: &str) -> EngineResult<()> {
    let normalized = normalize_path(path);
    let namespace = self.engine.namespace_write_guard()?;
    let txn = crate::engine::storage_engine::TransactionGuard::new(self.engine)?;
    crate::engine::index_cleanup::remove_file_from_resolved_indexes(self.engine, &normalized)?;

    // Now delete the file itself
    let result = self.delete_file(ctx, path);
    txn.finish_after(result, namespace)
  }

  /// Read directory data by path key, following hard links and checking the
  /// content cache. Returns the entry header and directory value bytes.
  ///
  /// Hard link detection: if the value at dir_key is exactly hash_length bytes,
  /// it's a hard link (content hash pointer). Follow it to get the actual data.
  /// Backward compatible: values >hash_length are inline data (pre-optimization).
  /// Walk HEAD's tree from root down to `path`, returning the canonical
  /// content hash for the directory at `path` (the merkle-authoritative
  /// reference, as opposed to whatever the local `dir_key` currently
  /// hard-links to). Returns None if `path` is not reachable from HEAD.
  /// Used by `verify --repair` to permanently fix stale dir_keys.
  pub fn canonical_directory_content_hash(&self, path: &str) -> EngineResult<Option<Vec<u8>>> {
    let hash_length = self.engine.hash_algo().hash_length();
    let head_hash = self.engine.head_hash()?;
    if head_hash.is_empty() || head_hash.iter().all(|&b| b == 0) {
      return Ok(None);
    }
    let normalized = normalize_path(path);
    if normalized == "/" {
      return Ok(Some(head_hash));
    }
    let segments: Vec<&str> = normalized.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let mut current_content_hash = head_hash;
    for segment in &segments {
      let content = match self.engine.get_entry(&current_content_hash)? {
        Some((_h, _k, v)) => v,
        None => return Ok(None),
      };
      let children = if !content.is_empty() && crate::engine::btree::is_btree_format(&content) {
        crate::engine::btree::btree_list_from_node(&content, self.engine, hash_length, false)?
      } else if content.is_empty() {
        return Ok(None);
      } else {
        deserialize_child_entries(&content, hash_length, 0)?
      };
      let child = match children.iter().find(|c| c.name == *segment) {
        Some(c) => c,
        None => return Ok(None),
      };
      if child.entry_type != EntryType::DirectoryIndex.to_u8() {
        return Ok(None);
      }
      current_content_hash = child.hash.clone();
    }
    Ok(Some(current_content_hash))
  }

  /// Walk root → `path`, checking every ancestor's `dir_key` hard-link
  /// against HEAD's canonical content hash. Any ancestor whose dir_key
  /// is a stale hard-link is rewritten to point at canonical. Returns
  /// the number of dir_keys repaired.
  ///
  /// Called by `list_directory` when divergence is detected, so the
  /// online repair propagates up the ancestor chain rather than staying
  /// pinned to the queried leaf. Without this, `snapshot_restore` /
  /// `fork_promote` interactions could leave ancestor dir_keys stale
  /// indefinitely — observed during the 2026-05-20 overnight S2 soak
  /// where `/`, `/soak`, and intermediate dirs accumulated 9 stale
  /// entries that `verify` flagged on every cycle but no read path
  /// would heal because reads of `/` and other ancestors bypass the
  /// dir_key index via the cached merkle root.
  ///
  /// Idempotent: an already-correct ancestor is skipped (no write).
  /// Best-effort: walking errors stop the heal without propagating, so
  /// a caller in the read path is never blocked.
  pub(crate) fn heal_stale_dir_keys_along_path(&self, path: &str) -> EngineResult<usize> {
    let _namespace = self.engine.namespace_write_guard()?;
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();
    let normalized = normalize_path(path);

    let head_hash = self.engine.head_hash()?;
    if head_hash.is_empty() || head_hash.iter().all(|&b| b == 0) {
      return Ok(0);
    }

    let segments: Vec<&str> = normalized.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();

    // Walk steps: (ancestor_path, canonical_content_hash_at_that_path).
    // Start with root pointing at HEAD's canonical content.
    let mut walk: Vec<(String, Vec<u8>)> = vec![("/".to_string(), head_hash.clone())];
    let mut current_content_hash = head_hash;
    for (i, segment) in segments.iter().enumerate() {
      let content = match self.engine.get_entry(&current_content_hash)? {
        Some((_h, _k, v)) => v,
        None => break,
      };
      let children = if !content.is_empty() && crate::engine::btree::is_btree_format(&content) {
        crate::engine::btree::btree_list_from_node(&content, self.engine, hash_length, false)?
      } else if content.is_empty() {
        break;
      } else {
        deserialize_child_entries(&content, hash_length, 0)?
      };
      let child = match children.iter().find(|c| c.name == *segment) {
        Some(c) => c,
        None => break,
      };
      if child.entry_type != EntryType::DirectoryIndex.to_u8() {
        break;
      }
      current_content_hash = child.hash.clone();
      let p = format!("/{}", segments[..=i].join("/"));
      walk.push((p, current_content_hash.clone()));
    }

    let mut repaired = 0;
    for (anc_path, canonical) in &walk {
      let anc_key = directory_path_hash(anc_path, &algo)?;
      let stored = match self.engine.get_entry(&anc_key)? {
        Some((_h, _k, v)) => v,
        None => continue,
      };
      // Only repair hard-link entries that have actually diverged.
      if stored.len() == hash_length && stored.as_slice() != canonical.as_slice() {
        self.engine.store_entry(EntryType::DirectoryIndex, &anc_key, canonical)?;
        tracing::info!(
          path = %anc_path,
          stale_target = %hex::encode(&stored),
          canonical_target = %hex::encode(canonical),
          "Auto-repaired stale dir_key during list_directory"
        );
        repaired += 1;
      }
    }
    Ok(repaired)
  }

  /// Repair a stale dir_key by rewriting it to hard-link the canonical
  /// content hash from HEAD's merkle walk. Handles both dead-target
  /// (post-GC) and diverged-target (alive but != HEAD) scenarios.
  /// Returns Ok(true) if a write happened, Ok(false) otherwise.
  pub fn repair_stale_dir_key(&self, path: &str) -> EngineResult<bool> {
    let _namespace = self.engine.namespace_write_guard()?;
    let algo = self.engine.hash_algo();
    let dir_key = directory_path_hash(path, &algo)?;
    // Skip if dir_key isn't a hard-link entry at all.
    let raw = match self.engine.get_entry(&dir_key)? {
      Some(e) => e,
      None => return Ok(false),
    };
    if raw.2.len() != algo.hash_length() {
      return Ok(false);
    }
    let canonical = match self.canonical_directory_content_hash(path)? {
      Some(h) => h,
      None => return Ok(false),
    };
    // Already pointing at HEAD's canonical — nothing to do.
    if canonical == raw.2 {
      return Ok(false);
    }
    self.engine.store_entry(EntryType::DirectoryIndex, &dir_key, &canonical)?;
    tracing::info!(
      path = %path,
      stale_target = %hex::encode(&raw.2),
      canonical_target = %hex::encode(&canonical),
      "Repaired stale dir_key hard-link"
    );
    Ok(true)
  }

  /// If `dir_key` is a hard-link whose target is either dead OR diverged
  /// from HEAD's canonical content reference, try to recover by walking
  /// HEAD's tree from root down to `path`. Returns the recovered
  /// directory content if recovery succeeds, None otherwise (no recovery
  /// needed, or recovery impossible).
  ///
  /// Known failure mode: `snapshot_restore` and `fork_promote` move HEAD
  /// without rewriting dir_keys. After those operations, live dir_keys
  /// can point at content hashes that are either (a) swept by GC if no
  /// snapshot still references them, or (b) STILL alive but no longer
  /// match HEAD's canonical view. Both cases cause `list_directory` to
  /// return stale or no data; both are repaired here.
  pub(crate) fn recover_directory_data_if_stale(
    &self,
    path: &str,
    dir_key: &[u8],
  ) -> EngineResult<Option<(crate::engine::entry_header::EntryHeader, Vec<u8>)>> {
    let hash_length = self.engine.hash_algo().hash_length();

    // Step 1: is this even a hard-link entry? If the value is the actual
    // content, no recovery possible from us — fall through.
    let entry = match self.engine.get_entry(dir_key)? {
      Some(e) => e,
      None => return Ok(None),
    };
    let value = &entry.2;
    if value.len() != hash_length {
      return Ok(None);
    }

    // Step 3: walk from HEAD root down to `path`, using ChildEntry.hash at
    // each level — that's the merkle-authoritative reference.
    let head_hash = self.engine.head_hash()?;
    if head_hash.is_empty() || head_hash.iter().all(|&b| b == 0) {
      return Ok(None);
    }

    let normalized = normalize_path(path);
    let segments: Vec<&str> = normalized.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();

    let mut current_content_hash = head_hash;
    for segment in &segments {
      let (content_header, content) = match self.engine.get_entry(&current_content_hash)? {
        Some((header, _key, value)) => (header, value),
        None => return Ok(None), // tree-level break — can't recover
      };
      if content.is_empty() {
        return Ok(None);
      }
      let child = if crate::engine::btree::is_btree_format(&content) {
        crate::engine::btree::btree_lookup(self.engine, &current_content_hash, segment, hash_length, false)?
      } else {
        let mut found = None;
        Self::visit_bounded_flat_children(&content, hash_length, content_header.entry_version, |child| {
          if child.name == *segment {
            found = Some(child.clone());
            return Ok(false);
          }
          Ok(true)
        })?;
        found
      };

      let child = match child {
        Some(child) => child,
        None => return Ok(None),
      };
      // Only follow if it's a directory
      if child.entry_type != EntryType::DirectoryIndex.to_u8() {
        return Ok(None);
      }
      current_content_hash = child.hash;
    }

    // If dir_key's hard-link target matches HEAD's canonical, nothing to
    // recover — caller falls through to the normal read path.
    if value.as_slice() == current_content_hash.as_slice() {
      return Ok(None);
    }

    // Step 4: read the canonical content. If alive, return it as the
    // recovered value. This handles BOTH:
    //   (a) dir_key target was swept by GC — only canonical is alive.
    //   (b) dir_key target is still alive (preserved by a snapshot) but
    //       no longer matches HEAD — listing would return stale data
    //       compared to the current HEAD view.
    let recovered = self.engine.get_entry(&current_content_hash)?;
    if let Some((header, _k, v)) = recovered {
      tracing::warn!(
        path = %normalized,
        stale_target = %hex::encode(value),
        canonical_target = %hex::encode(&current_content_hash),
        "Directory path-key diverged from HEAD; serving canonical content. \
         Run `aeordb verify --repair` to permanently fix the stale dir_key."
      );
      return Ok(Some((header, v)));
    }

    Ok(None)
  }

  pub(crate) fn read_directory_data(&self, dir_key: &[u8]) -> EngineResult<Option<(crate::engine::entry_header::EntryHeader, Vec<u8>)>> {
    let hash_length = self.engine.hash_algo().hash_length();

    let entry = match self.engine.get_entry(dir_key)? {
      Some(entry) => entry,
      None => return Ok(None),
    };

    let (header, _key, value) = entry;

    // Check if this is a hard link (value == hash_length bytes)
    if value.len() == hash_length {
      let content_key = &value;

      // Check cache first
      if let Some(cached) = self.engine.get_cached_dir_content(content_key)? {
        return Ok(Some((header, cached)));
      }

      // Cache miss — read from WAL
      match self.engine.get_entry(content_key)? {
        Some((_h, _k, content_value)) => {
          self.engine.cache_dir_content(content_key.to_vec(), content_value.clone())?;
          Ok(Some((header, content_value)))
        }
        None => {
          tracing::warn!("Hard link target not found for directory entry");
          Ok(None)
        }
      }
    } else {
      Ok(Some((header, value)))
    }
  }

  /// Maximum directory depth for update_parent_directories iteration.
  /// Prevents unbounded looping on pathologically deep paths.
  const MAX_DIRECTORY_DEPTH: usize = 1000;

  /// Update parent directories after a child is added or modified.
  /// Propagates from the immediate parent up to root, updating HEAD at the end.
  /// For directories with >= BTREE_CONVERSION_THRESHOLD children, uses B-tree
  /// storage for O(log N) insertions instead of rewriting the entire flat list.
  ///
  /// Iterative implementation: walks from the child's parent up to root,
  /// bounded by MAX_DIRECTORY_DEPTH as a safety measure.
  fn update_parent_directories(&self, child_path: &str, child_entry: ChildEntry) -> EngineResult<()> {
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    let mut current_child_path = child_path.to_string();
    let mut current_child_entry = child_entry;
    let mut batch = WriteBatch::new();

    for _depth in 0..Self::MAX_DIRECTORY_DEPTH {
      let parent = match parent_path(&current_child_path) {
        Some(parent) => parent,
        None => {
          // root has no parent
          if !batch.is_empty() {
            self.engine.flush_batch(batch)?;
          }
          return Ok(());
        }
      };

      // Don't propagate system paths (/.aeordb-*) to root — they're accessed
      // directly and listing root would filter them anyway. This prevents
      // system path operations from clobbering a recovered root directory.
      if parent == "/" && v0_is_detached_system_path(&current_child_path) {
        if !batch.is_empty() {
          self.engine.flush_batch(batch)?;
        }
        return Ok(());
      }

      let dir_key = directory_path_hash(&parent, &algo)?;

      // Read existing directory via the HEAD-canonical recovery path first.
      // A crash can leave a mutable dir:{path} hard link behind or ahead of
      // HEAD. Mutating from that stale body would publish a new HEAD that
      // drops committed siblings.
      let existing = match self.recover_directory_data_if_stale(&parent, &dir_key)? {
        Some(pair) => Some(pair),
        None => self.read_directory_data(&dir_key)?,
      };

      let (dir_value, content_key) = match existing {
        Some((_header, value)) if !value.is_empty() && crate::engine::btree::is_btree_format(&value) => {
          // === B-TREE FORMAT ===
          // B-tree nodes are stored synchronously by btree_insert_batched
          let (new_root_hash, new_root_data) =
            crate::engine::btree::btree_insert_batched(self.engine, &value, current_child_entry, hash_length, &algo)?;

          // Cache the B-tree root data for subsequent reads in this propagation
          self.engine.cache_dir_content(new_root_hash.clone(), new_root_data.clone())?;
          (new_root_data, new_root_hash)
        }
        Some((header, value)) => {
          // === FLAT FORMAT ===
          let mut children =
            if value.is_empty() { Vec::new() } else { deserialize_child_entries(&value, hash_length, header.entry_version)? };

          // Add or update the child
          let child_name = &current_child_entry.name;
          if let Some(existing) = children.iter_mut().find(|c| c.name == *child_name) {
            *existing = current_child_entry;
          } else {
            children.push(current_child_entry);
          }

          // Check if we should convert to B-tree
          if children.len() >= crate::engine::btree::BTREE_CONVERSION_THRESHOLD {
            // Convert flat -> B-tree (nodes stored synchronously)
            let root_hash = crate::engine::btree::btree_from_entries(self.engine, children, hash_length, &algo)?;
            let root_entry = self
              .engine
              .get_entry(&root_hash)?
              .ok_or_else(|| EngineError::NotFound("B-tree root not found after conversion".to_string()))?;
            self.engine.cache_dir_content(root_hash.clone(), root_entry.2.clone())?;
            (root_entry.2, root_hash)
          } else {
            // Stay flat — batch the content write
            let dir_value = serialize_child_entries(&children, hash_length)?;
            let content_key = directory_content_hash(&dir_value, &algo)?;
            batch.add(EntryType::DirectoryIndex, content_key.clone(), dir_value.clone());
            self.engine.cache_dir_content(content_key.clone(), dir_value.clone())?;
            (dir_value, content_key)
          }
        }
        None => {
          // New directory (implicitly created for an intermediate parent)
          self.engine.counters().increment_directories();
          let children = vec![current_child_entry];
          let dir_value = serialize_child_entries(&children, hash_length)?;
          let content_key = directory_content_hash(&dir_value, &algo)?;
          batch.add(EntryType::DirectoryIndex, content_key.clone(), dir_value.clone());
          self.engine.cache_dir_content(content_key.clone(), dir_value.clone())?;
          (dir_value, content_key)
        }
      };

      // Hard link at path-based key: store content hash instead of full data
      batch.add(EntryType::DirectoryIndex, dir_key, content_key.clone());

      // If this is root "/", flush the entire batch and update HEAD atomically
      if parent == "/" {
        self.engine.flush_batch_and_update_head(batch, &content_key)?;
        return Ok(());
      }

      // Set up next iteration: update grandparent with this directory as child
      let now_ms = chrono::Utc::now().timestamp_millis();
      current_child_entry = ChildEntry {
        entry_type: EntryType::DirectoryIndex.to_u8(),
        hash: content_key, // content hash for tree walker
        total_size: dir_value.len() as u64,
        created_at: now_ms,
        updated_at: now_ms,
        name: file_name(&parent).unwrap_or("").to_string(),
        content_type: None,
        virtual_time: now_ms as u64,
        node_id: 0,
      };
      current_child_path = parent;
    }

    Err(EngineError::InvalidInput(format!("Directory depth exceeds maximum of {} levels", Self::MAX_DIRECTORY_DEPTH)))
  }

  /// Remove a child entry from its parent directory and propagate up.
  /// Handles both flat and B-tree directory formats.
  fn remove_from_parent_directory(&self, child_path: &str) -> EngineResult<()> {
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    let parent = match parent_path(child_path) {
      Some(parent) => parent,
      None => return Ok(()),
    };

    let dir_key = directory_path_hash(&parent, &algo)?;
    let child_name = file_name(child_path).unwrap_or("").to_string();

    let existing = match self.recover_directory_data_if_stale(&parent, &dir_key)? {
      Some(pair) => Some(pair),
      None => self.read_directory_data(&dir_key)?,
    };

    let mut batch = WriteBatch::new();

    let (dir_value, content_key) = match existing {
      Some((header, value)) if !value.is_empty() && crate::engine::btree::is_btree_format(&value) => {
        // B-tree format: delete from tree
        let root_node = crate::engine::btree::BTreeNode::deserialize(&value, hash_length, header.entry_version)?;
        let root_hash = root_node.content_hash(hash_length, &algo)?;

        match crate::engine::btree::btree_delete(self.engine, &root_hash, &child_name, hash_length, &algo)? {
          Some(new_root_hash) => {
            let new_root_entry = self
              .engine
              .get_entry(&new_root_hash)?
              .ok_or_else(|| EngineError::NotFound("B-tree root not found after delete".to_string()))?;
            // Cache the B-tree root data
            self.engine.cache_dir_content(new_root_hash.clone(), new_root_entry.2.clone())?;
            (new_root_entry.2, new_root_hash)
          }
          None => {
            // Tree is empty -- store empty flat directory
            let dir_value = Vec::new();
            let content_key = directory_content_hash(&dir_value, &algo)?;
            batch.add(EntryType::DirectoryIndex, content_key.clone(), dir_value.clone());
            self.engine.cache_dir_content(content_key.clone(), dir_value.clone())?;
            (dir_value, content_key)
          }
        }
      }
      Some((header, value)) => {
        // Flat format
        let mut children =
          if value.is_empty() { Vec::new() } else { deserialize_child_entries(&value, hash_length, header.entry_version)? };

        children.retain(|c| c.name != child_name);

        let dir_value = serialize_child_entries(&children, hash_length)?;
        let content_key = directory_content_hash(&dir_value, &algo)?;
        batch.add(EntryType::DirectoryIndex, content_key.clone(), dir_value.clone());
        self.engine.cache_dir_content(content_key.clone(), dir_value.clone())?;
        (dir_value, content_key)
      }
      None => {
        let dir_value = Vec::new();
        let content_key = directory_content_hash(&dir_value, &algo)?;
        batch.add(EntryType::DirectoryIndex, content_key.clone(), dir_value.clone());
        self.engine.cache_dir_content(content_key.clone(), dir_value.clone())?;
        (dir_value, content_key)
      }
    };

    // Hard link at path-based key: store content hash instead of full data
    batch.add(EntryType::DirectoryIndex, dir_key, content_key.clone());

    // Propagate up
    if parent == "/" {
      self.engine.flush_batch_and_update_head(batch, &content_key)?;
      return Ok(());
    }

    // Flush batch before calling update_parent_directories (it creates its own batch)
    self.engine.flush_batch(batch)?;

    let del_now = chrono::Utc::now().timestamp_millis();
    let parent_child = ChildEntry {
      entry_type: EntryType::DirectoryIndex.to_u8(),
      hash: content_key, // content hash for tree walker
      total_size: dir_value.len() as u64,
      created_at: del_now,
      updated_at: del_now,
      name: file_name(&parent).unwrap_or("").to_string(),
      content_type: None,
      virtual_time: del_now as u64,
      node_id: 0,
    };

    self.update_parent_directories(&parent, parent_child)
  }

  /// Store a symlink at the given path pointing to the target path.
  /// If a symlink already exists at the path, updates its target (preserving created_at).
  /// Does NOT validate that the target exists.
  pub fn store_symlink(&self, ctx: &RequestContext, path: &str, target: &str) -> EngineResult<SymlinkRecord> {
    // SECURITY: Reject control characters in both path and target BEFORE
    // normalization. JSON deserializes \r\n into actual CR+LF bytes (0x0D, 0x0A)
    // which normalize_path does NOT strip. This prevents CRLF injection and
    // other control character attacks in symlink paths and targets.
    if path.bytes().any(|b| (b < 0x20 && b != 0) || b == 0x7F) {
      return Err(EngineError::InvalidInput("Symlink path contains control characters".to_string()));
    }
    if target.bytes().any(|b| (b < 0x20 && b != 0) || b == 0x7F) {
      return Err(EngineError::InvalidInput("Symlink target contains control characters".to_string()));
    }

    let normalized = normalize_path(path);
    let namespace = self.engine.namespace_write_guard()?;
    let txn = crate::engine::storage_engine::TransactionGuard::new(self.engine)?;
    let normalized_target = normalize_path(target);

    // M15: Reject storing at root path — it would create a ghost entry.
    if normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot store at root path".to_string()));
    }

    // M16: Reject self-referencing symlinks at creation time.
    if normalized == normalized_target {
      return Err(EngineError::InvalidInput(format!("Symlink cannot point to itself: {}", normalized)));
    }

    let algo = self.engine.hash_algo();
    let sys_flags = v0_system_entry_flags(&normalized);

    // Check if symlink already exists (preserve created_at on update)
    let symlink_key = symlink_path_hash(&normalized, &algo)?;
    let existing_created_at = match self.engine.get_entry(&symlink_key)? {
      Some((header, _key, value)) => {
        let existing = SymlinkRecord::deserialize(&value, header.entry_version)?;
        Some(existing.created_at)
      }
      None => None,
    };

    let mut record = SymlinkRecord::new(normalized.clone(), normalized_target);

    // Preserve original created_at on update
    if let Some(original_created_at) = existing_created_at {
      record.created_at = original_created_at;
    }

    let serialized = record.serialize()?;

    // Content-addressed key (immutable — for KV store entry)
    let content_key = symlink_content_hash(&serialized, &algo)?;
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::Symlink, &content_key, &serialized, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::Symlink, &content_key, &serialized)?;
    }

    // Identity hash (for ChildEntry.hash — excludes timestamps)
    let identity_key = symlink_identity_hash(&normalized, &record.target, &algo)?;
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::Symlink, &identity_key, &serialized, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::Symlink, &identity_key, &serialized)?;
    }

    // Path-based key (mutable — for reads/deletion)
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::Symlink, &symlink_key, &serialized, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::Symlink, &symlink_key, &serialized)?;
    }

    // Build child entry for parent directory
    let child = ChildEntry {
      entry_type: EntryType::Symlink.to_u8(),
      hash: identity_key,
      total_size: 0,
      created_at: record.created_at,
      updated_at: record.updated_at,
      name: file_name(&normalized).unwrap_or("").to_string(),
      content_type: None,
      virtual_time: chrono::Utc::now().timestamp_millis() as u64,
      node_id: 0,
    };

    self.update_parent_directories(&normalized, child)?;

    let entry_data = EntryEventData {
      path: normalized,
      entry_type: "symlink".to_string(),
      content_type: None,
      size: 0,
      hash: hex::encode(&record.target),
      created_at: record.created_at,
      updated_at: record.updated_at,
      previous_hash: None,
    };
    txn.commit_after(namespace)?;
    self.engine.counters().record_symlink_write(existing_created_at.is_some());
    ctx.emit(EVENT_ENTRIES_CREATED, serde_json::json!({"entries": [entry_data]}));

    Ok(record)
  }

  /// Read a SymlinkRecord at the given path, or None if not found.
  pub fn get_symlink(&self, path: &str) -> EngineResult<Option<SymlinkRecord>> {
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();

    let symlink_key = symlink_path_hash(&normalized, &algo)?;
    match self.engine.get_entry(&symlink_key)? {
      Some((header, _key, value)) => {
        let record = SymlinkRecord::deserialize(&value, header.entry_version)?;
        Ok(Some(record))
      }
      None => Ok(None),
    }
  }

  /// Delete a symlink at the given path.
  pub fn delete_symlink(&self, ctx: &RequestContext, path: &str) -> EngineResult<()> {
    let namespace = self.engine.namespace_write_guard()?;
    let txn = crate::engine::storage_engine::TransactionGuard::new(self.engine)?;

    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();
    let sys_flags = v0_system_entry_flags(&normalized);

    // Verify symlink exists
    let symlink_key = symlink_path_hash(&normalized, &algo)?;
    let record = match self.engine.get_entry(&symlink_key)? {
      Some((header, _key, value)) => SymlinkRecord::deserialize(&value, header.entry_version)?,
      None => return Err(EngineError::NotFound(normalized)),
    };

    // Store a DeletionRecord
    let deletion = DeletionRecord::new(normalized.clone(), None);
    let deletion_key = deletion_record_hash(&normalized, deletion.deleted_at, &algo)?;
    let deletion_value = deletion.serialize();
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::DeletionRecord, &deletion_key, &deletion_value, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::DeletionRecord, &deletion_key, &deletion_value)?;
    }

    // Mark as deleted in KV store
    self.engine.mark_entry_deleted(&symlink_key)?;

    // Remove from parent directory
    self.remove_from_parent_directory(&normalized)?;

    let entry_data = EntryEventData {
      path: normalized,
      entry_type: "symlink".to_string(),
      content_type: None,
      size: 0,
      hash: hex::encode(&record.target),
      created_at: record.created_at,
      updated_at: record.updated_at,
      previous_hash: None,
    };
    txn.commit_after(namespace)?;
    self.engine.counters().record_symlink_delete();
    ctx.emit(EVENT_ENTRIES_DELETED, serde_json::json!({"entries": [entry_data]}));

    Ok(())
  }

  /// Rename (move) a file from one path to another.
  ///
  /// This is a metadata-only operation — no chunk data is copied.
  /// The file's content (chunk_hashes), content_type, total_size, and
  /// created_at are preserved. Only the path and updated_at change.
  pub fn rename_file(&self, ctx: &RequestContext, old_path: &str, new_path: &str) -> EngineResult<FileRecord> {
    let namespace = self.engine.namespace_write_guard()?;
    let txn = crate::engine::storage_engine::TransactionGuard::new(self.engine)?;

    let old_normalized = normalize_path(old_path);
    let new_normalized = normalize_path(new_path);

    // Reject root paths
    if old_normalized == "/" || new_normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot rename root path".to_string()));
    }

    // Reject same source/destination
    if old_normalized == new_normalized {
      return Err(EngineError::InvalidInput("Source and destination paths are the same".to_string()));
    }

    // Reject cross-system-boundary renames
    let old_is_system = v0_is_detached_system_path(&old_normalized);
    let new_is_system = v0_is_detached_system_path(&new_normalized);
    if old_is_system != new_is_system {
      return Err(EngineError::InvalidInput("Cannot rename across system boundary".to_string()));
    }

    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();
    let sys_flags = v0_system_entry_flags(&new_normalized);

    // Read the source FileRecord
    let old_file_key = file_path_hash(&old_normalized, &algo)?;
    let old_record = match self.engine.get_entry(&old_file_key)? {
      Some((header, _key, value)) => FileRecord::deserialize(&value, hash_length, header.entry_version)?,
      None => return Err(EngineError::NotFound(old_normalized)),
    };

    // Check destination doesn't already exist (file or symlink)
    let new_file_key = file_path_hash(&new_normalized, &algo)?;
    if self.engine.has_entry(&new_file_key)? {
      return Err(EngineError::AlreadyExists(new_normalized));
    }
    let new_symlink_key = symlink_path_hash(&new_normalized, &algo)?;
    if self.engine.has_entry(&new_symlink_key)? {
      return Err(EngineError::AlreadyExists(new_normalized));
    }

    // Create a new FileRecord at the new path, preserving content fields
    let mut new_record =
      FileRecord::new(new_normalized.clone(), old_record.content_type.clone(), old_record.total_size, old_record.chunk_hashes.clone());
    new_record.content_hash = old_record.content_hash.clone();
    ensure_file_record_content_hash(self.engine, &mut new_record)?;
    new_record.created_at = old_record.created_at;

    let keys = materialize_file_record_entries(self.engine, &new_normalized, &mut new_record, sys_flags)?;
    let child = ChildEntry {
      entry_type: EntryType::FileRecord.to_u8(),
      hash: keys.identity_key,
      total_size: new_record.total_size,
      created_at: new_record.created_at,
      updated_at: new_record.updated_at,
      name: file_name(&new_normalized).unwrap_or("").to_string(),
      content_type: new_record.content_type.clone(),
      virtual_time: chrono::Utc::now().timestamp_millis() as u64,
      node_id: 0,
    };
    self.update_parent_directories(&new_normalized, child)?;

    // Delete old path: DeletionRecord + mark deleted + remove from parent
    let deletion = DeletionRecord::new(old_normalized.clone(), None);
    let deletion_key = deletion_record_hash(&old_normalized, deletion.deleted_at, &algo)?;
    let deletion_value = deletion.serialize();
    let old_sys_flags = v0_system_entry_flags(&old_normalized);
    if old_sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::DeletionRecord, &deletion_key, &deletion_value, old_sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::DeletionRecord, &deletion_key, &deletion_value)?;
    }
    self.engine.mark_entry_deleted(&old_file_key)?;
    self.remove_from_parent_directory(&old_normalized)?;

    let deleted_event = EntryEventData {
      path: old_normalized,
      entry_type: "file".to_string(),
      content_type: old_record.content_type.clone(),
      size: old_record.total_size,
      hash: old_record.content_hash_hex(),
      created_at: old_record.created_at,
      updated_at: old_record.updated_at,
      previous_hash: None,
    };
    let created_event = EntryEventData {
      path: new_normalized,
      entry_type: "file".to_string(),
      content_type: new_record.content_type.clone(),
      size: new_record.total_size,
      hash: new_record.content_hash_hex(),
      created_at: new_record.created_at,
      updated_at: new_record.updated_at,
      previous_hash: None,
    };
    txn.commit_after(namespace)?;
    self.engine.counters().record_write(0);
    ctx.emit(EVENT_ENTRIES_DELETED, serde_json::json!({"entries": [deleted_event]}));
    ctx.emit(EVENT_ENTRIES_CREATED, serde_json::json!({"entries": [created_event]}));

    Ok(new_record)
  }

  /// Copy a file to a new path. Reuses existing chunk hashes (no data duplication).
  pub fn copy_file(&self, ctx: &RequestContext, from_path: &str, to_path: &str) -> EngineResult<FileRecord> {
    let namespace = self.engine.namespace_write_guard()?;
    let txn = crate::engine::storage_engine::TransactionGuard::new(self.engine)?;

    let from_normalized = normalize_path(from_path);
    let to_normalized = normalize_path(to_path);

    if from_normalized == "/" || to_normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot copy root path".to_string()));
    }
    if from_normalized == to_normalized {
      return Err(EngineError::InvalidInput("Source and destination are the same".to_string()));
    }
    if v0_is_detached_system_path(&from_normalized) || v0_is_detached_system_path(&to_normalized) {
      return Err(EngineError::InvalidInput("Cannot copy system paths".to_string()));
    }

    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    // Read the source FileRecord
    let from_key = file_path_hash(&from_normalized, &algo)?;
    let source_record = match self.engine.get_entry(&from_key)? {
      Some((header, _key, value)) => FileRecord::deserialize(&value, hash_length, header.entry_version)?,
      None => return Err(EngineError::NotFound(from_normalized)),
    };

    // Use restore_file_from_record which handles all 3 keys + parent dirs
    self.restore_file_from_record(ctx, &to_normalized, &source_record)?;

    // Read back the new record
    let to_key = file_path_hash(&to_normalized, &algo)?;
    let result = match self.engine.get_entry(&to_key)? {
      Some((header, _key, value)) => Ok(FileRecord::deserialize(&value, hash_length, header.entry_version)?),
      None => Err(EngineError::NotFound(to_normalized)),
    };
    txn.finish_after(result, namespace)
  }

  /// Recursively copy a path (file or directory) to a new location.
  pub fn copy_path(&self, ctx: &RequestContext, from_path: &str, to_path: &str) -> EngineResult<Vec<String>> {
    let namespace = self.engine.namespace_write_guard()?;
    let txn = crate::engine::storage_engine::TransactionGuard::new(self.engine)?;

    let from_normalized = normalize_path(from_path);
    let to_normalized = normalize_path(to_path);
    let mut copied = Vec::new();

    // Check if source is a directory
    let algo = self.engine.hash_algo();
    let dir_key = directory_path_hash(&from_normalized, &algo)?;
    if self.engine.has_entry(&dir_key)? {
      // Directory — create destination dir and recurse
      let _ = self.create_directory(ctx, &to_normalized);
      let children = self.list_directory_strict(&from_normalized)?;
      for child in &children {
        let child_from = format!("{}/{}", from_normalized.trim_end_matches('/'), child.name);
        let child_to = format!("{}/{}", to_normalized.trim_end_matches('/'), child.name);
        let sub_copied = self.copy_path(ctx, &child_from, &child_to)?;
        copied.extend(sub_copied);
      }
      txn.commit_after(namespace)?;
      return Ok(copied);
    }

    // File
    self.copy_file(ctx, &from_normalized, &to_normalized)?;
    copied.push(to_normalized);
    txn.commit_after(namespace)?;
    Ok(copied)
  }

  /// Rename (move) a symlink from one path to another.
  ///
  /// This is a metadata-only operation — the symlink's target does NOT change,
  /// only its path. created_at is preserved.
  pub fn rename_symlink(&self, ctx: &RequestContext, old_path: &str, new_path: &str) -> EngineResult<SymlinkRecord> {
    let old_normalized = normalize_path(old_path);
    let namespace = self.engine.namespace_write_guard()?;
    let txn = crate::engine::storage_engine::TransactionGuard::new(self.engine)?;
    let new_normalized = normalize_path(new_path);

    // Reject root paths
    if old_normalized == "/" || new_normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot rename root path".to_string()));
    }

    // Reject same source/destination
    if old_normalized == new_normalized {
      return Err(EngineError::InvalidInput("Source and destination paths are the same".to_string()));
    }

    // Reject cross-system-boundary renames
    let old_is_system = v0_is_detached_system_path(&old_normalized);
    let new_is_system = v0_is_detached_system_path(&new_normalized);
    if old_is_system != new_is_system {
      return Err(EngineError::InvalidInput("Cannot rename across system boundary".to_string()));
    }

    let algo = self.engine.hash_algo();
    let sys_flags = v0_system_entry_flags(&new_normalized);

    // Read the source SymlinkRecord
    let old_symlink_key = symlink_path_hash(&old_normalized, &algo)?;
    let old_record = match self.engine.get_entry(&old_symlink_key)? {
      Some((header, _key, value)) => SymlinkRecord::deserialize(&value, header.entry_version)?,
      None => return Err(EngineError::NotFound(old_normalized)),
    };

    // Check destination doesn't already exist (file or symlink)
    let new_file_key = file_path_hash(&new_normalized, &algo)?;
    if self.engine.has_entry(&new_file_key)? {
      return Err(EngineError::AlreadyExists(new_normalized));
    }
    let new_symlink_key = symlink_path_hash(&new_normalized, &algo)?;
    if self.engine.has_entry(&new_symlink_key)? {
      return Err(EngineError::AlreadyExists(new_normalized));
    }

    // Create new SymlinkRecord at new path with same target, preserving created_at
    let mut new_record = SymlinkRecord::new(new_normalized.clone(), old_record.target.clone());
    new_record.created_at = old_record.created_at;

    let serialized = new_record.serialize()?;

    // Store at content-addressed key
    let content_key = symlink_content_hash(&serialized, &algo)?;
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::Symlink, &content_key, &serialized, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::Symlink, &content_key, &serialized)?;
    }

    // Store at identity hash
    let identity_key = symlink_identity_hash(&new_normalized, &new_record.target, &algo)?;
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::Symlink, &identity_key, &serialized, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::Symlink, &identity_key, &serialized)?;
    }

    // Store at path-based key
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::Symlink, &new_symlink_key, &serialized, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::Symlink, &new_symlink_key, &serialized)?;
    }

    // Build child entry and update parent directories
    let child = ChildEntry {
      entry_type: EntryType::Symlink.to_u8(),
      hash: identity_key,
      total_size: 0,
      created_at: new_record.created_at,
      updated_at: new_record.updated_at,
      name: file_name(&new_normalized).unwrap_or("").to_string(),
      content_type: None,
      virtual_time: chrono::Utc::now().timestamp_millis() as u64,
      node_id: 0,
    };
    self.update_parent_directories(&new_normalized, child)?;

    // Delete old path: DeletionRecord + mark deleted + remove from parent
    let deletion = DeletionRecord::new(old_normalized.clone(), None);
    let deletion_key = deletion_record_hash(&old_normalized, deletion.deleted_at, &algo)?;
    let deletion_value = deletion.serialize();
    let old_sys_flags = v0_system_entry_flags(&old_normalized);
    if old_sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::DeletionRecord, &deletion_key, &deletion_value, old_sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::DeletionRecord, &deletion_key, &deletion_value)?;
    }
    self.engine.mark_entry_deleted(&old_symlink_key)?;
    self.remove_from_parent_directory(&old_normalized)?;

    let deleted_event = EntryEventData {
      path: old_normalized,
      entry_type: "symlink".to_string(),
      content_type: None,
      size: 0,
      hash: hex::encode(&old_record.target),
      created_at: old_record.created_at,
      updated_at: old_record.updated_at,
      previous_hash: None,
    };
    let created_event = EntryEventData {
      path: new_normalized,
      entry_type: "symlink".to_string(),
      content_type: None,
      size: 0,
      hash: hex::encode(&new_record.target),
      created_at: new_record.created_at,
      updated_at: new_record.updated_at,
      previous_hash: None,
    };
    txn.commit_after(namespace)?;
    self.engine.counters().record_write(0);
    ctx.emit(EVENT_ENTRIES_DELETED, serde_json::json!({"entries": [deleted_event]}));
    ctx.emit(EVENT_ENTRIES_CREATED, serde_json::json!({"entries": [created_event]}));

    Ok(new_record)
  }
}

#[cfg(test)]
mod engine_file_stream_tests {
  use super::*;
  use crate::engine::memory_coordinator::{AdmissionClass, CriticalMemoryPurpose, MemoryOwner};
  use crate::engine::request_context::RequestContext;
  use crate::engine::storage_engine::StorageEngine;

  fn create_test_engine() -> (StorageEngine, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("test.aeordb");
    let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
    (engine, temp)
  }

  /// Build a payload big enough to span several 256 KB chunks so we can
  /// verify the stream walks them one at a time.
  fn multi_chunk_payload() -> Vec<u8> {
    // 5 full chunks + a partial = 6 chunks. Use deterministic bytes per
    // chunk so we can identify which chunk we got back.
    let mut data = Vec::with_capacity(DEFAULT_CHUNK_SIZE * 5 + 1024);
    for i in 0..5 {
      data.extend(std::iter::repeat(i as u8).take(DEFAULT_CHUNK_SIZE));
    }
    data.extend(std::iter::repeat(0xFFu8).take(1024));
    data
  }

  #[test]
  fn verification_flat_directory_parser_rejects_oversized_legacy_lists() {
    let entry = ChildEntry {
      entry_type: EntryType::FileRecord.to_u8(),
      hash: vec![0x5a; 32],
      total_size: 1,
      created_at: 1,
      updated_at: 1,
      name: "child".to_string(),
      content_type: None,
      virtual_time: 1,
      node_id: 1,
    };
    let encoded = entry.serialize(32).unwrap();
    let mut oversized = Vec::new();
    for _ in 0..=crate::engine::btree::BTREE_CONVERSION_THRESHOLD {
      oversized.extend_from_slice(&encoded);
    }

    let error = DirectoryOps::visit_bounded_flat_children(&oversized, 32, 0, |_child| Ok(true)).unwrap_err();

    assert!(matches!(error, EngineError::CorruptEntry { reason, .. } if reason.contains("flat directory exceeds")));
  }

  #[test]
  fn lazy_stream_yields_chunks_in_order() {
    let (engine, _temp) = create_test_engine();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    let payload = multi_chunk_payload();
    ops.store_file_buffered(&ctx, "/big.bin", &payload, Some("application/octet-stream")).unwrap();

    let stream = ops.read_file_streaming("/big.bin").unwrap();
    let expected_chunks = (payload.len() + DEFAULT_CHUNK_SIZE - 1) / DEFAULT_CHUNK_SIZE;
    assert_eq!(stream.chunk_count(), expected_chunks);

    let mut assembled = Vec::with_capacity(payload.len());
    for chunk in stream {
      assembled.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(assembled, payload);
  }

  #[test]
  fn constructor_does_no_chunk_io() {
    // With lazy semantics, building the stream is O(1) — no chunk reads
    // happen until next(). Verify by constructing a stream whose chunk
    // hashes are bogus: construction must succeed; iteration must surface
    // the chunk-not-found errors only after next() is called.
    let (engine, _temp) = create_test_engine();
    let bogus_hashes: Vec<Vec<u8>> = (0..4).map(|i| vec![i as u8; 32]).collect();
    let mut stream = EngineFileStream::from_chunk_hashes(bogus_hashes, &engine).unwrap();

    // Constructor returned Ok — no errors surfaced yet.
    // Each next() call surfaces the chunk-not-found error individually.
    for _ in 0..4 {
      let r = stream.next().unwrap();
      assert!(r.is_err(), "expected NotFound for bogus chunk hash");
    }
    assert!(stream.next().is_none(), "stream should be exhausted");
  }

  #[test]
  fn stream_inventory_and_decoded_chunk_reservations_live_until_their_exact_owners_drop() {
    let (engine, _temp) = create_test_engine();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx, "/reserved.bin", &multi_chunk_payload(), Some("application/octet-stream")).unwrap();
    let memory = engine.memory_coordinator();
    let baseline = memory.snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().clone();

    let mut stream = ops.read_file_streaming("/reserved.bin").unwrap();
    let inventory = memory.snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().clone();
    assert_eq!(inventory.active_reservations, baseline.active_reservations + 1);
    assert!(inventory.reserved_bytes > baseline.reserved_bytes);

    let chunk = stream.next_reserved().unwrap().unwrap();
    let decoded = memory.snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().clone();
    assert_eq!(decoded.active_reservations, inventory.active_reservations + 1);
    assert!(decoded.reserved_bytes >= inventory.reserved_bytes + chunk.len() as u64);

    drop(stream);
    let chunk_only = memory.snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().clone();
    assert_eq!(chunk_only.active_reservations, baseline.active_reservations + 1);
    drop(chunk);
    let released = memory.snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().clone();
    assert_eq!(released.active_reservations, baseline.active_reservations);
    assert_eq!(released.reserved_bytes, baseline.reserved_bytes);
  }

  #[test]
  fn stream_pressure_refuses_before_chunk_io_without_latching_storage() {
    let (engine, _temp) = create_test_engine();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx, "/pressure.bin", b"still durable", Some("application/octet-stream")).unwrap();
    let memory = engine.memory_coordinator();
    let snapshot = memory.snapshot().unwrap();
    let policy = snapshot.policy.unwrap();
    let remaining = policy.emergency_reserve_bytes - snapshot.critical_reserved_bytes;
    let pressure =
      memory.reserve(MemoryOwner::StreamingRead, remaining, AdmissionClass::Critical(CriticalMemoryPurpose::StreamingRead)).unwrap();

    let error = match ops.read_file_streaming("/pressure.bin") {
      Ok(_) => panic!("stream inventory admission unexpectedly succeeded"),
      Err(error) => error,
    };
    assert!(matches!(error, EngineError::ResourceExhausted(_)));
    assert!(engine.durability_failure().is_none());

    drop(pressure);
    assert_eq!(ops.read_file_streaming("/pressure.bin").unwrap().collect_to_vec().unwrap(), b"still durable");
  }

  #[test]
  fn compressed_stream_reserves_the_encoded_and_decoded_chunk_lifetimes() {
    let (engine, _temp) = create_test_engine();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    let payload = vec![b'z'; DEFAULT_CHUNK_SIZE * 2];
    ops.store_file_compressed(&ctx, "/compressed.bin", &payload, Some("application/octet-stream"), CompressionAlgorithm::Zstd).unwrap();
    let memory = engine.memory_coordinator();
    let baseline = memory.snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().clone();

    let mut stream = ops.read_file_streaming("/compressed.bin").unwrap();
    let chunk = stream.next_reserved().unwrap().unwrap();
    assert_eq!(chunk.as_ref(), &payload[..DEFAULT_CHUNK_SIZE]);
    let held = memory.snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().clone();
    assert!(held.active_reservations >= baseline.active_reservations + 2, "inventory and decoded chunk must both remain admitted");
    drop(chunk);
    drop(stream);
    let released = memory.snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().clone();
    assert_eq!(released.active_reservations, baseline.active_reservations);
    assert_eq!(released.reserved_bytes, baseline.reserved_bytes);
  }

  #[test]
  fn compressed_stream_rejects_expansion_through_the_bounded_decoder() {
    let (engine, _temp) = create_test_engine();
    let payload = vec![b'z'; DEFAULT_CHUNK_SIZE * 2];
    let chunk_key = chunk_content_hash(&payload, &engine.hash_algo()).unwrap();
    let compressed = compress(&payload, CompressionAlgorithm::Zstd).unwrap();
    engine.store_entry_compressed(EntryType::Chunk, &chunk_key, &compressed, CompressionAlgorithm::Zstd).unwrap();

    let mut stream = EngineFileStream::from_chunk_hashes(vec![chunk_key], &engine).unwrap();
    let error = stream.next().unwrap().unwrap_err();
    assert!(
      error.to_string().contains("exceeds caller bound 262144"),
      "compressed stream must reject expansion in the bounded decoder before collecting it: {error}"
    );
  }

  #[test]
  fn stream_metadata_rejects_a_chunk_header_length_that_disagrees_with_kv() {
    use std::io::{Seek, SeekFrom, Write};

    let (engine, temp) = create_test_engine();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx, "/malformed.bin", b"metadata must agree", Some("application/octet-stream")).unwrap();
    let record = ops.get_metadata("/malformed.bin").unwrap().unwrap();
    let metadata = engine.get_chunk_metadata(&record.chunk_hashes[0]).unwrap().unwrap();
    let original_total_length = metadata.total_length;
    let mut database = std::fs::OpenOptions::new().write(true).open(temp.path().join("test.aeordb")).unwrap();
    database.seek(SeekFrom::Start(metadata.offset + 27)).unwrap();
    database.write_all(&original_total_length.saturating_add(1).to_le_bytes()).unwrap();
    database.flush().unwrap();

    let error = engine.get_chunk_stream_metadata(&record.chunk_hashes[0], false).unwrap_err();

    database.seek(SeekFrom::Start(metadata.offset + 27)).unwrap();
    database.write_all(&original_total_length.to_le_bytes()).unwrap();
    database.flush().unwrap();
    assert!(matches!(error, EngineError::CorruptEntry { offset, .. } if offset == metadata.offset));
  }

  #[test]
  fn owned_arc_stream_can_be_static() {
    // Smoke test for the 'static path used by the HTTP server. The stream
    // must compile and run when the caller passes an Arc<StorageEngine>
    // and the stream outlives the function's borrow scope.
    let (engine, _temp) = create_test_engine();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx, "/x.bin", b"hello world", Some("text/plain")).unwrap();

    // Look up the file_key path -> FileRecord -> chunk_hashes.
    let algo = engine.hash_algo();
    let file_key = file_path_hash("/x.bin", &algo).unwrap();
    let (header, _key, value) = engine.get_entry(&file_key).unwrap().unwrap();
    let record = FileRecord::deserialize(&value, algo.hash_length(), header.entry_version).unwrap();

    let arc = std::sync::Arc::new(engine);
    let stream: EngineFileStream<'static> = EngineFileStream::from_chunk_hashes_owned(record.chunk_hashes, arc).unwrap();
    let collected = stream.collect_to_vec().unwrap();
    assert_eq!(collected, b"hello world");
  }

  #[test]
  fn dirty_recovery_preserves_writer_offset_past_recovered_entries() {
    use std::fs::OpenOptions;

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("dirty.aeordb");
    let path_str = path.to_str().unwrap();

    // Phase 1: clean session — write entries, then drop with shutdown.
    let post_clean_hot_tail: u64;
    {
      let engine = StorageEngine::create(path_str).unwrap();
      let ops = DirectoryOps::new(&engine);
      let ctx = RequestContext::system();
      for i in 0..20 {
        let p = format!("/f{i:02}.txt");
        let v = format!("value-{i}").into_bytes();
        ops.store_file_buffered(&ctx, &p, &v, Some("text/plain")).unwrap();
      }
      // Force a clean flush of everything so the on-disk header is current.
      engine.shutdown().unwrap();
      let mut f = OpenOptions::new().read(true).open(&path).unwrap();
      let (h, _) = crate::engine::file_header::read_active_header(&mut f).unwrap();
      post_clean_hot_tail = h.hot_tail_offset;
    }

    // Phase 2: simulate crash mid-flush. Rewrite the active header with
    // a rolled-back hot_tail_offset, and zero the hot tail bytes there.
    // (We use one of the earlier entry offsets — every entry has a header
    // smaller than the hot tail, so reading at that offset will fail the
    // hot tail magic check and trigger dirty recovery.)
    let rolled_back_offset = {
      let mut f = OpenOptions::new().read(true).write(true).open(&path).unwrap();
      let (mut header, active) = crate::engine::file_header::read_active_header(&mut f).unwrap();
      // Pick a target inside the WAL — kv_block_offset + kv_block_length
      // is the start of the WAL. We use that, which definitely has an
      // entry header at it (not a hot tail), so the hot tail load will fail.
      let target = header.kv_block_offset + header.kv_block_length;
      header.hot_tail_offset = target;
      crate::engine::file_header::write_header_to_inactive_slot(&mut f, &mut header, active).unwrap();
      crate::engine::native_durability::sync_file_data_native(&f).unwrap();
      target
    };
    assert!(
      rolled_back_offset < post_clean_hot_tail,
      "rolled-back offset {} must be earlier than post-shutdown {}",
      rolled_back_offset,
      post_clean_hot_tail
    );

    // Phase 3: open the engine (triggers dirty recovery), then drop it
    // cleanly so the on-disk header reflects post-recovery state. Read
    // the header back: hot_tail_offset must be >= the true WAL end.
    {
      let _engine = StorageEngine::open(path_str).unwrap();
      // drop here flushes + writes header
    }
    let recovered_hot_tail = {
      let mut f = OpenOptions::new().read(true).open(&path).unwrap();
      let (h, _) = crate::engine::file_header::read_active_header(&mut f).unwrap();
      h.hot_tail_offset
    };
    assert!(
      recovered_hot_tail >= post_clean_hot_tail,
      "post-recovery hot_tail_offset {} should be >= true WAL end {}",
      recovered_hot_tail,
      post_clean_hot_tail
    );

    // Phase 4: open again, write a NEW entry, drop. The new entry must
    // land at-or-past recovered_hot_tail — otherwise the append landed
    // at rolled_back_offset and overwrote recovered data. Check by
    // re-reading header.
    {
      let engine = StorageEngine::open(path_str).unwrap();
      let ops = DirectoryOps::new(&engine);
      let ctx = RequestContext::system();
      ops.store_file_buffered(&ctx, "/post-recovery.txt", b"new", None).unwrap();
    }
    let post_new_write_hot_tail = {
      let mut f = OpenOptions::new().read(true).open(&path).unwrap();
      let (h, _) = crate::engine::file_header::read_active_header(&mut f).unwrap();
      h.hot_tail_offset
    };
    assert!(
      post_new_write_hot_tail > recovered_hot_tail,
      "after a post-recovery write, hot_tail_offset {} must be > recovered {}",
      post_new_write_hot_tail,
      recovered_hot_tail
    );

    // Phase 5: reopen, confirm /post-recovery.txt is present and intact.
    let engine = StorageEngine::open(path_str).unwrap();
    let ops = DirectoryOps::new(&engine);

    // Phase 5: all previously-stored files should still be readable, AND
    // the post-recovery write must be intact (not overwritten by anything).
    for i in 0..20 {
      let p = format!("/f{i:02}.txt");
      let stream = ops.read_file_streaming(&p).expect("recovered file should be readable");
      let bytes = stream.collect_to_vec().expect("read should succeed");
      assert_eq!(bytes, format!("value-{i}").into_bytes(), "recovered file {} content", p);
    }
    let post_stream = ops.read_file_streaming("/post-recovery.txt").expect("post-recovery file readable");
    let post_bytes = post_stream.collect_to_vec().expect("read should succeed");
    assert_eq!(post_bytes, b"new");
  }

  #[test]
  fn size_hint_decrements_with_progress() {
    let (engine, _temp) = create_test_engine();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    let payload = multi_chunk_payload();
    ops.store_file_buffered(&ctx, "/bigger.bin", &payload, None).unwrap();
    let mut stream = ops.read_file_streaming("/bigger.bin").unwrap();
    let total = stream.chunk_count();
    let (lo, hi) = stream.size_hint();
    assert_eq!(lo, total);
    assert_eq!(hi, Some(total));
    let _ = stream.next().unwrap();
    let (lo2, hi2) = stream.size_hint();
    assert_eq!(lo2, total - 1);
    assert_eq!(hi2, Some(total - 1));
  }
}
