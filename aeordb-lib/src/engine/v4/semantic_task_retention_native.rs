//! Provisional all-task graph traversal, never durable retention authority.
use super::*;

#[derive(Clone, Copy, Debug)]
pub struct NativeSemanticTaskRetentionBoundsV1 {
  /// Combined discovery, graph and source-catalog logical work.
  pub maximum_work: u64,
  /// Actual physical reads across discovery and every selected task graph.
  pub maximum_read_bytes: u64,
  pub graphs: NativeSemanticTaskGraphBoundsV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemanticTaskRetentionSummaryV1 {
  pub tasks: u64,
  pub work: u64,
  pub read_bytes: u64,
  /// One captured task inventory and its graphs, not complete global GC.
  pub complete: bool,
}

/// Private enclosing admission, not an authority or replenishable budget.
pub(super) trait TaskRetentionAdmissionV1 {
  fn admit_work(&self, count: u64) -> Result<(), FirstAuthorityPublicationErrorV1>;
  fn admit_read_bytes(&self, bytes: u64) -> Result<(), FirstAuthorityPublicationErrorV1>;
}

struct RetentionBudgetV1 {
  maximum_work: u64,
  maximum_read_bytes: u64,
  work: Cell<u64>,
  read_bytes: Cell<u64>,
}

impl TaskRetentionAdmissionV1 for RetentionBudgetV1 {
  fn admit_work(&self, count: u64) -> Result<(), FirstAuthorityPublicationErrorV1> {
    let next = self.work.get().checked_add(count).filter(|next| *next <= self.maximum_work).ok_or_else(|| {
      FirstAuthorityPublicationErrorV1::invalid(
        "semantic_task_retention_work_bound",
        "captured task retention exhausted its combined work limit",
      )
    })?;
    self.work.set(next);
    Ok(())
  }

  fn admit_read_bytes(&self, bytes: u64) -> Result<(), FirstAuthorityPublicationErrorV1> {
    let next = self.read_bytes.get().checked_add(bytes).filter(|next| *next <= self.maximum_read_bytes).ok_or_else(|| {
      FirstAuthorityPublicationErrorV1::invalid(
        "semantic_task_retention_read_bound",
        "captured task retention exhausted its combined read-byte limit",
      )
    })?;
    self.read_bytes.set(next);
    Ok(())
  }
}

impl NativeSemanticMutationInventoryV1<'_> {
  /// Physical references may repeat; all callbacks remain provisional until
  /// successful completion. Ordinary chunk payloads remain opaque. This result
  /// grants no mark, resume, release, publication or activation capability.
  pub fn visit_captured_semantic_task_retention_entries(
    &self,
    bounds: NativeSemanticTaskRetentionBoundsV1,
    mut visitor: impl FnMut(&KVEntry) -> Result<(), SemanticMutationObservationErrorV1>,
  ) -> Result<SemanticTaskRetentionSummaryV1, SemanticTaskGraphErrorV1> {
    check_cancelled(&self.cancellation)?;
    self._memory.check_admission()?;
    if bounds.maximum_work == 0 || bounds.maximum_read_bytes == 0 {
      return Err(
        invalid("semantic_task_retention_bounds", "captured task retention requires positive combined work and byte limits").into(),
      );
    }
    let budget = RetentionBudgetV1 {
      maximum_work: bounds.maximum_work.min(self.bounds.maximum_work),
      maximum_read_bytes: bounds.maximum_read_bytes.min(self.bounds.maximum_read_bytes),
      work: Cell::new(0),
      read_bytes: Cell::new(0),
    };
    let mut graph_failure = None;
    let discovery = self.visit_entries(
      |observation| {
        let result: Result<(), SemanticTaskGraphErrorV1> = (|| {
          let task =
            observation.task()?.ok_or_else(|| invalid("semantic_task_retention_selection", "captured task selection is absent"))?;
          let task_id = task.task_id.try_into().map_err(|source| {
            FormatError::new(
              MalformedInputClass::IdentityKeyOrGenerationMismatch,
              "semantic_task_retention_identity",
              format!("captured task identity must be sixteen bytes: {source}"),
            )
          })?;
          self.visit_captured_task_entries(task_id, bounds.graphs, &mut visitor, false, Some(&budget))?;
          Ok(())
        })();
        match result {
          Ok(()) => Ok(true),
          Err(original) => {
            graph_failure = Some(original);
            Ok(false)
          }
        }
      },
      true,
      Some(&budget),
    );
    // Preserve the original graph/callback error even when stopping the
    // discovery observes simultaneous cancellation or memory pressure.
    if let Some(original) = graph_failure {
      return Err(original);
    }
    let discovery = discovery?;
    if !discovery.complete {
      return Err(invalid("semantic_task_retention_incomplete", "captured task discovery did not complete").into());
    }
    check_cancelled(&self.cancellation)?;
    self._memory.check_admission()?;
    Ok(SemanticTaskRetentionSummaryV1 {
      tasks: discovery.tasks,
      work: budget.work.get(),
      read_bytes: budget.read_bytes.get(),
      complete: true,
    })
  }
}
