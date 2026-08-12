use std::cmp::Ordering;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::gc::{
  EncodedImmutableGcArtifactV1, GcArtifactKindV1, ImmutableGcArtifactWriteV1, decode_gc_artifact_envelope, encode_immutable_gc_artifact,
  immutable_gc_artifact_key, u16_at, u64_at,
};
use super::gc_state::{
  GcDirectoryRoleV1, GcStateDirectoryEntryV1, GcStateDirectoryV1, GcStatePageV1, RootCandidateRecordV1, RootExpiryRecordV1,
  RootExpiryStateV1, decode_gc_state_artifact, decode_root_candidate_record_v1, decode_root_expiry_record_v1,
};
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::HashAlgorithm;

const ROOT_LIFECYCLE_CAPABILITIES: &[usize] = &[12, 17];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootExpiryManifestV1<'a> {
  pub database_id: &'a [u8],
  pub generation: u64,
  pub retention_ms: u64,
  pub optional_byte_budget: u64,
  pub directory_root_hash: Option<&'a [u8]>,
  pub next_page_id: u64,
  pub record_count: u64,
  pub logical_bytes: u64,
  pub mandatory_count: u64,
  pub mandatory_bytes: u64,
  pub optional_count: u64,
  pub optional_bytes: u64,
  pub oldest_retired_at_ms: Option<i64>,
  pub newest_retired_at_ms: Option<i64>,
  pub key: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootLifecycleManifestV1<'a> {
  pub database_id: &'a [u8],
  pub generation: u64,
  pub published_at_ms: i64,
  pub source_complete_mark_generation: u64,
  pub authority_root_set_digest: &'a [u8],
  pub candidate_directory_hash: Option<&'a [u8]>,
  pub root_expiry_manifest_hash: Option<&'a [u8]>,
  pub next_page_id: u64,
  pub candidate_count: u64,
  pub pending_count: u64,
  pub retired_evidence_count: u64,
  pub candidate_bytes: u64,
  pub expiry_bytes: u64,
  pub key: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootRetirementCommitV1<'a> {
  pub database_id: &'a [u8],
  pub namespace_root_hash: &'a [u8],
  pub retirement_id: &'a [u8],
  pub committed_at_ms: i64,
  pub pending_since_ms: i64,
  pub grace_at_pending_ms: u64,
  pub final_mark_generation: u64,
  pub reason: u16,
  pub prior_lifecycle_manifest_hash: &'a [u8],
  pub authority_root_set_digest: &'a [u8],
  pub admission_commit_payload_hash: &'a [u8],
  pub key: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootObjectReclaimProofV1<'a> {
  pub database_id: &'a [u8],
  pub namespace_root_hash: &'a [u8],
  pub proof_id: &'a [u8],
  pub generation: u64,
  pub retirement_commit_hash: &'a [u8],
  pub reclaimed_at_ms: i64,
  pub physical_inventory_manifest_hash: &'a [u8],
  pub root_object_incarnation_digest: &'a [u8],
  pub root_object_incarnation_count: u64,
  pub sweep_receipt_merkle_root: &'a [u8],
  pub sweep_receipt_count: u64,
  pub absence_digest: &'a [u8],
  pub key: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootLifecycleModelSummaryV1 {
  pub candidate_catalog_id: Option<[u8; 16]>,
  pub candidate_page_count: u64,
  pub candidate_count: u64,
  pub expiry_catalog_id: Option<[u8; 16]>,
  pub expiry_page_count: u64,
  pub expiry_count: u64,
  pub mandatory_expiry_count: u64,
  pub optional_expiry_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootLifecycleSupportClosureV1 {
  hash_algorithm: HashAlgorithm,
  database_id: [u8; 16],
  lifecycle_manifest_hash: Vec<u8>,
  expiry_manifest_hash: Option<Vec<u8>>,
  candidate_directory_hash: Option<Vec<u8>>,
  expiry_directory_hash: Option<Vec<u8>>,
  lifecycle_generation: u64,
  source_complete_mark_generation: u64,
  support_artifact_count: u64,
  retirement_commit_hash: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootLifecycleSupportLimitsV1 {
  pub maximum_candidate_records: u64,
  pub maximum_expiry_records: u64,
  pub maximum_support_artifacts: u64,
}

impl RootLifecycleSupportClosureV1 {
  pub const fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }

  pub const fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  pub fn lifecycle_manifest_hash(&self) -> &[u8] {
    &self.lifecycle_manifest_hash
  }

  pub fn expiry_manifest_hash(&self) -> Option<&[u8]> {
    self.expiry_manifest_hash.as_deref()
  }

  pub fn candidate_directory_hash(&self) -> Option<&[u8]> {
    self.candidate_directory_hash.as_deref()
  }

  pub fn expiry_directory_hash(&self) -> Option<&[u8]> {
    self.expiry_directory_hash.as_deref()
  }

  pub const fn lifecycle_generation(&self) -> u64 {
    self.lifecycle_generation
  }

  pub const fn source_complete_mark_generation(&self) -> u64 {
    self.source_complete_mark_generation
  }

  pub const fn support_artifact_count(&self) -> u64 {
    self.support_artifact_count
  }

  pub fn retirement_commit_hash(&self) -> Option<&[u8]> {
    self.retirement_commit_hash.as_deref()
  }
}

#[derive(Debug, Error)]
pub enum RootLifecycleSupportClosureErrorV1 {
  #[error("root-lifecycle support closure configuration is invalid: {0}")]
  InvalidConfiguration(&'static str),
  #[error("root-lifecycle support closure was canceled")]
  Canceled,
  #[error("root-lifecycle support closure exceeded its artifact limit")]
  ArtifactLimit,
  #[error("root-lifecycle support closure contains an unsupported artifact")]
  ArtifactKind,
  #[error("root-lifecycle support artifacts are not in child-before-parent order")]
  ArtifactOrder,
  #[error("root-lifecycle support directory does not describe the exact observed children")]
  DirectoryClosure,
  #[error("root-lifecycle support closure does not end at its manifest roots")]
  ManifestClosure,
  #[error("root-lifecycle support closure does not prove the exact requested retirement")]
  RetirementClosure,
  #[error(transparent)]
  Model(#[from] RootLifecycleModelErrorV1),
  #[error(transparent)]
  Format(#[from] FormatError),
  #[error(transparent)]
  Memory(#[from] MemoryCoordinatorError),
  #[error("root-lifecycle support closure validator has already failed")]
  Failed,
}

impl RootLifecycleSupportClosureErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::InvalidConfiguration(_) => "root_lifecycle_support_configuration",
      Self::Canceled => "root_lifecycle_support_canceled",
      Self::ArtifactLimit => "root_lifecycle_support_artifact_limit",
      Self::ArtifactKind => "root_lifecycle_support_artifact_kind",
      Self::ArtifactOrder => "root_lifecycle_support_artifact_order",
      Self::DirectoryClosure => "root_lifecycle_support_directory_closure",
      Self::ManifestClosure => "root_lifecycle_support_manifest_closure",
      Self::RetirementClosure => "root_lifecycle_support_retirement_closure",
      Self::Model(source) => source.code(),
      Self::Format(source) => source.code(),
      Self::Memory(_) => "root_lifecycle_support_memory",
      Self::Failed => "root_lifecycle_support_failed",
    }
  }
}

struct RootLifecycleChildSummaryV1 {
  lower_fence: Vec<u8>,
  upper_fence: Vec<u8>,
  child_hash: Vec<u8>,
  child_generation: u64,
  live_count: u64,
  tombstone_count: u64,
  page_count: u64,
  logical_bytes: u64,
  minimum_page_id: u64,
  maximum_page_id: u64,
  accounted_bytes: u64,
}

struct RootLifecycleRoleClosureV1 {
  levels: [Vec<RootLifecycleChildSummaryV1>; 17],
  expected_root: Option<Vec<u8>>,
  observed_root: bool,
}

impl RootLifecycleRoleClosureV1 {
  fn new(expected_root: Option<&[u8]>) -> Self {
    Self { levels: std::array::from_fn(|_| Vec::new()), expected_root: expected_root.map(ToOwned::to_owned), observed_root: false }
  }
}

pub struct RootLifecycleSupportClosureBuilderV1<'a> {
  manifest: &'a RootLifecycleManifestV1<'a>,
  expiry_manifest: Option<&'a RootExpiryManifestV1<'a>>,
  retirement: Option<&'a RootRetirementCommitV1<'a>>,
  algorithm: HashAlgorithm,
  model: Option<RootLifecycleReferenceModelV1<'a>>,
  candidate: RootLifecycleRoleClosureV1,
  expiry: RootLifecycleRoleClosureV1,
  cancellation: &'a CancellationToken,
  maximum_support_artifacts: u64,
  support_artifact_count: u64,
  retirement_expiry_match: bool,
  memory: MemoryReservation,
  failed: bool,
}

impl<'a> RootLifecycleSupportClosureBuilderV1<'a> {
  pub fn new(
    manifest: &'a RootLifecycleManifestV1<'a>,
    expiry_manifest: Option<&'a RootExpiryManifestV1<'a>>,
    algorithm: HashAlgorithm,
    cancellation: &'a CancellationToken,
    limits: RootLifecycleSupportLimitsV1,
    memory: &MemoryCoordinator,
  ) -> Result<Self, RootLifecycleSupportClosureErrorV1> {
    Self::new_inner(manifest, expiry_manifest, None, algorithm, cancellation, limits, memory)
  }

  pub fn new_for_retirement(
    manifest: &'a RootLifecycleManifestV1<'a>,
    expiry_manifest: &'a RootExpiryManifestV1<'a>,
    retirement: &'a RootRetirementCommitV1<'a>,
    algorithm: HashAlgorithm,
    cancellation: &'a CancellationToken,
    limits: RootLifecycleSupportLimitsV1,
    memory: &MemoryCoordinator,
  ) -> Result<Self, RootLifecycleSupportClosureErrorV1> {
    Self::new_inner(manifest, Some(expiry_manifest), Some(retirement), algorithm, cancellation, limits, memory)
  }

  fn new_inner(
    manifest: &'a RootLifecycleManifestV1<'a>,
    expiry_manifest: Option<&'a RootExpiryManifestV1<'a>>,
    retirement: Option<&'a RootRetirementCommitV1<'a>>,
    algorithm: HashAlgorithm,
    cancellation: &'a CancellationToken,
    limits: RootLifecycleSupportLimitsV1,
    memory: &MemoryCoordinator,
  ) -> Result<Self, RootLifecycleSupportClosureErrorV1> {
    if limits.maximum_support_artifacts == 0 {
      return Err(RootLifecycleSupportClosureErrorV1::InvalidConfiguration("support artifact limit must be nonzero"));
    }
    if manifest.database_id.len() != 16 {
      return Err(RootLifecycleSupportClosureErrorV1::InvalidConfiguration("lifecycle database identity has the wrong width"));
    }
    if cancellation.is_cancelled() {
      return Err(RootLifecycleSupportClosureErrorV1::Canceled);
    }
    if retirement.is_some_and(|value| {
      value.database_id != manifest.database_id
        || value.final_mark_generation != manifest.source_complete_mark_generation
        || value.authority_root_set_digest != manifest.authority_root_set_digest
        || value.committed_at_ms > manifest.published_at_ms
    }) {
      return Err(RootLifecycleSupportClosureErrorV1::RetirementClosure);
    }
    let model = RootLifecycleReferenceModelV1::new(
      manifest,
      expiry_manifest,
      algorithm,
      cancellation,
      limits.maximum_candidate_records,
      limits.maximum_expiry_records,
    )?;
    let expiry_directory_hash = expiry_manifest.and_then(|value| value.directory_root_hash);
    Ok(Self {
      manifest,
      expiry_manifest,
      retirement,
      algorithm,
      model: Some(model),
      candidate: RootLifecycleRoleClosureV1::new(manifest.candidate_directory_hash),
      expiry: RootLifecycleRoleClosureV1::new(expiry_directory_hash),
      cancellation,
      maximum_support_artifacts: limits.maximum_support_artifacts,
      support_artifact_count: 0,
      retirement_expiry_match: false,
      memory: memory.reserve(MemoryOwner::GarbageCollection, 0, AdmissionClass::Maintenance)?,
      failed: false,
    })
  }

  pub fn observe_encoded(&mut self, bytes: &[u8]) -> Result<(), RootLifecycleSupportClosureErrorV1> {
    if self.failed {
      return Err(RootLifecycleSupportClosureErrorV1::Failed);
    }
    match self.observe_encoded_inner(bytes) {
      Ok(()) => Ok(()),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }

  pub fn finish(mut self) -> Result<RootLifecycleSupportClosureV1, RootLifecycleSupportClosureErrorV1> {
    if self.failed {
      return Err(RootLifecycleSupportClosureErrorV1::Failed);
    }
    if self.cancellation.is_cancelled() {
      return Err(RootLifecycleSupportClosureErrorV1::Canceled);
    }
    let model = self.model.take().ok_or(RootLifecycleSupportClosureErrorV1::Failed)?;
    let _summary = model.finish()?;
    require_role_finished(&self.candidate)?;
    require_role_finished(&self.expiry)?;
    if self.retirement.is_some() && !self.retirement_expiry_match {
      return Err(RootLifecycleSupportClosureErrorV1::RetirementClosure);
    }
    let mut database_id = [0u8; 16];
    database_id.copy_from_slice(self.manifest.database_id);
    Ok(RootLifecycleSupportClosureV1 {
      hash_algorithm: self.algorithm,
      database_id,
      lifecycle_manifest_hash: self.manifest.key.clone(),
      expiry_manifest_hash: self.expiry_manifest.map(|value| value.key.clone()),
      candidate_directory_hash: self.manifest.candidate_directory_hash.map(ToOwned::to_owned),
      expiry_directory_hash: self.expiry_manifest.and_then(|value| value.directory_root_hash).map(ToOwned::to_owned),
      lifecycle_generation: self.manifest.generation,
      source_complete_mark_generation: self.manifest.source_complete_mark_generation,
      support_artifact_count: self.support_artifact_count,
      retirement_commit_hash: self.retirement.map(|value| value.key.clone()),
    })
  }

  fn observe_encoded_inner(&mut self, bytes: &[u8]) -> Result<(), RootLifecycleSupportClosureErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(RootLifecycleSupportClosureErrorV1::Canceled);
    }
    if self.support_artifact_count >= self.maximum_support_artifacts {
      return Err(RootLifecycleSupportClosureErrorV1::ArtifactLimit);
    }
    self.memory.check_admission()?;
    let artifact = decode_gc_state_artifact(bytes, self.algorithm)?;
    match artifact {
      super::gc_state::GcStateArtifactV1::Page(page) => {
        match page.role {
          GcDirectoryRoleV1::RootCandidates => {
            self.model_mut()?.observe_candidate_page(&page)?;
            self.observe_retirement_candidate_page(&page)?;
          }
          GcDirectoryRoleV1::RootExpiry => {
            self.model_mut()?.observe_expiry_page(&page)?;
            self.observe_retirement_expiry_page(&page)?;
          }
          GcDirectoryRoleV1::Candidates | GcDirectoryRoleV1::PhysicalInventory => {
            return Err(RootLifecycleSupportClosureErrorV1::ArtifactKind);
          }
        }
        let summary = child_summary_from_page(&page)?;
        self.push_summary(page.role, 0, summary)?;
      }
      super::gc_state::GcStateArtifactV1::Directory(directory) => {
        if !matches!(directory.role, GcDirectoryRoleV1::RootCandidates | GcDirectoryRoleV1::RootExpiry) {
          return Err(RootLifecycleSupportClosureErrorV1::ArtifactKind);
        }
        self.observe_directory(&directory)?;
      }
      super::gc_state::GcStateArtifactV1::Manifest(_)
      | super::gc_state::GcStateArtifactV1::CandidateDelta { .. }
      | super::gc_state::GcStateArtifactV1::RootRetirementCommit { .. }
      | super::gc_state::GcStateArtifactV1::RootObjectReclaimProof { .. }
      | super::gc_state::GcStateArtifactV1::RetirementJournal { .. } => {
        return Err(RootLifecycleSupportClosureErrorV1::ArtifactKind);
      }
    }
    self.support_artifact_count = self.support_artifact_count.checked_add(1).ok_or(RootLifecycleSupportClosureErrorV1::ArtifactLimit)?;
    Ok(())
  }

  fn observe_directory(&mut self, directory: &GcStateDirectoryV1<'_>) -> Result<(), RootLifecycleSupportClosureErrorV1> {
    let level = usize::from(directory.level);
    if level >= 17 {
      return Err(RootLifecycleSupportClosureErrorV1::ArtifactOrder);
    }
    let (released_bytes, is_expected_root, root_already_observed) = {
      let role = self.role_mut(directory.role)?;
      let children = &role.levels[level];
      if children.len() != directory.entries.len() {
        return Err(RootLifecycleSupportClosureErrorV1::ArtifactOrder);
      }
      if !children.iter().zip(&directory.entries).all(|(child, descriptor)| child_matches_descriptor(child, descriptor)) {
        return Err(RootLifecycleSupportClosureErrorV1::DirectoryClosure);
      }
      let released_bytes = children
        .iter()
        .try_fold(0u64, |total, child| total.checked_add(child.accounted_bytes))
        .ok_or(RootLifecycleSupportClosureErrorV1::ArtifactLimit)?;
      role.levels[level].clear();
      (released_bytes, role.expected_root.as_deref() == Some(directory.key.as_slice()), role.observed_root)
    };
    self.memory.shrink(released_bytes)?;

    if is_expected_root {
      if root_already_observed {
        return Err(RootLifecycleSupportClosureErrorV1::ManifestClosure);
      }
      match directory.role {
        GcDirectoryRoleV1::RootCandidates => validate_root_lifecycle_candidate_directory(self.manifest, directory)?,
        GcDirectoryRoleV1::RootExpiry => {
          let expiry = self.expiry_manifest.ok_or(RootLifecycleSupportClosureErrorV1::ManifestClosure)?;
          validate_root_expiry_manifest_directory(expiry, directory)?;
        }
        GcDirectoryRoleV1::Candidates | GcDirectoryRoleV1::PhysicalInventory => {
          return Err(RootLifecycleSupportClosureErrorV1::ArtifactKind);
        }
      }
      self.role_mut(directory.role)?.observed_root = true;
    }
    let summary = child_summary_from_directory(directory)?;
    self.push_summary(directory.role, level + 1, summary)
  }

  fn push_summary(
    &mut self,
    role: GcDirectoryRoleV1,
    level: usize,
    summary: RootLifecycleChildSummaryV1,
  ) -> Result<(), RootLifecycleSupportClosureErrorV1> {
    if level >= 17 {
      return Err(RootLifecycleSupportClosureErrorV1::ArtifactOrder);
    }
    self.memory.grow(summary.accounted_bytes)?;
    self.role_mut(role)?.levels[level].push(summary);
    Ok(())
  }

  fn role_mut(&mut self, role: GcDirectoryRoleV1) -> Result<&mut RootLifecycleRoleClosureV1, RootLifecycleSupportClosureErrorV1> {
    match role {
      GcDirectoryRoleV1::RootCandidates => Ok(&mut self.candidate),
      GcDirectoryRoleV1::RootExpiry => Ok(&mut self.expiry),
      GcDirectoryRoleV1::Candidates | GcDirectoryRoleV1::PhysicalInventory => Err(RootLifecycleSupportClosureErrorV1::ArtifactKind),
    }
  }

  fn model_mut(&mut self) -> Result<&mut RootLifecycleReferenceModelV1<'a>, RootLifecycleSupportClosureErrorV1> {
    self.model.as_mut().ok_or(RootLifecycleSupportClosureErrorV1::Failed)
  }

  fn observe_retirement_candidate_page(&self, page: &GcStatePageV1<'_>) -> Result<(), RootLifecycleSupportClosureErrorV1> {
    let Some(retirement) = self.retirement else {
      return Ok(());
    };
    let row_length = 36 + 3 * self.algorithm.hash_length();
    for row in page.records.chunks_exact(row_length) {
      if decode_root_candidate_record_v1(row, self.algorithm)?.namespace_root_hash == retirement.namespace_root_hash {
        return Err(RootLifecycleSupportClosureErrorV1::RetirementClosure);
      }
    }
    Ok(())
  }

  fn observe_retirement_expiry_page(&mut self, page: &GcStatePageV1<'_>) -> Result<(), RootLifecycleSupportClosureErrorV1> {
    let Some(retirement) = self.retirement else {
      return Ok(());
    };
    let row_length = 40 + 3 * self.algorithm.hash_length();
    for row in page.records.chunks_exact(row_length) {
      let record = decode_root_expiry_record_v1(row, self.algorithm)?;
      if record.namespace_root_hash != retirement.namespace_root_hash {
        continue;
      }
      if self.retirement_expiry_match
        || record.state != RootExpiryStateV1::LogicallyRetired
        || record.retired_at_ms != retirement.committed_at_ms
        || record.last_pending_since_ms != retirement.pending_since_ms
      {
        return Err(RootLifecycleSupportClosureErrorV1::RetirementClosure);
      }
      validate_root_expiry_retirement_commit(&record, retirement)?;
      self.retirement_expiry_match = true;
    }
    Ok(())
  }
}

fn child_summary_from_page(page: &GcStatePageV1<'_>) -> Result<RootLifecycleChildSummaryV1, RootLifecycleSupportClosureErrorV1> {
  let accounted_bytes = child_summary_accounted_bytes(page.lower_fence, page.upper_fence, &page.key)?;
  Ok(RootLifecycleChildSummaryV1 {
    lower_fence: page.lower_fence.to_vec(),
    upper_fence: page.upper_fence.to_vec(),
    child_hash: page.key.clone(),
    child_generation: page.generation,
    live_count: u64::from(page.record_count),
    tombstone_count: 0,
    page_count: 1,
    logical_bytes: page.logical_bytes,
    minimum_page_id: page.page_id,
    maximum_page_id: page.page_id,
    accounted_bytes,
  })
}

fn child_summary_from_directory(
  directory: &GcStateDirectoryV1<'_>,
) -> Result<RootLifecycleChildSummaryV1, RootLifecycleSupportClosureErrorV1> {
  let accounted_bytes = child_summary_accounted_bytes(directory.lower_fence, directory.upper_fence, &directory.key)?;
  Ok(RootLifecycleChildSummaryV1 {
    lower_fence: directory.lower_fence.to_vec(),
    upper_fence: directory.upper_fence.to_vec(),
    child_hash: directory.key.clone(),
    child_generation: directory.generation,
    live_count: directory.live_count,
    tombstone_count: directory.tombstone_count,
    page_count: directory.page_count,
    logical_bytes: directory.logical_bytes,
    minimum_page_id: directory.minimum_page_id,
    maximum_page_id: directory.maximum_page_id,
    accounted_bytes,
  })
}

fn child_summary_accounted_bytes(
  lower_fence: &[u8],
  upper_fence: &[u8],
  child_hash: &[u8],
) -> Result<u64, RootLifecycleSupportClosureErrorV1> {
  let bytes = lower_fence
    .len()
    .checked_add(upper_fence.len())
    .and_then(|value| value.checked_add(child_hash.len()))
    .and_then(|value| value.checked_add(std::mem::size_of::<RootLifecycleChildSummaryV1>()))
    .ok_or(RootLifecycleSupportClosureErrorV1::ArtifactLimit)?;
  match u64::try_from(bytes) {
    Ok(bytes) => Ok(bytes),
    Err(error) => Err(artifact_limit_from_size_conversion(error)),
  }
}

fn artifact_limit_from_size_conversion(_source: std::num::TryFromIntError) -> RootLifecycleSupportClosureErrorV1 {
  RootLifecycleSupportClosureErrorV1::ArtifactLimit
}

fn child_matches_descriptor(child: &RootLifecycleChildSummaryV1, descriptor: &GcStateDirectoryEntryV1<'_>) -> bool {
  child.lower_fence == descriptor.lower_fence
    && child.upper_fence == descriptor.upper_fence
    && child.child_hash == descriptor.child_hash
    && child.child_generation == descriptor.child_generation
    && child.live_count == descriptor.live_count
    && child.tombstone_count == descriptor.tombstone_count
    && child.page_count == descriptor.page_count
    && child.logical_bytes == descriptor.logical_bytes
    && child.minimum_page_id == descriptor.minimum_page_id
    && child.maximum_page_id == descriptor.maximum_page_id
}

fn require_role_finished(role: &RootLifecycleRoleClosureV1) -> Result<(), RootLifecycleSupportClosureErrorV1> {
  let mut summaries = role.levels.iter().flatten();
  let first = summaries.next();
  let exactly_one = first.is_some() && summaries.next().is_none();
  match role.expected_root.as_deref() {
    None if !role.observed_root && first.is_none() => Ok(()),
    Some(expected_root) if role.observed_root && exactly_one && first.is_some_and(|summary| summary.child_hash == expected_root) => Ok(()),
    None | Some(_) => Err(RootLifecycleSupportClosureErrorV1::ManifestClosure),
  }
}

#[derive(Debug, Error)]
pub enum RootLifecycleModelErrorV1 {
  #[error("root lifecycle traversal was canceled")]
  Canceled,
  #[error("root lifecycle record limit was exceeded")]
  RecordLimit,
  #[error("root lifecycle page belongs to another database")]
  DatabaseMismatch,
  #[error("root lifecycle pages disagree on catalog identity")]
  CatalogMismatch,
  #[error("root lifecycle records are not strictly ordered")]
  RecordOrder,
  #[error("root lifecycle candidate generation exceeds the source complete mark")]
  GenerationMismatch,
  #[error("root lifecycle counters overflowed")]
  ArithmeticOverflow,
  #[error("root lifecycle page aggregates do not close against the manifests")]
  ManifestAggregate,
  #[error(transparent)]
  Format(#[from] FormatError),
  #[error("root lifecycle model has already failed")]
  Failed,
}

impl RootLifecycleModelErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Canceled => "root_lifecycle_canceled",
      Self::RecordLimit => "root_lifecycle_record_limit",
      Self::DatabaseMismatch => "root_lifecycle_database",
      Self::CatalogMismatch => "root_lifecycle_catalog",
      Self::RecordOrder => "root_lifecycle_record_order",
      Self::GenerationMismatch => "root_lifecycle_generation",
      Self::ArithmeticOverflow => "root_lifecycle_arithmetic",
      Self::ManifestAggregate => "root_lifecycle_manifest_aggregate",
      Self::Format(error) => error.code(),
      Self::Failed => "root_lifecycle_failed",
    }
  }
}

/// Constant-memory validator for the candidate and expiry pages selected by
/// one immutable root-lifecycle manifest closure.
#[derive(Debug)]
pub struct RootLifecycleReferenceModelV1<'a> {
  manifest: &'a RootLifecycleManifestV1<'a>,
  expiry_manifest: Option<&'a RootExpiryManifestV1<'a>>,
  algorithm: HashAlgorithm,
  cancellation: &'a CancellationToken,
  maximum_candidate_records: u64,
  maximum_expiry_records: u64,
  candidate_catalog_id: Option<[u8; 16]>,
  candidate_page_count: u64,
  candidate_count: u64,
  candidate_bytes: u64,
  maximum_candidate_page_id: u64,
  previous_candidate_root: Vec<u8>,
  expiry_catalog_id: Option<[u8; 16]>,
  expiry_page_count: u64,
  expiry_count: u64,
  expiry_bytes: u64,
  maximum_expiry_page_id: u64,
  mandatory_expiry_count: u64,
  mandatory_expiry_bytes: u64,
  optional_expiry_count: u64,
  optional_expiry_bytes: u64,
  oldest_retired_at_ms: Option<i64>,
  newest_retired_at_ms: Option<i64>,
  previous_expiry_root: Vec<u8>,
  failed: bool,
}

impl<'a> RootLifecycleReferenceModelV1<'a> {
  pub fn new(
    manifest: &'a RootLifecycleManifestV1<'a>,
    expiry_manifest: Option<&'a RootExpiryManifestV1<'a>>,
    algorithm: HashAlgorithm,
    cancellation: &'a CancellationToken,
    maximum_candidate_records: u64,
    maximum_expiry_records: u64,
  ) -> Result<Self, RootLifecycleModelErrorV1> {
    if cancellation.is_cancelled() {
      return Err(RootLifecycleModelErrorV1::Canceled);
    }
    if manifest.candidate_count > maximum_candidate_records
      || expiry_manifest.is_some_and(|value| value.record_count > maximum_expiry_records)
    {
      return Err(RootLifecycleModelErrorV1::RecordLimit);
    }
    match (manifest.root_expiry_manifest_hash, expiry_manifest) {
      (Some(_), Some(expiry)) => validate_root_lifecycle_expiry_manifest(manifest, expiry)?,
      (None, None) => {}
      _ => return Err(RootLifecycleModelErrorV1::ManifestAggregate),
    }
    Ok(Self {
      manifest,
      expiry_manifest,
      algorithm,
      cancellation,
      maximum_candidate_records,
      maximum_expiry_records,
      candidate_catalog_id: None,
      candidate_page_count: 0,
      candidate_count: 0,
      candidate_bytes: 0,
      maximum_candidate_page_id: 0,
      previous_candidate_root: Vec::with_capacity(algorithm.hash_length()),
      expiry_catalog_id: None,
      expiry_page_count: 0,
      expiry_count: 0,
      expiry_bytes: 0,
      maximum_expiry_page_id: 0,
      mandatory_expiry_count: 0,
      mandatory_expiry_bytes: 0,
      optional_expiry_count: 0,
      optional_expiry_bytes: 0,
      oldest_retired_at_ms: None,
      newest_retired_at_ms: None,
      previous_expiry_root: Vec::with_capacity(algorithm.hash_length()),
      failed: false,
    })
  }

  pub fn observe_candidate_page(&mut self, page: &GcStatePageV1<'_>) -> Result<(), RootLifecycleModelErrorV1> {
    if self.failed {
      return Err(RootLifecycleModelErrorV1::Failed);
    }
    match self.observe_candidate_page_inner(page) {
      Ok(()) => Ok(()),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }

  pub fn observe_expiry_page(&mut self, page: &GcStatePageV1<'_>) -> Result<(), RootLifecycleModelErrorV1> {
    if self.failed {
      return Err(RootLifecycleModelErrorV1::Failed);
    }
    match self.observe_expiry_page_inner(page) {
      Ok(()) => Ok(()),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }

  pub fn finish(self) -> Result<RootLifecycleModelSummaryV1, RootLifecycleModelErrorV1> {
    if self.failed {
      return Err(RootLifecycleModelErrorV1::Failed);
    }
    if self.cancellation.is_cancelled() {
      return Err(RootLifecycleModelErrorV1::Canceled);
    }
    let candidates_populated = self.manifest.candidate_directory_hash.is_some();
    if self.candidate_count != self.manifest.candidate_count
      || self.candidate_bytes != self.manifest.candidate_bytes
      || candidates_populated != (self.candidate_page_count != 0)
      || candidates_populated != self.candidate_catalog_id.is_some()
      || (candidates_populated && self.maximum_candidate_page_id >= self.manifest.next_page_id)
      || (!candidates_populated && self.maximum_candidate_page_id != 0)
    {
      return Err(RootLifecycleModelErrorV1::ManifestAggregate);
    }
    if let Some(expiry) = self.expiry_manifest {
      let populated = expiry.directory_root_hash.is_some();
      if self.expiry_count != expiry.record_count
        || self.expiry_bytes != expiry.logical_bytes
        || self.mandatory_expiry_count != expiry.mandatory_count
        || self.mandatory_expiry_bytes != expiry.mandatory_bytes
        || self.optional_expiry_count != expiry.optional_count
        || self.optional_expiry_bytes != expiry.optional_bytes
        || self.oldest_retired_at_ms != expiry.oldest_retired_at_ms
        || self.newest_retired_at_ms != expiry.newest_retired_at_ms
        || populated != (self.expiry_page_count != 0)
        || populated != self.expiry_catalog_id.is_some()
        || (populated && self.maximum_expiry_page_id >= expiry.next_page_id)
        || (!populated && self.maximum_expiry_page_id != 0)
      {
        return Err(RootLifecycleModelErrorV1::ManifestAggregate);
      }
    } else if self.expiry_count != 0 || self.expiry_page_count != 0 {
      return Err(RootLifecycleModelErrorV1::ManifestAggregate);
    }
    Ok(RootLifecycleModelSummaryV1 {
      candidate_catalog_id: self.candidate_catalog_id,
      candidate_page_count: self.candidate_page_count,
      candidate_count: self.candidate_count,
      expiry_catalog_id: self.expiry_catalog_id,
      expiry_page_count: self.expiry_page_count,
      expiry_count: self.expiry_count,
      mandatory_expiry_count: self.mandatory_expiry_count,
      optional_expiry_count: self.optional_expiry_count,
    })
  }

  fn observe_candidate_page_inner(&mut self, page: &GcStatePageV1<'_>) -> Result<(), RootLifecycleModelErrorV1> {
    self.validate_page(page, GcDirectoryRoleV1::RootCandidates, self.candidate_catalog_id)?;
    let mut catalog_id = [0u8; 16];
    catalog_id.copy_from_slice(page.catalog_id);
    self.candidate_catalog_id = Some(catalog_id);
    let row_length = 36 + 3 * self.algorithm.hash_length();
    for row in page.records.chunks_exact(row_length) {
      self.check_cancellation_and_limit(self.candidate_count, self.maximum_candidate_records)?;
      let record = decode_root_candidate_record_v1(row, self.algorithm)?;
      if !self.previous_candidate_root.is_empty()
        && self.previous_candidate_root.as_slice().cmp(record.namespace_root_hash) != Ordering::Less
      {
        return Err(RootLifecycleModelErrorV1::RecordOrder);
      }
      if record.last_confirmed_unreachable_generation > self.manifest.source_complete_mark_generation {
        return Err(RootLifecycleModelErrorV1::GenerationMismatch);
      }
      self.previous_candidate_root.clear();
      self.previous_candidate_root.extend_from_slice(record.namespace_root_hash);
      self.candidate_count = self.candidate_count.checked_add(1).ok_or(RootLifecycleModelErrorV1::ArithmeticOverflow)?;
    }
    self.candidate_page_count = self.candidate_page_count.checked_add(1).ok_or(RootLifecycleModelErrorV1::ArithmeticOverflow)?;
    self.candidate_bytes = self.candidate_bytes.checked_add(page.logical_bytes).ok_or(RootLifecycleModelErrorV1::ArithmeticOverflow)?;
    self.maximum_candidate_page_id = self.maximum_candidate_page_id.max(page.page_id);
    Ok(())
  }

  fn observe_expiry_page_inner(&mut self, page: &GcStatePageV1<'_>) -> Result<(), RootLifecycleModelErrorV1> {
    self.validate_page(page, GcDirectoryRoleV1::RootExpiry, self.expiry_catalog_id)?;
    let mut catalog_id = [0u8; 16];
    catalog_id.copy_from_slice(page.catalog_id);
    self.expiry_catalog_id = Some(catalog_id);
    let row_length = 40 + 3 * self.algorithm.hash_length();
    for row in page.records.chunks_exact(row_length) {
      self.check_cancellation_and_limit(self.expiry_count, self.maximum_expiry_records)?;
      let record = decode_root_expiry_record_v1(row, self.algorithm)?;
      if !self.previous_expiry_root.is_empty() && self.previous_expiry_root.as_slice().cmp(record.namespace_root_hash) != Ordering::Less {
        return Err(RootLifecycleModelErrorV1::RecordOrder);
      }
      if record.final_mark_generation > self.manifest.source_complete_mark_generation {
        return Err(RootLifecycleModelErrorV1::GenerationMismatch);
      }
      let row_bytes = row.len() as u64;
      match record.state {
        RootExpiryStateV1::LogicallyRetired => {
          self.mandatory_expiry_count = self.mandatory_expiry_count.checked_add(1).ok_or(RootLifecycleModelErrorV1::ArithmeticOverflow)?;
          self.mandatory_expiry_bytes =
            self.mandatory_expiry_bytes.checked_add(row_bytes).ok_or(RootLifecycleModelErrorV1::ArithmeticOverflow)?;
        }
        RootExpiryStateV1::PhysicallyReclaimed => {
          self.optional_expiry_count = self.optional_expiry_count.checked_add(1).ok_or(RootLifecycleModelErrorV1::ArithmeticOverflow)?;
          self.optional_expiry_bytes =
            self.optional_expiry_bytes.checked_add(row_bytes).ok_or(RootLifecycleModelErrorV1::ArithmeticOverflow)?;
        }
      }
      self.oldest_retired_at_ms = Some(self.oldest_retired_at_ms.map_or(record.retired_at_ms, |value| value.min(record.retired_at_ms)));
      self.newest_retired_at_ms = Some(self.newest_retired_at_ms.map_or(record.retired_at_ms, |value| value.max(record.retired_at_ms)));
      self.previous_expiry_root.clear();
      self.previous_expiry_root.extend_from_slice(record.namespace_root_hash);
      self.expiry_count = self.expiry_count.checked_add(1).ok_or(RootLifecycleModelErrorV1::ArithmeticOverflow)?;
    }
    self.expiry_page_count = self.expiry_page_count.checked_add(1).ok_or(RootLifecycleModelErrorV1::ArithmeticOverflow)?;
    self.expiry_bytes = self.expiry_bytes.checked_add(page.logical_bytes).ok_or(RootLifecycleModelErrorV1::ArithmeticOverflow)?;
    self.maximum_expiry_page_id = self.maximum_expiry_page_id.max(page.page_id);
    Ok(())
  }

  fn validate_page(
    &self,
    page: &GcStatePageV1<'_>,
    expected_role: GcDirectoryRoleV1,
    catalog_id: Option<[u8; 16]>,
  ) -> Result<(), RootLifecycleModelErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(RootLifecycleModelErrorV1::Canceled);
    }
    if page.role != expected_role || page.database_id != self.manifest.database_id {
      return Err(RootLifecycleModelErrorV1::DatabaseMismatch);
    }
    if page.catalog_id.len() != 16 {
      return Err(RootLifecycleModelErrorV1::CatalogMismatch);
    }
    if catalog_id.is_some_and(|expected| page.catalog_id != expected) {
      return Err(RootLifecycleModelErrorV1::CatalogMismatch);
    }
    Ok(())
  }

  fn check_cancellation_and_limit(&self, count: u64, maximum: u64) -> Result<(), RootLifecycleModelErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(RootLifecycleModelErrorV1::Canceled);
    }
    if count >= maximum {
      return Err(RootLifecycleModelErrorV1::RecordLimit);
    }
    Ok(())
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootCandidateRecordWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub namespace_root_hash: &'a [u8],
  pub reason: u16,
  pub pending_since_ms: i64,
  pub first_unreachable_generation: u64,
  pub last_confirmed_unreachable_generation: u64,
  pub grace_at_pending_ms: u64,
  pub authority_root_set_digest: &'a [u8],
  pub admission_commit_payload_hash: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootExpiryRecordWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub namespace_root_hash: &'a [u8],
  pub retired_at_ms: i64,
  pub last_pending_since_ms: i64,
  pub final_mark_generation: u64,
  pub reason: u16,
  pub state: RootExpiryStateV1,
  pub retirement_commit_hash: &'a [u8],
  pub root_object_reclaim_proof_hash: Option<&'a [u8]>,
  pub evidence_expires_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootExpiryManifestWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8],
  pub generation: u64,
  pub retention_ms: u64,
  pub optional_byte_budget: u64,
  pub directory_root_hash: Option<&'a [u8]>,
  pub next_page_id: u64,
  pub record_count: u64,
  pub logical_bytes: u64,
  pub mandatory_count: u64,
  pub mandatory_bytes: u64,
  pub optional_count: u64,
  pub optional_bytes: u64,
  pub oldest_retired_at_ms: Option<i64>,
  pub newest_retired_at_ms: Option<i64>,
}

impl<'a> RootExpiryManifestWriteV1<'a> {
  pub fn from_decoded(hash_algorithm: HashAlgorithm, value: &'a RootExpiryManifestV1<'a>) -> Self {
    Self {
      hash_algorithm,
      database_id: value.database_id,
      generation: value.generation,
      retention_ms: value.retention_ms,
      optional_byte_budget: value.optional_byte_budget,
      directory_root_hash: value.directory_root_hash,
      next_page_id: value.next_page_id,
      record_count: value.record_count,
      logical_bytes: value.logical_bytes,
      mandatory_count: value.mandatory_count,
      mandatory_bytes: value.mandatory_bytes,
      optional_count: value.optional_count,
      optional_bytes: value.optional_bytes,
      oldest_retired_at_ms: value.oldest_retired_at_ms,
      newest_retired_at_ms: value.newest_retired_at_ms,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootLifecycleManifestWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8],
  pub generation: u64,
  pub published_at_ms: i64,
  pub source_complete_mark_generation: u64,
  pub authority_root_set_digest: &'a [u8],
  pub candidate_directory_hash: Option<&'a [u8]>,
  pub root_expiry_manifest_hash: Option<&'a [u8]>,
  pub next_page_id: u64,
  pub candidate_count: u64,
  pub pending_count: u64,
  pub retired_evidence_count: u64,
  pub candidate_bytes: u64,
  pub expiry_bytes: u64,
}

impl<'a> RootLifecycleManifestWriteV1<'a> {
  pub fn from_decoded(hash_algorithm: HashAlgorithm, value: &'a RootLifecycleManifestV1<'a>) -> Self {
    Self {
      hash_algorithm,
      database_id: value.database_id,
      generation: value.generation,
      published_at_ms: value.published_at_ms,
      source_complete_mark_generation: value.source_complete_mark_generation,
      authority_root_set_digest: value.authority_root_set_digest,
      candidate_directory_hash: value.candidate_directory_hash,
      root_expiry_manifest_hash: value.root_expiry_manifest_hash,
      next_page_id: value.next_page_id,
      candidate_count: value.candidate_count,
      pending_count: value.pending_count,
      retired_evidence_count: value.retired_evidence_count,
      candidate_bytes: value.candidate_bytes,
      expiry_bytes: value.expiry_bytes,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootRetirementCommitWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8],
  pub namespace_root_hash: &'a [u8],
  pub retirement_id: &'a [u8],
  pub committed_at_ms: i64,
  pub pending_since_ms: i64,
  pub grace_at_pending_ms: u64,
  pub final_mark_generation: u64,
  pub reason: u16,
  pub prior_lifecycle_manifest_hash: &'a [u8],
  pub authority_root_set_digest: &'a [u8],
  pub admission_commit_payload_hash: &'a [u8],
}

impl<'a> RootRetirementCommitWriteV1<'a> {
  pub fn from_decoded(hash_algorithm: HashAlgorithm, value: &'a RootRetirementCommitV1<'a>) -> Self {
    Self {
      hash_algorithm,
      database_id: value.database_id,
      namespace_root_hash: value.namespace_root_hash,
      retirement_id: value.retirement_id,
      committed_at_ms: value.committed_at_ms,
      pending_since_ms: value.pending_since_ms,
      grace_at_pending_ms: value.grace_at_pending_ms,
      final_mark_generation: value.final_mark_generation,
      reason: value.reason,
      prior_lifecycle_manifest_hash: value.prior_lifecycle_manifest_hash,
      authority_root_set_digest: value.authority_root_set_digest,
      admission_commit_payload_hash: value.admission_commit_payload_hash,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootObjectReclaimProofWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8],
  pub namespace_root_hash: &'a [u8],
  pub proof_id: &'a [u8],
  pub generation: u64,
  pub retirement_commit_hash: &'a [u8],
  pub reclaimed_at_ms: i64,
  pub physical_inventory_manifest_hash: &'a [u8],
  pub root_object_incarnation_digest: &'a [u8],
  pub root_object_incarnation_count: u64,
  pub sweep_receipt_merkle_root: &'a [u8],
  pub sweep_receipt_count: u64,
  pub absence_digest: &'a [u8],
}

impl<'a> RootObjectReclaimProofWriteV1<'a> {
  pub fn from_decoded(hash_algorithm: HashAlgorithm, value: &'a RootObjectReclaimProofV1<'a>) -> Self {
    Self {
      hash_algorithm,
      database_id: value.database_id,
      namespace_root_hash: value.namespace_root_hash,
      proof_id: value.proof_id,
      generation: value.generation,
      retirement_commit_hash: value.retirement_commit_hash,
      reclaimed_at_ms: value.reclaimed_at_ms,
      physical_inventory_manifest_hash: value.physical_inventory_manifest_hash,
      root_object_incarnation_digest: value.root_object_incarnation_digest,
      root_object_incarnation_count: value.root_object_incarnation_count,
      sweep_receipt_merkle_root: value.sweep_receipt_merkle_root,
      sweep_receipt_count: value.sweep_receipt_count,
      absence_digest: value.absence_digest,
    }
  }
}

pub fn decode_root_expiry_manifest_v1(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<RootExpiryManifestV1<'_>> {
  let _validated = decode_gc_state_artifact(bytes, algorithm)?;
  let artifact = decode_gc_artifact_envelope(bytes)?;
  if artifact.kind != GcArtifactKindV1::RootExpiryCatalogManifest {
    return Err(kind_error("root_expiry_manifest_kind", "artifact is not a root-expiry manifest"));
  }
  let hash_width = algorithm.hash_length();
  let body = artifact.body;
  let directory_root = &body[52..52 + hash_width];
  let oldest = i64_at(body, 108 + hash_width)?;
  let newest = i64_at(body, 116 + hash_width)?;
  Ok(RootExpiryManifestV1 {
    database_id: &artifact.identity[..16],
    generation: artifact.generation,
    retention_ms: u64_at(body, 36)?,
    optional_byte_budget: u64_at(body, 44)?,
    directory_root_hash: optional_hash(directory_root),
    next_page_id: u64_at(body, 52 + hash_width)?,
    record_count: u64_at(body, 60 + hash_width)?,
    logical_bytes: u64_at(body, 68 + hash_width)?,
    mandatory_count: u64_at(body, 76 + hash_width)?,
    mandatory_bytes: u64_at(body, 84 + hash_width)?,
    optional_count: u64_at(body, 92 + hash_width)?,
    optional_bytes: u64_at(body, 100 + hash_width)?,
    oldest_retired_at_ms: (oldest != 0).then_some(oldest),
    newest_retired_at_ms: (newest != 0).then_some(newest),
    key: immutable_gc_artifact_key(algorithm, artifact.kind, bytes),
  })
}

pub fn decode_root_lifecycle_manifest_v1(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<RootLifecycleManifestV1<'_>> {
  let _validated = decode_gc_state_artifact(bytes, algorithm)?;
  let artifact = decode_gc_artifact_envelope(bytes)?;
  if artifact.kind != GcArtifactKindV1::RootLifecycleManifest {
    return Err(kind_error("root_lifecycle_manifest_kind", "artifact is not a root-lifecycle manifest"));
  }
  let hash_width = algorithm.hash_length();
  let body = artifact.body;
  Ok(RootLifecycleManifestV1 {
    database_id: &artifact.identity[..16],
    generation: artifact.generation,
    published_at_ms: i64_at(body, 44)?,
    source_complete_mark_generation: u64_at(body, 52)?,
    authority_root_set_digest: &body[60..60 + hash_width],
    candidate_directory_hash: optional_hash(&body[60 + hash_width..60 + 2 * hash_width]),
    root_expiry_manifest_hash: optional_hash(&body[60 + 2 * hash_width..60 + 3 * hash_width]),
    next_page_id: u64_at(body, 60 + 3 * hash_width)?,
    candidate_count: u64_at(body, 68 + 3 * hash_width)?,
    pending_count: u64_at(body, 76 + 3 * hash_width)?,
    retired_evidence_count: u64_at(body, 84 + 3 * hash_width)?,
    candidate_bytes: u64_at(body, 92 + 3 * hash_width)?,
    expiry_bytes: u64_at(body, 100 + 3 * hash_width)?,
    key: immutable_gc_artifact_key(algorithm, artifact.kind, bytes),
  })
}

pub fn decode_root_retirement_commit_v1(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<RootRetirementCommitV1<'_>> {
  let _validated = decode_gc_state_artifact(bytes, algorithm)?;
  let artifact = decode_gc_artifact_envelope(bytes)?;
  if artifact.kind != GcArtifactKindV1::RootRetirementCommit {
    return Err(kind_error("root_retirement_kind", "artifact is not a root-retirement commit"));
  }
  let hash_width = algorithm.hash_length();
  let body = artifact.body;
  Ok(RootRetirementCommitV1 {
    database_id: &artifact.identity[..16],
    namespace_root_hash: &artifact.identity[16..16 + hash_width],
    retirement_id: &artifact.identity[16 + hash_width..],
    committed_at_ms: i64_at(body, 32 + hash_width)?,
    pending_since_ms: i64_at(body, 40 + hash_width)?,
    grace_at_pending_ms: u64_at(body, 48 + hash_width)?,
    final_mark_generation: u64_at(body, 56 + hash_width)?,
    reason: u16_at(body, 64 + hash_width)?,
    prior_lifecycle_manifest_hash: &body[72 + hash_width..72 + 2 * hash_width],
    authority_root_set_digest: &body[72 + 2 * hash_width..72 + 3 * hash_width],
    admission_commit_payload_hash: &body[72 + 3 * hash_width..],
    key: immutable_gc_artifact_key(algorithm, artifact.kind, bytes),
  })
}

pub fn decode_root_object_reclaim_proof_v1(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<RootObjectReclaimProofV1<'_>> {
  let _validated = decode_gc_state_artifact(bytes, algorithm)?;
  let artifact = decode_gc_artifact_envelope(bytes)?;
  if artifact.kind != GcArtifactKindV1::RootObjectReclaimProof {
    return Err(kind_error("root_reclaim_proof_kind", "artifact is not a root-object reclaim proof"));
  }
  let hash_width = algorithm.hash_length();
  let body = artifact.body;
  Ok(RootObjectReclaimProofV1 {
    database_id: &artifact.identity[..16],
    namespace_root_hash: &artifact.identity[16..16 + hash_width],
    proof_id: &artifact.identity[16 + hash_width..],
    generation: artifact.generation,
    retirement_commit_hash: &body[16 + hash_width..16 + 2 * hash_width],
    reclaimed_at_ms: i64_at(body, 16 + 2 * hash_width)?,
    physical_inventory_manifest_hash: &body[24 + 2 * hash_width..24 + 3 * hash_width],
    root_object_incarnation_digest: &body[24 + 3 * hash_width..24 + 4 * hash_width],
    root_object_incarnation_count: u64_at(body, 24 + 4 * hash_width)?,
    sweep_receipt_merkle_root: &body[32 + 4 * hash_width..32 + 5 * hash_width],
    sweep_receipt_count: u64_at(body, 32 + 5 * hash_width)?,
    absence_digest: &body[40 + 5 * hash_width..],
    key: immutable_gc_artifact_key(algorithm, artifact.kind, bytes),
  })
}

pub fn validate_root_lifecycle_candidate_directory(
  manifest: &RootLifecycleManifestV1<'_>,
  directory: &GcStateDirectoryV1<'_>,
) -> FormatResult<()> {
  if manifest.candidate_directory_hash.is_none()
    || directory.role != GcDirectoryRoleV1::RootCandidates
    || directory.database_id != manifest.database_id
    || manifest.candidate_directory_hash != Some(directory.key.as_slice())
    || directory.live_count != manifest.candidate_count
    || directory.tombstone_count != 0
    || directory.logical_bytes != manifest.candidate_bytes
    || directory.maximum_page_id >= manifest.next_page_id
  {
    return Err(closure_error(
      "root_lifecycle_candidate_directory",
      "root-candidate directory does not close against its selected lifecycle manifest",
    ));
  }
  Ok(())
}

pub fn validate_root_lifecycle_expiry_manifest(
  lifecycle: &RootLifecycleManifestV1<'_>,
  expiry: &RootExpiryManifestV1<'_>,
) -> FormatResult<()> {
  if lifecycle.root_expiry_manifest_hash.is_none()
    || expiry.database_id != lifecycle.database_id
    || lifecycle.root_expiry_manifest_hash != Some(expiry.key.as_slice())
    || expiry.record_count != lifecycle.retired_evidence_count
    || expiry.logical_bytes != lifecycle.expiry_bytes
  {
    return Err(closure_error(
      "root_lifecycle_expiry_manifest",
      "root-expiry manifest does not close against its selected lifecycle manifest",
    ));
  }
  Ok(())
}

pub fn validate_root_expiry_manifest_directory(
  manifest: &RootExpiryManifestV1<'_>,
  directory: &GcStateDirectoryV1<'_>,
) -> FormatResult<()> {
  if manifest.directory_root_hash.is_none()
    || directory.role != GcDirectoryRoleV1::RootExpiry
    || directory.database_id != manifest.database_id
    || manifest.directory_root_hash != Some(directory.key.as_slice())
    || directory.live_count != manifest.record_count
    || directory.tombstone_count != 0
    || directory.logical_bytes != manifest.logical_bytes
    || directory.maximum_page_id >= manifest.next_page_id
  {
    return Err(closure_error(
      "root_expiry_manifest_directory",
      "root-expiry directory does not close against its selected expiry manifest",
    ));
  }
  Ok(())
}

pub fn validate_root_expiry_retirement_commit(
  record: &RootExpiryRecordV1<'_>,
  retirement: &RootRetirementCommitV1<'_>,
) -> FormatResult<()> {
  if record.namespace_root_hash != retirement.namespace_root_hash
    || record.retirement_commit_hash != retirement.key
    || record.final_mark_generation != retirement.final_mark_generation
    || record.reason != retirement.reason
  {
    return Err(closure_error(
      "root_expiry_retirement_commit",
      "root-expiry record does not close against its immutable retirement commit",
    ));
  }
  Ok(())
}

pub fn validate_root_expiry_reclaim_proof(record: &RootExpiryRecordV1<'_>, proof: &RootObjectReclaimProofV1<'_>) -> FormatResult<()> {
  if record.state != RootExpiryStateV1::PhysicallyReclaimed
    || record.namespace_root_hash != proof.namespace_root_hash
    || record.retirement_commit_hash != proof.retirement_commit_hash
    || record.root_object_reclaim_proof_hash != Some(proof.key.as_slice())
    || proof.reclaimed_at_ms < record.retired_at_ms
    || record.evidence_expires_at_ms.is_none_or(|expires_at| expires_at < proof.reclaimed_at_ms)
  {
    return Err(closure_error(
      "root_expiry_reclaim_proof",
      "physically-reclaimed root-expiry record does not close against its reclaim proof",
    ));
  }
  Ok(())
}

pub fn encode_root_candidate_record_v1(request: &RootCandidateRecordWriteV1<'_>) -> FormatResult<Vec<u8>> {
  let hash_width = request.hash_algorithm.hash_length();
  require_hash(request.namespace_root_hash, hash_width, "root_candidate_row")?;
  require_hash(request.authority_root_set_digest, hash_width, "root_candidate_row")?;
  require_hash(request.admission_commit_payload_hash, hash_width, "root_candidate_row")?;
  let mut row = vec![0u8; 36 + 3 * hash_width];
  row[..hash_width].copy_from_slice(request.namespace_root_hash);
  row[hash_width] = 1;
  put_u16(&mut row, hash_width + 2, request.reason);
  put_i64(&mut row, hash_width + 4, request.pending_since_ms);
  put_u64(&mut row, hash_width + 12, request.first_unreachable_generation);
  put_u64(&mut row, hash_width + 20, request.last_confirmed_unreachable_generation);
  put_u64(&mut row, hash_width + 28, request.grace_at_pending_ms);
  row[hash_width + 36..hash_width + 36 + hash_width].copy_from_slice(request.authority_root_set_digest);
  row[hash_width + 36 + hash_width..].copy_from_slice(request.admission_commit_payload_hash);
  let _validated: RootCandidateRecordV1<'_> = decode_root_candidate_record_v1(&row, request.hash_algorithm)?;
  Ok(row)
}

pub fn encode_root_expiry_record_v1(request: &RootExpiryRecordWriteV1<'_>) -> FormatResult<Vec<u8>> {
  let hash_width = request.hash_algorithm.hash_length();
  require_hash(request.namespace_root_hash, hash_width, "root_expiry_row")?;
  require_hash(request.retirement_commit_hash, hash_width, "root_expiry_row")?;
  if let Some(proof) = request.root_object_reclaim_proof_hash {
    require_hash(proof, hash_width, "root_expiry_row_state")?;
  }
  let mut row = vec![0u8; 40 + 3 * hash_width];
  row[..hash_width].copy_from_slice(request.namespace_root_hash);
  put_i64(&mut row, hash_width, request.retired_at_ms);
  put_i64(&mut row, hash_width + 8, request.last_pending_since_ms);
  put_u64(&mut row, hash_width + 16, request.final_mark_generation);
  put_u16(&mut row, hash_width + 24, request.reason);
  row[hash_width + 32..hash_width + 32 + hash_width].copy_from_slice(request.retirement_commit_hash);
  match (request.state, request.root_object_reclaim_proof_hash, request.evidence_expires_at_ms) {
    (RootExpiryStateV1::LogicallyRetired, None, None) => row[hash_width + 26] = 1,
    (RootExpiryStateV1::PhysicallyReclaimed, Some(proof), Some(expires_at_ms)) => {
      row[hash_width + 26] = 2;
      row[hash_width + 27] = 1;
      row[hash_width + 32 + hash_width..hash_width + 32 + 2 * hash_width].copy_from_slice(proof);
      put_i64(&mut row, hash_width + 32 + 2 * hash_width, expires_at_ms);
    }
    _ => return Err(closure_error("root_expiry_row_state", "root-expiry state and optional reclaim evidence disagree")),
  }
  let _validated: RootExpiryRecordV1<'_> = decode_root_expiry_record_v1(&row, request.hash_algorithm)?;
  Ok(row)
}

pub fn encode_root_expiry_manifest_v1(request: &RootExpiryManifestWriteV1<'_>) -> FormatResult<EncodedImmutableGcArtifactV1> {
  let hash_width = request.hash_algorithm.hash_length();
  require_database_id(request.database_id)?;
  if let Some(root) = request.directory_root_hash {
    require_hash(root, hash_width, "root_expiry_manifest_state")?;
  }
  let mut body = vec![0u8; 124 + hash_width];
  write_capabilities(&mut body[4..36], ROOT_LIFECYCLE_CAPABILITIES);
  put_u64(&mut body, 36, request.retention_ms);
  put_u64(&mut body, 44, request.optional_byte_budget);
  if let Some(root) = request.directory_root_hash {
    body[52..52 + hash_width].copy_from_slice(root);
  }
  put_u64(&mut body, 52 + hash_width, request.next_page_id);
  put_u64(&mut body, 60 + hash_width, request.record_count);
  put_u64(&mut body, 68 + hash_width, request.logical_bytes);
  put_u64(&mut body, 76 + hash_width, request.mandatory_count);
  put_u64(&mut body, 84 + hash_width, request.mandatory_bytes);
  put_u64(&mut body, 92 + hash_width, request.optional_count);
  put_u64(&mut body, 100 + hash_width, request.optional_bytes);
  put_i64(&mut body, 108 + hash_width, request.oldest_retired_at_ms.map_or(0, std::convert::identity));
  put_i64(&mut body, 116 + hash_width, request.newest_retired_at_ms.map_or(0, std::convert::identity));
  let encoded =
    encode_manifest(request.hash_algorithm, GcArtifactKindV1::RootExpiryCatalogManifest, request.database_id, request.generation, &body)?;
  let _validated = decode_root_expiry_manifest_v1(&encoded.value, request.hash_algorithm)?;
  Ok(encoded)
}

pub fn encode_root_lifecycle_manifest_v1(request: &RootLifecycleManifestWriteV1<'_>) -> FormatResult<EncodedImmutableGcArtifactV1> {
  let hash_width = request.hash_algorithm.hash_length();
  require_database_id(request.database_id)?;
  require_hash(request.authority_root_set_digest, hash_width, "root_lifecycle_manifest_header")?;
  for root in [request.candidate_directory_hash, request.root_expiry_manifest_hash].into_iter().flatten() {
    require_hash(root, hash_width, "root_lifecycle_manifest_state")?;
  }
  let mut body = vec![0u8; 108 + 3 * hash_width];
  write_capabilities(&mut body[4..36], ROOT_LIFECYCLE_CAPABILITIES);
  put_u64(&mut body, 36, request.generation);
  put_i64(&mut body, 44, request.published_at_ms);
  put_u64(&mut body, 52, request.source_complete_mark_generation);
  body[60..60 + hash_width].copy_from_slice(request.authority_root_set_digest);
  if let Some(root) = request.candidate_directory_hash {
    body[60 + hash_width..60 + 2 * hash_width].copy_from_slice(root);
  }
  if let Some(root) = request.root_expiry_manifest_hash {
    body[60 + 2 * hash_width..60 + 3 * hash_width].copy_from_slice(root);
  }
  put_u64(&mut body, 60 + 3 * hash_width, request.next_page_id);
  put_u64(&mut body, 68 + 3 * hash_width, request.candidate_count);
  put_u64(&mut body, 76 + 3 * hash_width, request.pending_count);
  put_u64(&mut body, 84 + 3 * hash_width, request.retired_evidence_count);
  put_u64(&mut body, 92 + 3 * hash_width, request.candidate_bytes);
  put_u64(&mut body, 100 + 3 * hash_width, request.expiry_bytes);
  let encoded =
    encode_manifest(request.hash_algorithm, GcArtifactKindV1::RootLifecycleManifest, request.database_id, request.generation, &body)?;
  let _validated = decode_root_lifecycle_manifest_v1(&encoded.value, request.hash_algorithm)?;
  Ok(encoded)
}

pub fn encode_root_retirement_commit_v1(request: &RootRetirementCommitWriteV1<'_>) -> FormatResult<EncodedImmutableGcArtifactV1> {
  let hash_width = request.hash_algorithm.hash_length();
  require_database_id(request.database_id)?;
  require_hash(request.namespace_root_hash, hash_width, "root_retirement_shape")?;
  require_exact_nonzero(request.retirement_id, 16, "root_retirement_shape")?;
  require_hash(request.prior_lifecycle_manifest_hash, hash_width, "root_retirement_fields")?;
  require_hash(request.authority_root_set_digest, hash_width, "root_retirement_fields")?;
  require_hash(request.admission_commit_payload_hash, hash_width, "root_retirement_fields")?;
  let mut identity = Vec::with_capacity(32 + hash_width);
  identity.extend_from_slice(request.database_id);
  identity.extend_from_slice(request.namespace_root_hash);
  identity.extend_from_slice(request.retirement_id);
  let mut body = vec![0u8; 72 + 4 * hash_width];
  body[..32 + hash_width].copy_from_slice(&identity);
  put_i64(&mut body, 32 + hash_width, request.committed_at_ms);
  put_i64(&mut body, 40 + hash_width, request.pending_since_ms);
  put_u64(&mut body, 48 + hash_width, request.grace_at_pending_ms);
  put_u64(&mut body, 56 + hash_width, request.final_mark_generation);
  put_u16(&mut body, 64 + hash_width, request.reason);
  body[72 + hash_width..72 + 2 * hash_width].copy_from_slice(request.prior_lifecycle_manifest_hash);
  body[72 + 2 * hash_width..72 + 3 * hash_width].copy_from_slice(request.authority_root_set_digest);
  body[72 + 3 * hash_width..].copy_from_slice(request.admission_commit_payload_hash);
  let encoded = encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 {
    kind: GcArtifactKindV1::RootRetirementCommit,
    hash_algorithm: request.hash_algorithm,
    generation: request.final_mark_generation,
    identity: &identity,
    body: &body,
  })?;
  let _validated = decode_root_retirement_commit_v1(&encoded.value, request.hash_algorithm)?;
  Ok(encoded)
}

pub fn encode_root_object_reclaim_proof_v1(request: &RootObjectReclaimProofWriteV1<'_>) -> FormatResult<EncodedImmutableGcArtifactV1> {
  let hash_width = request.hash_algorithm.hash_length();
  require_database_id(request.database_id)?;
  require_hash(request.namespace_root_hash, hash_width, "root_reclaim_proof_shape")?;
  require_exact_nonzero(request.proof_id, 16, "root_reclaim_proof_shape")?;
  for value in [
    request.retirement_commit_hash,
    request.physical_inventory_manifest_hash,
    request.root_object_incarnation_digest,
    request.sweep_receipt_merkle_root,
    request.absence_digest,
  ] {
    require_hash(value, hash_width, "root_reclaim_proof_fields")?;
  }
  let mut identity = Vec::with_capacity(32 + hash_width);
  identity.extend_from_slice(request.database_id);
  identity.extend_from_slice(request.namespace_root_hash);
  identity.extend_from_slice(request.proof_id);
  let mut body = vec![0u8; 40 + 6 * hash_width];
  body[..16].copy_from_slice(request.database_id);
  body[16..16 + hash_width].copy_from_slice(request.namespace_root_hash);
  body[16 + hash_width..16 + 2 * hash_width].copy_from_slice(request.retirement_commit_hash);
  put_i64(&mut body, 16 + 2 * hash_width, request.reclaimed_at_ms);
  body[24 + 2 * hash_width..24 + 3 * hash_width].copy_from_slice(request.physical_inventory_manifest_hash);
  body[24 + 3 * hash_width..24 + 4 * hash_width].copy_from_slice(request.root_object_incarnation_digest);
  put_u64(&mut body, 24 + 4 * hash_width, request.root_object_incarnation_count);
  body[32 + 4 * hash_width..32 + 5 * hash_width].copy_from_slice(request.sweep_receipt_merkle_root);
  put_u64(&mut body, 32 + 5 * hash_width, request.sweep_receipt_count);
  body[40 + 5 * hash_width..].copy_from_slice(request.absence_digest);
  let encoded = encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 {
    kind: GcArtifactKindV1::RootObjectReclaimProof,
    hash_algorithm: request.hash_algorithm,
    generation: request.generation,
    identity: &identity,
    body: &body,
  })?;
  let _validated = decode_root_object_reclaim_proof_v1(&encoded.value, request.hash_algorithm)?;
  Ok(encoded)
}

fn encode_manifest(
  hash_algorithm: HashAlgorithm,
  kind: GcArtifactKindV1,
  database_id: &[u8],
  generation: u64,
  body: &[u8],
) -> FormatResult<EncodedImmutableGcArtifactV1> {
  let mut identity = Vec::with_capacity(24);
  identity.extend_from_slice(database_id);
  identity.extend_from_slice(&generation.to_le_bytes());
  encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 { kind, hash_algorithm, generation, identity: &identity, body })
}

fn require_database_id(value: &[u8]) -> FormatResult<()> {
  require_exact_nonzero(value, 16, "gc_manifest_identity")
}

fn require_hash(value: &[u8], hash_width: usize, code: &'static str) -> FormatResult<()> {
  require_exact_nonzero(value, hash_width, code)
}

fn require_exact_nonzero(value: &[u8], expected_length: usize, code: &'static str) -> FormatResult<()> {
  if value.len() != expected_length || value.iter().all(|byte| *byte == 0) {
    return Err(identity_error(code, format!("expected a nonzero {expected_length}-byte value")));
  }
  Ok(())
}

fn optional_hash(value: &[u8]) -> Option<&[u8]> {
  (!value.iter().all(|byte| *byte == 0)).then_some(value)
}

fn write_capabilities(value: &mut [u8], bits: &[usize]) {
  for bit in bits {
    value[bit / 8] |= 1 << (bit % 8);
  }
}

fn put_u16(value: &mut [u8], offset: usize, field: u16) {
  value[offset..offset + 2].copy_from_slice(&field.to_le_bytes());
}

fn put_u64(value: &mut [u8], offset: usize, field: u64) {
  value[offset..offset + 8].copy_from_slice(&field.to_le_bytes());
}

fn put_i64(value: &mut [u8], offset: usize, field: i64) {
  value[offset..offset + 8].copy_from_slice(&field.to_le_bytes());
}

fn i64_at(value: &[u8], offset: usize) -> FormatResult<i64> {
  let bytes = value
    .get(offset..offset + 8)
    .ok_or_else(|| FormatError::new(MalformedInputClass::TruncationOrTrailingBytes, "root_lifecycle_truncated", "i64 is truncated"))?;
  let mut raw = [0u8; 8];
  raw.copy_from_slice(bytes);
  Ok(i64::from_le_bytes(raw))
}

fn identity_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::IdentityKeyOrGenerationMismatch, code, context)
}

fn closure_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::CrossRecordClosureMismatch, code, context)
}

fn kind_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::UnknownTypeKindOrEnum, code, context)
}
