use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use base64::Engine as _;
use tokio::sync::Mutex;

use crate::engine::engine_event::{EngineEvent, EVENT_SYNCS_COMPLETED, EVENT_SYNCS_FAILED};
use crate::engine::event_bus::EventBus;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryOwner};
use crate::engine::merge::three_way_merge;
use crate::engine::operation_memory::OperationMemoryBudget;
use crate::engine::peer_connection::{ConnectionState, PeerConnection, PeerManager};
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::sync_apply::apply_merge_operations_with_conflicts;
use crate::engine::system_store;
use crate::engine::system_family_policy::SystemFamilyPolicyResolver;
use crate::engine::tree_walker::{diff_trees_with_budget, walk_version_tree_for_transfer_with_budget, VersionTree};
use crate::engine::v4::system_family::SystemFamilyTransferOperationV1;
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
  /// Hex-encoded remote root acknowledged by the last successful sync.
  pub last_synced_root_hash: Option<String>,
  /// Hex-encoded local post-merge root used as the local side's next
  /// three-way-merge base. Absent in legacy v0 state.
  #[serde(default)]
  pub last_local_root_hash: Option<String>,
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

const REMOTE_CHUNK_REQUEST_BATCH: usize = 256;
const LOCAL_CHUNK_TRANSFER_BATCH: usize = 256;
const REMOTE_SYNC_DIFF_MAX_BODY_BYTES: u64 = 128 * 1024 * 1024;
const REMOTE_SYNC_CHUNKS_MAX_BODY_BYTES: u64 = 96 * 1024 * 1024;
const REMOTE_SYNC_CHUNKS_MAX_DECODED_BYTES: u64 = 64 * 1024 * 1024;
const REMOTE_SYNC_ERROR_MAX_BODY_BYTES: u64 = 64 * 1024;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteSyncDiffResponse {
  root_hash: String,
  changes: RemoteSyncChanges,
  chunk_hashes_needed: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteSyncChanges {
  files_added: Vec<RemoteSyncFileEntry>,
  files_modified: Vec<RemoteSyncFileEntry>,
  files_deleted: Vec<RemoteSyncDeletedEntry>,
  symlinks_added: Vec<RemoteSyncSymlinkEntry>,
  symlinks_modified: Vec<RemoteSyncSymlinkEntry>,
  symlinks_deleted: Vec<RemoteSyncDeletedEntry>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteSyncFileEntry {
  path: String,
  hash: String,
  #[serde(default)]
  content_hash: Option<String>,
  size: u64,
  content_type: Option<String>,
  #[serde(default)]
  created_at: Option<i64>,
  #[serde(default)]
  updated_at: Option<i64>,
  chunk_hashes: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteSyncSymlinkEntry {
  path: String,
  hash: String,
  target: String,
  #[serde(default)]
  created_at: Option<i64>,
  #[serde(default)]
  updated_at: Option<i64>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteSyncDeletedEntry {
  path: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteChunksResponse {
  chunks: Vec<RemoteChunkEntry>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteChunkEntry {
  hash: String,
  data: String,
  size: u64,
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
  /// Receives coordinator-owned namespace acknowledgement events for both
  /// embedded and HTTP peer sync cycles.
  event_bus: Option<Arc<EventBus>>,
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
  pub fn new(engine: Arc<StorageEngine>, peer_manager: Arc<PeerManager>, clock_tracker: Arc<PeerClockTracker>, config: SyncConfig) -> Self {
    SyncEngine {
      engine,
      peer_manager,
      clock_tracker,
      config,
      sync_locks: Arc::new(Mutex::new(HashSet::new())),
      jwt_manager: None,
      event_bus: None,
    }
  }

  /// Provide the JwtManager so peer-to-peer HTTP requests carry an
  /// Authorization header. Without this, /sync/diff calls receive a
  /// 401 from the peer.
  pub fn with_jwt_manager(mut self, jwt: Arc<crate::auth::JwtManager>) -> Self {
    self.jwt_manager = Some(jwt);
    self
  }

  pub fn with_event_bus(mut self, event_bus: Arc<EventBus>) -> Self {
    self.event_bus = Some(event_bus);
    self
  }

  fn sync_request_context(&self) -> RequestContext {
    self.event_bus.as_ref().map_or_else(RequestContext::system, |event_bus| RequestContext::with_bus(Arc::clone(event_bus)))
  }

  /// Mint a short-lived root JWT for outbound sync requests.
  ///
  /// The token carries `scope: "sync"`. The auth middleware will reject
  /// this token on any path that isn't `/sync/*`, so a leaked sync token
  /// cannot be used to call file or admin APIs even though it has root
  /// `sub`. (The whole cluster shares the signing key, so without this
  /// scope a leaked token would grant full takeover anywhere.)
  fn mint_sync_token(&self) -> Result<Option<String>, String> {
    let Some(jwt) = self.jwt_manager.as_ref() else {
      return Ok(None);
    };
    let claims = crate::auth::TokenClaims {
      sub: crate::engine::ROOT_USER_ID.to_string(),
      iss: "aeordb".to_string(),
      iat: chrono::Utc::now().timestamp(),
      exp: chrono::Utc::now().timestamp() + 300, // 5 minutes
      scope: Some("sync".to_string()),
      permissions: None,
      key_id: None,
    };
    jwt.create_token(&claims).map(Some).map_err(|error| format!("Failed to mint peer sync token: {error}"))
  }

  /// Run a single sync cycle with a specific peer.
  ///
  /// Returns `Ok(SyncCycleResult)` describing what happened, or an error
  /// if the sync could not proceed (peer not found, not Active, lock
  /// contention, etc.).
  pub async fn sync_with_peer(&self, peer_node_id: u64) -> Result<SyncCycleResult, String> {
    // Check peer exists and is Active
    let peer = self.peer_manager.get_peer(peer_node_id).ok_or_else(|| format!("Peer {} not found", peer_node_id))?;

    if peer.state != ConnectionState::Active {
      return Err(format!("Peer {} is not Active (state: {:?})", peer_node_id, peer.state));
    }

    // Acquire per-peer sync lock (prevent concurrent syncs).
    // The SyncLockGuard ensures the lock is released even on panic.
    let _lock_guard = {
      let mut locks = self.sync_locks.lock().await;
      if locks.contains(&peer_node_id) {
        return Err(format!("Sync already in progress with peer {}", peer_node_id));
      }
      locks.insert(peer_node_id);
      SyncLockGuard { locks: Arc::clone(&self.sync_locks), peer_id: peer_node_id }
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
  pub fn sync_with_local_engine(&self, peer_node_id: u64, remote_engine: &StorageEngine) -> Result<SyncCycleResult, String> {
    let sync_state =
      system_store::get_peer_sync_state(&self.engine, peer_node_id).map_err(|e| format!("Failed to load peer sync state: {}", e))?;

    // Load peer config for selective sync paths
    let sync_paths = self.get_peer_sync_paths(peer_node_id)?;

    let local_vm = VersionManager::new(&self.engine);
    let remote_vm = VersionManager::new(remote_engine);

    let local_head = local_vm.get_head_hash().map_err(|e| format!("Failed to get local HEAD: {}", e))?;
    let remote_head = remote_vm.get_head_hash().map_err(|e| format!("Failed to get remote HEAD: {}", e))?;

    let remote_base_hash = sync_state
      .as_ref()
      .and_then(|state| state.last_synced_root_hash.as_deref())
      .map(|encoded| decode_persisted_peer_hash(&self.engine, encoded, "remote checkpoint"))
      .transpose()?;

    // Walk all trees through the same peer-replication policy used by the HTTP
    // producer. Detached portable state participates in each current tree;
    // historical roots deliberately exclude mutable detached state.
    let operation = SystemFamilyTransferOperationV1::PeerReplication;
    let mut local_memory =
      OperationMemoryBudget::new(&self.engine, "local peer sync", MemoryOwner::StreamingRead, AdmissionClass::Workload, 0, None)
        .map_err(|error| format!("Failed to admit local peer sync: {error}"))?;
    let mut remote_memory =
      OperationMemoryBudget::new(remote_engine, "remote peer sync", MemoryOwner::StreamingRead, AdmissionClass::Workload, 0, None)
        .map_err(|error| format!("Failed to admit remote peer sync: {error}"))?;
    let remote_tree = walk_version_tree_for_transfer_with_budget(remote_engine, &remote_head, operation, true, &mut remote_memory)
      .map_err(|error| format!("Failed to walk remote tree: {error}"))?;
    let remote_diff = if let Some(base_hash) = remote_base_hash.as_deref() {
      match remote_engine
        .get_entry_verified(base_hash)
        .map_err(|error| format!("Failed to inspect persisted remote checkpoint for peer {peer_node_id}: {error}"))?
      {
        Some((header, stored_key, _)) if stored_key == base_hash && header.entry_type == crate::engine::EntryType::DirectoryIndex => {
          let base_tree = walk_version_tree_for_transfer_with_budget(remote_engine, base_hash, operation, false, &mut remote_memory)
            .map_err(|error| format!("Failed to walk persisted remote checkpoint for peer {peer_node_id}: {error}"))?;
          diff_trees_with_budget(&base_tree, &remote_tree, &mut remote_memory)
            .map_err(|error| format!("Failed to diff remote tree for peer {peer_node_id}: {error}"))?
        }
        Some((header, _, _)) => {
          return Err(format!(
            "Persisted remote checkpoint for peer {peer_node_id} resolves to {:?}, expected DirectoryIndex",
            header.entry_type
          ));
        }
        None if sync_state.as_ref().is_some_and(|state| state.last_local_root_hash.is_none()) => {
          // Legacy v0 stored one ambiguous root. If it was the post-merge local
          // root, the peer cannot resolve it. A one-time full remote diff is
          // conservative: it may repeat idempotent additions but cannot invent
          // remote deletions for local-only paths.
          diff_trees_with_budget(&VersionTree::new(), &remote_tree, &mut remote_memory)
            .map_err(|error| format!("Failed to diff legacy remote tree for peer {peer_node_id}: {error}"))?
        }
        None => return Err(format!("Persisted remote checkpoint for peer {peer_node_id} is missing from the remote engine")),
      }
    } else {
      diff_trees_with_budget(&VersionTree::new(), &remote_tree, &mut remote_memory)
        .map_err(|error| format!("Failed to diff initial remote tree for peer {peer_node_id}: {error}"))?
    };
    let local_diff = self.compute_local_diff_for_remote_sync(
      &local_head,
      sync_state.as_ref(),
      remote_base_hash.as_deref(),
      sync_paths.as_deref(),
      &mut local_memory,
    )?;
    let remote_diff = if let Some(ref paths) = sync_paths { filter_tree_diff_by_paths(remote_diff, paths) } else { remote_diff };

    // If neither side has changes from the base, we're in sync
    if local_diff.is_empty() && remote_diff.is_empty() {
      self.save_sync_state_hex(peer_node_id, &remote_head, &local_head)?;
      return Ok(SyncCycleResult { changes_applied: false, conflicts_detected: 0, operations_applied: 0 });
    }

    // Three-way merge
    let merge_result = three_way_merge(&local_diff, &remote_diff);
    let operations_count = merge_result.operations.len();
    let conflicts_count = merge_result.conflicts.len();

    // Transfer every changed remote file chunk selected by registry policy
    // and selective paths. Conflict losers need the same immutable closure as
    // merge winners. File records are reconstructed by the shared merge path;
    // copying their old physical entries would add unreferenced KV state.
    self.transfer_missing_remote_diff_chunks(&remote_diff, remote_engine)?;

    // Publish selected operations and required local conflict evidence through
    // one hard namespace receipt.
    let context = self.sync_request_context();
    if !merge_result.operations.is_empty() || !merge_result.conflicts.is_empty() {
      apply_merge_operations_with_conflicts(&self.engine, &context, &merge_result.operations, &merge_result.conflicts, &remote_diff)
        .map_err(|e| format!("Failed to apply merge and conflict evidence: {}", e))?;
    }

    // Get the new local HEAD after merge
    let new_local_head = local_vm.get_head_hash().map_err(|e| format!("Failed to get post-merge HEAD: {}", e))?;

    // Update sync state
    self.save_sync_state_hex(peer_node_id, &remote_head, &new_local_head)?;

    // Update peer manager
    self.peer_manager.update_sync_state(peer_node_id, new_local_head, chrono::Utc::now().timestamp_millis() as u64);

    Ok(SyncCycleResult { changes_applied: operations_count > 0, conflicts_detected: conflicts_count, operations_applied: operations_count })
  }

  fn transfer_missing_remote_diff_chunks(
    &self,
    remote_diff: &crate::engine::tree_walker::TreeDiff,
    remote_engine: &StorageEngine,
  ) -> Result<(), String> {
    let mut seen = HashSet::new();
    let mut chunks = Vec::with_capacity(LOCAL_CHUNK_TRANSFER_BATCH);
    for (path, (_, file_record)) in remote_diff.added.iter().chain(remote_diff.modified.iter()) {
      for chunk_hash in &file_record.chunk_hashes {
        if !seen.insert(chunk_hash.clone()) {
          continue;
        }
        let exists = crate::engine::directory_ops::validate_existing_chunk_locator(&self.engine, "local peer sync", chunk_hash)
          .map_err(|error| format!("Failed to validate local chunk {}: {error}", hex::encode(chunk_hash)))?;
        if exists {
          continue;
        }
        let chunk_data = remote_engine
          .read_chunk(chunk_hash)
          .map_err(|error| format!("Failed to read remote chunk {} for '{}': {error}", hex::encode(chunk_hash), path))?
          .ok_or_else(|| format!("Remote file '{}' references missing chunk {}", path, hex::encode(chunk_hash)))?;
        chunks.push(crate::engine::sync_api::ChunkData { hash: chunk_hash.clone(), data: chunk_data });
        if chunks.len() == LOCAL_CHUNK_TRANSFER_BATCH {
          crate::engine::sync_api::apply_sync_chunks(&self.engine, &chunks)
            .map_err(|error| format!("Failed to store validated local peer chunks: {error}"))?;
          chunks.clear();
        }
      }
    }
    if !chunks.is_empty() {
      crate::engine::sync_api::apply_sync_chunks(&self.engine, &chunks)
        .map_err(|error| format!("Failed to store validated local peer chunks: {error}"))?;
    }
    Ok(())
  }

  /// Load sync state for a peer from system store.
  pub fn load_peer_sync_state(&self, peer_node_id: u64) -> crate::engine::errors::EngineResult<Option<PeerSyncState>> {
    system_store::get_peer_sync_state(&self.engine, peer_node_id)
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
  async fn do_sync_cycle_remote(&self, peer: &PeerConnection) -> Result<SyncCycleResult, String> {
    let client = reqwest::Client::new();
    let vm = VersionManager::new(&self.engine);

    let our_head = vm.get_head_hash().map_err(|e| format!("Failed to get HEAD: {}", e))?;

    // Load last synced state for this peer
    let sync_state =
      system_store::get_peer_sync_state(&self.engine, peer.node_id).map_err(|e| format!("Failed to load peer sync state: {}", e))?;
    let remote_base_hash = sync_state
      .as_ref()
      .and_then(|state| state.last_synced_root_hash.as_deref())
      .map(|encoded| decode_persisted_peer_hash(&self.engine, encoded, "remote checkpoint"))
      .transpose()?;
    let since_hash = remote_base_hash.as_ref().map(hex::encode);

    // Load peer config for selective sync paths
    let sync_paths = self.get_peer_sync_paths(peer.node_id)?;

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

    let sync_token = self.mint_sync_token()?;
    let mut req = client.post(format!("{}/sync/diff", peer.address));
    if let Some(ref tok) = sync_token {
      req = req.bearer_auth(tok);
    }
    let response = req.json(&diff_body).send().await.map_err(|e| format!("Failed to contact peer {}: {}", peer.node_id, e))?;

    if !response.status().is_success() {
      let status = response.status();
      let BoundedRemoteText { value: body, _memory: _error_response_memory } =
        read_bounded_remote_text(&self.engine, response, REMOTE_SYNC_ERROR_MAX_BODY_BYTES, "remote sync error response")
          .await
          .map_err(|error| format!("Peer {} returned {status}, but its error response could not be read: {error}", peer.node_id))?;
      return Err(format!("Peer {} returned {}: {}", peer.node_id, status, body));
    }

    let BoundedRemoteJson { value: diff_resp, _memory: _diff_response_memory } = read_bounded_remote_json::<RemoteSyncDiffResponse>(
      &self.engine,
      response,
      REMOTE_SYNC_DIFF_MAX_BODY_BYTES,
      "remote sync diff response",
    )
    .await
    .map_err(|e| format!("Failed to parse diff response from peer {}: {}", peer.node_id, e))?;
    let validated = validate_remote_sync_diff(&self.engine, diff_resp)
      .map_err(|error| format!("Peer {} returned invalid sync state: {error}", peer.node_id))?;

    self.fetch_and_store_remote_chunks(&client, peer, sync_token.as_deref(), &validated.required_chunks).await?;
    let (peer_root_bytes, remote_diff) =
      validated.into_tree_diff(&self.engine).map_err(|error| format!("Peer {} returned unusable sync state: {error}", peer.node_id))?;
    let mut local_memory =
      OperationMemoryBudget::new(&self.engine, "remote peer local diff", MemoryOwner::StreamingRead, AdmissionClass::Workload, 0, None)
        .map_err(|error| format!("Failed to admit local side of peer {} sync: {error}", peer.node_id))?;
    let local_diff = self.compute_local_diff_for_remote_sync(
      &our_head,
      sync_state.as_ref(),
      remote_base_hash.as_deref(),
      sync_paths.as_deref(),
      &mut local_memory,
    )?;
    let merge_result = three_way_merge(&local_diff, &remote_diff);
    let operations_count = merge_result.operations.len();
    let conflicts_count = merge_result.conflicts.len();
    if !merge_result.operations.is_empty() || !merge_result.conflicts.is_empty() {
      apply_merge_operations_with_conflicts(
        &self.engine,
        &self.sync_request_context(),
        &merge_result.operations,
        &merge_result.conflicts,
        &remote_diff,
      )
      .map_err(|error| format!("Failed to atomically apply sync from peer {}: {error}", peer.node_id))?;
    }
    let new_local_head = vm.get_head_hash().map_err(|error| format!("Failed to get post-sync HEAD: {error}"))?;

    // The peer checkpoint is the final acknowledgement. It is never advanced
    // until the complete remote shape, chunk closure, and namespace receipt
    // have succeeded.
    self.save_sync_state_hex(peer.node_id, &peer_root_bytes, &new_local_head)?;
    self.peer_manager.update_sync_state(peer.node_id, new_local_head, chrono::Utc::now().timestamp_millis() as u64);

    Ok(SyncCycleResult { changes_applied: operations_count > 0, conflicts_detected: conflicts_count, operations_applied: operations_count })
  }

  fn compute_local_diff_for_remote_sync(
    &self,
    local_head: &[u8],
    sync_state: Option<&PeerSyncState>,
    legacy_remote_base: Option<&[u8]>,
    sync_paths: Option<&[String]>,
    memory: &mut OperationMemoryBudget,
  ) -> Result<crate::engine::tree_walker::TreeDiff, String> {
    let operation = SystemFamilyTransferOperationV1::PeerReplication;
    let local_tree = walk_version_tree_for_transfer_with_budget(&self.engine, local_head, operation, true, memory)
      .map_err(|error| format!("Failed to walk local tree for remote sync: {error}"))?;
    let mut local_diff = if let Some(encoded) = sync_state.and_then(|state| state.last_local_root_hash.as_deref()) {
      let local_base = decode_persisted_peer_hash(&self.engine, encoded, "local merge base")?;
      let base_tree = walk_version_tree_for_transfer_with_budget(&self.engine, &local_base, operation, false, memory)
        .map_err(|error| format!("Failed to walk persisted local merge base: {error}"))?;
      diff_trees_with_budget(&base_tree, &local_tree, memory).map_err(|error| format!("Failed to diff local merge state: {error}"))?
    } else if sync_state.is_none() {
      diff_trees_with_budget(&VersionTree::new(), &local_tree, memory)
        .map_err(|error| format!("Failed to diff initial local merge state: {error}"))?
    } else if let Some(candidate) = legacy_remote_base {
      match self.engine.get_entry_verified(candidate).map_err(|error| format!("Failed to inspect legacy peer merge base: {error}"))? {
        Some((header, stored_key, _)) if stored_key == candidate && header.entry_type == crate::engine::EntryType::DirectoryIndex => {
          let base_tree = walk_version_tree_for_transfer_with_budget(&self.engine, candidate, operation, false, memory)
            .map_err(|error| format!("Failed to walk legacy peer merge base: {error}"))?;
          diff_trees_with_budget(&base_tree, &local_tree, memory)
            .map_err(|error| format!("Failed to diff legacy local merge state: {error}"))?
        }
        Some((header, _, _)) => {
          return Err(format!("Legacy peer merge base resolves to {:?}, expected DirectoryIndex", header.entry_type));
        }
        None => crate::engine::tree_walker::TreeDiff {
          added: std::collections::HashMap::new(),
          modified: std::collections::HashMap::new(),
          deleted: Vec::new(),
          new_chunks: HashSet::new(),
          changed_directories: std::collections::HashMap::new(),
          symlinks_added: std::collections::HashMap::new(),
          symlinks_modified: std::collections::HashMap::new(),
          symlinks_deleted: Vec::new(),
        },
      }
    } else {
      crate::engine::tree_walker::TreeDiff {
        added: std::collections::HashMap::new(),
        modified: std::collections::HashMap::new(),
        deleted: Vec::new(),
        new_chunks: HashSet::new(),
        changed_directories: std::collections::HashMap::new(),
        symlinks_added: std::collections::HashMap::new(),
        symlinks_modified: std::collections::HashMap::new(),
        symlinks_deleted: Vec::new(),
      }
    };
    if let Some(paths) = sync_paths {
      local_diff = filter_tree_diff_by_paths(local_diff, paths);
    }
    Ok(local_diff)
  }

  async fn fetch_and_store_remote_chunks(
    &self,
    client: &reqwest::Client,
    peer: &PeerConnection,
    sync_token: Option<&str>,
    required_chunks: &[Vec<u8>],
  ) -> Result<(), String> {
    let mut missing = Vec::new();
    for hash in required_chunks {
      let exists = crate::engine::directory_ops::validate_existing_chunk_locator(&self.engine, "remote sync", hash)
        .map_err(|error| format!("Failed to validate local chunk {}: {error}", hex::encode(hash)))?;
      if !exists {
        missing.push(hash.clone());
      }
    }

    for requested in missing.chunks(REMOTE_CHUNK_REQUEST_BATCH) {
      let requested_hex = requested.iter().map(hex::encode).collect::<Vec<_>>();
      let mut request = client.post(format!("{}/sync/chunks", peer.address));
      if let Some(token) = sync_token {
        request = request.bearer_auth(token);
      }
      let response = request
        .json(&serde_json::json!({ "hashes": requested_hex }))
        .send()
        .await
        .map_err(|error| format!("Failed to fetch chunks from peer {}: {error}", peer.node_id))?;
      if !response.status().is_success() {
        let status = response.status();
        let BoundedRemoteText { value: body, _memory: _error_response_memory } =
          read_bounded_remote_text(&self.engine, response, REMOTE_SYNC_ERROR_MAX_BODY_BYTES, "remote sync chunks error response")
            .await
            .map_err(|error| {
              format!("Peer {} chunks endpoint returned {status}, but its error response could not be read: {error}", peer.node_id)
            })?;
        return Err(format!("Peer {} chunks endpoint returned {status}: {body}", peer.node_id));
      }
      let BoundedRemoteJson { value: response, _memory: _chunk_response_memory } = read_bounded_remote_json::<RemoteChunksResponse>(
        &self.engine,
        response,
        REMOTE_SYNC_CHUNKS_MAX_BODY_BYTES,
        "remote sync chunks response",
      )
      .await
      .map_err(|error| format!("Failed to parse chunks response from peer {}: {error}", peer.node_id))?;
      let chunks = validate_remote_chunk_response(&self.engine, requested, response)
        .map_err(|error| format!("Peer {} returned invalid chunk state: {error}", peer.node_id))?;
      crate::engine::sync_api::apply_sync_chunks(&self.engine, &chunks)
        .map_err(|error| format!("Failed to store validated chunks from peer {}: {error}", peer.node_id))?;
    }
    Ok(())
  }

  /// Save sync state from a hex-encoded root hash string.
  fn save_sync_state_hex(&self, peer_node_id: u64, remote_root_hash: &[u8], local_root_hash: &[u8]) -> Result<(), String> {
    let state = PeerSyncState {
      last_synced_root_hash: Some(hex::encode(remote_root_hash)),
      last_local_root_hash: Some(hex::encode(local_root_hash)),
      last_sync_at: Some(chrono::Utc::now().timestamp_millis() as u64),
    };
    let ctx = RequestContext::system();
    system_store::store_peer_sync_state(&self.engine, &ctx, peer_node_id, &state)
      .map_err(|e| format!("Failed to store sync state for peer {}: {}", peer_node_id, e))
  }

  /// Load sync_paths from the peer config for selective sync.
  fn get_peer_sync_paths(&self, peer_node_id: u64) -> Result<Option<Vec<String>>, String> {
    let configs = system_store::get_peer_configs(&self.engine)
      .map_err(|error| format!("Failed to load configuration for peer {peer_node_id}: {error}"))?;
    Ok(configs.into_iter().find(|config| config.node_id == peer_node_id).and_then(|config| config.sync_paths))
  }
}

struct ValidatedRemoteSyncDiff {
  root_hash: Vec<u8>,
  files: Vec<ValidatedRemoteFile>,
  file_deletions: Vec<String>,
  symlinks: Vec<ValidatedRemoteSymlink>,
  symlink_deletions: Vec<String>,
  required_chunks: Vec<Vec<u8>>,
}

struct ValidatedRemoteFile {
  path: String,
  hash: Vec<u8>,
  content_hash: Option<Vec<u8>>,
  size: u64,
  content_type: Option<String>,
  created_at: Option<i64>,
  updated_at: Option<i64>,
  chunk_hashes: Vec<Vec<u8>>,
  modified: bool,
}

struct ValidatedRemoteSymlink {
  path: String,
  hash: Vec<u8>,
  target: String,
  created_at: Option<i64>,
  updated_at: Option<i64>,
  modified: bool,
}

impl ValidatedRemoteSyncDiff {
  fn into_tree_diff(self, engine: &StorageEngine) -> crate::engine::errors::EngineResult<(Vec<u8>, crate::engine::tree_walker::TreeDiff)> {
    let mut added = std::collections::HashMap::new();
    let mut modified = std::collections::HashMap::new();
    let mut new_chunks = HashSet::new();
    for file in self.files {
      let mut record = crate::engine::file_record::FileRecord::new(file.path.clone(), file.content_type, file.size, file.chunk_hashes);
      if let Some(created_at) = file.created_at {
        record.created_at = created_at;
      }
      if let Some(updated_at) = file.updated_at {
        record.updated_at = updated_at;
      }
      record.content_hash = match file.content_hash {
        Some(content_hash) => content_hash,
        None => crate::engine::directory_ops::whole_file_content_hash_from_chunks(engine, &record.chunk_hashes)?,
      };
      new_chunks.extend(record.chunk_hashes.iter().cloned());
      if file.modified {
        modified.insert(file.path, (file.hash, record));
      } else {
        added.insert(file.path, (file.hash, record));
      }
    }
    let mut symlinks_added = std::collections::HashMap::new();
    let mut symlinks_modified = std::collections::HashMap::new();
    for symlink in self.symlinks {
      let mut record = crate::engine::symlink_record::SymlinkRecord::new(symlink.path.clone(), symlink.target);
      if let Some(created_at) = symlink.created_at {
        record.created_at = created_at;
      }
      if let Some(updated_at) = symlink.updated_at {
        record.updated_at = updated_at;
      }
      if symlink.modified {
        symlinks_modified.insert(symlink.path, (symlink.hash, record));
      } else {
        symlinks_added.insert(symlink.path, (symlink.hash, record));
      }
    }
    Ok((
      self.root_hash,
      crate::engine::tree_walker::TreeDiff {
        added,
        modified,
        deleted: self.file_deletions,
        new_chunks,
        changed_directories: std::collections::HashMap::new(),
        symlinks_added,
        symlinks_modified,
        symlinks_deleted: self.symlink_deletions,
      },
    ))
  }
}

fn validate_remote_sync_diff(
  engine: &StorageEngine,
  response: RemoteSyncDiffResponse,
) -> Result<ValidatedRemoteSyncDiff, crate::engine::errors::EngineError> {
  const MAX_REMOTE_OPERATIONS: usize = 100_000;
  const MAX_REMOTE_CHUNK_REFERENCES: usize = 1_000_000;

  let hash_length = engine.hash_algo().hash_length();
  let root_hash = decode_remote_hash(&response.root_hash, hash_length, "root_hash")?;
  let RemoteSyncChanges { files_added, files_modified, files_deleted, symlinks_added, symlinks_modified, symlinks_deleted } =
    response.changes;
  let operation_count = files_added
    .len()
    .checked_add(files_modified.len())
    .and_then(|count| count.checked_add(files_deleted.len()))
    .and_then(|count| count.checked_add(symlinks_added.len()))
    .and_then(|count| count.checked_add(symlinks_modified.len()))
    .and_then(|count| count.checked_add(symlinks_deleted.len()))
    .ok_or_else(|| crate::engine::errors::EngineError::ResourceExhausted("remote sync operation count overflow".to_string()))?;
  if operation_count > MAX_REMOTE_OPERATIONS {
    return Err(crate::engine::errors::EngineError::ResourceExhausted(format!(
      "remote sync contains {operation_count} operations, maximum is {MAX_REMOTE_OPERATIONS}"
    )));
  }
  let mut file_entries = Vec::with_capacity(files_added.len().saturating_add(files_modified.len()));
  file_entries.extend(files_added.into_iter().map(|entry| (entry, false)));
  file_entries.extend(files_modified.into_iter().map(|entry| (entry, true)));
  let mut symlink_entries = Vec::with_capacity(symlinks_added.len().saturating_add(symlinks_modified.len()));
  symlink_entries.extend(symlinks_added.into_iter().map(|entry| (entry, false)));
  symlink_entries.extend(symlinks_modified.into_iter().map(|entry| (entry, true)));

  let resolver = SystemFamilyPolicyResolver::new(engine.hash_algo())?;
  let mut seen_paths = HashSet::with_capacity(operation_count);
  let mut required_chunks = BTreeSet::new();
  let mut chunk_references = 0usize;
  let mut files = Vec::with_capacity(file_entries.len());
  for (entry, modified) in file_entries {
    validate_remote_sync_path(&resolver, &mut seen_paths, &entry.path)?;
    let hash = decode_remote_hash(&entry.hash, hash_length, "file hash")?;
    let content_hash =
      entry.content_hash.as_deref().map(|encoded| decode_remote_hash(encoded, hash_length, "file content_hash")).transpose()?;
    if entry.content_type.as_ref().is_some_and(|content_type| content_type.len() > u16::MAX as usize) {
      return Err(crate::engine::errors::EngineError::InvalidInput(format!("remote sync content type is too long for '{}'", entry.path)));
    }
    chunk_references = chunk_references
      .checked_add(entry.chunk_hashes.len())
      .ok_or_else(|| crate::engine::errors::EngineError::ResourceExhausted("remote sync chunk reference count overflow".to_string()))?;
    if chunk_references > MAX_REMOTE_CHUNK_REFERENCES {
      return Err(crate::engine::errors::EngineError::ResourceExhausted(format!(
        "remote sync contains more than {MAX_REMOTE_CHUNK_REFERENCES} chunk references"
      )));
    }
    let mut chunk_hashes = Vec::with_capacity(entry.chunk_hashes.len());
    for encoded in entry.chunk_hashes {
      let hash = decode_remote_hash(&encoded, hash_length, "file chunk hash")?;
      required_chunks.insert(hash.clone());
      chunk_hashes.push(hash);
    }
    let expected_hash =
      crate::engine::directory_ops::file_identity_hash(&entry.path, entry.content_type.as_deref(), &chunk_hashes, &engine.hash_algo())?;
    if hash != expected_hash {
      return Err(crate::engine::errors::EngineError::CorruptEntry {
        offset: 0,
        reason: format!("remote sync file '{}' claimed hash does not match its identity", entry.path),
      });
    }
    files.push(ValidatedRemoteFile {
      path: entry.path,
      hash,
      content_hash,
      size: entry.size,
      content_type: entry.content_type,
      created_at: entry.created_at,
      updated_at: entry.updated_at,
      chunk_hashes,
      modified,
    });
  }

  let mut file_deletions = Vec::with_capacity(files_deleted.len());
  for entry in files_deleted {
    validate_remote_sync_path(&resolver, &mut seen_paths, &entry.path)?;
    file_deletions.push(entry.path);
  }

  let mut symlinks = Vec::with_capacity(symlink_entries.len());
  for (entry, modified) in symlink_entries {
    validate_remote_sync_path(&resolver, &mut seen_paths, &entry.path)?;
    let hash = decode_remote_hash(&entry.hash, hash_length, "symlink hash")?;
    if entry.target.is_empty()
      || entry.target.len() > u16::MAX as usize
      || entry.target.bytes().any(|byte| byte < 0x20 || byte == 0x7F)
      || crate::engine::path_utils::normalize_path(&entry.target) != entry.target
    {
      return Err(crate::engine::errors::EngineError::InvalidInput(format!("remote sync symlink '{}' has an invalid target", entry.path)));
    }
    let expected_hash = crate::engine::directory_ops::symlink_identity_hash(&entry.path, &entry.target, &engine.hash_algo())?;
    if hash != expected_hash {
      return Err(crate::engine::errors::EngineError::CorruptEntry {
        offset: 0,
        reason: format!("remote sync symlink '{}' claimed hash does not match its identity", entry.path),
      });
    }
    symlinks.push(ValidatedRemoteSymlink {
      path: entry.path,
      hash,
      target: entry.target,
      created_at: entry.created_at,
      updated_at: entry.updated_at,
      modified,
    });
  }

  let mut symlink_deletions = Vec::with_capacity(symlinks_deleted.len());
  for entry in symlinks_deleted {
    validate_remote_sync_path(&resolver, &mut seen_paths, &entry.path)?;
    symlink_deletions.push(entry.path);
  }

  let mut advertised_chunks = BTreeSet::new();
  for encoded in response.chunk_hashes_needed {
    let hash = decode_remote_hash(&encoded, hash_length, "chunk_hashes_needed entry")?;
    if !advertised_chunks.insert(hash) {
      return Err(crate::engine::errors::EngineError::InvalidInput("remote sync chunk manifest contains a duplicate hash".to_string()));
    }
  }
  if advertised_chunks != required_chunks {
    return Err(crate::engine::errors::EngineError::InvalidInput(format!(
      "remote sync chunk manifest is incomplete or contains unrelated hashes: required {}, advertised {}",
      required_chunks.len(),
      advertised_chunks.len()
    )));
  }

  Ok(ValidatedRemoteSyncDiff {
    root_hash,
    files,
    file_deletions,
    symlinks,
    symlink_deletions,
    required_chunks: required_chunks.into_iter().collect(),
  })
}

fn validate_remote_sync_path(
  resolver: &SystemFamilyPolicyResolver,
  seen_paths: &mut HashSet<String>,
  path: &str,
) -> Result<(), crate::engine::errors::EngineError> {
  if path.len() > u16::MAX as usize {
    return Err(crate::engine::errors::EngineError::InvalidInput(format!("remote sync path exceeds 65535 bytes: {}", path.len())));
  }
  if path.bytes().any(|byte| byte < 0x20 || byte == 0x7F) {
    return Err(crate::engine::errors::EngineError::InvalidInput(format!("remote sync path {path:?} contains control characters")));
  }
  if path.is_empty() || path == "/" || crate::engine::path_utils::normalize_path(path) != path {
    return Err(crate::engine::errors::EngineError::InvalidInput(format!("remote sync path '{path}' is not a canonical leaf path")));
  }
  if !seen_paths.insert(path.to_string()) {
    return Err(crate::engine::errors::EngineError::InvalidInput(format!("remote sync repeats mutation path '{path}'")));
  }
  resolver.require_transfer_leaf_path(path, SystemFamilyTransferOperationV1::PeerReplication)
}

fn decode_remote_hash(encoded: &str, hash_length: usize, field: &str) -> Result<Vec<u8>, crate::engine::errors::EngineError> {
  let hash = hex::decode(encoded)
    .map_err(|error| crate::engine::errors::EngineError::InvalidInput(format!("remote sync {field} is not valid hex: {error}")))?;
  if hash.len() != hash_length {
    return Err(crate::engine::errors::EngineError::InvalidInput(format!(
      "remote sync {field} is {} bytes, expected {hash_length}",
      hash.len()
    )));
  }
  Ok(hash)
}

fn decode_persisted_peer_hash(engine: &StorageEngine, encoded: &str, field: &str) -> Result<Vec<u8>, String> {
  let hash = hex::decode(encoded).map_err(|error| format!("Persisted peer {field} is not valid hex: {error}"))?;
  let expected = engine.hash_algo().hash_length();
  if hash.len() != expected {
    return Err(format!("Persisted peer {field} is {} bytes, expected {expected}", hash.len()));
  }
  Ok(hash)
}

fn validate_remote_chunk_response(
  engine: &StorageEngine,
  requested: &[Vec<u8>],
  response: RemoteChunksResponse,
) -> Result<Vec<crate::engine::sync_api::ChunkData>, crate::engine::errors::EngineError> {
  let expected = requested.iter().cloned().collect::<BTreeSet<_>>();
  if expected.len() != requested.len() {
    return Err(crate::engine::errors::EngineError::InvalidInput("local remote-chunk request contains duplicate hashes".to_string()));
  }
  if response.chunks.len() != expected.len() {
    return Err(crate::engine::errors::EngineError::InvalidInput(format!(
      "remote chunk response returned {} chunks for {} requested hashes",
      response.chunks.len(),
      expected.len()
    )));
  }

  let hash_length = engine.hash_algo().hash_length();
  let mut seen = BTreeSet::new();
  let mut decoded_total = 0u64;
  let mut chunks = Vec::with_capacity(response.chunks.len());
  for chunk in response.chunks {
    let hash = decode_remote_hash(&chunk.hash, hash_length, "chunk response hash")?;
    if !expected.contains(&hash) {
      return Err(crate::engine::errors::EngineError::InvalidInput(format!(
        "remote chunk response contains unrequested hash {}",
        chunk.hash
      )));
    }
    if !seen.insert(hash.clone()) {
      return Err(crate::engine::errors::EngineError::InvalidInput(format!("remote chunk response repeats hash {}", chunk.hash)));
    }
    decoded_total = decoded_total
      .checked_add(chunk.size)
      .ok_or_else(|| crate::engine::errors::EngineError::ResourceExhausted("remote chunk response size overflow".to_string()))?;
    if decoded_total > REMOTE_SYNC_CHUNKS_MAX_DECODED_BYTES {
      return Err(crate::engine::errors::EngineError::ResourceExhausted(format!(
        "remote chunk response exceeds {REMOTE_SYNC_CHUNKS_MAX_DECODED_BYTES} decoded bytes"
      )));
    }
    let data = base64::engine::general_purpose::STANDARD.decode(&chunk.data).map_err(|error| {
      crate::engine::errors::EngineError::InvalidInput(format!("remote chunk {} has invalid base64: {error}", chunk.hash))
    })?;
    if data.len() as u64 != chunk.size {
      return Err(crate::engine::errors::EngineError::InvalidInput(format!(
        "remote chunk {} declares {} bytes but decodes to {}",
        chunk.hash,
        chunk.size,
        data.len()
      )));
    }
    if crate::engine::directory_ops::chunk_content_hash(&data, &engine.hash_algo())? != hash {
      return Err(crate::engine::errors::EngineError::InvalidInput(format!(
        "remote chunk {} payload does not match its claimed hash",
        chunk.hash
      )));
    }
    chunks.push(crate::engine::sync_api::ChunkData { hash, data });
  }
  if seen != expected {
    return Err(crate::engine::errors::EngineError::InvalidInput("remote chunk response omitted a requested hash".to_string()));
  }
  Ok(chunks)
}

struct BoundedRemoteJson<T> {
  value: T,
  _memory: OperationMemoryBudget,
}

struct BoundedRemoteText {
  value: String,
  _memory: OperationMemoryBudget,
}

async fn read_bounded_remote_json<T: serde::de::DeserializeOwned>(
  engine: &StorageEngine,
  response: reqwest::Response,
  max_body_bytes: u64,
  operation: &'static str,
) -> Result<BoundedRemoteJson<T>, String> {
  let (body, mut memory) = read_bounded_remote_body(engine, response, max_body_bytes, operation).await?;
  let parsed_envelope_bytes = u64::try_from(body.len())
    .ok()
    .and_then(|bytes| bytes.checked_mul(3))
    .ok_or_else(|| format!("{operation} parsed-envelope estimate overflow"))?;
  memory.reserve(parsed_envelope_bytes, "parsed response envelope admission failed").map_err(|error| error.to_string())?;
  let value = serde_json::from_slice(&body).map_err(|error| error.to_string())?;
  Ok(BoundedRemoteJson { value, _memory: memory })
}

async fn read_bounded_remote_text(
  engine: &StorageEngine,
  response: reqwest::Response,
  max_body_bytes: u64,
  operation: &'static str,
) -> Result<BoundedRemoteText, String> {
  let (body, mut memory) = read_bounded_remote_body(engine, response, max_body_bytes, operation).await?;
  let text_envelope_bytes = u64::try_from(body.len())
    .ok()
    .and_then(|bytes| bytes.checked_mul(2))
    .ok_or_else(|| format!("{operation} text-envelope estimate overflow"))?;
  memory.reserve(text_envelope_bytes, "text response envelope admission failed").map_err(|error| error.to_string())?;
  let value = String::from_utf8_lossy(&body).into_owned();
  Ok(BoundedRemoteText { value, _memory: memory })
}

async fn read_bounded_remote_body(
  engine: &StorageEngine,
  mut response: reqwest::Response,
  max_body_bytes: u64,
  operation: &'static str,
) -> Result<(Vec<u8>, OperationMemoryBudget), String> {
  if response.content_length().is_some_and(|content_length| content_length > max_body_bytes) {
    return Err(format!("{operation} exceeds {max_body_bytes} bytes"));
  }

  let mut memory = OperationMemoryBudget::new(engine, operation, MemoryOwner::StreamingRead, AdmissionClass::Workload, 0, None)
    .map_err(|error| error.to_string())?;
  let mut body = Vec::new();
  while let Some(chunk) = response.chunk().await.map_err(|error| format!("failed while reading {operation}: {error}"))? {
    let new_length = body.len().checked_add(chunk.len()).ok_or_else(|| format!("{operation} length overflow"))?;
    if new_length as u64 > max_body_bytes {
      return Err(format!("{operation} exceeds {max_body_bytes} bytes"));
    }
    memory.reserve(chunk.len() as u64, "response body growth").map_err(|error| error.to_string())?;
    body.extend_from_slice(&chunk);
  }
  Ok((body, memory))
}

/// Filter a TreeDiff to only include entries matching the given glob patterns.
/// Entries whose paths don't match any pattern are removed from the diff.
fn filter_tree_diff_by_paths(mut diff: crate::engine::tree_walker::TreeDiff, paths: &[String]) -> crate::engine::tree_walker::TreeDiff {
  let matches = |path: &str| -> bool { paths.iter().any(|pattern| glob_match::glob_match(pattern, path)) };

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
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));
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
            )
            .increment(1);
            metrics::gauge!(
                crate::metrics::definitions::SYNC_CONSECUTIVE_FAILURES,
                "peer" => peer_id_str.clone()
            )
            .set(0.0);
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
            )
            .increment(1);
            metrics::gauge!(
                crate::metrics::definitions::SYNC_CONSECUTIVE_FAILURES,
                "peer" => peer_id_str.clone()
            )
            .set(failures as f64);
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

impl crate::engine::schema_version::JsonVersioned for PeerSyncState {
  const SCHEMA_VERSION: u8 = 1;

  fn serialize_versioned(&self) -> Vec<u8> {
    crate::engine::schema_version::write_json_with_version(self, Self::SCHEMA_VERSION)
      .expect("PeerSyncState serialization should never fail")
  }

  fn deserialize_versioned(data: &[u8]) -> crate::engine::errors::EngineResult<Self> {
    let version = crate::engine::schema_version::read_json_version(data)?;
    match version {
      0 | 1 => serde_json::from_slice(data)
        .map_err(|error| crate::engine::errors::EngineError::JsonParseError(format!("Failed to deserialize PeerSyncState: {error}"))),
      _ => Err(crate::engine::errors::EngineError::InvalidEntryVersion(version)),
    }
  }
}
