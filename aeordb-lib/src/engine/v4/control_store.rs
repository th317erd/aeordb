//! Selection and versioned publication for controls in canonical A/B/I slots.
//!
//! Pure selection callers retain ownership of bounded slot buffers. The
//! transition adapter publishes the approved system-flagged v0 representation;
//! the disconnected v4 adapter publishes the frozen FileRecord v1 wrapper.

use super::reader::{FormatError, FormatResult, MalformedInputClass};
use super::system_control::{
  SYSTEM_CONTROL_IDENTITY_LENGTH_CAP, SystemControlKindV1, SystemControlSelectionV1, SystemControlSlotV1, SystemControlV1,
  decode_system_control, select_system_control_pair, system_control_path,
};
use crate::engine::HashAlgorithm;
use crate::engine::directory_ops::{DEFAULT_CHUNK_SIZE, DirectoryOps, file_path_hash, whole_file_content_hash};
use crate::engine::entry_header::FLAG_SYSTEM;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::{CURRENT_FILE_RECORD_VERSION, FileRecord};
use crate::engine::storage_engine::{NamespaceWriteGuard, StorageEngine};

pub const SYSTEM_CONTROL_CONTENT_TYPE: &str = "application/vnd.aeordb.system-control";

#[derive(Clone, Copy, Debug, Default)]
pub struct ControlStoreSlotsV1<'a> {
  pub a: Option<&'a [u8]>,
  pub b: Option<&'a [u8]>,
  pub immutable: Option<&'a [u8]>,
}

#[derive(Clone, Debug)]
pub enum ControlStoreReadV1<'a> {
  Absent,
  Mutable(SystemControlSelectionV1<'a>),
  Immutable(SystemControlV1<'a>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedMutableControlV1 {
  pub database_id: [u8; 16],
  pub selected_slot: SystemControlSlotV1,
  pub sequence: u64,
  pub redundancy_degraded: bool,
  pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedImmutableControlV1 {
  pub database_id: [u8; 16],
  pub sequence: u64,
  pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ControlFileRecordPolicy {
  TransitionV0,
  V4Compatible,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LoadedControlFileRecord {
  entry_version: u8,
  bytes: Vec<u8>,
}

/// Typed proof that ControlStore owns namespace authority for one mutable v0
/// control publication. Only this module can construct the context; the
/// DirectoryOps adapter derives the canonical path from its typed fields.
pub(crate) struct V3ControlPublicationContextV0<'a> {
  engine: &'a StorageEngine,
  kind: SystemControlKindV1,
  identity: Vec<u8>,
  _authority: NamespaceWriteGuard<'a>,
}

impl<'a> V3ControlPublicationContextV0<'a> {
  fn new(engine: &'a StorageEngine, kind: SystemControlKindV1, identity: &[u8]) -> EngineResult<Self> {
    if kind.is_immutable() {
      return Err(EngineError::InvalidInput("v3 transition ControlStore only publishes mutable A/B controls".to_string()));
    }
    system_control_path(kind, identity, SystemControlSlotV1::A).map_err(format_error)?;
    let authority = engine.namespace_write_guard()?;
    Ok(Self { engine, kind, identity: identity.to_vec(), _authority: authority })
  }

  pub(crate) fn engine(&self) -> &StorageEngine {
    self.engine
  }

  pub(crate) fn target_path(&self, slot: SystemControlSlotV1) -> EngineResult<String> {
    if slot == SystemControlSlotV1::Immutable {
      return Err(EngineError::InvalidInput("mutable v0 ControlStore publication requires an A/B slot".to_string()));
    }
    system_control_path(self.kind, &self.identity, slot).map_err(format_error)
  }
}

/// Typed proof that v4 ControlStore owns namespace authority for one canonical
/// v1 control publication. String paths cannot construct this capability.
pub(crate) struct V4ControlPublicationContextV1<'a> {
  engine: &'a StorageEngine,
  kind: SystemControlKindV1,
  identity: Vec<u8>,
  _authority: NamespaceWriteGuard<'a>,
}

impl<'a> V4ControlPublicationContextV1<'a> {
  fn new(engine: &'a StorageEngine, kind: SystemControlKindV1, identity: &[u8]) -> EngineResult<Self> {
    let default_slot = if kind.is_immutable() { SystemControlSlotV1::Immutable } else { SystemControlSlotV1::A };
    system_control_path(kind, identity, default_slot).map_err(format_error)?;
    let authority = engine.namespace_write_guard()?;
    Ok(Self { engine, kind, identity: identity.to_vec(), _authority: authority })
  }

  pub(crate) fn engine(&self) -> &StorageEngine {
    self.engine
  }

  pub(crate) fn target_path(&self, slot: SystemControlSlotV1) -> EngineResult<String> {
    system_control_path(self.kind, &self.identity, slot).map_err(format_error)
  }
}

pub struct V3TransitionControlStore<'a> {
  engine: &'a StorageEngine,
}

impl<'a> V3TransitionControlStore<'a> {
  pub fn new(engine: &'a StorageEngine) -> Self {
    Self { engine }
  }

  pub fn load_mutable(
    &self,
    kind: SystemControlKindV1,
    database_id: [u8; 16],
    identity: &[u8],
  ) -> EngineResult<Option<LoadedMutableControlV1>> {
    let selected = self.discover_mutable(kind, identity)?;
    if let Some(selected) = selected.as_ref() {
      if selected.database_id != database_id {
        return Err(EngineError::InvalidInput("selected transition control belongs to a different database".to_string()));
      }
    }
    Ok(selected)
  }

  pub fn discover_mutable(&self, kind: SystemControlKindV1, identity: &[u8]) -> EngineResult<Option<LoadedMutableControlV1>> {
    if kind.is_immutable() {
      return Err(EngineError::InvalidInput("v3 transition ControlStore only publishes mutable A/B controls".to_string()));
    }
    let a_path = system_control_path(kind, identity, SystemControlSlotV1::A).map_err(format_error)?;
    let b_path = system_control_path(kind, identity, SystemControlSlotV1::B).map_err(format_error)?;
    let a = self.load_slot(kind, &a_path)?;
    let b = self.load_slot(kind, &b_path)?;
    discover_mutable_control(self.engine.hash_algo(), kind, identity, a, b).map_err(format_error)
  }

  pub fn publish_mutable(
    &self,
    kind: SystemControlKindV1,
    database_id: [u8; 16],
    identity: &[u8],
    bytes: &[u8],
  ) -> EngineResult<LoadedMutableControlV1> {
    if kind.is_immutable() {
      return Err(EngineError::InvalidInput("v3 transition ControlStore only publishes mutable A/B controls".to_string()));
    }
    let incoming = decode_system_control(bytes, self.engine.hash_algo()).map_err(format_error)?;
    verify_expected(&incoming, kind, database_id, identity).map_err(format_error)?;

    // The typed context owns sequence selection, the one DirectoryOps
    // coordinator publication, and selected-state read-back under the same
    // namespace authority lifetime without granting access by string path.
    let publication = V3ControlPublicationContextV0::new(self.engine, kind, identity)?;
    let current = self.load_mutable(kind, database_id, identity)?;
    let expected_sequence = match current.as_ref() {
      Some(selected) => {
        selected.sequence.checked_add(1).ok_or_else(|| EngineError::InvalidInput("control sequence exhausted".to_string()))?
      }
      None => incoming.sequence,
    };
    if incoming.sequence != expected_sequence {
      return Err(EngineError::InvalidInput(format!(
        "ControlStore expected next sequence {}, received {}",
        expected_sequence, incoming.sequence
      )));
    }
    let target_slot = next_mutable_publication_slot(current.as_ref().map(|selected| selected.selected_slot))?;
    let target_path = publication.target_path(target_slot)?;
    DirectoryOps::new(self.engine).store_transition_control_v0(&publication, target_slot, bytes)?;
    let read_back = self.load_slot(kind, &target_path)?.ok_or_else(|| EngineError::NotFound(target_path.clone()))?;
    if read_back != bytes {
      return Err(EngineError::DurabilityFailure(format!("ControlStore read-back mismatch for {target_path}")));
    }
    let selected = self
      .load_mutable(kind, database_id, identity)?
      .ok_or_else(|| EngineError::DurabilityFailure("ControlStore publication was not selected after read-back".to_string()))?;
    if selected.selected_slot != target_slot || selected.sequence != incoming.sequence || selected.bytes != bytes {
      return Err(EngineError::DurabilityFailure("ControlStore selected state does not match the published inactive slot".to_string()));
    }
    Ok(selected)
  }

  fn load_slot(&self, kind: SystemControlKindV1, path: &str) -> EngineResult<Option<Vec<u8>>> {
    load_control_file_record(self.engine, kind, path, ControlFileRecordPolicy::TransitionV0).map(|loaded| loaded.map(|loaded| loaded.bytes))
  }
}

/// Disconnected v4 ControlStore writer. P3a-4 deliberately exposes no service
/// caller; later capability activation owns that separate start gate.
pub struct V4ControlStore<'a> {
  engine: &'a StorageEngine,
}

impl<'a> V4ControlStore<'a> {
  pub fn new(engine: &'a StorageEngine) -> Self {
    Self { engine }
  }

  pub fn load_mutable(
    &self,
    kind: SystemControlKindV1,
    database_id: [u8; 16],
    identity: &[u8],
  ) -> EngineResult<Option<LoadedMutableControlV1>> {
    let selected = self.discover_mutable(kind, identity)?;
    if let Some(selected) = selected.as_ref() {
      if selected.database_id != database_id {
        return Err(EngineError::InvalidInput("selected v4 control belongs to a different database".to_string()));
      }
    }
    Ok(selected)
  }

  pub fn discover_mutable(&self, kind: SystemControlKindV1, identity: &[u8]) -> EngineResult<Option<LoadedMutableControlV1>> {
    if kind.is_immutable() {
      return Err(EngineError::InvalidInput("v4 mutable ControlStore requires an A/B control kind".to_string()));
    }
    let a_path = system_control_path(kind, identity, SystemControlSlotV1::A).map_err(format_error)?;
    let b_path = system_control_path(kind, identity, SystemControlSlotV1::B).map_err(format_error)?;
    let a = self.load_slot(kind, &a_path)?;
    let b = self.load_slot(kind, &b_path)?;
    discover_mutable_control(self.engine.hash_algo(), kind, identity, a.map(|loaded| loaded.bytes), b.map(|loaded| loaded.bytes))
      .map_err(format_error)
  }

  pub fn load_immutable(
    &self,
    kind: SystemControlKindV1,
    database_id: [u8; 16],
    identity: &[u8],
  ) -> EngineResult<Option<LoadedImmutableControlV1>> {
    if !kind.is_immutable() {
      return Err(EngineError::InvalidInput("v4 immutable ControlStore requires an I-slot control kind".to_string()));
    }
    let path = system_control_path(kind, identity, SystemControlSlotV1::Immutable).map_err(format_error)?;
    let Some(loaded) = self.load_slot(kind, &path)? else {
      return Ok(None);
    };
    loaded_immutable_control(self.engine.hash_algo(), kind, database_id, identity, &loaded.bytes).map(Some).map_err(format_error)
  }

  pub fn publish_mutable(
    &self,
    kind: SystemControlKindV1,
    database_id: [u8; 16],
    identity: &[u8],
    bytes: &[u8],
  ) -> EngineResult<LoadedMutableControlV1> {
    if kind.is_immutable() {
      return Err(EngineError::InvalidInput("v4 mutable ControlStore requires an A/B control kind".to_string()));
    }
    let incoming = decode_system_control(bytes, self.engine.hash_algo()).map_err(format_error)?;
    verify_expected(&incoming, kind, database_id, identity).map_err(format_error)?;

    let publication = V4ControlPublicationContextV1::new(self.engine, kind, identity)?;
    let current = self.load_mutable(kind, database_id, identity)?;
    let expected_sequence = match current.as_ref() {
      Some(selected) => {
        selected.sequence.checked_add(1).ok_or_else(|| EngineError::InvalidInput("control sequence exhausted".to_string()))?
      }
      None => incoming.sequence,
    };
    if incoming.sequence != expected_sequence {
      return Err(EngineError::InvalidInput(format!(
        "ControlStore expected next sequence {}, received {}",
        expected_sequence, incoming.sequence
      )));
    }

    let target_slot = next_mutable_publication_slot(current.as_ref().map(|selected| selected.selected_slot))?;
    let target_path = publication.target_path(target_slot)?;
    self.publish_slot(&publication, target_slot, bytes)?;
    let read_back = self.load_slot(kind, &target_path)?.ok_or_else(|| EngineError::NotFound(target_path.clone()))?;
    if read_back.entry_version != CURRENT_FILE_RECORD_VERSION || read_back.bytes != bytes {
      return Err(EngineError::DurabilityFailure(format!("v4 ControlStore read-back mismatch for {target_path}")));
    }
    let selected = self
      .load_mutable(kind, database_id, identity)?
      .ok_or_else(|| EngineError::DurabilityFailure("v4 ControlStore publication was not selected after read-back".to_string()))?;
    if selected.selected_slot != target_slot || selected.sequence != incoming.sequence || selected.bytes != bytes {
      return Err(EngineError::DurabilityFailure("v4 ControlStore selected state does not match the published inactive slot".to_string()));
    }
    Ok(selected)
  }

  pub fn publish_immutable(
    &self,
    kind: SystemControlKindV1,
    database_id: [u8; 16],
    identity: &[u8],
    bytes: &[u8],
  ) -> EngineResult<LoadedImmutableControlV1> {
    if !kind.is_immutable() {
      return Err(EngineError::InvalidInput("v4 immutable ControlStore requires an I-slot control kind".to_string()));
    }
    let incoming = decode_system_control(bytes, self.engine.hash_algo()).map_err(format_error)?;
    verify_expected(&incoming, kind, database_id, identity).map_err(format_error)?;

    let publication = V4ControlPublicationContextV1::new(self.engine, kind, identity)?;
    let target_path = publication.target_path(SystemControlSlotV1::Immutable)?;
    if let Some(existing) = self.load_slot(kind, &target_path)? {
      loaded_immutable_control(self.engine.hash_algo(), kind, database_id, identity, &existing.bytes).map_err(format_error)?;
      if existing.bytes != bytes {
        return Err(EngineError::InvalidInput("immutable ControlStore path already contains different validated bytes".to_string()));
      }
      if existing.entry_version == CURRENT_FILE_RECORD_VERSION {
        return loaded_immutable_control(self.engine.hash_algo(), kind, database_id, identity, &existing.bytes).map_err(format_error);
      }
    }

    self.publish_slot(&publication, SystemControlSlotV1::Immutable, bytes)?;
    let read_back = self.load_slot(kind, &target_path)?.ok_or_else(|| EngineError::NotFound(target_path.clone()))?;
    if read_back.entry_version != CURRENT_FILE_RECORD_VERSION || read_back.bytes != bytes {
      return Err(EngineError::DurabilityFailure(format!("v4 immutable ControlStore read-back mismatch for {target_path}")));
    }
    loaded_immutable_control(self.engine.hash_algo(), kind, database_id, identity, &read_back.bytes).map_err(format_error)
  }

  fn load_slot(&self, kind: SystemControlKindV1, path: &str) -> EngineResult<Option<LoadedControlFileRecord>> {
    load_control_file_record(self.engine, kind, path, ControlFileRecordPolicy::V4Compatible)
  }

  fn publish_slot(
    &self,
    publication: &V4ControlPublicationContextV1<'_>,
    target_slot: SystemControlSlotV1,
    bytes: &[u8],
  ) -> EngineResult<FileRecord> {
    DirectoryOps::new(self.engine).store_control_file_record_v1(publication, target_slot, bytes)
  }
}

fn load_control_file_record(
  engine: &StorageEngine,
  kind: SystemControlKindV1,
  path: &str,
  policy: ControlFileRecordPolicy,
) -> EngineResult<Option<LoadedControlFileRecord>> {
  let path_key = file_path_hash(path, &engine.hash_algo())?;
  let value_cap = match policy {
    ControlFileRecordPolicy::TransitionV0 => 4_096,
    ControlFileRecordPolicy::V4Compatible => control_file_record_value_cap(kind, path, engine.hash_algo().hash_length())?,
  };
  let Some((header, _, value)) = engine.get_entry_verified_bounded(&path_key, value_cap)? else {
    return Ok(None);
  };
  if header.entry_type != EntryType::FileRecord {
    return Err(EngineError::InvalidInput(format!("control {path} path key does not resolve to a FileRecord")));
  }
  if policy == ControlFileRecordPolicy::TransitionV0 && (header.flags & FLAG_SYSTEM == 0 || header.entry_version != 0) {
    return Err(EngineError::InvalidInput(format!("transition control {path} is not a system-flagged FileRecord v0")));
  }
  if policy == ControlFileRecordPolicy::V4Compatible
    && (header.flags & FLAG_SYSTEM == 0 || !matches!(header.entry_version, 0 | CURRENT_FILE_RECORD_VERSION))
  {
    return Err(EngineError::InvalidInput(format!("v4 control {path} is not a system-flagged compatible FileRecord")));
  }

  let record = FileRecord::deserialize(&value, engine.hash_algo().hash_length(), header.entry_version)?;
  if record.path != path {
    return Err(EngineError::InvalidInput(format!("control path-key mismatch for {path}")));
  }
  let encoded_cap = kind.encoded_cap();
  if record.total_size > encoded_cap as u64 {
    return Err(EngineError::InvalidInput(format!("control {path} length {} exceeds cap {encoded_cap}", record.total_size)));
  }
  if policy == ControlFileRecordPolicy::V4Compatible && header.entry_version == CURRENT_FILE_RECORD_VERSION {
    if record.content_type.as_deref() != Some(SYSTEM_CONTROL_CONTENT_TYPE) {
      return Err(EngineError::InvalidInput(format!("v4 control {path} content type is not {SYSTEM_CONTROL_CONTENT_TYPE}")));
    }
    if !record.metadata.is_empty() {
      return Err(EngineError::InvalidInput(format!("v4 control {path} FileRecord metadata must be empty")));
    }
  }

  let bytes = DirectoryOps::new(engine).read_file_record_body_bounded(&record, encoded_cap as u64)?;
  if policy == ControlFileRecordPolicy::V4Compatible && header.entry_version == CURRENT_FILE_RECORD_VERSION {
    let expected_content_hash = whole_file_content_hash(&bytes, &engine.hash_algo())?;
    if record.content_hash != expected_content_hash {
      return Err(EngineError::InvalidInput(format!("v4 control {path} content hash does not match its exact payload")));
    }
  }
  Ok(Some(LoadedControlFileRecord { entry_version: header.entry_version, bytes }))
}

fn control_file_record_value_cap(kind: SystemControlKindV1, path: &str, hash_length: usize) -> EngineResult<u32> {
  let encoded_cap = kind.encoded_cap();
  let maximum_chunk_count = encoded_cap
    .checked_add(DEFAULT_CHUNK_SIZE - 1)
    .ok_or_else(|| EngineError::InvalidInput("control FileRecord chunk-count bound overflow".to_string()))?
    / DEFAULT_CHUNK_SIZE;
  let maximum_content_type_length = SYSTEM_CONTROL_CONTENT_TYPE.len().max("application/octet-stream".len());
  let value_length = 2usize
    .checked_add(path.len())
    .and_then(|length| length.checked_add(2))
    .and_then(|length| length.checked_add(maximum_content_type_length))
    .and_then(|length| length.checked_add(8 + 8 + 8))
    .and_then(|length| length.checked_add(hash_length))
    .and_then(|length| length.checked_add(4 + 4))
    .and_then(|length| maximum_chunk_count.checked_mul(hash_length).and_then(|hashes| length.checked_add(hashes)))
    .ok_or_else(|| EngineError::InvalidInput("control FileRecord value bound overflow".to_string()))?;
  if value_length > u32::MAX as usize {
    return Err(EngineError::InvalidInput("control FileRecord value bound exceeds u32".to_string()));
  }
  Ok(value_length as u32)
}

fn loaded_immutable_control(
  algorithm: HashAlgorithm,
  kind: SystemControlKindV1,
  database_id: [u8; 16],
  identity: &[u8],
  bytes: &[u8],
) -> FormatResult<LoadedImmutableControlV1> {
  match select_control_store_read(algorithm, kind, database_id, identity, ControlStoreSlotsV1 { a: None, b: None, immutable: Some(bytes) })?
  {
    ControlStoreReadV1::Immutable(control) => {
      Ok(LoadedImmutableControlV1 { database_id, sequence: control.sequence, bytes: bytes.to_vec() })
    }
    ControlStoreReadV1::Absent | ControlStoreReadV1::Mutable(_) => {
      Err(identity_error("control_store_immutable_selection", "immutable ControlStore bytes did not select the I slot"))
    }
  }
}

/// Select one mutable A/B control from already bounded slot bytes.
///
/// This is shared by the live v3 ControlStore and deployment's read-only
/// database inspector so both paths apply exactly the same sequence, identity,
/// and torn-slot policy.
pub fn discover_mutable_control(
  algorithm: HashAlgorithm,
  kind: SystemControlKindV1,
  identity: &[u8],
  a: Option<Vec<u8>>,
  b: Option<Vec<u8>>,
) -> FormatResult<Option<LoadedMutableControlV1>> {
  if kind.is_immutable() {
    return Err(identity_error("control_store_discover_immutable", "mutable discovery cannot select an immutable control"));
  }
  let selection = match (a.as_deref(), b.as_deref()) {
    (None, None) => return Ok(None),
    (Some(a), Some(b)) => select_system_control_pair(algorithm, a, b)?,
    (Some(bytes), None) => select_single_mutable(algorithm, SystemControlSlotV1::A, bytes)?,
    (None, Some(bytes)) => select_single_mutable(algorithm, SystemControlSlotV1::B, bytes)?,
  };
  verify_kind_and_identity(&selection.control, kind, identity)?;
  loaded_mutable_control_from_selection(selection, a.as_deref(), b.as_deref()).map(Some)
}

fn next_mutable_publication_slot(current: Option<SystemControlSlotV1>) -> EngineResult<SystemControlSlotV1> {
  match current {
    Some(SystemControlSlotV1::A) => Ok(SystemControlSlotV1::B),
    Some(SystemControlSlotV1::B) | None => Ok(SystemControlSlotV1::A),
    Some(SystemControlSlotV1::Immutable) => {
      Err(EngineError::InvalidInput("mutable ControlStore selection unexpectedly references the immutable slot".to_string()))
    }
  }
}

fn loaded_mutable_control_from_selection(
  selection: SystemControlSelectionV1<'_>,
  a: Option<&[u8]>,
  b: Option<&[u8]>,
) -> FormatResult<LoadedMutableControlV1> {
  let selected_bytes = match selection.selected_slot {
    SystemControlSlotV1::A => a,
    SystemControlSlotV1::B => b,
    SystemControlSlotV1::Immutable => {
      return Err(identity_error(
        "control_store_mutable_selected_immutable",
        "mutable ControlStore selection unexpectedly references the immutable slot",
      ));
    }
  }
  .ok_or_else(|| identity_error("control_store_selected_slot_missing", "ControlStore selector referenced a slot that was not supplied"))?;
  let database_id = <[u8; 16]>::try_from(selection.control.database_id).map_err(|_| {
    FormatError::new(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "control_store_database_id_width",
      format!("selected database ID has length {}, expected 16", selection.control.database_id.len()),
    )
  })?;

  Ok(LoadedMutableControlV1 {
    database_id,
    selected_slot: selection.selected_slot,
    sequence: selection.control.sequence,
    redundancy_degraded: selection.redundancy_degraded,
    bytes: selected_bytes.to_vec(),
  })
}

pub fn select_control_store_read<'a>(
  algorithm: HashAlgorithm,
  expected_kind: SystemControlKindV1,
  expected_database_id: [u8; 16],
  expected_identity: &[u8],
  slots: ControlStoreSlotsV1<'a>,
) -> FormatResult<ControlStoreReadV1<'a>> {
  if expected_identity.len() > SYSTEM_CONTROL_IDENTITY_LENGTH_CAP {
    return Err(FormatError::new(
      MalformedInputClass::AllocationAmplification,
      "control_store_expected_identity_length",
      format!("{} exceeds cap {SYSTEM_CONTROL_IDENTITY_LENGTH_CAP}", expected_identity.len()),
    ));
  }
  if expected_kind.is_immutable() {
    return select_immutable(algorithm, expected_kind, expected_database_id, expected_identity, slots);
  }
  select_mutable(algorithm, expected_kind, expected_database_id, expected_identity, slots)
}

fn select_mutable<'a>(
  algorithm: HashAlgorithm,
  expected_kind: SystemControlKindV1,
  expected_database_id: [u8; 16],
  expected_identity: &[u8],
  slots: ControlStoreSlotsV1<'a>,
) -> FormatResult<ControlStoreReadV1<'a>> {
  if slots.immutable.is_some() {
    return Err(identity_error("control_store_mutable_i_slot", "mutable control received an immutable I slot"));
  }
  let selection = match (slots.a, slots.b) {
    (None, None) => return Ok(ControlStoreReadV1::Absent),
    (Some(a), Some(b)) => select_system_control_pair(algorithm, a, b)?,
    (Some(bytes), None) => select_single_mutable(algorithm, SystemControlSlotV1::A, bytes)?,
    (None, Some(bytes)) => select_single_mutable(algorithm, SystemControlSlotV1::B, bytes)?,
  };
  verify_expected(&selection.control, expected_kind, expected_database_id, expected_identity)?;
  Ok(ControlStoreReadV1::Mutable(selection))
}

fn select_single_mutable(algorithm: HashAlgorithm, slot: SystemControlSlotV1, bytes: &[u8]) -> FormatResult<SystemControlSelectionV1<'_>> {
  let control = decode_system_control(bytes, algorithm)?;
  if control.kind.is_immutable() {
    return Err(identity_error("control_store_mutable_slot_kind", "A/B slot contains an immutable control"));
  }
  Ok(SystemControlSelectionV1 { selected_slot: slot, control, redundancy_degraded: true })
}

fn select_immutable<'a>(
  algorithm: HashAlgorithm,
  expected_kind: SystemControlKindV1,
  expected_database_id: [u8; 16],
  expected_identity: &[u8],
  slots: ControlStoreSlotsV1<'a>,
) -> FormatResult<ControlStoreReadV1<'a>> {
  if slots.a.is_some() || slots.b.is_some() {
    return Err(identity_error("control_store_immutable_ab_slot", "immutable control received an A/B slot"));
  }
  let Some(bytes) = slots.immutable else {
    return Ok(ControlStoreReadV1::Absent);
  };
  let control = decode_system_control(bytes, algorithm)?;
  if !control.kind.is_immutable() {
    return Err(identity_error("control_store_immutable_slot_kind", "I slot contains a mutable control"));
  }
  verify_expected(&control, expected_kind, expected_database_id, expected_identity)?;
  Ok(ControlStoreReadV1::Immutable(control))
}

fn verify_expected(
  control: &SystemControlV1<'_>,
  expected_kind: SystemControlKindV1,
  expected_database_id: [u8; 16],
  expected_identity: &[u8],
) -> FormatResult<()> {
  verify_kind_and_identity(control, expected_kind, expected_identity)?;
  if control.database_id != expected_database_id {
    return Err(identity_error("control_store_database_id", "selected control belongs to a different database"));
  }
  Ok(())
}

fn verify_kind_and_identity(
  control: &SystemControlV1<'_>,
  expected_kind: SystemControlKindV1,
  expected_identity: &[u8],
) -> FormatResult<()> {
  if control.kind != expected_kind {
    return Err(identity_error("control_store_kind", "selected control kind does not match requested kind"));
  }
  if control.identity != expected_identity {
    return Err(identity_error("control_store_identity", "selected control identity does not match its canonical key"));
  }
  Ok(())
}

fn format_error(error: FormatError) -> EngineError {
  EngineError::InvalidInput(format!("invalid ControlStore record: {error}"))
}

fn identity_error(code: &'static str, detail: &'static str) -> FormatError {
  FormatError::new(MalformedInputClass::IdentityKeyOrGenerationMismatch, code, detail)
}

#[cfg(test)]
#[path = "../../../spec/engine/v4_control_store_internal_spec.rs"]
mod v4_control_store_internal_spec;
