//! Single serialized cadence for due v4 runtime publication.
//!
//! Scheduling remains owned by the existing server index timer. This type
//! only serializes access to the mutable selector-last publisher and delegates
//! count, age, and memory-pressure decisions to the runtime owner.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::engine::VirtualClock;

use super::index_coordinator::IndexFlushReasonV1;
use super::index_runtime_owner::{
  IndexRuntimeBatchPublisherV1, IndexRuntimeErrorV1, IndexRuntimeFlushOutcomeV1, IndexRuntimeLifecycleV1, IndexRuntimeOwnerV1,
};
use super::index_runtime_batch_publisher::NativeIndexRuntimeBatchPublisherV1;
use super::index_runtime_workspace_store::{IndexRuntimeWorkspaceIdentityV1, IndexRuntimeWorkspaceSelectedHeadV1};

pub type NativeIndexRuntimeCadenceV1 = IndexRuntimeCadenceV1<NativeIndexRuntimeBatchPublisherV1>;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IndexRuntimeCadenceErrorV1 {
  #[error("index runtime cadence clock returned a zero timestamp")]
  InvalidClock,
  #[error("index runtime cadence publisher lock is poisoned")]
  PublisherPoisoned,
  #[error("installed index runtime has no cadence publisher")]
  NotInstalled,
  #[error("index runtime drain made no progress while dirty records remained")]
  DrainStalled,
  #[error("index runtime drain accounting overflowed")]
  DrainAccountingOverflow,
  #[error("index runtime drain unexpectedly deferred until {retry_at_ms}")]
  DrainDeferred { retry_at_ms: u64 },
  #[error("index runtime cadence failed ({failure}) and could not latch degraded state: {source}")]
  FailureLatch { failure: &'static str, source: IndexRuntimeErrorV1 },
  #[error(transparent)]
  Runtime(#[from] IndexRuntimeErrorV1),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexRuntimeDrainOutcomeV1 {
  pub published_batches: u64,
  pub published_records: u64,
  pub publication_bytes: u64,
  pub highest_checkpoint_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeIndexRuntimeWorkspaceSnapshotV1 {
  pub identity: IndexRuntimeWorkspaceIdentityV1,
  pub path: PathBuf,
  pub selected_head: Option<IndexRuntimeWorkspaceSelectedHeadV1>,
}

pub struct IndexRuntimeCadenceV1<Publisher> {
  owner: Arc<IndexRuntimeOwnerV1>,
  publisher: Mutex<Publisher>,
  cancellation: CancellationToken,
  clock: Arc<dyn VirtualClock>,
}

impl<Publisher> IndexRuntimeCadenceV1<Publisher>
where
  Publisher: IndexRuntimeBatchPublisherV1 + Send,
{
  pub fn new(
    owner: Arc<IndexRuntimeOwnerV1>,
    publisher: Publisher,
    cancellation: CancellationToken,
    clock: Arc<dyn VirtualClock>,
  ) -> Result<Self, IndexRuntimeCadenceErrorV1> {
    if clock.now_ms() == 0 {
      return Err(IndexRuntimeCadenceErrorV1::InvalidClock);
    }
    Ok(Self { owner, publisher: Mutex::new(publisher), cancellation, clock })
  }

  pub fn flush_if_due(&self) -> Result<IndexRuntimeFlushOutcomeV1, IndexRuntimeCadenceErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(IndexRuntimeCadenceErrorV1::Runtime(IndexRuntimeErrorV1::Canceled));
    }
    let now_ms = self.clock.now_ms();
    if now_ms == 0 {
      return Err(self.fail_closed(
        IndexRuntimeCadenceErrorV1::InvalidClock,
        "cadence_invalid_clock",
        "index runtime cadence clock returned zero after installation; queued index work was retained",
      ));
    }
    let mut publisher = self.lock_publisher("index runtime cadence publisher lock was poisoned; queued index work was retained")?;
    let cancelled = self.cancellation.is_cancelled();
    Ok(self.owner.flush(now_ms, None, cancelled, &mut *publisher)?)
  }

  /// Close runtime admission and publish every exact dirty batch through this
  /// cadence's existing selector-last publisher before stopping the owner.
  pub fn drain_and_stop(&self) -> Result<IndexRuntimeDrainOutcomeV1, IndexRuntimeCadenceErrorV1> {
    let mut publisher =
      self.lock_publisher("index runtime cadence publisher lock was poisoned during shutdown; queued index work was retained")?;
    let snapshot = self.owner.snapshot()?;
    if snapshot.lifecycle == IndexRuntimeLifecycleV1::Stopped {
      return Ok(IndexRuntimeDrainOutcomeV1 {
        published_batches: 0,
        published_records: 0,
        publication_bytes: 0,
        highest_checkpoint_sequence: snapshot.highest_checkpoint_sequence,
      });
    }
    if self.cancellation.is_cancelled() {
      return Err(IndexRuntimeCadenceErrorV1::Runtime(IndexRuntimeErrorV1::Canceled));
    }
    let mut now_ms = self.clock.now_ms();
    if now_ms == 0 {
      return Err(self.fail_closed(
        IndexRuntimeCadenceErrorV1::InvalidClock,
        "cadence_invalid_clock",
        "index runtime cadence clock returned zero during shutdown; queued index work was retained",
      ));
    }
    match snapshot.lifecycle {
      IndexRuntimeLifecycleV1::Running => self.owner.begin_draining()?,
      IndexRuntimeLifecycleV1::Draining => {}
      lifecycle => return Err(IndexRuntimeCadenceErrorV1::Runtime(IndexRuntimeErrorV1::NotRunning { lifecycle })),
    }

    let mut outcome = IndexRuntimeDrainOutcomeV1 {
      published_batches: 0,
      published_records: 0,
      publication_bytes: 0,
      highest_checkpoint_sequence: snapshot.highest_checkpoint_sequence,
    };
    loop {
      let before = self.owner.snapshot()?;
      let retained_before = before.mutations.active_records.saturating_add(before.mutations.frozen_records);
      match self.owner.flush(now_ms, Some(IndexFlushReasonV1::Shutdown), false, &mut *publisher)? {
        IndexRuntimeFlushOutcomeV1::Idle => {
          self.owner.finish_draining()?;
          outcome.highest_checkpoint_sequence = self.owner.cached_snapshot().highest_checkpoint_sequence;
          return Ok(outcome);
        }
        IndexRuntimeFlushOutcomeV1::Deferred { retry_at_ms } => {
          return Err(IndexRuntimeCadenceErrorV1::DrainDeferred { retry_at_ms });
        }
        IndexRuntimeFlushOutcomeV1::Published { records, publication_bytes, checkpoint_sequence } => {
          outcome.published_batches =
            outcome.published_batches.checked_add(1).ok_or(IndexRuntimeCadenceErrorV1::DrainAccountingOverflow)?;
          outcome.published_records =
            outcome.published_records.checked_add(records).ok_or(IndexRuntimeCadenceErrorV1::DrainAccountingOverflow)?;
          outcome.publication_bytes =
            outcome.publication_bytes.checked_add(publication_bytes).ok_or(IndexRuntimeCadenceErrorV1::DrainAccountingOverflow)?;
          outcome.highest_checkpoint_sequence = outcome.highest_checkpoint_sequence.max(checkpoint_sequence);
          let after = self.owner.snapshot()?;
          let retained_after = after.mutations.active_records.saturating_add(after.mutations.frozen_records);
          if records == 0 || retained_after >= retained_before {
            return Err(self.fail_closed(
              IndexRuntimeCadenceErrorV1::DrainStalled,
              "cadence_drain_stalled",
              "index runtime shutdown publication did not reduce retained dirty records",
            ));
          }
          now_ms = self.clock.now_ms();
          if now_ms == 0 {
            return Err(self.fail_closed(
              IndexRuntimeCadenceErrorV1::InvalidClock,
              "cadence_invalid_clock",
              "index runtime cadence clock returned zero between shutdown batches; remaining work was retained",
            ));
          }
        }
      }
    }
  }

  fn fail_closed(&self, failure: IndexRuntimeCadenceErrorV1, code: &'static str, context: &str) -> IndexRuntimeCadenceErrorV1 {
    match self.owner.latch_cadence_failure(code, context.to_string()) {
      Ok(()) => failure,
      Err(source) => IndexRuntimeCadenceErrorV1::FailureLatch { failure: code, source },
    }
  }

  fn lock_publisher(&self, context: &str) -> Result<MutexGuard<'_, Publisher>, IndexRuntimeCadenceErrorV1> {
    self
      .publisher
      .lock()
      .map_err(|_| self.fail_closed(IndexRuntimeCadenceErrorV1::PublisherPoisoned, "cadence_publisher_poisoned", context))
  }
}

impl IndexRuntimeCadenceV1<NativeIndexRuntimeBatchPublisherV1> {
  /// Observe recovery evidence without waiting behind an in-flight publisher.
  /// A busy publisher is represented by no workspace snapshot; the owner
  /// snapshot still forces authoritative reconciliation.
  pub fn try_emergency_workspace_snapshot(&self) -> Result<Option<NativeIndexRuntimeWorkspaceSnapshotV1>, IndexRuntimeCadenceErrorV1> {
    let publisher = match self.publisher.try_lock() {
      Ok(publisher) => publisher,
      Err(TryLockError::WouldBlock) => return Ok(None),
      Err(TryLockError::Poisoned(_)) => return Err(IndexRuntimeCadenceErrorV1::PublisherPoisoned),
    };
    Ok(Some(NativeIndexRuntimeWorkspaceSnapshotV1 {
      identity: publisher.workspace_identity(),
      path: publisher.workspace_path().to_path_buf(),
      selected_head: publisher.workspace_head().map(|head| head.selected_descriptor()),
    }))
  }
}

#[cfg(test)]
#[path = "../../../spec/engine/index_runtime_cadence_internal_spec.rs"]
mod index_runtime_cadence_internal_spec;
