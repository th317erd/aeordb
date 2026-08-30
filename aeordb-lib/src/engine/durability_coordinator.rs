//! Shared durability admission, operation-ledger, and hard-frontier ownership.
//!
//! This module starts as a non-activating contract shell. Existing v3 writers
//! are migrated behind it in later P2a landing units; until then it must not
//! change their persistent bytes or acknowledgement behavior.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fmt;
use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::engine::native_durability::{
  NativeDurabilityError, NativeDurabilityErrorClass, NativeDurabilityOperation, sync_file_all_native, sync_file_data_native,
};
use crate::engine::memory_coordinator::{
  AdmissionClass, CriticalMemoryPurpose, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation,
};
use crate::engine::v4::contract_generated::{durability_operation_v1, os_error_class_v1, retry_class_v1};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitClass {
  HardAuthority,
  RecoverableSoftState,
  Disposable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeFileBarrierKind {
  Data,
  Full,
}

/// Testable platform boundary for recoverable file barriers. Production uses
/// the native implementation; fault suites can inject an exact barrier
/// failure without filling a filesystem or mutating process-wide limits.
pub trait RecoverableFileBarrier: fmt::Debug + Send + Sync {
  fn execute_file_barrier(&self, file: &File, barrier_kind: NativeFileBarrierKind) -> Result<(), NativeDurabilityError>;
}

#[derive(Debug, Default)]
struct NativeRecoverableFileBarrier;

impl RecoverableFileBarrier for NativeRecoverableFileBarrier {
  fn execute_file_barrier(&self, file: &File, barrier_kind: NativeFileBarrierKind) -> Result<(), NativeDurabilityError> {
    match barrier_kind {
      NativeFileBarrierKind::Data => sync_file_data_native(file),
      NativeFileBarrierKind::Full => sync_file_all_native(file),
    }
  }
}

impl CommitClass {
  pub const ALL: [Self; 3] = [Self::HardAuthority, Self::RecoverableSoftState, Self::Disposable];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DurabilityOperation {
  DependencyAppend,
  DataBarrier,
  AuthorityWrite,
  AuthorityBarrier,
  AuthorityReadback,
  HeaderAb,
  ControlAb,
  ParentDirectorySync,
  DurableReplace,
  Preallocation,
  VoidClaim,
  EmergencySpill,
  CutoverJournal,
  CutoverRename,
  ShutdownFlush,
}

impl DurabilityOperation {
  pub const ALL: [Self; 15] = [
    Self::DependencyAppend,
    Self::DataBarrier,
    Self::AuthorityWrite,
    Self::AuthorityBarrier,
    Self::AuthorityReadback,
    Self::HeaderAb,
    Self::ControlAb,
    Self::ParentDirectorySync,
    Self::DurableReplace,
    Self::Preallocation,
    Self::VoidClaim,
    Self::EmergencySpill,
    Self::CutoverJournal,
    Self::CutoverRename,
    Self::ShutdownFlush,
  ];

  pub const fn stable_id(self) -> u16 {
    match self {
      Self::DependencyAppend => durability_operation_v1::DEPENDENCY_APPEND,
      Self::DataBarrier => durability_operation_v1::DATA_BARRIER,
      Self::AuthorityWrite => durability_operation_v1::AUTHORITY_WRITE,
      Self::AuthorityBarrier => durability_operation_v1::AUTHORITY_BARRIER,
      Self::AuthorityReadback => durability_operation_v1::AUTHORITY_READBACK,
      Self::HeaderAb => durability_operation_v1::HEADER_AB,
      Self::ControlAb => durability_operation_v1::CONTROL_AB,
      Self::ParentDirectorySync => durability_operation_v1::PARENT_DIRECTORY_SYNC,
      Self::DurableReplace => durability_operation_v1::DURABLE_REPLACE,
      Self::Preallocation => durability_operation_v1::PREALLOCATION,
      Self::VoidClaim => durability_operation_v1::VOID_CLAIM,
      Self::EmergencySpill => durability_operation_v1::EMERGENCY_SPILL,
      Self::CutoverJournal => durability_operation_v1::CUTOVER_JOURNAL,
      Self::CutoverRename => durability_operation_v1::CUTOVER_RENAME,
      Self::ShutdownFlush => durability_operation_v1::SHUTDOWN_FLUSH,
    }
  }

  pub fn is_stable_id(value: u16) -> bool {
    Self::ALL.iter().any(|operation| operation.stable_id() == value)
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OsErrorClass {
  InterruptedNoProgress,
  NoSpace,
  Quota,
  ReadOnly,
  Permission,
  MediaIo,
  DeviceLost,
  InvalidHandle,
  UnsupportedDurability,
  ChecksumReadback,
  ShortWrite,
  TimeoutUnknown,
  OtherPersistentIo,
}

impl OsErrorClass {
  pub const ALL: [Self; 13] = [
    Self::InterruptedNoProgress,
    Self::NoSpace,
    Self::Quota,
    Self::ReadOnly,
    Self::Permission,
    Self::MediaIo,
    Self::DeviceLost,
    Self::InvalidHandle,
    Self::UnsupportedDurability,
    Self::ChecksumReadback,
    Self::ShortWrite,
    Self::TimeoutUnknown,
    Self::OtherPersistentIo,
  ];

  pub const fn stable_id(self) -> u16 {
    match self {
      Self::InterruptedNoProgress => os_error_class_v1::INTERRUPTED_NO_PROGRESS,
      Self::NoSpace => os_error_class_v1::NO_SPACE,
      Self::Quota => os_error_class_v1::QUOTA,
      Self::ReadOnly => os_error_class_v1::READ_ONLY,
      Self::Permission => os_error_class_v1::PERMISSION,
      Self::MediaIo => os_error_class_v1::MEDIA_IO,
      Self::DeviceLost => os_error_class_v1::DEVICE_LOST,
      Self::InvalidHandle => os_error_class_v1::INVALID_HANDLE,
      Self::UnsupportedDurability => os_error_class_v1::UNSUPPORTED_DURABILITY,
      Self::ChecksumReadback => os_error_class_v1::CHECKSUM_READBACK,
      Self::ShortWrite => os_error_class_v1::SHORT_WRITE,
      Self::TimeoutUnknown => os_error_class_v1::TIMEOUT_UNKNOWN,
      Self::OtherPersistentIo => os_error_class_v1::OTHER_PERSISTENT_IO,
    }
  }

  pub fn is_stable_id(value: u16) -> bool {
    Self::ALL.iter().any(|error_class| error_class.stable_id() == value)
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryClass {
  None,
  Immediate,
  BoundedBackoff,
  AfterDependency,
  AfterRepair,
  Never,
}

impl RetryClass {
  pub const ALL: [Self; 6] = [Self::None, Self::Immediate, Self::BoundedBackoff, Self::AfterDependency, Self::AfterRepair, Self::Never];

  pub const fn stable_id(self) -> u16 {
    match self {
      Self::None => retry_class_v1::NONE,
      Self::Immediate => retry_class_v1::IMMEDIATE,
      Self::BoundedBackoff => retry_class_v1::BOUNDED_BACKOFF,
      Self::AfterDependency => retry_class_v1::AFTER_DEPENDENCY,
      Self::AfterRepair => retry_class_v1::AFTER_REPAIR,
      Self::Never => retry_class_v1::NEVER,
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DurabilityFailureDisposition {
  pub os_error_class: Option<OsErrorClass>,
  pub retry_class: RetryClass,
  pub serious: bool,
  pub uncertain_completion: bool,
}

impl DurabilityFailureDisposition {
  pub const fn serious(os_error_class: OsErrorClass, retry_class: RetryClass) -> Self {
    Self { os_error_class: Some(os_error_class), retry_class, serious: true, uncertain_completion: false }
  }

  pub const fn transient(os_error_class: OsErrorClass, retry_class: RetryClass) -> Self {
    Self { os_error_class: Some(os_error_class), retry_class, serious: false, uncertain_completion: false }
  }

  pub const fn uncertain(os_error_class: OsErrorClass) -> Self {
    Self { os_error_class: Some(os_error_class), retry_class: RetryClass::AfterRepair, serious: true, uncertain_completion: true }
  }
}

pub fn classify_io_error(error: &std::io::Error, mutation_started: bool) -> DurabilityFailureDisposition {
  if let Some(disposition) = classify_raw_os_error(error.raw_os_error()) {
    return disposition;
  }
  match error.kind() {
    std::io::ErrorKind::Interrupted => DurabilityFailureDisposition::transient(OsErrorClass::InterruptedNoProgress, RetryClass::Immediate),
    std::io::ErrorKind::WouldBlock if !mutation_started => {
      DurabilityFailureDisposition::transient(OsErrorClass::OtherPersistentIo, RetryClass::BoundedBackoff)
    }
    std::io::ErrorKind::TimedOut if !mutation_started => {
      DurabilityFailureDisposition::transient(OsErrorClass::TimeoutUnknown, RetryClass::BoundedBackoff)
    }
    std::io::ErrorKind::TimedOut => DurabilityFailureDisposition::uncertain(OsErrorClass::TimeoutUnknown),
    std::io::ErrorKind::StorageFull => DurabilityFailureDisposition::serious(OsErrorClass::NoSpace, RetryClass::AfterRepair),
    std::io::ErrorKind::ReadOnlyFilesystem => DurabilityFailureDisposition::serious(OsErrorClass::ReadOnly, RetryClass::AfterRepair),
    std::io::ErrorKind::PermissionDenied => DurabilityFailureDisposition::serious(OsErrorClass::Permission, RetryClass::AfterRepair),
    std::io::ErrorKind::WriteZero | std::io::ErrorKind::UnexpectedEof => {
      DurabilityFailureDisposition::serious(OsErrorClass::ShortWrite, RetryClass::Never)
    }
    std::io::ErrorKind::NotConnected | std::io::ErrorKind::BrokenPipe => {
      DurabilityFailureDisposition::serious(OsErrorClass::DeviceLost, RetryClass::AfterRepair)
    }
    _ => DurabilityFailureDisposition::serious(OsErrorClass::OtherPersistentIo, RetryClass::Never),
  }
}

pub fn classify_native_durability_error(error: &NativeDurabilityError, mutation_started: bool) -> DurabilityFailureDisposition {
  classify_native_error_class(error.class(), error.raw_os_error(), error.io_error_kind(), mutation_started)
}

fn classify_native_error_class(
  class: NativeDurabilityErrorClass,
  raw_os_error: Option<i32>,
  io_error_kind: Option<std::io::ErrorKind>,
  mutation_started: bool,
) -> DurabilityFailureDisposition {
  match class {
    NativeDurabilityErrorClass::Unsupported => {
      DurabilityFailureDisposition::serious(OsErrorClass::UnsupportedDurability, RetryClass::Never)
    }
    NativeDurabilityErrorClass::UncertainCompletion => DurabilityFailureDisposition::uncertain(OsErrorClass::TimeoutUnknown),
    NativeDurabilityErrorClass::Verification => DurabilityFailureDisposition::serious(OsErrorClass::ChecksumReadback, RetryClass::Never),
    NativeDurabilityErrorClass::InvalidInput => {
      DurabilityFailureDisposition { os_error_class: None, retry_class: RetryClass::Never, serious: false, uncertain_completion: false }
    }
    NativeDurabilityErrorClass::Io => {
      let error = if let Some(raw_os_error) = raw_os_error {
        std::io::Error::from_raw_os_error(raw_os_error)
      } else {
        std::io::Error::from(io_error_kind.unwrap_or(std::io::ErrorKind::Other))
      };
      classify_io_error(&error, mutation_started)
    }
  }
}

fn classify_raw_os_error(raw: Option<i32>) -> Option<DurabilityFailureDisposition> {
  let raw = raw?;
  #[cfg(unix)]
  {
    let disposition = match raw {
      libc::EINTR => DurabilityFailureDisposition::transient(OsErrorClass::InterruptedNoProgress, RetryClass::Immediate),
      libc::ENOSPC => DurabilityFailureDisposition::serious(OsErrorClass::NoSpace, RetryClass::AfterRepair),
      libc::EDQUOT => DurabilityFailureDisposition::serious(OsErrorClass::Quota, RetryClass::AfterRepair),
      libc::EROFS => DurabilityFailureDisposition::serious(OsErrorClass::ReadOnly, RetryClass::AfterRepair),
      libc::EACCES | libc::EPERM => DurabilityFailureDisposition::serious(OsErrorClass::Permission, RetryClass::AfterRepair),
      libc::EIO => DurabilityFailureDisposition::serious(OsErrorClass::MediaIo, RetryClass::AfterRepair),
      libc::ENODEV => DurabilityFailureDisposition::serious(OsErrorClass::DeviceLost, RetryClass::AfterRepair),
      libc::EBADF => DurabilityFailureDisposition::serious(OsErrorClass::InvalidHandle, RetryClass::Never),
      libc::ETIMEDOUT => DurabilityFailureDisposition::uncertain(OsErrorClass::TimeoutUnknown),
      _ => return None,
    };
    Some(disposition)
  }
  #[cfg(windows)]
  {
    const ERROR_DISK_FULL: i32 = 112;
    const ERROR_WRITE_PROTECT: i32 = 19;
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_INVALID_HANDLE: i32 = 6;
    const ERROR_DISK_QUOTA_EXCEEDED: i32 = 1295;
    const ERROR_NOT_ENOUGH_QUOTA: i32 = 1816;
    const ERROR_IO_DEVICE: i32 = 1117;
    const ERROR_DEVICE_NOT_CONNECTED: i32 = 1167;
    const ERROR_SEM_TIMEOUT: i32 = 121;
    let disposition = match raw {
      ERROR_DISK_FULL => DurabilityFailureDisposition::serious(OsErrorClass::NoSpace, RetryClass::AfterRepair),
      ERROR_DISK_QUOTA_EXCEEDED | ERROR_NOT_ENOUGH_QUOTA => {
        DurabilityFailureDisposition::serious(OsErrorClass::Quota, RetryClass::AfterRepair)
      }
      ERROR_WRITE_PROTECT => DurabilityFailureDisposition::serious(OsErrorClass::ReadOnly, RetryClass::AfterRepair),
      ERROR_ACCESS_DENIED => DurabilityFailureDisposition::serious(OsErrorClass::Permission, RetryClass::AfterRepair),
      ERROR_IO_DEVICE => DurabilityFailureDisposition::serious(OsErrorClass::MediaIo, RetryClass::AfterRepair),
      ERROR_DEVICE_NOT_CONNECTED => DurabilityFailureDisposition::serious(OsErrorClass::DeviceLost, RetryClass::AfterRepair),
      ERROR_INVALID_HANDLE => DurabilityFailureDisposition::serious(OsErrorClass::InvalidHandle, RetryClass::Never),
      ERROR_SEM_TIMEOUT => DurabilityFailureDisposition::uncertain(OsErrorClass::TimeoutUnknown),
      _ => return None,
    };
    return Some(disposition);
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurabilityCommitPlan {
  class: CommitClass,
  operations: Vec<DurabilityOperation>,
}

impl DurabilityCommitPlan {
  pub fn new(class: CommitClass, operations: Vec<DurabilityOperation>) -> Result<Self, DurabilityCoordinatorError> {
    validate_plan(class, &operations)?;
    Ok(Self { class, operations })
  }

  pub fn class(&self) -> CommitClass {
    self.class
  }

  pub fn operations(&self) -> &[DurabilityOperation] {
    &self.operations
  }
}

fn validate_plan(class: CommitClass, operations: &[DurabilityOperation]) -> Result<(), DurabilityCoordinatorError> {
  match class {
    CommitClass::HardAuthority => validate_hard_plan(operations),
    CommitClass::RecoverableSoftState => validate_soft_plan(operations),
    CommitClass::Disposable => {
      if operations.is_empty() {
        Ok(())
      } else {
        Err(DurabilityCoordinatorError::invalid_plan("disposable work cannot contain durability operations"))
      }
    }
  }
}

fn validate_hard_plan(operations: &[DurabilityOperation]) -> Result<(), DurabilityCoordinatorError> {
  if operations.is_empty() {
    return Err(DurabilityCoordinatorError::invalid_plan("hard authority plan cannot be empty"));
  }
  reject_duplicate_operations(operations)?;

  let position = |needle| operations.iter().position(|operation| *operation == needle);
  let barrier = position(DurabilityOperation::AuthorityBarrier)
    .ok_or_else(|| DurabilityCoordinatorError::invalid_plan("hard authority plan requires an authority barrier"))?;
  let readback = position(DurabilityOperation::AuthorityReadback)
    .ok_or_else(|| DurabilityCoordinatorError::invalid_plan("hard authority plan requires authority read-back"))?;
  if readback <= barrier {
    return Err(DurabilityCoordinatorError::invalid_plan("authority read-back must follow the authority barrier"));
  }

  let authority_positions: Vec<usize> =
    operations.iter().enumerate().filter_map(|(index, operation)| is_authority_publication(*operation).then_some(index)).collect();
  if authority_positions.is_empty() {
    return Err(DurabilityCoordinatorError::invalid_plan("hard authority plan has no authority publication"));
  }
  if authority_positions.iter().any(|position| *position >= barrier) {
    return Err(DurabilityCoordinatorError::invalid_plan("every authority publication must precede its barrier"));
  }

  if let Some(dependency) = position(DurabilityOperation::DependencyAppend) {
    let data_barrier = position(DurabilityOperation::DataBarrier)
      .ok_or_else(|| DurabilityCoordinatorError::invalid_plan("dependency append requires a data barrier"))?;
    let Some(first_authority) = authority_positions.iter().min().copied() else {
      return Err(DurabilityCoordinatorError::invalid_plan("hard authority plan has no authority publication"));
    };
    if data_barrier <= dependency || data_barrier >= first_authority {
      return Err(DurabilityCoordinatorError::invalid_plan(
        "the data barrier must follow dependency append and precede authority publication",
      ));
    }
  }

  for selector in [DurabilityOperation::HeaderAb, DurabilityOperation::ControlAb] {
    if let Some(selector_position) = position(selector) {
      let authority_write = position(DurabilityOperation::AuthorityWrite)
        .ok_or_else(|| DurabilityCoordinatorError::invalid_plan("A/B publication requires an authority write"))?;
      if selector_position <= authority_write || selector_position >= barrier {
        return Err(DurabilityCoordinatorError::invalid_plan(
          "A/B publication must follow authority write and precede the authority barrier",
        ));
      }
    }
  }

  for namespace_mutation in [DurabilityOperation::DurableReplace, DurabilityOperation::CutoverRename] {
    if let Some(mutation_position) = position(namespace_mutation) {
      let parent_sync = position(DurabilityOperation::ParentDirectorySync)
        .ok_or_else(|| DurabilityCoordinatorError::invalid_plan("durable namespace mutation requires parent-directory sync"))?;
      if parent_sync <= mutation_position || parent_sync >= barrier {
        return Err(DurabilityCoordinatorError::invalid_plan(
          "parent-directory sync must follow namespace mutation and precede the authority barrier",
        ));
      }
    }
  }

  if operations.iter().skip(readback + 1).any(|operation| is_authority_publication(*operation) || is_barrier(*operation)) {
    return Err(DurabilityCoordinatorError::invalid_plan("authority work cannot follow final read-back"));
  }
  Ok(())
}

fn validate_soft_plan(operations: &[DurabilityOperation]) -> Result<(), DurabilityCoordinatorError> {
  reject_duplicate_operations(operations)?;
  if operations.iter().any(|operation| !matches!(operation, DurabilityOperation::DependencyAppend | DurabilityOperation::DataBarrier)) {
    return Err(DurabilityCoordinatorError::invalid_plan("recoverable soft state cannot publish authority"));
  }
  if let Some(data_barrier) = operations.iter().position(|operation| *operation == DurabilityOperation::DataBarrier) {
    let Some(dependency) = operations.iter().position(|operation| *operation == DurabilityOperation::DependencyAppend) else {
      return Err(DurabilityCoordinatorError::invalid_plan("soft data barrier has no dependency append"));
    };
    if data_barrier <= dependency {
      return Err(DurabilityCoordinatorError::invalid_plan("soft data barrier must follow dependency append"));
    }
  }
  Ok(())
}

fn reject_duplicate_operations(operations: &[DurabilityOperation]) -> Result<(), DurabilityCoordinatorError> {
  for (index, operation) in operations.iter().enumerate() {
    if operations[..index].contains(operation) {
      return Err(DurabilityCoordinatorError::invalid_plan(format!("duplicate durability operation: {operation:?}")));
    }
  }
  Ok(())
}

fn is_authority_publication(operation: DurabilityOperation) -> bool {
  matches!(
    operation,
    DurabilityOperation::AuthorityWrite
      | DurabilityOperation::HeaderAb
      | DurabilityOperation::ControlAb
      | DurabilityOperation::DurableReplace
      | DurabilityOperation::VoidClaim
      | DurabilityOperation::CutoverJournal
      | DurabilityOperation::CutoverRename
      | DurabilityOperation::ShutdownFlush
  )
}

fn is_barrier(operation: DurabilityOperation) -> bool {
  matches!(operation, DurabilityOperation::DataBarrier | DurabilityOperation::AuthorityBarrier | DurabilityOperation::ParentDirectorySync)
}

pub trait DurabilityExecutor {
  type Error: fmt::Display;

  fn execute(&mut self, sequence: u64, operation: DurabilityOperation) -> Result<(), Self::Error>;
}

pub trait DurabilityGroupExecutor {
  type Error: fmt::Display;

  fn execute_group(&mut self, sequences: &[u64], operation: DurabilityOperation) -> Result<(), Self::Error>;

  fn classify_error(&self, operation: DurabilityOperation, error: &Self::Error, mutation_started: bool) -> DurabilityFailureDisposition;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DurabilityTicket {
  coordinator_id: uuid::Uuid,
  sequence: u64,
}

impl DurabilityTicket {
  pub fn sequence(self) -> u64 {
    self.sequence
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurabilityFailure {
  pub sequence: u64,
  pub operation: DurabilityOperation,
  pub message: String,
  pub os_error_class: Option<OsErrorClass>,
  pub retry_class: RetryClass,
  pub attempts: u8,
  pub serious: bool,
  pub uncertain_completion: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurabilityCommitReceipt {
  pub sequence: u64,
  pub class: CommitClass,
  pub hard_frontier: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DurabilityWaiterState {
  Pending,
  Succeeded(DurabilityCommitReceipt),
  Failed(DurabilityFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurabilityLedgerEntry {
  pub sequence: u64,
  pub operation: DurabilityOperation,
  pub succeeded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct DurabilityBarrierObservation {
  pub operation: DurabilityOperation,
  pub first_sequence: u64,
  pub last_sequence: u64,
  pub waiter_count: usize,
  pub succeeded: bool,
  pub attempts: u8,
  pub latency_ms: u64,
  pub completed_at_ms: i64,
  pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurabilityCoordinatorSnapshot {
  pub hard_frontier: u64,
  pub next_sequence: u64,
  pub admitted: usize,
  pub executing: usize,
  pub proven: usize,
  pub failed: usize,
  pub pending_hard: usize,
  pub driver_active: bool,
  pub oldest_pending_age_ms: Option<u64>,
  pub last_barrier: Option<DurabilityBarrierObservation>,
  pub ledger: Vec<DurabilityLedgerEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DurabilityCoordinatorError {
  InvalidPlan(String),
  ForeignTicket,
  UnknownTicket,
  AlreadyExecuted,
  ExecutorFailure(DurabilityFailure),
  StateUnavailable,
  SequenceExhausted,
  InvalidConfiguration(String),
  ResourceExhausted(String),
}

impl DurabilityCoordinatorError {
  fn invalid_plan(message: impl Into<String>) -> Self {
    Self::InvalidPlan(message.into())
  }

  pub fn operation(&self) -> Option<DurabilityOperation> {
    match self {
      Self::ExecutorFailure(failure) => Some(failure.operation),
      _ => None,
    }
  }
}

impl fmt::Display for DurabilityCoordinatorError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::InvalidPlan(message) => write!(formatter, "invalid durability plan: {message}"),
      Self::ForeignTicket => write!(formatter, "durability ticket belongs to another coordinator"),
      Self::UnknownTicket => write!(formatter, "durability ticket is unknown or already retired"),
      Self::AlreadyExecuted => write!(formatter, "durability ticket has already started execution"),
      Self::ExecutorFailure(failure) => {
        write!(formatter, "durability operation {:?} failed at sequence {}: {}", failure.operation, failure.sequence, failure.message)
      }
      Self::StateUnavailable => write!(formatter, "durability coordinator state is unavailable"),
      Self::SequenceExhausted => write!(formatter, "durability sequence space is exhausted"),
      Self::InvalidConfiguration(message) => write!(formatter, "invalid durability coordinator configuration: {message}"),
      Self::ResourceExhausted(message) => write!(formatter, "durability memory admission failed: {message}"),
    }
  }
}

impl std::error::Error for DurabilityCoordinatorError {}

#[derive(Clone, Debug)]
enum CommitStatus {
  Admitted,
  Executing,
  Proven,
  Failed(DurabilityFailure),
}

struct CommitRecord {
  plan: DurabilityCommitPlan,
  status: CommitStatus,
  estimated_bytes: u64,
  admitted_at: Instant,
  _memory_reservation: Option<MemoryReservation>,
}

struct CoordinatorState {
  next_sequence: u64,
  hard_frontier: u64,
  hard_failure: Option<DurabilityFailure>,
  records: BTreeMap<u64, CommitRecord>,
  pending_hard: VecDeque<u64>,
  ledger: VecDeque<DurabilityLedgerEntry>,
  ledger_capacity: usize,
  driver_active: bool,
  last_barrier: Option<DurabilityBarrierObservation>,
}

impl CoordinatorState {
  fn new(ledger_capacity: usize) -> Self {
    Self {
      next_sequence: 1,
      hard_frontier: 0,
      hard_failure: None,
      records: BTreeMap::new(),
      pending_hard: VecDeque::new(),
      ledger: VecDeque::new(),
      ledger_capacity,
      driver_active: false,
      last_barrier: None,
    }
  }

  fn record_ledger(&mut self, entry: DurabilityLedgerEntry) {
    if self.ledger.len() == self.ledger_capacity {
      self.ledger.pop_front();
    }
    self.ledger.push_back(entry);
  }
}

pub const DEFAULT_DURABILITY_LEDGER_CAPACITY: usize = 4_096;
pub const DEFAULT_GROUP_COMMIT_MAX_BYTES: u64 = 64 * 1024 * 1024;
pub const DEFAULT_GROUP_COMMIT_MAX_DELAY: Duration = Duration::from_millis(100);
pub const MIN_GROUP_COMMIT_MAX_BYTES: u64 = 1024 * 1024;
pub const MAX_GROUP_COMMIT_MAX_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_GROUP_COMMIT_MAX_DELAY: Duration = Duration::from_millis(1_000);
/// A zero-byte estimate must not allow one execution driver to materialize an
/// arbitrarily large ticket/hash-set scratch group.
pub const MAX_GROUP_COMMIT_RECORDS: usize = 1_024;
pub const MAX_DURABILITY_WAITER_RECORDS: usize = 4_096;
const LIVE_GROUP_DRIVER_DELAY: Duration = Duration::from_millis(1);
const DURABILITY_RECORD_BASE_BYTES: u64 = 512;
const MAX_DURABILITY_FAILURE_MESSAGE_BYTES: u64 = 4 * 1024;
const DURABILITY_LEDGER_ENTRY_BYTES: u64 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DurabilityGroupPolicy {
  max_bytes: u64,
  max_delay: Duration,
}

impl DurabilityGroupPolicy {
  pub fn new(max_bytes: u64, max_delay: Duration) -> Result<Self, DurabilityCoordinatorError> {
    if !(MIN_GROUP_COMMIT_MAX_BYTES..=MAX_GROUP_COMMIT_MAX_BYTES).contains(&max_bytes) {
      return Err(DurabilityCoordinatorError::InvalidConfiguration(format!(
        "group commit max bytes must be between {MIN_GROUP_COMMIT_MAX_BYTES} and {MAX_GROUP_COMMIT_MAX_BYTES}"
      )));
    }
    if max_delay > MAX_GROUP_COMMIT_MAX_DELAY {
      return Err(DurabilityCoordinatorError::InvalidConfiguration(format!(
        "group commit max delay must not exceed {} ms",
        MAX_GROUP_COMMIT_MAX_DELAY.as_millis()
      )));
    }
    Ok(Self { max_bytes, max_delay })
  }

  pub fn max_bytes(self) -> u64 {
    self.max_bytes
  }

  pub fn max_delay(self) -> Duration {
    self.max_delay
  }
}

impl Default for DurabilityGroupPolicy {
  fn default() -> Self {
    Self { max_bytes: DEFAULT_GROUP_COMMIT_MAX_BYTES, max_delay: DEFAULT_GROUP_COMMIT_MAX_DELAY }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurabilityGroupPolicySnapshot {
  pub policy: Option<DurabilityGroupPolicy>,
  pub disabled_reason: Option<String>,
}

enum DurabilityGroupPolicyState {
  Configured(DurabilityGroupPolicy),
  Disabled { reason: String },
}

pub struct DurabilityCoordinator {
  id: uuid::Uuid,
  state: Mutex<CoordinatorState>,
  changed: Condvar,
  group_policy: RwLock<DurabilityGroupPolicyState>,
  recoverable_file_barrier: Arc<dyn RecoverableFileBarrier>,
  memory_policy: RwLock<Option<DurabilityMemoryPolicy>>,
  state_poison_reported: AtomicBool,
}

struct DurabilityMemoryPolicy {
  coordinator: Arc<MemoryCoordinator>,
  _ledger_reservation: Option<MemoryReservation>,
}

impl Default for DurabilityCoordinator {
  fn default() -> Self {
    Self::new()
  }
}

impl DurabilityCoordinator {
  pub fn new() -> Self {
    Self::with_policy(DurabilityGroupPolicy::default())
  }

  pub fn with_policy(group_policy: DurabilityGroupPolicy) -> Self {
    Self::with_components(group_policy, DEFAULT_DURABILITY_LEDGER_CAPACITY, Arc::new(NativeRecoverableFileBarrier))
  }

  pub fn with_recoverable_file_barrier(recoverable_file_barrier: Arc<dyn RecoverableFileBarrier>) -> Self {
    Self::with_components(DurabilityGroupPolicy::default(), DEFAULT_DURABILITY_LEDGER_CAPACITY, recoverable_file_barrier)
  }

  pub fn with_ledger_capacity(ledger_capacity: usize) -> Result<Self, DurabilityCoordinatorError> {
    if ledger_capacity == 0 {
      return Err(DurabilityCoordinatorError::InvalidConfiguration("ledger capacity must be nonzero".to_string()));
    }
    Ok(Self::with_components(DurabilityGroupPolicy::default(), ledger_capacity, Arc::new(NativeRecoverableFileBarrier)))
  }

  pub fn with_policy_and_memory_coordinator(
    group_policy: DurabilityGroupPolicy,
    ledger_capacity: usize,
    memory_coordinator: Arc<MemoryCoordinator>,
  ) -> Result<Self, DurabilityCoordinatorError> {
    if ledger_capacity == 0 {
      return Err(DurabilityCoordinatorError::InvalidConfiguration("ledger capacity must be nonzero".to_string()));
    }
    let coordinator = Self::with_components(group_policy, ledger_capacity, Arc::new(NativeRecoverableFileBarrier));
    coordinator.activate_memory_coordinator(memory_coordinator)?;
    Ok(coordinator)
  }

  fn with_components(
    group_policy: DurabilityGroupPolicy,
    ledger_capacity: usize,
    recoverable_file_barrier: Arc<dyn RecoverableFileBarrier>,
  ) -> Self {
    Self {
      id: uuid::Uuid::new_v4(),
      state: Mutex::new(CoordinatorState::new(ledger_capacity)),
      changed: Condvar::new(),
      group_policy: RwLock::new(DurabilityGroupPolicyState::Configured(group_policy)),
      recoverable_file_barrier,
      memory_policy: RwLock::new(None),
      state_poison_reported: AtomicBool::new(false),
    }
  }

  fn release_drive_permit(&self) {
    match self.state.lock() {
      Ok(mut state) => state.driver_active = false,
      Err(poisoned) => {
        if !self.state_poison_reported.swap(true, Ordering::AcqRel) {
          crate::metrics::record_system_soft_failure(
            "durability_coordinator",
            "permit_release_after_poison",
            self.id,
            "coordinator state lock was poisoned; normal coordinator APIs remain unavailable",
          );
        }
        poisoned.into_inner().driver_active = false;
      }
    }
    self.changed.notify_all();
  }

  pub(crate) fn activate_memory_coordinator(&self, memory_coordinator: Arc<MemoryCoordinator>) -> Result<(), DurabilityCoordinatorError> {
    let mut policy = self.memory_policy.write().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
    if policy.is_some() {
      return Err(DurabilityCoordinatorError::InvalidConfiguration(
        "durability memory coordinator was activated more than once".to_string(),
      ));
    }
    let state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
    if !state.records.is_empty() || !state.pending_hard.is_empty() {
      return Err(DurabilityCoordinatorError::InvalidConfiguration(
        "durability memory coordinator cannot activate while waiter records are live".to_string(),
      ));
    }
    let ledger_bytes = u64::try_from(state.ledger_capacity)
      .ok()
      .and_then(|capacity| capacity.checked_mul(DURABILITY_LEDGER_ENTRY_BYTES))
      .and_then(|bytes| bytes.checked_add(MAX_DURABILITY_FAILURE_MESSAGE_BYTES))
      .ok_or_else(|| DurabilityCoordinatorError::InvalidConfiguration("durability ledger reservation overflow".to_string()))?;
    drop(state);
    let policy_available = memory_coordinator.snapshot().map_err(durability_memory_error)?.policy.is_some();
    let ledger_reservation = if policy_available {
      Some(
        memory_coordinator
          .reserve(MemoryOwner::DurabilityWaiters, ledger_bytes, AdmissionClass::Critical(CriticalMemoryPurpose::DurableWrite))
          .map_err(durability_memory_error)?,
      )
    } else {
      return Ok(());
    };
    *policy = Some(DurabilityMemoryPolicy { coordinator: memory_coordinator, _ledger_reservation: ledger_reservation });
    Ok(())
  }

  pub fn group_policy(&self) -> Result<DurabilityGroupPolicy, DurabilityCoordinatorError> {
    let policy = self.group_policy.read().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
    match &*policy {
      DurabilityGroupPolicyState::Configured(policy) => Ok(*policy),
      DurabilityGroupPolicyState::Disabled { reason } => {
        Err(DurabilityCoordinatorError::InvalidConfiguration(format!("durability grouping is disabled: {reason}")))
      }
    }
  }

  pub fn group_policy_snapshot(&self) -> Result<DurabilityGroupPolicySnapshot, DurabilityCoordinatorError> {
    let policy = self.group_policy.read().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
    Ok(match &*policy {
      DurabilityGroupPolicyState::Configured(policy) => DurabilityGroupPolicySnapshot { policy: Some(*policy), disabled_reason: None },
      DurabilityGroupPolicyState::Disabled { reason } => {
        DurabilityGroupPolicySnapshot { policy: None, disabled_reason: Some(reason.clone()) }
      }
    })
  }

  pub fn reconfigure_group_policy(&self, policy: DurabilityGroupPolicy) -> Result<(), DurabilityCoordinatorError> {
    *self.group_policy.write().map_err(|_| DurabilityCoordinatorError::StateUnavailable)? = DurabilityGroupPolicyState::Configured(policy);
    self.changed.notify_all();
    Ok(())
  }

  pub fn disable_grouping(&self, reason: impl Into<String>) -> Result<(), DurabilityCoordinatorError> {
    let reason = bounded_failure_message(reason.into());
    if reason.is_empty() {
      return Err(DurabilityCoordinatorError::InvalidConfiguration("disabled durability grouping requires a nonempty reason".to_string()));
    }
    *self.group_policy.write().map_err(|_| DurabilityCoordinatorError::StateUnavailable)? = DurabilityGroupPolicyState::Disabled { reason };
    self.changed.notify_all();
    Ok(())
  }

  fn group_selection_policy(&self) -> Result<Option<DurabilityGroupPolicy>, DurabilityCoordinatorError> {
    let policy = self.group_policy.read().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
    Ok(match &*policy {
      DurabilityGroupPolicyState::Configured(policy) => Some(*policy),
      DurabilityGroupPolicyState::Disabled { .. } => None,
    })
  }

  pub fn admit(&self, plan: DurabilityCommitPlan) -> Result<DurabilityTicket, DurabilityCoordinatorError> {
    self.admit_sized(plan, 0)
  }

  pub fn admit_sized(&self, plan: DurabilityCommitPlan, estimated_bytes: u64) -> Result<DurabilityTicket, DurabilityCoordinatorError> {
    {
      let state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
      ensure_waiter_admission_available(&state, plan.class)?;
    }
    let memory_policy = self.memory_policy.read().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
    let memory_reservation = match memory_policy.as_ref() {
      Some(policy) => Some(
        policy
          .coordinator
          .reserve(
            MemoryOwner::DurabilityWaiters,
            durability_record_bytes(&plan)?,
            AdmissionClass::Critical(CriticalMemoryPurpose::DurableWrite),
          )
          .map_err(durability_memory_error)?,
      ),
      None => None,
    };
    let mut state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
    ensure_waiter_admission_available(&state, plan.class)?;
    let sequence = state.next_sequence;
    state.next_sequence = state.next_sequence.checked_add(1).ok_or(DurabilityCoordinatorError::SequenceExhausted)?;
    if plan.class == CommitClass::HardAuthority {
      state.pending_hard.push_back(sequence);
    }
    state.records.insert(
      sequence,
      CommitRecord {
        plan,
        status: CommitStatus::Admitted,
        estimated_bytes,
        admitted_at: Instant::now(),
        _memory_reservation: memory_reservation,
      },
    );
    drop(state);
    self.changed.notify_all();
    Ok(DurabilityTicket { coordinator_id: self.id, sequence })
  }

  /// Retire one exact ticket before execution begins.
  ///
  /// Cancellation consumes the sequence permanently but does not advance the
  /// hard frontier or latch a durability failure. It is reserved for callers
  /// that can still prove no dependency or authority byte was mutated.
  pub(crate) fn cancel_admitted(&self, ticket: DurabilityTicket) -> Result<(), DurabilityCoordinatorError> {
    self.validate_ticket(ticket)?;
    let mut state = match self.state.lock() {
      Ok(state) => state,
      Err(poisoned) => {
        drop(poisoned);
        return Err(DurabilityCoordinatorError::StateUnavailable);
      }
    };
    let record = state.records.get(&ticket.sequence).ok_or(DurabilityCoordinatorError::UnknownTicket)?;
    if !matches!(record.status, CommitStatus::Admitted) {
      return Err(DurabilityCoordinatorError::AlreadyExecuted);
    }
    if record.plan.class == CommitClass::HardAuthority {
      let position = state
        .pending_hard
        .iter()
        .position(|sequence| *sequence == ticket.sequence)
        .ok_or_else(|| DurabilityCoordinatorError::InvalidPlan("admitted hard ticket is absent from the pending frontier".to_string()))?;
      state.pending_hard.remove(position);
    }
    let removed = state.records.remove(&ticket.sequence).ok_or(DurabilityCoordinatorError::UnknownTicket)?;
    drop(state);
    drop(removed);
    self.changed.notify_all();
    Ok(())
  }

  /// Retire an admission guard, latching hard writes if its exact ticket can
  /// no longer be cancelled. A guard cannot return an error from `Drop`, and
  /// silently stranding an admitted hard ticket would block the frontier.
  pub(crate) fn cancel_admitted_or_latch_failure(&self, ticket: DurabilityTicket) {
    let cancellation_error = match self.cancel_admitted(ticket) {
      Ok(()) => return,
      Err(error) => error,
    };
    let mut state = match self.state.lock() {
      Ok(state) => state,
      Err(poisoned) => poisoned.into_inner(),
    };
    let failure = DurabilityFailure {
      sequence: ticket.sequence,
      operation: DurabilityOperation::DependencyAppend,
      message: bounded_failure_message(format!("unexecuted durability admission could not be retired: {cancellation_error}")),
      os_error_class: None,
      retry_class: RetryClass::Never,
      attempts: 0,
      serious: true,
      uncertain_completion: false,
    };
    state.pending_hard.retain(|sequence| *sequence != ticket.sequence);
    if let Some(record) = state.records.get_mut(&ticket.sequence) {
      record.status = CommitStatus::Failed(failure.clone());
    }
    if state.hard_failure.is_none() {
      state.hard_failure = Some(failure);
    }
    drop(state);
    self.changed.notify_all();
  }

  pub fn select_ready_hard_group(&self, force: bool) -> Result<Vec<DurabilityTicket>, DurabilityCoordinatorError> {
    self.select_ready_hard_group_at(Instant::now(), force)
  }

  pub fn execute_recoverable_file_barrier(
    &self,
    file: &File,
    barrier_kind: NativeFileBarrierKind,
    estimated_bytes: u64,
  ) -> Result<(), DurabilityCoordinatorError> {
    let plan = DurabilityCommitPlan::new(
      CommitClass::RecoverableSoftState,
      vec![DurabilityOperation::DependencyAppend, DurabilityOperation::DataBarrier],
    )?;
    let ticket = self.admit_sized(plan, estimated_bytes)?;
    let mut executor = NativeFileBarrierExecutor { file, barrier_kind, barrier: self.recoverable_file_barrier.as_ref() };
    self.execute_group(&[ticket], &mut executor)?;
    match self.take_waiter_state(ticket)? {
      DurabilityWaiterState::Succeeded(_) => Ok(()),
      DurabilityWaiterState::Failed(failure) => Err(DurabilityCoordinatorError::ExecutorFailure(failure)),
      DurabilityWaiterState::Pending => Err(DurabilityCoordinatorError::StateUnavailable),
    }
  }

  pub fn execute<E: DurabilityExecutor>(&self, ticket: DurabilityTicket, executor: &mut E) -> Result<(), DurabilityCoordinatorError> {
    let mut adapter = SingleExecutorAdapter { executor };
    self.execute_group(&[ticket], &mut adapter)
  }

  pub fn execute_group<E: DurabilityGroupExecutor>(
    &self,
    tickets: &[DurabilityTicket],
    executor: &mut E,
  ) -> Result<(), DurabilityCoordinatorError> {
    let (sequences, operations) = self.begin_group_execution(tickets)?;
    let mut execution_guard =
      GroupExecutionGuard { coordinator: self, sequences: &sequences, operation: None, operation_started: None, armed: true };
    let mut mutation_started = false;

    for operation in operations {
      execution_guard.operation = Some(operation);
      execution_guard.operation_started = Some(Instant::now());
      let operation_may_mutate = operation_may_mutate(operation);
      let mut attempts = 0u8;
      loop {
        attempts = attempts.saturating_add(1);
        match executor.execute_group(&sequences, operation) {
          Ok(()) => {
            let mut state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
            for sequence in &sequences {
              state.record_ledger(DurabilityLedgerEntry { sequence: *sequence, operation, succeeded: true });
            }
            if is_barrier(operation) {
              state.last_barrier = Some(barrier_observation(
                &sequences,
                operation,
                true,
                attempts,
                execution_guard.operation_started.unwrap_or_else(Instant::now),
                None,
              ));
            }
            mutation_started |= operation_may_mutate;
            execution_guard.operation = None;
            execution_guard.operation_started = None;
            break;
          }
          Err(error) => {
            let disposition = executor.classify_error(operation, &error, mutation_started || operation_may_mutate);
            let error = bounded_failure_message(error.to_string());
            let mut state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
            for sequence in &sequences {
              state.record_ledger(DurabilityLedgerEntry { sequence: *sequence, operation, succeeded: false });
            }
            if should_retry(disposition, attempts) {
              drop(state);
              if disposition.retry_class == RetryClass::BoundedBackoff {
                std::thread::sleep(retry_backoff(attempts));
              }
              continue;
            }
            if is_barrier(operation) {
              state.last_barrier = Some(barrier_observation(
                &sequences,
                operation,
                false,
                attempts,
                execution_guard.operation_started.unwrap_or_else(Instant::now),
                Some(error.clone()),
              ));
            }
            drop(state);
            let failures = self.fail_group(&sequences, operation, error, disposition, attempts)?;
            self.changed.notify_all();
            execution_guard.armed = false;
            let Some(first_failure) = failures.into_iter().next() else {
              return Err(DurabilityCoordinatorError::StateUnavailable);
            };
            return Err(DurabilityCoordinatorError::ExecutorFailure(first_failure));
          }
        }
      }
    }

    let mut state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
    for sequence in &sequences {
      let record = state.records.get_mut(sequence).ok_or(DurabilityCoordinatorError::UnknownTicket)?;
      record.status = CommitStatus::Proven;
    }
    advance_hard_frontier(&mut state);
    execution_guard.armed = false;
    drop(state);
    self.changed.notify_all();
    Ok(())
  }

  /// Permanently halt the in-process hard-authority frontier and wake every
  /// admitted hard waiter with the same failure evidence.
  ///
  /// Once an earlier hard commit can no longer be proven, no later hard
  /// waiter can safely cross it. The storage engine owns persistence of the
  /// database latch; this halt prevents waiters from being stranded while the
  /// latch is recorded.
  pub fn fail_pending_hard(
    &self,
    operation: DurabilityOperation,
    message: impl Into<String>,
    disposition: DurabilityFailureDisposition,
    attempts: u8,
  ) -> Result<Vec<DurabilityFailure>, DurabilityCoordinatorError> {
    let mut state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
    let failures = fail_pending_hard_locked(&mut state, operation, message.into(), disposition, attempts);
    drop(state);
    self.changed.notify_all();
    Ok(failures)
  }

  /// Fail every pending hard-authority waiter and retire one exact caller
  /// ticket under the same coordinator lock. This prevents cleanup from
  /// acknowledging only half of the terminal state transition.
  pub fn fail_pending_hard_and_take(
    &self,
    ticket: DurabilityTicket,
    operation: DurabilityOperation,
    message: impl Into<String>,
    disposition: DurabilityFailureDisposition,
    attempts: u8,
  ) -> Result<DurabilityWaiterState, DurabilityCoordinatorError> {
    self.validate_ticket(ticket)?;
    let mut state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
    let record = state.records.get(&ticket.sequence).ok_or(DurabilityCoordinatorError::UnknownTicket)?;
    if record.plan.class != CommitClass::HardAuthority {
      return Err(DurabilityCoordinatorError::InvalidPlan(
        "only a hard-authority ticket can be retired by a hard failure transition".to_string(),
      ));
    }

    fail_pending_hard_locked(&mut state, operation, message.into(), disposition, attempts);
    let waiter_state = waiter_state_locked(&state, ticket)?;
    if matches!(waiter_state, DurabilityWaiterState::Pending) {
      return Err(DurabilityCoordinatorError::InvalidPlan("hard failure transition left the selected waiter pending".to_string()));
    }
    let retired = state.records.remove(&ticket.sequence).ok_or(DurabilityCoordinatorError::UnknownTicket)?;
    drop(state);
    drop(retired);
    self.changed.notify_all();
    Ok(waiter_state)
  }

  pub fn wait_for_hard_turn(&self, ticket: DurabilityTicket) -> Result<DurabilityHardTurn<'_>, DurabilityCoordinatorError> {
    self.validate_ticket(ticket)?;
    let mut state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
    let ticket_record = state.records.get(&ticket.sequence).ok_or(DurabilityCoordinatorError::UnknownTicket)?;
    if ticket_record.plan.class != CommitClass::HardAuthority {
      return Err(DurabilityCoordinatorError::InvalidPlan("only hard-authority tickets can join the live hard-commit driver".to_string()));
    }

    loop {
      let waiter = waiter_state_locked(&state, ticket)?;
      if !matches!(waiter, DurabilityWaiterState::Pending) {
        return Ok(DurabilityHardTurn::Complete(waiter));
      }

      if !state.driver_active {
        let first_record =
          state.pending_hard.front().and_then(|sequence| state.records.get(sequence)).ok_or(DurabilityCoordinatorError::UnknownTicket)?;
        match &first_record.status {
          CommitStatus::Failed(failure) => return Err(DurabilityCoordinatorError::ExecutorFailure(failure.clone())),
          CommitStatus::Executing | CommitStatus::Proven => {
            state = self.changed.wait(state).map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
            continue;
          }
          CommitStatus::Admitted => {}
        }

        let now = Instant::now();
        let group_policy = self.group_selection_policy()?;
        let candidates = match group_policy {
          Some(policy) => select_ready_hard_group_locked(&state, self.id, policy, now, true)?,
          None => select_single_hard_ticket_locked(&state, self.id)?,
        };
        let first_admitted_at = first_record.admitted_at;
        let live_delay = group_policy.map(|policy| policy.max_delay.min(LIVE_GROUP_DRIVER_DELAY)).unwrap_or_default();
        let elapsed = now.checked_duration_since(first_admitted_at).unwrap_or_default();
        if candidates.len() > 1 || elapsed >= live_delay {
          state.driver_active = true;
          drop(state);
          return Ok(DurabilityHardTurn::Drive(DurabilityDrivePermit { coordinator: self, active: true }));
        }

        let remaining = live_delay.saturating_sub(elapsed);
        let (next_state, _) = self.changed.wait_timeout(state, remaining).map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
        state = next_state;
      } else {
        state = self.changed.wait(state).map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
      }
    }
  }

  pub fn waiter_state(&self, ticket: DurabilityTicket) -> Result<DurabilityWaiterState, DurabilityCoordinatorError> {
    self.validate_ticket(ticket)?;
    let state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
    waiter_state_locked(&state, ticket)
  }

  pub fn take_waiter_state(&self, ticket: DurabilityTicket) -> Result<DurabilityWaiterState, DurabilityCoordinatorError> {
    let waiter_state = self.waiter_state(ticket)?;
    if !matches!(waiter_state, DurabilityWaiterState::Pending) {
      let mut state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
      let retired = state.records.remove(&ticket.sequence).ok_or(DurabilityCoordinatorError::UnknownTicket)?;
      drop(state);
      drop(retired);
    }
    Ok(waiter_state)
  }

  pub fn snapshot(&self) -> Result<DurabilityCoordinatorSnapshot, DurabilityCoordinatorError> {
    let state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
    Ok(snapshot_locked(&state))
  }

  pub fn wait_until_idle(&self, timeout: Duration) -> Result<DurabilityCoordinatorSnapshot, DurabilityCoordinatorError> {
    let deadline = Instant::now() + timeout;
    let mut state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
    while coordinator_has_nonterminal_work(&state) {
      let now = Instant::now();
      if now >= deadline {
        break;
      }
      let remaining = deadline.saturating_duration_since(now);
      let (next_state, wait_result) =
        self.changed.wait_timeout(state, remaining).map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
      state = next_state;
      if wait_result.timed_out() {
        break;
      }
    }
    Ok(snapshot_locked(&state))
  }

  pub fn hard_failure(&self) -> Result<Option<DurabilityFailure>, DurabilityCoordinatorError> {
    let state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
    Ok(state.hard_failure.clone())
  }

  pub fn memory_reservations_active(&self) -> Result<bool, DurabilityCoordinatorError> {
    Ok(
      self
        .memory_policy
        .read()
        .map_err(|_| DurabilityCoordinatorError::StateUnavailable)?
        .as_ref()
        .is_some_and(|policy| policy._ledger_reservation.is_some()),
    )
  }

  fn validate_ticket(&self, ticket: DurabilityTicket) -> Result<(), DurabilityCoordinatorError> {
    if ticket.coordinator_id == self.id {
      Ok(())
    } else {
      Err(DurabilityCoordinatorError::ForeignTicket)
    }
  }

  fn select_ready_hard_group_at(&self, now: Instant, force: bool) -> Result<Vec<DurabilityTicket>, DurabilityCoordinatorError> {
    let state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
    match self.group_selection_policy()? {
      Some(policy) => select_ready_hard_group_locked(&state, self.id, policy, now, force),
      None => select_single_hard_ticket_locked(&state, self.id),
    }
  }

  fn begin_group_execution(
    &self,
    tickets: &[DurabilityTicket],
  ) -> Result<(Vec<u64>, Vec<DurabilityOperation>), DurabilityCoordinatorError> {
    if tickets.is_empty() {
      return Err(DurabilityCoordinatorError::InvalidPlan("durability group cannot be empty".to_string()));
    }
    if tickets.len() > MAX_GROUP_COMMIT_RECORDS {
      return Err(DurabilityCoordinatorError::InvalidPlan(format!(
        "durability group contains {} tickets; maximum is {MAX_GROUP_COMMIT_RECORDS}",
        tickets.len()
      )));
    }
    let mut unique = HashSet::with_capacity(tickets.len());
    for ticket in tickets {
      self.validate_ticket(*ticket)?;
      if !unique.insert(ticket.sequence) {
        return Err(DurabilityCoordinatorError::InvalidPlan("durability group contains a duplicate ticket".to_string()));
      }
    }

    let mut sequences: Vec<_> = tickets.iter().map(|ticket| ticket.sequence).collect();
    sequences.sort_unstable();

    let mut state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
    let first_sequence = *sequences.first().ok_or_else(|| DurabilityCoordinatorError::invalid_plan("durability group cannot be empty"))?;
    let first = state.records.get(&first_sequence).ok_or(DurabilityCoordinatorError::UnknownTicket)?;
    let expected_plan = first.plan.clone();
    for sequence in &sequences {
      let record = state.records.get(sequence).ok_or(DurabilityCoordinatorError::UnknownTicket)?;
      if !matches!(record.status, CommitStatus::Admitted) {
        return Err(DurabilityCoordinatorError::AlreadyExecuted);
      }
      if record.plan != expected_plan {
        return Err(DurabilityCoordinatorError::InvalidPlan("durability group plans are not compatible".to_string()));
      }
    }

    if expected_plan.class == CommitClass::HardAuthority
      && !state.pending_hard.iter().take(sequences.len()).copied().eq(sequences.iter().copied())
    {
      return Err(DurabilityCoordinatorError::InvalidPlan("hard authority group must be the exact contiguous pending prefix".to_string()));
    }

    for sequence in &sequences {
      let record = state.records.get_mut(sequence).ok_or(DurabilityCoordinatorError::UnknownTicket)?;
      record.status = CommitStatus::Executing;
    }
    Ok((sequences, expected_plan.operations))
  }

  fn fail_group(
    &self,
    sequences: &[u64],
    operation: DurabilityOperation,
    message: String,
    disposition: DurabilityFailureDisposition,
    attempts: u8,
  ) -> Result<Vec<DurabilityFailure>, DurabilityCoordinatorError> {
    let message = bounded_failure_message(message);
    let mut state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
    let hard_authority = sequences
      .first()
      .and_then(|sequence| state.records.get(sequence))
      .is_some_and(|record| record.plan.class == CommitClass::HardAuthority);
    if hard_authority {
      return Ok(fail_pending_hard_locked(&mut state, operation, message, disposition, attempts));
    }

    let mut failures = Vec::with_capacity(sequences.len());
    for sequence in sequences {
      let failure = DurabilityFailure {
        sequence: *sequence,
        operation,
        message: message.clone(),
        os_error_class: disposition.os_error_class,
        retry_class: disposition.retry_class,
        attempts,
        serious: disposition.serious,
        uncertain_completion: disposition.uncertain_completion,
      };
      let record = state.records.get_mut(sequence).ok_or(DurabilityCoordinatorError::UnknownTicket)?;
      record.status = CommitStatus::Failed(failure.clone());
      failures.push(failure);
    }
    Ok(failures)
  }
}

fn ensure_waiter_admission_available(state: &CoordinatorState, class: CommitClass) -> Result<(), DurabilityCoordinatorError> {
  if class == CommitClass::HardAuthority {
    if let Some(failure) = &state.hard_failure {
      return Err(DurabilityCoordinatorError::ExecutorFailure(failure.clone()));
    }
  }
  if state.records.len() >= MAX_DURABILITY_WAITER_RECORDS {
    return Err(DurabilityCoordinatorError::ResourceExhausted(format!(
      "durability waiter record limit reached: {MAX_DURABILITY_WAITER_RECORDS}"
    )));
  }
  Ok(())
}

fn coordinator_has_nonterminal_work(state: &CoordinatorState) -> bool {
  state.driver_active
    || state.records.values().any(|record| matches!(record.status, CommitStatus::Admitted | CommitStatus::Executing))
    || !state.pending_hard.is_empty()
}

fn snapshot_locked(state: &CoordinatorState) -> DurabilityCoordinatorSnapshot {
  let mut admitted = 0;
  let mut executing = 0;
  let mut proven = 0;
  let mut failed = 0;
  for record in state.records.values() {
    match record.status {
      CommitStatus::Admitted => admitted += 1,
      CommitStatus::Executing => executing += 1,
      CommitStatus::Proven => proven += 1,
      CommitStatus::Failed(_) => failed += 1,
    }
  }
  let now = Instant::now();
  let oldest_pending_age_ms = state
    .records
    .values()
    .filter(|record| matches!(record.status, CommitStatus::Admitted | CommitStatus::Executing))
    .map(|record| now.saturating_duration_since(record.admitted_at).as_millis().min(u64::MAX as u128) as u64)
    .max();
  DurabilityCoordinatorSnapshot {
    hard_frontier: state.hard_frontier,
    next_sequence: state.next_sequence,
    admitted,
    executing,
    proven,
    failed,
    pending_hard: state.pending_hard.len(),
    driver_active: state.driver_active,
    oldest_pending_age_ms,
    last_barrier: state.last_barrier.clone(),
    ledger: state.ledger.iter().cloned().collect(),
  }
}

fn barrier_observation(
  sequences: &[u64],
  operation: DurabilityOperation,
  succeeded: bool,
  attempts: u8,
  started_at: Instant,
  error: Option<String>,
) -> DurabilityBarrierObservation {
  DurabilityBarrierObservation {
    operation,
    first_sequence: sequences.first().copied().unwrap_or_default(),
    last_sequence: sequences.last().copied().unwrap_or_default(),
    waiter_count: sequences.len(),
    succeeded,
    attempts,
    latency_ms: started_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
    completed_at_ms: chrono::Utc::now().timestamp_millis(),
    error: error.map(bounded_failure_message),
  }
}

fn fail_pending_hard_locked(
  state: &mut CoordinatorState,
  operation: DurabilityOperation,
  message: String,
  disposition: DurabilityFailureDisposition,
  attempts: u8,
) -> Vec<DurabilityFailure> {
  if let Some(failure) = &state.hard_failure {
    return vec![failure.clone()];
  }

  let message = bounded_failure_message(message);
  let pending: Vec<_> = state.pending_hard.drain(..).collect();
  let mut failures = Vec::with_capacity(pending.len());
  for sequence in pending {
    let failure = DurabilityFailure {
      sequence,
      operation,
      message: message.clone(),
      os_error_class: disposition.os_error_class,
      retry_class: disposition.retry_class,
      attempts,
      serious: disposition.serious,
      uncertain_completion: disposition.uncertain_completion,
    };
    if let Some(record) = state.records.get_mut(&sequence) {
      record.status = CommitStatus::Failed(failure.clone());
    }
    failures.push(failure);
  }
  if let Some(failure) = failures.first() {
    state.hard_failure = Some(failure.clone());
  }
  failures
}

fn bounded_failure_message(mut message: String) -> String {
  let maximum = MAX_DURABILITY_FAILURE_MESSAGE_BYTES as usize;
  if message.len() <= maximum {
    return message;
  }
  let mut end = maximum;
  while end > 0 && !message.is_char_boundary(end) {
    end -= 1;
  }
  message.truncate(end);
  message
}

pub enum DurabilityHardTurn<'a> {
  Complete(DurabilityWaiterState),
  Drive(DurabilityDrivePermit<'a>),
}

pub struct DurabilityDrivePermit<'a> {
  coordinator: &'a DurabilityCoordinator,
  active: bool,
}

impl DurabilityDrivePermit<'_> {
  pub fn release(mut self) {
    self.release_inner();
  }

  fn release_inner(&mut self) {
    if !self.active {
      return;
    }
    self.coordinator.release_drive_permit();
    self.active = false;
  }
}

impl Drop for DurabilityDrivePermit<'_> {
  fn drop(&mut self) {
    self.release_inner();
  }
}

fn waiter_state_locked(state: &CoordinatorState, ticket: DurabilityTicket) -> Result<DurabilityWaiterState, DurabilityCoordinatorError> {
  let record = state.records.get(&ticket.sequence).ok_or(DurabilityCoordinatorError::UnknownTicket)?;
  match &record.status {
    CommitStatus::Failed(failure) => Ok(DurabilityWaiterState::Failed(failure.clone())),
    CommitStatus::Proven if record.plan.class != CommitClass::HardAuthority || ticket.sequence <= state.hard_frontier => {
      Ok(DurabilityWaiterState::Succeeded(DurabilityCommitReceipt {
        sequence: ticket.sequence,
        class: record.plan.class,
        hard_frontier: state.hard_frontier,
      }))
    }
    CommitStatus::Admitted | CommitStatus::Executing | CommitStatus::Proven => Ok(DurabilityWaiterState::Pending),
  }
}

fn select_ready_hard_group_locked(
  state: &CoordinatorState,
  coordinator_id: uuid::Uuid,
  policy: DurabilityGroupPolicy,
  now: Instant,
  force: bool,
) -> Result<Vec<DurabilityTicket>, DurabilityCoordinatorError> {
  let Some(first_sequence) = state.pending_hard.front().copied() else {
    return Ok(Vec::new());
  };
  let Some(first) = state.records.get(&first_sequence) else {
    return Err(DurabilityCoordinatorError::UnknownTicket);
  };
  if !matches!(first.status, CommitStatus::Admitted) {
    return Ok(Vec::new());
  }

  let mut selected = Vec::new();
  let mut selected_bytes = 0u64;
  for sequence in &state.pending_hard {
    let Some(record) = state.records.get(sequence) else {
      return Err(DurabilityCoordinatorError::UnknownTicket);
    };
    if !matches!(record.status, CommitStatus::Admitted) || record.plan != first.plan {
      break;
    }
    if !selected.is_empty() && selected_bytes.saturating_add(record.estimated_bytes) > policy.max_bytes {
      break;
    }
    selected.push(DurabilityTicket { coordinator_id, sequence: *sequence });
    selected_bytes = selected_bytes.saturating_add(record.estimated_bytes);
    if selected_bytes >= policy.max_bytes {
      break;
    }
    if selected.len() >= MAX_GROUP_COMMIT_RECORDS {
      break;
    }
  }

  let elapsed = now.checked_duration_since(first.admitted_at).unwrap_or_default();
  if force || policy.max_delay.is_zero() || selected_bytes >= policy.max_bytes || elapsed >= policy.max_delay {
    Ok(selected)
  } else {
    Ok(Vec::new())
  }
}

fn select_single_hard_ticket_locked(
  state: &CoordinatorState,
  coordinator_id: uuid::Uuid,
) -> Result<Vec<DurabilityTicket>, DurabilityCoordinatorError> {
  let Some(sequence) = state.pending_hard.front().copied() else {
    return Ok(Vec::new());
  };
  let record = state.records.get(&sequence).ok_or(DurabilityCoordinatorError::UnknownTicket)?;
  if !matches!(record.status, CommitStatus::Admitted) {
    return Ok(Vec::new());
  }
  Ok(vec![DurabilityTicket { coordinator_id, sequence }])
}

fn durability_record_bytes(plan: &DurabilityCommitPlan) -> Result<u64, DurabilityCoordinatorError> {
  let operation_bytes = u64::try_from(plan.operations.capacity())
    .ok()
    .and_then(|capacity| capacity.checked_mul(std::mem::size_of::<DurabilityOperation>() as u64))
    .ok_or_else(|| DurabilityCoordinatorError::ResourceExhausted("commit-plan memory estimate overflow".to_string()))?;
  DURABILITY_RECORD_BASE_BYTES
    .checked_add(MAX_DURABILITY_FAILURE_MESSAGE_BYTES.saturating_mul(3))
    .and_then(|bytes| bytes.checked_add(operation_bytes))
    .ok_or_else(|| DurabilityCoordinatorError::ResourceExhausted("waiter-record memory estimate overflow".to_string()))
}

fn durability_memory_error(error: MemoryCoordinatorError) -> DurabilityCoordinatorError {
  match error {
    MemoryCoordinatorError::PolicyUnavailable
    | MemoryCoordinatorError::HardLimitExceeded { .. }
    | MemoryCoordinatorError::SoftPressureDeferred { .. }
    | MemoryCoordinatorError::EmergencyReserveExceeded { .. } => DurabilityCoordinatorError::ResourceExhausted(error.to_string()),
    other => DurabilityCoordinatorError::InvalidConfiguration(format!("memory coordinator rejected durability accounting: {other}")),
  }
}

struct SingleExecutorAdapter<'a, E> {
  executor: &'a mut E,
}

struct NativeFileBarrierExecutor<'a> {
  file: &'a File,
  barrier_kind: NativeFileBarrierKind,
  barrier: &'a dyn RecoverableFileBarrier,
}

impl DurabilityGroupExecutor for NativeFileBarrierExecutor<'_> {
  type Error = NativeDurabilityError;

  fn execute_group(&mut self, _sequences: &[u64], operation: DurabilityOperation) -> Result<(), Self::Error> {
    match operation {
      DurabilityOperation::DependencyAppend => Ok(()),
      DurabilityOperation::DataBarrier => self.barrier.execute_file_barrier(self.file, self.barrier_kind),
      _ => Err(NativeDurabilityError::invalid(
        NativeDurabilityOperation::FileBarrier,
        format!("unsupported operation in recoverable file-barrier plan: {operation:?}"),
      )),
    }
  }

  fn classify_error(&self, _operation: DurabilityOperation, error: &Self::Error, mutation_started: bool) -> DurabilityFailureDisposition {
    classify_native_durability_error(error, mutation_started)
  }
}

impl<E: DurabilityExecutor> DurabilityGroupExecutor for SingleExecutorAdapter<'_, E> {
  type Error = E::Error;

  fn execute_group(&mut self, sequences: &[u64], operation: DurabilityOperation) -> Result<(), Self::Error> {
    self.executor.execute(sequences[0], operation)
  }

  fn classify_error(&self, _operation: DurabilityOperation, _error: &Self::Error, _mutation_started: bool) -> DurabilityFailureDisposition {
    DurabilityFailureDisposition::serious(OsErrorClass::OtherPersistentIo, RetryClass::Never)
  }
}

struct GroupExecutionGuard<'a> {
  coordinator: &'a DurabilityCoordinator,
  sequences: &'a [u64],
  operation: Option<DurabilityOperation>,
  operation_started: Option<Instant>,
  armed: bool,
}

impl Drop for GroupExecutionGuard<'_> {
  fn drop(&mut self) {
    if !self.armed {
      return;
    }
    let Some(operation) = self.operation else {
      return;
    };
    let mut state = match self.coordinator.state.lock() {
      Ok(state) => state,
      Err(poisoned) => {
        if !self.coordinator.state_poison_reported.swap(true, Ordering::AcqRel) {
          crate::metrics::record_system_soft_failure(
            "durability_coordinator",
            "execution_guard_after_poison",
            self.coordinator.id,
            "executor unwound while poisoning coordinator state; pending operations were failed and normal APIs remain unavailable",
          );
        }
        poisoned.into_inner()
      }
    };
    let hard_authority = self
      .sequences
      .first()
      .and_then(|sequence| state.records.get(sequence))
      .is_some_and(|record| record.plan.class == CommitClass::HardAuthority);
    let disposition = DurabilityFailureDisposition::uncertain(OsErrorClass::TimeoutUnknown);
    if hard_authority {
      for sequence in self.sequences {
        state.record_ledger(DurabilityLedgerEntry { sequence: *sequence, operation, succeeded: false });
      }
      if is_barrier(operation) {
        state.last_barrier = Some(barrier_observation(
          self.sequences,
          operation,
          false,
          1,
          self.operation_started.unwrap_or_else(Instant::now),
          Some("durability executor unwound before reporting completion".to_string()),
        ));
      }
      fail_pending_hard_locked(
        &mut state,
        operation,
        "durability executor unwound before reporting completion".to_string(),
        disposition,
        1,
      );
      drop(state);
      self.coordinator.changed.notify_all();
      return;
    }

    for sequence in self.sequences {
      let failure = DurabilityFailure {
        sequence: *sequence,
        operation,
        message: "durability executor unwound before reporting completion".to_string(),
        os_error_class: disposition.os_error_class,
        retry_class: disposition.retry_class,
        attempts: 1,
        serious: disposition.serious,
        uncertain_completion: disposition.uncertain_completion,
      };
      state.record_ledger(DurabilityLedgerEntry { sequence: *sequence, operation, succeeded: false });
      if let Some(record) = state.records.get_mut(sequence) {
        if matches!(record.status, CommitStatus::Executing) {
          record.status = CommitStatus::Failed(failure);
        }
      }
    }
    if is_barrier(operation) {
      state.last_barrier = Some(barrier_observation(
        self.sequences,
        operation,
        false,
        1,
        self.operation_started.unwrap_or_else(Instant::now),
        Some("durability executor unwound before reporting completion".to_string()),
      ));
    }
    drop(state);
    self.coordinator.changed.notify_all();
  }
}

fn operation_may_mutate(operation: DurabilityOperation) -> bool {
  !matches!(
    operation,
    DurabilityOperation::DataBarrier
      | DurabilityOperation::AuthorityBarrier
      | DurabilityOperation::AuthorityReadback
      | DurabilityOperation::ParentDirectorySync
  )
}

fn should_retry(disposition: DurabilityFailureDisposition, attempts: u8) -> bool {
  if disposition.uncertain_completion || disposition.serious {
    return false;
  }
  match disposition.retry_class {
    RetryClass::Immediate => attempts < 8,
    RetryClass::BoundedBackoff => attempts < 3,
    RetryClass::None | RetryClass::AfterDependency | RetryClass::AfterRepair | RetryClass::Never => false,
  }
}

fn retry_backoff(attempts: u8) -> std::time::Duration {
  std::time::Duration::from_millis(1u64 << attempts.saturating_sub(1).min(6))
}

fn advance_hard_frontier(state: &mut CoordinatorState) {
  while let Some(sequence) = state.pending_hard.front().copied() {
    let Some(record) = state.records.get(&sequence) else {
      break;
    };
    if !matches!(record.status, CommitStatus::Proven) {
      break;
    }
    state.hard_frontier = sequence;
    state.pending_hard.pop_front();
  }
}

#[cfg(test)]
#[path = "../../spec/engine/durability_coordinator_internal_spec.rs"]
mod durability_coordinator_internal_spec;

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn native_error_classes_have_exhaustive_fail_closed_dispositions() {
    let cases = [
      (NativeDurabilityErrorClass::Unsupported, Some(OsErrorClass::UnsupportedDurability), RetryClass::Never, true, false),
      (NativeDurabilityErrorClass::UncertainCompletion, Some(OsErrorClass::TimeoutUnknown), RetryClass::AfterRepair, true, true),
      (NativeDurabilityErrorClass::Verification, Some(OsErrorClass::ChecksumReadback), RetryClass::Never, true, false),
      (NativeDurabilityErrorClass::InvalidInput, None, RetryClass::Never, false, false),
    ];

    for (class, os_error_class, retry_class, serious, uncertain_completion) in cases {
      let disposition = classify_native_error_class(class, None, None, false);
      assert_eq!(disposition.os_error_class, os_error_class);
      assert_eq!(disposition.retry_class, retry_class);
      assert_eq!(disposition.serious, serious);
      assert_eq!(disposition.uncertain_completion, uncertain_completion);
    }

    let interrupted = classify_native_error_class(NativeDurabilityErrorClass::Io, None, Some(std::io::ErrorKind::Interrupted), false);
    assert_eq!(interrupted.os_error_class, Some(OsErrorClass::InterruptedNoProgress));
    assert_eq!(interrupted.retry_class, RetryClass::Immediate);
    assert!(!interrupted.serious);
  }
}
