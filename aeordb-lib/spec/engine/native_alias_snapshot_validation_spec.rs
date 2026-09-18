//! Cumulative native reads, bounded retained metadata and lifecycle failures.
use super::*;
use crate::engine::memory_coordinator::HostMemorySample;

fn named_alias(module: &[u8], name: &str) -> (String, Vec<u8>) {
  assert_eq!(name.len(), 5);
  let mut alias = fixtures::alias(module, "both");
  alias[128..133].copy_from_slice(name.as_bytes());
  fixtures::seal(&mut alias);
  (format!("/.aeordb-system/plugin-aliases/{}", blake3::hash(name.as_bytes()).to_hex()), alias)
}

#[test]
fn native_alias_snapshot_unique_pairs_share_exact_cumulative_read_budget() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("alias-snapshot-budget", None, [1; 16], algorithm, 0);
    let module = fixtures::module("both");
    let (second_path, second_alias) = named_alias(&module, "other");
    let first_alias = fixtures::alias(&module, "both");
    seed_plugin(&publisher, &module, "both");
    seed_files(&publisher, &[(second_path.clone(), "application/octet-stream", &second_alias)]);
    let kv = publisher.lock_kv().unwrap();
    let physical = |name: &str, body: &[u8]| -> u64 {
      [first_authority_file_path_hash(name, algorithm), first_authority_system_chunk_hash(body, algorithm)]
        .iter()
        .map(|key| u64::from(kv.get(key).unwrap().unwrap().total_length))
        .sum()
    };
    let first_bytes = physical(&fixtures::alias_path(), &first_alias);
    let second_bytes = physical(&second_path, &second_alias);
    let module_bytes = physical(&fixtures::artifact_path(&module), &module);
    drop(kv);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let source = br#"{"$v":1,"parser":"parse","indexes":[{"name":"z","type":"typed_exact_blake3_v1","source":{"plugin":"other"}},{"name":"y","type":"typed_exact_blake3_v1","source":{"plugin":"parse"}},{"name":"a","type":"typed_exact_blake3_v1","source":{"plugin":"other"}}]}"#;
    let mut request = snapshot_request(SemanticSourceAliasKindV1::IndexConfiguration, Some(source));
    let exact = first_bytes + second_bytes + 2 * module_bytes;
    request.plugins.maximum_read_bytes = exact - 1;
    let error = capture.prepare_current_semantic_alias_snapshot(request).err().expect("aggregate budget must refuse");
    assert!(matches!(error, NativeSemanticPluginSourceErrorV1::Source(ref error) if error.code() == "semantic_source_read_bound"));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    request.plugins.maximum_read_bytes = exact;
    let prepared = capture.prepare_current_semantic_alias_snapshot(request).unwrap();
    assert_eq!(prepared.resolve_parser_alias("parse").unwrap().unwrap().role, 1);
    assert_eq!(prepared.resolve_mapper_alias("parse").unwrap().unwrap().role, 2);
    assert_eq!(prepared.resolve_mapper_alias("other").unwrap().unwrap().role, 2);
    assert!(matches!(prepared.resolve_parser_alias("other"), Err(SemanticCompilationErrorV1::Operational { .. })));
    drop(prepared);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_alias_snapshot_never_retains_full_module_bodies() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("alias-snapshot-retained", None, [1; 16]);
  let mut module = fixtures::module("both");
  fixtures::custom("padding", &vec![0; 2 << 20], &mut module);
  seed_plugin(&publisher, &module, "both");
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let source = br#"{"$v":1,"parser":"parse","indexes":[{"name":"x","type":"typed_exact_blake3_v1","source":{"plugin":"parse"}}]}"#;
  let mut request = snapshot_request(SemanticSourceAliasKindV1::IndexConfiguration, Some(source));
  request.plugins.maximum_module_bytes = 4 << 20;
  request.plugins.maximum_chunk_entity_bytes = 4 << 20;
  let prepared = capture.prepare_current_semantic_alias_snapshot(request).unwrap();
  assert!(memory.snapshot().unwrap().reserved_bytes - baseline < 64 << 10);
  assert_eq!(prepared.resolve_parser_alias("parse").unwrap().unwrap().artifact_length, module.len() as u64);
  drop(prepared);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_alias_snapshot_missing_declared_role_is_unavailable_not_unprepared() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("alias-snapshot-role", None, [1; 16]);
  seed_plugin(&publisher, &fixtures::module("parser"), "parser");
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let source = br#"{"$v":1,"indexes":[{"name":"x","type":"typed_exact_blake3_v1","source":{"plugin":"parse"}}]}"#;
  let prepared =
    capture.prepare_current_semantic_alias_snapshot(snapshot_request(SemanticSourceAliasKindV1::IndexConfiguration, Some(source))).unwrap();
  assert!(prepared.resolve_mapper_alias("parse").unwrap().is_none());
  assert!(matches!(prepared.resolve_parser_alias("parse"), Err(SemanticCompilationErrorV1::Operational { .. })));
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_alias_snapshot_source_table_and_pair_bounds_refuse_and_release() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("alias-snapshot-bounds", None, [1; 16]);
  seed_plugin(&publisher, &fixtures::module("both"), "both");
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let source = br#"{"$v":1,"parsers":{"text/a":"parse","text/b":"parse"}}"#;
  for case in 0..7 {
    let mut request = snapshot_request(SemanticSourceAliasKindV1::ParserRegistry, Some(source));
    match case {
      0 => request.source.maximum_source_bytes = 1,
      1 => request.source.maximum_workspace_bytes = 1,
      2 => request.source.maximum_alias_occurrences = 1,
      3 => request.maximum_snapshot_bytes = 1,
      4 => request.plugins.maximum_module_bytes = 0,
      5 => request.plugins.maximum_read_bytes = 0,
      6 => request.plugins.maximum_workspace_bytes = 1,
      _ => unreachable!(),
    }
    assert!(capture.prepare_current_semantic_alias_snapshot(request).is_err(), "case{case}");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
  for source in [br#"{}"#.as_slice(), br#"{"$v":1,"parsers":{"text/a":"parse","TEXT/A":"other"}}"#.as_slice()] {
    assert!(matches!(
      capture.prepare_current_semantic_alias_snapshot(snapshot_request(SemanticSourceAliasKindV1::ParserRegistry, Some(source))),
      Err(NativeSemanticPluginSourceErrorV1::Identity(SemanticCompilationErrorV1::InvalidSource { .. }))
    ));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
  drop(capture.prepare_current_semantic_alias_snapshot(snapshot_request(SemanticSourceAliasKindV1::ParserRegistry, Some(source))).unwrap());
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_alias_snapshot_dependency_allocation_failures_release_partial_records_then_retry() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("alias-snapshot-allocation", None, [1; 16]);
  let module = fixtures::module("both");
  seed_plugin(&publisher, &module, "both");
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let source = br#"{"$v":1,"parser":"parse","indexes":[{"name":"x","type":"typed_exact_blake3_v1","source":{"plugin":"parse"}}]}"#;
  let request = snapshot_request(SemanticSourceAliasKindV1::IndexConfiguration, Some(source));
  let size = expected_dependency(&module, 1).len();
  for nth in 1..=4 {
    let (result, allocations) = allocation_probe::measure_nth(size, nth, || capture.prepare_current_semantic_alias_snapshot(request));
    assert!(allocations.injected_failure && allocations.matching_requests == nth, "{allocations:?}");
    assert!(result.is_err());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    drop(capture.prepare_current_semantic_alias_snapshot(request).unwrap());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_alias_snapshot_lookup_rechecks_cancellation_and_memory_without_io() {
  for cancel in [false, true] {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("alias-snapshot-lookup-check", None, [1; 16]);
    seed_plugin(&publisher, &fixtures::module("both"), "both");
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let source = br#"{"$v":1,"parsers":{"text/a":"parse"}}"#;
    let prepared =
      capture.prepare_current_semantic_alias_snapshot(snapshot_request(SemanticSourceAliasKindV1::ParserRegistry, Some(source))).unwrap();
    if cancel {
      cancellation.cancel();
    } else {
      memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
    }
    for alias in ["parse", "unprepared"] {
      let error = prepared.resolve_parser_alias(alias).unwrap_err();
      if cancel {
        assert!(matches!(error, SemanticCompilationErrorV1::Cancelled));
      } else {
        assert!(matches!(error, SemanticCompilationErrorV1::Resource { .. }));
      }
    }
    drop(prepared);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_alias_snapshot_final_checks_cover_present_missing_and_empty_sources() {
  for source in
    [None, Some(br#"{"$v":1,"parsers":{"text/a":"parse"}}"#.as_slice()), Some(br#"{"$v":1,"parsers":{"text/a":"absent"}}"#.as_slice())]
  {
    for cancel in [false, true] {
      let (_directory, path, _coordinator, publisher) = create_environment_for_database("alias-snapshot-final-check", None, [1; 16]);
      seed_plugin(&publisher, &fixtures::module("both"), "both");
      let before = fs::read(&path).unwrap();
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let request = snapshot_request(SemanticSourceAliasKindV1::ParserRegistry, source);
      let mut calls = 0;
      let result = capture.prepare_current_semantic_alias_snapshot_with_observer(request, || {
        calls += 1;
        assert!(publisher.root_state.try_lock().is_ok());
        assert!(publisher.kv.try_lock().is_ok());
        if cancel {
          cancellation.cancel();
        } else {
          memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
        }
      });
      assert_eq!(calls, 1);
      assert!(result.is_err());
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      if !cancel {
        memory.update_host_sample(HostMemorySample::default()).unwrap();
        drop(capture.prepare_current_semantic_alias_snapshot(request).unwrap());
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      }
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

#[test]
fn native_alias_snapshot_missing_module_never_becomes_prepared_absence() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("alias-snapshot-missing-module", None, [1; 16]);
  let module = fixtures::module("both");
  let alias = fixtures::alias(&module, "both");
  seed_files(&publisher, &[(fixtures::alias_path(), "application/octet-stream", &alias)]);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let source = br#"{"$v":1,"parsers":{"text/a":"parse"}}"#;
  let error = capture
    .prepare_current_semantic_alias_snapshot(snapshot_request(SemanticSourceAliasKindV1::ParserRegistry, Some(source)))
    .err()
    .expect("missing raw module must refuse");
  assert!(matches!(error, NativeSemanticPluginSourceErrorV1::Source(ref error) if error.code() == "semantic_plugin_source_module_missing"));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_alias_snapshot_exact_retained_limit_includes_both_copied_roles() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("alias-snapshot-exact-retained", None, [1; 16]);
  seed_plugin(&publisher, &fixtures::module("both"), "both");
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let source = br#"{"$v":1,"parser":"parse","indexes":[{"name":"x","type":"typed_exact_blake3_v1","source":{"plugin":"parse"}}]}"#;
  let mut request = snapshot_request(SemanticSourceAliasKindV1::IndexConfiguration, Some(source));
  let prepared = capture.prepare_current_semantic_alias_snapshot(request).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes - baseline;
  assert!(retained > 4096 && retained < 64 << 10);
  drop(prepared);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  request.maximum_snapshot_bytes = retained as usize;
  drop(capture.prepare_current_semantic_alias_snapshot(request).unwrap());
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  request.maximum_snapshot_bytes -= 1;
  assert!(matches!(
    capture.prepare_current_semantic_alias_snapshot(request),
    Err(NativeSemanticPluginSourceErrorV1::Identity(SemanticCompilationErrorV1::Resource { .. }))
  ));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  request.maximum_snapshot_bytes = usize::MAX;
  let prepared = capture.prepare_current_semantic_alias_snapshot(request).unwrap();
  assert_eq!(memory.snapshot().unwrap().reserved_bytes - baseline, retained);
  drop(prepared);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_alias_snapshot_empty_source_still_validates_pair_bounds_and_entry_admission() {
  for case in 0..6 {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("alias-snapshot-empty-admission", None, [1; 16]);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let mut request = snapshot_request(SemanticSourceAliasKindV1::ParserRegistry, None);
    match case {
      0 => request.plugins.maximum_module_bytes = 0,
      1 => request.plugins.maximum_chunk_entity_bytes = 0,
      2 => request.plugins.maximum_read_bytes = 0,
      3 => request.maximum_snapshot_bytes = 0,
      4 => cancellation.cancel(),
      5 => {
        memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
      }
      _ => unreachable!(),
    }
    assert!(capture.prepare_current_semantic_alias_snapshot_with_observer(request, || panic!("entry refusal reached completion")).is_err());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
