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

/// Per-peer sync health tracking.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SyncStatus {
    pub last_success_at: Option<u64>,
    pub last_attempt_at: Option<u64>,
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
    pub total_syncs: u64,
    pub total_failures: u64,
}

impl SyncStatus {
    pub fn new() -> Self {
        SyncStatus {
            last_success_at: None,
            last_attempt_at: None,
            last_error: None,
            consecutive_failures: 0,
            total_syncs: 0,
            total_failures: 0,
        }
    }

    pub fn record_success(&mut self) {
        let now = chrono::Utc::now().timestamp_millis() as u64;
        self.last_success_at = Some(now);
        self.last_attempt_at = Some(now);
        self.last_error = None;
        self.consecutive_failures = 0;
        self.total_syncs += 1;
    }

    pub fn record_failure(&mut self, error: String) {
        let now = chrono::Utc::now().timestamp_millis() as u64;
        self.last_attempt_at = Some(now);
        self.last_error = Some(error);
        self.consecutive_failures += 1;
        self.total_failures += 1;
        self.total_syncs += 1;
    }

    /// Calculate the next retry interval with exponential backoff and jitter.
    pub fn next_retry_interval_secs(&self, base_secs: u64, max_secs: u64) -> u64 {
        if self.consecutive_failures == 0 {
            return base_secs;
        }
        let exponent = (self.consecutive_failures - 1).min(8) as u32;
        let backoff = base_secs.saturating_mul(2u64.pow(exponent));
        let capped = backoff.min(max_secs);
        // Add +/-10% jitter
        let jitter_range = (capped as f64 * 0.1) as u64;
        if jitter_range > 0 {
            let jitter = rand::random::<u64>() % (jitter_range * 2);
            capped.saturating_sub(jitter_range).saturating_add(jitter)
        } else {
            capped
        }
    }

    /// Check if enough time has elapsed for a retry.
    pub fn should_retry(&self, base_secs: u64, max_secs: u64) -> bool {
        if self.consecutive_failures == 0 {
            return true;
        }
        let interval = self.next_retry_interval_secs(base_secs, max_secs);
        let now = chrono::Utc::now().timestamp_millis() as u64;
        match self.last_attempt_at {
            Some(last) => now >= last + (interval * 1000),
            None => true,
        }
    }
}

impl Default for SyncStatus {
    fn default() -> Self {
        Self::new()
    }
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
    pub sync_status: SyncStatus,
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
            sync_status: SyncStatus::new(),
        };

        match self.connections.write() {
            Ok(mut connections) => {
                connections.insert(config.node_id, connection.clone());
            }
            Err(e) => {
                tracing::warn!("PeerManager lock poisoned in add_peer: {}", e);
            }
        }

        connection
    }

    /// Remove a peer.
    pub fn remove_peer(&self, node_id: u64) -> bool {
        match self.connections.write() {
            Ok(mut connections) => connections.remove(&node_id).is_some(),
            Err(e) => {
                tracing::warn!("PeerManager lock poisoned in remove_peer: {}", e);
                false
            }
        }
    }

    /// Get a snapshot of a specific peer's connection state.
    pub fn get_peer(&self, node_id: u64) -> Option<PeerConnection> {
        match self.connections.read() {
            Ok(connections) => connections.get(&node_id).cloned(),
            Err(e) => {
                tracing::warn!("PeerManager lock poisoned in get_peer: {}", e);
                None
            }
        }
    }

    /// Get all peer connections.
    pub fn all_peers(&self) -> Vec<PeerConnection> {
        match self.connections.read() {
            Ok(connections) => connections.values().cloned().collect(),
            Err(e) => {
                tracing::warn!("PeerManager lock poisoned in all_peers: {}", e);
                Vec::new()
            }
        }
    }

    /// Transition a peer to Honeymoon state.
    pub fn start_honeymoon(&self, node_id: u64, started_at: u64) {
        match self.connections.write() {
            Ok(mut connections) => {
                if let Some(peer) = connections.get_mut(&node_id) {
                    peer.state = ConnectionState::Honeymoon {
                        started_at,
                        heartbeats_received: 0,
                    };
                }
            }
            Err(e) => {
                tracing::warn!("PeerManager lock poisoned in start_honeymoon: {}", e);
            }
        }
    }

    /// Record a heartbeat during honeymoon, incrementing the counter.
    pub fn record_honeymoon_heartbeat(&self, node_id: u64) -> Option<u32> {
        match self.connections.write() {
            Ok(mut connections) => {
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
                None
            }
            Err(e) => {
                tracing::warn!("PeerManager lock poisoned in record_honeymoon_heartbeat: {}", e);
                None
            }
        }
    }

    /// Transition a peer to Active state.
    pub fn activate_peer(&self, node_id: u64) {
        match self.connections.write() {
            Ok(mut connections) => {
                if let Some(peer) = connections.get_mut(&node_id) {
                    peer.state = ConnectionState::Active;
                }
            }
            Err(e) => {
                tracing::warn!("PeerManager lock poisoned in activate_peer: {}", e);
            }
        }
    }

    /// Transition a peer to Disconnected.
    pub fn disconnect_peer(&self, node_id: u64) {
        match self.connections.write() {
            Ok(mut connections) => {
                if let Some(peer) = connections.get_mut(&node_id) {
                    peer.state = ConnectionState::Disconnected;
                }
            }
            Err(e) => {
                tracing::warn!("PeerManager lock poisoned in disconnect_peer: {}", e);
            }
        }
    }

    /// Update clock stats for a peer.
    pub fn update_clock_stats(&self, node_id: u64, stats: PeerClockStats) {
        match self.connections.write() {
            Ok(mut connections) => {
                if let Some(peer) = connections.get_mut(&node_id) {
                    peer.clock_stats = Some(stats);
                }
            }
            Err(e) => {
                tracing::warn!("PeerManager lock poisoned in update_clock_stats: {}", e);
            }
        }
    }

    /// Update sync state for a peer.
    pub fn update_sync_state(&self, node_id: u64, root_hash: Vec<u8>, sync_time: u64) {
        match self.connections.write() {
            Ok(mut connections) => {
                if let Some(peer) = connections.get_mut(&node_id) {
                    peer.last_synced_root_hash = Some(root_hash);
                    peer.last_sync_at = Some(sync_time);
                }
            }
            Err(e) => {
                tracing::warn!("PeerManager lock poisoned in update_sync_state: {}", e);
            }
        }
    }

    /// Record a successful sync for a peer.
    pub fn record_sync_success(&self, node_id: u64) {
        match self.connections.write() {
            Ok(mut connections) => {
                if let Some(peer) = connections.get_mut(&node_id) {
                    peer.sync_status.record_success();
                }
            }
            Err(e) => {
                tracing::warn!("PeerManager lock poisoned in record_sync_success: {}", e);
            }
        }
    }

    /// Record a failed sync for a peer.
    pub fn record_sync_failure(&self, node_id: u64, error: String) {
        match self.connections.write() {
            Ok(mut connections) => {
                if let Some(peer) = connections.get_mut(&node_id) {
                    peer.sync_status.record_failure(error);
                }
            }
            Err(e) => {
                tracing::warn!("PeerManager lock poisoned in record_sync_failure: {}", e);
            }
        }
    }

    /// Get a snapshot of a peer's sync status.
    pub fn get_sync_status(&self, node_id: u64) -> Option<SyncStatus> {
        match self.connections.read() {
            Ok(connections) => connections.get(&node_id).map(|peer| peer.sync_status.clone()),
            Err(e) => {
                tracing::warn!("PeerManager lock poisoned in get_sync_status: {}", e);
                None
            }
        }
    }
}
