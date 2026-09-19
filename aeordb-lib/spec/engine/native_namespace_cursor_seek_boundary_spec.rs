//! Exclusive cursor bounds, cumulative budgets and failure lifetimes.
use super::*;

#[test]
fn namespace_cursor_late_seek_completes_under_a_budget_that_refuses_prefix_enumeration() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("namespace-cursor-seek-work", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    let mut children = Vec::new();
    for index in 0..24 {
      let owner = format!("p{index:03}");
      let (root, _) = namespace_configuration_tree(&publisher, &format!("/{owner}"), b"body", vec![]);
      children.push(namespace_directory_child(&owner, root));
    }
    let root = publish_namespace_directory(&publisher, children);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let reserved = memory.snapshot().unwrap().reserved_bytes;
    let mut request = namespace_source_request(&root);
    request.bounds.maximum_work = 768;
    let mut from_start = capture.open_namespace_configuration_cursor(request).unwrap();
    loop {
      match from_start.next_source() {
        Ok(Some(_)) => {}
        Ok(None) => panic!("fixture budget must not permit whole-prefix enumeration"),
        Err(error) => {
          assert_eq!(error.code(), "semantic_namespace_source_work_bound");
          break;
        }
      }
    }
    drop(from_start);
    let mut cursor = capture
      .open_namespace_configuration_cursor_after(request, "/p021/.aeordb-config/indexes.json")
      .expect("direct late seek must fit its bounded path workspace");
    for index in [22, 23] {
      assert_eq!(cursor.next_source().unwrap().unwrap().record().path, format!("/p{index:03}/.aeordb-config/indexes.json"));
    }
    assert!(cursor.next_source().unwrap().is_none());
    drop(cursor);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn namespace_cursor_seek_orders_utf8_and_non_directory_bounds_without_live_fallback() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("namespace-seek-utf8", None, [1; 16], algorithm, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
  let (file, _) = publish_namespace_configuration(&publisher, "/b", b"ordinary");
  let mut children = vec![NamespaceFixtureChild {
    name: "b".into(),
    kind: EntryTypeV4::FileRecord,
    key: file,
    size: 8,
    content_type: Some("application/json"),
  }];
  let owners = ["a.", "é!", "é", "é0", "中"];
  for owner in owners {
    let (tree, _) = namespace_configuration_tree(&publisher, &format!("/{owner}"), owner.as_bytes(), vec![]);
    children.push(namespace_directory_child(owner, tree));
  }
  let root = publish_namespace_directory(&publisher, children);
  let mut expected: Vec<_> = owners.map(|owner| format!("/{owner}/.aeordb-config/indexes.json")).into();
  expected.sort();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let before = fs::read(&path).unwrap();
  for owner in ["b", "é!", "é", "é/deleted", "é0", "中", "龍"] {
    let after = format!("/{owner}/.aeordb-config/indexes.json");
    let mut cursor = capture.open_namespace_configuration_cursor_after(namespace_source_request(&root), &after).unwrap();
    let mut actual = Vec::new();
    while let Some(source) = cursor.next_source().unwrap() {
      actual.push(source.record().path.clone());
    }
    assert_eq!(actual, expected.iter().filter(|path| *path > &after).cloned().collect::<Vec<_>>(), "{after}");
    drop(cursor);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  }
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn namespace_cursor_seek_pressure_cancellation_and_empty_end_never_resume_failed_state() {
  use crate::engine::memory_coordinator::HostMemorySample;
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("namespace-seek-lifecycle", None, [1; 16], algorithm, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
  let (a, _) = namespace_configuration_tree(&publisher, "/a", b"a", vec![]);
  let (z, _) = namespace_configuration_tree(&publisher, "/z", b"z", vec![]);
  let root = publish_namespace_directory(&publisher, vec![namespace_directory_child("a", a), namespace_directory_child("z", z)]);
  let before = fs::read(&path).unwrap();
  for cancel in [false, true] {
    for point in [0, 1, 2, 3] {
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let retained = memory.snapshot().unwrap().reserved_bytes;
      let after = if point == 3 { "/zz/.aeordb-config/indexes.json" } else { "/0/.aeordb-config/indexes.json" };
      let refuse = || {
        if cancel {
          cancellation.cancel();
        } else {
          memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
        }
      };
      let code = if cancel { "semantic_task_observation_cancelled" } else { "semantic_task_observation_memory" };
      if point == 0 {
        refuse();
        assert_eq!(capture.open_namespace_configuration_cursor_after(namespace_source_request(&root), after).err().unwrap().code(), code);
      } else {
        let mut cursor = capture.open_namespace_configuration_cursor_after(namespace_source_request(&root), after).unwrap();
        if point == 2 {
          assert_eq!(cursor.next_source().unwrap().unwrap().body(), b"a");
        } else if point == 3 {
          assert!(cursor.next_source().unwrap().is_none());
        }
        refuse();
        assert_eq!(cursor.next_source().err().unwrap().code(), code);
        memory.update_host_sample(HostMemorySample::default()).unwrap();
        assert_eq!(cursor.next_source().err().unwrap().code(), "semantic_namespace_cursor_failed");
        drop(cursor);
      }
      memory.update_host_sample(HostMemorySample::default()).unwrap();
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      if !cancel {
        let mut retry =
          capture.open_namespace_configuration_cursor_after(namespace_source_request(&root), "/a/.aeordb-config/indexes.json").unwrap();
        assert_eq!(retry.next_source().unwrap().unwrap().body(), b"z");
        assert!(retry.next_source().unwrap().is_none());
        drop(retry);
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      }
    }
  }
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn namespace_cursor_seek_relative_bound_allocation_refuses_releases_and_retries() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("namespace-seek-bound-allocation", None, [1; 16], algorithm, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
  let root = publish_namespace_directory(&publisher, vec![]);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let before = fs::read(&path).unwrap();
  let after = format!("/{}/.aeordb-config/indexes.json", "a".repeat(1237));
  let mut request = namespace_source_request(&root);
  request.bounds.maximum_path_bytes = after.len();
  let (result, allocations) =
    allocation_probe::measure(after.len() - 1, || capture.open_namespace_configuration_cursor_after(request, &after));
  assert!(allocations.injected_failure, "{allocations:?}");
  assert_eq!(result.err().unwrap().code(), "semantic_namespace_source_allocation");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  let mut retry = capture.open_namespace_configuration_cursor_after(request, &after).unwrap();
  assert!(retry.next_source().unwrap().is_none());
  drop(retry);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn namespace_cursor_seek_and_successors_share_one_work_and_read_budget() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("namespace-seek-budget", None, [1; 16], algorithm, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
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
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let before = fs::read(&path).unwrap();
  for read_budget in [false, true] {
    let mut constructor_refused = false;
    let mut partial_refused = false;
    let mut complete = false;
    let scale = if read_budget { 512 } else { 8 };
    let code = if read_budget { "semantic_source_read_bound" } else { "semantic_namespace_source_work_bound" };
    for step in 0..=128 {
      let budget = 1 + step * scale;
      let mut request = namespace_source_request(&root);
      if read_budget {
        request.bounds.sources.maximum_read_bytes = budget;
      } else {
        request.bounds.maximum_work = budget;
      }
      let mut cursor = match capture.open_namespace_configuration_cursor_after(request, "/a/.aeordb-config/indexes.json") {
        Ok(cursor) => cursor,
        Err(error) => {
          assert_eq!(error.code(), code);
          constructor_refused = true;
          assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
          continue;
        }
      };
      let mut actual = Vec::new();
      loop {
        match cursor.next_source() {
          Ok(Some(source)) => actual.push(source.record().path.clone()),
          Ok(None) => {
            complete = true;
            break;
          }
          Err(error) => {
            assert_eq!(error.code(), code);
            partial_refused |= !actual.is_empty();
            assert_eq!(cursor.next_source().err().unwrap().code(), "semantic_namespace_cursor_failed");
            break;
          }
        }
      }
      drop(cursor);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      if complete {
        assert_eq!(actual, ["/b/.aeordb-config/indexes.json", "/c/.aeordb-config/indexes.json"]);
        break;
      }
    }
    assert!(constructor_refused && partial_refused && complete, "read_budget={read_budget}");
  }
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn namespace_cursor_seek_rejects_noncanonical_nonconfiguration_and_over_bound_paths() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("namespace-cursor-seek-invalid", None, [1; 16], algorithm, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
  let root = publish_namespace_directory(&publisher, vec![]);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let reserved = memory.snapshot().unwrap().reserved_bytes;
  for after in [
    "",
    "relative/.aeordb-config/indexes.json",
    "/",
    "/a",
    "/a/file.txt",
    "/.aeordb-config/indexes.json",
    "/a//.aeordb-config/indexes.json",
    "/a/../b/.aeordb-config/indexes.json",
    "/a/.aeordb-config/indexes.json/",
  ] {
    assert!(capture.open_namespace_configuration_cursor_after(namespace_source_request(&root), after).is_err(), "{after}");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
  }
  let after = "/a/.aeordb-config/indexes.json";
  for mode in 0..2 {
    let mut request = namespace_source_request(&root);
    if mode == 0 {
      request.bounds.maximum_path_bytes = after.len() - 1;
    } else {
      request.bounds.maximum_path_depth = 2;
    }
    assert!(capture.open_namespace_configuration_cursor_after(request, after).is_err());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
  }
  let mut exact = namespace_source_request(&root);
  exact.bounds.maximum_path_bytes = after.len();
  exact.bounds.maximum_path_depth = 3;
  let mut cursor = capture.open_namespace_configuration_cursor_after(exact, after).unwrap();
  assert!(cursor.next_source().unwrap().is_none());
  drop(cursor);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn namespace_cursor_seek_keeps_captured_view_and_releases_root_allocation_refusal() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    // Target the cursor's fallible root copy, not the registry singleton's
    // inherited one-time digest allocations. This must also work in isolation.
    crate::engine::v4::system_family::embedded_system_family_registry(algorithm).unwrap();
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("namespace-cursor-seek-allocation", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    let (old, revision) = namespace_configuration_tree(&publisher, "/z", b"old", vec![]);
    let root = publish_namespace_directory(&publisher, vec![namespace_directory_child("z", old)]);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let (new, _) = namespace_configuration_tree(&publisher, "/z", b"new", vec![]);
    let new_root = publish_namespace_directory(&publisher, vec![namespace_directory_child("z", new)]);
    let before = fs::read(&path).unwrap();
    let reserved = memory.snapshot().unwrap().reserved_bytes;
    let after = "/a/.aeordb-config/indexes.json";
    let (result, allocations) = allocation_probe::measure(algorithm.hash_length(), || {
      capture.open_namespace_configuration_cursor_after(namespace_source_request(&root), after)
    });
    assert!(allocations.injected_failure);
    assert_eq!(result.err().unwrap().code(), "semantic_namespace_source_allocation");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
    let mut retry = capture.open_namespace_configuration_cursor_after(namespace_source_request(&root), after).unwrap();
    let row = retry.next_source().unwrap().unwrap();
    assert_eq!(row.revision(), revision);
    assert_eq!(row.body(), b"old");
    drop(row);
    drop(retry);
    let stale = capture.open_namespace_configuration_cursor_after(namespace_source_request(&new_root), after);
    match stale {
      Err(_) => {}
      Ok(mut cursor) => assert!(cursor.next_source().is_err(), "capture must not substitute a live locator"),
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
