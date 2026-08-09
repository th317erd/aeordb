//! Selection and v3 transition publication for controls in canonical A/B/I slots.
//!
//! Pure selection callers retain ownership of bounded slot buffers. The
//! transition adapter is the sole v3 FileRecord owner and publishes only the
//! approved system-flagged v0 representation.

use super::reader::{FormatError, FormatResult, MalformedInputClass};
use super::system_control::{
  SYSTEM_CONTROL_IDENTITY_LENGTH_CAP, SystemControlKindV1, SystemControlSelectionV1, SystemControlSlotV1, SystemControlV1,
  decode_system_control, select_system_control_pair, system_control_path,
};
use crate::engine::HashAlgorithm;
use crate::engine::directory_ops::{DirectoryOps, file_path_hash};
use crate::engine::entry_header::FLAG_SYSTEM;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::storage_engine::{NamespaceWriteGuard, StorageEngine};

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
    let target_slot = match current.as_ref().map(|selected| selected.selected_slot) {
      Some(SystemControlSlotV1::A) => SystemControlSlotV1::B,
      Some(SystemControlSlotV1::B) | None => SystemControlSlotV1::A,
      Some(SystemControlSlotV1::Immutable) => unreachable!("mutable selection cannot choose immutable slot"),
    };
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
    let path_key = file_path_hash(path, &self.engine.hash_algo())?;
    const TRANSITION_FILE_RECORD_VALUE_CAP: u32 = 4_096;
    let Some((header, _, value)) = self.engine.get_entry_verified_bounded(&path_key, TRANSITION_FILE_RECORD_VALUE_CAP)? else {
      return Ok(None);
    };
    if header.entry_type != EntryType::FileRecord {
      return Err(EngineError::InvalidInput(format!("transition control {path} path key does not resolve to a FileRecord")));
    }
    if header.flags & FLAG_SYSTEM == 0 || header.entry_version != 0 {
      return Err(EngineError::InvalidInput(format!("transition control {path} is not a system-flagged FileRecord v0")));
    }
    let record = FileRecord::deserialize(&value, self.engine.hash_algo().hash_length(), header.entry_version)?;
    if record.path != path {
      return Err(EngineError::InvalidInput(format!("transition control path-key mismatch for {path}")));
    }
    let encoded_cap = kind.encoded_cap();
    if record.total_size > encoded_cap as u64 {
      return Err(EngineError::InvalidInput(format!("transition control {path} length {} exceeds cap {encoded_cap}", record.total_size)));
    }
    let bytes = DirectoryOps::new(self.engine).read_file_buffered(path)?;
    if bytes.len() as u64 != record.total_size {
      return Err(EngineError::InvalidInput(format!("transition control {path} FileRecord size does not match content")));
    }
    Ok(Some(bytes))
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
  let bytes = match selection.selected_slot {
    SystemControlSlotV1::A => a.as_ref().expect("selected A slot is present").clone(),
    SystemControlSlotV1::B => b.as_ref().expect("selected B slot is present").clone(),
    SystemControlSlotV1::Immutable => unreachable!("mutable selection cannot choose immutable slot"),
  };
  Ok(Some(LoadedMutableControlV1 {
    database_id: selection.control.database_id.try_into().expect("validated control database ID width"),
    selected_slot: selection.selected_slot,
    sequence: selection.control.sequence,
    redundancy_degraded: selection.redundancy_degraded,
    bytes,
  }))
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
