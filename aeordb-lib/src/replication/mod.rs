pub mod log_store;
pub mod network;
pub mod raft_node;
pub mod state_machine;
pub mod types;

pub use raft_node::RaftNodeManager;
pub use types::{RaftNode, RaftRequest, RaftResponse};

use std::io::Cursor;

// Type configuration for our Raft instance.
// Wires together all concrete types that openraft needs:
// application data, response, node identity, snapshot format, etc.
openraft::declare_raft_types!(
  pub TypeConfig:
    D = RaftRequest,
    R = RaftResponse,
    NodeId = u64,
    Node = RaftNode,
    SnapshotData = Cursor<Vec<u8>>,
);
