//! Retained selection, aggregate budgets and refused-operation cleanup.
use super::*;
use super::super::resource_spec::{control_read_cost, file_read_cost};
use crate::engine::memory_coordinator::HostMemorySample;

fn same_pair(publisher: &V4FirstAuthorityPublisher, rows: &std::collections::BTreeMap<String, Option<Vec<u8>>>) -> Vec<u8> {
  let rows: Vec<_> = rows.iter().map(|(path, hash)| (path.as_str(), hash.as_deref())).collect();
  seed_catalog_pair(publisher, &rows, &rows)
}

#[test]
fn retained_alias_snapshot_one_companion_and_deduplicated_pairs_share_exact_read_and_work_limits() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("retained-alias-budget", None, [1; 16], algorithm, 0);
    let module = fixtures::module("both");
    let (first, artifact) = seed_module(&publisher, &module, "both");
    let mut second = fixtures::alias(&module, "both");
    second[128..133].copy_from_slice(b"other");
    fixtures::seal(&mut second);
    let second_path = format!("/.aeordb-system/plugin-aliases/{}", blake3::hash(b"other").to_hex());
    seed_files(&publisher, &[(second_path.clone(), "application/octet-stream", &second)]);
    let second = seed_retained_revision(&publisher, &second_path);
    let rows = std::collections::BTreeMap::from([
      (INDEX_SOURCE.to_string(), None),
      (PARSER_SOURCE.to_string(), None),
      (fixtures::alias_path(), Some(first.clone())),
      (second_path, Some(second.clone())),
      (fixtures::artifact_path(&module), Some(artifact.clone())),
    ]);
    let companion = same_pair(&publisher, &rows);
    let decoded = crate::engine::v4::semantic_source_capture::decode_semantic_source_capture_v1(&companion, algorithm).unwrap();
    let (_, companion_bytes, companion_reads) =
      control_read_cost(&publisher, SystemControlKindV1::SemanticSourceCapture, &checkpoint_identity());
    let (_, checkpoint_bytes, checkpoint_reads) =
      control_read_cost(&publisher, SystemControlKindV1::SemanticMutationCheckpoint, &checkpoint_identity());
    let (_, node_bytes, node_reads) = control_read_cost(&publisher, SystemControlKindV1::SemanticSourceNode, decoded.base_source_catalog);
    let first_cost = file_read_cost(&publisher, &first);
    let second_cost = file_read_cost(&publisher, &second);
    let module_cost = file_read_cost(&publisher, &artifact);
    // Base is one independently seeded leaf. Two distinct aliases each read
    // that leaf for the alias and module; repeated roles/uses are deduplicated.
    let exact_bytes = companion_bytes + checkpoint_bytes + 4 * node_bytes + first_cost.0 + second_cost.0 + 2 * module_cost.0;
    let exact_work = companion_reads + checkpoint_reads + 4 * (node_reads + 2) + first_cost.1 + second_cost.1 + 2 * module_cost.1;
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let source = br#"{"$v":1,"parser":"parse","indexes":[{"name":"z","type":"typed_exact_blake3_v1","source":{"plugin":"other"}},{"name":"y","type":"typed_exact_blake3_v1","source":{"plugin":"parse"}},{"name":"a","type":"typed_exact_blake3_v1","source":{"plugin":"other"}}]}"#;
    for mode in 0..4 {
      let mut request = snapshot_request(SemanticSourceAliasKindV1::IndexConfiguration, Some(source));
      let mut bounds = catalog_bounds();
      bounds.maximum_work = exact_work - u64::from(mode == 3);
      bounds.maximum_read_bytes = exact_bytes - u64::from(mode == 1);
      request.plugins.maximum_read_bytes = exact_bytes - u64::from(mode == 2);
      let result = capture.prepare_captured_semantic_alias_snapshot(&[2; 16], 1, SemanticSourceCatalogSideV1::Base, request, bounds);
      if mode == 0 {
        let snapshot = result.expect("independent exact cumulative budget must suffice");
        assert_eq!(snapshot.resolve_parser_alias("parse").unwrap().unwrap().role, 1);
        assert_eq!(snapshot.resolve_mapper_alias("parse").unwrap().unwrap().role, 2);
        assert_eq!(snapshot.resolve_mapper_alias("other").unwrap().unwrap().role, 2);
      } else {
        // The final exhausted read is a retained module chunk. Its existing
        // source-reader classification must survive the catalog adapter.
        let expected = if mode == 3 { "semantic_source_catalog_work_bound" } else { "semantic_source_read_bound" };
        let error = result.err().expect("either owner's aggregate ceiling must refuse");
        assert!(matches!(error, NativeSemanticPluginSourceErrorV1::Source(ref source) if source.code() == expected), "{mode}: {error:?}");
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    }
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn retained_alias_snapshot_empty_or_unused_inputs_still_validate_the_companion_and_identity() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("retained-alias-empty", None, [1; 16]);
  seed_catalog_pair(&publisher, &[(INDEX_SOURCE, None), (PARSER_SOURCE, None)], &[(INDEX_SOURCE, None), (PARSER_SOURCE, None)]);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  for (kind, source) in [
    (SemanticSourceAliasKindV1::ParserRegistry, None),
    (SemanticSourceAliasKindV1::IndexConfiguration, Some(br#"{"$v":1,"parser":"unused","indexes":[]}"#.as_slice())),
  ] {
    for (task, sequence, code) in [
      ([0; 16], 1, "semantic_source_catalog_identity"),
      ([2; 16], 0, "semantic_source_catalog_identity"),
      ([2; 16], 2, "semantic_source_catalog_capture_missing"),
    ] {
      let result = capture.prepare_captured_semantic_alias_snapshot(
        &task,
        sequence,
        SemanticSourceCatalogSideV1::Requested,
        snapshot_request(kind, source),
        catalog_bounds(),
      );
      assert!(matches!(result, Err(NativeSemanticPluginSourceErrorV1::Source(error)) if error.code() == code));
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    }
    drop(
      capture
        .prepare_captured_semantic_alias_snapshot(
          &[2; 16],
          1,
          SemanticSourceCatalogSideV1::Requested,
          snapshot_request(kind, source),
          catalog_bounds(),
        )
        .unwrap(),
    );
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  }
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn retained_alias_snapshot_required_artifact_unlisted_absent_or_missing_never_uses_current_module() {
  for mode in 0..3 {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("retained-alias-module", None, [1; 16]);
    let module = fixtures::module("both");
    let (alias, _) = seed_module(&publisher, &module, "both");
    let mut rows = std::collections::BTreeMap::from([
      (INDEX_SOURCE.to_string(), None),
      (PARSER_SOURCE.to_string(), None),
      (fixtures::alias_path(), Some(alias)),
    ]);
    if mode > 0 {
      rows.insert(fixtures::artifact_path(&module), (mode == 2).then(|| vec![0x91; 32]));
    }
    same_pair(&publisher, &rows);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let source = br#"{"$v":1,"parsers":{"text/plain":"parse"}}"#;
    for side in [SemanticSourceCatalogSideV1::Base, SemanticSourceCatalogSideV1::Requested] {
      let result = capture.prepare_captured_semantic_alias_snapshot(
        &[2; 16],
        1,
        side,
        snapshot_request(SemanticSourceAliasKindV1::ParserRegistry, Some(source)),
        catalog_bounds(),
      );
      let expected =
        ["semantic_source_catalog_unlisted", "semantic_plugin_source_module_missing", "semantic_source_retained_missing"][mode];
      let error = result.err().expect("bad retained artifact cannot resolve through current path");
      assert!(matches!(error, NativeSemanticPluginSourceErrorV1::Source(ref source) if source.code() == expected), "{mode}: {error:?}");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    }
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn retained_alias_snapshot_rechecks_cancellation_and_pressure_for_preparation_and_borrowed_resolution() {
  for cancel in [false, true] {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("retained-alias-lifecycle", None, [1; 16]);
    let module = fixtures::module("both");
    let (alias, artifact) = seed_module(&publisher, &module, "both");
    same_pair(
      &publisher,
      &std::collections::BTreeMap::from([
        (INDEX_SOURCE.to_string(), None),
        (PARSER_SOURCE.to_string(), None),
        (fixtures::alias_path(), Some(alias)),
        (fixtures::artifact_path(&module), Some(artifact)),
      ]),
    );
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let source = br#"{"$v":1,"parsers":{"text/plain":"parse"}}"#;
    let request = snapshot_request(SemanticSourceAliasKindV1::ParserRegistry, Some(source));
    let snapshot =
      capture.prepare_captured_semantic_alias_snapshot(&[2; 16], 1, SemanticSourceCatalogSideV1::Base, request, catalog_bounds()).unwrap();
    if cancel {
      cancellation.cancel();
    } else {
      memory.update_host_sample(HostMemorySample { rss_bytes: 512 << 20, ..Default::default() }).unwrap();
    }
    assert!(snapshot.resolve_parser_alias("parse").is_err());
    assert!(capture
      .prepare_captured_semantic_alias_snapshot(&[2; 16], 1, SemanticSourceCatalogSideV1::Base, request, catalog_bounds())
      .is_err());
    drop(snapshot);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    if !cancel {
      memory.update_host_sample(HostMemorySample::default()).unwrap();
      drop(
        capture
          .prepare_captured_semantic_alias_snapshot(&[2; 16], 1, SemanticSourceCatalogSideV1::Base, request, catalog_bounds())
          .unwrap(),
      );
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    }
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn retained_alias_snapshot_malformed_and_misbound_retained_inputs_are_errors_not_absence() {
  for case in 0..5 {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("retained-alias-invalid", None, [1; 16]);
    let module = fixtures::module("both");
    let mut alias = fixtures::alias(&module, "both");
    if case == 0 {
      alias[80] ^= 1;
    }
    let mut selected_module = module.clone();
    if case == 1 {
      // Correct physical record/chunk, but wrong bytes for the alias identity.
      let last = selected_module.len() - 1;
      selected_module[last] ^= 1;
    }
    seed_files(
      &publisher,
      &[
        (fixtures::alias_path(), "application/octet-stream", &alias),
        (fixtures::artifact_path(&module), "application/wasm", &selected_module),
      ],
    );
    let alias_id = seed_retained_revision(&publisher, &fixtures::alias_path());
    let artifact_id = seed_retained_revision(&publisher, &fixtures::artifact_path(&module));
    let rows = std::collections::BTreeMap::from([
      (INDEX_SOURCE.to_string(), None),
      (PARSER_SOURCE.to_string(), None),
      (fixtures::alias_path(), Some(if case == 2 { artifact_id.clone() } else { alias_id })),
      (fixtures::artifact_path(&module), Some(artifact_id)),
    ]);
    let companion = same_pair(&publisher, &rows);
    if case == 3 {
      corrupt_last_entity_byte(&publisher, &first_authority_system_chunk_hash(&module, HashAlgorithm::Blake3_256));
    } else if case == 4 {
      corrupt_last_entity_byte(&publisher, &first_authority_system_chunk_hash(&companion, HashAlgorithm::Blake3_256));
    }
    // A different, valid current alias must not conceal any retained failure.
    seed_module(&publisher, &fixtures::module("parser"), "parser");
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let source = br#"{"$v":1,"parsers":{"text/plain":"parse"}}"#;
    for side in [SemanticSourceCatalogSideV1::Base, SemanticSourceCatalogSideV1::Requested] {
      let error = capture
        .prepare_captured_semantic_alias_snapshot(
          &[2; 16],
          1,
          side,
          snapshot_request(SemanticSourceAliasKindV1::ParserRegistry, Some(source)),
          catalog_bounds(),
        )
        .err()
        .expect("invalid retained input cannot return a snapshot or absence");
      match case {
        0 => assert!(
          matches!(error, NativeSemanticPluginSourceErrorV1::Source(ref source) if source.code() == "plugin_alias_crc"),
          "{error:?}"
        ),
        1 => assert!(matches!(error, NativeSemanticPluginSourceErrorV1::Identity(_)), "{case}: {error:?}"),
        2 => assert!(
          matches!(error, NativeSemanticPluginSourceErrorV1::Source(ref source) if source.code() == "semantic_source_record_path"),
          "{error:?}"
        ),
        _ => assert!(
          matches!(error, NativeSemanticPluginSourceErrorV1::Source(ref source) if source.code() == "integrity_hash_mismatch"),
          "{case}: {error:?}"
        ),
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    }
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
