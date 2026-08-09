use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::engine::directory_ops::{BufferedFileTransform, DirectoryOps};
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::event_bus::EventBus;
use crate::engine::namespace_mutation::NamespaceMutationKind;
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::task_queue::{TaskQueue, TaskStatus};

const CRON_CONFIG_PATH: &str = "/.aeordb-config/cron.json";
const CRON_CONFIG_MAX_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronSchedule {
  pub id: String,
  pub task_type: String,
  pub schedule: String,
  pub args: serde_json::Value,
  #[serde(default = "default_enabled")]
  pub enabled: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CronScheduleUpdate {
  pub enabled: Option<bool>,
  pub schedule: Option<String>,
  pub task_type: Option<String>,
  pub args: Option<serde_json::Value>,
}

fn default_enabled() -> bool {
  true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CronConfig {
  pub schedules: Vec<CronSchedule>,
}

/// Load cron config from `/.aeordb-config/cron.json` in the engine.
/// A truly absent file is an empty schedule; unreadable, malformed, duplicate,
/// or semantically invalid persisted authority is an error.
pub fn load_cron_config(engine: &StorageEngine) -> EngineResult<Vec<CronSchedule>> {
  let ops = DirectoryOps::new(engine);
  match ops.read_file_buffered_bounded(CRON_CONFIG_PATH, CRON_CONFIG_MAX_BYTES) {
    Ok(data) => decode_cron_config(&data).map(|config| config.schedules),
    Err(EngineError::NotFound(_)) => Ok(Vec::new()),
    Err(error) => Err(error),
  }
}

/// Save cron config to `/.aeordb-config/cron.json` in the engine.
pub fn save_cron_config(engine: &StorageEngine, config: &CronConfig) -> EngineResult<()> {
  validate_cron_config(config)?;
  let ops = DirectoryOps::new(engine);
  let ctx = RequestContext::system();
  let data = serde_json::to_vec_pretty(config).map_err(|e| EngineError::InvalidInput(format!("serialization error: {e}")))?;
  ops.transform_file_buffered(
    &ctx,
    CRON_CONFIG_PATH,
    Some("application/json"),
    CRON_CONFIG_MAX_BYTES,
    NamespaceMutationKind::SystemWrite,
    move |_| Ok(BufferedFileTransform::Replace { data, output: () }),
  )
}

/// Atomically read, mutate, validate, and replace cron configuration.
///
/// The callback runs while namespace authority is held, so concurrent embedded
/// or HTTP mutations cannot overwrite schedules observed by another writer.
fn mutate_cron_config<T, F>(engine: &StorageEngine, mutate: F) -> EngineResult<T>
where
  F: FnOnce(&mut Vec<CronSchedule>) -> EngineResult<T>,
{
  let ops = DirectoryOps::new(engine);
  let ctx = RequestContext::system();
  ops.transform_file_buffered(
    &ctx,
    CRON_CONFIG_PATH,
    Some("application/json"),
    CRON_CONFIG_MAX_BYTES,
    NamespaceMutationKind::SystemWrite,
    move |existing| {
      let mut config = existing.map(decode_cron_config).transpose()?.unwrap_or_default();
      let output = mutate(&mut config.schedules)?;
      validate_cron_config(&config)?;
      let data = serde_json::to_vec_pretty(&config)
        .map_err(|error| EngineError::InvalidInput(format!("cron config serialization error: {error}")))?;
      Ok(BufferedFileTransform::Replace { data, output })
    },
  )
}

/// Create one schedule without exposing the namespace-locked transform callback.
pub fn create_cron_schedule(engine: &StorageEngine, schedule: CronSchedule) -> EngineResult<CronSchedule> {
  mutate_cron_config(engine, move |schedules| {
    if schedules.iter().any(|existing| existing.id == schedule.id) {
      return Err(EngineError::AlreadyExists(schedule.id.clone()));
    }
    schedules.push(schedule.clone());
    Ok(schedule)
  })
}

/// Update one schedule atomically with concurrent cron mutations.
pub fn update_cron_schedule(engine: &StorageEngine, id: &str, update: CronScheduleUpdate) -> EngineResult<CronSchedule> {
  let requested_id = id.to_string();
  mutate_cron_config(engine, move |schedules| {
    let schedule =
      schedules.iter_mut().find(|schedule| schedule.id == requested_id).ok_or_else(|| EngineError::NotFound(requested_id.clone()))?;
    if let Some(enabled) = update.enabled {
      schedule.enabled = enabled;
    }
    if let Some(expression) = update.schedule {
      schedule.schedule = expression;
    }
    if let Some(task_type) = update.task_type {
      schedule.task_type = task_type;
    }
    if let Some(args) = update.args {
      schedule.args = args;
    }
    Ok(schedule.clone())
  })
}

/// Delete one schedule atomically with concurrent cron mutations.
pub fn delete_cron_schedule(engine: &StorageEngine, id: &str) -> EngineResult<()> {
  let requested_id = id.to_string();
  mutate_cron_config(engine, move |schedules| {
    let original_len = schedules.len();
    schedules.retain(|schedule| schedule.id != requested_id);
    if schedules.len() == original_len {
      return Err(EngineError::NotFound(requested_id));
    }
    Ok(())
  })
}

fn decode_cron_config(data: &[u8]) -> EngineResult<CronConfig> {
  let config: CronConfig = serde_json::from_slice(data)
    .map_err(|error| EngineError::JsonParseError(format!("cron config at {CRON_CONFIG_PATH} is malformed: {error}")))?;
  validate_cron_config(&config)?;
  Ok(config)
}

fn validate_cron_config(config: &CronConfig) -> EngineResult<()> {
  let mut ids = HashSet::with_capacity(config.schedules.len());
  for schedule in &config.schedules {
    if !ids.insert(schedule.id.as_str()) {
      return Err(EngineError::InvalidInput(format!("cron config contains duplicate schedule id '{}'", schedule.id)));
    }
    validate_cron_expression(&schedule.schedule)
      .map_err(|error| EngineError::InvalidInput(format!("invalid cron expression for schedule '{}': {error}", schedule.id)))?;
  }
  Ok(())
}

/// Seed default cron schedules if no config file exists yet. Idempotent —
/// if the file already exists (even an empty `schedules: []`), this is a no-op,
/// so users can disable defaults without them being re-added on restart.
pub fn seed_default_cron_if_missing(engine: &StorageEngine) -> EngineResult<bool> {
  let ops = DirectoryOps::new(engine);
  let ctx = RequestContext::system();
  let seeded = ops.transform_file_buffered(
    &ctx,
    CRON_CONFIG_PATH,
    Some("application/json"),
    CRON_CONFIG_MAX_BYTES,
    NamespaceMutationKind::SystemWrite,
    |existing| {
      if existing.is_some() {
        return Ok(BufferedFileTransform::Keep(false));
      }
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
      validate_cron_config(&defaults)?;
      let data = serde_json::to_vec_pretty(&defaults)
        .map_err(|error| EngineError::InvalidInput(format!("default cron config serialization error: {error}")))?;
      Ok(BufferedFileTransform::Replace { data, output: true })
    },
  )?;
  if seeded {
    tracing::info!("Seeded default cron schedules: hourly cleanup, daily 03:00 GC");
  }
  Ok(seeded)
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
    let converted_range = if range_part == "*" { "*".to_string() } else { convert_dow_simple(range_part) };
    return format!("{}/{}", converted_range, step);
  }

  convert_dow_simple(field)
}

/// Convert simple DOW values: single numbers, ranges ("0-5"), lists ("0,3,5").
fn convert_dow_simple(field: &str) -> String {
  // List: "0,3,5"
  if field.contains(',') {
    return field.split(',').map(convert_dow_simple).collect::<Vec<_>>().join(",");
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
  cron::Schedule::from_str(&six_field).map(|_| ()).map_err(|e| e.to_string())
}

/// Check if a 5-field cron expression matches the current minute.
/// Converts to a 6-field expression for the `cron` crate, then checks if
/// any occurrence falls within the current minute window.
pub fn cron_matches_now(expression: &str) -> bool {
  cron_matches_now_checked(expression).unwrap_or(false)
}

fn cron_matches_now_checked(expression: &str) -> Result<bool, String> {
  use chrono::Timelike;

  let six_field = to_cron_crate_expression(expression);
  let schedule = cron::Schedule::from_str(&six_field).map_err(|error| error.to_string())?;

  // Build one second before the start of the current minute so that
  // `after()` (which is exclusive) will include second-0 of this minute.
  let now = chrono::Utc::now();
  let start_of_minute = now.with_second(0).and_then(|t| t.with_nanosecond(0)).unwrap_or(now) - chrono::Duration::seconds(1);

  // Ask: is the next occurrence after (start_of_minute - 1s) within this minute?
  Ok(match schedule.after(&start_of_minute).take(1).next() {
    Some(next) => {
      let diff = next.signed_duration_since(start_of_minute);
      // The occurrence should be at second 0 of this minute (diff == 0)
      // or within the 60-second window of this minute.
      diff.num_seconds() >= 0 && diff.num_seconds() < 60
    }
    None => false,
  })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CronTickResult {
  pub schedules_checked: usize,
  pub tasks_enqueued: usize,
  pub tasks_deduplicated: usize,
}

/// Execute one scheduler tick with explicit failure semantics.
///
/// A config or task-registry failure occurs before enqueue. If a later enqueue
/// fails after earlier due schedules succeeded, those tasks remain durable and
/// the next tick deduplicates them before retrying the remainder.
pub fn run_cron_tick(queue: &TaskQueue, engine: &StorageEngine) -> EngineResult<CronTickResult> {
  let schedules = load_cron_config(engine)?;
  let mut due = Vec::new();
  let mut result = CronTickResult::default();
  for schedule in schedules {
    if !schedule.enabled {
      continue;
    }
    result.schedules_checked = result
      .schedules_checked
      .checked_add(1)
      .ok_or_else(|| EngineError::ResourceExhausted("cron checked-schedule counter overflow".to_string()))?;
    if cron_matches_now_checked(&schedule.schedule)
      .map_err(|error| EngineError::InvalidInput(format!("invalid cron expression for schedule '{}': {error}", schedule.id)))?
    {
      due.push(schedule);
    }
  }
  if due.is_empty() {
    return Ok(result);
  }

  let mut tasks = queue.list_tasks()?;
  for schedule in due {
    let dominated = tasks.iter().any(|task| {
      (task.status == TaskStatus::Pending || task.status == TaskStatus::Running)
        && task.task_type == schedule.task_type
        && task.args == schedule.args
    });
    if dominated {
      result.tasks_deduplicated = result
        .tasks_deduplicated
        .checked_add(1)
        .ok_or_else(|| EngineError::ResourceExhausted("cron deduplicated-task counter overflow".to_string()))?;
      continue;
    }

    tasks.push(queue.enqueue(&schedule.task_type, schedule.args)?);
    result.tasks_enqueued = result
      .tasks_enqueued
      .checked_add(1)
      .ok_or_else(|| EngineError::ResourceExhausted("cron enqueued-task counter overflow".to_string()))?;
  }
  Ok(result)
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

      if let Err(error) = run_cron_tick(&queue, &engine) {
        tracing::error!(%error, "Cron scheduler tick failed; due work was not reported as successful");
      }
    }
  })
}
