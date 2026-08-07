use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::engine::errors::{EngineError, EngineResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct QueryRuntimePolicy {
  pub per_request_memory_bytes: u64,
  pub global_memory_bytes: u64,
  pub position_scan_buffer_bytes: u64,
}

impl QueryRuntimePolicy {
  pub fn new(per_request_memory_bytes: u64, global_memory_bytes: u64, position_scan_buffer_bytes: u64) -> EngineResult<Self> {
    if per_request_memory_bytes == 0 || per_request_memory_bytes > global_memory_bytes {
      return Err(EngineError::InvalidInput("query per-request memory must be nonzero and no larger than global query memory".to_string()));
    }
    if position_scan_buffer_bytes == 0 {
      return Err(EngineError::InvalidInput("query position-scan buffer must be nonzero".to_string()));
    }
    Ok(Self { per_request_memory_bytes, global_memory_bytes, position_scan_buffer_bytes })
  }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct QueryRuntimeSnapshot {
  pub policy: Option<QueryRuntimePolicy>,
  pub disabled_reason: Option<String>,
  pub reserved_bytes: u64,
  pub active_requests: u64,
}

#[derive(Clone)]
struct QueryRuntimeState {
  policy: Option<QueryRuntimePolicy>,
  disabled_reason: Option<String>,
}

pub(crate) struct QueryRuntime {
  state: RwLock<QueryRuntimeState>,
  reserved_bytes: AtomicU64,
  active_requests: AtomicU64,
}

impl QueryRuntime {
  pub(crate) fn new(policy: QueryRuntimePolicy) -> Self {
    Self {
      state: RwLock::new(QueryRuntimeState { policy: Some(policy), disabled_reason: None }),
      reserved_bytes: AtomicU64::new(0),
      active_requests: AtomicU64::new(0),
    }
  }

  pub(crate) fn disabled(reason: String) -> Self {
    Self {
      state: RwLock::new(QueryRuntimeState { policy: None, disabled_reason: Some(reason) }),
      reserved_bytes: AtomicU64::new(0),
      active_requests: AtomicU64::new(0),
    }
  }

  pub(crate) fn reconfigure(&self, policy: QueryRuntimePolicy) -> EngineResult<()> {
    let mut state = self
      .state
      .write()
      .map_err(|error| EngineError::IoError(std::io::Error::other(format!("query runtime state lock poisoned: {error}"))))?;
    state.policy = Some(policy);
    state.disabled_reason = None;
    Ok(())
  }

  pub(crate) fn snapshot(&self) -> EngineResult<QueryRuntimeSnapshot> {
    let state = self
      .state
      .read()
      .map_err(|error| EngineError::IoError(std::io::Error::other(format!("query runtime state lock poisoned: {error}"))))?
      .clone();
    Ok(QueryRuntimeSnapshot {
      policy: state.policy,
      disabled_reason: state.disabled_reason,
      reserved_bytes: self.reserved_bytes.load(Ordering::Acquire),
      active_requests: self.active_requests.load(Ordering::Acquire),
    })
  }

  pub(crate) fn start_request(self: &Arc<Self>) -> EngineResult<QueryRequestBudget> {
    let snapshot = self.snapshot()?;
    let policy = snapshot.policy.ok_or_else(|| {
      EngineError::ResourceExhausted(format!(
        "query runtime is disabled{}",
        snapshot.disabled_reason.as_deref().map_or_else(String::new, |reason| format!(": {reason}"))
      ))
    })?;
    increment_atomic(&self.active_requests, "active query request count")?;
    Ok(QueryRequestBudget {
      inner: Arc::new(QueryRequestBudgetInner {
        runtime: Arc::clone(self),
        per_request_memory_bytes: policy.per_request_memory_bytes,
        position_scan_buffer_bytes: policy.position_scan_buffer_bytes,
        reserved_bytes: AtomicU64::new(0),
      }),
    })
  }

  fn reserve_global(&self, bytes: u64) -> EngineResult<()> {
    let state = self
      .state
      .read()
      .map_err(|error| EngineError::IoError(std::io::Error::other(format!("query runtime state lock poisoned: {error}"))))?;
    let policy = state.policy.ok_or_else(|| EngineError::ResourceExhausted("query runtime is disabled".to_string()))?;
    reserve_atomic(&self.reserved_bytes, bytes, policy.global_memory_bytes, "global query memory")
  }

  fn release_global(&self, bytes: u64) {
    release_atomic(&self.reserved_bytes, bytes, "global query memory");
  }
}

struct QueryRequestBudgetInner {
  runtime: Arc<QueryRuntime>,
  per_request_memory_bytes: u64,
  position_scan_buffer_bytes: u64,
  reserved_bytes: AtomicU64,
}

impl Drop for QueryRequestBudgetInner {
  fn drop(&mut self) {
    assert_eq!(self.reserved_bytes.load(Ordering::Acquire), 0, "query request dropped with live runtime reservations");
    let previous = self.runtime.active_requests.fetch_sub(1, Ordering::AcqRel);
    assert!(previous > 0, "query runtime active-request accounting underflow");
  }
}

#[derive(Clone)]
pub(crate) struct QueryRequestBudget {
  inner: Arc<QueryRequestBudgetInner>,
}

impl QueryRequestBudget {
  pub(crate) fn reserve(&self, bytes: u64) -> EngineResult<QueryRuntimeReservation> {
    reserve_atomic(&self.inner.reserved_bytes, bytes, self.inner.per_request_memory_bytes, "per-request query memory")?;
    if let Err(error) = self.inner.runtime.reserve_global(bytes) {
      release_atomic(&self.inner.reserved_bytes, bytes, "per-request query memory");
      return Err(error);
    }
    Ok(QueryRuntimeReservation { request: self.clone(), bytes })
  }

  pub(crate) fn position_scan_buffer_bytes(&self) -> u64 {
    self.inner.position_scan_buffer_bytes
  }
}

pub(crate) struct QueryRuntimeReservation {
  request: QueryRequestBudget,
  bytes: u64,
}

impl QueryRuntimeReservation {
  pub(crate) fn bytes(&self) -> u64 {
    self.bytes
  }

  pub(crate) fn grow(&mut self, bytes: u64) -> EngineResult<()> {
    if bytes == 0 {
      return Ok(());
    }
    let next_bytes =
      self.bytes.checked_add(bytes).ok_or_else(|| EngineError::ResourceExhausted("query runtime reservation overflow".to_string()))?;
    reserve_atomic(&self.request.inner.reserved_bytes, bytes, self.request.inner.per_request_memory_bytes, "per-request query memory")?;
    if let Err(error) = self.request.inner.runtime.reserve_global(bytes) {
      release_atomic(&self.request.inner.reserved_bytes, bytes, "per-request query memory");
      return Err(error);
    }
    self.bytes = next_bytes;
    Ok(())
  }

  pub(crate) fn shrink(&mut self, bytes: u64) -> EngineResult<()> {
    if bytes > self.bytes {
      return Err(EngineError::InvalidInput(format!(
        "cannot shrink query runtime reservation by {bytes} bytes when it holds {}",
        self.bytes
      )));
    }
    self.bytes -= bytes;
    release_atomic(&self.request.inner.reserved_bytes, bytes, "per-request query memory");
    self.request.inner.runtime.release_global(bytes);
    Ok(())
  }
}

impl Drop for QueryRuntimeReservation {
  fn drop(&mut self) {
    release_atomic(&self.request.inner.reserved_bytes, self.bytes, "per-request query memory");
    self.request.inner.runtime.release_global(self.bytes);
  }
}

fn reserve_atomic(counter: &AtomicU64, bytes: u64, limit: u64, label: &str) -> EngineResult<()> {
  let mut current = counter.load(Ordering::Acquire);
  loop {
    let next = current.checked_add(bytes).ok_or_else(|| EngineError::ResourceExhausted(format!("{label} accounting overflow")))?;
    if next > limit {
      return Err(EngineError::ResourceExhausted(format!("{label} requires {next} bytes but its configured limit is {limit}")));
    }
    match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
      Ok(_) => return Ok(()),
      Err(observed) => current = observed,
    }
  }
}

fn increment_atomic(counter: &AtomicU64, label: &str) -> EngineResult<()> {
  let mut current = counter.load(Ordering::Acquire);
  loop {
    let next = current.checked_add(1).ok_or_else(|| EngineError::ResourceExhausted(format!("{label} overflow")))?;
    match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
      Ok(_) => return Ok(()),
      Err(observed) => current = observed,
    }
  }
}

fn release_atomic(counter: &AtomicU64, bytes: u64, label: &str) {
  let previous = counter.fetch_sub(bytes, Ordering::AcqRel);
  assert!(previous >= bytes, "{label} accounting underflow");
}
