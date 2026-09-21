//! Unselected checkpoint output phases through shared graph fixtures.
use super::*;
use super::super::unselected::remove_fixture_task_selection;

#[test]
fn native_unselected_checkpoint_graph_visits_all_phases_and_unadmitted_rebased_candidates() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for phase in 2..=5 {
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("unselected-checkpoint-phases", None, [1; 16], algorithm, 0);
      let mut fixture = phase_fixture(&publisher, phase, phase == 5);
      remove_fixture_task_selection(&publisher, &mut fixture.expected);
      if let Some(candidate) = &fixture.candidate {
        assert!(publisher
          .load_immutable_system_control(SystemControlKindV1::RootAdmissionCommit, &[1; 16], &candidate.root_hash)
          .unwrap()
          .is_none());
      }
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let before = fs::read(&path).unwrap();
      let mut actual = PhysicalSet::new();
      let summary = capture
        .visit_captured_semantic_checkpoint_metadata_entries(&[2; 16], 1, graph_bounds(), |entry| {
          assert!(publisher.root_state.try_lock().is_ok());
          assert!(publisher.kv.try_lock().is_ok());
          actual.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
          Ok(())
        })
        .unwrap();
      assert_eq!(actual, fixture.expected);
      assert_eq!(summary.checkpoint_sequence, 1);
      assert_eq!(summary.opaque_chunk_references, if phase == 5 { 2 } else { 1 });
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

#[test]
fn native_unselected_checkpoint_graph_refuses_missing_output_branches_after_provisional_visits() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for branch in ["catalog", "output", "candidate"] {
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("unselected-checkpoint-output", None, [1; 16], algorithm, 0);
      let mut fixture = phase_fixture(&publisher, 4, false);
      remove_fixture_task_selection(&publisher, &mut fixture.expected);
      let (key, expected_code) = match branch {
        "catalog" => (
          first_authority_file_path_hash(&semantic_object_path(algorithm, 2, &fixture.catalog.object_id).unwrap(), algorithm),
          "semantic_catalog_missing",
        ),
        "output" => (
          first_authority_file_path_hash(
            &semantic_object_path(algorithm, 1, &fixture.output.as_ref().unwrap().object_id).unwrap(),
            algorithm,
          ),
          "semantic_task_graph_state_missing",
        ),
        "candidate" => (fixture.candidate.as_ref().unwrap().root_hash.clone(), "semantic_task_graph_entity_missing"),
        _ => unreachable!(),
      };
      assert!(publisher.lock_kv().unwrap().mark_deleted(&key).unwrap());
      seed_files(&publisher, &[]);
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let before = fs::read(&path).unwrap();
      let mut visits = 0;
      let error = capture
        .visit_captured_semantic_checkpoint_metadata_entries(&[2; 16], 1, graph_bounds(), |_| {
          visits += 1;
          Ok(())
        })
        .unwrap_err();
      assert_eq!(error.code(), expected_code);
      assert!(visits > 0);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}
