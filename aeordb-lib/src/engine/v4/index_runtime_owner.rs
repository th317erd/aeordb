//! Single fail-closed owner for the v4 shadow index runtime.
//!
//! This owner composes the bounded soft handoff, producer queue, parser worker,
//! and ordered mutation memtable. Persistent storage remains injected by later
//! activation slices; no query-visible index generation is selected here.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::mem::size_of;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use thiserror::Error;

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};
use crate::engine::namespace_mutation::NamespaceMutationAcknowledgement;

use super::coverage_runtime::{
  SoftMutationAdmissionV1, SoftMutationHubErrorV1, SoftMutationHubOptionsV1, SoftMutationHubSnapshotV1, SoftMutationHubV1,
  SoftMutationLossReasonV1,
};
use super::index_coordinator::{
  FrozenIndexBatchV1, IndexCoordinatorLifecycleV1, IndexCoordinatorOptionsV1, IndexCoordinatorSnapshotV1, IndexCoordinatorV1,
  IndexFlushReasonV1,
};
use super::index_producer_admission::{
  IndexProducerJournalAdmissionErrorV1, IndexProducerJournalAdmissionSummaryV1, admit_mutation_journal_tasks,
};
use super::index_producer_collector::{
  IndexParserExecutorV1, IndexProducerCollectorErrorV1, IndexProducerCollectorOptionsV1, IndexProducerCollectorV1,
};
use super::index_producer_coordinator::{
  IndexProducerAdmissionV1, IndexProducerCoordinatorOptionsV1, IndexProducerCoordinatorSnapshotV1, IndexProducerCoordinatorV1,
  IndexProducerSpillStoreV1, IndexProducerTaskRequestV1,
};
use super::index_producer_executor::IndexProducerExecutorV1;
use super::index_producer_source::{IndexFileRevisionSourceV1, IndexSemanticScopeLimitsV1, IndexSemanticScopeSourceV1};
use super::index_producer_worker::{
  IndexProducerMutationWorkRequestV1, IndexProducerMutationWorkerV1, IndexProducerWorkerErrorV1, IndexProducerWorkerOutcomeV1,
};
use super::index_source::PluginMapperExecutorV1;
use super::index_task::MutationJournalV1;

const MAX_DEGRADED_CONTEXT_BYTES: usize = 16 * 1024;
const MAX_STABLE_CODE_BYTES: usize = 128;

#[derive(Debug, Clone, Copy)]
pub struct IndexRuntimeOwnerOptionsV1 {
  pub soft_hub: SoftMutationHubOptionsV1,
  pub producer: IndexProducerCoordinatorOptionsV1,
  pub mutations: IndexCoordinatorOptionsV1,
  pub collector: IndexProducerCollectorOptionsV1,
  pub semantic: IndexSemanticScopeLimitsV1,
  pub source_retry_after_ms: u64,
  pub publication_retry_after_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexRuntimeLifecycleV1 {
  Recovering,
  Running,
  Degraded,
  Draining,
  Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRuntimeDegradedStateV1 {
  pub code: &'static str,
  pub context: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexRuntimeRecoveryDecisionV1 {
  Ready { recovered_scopes: u32, highest_checkpoint_sequence: u64 },
  ReconciliationRequired { code: &'static str, context: String },
  Canceled,
}

pub struct IndexRuntimeMutationWorkRequestV1<'request, 'journal> {
  pub journal: &'request MutationJournalV1<'journal>,
  pub revision_source: &'request dyn IndexFileRevisionSourceV1,
  pub semantic_source: &'request dyn IndexSemanticScopeSourceV1,
  pub parser: &'request dyn IndexParserExecutorV1,
  pub mapper: Option<&'request dyn PluginMapperExecutorV1>,
  pub now_ms: u64,
  pub is_cancelled: &'request dyn Fn() -> bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum IndexRuntimeWorkOutcomeV1 {
  Idle,
  Deferred { retry_at_ms: u64 },
  Completed(IndexProducerWorkerOutcomeV1),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexRuntimePublicationErrorClassV1 {
  RetryableBeforeSelection,
  CancelledBeforeSelection,
  CommitUnknown,
  Corrupt,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("index runtime publication failed ({code}, {class:?}): {context}")]
pub struct IndexRuntimePublicationErrorV1 {
  class: IndexRuntimePublicationErrorClassV1,
  code: &'static str,
  context: String,
}

impl IndexRuntimePublicationErrorV1 {
  pub fn new(class: IndexRuntimePublicationErrorClassV1, code: &'static str, context: impl Into<String>) -> Self {
    Self { class, code, context: context.into() }
  }

  pub const fn class(&self) -> IndexRuntimePublicationErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRuntimePublicationReceiptV1 {
  pub batch_id: u64,
  pub attempt_id: u64,
  pub published_records: u64,
  pub publication_bytes: u64,
  pub checkpoint_sequence: u64,
}

pub trait IndexRuntimeBatchPublisherV1 {
  /// Publish one exact frozen batch.
  ///
  /// A retryable/cancelled error guarantees the selected checkpoint did not
  /// advance. Any unresolved post-selection result must be `CommitUnknown`.
  fn publish(&mut self, batch: &FrozenIndexBatchV1) -> Result<IndexRuntimePublicationReceiptV1, IndexRuntimePublicationErrorV1>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexRuntimeFlushOutcomeV1 {
  Idle,
  Deferred { retry_at_ms: u64 },
  Published { records: u64, publication_bytes: u64, checkpoint_sequence: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRuntimeSnapshotV1 {
  pub lifecycle: IndexRuntimeLifecycleV1,
  pub recovered_scopes: u32,
  pub highest_checkpoint_sequence: u64,
  pub degraded: Option<IndexRuntimeDegradedStateV1>,
  pub publication_in_flight: bool,
  pub soft_hub: SoftMutationHubSnapshotV1,
  pub producer: IndexProducerCoordinatorSnapshotV1,
  pub mutations: IndexCoordinatorSnapshotV1,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IndexRuntimeErrorV1 {
  #[error("invalid index runtime owner options or recovery: {0}")]
  Invalid(String),
  #[error("index runtime owner is not running: {lifecycle:?}")]
  NotRunning { lifecycle: IndexRuntimeLifecycleV1 },
  #[error("index runtime recovery was already resolved as {lifecycle:?}")]
  RecoveryAlreadyResolved { lifecycle: IndexRuntimeLifecycleV1 },
  #[error("index runtime recovery was canceled")]
  Canceled,
  #[error("index runtime owner lock is poisoned")]
  Poisoned,
  #[error("index runtime memory admission failed: {0}")]
  Memory(String),
  #[error("index runtime soft handoff failed: {0}")]
  SoftHub(String),
  #[error("index runtime producer coordination failed: {0}")]
  Producer(String),
  #[error(
    "index runtime drain is incomplete: soft={pending_soft_notices}, reconciliation={soft_reconciliation_required}, pending={pending_tasks}, leased={leased_tasks}, active={active_records}, frozen={frozen_records}"
  )]
  DrainIncomplete {
    pending_soft_notices: usize,
    soft_reconciliation_required: bool,
    pending_tasks: u32,
    leased_tasks: u32,
    active_records: u64,
    frozen_records: u64,
  },
  #[error("index runtime mutation coordinator failed: {0}")]
  Mutations(String),
  #[error("index runtime journal admission failed: {0}")]
  Journal(String),
  #[error("index runtime worker failed: {0}")]
  Work(String),
  #[error("index runtime publication failed: {0}")]
  Publication(String),
  #[error("index runtime publication batch {batch_id} attempt {attempt_id} is already in progress")]
  PublicationInProgress { batch_id: u64, attempt_id: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IndexRuntimePublicationAttemptV1 {
  batch_id: u64,
  attempt_id: u64,
}

struct IndexRuntimeStateV1 {
  lifecycle: IndexRuntimeLifecycleV1,
  recovered_scopes: u32,
  highest_checkpoint_sequence: u64,
  degraded: Option<IndexRuntimeDegradedStateV1>,
  producer: IndexProducerCoordinatorV1,
  mutations: IndexCoordinatorV1,
  worker: IndexProducerMutationWorkerV1,
  service_retry_not_before_ms: u64,
  publication_retry_not_before_ms: u64,
  publication_in_flight: Option<IndexRuntimePublicationAttemptV1>,
}

pub struct IndexRuntimeOwnerV1 {
  hash_algorithm: HashAlgorithm,
  soft_hub: Arc<SoftMutationHubV1>,
  _soft_capacity: MemoryReservation,
  state: Mutex<IndexRuntimeStateV1>,
  observability: ArcSwap<IndexRuntimeSnapshotV1>,
  source_retry_after_ms: u64,
  publication_retry_after_ms: u64,
}

impl IndexRuntimeOwnerV1 {
  pub fn new(
    coordinator_id: [u8; 16],
    hash_algorithm: HashAlgorithm,
    memory: MemoryCoordinator,
    options: IndexRuntimeOwnerOptionsV1,
    now_ms: u64,
  ) -> Result<Self, IndexRuntimeErrorV1> {
    let soft_hub = Arc::new(SoftMutationHubV1::new(options.soft_hub).map_err(soft_error)?);
    Self::new_with_soft_hub(coordinator_id, hash_algorithm, memory, options, now_ms, soft_hub)
  }

  pub fn new_with_soft_hub(
    coordinator_id: [u8; 16],
    hash_algorithm: HashAlgorithm,
    memory: MemoryCoordinator,
    options: IndexRuntimeOwnerOptionsV1,
    now_ms: u64,
    soft_hub: Arc<SoftMutationHubV1>,
  ) -> Result<Self, IndexRuntimeErrorV1> {
    if options.source_retry_after_ms == 0 || options.publication_retry_after_ms == 0 {
      return Err(IndexRuntimeErrorV1::Invalid("runtime retry delays must be nonzero".to_string()));
    }
    let soft_snapshot = soft_hub.snapshot().map_err(soft_error)?;
    if soft_snapshot.maximum_notices != options.soft_hub.maximum_notices
      || soft_snapshot.maximum_retained_bytes != options.soft_hub.maximum_retained_bytes
      || soft_snapshot.maximum_notice_bytes != options.soft_hub.maximum_notice_bytes
    {
      return Err(IndexRuntimeErrorV1::Invalid("shared soft hub capacity does not match the runtime owner options".to_string()));
    }
    let soft_capacity = modeled_soft_capacity(options.soft_hub)?;
    let soft_capacity = memory
      .reserve(MemoryOwner::IndexDirtyBuffers, soft_capacity, AdmissionClass::Workload)
      .map_err(|error| IndexRuntimeErrorV1::Memory(error.to_string()))?;
    let producer = IndexProducerCoordinatorV1::new(hash_algorithm, memory.clone(), options.producer)
      .map_err(|error| producer_error(error.to_string()))?;
    let mutations = IndexCoordinatorV1::new(coordinator_id, hash_algorithm, memory.clone(), options.mutations, now_ms)
      .map_err(|error| IndexRuntimeErrorV1::Mutations(error.to_string()))?;
    let collector = IndexProducerCollectorV1::new(hash_algorithm, memory, options.collector)
      .map_err(|error| IndexRuntimeErrorV1::Invalid(error.to_string()))?;
    let worker = IndexProducerMutationWorkerV1::new(
      hash_algorithm,
      IndexProducerExecutorV1::new(collector),
      options.semantic,
      options.source_retry_after_ms,
    )
    .map_err(|error| IndexRuntimeErrorV1::Invalid(error.to_string()))?;
    let state = IndexRuntimeStateV1 {
      lifecycle: IndexRuntimeLifecycleV1::Recovering,
      recovered_scopes: 0,
      highest_checkpoint_sequence: 0,
      degraded: None,
      producer,
      mutations,
      worker,
      service_retry_not_before_ms: 0,
      publication_retry_not_before_ms: 0,
      publication_in_flight: None,
    };
    let observability = ArcSwap::from_pointee(runtime_snapshot(&state, soft_snapshot));
    Ok(Self {
      hash_algorithm,
      soft_hub,
      _soft_capacity: soft_capacity,
      state: Mutex::new(state),
      observability,
      source_retry_after_ms: options.source_retry_after_ms,
      publication_retry_after_ms: options.publication_retry_after_ms,
    })
  }

  pub fn offer_acknowledgement(&self, acknowledgement: &NamespaceMutationAcknowledgement) -> SoftMutationAdmissionV1 {
    let admission = self.soft_hub.offer_acknowledgement(acknowledgement);
    match self.soft_hub.snapshot() {
      Ok(soft_hub) => {
        self.observability.rcu(|current| {
          let mut next = (**current).clone();
          next.soft_hub = soft_hub.clone();
          Arc::new(next)
        });
      }
      Err(error) => {
        tracing::error!(%error, "Index runtime soft-hub observability failed; projecting reconciliation-required state");
        self.observability.rcu(|current| {
          let mut next = (**current).clone();
          next.soft_hub.reconciliation_required = true;
          next.soft_hub.lost_through_sequence = Some(match next.soft_hub.lost_through_sequence {
            Some(sequence) => sequence.max(acknowledgement.publication_sequence),
            None => acknowledgement.publication_sequence,
          });
          if !next.soft_hub.loss_reasons.contains(&SoftMutationLossReasonV1::QueueUnavailable) {
            next.soft_hub.loss_reasons.push(SoftMutationLossReasonV1::QueueUnavailable);
          }
          next.soft_hub.dropped_notices = next.soft_hub.dropped_notices.saturating_add(1);
          next.soft_hub.loss_epoch = next.soft_hub.loss_epoch.saturating_add(1);
          Arc::new(next)
        });
        return SoftMutationAdmissionV1::ReconciliationRequired(SoftMutationLossReasonV1::QueueUnavailable);
      }
    }
    admission
  }

  pub fn snapshot(&self) -> Result<IndexRuntimeSnapshotV1, IndexRuntimeErrorV1> {
    let soft_hub = self.soft_hub.snapshot().map_err(soft_error)?;
    let state = self.state.lock().map_err(|_| IndexRuntimeErrorV1::Poisoned)?;
    Ok(runtime_snapshot(&state, soft_hub))
  }

  pub fn cached_snapshot(&self) -> Arc<IndexRuntimeSnapshotV1> {
    self.observability.load_full()
  }

  pub fn complete_recovery(&self, decision: IndexRuntimeRecoveryDecisionV1) -> Result<(), IndexRuntimeErrorV1> {
    let soft = self.soft_hub.snapshot().map_err(soft_error)?;
    let mut state = self.state.lock().map_err(|_| IndexRuntimeErrorV1::Poisoned)?;
    if state.lifecycle != IndexRuntimeLifecycleV1::Recovering {
      return Err(IndexRuntimeErrorV1::RecoveryAlreadyResolved { lifecycle: state.lifecycle });
    }
    match decision {
      IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes, highest_checkpoint_sequence } => {
        if (recovered_scopes == 0) != (highest_checkpoint_sequence == 0) {
          return Err(IndexRuntimeErrorV1::Invalid(
            "content-only recovery must use zero scopes/sequence and indexed recovery must use nonzero scopes/sequence".to_string(),
          ));
        }
        state.recovered_scopes = recovered_scopes;
        state.highest_checkpoint_sequence = highest_checkpoint_sequence;
        if soft.reconciliation_required {
          latch_degraded(
            &mut state,
            "soft_mutation_loss_during_recovery",
            "the soft mutation handoff lost authority before runtime recovery completed".to_string(),
          );
        } else {
          state.lifecycle = IndexRuntimeLifecycleV1::Running;
        }
      }
      IndexRuntimeRecoveryDecisionV1::ReconciliationRequired { code, context } => {
        validate_degraded(code, &context)?;
        state.degraded = Some(IndexRuntimeDegradedStateV1 { code, context });
        state.lifecycle = IndexRuntimeLifecycleV1::Degraded;
      }
      IndexRuntimeRecoveryDecisionV1::Canceled => return Err(IndexRuntimeErrorV1::Canceled),
    }
    let snapshot = runtime_snapshot(&state, soft);
    drop(state);
    self.observability.store(Arc::new(snapshot));
    Ok(())
  }

  fn refresh_cached_snapshot(&self) -> Result<(), IndexRuntimeErrorV1> {
    let snapshot = self.snapshot()?;
    self.observability.store(Arc::new(snapshot));
    Ok(())
  }

  pub fn admit_task(
    &self,
    request: IndexProducerTaskRequestV1<'_>,
    now_ms: u64,
    spill_store: &mut dyn IndexProducerSpillStoreV1,
  ) -> Result<IndexProducerAdmissionV1, IndexRuntimeErrorV1> {
    let result = (|| {
      let soft = self.soft_hub.snapshot().map_err(soft_error)?;
      let mut state = self.state.lock().map_err(|_| IndexRuntimeErrorV1::Poisoned)?;
      observe_soft_loss(&mut state, &soft);
      require_running(&state)?;
      state.producer.admit_or_spill(request, now_ms, spill_store).map_err(|error| producer_error(error.to_string()))
    })();
    self.finish_observed(result)
  }

  pub fn admit_mutation_journal(
    &self,
    journal: &MutationJournalV1<'_>,
    now_ms: u64,
    is_cancelled: &dyn Fn() -> bool,
    spill_store: &mut dyn IndexProducerSpillStoreV1,
  ) -> Result<IndexProducerJournalAdmissionSummaryV1, IndexRuntimeErrorV1> {
    let result = (|| {
      if is_cancelled() {
        return Err(IndexRuntimeErrorV1::Canceled);
      }
      let soft = self.soft_hub.snapshot().map_err(soft_error)?;
      let mut state = self.state.lock().map_err(|_| IndexRuntimeErrorV1::Poisoned)?;
      observe_soft_loss(&mut state, &soft);
      require_running(&state)?;
      admit_mutation_journal_tasks(self.hash_algorithm, &mut state.producer, journal, now_ms, is_cancelled, spill_store)
        .map_err(journal_error)
    })();
    self.finish_observed(result)
  }

  pub fn execute_next_mutation(
    &self,
    request: IndexRuntimeMutationWorkRequestV1<'_, '_>,
    spill_store: &mut dyn IndexProducerSpillStoreV1,
  ) -> Result<IndexRuntimeWorkOutcomeV1, IndexRuntimeErrorV1> {
    let result = (|| {
      if (request.is_cancelled)() {
        return Err(IndexRuntimeErrorV1::Canceled);
      }
      let soft = self.soft_hub.snapshot().map_err(soft_error)?;
      let mut state = self.state.lock().map_err(|_| IndexRuntimeErrorV1::Poisoned)?;
      observe_soft_loss(&mut state, &soft);
      require_serviceable(&state)?;
      if request.now_ms < state.service_retry_not_before_ms {
        return Ok(IndexRuntimeWorkOutcomeV1::Deferred { retry_at_ms: state.service_retry_not_before_ms });
      }
      let Some(lease) =
        state.producer.lease_next(request.now_ms, (request.is_cancelled)()).map_err(|error| producer_error(error.to_string()))?
      else {
        return Ok(IndexRuntimeWorkOutcomeV1::Idle);
      };
      let execution = {
        let IndexRuntimeStateV1 { producer, mutations, worker, .. } = &mut *state;
        catch_unwind(AssertUnwindSafe(|| {
          worker.execute(
            producer,
            mutations,
            IndexProducerMutationWorkRequestV1 {
              lease: &lease,
              journal: request.journal,
              revision_source: request.revision_source,
              semantic_source: request.semantic_source,
              parser: request.parser,
              mapper: request.mapper,
              now_ms: request.now_ms,
              is_cancelled: request.is_cancelled,
            },
            spill_store,
          )
        }))
      };
      match execution {
        Ok(Ok(outcome)) => {
          state.service_retry_not_before_ms = 0;
          Ok(IndexRuntimeWorkOutcomeV1::Completed(outcome))
        }
        Ok(Err(IndexProducerWorkerErrorV1::Cancelled)) => Err(IndexRuntimeErrorV1::Canceled),
        Ok(Err(error)) if retryable_worker_error(&error) => {
          let Some(retry_at_ms) = request.now_ms.checked_add(self.source_retry_after_ms) else {
            let context = "worker retry deadline overflowed after releasing the exact task lease".to_string();
            latch_degraded(&mut state, "worker_retry_deadline", context.clone());
            return Err(IndexRuntimeErrorV1::Work(context));
          };
          state.service_retry_not_before_ms = retry_at_ms;
          Err(IndexRuntimeErrorV1::Work(error.to_string()))
        }
        Ok(Err(error)) => {
          latch_degraded(&mut state, "worker_terminal", error.to_string());
          Err(IndexRuntimeErrorV1::Work(error.to_string()))
        }
        Err(_) => {
          let context = if state.producer.snapshot().leased_tasks != 0 {
            match state.producer.cancel(&lease) {
              Ok(()) => "index producer worker panicked".to_string(),
              Err(error) => format!("index producer worker panicked and its lease could not be released: {error}"),
            }
          } else {
            "index producer worker panicked after releasing its lease".to_string()
          };
          latch_degraded(&mut state, "worker_panic", context.clone());
          Err(IndexRuntimeErrorV1::Work(context))
        }
      }
    })();
    self.finish_observed(result)
  }

  pub fn flush(
    &self,
    now_ms: u64,
    requested_reason: Option<IndexFlushReasonV1>,
    cancelled: bool,
    publisher: &mut dyn IndexRuntimeBatchPublisherV1,
  ) -> Result<IndexRuntimeFlushOutcomeV1, IndexRuntimeErrorV1> {
    let result = (|| {
      if cancelled {
        return Err(IndexRuntimeErrorV1::Canceled);
      }
      let soft = self.soft_hub.snapshot().map_err(soft_error)?;
      let mut state = self.state.lock().map_err(|_| IndexRuntimeErrorV1::Poisoned)?;
      observe_soft_loss(&mut state, &soft);
      require_serviceable(&state)?;
      if let Some(attempt) = state.publication_in_flight {
        return Err(IndexRuntimeErrorV1::PublicationInProgress { batch_id: attempt.batch_id, attempt_id: attempt.attempt_id });
      }
      if now_ms < state.publication_retry_not_before_ms {
        return Ok(IndexRuntimeFlushOutcomeV1::Deferred { retry_at_ms: state.publication_retry_not_before_ms });
      }
      prepare_mutation_drain(&mut state)?;
      let snapshot = state.mutations.snapshot();
      let batch = if snapshot.frozen_records != 0 {
        state.mutations.retry_frozen(cancelled).map_err(|error| IndexRuntimeErrorV1::Mutations(error.to_string()))?
      } else {
        let reason =
          if state.lifecycle == IndexRuntimeLifecycleV1::Draining { Some(IndexFlushReasonV1::Shutdown) } else { requested_reason };
        let Some(batch) =
          state.mutations.begin_flush(now_ms, reason, cancelled).map_err(|error| IndexRuntimeErrorV1::Mutations(error.to_string()))?
        else {
          return Ok(IndexRuntimeFlushOutcomeV1::Idle);
        };
        batch
      };
      let attempt = IndexRuntimePublicationAttemptV1 { batch_id: batch.batch_id(), attempt_id: batch.attempt_id() };
      state.publication_in_flight = Some(attempt);
      self.observability.store(Arc::new(runtime_snapshot(&state, soft)));
      drop(state);

      let publication = catch_unwind(AssertUnwindSafe(|| publisher.publish(&batch)));
      let mut state = self.state.lock().map_err(|_| IndexRuntimeErrorV1::Poisoned)?;
      match state.publication_in_flight.take() {
        Some(current) if current == attempt => {}
        Some(current) => {
          state.publication_in_flight = Some(current);
          latch_degraded(
            &mut state,
            "publication_attempt_mismatch",
            "runtime publication attempt identity changed while storage I/O was active".to_string(),
          );
          return Err(IndexRuntimeErrorV1::Publication("publication attempt identity changed while I/O was active".to_string()));
        }
        None => {
          latch_degraded(
            &mut state,
            "publication_attempt_missing",
            "runtime publication attempt identity disappeared while storage I/O was active".to_string(),
          );
          return Err(IndexRuntimeErrorV1::Publication("publication attempt identity disappeared while I/O was active".to_string()));
        }
      }
      match publication {
        Err(_) => {
          latch_degraded(&mut state, "publication_panic", "index batch publisher panicked with an exact frozen batch retained".to_string());
          Err(IndexRuntimeErrorV1::Publication("index batch publisher panicked".to_string()))
        }
        Ok(Err(error)) if !valid_publication_error(&error) => {
          latch_degraded(
            &mut state,
            "publication_error_malformed",
            "publisher returned malformed or amplified failure evidence with an exact frozen batch retained".to_string(),
          );
          Err(IndexRuntimeErrorV1::Publication("publisher returned malformed failure evidence".to_string()))
        }
        Ok(Ok(receipt)) => {
          if !receipt_matches(&receipt, &batch, state.highest_checkpoint_sequence) {
            latch_degraded(
              &mut state,
              "publication_receipt_mismatch",
              "publisher returned a receipt for different batch bytes".to_string(),
            );
            return Err(IndexRuntimeErrorV1::Publication("publisher returned a dishonest or malformed receipt".to_string()));
          }
          if let Err(error) = state.mutations.complete_success(&batch) {
            let context = format!("selected checkpoint advanced but local batch finalization failed: {error}");
            latch_degraded(&mut state, "publication_local_finalize", context.clone());
            return Err(IndexRuntimeErrorV1::Mutations(context));
          }
          state.publication_retry_not_before_ms = 0;
          state.highest_checkpoint_sequence = state.highest_checkpoint_sequence.max(receipt.checkpoint_sequence);
          Ok(IndexRuntimeFlushOutcomeV1::Published {
            records: receipt.published_records,
            publication_bytes: receipt.publication_bytes,
            checkpoint_sequence: receipt.checkpoint_sequence,
          })
        }
        Ok(Err(error)) if error.class() == IndexRuntimePublicationErrorClassV1::RetryableBeforeSelection => {
          if let Err(restore) = state.mutations.complete_failure(&batch, now_ms) {
            let context = format!("{error}; batch restoration failed: {restore}");
            latch_degraded(&mut state, "publication_restore_failed", context.clone());
            return Err(IndexRuntimeErrorV1::Mutations(context));
          }
          let Some(retry_at_ms) = now_ms.checked_add(self.publication_retry_after_ms) else {
            let context = "publication retry deadline overflowed after restoring the exact batch".to_string();
            latch_degraded(&mut state, "publication_retry_deadline", context.clone());
            return Err(IndexRuntimeErrorV1::Publication(context));
          };
          state.publication_retry_not_before_ms = retry_at_ms;
          Err(IndexRuntimeErrorV1::Publication(error.to_string()))
        }
        Ok(Err(error)) if error.class() == IndexRuntimePublicationErrorClassV1::CancelledBeforeSelection => {
          if let Err(restore) = state.mutations.complete_failure(&batch, now_ms) {
            let context = format!("{error}; canceled batch restoration failed: {restore}");
            latch_degraded(&mut state, "publication_cancel_restore_failed", context.clone());
            return Err(IndexRuntimeErrorV1::Mutations(context));
          }
          Err(IndexRuntimeErrorV1::Canceled)
        }
        Ok(Err(error)) => {
          latch_degraded(&mut state, "publication_uncertain", error.to_string());
          Err(IndexRuntimeErrorV1::Publication(error.to_string()))
        }
      }
    })();
    self.finish_observed(result)
  }

  pub fn begin_draining(&self) -> Result<(), IndexRuntimeErrorV1> {
    let result = (|| {
      let soft = self.soft_hub.snapshot().map_err(soft_error)?;
      let mut state = self.state.lock().map_err(|_| IndexRuntimeErrorV1::Poisoned)?;
      observe_soft_loss(&mut state, &soft);
      require_running(&state)?;
      if let Err(error) = self.soft_hub.close_admission() {
        let context = format!("soft mutation admission could not close before drain: {error}");
        latch_degraded(&mut state, "soft_mutation_close_failed", context.clone());
        return Err(IndexRuntimeErrorV1::SoftHub(context));
      }
      state.lifecycle = IndexRuntimeLifecycleV1::Draining;
      Ok(())
    })();
    self.finish_observed(result)
  }

  pub fn finish_draining(&self) -> Result<(), IndexRuntimeErrorV1> {
    let result = (|| {
      let soft = self.soft_hub.snapshot().map_err(soft_error)?;
      let mut state = self.state.lock().map_err(|_| IndexRuntimeErrorV1::Poisoned)?;
      observe_soft_loss(&mut state, &soft);
      if state.lifecycle != IndexRuntimeLifecycleV1::Draining {
        return Err(IndexRuntimeErrorV1::NotRunning { lifecycle: state.lifecycle });
      }
      let producer = state.producer.snapshot();
      if soft.queued_notices != 0 || soft.reconciliation_required || producer.pending_tasks != 0 || producer.leased_tasks != 0 {
        let mutations = state.mutations.snapshot();
        return Err(IndexRuntimeErrorV1::DrainIncomplete {
          pending_soft_notices: soft.queued_notices,
          soft_reconciliation_required: soft.reconciliation_required,
          pending_tasks: producer.pending_tasks,
          leased_tasks: producer.leased_tasks,
          active_records: mutations.active_records,
          frozen_records: mutations.frozen_records,
        });
      }
      prepare_mutation_drain(&mut state)?;
      let mutations = state.mutations.snapshot();
      if mutations.active_records != 0 || mutations.frozen_records != 0 {
        return Err(IndexRuntimeErrorV1::DrainIncomplete {
          pending_soft_notices: soft.queued_notices,
          soft_reconciliation_required: soft.reconciliation_required,
          pending_tasks: 0,
          leased_tasks: 0,
          active_records: mutations.active_records,
          frozen_records: mutations.frozen_records,
        });
      }
      state.mutations.finish_draining().map_err(|error| IndexRuntimeErrorV1::Mutations(error.to_string()))?;
      state.lifecycle = IndexRuntimeLifecycleV1::Stopped;
      Ok(())
    })();
    self.finish_observed(result)
  }

  fn finish_observed<T>(&self, result: Result<T, IndexRuntimeErrorV1>) -> Result<T, IndexRuntimeErrorV1> {
    let observation = self.refresh_cached_snapshot();
    match result {
      Err(error) => Err(error),
      Ok(value) => {
        observation?;
        Ok(value)
      }
    }
  }
}

fn runtime_snapshot(state: &IndexRuntimeStateV1, soft_hub: SoftMutationHubSnapshotV1) -> IndexRuntimeSnapshotV1 {
  IndexRuntimeSnapshotV1 {
    lifecycle: state.lifecycle,
    recovered_scopes: state.recovered_scopes,
    highest_checkpoint_sequence: state.highest_checkpoint_sequence,
    degraded: state.degraded.clone(),
    publication_in_flight: state.publication_in_flight.is_some(),
    soft_hub,
    producer: state.producer.snapshot(),
    mutations: state.mutations.snapshot(),
  }
}

fn require_running(state: &IndexRuntimeStateV1) -> Result<(), IndexRuntimeErrorV1> {
  if state.lifecycle == IndexRuntimeLifecycleV1::Running {
    Ok(())
  } else {
    Err(IndexRuntimeErrorV1::NotRunning { lifecycle: state.lifecycle })
  }
}

fn require_serviceable(state: &IndexRuntimeStateV1) -> Result<(), IndexRuntimeErrorV1> {
  if matches!(state.lifecycle, IndexRuntimeLifecycleV1::Running | IndexRuntimeLifecycleV1::Draining) {
    Ok(())
  } else {
    Err(IndexRuntimeErrorV1::NotRunning { lifecycle: state.lifecycle })
  }
}

fn observe_soft_loss(state: &mut IndexRuntimeStateV1, soft: &SoftMutationHubSnapshotV1) {
  if soft.reconciliation_required && matches!(state.lifecycle, IndexRuntimeLifecycleV1::Running | IndexRuntimeLifecycleV1::Draining) {
    latch_degraded(state, "soft_mutation_loss", "the bounded soft mutation handoff requires authoritative reconciliation".to_string());
  }
}

fn prepare_mutation_drain(state: &mut IndexRuntimeStateV1) -> Result<(), IndexRuntimeErrorV1> {
  if state.lifecycle == IndexRuntimeLifecycleV1::Draining
    && state.producer.snapshot().pending_tasks == 0
    && state.producer.snapshot().leased_tasks == 0
    && state.mutations.snapshot().lifecycle == IndexCoordinatorLifecycleV1::Running
  {
    state.mutations.begin_draining().map_err(|error| IndexRuntimeErrorV1::Mutations(error.to_string()))?;
  }
  Ok(())
}

fn receipt_matches(receipt: &IndexRuntimePublicationReceiptV1, batch: &FrozenIndexBatchV1, selected_checkpoint_sequence: u64) -> bool {
  receipt.batch_id == batch.batch_id()
    && receipt.attempt_id == batch.attempt_id()
    && receipt.published_records == batch.records().len() as u64
    && receipt.publication_bytes == batch.publication_bytes()
    && receipt.checkpoint_sequence > selected_checkpoint_sequence
}

fn valid_publication_error(error: &IndexRuntimePublicationErrorV1) -> bool {
  valid_stable_code(error.code()) && !error.context().is_empty() && error.context().len() <= MAX_DEGRADED_CONTEXT_BYTES
}

fn modeled_soft_capacity(options: SoftMutationHubOptionsV1) -> Result<u64, IndexRuntimeErrorV1> {
  let slots = options
    .maximum_notices
    .checked_mul(size_of::<super::coverage_runtime::SoftMutationNoticeV1>())
    .ok_or_else(|| IndexRuntimeErrorV1::Invalid("soft mutation slot capacity overflowed".to_string()))?;
  let capacity = options
    .maximum_retained_bytes
    .checked_add(slots)
    .ok_or_else(|| IndexRuntimeErrorV1::Invalid("soft mutation capacity overflowed".to_string()))?;
  u64::try_from(capacity).map_err(|error| IndexRuntimeErrorV1::Invalid(format!("soft mutation capacity exceeds this platform: {error}")))
}

fn retryable_worker_error(error: &IndexProducerWorkerErrorV1) -> bool {
  matches!(
    error,
    IndexProducerWorkerErrorV1::Execution(super::index_producer_executor::IndexProducerExecutionErrorV1::Collector(
      IndexProducerCollectorErrorV1::ResourcePressure(_)
    ))
  )
}

fn latch_degraded(state: &mut IndexRuntimeStateV1, code: &'static str, context: String) {
  state.degraded = Some(IndexRuntimeDegradedStateV1 { code, context: bounded_context(context) });
  state.lifecycle = IndexRuntimeLifecycleV1::Degraded;
}

fn bounded_context(mut context: String) -> String {
  if context.len() <= MAX_DEGRADED_CONTEXT_BYTES {
    return context;
  }
  let mut boundary = MAX_DEGRADED_CONTEXT_BYTES;
  while !context.is_char_boundary(boundary) {
    boundary -= 1;
  }
  context.truncate(boundary);
  context
}

fn validate_degraded(code: &str, context: &str) -> Result<(), IndexRuntimeErrorV1> {
  if !valid_stable_code(code) || context.is_empty() || context.len() > MAX_DEGRADED_CONTEXT_BYTES {
    return Err(IndexRuntimeErrorV1::Invalid("degraded evidence must have a code and bounded nonempty context".to_string()));
  }
  Ok(())
}

fn valid_stable_code(code: &str) -> bool {
  !code.is_empty() && code.len() <= MAX_STABLE_CODE_BYTES
}

fn soft_error(error: SoftMutationHubErrorV1) -> IndexRuntimeErrorV1 {
  IndexRuntimeErrorV1::SoftHub(error.to_string())
}

fn producer_error(error: String) -> IndexRuntimeErrorV1 {
  IndexRuntimeErrorV1::Producer(error)
}

fn journal_error(error: IndexProducerJournalAdmissionErrorV1) -> IndexRuntimeErrorV1 {
  match error {
    IndexProducerJournalAdmissionErrorV1::Cancelled => IndexRuntimeErrorV1::Canceled,
    error => IndexRuntimeErrorV1::Journal(error.to_string()),
  }
}
