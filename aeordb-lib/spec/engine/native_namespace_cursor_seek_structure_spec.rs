//! Reuse independently encoded directory fixtures at the existing test owner.
use super::*;

#[test]
fn namespace_cursor_seek_does_not_read_or_certify_skipped_configuration_bodies() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("namespace-seek-skipped-body", None, [1; 16], algorithm, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
  let missing_configuration = publish_namespace_directory(
    &publisher,
    vec![NamespaceFixtureChild {
      name: "indexes.json".to_string(),
      kind: EntryTypeV4::FileRecord,
      key: vec![7; 32],
      size: 2,
      content_type: Some("application/json"),
    }],
  );
  let a = publish_namespace_directory(&publisher, vec![namespace_directory_child(".aeordb-config", missing_configuration)]);
  let (b, _) = namespace_configuration_tree(&publisher, "/b", b"retained", vec![]);
  let root = publish_namespace_directory(&publisher, vec![namespace_directory_child("a", a), namespace_directory_child("b", b)]);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let before = fs::read(&path).unwrap();
  let mut from_start = capture.open_namespace_configuration_cursor(namespace_source_request(&root)).unwrap();
  assert_eq!(from_start.next_source().err().unwrap().code(), "semantic_source_retained_missing");
  drop(from_start);
  let mut cursor =
    capture.open_namespace_configuration_cursor_after(namespace_source_request(&root), "/a/.aeordb-config/indexes.json").unwrap();
  assert_eq!(cursor.next_source().unwrap().unwrap().body(), b"retained");
  assert!(cursor.next_source().unwrap().is_none());
  drop(cursor);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn namespace_cursor_seek_validates_inherited_ranges_and_shapes_before_absence() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("namespace-seek-shape", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    let (a, _) = namespace_configuration_tree(&publisher, "/a", b"a", vec![]);
    let (z, _) = namespace_configuration_tree(&publisher, "/z", b"z", vec![]);
    let left_row = namespace_row_bytes(&namespace_directory_child("a", a), 11, 13);
    let right_row = namespace_row_bytes(&namespace_directory_child("z", z), 11, 13);
    let leaf = |row: &[u8]| {
      let mut bytes = vec![0, 1, 0];
      bytes.extend_from_slice(row);
      publish_namespace_value(&publisher, EntryTypeV4::DirectoryIndex, 0, b"btree:", &bytes)
    };
    let left = leaf(&left_row);
    let right = leaf(&right_row);
    let flat = publish_namespace_value(&publisher, EntryTypeV4::DirectoryIndex, 0, b"dirc:", &left_row);
    let valid = namespace_btree_root(&publisher, &left, &right, "m");
    // The right leaf contains a (<m). A late seek must not turn that invalid
    // inherited range into successful absence merely because a < the bound.
    let bad_range = namespace_btree_root(&publisher, &right, &left, "m");
    let flat_child = namespace_btree_root(&publisher, &flat, &right, "m");
    let missing_child = namespace_btree_root(&publisher, &left, &vec![7; algorithm.hash_length()], "m");
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(&path).unwrap();
    let mut request = namespace_source_request(&valid);
    request.bounds.maximum_btree_depth = 2;
    let mut cursor = capture.open_namespace_configuration_cursor_after(request, "/y/.aeordb-config/indexes.json").unwrap();
    assert_eq!(cursor.next_source().unwrap().unwrap().body(), b"z");
    assert!(cursor.next_source().unwrap().is_none());
    drop(cursor);
    for (root, after) in [
      (&bad_range, "/y/.aeordb-config/indexes.json"),
      (&flat_child, "/b/.aeordb-config/indexes.json"),
      (&missing_child, "/y/.aeordb-config/indexes.json"),
    ] {
      assert!(capture.open_namespace_configuration_cursor_after(namespace_source_request(root), after).is_err());
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    }
    request.bounds.maximum_btree_depth = 1;
    assert!(capture.open_namespace_configuration_cursor_after(request, "/y/.aeordb-config/indexes.json").is_err());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn namespace_cursor_seek_missing_or_corrupt_successor_is_terminal_not_end() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("namespace-seek-missing", None, [1; 16], algorithm, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
  let missing = vec![7; 32];
  let missing_directory = publish_namespace_directory(&publisher, vec![namespace_directory_child("z", missing.clone())]);
  let configuration = publish_namespace_directory(
    &publisher,
    vec![NamespaceFixtureChild {
      name: "indexes.json".to_string(),
      kind: EntryTypeV4::FileRecord,
      key: missing,
      size: 2,
      content_type: Some("application/json"),
    }],
  );
  let owner = publish_namespace_directory(&publisher, vec![namespace_directory_child(".aeordb-config", configuration)]);
  let missing_source = publish_namespace_directory(&publisher, vec![namespace_directory_child("z", owner)]);
  let (directory, revision) = namespace_configuration_tree(&publisher, "/z", b"body", vec![]);
  let corrupt = publish_namespace_directory(&publisher, vec![namespace_directory_child("z", directory)]);
  rewrite_namespace_flags(&publisher, &revision, WHOLE_ENTITY_V1_FLAG_SYSTEM);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let before = fs::read(&path).unwrap();
  for (root, code) in [
    (&missing_directory, "semantic_namespace_source_directory_missing"),
    (&missing_source, "semantic_source_retained_missing"),
    (&corrupt, "semantic_source_record_representation"),
  ] {
    let mut cursor =
      capture.open_namespace_configuration_cursor_after(namespace_source_request(root), "/a/.aeordb-config/indexes.json").unwrap();
    assert_eq!(cursor.next_source().err().expect("broken successor must refuse").code(), code);
    assert_eq!(cursor.next_source().err().unwrap().code(), "semantic_namespace_cursor_failed");
    drop(cursor);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  }
  assert_eq!(fs::read(&path).unwrap(), before);
}
