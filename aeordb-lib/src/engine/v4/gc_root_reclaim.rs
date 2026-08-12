use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::gc::EncodedImmutableGcArtifactV1;
use super::gc_lifecycle::{
  RootExpiryRecordWriteV1, RootObjectReclaimProofWriteV1, RootRetirementCommitV1, encode_root_expiry_record_v1,
  encode_root_object_reclaim_proof_v1, validate_root_expiry_reclaim_proof, validate_root_expiry_retirement_commit,
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

  let inventory_completed_at_ms =
    i64::try_from(request.final_physical_inventory.completed_at_ms).map_err(|_| RootObjectReclaimQualificationErrorV1::Time)?;
  let retention_ms = i64::try_from(request.retention_ms).map_err(|_| RootObjectReclaimQualificationErrorV1::Time)?;
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
