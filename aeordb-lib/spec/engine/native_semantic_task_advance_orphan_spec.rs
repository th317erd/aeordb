//! Staged Ready output never replaces selected work without task commitment.
use super::*;
use super::advance_boundary::start_fixture_compilation;

fn pair_identity(sequence: u64) -> [u8; 24] {
  let mut identity = [2; 24];
  identity[16..].copy_from_slice(&sequence.to_le_bytes());
  identity
}

fn pair(publisher: &V4FirstAuthorityPublisher, sequence: u64) -> [Vec<u8>; 2] {
  [SystemControlKindV1::SemanticMutationCheckpoint, SystemControlKindV1::SemanticSourceCapture]
    .map(|kind| publisher.load_immutable_system_control(kind, &[1; 16], &pair_identity(sequence)).unwrap().unwrap().bytes)
}

#[test]
fn native_task_advance_reopens_after_two_unselected_ready_pairs_with_fresh_fence() {
  let mut tree = Vec::new();
  let mut head = Vec::new();
  let mut abandoned = Vec::new();
  let (_directory, path) = with_initial_task_for_work(|mut fixture| {
    start_fixture_compilation(&mut fixture);
    tree = fixture.tree.to_vec();
    head = fixture.publisher.observe().unwrap().selected.header.head_hash;
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    for sequence in [3, 4] {
      let observed = observe_work_task(&fixture);
      let attempt = CancellationToken::new();
      let input = NativeSemanticTaskWorkRequestV1 {
        monotonic_now_ms: 40_000 + (sequence - 3) * 20_000,
        ..work_request(observed.header().selected.header.updated_at_ms + 1)
      };
      let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, &attempt, fixture.retirement).unwrap();
      assert_eq!(work.reserved_checkpoint_sequence(), sequence);
      let mut request = advance_request(fixture.tree, input.publication_timestamp_ms + 20);
      request.monotonic_now_ms = input.monotonic_now_ms + 10_000;
      let reached = std::cell::Cell::new(false);
      let error = work
        .advance_compilation_observed(
          request,
          fixture.retirement,
          (
            || {},
            || {},
            || {
              reached.set(true);
              attempt.cancel();
            },
          ),
          (
            &mut NoopFirstAuthorityDependencyObserverV1,
            &mut NoopFirstAuthorityDependencyObserverV1,
            &mut NoopFirstAuthorityDependencyObserverV1,
          ),
        )
        .unwrap_err();
      assert!(reached.get(), "{error}");
      assert!(error.committed_receipt().is_none());
      let selected = observe_work_task(&fixture);
      let task = selected.task().unwrap().unwrap();
      assert_eq!(task.control_sequence, sequence + 1);
      assert_eq!(task.checkpoint_sequence, 2);
      assert_eq!(task.fencing_token, sequence);
      assert_eq!(task.state, SemanticMutationTaskStateV1::Compiling);
      assert!(!task.pins_released);
      let bytes = pair(fixture.publisher, sequence);
      let checkpoint =
        crate::engine::v4::semantic_mutation_control::decode_semantic_mutation_checkpoint(&bytes[0], HashAlgorithm::Blake3_256).unwrap();
      assert_eq!(checkpoint.phase, SemanticMutationPhaseV1::Ready);
      let candidate = checkpoint.candidate_namespace_root.unwrap();
      assert!(fixture.publisher.load_immutable_entity_bounded(candidate, 1 << 20).unwrap().is_some());
      assert!(fixture
        .publisher
        .load_immutable_system_control(SystemControlKindV1::RootAdmissionCommit, &[1; 16], candidate)
        .unwrap()
        .is_none());
      abandoned.push(bytes);
      let capture = protection.capture_semantic_mutation_inventory(input.inventory_bounds, fixture.memory, fixture.cancellation).unwrap();
      let mut retained = std::collections::BTreeSet::new();
      assert!(
        capture
          .visit_captured_semantic_task_retention_entries(
            NativeSemanticTaskRetentionBoundsV1 { maximum_work: 16384, maximum_read_bytes: 64 << 20, graphs: selection_bounds() },
            |entry| {
              retained.insert(entry.hash.clone());
              Ok(())
            },
          )
          .unwrap()
          .complete
      );
      let old_path =
        system_control_path(SystemControlKindV1::SemanticMutationCheckpoint, &pair_identity(2), SystemControlSlotV1::Immutable).unwrap();
      assert!(retained.contains(&first_authority_file_path_hash(&old_path, HashAlgorithm::Blake3_256)));
      assert_eq!(fixture.publisher.observe().unwrap().selected.header.head_hash, head);
    }
  });
  let (_coordinator, publisher) = reopen(&path);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 1, 16 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let summary = publisher.reconstruct_retirement_journal_summary(&cancellation, &memory, 64, 64, 64, 4 << 20).unwrap().unwrap();
  let mut retirement = RetirementJournalOwnerV1::resume_chain(
    HashAlgorithm::Blake3_256,
    [1; 16],
    &summary,
    RetirementJournalBufferOptionsV1::new(1, 1 << 20, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  {
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let observed = publisher
      .observe_semantic_mutation_task(SemanticMutationObservationRequestV1 {
        database_id: &[1; 16],
        task_id: &[2; 16],
        memory: &memory,
        cancellation: &cancellation,
      })
      .unwrap();
    let input = work_request(observed.header().selected.header.updated_at_ms + 1);
    let work = protection.begin_semantic_task_work(&observed, input, &memory, &cancellation, &mut retirement).unwrap();
    assert_eq!(work.reserved_checkpoint_sequence(), 5);
    assert_eq!(work.receipt().control_sequence, 6);
    let receipt = work.advance_compilation(advance_request(&tree, input.publication_timestamp_ms + 20), &mut retirement).unwrap();
    assert_eq!(receipt.publication.control_sequence, 7);
    assert_eq!(receipt.phase, SemanticMutationPhaseV1::Ready);
    for (sequence, expected) in [3, 4].into_iter().zip(&abandoned) {
      assert_eq!(&pair(&publisher, sequence), expected);
    }
    let after = publisher.reconstruct_retirement_journal_summary(&cancellation, &memory, 64, 64, 64, 4 << 20).unwrap().unwrap();
    assert!(after.segment_count > summary.segment_count);
    assert!(after.last_replacement_sequence > summary.last_replacement_sequence);
    assert_eq!(publisher.observe().unwrap().selected.header.head_hash, head);
    let observed = publisher
      .observe_semantic_mutation_task(SemanticMutationObservationRequestV1 {
        database_id: &[1; 16],
        task_id: &[2; 16],
        memory: &memory,
        cancellation: &cancellation,
      })
      .unwrap();
    assert_eq!(observed.task().unwrap().unwrap().checkpoint_sequence, 5);
    assert!(!observed.task().unwrap().unwrap().pins_released);
  }
  drop(retirement);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}
