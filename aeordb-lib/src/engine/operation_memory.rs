use tokio_util::sync::CancellationToken;

use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::storage_engine::StorageEngine;

/// One reservation and cancellation boundary for a material engine operation.
/// Callers grow it before allocation and shrink transient charges after use.
pub(crate) struct OperationMemoryBudget {
  operation: &'static str,
  reservation: MemoryReservation,
  cancellation: Option<CancellationToken>,
  work_since_cancellation_check: usize,
}

impl OperationMemoryBudget {
  pub(crate) fn new(
    engine: &StorageEngine,
    operation: &'static str,
    owner: MemoryOwner,
    class: AdmissionClass,
    minimum_workspace_bytes: u64,
    cancellation: Option<&CancellationToken>,
  ) -> EngineResult<Self> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
      return Err(EngineError::Cancelled(operation.to_string()));
    }
    let reservation = engine
      .memory_coordinator()
      .reserve(owner, minimum_workspace_bytes, class)
      .map_err(|error| operation_memory_error(operation, "workspace admission failed", error))?;
    Ok(Self { operation, reservation, cancellation: cancellation.cloned(), work_since_cancellation_check: 0 })
  }

  pub(crate) fn reserve(&mut self, bytes: u64, context: &'static str) -> EngineResult<()> {
    if bytes == 0 {
      return Ok(());
    }
    self.reservation.grow(bytes).map_err(|error| operation_memory_error(self.operation, context, error))
  }

  pub(crate) fn release(&mut self, bytes: u64, context: &'static str) -> EngineResult<()> {
    if bytes == 0 {
      return Ok(());
    }
    self.reservation.shrink(bytes).map_err(|error| operation_memory_error(self.operation, context, error))
  }

  pub(crate) fn checkpoint(&self) -> u64 {
    self.reservation.bytes()
  }

  pub(crate) fn release_to(&mut self, checkpoint: u64, context: &'static str) -> EngineResult<()> {
    let current = self.reservation.bytes();
    let bytes = current.checked_sub(checkpoint).ok_or_else(|| {
      EngineError::IoError(std::io::Error::other(format!(
        "{} memory checkpoint {} exceeds current reservation {}",
        self.operation, checkpoint, current
      )))
    })?;
    self.release(bytes, context)
  }

  pub(crate) fn check_cancellation(&self) -> EngineResult<()> {
    if self.cancellation.as_ref().is_some_and(CancellationToken::is_cancelled) {
      return Err(EngineError::Cancelled(self.operation.to_string()));
    }
    Ok(())
  }

  pub(crate) fn record_work(&mut self, units: usize) -> EngineResult<()> {
    const CANCELLATION_QUANTUM: usize = 128;
    self.work_since_cancellation_check = self.work_since_cancellation_check.saturating_add(units);
    if self.work_since_cancellation_check >= CANCELLATION_QUANTUM {
      self.work_since_cancellation_check = 0;
      self.check_cancellation()?;
    }
    Ok(())
  }
}

fn operation_memory_error(operation: &str, context: &str, error: MemoryCoordinatorError) -> EngineError {
  match error {
    MemoryCoordinatorError::PolicyUnavailable
    | MemoryCoordinatorError::HardLimitExceeded { .. }
    | MemoryCoordinatorError::SoftPressureDeferred { .. }
    | MemoryCoordinatorError::EmergencyReserveExceeded { .. } => EngineError::ResourceExhausted(format!("{operation} {context}: {error}")),
    _ => EngineError::IoError(std::io::Error::other(format!("{operation} {context}: {error}"))),
  }
}
