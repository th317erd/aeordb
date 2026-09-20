//! Draft following-boundary tests; registered after the original failing run.
use super::*;
use std::path::Path;

fn with_checkpoint_fixture(
  test: impl FnOnce(&V4FirstAuthorityPublisher, &NativeStagedSemanticSourceUnionV1<'_>, &MemoryCoordinator, &CancellationToken, &Path),
) {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("captured-checkpoint-boundaries", None, [1; 16], algorithm, 0);
  let initial = request_for_database_and_algorithm([1; 16], algorithm);
  let root = publisher.publish(&initial).unwrap().namespace_root.root_hash;
  enable_node_staging(&publisher);
  seed_union_generation(&publisher);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  {
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let parent = tempfile::tempdir().unwrap();
    let staged = capture
      .prepare_and_stage_semantic_source_union(
        NativeSemanticSourceUnionRequestV1 {
          expected_base_root: &root,
          requested_directory_root: &initial.namespace_tree.root_hash,
          replacements: &[],
          workspace_parent: parent.path(),
          bounds: union_bounds(&initial.namespace_tree.root_hash),
        },
        staging_request(),
      )
      .unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    test(&publisher, &staged, &memory, &cancellation, &path);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn native_captured_checkpoint_invalid_requests_refuse_before_writes_and_allow_retry() {
  with_checkpoint_fixture(|_, staged, _, _, path| {
    let valid = request(staged.source_union().captured_header().updated_at_ms + 1);
    let before = fs::read(path).unwrap();
    for case in 0..8 {
      let mut invalid = valid;
      match case {
        0 => invalid.task_id = [0; 16],
        1 => invalid.mutation_count = 0,
        2 => invalid.captured_at_ms = -1,
        3 => invalid.captured_at_ms = staged.source_union().captured_header().updated_at_ms as i64 - 1,
        4 => invalid.publication_timestamp_ms = 0,
        5 => invalid.publication_timestamp_ms = i64::MAX as u64 + 1,
        6 => invalid.publication_timestamp_ms = valid.captured_at_ms as u64 - 1,
        _ => invalid.maximum_workspace_bytes = 1,
      }
      let error = staged.stage_initial_checkpoint(invalid).unwrap_err();
      assert!(error.committed_receipt().is_none(), "case={case}: {error:?}");
      assert_eq!(fs::read(path).unwrap(), before);
    }
    assert!(!staged.stage_initial_checkpoint(valid).unwrap().idempotent);
  });
}

#[test]
fn native_captured_checkpoint_same_identity_different_exact_bytes_cannot_replace_the_pair() {
  with_checkpoint_fixture(|_, staged, _, _, path| {
    let input = request(staged.source_union().captured_header().updated_at_ms + 1);
    staged.stage_initial_checkpoint(input).unwrap();
    let before = fs::read(path).unwrap();
    for changed in [
      NativeCapturedSemanticCheckpointRequestV1 { mutation_count: 4, ..input },
      NativeCapturedSemanticCheckpointRequestV1 { captured_at_ms: input.captured_at_ms + 1, ..input },
    ] {
      let error = staged.stage_initial_checkpoint(changed).unwrap_err();
      assert_eq!(error.code(), "semantic_source_node_collision");
      assert!(error.committed_receipt().is_none());
      assert_eq!(fs::read(path).unwrap(), before);
    }
    assert!(staged.stage_initial_checkpoint(input).unwrap().idempotent);
    assert_eq!(fs::read(path).unwrap(), before);
  });
}

#[test]
fn native_captured_checkpoint_final_cancellation_or_pressure_prevents_writes() {
  use crate::engine::memory_coordinator::HostMemorySample;
  for cancel in [false, true] {
    with_checkpoint_fixture(|publisher, staged, memory, cancellation, path| {
      let input = request(staged.source_union().captured_header().updated_at_ms + 1);
      let before = fs::read(path).unwrap();
      let result = staged.stage_initial_checkpoint_observed(
        input,
        || {
          assert!(publisher.root_state.try_lock().is_ok());
          if cancel {
            cancellation.cancel();
          } else {
            memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
          }
        },
        &mut NoopFirstAuthorityDependencyObserverV1,
      );
      let error = result.unwrap_err();
      assert_eq!(error.code(), if cancel { "semantic_task_observation_cancelled" } else { "semantic_task_observation_memory" });
      assert!(error.committed_receipt().is_none());
      memory.update_host_sample(HostMemorySample::default()).unwrap();
      assert_eq!(fs::read(path).unwrap(), before);
      if !cancel {
        assert!(!staged.stage_initial_checkpoint(input).unwrap().idempotent);
      }
    });
  }
}

#[test]
fn native_captured_checkpoint_actual_output_allocation_failure_preserves_bytes_and_retries() {
  with_checkpoint_fixture(|_, staged, memory, _, path| {
    let input = request(staged.source_union().captured_header().updated_at_ms + 1);
    let before = fs::read(path).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let length = 36 + 168 + 9 * staged.source_union().captured_header().hash_algorithm.hash_length();
    let (result, allocations) = measure(length, || staged.stage_initial_checkpoint(input));
    assert!(allocations.injected_failure, "{allocations:?}");
    let error = result.unwrap_err();
    assert_eq!(error.code(), "system_control_output_allocation");
    assert!(error.committed_receipt().is_none());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(path).unwrap(), before);
    assert!(!staged.stage_initial_checkpoint(input).unwrap().idempotent);
  });
}

#[test]
fn native_captured_checkpoint_rechecks_physical_owner_epoch_and_capabilities_before_retry() {
  for case in 0..4 {
    with_checkpoint_fixture(|publisher, staged, _, _, path| {
      let input = request(staged.source_union().captured_header().updated_at_ms + 1);
      staged.stage_initial_checkpoint(input).unwrap();
      let mut current = publisher.observe().unwrap().selected.header;
      match case {
        0 => current.physical_instance_id = [0x72; 16],
        1 => current.writer_fence_epoch += 1,
        2 => current.required_reader_capabilities[3] &= !0b0010,
        _ => current.required_writer_capabilities[3] &= !0b1000,
      }
      write_redundant_header(publisher, &current);
      let before = fs::read(path).unwrap();
      let error = staged.stage_initial_checkpoint(input).unwrap_err();
      assert_eq!(error.code(), if case < 2 { "semantic_source_node_owner" } else { "semantic_source_node_capability" });
      assert!(error.committed_receipt().is_none());
      assert_eq!(publisher.observe().unwrap().selected.header, current);
      assert_eq!(fs::read(path).unwrap(), before);
    });
  }
}

#[test]
fn native_captured_checkpoint_committed_error_and_late_cancellation_keep_exact_receipts() {
  for cancel in [false, true] {
    with_checkpoint_fixture(|publisher, staged, _, cancellation, _| {
      let input = request(staged.source_union().captured_header().updated_at_ms + 1);
      let mut failure = FailingPostCommitObserver;
      let mut late_cancel = CancelRetirementAfterCommitObserver { cancellation: cancellation.clone() };
      let observer: &mut dyn FirstAuthorityDependencyObserverV1 = if cancel { &mut late_cancel } else { &mut failure };
      let result = staged.stage_initial_checkpoint_observed(input, || {}, observer);
      let receipt = if cancel {
        assert!(cancellation.is_cancelled());
        result.unwrap()
      } else {
        let error = result.unwrap_err();
        assert_eq!(error.code(), "immutable_entity_committed_postcondition_failure");
        let receipt = error.committed_receipt().unwrap().clone();
        assert!(staged.stage_initial_checkpoint(input).unwrap().idempotent);
        receipt
      };
      assert_eq!(receipt.controls.len(), 2);
      assert!(!receipt.idempotent);
      for control in receipt.controls {
        assert!(publisher.locator(&control.path_key).unwrap().is_some());
      }
      assert_eq!(publisher.observe().unwrap().selected.header.head_hash, staged.source_union().base_authority().root_hash);
    });
  }
}

#[test]
fn native_captured_checkpoint_precommit_failures_select_neither_dependency() {
  for case in 0..4 {
    with_checkpoint_fixture(|publisher, staged, _, _, _| {
      let input = request(staged.source_union().captured_header().updated_at_ms + 1);
      let before = publisher.observe().unwrap();
      let mut visibility = FailingVisibilityObserver;
      let mut dependency = FailingDependencyObserver {
        phase: match case {
          0 => DependencyFailurePhase::BeforeEntity,
          1 => DependencyFailurePhase::EntityWritten,
          _ => DependencyFailurePhase::EntityStaged,
        },
        entity_index: 2,
      };
      let observer: &mut dyn FirstAuthorityDependencyObserverV1 = if case == 3 { &mut visibility } else { &mut dependency };
      let error = staged.stage_initial_checkpoint_observed(input, || {}, observer).unwrap_err();
      assert_eq!(error.code(), "durability_failure");
      assert!(error.committed_receipt().is_none());
      assert_eq!(publisher.observe().unwrap(), before);
      for kind in [SystemControlKindV1::SemanticMutationCheckpoint, SystemControlKindV1::SemanticSourceCapture] {
        let path = system_control_path(kind, &checkpoint_identity(), SystemControlSlotV1::Immutable).unwrap();
        let key = first_authority_file_path_hash(&path, before.selected.header.hash_algorithm);
        assert!(publisher.locator(&key).unwrap().is_none());
      }
      let retry = staged.stage_initial_checkpoint(input).unwrap_err();
      assert_eq!(retry.code(), "durability_failure");
      assert!(retry.committed_receipt().is_none());
    });
  }
}

#[test]
fn native_captured_checkpoint_concurrent_identical_calls_publish_once() {
  with_checkpoint_fixture(|publisher, staged, _, _, _| {
    let input = request(staged.source_union().captured_header().updated_at_ms + 1);
    let before = publisher.observe().unwrap().selected.header;
    let (ready_sender, ready_receiver) = mpsc::channel();
    let (release_first, first_receiver) = mpsc::channel();
    let (release_second, second_receiver) = mpsc::channel();
    let results = std::thread::scope(|scope| {
      let run = |release: mpsc::Receiver<()>| {
        staged.stage_initial_checkpoint_observed(
          input,
          || {
            ready_sender.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(2)).unwrap();
          },
          &mut NoopFirstAuthorityDependencyObserverV1,
        )
      };
      let first = scope.spawn(move || run(first_receiver));
      let second = scope.spawn(move || run(second_receiver));
      let ready =
        ready_receiver.recv_timeout(Duration::from_secs(2)).is_ok() && ready_receiver.recv_timeout(Duration::from_secs(2)).is_ok();
      let release_a = release_first.send(());
      let release_b = release_second.send(());
      let results = [first.join().unwrap().unwrap(), second.join().unwrap().unwrap()];
      assert!(ready && release_a.is_ok() && release_b.is_ok());
      results
    });
    assert_eq!(results.iter().filter(|receipt| receipt.idempotent).count(), 1);
    assert_eq!(
      results[0].controls,
      results[1]
        .controls
        .iter()
        .map(|control| {
          let mut expected = control.clone();
          expected.idempotent = results[0].idempotent;
          expected
        })
        .collect::<Vec<_>>()
    );
    let after = publisher.observe().unwrap().selected.header;
    assert_eq!(after.head_hash, before.head_hash);
    assert_eq!(after.entry_count, before.entry_count + 4);
    assert_eq!(after.write_sequence_high_water, before.write_sequence_high_water + 4);
  });
}

#[test]
fn native_captured_checkpoint_counts_deletions_and_namespace_only_sources() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for case in 0..3 {
      let (_directory, _path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("captured-checkpoint-source-counts", None, [1; 16], algorithm, 0);
      let initial = request_for_database_and_algorithm([1; 16], algorithm);
      let mut root = publisher.publish(&initial).unwrap().namespace_root.root_hash;
      enable_node_staging(&publisher);
      seed_union_generation(&publisher);
      let body = br#"{"$v":1,"indexes":[]}"#;
      let mut tree = initial.namespace_tree.root_hash.clone();
      if case == 0 {
        seed_files(&publisher, &[(INDEX_SOURCE.to_owned(), "application/json", body)]);
      } else {
        let (directory, _) = namespace_configuration_tree(&publisher, "/topic", body, vec![]);
        let configured_tree = publish_namespace_directory(&publisher, vec![namespace_directory_child("topic", directory)]);
        if case == 1 {
          tree = configured_tree;
        } else {
          let loaded = publisher.load_immutable_entity_bounded(&configured_tree, 1 << 20).unwrap().unwrap();
          let mut next = successor_request(&publisher, 0x93, "unused-fixture-child");
          next.semantic_state = initial.semantic_state.clone();
          next.namespace_tree = PreparedNamespaceTreeV0 { root_hash: configured_tree, stored_value: loaded.stored_value };
          root = publisher.publish_successor_authority(&next).unwrap().namespace_root.root_hash;
        }
      }
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let parent = tempfile::tempdir().unwrap();
      let deletion = [NativeSemanticSourceReplacementV1 { path: INDEX_SOURCE, file_record_id: None }];
      let staged = capture
        .prepare_and_stage_semantic_source_union(
          NativeSemanticSourceUnionRequestV1 {
            expected_base_root: &root,
            requested_directory_root: &tree,
            replacements: if case == 0 { &deletion } else { &[] },
            workspace_parent: parent.path(),
            bounds: union_bounds(&tree),
          },
          staging_request(),
        )
        .unwrap();
      let input = request(staged.source_union().captured_header().updated_at_ms + 1);
      let expected = expected_pair(staged.source_union(), input, u64::from(case == 1));
      staged.stage_initial_checkpoint(input).unwrap();
      let actual = publisher
        .load_immutable_system_control(SystemControlKindV1::SemanticMutationCheckpoint, &[1; 16], &checkpoint_identity())
        .unwrap()
        .unwrap();
      assert_eq!(actual.bytes, expected.0, "case={case}");
      let fresh = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let validated = fresh.validate_captured_semantic_source_union(&[2; 16], 1, validation_bounds(&tree)).unwrap();
      assert_eq!(validated.requested_configuration_count, u64::from(case == 1));
      assert_eq!(validated.base_configuration_count, u64::from(case != 1));
    }
  }
}

#[test]
fn native_captured_checkpoint_retry_completes_a_partially_existing_pair_without_replacing_it() {
  for existing in 0..2 {
    with_checkpoint_fixture(|publisher, staged, _, _, _| {
      let input = request(staged.source_union().captured_header().updated_at_ms + 1);
      let expected = expected_pair(staged.source_union(), input, 0);
      let (kind, bytes) = if existing == 0 {
        (SystemControlKindV1::SemanticMutationCheckpoint, expected.0.as_slice())
      } else {
        (SystemControlKindV1::SemanticSourceCapture, expected.1.as_slice())
      };
      seed(publisher, &[(kind, &checkpoint_identity(), SystemControlSlotV1::Immutable, bytes)]);
      let before = publisher.observe().unwrap().selected.header;
      let receipt = staged.stage_initial_checkpoint(input).unwrap();
      assert!(!receipt.idempotent);
      assert_eq!(receipt.controls.len(), 2);
      assert!(receipt.controls[existing].idempotent);
      assert!(!receipt.controls[1 - existing].idempotent);
      assert_eq!(receipt.observation.selected.header.entry_count, before.entry_count + 2);
      for (kind, bytes) in
        [(SystemControlKindV1::SemanticMutationCheckpoint, expected.0), (SystemControlKindV1::SemanticSourceCapture, expected.1)]
      {
        assert_eq!(publisher.load_immutable_system_control(kind, &[1; 16], &checkpoint_identity()).unwrap().unwrap().bytes, bytes);
      }
    });
  }
}

#[test]
fn native_captured_checkpoint_reopen_distinguishes_precommit_failure_from_committed_dependencies() {
  for committed in [false, true] {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("captured-checkpoint-recovery", None, [1; 16], algorithm, 0);
    let initial = request_for_database_and_algorithm([1; 16], algorithm);
    let root = publisher.publish(&initial).unwrap().namespace_root.root_hash;
    enable_node_staging(&publisher);
    seed_union_generation(&publisher);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let expected;
    {
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let parent = tempfile::tempdir().unwrap();
      let staged = capture
        .prepare_and_stage_semantic_source_union(
          NativeSemanticSourceUnionRequestV1 {
            expected_base_root: &root,
            requested_directory_root: &initial.namespace_tree.root_hash,
            replacements: &[],
            workspace_parent: parent.path(),
            bounds: union_bounds(&initial.namespace_tree.root_hash),
          },
          staging_request(),
        )
        .unwrap();
      let input = request(staged.source_union().captured_header().updated_at_ms + 1);
      expected = expected_pair(staged.source_union(), input, 0);
      let mut before_commit = FailingVisibilityObserver;
      let mut after_commit = FailingPostCommitObserver;
      let observer: &mut dyn FirstAuthorityDependencyObserverV1 = if committed { &mut after_commit } else { &mut before_commit };
      let error = staged.stage_initial_checkpoint_observed(input, || {}, observer).unwrap_err();
      assert_eq!(error.committed_receipt().is_some(), committed);
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    drop(publisher);
    let (_coordinator, publisher) = reopen(&path);
    let before_read = fs::read(&path).unwrap();
    for (kind, bytes) in
      [(SystemControlKindV1::SemanticMutationCheckpoint, expected.0), (SystemControlKindV1::SemanticSourceCapture, expected.1)]
    {
      let actual = publisher.load_immutable_system_control(kind, &[1; 16], &checkpoint_identity()).unwrap();
      if committed {
        assert_eq!(actual.unwrap().bytes, bytes);
      } else {
        assert!(actual.is_none());
      }
    }
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let inventory = capture.visit(|_| panic!("staged dependencies never become selected tasks after recovery")).unwrap();
    assert!(inventory.complete);
    assert_eq!(inventory.tasks, 0);
    assert_eq!(fs::read(&path).unwrap(), before_read);
    if committed {
      capture.validate_captured_semantic_source_union(&[2; 16], 1, validation_bounds(&initial.namespace_tree.root_hash)).unwrap();
      assert_eq!(fs::read(&path).unwrap(), before_read);
    } else {
      // There is no selected task to resume. A fresh capture can stage anew;
      // it must not pretend to be the earlier, now-lost in-process capture.
      let parent = tempfile::tempdir().unwrap();
      let staged = capture
        .prepare_and_stage_semantic_source_union(
          NativeSemanticSourceUnionRequestV1 {
            expected_base_root: &root,
            requested_directory_root: &initial.namespace_tree.root_hash,
            replacements: &[],
            workspace_parent: parent.path(),
            bounds: union_bounds(&initial.namespace_tree.root_hash),
          },
          staging_request(),
        )
        .unwrap();
      let input = request(staged.source_union().captured_header().updated_at_ms + 1);
      assert!(!staged.stage_initial_checkpoint(input).unwrap().idempotent);
    }
  }
}
