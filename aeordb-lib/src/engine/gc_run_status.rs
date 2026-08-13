use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwapOption;
use serde::Serialize;

use crate::engine::engine_event::{EngineEvent, EVENT_GC_STATUS};
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::event_bus::EventBus;
use crate::engine::v4::gc_run::{GcRunProgressSinkV1, GcRunStateV1, GcRunStatusV1};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GcRunStatusSnapshotV1 {
  #[serde(flatten)]
  pub status: GcRunStatusV1,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub task_id: Option<String>,
}

#[derive(Debug)]
struct RetainedGcRunStatusV1 {
  projection_sequence: u64,
  snapshot: GcRunStatusSnapshotV1,
}

#[derive(Debug, Default)]
pub struct GcRunStatusRegistryV1 {
  next_projection_sequence: AtomicU64,
  current: ArcSwapOption<RetainedGcRunStatusV1>,
}

impl GcRunStatusRegistryV1 {
  pub fn latest(&self) -> Option<GcRunStatusSnapshotV1> {
    self.current.load_full().map(|retained| retained.snapshot.clone())
  }

  pub fn for_task(&self, task_id: &str) -> Option<GcRunStatusSnapshotV1> {
    self.latest().filter(|snapshot| snapshot.task_id.as_deref() == Some(task_id))
  }

  pub(crate) fn projection_sink(
    self: &Arc<Self>,
    task_id: Option<String>,
    event_bus: Option<Arc<EventBus>>,
    observer: Option<Arc<dyn GcRunProgressSinkV1>>,
  ) -> EngineResult<Arc<dyn GcRunProgressSinkV1>> {
    if let Some(task_id) = task_id.as_deref() {
      if let Err(error) = uuid::Uuid::parse_str(task_id) {
        return Err(EngineError::InvalidInput(format!("GC task identity must be a UUID: {error}")));
      }
    }
    let prior_projection_sequence =
      match self.next_projection_sequence.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| current.checked_add(1)) {
        Ok(prior_projection_sequence) => prior_projection_sequence,
        Err(exhausted_projection_sequence) => {
          return Err(EngineError::ResourceExhausted(format!(
            "GC status projection sequence is exhausted at {exhausted_projection_sequence}"
          )));
        }
      };
    let projection_sequence = prior_projection_sequence + 1;
    Ok(Arc::new(GcRunStatusProjectionSinkV1 { registry: Arc::clone(self), projection_sequence, task_id, event_bus, observer }))
  }

  fn publish(&self, projection_sequence: u64, snapshot: GcRunStatusSnapshotV1) -> Option<Option<GcRunStatusSnapshotV1>> {
    loop {
      let current = self.current.load_full();
      if let Some(retained) = current.as_ref() {
        if projection_sequence < retained.projection_sequence {
          return None;
        }
        if projection_sequence == retained.projection_sequence
          && (snapshot.status.run_id != retained.snapshot.status.run_id
            || snapshot.status.observed_at_ms < retained.snapshot.status.observed_at_ms
            || (is_terminal(retained.snapshot.status.state) && !is_terminal(snapshot.status.state)))
        {
          return None;
        }
      }
      let previous = current.as_ref().map(|retained| retained.snapshot.clone());
      let replacement = Some(Arc::new(RetainedGcRunStatusV1 { projection_sequence, snapshot: snapshot.clone() }));
      let observed = self.current.compare_and_swap(&current, replacement);
      if same_retained_projection(observed.as_ref(), current.as_ref()) {
        return Some(previous);
      }
    }
  }
}

fn same_retained_projection(left: Option<&Arc<RetainedGcRunStatusV1>>, right: Option<&Arc<RetainedGcRunStatusV1>>) -> bool {
  match (left, right) {
    (Some(left), Some(right)) => Arc::ptr_eq(left, right),
    (None, None) => true,
    (Some(_), None) | (None, Some(_)) => false,
  }
}

struct GcRunStatusProjectionSinkV1 {
  registry: Arc<GcRunStatusRegistryV1>,
  projection_sequence: u64,
  task_id: Option<String>,
  event_bus: Option<Arc<EventBus>>,
  observer: Option<Arc<dyn GcRunProgressSinkV1>>,
}

impl GcRunProgressSinkV1 for GcRunStatusProjectionSinkV1 {
  fn publish(&self, status: &GcRunStatusV1) {
    let snapshot = GcRunStatusSnapshotV1 { status: status.clone(), task_id: self.task_id.clone() };
    if let Some(previous) = self.registry.publish(self.projection_sequence, snapshot.clone()) {
      log_transition(previous.as_ref(), &snapshot);
      if let Some(event_bus) = &self.event_bus {
        event_bus.emit(EngineEvent::new(EVENT_GC_STATUS, "system", serde_json::json!(snapshot)));
      }
    }
    if let Some(observer) = &self.observer {
      observer.publish(status);
    }
  }
}

fn is_terminal(state: GcRunStateV1) -> bool {
  state != GcRunStateV1::Running
}

fn log_transition(previous: Option<&GcRunStatusSnapshotV1>, snapshot: &GcRunStatusSnapshotV1) {
  let status = &snapshot.status;
  if let Some(previous) = previous {
    if previous.status.state == status.state && previous.status.phase == status.phase {
      return;
    }
  }
  let run_id = status.run_id.to_string();
  let phase = match status.phase {
    Some(phase) => phase.name(),
    None => "none",
  };
  if status.state == GcRunStateV1::Running {
    tracing::info!(run_id, task_id = snapshot.task_id.as_deref(), invocation = ?status.invocation, mode = ?status.mode, phase, "GC run entered phase");
  } else {
    tracing::info!(
      run_id,
      task_id = snapshot.task_id.as_deref(),
      invocation = ?status.invocation,
      mode = ?status.mode,
      state = ?status.state,
      phase,
      code = status.code.as_deref(),
      "GC run reached terminal state"
    );
  }
}

#[cfg(test)]
#[path = "../../spec/engine/gc_run_status_internal_spec.rs"]
mod internal_spec;
