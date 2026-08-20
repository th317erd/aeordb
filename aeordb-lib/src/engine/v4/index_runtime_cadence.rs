//! Single serialized cadence for due v4 runtime publication.
//!
//! Scheduling remains owned by the existing server index timer. This type
//! only serializes access to the mutable selector-last publisher and delegates
//! count, age, and memory-pressure decisions to the runtime owner.

use std::sync::{Arc, Mutex};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::engine::VirtualClock;

use super::index_runtime_owner::{IndexRuntimeBatchPublisherV1, IndexRuntimeErrorV1, IndexRuntimeFlushOutcomeV1, IndexRuntimeOwnerV1};
use super::index_runtime_batch_publisher::NativeIndexRuntimeBatchPublisherV1;

pub type NativeIndexRuntimeCadenceV1 = IndexRuntimeCadenceV1<NativeIndexRuntimeBatchPublisherV1>;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IndexRuntimeCadenceErrorV1 {
  #[error("index runtime cadence clock returned a zero timestamp")]
  InvalidClock,
  #[error("index runtime cadence publisher lock is poisoned")]
  PublisherPoisoned,
  #[error("index runtime cadence failed ({failure}) and could not latch degraded state: {source}")]
  FailureLatch { failure: &'static str, source: IndexRuntimeErrorV1 },
  #[error(transparent)]
  Runtime(#[from] IndexRuntimeErrorV1),
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
    let mut publisher = self.publisher.lock().map_err(|_| {
      self.fail_closed(
        IndexRuntimeCadenceErrorV1::PublisherPoisoned,
        "cadence_publisher_poisoned",
        "index runtime cadence publisher lock was poisoned; queued index work was retained",
      )
    })?;
    let cancelled = self.cancellation.is_cancelled();
    Ok(self.owner.flush(now_ms, None, cancelled, &mut *publisher)?)
  }

  fn fail_closed(&self, failure: IndexRuntimeCadenceErrorV1, code: &'static str, context: &str) -> IndexRuntimeCadenceErrorV1 {
    match self.owner.latch_cadence_failure(code, context.to_string()) {
      Ok(()) => failure,
      Err(source) => IndexRuntimeCadenceErrorV1::FailureLatch { failure: code, source },
    }
  }
}

#[cfg(test)]
#[path = "../../../spec/engine/index_runtime_cadence_internal_spec.rs"]
mod index_runtime_cadence_internal_spec;
