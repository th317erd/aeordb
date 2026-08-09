use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Error returned when request admission is denied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitError {
  LimitExceeded { retry_after_seconds: u64 },
  StateUnavailable,
}

impl std::fmt::Display for RateLimitError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::LimitExceeded { retry_after_seconds } => write!(formatter, "Rate limit exceeded. Retry after {retry_after_seconds} seconds."),
      Self::StateUnavailable => write!(formatter, "Rate limiter state is unavailable."),
    }
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
    Self { inner: Arc::new(Mutex::new(RateLimiterInner { windows: HashMap::new() })), max_requests, window_seconds }
  }

  /// Create a rate limiter with default settings (30 requests per 60 seconds).
  ///
  /// Note: this is used by BOTH the magic-link request endpoint and the
  /// auth-token exchange endpoint. 30/60s gives legitimate users plenty of
  /// retries on auth/token (where argon2 is the real cost) while still
  /// bounding spam on magic-link emails. The previous default of 5/60s
  /// locked out legitimate batch flows.
  pub fn default_config() -> Self {
    Self::new(30, 60)
  }

  /// Check whether a request from the given key is allowed.
  ///
  /// Returns `Ok(())` if allowed, or a typed denial when the limit is exceeded
  /// or its state cannot be trusted.
  pub fn check_rate_limit(&self, key: &str) -> Result<(), RateLimitError> {
    let mut inner = self.inner.lock().map_err(|_| RateLimitError::StateUnavailable)?;
    let now = Instant::now();
    let window_duration = std::time::Duration::from_secs(self.window_seconds);

    if self.max_requests == 0 {
      return Err(RateLimitError::LimitExceeded { retry_after_seconds: self.window_seconds.max(1) });
    }

    // M4: Evict oldest entries when the HashMap grows too large to prevent
    // unbounded memory growth from unique keys (e.g. IP-based rate limiting).
    if inner.windows.len() > 100_000 {
      let mut entries: Vec<_> = inner.windows.iter().map(|(k, v)| (k.clone(), v.last().copied())).collect();
      entries.sort_by_key(|(_, t)| *t);
      for (key, _) in entries.iter().take(10_000) {
        inner.windows.remove(key);
      }
    }

    let timestamps = inner.windows.entry(key.to_string()).or_default();

    // Remove expired entries.
    timestamps.retain(|timestamp| now.duration_since(*timestamp) < window_duration);

    if timestamps.len() as u64 >= self.max_requests {
      let oldest = timestamps.first().ok_or(RateLimitError::StateUnavailable)?;
      let elapsed = now.duration_since(*oldest);
      let retry_after = self.window_seconds.saturating_sub(elapsed.as_secs());
      return Err(RateLimitError::LimitExceeded { retry_after_seconds: retry_after.max(1) });
    }

    timestamps.push(now);
    Ok(())
  }

  /// Reset the rate limiter state for a given key (useful for testing).
  pub fn reset(&self, key: &str) -> Result<(), RateLimitError> {
    let mut inner = self.inner.lock().map_err(|_| RateLimitError::StateUnavailable)?;
    inner.windows.remove(key);
    Ok(())
  }

  /// Reset all rate limiter state.
  pub fn reset_all(&self) -> Result<(), RateLimitError> {
    let mut inner = self.inner.lock().map_err(|_| RateLimitError::StateUnavailable)?;
    inner.windows.clear();
    Ok(())
  }
}

#[cfg(test)]
#[path = "../../spec/auth/rate_limiter_internal_spec.rs"]
mod rate_limiter_internal_spec;
