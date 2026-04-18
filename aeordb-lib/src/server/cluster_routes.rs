use axum::{
    Extension,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

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
            return ErrorResponse::new("Missing required field: address")
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

    // Generate a node_id for the new peer using rand.
    let peer_node_id: u64 = rand::random();

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
    let mut peer_configs = system_store::get_peer_configs(&state.engine).unwrap_or_default();
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
            return ErrorResponse::new(format!("Invalid node_id: {}", node_id_string))
                .with_status(StatusCode::BAD_REQUEST)
                .into_response();
        }
    };

    let removed = state.peer_manager.remove_peer(node_id);
    if !removed {
        return ErrorResponse::new(format!("Peer not found: {}", node_id))
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

/// POST /admin/cluster/sync -- trigger sync (placeholder).
pub async fn trigger_sync(
    State(_state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
) -> Response {
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": "Sync engine not yet implemented",
        })),
    )
        .into_response()
}
