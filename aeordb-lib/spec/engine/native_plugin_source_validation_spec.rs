use super::*;
use crate::engine::memory_coordinator::HostMemorySample;
use crate::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1;

#[test]
fn native_plugin_sources_validate_names_and_all_bounds_even_for_absence() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("plugin-pair-admission", None, [1; 16]);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  for alias in ["", "bad\nname", "bad\0name", &"x".repeat(4097)] {
    assert!(capture.read_protected_plugin_sources(alias, plugin_bounds()).is_err());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  }
  for alias in ["folder/parse 💾", &"x".repeat(4096)] {
    assert!(capture.read_protected_plugin_sources(alias, plugin_bounds()).unwrap().is_none());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  }
  for bounds in [
    NativeSemanticPluginSourceBoundsV1 { maximum_module_bytes: 0, ..plugin_bounds() },
    NativeSemanticPluginSourceBoundsV1 { maximum_module_bytes: (64 << 20) + 1, ..plugin_bounds() },
    NativeSemanticPluginSourceBoundsV1 { maximum_chunk_entity_bytes: 0, ..plugin_bounds() },
    NativeSemanticPluginSourceBoundsV1 { maximum_chunk_entity_bytes: usize::MAX, ..plugin_bounds() },
    NativeSemanticPluginSourceBoundsV1 { maximum_source_chunks: 0, ..plugin_bounds() },
    NativeSemanticPluginSourceBoundsV1 { maximum_read_bytes: 0, ..plugin_bounds() },
    NativeSemanticPluginSourceBoundsV1 { maximum_workspace_bytes: (48 << 10) - 1, ..plugin_bounds() },
  ] {
    assert!(capture.read_protected_plugin_sources("parse", bounds).is_err(), "{bounds:?}");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  }
  assert!(capture
    .read_protected_plugin_sources("parse", NativeSemanticPluginSourceBoundsV1 { maximum_workspace_bytes: 48 << 10, ..plugin_bounds() })
    .unwrap()
    .is_none());
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_plugin_sources_missing_module_and_bad_identities_are_errors_not_absence() {
  for case in [
    "missing-module",
    "alias-crc",
    "alias-name",
    "alias-oversize",
    "fingerprint",
    "length",
    "metadata",
    "legacy",
    "framing",
    "duplicate",
    "role",
  ] {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("plugin-pair-invalid", None, [1; 16]);
    let mut module = fixtures::module("both");
    if case == "framing" {
      module.push(0);
    }
    if case == "duplicate" {
      fixtures::custom("aeordb.plugin.v1", &fixtures::manifest("both"), &mut module);
    }
    if case == "role" {
      module = b"\0asm\x01\0\0\0".to_vec();
      let mut manifest = fixtures::manifest("both");
      let end = manifest.len();
      manifest[end - 8] = 3;
      fixtures::custom("aeordb.plugin.v1", &manifest, &mut module);
    }
    let mut alias = fixtures::alias(&module, "both");
    match case {
      "alias-crc" => alias[80] ^= 1,
      "alias-oversize" => alias = vec![0; 16_773],
      "alias-name" | "fingerprint" | "length" | "metadata" | "legacy" => {
        let offset = match case {
          "alias-name" => 128,
          "fingerprint" => 40,
          "length" => 72,
          "metadata" => 134,
          _ => 12,
        };
        alias[offset] ^= if case == "legacy" { 4 } else { 1 };
        fixtures::seal(&mut alias);
      }
      _ => {}
    }
    let artifact_path = if case == "fingerprint" {
      let digest = &alias[40..72];
      let mut value = String::from("/.aeordb-system/plugin-artifacts/blake3/");
      for byte in digest {
        use std::fmt::Write;
        write!(value, "{byte:02x}").unwrap();
      }
      value
    } else {
      fixtures::artifact_path(&module)
    };
    let mut files = vec![(fixtures::alias_path(), "application/octet-stream", alias.as_slice())];
    if case != "missing-module" {
      files.push((artifact_path, "application/wasm", module.as_slice()));
    }
    seed_files(&publisher, &files);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let error = capture.read_protected_plugin_sources("parse", plugin_bounds()).err().expect(case);
    if case == "missing-module" {
      assert!(
        matches!(error, NativeSemanticPluginSourceErrorV1::Source(ref error) if error.code() == "semantic_plugin_source_module_missing")
      );
    }
    if case == "legacy" {
      assert!(matches!(error, NativeSemanticPluginSourceErrorV1::Identity(SemanticCompilationErrorV1::DependencyUnavailable { .. })));
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained, "{case}");
    assert_eq!(fs::read(&path).unwrap(), before, "{case}");
  }
}

#[test]
fn native_plugin_sources_one_cumulative_physical_read_budget_covers_both_records_and_chunks() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("plugin-pair-budget", None, [1; 16], algorithm, 0);
    let module = fixtures::module("both");
    let alias = fixtures::alias(&module, "both");
    seed_plugin(&publisher, &module, "both");
    // Independently sum the physical locators of both FileRecords and their
    // single seeded chunks; neither the pair reader nor its counter is the oracle.
    let kv = publisher.lock_kv().unwrap();
    let mut total = 0u64;
    for (name, body) in [(fixtures::alias_path(), alias.as_slice()), (fixtures::artifact_path(&module), module.as_slice())] {
      for key in [first_authority_file_path_hash(&name, algorithm), first_authority_system_chunk_hash(body, algorithm)] {
        total += u64::from(kv.get(&key).unwrap().unwrap().total_length);
      }
    }
    drop(kv);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let limited = NativeSemanticPluginSourceBoundsV1 { maximum_read_bytes: total - 1, ..plugin_bounds() };
    let error = capture.read_protected_plugin_sources("parse", limited).err().expect("one byte short must refuse");
    assert!(matches!(error, NativeSemanticPluginSourceErrorV1::Source(ref error) if error.code() == "semantic_source_read_bound"));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    let exact = NativeSemanticPluginSourceBoundsV1 {
      maximum_read_bytes: total,
      maximum_module_bytes: module.len(),
      maximum_workspace_bytes: 48 << 10,
      maximum_source_chunks: 1,
      ..plugin_bounds()
    };
    drop(capture.read_protected_plugin_sources("parse", exact).unwrap().unwrap());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert!(capture
      .read_protected_plugin_sources("parse", NativeSemanticPluginSourceBoundsV1 { maximum_module_bytes: module.len() - 1, ..exact })
      .is_err());
    assert!(capture
      .read_protected_plugin_sources("parse", NativeSemanticPluginSourceBoundsV1 { maximum_chunk_entity_bytes: 1, ..exact })
      .is_err());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_plugin_sources_final_cancellation_and_pressure_cover_present_and_absent_pairs() {
  for present in [false, true] {
    for cancel in [false, true] {
      let (_directory, path, _coordinator, publisher) = create_environment_for_database("plugin-pair-final-admission", None, [1; 16]);
      if present {
        seed_plugin(&publisher, &fixtures::module("both"), "both");
      }
      let before = fs::read(&path).unwrap();
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let retained = memory.snapshot().unwrap().reserved_bytes;
      let mut calls = 0;
      let result = capture.read_protected_plugin_sources_with_observer("parse", plugin_bounds(), || {
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
      let error = result.err().expect("final admission must refuse");
      assert!(
        matches!(error, NativeSemanticPluginSourceErrorV1::Source(ref error) if error.code() == if cancel { "semantic_task_observation_cancelled" } else { "semantic_task_observation_memory" })
      );
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      if !cancel {
        memory.update_host_sample(HostMemorySample::default()).unwrap();
        assert_eq!(capture.read_protected_plugin_sources("parse", plugin_bounds()).unwrap().is_some(), present);
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      }
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

#[test]
fn native_plugin_sources_path_body_and_dependency_allocation_refusals_release_then_retry() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("plugin-pair-allocation", None, [1; 16]);
  let mut module = fixtures::module("both");
  fixtures::custom("padding", &vec![0; 8192], &mut module);
  seed_plugin(&publisher, &module, "both");
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  drop(capture.read_protected_plugin_sources("parse", plugin_bounds()).unwrap());
  let retained = memory.snapshot().unwrap().reserved_bytes;
  for size in [
    fixtures::alias_path().len(),
    fixtures::artifact_path(&module).len(),
    fixtures::alias(&module, "both").len(),
    module.len(),
    expected_dependency(&module, 1).len(),
  ] {
    let (result, allocations) = measure(size, || capture.read_protected_plugin_sources("parse", plugin_bounds()));
    assert!(allocations.injected_failure, "{size}: {allocations:?}");
    let error = result.err().expect("actual allocator refusal must not return a pair");
    assert!(error.to_string().contains("allocation"), "{size}: {error}");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    drop(capture.read_protected_plugin_sources("parse", plugin_bounds()).unwrap().unwrap());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  }
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_plugin_sources_identity_records_are_not_core_bytecode_or_executor_admission() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("plugin-pair-not-executor", None, [1; 16]);
  let mut module = b"\0asm\x01\0\0\0".to_vec();
  fixtures::custom("aeordb.plugin.v1", &fixtures::manifest("parser"), &mut module);
  module.extend_from_slice(&[10, 4, 1, 2, 0, 0xff]);
  assert!(wasmi::Module::validate(&wasmi::Engine::default(), &module).is_err());
  seed_plugin(&publisher, &module, "parser");
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let pair = capture.read_protected_plugin_sources("parse", plugin_bounds()).unwrap().unwrap();
  assert_eq!(pair.artifact_source().body(), module);
  let record = pair.dependency_record(SemanticSourceAliasRoleV1::Parser).unwrap().unwrap();
  assert_eq!((record.executor_profile, record.abi), (2, 3));
  assert!(pair.dependency_record(SemanticSourceAliasRoleV1::Mapper).unwrap().is_none());
  assert_eq!(fs::read(&path).unwrap(), before);
}
