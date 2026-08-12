use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use serde::{Serialize, Serializer};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::engine::HashAlgorithm;

const MAXIMUM_STATUS_MESSAGE_BYTES: usize = 4 * 1_024;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GcRunIDV1([u8; 16]);

impl GcRunIDV1 {
  pub fn new(bytes: [u8; 16]) -> Result<Self, GcRunErrorV1> {
    if bytes.iter().all(|byte| *byte == 0) {
      return Err(GcRunErrorV1::invalid("gc_run_id", "GC run identity must not be all zeroes"));
    }
    Ok(Self(bytes))
  }

  pub fn as_bytes(self) -> [u8; 16] {
    self.0
  }
}

impl fmt::Debug for GcRunIDV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.debug_tuple("GcRunIDV1").field(&uuid::Uuid::from_bytes(self.0).to_string()).finish()
  }
}

impl fmt::Display for GcRunIDV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    fmt::Display::fmt(&uuid::Uuid::from_bytes(self.0), formatter)
  }
}

impl Serialize for GcRunIDV1 {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    serializer.serialize_str(&uuid::Uuid::from_bytes(self.0).to_string())
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GcRunInvocationV1 {
  Cli,
  Http,
  Task,
  Scheduled,
  RepairFollowUp,
  Embedded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GcRunModeV1 {
  NonDestructiveMark,
  Destructive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GcRunPhaseV1 {
  Prepare,
  Inventory,
  Mark,
  MutationConvergence,
  Finalize,
}

impl GcRunPhaseV1 {
  pub const NON_DESTRUCTIVE: [Self; 5] = [Self::Prepare, Self::Inventory, Self::Mark, Self::MutationConvergence, Self::Finalize];

  pub const fn name(self) -> &'static str {
    match self {
      Self::Prepare => "prepare",
      Self::Inventory => "inventory",
      Self::Mark => "mark",
      Self::MutationConvergence => "mutation_convergence",
      Self::Finalize => "finalize",
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GcRunStateV1 {
  Running,
  Complete,
  Incomplete,
  Cancelled,
  Failed,
  Refused,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcRunBasisV1 {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: [u8; 16],
  pub generation: u64,
  pub authority_root_set_digest: Vec<u8>,
  pub semantic_state_digest: Vec<u8>,
  pub kv_layout_generation: u64,
  pub kv_layout_fingerprint: Vec<u8>,
  pub effective_policy_fingerprint: [u8; 32],
  pub system_family_registry_fingerprint: [u8; 32],
  pub captured_header_sequence: u64,
  pub captured_write_high_water: u64,
  pub reconciled_through_sequence: u64,
  pub mutation_journal_head: Option<Vec<u8>>,
}

impl GcRunBasisV1 {
  fn validate(&self) -> Result<(), GcRunErrorV1> {
    if self.database_id.iter().all(|byte| *byte == 0)
      || self.generation == 0
      || self.kv_layout_generation == 0
      || self.captured_header_sequence == 0
      || self.captured_write_high_water == 0
      || self.reconciled_through_sequence > self.captured_write_high_water
    {
      return Err(GcRunErrorV1::invalid(
        "gc_run_basis_identity",
        "GC basis requires nonzero database, generation, layout, and authority boundaries with reconciliation at or below capture",
      ));
    }
    let hash_width = self.hash_algorithm.hash_length();
    for (name, digest) in [
      ("authority root set", self.authority_root_set_digest.as_slice()),
      ("semantic state", self.semantic_state_digest.as_slice()),
      ("KV layout", self.kv_layout_fingerprint.as_slice()),
    ] {
      if digest.len() != hash_width || digest.iter().all(|byte| *byte == 0) {
        return Err(GcRunErrorV1::invalid(
          "gc_run_basis_hash_width",
          format!("GC {name} digest must be a nonzero value with the selected hash width"),
        ));
      }
    }
    if self.effective_policy_fingerprint.iter().all(|byte| *byte == 0)
      || self.system_family_registry_fingerprint.iter().all(|byte| *byte == 0)
    {
      return Err(GcRunErrorV1::invalid("gc_run_basis_policy", "GC policy and system-family fingerprints must be nonzero"));
    }
    if self.mutation_journal_head.as_ref().is_some_and(|digest| digest.len() != hash_width || digest.iter().all(|byte| *byte == 0)) {
      return Err(GcRunErrorV1::invalid(
        "gc_run_basis_journal",
        "GC mutation-journal head must be absent or a nonzero value with the selected hash width",
      ));
    }
    Ok(())
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct GcRunBudgetsV1 {
  pub memory_minimum_bytes: u64,
  pub memory_maximum_bytes: u64,
  pub scratch_maximum_bytes: u64,
  pub scratch_minimum_free_bytes: u64,
}

impl GcRunBudgetsV1 {
  pub fn new(
    memory_minimum_bytes: u64,
    memory_maximum_bytes: u64,
    scratch_maximum_bytes: u64,
    scratch_minimum_free_bytes: u64,
  ) -> Result<Self, GcRunErrorV1> {
    if memory_minimum_bytes == 0
      || memory_maximum_bytes < memory_minimum_bytes
      || scratch_maximum_bytes == 0
      || scratch_minimum_free_bytes == 0
    {
      return Err(GcRunErrorV1::invalid(
        "gc_run_budgets",
        "GC budgets require nonzero memory and scratch bounds, with maximum memory at least the minimum",
      ));
    }
    Ok(Self { memory_minimum_bytes, memory_maximum_bytes, scratch_maximum_bytes, scratch_minimum_free_bytes })
  }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GcRunProgressUpdateV1 {
  pub phase_progress: f64,
  pub completed_units: u64,
  pub total_units: Option<u64>,
  pub eta_ms: Option<u64>,
  pub memory_reserved_bytes: u64,
  pub scratch_used_bytes: u64,
  pub mutation_journal_lag: u64,
  pub checkpoint_age_ms: Option<u64>,
  pub message: Option<String>,
}

impl Default for GcRunProgressUpdateV1 {
  fn default() -> Self {
    Self {
      phase_progress: 0.0,
      completed_units: 0,
      total_units: None,
      eta_ms: None,
      memory_reserved_bytes: 0,
      scratch_used_bytes: 0,
      mutation_journal_lag: 0,
      checkpoint_age_ms: None,
      message: None,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GcRunStatusV1 {
  pub run_id: GcRunIDV1,
  pub invocation: GcRunInvocationV1,
  pub mode: GcRunModeV1,
  pub state: GcRunStateV1,
  pub phase: Option<GcRunPhaseV1>,
  pub phase_progress: f64,
  pub overall_progress: f64,
  pub completed_units: u64,
  pub total_units: Option<u64>,
  pub eta_ms: Option<u64>,
  pub memory_reserved_bytes: u64,
  pub scratch_used_bytes: u64,
  pub mutation_journal_lag: u64,
  pub checkpoint_age_ms: Option<u64>,
  pub started_at_ms: i64,
  pub observed_at_ms: i64,
  pub completed_at_ms: Option<i64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub code: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub message: Option<String>,
}

pub trait GcRunProgressSinkV1: Send + Sync {
  fn publish(&self, status: &GcRunStatusV1);
}

#[derive(Debug, Default)]
pub struct NoopGcRunProgressSinkV1;

impl GcRunProgressSinkV1 for NoopGcRunProgressSinkV1 {
  fn publish(&self, _status: &GcRunStatusV1) {}
}

pub struct GcRunContextV1 {
  run_id: GcRunIDV1,
  invocation: GcRunInvocationV1,
  mode: GcRunModeV1,
  started_at_ms: i64,
  budgets: GcRunBudgetsV1,
  cancellation: CancellationToken,
  progress_sink: Arc<dyn GcRunProgressSinkV1>,
  basis: OnceLock<GcRunBasisV1>,
  started: AtomicBool,
}

impl fmt::Debug for GcRunContextV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("GcRunContextV1")
      .field("run_id", &self.run_id)
      .field("invocation", &self.invocation)
      .field("mode", &self.mode)
      .field("started_at_ms", &self.started_at_ms)
      .field("budgets", &self.budgets)
      .field("cancelled", &self.cancellation.is_cancelled())
      .field("basis_captured", &self.basis.get().is_some())
      .field("started", &self.started.load(Ordering::Acquire))
      .finish_non_exhaustive()
  }
}

impl GcRunContextV1 {
  pub fn new(
    run_id: GcRunIDV1,
    invocation: GcRunInvocationV1,
    mode: GcRunModeV1,
    started_at_ms: i64,
    budgets: GcRunBudgetsV1,
    cancellation: CancellationToken,
    progress_sink: Arc<dyn GcRunProgressSinkV1>,
  ) -> Result<Self, GcRunErrorV1> {
    if started_at_ms <= 0 {
      return Err(GcRunErrorV1::invalid("gc_run_started_at", "GC run start timestamp must be positive"));
    }
    Ok(Self {
      run_id,
      invocation,
      mode,
      started_at_ms,
      budgets,
      cancellation,
      progress_sink,
      basis: OnceLock::new(),
      started: AtomicBool::new(false),
    })
  }

  pub fn run_id(&self) -> GcRunIDV1 {
    self.run_id
  }

  pub fn invocation(&self) -> GcRunInvocationV1 {
    self.invocation
  }

  pub fn mode(&self) -> GcRunModeV1 {
    self.mode
  }

  pub fn budgets(&self) -> GcRunBudgetsV1 {
    self.budgets
  }

  pub fn cancellation(&self) -> &CancellationToken {
    &self.cancellation
  }

  pub fn basis(&self) -> Option<&GcRunBasisV1> {
    self.basis.get()
  }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("{message}")]
pub struct GcRunErrorV1 {
  code: &'static str,
  message: String,
}

impl GcRunErrorV1 {
  pub fn operation(code: &'static str, message: impl Into<String>) -> Self {
    if !valid_status_code(code) {
      return Self::invalid("gc_run_operation_error_code", "GC operation returned an invalid error code");
    }
    Self::invalid(code, message)
  }

  pub fn code(&self) -> &'static str {
    self.code
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self { code, message: message.into() }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GcRunPhaseOutcomeV1 {
  Continue,
  Incomplete { code: &'static str, message: String },
}

pub trait GcRunOperationV1 {
  fn execute_phase(&mut self, phase: GcRunPhaseV1, reporter: &mut GcRunPhaseReporterV1<'_>) -> Result<GcRunPhaseOutcomeV1, GcRunErrorV1>;
}

pub struct GcRunPhaseReporterV1<'a> {
  context: &'a GcRunContextV1,
  phase_index: usize,
  status: &'a mut GcRunStatusV1,
}

impl GcRunPhaseReporterV1<'_> {
  pub fn capture_basis(&mut self, basis: GcRunBasisV1) -> Result<(), GcRunErrorV1> {
    if self.status.phase != Some(GcRunPhaseV1::Prepare) {
      return Err(GcRunErrorV1::invalid("gc_run_basis_phase", "GC basis may only be captured during preparation"));
    }
    basis.validate()?;
    match self.context.basis.set(basis) {
      Ok(()) => Ok(()),
      Err(conflicting_basis) => Err(GcRunErrorV1::invalid(
        "gc_run_basis_duplicate",
        format!("GC basis was already captured; conflicting generation was {}", conflicting_basis.generation),
      )),
    }
  }

  pub fn check_cancellation(&self) -> Result<(), GcRunErrorV1> {
    if self.context.cancellation.is_cancelled() {
      return Err(GcRunErrorV1::invalid("gc_run_cancelled", "garbage collection was cancelled"));
    }
    Ok(())
  }

  pub fn report(&mut self, update: GcRunProgressUpdateV1) -> Result<(), GcRunErrorV1> {
    self.check_cancellation()?;
    if !update.phase_progress.is_finite() || !(0.0..=1.0).contains(&update.phase_progress) {
      return Err(GcRunErrorV1::invalid("gc_run_progress", "GC phase progress must be a finite value between zero and one"));
    }
    if update.phase_progress < self.status.phase_progress {
      return Err(GcRunErrorV1::invalid("gc_run_progress_regressed", "GC phase progress must not move backwards"));
    }
    if update.total_units.is_some_and(|total| update.completed_units > total) {
      return Err(GcRunErrorV1::invalid("gc_run_progress_units", "GC completed work exceeds total work"));
    }
    if update.memory_reserved_bytes > self.context.budgets.memory_maximum_bytes {
      return Err(GcRunErrorV1::invalid("gc_run_memory_budget", "GC reported memory beyond its captured maximum"));
    }
    if update.scratch_used_bytes > self.context.budgets.scratch_maximum_bytes {
      return Err(GcRunErrorV1::invalid("gc_run_scratch_budget", "GC reported scratch use beyond its captured maximum"));
    }
    if update.message.as_ref().is_some_and(|message| message.len() > MAXIMUM_STATUS_MESSAGE_BYTES) {
      return Err(GcRunErrorV1::invalid("gc_run_progress_message", "GC progress message exceeds its bounded status limit"));
    }

    self.status.phase_progress = update.phase_progress;
    self.status.overall_progress = (self.phase_index as f64 + update.phase_progress) / GcRunPhaseV1::NON_DESTRUCTIVE.len() as f64;
    self.status.completed_units = update.completed_units;
    self.status.total_units = update.total_units;
    self.status.eta_ms = update.eta_ms;
    self.status.memory_reserved_bytes = update.memory_reserved_bytes;
    self.status.scratch_used_bytes = update.scratch_used_bytes;
    self.status.mutation_journal_lag = update.mutation_journal_lag;
    self.status.checkpoint_age_ms = update.checkpoint_age_ms;
    self.status.message = update.message;
    refresh_observed_at(self.status);
    self.context.progress_sink.publish(self.status);
    Ok(())
  }

  fn complete_phase(&mut self) {
    self.status.phase_progress = 1.0;
    self.status.overall_progress = (self.phase_index + 1) as f64 / GcRunPhaseV1::NON_DESTRUCTIVE.len() as f64;
    refresh_observed_at(self.status);
    self.context.progress_sink.publish(self.status);
  }
}

pub fn execute_gc_run_v1(context: &GcRunContextV1, operation: &mut dyn GcRunOperationV1) -> Result<GcRunStatusV1, GcRunErrorV1> {
  match context.started.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire) {
    Ok(false) => {}
    Err(true) => return Err(GcRunErrorV1::invalid("gc_run_context_reused", "GC run context and identity may be executed only once")),
    Ok(true) | Err(false) => {
      return Err(GcRunErrorV1::invalid(
        "gc_run_context_state",
        "GC run context execution flag returned an impossible compare-exchange state",
      ));
    }
  }
  let mut status = initial_status(context);
  if context.mode == GcRunModeV1::Destructive {
    let error = GcRunErrorV1::invalid(
      "gc_run_destructive_disabled",
      "destructive v4 garbage collection is disabled until the P4-9 operator activation gate",
    );
    publish_terminal(context, &mut status, GcRunStateV1::Refused, &error);
    return Err(error);
  }
  if context.cancellation.is_cancelled() {
    let error = GcRunErrorV1::invalid("gc_run_cancelled", "garbage collection was cancelled before preparation");
    publish_terminal(context, &mut status, GcRunStateV1::Cancelled, &error);
    return Err(error);
  }

  for (phase_index, phase) in GcRunPhaseV1::NON_DESTRUCTIVE.iter().copied().enumerate() {
    if context.cancellation.is_cancelled() {
      let error = GcRunErrorV1::invalid("gc_run_cancelled", format!("garbage collection was cancelled before {}", phase.name()));
      publish_terminal(context, &mut status, GcRunStateV1::Cancelled, &error);
      return Err(error);
    }
    status.phase = Some(phase);
    status.phase_progress = 0.0;
    status.overall_progress = phase_index as f64 / GcRunPhaseV1::NON_DESTRUCTIVE.len() as f64;
    refresh_observed_at(&mut status);
    context.progress_sink.publish(&status);

    let outcome = {
      let mut reporter = GcRunPhaseReporterV1 { context, phase_index, status: &mut status };
      match operation.execute_phase(phase, &mut reporter) {
        Ok(outcome) => {
          if phase == GcRunPhaseV1::Prepare && matches!(outcome, GcRunPhaseOutcomeV1::Continue) && context.basis.get().is_none() {
            let error = GcRunErrorV1::invalid("gc_run_basis_missing", "GC preparation completed without a frozen authority basis");
            publish_terminal(context, reporter.status, GcRunStateV1::Failed, &error);
            return Err(error);
          }
          reporter.complete_phase();
          outcome
        }
        Err(error) => {
          let state = if error.code() == "gc_run_cancelled" { GcRunStateV1::Cancelled } else { GcRunStateV1::Failed };
          publish_terminal(context, reporter.status, state, &error);
          return Err(error);
        }
      }
    };
    if let GcRunPhaseOutcomeV1::Incomplete { code, message } = outcome {
      if !valid_status_code(code) || message.is_empty() {
        let error = GcRunErrorV1::invalid(
          "gc_run_incomplete_contract",
          "GC incomplete outcome requires a bounded snake-case code and nonempty message",
        );
        publish_terminal(context, &mut status, GcRunStateV1::Failed, &error);
        return Err(error);
      }
      let incomplete = GcRunErrorV1::invalid(code, message);
      publish_terminal(context, &mut status, GcRunStateV1::Incomplete, &incomplete);
      return Ok(status);
    }
  }

  status.state = GcRunStateV1::Complete;
  status.code = None;
  status.message = None;
  refresh_observed_at(&mut status);
  status.completed_at_ms = Some(status.observed_at_ms);
  context.progress_sink.publish(&status);
  Ok(status)
}

fn initial_status(context: &GcRunContextV1) -> GcRunStatusV1 {
  GcRunStatusV1 {
    run_id: context.run_id,
    invocation: context.invocation,
    mode: context.mode,
    state: GcRunStateV1::Running,
    phase: None,
    phase_progress: 0.0,
    overall_progress: 0.0,
    completed_units: 0,
    total_units: None,
    eta_ms: None,
    memory_reserved_bytes: 0,
    scratch_used_bytes: 0,
    mutation_journal_lag: 0,
    checkpoint_age_ms: None,
    started_at_ms: context.started_at_ms,
    observed_at_ms: wall_clock_ms().max(context.started_at_ms),
    completed_at_ms: None,
    code: None,
    message: None,
  }
}

fn publish_terminal(context: &GcRunContextV1, status: &mut GcRunStatusV1, state: GcRunStateV1, error: &GcRunErrorV1) {
  status.state = state;
  status.code = Some(error.code().to_string());
  status.message = Some(bounded_status_message(&error.to_string()));
  refresh_observed_at(status);
  status.completed_at_ms = Some(status.observed_at_ms);
  context.progress_sink.publish(status);
}

fn bounded_status_message(message: &str) -> String {
  if message.len() <= MAXIMUM_STATUS_MESSAGE_BYTES {
    return message.to_string();
  }
  let mut boundary = MAXIMUM_STATUS_MESSAGE_BYTES.saturating_sub(3);
  while boundary > 0 && !message.is_char_boundary(boundary) {
    boundary -= 1;
  }
  format!("{}...", &message[..boundary])
}

fn valid_status_code(code: &str) -> bool {
  !code.is_empty() && code.len() <= 128 && code.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn refresh_observed_at(status: &mut GcRunStatusV1) {
  status.observed_at_ms = status.observed_at_ms.max(wall_clock_ms());
}

fn wall_clock_ms() -> i64 {
  chrono::Utc::now().timestamp_millis()
}
