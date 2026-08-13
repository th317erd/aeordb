use std::sync::Arc;

use aeordb::engine::gc::{execute_gc_run, GcExecutionRequestV1};
use aeordb::engine::v4::gc_run::{GcRunInvocationV1, NoopGcRunProgressSinkV1};
use aeordb::engine::{RequestContext, StorageEngine};
use tokio_util::sync::CancellationToken;

use crate::utils::format_bytes;

pub async fn run(database: &str, dry_run: bool) -> Result<(), String> {
  if dry_run {
    println!("AeorDB Garbage Collection [DRY RUN]");
  } else {
    println!("AeorDB Garbage Collection");
  }
  println!("Database: {database}");
  println!();

  let database = database.to_string();
  let cancellation = CancellationToken::new();
  let execution_cancellation = cancellation.clone();
  let mut execution = tokio::task::spawn_blocking(move || {
    let engine = StorageEngine::open(&database).map_err(|error| format!("Error opening database: {error}"))?;
    execute_gc_run(
      &engine,
      &RequestContext::system(),
      GcExecutionRequestV1::new(GcRunInvocationV1::Cli, dry_run, execution_cancellation, Arc::new(NoopGcRunProgressSinkV1)),
    )
    .map_err(|error| format!("GC failed: {error}"))
  });

  let execution = tokio::select! {
    result = &mut execution => result.map_err(|error| format!("GC worker failed: {error}"))?,
    signal = crate::commands::start::shutdown_signal() => {
      cancellation.cancel();
      signal?;
      execution.await.map_err(|error| format!("GC worker failed after cancellation: {error}"))?
    }
  }?;
  let result = execution.result;
  if result.dry_run {
    println!("[DRY RUN] Would collect {} garbage entries ({})", result.garbage_entries, format_bytes(result.reclaimed_bytes),);
  } else {
    println!("Versions scanned: {}", result.versions_scanned);
    println!("Live entries:     {}", result.live_entries);
    println!("Garbage entries:  {}", result.garbage_entries);
    println!("Reclaimed:        {}", format_bytes(result.reclaimed_bytes));
    println!("Duration:         {:.1}s", result.duration_ms as f64 / 1000.0);
  }
  for warning in &result.cleanup_warnings {
    eprintln!("Warning: {warning}");
  }
  Ok(())
}
