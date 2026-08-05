//! Read-only selection for v4 controls stored in canonical A/B/I slots.
//!
//! This module does not read paths or write controls. Callers retain ownership
//! of the bounded slot buffers and receive borrowed decoded bodies.

use super::reader::{FormatError, FormatResult, MalformedInputClass};
use super::system_control::{
  SYSTEM_CONTROL_IDENTITY_LENGTH_CAP, SystemControlKindV1, SystemControlSelectionV1, SystemControlSlotV1, SystemControlV1,
  decode_system_control, select_system_control_pair,
};
use crate::engine::HashAlgorithm;

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
  if control.kind != expected_kind {
    return Err(identity_error("control_store_kind", "selected control kind does not match requested kind"));
  }
  if control.database_id != expected_database_id {
    return Err(identity_error("control_store_database_id", "selected control belongs to a different database"));
  }
  if control.identity != expected_identity {
    return Err(identity_error("control_store_identity", "selected control identity does not match its canonical key"));
  }
  Ok(())
}

fn identity_error(code: &'static str, detail: &'static str) -> FormatError {
  FormatError::new(MalformedInputClass::IdentityKeyOrGenerationMismatch, code, detail)
}
