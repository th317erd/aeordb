//! Database lifecycle configuration: snapshot retention and related policies.
//!
//! Stored as a virtual file at `/.aeordb-config/lifecycle.json` inside the
//! database. Loaded on demand by the GC task. Defaults preserve the
//! "always recoverable" promise: zero pruning unless the user opts in.

use serde::{Deserialize, Serialize};

use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;
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

/// Full lifecycle config schema. Extend with adjacent settings (GC cadence,
/// scrub schedule, auto-snapshot interval) as they're added.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct LifecycleConfig {
  #[serde(default)]
  pub snapshot_retention: SnapshotRetention,
}

/// Load lifecycle config. Returns defaults (everything zeroed = never prune)
/// if the file is missing or unparseable.
pub fn load_lifecycle_config(engine: &StorageEngine) -> LifecycleConfig {
  let ops = DirectoryOps::new(engine);
  match ops.read_file_buffered(LIFECYCLE_CONFIG_PATH) {
    Ok(data) => match serde_json::from_slice::<LifecycleConfig>(&data) {
      Ok(config) => config,
      Err(e) => {
        tracing::warn!("Failed to parse {}: {} — using defaults", LIFECYCLE_CONFIG_PATH, e);
        LifecycleConfig::default()
      }
    },
    Err(_) => LifecycleConfig::default(),
  }
}

/// Persist lifecycle config.
pub fn save_lifecycle_config(engine: &StorageEngine, config: &LifecycleConfig) -> EngineResult<()> {
  let ops = DirectoryOps::new(engine);
  let ctx = RequestContext::system();
  let data = serde_json::to_vec_pretty(config)
    .map_err(|e| EngineError::InvalidInput(format!("serialization error: {e}")))?;
  ops.store_file_buffered(&ctx, LIFECYCLE_CONFIG_PATH, &data, Some("application/json"))?;
  Ok(())
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
pub fn prune_expired_snapshots(
  engine: &StorageEngine,
  ctx: &RequestContext,
) -> EngineResult<PruneResult> {
  let config = load_lifecycle_config(engine);
  let auto_months = config.snapshot_retention.auto_months;
  let manual_months = config.snapshot_retention.manual_months;

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

    match vm.delete_snapshot(ctx, &snapshot.name) {
      Ok(()) => {
        tracing::info!(
          name = %snapshot.name,
          age_days = age_ms / (24 * 60 * 60 * 1000),
          snapshot_type = %snapshot_type(snapshot),
          "Pruned expired snapshot"
        );
        result.pruned_count += 1;
        result.pruned_names.push(snapshot.name.clone());
      }
      Err(e) => {
        tracing::warn!("Failed to prune snapshot {}: {}", snapshot.name, e);
      }
    }
  }

  Ok(result)
}
