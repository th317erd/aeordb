//! Native malformed closure and independent ordered-map expectations.
use super::*;
use crate::engine::v4::semantic_source_capture::decode_semantic_source_capture_v1;
use crate::engine::v4::semantic_mutation_control::encode_semantic_mutation_checkpoint;

fn replace_capture(publisher: &V4FirstAuthorityPublisher, bytes: &[u8]) {
  seed(publisher, &[(SystemControlKindV1::SemanticSourceCapture, &checkpoint_identity(), SystemControlSlotV1::Immutable, bytes)]);
}

#[test]
fn native_catalog_missing_checkpoint_never_becomes_an_empty_or_absent_capture() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-catalog-missing-checkpoint", None, [1; 16], algorithm, 0);
    let profile = if algorithm.hash_length() == 32 { "blake3-256" } else { "sha512" };
    let capture = fs::read(format!(
      "{}/spec/fixtures/v4/system-control-v1/control-{profile}-semantic-source-capture-valid.bin",
      env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    replace_capture(&publisher, &capture);
    let mut header = publisher.observe().unwrap().selected.header;
    header.required_reader_capabilities[3] |= 8;
    header.required_writer_capabilities[3] |= 8;
    write_redundant_header(&publisher, &header);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let mut calls = 0;
    let error = captured
      .visit_captured_protected_source_pairs(&[2; 16], 1, catalog_bounds(), |_, _, _| {
        calls += 1;
        Ok(true)
      })
      .unwrap_err();
    assert_eq!(error.code(), "semantic_source_catalog_checkpoint_missing");
    assert_eq!(calls, 0);
    assert_eq!(
      captured
        .read_captured_protected_source(&[2; 16], 1, SemanticSourceCatalogSideV1::Base, INDEX_SOURCE, catalog_bounds())
        .err()
        .expect("missing checkpoint")
        .code(),
      "semantic_source_catalog_checkpoint_missing"
    );
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_catalog_uses_one_captured_control_boundary_even_after_test_only_replacement() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-catalog-captured-boundary", None, [1; 16], algorithm, 0);
    let rows = [(INDEX_SOURCE, None), (PARSER_SOURCE, None), (ALIAS_SOURCE, None)];
    seed_catalog_pair(&publisher, &rows, &rows);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let old = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    seed_raw_source(&publisher, INDEX_SOURCE, 1, 1, 0, false, CompressionAlgorithm::Zstd);
    let revision = seed_retained_revision(&publisher, INDEX_SOURCE);
    // Deliberate test-only replacement of a normally immutable control path
    // proves snapshot use; no production immutable writer permits this.
    let rows = [(INDEX_SOURCE, Some(revision.as_slice())), (PARSER_SOURCE, None), (ALIAS_SOURCE, None)];
    seed_catalog_pair(&publisher, &rows, &rows);
    let fresh = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    assert!(matches!(
      old
        .read_captured_protected_source(&[2; 16], 1, SemanticSourceCatalogSideV1::Base, INDEX_SOURCE, catalog_bounds())
        .unwrap()
        .disposition(),
      SemanticSourceLookupDispositionV1::Absent
    ));
    let result =
      fresh.read_captured_protected_source(&[2; 16], 1, SemanticSourceCatalogSideV1::Base, INDEX_SOURCE, catalog_bounds()).unwrap();
    assert_eq!(result.disposition(), SemanticSourceLookupDispositionV1::Present);
    assert_eq!(result.source().expect("fresh capture must observe newly seeded retained revision").body(), b"firstsecond");
    drop(result);
    assert!(
      old
        .visit_captured_protected_source_pairs(&[2; 16], 1, catalog_bounds(), |_, base, requested| {
          assert!(base.is_none() && requested.is_none());
          Ok(true)
        })
        .unwrap()
        .complete
    );
    assert!(
      fresh
        .visit_captured_protected_source_pairs(&[2; 16], 1, catalog_bounds(), |path, base, requested| {
          assert_eq!(base.is_some(), path == INDEX_SOURCE);
          assert_eq!(requested.is_some(), path == INDEX_SOURCE);
          Ok(true)
        })
        .unwrap()
        .complete
    );
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_catalog_validated_counts_missing_nodes_and_inherited_ranges_are_not_declared_proof() {
  for case in 0..5 {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-catalog-closure-errors", None, [1; 16], algorithm, 0);
    let rows = [(INDEX_SOURCE, None), (PARSER_SOURCE, None), (ALIAS_SOURCE, None)];
    let capture = seed_catalog_pair(&publisher, &rows, &rows);
    let mut input = decode_semantic_source_capture_v1(&capture, algorithm).unwrap();
    let replacement_root;
    match case {
      0 => input.protected_path_count += 1,
      1 => input.base_catalog_node_count += 1,
      2 => input.requested_catalog_node_count += 1,
      3 => {
        replacement_root = vec![0x91; algorithm.hash_length()];
        input.requested_source_catalog = &replacement_root;
      }
      4 => {
        // Valid local framing, invalid inherited range: the left child contains
        // the separator itself and the right child also contains earlier keys.
        let body = encode_semantic_source_internal_v1(
          &[1; 16],
          &[
            SemanticSourceChildV1 { separator: None, node_id: input.base_source_catalog },
            SemanticSourceChildV1 { separator: Some(PARSER_SOURCE), node_id: input.requested_source_catalog },
          ],
          algorithm,
        )
        .unwrap();
        replacement_root = seed_catalog_node(&publisher, &body);
        input.requested_source_catalog = &replacement_root;
        input.requested_catalog_node_count = 5;
      }
      _ => unreachable!(),
    }
    replace_capture(&publisher, &encode_semantic_source_capture_v1(&input, algorithm).unwrap());
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let error = captured.visit_captured_protected_source_pairs(&[2; 16], 1, catalog_bounds(), |_, _, _| Ok(true)).unwrap_err();
    assert_eq!(
      error.code(),
      match case {
        0..=2 => "semantic_source_catalog_counts",
        3 => "semantic_source_catalog_node_missing",
        _ => "semantic_source_catalog_range",
      }
    );
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_catalog_present_rows_require_matching_retained_records_never_current_fallback() {
  for case in 0..2 {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-catalog-record-errors", None, [1; 16], algorithm, 0);
    seed_raw_source(&publisher, INDEX_SOURCE, 1, 0, 0, false, CompressionAlgorithm::None);
    let revision = if case == 0 { vec![0x92; algorithm.hash_length()] } else { seed_retained_revision(&publisher, INDEX_SOURCE) };
    let target = if case == 0 { INDEX_SOURCE } else { PARSER_SOURCE };
    let rows = [
      (INDEX_SOURCE, (case == 0).then_some(revision.as_slice())),
      (PARSER_SOURCE, (case == 1).then_some(revision.as_slice())),
      (ALIAS_SOURCE, None),
    ];
    seed_catalog_pair(&publisher, &rows, &rows);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let code = if case == 0 { "semantic_source_retained_missing" } else { "semantic_source_record_path" };
    assert_eq!(
      captured
        .read_captured_protected_source(&[2; 16], 1, SemanticSourceCatalogSideV1::Requested, target, catalog_bounds())
        .err()
        .expect("bad source must refuse")
        .code(),
      code
    );
    assert_eq!(captured.visit_captured_protected_source_pairs(&[2; 16], 1, catalog_bounds(), |_, _, _| Ok(true)).unwrap_err().code(), code);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_catalog_requires_both_global_inputs_and_rejects_non_source_leaf_families() {
  for case in 0..3 {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-catalog-required-paths", None, [1; 16], algorithm, 0);
    let rows = match case {
      0 => vec![(PARSER_SOURCE, None), (ALIAS_SOURCE, None), ("/.aeordb-system/plugin-aliases/other", None)],
      1 => vec![(INDEX_SOURCE, None), (ALIAS_SOURCE, None), ("/.aeordb-system/plugin-aliases/other", None)],
      _ => vec![(INDEX_SOURCE, None), (PARSER_SOURCE, None), ("/ordinary-file", None)],
    };
    seed_catalog_pair(&publisher, &rows, &rows);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let error = captured.visit_captured_protected_source_pairs(&[2; 16], 1, catalog_bounds(), |_, _, _| Ok(true)).unwrap_err();
    assert_eq!(error.code(), if case == 2 { "semantic_source_family" } else { "semantic_source_catalog_required_paths" });
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_catalog_binds_full_checkpoint_and_database_but_preserves_stale_capture_diagnostics() {
  for case in 0..3 {
    let algorithm = HashAlgorithm::Sha512;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-catalog-binding", None, [1; 16], algorithm, 0);
    let rows = [(INDEX_SOURCE, None), (PARSER_SOURCE, None), (ALIAS_SOURCE, None)];
    seed_catalog_pair(&publisher, &rows, &rows);
    if case == 0 {
      let bytes = frozen(algorithm, "checkpoint");
      let mut changed = decode_semantic_mutation_checkpoint(&bytes, algorithm).unwrap();
      let mut fingerprint = changed.compiler_fingerprint.to_vec();
      fingerprint[0] ^= 1;
      changed.compiler_fingerprint = &fingerprint;
      let bytes = encode_semantic_mutation_checkpoint(&changed, algorithm).unwrap();
      seed(
        &publisher,
        &[(SystemControlKindV1::SemanticMutationCheckpoint, &checkpoint_identity(), SystemControlSlotV1::Immutable, &bytes)],
      );
    } else {
      let mut header = publisher.observe().unwrap().selected.header;
      if case == 1 {
        header.database_id = [0x93; 16];
      } else {
        header.physical_instance_id = [0x94; 16];
        header.writer_fence_epoch += 17;
      }
      write_redundant_header(&publisher, &header);
    }
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let result = captured.visit_captured_protected_source_pairs(&[2; 16], 1, catalog_bounds(), |_, _, _| Ok(true));
    match case {
      0 => assert_eq!(result.unwrap_err().code(), "semantic_capture_checkpoint_binding"),
      1 => assert_eq!(result.unwrap_err().code(), "immutable_system_control_stored_mismatch"),
      _ => assert!(result.unwrap().complete),
    }
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_catalog_capability_identity_and_missing_companion_refusals_are_explicit() {
  for case in 0..7 {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-catalog-admission", None, [1; 16], algorithm, 0);
    let rows = [(INDEX_SOURCE, None), (PARSER_SOURCE, None), (ALIAS_SOURCE, None)];
    seed_catalog_pair(&publisher, &rows, &rows);
    if case < 4 {
      let mut header = publisher.observe().unwrap().selected.header;
      let capabilities = if case % 2 == 0 { &mut header.required_reader_capabilities } else { &mut header.required_writer_capabilities };
      capabilities[3] &= !(if case < 2 { 2 } else { 8 });
      write_redundant_header(&publisher, &header);
    }
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let task = if case == 4 { [0; 16] } else { [2; 16] };
    let sequence = if case == 5 {
      0
    } else if case == 6 {
      2
    } else {
      1
    };
    let error = captured.visit_captured_protected_source_pairs(&task, sequence, catalog_bounds(), |_, _, _| Ok(true)).unwrap_err();
    assert_eq!(
      error.code(),
      match case {
        0..=3 => "semantic_source_catalog_capability",
        4..=5 => "semantic_source_catalog_identity",
        _ => "semantic_source_catalog_capture_missing",
      }
    );
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_catalog_all_profiles_match_an_independent_ordered_map_and_point_seek_skips_prior_leaf() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    for aliases in [0, 1, 6] {
      let (_directory, _path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("source-catalog-ordered-model", None, [1; 16], algorithm, 0);
      let mut expected = std::collections::BTreeMap::new();
      expected.insert(INDEX_SOURCE.to_string(), None::<Vec<u8>>);
      expected.insert(PARSER_SOURCE.to_string(), None);
      for index in 0..aliases {
        expected.insert(format!("/.aeordb-system/plugin-aliases/item-{index:03}"), None);
      }
      let rows: Vec<_> = expected.iter().map(|(path, revision)| (path.as_str(), revision.as_deref())).collect();
      seed_catalog_pair(&publisher, &rows, &rows);
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let mut actual = Vec::new();
      let summary = captured
        .visit_captured_protected_source_pairs(&[2; 16], 1, catalog_bounds(), |path, base, requested| {
          assert!(base.is_none() && requested.is_none());
          actual.push(path.to_string());
          Ok(true)
        })
        .unwrap();
      assert_eq!(actual, expected.keys().cloned().collect::<Vec<_>>());
      assert_eq!(summary.paths as usize, expected.len());
      for path in expected.keys() {
        let mut bounds = catalog_bounds();
        // Two companion controls (four reads), two selected nodes (six work),
        // one selected row. Reading the other leaf exceeds this exact bound.
        bounds.maximum_work = 11;
        assert!(matches!(
          captured.read_captured_protected_source(&[2; 16], 1, SemanticSourceCatalogSideV1::Requested, path, bounds).unwrap().disposition(),
          SemanticSourceLookupDispositionV1::Absent
        ));
      }
    }
  }
}
