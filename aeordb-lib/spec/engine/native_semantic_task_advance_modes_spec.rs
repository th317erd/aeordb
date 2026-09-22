//! Real Complete bases, changed registry and durable dependency-pruning batches.
use super::*;
use super::super::modes::{BaseMode, compiler_mode_case_with_inspect};
use super::advance_boundary::resumed_request;

fn finish_mode(mode: BaseMode, expected_configurations: u64) {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let _ = compiler_mode_case_with_inspect(algorithm, mode, |fixture| {
      let head = fixture.publisher.observe().unwrap().selected.header.head_hash;
      let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
      let observed = observe_work_task(&fixture);
      let input = resumed_request(&fixture);
      let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
      let mut request = advance_request(fixture.tree, input.publication_timestamp_ms + 20);
      request.compiler_bounds.maximum_compiler_workspace_bytes = 128 << 20;
      let receipt = work.advance_compilation(request, fixture.retirement).unwrap();
      assert_eq!(receipt.phase, SemanticMutationPhaseV1::Ready);
      assert_eq!(receipt.configuration_steps, 1);
      assert_eq!(receipt.pruning_steps, 0);
      let selected = observe_work_task(&fixture);
      let checkpoint = selected.checkpoint().unwrap().unwrap();
      assert_eq!(checkpoint.configuration_count, expected_configurations);
      assert_eq!(checkpoint.expected_configuration_count, expected_configurations);
      assert!(checkpoint.pruning_catalog_root.is_none());
      assert_eq!(checkpoint.activation_generation, 0);
      assert!(!selected.task().unwrap().unwrap().pins_released);
      let candidate = checkpoint.candidate_namespace_root.unwrap();
      let admission =
        fixture.publisher.load_immutable_system_control(SystemControlKindV1::RootAdmissionCommit, &[1; 16], candidate).unwrap();
      if matches!(mode, BaseMode::Incremental) {
        // A no-op candidate is the already admitted base, not a new admission.
        assert_eq!(candidate, head);
        assert!(admission.is_some());
      } else {
        assert_ne!(candidate, head);
        assert!(admission.is_none());
      }
      assert_eq!(fixture.publisher.observe().unwrap().selected.header.head_hash, head);
    });
  }
}

#[test]
fn native_task_advance_completes_content_only_sources() {
  finish_mode(BaseMode::ContentOnly, 1);
}

#[test]
fn native_task_advance_completes_empty_complete_base() {
  finish_mode(BaseMode::CompleteEmpty, 0);
}

#[test]
fn native_task_advance_reuses_already_admitted_noop_candidate() {
  finish_mode(BaseMode::Incremental, 1);
}

#[test]
fn native_task_advance_completes_incremental_removal() {
  finish_mode(BaseMode::IncrementalRemoval, 0);
}

#[test]
fn native_task_advance_completes_changed_registry_from_retained_sources() {
  finish_mode(BaseMode::ChangedRegistry, 1);
}

#[test]
fn native_task_advance_prunes_one_dependency_per_checkpoint_across_true_restarts() {
  let algorithm = HashAlgorithm::Blake3_256;
  let mut tree = Vec::new();
  let mut original_head = Vec::new();
  let (_directory, path) = compiler_mode_case_with_inspect(algorithm, BaseMode::IncrementalPruning, |fixture| {
    tree = fixture.tree.to_vec();
    original_head = fixture.publisher.observe().unwrap().selected.header.head_hash;
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    let observed = observe_work_task(&fixture);
    assert_eq!(observed.checkpoint().unwrap().unwrap().dependency_count, 4);
    let input = resumed_request(&fixture);
    let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
    let mut request = advance_request(fixture.tree, input.publication_timestamp_ms + 20);
    request.compiler_bounds.maximum_compiler_workspace_bytes = 128 << 20;
    let result = work.advance_compilation(request, fixture.retirement).unwrap();
    assert_eq!(result.phase, SemanticMutationPhaseV1::Pruning);
    assert_eq!(result.configuration_steps, 1);
    assert_eq!(result.pruning_steps, 1);
    assert_eq!(result.publication.control_sequence, 5);
    let selected = observe_work_task(&fixture);
    let checkpoint = selected.checkpoint().unwrap().unwrap();
    assert_eq!(checkpoint.configuration_count, 0);
    assert_eq!(checkpoint.dependency_count, 3);
    assert_eq!(checkpoint.pruning_record_count, 3);
    assert_eq!(checkpoint.record_count, 4);
    assert!(checkpoint.pruning_catalog_root.is_some());
    assert!(checkpoint.semantic_state.is_none());
    assert!(checkpoint.candidate_namespace_root.is_none());
    assert!(!selected.task().unwrap().unwrap().pins_released);
  });
  for remaining in (0..3).rev() {
    let (_coordinator, publisher) = reopen(&path);
    let memory = MemoryCoordinator::new(MemoryPolicy::new(768 << 20, 1024 << 20, 1, 16 << 20).unwrap());
    let cancellation = CancellationToken::new();
    {
      let summary = publisher.reconstruct_retirement_journal_summary(&cancellation, &memory, 64, 64, 64, 4 << 20).unwrap().unwrap();
      let mut retirement = RetirementJournalOwnerV1::resume_chain(
        algorithm,
        [1; 16],
        &summary,
        RetirementJournalBufferOptionsV1::new(1, 1 << 20, 30_000),
        &cancellation,
        &memory,
      )
      .unwrap();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let observed = publisher
        .observe_semantic_mutation_task(SemanticMutationObservationRequestV1 {
          database_id: &[1; 16],
          task_id: &[2; 16],
          memory: &memory,
          cancellation: &cancellation,
        })
        .unwrap();
      assert_eq!(observed.checkpoint().unwrap().unwrap().phase, SemanticMutationPhaseV1::Pruning);
      let old_sequence = observed.task().unwrap().unwrap().checkpoint_sequence;
      let input = work_request(observed.header().selected.header.updated_at_ms + 1);
      let work = protection.begin_semantic_task_work(&observed, input, &memory, &cancellation, &mut retirement).unwrap();
      let mut request = advance_request(&tree, input.publication_timestamp_ms + 20);
      request.compiler_bounds.maximum_compiler_workspace_bytes = 128 << 20;
      let result = work.advance_compilation(request, &mut retirement).unwrap();
      assert_eq!(result.configuration_steps, 0);
      assert_eq!(result.pruning_steps, 1);
      assert_eq!(result.publication.control_sequence, 11 - 2 * remaining);
      let selected = publisher
        .observe_semantic_mutation_task(SemanticMutationObservationRequestV1 {
          database_id: &[1; 16],
          task_id: &[2; 16],
          memory: &memory,
          cancellation: &cancellation,
        })
        .unwrap();
      let checkpoint = selected.checkpoint().unwrap().unwrap();
      assert_eq!(checkpoint.dependency_count, remaining);
      assert_eq!(checkpoint.pruning_record_count, remaining);
      assert_eq!(checkpoint.record_count, 1 + remaining);
      assert_eq!(checkpoint.checkpoint_sequence, old_sequence + 1);
      assert!(!selected.task().unwrap().unwrap().pins_released);
      assert_eq!(checkpoint.phase, if remaining == 0 { SemanticMutationPhaseV1::Ready } else { SemanticMutationPhaseV1::Pruning });
      let capture = protection.capture_semantic_mutation_inventory(input.inventory_bounds, &memory, &cancellation).unwrap();
      // Both old and new graphs remain traversable; only current selection moves.
      for sequence in [old_sequence, old_sequence + 1] {
        capture.visit_captured_semantic_checkpoint_metadata_entries(&[2; 16], sequence, selection_bounds(), |_| Ok(())).unwrap();
      }
      assert_eq!(publisher.observe().unwrap().selected.header.head_hash, original_head);
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}
