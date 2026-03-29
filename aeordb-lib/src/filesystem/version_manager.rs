use chrono::{DateTime, Utc};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;

/// Metadata about a named version (snapshot) of the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionInfo {
  pub name: String,
  pub savepoint_id: u64,
  pub created_at: DateTime<Utc>,
  pub metadata: HashMap<String, String>,
}

#[derive(Debug, Error)]
pub enum VersionError {
  #[error("version not found: {0}")]
  VersionNotFound(String),

  #[error("version already exists: {0}")]
  VersionAlreadyExists(String),

  #[error("redb error: {0}")]
  RedbError(#[from] redb::Error),

  #[error("redb database error: {0}")]
  RedbDatabaseError(#[from] redb::DatabaseError),

  #[error("redb table error: {0}")]
  RedbTableError(#[from] redb::TableError),

  #[error("redb transaction error: {0}")]
  RedbTransactionError(#[from] redb::TransactionError),

  #[error("redb storage error: {0}")]
  RedbStorageError(#[from] redb::StorageError),

  #[error("redb commit error: {0}")]
  RedbCommitError(#[from] redb::CommitError),

  #[error("redb savepoint error: {0}")]
  RedbSavepointError(#[from] redb::SavepointError),

  #[error("serialization error: {0}")]
  SerializationError(String),
}

pub type Result<T> = std::result::Result<T, VersionError>;

const VERSIONS_TABLE: TableDefinition<&str, &[u8]> =
  TableDefinition::new("_system:versions");

/// Thin wrapper around redb's persistent savepoints that maps human-readable
/// version names to redb savepoint IDs.
///
/// # Important: restore semantics
///
/// `restore_version` rolls the ENTIRE database back to the savepoint state.
/// Because the versions metadata table is part of the database, version entries
/// created AFTER the restored version will be lost.  The restored version's own
/// metadata is re-written after the restore so it remains available.
///
/// redb also automatically deletes all persistent savepoints newer than the
/// restored one, so those savepoint IDs become invalid.
pub struct VersionManager {
  database: Arc<Database>,
}

impl VersionManager {
  pub fn new(database: Arc<Database>) -> Self {
    Self { database }
  }

  /// Create a new named version (snapshot of the current database state).
  ///
  /// The persistent savepoint is created first (capturing state before the
  /// metadata write), then the version metadata is stored in the same
  /// transaction.  On restore, the metadata for this version is re-inserted
  /// since it was not part of the captured snapshot.
  #[tracing::instrument(skip(self, metadata), fields(version_name = %name))]
  pub fn create_version(
    &self,
    name: &str,
    metadata: HashMap<String, String>,
  ) -> Result<VersionInfo> {
    let start = std::time::Instant::now();

    // Check for duplicate names before doing anything expensive.
    if self.get_version(name)?.is_some() {
      return Err(VersionError::VersionAlreadyExists(name.to_string()));
    }

    let write_transaction = self.database.begin_write()?;

    // persistent_savepoint() MUST be called before opening any tables.
    let savepoint_id = write_transaction.persistent_savepoint()?;

    let version_info = VersionInfo {
      name: name.to_string(),
      savepoint_id,
      created_at: Utc::now(),
      metadata,
    };

    let serialized = serde_json::to_vec(&version_info)
      .map_err(|error| VersionError::SerializationError(error.to_string()))?;

    {
      let mut table = write_transaction.open_table(VERSIONS_TABLE)?;
      table.insert(name, serialized.as_slice())?;
    }

    write_transaction.commit()?;

    let duration = start.elapsed().as_secs_f64();
    metrics::histogram!(crate::metrics::definitions::VERSION_SNAPSHOT_DURATION).record(duration);
    metrics::counter!(crate::metrics::definitions::VERSION_SNAPSHOTS_TOTAL).increment(1);

    tracing::info!(
      version_name = %name,
      savepoint_id = version_info.savepoint_id,
      "Version created"
    );

    Ok(version_info)
  }

  /// Get version info by name.
  pub fn get_version(&self, name: &str) -> Result<Option<VersionInfo>> {
    let read_transaction = self.database.begin_read()?;
    let table = match read_transaction.open_table(VERSIONS_TABLE) {
      Ok(table) => table,
      Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
      Err(error) => return Err(VersionError::RedbTableError(error)),
    };

    let guard = match table.get(name)? {
      Some(guard) => guard,
      None => return Ok(None),
    };

    let version_info: VersionInfo = serde_json::from_slice(guard.value())
      .map_err(|error| VersionError::SerializationError(error.to_string()))?;

    Ok(Some(version_info))
  }

  /// List all versions, ordered by created_at (oldest first).
  pub fn list_versions(&self) -> Result<Vec<VersionInfo>> {
    let read_transaction = self.database.begin_read()?;
    let table = match read_transaction.open_table(VERSIONS_TABLE) {
      Ok(table) => table,
      Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
      Err(error) => return Err(VersionError::RedbTableError(error)),
    };

    let mut versions = Vec::new();
    for result in table.iter()? {
      let (_key_guard, value_guard) = result?;
      let version_info: VersionInfo = serde_json::from_slice(value_guard.value())
        .map_err(|error| VersionError::SerializationError(error.to_string()))?;
      versions.push(version_info);
    }

    versions.sort_by_key(|version| version.created_at);
    Ok(versions)
  }

  /// Restore the database to a named version.
  ///
  /// **WARNING**: This restores the ENTIRE database state.  All changes made
  /// after the version was created are lost, including version metadata for
  /// any versions created after this one.  redb automatically deletes
  /// persistent savepoints newer than the restored one.
  ///
  /// The restored version's own metadata is re-written after the restore so
  /// that `get_version` / `list_versions` still return it.
  #[tracing::instrument(skip(self), fields(version_name = %name))]
  pub fn restore_version(&self, name: &str) -> Result<()> {
    let start = std::time::Instant::now();

    // Look up the version info before restoring (we need it to re-insert).
    let version_info = self
      .get_version(name)?
      .ok_or_else(|| VersionError::VersionNotFound(name.to_string()))?;

    let mut write_transaction = self.database.begin_write()?;

    let savepoint = write_transaction
      .get_persistent_savepoint(version_info.savepoint_id)?;

    write_transaction.restore_savepoint(&savepoint)?;

    // The restore rolled back the versions table to the state at savepoint
    // time, which was BEFORE the version's own metadata was written.
    // Re-insert it so the version remains discoverable.
    let serialized = serde_json::to_vec(&version_info)
      .map_err(|error| VersionError::SerializationError(error.to_string()))?;

    {
      let mut table = write_transaction.open_table(VERSIONS_TABLE)?;
      table.insert(name, serialized.as_slice())?;
    }

    write_transaction.commit()?;

    let duration = start.elapsed().as_secs_f64();
    metrics::histogram!(crate::metrics::definitions::VERSION_RESTORE_DURATION).record(duration);
    metrics::counter!(crate::metrics::definitions::VERSION_RESTORES_TOTAL).increment(1);

    tracing::warn!(
      version_name = %name,
      "Version restored (destructive operation)"
    );

    Ok(())
  }

  /// Delete a named version (frees the underlying persistent savepoint).
  #[tracing::instrument(skip(self), fields(version_name = %name))]
  pub fn delete_version(&self, name: &str) -> Result<()> {
    let version_info = self
      .get_version(name)?
      .ok_or_else(|| VersionError::VersionNotFound(name.to_string()))?;

    let write_transaction = self.database.begin_write()?;

    // Delete the savepoint first — if it fails, we haven't touched metadata.
    write_transaction
      .delete_persistent_savepoint(version_info.savepoint_id)?;

    {
      let mut table = write_transaction.open_table(VERSIONS_TABLE)?;
      table.remove(name)?;
    }

    write_transaction.commit()?;

    tracing::info!(version_name = %name, "Version deleted");

    Ok(())
  }

  /// Get the latest (most recently created) version, or None if no versions
  /// exist.
  pub fn latest_version(&self) -> Result<Option<VersionInfo>> {
    let versions = self.list_versions()?;
    Ok(versions.into_iter().last())
  }
}
