use super::*;

#[test]
fn native_semantic_source_union_valid_but_insufficient_storage_refuses_and_retries() {
  super::with_union_fixture(HashAlgorithm::Blake3_256, &[], |_, capture, _, _, root, tree, parent| {
    let mut request = super::empty_union_request(root, tree, parent);
    request.bounds.paths.maximum_stored_bytes = 32;
    let error = capture
      .prepare_semantic_source_union(request, |_, _| panic!("insufficient space emitted"), |_, _, _| panic!("insufficient space visited"))
      .err()
      .unwrap();
    assert!(
      matches!(
        error,
        NativeSemanticSourceUnionErrorV1::Compilation(
          crate::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1::Resource { .. }
        )
      ),
      "{error:?}"
    );
    assert_eq!(fs::read_dir(parent).unwrap().count(), 0);
    drop(capture.prepare_semantic_source_union(super::empty_union_request(root, tree, parent), |_, _| Ok(()), |_, _, _| Ok(())).unwrap());
  });
}

#[test]
fn native_semantic_source_union_requested_namespace_discovers_parser_and_mapper_aliases() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-union-namespace-alias", None, [1; 16], algorithm, 0);
    let initial = request_for_database_and_algorithm([1; 16], algorithm);
    let root = publisher.publish(&initial).unwrap().namespace_root.root_hash;
    let module = plugin_fixtures::module("both");
    seed_union_plugin(&publisher, &module, "both");
    let body = br#"{"$v":1,"parser":"parse","indexes":[{"name":"value","type":"typed_exact_blake3_v1"},{"name":"mapped","type":"typed_exact_blake3_v1","source":{"plugin":"parse"}}]}"#;
    let (directory, _) = namespace_configuration_tree(&publisher, "/new", body, vec![]);
    let requested = publish_namespace_directory(&publisher, vec![namespace_directory_child("new", directory)]);
    seed_union_generation(&publisher);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let mut sources = absent_globals();
    for path in [plugin_fixtures::alias_path(), plugin_fixtures::artifact_path(&module)] {
      sources.insert(path.clone(), Some(capture.read_protected_source(&path, source_bounds()).unwrap().unwrap().revision().to_vec()));
    }
    let mut fingerprint = sources.clone();
    fingerprint.insert("/new/.aeordb-config/indexes.json".into(), None);
    let parent = tempfile::tempdir().unwrap();
    let before = fs::read(&path).unwrap();
    assert_small_union(
      &capture,
      NativeSemanticSourceUnionRequestV1 {
        expected_base_root: &root,
        requested_directory_root: &requested,
        replacements: &[],
        workspace_parent: parent.path(),
        bounds: union_bounds(&requested),
      },
      algorithm,
      &sources,
      &sources,
      &fingerprint,
      &memory,
    );
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_source_union_staging_callbacks_keep_the_original_capture_across_current_replacement() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, _path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-union-stage-callback", None, [1; 16], algorithm, 0);
    let initial = request_for_database_and_algorithm([1; 16], algorithm);
    let root = publisher.publish(&initial).unwrap().namespace_root.root_hash;
    let original = br#"{"$v":1,"indexes":[]}"#;
    let replacement = br#"{ "$v": 1, "indexes": [] }"#;
    seed_files(&publisher, &[(INDEX_SOURCE.to_owned(), "application/json", original)]);
    seed_union_generation(&publisher);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let original_revision = capture.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap().revision().to_vec();
    let original_header = publisher.observe().unwrap().selected.header.slot_sequence;
    let parent = tempfile::tempdir().unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let mut expected = absent_globals();
    expected.insert(INDEX_SOURCE.to_owned(), Some(original_revision.clone()));
    let mut nodes = Vec::new();
    let result = capture
      .prepare_semantic_source_union(
        NativeSemanticSourceUnionRequestV1 {
          expected_base_root: &root,
          requested_directory_root: &initial.namespace_tree.root_hash,
          replacements: &[],
          workspace_parent: parent.path(),
          bounds: union_bounds(&initial.namespace_tree.root_hash),
        },
        |left, right| {
          nodes.push((left.to_vec(), right.to_vec()));
          Ok(())
        },
        |path, left, right| {
          if path == INDEX_SOURCE {
            assert_eq!(left.unwrap().body(), original);
            assert_eq!(right.unwrap().revision(), original_revision);
            left.unwrap().stage_retained_copy(source_bounds(), publisher.observe().unwrap().selected.header.updated_at_ms + 1)?;
            seed_files(&publisher, &[(INDEX_SOURCE.to_owned(), "application/json", replacement)]);
          } else {
            assert!(left.is_none() && right.is_none());
          }
          Ok(())
        },
      )
      .unwrap();
    assert_eq!(result.captured_header().slot_sequence, original_header);
    assert!(publisher.observe().unwrap().selected.header.slot_sequence > original_header);
    assert_eq!(result.fingerprint().digest(), union_digest(algorithm, &expected));
    assert_eq!(nodes.len(), 1);
    for bytes in [&nodes[0].0, &nodes[0].1] {
      let rows: SourceMap = decode_semantic_source_node_v1(bytes, algorithm)
        .unwrap()
        .leaf_entries()
        .unwrap()
        .map(|row| {
          let row = row.unwrap();
          (row.path.to_owned(), row.file_record_id.map(Vec::from))
        })
        .collect();
      assert_eq!(rows, expected);
    }
    // A retained copy created during callbacks is absent from the old settled
    // lookup, while a fresh capture reads it and the new current path separately.
    assert_eq!(
      capture.read_retained_protected_source(INDEX_SOURCE, &original_revision, source_bounds()).err().unwrap().code(),
      "semantic_source_retained_missing"
    );
    let fresh = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    assert_eq!(fresh.read_retained_protected_source(INDEX_SOURCE, &original_revision, source_bounds()).unwrap().body(), original);
    assert_eq!(fresh.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap().body(), replacement);
    assert_eq!(capture.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap().body(), original);
    drop(fresh);
    drop(result);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
  }
}

#[test]
fn native_semantic_source_union_actual_root_allocation_failure_preserves_cause_and_retry() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    super::with_union_fixture(algorithm, &[], |publisher, capture, _, _, root, tree, parent| {
      let length = publisher.locator(root).unwrap().unwrap().total_length as usize;
      let (result, allocations) = allocation_probe::measure(length, || {
        capture.prepare_semantic_source_union(
          super::empty_union_request(root, tree, parent),
          |_, _| panic!("failed allocation emitted"),
          |_, _, _| panic!("failed allocation visited"),
        )
      });
      assert!(allocations.injected_failure, "{allocations:?}");
      assert_eq!(super::source_code(result.err().unwrap()), "first_authority_readback_allocation");
      drop(capture.prepare_semantic_source_union(super::empty_union_request(root, tree, parent), |_, _| Ok(()), |_, _, _| Ok(())).unwrap());
    });
  }
}

#[test]
fn native_semantic_source_union_builds_multiple_catalog_leaves_from_bounded_deduplicated_runs() {
  use crate::engine::v4::plugin_identity::plugin_alias_path_v1;
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    super::with_union_fixture(algorithm, &[], |_, capture, _, _, root, tree, parent| {
      let mut names: Vec<_> = (0..300).map(|index| plugin_alias_path_v1(&format!("unused-{index:03}")).unwrap()).collect();
      names.sort();
      let replacements: Vec<_> = names.iter().map(|path| NativeSemanticSourceReplacementV1 { path, file_record_id: None }).collect();
      let mut request = super::empty_union_request(root, tree, parent);
      request.replacements = &replacements;
      let mut nodes = BTreeMap::new();
      let mut rows = SourceMap::new();
      let result = capture
        .prepare_semantic_source_union(
          request,
          |left, right| {
            assert_eq!(left, right);
            let node = decode_semantic_source_node_v1(left, algorithm).unwrap();
            if let Some(entries) = node.leaf_entries() {
              for entry in entries {
                let entry = entry.unwrap();
                assert!(rows.insert(entry.path.to_owned(), entry.file_record_id.map(Vec::from)).is_none());
              }
            } else {
              for child in node.children().unwrap() {
                assert!(nodes.contains_key(child.unwrap().node_id), "children must precede parents");
              }
            }
            let id = digest_parts(algorithm, &[b"aeordb.semantic-source-node.v1\0", &left[32..left.len() - 4]]);
            assert!(nodes.insert(id, left.to_vec()).is_none());
            Ok(())
          },
          |_, left, right| {
            assert!(left.is_none() && right.is_none());
            Ok(())
          },
        )
        .unwrap();
      let mut expected = absent_globals();
      for name in names {
        expected.insert(name, None);
      }
      assert_eq!(rows, expected);
      assert_eq!(nodes.len(), 3);
      assert_eq!(result.catalogs().node_count(), 3);
      assert_eq!(result.catalogs().path_count(), 302);
      assert!(nodes.contains_key(result.catalogs().base_root()));
      assert_eq!(result.catalogs().base_root(), result.catalogs().requested_root());
      assert_eq!(result.fingerprint().record_count(), 302);
      assert_eq!(result.fingerprint().digest(), union_digest(algorithm, &expected));
    });
  }
}
