//! Owned native source sinks; never selected task, restart or activation authority.
#[path = "semantic_captured_checkpoint_staging.rs"]
mod captured_checkpoint;
pub use captured_checkpoint::NativeCapturedSemanticCheckpointRequestV1;
#[path = "semantic_initial_task_selection.rs"]
mod initial_task;
pub use initial_task::{NativeInitialSemanticTaskSelectionErrorV1, NativeInitialSemanticTaskSelectionRequestV1};
use super::*;
use std::cell::RefCell;
use crate::engine::v4::plugin_identity::ALIAS_MAX_LENGTH;

#[derive(Clone, Copy, Debug)]
pub struct NativeSemanticSourceUnionStagingRequestV1 {
  pub publication_timestamp_ms: u64,
  pub maximum_source_copy_attempts: u64,
  /// Logical node/control-body and FileRecord payload attempts, including reuse.
  /// This does not meter the physical publisher's KV flush or wrapper I/O.
  pub maximum_payload_bytes: u64,
  /// Cumulative live chunk validation across retained-source copy attempts.
  /// Source discovery keeps the union's separate cumulative read/work budget.
  pub maximum_validation_read_bytes: u64,
  pub maximum_node_workspace_bytes: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SemanticSourceUnionStagingSummaryV1 {
  pub node_pair_attempts: u64,
  pub node_control_attempts: u64,
  pub source_copy_attempts: u64,
  pub attempted_payload_bytes: u64,
  pub validation_read_bytes: u64,
}

pub struct NativeStagedSemanticSourceUnionV1<'a> {
  union: NativeSemanticSourceUnionV1<'a>,
  summary: SemanticSourceUnionStagingSummaryV1,
  _memory: MemoryReservation,
}

impl NativeStagedSemanticSourceUnionV1<'_> {
  pub fn source_union(&self) -> &NativeSemanticSourceUnionV1<'_> {
    &self.union
  }

  pub fn summary(&self) -> SemanticSourceUnionStagingSummaryV1 {
    self.summary
  }
}

impl NativeSemanticMutationInventoryV1<'_> {
  /// Own both real sinks. Failures may leave unselected immutable dependencies;
  /// this operation never selects a task or claims atomic rollback of staging.
  /// The result retains protection through its source union, not across restart.
  pub fn prepare_and_stage_semantic_source_union(
    &self,
    request: NativeSemanticSourceUnionRequestV1<'_>,
    staging: NativeSemanticSourceUnionStagingRequestV1,
  ) -> Result<NativeStagedSemanticSourceUnionV1<'_>, NativeSemanticSourceUnionErrorV1> {
    self.prepare_and_stage_semantic_source_union_observed(request, staging, &mut NoopFirstAuthorityDependencyObserverV1)
  }

  pub(in crate::engine::v4::first_authority) fn prepare_and_stage_semantic_source_union_observed(
    &self,
    request: NativeSemanticSourceUnionRequestV1<'_>,
    staging: NativeSemanticSourceUnionStagingRequestV1,
    observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<NativeStagedSemanticSourceUnionV1<'_>, NativeSemanticSourceUnionErrorV1> {
    check_cancelled(&self.cancellation)?;
    self._memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    if staging.publication_timestamp_ms == 0 || staging.publication_timestamp_ms > i64::MAX as u64 {
      return Err(invalid("semantic_source_staging_time", "source staging time must fit the signed persistent range").into());
    }
    if staging.maximum_node_workspace_bytes == 0 {
      return Err(invalid("semantic_source_staging_workspace", "node staging requires a positive workspace ceiling").into());
    }
    // Fixed owner/counter/result scratch; all retained variable-size bytes stay
    // with the union and its existing per-source/node reservations.
    let memory =
      self.memory.reserve(MemoryOwner::Task, 256, AdmissionClass::Maintenance).map_err(SemanticMutationObservationErrorV1::from)?;
    let maximum_node_pairs = request.bounds.maximum_catalog_node_pairs;
    let source_bounds = NativeSemanticSourceReadBoundsV1 {
      maximum_body_bytes: request
        .bounds
        .namespace
        .sources
        .maximum_body_bytes
        .max(request.bounds.maximum_plugin_module_bytes)
        .max(ALIAS_MAX_LENGTH),
      ..request.bounds.namespace.sources
    };
    let summary = Cell::new(SemanticSourceUnionStagingSummaryV1::default());
    let observer = RefCell::new(observer);
    let union = self.prepare_semantic_source_union(
      request,
      |left, right| {
        check_cancelled(&self.cancellation)?;
        memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
        let mut next = summary.get();
        next.node_pair_attempts = charge_staging(next.node_pair_attempts, 1, maximum_node_pairs, "semantic_source_staging_node_bound")?;
        let distinct = left != right;
        next.node_control_attempts =
          charge_staging(next.node_control_attempts, if distinct { 2 } else { 1 }, u64::MAX, "semantic_source_staging_node_bound")?;
        next.attempted_payload_bytes = charge_staging(
          next.attempted_payload_bytes,
          left.len() as u64,
          staging.maximum_payload_bytes,
          "semantic_source_staging_payload_bound",
        )?;
        if distinct {
          next.attempted_payload_bytes = charge_staging(
            next.attempted_payload_bytes,
            right.len() as u64,
            staging.maximum_payload_bytes,
            "semantic_source_staging_payload_bound",
          )?;
        }
        summary.set(next);
        let receipt = self.stage_semantic_source_nodes_observed(
          NativeSemanticSourceNodeStagingRequestV1 {
            encoded_nodes: &[left, right],
            publication_timestamp_ms: staging.publication_timestamp_ms,
            maximum_workspace_bytes: staging.maximum_node_workspace_bytes,
          },
          || {},
          &mut **observer.borrow_mut(),
        )?;
        // Dependency success is provisional until the complete union finishes.
        // A committed error above retains the physical owner's original receipt.
        drop(receipt);
        Ok(())
      },
      |_, base, requested| {
        for (index, source) in [base, requested].into_iter().enumerate() {
          let Some(source) = source else { continue };
          if index == 1 && base.is_some_and(|base| base.revision() == source.revision()) {
            continue;
          }
          check_cancelled(&self.cancellation)?;
          memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
          let mut next = summary.get();
          next.source_copy_attempts =
            charge_staging(next.source_copy_attempts, 1, staging.maximum_source_copy_attempts, "semantic_source_staging_copy_bound")?;
          next.attempted_payload_bytes = charge_staging(
            next.attempted_payload_bytes,
            source.encoded_record().len() as u64,
            staging.maximum_payload_bytes,
            "semantic_source_staging_payload_bound",
          )?;
          let remaining = staging
            .maximum_validation_read_bytes
            .checked_sub(next.validation_read_bytes)
            .ok_or_else(|| staging_bound("semantic_source_read_bound"))?;
          let allowance = remaining.min(source_bounds.maximum_read_bytes);
          if allowance == 0 && !source.record().chunk_hashes.is_empty() {
            return Err(staging_bound("semantic_source_read_bound").into());
          }
          // The shared helper requires a positive limit even for zero chunks;
          // that empty case actually consumes zero validation bytes.
          let bounds = NativeSemanticSourceReadBoundsV1 { maximum_read_bytes: allowance.max(1), ..source_bounds };
          let (receipt, bytes) =
            source.stage_retained_copy_metered_observed(bounds, staging.publication_timestamp_ms, || {}, &mut **observer.borrow_mut())?;
          drop(receipt);
          next.validation_read_bytes =
            charge_staging(next.validation_read_bytes, bytes, staging.maximum_validation_read_bytes, "semantic_source_read_bound")?;
          summary.set(next);
        }
        Ok(())
      },
    )?;
    check_cancelled(&self.cancellation)?;
    memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    Ok(NativeStagedSemanticSourceUnionV1 { union, summary: summary.get(), _memory: memory })
  }
}

fn staging_bound(code: &'static str) -> SemanticMutationObservationErrorV1 {
  SemanticMutationObservationErrorV1::Resource { code, message: "source staging exceeds its cumulative admitted budget" }
}

fn charge_staging(current: u64, amount: u64, maximum: u64, code: &'static str) -> Result<u64, SemanticMutationObservationErrorV1> {
  current.checked_add(amount).filter(|next| *next <= maximum).ok_or_else(|| staging_bound(code))
}
