use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

use super::admission::{
  AdmissionModeV1, BinaryCapabilityProfileV1, CapabilitySetV1, SemanticReadOnlyAdmissionV1, V4AdmissionError, V4AdmissionResult,
  admit_v4_header,
};
use super::database_header::SelectedDatabaseHeaderV4;
use super::root_authority::ImmutableNamespaceAuthorityV1;
use super::system_family::SystemFamilyRegistryV1;

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
  #[error("root lifecycle query memory admission failed: {0}")]
  LifecycleMemory(String),
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
  #[error("the database has active request pins")]
  RequestPinned,
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
      Self::LifecycleMemory(_) => "read_view_memory_admission",
      Self::LockPoisoned | Self::AccountingCorrupt(_) => "read_pin_corrupt",
      Self::Memory(_) => "read_pin_memory_admission",
      Self::RootLimit => "read_pin_root_limit",
      Self::PinLimit => "read_pin_limit",
      Self::RootPinned => "root_pinned",
      Self::RequestPinned => "request_pinned",
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
  hash_algorithm: HashAlgorithm,
  maximum_tracked_roots: usize,
  maximum_active_pins: u64,
  failed: AtomicBool,
  #[cfg(test)]
  fail_next_cleanup: AtomicBool,
  global_admission_gate: RwLock<()>,
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
        hash_algorithm,
        maximum_tracked_roots,
        maximum_active_pins,
        failed: AtomicBool::new(false),
        #[cfg(test)]
        fail_next_cleanup: AtomicBool::new(false),
        global_admission_gate: RwLock::new(()),
        state: Mutex::new(RootPinCoordinatorStateV1 { root_gates: BTreeMap::new(), active_pin_count: 0 }),
      }),
    })
  }

  pub fn memory_coordinator(&self) -> Arc<MemoryCoordinator> {
    Arc::clone(&self.inner.memory_coordinator)
  }

  pub fn hash_algorithm(&self) -> HashAlgorithm {
    self.inner.hash_algorithm
  }

  pub fn validate_root_hash(&self, root_hash: &[u8]) -> Result<(), RootPinCoordinatorErrorV1> {
    if !self.root_hash_is_valid(root_hash) {
      return Err(RootPinCoordinatorErrorV1::InvalidRootHash);
    }
    Ok(())
  }

  pub fn root_hash_is_valid(&self, root_hash: &[u8]) -> bool {
    root_hash.len() == self.inner.hash_algorithm.hash_length() && root_hash.iter().any(|byte| *byte != 0)
  }

  pub fn active_pin_count(&self) -> Result<u64, RootPinCoordinatorErrorV1> {
    Ok(self.lock_state()?.active_pin_count)
  }

  pub fn tracked_root_count(&self) -> Result<usize, RootPinCoordinatorErrorV1> {
    Ok(self.lock_state()?.root_gates.len())
  }

  #[cfg(test)]
  pub(crate) fn fail_next_cleanup_for_test(&self) {
    self.inner.fail_next_cleanup.store(true, Ordering::Release);
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
    let _global_admission = self.lock_global_admission()?;
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
    let _global_admission = self.lock_global_admission()?;
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

  /// Run a database-wide final recheck while excluding all new request-pin
  /// admission. Existing request pins cause a bounded refusal.
  ///
  /// The callback must not reenter this coordinator.
  pub fn with_global_exclusion<T>(
    &self,
    cancellation: &CancellationToken,
    action: impl FnOnce() -> Result<T, RootPinCoordinatorErrorV1>,
  ) -> Result<T, RootPinCoordinatorErrorV1> {
    if self.inner.failed.load(Ordering::Acquire) {
      return Err(RootPinCoordinatorErrorV1::AccountingCorrupt("the coordinator previously failed"));
    }
    if cancellation.is_cancelled() {
      return Err(RootPinCoordinatorErrorV1::Canceled);
    }
    let _global_exclusion = self.lock_global_exclusion()?;
    if cancellation.is_cancelled() {
      return Err(RootPinCoordinatorErrorV1::Canceled);
    }
    if self.lock_state()?.active_pin_count != 0 {
      return Err(RootPinCoordinatorErrorV1::RequestPinned);
    }
    action()
  }

  fn validate_operation(&self, root_hash: &[u8], cancellation: &CancellationToken) -> Result<(), RootPinCoordinatorErrorV1> {
    if self.inner.failed.load(Ordering::Acquire) {
      return Err(RootPinCoordinatorErrorV1::AccountingCorrupt("the coordinator previously failed"));
    }
    if cancellation.is_cancelled() {
      return Err(RootPinCoordinatorErrorV1::Canceled);
    }
    self.validate_root_hash(root_hash)
  }

  fn lock_global_admission(&self) -> Result<RwLockReadGuard<'_, ()>, RootPinCoordinatorErrorV1> {
    match self.inner.global_admission_gate.read() {
      Ok(guard) => Ok(guard),
      Err(poisoned) => {
        drop(poisoned);
        self.inner.failed.store(true, Ordering::Release);
        Err(RootPinCoordinatorErrorV1::LockPoisoned)
      }
    }
  }

  fn lock_global_exclusion(&self) -> Result<RwLockWriteGuard<'_, ()>, RootPinCoordinatorErrorV1> {
    match self.inner.global_admission_gate.write() {
      Ok(guard) => Ok(guard),
      Err(poisoned) => {
        drop(poisoned);
        self.inner.failed.store(true, Ordering::Release);
        Err(RootPinCoordinatorErrorV1::LockPoisoned)
      }
    }
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
    #[cfg(test)]
    if self.inner.fail_next_cleanup.swap(false, Ordering::AcqRel) {
      return Err(RootPinCoordinatorErrorV1::AccountingCorrupt("injected root-pin cleanup failure"));
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadViewSelectorV1<'a> {
  CurrentHead,
  ExplicitRoot(&'a [u8]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadViewCredentialKindV1 {
  Ordinary,
  Share,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadViewConcealmentV1 {
  Reveal,
  Conceal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentReadAuthorizationV1<T> {
  authorization: T,
  credential_kind: ReadViewCredentialKindV1,
  concealment: ReadViewConcealmentV1,
}

impl<T> CurrentReadAuthorizationV1<T> {
  pub const fn new(authorization: T, credential_kind: ReadViewCredentialKindV1, concealment: ReadViewConcealmentV1) -> Self {
    Self { authorization, credential_kind, concealment }
  }

  pub const fn authorization(&self) -> &T {
    &self.authorization
  }

  pub const fn credential_kind(&self) -> ReadViewCredentialKindV1 {
    self.credential_kind
  }

  pub const fn concealment(&self) -> ReadViewConcealmentV1 {
    self.concealment
  }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ReadViewAuthorizationErrorV1 {
  #[error("current read authorization was denied")]
  Denied { concealment: ReadViewConcealmentV1 },
  #[error("current read authorization is unavailable: {message}")]
  Unavailable { concealment: ReadViewConcealmentV1, message: String },
  #[error("current read authorization is corrupt: {message}")]
  Corrupt { concealment: ReadViewConcealmentV1, message: String },
}

impl ReadViewAuthorizationErrorV1 {
  pub const fn denied(concealment: ReadViewConcealmentV1) -> Self {
    Self::Denied { concealment }
  }

  pub fn unavailable(concealment: ReadViewConcealmentV1, message: impl Into<String>) -> Self {
    Self::Unavailable { concealment, message: message.into() }
  }

  pub fn corrupt(concealment: ReadViewConcealmentV1, message: impl Into<String>) -> Self {
    Self::Corrupt { concealment, message: message.into() }
  }

  pub const fn code(&self) -> &'static str {
    match self {
      Self::Denied { .. } => "read_authorization_denied",
      Self::Unavailable { .. } => "read_authorization_unavailable",
      Self::Corrupt { .. } => "read_authorization_corrupt",
    }
  }

  pub const fn concealment(&self) -> ReadViewConcealmentV1 {
    match self {
      Self::Denied { concealment } | Self::Unavailable { concealment, .. } | Self::Corrupt { concealment, .. } => *concealment,
    }
  }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ReadViewAuthorizationFailureV1 {
  #[error("selected-root read authorization was denied")]
  Denied,
  #[error("selected-root read authorization was canceled")]
  Canceled,
  #[error("selected-root read authorization is unavailable: {0}")]
  Unavailable(String),
  #[error("selected-root read authorization is corrupt: {0}")]
  Corrupt(String),
}

impl ReadViewAuthorizationFailureV1 {
  pub const fn code(&self) -> &'static str {
    match self {
      Self::Denied => "read_authorization_denied",
      Self::Canceled => "read_view_canceled",
      Self::Unavailable(_) => "read_authorization_unavailable",
      Self::Corrupt(_) => "read_authorization_corrupt",
    }
  }
}

/// Request-owned authorization adapter used by the common read-view resolver.
///
/// Current authorization must not inspect selected-root authority. The second
/// call may only restrict the current authorization; it must never expand it.
pub trait ReadViewAuthorizerV1 {
  type CurrentAuthorization;
  type ResolvedAuthorization;

  fn authorize_current(
    &self,
    cancellation: &CancellationToken,
  ) -> Result<CurrentReadAuthorizationV1<Self::CurrentAuthorization>, ReadViewAuthorizationErrorV1>;

  fn restrict_to_selected_root(
    &self,
    current: &Self::CurrentAuthorization,
    header: &SelectedDatabaseHeaderV4,
    authority: &LoadedReadAuthorityV1,
    cancellation: &CancellationToken,
  ) -> Result<Self::ResolvedAuthorization, ReadViewAuthorizationFailureV1>;
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ReadViewSourceErrorV1 {
  #[error("selected-root source work was canceled")]
  Canceled,
  #[error("selected-root query memory admission failed: {0}")]
  Memory(String),
  #[error("selected database header is unavailable: {0}")]
  HeaderUnavailable(String),
  #[error("selected database header is corrupt: {0}")]
  HeaderCorrupt(String),
  #[error("root has no complete admission authority")]
  RootNotAdmitted,
  #[error("immutable root authority is unavailable: {0}")]
  AuthorityUnavailable(String),
  #[error("immutable root authority is corrupt: {0}")]
  AuthorityCorrupt(String),
}

impl ReadViewSourceErrorV1 {
  pub const fn code(&self) -> &'static str {
    match self {
      Self::Canceled => "read_view_canceled",
      Self::Memory(_) => "read_view_memory_admission",
      Self::HeaderUnavailable(_) => "database_header_unavailable",
      Self::HeaderCorrupt(_) => "database_header_corrupt",
      Self::RootNotAdmitted => "invalid_namespace_root",
      Self::AuthorityUnavailable(_) => "root_authority_unavailable",
      Self::AuthorityCorrupt(_) => "root_authority_corrupt",
    }
  }
}

pub struct LoadedReadAuthorityV1 {
  pub authority: ImmutableNamespaceAuthorityV1,
  pub legacy_root_hash: Option<Vec<u8>>,
  memory_reservation: Option<MemoryReservation>,
}

impl LoadedReadAuthorityV1 {
  pub const fn new(authority: ImmutableNamespaceAuthorityV1, legacy_root_hash: Option<Vec<u8>>) -> Self {
    Self { authority, legacy_root_hash, memory_reservation: None }
  }

  pub const fn new_accounted(
    authority: ImmutableNamespaceAuthorityV1,
    legacy_root_hash: Option<Vec<u8>>,
    memory_reservation: MemoryReservation,
  ) -> Self {
    Self { authority, legacy_root_hash, memory_reservation: Some(memory_reservation) }
  }

  pub fn retained_memory_bytes(&self) -> u64 {
    self.memory_reservation.as_ref().map_or(0, MemoryReservation::bytes)
  }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ReadViewLifecycleErrorV1 {
  #[error("root lifecycle authority is corrupt: {0}")]
  Corrupt(String),
  #[error("root lifecycle authority is unavailable: {0}")]
  Unavailable(String),
  #[error("root lifecycle observation was canceled")]
  Canceled,
  #[error("root lifecycle query memory admission failed: {0}")]
  Memory(String),
}

/// Storage adapter for one selected physical v4 database.
///
/// `capture_header` is called exactly once per resolution. Later methods must
/// use that captured header and must not recapture mutable HEAD state.
pub trait ReadViewAuthoritySourceV1: Send + Sync {
  fn capture_header(&self, cancellation: &CancellationToken) -> Result<SelectedDatabaseHeaderV4, ReadViewSourceErrorV1>;

  fn load_verified_authority(
    &self,
    header: &SelectedDatabaseHeaderV4,
    root_hash: &[u8],
    cancellation: &CancellationToken,
  ) -> Result<LoadedReadAuthorityV1, ReadViewSourceErrorV1>;

  fn observe_lifecycle(
    &self,
    header: &SelectedDatabaseHeaderV4,
    root_hash: &[u8],
    cancellation: &CancellationToken,
  ) -> Result<RootLifecycleObservationV1, ReadViewLifecycleErrorV1>;
}

#[derive(Debug, Error)]
pub enum ReadViewAuthorizedFailureV1 {
  #[error("read-view resolution was canceled")]
  Canceled,
  #[error("root hash does not match the selected database")]
  InvalidRootHash,
  #[error("the pin coordinator uses another hash algorithm")]
  CoordinatorHashAlgorithmMismatch,
  #[error("share credentials can resolve only the captured current HEAD")]
  ShareHistoricalRoot,
  #[error("hash is not an admitted namespace root")]
  InvalidNamespaceRoot,
  #[error("current HEAD has no complete immutable authority")]
  CurrentAuthorityCorrupt,
  #[error("loaded immutable root authority disagrees with the selected header: {0}")]
  AuthorityClosureCorrupt(&'static str),
  #[error("selected root requires unsupported reader capabilities: {0:?}")]
  UnsupportedCapabilities(Vec<u16>),
  #[error("selected v4 header is not semantically readable: {0}")]
  HeaderAdmission(#[source] V4AdmissionError),
  #[error(transparent)]
  Source(#[from] ReadViewSourceErrorV1),
  #[error(transparent)]
  Authorization(#[from] ReadViewAuthorizationFailureV1),
  #[error(transparent)]
  Pin(#[from] RootPinCoordinatorErrorV1),
}

impl ReadViewAuthorizedFailureV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Canceled => "read_view_canceled",
      Self::InvalidRootHash => "invalid_root_hash",
      Self::CoordinatorHashAlgorithmMismatch => "read_view_coordinator_mismatch",
      Self::ShareHistoricalRoot => "share_historical_root_forbidden",
      Self::InvalidNamespaceRoot => "invalid_namespace_root",
      Self::CurrentAuthorityCorrupt | Self::AuthorityClosureCorrupt(_) => "root_authority_corrupt",
      Self::UnsupportedCapabilities(_) => "unsupported_root_capabilities",
      Self::HeaderAdmission(error) => error.code(),
      Self::Source(error) => error.code(),
      Self::Authorization(error) => error.code(),
      Self::Pin(error) => error.code(),
    }
  }
}

#[derive(Debug, Error)]
pub enum ReadViewResolutionErrorV1 {
  #[error("read-view resolution was canceled before authorization")]
  Canceled,
  #[error(transparent)]
  CurrentAuthorization(#[from] ReadViewAuthorizationErrorV1),
  #[error("authorized read-view resolution failed: {failure}")]
  Authorized {
    #[source]
    failure: ReadViewAuthorizedFailureV1,
    concealment: ReadViewConcealmentV1,
  },
}

impl ReadViewResolutionErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Canceled => "read_view_canceled",
      Self::CurrentAuthorization(error) => error.code(),
      Self::Authorized { failure, .. } => failure.code(),
    }
  }

  pub const fn concealment(&self) -> Option<ReadViewConcealmentV1> {
    match self {
      Self::Canceled => None,
      Self::CurrentAuthorization(error) => Some(error.concealment()),
      Self::Authorized { concealment, .. } => Some(*concealment),
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadViewRootMetadataV1 {
  pub hash: Vec<u8>,
  pub state: ReadableRootStateV1,
  pub expires_at_ms: Option<i64>,
}

#[must_use = "dropping the resolved view releases its root request pin"]
pub struct ResolvedReadViewV1<A> {
  captured_header: SelectedDatabaseHeaderV4,
  database_id: [u8; 16],
  physical_instance_id: [u8; 16],
  hash_algorithm: HashAlgorithm,
  selected_header_slot: usize,
  header_slot_sequence: u64,
  write_sequence_high_water: u64,
  root_metadata: ReadViewRootMetadataV1,
  explicit_root: bool,
  legacy_root_hash: Option<Vec<u8>>,
  authority: ImmutableNamespaceAuthorityV1,
  authorization: A,
  credential_kind: ReadViewCredentialKindV1,
  concealment: ReadViewConcealmentV1,
  system_family_registry: &'static SystemFamilyRegistryV1<'static>,
  cancellation: CancellationToken,
  _pin: RootReadPinV1,
  _authority_memory: Option<MemoryReservation>,
}

impl<A> std::fmt::Debug for ResolvedReadViewV1<A> {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("ResolvedReadViewV1")
      .field("database_id", &hex::encode(self.database_id))
      .field("physical_instance_id", &hex::encode(self.physical_instance_id))
      .field("root", &self.root_metadata)
      .field("explicit_root", &self.explicit_root)
      .finish_non_exhaustive()
  }
}

impl<A> ResolvedReadViewV1<A> {
  pub const fn captured_header(&self) -> &SelectedDatabaseHeaderV4 {
    &self.captured_header
  }

  pub const fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  pub const fn physical_instance_id(&self) -> [u8; 16] {
    self.physical_instance_id
  }

  pub const fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }

  pub const fn selected_header_slot(&self) -> usize {
    self.selected_header_slot
  }

  pub const fn header_slot_sequence(&self) -> u64 {
    self.header_slot_sequence
  }

  pub const fn write_sequence_high_water(&self) -> u64 {
    self.write_sequence_high_water
  }

  pub const fn root_metadata(&self) -> &ReadViewRootMetadataV1 {
    &self.root_metadata
  }

  pub const fn is_explicit_root(&self) -> bool {
    self.explicit_root
  }

  pub fn legacy_root_hash(&self) -> Option<&[u8]> {
    self.legacy_root_hash.as_deref()
  }

  pub const fn authority(&self) -> &ImmutableNamespaceAuthorityV1 {
    &self.authority
  }

  pub const fn authorization(&self) -> &A {
    &self.authorization
  }

  pub const fn credential_kind(&self) -> ReadViewCredentialKindV1 {
    self.credential_kind
  }

  pub const fn concealment(&self) -> ReadViewConcealmentV1 {
    self.concealment
  }

  pub const fn system_family_registry(&self) -> &'static SystemFamilyRegistryV1<'static> {
    self.system_family_registry
  }

  pub const fn cancellation(&self) -> &CancellationToken {
    &self.cancellation
  }
}

pub struct ReadViewResolverV1<S> {
  authority_source: Arc<S>,
  pin_coordinator: RootReadPinCoordinatorV1,
  capability_profile: BinaryCapabilityProfileV1,
}

impl<S> ReadViewResolverV1<S>
where
  S: ReadViewAuthoritySourceV1,
{
  pub const fn new(
    authority_source: Arc<S>,
    pin_coordinator: RootReadPinCoordinatorV1,
    capability_profile: BinaryCapabilityProfileV1,
  ) -> Self {
    Self { authority_source, pin_coordinator, capability_profile }
  }

  pub fn resolve<A>(
    &self,
    selector: ReadViewSelectorV1<'_>,
    authorizer: &A,
    cancellation: &CancellationToken,
  ) -> Result<ResolvedReadViewV1<A::ResolvedAuthorization>, ReadViewResolutionErrorV1>
  where
    A: ReadViewAuthorizerV1,
  {
    if cancellation.is_cancelled() {
      return Err(ReadViewResolutionErrorV1::Canceled);
    }
    let current_authorization = authorizer.authorize_current(cancellation)?;
    let concealment = current_authorization.concealment();
    let authorized_error = |failure| ReadViewResolutionErrorV1::Authorized { failure, concealment };
    if cancellation.is_cancelled() {
      return Err(authorized_error(ReadViewAuthorizedFailureV1::Canceled));
    }

    let header = self.authority_source.capture_header(cancellation).map_err(|error| authorized_error(error.into()))?;
    if cancellation.is_cancelled() {
      return Err(authorized_error(ReadViewAuthorizedFailureV1::Canceled));
    }
    if header.header.hash_algorithm != self.pin_coordinator.hash_algorithm() {
      return Err(authorized_error(ReadViewAuthorizedFailureV1::CoordinatorHashAlgorithmMismatch));
    }
    let header_admission = semantic_header_admission(&header, self.capability_profile).map_err(authorized_error)?;

    let (root_hash, explicit_root) = match selector {
      ReadViewSelectorV1::CurrentHead => (header.header.head_hash.as_slice(), false),
      ReadViewSelectorV1::ExplicitRoot(root_hash) => (root_hash, true),
    };
    if !self.pin_coordinator.root_hash_is_valid(root_hash) {
      let failure =
        if explicit_root { ReadViewAuthorizedFailureV1::InvalidRootHash } else { ReadViewAuthorizedFailureV1::CurrentAuthorityCorrupt };
      return Err(authorized_error(failure));
    }
    if current_authorization.credential_kind() == ReadViewCredentialKindV1::Share && root_hash != header.header.head_hash {
      return Err(authorized_error(ReadViewAuthorizedFailureV1::ShareHistoricalRoot));
    }
    if cancellation.is_cancelled() {
      return Err(authorized_error(ReadViewAuthorizedFailureV1::Canceled));
    }

    let admission = self
      .pin_coordinator
      .admit_read(root_hash, cancellation, || {
        self.authority_source.observe_lifecycle(&header, root_hash, cancellation).map_err(|error| match error {
          ReadViewLifecycleErrorV1::Corrupt(_) => RootPinCoordinatorErrorV1::LifecycleCorrupt,
          ReadViewLifecycleErrorV1::Unavailable(_) => RootPinCoordinatorErrorV1::LifecycleUnavailable,
          ReadViewLifecycleErrorV1::Canceled => RootPinCoordinatorErrorV1::Canceled,
          ReadViewLifecycleErrorV1::Memory(message) => RootPinCoordinatorErrorV1::LifecycleMemory(message),
        })
      })
      .map_err(|error| authorized_error(error.into()))?;

    let loaded = match self.authority_source.load_verified_authority(&header, root_hash, cancellation) {
      Ok(loaded) => loaded,
      Err(ReadViewSourceErrorV1::RootNotAdmitted) => {
        let failure = if explicit_root {
          ReadViewAuthorizedFailureV1::InvalidNamespaceRoot
        } else {
          ReadViewAuthorizedFailureV1::CurrentAuthorityCorrupt
        };
        return Err(authorized_error(failure));
      }
      Err(error) => return Err(authorized_error(error.into())),
    };
    if cancellation.is_cancelled() {
      return Err(authorized_error(ReadViewAuthorizedFailureV1::Canceled));
    }
    validate_loaded_authority(&header, root_hash, &loaded).map_err(authorized_error)?;
    validate_root_capabilities(&loaded.authority, self.capability_profile).map_err(authorized_error)?;

    let authorization = authorizer
      .restrict_to_selected_root(current_authorization.authorization(), &header, &loaded, cancellation)
      .map_err(|error| match error {
        ReadViewAuthorizationFailureV1::Canceled => authorized_error(ReadViewAuthorizedFailureV1::Canceled),
        other => authorized_error(other.into()),
      })?;
    if cancellation.is_cancelled() {
      return Err(authorized_error(ReadViewAuthorizedFailureV1::Canceled));
    }

    let root_metadata =
      ReadViewRootMetadataV1 { hash: root_hash.to_vec(), state: admission.state, expires_at_ms: admission.state.expires_at_ms() };
    Ok(ResolvedReadViewV1 {
      database_id: header.header.database_id,
      physical_instance_id: header.header.physical_instance_id,
      hash_algorithm: header.header.hash_algorithm,
      selected_header_slot: header.selected_slot,
      header_slot_sequence: header.header.slot_sequence,
      write_sequence_high_water: header.header.write_sequence_high_water,
      root_metadata,
      explicit_root,
      legacy_root_hash: loaded.legacy_root_hash,
      authority: loaded.authority,
      authorization,
      credential_kind: current_authorization.credential_kind(),
      concealment,
      system_family_registry: header_admission.registry,
      cancellation: cancellation.clone(),
      _pin: admission.pin,
      _authority_memory: loaded.memory_reservation,
      captured_header: header,
    })
  }
}

fn semantic_header_admission(
  header: &SelectedDatabaseHeaderV4,
  capability_profile: BinaryCapabilityProfileV1,
) -> Result<SemanticReadOnlyAdmissionV1, ReadViewAuthorizedFailureV1> {
  match admit_v4_header(header, AdmissionModeV1::SemanticReadOnly, capability_profile, None) {
    Ok(V4AdmissionResult::SemanticReadOnly(admission)) => Ok(admission),
    Ok(V4AdmissionResult::DiagnosticRaw(_) | V4AdmissionResult::Writable(_)) => {
      Err(ReadViewAuthorizedFailureV1::AuthorityClosureCorrupt("semantic admission returned the wrong mode"))
    }
    Err(error) => Err(ReadViewAuthorizedFailureV1::HeaderAdmission(error)),
  }
}

fn validate_loaded_authority(
  header: &SelectedDatabaseHeaderV4,
  requested_root: &[u8],
  loaded: &LoadedReadAuthorityV1,
) -> Result<(), ReadViewAuthorizedFailureV1> {
  let authority = &loaded.authority;
  if authority.root.root_hash != requested_root || authority.admission.namespace_root != requested_root {
    return Err(ReadViewAuthorizedFailureV1::AuthorityClosureCorrupt("requested root identity mismatch"));
  }
  if authority.admission.database_id != header.header.database_id {
    return Err(ReadViewAuthorizedFailureV1::AuthorityClosureCorrupt("logical database identity mismatch"));
  }
  if authority.namespace_tree.root_hash != authority.root.namespace_tree_root {
    return Err(ReadViewAuthorizedFailureV1::AuthorityClosureCorrupt("namespace-tree edge mismatch"));
  }
  if authority.semantic_state.object_id != authority.root.semantic_state_root {
    return Err(ReadViewAuthorizedFailureV1::AuthorityClosureCorrupt("semantic-state edge mismatch"));
  }
  if authority.admission.selected_header_slot_sequence > header.header.slot_sequence
    || authority.admission.publication_sequence > header.header.write_sequence_high_water
  {
    return Err(ReadViewAuthorizedFailureV1::AuthorityClosureCorrupt("admission sequence exceeds the captured header"));
  }
  if let Some(legacy_root_hash) = &loaded.legacy_root_hash {
    if legacy_root_hash.len() != header.header.hash_algorithm.hash_length() || legacy_root_hash.iter().all(|byte| *byte == 0) {
      return Err(ReadViewAuthorizedFailureV1::AuthorityClosureCorrupt("legacy root mapping is malformed"));
    }
  }
  Ok(())
}

fn validate_root_capabilities(
  authority: &ImmutableNamespaceAuthorityV1,
  capability_profile: BinaryCapabilityProfileV1,
) -> Result<(), ReadViewAuthorizedFailureV1> {
  let root = CapabilitySetV1::from_bytes(authority.root.required_capabilities)
    .map_err(|_| ReadViewAuthorizedFailureV1::AuthorityClosureCorrupt("namespace root capability set is malformed"))?;
  let semantic = CapabilitySetV1::from_bytes(authority.semantic_state.required_capabilities)
    .map_err(|_| ReadViewAuthorizedFailureV1::AuthorityClosureCorrupt("semantic state capability set is malformed"))?;
  let missing = root.union(semantic).difference(capability_profile.supported_reader_capabilities);
  if !missing.is_empty() {
    return Err(ReadViewAuthorizedFailureV1::UnsupportedCapabilities(missing.bits()));
  }
  Ok(())
}
