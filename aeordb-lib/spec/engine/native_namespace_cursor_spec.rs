//! Pausable traversal must preserve the existing captured source reader.
use super::*;

#[test]
fn native_namespace_cursor_interleaves_two_roots_and_retains_exact_source_rows() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("namespace-cursor-interleave", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    let (base_a, base_revision) = namespace_configuration_tree(&publisher, "/a", b"base-a", vec![]);
    let (base_z, _) = namespace_configuration_tree(&publisher, "/z", b"base-z", vec![]);
    let base =
      publish_namespace_directory(&publisher, vec![namespace_directory_child("a", base_a), namespace_directory_child("z", base_z)]);
    let (requested_a, requested_revision) = namespace_configuration_tree(&publisher, "/a", b"requested-a", vec![]);
    let (requested_b, _) = namespace_configuration_tree(&publisher, "/b", b"requested-b", vec![]);
    let requested = publish_namespace_directory(
      &publisher,
      vec![namespace_directory_child("a", requested_a), namespace_directory_child("b", requested_b)],
    );
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(&path).unwrap();
    let mut left = capture.open_namespace_configuration_cursor(namespace_source_request(&base)).expect("pausable namespace traversal");
    let mut right = capture.open_namespace_configuration_cursor(namespace_source_request(&requested)).unwrap();
    let old = left.next_source().unwrap().unwrap();
    let new = right.next_source().unwrap().unwrap();
    assert_eq!(old.revision(), base_revision);
    assert_eq!(new.revision(), requested_revision);
    assert_eq!(old.body(), b"base-a");
    assert_eq!(new.body(), b"requested-a");
    assert!(publisher.root_state.try_lock().is_ok());
    assert!(publisher.kv.try_lock().is_ok());
    assert_eq!(right.next_source().unwrap().unwrap().record().path, "/b/.aeordb-config/indexes.json");
    assert_eq!(left.next_source().unwrap().unwrap().record().path, "/z/.aeordb-config/indexes.json");
    assert!(left.next_source().unwrap().is_none());
    assert!(right.next_source().unwrap().is_none());
    assert!(left.next_source().unwrap().is_none());
    drop(left);
    drop(right);
    assert!(memory.snapshot().unwrap().reserved_bytes > retained);
    assert_eq!(old.body(), b"base-a");
    drop(old);
    drop(new);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_namespace_cursor_empty_end_checks_cancellation_and_cannot_resume_after_failure() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("namespace-cursor-empty", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], HashAlgorithm::Blake3_256)).unwrap();
  let root = publish_namespace_directory(&publisher, vec![]);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let mut cursor = capture.open_namespace_configuration_cursor(namespace_source_request(&root)).expect("empty captured cursor");
  assert!(cursor.next_source().unwrap().is_none());
  cancellation.cancel();
  assert!(cursor.next_source().is_err());
  assert!(cursor.next_source().is_err());
  drop(cursor);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_namespace_cursor_pressure_failure_is_terminal_even_after_pressure_clears() {
  use crate::engine::memory_coordinator::HostMemorySample;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("namespace-cursor-pressure", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], HashAlgorithm::Blake3_256)).unwrap();
  let (directory, _) = namespace_configuration_tree(&publisher, "/docs", b"body", vec![]);
  let root = publish_namespace_directory(&publisher, vec![namespace_directory_child("docs", directory)]);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let mut cursor = capture.open_namespace_configuration_cursor(namespace_source_request(&root)).expect("bounded captured cursor");
  memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
  assert!(cursor.next_source().is_err());
  memory.update_host_sample(HostMemorySample::default()).unwrap();
  assert!(cursor.next_source().is_err());
  drop(cursor);
  let mut retry = capture.open_namespace_configuration_cursor(namespace_source_request(&root)).unwrap();
  assert_eq!(retry.next_source().unwrap().unwrap().body(), b"body");
  assert!(retry.next_source().unwrap().is_none());
  drop(retry);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}
