use crate::engine::storage_engine::StorageEngine;
use crate::engine::system_tables::SystemTables;

/// Minimum size (in bytes) for a valid JWT signing key (Ed25519 seed).
const SIGNING_KEY_MIN_LENGTH: usize = 32;

/// Check if this node has a valid JWT signing key in system tables.
///
/// The JWT signing key is stored at `::aeordb:config:jwt_signing_key` and must
/// be at least 32 bytes (an Ed25519 seed).
pub fn has_signing_key(engine: &StorageEngine) -> bool {
    let system_tables = SystemTables::new(engine);
    match system_tables.get_config("jwt_signing_key") {
        Ok(Some(key_bytes)) if key_bytes.len() >= SIGNING_KEY_MIN_LENGTH => true,
        _ => false,
    }
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

/// Determine the cluster mode by inspecting system tables.
///
/// Returns `"cluster"` if any peer configurations exist, otherwise
/// `"standalone"`. This is a heuristic based on persisted peer state — if
/// the node was started with `--peers`, those configs will have been stored.
pub fn get_cluster_mode(engine: &StorageEngine) -> String {
    let system_tables = SystemTables::new(engine);
    match system_tables.get_peer_configs() {
        Ok(peers) if !peers.is_empty() => "cluster".to_string(),
        _ => "standalone".to_string(),
    }
}
