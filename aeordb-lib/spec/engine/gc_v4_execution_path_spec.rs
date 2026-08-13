use std::sync::{Arc, Mutex};

use aeordb::engine::gc::{execute_gc_run, GcExecutionRequestV1};
use aeordb::engine::v4::gc_run::{GcRunInvocationV1, GcRunPhaseV1, GcRunProgressSinkV1, GcRunStateV1, GcRunStatusV1};
use aeordb::engine::{DirectoryOps, EngineError, RequestContext, StorageEngine};
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

struct PhaseCancellingSink {
  target: GcRunPhaseV1,
  cancellation: CancellationToken,
  statuses: Mutex<Vec<GcRunStatusV1>>,
}

impl PhaseCancellingSink {
  fn new(target: GcRunPhaseV1, cancellation: CancellationToken) -> Self {
    Self { target, cancellation, statuses: Mutex::new(Vec::new()) }
  }

  fn statuses(&self) -> Vec<GcRunStatusV1> {
    self.statuses.lock().unwrap().clone()
  }
}

impl GcRunProgressSinkV1 for PhaseCancellingSink {
  fn publish(&self, status: &GcRunStatusV1) {
    self.statuses.lock().unwrap().push(status.clone());
    if status.state == GcRunStateV1::Running && status.phase == Some(self.target) && status.phase_progress == 0.0 {
      self.cancellation.cancel();
    }
  }
}

fn create_engine(name: &str) -> (StorageEngine, tempfile::TempDir) {
  let temporary = tempfile::tempdir().unwrap();
  let database = temporary.path().join(format!("{name}.aeordb"));
  (StorageEngine::create(database.to_str().unwrap()).unwrap(), temporary)
}

#[test]
fn legacy_v3_dry_run_uses_the_shared_phase_executor_and_exact_invocation() {
  let (engine, _temporary) = create_engine("shared-gc-dry-run");
  let sink = Arc::new(RecordingSink::default());
  let request = GcExecutionRequestV1::new(GcRunInvocationV1::Embedded, true, CancellationToken::new()).with_progress_observer(sink.clone());

  let execution = execute_gc_run(&engine, &RequestContext::system(), request).unwrap();

  assert!(execution.result.dry_run);
  assert_eq!(execution.status.state, GcRunStateV1::Complete);
  assert_eq!(execution.status.invocation, GcRunInvocationV1::Embedded);
  assert_eq!(execution.status.phase, Some(GcRunPhaseV1::Finalize));
  assert_eq!(execution.status.overall_progress, 1.0);
  let statuses = sink.statuses();
  let entered: Vec<GcRunPhaseV1> = statuses
    .iter()
    .filter(|status| status.state == GcRunStateV1::Running && status.phase_progress == 0.0)
    .filter_map(|status| status.phase)
    .collect();
  assert_eq!(entered, GcRunPhaseV1::NON_DESTRUCTIVE);
  assert!(statuses.windows(2).all(|window| window[0].overall_progress <= window[1].overall_progress));
}

#[test]
fn sealed_legacy_v3_compatibility_preserves_reclamation_without_opening_the_v4_gate() {
  let (engine, _temporary) = create_engine("shared-gc-destructive");
  DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), "/live.txt", b"live", Some("text/plain")).unwrap();
  let sink = Arc::new(RecordingSink::default());
  let request = GcExecutionRequestV1::new(GcRunInvocationV1::Http, false, CancellationToken::new()).with_progress_observer(sink.clone());

  let execution = execute_gc_run(&engine, &RequestContext::system(), request).unwrap();

  assert!(!execution.result.dry_run);
  assert_eq!(execution.status.state, GcRunStateV1::Complete);
  assert_eq!(execution.status.invocation, GcRunInvocationV1::Http);
  assert!(sink.statuses().iter().all(|status| status.state != GcRunStateV1::Refused));

  let run_source = include_str!("../../src/engine/v4/gc_run.rs");
  assert!(run_source.contains("LegacyV3Compatibility"));
  assert!(run_source.contains("gc_run_destructive_disabled"));
  assert!(!run_source.contains("StorageEngine"));
}

#[test]
fn shared_cancellation_returns_the_existing_engine_error_before_gc_side_effects() {
  let (engine, _temporary) = create_engine("shared-gc-cancelled");
  let cancellation = CancellationToken::new();
  cancellation.cancel();
  let sink = Arc::new(RecordingSink::default());
  let request = GcExecutionRequestV1::new(GcRunInvocationV1::Task, false, cancellation).with_progress_observer(sink.clone());
  let head_before = engine.head_hash().unwrap();

  let error = execute_gc_run(&engine, &RequestContext::system(), request).unwrap_err();

  assert!(matches!(error, EngineError::Cancelled(operation) if operation == "garbage collection"));
  assert_eq!(sink.statuses().last().unwrap().state, GcRunStateV1::Cancelled);
  assert_eq!(engine.head_hash().unwrap(), head_before);
}

#[test]
fn shared_cancellation_stops_every_legacy_phase_and_releases_recheck_state() {
  for target in GcRunPhaseV1::NON_DESTRUCTIVE {
    let (engine, _temporary) = create_engine(&format!("shared-gc-cancel-{}", target.name()));
    DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), "/live.txt", b"live", Some("text/plain")).unwrap();
    let cancellation = CancellationToken::new();
    let sink = Arc::new(PhaseCancellingSink::new(target, cancellation.clone()));
    let request = GcExecutionRequestV1::new(GcRunInvocationV1::Scheduled, false, cancellation).with_progress_observer(sink.clone());

    let error = execute_gc_run(&engine, &RequestContext::system(), request).unwrap_err();

    assert!(matches!(error, EngineError::Cancelled(operation) if operation == "garbage collection"));
    let statuses = sink.statuses();
    let terminal = statuses.last().unwrap();
    assert_eq!(terminal.state, GcRunStateV1::Cancelled);
    assert_eq!(terminal.phase, Some(target));
    let target_index = GcRunPhaseV1::NON_DESTRUCTIVE.iter().position(|phase| *phase == target).unwrap();
    assert!(statuses.iter().all(|status| {
      status
        .phase
        .and_then(|phase| GcRunPhaseV1::NON_DESTRUCTIVE.iter().position(|candidate| *candidate == phase))
        .is_none_or(|phase_index| phase_index <= target_index)
    }));

    engine.begin_gc_recheck().expect("cancelled run must release legacy recheck ownership");
    engine.end_gc_recheck().unwrap();
    let follow_up = GcExecutionRequestV1::new(GcRunInvocationV1::Embedded, true, CancellationToken::new())
      .with_progress_observer(Arc::new(RecordingSink::default()));
    assert_eq!(execute_gc_run(&engine, &RequestContext::system(), follow_up).unwrap().status.state, GcRunStateV1::Complete);
  }
}
