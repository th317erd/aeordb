//! A task-only mark tied to its captured layout; never global GC authority.
#[path = "semantic_task_root_exclusion.rs"]
mod root_exclusion;
pub use root_exclusion::NativeSemanticTaskRootExclusionV1;
#[path = "semantic_task_physical_exclusion.rs"]
mod physical_exclusion;
pub use physical_exclusion::{NativeSemanticTaskPhysicalExclusionBoundsV1, NativeSemanticTaskPhysicalExclusionV1};
use super::*;
use crate::engine::kv_pages::MAX_ENTRIES_PER_PAGE;
use crate::engine::v4::gc_mark_runtime::{DenseMarkBitmapV1, MarkBitmapErrorV1};

#[derive(Clone, Copy, Debug)]
pub struct NativeSemanticTaskMarkBoundsV1 {
  pub retention: NativeSemanticTaskRetentionBoundsV1,
  pub maximum_slot_lookups: u64,
  /// Logical full-page charges, including cached and repeated resolutions.
  pub maximum_slot_page_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemanticTaskMarkSummaryV1 {
  pub retention: SemanticTaskRetentionSummaryV1,
  pub marked_slots: u64,
  pub slot_lookups: u64,
  pub slot_page_bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum SemanticTaskMarkErrorV1 {
  #[error("captured task mark geometry: {0}")]
  Geometry(#[source] std::num::TryFromIntError),
  #[error("captured task mark traversal: {0}")]
  Graph(#[from] SemanticTaskGraphErrorV1),
  #[error("captured task mark bitmap: {0}")]
  Bitmap(#[from] MarkBitmapErrorV1),
  #[error("captured task mark observation: {0}")]
  Observation(#[from] SemanticMutationObservationErrorV1),
}

impl SemanticTaskMarkErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Geometry(_) => "semantic_task_mark_geometry",
      Self::Graph(source) => source.code(),
      Self::Bitmap(source) => source.code(),
      Self::Observation(source) => source.code(),
    }
  }
}

impl From<MemoryCoordinatorError> for SemanticTaskMarkErrorV1 {
  fn from(source: MemoryCoordinatorError) -> Self {
    Self::Observation(source.into())
  }
}

impl From<EngineError> for SemanticTaskMarkErrorV1 {
  fn from(source: EngineError) -> Self {
    Self::Observation(FirstAuthorityPublicationErrorV1::from(source).into())
  }
}

/// Its borrow keeps the snapshot and staging protection alive. This task-only
/// contribution grants no task selection, global completion or reclaim permit.
pub struct NativeSemanticTaskMarkV1<'capture, 'protection> {
  capture: &'capture NativeSemanticMutationInventoryV1<'protection>,
  bitmap: DenseMarkBitmapV1,
  summary: SemanticTaskMarkSummaryV1,
}

impl fmt::Debug for NativeSemanticTaskMarkV1<'_, '_> {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("NativeSemanticTaskMarkV1").field("summary", &self.summary).finish_non_exhaustive()
  }
}

impl NativeSemanticTaskMarkV1<'_, '_> {
  pub fn summary(&self) -> &SemanticTaskMarkSummaryV1 {
    &self.summary
  }

  pub fn bitmap_bytes(&self) -> &[u8] {
    self.bitmap.bytes()
  }

  /// One separately admitted captured-page lookup. Every locator component must
  /// match; a set bit for a newer or older incarnation is not a match.
  pub fn is_captured_locator_marked(&self, locator: &KVEntry) -> Result<bool, SemanticTaskMarkErrorV1> {
    self.capture.check_mark_admission()?;
    let scratch = self.capture.reserve_mark_slot_scratch()?;
    let found = self.capture.snapshot.find_captured_slot(&locator.hash)?;
    self.capture.check_mark_admission()?;
    scratch.check_admission()?;
    match found {
      Some((position, captured)) if captured == *locator => Ok(self.bitmap.is_marked(position)?),
      _ => Ok(false),
    }
  }
}

impl<'protection> NativeSemanticMutationInventoryV1<'protection> {
  /// Build only the task contribution against this exact flushed capture.
  /// References may repeat and are charged every time. Failed traversal drops
  /// all provisional bits. No flush, writer, live lookup or global mark occurs.
  pub fn mark_captured_semantic_tasks(
    &self,
    bounds: NativeSemanticTaskMarkBoundsV1,
  ) -> Result<NativeSemanticTaskMarkV1<'_, 'protection>, SemanticTaskMarkErrorV1> {
    self.build_captured_semantic_task_mark(bounds, |_| Ok(()))
  }

  #[cfg(test)]
  pub(in crate::engine::v4::first_authority) fn mark_captured_semantic_tasks_observed_for_test(
    &self,
    bounds: NativeSemanticTaskMarkBoundsV1,
    after_lookup: impl FnMut(u64) -> Result<(), SemanticMutationObservationErrorV1>,
  ) -> Result<NativeSemanticTaskMarkV1<'_, 'protection>, SemanticTaskMarkErrorV1> {
    self.build_captured_semantic_task_mark(bounds, after_lookup)
  }

  fn build_captured_semantic_task_mark(
    &self,
    bounds: NativeSemanticTaskMarkBoundsV1,
    mut after_lookup: impl FnMut(u64) -> Result<(), SemanticMutationObservationErrorV1>,
  ) -> Result<NativeSemanticTaskMarkV1<'_, 'protection>, SemanticTaskMarkErrorV1> {
    self.check_mark_admission()?;
    if bounds.maximum_slot_lookups == 0 || bounds.maximum_slot_page_bytes == 0 {
      return Err(invalid("semantic_task_mark_bounds", "captured task mark requires positive slot lookup and page-byte limits").into());
    }
    // Refuse even an empty task inventory if any captured entry is buffered.
    if self.snapshot.buffer_len() != 0 {
      return Err(invalid("semantic_task_mark_buffered", "captured task mark requires an already flushed KV snapshot").into());
    }
    let bucket_count = u64::try_from(self.snapshot.bucket_count()).map_err(SemanticTaskMarkErrorV1::Geometry)?;
    let mut bitmap = DenseMarkBitmapV1::new(bucket_count, MAX_ENTRIES_PER_PAGE as u32, self.cancellation.clone(), &self.memory)?;
    let scratch = self.reserve_mark_slot_scratch()?;
    let page_bytes = page_size(self.snapshot.hash_algo().hash_length()) as u64;
    let mut slot_lookups = 0u64;
    let mut slot_page_bytes = 0u64;
    let mut original_failure = None;
    let retention = self.visit_captured_semantic_task_retention_entries(bounds.retention, |locator| {
      let result: Result<(), SemanticTaskMarkErrorV1> = (|| {
        self.check_mark_admission()?;
        scratch.check_admission()?;
        let next_lookups = slot_lookups
          .checked_add(1)
          .filter(|next| *next <= bounds.maximum_slot_lookups)
          .ok_or_else(|| invalid("semantic_task_mark_lookup_bound", "captured task mark exhausted its slot lookup limit"))?;
        let next_bytes = slot_page_bytes
          .checked_add(page_bytes)
          .filter(|next| *next <= bounds.maximum_slot_page_bytes)
          .ok_or_else(|| invalid("semantic_task_mark_page_bound", "captured task mark exhausted its logical page-byte limit"))?;
        slot_lookups = next_lookups;
        slot_page_bytes = next_bytes;
        let (position, captured) = self
          .snapshot
          .find_captured_slot(&locator.hash)?
          .ok_or_else(|| invalid("semantic_task_mark_locator", "task reference has no live slot in its captured layout"))?;
        if captured != *locator {
          return Err(invalid("semantic_task_mark_locator", "task reference disagrees with its captured physical locator").into());
        }
        self.check_mark_admission()?;
        scratch.check_admission()?;
        bitmap.mark(position)?;
        after_lookup(slot_lookups)?;
        Ok(())
      })();
      match result {
        Ok(()) => Ok(()),
        Err(source) => {
          original_failure = Some(source);
          Err(invalid("semantic_task_mark_callback", "captured task slot resolution failed"))
        }
      }
    });
    // Preserve the typed original slot/bitmap failure across graph unwinding,
    // including simultaneous cancellation or pressure at outer visitor checks.
    if let Some(source) = original_failure {
      return Err(source);
    }
    let retention = retention?;
    self.check_mark_admission()?;
    scratch.check_admission()?;
    let summary = SemanticTaskMarkSummaryV1 { retention, marked_slots: bitmap.marked_count(), slot_lookups, slot_page_bytes };
    drop(scratch);
    Ok(NativeSemanticTaskMarkV1 { capture: self, bitmap, summary })
  }

  fn check_mark_admission(&self) -> Result<(), SemanticTaskMarkErrorV1> {
    check_cancelled(&self.cancellation)?;
    self._memory.check_admission()?;
    Ok(())
  }

  fn reserve_mark_slot_scratch(&self) -> Result<MemoryReservation, SemanticTaskMarkErrorV1> {
    let bytes = (page_size(self.snapshot.hash_algo().hash_length()) as u64)
      .checked_mul(4)
      .ok_or_else(|| invalid("semantic_task_mark_memory_bound", "captured slot scratch exceeds its byte contract"))?;
    Ok(self.memory.reserve(MemoryOwner::GarbageCollection, bytes, AdmissionClass::Maintenance)?)
  }
}
