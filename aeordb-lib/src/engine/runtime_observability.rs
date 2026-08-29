use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};

use crate::engine::config_resolver::ConfigurationFamily;
use crate::engine::configuration_observability::{configuration_envelope, ConfigurationVisibility};
use crate::engine::durability_coordinator::{DurabilityBarrierObservation, DurabilityCoordinatorSnapshot, DurabilityGroupPolicySnapshot};
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::memory_coordinator::{MemoryOwnerSnapshot, MemoryPolicy, MemoryPressure};
use crate::engine::storage_engine::{
  DirectoryCacheMemoryStats, DurabilityFailureState, EmergencySpillReport, EngineCacheMemoryStats, EngineMemoryStats,
  IndexCacheMemoryStats, StorageEngine,
};
use crate::engine::v4::durability_recovery::PersistentDurabilityRecoveryState;

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeObservabilitySnapshot {
  pub memory: EngineMemoryStats,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub identity_engine: Option<IdentityEngineMemoryObservabilitySnapshot>,
  pub durability: DurabilityObservabilitySnapshot,
  pub configuration: ConfigurationObservabilitySnapshot,
  pub index_runtime: IndexRuntimeObservabilitySnapshot,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub gc: Option<crate::engine::gc_run_status::GcRunStatusSnapshotV1>,
}

/// Engine-local memory and cache residency for a distinct `file://` identity
/// authority. Process-wide RSS and the coordinator's process residual are
/// deliberately omitted because the primary and identity engines share one
/// process.
#[derive(Debug, Clone, Serialize)]
pub struct IdentityEngineMemoryObservabilitySnapshot {
  pub coordinator: IdentityEngineMemoryCoordinatorSnapshot,
  pub index_cache: IndexCacheMemoryStats,
  pub directory_cache: DirectoryCacheMemoryStats,
  pub caches: EngineCacheMemoryStats,
  pub estimated_engine_owned_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct IdentityEngineMemoryCoordinatorSnapshot {
  pub policy: Option<MemoryPolicy>,
  pub policy_error: Option<String>,
  pub pressure: MemoryPressure,
  pub maintenance_paused: bool,
  pub observed_bytes: u64,
  pub reserved_bytes: u64,
  pub critical_reserved_bytes: u64,
  pub accounted_bytes: u64,
  pub rejected_reservations: u64,
  pub deferred_reservations: u64,
  pub owners: Vec<MemoryOwnerSnapshot>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexRuntimeObservabilitySnapshot {
  pub installed: bool,
  pub state: &'static str,
  pub recovered_scopes: u32,
  pub highest_checkpoint_sequence: u64,
  pub publication_in_flight: bool,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub degraded: Option<IndexRuntimeDegradedObservability>,
  pub soft_mutations: IndexRuntimeSoftMutationObservability,
  pub producer: IndexRuntimeProducerObservability,
  pub mutations: IndexRuntimeMutationObservability,
  pub coverage: IndexRuntimeCoverageObservability,
  pub scope_ordinal_cache: IndexRuntimeScopeOrdinalCacheObservability,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexRuntimeDegradedObservability {
  pub code: &'static str,
  pub context: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexRuntimeSoftMutationObservability {
  pub admission_closed: bool,
  pub queued_notices: usize,
  pub retained_bytes: usize,
  pub maximum_notices: usize,
  pub maximum_retained_bytes: usize,
  pub maximum_notice_bytes: usize,
  pub latest_queued_publication_sequence: Option<u64>,
  pub reconciliation_required: bool,
  pub lost_through_sequence: Option<u64>,
  pub loss_reasons: Vec<&'static str>,
  pub dropped_notices: u64,
  pub loss_epoch: u64,
  pub reconciled_loss_epoch: u64,
  pub losses_in_flight: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexRuntimeProducerObservability {
  pub pending_tasks: u32,
  pub pending_bytes: u64,
  pub leased_tasks: u32,
  pub completed_tasks: u64,
  pub scheduled_retries: u64,
  pub spilled_tasks: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexRuntimeMutationObservability {
  pub state: &'static str,
  pub active_records: u64,
  pub active_mutations: u64,
  pub active_bytes: u64,
  pub frozen_records: u64,
  pub frozen_mutations: u64,
  pub frozen_bytes: u64,
  pub successful_flushes: u64,
  pub restored_flushes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexRuntimeCoverageObservability {
  pub refresh_attempts: u64,
  pub successful_refreshes: u64,
  pub failed_refreshes: u64,
  pub refresh_pending: bool,
  pub registry_entries: usize,
  pub registry_retained_bytes: u64,
  pub owner_requests_retained_bytes: u64,
  pub total_retained_bytes: u64,
  pub selected_generations: usize,
  pub unavailable_generations: usize,
  pub usable_nvt_generations: usize,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub last_failure: Option<IndexRuntimeDegradedObservability>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexRuntimeScopeOrdinalCacheObservability {
  pub entries: usize,
  pub resident_bytes: u64,
  pub pinned_entries: usize,
  pub hits: u64,
  pub misses: u64,
  pub evictions: u64,
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
    identity_engine: None,
    durability: durability_observability(engine, frontier, group_policy, runtime_failure, persistent_recovery, spill, visibility),
    configuration: ConfigurationObservabilitySnapshot {
      runtime: configuration_envelope(&configuration, ConfigurationFamily::Runtime, visibility),
      lifecycle: configuration_envelope(&configuration, ConfigurationFamily::Lifecycle, visibility),
    },
    index_runtime: index_runtime_observability(engine, visibility)?,
    gc: (visibility == ConfigurationVisibility::Root).then(|| engine.gc_run_status()).flatten(),
  })
}

/// Collect the shared runtime projection and attach engine-local identity
/// residency only when authentication owns a physically distinct engine.
pub fn collect_runtime_observability_with_identity_engine(
  engine: &Arc<StorageEngine>,
  identity_engine: &Arc<StorageEngine>,
  visibility: ConfigurationVisibility,
) -> EngineResult<RuntimeObservabilitySnapshot> {
  let mut runtime = collect_runtime_observability(engine, visibility)?;
  if !Arc::ptr_eq(engine, identity_engine) {
    runtime.identity_engine = Some(identity_engine_memory_observability(identity_engine.memory_stats()?));
  }
  Ok(runtime)
}

fn identity_engine_memory_observability(memory: EngineMemoryStats) -> IdentityEngineMemoryObservabilitySnapshot {
  let EngineMemoryStats { coordinator, index_cache, directory_cache, caches, estimated_engine_owned_bytes, .. } = memory;
  IdentityEngineMemoryObservabilitySnapshot {
    coordinator: IdentityEngineMemoryCoordinatorSnapshot {
      policy: coordinator.policy,
      policy_error: coordinator.policy_error,
      pressure: coordinator.pressure,
      maintenance_paused: coordinator.maintenance_paused,
      observed_bytes: coordinator.observed_bytes,
      reserved_bytes: coordinator.reserved_bytes,
      critical_reserved_bytes: coordinator.critical_reserved_bytes,
      accounted_bytes: coordinator.accounted_bytes,
      rejected_reservations: coordinator.rejected_reservations,
      deferred_reservations: coordinator.deferred_reservations,
      owners: coordinator.owners,
    },
    index_cache,
    directory_cache,
    caches,
    estimated_engine_owned_bytes,
  }
}

fn index_runtime_observability(
  engine: &StorageEngine,
  visibility: ConfigurationVisibility,
) -> EngineResult<IndexRuntimeObservabilitySnapshot> {
  let Some(snapshot) = engine.index_runtime_snapshot_v1() else {
    return Ok(IndexRuntimeObservabilitySnapshot {
      installed: false,
      state: "inactive",
      recovered_scopes: 0,
      highest_checkpoint_sequence: 0,
      publication_in_flight: false,
      degraded: None,
      soft_mutations: IndexRuntimeSoftMutationObservability {
        admission_closed: false,
        queued_notices: 0,
        retained_bytes: 0,
        maximum_notices: 0,
        maximum_retained_bytes: 0,
        maximum_notice_bytes: 0,
        latest_queued_publication_sequence: None,
        reconciliation_required: false,
        lost_through_sequence: None,
        loss_reasons: Vec::new(),
        dropped_notices: 0,
        loss_epoch: 0,
        reconciled_loss_epoch: 0,
        losses_in_flight: 0,
      },
      producer: IndexRuntimeProducerObservability {
        pending_tasks: 0,
        pending_bytes: 0,
        leased_tasks: 0,
        completed_tasks: 0,
        scheduled_retries: 0,
        spilled_tasks: 0,
      },
      mutations: IndexRuntimeMutationObservability {
        state: "inactive",
        active_records: 0,
        active_mutations: 0,
        active_bytes: 0,
        frozen_records: 0,
        frozen_mutations: 0,
        frozen_bytes: 0,
        successful_flushes: 0,
        restored_flushes: 0,
      },
      coverage: IndexRuntimeCoverageObservability {
        refresh_attempts: 0,
        successful_refreshes: 0,
        failed_refreshes: 0,
        refresh_pending: false,
        registry_entries: 0,
        registry_retained_bytes: 0,
        owner_requests_retained_bytes: 0,
        total_retained_bytes: 0,
        selected_generations: 0,
        unavailable_generations: 0,
        usable_nvt_generations: 0,
        last_failure: None,
      },
      scope_ordinal_cache: IndexRuntimeScopeOrdinalCacheObservability {
        entries: 0,
        resident_bytes: 0,
        pinned_entries: 0,
        hits: 0,
        misses: 0,
        evictions: 0,
      },
    });
  };

  let coverage = engine
    .index_runtime_coverage_snapshot_v1()
    .map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?
    .ok_or_else(|| EngineError::InvalidInput("installed index runtime has no coverage lifecycle".to_string()))?;
  let soft = &snapshot.soft_hub;
  let producer = &snapshot.producer;
  let mutations = &snapshot.mutations;
  Ok(IndexRuntimeObservabilitySnapshot {
    installed: true,
    state: snapshot.lifecycle.stable_name(),
    recovered_scopes: snapshot.recovered_scopes,
    highest_checkpoint_sequence: snapshot.highest_checkpoint_sequence,
    publication_in_flight: snapshot.publication_in_flight,
    degraded: snapshot.degraded.as_ref().map(|degraded| IndexRuntimeDegradedObservability {
      code: degraded.code,
      context: if visibility == ConfigurationVisibility::Root { degraded.context.clone() } else { "<redacted>".to_string() },
    }),
    soft_mutations: IndexRuntimeSoftMutationObservability {
      admission_closed: soft.admission_closed,
      queued_notices: soft.queued_notices,
      retained_bytes: soft.retained_bytes,
      maximum_notices: soft.maximum_notices,
      maximum_retained_bytes: soft.maximum_retained_bytes,
      maximum_notice_bytes: soft.maximum_notice_bytes,
      latest_queued_publication_sequence: soft.latest_queued_publication_sequence,
      reconciliation_required: soft.reconciliation_required,
      lost_through_sequence: soft.lost_through_sequence,
      loss_reasons: soft.loss_reasons.iter().map(|reason| soft_mutation_loss_reason_name(*reason)).collect(),
      dropped_notices: soft.dropped_notices,
      loss_epoch: soft.loss_epoch,
      reconciled_loss_epoch: soft.reconciled_loss_epoch,
      losses_in_flight: soft.losses_in_flight,
    },
    producer: IndexRuntimeProducerObservability {
      pending_tasks: producer.pending_tasks,
      pending_bytes: producer.pending_bytes,
      leased_tasks: producer.leased_tasks,
      completed_tasks: producer.completed_tasks,
      scheduled_retries: producer.scheduled_retries,
      spilled_tasks: producer.spilled_tasks,
    },
    mutations: IndexRuntimeMutationObservability {
      state: index_coordinator_lifecycle_name(mutations.lifecycle),
      active_records: mutations.active_records,
      active_mutations: mutations.active_mutations,
      active_bytes: mutations.active_bytes,
      frozen_records: mutations.frozen_records,
      frozen_mutations: mutations.frozen_mutations,
      frozen_bytes: mutations.frozen_bytes,
      successful_flushes: mutations.successful_flushes,
      restored_flushes: mutations.restored_flushes,
    },
    coverage: IndexRuntimeCoverageObservability {
      refresh_attempts: coverage.refresh_attempts,
      successful_refreshes: coverage.successful_refreshes,
      failed_refreshes: coverage.failed_refreshes,
      refresh_pending: coverage.refresh_pending,
      registry_entries: coverage.registry_entries,
      registry_retained_bytes: coverage.registry_retained_bytes,
      owner_requests_retained_bytes: coverage.owner_requests_retained_bytes,
      total_retained_bytes: coverage.total_retained_bytes,
      selected_generations: coverage.selected_generations,
      unavailable_generations: coverage.unavailable_generations,
      usable_nvt_generations: coverage.usable_nvt_generations,
      last_failure: coverage.last_failure.map(|failure| IndexRuntimeDegradedObservability {
        code: failure.code,
        context: if visibility == ConfigurationVisibility::Root { failure.context } else { "<redacted>".to_string() },
      }),
    },
    scope_ordinal_cache: IndexRuntimeScopeOrdinalCacheObservability {
      entries: coverage.scope_ordinal_cache.entries,
      resident_bytes: coverage.scope_ordinal_cache.resident_bytes,
      pinned_entries: coverage.scope_ordinal_cache.pinned_entries,
      hits: coverage.scope_ordinal_cache.hits,
      misses: coverage.scope_ordinal_cache.misses,
      evictions: coverage.scope_ordinal_cache.evictions,
    },
  })
}

fn soft_mutation_loss_reason_name(reason: crate::engine::v4::coverage_runtime::SoftMutationLossReasonV1) -> &'static str {
  use crate::engine::v4::coverage_runtime::SoftMutationLossReasonV1;
  match reason {
    SoftMutationLossReasonV1::InvalidNotice => "invalid_notice",
    SoftMutationLossReasonV1::QueueContended => "queue_contended",
    SoftMutationLossReasonV1::QueueFull => "queue_full",
    SoftMutationLossReasonV1::NoticeTooLarge => "notice_too_large",
    SoftMutationLossReasonV1::AllocationFailed => "allocation_failed",
    SoftMutationLossReasonV1::QueueUnavailable => "queue_unavailable",
  }
}

fn index_coordinator_lifecycle_name(lifecycle: crate::engine::v4::index_coordinator::IndexCoordinatorLifecycleV1) -> &'static str {
  use crate::engine::v4::index_coordinator::IndexCoordinatorLifecycleV1;
  match lifecycle {
    IndexCoordinatorLifecycleV1::Running => "running",
    IndexCoordinatorLifecycleV1::Draining => "draining",
    IndexCoordinatorLifecycleV1::Stopped => "stopped",
  }
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
    for field in
      ["spill_directory", "manifest_path", "hot_tail_path", "wal_tail_path", "index_buffer_path", "index_runtime_state_path", "db_path"]
    {
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
      index_runtime_state_path: Some(SECRET_PATH.to_string()),
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
      index_runtime_state_bytes: 0,
      wal_tail_bytes: 0,
      manifest_bytes: 0,
      total_bytes: 0,
      wal_tail_truncated: false,
      index_runtime_reconciliation_required: true,
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
