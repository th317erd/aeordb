use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::event_bus::EventBus;
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::task_queue::{TaskQueue, TaskStatus};

const CRON_CONFIG_PATH: &str = "/.aeordb-config/cron.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronSchedule {
    pub id: String,
    pub task_type: String,
    pub schedule: String,
    pub args: serde_json::Value,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronConfig {
    pub schedules: Vec<CronSchedule>,
}

/// Load cron config from `/.aeordb-config/cron.json` in the engine.
/// Returns empty vec if the file is not found or cannot be parsed.
pub fn load_cron_config(engine: &StorageEngine) -> Vec<CronSchedule> {
    let ops = DirectoryOps::new(engine);
    match ops.read_file(CRON_CONFIG_PATH) {
        Ok(data) => match serde_json::from_slice::<CronConfig>(&data) {
            Ok(config) => config.schedules,
            Err(e) => {
                tracing::warn!("Failed to parse /.aeordb-config/cron.json: {} — schedules disabled", e);
                Vec::new()
            }
        },
        Err(_) => Vec::new(),
    }
}

/// Save cron config to `/.aeordb-config/cron.json` in the engine.
pub fn save_cron_config(engine: &StorageEngine, config: &CronConfig) -> EngineResult<()> {
    let ops = DirectoryOps::new(engine);
    let ctx = RequestContext::system();
    let data = serde_json::to_vec_pretty(config)
        .map_err(|e| EngineError::InvalidInput(format!("serialization error: {e}")))?;
    ops.store_file(&ctx, CRON_CONFIG_PATH, &data, Some("application/json"))?;
    Ok(())
}

/// Seed default cron schedules if no config file exists yet. Idempotent —
/// if the file already exists (even an empty `schedules: []`), this is a no-op,
/// so users can disable defaults without them being re-added on restart.
pub fn seed_default_cron_if_missing(engine: &StorageEngine) -> EngineResult<bool> {
    let ops = DirectoryOps::new(engine);
    match ops.read_file(CRON_CONFIG_PATH) {
        Ok(_) => Ok(false),
        Err(EngineError::NotFound(_)) => {
            let defaults = CronConfig {
                schedules: vec![
                    CronSchedule {
                        id: "default-cleanup".to_string(),
                        task_type: "cleanup".to_string(),
                        schedule: "0 * * * *".to_string(),
                        args: serde_json::json!({}),
                        enabled: true,
                    },
                    CronSchedule {
                        id: "default-gc".to_string(),
                        task_type: "gc".to_string(),
                        schedule: "0 3 * * *".to_string(),
                        args: serde_json::json!({"dry_run": false}),
                        enabled: true,
                    },
                ],
            };
            save_cron_config(engine, &defaults)?;
            tracing::info!("Seeded default cron schedules: hourly cleanup, daily 03:00 GC");
            Ok(true)
        }
        Err(other) => Err(other),
    }
}

/// Convert a 5-field Unix cron expression to a 6-field expression compatible
/// with the `cron` crate. The cron crate uses the format:
///   sec min hour dom month dow
/// where DOW uses 1-7 (1=SUN) or named days, not Unix's 0-6 (0=SUN).
///
/// This function:
/// 1. Prepends "0 " for the seconds field
/// 2. Translates DOW numeric `0` and `7` to `1` (SUN) since the cron crate
///    doesn't accept `0` as a valid day-of-week value
fn to_cron_crate_expression(expression: &str) -> String {
    let fields: Vec<&str> = expression.split_whitespace().collect();
    if fields.len() != 5 {
        // Return as-is with seconds prepended; let the crate produce the parse error
        return format!("0 {}", expression);
    }

    // The 5th field (index 4) is day-of-week. Convert Unix DOW (0-7) to crate DOW (1-7).
    let dow = convert_dow_field(fields[4]);
    format!("0 {} {} {} {} {}", fields[0], fields[1], fields[2], fields[3], dow)
}

/// Convert a Unix cron DOW field to the cron crate's format.
/// Unix: 0=Sun, 1=Mon, ..., 6=Sat, 7=Sun
/// Crate: 1=Sun, 2=Mon, ..., 7=Sat
/// Handles ranges (e.g., "1-5"), lists (e.g., "0,3,5"), steps (e.g., "*/2"),
/// named days, and `?`/`*` wildcards.
fn convert_dow_field(field: &str) -> String {
    // Wildcards and named days pass through unchanged
    if field == "*" || field == "?" || field.contains(char::is_alphabetic) {
        return field.to_string();
    }

    // Handle step expressions: "*/2", "0-5/2", etc.
    if let Some((range_part, step)) = field.split_once('/') {
        let converted_range = if range_part == "*" {
            "*".to_string()
        } else {
            convert_dow_simple(range_part)
        };
        return format!("{}/{}", converted_range, step);
    }

    convert_dow_simple(field)
}

/// Convert simple DOW values: single numbers, ranges ("0-5"), lists ("0,3,5").
fn convert_dow_simple(field: &str) -> String {
    // List: "0,3,5"
    if field.contains(',') {
        return field
            .split(',')
            .map(|part| convert_dow_simple(part))
            .collect::<Vec<_>>()
            .join(",");
    }

    // Range: "0-5"
    if field.contains('-') {
        let parts: Vec<&str> = field.splitn(2, '-').collect();
        if parts.len() == 2 {
            let start = shift_dow(parts[0]);
            let end = shift_dow(parts[1]);
            return format!("{}-{}", start, end);
        }
    }

    // Single number
    shift_dow(field)
}

/// Shift a single DOW number from Unix (0-7) to crate (1-7) format.
fn shift_dow(value: &str) -> String {
    match value.parse::<u32>() {
        Ok(0) | Ok(7) => "1".to_string(), // Sunday
        Ok(n) if n <= 6 => (n + 1).to_string(),
        _ => value.to_string(), // pass through if not a valid number
    }
}

/// Validate a 5-field cron expression. Returns Ok(()) if valid, Err with message if not.
pub fn validate_cron_expression(expression: &str) -> Result<(), String> {
    let six_field = to_cron_crate_expression(expression);
    cron::Schedule::from_str(&six_field)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Check if a 5-field cron expression matches the current minute.
/// Converts to a 6-field expression for the `cron` crate, then checks if
/// any occurrence falls within the current minute window.
pub fn cron_matches_now(expression: &str) -> bool {
    use chrono::Timelike;

    let six_field = to_cron_crate_expression(expression);
    let schedule = match cron::Schedule::from_str(&six_field) {
        Ok(s) => s,
        Err(_) => return false,
    };

    // Build one second before the start of the current minute so that
    // `after()` (which is exclusive) will include second-0 of this minute.
    let now = chrono::Utc::now();
    let start_of_minute = now
        .with_second(0)
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(now)
        - chrono::Duration::seconds(1);

    // Ask: is the next occurrence after (start_of_minute - 1s) within this minute?
    match schedule.after(&start_of_minute).take(1).next() {
        Some(next) => {
            let diff = next.signed_duration_since(start_of_minute);
            // The occurrence should be at second 0 of this minute (diff == 0)
            // or within the 60-second window of this minute.
            diff.num_seconds() >= 0 && diff.num_seconds() < 60
        }
        None => false,
    }
}

/// Spawn the cron scheduler loop. Runs every 60 seconds, loading the cron
/// config and enqueuing matching tasks (with deduplication).
pub fn spawn_cron_scheduler(
    queue: Arc<TaskQueue>,
    engine: Arc<StorageEngine>,
    _event_bus: Arc<EventBus>,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("Cron scheduler shutting down");
                    break;
                }
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(60)) => {}
            }

            let schedules = load_cron_config(&engine);

            for schedule in &schedules {
                if !schedule.enabled {
                    continue;
                }

                if !cron_matches_now(&schedule.schedule) {
                    continue;
                }

                // Dedup: check if a task with same type+args is already pending or running.
                let dominated = match queue.list_tasks() {
                    Ok(tasks) => tasks.iter().any(|t| {
                        (t.status == TaskStatus::Pending || t.status == TaskStatus::Running)
                            && t.task_type == schedule.task_type
                            && t.args == schedule.args
                    }),
                    Err(_) => false,
                };

                if dominated {
                    continue;
                }

                let _ = queue.enqueue(&schedule.task_type, schedule.args.clone());
            }
        }
    })
}
