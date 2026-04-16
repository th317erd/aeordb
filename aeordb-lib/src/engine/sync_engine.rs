use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::engine::conflict_store::store_conflict;
use crate::engine::merge::three_way_merge;
use crate::engine::peer_connection::{ConnectionState, PeerConnection, PeerManager};
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::sync_apply::apply_merge_operations;
use crate::engine::system_store;
use crate::engine::tree_walker::{diff_trees, walk_version_tree, VersionTree};
use crate::engine::version_manager::VersionManager;
use crate::engine::virtual_clock::PeerClockTracker;

/// Configuration for the sync engine.
pub struct SyncConfig {
    /// How often (in seconds) the periodic fallback sync runs.
    pub periodic_interval_secs: u64,
    /// Shared secret for peer authentication (optional).
    pub cluster_secret: Option<String>,
}

/// Per-peer sync state (persisted in system tables).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PeerSyncState {
    /// Hex-encoded root hash of the last successfully synced state.
    pub last_synced_root_hash: Option<String>,
    /// Milliseconds since epoch of the last successful sync.
    pub last_sync_at: Option<u64>,
}

/// Result of a single sync cycle.
#[derive(Debug)]
pub struct SyncCycleResult {
    /// Whether changes were applied locally.
    pub changes_applied: bool,
    /// Number of conflicts detected during merge.
    pub conflicts_detected: usize,
    /// Number of merge operations applied.
    pub operations_applied: usize,
}

/// The sync engine orchestrates sync cycles between peers.
///
/// It uses a three-way merge strategy:
/// 1. Compute the diff between the common ancestor (last synced root) and local HEAD.
/// 2. Compute the diff between the common ancestor and the remote HEAD.
/// 3. Merge the two diffs, resolving conflicts via LWW.
/// 4. Apply resulting merge operations atomically.
///
/// For this phase, the sync engine works with local engine references
/// (two StorageEngine instances in the same process). Remote HTTP-based
/// sync will be wired up in a later phase.
pub struct SyncEngine {
    engine: Arc<StorageEngine>,
    peer_manager: Arc<PeerManager>,
    clock_tracker: Arc<PeerClockTracker>,
    config: SyncConfig,
    /// Per-peer sync lock to prevent concurrent syncs with the same peer.
    sync_locks: Mutex<HashMap<u64, bool>>,
}

impl SyncEngine {
    pub fn new(
        engine: Arc<StorageEngine>,
        peer_manager: Arc<PeerManager>,
        clock_tracker: Arc<PeerClockTracker>,
        config: SyncConfig,
    ) -> Self {
        SyncEngine {
            engine,
            peer_manager,
            clock_tracker,
            config,
            sync_locks: Mutex::new(HashMap::new()),
        }
    }

    /// Run a single sync cycle with a specific peer.
    ///
    /// Returns `Ok(SyncCycleResult)` describing what happened, or an error
    /// if the sync could not proceed (peer not found, not Active, lock
    /// contention, etc.).
    pub async fn sync_with_peer(&self, peer_node_id: u64) -> Result<SyncCycleResult, String> {
        // Check peer exists and is Active
        let peer = self.peer_manager.get_peer(peer_node_id)
            .ok_or_else(|| format!("Peer {} not found", peer_node_id))?;

        if peer.state != ConnectionState::Active {
            return Err(format!(
                "Peer {} is not Active (state: {:?})",
                peer_node_id, peer.state
            ));
        }

        // Acquire per-peer sync lock (prevent concurrent syncs)
        {
            let mut locks = self.sync_locks.lock().await;
            if *locks.get(&peer_node_id).unwrap_or(&false) {
                return Err(format!(
                    "Sync already in progress with peer {}",
                    peer_node_id
                ));
            }
            locks.insert(peer_node_id, true);
        }

        let result = self.do_sync_cycle_remote(&peer).await;

        // Release lock
        {
            let mut locks = self.sync_locks.lock().await;
            locks.insert(peer_node_id, false);
        }

        result
    }

    /// Perform a local sync cycle between this engine and a remote engine.
    ///
    /// This is the core sync logic that works without HTTP. Both engines
    /// must be accessible in-process. This is the primary method for testing
    /// and will also be used when sync is triggered locally.
    ///
    /// The `peer_node_id` identifies the peer for state tracking.
    /// The `remote_engine` is the other node's StorageEngine.
    pub fn sync_with_local_engine(
        &self,
        peer_node_id: u64,
        remote_engine: &StorageEngine,
    ) -> Result<SyncCycleResult, String> {
        let sync_state = system_store::get_peer_sync_state(&self.engine, peer_node_id)
            .map_err(|e| format!("Failed to load peer sync state: {}", e))?;

        let local_vm = VersionManager::new(&self.engine);
        let remote_vm = VersionManager::new(remote_engine);

        let local_head = local_vm.get_head_hash()
            .map_err(|e| format!("Failed to get local HEAD: {}", e))?;
        let remote_head = remote_vm.get_head_hash()
            .map_err(|e| format!("Failed to get remote HEAD: {}", e))?;

        // If heads are identical, nothing to do
        if local_head == remote_head {
            self.save_sync_state(peer_node_id, &remote_head);
            return Ok(SyncCycleResult {
                changes_applied: false,
                conflicts_detected: 0,
                operations_applied: 0,
            });
        }

        // Determine the base (common ancestor)
        let base_hash = sync_state
            .and_then(|s| s.last_synced_root_hash)
            .and_then(|h| hex::decode(&h).ok())
            .unwrap_or_default();

        // Walk all three trees
        let local_tree = walk_version_tree(&self.engine, &local_head)
            .map_err(|e| format!("Failed to walk local tree: {}", e))?;
        let remote_tree = walk_version_tree(remote_engine, &remote_head)
            .map_err(|e| format!("Failed to walk remote tree: {}", e))?;

        // Transfer any chunks from remote that we don't have locally
        self.transfer_missing_chunks(&remote_tree, remote_engine)?;

        let (local_diff, remote_diff) = if base_hash.is_empty() {
            // No common ancestor: treat empty tree as base
            let empty_tree = VersionTree::new();
            let local_diff = diff_trees(&empty_tree, &local_tree);
            let remote_diff = diff_trees(&empty_tree, &remote_tree);
            (local_diff, remote_diff)
        } else {
            // Walk the base tree from the local engine (it should have it)
            let base_tree = walk_version_tree(&self.engine, &base_hash)
                .map_err(|e| format!("Failed to walk base tree: {}", e))?;
            let local_diff = diff_trees(&base_tree, &local_tree);
            let remote_diff = diff_trees(&base_tree, &remote_tree);
            (local_diff, remote_diff)
        };

        // If neither side has changes from the base, we're in sync
        if local_diff.is_empty() && remote_diff.is_empty() {
            self.save_sync_state(peer_node_id, &remote_head);
            return Ok(SyncCycleResult {
                changes_applied: false,
                conflicts_detected: 0,
                operations_applied: 0,
            });
        }

        // Three-way merge
        let merge_result = three_way_merge(&local_diff, &remote_diff);
        let operations_count = merge_result.operations.len();
        let conflicts_count = merge_result.conflicts.len();

        // Apply merge operations to local engine
        let context = RequestContext::system();
        if !merge_result.operations.is_empty() {
            apply_merge_operations(&self.engine, &context, &merge_result.operations)
                .map_err(|e| format!("Failed to apply merge: {}", e))?;
        }

        // Store conflicts
        for conflict in &merge_result.conflicts {
            let _ = store_conflict(&self.engine, &context, conflict);
        }

        // Get the new local HEAD after merge
        let new_local_head = local_vm.get_head_hash()
            .map_err(|e| format!("Failed to get post-merge HEAD: {}", e))?;

        // Update sync state
        self.save_sync_state(peer_node_id, &new_local_head);

        // Update peer manager
        self.peer_manager.update_sync_state(
            peer_node_id,
            new_local_head,
            chrono::Utc::now().timestamp_millis() as u64,
        );

        Ok(SyncCycleResult {
            changes_applied: operations_count > 0,
            conflicts_detected: conflicts_count,
            operations_applied: operations_count,
        })
    }

    /// Transfer chunks from a remote tree that we don't have locally.
    ///
    /// Iterates all chunks referenced by the remote tree and copies any
    /// that are missing from the local engine.
    fn transfer_missing_chunks(
        &self,
        remote_tree: &VersionTree,
        remote_engine: &StorageEngine,
    ) -> Result<(), String> {
        for chunk_hash in &remote_tree.chunks {
            let has_locally = self.engine.has_entry(chunk_hash)
                .map_err(|e| format!("Failed to check local chunk: {}", e))?;

            if !has_locally {
                let entry = remote_engine.get_entry(chunk_hash)
                    .map_err(|e| format!("Failed to read remote chunk: {}", e))?;

                if let Some((header, _key, value)) = entry {
                    self.engine.store_entry(
                        header.entry_type,
                        chunk_hash,
                        &value,
                    ).map_err(|e| format!("Failed to store chunk locally: {}", e))?;
                }
            }
        }

        // Also transfer file records that we might need
        for (_path, (file_hash, _record)) in &remote_tree.files {
            let has_locally = self.engine.has_entry(file_hash)
                .map_err(|e| format!("Failed to check local file record: {}", e))?;

            if !has_locally {
                let entry = remote_engine.get_entry(file_hash)
                    .map_err(|e| format!("Failed to read remote file record: {}", e))?;

                if let Some((header, _key, value)) = entry {
                    self.engine.store_entry(
                        header.entry_type,
                        file_hash,
                        &value,
                    ).map_err(|e| format!("Failed to store file record locally: {}", e))?;
                }
            }
        }

        Ok(())
    }

    /// Save sync state for a peer, recording the root hash and current time.
    fn save_sync_state(
        &self,
        peer_node_id: u64,
        root_hash: &[u8],
    ) {
        let state = PeerSyncState {
            last_synced_root_hash: Some(hex::encode(root_hash)),
            last_sync_at: Some(chrono::Utc::now().timestamp_millis() as u64),
        };
        let ctx = RequestContext::system();
        let _ = system_store::store_peer_sync_state(&self.engine, &ctx, peer_node_id, &state);
    }

    /// Load sync state for a peer from system store.
    pub fn load_peer_sync_state(&self, peer_node_id: u64) -> Option<PeerSyncState> {
        system_store::get_peer_sync_state(&self.engine, peer_node_id).ok().flatten()
    }

    /// Sync with all active peers.
    ///
    /// Returns a vector of (peer_node_id, result) for each active peer.
    /// Inactive peers are silently skipped.
    pub async fn sync_all_peers(&self) -> Vec<(u64, Result<SyncCycleResult, String>)> {
        let peers = self.peer_manager.all_peers();
        let mut results = Vec::new();

        for peer in peers {
            if peer.state == ConnectionState::Active {
                let result = self.sync_with_peer(peer.node_id).await;
                results.push((peer.node_id, result));
            }
        }

        results
    }

    /// Get a reference to the underlying engine.
    pub fn engine(&self) -> &Arc<StorageEngine> {
        &self.engine
    }

    /// Get a reference to the peer manager.
    pub fn peer_manager(&self) -> &Arc<PeerManager> {
        &self.peer_manager
    }

    /// Get a reference to the clock tracker.
    pub fn clock_tracker(&self) -> &Arc<PeerClockTracker> {
        &self.clock_tracker
    }

    // -------------------------------------------------------------------------
    // Remote sync (HTTP-based) — placeholder for Phase 8
    // -------------------------------------------------------------------------

    /// Perform a sync cycle with a remote peer over HTTP.
    ///
    /// This is a placeholder that returns an error. The actual HTTP
    /// implementation will be wired up in Phase 8 when we have real
    /// multi-node infrastructure.
    async fn do_sync_cycle_remote(
        &self,
        peer: &PeerConnection,
    ) -> Result<SyncCycleResult, String> {
        // TODO(Phase 8): Implement HTTP-based sync cycle.
        // Steps:
        // 1. POST /sync/diff to get remote changes
        // 2. POST /sync/chunks to fetch missing chunks
        // 3. Three-way merge
        // 4. Apply merge operations
        // 5. Update sync state
        Err(format!(
            "Remote HTTP sync not yet implemented for peer {} ({}). \
             Use sync_with_local_engine() for in-process sync.",
            peer.node_id, peer.address
        ))
    }
}
