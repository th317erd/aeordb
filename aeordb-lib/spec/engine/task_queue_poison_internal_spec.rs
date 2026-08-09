use std::sync::Arc;

use super::*;

#[test]
fn poisoned_cancellation_authority_fails_closed_for_running_workers() {
  let (engine, _temporary) = crate::server::create_temp_engine_for_tests();
  let queue = Arc::new(TaskQueue::new(engine));
  let cancellation_state = Arc::clone(&queue.cancelled);

  let poisoner = std::thread::spawn(move || {
    let _guard = cancellation_state.write().unwrap();
    panic!("inject cancellation-state poison");
  });
  assert!(poisoner.join().is_err());

  assert!(queue.is_cancelled("not-yet-recorded"));
}
