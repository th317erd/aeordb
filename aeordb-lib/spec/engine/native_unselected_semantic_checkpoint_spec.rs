//! Initial checkpoint graph validation through real immutable staging.
use super::*;

fn checkpoint_graph_bounds() -> NativeSemanticTaskGraphBoundsV1 {
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

#[test]
fn native_unselected_checkpoint_graph_checks_a_real_staged_pair_without_selecting_a_task() {
  with_checkpoint_fixture(|publisher, staged, memory, cancellation, path| {
    let input = request(staged.source_union().captured_header().updated_at_ms + 1);
    staged.stage_initial_checkpoint(input).unwrap();
    let protection = publisher.acquire_staging_protection(memory, cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), memory, cancellation).unwrap();
    assert_eq!(capture.visit(|_| panic!("immutable dependencies are not selected tasks")).unwrap().tasks, 0);
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(path).unwrap();
    let mut visits = Vec::new();
    let result = capture
      .visit_captured_semantic_checkpoint_metadata_entries(&input.task_id, 1, checkpoint_graph_bounds(), |entry| {
        visits.push(entry.hash.clone());
        Ok(())
      })
      .expect("unselected checkpoint must validate its actual graph, not an absent-task summary");
    assert_eq!(result.checkpoint_sequence, 1);
    assert!(result.physical_reads > 0);
    assert!(visits.iter().any(|key| key == &staged.source_union().base_authority().root_hash));
    assert!(visits.iter().any(|key| key == staged.source_union().requested_directory_root()));
    assert_eq!(capture.visit(|_| panic!("read-only validation cannot select a task")).unwrap().tasks, 0);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(path).unwrap(), before);
  });
}

#[test]
fn native_unselected_checkpoint_graph_missing_pair_never_becomes_empty_success() {
  with_checkpoint_fixture(|publisher, staged, memory, cancellation, path| {
    let input = request(staged.source_union().captured_header().updated_at_ms + 1);
    staged.stage_initial_checkpoint(input).unwrap();
    let protection = publisher.acquire_staging_protection(memory, cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), memory, cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(path).unwrap();
    for (task, sequence) in [([0; 16], 1), ([3; 16], 1), (input.task_id, 0), (input.task_id, 2)] {
      let result = capture.visit_captured_semantic_checkpoint_metadata_entries(&task, sequence, checkpoint_graph_bounds(), |_| Ok(()));
      assert!(result.is_err(), "unselected/missing dependency must not be reported as absent-task success");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read(path).unwrap(), before);
    }
  });
}

#[test]
fn native_unselected_checkpoint_graph_enforces_exact_cumulative_work_and_read_limits() {
  with_checkpoint_fixture(|publisher, staged, memory, cancellation, path| {
    let input = request(staged.source_union().captured_header().updated_at_ms + 1);
    staged.stage_initial_checkpoint(input).unwrap();
    let protection = publisher.acquire_staging_protection(memory, cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), memory, cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(path).unwrap();
    let observed =
      capture.visit_captured_semantic_checkpoint_metadata_entries(&input.task_id, 1, checkpoint_graph_bounds(), |_| Ok(())).unwrap();
    assert!(observed.work > 1 && observed.read_bytes > 1);
    let exact =
      NativeSemanticTaskGraphBoundsV1 { maximum_work: observed.work, maximum_read_bytes: observed.read_bytes, ..checkpoint_graph_bounds() };
    assert_eq!(capture.visit_captured_semantic_checkpoint_metadata_entries(&input.task_id, 1, exact, |_| Ok(())).unwrap(), observed);
    for bounds in [
      NativeSemanticTaskGraphBoundsV1 { maximum_work: observed.work - 1, ..exact },
      NativeSemanticTaskGraphBoundsV1 { maximum_read_bytes: observed.read_bytes - 1, ..exact },
      NativeSemanticTaskGraphBoundsV1 { maximum_work: 0, ..exact },
      NativeSemanticTaskGraphBoundsV1 { maximum_namespace_workspace_bytes: 1, ..exact },
    ] {
      assert!(capture.visit_captured_semantic_checkpoint_metadata_entries(&input.task_id, 1, bounds, |_| Ok(())).is_err());
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read(path).unwrap(), before);
    }
    assert_eq!(capture.visit_captured_semantic_checkpoint_metadata_entries(&input.task_id, 1, exact, |_| Ok(())).unwrap(), observed);
  });
}

#[test]
fn native_unselected_checkpoint_graph_preserves_original_callback_failure_and_releases_memory() {
  use crate::engine::memory_coordinator::HostMemorySample;
  for pressure in [false, true] {
    with_checkpoint_fixture(|publisher, staged, memory, cancellation, path| {
      let input = request(staged.source_union().captured_header().updated_at_ms + 1);
      staged.stage_initial_checkpoint(input).unwrap();
      let protection = publisher.acquire_staging_protection(memory, cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), memory, cancellation).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let before = fs::read(path).unwrap();
      let mut calls = 0;
      let error = capture
        .visit_captured_semantic_checkpoint_metadata_entries(&input.task_id, 1, checkpoint_graph_bounds(), |_| {
          calls += 1;
          if pressure {
            memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..Default::default() }).unwrap();
          } else {
            cancellation.cancel();
          }
          Err(SemanticMutationObservationErrorV1::Resource {
            code: "unselected_graph_original_callback",
            message: "preserve the callback's own failure over subsequent interruption",
          })
        })
        .unwrap_err();
      assert_eq!(error.code(), "unselected_graph_original_callback");
      assert_eq!(calls, 1);
      memory.update_host_sample(HostMemorySample::default()).unwrap();
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read(path).unwrap(), before);
    });
  }
}

#[test]
fn native_unselected_checkpoint_graph_final_callback_cancellation_cannot_report_success() {
  with_checkpoint_fixture(|publisher, staged, memory, cancellation, path| {
    let input = request(staged.source_union().captured_header().updated_at_ms + 1);
    staged.stage_initial_checkpoint(input).unwrap();
    let protection = publisher.acquire_staging_protection(memory, cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), memory, cancellation).unwrap();
    let total = capture
      .visit_captured_semantic_checkpoint_metadata_entries(&input.task_id, 1, checkpoint_graph_bounds(), |_| Ok(()))
      .unwrap()
      .physical_reads;
    assert!(total > 0);
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(path).unwrap();
    let mut calls = 0;
    let error = capture
      .visit_captured_semantic_checkpoint_metadata_entries(&input.task_id, 1, checkpoint_graph_bounds(), |_| {
        calls += 1;
        if calls == total {
          cancellation.cancel();
        }
        Ok(())
      })
      .unwrap_err();
    assert_eq!(calls, total);
    assert_eq!(error.code(), "semantic_task_observation_cancelled");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(path).unwrap(), before);
  });
}

#[test]
fn native_unselected_checkpoint_graph_uses_its_capture_without_current_lookup_fallback() {
  with_checkpoint_fixture(|publisher, staged, memory, cancellation, path| {
    let input = request(staged.source_union().captured_header().updated_at_ms + 1);
    let protection = publisher.acquire_staging_protection(memory, cancellation).unwrap();
    let earlier = protection.capture_semantic_mutation_inventory(capture_bounds(), memory, cancellation).unwrap();
    staged.stage_initial_checkpoint(input).unwrap();
    let before = fs::read(path).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let error =
      earlier.visit_captured_semantic_checkpoint_metadata_entries(&input.task_id, 1, checkpoint_graph_bounds(), |_| Ok(())).unwrap_err();
    assert_eq!(error.code(), "semantic_source_catalog_capture_missing");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    let fresh = protection.capture_semantic_mutation_inventory(capture_bounds(), memory, cancellation).unwrap();
    fresh.visit_captured_semantic_checkpoint_metadata_entries(&input.task_id, 1, checkpoint_graph_bounds(), |_| Ok(())).unwrap();
    assert_eq!(fresh.visit(|_| panic!("graph validation is not task selection")).unwrap().tasks, 0);
    assert_eq!(fs::read(path).unwrap(), before);
  });
}
