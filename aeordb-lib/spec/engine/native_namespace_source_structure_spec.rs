//! Independent directory frames, ordinary source roles and shared metadata.
use super::*;
#[path = "native_namespace_cursor_seek_structure_spec.rs"]
mod cursor_seek_structure;

#[test]
fn native_namespace_source_reads_version_zero_records_and_empty_bodies_at_both_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for body in [b"".as_slice(), b"version-zero-body".as_slice()] {
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("namespace-version-zero", None, [1; 16], algorithm, 0);
      publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
      let source_path = "/docs/.aeordb-config/indexes.json";
      let (_, version_one) = publish_namespace_configuration(&publisher, source_path, body);
      let content_hash_offset = 2 + source_path.len() + 2 + b"application/json".len() + 8 + 8 + 8;
      let mut version_zero = version_one[..content_hash_offset].to_vec();
      version_zero.extend_from_slice(&version_one[content_hash_offset + algorithm.hash_length()..]);
      let revision = publish_namespace_value(&publisher, EntryTypeV4::FileRecord, 0, b"filec:", &version_zero);
      let before = fs::read(&path).unwrap();
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let source = capture
        .read_namespace_configuration_source(
          source_path,
          &revision,
          NativeSemanticSourceReadBoundsV1 { maximum_body_bytes: body.len(), ..source_bounds() },
        )
        .unwrap();
      assert_eq!(source.entity_version(), 0);
      assert_eq!(source.body(), body);
      assert_eq!(source.encoded_record(), version_zero);
      drop(source);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

fn namespace_row_bytes(child: &NamespaceFixtureChild, created: i64, updated: i64) -> Vec<u8> {
  let mut bytes = vec![child.kind.to_u8()];
  bytes.extend_from_slice(&child.key);
  bytes.extend_from_slice(&child.size.to_le_bytes());
  bytes.extend_from_slice(&created.to_le_bytes());
  bytes.extend_from_slice(&updated.to_le_bytes());
  bytes.extend_from_slice(&(child.name.len() as u16).to_le_bytes());
  bytes.extend_from_slice(child.name.as_bytes());
  let content_type = child.content_type.unwrap_or("");
  bytes.extend_from_slice(&(content_type.len() as u16).to_le_bytes());
  bytes.extend_from_slice(content_type.as_bytes());
  bytes.extend_from_slice(&0u64.to_le_bytes());
  bytes.extend_from_slice(&0u64.to_le_bytes());
  bytes
}

fn namespace_parent_tree(publisher: &V4FirstAuthorityPublisher, configuration: Vec<u8>) -> Vec<u8> {
  let owner = publish_namespace_directory(publisher, vec![namespace_directory_child(".aeordb-config", configuration)]);
  publish_namespace_directory(publisher, vec![namespace_directory_child("docs", owner)])
}

#[test]
fn native_namespace_source_checks_every_directory_to_record_metadata_field() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for case in ["path", "size", "content-type", "created", "updated"] {
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("namespace-metadata", None, [1; 16], algorithm, 0);
      publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
      let record_path = if case == "path" { "/other/.aeordb-config/indexes.json" } else { "/docs/.aeordb-config/indexes.json" };
      let (revision, _) = publish_namespace_configuration(&publisher, record_path, b"{}");
      let child = NamespaceFixtureChild {
        name: "indexes.json".to_string(),
        kind: EntryTypeV4::FileRecord,
        key: revision,
        size: if case == "size" { 3 } else { 2 },
        content_type: Some(if case == "content-type" { "text/plain" } else { "application/json" }),
      };
      let bytes = namespace_row_bytes(&child, if case == "created" { 12 } else { 11 }, if case == "updated" { 14 } else { 13 });
      let directory = publish_namespace_value(&publisher, EntryTypeV4::DirectoryIndex, 0, b"dirc:", &bytes);
      let root = namespace_parent_tree(&publisher, directory);
      let before = fs::read(&path).unwrap();
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let error = capture
        .visit_namespace_configuration_sources(namespace_source_request(&root), |_| panic!("mismatched source emitted"))
        .unwrap_err();
      assert_eq!(error.code(), if case == "path" { "semantic_source_record_path" } else { "selected_namespace_corrupt" });
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

fn namespace_btree_root(publisher: &V4FirstAuthorityPublisher, left: &[u8], right: &[u8], separator: &str) -> Vec<u8> {
  let mut bytes = vec![1, 1, 0];
  bytes.extend_from_slice(&(separator.len() as u16).to_le_bytes());
  bytes.extend_from_slice(separator.as_bytes());
  bytes.extend_from_slice(left);
  bytes.extend_from_slice(right);
  publish_namespace_value(publisher, EntryTypeV4::DirectoryIndex, 0, b"btree:", &bytes)
}

#[test]
fn native_namespace_source_reuses_btree_ranges_child_shapes_and_depth_bounds() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("namespace-btree", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    let (left_directory, _) = namespace_configuration_tree(&publisher, "/a", b"left", vec![]);
    let (right_directory, _) = namespace_configuration_tree(&publisher, "/z", b"right", vec![]);
    let left_row = namespace_row_bytes(&namespace_directory_child("a", left_directory), 11, 13);
    let right_row = namespace_row_bytes(&namespace_directory_child("z", right_directory), 11, 13);
    let leaf = |row: &[u8]| {
      let mut bytes = vec![0, 1, 0];
      bytes.extend_from_slice(row);
      publish_namespace_value(&publisher, EntryTypeV4::DirectoryIndex, 0, b"btree:", &bytes)
    };
    let left = leaf(&left_row);
    let right = leaf(&right_row);
    let flat = publish_namespace_value(&publisher, EntryTypeV4::DirectoryIndex, 0, b"dirc:", &left_row);
    let root = namespace_btree_root(&publisher, &left, &right, "m");
    let bad_range = namespace_btree_root(&publisher, &left, &right, "0");
    let flat_child = namespace_btree_root(&publisher, &flat, &right, "m");
    let repeated_child = namespace_btree_root(&publisher, &left, &left, "m");
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let mut request = namespace_source_request(&root);
    request.bounds.maximum_btree_depth = 2;
    let mut paths = Vec::new();
    let summary = capture
      .visit_namespace_configuration_sources(request, |source| {
        paths.push(source.record().path.clone());
        Ok(true)
      })
      .unwrap();
    assert!(summary.complete);
    assert_eq!(paths, ["/a/.aeordb-config/indexes.json", "/z/.aeordb-config/indexes.json"]);
    request.bounds.maximum_btree_depth = 1;
    assert!(capture.visit_namespace_configuration_sources(request, |_| panic!("too-shallow traversal emitted")).is_err());
    for root in [&bad_range, &flat_child, &repeated_child] {
      assert!(capture
        .visit_namespace_configuration_sources(namespace_source_request(root), |_| panic!("malformed B-tree emitted"))
        .is_err());
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

fn rewrite_namespace_flags(publisher: &V4FirstAuthorityPublisher, key: &[u8], flags: u8) {
  let header = publisher.observe().unwrap().selected.header;
  let kv = publisher.lock_kv().unwrap();
  let locator = kv.get(key).unwrap().unwrap();
  let bytes = read_entity_bounded(&publisher.file, &*kv, key, 4 << 20, header.write_sequence_high_water).unwrap().unwrap();
  let entity = decode_whole_entity(&bytes, header.hash_algorithm, header.write_sequence_high_water).unwrap();
  let replacement = encode_entity(
    entity.entity_version,
    entity.entry_type,
    flags,
    header.hash_algorithm,
    EntityPublicationOrder { timestamp_ms: entity.timestamp_ms, write_sequence: entity.write_sequence },
    entity.key,
    entity.stored_value,
  )
  .unwrap();
  assert_eq!(replacement.len(), bytes.len());
  write_file_at_native(&publisher.file, locator.offset, &replacement).unwrap();
}

#[test]
fn native_namespace_source_does_not_admit_protected_file_or_chunk_flags() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for chunk in [false, true] {
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("namespace-ordinary-flags", None, [1; 16], algorithm, 0);
      publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
      let source_path = "/docs/.aeordb-config/indexes.json";
      let (revision, _) = publish_namespace_configuration(&publisher, source_path, b"{}");
      let key = if chunk { digest_parts(algorithm, &[b"chunk:", b"{}"]) } else { revision.clone() };
      rewrite_namespace_flags(&publisher, &key, WHOLE_ENTITY_V1_FLAG_SYSTEM);
      let before = fs::read(&path).unwrap();
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let error = capture
        .read_namespace_configuration_source(source_path, &revision, source_bounds())
        .err()
        .expect("namespace source must remain ordinary");
      assert_eq!(error.code(), if chunk { "semantic_source_chunk_representation" } else { "semantic_source_record_representation" });
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

#[test]
fn native_namespace_source_missing_directories_and_configs_are_errors_not_absence() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("namespace-missing", None, [1; 16]);
  publisher.publish(&request_for_database_and_algorithm([1; 16], HashAlgorithm::Blake3_256)).unwrap();
  let missing = vec![7; 32];
  let missing_directory = publish_namespace_directory(&publisher, vec![namespace_directory_child("docs", missing.clone())]);
  let configuration = publish_namespace_directory(
    &publisher,
    vec![NamespaceFixtureChild {
      name: "indexes.json".to_string(),
      kind: EntryTypeV4::FileRecord,
      key: missing.clone(),
      size: 2,
      content_type: Some("application/json"),
    }],
  );
  let missing_source = namespace_parent_tree(&publisher, configuration);
  let wrong_role = publish_namespace_value(&publisher, EntryTypeV4::Chunk, 0, b"chunk:", b"not a directory");
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  for root in [&missing, &missing_directory, &missing_source, &wrong_role] {
    assert!(capture.visit_namespace_configuration_sources(namespace_source_request(root), |_| panic!("missing source emitted")).is_err());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_namespace_source_body_allocation_failure_releases_then_retries() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("namespace-body-allocation", None, [1; 16]);
  publisher.publish(&request_for_database_and_algorithm([1; 16], HashAlgorithm::Blake3_256)).unwrap();
  let body = vec![b'x'; 997];
  let (directory, revision) = namespace_configuration_tree(&publisher, "/docs", &body, vec![]);
  let root = publish_namespace_directory(&publisher, vec![namespace_directory_child("docs", directory)]);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let (result, allocations) = allocation_probe::measure(body.len(), || {
    capture.read_namespace_configuration_source("/docs/.aeordb-config/indexes.json", &revision, source_bounds())
  });
  assert!(allocations.injected_failure, "{allocations:?}");
  assert_eq!(result.err().expect("body allocation must refuse").code(), "semantic_source_body_allocation");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  let (result, allocations) = allocation_probe::measure(body.len(), || {
    capture.visit_namespace_configuration_sources(namespace_source_request(&root), |_| panic!("allocation failure emitted a source"))
  });
  assert!(allocations.injected_failure, "{allocations:?}");
  assert_eq!(result.unwrap_err().code(), "semantic_source_body_allocation");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  let summary = capture
    .visit_namespace_configuration_sources(namespace_source_request(&root), |source| {
      assert_eq!(source.body(), body);
      Ok(true)
    })
    .unwrap();
  assert!(summary.complete);
  assert_eq!(summary.configurations, 1);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}
