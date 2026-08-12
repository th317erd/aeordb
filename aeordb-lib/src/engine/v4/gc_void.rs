use std::cmp::Ordering;

use super::contract_generated::capability_bit;
use super::gc::{
  EncodedImmutableGcArtifactV1, GcArtifactEnvelopeV1, GcArtifactKindV1, ImmutableGcArtifactWriteV1, PhysicalIncarnationV1,
  compare_physical_incarnations_v1, decode_gc_artifact_envelope, decode_physical_incarnation, encode_immutable_gc_artifact,
  encode_physical_incarnation_into, immutable_gc_artifact_key, u16_at, u32_at, u64_at,
};
use super::hash::digest_parts;
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use crate::engine::HashAlgorithm;

const MAX_MANIFEST_LENGTH: usize = 1024 * 1024;
const MAX_PAGE_LENGTH: usize = 16 * 1024 * 1024;
const MAX_DIRECTORY_LENGTH: usize = 4 * 1024 * 1024;
const MAX_SWEEP_LENGTH: usize = 16 * 1024 * 1024;
const MAX_CANDIDATES: usize = 4_096;
const VOID_CAPABILITY_BITS: &[usize] = &[
  capability_bit::GC_ARTIFACT_V1 as usize,
  capability_bit::PHYSICAL_INVENTORY_V1 as usize,
  capability_bit::RECEIPT_BACKED_VOID_V1 as usize,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum VoidDirectoryRoleV1 {
  FreeExtents = 4,
  Claims = 5,
}

impl VoidDirectoryRoleV1 {
  pub fn from_u16(value: u16) -> Option<Self> {
    match value {
      4 => Some(Self::FreeExtents),
      5 => Some(Self::Claims),
      _ => None,
    }
  }

  pub fn name(self) -> &'static str {
    match self {
      Self::FreeExtents => "void-free-extents",
      Self::Claims => "void-claims",
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum VoidClaimSettlementOutcomeV1 {
  Settled = 1,
  Recovered = 2,
  AbandonedToQuarantine = 3,
}

impl VoidClaimSettlementOutcomeV1 {
  pub fn from_u16(value: u16) -> Option<Self> {
    match value {
      1 => Some(Self::Settled),
      2 => Some(Self::Recovered),
      3 => Some(Self::AbandonedToQuarantine),
      _ => None,
    }
  }

  pub fn name(self) -> &'static str {
    match self {
      Self::Settled => "settled",
      Self::Recovered => "recovered",
      Self::AbandonedToQuarantine => "abandoned",
    }
  }
}

#[derive(Debug, Clone)]
pub struct SweepProposalV1<'a> {
  pub database_id: &'a [u8],
  pub batch_id: &'a [u8],
  pub generation: u64,
  pub created_at_ms: i64,
  pub quarantine_manifest_hash: &'a [u8],
  pub candidate_count: u32,
  pub candidates: &'a [u8],
  pub key: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct SweepReceiptV1<'a> {
  pub recovered: bool,
  pub database_id: &'a [u8],
  pub batch_id: &'a [u8],
  pub generation: u64,
  pub reclaim_committed_at_ms: i64,
  pub proposal_hash: &'a [u8],
  pub void_catalog_hash: &'a [u8],
  pub outcome_count: u32,
  pub reclaimed_count: u64,
  pub reclaimed_bytes: u64,
  pub skipped_count: u64,
  pub failed_count: u64,
  pub outcomes: &'a [u8],
  pub key: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum SweepOutcomeClassV1 {
  Reclaimed = 1,
  SkippedReachable = 2,
  SkippedChanged = 3,
  SkippedPinned = 4,
  SkippedPolicy = 5,
  FailedIo = 6,
  FailedCorrupt = 7,
}

impl SweepOutcomeClassV1 {
  pub fn from_u16(value: u16) -> Option<Self> {
    match value {
      1 => Some(Self::Reclaimed),
      2 => Some(Self::SkippedReachable),
      3 => Some(Self::SkippedChanged),
      4 => Some(Self::SkippedPinned),
      5 => Some(Self::SkippedPolicy),
      6 => Some(Self::FailedIo),
      7 => Some(Self::FailedCorrupt),
      _ => None,
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SweepReceiptOutcomeV1<'a> {
  pub incarnation: PhysicalIncarnationV1<'a>,
  pub outcome: SweepOutcomeClassV1,
  pub stable_reason_detail: u16,
  pub resulting_void_offset: u64,
  pub resulting_void_length: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct SweepProposalWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8; 16],
  pub batch_id: &'a [u8; 16],
  pub generation: u64,
  pub created_at_ms: i64,
  pub quarantine_manifest_hash: &'a [u8],
  pub candidates: &'a [PhysicalIncarnationV1<'a>],
}

#[derive(Clone, Copy, Debug)]
pub struct SweepReceiptOutcomeWriteV1<'a> {
  pub incarnation: PhysicalIncarnationV1<'a>,
  pub outcome: SweepOutcomeClassV1,
  pub stable_reason_detail: u16,
  pub resulting_void_offset: u64,
  pub resulting_void_length: u32,
}

impl<'a> From<&SweepReceiptOutcomeV1<'a>> for SweepReceiptOutcomeWriteV1<'a> {
  fn from(value: &SweepReceiptOutcomeV1<'a>) -> Self {
    Self {
      incarnation: value.incarnation,
      outcome: value.outcome,
      stable_reason_detail: value.stable_reason_detail,
      resulting_void_offset: value.resulting_void_offset,
      resulting_void_length: value.resulting_void_length,
    }
  }
}

#[derive(Clone, Copy, Debug)]
pub struct SweepReceiptWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub recovered: bool,
  pub database_id: &'a [u8; 16],
  pub batch_id: &'a [u8; 16],
  pub generation: u64,
  pub reclaim_committed_at_ms: i64,
  pub proposal_hash: &'a [u8],
  pub void_catalog_hash: &'a [u8],
  pub outcomes: &'a [SweepReceiptOutcomeWriteV1<'a>],
}

impl<'a> SweepProposalV1<'a> {
  pub fn candidate_records(&self, algorithm: HashAlgorithm) -> FormatResult<SweepProposalCandidateRecordsV1<'a>> {
    let record_length = 24 + 2 * algorithm.hash_length();
    let record_count = self.candidate_count as usize;
    if self.quarantine_manifest_hash.len() != algorithm.hash_length()
      || record_count == 0
      || record_count > MAX_CANDIDATES
      || record_count.checked_mul(record_length) != Some(self.candidates.len())
    {
      return Err(closure_error("sweep_proposal_records", "sweep proposal candidate records do not match their declared shape"));
    }
    Ok(SweepProposalCandidateRecordsV1 { rows: self.candidates.chunks_exact(record_length), algorithm, previous: None, failed: false })
  }
}

#[derive(Debug)]
pub struct SweepProposalCandidateRecordsV1<'a> {
  rows: std::slice::ChunksExact<'a, u8>,
  algorithm: HashAlgorithm,
  previous: Option<PhysicalIncarnationV1<'a>>,
  failed: bool,
}

impl<'a> Iterator for SweepProposalCandidateRecordsV1<'a> {
  type Item = FormatResult<PhysicalIncarnationV1<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.failed {
      return None;
    }
    let row = self.rows.next()?;
    let incarnation = match decode_physical_incarnation(row, self.algorithm) {
      Ok(incarnation) => incarnation,
      Err(error) => {
        self.failed = true;
        return Some(Err(error));
      }
    };
    if self.previous.is_some_and(|previous| compare_physical_incarnations_v1(&previous, &incarnation) != Ordering::Less) {
      self.failed = true;
      return Some(Err(order_error("sweep_proposal_order", "sweep candidates are duplicate or out of order")));
    }
    self.previous = Some(incarnation);
    Some(Ok(incarnation))
  }
}

impl<'a> SweepReceiptV1<'a> {
  pub fn outcome_records(&self, algorithm: HashAlgorithm) -> FormatResult<SweepReceiptOutcomeRecordsV1<'a>> {
    let record_length = 48 + 2 * algorithm.hash_length();
    let record_count = self.outcome_count as usize;
    if self.proposal_hash.len() != algorithm.hash_length()
      || self.void_catalog_hash.len() != algorithm.hash_length()
      || record_count == 0
      || record_count > MAX_CANDIDATES
      || record_count.checked_mul(record_length) != Some(self.outcomes.len())
    {
      return Err(closure_error("sweep_receipt_records", "sweep receipt outcome records do not match their declared shape"));
    }
    Ok(SweepReceiptOutcomeRecordsV1 { rows: self.outcomes.chunks_exact(record_length), algorithm, previous: None, failed: false })
  }
}

#[derive(Debug)]
pub struct SweepReceiptOutcomeRecordsV1<'a> {
  rows: std::slice::ChunksExact<'a, u8>,
  algorithm: HashAlgorithm,
  previous: Option<PhysicalIncarnationV1<'a>>,
  failed: bool,
}

impl<'a> Iterator for SweepReceiptOutcomeRecordsV1<'a> {
  type Item = FormatResult<SweepReceiptOutcomeV1<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.failed {
      return None;
    }
    let row = self.rows.next()?;
    let outcome = match decode_sweep_receipt_outcome_v1(row, self.algorithm) {
      Ok(outcome) => outcome,
      Err(error) => {
        self.failed = true;
        return Some(Err(error));
      }
    };
    if self.previous.is_some_and(|previous| compare_physical_incarnations_v1(&previous, &outcome.incarnation) != Ordering::Less) {
      self.failed = true;
      return Some(Err(order_error("sweep_receipt_order", "sweep outcomes are duplicate or out of order")));
    }
    self.previous = Some(outcome.incarnation);
    Some(Ok(outcome))
  }
}

#[derive(Debug, Clone)]
pub struct VoidExtentPageV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8],
  pub catalog_id: &'a [u8],
  pub generation: u64,
  pub page_id: u64,
  pub record_count: u32,
  pub total_bytes: u64,
  pub lower_offset: u64,
  pub upper_offset: u64,
  pub records: &'a [u8],
  pub key: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct VoidDirectoryV1<'a> {
  pub role: VoidDirectoryRoleV1,
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
pub struct VoidCatalogManifestV1<'a> {
  pub database_id: &'a [u8],
  pub generation: u64,
  pub free_root: &'a [u8],
  pub claim_root: &'a [u8],
  pub free_count: u64,
  pub free_bytes: u64,
  pub claim_count: u64,
  pub claimed_bytes: u64,
  pub key: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct VoidClaimV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8],
  pub claim_id: &'a [u8],
  pub generation: u64,
  pub source_manifest_hash: &'a [u8],
  pub extent_count: u32,
  pub total_bytes: u64,
  pub extents: &'a [u8],
  pub stored_length: u64,
  pub key: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct VoidClaimSettlementV1<'a> {
  pub database_id: &'a [u8],
  pub claim_id: &'a [u8],
  pub generation: u64,
  pub recovered: bool,
  pub outcome: VoidClaimSettlementOutcomeV1,
  pub source_manifest_hash: &'a [u8],
  pub result_manifest_hash: &'a [u8],
  pub used_count: u32,
  pub unused_count: u32,
  pub used_bytes: u64,
  pub returned_bytes: u64,
  pub key: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum SweepVoidArtifactV1<'a> {
  SweepProposal(SweepProposalV1<'a>),
  SweepReceipt(SweepReceiptV1<'a>),
  VoidExtentPage(VoidExtentPageV1<'a>),
  VoidDirectory(VoidDirectoryV1<'a>),
  VoidCatalog(VoidCatalogManifestV1<'a>),
  VoidClaim(VoidClaimV1<'a>),
  VoidClaimSettlement(VoidClaimSettlementV1<'a>),
}

impl SweepVoidArtifactV1<'_> {
  pub fn key(&self) -> &[u8] {
    match self {
      Self::SweepProposal(value) => &value.key,
      Self::SweepReceipt(value) => &value.key,
      Self::VoidExtentPage(value) => &value.key,
      Self::VoidDirectory(value) => &value.key,
      Self::VoidCatalog(value) => &value.key,
      Self::VoidClaim(value) => &value.key,
      Self::VoidClaimSettlement(value) => &value.key,
    }
  }

  pub fn summary(&self) -> String {
    match self {
      Self::SweepProposal(value) => format!("gc:proposal:sweep:candidates={}:mark={}", value.candidate_count, value.generation),
      Self::SweepReceipt(value) => format!(
        "gc:receipt:sweep-{}:outcomes={}:reclaimed={}:skipped={}:failed={}",
        if value.recovered { "recovered" } else { "commit" },
        value.outcome_count,
        value.reclaimed_count,
        value.skipped_count,
        value.failed_count
      ),
      Self::VoidExtentPage(value) => format!("gc:page:void-free-extents:records={}", value.record_count),
      Self::VoidDirectory(value) => format!("gc:directory:{}:records={}", value.role.name(), value.live_count),
      Self::VoidCatalog(value) => format!(
        "gc:manifest:void-catalog:{}:free={}:claims={}:generation={}",
        if value.free_count == 0 && value.claim_count == 0 { "empty" } else { "populated" },
        value.free_count,
        value.claim_count,
        value.generation
      ),
      Self::VoidClaim(value) => format!("gc:claim:void:extents={}:bytes={}", value.extent_count, value.total_bytes),
      Self::VoidClaimSettlement(value) => {
        format!("gc:receipt:void-claim-settlement:{}:used={}:unused={}", value.outcome.name(), value.used_count, value.unused_count)
      }
    }
  }
}

pub fn decode_sweep_void_artifact(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<SweepVoidArtifactV1<'_>> {
  let hinted_kind = bytes.get(6..8).map(|raw| u16::from_le_bytes([raw[0], raw[1]])).and_then(GcArtifactKindV1::from_u16);
  let cap = match hinted_kind {
    Some(GcArtifactKindV1::VoidCatalogManifest | GcArtifactKindV1::VoidClaimSettlementReceipt) => MAX_MANIFEST_LENGTH,
    Some(GcArtifactKindV1::VoidExtentPage) => MAX_PAGE_LENGTH,
    Some(GcArtifactKindV1::GcArtifactDirectoryNode) => MAX_DIRECTORY_LENGTH,
    Some(
      GcArtifactKindV1::SweepProposal
      | GcArtifactKindV1::SweepCommitReceipt
      | GcArtifactKindV1::RecoveredSweepReceipt
      | GcArtifactKindV1::VoidClaim,
    ) => MAX_SWEEP_LENGTH,
    _ => super::gc::MAX_GC_ARTIFACT_LENGTH,
  };
  ensure_cap("sweep_void_artifact_length", bytes.len(), cap)?;
  let envelope = decode_gc_artifact_envelope(bytes)?;
  let key = immutable_gc_artifact_key(algorithm, envelope.kind, bytes);
  match envelope.kind {
    GcArtifactKindV1::SweepProposal => decode_sweep_proposal(envelope, algorithm, key).map(SweepVoidArtifactV1::SweepProposal),
    GcArtifactKindV1::SweepCommitReceipt | GcArtifactKindV1::RecoveredSweepReceipt => {
      decode_sweep_receipt(envelope, algorithm, key).map(SweepVoidArtifactV1::SweepReceipt)
    }
    GcArtifactKindV1::VoidExtentPage => decode_void_extent_page(envelope, algorithm, key).map(SweepVoidArtifactV1::VoidExtentPage),
    GcArtifactKindV1::GcArtifactDirectoryNode => decode_void_directory(envelope, algorithm, key).map(SweepVoidArtifactV1::VoidDirectory),
    GcArtifactKindV1::VoidCatalogManifest => decode_void_catalog(envelope, algorithm, key).map(SweepVoidArtifactV1::VoidCatalog),
    GcArtifactKindV1::VoidClaim => decode_void_claim(envelope, bytes.len(), algorithm, key).map(SweepVoidArtifactV1::VoidClaim),
    GcArtifactKindV1::VoidClaimSettlementReceipt => {
      decode_void_claim_settlement(envelope, algorithm, key).map(SweepVoidArtifactV1::VoidClaimSettlement)
    }
    _ => Err(kind_error("sweep_void_artifact_kind", format!("{} is not a sweep/Void artifact", envelope.kind.name()))),
  }
}

fn decode_sweep_proposal(artifact: GcArtifactEnvelopeV1<'_>, algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<SweepProposalV1<'_>> {
  let hash_width = algorithm.hash_length();
  if artifact.identity.len() != 32 || artifact.body.len() < 32 + 2 * hash_width {
    return Err(closure_error("sweep_proposal_shape", "sweep proposal identity or body shape is invalid"));
  }
  let database_id = &artifact.identity[..16];
  let batch_id = &artifact.identity[16..];
  if all_zero(database_id) || all_zero(batch_id) {
    return Err(identity_error("sweep_proposal_identity", "sweep proposal database and batch IDs must be nonzero"));
  }
  let body = artifact.body;
  if u32_at(body, 0)? != 0 || u16_at(body, 6)? != 0 {
    return Err(reserved_error("sweep_proposal_reserved", "sweep proposal flags/reserve must be zero"));
  }
  if u16_at(body, 4)? != 1 {
    return Err(kind_error("sweep_proposal_codec", "sweep proposal codec is not 1"));
  }
  if i64_at(body, 8)? <= 0 || all_zero(&body[16..16 + hash_width]) || u64_at(body, 16 + hash_width)? != artifact.generation {
    return Err(identity_error("sweep_proposal_generation", "sweep proposal time, quarantine root, or generation is invalid"));
  }
  let count = u32_at(body, 24 + hash_width)?;
  if count == 0 || count as usize > MAX_CANDIDATES {
    return Err(amplification_error("sweep_proposal_count", count as usize, MAX_CANDIDATES));
  }
  let record_length = 24 + 2 * hash_width;
  let expected_records = checked_mul(count as usize, record_length, "sweep proposal records")?;
  let records_length = usize::try_from(u32_at(body, 28 + hash_width)?).map_err(|_| overflow_error("sweep proposal records length"))?;
  if records_length != expected_records || checked_add(32 + 2 * hash_width, records_length, "sweep proposal body")? != body.len() {
    return Err(trailing_error("sweep_proposal_length", "sweep proposal record lengths do not close"));
  }
  let records = &body[32 + 2 * hash_width..];
  let digest = digest_parts(algorithm, &[b"aeordb.sweep-proposal.v1\0", records]);
  if body[32 + hash_width..32 + 2 * hash_width] != digest {
    return Err(error(MalformedInputClass::ChecksumOrIntegrityMismatch, "sweep_proposal_digest", "sweep proposal digest does not match"));
  }
  let proposal = SweepProposalV1 {
    database_id,
    batch_id,
    generation: artifact.generation,
    created_at_ms: i64_at(body, 8)?,
    quarantine_manifest_hash: &body[16..16 + hash_width],
    candidate_count: count,
    candidates: records,
    key,
  };
  for candidate in proposal.candidate_records(algorithm)? {
    candidate?;
  }
  Ok(proposal)
}

fn decode_sweep_receipt(artifact: GcArtifactEnvelopeV1<'_>, algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<SweepReceiptV1<'_>> {
  let hash_width = algorithm.hash_length();
  if artifact.identity.len() != 32 || artifact.body.len() < 64 + 2 * hash_width {
    return Err(closure_error("sweep_receipt_shape", "sweep receipt identity or body shape is invalid"));
  }
  let recovered = artifact.kind == GcArtifactKindV1::RecoveredSweepReceipt;
  let database_id = &artifact.identity[..16];
  let batch_id = &artifact.identity[16..];
  if all_zero(database_id) || all_zero(batch_id) {
    return Err(identity_error("sweep_receipt_identity", "sweep receipt database and batch IDs must be nonzero"));
  }
  let body = artifact.body;
  if u32_at(body, 0)? != u32::from(recovered) || u16_at(body, 6)? != 0 {
    return Err(reserved_error("sweep_receipt_flags", "sweep receipt kind, flags, or reserve disagree"));
  }
  if u16_at(body, 4)? != 1 {
    return Err(kind_error("sweep_receipt_codec", "sweep receipt codec is not 1"));
  }
  if i64_at(body, 8)? <= 0
    || body[16..16 + 2 * hash_width].chunks_exact(hash_width).any(all_zero)
    || u64_at(body, 16 + 2 * hash_width)? != artifact.generation
  {
    return Err(identity_error("sweep_receipt_roots", "sweep receipt timestamp, roots, or generation is invalid"));
  }
  let count = u32_at(body, 24 + 2 * hash_width)?;
  if count == 0 || count as usize > MAX_CANDIDATES {
    return Err(amplification_error("sweep_receipt_count", count as usize, MAX_CANDIDATES));
  }
  let record_length = 48 + 2 * hash_width;
  let expected_records = checked_mul(count as usize, record_length, "sweep outcome records")?;
  let records_length = usize::try_from(u32_at(body, 28 + 2 * hash_width)?).map_err(|_| overflow_error("sweep outcome length"))?;
  if records_length != expected_records || checked_add(64 + 2 * hash_width, records_length, "sweep receipt body")? != body.len() {
    return Err(trailing_error("sweep_receipt_length", "sweep outcome lengths do not close"));
  }
  let receipt = SweepReceiptV1 {
    recovered,
    database_id,
    batch_id,
    generation: artifact.generation,
    reclaim_committed_at_ms: i64_at(body, 8)?,
    proposal_hash: &body[16..16 + hash_width],
    void_catalog_hash: &body[16 + hash_width..16 + 2 * hash_width],
    outcome_count: count,
    reclaimed_count: u64_at(body, 32 + 2 * hash_width)?,
    reclaimed_bytes: u64_at(body, 40 + 2 * hash_width)?,
    skipped_count: u64_at(body, 48 + 2 * hash_width)?,
    failed_count: u64_at(body, 56 + 2 * hash_width)?,
    outcomes: &body[64 + 2 * hash_width..],
    key,
  };
  let mut reclaimed_count = 0u64;
  let mut reclaimed_bytes = 0u64;
  let mut skipped_count = 0u64;
  let mut failed_count = 0u64;
  for outcome in receipt.outcome_records(algorithm)? {
    let outcome = outcome?;
    match outcome.outcome {
      SweepOutcomeClassV1::Reclaimed => {
        reclaimed_count = reclaimed_count.checked_add(1).ok_or_else(|| overflow_error("reclaimed count"))?;
        reclaimed_bytes =
          reclaimed_bytes.checked_add(u64::from(outcome.resulting_void_length)).ok_or_else(|| overflow_error("reclaimed byte total"))?;
      }
      SweepOutcomeClassV1::SkippedReachable
      | SweepOutcomeClassV1::SkippedChanged
      | SweepOutcomeClassV1::SkippedPinned
      | SweepOutcomeClassV1::SkippedPolicy => {
        skipped_count = skipped_count.checked_add(1).ok_or_else(|| overflow_error("skipped count"))?;
      }
      SweepOutcomeClassV1::FailedIo | SweepOutcomeClassV1::FailedCorrupt => {
        failed_count = failed_count.checked_add(1).ok_or_else(|| overflow_error("failed count"))?;
      }
    }
  }
  if receipt.reclaimed_count != reclaimed_count
    || receipt.reclaimed_bytes != reclaimed_bytes
    || receipt.skipped_count != skipped_count
    || receipt.failed_count != failed_count
  {
    return Err(closure_error("sweep_receipt_totals", "sweep receipt counters do not match outcomes"));
  }
  Ok(receipt)
}

pub fn decode_sweep_receipt_outcome_v1(row: &[u8], algorithm: HashAlgorithm) -> FormatResult<SweepReceiptOutcomeV1<'_>> {
  let physical_length = 24 + 2 * algorithm.hash_length();
  if row.len() != physical_length + 24 {
    return Err(trailing_error("sweep_receipt_outcome_length", "sweep outcome has the wrong fixed length"));
  }
  let incarnation = decode_physical_incarnation(&row[..physical_length], algorithm)?;
  let outcome = SweepOutcomeClassV1::from_u16(u16_at(row, physical_length)?)
    .ok_or_else(|| kind_error("sweep_receipt_outcome", "unknown sweep outcome"))?;
  let stable_reason_detail = u16_at(row, physical_length + 2)?;
  let resulting_void_offset = u64_at(row, physical_length + 8)?;
  let resulting_void_length = u32_at(row, physical_length + 16)?;
  if u32_at(row, physical_length + 4)? != 0 || u32_at(row, physical_length + 20)? != 0 {
    return Err(reserved_error("sweep_receipt_outcome_reserved", "sweep outcome reserve is nonzero"));
  }
  if (outcome == SweepOutcomeClassV1::Reclaimed) != (resulting_void_offset != 0 && resulting_void_length != 0)
    || outcome == SweepOutcomeClassV1::Reclaimed
      && (resulting_void_offset != incarnation.wal_offset
        || resulting_void_length != incarnation.entity_length
        || stable_reason_detail != 0)
    || outcome != SweepOutcomeClassV1::Reclaimed && (resulting_void_offset != 0 || resulting_void_length != 0 || stable_reason_detail == 0)
  {
    return Err(closure_error("sweep_receipt_outcome_fields", "sweep outcome result does not match its class/incarnation"));
  }
  Ok(SweepReceiptOutcomeV1 { incarnation, outcome, stable_reason_detail, resulting_void_offset, resulting_void_length })
}

pub fn encode_sweep_proposal_v1(request: &SweepProposalWriteV1<'_>) -> FormatResult<EncodedImmutableGcArtifactV1> {
  let hash_width = request.hash_algorithm.hash_length();
  if request.database_id.iter().all(|byte| *byte == 0)
    || request.batch_id.iter().all(|byte| *byte == 0)
    || request.generation == 0
    || request.created_at_ms <= 0
    || request.quarantine_manifest_hash.len() != hash_width
    || all_zero(request.quarantine_manifest_hash)
    || request.candidates.is_empty()
    || request.candidates.len() > MAX_CANDIDATES
  {
    return Err(identity_error("sweep_proposal_write", "sweep proposal write identity, time, generation, hash, or count is invalid"));
  }
  let record_length = 24 + 2 * hash_width;
  let records_length =
    request.candidates.len().checked_mul(record_length).ok_or_else(|| overflow_error("sweep proposal records length"))?;
  let mut records = vec![0u8; records_length];
  let mut previous = None;
  for (index, candidate) in request.candidates.iter().enumerate() {
    if previous.is_some_and(|prior| compare_physical_incarnations_v1(&prior, candidate) != Ordering::Less) {
      return Err(order_error("sweep_proposal_order", "sweep candidates are duplicate or out of order"));
    }
    let start = index * record_length;
    encode_physical_incarnation_into(&mut records[start..start + record_length], candidate, request.hash_algorithm)?;
    previous = Some(*candidate);
  }
  let fixed_length = 32 + 2 * hash_width;
  let body_length = fixed_length.checked_add(records_length).ok_or_else(|| overflow_error("sweep proposal body length"))?;
  let mut body = vec![0u8; body_length];
  put_u16(&mut body, 4, 1);
  put_i64(&mut body, 8, request.created_at_ms);
  body[16..16 + hash_width].copy_from_slice(request.quarantine_manifest_hash);
  put_u64(&mut body, 16 + hash_width, request.generation);
  put_u32(&mut body, 24 + hash_width, request.candidates.len() as u32);
  put_u32(&mut body, 28 + hash_width, records_length as u32);
  let digest = digest_parts(request.hash_algorithm, &[b"aeordb.sweep-proposal.v1\0", &records]);
  body[32 + hash_width..fixed_length].copy_from_slice(&digest);
  body[fixed_length..].copy_from_slice(&records);
  let mut identity = [0u8; 32];
  identity[..16].copy_from_slice(request.database_id);
  identity[16..].copy_from_slice(request.batch_id);
  let encoded = encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 {
    kind: GcArtifactKindV1::SweepProposal,
    hash_algorithm: request.hash_algorithm,
    generation: request.generation,
    identity: &identity,
    body: &body,
  })?;
  let SweepVoidArtifactV1::SweepProposal(decoded) = decode_sweep_void_artifact(&encoded.value, request.hash_algorithm)? else {
    return Err(closure_error("sweep_proposal_write", "encoded sweep proposal decoded as another artifact kind"));
  };
  if decoded.key != encoded.key || decoded.candidate_count as usize != request.candidates.len() {
    return Err(closure_error("sweep_proposal_write", "encoded sweep proposal disagrees with its request"));
  }
  Ok(encoded)
}

pub fn encode_sweep_receipt_v1(request: &SweepReceiptWriteV1<'_>) -> FormatResult<EncodedImmutableGcArtifactV1> {
  let hash_width = request.hash_algorithm.hash_length();
  if request.database_id.iter().all(|byte| *byte == 0)
    || request.batch_id.iter().all(|byte| *byte == 0)
    || request.generation == 0
    || request.reclaim_committed_at_ms <= 0
    || request.proposal_hash.len() != hash_width
    || request.void_catalog_hash.len() != hash_width
    || all_zero(request.proposal_hash)
    || all_zero(request.void_catalog_hash)
    || request.outcomes.is_empty()
    || request.outcomes.len() > MAX_CANDIDATES
  {
    return Err(identity_error("sweep_receipt_write", "sweep receipt write identity, time, generation, hashes, or count is invalid"));
  }
  let record_length = 48 + 2 * hash_width;
  let records_length = request.outcomes.len().checked_mul(record_length).ok_or_else(|| overflow_error("sweep receipt records length"))?;
  let mut records = vec![0u8; records_length];
  let mut previous = None;
  let mut reclaimed_count = 0u64;
  let mut reclaimed_bytes = 0u64;
  let mut skipped_count = 0u64;
  let mut failed_count = 0u64;
  for (index, outcome) in request.outcomes.iter().enumerate() {
    if previous.is_some_and(|prior| compare_physical_incarnations_v1(&prior, &outcome.incarnation) != Ordering::Less) {
      return Err(order_error("sweep_receipt_order", "sweep outcomes are duplicate or out of order"));
    }
    let start = index * record_length;
    let record = &mut records[start..start + record_length];
    encode_physical_incarnation_into(&mut record[..24 + 2 * hash_width], &outcome.incarnation, request.hash_algorithm)?;
    put_u16(record, 24 + 2 * hash_width, outcome.outcome as u16);
    put_u16(record, 26 + 2 * hash_width, outcome.stable_reason_detail);
    put_u64(record, 32 + 2 * hash_width, outcome.resulting_void_offset);
    put_u32(record, 40 + 2 * hash_width, outcome.resulting_void_length);
    let decoded = decode_sweep_receipt_outcome_v1(record, request.hash_algorithm)?;
    match decoded.outcome {
      SweepOutcomeClassV1::Reclaimed => {
        reclaimed_count = reclaimed_count.checked_add(1).ok_or_else(|| overflow_error("reclaimed count"))?;
        reclaimed_bytes =
          reclaimed_bytes.checked_add(u64::from(decoded.resulting_void_length)).ok_or_else(|| overflow_error("reclaimed byte total"))?;
      }
      SweepOutcomeClassV1::SkippedReachable
      | SweepOutcomeClassV1::SkippedChanged
      | SweepOutcomeClassV1::SkippedPinned
      | SweepOutcomeClassV1::SkippedPolicy => {
        skipped_count = skipped_count.checked_add(1).ok_or_else(|| overflow_error("skipped count"))?;
      }
      SweepOutcomeClassV1::FailedIo | SweepOutcomeClassV1::FailedCorrupt => {
        failed_count = failed_count.checked_add(1).ok_or_else(|| overflow_error("failed count"))?;
      }
    }
    previous = Some(outcome.incarnation);
  }
  let fixed_length = 64 + 2 * hash_width;
  let body_length = fixed_length.checked_add(records_length).ok_or_else(|| overflow_error("sweep receipt body length"))?;
  let mut body = vec![0u8; body_length];
  put_u32(&mut body, 0, u32::from(request.recovered));
  put_u16(&mut body, 4, 1);
  put_i64(&mut body, 8, request.reclaim_committed_at_ms);
  body[16..16 + hash_width].copy_from_slice(request.proposal_hash);
  body[16 + hash_width..16 + 2 * hash_width].copy_from_slice(request.void_catalog_hash);
  put_u64(&mut body, 16 + 2 * hash_width, request.generation);
  put_u32(&mut body, 24 + 2 * hash_width, request.outcomes.len() as u32);
  put_u32(&mut body, 28 + 2 * hash_width, records_length as u32);
  put_u64(&mut body, 32 + 2 * hash_width, reclaimed_count);
  put_u64(&mut body, 40 + 2 * hash_width, reclaimed_bytes);
  put_u64(&mut body, 48 + 2 * hash_width, skipped_count);
  put_u64(&mut body, 56 + 2 * hash_width, failed_count);
  body[fixed_length..].copy_from_slice(&records);
  let mut identity = [0u8; 32];
  identity[..16].copy_from_slice(request.database_id);
  identity[16..].copy_from_slice(request.batch_id);
  let kind = if request.recovered { GcArtifactKindV1::RecoveredSweepReceipt } else { GcArtifactKindV1::SweepCommitReceipt };
  let encoded = encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 {
    kind,
    hash_algorithm: request.hash_algorithm,
    generation: request.generation,
    identity: &identity,
    body: &body,
  })?;
  let SweepVoidArtifactV1::SweepReceipt(decoded) = decode_sweep_void_artifact(&encoded.value, request.hash_algorithm)? else {
    return Err(closure_error("sweep_receipt_write", "encoded sweep receipt decoded as another artifact kind"));
  };
  if decoded.key != encoded.key || decoded.outcome_count as usize != request.outcomes.len() || decoded.recovered != request.recovered {
    return Err(closure_error("sweep_receipt_write", "encoded sweep receipt disagrees with its request"));
  }
  Ok(encoded)
}

fn decode_void_extent_page(
  artifact: GcArtifactEnvelopeV1<'_>,
  algorithm: HashAlgorithm,
  key: Vec<u8>,
) -> FormatResult<VoidExtentPageV1<'_>> {
  let hash_width = algorithm.hash_length();
  let row_length = 32 + 3 * hash_width;
  if artifact.identity.len() != 42 || artifact.body.len() < 80 {
    return Err(closure_error("void_extent_page_shape", "Void extent page identity or body shape is invalid"));
  }
  let database_id = &artifact.identity[..16];
  let catalog_id = &artifact.identity[16..32];
  let page_id = u64_at(artifact.identity, 34)?;
  if all_zero(database_id)
    || all_zero(catalog_id)
    || u16_at(artifact.identity, 32)? != VoidDirectoryRoleV1::FreeExtents as u16
    || page_id == 0
  {
    return Err(identity_error("void_extent_page_identity", "Void extent page identity is invalid"));
  }
  let body = artifact.body;
  if u32_at(body, 0)? != 0 || u16_at(body, 6)? != VoidDirectoryRoleV1::FreeExtents as u16 || body[40..64].iter().any(|byte| *byte != 0) {
    return Err(reserved_error("void_extent_page_reserved", "Void extent page flags, role, or reserve is invalid"));
  }
  if u16_at(body, 4)? != 1 {
    return Err(kind_error("void_extent_page_codec", "Void extent page codec is not 1"));
  }
  let count = u32_at(body, 16)?;
  if u32_at(body, 8)? != 8 || u32_at(body, 12)? != 8 || count == 0 || u32_at(body, 20)? != count {
    return Err(closure_error("void_extent_page_header", "Void extent fences or record counts are invalid"));
  }
  let expected_records = checked_mul(count as usize, row_length, "Void extent page records")?;
  let records_length = usize::try_from(u64_at(body, 24)?).map_err(|_| overflow_error("Void extent page records length"))?;
  if records_length != expected_records || checked_add(80, records_length, "Void extent page body")? != body.len() {
    return Err(trailing_error("void_extent_page_length", "Void extent records do not close the page"));
  }
  let lower_offset = u64_at(body, 64)?;
  let upper_offset = u64_at(body, 72)?;
  let records = &body[80..];
  let mut total_bytes = 0u64;
  let mut previous_end = None;
  let mut first = None;
  let mut last = None;
  for row in records.chunks_exact(row_length) {
    let extent = decode_void_extent(row, algorithm)?;
    if previous_end.is_some_and(|end| end > extent.offset) {
      return Err(order_error("void_extent_page_order", "Void extents overlap or are out of order"));
    }
    first.get_or_insert(extent.offset);
    last = Some(extent.offset);
    previous_end = Some(extent.offset + u64::from(extent.length));
    total_bytes = total_bytes.checked_add(u64::from(extent.length)).ok_or_else(|| overflow_error("Void extent byte total"))?;
  }
  if first != Some(lower_offset) || last != Some(upper_offset) || total_bytes != u64_at(body, 32)? {
    return Err(closure_error("void_extent_page_totals", "Void extent fences or totals do not match records"));
  }
  Ok(VoidExtentPageV1 {
    hash_algorithm: algorithm,
    database_id,
    catalog_id,
    generation: artifact.generation,
    page_id,
    record_count: count,
    total_bytes,
    lower_offset,
    upper_offset,
    records,
    key,
  })
}

struct VoidExtentView<'a> {
  offset: u64,
  length: u32,
  proposal_hash: &'a [u8],
}

fn decode_void_extent(row: &[u8], algorithm: HashAlgorithm) -> FormatResult<VoidExtentView<'_>> {
  let hash_width = algorithm.hash_length();
  if row.len() != 32 + 3 * hash_width {
    return Err(trailing_error("void_extent_length", "Void extent row has wrong fixed length"));
  }
  let offset = u64_at(row, 0)?;
  let length = u32_at(row, 8)?;
  if u32_at(row, 12)? != 0 {
    return Err(reserved_error("void_extent_reserved", "Void extent reserve is nonzero"));
  }
  if offset < 2_048
    || length == 0
    || offset.checked_add(u64::from(length)).is_none()
    || row[16..16 + 3 * hash_width].chunks(hash_width).any(all_zero)
    || u64_at(row, 16 + 3 * hash_width)? == 0
    || u64_at(row, 24 + 3 * hash_width)? == 0
  {
    return Err(identity_error("void_extent_fields", "Void extent identity, range, or publication sequence is invalid"));
  }
  Ok(VoidExtentView { offset, length, proposal_hash: &row[16..16 + hash_width] })
}

fn decode_void_directory(artifact: GcArtifactEnvelopeV1<'_>, algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<VoidDirectoryV1<'_>> {
  if artifact.identity.len() != 34 || artifact.body.len() < 80 {
    return Err(closure_error("void_directory_shape", "Void directory identity or body shape is invalid"));
  }
  let role = VoidDirectoryRoleV1::from_u16(u16_at(artifact.identity, 32)?)
    .ok_or_else(|| kind_error("void_directory_role", "artifact directory role is not a Void role"))?;
  let database_id = &artifact.identity[..16];
  let catalog_id = &artifact.identity[16..32];
  if all_zero(database_id) || all_zero(catalog_id) {
    return Err(identity_error("void_directory_identity", "Void directory database/catalog IDs must be nonzero"));
  }
  let body = artifact.body;
  if u16_at(body, 0)? != 0 || u32_at(body, 8)? != 0 || u32_at(body, 12)? != 0 || u32_at(body, 76)? != 0 {
    return Err(reserved_error("void_directory_reserved", "Void directory reserve fields must be zero"));
  }
  if u16_at(body, 2)? != role as u16 || u32_at(body, 4)? != 1 {
    return Err(kind_error("void_directory_codec", "Void directory role/codec does not match identity"));
  }
  let lower_length = usize::try_from(u32_at(body, 16)?).map_err(|_| overflow_error("Void lower fence length"))?;
  let upper_length = usize::try_from(u32_at(body, 20)?).map_err(|_| overflow_error("Void upper fence length"))?;
  if lower_length == 0 || upper_length == 0 {
    return Err(closure_error("void_directory_fences", "Void directory fences must be nonempty"));
  }
  let hash_width = algorithm.hash_length();
  let descriptor_fixed = 72 + hash_width;
  let entries_length = usize::try_from(u32_at(body, 72)?).map_err(|_| overflow_error("Void directory entries length"))?;
  if entries_length != checked_add(descriptor_fixed, lower_length + upper_length, "Void directory descriptor")?
    || checked_add(80 + lower_length + upper_length, entries_length, "Void directory body")? != body.len()
  {
    return Err(trailing_error("void_directory_length", "Void directory lengths do not close"));
  }
  let lower_fence = &body[80..80 + lower_length];
  let upper_fence = &body[80 + lower_length..80 + lower_length + upper_length];
  let cursor = 80 + lower_length + upper_length;
  let child_lower_length = usize::try_from(u32_at(body, cursor)?).map_err(|_| overflow_error("Void child lower fence length"))?;
  let child_upper_length = usize::try_from(u32_at(body, cursor + 4)?).map_err(|_| overflow_error("Void child upper fence length"))?;
  if child_lower_length != lower_length || child_upper_length != upper_length {
    return Err(closure_error("void_directory_descriptor_fences", "Void child fence lengths disagree"));
  }
  let page_id = u64_at(body, cursor + 8)?;
  let child_hash = &body[cursor + 16..cursor + 16 + hash_width];
  let fields = cursor + 16 + hash_width;
  let child_generation = u64_at(body, fields)?;
  let live_count = u64_at(body, fields + 8)?;
  let logical_bytes = u64_at(body, fields + 24)?;
  let key_start = cursor + descriptor_fixed;
  let fences_ordered = match role {
    VoidDirectoryRoleV1::FreeExtents if lower_length == 8 && upper_length == 8 => u64_at(lower_fence, 0)? <= u64_at(upper_fence, 0)?,
    VoidDirectoryRoleV1::Claims if lower_length == 16 && upper_length == 16 => lower_fence <= upper_fence,
    _ => false,
  };
  let expected_page_count = u64::from(role == VoidDirectoryRoleV1::FreeExtents);
  if all_zero(child_hash)
    || child_generation == 0
    || child_generation > artifact.generation
    || live_count == 0
    || logical_bytes == 0
    || !fences_ordered
    || (role == VoidDirectoryRoleV1::FreeExtents) != (page_id != 0)
    || body[fields + 32..fields + 56].iter().any(|byte| *byte != 0)
    || body[key_start..key_start + lower_length] != *lower_fence
    || body[key_start + lower_length..] != *upper_fence
    || u64_at(body, 24)? != live_count
    || u64_at(body, 32)? != 0
    || u64_at(body, 40)? != expected_page_count
    || u64_at(body, 48)? != logical_bytes
    || u64_at(body, 56)? != page_id
    || u64_at(body, 64)? != page_id
  {
    return Err(closure_error("void_directory_descriptor", "Void directory aggregate, descriptor, or fences are invalid"));
  }
  Ok(VoidDirectoryV1 {
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

fn decode_void_catalog(
  artifact: GcArtifactEnvelopeV1<'_>,
  algorithm: HashAlgorithm,
  key: Vec<u8>,
) -> FormatResult<VoidCatalogManifestV1<'_>> {
  let hash_width = algorithm.hash_length();
  if artifact.identity.len() != 24 || artifact.body.len() != 92 + 2 * hash_width {
    return Err(closure_error("void_catalog_shape", "Void catalog identity or body shape is invalid"));
  }
  let database_id = &artifact.identity[..16];
  if all_zero(database_id) || u64_at(artifact.identity, 16)? != artifact.generation {
    return Err(identity_error("void_catalog_identity", "Void catalog database/generation identity is invalid"));
  }
  let body = artifact.body;
  if u32_at(body, 0)? != 0 {
    return Err(reserved_error("void_catalog_flags", "Void catalog flags must be zero"));
  }
  validate_exact_capabilities(&body[4..36])?;
  if i64_at(body, 36)? <= 0 {
    return Err(identity_error("void_catalog_timestamp", "Void catalog publication time is invalid"));
  }
  let free_root = &body[44..44 + hash_width];
  let claim_root = &body[44 + hash_width..44 + 2 * hash_width];
  let next_page_id = u64_at(body, 44 + 2 * hash_width)?;
  let free_count = u64_at(body, 52 + 2 * hash_width)?;
  let free_bytes = u64_at(body, 60 + 2 * hash_width)?;
  let claim_count = u64_at(body, 68 + 2 * hash_width)?;
  let claimed_bytes = u64_at(body, 76 + 2 * hash_width)?;
  let previous_control_sequence = u64_at(body, 84 + 2 * hash_width)?;
  let populated = free_count.checked_add(claim_count).ok_or_else(|| overflow_error("Void catalog count total"))? > 0;
  if (free_count == 0) != (free_bytes == 0 && all_zero(free_root))
    || (claim_count == 0) != (claimed_bytes == 0 && all_zero(claim_root))
    || populated && next_page_id == 0
    || artifact.generation == 1 && previous_control_sequence != 0
    || artifact.generation > 1 && previous_control_sequence == 0
  {
    return Err(closure_error("void_catalog_fields", "Void catalog roots, counts, page ID, or previous control are invalid"));
  }
  Ok(VoidCatalogManifestV1 {
    database_id,
    generation: artifact.generation,
    free_root,
    claim_root,
    free_count,
    free_bytes,
    claim_count,
    claimed_bytes,
    key,
  })
}

fn decode_void_claim(
  artifact: GcArtifactEnvelopeV1<'_>,
  complete_length: usize,
  algorithm: HashAlgorithm,
  key: Vec<u8>,
) -> FormatResult<VoidClaimV1<'_>> {
  let hash_width = algorithm.hash_length();
  if artifact.identity.len() != 32 || artifact.body.len() < 56 + hash_width {
    return Err(closure_error("void_claim_shape", "Void claim identity or body shape is invalid"));
  }
  let database_id = &artifact.identity[..16];
  let claim_id = &artifact.identity[16..];
  if all_zero(database_id) || all_zero(claim_id) {
    return Err(identity_error("void_claim_identity", "Void claim database and claim IDs must be nonzero"));
  }
  let body = artifact.body;
  if u32_at(body, 0)? != 0 || u16_at(body, 6)? != 0 {
    return Err(reserved_error("void_claim_reserved", "Void claim flags/reserve must be zero"));
  }
  if u16_at(body, 4)? != 1 {
    return Err(kind_error("void_claim_codec", "Void claim codec is not 1"));
  }
  if i64_at(body, 8)? <= 0 || all_zero(&body[16..32]) || all_zero(&body[32..48]) || all_zero(&body[48..48 + hash_width]) {
    return Err(identity_error("void_claim_fields", "Void claim time, requester IDs, or source manifest is invalid"));
  }
  let count = u32_at(body, 48 + hash_width)?;
  if count == 0 || count as usize > MAX_CANDIDATES {
    return Err(amplification_error("void_claim_count", count as usize, MAX_CANDIDATES));
  }
  let record_length = 16 + hash_width;
  let expected_records = checked_mul(count as usize, record_length, "Void claim extents")?;
  let records_length = usize::try_from(u32_at(body, 52 + hash_width)?).map_err(|_| overflow_error("Void claim extent length"))?;
  if records_length != expected_records || checked_add(56 + hash_width, records_length, "Void claim body")? != body.len() {
    return Err(trailing_error("void_claim_length", "Void claim extent lengths do not close"));
  }
  let extents = &body[56 + hash_width..];
  let mut total_bytes = 0u64;
  let mut previous_end = None;
  for record in extents.chunks_exact(record_length) {
    let offset = u64_at(record, 0)?;
    let length = u32_at(record, 8)?;
    if u32_at(record, 12)? != 0 {
      return Err(reserved_error("void_claim_extent_reserved", "Void claim extent reserve is nonzero"));
    }
    if offset < 2_048
      || length == 0
      || all_zero(&record[16..])
      || offset.checked_add(u64::from(length)).is_none()
      || previous_end.is_some_and(|end| end > offset)
    {
      return Err(order_error("void_claim_extent", "Void claim extents are invalid, overlapping, or out of order"));
    }
    previous_end = Some(offset + u64::from(length));
    total_bytes = total_bytes.checked_add(u64::from(length)).ok_or_else(|| overflow_error("Void claim byte total"))?;
  }
  Ok(VoidClaimV1 {
    hash_algorithm: algorithm,
    database_id,
    claim_id,
    generation: artifact.generation,
    source_manifest_hash: &body[48..48 + hash_width],
    extent_count: count,
    total_bytes,
    extents,
    stored_length: u64::try_from(complete_length).map_err(|_| overflow_error("Void claim stored length"))?,
    key,
  })
}

fn decode_void_claim_settlement(
  artifact: GcArtifactEnvelopeV1<'_>,
  algorithm: HashAlgorithm,
  key: Vec<u8>,
) -> FormatResult<VoidClaimSettlementV1<'_>> {
  let hash_width = algorithm.hash_length();
  if artifact.identity.len() != 32 || artifact.body.len() != 40 + 3 * hash_width {
    return Err(closure_error("void_settlement_shape", "Void settlement identity or body shape is invalid"));
  }
  let database_id = &artifact.identity[..16];
  let claim_id = &artifact.identity[16..];
  if all_zero(database_id) || all_zero(claim_id) {
    return Err(identity_error("void_settlement_identity", "Void settlement database and claim IDs must be nonzero"));
  }
  let body = artifact.body;
  let flags = u32_at(body, 0)?;
  let outcome = VoidClaimSettlementOutcomeV1::from_u16(u16_at(body, 4)?)
    .ok_or_else(|| kind_error("void_settlement_outcome", "unknown Void settlement outcome"))?;
  let recovered = flags & 1 != 0;
  if flags & !1 != 0 || u16_at(body, 6)? != 0 {
    return Err(reserved_error("void_settlement_reserved", "Void settlement flags/reserve are invalid"));
  }
  let source_manifest_hash = &body[16..16 + hash_width];
  let result_manifest_hash = &body[16 + hash_width..16 + 2 * hash_width];
  let used_count = u32_at(body, 16 + 2 * hash_width)?;
  let unused_count = u32_at(body, 20 + 2 * hash_width)?;
  let used_bytes = u64_at(body, 24 + 2 * hash_width)?;
  let returned_bytes = u64_at(body, 32 + 2 * hash_width)?;
  if recovered != (outcome == VoidClaimSettlementOutcomeV1::Recovered)
    || i64_at(body, 8)? <= 0
    || all_zero(source_manifest_hash)
    || all_zero(result_manifest_hash)
    || source_manifest_hash == result_manifest_hash
    || all_zero(&body[40 + 2 * hash_width..])
    || (used_count == 0) != (used_bytes == 0)
    || outcome == VoidClaimSettlementOutcomeV1::Settled && (used_count == 0 || used_bytes == 0)
    || outcome == VoidClaimSettlementOutcomeV1::AbandonedToQuarantine
      && (used_count != 0 || unused_count != 0 || used_bytes != 0 || returned_bytes != 0)
    || (unused_count == 0) != (returned_bytes == 0)
  {
    return Err(closure_error("void_settlement_fields", "Void settlement outcome, roots, counts, or evidence are invalid"));
  }
  Ok(VoidClaimSettlementV1 {
    database_id,
    claim_id,
    generation: artifact.generation,
    recovered,
    outcome,
    source_manifest_hash,
    result_manifest_hash,
    used_count,
    unused_count,
    used_bytes,
    returned_bytes,
    key,
  })
}

pub fn validate_void_directory_child(directory: &SweepVoidArtifactV1<'_>, child: &SweepVoidArtifactV1<'_>) -> FormatResult<()> {
  let SweepVoidArtifactV1::VoidDirectory(directory) = directory else {
    return Err(kind_error("void_directory_closure", "parent is not a Void directory"));
  };
  let valid = match (directory.role, child) {
    (VoidDirectoryRoleV1::FreeExtents, SweepVoidArtifactV1::VoidExtentPage(page)) => {
      directory.database_id == page.database_id
        && directory.catalog_id == page.catalog_id
        && directory.child_generation == page.generation
        && directory.page_id == page.page_id
        && directory.live_count == u64::from(page.record_count)
        && directory.logical_bytes == page.total_bytes
        && directory.lower_fence == page.lower_offset.to_le_bytes()
        && directory.upper_fence == page.upper_offset.to_le_bytes()
        && directory.child_hash == page.key
    }
    (VoidDirectoryRoleV1::Claims, SweepVoidArtifactV1::VoidClaim(claim)) => {
      directory.database_id == claim.database_id
        && directory.child_generation == claim.generation
        && directory.page_id == 0
        && directory.live_count == 1
        && directory.logical_bytes == claim.stored_length
        && directory.lower_fence == claim.claim_id
        && directory.upper_fence == claim.claim_id
        && directory.child_hash == claim.key
    }
    _ => false,
  };
  if !valid {
    return Err(closure_error("void_directory_closure", "Void directory descriptor does not match its child"));
  }
  Ok(())
}

pub fn validate_void_manifest_root(manifest: &SweepVoidArtifactV1<'_>, directory: &SweepVoidArtifactV1<'_>) -> FormatResult<()> {
  let (SweepVoidArtifactV1::VoidCatalog(manifest), SweepVoidArtifactV1::VoidDirectory(directory)) = (manifest, directory) else {
    return Err(kind_error("void_manifest_root_closure", "closure requires a Void catalog and directory"));
  };
  let valid = manifest.database_id == directory.database_id
    && manifest.generation == directory.child_generation
    && match directory.role {
      VoidDirectoryRoleV1::FreeExtents => {
        manifest.free_root == directory.key && manifest.free_count == directory.live_count && manifest.free_bytes == directory.logical_bytes
      }
      VoidDirectoryRoleV1::Claims => {
        manifest.claim_root == directory.key && manifest.claim_count == directory.live_count && manifest.claimed_bytes > 0
      }
    };
  if !valid {
    return Err(closure_error("void_manifest_root_closure", "Void catalog root/count/bytes do not match directory"));
  }
  Ok(())
}

pub fn validate_void_claim_source(
  claim: &SweepVoidArtifactV1<'_>,
  source: &SweepVoidArtifactV1<'_>,
  source_page: &SweepVoidArtifactV1<'_>,
) -> FormatResult<()> {
  let (SweepVoidArtifactV1::VoidClaim(claim), SweepVoidArtifactV1::VoidCatalog(source), SweepVoidArtifactV1::VoidExtentPage(source_page)) =
    (claim, source, source_page)
  else {
    return Err(kind_error("void_claim_source_closure", "closure requires a Void claim, source catalog, and source extent page"));
  };
  let hash_width = claim.source_manifest_hash.len();
  let claim_record_length = 16 + hash_width;
  let source_record_length = 32 + 3 * hash_width;
  if claim.hash_algorithm != source_page.hash_algorithm {
    return Err(closure_error("void_claim_source_closure", "Void claim and source page use different hash algorithms"));
  }
  let mut every_extent_is_available = true;
  for claimed in claim.extents.chunks_exact(claim_record_length) {
    let claimed_offset = u64_at(claimed, 0)?;
    let claimed_length = u32_at(claimed, 8)?;
    let claimed_end = claimed_offset.checked_add(u64::from(claimed_length)).ok_or_else(|| overflow_error("claimed Void extent end"))?;
    let claimed_proposal = &claimed[16..];
    let mut found = false;
    for available in source_page.records.chunks_exact(source_record_length) {
      let extent = decode_void_extent(available, source_page.hash_algorithm)?;
      let available_end = extent.offset.checked_add(u64::from(extent.length)).ok_or_else(|| overflow_error("available Void extent end"))?;
      if claimed_offset >= extent.offset && claimed_end <= available_end && claimed_proposal == extent.proposal_hash {
        found = true;
        break;
      }
    }
    if !found {
      every_extent_is_available = false;
      break;
    }
  }
  if claim.database_id != source.database_id
    || claim.database_id != source_page.database_id
    || claim.source_manifest_hash != source.key
    || claim.generation != source.generation.checked_add(1).ok_or_else(|| overflow_error("Void claim generation"))?
    || source_page.generation != source.generation
    || u64::from(claim.extent_count) > source.free_count
    || claim.total_bytes > source.free_bytes
    || !every_extent_is_available
  {
    return Err(closure_error("void_claim_source_closure", "Void claim does not match or fit its source catalog"));
  }
  Ok(())
}

pub fn validate_sweep_receipt_closure(
  proposal: &SweepVoidArtifactV1<'_>,
  receipt: &SweepVoidArtifactV1<'_>,
  catalog: &SweepVoidArtifactV1<'_>,
) -> FormatResult<()> {
  let (SweepVoidArtifactV1::SweepProposal(proposal), SweepVoidArtifactV1::SweepReceipt(receipt), SweepVoidArtifactV1::VoidCatalog(catalog)) =
    (proposal, receipt, catalog)
  else {
    return Err(kind_error("sweep_receipt_closure", "closure requires proposal, receipt, and Void catalog"));
  };
  let hash_width = proposal.key.len();
  let candidate_length = 24 + 2 * hash_width;
  let outcome_length = 48 + 2 * hash_width;
  let candidates_match = proposal
    .candidates
    .chunks_exact(candidate_length)
    .zip(receipt.outcomes.chunks_exact(outcome_length))
    .all(|(candidate, outcome)| candidate == &outcome[..candidate_length]);
  if proposal.database_id != receipt.database_id
    || proposal.database_id != catalog.database_id
    || proposal.batch_id != receipt.batch_id
    || proposal.generation != receipt.generation
    || receipt.proposal_hash != proposal.key
    || receipt.void_catalog_hash != catalog.key
    || proposal.candidate_count != receipt.outcome_count
    || !candidates_match
  {
    return Err(closure_error("sweep_receipt_closure", "sweep receipt does not exactly cover its proposal/catalog"));
  }
  Ok(())
}

pub fn validate_void_settlement_closure(
  settlement: &SweepVoidArtifactV1<'_>,
  claim: &SweepVoidArtifactV1<'_>,
  source: &SweepVoidArtifactV1<'_>,
  result: &SweepVoidArtifactV1<'_>,
) -> FormatResult<()> {
  let (
    SweepVoidArtifactV1::VoidClaimSettlement(settlement),
    SweepVoidArtifactV1::VoidClaim(claim),
    SweepVoidArtifactV1::VoidCatalog(source),
    SweepVoidArtifactV1::VoidCatalog(result),
  ) = (settlement, claim, source, result)
  else {
    return Err(kind_error("void_settlement_closure", "closure requires settlement, claim, source, and result catalogs"));
  };
  if settlement.database_id != claim.database_id
    || settlement.database_id != source.database_id
    || settlement.database_id != result.database_id
    || settlement.claim_id != claim.claim_id
    || settlement.source_manifest_hash != source.key
    || settlement.result_manifest_hash != result.key
    || claim.generation != source.generation
    || settlement.generation != source.generation.checked_add(1).ok_or_else(|| overflow_error("Void settlement generation"))?
    || settlement.generation != result.generation
    || settlement.used_bytes.checked_add(settlement.returned_bytes).ok_or_else(|| overflow_error("Void settlement byte total"))?
      > claim.total_bytes
    || source.claim_count.checked_sub(1) != Some(result.claim_count)
    || source.claimed_bytes.checked_sub(claim.total_bytes) != Some(result.claimed_bytes)
    || result.free_count < source.free_count
    || source.free_count.checked_add(u64::from(settlement.unused_count)).is_none_or(|maximum| result.free_count > maximum)
    || source.free_bytes.checked_add(settlement.returned_bytes) != Some(result.free_bytes)
  {
    return Err(closure_error("void_settlement_closure", "Void settlement does not match its claim/catalog transition"));
  }
  Ok(())
}

fn validate_exact_capabilities(bytes: &[u8]) -> FormatResult<()> {
  let mut expected = [0u8; 32];
  for bit in VOID_CAPABILITY_BITS {
    expected[bit / 8] |= 1 << (bit % 8);
  }
  if bytes.len() != expected.len() {
    return Err(trailing_error("void_catalog_capabilities", "Void catalog capability vector has wrong width"));
  }
  if bytes.iter().zip(expected).any(|(actual, required)| actual & !required != 0) {
    return Err(error(
      MalformedInputClass::UnknownRequiredCapability,
      "void_catalog_unknown_capability",
      "Void catalog requires an unknown capability",
    ));
  }
  if bytes != expected {
    return Err(closure_error("void_catalog_capabilities", "Void catalog capabilities do not match the frozen set"));
  }
  Ok(())
}

fn i64_at(bytes: &[u8], offset: usize) -> FormatResult<i64> {
  let raw = bytes.get(offset..offset + 8).ok_or_else(|| trailing_error("sweep_void_truncated", format!("i64 at offset {offset}")))?;
  Ok(i64::from_le_bytes(raw.try_into().expect("checked sweep/Void i64 width")))
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
  bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
  bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn put_i64(bytes: &mut [u8], offset: usize, value: i64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
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
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "sweep_void_overflow", context)
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
