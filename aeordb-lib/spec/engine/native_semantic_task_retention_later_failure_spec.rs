//! A later missing dependency invalidates all earlier provisional callbacks.
use super::*;

#[test]
fn native_semantic_task_retention_later_task_failure_never_yields_partial_completion() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("retention-later-failure", None, [1; 16], algorithm, 0);
  let (mut expected, _, _) = seed_captured_graph(&publisher);
  seed_second_task(&publisher, &mut expected);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let original = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let mut order: Vec<[u8; 16]> = Vec::new();
  original
    .visit_metadata(|observation| {
      order.push(observation.task().unwrap().unwrap().task_id.try_into().unwrap());
      Ok(true)
    })
    .unwrap();
  assert_eq!(order.len(), 2);
  let mut first_graph = PhysicalSet::new();
  original
    .visit_captured_semantic_task_metadata_entries(&order[0], graph_bounds(), |entry| {
      first_graph.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
      Ok(())
    })
    .unwrap();
  let mut identity = checkpoint_identity();
  identity[..16].copy_from_slice(&order[1]);
  let key = first_authority_file_path_hash(
    &system_control_path(SystemControlKindV1::SemanticSourceCapture, &identity, SystemControlSlotV1::Immutable).unwrap(),
    algorithm,
  );
  let missing = publisher.locator(&key).unwrap().unwrap();
  assert!(publisher.lock_kv().unwrap().mark_deleted(&key).unwrap());
  seed_files(&publisher, &[]);
  let damaged = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let mut actual = PhysicalSet::new();
  let error = damaged
    .visit_captured_semantic_task_retention_entries(retention_bounds(), |entry| {
      actual.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
      Ok(())
    })
    .unwrap_err();
  assert_eq!(error.code(), "semantic_source_catalog_capture_missing");
  for (key, locator) in first_graph {
    assert_eq!(actual.get(&key), Some(&locator), "first complete graph must precede failure");
  }
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
  publisher.lock_kv().unwrap().insert(missing).unwrap();
  seed_files(&publisher, &[]);
  let restored = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let mut actual = PhysicalSet::new();
  assert!(
    restored
      .visit_captured_semantic_task_retention_entries(retention_bounds(), |entry| {
        actual.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
        Ok(())
      })
      .unwrap()
      .complete
  );
  assert_eq!(actual, expected);
  assert_eq!(
    damaged.visit_captured_semantic_task_retention_entries(retention_bounds(), |_| Ok(())).unwrap_err().code(),
    "semantic_source_catalog_capture_missing"
  );
}
