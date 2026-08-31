use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::supervise_server_and_initialization;

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

#[test]
fn shutdown_drains_background_tasks_before_closing_storage_write_authority() {
  let source = include_str!("../src/commands/start.rs");
  let cancel = source.find("cancel.cancel();").expect("shutdown must cancel server and worker admission");
  let drain = source.find("wait_for_background_tasks(runtime.handles").expect("shutdown must join background workers");
  let close = source.find("runtime.engine.begin_shutdown();").expect("shutdown must eventually close storage write authority");
  let flush = source.find("runtime.engine.shutdown()").expect("shutdown must durably flush the engine");

  assert!(cancel < drain, "worker cancellation must happen before worker drain");
  assert!(drain < close, "checkpointed tasks need storage write authority until every background worker has settled");
  assert!(close < flush, "storage write authority must close before the final engine flush");
}

#[tokio::test]
async fn initialization_task_panic_cancels_and_joins_the_listener() {
  let cancellation = CancellationToken::new();
  let server_observed_cancellation = Arc::new(AtomicBool::new(false));
  let observed = server_observed_cancellation.clone();
  let server_cancellation = cancellation.clone();
  let server_task = tokio::spawn(async move {
    server_cancellation.cancelled().await;
    observed.store(true, Ordering::Release);
    Ok(())
  });
  let initialization_task = tokio::spawn(async move {
    panic!("injected initialization panic");
    #[allow(unreachable_code)]
    Ok::<(), String>(())
  });

  let (initialization_result, server_result) =
    tokio::time::timeout(TEST_TIMEOUT, supervise_server_and_initialization(initialization_task, server_task, &cancellation))
      .await
      .expect("supervision must not hang");

  assert!(initialization_result.is_err(), "the initialization panic must remain visible");
  assert!(matches!(server_result, Ok(Ok(()))), "listener must finish cleanly after cancellation");
  assert!(cancellation.is_cancelled());
  assert!(server_observed_cancellation.load(Ordering::Acquire));
}
