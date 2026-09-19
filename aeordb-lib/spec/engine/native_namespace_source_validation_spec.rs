//! Boundary and lifecycle coverage for captured namespace sources.
use super::*;
use crate::engine::memory_coordinator::HostMemorySample;

#[test]
fn native_namespace_source_multiple_configurations_share_one_work_and_read_budget() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("namespace-cumulative-bounds", None, [1; 16]);
  publisher.publish(&request_for_database_and_algorithm([1; 16], HashAlgorithm::Blake3_256)).unwrap();
  let body = vec![b'x'; 128 << 10];
  let (left, _) = namespace_configuration_tree(&publisher, "/a", &body, vec![]);
  let (right, _) = namespace_configuration_tree(&publisher, "/b", &body, vec![]);
  let root = publish_namespace_directory(&publisher, vec![namespace_directory_child("a", left), namespace_directory_child("b", right)]);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let mut request = namespace_source_request(&root);
  // Enough for either complete source and the small directory traversal, but
  // not both source bodies. Reinitializing per-file budgets would wrongly pass.
  request.bounds.sources.maximum_read_bytes = body.len() as u64 + (64 << 10);
  let mut calls = 0;
  let error = capture
    .visit_namespace_configuration_sources(request, |_| {
      calls += 1;
      Ok(true)
    })
    .unwrap_err();
  assert_eq!(calls, 1);
  assert_eq!(error.code(), "semantic_source_read_bound");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  request = namespace_source_request(&root);
  request.bounds.maximum_work = 1;
  let error = capture.visit_namespace_configuration_sources(request, |_| panic!("exhausted work emitted a source")).unwrap_err();
  assert_eq!(error.code(), "semantic_namespace_source_work_bound");
  let summary = capture.visit_namespace_configuration_sources(namespace_source_request(&root), |_| Ok(true)).unwrap();
  assert_eq!(summary, NativeSemanticNamespaceSourceSummaryV1 { configurations: 2, complete: true });
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_namespace_source_exact_reader_limits_and_identity_are_explicit() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("namespace-source-limits", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    let source_path = "/docs/.aeordb-config/indexes.json";
    let body = vec![b'x'; 997];
    let (revision, _) = publish_namespace_configuration(&publisher, source_path, &body);
    let chunk = digest_parts(algorithm, &[b"chunk:", &body]);
    let kv = publisher.lock_kv().unwrap();
    let record_bytes = u64::from(kv.get(&revision).unwrap().unwrap().total_length);
    let chunk_bytes = u64::from(kv.get(&chunk).unwrap().unwrap().total_length);
    drop(kv);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let exact = NativeSemanticSourceReadBoundsV1 {
      maximum_body_bytes: body.len(),
      maximum_chunk_entity_bytes: chunk_bytes as usize,
      maximum_chunks: 1,
      maximum_read_bytes: record_bytes + chunk_bytes,
    };
    for case in 0..3 {
      let mut bounds = exact;
      let code = match case {
        0 => {
          bounds.maximum_body_bytes -= 1;
          "semantic_source_body_bound"
        }
        1 => {
          bounds.maximum_chunk_entity_bytes -= 1;
          "semantic_source_chunk_bound"
        }
        2 => {
          bounds.maximum_read_bytes -= 1;
          "semantic_source_read_bound"
        }
        _ => unreachable!(),
      };
      let error = capture.read_namespace_configuration_source(source_path, &revision, bounds).err().expect("short budget must refuse");
      assert_eq!(error.code(), code);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(capture.read_namespace_configuration_source(source_path, &revision, exact).unwrap().body(), body);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    }
    for invalid_path in [
      INDEX_SOURCE,
      "/docs/plain.json",
      "/docs/.aeordb-config/parsers.json",
      "/docs//.aeordb-config/indexes.json",
      "/other/.aeordb-config/indexes.json",
    ] {
      assert!(capture.read_namespace_configuration_source(invalid_path, &revision, exact).is_err());
    }
    for invalid_revision in
      [Vec::new(), vec![0; algorithm.hash_length()], vec![1; algorithm.hash_length() - 1], vec![9; algorithm.hash_length()]]
    {
      assert!(capture.read_namespace_configuration_source(source_path, &invalid_revision, exact).is_err());
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_namespace_source_empty_tree_still_checks_all_limits_and_exact_read_budget() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("namespace-empty-bounds", None, [1; 16]);
  publisher.publish(&request_for_database_and_algorithm([1; 16], HashAlgorithm::Blake3_256)).unwrap();
  let root = publish_namespace_directory(&publisher, vec![]);
  let root_bytes = u64::from(publisher.lock_kv().unwrap().get(&root).unwrap().unwrap().total_length);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let mut exact = namespace_source_request(&root);
  exact.bounds.maximum_work = 1;
  exact.bounds.sources.maximum_read_bytes = root_bytes;
  exact.bounds.maximum_directory_entity_bytes = root_bytes as usize;
  let summary = capture.visit_namespace_configuration_sources(exact, |_| panic!("empty tree visited a source")).unwrap();
  assert_eq!(summary, NativeSemanticNamespaceSourceSummaryV1 { configurations: 0, complete: true });
  for case in 0..14 {
    let mut request = exact;
    match case {
      0 => request.bounds.maximum_path_bytes = 0,
      1 => request.bounds.maximum_path_bytes = u16::MAX as usize + 1,
      2 => request.bounds.maximum_path_depth = 0,
      3 => request.bounds.maximum_path_depth = 257,
      4 => request.bounds.maximum_btree_depth = 0,
      5 => request.bounds.maximum_btree_depth = 257,
      6 => request.bounds.maximum_work = 0,
      7 => request.bounds.maximum_directory_entity_bytes = 0,
      8 => request.bounds.maximum_directory_entity_bytes = (48 << 20) + 1,
      9 => request.bounds.sources.maximum_body_bytes = (64 << 20) + 1,
      10 => request.bounds.sources.maximum_chunk_entity_bytes = 0,
      11 => request.bounds.sources.maximum_chunks = 0,
      12 => request.bounds.sources.maximum_read_bytes -= 1,
      13 => request.bounds.maximum_directory_entity_bytes -= 1,
      _ => unreachable!(),
    }
    assert!(capture.visit_namespace_configuration_sources(request, |_| panic!("invalid request visited a source")).is_err(), "case{case}");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_namespace_source_path_depth_early_stop_and_callback_failure_release_memory() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("namespace-path-bounds", None, [1; 16]);
  publisher.publish(&request_for_database_and_algorithm([1; 16], HashAlgorithm::Blake3_256)).unwrap();
  let (directory, _) = namespace_configuration_tree(&publisher, "/docs", b"{}", vec![]);
  let root = publish_namespace_directory(&publisher, vec![namespace_directory_child("docs", directory)]);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let mut exact = namespace_source_request(&root);
  exact.bounds.maximum_path_bytes = "/docs/.aeordb-config/indexes.json".len();
  exact.bounds.maximum_path_depth = 3;
  exact.bounds.maximum_btree_depth = 1;
  for case in 0..2 {
    let mut request = exact;
    if case == 0 {
      request.bounds.maximum_path_bytes -= 1;
    } else {
      request.bounds.maximum_path_depth -= 1;
    }
    assert!(capture.visit_namespace_configuration_sources(request, |_| panic!("path bound must refuse before callback")).is_err());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
  for keep_going in [false, true] {
    let mut calls = 0;
    let summary = capture
      .visit_namespace_configuration_sources(exact, |_| {
        calls += 1;
        Ok(keep_going)
      })
      .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(summary, NativeSemanticNamespaceSourceSummaryV1 { configurations: 1, complete: keep_going });
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
  let error = capture
    .visit_namespace_configuration_sources(exact, |_| {
      Err(NativeSemanticNamespaceSourceErrorV1::Source(SemanticMutationObservationErrorV1::Invalid {
        code: "test_callback",
        message: "stop",
      }))
    })
    .unwrap_err();
  assert_eq!(error.code(), "test_callback");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_namespace_source_final_cancellation_and_pressure_never_report_complete() {
  for (has_source, keep_going) in [(false, true), (true, true), (true, false)] {
    for cancel in [true, false] {
      let (_directory, path, _coordinator, publisher) = create_environment_for_database("namespace-final-check", None, [1; 16]);
      publisher.publish(&request_for_database_and_algorithm([1; 16], HashAlgorithm::Blake3_256)).unwrap();
      let children = if has_source {
        let (directory, _) = namespace_configuration_tree(&publisher, "/docs", b"{}", vec![]);
        vec![namespace_directory_child("docs", directory)]
      } else {
        vec![]
      };
      let root = publish_namespace_directory(&publisher, children);
      let before = fs::read(&path).unwrap();
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let mut calls = 0;
      let mut final_checks = 0;
      let result = capture.visit_namespace_configuration_sources_observed(
        namespace_source_request(&root),
        |_| {
          calls += 1;
          Ok(keep_going)
        },
        || {
          final_checks += 1;
          if cancel {
            cancellation.cancel();
          } else {
            memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
          }
        },
      );
      assert_eq!(calls, usize::from(has_source));
      assert_eq!(final_checks, 1);
      assert_eq!(
        result.unwrap_err().code(),
        if cancel { "semantic_task_observation_cancelled" } else { "semantic_task_observation_memory" }
      );
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      if !cancel {
        memory.update_host_sample(HostMemorySample::default()).unwrap();
        let summary = capture.visit_namespace_configuration_sources(namespace_source_request(&root), |_| Ok(true)).unwrap();
        assert!(summary.complete);
        assert_eq!(summary.configurations, u64::from(has_source));
      }
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

#[test]
fn native_namespace_source_work_exhaustion_is_always_a_resource_refusal() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("namespace-work-category", None, [1; 16]);
  publisher.publish(&request_for_database_and_algorithm([1; 16], HashAlgorithm::Blake3_256)).unwrap();
  let (directory, _) = namespace_configuration_tree(&publisher, "/docs", b"{}", vec![]);
  let root = publish_namespace_directory(&publisher, vec![namespace_directory_child("docs", directory)]);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let mut succeeded = false;
  // Exercise every exhaustion point, including FileRecord and chunk reads.
  for work in 1..=128 {
    let mut request = namespace_source_request(&root);
    request.bounds.maximum_work = work;
    let result = capture.visit_namespace_configuration_sources(request, |_| Ok(true));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    match result {
      Ok(summary) => {
        assert_eq!(summary, NativeSemanticNamespaceSourceSummaryV1 { configurations: 1, complete: true });
        succeeded = true;
        break;
      }
      Err(error) => assert!(
        matches!(
          error,
          NativeSemanticNamespaceSourceErrorV1::Source(SemanticMutationObservationErrorV1::ResourceRead {
            code: "semantic_namespace_source_work_bound",
            ..
          })
        ),
        "work={work}: {error:?}"
      ),
    }
  }
  assert!(succeeded, "bounded fixture must eventually fit the work budget");
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_namespace_source_entry_and_callback_cancellation_and_pressure_release_memory() {
  for at_entry in [true, false] {
    for cancel in [true, false] {
      let (_directory, path, _coordinator, publisher) = create_environment_for_database("namespace-lifecycle", None, [1; 16]);
      publisher.publish(&request_for_database_and_algorithm([1; 16], HashAlgorithm::Blake3_256)).unwrap();
      let (directory, revision) = namespace_configuration_tree(&publisher, "/docs", b"{}", vec![]);
      let root = publish_namespace_directory(&publisher, vec![namespace_directory_child("docs", directory)]);
      let before = fs::read(&path).unwrap();
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let refuse = || {
        if cancel {
          cancellation.cancel();
        } else {
          memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
        }
      };
      let expected = if cancel { "semantic_task_observation_cancelled" } else { "semantic_task_observation_memory" };
      if at_entry {
        refuse();
        let error = capture
          .read_namespace_configuration_source("/docs/.aeordb-config/indexes.json", &revision, source_bounds())
          .err()
          .expect("exact reader must reject before reading");
        assert_eq!(error.code(), expected);
      }
      let mut calls = 0;
      let error = capture
        .visit_namespace_configuration_sources(namespace_source_request(&root), |_| {
          calls += 1;
          refuse();
          Ok(false)
        })
        .unwrap_err();
      assert_eq!(calls, usize::from(!at_entry));
      assert_eq!(error.code(), expected);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      if !cancel {
        memory.update_host_sample(HostMemorySample::default()).unwrap();
        assert!(capture.visit_namespace_configuration_sources(namespace_source_request(&root), |_| Ok(true)).unwrap().complete);
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      }
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}
