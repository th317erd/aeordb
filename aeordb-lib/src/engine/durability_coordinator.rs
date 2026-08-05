//! Shared durability admission, operation-ledger, and hard-frontier ownership.
//!
//! This module starts as a non-activating contract shell. Existing v3 writers
//! are migrated behind it in later P2a landing units; until then it must not
//! change their persistent bytes or acknowledgement behavior.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::Mutex;

use crate::engine::v4::contract_generated::durability_operation_v1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitClass {
  HardAuthority,
  RecoverableSoftState,
  Disposable,
}

impl CommitClass {
  pub const ALL: [Self; 3] = [Self::HardAuthority, Self::RecoverableSoftState, Self::Disposable];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurabilityCoordinatorSnapshot {
  pub hard_frontier: u64,
  pub next_sequence: u64,
  pub admitted: usize,
  pub executing: usize,
  pub proven: usize,
  pub failed: usize,
  pub pending_hard: usize,
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

#[derive(Clone, Debug)]
struct CommitRecord {
  plan: DurabilityCommitPlan,
  status: CommitStatus,
}

#[derive(Debug)]
struct CoordinatorState {
  next_sequence: u64,
  hard_frontier: u64,
  records: BTreeMap<u64, CommitRecord>,
  pending_hard: VecDeque<u64>,
  ledger: VecDeque<DurabilityLedgerEntry>,
  ledger_capacity: usize,
}

impl CoordinatorState {
  fn new(ledger_capacity: usize) -> Self {
    Self {
      next_sequence: 1,
      hard_frontier: 0,
      records: BTreeMap::new(),
      pending_hard: VecDeque::new(),
      ledger: VecDeque::new(),
      ledger_capacity,
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

#[derive(Debug)]
pub struct DurabilityCoordinator {
  id: uuid::Uuid,
  state: Mutex<CoordinatorState>,
}

impl Default for DurabilityCoordinator {
  fn default() -> Self {
    Self::new()
  }
}

impl DurabilityCoordinator {
  pub fn new() -> Self {
    Self { id: uuid::Uuid::new_v4(), state: Mutex::new(CoordinatorState::new(DEFAULT_DURABILITY_LEDGER_CAPACITY)) }
  }

  pub fn with_ledger_capacity(ledger_capacity: usize) -> Result<Self, DurabilityCoordinatorError> {
    if ledger_capacity == 0 {
      return Err(DurabilityCoordinatorError::InvalidConfiguration("ledger capacity must be nonzero".to_string()));
    }
    Ok(Self { id: uuid::Uuid::new_v4(), state: Mutex::new(CoordinatorState::new(ledger_capacity)) })
  }

  pub fn admit(&self, plan: DurabilityCommitPlan) -> Result<DurabilityTicket, DurabilityCoordinatorError> {
    let mut state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
    let sequence = state.next_sequence;
    state.next_sequence = state.next_sequence.checked_add(1).ok_or(DurabilityCoordinatorError::SequenceExhausted)?;
    if plan.class == CommitClass::HardAuthority {
      state.pending_hard.push_back(sequence);
    }
    state.records.insert(sequence, CommitRecord { plan, status: CommitStatus::Admitted });
    Ok(DurabilityTicket { coordinator_id: self.id, sequence })
  }

  pub fn execute<E: DurabilityExecutor>(&self, ticket: DurabilityTicket, executor: &mut E) -> Result<(), DurabilityCoordinatorError> {
    self.validate_ticket(ticket)?;
    let operations = {
      let mut state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
      let record = state.records.get_mut(&ticket.sequence).ok_or(DurabilityCoordinatorError::UnknownTicket)?;
      if !matches!(record.status, CommitStatus::Admitted) {
        return Err(DurabilityCoordinatorError::AlreadyExecuted);
      }
      record.status = CommitStatus::Executing;
      record.plan.operations.clone()
    };

    let mut execution_guard = ExecutionGuard { coordinator: self, ticket, operation: None, armed: true };
    for operation in operations {
      execution_guard.operation = Some(operation);
      match executor.execute(ticket.sequence, operation) {
        Ok(()) => {
          let mut state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
          state.record_ledger(DurabilityLedgerEntry { sequence: ticket.sequence, operation, succeeded: true });
          execution_guard.operation = None;
        }
        Err(error) => {
          let failure = DurabilityFailure { sequence: ticket.sequence, operation, message: error.to_string() };
          let mut state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
          state.record_ledger(DurabilityLedgerEntry { sequence: ticket.sequence, operation, succeeded: false });
          let record = state.records.get_mut(&ticket.sequence).ok_or(DurabilityCoordinatorError::UnknownTicket)?;
          record.status = CommitStatus::Failed(failure.clone());
          execution_guard.armed = false;
          return Err(DurabilityCoordinatorError::ExecutorFailure(failure));
        }
      }
    }

    let mut state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
    let record = state.records.get_mut(&ticket.sequence).ok_or(DurabilityCoordinatorError::UnknownTicket)?;
    record.status = CommitStatus::Proven;
    advance_hard_frontier(&mut state);
    execution_guard.armed = false;
    Ok(())
  }

  pub fn waiter_state(&self, ticket: DurabilityTicket) -> Result<DurabilityWaiterState, DurabilityCoordinatorError> {
    self.validate_ticket(ticket)?;
    let state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
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

  pub fn take_waiter_state(&self, ticket: DurabilityTicket) -> Result<DurabilityWaiterState, DurabilityCoordinatorError> {
    let waiter_state = self.waiter_state(ticket)?;
    if !matches!(waiter_state, DurabilityWaiterState::Pending) {
      let mut state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
      state.records.remove(&ticket.sequence).ok_or(DurabilityCoordinatorError::UnknownTicket)?;
    }
    Ok(waiter_state)
  }

  pub fn snapshot(&self) -> Result<DurabilityCoordinatorSnapshot, DurabilityCoordinatorError> {
    let state = self.state.lock().map_err(|_| DurabilityCoordinatorError::StateUnavailable)?;
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
    Ok(DurabilityCoordinatorSnapshot {
      hard_frontier: state.hard_frontier,
      next_sequence: state.next_sequence,
      admitted,
      executing,
      proven,
      failed,
      pending_hard: state.pending_hard.len(),
      ledger: state.ledger.iter().cloned().collect(),
    })
  }

  fn validate_ticket(&self, ticket: DurabilityTicket) -> Result<(), DurabilityCoordinatorError> {
    if ticket.coordinator_id == self.id {
      Ok(())
    } else {
      Err(DurabilityCoordinatorError::ForeignTicket)
    }
  }
}

struct ExecutionGuard<'a> {
  coordinator: &'a DurabilityCoordinator,
  ticket: DurabilityTicket,
  operation: Option<DurabilityOperation>,
  armed: bool,
}

impl Drop for ExecutionGuard<'_> {
  fn drop(&mut self) {
    if !self.armed {
      return;
    }
    let Some(operation) = self.operation else {
      return;
    };
    let failure = DurabilityFailure {
      sequence: self.ticket.sequence,
      operation,
      message: "durability executor unwound before reporting completion".to_string(),
    };
    let Ok(mut state) = self.coordinator.state.lock() else {
      return;
    };
    state.record_ledger(DurabilityLedgerEntry { sequence: self.ticket.sequence, operation, succeeded: false });
    if let Some(record) = state.records.get_mut(&self.ticket.sequence) {
      if matches!(record.status, CommitStatus::Executing) {
        record.status = CommitStatus::Failed(failure);
      }
    }
  }
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
