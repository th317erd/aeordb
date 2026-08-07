use serde::Serialize;
use serde_json::{json, Value};

use crate::engine::config_resolver::ConfigurationFamily;
use crate::engine::configuration_observability::{configuration_envelope, ConfigurationVisibility};
use crate::engine::durability_coordinator::{DurabilityBarrierObservation, DurabilityCoordinatorSnapshot, DurabilityGroupPolicySnapshot};
use crate::engine::errors::EngineResult;
use crate::engine::storage_engine::{DurabilityFailureState, EmergencySpillReport, EngineMemoryStats, StorageEngine};
use crate::engine::v4::durability_recovery::PersistentDurabilityRecoveryState;

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeObservabilitySnapshot {
  pub memory: EngineMemoryStats,
  pub durability: DurabilityObservabilitySnapshot,
  pub configuration: ConfigurationObservabilitySnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigurationObservabilitySnapshot {
  pub runtime: Value,
  pub lifecycle: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct DurabilityObservabilitySnapshot {
  pub frontier: DurabilityFrontierSnapshot,
  pub group_policy: DurabilityGroupPolicyObservability,
  pub latch: DurabilityLatchObservability,
  pub spill: DurabilitySpillObservability,
  pub repair: DurabilityRepairObservability,
}

#[derive(Debug, Clone, Serialize)]
pub struct DurabilityFrontierSnapshot {
  pub hard_frontier: u64,
  pub next_sequence: u64,
  pub waiter_depth: usize,
  pub admitted: usize,
  pub executing: usize,
  pub proven: usize,
  pub failed: usize,
  pub pending_hard: usize,
  pub driver_active: bool,
  pub oldest_waiter_age_ms: Option<u64>,
  pub last_barrier: Option<DurabilityBarrierObservation>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DurabilityGroupPolicyObservability {
  pub enabled: bool,
  pub max_bytes: Option<u64>,
  pub max_delay_ms: Option<u64>,
  pub disabled_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DurabilityLatchObservability {
  pub read_only: bool,
  pub runtime_failure: Option<DurabilityFailureState>,
  pub persistent_recovery: Option<PersistentDurabilityRecoveryState>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DurabilitySpillObservability {
  pub count: u64,
  pub total_bytes: u64,
  pub locations: Vec<Value>,
  pub latest: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DurabilityRepairObservability {
  pub required: bool,
  pub state: &'static str,
  pub command: Option<Value>,
  pub progress: Option<f64>,
}

pub fn collect_runtime_observability(
  engine: &StorageEngine,
  visibility: ConfigurationVisibility,
) -> EngineResult<RuntimeObservabilitySnapshot> {
  let memory = engine.memory_stats()?;
  let frontier = engine.durability_snapshot()?;
  let group_policy = engine.durability_group_policy_snapshot()?;
  let runtime_failure = engine.durability_failure_state();
  let persistent_recovery = engine.persistent_durability_recovery();
  let spill = engine.emergency_spill_report();
  let configuration = engine.configuration_snapshot();

  Ok(RuntimeObservabilitySnapshot {
    memory,
    durability: durability_observability(engine, frontier, group_policy, runtime_failure, persistent_recovery, spill, visibility),
    configuration: ConfigurationObservabilitySnapshot {
      runtime: configuration_envelope(&configuration, ConfigurationFamily::Runtime, visibility),
      lifecycle: configuration_envelope(&configuration, ConfigurationFamily::Lifecycle, visibility),
    },
  })
}

fn durability_observability(
  engine: &StorageEngine,
  frontier: DurabilityCoordinatorSnapshot,
  group_policy: DurabilityGroupPolicySnapshot,
  runtime_failure: Option<DurabilityFailureState>,
  persistent_recovery: Option<PersistentDurabilityRecoveryState>,
  spill: Option<EmergencySpillReport>,
  visibility: ConfigurationVisibility,
) -> DurabilityObservabilitySnapshot {
  let read_only = runtime_failure.is_some() || persistent_recovery.as_ref().is_some_and(|recovery| recovery.blocks_writes);
  let repair_required = read_only || spill.as_ref().is_some_and(|report| report.succeeded);
  let repair_state = repair_state(runtime_failure.as_ref(), persistent_recovery.as_ref(), spill.as_ref());
  let repair_command = repair_required.then(|| {
    if visibility == ConfigurationVisibility::Root {
      Value::String(format!("aeordb verify --repair --force-fix-in-place -D {:?}", engine.database_path()))
    } else {
      redacted_value()
    }
  });
  let spill_total_bytes = spill.as_ref().map_or(0, |report| report.total_bytes);
  let spill_locations =
    spill.as_ref().and_then(|report| report.spill_directory.as_ref()).map(|path| vec![visible_path(path, visibility)]).unwrap_or_default();
  let spill_latest = spill.map(|report| visible_spill_report(report, visibility));
  let runtime_failure = visible_runtime_failure(runtime_failure, visibility);
  let last_barrier = visible_barrier_observation(frontier.last_barrier, visibility);

  DurabilityObservabilitySnapshot {
    frontier: DurabilityFrontierSnapshot {
      hard_frontier: frontier.hard_frontier,
      next_sequence: frontier.next_sequence,
      waiter_depth: frontier.admitted.saturating_add(frontier.executing),
      admitted: frontier.admitted,
      executing: frontier.executing,
      proven: frontier.proven,
      failed: frontier.failed,
      pending_hard: frontier.pending_hard,
      driver_active: frontier.driver_active,
      oldest_waiter_age_ms: frontier.oldest_pending_age_ms,
      last_barrier,
    },
    group_policy: match group_policy.policy {
      Some(policy) => DurabilityGroupPolicyObservability {
        enabled: true,
        max_bytes: Some(policy.max_bytes()),
        max_delay_ms: Some(policy.max_delay().as_millis().min(u64::MAX as u128) as u64),
        disabled_reason: None,
      },
      None => DurabilityGroupPolicyObservability {
        enabled: false,
        max_bytes: None,
        max_delay_ms: None,
        disabled_reason: group_policy.disabled_reason,
      },
    },
    latch: DurabilityLatchObservability { read_only, runtime_failure, persistent_recovery },
    spill: DurabilitySpillObservability {
      count: u64::from(spill_latest.is_some()),
      total_bytes: spill_total_bytes,
      locations: spill_locations,
      latest: spill_latest,
    },
    repair: DurabilityRepairObservability { required: repair_required, state: repair_state, command: repair_command, progress: None },
  }
}

fn repair_state(
  runtime_failure: Option<&DurabilityFailureState>,
  persistent_recovery: Option<&PersistentDurabilityRecoveryState>,
  spill: Option<&EmergencySpillReport>,
) -> &'static str {
  if persistent_recovery.is_some_and(PersistentDurabilityRecoveryState::is_repair_verifying) {
    return "verifying";
  }
  if persistent_recovery.is_some_and(PersistentDurabilityRecoveryState::is_catalog_replaying) {
    return "replaying";
  }
  if persistent_recovery.is_some_and(|recovery| recovery.blocks_writes) {
    return "required";
  }
  if runtime_failure.is_some() {
    return "runtime_failure";
  }
  if spill.is_some_and(|report| report.succeeded) {
    return "spill_pending";
  }
  "not_required"
}

fn visible_runtime_failure(
  mut failure: Option<DurabilityFailureState>,
  visibility: ConfigurationVisibility,
) -> Option<DurabilityFailureState> {
  if visibility == ConfigurationVisibility::Redacted {
    if let Some(failure) = failure.as_mut() {
      failure.first_failure = "<redacted>".to_string();
      failure.latest_failure = "<redacted>".to_string();
    }
  }
  failure
}

fn visible_barrier_observation(
  mut barrier: Option<DurabilityBarrierObservation>,
  visibility: ConfigurationVisibility,
) -> Option<DurabilityBarrierObservation> {
  if visibility == ConfigurationVisibility::Redacted {
    if let Some(error) = barrier.as_mut().and_then(|barrier| barrier.error.as_mut()) {
      *error = "<redacted>".to_string();
    }
  }
  barrier
}

fn visible_spill_report(report: EmergencySpillReport, visibility: ConfigurationVisibility) -> Value {
  let mut value = serde_json::to_value(report).unwrap_or_else(|error| json!({"serialization_error": error.to_string()}));
  if visibility == ConfigurationVisibility::Root {
    return value;
  }
  if let Some(object) = value.as_object_mut() {
    for field in ["spill_directory", "manifest_path", "hot_tail_path", "wal_tail_path", "index_buffer_path", "db_path"] {
      if object.get(field).is_some_and(|value| !value.is_null()) {
        object.insert(field.to_string(), redacted_value());
      }
    }
    for field in ["context", "failure"] {
      object.insert(field.to_string(), Value::String("<redacted>".to_string()));
    }
    if object.get("errors").is_some_and(|errors| errors.as_array().is_some_and(|errors| !errors.is_empty())) {
      object.insert("errors".to_string(), json!(["<redacted>"]));
    }
  }
  value
}

fn visible_path(path: &str, visibility: ConfigurationVisibility) -> Value {
  if visibility == ConfigurationVisibility::Root {
    Value::String(path.to_string())
  } else {
    redacted_value()
  }
}

fn redacted_value() -> Value {
  json!({"redacted": true})
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::engine::durability_coordinator::DurabilityOperation;

  const SECRET_PATH: &str = "/srv/private/database.aeordb";

  #[test]
  fn non_root_durability_observability_redacts_unstructured_diagnostics() {
    let failure = DurabilityFailureState {
      database_id: [1; 16],
      incident_id: [2; 16],
      creation_sequence: 3,
      first_failure_at_ms: 4,
      latest_failure_at_ms: 5,
      failed_operation: 6,
      os_error_class: 7,
      os_error_code: 8,
      last_selected_header_sequence: 9,
      last_durable_write_sequence: 10,
      last_durable_publication_sequence: 11,
      first_failure: format!("could not sync {SECRET_PATH}"),
      latest_failure: format!("could not reopen {SECRET_PATH}"),
      occurrence_count: 2,
    };
    let barrier = DurabilityBarrierObservation {
      operation: DurabilityOperation::DataBarrier,
      first_sequence: 12,
      last_sequence: 13,
      waiter_count: 2,
      succeeded: false,
      attempts: 1,
      latency_ms: 14,
      completed_at_ms: 15,
      error: Some(format!("barrier failed for {SECRET_PATH}")),
    };
    let spill = EmergencySpillReport {
      database_id: "01".repeat(16),
      incident_id: "02".repeat(16),
      source_location_class: Some(1),
      creation_sequence: 16,
      first_failure_at_ms: 17,
      latest_failure_at_ms: 18,
      failed_operation: 19,
      os_error_class: 20,
      os_error_code: 21,
      last_selected_header_sequence: 22,
      last_durable_write_sequence: 23,
      last_durable_publication_sequence: 24,
      attempted_at: "now".to_string(),
      context: format!("spill {SECRET_PATH}"),
      failure: format!("write failed for {SECRET_PATH}"),
      succeeded: false,
      spill_directory: Some(SECRET_PATH.to_string()),
      manifest_path: Some(SECRET_PATH.to_string()),
      hot_tail_path: Some(SECRET_PATH.to_string()),
      wal_tail_path: Some(SECRET_PATH.to_string()),
      index_buffer_path: Some(SECRET_PATH.to_string()),
      db_path: Some(SECRET_PATH.to_string()),
      hot_tail_writes: 0,
      hot_tail_voids: 0,
      index_pending_mutations: 0,
      index_dirty_saves: 0,
      index_deletes: 0,
      wal_tail_original_start: None,
      wal_tail_copy_start: None,
      wal_tail_end: None,
      hot_tail_bytes: 0,
      index_buffer_bytes: 0,
      wal_tail_bytes: 0,
      manifest_bytes: 0,
      total_bytes: 0,
      wal_tail_truncated: false,
      errors: vec![format!("cleanup failed for {SECRET_PATH}")],
    };

    let redacted_failure = visible_runtime_failure(Some(failure.clone()), ConfigurationVisibility::Redacted).unwrap();
    let redacted_barrier = visible_barrier_observation(Some(barrier.clone()), ConfigurationVisibility::Redacted).unwrap();
    let redacted_spill = visible_spill_report(spill.clone(), ConfigurationVisibility::Redacted);
    let redacted = serde_json::to_string(&(redacted_failure, redacted_barrier, redacted_spill)).unwrap();
    assert!(!redacted.contains(SECRET_PATH), "non-root observability leaked a host path: {redacted}");

    assert_eq!(
      visible_runtime_failure(Some(failure), ConfigurationVisibility::Root).unwrap().first_failure,
      format!("could not sync {SECRET_PATH}")
    );
    assert_eq!(
      visible_barrier_observation(Some(barrier), ConfigurationVisibility::Root).unwrap().error.as_deref(),
      Some("barrier failed for /srv/private/database.aeordb")
    );
    assert!(serde_json::to_string(&visible_spill_report(spill, ConfigurationVisibility::Root)).unwrap().contains(SECRET_PATH));
  }
}
