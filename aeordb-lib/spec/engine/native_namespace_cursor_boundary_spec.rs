use super::*;

#[test]
fn native_namespace_cursor_constructor_allocation_refusal_releases_then_retries() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("namespace-cursor-allocation", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    let root = publish_namespace_directory(&publisher, vec![]);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(&path).unwrap();
    let (result, allocations) =
      allocation_probe::measure(algorithm.hash_length(), || capture.open_namespace_configuration_cursor(namespace_source_request(&root)));
    assert!(allocations.injected_failure, "{allocations:?}");
    let error = result.err().expect("root identity allocation must refuse");
    assert_eq!(error.code(), "semantic_namespace_source_allocation");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    let mut retry = capture.open_namespace_configuration_cursor(namespace_source_request(&root)).unwrap();
    assert!(retry.next_source().unwrap().is_none());
    drop(retry);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_namespace_cursor_keeps_one_physical_read_budget_across_partial_results() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("namespace-cursor-read-budget", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], HashAlgorithm::Blake3_256)).unwrap();
  let body = vec![b'x'; 997];
  let mut children = Vec::new();
  for owner in ["a", "b", "c"] {
    let (directory, _) = namespace_configuration_tree(&publisher, &format!("/{owner}"), &body, vec![]);
    children.push(namespace_directory_child(owner, directory));
  }
  let root = publish_namespace_directory(&publisher, children);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let mut partial_refusal = false;
  let mut complete = false;
  for budget in (1..=65536).step_by(256) {
    let mut request = namespace_source_request(&root);
    request.bounds.sources.maximum_read_bytes = budget;
    let mut cursor = capture.open_namespace_configuration_cursor(request).unwrap();
    let mut paths = Vec::new();
    loop {
      match cursor.next_source() {
        Ok(Some(source)) => paths.push(source.record().path.clone()),
        Ok(None) => {
          complete = true;
          break;
        }
        Err(error) => {
          assert_eq!(error.code(), "semantic_source_read_bound");
          assert!(matches!(error, NativeSemanticNamespaceSourceErrorV1::Source(SemanticMutationObservationErrorV1::ResourceRead { .. })));
          partial_refusal |= !paths.is_empty();
          assert!(cursor.next_source().is_err());
          break;
        }
      }
    }
    drop(cursor);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    if complete {
      assert_eq!(paths, ["/a/.aeordb-config/indexes.json", "/b/.aeordb-config/indexes.json", "/c/.aeordb-config/indexes.json"]);
      break;
    }
  }
  assert!(complete && partial_refusal);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_namespace_cursor_partial_body_allocation_failure_is_terminal() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("namespace-cursor-body-allocation", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], HashAlgorithm::Blake3_256)).unwrap();
  let body = vec![b'x'; 997];
  let (first, _) = namespace_configuration_tree(&publisher, "/a", b"first", vec![]);
  let (second, _) = namespace_configuration_tree(&publisher, "/b", &body, vec![]);
  let root = publish_namespace_directory(&publisher, vec![namespace_directory_child("a", first), namespace_directory_child("b", second)]);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let mut cursor = capture.open_namespace_configuration_cursor(namespace_source_request(&root)).unwrap();
  let first = cursor.next_source().unwrap().unwrap();
  let (result, allocations) = allocation_probe::measure(body.len(), || cursor.next_source());
  assert!(allocations.injected_failure, "{allocations:?}");
  assert_eq!(result.err().expect("body allocation must refuse").code(), "semantic_source_body_allocation");
  assert!(cursor.next_source().is_err());
  assert_eq!(first.body(), b"first");
  drop(cursor);
  drop(first);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  let mut retry = capture.open_namespace_configuration_cursor(namespace_source_request(&root)).unwrap();
  assert_eq!(retry.next_source().unwrap().unwrap().body(), b"first");
  assert_eq!(retry.next_source().unwrap().unwrap().body(), body);
  assert!(retry.next_source().unwrap().is_none());
  drop(retry);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_namespace_cursor_keeps_one_work_budget_across_partial_results() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("namespace-cursor-work", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], HashAlgorithm::Blake3_256)).unwrap();
  let mut children = Vec::new();
  for owner in ["a", "b", "c"] {
    let (directory, _) = namespace_configuration_tree(&publisher, &format!("/{owner}"), b"body", vec![]);
    children.push(namespace_directory_child(owner, directory));
  }
  let root = publish_namespace_directory(&publisher, children);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let mut partial_refusal = false;
  let mut complete = false;
  for budget in 1..=256 {
    let mut request = namespace_source_request(&root);
    request.bounds.maximum_work = budget;
    let mut cursor = capture.open_namespace_configuration_cursor(request).unwrap();
    let mut paths = Vec::new();
    loop {
      match cursor.next_source() {
        Ok(Some(source)) => paths.push(source.record().path.clone()),
        Ok(None) => {
          complete = true;
          break;
        }
        Err(error) => {
          assert_eq!(error.code(), "semantic_namespace_source_work_bound");
          assert!(matches!(error, NativeSemanticNamespaceSourceErrorV1::Source(SemanticMutationObservationErrorV1::ResourceRead { .. })));
          partial_refusal |= !paths.is_empty();
          assert!(cursor.next_source().is_err());
          break;
        }
      }
    }
    drop(cursor);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    if complete {
      assert_eq!(paths, ["/a/.aeordb-config/indexes.json", "/b/.aeordb-config/indexes.json", "/c/.aeordb-config/indexes.json"]);
      break;
    }
  }
  assert!(complete && partial_refusal);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_namespace_cursor_stays_on_its_old_capture_after_publication_between_rows() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("namespace-cursor-old", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    let (first, _) = namespace_configuration_tree(&publisher, "/a", b"old-a", vec![]);
    let (second, old_revision) = namespace_configuration_tree(&publisher, "/b", b"old-b", vec![]);
    let root = publish_namespace_directory(&publisher, vec![namespace_directory_child("a", first), namespace_directory_child("b", second)]);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let mut cursor = capture.open_namespace_configuration_cursor(namespace_source_request(&root)).unwrap();
    assert_eq!(cursor.next_source().unwrap().unwrap().body(), b"old-a");
    let (new_directory, new_revision) = namespace_configuration_tree(&publisher, "/b", b"new-b", vec![]);
    let new_root = publish_namespace_directory(&publisher, vec![namespace_directory_child("b", new_directory)]);
    let before = fs::read(&path).unwrap();
    let old = cursor.next_source().unwrap().unwrap();
    assert_eq!(old.body(), b"old-b");
    assert_eq!(old.revision(), old_revision);
    assert!(cursor.next_source().unwrap().is_none());
    drop(old);
    drop(cursor);
    let mut stale = capture.open_namespace_configuration_cursor(namespace_source_request(&new_root)).unwrap();
    assert!(stale.next_source().is_err());
    assert!(stale.next_source().is_err());
    drop(stale);
    let fresh = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let mut current = fresh.open_namespace_configuration_cursor(namespace_source_request(&new_root)).unwrap();
    let new = current.next_source().unwrap().unwrap();
    assert_eq!(new.revision(), new_revision);
    assert_eq!(new.body(), b"new-b");
    assert!(current.next_source().unwrap().is_none());
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
