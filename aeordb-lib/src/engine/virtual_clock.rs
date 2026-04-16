use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

/// Pluggable clock interface for the database.
/// All methods take &self for object safety and thread-safe sharing.
pub trait VirtualClock: Send + Sync {
    /// Current virtual time in milliseconds since Unix epoch.
    fn now_ms(&self) -> u64;
    /// This node's unique identifier.
    fn node_id(&self) -> u64;
}

// ---------------------------------------------------------------------------
// SystemClock — production implementation backed by wall-clock time
// ---------------------------------------------------------------------------

pub struct SystemClock {
    node_id: u64,
}

impl SystemClock {
    pub fn new(node_id: u64) -> Self {
        SystemClock { node_id }
    }
}

impl VirtualClock for SystemClock {
    fn now_ms(&self) -> u64 {
        chrono::Utc::now().timestamp_millis() as u64
    }

    fn node_id(&self) -> u64 {
        self.node_id
    }
}

// ---------------------------------------------------------------------------
// MockClock — deterministic clock for testing
// ---------------------------------------------------------------------------

pub struct MockClock {
    time: AtomicU64,
    node_id: u64,
}

impl MockClock {
    pub fn new(node_id: u64, initial_time: u64) -> Self {
        MockClock {
            time: AtomicU64::new(initial_time),
            node_id,
        }
    }

    /// Set the clock to an absolute time (milliseconds).
    pub fn set_time(&self, time: u64) {
        self.time.store(time, Ordering::SeqCst);
    }

    /// Advance the clock by `ms` milliseconds.
    pub fn advance(&self, ms: u64) {
        self.time.fetch_add(ms, Ordering::SeqCst);
    }
}

impl VirtualClock for MockClock {
    fn now_ms(&self) -> u64 {
        self.time.load(Ordering::SeqCst)
    }

    fn node_id(&self) -> u64 {
        self.node_id
    }
}

// ---------------------------------------------------------------------------
// PeerClockStats — per-peer clock synchronisation statistics
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PeerClockStats {
    /// Exponentially-weighted clock offset in ms (positive = peer ahead).
    pub clock_offset_ms: f64,
    /// Exponentially-weighted one-way wire time estimate in ms.
    pub wire_time_ms: f64,
    /// Exponentially-weighted jitter (variance in wire time) in ms.
    pub jitter_ms: f64,
    /// Number of heartbeat samples recorded for this peer.
    pub samples: u32,
    /// Local receive time (ms) of the most recent heartbeat.
    pub last_updated_ms: u64,
}

// ---------------------------------------------------------------------------
// PeerClockTracker — aggregates clock stats across all known peers
// ---------------------------------------------------------------------------

pub struct PeerClockTracker {
    peers: RwLock<HashMap<u64, PeerClockStats>>,
    bounds_threshold_ms: u64,
}

impl PeerClockTracker {
    /// Create a new tracker.
    /// `bounds_threshold_ms` — heartbeats with a raw offset exceeding this
    /// value (in either direction) are rejected as unreasonable.
    pub fn new(bounds_threshold_ms: u64) -> Self {
        PeerClockTracker {
            peers: RwLock::new(HashMap::new()),
            bounds_threshold_ms,
        }
    }

    /// Record a heartbeat from a peer and update clock stats.
    ///
    /// * `peer_node_id`  — the originating node
    /// * `intent_time`   — the aligned boundary time this heartbeat targeted
    /// * `construct_time` — wall-clock time when the peer built the message
    /// * `receive_time`  — local wall-clock time when the message arrived
    ///
    /// Returns `false` if the heartbeat is rejected (unreasonable clock claim).
    pub fn record_heartbeat(
        &self,
        peer_node_id: u64,
        _intent_time: u64,
        construct_time: u64,
        receive_time: u64,
    ) -> bool {
        // Raw offset: positive means peer clock is ahead of ours.
        let raw_offset = construct_time as f64 - receive_time as f64;

        // Bounds check: reject if offset exceeds threshold.
        if raw_offset.abs() > self.bounds_threshold_ms as f64 {
            return false;
        }

        // One-way wire time estimate (clamped to non-negative).
        let wire_time = (receive_time as f64 - construct_time as f64).max(0.0);

        let mut peers = match self.peers.write() {
            Ok(guard) => guard,
            Err(_) => return false,
        };

        let stats = peers.entry(peer_node_id).or_insert(PeerClockStats {
            clock_offset_ms: 0.0,
            wire_time_ms: 0.0,
            jitter_ms: 0.0,
            samples: 0,
            last_updated_ms: receive_time,
        });

        // Exponential moving average:
        //   alpha = 0.5 for the first 5 samples (fast convergence)
        //   alpha = 0.2 afterwards (stability)
        let alpha = if stats.samples < 5 { 0.5 } else { 0.2 };

        stats.clock_offset_ms = stats.clock_offset_ms * (1.0 - alpha) + raw_offset * alpha;

        let previous_wire_time = stats.wire_time_ms;
        stats.wire_time_ms = stats.wire_time_ms * (1.0 - alpha) + wire_time * alpha;

        // Jitter tracks the variance in wire time.
        let wire_difference = (wire_time - previous_wire_time).abs();
        stats.jitter_ms = stats.jitter_ms * (1.0 - alpha) + wire_difference * alpha;

        stats.samples += 1;
        stats.last_updated_ms = receive_time;

        true
    }

    /// Get the stats for a specific peer, if any.
    pub fn get_peer_stats(&self, peer_node_id: u64) -> Option<PeerClockStats> {
        self.peers.read().ok()?.get(&peer_node_id).cloned()
    }

    /// Check whether we have enough samples from a peer for the clock
    /// relationship to be considered settled.
    pub fn is_settled(&self, peer_node_id: u64, min_samples: u32, max_jitter_ms: f64) -> bool {
        if let Some(stats) = self.get_peer_stats(peer_node_id) {
            stats.samples >= min_samples && stats.jitter_ms < max_jitter_ms
        } else {
            false
        }
    }

    /// Snapshot of all peer stats.
    pub fn all_peer_stats(&self) -> HashMap<u64, PeerClockStats> {
        self.peers.read().map(|guard| guard.clone()).unwrap_or_default()
    }

    /// Seed stats from persisted data (for fast honeymoon after restart).
    pub fn seed_peer(&self, peer_node_id: u64, stats: PeerClockStats) {
        if let Ok(mut peers) = self.peers.write() {
            peers.insert(peer_node_id, stats);
        }
    }
}
