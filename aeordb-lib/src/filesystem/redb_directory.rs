use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition, TableHandle};
use std::sync::Arc;
use thiserror::Error;

use super::directory_entry::{DirectoryEntry, EntryType};

#[derive(Debug, Error)]
pub enum DirectoryError {
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

  #[error("serialization error: {0}")]
  SerializationError(String),
}

pub type Result<T> = std::result::Result<T, DirectoryError>;

pub struct RedbDirectory {
  database: Arc<Database>,
}

impl RedbDirectory {
  pub fn new(database: Arc<Database>) -> Self {
    Self { database }
  }

  /// Build the redb table name for a directory path.
  fn table_name(directory_path: &str) -> String {
    format!("dir:{}", directory_path)
  }

  /// Insert or update an entry in a directory.
  #[tracing::instrument(skip(self, entry), fields(directory_path = %directory_path, entry_name = %entry.name))]
  pub fn insert_entry(&self, directory_path: &str, entry: &DirectoryEntry) -> Result<()> {
    let table_name = Self::table_name(directory_path);
    let table_definition: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);

    let serialized = entry.serialize_to_bytes().map_err(|error| {
      DirectoryError::SerializationError(error.to_string())
    })?;

    let write_transaction = self.database.begin_write()?;
    {
      let mut table = write_transaction.open_table(table_definition)?;
      table.insert(entry.name.as_str(), serialized.as_slice())?;
    }
    write_transaction.commit()?;
    tracing::debug!(
      directory_path = %directory_path,
      entry_name = %entry.name,
      "Directory entry inserted"
    );
    Ok(())
  }

  /// Get an entry by name from a directory.
  #[tracing::instrument(skip(self), level = "trace")]
  pub fn get_entry(
    &self,
    directory_path: &str,
    entry_name: &str,
  ) -> Result<Option<DirectoryEntry>> {
    let table_name = Self::table_name(directory_path);
    let table_definition: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);

    let read_transaction = self.database.begin_read()?;
    let table = match read_transaction.open_table(table_definition) {
      Ok(table) => table,
      Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
      Err(error) => return Err(DirectoryError::RedbTableError(error)),
    };

    match table.get(entry_name)? {
      Some(guard) => {
        let entry = DirectoryEntry::deserialize_from_bytes(guard.value())
          .map_err(|error| DirectoryError::SerializationError(error.to_string()))?;
        Ok(Some(entry))
      }
      None => Ok(None),
    }
  }

  /// Remove an entry from a directory, returns the removed entry.
  #[tracing::instrument(skip(self))]
  pub fn remove_entry(
    &self,
    directory_path: &str,
    entry_name: &str,
  ) -> Result<Option<DirectoryEntry>> {
    let table_name = Self::table_name(directory_path);
    let table_definition: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);

    let write_transaction = self.database.begin_write()?;
    let result = {
      let mut table = write_transaction.open_table(table_definition)?;
      let removed = table.remove(entry_name)?;
      match removed {
        Some(guard) => {
          let bytes = guard.value().to_vec();
          drop(guard);
          let entry = DirectoryEntry::deserialize_from_bytes(&bytes)
            .map_err(|error| DirectoryError::SerializationError(error.to_string()))?;
          Some(entry)
        }
        None => None,
      }
    };
    write_transaction.commit()?;
    if result.is_some() {
      tracing::debug!(
        directory_path = %directory_path,
        entry_name = %entry_name,
        "Directory entry removed"
      );
    }
    Ok(result)
  }

  /// List all entries in a directory, sorted by name.
  pub fn list_entries(&self, directory_path: &str) -> Result<Vec<DirectoryEntry>> {
    let table_name = Self::table_name(directory_path);
    let table_definition: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);

    let read_transaction = self.database.begin_read()?;
    let table = match read_transaction.open_table(table_definition) {
      Ok(table) => table,
      Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
      Err(error) => return Err(DirectoryError::RedbTableError(error)),
    };

    let mut entries = Vec::new();
    for result in table.iter()? {
      let (_key_guard, value_guard) = result?;
      let entry = DirectoryEntry::deserialize_from_bytes(value_guard.value())
        .map_err(|error| DirectoryError::SerializationError(error.to_string()))?;
      entries.push(entry);
    }

    // redb iterates keys in sorted order already for &str keys,
    // but let's be explicit to guarantee the contract.
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    tracing::debug!(
      directory_path = %directory_path,
      entry_count = entries.len(),
      "Directory entries listed"
    );
    Ok(entries)
  }

  /// Count entries in a directory.
  pub fn count_entries(&self, directory_path: &str) -> Result<u64> {
    let table_name = Self::table_name(directory_path);
    let table_definition: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);

    let read_transaction = self.database.begin_read()?;
    let table = match read_transaction.open_table(table_definition) {
      Ok(table) => table,
      Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
      Err(error) => return Err(DirectoryError::RedbTableError(error)),
    };

    Ok(table.len()?)
  }

  /// Check if a directory exists (has the table been created).
  pub fn directory_exists(&self, directory_path: &str) -> Result<bool> {
    let table_name = Self::table_name(directory_path);

    let read_transaction = self.database.begin_read()?;
    for handle in read_transaction.list_tables()? {
      if handle.name() == table_name {
        return Ok(true);
      }
    }
    Ok(false)
  }

  /// Create an empty directory (just ensures the table exists).
  pub fn create_directory(&self, directory_path: &str) -> Result<()> {
    let table_name = Self::table_name(directory_path);
    let table_definition: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);

    let write_transaction = self.database.begin_write()?;
    {
      // Opening the table creates it if it doesn't exist.
      let _table = write_transaction.open_table(table_definition)?;
    }
    write_transaction.commit()?;
    Ok(())
  }

  /// Delete an entire directory (drops the redb table).
  pub fn delete_directory(&self, directory_path: &str) -> Result<()> {
    let table_name = Self::table_name(directory_path);
    let table_definition: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);

    let write_transaction = self.database.begin_write()?;
    write_transaction.delete_table(table_definition)?;
    write_transaction.commit()?;
    Ok(())
  }

  /// List subdirectories of a path (scan entries with type Directory).
  pub fn list_subdirectories(&self, directory_path: &str) -> Result<Vec<String>> {
    let entries = self.list_entries(directory_path)?;
    let subdirectories = entries
      .into_iter()
      .filter(|entry| entry.entry_type == EntryType::Directory)
      .map(|entry| entry.name)
      .collect();
    Ok(subdirectories)
  }
}
