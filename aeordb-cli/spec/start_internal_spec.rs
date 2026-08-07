use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::supervise_server_and_initialization;

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
    tokio::time::timeout(Duration::from_secs(1), supervise_server_and_initialization(initialization_task, server_task, &cancellation))
      .await
      .expect("supervision must not hang");

  assert!(initialization_result.is_err(), "the initialization panic must remain visible");
  assert!(matches!(server_result, Ok(Ok(()))), "listener must finish cleanly after cancellation");
  assert!(cancellation.is_cancelled());
  assert!(server_observed_cancellation.load(Ordering::Acquire));
}
