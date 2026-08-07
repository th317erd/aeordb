use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::{run_task_scheduler, MaintenanceRunConfiguration, TaskSchedulerTiming};

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
