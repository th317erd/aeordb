use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::path_utils::normalize_path;

use super::contract_generated::stable_reason_v1;
use super::index_coordinator::{
  IndexCoordinatorErrorV1, IndexCoordinatorV1, IndexGroupMutationRequestV1, IndexMembershipOwnerClassV1, IndexMembershipStateV1,
  IndexMembershipTransitionRequestV1, IndexMutationGroupRequestV1, IndexMutationOperationV1, IndexMutationRequestV1,
};
use super::index_maintenance_scan::{
  IndexMaintenanceScanLimitsV1, IndexMaintenanceScanRequestV1, IndexProducerServiceModeV1,
  derive_index_maintenance_document_operation_id_v1, index_producer_service_mode_v1, validate_index_maintenance_scan_request_v1,
};
use super::index_page::{OrderedIndexRoleV1, decode_ordered_record};
use super::index_record::{DocumentStateOwnerV1, is_valid_document_state_class};

pub const INDEX_PRODUCER_SCOPE_BYTES_MAX: usize = 16 * 1024;
static NEXT_COORDINATOR_TOKEN: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexProducerCoordinatorOptionsV1 {
  pub max_pending_tasks: u32,
  pub max_pending_bytes: u64,
  pub max_attempts: u16,
  pub base_retry_ms: u64,
  pub max_retry_ms: u64,
  pub max_report_owners: u32,
  pub max_report_mutations: u32,
  pub max_report_bytes: u64,
}

impl IndexProducerCoordinatorOptionsV1 {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    max_pending_tasks: u32,
    max_pending_bytes: u64,
    max_attempts: u16,
    base_retry_ms: u64,
    max_retry_ms: u64,
    max_report_owners: u32,
    max_report_mutations: u32,
    max_report_bytes: u64,
  ) -> Result<Self, IndexProducerCoordinatorErrorV1> {
    if max_pending_tasks == 0 {
      return Err(IndexProducerCoordinatorErrorV1::InvalidOptions("pending task limit must be nonzero".to_string()));
    }
    if max_pending_bytes == 0 {
      return Err(IndexProducerCoordinatorErrorV1::InvalidOptions("pending byte limit must be nonzero".to_string()));
    }
    if max_attempts == 0 {
      return Err(IndexProducerCoordinatorErrorV1::InvalidOptions("retry attempt limit must be nonzero".to_string()));
    }
    if base_retry_ms == 0 || max_retry_ms == 0 || base_retry_ms > max_retry_ms {
      return Err(IndexProducerCoordinatorErrorV1::InvalidOptions(
        "retry delays must be nonzero and the base must not exceed the maximum".to_string(),
      ));
    }
    if max_report_owners == 0 || max_report_mutations == 0 || max_report_bytes == 0 {
      return Err(IndexProducerCoordinatorErrorV1::InvalidOptions("report limits must be nonzero".to_string()));
    }
    Ok(Self {
      max_pending_tasks,
      max_pending_bytes,
      max_attempts,
      base_retry_ms,
      max_retry_ms,
      max_report_owners,
      max_report_mutations,
      max_report_bytes,
    })
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum IndexProducerTaskKindV1 {
  MutationWindow,
  Reconcile,
  Build,
  Rebuild,
  Retire,
  Compact,
  Repair,
  ExplicitMutation,
  LegacyMigration,
}

impl IndexProducerTaskKindV1 {
  pub const fn id(self) -> u16 {
    match self {
      Self::MutationWindow => 1,
      Self::Reconcile => 2,
      Self::Build => 3,
      Self::Rebuild => 4,
      Self::Retire => 5,
      Self::Compact => 6,
      Self::Repair => 7,
      Self::ExplicitMutation => 8,
      Self::LegacyMigration => 9,
    }
  }

  pub const fn from_id(id: u16) -> Option<Self> {
    match id {
      1 => Some(Self::MutationWindow),
      2 => Some(Self::Reconcile),
      3 => Some(Self::Build),
      4 => Some(Self::Rebuild),
      5 => Some(Self::Retire),
      6 => Some(Self::Compact),
      7 => Some(Self::Repair),
      8 => Some(Self::ExplicitMutation),
      9 => Some(Self::LegacyMigration),
      _ => None,
    }
  }

  pub const fn requires_journal(self) -> bool {
    matches!(self, Self::MutationWindow | Self::Reconcile)
  }
}

#[derive(Debug, Clone, Copy)]
pub struct IndexProducerTaskRequestV1<'a> {
  pub operation_id: [u8; 16],
  pub kind: IndexProducerTaskKindV1,
  pub publication_sequence: u64,
  pub namespace_root_before: &'a [u8],
  pub namespace_root_after: &'a [u8],
  pub semantic_state_root: &'a [u8],
  pub journal_head: Option<&'a [u8]>,
  pub scope: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
pub struct IndexProducerTaskViewV1<'a> {
  operation_id: [u8; 16],
  kind: IndexProducerTaskKindV1,
  publication_sequence: u64,
  namespace_root_before: &'a [u8],
  namespace_root_after: &'a [u8],
  semantic_state_root: &'a [u8],
  journal_head: Option<&'a [u8]>,
  scope: Option<&'a str>,
}

impl IndexProducerTaskViewV1<'_> {
  pub const fn operation_id(&self) -> [u8; 16] {
    self.operation_id
  }

  pub const fn kind(&self) -> IndexProducerTaskKindV1 {
    self.kind
  }

  pub const fn publication_sequence(&self) -> u64 {
    self.publication_sequence
  }

  pub fn namespace_root_before(&self) -> &[u8] {
    self.namespace_root_before
  }

  pub fn namespace_root_after(&self) -> &[u8] {
    self.namespace_root_after
  }

  pub fn semantic_state_root(&self) -> &[u8] {
    self.semantic_state_root
  }

  pub fn journal_head(&self) -> Option<&[u8]> {
    self.journal_head
  }

  pub const fn scope(&self) -> Option<&str> {
    self.scope
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexProducerFallbackModeV1 {
  AuthoritativeScan,
  ExactPartialPlusScan,
  Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexProducerMutationV1 {
  pub owner_id: Vec<u8>,
  pub role: OrderedIndexRoleV1,
  pub operation: IndexMutationOperationV1,
  pub encoded_record: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexProducerMembershipTransitionV1 {
  pub owner_class: IndexMembershipOwnerClassV1,
  pub document_ordinal: u64,
  pub before: IndexMembershipStateV1,
  pub after: IndexMembershipStateV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexProducerOwnerDispositionV1 {
  Ready,
  FrozenUnindexable { stage: u8, reason: u16, evidence_hash: Option<Vec<u8>> },
  Retryable { stable_reason: u16, retry_after_ms: u64, fallback_mode: IndexProducerFallbackModeV1, evidence_hash: Option<Vec<u8>> },
  Degraded { stable_reason: u16, fallback_mode: IndexProducerFallbackModeV1, evidence_hash: Option<Vec<u8>> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexProducerOwnerOutcomeV1 {
  pub owner_id: Vec<u8>,
  pub disposition: IndexProducerOwnerDispositionV1,
  pub mutations: Vec<IndexProducerMutationV1>,
  pub membership: Option<IndexProducerMembershipTransitionV1>,
}

impl IndexProducerOwnerOutcomeV1 {
  pub fn ready(owner_id: Vec<u8>, mutations: Vec<IndexProducerMutationV1>) -> Self {
    Self { owner_id, disposition: IndexProducerOwnerDispositionV1::Ready, mutations, membership: None }
  }

  pub fn frozen_unindexable(
    owner_id: Vec<u8>,
    stage: u8,
    reason: u16,
    evidence_hash: Option<Vec<u8>>,
    mutations: Vec<IndexProducerMutationV1>,
  ) -> Self {
    Self {
      owner_id,
      disposition: IndexProducerOwnerDispositionV1::FrozenUnindexable { stage, reason, evidence_hash },
      mutations,
      membership: None,
    }
  }

  pub fn retryable(
    owner_id: Vec<u8>,
    stable_reason: u16,
    retry_after_ms: u64,
    fallback_mode: IndexProducerFallbackModeV1,
    evidence_hash: Option<Vec<u8>>,
  ) -> Self {
    Self {
      owner_id,
      disposition: IndexProducerOwnerDispositionV1::Retryable { stable_reason, retry_after_ms, fallback_mode, evidence_hash },
      mutations: Vec::new(),
      membership: None,
    }
  }

  pub fn degraded(
    owner_id: Vec<u8>,
    stable_reason: u16,
    fallback_mode: IndexProducerFallbackModeV1,
    evidence_hash: Option<Vec<u8>>,
    mutations: Vec<IndexProducerMutationV1>,
  ) -> Self {
    Self {
      owner_id,
      disposition: IndexProducerOwnerDispositionV1::Degraded { stable_reason, fallback_mode, evidence_hash },
      mutations,
      membership: None,
    }
  }

  pub fn with_membership(mut self, membership: IndexProducerMembershipTransitionV1) -> Self {
    self.membership = Some(membership);
    self
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexProducerReportV1 {
  pub outcomes: Vec<IndexProducerOwnerOutcomeV1>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexProducerMaintenanceDocumentRequestV1<'a> {
  pub revision_hash: &'a [u8],
  pub path: &'a str,
  pub report: IndexProducerReportV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexProducerSpillReasonV1 {
  AdmissionPressure,
  RetryExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexProducerSpillReceiptV1 {
  spill_id: [u8; 16],
  artifact_key: Vec<u8>,
}

impl IndexProducerSpillReceiptV1 {
  pub fn new(spill_id: [u8; 16], artifact_key: Vec<u8>) -> Result<Self, IndexProducerSpillErrorV1> {
    if spill_id == [0; 16] {
      return Err(IndexProducerSpillErrorV1::new("spill_identity_invalid", "spill identity is all zeroes"));
    }
    if artifact_key.is_empty() || artifact_key.iter().all(|byte| *byte == 0) {
      return Err(IndexProducerSpillErrorV1::new("spill_artifact_invalid", "spill artifact key is absent or all zeroes"));
    }
    Ok(Self { spill_id, artifact_key })
  }

  pub const fn spill_id(&self) -> [u8; 16] {
    self.spill_id
  }

  pub fn artifact_key(&self) -> &[u8] {
    &self.artifact_key
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexProducerSpillErrorV1 {
  code: &'static str,
  context: String,
}

impl IndexProducerSpillErrorV1 {
  pub fn new(code: &'static str, context: impl Into<String>) -> Self {
    Self { code, context: context.into() }
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

pub trait IndexProducerSpillStoreV1 {
  fn spill(
    &mut self,
    task: IndexProducerTaskViewV1<'_>,
    reason: IndexProducerSpillReasonV1,
  ) -> Result<IndexProducerSpillReceiptV1, IndexProducerSpillErrorV1>;
}

/// Durable recovery authority for a task that may also remain queued in
/// memory. This is distinct from pressure spill: successful persistence does
/// not remove the task from the bounded live coordinator.
pub trait IndexProducerDurableTaskStoreV1 {
  fn persist_task(&mut self, task: IndexProducerTaskViewV1<'_>) -> Result<IndexProducerSpillReceiptV1, IndexProducerSpillErrorV1>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexProducerAdmissionV1 {
  Queued,
  Duplicate,
  Spilled { receipt: IndexProducerSpillReceiptV1 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexProducerCompletionV1 {
  Completed { outcomes: Vec<IndexProducerOwnerOutcomeV1> },
  RetryScheduled { attempt: u16, next_retry_at_ms: u64, outcomes: Vec<IndexProducerOwnerOutcomeV1> },
  Spilled { receipt: IndexProducerSpillReceiptV1, outcomes: Vec<IndexProducerOwnerOutcomeV1> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexProducerMaintenanceProgressV1 {
  Advanced { document_operation_id: [u8; 16], outcomes: Vec<IndexProducerOwnerOutcomeV1> },
  RetryScheduled { document_operation_id: [u8; 16], attempt: u16, next_retry_at_ms: u64, outcomes: Vec<IndexProducerOwnerOutcomeV1> },
  Spilled { document_operation_id: [u8; 16], receipt: IndexProducerSpillReceiptV1, outcomes: Vec<IndexProducerOwnerOutcomeV1> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexProducerCoordinatorSnapshotV1 {
  pub pending_tasks: u32,
  pub pending_bytes: u64,
  pub leased_tasks: u32,
  pub completed_tasks: u64,
  pub scheduled_retries: u64,
  pub spilled_tasks: u64,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IndexProducerCoordinatorErrorV1 {
  #[error("invalid index producer coordinator options: {0}")]
  InvalidOptions(String),
  #[error("invalid index producer task: {0}")]
  InvalidTask(String),
  #[error("index producer operation {operation_id:?} conflicts with a retained task")]
  ConflictingTask { operation_id: [u8; 16] },
  #[error("index producer task requires spill: {context}; requested={requested_bytes}, limit={limit_bytes}")]
  SpillRequired { context: String, requested_bytes: u64, limit_bytes: u64 },
  #[error("index producer memory authority failed: {0}")]
  MemoryAuthority(String),
  #[error("index producer allocation failed: {0}")]
  Allocation(String),
  #[error("index producer work was cancelled")]
  Cancelled,
  #[error("index producer lease belongs to another coordinator")]
  ForeignLease,
  #[error("index producer lease is stale")]
  StaleLease,
  #[error("index producer clock regressed from {previous_ms} to {received_ms}")]
  ClockRegressed { previous_ms: u64, received_ms: u64 },
  #[error("invalid index producer report: {0}")]
  InvalidReport(String),
  #[error("invalid index producer maintenance document: {0}")]
  InvalidMaintenanceDocument(String),
  #[error("index mutation admission failed for owner {owner_id}: {source}")]
  MutationAdmission { owner_id: String, source: IndexCoordinatorErrorV1 },
  #[error("index producer spill failed ({code}): {context}")]
  SpillFailed { code: &'static str, context: String },
  #[error("index producer accounting overflow: {0}")]
  AccountingOverflow(&'static str),
  #[error("index producer invariant failed: {0}")]
  Invariant(String),
}

struct RetainedMaintenanceContinuationV1 {
  resume_after: String,
  retained_bytes: u64,
  _reservation: MemoryReservation,
}

pub(crate) struct IndexProducerJournalWorkPlanV1 {
  operation_id: [u8; 16],
  kind: IndexProducerTaskKindV1,
  publication_sequence: u64,
  namespace_root_before: Vec<u8>,
  namespace_root_after: Vec<u8>,
  semantic_state_root: Vec<u8>,
  journal_head: Vec<u8>,
  _reservation: MemoryReservation,
}

impl IndexProducerJournalWorkPlanV1 {
  pub(crate) const fn operation_id(&self) -> [u8; 16] {
    self.operation_id
  }

  pub(crate) const fn kind(&self) -> IndexProducerTaskKindV1 {
    self.kind
  }

  pub(crate) const fn publication_sequence(&self) -> u64 {
    self.publication_sequence
  }

  pub(crate) fn namespace_root_before(&self) -> &[u8] {
    &self.namespace_root_before
  }

  pub(crate) fn namespace_root_after(&self) -> &[u8] {
    &self.namespace_root_after
  }

  pub(crate) fn semantic_state_root(&self) -> &[u8] {
    &self.semantic_state_root
  }

  pub(crate) fn journal_head(&self) -> &[u8] {
    &self.journal_head
  }
}

pub(crate) struct IndexProducerMaintenanceScanPlanV1 {
  namespace_root: Vec<u8>,
  semantic_state_root: Vec<u8>,
  scope: String,
  resume_after: Option<String>,
  limits: IndexMaintenanceScanLimitsV1,
  _reservation: MemoryReservation,
}

impl IndexProducerMaintenanceScanPlanV1 {
  pub(crate) fn namespace_root(&self) -> &[u8] {
    &self.namespace_root
  }

  pub(crate) fn semantic_state_root(&self) -> &[u8] {
    &self.semantic_state_root
  }

  pub(crate) fn request<'request>(&'request self, is_cancelled: &'request dyn Fn() -> bool) -> IndexMaintenanceScanRequestV1<'request> {
    IndexMaintenanceScanRequestV1 {
      namespace_root: &self.namespace_root,
      scope: &self.scope,
      resume_after: self.resume_after.as_deref(),
      limits: self.limits,
      is_cancelled,
    }
  }
}

struct RetainedIndexProducerTaskV1 {
  operation_id: [u8; 16],
  kind: IndexProducerTaskKindV1,
  publication_sequence: u64,
  namespace_root_before: Vec<u8>,
  namespace_root_after: Vec<u8>,
  semantic_state_root: Vec<u8>,
  journal_head: Option<Vec<u8>>,
  scope: Option<String>,
  attempts: u16,
  next_attempt_at_ms: u64,
  maintenance_continuation: Option<RetainedMaintenanceContinuationV1>,
  retained_bytes: u64,
  _reservation: MemoryReservation,
}

impl RetainedIndexProducerTaskV1 {
  fn view(&self) -> IndexProducerTaskViewV1<'_> {
    IndexProducerTaskViewV1 {
      operation_id: self.operation_id,
      kind: self.kind,
      publication_sequence: self.publication_sequence,
      namespace_root_before: &self.namespace_root_before,
      namespace_root_after: &self.namespace_root_after,
      semantic_state_root: &self.semantic_state_root,
      journal_head: self.journal_head.as_deref(),
      scope: self.scope.as_deref(),
    }
  }

  fn matches(&self, request: &IndexProducerTaskRequestV1<'_>) -> bool {
    self.operation_id == request.operation_id
      && self.kind == request.kind
      && self.publication_sequence == request.publication_sequence
      && self.namespace_root_before == request.namespace_root_before
      && self.namespace_root_after == request.namespace_root_after
      && self.semantic_state_root == request.semantic_state_root
      && self.journal_head.as_deref() == request.journal_head
      && self.scope.as_deref() == request.scope
  }
}

#[derive(Debug, Clone)]
pub struct IndexProducerLeaseV1 {
  coordinator_token: u64,
  lease_id: u64,
  operation_id: [u8; 16],
  kind: IndexProducerTaskKindV1,
  publication_sequence: u64,
}

impl IndexProducerLeaseV1 {
  pub const fn operation_id(&self) -> [u8; 16] {
    self.operation_id
  }

  pub const fn kind(&self) -> IndexProducerTaskKindV1 {
    self.kind
  }

  pub const fn publication_sequence(&self) -> u64 {
    self.publication_sequence
  }
}

#[derive(Debug, Clone, Copy)]
struct ActiveLeaseV1 {
  lease_id: u64,
  operation_id: [u8; 16],
}

pub struct IndexProducerCoordinatorV1 {
  coordinator_token: u64,
  hash_algorithm: HashAlgorithm,
  memory: MemoryCoordinator,
  options: IndexProducerCoordinatorOptionsV1,
  tasks: Vec<RetainedIndexProducerTaskV1>,
  pending_bytes: u64,
  active_lease: Option<ActiveLeaseV1>,
  next_lease_id: u64,
  last_observed_ms: u64,
  completed_tasks: u64,
  scheduled_retries: u64,
  spilled_tasks: u64,
}

impl IndexProducerCoordinatorV1 {
  pub fn new(
    hash_algorithm: HashAlgorithm,
    memory: MemoryCoordinator,
    options: IndexProducerCoordinatorOptionsV1,
  ) -> Result<Self, IndexProducerCoordinatorErrorV1> {
    let coordinator_token = NEXT_COORDINATOR_TOKEN.fetch_add(1, Ordering::Relaxed);
    if coordinator_token == 0 {
      return Err(IndexProducerCoordinatorErrorV1::Invariant("coordinator token space exhausted".to_string()));
    }
    Ok(Self {
      coordinator_token,
      hash_algorithm,
      memory,
      options,
      tasks: Vec::new(),
      pending_bytes: 0,
      active_lease: None,
      next_lease_id: 1,
      last_observed_ms: 0,
      completed_tasks: 0,
      scheduled_retries: 0,
      spilled_tasks: 0,
    })
  }

  pub fn snapshot(&self) -> IndexProducerCoordinatorSnapshotV1 {
    let leased_tasks = u32::from(self.active_lease.is_some());
    // Admission caps this vector at a u32 count before every insertion.
    let retained_tasks = self.tasks.len() as u32;
    IndexProducerCoordinatorSnapshotV1 {
      pending_tasks: retained_tasks.saturating_sub(leased_tasks),
      pending_bytes: self.pending_bytes,
      leased_tasks,
      completed_tasks: self.completed_tasks,
      scheduled_retries: self.scheduled_retries,
      spilled_tasks: self.spilled_tasks,
    }
  }

  pub fn admit(
    &mut self,
    request: IndexProducerTaskRequestV1<'_>,
    now_ms: u64,
  ) -> Result<IndexProducerAdmissionV1, IndexProducerCoordinatorErrorV1> {
    self.observe_time(now_ms)?;
    self.validate_task(&request)?;
    if let Some(retained) = self.tasks.iter().find(|task| task.operation_id == request.operation_id) {
      return if retained.matches(&request) {
        Ok(IndexProducerAdmissionV1::Duplicate)
      } else {
        Err(IndexProducerCoordinatorErrorV1::ConflictingTask { operation_id: request.operation_id })
      };
    }
    if self.tasks.len() > u32::MAX as usize {
      return Err(IndexProducerCoordinatorErrorV1::AccountingOverflow("task count"));
    }
    let count = self.tasks.len() as u32;
    if count >= self.options.max_pending_tasks {
      return Err(IndexProducerCoordinatorErrorV1::SpillRequired {
        context: "pending task count limit".to_string(),
        requested_bytes: 0,
        limit_bytes: self.options.max_pending_bytes,
      });
    }
    let retained_bytes = retained_task_bytes(&request)?;
    let next_bytes =
      self.pending_bytes.checked_add(retained_bytes).ok_or(IndexProducerCoordinatorErrorV1::AccountingOverflow("pending task bytes"))?;
    if next_bytes > self.options.max_pending_bytes {
      return Err(IndexProducerCoordinatorErrorV1::SpillRequired {
        context: "pending task byte limit".to_string(),
        requested_bytes: retained_bytes,
        limit_bytes: self.options.max_pending_bytes,
      });
    }
    let reservation = self
      .memory
      .reserve(MemoryOwner::Task, retained_bytes, AdmissionClass::Workload)
      .map_err(|error| memory_error(retained_bytes, self.options.max_pending_bytes, error))?;
    let retained = clone_task(request, now_ms, retained_bytes, reservation)?;
    self
      .tasks
      .try_reserve(1)
      .map_err(|source| IndexProducerCoordinatorErrorV1::Allocation(format!("cannot reserve pending task slot: {source}")))?;
    self.tasks.push(retained);
    self.tasks.sort_unstable_by_key(|task| (task.publication_sequence, task.operation_id));
    self.pending_bytes = next_bytes;
    self.verify_accounting()?;
    Ok(IndexProducerAdmissionV1::Queued)
  }

  pub fn admit_or_spill(
    &mut self,
    request: IndexProducerTaskRequestV1<'_>,
    now_ms: u64,
    spill_store: &mut dyn IndexProducerSpillStoreV1,
  ) -> Result<IndexProducerAdmissionV1, IndexProducerCoordinatorErrorV1> {
    match self.admit(request, now_ms) {
      Ok(admission) => Ok(admission),
      Err(IndexProducerCoordinatorErrorV1::SpillRequired { .. }) => {
        let receipt =
          spill_store.spill(task_view_from_request(&request), IndexProducerSpillReasonV1::AdmissionPressure).map_err(spill_error)?;
        self.validate_spill_receipt(&receipt, request.operation_id)?;
        self.spilled_tasks = self.spilled_tasks.saturating_add(1);
        Ok(IndexProducerAdmissionV1::Spilled { receipt })
      }
      Err(error) => Err(error),
    }
  }

  pub fn admit_durable_or_spill<Store>(
    &mut self,
    request: IndexProducerTaskRequestV1<'_>,
    now_ms: u64,
    store: &mut Store,
  ) -> Result<IndexProducerAdmissionV1, IndexProducerCoordinatorErrorV1>
  where
    Store: IndexProducerDurableTaskStoreV1 + IndexProducerSpillStoreV1,
  {
    self.observe_time(now_ms)?;
    self.validate_task(&request)?;
    if let Some(retained) = self.tasks.iter().find(|task| task.operation_id == request.operation_id) {
      if !retained.matches(&request) {
        return Err(IndexProducerCoordinatorErrorV1::ConflictingTask { operation_id: request.operation_id });
      }
    }
    let receipt = store.persist_task(task_view_from_request(&request)).map_err(spill_error)?;
    self.validate_spill_receipt(&receipt, request.operation_id)?;
    self.admit_or_spill(request, now_ms, store)
  }

  pub fn lease_next(&mut self, now_ms: u64, cancelled: bool) -> Result<Option<IndexProducerLeaseV1>, IndexProducerCoordinatorErrorV1> {
    self.observe_time(now_ms)?;
    if cancelled {
      return Err(IndexProducerCoordinatorErrorV1::Cancelled);
    }
    if self.active_lease.is_some() {
      return Ok(None);
    }
    let Some(task) = self.tasks.iter().find(|task| task.next_attempt_at_ms <= now_ms) else {
      return Ok(None);
    };
    let lease_id = self.next_lease_id;
    self.next_lease_id = self.next_lease_id.checked_add(1).ok_or(IndexProducerCoordinatorErrorV1::AccountingOverflow("lease identity"))?;
    let lease = clone_lease(self.coordinator_token, lease_id, task)?;
    self.active_lease = Some(ActiveLeaseV1 { lease_id, operation_id: task.operation_id });
    Ok(Some(lease))
  }

  pub fn cancel(&mut self, lease: &IndexProducerLeaseV1) -> Result<(), IndexProducerCoordinatorErrorV1> {
    self.validate_lease(lease)?;
    self.active_lease = None;
    Ok(())
  }

  pub fn leased_task(&self, lease: &IndexProducerLeaseV1) -> Result<IndexProducerTaskViewV1<'_>, IndexProducerCoordinatorErrorV1> {
    self.validate_lease(lease)?;
    Ok(self.tasks[self.task_index(lease.operation_id)?].view())
  }

  pub(crate) fn leased_journal_work_plan(
    &self,
    lease: &IndexProducerLeaseV1,
  ) -> Result<IndexProducerJournalWorkPlanV1, IndexProducerCoordinatorErrorV1> {
    self.validate_lease(lease)?;
    let task = &self.tasks[self.task_index(lease.operation_id)?];
    if index_producer_service_mode_v1(task.kind) != IndexProducerServiceModeV1::JournalTransition {
      return Err(IndexProducerCoordinatorErrorV1::InvalidTask("leased task is not journal-transition work".to_string()));
    }
    let journal_head = task
      .journal_head
      .as_deref()
      .ok_or_else(|| IndexProducerCoordinatorErrorV1::Invariant("journal-transition task lost its journal head".to_string()))?;
    let requested_bytes = journal_work_plan_bytes(
      task.namespace_root_before.len(),
      task.namespace_root_after.len(),
      task.semantic_state_root.len(),
      journal_head.len(),
    )?;
    let mut reservation = self
      .memory
      .reserve(MemoryOwner::Task, requested_bytes, AdmissionClass::Workload)
      .map_err(|error| memory_error(requested_bytes, self.options.max_pending_bytes, error))?;
    let namespace_root_before = clone_bytes(&task.namespace_root_before, "journal plan namespace root before")?;
    let namespace_root_after = clone_bytes(&task.namespace_root_after, "journal plan namespace root after")?;
    let semantic_state_root = clone_bytes(&task.semantic_state_root, "journal plan semantic-state root")?;
    let journal_head = clone_bytes(journal_head, "journal plan head")?;
    let retained_bytes = journal_work_plan_bytes(
      namespace_root_before.capacity(),
      namespace_root_after.capacity(),
      semantic_state_root.capacity(),
      journal_head.capacity(),
    )?;
    if retained_bytes > requested_bytes {
      reservation
        .grow(retained_bytes - requested_bytes)
        .map_err(|error| memory_error(retained_bytes, self.options.max_pending_bytes, error))?;
    }
    Ok(IndexProducerJournalWorkPlanV1 {
      operation_id: task.operation_id,
      kind: task.kind,
      publication_sequence: task.publication_sequence,
      namespace_root_before,
      namespace_root_after,
      semantic_state_root,
      journal_head,
      _reservation: reservation,
    })
  }

  pub fn leased_maintenance_resume_after(&self, lease: &IndexProducerLeaseV1) -> Result<Option<&str>, IndexProducerCoordinatorErrorV1> {
    self.validate_lease(lease)?;
    let task = &self.tasks[self.task_index(lease.operation_id)?];
    require_maintenance_scan_task(task.kind)?;
    Ok(task.maintenance_continuation.as_ref().map(|continuation| continuation.resume_after.as_str()))
  }

  pub(crate) fn leased_maintenance_scan_plan(
    &self,
    lease: &IndexProducerLeaseV1,
    limits: IndexMaintenanceScanLimitsV1,
  ) -> Result<IndexProducerMaintenanceScanPlanV1, IndexProducerCoordinatorErrorV1> {
    self.validate_lease(lease)?;
    let task = &self.tasks[self.task_index(lease.operation_id)?];
    require_maintenance_scan_task(task.kind)?;
    let scope = task
      .scope
      .as_deref()
      .ok_or_else(|| IndexProducerCoordinatorErrorV1::Invariant("root-pinned document-scan task lost its scope".to_string()))?;
    let never_cancelled = || false;
    validate_index_maintenance_scan_request_v1(
      self.hash_algorithm,
      &IndexMaintenanceScanRequestV1 {
        namespace_root: &task.namespace_root_after,
        scope,
        resume_after: task.maintenance_continuation.as_ref().map(|continuation| continuation.resume_after.as_str()),
        limits,
        is_cancelled: &never_cancelled,
      },
    )
    .map_err(|error| IndexProducerCoordinatorErrorV1::InvalidMaintenanceDocument(error.to_string()))?;

    let requested_bytes = maintenance_scan_plan_bytes(
      task.namespace_root_after.len(),
      task.semantic_state_root.len(),
      scope.len(),
      task.maintenance_continuation.as_ref().map_or(0, |continuation| continuation.resume_after.len()),
    )?;
    let mut reservation = self
      .memory
      .reserve(MemoryOwner::Task, requested_bytes, AdmissionClass::Workload)
      .map_err(|error| memory_error(requested_bytes, self.options.max_pending_bytes, error))?;
    let namespace_root = clone_bytes(&task.namespace_root_after, "maintenance scan namespace root")?;
    let semantic_state_root = clone_bytes(&task.semantic_state_root, "maintenance scan semantic-state root")?;
    let scope = clone_string(scope, "maintenance scan scope")?;
    let resume_after = task
      .maintenance_continuation
      .as_ref()
      .map(|continuation| clone_string(&continuation.resume_after, "maintenance scan continuation"))
      .transpose()?;
    let retained_bytes = maintenance_scan_plan_bytes(
      namespace_root.capacity(),
      semantic_state_root.capacity(),
      scope.capacity(),
      resume_after.as_ref().map_or(0, String::capacity),
    )?;
    if retained_bytes > requested_bytes {
      reservation
        .grow(retained_bytes - requested_bytes)
        .map_err(|error| memory_error(retained_bytes, self.options.max_pending_bytes, error))?;
    }
    Ok(IndexProducerMaintenanceScanPlanV1 { namespace_root, semantic_state_root, scope, resume_after, limits, _reservation: reservation })
  }

  #[allow(clippy::too_many_arguments)]
  pub fn advance_maintenance_document(
    &mut self,
    lease: &IndexProducerLeaseV1,
    mut request: IndexProducerMaintenanceDocumentRequestV1<'_>,
    mutation_coordinator: &mut IndexCoordinatorV1,
    now_ms: u64,
    cancelled: bool,
    spill_store: &mut dyn IndexProducerSpillStoreV1,
  ) -> Result<IndexProducerMaintenanceProgressV1, IndexProducerCoordinatorErrorV1> {
    self.validate_lease(lease)?;
    if cancelled {
      self.active_lease = None;
      return Err(IndexProducerCoordinatorErrorV1::Cancelled);
    }
    self.observe_time(now_ms)?;
    let task_index = self.task_index(lease.operation_id)?;
    let task = &self.tasks[task_index];
    require_maintenance_scan_task(task.kind)?;
    validate_maintenance_document(self.hash_algorithm, task, request.revision_hash, request.path)?;
    let document_operation_id = derive_index_maintenance_document_operation_id_v1(
      self.hash_algorithm,
      task.operation_id,
      task.kind,
      &task.namespace_root_after,
      request.revision_hash,
      request.path,
    )
    .map_err(|error| IndexProducerCoordinatorErrorV1::InvalidMaintenanceDocument(error.to_string()))?;
    let requested_retry_ms = self.prepare_report(&mut request.report)?;
    let prepared_continuation =
      if requested_retry_ms.is_none() { Some(self.prepare_maintenance_continuation(task_index, request.path)?) } else { None };

    self.admit_report_mutations(&request.report, lease.publication_sequence, document_operation_id, mutation_coordinator, now_ms)?;
    if let Some(requested_retry_ms) = requested_retry_ms {
      return match self.schedule_retry(lease, requested_retry_ms, request.report.outcomes, now_ms, spill_store)? {
        IndexProducerCompletionV1::RetryScheduled { attempt, next_retry_at_ms, outcomes } => {
          Ok(IndexProducerMaintenanceProgressV1::RetryScheduled { document_operation_id, attempt, next_retry_at_ms, outcomes })
        }
        IndexProducerCompletionV1::Spilled { receipt, outcomes } => {
          Ok(IndexProducerMaintenanceProgressV1::Spilled { document_operation_id, receipt, outcomes })
        }
        IndexProducerCompletionV1::Completed { .. } => {
          Err(IndexProducerCoordinatorErrorV1::Invariant("retry scheduling unexpectedly completed a maintenance task".to_string()))
        }
      };
    }

    let prepared_continuation = prepared_continuation
      .ok_or_else(|| IndexProducerCoordinatorErrorV1::Invariant("maintenance continuation was not prepared".to_string()))?;
    self.install_maintenance_continuation(task_index, prepared_continuation)?;
    self.tasks[task_index].attempts = 0;
    self.tasks[task_index].next_attempt_at_ms = now_ms;
    Ok(IndexProducerMaintenanceProgressV1::Advanced { document_operation_id, outcomes: request.report.outcomes })
  }

  pub fn complete(
    &mut self,
    lease: &IndexProducerLeaseV1,
    mut report: IndexProducerReportV1,
    mutation_coordinator: &mut IndexCoordinatorV1,
    now_ms: u64,
    cancelled: bool,
    spill_store: &mut dyn IndexProducerSpillStoreV1,
  ) -> Result<IndexProducerCompletionV1, IndexProducerCoordinatorErrorV1> {
    self.validate_lease(lease)?;
    if cancelled {
      self.active_lease = None;
      return Err(IndexProducerCoordinatorErrorV1::Cancelled);
    }
    self.observe_time(now_ms)?;
    let requested_retry_ms = self.prepare_report(&mut report)?;
    self.admit_report_mutations(&report, lease.publication_sequence, lease.operation_id, mutation_coordinator, now_ms)?;
    if let Some(requested_retry_ms) = requested_retry_ms {
      return self.schedule_retry(lease, requested_retry_ms, report.outcomes, now_ms, spill_store);
    }

    let task_index = self.task_index(lease.operation_id)?;
    self.remove_task(task_index)?;
    self.active_lease = None;
    self.completed_tasks = self.completed_tasks.saturating_add(1);
    Ok(IndexProducerCompletionV1::Completed { outcomes: report.outcomes })
  }

  pub fn retry_task(
    &mut self,
    lease: &IndexProducerLeaseV1,
    requested_retry_ms: u64,
    now_ms: u64,
    cancelled: bool,
    spill_store: &mut dyn IndexProducerSpillStoreV1,
  ) -> Result<IndexProducerCompletionV1, IndexProducerCoordinatorErrorV1> {
    self.validate_lease(lease)?;
    if cancelled {
      self.active_lease = None;
      return Err(IndexProducerCoordinatorErrorV1::Cancelled);
    }
    if requested_retry_ms == 0 {
      return Err(IndexProducerCoordinatorErrorV1::InvalidTask("task retry delay must be nonzero".to_string()));
    }
    self.observe_time(now_ms)?;
    self.schedule_retry(lease, requested_retry_ms, Vec::new(), now_ms, spill_store)
  }

  fn schedule_retry(
    &mut self,
    lease: &IndexProducerLeaseV1,
    requested_retry_ms: u64,
    outcomes: Vec<IndexProducerOwnerOutcomeV1>,
    now_ms: u64,
    spill_store: &mut dyn IndexProducerSpillStoreV1,
  ) -> Result<IndexProducerCompletionV1, IndexProducerCoordinatorErrorV1> {
    let task_index = self.task_index(lease.operation_id)?;
    let attempt = self.tasks[task_index].attempts.saturating_add(1).min(self.options.max_attempts);
    if attempt >= self.options.max_attempts {
      let receipt = match spill_store.spill(self.tasks[task_index].view(), IndexProducerSpillReasonV1::RetryExhausted) {
        Ok(receipt) => receipt,
        Err(error) => {
          self.retain_after_spill_failure(task_index, attempt, now_ms);
          return Err(spill_error(error));
        }
      };
      if let Err(error) = self.validate_spill_receipt(&receipt, self.tasks[task_index].operation_id) {
        self.retain_after_spill_failure(task_index, attempt, now_ms);
        return Err(error);
      }
      self.remove_task(task_index)?;
      self.active_lease = None;
      self.spilled_tasks = self.spilled_tasks.saturating_add(1);
      return Ok(IndexProducerCompletionV1::Spilled { receipt, outcomes });
    }

    let exponential = retry_delay(self.options.base_retry_ms, self.options.max_retry_ms, attempt);
    let delay = requested_retry_ms.max(exponential).min(self.options.max_retry_ms);
    let Some(next_retry_at_ms) = now_ms.checked_add(delay) else {
      self.retain_after_spill_failure(task_index, attempt, now_ms);
      return Err(IndexProducerCoordinatorErrorV1::AccountingOverflow("retry deadline"));
    };
    self.tasks[task_index].attempts = attempt;
    self.tasks[task_index].next_attempt_at_ms = next_retry_at_ms;
    self.active_lease = None;
    self.scheduled_retries = self.scheduled_retries.saturating_add(1);
    Ok(IndexProducerCompletionV1::RetryScheduled { attempt, next_retry_at_ms, outcomes })
  }

  fn prepare_report(&self, report: &mut IndexProducerReportV1) -> Result<Option<u64>, IndexProducerCoordinatorErrorV1> {
    report.outcomes.sort_unstable_by(|left, right| left.owner_id.cmp(&right.owner_id));
    self.validate_report(report)?;
    Ok(
      report
        .outcomes
        .iter()
        .filter_map(|outcome| match outcome.disposition {
          IndexProducerOwnerDispositionV1::Retryable { retry_after_ms, .. } => Some(retry_after_ms),
          _ => None,
        })
        .max(),
    )
  }

  fn admit_report_mutations(
    &self,
    report: &IndexProducerReportV1,
    publication_sequence: u64,
    operation_id: [u8; 16],
    mutation_coordinator: &mut IndexCoordinatorV1,
    now_ms: u64,
  ) -> Result<(), IndexProducerCoordinatorErrorV1> {
    for outcome in &report.outcomes {
      if let Some(membership) = outcome.membership {
        const { assert!(usize::BITS <= u64::BITS) };
        let requested_bytes = outcome
          .mutations
          .len()
          .checked_mul(std::mem::size_of::<IndexGroupMutationRequestV1<'_>>())
          .ok_or(IndexProducerCoordinatorErrorV1::AccountingOverflow("semantic group request bytes"))? as u64;
        let _reservation = self
          .memory
          .reserve(MemoryOwner::Task, requested_bytes, AdmissionClass::Workload)
          .map_err(|error| memory_error(requested_bytes, self.options.max_pending_bytes, error))?;
        let mut mutations = Vec::new();
        mutations.try_reserve_exact(outcome.mutations.len()).map_err(|source| {
          IndexProducerCoordinatorErrorV1::Allocation(format!("cannot reserve semantic group request slots: {source}"))
        })?;
        for mutation in &outcome.mutations {
          mutations.push(IndexGroupMutationRequestV1 {
            operation: mutation.operation,
            mutation: IndexMutationRequestV1 {
              index_id: &mutation.owner_id,
              role: mutation.role,
              publication_sequence,
              operation_id,
              encoded_record: &mutation.encoded_record,
            },
          });
        }
        mutation_coordinator
          .admit_group(
            IndexMutationGroupRequestV1 {
              transition: IndexMembershipTransitionRequestV1 {
                owner_id: &outcome.owner_id,
                owner_class: membership.owner_class,
                publication_sequence,
                operation_id,
                document_ordinal: membership.document_ordinal,
                before: membership.before,
                after: membership.after,
              },
              mutations: &mutations,
            },
            now_ms,
          )
          .map_err(|source| IndexProducerCoordinatorErrorV1::MutationAdmission { owner_id: hex::encode(&outcome.owner_id), source })?;
        continue;
      }
      for mutation in &outcome.mutations {
        if mutation.operation != IndexMutationOperationV1::Upsert {
          return Err(IndexProducerCoordinatorErrorV1::InvalidReport(
            "an explicit remove mutation requires an owner/document membership transition".to_string(),
          ));
        }
        mutation_coordinator
          .admit(
            IndexMutationRequestV1 {
              index_id: &mutation.owner_id,
              role: mutation.role,
              publication_sequence,
              operation_id,
              encoded_record: &mutation.encoded_record,
            },
            now_ms,
          )
          .map_err(|source| IndexProducerCoordinatorErrorV1::MutationAdmission { owner_id: hex::encode(&mutation.owner_id), source })?;
      }
    }
    Ok(())
  }

  fn prepare_maintenance_continuation(
    &self,
    task_index: usize,
    resume_after: &str,
  ) -> Result<RetainedMaintenanceContinuationV1, IndexProducerCoordinatorErrorV1> {
    let old_bytes = self.tasks[task_index].maintenance_continuation.as_ref().map_or(0, |continuation| continuation.retained_bytes);
    let requested_bytes = u64::try_from(resume_after.len())
      .map_err(|source| IndexProducerCoordinatorErrorV1::Invariant(format!("maintenance continuation byte conversion failed: {source}")))?;
    let projected = self
      .pending_bytes
      .checked_sub(old_bytes)
      .and_then(|bytes| bytes.checked_add(requested_bytes))
      .ok_or(IndexProducerCoordinatorErrorV1::AccountingOverflow("maintenance continuation projection"))?;
    if projected > self.options.max_pending_bytes {
      return Err(IndexProducerCoordinatorErrorV1::SpillRequired {
        context: "maintenance continuation byte limit".to_string(),
        requested_bytes,
        limit_bytes: self.options.max_pending_bytes,
      });
    }
    let mut reservation = self
      .memory
      .reserve(MemoryOwner::Task, requested_bytes, AdmissionClass::Workload)
      .map_err(|error| memory_error(requested_bytes, self.options.max_pending_bytes, error))?;
    let resume_after = clone_string(resume_after, "maintenance continuation")?;
    let retained_bytes = u64::try_from(resume_after.capacity()).map_err(|source| {
      IndexProducerCoordinatorErrorV1::Invariant(format!("maintenance continuation capacity conversion failed: {source}"))
    })?;
    if retained_bytes > requested_bytes {
      reservation
        .grow(retained_bytes - requested_bytes)
        .map_err(|error| memory_error(retained_bytes, self.options.max_pending_bytes, error))?;
      let actual_projected = self
        .pending_bytes
        .checked_sub(old_bytes)
        .and_then(|bytes| bytes.checked_add(retained_bytes))
        .ok_or(IndexProducerCoordinatorErrorV1::AccountingOverflow("maintenance continuation capacity projection"))?;
      if actual_projected > self.options.max_pending_bytes {
        return Err(IndexProducerCoordinatorErrorV1::SpillRequired {
          context: "maintenance continuation capacity limit".to_string(),
          requested_bytes: retained_bytes,
          limit_bytes: self.options.max_pending_bytes,
        });
      }
    }
    Ok(RetainedMaintenanceContinuationV1 { resume_after, retained_bytes, _reservation: reservation })
  }

  fn install_maintenance_continuation(
    &mut self,
    task_index: usize,
    continuation: RetainedMaintenanceContinuationV1,
  ) -> Result<(), IndexProducerCoordinatorErrorV1> {
    let old_bytes = self.tasks[task_index].maintenance_continuation.as_ref().map_or(0, |current| current.retained_bytes);
    let next_task_bytes = self.tasks[task_index]
      .retained_bytes
      .checked_sub(old_bytes)
      .and_then(|bytes| bytes.checked_add(continuation.retained_bytes))
      .ok_or(IndexProducerCoordinatorErrorV1::AccountingOverflow("maintenance task continuation installation"))?;
    let next_pending_bytes = self
      .pending_bytes
      .checked_sub(old_bytes)
      .and_then(|bytes| bytes.checked_add(continuation.retained_bytes))
      .ok_or(IndexProducerCoordinatorErrorV1::AccountingOverflow("maintenance continuation installation"))?;
    if next_pending_bytes > self.options.max_pending_bytes {
      return Err(IndexProducerCoordinatorErrorV1::Invariant(
        "prepared maintenance continuation exceeds the pending byte limit".to_string(),
      ));
    }
    let old = self.tasks[task_index].maintenance_continuation.replace(continuation);
    self.tasks[task_index].retained_bytes = next_task_bytes;
    self.pending_bytes = next_pending_bytes;
    drop(old);
    self.verify_accounting()
  }

  fn validate_task(&self, request: &IndexProducerTaskRequestV1<'_>) -> Result<(), IndexProducerCoordinatorErrorV1> {
    let hash_width = self.hash_algorithm.hash_length();
    if request.operation_id == [0; 16] {
      return Err(IndexProducerCoordinatorErrorV1::InvalidTask("operation identity is all zeroes".to_string()));
    }
    if request.publication_sequence == 0 {
      return Err(IndexProducerCoordinatorErrorV1::InvalidTask("publication sequence must be nonzero".to_string()));
    }
    for (name, hash) in [
      ("namespace root before", request.namespace_root_before),
      ("namespace root after", request.namespace_root_after),
      ("semantic state root", request.semantic_state_root),
    ] {
      if hash.len() != hash_width || hash.iter().all(|byte| *byte == 0) {
        return Err(IndexProducerCoordinatorErrorV1::InvalidTask(format!("{name} must be a nonzero complete database hash")));
      }
    }
    if request.kind.requires_journal() {
      if request.namespace_root_before == request.namespace_root_after {
        return Err(IndexProducerCoordinatorErrorV1::InvalidTask("journal work must cross an exact root transition".to_string()));
      }
      let journal = request
        .journal_head
        .ok_or_else(|| IndexProducerCoordinatorErrorV1::InvalidTask("journal work requires a journal head".to_string()))?;
      if journal.len() != hash_width || journal.iter().all(|byte| *byte == 0) {
        return Err(IndexProducerCoordinatorErrorV1::InvalidTask("journal head must be a nonzero complete database hash".to_string()));
      }
      if request.scope.is_some() {
        return Err(IndexProducerCoordinatorErrorV1::InvalidTask("journal work cannot carry an ad hoc scope".to_string()));
      }
    } else {
      if request.namespace_root_before != request.namespace_root_after {
        return Err(IndexProducerCoordinatorErrorV1::InvalidTask("root-pinned maintenance work must use one exact root".to_string()));
      }
      if request.journal_head.is_some() {
        return Err(IndexProducerCoordinatorErrorV1::InvalidTask("non-journal work cannot carry a journal head".to_string()));
      }
      let scope =
        request.scope.ok_or_else(|| IndexProducerCoordinatorErrorV1::InvalidTask("root-pinned work requires a scope".to_string()))?;
      if scope.is_empty() || !scope.starts_with('/') || scope.len() > INDEX_PRODUCER_SCOPE_BYTES_MAX || normalize_path(scope) != scope {
        return Err(IndexProducerCoordinatorErrorV1::InvalidTask(
          "scope must be a nonempty canonical absolute path within the fixed bound".to_string(),
        ));
      }
    }
    Ok(())
  }

  fn validate_report(&self, report: &IndexProducerReportV1) -> Result<(), IndexProducerCoordinatorErrorV1> {
    if report.outcomes.len() > self.options.max_report_owners as usize {
      return Err(IndexProducerCoordinatorErrorV1::InvalidReport("per-owner outcome count exceeds the task bound".to_string()));
    }
    let hash_width = self.hash_algorithm.hash_length();
    let mut mutation_count = 0u32;
    let mut report_bytes = size_of::<IndexProducerReportV1>() as u64;
    report_bytes = report_bytes
      .checked_add((report.outcomes.len() as u64).saturating_mul(size_of::<IndexProducerOwnerOutcomeV1>() as u64))
      .ok_or(IndexProducerCoordinatorErrorV1::InvalidReport("report byte count overflow".to_string()))?;
    for outcome in &report.outcomes {
      validate_hash(&outcome.owner_id, hash_width, "outcome owner ID")?;
      validate_disposition(&outcome.disposition, hash_width)?;
      if let Some(membership) = outcome.membership {
        if membership.document_ordinal == 0
          || (membership.before.live && membership.before.unindexable)
          || (membership.after.live && membership.after.unindexable)
          || (membership.owner_class == IndexMembershipOwnerClassV1::ScopeCatalog
            && (membership.before.unindexable || membership.after.unindexable))
        {
          return Err(IndexProducerCoordinatorErrorV1::InvalidReport(
            "owner/document membership transition is incomplete or contradictory".to_string(),
          ));
        }
        match outcome.disposition {
          IndexProducerOwnerDispositionV1::Ready if membership.after.unindexable => {
            return Err(IndexProducerCoordinatorErrorV1::InvalidReport(
              "ready owner outcome cannot retain an unindexable after-state".to_string(),
            ));
          }
          IndexProducerOwnerDispositionV1::FrozenUnindexable { .. } if !membership.after.unindexable => {
            return Err(IndexProducerCoordinatorErrorV1::InvalidReport(
              "frozen owner outcome requires an unindexable after-state".to_string(),
            ));
          }
          IndexProducerOwnerDispositionV1::Retryable { .. } | IndexProducerOwnerDispositionV1::Degraded { .. } => {
            return Err(IndexProducerCoordinatorErrorV1::InvalidReport(
              "non-authoritative owner outcome cannot advance membership".to_string(),
            ));
          }
          _ => {}
        }
      }
      if matches!(outcome.disposition, IndexProducerOwnerDispositionV1::Retryable { .. }) && !outcome.mutations.is_empty() {
        return Err(IndexProducerCoordinatorErrorV1::InvalidReport("a retryable owner outcome cannot claim emitted mutations".to_string()));
      }
      report_bytes = report_bytes
        .checked_add(outcome.owner_id.len() as u64)
        .and_then(|bytes| bytes.checked_add((outcome.mutations.len() as u64).saturating_mul(size_of::<IndexProducerMutationV1>() as u64)))
        .and_then(|bytes| bytes.checked_add(disposition_evidence_bytes(&outcome.disposition) as u64))
        .ok_or(IndexProducerCoordinatorErrorV1::InvalidReport("report byte count overflow".to_string()))?;
      for mutation in &outcome.mutations {
        if mutation.owner_id != outcome.owner_id {
          return Err(IndexProducerCoordinatorErrorV1::InvalidReport(
            "an emitted mutation belongs to a different ordered owner than its outcome".to_string(),
          ));
        }
        if mutation.role == OrderedIndexRoleV1::NvtTile {
          return Err(IndexProducerCoordinatorErrorV1::InvalidReport(
            "NVT tiles are publication artifacts rather than ordered producer mutations".to_string(),
          ));
        }
        let decoded = decode_ordered_record(&mutation.encoded_record, self.hash_algorithm, mutation.role)
          .map_err(|source| IndexProducerCoordinatorErrorV1::InvalidReport(format!("malformed ordered mutation record: {source}")))?;
        if mutation.operation == IndexMutationOperationV1::RemoveExisting
          && (mutation.role != OrderedIndexRoleV1::ScopeReverse || decoded.tombstone)
        {
          return Err(IndexProducerCoordinatorErrorV1::InvalidReport(
            "remove-existing is legal only for a live scope-reverse record".to_string(),
          ));
        }
        if let Some(membership) = outcome.membership {
          if decoded.document_ordinal != membership.document_ordinal || mutation.role.owner_class() != membership.owner_class.id() {
            return Err(IndexProducerCoordinatorErrorV1::InvalidReport(
              "mutation owner/document identity disagrees with its membership transition".to_string(),
            ));
          }
        } else if mutation.operation != IndexMutationOperationV1::Upsert {
          return Err(IndexProducerCoordinatorErrorV1::InvalidReport("explicit remove mutation has no membership transition".to_string()));
        }
        mutation_count =
          mutation_count.checked_add(1).ok_or(IndexProducerCoordinatorErrorV1::InvalidReport("mutation count overflow".to_string()))?;
        report_bytes = report_bytes
          .checked_add(mutation.owner_id.len() as u64)
          .and_then(|bytes| bytes.checked_add(mutation.encoded_record.len() as u64))
          .ok_or(IndexProducerCoordinatorErrorV1::InvalidReport("report byte count overflow".to_string()))?;
      }
    }
    if report.outcomes.windows(2).any(|pair| pair[0].owner_id == pair[1].owner_id) {
      return Err(IndexProducerCoordinatorErrorV1::InvalidReport("report contains duplicate per-owner outcomes".to_string()));
    }
    if mutation_count > self.options.max_report_mutations {
      return Err(IndexProducerCoordinatorErrorV1::InvalidReport("mutation count exceeds the task bound".to_string()));
    }
    if report_bytes > self.options.max_report_bytes {
      return Err(IndexProducerCoordinatorErrorV1::InvalidReport("report bytes exceed the task bound".to_string()));
    }
    Ok(())
  }

  fn validate_spill_receipt(
    &self,
    receipt: &IndexProducerSpillReceiptV1,
    expected_operation_id: [u8; 16],
  ) -> Result<(), IndexProducerCoordinatorErrorV1> {
    if receipt.spill_id != expected_operation_id {
      return Err(IndexProducerCoordinatorErrorV1::SpillFailed {
        code: "spill_identity_mismatch",
        context: "spill receipt does not bind the exact producer operation identity".to_string(),
      });
    }
    if receipt.artifact_key.len() != self.hash_algorithm.hash_length() {
      return Err(IndexProducerCoordinatorErrorV1::SpillFailed {
        code: "spill_artifact_width",
        context: "spill receipt artifact key does not match the database hash width".to_string(),
      });
    }
    Ok(())
  }

  fn retain_after_spill_failure(&mut self, task_index: usize, attempt: u16, now_ms: u64) {
    self.tasks[task_index].attempts = attempt;
    self.tasks[task_index].next_attempt_at_ms = now_ms.saturating_add(self.options.max_retry_ms);
    self.active_lease = None;
    self.scheduled_retries = self.scheduled_retries.saturating_add(1);
  }

  fn validate_lease(&self, lease: &IndexProducerLeaseV1) -> Result<(), IndexProducerCoordinatorErrorV1> {
    if lease.coordinator_token != self.coordinator_token {
      return Err(IndexProducerCoordinatorErrorV1::ForeignLease);
    }
    match self.active_lease {
      Some(active) if active.lease_id == lease.lease_id && active.operation_id == lease.operation_id => Ok(()),
      _ => Err(IndexProducerCoordinatorErrorV1::StaleLease),
    }
  }

  fn task_index(&self, operation_id: [u8; 16]) -> Result<usize, IndexProducerCoordinatorErrorV1> {
    self
      .tasks
      .iter()
      .position(|task| task.operation_id == operation_id)
      .ok_or_else(|| IndexProducerCoordinatorErrorV1::Invariant("leased task disappeared".to_string()))
  }

  fn remove_task(&mut self, index: usize) -> Result<(), IndexProducerCoordinatorErrorV1> {
    self.verify_accounting()?;
    let next_pending_bytes = self
      .pending_bytes
      .checked_sub(self.tasks[index].retained_bytes)
      .ok_or(IndexProducerCoordinatorErrorV1::AccountingOverflow("pending task removal"))?;
    let removed = self.tasks.remove(index);
    self.pending_bytes = next_pending_bytes;
    drop(removed);
    self.verify_accounting()
  }

  fn observe_time(&mut self, now_ms: u64) -> Result<(), IndexProducerCoordinatorErrorV1> {
    if now_ms < self.last_observed_ms {
      return Err(IndexProducerCoordinatorErrorV1::ClockRegressed { previous_ms: self.last_observed_ms, received_ms: now_ms });
    }
    self.last_observed_ms = now_ms;
    Ok(())
  }

  fn verify_accounting(&self) -> Result<(), IndexProducerCoordinatorErrorV1> {
    let modeled = self.tasks.iter().try_fold(0u64, |sum, task| sum.checked_add(task.retained_bytes));
    if modeled != Some(self.pending_bytes) {
      return Err(IndexProducerCoordinatorErrorV1::Invariant("pending task byte accounting disagrees with retained tasks".to_string()));
    }
    Ok(())
  }
}

fn disposition_evidence_bytes(disposition: &IndexProducerOwnerDispositionV1) -> usize {
  match disposition {
    IndexProducerOwnerDispositionV1::Ready => 0,
    IndexProducerOwnerDispositionV1::FrozenUnindexable { evidence_hash, .. }
    | IndexProducerOwnerDispositionV1::Retryable { evidence_hash, .. }
    | IndexProducerOwnerDispositionV1::Degraded { evidence_hash, .. } => evidence_hash.as_ref().map_or(0, Vec::len),
  }
}

fn validate_hash(value: &[u8], hash_width: usize, name: &str) -> Result<(), IndexProducerCoordinatorErrorV1> {
  if value.len() != hash_width || value.iter().all(|byte| *byte == 0) {
    return Err(IndexProducerCoordinatorErrorV1::InvalidReport(format!("{name} must be a nonzero complete database hash")));
  }
  Ok(())
}

fn validate_disposition(disposition: &IndexProducerOwnerDispositionV1, hash_width: usize) -> Result<(), IndexProducerCoordinatorErrorV1> {
  let evidence = match disposition {
    IndexProducerOwnerDispositionV1::Ready => return Ok(()),
    IndexProducerOwnerDispositionV1::FrozenUnindexable { stage, reason, evidence_hash } => {
      let valid_value_store = is_valid_document_state_class(DocumentStateOwnerV1::ValueStore, *stage, *reason);
      let valid_field_index = is_valid_document_state_class(DocumentStateOwnerV1::FieldIndex, *stage, *reason);
      if !valid_value_store && !valid_field_index {
        return Err(IndexProducerCoordinatorErrorV1::InvalidReport(
          "frozen-unindexable outcome has an unknown document-state stage/reason pair".to_string(),
        ));
      }
      evidence_hash.as_deref()
    }
    IndexProducerOwnerDispositionV1::Retryable { stable_reason, retry_after_ms, evidence_hash, .. } => {
      validate_operational_reason(*stable_reason)?;
      if *retry_after_ms == 0 {
        return Err(IndexProducerCoordinatorErrorV1::InvalidReport("retryable outcome requires a nonzero retry delay".to_string()));
      }
      evidence_hash.as_deref()
    }
    IndexProducerOwnerDispositionV1::Degraded { stable_reason, evidence_hash, .. } => {
      validate_operational_reason(*stable_reason)?;
      evidence_hash.as_deref()
    }
  };
  if let Some(evidence) = evidence {
    validate_hash(evidence, hash_width, "outcome evidence hash")?;
  }
  Ok(())
}

fn validate_operational_reason(reason: u16) -> Result<(), IndexProducerCoordinatorErrorV1> {
  if !(stable_reason_v1::REQUESTED..=stable_reason_v1::UNKNOWN_PROTECTED_FAMILY).contains(&reason) {
    return Err(IndexProducerCoordinatorErrorV1::InvalidReport("operational outcome has an unknown stable reason".to_string()));
  }
  Ok(())
}

fn require_maintenance_scan_task(kind: IndexProducerTaskKindV1) -> Result<(), IndexProducerCoordinatorErrorV1> {
  match index_producer_service_mode_v1(kind) {
    IndexProducerServiceModeV1::AuthoritativeUpsertScan | IndexProducerServiceModeV1::AuthoritativeRetirementScan => Ok(()),
    IndexProducerServiceModeV1::JournalTransition | IndexProducerServiceModeV1::ArtifactCompaction => Err(
      IndexProducerCoordinatorErrorV1::InvalidMaintenanceDocument("only root-pinned document-scan tasks retain scan progress".to_string()),
    ),
  }
}

fn validate_maintenance_document(
  hash_algorithm: HashAlgorithm,
  task: &RetainedIndexProducerTaskV1,
  revision_hash: &[u8],
  path: &str,
) -> Result<(), IndexProducerCoordinatorErrorV1> {
  let scope = task
    .scope
    .as_deref()
    .ok_or_else(|| IndexProducerCoordinatorErrorV1::Invariant("root-pinned document-scan task lost its scope".to_string()))?;
  let never_cancelled = || false;
  let limits = IndexMaintenanceScanLimitsV1::new(1, 1, INDEX_PRODUCER_SCOPE_BYTES_MAX as u32)
    .map_err(|error| IndexProducerCoordinatorErrorV1::Invariant(error.to_string()))?;
  validate_index_maintenance_scan_request_v1(
    hash_algorithm,
    &IndexMaintenanceScanRequestV1 {
      namespace_root: &task.namespace_root_after,
      scope,
      resume_after: Some(path),
      limits,
      is_cancelled: &never_cancelled,
    },
  )
  .map_err(|error| IndexProducerCoordinatorErrorV1::InvalidMaintenanceDocument(error.to_string()))?;
  if revision_hash.len() != hash_algorithm.hash_length() || revision_hash.iter().all(|byte| *byte == 0) {
    return Err(IndexProducerCoordinatorErrorV1::InvalidMaintenanceDocument(
      "revision hash must be a nonzero complete database hash".to_string(),
    ));
  }
  if task.maintenance_continuation.as_ref().is_some_and(|continuation| path <= continuation.resume_after.as_str()) {
    return Err(IndexProducerCoordinatorErrorV1::InvalidMaintenanceDocument(
      "document path must advance beyond the retained continuation".to_string(),
    ));
  }
  Ok(())
}

fn retained_task_bytes(request: &IndexProducerTaskRequestV1<'_>) -> Result<u64, IndexProducerCoordinatorErrorV1> {
  let payload = request
    .namespace_root_before
    .len()
    .checked_add(request.namespace_root_after.len())
    .and_then(|bytes| bytes.checked_add(request.semantic_state_root.len()))
    .and_then(|bytes| bytes.checked_add(request.journal_head.map_or(0, <[u8]>::len)))
    .and_then(|bytes| bytes.checked_add(request.scope.map_or(0, str::len)))
    .and_then(|bytes| bytes.checked_add(size_of::<RetainedIndexProducerTaskV1>()))
    .ok_or(IndexProducerCoordinatorErrorV1::AccountingOverflow("retained task size"))?;
  Ok(payload as u64)
}

fn journal_work_plan_bytes(
  namespace_root_before_bytes: usize,
  namespace_root_after_bytes: usize,
  semantic_state_root_bytes: usize,
  journal_head_bytes: usize,
) -> Result<u64, IndexProducerCoordinatorErrorV1> {
  let retained = size_of::<IndexProducerJournalWorkPlanV1>()
    .checked_add(namespace_root_before_bytes)
    .and_then(|bytes| bytes.checked_add(namespace_root_after_bytes))
    .and_then(|bytes| bytes.checked_add(semantic_state_root_bytes))
    .and_then(|bytes| bytes.checked_add(journal_head_bytes))
    .ok_or(IndexProducerCoordinatorErrorV1::AccountingOverflow("journal work plan bytes"))?;
  u64::try_from(retained)
    .map_err(|source| IndexProducerCoordinatorErrorV1::Invariant(format!("journal work plan bytes exceed u64: {source}")))
}

fn maintenance_scan_plan_bytes(
  namespace_root_bytes: usize,
  semantic_state_root_bytes: usize,
  scope_bytes: usize,
  continuation_bytes: usize,
) -> Result<u64, IndexProducerCoordinatorErrorV1> {
  let retained = size_of::<IndexProducerMaintenanceScanPlanV1>()
    .checked_add(namespace_root_bytes)
    .and_then(|bytes| bytes.checked_add(semantic_state_root_bytes))
    .and_then(|bytes| bytes.checked_add(scope_bytes))
    .and_then(|bytes| bytes.checked_add(continuation_bytes))
    .ok_or(IndexProducerCoordinatorErrorV1::AccountingOverflow("maintenance scan plan bytes"))?;
  u64::try_from(retained)
    .map_err(|source| IndexProducerCoordinatorErrorV1::Invariant(format!("maintenance scan plan bytes exceed u64: {source}")))
}

fn clone_task(
  request: IndexProducerTaskRequestV1<'_>,
  now_ms: u64,
  retained_bytes: u64,
  reservation: MemoryReservation,
) -> Result<RetainedIndexProducerTaskV1, IndexProducerCoordinatorErrorV1> {
  Ok(RetainedIndexProducerTaskV1 {
    operation_id: request.operation_id,
    kind: request.kind,
    publication_sequence: request.publication_sequence,
    namespace_root_before: clone_bytes(request.namespace_root_before, "namespace root before")?,
    namespace_root_after: clone_bytes(request.namespace_root_after, "namespace root after")?,
    semantic_state_root: clone_bytes(request.semantic_state_root, "semantic state root")?,
    journal_head: request.journal_head.map(|value| clone_bytes(value, "journal head")).transpose()?,
    scope: request.scope.map(|value| clone_string(value, "task scope")).transpose()?,
    attempts: 0,
    next_attempt_at_ms: now_ms,
    maintenance_continuation: None,
    retained_bytes,
    _reservation: reservation,
  })
}

fn clone_lease(
  coordinator_token: u64,
  lease_id: u64,
  task: &RetainedIndexProducerTaskV1,
) -> Result<IndexProducerLeaseV1, IndexProducerCoordinatorErrorV1> {
  Ok(IndexProducerLeaseV1 {
    coordinator_token,
    lease_id,
    operation_id: task.operation_id,
    kind: task.kind,
    publication_sequence: task.publication_sequence,
  })
}

fn clone_bytes(value: &[u8], context: &str) -> Result<Vec<u8>, IndexProducerCoordinatorErrorV1> {
  let mut cloned = Vec::new();
  cloned
    .try_reserve_exact(value.len())
    .map_err(|source| IndexProducerCoordinatorErrorV1::Allocation(format!("cannot reserve {context}: {source}")))?;
  cloned.extend_from_slice(value);
  Ok(cloned)
}

fn clone_string(value: &str, context: &str) -> Result<String, IndexProducerCoordinatorErrorV1> {
  let mut cloned = String::new();
  cloned
    .try_reserve_exact(value.len())
    .map_err(|source| IndexProducerCoordinatorErrorV1::Allocation(format!("cannot reserve {context}: {source}")))?;
  cloned.push_str(value);
  Ok(cloned)
}

fn task_view_from_request<'a>(request: &IndexProducerTaskRequestV1<'a>) -> IndexProducerTaskViewV1<'a> {
  IndexProducerTaskViewV1 {
    operation_id: request.operation_id,
    kind: request.kind,
    publication_sequence: request.publication_sequence,
    namespace_root_before: request.namespace_root_before,
    namespace_root_after: request.namespace_root_after,
    semantic_state_root: request.semantic_state_root,
    journal_head: request.journal_head,
    scope: request.scope,
  }
}

fn retry_delay(base_ms: u64, maximum_ms: u64, attempt: u16) -> u64 {
  let shift = u32::from(attempt.saturating_sub(1)).min(63);
  base_ms.saturating_mul(1u64 << shift).min(maximum_ms)
}

fn memory_error(requested_bytes: u64, limit_bytes: u64, error: MemoryCoordinatorError) -> IndexProducerCoordinatorErrorV1 {
  match error {
    MemoryCoordinatorError::HardLimitExceeded { .. }
    | MemoryCoordinatorError::SoftPressureDeferred { .. }
    | MemoryCoordinatorError::EmergencyReserveExceeded { .. }
    | MemoryCoordinatorError::PolicyUnavailable => {
      IndexProducerCoordinatorErrorV1::SpillRequired { context: error.to_string(), requested_bytes, limit_bytes }
    }
    _ => IndexProducerCoordinatorErrorV1::MemoryAuthority(error.to_string()),
  }
}

fn spill_error(error: IndexProducerSpillErrorV1) -> IndexProducerCoordinatorErrorV1 {
  IndexProducerCoordinatorErrorV1::SpillFailed { code: error.code, context: error.context }
}
