//! Failing-first durable continuation from the selected compiler checkpoint.
#[path = "native_semantic_task_advance_boundary_spec.rs"]
mod advance_boundary;
#[path = "native_semantic_task_advance_dependency_spec.rs"]
mod advance_dependency;
#[path = "native_semantic_task_advance_fault_spec.rs"]
mod advance_fault;
#[path = "native_semantic_task_advance_guard_spec.rs"]
mod advance_guard;
#[path = "native_semantic_task_advance_modes_spec.rs"]
mod advance_modes;
#[path = "native_semantic_task_advance_order_spec.rs"]
mod advance_order;
#[path = "native_semantic_task_advance_orphan_spec.rs"]
mod advance_orphan;
#[path = "native_semantic_task_advance_output_spec.rs"]
mod advance_output;
#[path = "native_semantic_task_advance_resource_spec.rs"]
mod advance_resource;
#[path = "native_semantic_task_advance_restart_spec.rs"]
mod advance_restart;
use super::*;

fn advance_request(tree: &[u8], timestamp: u64) -> NativeSemanticTaskCompilerAdvanceRequestV1 {
  NativeSemanticTaskCompilerAdvanceRequestV1 {
    maximum_configuration_steps: 1,
    maximum_pruning_steps: 1,
    compiler_bounds: start_request(tree, timestamp).compiler_bounds,
    publication_timestamp_ms: timestamp,
    monotonic_now_ms: 40_000,
    maximum_workspace_bytes: 16 << 20,
  }
}

#[test]
fn native_task_advance_reacquires_persisted_compiling_work_after_reopen() {
  let (_directory, path) = with_initial_task_for_work(|fixture| {
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    let observed = observe_work_task(&fixture);
    let input = work_request(observed.header().selected.header.updated_at_ms + 1);
    let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
    let receipt = work.start_compilation(start_request(fixture.tree, input.publication_timestamp_ms + 20), fixture.retirement).unwrap();
    assert_eq!(receipt.control_sequence, 3);
    assert!(receipt.retirement_hard_publication_sequence.is_some());
  });
  // The fixture returns only after the original publisher and leases are gone.
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
  let head = publisher.observe().unwrap().selected.header.head_hash;
  let generation = publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationGeneration, &[1; 16], &[]).unwrap();
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
    let old = observed.task().unwrap().unwrap();
    assert_eq!(old.state, SemanticMutationTaskStateV1::Compiling);
    assert_eq!(old.checkpoint_sequence, 2);
    let mut checkpoint_identity = [0; 24];
    checkpoint_identity[..16].fill(2);
    checkpoint_identity[16..].copy_from_slice(&2u64.to_le_bytes());
    let old_checkpoint = publisher
      .load_immutable_system_control(SystemControlKindV1::SemanticMutationCheckpoint, &[1; 16], &checkpoint_identity)
      .unwrap()
      .unwrap();
    let input = work_request(observed.header().selected.header.updated_at_ms + 1);
    let work = protection
      .begin_semantic_task_work(&observed, input, &memory, &cancellation, &mut retirement)
      .expect("selected Compiling work must resume with a fresh fence after true reopen");
    assert_eq!(work.reserved_checkpoint_sequence(), 3);
    assert_eq!(work.receipt().control_sequence, 4);
    assert_eq!(work.receipt().selected_slot, SystemControlSlotV1::B);
    let selected = publisher
      .observe_semantic_mutation_task(SemanticMutationObservationRequestV1 {
        database_id: &[1; 16],
        task_id: &[2; 16],
        memory: &memory,
        cancellation: &cancellation,
      })
      .unwrap();
    let task = selected.task().unwrap().unwrap();
    assert_eq!(task.state, SemanticMutationTaskStateV1::Compiling);
    assert_eq!(task.fencing_token, 3);
    assert_eq!(task.checkpoint_sequence, 2);
    assert!(!task.pins_released);
    assert_eq!(selected.checkpoint().unwrap().unwrap().phase, SemanticMutationPhaseV1::Compiling);
    assert_eq!(
      publisher.load_immutable_system_control(SystemControlKindV1::SemanticMutationCheckpoint, &[1; 16], &checkpoint_identity).unwrap(),
      Some(old_checkpoint)
    );
    assert_eq!(publisher.observe().unwrap().selected.header.head_hash, head);
    assert_eq!(publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationGeneration, &[1; 16], &[]).unwrap(), generation);
  }
  drop(retirement);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn native_task_advance_selects_ready_output_without_activating_head() {
  with_initial_task_for_work(|fixture| {
    let head = fixture.publisher.observe().unwrap().selected.header.head_hash;
    let generation = fixture.publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationGeneration, &[1; 16], &[]).unwrap();
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    {
      let observed = observe_work_task(&fixture);
      let input = work_request(observed.header().selected.header.updated_at_ms + 1);
      let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
      work.start_compilation(start_request(fixture.tree, input.publication_timestamp_ms + 20), fixture.retirement).unwrap();
    }
    let observed = observe_work_task(&fixture);
    let input =
      NativeSemanticTaskWorkRequestV1 { monotonic_now_ms: 35_000, ..work_request(observed.header().selected.header.updated_at_ms + 1) };
    let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
    let receipt = work
      .advance_compilation(advance_request(fixture.tree, input.publication_timestamp_ms + 20), fixture.retirement)
      .expect("the empty captured source union must yield real durable Ready output");
    assert_eq!(receipt.publication.control_sequence, 5);
    assert_eq!(receipt.phase, SemanticMutationPhaseV1::Ready);
    assert_eq!(receipt.configuration_steps, 1);
    assert_eq!(receipt.pruning_steps, 0);
    let selected = observe_work_task(&fixture);
    let task = selected.task().unwrap().unwrap();
    let checkpoint = selected.checkpoint().unwrap().unwrap();
    assert_eq!(task.state, SemanticMutationTaskStateV1::ReadyToActivate);
    assert_eq!(task.checkpoint_sequence, 3);
    assert_eq!(task.fencing_token, 3);
    assert!(!task.pins_released);
    assert_eq!(checkpoint.phase, SemanticMutationPhaseV1::Ready);
    assert_eq!(checkpoint.configuration_count, 0);
    assert_eq!(checkpoint.expected_configuration_count, 0);
    assert_eq!(checkpoint.cursor, crate::engine::v4::semantic_mutation_control::SemanticMutationCursorV1::None);
    assert_eq!(checkpoint.activation_generation, 0);
    assert!(checkpoint.catalog_root.is_some());
    assert!(checkpoint.semantic_state.is_some());
    assert!(checkpoint.pruning_catalog_root.is_none());
    let candidate = checkpoint.candidate_namespace_root.expect("Ready needs a real staged candidate root");
    assert!(fixture
      .publisher
      .load_immutable_system_control(SystemControlKindV1::RootAdmissionCommit, &[1; 16], candidate)
      .unwrap()
      .is_none());
    assert_eq!(fixture.publisher.observe().unwrap().selected.header.head_hash, head);
    assert_eq!(
      fixture.publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationGeneration, &[1; 16], &[]).unwrap(),
      generation
    );
    let capture =
      protection.capture_semantic_mutation_inventory(work_request(1).inventory_bounds, fixture.memory, fixture.cancellation).unwrap();
    let mut retained = std::collections::BTreeSet::new();
    capture
      .visit_captured_semantic_checkpoint_metadata_entries(&[2; 16], 3, selection_bounds(), |entry| {
        retained.insert(entry.hash.clone());
        Ok(())
      })
      .unwrap();
    assert!(retained.contains(&head));
    assert!(retained.contains(candidate));
  });
}

#[test]
fn native_task_advance_reacquired_compiling_work_cannot_restart_captured_compilation() {
  with_initial_task_for_work(|fixture| {
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    {
      let observed = observe_work_task(&fixture);
      let input = work_request(observed.header().selected.header.updated_at_ms + 1);
      let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
      work.start_compilation(start_request(fixture.tree, input.publication_timestamp_ms + 20), fixture.retirement).unwrap();
    }
    let observed = observe_work_task(&fixture);
    let input =
      NativeSemanticTaskWorkRequestV1 { monotonic_now_ms: 35_000, ..work_request(observed.header().selected.header.updated_at_ms + 1) };
    let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
    let before = fs::read(fixture.path).unwrap();
    let error = work.start_compilation(start_request(fixture.tree, input.publication_timestamp_ms + 20), fixture.retirement).unwrap_err();
    assert_eq!(error.code(), "semantic_task_work_phase");
    assert!(error.committed_receipt().is_none());
    assert_eq!(fs::read(fixture.path).unwrap(), before);
    let selected = observe_work_task(&fixture);
    assert_eq!(selected.task().unwrap().unwrap().checkpoint_sequence, 2);
    assert_eq!(selected.task().unwrap().unwrap().state, SemanticMutationTaskStateV1::Compiling);
  });
}
