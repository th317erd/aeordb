//! In-process protection for the interval before a staged checkpoint is selected.
//! This guard is not namespace admission or durable task retention.
use super::*;

#[derive(Debug, thiserror::Error)]
pub enum StagingProtectionErrorV1 {
  #[error("native staging protection was cancelled")]
  Cancelled,
  #[error("native staging protection is unavailable: {0}")]
  Unavailable(&'static str),
  #[error(transparent)]
  Authority(#[from] FirstAuthorityPublicationErrorV1),
  #[error(transparent)]
  Memory(#[from] MemoryCoordinatorError),
}

impl StagingProtectionErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Cancelled => "staging_protection_cancelled",
      Self::Unavailable(_) => "staging_protection_unavailable",
      Self::Authority(source) => source.code(),
      Self::Memory(_) => "staging_protection_memory",
    }
  }
}

#[must_use = "retain staging protection until checkpoint selection or safe discard"]
pub struct NativeStagingProtectionV1<'publisher> {
  publisher: &'publisher V4FirstAuthorityPublisher,
  _memory: MemoryReservation,
}

impl fmt::Debug for NativeStagingProtectionV1<'_> {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("NativeStagingProtectionV1").finish_non_exhaustive()
  }
}

impl<'publisher> NativeStagingProtectionV1<'publisher> {
  pub(crate) fn publisher(&self) -> &'publisher V4FirstAuthorityPublisher {
    self.publisher
  }
}

impl V4FirstAuthorityPublisher {
  /// Protect in-process unselected work without holding the publication mutex
  /// across compilation. Acquire before capturing or publishing the protected
  /// dependencies; release only after checkpoint selection or safe discard.
  /// Reopening a file does not recover this process-local protection.
  pub fn acquire_staging_protection(
    &self,
    memory: &MemoryCoordinator,
    cancellation: &CancellationToken,
  ) -> Result<NativeStagingProtectionV1<'_>, StagingProtectionErrorV1> {
    if cancellation.is_cancelled() {
      return Err(StagingProtectionErrorV1::Cancelled);
    }
    let bytes = (std::mem::size_of::<NativeStagingProtectionV1<'_>>() as u64)
      .checked_add(128)
      .ok_or(StagingProtectionErrorV1::Unavailable("staging protection memory bound overflowed"))?;
    let reservation = memory.reserve(MemoryOwner::Task, bytes, AdmissionClass::Workload)?;
    let mut authority = self.root_state.lock().map_err(|poisoned| {
      drop(poisoned);
      FirstAuthorityPublicationErrorV1::StateLockPoisoned
    })?;
    if cancellation.is_cancelled() {
      return Err(StagingProtectionErrorV1::Cancelled);
    }
    reservation.check_admission()?;
    if authority.staging_accounting_failed {
      return Err(StagingProtectionErrorV1::Unavailable("staging protection accounting previously failed"));
    }
    authority.active_staging_protections = authority
      .active_staging_protections
      .checked_add(1)
      .ok_or(StagingProtectionErrorV1::Unavailable("staging protection count exhausted"))?;
    drop(authority);
    Ok(NativeStagingProtectionV1 { publisher: self, _memory: reservation })
  }
}

impl FirstAuthorityRootStateV1 {
  pub(super) fn ensure_no_staging_protection(&self) -> Result<(), FirstAuthorityPublicationErrorV1> {
    if self.staging_accounting_failed {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "staging_protection_accounting",
        "staging protection accounting failed; reclamation cannot proceed",
      ));
    }
    if self.active_staging_protections != 0 {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "staging_protection_active",
        "unselected objects have active in-process staging protection",
      ));
    }
    Ok(())
  }
}

impl Drop for NativeStagingProtectionV1<'_> {
  fn drop(&mut self) {
    let mut authority = match self.publisher.root_state.lock() {
      Ok(authority) => authority,
      Err(poisoned) => {
        drop(poisoned);
        // Every publication already rejects this same poisoned owner mutex.
        tracing::error!("staging protection release failed: physical authority mutex is poisoned");
        return;
      }
    };
    match authority.active_staging_protections.checked_sub(1) {
      Some(remaining) => authority.active_staging_protections = remaining,
      None => {
        authority.staging_accounting_failed = true;
        tracing::error!("staging protection release found corrupt accounting; reclamation remains blocked");
      }
    }
  }
}
