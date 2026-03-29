use chrono::{DateTime, Utc};
use redb::{backends::InMemoryBackend, Database, ReadableDatabase, ReadableTable, TableDefinition};
use thiserror::Error;
use uuid::Uuid;

use super::document::{Document, MetadataUpdates};
use crate::auth::api_key::ApiKeyRecord;
use crate::auth::magic_link::MagicLinkRecord;
use crate::auth::refresh::RefreshTokenRecord;

/// Each table stores documents keyed by UUID (as 128-bit value).
/// The value is a byte blob we encode ourselves (not user data format --
/// this is our internal envelope around the raw user bytes).
///
/// Internal storage layout per document value (all big-endian):
///   [16 bytes] document_id (UUID)
///   [8 bytes]  created_at  (millis since epoch, i64)
///   [8 bytes]  updated_at  (millis since epoch, i64)
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
  database: std::sync::Arc<Database>,
}

impl RedbStorage {
  /// Get a shared reference to the underlying redb Database.
  ///
  /// This is used by subsystems (e.g. PluginManager) that need direct
  /// access to the same database instance.
  pub fn database_arc(&self) -> std::sync::Arc<Database> {
    self.database.clone()
  }
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

  let total_size = 16 + 8 + 8 + 4 + content_type_bytes.len() + document.data.len();
  let mut buffer = Vec::with_capacity(total_size);

  buffer.extend_from_slice(document.document_id.as_bytes());
  buffer.extend_from_slice(&document.created_at.timestamp_millis().to_be_bytes());
  buffer.extend_from_slice(&document.updated_at.timestamp_millis().to_be_bytes());
  buffer.extend_from_slice(&content_type_length.to_be_bytes());
  buffer.extend_from_slice(content_type_bytes);
  buffer.extend_from_slice(&document.data);

  buffer
}

fn decode_document(bytes: &[u8]) -> Result<Document> {
  // Minimum size: 16 + 8 + 8 + 4 = 36 bytes
  if bytes.len() < 36 {
    return Err(StorageError::CorruptData);
  }

  let document_id = Uuid::from_bytes(bytes[0..16].try_into().unwrap());

  let created_at_millis = i64::from_be_bytes(bytes[16..24].try_into().unwrap());
  let created_at = DateTime::from_timestamp_millis(created_at_millis)
    .ok_or(StorageError::CorruptData)?;

  let updated_at_millis = i64::from_be_bytes(bytes[24..32].try_into().unwrap());
  let updated_at = DateTime::from_timestamp_millis(updated_at_millis)
    .ok_or(StorageError::CorruptData)?;

  let content_type_length =
    u32::from_be_bytes(bytes[32..36].try_into().unwrap()) as usize;

  if bytes.len() < 36 + content_type_length {
    return Err(StorageError::CorruptData);
  }

  let content_type = if content_type_length > 0 {
    let content_type_str =
      std::str::from_utf8(&bytes[36..36 + content_type_length])
        .map_err(|_| StorageError::CorruptData)?;
    Some(content_type_str.to_string())
  } else {
    None
  };

  let data_start = 36 + content_type_length;
  let data = bytes[data_start..].to_vec();

  Ok(Document {
    document_id,
    created_at,
    updated_at,
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
    Ok(Self {
      database: std::sync::Arc::new(database),
    })
  }

  /// Create an in-memory database (no disk I/O -- ideal for tests).
  pub fn new_in_memory() -> Result<Self> {
    let backend = InMemoryBackend::new();
    let database = Database::builder().create_with_backend(backend)?;
    Ok(Self {
      database: std::sync::Arc::new(database),
    })
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

  /// Retrieve a document by ID. Returns None if not found.
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
    Ok(Some(document))
  }

  /// Update a document's raw data. Bumps `updated_at`, preserves
  /// `created_at` and other metadata.
  /// Returns DocumentNotFound if the document does not exist.
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

  /// Update only metadata fields on a document.
  /// Returns DocumentNotFound if the document does not exist.
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

  /// Hard-delete a document (removes the record entirely).
  /// Returns DocumentNotFound if the document does not exist.
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

      let removed = table.remove(key)?;
      if removed.is_none() {
        return Err(StorageError::DocumentNotFound(document_id));
      }
    }

    write_transaction.commit()?;
    Ok(())
  }

  /// List all documents in a table.
  pub fn list_documents(
    &self,
    table_name: &str,
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
      documents.push(document);
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

  /// Look up a single API key record by key_id prefix (first 16 hex chars
  /// of the UUID, no dashes).
  pub fn get_system_api_key(&self, key_id_prefix: &str) -> Result<Option<ApiKeyRecord>> {
    let read_transaction = self.database.begin_read()?;
    let table = match read_transaction.open_table(Self::API_KEYS_TABLE) {
      Ok(table) => table,
      Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
      Err(error) => return Err(StorageError::RedbTable(error)),
    };

    for result in table.iter()? {
      let (_, value_guard) = result?;
      let record: ApiKeyRecord = serde_json::from_slice(value_guard.value())
        .map_err(|_| StorageError::CorruptData)?;
      let record_prefix = &record.key_id.simple().to_string()[..16];
      if record_prefix == key_id_prefix {
        return Ok(Some(record));
      }
    }

    Ok(None)
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

  // -------------------------------------------------------------------------
  // System config storage (table: "_system:config")
  // -------------------------------------------------------------------------

  const CONFIG_TABLE: TableDefinition<'static, &'static str, &'static [u8]> =
    TableDefinition::new("_system:config");

  /// Store a config value by key.
  pub fn store_config(&self, key: &str, value: &[u8]) -> Result<()> {
    let write_transaction = self.database.begin_write()?;
    {
      let mut table = write_transaction.open_table(Self::CONFIG_TABLE)?;
      table.insert(key, value)?;
    }
    write_transaction.commit()?;
    Ok(())
  }

  /// Retrieve a config value by key.
  pub fn get_config(&self, key: &str) -> Result<Option<Vec<u8>>> {
    let read_transaction = self.database.begin_read()?;
    let table = match read_transaction.open_table(Self::CONFIG_TABLE) {
      Ok(table) => table,
      Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
      Err(error) => return Err(StorageError::RedbTable(error)),
    };

    match table.get(key)? {
      Some(guard) => Ok(Some(guard.value().to_vec())),
      None => Ok(None),
    }
  }

  // -------------------------------------------------------------------------
  // Magic link storage (table: "_system:magic_links")
  // -------------------------------------------------------------------------

  const MAGIC_LINKS_TABLE: TableDefinition<'static, &'static str, &'static [u8]> =
    TableDefinition::new("_system:magic_links");

  /// Store a magic link record, keyed by code_hash.
  pub fn store_magic_link(
    &self,
    code_hash: &str,
    email: &str,
    expires_at: DateTime<Utc>,
  ) -> Result<()> {
    let record = MagicLinkRecord {
      code_hash: code_hash.to_string(),
      email: email.to_string(),
      created_at: Utc::now(),
      expires_at,
      is_used: false,
    };

    let encoded = serde_json::to_vec(&record)
      .map_err(|_| StorageError::CorruptData)?;

    let write_transaction = self.database.begin_write()?;
    {
      let mut table = write_transaction.open_table(Self::MAGIC_LINKS_TABLE)?;
      table.insert(code_hash, encoded.as_slice())?;
    }
    write_transaction.commit()?;
    Ok(())
  }

  /// Retrieve a magic link record by code_hash.
  pub fn get_magic_link(&self, code_hash: &str) -> Result<Option<MagicLinkRecord>> {
    let read_transaction = self.database.begin_read()?;
    let table = match read_transaction.open_table(Self::MAGIC_LINKS_TABLE) {
      Ok(table) => table,
      Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
      Err(error) => return Err(StorageError::RedbTable(error)),
    };

    match table.get(code_hash)? {
      Some(guard) => {
        let record: MagicLinkRecord = serde_json::from_slice(guard.value())
          .map_err(|_| StorageError::CorruptData)?;
        Ok(Some(record))
      }
      None => Ok(None),
    }
  }

  /// Mark a magic link as used.
  pub fn mark_magic_link_used(&self, code_hash: &str) -> Result<()> {
    let write_transaction = self.database.begin_write()?;
    {
      let mut table = write_transaction.open_table(Self::MAGIC_LINKS_TABLE)?;

      let existing_bytes = match table.get(code_hash)? {
        Some(guard) => guard.value().to_vec(),
        None => return Err(StorageError::CorruptData),
      };

      let mut record: MagicLinkRecord = serde_json::from_slice(&existing_bytes)
        .map_err(|_| StorageError::CorruptData)?;
      record.is_used = true;

      let encoded = serde_json::to_vec(&record)
        .map_err(|_| StorageError::CorruptData)?;
      table.insert(code_hash, encoded.as_slice())?;
    }
    write_transaction.commit()?;
    Ok(())
  }

  /// Remove all expired magic links. Returns the count of removed links.
  pub fn cleanup_expired_magic_links(&self) -> Result<u64> {
    let now = Utc::now();

    // First pass: read and collect expired keys.
    let keys_to_remove = {
      let read_transaction = self.database.begin_read()?;
      let table = match read_transaction.open_table(Self::MAGIC_LINKS_TABLE) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(error) => return Err(StorageError::RedbTable(error)),
      };

      let mut expired_keys: Vec<String> = Vec::new();
      for result in table.iter()? {
        let (key_guard, value_guard) = result?;
        let record: MagicLinkRecord = serde_json::from_slice(value_guard.value())
          .map_err(|_| StorageError::CorruptData)?;
        if record.expires_at < now {
          expired_keys.push(key_guard.value().to_string());
        }
      }
      expired_keys
    };

    if keys_to_remove.is_empty() {
      return Ok(0);
    }

    // Second pass: remove the expired keys.
    let write_transaction = self.database.begin_write()?;
    let mut removed_count = 0u64;
    {
      let mut table = write_transaction.open_table(Self::MAGIC_LINKS_TABLE)?;
      for key in &keys_to_remove {
        table.remove(key.as_str())?;
        removed_count += 1;
      }
    }
    write_transaction.commit()?;
    Ok(removed_count)
  }

  // -------------------------------------------------------------------------
  // Refresh token storage (table: "_system:refresh_tokens")
  // -------------------------------------------------------------------------

  const REFRESH_TOKENS_TABLE: TableDefinition<'static, &'static str, &'static [u8]> =
    TableDefinition::new("_system:refresh_tokens");

  /// Store a refresh token record, keyed by token_hash.
  pub fn store_refresh_token(
    &self,
    token_hash: &str,
    user_subject: &str,
    expires_at: DateTime<Utc>,
  ) -> Result<()> {
    let record = RefreshTokenRecord {
      token_hash: token_hash.to_string(),
      user_subject: user_subject.to_string(),
      created_at: Utc::now(),
      expires_at,
      is_revoked: false,
    };

    let encoded = serde_json::to_vec(&record)
      .map_err(|_| StorageError::CorruptData)?;

    let write_transaction = self.database.begin_write()?;
    {
      let mut table = write_transaction.open_table(Self::REFRESH_TOKENS_TABLE)?;
      table.insert(token_hash, encoded.as_slice())?;
    }
    write_transaction.commit()?;
    Ok(())
  }

  /// Retrieve a refresh token record by token_hash.
  pub fn get_refresh_token(&self, token_hash: &str) -> Result<Option<RefreshTokenRecord>> {
    let read_transaction = self.database.begin_read()?;
    let table = match read_transaction.open_table(Self::REFRESH_TOKENS_TABLE) {
      Ok(table) => table,
      Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
      Err(error) => return Err(StorageError::RedbTable(error)),
    };

    match table.get(token_hash)? {
      Some(guard) => {
        let record: RefreshTokenRecord = serde_json::from_slice(guard.value())
          .map_err(|_| StorageError::CorruptData)?;
        Ok(Some(record))
      }
      None => Ok(None),
    }
  }

  /// Revoke a refresh token by setting is_revoked = true.
  pub fn revoke_refresh_token(&self, token_hash: &str) -> Result<()> {
    let write_transaction = self.database.begin_write()?;
    {
      let mut table = write_transaction.open_table(Self::REFRESH_TOKENS_TABLE)?;

      let existing_bytes = match table.get(token_hash)? {
        Some(guard) => guard.value().to_vec(),
        None => return Err(StorageError::CorruptData),
      };

      let mut record: RefreshTokenRecord = serde_json::from_slice(&existing_bytes)
        .map_err(|_| StorageError::CorruptData)?;
      record.is_revoked = true;

      let encoded = serde_json::to_vec(&record)
        .map_err(|_| StorageError::CorruptData)?;
      table.insert(token_hash, encoded.as_slice())?;
    }
    write_transaction.commit()?;
    Ok(())
  }
}
