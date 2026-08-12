//! Bounded consumption and settlement of one durable Void claim.
//!
//! This module owns only a private claim-admission permit. It does not load
//! startup state or connect the v4 allocator to the live v3 storage engine.

use std::num::TryFromIntError;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::first_authority::{FirstAuthorityPublicationErrorV1, RootRetirementLineageStateV1, VoidClaimAdmissionPermitV1};
use super::header_publication::DatabaseHeaderObservationV4;
use super::gc_retirement::{RetirementJournalOwnerErrorV1, RetirementJournalReplacementAdmissionErrorV1};
use super::gc::{PhysicalIncarnationV1, encode_physical_incarnation_into};
use super::gc_void::{SweepVoidArtifactV1, VoidCatalogManifestV1, VoidClaimV1, VoidExtentRecordV1, decode_sweep_void_artifact};
use super::gc_void_claim::VoidClaimAdmittedExtentV1;
use super::gc_void_publication::{
  VoidCatalogClosureErrorV1, VoidCatalogClosureLimitsV1, VoidCatalogClosureSummaryV1, VoidCatalogClosureValidatorV1,
};
use super::hash::digest_parts;
use super::reader::FormatError;
use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

const MAXIMUM_VOID_CLAIM_ALLOCATIONS_V1: u32 = 4_096;
const VOID_CLAIM_ALLOCATION_EVIDENCE_DOMAIN_V1: &[u8] = b"aeordb.void-claim-allocation-evidence.v1\0";
const VOID_CLAIM_ALLOCATION_FINAL_DOMAIN_V1: &[u8] = b"aeordb.void-claim-allocation-final.v1\0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoidClaimAllocationLimitsV1 {
  pub maximum_allocations: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct VoidClaimSubrangeV1<'a> {
  pub ordinal: u32,
  pub offset: u64,
  pub length: u32,
  pub reclaim_commit_sequence: u64,
  pub void_generation: u64,
  pub origin_sweep_proposal_hash: &'a [u8],
  pub origin_quarantine_manifest_hash: &'a [u8],
  pub reclaimed_incarnation_digest: &'a [u8],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoidClaimDurableUseV1 {
  pub logical_key: Vec<u8>,
  pub integrity_digest: Vec<u8>,
  pub wal_offset: u64,
  pub write_sequence: u64,
  pub entity_length: u32,
  pub entry_type: u8,
  pub entity_version: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoidClaimWriteFailureV1 {
  DefinitelyUnwritten { reason_code: u16 },
  PossiblyWritten { reason_code: u16, evidence_digest: Vec<u8> },
}

pub trait VoidClaimAllocationSinkV1 {
  /// Consume exactly the supplied claim subrange. An error must classify
  /// whether bytes are definitely untouched or may have been modified.
  fn consume_void_claim_subrange(&mut self, request: VoidClaimSubrangeV1<'_>) -> Result<VoidClaimDurableUseV1, VoidClaimWriteFailureV1>;
}

#[must_use = "every allocation disposition must be retained through claim settlement"]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoidClaimAllocationDispositionV1 {
  Durable { ordinal: u32, wal_offset: u64, entity_length: u32, write_sequence: u64 },
  DefinitelyUnused { ordinal: u32, offset: u64, length: u32, reason_code: u16 },
  Uncertain { ordinal: u32, offset: u64, length: u32, reason_code: u16 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoidClaimReturnedExtentV1 {
  pub offset: u64,
  pub length: u32,
  pub reclaim_commit_sequence: u64,
  pub void_generation: u64,
  pub origin_sweep_proposal_hash: Vec<u8>,
  pub origin_quarantine_manifest_hash: Vec<u8>,
  pub reclaimed_incarnation_digest: Vec<u8>,
}

impl VoidClaimReturnedExtentV1 {
  pub fn as_record(&self) -> VoidExtentRecordV1<'_> {
    VoidExtentRecordV1 {
      offset: self.offset,
      length: self.length,
      reclaim_commit_sequence: self.reclaim_commit_sequence,
      void_generation: self.void_generation,
      origin_sweep_proposal_hash: &self.origin_sweep_proposal_hash,
      origin_quarantine_manifest_hash: &self.origin_quarantine_manifest_hash,
      reclaimed_incarnation_digest: &self.reclaimed_incarnation_digest,
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoidClaimUncertainExtentV1 {
  pub offset: u64,
  pub length: u32,
  pub reason_code: u16,
  pub evidence_digest: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoidClaimConsumptionOutcomeV1 {
  Settled,
  AbandonedToQuarantine,
}

#[must_use = "a consumed claim remains unavailable until this exact permit selects a claim-free catalog and publishes its receipt"]
pub struct VoidClaimConsumptionPermitV1 {
  hash_algorithm: HashAlgorithm,
  database_id: [u8; 16],
  claim_id: [u8; 16],
  claim_key: Vec<u8>,
  claim_write_sequence: u64,
  preclaim_manifest_key: Vec<u8>,
  source_manifest_key: Vec<u8>,
  source_manifest_write_sequence: u64,
  source_control_key: Vec<u8>,
  source_control_sequence: u64,
  source_control_write_sequence: u64,
  source_control_slot: u8,
  generation: u64,
  claimed_bytes: u64,
  outcome: VoidClaimConsumptionOutcomeV1,
  durable_uses: Box<[VoidClaimDurableUseV1]>,
  returned_extents: Box<[VoidClaimReturnedExtentV1]>,
  uncertain_extents: Box<[VoidClaimUncertainExtentV1]>,
  used_bytes: u64,
  returned_bytes: u64,
  uncertain_bytes: u64,
  evidence_digest: Vec<u8>,
  _claim_memory: VoidClaimAdmissionPermitV1,
  _allocation_memory: MemoryReservation,
}

impl std::fmt::Debug for VoidClaimConsumptionPermitV1 {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("VoidClaimConsumptionPermitV1")
      .field("claim_key", &hex::encode(&self.claim_key))
      .field("source_manifest_key", &hex::encode(&self.source_manifest_key))
      .field("outcome", &self.outcome)
      .field("used_bytes", &self.used_bytes)
      .field("returned_bytes", &self.returned_bytes)
      .field("uncertain_bytes", &self.uncertain_bytes)
      .finish_non_exhaustive()
  }
}

impl VoidClaimConsumptionPermitV1 {
  pub const fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }

  pub const fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  pub const fn claim_id(&self) -> [u8; 16] {
    self.claim_id
  }

  pub fn claim_key(&self) -> &[u8] {
    &self.claim_key
  }

  pub const fn claim_write_sequence(&self) -> u64 {
    self.claim_write_sequence
  }

  pub fn preclaim_manifest_key(&self) -> &[u8] {
    &self.preclaim_manifest_key
  }

  pub fn source_manifest_key(&self) -> &[u8] {
    &self.source_manifest_key
  }

  pub const fn source_manifest_write_sequence(&self) -> u64 {
    self.source_manifest_write_sequence
  }

  pub fn source_control_key(&self) -> &[u8] {
    &self.source_control_key
  }

  pub const fn source_control_write_sequence(&self) -> u64 {
    self.source_control_write_sequence
  }

  pub const fn source_control_sequence(&self) -> u64 {
    self.source_control_sequence
  }

  pub const fn source_control_slot(&self) -> u8 {
    self.source_control_slot
  }

  pub const fn generation(&self) -> u64 {
    self.generation
  }

  pub const fn claimed_bytes(&self) -> u64 {
    self.claimed_bytes
  }

  pub const fn outcome(&self) -> VoidClaimConsumptionOutcomeV1 {
    self.outcome
  }

  pub fn durable_uses(&self) -> &[VoidClaimDurableUseV1] {
    &self.durable_uses
  }

  pub fn returned_extents(&self) -> &[VoidClaimReturnedExtentV1] {
    &self.returned_extents
  }

  pub fn uncertain_extents(&self) -> &[VoidClaimUncertainExtentV1] {
    &self.uncertain_extents
  }

  pub const fn used_bytes(&self) -> u64 {
    self.used_bytes
  }

  pub const fn returned_bytes(&self) -> u64 {
    self.returned_bytes
  }

  pub const fn uncertain_bytes(&self) -> u64 {
    self.uncertain_bytes
  }

  pub fn evidence_digest(&self) -> &[u8] {
    &self.evidence_digest
  }
}

#[derive(Debug, Error)]
pub enum VoidClaimAllocationErrorV1 {
  #[error("{code}: {message}")]
  Invalid { code: &'static str, message: String },
  #[error("Void claim allocation was canceled")]
  Canceled,
  #[error("Void claim allocation owner is already failed")]
  Failed,
  #[error("Void claim allocation memory admission failed: {0}")]
  Memory(#[from] MemoryCoordinatorError),
  #[error("Void claim allocation failed: {0}")]
  Allocation(#[from] std::collections::TryReserveError),
  #[error("Void claim allocation integer conversion failed: {0}")]
  IntegerConversion(#[from] TryFromIntError),
  #[error("Void claim allocation format validation failed: {0}")]
  Format(#[from] FormatError),
}

impl VoidClaimAllocationErrorV1 {
  pub fn code(&self) -> &str {
    match self {
      Self::Invalid { code, .. } => code,
      Self::Canceled => "void_claim_allocation_canceled",
      Self::Failed => "void_claim_allocation_failed",
      Self::Memory(_) => "void_claim_allocation_memory",
      Self::Allocation(_) => "void_claim_allocation_allocation",
      Self::IntegerConversion(_) => "void_claim_allocation_integer_conversion",
      Self::Format(source) => source.code(),
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }
}

struct AllocationEvidenceChainV1 {
  algorithm: HashAlgorithm,
  value: Vec<u8>,
  count: u64,
}

impl AllocationEvidenceChainV1 {
  fn new(algorithm: HashAlgorithm, claim_key: &[u8]) -> Self {
    Self { algorithm, value: digest_parts(algorithm, &[VOID_CLAIM_ALLOCATION_EVIDENCE_DOMAIN_V1, claim_key]), count: 0 }
  }

  fn push(&mut self, parts: &[&[u8]]) -> Result<(), VoidClaimAllocationErrorV1> {
    if parts.len() > 10 {
      return Err(VoidClaimAllocationErrorV1::invalid(
        "void_claim_allocation_evidence_parts",
        "allocation evidence row exceeds its fixed part bound",
      ));
    }
    self.count = self
      .count
      .checked_add(1)
      .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_evidence_count", "evidence count overflowed"))?;
    let count = self.count.to_le_bytes();
    let mut inputs = [&[][..]; 13];
    inputs[0] = VOID_CLAIM_ALLOCATION_EVIDENCE_DOMAIN_V1;
    inputs[1] = &self.value;
    inputs[2] = &count;
    inputs[3..3 + parts.len()].copy_from_slice(parts);
    self.value = digest_parts(self.algorithm, &inputs[..3 + parts.len()]);
    Ok(())
  }
}

pub struct VoidClaimAllocationOwnerV1 {
  permit: Option<VoidClaimAdmissionPermitV1>,
  cancellation: CancellationToken,
  maximum_allocations: u32,
  allocation_count: u32,
  current_extent_index: usize,
  current_offset: u64,
  durable_uses: Vec<VoidClaimDurableUseV1>,
  returned_extents: Vec<VoidClaimReturnedExtentV1>,
  uncertain_extents: Vec<VoidClaimUncertainExtentV1>,
  used_bytes: u64,
  returned_bytes: u64,
  uncertain_bytes: u64,
  evidence: AllocationEvidenceChainV1,
  memory: MemoryReservation,
  failed: bool,
}

impl std::fmt::Debug for VoidClaimAllocationOwnerV1 {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("VoidClaimAllocationOwnerV1")
      .field("allocation_count", &self.allocation_count)
      .field("maximum_allocations", &self.maximum_allocations)
      .field("used_bytes", &self.used_bytes)
      .field("returned_bytes", &self.returned_bytes)
      .field("uncertain_bytes", &self.uncertain_bytes)
      .field("failed", &self.failed)
      .finish_non_exhaustive()
  }
}

impl VoidClaimAllocationOwnerV1 {
  pub fn new(
    permit: VoidClaimAdmissionPermitV1,
    limits: VoidClaimAllocationLimitsV1,
    memory: &MemoryCoordinator,
    cancellation: CancellationToken,
  ) -> Result<Self, VoidClaimAllocationErrorV1> {
    if cancellation.is_cancelled() {
      return Err(VoidClaimAllocationErrorV1::Canceled);
    }
    if limits.maximum_allocations == 0 || limits.maximum_allocations > MAXIMUM_VOID_CLAIM_ALLOCATIONS_V1 {
      return Err(VoidClaimAllocationErrorV1::invalid("void_claim_allocation_limits", "maximum allocations must be between one and 4,096"));
    }
    if permit.claimed_extents().is_empty()
      || permit.claimed_extents().len() > MAXIMUM_VOID_CLAIM_ALLOCATIONS_V1 as usize
      || permit.claimed_extents().iter().any(|extent| !valid_admitted_extent(extent, permit.hash_algorithm()))
    {
      return Err(VoidClaimAllocationErrorV1::invalid(
        "void_claim_allocation_permit",
        "claim permit contains incomplete extent provenance",
      ));
    }
    let hash_width = permit.hash_algorithm().hash_length();
    let allocation_count = usize::try_from(limits.maximum_allocations)?;
    let claimed_extent_count = permit.claimed_extents().len();
    let returned_capacity = allocation_count
      .checked_add(claimed_extent_count)
      .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_memory", "returned extent capacity overflowed"))?;
    let uncertain_capacity = allocation_count.max(claimed_extent_count);
    let retained_struct_bytes = allocation_count
      .checked_mul(std::mem::size_of::<VoidClaimDurableUseV1>())
      .and_then(|bytes| bytes.checked_add(returned_capacity.checked_mul(std::mem::size_of::<VoidClaimReturnedExtentV1>())?))
      .and_then(|bytes| bytes.checked_add(uncertain_capacity.checked_mul(std::mem::size_of::<VoidClaimUncertainExtentV1>())?))
      .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_memory", "allocation structure estimate overflowed"))?;
    let retained_hash_count = allocation_count
      .checked_mul(2)
      .and_then(|count| count.checked_add(returned_capacity.checked_mul(3)?))
      .and_then(|count| count.checked_add(uncertain_capacity))
      .and_then(|count| count.checked_add(16))
      .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_memory", "allocation hash estimate overflowed"))?;
    let accounted_bytes = retained_struct_bytes
      .checked_add(
        retained_hash_count
          .checked_mul(hash_width)
          .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_memory", "allocation hash bytes overflowed"))?,
      )
      .and_then(|bytes| bytes.checked_add(1_024))
      .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_memory", "allocation memory estimate overflowed"))?;
    let reservation = memory.reserve(MemoryOwner::GarbageCollection, u64::try_from(accounted_bytes)?, AdmissionClass::Maintenance)?;
    let mut durable_uses = Vec::new();
    let mut returned_extents = Vec::new();
    let mut uncertain_extents = Vec::new();
    durable_uses.try_reserve_exact(allocation_count)?;
    returned_extents.try_reserve_exact(returned_capacity)?;
    uncertain_extents.try_reserve_exact(uncertain_capacity)?;
    let current_offset = permit.claimed_extents()[0].offset;
    let evidence = AllocationEvidenceChainV1::new(permit.hash_algorithm(), permit.claim_key());
    Ok(Self {
      permit: Some(permit),
      cancellation,
      maximum_allocations: limits.maximum_allocations,
      allocation_count: 0,
      current_extent_index: 0,
      current_offset,
      durable_uses,
      returned_extents,
      uncertain_extents,
      used_bytes: 0,
      returned_bytes: 0,
      uncertain_bytes: 0,
      evidence,
      memory: reservation,
      failed: false,
    })
  }

  pub fn consume(
    &mut self,
    length: u32,
    sink: &mut dyn VoidClaimAllocationSinkV1,
  ) -> Result<VoidClaimAllocationDispositionV1, VoidClaimAllocationErrorV1> {
    self.preflight()?;
    if length == 0 {
      return Err(VoidClaimAllocationErrorV1::invalid("void_claim_allocation_length", "allocation length must be nonzero"));
    }
    if self.allocation_count >= self.maximum_allocations {
      return Err(VoidClaimAllocationErrorV1::invalid("void_claim_allocation_limit", "claim allocation count reached its admitted bound"));
    }
    let subrange = self.take_next_subrange(length)?;
    let ordinal = self.allocation_count;
    self.allocation_count += 1;
    let request = VoidClaimSubrangeV1 {
      ordinal,
      offset: subrange.offset,
      length: subrange.length,
      reclaim_commit_sequence: subrange.reclaim_commit_sequence,
      void_generation: subrange.void_generation,
      origin_sweep_proposal_hash: &subrange.origin_sweep_proposal_hash,
      origin_quarantine_manifest_hash: &subrange.origin_quarantine_manifest_hash,
      reclaimed_incarnation_digest: &subrange.reclaimed_incarnation_digest,
    };
    let disposition = match sink.consume_void_claim_subrange(request) {
      Ok(receipt) => self.record_durable_use(ordinal, &subrange, receipt),
      Err(VoidClaimWriteFailureV1::DefinitelyUnwritten { reason_code }) if reason_code != 0 => {
        self.record_returned(&subrange)?;
        self.push_evidence(ordinal, 2, &subrange, reason_code, &[])?;
        Ok(VoidClaimAllocationDispositionV1::DefinitelyUnused { ordinal, offset: subrange.offset, length: subrange.length, reason_code })
      }
      Err(VoidClaimWriteFailureV1::PossiblyWritten { reason_code, evidence_digest })
        if reason_code != 0
          && evidence_digest.len() == self.hash_algorithm()?.hash_length()
          && evidence_digest.iter().any(|byte| *byte != 0) =>
      {
        self.record_uncertain(&subrange, reason_code, evidence_digest.clone())?;
        self.push_evidence(ordinal, 3, &subrange, reason_code, &evidence_digest)?;
        Ok(VoidClaimAllocationDispositionV1::Uncertain { ordinal, offset: subrange.offset, length: subrange.length, reason_code })
      }
      Err(failure) => {
        let evidence_digest = digest_parts(
          self.hash_algorithm()?,
          &[b"aeordb.void-claim-invalid-sink-failure.v1\0", &ordinal.to_le_bytes(), &subrange.offset.to_le_bytes()],
        );
        self.record_uncertain(&subrange, u16::MAX, evidence_digest.clone())?;
        self.push_evidence(ordinal, 3, &subrange, u16::MAX, &evidence_digest)?;
        self.failed = true;
        Err(VoidClaimAllocationErrorV1::invalid(
          "void_claim_allocation_sink_failure",
          format!("sink returned an invalid write-failure classification: {failure:?}"),
        ))
      }
    };
    match disposition {
      Ok(disposition) => Ok(disposition),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }

  pub fn finish(mut self) -> Result<VoidClaimConsumptionPermitV1, VoidClaimAllocationErrorV1> {
    self.preflight()?;
    self.return_all_unallocated()?;
    let permit = self
      .permit
      .take()
      .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_permit", "claim permit was already consumed"))?;
    let outcome = if self.durable_uses.is_empty() {
      self.returned_extents.clear();
      self.returned_bytes = 0;
      self.uncertain_extents.clear();
      self.uncertain_bytes = 0;
      for extent in permit.claimed_extents() {
        self.record_uncertain_from_admitted(
          extent,
          u16::MAX,
          digest_parts(permit.hash_algorithm(), &[b"aeordb.void-claim-abandoned.v1\0"]),
        )?;
      }
      VoidClaimConsumptionOutcomeV1::AbandonedToQuarantine
    } else {
      merge_returned_extents(&mut self.returned_extents)?;
      self.returned_bytes = sum_returned_bytes(&self.returned_extents)?;
      VoidClaimConsumptionOutcomeV1::Settled
    };
    let total = self
      .used_bytes
      .checked_add(self.returned_bytes)
      .and_then(|bytes| bytes.checked_add(self.uncertain_bytes))
      .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_totals", "claim partition total overflowed"))?;
    if total != permit.claimed_bytes() {
      return Err(VoidClaimAllocationErrorV1::invalid(
        "void_claim_allocation_totals",
        "used, returned, and uncertain bytes do not exactly partition the durable claim",
      ));
    }
    let evidence_digest = digest_parts(
      permit.hash_algorithm(),
      &[
        VOID_CLAIM_ALLOCATION_FINAL_DOMAIN_V1,
        &self.evidence.value,
        &self.evidence.count.to_le_bytes(),
        &self.used_bytes.to_le_bytes(),
        &self.returned_bytes.to_le_bytes(),
        &self.uncertain_bytes.to_le_bytes(),
      ],
    );
    Ok(VoidClaimConsumptionPermitV1 {
      hash_algorithm: permit.hash_algorithm(),
      database_id: permit.database_id(),
      claim_id: permit.claim_id(),
      claim_key: permit.claim_key().to_vec(),
      claim_write_sequence: permit.claim_write_sequence(),
      preclaim_manifest_key: permit.source_manifest_key().to_vec(),
      source_manifest_key: permit.result_manifest_key().to_vec(),
      source_manifest_write_sequence: permit.result_manifest_write_sequence(),
      source_control_key: permit.result_control_key().to_vec(),
      source_control_sequence: permit.result_control_sequence(),
      source_control_write_sequence: permit.result_control_write_sequence(),
      source_control_slot: permit.result_control_slot(),
      generation: permit.generation(),
      claimed_bytes: permit.claimed_bytes(),
      outcome,
      durable_uses: self.durable_uses.into_boxed_slice(),
      returned_extents: self.returned_extents.into_boxed_slice(),
      uncertain_extents: self.uncertain_extents.into_boxed_slice(),
      used_bytes: self.used_bytes,
      returned_bytes: self.returned_bytes,
      uncertain_bytes: self.uncertain_bytes,
      evidence_digest,
      _claim_memory: permit,
      _allocation_memory: self.memory,
    })
  }

  fn preflight(&self) -> Result<(), VoidClaimAllocationErrorV1> {
    if self.failed {
      return Err(VoidClaimAllocationErrorV1::Failed);
    }
    if self.cancellation.is_cancelled() {
      return Err(VoidClaimAllocationErrorV1::Canceled);
    }
    if self.permit.is_none() {
      return Err(VoidClaimAllocationErrorV1::invalid("void_claim_allocation_permit", "claim permit is absent"));
    }
    Ok(())
  }

  fn hash_algorithm(&self) -> Result<HashAlgorithm, VoidClaimAllocationErrorV1> {
    self
      .permit
      .as_ref()
      .map(VoidClaimAdmissionPermitV1::hash_algorithm)
      .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_permit", "claim permit is absent"))
  }

  fn take_next_subrange(&mut self, length: u32) -> Result<VoidClaimReturnedExtentV1, VoidClaimAllocationErrorV1> {
    loop {
      let extent = self
        .permit
        .as_ref()
        .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_permit", "claim permit is absent"))?
        .claimed_extents()
        .get(self.current_extent_index)
        .cloned();
      let Some(extent) = extent else {
        return Err(VoidClaimAllocationErrorV1::invalid("void_claim_allocation_exhausted", "durable claim has no fitting subrange"));
      };
      let extent_end = extent
        .offset
        .checked_add(u64::from(extent.length))
        .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_extent", "claim extent end overflowed"))?;
      let remaining = extent_end
        .checked_sub(self.current_offset)
        .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_extent", "allocation cursor left its claim extent"))?;
      if remaining >= u64::from(length) {
        let subrange = returned_from_admitted(&extent, self.current_offset, length);
        self.current_offset = self
          .current_offset
          .checked_add(u64::from(length))
          .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_extent", "allocation cursor overflowed"))?;
        return Ok(subrange);
      }
      if remaining != 0 {
        let skipped = returned_from_admitted(&extent, self.current_offset, u32::try_from(remaining)?);
        self.record_returned(&skipped)?;
      }
      self.current_extent_index += 1;
      if let Some(next) = self.permit.as_ref().and_then(|permit| permit.claimed_extents().get(self.current_extent_index)) {
        self.current_offset = next.offset;
      }
    }
  }

  fn record_durable_use(
    &mut self,
    ordinal: u32,
    subrange: &VoidClaimReturnedExtentV1,
    receipt: VoidClaimDurableUseV1,
  ) -> Result<VoidClaimAllocationDispositionV1, VoidClaimAllocationErrorV1> {
    let algorithm = self.hash_algorithm()?;
    let incarnation = PhysicalIncarnationV1 {
      logical_key: &receipt.logical_key,
      integrity_or_legacy_digest: &receipt.integrity_digest,
      wal_offset: receipt.wal_offset,
      write_sequence: receipt.write_sequence,
      entity_length: receipt.entity_length,
      entry_type: receipt.entry_type,
      entity_version: receipt.entity_version,
    };
    let mut encoded_incarnation = vec![0u8; 24 + 2 * algorithm.hash_length()];
    let invalid_receipt_detail = if receipt.wal_offset != subrange.offset || receipt.entity_length != subrange.length {
      Some("claimed durable locator does not exactly occupy its allocated subrange".to_string())
    } else {
      match encode_physical_incarnation_into(&mut encoded_incarnation, &incarnation, algorithm) {
        Ok(()) => None,
        Err(error) => Some(format!("claimed durable locator is malformed: {error}")),
      }
    };
    if let Some(invalid_receipt_detail) = invalid_receipt_detail {
      let evidence_digest = digest_parts(
        algorithm,
        &[b"aeordb.void-claim-invalid-durable-receipt.v1\0", &ordinal.to_le_bytes(), &subrange.offset.to_le_bytes()],
      );
      self.record_uncertain(subrange, u16::MAX, evidence_digest.clone())?;
      self.push_evidence(ordinal, 3, subrange, u16::MAX, &evidence_digest)?;
      return Err(VoidClaimAllocationErrorV1::invalid("void_claim_allocation_durable_receipt", invalid_receipt_detail));
    }
    self.used_bytes = self
      .used_bytes
      .checked_add(u64::from(subrange.length))
      .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_used_bytes", "used byte total overflowed"))?;
    self.push_evidence(ordinal, 1, subrange, 0, &encoded_incarnation)?;
    let write_sequence = receipt.write_sequence;
    self.durable_uses.push(VoidClaimDurableUseV1 {
      logical_key: receipt.logical_key.as_slice().to_vec(),
      integrity_digest: receipt.integrity_digest.as_slice().to_vec(),
      wal_offset: receipt.wal_offset,
      write_sequence,
      entity_length: receipt.entity_length,
      entry_type: receipt.entry_type,
      entity_version: receipt.entity_version,
    });
    Ok(VoidClaimAllocationDispositionV1::Durable { ordinal, wal_offset: subrange.offset, entity_length: subrange.length, write_sequence })
  }

  fn record_returned(&mut self, subrange: &VoidClaimReturnedExtentV1) -> Result<(), VoidClaimAllocationErrorV1> {
    self.returned_bytes = self
      .returned_bytes
      .checked_add(u64::from(subrange.length))
      .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_returned_bytes", "returned byte total overflowed"))?;
    self.returned_extents.push(subrange.clone());
    Ok(())
  }

  fn record_uncertain(
    &mut self,
    subrange: &VoidClaimReturnedExtentV1,
    reason_code: u16,
    evidence_digest: Vec<u8>,
  ) -> Result<(), VoidClaimAllocationErrorV1> {
    self.uncertain_bytes = self
      .uncertain_bytes
      .checked_add(u64::from(subrange.length))
      .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_uncertain_bytes", "uncertain byte total overflowed"))?;
    self.uncertain_extents.push(VoidClaimUncertainExtentV1 {
      offset: subrange.offset,
      length: subrange.length,
      reason_code,
      evidence_digest: evidence_digest.as_slice().to_vec(),
    });
    Ok(())
  }

  fn record_uncertain_from_admitted(
    &mut self,
    extent: &VoidClaimAdmittedExtentV1,
    reason_code: u16,
    evidence_digest: Vec<u8>,
  ) -> Result<(), VoidClaimAllocationErrorV1> {
    self.record_uncertain(&returned_from_admitted(extent, extent.offset, extent.length), reason_code, evidence_digest)
  }

  fn return_all_unallocated(&mut self) -> Result<(), VoidClaimAllocationErrorV1> {
    let extent_count = self
      .permit
      .as_ref()
      .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_permit", "claim permit is absent"))?
      .claimed_extents()
      .len();
    while self.current_extent_index < extent_count {
      if self.cancellation.is_cancelled() {
        return Err(VoidClaimAllocationErrorV1::Canceled);
      }
      let extent = self
        .permit
        .as_ref()
        .and_then(|permit| permit.claimed_extents().get(self.current_extent_index))
        .cloned()
        .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_extent", "claim extent disappeared"))?;
      let end = extent
        .offset
        .checked_add(u64::from(extent.length))
        .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_extent", "claim extent end overflowed"))?;
      let start = if self.current_extent_index == 0 || self.current_offset >= extent.offset { self.current_offset } else { extent.offset };
      if start < end {
        self.record_returned(&returned_from_admitted(&extent, start, u32::try_from(end - start)?))?;
      }
      self.current_extent_index += 1;
      if let Some(next) = self.permit.as_ref().and_then(|permit| permit.claimed_extents().get(self.current_extent_index)) {
        self.current_offset = next.offset;
      }
    }
    Ok(())
  }

  fn push_evidence(
    &mut self,
    ordinal: u32,
    class: u16,
    subrange: &VoidClaimReturnedExtentV1,
    reason_code: u16,
    detail: &[u8],
  ) -> Result<(), VoidClaimAllocationErrorV1> {
    self.evidence.push(&[
      &ordinal.to_le_bytes(),
      &class.to_le_bytes(),
      &reason_code.to_le_bytes(),
      &subrange.offset.to_le_bytes(),
      &subrange.length.to_le_bytes(),
      &subrange.reclaim_commit_sequence.to_le_bytes(),
      &subrange.void_generation.to_le_bytes(),
      &subrange.origin_sweep_proposal_hash,
      &subrange.origin_quarantine_manifest_hash,
      detail,
    ])
  }
}

fn valid_admitted_extent(extent: &VoidClaimAdmittedExtentV1, algorithm: HashAlgorithm) -> bool {
  let hash_width = algorithm.hash_length();
  extent.offset >= 2_048
    && extent.length != 0
    && extent.reclaim_commit_sequence != 0
    && extent.void_generation != 0
    && extent.origin_sweep_proposal_hash.len() == hash_width
    && extent.origin_quarantine_manifest_hash.len() == hash_width
    && extent.reclaimed_incarnation_digest.len() == hash_width
    && extent.origin_sweep_proposal_hash.iter().any(|byte| *byte != 0)
    && extent.origin_quarantine_manifest_hash.iter().any(|byte| *byte != 0)
    && extent.reclaimed_incarnation_digest.iter().any(|byte| *byte != 0)
    && extent.offset.checked_add(u64::from(extent.length)).is_some()
}

fn returned_from_admitted(extent: &VoidClaimAdmittedExtentV1, offset: u64, length: u32) -> VoidClaimReturnedExtentV1 {
  VoidClaimReturnedExtentV1 {
    offset,
    length,
    reclaim_commit_sequence: extent.reclaim_commit_sequence,
    void_generation: extent.void_generation,
    origin_sweep_proposal_hash: extent.origin_sweep_proposal_hash.clone(),
    origin_quarantine_manifest_hash: extent.origin_quarantine_manifest_hash.clone(),
    reclaimed_incarnation_digest: extent.reclaimed_incarnation_digest.clone(),
  }
}

fn same_provenance(left: &VoidClaimReturnedExtentV1, right: &VoidClaimReturnedExtentV1) -> bool {
  left.reclaim_commit_sequence == right.reclaim_commit_sequence
    && left.void_generation == right.void_generation
    && left.origin_sweep_proposal_hash == right.origin_sweep_proposal_hash
    && left.origin_quarantine_manifest_hash == right.origin_quarantine_manifest_hash
    && left.reclaimed_incarnation_digest == right.reclaimed_incarnation_digest
}

fn merge_returned_extents(extents: &mut Vec<VoidClaimReturnedExtentV1>) -> Result<(), VoidClaimAllocationErrorV1> {
  extents.sort_unstable_by_key(|extent| extent.offset);
  let mut output = 0usize;
  for index in 0..extents.len() {
    if output != 0 {
      let previous = &extents[output - 1];
      let previous_end = previous
        .offset
        .checked_add(u64::from(previous.length))
        .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_returned_extent", "returned extent overflowed"))?;
      if previous_end > extents[index].offset {
        return Err(VoidClaimAllocationErrorV1::invalid("void_claim_allocation_returned_overlap", "returned claim fragments overlap"));
      }
      if previous_end == extents[index].offset && same_provenance(previous, &extents[index]) {
        let merged = u64::from(previous.length)
          .checked_add(u64::from(extents[index].length))
          .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_returned_extent", "merged extent overflowed"))?;
        extents[output - 1].length = u32::try_from(merged)?;
        continue;
      }
    }
    extents.swap(output, index);
    output += 1;
  }
  extents.truncate(output);
  Ok(())
}

fn sum_returned_bytes(extents: &[VoidClaimReturnedExtentV1]) -> Result<u64, VoidClaimAllocationErrorV1> {
  extents.iter().try_fold(0u64, |total, extent| {
    total
      .checked_add(u64::from(extent.length))
      .ok_or_else(|| VoidClaimAllocationErrorV1::invalid("void_claim_allocation_returned_bytes", "returned byte total overflowed"))
  })
}

const VOID_SETTLEMENT_FREE_DIGEST_DOMAIN_V1: &[u8] = b"aeordb.void-claim-settlement-free.v1\0";
const VOID_SETTLEMENT_CLAIM_DIGEST_DOMAIN_V1: &[u8] = b"aeordb.void-claim-settlement-claim.v1\0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoidClaimSettlementTransitionLimitsV1 {
  pub maximum_support_artifacts_per_catalog: u64,
}

#[derive(Debug)]
struct SettlementDigestChainV1 {
  algorithm: HashAlgorithm,
  domain: &'static [u8],
  value: Vec<u8>,
  count: u64,
}

impl SettlementDigestChainV1 {
  fn new(algorithm: HashAlgorithm, domain: &'static [u8]) -> Self {
    Self { algorithm, domain, value: digest_parts(algorithm, &[domain]), count: 0 }
  }

  fn push(&mut self, parts: &[&[u8]]) -> Result<(), VoidClaimSettlementTransitionErrorV1> {
    if parts.len() > 7 {
      return Err(VoidClaimSettlementTransitionErrorV1::invalid(
        "void_settlement_transition_digest_parts",
        "settlement semantic row exceeds its fixed part bound",
      ));
    }
    self.count = self
      .count
      .checked_add(1)
      .ok_or_else(|| VoidClaimSettlementTransitionErrorV1::invalid("void_settlement_transition_digest_count", "digest count overflowed"))?;
    let count = self.count.to_le_bytes();
    let mut inputs = [&[][..]; 10];
    inputs[0] = self.domain;
    inputs[1] = &self.value;
    inputs[2] = &count;
    inputs[3..3 + parts.len()].copy_from_slice(parts);
    self.value = digest_parts(self.algorithm, &inputs[..3 + parts.len()]);
    Ok(())
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoidClaimSettlementTransitionSummaryV1 {
  pub source_manifest_key: Vec<u8>,
  pub result_manifest_key: Vec<u8>,
  pub claim_key: Vec<u8>,
  pub used_count: u32,
  pub unused_count: u32,
  pub used_bytes: u64,
  pub returned_bytes: u64,
  pub uncertain_bytes: u64,
  pub evidence_digest: Vec<u8>,
  pub source_closure: VoidCatalogClosureSummaryV1,
  pub result_closure: VoidCatalogClosureSummaryV1,
}

#[derive(Debug, Error)]
pub enum VoidClaimSettlementTransitionErrorV1 {
  #[error("Void claim settlement transition was canceled")]
  Canceled,
  #[error("Void claim settlement transition validator is already failed")]
  Failed,
  #[error("{code}: {message}")]
  Invalid { code: &'static str, message: String },
  #[error("Void claim settlement transition closure failed: {0}")]
  Closure(#[from] VoidCatalogClosureErrorV1),
  #[error("Void claim settlement transition format failed: {0}")]
  Format(#[from] FormatError),
  #[error("Void claim settlement transition memory admission failed: {0}")]
  Memory(#[from] MemoryCoordinatorError),
  #[error("Void claim settlement transition integer conversion failed: {0}")]
  IntegerConversion(#[from] TryFromIntError),
}

impl VoidClaimSettlementTransitionErrorV1 {
  pub fn code(&self) -> &str {
    match self {
      Self::Canceled => "void_settlement_transition_canceled",
      Self::Failed => "void_settlement_transition_failed",
      Self::Invalid { code, .. } => code,
      Self::Closure(source) => source.code(),
      Self::Format(source) => source.code(),
      Self::Memory(_) => "void_settlement_transition_memory",
      Self::IntegerConversion(_) => "void_settlement_transition_integer_conversion",
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }
}

pub struct VoidClaimSettlementTransitionValidatorV1<'a> {
  source_manifest: &'a VoidCatalogManifestV1<'a>,
  result_manifest: &'a VoidCatalogManifestV1<'a>,
  claim: &'a VoidClaimV1<'a>,
  consumption: &'a VoidClaimConsumptionPermitV1,
  cancellation: CancellationToken,
  source_validator: Option<VoidCatalogClosureValidatorV1<'a>>,
  result_validator: Option<VoidCatalogClosureValidatorV1<'a>>,
  returned_index: usize,
  expected_free_digest: SettlementDigestChainV1,
  result_free_digest: SettlementDigestChainV1,
  expected_claim_digest: SettlementDigestChainV1,
  result_claim_digest: SettlementDigestChainV1,
  expected_free_count: u64,
  expected_free_bytes: u64,
  target_claim_seen: bool,
  source_closure: Option<VoidCatalogClosureSummaryV1>,
  _memory: MemoryReservation,
  failed: bool,
}

impl std::fmt::Debug for VoidClaimSettlementTransitionValidatorV1<'_> {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("VoidClaimSettlementTransitionValidatorV1")
      .field("source_manifest_key", &hex::encode(&self.source_manifest.key))
      .field("result_manifest_key", &hex::encode(&self.result_manifest.key))
      .field("claim_key", &hex::encode(&self.claim.key))
      .field("returned_index", &self.returned_index)
      .field("target_claim_seen", &self.target_claim_seen)
      .field("failed", &self.failed)
      .finish_non_exhaustive()
  }
}

impl<'a> VoidClaimSettlementTransitionValidatorV1<'a> {
  pub fn new(
    source: &'a SweepVoidArtifactV1<'a>,
    result: &'a SweepVoidArtifactV1<'a>,
    claim: &'a SweepVoidArtifactV1<'a>,
    consumption: &'a VoidClaimConsumptionPermitV1,
    cancellation: CancellationToken,
    limits: VoidClaimSettlementTransitionLimitsV1,
    memory: &MemoryCoordinator,
  ) -> Result<Self, VoidClaimSettlementTransitionErrorV1> {
    let (SweepVoidArtifactV1::VoidCatalog(source_manifest), SweepVoidArtifactV1::VoidCatalog(result_manifest)) = (source, result) else {
      return Err(VoidClaimSettlementTransitionErrorV1::invalid(
        "void_settlement_transition_catalog_kind",
        "settlement requires source and result Void catalogs",
      ));
    };
    let SweepVoidArtifactV1::VoidClaim(claim) = claim else {
      return Err(VoidClaimSettlementTransitionErrorV1::invalid(
        "void_settlement_transition_claim_kind",
        "settlement requires one immutable Void claim",
      ));
    };
    if cancellation.is_cancelled() {
      return Err(VoidClaimSettlementTransitionErrorV1::Canceled);
    }
    if limits.maximum_support_artifacts_per_catalog == 0 {
      return Err(VoidClaimSettlementTransitionErrorV1::invalid(
        "void_settlement_transition_limits",
        "support artifact limit must be nonzero",
      ));
    }
    let algorithm = consumption.hash_algorithm();
    let hash_width = algorithm.hash_length();
    if source_manifest.database_id != consumption.database_id()
      || result_manifest.database_id != consumption.database_id()
      || claim.database_id != consumption.database_id()
      || source_manifest.key != consumption.source_manifest_key()
      || claim.key != consumption.claim_key()
      || claim.claim_id != consumption.claim_id()
      || claim.source_manifest_hash != consumption.preclaim_manifest_key()
      || claim.generation != consumption.generation()
      || source_manifest.generation != consumption.generation()
      || result_manifest.generation
        != source_manifest
          .generation
          .checked_add(1)
          .ok_or_else(|| VoidClaimSettlementTransitionErrorV1::invalid("void_settlement_transition_generation", "generation overflowed"))?
      || result_manifest.previous_control_sequence != consumption.source_control_sequence()
      || result_manifest.published_at_ms < source_manifest.published_at_ms
      || result_manifest.next_page_id < source_manifest.next_page_id
      || consumption.evidence_digest().len() != hash_width
      || consumption.evidence_digest().iter().all(|byte| *byte == 0)
    {
      return Err(VoidClaimSettlementTransitionErrorV1::invalid(
        "void_settlement_transition_identity",
        "settlement permit, source, claim, result, generation, time, or predecessor identity differs",
      ));
    }
    validate_consumption_partition(consumption)?;
    validate_returned_extents(consumption.returned_extents(), algorithm)?;
    let closure_limits = VoidCatalogClosureLimitsV1 { maximum_support_artifacts: limits.maximum_support_artifacts_per_catalog };
    let accounted_bytes = u64::try_from(8usize.checked_mul(hash_width).and_then(|bytes| bytes.checked_add(512)).ok_or_else(|| {
      VoidClaimSettlementTransitionErrorV1::invalid("void_settlement_transition_memory", "validator memory estimate overflowed")
    })?)?;
    Ok(Self {
      source_manifest,
      result_manifest,
      claim,
      consumption,
      cancellation: cancellation.clone(),
      source_validator: Some(VoidCatalogClosureValidatorV1::new(source_manifest, algorithm, cancellation.clone(), closure_limits, memory)?),
      result_validator: Some(VoidCatalogClosureValidatorV1::new(result_manifest, algorithm, cancellation, closure_limits, memory)?),
      returned_index: 0,
      expected_free_digest: SettlementDigestChainV1::new(algorithm, VOID_SETTLEMENT_FREE_DIGEST_DOMAIN_V1),
      result_free_digest: SettlementDigestChainV1::new(algorithm, VOID_SETTLEMENT_FREE_DIGEST_DOMAIN_V1),
      expected_claim_digest: SettlementDigestChainV1::new(algorithm, VOID_SETTLEMENT_CLAIM_DIGEST_DOMAIN_V1),
      result_claim_digest: SettlementDigestChainV1::new(algorithm, VOID_SETTLEMENT_CLAIM_DIGEST_DOMAIN_V1),
      expected_free_count: 0,
      expected_free_bytes: 0,
      target_claim_seen: false,
      source_closure: None,
      _memory: memory.reserve(MemoryOwner::GarbageCollection, accounted_bytes, AdmissionClass::Maintenance)?,
      failed: false,
    })
  }

  pub fn observe_source_encoded(&mut self, bytes: &[u8]) -> Result<(), VoidClaimSettlementTransitionErrorV1> {
    if let Err(error) = self.preflight_source() {
      self.failed = true;
      return Err(error);
    }
    let result = self.observe_source_encoded_inner(bytes);
    self.latch(result)
  }

  pub fn finish_source(&mut self) -> Result<(), VoidClaimSettlementTransitionErrorV1> {
    if let Err(error) = self.preflight_source() {
      self.failed = true;
      return Err(error);
    }
    let result = self.finish_source_inner();
    self.latch(result)
  }

  pub fn observe_result_encoded(&mut self, bytes: &[u8]) -> Result<(), VoidClaimSettlementTransitionErrorV1> {
    if let Err(error) = self.preflight_result() {
      self.failed = true;
      return Err(error);
    }
    let result = self.observe_result_encoded_inner(bytes);
    self.latch(result)
  }

  pub fn finish(mut self) -> Result<VoidClaimSettlementTransitionSummaryV1, VoidClaimSettlementTransitionErrorV1> {
    self.preflight_result()?;
    let source_closure = self.source_closure.take().ok_or_else(|| {
      VoidClaimSettlementTransitionErrorV1::invalid("void_settlement_transition_phase", "source closure was not finished")
    })?;
    let result_closure = self
      .result_validator
      .take()
      .ok_or_else(|| {
        VoidClaimSettlementTransitionErrorV1::invalid("void_settlement_transition_phase", "result closure was already finished")
      })?
      .finish()?;
    let expected_claim_count = source_closure.outstanding_claim_count.checked_sub(1).ok_or_else(|| {
      VoidClaimSettlementTransitionErrorV1::invalid("void_settlement_transition_claim_count", "source has no claim to settle")
    })?;
    let expected_claimed_bytes = source_closure.claimed_bytes.checked_sub(self.consumption.claimed_bytes()).ok_or_else(|| {
      VoidClaimSettlementTransitionErrorV1::invalid("void_settlement_transition_claim_bytes", "claim bytes exceed source claimed bytes")
    })?;
    if self.result_free_digest.count != self.expected_free_digest.count
      || self.result_free_digest.value != self.expected_free_digest.value
      || self.result_claim_digest.count != self.expected_claim_digest.count
      || self.result_claim_digest.value != self.expected_claim_digest.value
      || result_closure.free_extent_count != self.expected_free_count
      || result_closure.free_bytes != self.expected_free_bytes
      || result_closure.outstanding_claim_count != expected_claim_count
      || result_closure.claimed_bytes != expected_claimed_bytes
    {
      return Err(VoidClaimSettlementTransitionErrorV1::invalid(
        "void_settlement_transition_result",
        "result catalog is not the exact source minus its claim plus only proven unused fragments",
      ));
    }
    Ok(VoidClaimSettlementTransitionSummaryV1 {
      source_manifest_key: self.source_manifest.key.clone(),
      result_manifest_key: self.result_manifest.key.clone(),
      claim_key: self.claim.key.clone(),
      used_count: u32::try_from(self.consumption.durable_uses().len())?,
      unused_count: u32::try_from(self.consumption.returned_extents().len())?,
      used_bytes: self.consumption.used_bytes(),
      returned_bytes: self.consumption.returned_bytes(),
      uncertain_bytes: self.consumption.uncertain_bytes(),
      evidence_digest: self.consumption.evidence_digest().to_vec(),
      source_closure,
      result_closure,
    })
  }

  fn observe_source_encoded_inner(&mut self, bytes: &[u8]) -> Result<(), VoidClaimSettlementTransitionErrorV1> {
    self.check_cancellation()?;
    self
      .source_validator
      .as_mut()
      .ok_or_else(|| VoidClaimSettlementTransitionErrorV1::invalid("void_settlement_transition_phase", "source validator is absent"))?
      .observe_encoded(bytes)?;
    match decode_sweep_void_artifact(bytes, self.consumption.hash_algorithm())? {
      SweepVoidArtifactV1::VoidExtentPage(page) => {
        for extent in page.extent_records()? {
          self.observe_source_free(&extent?)?;
        }
      }
      SweepVoidArtifactV1::VoidClaim(claim) => self.observe_source_claim(&claim)?,
      SweepVoidArtifactV1::VoidDirectory(_) => {}
      _ => {
        return Err(VoidClaimSettlementTransitionErrorV1::invalid(
          "void_settlement_transition_source_kind",
          "source closure contains a non-Void-support artifact",
        ));
      }
    }
    Ok(())
  }

  fn finish_source_inner(&mut self) -> Result<(), VoidClaimSettlementTransitionErrorV1> {
    self.check_cancellation()?;
    self.push_remaining_returned()?;
    if !self.target_claim_seen {
      return Err(VoidClaimSettlementTransitionErrorV1::invalid(
        "void_settlement_transition_claim_missing",
        "selected source closure omits the claim being settled",
      ));
    }
    let source_closure = self
      .source_validator
      .take()
      .ok_or_else(|| VoidClaimSettlementTransitionErrorV1::invalid("void_settlement_transition_phase", "source closure already finished"))?
      .finish()?;
    self.source_closure = Some(source_closure);
    Ok(())
  }

  fn observe_result_encoded_inner(&mut self, bytes: &[u8]) -> Result<(), VoidClaimSettlementTransitionErrorV1> {
    self.check_cancellation()?;
    self
      .result_validator
      .as_mut()
      .ok_or_else(|| VoidClaimSettlementTransitionErrorV1::invalid("void_settlement_transition_phase", "result validator is absent"))?
      .observe_encoded(bytes)?;
    match decode_sweep_void_artifact(bytes, self.consumption.hash_algorithm())? {
      SweepVoidArtifactV1::VoidExtentPage(page) => {
        for extent in page.extent_records()? {
          push_settlement_free_digest(&mut self.result_free_digest, &extent?)?;
        }
      }
      SweepVoidArtifactV1::VoidClaim(claim) => push_settlement_claim_digest(&mut self.result_claim_digest, &claim)?,
      SweepVoidArtifactV1::VoidDirectory(_) => {}
      _ => {
        return Err(VoidClaimSettlementTransitionErrorV1::invalid(
          "void_settlement_transition_result_kind",
          "result closure contains a non-Void-support artifact",
        ));
      }
    }
    Ok(())
  }

  fn observe_source_free(&mut self, source: &VoidExtentRecordV1<'_>) -> Result<(), VoidClaimSettlementTransitionErrorV1> {
    while let Some(returned) = self.consumption.returned_extents().get(self.returned_index) {
      if returned.offset >= source.offset {
        break;
      }
      let returned_end = returned.offset.checked_add(u64::from(returned.length)).ok_or_else(|| {
        VoidClaimSettlementTransitionErrorV1::invalid("void_settlement_transition_returned_extent", "returned extent overflowed")
      })?;
      if returned_end > source.offset {
        return Err(VoidClaimSettlementTransitionErrorV1::invalid(
          "void_settlement_transition_returned_overlap",
          "returned claim fragment overlaps selected free authority",
        ));
      }
      self.push_expected_returned(returned)?;
      self.returned_index += 1;
    }
    if let Some(returned) = self.consumption.returned_extents().get(self.returned_index) {
      let source_end = source.offset.checked_add(u64::from(source.length)).ok_or_else(|| {
        VoidClaimSettlementTransitionErrorV1::invalid("void_settlement_transition_source_extent", "source extent overflowed")
      })?;
      if returned.offset < source_end {
        return Err(VoidClaimSettlementTransitionErrorV1::invalid(
          "void_settlement_transition_returned_overlap",
          "returned claim fragment overlaps selected free authority",
        ));
      }
    }
    push_settlement_free_digest(&mut self.expected_free_digest, source)?;
    self.expected_free_count = self
      .expected_free_count
      .checked_add(1)
      .ok_or_else(|| VoidClaimSettlementTransitionErrorV1::invalid("void_settlement_transition_free_count", "free count overflowed"))?;
    self.expected_free_bytes = self
      .expected_free_bytes
      .checked_add(u64::from(source.length))
      .ok_or_else(|| VoidClaimSettlementTransitionErrorV1::invalid("void_settlement_transition_free_bytes", "free bytes overflowed"))?;
    Ok(())
  }

  fn observe_source_claim(&mut self, source: &VoidClaimV1<'_>) -> Result<(), VoidClaimSettlementTransitionErrorV1> {
    if source.claim_id == self.consumption.claim_id() {
      if self.target_claim_seen || source.key != self.consumption.claim_key() || source.total_bytes != self.consumption.claimed_bytes() {
        return Err(VoidClaimSettlementTransitionErrorV1::invalid(
          "void_settlement_transition_claim_changed",
          "target claim is duplicated or differs from its consumption permit",
        ));
      }
      self.target_claim_seen = true;
      return Ok(());
    }
    push_settlement_claim_digest(&mut self.expected_claim_digest, source)
  }

  fn push_remaining_returned(&mut self) -> Result<(), VoidClaimSettlementTransitionErrorV1> {
    while let Some(returned) = self.consumption.returned_extents().get(self.returned_index) {
      self.push_expected_returned(returned)?;
      self.returned_index += 1;
    }
    Ok(())
  }

  fn push_expected_returned(&mut self, returned: &VoidClaimReturnedExtentV1) -> Result<(), VoidClaimSettlementTransitionErrorV1> {
    push_settlement_free_digest(&mut self.expected_free_digest, &returned.as_record())?;
    self.expected_free_count = self
      .expected_free_count
      .checked_add(1)
      .ok_or_else(|| VoidClaimSettlementTransitionErrorV1::invalid("void_settlement_transition_free_count", "free count overflowed"))?;
    self.expected_free_bytes = self
      .expected_free_bytes
      .checked_add(u64::from(returned.length))
      .ok_or_else(|| VoidClaimSettlementTransitionErrorV1::invalid("void_settlement_transition_free_bytes", "free bytes overflowed"))?;
    Ok(())
  }

  fn preflight_source(&self) -> Result<(), VoidClaimSettlementTransitionErrorV1> {
    if self.failed {
      return Err(VoidClaimSettlementTransitionErrorV1::Failed);
    }
    if self.source_validator.is_none() || self.source_closure.is_some() {
      return Err(VoidClaimSettlementTransitionErrorV1::invalid(
        "void_settlement_transition_phase",
        "source observation is unavailable after source closure finishes",
      ));
    }
    self.check_cancellation()
  }

  fn preflight_result(&self) -> Result<(), VoidClaimSettlementTransitionErrorV1> {
    if self.failed {
      return Err(VoidClaimSettlementTransitionErrorV1::Failed);
    }
    if self.source_validator.is_some() || self.source_closure.is_none() || self.result_validator.is_none() {
      return Err(VoidClaimSettlementTransitionErrorV1::invalid(
        "void_settlement_transition_phase",
        "result observation requires one complete source closure",
      ));
    }
    self.check_cancellation()
  }

  fn check_cancellation(&self) -> Result<(), VoidClaimSettlementTransitionErrorV1> {
    if self.cancellation.is_cancelled() {
      Err(VoidClaimSettlementTransitionErrorV1::Canceled)
    } else {
      Ok(())
    }
  }

  fn latch(&mut self, result: Result<(), VoidClaimSettlementTransitionErrorV1>) -> Result<(), VoidClaimSettlementTransitionErrorV1> {
    match result {
      Ok(()) => Ok(()),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }
}

fn validate_consumption_partition(consumption: &VoidClaimConsumptionPermitV1) -> Result<(), VoidClaimSettlementTransitionErrorV1> {
  let used_bytes = consumption.durable_uses().iter().try_fold(0u64, |total, use_record| {
    total
      .checked_add(u64::from(use_record.entity_length))
      .ok_or_else(|| VoidClaimSettlementTransitionErrorV1::invalid("void_settlement_transition_used_bytes", "used byte total overflowed"))
  })?;
  let returned_bytes = consumption.returned_extents().iter().try_fold(0u64, |total, extent| {
    total.checked_add(u64::from(extent.length)).ok_or_else(|| {
      VoidClaimSettlementTransitionErrorV1::invalid("void_settlement_transition_returned_bytes", "returned byte total overflowed")
    })
  })?;
  let uncertain_bytes = consumption.uncertain_extents().iter().try_fold(0u64, |total, extent| {
    total.checked_add(u64::from(extent.length)).ok_or_else(|| {
      VoidClaimSettlementTransitionErrorV1::invalid("void_settlement_transition_uncertain_bytes", "uncertain byte total overflowed")
    })
  })?;
  let partition = used_bytes
    .checked_add(returned_bytes)
    .and_then(|bytes| bytes.checked_add(uncertain_bytes))
    .ok_or_else(|| VoidClaimSettlementTransitionErrorV1::invalid("void_settlement_transition_partition", "partition overflowed"))?;
  if used_bytes != consumption.used_bytes()
    || returned_bytes != consumption.returned_bytes()
    || uncertain_bytes != consumption.uncertain_bytes()
    || partition != consumption.claimed_bytes()
    || consumption.outcome() == VoidClaimConsumptionOutcomeV1::Settled && (used_bytes == 0 || consumption.durable_uses().is_empty())
    || consumption.outcome() == VoidClaimConsumptionOutcomeV1::AbandonedToQuarantine
      && (used_bytes != 0 || returned_bytes != 0 || !consumption.durable_uses().is_empty() || !consumption.returned_extents().is_empty())
  {
    return Err(VoidClaimSettlementTransitionErrorV1::invalid(
      "void_settlement_transition_partition",
      "consumption evidence does not exactly partition the claim for its outcome",
    ));
  }
  Ok(())
}

fn validate_returned_extents(
  extents: &[VoidClaimReturnedExtentV1],
  algorithm: HashAlgorithm,
) -> Result<(), VoidClaimSettlementTransitionErrorV1> {
  let mut previous_end = None;
  for extent in extents {
    if extent.length == 0
      || extent.offset < 2_048
      || extent.reclaim_commit_sequence == 0
      || extent.void_generation == 0
      || [&extent.origin_sweep_proposal_hash, &extent.origin_quarantine_manifest_hash, &extent.reclaimed_incarnation_digest]
        .iter()
        .any(|hash| hash.len() != algorithm.hash_length() || hash.iter().all(|byte| *byte == 0))
    {
      return Err(VoidClaimSettlementTransitionErrorV1::invalid(
        "void_settlement_transition_returned_extent",
        "returned extent identity or provenance is invalid",
      ));
    }
    let end = extent.offset.checked_add(u64::from(extent.length)).ok_or_else(|| {
      VoidClaimSettlementTransitionErrorV1::invalid("void_settlement_transition_returned_extent", "returned extent overflowed")
    })?;
    if previous_end.is_some_and(|prior| prior > extent.offset) {
      return Err(VoidClaimSettlementTransitionErrorV1::invalid(
        "void_settlement_transition_returned_order",
        "returned extents overlap or are out of order",
      ));
    }
    previous_end = Some(end);
  }
  Ok(())
}

fn push_settlement_free_digest(
  digest: &mut SettlementDigestChainV1,
  extent: &VoidExtentRecordV1<'_>,
) -> Result<(), VoidClaimSettlementTransitionErrorV1> {
  digest.push(&[
    &extent.offset.to_le_bytes(),
    &extent.length.to_le_bytes(),
    &extent.reclaim_commit_sequence.to_le_bytes(),
    &extent.void_generation.to_le_bytes(),
    extent.origin_sweep_proposal_hash,
    extent.origin_quarantine_manifest_hash,
    extent.reclaimed_incarnation_digest,
  ])
}

fn push_settlement_claim_digest(
  digest: &mut SettlementDigestChainV1,
  claim: &VoidClaimV1<'_>,
) -> Result<(), VoidClaimSettlementTransitionErrorV1> {
  digest.push(&[claim.claim_id, &claim.key])
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExistingVoidClaimSettlementReceiptV1 {
  pub receipt_hash: Vec<u8>,
  pub receipt_write_sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoidClaimSettlementAuthoritySnapshotV1 {
  pub selected_source_manifest_hash: Vec<u8>,
  pub selected_source_control_sequence: u64,
  pub source_catalog_receipt_backed: bool,
  pub source_catalog_closure_current: bool,
  pub claim_outstanding_exact: bool,
  pub durable_used_locators_exact: bool,
  pub uncertain_ranges_quarantined: bool,
  pub replacement_lineage_complete: bool,
  pub allocator_settlement_excluded: bool,
  pub no_other_settlement_active: bool,
  pub memory_coordinator_current: bool,
  pub receipt_search_complete: bool,
  pub conflicting_receipt_count: u32,
  pub existing_receipt: Option<ExistingVoidClaimSettlementReceiptV1>,
  pub repair_latch_clear: bool,
}

#[derive(Clone, Copy)]
pub struct VoidClaimSettlementAuthorityRequestV1<'a> {
  pub source_manifest: &'a VoidCatalogManifestV1<'a>,
  pub result_manifest: &'a VoidCatalogManifestV1<'a>,
  pub claim: &'a VoidClaimV1<'a>,
  pub transition: &'a VoidClaimSettlementTransitionSummaryV1,
  pub consumption: &'a VoidClaimConsumptionPermitV1,
  pub cancellation: &'a CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct VoidClaimSettlementAuthorityErrorV1 {
  code: String,
  message: String,
}

impl VoidClaimSettlementAuthorityErrorV1 {
  pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
    Self { code: code.into(), message: message.into() }
  }

  pub fn code(&self) -> &str {
    if self.code.is_empty() {
      "void_claim_settlement_authority"
    } else {
      self.code.as_str()
    }
  }
}

pub trait VoidClaimSettlementAuthorityV1 {
  /// Recheck exact durable locator, quarantine, receipt-search, and allocator
  /// state while first authority is held. Implementations must not reenter the
  /// first-authority publisher.
  fn recheck_void_claim_settlement_authority(
    &mut self,
    request: VoidClaimSettlementAuthorityRequestV1<'_>,
  ) -> Result<VoidClaimSettlementAuthoritySnapshotV1, VoidClaimSettlementAuthorityErrorV1>;
}

#[derive(Clone, Copy)]
pub struct VoidClaimSettlementPublicationRequestV1<'a> {
  pub result_manifest: &'a super::gc::EncodedImmutableGcArtifactV1,
  pub result_control: &'a super::gc::EncodedGcActiveControlV1,
  pub settlement: &'a super::gc::EncodedImmutableGcArtifactV1,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
  pub cancellation: &'a CancellationToken,
  pub memory: &'a MemoryCoordinator,
  pub transition_limits: VoidClaimSettlementTransitionLimitsV1,
}

#[must_use = "a selected claim-free Void result must retain its exact settlement receipt outcome"]
#[derive(Debug)]
pub struct VoidClaimSettlementHardPublicationReceiptV1 {
  pub result_manifest_key: Vec<u8>,
  pub result_manifest_write_sequence: u64,
  pub result_control_key: Vec<u8>,
  pub result_control_write_sequence: u64,
  pub result_control_slot: u8,
  pub settlement_key: Vec<u8>,
  pub settlement_write_sequence: u64,
  pub outcome: VoidClaimConsumptionOutcomeV1,
  pub lineage_state: RootRetirementLineageStateV1,
  pub observation: DatabaseHeaderObservationV4,
  pub idempotent: bool,
}

#[derive(Debug, Error)]
pub enum VoidClaimSettlementPublicationErrorV1 {
  #[error("{code}: {message}")]
  Invalid { code: &'static str, message: String },
  #[error("{code}: Void claim result committed, but post-commit handling failed: {message}")]
  Committed { code: &'static str, message: String, receipt: Box<VoidClaimSettlementHardPublicationReceiptV1> },
  #[error("Void claim settlement transition failed: {0}")]
  Transition(#[from] VoidClaimSettlementTransitionErrorV1),
  #[error("Void claim settlement authority recheck failed: {0}")]
  AuthorityRecheck(#[from] VoidClaimSettlementAuthorityErrorV1),
  #[error("Void claim settlement first-authority failure: {0}")]
  Authority(#[from] FirstAuthorityPublicationErrorV1),
  #[error("Void claim settlement retirement-lineage admission failed: {0}")]
  RetirementAdmission(#[from] RetirementJournalReplacementAdmissionErrorV1),
  #[error("Void claim settlement retirement-lineage owner failed: {0}")]
  RetirementOwner(#[from] RetirementJournalOwnerErrorV1),
  #[error("Void claim settlement format failed: {0}")]
  Format(#[from] FormatError),
  #[error("Void claim settlement integer conversion failed: {0}")]
  IntegerConversion(#[from] TryFromIntError),
}

impl VoidClaimSettlementPublicationErrorV1 {
  pub fn code(&self) -> &str {
    match self {
      Self::Invalid { code, .. } | Self::Committed { code, .. } => code,
      Self::Transition(source) => source.code(),
      Self::AuthorityRecheck(source) => source.code(),
      Self::Authority(source) => source.code(),
      Self::RetirementAdmission(source) => source.code(),
      Self::RetirementOwner(source) => source.code(),
      Self::Format(source) => source.code(),
      Self::IntegerConversion(_) => "void_claim_settlement_integer_conversion",
    }
  }

  pub(crate) fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }

  pub(crate) fn committed(code: &'static str, message: impl Into<String>, receipt: VoidClaimSettlementHardPublicationReceiptV1) -> Self {
    Self::Committed { code, message: message.into(), receipt: Box::new(receipt) }
  }

  pub fn committed_receipt(&self) -> Option<&VoidClaimSettlementHardPublicationReceiptV1> {
    match self {
      Self::Committed { receipt, .. } => Some(receipt),
      _ => None,
    }
  }
}

pub(crate) fn validate_void_claim_settlement_authority_v1(
  request: VoidClaimSettlementAuthorityRequestV1<'_>,
  snapshot: &VoidClaimSettlementAuthoritySnapshotV1,
) -> Result<(), VoidClaimSettlementPublicationErrorV1> {
  if request.cancellation.is_cancelled() {
    return Err(VoidClaimSettlementPublicationErrorV1::invalid(
      "void_claim_settlement_canceled",
      "claim settlement was canceled during final authority validation",
    ));
  }
  if snapshot.selected_source_manifest_hash != request.source_manifest.key
    || snapshot.selected_source_control_sequence != request.consumption.source_control_sequence()
    || request.result_manifest.previous_control_sequence != snapshot.selected_source_control_sequence
    || request.transition.source_manifest_key != request.source_manifest.key
    || request.transition.result_manifest_key != request.result_manifest.key
    || request.transition.claim_key != request.claim.key
  {
    return Err(VoidClaimSettlementPublicationErrorV1::invalid(
      "void_claim_settlement_source_authority",
      "selected source or settlement transition identity differs from caller-owned authority",
    ));
  }
  if !snapshot.source_catalog_receipt_backed
    || !snapshot.source_catalog_closure_current
    || !snapshot.claim_outstanding_exact
    || !snapshot.durable_used_locators_exact
    || request.consumption.uncertain_bytes() != 0 && !snapshot.uncertain_ranges_quarantined
    || !snapshot.replacement_lineage_complete
    || !snapshot.allocator_settlement_excluded
    || !snapshot.no_other_settlement_active
    || !snapshot.memory_coordinator_current
    || !snapshot.receipt_search_complete
    || snapshot.conflicting_receipt_count != 0
    || !snapshot.repair_latch_clear
  {
    return Err(VoidClaimSettlementPublicationErrorV1::invalid(
      "void_claim_settlement_authority_incomplete",
      "receipt, closure, claim, locator, quarantine, lineage, allocator, memory, search, or repair authority is incomplete",
    ));
  }
  if snapshot.existing_receipt.as_ref().is_some_and(|receipt| {
    receipt.receipt_hash.len() != request.consumption.hash_algorithm().hash_length()
      || receipt.receipt_hash.iter().all(|byte| *byte == 0)
      || receipt.receipt_write_sequence == 0
  }) {
    return Err(VoidClaimSettlementPublicationErrorV1::invalid(
      "void_claim_settlement_existing_identity",
      "existing settlement receipt authority has an invalid hash or write sequence",
    ));
  }
  Ok(())
}
