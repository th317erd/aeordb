//! Complete small-map oracles with real captured files and independent preimages.
#[path = "native_semantic_source_union_boundary_spec.rs"]
mod boundary;
#[path = "native_staged_source_union_spec.rs"]
mod staged_source;
#[path = "native_semantic_source_union_staging_spec.rs"]
mod staging;
use super::*;
use super::super::plugin_fixtures;
use std::collections::BTreeMap;
use crate::engine::v4::semantic_source_capture::{decode_semantic_source_node_v1, SemanticSourcePathWorkspaceBoundsV1};

const PARSER_SOURCE: &str = "/.aeordb-config/parsers.json";
type SourceMap = BTreeMap<String, Option<Vec<u8>>>;

fn union_bounds(tree: &[u8]) -> NativeSemanticSourceUnionBoundsV1 {
  let mut namespace = namespace_source_request(tree).bounds;
  namespace.maximum_work = 100_000;
  namespace.sources.maximum_read_bytes = 64 << 20;
  NativeSemanticSourceUnionBoundsV1 {
    namespace,
    paths: SemanticSourcePathWorkspaceBoundsV1 {
      maximum_input_paths: 1024,
      maximum_path_bytes: 1024,
      maximum_sort_bytes: 128 << 10,
      maximum_stored_bytes: 1 << 20,
      maximum_io_bytes: 16 << 20,
      maximum_paths_per_run: 1,
      merge_fan_in: 2,
      minimum_free_bytes: 0,
    },
    maximum_plugin_module_bytes: 1 << 20,
    maximum_plugin_workspace_bytes: 64 << 10,
    maximum_alias_occurrences: 1024,
    maximum_alias_workspace_bytes: 16 << 20,
    maximum_catalog_workspace_bytes: 32 << 20,
    maximum_catalog_node_pairs: 1024,
    maximum_catalog_output_bytes: 4 << 20,
    maximum_fingerprint_workspace_bytes: 64 << 10,
  }
}

fn seed_union_generation(publisher: &V4FirstAuthorityPublisher) {
  let algorithm = publisher.observe().unwrap().selected.header.hash_algorithm;
  let mut bytes = frozen(algorithm, "generation");
  bytes[16..24].copy_from_slice(&10u64.to_le_bytes());
  crc(&mut bytes);
  seed(publisher, &[(SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::A, &bytes)]);
}

fn absent_globals() -> SourceMap {
  [(INDEX_SOURCE.to_owned(), None), (PARSER_SOURCE.to_owned(), None)].into_iter().collect()
}

fn union_digest(algorithm: HashAlgorithm, rows: &SourceMap) -> Vec<u8> {
  let mut bytes = b"aeordb.semantic-mutation-sources.v1\0".to_vec();
  for (path, revision) in rows {
    bytes.extend_from_slice(&(path.len() as u32).to_le_bytes());
    bytes.extend_from_slice(path.as_bytes());
    if let Some(revision) = revision {
      bytes.extend_from_slice(revision);
    } else {
      bytes.resize(bytes.len() + algorithm.hash_length(), 0);
    }
  }
  match algorithm {
    HashAlgorithm::Blake3_256 => blake3::hash(&bytes).as_bytes().to_vec(),
    HashAlgorithm::Sha512 => {
      use sha2::Digest;
      sha2::Sha512::digest(&bytes).to_vec()
    }
    _ => panic!("fixture specifies only its two independent digest implementations"),
  }
}

fn assert_small_union(
  capture: &NativeSemanticMutationInventoryV1<'_>,
  request: NativeSemanticSourceUnionRequestV1<'_>,
  algorithm: HashAlgorithm,
  base: &SourceMap,
  requested: &SourceMap,
  fingerprint: &SourceMap,
  memory: &MemoryCoordinator,
) {
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let mut nodes = Vec::new();
  let mut observed_base = SourceMap::new();
  let mut observed_requested = SourceMap::new();
  let expected_root = request.expected_base_root.to_vec();
  let expected_tree = request.requested_directory_root.to_vec();
  let parent = request.workspace_parent;
  let result = capture
    .prepare_semantic_source_union(
      request,
      |left, right| {
        nodes.push((left.to_vec(), right.to_vec()));
        Ok(())
      },
      |path, left, right| {
        assert!(observed_base.insert(path.to_owned(), left.map(|source| source.revision().to_vec())).is_none());
        assert!(observed_requested.insert(path.to_owned(), right.map(|source| source.revision().to_vec())).is_none());
        Ok(())
      },
    )
    .expect("complete captured union must succeed");
  assert_eq!(&observed_base, base);
  assert_eq!(&observed_requested, requested);
  assert_eq!(result.base_authority().root_hash, expected_root);
  assert_eq!(result.requested_directory_root(), expected_tree);
  assert_eq!(result.generation_selection().control_sequence, 10);
  assert!(result.captured_header().slot_sequence > result.base_authority().selected_header_slot_sequence);
  assert_eq!(result.catalogs().path_count(), base.len() as u64);
  assert_eq!(result.catalogs().node_count(), 1);
  assert_eq!(nodes.len(), 1);
  for (bytes, expected, expected_root) in
    [(&nodes[0].0, base, result.catalogs().base_root()), (&nodes[0].1, requested, result.catalogs().requested_root())]
  {
    let node = decode_semantic_source_node_v1(bytes, algorithm).unwrap();
    let rows: SourceMap = node
      .leaf_entries()
      .unwrap()
      .map(|row| {
        let row = row.unwrap();
        (row.path.to_owned(), row.file_record_id.map(Vec::from))
      })
      .collect();
    assert_eq!(&rows, expected);
    assert_eq!(expected_root, digest_parts(algorithm, &[b"aeordb.semantic-source-node.v1\0", &bytes[32..bytes.len() - 4]]));
  }
  assert_eq!(result.fingerprint().record_count(), fingerprint.len() as u64);
  assert_eq!(result.fingerprint().digest(), union_digest(algorithm, fingerprint));
  assert_eq!(fs::read_dir(parent).unwrap().count(), 0, "finished preparation must release its private path sorter");
  assert!(memory.snapshot().unwrap().reserved_bytes > retained);
  drop(result);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
}

#[test]
fn native_semantic_source_union_empty_trees_include_both_absent_globals() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-union-empty", None, [1; 16], algorithm, 0);
    let initial = request_for_database_and_algorithm([1; 16], algorithm);
    let root = publisher.publish(&initial).unwrap().namespace_root.root_hash;
    seed_union_generation(&publisher);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let parent = tempfile::tempdir().unwrap();
    let before = fs::read(&path).unwrap();
    let globals = absent_globals();
    assert_small_union(
      &capture,
      NativeSemanticSourceUnionRequestV1 {
        expected_base_root: &root,
        requested_directory_root: &initial.namespace_tree.root_hash,
        replacements: &[],
        workspace_parent: parent.path(),
        bounds: union_bounds(&initial.namespace_tree.root_hash),
      },
      algorithm,
      &globals,
      &globals,
      &globals,
      &memory,
    );
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_source_union_merges_namespace_additions_changes_and_removals() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-union-namespaces", None, [1; 16], algorithm, 0);
    let initial = request_for_database_and_algorithm([1; 16], algorithm);
    publisher.publish(&initial).unwrap();
    let body = br#"{"$v":1,"indexes":[]}"#;
    let (a_tree, a_revision) = namespace_configuration_tree(&publisher, "/a", body, vec![]);
    let (b_old_tree, b_old_revision) = namespace_configuration_tree(&publisher, "/b", body, vec![]);
    let base_tree =
      publish_namespace_directory(&publisher, vec![namespace_directory_child("a", a_tree), namespace_directory_child("b", b_old_tree)]);
    let mut next = successor_request(&publisher, 0x72, "ignored-fixture-child");
    next.semantic_state = initial.semantic_state;
    next.namespace_tree = PreparedNamespaceTreeV0 {
      root_hash: base_tree.clone(),
      stored_value: publisher.load_immutable_entity_bounded(&base_tree, 1 << 20).unwrap().unwrap().stored_value,
    };
    let root = publisher.publish_successor_authority(&next).unwrap().namespace_root.root_hash;
    let (b_new_tree, b_new_revision) = namespace_configuration_tree(&publisher, "/b", br#"{ "$v": 1, "indexes": [] }"#, vec![]);
    assert_ne!(b_new_revision, b_old_revision);
    let (c_tree, _) = namespace_configuration_tree(&publisher, "/c", body, vec![]);
    let requested_tree =
      publish_namespace_directory(&publisher, vec![namespace_directory_child("b", b_new_tree), namespace_directory_child("c", c_tree)]);
    seed_union_generation(&publisher);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let parent = tempfile::tempdir().unwrap();
    let before = fs::read(&path).unwrap();
    let globals = absent_globals();
    let mut fingerprint = globals.clone();
    fingerprint.insert("/a/.aeordb-config/indexes.json".into(), Some(a_revision));
    fingerprint.insert("/b/.aeordb-config/indexes.json".into(), Some(b_old_revision));
    fingerprint.insert("/c/.aeordb-config/indexes.json".into(), None);
    assert_small_union(
      &capture,
      NativeSemanticSourceUnionRequestV1 {
        expected_base_root: &root,
        requested_directory_root: &requested_tree,
        replacements: &[],
        workspace_parent: parent.path(),
        bounds: union_bounds(&requested_tree),
      },
      algorithm,
      &globals,
      &globals,
      &fingerprint,
      &memory,
    );
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

fn seed_union_plugin(publisher: &V4FirstAuthorityPublisher, module: &[u8], role: &str) {
  let alias = plugin_fixtures::alias(module, role);
  seed_files(
    publisher,
    &[
      (plugin_fixtures::artifact_path(module), "application/wasm", module),
      (plugin_fixtures::alias_path(), "application/octet-stream", &alias),
    ],
  );
}

#[test]
fn native_semantic_source_union_keeps_both_alias_selections_after_last_reference_removal() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-union-selected-alias", None, [1; 16], algorithm, 0);
    let initial = request_for_database_and_algorithm([1; 16], algorithm);
    let root = publisher.publish(&initial).unwrap().namespace_root.root_hash;
    let module_a = plugin_fixtures::module("both");
    let module_b = plugin_fixtures::module("parser");
    let alias_path = plugin_fixtures::alias_path();
    let artifact_a_path = plugin_fixtures::artifact_path(&module_a);
    let artifact_b_path = plugin_fixtures::artifact_path(&module_b);
    seed_union_plugin(&publisher, &module_a, "both");
    seed_files(
      &publisher,
      &[(
        PARSER_SOURCE.to_owned(),
        "application/json",
        br#"{"$v":1,"parsers":{"text/a":"parse","text/b":"parse","application/c":"parse"}}"#,
      )],
    );
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let old = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let mut retained = Vec::new();
    for name in [&alias_path, &artifact_a_path] {
      let source = old.read_protected_source(name, source_bounds()).unwrap().unwrap();
      source.stage_retained_copy(source_bounds(), publisher.observe().unwrap().selected.header.updated_at_ms + 1).unwrap();
      retained.push(source.revision().to_vec());
    }
    drop(old);
    seed_union_plugin(&publisher, &module_b, "parser");
    seed_union_generation(&publisher);
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let mut base = absent_globals();
    for name in [PARSER_SOURCE, &alias_path, &artifact_a_path, &artifact_b_path] {
      base.insert(name.to_owned(), Some(capture.read_protected_source(name, source_bounds()).unwrap().unwrap().revision().to_vec()));
    }
    let mut requested = base.clone();
    requested.insert(PARSER_SOURCE.to_owned(), None);
    requested.insert(alias_path.clone(), Some(retained[0].clone()));
    let mut replacements = [
      NativeSemanticSourceReplacementV1 { path: PARSER_SOURCE, file_record_id: None },
      NativeSemanticSourceReplacementV1 { path: &alias_path, file_record_id: Some(&retained[0]) },
      NativeSemanticSourceReplacementV1 { path: &artifact_a_path, file_record_id: Some(&retained[1]) },
    ];
    replacements.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    let parent = tempfile::tempdir().unwrap();
    let before = fs::read(&path).unwrap();
    assert_small_union(
      &capture,
      NativeSemanticSourceUnionRequestV1 {
        expected_base_root: &root,
        requested_directory_root: &initial.namespace_tree.root_hash,
        replacements: &replacements,
        workspace_parent: parent.path(),
        bounds: union_bounds(&initial.namespace_tree.root_hash),
      },
      algorithm,
      &base,
      &requested,
      &base,
      &memory,
    );
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
