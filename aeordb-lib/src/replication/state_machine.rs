use std::io;
use std::io::Cursor;
use std::sync::Arc;

use futures_util::{Stream, StreamExt};
use openraft::entry::{RaftEntry, RaftPayload};
use openraft::storage::{EntryResponder, RaftSnapshotBuilder, RaftStateMachine};
use openraft::type_config::alias::{LogIdOf, SnapshotMetaOf, SnapshotOf, StoredMembershipOf};
use openraft::{EntryPayload, StoredMembership};

use super::TypeConfig;
use super::types::{RaftRequest, RaftResponse};
use crate::storage::chunk::Chunk;
use crate::storage::chunk_storage::ChunkStorage;

/// Raft state machine that applies committed operations to the chunk store.
///
/// Each committed log entry is decoded as a `RaftRequest` and applied to
/// the underlying `ChunkStorage` implementation.
pub struct ChunkStateMachine {
  chunk_storage: Arc<dyn ChunkStorage>,
  last_applied_log_id: Option<LogIdOf<TypeConfig>>,
  last_membership: StoredMembershipOf<TypeConfig>,
  /// Snapshot data: serialized summary of the current state.
  snapshot_data: Vec<u8>,
  /// Monotonically increasing snapshot index.
  snapshot_index: u64,
}

impl ChunkStateMachine {
  pub fn new(chunk_storage: Arc<dyn ChunkStorage>) -> Self {
    Self {
      chunk_storage,
      last_applied_log_id: None,
      last_membership: StoredMembership::default(),
      snapshot_data: Vec::new(),
      snapshot_index: 0,
    }
  }

  /// Apply a single Raft request to the chunk store. Returns a RaftResponse.
  fn apply_request(&self, request: &RaftRequest) -> RaftResponse {
    match request {
      RaftRequest::StoreChunk { hash, data } => {
        self.apply_store_chunk(hash, data)
      }
      RaftRequest::StoreHashMap { key, hash_map_data } => {
        self.apply_store_hash_map(key, hash_map_data)
      }
      RaftRequest::DeleteChunk { hash } => {
        self.apply_delete_chunk(hash)
      }
    }
  }

  fn apply_store_chunk(&self, hash: &[u8], data: &[u8]) -> RaftResponse {
    let chunk = Chunk::new(data.to_vec());

    // Verify the caller-provided hash matches the actual content hash.
    if hash.len() == 32 && hash != chunk.hash.as_slice() {
      return RaftResponse {
        success: false,
        message: Some("hash mismatch: provided hash does not match data content hash".to_string()),
      };
    }

    match self.chunk_storage.store_chunk(&chunk) {
      Ok(()) => RaftResponse {
        success: true,
        message: None,
      },
      Err(error) => RaftResponse {
        success: false,
        message: Some(format!("store_chunk failed: {}", error)),
      },
    }
  }

  fn apply_store_hash_map(&self, _key: &str, _hash_map_data: &[u8]) -> RaftResponse {
    // Hash map storage will be wired up when we integrate with the full
    // document layer. For now, acknowledge the request.
    RaftResponse {
      success: true,
      message: Some("hash map storage acknowledged (not yet wired)".to_string()),
    }
  }

  fn apply_delete_chunk(&self, hash: &[u8]) -> RaftResponse {
    if hash.len() != 32 {
      return RaftResponse {
        success: false,
        message: Some(format!("invalid chunk hash length: expected 32, got {}", hash.len())),
      };
    }

    let mut chunk_hash = [0u8; 32];
    chunk_hash.copy_from_slice(hash);

    match self.chunk_storage.remove_chunk(&chunk_hash) {
      Ok(removed) => RaftResponse {
        success: true,
        message: Some(if removed { "chunk removed".to_string() } else { "chunk not found".to_string() }),
      },
      Err(error) => RaftResponse {
        success: false,
        message: Some(format!("delete_chunk failed: {}", error)),
      },
    }
  }
}

// ---------------------------------------------------------------------------
// RaftSnapshotBuilder
// ---------------------------------------------------------------------------

/// Snapshot builder that captures the current state summary.
pub struct ChunkSnapshotBuilder {
  last_applied_log_id: Option<LogIdOf<TypeConfig>>,
  last_membership: StoredMembershipOf<TypeConfig>,
  snapshot_data: Vec<u8>,
  snapshot_index: u64,
}

impl RaftSnapshotBuilder<TypeConfig> for ChunkSnapshotBuilder {
  async fn build_snapshot(&mut self) -> Result<SnapshotOf<TypeConfig>, io::Error> {
    let snapshot = openraft::storage::Snapshot {
      meta: openraft::storage::SnapshotMeta {
        last_log_id: self.last_applied_log_id,
        last_membership: self.last_membership.clone(),
        snapshot_id: format!("snapshot-{}", self.snapshot_index),
      },
      snapshot: Cursor::new(self.snapshot_data.clone()),
    };
    Ok(snapshot)
  }
}

// ---------------------------------------------------------------------------
// RaftStateMachine
// ---------------------------------------------------------------------------

impl RaftStateMachine<TypeConfig> for ChunkStateMachine {
  type SnapshotBuilder = ChunkSnapshotBuilder;

  async fn applied_state(
    &mut self,
  ) -> Result<(Option<LogIdOf<TypeConfig>>, StoredMembershipOf<TypeConfig>), io::Error> {
    Ok((self.last_applied_log_id, self.last_membership.clone()))
  }

  async fn apply<Strm>(&mut self, entries: Strm) -> Result<(), io::Error>
  where
    Strm: Stream<Item = Result<EntryResponder<TypeConfig>, io::Error>> + Unpin + Send,
  {
    let mut stream = entries;

    while let Some(item) = stream.next().await {
      let (entry, responder) = item?;
      let log_id = entry.log_id();
      self.last_applied_log_id = Some(log_id);

      // Check for membership changes.
      if let Some(membership) = entry.get_membership() {
        self.last_membership = StoredMembership::new(
          Some(entry.log_id()),
          membership,
        );
      }

      // Apply the business logic payload.
      let response = match &entry.payload {
        EntryPayload::Blank => RaftResponse {
          success: true,
          message: None,
        },
        EntryPayload::Normal(request) => self.apply_request(request),
        EntryPayload::Membership(_) => RaftResponse {
          success: true,
          message: None,
        },
      };

      // Send the response if there is a client waiting.
      if let Some(responder) = responder {
        responder.send(response);
      }
    }

    Ok(())
  }

  async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
    ChunkSnapshotBuilder {
      last_applied_log_id: self.last_applied_log_id,
      last_membership: self.last_membership.clone(),
      snapshot_data: self.snapshot_data.clone(),
      snapshot_index: self.snapshot_index,
    }
  }

  async fn begin_receiving_snapshot(&mut self) -> Result<Cursor<Vec<u8>>, io::Error> {
    Ok(Cursor::new(Vec::new()))
  }

  async fn install_snapshot(
    &mut self,
    meta: &SnapshotMetaOf<TypeConfig>,
    snapshot: Cursor<Vec<u8>>,
  ) -> Result<(), io::Error> {
    self.last_applied_log_id = meta.last_log_id;
    self.last_membership = meta.last_membership.clone();
    self.snapshot_data = snapshot.into_inner();
    self.snapshot_index += 1;
    Ok(())
  }

  async fn get_current_snapshot(&mut self) -> Result<Option<SnapshotOf<TypeConfig>>, io::Error> {
    if self.last_applied_log_id.is_none() {
      return Ok(None);
    }

    let snapshot = openraft::storage::Snapshot {
      meta: openraft::storage::SnapshotMeta {
        last_log_id: self.last_applied_log_id,
        last_membership: self.last_membership.clone(),
        snapshot_id: format!("snapshot-{}", self.snapshot_index),
      },
      snapshot: Cursor::new(self.snapshot_data.clone()),
    };
    Ok(Some(snapshot))
  }
}
