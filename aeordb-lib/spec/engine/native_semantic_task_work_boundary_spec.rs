//! Failing-first retries, stale work, invalid inputs and counter exhaustion.
#[path = "native_semantic_task_advance_spec.rs"]
mod advance;
#[path = "native_semantic_task_work_compiler_boundary_spec.rs"]
mod compiler_boundary;
#[path = "native_semantic_task_work_guard_spec.rs"]
mod guarded;
#[path = "native_semantic_task_work_modes_spec.rs"]
mod modes;
use super::*;

struct TaskWorkFixture<'a> {
  publisher: &'a V4FirstAuthorityPublisher,
  memory: &'a MemoryCoordinator,
  cancellation: &'a CancellationToken,
  path: &'a Path,
  tree: &'a [u8],
  retirement: &'a mut RetirementJournalOwnerV1,
}

fn with_initial_task_for_work(test: impl FnOnce(TaskWorkFixture<'_>)) -> (tempfile::TempDir, PathBuf) {
  with_initial_task_for_work_algorithm(HashAlgorithm::Blake3_256, test)
}

fn with_initial_task_for_work_algorithm(algorithm: HashAlgorithm, test: impl FnOnce(TaskWorkFixture<'_>)) -> (tempfile::TempDir, PathBuf) {
  let (directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("task-work-boundaries", None, [1; 16], algorithm, 0);
  let initial = request_for_database_and_algorithm([1; 16], algorithm);
  let root = publisher.publish(&initial).unwrap().namespace_root.root_hash;
  enable_node_staging(&publisher);
  seed_union_generation(&publisher);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 1, 16 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let mut retirement = selection_retirement(algorithm, &memory, &cancellation);
  {
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let staged = capture
      .prepare_and_stage_semantic_source_union(
        NativeSemanticSourceUnionRequestV1 {
          expected_base_root: &root,
          requested_directory_root: &initial.namespace_tree.root_hash,
          replacements: &[],
          workspace_parent: workspace.path(),
          bounds: union_bounds(&initial.namespace_tree.root_hash),
        },
        staging_request(),
      )
      .unwrap();
    let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
    staged.stage_initial_checkpoint(checkpoint).unwrap();
    staged.select_initial_task(selection_request(checkpoint), &mut retirement).unwrap();
  }
  test(TaskWorkFixture {
    publisher: &publisher,
    memory: &memory,
    cancellation: &cancellation,
    path: &path,
    tree: &initial.namespace_tree.root_hash,
    retirement: &mut retirement,
  });
  drop(retirement);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  (directory, path)
}

fn observe_work_task(fixture: &TaskWorkFixture<'_>) -> SemanticMutationObservationV1 {
  fixture
    .publisher
    .observe_semantic_mutation_task(SemanticMutationObservationRequestV1 {
      database_id: &[1; 16],
      task_id: &[2; 16],
      memory: fixture.memory,
      cancellation: fixture.cancellation,
    })
    .unwrap()
}

#[test]
fn native_task_work_exact_retry_is_read_only_and_a_new_reservation_excludes_old_work() {
  with_initial_task_for_work(|fixture| {
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    let observed = observe_work_task(&fixture);
    let input = work_request(observed.header().selected.header.updated_at_ms + 1);
    let old_work = protection
      .begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement)
      .expect("a real initial task must reserve fenced work");
    let before_retry = fs::read(fixture.path).unwrap();
    let retry = protection
      .begin_semantic_task_work(
        &observed,
        NativeSemanticTaskWorkRequestV1 { publication_timestamp_ms: input.publication_timestamp_ms + 1, ..input },
        fixture.memory,
        fixture.cancellation,
        fixture.retirement,
      )
      .unwrap();
    assert!(retry.receipt().idempotent);
    assert_eq!(retry.receipt().control_digest, old_work.receipt().control_digest);
    assert_eq!(retry.reserved_checkpoint_sequence(), 2);
    assert_eq!(fs::read(fixture.path).unwrap(), before_retry);
    drop(retry);
    let current = observe_work_task(&fixture);
    let next_input = work_request(current.header().selected.header.updated_at_ms + 1);
    let next = protection.begin_semantic_task_work(&current, next_input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
    assert_eq!(next.reserved_checkpoint_sequence(), 3);
    assert_eq!(next.receipt().control_sequence, 3);
    let after_new_reservation = fs::read(fixture.path).unwrap();
    let error =
      old_work.start_compilation(start_request(fixture.tree, next_input.publication_timestamp_ms + 10), fixture.retirement).unwrap_err();
    assert!(error.committed_receipt().is_none());
    assert_eq!(fs::read(fixture.path).unwrap(), after_new_reservation);
    let selected =
      next.start_compilation(start_request(fixture.tree, next_input.publication_timestamp_ms + 20), fixture.retirement).unwrap();
    assert_eq!(selected.control_sequence, 4);
    let finished = observe_work_task(&fixture);
    assert_eq!(finished.task().unwrap().unwrap().checkpoint_sequence, 3);
    assert_eq!(finished.task().unwrap().unwrap().fencing_token, 3);
    assert_eq!(finished.checkpoint().unwrap().unwrap().phase, SemanticMutationPhaseV1::Compiling);
  });
}

#[test]
fn native_task_work_compiler_duplicate_exact_acquisition_cannot_select_twice() {
  with_initial_task_for_work(|fixture| {
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    let observed = observe_work_task(&fixture);
    let input = work_request(observed.header().selected.header.updated_at_ms + 1);
    let first = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
    let duplicate =
      protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
    assert_eq!(first.reserved_checkpoint_sequence(), duplicate.reserved_checkpoint_sequence());
    assert!(duplicate.receipt().idempotent);
    let request = start_request(fixture.tree, input.publication_timestamp_ms + 20);
    let receipt = first.start_compilation(request, fixture.retirement).unwrap();
    let before = fs::read(fixture.path).unwrap();
    let error = duplicate.start_compilation(request, fixture.retirement).unwrap_err();
    assert_eq!(error.code(), "semantic_task_work_conflict");
    assert!(error.committed_receipt().is_none());
    assert_eq!(fs::read(fixture.path).unwrap(), before);
    assert_eq!(observe_work_task(&fixture).task().unwrap().unwrap().control_sequence, receipt.control_sequence);
  });
}

#[test]
fn native_task_work_invalid_requests_refuse_without_writes_or_retained_reservations() {
  with_initial_task_for_work(|fixture| {
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    let observed = observe_work_task(&fixture);
    let valid = work_request(observed.header().selected.header.updated_at_ms + 1);
    let before = fs::read(fixture.path).unwrap();
    let baseline = fixture.memory.snapshot().unwrap().reserved_bytes;
    for invalid in [
      NativeSemanticTaskWorkRequestV1 { holder_boot_id: [0; 16], ..valid },
      NativeSemanticTaskWorkRequestV1 { acquired_at_ms: -1, ..valid },
      NativeSemanticTaskWorkRequestV1 { acquired_at_ms: observed.task().unwrap().unwrap().updated_at_ms - 1, ..valid },
      NativeSemanticTaskWorkRequestV1 { publication_timestamp_ms: 0, ..valid },
      NativeSemanticTaskWorkRequestV1 { publication_timestamp_ms: i64::MAX as u64 + 1, ..valid },
      NativeSemanticTaskWorkRequestV1 { publication_timestamp_ms: valid.acquired_at_ms as u64 - 1, ..valid },
      NativeSemanticTaskWorkRequestV1 { monotonic_now_ms: 0, ..valid },
      NativeSemanticTaskWorkRequestV1 { maximum_workspace_bytes: 1, ..valid },
      NativeSemanticTaskWorkRequestV1 {
        inventory_bounds: NativeSemanticMutationInventoryBoundsV1 { maximum_read_bytes: 1, ..valid.inventory_bounds },
        ..valid
      },
      NativeSemanticTaskWorkRequestV1 {
        inventory_bounds: NativeSemanticMutationInventoryBoundsV1 { maximum_work: 0, ..valid.inventory_bounds },
        ..valid
      },
      NativeSemanticTaskWorkRequestV1 { graph_bounds: NativeSemanticTaskGraphBoundsV1 { maximum_work: 0, ..valid.graph_bounds }, ..valid },
      NativeSemanticTaskWorkRequestV1 {
        graph_bounds: NativeSemanticTaskGraphBoundsV1 { maximum_read_bytes: 1, ..valid.graph_bounds },
        ..valid
      },
    ] {
      let error =
        protection.begin_semantic_task_work(&observed, invalid, fixture.memory, fixture.cancellation, fixture.retirement).unwrap_err();
      assert!(error.committed_receipt().is_none());
      assert_eq!(fs::read(fixture.path).unwrap(), before);
      assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, baseline);
    }
    let work = protection.begin_semantic_task_work(&observed, valid, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
    assert_eq!(work.receipt().control_sequence, 2);
  });
}

#[test]
fn native_task_work_never_reactivates_terminal_or_released_tasks() {
  for state in [7u16, 8, 9] {
    for released in [false, true] {
      with_initial_task_for_work(|fixture| {
        let mut bytes = fixture
          .publisher
          .load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16])
          .unwrap()
          .unwrap()
          .bytes;
        bytes[32 + 96..32 + 98].copy_from_slice(&state.to_le_bytes());
        bytes[32 + 98..32 + 100].copy_from_slice(&u16::from(released).to_le_bytes());
        crc(&mut bytes);
        seed(fixture.publisher, &[(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &bytes)]);
        let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
        let observed = observe_work_task(&fixture);
        let before = fs::read(fixture.path).unwrap();
        let input = work_request(observed.header().selected.header.updated_at_ms + 1);
        let error =
          protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap_err();
        assert!(error.committed_receipt().is_none());
        assert_eq!(fs::read(fixture.path).unwrap(), before);
      });
    }
  }
}

#[test]
fn native_task_work_checks_fence_and_both_control_advances_before_any_publication() {
  for (sequence, fence) in [(u64::MAX, 1u64), (u64::MAX - 1, 1), (1, u64::MAX)] {
    with_initial_task_for_work(|fixture| {
      let mut bytes = fixture
        .publisher
        .load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16])
        .unwrap()
        .unwrap()
        .bytes;
      bytes[16..24].copy_from_slice(&sequence.to_le_bytes());
      bytes[32 + 64..32 + 72].copy_from_slice(&fence.to_le_bytes());
      crc(&mut bytes);
      seed(fixture.publisher, &[(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &bytes)]);
      let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
      let observed = observe_work_task(&fixture);
      let before = fs::read(fixture.path).unwrap();
      let input = work_request(observed.header().selected.header.updated_at_ms + 1);
      let error =
        protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap_err();
      assert!(error.code().ends_with("_exhausted"), "{error:?}");
      assert!(error.committed_receipt().is_none());
      assert_eq!(fs::read(fixture.path).unwrap(), before);
    });
  }
}
