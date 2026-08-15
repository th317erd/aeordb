use std::path::PathBuf;

use crate::engine::configuration_authority::ConfigurationAuthoritySnapshot;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::storage_engine::StorageEngine;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GcRunConfiguration {
  pub generation: u64,
  pub mark_memory_preferred_bytes: u64,
  pub mark_memory_minimum_bytes: u64,
  pub mark_scratch_free_reserve_bytes: u64,
  pub mark_scratch_max_bytes: Option<u64>,
  pub checkpoint_after_seconds: u64,
  pub checkpoint_after_dirty_bytes: u64,
  pub mark_workspace_root: PathBuf,
  pub root_expiry_retention_seconds: u64,
  pub root_expiry_max_bytes: u64,
  pub root_lifecycle_hard_max_bytes: u64,
  pub pending_delete_grace_seconds: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MaintenanceRunConfiguration {
  pub generation: u64,
  pub max_concurrent_tasks: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
// P3c migration preflight captures these values once so later migration
// phases cannot fall back to live configuration reads mid-run.
// The type is public for the disconnected P3c evidence adapter.
pub struct MigrationRunConfiguration {
  pub generation: u64,
  pub capture_max_bytes: u64,
  pub capture_free_reserve_bytes: u64,
  pub checkpoint_after_seconds: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotRetentionRunConfiguration {
  pub generation: u64,
  pub auto_months: u32,
  pub manual_months: u32,
}

impl StorageEngine {
  pub(crate) fn capture_gc_run_configuration(&self) -> EngineResult<GcRunConfiguration> {
    let snapshot = self.configuration_snapshot();
    Ok(GcRunConfiguration {
      generation: snapshot.generation,
      mark_memory_preferred_bytes: required_unsigned(&snapshot, "garbage_collection.mark_memory_preferred_bytes")?,
      mark_memory_minimum_bytes: required_unsigned(&snapshot, "garbage_collection.mark_memory_minimum_bytes")?,
      mark_scratch_free_reserve_bytes: required_unsigned(&snapshot, "garbage_collection.mark_scratch_free_reserve_bytes")?,
      mark_scratch_max_bytes: required_optional_bytes(&snapshot, "garbage_collection.mark_scratch_max_bytes")?,
      checkpoint_after_seconds: required_unsigned(&snapshot, "garbage_collection.checkpoint_after_seconds")?,
      checkpoint_after_dirty_bytes: required_unsigned(&snapshot, "garbage_collection.checkpoint_after_dirty_bytes")?,
      mark_workspace_root: required_path(&snapshot, "garbage_collection.mark_workspace_root")?,
      root_expiry_retention_seconds: required_unsigned(&snapshot, "garbage_collection.root_expiry_retention_seconds")?,
      root_expiry_max_bytes: required_unsigned(&snapshot, "garbage_collection.root_expiry_max_bytes")?,
      root_lifecycle_hard_max_bytes: required_unsigned(&snapshot, "garbage_collection.root_lifecycle_hard_max_bytes")?,
      pending_delete_grace_seconds: required_unsigned(&snapshot, "lifecycle.garbage_collection_pending_delete_grace_seconds")?,
    })
  }

  pub(crate) fn capture_maintenance_run_configuration(&self) -> EngineResult<MaintenanceRunConfiguration> {
    let snapshot = self.configuration_snapshot();
    let max_concurrent_tasks = usize::try_from(required_unsigned(&snapshot, "maintenance.max_concurrent_tasks")?)
      .map_err(|_| EngineError::InvalidInput("maintenance.max_concurrent_tasks does not fit this platform".to_string()))?;
    Ok(MaintenanceRunConfiguration { generation: snapshot.generation, max_concurrent_tasks })
  }

  #[allow(dead_code)] // Called when the P3c migration state owner is activated.
  pub(crate) fn capture_migration_run_configuration(&self) -> EngineResult<MigrationRunConfiguration> {
    let snapshot = self.configuration_snapshot();
    Ok(MigrationRunConfiguration {
      generation: snapshot.generation,
      capture_max_bytes: required_unsigned(&snapshot, "migration.capture_max_bytes")?,
      capture_free_reserve_bytes: required_unsigned(&snapshot, "migration.capture_free_reserve_bytes")?,
      checkpoint_after_seconds: required_unsigned(&snapshot, "migration.checkpoint_after_seconds")?,
    })
  }

  pub(crate) fn capture_snapshot_retention_run_configuration(&self) -> EngineResult<SnapshotRetentionRunConfiguration> {
    let snapshot = self.configuration_snapshot();
    let auto_months = u32::try_from(required_unsigned(&snapshot, "lifecycle.snapshot_retention_auto_months")?)
      .map_err(|_| EngineError::InvalidInput("lifecycle.snapshot_retention_auto_months exceeds u32".to_string()))?;
    let manual_months = u32::try_from(required_unsigned(&snapshot, "lifecycle.snapshot_retention_manual_months")?)
      .map_err(|_| EngineError::InvalidInput("lifecycle.snapshot_retention_manual_months exceeds u32".to_string()))?;
    Ok(SnapshotRetentionRunConfiguration { generation: snapshot.generation, auto_months, manual_months })
  }
}

fn required_unsigned(snapshot: &ConfigurationAuthoritySnapshot, path: &str) -> EngineResult<u64> {
  snapshot.resolved_unsigned(path).ok_or_else(|| EngineError::InvalidInput(format!("{path} is unresolved for a new operation")))
}

fn required_optional_bytes(snapshot: &ConfigurationAuthoritySnapshot, path: &str) -> EngineResult<Option<u64>> {
  snapshot.resolved_optional_bytes(path).ok_or_else(|| EngineError::InvalidInput(format!("{path} is unresolved for a new operation")))
}

fn required_path(snapshot: &ConfigurationAuthoritySnapshot, path: &str) -> EngineResult<PathBuf> {
  snapshot
    .resolved_path(path)
    .map(PathBuf::from)
    .ok_or_else(|| EngineError::InvalidInput(format!("{path} is unresolved for a new operation")))
}
