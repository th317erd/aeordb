use std::sync::{Arc, Mutex};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::gc_run::{
  execute_gc_run_v1, GcRunBasisV1, GcRunBudgetsV1, GcRunContextV1, GcRunErrorV1, GcRunIDV1, GcRunInvocationV1, GcRunModeV1,
  GcRunOperationV1, GcRunPhaseOutcomeV1, GcRunPhaseReporterV1, GcRunPhaseV1, GcRunProgressSinkV1, GcRunProgressUpdateV1, GcRunStateV1,
  GcRunStatusV1,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct RecordingSink {
  statuses: Mutex<Vec<GcRunStatusV1>>,
}

impl RecordingSink {
  fn statuses(&self) -> Vec<GcRunStatusV1> {
    self.statuses.lock().unwrap().clone()
  }
}

impl GcRunProgressSinkV1 for RecordingSink {
  fn publish(&self, status: &GcRunStatusV1) {
    self.statuses.lock().unwrap().push(status.clone());
  }
}

struct RecordingOperation {
  phases: Vec<GcRunPhaseV1>,
  cancellation: CancellationToken,
  cancel_after: Option<GcRunPhaseV1>,
  incomplete_at: Option<GcRunPhaseV1>,
  regress_progress_at: Option<GcRunPhaseV1>,
}

impl RecordingOperation {
  fn new(cancellation: CancellationToken) -> Self {
    Self { phases: Vec::new(), cancellation, cancel_after: None, incomplete_at: None, regress_progress_at: None }
  }
}

impl GcRunOperationV1 for RecordingOperation {
  fn execute_phase(&mut self, phase: GcRunPhaseV1, reporter: &mut GcRunPhaseReporterV1<'_>) -> Result<GcRunPhaseOutcomeV1, GcRunErrorV1> {
    self.phases.push(phase);
    if phase == GcRunPhaseV1::Prepare {
      reporter.capture_basis(test_basis())?;
    }
    reporter.report(GcRunProgressUpdateV1 {
      phase_progress: 0.5,
      completed_units: 5,
      total_units: Some(10),
      eta_ms: Some(1_000),
      memory_reserved_bytes: 4 * 1_024 * 1_024,
      scratch_used_bytes: 8 * 1_024 * 1_024,
      mutation_journal_lag: 3,
      checkpoint_age_ms: Some(250),
      message: Some(format!("{} halfway", phase.name())),
    })?;
    if self.regress_progress_at == Some(phase) {
      reporter.report(GcRunProgressUpdateV1 { phase_progress: 0.25, ..GcRunProgressUpdateV1::default() })?;
    }
    if self.cancel_after == Some(phase) {
      self.cancellation.cancel();
    }
    if self.incomplete_at == Some(phase) {
      return Ok(GcRunPhaseOutcomeV1::Incomplete {
        code: "test_incomplete",
        message: "test deliberately stopped without publication".to_string(),
      });
    }
    Ok(GcRunPhaseOutcomeV1::Continue)
  }
}

fn test_basis() -> GcRunBasisV1 {
  GcRunBasisV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    database_id: [0x21; 16],
    generation: 7,
    authority_root_set_digest: vec![0x31; 32],
    semantic_state_digest: vec![0x32; 32],
    kv_layout_generation: 11,
    kv_layout_fingerprint: vec![0x33; 32],
    effective_policy_fingerprint: [0x34; 32],
    system_family_registry_fingerprint: [0x35; 32],
    captured_header_sequence: 13,
    captured_write_high_water: 17,
    reconciled_through_sequence: 17,
    mutation_journal_head: Some(vec![0x36; 32]),
  }
}

fn run_context(mode: GcRunModeV1, cancellation: CancellationToken, sink: Arc<RecordingSink>) -> GcRunContextV1 {
  GcRunContextV1::new(
    GcRunIDV1::new([0x41; 16]).unwrap(),
    GcRunInvocationV1::Embedded,
    mode,
    10_000,
    GcRunBudgetsV1::new(64 * 1_024 * 1_024, 128 * 1_024 * 1_024, 2 * 1_024 * 1_024 * 1_024, 8 * 1_024 * 1_024).unwrap(),
    cancellation,
    sink,
  )
  .unwrap()
}

#[test]
fn non_destructive_executor_owns_exact_phase_order_and_monotonic_status() {
  let cancellation = CancellationToken::new();
  let sink = Arc::new(RecordingSink::default());
  let context = run_context(GcRunModeV1::NonDestructiveMark, cancellation.clone(), sink.clone());
  let mut operation = RecordingOperation::new(cancellation);

  let terminal = execute_gc_run_v1(&context, &mut operation).unwrap();

  assert_eq!(operation.phases, GcRunPhaseV1::NON_DESTRUCTIVE);
  assert_eq!(terminal.state, GcRunStateV1::Complete);
  assert_eq!(terminal.phase, Some(GcRunPhaseV1::Finalize));
  assert_eq!(terminal.phase_progress, 1.0);
  assert_eq!(terminal.overall_progress, 1.0);
  assert_eq!(terminal.completed_at_ms, Some(terminal.observed_at_ms));
  assert_eq!(context.basis(), Some(&test_basis()));

  let statuses = sink.statuses();
  assert_eq!(statuses.first().unwrap().state, GcRunStateV1::Running);
  assert_eq!(statuses.last().unwrap(), &terminal);
  assert!(statuses.windows(2).all(|window| window[0].overall_progress <= window[1].overall_progress));
  assert!(statuses.iter().all(|status| status.run_id == GcRunIDV1::new([0x41; 16]).unwrap()));
  assert!(statuses.iter().any(|status| status.mutation_journal_lag == 3 && status.checkpoint_age_ms == Some(250)));
}

#[test]
fn cancellation_before_and_between_phases_never_advances_later_work() {
  let already_cancelled = CancellationToken::new();
  already_cancelled.cancel();
  let sink = Arc::new(RecordingSink::default());
  let context = run_context(GcRunModeV1::NonDestructiveMark, already_cancelled.clone(), sink.clone());
  let mut operation = RecordingOperation::new(already_cancelled);
  let error = execute_gc_run_v1(&context, &mut operation).unwrap_err();
  assert_eq!(error.code(), "gc_run_cancelled");
  assert!(operation.phases.is_empty());
  assert_eq!(sink.statuses().last().unwrap().state, GcRunStateV1::Cancelled);

  let cancellation = CancellationToken::new();
  let sink = Arc::new(RecordingSink::default());
  let context = run_context(GcRunModeV1::NonDestructiveMark, cancellation.clone(), sink.clone());
  let mut operation = RecordingOperation::new(cancellation);
  operation.cancel_after = Some(GcRunPhaseV1::Inventory);
  let error = execute_gc_run_v1(&context, &mut operation).unwrap_err();
  assert_eq!(error.code(), "gc_run_cancelled");
  assert_eq!(operation.phases, vec![GcRunPhaseV1::Prepare, GcRunPhaseV1::Inventory]);
  assert_eq!(sink.statuses().last().unwrap().state, GcRunStateV1::Cancelled);
}

#[test]
fn incomplete_and_invalid_progress_fail_conservatively() {
  let cancellation = CancellationToken::new();
  let sink = Arc::new(RecordingSink::default());
  let context = run_context(GcRunModeV1::NonDestructiveMark, cancellation.clone(), sink.clone());
  let mut operation = RecordingOperation::new(cancellation);
  operation.incomplete_at = Some(GcRunPhaseV1::Mark);
  let terminal = execute_gc_run_v1(&context, &mut operation).unwrap();
  assert_eq!(terminal.state, GcRunStateV1::Incomplete);
  assert_eq!(terminal.code.as_deref(), Some("test_incomplete"));
  assert_eq!(operation.phases, vec![GcRunPhaseV1::Prepare, GcRunPhaseV1::Inventory, GcRunPhaseV1::Mark]);

  let cancellation = CancellationToken::new();
  let sink = Arc::new(RecordingSink::default());
  let context = run_context(GcRunModeV1::NonDestructiveMark, cancellation.clone(), sink.clone());
  let mut operation = RecordingOperation::new(cancellation);
  operation.regress_progress_at = Some(GcRunPhaseV1::Prepare);
  let error = execute_gc_run_v1(&context, &mut operation).unwrap_err();
  assert_eq!(error.code(), "gc_run_progress_regressed");
  assert_eq!(operation.phases, vec![GcRunPhaseV1::Prepare]);
  assert_eq!(sink.statuses().last().unwrap().state, GcRunStateV1::Failed);
}

#[test]
fn destructive_activation_is_refused_before_operation_or_progress() {
  let cancellation = CancellationToken::new();
  let sink = Arc::new(RecordingSink::default());
  let context = run_context(GcRunModeV1::Destructive, cancellation.clone(), sink.clone());
  let mut operation = RecordingOperation::new(cancellation);

  let error = execute_gc_run_v1(&context, &mut operation).unwrap_err();

  assert_eq!(error.code(), "gc_run_destructive_disabled");
  assert!(operation.phases.is_empty());
  let statuses = sink.statuses();
  assert_eq!(statuses.len(), 1);
  assert_eq!(statuses[0].state, GcRunStateV1::Refused);
  assert_eq!(statuses[0].overall_progress, 0.0);
}

#[test]
fn malformed_identity_and_budgets_are_rejected_before_context_construction() {
  assert_eq!(GcRunIDV1::new([0; 16]).unwrap_err().code(), "gc_run_id");
  for values in [(0, 128, 1_024, 1), (129, 128, 1_024, 1), (1, 128, 0, 1), (1, 128, 1_024, 0)] {
    assert_eq!(GcRunBudgetsV1::new(values.0, values.1, values.2, values.3).unwrap_err().code(), "gc_run_budgets");
  }
  let error = GcRunContextV1::new(
    GcRunIDV1::new([1; 16]).unwrap(),
    GcRunInvocationV1::Cli,
    GcRunModeV1::NonDestructiveMark,
    0,
    GcRunBudgetsV1::new(1, 1, 1, 1).unwrap(),
    CancellationToken::new(),
    Arc::new(RecordingSink::default()),
  )
  .unwrap_err();
  assert_eq!(error.code(), "gc_run_started_at");
}

#[test]
fn progress_cannot_exceed_captured_memory_or_scratch_budgets() {
  struct OversizedOperation;
  impl GcRunOperationV1 for OversizedOperation {
    fn execute_phase(
      &mut self,
      _phase: GcRunPhaseV1,
      reporter: &mut GcRunPhaseReporterV1<'_>,
    ) -> Result<GcRunPhaseOutcomeV1, GcRunErrorV1> {
      reporter.report(GcRunProgressUpdateV1 { memory_reserved_bytes: 128 * 1_024 * 1_024 + 1, ..Default::default() })?;
      Ok(GcRunPhaseOutcomeV1::Continue)
    }
  }

  let cancellation = CancellationToken::new();
  let sink = Arc::new(RecordingSink::default());
  let context = run_context(GcRunModeV1::NonDestructiveMark, cancellation, sink.clone());
  let error = execute_gc_run_v1(&context, &mut OversizedOperation).unwrap_err();
  assert_eq!(error.code(), "gc_run_memory_budget");
  assert_eq!(sink.statuses().last().unwrap().state, GcRunStateV1::Failed);
}

#[test]
fn malformed_progress_and_oversized_diagnostics_remain_bounded_failures() {
  struct OneUpdate(Option<GcRunProgressUpdateV1>);
  impl GcRunOperationV1 for OneUpdate {
    fn execute_phase(
      &mut self,
      _phase: GcRunPhaseV1,
      reporter: &mut GcRunPhaseReporterV1<'_>,
    ) -> Result<GcRunPhaseOutcomeV1, GcRunErrorV1> {
      reporter.report(self.0.take().unwrap())?;
      Ok(GcRunPhaseOutcomeV1::Continue)
    }
  }

  fn assert_update_error(update: GcRunProgressUpdateV1, expected_code: &str) {
    let cancellation = CancellationToken::new();
    let sink = Arc::new(RecordingSink::default());
    let context = run_context(GcRunModeV1::NonDestructiveMark, cancellation, sink.clone());
    let error = execute_gc_run_v1(&context, &mut OneUpdate(Some(update))).unwrap_err();
    assert_eq!(error.code(), expected_code);
    assert_eq!(sink.statuses().last().unwrap().state, GcRunStateV1::Failed);
  }

  assert_update_error(GcRunProgressUpdateV1 { phase_progress: f64::NAN, ..Default::default() }, "gc_run_progress");
  assert_update_error(GcRunProgressUpdateV1 { phase_progress: 1.01, ..Default::default() }, "gc_run_progress");
  assert_update_error(GcRunProgressUpdateV1 { completed_units: 2, total_units: Some(1), ..Default::default() }, "gc_run_progress_units");
  assert_update_error(
    GcRunProgressUpdateV1 { scratch_used_bytes: 2 * 1_024 * 1_024 * 1_024 + 1, ..Default::default() },
    "gc_run_scratch_budget",
  );
  assert_update_error(GcRunProgressUpdateV1 { message: Some("x".repeat(4 * 1_024 + 1)), ..Default::default() }, "gc_run_progress_message");

  struct OversizedFailure;
  impl GcRunOperationV1 for OversizedFailure {
    fn execute_phase(
      &mut self,
      _phase: GcRunPhaseV1,
      _reporter: &mut GcRunPhaseReporterV1<'_>,
    ) -> Result<GcRunPhaseOutcomeV1, GcRunErrorV1> {
      Err(GcRunErrorV1::operation("test_oversized_failure", "z".repeat(8 * 1_024)))
    }
  }
  let sink = Arc::new(RecordingSink::default());
  let context = run_context(GcRunModeV1::NonDestructiveMark, CancellationToken::new(), sink.clone());
  let error = execute_gc_run_v1(&context, &mut OversizedFailure).unwrap_err();
  assert_eq!(error.to_string().len(), 8 * 1_024);
  let terminal = sink.statuses().pop().unwrap();
  assert_eq!(terminal.state, GcRunStateV1::Failed);
  assert_eq!(terminal.message.as_ref().unwrap().len(), 4 * 1_024);
  assert!(terminal.message.unwrap().ends_with("..."));
}

#[test]
fn prepare_requires_one_valid_frozen_basis_and_contexts_are_single_use() {
  struct MissingBasis;
  impl GcRunOperationV1 for MissingBasis {
    fn execute_phase(
      &mut self,
      _phase: GcRunPhaseV1,
      _reporter: &mut GcRunPhaseReporterV1<'_>,
    ) -> Result<GcRunPhaseOutcomeV1, GcRunErrorV1> {
      Ok(GcRunPhaseOutcomeV1::Continue)
    }
  }

  let sink = Arc::new(RecordingSink::default());
  let context = run_context(GcRunModeV1::NonDestructiveMark, CancellationToken::new(), sink.clone());
  let error = execute_gc_run_v1(&context, &mut MissingBasis).unwrap_err();
  assert_eq!(error.code(), "gc_run_basis_missing");
  assert_eq!(sink.statuses().last().unwrap().state, GcRunStateV1::Failed);

  let cancellation = CancellationToken::new();
  let sink = Arc::new(RecordingSink::default());
  let context = run_context(GcRunModeV1::NonDestructiveMark, cancellation.clone(), sink);
  let mut operation = RecordingOperation::new(cancellation);
  execute_gc_run_v1(&context, &mut operation).unwrap();
  let error = execute_gc_run_v1(&context, &mut operation).unwrap_err();
  assert_eq!(error.code(), "gc_run_context_reused");

  let mut invalid_basis = test_basis();
  invalid_basis.kv_layout_fingerprint.pop();
  struct InvalidBasis(Option<GcRunBasisV1>);
  impl GcRunOperationV1 for InvalidBasis {
    fn execute_phase(
      &mut self,
      _phase: GcRunPhaseV1,
      reporter: &mut GcRunPhaseReporterV1<'_>,
    ) -> Result<GcRunPhaseOutcomeV1, GcRunErrorV1> {
      reporter.capture_basis(self.0.take().unwrap())?;
      Ok(GcRunPhaseOutcomeV1::Continue)
    }
  }
  let sink = Arc::new(RecordingSink::default());
  let context = run_context(GcRunModeV1::NonDestructiveMark, CancellationToken::new(), sink);
  let error = execute_gc_run_v1(&context, &mut InvalidBasis(Some(invalid_basis))).unwrap_err();
  assert_eq!(error.code(), "gc_run_basis_hash_width");
}

#[test]
fn duplicate_or_late_basis_and_malformed_incomplete_outcomes_fail_closed() {
  struct DuplicateBasis;
  impl GcRunOperationV1 for DuplicateBasis {
    fn execute_phase(
      &mut self,
      _phase: GcRunPhaseV1,
      reporter: &mut GcRunPhaseReporterV1<'_>,
    ) -> Result<GcRunPhaseOutcomeV1, GcRunErrorV1> {
      reporter.capture_basis(test_basis())?;
      reporter.capture_basis(test_basis())?;
      Ok(GcRunPhaseOutcomeV1::Continue)
    }
  }
  let context = run_context(GcRunModeV1::NonDestructiveMark, CancellationToken::new(), Arc::new(RecordingSink::default()));
  assert_eq!(execute_gc_run_v1(&context, &mut DuplicateBasis).unwrap_err().code(), "gc_run_basis_duplicate");

  struct LateBasis;
  impl GcRunOperationV1 for LateBasis {
    fn execute_phase(&mut self, phase: GcRunPhaseV1, reporter: &mut GcRunPhaseReporterV1<'_>) -> Result<GcRunPhaseOutcomeV1, GcRunErrorV1> {
      if phase == GcRunPhaseV1::Prepare {
        reporter.capture_basis(test_basis())?;
      } else if phase == GcRunPhaseV1::Inventory {
        reporter.capture_basis(test_basis())?;
      }
      Ok(GcRunPhaseOutcomeV1::Continue)
    }
  }
  let context = run_context(GcRunModeV1::NonDestructiveMark, CancellationToken::new(), Arc::new(RecordingSink::default()));
  assert_eq!(execute_gc_run_v1(&context, &mut LateBasis).unwrap_err().code(), "gc_run_basis_phase");

  struct InvalidIncomplete;
  impl GcRunOperationV1 for InvalidIncomplete {
    fn execute_phase(
      &mut self,
      _phase: GcRunPhaseV1,
      _reporter: &mut GcRunPhaseReporterV1<'_>,
    ) -> Result<GcRunPhaseOutcomeV1, GcRunErrorV1> {
      Ok(GcRunPhaseOutcomeV1::Incomplete { code: "", message: String::new() })
    }
  }
  let sink = Arc::new(RecordingSink::default());
  let context = run_context(GcRunModeV1::NonDestructiveMark, CancellationToken::new(), sink.clone());
  let error = execute_gc_run_v1(&context, &mut InvalidIncomplete).unwrap_err();
  assert_eq!(error.code(), "gc_run_incomplete_contract");
  assert_eq!(sink.statuses().last().unwrap().state, GcRunStateV1::Failed);
}

#[test]
fn status_serialization_uses_uuid_run_ids_and_the_runtime_stays_architecture_isolated() {
  let cancellation = CancellationToken::new();
  let sink = Arc::new(RecordingSink::default());
  let context = run_context(GcRunModeV1::NonDestructiveMark, cancellation.clone(), sink);
  let mut operation = RecordingOperation::new(cancellation);
  let terminal = execute_gc_run_v1(&context, &mut operation).unwrap();
  let serialized = serde_json::to_value(&terminal).unwrap();
  assert_eq!(serialized["run_id"], "41414141-4141-4141-4141-414141414141");
  assert_eq!(serialized["started_at_ms"], 10_000);
  assert_eq!(serialized["state"], "complete");

  let source = include_str!("../../src/engine/v4/gc_run.rs");
  for forbidden in ["StorageEngine", "VoidManager", "run_gc(", "gc_sweep", "crate::server", "task_worker", "publish_mark_run_checkpoint"] {
    assert!(!source.contains(forbidden), "P4-8a runtime must remain disconnected from {forbidden}");
  }
}
