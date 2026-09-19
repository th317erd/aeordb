//! Real source-union sinks and reopen readback; no task selection is inferred.
use super::*;

fn enable_node_staging(publisher: &V4FirstAuthorityPublisher) {
  let mut header = publisher.observe().unwrap().selected.header;
  header.required_reader_capabilities[3] |= 0b1010;
  header.required_writer_capabilities[3] |= 0b1010;
  write_redundant_header(publisher, &header);
}

#[test]
fn native_semantic_source_union_persists_distinct_catalogs_and_retained_sources_through_reopen() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-union-persistent-sinks", None, [1; 16], algorithm, 0);
    let initial = request_for_database_and_algorithm([1; 16], algorithm);
    let root = publisher.publish(&initial).unwrap().namespace_root.root_hash;
    enable_node_staging(&publisher);
    seed_union_generation(&publisher);
    let previous_body = br#"{"$v":1,"indexes":[]}"#;
    let current_body = br#"{ "$v": 1, "indexes": [] }"#;
    seed_files(&publisher, &[(INDEX_SOURCE.to_owned(), "application/json", previous_body)]);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let previous_revision;
    let current_revision;
    let mut expected_nodes = BTreeMap::new();
    {
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      {
        let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
        let source = capture.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
        previous_revision = source.revision().to_vec();
        source.stage_retained_copy(source_bounds(), publisher.observe().unwrap().selected.header.updated_at_ms + 1).unwrap();
      }
      seed_files(&publisher, &[(INDEX_SOURCE.to_owned(), "application/json", current_body)]);
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      current_revision = capture.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap().revision().to_vec();
      assert_ne!(previous_revision, current_revision);
      let replacements = [NativeSemanticSourceReplacementV1 { path: INDEX_SOURCE, file_record_id: Some(&previous_revision) }];
      let parent = tempfile::tempdir().unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let mut visited = Vec::new();
      let result = capture
        .prepare_semantic_source_union(
          NativeSemanticSourceUnionRequestV1 {
            expected_base_root: &root,
            requested_directory_root: &initial.namespace_tree.root_hash,
            replacements: &replacements,
            workspace_parent: parent.path(),
            bounds: union_bounds(&initial.namespace_tree.root_hash),
          },
          |left, right| {
            let receipt = capture.stage_semantic_source_nodes(NativeSemanticSourceNodeStagingRequestV1 {
              encoded_nodes: &[left, right],
              publication_timestamp_ms: publisher.observe().unwrap().selected.header.updated_at_ms + 1,
              maximum_workspace_bytes: 16 << 20,
            })?;
            assert_eq!(receipt.controls.len(), 2);
            for bytes in [left, right] {
              let decoded = decode_system_control(bytes, algorithm).unwrap();
              assert!(expected_nodes.insert(decoded.identity, bytes.to_vec()).is_none());
            }
            Ok(())
          },
          |path, base, requested| {
            visited.push(path.to_owned());
            if path == INDEX_SOURCE {
              assert_eq!(base.unwrap().body(), current_body);
              assert_eq!(requested.unwrap().body(), previous_body);
            } else {
              assert!(base.is_none() && requested.is_none());
            }
            for source in [base, requested].into_iter().flatten() {
              source.stage_retained_copy(source_bounds(), publisher.observe().unwrap().selected.header.updated_at_ms + 1)?;
            }
            Ok(())
          },
        )
        .unwrap();
      assert_eq!(visited, [INDEX_SOURCE, PARSER_SOURCE]);
      assert_eq!(result.catalogs().path_count(), 2);
      assert_eq!(result.catalogs().node_count(), 1);
      assert_ne!(result.catalogs().base_root(), result.catalogs().requested_root());
      for (node_id, revision) in
        [(result.catalogs().base_root(), &current_revision), (result.catalogs().requested_root(), &previous_revision)]
      {
        let bytes = expected_nodes.get(node_id).unwrap();
        let node = decode_semantic_source_node_v1(bytes, algorithm).unwrap();
        let actual: SourceMap = node
          .leaf_entries()
          .unwrap()
          .map(|row| {
            let row = row.unwrap();
            (row.path.to_owned(), row.file_record_id.map(Vec::from))
          })
          .collect();
        let mut expected = absent_globals();
        expected.insert(INDEX_SOURCE.to_owned(), Some(revision.clone()));
        assert_eq!(actual, expected);
      }
      let mut fingerprint = absent_globals();
      fingerprint.insert(INDEX_SOURCE.to_owned(), Some(current_revision.clone()));
      assert_eq!(result.fingerprint().digest(), union_digest(algorithm, &fingerprint));
      assert_eq!(publisher.observe().unwrap().selected.header.head_hash, root);
      drop(result);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
      assert_eq!(capture.visit(|_| panic!("node publication must not select a task")).unwrap().tasks, 0);
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    seed_files(&publisher, &[(INDEX_SOURCE.to_owned(), "application/json", b"later current contents")]);
    drop(publisher);
    let (_coordinator, reopened) = reopen(&path);
    let before = fs::read(&path).unwrap();
    let protection = reopened.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    for (identity, expected) in expected_nodes {
      assert_eq!(
        reopened.load_immutable_system_control(SystemControlKindV1::SemanticSourceNode, &[1; 16], &identity).unwrap().unwrap().bytes,
        expected
      );
    }
    for (revision, body) in [(&previous_revision, previous_body.as_slice()), (&current_revision, current_body.as_slice())] {
      assert_eq!(capture.read_retained_protected_source(INDEX_SOURCE, revision, source_bounds()).unwrap().body(), body);
    }
    assert_eq!(capture.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap().body(), b"later current contents");
    assert_eq!(capture.visit(|_| panic!("reopen must not invent a task")).unwrap().tasks, 0);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_source_union_preserves_committed_node_error_and_retries_exactly() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("source-union-committed-error", None, [1; 16], algorithm, 0);
  let initial = request_for_database_and_algorithm([1; 16], algorithm);
  let root = publisher.publish(&initial).unwrap().namespace_root.root_hash;
  enable_node_staging(&publisher);
  seed_union_generation(&publisher);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let parent = tempfile::tempdir().unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let timestamp = publisher.observe().unwrap().selected.header.updated_at_ms + 1;
  let request = || NativeSemanticSourceUnionRequestV1 {
    expected_base_root: &root,
    requested_directory_root: &initial.namespace_tree.root_hash,
    replacements: &[],
    workspace_parent: parent.path(),
    bounds: union_bounds(&initial.namespace_tree.root_hash),
  };
  let error = capture
    .prepare_semantic_source_union(
      request(),
      |left, right| {
        capture.stage_semantic_source_nodes_observed(
          NativeSemanticSourceNodeStagingRequestV1 {
            encoded_nodes: &[left, right],
            publication_timestamp_ms: timestamp,
            maximum_workspace_bytes: 16 << 20,
          },
          || {},
          &mut FailingPostCommitObserver,
        )?;
        Ok(())
      },
      |_, _, _| Ok(()),
    )
    .err()
    .unwrap();
  let NativeSemanticSourceUnionErrorV1::ControlPublication(error) = error else {
    panic!("original control-publication cause must survive the builder adapter")
  };
  assert_eq!(error.code(), "immutable_entity_committed_postcondition_failure");
  let receipt = error.committed_receipt().unwrap();
  assert_eq!(receipt.controls.len(), 1);
  assert!(publisher.locator(&receipt.controls[0].path_key).unwrap().is_some());
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
  let result = capture
    .prepare_semantic_source_union(
      request(),
      |left, right| {
        assert!(
          capture
            .stage_semantic_source_nodes(NativeSemanticSourceNodeStagingRequestV1 {
              encoded_nodes: &[left, right],
              publication_timestamp_ms: timestamp + 1,
              maximum_workspace_bytes: 16 << 20,
            })?
            .idempotent
        );
        Ok(())
      },
      |_, _, _| Ok(()),
    )
    .unwrap();
  assert_eq!(result.catalogs().path_count(), 2);
  assert_eq!(publisher.observe().unwrap().selected.header.head_hash, root);
  drop(result);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
}
