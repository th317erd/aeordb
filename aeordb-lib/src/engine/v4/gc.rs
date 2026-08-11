use std::cmp::Ordering;

use super::hash::digest_parts;
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use crate::engine::HashAlgorithm;

const AGCA_HEADER_LENGTH: usize = 32;
pub const MAX_GC_ARTIFACT_LENGTH: usize = 64 * 1_024 * 1_024;
const MAX_GC_MANIFEST_LENGTH: usize = 1_024 * 1_024;
const MAX_GC_PAGE_LENGTH: usize = 16 * 1_024 * 1_024;
const MAX_GC_DIRECTORY_LENGTH: usize = 4 * 1_024 * 1_024;
const MAX_MARK_RUN_CHECKPOINT_LENGTH: usize = 32 + 40 + 256 * 1_024 + 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum GcArtifactKindV1 {
  QuarantineActiveControl = 0x0001,
  MarkRunActiveControl = 0x0002,
  PhysicalInventoryActiveControl = 0x0003,
  AuditCatalogActiveControl = 0x0004,
  VoidCatalogActiveControl = 0x0005,
  RootLifecycleActiveControl = 0x0006,
  QuarantineManifest = 0x0010,
  RootExpiryCatalogManifest = 0x0011,
  PhysicalInventoryManifest = 0x0012,
  MarkRunCheckpoint = 0x0013,
  AuditCatalogManifest = 0x0014,
  GcRunSummary = 0x0015,
  VoidCatalogManifest = 0x0016,
  RootLifecycleManifest = 0x0017,
  GcArtifactDirectoryNode = 0x001f,
  CandidatePage = 0x0020,
  CandidateDelta = 0x0021,
  RootExpiryPage = 0x0022,
  RetirementJournalSegment = 0x0023,
  PhysicalInventoryPage = 0x0024,
  MarkMutationJournalSegment = 0x0025,
  VoidExtentPage = 0x0026,
  VoidClaim = 0x0027,
  RootCandidatePage = 0x0028,
  SweepProposal = 0x0030,
  SweepCommitReceipt = 0x0031,
  RecoveredSweepReceipt = 0x0032,
  CorruptGcEvidence = 0x0033,
  AuditDetailPage = 0x0034,
  AuditSummaryPage = 0x0035,
  AuditPin = 0x0036,
  RootRetirementCommit = 0x0037,
  VoidClaimSettlementReceipt = 0x0038,
  RootObjectReclaimProof = 0x0039,
}

impl GcArtifactKindV1 {
  pub const ALL: [Self; 34] = [
    Self::QuarantineActiveControl,
    Self::MarkRunActiveControl,
    Self::PhysicalInventoryActiveControl,
    Self::AuditCatalogActiveControl,
    Self::VoidCatalogActiveControl,
    Self::RootLifecycleActiveControl,
    Self::QuarantineManifest,
    Self::RootExpiryCatalogManifest,
    Self::PhysicalInventoryManifest,
    Self::MarkRunCheckpoint,
    Self::AuditCatalogManifest,
    Self::GcRunSummary,
    Self::VoidCatalogManifest,
    Self::RootLifecycleManifest,
    Self::GcArtifactDirectoryNode,
    Self::CandidatePage,
    Self::CandidateDelta,
    Self::RootExpiryPage,
    Self::RetirementJournalSegment,
    Self::PhysicalInventoryPage,
    Self::MarkMutationJournalSegment,
    Self::VoidExtentPage,
    Self::VoidClaim,
    Self::RootCandidatePage,
    Self::SweepProposal,
    Self::SweepCommitReceipt,
    Self::RecoveredSweepReceipt,
    Self::CorruptGcEvidence,
    Self::AuditDetailPage,
    Self::AuditSummaryPage,
    Self::AuditPin,
    Self::RootRetirementCommit,
    Self::VoidClaimSettlementReceipt,
    Self::RootObjectReclaimProof,
  ];

  pub fn from_u16(value: u16) -> Option<Self> {
    Self::ALL.into_iter().find(|kind| *kind as u16 == value)
  }

  pub fn name(self) -> &'static str {
    match self {
      Self::QuarantineActiveControl => "quarantine",
      Self::MarkRunActiveControl => "mark-run",
      Self::PhysicalInventoryActiveControl => "physical-inventory",
      Self::AuditCatalogActiveControl => "audit-catalog",
      Self::VoidCatalogActiveControl => "void-catalog",
      Self::RootLifecycleActiveControl => "root-lifecycle",
      Self::QuarantineManifest => "quarantine-manifest",
      Self::RootExpiryCatalogManifest => "root-expiry-catalog-manifest",
      Self::PhysicalInventoryManifest => "physical-inventory-manifest",
      Self::MarkRunCheckpoint => "mark-run-checkpoint",
      Self::AuditCatalogManifest => "audit-catalog-manifest",
      Self::GcRunSummary => "gc-run-summary",
      Self::VoidCatalogManifest => "void-catalog-manifest",
      Self::RootLifecycleManifest => "root-lifecycle-manifest",
      Self::GcArtifactDirectoryNode => "gc-artifact-directory-node",
      Self::CandidatePage => "candidate-page",
      Self::CandidateDelta => "candidate-delta",
      Self::RootExpiryPage => "root-expiry-page",
      Self::RetirementJournalSegment => "retirement-journal-segment",
      Self::PhysicalInventoryPage => "physical-inventory-page",
      Self::MarkMutationJournalSegment => "mark-mutation-journal-segment",
      Self::VoidExtentPage => "void-extent-page",
      Self::VoidClaim => "void-claim",
      Self::RootCandidatePage => "root-candidate-page",
      Self::SweepProposal => "sweep-proposal",
      Self::SweepCommitReceipt => "sweep-commit-receipt",
      Self::RecoveredSweepReceipt => "recovered-sweep-receipt",
      Self::CorruptGcEvidence => "corrupt-gc-evidence",
      Self::AuditDetailPage => "audit-detail-page",
      Self::AuditSummaryPage => "audit-summary-page",
      Self::AuditPin => "audit-pin",
      Self::RootRetirementCommit => "root-retirement-commit",
      Self::VoidClaimSettlementReceipt => "void-claim-settlement-receipt",
      Self::RootObjectReclaimProof => "root-object-reclaim-proof",
    }
  }

  pub fn is_control(self) -> bool {
    matches!(
      self,
      Self::QuarantineActiveControl
        | Self::MarkRunActiveControl
        | Self::PhysicalInventoryActiveControl
        | Self::AuditCatalogActiveControl
        | Self::VoidCatalogActiveControl
        | Self::RootLifecycleActiveControl
    )
  }

  pub fn control_target(self) -> Option<Self> {
    match self {
      Self::QuarantineActiveControl => Some(Self::QuarantineManifest),
      Self::MarkRunActiveControl => Some(Self::MarkRunCheckpoint),
      Self::PhysicalInventoryActiveControl => Some(Self::PhysicalInventoryManifest),
      Self::AuditCatalogActiveControl => Some(Self::AuditCatalogManifest),
      Self::VoidCatalogActiveControl => Some(Self::VoidCatalogManifest),
      Self::RootLifecycleActiveControl => Some(Self::RootLifecycleManifest),
      _ => None,
    }
  }

  pub fn immutable_maximum_encoded_length(self) -> Option<usize> {
    match self {
      Self::QuarantineActiveControl
      | Self::MarkRunActiveControl
      | Self::PhysicalInventoryActiveControl
      | Self::AuditCatalogActiveControl
      | Self::VoidCatalogActiveControl
      | Self::RootLifecycleActiveControl => None,
      Self::QuarantineManifest
      | Self::RootExpiryCatalogManifest
      | Self::PhysicalInventoryManifest
      | Self::AuditCatalogManifest
      | Self::GcRunSummary
      | Self::VoidCatalogManifest
      | Self::RootLifecycleManifest
      | Self::CorruptGcEvidence
      | Self::AuditPin
      | Self::RootRetirementCommit
      | Self::VoidClaimSettlementReceipt
      | Self::RootObjectReclaimProof => Some(MAX_GC_MANIFEST_LENGTH),
      Self::MarkRunCheckpoint => Some(MAX_MARK_RUN_CHECKPOINT_LENGTH),
      Self::GcArtifactDirectoryNode => Some(MAX_GC_DIRECTORY_LENGTH),
      Self::CandidatePage
      | Self::RootExpiryPage
      | Self::RetirementJournalSegment
      | Self::PhysicalInventoryPage
      | Self::MarkMutationJournalSegment
      | Self::VoidExtentPage
      | Self::VoidClaim
      | Self::RootCandidatePage
      | Self::SweepProposal
      | Self::SweepCommitReceipt
      | Self::RecoveredSweepReceipt
      | Self::AuditDetailPage
      | Self::AuditSummaryPage => Some(MAX_GC_PAGE_LENGTH),
      Self::CandidateDelta => Some(MAX_GC_ARTIFACT_LENGTH),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImmutableGcArtifactWriteV1<'a> {
  pub kind: GcArtifactKindV1,
  pub hash_algorithm: HashAlgorithm,
  pub generation: u64,
  pub identity: &'a [u8],
  pub body: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedImmutableGcArtifactV1 {
  pub key: Vec<u8>,
  pub value: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcActiveControlWriteV1<'a> {
  pub kind: GcArtifactKindV1,
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8; 16],
  pub slot: u8,
  pub sequence: u64,
  pub generation: u64,
  pub target_manifest_hash: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedGcActiveControlV1 {
  pub key: Vec<u8>,
  pub value: Vec<u8>,
}

/// Return the exact immutable AGCA envelope length after validating every
/// representational and per-kind bound, without allocating the artifact.
pub fn checked_immutable_gc_artifact_encoded_length(
  kind: GcArtifactKindV1,
  identity_length: usize,
  body_length: usize,
) -> FormatResult<usize> {
  let maximum_length = kind.immutable_maximum_encoded_length().ok_or_else(|| {
    error(MalformedInputClass::UnknownTypeKindOrEnum, "gc_immutable_artifact_kind", format!("{} is a mutable control kind", kind.name()))
  })?;
  if identity_length == 0 {
    return Err(error(MalformedInputClass::IdentityKeyOrGenerationMismatch, "gc_artifact_metadata", "AGCA identity is empty"));
  }
  let _identity_length = checked_gc_u16(identity_length, "AGCA identity length exceeds u16")?;
  let _body_length = checked_gc_u32(body_length, "AGCA body length exceeds u32")?;
  let total_length = AGCA_HEADER_LENGTH
    .checked_add(identity_length)
    .and_then(|length| length.checked_add(body_length))
    .and_then(|length| length.checked_add(4))
    .ok_or_else(|| length_error("AGCA length formula overflow"))?;
  let _total_length = checked_gc_u32(total_length, "AGCA total length exceeds u32")?;
  if total_length > maximum_length {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "gc_artifact_length",
      format!("{total_length} bytes exceeds {maximum_length}-byte cap for {}", kind.name()),
    ));
  }
  Ok(total_length)
}

/// Encode one complete immutable AGCA envelope without publishing it.
pub fn encode_immutable_gc_artifact(request: &ImmutableGcArtifactWriteV1<'_>) -> FormatResult<EncodedImmutableGcArtifactV1> {
  let total_length = checked_immutable_gc_artifact_encoded_length(request.kind, request.identity.len(), request.body.len())?;
  let value = encode_gc_artifact_envelope(request.kind, request.generation, request.identity, request.body, total_length)?;

  let decoded = decode_gc_artifact_envelope(&value)?;
  if decoded.kind != request.kind {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "gc_artifact_kind",
      "encoded immutable artifact kind disagrees with its request",
    ));
  }
  let key = immutable_gc_artifact_key(request.hash_algorithm, request.kind, &value);
  Ok(EncodedImmutableGcArtifactV1 { key, value })
}

/// Encode one complete mutable A/B GC control without publishing it.
pub fn encode_gc_active_control(request: &GcActiveControlWriteV1<'_>) -> FormatResult<EncodedGcActiveControlV1> {
  if !request.kind.is_control() {
    return Err(error(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "gc_control_kind",
      format!("{} is not a mutable GC control kind", request.kind.name()),
    ));
  }
  if request.database_id.iter().all(|byte| *byte == 0)
    || request.slot > 1
    || request.sequence == 0
    || request.generation == 0
    || request.target_manifest_hash.len() != request.hash_algorithm.hash_length()
    || request.target_manifest_hash.iter().all(|byte| *byte == 0)
  {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "gc_control_identity_or_body",
      "GC active control has a zero, invalid, or width-mismatched identity, slot, sequence, generation, or target",
    ));
  }

  let mut identity = [0u8; 17];
  identity[..16].copy_from_slice(request.database_id);
  identity[16] = request.slot;
  let mut body = Vec::with_capacity(8 + request.target_manifest_hash.len());
  body.extend_from_slice(&request.sequence.to_le_bytes());
  body.extend_from_slice(request.target_manifest_hash);
  let total_length = AGCA_HEADER_LENGTH + identity.len() + body.len() + 4;
  let value = encode_gc_artifact_envelope(request.kind, request.generation, &identity, &body, total_length)?;
  let decoded = decode_gc_active_control(&value, request.hash_algorithm)?;
  Ok(EncodedGcActiveControlV1 { key: decoded.key, value })
}

fn encode_gc_artifact_envelope(
  kind: GcArtifactKindV1,
  generation: u64,
  identity: &[u8],
  body: &[u8],
  total_length: usize,
) -> FormatResult<Vec<u8>> {
  if generation == 0 || identity.is_empty() {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "gc_artifact_metadata",
      "AGCA generation and identity must be nonzero",
    ));
  }
  let identity_length = checked_gc_u16(identity.len(), "AGCA identity length exceeds u16")?;
  let body_length = checked_gc_u32(body.len(), "AGCA body length exceeds u32")?;
  let expected_length = AGCA_HEADER_LENGTH
    .checked_add(identity.len())
    .and_then(|length| length.checked_add(body.len()))
    .and_then(|length| length.checked_add(4))
    .ok_or_else(|| length_error("AGCA length formula overflow"))?;
  if total_length != expected_length || total_length > MAX_GC_ARTIFACT_LENGTH {
    return Err(length_error("AGCA encoded length is invalid"));
  }
  let total_length_u32 = checked_gc_u32(total_length, "AGCA total length exceeds u32")?;
  let mut value = vec![0u8; total_length];
  value[..4].copy_from_slice(b"AGCA");
  value[4..6].copy_from_slice(&1u16.to_le_bytes());
  value[6..8].copy_from_slice(&(kind as u16).to_le_bytes());
  value[8..10].copy_from_slice(&(AGCA_HEADER_LENGTH as u16).to_le_bytes());
  value[12..16].copy_from_slice(&total_length_u32.to_le_bytes());
  value[16..18].copy_from_slice(&identity_length.to_le_bytes());
  value[20..24].copy_from_slice(&body_length.to_le_bytes());
  value[24..32].copy_from_slice(&generation.to_le_bytes());
  let identity_end = AGCA_HEADER_LENGTH + identity.len();
  let body_end = identity_end + body.len();
  value[AGCA_HEADER_LENGTH..identity_end].copy_from_slice(identity);
  value[identity_end..body_end].copy_from_slice(body);
  let checksum = crc32fast::hash(&value[..body_end]);
  value[body_end..].copy_from_slice(&checksum.to_le_bytes());
  Ok(value)
}

#[derive(Debug, Clone, Copy)]
pub struct GcArtifactEnvelopeV1<'a> {
  pub kind: GcArtifactKindV1,
  pub generation: u64,
  pub identity: &'a [u8],
  pub body: &'a [u8],
}

#[derive(Debug, Clone)]
pub struct GcActiveControlV1<'a> {
  pub kind: GcArtifactKindV1,
  pub database_id: &'a [u8],
  pub slot: u8,
  pub sequence: u64,
  pub generation: u64,
  pub target_manifest_hash: &'a [u8],
  pub key: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalIncarnationV1<'a> {
  pub logical_key: &'a [u8],
  pub integrity_or_legacy_digest: &'a [u8],
  pub wal_offset: u64,
  pub write_sequence: u64,
  pub entity_length: u32,
  pub entry_type: u8,
  pub entity_version: u8,
}

pub fn decode_gc_artifact_envelope(value: &[u8]) -> FormatResult<GcArtifactEnvelopeV1<'_>> {
  if value.len() > MAX_GC_ARTIFACT_LENGTH {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "gc_artifact_length",
      format!("{} bytes exceeds {MAX_GC_ARTIFACT_LENGTH}-byte cap", value.len()),
    ));
  }
  if value.len() < AGCA_HEADER_LENGTH + 1 + 4 {
    return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "gc_artifact_length", "AGCA value is shorter than minimum framing"));
  }
  if &value[..4] != b"AGCA" || u16_at(value, 4)? != 1 || usize::from(u16_at(value, 8)?) != AGCA_HEADER_LENGTH {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "gc_artifact_envelope", "expected AGCA schema version 1"));
  }
  let kind = GcArtifactKindV1::from_u16(u16_at(value, 6)?)
    .ok_or_else(|| error(MalformedInputClass::UnknownTypeKindOrEnum, "gc_artifact_kind", "unknown permanent GC artifact kind"))?;
  if u16_at(value, 10)? != 0 || u16_at(value, 18)? != 0 {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "gc_artifact_metadata", "AGCA reserve fields must be zero"));
  }
  if usize::try_from(u32_at(value, 12)?).map_err(|_| length_error("AGCA total length conversion"))? != value.len() {
    return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "gc_artifact_metadata", "AGCA total length mismatch"));
  }
  let identity_length = usize::from(u16_at(value, 16)?);
  if identity_length == 0 {
    return Err(error(MalformedInputClass::IdentityKeyOrGenerationMismatch, "gc_artifact_metadata", "AGCA identity is empty"));
  }
  let body_length = usize::try_from(u32_at(value, 20)?).map_err(|_| length_error("AGCA body length conversion"))?;
  let generation = u64_at(value, 24)?;
  if generation == 0 {
    return Err(error(MalformedInputClass::IdentityKeyOrGenerationMismatch, "gc_artifact_metadata", "AGCA generation is zero"));
  }
  let identity_end = AGCA_HEADER_LENGTH.checked_add(identity_length).ok_or_else(|| length_error("AGCA identity end overflow"))?;
  let body_end = identity_end.checked_add(body_length).ok_or_else(|| length_error("AGCA body end overflow"))?;
  if body_end.checked_add(4) != Some(value.len()) {
    return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "gc_artifact_metadata", "AGCA identity/body lengths do not close"));
  }
  if u32_at(value, body_end)? != crc32fast::hash(&value[..body_end]) {
    return Err(error(MalformedInputClass::ChecksumOrIntegrityMismatch, "gc_artifact_crc", "AGCA CRC does not match"));
  }
  Ok(GcArtifactEnvelopeV1 { kind, generation, identity: &value[AGCA_HEADER_LENGTH..identity_end], body: &value[identity_end..body_end] })
}

pub fn decode_gc_active_control(value: &[u8], algorithm: HashAlgorithm) -> FormatResult<GcActiveControlV1<'_>> {
  let envelope = decode_gc_artifact_envelope(value)?;
  let hash_width = algorithm.hash_length();
  if !envelope.kind.is_control() || envelope.identity.len() != 17 || envelope.body.len() != 8 + hash_width {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "gc_control_shape",
      "GC active control identity/body shape does not match its kind",
    ));
  }
  let database_id = &envelope.identity[..16];
  let slot = envelope.identity[16];
  let sequence = u64_at(envelope.body, 0)?;
  let target_manifest_hash = &envelope.body[8..];
  if database_id.iter().all(|byte| *byte == 0) || slot > 1 || sequence == 0 || target_manifest_hash.iter().all(|byte| *byte == 0) {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "gc_control_identity_or_body",
      "GC active control has a zero or invalid identity, slot, sequence, or target",
    ));
  }
  let kind_bytes = (envelope.kind as u16).to_le_bytes();
  let key = digest_parts(algorithm, &[b"aeordb.gc-artifact.control.v1\0", &kind_bytes, envelope.identity]);
  Ok(GcActiveControlV1 { kind: envelope.kind, database_id, slot, sequence, generation: envelope.generation, target_manifest_hash, key })
}

pub fn select_gc_active_control<'control, 'data>(
  slot_a: &'control GcActiveControlV1<'data>,
  slot_a_closure_valid: bool,
  slot_b: &'control GcActiveControlV1<'data>,
  slot_b_closure_valid: bool,
) -> FormatResult<Option<&'control GcActiveControlV1<'data>>> {
  if slot_a.kind != slot_b.kind || slot_a.database_id != slot_b.database_id || slot_a.slot != 0 || slot_b.slot != 1 {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "gc_control_pair_mismatch",
      "GC control pair must be matching A/B slots for one database and kind",
    ));
  }
  if slot_a.sequence == slot_b.sequence
    && (slot_a.target_manifest_hash != slot_b.target_manifest_hash || slot_a.generation != slot_b.generation)
  {
    return Err(error(
      MalformedInputClass::AmbiguousEqualSequenceSelector,
      "gc_control_pair_ambiguous",
      "equal control sequences disagree on generation or target",
    ));
  }
  match (slot_a_closure_valid, slot_b_closure_valid) {
    (false, false) => Ok(None),
    (true, false) => Ok(Some(slot_a)),
    (false, true) => Ok(Some(slot_b)),
    (true, true) if slot_a.sequence >= slot_b.sequence => Ok(Some(slot_a)),
    (true, true) => Ok(Some(slot_b)),
  }
}

pub fn decode_physical_incarnation(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<PhysicalIncarnationV1<'_>> {
  let hash_width = algorithm.hash_length();
  let expected_length = 24usize.checked_add(2 * hash_width).ok_or_else(|| length_error("physical incarnation length overflow"))?;
  if bytes.len() != expected_length {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "physical_incarnation_length",
      format!("physical incarnation is {} bytes, expected {expected_length}", bytes.len()),
    ));
  }
  let logical_key = &bytes[..hash_width];
  let integrity_or_legacy_digest = &bytes[hash_width..2 * hash_width];
  let wal_offset = u64_at(bytes, 2 * hash_width)?;
  let write_sequence = u64_at(bytes, 2 * hash_width + 8)?;
  let entity_length = u32_at(bytes, 2 * hash_width + 16)?;
  let entry_type = bytes[2 * hash_width + 20];
  let entity_version = bytes[2 * hash_width + 21];
  if bytes[2 * hash_width + 22..].iter().any(|byte| *byte != 0) {
    return Err(error(
      MalformedInputClass::NonzeroReservedOrPadding,
      "physical_incarnation_fields",
      "physical incarnation reserve bytes must be zero",
    ));
  }
  if logical_key.iter().all(|byte| *byte == 0)
    || integrity_or_legacy_digest.iter().all(|byte| *byte == 0)
    || wal_offset == 0
    || entity_length == 0
    || !(1..=0x0a).contains(&entry_type)
    || (entity_version == 0) != (write_sequence == 0)
  {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "physical_incarnation_fields",
      "physical incarnation identity, version, sequence, type, or length is invalid",
    ));
  }
  if wal_offset.checked_add(u64::from(entity_length)).is_none() {
    return Err(length_error("physical incarnation WAL extent overflows u64"));
  }
  Ok(PhysicalIncarnationV1 {
    logical_key,
    integrity_or_legacy_digest,
    wal_offset,
    write_sequence,
    entity_length,
    entry_type,
    entity_version,
  })
}

pub fn compare_physical_incarnations_v1(left: &PhysicalIncarnationV1<'_>, right: &PhysicalIncarnationV1<'_>) -> Ordering {
  left
    .logical_key
    .cmp(right.logical_key)
    .then_with(|| left.integrity_or_legacy_digest.cmp(right.integrity_or_legacy_digest))
    .then_with(|| left.wal_offset.cmp(&right.wal_offset))
    .then_with(|| left.write_sequence.cmp(&right.write_sequence))
    .then_with(|| left.entity_length.cmp(&right.entity_length))
    .then_with(|| left.entry_type.cmp(&right.entry_type))
    .then_with(|| left.entity_version.cmp(&right.entity_version))
}

pub fn immutable_gc_artifact_key(algorithm: HashAlgorithm, kind: GcArtifactKindV1, complete_value: &[u8]) -> Vec<u8> {
  let kind_bytes = (kind as u16).to_le_bytes();
  digest_parts(algorithm, &[b"aeordb.gc-artifact.immutable.v1\0", &kind_bytes, complete_value])
}

pub(crate) fn u16_at(bytes: &[u8], offset: usize) -> FormatResult<u16> {
  let raw = bytes
    .get(offset..offset + 2)
    .ok_or_else(|| error(MalformedInputClass::TruncationOrTrailingBytes, "gc_artifact_truncated", format!("u16 at offset {offset}")))?;
  Ok(u16::from_le_bytes(raw.try_into().expect("checked GC u16 width")))
}

pub(crate) fn u32_at(bytes: &[u8], offset: usize) -> FormatResult<u32> {
  let raw = bytes
    .get(offset..offset + 4)
    .ok_or_else(|| error(MalformedInputClass::TruncationOrTrailingBytes, "gc_artifact_truncated", format!("u32 at offset {offset}")))?;
  Ok(u32::from_le_bytes(raw.try_into().expect("checked GC u32 width")))
}

pub(crate) fn u64_at(bytes: &[u8], offset: usize) -> FormatResult<u64> {
  let raw = bytes
    .get(offset..offset + 8)
    .ok_or_else(|| error(MalformedInputClass::TruncationOrTrailingBytes, "gc_artifact_truncated", format!("u64 at offset {offset}")))?;
  Ok(u64::from_le_bytes(raw.try_into().expect("checked GC u64 width")))
}

fn checked_gc_u16(value: usize, context: &'static str) -> FormatResult<u16> {
  if value > usize::from(u16::MAX) {
    return Err(length_error(context));
  }
  Ok(value as u16)
}

fn checked_gc_u32(value: usize, context: &'static str) -> FormatResult<u32> {
  if value > u32::MAX as usize {
    return Err(length_error(context));
  }
  Ok(value as u32)
}

fn length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "gc_artifact_overflow", context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
