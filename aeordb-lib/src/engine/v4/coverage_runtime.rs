use std::collections::VecDeque;
use std::mem::size_of;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use crate::engine::HashAlgorithm;
use crate::engine::namespace_mutation::{NamespaceMutationAcknowledgement, NamespaceMutationKind, NamespaceMutationSourceIdentity};

const MAX_CONTROL_IDENTITIES: usize = 4_096;
pub const DEFAULT_SOFT_MUTATION_MAX_NOTICES: usize = 4_096;
pub const DEFAULT_SOFT_MUTATION_MAX_RETAINED_BYTES: usize = 8 * 1_024 * 1_024;
pub const DEFAULT_SOFT_MUTATION_MAX_NOTICE_BYTES: usize = 256 * 1_024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageControlIdentityV1 {
  pub domain: u16,
  pub identity: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageAuthorityV1 {
  pub source_namespace_root: Vec<u8>,
  pub control_identities: Vec<CoverageControlIdentityV1>,
}

impl CoverageAuthorityV1 {
  pub fn new(
    hash_algorithm: HashAlgorithm,
    source_namespace_root: Vec<u8>,
    control_identities: Vec<CoverageControlIdentityV1>,
  ) -> Result<Self, CoverageRuntimeErrorV1> {
    let hash_width = hash_algorithm.hash_length();
    if source_namespace_root.len() != hash_width {
      return Err(CoverageRuntimeErrorV1::InvalidNamespaceRootWidth { expected: hash_width, actual: source_namespace_root.len() });
    }
    if source_namespace_root.iter().all(|byte| *byte == 0) {
      return Err(CoverageRuntimeErrorV1::ZeroNamespaceRoot);
    }
    if control_identities.len() > MAX_CONTROL_IDENTITIES {
      return Err(CoverageRuntimeErrorV1::TooManyControlIdentities { maximum: MAX_CONTROL_IDENTITIES, actual: control_identities.len() });
    }
    let mut previous_domain = None;
    for control in &control_identities {
      if control.domain == 0 || previous_domain.is_some_and(|previous| previous >= control.domain) {
        return Err(CoverageRuntimeErrorV1::ControlIdentitiesNotStrictlyOrdered);
      }
      if control.identity.len() != hash_width {
        return Err(CoverageRuntimeErrorV1::InvalidControlIdentityWidth {
          domain: control.domain,
          expected: hash_width,
          actual: control.identity.len(),
        });
      }
      if control.identity.iter().all(|byte| *byte == 0) {
        return Err(CoverageRuntimeErrorV1::ZeroControlIdentity { domain: control.domain });
      }
      previous_domain = Some(control.domain);
    }
    Ok(Self { source_namespace_root, control_identities })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageBoundaryV1 {
  pub authority: CoverageAuthorityV1,
  pub publication_sequence: u64,
}

impl CoverageBoundaryV1 {
  pub fn new(authority: CoverageAuthorityV1, publication_sequence: u64) -> Result<Self, CoverageRuntimeErrorV1> {
    if publication_sequence == 0 {
      return Err(CoverageRuntimeErrorV1::ZeroPublicationSequence);
    }
    Ok(Self { authority, publication_sequence })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageMutationV1 {
  pub mutation_id: [u8; 16],
  pub publication_sequence: u64,
  pub before: CoverageAuthorityV1,
  pub after: CoverageAuthorityV1,
}

impl CoverageMutationV1 {
  pub fn new(
    mutation_id: [u8; 16],
    publication_sequence: u64,
    before: CoverageAuthorityV1,
    after: CoverageAuthorityV1,
  ) -> Result<Self, CoverageRuntimeErrorV1> {
    if mutation_id.iter().all(|byte| *byte == 0) {
      return Err(CoverageRuntimeErrorV1::ZeroMutationId);
    }
    if publication_sequence == 0 {
      return Err(CoverageRuntimeErrorV1::ZeroPublicationSequence);
    }
    if before == after {
      return Err(CoverageRuntimeErrorV1::MutationDoesNotChangeAuthority);
    }
    Ok(Self { mutation_id, publication_sequence, before, after })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoverageGapReasonV1 {
  SoftStateLost,
  AuthorityDiscontinuity,
  NonMonotonicPublication,
  ConflictingDuplicate,
  AlreadyLatched,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoverageObservationV1 {
  Applied(CoverageBoundaryV1),
  Duplicate,
  ReconciliationRequired(CoverageGapReasonV1),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoverageReconciliationV1 {
  AlreadyExact { covered: CoverageBoundaryV1, authority_sequence: u64 },
  BoundedDiffRequired { from: CoverageBoundaryV1, to: CoverageAuthorityV1, authority_sequence: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageTrackerV1 {
  coverage_epoch_id: [u8; 16],
  covered: CoverageBoundaryV1,
  last_mutation: Option<CoverageMutationV1>,
  reconciliation_reason: Option<CoverageGapReasonV1>,
  lost_through_sequence: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageReconciliationProofV1 {
  coverage_epoch_id: [u8; 16],
  boundary: CoverageBoundaryV1,
}

impl CoverageReconciliationProofV1 {
  pub fn coverage_epoch_id(&self) -> [u8; 16] {
    self.coverage_epoch_id
  }

  pub fn boundary(&self) -> &CoverageBoundaryV1 {
    &self.boundary
  }
}

impl CoverageTrackerV1 {
  pub fn new(coverage_epoch_id: [u8; 16], covered: CoverageBoundaryV1) -> Result<Self, CoverageRuntimeErrorV1> {
    if coverage_epoch_id.iter().all(|byte| *byte == 0) {
      return Err(CoverageRuntimeErrorV1::ZeroCoverageEpoch);
    }
    Ok(Self { coverage_epoch_id, covered, last_mutation: None, reconciliation_reason: None, lost_through_sequence: None })
  }

  pub fn coverage_epoch_id(&self) -> [u8; 16] {
    self.coverage_epoch_id
  }

  pub fn covered(&self) -> &CoverageBoundaryV1 {
    &self.covered
  }

  pub fn requires_reconciliation(&self) -> bool {
    self.reconciliation_reason.is_some()
  }

  pub fn lost_through_sequence(&self) -> Option<u64> {
    self.lost_through_sequence
  }

  pub fn observe(&mut self, mutation: CoverageMutationV1) -> CoverageObservationV1 {
    if self.reconciliation_reason.is_some() {
      return CoverageObservationV1::ReconciliationRequired(CoverageGapReasonV1::AlreadyLatched);
    }
    if self.last_mutation.as_ref().is_some_and(|previous| previous.mutation_id == mutation.mutation_id) {
      if self.last_mutation.as_ref() == Some(&mutation) {
        return CoverageObservationV1::Duplicate;
      }
      return self.latch(CoverageGapReasonV1::ConflictingDuplicate, mutation.publication_sequence);
    }
    if mutation.publication_sequence <= self.covered.publication_sequence {
      return self.latch(CoverageGapReasonV1::NonMonotonicPublication, mutation.publication_sequence);
    }
    if mutation.before != self.covered.authority {
      return self.latch(CoverageGapReasonV1::AuthorityDiscontinuity, mutation.publication_sequence);
    }

    self.covered = CoverageBoundaryV1 { authority: mutation.after.clone(), publication_sequence: mutation.publication_sequence };
    self.last_mutation = Some(mutation);
    CoverageObservationV1::Applied(self.covered.clone())
  }

  pub fn mark_soft_state_lost(&mut self, observed_sequence: u64) {
    self.reconciliation_reason.get_or_insert(CoverageGapReasonV1::SoftStateLost);
    self.lost_through_sequence = Some(self.lost_through_sequence.map_or(observed_sequence, |current| current.max(observed_sequence)));
  }

  pub fn reconcile_against(
    &self,
    selected_authority: &CoverageAuthorityV1,
    authority_sequence: u64,
  ) -> Result<CoverageReconciliationV1, CoverageRuntimeErrorV1> {
    if authority_sequence < self.covered.publication_sequence {
      return Err(CoverageRuntimeErrorV1::AuthoritySequenceRegressed {
        covered: self.covered.publication_sequence,
        authority: authority_sequence,
      });
    }
    if selected_authority == &self.covered.authority {
      return Ok(CoverageReconciliationV1::AlreadyExact { covered: self.covered.clone(), authority_sequence });
    }
    Ok(CoverageReconciliationV1::BoundedDiffRequired { from: self.covered.clone(), to: selected_authority.clone(), authority_sequence })
  }

  pub fn accept_reconciled(&mut self, boundary: CoverageBoundaryV1) -> Result<CoverageReconciliationProofV1, CoverageRuntimeErrorV1> {
    if boundary.publication_sequence < self.covered.publication_sequence {
      return Err(CoverageRuntimeErrorV1::AuthoritySequenceRegressed {
        covered: self.covered.publication_sequence,
        authority: boundary.publication_sequence,
      });
    }
    self.covered = boundary.clone();
    self.last_mutation = None;
    self.reconciliation_reason = None;
    self.lost_through_sequence = None;
    Ok(CoverageReconciliationProofV1 { coverage_epoch_id: self.coverage_epoch_id, boundary })
  }

  fn latch(&mut self, reason: CoverageGapReasonV1, observed_sequence: u64) -> CoverageObservationV1 {
    self.reconciliation_reason = Some(reason);
    self.lost_through_sequence = Some(self.lost_through_sequence.map_or(observed_sequence, |current| current.max(observed_sequence)));
    CoverageObservationV1::ReconciliationRequired(reason)
  }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CoverageRuntimeErrorV1 {
  #[error("coverage namespace root is zero")]
  ZeroNamespaceRoot,
  #[error("coverage namespace root has width {actual}, expected {expected}")]
  InvalidNamespaceRootWidth { expected: usize, actual: usize },
  #[error("coverage contains {actual} control identities, maximum {maximum}")]
  TooManyControlIdentities { maximum: usize, actual: usize },
  #[error("coverage control identities are not strictly ordered by nonzero domain")]
  ControlIdentitiesNotStrictlyOrdered,
  #[error("coverage control identity for domain {domain} is zero")]
  ZeroControlIdentity { domain: u16 },
  #[error("coverage control identity for domain {domain} has width {actual}, expected {expected}")]
  InvalidControlIdentityWidth { domain: u16, expected: usize, actual: usize },
  #[error("coverage publication sequence is zero")]
  ZeroPublicationSequence,
  #[error("coverage epoch is zero")]
  ZeroCoverageEpoch,
  #[error("coverage mutation ID is zero")]
  ZeroMutationId,
  #[error("coverage mutation does not change namespace or control authority")]
  MutationDoesNotChangeAuthority,
  #[error("authority sequence {authority} precedes covered sequence {covered}")]
  AuthoritySequenceRegressed { covered: u64, authority: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SoftMutationHubOptionsV1 {
  pub maximum_notices: usize,
  pub maximum_retained_bytes: usize,
  pub maximum_notice_bytes: usize,
}

impl SoftMutationHubOptionsV1 {
  pub fn new(maximum_notices: usize, maximum_retained_bytes: usize, maximum_notice_bytes: usize) -> Result<Self, SoftMutationHubErrorV1> {
    if maximum_notices == 0 || maximum_retained_bytes == 0 || maximum_notice_bytes == 0 {
      return Err(SoftMutationHubErrorV1::InvalidOptions("soft mutation limits must be nonzero"));
    }
    if maximum_notice_bytes > maximum_retained_bytes {
      return Err(SoftMutationHubErrorV1::InvalidOptions("soft mutation notice limit cannot exceed the total retained-byte limit"));
    }
    Ok(Self { maximum_notices, maximum_retained_bytes, maximum_notice_bytes })
  }

  pub fn engine_default() -> Self {
    Self {
      maximum_notices: DEFAULT_SOFT_MUTATION_MAX_NOTICES,
      maximum_retained_bytes: DEFAULT_SOFT_MUTATION_MAX_RETAINED_BYTES,
      maximum_notice_bytes: DEFAULT_SOFT_MUTATION_MAX_NOTICE_BYTES,
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SoftMutationLossReasonV1 {
  InvalidNotice,
  QueueContended,
  QueueFull,
  NoticeTooLarge,
  AllocationFailed,
  QueueUnavailable,
}

impl SoftMutationLossReasonV1 {
  const ALL: [Self; 6] =
    [Self::InvalidNotice, Self::QueueContended, Self::QueueFull, Self::NoticeTooLarge, Self::AllocationFailed, Self::QueueUnavailable];

  const fn bit(self) -> u8 {
    match self {
      Self::InvalidNotice => 1 << 0,
      Self::QueueContended => 1 << 1,
      Self::QueueFull => 1 << 2,
      Self::NoticeTooLarge => 1 << 3,
      Self::AllocationFailed => 1 << 4,
      Self::QueueUnavailable => 1 << 5,
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SoftMutationAdmissionV1 {
  Accepted,
  ReconciliationRequired(SoftMutationLossReasonV1),
  ReconciliationAlreadyRequired,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoftMutationNoticeV1 {
  pub operation_id: [u8; 16],
  pub kind: NamespaceMutationKind,
  pub publication_sequence: u64,
  pub committed_at_ms: u64,
  pub previous_namespace_root: Vec<u8>,
  pub namespace_root: Vec<u8>,
  pub source_identities: Vec<NamespaceMutationSourceIdentity>,
  retained_bytes: usize,
}

impl SoftMutationNoticeV1 {
  pub fn retained_bytes(&self) -> usize {
    self.retained_bytes
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoftMutationDrainV1 {
  pub notices: Vec<SoftMutationNoticeV1>,
  pub retained_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoftMutationHubSnapshotV1 {
  pub queued_notices: usize,
  pub retained_bytes: usize,
  pub maximum_notices: usize,
  pub maximum_retained_bytes: usize,
  pub maximum_notice_bytes: usize,
  pub latest_queued_publication_sequence: Option<u64>,
  pub reconciliation_required: bool,
  pub lost_through_sequence: Option<u64>,
  pub loss_reasons: Vec<SoftMutationLossReasonV1>,
  pub dropped_notices: u64,
  pub loss_epoch: u64,
  pub reconciled_loss_epoch: u64,
  pub losses_in_flight: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SoftMutationReconciliationTokenV1 {
  loss_epoch: u64,
  lost_through_sequence: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SoftMutationReconciliationClearV1 {
  Cleared,
  AlreadyExact,
  Stale,
  BoundaryBehind,
  QueueUnavailable,
}

#[derive(Debug)]
struct SoftMutationQueueV1 {
  notices: VecDeque<SoftMutationNoticeV1>,
  retained_bytes: usize,
}

#[derive(Debug)]
pub struct SoftMutationHubV1 {
  options: SoftMutationHubOptionsV1,
  queue: Mutex<SoftMutationQueueV1>,
  lost_through_sequence: AtomicU64,
  loss_reason_bits: AtomicU8,
  dropped_notices: AtomicU64,
  loss_epoch: AtomicU64,
  reconciled_loss_epoch: AtomicU64,
  losses_in_flight: AtomicU64,
}

impl SoftMutationHubV1 {
  pub fn new(options: SoftMutationHubOptionsV1) -> Result<Self, SoftMutationHubErrorV1> {
    SoftMutationHubOptionsV1::new(options.maximum_notices, options.maximum_retained_bytes, options.maximum_notice_bytes)?;
    let mut notices = VecDeque::new();
    notices.try_reserve_exact(options.maximum_notices).map_err(|error| SoftMutationHubErrorV1::Allocation(error.to_string()))?;
    Ok(Self {
      options,
      queue: Mutex::new(SoftMutationQueueV1 { notices, retained_bytes: 0 }),
      lost_through_sequence: AtomicU64::new(0),
      loss_reason_bits: AtomicU8::new(0),
      dropped_notices: AtomicU64::new(0),
      loss_epoch: AtomicU64::new(0),
      reconciled_loss_epoch: AtomicU64::new(0),
      losses_in_flight: AtomicU64::new(0),
    })
  }

  pub fn offer_acknowledgement(&self, acknowledgement: &NamespaceMutationAcknowledgement) -> SoftMutationAdmissionV1 {
    if self.reconciliation_required() {
      self.record_loss(acknowledgement.publication_sequence, None);
      return SoftMutationAdmissionV1::ReconciliationAlreadyRequired;
    }
    if acknowledgement.publication_sequence == 0 {
      return self.latch_loss(0, SoftMutationLossReasonV1::InvalidNotice);
    }

    let retained_bytes = match acknowledgement_retained_bytes(acknowledgement) {
      Some(bytes) if bytes <= self.options.maximum_notice_bytes => bytes,
      _ => return self.latch_loss(acknowledgement.publication_sequence, SoftMutationLossReasonV1::NoticeTooLarge),
    };
    let mut queue = match self.queue.try_lock() {
      Ok(queue) => queue,
      Err(std::sync::TryLockError::WouldBlock) => {
        return self.latch_loss(acknowledgement.publication_sequence, SoftMutationLossReasonV1::QueueContended);
      }
      Err(std::sync::TryLockError::Poisoned(_)) => {
        return self.latch_loss(acknowledgement.publication_sequence, SoftMutationLossReasonV1::QueueUnavailable);
      }
    };
    let next_bytes = match queue.retained_bytes.checked_add(retained_bytes) {
      Some(bytes) => bytes,
      None => return self.latch_loss(acknowledgement.publication_sequence, SoftMutationLossReasonV1::QueueFull),
    };
    if queue.notices.len() >= self.options.maximum_notices || next_bytes > self.options.maximum_retained_bytes {
      return self.latch_loss(acknowledgement.publication_sequence, SoftMutationLossReasonV1::QueueFull);
    }
    let notice = match clone_notice(acknowledgement, retained_bytes) {
      Ok(notice) => notice,
      Err(error) => {
        drop(error);
        return self.latch_loss(acknowledgement.publication_sequence, SoftMutationLossReasonV1::AllocationFailed);
      }
    };
    queue.notices.push_back(notice);
    queue.retained_bytes = next_bytes;
    SoftMutationAdmissionV1::Accepted
  }

  /// Mark an acknowledgement as lost without consulting the bounded queue.
  ///
  /// This lock-free fallback is reserved for failures around the normal offer
  /// boundary itself. Once the hard namespace mutation is acknowledged, an
  /// unexpected soft-handoff panic must still make exact coverage impossible
  /// to claim.
  pub(crate) fn force_reconciliation_required(
    &self,
    publication_sequence: u64,
    reason: SoftMutationLossReasonV1,
  ) -> SoftMutationAdmissionV1 {
    self.latch_loss(publication_sequence, reason)
  }

  pub fn snapshot(&self) -> Result<SoftMutationHubSnapshotV1, SoftMutationHubErrorV1> {
    let queue = self.queue.lock().map_err(|_| SoftMutationHubErrorV1::QueueUnavailable)?;
    let lost = self.lost_through_sequence.load(Ordering::Acquire);
    let reason_bits = self.loss_reason_bits.load(Ordering::Acquire);
    let loss_epoch = self.loss_epoch.load(Ordering::Acquire);
    let reconciled_loss_epoch = self.reconciled_loss_epoch.load(Ordering::Acquire);
    let losses_in_flight = self.losses_in_flight.load(Ordering::Acquire);
    Ok(SoftMutationHubSnapshotV1 {
      queued_notices: queue.notices.len(),
      retained_bytes: queue.retained_bytes,
      maximum_notices: self.options.maximum_notices,
      maximum_retained_bytes: self.options.maximum_retained_bytes,
      maximum_notice_bytes: self.options.maximum_notice_bytes,
      latest_queued_publication_sequence: queue.notices.back().map(|notice| notice.publication_sequence),
      reconciliation_required: losses_in_flight != 0 || loss_epoch != reconciled_loss_epoch,
      lost_through_sequence: (lost != 0).then_some(lost),
      loss_reasons: SoftMutationLossReasonV1::ALL.into_iter().filter(|reason| reason_bits & reason.bit() != 0).collect(),
      dropped_notices: self.dropped_notices.load(Ordering::Relaxed),
      loss_epoch,
      reconciled_loss_epoch,
      losses_in_flight,
    })
  }

  pub fn reconciliation_token(&self) -> SoftMutationReconciliationTokenV1 {
    let loss_epoch = self.loss_epoch.load(Ordering::Acquire);
    let lost = self.lost_through_sequence.load(Ordering::Acquire);
    SoftMutationReconciliationTokenV1 { loss_epoch, lost_through_sequence: (lost != 0).then_some(lost) }
  }

  pub fn try_clear_reconciliation(
    &self,
    token: SoftMutationReconciliationTokenV1,
    proof: &CoverageReconciliationProofV1,
  ) -> SoftMutationReconciliationClearV1 {
    let reconciled_through_sequence = proof.boundary.publication_sequence;
    let current_epoch = self.loss_epoch.load(Ordering::Acquire);
    let reconciled_epoch = self.reconciled_loss_epoch.load(Ordering::Acquire);
    if current_epoch == reconciled_epoch {
      return SoftMutationReconciliationClearV1::AlreadyExact;
    }
    if self.losses_in_flight.load(Ordering::Acquire) != 0 || token.loss_epoch == u64::MAX || token.loss_epoch != current_epoch {
      return SoftMutationReconciliationClearV1::Stale;
    }
    if token.lost_through_sequence.is_some_and(|lost| lost > reconciled_through_sequence) {
      return SoftMutationReconciliationClearV1::BoundaryBehind;
    }

    let mut queue = match self.queue.lock() {
      Ok(queue) => queue,
      Err(error) => {
        tracing::error!(?error, "Soft mutation queue was poisoned while clearing reconciled loss");
        return SoftMutationReconciliationClearV1::QueueUnavailable;
      }
    };
    if queue.notices.iter().any(|notice| notice.publication_sequence > reconciled_through_sequence) {
      return SoftMutationReconciliationClearV1::BoundaryBehind;
    }
    if self.losses_in_flight.load(Ordering::Acquire) != 0 || self.loss_epoch.load(Ordering::Acquire) != token.loss_epoch {
      return SoftMutationReconciliationClearV1::Stale;
    }
    queue.notices.clear();
    queue.retained_bytes = 0;
    self.reconciled_loss_epoch.store(token.loss_epoch, Ordering::Release);
    if self.losses_in_flight.load(Ordering::Acquire) == 0 && self.loss_epoch.load(Ordering::Acquire) == token.loss_epoch {
      SoftMutationReconciliationClearV1::Cleared
    } else {
      SoftMutationReconciliationClearV1::Stale
    }
  }

  pub fn try_drain(&self, maximum_notices: usize, maximum_bytes: usize) -> Result<SoftMutationDrainV1, SoftMutationHubErrorV1> {
    if maximum_notices == 0 || maximum_bytes == 0 {
      return Err(SoftMutationHubErrorV1::InvalidOptions("soft mutation drain limits must be nonzero"));
    }
    let mut notices = Vec::new();
    notices
      .try_reserve_exact(maximum_notices.min(self.options.maximum_notices))
      .map_err(|error| SoftMutationHubErrorV1::Allocation(error.to_string()))?;
    let mut queue = self.queue.try_lock().map_err(|_| SoftMutationHubErrorV1::QueueUnavailable)?;
    let mut count = 0usize;
    let mut retained_bytes = 0usize;
    for notice in &queue.notices {
      if count == maximum_notices {
        break;
      }
      let Some(next_bytes) = retained_bytes.checked_add(notice.retained_bytes) else {
        return Err(SoftMutationHubErrorV1::ArithmeticOverflow);
      };
      if next_bytes > maximum_bytes {
        break;
      }
      retained_bytes = next_bytes;
      count += 1;
    }
    if count == 0 {
      if let Some(first) = queue.notices.front() {
        return Err(SoftMutationHubErrorV1::DrainLimitTooSmall { required: first.retained_bytes, maximum: maximum_bytes });
      }
    }
    let remaining_bytes = queue.retained_bytes.checked_sub(retained_bytes).ok_or(SoftMutationHubErrorV1::ArithmeticOverflow)?;
    for _ in 0..count {
      notices.push(queue.notices.pop_front().ok_or(SoftMutationHubErrorV1::QueueUnavailable)?);
    }
    queue.retained_bytes = remaining_bytes;
    Ok(SoftMutationDrainV1 { notices, retained_bytes })
  }

  fn latch_loss(&self, publication_sequence: u64, reason: SoftMutationLossReasonV1) -> SoftMutationAdmissionV1 {
    self.record_loss(publication_sequence, Some(reason));
    SoftMutationAdmissionV1::ReconciliationRequired(reason)
  }

  fn reconciliation_required(&self) -> bool {
    self.losses_in_flight.load(Ordering::Acquire) != 0
      || self.loss_epoch.load(Ordering::Acquire) != self.reconciled_loss_epoch.load(Ordering::Acquire)
  }

  fn record_loss(&self, publication_sequence: u64, reason: Option<SoftMutationLossReasonV1>) {
    self.losses_in_flight.fetch_add(1, Ordering::AcqRel);
    self.lost_through_sequence.fetch_max(publication_sequence, Ordering::AcqRel);
    if let Some(reason) = reason {
      self.loss_reason_bits.fetch_or(reason.bit(), Ordering::AcqRel);
    }
    let mut current = self.dropped_notices.load(Ordering::Acquire);
    loop {
      match self.dropped_notices.compare_exchange_weak(current, current.saturating_add(1), Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => break,
        Err(observed) => current = observed,
      }
    }
    let mut epoch = self.loss_epoch.load(Ordering::Acquire);
    while epoch != u64::MAX {
      match self.loss_epoch.compare_exchange_weak(epoch, epoch + 1, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => break,
        Err(observed) => epoch = observed,
      }
    }
    self.losses_in_flight.fetch_sub(1, Ordering::AcqRel);
  }

  #[cfg(test)]
  pub(crate) fn lock_queue_for_test(&self) -> Result<SoftMutationQueueTestGuardV1<'_>, SoftMutationHubErrorV1> {
    self.queue.lock().map(SoftMutationQueueTestGuardV1).map_err(|_| SoftMutationHubErrorV1::QueueUnavailable)
  }
}

#[cfg(test)]
pub(crate) struct SoftMutationQueueTestGuardV1<'a>(#[allow(dead_code)] std::sync::MutexGuard<'a, SoftMutationQueueV1>);

fn acknowledgement_retained_bytes(acknowledgement: &NamespaceMutationAcknowledgement) -> Option<usize> {
  let mut bytes = size_of::<SoftMutationNoticeV1>()
    .checked_add(acknowledgement.previous_root_hash.len())?
    .checked_add(acknowledgement.root_hash.len())?
    .checked_add(acknowledgement.source_identities.len().checked_mul(size_of::<NamespaceMutationSourceIdentity>())?)?;
  for source in &acknowledgement.source_identities {
    bytes = bytes.checked_add(source.path.len())?;
    bytes = bytes.checked_add(source.previous_identity.as_ref().map_or(0, Vec::len))?;
    bytes = bytes.checked_add(source.new_identity.as_ref().map_or(0, Vec::len))?;
  }
  Some(bytes)
}

fn clone_notice(
  acknowledgement: &NamespaceMutationAcknowledgement,
  retained_bytes: usize,
) -> Result<SoftMutationNoticeV1, SoftMutationHubErrorV1> {
  let mut source_identities = Vec::new();
  source_identities
    .try_reserve_exact(acknowledgement.source_identities.len())
    .map_err(|error| SoftMutationHubErrorV1::Allocation(error.to_string()))?;
  for source in &acknowledgement.source_identities {
    let mut path = String::new();
    path.try_reserve_exact(source.path.len()).map_err(|error| SoftMutationHubErrorV1::Allocation(error.to_string()))?;
    path.push_str(&source.path);
    source_identities.push(NamespaceMutationSourceIdentity {
      path,
      entry_type: source.entry_type,
      previous_identity: clone_optional_bytes(source.previous_identity.as_deref())?,
      new_identity: clone_optional_bytes(source.new_identity.as_deref())?,
    });
  }
  Ok(SoftMutationNoticeV1 {
    operation_id: *acknowledgement.operation_id.as_bytes(),
    kind: acknowledgement.kind,
    publication_sequence: acknowledgement.publication_sequence,
    committed_at_ms: chrono::Utc::now().timestamp_millis().max(1) as u64,
    previous_namespace_root: clone_bytes(&acknowledgement.previous_root_hash)?,
    namespace_root: clone_bytes(&acknowledgement.root_hash)?,
    source_identities,
    retained_bytes,
  })
}

fn clone_optional_bytes(value: Option<&[u8]>) -> Result<Option<Vec<u8>>, SoftMutationHubErrorV1> {
  value.map(clone_bytes).transpose()
}

fn clone_bytes(value: &[u8]) -> Result<Vec<u8>, SoftMutationHubErrorV1> {
  let mut cloned = Vec::new();
  cloned.try_reserve_exact(value.len()).map_err(|error| SoftMutationHubErrorV1::Allocation(error.to_string()))?;
  cloned.extend_from_slice(value);
  Ok(cloned)
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SoftMutationHubErrorV1 {
  #[error("invalid soft mutation hub options: {0}")]
  InvalidOptions(&'static str),
  #[error("soft mutation hub allocation failed: {0}")]
  Allocation(String),
  #[error("soft mutation hub queue is unavailable")]
  QueueUnavailable,
  #[error("soft mutation hub accounting overflowed")]
  ArithmeticOverflow,
  #[error("soft mutation drain byte limit {maximum} is smaller than the first queued notice ({required} bytes)")]
  DrainLimitTooSmall { required: usize, maximum: usize },
}
