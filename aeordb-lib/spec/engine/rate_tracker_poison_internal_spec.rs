use std::sync::Arc;

use super::RateTracker;

#[test]
fn poisoned_rate_samples_remain_available_as_degraded_telemetry() {
  let tracker = Arc::new(RateTracker::new());
  let poison_target = Arc::clone(&tracker);
  assert!(std::thread::spawn(move || {
    let _guard = poison_target.samples.lock().unwrap();
    panic!("poison rate samples");
  })
  .join()
  .is_err());

  tracker.record(1_000, 10);
  tracker.record(2_000, 20);
  assert_eq!(tracker.sample_count(), 2);
  assert_eq!(tracker.snapshot().rate_1m, 10.0);
}
