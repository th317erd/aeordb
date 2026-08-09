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
use crate::engine::index_store::{IndexWriteBuffer, IndexWriteBufferOptions, DEFAULT_INDEX_BUFFER_FLUSH_WRITES};
use crate::engine::indexing_pipeline::IndexingPipeline;
use crate::engine::symlink_record::{SymlinkRecord, symlink_path_hash, symlink_content_hash};
use crate::engine::index_config_resolver::IndexConfigResolver;
use crate::engine::merge_patch::{apply_merge_patch, MergeDepth};
use crate::engine::merge::{ConflictEntry, MergeOp};
use crate::engine::memory_coordinator::{AdmissionClass, CriticalMemoryPurpose, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::namespace_mutation::{
  NamespaceMutationAcknowledgement, NamespaceMutationBatch, NamespaceMutationCoordinator, NamespaceMutationFanout, NamespaceMutationKind,
  NamespaceMutationSourceIdentity,
};
use crate::engine::operation_memory::OperationMemoryBudget;
use crate::engine::engine_event::{EntryEventData, EVENT_ENTRIES_CREATED, EVENT_ENTRIES_DELETED};
use crate::engine::path_utils::{file_name, normalize_path, parent_path};
use crate::engine::request_context::RequestContext;
use crate::engine::rss_sampler::PhaseSampler;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::system_family_policy::GenericDataPathSelection;
use crate::engine::traversal::{TraversalIntegrity, VisitorCompletion};
use crate::engine::v4::control_store::V3ControlPublicationContextV0;
use crate::engine::v4::system_control::SystemControlSlotV1;
use crate::engine::v4::system_family::SystemFamilyClassificationV1;
use crate::engine::SystemFamilyPolicyResolver;

/// Default chunk size for splitting file data (256 KB).
pub const DEFAULT_CHUNK_SIZE: usize = 262_144;
const SYSTEM_FILE_ALIAS_RECORD_MAX_BYTES: u32 = 1024 * 1024;
const SYSTEM_FILE_ALIAS_WORKSPACE_BYTES: u64 = SYSTEM_FILE_ALIAS_RECORD_MAX_BYTES as u64 * 8;
const DIRECTORY_REPAIR_FAILURE_PATH_BYTES: usize = 768;
const DIRECTORY_REPAIR_FAILURE_ERROR_BYTES: usize = 512;

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

#[derive(Clone, Debug)]
struct CurrentEntryReference {
  hash: Vec<u8>,
  entry_type: EntryType,
  root_selected: bool,
}

/// Immutable remote version retained solely so acknowledged conflict evidence
/// can be resolved later. These records never publish a path locator or child.
#[derive(Clone, Debug)]
pub(crate) enum SyncImmutableVersion {
  File { identity_hash: Vec<u8>, record: FileRecord },
  Symlink { identity_hash: Vec<u8>, record: SymlinkRecord },
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

#[cfg(test)]
std::thread_local! {
  static DIRECTORY_REPAIR_TEST_FAILURE_AFTER_ACKNOWLEDGEMENTS: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
struct DirectoryRepairTestFaultGuard {
  previous: Option<usize>,
}

#[cfg(test)]
impl DirectoryRepairTestFaultGuard {
  fn fail_after_acknowledgements(completed: usize) -> Self {
    let previous = DIRECTORY_REPAIR_TEST_FAILURE_AFTER_ACKNOWLEDGEMENTS.with(|value| value.replace(Some(completed)));
    Self { previous }
  }
}

#[cfg(test)]
impl Drop for DirectoryRepairTestFaultGuard {
  fn drop(&mut self) {
    DIRECTORY_REPAIR_TEST_FAILURE_AFTER_ACKNOWLEDGEMENTS.with(|value| value.set(self.previous));
  }
}

fn bounded_directory_repair_text(value: &str, maximum_bytes: usize) -> String {
  if value.len() <= maximum_bytes {
    return value.to_string();
  }

  let mut boundary = maximum_bytes.saturating_sub(3);
  while boundary > 0 && !value.is_char_boundary(boundary) {
    boundary -= 1;
  }
  format!("{}...", &value[..boundary])
}

fn directory_repair_failure(error: EngineError, completed: usize, phase: &'static str, path: &str) -> EngineError {
  if completed == 0 || matches!(&error, EngineError::PartialOperation { operation, .. } if operation == "directory tree repair") {
    return error;
  }

  let path = bounded_directory_repair_text(path, DIRECTORY_REPAIR_FAILURE_PATH_BYTES);
  let error = bounded_directory_repair_text(&error.to_string(), DIRECTORY_REPAIR_FAILURE_ERROR_BYTES);
  EngineError::PartialOperation {
    operation: "directory tree repair".to_string(),
    completed,
    failed: 1,
    evidence: format!("directories_written={completed}; phase={phase}; path={path}; error={error}"),
  }
}

#[cfg(test)]
fn inject_directory_repair_failure(completed: usize) -> EngineResult<()> {
  let should_fail = DIRECTORY_REPAIR_TEST_FAILURE_AFTER_ACKNOWLEDGEMENTS.with(|value| value.get() == Some(completed));
  if should_fail {
    return Err(EngineError::InvalidInput("injected full-directory repair failure after acknowledgement".to_string()));
  }
  Ok(())
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

pub(crate) fn validate_existing_chunk_locator(engine: &StorageEngine, owner: &str, chunk_hash: &[u8]) -> EngineResult<bool> {
  let hash_length = engine.hash_algo().hash_length();
  if chunk_hash.len() != hash_length {
    return Err(EngineError::InvalidInput(format!(
      "{} references a chunk hash with length {}, expected {}",
      owner,
      chunk_hash.len(),
      hash_length,
    )));
  }
  match engine.get_chunk_metadata(chunk_hash) {
    Ok(Some(_)) => Ok(true),
    Ok(None) => Ok(false),
    Err(EngineError::InvalidInput(_)) => {
      let offset = engine.get_kv_entry(chunk_hash)?.map_or(0, |entry| entry.offset);
      Err(EngineError::CorruptEntry { offset, reason: format!("{} references a non-chunk entry {}", owner, hex::encode(chunk_hash)) })
    }
    Err(error) => Err(error),
  }
}

fn validate_existing_file_chunks(engine: &StorageEngine, path: &str, chunk_hashes: &[Vec<u8>]) -> EngineResult<()> {
  let owner = format!("file '{path}'");
  for chunk_hash in chunk_hashes {
    if !validate_existing_chunk_locator(engine, &owner, chunk_hash)? {
      return Err(EngineError::CorruptEntry { offset: 0, reason: format!("{owner} references missing chunk {}", hex::encode(chunk_hash)) });
    }
  }
  Ok(())
}

fn file_records_are_identical_aliases(left: &FileRecord, right: &FileRecord) -> bool {
  left.content_type == right.content_type
    && left.total_size == right.total_size
    && left.created_at == right.created_at
    && left.updated_at == right.updated_at
    && left.metadata == right.metadata
    && left.content_hash == right.content_hash
    && left.chunk_hashes == right.chunk_hashes
}

fn validate_sync_file_record(engine: &StorageEngine, claimed_hash: &[u8], record: &FileRecord) -> EngineResult<()> {
  let algorithm = engine.hash_algo();
  let hash_length = algorithm.hash_length();
  if claimed_hash.len() != hash_length {
    return Err(EngineError::InvalidInput(format!(
      "sync FileRecord hash length {} does not match expected length {hash_length}",
      claimed_hash.len()
    )));
  }
  if normalize_path(&record.path) != record.path {
    return Err(EngineError::InvalidInput(format!("sync FileRecord path '{}' is not canonical", record.path)));
  }

  let mut hasher = algorithm.incremental_hasher()?;
  let mut total_size = 0u64;
  for chunk_hash in &record.chunk_hashes {
    let chunk = match read_chunk_reserved(engine, chunk_hash, false) {
      Ok(chunk) => chunk,
      Err(EngineError::NotFound(_)) => {
        return Err(EngineError::NotFound(format!("Missing chunk during sync merge for '{}': {}", record.path, hex::encode(chunk_hash))))
      }
      Err(error) => return Err(error),
    };
    if chunk_content_hash(chunk.as_ref(), &algorithm)? != *chunk_hash {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("sync FileRecord '{}' references a chunk stored under a noncanonical key", record.path),
      });
    }
    total_size = total_size
      .checked_add(chunk.len() as u64)
      .ok_or_else(|| EngineError::ResourceExhausted("sync FileRecord size overflow".to_string()))?;
    hasher.update(chunk.as_ref());
  }
  if total_size != record.total_size {
    return Err(EngineError::CorruptEntry {
      offset: 0,
      reason: format!("sync FileRecord '{}' declares {} bytes but its chunks contain {total_size}", record.path, record.total_size),
    });
  }
  let computed_content_hash = hasher.finalize();
  if !record.content_hash.is_empty() && record.content_hash != computed_content_hash {
    return Err(EngineError::CorruptEntry {
      offset: 0,
      reason: format!("sync FileRecord '{}' whole-file hash does not match its chunks", record.path),
    });
  }

  let identity_hash = file_identity_hash(&record.path, record.content_type.as_deref(), &record.chunk_hashes, &algorithm)?;
  let mut canonical_record = record.clone();
  canonical_record.content_hash = computed_content_hash;
  let serialized_hash = file_content_hash(&canonical_record.serialize(hash_length)?, &algorithm)?;
  if claimed_hash != identity_hash && claimed_hash != serialized_hash {
    return Err(EngineError::CorruptEntry {
      offset: 0,
      reason: format!("sync FileRecord '{}' claimed hash is neither its identity nor serialized-content key", record.path),
    });
  }
  Ok(())
}

fn validate_sync_symlink(algorithm: &HashAlgorithm, hash_length: usize, claimed_hash: &[u8], record: &SymlinkRecord) -> EngineResult<()> {
  if claimed_hash.len() != hash_length {
    return Err(EngineError::InvalidInput(format!(
      "sync symlink hash length {} does not match expected length {hash_length}",
      claimed_hash.len()
    )));
  }
  if normalize_path(&record.path) != record.path {
    return Err(EngineError::InvalidInput(format!("sync symlink path '{}' is not canonical", record.path)));
  }
  let identity_hash = symlink_identity_hash(&record.path, &normalize_path(&record.target), algorithm)?;
  let content_hash = symlink_content_hash(&record.serialize()?, algorithm)?;
  if claimed_hash != identity_hash && claimed_hash != content_hash {
    return Err(EngineError::CorruptEntry {
      offset: 0,
      reason: format!("sync symlink '{}' claimed hash is neither its identity nor serialized-content key", record.path),
    });
  }
  Ok(())
}

#[derive(Debug)]
struct PreparedFileRecordEntries {
  content_key: Vec<u8>,
  identity_key: Vec<u8>,
  file_key: Vec<u8>,
  file_value: Vec<u8>,
  flags: u8,
  entry_version: u8,
}

fn prepare_file_record_entries_at_version(
  engine: &StorageEngine,
  normalized_path: &str,
  record: &mut FileRecord,
  flags: u8,
  entry_version: u8,
) -> EngineResult<PreparedFileRecordEntries> {
  let algo = engine.hash_algo();
  let hash_length = algo.hash_length();
  ensure_file_record_content_hash(engine, record)?;
  let file_value = record.serialize_for_version(hash_length, entry_version)?;
  Ok(PreparedFileRecordEntries {
    content_key: file_content_hash(&file_value, &algo)?,
    identity_key: file_identity_hash(normalized_path, record.content_type.as_deref(), &record.chunk_hashes, &algo)?,
    file_key: file_path_hash(normalized_path, &algo)?,
    file_value,
    flags,
    entry_version,
  })
}

fn add_prepared_file_record_entries(batch: &mut NamespaceMutationBatch, prepared: &PreparedFileRecordEntries) -> EngineResult<()> {
  batch.store_dependency_with_version(
    EntryType::FileRecord,
    prepared.content_key.clone(),
    prepared.file_value.clone(),
    prepared.flags,
    prepared.entry_version,
  )?;
  batch.store_dependency_with_version(
    EntryType::FileRecord,
    prepared.identity_key.clone(),
    prepared.file_value.clone(),
    prepared.flags,
    prepared.entry_version,
  )?;
  batch.replace_locator_with_version(
    EntryType::FileRecord,
    prepared.file_key.clone(),
    prepared.file_value.clone(),
    prepared.flags,
    prepared.entry_version,
  )
}

#[derive(Debug, Clone)]
pub(crate) struct FileRecordPublishInput {
  pub normalized_path: String,
  pub content_type: Option<String>,
  pub total_size: u64,
  pub metadata: Vec<u8>,
  pub chunk_hashes: Vec<Vec<u8>>,
  pub content_hash: Vec<u8>,
  pub flags: u8,
  pub created_at_override: Option<i64>,
  pub updated_at_override: Option<i64>,
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

#[derive(Debug, Clone)]
pub(crate) struct BatchFilePublicationInput {
  pub publication: FileRecordPublishInput,
  pub throughput_bytes: u64,
}

pub(crate) enum BufferedFileTransform<T> {
  Keep(T),
  Replace { data: Vec<u8>, output: T },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SystemFileAliasMigrationOutcome {
  SourceMissing,
  Moved,
  IdenticalAliasRetired,
}

#[derive(Debug)]
struct PreparedFileRecordPublication {
  entries: PreparedFileRecordEntries,
  result: FileRecordPublishResult,
  previous_identity: Option<Vec<u8>>,
}

fn prepare_buffered_file_publication(
  engine: &StorageEngine,
  normalized_path: String,
  data: &[u8],
  content_type: String,
  flags: u8,
  chunk_owner: &str,
  chunk_dependencies: &mut std::collections::BTreeMap<Vec<u8>, (Vec<u8>, u8)>,
  counter_delta: &mut DirectoryMutationCounterDelta,
) -> EngineResult<PreparedFileRecordPublication> {
  let algorithm = engine.hash_algo();
  let mut chunk_hashes = Vec::new();
  for chunk_data in data.chunks(DEFAULT_CHUNK_SIZE) {
    let chunk_key = chunk_content_hash(chunk_data, &algorithm)?;
    if validate_existing_chunk_locator(engine, chunk_owner, &chunk_key)? {
      counter_delta.chunks_deduped = counter_delta
        .chunks_deduped
        .checked_add(1)
        .ok_or_else(|| EngineError::ResourceExhausted("buffered publication deduplicated chunk counter overflow".to_string()))?;
    } else if let Some((existing_data, existing_flags)) = chunk_dependencies.get_mut(&chunk_key) {
      if existing_data.as_slice() != chunk_data {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("buffered publication chunk hash collision at {}", hex::encode(&chunk_key)),
        });
      }
      *existing_flags |= flags;
      counter_delta.chunks_deduped = counter_delta
        .chunks_deduped
        .checked_add(1)
        .ok_or_else(|| EngineError::ResourceExhausted("buffered publication deduplicated chunk counter overflow".to_string()))?;
    } else {
      chunk_dependencies.insert(chunk_key.clone(), (chunk_data.to_vec(), flags));
      counter_delta.chunk_stored_sizes.push(chunk_data.len() as u64);
    }
    chunk_hashes.push(chunk_key);
  }

  prepare_file_record_publication_at_version(
    engine,
    FileRecordPublishInput {
      normalized_path,
      content_type: Some(content_type),
      total_size: data.len() as u64,
      metadata: Vec::new(),
      chunk_hashes,
      content_hash: whole_file_content_hash(data, &algorithm)?,
      flags,
      created_at_override: None,
      updated_at_override: None,
      prefer_existing_created_at: true,
    },
    CURRENT_FILE_RECORD_VERSION,
  )
}

enum FileRecordRestoreSource {
  Historical(FileRecord),
  DeletedLocator,
}

fn prepare_file_record_publication_at_version(
  engine: &StorageEngine,
  input: FileRecordPublishInput,
  entry_version: u8,
) -> EngineResult<PreparedFileRecordPublication> {
  let normalized = normalize_path(&input.normalized_path);
  let current = DirectoryOps::new(engine).resolve_current_file_record_from(engine, &normalized)?;
  let (existing_created_at, existing_total_size, previous_identity) = match current {
    Some((identity, existing)) => (Some(existing.created_at), Some(existing.total_size), Some(identity)),
    None => (None, None, None),
  };

  let mut file_record = FileRecord::new(normalized.clone(), input.content_type.clone(), input.total_size, input.chunk_hashes);
  file_record.metadata = input.metadata;
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
  if let Some(updated_at_override) = input.updated_at_override {
    file_record.updated_at = updated_at_override;
  }

  let entries = prepare_file_record_entries_at_version(engine, &normalized, &mut file_record, input.flags, entry_version)?;
  let content_type = file_record.content_type.clone();
  let child_entry = ChildEntry {
    entry_type: EntryType::FileRecord.to_u8(),
    hash: entries.identity_key.clone(),
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

  Ok(PreparedFileRecordPublication {
    entries,
    result: FileRecordPublishResult { normalized_path: normalized, file_record, child_entry, event_entry, existing_total_size },
    previous_identity,
  })
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

#[derive(Debug)]
enum DirectoryMutationCounterEffect {
  None,
  FileWrite { previous_size: Option<u64>, new_size: u64, throughput_bytes: u64 },
  DirectoryCreate,
  DirectoryDelete,
  SymlinkWrite { existed: bool },
  SymlinkDelete,
  Aggregate(DirectoryMutationCounterDelta),
}

#[derive(Debug, Default)]
struct DirectoryMutationCounterDelta {
  throughput_bytes: u64,
  file_writes: Vec<(Option<u64>, u64)>,
  file_delete_sizes: Vec<u64>,
  chunk_stored_sizes: Vec<u64>,
  chunks_deduped: u64,
  directories_created: u64,
  directories_deleted: u64,
  symlinks_created: u64,
  symlinks_deleted: u64,
}

#[derive(Debug)]
struct DirectoryMutationEffects {
  counter: DirectoryMutationCounterEffect,
  implicit_directories: u64,
  cache_writes: Vec<(Vec<u8>, Vec<u8>)>,
  events: Vec<(&'static str, serde_json::Value)>,
  metadata_index_removal_paths: Vec<String>,
  metadata_index_paths: Vec<String>,
}

impl DirectoryMutationEffects {
  fn new(counter: DirectoryMutationCounterEffect) -> Self {
    Self {
      counter,
      implicit_directories: 0,
      cache_writes: Vec::new(),
      events: Vec::new(),
      metadata_index_removal_paths: Vec::new(),
      metadata_index_paths: Vec::new(),
    }
  }
}

#[derive(Debug, Default)]
struct PlannedDirectoryDelta {
  ensure_exists: bool,
  upserts: std::collections::BTreeMap<String, ChildEntry>,
  removals: std::collections::BTreeSet<String>,
}

#[derive(Debug, Default)]
struct DirectoryMutationPlanner {
  deltas: std::collections::BTreeMap<String, PlannedDirectoryDelta>,
  dependencies: std::collections::BTreeMap<Vec<u8>, Vec<u8>>,
}

#[derive(Debug)]
enum PreparedCopyKind {
  File(FileRecord),
  Directory,
  Symlink(SymlinkRecord),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CopySourceConstraint {
  Any,
  FileOnly,
}

#[derive(Debug)]
struct PreparedCopyEntry {
  source_path: String,
  destination_path: String,
  source_identity: Vec<u8>,
  kind: PreparedCopyKind,
}

#[derive(Debug)]
struct CopyPublicationResult {
  copied_paths: Vec<String>,
  file_records: std::collections::BTreeMap<String, FileRecord>,
}

impl DirectoryMutationPlanner {
  fn ensure_directory(&mut self, path: &str) -> EngineResult<()> {
    let normalized = normalize_path(path);
    if normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot create root directory through a directory delta".to_string()));
    }
    self.deltas.entry(normalized).or_default().ensure_exists = true;
    Ok(())
  }

  fn upsert_child(&mut self, child_path: &str, child_entry: ChildEntry) -> EngineResult<()> {
    let normalized = normalize_path(child_path);
    let expected_name = file_name(&normalized).unwrap_or("");
    if expected_name.is_empty() || child_entry.name != expected_name {
      return Err(EngineError::InvalidInput(format!(
        "Directory child name '{}' does not match canonical path '{}'",
        child_entry.name, normalized
      )));
    }
    let parent =
      parent_path(&normalized).ok_or_else(|| EngineError::InvalidInput(format!("Path '{}' has no parent directory", normalized)))?;
    let delta = self.deltas.entry(parent).or_default();
    if delta.removals.contains(expected_name) {
      return Err(EngineError::InvalidInput(format!("Directory delta both removes and upserts '{}'", normalized)));
    }
    if delta.upserts.insert(expected_name.to_string(), child_entry).is_some() {
      return Err(EngineError::InvalidInput(format!("Directory delta contains duplicate destination '{}'", normalized)));
    }
    Ok(())
  }

  fn remove_child(&mut self, child_path: &str) -> EngineResult<()> {
    let normalized = normalize_path(child_path);
    let child_name = file_name(&normalized).unwrap_or("");
    if child_name.is_empty() {
      return Err(EngineError::InvalidInput(format!("Path '{}' has no removable child name", normalized)));
    }
    let parent =
      parent_path(&normalized).ok_or_else(|| EngineError::InvalidInput(format!("Path '{}' has no parent directory", normalized)))?;
    let delta = self.deltas.entry(parent).or_default();
    if delta.upserts.contains_key(child_name) {
      return Err(EngineError::InvalidInput(format!("Directory delta both upserts and removes '{}'", normalized)));
    }
    if !delta.removals.insert(child_name.to_string()) {
      return Err(EngineError::InvalidInput(format!("Directory delta contains duplicate removal '{}'", normalized)));
    }
    Ok(())
  }

  fn add_dependency(&mut self, key: Vec<u8>, value: Vec<u8>) -> EngineResult<()> {
    if let Some(existing) = self.dependencies.get(&key) {
      if existing != &value {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("planned directory dependency {} has conflicting bytes", hex::encode(key)),
        });
      }
      return Ok(());
    }
    self.dependencies.insert(key, value);
    Ok(())
  }

  fn finalize(
    mut self,
    operations: &DirectoryOps<'_>,
    batch: &mut NamespaceMutationBatch,
    effects: &mut DirectoryMutationEffects,
  ) -> EngineResult<()> {
    let algorithm = operations.engine.hash_algo();
    let hash_length = algorithm.hash_length();

    while !self.deltas.is_empty() {
      let path = self
        .deltas
        .keys()
        .max_by(|left, right| directory_depth(left).cmp(&directory_depth(right)).then_with(|| left.cmp(right)))
        .cloned()
        .ok_or_else(|| EngineError::CorruptEntry { offset: 0, reason: "directory delta queue became empty unexpectedly".to_string() })?;
      let delta = self
        .deltas
        .remove(&path)
        .ok_or_else(|| EngineError::CorruptEntry { offset: 0, reason: format!("directory delta disappeared for '{path}'") })?;
      let directory_key = directory_path_hash(&path, &algorithm)?;
      if let Some(reference) = operations.current_entry_reference_from(operations.engine, &path)? {
        if reference.entry_type != EntryType::DirectoryIndex {
          return Err(EngineError::AlreadyExists(path));
        }
      }
      let existing =
        operations.resolve_current_directory_data_from(operations.engine, &path)?.map(|(_identity, header, value)| (header, value));
      let directory_was_missing = existing.is_none();
      if delta.ensure_exists && !directory_was_missing {
        return Err(EngineError::AlreadyExists(path));
      }

      let (directory_data, content_key) = match existing {
        Some((header, value)) if !value.is_empty() && crate::engine::btree::is_btree_format(&value) => {
          let root = crate::engine::btree::BTreeNode::deserialize(&value, hash_length, header.entry_version)?;
          let root_hash = root.content_hash(hash_length, &algorithm)?;
          for name in &delta.removals {
            if crate::engine::btree::btree_lookup(operations.engine, &root_hash, name, hash_length, false)?.is_none() {
              return Err(EngineError::CorruptEntry {
                offset: 0,
                reason: format!("directory '{}' does not contain child '{}' selected for removal", path, name),
              });
            }
          }
          for entry in delta.upserts.values() {
            if let Some(existing_child) =
              crate::engine::btree::btree_lookup(operations.engine, &root_hash, &entry.name, hash_length, false)?
            {
              if existing_child.entry_type != entry.entry_type {
                return Err(EngineError::AlreadyExists(format!("{}/{}", path.trim_end_matches('/'), entry.name)));
              }
            }
          }
          if delta.removals.is_empty() && delta.upserts.is_empty() {
            (value, root_hash)
          } else {
            let mutation = crate::engine::btree::BTreeMutationDelta {
              removals: delta.removals.into_iter().collect(),
              upserts: delta.upserts.into_values().collect(),
            };
            match crate::engine::btree::btree_plan_apply(operations.engine, &value, mutation, hash_length, &algorithm)? {
              Some(plan) => {
                for write in plan.node_writes() {
                  self.add_dependency(write.key.clone(), write.value.clone())?;
                }
                (plan.root_data().to_vec(), plan.root_hash().to_vec())
              }
              None => {
                let data = Vec::new();
                let key = directory_content_hash(&data, &algorithm)?;
                self.add_dependency(key.clone(), data.clone())?;
                (data, key)
              }
            }
          }
        }
        Some((header, value)) => {
          let children = if value.is_empty() { Vec::new() } else { deserialize_child_entries(&value, hash_length, header.entry_version)? };
          let mut by_name = std::collections::BTreeMap::new();
          for child in children {
            let child_name = child.name.clone();
            if by_name.insert(child_name.clone(), child).is_some() {
              return Err(EngineError::CorruptEntry {
                offset: 0,
                reason: format!("directory '{}' contains duplicate child '{}'", path, child_name),
              });
            }
          }
          for name in delta.removals {
            if by_name.remove(&name).is_none() {
              return Err(EngineError::CorruptEntry {
                offset: 0,
                reason: format!("directory '{}' does not contain child '{}' selected for removal", path, name),
              });
            }
          }
          for (name, entry) in delta.upserts {
            if by_name.get(&name).is_some_and(|existing_child| existing_child.entry_type != entry.entry_type) {
              return Err(EngineError::AlreadyExists(format!("{}/{}", path.trim_end_matches('/'), name)));
            }
            by_name.insert(name, entry);
          }
          let children: Vec<_> = by_name.into_values().collect();
          if children.len() >= crate::engine::btree::BTREE_CONVERSION_THRESHOLD {
            let plan = crate::engine::btree::btree_plan_from_entries(children, hash_length, &algorithm)?;
            for write in plan.node_writes() {
              self.add_dependency(write.key.clone(), write.value.clone())?;
            }
            (plan.root_data().to_vec(), plan.root_hash().to_vec())
          } else {
            let data = serialize_child_entries(&children, hash_length)?;
            let key = directory_content_hash(&data, &algorithm)?;
            self.add_dependency(key.clone(), data.clone())?;
            (data, key)
          }
        }
        None => {
          if !delta.removals.is_empty() {
            return Err(EngineError::CorruptEntry {
              offset: 0,
              reason: format!("missing directory '{}' cannot satisfy child removals", path),
            });
          }
          let children: Vec<_> = delta.upserts.into_values().collect();
          if children.len() >= crate::engine::btree::BTREE_CONVERSION_THRESHOLD {
            let plan = crate::engine::btree::btree_plan_from_entries(children, hash_length, &algorithm)?;
            for write in plan.node_writes() {
              self.add_dependency(write.key.clone(), write.value.clone())?;
            }
            (plan.root_data().to_vec(), plan.root_hash().to_vec())
          } else {
            let data = serialize_child_entries(&children, hash_length)?;
            let key = directory_content_hash(&data, &algorithm)?;
            self.add_dependency(key.clone(), data.clone())?;
            (data, key)
          }
        }
      };

      if directory_was_missing {
        effects.implicit_directories = effects
          .implicit_directories
          .checked_add(1)
          .ok_or_else(|| EngineError::ResourceExhausted("implicit directory count overflow".to_string()))?;
      }
      effects.cache_writes.push((content_key.clone(), directory_data.clone()));
      batch.replace_locator(EntryType::DirectoryIndex, directory_key, content_key.clone(), 0)?;
      if path == "/" {
        batch.set_incremental_head_hash(content_key);
        continue;
      }

      let parent = parent_path(&path).ok_or_else(|| EngineError::InvalidInput(format!("Directory '{}' has no parent", path)))?;
      if parent == "/" && v0_is_detached_system_path(&path) {
        continue;
      }
      let now = chrono::Utc::now().timestamp_millis();
      self.upsert_child(
        &path,
        ChildEntry {
          entry_type: EntryType::DirectoryIndex.to_u8(),
          hash: content_key,
          total_size: directory_data.len() as u64,
          created_at: now,
          updated_at: now,
          name: file_name(&path).unwrap_or("").to_string(),
          content_type: None,
          virtual_time: now as u64,
          node_id: 0,
        },
      )?;
    }

    for (key, value) in self.dependencies {
      batch.store_dependency(EntryType::DirectoryIndex, key, value, 0)?;
    }
    Ok(())
  }
}

fn directory_depth(path: &str) -> usize {
  path.split('/').filter(|segment| !segment.is_empty()).count()
}

struct DirectoryMutationFanout<'a> {
  engine: &'a StorageEngine,
  context: Option<&'a RequestContext>,
  effects: std::sync::Arc<std::sync::OnceLock<DirectoryMutationEffects>>,
}

impl NamespaceMutationFanout for DirectoryMutationFanout<'_> {
  fn publish(&self, acknowledgement: &NamespaceMutationAcknowledgement) {
    let Some(effects) = self.effects.get() else {
      tracing::error!(operation_id = %acknowledgement.operation_id, "Directory mutation committed without post-commit effects");
      return;
    };

    for (key, value) in &effects.cache_writes {
      if let Err(error) = self.engine.cache_dir_content(key.clone(), value.clone()) {
        tracing::warn!(operation_id = %acknowledgement.operation_id, error = %error, "Post-commit directory cache fill failed");
      }
    }

    for _ in 0..effects.implicit_directories {
      self.engine.counters().increment_directories();
    }
    match &effects.counter {
      DirectoryMutationCounterEffect::None => {}
      DirectoryMutationCounterEffect::FileWrite { previous_size, new_size, throughput_bytes } => {
        self.engine.counters().record_file_write(*previous_size, *new_size, *throughput_bytes);
      }
      DirectoryMutationCounterEffect::DirectoryCreate => self.engine.counters().record_directory_create(),
      DirectoryMutationCounterEffect::DirectoryDelete => self.engine.counters().record_directory_delete(),
      DirectoryMutationCounterEffect::SymlinkWrite { existed } => self.engine.counters().record_symlink_write(*existed),
      DirectoryMutationCounterEffect::SymlinkDelete => self.engine.counters().record_symlink_delete(),
      DirectoryMutationCounterEffect::Aggregate(delta) => {
        self.engine.counters().record_write(delta.throughput_bytes);
        for (previous_size, new_size) in &delta.file_writes {
          match previous_size {
            None => {
              self.engine.counters().increment_files();
              self.engine.counters().add_logical_data_size(*new_size);
            }
            Some(old_size) if new_size >= old_size => self.engine.counters().add_logical_data_size(*new_size - *old_size),
            Some(old_size) => self.engine.counters().sub_logical_data_size(*old_size - *new_size),
          }
        }
        for size in &delta.file_delete_sizes {
          self.engine.counters().decrement_files();
          self.engine.counters().sub_logical_data_size(*size);
        }
        for size in &delta.chunk_stored_sizes {
          self.engine.counters().record_chunk_stored(*size);
        }
        for _ in 0..delta.chunks_deduped {
          self.engine.counters().record_chunk_deduped();
        }
        for _ in 0..delta.directories_created {
          self.engine.counters().increment_directories();
        }
        for _ in 0..delta.directories_deleted {
          self.engine.counters().decrement_directories();
        }
        for _ in 0..delta.symlinks_created {
          self.engine.counters().increment_symlinks();
        }
        for _ in 0..delta.symlinks_deleted {
          self.engine.counters().decrement_symlinks();
        }
      }
    }

    for path in &effects.metadata_index_removal_paths {
      if let Err(error) = crate::engine::index_cleanup::remove_file_from_resolved_indexes(self.engine, path) {
        crate::metrics::record_system_soft_failure(
          "index_cleanup",
          "post_commit_path_removal",
          format_args!("operation_id={} path={path}", acknowledgement.operation_id),
          error,
        );
      }
    }

    if !effects.metadata_index_paths.is_empty() {
      let pipeline = IndexingPipeline::new(self.engine);
      let options = IndexWriteBufferOptions::new(DEFAULT_INDEX_BUFFER_FLUSH_WRITES, std::time::Duration::from_secs(300));
      let mut index_buffer = IndexWriteBuffer::new(self.engine, options);
      for path in &effects.metadata_index_paths {
        if let Err(error) = pipeline.run_metadata_only_buffered_with_outcome(path, &mut index_buffer) {
          tracing::warn!(operation_id = %acknowledgement.operation_id, path, error = %error, "Post-commit metadata indexing failed");
        }
        if let Err(error) = index_buffer.flush_if_due() {
          tracing::warn!(operation_id = %acknowledgement.operation_id, path, error = %error, "Post-commit metadata index flush failed");
        }
      }
    }

    if let Some(context) = self.context {
      for (event_type, event_payload) in &effects.events {
        let mut payload = event_payload.clone();
        if let Err(error) = acknowledgement.annotate_event_payload(&mut payload) {
          tracing::error!(operation_id = %acknowledgement.operation_id, error = %error, "Directory mutation event payload is invalid");
          continue;
        }
        context.emit(event_type, payload);
      }
    }
  }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FileDeletionRequirement {
  /// If this file is absent, publish nothing and report an empty result.
  Primary,
  /// If this file is absent while the primary is present, fail the operation.
  Required,
  /// If this file is absent while the primary is present, continue without it.
  Optional,
}

#[derive(Clone, Debug)]
pub(crate) struct FileDeletionRequest {
  pub path: String,
  pub requirement: FileDeletionRequirement,
  expected_identity: Option<Vec<u8>>,
}

impl FileDeletionRequest {
  pub(crate) fn primary(path: impl Into<String>) -> Self {
    Self { path: path.into(), requirement: FileDeletionRequirement::Primary, expected_identity: None }
  }

  pub(crate) fn required(path: impl Into<String>) -> Self {
    Self { path: path.into(), requirement: FileDeletionRequirement::Required, expected_identity: None }
  }

  pub(crate) fn optional(path: impl Into<String>) -> Self {
    Self { path: path.into(), requirement: FileDeletionRequirement::Optional, expected_identity: None }
  }

  /// Delete only if the current FileRecord still has the identity observed by
  /// the caller. Absence or replacement is a successful no-op.
  pub(crate) fn optional_matching_identity(path: impl Into<String>, expected_identity: Vec<u8>) -> Self {
    Self { path: path.into(), requirement: FileDeletionRequirement::Optional, expected_identity: Some(expected_identity) }
  }
}

impl<'a> DirectoryOps<'a> {
  /// Create a new `DirectoryOps` handle wrapping the given storage engine.
  pub fn new(engine: &'a StorageEngine) -> Self {
    DirectoryOps { engine }
  }

  fn execute_namespace_mutation<'operation, T, F>(
    &'operation self,
    context: Option<&'operation RequestContext>,
    prepare: F,
  ) -> EngineResult<T>
  where
    F: FnOnce(&StorageEngine) -> EngineResult<(NamespaceMutationBatch, T, DirectoryMutationEffects)>,
  {
    let effects = std::sync::Arc::new(std::sync::OnceLock::new());
    let fanout = std::sync::Arc::new(DirectoryMutationFanout { engine: self.engine, context, effects: effects.clone() });
    let coordinator = NamespaceMutationCoordinator::with_fanout(self.engine, fanout);
    let (_acknowledgement, output) = coordinator.prepare_and_execute(|planning_engine| {
      let (batch, output, planned_effects) = prepare(planning_engine)?;
      effects
        .set(planned_effects)
        .map_err(|_| EngineError::InvalidInput("directory mutation effects were prepared more than once".to_string()))?;
      Ok((batch, output))
    })?;
    Ok(output)
  }

  fn execute_optional_namespace_mutation<'operation, T, F>(
    &'operation self,
    context: Option<&'operation RequestContext>,
    prepare: F,
  ) -> EngineResult<T>
  where
    F: FnOnce(&StorageEngine) -> EngineResult<(Option<(NamespaceMutationBatch, DirectoryMutationEffects)>, T)>,
  {
    let effects = std::sync::Arc::new(std::sync::OnceLock::new());
    let fanout = std::sync::Arc::new(DirectoryMutationFanout { engine: self.engine, context, effects: effects.clone() });
    let coordinator = NamespaceMutationCoordinator::with_fanout(self.engine, fanout);
    let (_acknowledgement, output) = coordinator.prepare_and_maybe_execute(|planning_engine| {
      let (prepared, output) = prepare(planning_engine)?;
      let Some((batch, planned_effects)) = prepared else {
        return Ok((None, output));
      };
      effects
        .set(planned_effects)
        .map_err(|_| EngineError::InvalidInput("directory mutation effects were prepared more than once".to_string()))?;
      Ok((Some(batch), output))
    })?;
    Ok(output)
  }

  fn execute_file_publication(
    &self,
    context: &RequestContext,
    input: FileRecordPublishInput,
    entry_version: u8,
    throughput_bytes: u64,
    emit_event: bool,
    index_metadata: bool,
  ) -> EngineResult<FileRecord> {
    self.execute_file_publication_with(
      context,
      NamespaceMutationKind::FileWrite,
      throughput_bytes,
      emit_event,
      index_metadata,
      move |_planning_engine| Ok((input, entry_version, false)),
    )
  }

  fn execute_file_publication_with<'operation, F>(
    &'operation self,
    context: &'operation RequestContext,
    kind: NamespaceMutationKind,
    throughput_bytes: u64,
    emit_event: bool,
    index_metadata: bool,
    prepare_input: F,
  ) -> EngineResult<FileRecord>
  where
    F: FnOnce(&StorageEngine) -> EngineResult<(FileRecordPublishInput, u8, bool)>,
  {
    self.execute_namespace_mutation(Some(context), move |planning_engine| {
      let (input, entry_version, require_absent) = prepare_input(planning_engine)?;
      validate_existing_file_chunks(planning_engine, &input.normalized_path, &input.chunk_hashes)?;
      let prepared = prepare_file_record_publication_at_version(planning_engine, input, entry_version)?;
      if require_absent && prepared.previous_identity.is_some() {
        return Err(EngineError::AlreadyExists(prepared.result.normalized_path));
      }
      let mut batch = NamespaceMutationBatch::new(kind);
      add_prepared_file_record_entries(&mut batch, &prepared.entries)?;
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: prepared.result.normalized_path.clone(),
        entry_type: Some(EntryType::FileRecord.to_u8()),
        previous_identity: prepared.previous_identity.clone(),
        new_identity: Some(prepared.entries.identity_key.clone()),
      })?;

      let mut effects = DirectoryMutationEffects::new(DirectoryMutationCounterEffect::FileWrite {
        previous_size: prepared.result.existing_total_size,
        new_size: prepared.result.file_record.total_size,
        throughput_bytes,
      });
      self.plan_parent_directories(&mut batch, &prepared.result.normalized_path, prepared.result.child_entry.clone(), &mut effects)?;
      if emit_event {
        effects.events.push((EVENT_ENTRIES_CREATED, serde_json::json!({"entries": [prepared.result.event_entry.clone()]})));
      }
      if index_metadata {
        effects.metadata_index_paths.push(prepared.result.normalized_path.clone());
      }
      Ok((batch, prepared.result.file_record, effects))
    })
  }

  fn execute_file_record_restore(&self, context: &RequestContext, path: &str, source: FileRecordRestoreSource) -> EngineResult<FileRecord> {
    let normalized = normalize_path(path);
    let file_key = file_path_hash(&normalized, &self.engine.hash_algo())?;
    self.execute_file_publication_with(context, NamespaceMutationKind::Restore, 0, true, true, move |planning_engine| {
      let (record, flags, require_absent) = match source {
        FileRecordRestoreSource::Historical(mut record) => {
          if record.content_type.is_none() {
            record.content_type = Some("application/octet-stream".to_string());
          }
          (record, 0, false)
        }
        FileRecordRestoreSource::DeletedLocator => {
          if !planning_engine.is_entry_deleted(&file_key)? {
            return Err(EngineError::NotFound(format!("No deleted record found for file: {normalized}")));
          }
          let (header, stored_key, value) = planning_engine.get_entry_verified_including_deleted(&file_key)?.ok_or_else(|| {
            EngineError::CorruptEntry { offset: 0, reason: format!("Deleted file locator '{normalized}' lost its record") }
          })?;
          if stored_key != file_key || header.entry_type != EntryType::FileRecord {
            return Err(EngineError::CorruptEntry {
              offset: 0,
              reason: format!("Deleted file locator '{normalized}' resolves to the wrong record"),
            });
          }
          (FileRecord::deserialize(&value, planning_engine.hash_algo().hash_length(), header.entry_version)?, header.flags, true)
        }
      };
      if record.path != normalized {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("Restore FileRecord path '{}' does not match requested path '{normalized}'", record.path),
        });
      }
      Ok((
        FileRecordPublishInput {
          normalized_path: normalized,
          content_type: record.content_type,
          total_size: record.total_size,
          metadata: record.metadata,
          chunk_hashes: record.chunk_hashes,
          content_hash: record.content_hash,
          flags,
          created_at_override: Some(record.created_at),
          updated_at_override: Some(record.updated_at),
          prefer_existing_created_at: true,
        },
        CURRENT_FILE_RECORD_VERSION,
        require_absent,
      ))
    })
  }

  pub(crate) fn execute_file_publications(
    &self,
    context: &RequestContext,
    inputs: Vec<BatchFilePublicationInput>,
    kind: NamespaceMutationKind,
  ) -> EngineResult<Vec<FileRecordPublishResult>> {
    if inputs.is_empty() {
      return Err(EngineError::InvalidInput("No files provided for namespace publication".to_string()));
    }

    self.execute_namespace_mutation(Some(context), move |planning_engine| {
      let mut batch = NamespaceMutationBatch::new(kind);
      let mut planner = DirectoryMutationPlanner::default();
      let mut counter_delta = DirectoryMutationCounterDelta::default();
      let mut results = Vec::with_capacity(inputs.len());
      let mut event_entries = Vec::with_capacity(inputs.len());
      let mut seen_paths = std::collections::HashSet::with_capacity(inputs.len());

      for input in inputs {
        let normalized = normalize_path(&input.publication.normalized_path);
        if normalized == "/" {
          return Err(EngineError::InvalidInput("Cannot store at root path".to_string()));
        }
        if !seen_paths.insert(normalized.clone()) {
          return Err(EngineError::InvalidInput(format!("Duplicate batch path: {normalized}")));
        }

        let mut publication = input.publication;
        publication.normalized_path = normalized;
        validate_existing_file_chunks(planning_engine, &publication.normalized_path, &publication.chunk_hashes)?;
        let prepared = prepare_file_record_publication_at_version(planning_engine, publication, CURRENT_FILE_RECORD_VERSION)?;
        add_prepared_file_record_entries(&mut batch, &prepared.entries)?;
        batch.add_source_identity(NamespaceMutationSourceIdentity {
          path: prepared.result.normalized_path.clone(),
          entry_type: Some(EntryType::FileRecord.to_u8()),
          previous_identity: prepared.previous_identity.clone(),
          new_identity: Some(prepared.entries.identity_key.clone()),
        })?;
        planner.upsert_child(&prepared.result.normalized_path, prepared.result.child_entry.clone())?;
        counter_delta.throughput_bytes = counter_delta
          .throughput_bytes
          .checked_add(input.throughput_bytes)
          .ok_or_else(|| EngineError::ResourceExhausted("batch write throughput counter overflow".to_string()))?;
        counter_delta.file_writes.push((prepared.result.existing_total_size, prepared.result.file_record.total_size));
        event_entries.push(prepared.result.event_entry.clone());
        results.push(prepared.result);
      }

      let mut effects = DirectoryMutationEffects::new(DirectoryMutationCounterEffect::Aggregate(counter_delta));
      effects.metadata_index_paths.extend(results.iter().map(|result| result.normalized_path.clone()));
      effects.events.push((EVENT_ENTRIES_CREATED, serde_json::json!({ "entries": event_entries })));
      planner.finalize(self, &mut batch, &mut effects)?;
      Ok((batch, results, effects))
    })
  }

  /// Apply one peer merge as a single namespace publication.
  ///
  /// Every operation and referenced chunk is validated while namespace
  /// authority is held. A failure therefore publishes no partial HEAD,
  /// locator, counter, index, or event state. Missing delete targets are the
  /// only idempotent no-op; corruption and every other failure are surfaced.
  /// Exact unresolved-conflict reuse is decided under the same authority so
  /// concurrent peers cannot both record or count one winner/loser pair.
  pub fn apply_sync_merge(&self, context: &RequestContext, operations: &[MergeOp]) -> EngineResult<()> {
    self.apply_sync_receipt(context, operations, &[], &[]).map(|_| ())
  }

  pub(crate) fn apply_sync_receipt(
    &self,
    context: &RequestContext,
    operations: &[MergeOp],
    conflicts: &[ConflictEntry],
    immutable_versions: &[SyncImmutableVersion],
  ) -> EngineResult<usize> {
    if operations.is_empty() && conflicts.is_empty() {
      return Ok(0);
    }

    let evidence_candidates =
      conflicts.iter().map(crate::engine::conflict_store::conflict_metadata_file).collect::<EngineResult<Vec<_>>>()?;

    let operation_bytes = operations.iter().try_fold(0u64, |total, operation| {
      let bytes = match operation {
        MergeOp::AddFile { path, file_hash, file_record } => path
          .len()
          .checked_add(file_hash.len())
          .and_then(|bytes| bytes.checked_add(file_record.path.len()))
          .and_then(|bytes| bytes.checked_add(file_record.content_type.as_ref().map_or(0, String::len)))
          .and_then(|bytes| bytes.checked_add(file_record.content_hash.len()))
          .and_then(|bytes| bytes.checked_add(file_record.chunk_hashes.iter().map(Vec::len).sum::<usize>())),
        MergeOp::DeleteFile { path } | MergeOp::DeleteSymlink { path } => Some(path.len()),
        MergeOp::AddSymlink { path, symlink_hash, symlink_record } => path
          .len()
          .checked_add(symlink_hash.len())
          .and_then(|bytes| bytes.checked_add(symlink_record.path.len()))
          .and_then(|bytes| bytes.checked_add(symlink_record.target.len())),
      }
      .and_then(|bytes| bytes.checked_add(512))
      .and_then(|bytes| u64::try_from(bytes).ok())
      .ok_or_else(|| EngineError::ResourceExhausted("sync merge planning estimate overflow".to_string()))?;
      total.checked_add(bytes).ok_or_else(|| EngineError::ResourceExhausted("sync merge planning estimate overflow".to_string()))
    })?;
    let evidence_bytes = evidence_candidates.iter().try_fold(operation_bytes, |total, file| {
      let bytes = file
        .path
        .len()
        .checked_add(file.data.len())
        .and_then(|bytes| bytes.checked_add(file.content_type.as_ref().map_or(0, String::len)))
        .and_then(|bytes| bytes.checked_add(512))
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or_else(|| EngineError::ResourceExhausted("sync evidence planning estimate overflow".to_string()))?;
      total.checked_add(bytes).ok_or_else(|| EngineError::ResourceExhausted("sync evidence planning estimate overflow".to_string()))
    })?;
    let retained_bytes = immutable_versions.iter().try_fold(evidence_bytes, |total, version| {
      let bytes = match version {
        SyncImmutableVersion::File { identity_hash, record } => identity_hash
          .len()
          .checked_add(record.path.len())
          .and_then(|bytes| bytes.checked_add(record.content_type.as_ref().map_or(0, String::len)))
          .and_then(|bytes| bytes.checked_add(record.metadata.len()))
          .and_then(|bytes| bytes.checked_add(record.content_hash.len()))
          .and_then(|bytes| bytes.checked_add(record.chunk_hashes.iter().map(Vec::len).sum::<usize>())),
        SyncImmutableVersion::Symlink { identity_hash, record } => {
          identity_hash.len().checked_add(record.path.len()).and_then(|bytes| bytes.checked_add(record.target.len()))
        }
      }
      .and_then(|bytes| bytes.checked_add(512))
      .and_then(|bytes| u64::try_from(bytes).ok())
      .ok_or_else(|| EngineError::ResourceExhausted("sync immutable-version planning estimate overflow".to_string()))?;
      total
        .checked_add(bytes)
        .ok_or_else(|| EngineError::ResourceExhausted("sync immutable-version planning estimate overflow".to_string()))
    })?;
    let mut memory = OperationMemoryBudget::new(
      self.engine,
      "sync merge planning",
      MemoryOwner::DurabilityWaiters,
      AdmissionClass::Workload,
      retained_bytes,
      None,
    )?;

    self.execute_optional_namespace_mutation(Some(context), move |planning_engine| {
      let algorithm = planning_engine.hash_algo();
      let hash_length = algorithm.hash_length();
      let unrecorded_conflicts = crate::engine::conflict_store::unrecorded_conflicts(planning_engine, conflicts)?;
      let buffered_evidence =
        unrecorded_conflicts.iter().map(crate::engine::conflict_store::conflict_metadata_file).collect::<EngineResult<Vec<_>>>()?;
      let mut unrecorded_conflict_paths_by_hash = std::collections::HashMap::new();
      for conflict in &unrecorded_conflicts {
        let conflict_path = normalize_path(&conflict.path);
        for version in [&conflict.winner, &conflict.loser] {
          if version.hash.is_empty() {
            continue;
          }
          if let Some(existing_path) = unrecorded_conflict_paths_by_hash.insert(version.hash.clone(), conflict_path.clone()) {
            if existing_path != conflict_path {
              return Err(EngineError::CorruptEntry {
                offset: 0,
                reason: format!("sync conflict identity is assigned to both '{existing_path}' and '{conflict_path}'"),
              });
            }
          }
        }
      }
      let newly_recorded_conflicts = buffered_evidence.len();
      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::SyncApply);
      let mut planner = DirectoryMutationPlanner::default();
      let mut counter_delta = DirectoryMutationCounterDelta::default();
      let mut created_events = Vec::new();
      let mut deleted_events = Vec::new();
      let mut metadata_index_paths = Vec::new();
      let mut metadata_index_removal_paths = Vec::new();
      let mut seen_paths = std::collections::HashSet::with_capacity(operations.len().saturating_add(buffered_evidence.len()));
      let mut planned_chunk_keys = std::collections::HashSet::new();
      let mut changed = false;

      for version in immutable_versions {
        memory.record_work(1)?;
        let (identity_hash, version_path) = match version {
          SyncImmutableVersion::File { identity_hash, record } => (identity_hash, &record.path),
          SyncImmutableVersion::Symlink { identity_hash, record } => (identity_hash, &record.path),
        };
        let Some(conflict_path) = unrecorded_conflict_paths_by_hash.get(identity_hash) else {
          continue;
        };
        if normalize_path(version_path) != *conflict_path {
          return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: format!("sync conflict version path '{version_path}' does not match evidence path '{conflict_path}'"),
          });
        }
        match version {
          SyncImmutableVersion::File { identity_hash, record } => {
            validate_sync_file_record(planning_engine, identity_hash, record)?;
            let expected_identity = file_identity_hash(&record.path, record.content_type.as_deref(), &record.chunk_hashes, &algorithm)?;
            if *identity_hash != expected_identity {
              return Err(EngineError::CorruptEntry {
                offset: 0,
                reason: format!("sync conflict FileRecord '{}' is not keyed by its canonical identity", record.path),
              });
            }
            let mut canonical_record = record.clone();
            if canonical_record.content_hash.is_empty() {
              canonical_record.content_hash = whole_file_content_hash_from_chunks(planning_engine, &canonical_record.chunk_hashes)?;
            }
            let serialized = canonical_record.serialize(hash_length)?;
            let content_key = file_content_hash(&serialized, &algorithm)?;
            let flags = v0_system_entry_flags(&canonical_record.path);
            batch.store_dependency_with_version(
              EntryType::FileRecord,
              identity_hash.clone(),
              serialized.clone(),
              flags,
              CURRENT_FILE_RECORD_VERSION,
            )?;
            if content_key != *identity_hash {
              batch.store_dependency_with_version(EntryType::FileRecord, content_key, serialized, flags, CURRENT_FILE_RECORD_VERSION)?;
            }
          }
          SyncImmutableVersion::Symlink { identity_hash, record } => {
            validate_sync_symlink(&algorithm, hash_length, identity_hash, record)?;
            let expected_identity = symlink_identity_hash(&record.path, &normalize_path(&record.target), &algorithm)?;
            if *identity_hash != expected_identity {
              return Err(EngineError::CorruptEntry {
                offset: 0,
                reason: format!("sync conflict symlink '{}' is not keyed by its canonical identity", record.path),
              });
            }
            let serialized = record.serialize()?;
            let content_key = symlink_content_hash(&serialized, &algorithm)?;
            let flags = v0_system_entry_flags(&record.path);
            batch.store_dependency(EntryType::Symlink, identity_hash.clone(), serialized.clone(), flags)?;
            if content_key != *identity_hash {
              batch.store_dependency(EntryType::Symlink, content_key, serialized, flags)?;
            }
          }
        }
      }

      for file in buffered_evidence {
        memory.record_work(1)?;
        let path = normalize_path(&file.path);
        if path == "/" {
          return Err(EngineError::InvalidInput("Sync evidence cannot target the root path".to_string()));
        }
        if !seen_paths.insert(path.clone()) {
          return Err(EngineError::InvalidInput(format!("Sync receipt contains multiple operations for '{path}'")));
        }
        let flags = v0_system_entry_flags(&path);
        let mut chunk_hashes = Vec::new();
        for chunk_data in file.data.chunks(DEFAULT_CHUNK_SIZE) {
          let chunk_key = chunk_content_hash(chunk_data, &algorithm)?;
          if planned_chunk_keys.insert(chunk_key.clone()) && !validate_existing_chunk_locator(planning_engine, "sync evidence", &chunk_key)?
          {
            batch.store_dependency(EntryType::Chunk, chunk_key.clone(), chunk_data.to_vec(), flags)?;
          }
          chunk_hashes.push(chunk_key);
        }
        let detected_content_type = crate::engine::content_type::detect_content_type(&file.data, file.content_type.as_deref());
        let prepared = prepare_file_record_publication_at_version(
          planning_engine,
          FileRecordPublishInput {
            normalized_path: path.clone(),
            content_type: Some(detected_content_type),
            total_size: file.data.len() as u64,
            metadata: Vec::new(),
            chunk_hashes,
            content_hash: whole_file_content_hash(&file.data, &algorithm)?,
            flags,
            created_at_override: None,
            updated_at_override: None,
            prefer_existing_created_at: true,
          },
          CURRENT_FILE_RECORD_VERSION,
        )?;
        add_prepared_file_record_entries(&mut batch, &prepared.entries)?;
        batch.add_source_identity(NamespaceMutationSourceIdentity {
          path: path.clone(),
          entry_type: Some(EntryType::FileRecord.to_u8()),
          previous_identity: prepared.previous_identity.clone(),
          new_identity: Some(prepared.entries.identity_key.clone()),
        })?;
        planner.upsert_child(&path, prepared.result.child_entry.clone())?;
        counter_delta.throughput_bytes = counter_delta
          .throughput_bytes
          .checked_add(file.data.len() as u64)
          .ok_or_else(|| EngineError::ResourceExhausted("sync evidence throughput counter overflow".to_string()))?;
        counter_delta.file_writes.push((prepared.result.existing_total_size, prepared.result.file_record.total_size));
        metadata_index_paths.push(path);
        created_events.push(prepared.result.event_entry);
        changed = true;
      }

      for operation in operations {
        memory.record_work(1)?;
        let path = match operation {
          MergeOp::AddFile { path, .. }
          | MergeOp::DeleteFile { path }
          | MergeOp::AddSymlink { path, .. }
          | MergeOp::DeleteSymlink { path } => normalize_path(path),
        };
        if path == "/" {
          return Err(EngineError::InvalidInput("Sync merge cannot mutate the root path".to_string()));
        }
        if !seen_paths.insert(path.clone()) {
          return Err(EngineError::InvalidInput(format!("Sync merge contains multiple operations for '{path}'")));
        }

        match operation {
          MergeOp::AddFile { file_hash, file_record, .. } => {
            if file_record.path != path {
              return Err(EngineError::InvalidInput(format!(
                "sync FileRecord path '{}' does not match merge path '{path}'",
                file_record.path
              )));
            }
            validate_sync_file_record(planning_engine, file_hash, file_record)?;
            let prepared = prepare_file_record_publication_at_version(
              planning_engine,
              FileRecordPublishInput {
                normalized_path: path.clone(),
                content_type: file_record.content_type.clone(),
                total_size: file_record.total_size,
                metadata: file_record.metadata.clone(),
                chunk_hashes: file_record.chunk_hashes.clone(),
                content_hash: file_record.content_hash.clone(),
                flags: v0_system_entry_flags(&path),
                created_at_override: Some(file_record.created_at),
                updated_at_override: Some(file_record.updated_at),
                prefer_existing_created_at: true,
              },
              CURRENT_FILE_RECORD_VERSION,
            )?;
            add_prepared_file_record_entries(&mut batch, &prepared.entries)?;
            batch.add_source_identity(NamespaceMutationSourceIdentity {
              path: path.clone(),
              entry_type: Some(EntryType::FileRecord.to_u8()),
              previous_identity: prepared.previous_identity.clone(),
              new_identity: Some(prepared.entries.identity_key.clone()),
            })?;
            planner.upsert_child(&path, prepared.result.child_entry.clone())?;
            counter_delta.file_writes.push((prepared.result.existing_total_size, prepared.result.file_record.total_size));
            metadata_index_paths.push(path);
            created_events.push(prepared.result.event_entry);
            changed = true;
          }
          MergeOp::DeleteFile { .. } => {
            if !self.sync_delete_target_matches(planning_engine, &path, EntryType::FileRecord)? {
              continue;
            }
            let Some((previous_identity, record)) = self.resolve_current_file_record_from(planning_engine, &path)? else {
              return Err(EngineError::CorruptEntry {
                offset: 0,
                reason: format!("sync file delete target '{path}' disappeared during authoritative planning"),
              });
            };
            let file_key = file_path_hash(&path, &algorithm)?;
            let deletion = DeletionRecord::new(path.clone(), None);
            let deletion_key = deletion_record_hash(&path, deletion.deleted_at, &algorithm)?;
            batch.store_dependency(EntryType::DeletionRecord, deletion_key, deletion.serialize(), v0_system_entry_flags(&path))?;
            if planning_engine.has_entry(&file_key)? {
              batch.retire_locator(file_key)?;
            }
            batch.add_source_identity(NamespaceMutationSourceIdentity {
              path: path.clone(),
              entry_type: Some(EntryType::FileRecord.to_u8()),
              previous_identity: Some(previous_identity),
              new_identity: None,
            })?;
            planner.remove_child(&path)?;
            counter_delta.file_delete_sizes.push(record.total_size);
            metadata_index_removal_paths.push(path.clone());
            deleted_events.push(EntryEventData {
              path,
              entry_type: "file".to_string(),
              content_type: record.content_type.clone(),
              size: record.total_size,
              hash: record.content_hash_hex(),
              created_at: record.created_at,
              updated_at: record.updated_at,
              previous_hash: None,
            });
            changed = true;
          }
          MergeOp::AddSymlink { symlink_hash, symlink_record, .. } => {
            if symlink_record.path != path {
              return Err(EngineError::InvalidInput(format!(
                "sync symlink path '{}' does not match merge path '{path}'",
                symlink_record.path
              )));
            }
            validate_sync_symlink(&algorithm, hash_length, symlink_hash, symlink_record)?;
            let target = normalize_path(&symlink_record.target);
            if path == target {
              return Err(EngineError::InvalidInput(format!("Symlink cannot point to itself: {path}")));
            }
            let current = self.resolve_current_symlink_record_from(planning_engine, &path)?;
            let (existing_created_at, previous_identity) = match current {
              Some((identity, record)) => (Some(record.created_at), Some(identity)),
              None => (None, None),
            };
            let mut record = SymlinkRecord::new(path.clone(), target);
            record.created_at = existing_created_at.unwrap_or(symlink_record.created_at);
            record.updated_at = symlink_record.updated_at;
            let serialized = record.serialize()?;
            let content_key = symlink_content_hash(&serialized, &algorithm)?;
            let identity_key = symlink_identity_hash(&path, &record.target, &algorithm)?;
            let symlink_key = symlink_path_hash(&path, &algorithm)?;
            let flags = v0_system_entry_flags(&path);
            batch.store_dependency(EntryType::Symlink, content_key, serialized.clone(), flags)?;
            batch.store_dependency(EntryType::Symlink, identity_key.clone(), serialized.clone(), flags)?;
            batch.replace_locator(EntryType::Symlink, symlink_key, serialized, flags)?;
            batch.add_source_identity(NamespaceMutationSourceIdentity {
              path: path.clone(),
              entry_type: Some(EntryType::Symlink.to_u8()),
              previous_identity,
              new_identity: Some(identity_key.clone()),
            })?;
            planner.upsert_child(
              &path,
              ChildEntry {
                entry_type: EntryType::Symlink.to_u8(),
                hash: identity_key,
                total_size: 0,
                created_at: record.created_at,
                updated_at: record.updated_at,
                name: file_name(&path).unwrap_or("").to_string(),
                content_type: None,
                virtual_time: record.updated_at.max(0) as u64,
                node_id: 0,
              },
            )?;
            if existing_created_at.is_none() {
              counter_delta.symlinks_created = counter_delta
                .symlinks_created
                .checked_add(1)
                .ok_or_else(|| EngineError::ResourceExhausted("sync symlink counter overflow".to_string()))?;
            }
            created_events.push(EntryEventData {
              path,
              entry_type: "symlink".to_string(),
              content_type: None,
              size: 0,
              hash: hex::encode(&record.target),
              created_at: record.created_at,
              updated_at: record.updated_at,
              previous_hash: None,
            });
            changed = true;
          }
          MergeOp::DeleteSymlink { .. } => {
            if !self.sync_delete_target_matches(planning_engine, &path, EntryType::Symlink)? {
              continue;
            }
            let Some((previous_identity, record)) = self.resolve_current_symlink_record_from(planning_engine, &path)? else {
              return Err(EngineError::CorruptEntry {
                offset: 0,
                reason: format!("sync symlink delete target '{path}' disappeared during authoritative planning"),
              });
            };
            let symlink_key = symlink_path_hash(&path, &algorithm)?;
            let deletion = DeletionRecord::new(path.clone(), None);
            let deletion_key = deletion_record_hash(&path, deletion.deleted_at, &algorithm)?;
            batch.store_dependency(EntryType::DeletionRecord, deletion_key, deletion.serialize(), v0_system_entry_flags(&path))?;
            if planning_engine.has_entry(&symlink_key)? {
              batch.retire_locator(symlink_key)?;
            }
            batch.add_source_identity(NamespaceMutationSourceIdentity {
              path: path.clone(),
              entry_type: Some(EntryType::Symlink.to_u8()),
              previous_identity: Some(previous_identity),
              new_identity: None,
            })?;
            planner.remove_child(&path)?;
            counter_delta.symlinks_deleted = counter_delta
              .symlinks_deleted
              .checked_add(1)
              .ok_or_else(|| EngineError::ResourceExhausted("sync symlink counter overflow".to_string()))?;
            deleted_events.push(EntryEventData {
              path,
              entry_type: "symlink".to_string(),
              content_type: None,
              size: 0,
              hash: hex::encode(&record.target),
              created_at: record.created_at,
              updated_at: record.updated_at,
              previous_hash: None,
            });
            changed = true;
          }
        }
      }

      if !changed {
        return Ok((None, newly_recorded_conflicts));
      }
      let mut effects = DirectoryMutationEffects::new(DirectoryMutationCounterEffect::Aggregate(counter_delta));
      effects.metadata_index_paths = metadata_index_paths;
      effects.metadata_index_removal_paths = metadata_index_removal_paths;
      if !created_events.is_empty() {
        effects.events.push((EVENT_ENTRIES_CREATED, serde_json::json!({ "entries": created_events })));
      }
      if !deleted_events.is_empty() {
        effects.events.push((EVENT_ENTRIES_DELETED, serde_json::json!({ "entries": deleted_events })));
      }
      planner.finalize(self, &mut batch, &mut effects)?;
      Ok((Some((batch, effects)), newly_recorded_conflicts))
    })
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

  /// Read and conditionally replace one small file while namespace authority
  /// is held. The transform sees one immutable current body (or absence), and
  /// any replacement shares a single root/locator acknowledgement.
  pub(crate) fn transform_file_buffered<T, F>(
    &self,
    ctx: &RequestContext,
    path: &str,
    content_type: Option<&str>,
    maximum_bytes: u64,
    kind: NamespaceMutationKind,
    transform: F,
  ) -> EngineResult<T>
  where
    F: FnOnce(Option<&[u8]>) -> EngineResult<BufferedFileTransform<T>>,
  {
    let normalized = normalize_path(path);
    if normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot transform the root path as a file".to_string()));
    }
    let requested_content_type = content_type.map(str::to_string);

    self.execute_optional_namespace_mutation(Some(ctx), move |planning_engine| {
      let existing = match self.read_file_buffered_bounded(&normalized, maximum_bytes) {
        Ok(data) => Some(data),
        Err(EngineError::NotFound(_)) => None,
        Err(error) => return Err(error),
      };
      let (data, output) = match transform(existing.as_deref())? {
        BufferedFileTransform::Keep(output) => return Ok((None, output)),
        BufferedFileTransform::Replace { data, output } => (data, output),
      };
      if data.len() as u64 > maximum_bytes {
        return Err(EngineError::ResourceExhausted(format!(
          "buffered transform for '{normalized}' produced {} bytes, exceeding the {maximum_bytes}-byte limit",
          data.len(),
        )));
      }

      let flags = v0_system_entry_flags(&normalized);
      let detected_content_type = crate::engine::content_type::detect_content_type(&data, requested_content_type.as_deref());
      let total_size = data.len() as u64;
      let mut chunk_dependencies = std::collections::BTreeMap::<Vec<u8>, (Vec<u8>, u8)>::new();
      let mut counter_delta = DirectoryMutationCounterDelta::default();
      let chunk_owner = format!("buffered transform '{normalized}'");
      let prepared = prepare_buffered_file_publication(
        planning_engine,
        normalized.clone(),
        &data,
        detected_content_type,
        flags,
        &chunk_owner,
        &mut chunk_dependencies,
        &mut counter_delta,
      )?;

      let mut batch = NamespaceMutationBatch::new(kind);
      for (chunk_key, (chunk_data, chunk_flags)) in chunk_dependencies {
        batch.store_dependency(EntryType::Chunk, chunk_key, chunk_data, chunk_flags)?;
      }
      add_prepared_file_record_entries(&mut batch, &prepared.entries)?;
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: normalized.clone(),
        entry_type: Some(EntryType::FileRecord.to_u8()),
        previous_identity: prepared.previous_identity.clone(),
        new_identity: Some(prepared.entries.identity_key.clone()),
      })?;

      counter_delta.throughput_bytes = total_size;
      counter_delta.file_writes.push((prepared.result.existing_total_size, prepared.result.file_record.total_size));
      let mut planner = DirectoryMutationPlanner::default();
      planner.upsert_child(&normalized, prepared.result.child_entry.clone())?;
      let mut effects = DirectoryMutationEffects::new(DirectoryMutationCounterEffect::Aggregate(counter_delta));
      effects.events.push((EVENT_ENTRIES_CREATED, serde_json::json!({ "entries": [prepared.result.event_entry] })));
      planner.finalize(self, &mut batch, &mut effects)?;
      Ok((Some((batch, effects)), output))
    })
  }

  pub(crate) fn store_transition_control_v0(
    &self,
    publication: &V3ControlPublicationContextV0<'_>,
    target_slot: SystemControlSlotV1,
    data: &[u8],
  ) -> EngineResult<FileRecord> {
    if !std::ptr::eq(self.engine, publication.engine()) {
      return Err(EngineError::InvalidInput("transition control publication context belongs to a different engine".to_string()));
    }
    let normalized = publication.target_path(target_slot)?;
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
    self.merge_json_file_bounded(ctx, path, patch, depth, None)
  }

  pub(crate) fn merge_json_file_bounded(
    &self,
    ctx: &RequestContext,
    path: &str,
    patch: serde_json::Value,
    depth: MergeDepth,
    maximum_existing_bytes: Option<usize>,
  ) -> EngineResult<JsonMergeFileResult> {
    let mut merged =
      self.execute_json_merge_patches(ctx, vec![JsonMergeFilePatch { path: path.to_string(), patch, depth }], maximum_existing_bytes)?;
    merged.pop().ok_or_else(|| EngineError::CorruptEntry { offset: 0, reason: "single JSON merge produced no result".to_string() })
  }

  /// Apply JSON merge patches to multiple small JSON files in one write batch.
  ///
  /// All target documents are read, parsed, and merged before the batch write
  /// starts, so invalid JSON in any existing file prevents every write in the
  /// batch.
  pub fn merge_json_files_batch(&self, ctx: &RequestContext, patches: Vec<JsonMergeFilePatch>) -> EngineResult<JsonMergeBatchResult> {
    let results = self.execute_json_merge_patches(ctx, patches, None)?;
    let files = results
      .iter()
      .map(|result| JsonMergedFile { path: result.file_record.path.clone(), size: result.file_record.total_size, created: result.created })
      .collect();
    Ok(JsonMergeBatchResult { merged: results.len(), files })
  }

  fn execute_json_merge_patches(
    &self,
    ctx: &RequestContext,
    patches: Vec<JsonMergeFilePatch>,
    maximum_existing_bytes: Option<usize>,
  ) -> EngineResult<Vec<JsonMergeFileResult>> {
    if patches.is_empty() {
      return Err(EngineError::InvalidInput("No JSON merge patches provided".to_string()));
    }

    let mut seen_paths = std::collections::HashSet::with_capacity(patches.len());
    let mut normalized_patches = Vec::with_capacity(patches.len());
    for mut patch in patches {
      if patch.path.bytes().any(|byte| byte < 0x20 || byte == 0x7F) {
        return Err(EngineError::InvalidInput("JSON merge path contains control characters".to_string()));
      }
      let normalized = normalize_path(&patch.path);
      if normalized == "/" {
        return Err(EngineError::InvalidInput("Cannot store at root path".to_string()));
      }
      if !seen_paths.insert(normalized.clone()) {
        return Err(EngineError::InvalidInput(format!("Duplicate batch path: {}", normalized)));
      }
      patch.path = normalized;
      normalized_patches.push(patch);
    }

    self.execute_namespace_mutation(Some(ctx), move |planning_engine| {
      let mut chunk_dependencies: std::collections::BTreeMap<Vec<u8>, (Vec<u8>, u8)> = std::collections::BTreeMap::new();
      let mut counter_delta = DirectoryMutationCounterDelta::default();
      let mut prepared_merges = Vec::with_capacity(normalized_patches.len());

      for patch in normalized_patches {
        let (serialized, existed) = self.prepare_json_merge(&patch.path, patch.patch, patch.depth, maximum_existing_bytes)?;
        let flags = v0_system_entry_flags(&patch.path);
        let chunk_owner = format!("JSON merge '{}'", patch.path);
        let total_size = serialized.len() as u64;
        let prepared = prepare_buffered_file_publication(
          planning_engine,
          patch.path,
          &serialized,
          "application/json".to_string(),
          flags,
          &chunk_owner,
          &mut chunk_dependencies,
          &mut counter_delta,
        )?;
        counter_delta.throughput_bytes = counter_delta
          .throughput_bytes
          .checked_add(total_size)
          .ok_or_else(|| EngineError::ResourceExhausted("JSON merge throughput counter overflow".to_string()))?;
        prepared_merges.push((prepared, !existed));
      }

      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::Merge);
      for (chunk_key, (chunk_data, flags)) in chunk_dependencies {
        batch.store_dependency(EntryType::Chunk, chunk_key, chunk_data, flags)?;
      }
      let mut planner = DirectoryMutationPlanner::default();
      let mut event_entries = Vec::with_capacity(prepared_merges.len());
      let mut results = Vec::with_capacity(prepared_merges.len());
      let mut metadata_index_paths = Vec::with_capacity(prepared_merges.len());
      for (prepared, created) in prepared_merges {
        add_prepared_file_record_entries(&mut batch, &prepared.entries)?;
        batch.add_source_identity(NamespaceMutationSourceIdentity {
          path: prepared.result.normalized_path.clone(),
          entry_type: Some(EntryType::FileRecord.to_u8()),
          previous_identity: prepared.previous_identity.clone(),
          new_identity: Some(prepared.entries.identity_key.clone()),
        })?;
        planner.upsert_child(&prepared.result.normalized_path, prepared.result.child_entry.clone())?;
        counter_delta.file_writes.push((prepared.result.existing_total_size, prepared.result.file_record.total_size));
        metadata_index_paths.push(prepared.result.normalized_path.clone());
        event_entries.push(prepared.result.event_entry.clone());
        results.push(JsonMergeFileResult { file_record: prepared.result.file_record, created });
      }

      let mut effects = DirectoryMutationEffects::new(DirectoryMutationCounterEffect::Aggregate(counter_delta));
      effects.metadata_index_paths = metadata_index_paths;
      effects.events.push((EVENT_ENTRIES_CREATED, serde_json::json!({ "entries": event_entries })));
      planner.finalize(self, &mut batch, &mut effects)?;
      Ok((batch, results, effects))
    })
  }

  fn prepare_json_merge(
    &self,
    path: &str,
    patch: serde_json::Value,
    depth: MergeDepth,
    maximum_existing_bytes: Option<usize>,
  ) -> EngineResult<(Vec<u8>, bool)> {
    let normalized = normalize_path(path);
    if normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot store at root path".to_string()));
    }

    let (mut target, existed) = match self.read_file_buffered(&normalized) {
      Ok(bytes) => {
        if let Some(maximum) = maximum_existing_bytes {
          if bytes.len() > maximum {
            return Err(EngineError::InvalidInput(format!(
              "stored file at {} is {} bytes, exceeds {} byte merge cap",
              normalized,
              bytes.len(),
              maximum,
            )));
          }
        }
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
    self.finalize_file_with_content_hash(ctx, path, chunk_hashes, total_size, content_type, &first_bytes, content_hash)
  }

  /// Store a single data chunk and return its hash. Deduplicates automatically.
  /// Used by streaming upload to store chunks as they arrive without buffering.
  pub fn store_chunk(&self, data: &[u8]) -> EngineResult<Vec<u8>> {
    let _mem = PhaseSampler::start("store_chunk", std::time::Duration::from_millis(50));
    let algo = self.engine.hash_algo();
    let chunk_key = chunk_content_hash(data, &algo)?;
    if !validate_existing_chunk_locator(self.engine, "chunk staging", &chunk_key)? {
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
    self.finalize_file_with_content_hash(ctx, path, chunk_hashes, total_size, content_type, first_bytes, content_hash)
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

    let sys_flags = v0_system_entry_flags(&normalized);
    let detected_content_type = crate::engine::content_type::detect_content_type(first_bytes, content_type);
    let file_record = self.execute_file_publication(
      ctx,
      FileRecordPublishInput {
        normalized_path: normalized,
        content_type: Some(detected_content_type),
        total_size,
        metadata: Vec::new(),
        chunk_hashes,
        content_hash,
        flags: sys_flags,
        created_at_override: None,
        updated_at_override: None,
        prefer_existing_created_at: true,
      },
      CURRENT_FILE_RECORD_VERSION,
      total_size,
      true,
      true,
    )?;

    let elapsed = timer_start.elapsed().as_secs_f64();
    metrics::histogram!(crate::metrics::definitions::FILE_STORE_DURATION).record(elapsed);

    Ok(file_record)
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
  /// **Atomicity (M15)**: This method stores immutable chunks before one
  /// coordinated FileRecord, path-locator, parent-directory, and HEAD
  /// publication. A crash before publication can leave unreachable chunks,
  /// which normal GC may reclaim, but readers cannot observe a partially
  /// published namespace mutation.
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
        if !validate_existing_chunk_locator(self.engine, "compressed file chunk staging", &chunk_key)? {
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

    let total_size = data.len() as u64;
    self.execute_file_publication(
      ctx,
      FileRecordPublishInput {
        normalized_path: normalized,
        content_type: Some(detected_content_type),
        total_size,
        metadata: Vec::new(),
        chunk_hashes,
        content_hash: whole_file_content_hash(data, &algo)?,
        flags: sys_flags,
        created_at_override: None,
        updated_at_override: None,
        prefer_existing_created_at: true,
      },
      file_record_version,
      total_size,
      emit_event,
      false,
    )
  }

  /// Restore a file from an existing FileRecord without re-reading chunk data.
  /// The chunks must already exist in the database (e.g., from a historical snapshot).
  /// This avoids loading the entire file into memory for large file restores.
  pub fn restore_file_from_record(&self, ctx: &RequestContext, path: &str, source_record: &FileRecord) -> EngineResult<()> {
    self.execute_file_record_restore(ctx, path, FileRecordRestoreSource::Historical(source_record.clone()))?;
    Ok(())
  }

  /// Read a file as a streaming iterator of chunk data.
  pub fn read_file_streaming(&self, path: &str) -> EngineResult<EngineFileStream<'_>> {
    let timer_start = std::time::Instant::now();
    let normalized = normalize_path(path);
    let file_record = self.resolve_current_file_record(&normalized)?;

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

  /// Read a small file only after its declared size passes the caller's bound.
  pub(crate) fn read_file_buffered_bounded(&self, path: &str, maximum_bytes: u64) -> EngineResult<Vec<u8>> {
    self.read_file_buffered_bounded_with_identity(path, maximum_bytes).map(|(_identity, data)| data)
  }

  /// Resolve one current FileRecord once, then read its immutable chunks under
  /// a declared body bound. The returned identity can be rechecked by a later
  /// conditional namespace mutation without coupling the caller to FileRecord
  /// serialization details.
  pub(crate) fn read_file_buffered_bounded_with_identity(&self, path: &str, maximum_bytes: u64) -> EngineResult<(Vec<u8>, Vec<u8>)> {
    let timer_start = std::time::Instant::now();
    let normalized = normalize_path(path);
    let (identity, file_record) = self
      .resolve_current_file_record_from_bounded(self.engine, &normalized, SYSTEM_FILE_ALIAS_RECORD_MAX_BYTES)?
      .ok_or_else(|| EngineError::NotFound(normalized.clone()))?;
    if file_record.total_size > maximum_bytes {
      return Err(EngineError::ResourceExhausted(format!(
        "buffered read for '{normalized}' declares {} bytes, exceeding the {maximum_bytes}-byte limit",
        file_record.total_size,
      )));
    }
    self.engine.counters().record_read(file_record.total_size);
    metrics::histogram!(crate::metrics::definitions::FILE_READ_DURATION).record(timer_start.elapsed().as_secs_f64());
    let data = EngineFileStream::new_with_expected_total_size(file_record.chunk_hashes, self.engine, false, Some(file_record.total_size))?
      .collect_to_vec()?;
    Ok((identity, data))
  }

  /// Delete a file, storing a DeletionRecord and updating parent directories.
  /// Takes an auto-snapshot before delete (throttled to once per minute).
  pub fn delete_file(&self, ctx: &RequestContext, path: &str) -> EngineResult<()> {
    let normalized = normalize_path(path);

    // Verify the file exists FIRST — before auto-snapshot or any side-effect.
    // A delete of a nonexistent file must produce zero observable side-effects:
    // no auto-snapshot, no event, no counter changes.
    if self.resolve_current_file_record_from(self.engine, &normalized)?.is_none() {
      return Err(EngineError::NotFound(normalized));
    }

    // File confirmed to exist. Now take an auto-snapshot before mutating
    // (at most once per minute).
    if v0_system_entry_flags(&normalized) == 0 {
      self.auto_snapshot_before_delete(ctx);
    }

    let deleted =
      self.delete_files_batch_with_kind(ctx, vec![FileDeletionRequest::required(normalized.clone())], NamespaceMutationKind::FileDelete)?;
    if deleted.as_slice() != [normalized.as_str()] {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("single-file delete for '{normalized}' did not report exactly one deleted path"),
      });
    }
    Ok(())
  }

  /// Delete a related set of files under one namespace acknowledgement.
  ///
  /// At most one request may be marked [`FileDeletionRequirement::Primary`].
  /// If that primary is absent, no companion is inspected or deleted. This is
  /// useful for compound authorities such as a user and its managed group,
  /// where an absent owner preserves the existing `false`/no-op contract.
  pub(crate) fn delete_files_batch_with_kind(
    &self,
    context: &RequestContext,
    requests: Vec<FileDeletionRequest>,
    kind: NamespaceMutationKind,
  ) -> EngineResult<Vec<String>> {
    if requests.is_empty() {
      return Err(EngineError::InvalidInput("No files provided for batch deletion".to_string()));
    }

    let primary_count = requests.iter().filter(|request| request.requirement == FileDeletionRequirement::Primary).count();
    if primary_count > 1 {
      return Err(EngineError::InvalidInput("Batch deletion accepts at most one primary file".to_string()));
    }

    let mut normalized_requests = Vec::with_capacity(requests.len());
    let mut seen_paths = std::collections::HashSet::with_capacity(requests.len());
    for request in requests {
      let normalized = normalize_path(&request.path);
      if normalized == "/" {
        return Err(EngineError::InvalidInput("Cannot delete the root path as a file".to_string()));
      }
      if !seen_paths.insert(normalized.clone()) {
        return Err(EngineError::InvalidInput(format!("Duplicate batch deletion path: {normalized}")));
      }
      normalized_requests.push(FileDeletionRequest {
        path: normalized,
        requirement: request.requirement,
        expected_identity: request.expected_identity,
      });
    }

    self.execute_optional_namespace_mutation(Some(context), move |planning_engine| {
      if let Some(primary) = normalized_requests.iter().find(|request| request.requirement == FileDeletionRequirement::Primary) {
        let Some(reference) = self.current_entry_reference_from(planning_engine, &primary.path)? else {
          return Ok((None, Vec::new()));
        };
        if reference.entry_type != EntryType::FileRecord {
          return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: format!("batch deletion primary '{}' resolves to {:?} instead of FileRecord", primary.path, reference.entry_type),
          });
        }
      }

      let algorithm = planning_engine.hash_algo();
      let mut batch = NamespaceMutationBatch::new(kind);
      let mut planner = DirectoryMutationPlanner::default();
      let mut counter_delta = DirectoryMutationCounterDelta::default();
      let mut deleted_paths = Vec::with_capacity(normalized_requests.len());
      let mut deleted_events = Vec::with_capacity(normalized_requests.len());
      let mut metadata_index_removal_paths = Vec::with_capacity(normalized_requests.len());

      for request in &normalized_requests {
        let Some(reference) = self.current_entry_reference_from(planning_engine, &request.path)? else {
          match request.requirement {
            FileDeletionRequirement::Required => return Err(EngineError::NotFound(request.path.clone())),
            FileDeletionRequirement::Primary | FileDeletionRequirement::Optional => continue,
          }
        };
        if reference.entry_type != EntryType::FileRecord {
          return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: format!("batch deletion path '{}' resolves to {:?} instead of FileRecord", request.path, reference.entry_type),
          });
        }
        let (previous_identity, record) =
          self.resolve_current_file_record_from(planning_engine, &request.path)?.ok_or_else(|| EngineError::CorruptEntry {
            offset: 0,
            reason: format!("batch deletion FileRecord '{}' disappeared during authoritative planning", request.path),
          })?;
        if request.expected_identity.as_ref().is_some_and(|expected| expected != &previous_identity) {
          continue;
        }
        let file_key = file_path_hash(&request.path, &algorithm)?;
        let deletion = DeletionRecord::new(request.path.clone(), None);
        let deletion_key = deletion_record_hash(&request.path, deletion.deleted_at, &algorithm)?;
        batch.store_dependency(EntryType::DeletionRecord, deletion_key, deletion.serialize(), v0_system_entry_flags(&request.path))?;
        if planning_engine.has_entry(&file_key)? {
          batch.retire_locator(file_key)?;
        }
        batch.add_source_identity(NamespaceMutationSourceIdentity {
          path: request.path.clone(),
          entry_type: Some(EntryType::FileRecord.to_u8()),
          previous_identity: Some(previous_identity),
          new_identity: None,
        })?;
        planner.remove_child(&request.path)?;
        counter_delta.file_delete_sizes.push(record.total_size);
        metadata_index_removal_paths.push(request.path.clone());
        deleted_events.push(EntryEventData {
          path: request.path.clone(),
          entry_type: "file".to_string(),
          content_type: record.content_type.clone(),
          size: record.total_size,
          hash: record.content_hash_hex(),
          created_at: record.created_at,
          updated_at: record.updated_at,
          previous_hash: None,
        });
        deleted_paths.push(request.path.clone());
      }

      if deleted_paths.is_empty() {
        return Ok((None, deleted_paths));
      }
      let mut effects = DirectoryMutationEffects::new(DirectoryMutationCounterEffect::Aggregate(counter_delta));
      effects.metadata_index_removal_paths = metadata_index_removal_paths;
      effects.events.push((EVENT_ENTRIES_DELETED, serde_json::json!({ "entries": deleted_events })));
      planner.finalize(self, &mut batch, &mut effects)?;
      Ok((Some((batch, effects)), deleted_paths))
    })
  }

  /// Delete an empty directory. Returns an error if the directory has children.
  ///
  /// The emptiness check and deletion plan run under the same namespace
  /// authority, so a concurrent namespace writer cannot add a child between
  /// validation and the hard publication.
  pub fn delete_directory(&self, ctx: &RequestContext, path: &str) -> EngineResult<()> {
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();

    if normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot delete root directory".to_string()));
    }
    let dir_key = directory_path_hash(&normalized, &algo)?;
    self.execute_namespace_mutation(Some(ctx), move |planning_engine| {
      let children = self.list_directory(&normalized)?;
      if !children.is_empty() {
        return Err(EngineError::InvalidInput(format!("Directory '{}' is not empty ({} children)", normalized, children.len())));
      }
      let (previous_identity, _header, _directory_value) =
        self.resolve_current_directory_data_from(planning_engine, &normalized)?.ok_or_else(|| EngineError::NotFound(normalized.clone()))?;
      let deletion = DeletionRecord::new(normalized.clone(), None);
      let deletion_key = deletion_record_hash(&normalized, deletion.deleted_at, &algo)?;
      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::DirectoryDelete);
      batch.store_dependency(EntryType::DeletionRecord, deletion_key, deletion.serialize(), v0_system_entry_flags(&normalized))?;
      if planning_engine.has_entry(&dir_key)? {
        batch.retire_locator(dir_key.clone())?;
      }
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: normalized.clone(),
        entry_type: Some(EntryType::DirectoryIndex.to_u8()),
        previous_identity: Some(previous_identity),
        new_identity: None,
      })?;
      let mut effects = DirectoryMutationEffects::new(DirectoryMutationCounterEffect::DirectoryDelete);
      self.plan_remove_from_parent_directory(&mut batch, &normalized, &mut effects)?;
      effects.events.push((EVENT_ENTRIES_DELETED, serde_json::json!({"entries": [{"path": normalized, "entry_type": "directory"}]})));
      Ok((batch, (), effects))
    })
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
    self.visit_live_directory_children_with_mode(path, crate::engine::btree::BTreeWalkMode::BestEffort, true, &mut visitor)
  }

  pub(crate) fn visit_live_directory_children_strict<F>(&self, path: &str, mut visitor: F) -> EngineResult<bool>
  where
    F: FnMut(&ChildEntry) -> EngineResult<bool>,
  {
    self.visit_live_directory_children_with_mode(path, crate::engine::btree::BTreeWalkMode::Strict, true, &mut visitor)
  }

  fn visit_live_directory_children_strict_no_heal<F>(&self, path: &str, mut visitor: F) -> EngineResult<bool>
  where
    F: FnMut(&ChildEntry) -> EngineResult<bool>,
  {
    self.visit_live_directory_children_with_mode(path, crate::engine::btree::BTreeWalkMode::Strict, false, &mut visitor)
  }

  fn visit_live_directory_children_with_mode<F>(
    &self,
    path: &str,
    mode: crate::engine::btree::BTreeWalkMode,
    heal_stale: bool,
    visitor: &mut F,
  ) -> EngineResult<bool>
  where
    F: FnMut(&ChildEntry) -> EngineResult<bool>,
  {
    let normalized = normalize_path(path);
    let hash_length = self.engine.hash_algo().hash_length();
    let Some((header, value)) = self.load_directory_listing_data_inner(&normalized, heal_stale)? else {
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
    _heal_stale: bool,
  ) -> EngineResult<Option<(crate::engine::entry_header::EntryHeader, Vec<u8>)>> {
    Ok(self.resolve_current_directory_data_from(self.engine, normalized)?.map(|(_identity, header, value)| (header, value)))
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
    let child_path = if parent == "/" { format!("/{}", child.name) } else { format!("{}/{}", parent, child.name) };
    let expected_type = EntryType::from_u8(child.entry_type)?;
    let Some(header) = self.engine.get_entry_header(&child.hash)? else {
      let error = EngineError::CorruptEntry {
        offset: 0,
        reason: format!("directory '{parent}' contains child '{child_path}' whose root-selected content is missing"),
      };
      if mode == crate::engine::btree::BTreeWalkMode::Strict {
        return Err(error);
      }
      tracing::warn!(parent = %parent, child_path = %child_path, entry_type = child.entry_type, error = %error, "Skipping directory child with missing root-selected content");
      return Ok(false);
    };
    if header.entry_type != expected_type {
      let error = EngineError::CorruptEntry {
        offset: 0,
        reason: format!("directory '{parent}' child '{child_path}' resolves to {:?} instead of {expected_type:?}", header.entry_type),
      };
      if mode == crate::engine::btree::BTreeWalkMode::Strict {
        return Err(error);
      }
      tracing::warn!(parent = %parent, child_path = %child_path, entry_type = child.entry_type, error = %error, "Skipping directory child with wrong root-selected content type");
      return Ok(false);
    }
    Ok(true)
  }

  /// Create an empty directory at the given path.
  pub fn create_directory(&self, ctx: &RequestContext, path: &str) -> EngineResult<()> {
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();
    let dir_key = directory_path_hash(&normalized, &algo)?;
    let content_key = directory_content_hash(&[], &algo)?;
    let now = chrono::Utc::now().timestamp_millis();
    self.execute_namespace_mutation(Some(ctx), move |planning_engine| {
      let previous_identity =
        self.resolve_current_directory_data_from(planning_engine, &normalized)?.map(|(identity, _header, _value)| identity);
      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::DirectoryCreate);
      batch.store_dependency(EntryType::DirectoryIndex, content_key.clone(), Vec::new(), 0)?;
      batch.replace_locator(EntryType::DirectoryIndex, dir_key.clone(), content_key.clone(), 0)?;
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: normalized.clone(),
        entry_type: Some(EntryType::DirectoryIndex.to_u8()),
        previous_identity,
        new_identity: Some(content_key.clone()),
      })?;
      let mut effects = DirectoryMutationEffects::new(DirectoryMutationCounterEffect::DirectoryCreate);
      effects.cache_writes.push((content_key.clone(), Vec::new()));
      if normalized != "/" {
        self.plan_parent_directories(
          &mut batch,
          &normalized,
          ChildEntry {
            entry_type: EntryType::DirectoryIndex.to_u8(),
            hash: content_key.clone(),
            total_size: 0,
            created_at: now,
            updated_at: now,
            name: file_name(&normalized).unwrap_or("").to_string(),
            content_type: None,
            virtual_time: now as u64,
            node_id: 0,
          },
          &mut effects,
        )?;
      }
      effects.events.push((
        EVENT_ENTRIES_CREATED,
        serde_json::json!({"entries": [EntryEventData {
          path: normalized.clone(),
          entry_type: "directory".to_string(),
          content_type: None,
          size: 0,
          hash: String::new(),
          created_at: now,
          updated_at: now,
          previous_hash: None,
        }]}),
      ));
      Ok((batch, (), effects))
    })
  }

  /// Get the FileRecord metadata for a file path.
  pub fn get_metadata(&self, path: &str) -> EngineResult<Option<FileRecord>> {
    let normalized = normalize_path(path);
    Ok(self.resolve_current_file_record_from(self.engine, &normalized)?.map(|(_identity, record)| record))
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
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();
    let file_key = file_path_hash(&normalized, &algo)?;
    self.execute_optional_namespace_mutation(None, move |planning_engine| {
      let path_entry = planning_engine.get_kv_entry(&file_key)?.ok_or_else(|| EngineError::NotFound(normalized.clone()))?;
      let record_workspace = u64::from(path_entry.total_length)
        .checked_mul(3)
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<FileRecord>() as u64))
        .ok_or_else(|| EngineError::ResourceExhausted("file record migration record estimate overflow".to_string()))?;
      memory.reserve(record_workspace, "file record migration record admission failed")?;
      let (path_header, _stored_key, value) =
        planning_engine.get_entry(&file_key)?.ok_or_else(|| EngineError::NotFound(normalized.clone()))?;
      let mut record = FileRecord::deserialize(&value, hash_length, path_header.entry_version)?;
      let mut needs_migration = path_header.entry_version < CURRENT_FILE_RECORD_VERSION;
      if record.path != normalized {
        record.path = normalized.clone();
        needs_migration = true;
      }
      if record.content_hash.len() != hash_length {
        needs_migration = true;
      }
      ensure_file_record_content_hash_for_migration(planning_engine, &mut record, memory)?;
      let identity_key = file_identity_hash(&normalized, record.content_type.as_deref(), &record.chunk_hashes, &algo)?;
      if file_record_header_needs_migration(planning_engine, &identity_key)? {
        needs_migration = true;
      }
      let file_value = record.serialize(hash_length)?;
      let content_key = file_content_hash(&file_value, &algo)?;
      if file_record_header_needs_migration(planning_engine, &content_key)? {
        needs_migration = true;
      }
      if !needs_migration {
        return Ok((None, false));
      }

      let prepared =
        prepare_file_record_entries_at_version(planning_engine, &normalized, &mut record, path_header.flags, CURRENT_FILE_RECORD_VERSION)?;
      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::MaintenanceRepair);
      add_prepared_file_record_entries(&mut batch, &prepared)?;
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: normalized,
        entry_type: Some(EntryType::FileRecord.to_u8()),
        previous_identity: Some(identity_key),
        new_identity: Some(prepared.identity_key.clone()),
      })?;
      Ok((Some((batch, DirectoryMutationEffects::new(DirectoryMutationCounterEffect::None))), true))
    })
  }

  /// Check if a file or directory exists at the given path.
  pub fn exists(&self, path: &str) -> EngineResult<bool> {
    let normalized = normalize_path(path);
    Ok(matches!(
      self.current_entry_reference_from(self.engine, &normalized)?.map(|reference| reference.entry_type),
      Some(EntryType::FileRecord | EntryType::DirectoryIndex)
    ))
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
  fn auto_snapshot_throttled(&self, lane: &std::sync::atomic::AtomicI64, throttle_ms: i64, prefix: &str) {
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
  fn auto_snapshot_before_delete(&self, _ctx: &RequestContext) {
    self.auto_snapshot_throttled(&self.engine.last_auto_snapshot_delete, 60_000, "auto-pre-delete");
  }

  /// Auto-snapshot before restore — own lane, 60s throttle.
  pub fn auto_snapshot_before_restore(&self, _ctx: &RequestContext) {
    self.auto_snapshot_throttled(&self.engine.last_auto_snapshot_restore, 60_000, "auto-pre-restore");
  }

  /// Restore a deleted file by un-marking it in the KV and re-adding
  /// it to its parent directory.
  pub fn restore_deleted_file(&self, ctx: &RequestContext, path: &str) -> EngineResult<()> {
    self.execute_file_record_restore(ctx, path, FileRecordRestoreSource::DeletedLocator)?;
    Ok(())
  }

  /// Ensure the root directory exists. Called during database creation.
  pub fn ensure_root_directory(&self, _ctx: &RequestContext) -> EngineResult<()> {
    let algo = self.engine.hash_algo();
    let dir_key = directory_path_hash("/", &algo)?;
    let content_key = directory_content_hash(&[], &algo)?;
    self.execute_optional_namespace_mutation(None, move |planning_engine| {
      // Existing root state is never replaced here, even when unreadable.
      // Startup repair owns recovery; silently recreating root would discard
      // the only authoritative link to a live directory tree.
      if planning_engine.has_entry(&dir_key)? {
        match self.list_directory_window_strict("/", 0, 1) {
          Ok(_) => {}
          Err(error) => tracing::warn!(
            %error,
            "Root directory exists but is not completely readable. Run 'aeordb verify --repair' to recover."
          ),
        }
        return Ok((None, ()));
      }

      let head_hash = planning_engine.head_hash()?;
      if !head_hash.is_empty() && !head_hash.iter().all(|byte| *byte == 0) {
        tracing::warn!(
          head_hash = %hex::encode(&head_hash),
          "Root directory locator is missing while namespace authority remains. Run 'aeordb verify --repair' to recover."
        );
        return Ok((None, ()));
      }

      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::DirectoryCreate);
      batch.store_dependency(EntryType::DirectoryIndex, content_key.clone(), Vec::new(), 0)?;
      batch.replace_locator(EntryType::DirectoryIndex, dir_key, Vec::new(), 0)?;
      batch.set_incremental_head_hash(content_key.clone());
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: "/".to_string(),
        entry_type: Some(EntryType::DirectoryIndex.to_u8()),
        previous_identity: None,
        new_identity: Some(content_key),
      })?;
      Ok((Some((batch, DirectoryMutationEffects::new(DirectoryMutationCounterEffect::None))), ()))
    })
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
    let _namespace = self.engine.namespace_write_guard()?;
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
      let mut cursor = workspace
        .finish_depth(depth)
        .map_err(|error| directory_repair_failure(error, dirs_written, "finish_depth", &format!("depth:{depth}")))?;
      loop {
        let next_group = cursor
          .next_group(&mut workspace)
          .map_err(|error| directory_repair_failure(error, dirs_written, "read_group", &format!("depth:{depth}")))?;
        let Some((dir_path, mut children)) = next_group else {
          break;
        };
        Self::sort_rebuilt_children(&mut children);
        let store_result = self.store_rebuilt_directory(&dir_path, children, hash_length, &algo, false);
        let release_result = workspace.release_group();
        let (content_key, dir_size) = match store_result {
          Ok(stored) => {
            dirs_written += 1;
            if let Err(error) = release_result {
              return Err(directory_repair_failure(error, dirs_written, "release_group", &dir_path));
            }
            stored
          }
          Err(error) => {
            if let Err(cleanup_error) = release_result {
              metrics::counter!("aeordb_directory_repair_cleanup_failures_total").increment(1);
              tracing::error!(path = %dir_path, %cleanup_error, "Directory repair group cleanup also failed after the primary publication failure");
            }
            return Err(directory_repair_failure(error, dirs_written, "publish_directory", &dir_path));
          }
        };

        #[cfg(test)]
        inject_directory_repair_failure(dirs_written)
          .map_err(|error| directory_repair_failure(error, dirs_written, "post_publication", &dir_path))?;

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
          workspace.push_child(&parent, child).map_err(|error| directory_repair_failure(error, dirs_written, "queue_parent", &parent))?;
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
    let _namespace = self.engine.namespace_write_guard()?;
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
    self.store_rebuilt_directory(&normalized, children, hash_length, &algo, true)?;

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
      if !validate_existing_chunk_locator(self.engine, &format!("file '{}'", record.path), chunk_hash)? {
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
    propagate_parent: bool,
  ) -> EngineResult<(Vec<u8>, u64)> {
    let normalized = normalize_path(dir_path);
    let dir_key = directory_path_hash(&normalized, algo)?;
    self.execute_namespace_mutation(None, move |_planning_engine| {
      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::MaintenanceRepair);
      let (dir_value, content_key) = if children.len() >= crate::engine::btree::BTREE_CONVERSION_THRESHOLD {
        let plan = crate::engine::btree::btree_plan_from_entries(children, hash_length, algo)?;
        Self::add_btree_plan_dependencies(&mut batch, &plan)?;
        (plan.root_data().to_vec(), plan.root_hash().to_vec())
      } else {
        let dir_value = serialize_child_entries(&children, hash_length)?;
        let content_key = directory_content_hash(&dir_value, algo)?;
        batch.store_dependency(EntryType::DirectoryIndex, content_key.clone(), dir_value.clone(), 0)?;
        (dir_value, content_key)
      };
      let dir_size = dir_value.len() as u64;
      let mut effects = DirectoryMutationEffects::new(DirectoryMutationCounterEffect::None);
      effects.cache_writes.push((content_key.clone(), dir_value));
      batch.replace_locator(EntryType::DirectoryIndex, dir_key, content_key.clone(), 0)?;
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: normalized.clone(),
        entry_type: Some(EntryType::DirectoryIndex.to_u8()),
        previous_identity: None,
        new_identity: Some(content_key.clone()),
      })?;

      if normalized == "/" {
        batch.set_incremental_head_hash(content_key.clone());
      } else if propagate_parent {
        let now_ms = chrono::Utc::now().timestamp_millis();
        self.plan_parent_directories(
          &mut batch,
          &normalized,
          ChildEntry {
            name: file_name(&normalized).unwrap_or("").to_string(),
            entry_type: EntryType::DirectoryIndex.to_u8(),
            hash: content_key.clone(),
            total_size: dir_size,
            content_type: None,
            created_at: now_ms,
            updated_at: now_ms,
            virtual_time: now_ms as u64,
            node_id: 0,
          },
          &mut effects,
        )?;
      }

      Ok((batch, (content_key, dir_size), effects))
    })
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
    crate::engine::index_cleanup::remove_file_from_resolved_indexes(self.engine, &normalized)?;
    self.delete_file(ctx, path)
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

  /// Repair a stale dir_key by rewriting it to hard-link the canonical
  /// content hash from HEAD's merkle walk. Handles both dead-target
  /// (post-GC) and diverged-target (alive but != HEAD) scenarios.
  /// Returns Ok(true) if a write happened, Ok(false) otherwise.
  pub fn repair_stale_dir_key(&self, path: &str) -> EngineResult<bool> {
    let normalized = normalize_path(path);
    let log_path = normalized.clone();
    let repaired = self.execute_optional_namespace_mutation(None, move |planning_engine| {
      let algo = planning_engine.hash_algo();
      let dir_key = directory_path_hash(&normalized, &algo)?;
      let Some((_header, _key, current_target)) = planning_engine.get_entry(&dir_key)? else {
        return Ok((None, false));
      };
      if current_target.len() != algo.hash_length() {
        return Ok((None, false));
      }
      let Some(canonical_target) = self.canonical_directory_content_hash(&normalized)? else {
        return Ok((None, false));
      };
      if canonical_target == current_target {
        return Ok((None, false));
      }

      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::MaintenanceRepair);
      batch.replace_locator(EntryType::DirectoryIndex, dir_key, canonical_target.clone(), 0)?;
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: normalized,
        entry_type: Some(EntryType::DirectoryIndex.to_u8()),
        previous_identity: Some(current_target),
        new_identity: Some(canonical_target),
      })?;
      Ok((Some((batch, DirectoryMutationEffects::new(DirectoryMutationCounterEffect::None))), true))
    })?;
    if repaired {
      tracing::info!(path = %log_path, "Repaired stale dir_key hard-link through maintenance authority");
    }
    Ok(repaired)
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

  /// Maximum directory depth for parent-directory mutation planning.
  /// Prevents unbounded looping on pathologically deep paths.
  const MAX_DIRECTORY_DEPTH: usize = 1000;

  fn add_btree_plan_dependencies(batch: &mut NamespaceMutationBatch, plan: &crate::engine::btree::BTreeMutationPlan) -> EngineResult<()> {
    for write in plan.node_writes() {
      batch.store_dependency(EntryType::DirectoryIndex, write.key.clone(), write.value.clone(), 0)?;
    }
    Ok(())
  }

  fn plan_parent_directories(
    &self,
    batch: &mut NamespaceMutationBatch,
    child_path: &str,
    child_entry: ChildEntry,
    effects: &mut DirectoryMutationEffects,
  ) -> EngineResult<()> {
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();
    let mut current_child_path = child_path.to_string();
    let mut current_child_entry = child_entry;

    for _depth in 0..Self::MAX_DIRECTORY_DEPTH {
      let Some(parent) = parent_path(&current_child_path) else {
        return Ok(());
      };
      if parent == "/" && v0_is_detached_system_path(&current_child_path) {
        return Ok(());
      }

      let dir_key = directory_path_hash(&parent, &algo)?;
      if parent != "/" {
        if let Some(reference) = self.current_entry_reference_from(self.engine, &parent)? {
          if reference.entry_type != EntryType::DirectoryIndex {
            return Err(EngineError::AlreadyExists(parent));
          }
        }
      }
      let existing = self.resolve_current_directory_data_from(self.engine, &parent)?.map(|(_identity, header, value)| (header, value));

      let (dir_value, content_key) = match existing {
        Some((header, value)) if !value.is_empty() && crate::engine::btree::is_btree_format(&value) => {
          let root = crate::engine::btree::BTreeNode::deserialize(&value, hash_length, header.entry_version)?;
          let root_hash = root.content_hash(hash_length, &algo)?;
          if let Some(existing_child) =
            crate::engine::btree::btree_lookup(self.engine, &root_hash, &current_child_entry.name, hash_length, false)?
          {
            if existing_child.entry_type != current_child_entry.entry_type {
              return Err(EngineError::AlreadyExists(current_child_path));
            }
          }
          let plan = crate::engine::btree::btree_plan_insert(self.engine, &value, current_child_entry, hash_length, &algo)?;
          Self::add_btree_plan_dependencies(batch, &plan)?;
          (plan.root_data().to_vec(), plan.root_hash().to_vec())
        }
        Some((header, value)) => {
          let mut children =
            if value.is_empty() { Vec::new() } else { deserialize_child_entries(&value, hash_length, header.entry_version)? };
          let child_name = &current_child_entry.name;
          if let Some(existing_child) = children.iter_mut().find(|child| child.name == *child_name) {
            if existing_child.entry_type != current_child_entry.entry_type {
              return Err(EngineError::AlreadyExists(current_child_path));
            }
            *existing_child = current_child_entry;
          } else {
            children.push(current_child_entry);
          }
          if children.len() >= crate::engine::btree::BTREE_CONVERSION_THRESHOLD {
            let plan = crate::engine::btree::btree_plan_from_entries(children, hash_length, &algo)?;
            Self::add_btree_plan_dependencies(batch, &plan)?;
            (plan.root_data().to_vec(), plan.root_hash().to_vec())
          } else {
            let dir_value = serialize_child_entries(&children, hash_length)?;
            let content_key = directory_content_hash(&dir_value, &algo)?;
            batch.store_dependency(EntryType::DirectoryIndex, content_key.clone(), dir_value.clone(), 0)?;
            (dir_value, content_key)
          }
        }
        None => {
          effects.implicit_directories = effects
            .implicit_directories
            .checked_add(1)
            .ok_or_else(|| EngineError::ResourceExhausted("implicit directory count overflow".to_string()))?;
          let dir_value = serialize_child_entries(&[current_child_entry], hash_length)?;
          let content_key = directory_content_hash(&dir_value, &algo)?;
          batch.store_dependency(EntryType::DirectoryIndex, content_key.clone(), dir_value.clone(), 0)?;
          (dir_value, content_key)
        }
      };

      effects.cache_writes.push((content_key.clone(), dir_value.clone()));
      batch.replace_locator(EntryType::DirectoryIndex, dir_key, content_key.clone(), 0)?;
      if parent == "/" {
        batch.set_incremental_head_hash(content_key);
        return Ok(());
      }

      let now_ms = chrono::Utc::now().timestamp_millis();
      current_child_entry = ChildEntry {
        entry_type: EntryType::DirectoryIndex.to_u8(),
        hash: content_key,
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

  fn plan_remove_from_parent_directory(
    &self,
    batch: &mut NamespaceMutationBatch,
    child_path: &str,
    effects: &mut DirectoryMutationEffects,
  ) -> EngineResult<()> {
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();
    let Some(parent) = parent_path(child_path) else {
      return Ok(());
    };
    let dir_key = directory_path_hash(&parent, &algo)?;
    let child_name = file_name(child_path).unwrap_or("").to_string();
    let existing = self.resolve_current_directory_data_from(self.engine, &parent)?.map(|(_identity, header, value)| (header, value));

    let (dir_value, content_key) = match existing {
      Some((header, value)) if !value.is_empty() && crate::engine::btree::is_btree_format(&value) => {
        let root_node = crate::engine::btree::BTreeNode::deserialize(&value, hash_length, header.entry_version)?;
        let root_hash = root_node.content_hash(hash_length, &algo)?;
        match crate::engine::btree::btree_plan_delete(self.engine, &root_hash, &child_name, hash_length, &algo)? {
          Some(plan) => {
            Self::add_btree_plan_dependencies(batch, &plan)?;
            (plan.root_data().to_vec(), plan.root_hash().to_vec())
          }
          None => {
            let dir_value = Vec::new();
            let content_key = directory_content_hash(&dir_value, &algo)?;
            batch.store_dependency(EntryType::DirectoryIndex, content_key.clone(), dir_value.clone(), 0)?;
            (dir_value, content_key)
          }
        }
      }
      Some((header, value)) => {
        let mut children =
          if value.is_empty() { Vec::new() } else { deserialize_child_entries(&value, hash_length, header.entry_version)? };
        children.retain(|child| child.name != child_name);
        let dir_value = serialize_child_entries(&children, hash_length)?;
        let content_key = directory_content_hash(&dir_value, &algo)?;
        batch.store_dependency(EntryType::DirectoryIndex, content_key.clone(), dir_value.clone(), 0)?;
        (dir_value, content_key)
      }
      None => {
        let dir_value = Vec::new();
        let content_key = directory_content_hash(&dir_value, &algo)?;
        batch.store_dependency(EntryType::DirectoryIndex, content_key.clone(), dir_value.clone(), 0)?;
        (dir_value, content_key)
      }
    };

    effects.cache_writes.push((content_key.clone(), dir_value.clone()));
    batch.replace_locator(EntryType::DirectoryIndex, dir_key, content_key.clone(), 0)?;
    if parent == "/" {
      batch.set_incremental_head_hash(content_key);
      return Ok(());
    }

    let now_ms = chrono::Utc::now().timestamp_millis();
    self.plan_parent_directories(
      batch,
      &parent,
      ChildEntry {
        entry_type: EntryType::DirectoryIndex.to_u8(),
        hash: content_key,
        total_size: dir_value.len() as u64,
        created_at: now_ms,
        updated_at: now_ms,
        name: file_name(&parent).unwrap_or("").to_string(),
        content_type: None,
        virtual_time: now_ms as u64,
        node_id: 0,
      },
      effects,
    )
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
    let symlink_key = symlink_path_hash(&normalized, &algo)?;
    self.execute_namespace_mutation(Some(ctx), move |planning_engine| {
      let (existing_created_at, previous_identity) = match self.resolve_current_symlink_record_from(planning_engine, &normalized)? {
        Some((identity, existing)) => (Some(existing.created_at), Some(identity)),
        None => (None, None),
      };
      let mut record = SymlinkRecord::new(normalized.clone(), normalized_target.clone());
      if let Some(original_created_at) = existing_created_at {
        record.created_at = original_created_at;
      }
      let serialized = record.serialize()?;
      let content_key = symlink_content_hash(&serialized, &algo)?;
      let identity_key = symlink_identity_hash(&normalized, &record.target, &algo)?;
      let sys_flags = v0_system_entry_flags(&normalized);
      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::SymlinkWrite);
      batch.store_dependency(EntryType::Symlink, content_key, serialized.clone(), sys_flags)?;
      batch.store_dependency(EntryType::Symlink, identity_key.clone(), serialized.clone(), sys_flags)?;
      batch.replace_locator(EntryType::Symlink, symlink_key.clone(), serialized, sys_flags)?;
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: normalized.clone(),
        entry_type: Some(EntryType::Symlink.to_u8()),
        previous_identity,
        new_identity: Some(identity_key.clone()),
      })?;
      let mut effects =
        DirectoryMutationEffects::new(DirectoryMutationCounterEffect::SymlinkWrite { existed: existing_created_at.is_some() });
      self.plan_parent_directories(
        &mut batch,
        &normalized,
        ChildEntry {
          entry_type: EntryType::Symlink.to_u8(),
          hash: identity_key,
          total_size: 0,
          created_at: record.created_at,
          updated_at: record.updated_at,
          name: file_name(&normalized).unwrap_or("").to_string(),
          content_type: None,
          virtual_time: chrono::Utc::now().timestamp_millis() as u64,
          node_id: 0,
        },
        &mut effects,
      )?;
      effects.events.push((
        EVENT_ENTRIES_CREATED,
        serde_json::json!({"entries": [EntryEventData {
          path: normalized.clone(),
          entry_type: "symlink".to_string(),
          content_type: None,
          size: 0,
          hash: hex::encode(&record.target),
          created_at: record.created_at,
          updated_at: record.updated_at,
          previous_hash: None,
        }]}),
      ));
      Ok((batch, record, effects))
    })
  }

  /// Read a SymlinkRecord at the given path, or None if not found.
  pub fn get_symlink(&self, path: &str) -> EngineResult<Option<SymlinkRecord>> {
    let normalized = normalize_path(path);
    Ok(self.resolve_current_symlink_record_from(self.engine, &normalized)?.map(|(_identity, record)| record))
  }

  fn path_uses_namespace_root_from(engine: &StorageEngine, path: &str) -> EngineResult<bool> {
    Ok(matches!(SystemFamilyPolicyResolver::new(engine.hash_algo())?.classify_path(path)?, SystemFamilyClassificationV1::Ordinary))
  }

  fn current_entry_reference_from(&self, engine: &StorageEngine, normalized: &str) -> EngineResult<Option<CurrentEntryReference>> {
    if Self::path_uses_namespace_root_from(engine, normalized)? {
      let head_hash = engine.head_hash()?;
      let (hash, entry_type) = match crate::engine::version_access::resolve_entry_reference_at_version(engine, &head_hash, normalized) {
        Ok(reference) => reference,
        Err(EngineError::NotFound(_)) => return Ok(None),
        Err(error) => return Err(error),
      };
      let header = engine
        .get_entry_header_including_deleted(&hash)?
        .ok_or_else(|| EngineError::CorruptEntry { offset: 0, reason: format!("HEAD-selected path '{normalized}' has no entity body") })?;
      if header.entry_type != entry_type {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("HEAD-selected path '{normalized}' resolves to {:?} instead of {entry_type:?}", header.entry_type),
        });
      }
      return Ok(Some(CurrentEntryReference { hash, entry_type, root_selected: true }));
    }

    let algorithm = engine.hash_algo();
    let candidates = [
      (file_path_hash(normalized, &algorithm)?, EntryType::FileRecord),
      (directory_path_hash(normalized, &algorithm)?, EntryType::DirectoryIndex),
      (symlink_path_hash(normalized, &algorithm)?, EntryType::Symlink),
    ];
    let mut selected = None;
    for (hash, entry_type) in candidates {
      let Some(entry) = engine.get_kv_entry(&hash)? else {
        continue;
      };
      if entry.is_deleted() {
        continue;
      }
      if entry.entry_type() != entry_type.to_kv_type() {
        return Err(EngineError::CorruptEntry {
          offset: entry.offset,
          reason: format!("detached path '{normalized}' locator resolves to the wrong entry type"),
        });
      }
      if selected.is_some() {
        return Err(EngineError::CorruptEntry {
          offset: entry.offset,
          reason: format!("detached path '{normalized}' has multiple live locator authorities"),
        });
      }
      selected = Some(CurrentEntryReference { hash, entry_type, root_selected: false });
    }
    Ok(selected)
  }

  pub(crate) fn resolve_current_entry_identity_from(
    &self,
    engine: &StorageEngine,
    path: &str,
  ) -> EngineResult<Option<(EntryType, Vec<u8>)>> {
    let normalized = normalize_path(path);
    let Some(reference) = self.current_entry_reference_from(engine, &normalized)? else {
      return Ok(None);
    };
    match reference.entry_type {
      EntryType::FileRecord => {
        Ok(self.resolve_current_file_record_from(engine, &normalized)?.map(|(identity, _record)| (EntryType::FileRecord, identity)))
      }
      EntryType::DirectoryIndex => Ok(
        self
          .resolve_current_directory_data_from(engine, &normalized)?
          .map(|(identity, _header, _value)| (EntryType::DirectoryIndex, identity)),
      ),
      EntryType::Symlink => {
        Ok(self.resolve_current_symlink_record_from(engine, &normalized)?.map(|(identity, _record)| (EntryType::Symlink, identity)))
      }
      other => Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("Current path '{normalized}' resolves to unsupported namespace entry type {other:?}"),
      }),
    }
  }

  pub(crate) fn resolve_live_locator_identity_from(
    &self,
    engine: &StorageEngine,
    path: &str,
    entry_type: EntryType,
  ) -> EngineResult<Option<Vec<u8>>> {
    let normalized = normalize_path(path);
    let algorithm = engine.hash_algo();
    let locator_key = match entry_type {
      EntryType::FileRecord => file_path_hash(&normalized, &algorithm)?,
      EntryType::Symlink => symlink_path_hash(&normalized, &algorithm)?,
      other => {
        return Err(EngineError::InvalidInput(format!("Live locator identity resolution does not support {other:?} at '{normalized}'")))
      }
    };
    let Some((header, stored_key, value)) = engine.get_entry_verified(&locator_key)? else {
      return Ok(None);
    };
    if stored_key != locator_key || header.entry_type != entry_type {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("Live {:?} locator '{normalized}' resolves to the wrong record", entry_type),
      });
    }
    match entry_type {
      EntryType::FileRecord => {
        let record = FileRecord::deserialize(&value, algorithm.hash_length(), header.entry_version)?;
        if record.path != normalized {
          return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: format!("Live FileRecord locator path '{}' does not match '{normalized}'", record.path),
          });
        }
        file_identity_hash(&normalized, record.content_type.as_deref(), &record.chunk_hashes, &algorithm).map(Some)
      }
      EntryType::Symlink => {
        let record = SymlinkRecord::deserialize(&value, header.entry_version)?;
        if record.path != normalized {
          return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: format!("Live Symlink locator path '{}' does not match '{normalized}'", record.path),
          });
        }
        symlink_identity_hash(&normalized, &record.target, &algorithm).map(Some)
      }
      _ => unreachable!("entry type was constrained above"),
    }
  }

  fn sync_delete_target_matches(&self, engine: &StorageEngine, normalized: &str, expected: EntryType) -> EngineResult<bool> {
    let Some(reference) = self.current_entry_reference_from(engine, normalized)? else {
      return Ok(false);
    };
    if reference.entry_type != expected {
      return Err(EngineError::InvalidInput(format!(
        "sync delete expected '{}' to be {:?}, but the selected namespace contains {:?}",
        normalized, expected, reference.entry_type
      )));
    }
    Ok(true)
  }

  fn resolve_current_file_record_from(&self, engine: &StorageEngine, normalized: &str) -> EngineResult<Option<(Vec<u8>, FileRecord)>> {
    self.resolve_current_file_record_from_bounded(engine, normalized, u32::MAX)
  }

  fn resolve_current_file_record_from_bounded(
    &self,
    engine: &StorageEngine,
    normalized: &str,
    maximum_value_length: u32,
  ) -> EngineResult<Option<(Vec<u8>, FileRecord)>> {
    let Some(reference) = self.current_entry_reference_from(engine, normalized)? else {
      return Ok(None);
    };
    if reference.entry_type != EntryType::FileRecord {
      return Ok(None);
    }
    let entry_result = if reference.root_selected {
      engine.get_entry_including_deleted_verified_bounded(&reference.hash, maximum_value_length)
    } else {
      engine.get_entry_verified_bounded(&reference.hash, maximum_value_length)
    };
    let entry = entry_result
      .map_err(|error| match error {
        EngineError::InvalidInput(reason) if reason.contains("exceeds caller bound") => {
          EngineError::ResourceExhausted(format!("FileRecord '{normalized}' exceeds the {maximum_value_length}-byte read bound: {reason}"))
        }
        other => other,
      })?
      .ok_or_else(|| EngineError::CorruptEntry { offset: 0, reason: format!("current file '{normalized}' lost its selected record") })?;
    let (header, stored_key, value) = entry;
    if stored_key != reference.hash || header.entry_type != EntryType::FileRecord {
      return Err(EngineError::CorruptEntry { offset: 0, reason: format!("current file '{normalized}' resolved to the wrong record") });
    }
    let record = FileRecord::deserialize(&value, engine.hash_algo().hash_length(), header.entry_version)?;
    if record.path != normalized {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("current FileRecord path '{}' does not match '{normalized}'", record.path),
      });
    }
    let identity = if reference.root_selected {
      reference.hash
    } else {
      file_identity_hash(normalized, record.content_type.as_deref(), &record.chunk_hashes, &engine.hash_algo())?
    };
    Ok(Some((identity, record)))
  }

  fn resolve_current_symlink_record_from(
    &self,
    engine: &StorageEngine,
    normalized: &str,
  ) -> EngineResult<Option<(Vec<u8>, SymlinkRecord)>> {
    let Some(reference) = self.current_entry_reference_from(engine, normalized)? else {
      return Ok(None);
    };
    if reference.entry_type != EntryType::Symlink {
      return Ok(None);
    }
    let entry = if reference.root_selected {
      engine.get_entry_verified_including_deleted(&reference.hash)?
    } else {
      engine.get_entry_verified(&reference.hash)?
    }
    .ok_or_else(|| EngineError::CorruptEntry { offset: 0, reason: format!("current symlink '{normalized}' lost its selected record") })?;
    let (header, stored_key, value) = entry;
    if stored_key != reference.hash || header.entry_type != EntryType::Symlink {
      return Err(EngineError::CorruptEntry { offset: 0, reason: format!("current symlink '{normalized}' resolved to the wrong record") });
    }
    let record = SymlinkRecord::deserialize(&value, header.entry_version)?;
    if record.path != normalized {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("current Symlink path '{}' does not match '{normalized}'", record.path),
      });
    }
    let identity =
      if reference.root_selected { reference.hash } else { symlink_identity_hash(normalized, &record.target, &engine.hash_algo())? };
    Ok(Some((identity, record)))
  }

  fn resolve_current_directory_data_from(
    &self,
    engine: &StorageEngine,
    normalized: &str,
  ) -> EngineResult<Option<(Vec<u8>, crate::engine::entry_header::EntryHeader, Vec<u8>)>> {
    if Self::path_uses_namespace_root_from(engine, normalized)? {
      let head_hash = engine.head_hash()?;
      return match crate::engine::version_access::resolve_directory_at_version(engine, &head_hash, normalized) {
        Ok(resolved) => Ok(Some((resolved.hash, resolved.header, resolved.value))),
        Err(EngineError::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
      };
    }

    let directory_key = directory_path_hash(normalized, &engine.hash_algo())?;
    let Some((header, value)) = self.read_directory_data(&directory_key)? else {
      return Ok(None);
    };
    let identity = if !value.is_empty() && crate::engine::btree::is_btree_format(&value) {
      crate::engine::btree::BTreeNode::deserialize(&value, engine.hash_algo().hash_length(), header.entry_version)?
        .content_hash(engine.hash_algo().hash_length(), &engine.hash_algo())?
    } else {
      directory_content_hash(&value, &engine.hash_algo())?
    };
    Ok(Some((identity, header, value)))
  }

  fn resolve_current_file_record(&self, normalized: &str) -> EngineResult<FileRecord> {
    self
      .resolve_current_file_record_from(self.engine, normalized)?
      .map(|(_identity, record)| record)
      .ok_or_else(|| EngineError::NotFound(normalized.to_string()))
  }

  /// Delete a symlink at the given path.
  pub fn delete_symlink(&self, ctx: &RequestContext, path: &str) -> EngineResult<()> {
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();
    let symlink_key = symlink_path_hash(&normalized, &algo)?;
    self.execute_namespace_mutation(Some(ctx), move |planning_engine| {
      let (previous_identity, record) =
        self.resolve_current_symlink_record_from(planning_engine, &normalized)?.ok_or_else(|| EngineError::NotFound(normalized.clone()))?;
      let deletion = DeletionRecord::new(normalized.clone(), None);
      let deletion_key = deletion_record_hash(&normalized, deletion.deleted_at, &algo)?;
      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::SymlinkDelete);
      batch.store_dependency(EntryType::DeletionRecord, deletion_key, deletion.serialize(), v0_system_entry_flags(&normalized))?;
      if planning_engine.has_entry(&symlink_key)? {
        batch.retire_locator(symlink_key.clone())?;
      }
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: normalized.clone(),
        entry_type: Some(EntryType::Symlink.to_u8()),
        previous_identity: Some(previous_identity),
        new_identity: None,
      })?;
      let mut effects = DirectoryMutationEffects::new(DirectoryMutationCounterEffect::SymlinkDelete);
      self.plan_remove_from_parent_directory(&mut batch, &normalized, &mut effects)?;
      effects.events.push((
        EVENT_ENTRIES_DELETED,
        serde_json::json!({"entries": [EntryEventData {
          path: normalized.clone(),
          entry_type: "symlink".to_string(),
          content_type: None,
          size: 0,
          hash: hex::encode(&record.target),
          created_at: record.created_at,
          updated_at: record.updated_at,
          previous_hash: None,
        }]}),
      ));
      Ok((batch, (), effects))
    })
  }

  /// Atomically move one legacy detached-system file to its canonical alias.
  /// Exact duplicate aliases converge by retiring only the legacy locator;
  /// divergent aliases fail without publishing either side.
  pub(crate) fn migrate_system_file_alias(
    &self,
    context: &RequestContext,
    old_path: &str,
    new_path: &str,
  ) -> EngineResult<SystemFileAliasMigrationOutcome> {
    let old_normalized = normalize_path(old_path);
    let new_normalized = normalize_path(new_path);
    if old_normalized == "/" || new_normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot migrate the root path as a system file".to_string()));
    }
    if old_normalized == new_normalized {
      return Err(EngineError::InvalidInput("Legacy and canonical system paths are the same".to_string()));
    }
    if !v0_is_detached_system_path(&old_normalized) || !v0_is_detached_system_path(&new_normalized) {
      return Err(EngineError::InvalidInput("System file alias migration requires two detached system paths".to_string()));
    }

    let algorithm = self.engine.hash_algo();
    let old_file_key = file_path_hash(&old_normalized, &algorithm)?;
    let mut memory = OperationMemoryBudget::new(
      self.engine,
      "system file alias migration planning",
      MemoryOwner::DurabilityWaiters,
      AdmissionClass::Workload,
      0,
      None,
    )?;

    self.execute_optional_namespace_mutation(Some(context), move |planning_engine| {
      let Some(old_reference) = self.current_entry_reference_from(planning_engine, &old_normalized)? else {
        return Ok((None, SystemFileAliasMigrationOutcome::SourceMissing));
      };
      if old_reference.entry_type != EntryType::FileRecord {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("legacy system path '{old_normalized}' resolves to {:?} instead of FileRecord", old_reference.entry_type),
        });
      }
      memory.reserve(SYSTEM_FILE_ALIAS_WORKSPACE_BYTES, "system FileRecord alias migration admission failed")?;
      let (old_identity, mut old_record) = self
        .resolve_current_file_record_from_bounded(planning_engine, &old_normalized, SYSTEM_FILE_ALIAS_RECORD_MAX_BYTES)?
        .ok_or_else(|| EngineError::CorruptEntry {
          offset: 0,
          reason: format!("legacy system FileRecord '{old_normalized}' disappeared during authoritative migration planning"),
        })?;
      ensure_file_record_content_hash_for_migration(planning_engine, &mut old_record, &mut memory)?;
      validate_existing_file_chunks(planning_engine, &old_normalized, &old_record.chunk_hashes)?;

      let destination = self.current_entry_reference_from(planning_engine, &new_normalized)?;
      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::SystemWrite);
      let deletion = DeletionRecord::new(old_normalized.clone(), None);
      let deletion_key = deletion_record_hash(&old_normalized, deletion.deleted_at, &algorithm)?;
      batch.store_dependency(EntryType::DeletionRecord, deletion_key, deletion.serialize(), v0_system_entry_flags(&old_normalized))?;
      if planning_engine.has_entry(&old_file_key)? {
        batch.retire_locator(old_file_key.clone())?;
      }
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: old_normalized.clone(),
        entry_type: Some(EntryType::FileRecord.to_u8()),
        previous_identity: Some(old_identity),
        new_identity: None,
      })?;

      let mut planner = DirectoryMutationPlanner::default();
      planner.remove_child(&old_normalized)?;
      let outcome = if let Some(destination_reference) = destination {
        if destination_reference.entry_type != EntryType::FileRecord {
          return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: format!(
              "canonical system path '{new_normalized}' resolves to {:?} instead of FileRecord",
              destination_reference.entry_type
            ),
          });
        }
        let (destination_identity, mut destination_record) = self
          .resolve_current_file_record_from_bounded(planning_engine, &new_normalized, SYSTEM_FILE_ALIAS_RECORD_MAX_BYTES)?
          .ok_or_else(|| EngineError::CorruptEntry {
            offset: 0,
            reason: format!("canonical system FileRecord '{new_normalized}' disappeared during authoritative migration planning"),
          })?;
        ensure_file_record_content_hash_for_migration(planning_engine, &mut destination_record, &mut memory)?;
        validate_existing_file_chunks(planning_engine, &new_normalized, &destination_record.chunk_hashes)?;
        if !file_records_are_identical_aliases(&old_record, &destination_record) {
          return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: format!("divergent system path aliases '{old_normalized}' and '{new_normalized}' require operator repair"),
          });
        }
        batch.add_source_identity(NamespaceMutationSourceIdentity {
          path: new_normalized.clone(),
          entry_type: Some(EntryType::FileRecord.to_u8()),
          previous_identity: Some(destination_identity.clone()),
          new_identity: Some(destination_identity),
        })?;
        SystemFileAliasMigrationOutcome::IdenticalAliasRetired
      } else {
        let prepared = prepare_file_record_publication_at_version(
          planning_engine,
          FileRecordPublishInput {
            normalized_path: new_normalized.clone(),
            content_type: old_record.content_type.clone(),
            total_size: old_record.total_size,
            metadata: old_record.metadata.clone(),
            chunk_hashes: old_record.chunk_hashes.clone(),
            content_hash: old_record.content_hash.clone(),
            flags: v0_system_entry_flags(&new_normalized),
            created_at_override: Some(old_record.created_at),
            updated_at_override: Some(old_record.updated_at),
            prefer_existing_created_at: false,
          },
          CURRENT_FILE_RECORD_VERSION,
        )?;
        add_prepared_file_record_entries(&mut batch, &prepared.entries)?;
        batch.add_source_identity(NamespaceMutationSourceIdentity {
          path: new_normalized.clone(),
          entry_type: Some(EntryType::FileRecord.to_u8()),
          previous_identity: None,
          new_identity: Some(prepared.entries.identity_key.clone()),
        })?;
        planner.upsert_child(&new_normalized, prepared.result.child_entry)?;
        SystemFileAliasMigrationOutcome::Moved
      };

      let mut effects = DirectoryMutationEffects::new(DirectoryMutationCounterEffect::None);
      planner.finalize(self, &mut batch, &mut effects)?;
      Ok((Some((batch, effects)), outcome))
    })
  }

  /// Rename (move) a file from one path to another.
  ///
  /// This is a metadata-only operation — no chunk data is copied.
  /// The file's content (chunk_hashes), content_type, total_size, and
  /// created_at are preserved. Only the path and updated_at change.
  pub fn rename_file(&self, ctx: &RequestContext, old_path: &str, new_path: &str) -> EngineResult<FileRecord> {
    let old_normalized = normalize_path(old_path);
    let new_normalized = normalize_path(new_path);
    if old_normalized == "/" || new_normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot rename root path".to_string()));
    }
    if old_normalized == new_normalized {
      return Err(EngineError::InvalidInput("Source and destination paths are the same".to_string()));
    }
    if v0_is_detached_system_path(&old_normalized) != v0_is_detached_system_path(&new_normalized) {
      return Err(EngineError::InvalidInput("Cannot rename across system boundary".to_string()));
    }

    let algo = self.engine.hash_algo();
    let old_file_key = file_path_hash(&old_normalized, &algo)?;
    let mut memory =
      OperationMemoryBudget::new(self.engine, "file rename planning", MemoryOwner::DurabilityWaiters, AdmissionClass::Workload, 0, None)?;

    self.execute_namespace_mutation(Some(ctx), move |planning_engine| {
      let (old_identity, mut old_record) = self
        .resolve_current_file_record_from(planning_engine, &old_normalized)?
        .ok_or_else(|| EngineError::NotFound(old_normalized.clone()))?;
      ensure_file_record_content_hash_for_migration(planning_engine, &mut old_record, &mut memory)?;
      validate_existing_file_chunks(planning_engine, &old_normalized, &old_record.chunk_hashes)?;
      if self.current_entry_reference_from(planning_engine, &new_normalized)?.is_some() {
        return Err(EngineError::AlreadyExists(new_normalized.clone()));
      }

      let prepared = prepare_file_record_publication_at_version(
        planning_engine,
        FileRecordPublishInput {
          normalized_path: new_normalized.clone(),
          content_type: old_record.content_type.clone(),
          total_size: old_record.total_size,
          metadata: old_record.metadata.clone(),
          chunk_hashes: old_record.chunk_hashes.clone(),
          content_hash: old_record.content_hash.clone(),
          flags: v0_system_entry_flags(&new_normalized),
          created_at_override: Some(old_record.created_at),
          updated_at_override: None,
          prefer_existing_created_at: false,
        },
        CURRENT_FILE_RECORD_VERSION,
      )?;
      let deletion = DeletionRecord::new(old_normalized.clone(), None);
      let deletion_key = deletion_record_hash(&old_normalized, deletion.deleted_at, &algo)?;

      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::Rename);
      add_prepared_file_record_entries(&mut batch, &prepared.entries)?;
      batch.store_dependency(EntryType::DeletionRecord, deletion_key, deletion.serialize(), v0_system_entry_flags(&old_normalized))?;
      if planning_engine.has_entry(&old_file_key)? {
        batch.retire_locator(old_file_key.clone())?;
      }
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: old_normalized.clone(),
        entry_type: Some(EntryType::FileRecord.to_u8()),
        previous_identity: Some(old_identity),
        new_identity: None,
      })?;
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: new_normalized.clone(),
        entry_type: Some(EntryType::FileRecord.to_u8()),
        previous_identity: None,
        new_identity: Some(prepared.entries.identity_key.clone()),
      })?;

      let mut planner = DirectoryMutationPlanner::default();
      planner.remove_child(&old_normalized)?;
      planner.upsert_child(&new_normalized, prepared.result.child_entry.clone())?;
      let mut effects = DirectoryMutationEffects::new(DirectoryMutationCounterEffect::Aggregate(DirectoryMutationCounterDelta::default()));
      effects.metadata_index_removal_paths.push(old_normalized.clone());
      effects.metadata_index_paths.push(new_normalized.clone());
      effects.events.push((
        EVENT_ENTRIES_DELETED,
        serde_json::json!({"entries": [EntryEventData {
          path: old_normalized.clone(),
          entry_type: "file".to_string(),
          content_type: old_record.content_type.clone(),
          size: old_record.total_size,
          hash: old_record.content_hash_hex(),
          created_at: old_record.created_at,
          updated_at: old_record.updated_at,
          previous_hash: None,
        }]}),
      ));
      effects.events.push((EVENT_ENTRIES_CREATED, serde_json::json!({"entries": [prepared.result.event_entry.clone()]})));
      planner.finalize(self, &mut batch, &mut effects)?;
      Ok((batch, prepared.result.file_record, effects))
    })
  }

  /// Copy a file to a new path. Reuses existing chunk hashes (no data duplication).
  pub fn copy_file(&self, ctx: &RequestContext, from_path: &str, to_path: &str) -> EngineResult<FileRecord> {
    let from_normalized = normalize_path(from_path);
    let to_normalized = normalize_path(to_path);
    let mut result = self.execute_copy_mappings(ctx, vec![(from_normalized, to_normalized.clone())], CopySourceConstraint::FileOnly)?;
    result.file_records.remove(&to_normalized).ok_or_else(|| EngineError::CorruptEntry {
      offset: 0,
      reason: format!("acknowledged file copy did not return its planned record for '{to_normalized}'"),
    })
  }

  /// Recursively copy a path (file or directory) to a new location.
  pub fn copy_path(&self, ctx: &RequestContext, from_path: &str, to_path: &str) -> EngineResult<Vec<String>> {
    let from_normalized = normalize_path(from_path);
    let to_normalized = normalize_path(to_path);
    self.execute_copy_mappings(ctx, vec![(from_normalized, to_normalized)], CopySourceConstraint::Any).map(|result| result.copied_paths)
  }

  /// Atomically copy one or more source paths into a destination directory.
  pub fn copy_paths(&self, ctx: &RequestContext, from_paths: &[String], destination: &str) -> EngineResult<Vec<String>> {
    if from_paths.is_empty() {
      return Err(EngineError::InvalidInput("No source paths provided for copy".to_string()));
    }
    let destination = normalize_path(destination);
    if v0_is_detached_system_path(&destination) {
      return Err(EngineError::InvalidInput("Cannot copy system paths".to_string()));
    }

    let mut mappings = Vec::with_capacity(from_paths.len());
    let mut source_paths = std::collections::HashSet::with_capacity(from_paths.len());
    let mut destination_paths = std::collections::HashSet::with_capacity(from_paths.len());
    for from_path in from_paths {
      let source = normalize_path(from_path);
      let name = file_name(&source).unwrap_or("");
      if source == "/" || name.is_empty() {
        return Err(EngineError::InvalidInput("Cannot copy root path".to_string()));
      }
      if v0_is_detached_system_path(&source) {
        return Err(EngineError::InvalidInput("Cannot copy system paths".to_string()));
      }
      if !source_paths.insert(source.clone()) {
        return Err(EngineError::InvalidInput(format!("Duplicate copy source: {source}")));
      }
      let target = if destination == "/" { format!("/{name}") } else { format!("{}/{name}", destination.trim_end_matches('/')) };
      if !destination_paths.insert(target.clone()) {
        return Err(EngineError::AlreadyExists(target));
      }
      mappings.push((source, target));
    }
    self.execute_copy_mappings(ctx, mappings, CopySourceConstraint::Any).map(|result| result.copied_paths)
  }

  fn execute_copy_mappings(
    &self,
    ctx: &RequestContext,
    mappings: Vec<(String, String)>,
    source_constraint: CopySourceConstraint,
  ) -> EngineResult<CopyPublicationResult> {
    if mappings.is_empty() {
      return Err(EngineError::InvalidInput("No copy mappings provided".to_string()));
    }
    let mut memory = OperationMemoryBudget::new(
      self.engine,
      "copy namespace planning",
      MemoryOwner::DurabilityWaiters,
      AdmissionClass::Workload,
      0,
      None,
    )?;
    let mut normalized_mappings = Vec::with_capacity(mappings.len());
    for (source, destination) in mappings {
      let mapping_bytes = copy_workspace_bytes(
        std::mem::size_of::<(String, String, usize)>() + 256,
        &[source.len(), destination.len()],
        "copy mapping estimate overflow",
      )?;
      memory.reserve(mapping_bytes, "copy mapping admission failed")?;
      memory.record_work(1)?;
      let source = normalize_path(&source);
      let destination = normalize_path(&destination);
      if source == "/" || destination == "/" {
        return Err(EngineError::InvalidInput("Cannot copy root path".to_string()));
      }
      if source == destination {
        return Err(EngineError::InvalidInput("Source and destination are the same".to_string()));
      }
      if destination.starts_with(&format!("{}/", source.trim_end_matches('/'))) {
        return Err(EngineError::InvalidInput(format!("Cannot copy '{}' into its own descendant '{}'", source, destination)));
      }
      if v0_is_detached_system_path(&source) || v0_is_detached_system_path(&destination) {
        return Err(EngineError::InvalidInput("Cannot copy system paths".to_string()));
      }
      normalized_mappings.push((source, destination));
    }

    self.execute_namespace_mutation(Some(ctx), move |planning_engine| {
      let prepared_entries = self.prepare_copy_entries(planning_engine, normalized_mappings, source_constraint, &mut memory)?;
      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::Copy);
      let mut planner = DirectoryMutationPlanner::default();
      let mut counter_delta = DirectoryMutationCounterDelta::default();
      let mut copied_paths = Vec::new();
      let mut file_records = std::collections::BTreeMap::new();
      let mut metadata_index_paths = Vec::new();
      let mut event_entries = Vec::with_capacity(prepared_entries.len());

      for prepared in prepared_entries {
        batch.add_source_identity(NamespaceMutationSourceIdentity {
          path: prepared.source_path.clone(),
          entry_type: Some(match &prepared.kind {
            PreparedCopyKind::File(_) => EntryType::FileRecord.to_u8(),
            PreparedCopyKind::Directory => EntryType::DirectoryIndex.to_u8(),
            PreparedCopyKind::Symlink(_) => EntryType::Symlink.to_u8(),
          }),
          previous_identity: Some(prepared.source_identity.clone()),
          new_identity: Some(prepared.source_identity.clone()),
        })?;

        match prepared.kind {
          PreparedCopyKind::File(source_record) => {
            let publication = prepare_file_record_publication_at_version(
              planning_engine,
              FileRecordPublishInput {
                normalized_path: prepared.destination_path.clone(),
                content_type: source_record.content_type.clone(),
                total_size: source_record.total_size,
                metadata: source_record.metadata.clone(),
                chunk_hashes: source_record.chunk_hashes.clone(),
                content_hash: source_record.content_hash.clone(),
                flags: 0,
                created_at_override: Some(source_record.created_at),
                updated_at_override: None,
                prefer_existing_created_at: false,
              },
              CURRENT_FILE_RECORD_VERSION,
            )?;
            add_prepared_file_record_entries(&mut batch, &publication.entries)?;
            batch.add_source_identity(NamespaceMutationSourceIdentity {
              path: prepared.destination_path.clone(),
              entry_type: Some(EntryType::FileRecord.to_u8()),
              previous_identity: None,
              new_identity: Some(publication.entries.identity_key.clone()),
            })?;
            planner.upsert_child(&prepared.destination_path, publication.result.child_entry.clone())?;
            counter_delta.file_writes.push((None, publication.result.file_record.total_size));
            event_entries.push(publication.result.event_entry);
            metadata_index_paths.push(prepared.destination_path.clone());
            copied_paths.push(prepared.destination_path.clone());
            if file_records.insert(prepared.destination_path.clone(), publication.result.file_record).is_some() {
              return Err(EngineError::InvalidInput(format!("Copy contains duplicate file destination '{}'", prepared.destination_path)));
            }
          }
          PreparedCopyKind::Directory => {
            planner.ensure_directory(&prepared.destination_path)?;
            let now = chrono::Utc::now().timestamp_millis();
            event_entries.push(EntryEventData {
              path: prepared.destination_path,
              entry_type: "directory".to_string(),
              content_type: None,
              size: 0,
              hash: hex::encode(prepared.source_identity),
              created_at: now,
              updated_at: now,
              previous_hash: None,
            });
          }
          PreparedCopyKind::Symlink(source_record) => {
            let mut destination_record = SymlinkRecord::new(prepared.destination_path.clone(), source_record.target.clone());
            destination_record.created_at = source_record.created_at;
            let serialized = destination_record.serialize()?;
            let content_key = symlink_content_hash(&serialized, &planning_engine.hash_algo())?;
            let identity_key = symlink_identity_hash(&prepared.destination_path, &destination_record.target, &planning_engine.hash_algo())?;
            let path_key = symlink_path_hash(&prepared.destination_path, &planning_engine.hash_algo())?;
            batch.store_dependency(EntryType::Symlink, content_key, serialized.clone(), 0)?;
            batch.store_dependency(EntryType::Symlink, identity_key.clone(), serialized.clone(), 0)?;
            batch.replace_locator(EntryType::Symlink, path_key, serialized, 0)?;
            batch.add_source_identity(NamespaceMutationSourceIdentity {
              path: prepared.destination_path.clone(),
              entry_type: Some(EntryType::Symlink.to_u8()),
              previous_identity: None,
              new_identity: Some(identity_key.clone()),
            })?;
            planner.upsert_child(
              &prepared.destination_path,
              ChildEntry {
                entry_type: EntryType::Symlink.to_u8(),
                hash: identity_key,
                total_size: 0,
                created_at: destination_record.created_at,
                updated_at: destination_record.updated_at,
                name: file_name(&prepared.destination_path).unwrap_or("").to_string(),
                content_type: None,
                virtual_time: chrono::Utc::now().timestamp_millis() as u64,
                node_id: 0,
              },
            )?;
            counter_delta.symlinks_created = counter_delta
              .symlinks_created
              .checked_add(1)
              .ok_or_else(|| EngineError::ResourceExhausted("copied symlink counter overflow".to_string()))?;
            event_entries.push(EntryEventData {
              path: prepared.destination_path.clone(),
              entry_type: "symlink".to_string(),
              content_type: None,
              size: 0,
              hash: hex::encode(&destination_record.target),
              created_at: destination_record.created_at,
              updated_at: destination_record.updated_at,
              previous_hash: None,
            });
            copied_paths.push(prepared.destination_path);
          }
        }
      }

      let mut effects = DirectoryMutationEffects::new(DirectoryMutationCounterEffect::Aggregate(counter_delta));
      effects.metadata_index_paths = metadata_index_paths;
      effects.events.push((EVENT_ENTRIES_CREATED, serde_json::json!({ "entries": event_entries })));
      planner.finalize(self, &mut batch, &mut effects)?;
      copied_paths.sort();
      Ok((batch, CopyPublicationResult { copied_paths, file_records }, effects))
    })
  }

  fn prepare_copy_entries(
    &self,
    planning_engine: &StorageEngine,
    mappings: Vec<(String, String)>,
    source_constraint: CopySourceConstraint,
    memory: &mut OperationMemoryBudget,
  ) -> EngineResult<Vec<PreparedCopyEntry>> {
    let mut pending: Vec<(String, String, usize)> = mappings.into_iter().map(|(source, destination)| (source, destination, 0)).collect();
    let mut prepared = std::collections::BTreeMap::new();

    while let Some((source_path, destination_path, depth)) = pending.pop() {
      if depth > Self::MAX_DIRECTORY_DEPTH {
        return Err(EngineError::InvalidInput(format!("Copy depth exceeds maximum of {} levels", Self::MAX_DIRECTORY_DEPTH)));
      }
      if prepared.contains_key(&destination_path) {
        return Err(EngineError::AlreadyExists(destination_path));
      }
      if self.current_entry_reference_from(planning_engine, &destination_path)?.is_some() {
        return Err(EngineError::AlreadyExists(destination_path));
      }

      let source_reference =
        self.current_entry_reference_from(planning_engine, &source_path)?.ok_or_else(|| EngineError::NotFound(source_path.clone()))?;
      if source_constraint == CopySourceConstraint::FileOnly && source_reference.entry_type != EntryType::FileRecord {
        return Err(EngineError::InvalidInput(format!("copy_file requires a file source: '{source_path}'")));
      }

      let source_entry = planning_engine.get_kv_entry(&source_reference.hash)?.ok_or_else(|| EngineError::CorruptEntry {
        offset: 0,
        reason: format!("copy source '{}' lost its selected entity during planning", source_path),
      })?;
      let source_workspace = u64::from(source_entry.total_length)
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(512))
        .ok_or_else(|| EngineError::ResourceExhausted("copy source metadata estimate overflow".to_string()))?;
      memory.reserve(source_workspace, "copy source metadata admission failed")?;
      memory.record_work(1)?;

      let entry = if source_reference.entry_type == EntryType::DirectoryIndex {
        let (identity, _header, _directory_data) =
          self.resolve_current_directory_data_from(planning_engine, &source_path)?.ok_or_else(|| EngineError::CorruptEntry {
            offset: source_entry.offset,
            reason: format!("copy source directory '{}' has no readable content", source_path),
          })?;
        self.visit_live_directory_children_strict_no_heal(&source_path, |child| {
          if child.name.is_empty() || child.name == "." || child.name == ".." || child.name.contains('/') {
            return Err(EngineError::CorruptEntry {
              offset: 0,
              reason: format!("copy source directory '{}' contains invalid child name '{}'", source_path, child.name),
            });
          }
          let child_workspace = copy_workspace_bytes(
            std::mem::size_of::<(String, String, usize)>() + 256,
            &[source_path.len(), destination_path.len(), child.name.len(), child.name.len()],
            "copy child path estimate overflow",
          )?;
          memory.reserve(child_workspace, "copy child path admission failed")?;
          memory.record_work(1)?;
          let child_source = format!("{}/{}", source_path.trim_end_matches('/'), child.name);
          let child_destination = format!("{}/{}", destination_path.trim_end_matches('/'), child.name);
          pending.push((child_source, child_destination, depth + 1));
          Ok(true)
        })?;
        PreparedCopyEntry { source_path, destination_path, source_identity: identity, kind: PreparedCopyKind::Directory }
      } else if source_reference.entry_type == EntryType::FileRecord {
        let (identity, mut record) =
          self.resolve_current_file_record_from(planning_engine, &source_path)?.ok_or_else(|| EngineError::CorruptEntry {
            offset: source_entry.offset,
            reason: format!("copy source file '{}' lost its selected record during planning", source_path),
          })?;
        ensure_file_record_content_hash_for_migration(planning_engine, &mut record, memory)?;
        validate_existing_file_chunks(planning_engine, &source_path, &record.chunk_hashes)?;
        for _ in &record.chunk_hashes {
          memory.record_work(1)?;
        }
        PreparedCopyEntry { source_path, destination_path, source_identity: identity, kind: PreparedCopyKind::File(record) }
      } else if source_reference.entry_type == EntryType::Symlink {
        let (identity, record) =
          self.resolve_current_symlink_record_from(planning_engine, &source_path)?.ok_or_else(|| EngineError::CorruptEntry {
            offset: source_entry.offset,
            reason: format!("copy source symlink '{}' lost its selected record during planning", source_path),
          })?;
        PreparedCopyEntry { source_path, destination_path, source_identity: identity, kind: PreparedCopyKind::Symlink(record) }
      } else {
        return Err(EngineError::InvalidInput(format!(
          "copy source '{}' has unsupported entry type {:?}",
          source_path, source_reference.entry_type
        )));
      };
      prepared.insert(entry.destination_path.clone(), entry);
    }

    Ok(prepared.into_values().collect())
  }

  /// Rename (move) a symlink from one path to another.
  ///
  /// This is a metadata-only operation — the symlink's target does NOT change,
  /// only its path. created_at is preserved.
  pub fn rename_symlink(&self, ctx: &RequestContext, old_path: &str, new_path: &str) -> EngineResult<SymlinkRecord> {
    let old_normalized = normalize_path(old_path);
    let new_normalized = normalize_path(new_path);
    if old_normalized == "/" || new_normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot rename root path".to_string()));
    }
    if old_normalized == new_normalized {
      return Err(EngineError::InvalidInput("Source and destination paths are the same".to_string()));
    }
    if v0_is_detached_system_path(&old_normalized) != v0_is_detached_system_path(&new_normalized) {
      return Err(EngineError::InvalidInput("Cannot rename across system boundary".to_string()));
    }

    let algo = self.engine.hash_algo();
    let old_symlink_key = symlink_path_hash(&old_normalized, &algo)?;
    let new_symlink_key = symlink_path_hash(&new_normalized, &algo)?;

    self.execute_namespace_mutation(Some(ctx), move |planning_engine| {
      let (old_identity, old_record) = self
        .resolve_current_symlink_record_from(planning_engine, &old_normalized)?
        .ok_or_else(|| EngineError::NotFound(old_normalized.clone()))?;
      if self.current_entry_reference_from(planning_engine, &new_normalized)?.is_some() {
        return Err(EngineError::AlreadyExists(new_normalized.clone()));
      }

      let mut new_record = SymlinkRecord::new(new_normalized.clone(), old_record.target.clone());
      new_record.created_at = old_record.created_at;
      let serialized = new_record.serialize()?;
      let content_key = symlink_content_hash(&serialized, &algo)?;
      let identity_key = symlink_identity_hash(&new_normalized, &new_record.target, &algo)?;
      let deletion = DeletionRecord::new(old_normalized.clone(), None);
      let deletion_key = deletion_record_hash(&old_normalized, deletion.deleted_at, &algo)?;

      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::Rename);
      batch.store_dependency(EntryType::Symlink, content_key, serialized.clone(), v0_system_entry_flags(&new_normalized))?;
      batch.store_dependency(EntryType::Symlink, identity_key.clone(), serialized.clone(), v0_system_entry_flags(&new_normalized))?;
      batch.replace_locator(EntryType::Symlink, new_symlink_key.clone(), serialized, v0_system_entry_flags(&new_normalized))?;
      batch.store_dependency(EntryType::DeletionRecord, deletion_key, deletion.serialize(), v0_system_entry_flags(&old_normalized))?;
      if planning_engine.has_entry(&old_symlink_key)? {
        batch.retire_locator(old_symlink_key.clone())?;
      }
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: old_normalized.clone(),
        entry_type: Some(EntryType::Symlink.to_u8()),
        previous_identity: Some(old_identity),
        new_identity: None,
      })?;
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: new_normalized.clone(),
        entry_type: Some(EntryType::Symlink.to_u8()),
        previous_identity: None,
        new_identity: Some(identity_key.clone()),
      })?;

      let mut planner = DirectoryMutationPlanner::default();
      planner.remove_child(&old_normalized)?;
      planner.upsert_child(
        &new_normalized,
        ChildEntry {
          entry_type: EntryType::Symlink.to_u8(),
          hash: identity_key,
          total_size: 0,
          created_at: new_record.created_at,
          updated_at: new_record.updated_at,
          name: file_name(&new_normalized).unwrap_or("").to_string(),
          content_type: None,
          virtual_time: chrono::Utc::now().timestamp_millis() as u64,
          node_id: 0,
        },
      )?;
      let mut effects = DirectoryMutationEffects::new(DirectoryMutationCounterEffect::Aggregate(DirectoryMutationCounterDelta::default()));
      effects.events.push((
        EVENT_ENTRIES_DELETED,
        serde_json::json!({"entries": [EntryEventData {
          path: old_normalized.clone(),
          entry_type: "symlink".to_string(),
          content_type: None,
          size: 0,
          hash: hex::encode(&old_record.target),
          created_at: old_record.created_at,
          updated_at: old_record.updated_at,
          previous_hash: None,
        }]}),
      ));
      effects.events.push((
        EVENT_ENTRIES_CREATED,
        serde_json::json!({"entries": [EntryEventData {
          path: new_normalized.clone(),
          entry_type: "symlink".to_string(),
          content_type: None,
          size: 0,
          hash: hex::encode(&new_record.target),
          created_at: new_record.created_at,
          updated_at: new_record.updated_at,
          previous_hash: None,
        }]}),
      ));
      planner.finalize(self, &mut batch, &mut effects)?;
      Ok((batch, new_record, effects))
    })
  }
}

fn copy_workspace_bytes(base: usize, dynamic: &[usize], overflow_message: &str) -> EngineResult<u64> {
  let bytes = dynamic
    .iter()
    .try_fold(base, |total, bytes| total.checked_add(*bytes))
    .and_then(|bytes| u64::try_from(bytes).ok())
    .ok_or_else(|| EngineError::ResourceExhausted(overflow_message.to_string()))?;
  Ok(bytes)
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

  #[test]
  fn full_directory_repair_preserves_every_acknowledgement_before_a_later_failure() {
    let (engine, _temp) = create_test_engine();
    let context = RequestContext::system();
    let operations = DirectoryOps::new(&engine);
    operations.store_file_buffered(&context, "/repair/a/b/file.txt", b"body", Some("text/plain")).unwrap();
    let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
    let _fault = DirectoryRepairTestFaultGuard::fail_after_acknowledgements(1);

    let error = operations.rebuild_directory_tree(&context).expect_err("the second repair step must fail after one acknowledgement");
    let EngineError::PartialOperation { operation, completed, failed, evidence } = error else {
      panic!("full repair erased prior acknowledgement evidence: {error}");
    };
    assert_eq!(operation, "directory tree repair");
    assert_eq!(completed, 1);
    assert_eq!(failed, 1);
    assert!(evidence.contains("phase=post_publication"), "missing failure phase: {evidence}");
    assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before + 1);
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
  fn conditional_batch_delete_skips_a_replaced_file_identity() {
    let (engine, _temp) = create_test_engine();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    let path = "/.aeordb-system/refresh-tokens/conditional.json";
    ops.store_file_buffered(&ctx, path, b"old expired authority", Some("application/json")).unwrap();
    let (expected_identity, old_body) = ops.read_file_buffered_bounded_with_identity(path, 1024).unwrap();
    assert_eq!(old_body, b"old expired authority");

    ops.store_file_buffered(&ctx, path, b"new valid authority", Some("application/json")).unwrap();
    let before = engine.durability_snapshot().unwrap().next_sequence;
    let deleted = ops
      .delete_files_batch_with_kind(
        &ctx,
        vec![FileDeletionRequest::optional_matching_identity(path, expected_identity)],
        NamespaceMutationKind::MaintenanceRepair,
      )
      .unwrap();

    assert!(deleted.is_empty(), "a stale cleanup candidate must not delete replacement authority");
    assert_eq!(engine.durability_snapshot().unwrap().next_sequence, before, "all-changed conditional batch must publish nothing");
    assert_eq!(ops.read_file_buffered(path).unwrap(), b"new valid authority");
  }

  #[test]
  fn batch_file_publication_rejects_missing_chunks_before_namespace_publication() {
    let (engine, _temp) = create_test_engine();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.ensure_root_directory(&ctx).unwrap();
    let original_head = engine.head_hash().unwrap();
    let hash_length = engine.hash_algo().hash_length();

    let result = ops.execute_file_publications(
      &ctx,
      vec![BatchFilePublicationInput {
        publication: FileRecordPublishInput {
          normalized_path: "/missing-chunk.bin".to_string(),
          content_type: Some("application/octet-stream".to_string()),
          total_size: 1,
          metadata: Vec::new(),
          chunk_hashes: vec![vec![0xA5; hash_length]],
          content_hash: vec![0x5A; hash_length],
          flags: 0,
          created_at_override: None,
          updated_at_override: None,
          prefer_existing_created_at: true,
        },
        throughput_bytes: 0,
      }],
      NamespaceMutationKind::BatchWrite,
    );

    assert!(
      matches!(&result, Err(EngineError::CorruptEntry { reason, .. }) if reason.contains("missing chunk")),
      "namespace publication accepted a missing chunk: {result:?}"
    );
    assert_eq!(engine.head_hash().unwrap(), original_head);
    assert!(ops.get_metadata("/missing-chunk.bin").unwrap().is_none());
  }

  #[test]
  fn batch_file_publication_rejects_malformed_and_non_chunk_references_before_namespace_publication() {
    for (case, chunk_hash, install_wrong_type) in [("malformed-width", vec![0xB5; 31], false), ("wrong-entry-type", vec![0xB6; 32], true)] {
      let (engine, _temp) = create_test_engine();
      let ctx = RequestContext::system();
      let ops = DirectoryOps::new(&engine);
      ops.ensure_root_directory(&ctx).unwrap();
      if install_wrong_type {
        engine.store_entry(EntryType::DirectoryIndex, &chunk_hash, b"not a chunk").unwrap();
      }
      let original_head = engine.head_hash().unwrap();

      let result = ops.execute_file_publications(
        &ctx,
        vec![BatchFilePublicationInput {
          publication: FileRecordPublishInput {
            normalized_path: format!("/{case}.bin"),
            content_type: Some("application/octet-stream".to_string()),
            total_size: 1,
            metadata: Vec::new(),
            chunk_hashes: vec![chunk_hash],
            content_hash: vec![0x5A; engine.hash_algo().hash_length()],
            flags: 0,
            created_at_override: None,
            updated_at_override: None,
            prefer_existing_created_at: true,
          },
          throughput_bytes: 0,
        }],
        NamespaceMutationKind::BatchWrite,
      );

      if install_wrong_type {
        assert!(matches!(&result, Err(EngineError::CorruptEntry { .. })), "namespace publication accepted {case}: {result:?}");
      } else {
        assert!(matches!(&result, Err(EngineError::InvalidInput(_))), "namespace publication accepted {case}: {result:?}");
      }
      assert_eq!(engine.head_hash().unwrap(), original_head);
      assert!(ops.get_metadata(&format!("/{case}.bin")).unwrap().is_none());
    }
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
