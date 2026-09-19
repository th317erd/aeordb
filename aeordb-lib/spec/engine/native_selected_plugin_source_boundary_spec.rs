use super::*;
use crate::engine::memory_coordinator::HostMemorySample;
use crate::engine::v4::plugin_identity::plugin_alias_path_v1;

#[test]
fn native_selected_plugin_refuses_sources_from_another_capture_on_either_read() {
  for wrong_artifact in [false, true] {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("selected-plugin-capture", None, [1; 16]);
    let module = fixtures::module("both");
    seed_plugin(&publisher, &module, "both");
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let other = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let mut reads = 0;
    let error = capture
      .read_plugin_sources_with_selected_reader(
        "parse",
        plugin_bounds(),
        |path, bounds| {
          reads += 1;
          let is_artifact = path == fixtures::artifact_path(&module);
          if wrong_artifact == is_artifact {
            other.read_protected_source(path, bounds)
          } else {
            capture.read_protected_source(path, bounds)
          }
        },
        || panic!("mixed captures cannot complete"),
      )
      .err()
      .expect("cross-capture source must refuse");
    assert!(matches!(error, NativeSemanticPluginSourceErrorV1::Source(ref source) if source.code() == "semantic_plugin_source_capture"));
    assert_eq!(reads, if wrong_artifact { 2 } else { 1 });
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_selected_plugin_refuses_another_path_even_when_its_body_matches() {
  for wrong_artifact in [false, true] {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("selected-plugin-path", None, [1; 16]);
    let module = fixtures::module("both");
    seed_plugin(&publisher, &module, "both");
    let other_path =
      if wrong_artifact { fixtures::artifact_path(&fixtures::module("mapper")) } else { plugin_alias_path_v1("other").unwrap() };
    let body = if wrong_artifact { module.clone() } else { fixtures::alias(&module, "both") };
    seed_files(&publisher, &[(other_path.clone(), "application/octet-stream", &body)]);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let error = capture
      .read_plugin_sources_with_selected_reader(
        "parse",
        plugin_bounds(),
        |path, bounds| {
          let selected = if wrong_artifact == (path == fixtures::artifact_path(&module)) { &other_path } else { path };
          capture.read_protected_source(selected, bounds)
        },
        || panic!("wrong path cannot complete"),
      )
      .err()
      .expect("different source path must refuse");
    assert!(matches!(error, NativeSemanticPluginSourceErrorV1::Source(ref source) if source.code() == "semantic_plugin_source_path"));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_selected_plugin_selected_rows_still_obey_body_and_chunk_ceilings() {
  for too_many_chunks in [false, true] {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("selected-plugin-row-limit", None, [1; 16]);
    let source_path = fixtures::alias_path();
    if too_many_chunks {
      seed_raw_source(
        &publisher,
        &source_path,
        1,
        WHOLE_ENTITY_V1_FLAG_SYSTEM,
        WHOLE_ENTITY_V1_FLAG_SYSTEM,
        true,
        CompressionAlgorithm::None,
      );
    } else {
      seed_files(&publisher, &[(source_path, "application/octet-stream", &vec![b'x'; 17000])]);
    }
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let bounds = NativeSemanticPluginSourceBoundsV1 { maximum_source_chunks: 1, ..plugin_bounds() };
    let error = capture
      .read_plugin_sources_with_selected_reader(
        "parse",
        bounds,
        |path, _| capture.read_protected_source(path, source_bounds()),
        || panic!("oversized selected source cannot complete"),
      )
      .err()
      .expect("selected source must respect bounds before parsing");
    assert!(matches!(error, NativeSemanticPluginSourceErrorV1::Source(ref source) if source.code() == "semantic_source_body_bound"));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_selected_plugin_cancellation_and_pressure_after_selection_do_not_complete() {
  for cancel in [false, true] {
    for stage in ["absent-alias", "present-alias", "present-artifact"] {
      let (_directory, path, _coordinator, publisher) = create_environment_for_database("selected-plugin-late", None, [1; 16]);
      seed_plugin(&publisher, &fixtures::module("both"), "both");
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let before = fs::read(&path).unwrap();
      let retained = memory.snapshot().unwrap().reserved_bytes;
      let error = capture
        .read_plugin_sources_with_selected_reader(
          "parse",
          plugin_bounds(),
          |path, bounds| {
            let source = if stage == "absent-alias" { None } else { capture.read_protected_source(path, bounds)? };
            if stage != "present-artifact" || path != fixtures::alias_path() {
              if cancel {
                cancellation.cancel();
              } else {
                memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
              }
            }
            Ok(source)
          },
          || panic!("late refusal cannot reach completion observer"),
        )
        .err()
        .expect("late cancellation or pressure must refuse");
      assert!(matches!(error, NativeSemanticPluginSourceErrorV1::Source(ref source) if source.code() == if cancel {
        "semantic_task_observation_cancelled"
      } else { "semantic_task_observation_memory" }));
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      if !cancel {
        memory.update_host_sample(HostMemorySample::default()).unwrap();
        drop(capture.read_protected_plugin_sources("parse", plugin_bounds()).unwrap().unwrap());
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      }
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

#[test]
fn native_selected_plugin_artifact_absence_is_an_error_not_an_absent_alias() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("selected-plugin-module-absent", None, [1; 16]);
  seed_plugin(&publisher, &fixtures::module("both"), "both");
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let error = capture
    .read_plugin_sources_with_selected_reader(
      "parse",
      plugin_bounds(),
      |path, bounds| if path == fixtures::alias_path() { capture.read_protected_source(path, bounds) } else { Ok(None) },
      || panic!("absent artifact cannot complete"),
    )
    .err()
    .expect("missing artifact must refuse");
  assert!(
    matches!(error, NativeSemanticPluginSourceErrorV1::Source(ref source) if source.code() == "semantic_plugin_source_module_missing")
  );
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}
