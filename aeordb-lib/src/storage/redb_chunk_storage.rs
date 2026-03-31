// LEGACY: Used only by ChunkStore -> PathResolver -> /fs/ routes.
// Remove once /fs/ routes are migrated to the engine.

use std::sync::Arc;

use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};

use super::chunk::{Chunk, ChunkHash};
use super::chunk_header::{ChunkHeader, HEADER_SIZE};
use super::chunk_storage::{ChunkStorage, ChunkStoreError};

/// Table definition for chunk storage: key = 32-byte hash (as &[u8]), value = header+data.
const CHUNKS_TABLE: TableDefinition<&[u8], &[u8]> =
  TableDefinition::new("_chunks:data");

/// Redb-backed implementation of ChunkStorage.
pub struct RedbChunkStorage {
  database: Arc<Database>,
}

impl RedbChunkStorage {
  pub fn new(database: Arc<Database>) -> Self {
    Self { database }
  }
}

/// Serialize a chunk (header + data) into a single byte vector.
fn serialize_chunk(chunk: &Chunk) -> Vec<u8> {
  let header_bytes = chunk.header.serialize();
  let mut buffer = Vec::with_capacity(HEADER_SIZE + chunk.data.len());
  buffer.extend_from_slice(&header_bytes);
  buffer.extend_from_slice(&chunk.data);
  buffer
}

/// Deserialize a chunk from stored bytes (header + data).
fn deserialize_chunk(hash: &ChunkHash, stored: &[u8]) -> Result<Chunk, ChunkStoreError> {
  if stored.len() < HEADER_SIZE {
    return Err(ChunkStoreError::SerializationError(format!(
      "stored chunk too short: {} bytes, need at least {HEADER_SIZE}",
      stored.len(),
    )));
  }

  let header = ChunkHeader::deserialize_from_slice(stored).map_err(|error| {
    ChunkStoreError::SerializationError(format!("chunk header: {error}"))
  })?;
  let data = stored[HEADER_SIZE..].to_vec();

  Ok(Chunk {
    hash: *hash,
    data,
    header,
  })
}

impl ChunkStorage for RedbChunkStorage {
  fn store_chunk(&self, chunk: &Chunk) -> Result<(), ChunkStoreError> {
    let write_transaction = self.database.begin_write().map_err(|error| {
      ChunkStoreError::RedbError(format!("begin_write: {error}"))
    })?;
    {
      let mut table = write_transaction.open_table(CHUNKS_TABLE).map_err(|error| {
        ChunkStoreError::RedbError(format!("open_table: {error}"))
      })?;
      // Only insert if not already present (content-addressed dedup).
      let key: &[u8] = &chunk.hash;
      let exists = table.get(key).map_err(|error| {
        ChunkStoreError::RedbError(format!("get: {error}"))
      })?.is_some();
      if exists {
        tracing::debug!(
          hash_prefix = %hex::encode(&chunk.hash[..8]),
          "Chunk dedup: already exists, skipped write"
        );
        metrics::counter!(crate::metrics::definitions::CHUNKS_DEDUPLICATED_TOTAL).increment(1);
      } else {
        tracing::trace!(
          hash_prefix = %hex::encode(&chunk.hash[..8]),
          size = chunk.data.len(),
          "Chunk stored (redb)"
        );
        let serialized = serialize_chunk(chunk);
        table.insert(key, serialized.as_slice()).map_err(|error| {
          ChunkStoreError::RedbError(format!("insert: {error}"))
        })?;
      }
    }
    write_transaction.commit().map_err(|error| {
      ChunkStoreError::RedbError(format!("commit: {error}"))
    })?;
    Ok(())
  }

  fn get_chunk(&self, hash: &ChunkHash) -> Result<Option<Chunk>, ChunkStoreError> {
    let read_transaction = self.database.begin_read().map_err(|error| {
      ChunkStoreError::RedbError(format!("begin_read: {error}"))
    })?;
    let table = match read_transaction.open_table(CHUNKS_TABLE) {
      Ok(table) => table,
      Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
      Err(error) => return Err(ChunkStoreError::RedbError(format!("open_table: {error}"))),
    };

    let key: &[u8] = hash;
    match table.get(key).map_err(|error| {
      ChunkStoreError::RedbError(format!("get: {error}"))
    })? {
      Some(guard) => {
        let stored = guard.value();
        tracing::trace!(
          hash_prefix = %hex::encode(&hash[..8]),
          "Chunk retrieved (redb)"
        );
        Ok(Some(deserialize_chunk(hash, stored)?))
      }
      None => Ok(None),
    }
  }

  fn has_chunk(&self, hash: &ChunkHash) -> Result<bool, ChunkStoreError> {
    let read_transaction = self.database.begin_read().map_err(|error| {
      ChunkStoreError::RedbError(format!("begin_read: {error}"))
    })?;
    let table = match read_transaction.open_table(CHUNKS_TABLE) {
      Ok(table) => table,
      Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
      Err(error) => return Err(ChunkStoreError::RedbError(format!("open_table: {error}"))),
    };

    let key: &[u8] = hash;
    Ok(table.get(key).map_err(|error| {
      ChunkStoreError::RedbError(format!("get: {error}"))
    })?.is_some())
  }

  fn remove_chunk(&self, hash: &ChunkHash) -> Result<bool, ChunkStoreError> {
    let write_transaction = self.database.begin_write().map_err(|error| {
      ChunkStoreError::RedbError(format!("begin_write: {error}"))
    })?;
    let removed = {
      let mut table = write_transaction.open_table(CHUNKS_TABLE).map_err(|error| {
        ChunkStoreError::RedbError(format!("open_table: {error}"))
      })?;
      let key: &[u8] = hash;
      let result = table.remove(key).map_err(|error| {
        ChunkStoreError::RedbError(format!("remove: {error}"))
      })?;
      result.is_some()
    };
    write_transaction.commit().map_err(|error| {
      ChunkStoreError::RedbError(format!("commit: {error}"))
    })?;
    Ok(removed)
  }

  fn chunk_count(&self) -> Result<u64, ChunkStoreError> {
    let read_transaction = self.database.begin_read().map_err(|error| {
      ChunkStoreError::RedbError(format!("begin_read: {error}"))
    })?;
    let table = match read_transaction.open_table(CHUNKS_TABLE) {
      Ok(table) => table,
      Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
      Err(error) => return Err(ChunkStoreError::RedbError(format!("open_table: {error}"))),
    };
    table.len().map_err(|error| {
      ChunkStoreError::RedbError(format!("len: {error}"))
    })
  }

  fn list_chunk_hashes(&self) -> Result<Vec<ChunkHash>, ChunkStoreError> {
    let read_transaction = self.database.begin_read().map_err(|error| {
      ChunkStoreError::RedbError(format!("begin_read: {error}"))
    })?;
    let table = match read_transaction.open_table(CHUNKS_TABLE) {
      Ok(table) => table,
      Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
      Err(error) => return Err(ChunkStoreError::RedbError(format!("open_table: {error}"))),
    };

    let mut hashes = Vec::new();
    for result in table.iter().map_err(|error| {
      ChunkStoreError::RedbError(format!("iter: {error}"))
    })? {
      let (key_guard, _) = result.map_err(|error| {
        ChunkStoreError::RedbError(format!("iter next: {error}"))
      })?;
      let key_bytes = key_guard.value();
      let mut hash = [0u8; 32];
      if key_bytes.len() == 32 {
        hash.copy_from_slice(key_bytes);
        hashes.push(hash);
      }
    }
    Ok(hashes)
  }
}
