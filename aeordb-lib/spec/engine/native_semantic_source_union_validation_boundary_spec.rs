use super::*;
#[path = "native_semantic_source_union_validation_resource_spec.rs"]
mod resource;
use std::path::Path;

fn with_validation_capture(
  publisher: &V4FirstAuthorityPublisher,
  path: &Path,
  check: impl FnOnce(&NativeSemanticMutationInventoryV1<'_>, &MemoryCoordinator, &CancellationToken),
) {
  let before = fs::read(path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  check(&capture, &memory, &cancellation);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(path).unwrap(), before);
}

fn validation_error_code(error: NativeSemanticSourceUnionErrorV1) -> &'static str {
  match error {
    NativeSemanticSourceUnionErrorV1::Source(error) => error.code(),
    NativeSemanticSourceUnionErrorV1::RootAuthority(error) => error.code(),
    NativeSemanticSourceUnionErrorV1::Namespace(error) => error.code(),
    NativeSemanticSourceUnionErrorV1::Plugin(NativeSemanticPluginSourceErrorV1::Source(error)) => error.code(),
    error => panic!("expected concrete source error, got {error:?}"),
  }
}

fn seed_validation_module(publisher: &V4FirstAuthorityPublisher, module: &[u8]) -> (Vec<u8>, Vec<u8>) {
  let alias = plugin_fixtures::alias(module, "both");
  seed_files(
    publisher,
    &[
      (plugin_fixtures::artifact_path(module), "application/wasm", module),
      (plugin_fixtures::alias_path(), "application/octet-stream", &alias),
    ],
  );
  (
    seed_retained_revision(publisher, &plugin_fixtures::alias_path()),
    seed_retained_revision(publisher, &plugin_fixtures::artifact_path(module)),
  )
}

#[test]
fn retained_source_union_validation_requires_both_plugin_revisions_for_removed_and_added_references() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("retained-union-plugins", None, [1; 16], algorithm, 0);
    let initial = request_for_database_and_algorithm([1; 16], algorithm);
    let base = publisher.publish(&initial).unwrap().namespace_root.root_hash;
    let old_module = plugin_fixtures::module("both");
    let mut new_module = old_module.clone();
    plugin_fixtures::custom("fixture-revision", b"requested", &mut new_module);
    let (old_alias, old_artifact) = seed_validation_module(&publisher, &old_module);
    let (new_alias, new_artifact) = seed_validation_module(&publisher, &new_module);
    let registry = br#"{"$v":1,"parsers":{"text/plain":"parse"}}"#;
    seed_files(&publisher, &[(PARSER_SOURCE.to_string(), "application/json", registry)]);
    let parser_revision = seed_retained_revision(&publisher, PARSER_SOURCE);
    let configuration =
      br#"{"$v":1,"parser":"parse","indexes":[{"name":"value","type":"typed_exact_blake3_v1","source":{"plugin":"parse"}}]}"#;
    let (namespace, _) = namespace_configuration_tree(&publisher, "/z", configuration, vec![]);
    let requested_tree = publish_namespace_directory(&publisher, vec![namespace_directory_child("z", namespace)]);
    let mut base_sources = absent_globals();
    base_sources.insert(PARSER_SOURCE.to_string(), Some(parser_revision));
    base_sources.insert(plugin_fixtures::alias_path(), Some(old_alias));
    base_sources.insert(plugin_fixtures::artifact_path(&old_module), Some(old_artifact));
    base_sources.insert(plugin_fixtures::artifact_path(&new_module), None);
    let mut requested_sources = base_sources.clone();
    requested_sources.insert(PARSER_SOURCE.to_string(), None);
    requested_sources.insert(plugin_fixtures::alias_path(), Some(new_alias));
    requested_sources.insert(plugin_fixtures::artifact_path(&old_module), None);
    requested_sources.insert(plugin_fixtures::artifact_path(&new_module), Some(new_artifact));
    let mut fingerprint = base_sources.clone();
    fingerprint.insert("/z/.aeordb-config/indexes.json".to_string(), None);
    seed_validation_pair(&publisher, &base, &requested_tree, &base_sources, &requested_sources, &fingerprint, 1);
    seed_files(&publisher, &[(plugin_fixtures::alias_path(), "application/octet-stream", b"invalid unrelated current alias")]);
    drop(publisher);
    let (_coordinator, publisher) = reopen(&path);
    with_validation_capture(&publisher, &path, |capture, _, _| {
      let result = capture.validate_captured_semantic_source_union(&[2; 16], 1, validation_bounds(&requested_tree)).unwrap();
      assert_eq!((result.protected_paths, result.namespace_paths), (5, 1));
      assert_eq!((result.base_configuration_count, result.requested_configuration_count), (0, 1));
    });
  }
}

#[test]
fn retained_source_union_validation_rejects_omitted_required_aliases_and_modules() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for case in ["unlisted-alias", "absent-alias", "unlisted-module", "absent-module"] {
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("retained-union-membership", None, [1; 16], algorithm, 0);
      let initial = request_for_database_and_algorithm([1; 16], algorithm);
      let base = publisher.publish(&initial).unwrap().namespace_root.root_hash;
      let module = plugin_fixtures::module("both");
      let (alias, _) = seed_validation_module(&publisher, &module);
      let registry = br#"{"$v":1,"parsers":{"text/plain":"parse"}}"#;
      seed_files(&publisher, &[(PARSER_SOURCE.to_string(), "application/json", registry)]);
      let parser_revision = seed_retained_revision(&publisher, PARSER_SOURCE);
      let mut sources = absent_globals();
      sources.insert(PARSER_SOURCE.to_string(), Some(parser_revision));
      match case {
        "unlisted-alias" => {}
        "absent-alias" => {
          sources.insert(plugin_fixtures::alias_path(), None);
        }
        "unlisted-module" => {
          sources.insert(plugin_fixtures::alias_path(), Some(alias));
        }
        "absent-module" => {
          sources.insert(plugin_fixtures::alias_path(), Some(alias));
          sources.insert(plugin_fixtures::artifact_path(&module), None);
        }
        _ => unreachable!(),
      }
      let mut requested = sources.clone();
      requested.insert(PARSER_SOURCE.to_string(), None);
      seed_validation_pair(&publisher, &base, &initial.namespace_tree.root_hash, &sources, &requested, &sources, 0);
      with_validation_capture(&publisher, &path, |capture, _, _| {
        let result = capture.validate_captured_semantic_source_union(&[2; 16], 1, validation_bounds(&initial.namespace_tree.root_hash));
        if case == "absent-alias" {
          assert_eq!(result.unwrap().requested_configuration_count, 0, "explicit absence must ignore the current alias");
        } else {
          let expected = if case == "absent-module" { "semantic_plugin_source_module_missing" } else { "semantic_source_catalog_unlisted" };
          assert_eq!(validation_error_code(result.unwrap_err()), expected, "{algorithm:?}/{case}");
        }
      });
    }
  }
}

#[test]
fn retained_source_union_validation_checks_namespace_inclusive_fingerprint_and_actual_final_count() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for wrong_count in [false, true] {
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("retained-union-count-fingerprint", None, [1; 16], algorithm, 0);
      let initial = request_for_database_and_algorithm([1; 16], algorithm);
      let base = publisher.publish(&initial).unwrap().namespace_root.root_hash;
      let (namespace, _) = namespace_configuration_tree(&publisher, "/a", br#"{"$v":1,"indexes":[]}"#, vec![]);
      let requested = publish_namespace_directory(&publisher, vec![namespace_directory_child("a", namespace)]);
      let globals = absent_globals();
      let mut fingerprint = globals.clone();
      if wrong_count {
        fingerprint.insert("/a/.aeordb-config/indexes.json".to_string(), None);
      }
      seed_validation_pair(&publisher, &base, &requested, &globals, &globals, &fingerprint, if wrong_count { 0 } else { 1 });
      with_validation_capture(&publisher, &path, |capture, _, _| {
        let error = capture.validate_captured_semantic_source_union(&[2; 16], 1, validation_bounds(&requested)).unwrap_err();
        assert_eq!(
          validation_error_code(error),
          if wrong_count { "semantic_source_union_configuration_count" } else { "semantic_source_union_fingerprint" }
        );
      });
    }
  }
}

#[test]
fn retained_source_union_validation_missing_and_damaged_base_metadata_never_become_absence() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for kind in ["root", "tree", "state", "admission"] {
      for missing in [false, true] {
        let (_directory, path, _coordinator, publisher) =
          create_environment_for_algorithm_at_kv_stage("retained-union-metadata", None, [1; 16], algorithm, 0);
        let initial = request_for_database_and_algorithm([1; 16], algorithm);
        let base = publisher.publish(&initial).unwrap().namespace_root.root_hash;
        let globals = absent_globals();
        seed_validation_pair(&publisher, &base, &initial.namespace_tree.root_hash, &globals, &globals, &globals, 0);
        let key = match kind {
          "root" => base.clone(),
          "tree" => initial.namespace_tree.root_hash.clone(),
          "state" => {
            first_authority_file_path_hash(&semantic_object_path(algorithm, 1, &initial.semantic_state.object_id).unwrap(), algorithm)
          }
          "admission" => first_authority_file_path_hash(
            &system_control_path(SystemControlKindV1::RootAdmissionCommit, &base, SystemControlSlotV1::Immutable).unwrap(),
            algorithm,
          ),
          _ => unreachable!(),
        };
        if missing {
          assert!(publisher.lock_kv().unwrap().mark_deleted(&key).unwrap());
          seed_files(&publisher, &[]);
        } else {
          corrupt_last_entity_byte(&publisher, &key);
        }
        with_validation_capture(&publisher, &path, |capture, _, _| {
          let error =
            capture.validate_captured_semantic_source_union(&[2; 16], 1, validation_bounds(&initial.namespace_tree.root_hash)).unwrap_err();
          let expected = if !missing {
            "integrity_hash_mismatch"
          } else {
            match kind {
              "root" => "semantic_source_union_base_missing",
              "tree" => "semantic_namespace_source_directory_missing",
              _ => "missing_immutable_reference",
            }
          };
          assert_eq!(validation_error_code(error), expected, "{algorithm:?}/{kind}/{missing}");
        });
      }
    }
  }
}

#[test]
fn retained_source_union_validation_checks_historical_admission_database_and_sequence() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for future in [false, true] {
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("retained-union-admission", None, [1; 16], algorithm, 0);
      let initial = request_for_database_and_algorithm([1; 16], algorithm);
      let published = publisher.publish(&initial).unwrap();
      let base = published.namespace_root.root_hash;
      let globals = absent_globals();
      seed_validation_pair(&publisher, &base, &initial.namespace_tree.root_hash, &globals, &globals, &globals, 0);
      let mut admission = decode_root_admission_commit(&published.admission_control, algorithm).unwrap();
      if future {
        admission.selected_header_slot_sequence = u64::MAX;
      } else {
        admission.database_id = [0x32; 16];
      }
      let bytes = encode_root_admission_commit_control(&admission, algorithm).unwrap();
      seed(&publisher, &[(SystemControlKindV1::RootAdmissionCommit, &base, SystemControlSlotV1::Immutable, &bytes)]);
      with_validation_capture(&publisher, &path, |capture, _, _| {
        let error =
          capture.validate_captured_semantic_source_union(&[2; 16], 1, validation_bounds(&initial.namespace_tree.root_hash)).unwrap_err();
        assert_eq!(
          validation_error_code(error),
          if future { "captured_authority_admission_sequence" } else { "root_admission_database_mismatch" }
        );
      });
    }
  }
}

#[test]
fn retained_source_union_validation_validates_unused_declared_rows_and_exact_paired_paths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for case in ["valid-extra", "missing-extra", "different-paths", "missing-global"] {
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("retained-union-all-declared", None, [1; 16], algorithm, 0);
      let initial = request_for_database_and_algorithm([1; 16], algorithm);
      let base = publisher.publish(&initial).unwrap().namespace_root.root_hash;
      let mut sources = absent_globals();
      let alias_path = plugin_fixtures::alias_path();
      sources.insert(alias_path.clone(), if case == "missing-extra" { Some(vec![0x77; algorithm.hash_length()]) } else { None });
      if case == "missing-global" {
        sources.remove(INDEX_SOURCE);
      }
      let mut requested = sources.clone();
      if case == "different-paths" {
        requested.remove(&alias_path);
        requested.insert("/.aeordb-system/plugin-aliases/other".to_string(), None);
      }
      seed_validation_pair(&publisher, &base, &initial.namespace_tree.root_hash, &sources, &requested, &sources, 0);
      with_validation_capture(&publisher, &path, |capture, _, _| {
        let result = capture.validate_captured_semantic_source_union(&[2; 16], 1, validation_bounds(&initial.namespace_tree.root_hash));
        if case == "valid-extra" {
          assert_eq!(result.unwrap().protected_paths, 3);
        } else {
          let expected = match case {
            "missing-extra" => "semantic_source_retained_missing",
            "different-paths" => "semantic_source_catalog_paths",
            "missing-global" => "semantic_source_catalog_required_paths",
            _ => unreachable!(),
          };
          assert_eq!(validation_error_code(result.unwrap_err()), expected, "{algorithm:?}/{case}");
        }
      });
    }
  }
}

#[test]
fn retained_source_union_validation_rejects_empty_or_malformed_configuration_bytes() {
  let algorithm = HashAlgorithm::Blake3_256;
  for kind in ["global-index", "global-parser", "namespace"] {
    for bytes in [b"".as_slice(), b"{".as_slice()] {
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("retained-union-invalid-config", None, [1; 16], algorithm, 0);
      let initial = request_for_database_and_algorithm([1; 16], algorithm);
      let base = publisher.publish(&initial).unwrap().namespace_root.root_hash;
      let mut sources = absent_globals();
      let mut requested_tree = initial.namespace_tree.root_hash;
      let fingerprint = if kind == "namespace" {
        let (namespace, _) = namespace_configuration_tree(&publisher, "/a", bytes, vec![]);
        requested_tree = publish_namespace_directory(&publisher, vec![namespace_directory_child("a", namespace)]);
        let mut fingerprint = sources.clone();
        fingerprint.insert("/a/.aeordb-config/indexes.json".to_string(), None);
        fingerprint
      } else {
        let source_path = if kind == "global-index" { INDEX_SOURCE } else { PARSER_SOURCE };
        seed_files(&publisher, &[(source_path.to_string(), "application/json", bytes)]);
        sources.insert(source_path.to_string(), Some(seed_retained_revision(&publisher, source_path)));
        sources.clone()
      };
      seed_validation_pair(&publisher, &base, &requested_tree, &sources, &sources, &fingerprint, u64::from(kind != "global-parser"));
      with_validation_capture(&publisher, &path, |capture, _, _| {
        let error = capture.validate_captured_semantic_source_union(&[2; 16], 1, validation_bounds(&requested_tree)).unwrap_err();
        assert!(
          matches!(
            error,
            NativeSemanticSourceUnionErrorV1::Compilation(
              crate::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1::InvalidSource { .. }
            )
          ),
          "{kind}: {error:?}"
        );
      });
    }
  }
}
