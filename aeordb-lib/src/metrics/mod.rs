pub mod definitions;
pub mod http_metrics_layer;

use std::sync::OnceLock;

use metrics_exporter_prometheus::PrometheusHandle;

use crate::engine::storage_engine::EngineMemoryStats;
use crate::engine::runtime_observability::RuntimeObservabilitySnapshot;
use crate::engine::v4::gc_run::{GcRunInvocationV1, GcRunModeV1, GcRunPhaseV1, GcRunStateV1};

static GLOBAL_HANDLE: OnceLock<Result<PrometheusHandle, String>> = OnceLock::new();

/// Install AeorDB's Prometheus recorder without taking process-exit policy
/// away from an embedding host.
pub fn try_initialize_metrics() -> Result<PrometheusHandle, String> {
  GLOBAL_HANDLE
    .get_or_init(|| {
      metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .map_err(|error| format!("failed to install AeorDB metrics recorder: {error}"))
    })
    .clone()
}

/// Install the Prometheus recorder globally and return the handle used to
/// render metrics in Prometheus text format.
///
/// Safe to call multiple times -- only the first call installs the recorder;
/// subsequent calls return the same handle.
pub fn initialize_metrics() -> PrometheusHandle {
  try_initialize_metrics().expect("failed to install Prometheus metrics recorder")
}

/// Record a non-authoritative follow-up failure without changing the outcome
/// of an already-acknowledged mutation. Subsystem and operation must be fixed
/// call-site constants so Prometheus label cardinality remains bounded.
pub fn record_system_soft_failure(
  subsystem: &'static str,
  operation: &'static str,
  context: impl std::fmt::Display,
  error: impl std::fmt::Display,
) {
  tracing::warn!(subsystem, operation, context = %context, error = %error, "Derived system follow-up failed; authoritative mutation outcome is unchanged");
  metrics::counter!(definitions::SYSTEM_SOFT_FAILURES_TOTAL, "subsystem" => subsystem, "operation" => operation).increment(1);
}

pub fn record_memory_metrics(memory: &EngineMemoryStats) {
  metrics::gauge!(definitions::PROCESS_RSS_BYTES).set(memory.process.rss_bytes as f64);
  metrics::gauge!(definitions::PROCESS_PEAK_RSS_BYTES).set(memory.process.peak_rss_bytes as f64);
  metrics::gauge!(definitions::PROCESS_VIRTUAL_BYTES).set(memory.process.virtual_bytes as f64);
  metrics::gauge!(definitions::PROCESS_DATA_BYTES).set(memory.process.data_bytes as f64);
  metrics::gauge!(definitions::PROCESS_SWAP_BYTES).set(memory.process.swap_bytes as f64);
  metrics::gauge!(definitions::PROCESS_THREAD_COUNT).set(memory.process.thread_count as f64);
  metrics::gauge!(definitions::PROCESS_FD_COUNT).set(memory.process.fd_count as f64);
  record_optional_gauge(definitions::PROCESS_PRIVATE_BYTES, memory.process.private_bytes);
  record_optional_gauge(definitions::PROCESS_SHARED_BYTES, memory.process.shared_bytes);
  record_optional_gauge(definitions::PROCESS_MAPPED_BYTES, memory.process.mapped_bytes);
  record_optional_gauge(definitions::PROCESS_ALLOCATOR_BYTES, memory.process.allocator_bytes);
  let coordinator = &memory.coordinator;
  metrics::gauge!(definitions::MEMORY_OBSERVED_BYTES).set(coordinator.observed_bytes as f64);
  metrics::gauge!(definitions::MEMORY_RESERVED_BYTES).set(coordinator.reserved_bytes as f64);
  metrics::gauge!(definitions::MEMORY_CRITICAL_RESERVED_BYTES).set(coordinator.critical_reserved_bytes as f64);
  metrics::gauge!(definitions::MEMORY_ACCOUNTED_BYTES).set(coordinator.accounted_bytes as f64);
  metrics::gauge!(definitions::MEMORY_UNACCOUNTED_RSS_BYTES).set(coordinator.unaccounted_rss_bytes as f64);
  metrics::gauge!(definitions::MEMORY_REJECTED_RESERVATIONS).set(coordinator.rejected_reservations as f64);
  metrics::gauge!(definitions::MEMORY_DEFERRED_RESERVATIONS).set(coordinator.deferred_reservations as f64);
  metrics::gauge!(definitions::MEMORY_MAINTENANCE_PAUSED).set(bool_gauge(coordinator.maintenance_paused));
  let current_pressure = match coordinator.pressure {
    crate::engine::memory_coordinator::MemoryPressure::Unconfigured => "unconfigured",
    crate::engine::memory_coordinator::MemoryPressure::Normal => "normal",
    crate::engine::memory_coordinator::MemoryPressure::Soft => "soft",
    crate::engine::memory_coordinator::MemoryPressure::Hard => "hard",
  };
  for pressure in ["unconfigured", "normal", "soft", "hard"] {
    metrics::gauge!(definitions::MEMORY_PRESSURE, "level" => pressure).set(bool_gauge(pressure == current_pressure));
  }
  for owner in &coordinator.owners {
    let name = owner.owner.as_str();
    metrics::gauge!(definitions::MEMORY_OWNER_RESIDENT_BYTES, "owner" => name).set(owner.observed.resident_bytes as f64);
    metrics::gauge!(definitions::MEMORY_OWNER_CLEAN_BYTES, "owner" => name).set(owner.observed.clean_bytes as f64);
    metrics::gauge!(definitions::MEMORY_OWNER_DIRTY_BYTES, "owner" => name).set(owner.observed.dirty_bytes as f64);
    metrics::gauge!(definitions::MEMORY_OWNER_EVICTABLE_BYTES, "owner" => name).set(owner.observed.evictable_bytes as f64);
    metrics::gauge!(definitions::MEMORY_OWNER_PINNED_BYTES, "owner" => name).set(owner.observed.pinned_bytes as f64);
    metrics::gauge!(definitions::MEMORY_OWNER_SPILL_BYTES, "owner" => name).set(owner.observed.spill_bytes as f64);
    metrics::gauge!(definitions::MEMORY_OWNER_RESERVED_BYTES, "owner" => name).set(owner.reserved_bytes as f64);
    metrics::gauge!(definitions::MEMORY_OWNER_CRITICAL_RESERVED_BYTES, "owner" => name).set(owner.critical_reserved_bytes as f64);
    metrics::gauge!(definitions::MEMORY_OWNER_PEAK_RESERVED_BYTES, "owner" => name).set(owner.peak_reserved_bytes as f64);
    metrics::gauge!(definitions::MEMORY_OWNER_ACTIVE_RESERVATIONS, "owner" => name).set(owner.active_reservations as f64);
    metrics::gauge!(definitions::MEMORY_OWNER_ITEMS, "owner" => name).set(owner.observed.items as f64);
    metrics::gauge!(definitions::MEMORY_OWNER_HITS, "owner" => name).set(owner.observed.hits as f64);
    metrics::gauge!(definitions::MEMORY_OWNER_MISSES, "owner" => name).set(owner.observed.misses as f64);
    metrics::gauge!(definitions::MEMORY_OWNER_EVICTIONS, "owner" => name).set(owner.observed.evictions as f64);
    metrics::gauge!(definitions::MEMORY_OWNER_REJECTIONS, "owner" => name).set(owner.rejections as f64);
    metrics::gauge!(definitions::MEMORY_OWNER_DEFERRALS, "owner" => name).set(owner.deferrals as f64);
  }
  metrics::gauge!(definitions::ENGINE_MEMORY_ESTIMATED_BYTES).set(memory.estimated_engine_owned_bytes as f64);
  metrics::gauge!(definitions::INDEX_CACHE_ESTIMATED_BYTES).set(memory.index_cache.estimated_bytes as f64);
  metrics::gauge!(definitions::INDEX_CACHE_ESTIMATED_CLEAN_BYTES).set(memory.index_cache.estimated_clean_bytes as f64);
  metrics::gauge!(definitions::INDEX_CACHE_ESTIMATED_DIRTY_BYTES).set(memory.index_cache.estimated_dirty_bytes as f64);
  metrics::gauge!(definitions::INDEX_CACHE_CLEAN_RESERVED_BYTES).set(memory.index_cache.clean_reserved_bytes as f64);
  metrics::gauge!(definitions::INDEX_CACHE_DIRTY_RESERVED_BYTES).set(memory.index_cache.dirty_reserved_bytes as f64);
  metrics::gauge!(definitions::INDEX_CACHE_FLUSH_RESERVED_BYTES).set(memory.index_cache.flush_reserved_bytes as f64);
  metrics::gauge!(definitions::INDEX_CACHE_CACHED_INDEXES).set(memory.index_cache.cached_indexes as f64);
  metrics::gauge!(definitions::INDEX_CACHE_DIRTY_INDEXES).set(memory.index_cache.dirty_indexes as f64);
  metrics::gauge!(definitions::INDEX_CACHE_FLUSHING_INDEXES).set(memory.index_cache.flushing_indexes as f64);
  metrics::gauge!(definitions::INDEX_CACHE_PENDING_MUTATIONS).set(memory.index_cache.pending_mutations as f64);
  metrics::gauge!(definitions::INDEX_CACHE_MAX_BYTES).set(memory.index_cache.max_bytes as f64);
  metrics::gauge!(definitions::INDEX_MUTATION_BUFFER_MAX_BYTES).set(memory.index_cache.mutation_max_bytes as f64);
  metrics::gauge!(definitions::INDEX_PUBLICATION_BATCH_MAX_BYTES).set(memory.index_cache.publication_batch_max_bytes as f64);
  metrics::gauge!(definitions::INDEX_CACHE_EVICTIONS).set(memory.index_cache.evictions as f64);
  metrics::gauge!(definitions::INDEX_CACHE_EVICTED_INDEXES).set(memory.index_cache.evicted_indexes as f64);
  metrics::gauge!(definitions::INDEX_CACHE_EVICTED_BYTES).set(memory.index_cache.evicted_bytes as f64);
  metrics::gauge!(definitions::INDEX_CACHE_ENTRIES).set(memory.index_cache.entries as f64);
  metrics::gauge!(definitions::INDEX_CACHE_VALUES).set(memory.index_cache.values as f64);
  metrics::gauge!(definitions::DIRECTORY_CACHE_ESTIMATED_BYTES).set(memory.directory_cache.estimated_bytes as f64);
  metrics::gauge!(definitions::DIRECTORY_CACHE_ENTRIES).set(memory.directory_cache.entries as f64);
}

pub fn record_runtime_metrics(runtime: &RuntimeObservabilitySnapshot) {
  record_memory_metrics(&runtime.memory);
  record_index_runtime_metrics(&runtime.index_runtime);
  let durability = &runtime.durability;
  metrics::gauge!(definitions::DURABILITY_HARD_FRONTIER).set(durability.frontier.hard_frontier as f64);
  metrics::gauge!(definitions::DURABILITY_NEXT_SEQUENCE).set(durability.frontier.next_sequence as f64);
  metrics::gauge!(definitions::DURABILITY_WAITER_DEPTH).set(durability.frontier.waiter_depth as f64);
  metrics::gauge!(definitions::DURABILITY_PENDING_HARD).set(durability.frontier.pending_hard as f64);
  record_optional_gauge(definitions::DURABILITY_OLDEST_WAITER_AGE_MS, durability.frontier.oldest_waiter_age_ms);
  record_optional_gauge(
    definitions::DURABILITY_LAST_BARRIER_LATENCY_MS,
    durability.frontier.last_barrier.as_ref().map(|barrier| barrier.latency_ms),
  );
  metrics::gauge!(definitions::DURABILITY_LAST_BARRIER_SUCCESS)
    .set(durability.frontier.last_barrier.as_ref().map_or(f64::NAN, |barrier| bool_gauge(barrier.succeeded)));
  metrics::gauge!(definitions::DURABILITY_GROUP_COMMIT_ENABLED).set(bool_gauge(durability.group_policy.enabled));
  record_optional_gauge(definitions::DURABILITY_GROUP_COMMIT_MAX_BYTES, durability.group_policy.max_bytes);
  record_optional_gauge(definitions::DURABILITY_GROUP_COMMIT_MAX_DELAY_MS, durability.group_policy.max_delay_ms);
  metrics::gauge!(definitions::DURABILITY_READ_ONLY).set(bool_gauge(durability.latch.read_only));
  metrics::gauge!(definitions::DURABILITY_SPILL_COUNT).set(durability.spill.count as f64);
  metrics::gauge!(definitions::DURABILITY_SPILL_BYTES).set(durability.spill.total_bytes as f64);
  metrics::gauge!(definitions::DURABILITY_REPAIR_REQUIRED).set(bool_gauge(durability.repair.required));
  record_gc_metrics(runtime.gc.as_ref());
  record_configuration_family("runtime", &runtime.configuration.runtime);
  record_configuration_family("lifecycle", &runtime.configuration.lifecycle);
}

fn record_index_runtime_metrics(runtime: &crate::engine::runtime_observability::IndexRuntimeObservabilitySnapshot) {
  metrics::gauge!(definitions::INDEX_RUNTIME_INSTALLED).set(bool_gauge(runtime.installed));
  for state in ["inactive", "recovering", "running", "degraded", "draining", "stopped"] {
    metrics::gauge!(definitions::INDEX_RUNTIME_STATE, "state" => state).set(bool_gauge(runtime.state == state));
  }
  metrics::gauge!(definitions::INDEX_RUNTIME_PENDING_TASKS).set(runtime.producer.pending_tasks as f64);
  metrics::gauge!(definitions::INDEX_RUNTIME_PENDING_TASK_BYTES).set(runtime.producer.pending_bytes as f64);
  metrics::gauge!(definitions::INDEX_RUNTIME_QUEUED_MUTATIONS)
    .set(runtime.mutations.active_mutations.saturating_add(runtime.mutations.frozen_mutations) as f64);
  metrics::gauge!(definitions::INDEX_RUNTIME_MUTATION_BYTES)
    .set(runtime.mutations.active_bytes.saturating_add(runtime.mutations.frozen_bytes) as f64);
  metrics::gauge!(definitions::INDEX_RUNTIME_RECONCILIATION_REQUIRED).set(bool_gauge(runtime.soft_mutations.reconciliation_required));
  metrics::gauge!(definitions::INDEX_RUNTIME_PUBLICATION_IN_FLIGHT).set(bool_gauge(runtime.publication_in_flight));
  metrics::gauge!(definitions::INDEX_RUNTIME_COVERAGE_ENTRIES).set(runtime.coverage.registry_entries as f64);
  metrics::gauge!(definitions::INDEX_RUNTIME_COVERAGE_RETAINED_BYTES).set(runtime.coverage.total_retained_bytes as f64);
  metrics::gauge!(definitions::INDEX_RUNTIME_COVERAGE_REFRESH_ATTEMPTS).set(runtime.coverage.refresh_attempts as f64);
  metrics::gauge!(definitions::INDEX_RUNTIME_COVERAGE_REFRESH_SUCCESSES).set(runtime.coverage.successful_refreshes as f64);
  metrics::gauge!(definitions::INDEX_RUNTIME_COVERAGE_REFRESH_PENDING).set(bool_gauge(runtime.coverage.refresh_pending));
  metrics::gauge!(definitions::INDEX_RUNTIME_COVERAGE_REFRESH_FAILURES).set(runtime.coverage.failed_refreshes as f64);
  metrics::gauge!(definitions::INDEX_RUNTIME_COVERAGE_SELECTED_GENERATIONS).set(runtime.coverage.selected_generations as f64);
  metrics::gauge!(definitions::INDEX_RUNTIME_COVERAGE_UNAVAILABLE_GENERATIONS).set(runtime.coverage.unavailable_generations as f64);
  metrics::gauge!(definitions::INDEX_RUNTIME_COVERAGE_USABLE_NVT_GENERATIONS).set(runtime.coverage.usable_nvt_generations as f64);
  metrics::gauge!(definitions::INDEX_RUNTIME_SCOPE_CACHE_ENTRIES).set(runtime.scope_ordinal_cache.entries as f64);
  metrics::gauge!(definitions::INDEX_RUNTIME_SCOPE_CACHE_RESIDENT_BYTES).set(runtime.scope_ordinal_cache.resident_bytes as f64);
  metrics::gauge!(definitions::INDEX_RUNTIME_SCOPE_CACHE_PINNED_ENTRIES).set(runtime.scope_ordinal_cache.pinned_entries as f64);
  metrics::gauge!(definitions::INDEX_RUNTIME_SCOPE_CACHE_HITS).set(runtime.scope_ordinal_cache.hits as f64);
  metrics::gauge!(definitions::INDEX_RUNTIME_SCOPE_CACHE_MISSES).set(runtime.scope_ordinal_cache.misses as f64);
  metrics::gauge!(definitions::INDEX_RUNTIME_SCOPE_CACHE_EVICTIONS).set(runtime.scope_ordinal_cache.evictions as f64);
}

fn record_gc_metrics(gc: Option<&crate::engine::GcRunStatusSnapshotV1>) {
  let status = gc.map(|snapshot| &snapshot.status);
  metrics::gauge!(definitions::GC_RUN_ACTIVE).set(bool_gauge(status.is_some_and(|status| status.state == GcRunStateV1::Running)));
  metrics::gauge!(definitions::GC_RUN_PROGRESS_RATIO).set(status.map_or(f64::NAN, |status| status.overall_progress));
  metrics::gauge!(definitions::GC_RUN_PHASE_PROGRESS_RATIO).set(status.map_or(f64::NAN, |status| status.phase_progress));
  metrics::gauge!(definitions::GC_RUN_MEMORY_RESERVED_BYTES).set(status.map_or(f64::NAN, |status| status.memory_reserved_bytes as f64));
  metrics::gauge!(definitions::GC_RUN_SCRATCH_USED_BYTES).set(status.map_or(f64::NAN, |status| status.scratch_used_bytes as f64));
  metrics::gauge!(definitions::GC_RUN_MUTATION_JOURNAL_LAG).set(status.map_or(f64::NAN, |status| status.mutation_journal_lag as f64));
  metrics::gauge!(definitions::GC_RUN_ETA_MS).set(status.and_then(|status| status.eta_ms).map_or(f64::NAN, |value| value as f64));
  metrics::gauge!(definitions::GC_RUN_CHECKPOINT_AGE_MS)
    .set(status.and_then(|status| status.checkpoint_age_ms).map_or(f64::NAN, |value| value as f64));

  let current_state = status.map(|status| gc_state_name(status.state));
  for state in ["running", "complete", "incomplete", "cancelled", "failed", "refused"] {
    metrics::gauge!(definitions::GC_RUN_STATE, "state" => state).set(bool_gauge(current_state == Some(state)));
  }
  let current_phase = status.and_then(|status| status.phase).map(GcRunPhaseV1::name);
  for phase in ["prepare", "inventory", "mark", "mutation_convergence", "finalize"] {
    metrics::gauge!(definitions::GC_RUN_PHASE, "phase" => phase).set(bool_gauge(current_phase == Some(phase)));
  }
  let current_invocation = status.map(|status| gc_invocation_name(status.invocation));
  for invocation in ["cli", "http", "task", "scheduled", "repair_follow_up", "embedded"] {
    metrics::gauge!(definitions::GC_RUN_INVOCATION, "invocation" => invocation).set(bool_gauge(current_invocation == Some(invocation)));
  }
  let current_mode = status.map(|status| gc_mode_name(status.mode));
  for mode in ["non_destructive_mark", "destructive"] {
    metrics::gauge!(definitions::GC_RUN_MODE, "mode" => mode).set(bool_gauge(current_mode == Some(mode)));
  }
}

fn gc_state_name(state: GcRunStateV1) -> &'static str {
  match state {
    GcRunStateV1::Running => "running",
    GcRunStateV1::Complete => "complete",
    GcRunStateV1::Incomplete => "incomplete",
    GcRunStateV1::Cancelled => "cancelled",
    GcRunStateV1::Failed => "failed",
    GcRunStateV1::Refused => "refused",
  }
}

fn gc_invocation_name(invocation: GcRunInvocationV1) -> &'static str {
  match invocation {
    GcRunInvocationV1::Cli => "cli",
    GcRunInvocationV1::Http => "http",
    GcRunInvocationV1::Task => "task",
    GcRunInvocationV1::Scheduled => "scheduled",
    GcRunInvocationV1::RepairFollowUp => "repair_follow_up",
    GcRunInvocationV1::Embedded => "embedded",
  }
}

fn gc_mode_name(mode: GcRunModeV1) -> &'static str {
  match mode {
    GcRunModeV1::NonDestructiveMark => "non_destructive_mark",
    GcRunModeV1::Destructive => "destructive",
  }
}

fn record_optional_gauge(name: &'static str, value: Option<u64>) {
  metrics::gauge!(name).set(value.map_or(f64::NAN, |value| value as f64));
}

fn bool_gauge(value: bool) -> f64 {
  if value {
    1.0
  } else {
    0.0
  }
}

fn record_configuration_family(family: &'static str, envelope: &serde_json::Value) {
  let status = &envelope["status"];
  metrics::gauge!(definitions::CONFIGURATION_FAMILY_VALID, "family" => family).set(bool_gauge(status["valid"].as_bool().unwrap_or(false)));
  metrics::gauge!(definitions::CONFIGURATION_FAMILY_DEGRADED, "family" => family)
    .set(bool_gauge(status["degraded"].as_bool().unwrap_or(true)));
  metrics::gauge!(definitions::CONFIGURATION_PENDING_RESTART, "family" => family)
    .set(status["pending_restart"].as_object().map_or(0, serde_json::Map::len) as f64);
  metrics::gauge!(definitions::CONFIGURATION_PENDING_CONVERGENCE, "family" => family)
    .set(status["pending_convergence"].as_object().map_or(0, serde_json::Map::len) as f64);
  metrics::gauge!(definitions::CONFIGURATION_DISABLED_CAPABILITIES, "family" => family)
    .set(status["disabled_capabilities"].as_array().map_or(0, Vec::len) as f64);

  const SOURCES: [&str; 9] = [
    "default",
    "stored_runtime_v1",
    "stored_lifecycle_v0",
    "stored_lifecycle_v1",
    "environment",
    "deprecated_environment",
    "command_line",
    "last_known_good",
    "append_history",
  ];
  let Some(sources) = status["sources"].as_object() else {
    return;
  };
  for (path, source) in sources {
    let active_source = source.as_str();
    for candidate in SOURCES {
      metrics::gauge!(
        definitions::CONFIGURATION_PROPERTY_ACTIVE,
        "family" => family,
        "path" => path.clone(),
        "source" => candidate,
      )
      .set(bool_gauge(active_source == Some(candidate)));
    }
  }
}
