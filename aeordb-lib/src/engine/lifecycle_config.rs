//! Database lifecycle configuration: snapshot retention and related policies.
//!
//! Stored as a virtual file at `/.aeordb-config/lifecycle.json` inside the
//! database. The configuration authority validates and activates it as one
//! coherent generation. Defaults preserve the "always recoverable" promise:
//! zero pruning unless the user opts in.

use serde::{Deserialize, Serialize};

use crate::engine::config_resolver::ConfigurationFamily;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::v4::migration_source_gc::SourceGcMutationPermitV1;
use crate::engine::version_manager::{VersionManager, SnapshotInfo};

pub const LIFECYCLE_CONFIG_PATH: &str = "/.aeordb-config/lifecycle.json";

pub const SNAPSHOT_TYPE_KEY: &str = "type";
pub const SNAPSHOT_TYPE_AUTO: &str = "auto";
pub const SNAPSHOT_TYPE_MANUAL: &str = "manual";

/// Retention policy for snapshots. A value of 0 means "never prune".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotRetention {
  /// Months after which auto-snapshots are eligible for pruning. 0 = never.
  #[serde(default)]
  pub auto_months: u32,
  /// Months after which manual snapshots are eligible for pruning. 0 = never.
  #[serde(default)]
  pub manual_months: u32,
}

impl Default for SnapshotRetention {
  fn default() -> Self {
    SnapshotRetention { auto_months: 0, manual_months: 0 }
  }
}

fn default_snapshot_writes_enabled() -> bool {
  true
}

/// Full lifecycle config schema. Extend with adjacent settings (GC cadence,
/// scrub schedule, auto-snapshot interval) as they're added.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LifecycleConfig {
  /// Whether new snapshots may be written. Existing snapshots remain readable,
  /// restorable, deletable, and eligible for retention pruning.
  #[serde(default = "default_snapshot_writes_enabled")]
  pub snapshot_writes_enabled: bool,
  #[serde(default)]
  pub snapshot_retention: SnapshotRetention,
}

impl Default for LifecycleConfig {
  fn default() -> Self {
    LifecycleConfig { snapshot_writes_enabled: true, snapshot_retention: SnapshotRetention::default() }
  }
}

/// Read the active lifecycle policy from the configuration authority.
///
/// Missing properties use their registered defaults. An unresolved property
/// fails closed: snapshot writes stay disabled and retention stays off rather
/// than silently substituting a potentially destructive policy.
pub fn load_lifecycle_config(engine: &StorageEngine) -> LifecycleConfig {
  let snapshot = engine.configuration_snapshot();
  LifecycleConfig {
    snapshot_writes_enabled: snapshot.resolved_boolean("lifecycle.snapshot_writes_enabled").unwrap_or(false),
    snapshot_retention: SnapshotRetention {
      auto_months: snapshot
        .resolved_unsigned("lifecycle.snapshot_retention_auto_months")
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0),
      manual_months: snapshot
        .resolved_unsigned("lifecycle.snapshot_retention_manual_months")
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0),
    },
  }
}

/// Validate, durably persist, and activate lifecycle configuration.
pub fn save_lifecycle_config(engine: &StorageEngine, config: &LifecycleConfig) -> EngineResult<()> {
  let mut value = serde_json::to_value(config).map_err(|e| EngineError::InvalidInput(format!("serialization error: {e}")))?;
  value.as_object_mut().expect("LifecycleConfig serializes as an object").insert("schema_version".to_string(), serde_json::Value::from(1));
  let data = serde_json::to_vec_pretty(&value).map_err(|e| EngineError::InvalidInput(format!("serialization error: {e}")))?;
  engine.replace_configuration_document(ConfigurationFamily::Lifecycle, &data)?;
  Ok(())
}

/// Whether callers are allowed to create new snapshot records.
///
/// This only gates snapshot writes. Existing snapshot reads, restores, deletes,
/// exports, and retention pruning keep working when writes are disabled.
pub fn snapshot_writes_enabled(engine: &StorageEngine) -> bool {
  load_lifecycle_config(engine).snapshot_writes_enabled
}

/// Return an explicit engine error when snapshot writes are disabled.
pub fn ensure_snapshot_writes_enabled(engine: &StorageEngine) -> EngineResult<()> {
  if snapshot_writes_enabled(engine) {
    Ok(())
  } else {
    Err(EngineError::SnapshotWritesDisabled)
  }
}

/// Result of a snapshot retention pass.
#[derive(Debug, Clone, Default)]
pub struct PruneResult {
  pub pruned_count: usize,
  pub pruned_names: Vec<String>,
  pub skipped_engine_internal: usize,
}

/// Classify a snapshot's retention type from its metadata. Snapshots with no
/// explicit type default to `manual` — this matches the principle that
/// untagged snapshots are user-intentional and protected by default.
pub fn snapshot_type(info: &SnapshotInfo) -> &str {
  match info.metadata.get(SNAPSHOT_TYPE_KEY).map(String::as_str) {
    Some(SNAPSHOT_TYPE_AUTO) => SNAPSHOT_TYPE_AUTO,
    _ => SNAPSHOT_TYPE_MANUAL,
  }
}

/// True if a snapshot name is engine-internal and should never be touched by
/// the user-facing retention policy (engine has its own retention for these,
/// e.g. pre-GC snapshots are pruned to last 3 in run_gc).
fn is_engine_internal(name: &str) -> bool {
  name.starts_with("_aeordb_")
}

/// Walk all snapshots and delete those whose age exceeds the configured
/// retention for their type. Engine-internal snapshots (`_aeordb_*`) are
/// always skipped here — they have separate retention handled by the engine.
///
/// Returns the names of pruned snapshots so callers can log/emit them. The
/// actual reclamation of orphaned data happens in the next GC sweep.
pub fn prune_expired_snapshots(engine: &StorageEngine, ctx: &RequestContext) -> EngineResult<PruneResult> {
  prune_expired_snapshots_with_post_capture_hook(engine, ctx, || {})
}

#[doc(hidden)]
pub fn prune_expired_snapshots_with_post_capture_hook<F>(
  engine: &StorageEngine,
  ctx: &RequestContext,
  post_capture_hook: F,
) -> EngineResult<PruneResult>
where
  F: FnOnce(),
{
  let mutation_permit = engine.admit_migration_sensitive_gc_mutation()?;
  prune_expired_snapshots_admitted_with_post_capture_hook(engine, ctx, &mutation_permit, post_capture_hook)
}

pub(crate) fn prune_expired_snapshots_admitted(
  engine: &StorageEngine,
  ctx: &RequestContext,
  mutation_permit: &SourceGcMutationPermitV1<'_>,
) -> EngineResult<PruneResult> {
  prune_expired_snapshots_admitted_with_post_capture_hook(engine, ctx, mutation_permit, || {})
}

fn prune_expired_snapshots_admitted_with_post_capture_hook<F>(
  engine: &StorageEngine,
  ctx: &RequestContext,
  _mutation_permit: &SourceGcMutationPermitV1<'_>,
  post_capture_hook: F,
) -> EngineResult<PruneResult>
where
  F: FnOnce(),
{
  let _mem = crate::engine::rss_sampler::PhaseSampler::start("prune_expired_snapshots", std::time::Duration::from_millis(50));
  let run_configuration = engine.capture_snapshot_retention_run_configuration()?;
  post_capture_hook();
  let auto_months = run_configuration.auto_months;
  let manual_months = run_configuration.manual_months;
  tracing::debug!(
    configuration_generation = run_configuration.generation,
    auto_months,
    manual_months,
    "Captured snapshot-retention policy"
  );

  if auto_months == 0 && manual_months == 0 {
    return Ok(PruneResult::default());
  }

  let vm = VersionManager::new(engine);
  let snapshots = vm.list_snapshots()?;

  let now_ms = chrono::Utc::now().timestamp_millis();
  let mut result = PruneResult::default();

  for snapshot in &snapshots {
    if is_engine_internal(&snapshot.name) {
      result.skipped_engine_internal += 1;
      continue;
    }

    let months = match snapshot_type(snapshot) {
      SNAPSHOT_TYPE_AUTO => auto_months,
      _ => manual_months,
    };
    if months == 0 {
      continue;
    }

    let age_ms = now_ms.saturating_sub(snapshot.created_at);
    let threshold_ms = (months as i64) * 30 * 24 * 60 * 60 * 1000;
    if age_ms < threshold_ms {
      continue;
    }

    vm.delete_snapshot(ctx, &snapshot.name)?;
    tracing::info!(
      name = %snapshot.name,
      age_days = age_ms / (24 * 60 * 60 * 1000),
      snapshot_type = %snapshot_type(snapshot),
      "Pruned expired snapshot"
    );
    result.pruned_count += 1;
    result.pruned_names.push(snapshot.name.clone());
  }

  Ok(result)
}
