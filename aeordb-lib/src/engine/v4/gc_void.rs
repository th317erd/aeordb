use std::cmp::Ordering;

use super::contract_generated::capability_bit;
use super::gc::{
  EncodedImmutableGcArtifactV1, GcArtifactEnvelopeV1, GcArtifactKindV1, ImmutableGcArtifactWriteV1, PhysicalIncarnationV1,
  compare_physical_incarnations_v1, decode_gc_artifact_envelope, decode_physical_incarnation, encode_immutable_gc_artifact,
  encode_physical_incarnation_into, immutable_gc_artifact_key, u16_at, u32_at, u64_at,
};
use super::hash::digest_parts;
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use super::gc_state::{GcDirectoryRoleV1, GcStateArtifactV1, GcStateDirectoryV1, decode_gc_state_artifact, validate_gc_directory_child};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoidExtentRecordV1<'a> {
  pub offset: u64,
  pub length: u32,
  pub origin_sweep_proposal_hash: &'a [u8],
  pub origin_quarantine_manifest_hash: &'a [u8],
  pub reclaimed_incarnation_digest: &'a [u8],
  pub reclaim_commit_sequence: u64,
  pub void_generation: u64,
}

#[derive(Debug)]
pub struct VoidExtentRecordsV1<'a> {
  rows: std::slice::ChunksExact<'a, u8>,
  algorithm: HashAlgorithm,
  previous_end: Option<u64>,
  failed: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct VoidExtentPageWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8; 16],
  pub catalog_id: &'a [u8; 16],
  pub generation: u64,
  pub page_id: u64,
  pub extents: &'a [VoidExtentRecordV1<'a>],
}

#[derive(Debug, Clone)]
pub struct VoidCatalogManifestV1<'a> {
  pub database_id: &'a [u8],
  pub generation: u64,
  pub published_at_ms: i64,
  pub free_root: &'a [u8],
  pub claim_root: &'a [u8],
  pub free_count: u64,
  pub free_bytes: u64,
  pub claim_count: u64,
  pub claimed_bytes: u64,
  pub next_page_id: u64,
  pub previous_control_sequence: u64,
  pub key: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct VoidCatalogManifestWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8; 16],
  pub generation: u64,
  pub published_at_ms: i64,
  pub free_root: Option<&'a [u8]>,
  pub claim_root: Option<&'a [u8]>,
  pub next_page_id: u64,
  pub free_count: u64,
  pub free_bytes: u64,
  pub claim_count: u64,
  pub claimed_bytes: u64,
  pub previous_control_sequence: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoidClaimExtentV1<'a> {
  pub offset: u64,
  pub length: u32,
  pub origin_sweep_proposal_hash: &'a [u8],
}

#[derive(Debug)]
pub struct VoidClaimExtentsV1<'a> {
  rows: std::slice::ChunksExact<'a, u8>,
  algorithm: HashAlgorithm,
  previous_end: Option<u64>,
  failed: bool,
}

#[derive(Debug, Clone)]
pub struct VoidClaimV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8],
  pub claim_id: &'a [u8],
  pub generation: u64,
  pub created_at_ms: i64,
  pub requesting_boot_id: &'a [u8],
  pub requesting_task_or_batch_id: &'a [u8],
  pub source_manifest_hash: &'a [u8],
  pub extent_count: u32,
  pub total_bytes: u64,
  pub extents: &'a [u8],
  pub stored_length: u64,
  pub key: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct VoidClaimWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8; 16],
  pub claim_id: &'a [u8; 16],
  pub generation: u64,
  pub created_at_ms: i64,
  pub requesting_boot_id: &'a [u8; 16],
  pub requesting_task_or_batch_id: &'a [u8; 16],
  pub source_manifest_hash: &'a [u8],
  pub extents: &'a [VoidClaimExtentV1<'a>],
}

#[derive(Debug, Clone)]
pub struct VoidClaimSettlementV1<'a> {
  pub database_id: &'a [u8],
  pub claim_id: &'a [u8],
  pub generation: u64,
  pub settled_at_ms: i64,
  pub recovered: bool,
  pub outcome: VoidClaimSettlementOutcomeV1,
  pub source_manifest_hash: &'a [u8],
  pub result_manifest_hash: &'a [u8],
  pub used_count: u32,
  pub unused_count: u32,
  pub used_bytes: u64,
  pub returned_bytes: u64,
  pub evidence_digest: &'a [u8],
  pub key: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct VoidClaimSettlementWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8; 16],
  pub claim_id: &'a [u8; 16],
  pub generation: u64,
  pub outcome: VoidClaimSettlementOutcomeV1,
  pub settled_at_ms: i64,
  pub source_manifest_hash: &'a [u8],
  pub result_manifest_hash: &'a [u8],
  pub used_count: u32,
  pub unused_count: u32,
  pub used_bytes: u64,
  pub returned_bytes: u64,
  pub evidence_digest: &'a [u8],
}

impl<'a> VoidExtentPageV1<'a> {
  pub fn extent_records(&self) -> FormatResult<VoidExtentRecordsV1<'a>> {
    let row_length = 32 + 3 * self.hash_algorithm.hash_length();
    let maximum_records = (MAX_PAGE_LENGTH.saturating_sub(80)) / row_length;
    if self.record_count == 0
      || self.record_count as usize > maximum_records
      || (self.record_count as usize).checked_mul(row_length) != Some(self.records.len())
    {
      return Err(closure_error("void_extent_records", "Void extent records do not match their declared shape"));
    }
    Ok(VoidExtentRecordsV1 {
      rows: self.records.chunks_exact(row_length),
      algorithm: self.hash_algorithm,
      previous_end: None,
      failed: false,
    })
  }
}

impl<'a> Iterator for VoidExtentRecordsV1<'a> {
  type Item = FormatResult<VoidExtentRecordV1<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.failed {
      return None;
    }
    let row = self.rows.next()?;
    let extent = match decode_void_extent_record_v1(row, self.algorithm) {
      Ok(extent) => extent,
      Err(error) => {
        self.failed = true;
        return Some(Err(error));
      }
    };
    if self.previous_end.is_some_and(|end| end > extent.offset) {
      self.failed = true;
      return Some(Err(order_error("void_extent_page_order", "Void extents overlap or are out of order")));
    }
    self.previous_end = extent.offset.checked_add(u64::from(extent.length));
    Some(Ok(extent))
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    if self.failed {
      (0, Some(0))
    } else {
      self.rows.size_hint()
    }
  }
}

impl<'a> VoidClaimV1<'a> {
  pub fn extent_records(&self) -> FormatResult<VoidClaimExtentsV1<'a>> {
    let row_length = 16 + self.hash_algorithm.hash_length();
    if self.extent_count == 0
      || self.extent_count as usize > MAX_CANDIDATES
      || (self.extent_count as usize).checked_mul(row_length) != Some(self.extents.len())
    {
      return Err(closure_error("void_claim_records", "Void claim extents do not match their declared shape"));
    }
    Ok(VoidClaimExtentsV1 {
      rows: self.extents.chunks_exact(row_length),
      algorithm: self.hash_algorithm,
      previous_end: None,
      failed: false,
    })
  }
}

impl<'a> Iterator for VoidClaimExtentsV1<'a> {
  type Item = FormatResult<VoidClaimExtentV1<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.failed {
      return None;
    }
    let row = self.rows.next()?;
    let extent = match decode_void_claim_extent_v1(row, self.algorithm) {
      Ok(extent) => extent,
      Err(error) => {
        self.failed = true;
        return Some(Err(error));
      }
    };
    if self.previous_end.is_some_and(|end| end > extent.offset) {
      self.failed = true;
      return Some(Err(order_error("void_claim_extent", "Void claim extents overlap or are out of order")));
    }
    self.previous_end = extent.offset.checked_add(u64::from(extent.length));
    Some(Ok(extent))
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    if self.failed {
      (0, Some(0))
    } else {
      self.rows.size_hint()
    }
  }
}

#[derive(Debug, Clone)]
pub enum SweepVoidArtifactV1<'a> {
  SweepProposal(SweepProposalV1<'a>),
  SweepReceipt(SweepReceiptV1<'a>),
  VoidExtentPage(VoidExtentPageV1<'a>),
  VoidDirectory(GcStateDirectoryV1<'a>),
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
      Self::VoidDirectory(value) => format!("gc:directory:{}:records={}", value.role.directory_name(), value.live_count),
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
    GcArtifactKindV1::GcArtifactDirectoryNode => {
      let GcStateArtifactV1::Directory(directory) = decode_gc_state_artifact(bytes, algorithm)? else {
        return Err(kind_error("void_directory_kind", "shared GC directory decoder returned another artifact kind"));
      };
      if !matches!(directory.role, GcDirectoryRoleV1::FreeExtents | GcDirectoryRoleV1::Claims) {
        return Err(kind_error("void_directory_role", "artifact directory role is not a Void role"));
      }
      Ok(SweepVoidArtifactV1::VoidDirectory(directory))
    }
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

pub fn encode_void_extent_page_v1(request: &VoidExtentPageWriteV1<'_>) -> FormatResult<EncodedImmutableGcArtifactV1> {
  if request.database_id.iter().all(|byte| *byte == 0)
    || request.catalog_id.iter().all(|byte| *byte == 0)
    || request.generation == 0
    || request.page_id == 0
    || request.extents.is_empty()
  {
    return Err(identity_error("void_extent_page_write", "Void extent page identity, generation, page ID, or count is invalid"));
  }
  let hash_width = request.hash_algorithm.hash_length();
  let row_length = 32 + 3 * hash_width;
  let records_length = checked_mul(request.extents.len(), row_length, "Void extent page records")?;
  let body_length = checked_add(80, records_length, "Void extent page body")?;
  ensure_cap("void_extent_page_write", body_length, MAX_PAGE_LENGTH)?;
  let mut records = vec![0u8; records_length];
  let mut total_bytes = 0u64;
  let mut previous_end = None;
  for (index, extent) in request.extents.iter().enumerate() {
    let start = index * row_length;
    encode_void_extent_record_into(&mut records[start..start + row_length], extent, request.hash_algorithm)?;
    let decoded = decode_void_extent_record_v1(&records[start..start + row_length], request.hash_algorithm)?;
    if previous_end.is_some_and(|end| end > decoded.offset) {
      return Err(order_error("void_extent_page_order", "Void extents overlap or are out of order"));
    }
    previous_end = decoded.offset.checked_add(u64::from(decoded.length));
    total_bytes = total_bytes.checked_add(u64::from(decoded.length)).ok_or_else(|| overflow_error("Void extent byte total"))?;
  }
  let lower_offset = request.extents[0].offset;
  let upper_offset = request.extents[request.extents.len() - 1].offset;
  let count = usize_to_u32(request.extents.len(), "Void extent page count")?;
  let mut body = vec![0u8; body_length];
  put_u16(&mut body, 4, 1);
  put_u16(&mut body, 6, GcDirectoryRoleV1::FreeExtents as u16);
  put_u32(&mut body, 8, 8);
  put_u32(&mut body, 12, 8);
  put_u32(&mut body, 16, count);
  put_u32(&mut body, 20, count);
  put_u64(&mut body, 24, usize_to_u64(records_length));
  put_u64(&mut body, 32, total_bytes);
  put_u64(&mut body, 64, lower_offset);
  put_u64(&mut body, 72, upper_offset);
  body[80..].copy_from_slice(&records);
  let mut identity = Vec::with_capacity(42);
  identity.extend_from_slice(request.database_id);
  identity.extend_from_slice(request.catalog_id);
  identity.extend_from_slice(&(GcDirectoryRoleV1::FreeExtents as u16).to_le_bytes());
  identity.extend_from_slice(&request.page_id.to_le_bytes());
  let encoded = encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 {
    kind: GcArtifactKindV1::VoidExtentPage,
    hash_algorithm: request.hash_algorithm,
    generation: request.generation,
    identity: &identity,
    body: &body,
  })?;
  let SweepVoidArtifactV1::VoidExtentPage(decoded) = decode_sweep_void_artifact(&encoded.value, request.hash_algorithm)? else {
    return Err(closure_error("void_extent_page_write", "encoded Void extent page decoded as another artifact kind"));
  };
  if decoded.key != encoded.key || decoded.record_count != count || decoded.total_bytes != total_bytes {
    return Err(closure_error("void_extent_page_write", "encoded Void extent page disagrees with its request"));
  }
  Ok(encoded)
}

fn encode_void_extent_record_into(target: &mut [u8], extent: &VoidExtentRecordV1<'_>, algorithm: HashAlgorithm) -> FormatResult<()> {
  let hash_width = algorithm.hash_length();
  if target.len() != 32 + 3 * hash_width
    || extent.origin_sweep_proposal_hash.len() != hash_width
    || extent.origin_quarantine_manifest_hash.len() != hash_width
    || extent.reclaimed_incarnation_digest.len() != hash_width
  {
    return Err(closure_error("void_extent_write", "Void extent hashes or destination have the wrong width"));
  }
  put_u64(target, 0, extent.offset);
  put_u32(target, 8, extent.length);
  target[16..16 + hash_width].copy_from_slice(extent.origin_sweep_proposal_hash);
  target[16 + hash_width..16 + 2 * hash_width].copy_from_slice(extent.origin_quarantine_manifest_hash);
  target[16 + 2 * hash_width..16 + 3 * hash_width].copy_from_slice(extent.reclaimed_incarnation_digest);
  put_u64(target, 16 + 3 * hash_width, extent.reclaim_commit_sequence);
  put_u64(target, 24 + 3 * hash_width, extent.void_generation);
  decode_void_extent_record_v1(target, algorithm).map(|_| ())
}

pub fn encode_void_catalog_manifest_v1(request: &VoidCatalogManifestWriteV1<'_>) -> FormatResult<EncodedImmutableGcArtifactV1> {
  let hash_width = request.hash_algorithm.hash_length();
  let root_is_valid = |root: Option<&[u8]>, count: u64, bytes: u64| match root {
    Some(root) => root.len() == hash_width && !all_zero(root) && count > 0 && bytes > 0,
    None => count == 0 && bytes == 0,
  };
  if request.database_id.iter().all(|byte| *byte == 0)
    || request.generation == 0
    || request.published_at_ms <= 0
    || request.next_page_id == 0
    || !root_is_valid(request.free_root, request.free_count, request.free_bytes)
    || !root_is_valid(request.claim_root, request.claim_count, request.claimed_bytes)
    || (request.generation == 1) != (request.previous_control_sequence == 0)
  {
    return Err(closure_error("void_catalog_write", "Void catalog identity, roots, counts, or prior control are invalid"));
  }
  let mut body = vec![0u8; 92 + 2 * hash_width];
  write_exact_capabilities(&mut body[4..36]);
  put_i64(&mut body, 36, request.published_at_ms);
  if let Some(root) = request.free_root {
    body[44..44 + hash_width].copy_from_slice(root);
  }
  if let Some(root) = request.claim_root {
    body[44 + hash_width..44 + 2 * hash_width].copy_from_slice(root);
  }
  put_u64(&mut body, 44 + 2 * hash_width, request.next_page_id);
  put_u64(&mut body, 52 + 2 * hash_width, request.free_count);
  put_u64(&mut body, 60 + 2 * hash_width, request.free_bytes);
  put_u64(&mut body, 68 + 2 * hash_width, request.claim_count);
  put_u64(&mut body, 76 + 2 * hash_width, request.claimed_bytes);
  put_u64(&mut body, 84 + 2 * hash_width, request.previous_control_sequence);
  let mut identity = Vec::with_capacity(24);
  identity.extend_from_slice(request.database_id);
  identity.extend_from_slice(&request.generation.to_le_bytes());
  let encoded = encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 {
    kind: GcArtifactKindV1::VoidCatalogManifest,
    hash_algorithm: request.hash_algorithm,
    generation: request.generation,
    identity: &identity,
    body: &body,
  })?;
  let SweepVoidArtifactV1::VoidCatalog(decoded) = decode_sweep_void_artifact(&encoded.value, request.hash_algorithm)? else {
    return Err(closure_error("void_catalog_write", "encoded Void catalog decoded as another artifact kind"));
  };
  if decoded.key != encoded.key || decoded.next_page_id != request.next_page_id {
    return Err(closure_error("void_catalog_write", "encoded Void catalog disagrees with its request"));
  }
  Ok(encoded)
}

pub fn encode_void_claim_v1(request: &VoidClaimWriteV1<'_>) -> FormatResult<EncodedImmutableGcArtifactV1> {
  let hash_width = request.hash_algorithm.hash_length();
  if request.database_id.iter().all(|byte| *byte == 0)
    || request.claim_id.iter().all(|byte| *byte == 0)
    || request.requesting_boot_id.iter().all(|byte| *byte == 0)
    || request.requesting_task_or_batch_id.iter().all(|byte| *byte == 0)
    || request.generation == 0
    || request.created_at_ms <= 0
    || request.source_manifest_hash.len() != hash_width
    || all_zero(request.source_manifest_hash)
    || request.extents.is_empty()
    || request.extents.len() > MAX_CANDIDATES
  {
    return Err(identity_error("void_claim_write", "Void claim identity, requester, source, time, generation, or count is invalid"));
  }
  let row_length = 16 + hash_width;
  let records_length = checked_mul(request.extents.len(), row_length, "Void claim extents")?;
  let body_length = checked_add(56 + hash_width, records_length, "Void claim body")?;
  ensure_cap("void_claim_write", body_length, MAX_SWEEP_LENGTH)?;
  let mut records = vec![0u8; records_length];
  let mut previous_end = None;
  for (index, extent) in request.extents.iter().enumerate() {
    let start = index * row_length;
    let row = &mut records[start..start + row_length];
    if extent.origin_sweep_proposal_hash.len() != hash_width {
      return Err(closure_error("void_claim_extent_write", "Void claim proposal hash has the wrong width"));
    }
    put_u64(row, 0, extent.offset);
    put_u32(row, 8, extent.length);
    row[16..].copy_from_slice(extent.origin_sweep_proposal_hash);
    let decoded = decode_void_claim_extent_v1(row, request.hash_algorithm)?;
    if previous_end.is_some_and(|end| end > decoded.offset) {
      return Err(order_error("void_claim_extent", "Void claim extents overlap or are out of order"));
    }
    previous_end = decoded.offset.checked_add(u64::from(decoded.length));
  }
  let count = usize_to_u32(request.extents.len(), "Void claim count")?;
  let mut body = vec![0u8; body_length];
  put_u16(&mut body, 4, 1);
  put_i64(&mut body, 8, request.created_at_ms);
  body[16..32].copy_from_slice(request.requesting_boot_id);
  body[32..48].copy_from_slice(request.requesting_task_or_batch_id);
  body[48..48 + hash_width].copy_from_slice(request.source_manifest_hash);
  put_u32(&mut body, 48 + hash_width, count);
  put_u32(&mut body, 52 + hash_width, usize_to_u32(records_length, "Void claim extents length")?);
  body[56 + hash_width..].copy_from_slice(&records);
  let mut identity = [0u8; 32];
  identity[..16].copy_from_slice(request.database_id);
  identity[16..].copy_from_slice(request.claim_id);
  let encoded = encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 {
    kind: GcArtifactKindV1::VoidClaim,
    hash_algorithm: request.hash_algorithm,
    generation: request.generation,
    identity: &identity,
    body: &body,
  })?;
  let SweepVoidArtifactV1::VoidClaim(decoded) = decode_sweep_void_artifact(&encoded.value, request.hash_algorithm)? else {
    return Err(closure_error("void_claim_write", "encoded Void claim decoded as another artifact kind"));
  };
  if decoded.key != encoded.key || decoded.extent_count != count {
    return Err(closure_error("void_claim_write", "encoded Void claim disagrees with its request"));
  }
  Ok(encoded)
}

pub fn encode_void_claim_settlement_v1(request: &VoidClaimSettlementWriteV1<'_>) -> FormatResult<EncodedImmutableGcArtifactV1> {
  let hash_width = request.hash_algorithm.hash_length();
  if request.database_id.iter().all(|byte| *byte == 0)
    || request.claim_id.iter().all(|byte| *byte == 0)
    || request.generation == 0
    || request.settled_at_ms <= 0
    || request.source_manifest_hash.len() != hash_width
    || request.result_manifest_hash.len() != hash_width
    || request.evidence_digest.len() != hash_width
    || all_zero(request.source_manifest_hash)
    || all_zero(request.result_manifest_hash)
    || all_zero(request.evidence_digest)
  {
    return Err(identity_error("void_settlement_write", "Void settlement identity, time, generation, or hashes are invalid"));
  }
  let mut body = vec![0u8; 40 + 3 * hash_width];
  put_u32(&mut body, 0, u32::from(request.outcome == VoidClaimSettlementOutcomeV1::Recovered));
  put_u16(&mut body, 4, request.outcome as u16);
  put_i64(&mut body, 8, request.settled_at_ms);
  body[16..16 + hash_width].copy_from_slice(request.source_manifest_hash);
  body[16 + hash_width..16 + 2 * hash_width].copy_from_slice(request.result_manifest_hash);
  put_u32(&mut body, 16 + 2 * hash_width, request.used_count);
  put_u32(&mut body, 20 + 2 * hash_width, request.unused_count);
  put_u64(&mut body, 24 + 2 * hash_width, request.used_bytes);
  put_u64(&mut body, 32 + 2 * hash_width, request.returned_bytes);
  body[40 + 2 * hash_width..].copy_from_slice(request.evidence_digest);
  let mut identity = [0u8; 32];
  identity[..16].copy_from_slice(request.database_id);
  identity[16..].copy_from_slice(request.claim_id);
  let encoded = encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 {
    kind: GcArtifactKindV1::VoidClaimSettlementReceipt,
    hash_algorithm: request.hash_algorithm,
    generation: request.generation,
    identity: &identity,
    body: &body,
  })?;
  let SweepVoidArtifactV1::VoidClaimSettlement(decoded) = decode_sweep_void_artifact(&encoded.value, request.hash_algorithm)? else {
    return Err(closure_error("void_settlement_write", "encoded Void settlement decoded as another artifact kind"));
  };
  if decoded.key != encoded.key || decoded.outcome != request.outcome {
    return Err(closure_error("void_settlement_write", "encoded Void settlement disagrees with its request"));
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
    || u16_at(artifact.identity, 32)? != GcDirectoryRoleV1::FreeExtents as u16
    || page_id == 0
  {
    return Err(identity_error("void_extent_page_identity", "Void extent page identity is invalid"));
  }
  let body = artifact.body;
  if u32_at(body, 0)? != 0 || u16_at(body, 6)? != GcDirectoryRoleV1::FreeExtents as u16 || body[40..64].iter().any(|byte| *byte != 0) {
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
    let extent = decode_void_extent_record_v1(row, algorithm)?;
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

pub fn decode_void_extent_record_v1(row: &[u8], algorithm: HashAlgorithm) -> FormatResult<VoidExtentRecordV1<'_>> {
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
  Ok(VoidExtentRecordV1 {
    offset,
    length,
    origin_sweep_proposal_hash: &row[16..16 + hash_width],
    origin_quarantine_manifest_hash: &row[16 + hash_width..16 + 2 * hash_width],
    reclaimed_incarnation_digest: &row[16 + 2 * hash_width..16 + 3 * hash_width],
    reclaim_commit_sequence: u64_at(row, 16 + 3 * hash_width)?,
    void_generation: u64_at(row, 24 + 3 * hash_width)?,
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
    published_at_ms: i64_at(body, 36)?,
    free_root,
    claim_root,
    free_count,
    free_bytes,
    claim_count,
    claimed_bytes,
    next_page_id,
    previous_control_sequence,
    key,
  })
}

pub fn decode_void_claim_extent_v1(row: &[u8], algorithm: HashAlgorithm) -> FormatResult<VoidClaimExtentV1<'_>> {
  let hash_width = algorithm.hash_length();
  if row.len() != 16 + hash_width {
    return Err(trailing_error("void_claim_extent_length", "Void claim extent row has wrong fixed length"));
  }
  let offset = u64_at(row, 0)?;
  let length = u32_at(row, 8)?;
  if u32_at(row, 12)? != 0 {
    return Err(reserved_error("void_claim_extent_reserved", "Void claim extent reserve is nonzero"));
  }
  if offset < 2_048 || length == 0 || offset.checked_add(u64::from(length)).is_none() || all_zero(&row[16..]) {
    return Err(identity_error("void_claim_extent", "Void claim extent range or origin is invalid"));
  }
  Ok(VoidClaimExtentV1 { offset, length, origin_sweep_proposal_hash: &row[16..] })
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
  let mut claim = VoidClaimV1 {
    hash_algorithm: algorithm,
    database_id,
    claim_id,
    generation: artifact.generation,
    created_at_ms: i64_at(body, 8)?,
    requesting_boot_id: &body[16..32],
    requesting_task_or_batch_id: &body[32..48],
    source_manifest_hash: &body[48..48 + hash_width],
    extent_count: count,
    total_bytes: 0,
    extents,
    stored_length: u64::try_from(complete_length).map_err(|_| overflow_error("Void claim stored length"))?,
    key,
  };
  let mut total_bytes = 0u64;
  for extent in claim.extent_records()? {
    let extent = extent?;
    total_bytes = total_bytes.checked_add(u64::from(extent.length)).ok_or_else(|| overflow_error("Void claim byte total"))?;
  }
  claim.total_bytes = total_bytes;
  Ok(claim)
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
    settled_at_ms: i64_at(body, 8)?,
    recovered,
    outcome,
    source_manifest_hash,
    result_manifest_hash,
    used_count,
    unused_count,
    used_bytes,
    returned_bytes,
    evidence_digest: &body[40 + 2 * hash_width..],
    key,
  })
}

pub fn validate_void_directory_child(directory: &SweepVoidArtifactV1<'_>, child: &SweepVoidArtifactV1<'_>) -> FormatResult<()> {
  let SweepVoidArtifactV1::VoidDirectory(directory) = directory else {
    return Err(kind_error("void_directory_closure", "parent is not a Void directory"));
  };
  if let SweepVoidArtifactV1::VoidDirectory(child) = child {
    return validate_gc_directory_child(directory, child);
  }
  let descriptor = directory.entries.iter().find(|entry| entry.child_hash == child.key());
  let valid = descriptor.is_some_and(|descriptor| match (directory.role, child) {
    (GcDirectoryRoleV1::FreeExtents, SweepVoidArtifactV1::VoidExtentPage(page)) => {
      directory.level == 0
        && directory.database_id == page.database_id
        && directory.catalog_id == page.catalog_id
        && descriptor.child_generation == page.generation
        && descriptor.minimum_page_id == page.page_id
        && descriptor.maximum_page_id == page.page_id
        && descriptor.live_count == u64::from(page.record_count)
        && descriptor.tombstone_count == 0
        && descriptor.page_count == 1
        && descriptor.logical_bytes == page.total_bytes
        && descriptor.lower_fence == page.lower_offset.to_le_bytes()
        && descriptor.upper_fence == page.upper_offset.to_le_bytes()
    }
    (GcDirectoryRoleV1::Claims, SweepVoidArtifactV1::VoidClaim(claim)) => {
      directory.level == 0
        && directory.database_id == claim.database_id
        && descriptor.child_generation == claim.generation
        && descriptor.minimum_page_id == 0
        && descriptor.maximum_page_id == 0
        && descriptor.live_count == 1
        && descriptor.tombstone_count == 0
        && descriptor.page_count == 0
        && descriptor.logical_bytes == claim.stored_length
        && descriptor.lower_fence == claim.claim_id
        && descriptor.upper_fence == claim.claim_id
    }
    _ => false,
  });
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
    && directory.generation >= manifest.generation
    && (directory.level != 0 || directory.entries.iter().all(|entry| entry.child_generation == manifest.generation))
    && match directory.role {
      GcDirectoryRoleV1::FreeExtents => {
        manifest.free_root == directory.key && manifest.free_count == directory.live_count && manifest.free_bytes == directory.logical_bytes
      }
      GcDirectoryRoleV1::Claims => {
        manifest.claim_root == directory.key && manifest.claim_count == directory.live_count && manifest.claimed_bytes > 0
      }
      _ => false,
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
  if claim.hash_algorithm != source_page.hash_algorithm {
    return Err(closure_error("void_claim_source_closure", "Void claim and source page use different hash algorithms"));
  }
  let mut every_extent_is_available = true;
  for claimed in claim.extent_records()? {
    let claimed = claimed?;
    let claimed_end = claimed.offset.checked_add(u64::from(claimed.length)).ok_or_else(|| overflow_error("claimed Void extent end"))?;
    let mut found = false;
    for available in source_page.extent_records()? {
      let extent = available?;
      let available_end = extent.offset.checked_add(u64::from(extent.length)).ok_or_else(|| overflow_error("available Void extent end"))?;
      if claimed.offset >= extent.offset
        && claimed_end <= available_end
        && claimed.origin_sweep_proposal_hash == extent.origin_sweep_proposal_hash
      {
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

fn write_exact_capabilities(bytes: &mut [u8]) {
  debug_assert_eq!(bytes.len(), 32);
  for bit in VOID_CAPABILITY_BITS {
    bytes[bit / 8] |= 1 << (bit % 8);
  }
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

fn usize_to_u32(value: usize, context: &'static str) -> FormatResult<u32> {
  if value > u32::MAX as usize {
    return Err(overflow_error(context));
  }
  Ok(value as u32)
}

fn usize_to_u64(value: usize) -> u64 {
  value as u64
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
