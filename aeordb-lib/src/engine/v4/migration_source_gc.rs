//! Source mutating-GC suspension owned by one fenced migration lease.

use std::fmt::{self, Display, Formatter};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};

use super::gc_retirement::RetirementJournalOwnerV1;
use super::migration_owner::{MigrationLeaseReleaseRequestV1, MigrationStateOwnerErrorV1, MigrationStateOwnerV1};
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::native_durability::platform_file_identity;
use crate::engine::storage_engine::StorageEngine;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationSourceGcSuspensionRequestV1 {
  pub suspended_at_ms: i64,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationSourceGcSuspensionReceiptV1 {
  pub progress_control_sequence: u64,
  pub fencing_token: u64,
  pub interlock_idempotent: bool,
  pub progress_idempotent: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationSourceGcReleaseReceiptV1 {
  pub lease_control_sequence: u64,
  pub fencing_token: u64,
  pub interlock_idempotent: bool,
  pub lease_idempotent: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationSourceGcSuspensionOwnerV1 {
  binding: SourceGcSuspensionBindingV1,
}

impl MigrationSourceGcSuspensionOwnerV1 {
  pub fn recover_latched(
    source: &StorageEngine,
    migration_owner: &MigrationStateOwnerV1,
  ) -> Result<Self, MigrationSourceGcSuspensionErrorV1> {
    let binding = validated_source_binding(source, migration_owner)?;
    source.recover_migration_source_gc_suspension(binding)?;
    Ok(Self { binding })
  }

  pub fn reopen_source_suspended(
    source_path: &str,
    migration_owner: &MigrationStateOwnerV1,
    request: MigrationSourceGcSuspensionRequestV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<(StorageEngine, Self, MigrationSourceGcSuspensionReceiptV1), MigrationSourceGcSuspensionErrorV1> {
    let source = StorageEngine::open(source_path)?;
    let (owner, receipt) = Self::suspend(&source, migration_owner, request, retirement_owner)?;
    Ok((source, owner, receipt))
  }

  pub fn suspend(
    source: &StorageEngine,
    migration_owner: &MigrationStateOwnerV1,
    request: MigrationSourceGcSuspensionRequestV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<(Self, MigrationSourceGcSuspensionReceiptV1), MigrationSourceGcSuspensionErrorV1> {
    validate_request(request)?;
    let binding = validated_source_binding(source, migration_owner)?;
    migration_owner.validate_source_gc_suspension_claim(
      request.suspended_at_ms,
      request.publication_timestamp_ms,
      request.monotonic_now_ms,
    )?;

    let interlock = source.activate_migration_source_gc_suspension(binding)?;
    let progress = migration_owner.claim_source_gc_suspension(
      request.suspended_at_ms,
      request.publication_timestamp_ms,
      request.monotonic_now_ms,
      retirement_owner,
    )?;
    Ok((
      Self { binding },
      MigrationSourceGcSuspensionReceiptV1 {
        progress_control_sequence: progress.control_sequence,
        fencing_token: progress.fencing_token,
        interlock_idempotent: interlock.idempotent,
        progress_idempotent: progress.idempotent,
      },
    ))
  }

  pub fn release_after_early_terminal(
    &self,
    source: &StorageEngine,
    migration_owner: &MigrationStateOwnerV1,
    request: MigrationLeaseReleaseRequestV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<MigrationSourceGcReleaseReceiptV1, MigrationSourceGcSuspensionErrorV1> {
    self.validate_owner(migration_owner)?;
    let source_binding = validated_source_binding(source, migration_owner)?;
    if source_binding != self.binding {
      return Err(MigrationSourceGcSuspensionErrorV1::invalid(
        "migration_source_gc_owner_fenced",
        "source GC release no longer matches the selected migration holder and fencing token",
      ));
    }
    let lease = migration_owner.release(request, retirement_owner)?;
    let interlock = source.release_migration_source_gc_suspension(self.binding)?;
    Ok(MigrationSourceGcReleaseReceiptV1 {
      lease_control_sequence: lease.control_sequence,
      fencing_token: lease.fencing_token,
      interlock_idempotent: interlock.idempotent,
      lease_idempotent: lease.idempotent,
    })
  }

  fn validate_owner(&self, owner: &MigrationStateOwnerV1) -> Result<(), MigrationSourceGcSuspensionErrorV1> {
    let selected = SourceGcSuspensionBindingV1 {
      database_id: owner.database_id(),
      migration_id: owner.migration_id(),
      source_physical_instance_id: owner.source_physical_instance_id(),
      holder_boot_id: owner.holder_boot_id(),
      fencing_token: owner.fencing_token(),
    };
    if selected != self.binding {
      return Err(MigrationSourceGcSuspensionErrorV1::invalid(
        "migration_source_gc_owner_fenced",
        "source GC suspension owner no longer matches the selected migration holder and fencing token",
      ));
    }
    Ok(())
  }
}

#[derive(Debug)]
pub enum MigrationSourceGcSuspensionErrorV1 {
  Invalid { code: &'static str, message: String },
  Engine(EngineError),
  Migration(MigrationStateOwnerErrorV1),
}

impl MigrationSourceGcSuspensionErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Invalid { code, .. } => code,
      Self::Engine(EngineError::MigrationGcSuspended { .. }) => "migration_source_gc_suspended",
      Self::Engine(_) => "migration_source_gc_engine",
      Self::Migration(source) => source.code(),
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }
}

impl Display for MigrationSourceGcSuspensionErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Invalid { code, message } => write!(formatter, "{code}: {message}"),
      Self::Engine(source) => write!(formatter, "migration source GC interlock failed: {source}"),
      Self::Migration(source) => write!(formatter, "migration source GC control publication failed: {source}"),
    }
  }
}

impl std::error::Error for MigrationSourceGcSuspensionErrorV1 {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      Self::Engine(source) => Some(source),
      Self::Migration(source) => Some(source),
      Self::Invalid { .. } => None,
    }
  }
}

impl From<EngineError> for MigrationSourceGcSuspensionErrorV1 {
  fn from(source: EngineError) -> Self {
    Self::Engine(source)
  }
}

impl From<MigrationStateOwnerErrorV1> for MigrationSourceGcSuspensionErrorV1 {
  fn from(source: MigrationStateOwnerErrorV1) -> Self {
    Self::Migration(source)
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SourceGcSuspensionBindingV1 {
  pub database_id: [u8; 16],
  pub migration_id: [u8; 16],
  pub source_physical_instance_id: [u8; 16],
  pub holder_boot_id: [u8; 16],
  pub fencing_token: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SourceGcInterlockReceiptV1 {
  pub idempotent: bool,
}

#[derive(Default)]
struct SourceGcInterlockStateV1 {
  selected: Option<SourceGcSuspensionBindingV1>,
  pending: Option<SourceGcSuspensionBindingV1>,
  active_mutations: u64,
}

#[derive(Default)]
pub(crate) struct SourceGcMutationInterlockV1 {
  state: Mutex<SourceGcInterlockStateV1>,
  wake: Condvar,
  poisoned: AtomicBool,
}

impl SourceGcMutationInterlockV1 {
  pub fn admit_mutation(&self) -> EngineResult<SourceGcMutationPermitV1<'_>> {
    if self.poisoned.load(Ordering::Acquire) {
      return Err(EngineError::DurabilityFailure("source GC mutation interlock is poisoned".to_string()));
    }
    let mut state = self.lock_state()?;
    if let Some(binding) = state.selected.or(state.pending) {
      return Err(suspended_error(binding));
    }
    state.active_mutations = state
      .active_mutations
      .checked_add(1)
      .ok_or_else(|| EngineError::ResourceExhausted("source GC mutation permit count overflowed".to_string()))?;
    Ok(SourceGcMutationPermitV1 { interlock: self, active: true })
  }

  pub fn suspend(&self, binding: SourceGcSuspensionBindingV1) -> Result<SourceGcInterlockReceiptV1, MigrationSourceGcSuspensionErrorV1> {
    validate_binding(binding)?;
    if self.poisoned.load(Ordering::Acquire) {
      return Err(MigrationSourceGcSuspensionErrorV1::invalid(
        "migration_source_gc_interlock_poisoned",
        "source GC mutation interlock is poisoned",
      ));
    }
    let mut state = self.lock_state().map_err(MigrationSourceGcSuspensionErrorV1::from)?;
    loop {
      if let Some(selected) = state.selected {
        if selected == binding {
          return Ok(SourceGcInterlockReceiptV1 { idempotent: true });
        }
        validate_rebind(selected, binding)?;
        state.selected = Some(binding);
        return Ok(SourceGcInterlockReceiptV1 { idempotent: false });
      }
      if let Some(pending) = state.pending {
        if pending != binding {
          return Err(MigrationSourceGcSuspensionErrorV1::invalid(
            "migration_source_gc_owner_fenced",
            "another migration holder is already waiting to suspend source GC",
          ));
        }
        state = self.wait_state(state)?;
        continue;
      }
      state.pending = Some(binding);
      while state.active_mutations != 0 {
        state = self.wait_state(state)?;
      }
      state.selected = Some(binding);
      state.pending = None;
      self.wake.notify_all();
      return Ok(SourceGcInterlockReceiptV1 { idempotent: false });
    }
  }

  pub fn release(&self, binding: SourceGcSuspensionBindingV1) -> Result<SourceGcInterlockReceiptV1, MigrationSourceGcSuspensionErrorV1> {
    if self.poisoned.load(Ordering::Acquire) {
      return Err(MigrationSourceGcSuspensionErrorV1::invalid(
        "migration_source_gc_interlock_poisoned",
        "source GC mutation interlock is poisoned",
      ));
    }
    let mut state = self.lock_state().map_err(MigrationSourceGcSuspensionErrorV1::from)?;
    match state.selected {
      Some(selected) if selected == binding => {
        state.selected = None;
        self.wake.notify_all();
        Ok(SourceGcInterlockReceiptV1 { idempotent: false })
      }
      None if state.pending.is_none() => Ok(SourceGcInterlockReceiptV1 { idempotent: true }),
      _ => Err(MigrationSourceGcSuspensionErrorV1::invalid(
        "migration_source_gc_owner_fenced",
        "source GC suspension belongs to another migration holder or fencing token",
      )),
    }
  }

  pub fn recover_latched(
    &self,
    binding: SourceGcSuspensionBindingV1,
  ) -> Result<SourceGcInterlockReceiptV1, MigrationSourceGcSuspensionErrorV1> {
    validate_binding(binding)?;
    if self.poisoned.load(Ordering::Acquire) {
      return Err(MigrationSourceGcSuspensionErrorV1::invalid(
        "migration_source_gc_interlock_poisoned",
        "source GC mutation interlock is poisoned",
      ));
    }
    let mut state = self.lock_state().map_err(MigrationSourceGcSuspensionErrorV1::from)?;
    loop {
      match (state.selected, state.pending) {
        (Some(selected), _) if selected == binding => return Ok(SourceGcInterlockReceiptV1 { idempotent: true }),
        (Some(_), _) => {
          return Err(MigrationSourceGcSuspensionErrorV1::invalid(
            "migration_source_gc_owner_fenced",
            "source GC suspension belongs to another migration holder or fencing token",
          ));
        }
        (None, Some(pending)) if pending == binding => state = self.wait_state(state)?,
        (None, Some(_)) => {
          return Err(MigrationSourceGcSuspensionErrorV1::invalid(
            "migration_source_gc_owner_fenced",
            "another migration holder is waiting to suspend source GC",
          ));
        }
        (None, None) => {
          return Err(MigrationSourceGcSuspensionErrorV1::invalid(
            "migration_source_gc_not_latched",
            "no source GC suspension is latched for this migration holder",
          ));
        }
      }
    }
  }

  fn lock_state(&self) -> EngineResult<MutexGuard<'_, SourceGcInterlockStateV1>> {
    self.state.lock().map_err(|error| {
      self.poisoned.store(true, Ordering::Release);
      EngineError::DurabilityFailure(format!("source GC mutation interlock lock failed: {error}"))
    })
  }

  fn wait_state<'a>(
    &self,
    state: MutexGuard<'a, SourceGcInterlockStateV1>,
  ) -> Result<MutexGuard<'a, SourceGcInterlockStateV1>, MigrationSourceGcSuspensionErrorV1> {
    self.wake.wait(state).map_err(|error| {
      self.poisoned.store(true, Ordering::Release);
      MigrationSourceGcSuspensionErrorV1::invalid(
        "migration_source_gc_interlock_poisoned",
        format!("source GC mutation interlock wait failed: {error}"),
      )
    })
  }
}

pub(crate) struct SourceGcMutationPermitV1<'a> {
  interlock: &'a SourceGcMutationInterlockV1,
  active: bool,
}

impl Drop for SourceGcMutationPermitV1<'_> {
  fn drop(&mut self) {
    if !self.active {
      return;
    }
    match self.interlock.state.lock() {
      Ok(mut state) => {
        if state.active_mutations == 0 {
          self.interlock.poisoned.store(true, Ordering::Release);
          tracing::error!("source GC mutation permit underflow poisoned the interlock");
        } else {
          state.active_mutations -= 1;
          if state.active_mutations == 0 {
            self.interlock.wake.notify_all();
          }
        }
      }
      Err(error) => {
        self.interlock.poisoned.store(true, Ordering::Release);
        tracing::error!(%error, "source GC mutation permit release poisoned the interlock");
      }
    }
    self.active = false;
  }
}

fn validate_binding(binding: SourceGcSuspensionBindingV1) -> Result<(), MigrationSourceGcSuspensionErrorV1> {
  if [binding.database_id, binding.migration_id, binding.source_physical_instance_id, binding.holder_boot_id]
    .iter()
    .any(|value| value.iter().all(|byte| *byte == 0))
    || binding.fencing_token == 0
  {
    return Err(MigrationSourceGcSuspensionErrorV1::invalid(
      "migration_source_gc_binding",
      "source GC suspension identities and fencing token must be nonzero",
    ));
  }
  Ok(())
}

fn validated_source_binding(
  source: &StorageEngine,
  migration_owner: &MigrationStateOwnerV1,
) -> Result<SourceGcSuspensionBindingV1, MigrationSourceGcSuspensionErrorV1> {
  if source.hash_algo() != migration_owner.hash_algorithm() {
    return Err(MigrationSourceGcSuspensionErrorV1::invalid(
      "migration_source_gc_hash_algorithm",
      "opened source hash algorithm does not match the selected migration permit",
    ));
  }
  let actual_identity = platform_file_identity(source.database_path()).map_err(|source| {
    MigrationSourceGcSuspensionErrorV1::invalid(
      "migration_source_gc_file_identity",
      format!("cannot identify the opened migration source: {source}"),
    )
  })?;
  if actual_identity != migration_owner.source_file_identity() {
    return Err(MigrationSourceGcSuspensionErrorV1::invalid(
      "migration_source_gc_file_identity",
      "opened source file identity does not match the selected migration permit",
    ));
  }
  Ok(SourceGcSuspensionBindingV1 {
    database_id: migration_owner.database_id(),
    migration_id: migration_owner.migration_id(),
    source_physical_instance_id: migration_owner.source_physical_instance_id(),
    holder_boot_id: migration_owner.holder_boot_id(),
    fencing_token: migration_owner.fencing_token(),
  })
}

fn validate_request(request: MigrationSourceGcSuspensionRequestV1) -> Result<(), MigrationSourceGcSuspensionErrorV1> {
  if request.suspended_at_ms < 0 || request.publication_timestamp_ms == 0 || request.monotonic_now_ms == 0 {
    return Err(MigrationSourceGcSuspensionErrorV1::invalid(
      "migration_source_gc_times",
      "source GC suspension times must be nonnegative and publication clocks must be nonzero",
    ));
  }
  if request.publication_timestamp_ms > i64::MAX as u64 {
    return Err(MigrationSourceGcSuspensionErrorV1::invalid(
      "migration_source_gc_time_range",
      "source GC suspension publication time exceeds the durable FileRecord time range",
    ));
  }
  if request.publication_timestamp_ms < request.suspended_at_ms as u64 {
    return Err(MigrationSourceGcSuspensionErrorV1::invalid(
      "migration_source_gc_publication_before_suspension",
      "source GC suspension publication time cannot precede its semantic suspension time",
    ));
  }
  Ok(())
}

fn validate_rebind(
  selected: SourceGcSuspensionBindingV1,
  target: SourceGcSuspensionBindingV1,
) -> Result<(), MigrationSourceGcSuspensionErrorV1> {
  if selected.database_id != target.database_id
    || selected.migration_id != target.migration_id
    || selected.source_physical_instance_id != target.source_physical_instance_id
    || target.fencing_token <= selected.fencing_token
  {
    return Err(MigrationSourceGcSuspensionErrorV1::invalid(
      "migration_source_gc_owner_fenced",
      "source GC suspension can only advance to a larger token for the same migration and source",
    ));
  }
  Ok(())
}

fn suspended_error(binding: SourceGcSuspensionBindingV1) -> EngineError {
  EngineError::MigrationGcSuspended { migration_id: binding.migration_id, fencing_token: binding.fencing_token }
}
