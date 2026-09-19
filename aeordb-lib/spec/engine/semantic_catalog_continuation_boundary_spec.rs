//! Phase/request boundaries consume refused work and never publish completion.
use super::*;
use super::restart_spec::{checkpoint, native_configuration};
use aeordb::engine::v4::semantic_catalog_compiler::admit_semantic_catalog_progress_v1;

fn refused<T>(result: Result<T, SemanticCatalogCompilationErrorV1>) -> SemanticCatalogCompilationErrorV1 {
  match result {
    Err(error) => error,
    Ok(_) => panic!("invalid continuation was accepted"),
  }
}

#[test]
fn catalog_continuation_resume_rebinds_registry_hash_count_workspace_and_cancellation() {
  for algorithm in [ALGORITHMS[0], ALGORITHMS[2]] {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let changed = compile_parser_registry_v1(
      ParserRegistryCompilationRequestV1 {
        source: Some(br#"{"$v":1,"parsers":{"text/plain":"p"}}"#),
        hash_algorithm: algorithm,
        maximum_source_bytes: 1 << 20,
        maximum_workspace_bytes: 64 << 20,
      },
      &AvailableSnapshot,
      &memory,
      &|| false,
    )
    .unwrap();
    let mut store = Store::new(algorithm);
    let original =
      compile_semantic_catalog_v1(request(algorithm, 0), &registry, [], &mut store, &memory, &|| false).unwrap().semantic_state().clone();
    let work = SemanticCatalogContinuationV1::start(request(algorithm, 1), &registry, &mut store, &memory, &|| false).unwrap();
    let bytes = checkpoint(&work, &original, 1, algorithm);
    drop(work);
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let writes = store.writes;
    for mode in 0..6 {
      let progress = admit_semantic_catalog_progress_v1(request(algorithm, 1), &bytes, &registry, &store, &memory, &|| false).unwrap();
      let mut next_request = request(algorithm, 1);
      let next_registry = if mode == 0 { &changed } else { &registry };
      if mode == 1 {
        next_request.hash_algorithm = HashAlgorithm::Sha3_512;
      }
      if mode == 2 {
        next_request.expected_configuration_count = 2;
      }
      if mode == 3 {
        next_request.maximum_workspace_bytes = 0;
      }
      if mode == 5 {
        next_request.required_capabilities[0] = 1;
      }
      let error =
        refused(SemanticCatalogContinuationV1::from_progress(next_request, progress, next_registry, &store, &memory, &|| mode == 4));
      match mode {
        0 => assert!(matches!(error, SemanticCatalogCompilationErrorV1::InvalidInput { code: "semantic_catalog_registry_changed", .. })),
        1 | 2 | 5 => {
          assert!(matches!(error, SemanticCatalogCompilationErrorV1::InvalidInput { code: "semantic_catalog_progress_request", .. }))
        }
        3 => assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.code() == "semantic_catalog_workspace")),
        4 => assert!(
          matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == SemanticCatalogReadErrorClassV1::Cancelled)
        ),
        _ => unreachable!(),
      }
      assert_eq!(store.writes, writes);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    }
    let progress = admit_semantic_catalog_progress_v1(request(algorithm, 1), &bytes, &registry, &store, &memory, &|| false).unwrap();
    let retry =
      SemanticCatalogContinuationV1::from_progress(request(algorithm, 1), progress, &registry, &store, &memory, &|| false).unwrap();
    drop(retry);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  }
}

#[test]
fn catalog_continuation_phase_and_final_count_refusals_do_not_publish_completion() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let mut store = Store::new(algorithm);
  let base = compile_semantic_catalog_v1(
    request(algorithm, 1),
    &registry,
    [Ok(native_configuration(algorithm, &registry, &memory, "/"))],
    &mut store,
    &memory,
    &|| false,
  )
  .unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  for mode in 0..7 {
    let mut work =
      SemanticCatalogContinuationV1::from_complete(request(algorithm, 0), &base, &registry, &store, &memory, &|| false).unwrap();
    if mode >= 3 {
      work = work.apply(SemanticCatalogConfigurationMutationV1::Remove("/".into()), &mut store).unwrap();
      work = work.finish_configurations(&mut store).unwrap();
    }
    if mode == 6 {
      for _ in 0..4 {
        work = work.prune_one(&mut store).unwrap();
      }
    }
    let writes = store.writes;
    let error = match mode {
      0 => refused(work.finish(&mut store)),
      1 => refused(work.prune_one(&mut store)),
      2 => refused(work.finish_configurations(&mut store)),
      3 => refused(work.apply(SemanticCatalogConfigurationMutationV1::Remove("/missing".into()), &mut store)),
      4 => refused(work.finish_configurations(&mut store)),
      5 => refused(work.finish(&mut store)),
      6 => refused(work.prune_one(&mut store)),
      _ => unreachable!(),
    };
    let expected = match mode {
      2 => "semantic_catalog_configuration_count",
      5 => "semantic_catalog_pruning_incomplete",
      6 => "semantic_catalog_pruning_empty",
      _ => "semantic_catalog_continuation_phase",
    };
    assert!(matches!(error, SemanticCatalogCompilationErrorV1::InvalidInput { code, .. } if code == expected));
    assert_eq!(store.writes, writes);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(store.objects.keys().filter(|(kind, _)| *kind == 1).count(), 1, "no new Complete state");
  }
}

#[test]
fn catalog_continuation_pruning_excludes_dependencies_still_owned_by_another_configuration() {
  for algorithm in ALGORITHMS {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let base = compile_semantic_catalog_v1(
      request(algorithm, 2),
      &registry,
      ["/left", "/right"].into_iter().map(|owner| Ok(native_configuration(algorithm, &registry, &memory, owner))),
      &mut store,
      &memory,
      &|| false,
    )
    .unwrap();
    let work = SemanticCatalogContinuationV1::from_complete(request(algorithm, 1), &base, &registry, &store, &memory, &|| false).unwrap();
    let work = work.apply(SemanticCatalogConfigurationMutationV1::Remove("/left".into()), &mut store).unwrap();
    assert_eq!(work.pruning_candidates().record_count, 4);
    let work = work.finish_configurations(&mut store).unwrap();
    assert_eq!(work.pruning_candidates().record_count, 0);
    assert_eq!(work.dependency_count(), 4);
    let result = work.finish(&mut store).unwrap();
    let mut fresh_store = Store::new(algorithm);
    let fresh = compile_semantic_catalog_v1(
      request(algorithm, 1),
      &registry,
      [Ok(native_configuration(algorithm, &registry, &memory, "/right"))],
      &mut fresh_store,
      &memory,
      &|| false,
    )
    .unwrap();
    assert_eq!(result.semantic_state(), fresh.semantic_state());
  }
}
