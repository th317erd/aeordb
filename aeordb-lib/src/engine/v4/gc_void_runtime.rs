//! Bounded restart reconstruction of selected, receipt-backed Void space.
//!
//! This state is an observation, not overwrite authority. Allocation still
//! requires a durable claim admitted by the first-authority owner.

use std::num::TryFromIntError;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::first_authority::FirstAuthorityPublicationErrorV1;
use super::gc_state::GcDirectoryRoleV1;
use super::gc_void::{SweepVoidArtifactV1, VoidCatalogManifestV1, VoidExtentRecordV1, decode_sweep_void_artifact};
use super::gc_void_publication::{
  VoidCatalogClosureErrorV1, VoidCatalogClosureLimitsV1, VoidCatalogClosureValidatorV1, VoidCatalogPublicationErrorV1,
};
use super::reader::FormatError;
use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

const MAXIMUM_OUTSTANDING_CLAIM_EXTENTS_V1: u64 = 1_000_000;
const MAXIMUM_REUSABLE_CANDIDATE_EXTENTS_V1: u32 = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoidReusableStateLimitsV1 {
  pub maximum_support_artifacts: u64,
  pub maximum_outstanding_claim_extents: u64,
  pub maximum_candidate_extents: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct VoidReusableStateIdentityV1<'a> {
  pub selected_manifest_key: &'a [u8],
  pub selected_control_key: &'a [u8],
  pub selected_control_sequence: u64,
  pub selected_control_write_sequence: u64,
  pub selected_control_slot: u8,
}

#[derive(Clone, Copy)]
pub struct VoidReusableStateReconstructionRequestV1<'a> {
  pub cancellation: &'a CancellationToken,
  pub memory: &'a MemoryCoordinator,
  pub limits: VoidReusableStateLimitsV1,
}

#[derive(Clone, Copy, Debug)]
pub struct VoidReclaimReceiptAuthorityRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8],
  pub selected_manifest_key: &'a [u8],
  pub selected_generation: u64,
  pub extent: VoidExtentRecordV1<'a>,
  pub cancellation: &'a CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoidReclaimReceiptAuthoritySnapshotV1 {
  pub database_id: [u8; 16],
  pub selected_manifest_key: Vec<u8>,
  pub selected_generation: u64,
  pub origin_sweep_proposal_hash: Vec<u8>,
  pub origin_quarantine_manifest_hash: Vec<u8>,
  pub reclaimed_incarnation_digest: Vec<u8>,
  pub proposal_write_sequence: u64,
  pub receipt_hash: Vec<u8>,
  pub receipt_write_sequence: u64,
  pub reclaim_commit_sequence: u64,
  pub receipt_reclaimed_offset: u64,
  pub receipt_reclaimed_length: u32,
  pub exact_proposal_receipt_current: bool,
  pub locator_removal_durable: bool,
  pub replacement_lineage_complete: bool,
  pub receipt_search_complete: bool,
  pub conflicting_receipt_count: u32,
  pub repair_latch_clear: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct VoidReclaimReceiptAuthorityErrorV1 {
  code: String,
  message: String,
}

impl VoidReclaimReceiptAuthorityErrorV1 {
  pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
    Self { code: code.into(), message: message.into() }
  }

  pub fn code(&self) -> &str {
    if self.code.is_empty() {
      "void_runtime_receipt_authority"
    } else {
      self.code.as_str()
    }
  }
}

pub trait VoidReclaimReceiptAuthorityV1 {
  /// Resolve one exact sweep receipt while the caller holds first authority.
  /// Implementations must not reenter the first-authority owner.
  fn recheck_void_reclaim_receipt_authority(
    &mut self,
    request: VoidReclaimReceiptAuthorityRequestV1<'_>,
  ) -> Result<VoidReclaimReceiptAuthoritySnapshotV1, VoidReclaimReceiptAuthorityErrorV1>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoidReusableExtentV1 {
  pub offset: u64,
  pub length: u32,
  pub origin_sweep_proposal_hash: Vec<u8>,
  pub origin_quarantine_manifest_hash: Vec<u8>,
  pub reclaimed_incarnation_digest: Vec<u8>,
  pub reclaim_commit_sequence: u64,
  pub void_generation: u64,
}

#[must_use = "reconstructed Void state is only a bounded observation; allocation still requires a durable claim"]
pub struct VoidReusableSpaceStateV1 {
  selected_manifest_key: Vec<u8>,
  selected_control_key: Vec<u8>,
  selected_control_sequence: u64,
  selected_control_write_sequence: u64,
  selected_control_slot: u8,
  generation: u64,
  support_artifact_count: u64,
  free_count: u64,
  free_bytes: u64,
  outstanding_claim_count: u64,
  claimed_bytes: u64,
  candidate_extents: Vec<VoidReusableExtentV1>,
  candidate_window_truncated: bool,
  _memory: MemoryReservation,
}

impl std::fmt::Debug for VoidReusableSpaceStateV1 {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("VoidReusableSpaceStateV1")
      .field("selected_manifest_key", &hex::encode(&self.selected_manifest_key))
      .field("selected_control_key", &hex::encode(&self.selected_control_key))
      .field("selected_control_sequence", &self.selected_control_sequence)
      .field("generation", &self.generation)
      .field("free_count", &self.free_count)
      .field("outstanding_claim_count", &self.outstanding_claim_count)
      .field("candidate_extent_count", &self.candidate_extents.len())
      .field("candidate_window_truncated", &self.candidate_window_truncated)
      .finish_non_exhaustive()
  }
}

impl VoidReusableSpaceStateV1 {
  pub fn selected_manifest_key(&self) -> &[u8] {
    &self.selected_manifest_key
  }

  pub fn selected_control_key(&self) -> &[u8] {
    &self.selected_control_key
  }

  pub const fn selected_control_sequence(&self) -> u64 {
    self.selected_control_sequence
  }

  pub const fn selected_control_write_sequence(&self) -> u64 {
    self.selected_control_write_sequence
  }

  pub const fn selected_control_slot(&self) -> u8 {
    self.selected_control_slot
  }

  pub const fn generation(&self) -> u64 {
    self.generation
  }

  pub const fn support_artifact_count(&self) -> u64 {
    self.support_artifact_count
  }

  pub const fn free_count(&self) -> u64 {
    self.free_count
  }

  pub const fn free_bytes(&self) -> u64 {
    self.free_bytes
  }

  pub const fn outstanding_claim_count(&self) -> u64 {
    self.outstanding_claim_count
  }

  pub const fn claimed_bytes(&self) -> u64 {
    self.claimed_bytes
  }

  pub fn candidate_extents(&self) -> &[VoidReusableExtentV1] {
    &self.candidate_extents
  }

  pub const fn candidate_window_truncated(&self) -> bool {
    self.candidate_window_truncated
  }
}

#[derive(Debug, Error)]
pub enum VoidReusableStateErrorV1 {
  #[error("Void reusable-state reconstruction was canceled")]
  Canceled,
  #[error("Void reusable-state validator is already failed")]
  Failed,
  #[error("{code}: {message}")]
  Invalid { code: &'static str, message: String },
  #[error("Void reusable-state receipt-authority recheck failed: {0}")]
  ReceiptAuthority(#[from] VoidReclaimReceiptAuthorityErrorV1),
  #[error("Void reusable-state closure validation failed: {0}")]
  Closure(#[from] VoidCatalogClosureErrorV1),
  #[error("Void reusable-state first-authority operation failed: {0}")]
  Authority(#[from] FirstAuthorityPublicationErrorV1),
  #[error("Void reusable-state durable support read failed: {0}")]
  Support(#[from] VoidCatalogPublicationErrorV1),
  #[error("Void reusable-state memory admission failed: {0}")]
  Memory(#[from] MemoryCoordinatorError),
  #[error("Void reusable-state allocation failed: {0}")]
  Allocation(#[from] std::collections::TryReserveError),
  #[error("Void reusable-state integer conversion failed: {0}")]
  IntegerConversion(#[from] TryFromIntError),
  #[error("Void reusable-state format failure: {0}")]
  Format(#[from] FormatError),
}

impl VoidReusableStateErrorV1 {
  pub fn code(&self) -> &str {
    match self {
      Self::Canceled => "void_runtime_canceled",
      Self::Failed => "void_runtime_failed",
      Self::Invalid { code, .. } => code,
      Self::ReceiptAuthority(source) => source.code(),
      Self::Closure(source) => source.code(),
      Self::Authority(source) => source.code(),
      Self::Support(source) => source.code(),
      Self::Memory(_) => "void_runtime_memory",
      Self::Allocation(_) => "void_runtime_allocation",
      Self::IntegerConversion(_) => "void_runtime_integer_conversion",
      Self::Format(source) => source.code(),
    }
  }

  pub(crate) fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }
}

#[derive(Clone, Copy, Debug)]
struct OutstandingClaimIntervalV1 {
  offset: u64,
  end: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReconstructionPhaseV1 {
  Claims,
  Free,
}

pub struct VoidReusableStateValidatorV1<'a> {
  manifest: &'a VoidCatalogManifestV1<'a>,
  algorithm: HashAlgorithm,
  identity: VoidReusableStateIdentityV1<'a>,
  cancellation: CancellationToken,
  closure: Option<VoidCatalogClosureValidatorV1<'a>>,
  phase: ReconstructionPhaseV1,
  maximum_outstanding_claim_extents: usize,
  outstanding_claim_intervals: Vec<OutstandingClaimIntervalV1>,
  _claim_memory: MemoryReservation,
  maximum_candidate_extents: usize,
  candidate_extents: Vec<VoidReusableExtentV1>,
  state_memory: MemoryReservation,
  observed_free_extent_count: u64,
  failed: bool,
}

impl std::fmt::Debug for VoidReusableStateValidatorV1<'_> {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("VoidReusableStateValidatorV1")
      .field("manifest_key", &hex::encode(&self.manifest.key))
      .field("phase", &self.phase)
      .field("outstanding_claim_extent_count", &self.outstanding_claim_intervals.len())
      .field("candidate_extent_count", &self.candidate_extents.len())
      .field("failed", &self.failed)
      .finish_non_exhaustive()
  }
}

impl<'a> VoidReusableStateValidatorV1<'a> {
  pub fn new(
    manifest: &'a VoidCatalogManifestV1<'a>,
    algorithm: HashAlgorithm,
    identity: VoidReusableStateIdentityV1<'a>,
    cancellation: CancellationToken,
    limits: VoidReusableStateLimitsV1,
    memory: &MemoryCoordinator,
  ) -> Result<Self, VoidReusableStateErrorV1> {
    validate_limits(limits)?;
    if cancellation.is_cancelled() {
      return Err(VoidReusableStateErrorV1::Canceled);
    }
    let hash_width = algorithm.hash_length();
    if manifest.database_id.len() != 16
      || manifest.key.len() != hash_width
      || identity.selected_manifest_key != manifest.key
      || identity.selected_control_key.len() != hash_width
      || identity.selected_control_key.iter().all(|byte| *byte == 0)
      || identity.selected_control_sequence == 0
      || identity.selected_control_write_sequence == 0
      || identity.selected_control_slot > 1
    {
      return Err(VoidReusableStateErrorV1::invalid(
        "void_runtime_identity",
        "selected manifest/control identity is incomplete or differs from the selected hash profile",
      ));
    }

    let maximum_outstanding_claim_extents = usize::try_from(limits.maximum_outstanding_claim_extents)?;
    let maximum_candidate_extents = usize::try_from(limits.maximum_candidate_extents)?;
    let claim_bytes = maximum_outstanding_claim_extents
      .checked_mul(std::mem::size_of::<OutstandingClaimIntervalV1>())
      .ok_or_else(|| VoidReusableStateErrorV1::invalid("void_runtime_memory", "claim interval memory bound overflowed"))?;
    let candidate_unit_bytes = std::mem::size_of::<VoidReusableExtentV1>()
      .checked_add(
        hash_width
          .checked_mul(3)
          .ok_or_else(|| VoidReusableStateErrorV1::invalid("void_runtime_memory", "candidate hash memory bound overflowed"))?,
      )
      .ok_or_else(|| VoidReusableStateErrorV1::invalid("void_runtime_memory", "candidate memory unit overflowed"))?;
    let identity_bytes = hash_width
      .checked_mul(2)
      .ok_or_else(|| VoidReusableStateErrorV1::invalid("void_runtime_memory", "selected identity memory bound overflowed"))?;
    let state_bytes = maximum_candidate_extents
      .checked_mul(candidate_unit_bytes)
      .and_then(|bytes| bytes.checked_add(identity_bytes))
      .ok_or_else(|| VoidReusableStateErrorV1::invalid("void_runtime_memory", "reusable-state memory bound overflowed"))?;
    let claim_memory = memory.reserve(MemoryOwner::GarbageCollection, u64::try_from(claim_bytes)?, AdmissionClass::Maintenance)?;
    let state_memory = memory.reserve(MemoryOwner::GarbageCollection, u64::try_from(state_bytes)?, AdmissionClass::Maintenance)?;

    let mut outstanding_claim_intervals = Vec::new();
    outstanding_claim_intervals.try_reserve_exact(maximum_outstanding_claim_extents)?;
    let mut candidate_extents = Vec::new();
    candidate_extents.try_reserve_exact(maximum_candidate_extents)?;
    let closure = VoidCatalogClosureValidatorV1::new(
      manifest,
      algorithm,
      cancellation.clone(),
      VoidCatalogClosureLimitsV1 { maximum_support_artifacts: limits.maximum_support_artifacts },
      memory,
    )?;
    Ok(Self {
      manifest,
      algorithm,
      identity,
      cancellation,
      closure: Some(closure),
      phase: ReconstructionPhaseV1::Claims,
      maximum_outstanding_claim_extents,
      outstanding_claim_intervals,
      _claim_memory: claim_memory,
      maximum_candidate_extents,
      candidate_extents,
      state_memory,
      observed_free_extent_count: 0,
      failed: false,
    })
  }

  pub fn observe_claim_encoded(&mut self, bytes: &[u8]) -> Result<(), VoidReusableStateErrorV1> {
    if let Err(source) = self.preflight(ReconstructionPhaseV1::Claims) {
      self.failed = true;
      return Err(source);
    }
    let result = self.observe_claim_encoded_inner(bytes);
    self.latch(result)
  }

  pub fn finish_claims(&mut self) -> Result<(), VoidReusableStateErrorV1> {
    if let Err(source) = self.preflight(ReconstructionPhaseV1::Claims) {
      self.failed = true;
      return Err(source);
    }
    let result = self.finish_claims_inner();
    self.latch(result)
  }

  pub fn observe_free_encoded(
    &mut self,
    bytes: &[u8],
    authority: &mut dyn VoidReclaimReceiptAuthorityV1,
  ) -> Result<(), VoidReusableStateErrorV1> {
    if let Err(source) = self.preflight(ReconstructionPhaseV1::Free) {
      self.failed = true;
      return Err(source);
    }
    let result = self.observe_free_encoded_inner(bytes, authority);
    self.latch(result)
  }

  pub fn finish(mut self) -> Result<VoidReusableSpaceStateV1, VoidReusableStateErrorV1> {
    self.preflight(ReconstructionPhaseV1::Free)?;
    let closure = self.closure.take().ok_or(VoidReusableStateErrorV1::Failed)?.finish()?;
    if self.observed_free_extent_count != closure.free_extent_count {
      return Err(VoidReusableStateErrorV1::invalid(
        "void_runtime_free_count",
        "receipt-proven free extent count differs from the closure summary",
      ));
    }
    let selected_manifest_key = clone_bytes(self.identity.selected_manifest_key)?;
    let selected_control_key = clone_bytes(self.identity.selected_control_key)?;
    let candidate_window_truncated = closure.free_extent_count > u64::try_from(self.candidate_extents.len())?;
    Ok(VoidReusableSpaceStateV1 {
      selected_manifest_key,
      selected_control_key,
      selected_control_sequence: self.identity.selected_control_sequence,
      selected_control_write_sequence: self.identity.selected_control_write_sequence,
      selected_control_slot: self.identity.selected_control_slot,
      generation: self.manifest.generation,
      support_artifact_count: closure.support_artifact_count,
      free_count: closure.free_extent_count,
      free_bytes: closure.free_bytes,
      outstanding_claim_count: closure.outstanding_claim_count,
      claimed_bytes: closure.claimed_bytes,
      candidate_extents: self.candidate_extents,
      candidate_window_truncated,
      _memory: self.state_memory,
    })
  }

  fn observe_claim_encoded_inner(&mut self, bytes: &[u8]) -> Result<(), VoidReusableStateErrorV1> {
    self.closure.as_mut().ok_or(VoidReusableStateErrorV1::Failed)?.observe_encoded(bytes)?;
    match decode_sweep_void_artifact(bytes, self.algorithm)? {
      SweepVoidArtifactV1::VoidClaim(claim) => {
        for extent in claim.extent_records()? {
          self.check_cancellation()?;
          let extent = extent?;
          if self.outstanding_claim_intervals.len() >= self.maximum_outstanding_claim_extents {
            return Err(VoidReusableStateErrorV1::invalid(
              "void_runtime_claim_extent_limit",
              "outstanding Void claim extents exceed their admitted bound",
            ));
          }
          let end = extent
            .offset
            .checked_add(u64::from(extent.length))
            .ok_or_else(|| VoidReusableStateErrorV1::invalid("void_runtime_claim_extent", "outstanding claim extent end overflowed"))?;
          self.outstanding_claim_intervals.push(OutstandingClaimIntervalV1 { offset: extent.offset, end });
        }
        Ok(())
      }
      SweepVoidArtifactV1::VoidDirectory(directory) if directory.role == GcDirectoryRoleV1::Claims => Ok(()),
      _ => Err(VoidReusableStateErrorV1::invalid(
        "void_runtime_claim_kind",
        "claims-first traversal accepts only outstanding claims and claim directories",
      )),
    }
  }

  fn finish_claims_inner(&mut self) -> Result<(), VoidReusableStateErrorV1> {
    self.check_cancellation()?;
    self.outstanding_claim_intervals.sort_unstable_by_key(|extent| extent.offset);
    if self.outstanding_claim_intervals.windows(2).any(|pair| pair[0].end > pair[1].offset) {
      return Err(VoidReusableStateErrorV1::invalid(
        "void_runtime_claim_overlap",
        "outstanding Void claims overlap after physical offset ordering",
      ));
    }
    self.phase = ReconstructionPhaseV1::Free;
    Ok(())
  }

  fn observe_free_encoded_inner(
    &mut self,
    bytes: &[u8],
    authority: &mut dyn VoidReclaimReceiptAuthorityV1,
  ) -> Result<(), VoidReusableStateErrorV1> {
    self.closure.as_mut().ok_or(VoidReusableStateErrorV1::Failed)?.observe_encoded(bytes)?;
    match decode_sweep_void_artifact(bytes, self.algorithm)? {
      SweepVoidArtifactV1::VoidExtentPage(page) => {
        for extent in page.extent_records()? {
          self.check_cancellation()?;
          let extent = extent?;
          if self.overlaps_outstanding_claim(extent)? {
            return Err(VoidReusableStateErrorV1::invalid(
              "void_runtime_claim_free_overlap",
              "selected free space overlaps an outstanding durable Void claim",
            ));
          }
          let request = VoidReclaimReceiptAuthorityRequestV1 {
            hash_algorithm: self.algorithm,
            database_id: self.manifest.database_id,
            selected_manifest_key: self.manifest.key.as_slice(),
            selected_generation: self.manifest.generation,
            extent,
            cancellation: &self.cancellation,
          };
          let snapshot = authority.recheck_void_reclaim_receipt_authority(request)?;
          validate_receipt_authority(request, &snapshot)?;
          self.observed_free_extent_count = self
            .observed_free_extent_count
            .checked_add(1)
            .ok_or_else(|| VoidReusableStateErrorV1::invalid("void_runtime_free_count", "receipt-proven free count overflowed"))?;
          if self.candidate_extents.len() < self.maximum_candidate_extents {
            self.candidate_extents.push(copy_extent(extent)?);
          }
        }
        Ok(())
      }
      SweepVoidArtifactV1::VoidDirectory(directory) if directory.role == GcDirectoryRoleV1::FreeExtents => Ok(()),
      _ => Err(VoidReusableStateErrorV1::invalid(
        "void_runtime_free_kind",
        "free traversal accepts only Void extent pages and free-extent directories",
      )),
    }
  }

  fn overlaps_outstanding_claim(&self, extent: VoidExtentRecordV1<'_>) -> Result<bool, VoidReusableStateErrorV1> {
    let end = extent
      .offset
      .checked_add(u64::from(extent.length))
      .ok_or_else(|| VoidReusableStateErrorV1::invalid("void_runtime_free_extent", "free extent end overflowed"))?;
    let candidate_index = self.outstanding_claim_intervals.partition_point(|claim| claim.end <= extent.offset);
    Ok(self.outstanding_claim_intervals.get(candidate_index).is_some_and(|claim| claim.offset < end))
  }

  fn preflight(&self, expected_phase: ReconstructionPhaseV1) -> Result<(), VoidReusableStateErrorV1> {
    if self.failed {
      return Err(VoidReusableStateErrorV1::Failed);
    }
    if self.cancellation.is_cancelled() {
      return Err(VoidReusableStateErrorV1::Canceled);
    }
    if self.phase != expected_phase {
      return Err(VoidReusableStateErrorV1::invalid("void_runtime_phase", "Void reusable-state traversal phase is out of order"));
    }
    Ok(())
  }

  fn check_cancellation(&self) -> Result<(), VoidReusableStateErrorV1> {
    if self.cancellation.is_cancelled() {
      Err(VoidReusableStateErrorV1::Canceled)
    } else {
      Ok(())
    }
  }

  fn latch<T>(&mut self, result: Result<T, VoidReusableStateErrorV1>) -> Result<T, VoidReusableStateErrorV1> {
    match result {
      Ok(value) => Ok(value),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }
}

fn validate_limits(limits: VoidReusableStateLimitsV1) -> Result<(), VoidReusableStateErrorV1> {
  if limits.maximum_support_artifacts == 0
    || limits.maximum_outstanding_claim_extents == 0
    || limits.maximum_outstanding_claim_extents > MAXIMUM_OUTSTANDING_CLAIM_EXTENTS_V1
    || limits.maximum_candidate_extents == 0
    || limits.maximum_candidate_extents > MAXIMUM_REUSABLE_CANDIDATE_EXTENTS_V1
  {
    return Err(VoidReusableStateErrorV1::invalid(
      "void_runtime_limits",
      "support, claim, and candidate bounds must be nonzero and remain within frozen runtime safety caps",
    ));
  }
  Ok(())
}

fn validate_receipt_authority(
  request: VoidReclaimReceiptAuthorityRequestV1<'_>,
  snapshot: &VoidReclaimReceiptAuthoritySnapshotV1,
) -> Result<(), VoidReusableStateErrorV1> {
  if request.cancellation.is_cancelled() {
    return Err(VoidReusableStateErrorV1::Canceled);
  }
  let hash_width = request.hash_algorithm.hash_length();
  let identity_exact = snapshot.database_id.as_slice() == request.database_id
    && snapshot.selected_manifest_key == request.selected_manifest_key
    && snapshot.selected_generation == request.selected_generation
    && snapshot.origin_sweep_proposal_hash == request.extent.origin_sweep_proposal_hash
    && snapshot.origin_quarantine_manifest_hash == request.extent.origin_quarantine_manifest_hash
    && snapshot.reclaimed_incarnation_digest == request.extent.reclaimed_incarnation_digest
    && snapshot.selected_manifest_key.len() == hash_width
    && snapshot.origin_sweep_proposal_hash.len() == hash_width
    && snapshot.origin_quarantine_manifest_hash.len() == hash_width
    && snapshot.reclaimed_incarnation_digest.len() == hash_width
    && snapshot.receipt_hash.len() == hash_width
    && snapshot.receipt_hash.iter().any(|byte| *byte != 0)
    && snapshot.proposal_write_sequence != 0
    && snapshot.receipt_write_sequence != 0
    && snapshot.reclaim_commit_sequence == request.extent.reclaim_commit_sequence
    && snapshot.reclaim_commit_sequence != 0;
  let receipt_end = snapshot.receipt_reclaimed_offset.checked_add(u64::from(snapshot.receipt_reclaimed_length));
  let extent_end = request.extent.offset.checked_add(u64::from(request.extent.length));
  let range_contains_extent = match (receipt_end, extent_end) {
    (Some(receipt_end), Some(extent_end)) => {
      snapshot.receipt_reclaimed_length != 0 && snapshot.receipt_reclaimed_offset <= request.extent.offset && receipt_end >= extent_end
    }
    _ => false,
  };
  if !identity_exact
    || !range_contains_extent
    || !snapshot.exact_proposal_receipt_current
    || !snapshot.locator_removal_durable
    || !snapshot.replacement_lineage_complete
    || !snapshot.receipt_search_complete
    || snapshot.conflicting_receipt_count != 0
    || !snapshot.repair_latch_clear
  {
    return Err(VoidReusableStateErrorV1::invalid(
      "void_runtime_receipt_authority_incomplete",
      "selected free extent lacks one exact, durable, conflict-free sweep receipt and lineage proof",
    ));
  }
  Ok(())
}

fn copy_extent(extent: VoidExtentRecordV1<'_>) -> Result<VoidReusableExtentV1, VoidReusableStateErrorV1> {
  Ok(VoidReusableExtentV1 {
    offset: extent.offset,
    length: extent.length,
    origin_sweep_proposal_hash: clone_bytes(extent.origin_sweep_proposal_hash)?,
    origin_quarantine_manifest_hash: clone_bytes(extent.origin_quarantine_manifest_hash)?,
    reclaimed_incarnation_digest: clone_bytes(extent.reclaimed_incarnation_digest)?,
    reclaim_commit_sequence: extent.reclaim_commit_sequence,
    void_generation: extent.void_generation,
  })
}

fn clone_bytes(bytes: &[u8]) -> Result<Vec<u8>, std::collections::TryReserveError> {
  let mut copy = Vec::new();
  copy.try_reserve_exact(bytes.len())?;
  copy.extend_from_slice(bytes);
  Ok(copy)
}
