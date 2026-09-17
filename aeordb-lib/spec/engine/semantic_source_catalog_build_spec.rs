//! Independent bytes and emitted dependency graph, not a retention permit.
#[path = "semantic_source_catalog_build_graph_spec.rs"]
mod graph;
#[path = "semantic_source_catalog_build_validation_spec.rs"]
mod validation;
use super::*;
use std::collections::BTreeMap;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1;
use aeordb::engine::v4::semantic_source_capture::{
  build_semantic_source_catalog_pair_v1, SemanticSourceCatalogBuildRequestV1, SemanticSourceCatalogPairRowV1,
};

fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(64 << 20, 96 << 20, 1, 16 << 20).unwrap())
}

fn request(algorithm: HashAlgorithm, count: u64) -> SemanticSourceCatalogBuildRequestV1 {
  SemanticSourceCatalogBuildRequestV1 {
    database_id: [1; 16],
    hash_algorithm: algorithm,
    expected_path_count: count,
    maximum_path_bytes: 65_535,
    maximum_workspace_bytes: 32 << 20,
    maximum_node_pairs: 100_000,
    maximum_output_bytes: 128 << 20,
  }
}

fn row(index: usize, algorithm: HashAlgorithm) -> SemanticSourceCatalogPairRowV1 {
  SemanticSourceCatalogPairRowV1 {
    path: format!("/p/{index:05}"),
    base_file_record_id: index.is_multiple_of(2).then(|| vec![1; algorithm.hash_length()]),
    requested_file_record_id: (!index.is_multiple_of(3)).then(|| vec![2; algorithm.hash_length()]),
  }
}

fn leaf(algorithm: HashAlgorithm, rows: &[SemanticSourceCatalogPairRowV1], requested: bool) -> Vec<u8> {
  let mut body = vec![0; 32];
  body[..16].fill(1);
  body[16..18].copy_from_slice(&1u16.to_le_bytes());
  body[20..24].copy_from_slice(&(rows.len() as u32).to_le_bytes());
  for row in rows {
    body.extend_from_slice(&(row.path.len() as u32).to_le_bytes());
    body.extend_from_slice(row.path.as_bytes());
    let identity = if requested { &row.requested_file_record_id } else { &row.base_file_record_id };
    match identity {
      Some(identity) => body.extend_from_slice(identity),
      None => body.resize(body.len() + algorithm.hash_length(), 0),
    }
  }
  let length = (body.len() - 32) as u32;
  body[24..28].copy_from_slice(&length.to_le_bytes());
  envelope(b"ASCN", 1, &body)
}

fn identity(bytes: &[u8], algorithm: HashAlgorithm) -> Vec<u8> {
  let decoded = decode_system_control(bytes, algorithm).unwrap();
  let mut preimage = b"aeordb.semantic-source-node.v1\0".to_vec();
  preimage.extend_from_slice(decoded.body);
  independent_digest(algorithm, &preimage)
}

#[test]
fn catalog_build_pair_matches_independent_two_path_leaves_for_every_hash() {
  for algorithm in ALGORITHMS {
    let rows = vec![row(0, algorithm), row(1, algorithm)];
    let expected_base = leaf(algorithm, &rows, false);
    let expected_requested = leaf(algorithm, &rows, true);
    let memory = memory();
    let mut emissions = 0;
    let result = build_semantic_source_catalog_pair_v1(
      request(algorithm, 2),
      rows.into_iter().map(Ok),
      &mut |base, requested| {
        assert_eq!(base, expected_base);
        assert_eq!(requested, expected_requested);
        emissions += 1;
        Ok(())
      },
      &memory,
      &|| false,
    )
    .expect("paired ordered assembly must succeed");
    assert_eq!(emissions, 1);
    assert_eq!(result.path_count(), 2);
    assert_eq!(result.node_count(), 1);
    assert_eq!(result.base_root(), identity(&expected_base, algorithm));
    assert_eq!(result.requested_root(), identity(&expected_requested, algorithm));
    assert!(memory.snapshot().unwrap().reserved_bytes > 0);
    drop(result);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

fn collect_rows(
  root: &[u8],
  nodes: &BTreeMap<Vec<u8>, Vec<u8>>,
  algorithm: HashAlgorithm,
  output: &mut Vec<(String, Option<Vec<u8>>)>,
  depth: usize,
) {
  assert!(depth <= 8);
  let node = decode_semantic_source_node_v1(&nodes[root], algorithm).unwrap();
  if let Some(rows) = node.leaf_entries() {
    for row in rows {
      let row = row.unwrap();
      output.push((row.path.to_owned(), row.file_record_id.map(Vec::from)));
    }
  } else {
    for child in node.children().unwrap() {
      collect_rows(child.unwrap().node_id, nodes, algorithm, output, depth + 1);
    }
  }
}

#[test]
fn catalog_build_pair_splits_odd_leaf_count_and_emits_children_first() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let rows: Vec<_> = (0..1025).map(|index| row(index, algorithm)).collect();
    let mut base_nodes = BTreeMap::new();
    let mut requested_nodes = BTreeMap::new();
    let result = build_semantic_source_catalog_pair_v1(
      request(algorithm, rows.len() as u64),
      rows.clone().into_iter().map(Ok),
      &mut |base, requested| {
        for (bytes, nodes) in [(base, &mut base_nodes), (requested, &mut requested_nodes)] {
          let node = decode_semantic_source_node_v1(bytes, algorithm).unwrap();
          if let Some(children) = node.children() {
            let children: Vec<_> = children.map(Result::unwrap).collect();
            assert_eq!(children.len(), 2);
            for child in children {
              assert!(nodes.contains_key(child.node_id));
            }
          }
          assert!(nodes.insert(identity(bytes, algorithm), bytes.to_vec()).is_none());
        }
        Ok(())
      },
      &memory(),
      &|| false,
    )
    .expect("five leaves must form one bounded dependency-first tree");
    assert_eq!(result.path_count(), 1025);
    assert_eq!(result.node_count(), 9);
    assert_eq!(base_nodes.len(), 9);
    assert_eq!(requested_nodes.len(), 9);
    for (root, nodes, requested) in [(result.base_root(), &base_nodes, false), (result.requested_root(), &requested_nodes, true)] {
      let mut actual = Vec::new();
      collect_rows(root, nodes, algorithm, &mut actual, 0);
      let expected: Vec<_> = rows
        .iter()
        .map(|row| (row.path.clone(), if requested { row.requested_file_record_id.clone() } else { row.base_file_record_id.clone() }))
        .collect();
      assert_eq!(actual, expected);
    }
  }
}

#[test]
fn catalog_build_pair_preserves_sink_failure_and_releases_its_workspace() {
  let algorithm = HashAlgorithm::Blake3_256;
  let memory = memory();
  let mut emissions = 0;
  let result = build_semantic_source_catalog_pair_v1(
    request(algorithm, 513),
    (0..513).map(|index| Ok(row(index, algorithm))),
    &mut |_, _| {
      emissions += 1;
      if emissions == 3 {
        return Err(SemanticCompilationErrorV1::Operational { path: "test-sink", message: "refused node pair".into() });
      }
      Ok(())
    },
    &memory,
    &|| false,
  );
  assert!(matches!(result, Err(SemanticCompilationErrorV1::Operational { path: "test-sink", .. })));
  assert_eq!(emissions, 3);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}
