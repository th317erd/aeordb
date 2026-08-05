use std::cmp::Ordering;

use super::config_value::{CanonicalValueBounds, validate_canonical_value};
use super::contract_generated::capability_bit;
use super::gc::{GcArtifactEnvelopeV1, GcArtifactKindV1, decode_gc_artifact_envelope, immutable_gc_artifact_key, u16_at, u32_at, u64_at};
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use crate::engine::HashAlgorithm;

const MAX_MANIFEST_LENGTH: usize = 1024 * 1024;
const MAX_PAGE_LENGTH: usize = 16 * 1024 * 1024;
const MAX_DIRECTORY_LENGTH: usize = 4 * 1024 * 1024;
const MAX_PINS: usize = 4_096;
const MAX_EVIDENCE_HASHES: usize = 64;
const AUDIT_CAPABILITY_BITS: &[usize] = &[capability_bit::GC_ARTIFACT_V1 as usize];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum AuditDirectoryRoleV1 {
  Detail = 6,
  Summary = 7,
}

impl AuditDirectoryRoleV1 {
  fn from_u16(value: u16) -> Option<Self> {
    match value {
      6 => Some(Self::Detail),
      7 => Some(Self::Summary),
      _ => None,
    }
  }

  fn from_page_kind(kind: GcArtifactKindV1) -> Option<Self> {
    match kind {
      GcArtifactKindV1::AuditDetailPage => Some(Self::Detail),
      GcArtifactKindV1::AuditSummaryPage => Some(Self::Summary),
      _ => None,
    }
  }

  fn name(self) -> &'static str {
    match self {
      Self::Detail => "audit-detail",
      Self::Summary => "audit-summary",
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum AuditEventKindV1 {
  MarkStarted = 1,
  MarkCompleted = 2,
  MarkCanceled = 3,
  RootPending = 4,
  RootRetired = 5,
  SweepProposed = 6,
  SweepCommitted = 7,
  SweepRecovered = 8,
  VoidClaimed = 9,
  VoidClaimSettled = 10,
  VoidClaimRecovered = 11,
  CorruptEvidence = 12,
  RetentionCompacted = 13,
  GcDisabled = 14,
}

impl AuditEventKindV1 {
  fn from_u16(value: u16) -> Option<Self> {
    match value {
      1 => Some(Self::MarkStarted),
      2 => Some(Self::MarkCompleted),
      3 => Some(Self::MarkCanceled),
      4 => Some(Self::RootPending),
      5 => Some(Self::RootRetired),
      6 => Some(Self::SweepProposed),
      7 => Some(Self::SweepCommitted),
      8 => Some(Self::SweepRecovered),
      9 => Some(Self::VoidClaimed),
      10 => Some(Self::VoidClaimSettled),
      11 => Some(Self::VoidClaimRecovered),
      12 => Some(Self::CorruptEvidence),
      13 => Some(Self::RetentionCompacted),
      14 => Some(Self::GcDisabled),
      _ => None,
    }
  }

  fn requires_batch(self) -> bool {
    matches!(
      self,
      Self::SweepProposed
        | Self::SweepCommitted
        | Self::SweepRecovered
        | Self::VoidClaimed
        | Self::VoidClaimSettled
        | Self::VoidClaimRecovered
    )
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum GcRunKindV1 {
  Mark = 1,
  Sweep = 2,
  PhysicalInventory = 3,
  AuditCompaction = 4,
  VoidReconcile = 5,
  RootLifecycleReconcile = 6,
}

impl GcRunKindV1 {
  fn from_u16(value: u16) -> Option<Self> {
    match value {
      1 => Some(Self::Mark),
      2 => Some(Self::Sweep),
      3 => Some(Self::PhysicalInventory),
      4 => Some(Self::AuditCompaction),
      5 => Some(Self::VoidReconcile),
      6 => Some(Self::RootLifecycleReconcile),
      _ => None,
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum GcOutcomeV1 {
  Complete = 1,
  Canceled = 2,
  Failed = 3,
  Skipped = 4,
  Recovered = 5,
}

impl GcOutcomeV1 {
  fn from_u16(value: u16) -> Option<Self> {
    match value {
      1 => Some(Self::Complete),
      2 => Some(Self::Canceled),
      3 => Some(Self::Failed),
      4 => Some(Self::Skipped),
      5 => Some(Self::Recovered),
      _ => None,
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum GcErrorClassV1 {
  Framing = 1,
  Checksum = 2,
  BoundsOrOverlap = 3,
  MissingEdge = 4,
  WrongIdentity = 5,
  AmbiguousControl = 6,
  IncompleteAuthorityWalk = 7,
  WorkspaceTamperOrLoss = 8,
  PolicyUnavailable = 9,
  UnsupportedCodec = 10,
}

impl GcErrorClassV1 {
  fn from_u16(value: u16) -> Option<Self> {
    match value {
      1 => Some(Self::Framing),
      2 => Some(Self::Checksum),
      3 => Some(Self::BoundsOrOverlap),
      4 => Some(Self::MissingEdge),
      5 => Some(Self::WrongIdentity),
      6 => Some(Self::AmbiguousControl),
      7 => Some(Self::IncompleteAuthorityWalk),
      8 => Some(Self::WorkspaceTamperOrLoss),
      9 => Some(Self::PolicyUnavailable),
      10 => Some(Self::UnsupportedCodec),
      _ => None,
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum AuditPinReasonV1 {
  OperatorInvestigation = 1,
  RepairEvidence = 2,
  MigrationVerification = 3,
  SupportBundle = 4,
}

impl AuditPinReasonV1 {
  fn from_u16(value: u16) -> Option<Self> {
    match value {
      1 => Some(Self::OperatorInvestigation),
      2 => Some(Self::RepairEvidence),
      3 => Some(Self::MigrationVerification),
      4 => Some(Self::SupportBundle),
      _ => None,
    }
  }
}

#[derive(Clone, Copy, Debug)]
pub struct AuditDetailRecordV1<'a> {
  pub raw: &'a [u8],
  pub event_id: &'a [u8],
  pub event_kind: AuditEventKindV1,
  pub outcome: GcOutcomeV1,
  pub occurred_at_ms: i64,
  pub run_id: &'a [u8],
  pub batch_id: &'a [u8],
  pub payload: &'a [u8],
}

#[derive(Clone, Copy, Debug)]
pub struct AuditSummaryRecordV1<'a> {
  pub raw: &'a [u8],
  pub run_id: &'a [u8],
  pub started_at_ms: i64,
  pub completed_at_ms: i64,
  pub run_kind: GcRunKindV1,
  pub outcome: GcOutcomeV1,
  pub mark_generation: u64,
  pub scanned_count: u64,
  pub candidate_count: u64,
  pub reclaimed_count: u64,
  pub reclaimed_bytes: u64,
  pub evidence_digest: &'a [u8],
}

#[derive(Clone, Copy, Debug)]
pub enum AuditPageRecordV1<'a> {
  Detail(AuditDetailRecordV1<'a>),
  Summary(AuditSummaryRecordV1<'a>),
}

impl AuditPageRecordV1<'_> {
  fn raw(&self) -> &[u8] {
    match self {
      Self::Detail(record) => record.raw,
      Self::Summary(record) => record.raw,
    }
  }

  fn timestamp(&self) -> i64 {
    match self {
      Self::Detail(record) => record.occurred_at_ms,
      Self::Summary(record) => record.completed_at_ms,
    }
  }
}

#[derive(Debug, Clone)]
pub struct AuditPageV1<'a> {
  pub role: AuditDirectoryRoleV1,
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8],
  pub catalog_id: &'a [u8],
  pub generation: u64,
  pub page_id: u64,
  pub record_count: u32,
  pub logical_bytes: u64,
  pub oldest_at_ms: i64,
  pub newest_at_ms: i64,
  pub lower_fence: &'a [u8],
  pub upper_fence: &'a [u8],
  pub records: &'a [u8],
  pub key: Vec<u8>,
}

impl<'a> AuditPageV1<'a> {
  pub fn iter(&self) -> AuditPageRecordIterV1<'a> {
    AuditPageRecordIterV1 { role: self.role, algorithm: self.hash_algorithm, remaining: self.record_count, bytes: self.records, offset: 0 }
  }
}

pub struct AuditPageRecordIterV1<'a> {
  role: AuditDirectoryRoleV1,
  algorithm: HashAlgorithm,
  remaining: u32,
  bytes: &'a [u8],
  offset: usize,
}

impl<'a> Iterator for AuditPageRecordIterV1<'a> {
  type Item = FormatResult<AuditPageRecordV1<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.remaining == 0 {
      return None;
    }
    let remaining = match self.bytes.get(self.offset..) {
      Some(remaining) => remaining,
      None => {
        self.remaining = 0;
        return Some(Err(trailing_error("audit_page_iterator_offset", "audit page iterator offset exceeds record body")));
      }
    };
    let length = record_length(self.role, self.algorithm, remaining);
    let result = length.and_then(|length| {
      let end = self.offset.checked_add(length).ok_or_else(|| overflow_error("audit page iterator record end"))?;
      let raw = self
        .bytes
        .get(self.offset..end)
        .ok_or_else(|| trailing_error("audit_page_record_length", "audit page iterator record exceeds body"))?;
      let record = decode_page_record(self.role, self.algorithm, raw)?;
      self.offset = end;
      self.remaining -= 1;
      Ok(record)
    });
    if result.is_err() {
      self.remaining = 0;
    }
    Some(result)
  }
}

#[derive(Debug, Clone)]
pub struct AuditDirectoryV1<'a> {
  pub role: AuditDirectoryRoleV1,
  pub database_id: &'a [u8],
  pub catalog_id: &'a [u8],
  pub generation: u64,
  pub child_generation: u64,
  pub page_id: u64,
  pub live_count: u64,
  pub logical_bytes: u64,
  pub lower_fence: &'a [u8],
  pub upper_fence: &'a [u8],
  pub child_hash: &'a [u8],
  pub key: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct AuditCatalogManifestV1<'a> {
  pub database_id: &'a [u8],
  pub generation: u64,
  pub published_at_ms: i64,
  pub detail_root: &'a [u8],
  pub summary_root: &'a [u8],
  pub detail_next_page_id: u64,
  pub summary_next_page_id: u64,
  pub detail_count: u64,
  pub detail_bytes: u64,
  pub summary_count: u64,
  pub summary_bytes: u64,
  pub oldest_detail_at_ms: i64,
  pub newest_detail_at_ms: i64,
  pub oldest_summary_at_ms: i64,
  pub newest_summary_at_ms: i64,
  pub detail_retention_cutoff_ms: i64,
  pub summary_retention_cutoff_ms: i64,
  pub pin_count: u32,
  pub pins: &'a [u8],
  pub hash_width: usize,
  pub key: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct GcRunSummaryV1<'a> {
  pub database_id: &'a [u8],
  pub generation: u64,
  pub record: AuditSummaryRecordV1<'a>,
  pub key: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct CorruptGcEvidenceV1<'a> {
  pub database_id: &'a [u8],
  pub evidence_id: &'a [u8],
  pub generation: u64,
  pub detected_at_ms: i64,
  pub error_class: GcErrorClassV1,
  pub observed_entry_type: Option<u8>,
  pub observed_artifact_kind: Option<GcArtifactKindV1>,
  pub physical_range: Option<(u64, u32)>,
  pub write_sequence: Option<u64>,
  pub expected_hash: Option<&'a [u8]>,
  pub observed_hash: Option<&'a [u8]>,
  pub run_id: Option<&'a [u8]>,
  pub control_kind: Option<GcArtifactKindV1>,
  pub control_identity_digest: Option<&'a [u8]>,
  pub context: &'a [u8],
  pub evidence_count: u16,
  pub evidence_hashes: &'a [u8],
  pub key: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct AuditPinV1<'a> {
  pub database_id: &'a [u8],
  pub pin_id: &'a [u8],
  pub generation: u64,
  pub created_at_ms: i64,
  pub expires_at_ms: i64,
  pub creator_identity_digest: &'a [u8],
  pub reason: AuditPinReasonV1,
  pub artifact_count: u32,
  pub artifact_hashes: &'a [u8],
  pub hash_width: usize,
  pub key: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum AuditArtifactV1<'a> {
  Manifest(AuditCatalogManifestV1<'a>),
  Page(AuditPageV1<'a>),
  Directory(AuditDirectoryV1<'a>),
  RunSummary(GcRunSummaryV1<'a>),
  CorruptEvidence(CorruptGcEvidenceV1<'a>),
  Pin(AuditPinV1<'a>),
}

impl AuditArtifactV1<'_> {
  pub fn key(&self) -> &[u8] {
    match self {
      Self::Manifest(value) => &value.key,
      Self::Page(value) => &value.key,
      Self::Directory(value) => &value.key,
      Self::RunSummary(value) => &value.key,
      Self::CorruptEvidence(value) => &value.key,
      Self::Pin(value) => &value.key,
    }
  }

  pub fn summary(&self) -> String {
    match self {
      Self::Manifest(value) => format!(
        "gc:manifest:audit-catalog:{}:details={}:summaries={}:pins={}:generation={}",
        if value.detail_count == 0 && value.summary_count == 0 && value.pin_count == 0 { "empty" } else { "populated" },
        value.detail_count,
        value.summary_count,
        value.pin_count,
        value.generation
      ),
      Self::Page(value) => format!("gc:page:{}:records={}", value.role.name(), value.record_count),
      Self::Directory(value) => format!("gc:directory:{}:records={}", value.role.name(), value.live_count),
      Self::RunSummary(value) => format!(
        "gc:summary:run:kind={}:outcome={}:reclaimed={}",
        value.record.run_kind as u16, value.record.outcome as u16, value.record.reclaimed_count
      ),
      Self::CorruptEvidence(value) => {
        format!("gc:evidence:corrupt:class={}:items={}:context={}", value.error_class as u16, value.evidence_count, value.context.len())
      }
      Self::Pin(value) => format!("gc:pin:audit:reason={}:artifacts={}", value.reason as u16, value.artifact_count),
    }
  }
}

pub fn decode_audit_artifact(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<AuditArtifactV1<'_>> {
  let hinted_kind = bytes.get(6..8).map(|raw| u16::from_le_bytes([raw[0], raw[1]])).and_then(GcArtifactKindV1::from_u16);
  let cap = match hinted_kind {
    Some(GcArtifactKindV1::AuditDetailPage | GcArtifactKindV1::AuditSummaryPage) => MAX_PAGE_LENGTH,
    Some(GcArtifactKindV1::GcArtifactDirectoryNode) => MAX_DIRECTORY_LENGTH,
    Some(
      GcArtifactKindV1::AuditCatalogManifest
      | GcArtifactKindV1::GcRunSummary
      | GcArtifactKindV1::CorruptGcEvidence
      | GcArtifactKindV1::AuditPin,
    ) => MAX_MANIFEST_LENGTH,
    _ => super::gc::MAX_GC_ARTIFACT_LENGTH,
  };
  ensure_cap("audit_artifact_length", bytes.len(), cap)?;
  let envelope = decode_gc_artifact_envelope(bytes)?;
  let key = immutable_gc_artifact_key(algorithm, envelope.kind, bytes);
  match envelope.kind {
    GcArtifactKindV1::AuditCatalogManifest => decode_manifest(envelope, algorithm, key).map(AuditArtifactV1::Manifest),
    GcArtifactKindV1::AuditDetailPage | GcArtifactKindV1::AuditSummaryPage => {
      decode_page(envelope, algorithm, key).map(AuditArtifactV1::Page)
    }
    GcArtifactKindV1::GcArtifactDirectoryNode => decode_directory(envelope, algorithm, key).map(AuditArtifactV1::Directory),
    GcArtifactKindV1::GcRunSummary => decode_run_summary(envelope, algorithm, key).map(AuditArtifactV1::RunSummary),
    GcArtifactKindV1::CorruptGcEvidence => decode_corrupt_evidence(envelope, algorithm, key).map(AuditArtifactV1::CorruptEvidence),
    GcArtifactKindV1::AuditPin => decode_pin(envelope, algorithm, key).map(AuditArtifactV1::Pin),
    _ => Err(kind_error("audit_artifact_kind", format!("{} is not an audit artifact", envelope.kind.name()))),
  }
}

fn decode_detail(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<AuditDetailRecordV1<'_>> {
  let hash_width = algorithm.hash_length();
  let fixed = 52 + hash_width;
  if bytes.len() < fixed {
    return Err(trailing_error("audit_detail_truncated", "audit detail record is shorter than its fixed body"));
  }
  let payload_length = usize::try_from(u32_at(bytes, hash_width + 44)?).map_err(|_| overflow_error("audit detail payload length"))?;
  if checked_add(fixed, payload_length, "audit detail record")? != bytes.len() {
    return Err(trailing_error("audit_detail_length", "audit detail payload does not close record"));
  }
  let event_kind = AuditEventKindV1::from_u16(u16_at(bytes, hash_width)?)
    .ok_or_else(|| kind_error("audit_detail_event_kind", "unknown audit event kind"))?;
  let outcome =
    GcOutcomeV1::from_u16(u16_at(bytes, hash_width + 2)?).ok_or_else(|| kind_error("audit_detail_outcome", "unknown GC outcome"))?;
  let occurred_at_ms = i64_at(bytes, hash_width + 4)?;
  let run_id = &bytes[hash_width + 12..hash_width + 28];
  let batch_id = &bytes[hash_width + 28..hash_width + 44];
  let payload = &bytes[fixed..];
  if u32_at(bytes, hash_width + 48)? != 0 {
    return Err(reserved_error("audit_detail_reserved", "audit detail reserve must be zero"));
  }
  if all_zero(&bytes[..hash_width])
    || occurred_at_ms <= 0
    || all_zero(run_id)
    || payload.is_empty()
    || event_kind.requires_batch() != !all_zero(batch_id)
  {
    return Err(closure_error("audit_detail_fields", "audit detail identity, time, batch, payload, or reserve is invalid"));
  }
  validate_canonical_value(payload, CanonicalValueBounds::AUDIT_VALUE)?;
  Ok(AuditDetailRecordV1 { raw: bytes, event_id: &bytes[..hash_width], event_kind, outcome, occurred_at_ms, run_id, batch_id, payload })
}

fn decode_summary(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<AuditSummaryRecordV1<'_>> {
  let hash_width = algorithm.hash_length();
  if bytes.len() != 76 + hash_width {
    return Err(trailing_error("audit_summary_length", "audit summary record has wrong fixed length"));
  }
  let record = AuditSummaryRecordV1 {
    raw: bytes,
    run_id: &bytes[..16],
    started_at_ms: i64_at(bytes, 16)?,
    completed_at_ms: i64_at(bytes, 24)?,
    run_kind: GcRunKindV1::from_u16(u16_at(bytes, 32)?).ok_or_else(|| kind_error("audit_summary_run_kind", "unknown GC run kind"))?,
    outcome: GcOutcomeV1::from_u16(u16_at(bytes, 34)?).ok_or_else(|| kind_error("audit_summary_outcome", "unknown GC outcome"))?,
    mark_generation: u64_at(bytes, 36)?,
    scanned_count: u64_at(bytes, 44)?,
    candidate_count: u64_at(bytes, 52)?,
    reclaimed_count: u64_at(bytes, 60)?,
    reclaimed_bytes: u64_at(bytes, 68)?,
    evidence_digest: &bytes[76..],
  };
  if all_zero(record.run_id)
    || record.started_at_ms <= 0
    || record.completed_at_ms < record.started_at_ms
    || record.candidate_count > record.scanned_count
    || record.reclaimed_count > record.candidate_count
    || (record.reclaimed_count == 0) != (record.reclaimed_bytes == 0)
    || all_zero(record.evidence_digest)
  {
    return Err(closure_error("audit_summary_fields", "audit summary identity, times, counters, or evidence are invalid"));
  }
  Ok(record)
}

fn record_length(role: AuditDirectoryRoleV1, algorithm: HashAlgorithm, bytes: &[u8]) -> FormatResult<usize> {
  let hash_width = algorithm.hash_length();
  match role {
    AuditDirectoryRoleV1::Detail => {
      let fixed = 52 + hash_width;
      if bytes.len() < fixed {
        return Err(trailing_error("audit_detail_truncated", "audit detail record is truncated"));
      }
      checked_add(
        fixed,
        usize::try_from(u32_at(bytes, hash_width + 44)?).map_err(|_| overflow_error("audit detail payload length"))?,
        "audit detail record",
      )
    }
    AuditDirectoryRoleV1::Summary => Ok(76 + hash_width),
  }
}

fn decode_page_record(role: AuditDirectoryRoleV1, algorithm: HashAlgorithm, bytes: &[u8]) -> FormatResult<AuditPageRecordV1<'_>> {
  match role {
    AuditDirectoryRoleV1::Detail => decode_detail(bytes, algorithm).map(AuditPageRecordV1::Detail),
    AuditDirectoryRoleV1::Summary => decode_summary(bytes, algorithm).map(AuditPageRecordV1::Summary),
  }
}

fn compare_records(left: &AuditPageRecordV1<'_>, right: &AuditPageRecordV1<'_>) -> FormatResult<Ordering> {
  match (left, right) {
    (AuditPageRecordV1::Detail(left), AuditPageRecordV1::Detail(right)) => {
      Ok(left.occurred_at_ms.cmp(&right.occurred_at_ms).then_with(|| left.event_id.cmp(right.event_id)))
    }
    (AuditPageRecordV1::Summary(left), AuditPageRecordV1::Summary(right)) => {
      Ok(left.completed_at_ms.cmp(&right.completed_at_ms).then_with(|| left.run_id.cmp(right.run_id)))
    }
    _ => Err(kind_error("audit_page_record_role", "audit page record variants do not share one role")),
  }
}

fn record_matches_key(record: &AuditPageRecordV1<'_>, key: &[u8]) -> FormatResult<bool> {
  if key.len() < 8 {
    return Err(trailing_error("audit_page_fence", "audit page fence is shorter than timestamp"));
  }
  Ok(match record {
    AuditPageRecordV1::Detail(record) => i64_at(key, 0)? == record.occurred_at_ms && &key[8..] == record.event_id,
    AuditPageRecordV1::Summary(record) => i64_at(key, 0)? == record.completed_at_ms && &key[8..] == record.run_id,
  })
}

fn decode_page(artifact: GcArtifactEnvelopeV1<'_>, algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<AuditPageV1<'_>> {
  let role = AuditDirectoryRoleV1::from_page_kind(artifact.kind)
    .ok_or_else(|| kind_error("audit_page_kind", "artifact kind is not an audit page"))?;
  let hash_width = algorithm.hash_length();
  if artifact.identity.len() != 42 || artifact.body.len() < 64 {
    return Err(closure_error("audit_page_shape", "audit page identity or body is malformed"));
  }
  let database_id = &artifact.identity[..16];
  let catalog_id = &artifact.identity[16..32];
  let page_id = u64_at(artifact.identity, 34)?;
  if all_zero(database_id) || all_zero(catalog_id) || u16_at(artifact.identity, 32)? != role as u16 || page_id == 0 {
    return Err(identity_error("audit_page_identity", "audit page identity is invalid"));
  }
  let body = artifact.body;
  let lower_length = usize::try_from(u32_at(body, 8)?).map_err(|_| overflow_error("audit lower fence length"))?;
  let upper_length = usize::try_from(u32_at(body, 12)?).map_err(|_| overflow_error("audit upper fence length"))?;
  let record_count = u32_at(body, 16)?;
  let records_length = usize::try_from(u64_at(body, 24)?).map_err(|_| overflow_error("audit page records length"))?;
  let key_length = match role {
    AuditDirectoryRoleV1::Detail => 8 + hash_width,
    AuditDirectoryRoleV1::Summary => 24,
  };
  let minimum_record_length = match role {
    AuditDirectoryRoleV1::Detail => 52 + hash_width + 5,
    AuditDirectoryRoleV1::Summary => 76 + hash_width,
  };
  if record_count as usize > records_length / minimum_record_length {
    return Err(amplification_error("audit_page_record_count", record_count as usize, records_length / minimum_record_length));
  }
  if u32_at(body, 0)? != 0 || body[40..64].iter().any(|byte| *byte != 0) {
    return Err(reserved_error("audit_page_reserved", "audit page flags and reserve must be zero"));
  }
  if u16_at(body, 4)? != 1
    || u16_at(body, 6)? != role as u16
    || lower_length != key_length
    || upper_length != key_length
    || record_count == 0
    || u32_at(body, 20)? != record_count
    || u64_at(body, 32)? != records_length as u64
  {
    return Err(closure_error("audit_page_header", "audit page header, role, counts, or reserves are invalid"));
  }
  let fence_length = checked_add(lower_length, upper_length, "audit page fence lengths")?;
  let fences_end = checked_add(64, fence_length, "audit page fences")?;
  if checked_add(fences_end, records_length, "audit page body")? != body.len() {
    return Err(trailing_error("audit_page_length", "audit page lengths do not close"));
  }
  let lower_fence = &body[64..64 + lower_length];
  let upper_fence = &body[64 + lower_length..fences_end];
  let records = &body[fences_end..];
  let mut iterator = AuditPageRecordIterV1 { role, algorithm, remaining: record_count, bytes: records, offset: 0 };
  let mut previous = None;
  let mut first = None;
  let mut last = None;
  for record in iterator.by_ref() {
    let record = record?;
    if let Some(prior) = previous.as_ref() {
      if compare_records(prior, &record)? != Ordering::Less {
        return Err(order_error("audit_page_order", "audit page records are duplicate or out of order"));
      }
    }
    first.get_or_insert(record);
    last = Some(record);
    previous = Some(record);
  }
  let first = first.ok_or_else(|| closure_error("audit_page_empty", "audit page contains no records"))?;
  let last = last.ok_or_else(|| closure_error("audit_page_empty", "audit page contains no records"))?;
  if iterator.offset != records.len() || !record_matches_key(&first, lower_fence)? || !record_matches_key(&last, upper_fence)? {
    return Err(closure_error("audit_page_fences", "audit page records do not close their fences"));
  }
  Ok(AuditPageV1 {
    role,
    hash_algorithm: algorithm,
    database_id,
    catalog_id,
    generation: artifact.generation,
    page_id,
    record_count,
    logical_bytes: records_length as u64,
    oldest_at_ms: first.timestamp(),
    newest_at_ms: last.timestamp(),
    lower_fence,
    upper_fence,
    records,
    key,
  })
}

fn compare_audit_key(role: AuditDirectoryRoleV1, algorithm: HashAlgorithm, left: &[u8], right: &[u8]) -> FormatResult<Ordering> {
  let expected = match role {
    AuditDirectoryRoleV1::Detail => 8 + algorithm.hash_length(),
    AuditDirectoryRoleV1::Summary => 24,
  };
  if left.len() != expected || right.len() != expected {
    return Err(trailing_error("audit_directory_key_length", "audit directory fence has wrong width"));
  }
  Ok(i64_at(left, 0)?.cmp(&i64_at(right, 0)?).then_with(|| left[8..].cmp(&right[8..])))
}

fn decode_directory(artifact: GcArtifactEnvelopeV1<'_>, algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<AuditDirectoryV1<'_>> {
  if artifact.identity.len() != 34 || artifact.body.len() < 80 {
    return Err(closure_error("audit_directory_shape", "audit directory identity or body is malformed"));
  }
  let role = AuditDirectoryRoleV1::from_u16(u16_at(artifact.identity, 32)?)
    .ok_or_else(|| kind_error("audit_directory_role", "artifact directory role is not an audit role"))?;
  let database_id = &artifact.identity[..16];
  let catalog_id = &artifact.identity[16..32];
  if all_zero(database_id) || all_zero(catalog_id) {
    return Err(identity_error("audit_directory_identity", "audit directory database/catalog IDs are zero"));
  }
  let body = artifact.body;
  let lower_length = usize::try_from(u32_at(body, 16)?).map_err(|_| overflow_error("audit lower fence length"))?;
  let upper_length = usize::try_from(u32_at(body, 20)?).map_err(|_| overflow_error("audit upper fence length"))?;
  let entries_length = usize::try_from(u32_at(body, 72)?).map_err(|_| overflow_error("audit directory entries length"))?;
  let hash_width = algorithm.hash_length();
  let descriptor_fixed = 72 + hash_width;
  let fence_length = checked_add(lower_length, upper_length, "audit directory fence lengths")?;
  let key_length = match role {
    AuditDirectoryRoleV1::Detail => 8 + hash_width,
    AuditDirectoryRoleV1::Summary => 24,
  };
  if u16_at(body, 0)? != 0 || u32_at(body, 8)? != 0 || u32_at(body, 12)? != 0 || u32_at(body, 76)? != 0 {
    return Err(reserved_error("audit_directory_reserved", "audit directory flags and reserves must be zero"));
  }
  if u16_at(body, 2)? != role as u16
    || u32_at(body, 4)? != 1
    || lower_length != key_length
    || upper_length != key_length
    || entries_length != checked_add(descriptor_fixed, fence_length, "audit directory descriptor")?
    || checked_add(checked_add(80, fence_length, "audit directory fences")?, entries_length, "audit directory body")? != body.len()
  {
    return Err(closure_error("audit_directory_header", "audit directory header, role, lengths, or reserves are invalid"));
  }
  let lower_fence = &body[80..80 + lower_length];
  let upper_fence = &body[80 + lower_length..80 + lower_length + upper_length];
  let cursor = 80 + lower_length + upper_length;
  let page_id = u64_at(body, cursor + 8)?;
  let child_hash = &body[cursor + 16..cursor + 16 + hash_width];
  let fields = cursor + 16 + hash_width;
  let child_generation = u64_at(body, fields)?;
  let live_count = u64_at(body, fields + 8)?;
  let logical_bytes = u64_at(body, fields + 24)?;
  let key_start = cursor + descriptor_fixed;
  if body[fields + 32..fields + 56].iter().any(|byte| *byte != 0) {
    return Err(reserved_error("audit_directory_descriptor_reserved", "audit directory descriptor reserve must be zero"));
  }
  if all_zero(child_hash)
    || child_generation == 0
    || child_generation > artifact.generation
    || live_count == 0
    || logical_bytes == 0
    || page_id == 0
    || compare_audit_key(role, algorithm, lower_fence, upper_fence)? == Ordering::Greater
    || body[key_start..key_start + lower_length] != *lower_fence
    || body[key_start + lower_length..] != *upper_fence
    || u64_at(body, 24)? != live_count
    || u64_at(body, 32)? != 0
    || u64_at(body, 40)? != 1
    || u64_at(body, 48)? != logical_bytes
    || u64_at(body, 56)? != page_id
    || u64_at(body, 64)? != page_id
  {
    return Err(closure_error("audit_directory_descriptor", "audit directory descriptor or aggregates are invalid"));
  }
  Ok(AuditDirectoryV1 {
    role,
    database_id,
    catalog_id,
    generation: artifact.generation,
    child_generation,
    page_id,
    live_count,
    logical_bytes,
    lower_fence,
    upper_fence,
    child_hash,
    key,
  })
}

fn decode_manifest(artifact: GcArtifactEnvelopeV1<'_>, algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<AuditCatalogManifestV1<'_>> {
  let hash_width = algorithm.hash_length();
  if artifact.identity.len() != 24 || artifact.body.len() < 148 + 2 * hash_width {
    return Err(closure_error("audit_manifest_shape", "audit manifest identity or body is malformed"));
  }
  let database_id = &artifact.identity[..16];
  if all_zero(database_id) || u64_at(artifact.identity, 16)? != artifact.generation {
    return Err(identity_error("audit_manifest_identity", "audit manifest database/generation identity is invalid"));
  }
  let body = artifact.body;
  let pin_count = u32_at(body, 140 + 2 * hash_width)?;
  let pins_length = usize::try_from(u32_at(body, 144 + 2 * hash_width)?).map_err(|_| overflow_error("audit manifest pins length"))?;
  if pin_count as usize > MAX_PINS {
    return Err(amplification_error("audit_manifest_pins", pin_count as usize, MAX_PINS));
  }
  if pins_length != checked_mul(pin_count as usize, hash_width, "audit manifest pins")?
    || checked_add(148 + 2 * hash_width, pins_length, "audit manifest body")? != body.len()
  {
    return Err(trailing_error("audit_manifest_length", "audit manifest lengths do not close"));
  }
  if u32_at(body, 0)? != 0 {
    return Err(reserved_error("audit_manifest_flags", "audit manifest flags must be zero"));
  }
  validate_capabilities(&body[4..36])?;
  let published_at_ms = i64_at(body, 36)?;
  if published_at_ms <= 0 {
    return Err(identity_error("audit_manifest_time", "audit manifest publication time is invalid"));
  }
  let detail_root = &body[44..44 + hash_width];
  let summary_root = &body[44 + hash_width..44 + 2 * hash_width];
  let detail_next_page_id = u64_at(body, 44 + 2 * hash_width)?;
  let summary_next_page_id = u64_at(body, 52 + 2 * hash_width)?;
  let detail_count = u64_at(body, 60 + 2 * hash_width)?;
  let detail_bytes = u64_at(body, 68 + 2 * hash_width)?;
  let summary_count = u64_at(body, 76 + 2 * hash_width)?;
  let summary_bytes = u64_at(body, 84 + 2 * hash_width)?;
  let oldest_detail_at_ms = i64_at(body, 92 + 2 * hash_width)?;
  let newest_detail_at_ms = i64_at(body, 100 + 2 * hash_width)?;
  let oldest_summary_at_ms = i64_at(body, 108 + 2 * hash_width)?;
  let newest_summary_at_ms = i64_at(body, 116 + 2 * hash_width)?;
  let detail_retention_cutoff_ms = i64_at(body, 124 + 2 * hash_width)?;
  let summary_retention_cutoff_ms = i64_at(body, 132 + 2 * hash_width)?;
  let detail_present = detail_count != 0;
  let summary_present = summary_count != 0;
  if detail_present
    != (!all_zero(detail_root) && detail_bytes != 0 && oldest_detail_at_ms > 0 && newest_detail_at_ms >= oldest_detail_at_ms)
    || !detail_present && (detail_bytes != 0 || oldest_detail_at_ms != 0 || newest_detail_at_ms != 0)
    || summary_present
      != (!all_zero(summary_root) && summary_bytes != 0 && oldest_summary_at_ms > 0 && newest_summary_at_ms >= oldest_summary_at_ms)
    || !summary_present && (summary_bytes != 0 || oldest_summary_at_ms != 0 || newest_summary_at_ms != 0)
    || detail_present && detail_next_page_id <= 1
    || !detail_present && detail_next_page_id != 1
    || summary_present && summary_next_page_id <= 1
    || !summary_present && summary_next_page_id != 1
    || detail_retention_cutoff_ms <= 0
    || summary_retention_cutoff_ms <= 0
  {
    return Err(closure_error("audit_manifest_fields", "audit manifest roots, counters, times, or cutoffs are invalid"));
  }
  let pins = &body[148 + 2 * hash_width..];
  validate_sorted_hashes(pins, hash_width, "audit_manifest_pin_order")?;
  Ok(AuditCatalogManifestV1 {
    database_id,
    generation: artifact.generation,
    published_at_ms,
    detail_root,
    summary_root,
    detail_next_page_id,
    summary_next_page_id,
    detail_count,
    detail_bytes,
    summary_count,
    summary_bytes,
    oldest_detail_at_ms,
    newest_detail_at_ms,
    oldest_summary_at_ms,
    newest_summary_at_ms,
    detail_retention_cutoff_ms,
    summary_retention_cutoff_ms,
    pin_count,
    pins,
    hash_width,
    key,
  })
}

fn decode_run_summary(artifact: GcArtifactEnvelopeV1<'_>, algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<GcRunSummaryV1<'_>> {
  if artifact.identity.len() != 32 {
    return Err(closure_error("gc_run_summary_shape", "GC run summary identity has wrong length"));
  }
  let database_id = &artifact.identity[..16];
  let record = decode_summary(artifact.body, algorithm)?;
  if all_zero(database_id) || &artifact.identity[16..] != record.run_id {
    return Err(identity_error("gc_run_summary_identity", "GC run summary identity does not match body"));
  }
  Ok(GcRunSummaryV1 { database_id, generation: artifact.generation, record, key })
}

fn decode_corrupt_evidence(
  artifact: GcArtifactEnvelopeV1<'_>,
  algorithm: HashAlgorithm,
  key: Vec<u8>,
) -> FormatResult<CorruptGcEvidenceV1<'_>> {
  let hash_width = algorithm.hash_length();
  if artifact.identity.len() != 32 || artifact.body.len() < 68 + 3 * hash_width {
    return Err(closure_error("corrupt_evidence_shape", "corrupt evidence identity or body is malformed"));
  }
  let database_id = &artifact.identity[..16];
  let evidence_id = &artifact.identity[16..];
  if all_zero(database_id) || all_zero(evidence_id) {
    return Err(identity_error("corrupt_evidence_identity", "corrupt evidence database/evidence IDs must be nonzero"));
  }
  let body = artifact.body;
  let context_length =
    usize::try_from(u32_at(body, 60 + 3 * hash_width)?).map_err(|_| overflow_error("corrupt evidence context length"))?;
  let evidence_count = u16_at(body, 64 + 3 * hash_width)?;
  if evidence_count as usize > MAX_EVIDENCE_HASHES {
    return Err(amplification_error("corrupt_evidence_count", evidence_count as usize, MAX_EVIDENCE_HASHES));
  }
  let context_start = 68 + 3 * hash_width;
  let context_end = checked_add(context_start, context_length, "corrupt evidence context")?;
  let evidence_length = checked_mul(evidence_count as usize, hash_width, "corrupt evidence hashes")?;
  if checked_add(context_end, evidence_length, "corrupt evidence body")? != body.len() {
    return Err(trailing_error("corrupt_evidence_length", "corrupt evidence lengths do not close"));
  }
  let flags = body[11];
  let observed_entry = body[10];
  let observed_kind = u16_at(body, 12)?;
  let physical_offset = u64_at(body, 16)?;
  let physical_length = u32_at(body, 24)?;
  let write_sequence = u64_at(body, 32)?;
  let expected_hash = &body[40..40 + hash_width];
  let observed_hash = &body[40 + hash_width..40 + 2 * hash_width];
  let run_id = &body[40 + 2 * hash_width..56 + 2 * hash_width];
  let control_kind = u16_at(body, 56 + 2 * hash_width)?;
  let control_digest = &body[60 + 2 * hash_width..60 + 3 * hash_width];
  let optional_valid = presence(flags, 0) == (observed_entry != 0)
    && presence(flags, 1) == (observed_kind != 0)
    && presence(flags, 2) == (physical_offset != 0)
    && presence(flags, 2) == (physical_length != 0)
    && (!presence(flags, 2) || physical_offset.checked_add(u64::from(physical_length)).is_some())
    && presence(flags, 3) == (write_sequence != 0)
    && presence(flags, 4) == !all_zero(expected_hash)
    && presence(flags, 5) == !all_zero(observed_hash)
    && presence(flags, 6) == !all_zero(run_id)
    && presence(flags, 7) == (control_kind != 0)
    && presence(flags, 7) == !all_zero(control_digest);
  let detected_at_ms = i64_at(body, 0)?;
  let observed_artifact_kind = GcArtifactKindV1::from_u16(observed_kind);
  let control_artifact_kind = GcArtifactKindV1::from_u16(control_kind);
  let error_class =
    GcErrorClassV1::from_u16(u16_at(body, 8)?).ok_or_else(|| kind_error("corrupt_evidence_error_class", "unknown GC error class"))?;
  if presence(flags, 0) && !(1..=0x0a).contains(&observed_entry)
    || presence(flags, 1) && observed_artifact_kind.is_none()
    || presence(flags, 7) && !control_artifact_kind.is_some_and(GcArtifactKindV1::is_control)
  {
    return Err(kind_error("corrupt_evidence_observed_kind", "corrupt evidence contains an unknown entry, artifact, or control kind"));
  }
  if u16_at(body, 14)? != 0 || u32_at(body, 28)? != 0 || u16_at(body, 58 + 2 * hash_width)? != 0 || u16_at(body, 66 + 3 * hash_width)? != 0
  {
    return Err(reserved_error("corrupt_evidence_reserved", "corrupt evidence reserves must be zero"));
  }
  if detected_at_ms <= 0 || !optional_valid {
    return Err(closure_error("corrupt_evidence_fields", "corrupt evidence optionals, range, or reserves are invalid"));
  }
  let context = &body[context_start..context_end];
  if context.is_empty() {
    return Err(closure_error("corrupt_evidence_context", "corrupt evidence context is empty"));
  }
  validate_canonical_value(context, CanonicalValueBounds::AUDIT_VALUE)?;
  let evidence_hashes = &body[context_end..];
  validate_sorted_hashes(evidence_hashes, hash_width, "corrupt_evidence_order")?;
  Ok(CorruptGcEvidenceV1 {
    database_id,
    evidence_id,
    generation: artifact.generation,
    detected_at_ms,
    error_class,
    observed_entry_type: presence(flags, 0).then_some(observed_entry),
    observed_artifact_kind: presence(flags, 1).then_some(observed_artifact_kind).flatten(),
    physical_range: presence(flags, 2).then_some((physical_offset, physical_length)),
    write_sequence: presence(flags, 3).then_some(write_sequence),
    expected_hash: presence(flags, 4).then_some(expected_hash),
    observed_hash: presence(flags, 5).then_some(observed_hash),
    run_id: presence(flags, 6).then_some(run_id),
    control_kind: presence(flags, 7).then_some(control_artifact_kind).flatten(),
    control_identity_digest: presence(flags, 7).then_some(control_digest),
    context,
    evidence_count,
    evidence_hashes,
    key,
  })
}

fn decode_pin(artifact: GcArtifactEnvelopeV1<'_>, algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<AuditPinV1<'_>> {
  let hash_width = algorithm.hash_length();
  if artifact.identity.len() != 32 || artifact.body.len() < 32 + hash_width {
    return Err(closure_error("audit_pin_shape", "audit pin identity or body is malformed"));
  }
  let database_id = &artifact.identity[..16];
  let pin_id = &artifact.identity[16..];
  if all_zero(database_id) || all_zero(pin_id) {
    return Err(identity_error("audit_pin_identity", "audit pin database/pin IDs must be nonzero"));
  }
  let body = artifact.body;
  let artifact_count = u32_at(body, 20 + hash_width)?;
  let artifacts_length = usize::try_from(u32_at(body, 24 + hash_width)?).map_err(|_| overflow_error("audit pin artifacts length"))?;
  if artifact_count == 0 {
    return Err(closure_error("audit_pin_count", "audit pin must root at least one artifact"));
  }
  if artifact_count as usize > MAX_PINS {
    return Err(amplification_error("audit_pin_count", artifact_count as usize, MAX_PINS));
  }
  if artifacts_length != checked_mul(artifact_count as usize, hash_width, "audit pin hashes")?
    || checked_add(32 + hash_width, artifacts_length, "audit pin body")? != body.len()
  {
    return Err(trailing_error("audit_pin_length", "audit pin lengths do not close"));
  }
  let created_at_ms = i64_at(body, 0)?;
  let expires_at_ms = i64_at(body, 8)?;
  let reason =
    AuditPinReasonV1::from_u16(u16_at(body, 16 + hash_width)?).ok_or_else(|| kind_error("audit_pin_reason", "unknown audit pin reason"))?;
  if u16_at(body, 18 + hash_width)? != 0 || u32_at(body, 28 + hash_width)? != 0 {
    return Err(reserved_error("audit_pin_reserved", "audit pin flags and reserve must be zero"));
  }
  if created_at_ms <= 0 || expires_at_ms != 0 && expires_at_ms <= created_at_ms || all_zero(&body[16..16 + hash_width]) {
    return Err(closure_error("audit_pin_fields", "audit pin time, creator, reason, or reserves are invalid"));
  }
  let artifact_hashes = &body[32 + hash_width..];
  validate_sorted_hashes(artifact_hashes, hash_width, "audit_pin_order")?;
  Ok(AuditPinV1 {
    database_id,
    pin_id,
    generation: artifact.generation,
    created_at_ms,
    expires_at_ms,
    creator_identity_digest: &body[16..16 + hash_width],
    reason,
    artifact_count,
    artifact_hashes,
    hash_width,
    key,
  })
}

pub fn validate_audit_directory_child(directory: &AuditArtifactV1<'_>, page: &AuditArtifactV1<'_>) -> FormatResult<()> {
  let (AuditArtifactV1::Directory(directory), AuditArtifactV1::Page(page)) = (directory, page) else {
    return Err(kind_error("audit_directory_closure", "closure requires an audit directory and page"));
  };
  if directory.role != page.role
    || directory.database_id != page.database_id
    || directory.catalog_id != page.catalog_id
    || directory.child_generation != page.generation
    || directory.page_id != page.page_id
    || directory.live_count != u64::from(page.record_count)
    || directory.logical_bytes != page.logical_bytes
    || directory.lower_fence != page.lower_fence
    || directory.upper_fence != page.upper_fence
    || directory.child_hash != page.key
  {
    return Err(closure_error("audit_directory_closure", "audit directory descriptor does not match page"));
  }
  Ok(())
}

pub fn validate_audit_manifest_directory(manifest: &AuditArtifactV1<'_>, directory: &AuditArtifactV1<'_>) -> FormatResult<()> {
  let (AuditArtifactV1::Manifest(manifest), AuditArtifactV1::Directory(directory)) = (manifest, directory) else {
    return Err(kind_error("audit_manifest_closure", "closure requires audit manifest and directory"));
  };
  let directory_oldest_at_ms = i64_at(directory.lower_fence, 0)?;
  let directory_newest_at_ms = i64_at(directory.upper_fence, 0)?;
  let valid = manifest.database_id == directory.database_id
    && manifest.generation == directory.child_generation
    && match directory.role {
      AuditDirectoryRoleV1::Detail => {
        manifest.detail_root == directory.key
          && manifest.detail_count == directory.live_count
          && manifest.detail_bytes == directory.logical_bytes
          && manifest.oldest_detail_at_ms == directory_oldest_at_ms
          && manifest.newest_detail_at_ms == directory_newest_at_ms
      }
      AuditDirectoryRoleV1::Summary => {
        manifest.summary_root == directory.key
          && manifest.summary_count == directory.live_count
          && manifest.summary_bytes == directory.logical_bytes
          && manifest.oldest_summary_at_ms == directory_oldest_at_ms
          && manifest.newest_summary_at_ms == directory_newest_at_ms
      }
    };
  if !valid {
    return Err(closure_error("audit_manifest_closure", "audit manifest aggregates do not match directory root"));
  }
  Ok(())
}

pub fn validate_run_summary_page_record(summary: &AuditArtifactV1<'_>, page: &AuditArtifactV1<'_>) -> FormatResult<()> {
  let (AuditArtifactV1::RunSummary(summary), AuditArtifactV1::Page(page)) = (summary, page) else {
    return Err(kind_error("audit_run_summary_closure", "closure requires run summary and audit page"));
  };
  if page.role != AuditDirectoryRoleV1::Summary {
    return Err(closure_error("audit_run_summary_closure", "run summary body is absent from summary page"));
  }
  let mut found = false;
  for record in page.iter() {
    if record?.raw() == summary.record.raw {
      found = true;
      break;
    }
  }
  if !found {
    return Err(closure_error("audit_run_summary_closure", "run summary body is absent from summary page"));
  }
  Ok(())
}

pub fn validate_audit_manifest_pin(manifest: &AuditArtifactV1<'_>, pin: &AuditArtifactV1<'_>) -> FormatResult<()> {
  let (AuditArtifactV1::Manifest(manifest), AuditArtifactV1::Pin(pin)) = (manifest, pin) else {
    return Err(kind_error("audit_manifest_pin_closure", "closure requires audit manifest and pin"));
  };
  if manifest.database_id != pin.database_id || !sorted_hashes_contain(manifest.pins, manifest.hash_width, &pin.key) {
    return Err(closure_error("audit_manifest_pin_closure", "audit manifest does not root pin"));
  }
  Ok(())
}

pub fn validate_audit_pin_target(
  pin: &AuditArtifactV1<'_>,
  target_database_id: &[u8],
  target_kind: GcArtifactKindV1,
  target_key: &[u8],
) -> FormatResult<()> {
  let AuditArtifactV1::Pin(pin) = pin else {
    return Err(kind_error("audit_pin_target_closure", "source is not an audit pin"));
  };
  if target_database_id != pin.database_id
    || target_kind.is_control()
    || target_key.len() != pin.hash_width
    || !sorted_hashes_contain(pin.artifact_hashes, pin.hash_width, target_key)
  {
    return Err(closure_error("audit_pin_target_closure", "audit pin does not root immutable GC target"));
  }
  Ok(())
}

fn validate_capabilities(bytes: &[u8]) -> FormatResult<()> {
  let mut expected = [0u8; 32];
  for bit in AUDIT_CAPABILITY_BITS {
    expected[bit / 8] |= 1 << (bit % 8);
  }
  if bytes.len() != expected.len() {
    return Err(trailing_error("audit_capability_width", "audit capability vector has wrong width"));
  }
  if bytes.iter().zip(expected).any(|(actual, required)| actual & !required != 0) {
    return Err(error(
      MalformedInputClass::UnknownRequiredCapability,
      "audit_unknown_capability",
      "audit catalog requires an unknown capability",
    ));
  }
  if bytes != expected {
    return Err(closure_error("audit_capabilities", "audit catalog capabilities do not match frozen set"));
  }
  Ok(())
}

fn validate_sorted_hashes(bytes: &[u8], hash_width: usize, code: &'static str) -> FormatResult<()> {
  let mut previous = None;
  for hash in bytes.chunks_exact(hash_width) {
    if all_zero(hash) || previous.is_some_and(|prior| prior >= hash) {
      return Err(order_error(code, "artifact hashes are zero, duplicate, or out of order"));
    }
    previous = Some(hash);
  }
  Ok(())
}

fn sorted_hashes_contain(bytes: &[u8], hash_width: usize, target: &[u8]) -> bool {
  if hash_width == 0 || target.len() != hash_width || !bytes.len().is_multiple_of(hash_width) {
    return false;
  }
  let mut lower = 0usize;
  let mut upper = bytes.len() / hash_width;
  while lower < upper {
    let middle = lower + (upper - lower) / 2;
    let start = middle * hash_width;
    let hash = &bytes[start..start + hash_width];
    match hash.cmp(target) {
      Ordering::Less => lower = middle + 1,
      Ordering::Equal => return true,
      Ordering::Greater => upper = middle,
    }
  }
  false
}

fn presence(flags: u8, bit: u8) -> bool {
  flags & (1 << bit) != 0
}

fn i64_at(bytes: &[u8], offset: usize) -> FormatResult<i64> {
  let raw = bytes.get(offset..offset + 8).ok_or_else(|| trailing_error("gc_audit_truncated", format!("i64 at offset {offset}")))?;
  Ok(i64::from_le_bytes(raw.try_into().expect("checked audit i64 width")))
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
}

fn ensure_cap(code: &'static str, actual: usize, cap: usize) -> FormatResult<()> {
  if actual > cap {
    return Err(amplification_error(code, actual, cap));
  }
  Ok(())
}

fn checked_add(left: usize, right: usize, context: &'static str) -> FormatResult<usize> {
  left.checked_add(right).ok_or_else(|| overflow_error(context))
}

fn checked_mul(left: usize, right: usize, context: &'static str) -> FormatResult<usize> {
  left.checked_mul(right).ok_or_else(|| overflow_error(context))
}

fn amplification_error(code: &'static str, actual: usize, cap: usize) -> FormatError {
  error(MalformedInputClass::AllocationAmplification, code, format!("{actual} exceeds cap {cap}"))
}

fn overflow_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "gc_audit_overflow", context)
}

fn trailing_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::TruncationOrTrailingBytes, code, context)
}

fn reserved_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::NonzeroReservedOrPadding, code, context)
}

fn identity_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::IdentityKeyOrGenerationMismatch, code, context)
}

fn kind_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::UnknownTypeKindOrEnum, code, context)
}

fn order_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::NoncanonicalOrderOrDuplicate, code, context)
}

fn closure_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::CrossRecordClosureMismatch, code, context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
