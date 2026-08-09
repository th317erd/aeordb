use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::engine::errors::EngineError;

use super::{
  preserve_reindex_partial_on_terminal, reindex_error_requires_immediate_failure, run_task_scheduler, MaintenanceRunConfiguration,
  ReindexFailureSummary, TaskSchedulerTiming,
};

#[derive(Default)]
struct DispatchProbe {
  active: AtomicUsize,
  maximum_active: AtomicUsize,
  started: AtomicUsize,
  permits: Mutex<usize>,
  wake: Condvar,
}

impl DispatchProbe {
  fn run(&self, cancel: CancellationToken) -> crate::engine::errors::EngineResult<bool> {
    let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
    self.maximum_active.fetch_max(active, Ordering::SeqCst);
    self.started.fetch_add(1, Ordering::SeqCst);

    let mut permits = self.permits.lock().expect("dispatch probe lock");
    while *permits == 0 && !cancel.is_cancelled() {
      let (next, _) = self.wake.wait_timeout(permits, Duration::from_millis(5)).expect("dispatch probe wait");
      permits = next;
    }
    if *permits > 0 {
      *permits -= 1;
    }
    drop(permits);
    self.active.fetch_sub(1, Ordering::SeqCst);
    Ok(true)
  }

  fn release(&self, count: usize) {
    *self.permits.lock().expect("dispatch probe lock") += count;
    self.wake.notify_all();
  }
}

async fn wait_for_count(counter: &AtomicUsize, expected: usize) {
  tokio::time::timeout(Duration::from_secs(2), async {
    while counter.load(Ordering::SeqCst) < expected {
      tokio::time::sleep(Duration::from_millis(1)).await;
    }
  })
  .await
  .expect("scheduler did not reach the expected dispatch count");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduler_applies_new_caps_only_to_future_dispatch_and_drains_on_cancel() {
  let generation = Arc::new(AtomicU64::new(1));
  let limit = Arc::new(AtomicUsize::new(2));
  let probe = Arc::new(DispatchProbe::default());
  let cancel = CancellationToken::new();

  let capture_generation = Arc::clone(&generation);
  let capture_limit = Arc::clone(&limit);
  let runner_probe = Arc::clone(&probe);
  let handle = tokio::spawn(run_task_scheduler(
    cancel.clone(),
    move || {
      Ok(MaintenanceRunConfiguration {
        generation: capture_generation.load(Ordering::SeqCst),
        max_concurrent_tasks: capture_limit.load(Ordering::SeqCst),
      })
    },
    Arc::new(move |_configuration, worker_cancel| runner_probe.run(worker_cancel)),
    TaskSchedulerTiming::TEST,
  ));

  wait_for_count(&probe.started, 2).await;
  assert_eq!(probe.active.load(Ordering::SeqCst), 2);

  generation.store(2, Ordering::SeqCst);
  limit.store(1, Ordering::SeqCst);
  probe.release(1);
  tokio::time::sleep(Duration::from_millis(20)).await;
  assert_eq!(probe.started.load(Ordering::SeqCst), 2, "a decreased cap must drain old workers before dispatching another");

  probe.release(1);
  wait_for_count(&probe.started, 3).await;
  assert_eq!(probe.active.load(Ordering::SeqCst), 1);

  generation.store(3, Ordering::SeqCst);
  limit.store(3, Ordering::SeqCst);
  wait_for_count(&probe.started, 5).await;
  assert_eq!(probe.active.load(Ordering::SeqCst), 3, "an increased cap must apply to future dispatches without restarting the worker");
  assert_eq!(probe.maximum_active.load(Ordering::SeqCst), 3);

  cancel.cancel();
  probe.wake.notify_all();
  tokio::time::timeout(Duration::from_secs(2), handle).await.expect("scheduler cancellation must drain admitted workers").unwrap();
  assert_eq!(probe.active.load(Ordering::SeqCst), 0);
  assert_eq!(probe.started.load(Ordering::SeqCst), 5, "shutdown must not admit more workers after cancellation");
}

#[test]
fn reindex_never_downgrades_engine_authority_failures_to_file_level_damage() {
  let immediate = [
    EngineError::IoError(std::io::Error::other("disk failed")),
    EngineError::InvalidMagic,
    EngineError::InvalidHashAlgorithm(99),
    EngineError::PartialOperation { operation: "nested".into(), completed: 1, failed: 1, evidence: "partial".into() },
    EngineError::SystemFamilyPolicy { code: "policy", reason: "authority unavailable".into() },
    EngineError::DurabilityFailure("barrier failed".into()),
    EngineError::PostMutationDurabilityFailure("publication failed".into()),
    EngineError::ShuttingDown,
    EngineError::Cancelled("reindex".into()),
  ];
  for error in &immediate {
    assert!(reindex_error_requires_immediate_failure(error), "reindex would downgrade serious failure: {error}");
  }

  let collectable = [
    EngineError::CorruptEntry { offset: 4, reason: "bad file record".into() },
    EngineError::UnexpectedEof,
    EngineError::InvalidEntryVersion(9),
    EngineError::NotFound("concurrently removed".into()),
    EngineError::JsonParseError("bad document".into()),
  ];
  for error in &collectable {
    assert!(!reindex_error_requires_immediate_failure(error), "reindex would abort instead of retaining exact file evidence: {error}");
  }
}

#[test]
fn reindex_mixed_terminal_failure_retains_prior_exact_partial_evidence() {
  let first = EngineError::CorruptEntry { offset: 4, reason: "bad file record".into() };
  let mut failures = ReindexFailureSummary::default();
  failures.record("migration", "/docs/a.json", &first);
  let error = failures.into_terminal_error(2, "index-authority", "/docs/d.json", EngineError::DurabilityFailure("barrier failed".into()));
  let EngineError::PartialOperation { operation, completed, failed, evidence } = error else {
    panic!("a mixed terminal failure must retain partial-operation evidence");
  };
  assert_eq!(operation, "reindex");
  assert_eq!(completed, 2);
  assert_eq!(failed, 2);
  assert!(evidence.contains("migration /docs/a.json"));
  assert!(evidence.contains("index-authority /docs/d.json"));
  assert!(evidence.contains("Durability failure: barrier failed"));

  let first_failure = ReindexFailureSummary::default().into_terminal_error(
    0,
    "index-authority",
    "/docs/a.json",
    EngineError::DurabilityFailure("first barrier failed".into()),
  );
  assert!(matches!(first_failure, EngineError::DurabilityFailure(_)), "the first serious failure must retain its original type");
}

#[test]
fn reindex_flush_and_checkpoint_failures_cannot_erase_prior_partial_evidence() {
  let first = EngineError::CorruptEntry { offset: 4, reason: "bad file record".into() };
  let mut failures = ReindexFailureSummary::default();
  failures.record("migration", "/docs/a.json", &first);

  let error = preserve_reindex_partial_on_terminal::<()>(
    Err(EngineError::PostMutationDurabilityFailure("checkpoint publication failed".into())),
    &failures,
    2,
    "checkpoint",
    "/docs/b.json",
  )
  .unwrap_err();
  let EngineError::PartialOperation { completed, failed, evidence, .. } = error else {
    panic!("a later checkpoint failure must retain prior partial evidence");
  };
  assert_eq!(completed, 2);
  assert_eq!(failed, 2);
  assert!(evidence.contains("migration /docs/a.json"));
  assert!(evidence.contains("checkpoint /docs/b.json"));
  assert!(evidence.contains("Post-mutation durability failure: checkpoint publication failed"));

  let value = preserve_reindex_partial_on_terminal(Ok(17_u8), &failures, 2, "checkpoint", "/docs/b.json").unwrap();
  assert_eq!(value, 17, "successful authority work must not alter the accumulated partial outcome");
}
