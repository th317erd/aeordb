//! Independent local-operation thresholds prove combined accounting.
use super::*;

#[test]
fn native_semantic_task_retention_counts_inventory_graph_and_source_work_once_per_category() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("retention-work-oracle", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  let (mut expected, _, _) = seed_captured_graph(&publisher);
  seed_second_task(&publisher, &mut expected);
  // Include a raw deleted entry in discovery work. The separate KV admission
  // regressions also cover frozen buffered overrides and deleted buffer rows.
  seed_files(&publisher, &[("/discarded".to_string(), "application/octet-stream", b"discarded")]);
  let discarded_key = first_authority_file_path_hash("/discarded", HashAlgorithm::Blake3_256);
  assert!(publisher.lock_kv().unwrap().mark_deleted(&discarded_key).unwrap());
  seed_files(&publisher, &[]);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let mut low = 1;
  let mut high = retention_capture_bounds().maximum_work;
  while low < high {
    let middle = low + (high - low) / 2;
    let local = protection
      .capture_semantic_mutation_inventory(
        NativeSemanticMutationInventoryBoundsV1 { maximum_work: middle, ..retention_capture_bounds() },
        &memory,
        &cancellation,
      )
      .unwrap();
    match local.visit_metadata(|_| Ok(true)) {
      Ok(summary) => {
        assert!(summary.complete);
        assert_eq!(summary.tasks, 2);
        high = middle;
      }
      Err(SemanticMutationObservationErrorV1::Authority(FirstAuthorityPublicationErrorV1::Engine(EngineError::ResourceExhausted(
        message,
      )))) => {
        assert_eq!(message, "captured KV entry scan work limit");
        low = middle + 1;
      }
      Err(error) => panic!("unexpected independent inventory failure: {error:?}"),
    }
  }
  let mut expected_work = low;
  for task_id in [[2; 16], [3; 16]] {
    expected_work += capture.visit_captured_semantic_task_metadata_entries(&task_id, graph_bounds(), |_| Ok(())).unwrap().work;
    let mut source_low = 1;
    let mut source_high = graph_bounds().sources.maximum_work;
    while source_low < source_high {
      let middle = source_low + (source_high - source_low) / 2;
      match capture.visit_captured_source_physical_entries(
        &task_id,
        1,
        NativeSemanticSourceCatalogBoundsV1 { maximum_work: middle, ..graph_bounds().sources },
        |_| Ok(()),
      ) {
        Ok(summary) => {
          assert!(summary.complete);
          source_high = middle;
        }
        Err(error) => {
          assert_eq!(error.code(), "semantic_source_catalog_work_bound");
          source_low = middle + 1;
        }
      }
    }
    expected_work += source_low;
  }
  let summary = capture.visit_captured_semantic_task_retention_entries(retention_bounds(), |_| Ok(())).unwrap();
  assert_eq!(summary.work, expected_work);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_semantic_task_retention_intersects_capture_limits_without_per_task_reset() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("retention-capture-limits", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  let (mut expected, _, _) = seed_captured_graph(&publisher);
  seed_second_task(&publisher, &mut expected);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let total = capture.visit_captured_semantic_task_retention_entries(retention_bounds(), |_| Ok(())).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  for read_limit in [false, true] {
    let bounds = if read_limit {
      NativeSemanticMutationInventoryBoundsV1 { maximum_read_bytes: total.read_bytes - 1, ..retention_capture_bounds() }
    } else {
      NativeSemanticMutationInventoryBoundsV1 { maximum_work: total.work - 1, ..retention_capture_bounds() }
    };
    let limited = protection.capture_semantic_mutation_inventory(bounds, &memory, &cancellation).unwrap();
    for task_id in [[2; 16], [3; 16]] {
      limited.visit_captured_semantic_task_metadata_entries(&task_id, graph_bounds(), |_| Ok(())).unwrap();
    }
    let error = limited.visit_captured_semantic_task_retention_entries(retention_bounds(), |_| Ok(())).unwrap_err();
    assert_eq!(error.code(), if read_limit { "semantic_task_retention_read_bound" } else { "semantic_task_retention_work_bound" });
  }
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_semantic_task_retention_insufficient_nested_scratch_refuses_without_leaks() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("retention-scratch", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  seed_captured_graph(&publisher);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let large = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  assert_eq!(
    large.visit_captured_semantic_task_retention_entries(retention_bounds(), |_| Ok(())).unwrap_err().code(),
    "semantic_task_observation_memory"
  );
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  let small = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  assert!(small.visit_captured_semantic_task_retention_entries(retention_bounds(), |_| Ok(())).unwrap().complete);
  drop(small);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}
