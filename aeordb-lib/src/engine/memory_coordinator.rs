use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryOwner {
  KvResidentPages,
  KvSnapshotGenerations,
  KvWriteBuffers,
  DurabilityWaiters,
  DirectoryCache,
  IndexCleanCache,
  IndexDirtyBuffers,
  Query,
  StreamingRead,
  ParserPlugin,
  Task,
  GarbageCollection,
  Migration,
  BackupRestore,
  Repair,
  VoidManager,
  ServerCaches,
  HealthStatus,
  EmergencySpill,
  Shutdown,
}

impl MemoryOwner {
  pub const ALL: [Self; 20] = [
    Self::KvResidentPages,
    Self::KvSnapshotGenerations,
    Self::KvWriteBuffers,
    Self::DurabilityWaiters,
    Self::DirectoryCache,
    Self::IndexCleanCache,
    Self::IndexDirtyBuffers,
    Self::Query,
    Self::StreamingRead,
    Self::ParserPlugin,
    Self::Task,
    Self::GarbageCollection,
    Self::Migration,
    Self::BackupRestore,
    Self::Repair,
    Self::VoidManager,
    Self::ServerCaches,
    Self::HealthStatus,
    Self::EmergencySpill,
    Self::Shutdown,
  ];

  pub const fn as_str(self) -> &'static str {
    match self {
      Self::KvResidentPages => "kv_resident_pages",
      Self::KvSnapshotGenerations => "kv_snapshot_generations",
      Self::KvWriteBuffers => "kv_write_buffers",
      Self::DurabilityWaiters => "durability_waiters",
      Self::DirectoryCache => "directory_cache",
      Self::IndexCleanCache => "index_clean_cache",
      Self::IndexDirtyBuffers => "index_dirty_buffers",
      Self::Query => "query",
      Self::StreamingRead => "streaming_read",
      Self::ParserPlugin => "parser_plugin",
      Self::Task => "task",
      Self::GarbageCollection => "garbage_collection",
      Self::Migration => "migration",
      Self::BackupRestore => "backup_restore",
      Self::Repair => "repair",
      Self::VoidManager => "void_manager",
      Self::ServerCaches => "server_caches",
      Self::HealthStatus => "health_status",
      Self::EmergencySpill => "emergency_spill",
      Self::Shutdown => "shutdown",
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CriticalMemoryPurpose {
  DurableWrite,
  StreamingRead,
  HealthStatus,
  EmergencySpill,
  Shutdown,
  BoundedRecovery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionClass {
  Cache,
  Workload,
  Maintenance,
  Critical(CriticalMemoryPurpose),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPressure {
  Unconfigured,
  Normal,
  Soft,
  Hard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct MemoryPolicy {
  pub soft_limit_bytes: u64,
  pub hard_limit_bytes: u64,
  pub host_available_floor_bytes: u64,
  pub emergency_reserve_bytes: u64,
}

impl MemoryPolicy {
  pub fn new(
    soft_limit_bytes: u64,
    hard_limit_bytes: u64,
    host_available_floor_bytes: u64,
    emergency_reserve_bytes: u64,
  ) -> Result<Self, MemoryCoordinatorError> {
    if soft_limit_bytes == 0 {
      return Err(MemoryCoordinatorError::InvalidPolicy("soft limit must be nonzero".to_string()));
    }
    if hard_limit_bytes == 0 {
      return Err(MemoryCoordinatorError::InvalidPolicy("hard limit must be nonzero".to_string()));
    }
    if host_available_floor_bytes == 0 {
      return Err(MemoryCoordinatorError::InvalidPolicy("host available floor must be nonzero".to_string()));
    }
    if emergency_reserve_bytes == 0 || emergency_reserve_bytes >= hard_limit_bytes {
      return Err(MemoryCoordinatorError::InvalidPolicy("emergency reserve must be nonzero and smaller than the hard limit".to_string()));
    }
    let ordinary_limit_bytes = hard_limit_bytes - emergency_reserve_bytes;
    if soft_limit_bytes > ordinary_limit_bytes {
      return Err(MemoryCoordinatorError::InvalidPolicy("soft limit must not exceed the hard limit minus emergency reserve".to_string()));
    }
    Ok(Self { soft_limit_bytes, hard_limit_bytes, host_available_floor_bytes, emergency_reserve_bytes })
  }

  pub const fn ordinary_limit_bytes(self) -> u64 {
    self.hard_limit_bytes - self.emergency_reserve_bytes
  }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct HostMemorySample {
  pub rss_bytes: u64,
  pub private_bytes: Option<u64>,
  pub mapped_bytes: Option<u64>,
  pub allocator_bytes: Option<u64>,
  pub host_available_bytes: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MemoryObservation {
  pub resident_bytes: u64,
  pub clean_bytes: u64,
  pub dirty_bytes: u64,
  pub evictable_bytes: u64,
  pub pinned_bytes: u64,
  pub spill_bytes: u64,
  pub items: u64,
  pub hits: u64,
  pub misses: u64,
  pub evictions: u64,
}

impl MemoryObservation {
  fn validate(&self, owner: MemoryOwner) -> Result<(), MemoryCoordinatorError> {
    if self.clean_bytes.checked_add(self.dirty_bytes).is_none_or(|classified| classified > self.resident_bytes) {
      return Err(MemoryCoordinatorError::InvalidObservation {
        owner,
        message: "clean plus dirty bytes exceed resident bytes".to_string(),
      });
    }
    if self.evictable_bytes > self.clean_bytes {
      return Err(MemoryCoordinatorError::InvalidObservation { owner, message: "evictable bytes exceed clean bytes".to_string() });
    }
    if self.pinned_bytes > self.resident_bytes {
      return Err(MemoryCoordinatorError::InvalidObservation { owner, message: "pinned bytes exceed resident bytes".to_string() });
    }
    Ok(())
  }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MemoryOwnerSnapshot {
  pub owner: MemoryOwner,
  pub observed: MemoryObservation,
  pub reserved_bytes: u64,
  pub critical_reserved_bytes: u64,
  pub peak_reserved_bytes: u64,
  pub active_reservations: u64,
  pub rejections: u64,
  pub deferrals: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MemoryCoordinatorSnapshot {
  pub policy: Option<MemoryPolicy>,
  pub policy_error: Option<String>,
  pub host: HostMemorySample,
  pub pressure: MemoryPressure,
  pub maintenance_paused: bool,
  pub observed_bytes: u64,
  pub reserved_bytes: u64,
  pub critical_reserved_bytes: u64,
  pub accounted_bytes: u64,
  pub unaccounted_rss_bytes: u64,
  pub rejected_reservations: u64,
  pub deferred_reservations: u64,
  pub owners: Vec<MemoryOwnerSnapshot>,
}

impl MemoryCoordinatorSnapshot {
  pub fn owner(&self, owner: MemoryOwner) -> Option<&MemoryOwnerSnapshot> {
    self.owners.iter().find(|snapshot| snapshot.owner == owner)
  }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MemoryCoordinatorError {
  #[error("invalid memory policy: {0}")]
  InvalidPolicy(String),
  #[error("memory policy is unavailable")]
  PolicyUnavailable,
  #[error("invalid observation for {owner:?}: {message}")]
  InvalidObservation { owner: MemoryOwner, message: String },
  #[error("critical purpose {purpose:?} is not valid for owner {owner:?}")]
  InvalidCriticalOwner { owner: MemoryOwner, purpose: CriticalMemoryPurpose },
  #[error(
    "memory request for {owner:?} exceeds ordinary limit: requested={requested_bytes}, accounted={accounted_bytes}, ordinary_limit={ordinary_limit_bytes}"
  )]
  HardLimitExceeded { owner: MemoryOwner, requested_bytes: u64, accounted_bytes: u64, ordinary_limit_bytes: u64 },
  #[error(
    "memory request for {owner:?} exceeds emergency reserve: requested={requested_bytes}, critical_reserved={critical_reserved_bytes}, emergency_reserve={emergency_reserve_bytes}"
  )]
  EmergencyReserveExceeded { owner: MemoryOwner, requested_bytes: u64, critical_reserved_bytes: u64, emergency_reserve_bytes: u64 },
  #[error("memory request for {owner:?} deferred by {pressure:?} pressure")]
  SoftPressureDeferred { owner: MemoryOwner, pressure: MemoryPressure },
  #[error("memory accounting overflow for {owner:?}")]
  AccountingOverflow { owner: MemoryOwner },
  #[error("cannot shrink {owner:?} reservation by {requested_bytes} bytes when it holds {reserved_bytes}")]
  InvalidShrink { owner: MemoryOwner, requested_bytes: u64, reserved_bytes: u64 },
  #[error("memory accounting invariant failed for {owner:?}: {message}")]
  AccountingInvariant { owner: MemoryOwner, message: String },
  #[error("memory coordinator lock is poisoned")]
  Poisoned,
  #[error("cannot observe {owner:?} memory: {message}")]
  ObservationFailed { owner: MemoryOwner, message: String },
}

#[derive(Default)]
struct OwnerState {
  observed: MemoryObservation,
  reserved_bytes: u64,
  critical_reserved_bytes: u64,
  peak_reserved_bytes: u64,
  active_reservations: u64,
  rejections: u64,
  deferrals: u64,
}

struct CoordinatorState {
  policy: Option<MemoryPolicy>,
  policy_error: Option<String>,
  host: HostMemorySample,
  owners: BTreeMap<MemoryOwner, OwnerState>,
  rejected_reservations: u64,
  deferred_reservations: u64,
}

impl CoordinatorState {
  fn new(policy: Option<MemoryPolicy>, policy_error: Option<String>) -> Self {
    let owners = MemoryOwner::ALL.into_iter().map(|owner| (owner, OwnerState::default())).collect();
    Self { policy, policy_error, host: HostMemorySample::default(), owners, rejected_reservations: 0, deferred_reservations: 0 }
  }

  fn totals(&self) -> Result<(u64, u64, u64), MemoryCoordinatorError> {
    let mut observed = 0u64;
    let mut reserved = 0u64;
    let mut critical = 0u64;
    for (owner, state) in &self.owners {
      observed = observed.checked_add(state.observed.resident_bytes).ok_or(MemoryCoordinatorError::AccountingOverflow { owner: *owner })?;
      reserved = reserved.checked_add(state.reserved_bytes).ok_or(MemoryCoordinatorError::AccountingOverflow { owner: *owner })?;
      critical = critical.checked_add(state.critical_reserved_bytes).ok_or(MemoryCoordinatorError::AccountingOverflow { owner: *owner })?;
    }
    Ok((observed, reserved, critical))
  }

  fn pressure(&self, accounted_bytes: u64) -> MemoryPressure {
    let Some(policy) = self.policy else {
      return MemoryPressure::Unconfigured;
    };
    if accounted_bytes >= policy.hard_limit_bytes || self.host.rss_bytes >= policy.hard_limit_bytes {
      return MemoryPressure::Hard;
    }
    let host_below_floor = self.host.host_available_bytes.is_some_and(|available| available < policy.host_available_floor_bytes);
    if accounted_bytes >= policy.soft_limit_bytes || self.host.rss_bytes >= policy.soft_limit_bytes || host_below_floor {
      MemoryPressure::Soft
    } else {
      MemoryPressure::Normal
    }
  }
}

struct MemoryCoordinatorInner {
  state: Mutex<CoordinatorState>,
}

#[derive(Clone)]
pub struct MemoryCoordinator {
  inner: Arc<MemoryCoordinatorInner>,
}

impl MemoryCoordinator {
  pub fn new(policy: MemoryPolicy) -> Self {
    Self { inner: Arc::new(MemoryCoordinatorInner { state: Mutex::new(CoordinatorState::new(Some(policy), None)) }) }
  }

  pub fn without_policy() -> Self {
    Self::without_policy_reason("memory policy was not resolved")
  }

  pub fn without_policy_reason(reason: impl Into<String>) -> Self {
    Self { inner: Arc::new(MemoryCoordinatorInner { state: Mutex::new(CoordinatorState::new(None, Some(reason.into()))) }) }
  }

  fn lock(&self) -> Result<MutexGuard<'_, CoordinatorState>, MemoryCoordinatorError> {
    self.inner.state.lock().map_err(|_| MemoryCoordinatorError::Poisoned)
  }

  pub fn observe_legacy(&self, owner: MemoryOwner, observation: MemoryObservation) -> Result<(), MemoryCoordinatorError> {
    observation.validate(owner)?;
    self.lock()?.owners.get_mut(&owner).expect("fixed memory owner exists").observed = observation;
    Ok(())
  }

  pub fn update_host_sample(&self, sample: HostMemorySample) -> Result<(), MemoryCoordinatorError> {
    self.lock()?.host = sample;
    Ok(())
  }

  pub fn reserve(
    &self,
    owner: MemoryOwner,
    requested_bytes: u64,
    class: AdmissionClass,
  ) -> Result<MemoryReservation, MemoryCoordinatorError> {
    self.admit_bytes(owner, requested_bytes, class, true)?;
    Ok(MemoryReservation { coordinator: self.clone(), owner, class, bytes: requested_bytes, released: false })
  }

  fn admit_bytes(
    &self,
    owner: MemoryOwner,
    requested_bytes: u64,
    class: AdmissionClass,
    new_reservation: bool,
  ) -> Result<(), MemoryCoordinatorError> {
    let mut state = self.lock()?;
    let policy = state.policy.ok_or(MemoryCoordinatorError::PolicyUnavailable)?;
    let (observed, reserved, critical_reserved) = state.totals()?;
    let accounted = observed.checked_add(reserved).ok_or(MemoryCoordinatorError::AccountingOverflow { owner })?;
    let pressure = state.pressure(accounted);

    if let AdmissionClass::Critical(purpose) = class {
      if !critical_owner_matches(owner, purpose) {
        return Err(MemoryCoordinatorError::InvalidCriticalOwner { owner, purpose });
      }
      let projected_critical =
        critical_reserved.checked_add(requested_bytes).ok_or(MemoryCoordinatorError::AccountingOverflow { owner })?;
      if projected_critical > policy.emergency_reserve_bytes {
        let owner_state = state.owners.get_mut(&owner).expect("fixed memory owner exists");
        owner_state.rejections = owner_state.rejections.saturating_add(1);
        state.rejected_reservations = state.rejected_reservations.saturating_add(1);
        return Err(MemoryCoordinatorError::EmergencyReserveExceeded {
          owner,
          requested_bytes,
          critical_reserved_bytes: critical_reserved,
          emergency_reserve_bytes: policy.emergency_reserve_bytes,
        });
      }
    } else {
      let projected = accounted.checked_add(requested_bytes).ok_or(MemoryCoordinatorError::AccountingOverflow { owner })?;
      if projected > policy.ordinary_limit_bytes() || state.host.rss_bytes >= policy.hard_limit_bytes {
        let owner_state = state.owners.get_mut(&owner).expect("fixed memory owner exists");
        owner_state.rejections = owner_state.rejections.saturating_add(1);
        state.rejected_reservations = state.rejected_reservations.saturating_add(1);
        return Err(MemoryCoordinatorError::HardLimitExceeded {
          owner,
          requested_bytes,
          accounted_bytes: accounted,
          ordinary_limit_bytes: policy.ordinary_limit_bytes(),
        });
      }
      if matches!(class, AdmissionClass::Cache | AdmissionClass::Maintenance)
        && (pressure != MemoryPressure::Normal || projected >= policy.soft_limit_bytes)
      {
        let owner_state = state.owners.get_mut(&owner).expect("fixed memory owner exists");
        owner_state.deferrals = owner_state.deferrals.saturating_add(1);
        state.deferred_reservations = state.deferred_reservations.saturating_add(1);
        return Err(MemoryCoordinatorError::SoftPressureDeferred {
          owner,
          pressure: if pressure == MemoryPressure::Normal { MemoryPressure::Soft } else { pressure },
        });
      }
    }

    let owner_state = state.owners.get_mut(&owner).expect("fixed memory owner exists");
    let next_reserved =
      owner_state.reserved_bytes.checked_add(requested_bytes).ok_or(MemoryCoordinatorError::AccountingOverflow { owner })?;
    let next_critical = if matches!(class, AdmissionClass::Critical(_)) {
      owner_state.critical_reserved_bytes.checked_add(requested_bytes).ok_or(MemoryCoordinatorError::AccountingOverflow { owner })?
    } else {
      owner_state.critical_reserved_bytes
    };
    let next_active = if new_reservation {
      owner_state.active_reservations.checked_add(1).ok_or(MemoryCoordinatorError::AccountingOverflow { owner })?
    } else {
      owner_state.active_reservations
    };
    owner_state.reserved_bytes = next_reserved;
    owner_state.critical_reserved_bytes = next_critical;
    owner_state.peak_reserved_bytes = owner_state.peak_reserved_bytes.max(next_reserved);
    owner_state.active_reservations = next_active;
    Ok(())
  }

  fn release_bytes(
    &self,
    owner: MemoryOwner,
    bytes: u64,
    class: AdmissionClass,
    retire_reservation: bool,
  ) -> Result<(), MemoryCoordinatorError> {
    let mut state = self.lock()?;
    let owner_state = state.owners.get_mut(&owner).expect("fixed memory owner exists");
    let next_reserved = owner_state.reserved_bytes.checked_sub(bytes).ok_or_else(|| MemoryCoordinatorError::AccountingInvariant {
      owner,
      message: format!("release of {bytes} exceeds {} reserved bytes", owner_state.reserved_bytes),
    })?;
    let next_critical = if matches!(class, AdmissionClass::Critical(_)) {
      owner_state.critical_reserved_bytes.checked_sub(bytes).ok_or_else(|| MemoryCoordinatorError::AccountingInvariant {
        owner,
        message: format!("critical release of {bytes} exceeds {} critical bytes", owner_state.critical_reserved_bytes),
      })?
    } else {
      owner_state.critical_reserved_bytes
    };
    let next_active = if retire_reservation {
      owner_state
        .active_reservations
        .checked_sub(1)
        .ok_or_else(|| MemoryCoordinatorError::AccountingInvariant { owner, message: "reservation count underflow".to_string() })?
    } else {
      owner_state.active_reservations
    };
    owner_state.reserved_bytes = next_reserved;
    owner_state.critical_reserved_bytes = next_critical;
    owner_state.active_reservations = next_active;
    Ok(())
  }

  pub fn snapshot(&self) -> Result<MemoryCoordinatorSnapshot, MemoryCoordinatorError> {
    let state = self.lock()?;
    let (observed_bytes, reserved_bytes, critical_reserved_bytes) = state.totals()?;
    let accounted_bytes =
      observed_bytes.checked_add(reserved_bytes).ok_or(MemoryCoordinatorError::AccountingOverflow { owner: MemoryOwner::HealthStatus })?;
    let pressure = state.pressure(accounted_bytes);
    let owners = MemoryOwner::ALL
      .into_iter()
      .map(|owner| {
        let current = state.owners.get(&owner).expect("fixed memory owner exists");
        MemoryOwnerSnapshot {
          owner,
          observed: current.observed.clone(),
          reserved_bytes: current.reserved_bytes,
          critical_reserved_bytes: current.critical_reserved_bytes,
          peak_reserved_bytes: current.peak_reserved_bytes,
          active_reservations: current.active_reservations,
          rejections: current.rejections,
          deferrals: current.deferrals,
        }
      })
      .collect();
    Ok(MemoryCoordinatorSnapshot {
      policy: state.policy,
      policy_error: state.policy_error.clone(),
      host: state.host,
      pressure,
      maintenance_paused: matches!(pressure, MemoryPressure::Soft | MemoryPressure::Hard),
      observed_bytes,
      reserved_bytes,
      critical_reserved_bytes,
      accounted_bytes,
      unaccounted_rss_bytes: state.host.rss_bytes.saturating_sub(accounted_bytes),
      rejected_reservations: state.rejected_reservations,
      deferred_reservations: state.deferred_reservations,
      owners,
    })
  }
}

fn critical_owner_matches(owner: MemoryOwner, purpose: CriticalMemoryPurpose) -> bool {
  matches!(
    (owner, purpose),
    (MemoryOwner::KvWriteBuffers | MemoryOwner::DurabilityWaiters | MemoryOwner::IndexDirtyBuffers, CriticalMemoryPurpose::DurableWrite)
      | (MemoryOwner::StreamingRead, CriticalMemoryPurpose::StreamingRead)
      | (MemoryOwner::HealthStatus, CriticalMemoryPurpose::HealthStatus)
      | (MemoryOwner::EmergencySpill, CriticalMemoryPurpose::EmergencySpill)
      | (MemoryOwner::Shutdown, CriticalMemoryPurpose::Shutdown)
      | (MemoryOwner::Repair, CriticalMemoryPurpose::BoundedRecovery)
  )
}

pub struct MemoryReservation {
  coordinator: MemoryCoordinator,
  owner: MemoryOwner,
  class: AdmissionClass,
  bytes: u64,
  released: bool,
}

impl MemoryReservation {
  pub const fn owner(&self) -> MemoryOwner {
    self.owner
  }

  pub const fn bytes(&self) -> u64 {
    self.bytes
  }

  pub fn grow(&mut self, additional_bytes: u64) -> Result<(), MemoryCoordinatorError> {
    self.coordinator.admit_bytes(self.owner, additional_bytes, self.class, false)?;
    self.bytes = self.bytes.checked_add(additional_bytes).ok_or(MemoryCoordinatorError::AccountingOverflow { owner: self.owner })?;
    Ok(())
  }

  pub fn shrink(&mut self, bytes: u64) -> Result<(), MemoryCoordinatorError> {
    if bytes > self.bytes {
      return Err(MemoryCoordinatorError::InvalidShrink { owner: self.owner, requested_bytes: bytes, reserved_bytes: self.bytes });
    }
    self.coordinator.release_bytes(self.owner, bytes, self.class, false)?;
    self.bytes -= bytes;
    Ok(())
  }

  pub fn release(mut self) -> Result<(), MemoryCoordinatorError> {
    self.release_inner()
  }

  fn release_inner(&mut self) -> Result<(), MemoryCoordinatorError> {
    if self.released {
      return Ok(());
    }
    self.coordinator.release_bytes(self.owner, self.bytes, self.class, true)?;
    self.bytes = 0;
    self.released = true;
    Ok(())
  }
}

impl Drop for MemoryReservation {
  fn drop(&mut self) {
    if let Err(error) = self.release_inner() {
      tracing::error!(owner = self.owner.as_str(), %error, "Memory reservation release failed");
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn poisoned_state_is_never_reported_as_an_empty_success() {
    let coordinator = MemoryCoordinator::new(MemoryPolicy::new(600, 800, 200, 100).unwrap());
    let inner = Arc::clone(&coordinator.inner);
    let _ = std::panic::catch_unwind(move || {
      let _guard = inner.state.lock().unwrap();
      panic!("poison coordinator state");
    });

    assert_eq!(coordinator.snapshot().unwrap_err(), MemoryCoordinatorError::Poisoned);
    assert!(matches!(coordinator.reserve(MemoryOwner::Query, 1, AdmissionClass::Workload), Err(MemoryCoordinatorError::Poisoned)));
  }
}
