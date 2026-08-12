//! Bounded qualification for immutable Void claims.
//!
//! This module proves the semantic source-to-result catalog transition. It
//! does not publish either catalog and cannot grant reusable-space authority.

use std::num::TryFromIntError;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::first_authority::{FirstAuthorityPublicationErrorV1, VoidClaimAdmissionPermitV1};
use super::gc_retirement::{RetirementJournalOwnerErrorV1, RetirementJournalReplacementAdmissionErrorV1};
use super::gc_void::{SweepVoidArtifactV1, VoidCatalogManifestV1, VoidClaimV1, VoidExtentRecordV1, decode_sweep_void_artifact};
use super::gc_void_publication::{
  VoidCatalogClosureErrorV1, VoidCatalogClosureLimitsV1, VoidCatalogClosureSummaryV1, VoidCatalogClosureValidatorV1,
};
use super::hash::digest_parts;
use super::reader::FormatError;
use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

const FREE_DIGEST_DOMAIN: &[u8] = b"aeordb.void-claim.free-transition.v1\0";
const CLAIM_DIGEST_DOMAIN: &[u8] = b"aeordb.void-claim.claim-transition.v1\0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoidClaimTransitionLimitsV1 {
  pub maximum_support_artifacts_per_catalog: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoidClaimAdmittedExtentV1 {
  pub offset: u64,
  pub length: u32,
  pub reclaim_commit_sequence: u64,
  pub void_generation: u64,
  pub origin_sweep_proposal_hash: Vec<u8>,
  pub origin_quarantine_manifest_hash: Vec<u8>,
  pub reclaimed_incarnation_digest: Vec<u8>,
}

pub struct VoidClaimTransitionSummaryV1 {
  pub source_manifest_key: Vec<u8>,
  pub result_manifest_key: Vec<u8>,
  pub claim_key: Vec<u8>,
  pub claim_id: [u8; 16],
  pub claimed_extent_count: u32,
  pub claimed_bytes: u64,
  pub source_closure: VoidCatalogClosureSummaryV1,
  pub result_closure: VoidCatalogClosureSummaryV1,
  claimed_extents: Box<[VoidClaimAdmittedExtentV1]>,
  _memory: MemoryReservation,
}

impl std::fmt::Debug for VoidClaimTransitionSummaryV1 {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("VoidClaimTransitionSummaryV1")
      .field("source_manifest_key", &hex::encode(&self.source_manifest_key))
      .field("result_manifest_key", &hex::encode(&self.result_manifest_key))
      .field("claim_key", &hex::encode(&self.claim_key))
      .field("claimed_extent_count", &self.claimed_extent_count)
      .field("claimed_bytes", &self.claimed_bytes)
      .finish_non_exhaustive()
  }
}

impl VoidClaimTransitionSummaryV1 {
  pub fn claimed_extents(&self) -> &[VoidClaimAdmittedExtentV1] {
    &self.claimed_extents
  }

  pub(crate) fn into_claimed_extents_with_memory(self) -> (Box<[VoidClaimAdmittedExtentV1]>, MemoryReservation) {
    (self.claimed_extents, self._memory)
  }
}

#[derive(Debug, Error)]
pub enum VoidClaimTransitionErrorV1 {
  #[error("Void claim transition validation was canceled")]
  Canceled,
  #[error("Void claim transition validator is already failed")]
  Failed,
  #[error("{code}: {message}")]
  Invalid { code: &'static str, message: String },
  #[error("Void claim transition memory admission failed: {0}")]
  Memory(#[from] MemoryCoordinatorError),
  #[error("Void claim transition allocation failed: {0}")]
  Allocation(#[from] std::collections::TryReserveError),
  #[error("Void claim transition integer conversion failed: {0}")]
  IntegerConversion(#[from] TryFromIntError),
  #[error("Void claim transition closure failed: {0}")]
  Closure(#[from] VoidCatalogClosureErrorV1),
  #[error("Void claim transition format failed: {0}")]
  Format(#[from] FormatError),
}

impl VoidClaimTransitionErrorV1 {
  pub fn code(&self) -> &str {
    match self {
      Self::Canceled => "void_claim_transition_canceled",
      Self::Failed => "void_claim_transition_failed",
      Self::Invalid { code, .. } => code,
      Self::Memory(_) => "void_claim_transition_memory",
      Self::Allocation(_) => "void_claim_transition_allocation",
      Self::IntegerConversion(_) => "void_claim_transition_integer_conversion",
      Self::Closure(source) => source.code(),
      Self::Format(source) => source.code(),
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }
}

#[derive(Debug)]
struct DigestChainV1 {
  algorithm: HashAlgorithm,
  domain: &'static [u8],
  value: Vec<u8>,
  count: u64,
}

impl DigestChainV1 {
  fn new(algorithm: HashAlgorithm, domain: &'static [u8]) -> Self {
    Self { algorithm, domain, value: digest_parts(algorithm, &[domain]), count: 0 }
  }

  fn push(&mut self, parts: &[&[u8]]) -> Result<(), VoidClaimTransitionErrorV1> {
    let next_count = self
      .count
      .checked_add(1)
      .ok_or_else(|| VoidClaimTransitionErrorV1::invalid("void_claim_transition_digest_count", "semantic digest count overflowed"))?;
    if parts.len() > 7 {
      return Err(VoidClaimTransitionErrorV1::invalid(
        "void_claim_transition_digest_parts",
        "semantic digest row exceeds its fixed part bound",
      ));
    }
    let mut digest_parts_input = [&[][..]; 10];
    digest_parts_input[0] = self.domain;
    digest_parts_input[1] = self.value.as_slice();
    let count_bytes = next_count.to_le_bytes();
    digest_parts_input[2] = &count_bytes;
    digest_parts_input[3..3 + parts.len()].copy_from_slice(parts);
    self.value = digest_parts(self.algorithm, &digest_parts_input[..3 + parts.len()]);
    self.count = next_count;
    Ok(())
  }
}

pub struct VoidClaimTransitionValidatorV1<'a> {
  source_manifest: &'a VoidCatalogManifestV1<'a>,
  result_manifest: &'a VoidCatalogManifestV1<'a>,
  claim: &'a VoidClaimV1<'a>,
  cancellation: CancellationToken,
  source_validator: Option<VoidCatalogClosureValidatorV1<'a>>,
  result_validator: Option<VoidCatalogClosureValidatorV1<'a>>,
  claimed_extents: Vec<VoidClaimAdmittedExtentV1>,
  next_claimed_extent: usize,
  expected_free_digest: DigestChainV1,
  result_free_digest: DigestChainV1,
  expected_claim_digest: DigestChainV1,
  result_claim_digest: DigestChainV1,
  new_claim_inserted: bool,
  expected_free_count: u64,
  expected_free_bytes: u64,
  source_closure: Option<VoidCatalogClosureSummaryV1>,
  memory: MemoryReservation,
  failed: bool,
}

impl std::fmt::Debug for VoidClaimTransitionValidatorV1<'_> {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("VoidClaimTransitionValidatorV1")
      .field("source_manifest_key", &hex::encode(&self.source_manifest.key))
      .field("result_manifest_key", &hex::encode(&self.result_manifest.key))
      .field("claim_key", &hex::encode(&self.claim.key))
      .field("failed", &self.failed)
      .finish_non_exhaustive()
  }
}

impl<'a> VoidClaimTransitionValidatorV1<'a> {
  pub fn new(
    source: &'a SweepVoidArtifactV1<'a>,
    result: &'a SweepVoidArtifactV1<'a>,
    claim: &'a SweepVoidArtifactV1<'a>,
    cancellation: CancellationToken,
    limits: VoidClaimTransitionLimitsV1,
    memory: &MemoryCoordinator,
  ) -> Result<Self, VoidClaimTransitionErrorV1> {
    let (SweepVoidArtifactV1::VoidCatalog(source_manifest), SweepVoidArtifactV1::VoidCatalog(result_manifest)) = (source, result) else {
      return Err(VoidClaimTransitionErrorV1::invalid(
        "void_claim_transition_catalog_kind",
        "claim transition requires source and result Void catalog manifests",
      ));
    };
    let SweepVoidArtifactV1::VoidClaim(claim) = claim else {
      return Err(VoidClaimTransitionErrorV1::invalid(
        "void_claim_transition_claim_kind",
        "claim transition requires one immutable Void claim",
      ));
    };
    if cancellation.is_cancelled() {
      return Err(VoidClaimTransitionErrorV1::Canceled);
    }
    if limits.maximum_support_artifacts_per_catalog == 0 {
      return Err(VoidClaimTransitionErrorV1::invalid("void_claim_transition_limits", "support artifact limit must be nonzero"));
    }
    let algorithm = claim.hash_algorithm;
    let hash_width = algorithm.hash_length();
    if source_manifest.database_id != result_manifest.database_id
      || source_manifest.database_id != claim.database_id
      || claim.source_manifest_hash != source_manifest.key
      || source_manifest.key.len() != hash_width
      || result_manifest.key.len() != hash_width
      || claim.key.len() != hash_width
      || claim.claim_id.len() != 16
      || claim.generation
        != source_manifest
          .generation
          .checked_add(1)
          .ok_or_else(|| VoidClaimTransitionErrorV1::invalid("void_claim_transition_generation", "source generation cannot advance"))?
      || result_manifest.generation != claim.generation
      || claim.created_at_ms < source_manifest.published_at_ms
      || result_manifest.published_at_ms < source_manifest.published_at_ms
      || result_manifest.published_at_ms < claim.created_at_ms
      || result_manifest.next_page_id < source_manifest.next_page_id
    {
      return Err(VoidClaimTransitionErrorV1::invalid(
        "void_claim_transition_identity",
        "source, claim, and result identities, generations, times, or page allocator do not form one transition",
      ));
    }

    let claim_extent_count = usize::try_from(claim.extent_count)?;
    let accounted_bytes = claim_extent_count
      .checked_mul(std::mem::size_of::<VoidClaimAdmittedExtentV1>() + 3 * hash_width)
      .and_then(|bytes| bytes.checked_add(5 * hash_width + 3 * hash_width + 16))
      .ok_or_else(|| VoidClaimTransitionErrorV1::invalid("void_claim_transition_memory", "claim memory accounting overflowed"))?;
    let claim_memory = memory.reserve(MemoryOwner::GarbageCollection, u64::try_from(accounted_bytes)?, AdmissionClass::Maintenance)?;
    let mut claimed_extents = Vec::new();
    if let Err(source) = claimed_extents.try_reserve_exact(claim_extent_count) {
      drop(claim_memory);
      return Err(source.into());
    }
    for extent in claim.extent_records()? {
      let extent = extent?;
      let mut origin_sweep_proposal_hash = Vec::new();
      origin_sweep_proposal_hash.try_reserve_exact(extent.origin_sweep_proposal_hash.len())?;
      origin_sweep_proposal_hash.extend_from_slice(extent.origin_sweep_proposal_hash);
      claimed_extents.push(VoidClaimAdmittedExtentV1 {
        offset: extent.offset,
        length: extent.length,
        reclaim_commit_sequence: 0,
        void_generation: 0,
        origin_sweep_proposal_hash,
        origin_quarantine_manifest_hash: Vec::new(),
        reclaimed_incarnation_digest: Vec::new(),
      });
    }

    let closure_limits = VoidCatalogClosureLimitsV1 { maximum_support_artifacts: limits.maximum_support_artifacts_per_catalog };
    Ok(Self {
      source_manifest,
      result_manifest,
      claim,
      cancellation: cancellation.clone(),
      source_validator: Some(VoidCatalogClosureValidatorV1::new(source_manifest, algorithm, cancellation.clone(), closure_limits, memory)?),
      result_validator: Some(VoidCatalogClosureValidatorV1::new(result_manifest, algorithm, cancellation, closure_limits, memory)?),
      claimed_extents,
      next_claimed_extent: 0,
      expected_free_digest: DigestChainV1::new(algorithm, FREE_DIGEST_DOMAIN),
      result_free_digest: DigestChainV1::new(algorithm, FREE_DIGEST_DOMAIN),
      expected_claim_digest: DigestChainV1::new(algorithm, CLAIM_DIGEST_DOMAIN),
      result_claim_digest: DigestChainV1::new(algorithm, CLAIM_DIGEST_DOMAIN),
      new_claim_inserted: false,
      expected_free_count: 0,
      expected_free_bytes: 0,
      source_closure: None,
      memory: claim_memory,
      failed: false,
    })
  }

  pub fn observe_source_encoded(&mut self, bytes: &[u8]) -> Result<(), VoidClaimTransitionErrorV1> {
    if let Err(error) = self.preflight_source() {
      self.failed = true;
      return Err(error);
    }
    let result = self.observe_source_encoded_inner(bytes);
    self.latch(result)
  }

  pub fn finish_source(&mut self) -> Result<(), VoidClaimTransitionErrorV1> {
    if let Err(error) = self.preflight_source() {
      self.failed = true;
      return Err(error);
    }
    let result = self.finish_source_inner();
    self.latch(result)
  }

  pub fn observe_result_encoded(&mut self, bytes: &[u8]) -> Result<(), VoidClaimTransitionErrorV1> {
    if let Err(error) = self.preflight_result() {
      self.failed = true;
      return Err(error);
    }
    let result = self.observe_result_encoded_inner(bytes);
    self.latch(result)
  }

  pub fn finish(mut self) -> Result<VoidClaimTransitionSummaryV1, VoidClaimTransitionErrorV1> {
    self.preflight_result()?;
    let source_closure = self.source_closure.take().ok_or_else(|| {
      VoidClaimTransitionErrorV1::invalid("void_claim_transition_phase", "source closure was not finished before result closure")
    })?;
    let result_closure = self
      .result_validator
      .take()
      .ok_or_else(|| VoidClaimTransitionErrorV1::invalid("void_claim_transition_phase", "result closure was already finished"))?
      .finish()?;
    if self.result_free_digest.count != self.expected_free_digest.count
      || self.result_free_digest.value != self.expected_free_digest.value
      || self.result_claim_digest.count != self.expected_claim_digest.count
      || self.result_claim_digest.value != self.expected_claim_digest.value
      || result_closure.free_extent_count != self.expected_free_count
      || result_closure.free_bytes != self.expected_free_bytes
      || result_closure.outstanding_claim_count
        != source_closure
          .outstanding_claim_count
          .checked_add(1)
          .ok_or_else(|| VoidClaimTransitionErrorV1::invalid("void_claim_transition_claim_count", "outstanding claim count overflowed"))?
      || result_closure.claimed_bytes
        != source_closure
          .claimed_bytes
          .checked_add(self.claim.total_bytes)
          .ok_or_else(|| VoidClaimTransitionErrorV1::invalid("void_claim_transition_claim_bytes", "outstanding claim bytes overflowed"))?
    {
      return Err(VoidClaimTransitionErrorV1::invalid(
        "void_claim_transition_result",
        "replacement catalog is not the exact source catalog minus the claim plus immutable claim evidence",
      ));
    }
    let mut claim_id = [0u8; 16];
    claim_id.copy_from_slice(self.claim.claim_id);
    Ok(VoidClaimTransitionSummaryV1 {
      source_manifest_key: self.source_manifest.key.clone(),
      result_manifest_key: self.result_manifest.key.clone(),
      claim_key: self.claim.key.clone(),
      claim_id,
      claimed_extent_count: self.claim.extent_count,
      claimed_bytes: self.claim.total_bytes,
      source_closure,
      result_closure,
      claimed_extents: self.claimed_extents.into_boxed_slice(),
      _memory: self.memory,
    })
  }

  fn observe_source_encoded_inner(&mut self, bytes: &[u8]) -> Result<(), VoidClaimTransitionErrorV1> {
    self.check_cancellation()?;
    self
      .source_validator
      .as_mut()
      .ok_or_else(|| {
        VoidClaimTransitionErrorV1::invalid("void_claim_transition_phase", "source validator disappeared after transition preflight")
      })?
      .observe_encoded(bytes)?;
    match decode_sweep_void_artifact(bytes, self.claim.hash_algorithm)? {
      SweepVoidArtifactV1::VoidExtentPage(page) => {
        for extent in page.extent_records()? {
          self.observe_source_free_extent(&extent?)?;
        }
      }
      SweepVoidArtifactV1::VoidClaim(existing) => self.observe_source_claim(&existing)?,
      SweepVoidArtifactV1::VoidDirectory(_) => {}
      _ => {
        return Err(VoidClaimTransitionErrorV1::invalid(
          "void_claim_transition_source_kind",
          "source closure contains a non-Void-support artifact",
        ));
      }
    }
    Ok(())
  }

  fn finish_source_inner(&mut self) -> Result<(), VoidClaimTransitionErrorV1> {
    self.check_cancellation()?;
    if self.next_claimed_extent != self.claimed_extents.len() {
      return Err(VoidClaimTransitionErrorV1::invalid(
        "void_claim_transition_unavailable",
        "one or more claimed ranges are absent from selected free-space authority",
      ));
    }
    self.insert_new_claim_if_needed()?;
    let source_closure = self
      .source_validator
      .take()
      .ok_or_else(|| VoidClaimTransitionErrorV1::invalid("void_claim_transition_phase", "source closure was already finished"))?
      .finish()?;
    if self.expected_free_bytes
      != source_closure
        .free_bytes
        .checked_sub(self.claim.total_bytes)
        .ok_or_else(|| VoidClaimTransitionErrorV1::invalid("void_claim_transition_free_bytes", "claim bytes exceed selected free bytes"))?
    {
      return Err(VoidClaimTransitionErrorV1::invalid(
        "void_claim_transition_free_bytes",
        "source-minus-claim free-byte accounting does not close",
      ));
    }
    self.source_closure = Some(source_closure);
    Ok(())
  }

  fn observe_result_encoded_inner(&mut self, bytes: &[u8]) -> Result<(), VoidClaimTransitionErrorV1> {
    self.check_cancellation()?;
    self
      .result_validator
      .as_mut()
      .ok_or_else(|| {
        VoidClaimTransitionErrorV1::invalid("void_claim_transition_phase", "result validator disappeared after transition preflight")
      })?
      .observe_encoded(bytes)?;
    match decode_sweep_void_artifact(bytes, self.claim.hash_algorithm)? {
      SweepVoidArtifactV1::VoidExtentPage(page) => {
        for extent in page.extent_records()? {
          let extent = extent?;
          push_free_extent_digest(&mut self.result_free_digest, &extent)?;
        }
      }
      SweepVoidArtifactV1::VoidClaim(claim) => push_claim_digest(&mut self.result_claim_digest, &claim)?,
      SweepVoidArtifactV1::VoidDirectory(_) => {}
      _ => {
        return Err(VoidClaimTransitionErrorV1::invalid(
          "void_claim_transition_result_kind",
          "result closure contains a non-Void-support artifact",
        ));
      }
    }
    Ok(())
  }

  fn observe_source_free_extent(&mut self, source: &VoidExtentRecordV1<'_>) -> Result<(), VoidClaimTransitionErrorV1> {
    self.check_cancellation()?;
    let source_end = source
      .offset
      .checked_add(u64::from(source.length))
      .ok_or_else(|| VoidClaimTransitionErrorV1::invalid("void_claim_transition_source_extent", "source extent end overflowed"))?;
    let mut cursor = source.offset;
    while self.next_claimed_extent < self.claimed_extents.len() {
      let claimed_index = self.next_claimed_extent;
      let claimed_offset = self.claimed_extents[claimed_index].offset;
      let claimed_length = self.claimed_extents[claimed_index].length;
      if claimed_offset >= source_end {
        break;
      }
      if claimed_offset < cursor {
        return Err(VoidClaimTransitionErrorV1::invalid(
          "void_claim_transition_unavailable",
          "claimed ranges overlap or start outside selected free-space authority",
        ));
      }
      let claimed_end = claimed_offset
        .checked_add(u64::from(claimed_length))
        .ok_or_else(|| VoidClaimTransitionErrorV1::invalid("void_claim_transition_claim_extent", "claimed extent end overflowed"))?;
      if claimed_end > source_end || self.claimed_extents[claimed_index].origin_sweep_proposal_hash != source.origin_sweep_proposal_hash {
        return Err(VoidClaimTransitionErrorV1::invalid(
          "void_claim_transition_unavailable",
          "claimed range is not contained in one source extent with exact sweep provenance",
        ));
      }
      let claimed = &mut self.claimed_extents[claimed_index];
      claimed.reclaim_commit_sequence = source.reclaim_commit_sequence;
      claimed.void_generation = source.void_generation;
      claimed.origin_quarantine_manifest_hash.try_reserve_exact(source.origin_quarantine_manifest_hash.len())?;
      claimed.origin_quarantine_manifest_hash.extend_from_slice(source.origin_quarantine_manifest_hash);
      claimed.reclaimed_incarnation_digest.try_reserve_exact(source.reclaimed_incarnation_digest.len())?;
      claimed.reclaimed_incarnation_digest.extend_from_slice(source.reclaimed_incarnation_digest);
      if cursor < claimed_offset {
        self.push_expected_free_fragment(source, cursor, claimed_offset - cursor)?;
      }
      cursor = claimed_end;
      self.next_claimed_extent += 1;
    }
    if cursor < source_end {
      self.push_expected_free_fragment(source, cursor, source_end - cursor)?;
    }
    Ok(())
  }

  fn push_expected_free_fragment(
    &mut self,
    source: &VoidExtentRecordV1<'_>,
    offset: u64,
    length: u64,
  ) -> Result<(), VoidClaimTransitionErrorV1> {
    let length = u32::try_from(length).map_err(|source| {
      VoidClaimTransitionErrorV1::invalid("void_claim_transition_fragment_length", format!("free fragment exceeds u32: {source}"))
    })?;
    push_free_fields_digest(&mut self.expected_free_digest, offset, length, source)?;
    self.expected_free_count = self
      .expected_free_count
      .checked_add(1)
      .ok_or_else(|| VoidClaimTransitionErrorV1::invalid("void_claim_transition_free_count", "expected free count overflowed"))?;
    self.expected_free_bytes = self
      .expected_free_bytes
      .checked_add(u64::from(length))
      .ok_or_else(|| VoidClaimTransitionErrorV1::invalid("void_claim_transition_free_bytes", "expected free bytes overflowed"))?;
    Ok(())
  }

  fn observe_source_claim(&mut self, existing: &VoidClaimV1<'_>) -> Result<(), VoidClaimTransitionErrorV1> {
    if existing.claim_id == self.claim.claim_id {
      return Err(VoidClaimTransitionErrorV1::invalid(
        "void_claim_transition_duplicate_claim",
        "selected source catalog already contains the requested claim ID",
      ));
    }
    if !self.new_claim_inserted && self.claim.claim_id < existing.claim_id {
      push_claim_digest(&mut self.expected_claim_digest, self.claim)?;
      self.new_claim_inserted = true;
    }
    push_claim_digest(&mut self.expected_claim_digest, existing)
  }

  fn insert_new_claim_if_needed(&mut self) -> Result<(), VoidClaimTransitionErrorV1> {
    if !self.new_claim_inserted {
      push_claim_digest(&mut self.expected_claim_digest, self.claim)?;
      self.new_claim_inserted = true;
    }
    Ok(())
  }

  fn preflight_source(&self) -> Result<(), VoidClaimTransitionErrorV1> {
    if self.failed {
      return Err(VoidClaimTransitionErrorV1::Failed);
    }
    if self.source_validator.is_none() || self.source_closure.is_some() {
      return Err(VoidClaimTransitionErrorV1::invalid(
        "void_claim_transition_phase",
        "source observation is unavailable after source closure finishes",
      ));
    }
    self.check_cancellation()
  }

  fn preflight_result(&self) -> Result<(), VoidClaimTransitionErrorV1> {
    if self.failed {
      return Err(VoidClaimTransitionErrorV1::Failed);
    }
    if self.source_validator.is_some() || self.source_closure.is_none() || self.result_validator.is_none() {
      return Err(VoidClaimTransitionErrorV1::invalid(
        "void_claim_transition_phase",
        "result observation requires one complete source closure",
      ));
    }
    self.check_cancellation()
  }

  fn check_cancellation(&self) -> Result<(), VoidClaimTransitionErrorV1> {
    if self.cancellation.is_cancelled() {
      Err(VoidClaimTransitionErrorV1::Canceled)
    } else {
      Ok(())
    }
  }

  fn latch(&mut self, result: Result<(), VoidClaimTransitionErrorV1>) -> Result<(), VoidClaimTransitionErrorV1> {
    match result {
      Ok(()) => Ok(()),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoidClaimAdmissionAuthoritySnapshotV1 {
  pub selected_source_manifest_hash: Vec<u8>,
  pub selected_source_control_sequence: u64,
  pub source_catalog_receipt_backed: bool,
  pub source_catalog_closure_current: bool,
  pub allocator_admission_excluded: bool,
  pub no_other_claim_admission_active: bool,
  pub in_memory_void_authority_current: bool,
  pub conflicting_receipt_count: u32,
  pub repair_latch_clear: bool,
}

#[derive(Clone, Copy)]
pub struct VoidClaimAdmissionAuthorityRequestV1<'a> {
  pub source_manifest: &'a VoidCatalogManifestV1<'a>,
  pub result_manifest: &'a VoidCatalogManifestV1<'a>,
  pub claim: &'a VoidClaimV1<'a>,
  pub transition: &'a VoidClaimTransitionSummaryV1,
  pub selected_source_control_sequence: u64,
  pub cancellation: &'a CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct VoidClaimAdmissionAuthorityErrorV1 {
  code: String,
  message: String,
}

impl VoidClaimAdmissionAuthorityErrorV1 {
  pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
    Self { code: code.into(), message: message.into() }
  }

  pub fn code(&self) -> &str {
    if self.code.is_empty() {
      "void_claim_admission_authority"
    } else {
      self.code.as_str()
    }
  }
}

pub trait VoidClaimAdmissionAuthorityV1 {
  /// Recheck the exact receipt-backed source, allocator exclusion, in-memory
  /// authority, conflicting receipts, and repair state while first authority
  /// is held. Implementations must not reenter the first-authority publisher.
  fn recheck_void_claim_admission_authority(
    &mut self,
    request: VoidClaimAdmissionAuthorityRequestV1<'_>,
  ) -> Result<VoidClaimAdmissionAuthoritySnapshotV1, VoidClaimAdmissionAuthorityErrorV1>;
}

#[derive(Debug, Error)]
pub enum VoidClaimAdmissionErrorV1 {
  #[error("{code}: {message}")]
  Invalid { code: &'static str, message: String },
  #[error("{code}: Void claim committed, but post-commit handling failed: {message}")]
  Committed { code: &'static str, message: String, permit: Box<VoidClaimAdmissionPermitV1> },
  #[error("Void claim transition failed: {0}")]
  Transition(#[from] VoidClaimTransitionErrorV1),
  #[error("Void claim authority recheck failed: {0}")]
  AuthorityRecheck(#[from] VoidClaimAdmissionAuthorityErrorV1),
  #[error("Void claim first-authority failure: {0}")]
  Authority(#[from] FirstAuthorityPublicationErrorV1),
  #[error("Void claim retirement-lineage admission failed: {0}")]
  RetirementAdmission(#[from] RetirementJournalReplacementAdmissionErrorV1),
  #[error("Void claim retirement-lineage owner failed: {0}")]
  RetirementOwner(#[from] RetirementJournalOwnerErrorV1),
  #[error("Void claim format failed: {0}")]
  Format(#[from] FormatError),
  #[error("Void claim allocation failed: {0}")]
  Allocation(#[from] std::collections::TryReserveError),
}

impl VoidClaimAdmissionErrorV1 {
  pub fn code(&self) -> &str {
    match self {
      Self::Invalid { code, .. } | Self::Committed { code, .. } => code,
      Self::Transition(source) => source.code(),
      Self::AuthorityRecheck(source) => source.code(),
      Self::Authority(source) => source.code(),
      Self::RetirementAdmission(source) => source.code(),
      Self::RetirementOwner(source) => source.code(),
      Self::Format(source) => source.code(),
      Self::Allocation(_) => "void_claim_admission_allocation",
    }
  }

  pub(crate) fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }

  pub(crate) fn committed(code: &'static str, message: impl Into<String>, permit: VoidClaimAdmissionPermitV1) -> Self {
    Self::Committed { code, message: message.into(), permit: Box::new(permit) }
  }

  pub(crate) fn support(source: &super::gc_void_publication::VoidCatalogPublicationErrorV1) -> Self {
    Self::invalid("void_claim_admission_support", format!("{}: {source}", source.code()))
  }

  pub fn committed_permit(&self) -> Option<&VoidClaimAdmissionPermitV1> {
    match self {
      Self::Committed { permit, .. } => Some(permit),
      Self::Invalid { .. }
      | Self::Transition(_)
      | Self::AuthorityRecheck(_)
      | Self::Authority(_)
      | Self::RetirementAdmission(_)
      | Self::RetirementOwner(_)
      | Self::Format(_)
      | Self::Allocation(_) => None,
    }
  }
}

pub(crate) fn validate_void_claim_admission_authority_v1(
  request: VoidClaimAdmissionAuthorityRequestV1<'_>,
  snapshot: &VoidClaimAdmissionAuthoritySnapshotV1,
) -> Result<(), VoidClaimAdmissionErrorV1> {
  if request.cancellation.is_cancelled() {
    return Err(VoidClaimAdmissionErrorV1::invalid(
      "void_claim_admission_canceled",
      "claim admission was canceled during final authority validation",
    ));
  }
  if snapshot.selected_source_manifest_hash != request.source_manifest.key
    || snapshot.selected_source_control_sequence != request.selected_source_control_sequence
    || request.result_manifest.previous_control_sequence != request.selected_source_control_sequence
    || request.transition.source_manifest_key != request.source_manifest.key
    || request.transition.result_manifest_key != request.result_manifest.key
    || request.transition.claim_key != request.claim.key
  {
    return Err(VoidClaimAdmissionErrorV1::invalid(
      "void_claim_admission_source_authority",
      "selected source or transition identity differs from caller-owned authority",
    ));
  }
  if !snapshot.source_catalog_receipt_backed
    || !snapshot.source_catalog_closure_current
    || !snapshot.allocator_admission_excluded
    || !snapshot.no_other_claim_admission_active
    || !snapshot.in_memory_void_authority_current
    || snapshot.conflicting_receipt_count != 0
    || !snapshot.repair_latch_clear
  {
    return Err(VoidClaimAdmissionErrorV1::invalid(
      "void_claim_admission_authority_incomplete",
      "receipt, closure, allocator, claim-owner, memory, conflict, or repair authority is incomplete",
    ));
  }
  Ok(())
}

fn push_free_extent_digest(chain: &mut DigestChainV1, extent: &VoidExtentRecordV1<'_>) -> Result<(), VoidClaimTransitionErrorV1> {
  push_free_fields_digest(chain, extent.offset, extent.length, extent)
}

fn push_free_fields_digest(
  chain: &mut DigestChainV1,
  offset: u64,
  length: u32,
  source: &VoidExtentRecordV1<'_>,
) -> Result<(), VoidClaimTransitionErrorV1> {
  chain.push(&[
    &offset.to_le_bytes(),
    &length.to_le_bytes(),
    &source.reclaim_commit_sequence.to_le_bytes(),
    &source.void_generation.to_le_bytes(),
    source.origin_sweep_proposal_hash,
    source.origin_quarantine_manifest_hash,
    source.reclaimed_incarnation_digest,
  ])
}

fn push_claim_digest(chain: &mut DigestChainV1, claim: &VoidClaimV1<'_>) -> Result<(), VoidClaimTransitionErrorV1> {
  chain.push(&[claim.claim_id, &claim.key])
}
