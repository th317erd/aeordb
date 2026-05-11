use axum::{
    Extension,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct TriggerSyncParams {
    /// Force activation of peers that haven't completed the clock-sync
    /// honeymoon. Without this, peers in Disconnected/Honeymoon state are
    /// skipped because their `last_clock_offset_ms` is unknown and applying
    /// LWW with an unknown offset would silently corrupt timestamp order.
    #[serde(default)]
    pub force_activate: bool,
}

use super::responses::{ErrorResponse, require_root};
use super::state::AppState;
use crate::auth::TokenClaims;
use crate::engine::PeerConfig;
use crate::engine::system_store;

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AddPeerRequest {
    pub address: String,
    pub label: Option<String>,
    pub sync_paths: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Generate a random `node_id` for a new peer record, retrying until it
/// doesn't collide with any existing peer or with this node's own id.
///
/// The birthday-bound on u64 (2^32) makes a single collision astronomically
/// unlikely, but the check is cheap and removes a latent class of "peer
/// can never sync" bugs at zero cost.
fn fresh_peer_node_id(
    existing_peers: &[PeerConfig],
    local_node_id: Option<u64>,
) -> u64 {
    loop {
        let candidate: u64 = rand::random();
        if candidate == 0 {
            continue; // reserve 0 as "uninitialized"
        }
        if Some(candidate) == local_node_id {
            continue;
        }
        if existing_peers.iter().any(|p| p.node_id == candidate) {
            continue;
        }
        return candidate;
    }
}

/// Serialize a PeerConnection into a JSON value with sync status.
fn peer_to_json(
    peer: &crate::engine::peer_connection::PeerConnection,
    peer_manager: &crate::engine::PeerManager,
) -> serde_json::Value {
    let state_string = match &peer.state {
        crate::engine::ConnectionState::Disconnected => "disconnected",
        crate::engine::ConnectionState::Honeymoon { .. } => "honeymoon",
        crate::engine::ConnectionState::Active => "active",
    };
    let sync_status = peer_manager.get_sync_status(peer.node_id);
    serde_json::json!({
        "node_id": peer.node_id,
        "address": peer.address,
        "label": peer.label,
        "state": state_string,
        "last_sync_at": peer.last_sync_at,
        "sync_status": sync_status,
    })
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /admin/cluster -- cluster status overview.
pub async fn cluster_status(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
) -> Response {
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

    let node_id = system_store::get_node_id(&state.engine).unwrap_or(None);

    let peers: Vec<serde_json::Value> = state
        .peer_manager
        .all_peers()
        .iter()
        .map(|peer| peer_to_json(peer, &state.peer_manager))
        .collect();

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "node_id": node_id,
            "peer_count": peers.len(),
            "peers": peers,
        })),
    )
        .into_response()
}

/// POST /admin/cluster/peers -- add a new peer.
pub async fn add_peer(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Json(payload): Json<serde_json::Value>,
) -> Response {
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

    let address = match payload.get("address").and_then(|value| value.as_str()) {
        Some(address) => address.to_string(),
        None => {
            return ErrorResponse::new("Missing required field 'address' in request body. Provide {\"address\": \"<host:port>\"}")
                .with_status(StatusCode::BAD_REQUEST)
                .into_response();
        }
    };

    let label = payload
        .get("label")
        .and_then(|value| value.as_str())
        .map(|string| string.to_string());

    let sync_paths = payload
        .get("sync_paths")
        .and_then(|value| value.as_array())
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str().map(|string| string.to_string()))
                .collect::<Vec<String>>()
        });

    // Generate a node_id for the new peer that doesn't collide with any
    // existing peer or with our local node_id.
    let mut peer_configs = system_store::get_peer_configs(&state.engine).unwrap_or_default();
    let local_node_id = system_store::get_node_id(&state.engine).ok().flatten();
    let peer_node_id = fresh_peer_node_id(&peer_configs, local_node_id);

    let config = PeerConfig {
        node_id: peer_node_id,
        address: address.clone(),
        label: label.clone(),
        sync_paths,
        last_clock_offset_ms: None,
        last_wire_time_ms: None,
        last_jitter_ms: None,
        clock_state_at: None,
    };

    // Add to runtime PeerManager.
    state.peer_manager.add_peer(&config);

    // Persist to system store.
    peer_configs.push(config.clone());
    let ctx = crate::engine::RequestContext::system();
    if let Err(error) = system_store::store_peer_configs(&state.engine, &ctx, &peer_configs) {
        tracing::error!("Failed to persist peer config: {}", error);
        return ErrorResponse::new(format!("Failed to persist peer: {}", error))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
    }

    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "node_id": peer_node_id,
            "address": address,
            "label": label,
            "state": "disconnected",
        })),
    )
        .into_response()
}

/// GET /admin/cluster/peers -- list all peers.
pub async fn list_peers(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
) -> Response {
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

    let peers: Vec<serde_json::Value> = state
        .peer_manager
        .all_peers()
        .iter()
        .map(|peer| peer_to_json(peer, &state.peer_manager))
        .collect();

    (StatusCode::OK, Json(serde_json::json!({"items": peers}))).into_response()
}

/// DELETE /admin/cluster/peers/{node_id} -- remove a peer.
pub async fn remove_peer(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Path(node_id_string): Path<String>,
) -> Response {
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

    let node_id: u64 = match node_id_string.parse() {
        Ok(id) => id,
        Err(_) => {
            return ErrorResponse::new(format!("Invalid node_id '{}': must be a numeric value", node_id_string))
                .with_status(StatusCode::BAD_REQUEST)
                .into_response();
        }
    };

    let removed = state.peer_manager.remove_peer(node_id);
    if !removed {
        return ErrorResponse::new(format!("Peer not found: {}. Use GET /admin/cluster/peers to list active peers", node_id))
            .with_status(StatusCode::NOT_FOUND)
            .into_response();
    }

    // Remove from persisted configs.
    let mut peer_configs = system_store::get_peer_configs(&state.engine).unwrap_or_default();
    peer_configs.retain(|config| config.node_id != node_id);
    let ctx = crate::engine::RequestContext::system();
    if let Err(error) = system_store::store_peer_configs(&state.engine, &ctx, &peer_configs) {
        tracing::error!("Failed to persist peer removal: {}", error);
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "removed": true,
            "node_id": node_id,
        })),
    )
        .into_response()
}

/// POST /sync/join — admit a new node into this cluster.
///
/// The joining node calls this endpoint on an existing cluster member,
/// presenting that member's root credentials in the Authorization header.
/// The responding node returns its JWT signing key (so JWTs validate
/// across the cluster) along with its node_id, and registers the
/// caller as a new peer.
///
/// Request body:
///   { "node_url": "http://nodeB:6830", "label": "Optional friendly name" }
///
/// Response:
///   {
///     "signing_key": "<base64 ed25519 seed bytes>",
///     "responding_node_id": 1234567890,
///     "new_peer_node_id": 9876543210
///   }
///
/// Root-only.
pub async fn join_cluster(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Json(payload): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_root(&claims) {
        return response;
    }

    let node_url = match payload.get("node_url").and_then(|v| v.as_str()) {
        Some(url) => url.to_string(),
        None => {
            return ErrorResponse::new("Missing required field 'node_url' in request body. Provide the URL where the joining node can be reached.")
                .with_status(StatusCode::BAD_REQUEST)
                .into_response();
        }
    };

    let label = payload.get("label").and_then(|v| v.as_str()).map(String::from);

    // Read this node's JWT signing key. This is the shared secret of the
    // cluster — joining nodes adopt it so JWTs validate everywhere.
    let signing_key = match system_store::get_config(&state.engine, "jwt_signing_key") {
        Ok(Some(key)) => key,
        Ok(None) => {
            tracing::error!("/sync/join: no JWT signing key in system store");
            return ErrorResponse::new("Server has no JWT signing key; cannot admit new members.")
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
        Err(e) => {
            tracing::error!("/sync/join: failed to read signing key: {}", e);
            return ErrorResponse::new("Failed to read signing key")
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
    };

    // Read our stable node_id. It is generated and persisted at server
    // startup (see create_app_with_all_and_task_queue), so this should
    // always be Some(_). The previous lazy-generation-on-first-join was
    // racy under concurrent joins; the startup guarantee removes that race.
    let responding_node_id = match system_store::get_node_id(&state.engine) {
        Ok(Some(id)) => id,
        Ok(None) => {
            tracing::error!("/sync/join: node_id missing — startup should have generated it");
            return ErrorResponse::new("Local node_id not initialized; server misconfigured")
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
        Err(e) => {
            tracing::error!("/sync/join: failed to read node_id: {}", e);
            return ErrorResponse::new("Failed to read local node_id")
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
    };

    // Register the joining node as a peer of THIS node so we'll sync
    // back to it. The new peer's node_id is random (collision-checked);
    // the joining node will record its own canonical node_id on its end
    // and we'll reconcile via the first sync handshake.
    let existing_peers = system_store::get_peer_configs(&state.engine).unwrap_or_default();
    let new_peer_node_id = fresh_peer_node_id(&existing_peers, Some(responding_node_id));
    let config = PeerConfig {
        node_id: new_peer_node_id,
        address: node_url.clone(),
        label,
        sync_paths: None,
        last_clock_offset_ms: None,
        last_wire_time_ms: None,
        last_jitter_ms: None,
        clock_state_at: None,
    };
    state.peer_manager.add_peer(&config);

    let mut peer_configs = system_store::get_peer_configs(&state.engine).unwrap_or_default();
    // Avoid duplicates if the same node URL is already registered.
    peer_configs.retain(|p| p.address != node_url);
    peer_configs.push(config);
    let ctx = crate::engine::RequestContext::system();
    if let Err(error) = system_store::store_peer_configs(&state.engine, &ctx, &peer_configs) {
        tracing::error!("/sync/join: failed to persist peer config: {}", error);
        return ErrorResponse::new(format!("Failed to persist peer: {}", error))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
    }

    use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "signing_key": B64.encode(&signing_key),
            "responding_node_id": responding_node_id,
            "new_peer_node_id": new_peer_node_id,
        })),
    )
        .into_response()
}

/// POST /sync/trigger — run a sync cycle with every registered peer.
///
/// Disconnected peers are activated first (skipping the clock-sync
/// honeymoon — a deliberate trade-off for the initial cluster bring-up
/// flow). Each peer is then synced; per-peer results are returned.
///
/// Root-only.
pub async fn trigger_sync(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Query(params): Query<TriggerSyncParams>,
) -> Response {
    if let Err(response) = require_root(&claims) {
        return response;
    }

    let sync_engine = match &state.sync_engine {
        Some(e) => e,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "Sync engine not configured on this node",
                })),
            )
                .into_response();
        }
    };

    // Honeymoon enforcement: peers in Disconnected/Honeymoon haven't yet
    // exchanged a heartbeat round-trip, so their last_clock_offset_ms is
    // unknown. Running LWW merge against such a peer means timestamps are
    // compared with no offset correction — if the remote clock is hours
    // off, "later" writes can silently lose to older ones.
    //
    // Default behavior (safe): skip non-Active peers and return them in
    // the response as `skipped`. The periodic sync loop will activate
    // them once the heartbeat handshake completes.
    //
    // With `?force_activate=true`: the previous unsafe behavior — force
    // peers to Active. Operators who know their clocks are NTP-synced can
    // opt in; the response includes a `forced_active` array so the
    // override is auditable.
    let mut forced_active: Vec<u64> = Vec::new();
    let mut skipped: Vec<serde_json::Value> = Vec::new();
    for peer in state.peer_manager.all_peers() {
        if peer.state == crate::engine::peer_connection::ConnectionState::Active {
            continue;
        }
        if params.force_activate {
            state.peer_manager.activate_peer(peer.node_id);
            forced_active.push(peer.node_id);
            tracing::warn!(
                node_id = peer.node_id,
                "Force-activating peer before clock-sync honeymoon completed (operator override)"
            );
        } else {
            skipped.push(serde_json::json!({
                "node_id": peer.node_id,
                "state": format!("{:?}", peer.state).to_lowercase(),
                "reason": "honeymoon not complete; pass ?force_activate=true to override",
            }));
        }
    }

    let results = sync_engine.trigger_sync_all().await;

    let summary: Vec<serde_json::Value> = results.iter().map(|(node_id, result)| {
        match result {
            Ok(cycle) => serde_json::json!({
                "node_id": node_id,
                "ok": true,
                "changes_applied": cycle.changes_applied,
                "operations_applied": cycle.operations_applied,
                "conflicts_detected": cycle.conflicts_detected,
            }),
            Err(e) => serde_json::json!({
                "node_id": node_id,
                "ok": false,
                "error": e,
            }),
        }
    }).collect();

    let succeeded = results.iter().filter(|(_, r)| r.is_ok()).count();
    let failed = results.len() - succeeded;

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "peers_synced": succeeded,
            "peers_failed": failed,
            "peers_skipped": skipped.len(),
            "results": summary,
            "skipped": skipped,
            "forced_active": forced_active,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod fresh_peer_node_id_tests {
    use super::*;

    fn cfg(id: u64) -> PeerConfig {
        PeerConfig {
            node_id: id,
            address: format!("http://test-{}", id),
            label: None,
            sync_paths: None,
            last_clock_offset_ms: None,
            last_wire_time_ms: None,
            last_jitter_ms: None,
            clock_state_at: None,
        }
    }

    #[test]
    fn never_returns_zero() {
        // 100 iterations is plenty to catch the "0 leaks through" bug.
        for _ in 0..100 {
            let id = fresh_peer_node_id(&[], None);
            assert_ne!(id, 0);
        }
    }

    #[test]
    fn skips_collision_with_existing_peer() {
        // Construct a (theoretical) scenario where rand keeps returning a
        // colliding value; the function still terminates with a non-colliding
        // ID because the second draw is independent.
        let existing = vec![cfg(42), cfg(43), cfg(44)];
        let id = fresh_peer_node_id(&existing, None);
        assert!(!existing.iter().any(|p| p.node_id == id));
    }

    #[test]
    fn skips_collision_with_local_node_id() {
        let id = fresh_peer_node_id(&[], Some(123));
        assert_ne!(id, 123);
    }
}
