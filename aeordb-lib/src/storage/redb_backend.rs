use chrono::{DateTime, Utc};
use redb::{backends::InMemoryBackend, Database, ReadableDatabase, ReadableTable, TableDefinition};
use thiserror::Error;
use uuid::Uuid;

use super::document::{Document, MetadataUpdates};
use crate::auth::api_key::ApiKeyRecord;

/// Each table stores documents keyed by UUID (as 128-bit value).
/// The value is a byte blob we encode ourselves (not user data format --
/// this is our internal envelope around the raw user bytes).
///
/// Internal storage layout per document value (all big-endian):
///   [16 bytes] document_id (UUID)
///   [8 bytes]  created_at  (millis since epoch, i64)
///   [8 bytes]  updated_at  (millis since epoch, i64)
///   [1 byte]   is_deleted  (0 or 1)
///   [4 bytes]  content_type length (u32, 0 means None)
///   [N bytes]  content_type UTF-8 string (if length > 0)
///   [remaining] raw user data
///

#[derive(Debug, Error)]
pub enum StorageError {
  #[error("redb error: {0}")]
  Redb(#[from] redb::Error),

  #[error("redb database error: {0}")]
  RedbDatabase(#[from] redb::DatabaseError),

  #[error("redb table error: {0}")]
  RedbTable(#[from] redb::TableError),

  #[error("redb transaction error: {0}")]
  RedbTransaction(#[from] redb::TransactionError),

  #[error("redb storage error: {0}")]
  RedbStorage(#[from] redb::StorageError),

  #[error("redb commit error: {0}")]
  RedbCommit(#[from] redb::CommitError),

  #[error("document not found: {0}")]
  DocumentNotFound(Uuid),

  #[error("corrupt document data")]
  CorruptData,
}

pub type Result<T> = std::result::Result<T, StorageError>;

pub struct RedbStorage {
  database: Database,
}

// ---------------------------------------------------------------------------
// Internal serialisation helpers
// ---------------------------------------------------------------------------

fn encode_document(document: &Document) -> Vec<u8> {
  let content_type_bytes = document
    .content_type
    .as_ref()
    .map(|s| s.as_bytes())
    .unwrap_or(&[]);
  let content_type_length = content_type_bytes.len() as u32;

  let total_size = 16 + 8 + 8 + 1 + 4 + content_type_bytes.len() + document.data.len();
  let mut buffer = Vec::with_capacity(total_size);

  buffer.extend_from_slice(document.document_id.as_bytes());
  buffer.extend_from_slice(&document.created_at.timestamp_millis().to_be_bytes());
  buffer.extend_from_slice(&document.updated_at.timestamp_millis().to_be_bytes());
  buffer.push(if document.is_deleted { 1 } else { 0 });
  buffer.extend_from_slice(&content_type_length.to_be_bytes());
  buffer.extend_from_slice(content_type_bytes);
  buffer.extend_from_slice(&document.data);

  buffer
}

fn decode_document(bytes: &[u8]) -> Result<Document> {
  // Minimum size: 16 + 8 + 8 + 1 + 4 = 37 bytes
  if bytes.len() < 37 {
    return Err(StorageError::CorruptData);
  }

  let document_id = Uuid::from_bytes(bytes[0..16].try_into().unwrap());

  let created_at_millis = i64::from_be_bytes(bytes[16..24].try_into().unwrap());
  let created_at = DateTime::from_timestamp_millis(created_at_millis)
    .ok_or(StorageError::CorruptData)?;

  let updated_at_millis = i64::from_be_bytes(bytes[24..32].try_into().unwrap());
  let updated_at = DateTime::from_timestamp_millis(updated_at_millis)
    .ok_or(StorageError::CorruptData)?;

  let is_deleted = bytes[32] != 0;

  let content_type_length =
    u32::from_be_bytes(bytes[33..37].try_into().unwrap()) as usize;

  if bytes.len() < 37 + content_type_length {
    return Err(StorageError::CorruptData);
  }

  let content_type = if content_type_length > 0 {
    let content_type_str =
      std::str::from_utf8(&bytes[37..37 + content_type_length])
        .map_err(|_| StorageError::CorruptData)?;
    Some(content_type_str.to_string())
  } else {
    None
  };

  let data_start = 37 + content_type_length;
  let data = bytes[data_start..].to_vec();

  Ok(Document {
    document_id,
    created_at,
    updated_at,
    is_deleted,
    data,
    content_type,
  })
}

/// Truncate a DateTime to millisecond precision to match our storage format.
fn truncate_to_millis(datetime: DateTime<Utc>) -> DateTime<Utc> {
  DateTime::from_timestamp_millis(datetime.timestamp_millis())
    .expect("valid millis round-trip")
}

fn uuid_to_key(id: &Uuid) -> u128 {
  id.as_u128()
}

impl RedbStorage {
  /// Open or create a database at the given filesystem path.
  pub fn new(path: &str) -> Result<Self> {
    let database = Database::create(path)?;
    Ok(Self { database })
  }

  /// Create an in-memory database (no disk I/O -- ideal for tests).
  pub fn new_in_memory() -> Result<Self> {
    let backend = InMemoryBackend::new();
    let database = Database::builder().create_with_backend(backend)?;
    Ok(Self { database })
  }

  /// Create a new document with an auto-generated UUID.
  pub fn create_document(
    &self,
    table_name: &str,
    data: Vec<u8>,
    content_type: Option<String>,
  ) -> Result<Document> {
    self.create_document_with_id(table_name, Uuid::new_v4(), data, content_type)
  }

  /// Create a new document with a caller-supplied UUID.
  pub fn create_document_with_id(
    &self,
    table_name: &str,
    document_id: Uuid,
    data: Vec<u8>,
    content_type: Option<String>,
  ) -> Result<Document> {
    let now = truncate_to_millis(Utc::now());
    let document = Document {
      document_id,
      created_at: now,
      updated_at: now,
      is_deleted: false,
      data,
      content_type,
    };

    let table_definition: TableDefinition<u128, &[u8]> =
      TableDefinition::new(table_name);
    let write_transaction = self.database.begin_write()?;
    {
      let mut table = write_transaction.open_table(table_definition)?;
      let encoded = encode_document(&document);
      table.insert(uuid_to_key(&document_id), encoded.as_slice())?;
    }
    write_transaction.commit()?;

    Ok(document)
  }

  /// Retrieve a document by ID. Returns None if not found or if the
  /// document is soft-deleted.
  pub fn get_document(
    &self,
    table_name: &str,
    document_id: Uuid,
  ) -> Result<Option<Document>> {
    let table_definition: TableDefinition<u128, &[u8]> =
      TableDefinition::new(table_name);
    let read_transaction = self.database.begin_read()?;
    let table = match read_transaction.open_table(table_definition) {
      Ok(table) => table,
      Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
      Err(error) => return Err(StorageError::RedbTable(error)),
    };

    let key = uuid_to_key(&document_id);
    let guard = match table.get(key)? {
      Some(guard) => guard,
      None => return Ok(None),
    };

    let document = decode_document(guard.value())?;

    if document.is_deleted {
      return Ok(None);
    }

    Ok(Some(document))
  }

  /// Update a document's raw data. Bumps `updated_at`, preserves
  /// `created_at` and other metadata.
  pub fn update_document(
    &self,
    table_name: &str,
    document_id: Uuid,
    data: Vec<u8>,
  ) -> Result<Document> {
    let table_definition: TableDefinition<u128, &[u8]> =
      TableDefinition::new(table_name);
    let write_transaction = self.database.begin_write()?;

    let document = {
      let mut table = write_transaction.open_table(table_definition)?;
      let key = uuid_to_key(&document_id);

      let guard = table
        .get(key)?
        .ok_or(StorageError::DocumentNotFound(document_id))?;
      let existing = decode_document(guard.value())?;
      drop(guard);

      let updated = Document {
        document_id: existing.document_id,
        created_at: existing.created_at,
        updated_at: truncate_to_millis(Utc::now()),
        is_deleted: existing.is_deleted,
        data,
        content_type: existing.content_type,
      };

      let encoded = encode_document(&updated);
      table.insert(key, encoded.as_slice())?;
      updated
    };

    write_transaction.commit()?;
    Ok(document)
  }

  /// Update only metadata fields on a document (e.g. undelete).
  pub fn update_document_metadata(
    &self,
    table_name: &str,
    document_id: Uuid,
    metadata_updates: MetadataUpdates,
  ) -> Result<Document> {
    let table_definition: TableDefinition<u128, &[u8]> =
      TableDefinition::new(table_name);
    let write_transaction = self.database.begin_write()?;

    let document = {
      let mut table = write_transaction.open_table(table_definition)?;
      let key = uuid_to_key(&document_id);

      let guard = table
        .get(key)?
        .ok_or(StorageError::DocumentNotFound(document_id))?;
      let existing = decode_document(guard.value())?;
      drop(guard);

      let updated = Document {
        document_id: existing.document_id,
        created_at: metadata_updates
          .created_at
          .unwrap_or(existing.created_at),
        updated_at: metadata_updates
          .updated_at
          .unwrap_or_else(|| truncate_to_millis(Utc::now())),
        is_deleted: metadata_updates
          .is_deleted
          .unwrap_or(existing.is_deleted),
        data: existing.data,
        content_type: metadata_updates
          .content_type
          .unwrap_or(existing.content_type),
      };

      let encoded = encode_document(&updated);
      table.insert(key, encoded.as_slice())?;
      updated
    };

    write_transaction.commit()?;
    Ok(document)
  }

  /// Soft-delete a document (sets is_deleted = true).
  pub fn delete_document(
    &self,
    table_name: &str,
    document_id: Uuid,
  ) -> Result<()> {
    let table_definition: TableDefinition<u128, &[u8]> =
      TableDefinition::new(table_name);
    let write_transaction = self.database.begin_write()?;

    {
      let mut table = write_transaction.open_table(table_definition)?;
      let key = uuid_to_key(&document_id);

      let guard = table
        .get(key)?
        .ok_or(StorageError::DocumentNotFound(document_id))?;
      let existing = decode_document(guard.value())?;
      drop(guard);

      let updated = Document {
        is_deleted: true,
        updated_at: truncate_to_millis(Utc::now()),
        ..existing
      };

      let encoded = encode_document(&updated);
      table.insert(key, encoded.as_slice())?;
    }

    write_transaction.commit()?;
    Ok(())
  }

  /// List documents in a table. By default, soft-deleted documents are
  /// excluded. Pass `include_deleted: true` to include them.
  pub fn list_documents(
    &self,
    table_name: &str,
    include_deleted: bool,
  ) -> Result<Vec<Document>> {
    let table_definition: TableDefinition<u128, &[u8]> =
      TableDefinition::new(table_name);
    let read_transaction = self.database.begin_read()?;
    let table = match read_transaction.open_table(table_definition) {
      Ok(table) => table,
      Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
      Err(error) => return Err(StorageError::RedbTable(error)),
    };

    let mut documents = Vec::new();
    for result in table.iter()? {
      let (_, value_guard) = result?;
      let document = decode_document(value_guard.value())?;
      if include_deleted || !document.is_deleted {
        documents.push(document);
      }
    }

    Ok(documents)
  }

  // -------------------------------------------------------------------------
  // System API key storage (table: "_system:api_keys")
  // -------------------------------------------------------------------------

  const API_KEYS_TABLE: TableDefinition<'static, u128, &'static [u8]> =
    TableDefinition::new("_system:api_keys");

  /// Store an API key record.
  pub fn store_api_key(&self, record: &ApiKeyRecord) -> Result<()> {
    let encoded = serde_json::to_vec(record)
      .map_err(|_| StorageError::CorruptData)?;

    let write_transaction = self.database.begin_write()?;
    {
      let mut table = write_transaction.open_table(Self::API_KEYS_TABLE)?;
      let key = uuid_to_key(&record.key_id);
      table.insert(key, encoded.as_slice())?;
    }
    write_transaction.commit()?;
    Ok(())
  }

  /// List all API key records.
  pub fn list_system_api_keys(&self) -> Result<Vec<ApiKeyRecord>> {
    let read_transaction = self.database.begin_read()?;
    let table = match read_transaction.open_table(Self::API_KEYS_TABLE) {
      Ok(table) => table,
      Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
      Err(error) => return Err(StorageError::RedbTable(error)),
    };

    let mut records = Vec::new();
    for result in table.iter()? {
      let (_, value_guard) = result?;
      let record: ApiKeyRecord = serde_json::from_slice(value_guard.value())
        .map_err(|_| StorageError::CorruptData)?;
      records.push(record);
    }

    Ok(records)
  }

  /// Revoke an API key by setting is_revoked = true.
  /// Returns true if the key was found, false otherwise.
  pub fn revoke_api_key(&self, key_id: Uuid) -> Result<bool> {
    let write_transaction = self.database.begin_write()?;
    let found = {
      let mut table = write_transaction.open_table(Self::API_KEYS_TABLE)?;
      let key = uuid_to_key(&key_id);

      let existing_bytes = {
        match table.get(key)? {
          Some(guard) => {
            let bytes = guard.value().to_vec();
            Some(bytes)
          }
          None => None,
        }
      };

      match existing_bytes {
        Some(bytes) => {
          let mut record: ApiKeyRecord = serde_json::from_slice(&bytes)
            .map_err(|_| StorageError::CorruptData)?;
          record.is_revoked = true;
          let encoded = serde_json::to_vec(&record)
            .map_err(|_| StorageError::CorruptData)?;
          table.insert(key, encoded.as_slice())?;
          true
        }
        None => false,
      }
    };
    write_transaction.commit()?;
    Ok(found)
  }
}
