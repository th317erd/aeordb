use std::collections::HashMap;
use std::sync::RwLock;

use crate::engine::virtual_clock::PeerClockStats;

/// Connection lifecycle: Disconnected -> Honeymoon -> Active
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionState {
    Disconnected,
    Honeymoon {
        started_at: u64,
        heartbeats_received: u32,
    },
    Active,
}

/// Runtime state for a peer connection (NOT persisted -- rebuilt on startup).
#[derive(Debug, Clone)]
pub struct PeerConnection {
    pub node_id: u64,
    pub address: String,
    pub label: Option<String>,
    pub state: ConnectionState,
    pub clock_stats: Option<PeerClockStats>,
    pub last_synced_root_hash: Option<Vec<u8>>,
    pub last_sync_at: Option<u64>,
}

/// Persistent peer configuration (stored in system tables).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PeerConfig {
    pub node_id: u64,
    pub address: String,
    pub label: Option<String>,
    pub sync_paths: Option<Vec<String>>,
    pub last_clock_offset_ms: Option<f64>,
    pub last_wire_time_ms: Option<f64>,
    pub last_jitter_ms: Option<f64>,
    pub clock_state_at: Option<u64>,
}

/// Manages all peer connections.
pub struct PeerManager {
    connections: RwLock<HashMap<u64, PeerConnection>>,
}

impl PeerManager {
    pub fn new() -> Self {
        PeerManager {
            connections: RwLock::new(HashMap::new()),
        }
    }

    /// Add or update a peer connection.
    pub fn add_peer(&self, config: &PeerConfig) -> PeerConnection {
        let connection = PeerConnection {
            node_id: config.node_id,
            address: config.address.clone(),
            label: config.label.clone(),
            state: ConnectionState::Disconnected,
            clock_stats: None,
            last_synced_root_hash: None,
            last_sync_at: None,
        };

        if let Ok(mut connections) = self.connections.write() {
            connections.insert(config.node_id, connection.clone());
        }

        connection
    }

    /// Remove a peer.
    pub fn remove_peer(&self, node_id: u64) -> bool {
        self.connections
            .write()
            .map(|mut connections| connections.remove(&node_id).is_some())
            .unwrap_or(false)
    }

    /// Get a snapshot of a specific peer's connection state.
    pub fn get_peer(&self, node_id: u64) -> Option<PeerConnection> {
        self.connections.read().ok()?.get(&node_id).cloned()
    }

    /// Get all peer connections.
    pub fn all_peers(&self) -> Vec<PeerConnection> {
        self.connections
            .read()
            .map(|connections| connections.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Transition a peer to Honeymoon state.
    pub fn start_honeymoon(&self, node_id: u64, started_at: u64) {
        if let Ok(mut connections) = self.connections.write() {
            if let Some(peer) = connections.get_mut(&node_id) {
                peer.state = ConnectionState::Honeymoon {
                    started_at,
                    heartbeats_received: 0,
                };
            }
        }
    }

    /// Record a heartbeat during honeymoon, incrementing the counter.
    pub fn record_honeymoon_heartbeat(&self, node_id: u64) -> Option<u32> {
        if let Ok(mut connections) = self.connections.write() {
            if let Some(peer) = connections.get_mut(&node_id) {
                if let ConnectionState::Honeymoon {
                    heartbeats_received,
                    ..
                } = &mut peer.state
                {
                    *heartbeats_received += 1;
                    return Some(*heartbeats_received);
                }
            }
        }
        None
    }

    /// Transition a peer to Active state.
    pub fn activate_peer(&self, node_id: u64) {
        if let Ok(mut connections) = self.connections.write() {
            if let Some(peer) = connections.get_mut(&node_id) {
                peer.state = ConnectionState::Active;
            }
        }
    }

    /// Transition a peer to Disconnected.
    pub fn disconnect_peer(&self, node_id: u64) {
        if let Ok(mut connections) = self.connections.write() {
            if let Some(peer) = connections.get_mut(&node_id) {
                peer.state = ConnectionState::Disconnected;
            }
        }
    }

    /// Update clock stats for a peer.
    pub fn update_clock_stats(&self, node_id: u64, stats: PeerClockStats) {
        if let Ok(mut connections) = self.connections.write() {
            if let Some(peer) = connections.get_mut(&node_id) {
                peer.clock_stats = Some(stats);
            }
        }
    }

    /// Update sync state for a peer.
    pub fn update_sync_state(&self, node_id: u64, root_hash: Vec<u8>, sync_time: u64) {
        if let Ok(mut connections) = self.connections.write() {
            if let Some(peer) = connections.get_mut(&node_id) {
                peer.last_synced_root_hash = Some(root_hash);
                peer.last_sync_at = Some(sync_time);
            }
        }
    }
}
