//! Cumulative budget, cancellation and committed-outcome targets.
use super::*;
use std::path::Path;

fn with_staged_fixture(
  globals: &[(String, &str, &[u8])],
  test: impl FnOnce(
    &V4FirstAuthorityPublisher,
    &NativeSemanticMutationInventoryV1<'_>,
    &MemoryCoordinator,
    &CancellationToken,
    &[u8],
    &[u8],
    &Path,
    &Path,
  ),
) {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("owned-source-union-boundaries", None, [1; 16], algorithm, 0);
  let initial = request_for_database_and_algorithm([1; 16], algorithm);
  let root = publisher.publish(&initial).unwrap().namespace_root.root_hash;
  seed_files(&publisher, globals);
  seed_union_generation(&publisher);
  let mut header = publisher.observe().unwrap().selected.header;
  header.required_reader_capabilities[3] |= 0b1010;
  header.required_writer_capabilities[3] |= 0b1010;
  write_redundant_header(&publisher, &header);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let parent = tempfile::tempdir().unwrap();
  test(&publisher, &capture, &memory, &cancellation, &root, &initial.namespace_tree.root_hash, parent.path(), &path);
  assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
}

fn owned_request<'a>(root: &'a [u8], tree: &'a [u8], parent: &'a Path) -> NativeSemanticSourceUnionRequestV1<'a> {
  NativeSemanticSourceUnionRequestV1 {
    expected_base_root: root,
    requested_directory_root: tree,
    replacements: &[],
    workspace_parent: parent,
    bounds: union_bounds(tree),
  }
}

fn staged_error_code(error: &NativeSemanticSourceUnionErrorV1) -> &'static str {
  match error {
    NativeSemanticSourceUnionErrorV1::Source(source) => source.code(),
    NativeSemanticSourceUnionErrorV1::Publication(source) => source.code(),
    NativeSemanticSourceUnionErrorV1::ControlPublication(source) => source.code(),
    NativeSemanticSourceUnionErrorV1::Namespace(source) => source.code(),
    other => panic!("unexpected error category: {other:?}"),
  }
}

#[test]
fn native_staged_source_union_exact_cumulative_limits_pass_and_one_less_refuses_with_retry() {
  let index = br#"{"$v":1,"indexes":[]}"#;
  let parser = br#"{"$v":1,"parsers":{}}"#;
  with_staged_fixture(
    &[(INDEX_SOURCE.into(), "application/json", index), (PARSER_SOURCE.into(), "application/json", parser)],
    |_, capture, _, _, root, tree, parent, path| {
      let measured =
        capture.prepare_and_stage_semantic_source_union(owned_request(root, tree, parent), staging_request()).unwrap().summary();
      assert_eq!(measured.source_copy_attempts, 2);
      assert!(measured.validation_read_bytes > 1);
      let exact = NativeSemanticSourceUnionStagingRequestV1 {
        maximum_source_copy_attempts: measured.source_copy_attempts,
        maximum_payload_bytes: measured.attempted_payload_bytes,
        maximum_validation_read_bytes: measured.validation_read_bytes,
        ..staging_request()
      };
      let before = fs::read(path).unwrap();
      assert_eq!(capture.prepare_and_stage_semantic_source_union(owned_request(root, tree, parent), exact).unwrap().summary(), measured);
      for case in 0..4 {
        let mut insufficient = exact;
        let expected = match case {
          0 => {
            insufficient.maximum_source_copy_attempts -= 1;
            "semantic_source_staging_copy_bound"
          }
          1 => {
            insufficient.maximum_payload_bytes -= 1;
            "semantic_source_staging_payload_bound"
          }
          2 => {
            insufficient.maximum_validation_read_bytes -= 1;
            "semantic_source_read_bound"
          }
          _ => {
            insufficient.maximum_node_workspace_bytes = 1;
            "semantic_source_node_workspace"
          }
        };
        let error = capture.prepare_and_stage_semantic_source_union(owned_request(root, tree, parent), insufficient).err().unwrap();
        assert_eq!(staged_error_code(&error), expected, "case={case}, {error:?}");
        assert_eq!(fs::read_dir(parent).unwrap().count(), 0);
        assert_eq!(fs::read(path).unwrap(), before);
        assert_eq!(capture.prepare_and_stage_semantic_source_union(owned_request(root, tree, parent), exact).unwrap().summary(), measured);
      }
      assert_eq!(fs::read(path).unwrap(), before);
    },
  );
}

#[test]
fn native_staged_source_union_empty_sources_require_no_copy_or_validation_allowance() {
  with_staged_fixture(&[], |_, capture, _, _, root, tree, parent, _| {
    let request =
      NativeSemanticSourceUnionStagingRequestV1 { maximum_source_copy_attempts: 0, maximum_validation_read_bytes: 0, ..staging_request() };
    let result = capture.prepare_and_stage_semantic_source_union(owned_request(root, tree, parent), request).unwrap();
    assert_eq!(result.summary().source_copy_attempts, 0);
    assert_eq!(result.summary().validation_read_bytes, 0);
    assert_eq!(result.summary().node_control_attempts, 1);
  });
}

#[test]
fn native_staged_source_union_precancellation_and_bad_publication_time_preserve_bytes() {
  with_staged_fixture(&[], |_, capture, _, cancellation, root, tree, parent, path| {
    let before = fs::read(path).unwrap();
    for timestamp in [0, i64::MAX as u64 + 1] {
      let request = NativeSemanticSourceUnionStagingRequestV1 { publication_timestamp_ms: timestamp, ..staging_request() };
      let error = capture.prepare_and_stage_semantic_source_union(owned_request(root, tree, parent), request).err().unwrap();
      assert_eq!(staged_error_code(&error), "semantic_source_staging_time");
      assert_eq!(fs::read(path).unwrap(), before);
    }
    cancellation.cancel();
    let error = capture.prepare_and_stage_semantic_source_union(owned_request(root, tree, parent), staging_request()).err().unwrap();
    assert_eq!(staged_error_code(&error), "semantic_task_observation_cancelled");
    assert_eq!(fs::read(path).unwrap(), before);
  });
}

#[test]
fn native_staged_source_union_preserves_post_commit_failures_for_both_owned_sinks() {
  for present in [false, true] {
    let globals =
      if present { vec![(INDEX_SOURCE.to_owned(), "application/json", br#"{"$v":1,"indexes":[]}"#.as_slice())] } else { vec![] };
    with_staged_fixture(&globals, |publisher, capture, _, _, root, tree, parent, _| {
      let error = capture
        .prepare_and_stage_semantic_source_union_observed(
          owned_request(root, tree, parent),
          staging_request(),
          &mut FailingPostCommitObserver,
        )
        .err()
        .unwrap();
      assert_eq!(staged_error_code(&error), "immutable_entity_committed_postcondition_failure");
      match error {
        NativeSemanticSourceUnionErrorV1::Publication(source) if present => assert!(source.committed_receipt().is_some()),
        NativeSemanticSourceUnionErrorV1::ControlPublication(source) if !present => assert!(source.committed_receipt().is_some()),
        other => panic!("original committed outcome was lost: {other:?}"),
      }
      assert_eq!(fs::read_dir(parent).unwrap().count(), 0);
      assert_eq!(publisher.observe().unwrap().selected.header.head_hash, root);
      drop(capture.prepare_and_stage_semantic_source_union(owned_request(root, tree, parent), staging_request()).unwrap());
    });
  }
}

#[test]
fn native_staged_source_union_plugin_payloads_use_their_own_larger_body_ceiling() {
  let module = plugin_fixtures::module("both");
  let alias = plugin_fixtures::alias(&module, "both");
  let module_path = plugin_fixtures::artifact_path(&module);
  let alias_path = plugin_fixtures::alias_path();
  let parser = br#"{"$v":1,"parsers":{"text/plain":"parse"}}"#;
  assert!(parser.len() <= 64 && module.len() > 64 && alias.len() > 64);
  with_staged_fixture(
    &[
      (PARSER_SOURCE.into(), "application/json", parser),
      (module_path.clone(), "application/wasm", &module),
      (alias_path.clone(), "application/octet-stream", &alias),
    ],
    |publisher, capture, memory, cancellation, root, tree, parent, _| {
      let mut request = owned_request(root, tree, parent);
      request.bounds.namespace.sources.maximum_body_bytes = 64;
      request.bounds.maximum_plugin_module_bytes = module.len();
      let expected_sources: Vec<_> = [PARSER_SOURCE, &alias_path, &module_path]
        .into_iter()
        .map(|path| capture.read_protected_source(path, source_bounds()).unwrap().unwrap())
        .collect();
      let expected_reads: u64 = expected_sources
        .iter()
        .flat_map(|source| &source.record().chunk_hashes)
        .map(|key| u64::from(publisher.locator(key).unwrap().unwrap().total_length))
        .sum();
      let result = capture.prepare_and_stage_semantic_source_union(request, staging_request()).unwrap();
      assert_eq!(result.source_union().catalogs().path_count(), 4);
      assert_eq!(result.summary().source_copy_attempts, 3);
      assert_eq!(result.summary().validation_read_bytes, expected_reads);
      let protection = publisher.acquire_staging_protection(memory, cancellation).unwrap();
      let fresh = protection.capture_semantic_mutation_inventory(capture_bounds(), memory, cancellation).unwrap();
      for source in &expected_sources {
        assert_eq!(
          fresh.read_retained_protected_source(&source.record().path, source.revision(), source_bounds()).unwrap().body(),
          source.body()
        );
      }
    },
  );
}

struct InterruptOwnedStaging<'a> {
  memory: &'a MemoryCoordinator,
  cancellation: &'a CancellationToken,
  pressure: bool,
  commits: usize,
}

impl FirstAuthorityDependencyObserverV1 for InterruptOwnedStaging<'_> {
  fn staged(&mut self, _: &DiskKVStore, _: &[PreparedWholeEntityV1]) -> Result<(), NativeDurabilityError> {
    Ok(())
  }

  fn authority_committed(&mut self, _: &DiskKVStore, _: &[PreparedWholeEntityV1]) -> Result<(), FirstAuthorityPublicationErrorV1> {
    self.commits += 1;
    if self.pressure {
      self
        .memory
        .update_host_sample(crate::engine::memory_coordinator::HostMemorySample { rss_bytes: 96 << 20, ..Default::default() })
        .unwrap();
    } else {
      self.cancellation.cancel();
    }
    Ok(())
  }
}

#[test]
fn native_staged_source_union_interruptions_after_dependency_commit_never_claim_rollback_or_completion() {
  for pressure in [false, true] {
    for present in [false, true] {
      let globals = if present { vec![(INDEX_SOURCE.into(), "application/json", br#"{"$v":1,"indexes":[]}"#.as_slice())] } else { vec![] };
      with_staged_fixture(&globals, |publisher, capture, memory, cancellation, root, tree, parent, path| {
        let before = fs::read(path).unwrap();
        let baseline = memory.snapshot().unwrap().reserved_bytes;
        let mut observer = InterruptOwnedStaging { memory, cancellation, pressure, commits: 0 };
        let error = capture
          .prepare_and_stage_semantic_source_union_observed(owned_request(root, tree, parent), staging_request(), &mut observer)
          .err()
          .unwrap();
        assert_eq!(observer.commits, 1);
        assert_eq!(
          staged_error_code(&error),
          if pressure { "semantic_task_observation_memory" } else { "semantic_task_observation_cancelled" }
        );
        assert_ne!(fs::read(path).unwrap(), before, "the dependency really committed before interruption");
        assert_eq!(publisher.observe().unwrap().selected.header.head_hash, root);
        assert_eq!(fs::read_dir(parent).unwrap().count(), 0);
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
        memory.update_host_sample(Default::default()).unwrap();
        let retry_cancel = CancellationToken::new();
        let protection = publisher.acquire_staging_protection(memory, &retry_cancel).unwrap();
        let fresh = protection.capture_semantic_mutation_inventory(capture_bounds(), memory, &retry_cancel).unwrap();
        drop(fresh.prepare_and_stage_semantic_source_union(owned_request(root, tree, parent), staging_request()).unwrap());
        assert_eq!(fresh.visit(|_| panic!("source staging selected a task")).unwrap().tasks, 0);
      });
    }
  }
}

#[test]
fn native_staged_source_union_cold_partial_budget_failure_keeps_only_unselected_dependencies_and_retries() {
  with_staged_fixture(
    &[
      (INDEX_SOURCE.into(), "application/json", br#"{"$v":1,"indexes":[]}"#),
      (PARSER_SOURCE.into(), "application/json", br#"{"$v":1,"parsers":{}}"#),
    ],
    |publisher, capture, _, _, root, tree, parent, _| {
      let revisions: Vec<_> = [INDEX_SOURCE, PARSER_SOURCE]
        .into_iter()
        .map(|path| capture.read_protected_source(path, source_bounds()).unwrap().unwrap().revision().to_vec())
        .collect();
      for revision in &revisions {
        assert!(publisher.locator(revision).unwrap().is_none());
      }
      let error = capture
        .prepare_and_stage_semantic_source_union(
          owned_request(root, tree, parent),
          NativeSemanticSourceUnionStagingRequestV1 { maximum_source_copy_attempts: 1, ..staging_request() },
        )
        .err()
        .unwrap();
      assert_eq!(staged_error_code(&error), "semantic_source_staging_copy_bound");
      assert!(publisher.locator(&revisions[0]).unwrap().is_some());
      assert!(publisher.locator(&revisions[1]).unwrap().is_none());
      assert_eq!(publisher.observe().unwrap().selected.header.head_hash, root);
      let result = capture.prepare_and_stage_semantic_source_union(owned_request(root, tree, parent), staging_request()).unwrap();
      assert_eq!(result.summary().source_copy_attempts, 2, "idempotent retries still count attempts");
      for revision in &revisions {
        assert!(publisher.locator(revision).unwrap().is_some());
      }
    },
  );
}

#[test]
fn native_staged_source_union_entry_pressure_and_zero_workspace_are_byte_stable() {
  with_staged_fixture(&[], |_, capture, memory, _, root, tree, parent, path| {
    let before = fs::read(path).unwrap();
    let error = capture
      .prepare_and_stage_semantic_source_union(
        owned_request(root, tree, parent),
        NativeSemanticSourceUnionStagingRequestV1 { maximum_node_workspace_bytes: 0, ..staging_request() },
      )
      .err()
      .unwrap();
    assert_eq!(staged_error_code(&error), "semantic_source_staging_workspace");
    memory.update_host_sample(crate::engine::memory_coordinator::HostMemorySample { rss_bytes: 96 << 20, ..Default::default() }).unwrap();
    let error = capture.prepare_and_stage_semantic_source_union(owned_request(root, tree, parent), staging_request()).err().unwrap();
    assert_eq!(staged_error_code(&error), "semantic_task_observation_memory");
    assert_eq!(fs::read(path).unwrap(), before);
    memory.update_host_sample(Default::default()).unwrap();
    drop(capture.prepare_and_stage_semantic_source_union(owned_request(root, tree, parent), staging_request()).unwrap());
  });
}

#[test]
fn native_staged_source_union_meter_reports_zero_for_a_valid_empty_record_without_chunks() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("metered-empty-record", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    let record = FileRecord {
      path: INDEX_SOURCE.into(),
      content_type: Some("application/json".into()),
      total_size: 0,
      created_at: 1,
      updated_at: 1,
      metadata: vec![],
      content_hash: digest_parts(algorithm, &[b""]),
      chunk_hashes: vec![],
    }
    .serialize(algorithm.hash_length())
    .unwrap();
    let revision = publish_namespace_value(&publisher, EntryTypeV4::FileRecord, 1, b"filec:", &record);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let source = capture.read_retained_protected_source(INDEX_SOURCE, &revision, source_bounds()).unwrap();
    assert!(source.body().is_empty() && source.record().chunk_hashes.is_empty());
    let bounds = NativeSemanticSourceReadBoundsV1 { maximum_body_bytes: 0, maximum_read_bytes: 1, ..source_bounds() };
    let (first, reads) = source.stage_retained_copy_metered(bounds, 100).unwrap();
    assert!(first.idempotent);
    assert_eq!(reads, 0);
    let before = fs::read(&path).unwrap();
    let (repeat, reads) = source.stage_retained_copy_metered(bounds, 101).unwrap();
    assert!(repeat.idempotent);
    assert_eq!(reads, 0);
    assert_eq!(repeat.entities[0].write_sequence, first.entities[0].write_sequence);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
