use serde::Serialize;
use uuid::Uuid;

/// A single engine event with envelope metadata + typed payload.
#[derive(Debug, Clone, Serialize)]
pub struct EngineEvent {
    pub event_id: String,
    pub event_type: String,
    pub timestamp: i64,
    pub user_id: String,
    pub payload: serde_json::Value,
}

impl EngineEvent {
    pub fn new(event_type: &str, user_id: &str, payload: serde_json::Value) -> Self {
        EngineEvent {
            event_id: Uuid::new_v4().to_string(),
            event_type: event_type.to_string(),
            timestamp: chrono::Utc::now().timestamp_millis(),
            user_id: user_id.to_string(),
            payload,
        }
    }
}

// --- Payload data structs ---

#[derive(Debug, Clone, Serialize)]
pub struct EntryEventData {
    pub path: String,
    pub entry_type: String, // "file" or "directory"
    pub content_type: Option<String>,
    pub size: u64,
    pub hash: String,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VersionEventData {
    pub name: String,
    pub version_type: Option<String>, // "snapshot" or "fork" (None for promote/restore)
    pub root_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserEventData {
    pub target_user_id: String,
    pub username: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PermissionChangeData {
    pub path: String,
    pub group_name: String,
    pub action: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportEventData {
    pub backup_type: String,
    pub version_hash: String,
    pub entries_imported: u64,
    pub head_promoted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexEventData {
    pub path: String,
    pub field_name: String,
    pub strategy: String,
    pub entry_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorEventData {
    pub path: Option<String>,
    pub error_type: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenEventData {
    pub target_user_id: String,
    pub method: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiKeyEventData {
    pub target_user_id: String,
    pub key_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginEventData {
    pub name: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HeartbeatData {
    pub entry_count: u64,
    pub kv_entries: usize,
    pub chunk_count: usize,
    pub file_count: usize,
    pub directory_count: usize,
    pub snapshot_count: usize,
    pub fork_count: usize,
    pub void_count: usize,
    pub void_space_bytes: u64,
    pub db_file_size_bytes: u64,
    pub kv_size_bytes: u64,
    pub nvt_buckets: usize,
    /// The aligned boundary time (ms) this heartbeat targeted.
    pub intent_time: u64,
    /// Actual wall-clock time (ms) when the heartbeat message was constructed.
    pub construct_time: u64,
    /// This node's unique identifier.
    pub node_id: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GcEventData {
    pub versions_scanned: usize,
    pub live_entries: usize,
    pub garbage_entries: usize,
    pub reclaimed_bytes: u64,
    pub duration_ms: u64,
    pub dry_run: bool,
}

// --- Event type constants ---
pub const EVENT_ENTRIES_CREATED: &str = "entries_created";
pub const EVENT_ENTRIES_UPDATED: &str = "entries_updated";
pub const EVENT_ENTRIES_DELETED: &str = "entries_deleted";
pub const EVENT_VERSIONS_CREATED: &str = "versions_created";
pub const EVENT_VERSIONS_DELETED: &str = "versions_deleted";
pub const EVENT_VERSIONS_PROMOTED: &str = "versions_promoted";
pub const EVENT_VERSIONS_RESTORED: &str = "versions_restored";
pub const EVENT_USERS_CREATED: &str = "users_created";
pub const EVENT_USERS_ACTIVATED: &str = "users_activated";
pub const EVENT_USERS_DEACTIVATED: &str = "users_deactivated";
pub const EVENT_PERMISSIONS_CHANGED: &str = "permissions_changed";
pub const EVENT_IMPORTS_COMPLETED: &str = "imports_completed";
pub const EVENT_INDEXES_UPDATED: &str = "indexes_updated";
pub const EVENT_ERRORS: &str = "errors";
pub const EVENT_TOKENS_EXCHANGED: &str = "tokens_exchanged";
pub const EVENT_API_KEYS_CREATED: &str = "api_keys_created";
pub const EVENT_API_KEYS_REVOKED: &str = "api_keys_revoked";
pub const EVENT_PLUGINS_DEPLOYED: &str = "plugins_deployed";
pub const EVENT_PLUGINS_REMOVED: &str = "plugins_removed";
pub const EVENT_HEARTBEAT: &str = "heartbeat";
pub const EVENT_GC_COMPLETED: &str = "gc_completed";
pub const EVENT_TASK_CREATED: &str = "task_created";
pub const EVENT_TASK_STARTED: &str = "task_started";
pub const EVENT_TASK_COMPLETED: &str = "task_completed";
pub const EVENT_TASK_FAILED: &str = "task_failed";
pub const EVENT_TASK_CANCELLED: &str = "task_cancelled";
pub const EVENT_SYNC_SUCCEEDED: &str = "sync_succeeded";
pub const EVENT_SYNC_FAILED: &str = "sync_failed";
