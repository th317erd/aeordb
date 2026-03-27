use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Error returned when a rate limit is exceeded.
#[derive(Debug, Clone)]
pub struct RateLimitError {
  pub retry_after_seconds: u64,
}

impl std::fmt::Display for RateLimitError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(
      formatter,
      "Rate limit exceeded. Retry after {} seconds.",
      self.retry_after_seconds
    )
  }
}

impl std::error::Error for RateLimitError {}

/// Sliding-window rate limiter that tracks request timestamps per key.
#[derive(Debug, Clone)]
pub struct RateLimiter {
  inner: Arc<Mutex<RateLimiterInner>>,
  max_requests: u64,
  window_seconds: u64,
}

#[derive(Debug)]
struct RateLimiterInner {
  windows: HashMap<String, Vec<Instant>>,
}

impl RateLimiter {
  /// Create a new rate limiter with the given limits.
  pub fn new(max_requests: u64, window_seconds: u64) -> Self {
    Self {
      inner: Arc::new(Mutex::new(RateLimiterInner {
        windows: HashMap::new(),
      })),
      max_requests,
      window_seconds,
    }
  }

  /// Create a rate limiter with default settings (5 requests per 60 seconds).
  pub fn default_config() -> Self {
    Self::new(5, 60)
  }

  /// Check whether a request from the given key is allowed.
  ///
  /// Returns Ok(()) if allowed, Err(RateLimitError) if the limit is exceeded.
  pub fn check_rate_limit(&self, key: &str) -> Result<(), RateLimitError> {
    let mut inner = self.inner.lock().expect("rate limiter lock poisoned");
    let now = Instant::now();
    let window_duration = std::time::Duration::from_secs(self.window_seconds);

    let timestamps = inner
      .windows
      .entry(key.to_string())
      .or_default();

    // Remove expired entries.
    timestamps.retain(|timestamp| now.duration_since(*timestamp) < window_duration);

    if timestamps.len() as u64 >= self.max_requests {
      let oldest = timestamps.first().expect("timestamps not empty");
      let elapsed = now.duration_since(*oldest);
      let retry_after = self.window_seconds.saturating_sub(elapsed.as_secs());
      return Err(RateLimitError {
        retry_after_seconds: retry_after.max(1),
      });
    }

    timestamps.push(now);
    Ok(())
  }

  /// Reset the rate limiter state for a given key (useful for testing).
  pub fn reset(&self, key: &str) {
    let mut inner = self.inner.lock().expect("rate limiter lock poisoned");
    inner.windows.remove(key);
  }

  /// Reset all rate limiter state.
  pub fn reset_all(&self) {
    let mut inner = self.inner.lock().expect("rate limiter lock poisoned");
    inner.windows.clear();
  }
}
