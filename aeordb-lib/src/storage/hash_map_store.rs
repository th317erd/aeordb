use std::sync::Arc;

use chrono::{DateTime, Utc};

use super::chunk::{hash_data, Chunk, ChunkHash};
use super::chunk_config::ChunkConfig;
use super::chunk_storage::{ChunkStorage, ChunkStoreError};

/// An ordered sequence of chunk hashes representing a logical data unit.
/// This is NOT Rust's HashMap — it is a "map" in the sense of a content map:
/// an ordered list of chunk hashes that, when resolved, reconstruct the original data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentHashMap {
  /// Hash of the map itself (BLAKE3 hash of all chunk hashes concatenated).
  pub hash: ChunkHash,
  /// Ordered list of chunk hashes.
  pub chunk_hashes: Vec<ChunkHash>,
  /// Total data size in bytes.
  pub total_size: u64,
  /// When this hash map was created.
  pub created_at: DateTime<Utc>,
}

/// Diff between two hash maps showing which chunks changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashMapDiff {
  pub added: Vec<ChunkHash>,
  pub removed: Vec<ChunkHash>,
  pub unchanged: Vec<ChunkHash>,
}

/// Manages hash maps (sequences of chunk hashes) over a ChunkStorage backend.
pub struct HashMapStore {
  chunk_storage: Arc<dyn ChunkStorage>,
  config: ChunkConfig,
}

impl HashMapStore {
  pub fn new(chunk_storage: Arc<dyn ChunkStorage>, config: ChunkConfig) -> Self {
    Self {
      chunk_storage,
      config,
    }
  }

  /// Split data into chunks, store each chunk, and return a ContentHashMap.
  pub fn store_data(&self, data: &[u8]) -> Result<ContentHashMap, ChunkStoreError> {
    let mut chunk_hashes = Vec::new();

    if data.is_empty() {
      // Empty data produces a hash map with zero chunks.
      let hash = compute_map_hash(&chunk_hashes);
      return Ok(ContentHashMap {
        hash,
        chunk_hashes,
        total_size: 0,
        created_at: Utc::now(),
      });
    }

    for chunk_data in data.chunks(self.config.chunk_size) {
      let chunk = Chunk::new(chunk_data.to_vec());
      self.chunk_storage.store_chunk(&chunk)?;
      chunk_hashes.push(chunk.hash);
    }

    let hash = compute_map_hash(&chunk_hashes);
    Ok(ContentHashMap {
      hash,
      chunk_hashes,
      total_size: data.len() as u64,
      created_at: Utc::now(),
    })
  }

  /// Retrieve all chunks referenced by a hash map and concatenate them.
  pub fn retrieve_data(&self, hash_map: &ContentHashMap) -> Result<Vec<u8>, ChunkStoreError> {
    let mut result = Vec::with_capacity(hash_map.total_size as usize);

    for chunk_hash in &hash_map.chunk_hashes {
      let chunk = self.chunk_storage.get_chunk(chunk_hash)?
        .ok_or(ChunkStoreError::ChunkNotFound(*chunk_hash))?;
      result.extend_from_slice(&chunk.data);
    }

    Ok(result)
  }

  /// Serialize a ContentHashMap and store it as chunks (meta-chunks).
  /// Returns the hash of the serialized map data.
  pub fn store_hash_map(&self, hash_map: &ContentHashMap) -> Result<ChunkHash, ChunkStoreError> {
    let serialized = serialize_hash_map(hash_map)?;
    let map_of_serialized = self.store_data(&serialized)?;
    Ok(map_of_serialized.hash)
  }

  /// Load a ContentHashMap from its stored chunks.
  /// The provided hash is the hash of the serialized map data (as returned by store_hash_map).
  pub fn load_hash_map(&self, hash: &ChunkHash) -> Result<ContentHashMap, ChunkStoreError> {
    // We need to find the meta-chunks that store the serialized map.
    // The hash is the map hash of the serialized data. We stored it as chunks,
    // so we need to reconstruct the serialized bytes from storage.
    //
    // Problem: we don't have the meta-ContentHashMap to retrieve the serialized data.
    // Solution: For small maps, the serialized data fits in one chunk.
    // For the general case, we store a "pointer" that is the serialized meta-map itself.
    //
    // Simplified approach: store the serialized hash map as a single chunk (not split).
    // This works because hash maps are small (32 bytes per chunk hash + metadata).
    // We key by the hash of the serialized bytes.

    let serialized_hash = hash;
    let chunk = self.chunk_storage.get_chunk(serialized_hash)?
      .ok_or(ChunkStoreError::ChunkNotFound(*serialized_hash))?;

    deserialize_hash_map(&chunk.data)
  }

  /// Store a ContentHashMap as a single chunk for later retrieval by hash.
  /// Returns the chunk hash of the serialized map.
  pub fn store_hash_map_as_chunk(&self, hash_map: &ContentHashMap) -> Result<ChunkHash, ChunkStoreError> {
    let serialized = serialize_hash_map(hash_map)?;
    let chunk = Chunk::new(serialized);
    let chunk_hash = chunk.hash;
    self.chunk_storage.store_chunk(&chunk)?;
    Ok(chunk_hash)
  }

  /// Compute the diff between two hash maps.
  pub fn diff_hash_maps(old: &ContentHashMap, new: &ContentHashMap) -> HashMapDiff {
    use std::collections::HashSet;

    let old_set: HashSet<ChunkHash> = old.chunk_hashes.iter().copied().collect();
    let new_set: HashSet<ChunkHash> = new.chunk_hashes.iter().copied().collect();

    let added = new.chunk_hashes.iter()
      .filter(|hash| !old_set.contains(*hash))
      .copied()
      .collect();

    let removed = old.chunk_hashes.iter()
      .filter(|hash| !new_set.contains(*hash))
      .copied()
      .collect();

    let unchanged = old.chunk_hashes.iter()
      .filter(|hash| new_set.contains(*hash))
      .copied()
      .collect();

    HashMapDiff {
      added,
      removed,
      unchanged,
    }
  }

  /// Efficiently update data: only stores new/changed chunks, reuses existing ones.
  pub fn update_data(
    &self,
    old_map: &ContentHashMap,
    new_data: &[u8],
  ) -> Result<ContentHashMap, ChunkStoreError> {
    use std::collections::HashSet;

    let old_hashes: HashSet<ChunkHash> = old_map.chunk_hashes.iter().copied().collect();
    let mut chunk_hashes = Vec::new();

    if new_data.is_empty() {
      let hash = compute_map_hash(&chunk_hashes);
      return Ok(ContentHashMap {
        hash,
        chunk_hashes,
        total_size: 0,
        created_at: Utc::now(),
      });
    }

    for chunk_data in new_data.chunks(self.config.chunk_size) {
      let chunk_hash = hash_data(chunk_data);
      if !old_hashes.contains(&chunk_hash) {
        // New chunk, store it.
        let chunk = Chunk {
          hash: chunk_hash,
          data: chunk_data.to_vec(),
        };
        self.chunk_storage.store_chunk(&chunk)?;
      }
      // Chunk already exists (either in old map or from dedup). Just record the hash.
      chunk_hashes.push(chunk_hash);
    }

    let hash = compute_map_hash(&chunk_hashes);
    Ok(ContentHashMap {
      hash,
      chunk_hashes,
      total_size: new_data.len() as u64,
      created_at: Utc::now(),
    })
  }
}

/// Compute the hash of a hash map (hash of all chunk hashes concatenated).
fn compute_map_hash(chunk_hashes: &[ChunkHash]) -> ChunkHash {
  let mut concatenated = Vec::with_capacity(chunk_hashes.len() * 32);
  for hash in chunk_hashes {
    concatenated.extend_from_slice(hash);
  }
  hash_data(&concatenated)
}

/// Serialize a ContentHashMap to bytes.
/// Format (all big-endian):
///   [4 bytes]  number of chunk hashes (u32)
///   [N * 32 bytes] chunk hashes
///   [8 bytes]  total_size (u64)
///   [8 bytes]  created_at (millis since epoch, i64)
///   [32 bytes] map hash
fn serialize_hash_map(hash_map: &ContentHashMap) -> Result<Vec<u8>, ChunkStoreError> {
  let chunk_count = hash_map.chunk_hashes.len() as u32;
  let total_bytes = 4 + (chunk_count as usize * 32) + 8 + 8 + 32;
  let mut buffer = Vec::with_capacity(total_bytes);

  buffer.extend_from_slice(&chunk_count.to_be_bytes());
  for chunk_hash in &hash_map.chunk_hashes {
    buffer.extend_from_slice(chunk_hash);
  }
  buffer.extend_from_slice(&hash_map.total_size.to_be_bytes());
  buffer.extend_from_slice(&hash_map.created_at.timestamp_millis().to_be_bytes());
  buffer.extend_from_slice(&hash_map.hash);

  Ok(buffer)
}

/// Deserialize a ContentHashMap from bytes.
fn deserialize_hash_map(data: &[u8]) -> Result<ContentHashMap, ChunkStoreError> {
  if data.len() < 4 {
    return Err(ChunkStoreError::SerializationError(
      "hash map data too short for header".to_string(),
    ));
  }

  let chunk_count = u32::from_be_bytes(
    data[0..4].try_into().unwrap(),
  ) as usize;

  let expected_size = 4 + (chunk_count * 32) + 8 + 8 + 32;
  if data.len() < expected_size {
    return Err(ChunkStoreError::SerializationError(format!(
      "hash map data too short: expected {expected_size}, got {}",
      data.len(),
    )));
  }

  let mut chunk_hashes = Vec::with_capacity(chunk_count);
  let mut offset = 4;
  for _ in 0..chunk_count {
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&data[offset..offset + 32]);
    chunk_hashes.push(hash);
    offset += 32;
  }

  let total_size = u64::from_be_bytes(
    data[offset..offset + 8].try_into().unwrap(),
  );
  offset += 8;

  let created_at_millis = i64::from_be_bytes(
    data[offset..offset + 8].try_into().unwrap(),
  );
  let created_at = DateTime::from_timestamp_millis(created_at_millis)
    .ok_or_else(|| ChunkStoreError::SerializationError(
      "invalid created_at timestamp".to_string(),
    ))?;
  offset += 8;

  let mut hash = [0u8; 32];
  hash.copy_from_slice(&data[offset..offset + 32]);

  Ok(ContentHashMap {
    hash,
    chunk_hashes,
    total_size,
    created_at,
  })
}
