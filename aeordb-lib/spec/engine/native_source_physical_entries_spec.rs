//! Read-set fixtures only; these do not exercise native task publication.
use super::*;
use std::collections::BTreeMap;
#[path = "native_source_physical_entries_boundary_spec.rs"]
mod boundary;

type ExpectedPhysicalEntries = BTreeMap<Vec<u8>, (u8, u64, u32)>;

fn physical_catalog_fixture(publisher: &V4FirstAuthorityPublisher) -> ExpectedPhysicalEntries {
  let algorithm = publisher.observe().unwrap().selected.header.hash_algorithm;
  seed_raw_source(publisher, INDEX_SOURCE, 0, 1, 0, false, CompressionAlgorithm::Zstd);
  let old = seed_retained_revision(publisher, INDEX_SOURCE);
  let new_body = b"new captured input";
  seed_files(publisher, &[(INDEX_SOURCE.into(), "application/json", new_body)]);
  let new = seed_retained_revision(publisher, INDEX_SOURCE);
  let base = [(INDEX_SOURCE, Some(old.as_slice())), (PARSER_SOURCE, None), (ALIAS_SOURCE, None)];
  let requested = [(INDEX_SOURCE, Some(new.as_slice())), (PARSER_SOURCE, None), (ALIAS_SOURCE, None)];
  let companion = seed_catalog_pair(publisher, &base, &requested);
  seed_files(publisher, &[(INDEX_SOURCE.into(), "application/json", b"unrelated current input")]);

  // The oracle names the four fixture nodes explicitly, rather than walking
  // either tree with the implementation under test.
  let base_rows: Vec<_> = base.iter().map(|(path, revision)| SemanticSourceLeafEntryV1 { path, file_record_id: *revision }).collect();
  let requested_rows: Vec<_> =
    requested.iter().map(|(path, revision)| SemanticSourceLeafEntryV1 { path, file_record_id: *revision }).collect();
  let base_leaf = encode_semantic_source_leaf_v1(&[1; 16], &base_rows, algorithm).unwrap();
  let requested_left = encode_semantic_source_leaf_v1(&[1; 16], &requested_rows[..1], algorithm).unwrap();
  let requested_right = encode_semantic_source_leaf_v1(&[1; 16], &requested_rows[1..], algorithm).unwrap();
  let node_identity = |bytes: &[u8]| digest_parts(algorithm, &[b"aeordb.semantic-source-node.v1\0", &bytes[32..bytes.len() - 4]]);
  let left_id = node_identity(&requested_left);
  let right_id = node_identity(&requested_right);
  let requested_root = encode_semantic_source_internal_v1(
    &[1; 16],
    &[
      SemanticSourceChildV1 { separator: None, node_id: &left_id },
      SemanticSourceChildV1 { separator: Some(PARSER_SOURCE), node_id: &right_id },
    ],
    algorithm,
  )
  .unwrap();
  let mut expected_keys = BTreeMap::new();
  for key in [&old, &new] {
    expected_keys.insert(key.clone(), KV_TYPE_FILE_RECORD);
  }
  for body in [b"first".as_slice(), b"second"] {
    expected_keys.insert(digest_parts(algorithm, &[b"chunk:", body]), KV_TYPE_CHUNK);
  }
  expected_keys.insert(digest_parts(algorithm, &[b"system::", new_body]), KV_TYPE_CHUNK);
  let mut include_control = |kind, identity: &[u8], bytes: &[u8]| {
    let path = system_control_path(kind, identity, SystemControlSlotV1::Immutable).unwrap();
    expected_keys.insert(first_authority_file_path_hash(&path, algorithm), KV_TYPE_FILE_RECORD);
    expected_keys.insert(digest_parts(algorithm, &[b"system::", bytes]), KV_TYPE_CHUNK);
  };
  for node in [&base_leaf, &requested_left, &requested_right, &requested_root] {
    include_control(SystemControlKindV1::SemanticSourceNode, &node_identity(node), node);
  }
  include_control(SystemControlKindV1::SemanticSourceCapture, &checkpoint_identity(), &companion);
  include_control(SystemControlKindV1::SemanticMutationCheckpoint, &checkpoint_identity(), &frozen(algorithm, "checkpoint"));
  assert_eq!(expected_keys.len(), 17);
  expected_keys
    .into_iter()
    .map(|(key, role)| {
      let locator = publisher.locator(&key).unwrap().unwrap();
      assert_eq!(locator.type_flags, role);
      (key, (role, locator.offset, locator.total_length))
    })
    .collect()
}

#[test]
fn native_source_physical_entries_cover_exact_controls_nodes_retained_records_and_chunks_after_reopen() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-physical-entries", None, [1; 16], algorithm, 0);
    let expected = physical_catalog_fixture(&publisher);
    drop(publisher);
    let (_coordinator, reopened) = reopen(&path);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = reopened.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let mut actual = BTreeMap::new();
    let mut visits = 0;
    let summary = capture
      .visit_captured_source_physical_entries(&[2; 16], 1, catalog_bounds(), |entry| {
        assert!(reopened.root_state.try_lock().is_ok());
        assert!(reopened.kv.try_lock().is_ok());
        visits += 1;
        actual.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
        Ok(())
      })
      .unwrap();
    assert_eq!(summary, SemanticSourceCatalogSummaryV1 { paths: 3, base_nodes: 1, requested_nodes: 3, complete: true });
    assert_eq!(visits, 17);
    assert_eq!(actual, expected);
    assert!(!actual.contains_key(&first_authority_file_path_hash(INDEX_SOURCE, algorithm)));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_source_physical_entries_preserve_callback_cause_without_writes_and_allow_retry() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("source-physical-callback", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  let expected = physical_catalog_fixture(&publisher);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let mut calls = 0;
  let error = capture
    .visit_captured_source_physical_entries(&[2; 16], 1, catalog_bounds(), |_| {
      calls += 1;
      Err(SemanticMutationObservationErrorV1::Resource { code: "physical_entry_test_callback", message: "original visitor cause" })
    })
    .unwrap_err();
  assert!(matches!(
    error,
    SemanticMutationObservationErrorV1::Resource { code: "physical_entry_test_callback", message: "original visitor cause" }
  ));
  assert_eq!(calls, 1);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  let mut actual = BTreeMap::new();
  assert!(
    capture
      .visit_captured_source_physical_entries(&[2; 16], 1, catalog_bounds(), |entry| {
        actual.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
        Ok(())
      })
      .unwrap()
      .complete
  );
  assert_eq!(actual, expected);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}
