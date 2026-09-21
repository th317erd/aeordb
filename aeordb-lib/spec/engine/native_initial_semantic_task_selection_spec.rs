//! Initial task selection: real staging, independent bytes and native boundaries.
#[path = "native_semantic_task_work_spec.rs"]
mod work;
use super::*;

fn selection_bounds() -> NativeSemanticTaskGraphBoundsV1 {
  NativeSemanticTaskGraphBoundsV1 {
    maximum_work: 8192,
    maximum_read_bytes: 64 << 20,
    maximum_namespace_workspace_bytes: 16 << 20,
    maximum_depth: 16,
    maximum_path_bytes: 1024,
    maximum_decoded_chunk_bytes: 2 << 20,
    sources: NativeSemanticSourceCatalogBoundsV1 {
      maximum_depth: 8,
      maximum_work: 4096,
      maximum_read_bytes: 64 << 20,
      maximum_source_bytes: 1 << 20,
      maximum_chunk_entity_bytes: 2 << 20,
      maximum_source_chunks: 1024,
    },
  }
}

fn selection_request(checkpoint: NativeCapturedSemanticCheckpointRequestV1) -> NativeInitialSemanticTaskSelectionRequestV1 {
  NativeInitialSemanticTaskSelectionRequestV1 {
    checkpoint,
    holder_boot_id: [7; 16],
    publication_timestamp_ms: checkpoint.publication_timestamp_ms + 20,
    monotonic_now_ms: 10_000,
    inventory_bounds: capture_bounds(),
    graph_bounds: selection_bounds(),
    maximum_workspace_bytes: 16 << 20,
  }
}

fn selection_retirement(
  algorithm: HashAlgorithm,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
) -> RetirementJournalOwnerV1 {
  RetirementJournalOwnerV1::new_chain(
    algorithm,
    [1; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1 << 20, 30_000),
    cancellation,
    memory,
  )
  .unwrap()
}

// Literal ASMT body offsets; do not use the production task serializer as oracle.
// Initial logical creation/update time is the immutable capture time; a later
// physical publication attempt cannot change those bytes on an exact retry.
fn expected_initial_task(staged: &NativeStagedSemanticSourceUnionV1<'_>, input: NativeInitialSemanticTaskSelectionRequestV1) -> Vec<u8> {
  let source = staged.source_union();
  let header = source.captured_header();
  let (checkpoint, _) = expected_pair(source, input.checkpoint, source.requested_configuration_count());
  let mut body = vec![0; 112 + header.hash_algorithm.hash_length()];
  body[..16].copy_from_slice(&header.database_id);
  body[16..32].copy_from_slice(&input.checkpoint.task_id);
  body[32..48].copy_from_slice(&header.physical_instance_id);
  body[48..64].copy_from_slice(&input.holder_boot_id);
  body[64..72].copy_from_slice(&1u64.to_le_bytes());
  body[72..80].copy_from_slice(&header.writer_fence_epoch.to_le_bytes());
  body[80..88].copy_from_slice(&input.checkpoint.captured_at_ms.to_le_bytes());
  body[88..96].copy_from_slice(&input.checkpoint.captured_at_ms.to_le_bytes());
  body[96..98].copy_from_slice(&1u16.to_le_bytes()); // Queued; pins remain held.
  body[100..108].copy_from_slice(&1u64.to_le_bytes());
  body[112..].copy_from_slice(&digest_parts(header.hash_algorithm, &[&checkpoint]));
  envelope(b"ASMT", &body)
}

#[test]
fn native_initial_task_selection_selects_exact_derived_task_without_changing_head_or_generation() {
  with_checkpoint_fixture(|publisher, staged, memory, cancellation, _path| {
    let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
    staged.stage_initial_checkpoint(checkpoint).unwrap();
    let input = selection_request(checkpoint);
    let expected = expected_initial_task(staged, input);
    let before = publisher.observe().unwrap();
    let generation = publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationGeneration, &[1; 16], &[]).unwrap();
    let mut retirement = selection_retirement(before.selected.header.hash_algorithm, memory, cancellation);
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let receipt =
      staged.select_initial_task(input, &mut retirement).expect("a qualified staged checkpoint must acquire durable task selection");
    assert!(!receipt.idempotent);
    assert_eq!(receipt.control_sequence, 1);
    assert_eq!(receipt.selected_slot, SystemControlSlotV1::A);
    assert!(!receipt.replaced_slot);
    let selected = publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().unwrap();
    assert_eq!(selected.bytes, expected);
    assert_eq!(publisher.observe().unwrap().selected.header.head_hash, before.selected.header.head_hash);
    assert_eq!(publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationGeneration, &[1; 16], &[]).unwrap(), generation);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    let protection = publisher.acquire_staging_protection(memory, cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), memory, cancellation).unwrap();
    assert_eq!(capture.visit(|_| Ok(true)).unwrap().tasks, 1);
    let summary = capture.visit_captured_semantic_task_metadata_entries(&[2; 16], selection_bounds(), |_| Ok(())).unwrap();
    assert_eq!(summary.disposition, SemanticMutationObservationDispositionV1::CheckpointHeld);
    assert_eq!(summary.checkpoint_sequence, Some(1));
    assert!(summary.physical_reads > 0);
  });
}

#[test]
fn native_initial_task_selection_exact_retry_is_byte_stable_and_another_holder_cannot_replace_it() {
  with_checkpoint_fixture(|publisher, staged, memory, cancellation, path| {
    let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
    staged.stage_initial_checkpoint(checkpoint).unwrap();
    let input = selection_request(checkpoint);
    let mut retirement = selection_retirement(HashAlgorithm::Blake3_256, memory, cancellation);
    let first = staged.select_initial_task(input, &mut retirement).unwrap();
    let before = fs::read(path).unwrap();
    let later = NativeInitialSemanticTaskSelectionRequestV1 { publication_timestamp_ms: input.publication_timestamp_ms + 1, ..input };
    let retry = staged.select_initial_task(later, &mut retirement).unwrap();
    assert!(retry.idempotent);
    assert_eq!(retry.control_digest, first.control_digest);
    assert_eq!(fs::read(path).unwrap(), before);
    let changed = NativeInitialSemanticTaskSelectionRequestV1 { holder_boot_id: [8; 16], ..later };
    assert!(staged.select_initial_task(changed, &mut retirement).is_err());
    assert_eq!(fs::read(path).unwrap(), before);
    assert_eq!(
      publisher
        .load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16])
        .unwrap()
        .unwrap()
        .control_sequence,
      1
    );
  });
}

#[test]
fn native_initial_task_selection_missing_dependency_does_not_select_an_empty_task() {
  with_checkpoint_fixture(|publisher, staged, memory, cancellation, path| {
    let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
    let input = selection_request(checkpoint);
    let mut retirement = selection_retirement(HashAlgorithm::Blake3_256, memory, cancellation);
    let before = fs::read(path).unwrap();
    assert!(staged.select_initial_task(input, &mut retirement).is_err());
    assert_eq!(fs::read(path).unwrap(), before);
    assert!(publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().is_none());
    staged.stage_initial_checkpoint(checkpoint).unwrap();
    assert!(!staged.select_initial_task(input, &mut retirement).unwrap().idempotent);
  });
}

#[test]
fn native_initial_task_selection_invalid_holder_or_bounded_read_refusal_preserves_unselected_bytes() {
  with_checkpoint_fixture(|publisher, staged, memory, cancellation, path| {
    let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
    staged.stage_initial_checkpoint(checkpoint).unwrap();
    let valid = selection_request(checkpoint);
    let mut retirement = selection_retirement(HashAlgorithm::Blake3_256, memory, cancellation);
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(path).unwrap();
    for input in [
      NativeInitialSemanticTaskSelectionRequestV1 { holder_boot_id: [0; 16], ..valid },
      NativeInitialSemanticTaskSelectionRequestV1 { maximum_workspace_bytes: 1, ..valid },
      NativeInitialSemanticTaskSelectionRequestV1 { publication_timestamp_ms: 0, ..valid },
      NativeInitialSemanticTaskSelectionRequestV1 { publication_timestamp_ms: i64::MAX as u64 + 1, ..valid },
      NativeInitialSemanticTaskSelectionRequestV1 { publication_timestamp_ms: checkpoint.captured_at_ms as u64 - 1, ..valid },
      NativeInitialSemanticTaskSelectionRequestV1 { monotonic_now_ms: 0, ..valid },
      NativeInitialSemanticTaskSelectionRequestV1 {
        inventory_bounds: NativeSemanticMutationInventoryBoundsV1 { maximum_work: 0, ..capture_bounds() },
        ..valid
      },
      NativeInitialSemanticTaskSelectionRequestV1 {
        inventory_bounds: NativeSemanticMutationInventoryBoundsV1 { maximum_read_bytes: 1, ..capture_bounds() },
        ..valid
      },
      NativeInitialSemanticTaskSelectionRequestV1 {
        graph_bounds: NativeSemanticTaskGraphBoundsV1 { maximum_work: 1, ..selection_bounds() },
        ..valid
      },
      NativeInitialSemanticTaskSelectionRequestV1 {
        graph_bounds: NativeSemanticTaskGraphBoundsV1 { maximum_read_bytes: 1, ..selection_bounds() },
        ..valid
      },
    ] {
      assert!(staged.select_initial_task(input, &mut retirement).is_err());
      assert_eq!(fs::read(path).unwrap(), before);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert!(publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().is_none());
    }
    assert!(!staged.select_initial_task(valid, &mut retirement).unwrap().idempotent);
  });
}

#[test]
fn native_initial_task_selection_survives_reopen_and_retains_checkpoint_edges_for_all_hashes() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("initial-task-selection-reopen", None, [1; 16], algorithm, 0);
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
      let input = selection_request(checkpoint);
      expected = expected_initial_task(&staged, input);
      let mut retirement = selection_retirement(algorithm, &memory, &cancellation);
      staged.select_initial_task(input, &mut retirement).unwrap();
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    drop(publisher);
    let (_coordinator, publisher) = reopen(&path);
    let selected = publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().unwrap();
    assert_eq!(selected.bytes, expected);
    let before = fs::read(&path).unwrap();
    {
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      // Discovery and graph scratch overlap. Match the existing small-fixture
      // retention cap while keeping the 64MiB soft / 96MiB hard memory policy.
      let bounds = NativeSemanticMutationInventoryBoundsV1 { maximum_entity_bytes: 256 << 10, ..capture_bounds() };
      let capture = protection.capture_semantic_mutation_inventory(bounds, &memory, &cancellation).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let mut seen = std::collections::BTreeSet::new();
      let retained = capture
        .visit_captured_semantic_task_retention_entries(
          NativeSemanticTaskRetentionBoundsV1 { maximum_work: 16384, maximum_read_bytes: 64 << 20, graphs: selection_bounds() },
          |entry| {
            seen.insert(entry.hash.clone());
            Ok(())
          },
        )
        .unwrap();
      assert!(retained.complete);
      assert_eq!(retained.tasks, 1);
      assert!(seen.contains(&root));
      assert!(seen.contains(&initial.namespace_tree.root_hash));
      for kind in [SystemControlKindV1::SemanticMutationCheckpoint, SystemControlKindV1::SemanticSourceCapture] {
        let control_path = system_control_path(kind, &checkpoint_identity(), SystemControlSlotV1::Immutable).unwrap();
        assert!(seen.contains(&first_authority_file_path_hash(&control_path, algorithm)));
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_initial_task_selection_committed_error_and_late_cancellation_preserve_selected_receipts() {
  for cancel in [false, true] {
    with_checkpoint_fixture(|publisher, staged, memory, cancellation, _path| {
      let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
      staged.stage_initial_checkpoint(checkpoint).unwrap();
      let input = selection_request(checkpoint);
      let expected = expected_initial_task(staged, input);
      let mut retirement = selection_retirement(HashAlgorithm::Blake3_256, memory, cancellation);
      let mut failure = FailingPostCommitObserver;
      let mut late_cancel = CancelRetirementAfterCommitObserver { cancellation: cancellation.clone() };
      let observer: &mut dyn FirstAuthorityDependencyObserverV1 = if cancel { &mut late_cancel } else { &mut failure };
      let result = staged.select_initial_task_observed(input, &mut retirement, || {}, observer);
      let receipt = if cancel {
        assert!(cancellation.is_cancelled());
        result.unwrap()
      } else {
        let error = result.unwrap_err();
        assert_eq!(error.code(), "mutable_control_committed_postcondition_failure");
        let receipt = error.committed_receipt().unwrap().clone();
        assert!(staged.select_initial_task(input, &mut retirement).unwrap().idempotent);
        receipt
      };
      assert!(!receipt.idempotent);
      assert_eq!(receipt.control_sequence, 1);
      let selected = publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().unwrap();
      assert_eq!(selected.bytes, expected);
      assert_eq!(publisher.observe().unwrap().selected.header.head_hash, staged.source_union().base_authority().root_hash);
    });
  }
}

#[test]
fn native_initial_task_selection_precommit_failure_never_selects_task_authority() {
  for case in 0..4 {
    with_checkpoint_fixture(|publisher, staged, memory, cancellation, _path| {
      let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
      staged.stage_initial_checkpoint(checkpoint).unwrap();
      let input = selection_request(checkpoint);
      let mut retirement = selection_retirement(HashAlgorithm::Blake3_256, memory, cancellation);
      let mut visibility = FailingVisibilityObserver;
      let mut dependency = FailingDependencyObserver {
        phase: match case {
          0 => DependencyFailurePhase::BeforeEntity,
          1 => DependencyFailurePhase::EntityWritten,
          _ => DependencyFailurePhase::EntityStaged,
        },
        entity_index: 0,
      };
      let observer: &mut dyn FirstAuthorityDependencyObserverV1 = if case == 3 { &mut visibility } else { &mut dependency };
      let error = staged.select_initial_task_observed(input, &mut retirement, || {}, observer).unwrap_err();
      assert_eq!(error.code(), "durability_failure");
      assert!(error.committed_receipt().is_none());
      assert!(publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().is_none());
      assert_eq!(publisher.observe().unwrap().selected.header.head_hash, staged.source_union().base_authority().root_hash);
    });
  }
}

#[test]
fn native_initial_task_selection_exact_retry_cannot_bypass_current_physical_owner_or_writer_fence() {
  for physical in [false, true] {
    with_checkpoint_fixture(|publisher, staged, memory, cancellation, path| {
      let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
      staged.stage_initial_checkpoint(checkpoint).unwrap();
      let input = selection_request(checkpoint);
      let mut retirement = selection_retirement(HashAlgorithm::Blake3_256, memory, cancellation);
      staged.select_initial_task(input, &mut retirement).unwrap();
      let mut header = publisher.observe().unwrap().selected.header;
      header.slot_sequence += 1;
      if physical {
        header.physical_instance_id = [9; 16];
      } else {
        header.writer_fence_epoch += 1;
      }
      write_redundant_header(publisher, &header);
      let before = fs::read(path).unwrap();
      let error = staged.select_initial_task(input, &mut retirement).unwrap_err();
      assert!(error.committed_receipt().is_none());
      assert_eq!(fs::read(path).unwrap(), before);
    });
  }
}

#[test]
fn native_initial_task_selection_final_admission_checks_cancel_before_new_selection_and_exact_retry() {
  for retry in [false, true] {
    with_checkpoint_fixture(|publisher, staged, memory, cancellation, path| {
      let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
      staged.stage_initial_checkpoint(checkpoint).unwrap();
      let input = selection_request(checkpoint);
      let mut retirement = selection_retirement(HashAlgorithm::Blake3_256, memory, cancellation);
      if retry {
        staged.select_initial_task(input, &mut retirement).unwrap();
      }
      let before = fs::read(path).unwrap();
      let selected = publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap();
      let error = staged
        .select_initial_task_observed(input, &mut retirement, || cancellation.cancel(), &mut NoopFirstAuthorityDependencyObserverV1)
        .unwrap_err();
      assert_eq!(error.code(), "semantic_task_observation_cancelled");
      assert!(error.committed_receipt().is_none());
      assert_eq!(publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap(), selected);
      assert_eq!(fs::read(path).unwrap(), before);
    });
  }
}

#[test]
fn native_initial_task_selection_generation_changes_cannot_hide_behind_equal_bodies_or_exact_retry() {
  for retry in [false, true] {
    for advances in [1u64, 2] {
      with_checkpoint_fixture(|publisher, staged, memory, cancellation, path| {
        let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
        staged.stage_initial_checkpoint(checkpoint).unwrap();
        let input = selection_request(checkpoint);
        let mut retirement = selection_retirement(HashAlgorithm::Blake3_256, memory, cancellation);
        if retry {
          staged.select_initial_task(input, &mut retirement).unwrap();
        }
        let generation =
          publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationGeneration, &[1; 16], &[]).unwrap().unwrap();
        for advance in 1..=advances {
          let mut bytes = generation.bytes.clone();
          bytes[16..24].copy_from_slice(&(generation.control_sequence + advance).to_le_bytes());
          crc(&mut bytes);
          // ASMG's body is deliberately unchanged: its envelope sequence must
          // detect even semantic-equivalent rewrites and change-and-back.
          let slot = if advance == 1 { SystemControlSlotV1::B } else { SystemControlSlotV1::A };
          seed(publisher, &[(SystemControlKindV1::SemanticMutationGeneration, &[], slot, &bytes)]);
        }
        let before = fs::read(path).unwrap();
        let selected = publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap();
        let error = staged.select_initial_task(input, &mut retirement).unwrap_err();
        assert!(error.committed_receipt().is_none());
        assert_eq!(publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap(), selected);
        assert_eq!(fs::read(path).unwrap(), before);
      });
    }
  }
}

#[test]
fn native_initial_task_selection_allows_prior_ordinary_head_advance_but_refuses_a_late_frontier_change() {
  for late in [false, true] {
    with_checkpoint_fixture(|publisher, staged, memory, cancellation, path| {
      let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
      staged.stage_initial_checkpoint(checkpoint).unwrap();
      let input = selection_request(checkpoint);
      let mut retirement = selection_retirement(HashAlgorithm::Blake3_256, memory, cancellation);
      let advance = || {
        let mut next = successor_request(publisher, 0x95, "ordinary-after-capture");
        next.semantic_state = request_for_database_and_algorithm([1; 16], HashAlgorithm::Blake3_256).semantic_state;
        publisher.publish_successor_authority(&next).unwrap();
      };
      if !late {
        advance();
      }
      let mut after_other_publication = None;
      let result = staged.select_initial_task_observed(
        input,
        &mut retirement,
        || {
          assert!(publisher.root_state.try_lock().is_ok());
          assert!(publisher.kv.try_lock().is_ok());
          if late {
            advance();
          }
          after_other_publication = Some(fs::read(path).unwrap());
        },
        &mut NoopFirstAuthorityDependencyObserverV1,
      );
      if late {
        assert!(result.unwrap_err().committed_receipt().is_none());
        assert_eq!(fs::read(path).unwrap(), after_other_publication.unwrap());
        assert!(publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().is_none());
        assert!(!staged.select_initial_task(input, &mut retirement).unwrap().idempotent);
      } else {
        assert!(!result.unwrap().idempotent);
      }
      assert_ne!(publisher.observe().unwrap().selected.header.head_hash, staged.source_union().base_authority().root_hash);
    });
  }
}

#[test]
fn native_initial_task_selection_final_pressure_preserves_new_and_retry_state_then_releases_reservations() {
  use crate::engine::memory_coordinator::HostMemorySample;
  for retry in [false, true] {
    with_checkpoint_fixture(|publisher, staged, memory, cancellation, path| {
      let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
      staged.stage_initial_checkpoint(checkpoint).unwrap();
      let input = selection_request(checkpoint);
      let mut retirement = selection_retirement(HashAlgorithm::Blake3_256, memory, cancellation);
      if retry {
        staged.select_initial_task(input, &mut retirement).unwrap();
      }
      let before = fs::read(path).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let error = staged
        .select_initial_task_observed(
          input,
          &mut retirement,
          || memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap(),
          &mut NoopFirstAuthorityDependencyObserverV1,
        )
        .unwrap_err();
      assert_eq!(error.code(), "semantic_task_observation_memory");
      assert!(error.committed_receipt().is_none());
      memory.update_host_sample(HostMemorySample::default()).unwrap();
      assert_eq!(fs::read(path).unwrap(), before);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(staged.select_initial_task(input, &mut retirement).unwrap().idempotent, retry);
      assert!(publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().is_some());
    });
  }
}

#[test]
fn native_initial_task_selection_derived_pair_mismatch_is_not_repaired_or_selected_implicitly() {
  with_checkpoint_fixture(|publisher, staged, memory, cancellation, path| {
    let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
    staged.stage_initial_checkpoint(checkpoint).unwrap();
    let input = selection_request(checkpoint);
    let mut retirement = selection_retirement(HashAlgorithm::Blake3_256, memory, cancellation);
    let before = fs::read(path).unwrap();
    for changed in [
      NativeCapturedSemanticCheckpointRequestV1 { mutation_count: checkpoint.mutation_count + 1, ..checkpoint },
      NativeCapturedSemanticCheckpointRequestV1 { captured_at_ms: checkpoint.captured_at_ms + 1, ..checkpoint },
    ] {
      let error = staged
        .select_initial_task(NativeInitialSemanticTaskSelectionRequestV1 { checkpoint: changed, ..input }, &mut retirement)
        .unwrap_err();
      assert!(error.committed_receipt().is_none());
      assert_eq!(fs::read(path).unwrap(), before);
      assert!(publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().is_none());
    }
    assert!(!staged.select_initial_task(input, &mut retirement).unwrap().idempotent);
  });
}

#[test]
fn native_initial_task_selection_final_generation_change_invalidates_new_selection_and_retry() {
  for retry in [false, true] {
    with_checkpoint_fixture(|publisher, staged, memory, cancellation, path| {
      let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
      staged.stage_initial_checkpoint(checkpoint).unwrap();
      let input = selection_request(checkpoint);
      let mut retirement = selection_retirement(HashAlgorithm::Blake3_256, memory, cancellation);
      if retry {
        staged.select_initial_task(input, &mut retirement).unwrap();
      }
      let selected = publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap();
      let generation =
        publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationGeneration, &[1; 16], &[]).unwrap().unwrap();
      let mut bytes = generation.bytes;
      bytes[16..24].copy_from_slice(&(generation.control_sequence + 1).to_le_bytes());
      crc(&mut bytes);
      let mut after_generation = None;
      let error = staged
        .select_initial_task_observed(
          input,
          &mut retirement,
          || {
            seed(publisher, &[(SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::B, &bytes)]);
            after_generation = Some(fs::read(path).unwrap());
          },
          &mut NoopFirstAuthorityDependencyObserverV1,
        )
        .unwrap_err();
      assert!(error.committed_receipt().is_none());
      assert_eq!(fs::read(path).unwrap(), after_generation.unwrap());
      assert_eq!(publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap(), selected);
    });
  }
}

#[test]
fn native_initial_task_selection_cannot_reset_an_advanced_or_released_task() {
  for released in [false, true] {
    with_checkpoint_fixture(|publisher, staged, memory, cancellation, path| {
      let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
      staged.stage_initial_checkpoint(checkpoint).unwrap();
      let input = selection_request(checkpoint);
      let mut retirement = selection_retirement(HashAlgorithm::Blake3_256, memory, cancellation);
      staged.select_initial_task(input, &mut retirement).unwrap();
      let mut next = expected_initial_task(staged, input);
      next[16..24].copy_from_slice(&2u64.to_le_bytes());
      if released {
        next[32 + 96..32 + 98].copy_from_slice(&8u16.to_le_bytes());
        next[32 + 98..32 + 100].copy_from_slice(&1u16.to_le_bytes());
      }
      crc(&mut next);
      seed(publisher, &[(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::B, &next)]);
      let before = fs::read(path).unwrap();
      let error = staged.select_initial_task(input, &mut retirement).unwrap_err();
      assert!(error.committed_receipt().is_none());
      assert_eq!(fs::read(path).unwrap(), before);
      assert_eq!(
        publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().unwrap().bytes,
        next
      );
    });
  }
}

#[test]
fn native_initial_task_selection_missing_base_or_companion_cannot_select_partial_retention() {
  for companion in [false, true] {
    with_checkpoint_fixture(|publisher, staged, memory, cancellation, path| {
      let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
      staged.stage_initial_checkpoint(checkpoint).unwrap();
      let key = if companion {
        first_authority_file_path_hash(
          &system_control_path(SystemControlKindV1::SemanticSourceCapture, &checkpoint_identity(), SystemControlSlotV1::Immutable).unwrap(),
          HashAlgorithm::Blake3_256,
        )
      } else {
        staged.source_union().base_authority().root_hash.clone()
      };
      assert!(publisher.lock_kv().unwrap().mark_deleted(&key).unwrap());
      seed_files(publisher, &[]);
      let before = fs::read(path).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let mut retirement = selection_retirement(HashAlgorithm::Blake3_256, memory, cancellation);
      let error = staged.select_initial_task(selection_request(checkpoint), &mut retirement).unwrap_err();
      assert!(error.committed_receipt().is_none());
      drop(retirement);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read(path).unwrap(), before);
      assert!(publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().is_none());
    });
  }
}

#[test]
fn native_initial_task_selection_actual_task_output_allocation_refuses_without_writes_and_retries() {
  with_checkpoint_fixture(|publisher, staged, memory, cancellation, path| {
    let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
    staged.stage_initial_checkpoint(checkpoint).unwrap();
    let input = selection_request(checkpoint);
    let mut retirement = selection_retirement(HashAlgorithm::Blake3_256, memory, cancellation);
    let before = fs::read(path).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    assert!(!cancellation.is_cancelled());
    let length = 36 + 112 + staged.source_union().captured_header().hash_algorithm.hash_length();
    let (result, allocations) = measure(length, || staged.select_initial_task(input, &mut retirement));
    assert!(allocations.injected_failure, "{allocations:?}");
    let error = result.unwrap_err();
    assert_eq!(error.code(), "system_control_output_allocation");
    assert!(error.committed_receipt().is_none());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(path).unwrap(), before);
    assert!(publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().is_none());
    assert!(!staged.select_initial_task(input, &mut retirement).unwrap().idempotent);
  });
}

#[test]
fn native_initial_task_selection_current_capabilities_are_required_before_new_selection_and_retry() {
  for retry in [false, true] {
    for reader in [false, true] {
      with_checkpoint_fixture(|publisher, staged, memory, cancellation, path| {
        let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
        staged.stage_initial_checkpoint(checkpoint).unwrap();
        let input = selection_request(checkpoint);
        let mut retirement = selection_retirement(HashAlgorithm::Blake3_256, memory, cancellation);
        if retry {
          staged.select_initial_task(input, &mut retirement).unwrap();
        }
        let mut header = publisher.observe().unwrap().selected.header;
        if reader {
          header.required_reader_capabilities[3] &= !0b0010;
        } else {
          header.required_writer_capabilities[3] &= !0b1000;
        }
        write_redundant_header(publisher, &header);
        let before = fs::read(path).unwrap();
        let error = staged.select_initial_task(input, &mut retirement).unwrap_err();
        assert!(error.committed_receipt().is_none());
        assert_eq!(fs::read(path).unwrap(), before);
      });
    }
  }
}

#[test]
fn native_initial_task_selection_reopen_distinguishes_precommit_failure_and_committed_task() {
  for committed in [false, true] {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("initial-task-selection-recovery", None, [1; 16], algorithm, 0);
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
      let input = selection_request(checkpoint);
      expected = expected_initial_task(&staged, input);
      let mut retirement = selection_retirement(algorithm, &memory, &cancellation);
      let mut before_commit = FailingVisibilityObserver;
      let mut after_commit = FailingPostCommitObserver;
      let observer: &mut dyn FirstAuthorityDependencyObserverV1 = if committed { &mut after_commit } else { &mut before_commit };
      let error = staged.select_initial_task_observed(input, &mut retirement, || {}, observer).unwrap_err();
      assert_eq!(error.committed_receipt().is_some(), committed);
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    drop(publisher);
    let (_coordinator, publisher) = reopen(&path);
    let before = fs::read(&path).unwrap();
    let selected = publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap();
    if committed {
      assert_eq!(selected.unwrap().bytes, expected);
    } else {
      assert!(selected.is_none());
    }
    assert_eq!(publisher.observe().unwrap().selected.header.head_hash, root);
    {
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let observed = capture.visit(|_| Ok(true)).unwrap();
      assert!(observed.complete);
      assert_eq!(observed.tasks, u64::from(committed));
      let graph = capture.visit_captured_semantic_task_metadata_entries(&[2; 16], selection_bounds(), |_| Ok(())).unwrap();
      assert_eq!(
        graph.disposition,
        if committed { SemanticMutationObservationDispositionV1::CheckpointHeld } else { SemanticMutationObservationDispositionV1::Absent }
      );
      // The dependencies preceded either attempted selector and remain readable;
      // their presence alone must never manufacture task authority after reopen.
      capture.visit_captured_semantic_checkpoint_metadata_entries(&[2; 16], 1, selection_bounds(), |_| Ok(())).unwrap();
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
