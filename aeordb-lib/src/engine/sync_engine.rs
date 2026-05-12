use std::collections::HashSet;
use std::sync::Arc;

use base64::Engine as _;
use tokio::sync::Mutex;

use crate::engine::compression::{decompress, CompressionAlgorithm};
use crate::engine::conflict_store::store_conflict;
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::engine_event::{EngineEvent, EVENT_SYNCS_COMPLETED, EVENT_SYNCS_FAILED};
use crate::engine::event_bus::EventBus;
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
    // TODO: Use for configurable sync intervals, retry policies, and chunk size limits.
    #[allow(dead_code)]
    config: SyncConfig,
    /// Per-peer sync lock to prevent concurrent syncs with the same peer.
    /// Presence in the set = locked. Absence = unlocked.
    sync_locks: Arc<Mutex<HashSet<u64>>>,
    /// Mints root-level JWTs for peer-to-peer sync HTTP requests. The whole
    /// cluster shares the same signing key (via /sync/join), so a JWT
    /// signed here is accepted by any peer.
    jwt_manager: Option<Arc<crate::auth::JwtManager>>,
}

/// RAII guard that removes a peer ID from the sync lock set on drop.
/// Ensures the lock is released even if the sync panics.
struct SyncLockGuard {
    locks: Arc<Mutex<HashSet<u64>>>,
    peer_id: u64,
}

impl Drop for SyncLockGuard {
    fn drop(&mut self) {
        // Use try_lock to clean up synchronously when possible. The previous
        // fallback that spawned an async task from Drop was unsafe during
        // runtime shutdown — `tokio::spawn` panics when there is no reactor
        // (e.g. a `#[tokio::test]` panic, or shutdown after the runtime
        // begins to wind down). The spawn-fail leaked the peer's slot in
        // `sync_locks` forever; the peer could never sync again until restart.
        //
        // Today we rely on the surrounding code to release the lock before
        // any await (see `sync_with_peer`), so try_lock should always
        // succeed. If it ever fails, we log loudly and accept the leak for
        // this peer instead of risking a panic in Drop. This is recoverable
        // (a restart fixes it); a panic in Drop is not.
        if let Ok(mut locks) = self.locks.try_lock() {
            locks.remove(&self.peer_id);
        } else {
            tracing::error!(
                peer_id = self.peer_id,
                "SyncLockGuard::drop could not acquire sync_locks via try_lock — \
                 the slot will not be cleaned up. This is recoverable on next \
                 server restart but indicates a logic bug: SyncLockGuard was \
                 held across an await on the locks mutex."
            );
        }
    }
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
            sync_locks: Arc::new(Mutex::new(HashSet::new())),
            jwt_manager: None,
        }
    }

    /// Provide the JwtManager so peer-to-peer HTTP requests carry an
    /// Authorization header. Without this, /sync/diff calls receive a
    /// 401 from the peer.
    pub fn with_jwt_manager(mut self, jwt: Arc<crate::auth::JwtManager>) -> Self {
        self.jwt_manager = Some(jwt);
        self
    }

    /// Mint a short-lived root JWT for outbound sync requests.
    ///
    /// The token carries `scope: "sync"`. The auth middleware will reject
    /// this token on any path that isn't `/sync/*`, so a leaked sync token
    /// cannot be used to call file or admin APIs even though it has root
    /// `sub`. (The whole cluster shares the signing key, so without this
    /// scope a leaked token would grant full takeover anywhere.)
    fn mint_sync_token(&self) -> Option<String> {
        let jwt = self.jwt_manager.as_ref()?;
        let claims = crate::auth::TokenClaims {
            sub: crate::engine::ROOT_USER_ID.to_string(),
            iss: "aeordb".to_string(),
            iat: chrono::Utc::now().timestamp(),
            exp: chrono::Utc::now().timestamp() + 300, // 5 minutes
            scope: Some("sync".to_string()),
            permissions: None,
            key_id: None,
        };
        jwt.create_token(&claims).ok()
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

        // Acquire per-peer sync lock (prevent concurrent syncs).
        // The SyncLockGuard ensures the lock is released even on panic.
        let _lock_guard = {
            let mut locks = self.sync_locks.lock().await;
            if locks.contains(&peer_node_id) {
                return Err(format!(
                    "Sync already in progress with peer {}",
                    peer_node_id
                ));
            }
            locks.insert(peer_node_id);
            SyncLockGuard {
                locks: Arc::clone(&self.sync_locks),
                peer_id: peer_node_id,
            }
        };

        self.do_sync_cycle_remote(&peer).await
        // _lock_guard dropped here, removing peer_node_id from sync_locks
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

        // Load peer config for selective sync paths
        let sync_paths = self.get_peer_sync_paths(peer_node_id);

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

        // Apply selective sync path filtering to the remote diff
        let remote_diff = if let Some(ref paths) = sync_paths {
            filter_tree_diff_by_paths(remote_diff, paths)
        } else {
            remote_diff
        };

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
        for (file_hash, _record) in remote_tree.files.values() {
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

        let futures = peers.into_iter().filter_map(|peer| {
            if peer.state == ConnectionState::Active {
                let node_id = peer.node_id;
                Some(async move { (node_id, self.sync_with_peer(node_id).await) })
            } else {
                None
            }
        });

        futures_util::future::join_all(futures).await
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

    /// Trigger a manual sync with all active peers.
    ///
    /// This is the public entry point for on-demand sync (e.g. from an
    /// admin endpoint or CLI command).
    pub async fn trigger_sync_all(&self) -> Vec<(u64, Result<SyncCycleResult, String>)> {
        self.sync_all_peers().await
    }

    // -------------------------------------------------------------------------
    // Remote sync (HTTP-based)
    // -------------------------------------------------------------------------

    /// Perform a sync cycle with a remote peer over HTTP.
    ///
    /// Protocol:
    /// 1. POST `{peer}/sync/diff` with our current HEAD and last-synced hash
    /// 2. Parse the diff response (files added/modified/deleted, symlinks, chunk hashes)
    /// 3. POST `{peer}/sync/chunks` to fetch any chunks we're missing
    /// 4. Reassemble files from chunks and apply changes via DirectoryOps
    /// 5. Persist sync state so the next cycle is incremental
    async fn do_sync_cycle_remote(
        &self,
        peer: &PeerConnection,
    ) -> Result<SyncCycleResult, String> {
        let client = reqwest::Client::new();
        let vm = VersionManager::new(&self.engine);

        let our_head = vm.get_head_hash()
            .map_err(|e| format!("Failed to get HEAD: {}", e))?;

        // Load last synced state for this peer
        let sync_state = system_store::get_peer_sync_state(&self.engine, peer.node_id)
            .map_err(|e| format!("Failed to load peer sync state: {}", e))?;
        let since_hash = sync_state.and_then(|s| s.last_synced_root_hash);

        // Load peer config for selective sync paths
        let sync_paths = self.get_peer_sync_paths(peer.node_id);

        // Step 1: Request diff from peer
        let mut diff_body = serde_json::json!({
            "current_root_hash": hex::encode(&our_head),
        });
        if let Some(ref since) = since_hash {
            diff_body["since_root_hash"] = serde_json::json!(since);
        }
        if let Some(ref paths) = sync_paths {
            diff_body["paths"] = serde_json::json!(paths);
        }

        let sync_token = self.mint_sync_token();
        let mut req = client.post(format!("{}/sync/diff", peer.address));
        if let Some(ref tok) = sync_token {
            req = req.bearer_auth(tok);
        }
        let response = req
            .json(&diff_body)
            .send().await
            .map_err(|e| format!("Failed to contact peer {}: {}", peer.node_id, e))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("Peer {} returned {}: {}", peer.node_id, status, body));
        }

        let diff_resp: serde_json::Value = response.json().await
            .map_err(|e| format!("Failed to parse diff response from peer {}: {}", peer.node_id, e))?;

        let peer_root_hex = diff_resp["root_hash"]
            .as_str()
            .unwrap_or("")
            .to_string();

        // Check if there are any changes to apply
        let changes = &diff_resp["changes"];
        let has_file_changes = ["files_added", "files_modified", "files_deleted"]
            .iter()
            .any(|k| {
                changes[k]
                    .as_array()
                    .is_some_and(|a| !a.is_empty())
            });
        let has_symlink_changes = ["symlinks_added", "symlinks_modified", "symlinks_deleted"]
            .iter()
            .any(|k| {
                changes[k]
                    .as_array()
                    .is_some_and(|a| !a.is_empty())
            });

        if !has_file_changes && !has_symlink_changes {
            // No remote changes — update sync state and return
            let peer_root_bytes = hex::decode(&peer_root_hex).map_err(|e| {
                format!(
                    "Peer {} returned unparseable root hash '{}': {}",
                    peer.node_id, peer_root_hex, e
                )
            })?;
            self.save_sync_state_hex(peer.node_id, &peer_root_hex);
            self.peer_manager.update_sync_state(
                peer.node_id,
                peer_root_bytes,
                chrono::Utc::now().timestamp_millis() as u64,
            );
            return Ok(SyncCycleResult {
                changes_applied: false,
                conflicts_detected: 0,
                operations_applied: 0,
            });
        }

        // Step 2: Fetch needed chunks from the peer
        let chunk_hashes: Vec<String> = diff_resp["chunk_hashes_needed"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        if !chunk_hashes.is_empty() {
            let mut chunks_req = client.post(format!("{}/sync/chunks", peer.address));
            if let Some(ref tok) = sync_token {
                chunks_req = chunks_req.bearer_auth(tok);
            }
            let chunks_resp = chunks_req
                .json(&serde_json::json!({ "hashes": chunk_hashes }))
                .send().await
                .map_err(|e| format!("Failed to fetch chunks from peer {}: {}", peer.node_id, e))?;

            if !chunks_resp.status().is_success() {
                let status = chunks_resp.status();
                let body = chunks_resp.text().await.unwrap_or_default();
                return Err(format!(
                    "Peer {} chunks endpoint returned {}: {}",
                    peer.node_id, status, body
                ));
            }

            let chunks_data: serde_json::Value = chunks_resp.json().await
                .map_err(|e| format!("Failed to parse chunks response from peer {}: {}", peer.node_id, e))?;

            // Store each received chunk in our local engine
            if let Some(chunks) = chunks_data["chunks"].as_array() {
                for chunk in chunks {
                    let hash_hex = chunk["hash"].as_str().unwrap_or("");
                    let data_b64 = chunk["data"].as_str().unwrap_or("");
                    if let (Ok(hash), Ok(data)) = (
                        hex::decode(hash_hex),
                        base64::engine::general_purpose::STANDARD.decode(data_b64),
                    ) {
                        if !self.engine.has_entry(&hash).unwrap_or(false) {
                            let _ = self.engine.store_entry(
                                crate::engine::entry_type::EntryType::Chunk,
                                &hash,
                                &data,
                            );
                        }
                    }
                }
            }
        }

        // Step 3: Apply changes from the diff response via DirectoryOps
        let ctx = RequestContext::system();
        let ops = DirectoryOps::new(&self.engine);
        let mut operations_count: usize = 0;
        let conflicts_count: usize = 0;

        // Process file additions and modifications
        for category in ["files_added", "files_modified"] {
            if let Some(entries) = changes[category].as_array() {
                for entry in entries {
                    let path = entry["path"].as_str().unwrap_or("");
                    if path.is_empty() {
                        continue;
                    }

                    // Reconstruct file data from chunk hashes
                    let entry_chunk_hashes: Vec<Vec<u8>> = entry["chunk_hashes"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|h| h.as_str().and_then(|s| hex::decode(s).ok()))
                                .collect()
                        })
                        .unwrap_or_default();

                    // Read chunks and assemble file content
                    let mut file_data = Vec::new();
                    let mut all_chunks_available = true;
                    for ch in &entry_chunk_hashes {
                        match self.engine.get_entry(ch) {
                            Ok(Some((header, _key, value))) => {
                                let data =
                                    if header.compression_algo != CompressionAlgorithm::None {
                                        decompress(&value, header.compression_algo)
                                            .unwrap_or(value)
                                    } else {
                                        value
                                    };
                                file_data.extend_from_slice(&data);
                            }
                            _ => {
                                all_chunks_available = false;
                                tracing::warn!(
                                    "Missing chunk {} for file {} during sync with peer {}",
                                    hex::encode(ch),
                                    path,
                                    peer.node_id
                                );
                                break;
                            }
                        }
                    }

                    if all_chunks_available {
                        let content_type = entry["content_type"].as_str();
                        let _ = ops.store_file(&ctx, path, &file_data, content_type);
                        operations_count += 1;
                    }
                }
            }
        }

        // Process file deletions
        if let Some(deleted) = changes["files_deleted"].as_array() {
            for entry in deleted {
                let path = entry["path"].as_str().unwrap_or("");
                if !path.is_empty() {
                    let _ = ops.delete_file(&ctx, path);
                    operations_count += 1;
                }
            }
        }

        // Process symlink additions and modifications
        for category in ["symlinks_added", "symlinks_modified"] {
            if let Some(entries) = changes[category].as_array() {
                for entry in entries {
                    let path = entry["path"].as_str().unwrap_or("");
                    let target = entry["target"].as_str().unwrap_or("");
                    if !path.is_empty() && !target.is_empty() {
                        let _ = ops.store_symlink(&ctx, path, target);
                        operations_count += 1;
                    }
                }
            }
        }

        // Process symlink deletions
        if let Some(deleted) = changes["symlinks_deleted"].as_array() {
            for entry in deleted {
                let path = entry["path"].as_str().unwrap_or("");
                if !path.is_empty() {
                    let _ = ops.delete_symlink(&ctx, path);
                    operations_count += 1;
                }
            }
        }

        // Update sync state
        let peer_root_bytes = hex::decode(&peer_root_hex).map_err(|e| {
            format!(
                "Peer {} returned unparseable root hash '{}': {}",
                peer.node_id, peer_root_hex, e
            )
        })?;
        self.save_sync_state_hex(peer.node_id, &peer_root_hex);
        self.peer_manager.update_sync_state(
            peer.node_id,
            peer_root_bytes,
            chrono::Utc::now().timestamp_millis() as u64,
        );

        Ok(SyncCycleResult {
            changes_applied: operations_count > 0,
            conflicts_detected: conflicts_count,
            operations_applied: operations_count,
        })
    }

    /// Save sync state from a hex-encoded root hash string.
    fn save_sync_state_hex(&self, peer_node_id: u64, root_hash_hex: &str) {
        let state = PeerSyncState {
            last_synced_root_hash: Some(root_hash_hex.to_string()),
            last_sync_at: Some(chrono::Utc::now().timestamp_millis() as u64),
        };
        let ctx = RequestContext::system();
        let _ = system_store::store_peer_sync_state(&self.engine, &ctx, peer_node_id, &state);
    }

    /// Load sync_paths from the peer config for selective sync.
    fn get_peer_sync_paths(&self, peer_node_id: u64) -> Option<Vec<String>> {
        let configs = system_store::get_peer_configs(&self.engine).ok()?;
        configs.into_iter()
            .find(|c| c.node_id == peer_node_id)
            .and_then(|c| c.sync_paths)
    }
}

/// Filter a TreeDiff to only include entries matching the given glob patterns.
/// Entries whose paths don't match any pattern are removed from the diff.
fn filter_tree_diff_by_paths(mut diff: crate::engine::tree_walker::TreeDiff, paths: &[String]) -> crate::engine::tree_walker::TreeDiff {
    let matches = |path: &str| -> bool {
        paths.iter().any(|pattern| glob_match::glob_match(pattern, path))
    };

    diff.added.retain(|path, _| matches(path));
    diff.modified.retain(|path, _| matches(path));
    diff.deleted.retain(|path| matches(path));
    diff.symlinks_added.retain(|path, _| matches(path));
    diff.symlinks_modified.retain(|path, _| matches(path));
    diff.symlinks_deleted.retain(|path| matches(path));

    diff
}

// ---------------------------------------------------------------------------
// Background sync loop
// ---------------------------------------------------------------------------

/// Spawn a background sync task that periodically syncs with all active peers.
///
/// The task runs indefinitely, ticking every `interval_secs` seconds. On each
/// tick it iterates active peers, respects exponential backoff for previously
/// failed peers, records sync status, and emits events via the EventBus.
///
/// Missed ticks (e.g. when a sync cycle takes longer than the interval) are
/// skipped rather than queued.
///
/// Accepts a [`CancellationToken`](tokio_util::sync::CancellationToken) for
/// graceful shutdown. When the token is cancelled, the loop exits after the
/// current tick completes.
///
/// Returns a `JoinHandle` that resolves when the task exits.
pub fn spawn_sync_loop(
    sync_engine: Arc<SyncEngine>,
    interval_secs: u64,
    event_bus: Option<Arc<EventBus>>,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let max_backoff_secs: u64 = 300;

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(
            tokio::time::Duration::from_secs(interval_secs),
        );
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("Sync loop shutting down");
                    break;
                }
                _ = interval.tick() => {}
            }

            let peers = sync_engine.peer_manager().all_peers();
            let mut results = Vec::new();

            for peer in peers {
                if peer.state != ConnectionState::Active {
                    continue;
                }

                // Check backoff -- skip if too soon to retry
                if let Some(status) = sync_engine.peer_manager().get_sync_status(peer.node_id) {
                    if !status.should_retry(interval_secs, max_backoff_secs) {
                        continue;
                    }
                }

                let result = sync_engine.sync_with_peer(peer.node_id).await;
                let peer_id_str = peer.node_id.to_string();
                match &result {
                    Ok(r) => {
                        sync_engine.peer_manager().record_sync_success(peer.node_id);
                        metrics::counter!(
                            crate::metrics::definitions::SYNC_CYCLES_TOTAL,
                            "peer" => peer_id_str.clone(),
                            "result" => "success"
                        ).increment(1);
                        metrics::gauge!(
                            crate::metrics::definitions::SYNC_CONSECUTIVE_FAILURES,
                            "peer" => peer_id_str.clone()
                        ).set(0.0);
                        if r.changes_applied {
                            tracing::info!(
                                peer = peer.node_id,
                                operations = r.operations_applied,
                                conflicts = r.conflicts_detected,
                                "Sync with peer completed",
                            );
                        }
                        if let Some(ref bus) = event_bus {
                            let event = EngineEvent::new(
                                EVENT_SYNCS_COMPLETED,
                                "sync",
                                serde_json::json!({
                                    "peer_node_id": peer.node_id,
                                    "operations_applied": r.operations_applied,
                                    "conflicts_detected": r.conflicts_detected,
                                }),
                            );
                            bus.emit(event);
                        }
                    }
                    Err(e) => {
                        sync_engine.peer_manager().record_sync_failure(peer.node_id, e.clone());
                        let status = sync_engine.peer_manager().get_sync_status(peer.node_id);
                        let failures = status.map(|s| s.consecutive_failures).unwrap_or(0);
                        metrics::counter!(
                            crate::metrics::definitions::SYNC_CYCLES_TOTAL,
                            "peer" => peer_id_str.clone(),
                            "result" => "failure"
                        ).increment(1);
                        metrics::gauge!(
                            crate::metrics::definitions::SYNC_CONSECUTIVE_FAILURES,
                            "peer" => peer_id_str.clone()
                        ).set(failures as f64);
                        tracing::warn!(
                            peer = peer.node_id,
                            attempt = failures,
                            error = %e,
                            "Sync with peer failed",
                        );
                        if let Some(ref bus) = event_bus {
                            let event = EngineEvent::new(
                                EVENT_SYNCS_FAILED,
                                "sync",
                                serde_json::json!({
                                    "peer_node_id": peer.node_id,
                                    "error": e,
                                    "consecutive_failures": failures,
                                }),
                            );
                            bus.emit(event);
                        }
                    }
                }
                results.push((peer.node_id, result));
            }
        }
    })
}
