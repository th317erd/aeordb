use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

const ROOT_GATE_ACCOUNTED_BASE_BYTES: u64 = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootLifecycleObservationV1 {
  Live,
  Retained,
  PendingDelete { pending_since_ms: i64, grace_at_pending_ms: u64, current_configured_grace_ms: u64 },
  LogicallyRetired,
  PhysicallyReclaimed,
  UnknownOrUnadmitted,
  Corrupt,
  Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadableRootStateV1 {
  Live,
  Retained,
  PendingDelete { pending_since_ms: i64, expires_at_ms: i64 },
}

impl ReadableRootStateV1 {
  pub const fn expires_at_ms(self) -> Option<i64> {
    match self {
      Self::Live | Self::Retained => None,
      Self::PendingDelete { expires_at_ms, .. } => Some(expires_at_ms),
    }
  }
}

#[derive(Debug, Error)]
pub enum RootPinCoordinatorErrorV1 {
  #[error("invalid root-pin coordinator configuration: {0}")]
  InvalidConfiguration(&'static str),
  #[error("root hash does not match the selected database hash algorithm")]
  InvalidRootHash,
  #[error("read-view admission was canceled")]
  Canceled,
  #[error("root is logically unavailable")]
  RootExpired,
  #[error("hash is not an admitted namespace root")]
  InvalidNamespaceRoot,
  #[error("root lifecycle authority is corrupt")]
  LifecycleCorrupt,
  #[error("root lifecycle authority is unavailable")]
  LifecycleUnavailable,
  #[error("root-pin coordinator lock is poisoned")]
  LockPoisoned,
  #[error("root-pin coordinator accounting is corrupt: {0}")]
  AccountingCorrupt(&'static str),
  #[error("root-pin memory admission failed: {0}")]
  Memory(#[from] MemoryCoordinatorError),
  #[error("root-pin distinct-root limit is exhausted")]
  RootLimit,
  #[error("root-pin active-pin limit is exhausted")]
  PinLimit,
  #[error("root has active request pins")]
  RootPinned,
}

impl RootPinCoordinatorErrorV1 {
  pub const fn code(&self) -> &'static str {
    match self {
      Self::InvalidConfiguration(_) => "read_pin_invalid_configuration",
      Self::InvalidRootHash => "invalid_root_hash",
      Self::Canceled => "read_view_canceled",
      Self::RootExpired => "root_expired",
      Self::InvalidNamespaceRoot => "invalid_namespace_root",
      Self::LifecycleCorrupt => "root_lifecycle_corrupt",
      Self::LifecycleUnavailable => "root_lifecycle_unavailable",
      Self::LockPoisoned | Self::AccountingCorrupt(_) => "read_pin_corrupt",
      Self::Memory(_) => "read_pin_memory_admission",
      Self::RootLimit => "read_pin_root_limit",
      Self::PinLimit => "read_pin_limit",
      Self::RootPinned => "root_pinned",
    }
  }
}

struct RootGateV1 {
  root_hash: Vec<u8>,
  active_pins: Mutex<u64>,
  _memory_reservation: MemoryReservation,
}

struct RootPinCoordinatorStateV1 {
  root_gates: BTreeMap<Vec<u8>, Arc<RootGateV1>>,
  active_pin_count: u64,
}

struct RootReadPinCoordinatorInnerV1 {
  memory_coordinator: Arc<MemoryCoordinator>,
  hash_width: usize,
  maximum_tracked_roots: usize,
  maximum_active_pins: u64,
  failed: AtomicBool,
  state: Mutex<RootPinCoordinatorStateV1>,
}

#[derive(Clone)]
pub struct RootReadPinCoordinatorV1 {
  inner: Arc<RootReadPinCoordinatorInnerV1>,
}

#[must_use = "dropping the guard releases the root request pin"]
pub struct RootReadPinV1 {
  coordinator: RootReadPinCoordinatorV1,
  root_gate: Arc<RootGateV1>,
  released: bool,
}

impl std::fmt::Debug for RootReadPinV1 {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("RootReadPinV1")
      .field("root_hash", &hex::encode(&self.root_gate.root_hash))
      .field("released", &self.released)
      .finish()
  }
}

#[must_use = "the admission owns the root request pin"]
#[derive(Debug)]
pub struct RootReadAdmissionV1 {
  pub state: ReadableRootStateV1,
  pub pin: RootReadPinV1,
}

impl RootReadPinCoordinatorV1 {
  pub fn new(
    memory_coordinator: Arc<MemoryCoordinator>,
    hash_algorithm: HashAlgorithm,
    maximum_tracked_roots: usize,
    maximum_active_pins: u64,
  ) -> Result<Self, RootPinCoordinatorErrorV1> {
    if maximum_tracked_roots == 0 {
      return Err(RootPinCoordinatorErrorV1::InvalidConfiguration("maximum tracked roots must be nonzero"));
    }
    if maximum_active_pins == 0 {
      return Err(RootPinCoordinatorErrorV1::InvalidConfiguration("maximum active pins must be nonzero"));
    }
    Ok(Self {
      inner: Arc::new(RootReadPinCoordinatorInnerV1 {
        memory_coordinator,
        hash_width: hash_algorithm.hash_length(),
        maximum_tracked_roots,
        maximum_active_pins,
        failed: AtomicBool::new(false),
        state: Mutex::new(RootPinCoordinatorStateV1 { root_gates: BTreeMap::new(), active_pin_count: 0 }),
      }),
    })
  }

  pub fn memory_coordinator(&self) -> Arc<MemoryCoordinator> {
    Arc::clone(&self.inner.memory_coordinator)
  }

  pub fn active_pin_count(&self) -> Result<u64, RootPinCoordinatorErrorV1> {
    Ok(self.lock_state()?.active_pin_count)
  }

  pub fn tracked_root_count(&self) -> Result<usize, RootPinCoordinatorErrorV1> {
    Ok(self.lock_state()?.root_gates.len())
  }

  /// Observe lifecycle and acquire the pin under one root guard.
  ///
  /// The lifecycle callback must not reenter this coordinator for the same root.
  pub fn admit_read(
    &self,
    root_hash: &[u8],
    cancellation: &CancellationToken,
    observe_lifecycle: impl FnOnce() -> Result<RootLifecycleObservationV1, RootPinCoordinatorErrorV1>,
  ) -> Result<RootReadAdmissionV1, RootPinCoordinatorErrorV1> {
    self.validate_operation(root_hash, cancellation)?;
    let root_gate = self.root_gate(root_hash)?;
    let mut active_pins = self.lock_root_gate(&root_gate)?;
    if cancellation.is_cancelled() {
      drop(active_pins);
      return self.finish_with_cleanup(&root_gate, Err(RootPinCoordinatorErrorV1::Canceled));
    }
    let readable_state = match observe_lifecycle().and_then(readable_state) {
      Ok(state) => state,
      Err(error) => {
        drop(active_pins);
        return self.finish_with_cleanup(&root_gate, Err(error));
      }
    };
    if cancellation.is_cancelled() {
      drop(active_pins);
      return self.finish_with_cleanup(&root_gate, Err(RootPinCoordinatorErrorV1::Canceled));
    }

    let mut coordinator_state = match self.lock_state() {
      Ok(state) => state,
      Err(error) => {
        drop(active_pins);
        return self.finish_with_cleanup(&root_gate, Err(error));
      }
    };
    let Some(next_total) = coordinator_state.active_pin_count.checked_add(1) else {
      drop(coordinator_state);
      drop(active_pins);
      return self.finish_with_cleanup(&root_gate, Err(RootPinCoordinatorErrorV1::PinLimit));
    };
    if next_total > self.inner.maximum_active_pins {
      drop(coordinator_state);
      drop(active_pins);
      return self.finish_with_cleanup(&root_gate, Err(RootPinCoordinatorErrorV1::PinLimit));
    }
    let Some(next_root_total) = active_pins.checked_add(1) else {
      drop(coordinator_state);
      drop(active_pins);
      return self.finish_with_cleanup(&root_gate, Err(RootPinCoordinatorErrorV1::PinLimit));
    };
    *active_pins = next_root_total;
    coordinator_state.active_pin_count = next_total;
    drop(coordinator_state);
    drop(active_pins);

    Ok(RootReadAdmissionV1 { state: readable_state, pin: RootReadPinV1 { coordinator: self.clone(), root_gate, released: false } })
  }

  /// Run a retirement recheck while excluding request-pin acquisition.
  ///
  /// The retirement callback must not reenter this coordinator for the same root.
  pub fn with_retirement_exclusion<T>(
    &self,
    root_hash: &[u8],
    cancellation: &CancellationToken,
    retire: impl FnOnce() -> Result<T, RootPinCoordinatorErrorV1>,
  ) -> Result<T, RootPinCoordinatorErrorV1> {
    self.validate_operation(root_hash, cancellation)?;
    let root_gate = self.root_gate(root_hash)?;
    let active_pins = self.lock_root_gate(&root_gate)?;
    if cancellation.is_cancelled() {
      drop(active_pins);
      return self.finish_with_cleanup(&root_gate, Err(RootPinCoordinatorErrorV1::Canceled));
    }
    if *active_pins != 0 {
      drop(active_pins);
      return self.finish_with_cleanup(&root_gate, Err(RootPinCoordinatorErrorV1::RootPinned));
    }
    let result = retire();
    drop(active_pins);
    self.finish_with_cleanup(&root_gate, result)
  }

  fn validate_operation(&self, root_hash: &[u8], cancellation: &CancellationToken) -> Result<(), RootPinCoordinatorErrorV1> {
    if self.inner.failed.load(Ordering::Acquire) {
      return Err(RootPinCoordinatorErrorV1::AccountingCorrupt("the coordinator previously failed"));
    }
    if cancellation.is_cancelled() {
      return Err(RootPinCoordinatorErrorV1::Canceled);
    }
    if root_hash.len() != self.inner.hash_width || root_hash.iter().all(|byte| *byte == 0) {
      return Err(RootPinCoordinatorErrorV1::InvalidRootHash);
    }
    Ok(())
  }

  fn root_gate(&self, root_hash: &[u8]) -> Result<Arc<RootGateV1>, RootPinCoordinatorErrorV1> {
    let mut state = self.lock_state()?;
    if let Some(existing) = state.root_gates.get(root_hash) {
      return Ok(Arc::clone(existing));
    }
    if state.root_gates.len() >= self.inner.maximum_tracked_roots {
      return Err(RootPinCoordinatorErrorV1::RootLimit);
    }
    let root_hash_bytes = u64::try_from(root_hash.len()).map_err(|_| RootPinCoordinatorErrorV1::RootLimit)?;
    let accounted_bytes = ROOT_GATE_ACCOUNTED_BASE_BYTES.checked_add(root_hash_bytes).ok_or(RootPinCoordinatorErrorV1::RootLimit)?;
    let reservation = self.inner.memory_coordinator.reserve(MemoryOwner::ServerCaches, accounted_bytes, AdmissionClass::Workload)?;
    let gate = Arc::new(RootGateV1 { root_hash: root_hash.to_vec(), active_pins: Mutex::new(0), _memory_reservation: reservation });
    state.root_gates.insert(root_hash.to_vec(), Arc::clone(&gate));
    Ok(gate)
  }

  fn lock_state(&self) -> Result<MutexGuard<'_, RootPinCoordinatorStateV1>, RootPinCoordinatorErrorV1> {
    self.inner.state.lock().map_err(|_| {
      self.inner.failed.store(true, Ordering::Release);
      RootPinCoordinatorErrorV1::LockPoisoned
    })
  }

  fn lock_root_gate<'a>(&self, root_gate: &'a RootGateV1) -> Result<MutexGuard<'a, u64>, RootPinCoordinatorErrorV1> {
    root_gate.active_pins.lock().map_err(|_| {
      self.inner.failed.store(true, Ordering::Release);
      RootPinCoordinatorErrorV1::LockPoisoned
    })
  }

  fn cleanup_root_gate(&self, root_gate: &Arc<RootGateV1>) -> Result<(), RootPinCoordinatorErrorV1> {
    let active_pins = self.lock_root_gate(root_gate)?;
    if *active_pins != 0 {
      return Ok(());
    }
    drop(active_pins);
    let mut state = self.lock_state()?;
    let removable = state
      .root_gates
      .get(&root_gate.root_hash)
      .is_some_and(|current| Arc::ptr_eq(current, root_gate) && Arc::strong_count(root_gate) == 2);
    if removable {
      state.root_gates.remove(&root_gate.root_hash);
    }
    Ok(())
  }

  fn finish_with_cleanup<T>(
    &self,
    root_gate: &Arc<RootGateV1>,
    primary: Result<T, RootPinCoordinatorErrorV1>,
  ) -> Result<T, RootPinCoordinatorErrorV1> {
    match (primary, self.cleanup_root_gate(root_gate)) {
      (result, Ok(())) => result,
      (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
      (Err(primary_error), Err(cleanup_error)) => {
        tracing::error!(
          root_hash = %hex::encode(&root_gate.root_hash),
          primary_error = %primary_error,
          cleanup_error = %cleanup_error,
          "Root-pin cleanup failed while preserving a primary error"
        );
        Err(cleanup_error)
      }
    }
  }

  fn release_pin(&self, root_gate: &Arc<RootGateV1>) -> Result<(), RootPinCoordinatorErrorV1> {
    let mut active_pins = self.lock_root_gate(root_gate)?;
    let Some(next_root_total) = active_pins.checked_sub(1) else {
      self.inner.failed.store(true, Ordering::Release);
      return Err(RootPinCoordinatorErrorV1::AccountingCorrupt("per-root pin count underflow"));
    };
    let mut state = self.lock_state()?;
    let Some(next_total) = state.active_pin_count.checked_sub(1) else {
      self.inner.failed.store(true, Ordering::Release);
      return Err(RootPinCoordinatorErrorV1::AccountingCorrupt("global pin count underflow"));
    };
    *active_pins = next_root_total;
    state.active_pin_count = next_total;
    drop(state);
    drop(active_pins);
    self.cleanup_root_gate(root_gate)
  }
}

impl Drop for RootReadPinV1 {
  fn drop(&mut self) {
    if self.released {
      return;
    }
    if let Err(error) = self.coordinator.release_pin(&self.root_gate) {
      tracing::error!(root_hash = %hex::encode(&self.root_gate.root_hash), %error, "Failed to release a root request pin");
    }
    self.released = true;
  }
}

fn readable_state(observation: RootLifecycleObservationV1) -> Result<ReadableRootStateV1, RootPinCoordinatorErrorV1> {
  match observation {
    RootLifecycleObservationV1::Live => Ok(ReadableRootStateV1::Live),
    RootLifecycleObservationV1::Retained => Ok(ReadableRootStateV1::Retained),
    RootLifecycleObservationV1::PendingDelete { pending_since_ms, grace_at_pending_ms, current_configured_grace_ms } => {
      if pending_since_ms <= 0 {
        return Err(RootPinCoordinatorErrorV1::LifecycleCorrupt);
      }
      let effective_grace_ms = grace_at_pending_ms.max(current_configured_grace_ms);
      let effective_grace_ms = i64::try_from(effective_grace_ms).map_err(|_| RootPinCoordinatorErrorV1::LifecycleCorrupt)?;
      let expires_at_ms = pending_since_ms.checked_add(effective_grace_ms).ok_or(RootPinCoordinatorErrorV1::LifecycleCorrupt)?;
      Ok(ReadableRootStateV1::PendingDelete { pending_since_ms, expires_at_ms })
    }
    RootLifecycleObservationV1::LogicallyRetired | RootLifecycleObservationV1::PhysicallyReclaimed => {
      Err(RootPinCoordinatorErrorV1::RootExpired)
    }
    RootLifecycleObservationV1::UnknownOrUnadmitted => Err(RootPinCoordinatorErrorV1::InvalidNamespaceRoot),
    RootLifecycleObservationV1::Corrupt => Err(RootPinCoordinatorErrorV1::LifecycleCorrupt),
    RootLifecycleObservationV1::Unavailable => Err(RootPinCoordinatorErrorV1::LifecycleUnavailable),
  }
}
