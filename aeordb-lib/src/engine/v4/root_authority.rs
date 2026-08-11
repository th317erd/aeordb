use std::error::Error;
use std::fmt::{self, Display, Formatter};

use crate::engine::HashAlgorithm;

use super::namespace::{
  NamespaceRootV1, NamespaceTreeRootV0, SemanticObjectKind, SemanticStateV1, decode_namespace_root_entity, decode_namespace_tree_root_v0,
  decode_semantic_object,
};
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use super::system_control::{SystemControlKindV1, decode_system_control};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootAuthorityReferenceRoleV1 {
  NamespaceRoot,
  NamespaceTreeRoot,
  SemanticStateRoot,
  RootAdmissionCommit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootAuthorityReadError {
  Missing { role: RootAuthorityReferenceRoleV1, identity: Vec<u8> },
  Invalid { role: RootAuthorityReferenceRoleV1, identity: Vec<u8>, source: FormatError },
}

impl RootAuthorityReadError {
  pub fn role(&self) -> RootAuthorityReferenceRoleV1 {
    match self {
      Self::Missing { role, .. } | Self::Invalid { role, .. } => *role,
    }
  }

  pub fn code(&self) -> &'static str {
    match self {
      Self::Missing { .. } => "missing_immutable_reference",
      Self::Invalid { source, .. } => source.code(),
    }
  }

  pub fn identity(&self) -> &[u8] {
    match self {
      Self::Missing { identity, .. } | Self::Invalid { identity, .. } => identity,
    }
  }
}

impl Display for RootAuthorityReadError {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Missing { role, identity } => write!(formatter, "missing {role:?} {}", hex::encode(identity)),
      Self::Invalid { role, identity, source } => write!(formatter, "invalid {role:?} {}: {source}", hex::encode(identity)),
    }
  }
}

impl Error for RootAuthorityReadError {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Missing { .. } => None,
      Self::Invalid { source, .. } => Some(source),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum RootAuthorityKindV1 {
  Head = 1,
  Snapshot = 2,
  Fork = 3,
  SyncBase = 4,
  MigrationMap = 5,
}

impl RootAuthorityKindV1 {
  fn from_u16(value: u16) -> Option<Self> {
    match value {
      1 => Some(Self::Head),
      2 => Some(Self::Snapshot),
      3 => Some(Self::Fork),
      4 => Some(Self::SyncBase),
      5 => Some(Self::MigrationMap),
      _ => None,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootAdmissionCommitV1 {
  pub database_id: [u8; 16],
  pub namespace_root: Vec<u8>,
  pub transaction_id: [u8; 16],
  pub publication_started_at_ms: i64,
  pub authority_kind: RootAuthorityKindV1,
  pub recovered_from_selected_authority: bool,
  pub authority_identity_digest: Vec<u8>,
  pub authority_after: Vec<u8>,
  pub selected_header_slot_sequence: u64,
  pub publication_sequence: u64,
  pub prepare_payload_hash: Vec<u8>,
}

pub fn decode_root_admission_commit(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<RootAdmissionCommitV1> {
  let control = decode_system_control(value, hash_algorithm)?;
  if control.kind != SystemControlKindV1::RootAdmissionCommit {
    return Err(format_error(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "root_admission_control_kind",
      format!("expected RootAdmissionCommit, got {:?}", control.kind),
    ));
  }
  let hash_width = hash_algorithm.hash_length();
  let body = control.body;
  let authority_kind_value = u16_at(body, 40 + hash_width)?;
  let authority_kind = RootAuthorityKindV1::from_u16(authority_kind_value).ok_or_else(|| {
    format_error(MalformedInputClass::UnknownTypeKindOrEnum, "root_admission_authority_kind", format!("kind {authority_kind_value}"))
  })?;
  let root_format = u16_at(body, 42 + hash_width)?;
  if root_format != 1 {
    return Err(format_error(MalformedInputClass::UnknownMagicOrVersion, "root_admission_root_format", format!("format {root_format}")));
  }
  let flags = u32_at(body, 44 + hash_width)?;
  Ok(RootAdmissionCommitV1 {
    database_id: array_16_at(body, 0)?,
    namespace_root: body[16..16 + hash_width].to_vec(),
    transaction_id: array_16_at(body, 16 + hash_width)?,
    publication_started_at_ms: i64_at(body, 32 + hash_width)?,
    authority_kind,
    recovered_from_selected_authority: flags & 1 != 0,
    authority_identity_digest: body[48 + hash_width..48 + 2 * hash_width].to_vec(),
    authority_after: body[48 + 2 * hash_width..48 + 3 * hash_width].to_vec(),
    selected_header_slot_sequence: u64_at(body, 48 + 3 * hash_width)?,
    publication_sequence: u64_at(body, 56 + 3 * hash_width)?,
    prepare_payload_hash: body[64 + 3 * hash_width..64 + 4 * hash_width].to_vec(),
  })
}

#[derive(Debug, Clone)]
pub struct ImmutableNamespaceAuthorityInputV1<'a> {
  pub expected_root_hash: &'a [u8],
  pub expected_database_id: &'a [u8; 16],
  pub root_entity: Option<&'a [u8]>,
  pub namespace_tree_entity: Option<&'a [u8]>,
  pub semantic_state_object: Option<&'a [u8]>,
  pub admission_control: Option<&'a [u8]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImmutableNamespaceAuthorityV1 {
  pub root: NamespaceRootV1,
  pub namespace_tree: NamespaceTreeRootV0,
  pub semantic_state: SemanticStateV1,
  pub admission: RootAdmissionCommitV1,
}

pub fn decode_immutable_namespace_authority(
  input: ImmutableNamespaceAuthorityInputV1<'_>,
  hash_algorithm: HashAlgorithm,
  write_sequence_high_water: u64,
) -> Result<ImmutableNamespaceAuthorityV1, RootAuthorityReadError> {
  let hash_width = hash_algorithm.hash_length();
  if input.expected_root_hash.len() != hash_width {
    return invalid(
      RootAuthorityReferenceRoleV1::NamespaceRoot,
      input.expected_root_hash,
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "namespace_root_hash_width",
      format!("expected {hash_width}, got {}", input.expected_root_hash.len()),
    );
  }
  let root_bytes = required(input.root_entity, RootAuthorityReferenceRoleV1::NamespaceRoot, input.expected_root_hash)?;
  let root = map_invalid(
    RootAuthorityReferenceRoleV1::NamespaceRoot,
    input.expected_root_hash,
    decode_namespace_root_entity(root_bytes, hash_algorithm, write_sequence_high_water),
  )?;
  if root.root_hash != input.expected_root_hash {
    return invalid(
      RootAuthorityReferenceRoleV1::NamespaceRoot,
      input.expected_root_hash,
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "namespace_root_identity_mismatch",
      "decoded root does not match the requested root hash",
    );
  }

  let namespace_tree_bytes =
    required(input.namespace_tree_entity, RootAuthorityReferenceRoleV1::NamespaceTreeRoot, &root.namespace_tree_root)?;
  let namespace_tree = map_invalid(
    RootAuthorityReferenceRoleV1::NamespaceTreeRoot,
    &root.namespace_tree_root,
    decode_namespace_tree_root_v0(namespace_tree_bytes, &root.namespace_tree_root, hash_algorithm, write_sequence_high_water),
  )?;

  let semantic_bytes = required(input.semantic_state_object, RootAuthorityReferenceRoleV1::SemanticStateRoot, &root.semantic_state_root)?;
  let semantic_object = map_invalid(
    RootAuthorityReferenceRoleV1::SemanticStateRoot,
    &root.semantic_state_root,
    decode_semantic_object(semantic_bytes, hash_algorithm),
  )?;
  if semantic_object.object_id != root.semantic_state_root {
    return invalid(
      RootAuthorityReferenceRoleV1::SemanticStateRoot,
      &root.semantic_state_root,
      MalformedInputClass::CrossRecordClosureMismatch,
      "semantic_state_identity_mismatch",
      "semantic object identity does not match the NamespaceRoot edge",
    );
  }
  if !matches!(semantic_object.kind, SemanticObjectKind::State { .. }) {
    return invalid(
      RootAuthorityReferenceRoleV1::SemanticStateRoot,
      &root.semantic_state_root,
      MalformedInputClass::CrossRecordClosureMismatch,
      "semantic_state_kind_mismatch",
      "NamespaceRoot semantic edge does not name a SemanticStateRoot",
    );
  }
  let semantic_state = semantic_object.semantic_state.ok_or_else(|| RootAuthorityReadError::Invalid {
    role: RootAuthorityReferenceRoleV1::SemanticStateRoot,
    identity: root.semantic_state_root.clone(),
    source: format_error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "semantic_state_fields_missing",
      "decoded SemanticStateRoot omitted its typed authority fields",
    ),
  })?;

  let admission_bytes = required(input.admission_control, RootAuthorityReferenceRoleV1::RootAdmissionCommit, input.expected_root_hash)?;
  let admission = map_invalid(
    RootAuthorityReferenceRoleV1::RootAdmissionCommit,
    input.expected_root_hash,
    decode_root_admission_commit(admission_bytes, hash_algorithm),
  )?;
  if admission.namespace_root != root.root_hash {
    return invalid(
      RootAuthorityReferenceRoleV1::RootAdmissionCommit,
      input.expected_root_hash,
      MalformedInputClass::CrossRecordClosureMismatch,
      "root_admission_identity_mismatch",
      "admission commit names another NamespaceRoot",
    );
  }
  if &admission.database_id != input.expected_database_id {
    return invalid(
      RootAuthorityReferenceRoleV1::RootAdmissionCommit,
      input.expected_root_hash,
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "root_admission_database_mismatch",
      "admission commit belongs to another logical database",
    );
  }

  Ok(ImmutableNamespaceAuthorityV1 { root, namespace_tree, semantic_state, admission })
}

fn required<'a>(value: Option<&'a [u8]>, role: RootAuthorityReferenceRoleV1, identity: &[u8]) -> Result<&'a [u8], RootAuthorityReadError> {
  value.ok_or_else(|| RootAuthorityReadError::Missing { role, identity: identity.to_vec() })
}

fn map_invalid<T>(role: RootAuthorityReferenceRoleV1, identity: &[u8], result: FormatResult<T>) -> Result<T, RootAuthorityReadError> {
  result.map_err(|source| RootAuthorityReadError::Invalid { role, identity: identity.to_vec(), source })
}

fn invalid<T>(
  role: RootAuthorityReferenceRoleV1,
  identity: &[u8],
  class: MalformedInputClass,
  code: &'static str,
  context: impl Into<String>,
) -> Result<T, RootAuthorityReadError> {
  Err(RootAuthorityReadError::Invalid { role, identity: identity.to_vec(), source: format_error(class, code, context) })
}

fn format_error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}

fn array_16_at(bytes: &[u8], offset: usize) -> FormatResult<[u8; 16]> {
  let value = bytes.get(offset..offset + 16).ok_or_else(root_admission_bounds_error)?;
  let mut result = [0u8; 16];
  result.copy_from_slice(value);
  Ok(result)
}

fn u16_at(bytes: &[u8], offset: usize) -> FormatResult<u16> {
  let value = bytes.get(offset..offset + 2).ok_or_else(root_admission_bounds_error)?;
  Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn u32_at(bytes: &[u8], offset: usize) -> FormatResult<u32> {
  let value = bytes.get(offset..offset + 4).ok_or_else(root_admission_bounds_error)?;
  Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn u64_at(bytes: &[u8], offset: usize) -> FormatResult<u64> {
  let value = bytes.get(offset..offset + 8).ok_or_else(root_admission_bounds_error)?;
  Ok(u64::from_le_bytes([value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7]]))
}

fn i64_at(bytes: &[u8], offset: usize) -> FormatResult<i64> {
  let value = bytes.get(offset..offset + 8).ok_or_else(root_admission_bounds_error)?;
  Ok(i64::from_le_bytes([value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7]]))
}

fn root_admission_bounds_error() -> FormatError {
  format_error(
    MalformedInputClass::TruncationOrTrailingBytes,
    "root_admission_bounds",
    "validated root admission body is shorter than its typed field layout",
  )
}
