//! Fenced owner for the disconnected v3-to-v4 migration controls.
//!
//! The owner consumes one opaque preflight permit and publishes only through
//! the physical v4 first authority. It deliberately has no live service,
//! namespace, source-file, or garbage-collection authority.

use std::error::Error;
use std::fmt::{self, Debug, Display, Formatter};
use std::sync::Arc;

use super::first_authority::{
  FirstAuthorityPublicationErrorV1, LoadedMutableSystemControlV1, MutableSystemControlPublicationErrorV1,
  MutableSystemControlPublicationRequestV1, MutableSystemControlPublicationReceiptV1, V4FirstAuthorityPublisher,
};
use super::gc_retirement::{RetirementJournalOwnerErrorV1, RetirementJournalOwnerV1};
use super::header_publication::DatabaseHeaderObservationV4;
use super::migration_control::{
  MigrationLeaseBodyV1, MigrationLeaseControlV1, MigrationLeaseStateV1, MigrationPhaseV1, MigrationProgressBodyV1,
  MigrationProgressControlV1, MigrationProgressStateV1, decode_migration_lease_control, decode_migration_progress_control,
  encode_migration_lease_control, encode_migration_progress_control,
};
use super::migration_preflight::MigrationPreflightPermitV1;
use super::reader::FormatError;
use super::system_control::SystemControlKindV1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationAcquisitionRequestV1 {
  pub holder_boot_id: [u8; 16],
  pub acquired_at_ms: i64,
  pub lease_duration_ms: i64,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationAcquisitionReceiptV1 {
  pub lease_control_sequence: u64,
  pub progress_control_sequence: u64,
  pub fencing_token: u64,
  pub resumed_partial: bool,
  pub idempotent: bool,
}

pub struct MigrationStateOwnerV1 {
  publisher: Arc<V4FirstAuthorityPublisher>,
  permit: MigrationPreflightPermitV1,
  holder_boot_id: [u8; 16],
  fencing_token: u64,
}

impl Debug for MigrationStateOwnerV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("MigrationStateOwnerV1")
      .field("database_id", &self.permit.database_id())
      .field("migration_id", &self.permit.migration_id())
      .field("holder_boot_id", &self.holder_boot_id)
      .field("fencing_token", &self.fencing_token)
      .finish_non_exhaustive()
  }
}

impl MigrationStateOwnerV1 {
  pub fn acquire(
    publisher: Arc<V4FirstAuthorityPublisher>,
    permit: MigrationPreflightPermitV1,
    request: MigrationAcquisitionRequestV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<(Self, MigrationAcquisitionReceiptV1), MigrationStateOwnerErrorV1> {
    validate_acquisition_request(request)?;
    validate_destination_authority(&publisher, &permit)?;

    let initial_lease = load_lease(&publisher, &permit)?;
    let initial_progress = load_progress(&publisher, &permit)?;
    if initial_lease.is_none() && initial_progress.is_some() {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_progress_without_lease",
        "migration progress exists without a selected migration lease",
      ));
    }

    let lease_was_present = initial_lease.is_some();
    let progress_was_present = initial_progress.is_some();
    let lease = match initial_lease {
      Some((_, lease)) => {
        validate_held_lease(&lease, &permit, request)?;
        lease
      }
      None => {
        let expires_at_ms = request
          .acquired_at_ms
          .checked_add(request.lease_duration_ms)
          .ok_or_else(|| MigrationStateOwnerErrorV1::invalid("migration_lease_time_overflow", "migration lease expiry overflowed"))?;
        let body = MigrationLeaseBodyV1 {
          database_id: permit.database_id(),
          migration_id: permit.migration_id(),
          source_physical_instance_id: permit.source_physical_instance_id(),
          destination_physical_instance_id: permit.destination_physical_instance_id(),
          holder_boot_id: request.holder_boot_id,
          fencing_token: 1,
          acquired_at_ms: request.acquired_at_ms,
          renewed_at_ms: request.acquired_at_ms,
          expires_at_ms,
          source_header_sequence: permit.source_header_sequence(),
          state: MigrationLeaseStateV1::Held,
        };
        let encoded = encode_migration_lease_control(1, &body, permit.hash_algorithm())?;
        let receipt = match publish_control(
          &publisher,
          &permit,
          SystemControlKindV1::MigrationLease,
          &encoded,
          request.publication_timestamp_ms,
          request.monotonic_now_ms,
          retirement_owner,
        ) {
          Ok(receipt) => receipt,
          Err(source) if source.committed_receipt().is_some() => {
            return Err(MigrationStateOwnerErrorV1::LeaseCommitted { source: Box::new(source) });
          }
          Err(source) => return Err(MigrationStateOwnerErrorV1::Publication(source)),
        };
        MigrationLeaseControlV1 { sequence: receipt.control_sequence, body }
      }
    };

    let progress_publication_timestamp = request.publication_timestamp_ms.checked_add(1).ok_or_else(|| {
      MigrationStateOwnerErrorV1::invalid("migration_progress_time_overflow", "migration progress publication timestamp overflowed")
    })?;
    let progress_monotonic_now = request.monotonic_now_ms.checked_add(1).ok_or_else(|| {
      MigrationStateOwnerErrorV1::invalid("migration_progress_time_overflow", "migration progress monotonic timestamp overflowed")
    })?;
    let progress = match initial_progress {
      Some((_, progress)) => {
        validate_bound_progress(&progress, &lease, &permit)?;
        progress
      }
      None => {
        let hash_width = permit.hash_algorithm().hash_length();
        let body = MigrationProgressBodyV1 {
          database_id: permit.database_id(),
          migration_id: permit.migration_id(),
          source_physical_instance_id: permit.source_physical_instance_id(),
          destination_physical_instance_id: permit.destination_physical_instance_id(),
          fencing_token: lease.body.fencing_token,
          phase: MigrationPhaseV1::Preflight,
          state: MigrationProgressStateV1::Pending,
          flags: 0,
          source_header_sequence: permit.source_header_sequence(),
          destination_header_sequence: 0,
          copied_through_write_sequence: 0,
          captured_through_publication_sequence: 0,
          reconciled_through_publication_sequence: 0,
          namespace_count: 0,
          entity_count: 0,
          copied_bytes: 0,
          updated_at_ms: lease.body.acquired_at_ms,
          source_capture_head: permit.source_capture_head().to_vec(),
          checkpoint_artifact: vec![0; hash_width],
          legacy_root_map_control_payload_hash: vec![0; hash_width],
          effective_config_fingerprint: permit.effective_configuration_fingerprint().to_vec(),
          system_family_registry_fingerprint: permit.system_family_registry_fingerprint().to_vec(),
          last_error_evidence: vec![0; hash_width],
        };
        let encoded = encode_migration_progress_control(1, &body, permit.hash_algorithm())?;
        let receipt = match publish_control(
          &publisher,
          &permit,
          SystemControlKindV1::MigrationProgress,
          &encoded,
          progress_publication_timestamp,
          progress_monotonic_now,
          retirement_owner,
        ) {
          Ok(receipt) => receipt,
          Err(source) if source.committed_receipt().is_some() => {
            return Err(MigrationStateOwnerErrorV1::AcquisitionCommitted {
              lease_control_sequence: lease.sequence,
              source: Box::new(source),
            });
          }
          Err(source) => {
            return Err(MigrationStateOwnerErrorV1::PartialAcquisition {
              lease_control_sequence: lease.sequence,
              source: Box::new(source),
            });
          }
        };
        MigrationProgressControlV1 { sequence: receipt.control_sequence, body }
      }
    };

    let owner = Self { publisher, permit, holder_boot_id: lease.body.holder_boot_id, fencing_token: lease.body.fencing_token };
    let receipt = MigrationAcquisitionReceiptV1 {
      lease_control_sequence: lease.sequence,
      progress_control_sequence: progress.sequence,
      fencing_token: lease.body.fencing_token,
      resumed_partial: lease_was_present && !progress_was_present,
      idempotent: lease_was_present && progress_was_present,
    };
    Ok((owner, receipt))
  }

  pub const fn fencing_token(&self) -> u64 {
    self.fencing_token
  }

  pub const fn holder_boot_id(&self) -> [u8; 16] {
    self.holder_boot_id
  }

  pub const fn database_id(&self) -> [u8; 16] {
    self.permit.database_id()
  }

  pub const fn migration_id(&self) -> [u8; 16] {
    self.permit.migration_id()
  }

  pub const fn preflight_evidence_fingerprint(&self) -> [u8; 32] {
    self.permit.evidence_fingerprint()
  }

  pub fn destination_observation(&self) -> Result<DatabaseHeaderObservationV4, MigrationStateOwnerErrorV1> {
    self.publisher.observe().map_err(Into::into)
  }
}

#[derive(Debug)]
pub enum MigrationStateOwnerErrorV1 {
  Invalid { code: &'static str, message: String },
  LeaseCommitted { source: Box<MutableSystemControlPublicationErrorV1> },
  PartialAcquisition { lease_control_sequence: u64, source: Box<MutableSystemControlPublicationErrorV1> },
  AcquisitionCommitted { lease_control_sequence: u64, source: Box<MutableSystemControlPublicationErrorV1> },
  Format(FormatError),
  Authority(FirstAuthorityPublicationErrorV1),
  Publication(MutableSystemControlPublicationErrorV1),
  RetirementOwner(RetirementJournalOwnerErrorV1),
}

impl MigrationStateOwnerErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Invalid { code, .. } => code,
      Self::LeaseCommitted { .. } => "migration_lease_committed",
      Self::PartialAcquisition { .. } => "migration_acquisition_partial",
      Self::AcquisitionCommitted { .. } => "migration_acquisition_committed",
      Self::Format(source) => source.code(),
      Self::Authority(source) => source.code(),
      Self::Publication(source) => source.code(),
      Self::RetirementOwner(source) => source.code(),
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }
}

impl Display for MigrationStateOwnerErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    let code = self.code();
    match self {
      Self::Invalid { message, .. } => write!(formatter, "{code}: {message}"),
      Self::LeaseCommitted { source } => write!(formatter, "{code}: lease committed but acquisition did not continue: {source}"),
      Self::PartialAcquisition { lease_control_sequence, source } => {
        write!(formatter, "{code}: lease control {lease_control_sequence} is durable but initial progress did not commit: {source}")
      }
      Self::AcquisitionCommitted { lease_control_sequence, source } => {
        write!(formatter, "{code}: lease control {lease_control_sequence} and progress committed but post-commit handling failed: {source}")
      }
      Self::Format(source) => write!(formatter, "{code}: migration control format error: {source}"),
      Self::Authority(source) => write!(formatter, "{code}: migration authority error: {source}"),
      Self::Publication(source) => write!(formatter, "{code}: migration control publication error: {source}"),
      Self::RetirementOwner(source) => write!(formatter, "{code}: migration retirement owner error: {source}"),
    }
  }
}

impl Error for MigrationStateOwnerErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::LeaseCommitted { source } | Self::PartialAcquisition { source, .. } | Self::AcquisitionCommitted { source, .. } => {
        Some(source.as_ref())
      }
      Self::Format(source) => Some(source),
      Self::Authority(source) => Some(source),
      Self::Publication(source) => Some(source),
      Self::RetirementOwner(source) => Some(source),
      Self::Invalid { .. } => None,
    }
  }
}

impl From<FormatError> for MigrationStateOwnerErrorV1 {
  fn from(source: FormatError) -> Self {
    Self::Format(source)
  }
}

impl From<FirstAuthorityPublicationErrorV1> for MigrationStateOwnerErrorV1 {
  fn from(source: FirstAuthorityPublicationErrorV1) -> Self {
    Self::Authority(source)
  }
}

impl From<RetirementJournalOwnerErrorV1> for MigrationStateOwnerErrorV1 {
  fn from(source: RetirementJournalOwnerErrorV1) -> Self {
    Self::RetirementOwner(source)
  }
}

fn validate_acquisition_request(request: MigrationAcquisitionRequestV1) -> Result<(), MigrationStateOwnerErrorV1> {
  if request.holder_boot_id.iter().all(|byte| *byte == 0) {
    return Err(MigrationStateOwnerErrorV1::invalid("migration_holder_boot_identity", "migration holder boot identity must be nonzero"));
  }
  if request.acquired_at_ms < 0 || request.lease_duration_ms <= 0 {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_lease_times",
      "migration acquisition time must be nonnegative and lease duration must be positive",
    ));
  }
  if request.publication_timestamp_ms == 0 || request.monotonic_now_ms == 0 {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_publication_times",
      "migration publication and monotonic timestamps must be nonzero",
    ));
  }
  request
    .acquired_at_ms
    .checked_add(request.lease_duration_ms)
    .ok_or_else(|| MigrationStateOwnerErrorV1::invalid("migration_lease_time_overflow", "migration lease expiry overflowed"))?;
  request.publication_timestamp_ms.checked_add(1).ok_or_else(|| {
    MigrationStateOwnerErrorV1::invalid("migration_progress_time_overflow", "migration progress publication timestamp overflowed")
  })?;
  request.monotonic_now_ms.checked_add(1).ok_or_else(|| {
    MigrationStateOwnerErrorV1::invalid("migration_progress_time_overflow", "migration progress monotonic timestamp overflowed")
  })?;
  Ok(())
}

fn validate_destination_authority(
  publisher: &V4FirstAuthorityPublisher,
  permit: &MigrationPreflightPermitV1,
) -> Result<(), MigrationStateOwnerErrorV1> {
  let observation = publisher.observe()?;
  let header = &observation.selected.header;
  if observation.selected.redundancy_degraded || header.head_hash.iter().all(|byte| *byte == 0) {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_destination_authority",
      "migration acquisition requires initialized non-degraded destination first authority",
    ));
  }
  if header.database_id != permit.database_id()
    || header.physical_instance_id != permit.destination_physical_instance_id()
    || header.hash_algorithm != permit.hash_algorithm()
  {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_destination_identity",
      "preflight permit and destination database authority disagree",
    ));
  }
  if header.system_family_registry_fingerprint != permit.system_family_registry_fingerprint() {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_destination_system_family",
      "preflight permit and destination SystemFamily registry disagree",
    ));
  }
  Ok(())
}

fn validate_held_lease(
  lease: &MigrationLeaseControlV1,
  permit: &MigrationPreflightPermitV1,
  request: MigrationAcquisitionRequestV1,
) -> Result<(), MigrationStateOwnerErrorV1> {
  let body = &lease.body;
  if body.database_id != permit.database_id()
    || body.migration_id != permit.migration_id()
    || body.source_physical_instance_id != permit.source_physical_instance_id()
    || body.destination_physical_instance_id != permit.destination_physical_instance_id()
    || body.source_header_sequence != permit.source_header_sequence()
  {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_lease_binding",
      "selected migration lease does not match the exact preflight permit",
    ));
  }
  if body.state != MigrationLeaseStateV1::Held {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_lease_not_held",
      format!("selected migration lease is {:?}, not held", body.state),
    ));
  }
  if body.holder_boot_id != request.holder_boot_id {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_lease_held_by_other_boot",
      "selected migration lease belongs to another boot identity",
    ));
  }
  if request.acquired_at_ms >= body.expires_at_ms {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_lease_expired",
      "selected migration lease has expired and requires explicit takeover recovery",
    ));
  }
  Ok(())
}

fn validate_bound_progress(
  progress: &MigrationProgressControlV1,
  lease: &MigrationLeaseControlV1,
  permit: &MigrationPreflightPermitV1,
) -> Result<(), MigrationStateOwnerErrorV1> {
  let body = &progress.body;
  if body.database_id != permit.database_id()
    || body.migration_id != permit.migration_id()
    || body.source_physical_instance_id != permit.source_physical_instance_id()
    || body.destination_physical_instance_id != permit.destination_physical_instance_id()
    || body.source_header_sequence != permit.source_header_sequence()
    || body.fencing_token != lease.body.fencing_token
  {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_progress_binding",
      "selected migration progress does not match the exact permit and lease token",
    ));
  }
  if body.source_capture_head != permit.source_capture_head()
    || body.effective_config_fingerprint != permit.effective_configuration_fingerprint()
    || body.system_family_registry_fingerprint != permit.system_family_registry_fingerprint()
  {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_progress_policy_binding",
      "selected migration progress does not match preflight source, configuration, or SystemFamily evidence",
    ));
  }
  Ok(())
}

fn load_lease(
  publisher: &V4FirstAuthorityPublisher,
  permit: &MigrationPreflightPermitV1,
) -> Result<Option<(LoadedMutableSystemControlV1, MigrationLeaseControlV1)>, MigrationStateOwnerErrorV1> {
  publisher
    .load_mutable_system_control(SystemControlKindV1::MigrationLease, &permit.database_id(), &permit.migration_id())?
    .map(|loaded| {
      let decoded = decode_migration_lease_control(&loaded.bytes, permit.hash_algorithm())?;
      Ok((loaded, decoded))
    })
    .transpose()
}

fn load_progress(
  publisher: &V4FirstAuthorityPublisher,
  permit: &MigrationPreflightPermitV1,
) -> Result<Option<(LoadedMutableSystemControlV1, MigrationProgressControlV1)>, MigrationStateOwnerErrorV1> {
  publisher
    .load_mutable_system_control(SystemControlKindV1::MigrationProgress, &permit.database_id(), &permit.migration_id())?
    .map(|loaded| {
      let decoded = decode_migration_progress_control(&loaded.bytes, permit.hash_algorithm())?;
      Ok((loaded, decoded))
    })
    .transpose()
}

fn publish_control(
  publisher: &V4FirstAuthorityPublisher,
  permit: &MigrationPreflightPermitV1,
  kind: SystemControlKindV1,
  encoded_control: &[u8],
  publication_timestamp_ms: u64,
  monotonic_now_ms: u64,
  retirement_owner: &mut RetirementJournalOwnerV1,
) -> Result<MutableSystemControlPublicationReceiptV1, MutableSystemControlPublicationErrorV1> {
  publisher.publish_mutable_system_control(
    MutableSystemControlPublicationRequestV1 {
      database_id: &permit.database_id(),
      kind,
      identity: &permit.migration_id(),
      expected: None,
      encoded_control,
      publication_timestamp_ms,
      monotonic_now_ms,
    },
    retirement_owner,
  )
}
