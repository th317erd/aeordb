//! Failing-first native composition targets; provisional graph references only.
#[path = "native_semantic_task_retention_boundary_spec.rs"]
mod boundary;
#[path = "native_semantic_task_mark_characterization_spec.rs"]
mod mark_characterization;
#[path = "native_semantic_task_mark_spec.rs"]
mod task_mark;
use super::*;
use super::retention_characterization::seed_second_task;

fn retention_bounds() -> NativeSemanticTaskRetentionBoundsV1 {
  NativeSemanticTaskRetentionBoundsV1 { maximum_work: 100_000, maximum_read_bytes: 64 << 20, graphs: graph_bounds() }
}

fn retention_capture_bounds() -> NativeSemanticMutationInventoryBoundsV1 {
  // Small fixtures need no 4MiB entity scratch; concurrent discovery and graph
  // work must stay under the unchanged 64MiB soft / 96MiB hard policy.
  NativeSemanticMutationInventoryBoundsV1 { maximum_entity_bytes: 256 << 10, ..capture_bounds() }
}

#[test]
fn native_semantic_task_retention_streams_two_selected_task_graphs_from_one_capture() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("task-retention-two", None, [1; 16], algorithm, 0);
    let (mut expected, _, _) = seed_captured_graph(&publisher);
    seed_second_task(&publisher, &mut expected);
    assert_eq!(expected.len(), 26);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    // Prove the independently constructed second binding passes the existing
    // single-task owner before asking the new composed entry to discover it.
    let mut graph_read_bytes = 0;
    for task_id in [[2; 16], [3; 16]] {
      let graph = capture.visit_captured_semantic_task_metadata_entries(&task_id, graph_bounds(), |_| Ok(())).unwrap();
      assert_eq!(graph.checkpoint_sequence, Some(1));
      graph_read_bytes += graph.read_bytes;
    }
    // The already-qualified discovery entry independently identifies its exact
    // read threshold. The new composition must account for discovery PLUS both
    // existing graph operations, not just report internally consistent counters.
    let mut low = 1;
    let mut high = 64 << 20;
    while low < high {
      let middle = low + (high - low) / 2;
      let discovery = protection
        .capture_semantic_mutation_inventory(
          NativeSemanticMutationInventoryBoundsV1 { maximum_read_bytes: middle, ..retention_capture_bounds() },
          &memory,
          &cancellation,
        )
        .unwrap();
      match discovery.visit_metadata(|_| Ok(true)) {
        Ok(summary) => {
          assert_eq!(summary.tasks, 2);
          assert!(summary.complete);
          high = middle;
        }
        Err(error) => {
          assert_eq!(error.code(), "semantic_task_inventory_read_bound");
          low = middle + 1;
        }
      }
    }
    let mut actual = PhysicalSet::new();
    let summary = capture
      .visit_captured_semantic_task_retention_entries(retention_bounds(), |entry| {
        actual.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
        Ok(())
      })
      .expect("discover both selected graphs under one captured operation");
    assert_eq!(actual, expected);
    assert_eq!(summary.tasks, 2);
    assert!(summary.complete);
    assert!(summary.work > 0 && summary.read_bytes > 0);
    assert_eq!(summary.read_bytes, low + graph_read_bytes);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_task_retention_uses_one_exact_budget_across_discovery_and_tasks() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("task-retention-quota", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  let (mut expected, _, _) = seed_captured_graph(&publisher);
  seed_second_task(&publisher, &mut expected);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let observed = capture.visit_captured_semantic_task_retention_entries(retention_bounds(), |_| Ok(())).unwrap();
  for (work, bytes, succeeds) in [
    (observed.work, observed.read_bytes, true),
    (observed.work - 1, observed.read_bytes, false),
    (observed.work, observed.read_bytes - 1, false),
  ] {
    let mut callbacks = 0;
    let result = capture.visit_captured_semantic_task_retention_entries(
      NativeSemanticTaskRetentionBoundsV1 { maximum_work: work, maximum_read_bytes: bytes, ..retention_bounds() },
      |_| {
        callbacks += 1;
        Ok(())
      },
    );
    if succeeds {
      assert_eq!(result.unwrap(), observed);
    } else {
      assert!(result.is_err(), "a per-task reset must not turn an insufficient combined quota into success");
      assert!(callbacks > 0);
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
