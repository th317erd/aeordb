use std::sync::{Arc, Mutex};

use serial_test::serial;

use super::*;
use crate::engine::errors::EngineError;

fn soft_failure_count(rendered: &str, operation: &str) -> f64 {
  rendered
    .lines()
    .find_map(|line| {
      if !line.starts_with("aeordb_system_soft_failures_total")
        || !line.contains("subsystem=\"index_cleanup\"")
        || !line.contains(&format!("operation=\"{operation}\""))
      {
        return None;
      }
      line.split_whitespace().last()?.parse().ok()
    })
    .unwrap_or(0.0)
}

#[test]
fn queue_reports_that_the_cleanup_worker_is_unavailable() {
  let (tx, rx) = mpsc::unbounded_channel();
  drop(rx);
  let sender = IndexCleanupSender { tx };

  let error = sender.queue("/docs/retired.txt".to_string()).unwrap_err();

  assert_eq!(error.to_string(), "index cleanup worker is unavailable");
}

#[test]
#[serial]
fn batch_cleanup_continues_after_each_failure_and_records_visible_evidence() {
  let metrics = crate::metrics::initialize_metrics();
  let before = soft_failure_count(&metrics.render(), "path_removal");
  let visited = Arc::new(Mutex::new(Vec::new()));
  let visited_by_removal = Arc::clone(&visited);
  let paths = vec!["/one.json".to_string(), "/two.json".to_string(), "/three.json".to_string()];

  let outcome = process_batch_with(&paths, move |path| {
    visited_by_removal.lock().unwrap().push(path.to_string());
    if path == "/two.json" {
      return Ok(2);
    }
    Err(EngineError::InvalidInput(format!("injected cleanup failure for {path}")))
  });

  assert_eq!(*visited.lock().unwrap(), paths);
  assert_eq!(outcome.attempted_paths, 3);
  assert_eq!(outcome.failed_paths, 2);
  assert!(soft_failure_count(&metrics.render(), "path_removal") >= before + 2.0);
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn worker_panics_are_recorded_without_terminating_the_cleanup_loop() {
  let metrics = crate::metrics::initialize_metrics();
  let before = soft_failure_count(&metrics.render(), "worker_join");
  let join_error = tokio::task::spawn_blocking(|| panic!("injected index cleanup panic")).await.unwrap_err();

  record_worker_join_failure(7, join_error);

  assert!(soft_failure_count(&metrics.render(), "worker_join") >= before + 1.0);
}
