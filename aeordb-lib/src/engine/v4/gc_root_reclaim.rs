use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::gc::EncodedImmutableGcArtifactV1;
use super::gc_lifecycle::{
  RootExpiryManifestV1, RootExpiryRecordWriteV1, RootLifecycleManifestV1, RootObjectReclaimProofWriteV1, RootRetirementCommitV1,
  encode_root_expiry_record_v1, encode_root_object_reclaim_proof_v1, validate_root_expiry_reclaim_proof, root_expiry_result_digest_start,
  root_expiry_result_digest_step, validate_root_expiry_retirement_commit, validate_root_lifecycle_expiry_manifest,
};
use super::gc_state::{PhysicalInventoryManifestV1, RootExpiryRecordV1, RootExpiryStateV1, decode_root_expiry_record_v1};
use super::reader::FormatError;
use crate::engine::HashAlgorithm;

#[derive(Debug, Clone, Copy)]
pub struct RootObjectReclaimQualificationRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub prior_expiry: &'a RootExpiryRecordV1<'a>,
  pub retirement: &'a RootRetirementCommitV1<'a>,
  pub final_physical_inventory: &'a PhysicalInventoryManifestV1<'a>,
  pub proof_id: &'a [u8],
  pub reclaimed_at_ms: i64,
  pub latest_sweep_receipt_generation: u64,
  pub root_object_incarnation_digest: &'a [u8],
  pub root_object_incarnation_count: u64,
  pub sweep_receipt_merkle_root: &'a [u8],
  pub sweep_receipt_count: u64,
  pub absence_digest: &'a [u8],
  pub retention_ms: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct RootObjectReclaimEvidenceVerificationRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8],
  pub namespace_root_hash: &'a [u8],
  pub retirement_commit_hash: &'a [u8],
  pub final_physical_inventory: &'a PhysicalInventoryManifestV1<'a>,
  pub final_physical_inventory_generation: u64,
  pub latest_sweep_receipt_generation: u64,
  pub root_object_incarnation_digest: &'a [u8],
  pub root_object_incarnation_count: u64,
  pub sweep_receipt_merkle_root: &'a [u8],
  pub sweep_receipt_count: u64,
  pub absence_digest: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct RootObjectReclaimEvidenceVerificationErrorV1 {
  code: String,
  message: String,
}

impl RootObjectReclaimEvidenceVerificationErrorV1 {
  pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
    Self { code: code.into(), message: message.into() }
  }

  pub fn code(&self) -> &str {
    if self.code.is_empty() {
      "root_reclaim_evidence"
    } else {
      self.code.as_str()
    }
  }
}

pub trait RootObjectReclaimEvidenceVerifierV1 {
  /// Verifies the caller-owned physical inventory, exact incarnation set,
  /// durable receipt set, and final absence proof. P4-5/P4-6 own the concrete
  /// implementation; this lifecycle boundary only binds accepted evidence.
  fn verify_root_object_reclaim(
    &mut self,
    request: RootObjectReclaimEvidenceVerificationRequestV1<'_>,
  ) -> Result<(), RootObjectReclaimEvidenceVerificationErrorV1>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualifiedRootObjectReclaimV1 {
  encoded_proof: EncodedImmutableGcArtifactV1,
  encoded_expiry_record: Vec<u8>,
  evidence_expires_at_ms: i64,
}

impl QualifiedRootObjectReclaimV1 {
  pub fn encoded_proof(&self) -> &EncodedImmutableGcArtifactV1 {
    &self.encoded_proof
  }

  pub fn encoded_expiry_record(&self) -> &[u8] {
    &self.encoded_expiry_record
  }

  pub const fn evidence_expires_at_ms(&self) -> i64 {
    self.evidence_expires_at_ms
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootExpiryRetentionCutoffV1 {
  pub evidence_expires_at_ms: i64,
  pub namespace_root_hash: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootExpiryRetentionSelectionV1 {
  KeepAll,
  AtOrAfter(RootExpiryRetentionCutoffV1),
}

#[derive(Debug)]
pub struct RootExpiryRetentionContextV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub prior_lifecycle: &'a RootLifecycleManifestV1<'a>,
  pub prior_expiry: &'a RootExpiryManifestV1<'a>,
  pub lifecycle_generation: u64,
  pub completed_at_ms: i64,
  pub retention_ms: u64,
  pub optional_byte_budget: u64,
  pub maximum_records: u64,
  pub selection: RootExpiryRetentionSelectionV1,
  pub qualified_reclaim: &'a QualifiedRootObjectReclaimV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootExpiryRetentionActionV1 {
  RetainedMandatory,
  RetainedOptional,
  ReclaimedAndRetained,
  DroppedExpired,
  DroppedForBudget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootExpiryRetentionSummaryV1 {
  pub prior_count: u64,
  pub prior_bytes: u64,
  pub resulting_count: u64,
  pub resulting_bytes: u64,
  pub resulting_mandatory_count: u64,
  pub resulting_mandatory_bytes: u64,
  pub resulting_optional_count: u64,
  pub resulting_optional_bytes: u64,
  pub oldest_retired_at_ms: Option<i64>,
  pub newest_retired_at_ms: Option<i64>,
  pub expired_count: u64,
  pub budget_evicted_count: u64,
  pub reclaimed_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootExpiryRetentionPermitV1 {
  summary: RootExpiryRetentionSummaryV1,
  hash_algorithm: HashAlgorithm,
  database_id: [u8; 16],
  prior_lifecycle_manifest_hash: Vec<u8>,
  prior_expiry_manifest_hash: Vec<u8>,
  lifecycle_generation: u64,
  completed_at_ms: i64,
  retention_ms: u64,
  optional_byte_budget: u64,
  namespace_root_hash: Vec<u8>,
  root_object_reclaim_proof_hash: Vec<u8>,
  resulting_expiry_records_digest: Vec<u8>,
}

impl RootExpiryRetentionPermitV1 {
  pub const fn summary(&self) -> &RootExpiryRetentionSummaryV1 {
    &self.summary
  }

  pub const fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }

  pub const fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  pub fn prior_lifecycle_manifest_hash(&self) -> &[u8] {
    &self.prior_lifecycle_manifest_hash
  }

  pub fn prior_expiry_manifest_hash(&self) -> &[u8] {
    &self.prior_expiry_manifest_hash
  }

  pub const fn lifecycle_generation(&self) -> u64 {
    self.lifecycle_generation
  }

  pub const fn completed_at_ms(&self) -> i64 {
    self.completed_at_ms
  }

  pub const fn retention_ms(&self) -> u64 {
    self.retention_ms
  }

  pub const fn optional_byte_budget(&self) -> u64 {
    self.optional_byte_budget
  }

  pub fn namespace_root_hash(&self) -> &[u8] {
    &self.namespace_root_hash
  }

  pub fn root_object_reclaim_proof_hash(&self) -> &[u8] {
    &self.root_object_reclaim_proof_hash
  }

  pub fn resulting_expiry_records_digest(&self) -> &[u8] {
    &self.resulting_expiry_records_digest
  }
}

#[derive(Debug, Error)]
pub enum RootExpiryRetentionErrorV1 {
  #[error("invalid root-expiry retention configuration: {0}")]
  InvalidConfiguration(&'static str),
  #[error("root-expiry retention was canceled")]
  Canceled,
  #[error("root-expiry retention generation is stale")]
  StaleGeneration,
  #[error("root-expiry retention input does not close against its selected manifest")]
  ManifestAggregate,
  #[error("root-expiry retention rows are not in strict NamespaceRoot order")]
  RecordOrder,
  #[error("root-expiry retention exceeded its record limit")]
  RecordLimit,
  #[error("root-expiry retention did not apply and retain the exact qualified reclaim")]
  Target,
  #[error("root-expiry retained optional evidence exceeds its byte budget")]
  Budget,
  #[error("root-expiry retention evicted evidence even though another row fits")]
  Nonmaximal,
  #[error("root-expiry retention accounting overflowed or underflowed")]
  Arithmetic,
  #[error("root-expiry retention integer conversion failed: {0}")]
  IntegerConversion(#[from] std::num::TryFromIntError),
  #[error(transparent)]
  Format(Box<FormatError>),
  #[error("root-expiry retention model has already failed")]
  Failed,
}

impl RootExpiryRetentionErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::InvalidConfiguration(_) => "root_expiry_retention_configuration",
      Self::Canceled => "root_expiry_retention_canceled",
      Self::StaleGeneration => "root_expiry_retention_generation",
      Self::ManifestAggregate => "root_expiry_retention_manifest",
      Self::RecordOrder => "root_expiry_retention_order",
      Self::RecordLimit => "root_expiry_retention_limit",
      Self::Target => "root_expiry_retention_target",
      Self::Budget => "root_expiry_retention_budget",
      Self::Nonmaximal => "root_expiry_retention_nonmaximal",
      Self::Arithmetic => "root_expiry_retention_arithmetic",
      Self::IntegerConversion(_) => "root_expiry_retention_arithmetic",
      Self::Format(source) => source.code(),
      Self::Failed => "root_expiry_retention_failed",
    }
  }
}

impl From<FormatError> for RootExpiryRetentionErrorV1 {
  fn from(error: FormatError) -> Self {
    Self::Format(Box::new(error))
  }
}

#[derive(Debug)]
pub struct RootExpiryRetentionModelV1<'a> {
  context: RootExpiryRetentionContextV1<'a>,
  cancellation: &'a CancellationToken,
  row_bytes: u64,
  target_root_hash: Vec<u8>,
  target_retired_at_ms: i64,
  target_last_pending_since_ms: i64,
  target_final_mark_generation: u64,
  target_reason: u16,
  target_retirement_commit_hash: Vec<u8>,
  target_evidence_expires_at_ms: i64,
  target_expiry_record: Vec<u8>,
  resulting_expiry_records_digest: Vec<u8>,
  previous_root_hash: Vec<u8>,
  prior_count: u64,
  prior_bytes: u64,
  prior_mandatory_count: u64,
  prior_mandatory_bytes: u64,
  prior_optional_count: u64,
  prior_optional_bytes: u64,
  prior_oldest_retired_at_ms: Option<i64>,
  prior_newest_retired_at_ms: Option<i64>,
  resulting_mandatory_count: u64,
  resulting_mandatory_bytes: u64,
  resulting_optional_count: u64,
  resulting_optional_bytes: u64,
  oldest_retained_optional_expires_at_ms: Option<i64>,
  oldest_retained_optional_root_hash: Vec<u8>,
  resulting_oldest_retired_at_ms: Option<i64>,
  resulting_newest_retired_at_ms: Option<i64>,
  expired_count: u64,
  budget_evicted_count: u64,
  reclaimed_count: u64,
  target_seen: bool,
  failed: bool,
}

impl<'a> RootExpiryRetentionModelV1<'a> {
  pub fn new(context: RootExpiryRetentionContextV1<'a>, cancellation: &'a CancellationToken) -> Result<Self, RootExpiryRetentionErrorV1> {
    if cancellation.is_cancelled() {
      return Err(RootExpiryRetentionErrorV1::Canceled);
    }
    if context.maximum_records == 0 || context.retention_ms == 0 || context.completed_at_ms <= 0 {
      return Err(RootExpiryRetentionErrorV1::InvalidConfiguration("record limit, retention, and completion time must be positive"));
    }
    if context.lifecycle_generation <= context.prior_lifecycle.generation
      || context.prior_expiry.generation != context.prior_lifecycle.generation
    {
      return Err(RootExpiryRetentionErrorV1::StaleGeneration);
    }
    validate_root_lifecycle_expiry_manifest(context.prior_lifecycle, context.prior_expiry)?;
    let hash_width = context.hash_algorithm.hash_length();
    if context.prior_lifecycle.database_id != context.prior_expiry.database_id
      || context.prior_lifecycle.database_id.len() != 16
      || !valid_hash(context.prior_lifecycle.key.as_slice(), hash_width)
      || !valid_hash(context.prior_expiry.key.as_slice(), hash_width)
    {
      return Err(RootExpiryRetentionErrorV1::ManifestAggregate);
    }
    let row_bytes = u64::try_from(40usize + 3 * hash_width)?;
    validate_prior_expiry_aggregates(context.prior_expiry, row_bytes, context.maximum_records)?;
    if let RootExpiryRetentionSelectionV1::AtOrAfter(cutoff) = &context.selection {
      if cutoff.evidence_expires_at_ms <= context.completed_at_ms || !valid_hash(&cutoff.namespace_root_hash, hash_width) {
        return Err(RootExpiryRetentionErrorV1::InvalidConfiguration(
          "retention cutoff must identify nonexpired evidence with a valid root hash",
        ));
      }
    }

    let target_replacement = decode_root_expiry_record_v1(context.qualified_reclaim.encoded_expiry_record(), context.hash_algorithm)?;
    let target_proof =
      super::gc_lifecycle::decode_root_object_reclaim_proof_v1(&context.qualified_reclaim.encoded_proof.value, context.hash_algorithm)?;
    validate_root_expiry_reclaim_proof(&target_replacement, &target_proof)?;
    let target_evidence_expires_at_ms = target_replacement.evidence_expires_at_ms.ok_or(RootExpiryRetentionErrorV1::Target)?;
    let retention_ms = i64::try_from(context.retention_ms)?;
    if target_proof.database_id != context.prior_lifecycle.database_id
      || target_proof.reclaimed_at_ms > context.completed_at_ms
      || target_proof.reclaimed_at_ms.checked_add(retention_ms) != Some(target_evidence_expires_at_ms)
      || target_evidence_expires_at_ms <= context.completed_at_ms
    {
      return Err(RootExpiryRetentionErrorV1::Target);
    }
    let target_expiry_record = context.qualified_reclaim.encoded_expiry_record().to_vec();
    let resulting_expiry_records_digest = root_expiry_result_digest_start(context.hash_algorithm);

    Ok(Self {
      context,
      cancellation,
      row_bytes,
      target_root_hash: target_replacement.namespace_root_hash.to_vec(),
      target_retired_at_ms: target_replacement.retired_at_ms,
      target_last_pending_since_ms: target_replacement.last_pending_since_ms,
      target_final_mark_generation: target_replacement.final_mark_generation,
      target_reason: target_replacement.reason,
      target_retirement_commit_hash: target_replacement.retirement_commit_hash.to_vec(),
      target_evidence_expires_at_ms,
      target_expiry_record,
      resulting_expiry_records_digest,
      previous_root_hash: Vec::with_capacity(hash_width),
      prior_count: 0,
      prior_bytes: 0,
      prior_mandatory_count: 0,
      prior_mandatory_bytes: 0,
      prior_optional_count: 0,
      prior_optional_bytes: 0,
      prior_oldest_retired_at_ms: None,
      prior_newest_retired_at_ms: None,
      resulting_mandatory_count: 0,
      resulting_mandatory_bytes: 0,
      resulting_optional_count: 0,
      resulting_optional_bytes: 0,
      oldest_retained_optional_expires_at_ms: None,
      oldest_retained_optional_root_hash: Vec::with_capacity(hash_width),
      resulting_oldest_retired_at_ms: None,
      resulting_newest_retired_at_ms: None,
      expired_count: 0,
      budget_evicted_count: 0,
      reclaimed_count: 0,
      target_seen: false,
      failed: false,
    })
  }

  pub fn observe(&mut self, record: &RootExpiryRecordV1<'_>) -> Result<RootExpiryRetentionActionV1, RootExpiryRetentionErrorV1> {
    if self.failed {
      return Err(RootExpiryRetentionErrorV1::Failed);
    }
    match self.observe_inner(record) {
      Ok(action) => Ok(action),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }

  pub fn finish(self) -> Result<RootExpiryRetentionPermitV1, RootExpiryRetentionErrorV1> {
    if self.failed {
      return Err(RootExpiryRetentionErrorV1::Failed);
    }
    if self.cancellation.is_cancelled() {
      return Err(RootExpiryRetentionErrorV1::Canceled);
    }
    if !self.target_seen || self.reclaimed_count != 1 {
      return Err(RootExpiryRetentionErrorV1::Target);
    }
    if self.prior_count != self.context.prior_expiry.record_count
      || self.prior_bytes != self.context.prior_expiry.logical_bytes
      || self.prior_mandatory_count != self.context.prior_expiry.mandatory_count
      || self.prior_mandatory_bytes != self.context.prior_expiry.mandatory_bytes
      || self.prior_optional_count != self.context.prior_expiry.optional_count
      || self.prior_optional_bytes != self.context.prior_expiry.optional_bytes
      || self.prior_oldest_retired_at_ms != self.context.prior_expiry.oldest_retired_at_ms
      || self.prior_newest_retired_at_ms != self.context.prior_expiry.newest_retired_at_ms
    {
      return Err(RootExpiryRetentionErrorV1::ManifestAggregate);
    }
    if self.resulting_optional_bytes > self.context.optional_byte_budget {
      return Err(RootExpiryRetentionErrorV1::Budget);
    }
    if self.budget_evicted_count != 0
      && self.resulting_optional_bytes.checked_add(self.row_bytes).is_some_and(|bytes| bytes <= self.context.optional_byte_budget)
    {
      return Err(RootExpiryRetentionErrorV1::Nonmaximal);
    }
    if self.budget_evicted_count == 0 && !matches!(self.context.selection, RootExpiryRetentionSelectionV1::KeepAll) {
      return Err(RootExpiryRetentionErrorV1::Nonmaximal);
    }
    if let RootExpiryRetentionSelectionV1::AtOrAfter(cutoff) = &self.context.selection {
      if self.oldest_retained_optional_expires_at_ms != Some(cutoff.evidence_expires_at_ms)
        || self.oldest_retained_optional_root_hash != cutoff.namespace_root_hash
      {
        return Err(RootExpiryRetentionErrorV1::Nonmaximal);
      }
    }
    let resulting_count =
      self.resulting_mandatory_count.checked_add(self.resulting_optional_count).ok_or(RootExpiryRetentionErrorV1::Arithmetic)?;
    let resulting_bytes =
      self.resulting_mandatory_bytes.checked_add(self.resulting_optional_bytes).ok_or(RootExpiryRetentionErrorV1::Arithmetic)?;
    let summary = RootExpiryRetentionSummaryV1 {
      prior_count: self.prior_count,
      prior_bytes: self.prior_bytes,
      resulting_count,
      resulting_bytes,
      resulting_mandatory_count: self.resulting_mandatory_count,
      resulting_mandatory_bytes: self.resulting_mandatory_bytes,
      resulting_optional_count: self.resulting_optional_count,
      resulting_optional_bytes: self.resulting_optional_bytes,
      oldest_retired_at_ms: self.resulting_oldest_retired_at_ms,
      newest_retired_at_ms: self.resulting_newest_retired_at_ms,
      expired_count: self.expired_count,
      budget_evicted_count: self.budget_evicted_count,
      reclaimed_count: self.reclaimed_count,
    };
    let mut database_id = [0u8; 16];
    database_id.copy_from_slice(self.context.prior_lifecycle.database_id);
    let target_proof = super::gc_lifecycle::decode_root_object_reclaim_proof_v1(
      &self.context.qualified_reclaim.encoded_proof.value,
      self.context.hash_algorithm,
    )?;
    Ok(RootExpiryRetentionPermitV1 {
      summary,
      hash_algorithm: self.context.hash_algorithm,
      database_id,
      prior_lifecycle_manifest_hash: self.context.prior_lifecycle.key.clone(),
      prior_expiry_manifest_hash: self.context.prior_expiry.key.clone(),
      lifecycle_generation: self.context.lifecycle_generation,
      completed_at_ms: self.context.completed_at_ms,
      retention_ms: self.context.retention_ms,
      optional_byte_budget: self.context.optional_byte_budget,
      namespace_root_hash: self.target_root_hash,
      root_object_reclaim_proof_hash: target_proof.key,
      resulting_expiry_records_digest: self.resulting_expiry_records_digest,
    })
  }

  fn observe_inner(&mut self, record: &RootExpiryRecordV1<'_>) -> Result<RootExpiryRetentionActionV1, RootExpiryRetentionErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(RootExpiryRetentionErrorV1::Canceled);
    }
    if self.prior_count >= self.context.maximum_records {
      return Err(RootExpiryRetentionErrorV1::RecordLimit);
    }
    if !self.previous_root_hash.is_empty() && self.previous_root_hash.as_slice() >= record.namespace_root_hash {
      return Err(RootExpiryRetentionErrorV1::RecordOrder);
    }
    self.observe_prior_aggregate(record)?;
    self.previous_root_hash.clear();
    self.previous_root_hash.extend_from_slice(record.namespace_root_hash);

    if record.namespace_root_hash == self.target_root_hash {
      return self.observe_target(record);
    }
    match record.state {
      RootExpiryStateV1::LogicallyRetired => {
        self.add_resulting_mandatory(record.retired_at_ms)?;
        self.digest_retained_record(record)?;
        Ok(RootExpiryRetentionActionV1::RetainedMandatory)
      }
      RootExpiryStateV1::PhysicallyReclaimed => self.observe_optional(record),
    }
  }

  fn observe_target(&mut self, record: &RootExpiryRecordV1<'_>) -> Result<RootExpiryRetentionActionV1, RootExpiryRetentionErrorV1> {
    if self.target_seen
      || record.state != RootExpiryStateV1::LogicallyRetired
      || record.retired_at_ms != self.target_retired_at_ms
      || record.last_pending_since_ms != self.target_last_pending_since_ms
      || record.final_mark_generation != self.target_final_mark_generation
      || record.reason != self.target_reason
      || record.retirement_commit_hash != self.target_retirement_commit_hash
      || record.root_object_reclaim_proof_hash.is_some()
      || record.evidence_expires_at_ms.is_some()
      || !self.selection_retains(self.target_evidence_expires_at_ms, record.namespace_root_hash)
    {
      return Err(RootExpiryRetentionErrorV1::Target);
    }
    self.target_seen = true;
    self.reclaimed_count = checked_increment(self.reclaimed_count)?;
    self.add_resulting_optional(record.retired_at_ms, self.target_evidence_expires_at_ms, record.namespace_root_hash)?;
    self.resulting_expiry_records_digest =
      root_expiry_result_digest_step(self.context.hash_algorithm, &self.resulting_expiry_records_digest, &self.target_expiry_record);
    Ok(RootExpiryRetentionActionV1::ReclaimedAndRetained)
  }

  fn observe_optional(&mut self, record: &RootExpiryRecordV1<'_>) -> Result<RootExpiryRetentionActionV1, RootExpiryRetentionErrorV1> {
    let expires_at_ms = record.evidence_expires_at_ms.ok_or(RootExpiryRetentionErrorV1::ManifestAggregate)?;
    if expires_at_ms <= self.context.completed_at_ms {
      self.expired_count = checked_increment(self.expired_count)?;
      return Ok(RootExpiryRetentionActionV1::DroppedExpired);
    }
    if !self.selection_retains(expires_at_ms, record.namespace_root_hash) {
      self.budget_evicted_count = checked_increment(self.budget_evicted_count)?;
      return Ok(RootExpiryRetentionActionV1::DroppedForBudget);
    }
    self.add_resulting_optional(record.retired_at_ms, expires_at_ms, record.namespace_root_hash)?;
    self.digest_retained_record(record)?;
    Ok(RootExpiryRetentionActionV1::RetainedOptional)
  }

  fn digest_retained_record(&mut self, record: &RootExpiryRecordV1<'_>) -> Result<(), RootExpiryRetentionErrorV1> {
    let encoded = encode_root_expiry_record_v1(&RootExpiryRecordWriteV1 {
      hash_algorithm: self.context.hash_algorithm,
      namespace_root_hash: record.namespace_root_hash,
      retired_at_ms: record.retired_at_ms,
      last_pending_since_ms: record.last_pending_since_ms,
      final_mark_generation: record.final_mark_generation,
      reason: record.reason,
      state: record.state,
      retirement_commit_hash: record.retirement_commit_hash,
      root_object_reclaim_proof_hash: record.root_object_reclaim_proof_hash,
      evidence_expires_at_ms: record.evidence_expires_at_ms,
    })?;
    self.resulting_expiry_records_digest =
      root_expiry_result_digest_step(self.context.hash_algorithm, &self.resulting_expiry_records_digest, &encoded);
    Ok(())
  }

  fn selection_retains(&self, evidence_expires_at_ms: i64, namespace_root_hash: &[u8]) -> bool {
    match &self.context.selection {
      RootExpiryRetentionSelectionV1::KeepAll => true,
      RootExpiryRetentionSelectionV1::AtOrAfter(cutoff) => {
        (evidence_expires_at_ms, namespace_root_hash) >= (cutoff.evidence_expires_at_ms, cutoff.namespace_root_hash.as_slice())
      }
    }
  }

  fn observe_prior_aggregate(&mut self, record: &RootExpiryRecordV1<'_>) -> Result<(), RootExpiryRetentionErrorV1> {
    self.prior_count = checked_increment(self.prior_count)?;
    self.prior_bytes = self.prior_bytes.checked_add(self.row_bytes).ok_or(RootExpiryRetentionErrorV1::Arithmetic)?;
    match record.state {
      RootExpiryStateV1::LogicallyRetired => {
        self.prior_mandatory_count = checked_increment(self.prior_mandatory_count)?;
        self.prior_mandatory_bytes =
          self.prior_mandatory_bytes.checked_add(self.row_bytes).ok_or(RootExpiryRetentionErrorV1::Arithmetic)?;
      }
      RootExpiryStateV1::PhysicallyReclaimed => {
        self.prior_optional_count = checked_increment(self.prior_optional_count)?;
        self.prior_optional_bytes = self.prior_optional_bytes.checked_add(self.row_bytes).ok_or(RootExpiryRetentionErrorV1::Arithmetic)?;
      }
    }
    update_time_bounds(&mut self.prior_oldest_retired_at_ms, &mut self.prior_newest_retired_at_ms, record.retired_at_ms);
    Ok(())
  }

  fn add_resulting_mandatory(&mut self, retired_at_ms: i64) -> Result<(), RootExpiryRetentionErrorV1> {
    self.resulting_mandatory_count = checked_increment(self.resulting_mandatory_count)?;
    self.resulting_mandatory_bytes =
      self.resulting_mandatory_bytes.checked_add(self.row_bytes).ok_or(RootExpiryRetentionErrorV1::Arithmetic)?;
    update_time_bounds(&mut self.resulting_oldest_retired_at_ms, &mut self.resulting_newest_retired_at_ms, retired_at_ms);
    Ok(())
  }

  fn add_resulting_optional(
    &mut self,
    retired_at_ms: i64,
    evidence_expires_at_ms: i64,
    namespace_root_hash: &[u8],
  ) -> Result<(), RootExpiryRetentionErrorV1> {
    self.resulting_optional_count = checked_increment(self.resulting_optional_count)?;
    self.resulting_optional_bytes =
      self.resulting_optional_bytes.checked_add(self.row_bytes).ok_or(RootExpiryRetentionErrorV1::Arithmetic)?;
    let replaces_oldest = match self.oldest_retained_optional_expires_at_ms {
      None => true,
      Some(oldest_expires_at_ms) => {
        (evidence_expires_at_ms, namespace_root_hash) < (oldest_expires_at_ms, self.oldest_retained_optional_root_hash.as_slice())
      }
    };
    if replaces_oldest {
      self.oldest_retained_optional_expires_at_ms = Some(evidence_expires_at_ms);
      self.oldest_retained_optional_root_hash.clear();
      self.oldest_retained_optional_root_hash.extend_from_slice(namespace_root_hash);
    }
    update_time_bounds(&mut self.resulting_oldest_retired_at_ms, &mut self.resulting_newest_retired_at_ms, retired_at_ms);
    Ok(())
  }
}

#[derive(Debug, Error)]
pub enum RootObjectReclaimQualificationErrorV1 {
  #[error("root-object reclaim qualification was canceled")]
  Canceled,
  #[error("root-object reclaim requires exact mandatory logical-retirement evidence")]
  InvalidLifecycleState,
  #[error("root-object reclaim evidence belongs to another database")]
  DatabaseMismatch,
  #[error("root-object reclaim evidence has an invalid identity or digest width")]
  InvalidEvidence,
  #[error("the final physical inventory is not newer than every sweep receipt")]
  InventoryOrder,
  #[error("root-object reclaim evidence has an invalid or overflowing time")]
  Time,
  #[error("root-object reclaim integer conversion failed: {0}")]
  IntegerConversion(#[from] std::num::TryFromIntError),
  #[error(transparent)]
  Verification(#[from] RootObjectReclaimEvidenceVerificationErrorV1),
  #[error(transparent)]
  Format(Box<FormatError>),
}

impl RootObjectReclaimQualificationErrorV1 {
  pub fn code(&self) -> &str {
    match self {
      Self::Canceled => "root_reclaim_canceled",
      Self::InvalidLifecycleState => "root_reclaim_lifecycle_state",
      Self::DatabaseMismatch => "root_reclaim_database",
      Self::InvalidEvidence => "root_reclaim_evidence",
      Self::InventoryOrder => "root_reclaim_inventory_order",
      Self::Time => "root_reclaim_time",
      Self::IntegerConversion(_) => "root_reclaim_time",
      Self::Verification(source) => source.code(),
      Self::Format(source) => source.code(),
    }
  }
}

impl From<FormatError> for RootObjectReclaimQualificationErrorV1 {
  fn from(error: FormatError) -> Self {
    Self::Format(Box::new(error))
  }
}

pub fn qualify_root_object_reclaim_v1(
  request: RootObjectReclaimQualificationRequestV1<'_>,
  cancellation: &CancellationToken,
  verifier: &mut dyn RootObjectReclaimEvidenceVerifierV1,
) -> Result<QualifiedRootObjectReclaimV1, RootObjectReclaimQualificationErrorV1> {
  if cancellation.is_cancelled() {
    return Err(RootObjectReclaimQualificationErrorV1::Canceled);
  }
  if request.prior_expiry.state != RootExpiryStateV1::LogicallyRetired
    || request.prior_expiry.root_object_reclaim_proof_hash.is_some()
    || request.prior_expiry.evidence_expires_at_ms.is_some()
  {
    return Err(RootObjectReclaimQualificationErrorV1::InvalidLifecycleState);
  }
  validate_root_expiry_retirement_commit(request.prior_expiry, request.retirement)?;
  if request.prior_expiry.retired_at_ms != request.retirement.committed_at_ms
    || request.prior_expiry.last_pending_since_ms != request.retirement.pending_since_ms
  {
    return Err(RootObjectReclaimQualificationErrorV1::InvalidLifecycleState);
  }

  let hash_width = request.hash_algorithm.hash_length();
  if request.retirement.database_id != request.final_physical_inventory.database_id
    || request.retirement.database_id.len() != 16
    || request.retirement.database_id.iter().all(|byte| *byte == 0)
  {
    return Err(RootObjectReclaimQualificationErrorV1::DatabaseMismatch);
  }
  if request.proof_id.len() != 16
    || request.proof_id.iter().all(|byte| *byte == 0)
    || !valid_hash(request.final_physical_inventory.key.as_slice(), hash_width)
    || !valid_hash(request.root_object_incarnation_digest, hash_width)
    || !valid_hash(request.sweep_receipt_merkle_root, hash_width)
    || !valid_hash(request.absence_digest, hash_width)
    || request.root_object_incarnation_count == 0
    || request.sweep_receipt_count == 0
  {
    return Err(RootObjectReclaimQualificationErrorV1::InvalidEvidence);
  }
  if request.latest_sweep_receipt_generation == 0 || request.final_physical_inventory.generation <= request.latest_sweep_receipt_generation
  {
    return Err(RootObjectReclaimQualificationErrorV1::InventoryOrder);
  }

  let inventory_completed_at_ms = i64::try_from(request.final_physical_inventory.completed_at_ms)?;
  let retention_ms = i64::try_from(request.retention_ms)?;
  let evidence_expires_at_ms = request.reclaimed_at_ms.checked_add(retention_ms).ok_or(RootObjectReclaimQualificationErrorV1::Time)?;
  if request.retention_ms == 0
    || inventory_completed_at_ms <= 0
    || request.reclaimed_at_ms < inventory_completed_at_ms
    || request.reclaimed_at_ms < request.prior_expiry.retired_at_ms
    || evidence_expires_at_ms < request.reclaimed_at_ms
  {
    return Err(RootObjectReclaimQualificationErrorV1::Time);
  }

  verifier.verify_root_object_reclaim(RootObjectReclaimEvidenceVerificationRequestV1 {
    hash_algorithm: request.hash_algorithm,
    database_id: request.retirement.database_id,
    namespace_root_hash: request.prior_expiry.namespace_root_hash,
    retirement_commit_hash: request.prior_expiry.retirement_commit_hash,
    final_physical_inventory: request.final_physical_inventory,
    final_physical_inventory_generation: request.final_physical_inventory.generation,
    latest_sweep_receipt_generation: request.latest_sweep_receipt_generation,
    root_object_incarnation_digest: request.root_object_incarnation_digest,
    root_object_incarnation_count: request.root_object_incarnation_count,
    sweep_receipt_merkle_root: request.sweep_receipt_merkle_root,
    sweep_receipt_count: request.sweep_receipt_count,
    absence_digest: request.absence_digest,
  })?;
  if cancellation.is_cancelled() {
    return Err(RootObjectReclaimQualificationErrorV1::Canceled);
  }

  let encoded_proof = encode_root_object_reclaim_proof_v1(&RootObjectReclaimProofWriteV1 {
    hash_algorithm: request.hash_algorithm,
    database_id: request.retirement.database_id,
    namespace_root_hash: request.prior_expiry.namespace_root_hash,
    proof_id: request.proof_id,
    generation: request.final_physical_inventory.generation,
    retirement_commit_hash: request.prior_expiry.retirement_commit_hash,
    reclaimed_at_ms: request.reclaimed_at_ms,
    physical_inventory_manifest_hash: &request.final_physical_inventory.key,
    root_object_incarnation_digest: request.root_object_incarnation_digest,
    root_object_incarnation_count: request.root_object_incarnation_count,
    sweep_receipt_merkle_root: request.sweep_receipt_merkle_root,
    sweep_receipt_count: request.sweep_receipt_count,
    absence_digest: request.absence_digest,
  })?;
  let encoded_expiry_record = encode_root_expiry_record_v1(&RootExpiryRecordWriteV1 {
    hash_algorithm: request.hash_algorithm,
    namespace_root_hash: request.prior_expiry.namespace_root_hash,
    retired_at_ms: request.prior_expiry.retired_at_ms,
    last_pending_since_ms: request.prior_expiry.last_pending_since_ms,
    final_mark_generation: request.prior_expiry.final_mark_generation,
    reason: request.prior_expiry.reason,
    state: RootExpiryStateV1::PhysicallyReclaimed,
    retirement_commit_hash: request.prior_expiry.retirement_commit_hash,
    root_object_reclaim_proof_hash: Some(&encoded_proof.key),
    evidence_expires_at_ms: Some(evidence_expires_at_ms),
  })?;
  let proof = super::gc_lifecycle::decode_root_object_reclaim_proof_v1(&encoded_proof.value, request.hash_algorithm)?;
  let replacement = decode_root_expiry_record_v1(&encoded_expiry_record, request.hash_algorithm)?;
  validate_root_expiry_reclaim_proof(&replacement, &proof)?;

  Ok(QualifiedRootObjectReclaimV1 { encoded_proof, encoded_expiry_record, evidence_expires_at_ms })
}

fn valid_hash(value: &[u8], hash_width: usize) -> bool {
  value.len() == hash_width && value.iter().any(|byte| *byte != 0)
}

fn validate_prior_expiry_aggregates(
  expiry: &RootExpiryManifestV1<'_>,
  row_bytes: u64,
  maximum_records: u64,
) -> Result<(), RootExpiryRetentionErrorV1> {
  if expiry.record_count == 0 || expiry.record_count > maximum_records || expiry.directory_root_hash.is_none() {
    return Err(RootExpiryRetentionErrorV1::ManifestAggregate);
  }
  let mandatory_bytes = expiry.mandatory_count.checked_mul(row_bytes).ok_or(RootExpiryRetentionErrorV1::Arithmetic)?;
  let optional_bytes = expiry.optional_count.checked_mul(row_bytes).ok_or(RootExpiryRetentionErrorV1::Arithmetic)?;
  let record_count = expiry.mandatory_count.checked_add(expiry.optional_count).ok_or(RootExpiryRetentionErrorV1::Arithmetic)?;
  let logical_bytes = mandatory_bytes.checked_add(optional_bytes).ok_or(RootExpiryRetentionErrorV1::Arithmetic)?;
  if mandatory_bytes != expiry.mandatory_bytes
    || optional_bytes != expiry.optional_bytes
    || record_count != expiry.record_count
    || logical_bytes != expiry.logical_bytes
    || expiry.oldest_retired_at_ms.is_none()
    || expiry.newest_retired_at_ms.is_none()
    || expiry.oldest_retired_at_ms > expiry.newest_retired_at_ms
  {
    return Err(RootExpiryRetentionErrorV1::ManifestAggregate);
  }
  Ok(())
}

fn checked_increment(value: u64) -> Result<u64, RootExpiryRetentionErrorV1> {
  value.checked_add(1).ok_or(RootExpiryRetentionErrorV1::Arithmetic)
}

fn update_time_bounds(oldest: &mut Option<i64>, newest: &mut Option<i64>, value: i64) {
  *oldest = Some(oldest.map_or(value, |current| current.min(value)));
  *newest = Some(newest.map_or(value, |current| current.max(value)));
}
