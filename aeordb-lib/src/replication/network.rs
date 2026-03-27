use std::future::Future;

use openraft::anyerror::AnyError;
use openraft::errors::{RPCError, ReplicationClosed, StreamingError, Unreachable};
use openraft::network::RPCOption;
use openraft::network::v2::RaftNetworkV2;
use openraft::raft::{
  AppendEntriesRequest, AppendEntriesResponse, SnapshotResponse, VoteRequest, VoteResponse,
};
use openraft::type_config::alias::{SnapshotOf, VoteOf};

use super::TypeConfig;
use super::types::RaftNode;

/// Stub network factory.
///
/// In single-node mode no actual networking is needed. Multi-node
/// transport (HTTP-based) will be implemented when we wire up the axum
/// server for inter-node communication.
pub struct StubNetworkFactory;

impl openraft::network::RaftNetworkFactory<TypeConfig> for StubNetworkFactory {
  type Network = StubNetworkConnection;

  async fn new_client(&mut self, _target: u64, _node: &RaftNode) -> Self::Network {
    StubNetworkConnection
  }
}

/// Stub network connection. All methods return `Unreachable` errors
/// because single-node mode never actually sends RPCs.
pub struct StubNetworkConnection;

impl RaftNetworkV2<TypeConfig> for StubNetworkConnection {
  async fn append_entries(
    &mut self,
    _rpc: AppendEntriesRequest<TypeConfig>,
    _option: RPCOption,
  ) -> Result<AppendEntriesResponse<TypeConfig>, RPCError<TypeConfig>> {
    Err(RPCError::Unreachable(Unreachable::new(&AnyError::error(
      "network not implemented: single-node mode",
    ))))
  }

  async fn vote(
    &mut self,
    _rpc: VoteRequest<TypeConfig>,
    _option: RPCOption,
  ) -> Result<VoteResponse<TypeConfig>, RPCError<TypeConfig>> {
    Err(RPCError::Unreachable(Unreachable::new(&AnyError::error(
      "network not implemented: single-node mode",
    ))))
  }

  async fn full_snapshot(
    &mut self,
    _vote: VoteOf<TypeConfig>,
    _snapshot: SnapshotOf<TypeConfig>,
    _cancel: impl Future<Output = ReplicationClosed> + Send + 'static,
    _option: RPCOption,
  ) -> Result<SnapshotResponse<TypeConfig>, StreamingError<TypeConfig>> {
    Err(StreamingError::Unreachable(Unreachable::new(
      &AnyError::error("network not implemented: single-node mode"),
    )))
  }
}
