use super::*;
#[path = "native_semantic_source_union_integration_spec.rs"]
mod integration;
use std::cell::Cell;
use crate::engine::memory_coordinator::HostMemorySample;
use std::path::Path;

fn with_union_fixture(
  algorithm: HashAlgorithm,
  globals: &[(String, &str, &[u8])],
  test: impl FnOnce(
    &V4FirstAuthorityPublisher,
    &NativeSemanticMutationInventoryV1<'_>,
    &MemoryCoordinator,
    &CancellationToken,
    &[u8],
    &[u8],
    &Path,
  ),
) {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("source-union-boundary", None, [1; 16], algorithm, 0);
  let initial = request_for_database_and_algorithm([1; 16], algorithm);
  let root = publisher.publish(&initial).unwrap().namespace_root.root_hash;
  seed_files(&publisher, globals);
  seed_union_generation(&publisher);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let before = fs::read(&path).unwrap();
  let parent = tempfile::tempdir().unwrap();
  test(&publisher, &capture, &memory, &cancellation, &root, &initial.namespace_tree.root_hash, parent.path());
  assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}

fn empty_union_request<'a>(root: &'a [u8], tree: &'a [u8], parent: &'a Path) -> NativeSemanticSourceUnionRequestV1<'a> {
  NativeSemanticSourceUnionRequestV1 {
    expected_base_root: root,
    requested_directory_root: tree,
    replacements: &[],
    workspace_parent: parent,
    bounds: union_bounds(tree),
  }
}

fn source_code(error: NativeSemanticSourceUnionErrorV1) -> &'static str {
  match error {
    NativeSemanticSourceUnionErrorV1::Source(error) => error.code(),
    NativeSemanticSourceUnionErrorV1::Namespace(error) => error.code(),
    NativeSemanticSourceUnionErrorV1::Plugin(NativeSemanticPluginSourceErrorV1::Source(error)) => error.code(),
    error => panic!("expected source error, got {error:?}"),
  }
}

#[test]
fn native_semantic_source_union_replacement_metadata_is_checked_before_callbacks() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    with_union_fixture(algorithm, &[], |_, capture, _, _, root, tree, parent| {
      let width = algorithm.hash_length();
      let zero = vec![0; width];
      let short = vec![3; width - 1];
      let cases = [
        (
          vec![
            NativeSemanticSourceReplacementV1 { path: PARSER_SOURCE, file_record_id: None },
            NativeSemanticSourceReplacementV1 { path: INDEX_SOURCE, file_record_id: None },
          ],
          "semantic_source_union_replacement_order",
        ),
        (
          vec![NativeSemanticSourceReplacementV1 { path: INDEX_SOURCE, file_record_id: None }; 2],
          "semantic_source_union_replacement_order",
        ),
        (
          vec![NativeSemanticSourceReplacementV1 { path: INDEX_SOURCE, file_record_id: Some(&zero) }],
          "semantic_source_union_replacement_identity",
        ),
        (
          vec![NativeSemanticSourceReplacementV1 { path: INDEX_SOURCE, file_record_id: Some(&short) }],
          "semantic_source_union_replacement_identity",
        ),
        (vec![NativeSemanticSourceReplacementV1 { path: "/ordinary.txt", file_record_id: None }], "semantic_source_family"),
      ];
      for (replacements, expected) in cases {
        let mut request = empty_union_request(root, tree, parent);
        request.replacements = &replacements;
        let error = capture
          .prepare_semantic_source_union(
            request,
            |_, _| panic!("invalid metadata emitted catalog"),
            |_, _, _| panic!("invalid metadata visited source"),
          )
          .err()
          .unwrap();
        assert_eq!(source_code(error), expected);
      }
    });
  }
}

#[test]
fn native_semantic_source_union_missing_revision_never_falls_back_to_current() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    with_union_fixture(
      algorithm,
      &[(INDEX_SOURCE.to_owned(), "application/json", br#"{"$v":1,"indexes":[]}"#)],
      |_, capture, _, _, root, tree, parent| {
        assert!(capture.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().is_some());
        let missing = vec![0x79; algorithm.hash_length()];
        let replacements = [NativeSemanticSourceReplacementV1 { path: INDEX_SOURCE, file_record_id: Some(&missing) }];
        let mut request = empty_union_request(root, tree, parent);
        request.replacements = &replacements;
        let error = capture
          .prepare_semantic_source_union(
            request,
            |_, _| panic!("missing revision emitted catalog"),
            |_, _, _| panic!("missing revision visited source"),
          )
          .err()
          .unwrap();
        assert_eq!(source_code(error), "semantic_source_retained_missing");
        drop(capture.prepare_semantic_source_union(empty_union_request(root, tree, parent), |_, _| Ok(()), |_, _, _| Ok(())).unwrap());
      },
    );
  }
}

#[test]
fn native_semantic_source_union_preserves_callback_errors_and_releases_workspaces() {
  for from_catalog in [false, true] {
    with_union_fixture(HashAlgorithm::Blake3_256, &[], |publisher, capture, _, _, root, tree, parent| {
      let visits = Cell::new(0);
      let emissions = Cell::new(0);
      let failure = || {
        NativeSemanticSourceUnionErrorV1::Source(SemanticMutationObservationErrorV1::Invalid {
          code: "union_test_callback",
          message: "caller sentinel",
        })
      };
      let error = capture
        .prepare_semantic_source_union(
          empty_union_request(root, tree, parent),
          |_, _| {
            emissions.set(emissions.get() + 1);
            assert!(publisher.root_state.try_lock().is_ok());
            assert!(publisher.kv.try_lock().is_ok());
            assert!(from_catalog);
            Err(failure())
          },
          |_, _, _| {
            visits.set(visits.get() + 1);
            assert!(publisher.root_state.try_lock().is_ok());
            assert!(publisher.kv.try_lock().is_ok());
            if from_catalog {
              Ok(())
            } else {
              Err(failure())
            }
          },
        )
        .err()
        .unwrap();
      assert_eq!(source_code(error), "union_test_callback");
      assert_eq!(visits.get(), if from_catalog { 2 } else { 1 });
      assert_eq!(emissions.get(), usize::from(from_catalog));
      drop(capture.prepare_semantic_source_union(empty_union_request(root, tree, parent), |_, _| Ok(()), |_, _, _| Ok(())).unwrap());
    });
  }
}

#[test]
fn native_semantic_source_union_checks_cancellation_and_pressure_around_callbacks() {
  for pressure in [false, true] {
    for stage in ["entry", "source", "catalog"] {
      with_union_fixture(HashAlgorithm::Blake3_256, &[], |_, capture, memory, cancellation, root, tree, parent| {
        let interrupt = || {
          if pressure {
            memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
          } else {
            cancellation.cancel();
          }
        };
        if stage == "entry" {
          interrupt();
        }
        let visits = Cell::new(0);
        let emissions = Cell::new(0);
        let error = capture
          .prepare_semantic_source_union(
            empty_union_request(root, tree, parent),
            |_, _| {
              emissions.set(emissions.get() + 1);
              assert_eq!(stage, "catalog");
              interrupt();
              Ok(())
            },
            |_, _, _| {
              visits.set(visits.get() + 1);
              assert_ne!(stage, "entry");
              if stage == "source" {
                interrupt();
              }
              Ok(())
            },
          )
          .err()
          .unwrap();
        assert_eq!(source_code(error), if pressure { "semantic_task_observation_memory" } else { "semantic_task_observation_cancelled" });
        assert_eq!(
          visits.get(),
          match stage {
            "entry" => 0,
            "source" => 1,
            _ => 2,
          }
        );
        assert_eq!(emissions.get(), usize::from(stage == "catalog"));
        memory.update_host_sample(HostMemorySample::default()).unwrap();
        if pressure {
          drop(capture.prepare_semantic_source_union(empty_union_request(root, tree, parent), |_, _| Ok(()), |_, _, _| Ok(())).unwrap());
        }
      });
    }
  }
}

#[test]
fn native_semantic_source_union_consumes_one_work_budget_across_both_namespace_passes() {
  with_union_fixture(HashAlgorithm::Blake3_256, &[], |_, capture, _, _, root, tree, parent| {
    // Measure the exact smallest successful shared budget, including base
    // binding, catalog preparation and both namespace passes.
    let mut minimum = None;
    for limit in 1..=64 {
      let mut request = empty_union_request(root, tree, parent);
      request.bounds.namespace.maximum_work = limit;
      match capture.prepare_semantic_source_union(request, |_, _| Ok(()), |_, _, _| Ok(())) {
        Ok(result) => {
          drop(result);
          minimum = Some(limit);
          break;
        }
        Err(error) => assert_eq!(source_code(error), "semantic_namespace_source_work_bound"),
      }
    }
    let minimum = minimum.expect("small empty union must fit64work units");
    assert!(minimum > 4);
    let mut request = empty_union_request(root, tree, parent);
    request.bounds.namespace.maximum_work = minimum - 1;
    let visits = Cell::new(0);
    let emissions = Cell::new(0);
    let error = capture
      .prepare_semantic_source_union(
        request,
        |_, _| {
          emissions.set(emissions.get() + 1);
          Ok(())
        },
        |_, _, _| {
          visits.set(visits.get() + 1);
          Ok(())
        },
      )
      .err()
      .unwrap();
    assert_eq!(source_code(error), "semantic_namespace_source_work_bound");
    assert_eq!(visits.get(), 2, "last budget refusal occurs after catalog sources");
    assert_eq!(emissions.get(), 1, "catalog output is provisional until final fingerprint traversal");
  });
}

#[test]
fn native_semantic_source_union_rejects_malformed_configuration_without_partial_success() {
  for path in [INDEX_SOURCE, PARSER_SOURCE] {
    with_union_fixture(
      HashAlgorithm::Blake3_256,
      &[(path.to_owned(), "application/json", b"{")],
      |_, capture, _, _, root, tree, parent| {
        let error = capture
          .prepare_semantic_source_union(
            empty_union_request(root, tree, parent),
            |_, _| panic!("malformed source emitted"),
            |_, _, _| panic!("malformed source visited"),
          )
          .err()
          .unwrap();
        assert!(
          matches!(
            error,
            NativeSemanticSourceUnionErrorV1::Compilation(
              crate::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1::InvalidSource { .. }
            )
          ),
          "{error:?}"
        );
      },
    );
  }
}

#[test]
fn native_semantic_source_union_consumes_one_read_budget_through_fingerprint_completion() {
  with_union_fixture(HashAlgorithm::Blake3_256, &[], |_, capture, _, _, root, tree, parent| {
    let mut low = 1u64;
    let mut high = 64 << 20;
    while low < high {
      let middle = low + (high - low) / 2;
      let mut request = empty_union_request(root, tree, parent);
      request.bounds.namespace.sources.maximum_read_bytes = middle;
      match capture.prepare_semantic_source_union(request, |_, _| Ok(()), |_, _, _| Ok(())) {
        Ok(result) => {
          drop(result);
          high = middle;
        }
        Err(error) => {
          assert!(["semantic_task_inventory_read_bound", "semantic_source_read_bound"].contains(&source_code(error)));
          low = middle + 1;
        }
      }
    }
    assert!(low > 1);
    let mut request = empty_union_request(root, tree, parent);
    request.bounds.namespace.sources.maximum_read_bytes = low - 1;
    let emissions = Cell::new(0);
    let error = capture
      .prepare_semantic_source_union(
        request,
        |_, _| {
          emissions.set(emissions.get() + 1);
          Ok(())
        },
        |_, _, _| Ok(()),
      )
      .err()
      .unwrap();
    assert_eq!(source_code(error), "semantic_source_read_bound");
    assert_eq!(emissions.get(), 1, "last namespace read shares earlier consumed budget");
    let mut request = empty_union_request(root, tree, parent);
    request.bounds.namespace.sources.maximum_read_bytes = low;
    drop(capture.prepare_semantic_source_union(request, |_, _| Ok(()), |_, _, _| Ok(())).unwrap());
  });
}

#[test]
fn native_semantic_source_union_accounts_absent_aliases_globally_across_sources_and_sides() {
  let index = br#"{"$v":1,"parser":"parse","indexes":[{"name":"plain","type":"typed_exact_blake3_v1"}]}"#;
  let registry = br#"{"$v":1,"parsers":{"text/plain":"parse"}}"#;
  with_union_fixture(
    HashAlgorithm::Blake3_256,
    &[(INDEX_SOURCE.to_owned(), "application/json", index), (PARSER_SOURCE.to_owned(), "application/json", registry)],
    |_, capture, memory, _, root, tree, parent| {
      let mut request = empty_union_request(root, tree, parent);
      request.bounds.maximum_alias_occurrences = 3;
      let error = capture
        .prepare_semantic_source_union(
          request,
          |_, _| panic!("incomplete discovery emitted"),
          |_, _, _| panic!("incomplete discovery visited"),
        )
        .err()
        .unwrap();
      assert_eq!(source_code(error), "semantic_source_union_alias_work");
      let mut sources = absent_globals();
      for path in [INDEX_SOURCE, PARSER_SOURCE] {
        sources.insert(path.to_owned(), Some(capture.read_protected_source(path, source_bounds()).unwrap().unwrap().revision().to_vec()));
      }
      sources.insert(plugin_fixtures::alias_path(), None);
      let mut request = empty_union_request(root, tree, parent);
      request.bounds.maximum_alias_occurrences = 4;
      assert_small_union(capture, request, HashAlgorithm::Blake3_256, &sources, &sources, &sources, memory);
    },
  );
}

#[test]
fn native_semantic_source_union_includes_explicit_unreferenced_change_without_inventorying_all_aliases() {
  use crate::engine::v4::plugin_identity::plugin_alias_path_v1;
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let changed = plugin_alias_path_v1("changed").unwrap();
    let unused = plugin_alias_path_v1("unused").unwrap();
    with_union_fixture(
      algorithm,
      &[
        (changed.clone(), "application/octet-stream", b"unreferenced bytes"),
        (unused.clone(), "application/octet-stream", b"unrelated bytes"),
      ],
      |_, capture, memory, _, root, tree, parent| {
        let mut base = absent_globals();
        base.insert(changed.clone(), Some(capture.read_protected_source(&changed, source_bounds()).unwrap().unwrap().revision().to_vec()));
        let mut requested = base.clone();
        requested.insert(changed.clone(), None);
        let replacements = [NativeSemanticSourceReplacementV1 { path: &changed, file_record_id: None }];
        let mut request = empty_union_request(root, tree, parent);
        request.replacements = &replacements;
        assert_small_union(capture, request, algorithm, &base, &requested, &base, memory);
      },
    );
  }
}

#[test]
fn native_semantic_source_union_bounds_scratch_and_catalog_output_with_clean_retry() {
  for kind in ["input-count", "stored-bytes", "io-bytes", "catalog-bytes", "fingerprint-workspace"] {
    with_union_fixture(HashAlgorithm::Blake3_256, &[], |_, capture, _, _, root, tree, parent| {
      let mut request = empty_union_request(root, tree, parent);
      match kind {
        "input-count" => request.bounds.paths.maximum_input_paths = 1,
        "stored-bytes" => request.bounds.paths.maximum_stored_bytes = 1,
        "io-bytes" => request.bounds.paths.maximum_io_bytes = 1,
        "catalog-bytes" => request.bounds.maximum_catalog_output_bytes = 1,
        "fingerprint-workspace" => request.bounds.maximum_fingerprint_workspace_bytes = 1,
        _ => unreachable!(),
      }
      let error = capture.prepare_semantic_source_union(request, |_, _| Ok(()), |_, _, _| Ok(())).err().unwrap();
      assert!(
        match &error {
          NativeSemanticSourceUnionErrorV1::Compilation(
            crate::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1::InvalidSource { .. },
          ) => kind == "stored-bytes",
          NativeSemanticSourceUnionErrorV1::Compilation(
            crate::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1::Resource { .. },
          ) => kind != "stored-bytes",
          _ => false,
        },
        "{kind}: {error:?}"
      );
      assert_eq!(fs::read_dir(parent).unwrap().count(), 0);
      drop(capture.prepare_semantic_source_union(empty_union_request(root, tree, parent), |_, _| Ok(()), |_, _, _| Ok(())).unwrap());
    });
  }
}
