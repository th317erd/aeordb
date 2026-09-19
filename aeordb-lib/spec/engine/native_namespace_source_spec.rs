//! Actual captured files with independently specified record and child bytes.
#[path = "native_semantic_task_graph_spec.rs"]
mod task_graph;
use super::*;
#[path = "native_namespace_cursor_spec.rs"]
mod cursor;
#[path = "native_namespace_cursor_boundary_spec.rs"]
mod cursor_boundary;
#[path = "native_namespace_cursor_seek_spec.rs"]
mod cursor_seek;
#[path = "native_namespace_cursor_seek_boundary_spec.rs"]
mod cursor_seek_boundary;
#[path = "native_semantic_source_union_spec.rs"]
mod source_union;
#[path = "native_namespace_source_structure_spec.rs"]
mod structure;
#[path = "native_namespace_source_validation_spec.rs"]
mod validation;

struct NamespaceFixtureChild {
  name: String,
  kind: EntryTypeV4,
  key: Vec<u8>,
  size: u64,
  content_type: Option<&'static str>,
}

fn publish_namespace_value(publisher: &V4FirstAuthorityPublisher, kind: EntryTypeV4, version: u8, domain: &[u8], bytes: &[u8]) -> Vec<u8> {
  let header = publisher.observe().unwrap().selected.header;
  let key = digest_parts(header.hash_algorithm, &[domain, bytes]);
  publisher
    .publish_immutable_entity_batch(ImmutableEntityBatchPublicationRequestV1 {
      database_id: &[1; 16],
      entities: &[ImmutableEntityWriteV1 { entity_version: version, entry_type: kind, flags: 0, key: &key, stored_value: bytes }],
      publication_timestamp_ms: header.updated_at_ms + 1,
    })
    .unwrap();
  key
}

fn publish_namespace_configuration(publisher: &V4FirstAuthorityPublisher, path: &str, body: &[u8]) -> (Vec<u8>, Vec<u8>) {
  let algorithm = publisher.observe().unwrap().selected.header.hash_algorithm;
  let chunk = publish_namespace_value(publisher, EntryTypeV4::Chunk, 0, b"chunk:", body);
  let content_type = b"application/json";
  let mut record = Vec::new();
  record.extend_from_slice(&(path.len() as u16).to_le_bytes());
  record.extend_from_slice(path.as_bytes());
  record.extend_from_slice(&(content_type.len() as u16).to_le_bytes());
  record.extend_from_slice(content_type);
  record.extend_from_slice(&(body.len() as u64).to_le_bytes());
  record.extend_from_slice(&11i64.to_le_bytes());
  record.extend_from_slice(&13i64.to_le_bytes());
  record.extend_from_slice(&digest_parts(algorithm, &[body]));
  record.extend_from_slice(&4u32.to_le_bytes());
  record.extend_from_slice(&[1, 0, 2, 255]);
  record.extend_from_slice(&1u32.to_le_bytes());
  record.extend_from_slice(&chunk);
  let revision = publish_namespace_value(publisher, EntryTypeV4::FileRecord, 1, b"filec:", &record);
  (revision, record)
}

fn publish_namespace_directory(publisher: &V4FirstAuthorityPublisher, mut children: Vec<NamespaceFixtureChild>) -> Vec<u8> {
  children.sort_unstable_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
  let mut bytes = Vec::new();
  for child in children {
    bytes.push(child.kind.to_u8());
    bytes.extend_from_slice(&child.key);
    bytes.extend_from_slice(&child.size.to_le_bytes());
    bytes.extend_from_slice(&11i64.to_le_bytes());
    bytes.extend_from_slice(&13i64.to_le_bytes());
    bytes.extend_from_slice(&(child.name.len() as u16).to_le_bytes());
    bytes.extend_from_slice(child.name.as_bytes());
    let content_type = child.content_type.unwrap_or("");
    bytes.extend_from_slice(&(content_type.len() as u16).to_le_bytes());
    bytes.extend_from_slice(content_type.as_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
  }
  publish_namespace_value(publisher, EntryTypeV4::DirectoryIndex, 0, b"dirc:", &bytes)
}

fn namespace_directory_child(name: &str, key: Vec<u8>) -> NamespaceFixtureChild {
  NamespaceFixtureChild { name: name.to_string(), kind: EntryTypeV4::DirectoryIndex, key, size: 0, content_type: None }
}

fn namespace_configuration_tree(
  publisher: &V4FirstAuthorityPublisher,
  owner: &str,
  body: &[u8],
  mut extra: Vec<NamespaceFixtureChild>,
) -> (Vec<u8>, Vec<u8>) {
  let path = format!("{owner}/.aeordb-config/indexes.json");
  let (revision, _) = publish_namespace_configuration(publisher, &path, body);
  let configuration = publish_namespace_directory(
    publisher,
    vec![NamespaceFixtureChild {
      name: "indexes.json".to_string(),
      kind: EntryTypeV4::FileRecord,
      key: revision.clone(),
      size: body.len() as u64,
      content_type: Some("application/json"),
    }],
  );
  extra.push(namespace_directory_child(".aeordb-config", configuration));
  (publish_namespace_directory(publisher, extra), revision)
}

fn namespace_source_request(tree_root: &[u8]) -> NativeSemanticNamespaceSourceRequestV1<'_> {
  NativeSemanticNamespaceSourceRequestV1 {
    tree_root,
    bounds: NativeSemanticNamespaceSourceBoundsV1 {
      maximum_path_bytes: 1024,
      maximum_path_depth: 8,
      maximum_btree_depth: 8,
      maximum_directory_entity_bytes: 1 << 20,
      maximum_work: 4096,
      sources: source_bounds(),
    },
  }
}

#[test]
fn native_namespace_source_reads_exact_content_revision_without_a_current_path_record() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("namespace-exact-source", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    let source_path = "/docs/.aeordb-config/indexes.json";
    let body = b"{ \"$v\": 1, \"indexes\": [] }\n";
    let (revision, record) = publish_namespace_configuration(&publisher, source_path, body);
    assert!(publisher.lock_kv().unwrap().get(&first_authority_file_path_hash(source_path, algorithm)).unwrap().is_none());
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let source = capture
      .read_namespace_configuration_source(source_path, &revision, source_bounds())
      .expect("exact namespace source must read from the captured content key");
    assert_eq!(source.body(), body);
    assert_eq!(source.encoded_record(), record);
    assert_eq!(source.revision(), revision);
    assert_eq!(source.record().path, source_path);
    assert_eq!(source.record().metadata, [1, 0, 2, 255]);
    assert_eq!(source.entity_version(), 1);
    assert_eq!(source.flags(), 0);
    assert!(capture.read_protected_source(source_path, source_bounds()).is_err());
    drop(source);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_namespace_source_visits_nested_configurations_in_complete_path_order() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("namespace-source-order", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    let body = br#"{"$v":1,"indexes":[]}"#;
    let (nested, _) = namespace_configuration_tree(&publisher, "/a/child", body, vec![]);
    let (first, _) = namespace_configuration_tree(&publisher, "/a", body, vec![namespace_directory_child("child", nested)]);
    let (dash, _) = namespace_configuration_tree(&publisher, "/a-", body, vec![]);
    let (dot, _) = namespace_configuration_tree(&publisher, "/a.", body, vec![]);
    let root = publish_namespace_directory(
      &publisher,
      vec![
        namespace_directory_child("a", first),
        namespace_directory_child("a-", dash),
        namespace_directory_child("a.", dot),
        NamespaceFixtureChild {
          name: "ordinary.txt".to_string(),
          kind: EntryTypeV4::FileRecord,
          key: vec![8; algorithm.hash_length()],
          size: 0,
          content_type: None,
        },
      ],
    );
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let mut paths = Vec::new();
    let summary = capture
      .visit_namespace_configuration_sources(namespace_source_request(&root), |source| {
        assert_eq!(source.body(), body);
        assert!(publisher.root_state.try_lock().is_ok());
        assert!(publisher.kv.try_lock().is_ok());
        paths.push(source.record().path.clone());
        Ok(true)
      })
      .expect("captured namespace configuration traversal must complete");
    assert_eq!(
      paths,
      [
        "/a-/.aeordb-config/indexes.json",
        "/a./.aeordb-config/indexes.json",
        "/a/.aeordb-config/indexes.json",
        "/a/child/.aeordb-config/indexes.json"
      ]
    );
    assert_eq!(summary, NativeSemanticNamespaceSourceSummaryV1 { configurations: 4, complete: true });
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_namespace_source_old_capture_stays_fixed_after_new_source_and_tree_publication() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("namespace-source-capture", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    let (old_directory, old_revision) = namespace_configuration_tree(&publisher, "/docs", b"old", vec![]);
    let old_root = publish_namespace_directory(&publisher, vec![namespace_directory_child("docs", old_directory)]);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let old = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let (new_directory, new_revision) = namespace_configuration_tree(&publisher, "/docs", b"new", vec![]);
    let new_root = publish_namespace_directory(&publisher, vec![namespace_directory_child("docs", new_directory)]);
    let fresh = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let mut old_bodies = Vec::new();
    old
      .visit_namespace_configuration_sources(namespace_source_request(&old_root), |source| {
        assert_eq!(source.revision(), old_revision);
        old_bodies.push(source.body().to_vec());
        Ok(true)
      })
      .expect("old namespace capture must preserve its exact sources");
    assert_eq!(old_bodies, [b"old".to_vec()]);
    let source_path = "/docs/.aeordb-config/indexes.json";
    assert!(old.read_namespace_configuration_source(source_path, &new_revision, source_bounds()).is_err());
    let mut new_bodies = Vec::new();
    fresh
      .visit_namespace_configuration_sources(namespace_source_request(&new_root), |source| {
        assert_eq!(source.revision(), new_revision);
        new_bodies.push(source.body().to_vec());
        Ok(true)
      })
      .unwrap();
    assert_eq!(new_bodies, [b"new".to_vec()]);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
