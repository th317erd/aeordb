//! Bounded validation and authority contracts for selected Void catalogs.
//!
//! Publication does not grant allocator authority. A selected catalog remains
//! blocked until the exact sweep receipt is reconciled by the separate receipt
//! owner.

use std::array;
use std::num::TryFromIntError;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::first_authority::{FirstAuthorityPublicationErrorV1, VoidCatalogPublicationReceiptV1};
use super::gc_retirement::{RetirementJournalOwnerErrorV1, RetirementJournalReplacementAdmissionErrorV1};
use super::gc_state::{GcDirectoryRoleV1, GcStateDirectoryEntryV1, GcStateDirectoryV1, MAXIMUM_GC_DIRECTORY_ENTRIES_V1};
use super::gc_sweep_removal::SweepLocatorRemovalCompletionPermitV1;
use super::gc_void::{
  SweepOutcomeClassV1, SweepVoidArtifactV1, VoidCatalogManifestV1, VoidClaimV1, VoidExtentPageV1, decode_sweep_void_artifact,
};
use super::reader::FormatError;
use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

const MAXIMUM_VOID_DIRECTORY_LEVELS_V1: usize = 17;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoidCatalogClosureLimitsV1 {
  pub maximum_support_artifacts: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoidCatalogClosureSummaryV1 {
  pub manifest_key: Vec<u8>,
  pub support_artifact_count: u64,
  pub free_page_count: u64,
  pub free_extent_count: u64,
  pub free_bytes: u64,
  pub outstanding_claim_count: u64,
  pub claimed_bytes: u64,
}

#[derive(Debug, Error)]
pub enum VoidCatalogClosureErrorV1 {
  #[error("Void catalog closure validation was canceled")]
  Canceled,
  #[error("Void catalog closure validator is already failed")]
  Failed,
  #[error("{code}: {message}")]
  Invalid { code: &'static str, message: String },
  #[error("Void catalog support exceeds its admitted artifact limit")]
  ArtifactLimit,
  #[error("Void catalog closure memory admission failed: {0}")]
  Memory(#[from] MemoryCoordinatorError),
  #[error("Void catalog closure allocation failed: {0}")]
  Allocation(#[from] std::collections::TryReserveError),
  #[error("Void catalog closure integer conversion failed: {0}")]
  IntegerConversion(#[from] TryFromIntError),
  #[error("Void catalog closure format failure: {0}")]
  Format(#[from] FormatError),
}

impl VoidCatalogClosureErrorV1 {
  pub fn code(&self) -> &str {
    match self {
      Self::Canceled => "void_closure_canceled",
      Self::Failed => "void_closure_failed",
      Self::Invalid { code, .. } => code,
      Self::ArtifactLimit => "void_closure_artifact_limit",
      Self::Memory(_) => "void_closure_memory",
      Self::Allocation(_) => "void_closure_allocation",
      Self::IntegerConversion(_) => "void_closure_integer_conversion",
      Self::Format(source) => source.code(),
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }
}

#[derive(Debug)]
struct VoidCatalogChildSummaryV1 {
  role: GcDirectoryRoleV1,
  child_hash: Vec<u8>,
  child_generation: u64,
  live_count: u64,
  tombstone_count: u64,
  page_count: u64,
  logical_bytes: u64,
  minimum_page_id: u64,
  maximum_page_id: u64,
  lower_fence: Vec<u8>,
  upper_fence: Vec<u8>,
  accounted_bytes: u64,
}

#[derive(Debug)]
struct ExpectedReclaimedExtentV1 {
  offset: u64,
  length: u32,
  reclaim_commit_sequence: u64,
  incarnation_digest: [u8; 64],
  has_incarnation_digest: bool,
}

#[derive(Debug)]
struct SweepCompletionBindingV1 {
  proposal_hash: Vec<u8>,
  quarantine_manifest_hash: Vec<u8>,
  expected_extents: Box<[ExpectedReclaimedExtentV1]>,
  next_extent: usize,
}

pub struct VoidCatalogClosureValidatorV1<'a> {
  manifest: &'a VoidCatalogManifestV1<'a>,
  algorithm: HashAlgorithm,
  cancellation: CancellationToken,
  maximum_support_artifacts: u64,
  support_artifact_count: u64,
  memory: MemoryReservation,
  free_levels: [Vec<VoidCatalogChildSummaryV1>; MAXIMUM_VOID_DIRECTORY_LEVELS_V1],
  claim_levels: [Vec<VoidCatalogChildSummaryV1>; MAXIMUM_VOID_DIRECTORY_LEVELS_V1],
  free_page_count: u64,
  free_extent_count: u64,
  free_bytes: u64,
  previous_free_end: Option<u64>,
  outstanding_claim_count: u64,
  claimed_bytes: u64,
  previous_claim_id: Vec<u8>,
  sweep_completion: Option<SweepCompletionBindingV1>,
  failed: bool,
}

impl std::fmt::Debug for VoidCatalogClosureValidatorV1<'_> {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("VoidCatalogClosureValidatorV1")
      .field("manifest_key", &hex::encode(&self.manifest.key))
      .field("support_artifact_count", &self.support_artifact_count)
      .field("maximum_support_artifacts", &self.maximum_support_artifacts)
      .field("failed", &self.failed)
      .finish_non_exhaustive()
  }
}

impl<'a> VoidCatalogClosureValidatorV1<'a> {
  pub fn new(
    manifest: &'a VoidCatalogManifestV1<'a>,
    algorithm: HashAlgorithm,
    cancellation: CancellationToken,
    limits: VoidCatalogClosureLimitsV1,
    memory: &MemoryCoordinator,
  ) -> Result<Self, VoidCatalogClosureErrorV1> {
    if limits.maximum_support_artifacts == 0 {
      return Err(VoidCatalogClosureErrorV1::invalid("void_closure_limits", "support artifact limit must be nonzero"));
    }
    if cancellation.is_cancelled() {
      return Err(VoidCatalogClosureErrorV1::Canceled);
    }
    let hash_width = algorithm.hash_length();
    if manifest.database_id.len() != 16
      || manifest.key.len() != hash_width
      || manifest.free_root.len() != hash_width
      || manifest.claim_root.len() != hash_width
    {
      return Err(VoidCatalogClosureErrorV1::invalid(
        "void_closure_manifest_identity",
        "Void catalog database, key, or root width differs from the selected hash profile",
      ));
    }
    Ok(Self {
      manifest,
      algorithm,
      cancellation,
      maximum_support_artifacts: limits.maximum_support_artifacts,
      support_artifact_count: 0,
      memory: memory.reserve(MemoryOwner::GarbageCollection, 0, AdmissionClass::Maintenance)?,
      free_levels: array::from_fn(|_| Vec::new()),
      claim_levels: array::from_fn(|_| Vec::new()),
      free_page_count: 0,
      free_extent_count: 0,
      free_bytes: 0,
      previous_free_end: None,
      outstanding_claim_count: 0,
      claimed_bytes: 0,
      previous_claim_id: Vec::new(),
      sweep_completion: None,
      failed: false,
    })
  }

  /// Bind this closure to the exact newly reclaimed ranges from one completed
  /// sweep. Older copy-on-write extents may remain in the catalog, but every
  /// range attributed to this proposal must match one reclaimed outcome.
  pub fn bind_sweep_completion(&mut self, completion: &SweepLocatorRemovalCompletionPermitV1) -> Result<(), VoidCatalogClosureErrorV1> {
    self.preflight_observation()?;
    let result = self.bind_sweep_completion_inner(completion, None);
    self.latch(result)
  }

  pub(crate) fn bind_sweep_completion_with_incarnation_digests(
    &mut self,
    completion: &SweepLocatorRemovalCompletionPermitV1,
    reclaimed_incarnation_digests: &[Vec<u8>],
  ) -> Result<(), VoidCatalogClosureErrorV1> {
    self.preflight_observation()?;
    let result = self.bind_sweep_completion_inner(completion, Some(reclaimed_incarnation_digests));
    self.latch(result)
  }

  pub fn observe_encoded(&mut self, bytes: &[u8]) -> Result<(), VoidCatalogClosureErrorV1> {
    self.preflight_observation()?;
    let result = self.observe_encoded_inner(bytes);
    self.latch(result)
  }

  pub fn finish(self) -> Result<VoidCatalogClosureSummaryV1, VoidCatalogClosureErrorV1> {
    self.preflight()?;
    self.validate_root(GcDirectoryRoleV1::FreeExtents)?;
    self.validate_root(GcDirectoryRoleV1::Claims)?;
    if self.sweep_completion.as_ref().is_some_and(|binding| binding.next_extent != binding.expected_extents.len()) {
      return Err(VoidCatalogClosureErrorV1::invalid(
        "void_closure_sweep_extents",
        "selected Void catalog omits one or more extents reclaimed by its bound sweep completion",
      ));
    }
    if self.free_extent_count != self.manifest.free_count
      || self.free_bytes != self.manifest.free_bytes
      || self.outstanding_claim_count != self.manifest.claim_count
      || self.claimed_bytes != self.manifest.claimed_bytes
    {
      return Err(VoidCatalogClosureErrorV1::invalid(
        "void_closure_manifest_totals",
        "observed free extents or outstanding claims do not close against the Void catalog totals",
      ));
    }
    let mut manifest_key = Vec::new();
    manifest_key.try_reserve_exact(self.manifest.key.len())?;
    manifest_key.extend_from_slice(&self.manifest.key);
    Ok(VoidCatalogClosureSummaryV1 {
      manifest_key,
      support_artifact_count: self.support_artifact_count,
      free_page_count: self.free_page_count,
      free_extent_count: self.free_extent_count,
      free_bytes: self.free_bytes,
      outstanding_claim_count: self.outstanding_claim_count,
      claimed_bytes: self.claimed_bytes,
    })
  }

  fn observe_encoded_inner(&mut self, bytes: &[u8]) -> Result<(), VoidCatalogClosureErrorV1> {
    self.support_artifact_count = self
      .support_artifact_count
      .checked_add(1)
      .ok_or_else(|| VoidCatalogClosureErrorV1::invalid("void_closure_artifact_count", "support artifact count overflowed"))?;
    if self.support_artifact_count > self.maximum_support_artifacts {
      return Err(VoidCatalogClosureErrorV1::ArtifactLimit);
    }
    match decode_sweep_void_artifact(bytes, self.algorithm)? {
      SweepVoidArtifactV1::VoidExtentPage(page) => self.observe_extent_page(&page),
      SweepVoidArtifactV1::VoidClaim(claim) => self.observe_claim(&claim),
      SweepVoidArtifactV1::VoidDirectory(directory) => self.observe_directory(&directory),
      SweepVoidArtifactV1::SweepProposal(_)
      | SweepVoidArtifactV1::SweepReceipt(_)
      | SweepVoidArtifactV1::VoidCatalog(_)
      | SweepVoidArtifactV1::VoidClaimSettlement(_) => Err(VoidCatalogClosureErrorV1::invalid(
        "void_closure_artifact_kind",
        "Void catalog support accepts only extent pages, outstanding claims, and their directories",
      )),
    }
  }

  fn bind_sweep_completion_inner(
    &mut self,
    completion: &SweepLocatorRemovalCompletionPermitV1,
    reclaimed_incarnation_digests: Option<&[Vec<u8>]>,
  ) -> Result<(), VoidCatalogClosureErrorV1> {
    if self.sweep_completion.is_some() {
      return Err(VoidCatalogClosureErrorV1::invalid(
        "void_closure_sweep_binding",
        "Void catalog closure is already bound to a sweep completion",
      ));
    }
    if completion.hash_algorithm() != self.algorithm
      || completion.database_id().as_slice() != self.manifest.database_id
      || completion.proposal_hash().len() != self.algorithm.hash_length()
      || completion.quarantine_manifest_hash().len() != self.algorithm.hash_length()
    {
      return Err(VoidCatalogClosureErrorV1::invalid(
        "void_closure_sweep_identity",
        "sweep completion and Void catalog differ in hash profile, database, generation, or hash width",
      ));
    }

    let reclaimed_count = completion.outcomes().iter().filter(|outcome| outcome.outcome == SweepOutcomeClassV1::Reclaimed).count();
    if reclaimed_count == 0 {
      return Err(VoidCatalogClosureErrorV1::invalid(
        "void_closure_sweep_empty",
        "selector publication requires at least one newly reclaimed extent",
      ));
    }
    if reclaimed_incarnation_digests
      .is_some_and(|digests| digests.len() != reclaimed_count || digests.iter().any(|digest| digest.len() != self.algorithm.hash_length()))
    {
      return Err(VoidCatalogClosureErrorV1::invalid(
        "void_closure_sweep_incarnations",
        "reclaimed incarnation digests do not exactly cover the sweep's reclaimed outcomes",
      ));
    }
    let accounted_bytes = reclaimed_count
      .checked_mul(std::mem::size_of::<ExpectedReclaimedExtentV1>())
      .and_then(|bytes| bytes.checked_add(completion.proposal_hash().len()))
      .and_then(|bytes| bytes.checked_add(completion.quarantine_manifest_hash().len()))
      .ok_or_else(|| VoidCatalogClosureErrorV1::invalid("void_closure_memory_accounting", "sweep-binding size overflowed"))?;
    let accounted_bytes = u64::try_from(accounted_bytes)?;
    self.memory.grow(accounted_bytes)?;

    let mut expected_extents = Vec::new();
    if let Err(source) = expected_extents.try_reserve_exact(reclaimed_count) {
      self.memory.shrink(accounted_bytes)?;
      return Err(source.into());
    }
    let mut reclaimed_index = 0usize;
    for outcome in completion.outcomes() {
      if outcome.outcome == SweepOutcomeClassV1::Reclaimed {
        let mut incarnation_digest = [0u8; 64];
        if let Some(digests) = reclaimed_incarnation_digests {
          incarnation_digest[..self.algorithm.hash_length()].copy_from_slice(&digests[reclaimed_index]);
        }
        expected_extents.push(ExpectedReclaimedExtentV1 {
          offset: outcome.resulting_void_offset,
          length: outcome.resulting_void_length,
          reclaim_commit_sequence: completion.reclaim_commit_sequence(),
          incarnation_digest,
          has_incarnation_digest: reclaimed_incarnation_digests.is_some(),
        });
        reclaimed_index += 1;
      }
    }
    expected_extents.sort_unstable_by_key(|extent| extent.offset);
    if expected_extents.windows(2).any(|pair| pair[0].offset.checked_add(u64::from(pair[0].length)).is_none_or(|end| end > pair[1].offset))
    {
      self.memory.shrink(accounted_bytes)?;
      return Err(VoidCatalogClosureErrorV1::invalid(
        "void_closure_sweep_extents",
        "reclaimed sweep outcomes overlap or overflow after offset ordering",
      ));
    }

    let mut proposal_hash = Vec::new();
    let mut quarantine_manifest_hash = Vec::new();
    if let Err(source) = proposal_hash.try_reserve_exact(completion.proposal_hash().len()) {
      self.memory.shrink(accounted_bytes)?;
      return Err(source.into());
    }
    if let Err(source) = quarantine_manifest_hash.try_reserve_exact(completion.quarantine_manifest_hash().len()) {
      self.memory.shrink(accounted_bytes)?;
      return Err(source.into());
    }
    proposal_hash.extend_from_slice(completion.proposal_hash());
    quarantine_manifest_hash.extend_from_slice(completion.quarantine_manifest_hash());
    self.sweep_completion = Some(SweepCompletionBindingV1 {
      proposal_hash,
      quarantine_manifest_hash,
      expected_extents: expected_extents.into_boxed_slice(),
      next_extent: 0,
    });
    Ok(())
  }

  fn observe_extent_page(&mut self, page: &VoidExtentPageV1<'_>) -> Result<(), VoidCatalogClosureErrorV1> {
    if page.database_id != self.manifest.database_id || page.generation > self.manifest.generation {
      return Err(VoidCatalogClosureErrorV1::invalid(
        "void_closure_free_identity",
        "Void extent page belongs to another database or a future catalog generation",
      ));
    }
    for extent in page.extent_records()? {
      self.check_cancellation()?;
      let extent = extent?;
      if extent.void_generation > page.generation {
        return Err(VoidCatalogClosureErrorV1::invalid(
          "void_closure_free_generation",
          "Void extent provenance names a generation newer than its containing immutable page",
        ));
      }
      if self.previous_free_end.is_some_and(|end| end > extent.offset) {
        return Err(VoidCatalogClosureErrorV1::invalid(
          "void_closure_free_order",
          "Void extents overlap or are out of order across immutable pages",
        ));
      }
      if let Some(binding) = &mut self.sweep_completion {
        if extent.origin_sweep_proposal_hash == binding.proposal_hash {
          let Some(expected) = binding.expected_extents.get(binding.next_extent) else {
            return Err(VoidCatalogClosureErrorV1::invalid(
              "void_closure_sweep_extents",
              "selected Void catalog contains an extra extent attributed to its bound sweep completion",
            ));
          };
          if extent.origin_quarantine_manifest_hash != binding.quarantine_manifest_hash
            || extent.offset != expected.offset
            || extent.length != expected.length
            || extent.reclaim_commit_sequence != expected.reclaim_commit_sequence
            || expected.has_incarnation_digest
              && extent.reclaimed_incarnation_digest != &expected.incarnation_digest[..self.algorithm.hash_length()]
          {
            return Err(VoidCatalogClosureErrorV1::invalid(
              "void_closure_sweep_extents",
              "selected Void extent differs from its bound sweep outcome or quarantine origin",
            ));
          }
          binding.next_extent += 1;
        }
      }
      self.previous_free_end = extent.offset.checked_add(u64::from(extent.length));
      self.free_extent_count = self
        .free_extent_count
        .checked_add(1)
        .ok_or_else(|| VoidCatalogClosureErrorV1::invalid("void_closure_free_count", "free extent count overflowed"))?;
      self.free_bytes = self
        .free_bytes
        .checked_add(u64::from(extent.length))
        .ok_or_else(|| VoidCatalogClosureErrorV1::invalid("void_closure_free_bytes", "free extent bytes overflowed"))?;
    }
    self.free_page_count = self
      .free_page_count
      .checked_add(1)
      .ok_or_else(|| VoidCatalogClosureErrorV1::invalid("void_closure_free_pages", "free extent page count overflowed"))?;
    let summary = VoidCatalogChildSummaryV1 {
      role: GcDirectoryRoleV1::FreeExtents,
      child_hash: page.key.clone(),
      child_generation: page.generation,
      live_count: u64::from(page.record_count),
      tombstone_count: 0,
      page_count: 1,
      logical_bytes: page.total_bytes,
      minimum_page_id: page.page_id,
      maximum_page_id: page.page_id,
      lower_fence: page.lower_offset.to_le_bytes().to_vec(),
      upper_fence: page.upper_offset.to_le_bytes().to_vec(),
      accounted_bytes: 0,
    };
    self.push_summary(0, summary)
  }

  fn observe_claim(&mut self, claim: &VoidClaimV1<'_>) -> Result<(), VoidCatalogClosureErrorV1> {
    if claim.database_id != self.manifest.database_id || claim.generation > self.manifest.generation {
      return Err(VoidCatalogClosureErrorV1::invalid(
        "void_closure_claim_identity",
        "Void claim belongs to another database or a future catalog generation",
      ));
    }
    if !self.previous_claim_id.is_empty() && self.previous_claim_id.as_slice() >= claim.claim_id {
      return Err(VoidCatalogClosureErrorV1::invalid(
        "void_closure_claim_order",
        "Void claims are duplicate or out of order across immutable claim directories",
      ));
    }
    for extent in claim.extent_records()? {
      self.check_cancellation()?;
      extent?;
    }
    self.previous_claim_id.clear();
    self.previous_claim_id.try_reserve_exact(claim.claim_id.len())?;
    self.previous_claim_id.extend_from_slice(claim.claim_id);
    self.outstanding_claim_count = self
      .outstanding_claim_count
      .checked_add(1)
      .ok_or_else(|| VoidCatalogClosureErrorV1::invalid("void_closure_claim_count", "outstanding claim count overflowed"))?;
    self.claimed_bytes = self
      .claimed_bytes
      .checked_add(claim.total_bytes)
      .ok_or_else(|| VoidCatalogClosureErrorV1::invalid("void_closure_claim_bytes", "claimed bytes overflowed"))?;
    let summary = VoidCatalogChildSummaryV1 {
      role: GcDirectoryRoleV1::Claims,
      child_hash: claim.key.clone(),
      child_generation: claim.generation,
      live_count: 1,
      tombstone_count: 0,
      page_count: 0,
      logical_bytes: claim.stored_length,
      minimum_page_id: 0,
      maximum_page_id: 0,
      lower_fence: claim.claim_id.to_vec(),
      upper_fence: claim.claim_id.to_vec(),
      accounted_bytes: 0,
    };
    self.push_summary(0, summary)
  }

  fn observe_directory(&mut self, directory: &GcStateDirectoryV1<'_>) -> Result<(), VoidCatalogClosureErrorV1> {
    if !matches!(directory.role, GcDirectoryRoleV1::FreeExtents | GcDirectoryRoleV1::Claims)
      || directory.database_id != self.manifest.database_id
      || directory.generation > self.manifest.generation
    {
      return Err(VoidCatalogClosureErrorV1::invalid(
        "void_closure_directory_identity",
        "Void directory belongs to another role, database, or a future catalog generation",
      ));
    }
    let level = usize::from(directory.level);
    if level >= MAXIMUM_VOID_DIRECTORY_LEVELS_V1 - 1 {
      return Err(VoidCatalogClosureErrorV1::invalid("void_closure_directory_depth", "Void directory graph exceeds the frozen depth"));
    }
    let levels = self.levels_mut(directory.role)?;
    let children = &levels[level];
    if children.len() != directory.entries.len()
      || children
        .iter()
        .zip(&directory.entries)
        .any(|(child, descriptor)| !child_matches_descriptor(child, descriptor, directory.generation))
    {
      return Err(VoidCatalogClosureErrorV1::invalid(
        "void_closure_directory_children",
        "Void directory was not observed after its exact ordered immutable children",
      ));
    }
    let children = std::mem::take(&mut levels[level]);
    let released_bytes = children.iter().try_fold(0u64, |total, child| {
      total.checked_add(child.accounted_bytes).ok_or_else(|| {
        VoidCatalogClosureErrorV1::invalid("void_closure_memory_accounting", "Void child-summary memory accounting overflowed")
      })
    })?;
    drop(children);
    self.memory.shrink(released_bytes)?;
    let summary = VoidCatalogChildSummaryV1 {
      role: directory.role,
      child_hash: directory.key.clone(),
      child_generation: directory.generation,
      live_count: directory.live_count,
      tombstone_count: directory.tombstone_count,
      page_count: directory.page_count,
      logical_bytes: directory.logical_bytes,
      minimum_page_id: directory.minimum_page_id,
      maximum_page_id: directory.maximum_page_id,
      lower_fence: directory.lower_fence.to_vec(),
      upper_fence: directory.upper_fence.to_vec(),
      accounted_bytes: 0,
    };
    self.push_summary(level + 1, summary)
  }

  fn validate_root(&self, role: GcDirectoryRoleV1) -> Result<(), VoidCatalogClosureErrorV1> {
    let (root, expected_count, expected_bytes, expected_pages) = match role {
      GcDirectoryRoleV1::FreeExtents => (self.manifest.free_root, self.manifest.free_count, self.manifest.free_bytes, self.free_page_count),
      GcDirectoryRoleV1::Claims => (self.manifest.claim_root, self.manifest.claim_count, 0, 0),
      _ => {
        return Err(VoidCatalogClosureErrorV1::invalid(
          "void_closure_root_role",
          "Void closure root validation received a non-Void directory role",
        ));
      }
    };
    let levels = self.levels(role)?;
    let mut summaries = levels.iter().flatten();
    if root.iter().all(|byte| *byte == 0) {
      if summaries.next().is_some() || expected_count != 0 || expected_bytes != 0 || expected_pages != 0 {
        return Err(VoidCatalogClosureErrorV1::invalid(
          "void_closure_empty_root",
          "empty Void root has observed support or nonzero aggregates",
        ));
      }
      return Ok(());
    }
    let Some(summary) = summaries.next() else {
      return Err(VoidCatalogClosureErrorV1::invalid("void_closure_root_missing", "populated Void root was not observed"));
    };
    if summaries.next().is_some()
      || summary.role != role
      || summary.child_hash != root
      || summary.child_generation > self.manifest.generation
      || summary.live_count != expected_count
      || summary.tombstone_count != 0
      || summary.page_count != expected_pages
      || role == GcDirectoryRoleV1::FreeExtents && summary.logical_bytes != expected_bytes
    {
      return Err(VoidCatalogClosureErrorV1::invalid(
        "void_closure_root_mismatch",
        "observed Void root identity or aggregates differ from its manifest",
      ));
    }
    Ok(())
  }

  fn push_summary(&mut self, level: usize, mut summary: VoidCatalogChildSummaryV1) -> Result<(), VoidCatalogClosureErrorV1> {
    let accounted_bytes = std::mem::size_of::<VoidCatalogChildSummaryV1>()
      .checked_add(summary.child_hash.capacity())
      .and_then(|bytes| bytes.checked_add(summary.lower_fence.capacity()))
      .and_then(|bytes| bytes.checked_add(summary.upper_fence.capacity()))
      .ok_or_else(|| VoidCatalogClosureErrorV1::invalid("void_closure_memory_accounting", "Void child-summary size overflowed"))?;
    summary.accounted_bytes = u64::try_from(accounted_bytes)?;
    self.memory.grow(summary.accounted_bytes)?;
    let levels = self.levels_mut(summary.role)?;
    let Some(pending) = levels.get_mut(level) else {
      self.memory.shrink(summary.accounted_bytes)?;
      return Err(VoidCatalogClosureErrorV1::invalid("void_closure_directory_depth", "Void directory graph exceeds the frozen depth"));
    };
    if pending.len() >= MAXIMUM_GC_DIRECTORY_ENTRIES_V1 as usize {
      self.memory.shrink(summary.accounted_bytes)?;
      return Err(VoidCatalogClosureErrorV1::invalid(
        "void_closure_directory_children",
        "Void directory level exceeds the frozen pending-child bound",
      ));
    }
    if let Err(error) = pending.try_reserve_exact(1) {
      self.memory.shrink(summary.accounted_bytes)?;
      return Err(error.into());
    }
    pending.push(summary);
    Ok(())
  }

  fn levels(
    &self,
    role: GcDirectoryRoleV1,
  ) -> Result<&[Vec<VoidCatalogChildSummaryV1>; MAXIMUM_VOID_DIRECTORY_LEVELS_V1], VoidCatalogClosureErrorV1> {
    match role {
      GcDirectoryRoleV1::FreeExtents => Ok(&self.free_levels),
      GcDirectoryRoleV1::Claims => Ok(&self.claim_levels),
      _ => Err(VoidCatalogClosureErrorV1::invalid("void_closure_role", "requested levels for a non-Void role")),
    }
  }

  fn levels_mut(
    &mut self,
    role: GcDirectoryRoleV1,
  ) -> Result<&mut [Vec<VoidCatalogChildSummaryV1>; MAXIMUM_VOID_DIRECTORY_LEVELS_V1], VoidCatalogClosureErrorV1> {
    match role {
      GcDirectoryRoleV1::FreeExtents => Ok(&mut self.free_levels),
      GcDirectoryRoleV1::Claims => Ok(&mut self.claim_levels),
      _ => Err(VoidCatalogClosureErrorV1::invalid("void_closure_role", "requested levels for a non-Void role")),
    }
  }

  fn preflight(&self) -> Result<(), VoidCatalogClosureErrorV1> {
    if self.failed {
      return Err(VoidCatalogClosureErrorV1::Failed);
    }
    self.check_cancellation()?;
    self.memory.check_admission()?;
    Ok(())
  }

  fn preflight_observation(&mut self) -> Result<(), VoidCatalogClosureErrorV1> {
    if let Err(error) = self.preflight() {
      self.failed = true;
      return Err(error);
    }
    Ok(())
  }

  fn latch(&mut self, result: Result<(), VoidCatalogClosureErrorV1>) -> Result<(), VoidCatalogClosureErrorV1> {
    match result {
      Ok(()) => Ok(()),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }

  fn check_cancellation(&self) -> Result<(), VoidCatalogClosureErrorV1> {
    if self.cancellation.is_cancelled() {
      Err(VoidCatalogClosureErrorV1::Canceled)
    } else {
      Ok(())
    }
  }
}

fn child_matches_descriptor(child: &VoidCatalogChildSummaryV1, descriptor: &GcStateDirectoryEntryV1<'_>, parent_generation: u64) -> bool {
  child.child_generation <= parent_generation
    && child.child_hash == descriptor.child_hash
    && child.child_generation == descriptor.child_generation
    && child.live_count == descriptor.live_count
    && child.tombstone_count == descriptor.tombstone_count
    && child.page_count == descriptor.page_count
    && child.logical_bytes == descriptor.logical_bytes
    && child.minimum_page_id == descriptor.minimum_page_id
    && child.maximum_page_id == descriptor.maximum_page_id
    && child.lower_fence == descriptor.lower_fence
    && child.upper_fence == descriptor.upper_fence
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoidCatalogPublicationAuthoritySnapshotV1 {
  pub selected_prior_manifest_hash: Option<Vec<u8>>,
  pub selected_prior_control_sequence: u64,
  pub exact_locator_removal_completion_current: bool,
  pub prior_free_extents_preserved: bool,
  pub prior_outstanding_claims_preserved: bool,
  pub no_unexplained_free_extents_added: bool,
  pub allocator_admission_blocked: bool,
  pub receipt_reconciliation_required: bool,
  pub conflicting_receipt_count: u32,
  pub repair_latch_clear: bool,
}

#[derive(Clone, Copy)]
pub struct VoidCatalogPublicationAuthorityRequestV1<'a> {
  pub completion: &'a SweepLocatorRemovalCompletionPermitV1,
  pub manifest: &'a VoidCatalogManifestV1<'a>,
  pub closure: &'a VoidCatalogClosureSummaryV1,
  pub selected_prior_manifest_hash: Option<&'a [u8]>,
  pub selected_prior_control_sequence: u64,
  pub cancellation: &'a CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct VoidCatalogPublicationAuthorityErrorV1 {
  code: String,
  message: String,
}

impl VoidCatalogPublicationAuthorityErrorV1 {
  pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
    Self { code: code.into(), message: message.into() }
  }

  pub fn code(&self) -> &str {
    if self.code.is_empty() {
      "void_publication_authority"
    } else {
      self.code.as_str()
    }
  }
}

pub trait VoidCatalogPublicationAuthorityV1 {
  /// Recheck sweep completion, prior Void authority, receipt state, repair
  /// state, and allocator exclusion while first authority is held.
  fn recheck_void_catalog_publication_authority(
    &mut self,
    request: VoidCatalogPublicationAuthorityRequestV1<'_>,
  ) -> Result<VoidCatalogPublicationAuthoritySnapshotV1, VoidCatalogPublicationAuthorityErrorV1>;
}

#[derive(Debug, Error)]
pub enum VoidCatalogPublicationErrorV1 {
  #[error("{code}: {message}")]
  Invalid { code: &'static str, message: String },
  #[error("{code}: selected Void catalog committed, but post-commit handling failed: {message}")]
  Committed { code: &'static str, message: String, receipt: Box<VoidCatalogPublicationReceiptV1> },
  #[error("Void catalog closure failure: {0}")]
  Closure(#[from] VoidCatalogClosureErrorV1),
  #[error("Void catalog authority recheck failed: {0}")]
  AuthorityRecheck(#[from] VoidCatalogPublicationAuthorityErrorV1),
  #[error("Void catalog first-authority failure: {0}")]
  Authority(#[from] FirstAuthorityPublicationErrorV1),
  #[error("Void catalog retirement-lineage admission failed: {0}")]
  RetirementAdmission(#[from] RetirementJournalReplacementAdmissionErrorV1),
  #[error("Void catalog retirement-lineage owner failed: {0}")]
  RetirementOwner(#[from] RetirementJournalOwnerErrorV1),
  #[error("Void catalog format failure: {0}")]
  Format(#[from] FormatError),
  #[error("Void catalog allocation failed: {0}")]
  Allocation(#[from] std::collections::TryReserveError),
}

impl VoidCatalogPublicationErrorV1 {
  pub fn code(&self) -> &str {
    match self {
      Self::Invalid { code, .. } | Self::Committed { code, .. } => code,
      Self::Closure(source) => source.code(),
      Self::AuthorityRecheck(source) => source.code(),
      Self::Authority(source) => source.code(),
      Self::RetirementAdmission(source) => source.code(),
      Self::RetirementOwner(source) => source.code(),
      Self::Format(source) => source.code(),
      Self::Allocation(_) => "void_publication_allocation",
    }
  }

  pub(crate) fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }

  pub(crate) fn committed(code: &'static str, message: impl Into<String>, receipt: VoidCatalogPublicationReceiptV1) -> Self {
    Self::Committed { code, message: message.into(), receipt: Box::new(receipt) }
  }

  pub fn committed_receipt(&self) -> Option<&VoidCatalogPublicationReceiptV1> {
    match self {
      Self::Committed { receipt, .. } => Some(receipt),
      Self::Invalid { .. }
      | Self::Closure(_)
      | Self::AuthorityRecheck(_)
      | Self::Authority(_)
      | Self::RetirementAdmission(_)
      | Self::RetirementOwner(_)
      | Self::Format(_)
      | Self::Allocation(_) => None,
    }
  }
}

pub(crate) fn validate_void_catalog_publication_authority_v1(
  request: VoidCatalogPublicationAuthorityRequestV1<'_>,
  snapshot: &VoidCatalogPublicationAuthoritySnapshotV1,
) -> Result<(), VoidCatalogPublicationErrorV1> {
  if request.selected_prior_control_sequence != request.manifest.previous_control_sequence
    || snapshot.selected_prior_control_sequence != request.selected_prior_control_sequence
    || snapshot.selected_prior_manifest_hash.as_deref() != request.selected_prior_manifest_hash
    || request.manifest.previous_control_sequence == 0 && request.selected_prior_manifest_hash.is_some()
    || request.manifest.previous_control_sequence != 0 && request.selected_prior_manifest_hash.is_none()
  {
    return Err(VoidCatalogPublicationErrorV1::invalid(
      "void_publication_prior_authority",
      "selected prior Void authority differs from the proposed manifest predecessor",
    ));
  }
  if !snapshot.exact_locator_removal_completion_current
    || !snapshot.prior_free_extents_preserved
    || !snapshot.prior_outstanding_claims_preserved
    || !snapshot.no_unexplained_free_extents_added
    || !snapshot.allocator_admission_blocked
    || !snapshot.receipt_reconciliation_required
    || snapshot.conflicting_receipt_count != 0
    || !snapshot.repair_latch_clear
  {
    return Err(VoidCatalogPublicationErrorV1::invalid(
      "void_publication_authority_changed",
      "sweep completion, prior catalog, allocator, receipt, or repair authority is incomplete",
    ));
  }
  if request.cancellation.is_cancelled() {
    return Err(VoidCatalogPublicationErrorV1::invalid("void_publication_canceled", "Void catalog publication was canceled"));
  }
  Ok(())
}
