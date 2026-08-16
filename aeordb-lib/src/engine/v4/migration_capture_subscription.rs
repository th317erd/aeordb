//! Optional, independently bounded source-publication capture for migration.
//!
//! This is a recoverable-soft acceleration path. The acknowledged source
//! mutation and the primary index-runtime handoff never depend on this queue.

use std::sync::Arc;

#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

use super::coverage_runtime::{
  SoftMutationAdmissionV1, SoftMutationDrainV1, SoftMutationHubErrorV1, SoftMutationHubSnapshotV1, SoftMutationHubV1,
  SoftMutationLossReasonV1,
};
use crate::engine::errors::EngineError;
use crate::engine::namespace_mutation::NamespaceMutationAcknowledgement;
use crate::engine::storage_engine::StorageEngine;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationCaptureSubscriptionIdentityV1 {
  pub migration_id: [u8; 16],
  pub holder_boot_id: [u8; 16],
  pub fencing_token: u64,
  pub registration_id: [u8; 16],
}

impl MigrationCaptureSubscriptionIdentityV1 {
  pub fn new(
    migration_id: [u8; 16],
    holder_boot_id: [u8; 16],
    fencing_token: u64,
    registration_id: [u8; 16],
  ) -> Result<Self, MigrationCaptureSubscriptionErrorV1> {
    let identity = Self { migration_id, holder_boot_id, fencing_token, registration_id };
    identity.validate()?;
    Ok(identity)
  }

  pub(crate) fn validate(self) -> Result<(), MigrationCaptureSubscriptionErrorV1> {
    if self.migration_id == [0; 16] || self.holder_boot_id == [0; 16] || self.registration_id == [0; 16] {
      return Err(MigrationCaptureSubscriptionErrorV1::Invalid {
        code: "migration_capture_subscription_identity",
        message: "migration, holder boot, and unique registration identities must be nonzero".to_string(),
      });
    }
    if self.fencing_token == 0 {
      return Err(MigrationCaptureSubscriptionErrorV1::Invalid {
        code: "migration_capture_subscription_fence",
        message: "migration capture fencing token must be nonzero".to_string(),
      });
    }
    Ok(())
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationCaptureSubscriptionBoundaryV1 {
  pub source_namespace_root: Vec<u8>,
  pub publication_sequence: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationCaptureOfferV1 {
  IgnoredAtOrBeforeBoundary,
  Offered(SoftMutationAdmissionV1),
}

#[derive(Debug)]
pub struct MigrationCaptureSubscriptionV1 {
  identity: MigrationCaptureSubscriptionIdentityV1,
  boundary: MigrationCaptureSubscriptionBoundaryV1,
  hub: Arc<SoftMutationHubV1>,
  #[cfg(test)]
  panic_on_next_offer: AtomicBool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct MigrationCaptureSubscriptionOwnerV1 {
  identity: MigrationCaptureSubscriptionIdentityV1,
}

impl MigrationCaptureSubscriptionOwnerV1 {
  /// Install a caller-budgeted hub at one exact source boundary.
  ///
  /// The caller must reserve the modeled hub capacity under
  /// `MemoryOwner::Migration` before constructing the hub and retain that
  /// reservation for at least as long as this registration and its retired
  /// drain handle. The capture runtime owner introduced by the next landing
  /// owns that reservation; this handoff does not allocate an implicit queue.
  pub fn register(
    source: &StorageEngine,
    identity: MigrationCaptureSubscriptionIdentityV1,
    hub: Arc<SoftMutationHubV1>,
  ) -> Result<(Self, Arc<MigrationCaptureSubscriptionV1>), MigrationCaptureSubscriptionErrorV1> {
    identity.validate()?;
    let subscription = source.register_migration_capture_subscription(identity, hub)?;
    Ok((Self { identity }, subscription))
  }

  pub fn unregister(&self, source: &StorageEngine) -> Result<RetiredMigrationCaptureSubscriptionV1, MigrationCaptureSubscriptionErrorV1> {
    source.unregister_migration_capture_subscription(self.identity)
  }

  pub const fn identity(&self) -> MigrationCaptureSubscriptionIdentityV1 {
    self.identity
  }
}

impl MigrationCaptureSubscriptionV1 {
  pub fn new(
    identity: MigrationCaptureSubscriptionIdentityV1,
    boundary: MigrationCaptureSubscriptionBoundaryV1,
    hub: Arc<SoftMutationHubV1>,
  ) -> Result<Self, MigrationCaptureSubscriptionErrorV1> {
    identity.validate()?;
    if Arc::strong_count(&hub) != 1 {
      return Err(MigrationCaptureSubscriptionErrorV1::HubNotExclusive);
    }
    let snapshot = hub.snapshot().map_err(MigrationCaptureSubscriptionErrorV1::Hub)?;
    if snapshot.admission_closed
      || snapshot.queued_notices != 0
      || snapshot.retained_bytes != 0
      || snapshot.latest_queued_publication_sequence.is_some()
      || snapshot.reconciliation_required
      || snapshot.lost_through_sequence.is_some()
      || !snapshot.loss_reasons.is_empty()
      || snapshot.dropped_notices != 0
      || snapshot.loss_epoch != 0
      || snapshot.reconciled_loss_epoch != 0
      || snapshot.losses_in_flight != 0
    {
      return Err(MigrationCaptureSubscriptionErrorV1::HubNotPristine);
    }
    Ok(Self {
      identity,
      boundary,
      hub,
      #[cfg(test)]
      panic_on_next_offer: AtomicBool::new(false),
    })
  }

  pub const fn identity(&self) -> MigrationCaptureSubscriptionIdentityV1 {
    self.identity
  }

  pub const fn boundary(&self) -> &MigrationCaptureSubscriptionBoundaryV1 {
    &self.boundary
  }

  pub fn offer_acknowledgement(&self, acknowledgement: &NamespaceMutationAcknowledgement) -> MigrationCaptureOfferV1 {
    #[cfg(test)]
    if self.panic_on_next_offer.swap(false, Ordering::AcqRel) {
      std::panic::resume_unwind(Box::new("injected migration capture subscription panic"));
    }
    if acknowledgement.publication_sequence == 0 {
      return MigrationCaptureOfferV1::Offered(self.hub.offer_acknowledgement(acknowledgement));
    }
    if acknowledgement.publication_sequence <= self.boundary.publication_sequence {
      return MigrationCaptureOfferV1::IgnoredAtOrBeforeBoundary;
    }
    MigrationCaptureOfferV1::Offered(self.hub.offer_acknowledgement(acknowledgement))
  }

  pub fn snapshot(&self) -> Result<SoftMutationHubSnapshotV1, SoftMutationHubErrorV1> {
    self.hub.snapshot()
  }

  pub fn try_drain(&self, maximum_notices: usize, maximum_bytes: usize) -> Result<SoftMutationDrainV1, SoftMutationHubErrorV1> {
    self.hub.try_drain(maximum_notices, maximum_bytes)
  }

  pub(crate) fn close_admission(&self) -> Result<(), SoftMutationHubErrorV1> {
    self.hub.close_admission()
  }

  pub(crate) fn force_reconciliation_required(&self, publication_sequence: u64) -> SoftMutationAdmissionV1 {
    self.hub.force_reconciliation_required(publication_sequence, SoftMutationLossReasonV1::QueueUnavailable)
  }

  #[cfg(test)]
  pub(crate) fn panic_on_next_offer_for_test(&self) {
    self.panic_on_next_offer.store(true, Ordering::Release);
  }

  #[cfg(test)]
  pub(crate) fn lock_queue_for_test(&self) -> Result<super::coverage_runtime::SoftMutationQueueTestGuardV1<'_>, SoftMutationHubErrorV1> {
    self.hub.lock_queue_for_test()
  }
}

#[derive(Debug)]
pub struct RetiredMigrationCaptureSubscriptionV1 {
  pub(crate) subscription: Arc<MigrationCaptureSubscriptionV1>,
  pub(crate) close_error: Option<SoftMutationHubErrorV1>,
}

impl RetiredMigrationCaptureSubscriptionV1 {
  pub fn subscription(&self) -> &Arc<MigrationCaptureSubscriptionV1> {
    &self.subscription
  }

  pub const fn close_error(&self) -> Option<&SoftMutationHubErrorV1> {
    self.close_error.as_ref()
  }
}

#[derive(Debug, thiserror::Error)]
pub enum MigrationCaptureSubscriptionErrorV1 {
  #[error("{code}: {message}")]
  Invalid { code: &'static str, message: String },
  #[error("migration capture subscription requires an unused, open hub")]
  HubNotPristine,
  #[error("migration capture subscription requires exclusive ownership of its hub")]
  HubNotExclusive,
  #[error("migration capture subscription hub failed: {0}")]
  Hub(SoftMutationHubErrorV1),
  #[error("a migration capture subscription is already registered")]
  AlreadyRegistered,
  #[error("no migration capture subscription is registered")]
  NotRegistered,
  #[error("migration capture subscription is owned by a different registration token")]
  OwnerFenced,
  #[error("migration capture subscription engine operation failed: {0}")]
  Engine(EngineError),
}

impl MigrationCaptureSubscriptionErrorV1 {
  pub const fn code(&self) -> &'static str {
    match self {
      Self::Invalid { code, .. } => code,
      Self::HubNotPristine => "migration_capture_subscription_hub_state",
      Self::HubNotExclusive => "migration_capture_subscription_hub_ownership",
      Self::Hub(_) => "migration_capture_subscription_hub",
      Self::AlreadyRegistered => "migration_capture_subscription_already_registered",
      Self::NotRegistered => "migration_capture_subscription_not_registered",
      Self::OwnerFenced => "migration_capture_subscription_owner_fenced",
      Self::Engine(_) => "migration_capture_subscription_engine",
    }
  }
}
