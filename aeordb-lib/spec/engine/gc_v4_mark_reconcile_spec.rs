use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Duration;

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
  MarkMutationFinalPublicationReceiptV1, MarkMutationFinalPublicationRequestV1, MarkMutationJournalBufferOptionsV1,
  MarkMutationJournalChainStartV1, MarkMutationJournalDurabilityReceiptV1, MarkMutationJournalDurableSinkV1,
  MarkMutationJournalOwnerErrorV1, MarkMutationJournalOwnerV1, MarkMutationJournalSinkErrorV1, MarkMutationObservationV1,
  MarkMutationReconcilerV1, MarkMutationRestartReasonV1, MarkMutationRunVisitorV1, PreparedMarkMutationJournalSegmentV1,
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

struct CancelingApplier<'a> {
  cancellation: &'a CancellationToken,
  sequences: Vec<u64>,
}

impl MarkMutationApplierV1 for CancelingApplier<'_> {
  fn apply(&mut self, record: &MarkMutationRecordV1<'_>) -> Result<(), aeordb::engine::v4::gc_mark_convergence::MarkMutationApplyErrorV1> {
    self.sequences.push(record.publication_sequence);
    self.cancellation.cancel();
    Ok(())
  }
}

#[derive(Debug)]
struct InjectedSinkFailure;

impl std::fmt::Display for InjectedSinkFailure {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str("injected soft mutation sink failure")
  }
}

impl std::error::Error for InjectedSinkFailure {}

#[derive(Default)]
struct RetryMutationSink {
  fail_next: bool,
  attempts: Vec<Vec<u8>>,
  durable_segments: Vec<Vec<u8>>,
  next_hard_publication_sequence: u64,
}

impl MarkMutationJournalDurableSinkV1 for RetryMutationSink {
  fn publish_mark_mutation_segment_synced(
    &mut self,
    segment: &PreparedMarkMutationJournalSegmentV1<'_>,
  ) -> Result<MarkMutationJournalDurabilityReceiptV1, MarkMutationJournalSinkErrorV1> {
    self.attempts.push(segment.value.to_vec());
    if self.fail_next {
      self.fail_next = false;
      return Err(MarkMutationJournalSinkErrorV1::new("injected_mark_sink", InjectedSinkFailure));
    }
    self.durable_segments.push(segment.value.to_vec());
    self.next_hard_publication_sequence = self.next_hard_publication_sequence.max(segment.last_publication_sequence) + 1;
    Ok(MarkMutationJournalDurabilityReceiptV1 {
      artifact_key: segment.artifact_key.to_vec(),
      stored_value_length: segment.value.len() as u32,
      hard_publication_sequence: self.next_hard_publication_sequence,
    })
  }
}

fn journal_run(
  algorithm: HashAlgorithm,
  database_id: &[u8; 16],
  run_id: &[u8; 16],
  segment_ordinal: u64,
  previous_segment_hash: Option<&[u8]>,
  mutation: (u64, u8, &[u8], &[u8]),
) -> (Vec<u8>, Vec<u8>) {
  let (publication_sequence, mutation_value, root_before, root_after) = mutation;
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

struct BypassFinalGuard {
  receipt: MarkMutationFinalPublicationReceiptV1,
}

impl MarkMutationFinalGuardAuthorityV1 for BypassFinalGuard {
  fn execute_exclusively(
    &mut self,
    _operation: &mut dyn MarkMutationFinalGuardOperationV1,
  ) -> Result<MarkMutationFinalPublicationReceiptV1, MarkMutationConvergenceErrorV1> {
    Ok(self.receipt.clone())
  }
}

struct DoubleExecutionFinalGuard {
  source: ScriptedMutationSource,
}

impl MarkMutationFinalGuardAuthorityV1 for DoubleExecutionFinalGuard {
  fn execute_exclusively(
    &mut self,
    operation: &mut dyn MarkMutationFinalGuardOperationV1,
  ) -> Result<MarkMutationFinalPublicationReceiptV1, MarkMutationConvergenceErrorV1> {
    self.source.guard_active = true;
    let first = operation.execute(&mut self.source);
    if first.is_err() {
      self.source.guard_active = false;
      return first;
    }
    let second = operation.execute(&mut self.source);
    self.source.guard_active = false;
    second
  }
}

struct CancelBeforeExecutionFinalGuard<'a> {
  cancellation: &'a CancellationToken,
  source: ScriptedMutationSource,
}

impl MarkMutationFinalGuardAuthorityV1 for CancelBeforeExecutionFinalGuard<'_> {
  fn execute_exclusively(
    &mut self,
    operation: &mut dyn MarkMutationFinalGuardOperationV1,
  ) -> Result<MarkMutationFinalPublicationReceiptV1, MarkMutationConvergenceErrorV1> {
    self.source.guard_active = true;
    self.cancellation.cancel();
    let result = operation.execute(&mut self.source);
    self.source.guard_active = false;
    result
  }
}

struct IgnoredVisitorFailureSource {
  boundary: MarkMutationBoundaryV1,
  malformed_run: Vec<u8>,
}

impl MarkMutationDrainSourceV1 for IgnoredVisitorFailureSource {
  fn capture_boundary(&mut self, _after: &MarkMutationCursorV1) -> Result<MarkMutationBoundaryV1, MarkMutationConvergenceErrorV1> {
    Ok(self.boundary.clone())
  }

  fn visit_runs(
    &mut self,
    _after: &MarkMutationCursorV1,
    _through_publication_sequence: u64,
    visitor: &mut dyn MarkMutationRunVisitorV1,
  ) -> Result<(), MarkMutationConvergenceErrorV1> {
    let _ignored = visitor.visit_run(MarkMutationEncodedRunV1::JournalSegment(&self.malformed_run));
    Ok(())
  }
}

struct SharedFinalGate {
  exclusion: Mutex<()>,
  ready: Barrier,
  active: AtomicUsize,
  maximum_active: AtomicUsize,
}

struct ConcurrentFinalGuard {
  shared: Arc<SharedFinalGate>,
  source: ScriptedMutationSource,
}

impl MarkMutationFinalGuardAuthorityV1 for ConcurrentFinalGuard {
  fn execute_exclusively(
    &mut self,
    operation: &mut dyn MarkMutationFinalGuardOperationV1,
  ) -> Result<MarkMutationFinalPublicationReceiptV1, MarkMutationConvergenceErrorV1> {
    self.shared.ready.wait();
    let _exclusive = self.shared.exclusion.lock().unwrap();
    let active = self.shared.active.fetch_add(1, Ordering::SeqCst) + 1;
    self.shared.maximum_active.fetch_max(active, Ordering::SeqCst);
    self.source.guard_active = true;
    thread::sleep(Duration::from_millis(25));
    let result = operation.execute(&mut self.source);
    self.source.guard_active = false;
    self.shared.active.fetch_sub(1, Ordering::SeqCst);
    result
  }
}

fn stable_final_receipt(
  algorithm: HashAlgorithm,
  layout: &[u8],
  root: &[u8],
  reconciled_through_publication_sequence: u64,
) -> MarkMutationFinalPublicationReceiptV1 {
  MarkMutationFinalPublicationReceiptV1 {
    hash_algorithm: algorithm,
    database_id: [0x31; 16],
    run_id: [0x51; 16],
    generation: 77,
    checkpoint_sequence: 7,
    kv_layout_generation: 8,
    kv_layout_fingerprint: layout.to_vec(),
    authority_root_hash: root.to_vec(),
    reconciled_through_publication_sequence,
    reconciled_through_mutation_id: Vec::new(),
    mutation_journal_head: Vec::new(),
    applied_records: 0,
    hard_publication_sequence: reconciled_through_publication_sequence + 100,
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
    let (first, first_key) = journal_run(algorithm, &database_id, &run_id, 1, None, (11, 0x21, &initial_root, &middle_root));
    let (second, _) = journal_run(algorithm, &database_id, &run_id, 2, Some(&first_key), (12, 0x31, &middle_root, &final_root));
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
  let (first, first_key) = journal_run(algorithm, &database_id, &run_id, 1, None, (11, 0x21, &initial_root, &final_root));
  let (second, _) = journal_run(algorithm, &database_id, &run_id, 2, Some(&first_key), (11, 0x31, &initial_root, &final_root));
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
  let (first, first_key) = journal_run(algorithm, &database_id, &run_id, 1, None, (11, 0x21, &initial_root, &final_root));
  let (second, _) = journal_run(algorithm, &database_id, &run_id, 2, Some(&first_key), (11, 0x31, &initial_root, &final_root));
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
  let (first, first_key) = journal_run(algorithm, &database_id, &run_id, 1, None, (11, 0x21, &initial_root, &middle_root));
  let (second, _) = journal_run(algorithm, &database_id, &run_id, 2, Some(&first_key), (12, 0x31, &middle_root, &final_root));
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
  let (gap, _) = journal_run(algorithm, &database_id, &run_id, 1, None, (12, 0x31, &initial_root, &later_root));
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
fn cancellation_between_records_stops_the_run_before_applying_more_mutations() {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x31; 16];
  let run_id = [0x51; 16];
  let layout = repeated_hash(algorithm, 0x71);
  let initial_root = repeated_hash(algorithm, 0x81);
  let middle_root = repeated_hash(algorithm, 0x82);
  let final_root = repeated_hash(algorithm, 0x83);
  let first_mutation_id = repeated_hash(algorithm, 0x21);
  let first_logical_key = repeated_hash(algorithm, 0x22);
  let first_incarnation = physical_incarnation(algorithm, 0x23, 11);
  let second_mutation_id = repeated_hash(algorithm, 0x31);
  let second_logical_key = repeated_hash(algorithm, 0x32);
  let second_incarnation = physical_incarnation(algorithm, 0x33, 12);
  let records = [
    MarkMutationRecordWriteV1 {
      publication_sequence: 11,
      mutation_id: &first_mutation_id,
      root_before: &initial_root,
      root_after: &middle_root,
      published_logical_key: &first_logical_key,
      new_incarnation: &first_incarnation,
      operation: MarkMutationOperationV1::Replace,
    },
    MarkMutationRecordWriteV1 {
      publication_sequence: 12,
      mutation_id: &second_mutation_id,
      root_before: &middle_root,
      root_after: &final_root,
      published_logical_key: &second_logical_key,
      new_incarnation: &second_incarnation,
      operation: MarkMutationOperationV1::Replace,
    },
  ];
  let journal = encode_mark_mutation_journal_segment(&MarkMutationJournalSegmentWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    run_id: &run_id,
    generation: 77,
    segment_ordinal: 1,
    previous_segment_hash: None,
    records: &records,
  })
  .unwrap();
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let mut reconciler = reconciler(algorithm, &cancellation, &memory, &initial_root, &layout, 10, 2);
  let mut applier = CancelingApplier { cancellation: &cancellation, sequences: Vec::new() };

  let error = reconciler.reconcile_run(MarkMutationEncodedRunV1::JournalSegment(&journal.value), &mut applier).unwrap_err();

  assert!(matches!(error, MarkMutationConvergenceErrorV1::Canceled));
  assert_eq!(applier.sequences, [11]);
  let status = reconciler.status();
  assert_eq!(status.reconciled_through_publication_sequence, 11);
  assert_eq!(status.current_root_hash, middle_root);
  assert_eq!(status.restart_required, Some(MarkMutationRestartReasonV1::Canceled));
}

#[test]
fn final_publication_occurs_once_inside_the_guard_after_an_empty_second_drain() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let database_id = [0x31; 16];
    let run_id = [0x51; 16];
    let layout = repeated_hash(algorithm, 0x71);
    let initial_root = repeated_hash(algorithm, 0x81);
    let final_root = repeated_hash(algorithm, 0x82);
    let (journal, journal_key) = journal_run(algorithm, &database_id, &run_id, 1, None, (11, 0x21, &initial_root, &final_root));
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

  let (first, first_key) = journal_run(algorithm, &database_id, &run_id, 1, None, (11, 0x21, &initial_root, &middle_root));
  let (second, _) = journal_run(algorithm, &database_id, &run_id, 2, Some(&first_key), (12, 0x31, &middle_root, &final_root));
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

#[test]
fn failed_soft_publication_retries_exact_bytes_but_only_a_fresh_run_can_resume_them() {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x31; 16];
  let run_id = [0x51; 16];
  let layout = repeated_hash(algorithm, 0x71);
  let initial_root = repeated_hash(algorithm, 0x81);
  let final_root = repeated_hash(algorithm, 0x82);
  let mutation_id = repeated_hash(algorithm, 0x21);
  let logical_key = repeated_hash(algorithm, 0x22);
  let incarnation = physical_incarnation(algorithm, 0x23, 11);
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let options = MarkMutationJournalBufferOptionsV1::new(1, 1024, 4096, 30_000).unwrap();
  let mut owner = MarkMutationJournalOwnerV1::new_chain(
    MarkMutationJournalChainStartV1 {
      algorithm,
      database_id,
      run_id,
      generation: 77,
      captured_publication_sequence: 10,
      options,
      cancellation: &cancellation,
    },
    &memory,
  )
  .unwrap();
  assert_eq!(
    owner.observe_committed(
      MarkMutationRecordWriteV1 {
        publication_sequence: 11,
        mutation_id: &mutation_id,
        root_before: &initial_root,
        root_after: &final_root,
        published_logical_key: &logical_key,
        new_incarnation: &incarnation,
        operation: MarkMutationOperationV1::Replace,
      },
      1,
    ),
    MarkMutationObservationV1::Buffered { flush_due: true }
  );
  let mut sink = RetryMutationSink { fail_next: true, ..RetryMutationSink::default() };

  assert!(matches!(owner.flush(&mut sink), Err(MarkMutationJournalOwnerErrorV1::Sink { .. })));
  assert!(owner.status().incomplete);
  assert_eq!(owner.status().pending_records, 1);
  assert!(owner.flush(&mut sink).unwrap());
  assert_eq!(sink.attempts.len(), 2);
  assert_eq!(sink.attempts[0], sink.attempts[1]);
  assert_eq!(sink.durable_segments, [sink.attempts[0].clone()]);
  assert!(owner.status().incomplete, "an exact retry cannot restore the failed run's reclaim authority");

  let fresh_cancellation = CancellationToken::new();
  let fresh_memory = memory_coordinator();
  let mut fresh = reconciler(algorithm, &fresh_cancellation, &fresh_memory, &initial_root, &layout, 10, 2);
  let mut applier = RecordingApplier::default();
  fresh.reconcile_run(MarkMutationEncodedRunV1::JournalSegment(&sink.durable_segments[0]), &mut applier).unwrap();
  let status = fresh.status();
  assert_eq!(status.reconciled_through_publication_sequence, 11);
  assert_eq!(status.current_root_hash, final_root);
  assert_eq!(status.restart_required, None);
  assert_eq!(applier.sequences, [11]);
}

#[test]
fn malformed_predecessors_and_ignored_visitor_failures_cannot_complete_catch_up() {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x31; 16];
  let run_id = [0x51; 16];
  let layout = repeated_hash(algorithm, 0x71);
  let initial_root = repeated_hash(algorithm, 0x81);
  let final_root = repeated_hash(algorithm, 0x82);
  let false_predecessor = repeated_hash(algorithm, 0x91);
  let (malformed_chain, _) =
    journal_run(algorithm, &database_id, &run_id, 1, Some(&false_predecessor), (11, 0x21, &initial_root, &final_root));
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let mut malformed = reconciler(algorithm, &cancellation, &memory, &initial_root, &layout, 10, 2);
  let mut applier = RecordingApplier::default();

  let error = malformed.reconcile_run(MarkMutationEncodedRunV1::JournalSegment(&malformed_chain), &mut applier).unwrap_err();

  assert!(matches!(error, MarkMutationConvergenceErrorV1::RestartRequired(MarkMutationRestartReasonV1::ContextMismatch)));
  assert!(applier.sequences.is_empty());
  assert_eq!(malformed.status().reconciled_through_publication_sequence, 10);

  let memory = memory_coordinator();
  let mut ignored = reconciler(algorithm, &cancellation, &memory, &initial_root, &layout, 10, 2);
  let mut source =
    IgnoredVisitorFailureSource { boundary: boundary(&layout, &initial_root, 10, None), malformed_run: b"not a mutation journal".to_vec() };
  let error = ignored.catch_up(&mut source, &mut RecordingApplier::default()).unwrap_err();
  assert!(matches!(error, MarkMutationConvergenceErrorV1::RestartRequired(MarkMutationRestartReasonV1::MalformedRun)));
  assert_eq!(ignored.status().final_publication_sequence, None);
}

#[test]
fn final_authority_must_execute_exactly_once_and_observe_guard_entry_cancellation() {
  let algorithm = HashAlgorithm::Blake3_256;
  let layout = repeated_hash(algorithm, 0x71);
  let initial_root = repeated_hash(algorithm, 0x81);
  let cancellation = CancellationToken::new();

  let memory = memory_coordinator();
  let mut bypassed = reconciler(algorithm, &cancellation, &memory, &initial_root, &layout, 10, 2);
  let mut bypass = BypassFinalGuard { receipt: stable_final_receipt(algorithm, &layout, &initial_root, 10) };
  let error = bypassed.finalize_guarded(&mut bypass, &mut RecordingApplier::default()).unwrap_err();
  assert!(matches!(error, MarkMutationConvergenceErrorV1::RestartRequired(MarkMutationRestartReasonV1::PublicationFailure)));
  assert_eq!(bypassed.status().final_publication_sequence, None);

  let memory = memory_coordinator();
  let mut doubled = reconciler(algorithm, &cancellation, &memory, &initial_root, &layout, 10, 2);
  let stable = boundary(&layout, &initial_root, 10, None);
  let mut double = DoubleExecutionFinalGuard {
    source: ScriptedMutationSource {
      boundaries: VecDeque::from([stable.clone(), stable.clone(), stable]),
      drains: VecDeque::from([Vec::new(), Vec::new()]),
      publication_count: 0,
      guard_active: false,
      dishonest_receipt: false,
    },
  };
  let error = doubled.finalize_guarded(&mut double, &mut RecordingApplier::default()).unwrap_err();
  assert!(matches!(error, MarkMutationConvergenceErrorV1::RestartRequired(MarkMutationRestartReasonV1::PublicationFailure)));
  assert_eq!(double.source.publication_count, 1);
  assert_eq!(doubled.status().final_publication_sequence, None);

  let canceled = CancellationToken::new();
  let memory = memory_coordinator();
  let mut canceled_reconciler = reconciler(algorithm, &canceled, &memory, &initial_root, &layout, 10, 2);
  let mut canceling_guard = CancelBeforeExecutionFinalGuard {
    cancellation: &canceled,
    source: ScriptedMutationSource {
      boundaries: VecDeque::from([boundary(&layout, &initial_root, 10, None)]),
      drains: VecDeque::new(),
      publication_count: 0,
      guard_active: false,
      dishonest_receipt: false,
    },
  };
  let error = canceled_reconciler.finalize_guarded(&mut canceling_guard, &mut RecordingApplier::default()).unwrap_err();
  assert!(matches!(error, MarkMutationConvergenceErrorV1::Canceled));
  assert_eq!(canceling_guard.source.publication_count, 0);
  assert_eq!(canceled_reconciler.status().restart_required, Some(MarkMutationRestartReasonV1::Canceled));
}

#[test]
fn concurrent_finalizers_share_one_real_exclusion_boundary() {
  let algorithm = HashAlgorithm::Blake3_256;
  let layout = repeated_hash(algorithm, 0x71);
  let initial_root = repeated_hash(algorithm, 0x81);
  let shared = Arc::new(SharedFinalGate {
    exclusion: Mutex::new(()),
    ready: Barrier::new(2),
    active: AtomicUsize::new(0),
    maximum_active: AtomicUsize::new(0),
  });
  let first_cancellation = CancellationToken::new();
  let second_cancellation = CancellationToken::new();
  let first_memory = memory_coordinator();
  let second_memory = memory_coordinator();
  let mut first = reconciler(algorithm, &first_cancellation, &first_memory, &initial_root, &layout, 10, 2);
  let mut second = reconciler(algorithm, &second_cancellation, &second_memory, &initial_root, &layout, 10, 2);
  let source = || {
    let stable = boundary(&layout, &initial_root, 10, None);
    ScriptedMutationSource {
      boundaries: VecDeque::from([stable.clone(), stable.clone(), stable]),
      drains: VecDeque::from([Vec::new(), Vec::new()]),
      publication_count: 0,
      guard_active: false,
      dishonest_receipt: false,
    }
  };
  let mut first_guard = ConcurrentFinalGuard { shared: shared.clone(), source: source() };
  let mut second_guard = ConcurrentFinalGuard { shared: shared.clone(), source: source() };

  let (first_result, second_result) = thread::scope(|scope| {
    let first_thread = scope.spawn(|| first.finalize_guarded(&mut first_guard, &mut RecordingApplier::default()));
    let second_thread = scope.spawn(|| second.finalize_guarded(&mut second_guard, &mut RecordingApplier::default()));
    (first_thread.join().unwrap(), second_thread.join().unwrap())
  });

  assert!(first_result.is_ok());
  assert!(second_result.is_ok());
  assert_eq!(shared.maximum_active.load(Ordering::SeqCst), 1);
  assert_eq!(shared.active.load(Ordering::SeqCst), 0);
  assert_eq!(first_guard.source.publication_count, 1);
  assert_eq!(second_guard.source.publication_count, 1);
}

#[test]
fn mark_convergence_symbols_remain_confined_to_disconnected_v4_modules() {
  fn collect_rust_sources(directory: &Path, output: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).unwrap() {
      let entry = entry.unwrap();
      let metadata = fs::symlink_metadata(entry.path()).unwrap();
      if metadata.file_type().is_symlink() {
        continue;
      }
      if metadata.is_dir() {
        collect_rust_sources(&entry.path(), output);
      } else if entry.path().extension().is_some_and(|extension| extension == "rs") {
        output.push(entry.path());
      }
    }
  }

  let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let mut sources = Vec::new();
  collect_rust_sources(&source_root, &mut sources);
  let confinement = [
    ("MarkMutationReconcilerV1", &["engine/v4/gc_mark_convergence.rs"][..]),
    ("MarkMutationFinalGuardAuthorityV1", &["engine/v4/gc_mark_convergence.rs"][..]),
    ("MarkMutationDrainSourceV1", &["engine/v4/gc_mark_convergence.rs"][..]),
    ("MarkMutationJournalOwnerV1", &["engine/v4/gc_mark_convergence.rs"][..]),
    ("mark_workspace_mutation_records_v1", &["engine/v4/gc_mark.rs", "engine/v4/gc_mark_convergence.rs"][..]),
    ("MarkMutationJournalDurableSinkV1", &["engine/v4/gc_mark_convergence.rs", "engine/v4/first_authority.rs"][..]),
  ];

  for path in sources {
    let relative = path.strip_prefix(&source_root).unwrap().to_string_lossy().replace('\\', "/");
    let source = fs::read_to_string(&path).unwrap();
    for (symbol, allowed) in confinement {
      if source.contains(symbol) {
        assert!(allowed.contains(&relative.as_str()), "{symbol} escaped disconnected v4 ownership into {relative}");
      }
    }
  }
}
