use std::sync::Arc;
use axum::extract::State;
use axum::Extension;
use axum::response::{Html, IntoResponse, Json};
use axum::http::{header, StatusCode};
use serde::Serialize;
use super::state::AppState;
use crate::engine::directory_ops::DEFAULT_CHUNK_SIZE;
use crate::engine::health::check_disk;
use crate::engine::rate_tracker::RateSnapshot;

// Embed portal assets at compile time
const PORTAL_HTML: &str = include_str!("../portal/index.html");
const PORTAL_APP_MJS: &str = include_str!("../portal/app.mjs");
const PORTAL_DASHBOARD_MJS: &str = include_str!("../portal/dashboard.mjs");
const PORTAL_USERS_MJS: &str = include_str!("../portal/users.mjs");
const PORTAL_GROUPS_MJS: &str = include_str!("../portal/groups.mjs");
const PORTAL_SHARED_UTILS_JS: &str = include_str!("../portal/shared/utils.js");
const PORTAL_SHARED_API_JS: &str = include_str!("../portal/shared/api.js");
const PORTAL_SHARED_CRUDLIFY_JS: &str = include_str!("../portal/shared/components/aeor-crudlify.js");
const PORTAL_SHARED_TOASTS_JS: &str = include_str!("../portal/shared/components/aeor-toast.js");
const PORTAL_SHARED_MODAL_JS: &str = include_str!("../portal/shared/components/aeor-modal.js");
const PORTAL_SHARED_LOGIN_JS: &str = include_str!("../portal/shared/components/aeor-login.js");
const PORTAL_SHARED_DASHBOARD_JS: &str = include_str!("../portal/shared/components/aeor-dashboard.js");
const PORTAL_SHARED_TOKENS_CSS: &str = include_str!("../portal/shared/styles/tokens.css");
const PORTAL_SHARED_COMPONENTS_CSS: &str = include_str!("../portal/shared/styles/components.css");
const PORTAL_FILES_MJS: &str = include_str!("../portal/files.mjs");
const PORTAL_SNAPSHOTS_MJS: &str = include_str!("../portal/snapshots.mjs");
const PORTAL_SETTINGS_MJS: &str = include_str!("../portal/settings.mjs");
const PORTAL_SHARED_FILE_BROWSER_JS: &str = include_str!("../portal/shared/components/aeor-file-browser.js");
const PORTAL_SHARED_FILE_BROWSER_ADAPTER_JS: &str = include_str!("../portal/shared/components/aeor-file-browser-adapter.js");
const PORTAL_SHARED_FILE_VIEW_SHARED_JS: &str = include_str!("../portal/shared/components/aeor-file-view-shared.js");
const PORTAL_SHARED_PREVIEW_IMAGE_JS: &str = include_str!("../portal/shared/components/previews/aeor-preview-image.js");
const PORTAL_SHARED_PREVIEW_VIDEO_JS: &str = include_str!("../portal/shared/components/previews/aeor-preview-video.js");
const PORTAL_SHARED_PREVIEW_AUDIO_JS: &str = include_str!("../portal/shared/components/previews/aeor-preview-audio.js");
const PORTAL_SHARED_PREVIEW_TEXT_JS: &str = include_str!("../portal/shared/components/previews/aeor-preview-text.js");
const PORTAL_SHARED_PREVIEW_DEFAULT_JS: &str = include_str!("../portal/shared/components/previews/aeor-preview-default.js");
const PORTAL_SHARED_PREVIEW_PDF_JS: &str = include_str!("../portal/shared/components/previews/aeor-preview-pdf.js");
const PORTAL_SHARED_FILE_BROWSER_BASE_JS: &str = include_str!("../portal/shared/components/aeor-file-browser-base.js");
const PORTAL_SHARED_FILE_BROWSER_PORTAL_JS: &str = include_str!("../portal/shared/components/aeor-file-browser-portal.js");
const PORTAL_SHARED_CONFIRM_BUTTON_JS: &str = include_str!("../portal/shared/components/aeor-confirm-button.js");
const PORTAL_SHARED_INFO_BOX_JS: &str = include_str!("../portal/shared/components/aeor-info-box.js");
const PORTAL_SHARED_TAB_VIEW_JS: &str = include_str!("../portal/shared/components/aeor-tab-view.js");
const PORTAL_SHARED_SNAPSHOT_CARD_JS: &str = include_str!("../portal/shared/components/aeor-snapshot-card.js");
const PORTAL_SHARED_ADMIN_PAGE_JS: &str = include_str!("../portal/shared/components/aeor-admin-page.js");
const PORTAL_SHARED_KEYS_PAGE_JS: &str = include_str!("../portal/shared/components/aeor-keys-page.js");

// Universal `aeor-web-components` (symlinked into portal/aeor/ at build time).
// The migrated DB-shared components import from `/aeor/...`, so the portal must
// serve these alongside `/shared/...`. See engine-task-portal-universal-aeor-route-2026-05-19.md.
const PORTAL_AEOR_ELEMENTS_JS:        &str = include_str!("../portal/aeor/elements.js");
const PORTAL_AEOR_QUERY_JS:           &str = include_str!("../portal/aeor/query.js");
const PORTAL_AEOR_REACTIVE_STATE_JS:  &str = include_str!("../portal/aeor/reactive-state.js");
const PORTAL_AEOR_CONFIRM_JS:         &str = include_str!("../portal/aeor/confirm.js");
const PORTAL_AEOR_UTILS_JS:           &str = include_str!("../portal/aeor/utils.js");

const PORTAL_AEOR_CHECKBOX_JS:        &str = include_str!("../portal/aeor/components/aeor-checkbox.js");
const PORTAL_AEOR_CHECKBOX_CSS:       &str = include_str!("../portal/aeor/components/aeor-checkbox.css");
const PORTAL_AEOR_CONFIRM_BUTTON_JS:  &str = include_str!("../portal/aeor/components/aeor-confirm-button.js");
const PORTAL_AEOR_CONFIRM_BUTTON_CSS: &str = include_str!("../portal/aeor/components/aeor-confirm-button.css");
const PORTAL_AEOR_CONFIRM_DIALOG_CSS: &str = include_str!("../portal/aeor/components/aeor-confirm-dialog.css");
const PORTAL_AEOR_DISCLOSURE_JS:      &str = include_str!("../portal/aeor/components/aeor-disclosure.js");
const PORTAL_AEOR_DISCLOSURE_CSS:     &str = include_str!("../portal/aeor/components/aeor-disclosure.css");
const PORTAL_AEOR_INFO_BOX_JS:        &str = include_str!("../portal/aeor/components/aeor-info-box.js");
const PORTAL_AEOR_INFO_BOX_CSS:       &str = include_str!("../portal/aeor/components/aeor-info-box.css");
const PORTAL_AEOR_INPUT_JS:           &str = include_str!("../portal/aeor/components/aeor-input.js");
const PORTAL_AEOR_INPUT_CSS:          &str = include_str!("../portal/aeor/components/aeor-input.css");
const PORTAL_AEOR_MODAL_JS:           &str = include_str!("../portal/aeor/components/aeor-modal.js");
const PORTAL_AEOR_MODAL_CSS:          &str = include_str!("../portal/aeor/components/aeor-modal.css");
const PORTAL_AEOR_PROMPT_JS:          &str = include_str!("../portal/aeor/components/aeor-prompt.js");
const PORTAL_AEOR_PROMPT_CSS:         &str = include_str!("../portal/aeor/components/aeor-prompt.css");
const PORTAL_AEOR_SELECT_JS:          &str = include_str!("../portal/aeor/components/aeor-select.js");
const PORTAL_AEOR_SELECT_CSS:         &str = include_str!("../portal/aeor/components/aeor-select.css");
const PORTAL_AEOR_SPLIT_BUTTON_JS:    &str = include_str!("../portal/aeor/components/aeor-split-button.js");
const PORTAL_AEOR_SPLIT_BUTTON_CSS:   &str = include_str!("../portal/aeor/components/aeor-split-button.css");
const PORTAL_AEOR_TAB_VIEW_JS:        &str = include_str!("../portal/aeor/components/aeor-tab-view.js");
const PORTAL_AEOR_TAB_VIEW_CSS:       &str = include_str!("../portal/aeor/components/aeor-tab-view.css");
const PORTAL_AEOR_TOAST_JS:           &str = include_str!("../portal/aeor/components/aeor-toast.js");
const PORTAL_AEOR_TOAST_CSS:          &str = include_str!("../portal/aeor/components/aeor-toast.css");

const PORTAL_AEOR_TOKENS_CSS:         &str = include_str!("../portal/aeor/styles/tokens.css");
const PORTAL_AEOR_GLOBALS_CSS:        &str = include_str!("../portal/aeor/styles/globals.css");
const PORTAL_AEOR_COMPONENTS_CSS:     &str = include_str!("../portal/aeor/styles/components.css");

/// Serve the main portal HTML page.
pub async fn portal_index() -> Html<&'static str> {
    Html(PORTAL_HTML)
}

/// Serve portal JS assets with correct content type.
pub async fn portal_asset(
    request: axum::http::request::Parts,
) -> impl IntoResponse {
    let filename = request.uri.path().trim_start_matches('/');
    let (content, content_type) = match filename {
        "app.mjs" => (PORTAL_APP_MJS, "application/javascript; charset=utf-8"),
        "dashboard.mjs" => (PORTAL_DASHBOARD_MJS, "application/javascript; charset=utf-8"),
        "users.mjs" => (PORTAL_USERS_MJS, "application/javascript; charset=utf-8"),
        "groups.mjs" => (PORTAL_GROUPS_MJS, "application/javascript; charset=utf-8"),
        "files.mjs" => (PORTAL_FILES_MJS, "application/javascript; charset=utf-8"),
        "snapshots.mjs" => (PORTAL_SNAPSHOTS_MJS, "application/javascript; charset=utf-8"),
        "settings.mjs" => (PORTAL_SETTINGS_MJS, "application/javascript; charset=utf-8"),
        _ => return (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "text/plain")], "Not found").into_response(),
    };

    (StatusCode::OK, [(header::CONTENT_TYPE, content_type)], content).into_response()
}

/// Serve shared web-component assets (symlinked into portal/shared/ at build time).
/// Accepts a wildcard path to support nested directories (e.g., components/aeor-crudlify.js).
pub async fn portal_shared_asset(
    axum::extract::Path(path): axum::extract::Path<String>,
) -> impl IntoResponse {
    let (content, content_type) = match path.as_str() {
        "utils.js" => (PORTAL_SHARED_UTILS_JS, "application/javascript; charset=utf-8"),
        "api.js" => (PORTAL_SHARED_API_JS, "application/javascript; charset=utf-8"),
        "components/aeor-crudlify.js" => (PORTAL_SHARED_CRUDLIFY_JS, "application/javascript; charset=utf-8"),
        "components/aeor-toast.js" => (PORTAL_SHARED_TOASTS_JS, "application/javascript; charset=utf-8"),
        "components/aeor-modal.js" => (PORTAL_SHARED_MODAL_JS, "application/javascript; charset=utf-8"),
        "components/aeor-login.js" => (PORTAL_SHARED_LOGIN_JS, "application/javascript; charset=utf-8"),
        "components/aeor-dashboard.js" => (PORTAL_SHARED_DASHBOARD_JS, "application/javascript; charset=utf-8"),
        "components/aeor-file-browser.js" => (PORTAL_SHARED_FILE_BROWSER_JS, "application/javascript; charset=utf-8"),
        "components/aeor-file-browser-adapter.js" => (PORTAL_SHARED_FILE_BROWSER_ADAPTER_JS, "application/javascript; charset=utf-8"),
        "components/aeor-file-browser-base.js" => (PORTAL_SHARED_FILE_BROWSER_BASE_JS, "application/javascript; charset=utf-8"),
        "components/aeor-file-browser-portal.js" => (PORTAL_SHARED_FILE_BROWSER_PORTAL_JS, "application/javascript; charset=utf-8"),
        "components/aeor-confirm-button.js" => (PORTAL_SHARED_CONFIRM_BUTTON_JS, "application/javascript; charset=utf-8"),
        "components/aeor-info-box.js" => (PORTAL_SHARED_INFO_BOX_JS, "application/javascript; charset=utf-8"),
        "components/aeor-tab-view.js" => (PORTAL_SHARED_TAB_VIEW_JS, "application/javascript; charset=utf-8"),
        "components/aeor-snapshot-card.js" => (PORTAL_SHARED_SNAPSHOT_CARD_JS, "application/javascript; charset=utf-8"),
        "components/aeor-admin-page.js" => (PORTAL_SHARED_ADMIN_PAGE_JS, "application/javascript; charset=utf-8"),
        "components/aeor-keys-page.js" => (PORTAL_SHARED_KEYS_PAGE_JS, "application/javascript; charset=utf-8"),
        "components/aeor-file-view-shared.js" => (PORTAL_SHARED_FILE_VIEW_SHARED_JS, "application/javascript; charset=utf-8"),
        "components/previews/aeor-preview-image.js" => (PORTAL_SHARED_PREVIEW_IMAGE_JS, "application/javascript; charset=utf-8"),
        "components/previews/aeor-preview-video.js" => (PORTAL_SHARED_PREVIEW_VIDEO_JS, "application/javascript; charset=utf-8"),
        "components/previews/aeor-preview-audio.js" => (PORTAL_SHARED_PREVIEW_AUDIO_JS, "application/javascript; charset=utf-8"),
        "components/previews/aeor-preview-text.js" => (PORTAL_SHARED_PREVIEW_TEXT_JS, "application/javascript; charset=utf-8"),
        "components/previews/aeor-preview-default.js" => (PORTAL_SHARED_PREVIEW_DEFAULT_JS, "application/javascript; charset=utf-8"),
        "components/previews/aeor-preview-pdf.js" => (PORTAL_SHARED_PREVIEW_PDF_JS, "application/javascript; charset=utf-8"),
        "styles/tokens.css" => (PORTAL_SHARED_TOKENS_CSS, "text/css; charset=utf-8"),
        "styles/components.css" => (PORTAL_SHARED_COMPONENTS_CSS, "text/css; charset=utf-8"),
        _ => return (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "text/plain")], "Not found").into_response(),
    };

    (StatusCode::OK, [(header::CONTENT_TYPE, content_type)], content).into_response()
}

/// Serve universal `aeor-web-components` assets (symlinked into portal/aeor/
/// at build time). Lookup by relative path under `/aeor/`. Migrated DB-shared
/// components import these via `/aeor/elements.js`, `/aeor/components/*`,
/// `/aeor/styles/*`. See engine-task-portal-universal-aeor-route-2026-05-19.md
/// for context.
///
/// In dev builds, on a cache miss we also fall back to reading the file from
/// disk via the symlink so we can hot-edit `aeor-web-components` without
/// rebuilding the engine. Release builds serve only the baked-in content.
pub async fn portal_aeor_asset(
    axum::extract::Path(path): axum::extract::Path<String>,
) -> impl IntoResponse {
    let (content, content_type) = match path.as_str() {
        // Core
        "elements.js"           => (PORTAL_AEOR_ELEMENTS_JS,        "application/javascript; charset=utf-8"),
        "query.js"              => (PORTAL_AEOR_QUERY_JS,           "application/javascript; charset=utf-8"),
        "reactive-state.js"     => (PORTAL_AEOR_REACTIVE_STATE_JS,  "application/javascript; charset=utf-8"),
        "confirm.js"            => (PORTAL_AEOR_CONFIRM_JS,         "application/javascript; charset=utf-8"),
        "utils.js"              => (PORTAL_AEOR_UTILS_JS,           "application/javascript; charset=utf-8"),

        // Components — JS
        "components/aeor-checkbox.js"       => (PORTAL_AEOR_CHECKBOX_JS,       "application/javascript; charset=utf-8"),
        "components/aeor-confirm-button.js" => (PORTAL_AEOR_CONFIRM_BUTTON_JS, "application/javascript; charset=utf-8"),
        "components/aeor-disclosure.js"     => (PORTAL_AEOR_DISCLOSURE_JS,     "application/javascript; charset=utf-8"),
        "components/aeor-info-box.js"       => (PORTAL_AEOR_INFO_BOX_JS,       "application/javascript; charset=utf-8"),
        "components/aeor-input.js"          => (PORTAL_AEOR_INPUT_JS,          "application/javascript; charset=utf-8"),
        "components/aeor-modal.js"          => (PORTAL_AEOR_MODAL_JS,          "application/javascript; charset=utf-8"),
        "components/aeor-prompt.js"         => (PORTAL_AEOR_PROMPT_JS,         "application/javascript; charset=utf-8"),
        "components/aeor-select.js"         => (PORTAL_AEOR_SELECT_JS,         "application/javascript; charset=utf-8"),
        "components/aeor-split-button.js"   => (PORTAL_AEOR_SPLIT_BUTTON_JS,   "application/javascript; charset=utf-8"),
        "components/aeor-tab-view.js"       => (PORTAL_AEOR_TAB_VIEW_JS,       "application/javascript; charset=utf-8"),
        "components/aeor-toast.js"          => (PORTAL_AEOR_TOAST_JS,          "application/javascript; charset=utf-8"),

        // Components — CSS
        "components/aeor-checkbox.css"       => (PORTAL_AEOR_CHECKBOX_CSS,       "text/css; charset=utf-8"),
        "components/aeor-confirm-button.css" => (PORTAL_AEOR_CONFIRM_BUTTON_CSS, "text/css; charset=utf-8"),
        "components/aeor-confirm-dialog.css" => (PORTAL_AEOR_CONFIRM_DIALOG_CSS, "text/css; charset=utf-8"),
        "components/aeor-disclosure.css"     => (PORTAL_AEOR_DISCLOSURE_CSS,     "text/css; charset=utf-8"),
        "components/aeor-info-box.css"       => (PORTAL_AEOR_INFO_BOX_CSS,       "text/css; charset=utf-8"),
        "components/aeor-input.css"          => (PORTAL_AEOR_INPUT_CSS,          "text/css; charset=utf-8"),
        "components/aeor-modal.css"          => (PORTAL_AEOR_MODAL_CSS,          "text/css; charset=utf-8"),
        "components/aeor-prompt.css"         => (PORTAL_AEOR_PROMPT_CSS,         "text/css; charset=utf-8"),
        "components/aeor-select.css"         => (PORTAL_AEOR_SELECT_CSS,         "text/css; charset=utf-8"),
        "components/aeor-split-button.css"   => (PORTAL_AEOR_SPLIT_BUTTON_CSS,   "text/css; charset=utf-8"),
        "components/aeor-tab-view.css"       => (PORTAL_AEOR_TAB_VIEW_CSS,       "text/css; charset=utf-8"),
        "components/aeor-toast.css"          => (PORTAL_AEOR_TOAST_CSS,          "text/css; charset=utf-8"),

        // Shared styles
        "styles/tokens.css"     => (PORTAL_AEOR_TOKENS_CSS,     "text/css; charset=utf-8"),
        "styles/globals.css"    => (PORTAL_AEOR_GLOBALS_CSS,    "text/css; charset=utf-8"),
        "styles/components.css" => (PORTAL_AEOR_COMPONENTS_CSS, "text/css; charset=utf-8"),

        _ => {
            // Dev-mode disk fallback: read straight off the symlinked
            // aeor-web-components checkout so we can hot-edit during
            // development without rebuilding. Release builds skip this
            // and 404 unknown paths.
            #[cfg(debug_assertions)]
            {
                if let Some(body) = read_aeor_from_disk(&path) {
                    let ct = guess_content_type(&path);
                    return (StatusCode::OK, [(header::CONTENT_TYPE, ct)], body).into_response();
                }
            }
            return (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "text/plain")], "Not found").into_response();
        }
    };

    (StatusCode::OK, [(header::CONTENT_TYPE, content_type)], content).into_response()
}

/// Dev-only: read a file from the on-disk `aeor-web-components` symlink so
/// the engine doesn't need a rebuild for every component edit during
/// development. Guarded against path-traversal by rejecting `..` segments
/// and absolute paths.
#[cfg(debug_assertions)]
fn read_aeor_from_disk(path: &str) -> Option<String> {
    if path.contains("..") || path.starts_with('/') {
        return None;
    }
    // The directory `aeordb-lib/src/portal/aeor` is a symlink to the
    // aeor-web-components checkout — same layout as `portal/shared`.
    let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/portal/aeor");
    let full = base.join(path);
    // Re-verify after join: the resolved path must still live under base.
    let canonical_base = std::fs::canonicalize(&base).ok()?;
    let canonical_full = std::fs::canonicalize(&full).ok()?;
    if !canonical_full.starts_with(&canonical_base) {
        return None;
    }
    std::fs::read_to_string(&canonical_full).ok()
}

#[cfg(debug_assertions)]
fn guess_content_type(path: &str) -> &'static str {
    if path.ends_with(".js") || path.ends_with(".mjs") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".json") {
        "application/json; charset=utf-8"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "application/octet-stream"
    }
}

// ── Enhanced stats response types ───────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct EnhancedStats {
    pub identity: StatsIdentity,
    pub counts: StatsCounts,
    pub sizes: StatsSizes,
    pub throughput: StatsThroughput,
    pub health: StatsHealth,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsIdentity {
    pub version: String,
    pub database_path: String,
    pub hash_algorithm: String,
    pub chunk_size: usize,
    pub node_id: u64,
    pub uptime_seconds: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsCounts {
    pub files: u64,
    pub directories: u64,
    pub symlinks: u64,
    pub chunks: u64,
    pub snapshots: u64,
    pub forks: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsSizes {
    pub disk_total: u64,
    pub kv_file: u64,
    pub logical_data: u64,
    pub chunk_data: u64,
    pub void_space: u64,
    pub dedup_savings: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsThroughput {
    pub writes_per_sec: RateSnapshot,
    pub reads_per_sec: RateSnapshot,
    pub bytes_written_per_sec: RateSnapshot,
    pub bytes_read_per_sec: RateSnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsHealth {
    pub disk_usage_percent: f64,
    pub dedup_hit_rate: f64,
    pub write_buffer_depth: u64,
}

/// Return O(1) database stats as JSON using atomic counters and rate trackers.
///
/// Replaces the old `engine.stats()` call which performed an O(n) KV scan.
/// All data now comes from:
/// - `EngineCounters::snapshot()` — O(1) atomic reads
/// - `RateTrackerSet::snapshot()` — O(window_size) but bounded at 900 samples
/// - `check_disk()` — single `statvfs` syscall
/// - `std::fs::metadata()` — single `stat` syscall
pub async fn get_stats(
    State(state): State<AppState>,
    claims: Option<Extension<crate::auth::TokenClaims>>,
    rate_ext: Option<Extension<Arc<crate::engine::rate_tracker::RateTrackerSet>>>,
    db_path_ext: Option<Extension<String>>,
) -> axum::response::Response {
    // Block share tokens from accessing stats
    if let Some(Extension(ref c)) = claims {
        if c.sub.starts_with("share:") {
            return (StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "Not available for share links"}))).into_response();
        }
    }
    get_stats_inner(state, rate_ext, db_path_ext).into_response()
}

fn get_stats_inner(
    state: AppState,
    rate_ext: Option<Extension<Arc<crate::engine::rate_tracker::RateTrackerSet>>>,
    db_path_ext: Option<Extension<String>>,
) -> Json<EnhancedStats> {
    // O(1) counter snapshot from atomics
    let counters = state.engine.counters().snapshot();

    // Uptime from monotonic Instant (immune to wall-clock drift)
    let uptime_seconds = state.startup_instant.elapsed().as_secs();

    // Hash algorithm name from engine
    let hash_algorithm = format!("{:?}", state.engine.hash_algo());

    // Database file size: single stat() call
    let db_path_string = db_path_ext.map(|Extension(p)| p).unwrap_or_else(|| state.db_path.clone());
    let db_path = &db_path_string;
    let disk_total = std::fs::metadata(db_path)
        .map(|m| m.len())
        .unwrap_or(0);

    // KV file size: single stat() call
    let kv_path = format!("{}.kv", db_path);
    let kv_file = std::fs::metadata(&kv_path)
        .map(|m| m.len())
        .unwrap_or(0);

    // Dedup savings
    let dedup_savings = counters.logical_data_size
        .saturating_sub(counters.chunk_data_size);

    // Throughput from rate trackers (zero-rate fallback if not wired up)
    // Try AppState first, then Extension layer, then zero fallback
    let ext_trackers = rate_ext.map(|Extension(t)| t);
    let tracker_ref = state.rate_trackers.as_ref().or(ext_trackers.as_ref());
    let throughput = match tracker_ref {
        Some(trackers) => {
            let rate_snapshot = trackers.snapshot();
            StatsThroughput {
                writes_per_sec: rate_snapshot.writes,
                reads_per_sec: rate_snapshot.reads,
                bytes_written_per_sec: rate_snapshot.bytes_written,
                bytes_read_per_sec: rate_snapshot.bytes_read,
            }
        }
        None => {
            let zero = RateSnapshot {
                rate_1m: 0.0,
                rate_5m: 0.0,
                rate_15m: 0.0,
                peak_1m: 0.0,
            };
            StatsThroughput {
                writes_per_sec: zero.clone(),
                reads_per_sec: zero.clone(),
                bytes_written_per_sec: zero.clone(),
                bytes_read_per_sec: zero,
            }
        }
    };

    // Disk health: single statvfs call
    let disk_health = check_disk(db_path);

    // Dedup hit rate: chunks_deduped / (chunks + chunks_deduped)
    let total_chunk_operations = counters.chunks + counters.chunks_deduped_total;
    let dedup_hit_rate = if total_chunk_operations > 0 {
        counters.chunks_deduped_total as f64 / total_chunk_operations as f64
    } else {
        0.0
    };

    // SECURITY: Only expose the database filename, not the full absolute path.
    // The full path leaks server filesystem layout to authenticated users.
    let db_filename = std::path::Path::new(db_path)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("unknown")
        .to_string();

    let stats = EnhancedStats {
        identity: StatsIdentity {
            version: env!("CARGO_PKG_VERSION").to_string(),
            database_path: db_filename,
            hash_algorithm,
            chunk_size: DEFAULT_CHUNK_SIZE,
            node_id: 1,
            uptime_seconds,
        },
        counts: StatsCounts {
            files: counters.files,
            directories: counters.directories,
            symlinks: counters.symlinks,
            chunks: counters.chunks,
            snapshots: counters.snapshots,
            forks: counters.forks,
        },
        sizes: StatsSizes {
            disk_total,
            kv_file,
            logical_data: counters.logical_data_size,
            chunk_data: counters.chunk_data_size,
            void_space: counters.void_space,
            dedup_savings,
        },
        throughput,
        health: StatsHealth {
            disk_usage_percent: disk_health.usage_percent,
            dedup_hit_rate,
            write_buffer_depth: counters.write_buffer_depth,
        },
    };

    Json(stats)
}

