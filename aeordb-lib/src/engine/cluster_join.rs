use crate::engine::storage_engine::StorageEngine;
use crate::engine::system_store;

/// Check if this node has a valid JWT signing key in system store.
///
/// The JWT signing key is stored at `/.aeordb-system/config/jwt_signing_key` and must
/// be exactly 32 bytes (an Ed25519 seed).
pub fn has_signing_key(engine: &StorageEngine) -> bool {
  matches!(system_store::get_jwt_signing_key(engine), Ok(Some(_)))
}

/// Check if this node is ready to serve client HTTP traffic.
///
/// In cluster mode, the node must have a valid JWT signing key (received via
/// sync from the cluster) before it can authenticate or issue tokens. Without
/// the signing key, the node cannot verify JWTs and must reject all client
/// requests.
///
/// In standalone mode, this always returns true because the signing key is
/// generated locally during bootstrap and is always available.
pub fn is_ready_for_traffic(engine: &StorageEngine, is_cluster_mode: bool) -> bool {
  if !is_cluster_mode {
    return true;
  }
  has_signing_key(engine)
}

/// Determine the cluster mode by inspecting system store.
///
/// Returns `"cluster"` if any peer configurations exist, otherwise
/// `"standalone"`. This is a heuristic based on persisted peer state — if
/// the node was started with `--peers`, those configs will have been stored.
pub fn get_cluster_mode(engine: &StorageEngine) -> crate::engine::errors::EngineResult<String> {
  let peers = system_store::get_peer_configs(engine)?;
  Ok(if peers.is_empty() { "standalone".to_string() } else { "cluster".to_string() })
}
