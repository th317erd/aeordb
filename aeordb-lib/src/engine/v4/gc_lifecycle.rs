use std::cmp::Ordering;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::gc::{
  EncodedImmutableGcArtifactV1, GcArtifactKindV1, ImmutableGcArtifactWriteV1, decode_gc_artifact_envelope, encode_immutable_gc_artifact,
  immutable_gc_artifact_key, u16_at, u64_at,
};
use super::gc_state::{
  GcDirectoryRoleV1, GcStateDirectoryV1, GcStatePageV1, RootCandidateRecordV1, RootExpiryRecordV1, RootExpiryStateV1,
  decode_gc_state_artifact, decode_root_candidate_record_v1, decode_root_expiry_record_v1,
};
use super::reader::{FormatError, FormatResult, MalformedInputClass};
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
