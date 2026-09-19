//! Selection changes input identity, never the shared artifact interpretation.
use super::*;

#[test]
fn native_selected_plugin_retained_pair_does_not_follow_the_current_alias() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("selected-plugin-retained", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    let old_module = fixtures::module("both");
    let new_module = fixtures::module("mapper");
    seed_plugin(&publisher, &old_module, "both");
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let original = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let old = original.read_protected_plugin_sources("parse", plugin_bounds()).unwrap().unwrap();
    let alias_revision = old.alias_source().revision().to_vec();
    let artifact_revision = old.artifact_source().revision().to_vec();
    for source in [old.alias_source(), old.artifact_source()] {
      let timestamp = publisher.observe().unwrap().selected.header.updated_at_ms + 1;
      source.stage_retained_copy(source_bounds(), timestamp).unwrap();
    }
    drop(old);
    drop(original);
    seed_plugin(&publisher, &new_module, "mapper");
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let mut paths = Vec::new();
    let selected = capture
      .read_plugin_sources_with_selected_reader(
        "parse",
        plugin_bounds(),
        |path, bounds| {
          paths.push(path.to_owned());
          let revision = if path == fixtures::alias_path() {
            &alias_revision
          } else {
            assert_eq!(path, fixtures::artifact_path(&old_module));
            &artifact_revision
          };
          capture.read_retained_protected_source(path, revision, bounds).map(Some)
        },
        || {},
      )
      .expect("selected inputs must use the shared pair reader")
      .unwrap();
    assert_eq!(paths, [fixtures::alias_path(), fixtures::artifact_path(&old_module)]);
    assert_eq!(selected.alias_source().revision(), alias_revision);
    assert_eq!(selected.artifact_source().revision(), artifact_revision);
    assert_eq!(selected.artifact_source().body(), old_module);
    assert_eq!(selected.dependency_bytes(SemanticSourceAliasRoleV1::Parser).unwrap(), expected_dependency(&old_module, 1));
    assert_eq!(selected.dependency_bytes(SemanticSourceAliasRoleV1::Mapper).unwrap(), expected_dependency(&old_module, 2));
    drop(selected);
    let current = capture.read_protected_plugin_sources("parse", plugin_bounds()).unwrap().unwrap();
    assert_eq!(current.artifact_source().body(), new_module);
    assert!(current.dependency_bytes(SemanticSourceAliasRoleV1::Parser).is_none());
    drop(current);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_selected_plugin_explicit_alias_absence_never_reads_current_sources() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("selected-plugin-absence", None, [1; 16]);
  seed_plugin(&publisher, &fixtures::module("both"), "both");
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let mut reads = 0;
  let mut completed = false;
  let result = capture
    .read_plugin_sources_with_selected_reader(
      "parse",
      plugin_bounds(),
      |path, _bounds| {
        assert_eq!(path, fixtures::alias_path());
        reads += 1;
        Ok(None)
      },
      || {
        completed = true;
      },
    )
    .unwrap();
  assert!(result.is_none());
  assert_eq!(reads, 1);
  assert!(completed);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_selected_plugin_artifact_error_is_preserved_and_fresh_selection_retries() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("selected-plugin-error", None, [1; 16]);
  let module = fixtures::module("both");
  seed_plugin(&publisher, &module, "both");
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let mut reads = 0;
  let error = capture
    .read_plugin_sources_with_selected_reader(
      "parse",
      plugin_bounds(),
      |path, bounds| {
        reads += 1;
        if path == fixtures::alias_path() {
          capture.read_protected_source(path, bounds)
        } else {
          Err(SemanticMutationObservationErrorV1::Invalid { code: "selected_fixture_failure", message: "selected artifact failed" })
        }
      },
      || panic!("failed selection cannot complete"),
    )
    .err()
    .expect("artifact selection must fail");
  assert!(matches!(error, NativeSemanticPluginSourceErrorV1::Source(ref source) if source.code() == "selected_fixture_failure"));
  assert_eq!(reads, 2);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  let retry = capture
    .read_plugin_sources_with_selected_reader("parse", plugin_bounds(), |path, bounds| capture.read_protected_source(path, bounds), || {})
    .unwrap()
    .unwrap();
  assert_eq!(retry.artifact_source().body(), module);
  drop(retry);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}
