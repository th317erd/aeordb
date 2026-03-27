use std::fmt;

use serde::{Deserialize, Serialize};

/// A write request submitted through the Raft consensus layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RaftRequest {
  /// Store a content-addressed chunk by its hash.
  StoreChunk { hash: Vec<u8>, data: Vec<u8> },
  /// Store a serialized hash map under a string key.
  StoreHashMap { key: String, hash_map_data: Vec<u8> },
  /// Delete a content-addressed chunk by its hash.
  DeleteChunk { hash: Vec<u8> },
}

impl fmt::Display for RaftRequest {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      RaftRequest::StoreChunk { hash, data } => {
        write!(formatter, "StoreChunk(hash={} bytes, data={} bytes)", hash.len(), data.len())
      }
      RaftRequest::StoreHashMap { key, hash_map_data } => {
        write!(formatter, "StoreHashMap(key={}, data={} bytes)", key, hash_map_data.len())
      }
      RaftRequest::DeleteChunk { hash } => {
        write!(formatter, "DeleteChunk(hash={} bytes)", hash.len())
      }
    }
  }
}

/// The response returned after a Raft write is applied to the state machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RaftResponse {
  pub success: bool,
  pub message: Option<String>,
}

impl fmt::Display for RaftResponse {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match &self.message {
      Some(message) => write!(formatter, "RaftResponse(success={}, message={})", self.success, message),
      None => write!(formatter, "RaftResponse(success={})", self.success),
    }
  }
}

/// Identifies a node in the Raft cluster.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct RaftNode {
  pub address: String,
}

impl fmt::Display for RaftNode {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "RaftNode({})", self.address)
  }
}
