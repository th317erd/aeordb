//! Positive targets for the owned, cumulatively budgeted native source sinks.
#[path = "native_staged_source_union_boundary_spec.rs"]
mod boundary;
use super::*;

pub(super) fn staging_request() -> NativeSemanticSourceUnionStagingRequestV1 {
  NativeSemanticSourceUnionStagingRequestV1 {
    publication_timestamp_ms: 100,
    maximum_source_copy_attempts: 16,
    maximum_payload_bytes: 16 << 20,
    maximum_validation_read_bytes: 32 << 20,
    maximum_node_workspace_bytes: 16 << 20,
  }
}

fn staged_union_case(algorithm: HashAlgorithm, scenario: u8) {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("owned-source-union", None, [1; 16], algorithm, 0);
  let initial = request_for_database_and_algorithm([1; 16], algorithm);
  let root = publisher.publish(&initial).unwrap().namespace_root.root_hash;
  let mut header = publisher.observe().unwrap().selected.header;
  header.required_reader_capabilities[3] |= 0b1010;
  header.required_writer_capabilities[3] |= 0b1010;
  write_redundant_header(&publisher, &header);
  seed_union_generation(&publisher);
  let original = br#"{"$v":1,"indexes":[]}"#;
  let replacement = br#"{ "$v": 1, "indexes": [] }"#;
  let parser = br#"{"$v":1,"parsers":{}}"#;
  if scenario != 0 {
    seed_files(&publisher, &[(INDEX_SOURCE.to_owned(), "application/json", original)]);
  }
  if scenario == 1 {
    seed_files(&publisher, &[(PARSER_SOURCE.to_owned(), "application/json", parser)]);
  }
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let mut expected_sources = BTreeMap::new();
  let mut expected_nodes = BTreeMap::new();
  {
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let old_revision = if scenario == 2 {
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let source = capture.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
      source.stage_retained_copy(source_bounds(), 90).unwrap();
      let identity = source.revision().to_vec();
      drop(source);
      drop(capture);
      seed_files(&publisher, &[(INDEX_SOURCE.to_owned(), "application/json", replacement)]);
      Some(identity)
    } else {
      None
    };
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let mut base = absent_globals();
    let mut requested = absent_globals();
    for path in [INDEX_SOURCE, PARSER_SOURCE] {
      for request_side in [false, true] {
        let source = match (request_side && path == INDEX_SOURCE, old_revision.as_ref()) {
          (true, Some(revision)) => Some(capture.read_retained_protected_source(path, revision, source_bounds()).unwrap()),
          _ => capture.read_protected_source(path, source_bounds()).unwrap(),
        };
        if let Some(source) = source {
          let revision = source.revision().to_vec();
          let reads: u64 =
            source.record().chunk_hashes.iter().map(|key| u64::from(publisher.locator(key).unwrap().unwrap().total_length)).sum();
          expected_sources.insert(revision.clone(), (path.to_owned(), source.body().to_vec(), reads, source.encoded_record().len() as u64));
          if request_side {
            requested.insert(path.to_owned(), Some(revision));
          } else {
            base.insert(path.to_owned(), Some(revision));
          }
        }
      }
    }
    let replacements: Vec<_> = old_revision
      .iter()
      .map(|revision| NativeSemanticSourceReplacementV1 { path: INDEX_SOURCE, file_record_id: Some(revision.as_slice()) })
      .collect();
    let parent = tempfile::tempdir().unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let result = capture
      .prepare_and_stage_semantic_source_union(
        NativeSemanticSourceUnionRequestV1 {
          expected_base_root: &root,
          requested_directory_root: &initial.namespace_tree.root_hash,
          replacements: &replacements,
          workspace_parent: parent.path(),
          bounds: union_bounds(&initial.namespace_tree.root_hash),
        },
        staging_request(),
      )
      .unwrap();
    let union = result.source_union();
    assert_eq!(union.fingerprint().digest(), union_digest(algorithm, &base));
    assert_eq!(union.catalogs().path_count(), 2);
    for (identity, expected) in [(union.catalogs().base_root(), &base), (union.catalogs().requested_root(), &requested)] {
      let bytes =
        publisher.load_immutable_system_control(SystemControlKindV1::SemanticSourceNode, &[1; 16], identity).unwrap().unwrap().bytes;
      let node = decode_semantic_source_node_v1(&bytes, algorithm).unwrap();
      let actual: SourceMap = node
        .leaf_entries()
        .unwrap()
        .map(|row| {
          let row = row.unwrap();
          (row.path.to_owned(), row.file_record_id.map(Vec::from))
        })
        .collect();
      assert_eq!(&actual, expected);
      expected_nodes.insert(identity.to_vec(), bytes);
    }
    let summary = result.summary();
    assert_eq!(summary.node_pair_attempts, 1);
    assert_eq!(summary.node_control_attempts, expected_nodes.len() as u64);
    assert_eq!(summary.source_copy_attempts, expected_sources.len() as u64);
    assert_eq!(summary.validation_read_bytes, expected_sources.values().map(|row| row.2).sum::<u64>());
    assert_eq!(
      summary.attempted_payload_bytes,
      expected_sources.values().map(|row| row.3).sum::<u64>() + expected_nodes.values().map(|bytes| bytes.len() as u64).sum::<u64>()
    );
    assert_eq!(publisher.observe().unwrap().selected.header.head_hash, root);
    drop(result);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
  }
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  drop(publisher);
  let (_coordinator, reopened) = reopen(&path);
  let before = fs::read(&path).unwrap();
  let protection = reopened.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  for (identity, bytes) in expected_nodes {
    assert_eq!(
      reopened.load_immutable_system_control(SystemControlKindV1::SemanticSourceNode, &[1; 16], &identity).unwrap().unwrap().bytes,
      bytes
    );
  }
  for (revision, (path, body, _, _)) in expected_sources {
    assert_eq!(capture.read_retained_protected_source(&path, &revision, source_bounds()).unwrap().body(), body);
  }
  assert_eq!(capture.visit(|_| panic!("source staging must not select a task")).unwrap().tasks, 0);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_staged_source_union_empty_globals_still_persist_a_real_catalog() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    staged_union_case(algorithm, 0);
  }
}

#[test]
fn native_staged_source_union_present_globals_have_exact_cumulative_read_and_payload_counts() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    staged_union_case(algorithm, 1);
  }
}

#[test]
fn native_staged_source_union_distinct_old_and_requested_sources_reopen_exactly() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    staged_union_case(algorithm, 2);
  }
}

#[test]
fn native_staged_source_union_metered_retained_copy_preserves_receipt_and_counts_repeat_reads() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("metered-retained-source", None, [1; 16], algorithm, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
  seed_files(&publisher, &[(INDEX_SOURCE.to_owned(), "application/json", br#"{"$v":1,"indexes":[]}"#)]);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let source = capture.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
  let expected: u64 = source.record().chunk_hashes.iter().map(|key| u64::from(publisher.locator(key).unwrap().unwrap().total_length)).sum();
  assert!(expected > 0);
  let (receipt, reads) = source.stage_retained_copy_metered(source_bounds(), 100).unwrap();
  assert!(!receipt.idempotent);
  assert_eq!(reads, expected);
  let before = fs::read(&path).unwrap();
  let (retry, retry_reads) = source.stage_retained_copy_metered(source_bounds(), 101).unwrap();
  assert!(retry.idempotent);
  assert_eq!(retry_reads, expected);
  assert_eq!(retry.entities[0].key, receipt.entities[0].key);
  assert_eq!(retry.entities[0].write_sequence, receipt.entities[0].write_sequence);
  assert_eq!(fs::read(&path).unwrap(), before);
}
