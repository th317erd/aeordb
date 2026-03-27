use std::collections::BTreeMap;
use std::sync::Arc;

use openraft::Config;

use super::TypeConfig;
use super::log_store::InMemoryLogStore;
use super::network::StubNetworkFactory;
use super::state_machine::ChunkStateMachine;
use super::types::{RaftNode, RaftRequest, RaftResponse};
use crate::storage::chunk_storage::ChunkStorage;

/// High-level manager that owns the Raft instance and all supporting stores.
///
/// Provides a simple API for bootstrapping, writing, and querying cluster state.
pub struct RaftNodeManager {
  pub raft: openraft::Raft<TypeConfig, ChunkStateMachine>,
  pub config: Arc<Config>,
  node_id: u64,
}

impl RaftNodeManager {
  /// Create a new Raft node.
  ///
  /// The node starts in an uninitialized state. Call `initialize_single_node`
  /// to bootstrap it as a single-member cluster.
  pub async fn new(
    node_id: u64,
    chunk_storage: Arc<dyn ChunkStorage>,
  ) -> Result<Self, Box<dyn std::error::Error>> {
    let config = Config {
      heartbeat_interval: 500,
      election_timeout_min: 1500,
      election_timeout_max: 3000,
      ..Default::default()
    };
    let config = Arc::new(config.validate().map_err(|error| {
      format!("invalid raft config: {}", error)
    })?);

    let log_store = InMemoryLogStore::new();
    let state_machine = ChunkStateMachine::new(chunk_storage);
    let network = StubNetworkFactory;

    let raft = openraft::Raft::new(
      node_id,
      config.clone(),
      network,
      log_store,
      state_machine,
    )
    .await
    .map_err(|error| format!("failed to create raft instance: {}", error))?;

    Ok(Self { raft, config, node_id })
  }

  /// Bootstrap this node as a single-member cluster.
  ///
  /// After initialization the node will immediately elect itself leader
  /// and be ready to accept writes.
  pub async fn initialize_single_node(&self) -> Result<(), Box<dyn std::error::Error>> {
    let mut members = BTreeMap::new();
    members.insert(
      self.node_id,
      RaftNode {
        address: "127.0.0.1:0".to_string(),
      },
    );

    self
      .raft
      .initialize(members)
      .await
      .map_err(|error| format!("failed to initialize single-node cluster: {}", error))?;

    Ok(())
  }

  /// Submit a write request through the Raft consensus layer.
  ///
  /// The request is appended to the Raft log, replicated (single-node:
  /// immediate), then applied to the state machine. Returns the response
  /// produced by the state machine.
  pub async fn client_write(
    &self,
    request: RaftRequest,
  ) -> Result<RaftResponse, Box<dyn std::error::Error>> {
    let response = self
      .raft
      .client_write(request)
      .await
      .map_err(|error| format!("client_write failed: {}", error))?;

    Ok(response.response().clone())
  }

  /// Check whether this node is currently the Raft leader.
  pub async fn is_leader(&self) -> bool {
    self
      .raft
      .ensure_linearizable(openraft::ReadPolicy::LeaseRead)
      .await
      .is_ok()
  }

  /// Return the node ID of this Raft node.
  pub fn node_id(&self) -> u64 {
    self.node_id
  }
}
