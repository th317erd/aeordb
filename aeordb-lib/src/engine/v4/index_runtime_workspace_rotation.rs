//! Streaming classification for bounded index-runtime workspace rotation.
//!
//! A cumulative workspace is recovery evidence, not a completion ledger. This
//! planner permits history to be discarded only behind an exact selected
//! immutable-coverage frontier and retains unresolved batches plus the sole
//! producer coordinator's canonical pending task set.

use thiserror::Error;

use crate::engine::HashAlgorithm;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexRuntimeImmutableCoverageProofV1<'a> {
  pub runtime_id: [u8; 16],
  pub generation: u64,
  pub source_namespace_root: &'a [u8],
  pub coverage_epoch_id: [u8; 16],
  pub covered_through_publication_sequence: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexRuntimeWorkspaceRotationEntryV1 {
  RuntimeBatch { sequence: u64, object_id: [u8; 16], minimum_publication_sequence: u64, maximum_publication_sequence: u64 },
  ProducerTask { sequence: u64, object_id: [u8; 16], operation_id: [u8; 16], publication_sequence: u64 },
}

impl IndexRuntimeWorkspaceRotationEntryV1 {
  pub const fn runtime_batch(
    sequence: u64,
    object_id: [u8; 16],
    minimum_publication_sequence: u64,
    maximum_publication_sequence: u64,
  ) -> Self {
    Self::RuntimeBatch { sequence, object_id, minimum_publication_sequence, maximum_publication_sequence }
  }

  pub const fn producer_task(sequence: u64, object_id: [u8; 16], operation_id: [u8; 16], publication_sequence: u64) -> Self {
    Self::ProducerTask { sequence, object_id, operation_id, publication_sequence }
  }

  pub const fn sequence(self) -> u64 {
    match self {
      Self::RuntimeBatch { sequence, .. } | Self::ProducerTask { sequence, .. } => sequence,
    }
  }

  const fn object_id(self) -> [u8; 16] {
    match self {
      Self::RuntimeBatch { object_id, .. } | Self::ProducerTask { object_id, .. } => object_id,
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexRuntimeWorkspaceRotationDispositionV1 {
  DiscardRepresented,
  RetainUnresolvedBatch,
  RetainPendingTask,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexRuntimeWorkspaceRotationSummaryV1 {
  pub observed_objects: u64,
  pub discarded_objects: u64,
  pub retained_runtime_batches: u64,
  pub retained_pending_tasks: u64,
  retained_objects: u64,
}

impl IndexRuntimeWorkspaceRotationSummaryV1 {
  pub const fn retained_objects(self) -> u64 {
    self.retained_objects
  }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IndexRuntimeWorkspaceRotationErrorV1 {
  #[error("index runtime workspace rotation input is invalid: {0}")]
  Invalid(&'static str),
  #[error("index runtime workspace coverage belongs to another runtime, generation, or source root")]
  ForeignCoverage,
  #[error("index runtime workspace rotation was canceled")]
  Canceled,
  #[error("index runtime workspace inventory sequence is discontinuous: expected {expected}, observed {observed}")]
  InventorySequence { expected: u64, observed: u64 },
  #[error("index runtime workspace inventory is incomplete: expected {expected}, observed {observed}")]
  InventoryIncomplete { expected: u64, observed: u64 },
  #[error("pending producer task {operation_id:?} is already represented by selected immutable coverage")]
  PendingTaskAlreadyCovered { operation_id: [u8; 16] },
  #[error("completed producer task {operation_id:?} is not represented by selected immutable coverage")]
  UnprovenCompletedTask { operation_id: [u8; 16] },
  #[error("pending producer task {operation_id:?} is absent from the selected workspace")]
  PendingTaskMissing { operation_id: [u8; 16] },
  #[error("pending producer task {operation_id:?} appears more than once in the selected workspace")]
  DuplicatePendingTask { operation_id: [u8; 16] },
  #[error("index runtime workspace rotation allocation failed: {0}")]
  Allocation(String),
  #[error("index runtime workspace rotation accounting overflowed")]
  AccountingOverflow,
}

pub struct IndexRuntimeWorkspaceRotationPlannerV1<'a> {
  selected_object_count: u64,
  covered_through_publication_sequence: u64,
  pending_operation_ids: &'a [[u8; 16]],
  matched_pending: Vec<bool>,
  next_sequence: u64,
  summary: IndexRuntimeWorkspaceRotationSummaryV1,
  is_cancelled: &'a dyn Fn() -> bool,
}

impl<'a> IndexRuntimeWorkspaceRotationPlannerV1<'a> {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    hash_algorithm: HashAlgorithm,
    runtime_id: [u8; 16],
    generation: u64,
    source_namespace_root: &[u8],
    selected_object_count: u64,
    coverage: IndexRuntimeImmutableCoverageProofV1<'_>,
    pending_operation_ids: &'a [[u8; 16]],
    is_cancelled: &'a dyn Fn() -> bool,
  ) -> Result<Self, IndexRuntimeWorkspaceRotationErrorV1> {
    if is_cancelled() {
      return Err(IndexRuntimeWorkspaceRotationErrorV1::Canceled);
    }
    let hash_width = hash_algorithm.hash_length();
    if runtime_id.iter().all(|byte| *byte == 0)
      || generation == 0
      || source_namespace_root.len() != hash_width
      || source_namespace_root.iter().all(|byte| *byte == 0)
      || selected_object_count == 0
      || coverage.coverage_epoch_id.iter().all(|byte| *byte == 0)
      || coverage.covered_through_publication_sequence == 0
    {
      return Err(IndexRuntimeWorkspaceRotationErrorV1::Invalid(
        "runtime identity, generation, source root, selected object count, or coverage frontier is incomplete",
      ));
    }
    if coverage.runtime_id != runtime_id
      || coverage.generation != generation
      || coverage.source_namespace_root != source_namespace_root
      || coverage.source_namespace_root.len() != hash_width
    {
      return Err(IndexRuntimeWorkspaceRotationErrorV1::ForeignCoverage);
    }
    const { assert!(usize::BITS <= u64::BITS) };
    let pending_count = pending_operation_ids.len() as u64;
    if pending_count > selected_object_count {
      return Err(IndexRuntimeWorkspaceRotationErrorV1::Invalid("pending task count exceeds the selected workspace object count"));
    }
    let mut previous = None;
    for operation_id in pending_operation_ids {
      if operation_id.iter().all(|byte| *byte == 0) || previous.is_some_and(|previous| previous >= operation_id) {
        return Err(IndexRuntimeWorkspaceRotationErrorV1::Invalid(
          "pending operation identities must be nonzero and in strict canonical order",
        ));
      }
      previous = Some(operation_id);
    }
    let mut matched_pending = Vec::new();
    matched_pending
      .try_reserve_exact(pending_operation_ids.len())
      .map_err(|error| IndexRuntimeWorkspaceRotationErrorV1::Allocation(error.to_string()))?;
    matched_pending.resize(pending_operation_ids.len(), false);
    Ok(Self {
      selected_object_count,
      covered_through_publication_sequence: coverage.covered_through_publication_sequence,
      pending_operation_ids,
      matched_pending,
      next_sequence: 1,
      summary: IndexRuntimeWorkspaceRotationSummaryV1::default(),
      is_cancelled,
    })
  }

  pub fn observe(
    &mut self,
    entry: IndexRuntimeWorkspaceRotationEntryV1,
  ) -> Result<IndexRuntimeWorkspaceRotationDispositionV1, IndexRuntimeWorkspaceRotationErrorV1> {
    self.check_cancellation()?;
    let observed = entry.sequence();
    if observed != self.next_sequence || observed > self.selected_object_count {
      return Err(IndexRuntimeWorkspaceRotationErrorV1::InventorySequence { expected: self.next_sequence, observed });
    }
    if entry.object_id().iter().all(|byte| *byte == 0) {
      return Err(IndexRuntimeWorkspaceRotationErrorV1::Invalid("workspace object identity is zero"));
    }
    let disposition = match entry {
      IndexRuntimeWorkspaceRotationEntryV1::RuntimeBatch { minimum_publication_sequence, maximum_publication_sequence, .. } => {
        if minimum_publication_sequence == 0 || maximum_publication_sequence < minimum_publication_sequence {
          return Err(IndexRuntimeWorkspaceRotationErrorV1::Invalid("runtime batch publication interval is invalid"));
        }
        if maximum_publication_sequence <= self.covered_through_publication_sequence {
          IndexRuntimeWorkspaceRotationDispositionV1::DiscardRepresented
        } else {
          IndexRuntimeWorkspaceRotationDispositionV1::RetainUnresolvedBatch
        }
      }
      IndexRuntimeWorkspaceRotationEntryV1::ProducerTask { operation_id, publication_sequence, .. } => {
        if operation_id.iter().all(|byte| *byte == 0) || publication_sequence == 0 {
          return Err(IndexRuntimeWorkspaceRotationErrorV1::Invalid("producer task identity or publication sequence is zero"));
        }
        let pending_index = self.pending_operation_ids.partition_point(|pending| pending < &operation_id);
        if self.pending_operation_ids.get(pending_index) == Some(&operation_id) {
          if publication_sequence <= self.covered_through_publication_sequence {
            return Err(IndexRuntimeWorkspaceRotationErrorV1::PendingTaskAlreadyCovered { operation_id });
          }
          if self.matched_pending[pending_index] {
            return Err(IndexRuntimeWorkspaceRotationErrorV1::DuplicatePendingTask { operation_id });
          }
          self.matched_pending[pending_index] = true;
          IndexRuntimeWorkspaceRotationDispositionV1::RetainPendingTask
        } else if publication_sequence > self.covered_through_publication_sequence {
          return Err(IndexRuntimeWorkspaceRotationErrorV1::UnprovenCompletedTask { operation_id });
        } else {
          IndexRuntimeWorkspaceRotationDispositionV1::DiscardRepresented
        }
      }
    };
    self.summary.observed_objects = checked_increment(self.summary.observed_objects)?;
    match disposition {
      IndexRuntimeWorkspaceRotationDispositionV1::DiscardRepresented => {
        self.summary.discarded_objects = checked_increment(self.summary.discarded_objects)?;
      }
      IndexRuntimeWorkspaceRotationDispositionV1::RetainUnresolvedBatch => {
        self.summary.retained_runtime_batches = checked_increment(self.summary.retained_runtime_batches)?;
        self.summary.retained_objects = checked_increment(self.summary.retained_objects)?;
      }
      IndexRuntimeWorkspaceRotationDispositionV1::RetainPendingTask => {
        self.summary.retained_pending_tasks = checked_increment(self.summary.retained_pending_tasks)?;
        self.summary.retained_objects = checked_increment(self.summary.retained_objects)?;
      }
    }
    self.next_sequence = self.next_sequence.checked_add(1).ok_or(IndexRuntimeWorkspaceRotationErrorV1::AccountingOverflow)?;
    Ok(disposition)
  }

  pub fn finish(self) -> Result<IndexRuntimeWorkspaceRotationSummaryV1, IndexRuntimeWorkspaceRotationErrorV1> {
    self.check_cancellation()?;
    if self.summary.observed_objects != self.selected_object_count {
      return Err(IndexRuntimeWorkspaceRotationErrorV1::InventoryIncomplete {
        expected: self.selected_object_count,
        observed: self.summary.observed_objects,
      });
    }
    if let Some(index) = self.matched_pending.iter().position(|matched| !matched) {
      return Err(IndexRuntimeWorkspaceRotationErrorV1::PendingTaskMissing { operation_id: self.pending_operation_ids[index] });
    }
    Ok(self.summary)
  }

  fn check_cancellation(&self) -> Result<(), IndexRuntimeWorkspaceRotationErrorV1> {
    if (self.is_cancelled)() {
      Err(IndexRuntimeWorkspaceRotationErrorV1::Canceled)
    } else {
      Ok(())
    }
  }
}

fn checked_increment(value: u64) -> Result<u64, IndexRuntimeWorkspaceRotationErrorV1> {
  value.checked_add(1).ok_or(IndexRuntimeWorkspaceRotationErrorV1::AccountingOverflow)
}
