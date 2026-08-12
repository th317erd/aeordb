//! Crash-safe DatabaseHeaderV4 publication without service-writer activation.

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs::File;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::engine::durability_coordinator::{
  CommitClass, DurabilityCommitPlan, DurabilityCommitReceipt, DurabilityCoordinator, DurabilityCoordinatorError,
  DurabilityFailureDisposition, DurabilityGroupExecutor, DurabilityOperation, DurabilityWaiterState, classify_native_durability_error,
};
use crate::engine::native_durability::{
  NativeDurabilityError, NativeDurabilityOperation, read_file_at_native, sync_file_all_native, sync_file_data_native,
  verify_file_bytes_native, write_file_at_native,
};

use super::database_header::{
  DATABASE_HEADER_V4_REGION_LENGTH, DATABASE_HEADER_V4_SLOT_LENGTH, DatabaseHeaderV4, SelectedDatabaseHeaderV4, decode_header_region,
  encode_database_header_slot,
};
use super::reader::FormatError;

const PUBLICATION_OPERATIONS: [DurabilityOperation; 6] = [
  DurabilityOperation::DependencyAppend,
  DurabilityOperation::DataBarrier,
  DurabilityOperation::AuthorityWrite,
  DurabilityOperation::HeaderAb,
  DurabilityOperation::AuthorityBarrier,
  DurabilityOperation::AuthorityReadback,
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseHeaderObservationV4 {
  pub region: [u8; DATABASE_HEADER_V4_REGION_LENGTH],
  pub selected: SelectedDatabaseHeaderV4,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseHeaderPublicationReceiptV4 {
  pub durability: DurabilityCommitReceipt,
  pub observation: DatabaseHeaderObservationV4,
}

#[derive(Debug)]
pub enum DatabaseHeaderPublicationErrorV4 {
  Invalid { code: &'static str, message: String },
  Format(FormatError),
  Native(NativeDurabilityError),
  Durability(DurabilityCoordinatorError),
  PublicationLockPoisoned,
}

impl DatabaseHeaderPublicationErrorV4 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Invalid { code, .. } => code,
      Self::Format(error) => error.code(),
      Self::Native(_) => "native_io_failure",
      Self::Durability(_) => "durability_failure",
      Self::PublicationLockPoisoned => "publication_lock_poisoned",
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }
}

impl Display for DatabaseHeaderPublicationErrorV4 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Invalid { code, message } => write!(formatter, "{code}: {message}"),
      Self::Format(error) => write!(formatter, "database header format error: {error}"),
      Self::Native(error) => write!(formatter, "database header native I/O error: {error}"),
      Self::Durability(error) => write!(formatter, "database header durability error: {error}"),
      Self::PublicationLockPoisoned => formatter.write_str("database header publication lock is poisoned"),
    }
  }
}

impl Error for DatabaseHeaderPublicationErrorV4 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Format(error) => Some(error),
      Self::Native(error) => Some(error),
      Self::Durability(error) => Some(error),
      Self::Invalid { .. } | Self::PublicationLockPoisoned => None,
    }
  }
}

impl From<FormatError> for DatabaseHeaderPublicationErrorV4 {
  fn from(error: FormatError) -> Self {
    Self::Format(error)
  }
}

impl From<NativeDurabilityError> for DatabaseHeaderPublicationErrorV4 {
  fn from(error: NativeDurabilityError) -> Self {
    Self::Native(error)
  }
}

impl From<DurabilityCoordinatorError> for DatabaseHeaderPublicationErrorV4 {
  fn from(error: DurabilityCoordinatorError) -> Self {
    Self::Durability(error)
  }
}

pub fn observe_database_header_v4(file: &File) -> Result<DatabaseHeaderObservationV4, DatabaseHeaderPublicationErrorV4> {
  NativeHeaderPublicationIo.read_observation(file)
}

/// One serialization authority for a single v4 database file.
///
/// The future storage-engine owner must retain exactly one instance for each
/// open database. This shadow API does not activate a startup or service
/// writer; it only proves the publication primitive used by those later
/// phases.
pub struct DatabaseHeaderPublisherV4 {
  coordinator: Arc<DurabilityCoordinator>,
  publication_lock: Mutex<()>,
  io: Arc<dyn HeaderPublicationIo>,
}

pub(crate) trait HeaderPublicationDependencyV4 {
  fn append_dependency(&mut self, publication_sequence: u64) -> Result<(), NativeDurabilityError>;
}

struct NoopHeaderPublicationDependencyV4;

impl HeaderPublicationDependencyV4 for NoopHeaderPublicationDependencyV4 {
  fn append_dependency(&mut self, _publication_sequence: u64) -> Result<(), NativeDurabilityError> {
    Ok(())
  }
}

/// An exact inactive-slot successor whose hard-authority ticket has been
/// admitted while the per-file publication lock remains held.
pub(crate) struct AdmittedDatabaseHeaderPublicationV4<'a> {
  publisher: &'a DatabaseHeaderPublisherV4,
  file: &'a File,
  _authority: MutexGuard<'a, ()>,
  ticket: crate::engine::durability_coordinator::DurabilityTicket,
  execution_started: bool,
  writes: Vec<(usize, [u8; DATABASE_HEADER_V4_SLOT_LENGTH])>,
  expected_region: [u8; DATABASE_HEADER_V4_REGION_LENGTH],
  expected_selected: SelectedDatabaseHeaderV4,
}

impl AdmittedDatabaseHeaderPublicationV4<'_> {
  pub(crate) fn sequence(&self) -> u64 {
    self.ticket.sequence()
  }

  pub(crate) fn expected_observation(&self) -> DatabaseHeaderObservationV4 {
    DatabaseHeaderObservationV4 { region: self.expected_region, selected: self.expected_selected.clone() }
  }

  pub(crate) fn commit(self) -> Result<DatabaseHeaderPublicationReceiptV4, DatabaseHeaderPublicationErrorV4> {
    let mut dependency = NoopHeaderPublicationDependencyV4;
    self.commit_with_dependency(&mut dependency)
  }

  pub(crate) fn commit_with_dependency(
    mut self,
    dependency: &mut dyn HeaderPublicationDependencyV4,
  ) -> Result<DatabaseHeaderPublicationReceiptV4, DatabaseHeaderPublicationErrorV4> {
    let ticket = self.ticket;
    self.execution_started = true;
    let mut executor = HeaderPublicationExecutor {
      file: self.file,
      io: self.publisher.io.as_ref(),
      writes: &self.writes,
      expected_region: &self.expected_region,
      dependency,
    };
    if let Err(execution_error) = self.publisher.coordinator.execute_group(&[ticket], &mut executor) {
      return match self.publisher.coordinator.take_waiter_state(ticket) {
        Ok(DurabilityWaiterState::Failed(_)) => Err(DatabaseHeaderPublicationErrorV4::Durability(execution_error)),
        Ok(other) => Err(DatabaseHeaderPublicationErrorV4::invalid(
          "durability_failure_cleanup_state",
          format!("header publication failed as {execution_error}, but waiter cleanup observed {other:?}"),
        )),
        Err(cleanup_error) => Err(DatabaseHeaderPublicationErrorV4::invalid(
          "durability_failure_cleanup_failed",
          format!("header publication failed as {execution_error}; waiter cleanup also failed: {cleanup_error}"),
        )),
      };
    }
    let durability = match self.publisher.coordinator.take_waiter_state(ticket)? {
      DurabilityWaiterState::Succeeded(receipt) => receipt,
      DurabilityWaiterState::Failed(failure) => {
        return Err(DatabaseHeaderPublicationErrorV4::invalid("durability_failure", failure.message));
      }
      DurabilityWaiterState::Pending => {
        return Err(DatabaseHeaderPublicationErrorV4::invalid(
          "durability_waiter_pending",
          "header publication remained pending after synchronous execution",
        ));
      }
    };
    let observation = DatabaseHeaderObservationV4 { region: self.expected_region, selected: self.expected_selected.clone() };
    Ok(DatabaseHeaderPublicationReceiptV4 { durability, observation })
  }
}

impl Drop for AdmittedDatabaseHeaderPublicationV4<'_> {
  fn drop(&mut self) {
    if self.execution_started {
      return;
    }
    self.publisher.coordinator.cancel_admitted_or_latch_failure(self.ticket);
  }
}

impl DatabaseHeaderPublisherV4 {
  pub fn new(coordinator: Arc<DurabilityCoordinator>) -> Self {
    Self { coordinator, publication_lock: Mutex::new(()), io: Arc::new(NativeHeaderPublicationIo) }
  }

  /// Publish a checked successor to the currently inactive slot.
  ///
  /// Any dependency bytes must already have been appended through the
  /// caller's exclusive file-writer authority. This operation executes a data
  /// barrier before touching the header, then a full authority barrier and
  /// exact region read-back before acknowledgement.
  pub fn publish_inactive_slot(
    &self,
    file: &File,
    expected: &DatabaseHeaderObservationV4,
    candidate: DatabaseHeaderV4,
  ) -> Result<DatabaseHeaderPublicationReceiptV4, DatabaseHeaderPublicationErrorV4> {
    self.admit_inactive_slot(file, expected, candidate)?.commit()
  }

  pub(crate) fn admit_inactive_slot<'a>(
    &'a self,
    file: &'a File,
    expected: &DatabaseHeaderObservationV4,
    candidate: DatabaseHeaderV4,
  ) -> Result<AdmittedDatabaseHeaderPublicationV4<'a>, DatabaseHeaderPublicationErrorV4> {
    self.admit_inactive_slot_with_dependency_bytes(file, expected, candidate, 0)
  }

  pub(crate) fn admit_inactive_slot_with_dependency_bytes<'a>(
    &'a self,
    file: &'a File,
    expected: &DatabaseHeaderObservationV4,
    mut candidate: DatabaseHeaderV4,
    dependency_bytes: u64,
  ) -> Result<AdmittedDatabaseHeaderPublicationV4<'a>, DatabaseHeaderPublicationErrorV4> {
    let authority = match self.publication_lock.lock() {
      Ok(authority) => authority,
      Err(poisoned) => {
        drop(poisoned);
        return Err(DatabaseHeaderPublicationErrorV4::PublicationLockPoisoned);
      }
    };
    self.require_current(file, expected)?;
    validate_ordinary_transition(&expected.selected.header, &candidate)?;
    candidate.slot_sequence = next_sequence(expected.selected.header.slot_sequence)?;
    let target_slot = 1 - expected.selected.selected_slot;
    let target_bytes = encode_database_header_slot(&candidate)?;
    let mut expected_region = expected.region;
    replace_slot(&mut expected_region, target_slot, &target_bytes);
    let expected_selected = decode_header_region(&expected_region)?;
    if expected_selected.selected_slot != target_slot || expected_selected.header != candidate || expected_selected.redundancy_degraded {
      return Err(DatabaseHeaderPublicationErrorV4::invalid(
        "inactive_slot_not_selected",
        "ordinary publication did not produce an exact non-degraded inactive-slot successor",
      ));
    }
    let plan = DurabilityCommitPlan::new(CommitClass::HardAuthority, PUBLICATION_OPERATIONS.to_vec())?;
    let estimated_bytes = dependency_bytes.checked_add(DATABASE_HEADER_V4_SLOT_LENGTH as u64).ok_or_else(|| {
      DatabaseHeaderPublicationErrorV4::invalid("publication_size_overflow", "header publication byte estimate overflowed")
    })?;
    let ticket = self.coordinator.admit_sized(plan, estimated_bytes)?;
    Ok(AdmittedDatabaseHeaderPublicationV4 {
      publisher: self,
      file,
      _authority: authority,
      ticket,
      execution_started: false,
      writes: vec![(target_slot, target_bytes)],
      expected_region,
      expected_selected,
    })
  }

  pub fn advance_writer_fence(
    &self,
    file: &File,
    expected: &DatabaseHeaderObservationV4,
    updated_at_ms: u64,
  ) -> Result<DatabaseHeaderPublicationReceiptV4, DatabaseHeaderPublicationErrorV4> {
    self.publish_dual_fence(file, expected, expected.selected.header.physical_instance_id, updated_at_ms, false)
  }

  pub fn adopt_physical_instance(
    &self,
    file: &File,
    expected: &DatabaseHeaderObservationV4,
    physical_instance_id: [u8; 16],
    updated_at_ms: u64,
  ) -> Result<DatabaseHeaderPublicationReceiptV4, DatabaseHeaderPublicationErrorV4> {
    self.publish_dual_fence(file, expected, physical_instance_id, updated_at_ms, true)
  }

  fn publish_dual_fence(
    &self,
    file: &File,
    expected: &DatabaseHeaderObservationV4,
    physical_instance_id: [u8; 16],
    updated_at_ms: u64,
    adoption: bool,
  ) -> Result<DatabaseHeaderPublicationReceiptV4, DatabaseHeaderPublicationErrorV4> {
    let _authority = match self.publication_lock.lock() {
      Ok(authority) => authority,
      Err(poisoned) => {
        drop(poisoned);
        return Err(DatabaseHeaderPublicationErrorV4::PublicationLockPoisoned);
      }
    };
    self.require_current(file, expected)?;
    let source = &expected.selected.header;
    if updated_at_ms < source.updated_at_ms {
      return Err(DatabaseHeaderPublicationErrorV4::invalid(
        "header_updated_at_regressed",
        format!("updated_at_ms {updated_at_ms} is older than selected header {}", source.updated_at_ms),
      ));
    }
    if physical_instance_id == [0; 16] || (adoption && physical_instance_id == source.physical_instance_id) {
      return Err(DatabaseHeaderPublicationErrorV4::invalid(
        "invalid_physical_identity_transition",
        "clone adoption requires a new random nonzero physical instance identity",
      ));
    }
    if !adoption && physical_instance_id != source.physical_instance_id {
      return Err(DatabaseHeaderPublicationErrorV4::invalid(
        "invalid_physical_identity_transition",
        "same-identity writer fencing cannot change physical instance identity",
      ));
    }
    let writer_fence_epoch = source
      .writer_fence_epoch
      .checked_add(1)
      .ok_or_else(|| DatabaseHeaderPublicationErrorV4::invalid("writer_fence_exhausted", "writer fence epoch is exhausted"))?;
    let final_sequence = next_sequence(source.slot_sequence)?;
    let mut first = source.clone();
    first.physical_instance_id = physical_instance_id;
    first.writer_fence_epoch = writer_fence_epoch;
    let mut second = first.clone();
    second.slot_sequence = final_sequence;
    second.updated_at_ms = updated_at_ms;
    let first_bytes = encode_database_header_slot(&first)?;
    let second_bytes = encode_database_header_slot(&second)?;
    let first_slot = 1 - expected.selected.selected_slot;
    let second_slot = expected.selected.selected_slot;
    let mut first_prefix = expected.region;
    replace_slot(&mut first_prefix, first_slot, &first_bytes);
    let prefix_error = match decode_header_region(&first_prefix) {
      Ok(_) => {
        return Err(DatabaseHeaderPublicationErrorV4::invalid(
          "unsafe_fence_first_write",
          "first fencing write selected a header instead of failing closed",
        ));
      }
      Err(error) => error,
    };
    if prefix_error.code() != "ambiguous_equal_sequence" {
      return Err(DatabaseHeaderPublicationErrorV4::invalid(
        "unsafe_fence_first_write",
        format!("first fencing write must fail closed as equal-sequence ambiguity, got {}", prefix_error.code()),
      ));
    }
    let mut expected_region = first_prefix;
    replace_slot(&mut expected_region, second_slot, &second_bytes);
    let expected_selected = decode_header_region(&expected_region)?;
    if expected_selected.selected_slot != second_slot
      || expected_selected.header != second
      || expected_selected.redundancy_degraded
      || selected_slot_header(&expected_region, first_slot)? != first
    {
      return Err(DatabaseHeaderPublicationErrorV4::invalid(
        "dual_fence_not_closed",
        "writer fencing did not produce two exact adopted slots and a deterministic successor",
      ));
    }
    self.publish(file, vec![(first_slot, first_bytes), (second_slot, second_bytes)], expected_region, expected_selected)
  }

  fn require_current(&self, file: &File, expected: &DatabaseHeaderObservationV4) -> Result<(), DatabaseHeaderPublicationErrorV4> {
    let current = self.io.read_observation(file)?;
    if current.region != expected.region || current.selected != expected.selected {
      return Err(DatabaseHeaderPublicationErrorV4::invalid(
        "stale_header_observation",
        "database header region changed after the caller observed it",
      ));
    }
    Ok(())
  }

  fn publish(
    &self,
    file: &File,
    writes: Vec<(usize, [u8; DATABASE_HEADER_V4_SLOT_LENGTH])>,
    expected_region: [u8; DATABASE_HEADER_V4_REGION_LENGTH],
    expected_selected: SelectedDatabaseHeaderV4,
  ) -> Result<DatabaseHeaderPublicationReceiptV4, DatabaseHeaderPublicationErrorV4> {
    let plan = DurabilityCommitPlan::new(CommitClass::HardAuthority, PUBLICATION_OPERATIONS.to_vec())?;
    let estimated_bytes = match writes.len() {
      1 => DATABASE_HEADER_V4_SLOT_LENGTH as u64,
      2 => DATABASE_HEADER_V4_REGION_LENGTH as u64,
      count => {
        return Err(DatabaseHeaderPublicationErrorV4::invalid(
          "invalid_publication_cardinality",
          format!("header publication requires one or two slot writes, got {count}"),
        ));
      }
    };
    let ticket = self.coordinator.admit_sized(plan, estimated_bytes)?;
    let mut dependency = NoopHeaderPublicationDependencyV4;
    let mut executor = HeaderPublicationExecutor {
      file,
      io: self.io.as_ref(),
      writes: &writes,
      expected_region: &expected_region,
      dependency: &mut dependency,
    };
    if let Err(execution_error) = self.coordinator.execute_group(&[ticket], &mut executor) {
      return match self.coordinator.take_waiter_state(ticket) {
        Ok(DurabilityWaiterState::Failed(_)) => Err(DatabaseHeaderPublicationErrorV4::Durability(execution_error)),
        Ok(other) => Err(DatabaseHeaderPublicationErrorV4::invalid(
          "durability_failure_cleanup_state",
          format!("header publication failed as {execution_error}, but waiter cleanup observed {other:?}"),
        )),
        Err(cleanup_error) => Err(DatabaseHeaderPublicationErrorV4::invalid(
          "durability_failure_cleanup_failed",
          format!("header publication failed as {execution_error}; waiter cleanup also failed: {cleanup_error}"),
        )),
      };
    }
    let durability = match self.coordinator.take_waiter_state(ticket)? {
      DurabilityWaiterState::Succeeded(receipt) => receipt,
      DurabilityWaiterState::Failed(failure) => {
        return Err(DatabaseHeaderPublicationErrorV4::invalid("durability_failure", failure.message));
      }
      DurabilityWaiterState::Pending => {
        return Err(DatabaseHeaderPublicationErrorV4::invalid(
          "durability_waiter_pending",
          "header publication remained pending after synchronous execution",
        ));
      }
    };
    let observation = DatabaseHeaderObservationV4 { region: expected_region, selected: expected_selected };
    Ok(DatabaseHeaderPublicationReceiptV4 { durability, observation })
  }
}

#[cfg(test)]
impl DatabaseHeaderPublisherV4 {
  pub(super) fn with_io(coordinator: Arc<DurabilityCoordinator>, io: Arc<dyn HeaderPublicationIo>) -> Self {
    Self { coordinator, publication_lock: Mutex::new(()), io }
  }
}

fn validate_ordinary_transition(source: &DatabaseHeaderV4, candidate: &DatabaseHeaderV4) -> Result<(), DatabaseHeaderPublicationErrorV4> {
  if candidate.slot_sequence != source.slot_sequence {
    return Err(DatabaseHeaderPublicationErrorV4::invalid(
      "candidate_sequence_not_current",
      "the publisher, not the caller, advances the inactive-slot sequence",
    ));
  }
  if candidate.hash_algorithm != source.hash_algorithm
    || candidate.created_at_ms != source.created_at_ms
    || candidate.database_id != source.database_id
    || candidate.physical_instance_id != source.physical_instance_id
    || candidate.writer_fence_epoch != source.writer_fence_epoch
    || candidate.required_reader_capabilities != source.required_reader_capabilities
    || candidate.required_writer_capabilities != source.required_writer_capabilities
    || candidate.system_family_registry_version != source.system_family_registry_version
    || candidate.system_family_registry_fingerprint != source.system_family_registry_fingerprint
  {
    return Err(DatabaseHeaderPublicationErrorV4::invalid(
      "ordinary_header_identity_changed",
      "ordinary inactive-slot publication cannot change immutable database, physical, fence, hash, capability-floor, or registry identity",
    ));
  }
  if candidate.updated_at_ms < source.updated_at_ms {
    return Err(DatabaseHeaderPublicationErrorV4::invalid(
      "header_updated_at_regressed",
      format!("updated_at_ms {} is older than selected header {}", candidate.updated_at_ms, source.updated_at_ms),
    ));
  }
  if candidate.write_sequence_high_water < source.write_sequence_high_water {
    return Err(DatabaseHeaderPublicationErrorV4::invalid("write_sequence_regressed", "write sequence high-water mark cannot regress"));
  }
  Ok(())
}

fn next_sequence(sequence: u64) -> Result<u64, DatabaseHeaderPublicationErrorV4> {
  sequence
    .checked_add(1)
    .ok_or_else(|| DatabaseHeaderPublicationErrorV4::invalid("header_sequence_exhausted", "database header sequence is exhausted"))
}

fn replace_slot(region: &mut [u8; DATABASE_HEADER_V4_REGION_LENGTH], slot: usize, bytes: &[u8; DATABASE_HEADER_V4_SLOT_LENGTH]) {
  let start = slot * DATABASE_HEADER_V4_SLOT_LENGTH;
  region[start..start + DATABASE_HEADER_V4_SLOT_LENGTH].copy_from_slice(bytes);
}

fn selected_slot_header(
  region: &[u8; DATABASE_HEADER_V4_REGION_LENGTH],
  slot: usize,
) -> Result<DatabaseHeaderV4, DatabaseHeaderPublicationErrorV4> {
  let start = slot * DATABASE_HEADER_V4_SLOT_LENGTH;
  let bytes = &region[start..start + DATABASE_HEADER_V4_SLOT_LENGTH];
  let mut duplicated = [0u8; DATABASE_HEADER_V4_REGION_LENGTH];
  duplicated[..DATABASE_HEADER_V4_SLOT_LENGTH].copy_from_slice(bytes);
  duplicated[DATABASE_HEADER_V4_SLOT_LENGTH..].copy_from_slice(bytes);
  Ok(decode_header_region(&duplicated)?.header)
}

pub(super) trait HeaderPublicationIo: fmt::Debug + Send + Sync {
  fn read_observation(&self, file: &File) -> Result<DatabaseHeaderObservationV4, DatabaseHeaderPublicationErrorV4>;
  fn data_barrier(&self, file: &File) -> Result<(), NativeDurabilityError>;
  fn write_slot(&self, file: &File, slot: usize, bytes: &[u8; DATABASE_HEADER_V4_SLOT_LENGTH]) -> Result<(), NativeDurabilityError>;
  fn full_barrier(&self, file: &File) -> Result<(), NativeDurabilityError>;
  fn verify_region(&self, file: &File, expected: &[u8; DATABASE_HEADER_V4_REGION_LENGTH]) -> Result<(), NativeDurabilityError>;
}

#[derive(Debug)]
struct NativeHeaderPublicationIo;

impl HeaderPublicationIo for NativeHeaderPublicationIo {
  fn read_observation(&self, file: &File) -> Result<DatabaseHeaderObservationV4, DatabaseHeaderPublicationErrorV4> {
    let mut region = [0u8; DATABASE_HEADER_V4_REGION_LENGTH];
    read_file_at_native(file, 0, &mut region)?;
    let selected = decode_header_region(&region)?;
    Ok(DatabaseHeaderObservationV4 { region, selected })
  }

  fn data_barrier(&self, file: &File) -> Result<(), NativeDurabilityError> {
    sync_file_data_native(file)
  }

  fn write_slot(&self, file: &File, slot: usize, bytes: &[u8; DATABASE_HEADER_V4_SLOT_LENGTH]) -> Result<(), NativeDurabilityError> {
    write_file_at_native(file, (slot * DATABASE_HEADER_V4_SLOT_LENGTH) as u64, bytes)
  }

  fn full_barrier(&self, file: &File) -> Result<(), NativeDurabilityError> {
    sync_file_all_native(file)
  }

  fn verify_region(&self, file: &File, expected: &[u8; DATABASE_HEADER_V4_REGION_LENGTH]) -> Result<(), NativeDurabilityError> {
    verify_file_bytes_native(file, 0, expected)
  }
}

struct HeaderPublicationExecutor<'a> {
  file: &'a File,
  io: &'a dyn HeaderPublicationIo,
  writes: &'a [(usize, [u8; DATABASE_HEADER_V4_SLOT_LENGTH])],
  expected_region: &'a [u8; DATABASE_HEADER_V4_REGION_LENGTH],
  dependency: &'a mut dyn HeaderPublicationDependencyV4,
}

impl DurabilityGroupExecutor for HeaderPublicationExecutor<'_> {
  type Error = NativeDurabilityError;

  fn execute_group(&mut self, sequences: &[u64], operation: DurabilityOperation) -> Result<(), Self::Error> {
    match operation {
      DurabilityOperation::DependencyAppend => {
        let sequence = sequences
          .first()
          .copied()
          .ok_or_else(|| NativeDurabilityError::invalid(NativeDurabilityOperation::WriteAt, "header publication omitted its sequence"))?;
        self.dependency.append_dependency(sequence)
      }
      DurabilityOperation::HeaderAb => Ok(()),
      DurabilityOperation::DataBarrier => self.io.data_barrier(self.file),
      DurabilityOperation::AuthorityWrite => {
        for (slot, bytes) in self.writes {
          self.io.write_slot(self.file, *slot, bytes)?;
        }
        Ok(())
      }
      DurabilityOperation::AuthorityBarrier => self.io.full_barrier(self.file),
      DurabilityOperation::AuthorityReadback => self.io.verify_region(self.file, self.expected_region),
      _ => Err(NativeDurabilityError::invalid(
        NativeDurabilityOperation::WriteAt,
        format!("unsupported operation in v4 database-header publication plan: {operation:?}"),
      )),
    }
  }

  fn classify_error(&self, _operation: DurabilityOperation, error: &Self::Error, mutation_started: bool) -> DurabilityFailureDisposition {
    classify_native_durability_error(error, mutation_started)
  }
}

#[cfg(test)]
#[path = "../../../spec/engine/v4_header_publication_internal_spec.rs"]
mod v4_header_publication_internal_spec;
