use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::index_runtime_workspace_rotation::{
  IndexRuntimeImmutableCoverageProofV1, IndexRuntimeWorkspaceRotationDispositionV1, IndexRuntimeWorkspaceRotationEntryV1,
  IndexRuntimeWorkspaceRotationErrorV1, IndexRuntimeWorkspaceRotationPlannerV1,
};

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;
const RUNTIME_ID: [u8; 16] = [0x44; 16];
const SOURCE_ROOT: [u8; 32] = [0x55; 32];
const COVERAGE_EPOCH: [u8; 16] = [0x66; 16];

fn proof(covered_through: u64) -> IndexRuntimeImmutableCoverageProofV1<'static> {
  IndexRuntimeImmutableCoverageProofV1 {
    runtime_id: RUNTIME_ID,
    generation: 7,
    source_namespace_root: &SOURCE_ROOT,
    coverage_epoch_id: COVERAGE_EPOCH,
    covered_through_publication_sequence: covered_through,
  }
}

fn runtime(sequence: u64, minimum: u64, maximum: u64) -> IndexRuntimeWorkspaceRotationEntryV1 {
  IndexRuntimeWorkspaceRotationEntryV1::runtime_batch(sequence, object_id(sequence), minimum, maximum)
}

fn task(sequence: u64, operation_id: [u8; 16], publication_sequence: u64) -> IndexRuntimeWorkspaceRotationEntryV1 {
  IndexRuntimeWorkspaceRotationEntryV1::producer_task(sequence, object_id(sequence), operation_id, publication_sequence)
}

fn planner<'a>(
  selected_object_count: u64,
  pending_operation_ids: &'a [[u8; 16]],
  covered_through: u64,
  is_cancelled: &'a dyn Fn() -> bool,
) -> IndexRuntimeWorkspaceRotationPlannerV1<'a> {
  IndexRuntimeWorkspaceRotationPlannerV1::new(
    ALGORITHM,
    RUNTIME_ID,
    7,
    &SOURCE_ROOT,
    selected_object_count,
    proof(covered_through),
    pending_operation_ids,
    is_cancelled,
  )
  .unwrap()
}

#[test]
fn exact_frontier_discards_only_represented_history_and_retains_exact_pending_work() {
  let pending = [[0x81; 16], [0x82; 16]];
  let never_cancelled = || false;
  let mut planner = planner(6, &pending, 10, &never_cancelled);

  assert_eq!(planner.observe(runtime(1, 2, 4)).unwrap(), IndexRuntimeWorkspaceRotationDispositionV1::DiscardRepresented);
  assert_eq!(planner.observe(task(2, [0x71; 16], 5)).unwrap(), IndexRuntimeWorkspaceRotationDispositionV1::DiscardRepresented);
  assert_eq!(planner.observe(runtime(3, 9, 12)).unwrap(), IndexRuntimeWorkspaceRotationDispositionV1::RetainUnresolvedBatch);
  assert_eq!(planner.observe(runtime(4, 11, 13)).unwrap(), IndexRuntimeWorkspaceRotationDispositionV1::RetainUnresolvedBatch);
  assert_eq!(planner.observe(task(5, [0x81; 16], 14)).unwrap(), IndexRuntimeWorkspaceRotationDispositionV1::RetainPendingTask);
  assert_eq!(planner.observe(task(6, [0x82; 16], 15)).unwrap(), IndexRuntimeWorkspaceRotationDispositionV1::RetainPendingTask);

  let summary = planner.finish().unwrap();
  assert_eq!(summary.observed_objects, 6);
  assert_eq!(summary.discarded_objects, 2);
  assert_eq!(summary.retained_runtime_batches, 2);
  assert_eq!(summary.retained_pending_tasks, 2);
  assert_eq!(summary.retained_objects(), 4);
}

#[test]
fn replay_bound_depends_on_pending_work_not_historical_completion_count() {
  let pending = [[0xf1; 16], [0xf2; 16]];
  let never_cancelled = || false;
  let historical = 10_000u64;
  let mut planner = planner(historical + 2, &pending, historical, &never_cancelled);
  for sequence in 1..=historical {
    let operation_id = operation_id(sequence);
    assert_eq!(
      planner.observe(task(sequence, operation_id, sequence)).unwrap(),
      IndexRuntimeWorkspaceRotationDispositionV1::DiscardRepresented
    );
  }
  assert_eq!(
    planner.observe(task(historical + 1, pending[0], historical + 1)).unwrap(),
    IndexRuntimeWorkspaceRotationDispositionV1::RetainPendingTask
  );
  assert_eq!(
    planner.observe(task(historical + 2, pending[1], historical + 2)).unwrap(),
    IndexRuntimeWorkspaceRotationDispositionV1::RetainPendingTask
  );
  let summary = planner.finish().unwrap();
  assert_eq!(summary.discarded_objects, historical);
  assert_eq!(summary.retained_objects(), 2);
}

#[test]
fn pending_and_completed_task_state_must_close_against_the_coverage_frontier() {
  let never_cancelled = || false;
  let pending = [[0x81; 16]];

  let mut covered_pending = planner(1, &pending, 10, &never_cancelled);
  assert!(matches!(
    covered_pending.observe(task(1, pending[0], 10)),
    Err(IndexRuntimeWorkspaceRotationErrorV1::PendingTaskAlreadyCovered { .. })
  ));

  let mut unproven_completion = planner(1, &[], 10, &never_cancelled);
  assert!(matches!(
    unproven_completion.observe(task(1, [0x71; 16], 11)),
    Err(IndexRuntimeWorkspaceRotationErrorV1::UnprovenCompletedTask { .. })
  ));

  let mut missing_pending = planner(1, &pending, 10, &never_cancelled);
  assert_eq!(missing_pending.observe(runtime(1, 11, 12)).unwrap(), IndexRuntimeWorkspaceRotationDispositionV1::RetainUnresolvedBatch);
  assert!(matches!(missing_pending.finish(), Err(IndexRuntimeWorkspaceRotationErrorV1::PendingTaskMissing { .. })));
}

#[test]
fn constructor_rejects_foreign_or_noncanonical_authority_before_observation() {
  let never_cancelled = || false;
  let pending = [[0x82; 16], [0x81; 16]];
  assert!(matches!(
    IndexRuntimeWorkspaceRotationPlannerV1::new(ALGORITHM, RUNTIME_ID, 7, &SOURCE_ROOT, 1, proof(10), &pending, &never_cancelled,),
    Err(IndexRuntimeWorkspaceRotationErrorV1::Invalid(_))
  ));

  let mut foreign = proof(10);
  foreign.runtime_id = [0x99; 16];
  assert!(matches!(
    IndexRuntimeWorkspaceRotationPlannerV1::new(ALGORITHM, RUNTIME_ID, 7, &SOURCE_ROOT, 1, foreign, &[], &never_cancelled,),
    Err(IndexRuntimeWorkspaceRotationErrorV1::ForeignCoverage)
  ));
}

#[test]
fn malformed_or_out_of_order_inventory_fails_closed() {
  let never_cancelled = || false;
  let mut out_of_order = planner(2, &[], 10, &never_cancelled);
  assert!(matches!(
    out_of_order.observe(runtime(2, 1, 2)),
    Err(IndexRuntimeWorkspaceRotationErrorV1::InventorySequence { expected: 1, observed: 2 })
  ));

  let mut invalid_interval = planner(1, &[], 10, &never_cancelled);
  assert!(matches!(invalid_interval.observe(runtime(1, 0, 2)), Err(IndexRuntimeWorkspaceRotationErrorV1::Invalid(_))));

  let incomplete = planner(1, &[], 10, &never_cancelled);
  assert!(matches!(incomplete.finish(), Err(IndexRuntimeWorkspaceRotationErrorV1::InventoryIncomplete { .. })));
}

#[test]
fn cancellation_is_checked_before_and_during_streaming_inventory() {
  let initially_cancelled = || true;
  assert!(matches!(
    IndexRuntimeWorkspaceRotationPlannerV1::new(ALGORITHM, RUNTIME_ID, 7, &SOURCE_ROOT, 1, proof(10), &[], &initially_cancelled,),
    Err(IndexRuntimeWorkspaceRotationErrorV1::Canceled)
  ));

  let calls = std::cell::Cell::new(0u8);
  let cancel_after_construction = || {
    let next = calls.get().saturating_add(1);
    calls.set(next);
    next > 1
  };
  let mut planner = planner(1, &[], 10, &cancel_after_construction);
  assert!(matches!(planner.observe(runtime(1, 1, 2)), Err(IndexRuntimeWorkspaceRotationErrorV1::Canceled)));
}

#[test]
fn duplicate_pending_identity_and_cancellation_at_finish_fail_closed() {
  let never_cancelled = || false;
  let pending = [[0x81; 16]];
  let mut duplicate = planner(2, &pending, 10, &never_cancelled);
  assert_eq!(duplicate.observe(task(1, pending[0], 11)).unwrap(), IndexRuntimeWorkspaceRotationDispositionV1::RetainPendingTask);
  assert!(matches!(duplicate.observe(task(2, pending[0], 12)), Err(IndexRuntimeWorkspaceRotationErrorV1::DuplicatePendingTask { .. })));

  let calls = std::cell::Cell::new(0u8);
  let cancel_at_finish = || {
    let next = calls.get().saturating_add(1);
    calls.set(next);
    next > 2
  };
  let mut canceled = planner(1, &[], 10, &cancel_at_finish);
  assert_eq!(canceled.observe(runtime(1, 1, 2)).unwrap(), IndexRuntimeWorkspaceRotationDispositionV1::DiscardRepresented);
  assert!(matches!(canceled.finish(), Err(IndexRuntimeWorkspaceRotationErrorV1::Canceled)));
}

#[test]
fn widest_hash_profile_accepts_exact_authority_and_rejects_root_drift() {
  const WIDE_ROOT: [u8; 64] = [0x75; 64];
  let never_cancelled = || false;
  let coverage = IndexRuntimeImmutableCoverageProofV1 {
    runtime_id: RUNTIME_ID,
    generation: 7,
    source_namespace_root: &WIDE_ROOT,
    coverage_epoch_id: COVERAGE_EPOCH,
    covered_through_publication_sequence: 10,
  };
  let mut planner =
    IndexRuntimeWorkspaceRotationPlannerV1::new(HashAlgorithm::Sha512, RUNTIME_ID, 7, &WIDE_ROOT, 1, coverage, &[], &never_cancelled)
      .unwrap();
  assert_eq!(planner.observe(runtime(1, 1, 10)).unwrap(), IndexRuntimeWorkspaceRotationDispositionV1::DiscardRepresented);
  assert_eq!(planner.finish().unwrap().retained_objects(), 0);

  let wrong_root = [0x76; 64];
  assert!(matches!(
    IndexRuntimeWorkspaceRotationPlannerV1::new(HashAlgorithm::Sha512, RUNTIME_ID, 7, &wrong_root, 1, coverage, &[], &never_cancelled,),
    Err(IndexRuntimeWorkspaceRotationErrorV1::ForeignCoverage)
  ));
}

fn operation_id(sequence: u64) -> [u8; 16] {
  let mut operation_id = [0u8; 16];
  operation_id[..8].copy_from_slice(&sequence.to_le_bytes());
  operation_id[8..].copy_from_slice(&sequence.rotate_left(17).to_le_bytes());
  operation_id
}

fn object_id(sequence: u64) -> [u8; 16] {
  let mut object_id = operation_id(sequence);
  object_id[15] ^= 0xa5;
  object_id
}
