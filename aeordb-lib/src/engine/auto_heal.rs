//! Cluster auto-healing.
//!
//! When corruption is detected and the node has peers, attempt to
//! recover the corrupt entry from a healthy peer before quarantining.

use crate::engine::entry_header::EntryHeader;
use crate::engine::entry_type::EntryType;
use crate::engine::peer_connection::{ConnectionState, PeerManager};
use crate::engine::storage_engine::StorageEngine;

/// Attempt to heal a corrupt entry by requesting it from cluster peers.
///
/// Returns `true` if the entry was successfully healed, `false` if not.
///
/// The healed data is re-verified after receiving from the peer.
/// We never trust the network -- always verify the hash.
pub fn try_heal_from_peers(
    engine: &StorageEngine,
    peer_manager: &PeerManager,
    hash: &[u8],
) -> bool {
    let peers = peer_manager.all_peers();

    if peers.is_empty() {
        return false;
    }

    let hash_hex = hex::encode(hash);
    let hash_short = &hash_hex[..16.min(hash_hex.len())];

    for peer in &peers {
        if peer.state != ConnectionState::Active {
            continue;
        }

        tracing::info!(
            "Attempting to heal entry {} from peer {} ({})",
            hash_short,
            peer.node_id,
            peer.address,
        );

        match request_chunk_from_peer(&peer.address, hash) {
            Ok(data) => {
                // Re-verify the hash -- NEVER trust the network
                let algo = engine.hash_algo();
                let computed = match EntryHeader::compute_hash(
                    EntryType::Chunk,
                    hash,
                    &data,
                    algo,
                ) {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::warn!(
                            "Failed to compute verification hash for entry {} from peer {}: {}",
                            hash_short,
                            peer.node_id,
                            e,
                        );
                        continue;
                    }
                };

                if computed != hash {
                    tracing::warn!(
                        "Peer {} returned data with mismatched hash for entry {}",
                        peer.node_id,
                        hash_short,
                    );
                    continue;
                }

                // Hash verified -- store locally
                match engine.store_entry(EntryType::Chunk, hash, &data) {
                    Ok(_) => {
                        tracing::info!(
                            "Auto-healed entry {} from peer {}",
                            hash_short,
                            peer.node_id,
                        );
                        return true;
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to store healed entry {}: {}",
                            hash_short,
                            e,
                        );
                    }
                }
            }
            Err(e) => {
                tracing::debug!(
                    "Peer {} could not provide entry {}: {}",
                    peer.node_id,
                    hash_short,
                    e,
                );
            }
        }
    }

    false
}

/// Request a single chunk from a peer via the sync/chunks protocol.
///
/// The sync/chunks endpoint accepts POST with `{"hashes": ["hex1"]}`.
/// For now, this returns Err -- full implementation requires wiring
/// into the async runtime or using a blocking HTTP client.
/// The healing framework is in place; the transport layer can be
/// plugged in later.
fn request_chunk_from_peer(
    peer_address: &str,
    hash: &[u8],
) -> Result<Vec<u8>, String> {
    // The sync/chunks endpoint accepts POST with {"hashes": ["hex1"]}
    // For now, this returns Err -- full implementation requires an HTTP client.
    // The healing framework is in place; the transport layer can be plugged in later.
    let _url = format!("{}/sync/chunks", peer_address);
    let _hash_hex = hex::encode(hash);
    Err(format!(
        "Auto-heal transport not yet configured for peer {}",
        peer_address
    ))
}
