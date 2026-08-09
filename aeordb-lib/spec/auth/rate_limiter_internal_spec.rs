use std::panic::{AssertUnwindSafe, catch_unwind};

use super::*;

fn poison(limiter: &RateLimiter) {
  let inner = limiter.inner.clone();
  let _ = catch_unwind(AssertUnwindSafe(move || {
    let _guard = inner.lock().unwrap();
    panic!("intentional rate-limiter poison");
  }));
}

#[test]
fn poisoned_state_is_reported_instead_of_panicking_request_or_reset_paths() {
  let limiter = RateLimiter::new(1, 60);
  poison(&limiter);

  let check = catch_unwind(AssertUnwindSafe(|| limiter.check_rate_limit("caller")));
  let reset = catch_unwind(AssertUnwindSafe(|| limiter.reset("caller")));
  let reset_all = catch_unwind(AssertUnwindSafe(|| limiter.reset_all()));

  assert!(check.is_ok(), "request admission must not panic after limiter poison");
  assert!(reset.is_ok(), "single-key reset must not panic after limiter poison");
  assert!(reset_all.is_ok(), "global reset must not panic after limiter poison");
  assert_eq!(check.unwrap(), Err(RateLimitError::StateUnavailable));
  assert_eq!(reset.unwrap(), Err(RateLimitError::StateUnavailable));
  assert_eq!(reset_all.unwrap(), Err(RateLimitError::StateUnavailable));
}

#[test]
fn zero_request_configuration_denies_without_reaching_an_empty_window_invariant() {
  let limiter = RateLimiter::new(0, 60);

  assert_eq!(limiter.check_rate_limit("caller"), Err(RateLimitError::LimitExceeded { retry_after_seconds: 60 }));
}
