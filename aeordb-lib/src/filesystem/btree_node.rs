use crate::storage::{ChunkHash, ChunkStoreError};
use serde::{Deserialize, Serialize};

use super::index_entry::IndexEntry;

pub const BTREE_FORMAT_VERSION: u8 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BTreeNode {
  Branch(BranchNode),
  Leaf(LeafNode),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchNode {
  pub format_version: u8,
  pub keys: Vec<String>,
  pub children: Vec<ChunkHash>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeafNode {
  pub format_version: u8,
  pub entries: Vec<IndexEntry>,
}

impl BTreeNode {
  pub fn serialize(&self) -> Result<Vec<u8>, ChunkStoreError> {
    serde_json::to_vec(self).map_err(|error| {
      ChunkStoreError::SerializationError(format!("failed to serialize BTreeNode: {error}"))
    })
  }

  pub fn deserialize(bytes: &[u8]) -> Result<BTreeNode, ChunkStoreError> {
    serde_json::from_slice(bytes).map_err(|error| {
      ChunkStoreError::SerializationError(format!("failed to deserialize BTreeNode: {error}"))
    })
  }

  pub fn node_type_byte(&self) -> u8 {
    match self {
      BTreeNode::Branch(_) => 0x01,
      BTreeNode::Leaf(_) => 0x02,
    }
  }
}

impl BranchNode {
  pub fn new() -> Self {
    Self {
      format_version: BTREE_FORMAT_VERSION,
      keys: Vec::new(),
      children: Vec::new(),
    }
  }
}

impl Default for BranchNode {
  fn default() -> Self {
    Self::new()
  }
}

impl LeafNode {
  pub fn new() -> Self {
    Self {
      format_version: BTREE_FORMAT_VERSION,
      entries: Vec::new(),
    }
  }
}

impl Default for LeafNode {
  fn default() -> Self {
    Self::new()
  }
}
