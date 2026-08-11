use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::gc_mark::{
  MarkMutationJournalSegmentWriteV1, MarkMutationOperationV1, MarkMutationRecordV1, MarkMutationRecordWriteV1,
  encode_mark_mutation_journal_segment, mark_workspace_mutation_records_v1,
};
use aeordb::engine::v4::gc_mark_convergence::{
  MarkMutationApplierV1, MarkMutationBoundaryV1, MarkMutationCatchUpV1, MarkMutationConvergenceBasisV1, MarkMutationConvergenceErrorV1,
  MarkMutationConvergenceOptionsV1, MarkMutationCursorV1, MarkMutationDrainSourceV1, MarkMutationEncodedRunV1,
  MarkMutationFinalGuardAuthorityV1, MarkMutationFinalGuardOperationV1, MarkMutationFinalGuardSessionV1,
  MarkMutationFinalPublicationReceiptV1, MarkMutationFinalPublicationRequestV1, MarkMutationReconcilerV1, MarkMutationRestartReasonV1,
  MarkMutationRunVisitorV1,
};
use tokio_util::sync::CancellationToken;

fn workspace_fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-mark-workspace-object-v1")
}

fn repeated_hash(algorithm: HashAlgorithm, value: u8) -> Vec<u8> {
  vec![value; algorithm.hash_length()]
}

fn physical_incarnation(algorithm: HashAlgorithm, value: u8, write_sequence: u64) -> Vec<u8> {
  let width = algorithm.hash_length();
  let mut bytes = Vec::with_capacity(24 + 2 * width);
  bytes.extend_from_slice(&repeated_hash(algorithm, value));
  bytes.extend_from_slice(&repeated_hash(algorithm, value.wrapping_add(1)));
  bytes.extend_from_slice(&(4096 + write_sequence * 512).to_le_bytes());
  bytes.extend_from_slice(&write_sequence.to_le_bytes());
  bytes.extend_from_slice(&256u32.to_le_bytes());
  bytes.push(2);
  bytes.push(1);
  bytes.extend_from_slice(&0u16.to_le_bytes());
  bytes
}

fn memory_coordinator() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(64 * 1024 * 1024, 96 * 1024 * 1024, 1, 8 * 1024 * 1024).unwrap())
}

#[derive(Default)]
struct RecordingApplier {
  sequences: Vec<u64>,
}

impl MarkMutationApplierV1 for RecordingApplier {
  fn apply(&mut self, record: &MarkMutationRecordV1<'_>) -> Result<(), aeordb::engine::v4::gc_mark_convergence::MarkMutationApplyErrorV1> {
    self.sequences.push(record.publication_sequence);
    Ok(())
  }
}

#[derive(Debug)]
struct InjectedApplyFailure;

impl std::fmt::Display for InjectedApplyFailure {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str("injected mark mutation apply failure")
  }
}

impl std::error::Error for InjectedApplyFailure {}

struct FailingApplier;

impl MarkMutationApplierV1 for FailingApplier {
  fn apply(&mut self, _record: &MarkMutationRecordV1<'_>) -> Result<(), aeordb::engine::v4::gc_mark_convergence::MarkMutationApplyErrorV1> {
    Err(aeordb::engine::v4::gc_mark_convergence::MarkMutationApplyErrorV1::new("injected_mark_apply", InjectedApplyFailure))
  }
}

fn journal_run(
  algorithm: HashAlgorithm,
  database_id: &[u8; 16],
  run_id: &[u8; 16],
  segment_ordinal: u64,
  previous_segment_hash: Option<&[u8]>,
  publication_sequence: u64,
  mutation_value: u8,
  root_before: &[u8],
  root_after: &[u8],
) -> (Vec<u8>, Vec<u8>) {
  let mutation_id = repeated_hash(algorithm, mutation_value);
  let logical_key = repeated_hash(algorithm, mutation_value.wrapping_add(1));
  let incarnation = physical_incarnation(algorithm, mutation_value.wrapping_add(2), publication_sequence);
  let record = MarkMutationRecordWriteV1 {
    publication_sequence,
    mutation_id: &mutation_id,
    root_before,
    root_after,
    published_logical_key: &logical_key,
    new_incarnation: &incarnation,
    operation: MarkMutationOperationV1::Replace,
  };
  let encoded = encode_mark_mutation_journal_segment(&MarkMutationJournalSegmentWriteV1 {
    hash_algorithm: algorithm,
    database_id,
    run_id,
    generation: 77,
    segment_ordinal,
    previous_segment_hash,
    records: &[record],
  })
  .unwrap();
  (encoded.value, encoded.key)
}

fn reconciler<'a>(
  algorithm: HashAlgorithm,
  cancellation: &'a CancellationToken,
  memory: &MemoryCoordinator,
  root: &'a [u8],
  layout: &'a [u8],
  through: u64,
  maximum_rounds: u32,
) -> MarkMutationReconcilerV1<'a> {
  MarkMutationReconcilerV1::new(
    MarkMutationConvergenceBasisV1 {
      algorithm,
      database_id: [0x31; 16],
      run_id: [0x51; 16],
      generation: 77,
      checkpoint_sequence: 7,
      kv_layout_generation: 8,
      kv_layout_fingerprint: layout,
      reconciled_root_hash: root,
      reconciled_through_publication_sequence: through,
      mutation_journal_head: None,
      mutation_journal_segment_ordinal: 0,
      options: MarkMutationConvergenceOptionsV1::new(1024, maximum_rounds).unwrap(),
      cancellation,
    },
    memory,
  )
  .unwrap()
}

fn boundary(layout: &[u8], root: &[u8], publication_sequence: u64, mutation_value: Option<u8>) -> MarkMutationBoundaryV1 {
  MarkMutationBoundaryV1 {
    kv_layout_generation: 8,
    kv_layout_fingerprint: layout.to_vec(),
    authority_root_hash: root.to_vec(),
    publication_sequence,
    mutation_id: mutation_value.map_or_else(Vec::new, |value| vec![value; layout.len()]),
  }
}

struct ScriptedMutationSource {
  boundaries: VecDeque<MarkMutationBoundaryV1>,
  drains: VecDeque<Vec<Vec<u8>>>,
  publication_count: u32,
  guard_active: bool,
  dishonest_receipt: bool,
}

impl MarkMutationDrainSourceV1 for ScriptedMutationSource {
  fn capture_boundary(&mut self, _after: &MarkMutationCursorV1) -> Result<MarkMutationBoundaryV1, MarkMutationConvergenceErrorV1> {
    self.boundaries.pop_front().ok_or(MarkMutationConvergenceErrorV1::InvalidOptions("scripted boundary exhausted"))
  }

  fn visit_runs(
    &mut self,
    _after: &MarkMutationCursorV1,
    _through_publication_sequence: u64,
    visitor: &mut dyn MarkMutationRunVisitorV1,
  ) -> Result<(), MarkMutationConvergenceErrorV1> {
    let runs = self.drains.pop_front().ok_or(MarkMutationConvergenceErrorV1::InvalidOptions("scripted drain exhausted"))?;
    for run in &runs {
      visitor.visit_run(MarkMutationEncodedRunV1::JournalSegment(run))?;
    }
    Ok(())
  }
}

impl MarkMutationFinalGuardSessionV1 for ScriptedMutationSource {
  fn publish_final(
    &mut self,
    request: &MarkMutationFinalPublicationRequestV1,
  ) -> Result<MarkMutationFinalPublicationReceiptV1, MarkMutationConvergenceErrorV1> {
    assert!(self.guard_active, "final publication escaped the exclusive guard");
    self.publication_count += 1;
    Ok(MarkMutationFinalPublicationReceiptV1 {
      hash_algorithm: request.hash_algorithm,
      database_id: request.database_id,
      run_id: request.run_id,
      generation: request.generation,
      checkpoint_sequence: request.checkpoint_sequence,
      kv_layout_generation: request.kv_layout_generation,
      kv_layout_fingerprint: request.kv_layout_fingerprint.clone(),
      authority_root_hash: if self.dishonest_receipt {
        vec![0xa5; request.authority_root_hash.len()]
      } else {
        request.authority_root_hash.clone()
      },
      reconciled_through_publication_sequence: request.reconciled_through_publication_sequence,
      reconciled_through_mutation_id: request.reconciled_through_mutation_id.clone(),
      mutation_journal_head: request.mutation_journal_head.clone(),
      applied_records: request.applied_records,
      hard_publication_sequence: request.reconciled_through_publication_sequence + 100,
    })
  }
}

struct ScriptedFinalGuard {
  source: ScriptedMutationSource,
}

impl MarkMutationFinalGuardAuthorityV1 for ScriptedFinalGuard {
  fn execute_exclusively(
    &mut self,
    operation: &mut dyn MarkMutationFinalGuardOperationV1,
  ) -> Result<MarkMutationFinalPublicationReceiptV1, MarkMutationConvergenceErrorV1> {
    assert!(!self.source.guard_active);
    self.source.guard_active = true;
    let result = operation.execute(&mut self.source);
    self.source.guard_active = false;
    result
  }
}

struct FailingAfterOperationGuard {
  source: ScriptedMutationSource,
}

impl MarkMutationFinalGuardAuthorityV1 for FailingAfterOperationGuard {
  fn execute_exclusively(
    &mut self,
    operation: &mut dyn MarkMutationFinalGuardOperationV1,
  ) -> Result<MarkMutationFinalPublicationReceiptV1, MarkMutationConvergenceErrorV1> {
    self.source.guard_active = true;
    let result = operation.execute(&mut self.source);
    self.source.guard_active = false;
    result?;
    Err(MarkMutationConvergenceErrorV1::InvalidOptions("injected post-operation authority failure"))
  }
}

#[test]
fn one_reconciler_streams_workspace_and_journal_runs_through_an_exact_root_chain() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let fixture_name = match algorithm {
      HashAlgorithm::Blake3_256 => "agwo-blake3-256-mutation-valid.bin",
      HashAlgorithm::Sha512 => "agwo-sha512-mutation-valid.bin",
      _ => unreachable!(),
    };
    let workspace_bytes = fs::read(workspace_fixture_root().join(fixture_name)).unwrap();
    let workspace_record = mark_workspace_mutation_records_v1(&workspace_bytes, algorithm).unwrap().next().unwrap().unwrap();
    let database_id: [u8; 16] = workspace_bytes[16..32].try_into().unwrap();
    let run_id: [u8; 16] = workspace_bytes[32..48].try_into().unwrap();
    let layout_fingerprint = repeated_hash(algorithm, 0x91);
    let final_root = repeated_hash(algorithm, 0xe3);
    let mutation_id = repeated_hash(algorithm, 0xd1);
    let logical_key = repeated_hash(algorithm, 0xd2);
    let incarnation = physical_incarnation(algorithm, 0xd3, 803);
    let journal_record = MarkMutationRecordWriteV1 {
      publication_sequence: 803,
      mutation_id: &mutation_id,
      root_before: workspace_record.root_after,
      root_after: &final_root,
      published_logical_key: &logical_key,
      new_incarnation: &incarnation,
      operation: MarkMutationOperationV1::Replace,
    };
    let journal = encode_mark_mutation_journal_segment(&MarkMutationJournalSegmentWriteV1 {
      hash_algorithm: algorithm,
      database_id: &database_id,
      run_id: &run_id,
      generation: 77,
      segment_ordinal: 1,
      previous_segment_hash: None,
      records: &[journal_record],
    })
    .unwrap();
    let cancellation = CancellationToken::new();
    let memory = memory_coordinator();
    let options = MarkMutationConvergenceOptionsV1::new(1024, 4).unwrap();
    let mut reconciler = MarkMutationReconcilerV1::new(
      MarkMutationConvergenceBasisV1 {
        algorithm,
        database_id,
        run_id,
        generation: 77,
        checkpoint_sequence: 7,
        kv_layout_generation: 8,
        kv_layout_fingerprint: &layout_fingerprint,
        reconciled_root_hash: workspace_record.root_before,
        reconciled_through_publication_sequence: 801,
        mutation_journal_head: None,
        mutation_journal_segment_ordinal: 0,
        options,
        cancellation: &cancellation,
      },
      &memory,
    )
    .unwrap();
    let mut applier = RecordingApplier::default();

    reconciler.reconcile_run(MarkMutationEncodedRunV1::WorkspaceObject(&workspace_bytes), &mut applier).unwrap();
    reconciler.reconcile_run(MarkMutationEncodedRunV1::JournalSegment(&journal.value), &mut applier).unwrap();

    let status = reconciler.status();
    assert_eq!(status.reconciled_through_publication_sequence, 803);
    assert_eq!(status.current_root_hash, final_root);
    assert_eq!(status.mutation_journal_head, journal.key);
    assert_eq!(status.mutation_journal_segment_ordinal, 1);
    assert_eq!(status.applied_records, 2);
    assert_eq!(applier.sequences, [802, 803]);
  }
}

#[test]
fn catch_up_reaches_a_moving_boundary_in_bounded_rounds() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let database_id = [0x31; 16];
    let run_id = [0x51; 16];
    let layout = repeated_hash(algorithm, 0x71);
    let initial_root = repeated_hash(algorithm, 0x81);
    let middle_root = repeated_hash(algorithm, 0x82);
    let final_root = repeated_hash(algorithm, 0x83);
    let (first, first_key) = journal_run(algorithm, &database_id, &run_id, 1, None, 11, 0x21, &initial_root, &middle_root);
    let (second, _) = journal_run(algorithm, &database_id, &run_id, 2, Some(&first_key), 12, 0x31, &middle_root, &final_root);
    let mut source = ScriptedMutationSource {
      boundaries: VecDeque::from([
        boundary(&layout, &middle_root, 11, Some(0x21)),
        boundary(&layout, &final_root, 12, Some(0x31)),
        boundary(&layout, &final_root, 12, Some(0x31)),
        boundary(&layout, &final_root, 12, Some(0x31)),
      ]),
      drains: VecDeque::from([vec![first], vec![second]]),
      publication_count: 0,
      guard_active: false,
      dishonest_receipt: false,
    };
    let cancellation = CancellationToken::new();
    let memory = memory_coordinator();
    let mut reconciler = reconciler(algorithm, &cancellation, &memory, &initial_root, &layout, 10, 4);
    let mut applier = RecordingApplier::default();

    let caught_up: MarkMutationCatchUpV1 = reconciler.catch_up(&mut source, &mut applier).unwrap();

    assert_eq!(caught_up.rounds, 2);
    assert_eq!(caught_up.boundary.authority_root_hash, final_root);
    assert_eq!(caught_up.boundary.publication_sequence, 12);
    assert_eq!(applier.sequences, [11, 12]);
    assert_eq!(reconciler.status().restart_required, None);
  }
}

#[test]
fn catch_up_accepts_a_boundary_advancing_within_one_publication() {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x31; 16];
  let run_id = [0x51; 16];
  let layout = repeated_hash(algorithm, 0x71);
  let initial_root = repeated_hash(algorithm, 0x81);
  let final_root = repeated_hash(algorithm, 0x82);
  let (first, first_key) = journal_run(algorithm, &database_id, &run_id, 1, None, 11, 0x21, &initial_root, &final_root);
  let (second, _) = journal_run(algorithm, &database_id, &run_id, 2, Some(&first_key), 11, 0x31, &initial_root, &final_root);
  let first_boundary = boundary(&layout, &final_root, 11, Some(0x21));
  let final_boundary = boundary(&layout, &final_root, 11, Some(0x31));
  let mut source = ScriptedMutationSource {
    boundaries: VecDeque::from([first_boundary, final_boundary.clone(), final_boundary.clone(), final_boundary]),
    drains: VecDeque::from([vec![first], vec![second]]),
    publication_count: 0,
    guard_active: false,
    dishonest_receipt: false,
  };
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let mut reconciler = reconciler(algorithm, &cancellation, &memory, &initial_root, &layout, 10, 4);
  let mut applier = RecordingApplier::default();

  let caught_up = reconciler.catch_up(&mut source, &mut applier).unwrap();

  assert_eq!(caught_up.rounds, 2);
  assert_eq!(caught_up.boundary.mutation_id, repeated_hash(algorithm, 0x31));
  assert_eq!(applier.sequences, [11, 11]);
  assert_eq!(reconciler.status().restart_required, None);
}

#[test]
fn exact_boundary_rejects_an_omitted_same_publication_tail() {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x31; 16];
  let run_id = [0x51; 16];
  let layout = repeated_hash(algorithm, 0x71);
  let initial_root = repeated_hash(algorithm, 0x81);
  let final_root = repeated_hash(algorithm, 0x82);
  let (first, first_key) = journal_run(algorithm, &database_id, &run_id, 1, None, 11, 0x21, &initial_root, &final_root);
  let (second, _) = journal_run(algorithm, &database_id, &run_id, 2, Some(&first_key), 11, 0x31, &initial_root, &final_root);
  let target = boundary(&layout, &final_root, 11, Some(0x31));
  let cancellation = CancellationToken::new();

  let memory = memory_coordinator();
  let mut incomplete = reconciler(algorithm, &cancellation, &memory, &initial_root, &layout, 10, 2);
  let mut source = ScriptedMutationSource {
    boundaries: VecDeque::from([target.clone()]),
    drains: VecDeque::from([vec![first.clone()]]),
    publication_count: 0,
    guard_active: false,
    dishonest_receipt: false,
  };
  let error = incomplete.catch_up(&mut source, &mut RecordingApplier::default()).unwrap_err();
  assert!(matches!(error, MarkMutationConvergenceErrorV1::RestartRequired(MarkMutationRestartReasonV1::JournalGap)));
  assert_eq!(incomplete.status().last_mutation_id, repeated_hash(algorithm, 0x21));

  let memory = memory_coordinator();
  let mut complete = reconciler(algorithm, &cancellation, &memory, &initial_root, &layout, 10, 2);
  let mut source = ScriptedMutationSource {
    boundaries: VecDeque::from([target.clone(), target]),
    drains: VecDeque::from([vec![first, second]]),
    publication_count: 0,
    guard_active: false,
    dishonest_receipt: false,
  };
  complete.catch_up(&mut source, &mut RecordingApplier::default()).unwrap();
  assert_eq!(complete.status().last_mutation_id, repeated_hash(algorithm, 0x31));
}

#[test]
fn catch_up_starvation_and_apply_failure_latch_restart_without_advancing_false_state() {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x31; 16];
  let run_id = [0x51; 16];
  let layout = repeated_hash(algorithm, 0x71);
  let initial_root = repeated_hash(algorithm, 0x81);
  let middle_root = repeated_hash(algorithm, 0x82);
  let final_root = repeated_hash(algorithm, 0x83);
  let later_root = repeated_hash(algorithm, 0x84);
  let (first, first_key) = journal_run(algorithm, &database_id, &run_id, 1, None, 11, 0x21, &initial_root, &middle_root);
  let (second, _) = journal_run(algorithm, &database_id, &run_id, 2, Some(&first_key), 12, 0x31, &middle_root, &final_root);
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let mut starving = reconciler(algorithm, &cancellation, &memory, &initial_root, &layout, 10, 2);
  let mut source = ScriptedMutationSource {
    boundaries: VecDeque::from([
      boundary(&layout, &middle_root, 11, Some(0x21)),
      boundary(&layout, &final_root, 12, Some(0x31)),
      boundary(&layout, &final_root, 12, Some(0x31)),
      boundary(&layout, &later_root, 13, Some(0x41)),
    ]),
    drains: VecDeque::from([vec![first.clone()], vec![second]]),
    publication_count: 0,
    guard_active: false,
    dishonest_receipt: false,
  };
  let error = starving.catch_up(&mut source, &mut RecordingApplier::default()).unwrap_err();
  assert!(matches!(error, MarkMutationConvergenceErrorV1::RestartRequired(MarkMutationRestartReasonV1::Starvation)));
  assert_eq!(starving.status().restart_required, Some(MarkMutationRestartReasonV1::Starvation));

  let memory = memory_coordinator();
  let mut failing = reconciler(algorithm, &cancellation, &memory, &initial_root, &layout, 10, 2);
  let mut source = ScriptedMutationSource {
    boundaries: VecDeque::from([boundary(&layout, &middle_root, 11, Some(0x21))]),
    drains: VecDeque::from([vec![first]]),
    publication_count: 0,
    guard_active: false,
    dishonest_receipt: false,
  };
  let error = failing.catch_up(&mut source, &mut FailingApplier).unwrap_err();
  assert!(matches!(error, MarkMutationConvergenceErrorV1::Apply(_)));
  let status = failing.status();
  assert_eq!(status.reconciled_through_publication_sequence, 10);
  assert_eq!(status.current_root_hash, initial_root);
  assert_eq!(status.applied_records, 0);
  assert_eq!(status.restart_required, Some(MarkMutationRestartReasonV1::ApplyFailure));
}

#[test]
fn gap_malformed_source_cancellation_and_memory_pressure_all_require_restart() {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x31; 16];
  let run_id = [0x51; 16];
  let layout = repeated_hash(algorithm, 0x71);
  let initial_root = repeated_hash(algorithm, 0x81);
  let later_root = repeated_hash(algorithm, 0x83);
  let (gap, _) = journal_run(algorithm, &database_id, &run_id, 1, None, 12, 0x31, &initial_root, &later_root);
  let cancellation = CancellationToken::new();

  let memory = memory_coordinator();
  let mut gap_reconciler = reconciler(algorithm, &cancellation, &memory, &initial_root, &layout, 10, 2);
  let error = gap_reconciler.reconcile_run(MarkMutationEncodedRunV1::JournalSegment(&gap), &mut RecordingApplier::default()).unwrap_err();
  assert!(matches!(error, MarkMutationConvergenceErrorV1::RestartRequired(MarkMutationRestartReasonV1::JournalGap)));

  let memory = memory_coordinator();
  let mut malformed = reconciler(algorithm, &cancellation, &memory, &initial_root, &layout, 10, 2);
  let error =
    malformed.reconcile_run(MarkMutationEncodedRunV1::JournalSegment(b"not a journal"), &mut RecordingApplier::default()).unwrap_err();
  assert!(matches!(error, MarkMutationConvergenceErrorV1::Format(_)));
  assert_eq!(malformed.status().restart_required, Some(MarkMutationRestartReasonV1::MalformedRun));

  let memory = memory_coordinator();
  let mut exhausted = reconciler(algorithm, &cancellation, &memory, &initial_root, &layout, 10, 2);
  let mut source = ScriptedMutationSource {
    boundaries: VecDeque::new(),
    drains: VecDeque::new(),
    publication_count: 0,
    guard_active: false,
    dishonest_receipt: false,
  };
  let error = exhausted.catch_up(&mut source, &mut RecordingApplier::default()).unwrap_err();
  assert!(matches!(error, MarkMutationConvergenceErrorV1::RestartRequired(MarkMutationRestartReasonV1::SourceFailure)));

  let canceled = CancellationToken::new();
  let memory = memory_coordinator();
  let mut canceled_reconciler = reconciler(algorithm, &canceled, &memory, &initial_root, &layout, 10, 2);
  canceled.cancel();
  let error = canceled_reconciler.catch_up(&mut source, &mut RecordingApplier::default()).unwrap_err();
  assert!(matches!(error, MarkMutationConvergenceErrorV1::Canceled));
  assert_eq!(canceled_reconciler.status().restart_required, Some(MarkMutationRestartReasonV1::Canceled));

  let memory = memory_coordinator();
  let mut pressured = reconciler(algorithm, &cancellation, &memory, &initial_root, &layout, 10, 2);
  let _pressure = memory
    .reserve(
      aeordb::engine::memory_coordinator::MemoryOwner::Query,
      64 * 1024 * 1024,
      aeordb::engine::memory_coordinator::AdmissionClass::Workload,
    )
    .unwrap();
  let error = pressured.catch_up(&mut source, &mut RecordingApplier::default()).unwrap_err();
  assert!(matches!(error, MarkMutationConvergenceErrorV1::Memory(_)));
  assert_eq!(pressured.status().restart_required, Some(MarkMutationRestartReasonV1::MemoryPressure));
}

#[test]
fn final_publication_occurs_once_inside_the_guard_after_an_empty_second_drain() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let database_id = [0x31; 16];
    let run_id = [0x51; 16];
    let layout = repeated_hash(algorithm, 0x71);
    let initial_root = repeated_hash(algorithm, 0x81);
    let final_root = repeated_hash(algorithm, 0x82);
    let (journal, journal_key) = journal_run(algorithm, &database_id, &run_id, 1, None, 11, 0x21, &initial_root, &final_root);
    let cancellation = CancellationToken::new();
    let memory = memory_coordinator();
    let mut reconciler = reconciler(algorithm, &cancellation, &memory, &initial_root, &layout, 10, 2);
    let stable = boundary(&layout, &final_root, 11, Some(0x21));
    let mut authority = ScriptedFinalGuard {
      source: ScriptedMutationSource {
        boundaries: VecDeque::from([stable.clone(), stable.clone(), stable]),
        drains: VecDeque::from([vec![journal], Vec::new()]),
        publication_count: 0,
        guard_active: false,
        dishonest_receipt: false,
      },
    };

    let receipt = reconciler.finalize_guarded(&mut authority, &mut RecordingApplier::default()).unwrap();

    assert_eq!(authority.source.publication_count, 1);
    assert!(!authority.source.guard_active);
    assert_eq!(receipt.authority_root_hash, final_root);
    assert_eq!(receipt.reconciled_through_publication_sequence, 11);
    assert_eq!(receipt.mutation_journal_head, journal_key);
    assert_eq!(reconciler.status().final_publication_sequence, Some(receipt.hard_publication_sequence));
  }
}

#[test]
fn final_guard_refuses_layout_drift_nonempty_second_drain_and_dishonest_receipts() {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x31; 16];
  let run_id = [0x51; 16];
  let layout = repeated_hash(algorithm, 0x71);
  let wrong_layout = repeated_hash(algorithm, 0x72);
  let initial_root = repeated_hash(algorithm, 0x81);
  let middle_root = repeated_hash(algorithm, 0x82);
  let final_root = repeated_hash(algorithm, 0x83);
  let cancellation = CancellationToken::new();

  let memory = memory_coordinator();
  let mut drifted = reconciler(algorithm, &cancellation, &memory, &initial_root, &layout, 10, 2);
  let mut authority = ScriptedFinalGuard {
    source: ScriptedMutationSource {
      boundaries: VecDeque::from([boundary(&wrong_layout, &initial_root, 10, None)]),
      drains: VecDeque::new(),
      publication_count: 0,
      guard_active: false,
      dishonest_receipt: false,
    },
  };
  let error = drifted.finalize_guarded(&mut authority, &mut RecordingApplier::default()).unwrap_err();
  assert!(matches!(error, MarkMutationConvergenceErrorV1::RestartRequired(MarkMutationRestartReasonV1::LayoutChanged)));
  assert_eq!(authority.source.publication_count, 0);

  let (first, first_key) = journal_run(algorithm, &database_id, &run_id, 1, None, 11, 0x21, &initial_root, &middle_root);
  let (second, _) = journal_run(algorithm, &database_id, &run_id, 2, Some(&first_key), 12, 0x31, &middle_root, &final_root);
  let memory = memory_coordinator();
  let mut raced = reconciler(algorithm, &cancellation, &memory, &initial_root, &layout, 10, 2);
  let first_boundary = boundary(&layout, &middle_root, 11, Some(0x21));
  let mut authority = ScriptedFinalGuard {
    source: ScriptedMutationSource {
      boundaries: VecDeque::from([first_boundary.clone(), first_boundary]),
      drains: VecDeque::from([vec![first], vec![second]]),
      publication_count: 0,
      guard_active: false,
      dishonest_receipt: false,
    },
  };
  let error = raced.finalize_guarded(&mut authority, &mut RecordingApplier::default()).unwrap_err();
  assert!(matches!(error, MarkMutationConvergenceErrorV1::RestartRequired(MarkMutationRestartReasonV1::FinalDrainNotEmpty)));
  assert_eq!(authority.source.publication_count, 0);

  let memory = memory_coordinator();
  let mut dishonest = reconciler(algorithm, &cancellation, &memory, &initial_root, &layout, 10, 2);
  let stable = boundary(&layout, &initial_root, 10, None);
  let mut authority = ScriptedFinalGuard {
    source: ScriptedMutationSource {
      boundaries: VecDeque::from([stable.clone(), stable.clone(), stable]),
      drains: VecDeque::from([Vec::new(), Vec::new()]),
      publication_count: 0,
      guard_active: false,
      dishonest_receipt: true,
    },
  };
  let error = dishonest.finalize_guarded(&mut authority, &mut RecordingApplier::default()).unwrap_err();
  assert!(matches!(error, MarkMutationConvergenceErrorV1::RestartRequired(MarkMutationRestartReasonV1::PublicationFailure)));
  assert_eq!(authority.source.publication_count, 1);
  assert_eq!(dishonest.status().final_publication_sequence, None);

  let memory = memory_coordinator();
  let mut uncertain = reconciler(algorithm, &cancellation, &memory, &initial_root, &layout, 10, 2);
  let stable = boundary(&layout, &initial_root, 10, None);
  let mut authority = FailingAfterOperationGuard {
    source: ScriptedMutationSource {
      boundaries: VecDeque::from([stable.clone(), stable.clone(), stable]),
      drains: VecDeque::from([Vec::new(), Vec::new()]),
      publication_count: 0,
      guard_active: false,
      dishonest_receipt: false,
    },
  };
  let error = uncertain.finalize_guarded(&mut authority, &mut RecordingApplier::default()).unwrap_err();
  assert!(matches!(error, MarkMutationConvergenceErrorV1::RestartRequired(MarkMutationRestartReasonV1::PublicationFailure)));
  assert_eq!(authority.source.publication_count, 1);
  assert_eq!(uncertain.status().final_publication_sequence, None);
}
