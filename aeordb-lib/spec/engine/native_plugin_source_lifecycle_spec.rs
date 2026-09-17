//! Additional native capture, lifecycle and partial-allocation boundaries.
use super::*;
use crate::engine::memory_coordinator::HostMemorySample;
use crate::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1;

#[test]
fn native_plugin_sources_large_admission_ceilings_do_not_reserve_maximum_sized_bodies() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("plugin-pair-small-body", None, [1; 16]);
  let module = fixtures::module("both");
  seed_plugin(&publisher, &module, "both");
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let bounds = NativeSemanticPluginSourceBoundsV1 {
    maximum_module_bytes: 64 << 20,
    maximum_chunk_entity_bytes: (64 << 20) + 8192,
    maximum_source_chunks: u64::MAX,
    maximum_read_bytes: u64::MAX,
    maximum_workspace_bytes: usize::MAX,
  };
  let pair = capture.read_protected_plugin_sources("parse", bounds).unwrap().unwrap();
  assert!(memory.snapshot().unwrap().reserved_bytes - retained < 1 << 20);
  assert_eq!(pair.artifact_source().body(), module);
  drop(pair);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_plugin_sources_module_lookup_remains_captured_after_same_path_replacement() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("plugin-module-replacement", None, [1; 16], algorithm, 0);
    let module = fixtures::module("both");
    seed_plugin(&publisher, &module, "both");
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let old = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let mut changed = module.clone();
    changed.push(0);
    seed_files(&publisher, &[(fixtures::artifact_path(&module), "application/wasm", &changed)]);
    let fresh = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let pair = old.read_protected_plugin_sources("parse", plugin_bounds()).unwrap().unwrap();
    assert_eq!(pair.artifact_source().body(), module);
    drop(pair);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert!(matches!(
      fresh.read_protected_plugin_sources("parse", plugin_bounds()),
      Err(NativeSemanticPluginSourceErrorV1::Identity(SemanticCompilationErrorV1::InvalidSource { .. }))
    ));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_plugin_sources_entry_cancellation_and_pressure_stop_before_completion() {
  for cancel in [false, true] {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("plugin-pair-entry-admission", None, [1; 16]);
    seed_plugin(&publisher, &fixtures::module("both"), "both");
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    if cancel {
      cancellation.cancel();
    } else {
      memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
    }
    let error = capture
      .read_protected_plugin_sources_with_observer("parse", plugin_bounds(), || panic!("entry refusal reached completion"))
      .err()
      .expect("entry must refuse");
    assert!(
      matches!(error, NativeSemanticPluginSourceErrorV1::Source(ref error) if error.code() == if cancel { "semantic_task_observation_cancelled" } else { "semantic_task_observation_memory" })
    );
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    if !cancel {
      memory.update_host_sample(HostMemorySample::default()).unwrap();
      drop(capture.read_protected_plugin_sources("parse", plugin_bounds()).unwrap().unwrap());
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    }
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_plugin_sources_physical_alias_and_module_failures_are_not_absence() {
  for case in ["alias-record-role", "module-record-role", "module-chunk-integrity"] {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("plugin-pair-physical", None, [1; 16]);
    let module = fixtures::module("both");
    seed_plugin(&publisher, &module, "both");
    if case == "module-chunk-integrity" {
      corrupt_last_entity_byte(&publisher, &first_authority_system_chunk_hash(&module, HashAlgorithm::Blake3_256));
    } else {
      let name = if case == "alias-record-role" { fixtures::alias_path() } else { fixtures::artifact_path(&module) };
      let key = first_authority_file_path_hash(&name, HashAlgorithm::Blake3_256);
      let mut kv = publisher.lock_kv().unwrap();
      let mut locator = kv.get(&key).unwrap().unwrap();
      locator.type_flags = KV_TYPE_CHUNK;
      kv.insert(locator).unwrap();
      kv.force_flush_hot_buffer().unwrap();
    }
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let error = capture.read_protected_plugin_sources("parse", plugin_bounds()).err().expect(case);
    assert!(
      matches!(error, NativeSemanticPluginSourceErrorV1::Source(ref error) if error.code() == if case == "module-chunk-integrity" { "integrity_hash_mismatch" } else { "semantic_source_record_role" })
    );
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_plugin_sources_second_role_allocation_refusal_releases_the_first_record() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("plugin-pair-second-allocation", None, [1; 16]);
  let module = fixtures::module("both");
  seed_plugin(&publisher, &module, "both");
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let size = expected_dependency(&module, 1).len();
  assert_eq!(size, expected_dependency(&module, 2).len());
  let (result, allocations) = allocation_probe::measure_nth(size, 2, || capture.read_protected_plugin_sources("parse", plugin_bounds()));
  assert!(allocations.injected_failure && allocations.matching_requests == 2, "{allocations:?}");
  let error = result.err().expect("second role allocation must refuse");
  assert!(error.to_string().contains("dependency_writer_allocation"), "{error}");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  drop(capture.read_protected_plugin_sources("parse", plugin_bounds()).unwrap().unwrap());
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}
