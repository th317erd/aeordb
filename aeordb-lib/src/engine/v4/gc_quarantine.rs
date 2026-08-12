use std::cmp::Ordering;

use super::gc::{
  EncodedImmutableGcArtifactV1, GcArtifactKindV1, ImmutableGcArtifactWriteV1, PhysicalIncarnationV1, compare_physical_incarnations_v1,
  checked_immutable_gc_artifact_encoded_length, decode_gc_artifact_envelope, decode_physical_incarnation, encode_immutable_gc_artifact,
  immutable_gc_artifact_key,
};
use super::gc_state::{
  GcDirectoryRoleV1, GcStateDirectoryEntryV1, GcStateDirectoryV1, GcStateManifestV1, GcStatePageV1, MAXIMUM_GC_DIRECTORY_ENTRIES_V1,
};
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

const MAXIMUM_QUARANTINE_MANIFEST_LENGTH: usize = 1_024 * 1_024;
const MAXIMUM_CANDIDATE_DELTAS: usize = 256;
const MAXIMUM_CANDIDATE_DELTA_BYTES: u64 = 64 * 1_024 * 1_024;
const MAXIMUM_DIRECTORY_LEVELS: usize = 17;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum PhysicalQuarantineCandidateClassV1 {
  UnreachableActiveLocator = 1,
  RetiredLowerIncarnation = 2,
  OrphanUncommittedIncarnation = 3,
  ExpiredDerivedArtifact = 4,
  ExpiredGcAuditArtifact = 5,
  ExpiredNamespaceRootClosure = 6,
  UnexplainedGapInventoryCandidate = 7,
}

impl PhysicalQuarantineCandidateClassV1 {
  fn from_u16(value: u16) -> FormatResult<Self> {
    match value {
      1 => Ok(Self::UnreachableActiveLocator),
      2 => Ok(Self::RetiredLowerIncarnation),
      3 => Ok(Self::OrphanUncommittedIncarnation),
      4 => Ok(Self::ExpiredDerivedArtifact),
      5 => Ok(Self::ExpiredGcAuditArtifact),
      6 => Ok(Self::ExpiredNamespaceRootClosure),
      7 => Ok(Self::UnexplainedGapInventoryCandidate),
      _ => Err(error(
        MalformedInputClass::UnknownTypeKindOrEnum,
        "candidate_row_class",
        format!("unknown physical quarantine candidate class {value}"),
      )),
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalQuarantineCandidateV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub encoded: &'a [u8],
  pub incarnation: PhysicalIncarnationV1<'a>,
  pub class: PhysicalQuarantineCandidateClassV1,
  pub pending_since_ms: u64,
  pub first_unreachable_generation: u64,
  pub grace_at_pending_ms: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct PhysicalQuarantineCandidateWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub incarnation: PhysicalIncarnationV1<'a>,
  pub class: PhysicalQuarantineCandidateClassV1,
  pub pending_since_ms: u64,
  pub first_unreachable_generation: u64,
  pub grace_at_pending_ms: u64,
}

impl<'a> From<&PhysicalQuarantineCandidateV1<'a>> for PhysicalQuarantineCandidateWriteV1<'a> {
  fn from(value: &PhysicalQuarantineCandidateV1<'a>) -> Self {
    Self {
      hash_algorithm: value.hash_algorithm,
      incarnation: value.incarnation,
      class: value.class,
      pending_since_ms: value.pending_since_ms,
      first_unreachable_generation: value.first_unreachable_generation,
      grace_at_pending_ms: value.grace_at_pending_ms,
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateDeltaOperationV1 {
  Set,
  Clear,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CandidateDeltaRecordV1<'a> {
  pub operation: CandidateDeltaOperationV1,
  pub candidate: PhysicalQuarantineCandidateV1<'a>,
}

#[derive(Clone, Copy, Debug)]
pub struct CandidateDeltaRecordWriteV1<'a> {
  pub operation: CandidateDeltaOperationV1,
  pub candidate: PhysicalQuarantineCandidateWriteV1<'a>,
}

impl<'a> From<&CandidateDeltaRecordV1<'a>> for CandidateDeltaRecordWriteV1<'a> {
  fn from(value: &CandidateDeltaRecordV1<'a>) -> Self {
    Self { operation: value.operation, candidate: PhysicalQuarantineCandidateWriteV1::from(&value.candidate) }
  }
}

#[derive(Clone, Debug)]
pub struct CandidateDeltaV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8],
  pub mark_generation: u64,
  pub delta_ordinal: u32,
  pub previous_delta_hash: Option<&'a [u8]>,
  pub record_count: u32,
  records: &'a [u8],
  pub key: Vec<u8>,
}

impl<'a> CandidateDeltaV1<'a> {
  pub fn records(&self) -> FormatResult<CandidateDeltaRecordsV1<'a>> {
    let record_length =
      56usize.checked_add(2 * self.hash_algorithm.hash_length()).ok_or_else(|| length_error("candidate delta record width overflow"))?;
    let record_count =
      usize::try_from(self.record_count).map_err(|error| length_error(format!("candidate delta record count: {error}")))?;
    let records_length = record_count.checked_mul(record_length).ok_or_else(|| length_error("candidate delta records overflow"))?;
    if self.records.len() != records_length {
      return Err(closure_error("candidate_delta_count", "candidate delta count no longer closes against its borrowed records"));
    }
    Ok(CandidateDeltaRecordsV1 { rows: self.records.chunks_exact(record_length), algorithm: self.hash_algorithm })
  }
}

#[derive(Clone, Copy, Debug)]
pub struct CandidateDeltaWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: [u8; 16],
  pub mark_generation: u64,
  pub delta_ordinal: u32,
  pub previous_delta_hash: Option<&'a [u8]>,
  pub records: &'a [CandidateDeltaRecordWriteV1<'a>],
}

#[derive(Debug)]
pub struct CandidateDeltaRecordsV1<'a> {
  rows: std::slice::ChunksExact<'a, u8>,
  algorithm: HashAlgorithm,
}

impl<'a> Iterator for CandidateDeltaRecordsV1<'a> {
  type Item = FormatResult<CandidateDeltaRecordV1<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    self.rows.next().map(|row| decode_candidate_delta_record_v1(row, self.algorithm))
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    self.rows.size_hint()
  }
}

impl ExactSizeIterator for CandidateDeltaRecordsV1<'_> {}

#[derive(Clone, Debug)]
pub struct QuarantineManifestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8],
  pub mark_generation: u64,
  pub completed_at_ms: u64,
  pub required_capabilities: &'a [u8],
  pub authority_root_set_digest: &'a [u8],
  pub semantic_state_digest: &'a [u8],
  pub kv_layout_fingerprint: &'a [u8],
  pub mark_result_digest: &'a [u8],
  pub candidate_directory_root: Option<&'a [u8]>,
  pub captured_root_lifecycle_manifest: &'a [u8],
  pub delta_count: u32,
  pub candidate_count: u64,
  pub candidate_bytes: u64,
  pub eligible_count_hint: u64,
  pub eligible_bytes_hint: u64,
  pub next_candidate_page_id: u64,
  pub delta_hashes: &'a [u8],
  pub key: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct QuarantineManifestWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: [u8; 16],
  pub mark_generation: u64,
  pub completed_at_ms: u64,
  pub required_capabilities: &'a [u8],
  pub authority_root_set_digest: &'a [u8],
  pub semantic_state_digest: &'a [u8],
  pub kv_layout_fingerprint: &'a [u8],
  pub mark_result_digest: &'a [u8],
  pub candidate_directory_root: Option<&'a [u8]>,
  pub captured_root_lifecycle_manifest: &'a [u8],
  pub candidate_count: u64,
  pub candidate_bytes: u64,
  pub eligible_count_hint: u64,
  pub eligible_bytes_hint: u64,
  pub next_candidate_page_id: u64,
  pub delta_hashes: &'a [u8],
}

impl<'a> QuarantineManifestWriteV1<'a> {
  pub fn from_decoded(value: &QuarantineManifestV1<'a>) -> FormatResult<Self> {
    if value.database_id.len() != 16 {
      return Err(closure_error("quarantine_manifest_identity", "decoded quarantine database identity has the wrong width"));
    }
    let mut database_id = [0u8; 16];
    database_id.copy_from_slice(value.database_id);
    Ok(Self {
      hash_algorithm: value.hash_algorithm,
      database_id,
      mark_generation: value.mark_generation,
      completed_at_ms: value.completed_at_ms,
      required_capabilities: value.required_capabilities,
      authority_root_set_digest: value.authority_root_set_digest,
      semantic_state_digest: value.semantic_state_digest,
      kv_layout_fingerprint: value.kv_layout_fingerprint,
      mark_result_digest: value.mark_result_digest,
      candidate_directory_root: value.candidate_directory_root,
      captured_root_lifecycle_manifest: value.captured_root_lifecycle_manifest,
      candidate_count: value.candidate_count,
      candidate_bytes: value.candidate_bytes,
      eligible_count_hint: value.eligible_count_hint,
      eligible_bytes_hint: value.eligible_bytes_hint,
      next_candidate_page_id: value.next_candidate_page_id,
      delta_hashes: value.delta_hashes,
    })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantineClosureSummaryV1 {
  pub base_page_count: u64,
  pub base_record_count: u64,
  pub base_logical_bytes: u64,
  pub declared_candidate_count: u64,
  pub declared_candidate_bytes: u64,
  pub delta_count: u64,
  pub delta_record_count: u64,
  pub delta_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuarantineClosureLimitsV1 {
  pub maximum_support_artifacts: u64,
}

#[derive(Debug, Error)]
pub enum QuarantineClosureErrorV1 {
  #[error("physical-quarantine closure configuration is invalid: {0}")]
  InvalidConfiguration(&'static str),
  #[error("physical-quarantine closure validation was canceled")]
  Canceled,
  #[error("physical-quarantine closure exceeded its support-artifact limit")]
  ArtifactLimit,
  #[error(transparent)]
  Allocation(#[from] std::collections::TryReserveError),
  #[error(transparent)]
  IntegerConversion(#[from] std::num::TryFromIntError),
  #[error(transparent)]
  Format(#[from] FormatError),
  #[error(transparent)]
  Memory(#[from] MemoryCoordinatorError),
  #[error("physical-quarantine closure validator has already failed")]
  Failed,
}

impl QuarantineClosureErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::InvalidConfiguration(_) => "quarantine_closure_configuration",
      Self::Canceled => "quarantine_closure_canceled",
      Self::ArtifactLimit | Self::Allocation(_) | Self::IntegerConversion(_) => "quarantine_closure_artifact_limit",
      Self::Format(source) => source.code(),
      Self::Memory(_) => "quarantine_closure_memory",
      Self::Failed => "quarantine_closure_failed",
    }
  }
}

#[derive(Clone, Debug)]
struct QuarantineChildSummaryV1 {
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

pub struct QuarantineClosureValidatorV1<'a> {
  manifest: &'a QuarantineManifestV1<'a>,
  directory: Option<&'a GcStateDirectoryV1<'a>>,
  algorithm: HashAlgorithm,
  cancellation: CancellationToken,
  maximum_support_artifacts: u64,
  support_artifact_count: u64,
  memory: MemoryReservation,
  levels: [Vec<QuarantineChildSummaryV1>; MAXIMUM_DIRECTORY_LEVELS],
  base_page_count: u64,
  base_record_count: u64,
  base_logical_bytes: u64,
  last_page_id: u64,
  previous_upper_fence: Vec<u8>,
  delta_count: u64,
  delta_record_count: u64,
  delta_bytes: u64,
  last_delta_generation: u64,
  last_delta_ordinal: u32,
  previous_delta_hash: Vec<u8>,
  failed: bool,
}

impl std::fmt::Debug for QuarantineClosureValidatorV1<'_> {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("QuarantineClosureValidatorV1")
      .field("algorithm", &self.algorithm)
      .field("maximum_support_artifacts", &self.maximum_support_artifacts)
      .field("support_artifact_count", &self.support_artifact_count)
      .field("base_page_count", &self.base_page_count)
      .field("delta_count", &self.delta_count)
      .field("failed", &self.failed)
      .finish_non_exhaustive()
  }
}

impl<'a> QuarantineClosureValidatorV1<'a> {
  pub fn new(
    manifest: &'a QuarantineManifestV1<'a>,
    directory: Option<&'a GcStateDirectoryV1<'a>>,
    lifecycle: &GcStateManifestV1<'_>,
    algorithm: HashAlgorithm,
    cancellation: CancellationToken,
    limits: QuarantineClosureLimitsV1,
    memory: &MemoryCoordinator,
  ) -> Result<Self, QuarantineClosureErrorV1> {
    if limits.maximum_support_artifacts == 0 {
      return Err(QuarantineClosureErrorV1::InvalidConfiguration("support artifact limit must be nonzero"));
    }
    if cancellation.is_cancelled() {
      return Err(QuarantineClosureErrorV1::Canceled);
    }
    let expected_width = algorithm.hash_length();
    if manifest.hash_algorithm != algorithm
      || manifest.authority_root_set_digest.len() != expected_width
      || lifecycle.kind != GcArtifactKindV1::RootLifecycleManifest
      || lifecycle.database_id != manifest.database_id
      || lifecycle.key != manifest.captured_root_lifecycle_manifest
    {
      return Err(
        closure_error(
          "quarantine_lifecycle_basis",
          "quarantine manifest does not close against its exact captured root-lifecycle manifest",
        )
        .into(),
      );
    }
    match (manifest.candidate_directory_root, directory) {
      (None, None) => {}
      (Some(root), Some(directory))
        if directory.role == GcDirectoryRoleV1::Candidates
          && directory.database_id == manifest.database_id
          && directory.key == root
          && directory.maximum_page_id < manifest.next_candidate_page_id => {}
      _ => {
        return Err(
          closure_error("quarantine_candidate_directory", "quarantine candidate directory does not close against its manifest").into(),
        );
      }
    }
    let support_artifact_count = u64::from(directory.is_some());
    if support_artifact_count > limits.maximum_support_artifacts {
      return Err(QuarantineClosureErrorV1::ArtifactLimit);
    }
    Ok(Self {
      manifest,
      directory,
      algorithm,
      cancellation,
      maximum_support_artifacts: limits.maximum_support_artifacts,
      support_artifact_count,
      memory: memory.reserve(MemoryOwner::GarbageCollection, 0, AdmissionClass::Maintenance)?,
      levels: std::array::from_fn(|_| Vec::new()),
      base_page_count: 0,
      base_record_count: 0,
      base_logical_bytes: 0,
      last_page_id: 0,
      previous_upper_fence: Vec::with_capacity(24 + 2 * expected_width),
      delta_count: 0,
      delta_record_count: 0,
      delta_bytes: 0,
      last_delta_generation: 0,
      last_delta_ordinal: 0,
      previous_delta_hash: Vec::with_capacity(expected_width),
      failed: false,
    })
  }

  pub fn observe_base_page(&mut self, page: &GcStatePageV1<'_>) -> Result<(), QuarantineClosureErrorV1> {
    self.preflight_observation()?;
    let result = self.observe_base_page_inner(page);
    self.latch_observation(result)
  }

  pub fn observe_delta(&mut self, bytes: &[u8]) -> Result<(), QuarantineClosureErrorV1> {
    self.preflight_observation()?;
    let result = self.observe_delta_inner(bytes);
    self.latch_observation(result)
  }

  pub fn observe_base_directory(&mut self, directory: &GcStateDirectoryV1<'_>) -> Result<(), QuarantineClosureErrorV1> {
    self.preflight_observation()?;
    let result = self.observe_base_directory_inner(directory, false);
    self.latch_observation(result)
  }

  pub fn finish(mut self) -> Result<QuarantineClosureSummaryV1, QuarantineClosureErrorV1> {
    self.preflight()?;
    let root = self.directory;
    if let Some(root) = root {
      self.observe_base_directory_inner(root, true)?;
      let mut summaries = self.levels.iter().flatten();
      let Some(summary) = summaries.next() else {
        return Err(closure_error("quarantine_candidate_directory", "quarantine root directory was not observed").into());
      };
      if summaries.next().is_some()
        || summary.child_hash != root.key
        || self.base_page_count != root.page_count
        || self.base_record_count != root.live_count
        || self.base_logical_bytes != root.logical_bytes
      {
        return Err(
          closure_error("quarantine_closure_totals", "quarantine base graph does not close against its compacted root directory").into(),
        );
      }
    } else if self.base_page_count != 0
      || self.base_record_count != 0
      || self.base_logical_bytes != 0
      || self.levels.iter().any(|level| !level.is_empty())
    {
      return Err(closure_error("quarantine_closure_totals", "quarantine manifest without a base root observed base artifacts").into());
    }
    if self.delta_count != u64::from(self.manifest.delta_count) {
      return Err(closure_error("quarantine_closure_totals", "quarantine observed delta count does not close against the manifest").into());
    }
    Ok(QuarantineClosureSummaryV1 {
      base_page_count: self.base_page_count,
      base_record_count: self.base_record_count,
      base_logical_bytes: self.base_logical_bytes,
      declared_candidate_count: self.manifest.candidate_count,
      declared_candidate_bytes: self.manifest.candidate_bytes,
      delta_count: self.delta_count,
      delta_record_count: self.delta_record_count,
      delta_bytes: self.delta_bytes,
    })
  }

  fn observe_base_page_inner(&mut self, page: &GcStatePageV1<'_>) -> Result<(), QuarantineClosureErrorV1> {
    let Some(root) = self.directory else {
      return Err(closure_error("quarantine_base_page", "empty quarantine state cannot contain a base candidate page").into());
    };
    if page.role != GcDirectoryRoleV1::Candidates
      || page.database_id != root.database_id
      || page.catalog_id != root.catalog_id
      || page.generation > root.generation
      || page.page_id >= self.manifest.next_candidate_page_id
    {
      return Err(closure_error("quarantine_base_page", "quarantine base page does not belong to the compacted candidate graph").into());
    }
    let fences_out_of_order = !self.previous_upper_fence.is_empty()
      && candidate_fence_order(&self.previous_upper_fence, page.lower_fence, self.algorithm)? != Ordering::Less;
    if self.last_page_id >= page.page_id || fences_out_of_order {
      return Err(order_error("quarantine_base_page_order", "quarantine base pages are duplicated, reordered, or overlapping").into());
    }
    for record in quarantine_candidate_records_v1(page, self.algorithm)? {
      self.check_cancellation()?;
      record?;
    }
    self.base_page_count = self.base_page_count.checked_add(1).ok_or_else(|| length_error("quarantine base page count overflow"))?;
    self.base_record_count = self
      .base_record_count
      .checked_add(u64::from(page.record_count))
      .ok_or_else(|| length_error("quarantine base record count overflow"))?;
    self.base_logical_bytes =
      self.base_logical_bytes.checked_add(page.logical_bytes).ok_or_else(|| length_error("quarantine base logical bytes overflow"))?;
    self.last_page_id = page.page_id;
    self.previous_upper_fence.clear();
    self.previous_upper_fence.extend_from_slice(page.upper_fence);
    let summary = child_summary_from_page(page, &mut self.memory)?;
    self.push_summary(0, summary)?;
    Ok(())
  }

  fn observe_base_directory_inner(&mut self, directory: &GcStateDirectoryV1<'_>, root: bool) -> Result<(), QuarantineClosureErrorV1> {
    let Some(expected_root) = self.directory else {
      return Err(closure_error("quarantine_base_directory", "empty quarantine state cannot contain a candidate directory").into());
    };
    let level = usize::from(directory.level);
    if level >= MAXIMUM_DIRECTORY_LEVELS - 1
      || directory.role != GcDirectoryRoleV1::Candidates
      || directory.database_id != expected_root.database_id
      || directory.catalog_id != expected_root.catalog_id
      || directory.generation > expected_root.generation
      || (!root && (directory.key == expected_root.key || directory.level >= expected_root.level))
      || (root && directory.key != expected_root.key)
    {
      return Err(closure_error("quarantine_base_directory", "quarantine directory is outside the selected compacted graph").into());
    }
    let children = &self.levels[level];
    if children.len() != directory.entries.len() {
      return Err(
        order_error("quarantine_base_directory_order", "quarantine directory was observed before or after its exact postorder children")
          .into(),
      );
    }
    for (child, descriptor) in children.iter().zip(&directory.entries) {
      self.check_cancellation()?;
      if !child_matches_descriptor(child, descriptor) {
        return Err(
          closure_error(
            "quarantine_base_directory_closure",
            "quarantine directory descriptors do not match the observed immutable children",
          )
          .into(),
        );
      }
    }
    let children = std::mem::take(&mut self.levels[level]);
    let released_bytes = children
      .iter()
      .try_fold(0u64, |total, child| total.checked_add(child.accounted_bytes).ok_or(QuarantineClosureErrorV1::ArtifactLimit))?;
    drop(children);
    self.memory.shrink(released_bytes)?;
    let summary = child_summary_from_directory(directory, &mut self.memory)?;
    self.push_summary(level + 1, summary)
  }

  fn push_summary(&mut self, level: usize, summary: QuarantineChildSummaryV1) -> Result<(), QuarantineClosureErrorV1> {
    let pending = self
      .levels
      .get_mut(level)
      .ok_or_else(|| order_error("quarantine_base_directory_order", "quarantine directory graph exceeds the frozen depth"))?;
    if pending.len() >= MAXIMUM_GC_DIRECTORY_ENTRIES_V1 as usize {
      return Err(
        error(
          MalformedInputClass::AllocationAmplification,
          "quarantine_base_directory_children",
          "quarantine directory graph exceeds the frozen pending-child bound",
        )
        .into(),
      );
    }
    pending.try_reserve_exact(1)?;
    pending.push(summary);
    Ok(())
  }

  fn observe_delta_inner(&mut self, bytes: &[u8]) -> Result<(), QuarantineClosureErrorV1> {
    let delta = decode_candidate_delta_v1(bytes, self.algorithm)?;
    let expected_index = usize::try_from(self.delta_count)?;
    let hash_width = self.algorithm.hash_length();
    let expected_hash = self
      .manifest
      .delta_hashes
      .get(expected_index * hash_width..(expected_index + 1) * hash_width)
      .ok_or_else(|| closure_error("quarantine_delta_count", "observed more deltas than the manifest names"))?;
    let delta_position = (delta.mark_generation, delta.delta_ordinal);
    let prior_position = (self.last_delta_generation, self.last_delta_ordinal);
    if delta.database_id != self.manifest.database_id
      || delta.mark_generation > self.manifest.mark_generation
      || (self.delta_count != 0 && delta_position <= prior_position)
      || delta.key != expected_hash
    {
      return Err(closure_error("quarantine_delta_identity", "quarantine delta identity/order/hash differs from the manifest").into());
    }
    let predecessor_matches = if self.previous_delta_hash.is_empty() {
      delta.previous_delta_hash.is_none()
    } else {
      delta.previous_delta_hash == Some(self.previous_delta_hash.as_slice())
    };
    if !predecessor_matches {
      return Err(closure_error("quarantine_delta_predecessor", "quarantine delta predecessor does not close the ordered chain").into());
    }
    for record in delta.records()? {
      self.check_cancellation()?;
      record?;
    }
    let encoded_bytes = u64::try_from(bytes.len()).map_err(|error| length_error(format!("candidate delta byte count: {error}")))?;
    let delta_bytes = self.delta_bytes.checked_add(encoded_bytes).ok_or_else(|| length_error("candidate delta bytes overflow"))?;
    if delta_bytes > MAXIMUM_CANDIDATE_DELTA_BYTES {
      return Err(
        error(
          MalformedInputClass::AllocationAmplification,
          "quarantine_delta_bytes",
          "candidate deltas exceed the frozen 64 MiB compaction threshold",
        )
        .into(),
      );
    }
    self.delta_count = self.delta_count.checked_add(1).ok_or_else(|| length_error("quarantine delta count overflow"))?;
    self.delta_record_count = self
      .delta_record_count
      .checked_add(u64::from(delta.record_count))
      .ok_or_else(|| length_error("quarantine delta record count overflow"))?;
    self.last_delta_generation = delta.mark_generation;
    self.last_delta_ordinal = delta.delta_ordinal;
    self.delta_bytes = delta_bytes;
    self.previous_delta_hash.clear();
    self.previous_delta_hash.extend_from_slice(&delta.key);
    Ok(())
  }

  fn preflight(&self) -> Result<(), QuarantineClosureErrorV1> {
    if self.failed {
      return Err(QuarantineClosureErrorV1::Failed);
    }
    self.check_cancellation()?;
    self.memory.check_admission()?;
    Ok(())
  }

  fn preflight_observation(&mut self) -> Result<(), QuarantineClosureErrorV1> {
    if let Err(error) = self.preflight() {
      self.failed = true;
      return Err(error);
    }
    if self.support_artifact_count >= self.maximum_support_artifacts {
      self.failed = true;
      return Err(QuarantineClosureErrorV1::ArtifactLimit);
    }
    Ok(())
  }

  fn latch_observation(&mut self, result: Result<(), QuarantineClosureErrorV1>) -> Result<(), QuarantineClosureErrorV1> {
    match result {
      Ok(()) => {
        self.support_artifact_count = self.support_artifact_count.checked_add(1).ok_or(QuarantineClosureErrorV1::ArtifactLimit)?;
        Ok(())
      }
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }

  fn check_cancellation(&self) -> Result<(), QuarantineClosureErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(QuarantineClosureErrorV1::Canceled);
    }
    Ok(())
  }
}

fn child_summary_from_page(
  page: &GcStatePageV1<'_>,
  memory: &mut MemoryReservation,
) -> Result<QuarantineChildSummaryV1, QuarantineClosureErrorV1> {
  let accounted_bytes = child_summary_accounted_bytes(page.lower_fence, page.upper_fence, &page.key)?;
  memory.grow(accounted_bytes)?;
  Ok(QuarantineChildSummaryV1 {
    lower_fence: try_copy_bytes(page.lower_fence)?,
    upper_fence: try_copy_bytes(page.upper_fence)?,
    child_hash: try_copy_bytes(&page.key)?,
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
  memory: &mut MemoryReservation,
) -> Result<QuarantineChildSummaryV1, QuarantineClosureErrorV1> {
  let accounted_bytes = child_summary_accounted_bytes(directory.lower_fence, directory.upper_fence, &directory.key)?;
  memory.grow(accounted_bytes)?;
  Ok(QuarantineChildSummaryV1 {
    lower_fence: try_copy_bytes(directory.lower_fence)?,
    upper_fence: try_copy_bytes(directory.upper_fence)?,
    child_hash: try_copy_bytes(&directory.key)?,
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

fn child_summary_accounted_bytes(lower_fence: &[u8], upper_fence: &[u8], child_hash: &[u8]) -> Result<u64, QuarantineClosureErrorV1> {
  let byte_count = lower_fence
    .len()
    .checked_add(upper_fence.len())
    .and_then(|value| value.checked_add(child_hash.len()))
    .and_then(|value| value.checked_add(std::mem::size_of::<QuarantineChildSummaryV1>()))
    .ok_or(QuarantineClosureErrorV1::ArtifactLimit)?;
  Ok(u64::try_from(byte_count)?)
}

fn try_copy_bytes(source: &[u8]) -> Result<Vec<u8>, QuarantineClosureErrorV1> {
  let mut destination = Vec::new();
  destination.try_reserve_exact(source.len())?;
  destination.extend_from_slice(source);
  Ok(destination)
}

fn child_matches_descriptor(child: &QuarantineChildSummaryV1, descriptor: &GcStateDirectoryEntryV1<'_>) -> bool {
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

fn candidate_fence_order(left: &[u8], right: &[u8], algorithm: HashAlgorithm) -> FormatResult<Ordering> {
  let left = decode_physical_incarnation(left, algorithm)?;
  let right = decode_physical_incarnation(right, algorithm)?;
  Ok(compare_physical_incarnations_v1(&left, &right))
}

pub fn decode_physical_quarantine_candidate_v1(
  row: &[u8],
  algorithm: HashAlgorithm,
  clear: bool,
) -> FormatResult<PhysicalQuarantineCandidateV1<'_>> {
  let hash_width = algorithm.hash_length();
  let incarnation_length = 24 + 2 * hash_width;
  if row.len() != incarnation_length + 28 {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "candidate_row_length",
      "physical quarantine candidate has the wrong fixed length",
    ));
  }
  let incarnation = decode_physical_incarnation(&row[..incarnation_length], algorithm)?;
  let class = PhysicalQuarantineCandidateClassV1::from_u16(u16_at(row, incarnation_length)?)?;
  if u16_at(row, incarnation_length + 2)? != 0 {
    return Err(error(
      MalformedInputClass::NonzeroReservedOrPadding,
      "candidate_row_flags",
      "physical quarantine candidate flags must be zero",
    ));
  }
  let pending_since_ms = u64_at(row, incarnation_length + 4)?;
  let first_unreachable_generation = u64_at(row, incarnation_length + 12)?;
  let grace_at_pending_ms = u64_at(row, incarnation_length + 20)?;
  if (clear && (pending_since_ms != 0 || first_unreachable_generation != 0 || grace_at_pending_ms != 0))
    || (!clear && (pending_since_ms == 0 || first_unreachable_generation == 0))
  {
    return Err(closure_error("candidate_row_state", "candidate set/clear state fields disagree with the delta operation"));
  }
  Ok(PhysicalQuarantineCandidateV1 {
    hash_algorithm: algorithm,
    encoded: row,
    incarnation,
    class,
    pending_since_ms,
    first_unreachable_generation,
    grace_at_pending_ms,
  })
}

pub fn encode_physical_quarantine_candidate_v1(request: &PhysicalQuarantineCandidateWriteV1<'_>) -> FormatResult<Vec<u8>> {
  let hash_width = request.hash_algorithm.hash_length();
  if request.pending_since_ms == 0 || request.first_unreachable_generation == 0 {
    return Err(closure_error("candidate_row_state", "set candidate timing and first generation must be nonzero"));
  }
  let mut row = vec![0; 52 + 2 * hash_width];
  encode_physical_incarnation(&mut row[..24 + 2 * hash_width], &request.incarnation, hash_width)?;
  put_u16(&mut row, 24 + 2 * hash_width, request.class as u16);
  put_u64(&mut row, 28 + 2 * hash_width, request.pending_since_ms);
  put_u64(&mut row, 36 + 2 * hash_width, request.first_unreachable_generation);
  put_u64(&mut row, 44 + 2 * hash_width, request.grace_at_pending_ms);
  decode_physical_quarantine_candidate_v1(&row, request.hash_algorithm, false)?;
  Ok(row)
}

pub fn quarantine_candidate_records_v1<'a>(
  page: &'a GcStatePageV1<'a>,
  algorithm: HashAlgorithm,
) -> FormatResult<PhysicalQuarantineCandidateRecordsV1<'a>> {
  if page.role != GcDirectoryRoleV1::Candidates {
    return Err(closure_error("quarantine_candidate_page_role", "page is not physical quarantine candidate state"));
  }
  let row_length = 52 + 2 * algorithm.hash_length();
  let record_count = usize::try_from(page.record_count).map_err(|error| length_error(format!("candidate page record count: {error}")))?;
  let records_length = record_count.checked_mul(row_length).ok_or_else(|| length_error("candidate page records overflow"))?;
  if page.records.len() != records_length {
    return Err(closure_error("quarantine_candidate_page_count", "candidate page count does not close against its rows"));
  }
  Ok(PhysicalQuarantineCandidateRecordsV1 { rows: page.records.chunks_exact(row_length), algorithm })
}

#[derive(Debug)]
pub struct PhysicalQuarantineCandidateRecordsV1<'a> {
  rows: std::slice::ChunksExact<'a, u8>,
  algorithm: HashAlgorithm,
}

impl<'a> Iterator for PhysicalQuarantineCandidateRecordsV1<'a> {
  type Item = FormatResult<PhysicalQuarantineCandidateV1<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    self.rows.next().map(|row| decode_physical_quarantine_candidate_v1(row, self.algorithm, false))
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    self.rows.size_hint()
  }
}

impl ExactSizeIterator for PhysicalQuarantineCandidateRecordsV1<'_> {}

pub fn decode_candidate_delta_v1(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<CandidateDeltaV1<'_>> {
  let artifact = decode_gc_artifact_envelope(bytes)?;
  let hash_width = algorithm.hash_length();
  if artifact.kind != GcArtifactKindV1::CandidateDelta
    || artifact.identity.len() != 28
    || artifact.identity[..16].iter().all(|byte| *byte == 0)
    || u64_at(artifact.identity, 16)? != artifact.generation
    || u32_at(artifact.identity, 24)? == 0
    || artifact.body.len() < 16 + hash_width
  {
    return Err(closure_error("candidate_delta_identity", "candidate delta identity or body shape is invalid"));
  }
  let body = artifact.body;
  if u32_at(body, 0)? != 0 || u16_at(body, 4)? != 1 || u16_at(body, 6)? != 0 {
    return Err(closure_error("candidate_delta_header", "candidate delta flags, codec, or reserve are invalid"));
  }
  let record_count = u32_at(body, 8)?;
  let records_length = usize::try_from(u32_at(body, 12)?)
    .map_err(|error| length_error(format!("candidate delta records length conversion failed: {error}")))?;
  let record_length = 56usize.checked_add(2 * hash_width).ok_or_else(|| length_error("candidate delta record width overflow"))?;
  let record_count_usize = usize::try_from(record_count).map_err(|error| length_error(format!("candidate delta record count: {error}")))?;
  let expected_records_length =
    record_count_usize.checked_mul(record_length).ok_or_else(|| length_error("candidate delta records overflow"))?;
  if record_count == 0
    || 16usize.checked_add(hash_width).and_then(|length| length.checked_add(records_length)) != Some(body.len())
    || expected_records_length != records_length
  {
    return Err(closure_error("candidate_delta_count", "candidate delta count and record bytes disagree"));
  }
  let previous = &body[16..16 + hash_width];
  let records = &body[16 + hash_width..];
  let mut prior: Option<PhysicalQuarantineCandidateV1<'_>> = None;
  for row in records.chunks_exact(record_length) {
    let decoded = decode_candidate_delta_record_v1(row, algorithm)?;
    if prior.is_some_and(|prior| compare_physical_incarnations_v1(&prior.incarnation, &decoded.candidate.incarnation) != Ordering::Less) {
      return Err(order_error("candidate_delta_order", "candidate delta records are not strictly ordered by physical incarnation"));
    }
    prior = Some(decoded.candidate);
  }
  Ok(CandidateDeltaV1 {
    hash_algorithm: algorithm,
    database_id: &artifact.identity[..16],
    mark_generation: artifact.generation,
    delta_ordinal: u32_at(artifact.identity, 24)?,
    previous_delta_hash: (!previous.iter().all(|byte| *byte == 0)).then_some(previous),
    record_count,
    records,
    key: immutable_gc_artifact_key(algorithm, artifact.kind, bytes),
  })
}

pub fn encode_candidate_delta_v1(request: &CandidateDeltaWriteV1<'_>) -> FormatResult<EncodedImmutableGcArtifactV1> {
  let hash_width = request.hash_algorithm.hash_length();
  let record_count =
    u32::try_from(request.records.len()).map_err(|error| length_error(format!("candidate delta count exceeds u32: {error}")))?;
  if request.database_id.iter().all(|byte| *byte == 0)
    || request.mark_generation == 0
    || request.delta_ordinal == 0
    || request.records.is_empty()
  {
    return Err(closure_error("candidate_delta_identity", "candidate delta identity and records must be nonzero"));
  }
  let mut identity = Vec::with_capacity(28);
  identity.extend_from_slice(&request.database_id);
  identity.extend_from_slice(&request.mark_generation.to_le_bytes());
  identity.extend_from_slice(&request.delta_ordinal.to_le_bytes());
  let record_length = 56usize.checked_add(2 * hash_width).ok_or_else(|| length_error("candidate delta record width overflow"))?;
  let records_length = request.records.len().checked_mul(record_length).ok_or_else(|| length_error("candidate delta records overflow"))?;
  let records_length_u32 =
    u32::try_from(records_length).map_err(|error| length_error(format!("candidate delta bytes exceed u32: {error}")))?;
  let body_length = 16usize
    .checked_add(hash_width)
    .and_then(|length| length.checked_add(records_length))
    .ok_or_else(|| length_error("candidate delta body length overflow"))?;
  checked_immutable_gc_artifact_encoded_length(GcArtifactKindV1::CandidateDelta, 28, body_length)?;
  let mut prior: Option<PhysicalIncarnationV1<'_>> = None;
  for record in request.records {
    validate_candidate_delta_record_write_v1(record, request.hash_algorithm)?;
    if prior.is_some_and(|prior| compare_physical_incarnations_v1(&prior, &record.candidate.incarnation) != Ordering::Less) {
      return Err(order_error("candidate_delta_order", "candidate delta write records are not strictly ordered"));
    }
    prior = Some(record.candidate.incarnation);
  }

  let mut body = vec![0; body_length];
  put_u16(&mut body, 4, 1);
  put_u32(&mut body, 8, record_count);
  put_u32(&mut body, 12, records_length_u32);
  if let Some(previous) = request.previous_delta_hash {
    require_hash(previous, hash_width, "candidate_delta_predecessor")?;
    body[16..16 + hash_width].copy_from_slice(previous);
  }
  for (index, record) in request.records.iter().enumerate() {
    let start = 16 + hash_width + index * record_length;
    body[start] = match record.operation {
      CandidateDeltaOperationV1::Set => 1,
      CandidateDeltaOperationV1::Clear => 2,
    };
    let candidate_start = start + 4;
    match record.operation {
      CandidateDeltaOperationV1::Set => {
        let encoded = encode_physical_quarantine_candidate_v1(&record.candidate)?;
        body[candidate_start..candidate_start + encoded.len()].copy_from_slice(&encoded);
      }
      CandidateDeltaOperationV1::Clear => {
        encode_physical_incarnation(
          &mut body[candidate_start..candidate_start + 24 + 2 * hash_width],
          &record.candidate.incarnation,
          hash_width,
        )?;
        put_u16(&mut body, candidate_start + 24 + 2 * hash_width, record.candidate.class as u16);
      }
    }
  }
  let encoded = encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 {
    kind: GcArtifactKindV1::CandidateDelta,
    hash_algorithm: request.hash_algorithm,
    generation: request.mark_generation,
    identity: &identity,
    body: &body,
  })?;
  decode_candidate_delta_v1(&encoded.value, request.hash_algorithm)?;
  Ok(encoded)
}

pub fn decode_quarantine_manifest_v1(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<QuarantineManifestV1<'_>> {
  if bytes.len() > MAXIMUM_QUARANTINE_MANIFEST_LENGTH {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "quarantine_manifest_length",
      "quarantine manifest exceeds its fixed cap",
    ));
  }
  let artifact = decode_gc_artifact_envelope(bytes)?;
  let hash_width = algorithm.hash_length();
  if artifact.kind != GcArtifactKindV1::QuarantineManifest
    || artifact.identity.len() != 24
    || artifact.identity[..16].iter().all(|byte| *byte == 0)
    || u64_at(artifact.identity, 16)? != artifact.generation
    || artifact.body.len() < 100 + 6 * hash_width
  {
    return Err(closure_error("quarantine_manifest_identity", "quarantine manifest identity or body shape is invalid"));
  }
  let body = artifact.body;
  let required_capabilities = &body[4..36];
  if u32_at(body, 0)? != 0
    || !capabilities_exact(required_capabilities, &[12, 13, 15, 17])
    || u64_at(body, 36)? != artifact.generation
    || u64_at(body, 44)? == 0
    || (0..4).any(|index| body[52 + index * hash_width..52 + (index + 1) * hash_width].iter().all(|byte| *byte == 0))
  {
    return Err(closure_error("quarantine_manifest_header", "quarantine manifest capture authority is invalid"));
  }
  let candidate_root = &body[52 + 4 * hash_width..52 + 5 * hash_width];
  let lifecycle = &body[52 + 5 * hash_width..52 + 6 * hash_width];
  let delta_count = u32_at(body, 52 + 6 * hash_width)?;
  let delta_count_usize = usize::try_from(delta_count).map_err(|error| length_error(format!("quarantine delta count: {error}")))?;
  let delta_hashes_length = delta_count_usize.checked_mul(hash_width).ok_or_else(|| length_error("quarantine delta hashes overflow"))?;
  let expected_body_length = 100usize
    .checked_add(6 * hash_width)
    .and_then(|start| start.checked_add(delta_hashes_length))
    .ok_or_else(|| length_error("quarantine manifest body length overflow"))?;
  if delta_count_usize > MAXIMUM_CANDIDATE_DELTAS
    || body[56 + 6 * hash_width..60 + 6 * hash_width].iter().any(|byte| *byte != 0)
    || expected_body_length != body.len()
  {
    return Err(closure_error("quarantine_manifest_formula", "quarantine manifest delta framing or reserve is invalid"));
  }
  let candidate_count = u64_at(body, 60 + 6 * hash_width)?;
  let candidate_bytes = u64_at(body, 68 + 6 * hash_width)?;
  let eligible_count_hint = u64_at(body, 76 + 6 * hash_width)?;
  let eligible_bytes_hint = u64_at(body, 84 + 6 * hash_width)?;
  let next_candidate_page_id = u64_at(body, 92 + 6 * hash_width)?;
  let delta_hashes = &body[100 + 6 * hash_width..];
  if lifecycle.iter().all(|byte| *byte == 0)
    || next_candidate_page_id == 0
    || eligible_count_hint > candidate_count
    || eligible_bytes_hint > candidate_bytes
    || (candidate_count == 0) != (candidate_bytes == 0)
    || (eligible_count_hint == 0) != (eligible_bytes_hint == 0)
    || (candidate_count != 0 && candidate_root.iter().all(|byte| *byte == 0) && delta_count == 0)
    || delta_hashes.chunks_exact(hash_width).any(|hash| hash.iter().all(|byte| *byte == 0))
  {
    return Err(closure_error("quarantine_manifest_state", "quarantine manifest roots, counts, bytes, or hints disagree"));
  }
  Ok(QuarantineManifestV1 {
    hash_algorithm: algorithm,
    database_id: &artifact.identity[..16],
    mark_generation: artifact.generation,
    completed_at_ms: u64_at(body, 44)?,
    required_capabilities,
    authority_root_set_digest: &body[52..52 + hash_width],
    semantic_state_digest: &body[52 + hash_width..52 + 2 * hash_width],
    kv_layout_fingerprint: &body[52 + 2 * hash_width..52 + 3 * hash_width],
    mark_result_digest: &body[52 + 3 * hash_width..52 + 4 * hash_width],
    candidate_directory_root: (!candidate_root.iter().all(|byte| *byte == 0)).then_some(candidate_root),
    captured_root_lifecycle_manifest: lifecycle,
    delta_count,
    candidate_count,
    candidate_bytes,
    eligible_count_hint,
    eligible_bytes_hint,
    next_candidate_page_id,
    delta_hashes,
    key: immutable_gc_artifact_key(algorithm, artifact.kind, bytes),
  })
}

pub fn encode_quarantine_manifest_v1(request: &QuarantineManifestWriteV1<'_>) -> FormatResult<EncodedImmutableGcArtifactV1> {
  let hash_width = request.hash_algorithm.hash_length();
  let delta_count = request.delta_hashes.len().checked_div(hash_width).ok_or_else(|| length_error("zero quarantine hash width"))?;
  let delta_count_u32 = u32::try_from(delta_count).map_err(|error| length_error(format!("quarantine delta count exceeds u32: {error}")))?;
  if request.database_id.iter().all(|byte| *byte == 0)
    || request.mark_generation == 0
    || request.completed_at_ms == 0
    || request.required_capabilities.len() != 32
    || delta_count > MAXIMUM_CANDIDATE_DELTAS
    || delta_count * hash_width != request.delta_hashes.len()
  {
    return Err(closure_error(
      "quarantine_manifest_identity",
      "quarantine manifest write identity, time, capabilities, or deltas are invalid",
    ));
  }
  for hash in [
    request.authority_root_set_digest,
    request.semantic_state_digest,
    request.kv_layout_fingerprint,
    request.mark_result_digest,
    request.captured_root_lifecycle_manifest,
  ] {
    require_hash(hash, hash_width, "quarantine_manifest_hash")?;
  }
  if let Some(root) = request.candidate_directory_root {
    require_hash(root, hash_width, "quarantine_candidate_directory")?;
  }
  if !capabilities_exact(request.required_capabilities, &[12, 13, 15, 17]) {
    return Err(closure_error("quarantine_manifest_capabilities", "quarantine manifest capabilities are not the frozen exact set"));
  }
  let mut identity = Vec::with_capacity(24);
  identity.extend_from_slice(&request.database_id);
  identity.extend_from_slice(&request.mark_generation.to_le_bytes());
  let mut body = vec![0; 100 + 6 * hash_width + request.delta_hashes.len()];
  body[4..36].copy_from_slice(request.required_capabilities);
  put_u64(&mut body, 36, request.mark_generation);
  put_u64(&mut body, 44, request.completed_at_ms);
  body[52..52 + hash_width].copy_from_slice(request.authority_root_set_digest);
  body[52 + hash_width..52 + 2 * hash_width].copy_from_slice(request.semantic_state_digest);
  body[52 + 2 * hash_width..52 + 3 * hash_width].copy_from_slice(request.kv_layout_fingerprint);
  body[52 + 3 * hash_width..52 + 4 * hash_width].copy_from_slice(request.mark_result_digest);
  if let Some(root) = request.candidate_directory_root {
    body[52 + 4 * hash_width..52 + 5 * hash_width].copy_from_slice(root);
  }
  body[52 + 5 * hash_width..52 + 6 * hash_width].copy_from_slice(request.captured_root_lifecycle_manifest);
  put_u32(&mut body, 52 + 6 * hash_width, delta_count_u32);
  put_u64(&mut body, 60 + 6 * hash_width, request.candidate_count);
  put_u64(&mut body, 68 + 6 * hash_width, request.candidate_bytes);
  put_u64(&mut body, 76 + 6 * hash_width, request.eligible_count_hint);
  put_u64(&mut body, 84 + 6 * hash_width, request.eligible_bytes_hint);
  put_u64(&mut body, 92 + 6 * hash_width, request.next_candidate_page_id);
  body[100 + 6 * hash_width..].copy_from_slice(request.delta_hashes);
  let encoded = encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 {
    kind: GcArtifactKindV1::QuarantineManifest,
    hash_algorithm: request.hash_algorithm,
    generation: request.mark_generation,
    identity: &identity,
    body: &body,
  })?;
  decode_quarantine_manifest_v1(&encoded.value, request.hash_algorithm)?;
  Ok(encoded)
}

fn decode_candidate_delta_record_v1(row: &[u8], algorithm: HashAlgorithm) -> FormatResult<CandidateDeltaRecordV1<'_>> {
  if row.len() != 56 + 2 * algorithm.hash_length() {
    return Err(closure_error("candidate_delta_record_length", "candidate delta record has the wrong fixed width"));
  }
  let operation = match row[0] {
    1 => CandidateDeltaOperationV1::Set,
    2 => CandidateDeltaOperationV1::Clear,
    value => {
      return Err(error(
        MalformedInputClass::UnknownTypeKindOrEnum,
        "candidate_delta_operation",
        format!("unknown candidate delta operation {value}"),
      ));
    }
  };
  if row[1..4].iter().any(|byte| *byte != 0) {
    return Err(error(
      MalformedInputClass::NonzeroReservedOrPadding,
      "candidate_delta_operation",
      "candidate delta operation reserve bytes must be zero",
    ));
  }
  let candidate = decode_physical_quarantine_candidate_v1(&row[4..], algorithm, operation == CandidateDeltaOperationV1::Clear)?;
  Ok(CandidateDeltaRecordV1 { operation, candidate })
}

fn validate_candidate_delta_record_write_v1(record: &CandidateDeltaRecordWriteV1<'_>, algorithm: HashAlgorithm) -> FormatResult<()> {
  if record.candidate.hash_algorithm != algorithm {
    return Err(closure_error("candidate_delta_hash_profile", "candidate delta rows use a different hash profile"));
  }
  match record.operation {
    CandidateDeltaOperationV1::Set => {
      encode_physical_quarantine_candidate_v1(&record.candidate)?;
    }
    CandidateDeltaOperationV1::Clear => {
      if record.candidate.pending_since_ms != 0
        || record.candidate.first_unreachable_generation != 0
        || record.candidate.grace_at_pending_ms != 0
      {
        return Err(closure_error("candidate_row_state", "clear candidate timing and generation fields must be zero"));
      }
      let mut incarnation = vec![0; 24 + 2 * algorithm.hash_length()];
      encode_physical_incarnation(&mut incarnation, &record.candidate.incarnation, algorithm.hash_length())?;
    }
  }
  Ok(())
}

fn encode_physical_incarnation(destination: &mut [u8], incarnation: &PhysicalIncarnationV1<'_>, hash_width: usize) -> FormatResult<()> {
  if destination.len() != 24 + 2 * hash_width
    || incarnation.logical_key.len() != hash_width
    || incarnation.integrity_or_legacy_digest.len() != hash_width
  {
    return Err(closure_error("physical_incarnation_length", "physical incarnation write has a mismatched hash width"));
  }
  destination[..hash_width].copy_from_slice(incarnation.logical_key);
  destination[hash_width..2 * hash_width].copy_from_slice(incarnation.integrity_or_legacy_digest);
  put_u64(destination, 2 * hash_width, incarnation.wal_offset);
  put_u64(destination, 2 * hash_width + 8, incarnation.write_sequence);
  put_u32(destination, 2 * hash_width + 16, incarnation.entity_length);
  destination[2 * hash_width + 20] = incarnation.entry_type;
  destination[2 * hash_width + 21] = incarnation.entity_version;
  decode_physical_incarnation(
    destination,
    match hash_width {
      32 => HashAlgorithm::Blake3_256,
      64 => HashAlgorithm::Sha512,
      _ => return Err(closure_error("physical_incarnation_length", "unsupported physical incarnation hash width")),
    },
  )?;
  Ok(())
}

fn capabilities_exact(capabilities: &[u8], enabled: &[usize]) -> bool {
  capabilities.len() == 32
    && capabilities.iter().enumerate().all(|(index, byte)| {
      let expected = (0..8).fold(0u8, |value, bit| {
        let capability = index * 8 + bit;
        if enabled.contains(&capability) {
          value | (1 << bit)
        } else {
          value
        }
      });
      *byte == expected
    })
}

fn require_hash(hash: &[u8], width: usize, code: &'static str) -> FormatResult<()> {
  if hash.len() != width || hash.iter().all(|byte| *byte == 0) {
    return Err(error(MalformedInputClass::IdentityKeyOrGenerationMismatch, code, "hash has the wrong width or is zero"));
  }
  Ok(())
}

fn u16_at(bytes: &[u8], offset: usize) -> FormatResult<u16> {
  let value = bytes.get(offset..offset + 2).ok_or_else(|| length_error("u16 field is truncated"))?;
  let mut encoded = [0u8; 2];
  encoded.copy_from_slice(value);
  Ok(u16::from_le_bytes(encoded))
}

fn u32_at(bytes: &[u8], offset: usize) -> FormatResult<u32> {
  let value = bytes.get(offset..offset + 4).ok_or_else(|| length_error("u32 field is truncated"))?;
  let mut encoded = [0u8; 4];
  encoded.copy_from_slice(value);
  Ok(u32::from_le_bytes(encoded))
}

fn u64_at(bytes: &[u8], offset: usize) -> FormatResult<u64> {
  let value = bytes.get(offset..offset + 8).ok_or_else(|| length_error("u64 field is truncated"))?;
  let mut encoded = [0u8; 8];
  encoded.copy_from_slice(value);
  Ok(u64::from_le_bytes(encoded))
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

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}

fn closure_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::CrossRecordClosureMismatch, code, context)
}

fn order_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::NoncanonicalOrderOrDuplicate, code, context)
}

fn length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "gc_quarantine_length", context)
}
