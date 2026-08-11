//! Deterministic write-once storage for immutable v4 semantic objects.
//!
//! This module owns the only typed publication capability. It deliberately
//! does not select a namespace root or expose a service caller.

use super::namespace::{SemanticObjectV1, decode_semantic_object};
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use crate::engine::directory_ops::{DEFAULT_CHUNK_SIZE, DirectoryOps, file_path_hash, whole_file_content_hash};
use crate::engine::entry_header::FLAG_SYSTEM;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::{CURRENT_FILE_RECORD_VERSION, FileRecord};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryOwner};
use crate::engine::operation_memory::OperationMemoryBudget;
use crate::engine::storage_engine::{NamespaceWriteGuard, StorageEngine};
use crate::engine::HashAlgorithm;

pub const SEMANTIC_OBJECT_CONTENT_TYPE: &str = "application/vnd.aeordb.semantic-object";

const SEMANTIC_STATE_CAP: usize = 4 * 1024;
const SEMANTIC_CATALOG_LEAF_CAP: usize = 1024 * 1024;
const SEMANTIC_CATALOG_INTERNAL_CAP: usize = 64 * 1024;
const SEMANTIC_DEFINITION_CAP: usize = 1024 * 1024;
const SEMANTIC_STORE_BASE_WORKSPACE: u64 = 4 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedSemanticObjectV1 {
  pub path: String,
  pub object: SemanticObjectV1,
  pub bytes: Vec<u8>,
}

/// Typed proof that the semantic store owns the canonical object path and the
/// namespace authority for one write-once publication.
pub(crate) struct V4SemanticObjectPublicationContextV1<'a> {
  engine: &'a StorageEngine,
  path: String,
  _authority: NamespaceWriteGuard<'a>,
}

impl<'a> V4SemanticObjectPublicationContextV1<'a> {
  fn new(engine: &'a StorageEngine, object: &SemanticObjectV1) -> EngineResult<Self> {
    let path = semantic_object_path(engine.hash_algo(), object.kind_id, &object.object_id).map_err(format_error)?;
    let authority = engine.namespace_write_guard()?;
    Ok(Self { engine, path, _authority: authority })
  }

  pub(crate) fn engine(&self) -> &StorageEngine {
    self.engine
  }

  pub(crate) fn target_path(&self) -> &str {
    &self.path
  }
}

/// Disconnected v4 semantic-object writer. P3b-2b intentionally gives this
/// type no production caller; first-authority publication is owned by P3b-2c.
pub struct V4SemanticObjectStore<'a> {
  engine: &'a StorageEngine,
}

impl<'a> V4SemanticObjectStore<'a> {
  pub fn new(engine: &'a StorageEngine) -> Self {
    Self { engine }
  }

  pub fn load(&self, kind_id: u16, object_id: &[u8]) -> EngineResult<Option<LoadedSemanticObjectV1>> {
    let path = semantic_object_path(self.engine.hash_algo(), kind_id, object_id).map_err(format_error)?;
    let mut memory = OperationMemoryBudget::new(
      self.engine,
      "semantic-object load",
      MemoryOwner::DurabilityWaiters,
      AdmissionClass::Workload,
      SEMANTIC_STORE_BASE_WORKSPACE,
      None,
    )?;
    load_semantic_file_record(self.engine, kind_id, object_id, &path, &mut memory)
  }

  pub fn publish(&self, expected_object_id: &[u8], bytes: &[u8]) -> EngineResult<LoadedSemanticObjectV1> {
    enforce_declared_kind_cap(bytes)?;
    let publication_workspace = semantic_publication_workspace(bytes.len())?;
    let mut memory = OperationMemoryBudget::new(
      self.engine,
      "semantic-object publication",
      MemoryOwner::DurabilityWaiters,
      AdmissionClass::Workload,
      publication_workspace,
      None,
    )?;
    let object = decode_semantic_object(bytes, self.engine.hash_algo()).map_err(format_error)?;
    enforce_kind_cap(object.kind_id, bytes.len())?;
    if object.object_id != expected_object_id {
      return Err(EngineError::InvalidInput("semantic-object expected identity does not match the exact encoded bytes".to_string()));
    }

    let publication = V4SemanticObjectPublicationContextV1::new(self.engine, &object)?;
    if let Some(existing) =
      load_semantic_file_record(self.engine, object.kind_id, &object.object_id, publication.target_path(), &mut memory)?
    {
      if existing.bytes != bytes {
        return Err(EngineError::InvalidInput(
          "semantic-object path already contains different validated bytes for the same identity".to_string(),
        ));
      }
      return Ok(existing);
    }

    DirectoryOps::new(self.engine).store_semantic_file_record_v1(&publication, bytes)?;
    let read_back = load_semantic_file_record(self.engine, object.kind_id, &object.object_id, publication.target_path(), &mut memory)?
      .ok_or_else(|| EngineError::DurabilityFailure(format!("semantic-object read-back is absent at {}", publication.target_path())))?;
    if read_back.bytes != bytes || read_back.object != object {
      return Err(EngineError::DurabilityFailure(format!("semantic-object read-back mismatch at {}", publication.target_path())));
    }
    Ok(read_back)
  }
}

pub fn semantic_object_path(algorithm: HashAlgorithm, kind_id: u16, object_id: &[u8]) -> FormatResult<String> {
  semantic_object_cap(kind_id)?;
  let hash_width = algorithm.hash_length();
  if object_id.len() != hash_width {
    return Err(FormatError::new(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "semantic_object_path_hash_width",
      format!("object ID has {} bytes, expected {hash_width}", object_id.len()),
    ));
  }
  if object_id.iter().all(|byte| *byte == 0) {
    return Err(FormatError::new(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "semantic_object_path_zero_identity",
      "object ID must be nonzero",
    ));
  }
  Ok(format!("/.aeordb-system/semantic-objects/{:04x}/{kind_id:04x}/{}", algorithm.to_u16(), hex::encode(object_id)))
}

fn load_semantic_file_record(
  engine: &StorageEngine,
  expected_kind_id: u16,
  expected_object_id: &[u8],
  path: &str,
  memory: &mut OperationMemoryBudget,
) -> EngineResult<Option<LoadedSemanticObjectV1>> {
  let encoded_cap = semantic_object_cap(expected_kind_id).map_err(format_error)?;
  let path_key = file_path_hash(path, &engine.hash_algo())?;
  let value_cap = semantic_file_record_value_cap(encoded_cap, path, engine.hash_algo().hash_length())?;
  let Some((header, _, value)) = engine.get_entry_verified_bounded(&path_key, value_cap)? else {
    return Ok(None);
  };
  if header.entry_type != EntryType::FileRecord {
    return Err(EngineError::InvalidInput(format!("semantic object {path} path key does not resolve to a FileRecord")));
  }
  if header.flags & FLAG_SYSTEM == 0 || header.entry_version != CURRENT_FILE_RECORD_VERSION {
    return Err(EngineError::InvalidInput(format!("semantic object {path} is not a system-flagged FileRecord v1")));
  }

  let record = FileRecord::deserialize(&value, engine.hash_algo().hash_length(), header.entry_version)?;
  if record.path != path {
    return Err(EngineError::InvalidInput(format!("semantic object path-key mismatch for {path}")));
  }
  if record.content_type.as_deref() != Some(SEMANTIC_OBJECT_CONTENT_TYPE) {
    return Err(EngineError::InvalidInput(format!("semantic object {path} content type is not {SEMANTIC_OBJECT_CONTENT_TYPE}")));
  }
  if !record.metadata.is_empty() {
    return Err(EngineError::InvalidInput(format!("semantic object {path} FileRecord metadata must be empty")));
  }
  if record.total_size > encoded_cap as u64 {
    return Err(EngineError::InvalidInput(format!(
      "semantic object kind {expected_kind_id:#06x} length {} exceeds cap {encoded_cap}",
      record.total_size
    )));
  }

  let body_workspace = record
    .total_size
    .checked_mul(2)
    .ok_or_else(|| EngineError::ResourceExhausted("semantic-object body workspace overflow".to_string()))?;
  memory.reserve(body_workspace, "semantic-object body admission failed")?;
  let bytes = DirectoryOps::new(engine).read_file_record_body_bounded(&record, encoded_cap as u64)?;
  let expected_content_hash = whole_file_content_hash(&bytes, &engine.hash_algo())?;
  if record.content_hash != expected_content_hash {
    return Err(EngineError::InvalidInput(format!("semantic object {path} content hash does not match its exact payload")));
  }
  let object = decode_semantic_object(&bytes, engine.hash_algo()).map_err(format_error)?;
  if object.kind_id != expected_kind_id || object.object_id != expected_object_id {
    return Err(EngineError::InvalidInput(format!("semantic object {path} identity does not match its canonical path")));
  }
  Ok(Some(LoadedSemanticObjectV1 { path: path.to_string(), object, bytes }))
}

pub(crate) fn semantic_object_cap(kind_id: u16) -> FormatResult<usize> {
  match kind_id {
    0x0001 => Ok(SEMANTIC_STATE_CAP),
    0x0002 => Ok(SEMANTIC_CATALOG_LEAF_CAP),
    0x0003 => Ok(SEMANTIC_CATALOG_INTERNAL_CAP),
    0x0004 => Ok(SEMANTIC_DEFINITION_CAP),
    _ => Err(FormatError::new(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "semantic_object_path_kind",
      format!("unknown semantic-object kind {kind_id:#06x}"),
    )),
  }
}

fn enforce_kind_cap(kind_id: u16, actual: usize) -> EngineResult<()> {
  let cap = semantic_object_cap(kind_id).map_err(format_error)?;
  if actual > cap {
    return Err(EngineError::InvalidInput(format!("semantic object kind {kind_id:#06x} length {actual} exceeds cap {cap}")));
  }
  Ok(())
}

fn enforce_declared_kind_cap(bytes: &[u8]) -> EngineResult<()> {
  if bytes.len() < 8 || &bytes[..4] != b"ASEM" {
    return Ok(());
  }
  let kind_id = u16::from_le_bytes([bytes[6], bytes[7]]);
  enforce_kind_cap(kind_id, bytes.len())
}

fn semantic_publication_workspace(body_length: usize) -> EngineResult<u64> {
  let body_length = match u64::try_from(body_length) {
    Ok(body_length) => body_length,
    Err(error) => {
      return Err(EngineError::ResourceExhausted(format!("semantic-object publication length exceeds this platform: {error}")));
    }
  };
  SEMANTIC_STORE_BASE_WORKSPACE
    .checked_add(body_length)
    .ok_or_else(|| EngineError::ResourceExhausted("semantic-object publication workspace overflow".to_string()))
}

fn semantic_file_record_value_cap(encoded_cap: usize, path: &str, hash_length: usize) -> EngineResult<u32> {
  let maximum_chunk_count = encoded_cap
    .checked_add(DEFAULT_CHUNK_SIZE - 1)
    .ok_or_else(|| EngineError::InvalidInput("semantic-object FileRecord chunk-count bound overflow".to_string()))?
    / DEFAULT_CHUNK_SIZE;
  let value_length = 2usize
    .checked_add(path.len())
    .and_then(|length| length.checked_add(2 + SEMANTIC_OBJECT_CONTENT_TYPE.len()))
    .and_then(|length| length.checked_add(8 + 8 + 8))
    .and_then(|length| length.checked_add(hash_length))
    .and_then(|length| length.checked_add(4 + encoded_cap + 4))
    .and_then(|length| maximum_chunk_count.checked_mul(hash_length).and_then(|hashes| length.checked_add(hashes)))
    .ok_or_else(|| EngineError::InvalidInput("semantic-object FileRecord value bound overflow".to_string()))?;
  if value_length > u32::MAX as usize {
    return Err(EngineError::InvalidInput("semantic-object FileRecord value bound exceeds u32".to_string()));
  }
  Ok(value_length as u32)
}

fn format_error(error: FormatError) -> EngineError {
  EngineError::InvalidInput(format!("invalid semantic-object record: {error}"))
}
