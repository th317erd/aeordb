//! Live final-write freeze and exact source-authority capture for v3-to-v4 migration.
//!
//! A durable migration flag records history; it is never treated as a live
//! process lock after restart. The nonconstructible owner below must remain
//! alive through reconciliation and later destination verification.

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use super::migration_preflight::MigrationPreflightPermitV1;
use super::reader::FormatError;
use super::system_family::embedded_system_family_registry;
use crate::engine::native_durability::{PlatformFileIdentityDescriptorV1, platform_file_identity};
use crate::engine::storage_engine::{ExclusiveReadOnlyEngineMaintenanceGuard, FrozenSourceAuthoritySnapshot, StorageEngine};
use crate::engine::{EngineError, HashAlgorithm};

const MAXIMUM_FREEZE_ACQUISITION_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

pub struct MigrationSourceWriteFreezeRequestV1<'a> {
  pub permit: &'a MigrationPreflightPermitV1,
  pub source: &'a StorageEngine,
  pub cancellation: &'a CancellationToken,
  pub acquisition_timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenMigrationSourceAuthorityV1 {
  pub physical_identity: PlatformFileIdentityDescriptorV1,
  pub header_sequence: u64,
  pub namespace_root: Vec<u8>,
  pub hard_publication_frontier: u64,
  pub hash_algorithm: HashAlgorithm,
  pub system_family_registry_fingerprint: Vec<u8>,
}

pub struct MigrationSourceWriteFreezeV1<'a> {
  source: &'a StorageEngine,
  authority: FrozenMigrationSourceAuthorityV1,
  _exclusive: ExclusiveReadOnlyEngineMaintenanceGuard<'a>,
}

impl MigrationSourceWriteFreezeV1<'_> {
  pub const fn authority(&self) -> &FrozenMigrationSourceAuthorityV1 {
    &self.authority
  }

  pub fn validate_unchanged(&self) -> Result<(), MigrationFinalReconciliationErrorV1> {
    let identity = source_identity(self.source)?;
    let snapshot = self.source.frozen_source_authority_snapshot()?;
    if identity != self.authority.physical_identity || !snapshot_matches(&snapshot, &self.authority) {
      return Err(MigrationFinalReconciliationErrorV1::invalid(
        "migration_final_freeze_authority_changed",
        "source physical identity, header, HEAD, or hard-publication frontier changed while final freeze was held",
      ));
    }
    Ok(())
  }
}

#[derive(Debug)]
pub enum MigrationFinalReconciliationErrorV1 {
  Invalid { code: &'static str, message: String },
  Engine(EngineError),
  Format(FormatError),
}

impl MigrationFinalReconciliationErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Invalid { code, .. } => code,
      Self::Engine(_) => "migration_final_freeze_engine",
      Self::Format(source) => source.code(),
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }
}

impl Display for MigrationFinalReconciliationErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Invalid { code, message } => write!(formatter, "{code}: {message}"),
      Self::Engine(source) => Display::fmt(source, formatter),
      Self::Format(source) => Display::fmt(source, formatter),
    }
  }
}

impl Error for MigrationFinalReconciliationErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Invalid { .. } => None,
      Self::Engine(source) => Some(source),
      Self::Format(source) => Some(source),
    }
  }
}

impl From<EngineError> for MigrationFinalReconciliationErrorV1 {
  fn from(source: EngineError) -> Self {
    Self::Engine(source)
  }
}

impl From<FormatError> for MigrationFinalReconciliationErrorV1 {
  fn from(source: FormatError) -> Self {
    Self::Format(source)
  }
}

pub fn acquire_migration_source_write_freeze_v1<'a>(
  request: MigrationSourceWriteFreezeRequestV1<'a>,
) -> Result<MigrationSourceWriteFreezeV1<'a>, MigrationFinalReconciliationErrorV1> {
  validate_request(&request)?;
  let before = source_identity(request.source)?;
  if before != request.permit.source_file_identity() {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_freeze_source_identity",
      "live source file identity differs from the preflight permit",
    ));
  }
  let acquisition_started = Instant::now();
  let exclusive = request
    .source
    .acquire_exclusive_read_only_engine_maintenance("migration_final_write_freeze", request.acquisition_timeout, Some(request.cancellation))
    .map_err(|error| map_acquisition_error(error, acquisition_started, request.acquisition_timeout, request.cancellation))?;
  if request.cancellation.is_cancelled() {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_freeze_canceled",
      "final source write freeze was canceled after admission",
    ));
  }
  let after = source_identity(request.source)?;
  let snapshot = request.source.frozen_source_authority_snapshot()?;
  if after != before {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_freeze_source_replaced",
      "source file identity changed while final write admission was closing",
    ));
  }
  if snapshot.hash_algorithm != request.permit.hash_algorithm()
    || snapshot.header_sequence < request.permit.source_header_sequence()
    || snapshot.namespace_root.len() != request.permit.hash_algorithm().hash_length()
  {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_freeze_source_frontier",
      "frozen source header, hash profile, or HEAD is inconsistent with preflight",
    ));
  }
  let registry = embedded_system_family_registry(snapshot.hash_algorithm)?;
  if registry.operational_fingerprint != request.permit.system_family_registry_fingerprint() {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_freeze_system_family",
      "frozen source SystemFamily registry differs from preflight",
    ));
  }
  Ok(MigrationSourceWriteFreezeV1 {
    source: request.source,
    authority: FrozenMigrationSourceAuthorityV1 {
      physical_identity: after,
      header_sequence: snapshot.header_sequence,
      namespace_root: snapshot.namespace_root,
      hard_publication_frontier: snapshot.hard_publication_frontier,
      hash_algorithm: snapshot.hash_algorithm,
      system_family_registry_fingerprint: registry.operational_fingerprint.clone(),
    },
    _exclusive: exclusive,
  })
}

fn validate_request(request: &MigrationSourceWriteFreezeRequestV1<'_>) -> Result<(), MigrationFinalReconciliationErrorV1> {
  if request.acquisition_timeout.is_zero() || request.acquisition_timeout > MAXIMUM_FREEZE_ACQUISITION_TIMEOUT {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_freeze_timeout",
      "final source write-freeze acquisition timeout must be within (0, 24 hours]",
    ));
  }
  if request.cancellation.is_cancelled() {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_freeze_canceled",
      "final source write freeze was canceled before admission",
    ));
  }
  if request.source.hash_algo() != request.permit.hash_algorithm() {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_freeze_hash_profile",
      "live source hash profile differs from the preflight permit",
    ));
  }
  Ok(())
}

fn source_identity(source: &StorageEngine) -> Result<PlatformFileIdentityDescriptorV1, MigrationFinalReconciliationErrorV1> {
  platform_file_identity(source.database_path())
    .map_err(|error| MigrationFinalReconciliationErrorV1::Engine(EngineError::IoError(std::io::Error::other(error.to_string()))))
}

fn snapshot_matches(snapshot: &FrozenSourceAuthoritySnapshot, authority: &FrozenMigrationSourceAuthorityV1) -> bool {
  snapshot.header_sequence == authority.header_sequence
    && snapshot.namespace_root == authority.namespace_root
    && snapshot.hard_publication_frontier == authority.hard_publication_frontier
    && snapshot.hash_algorithm == authority.hash_algorithm
}

fn map_acquisition_error(
  error: EngineError,
  started: Instant,
  timeout: Duration,
  cancellation: &CancellationToken,
) -> MigrationFinalReconciliationErrorV1 {
  if cancellation.is_cancelled() {
    return MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_freeze_canceled",
      "final source write freeze was canceled while waiting for exclusive admission",
    );
  }
  if started.elapsed() >= timeout {
    return MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_freeze_timeout",
      "final source write freeze timed out while waiting for exclusive admission",
    );
  }
  MigrationFinalReconciliationErrorV1::Engine(error)
}
