//! Independent byte and resource regressions for source-capture writers.
use super::*;
use aeordb::engine::v4::semantic_source_capture::{
  encode_semantic_source_capture_v1, encode_semantic_source_internal_v1, encode_semantic_source_leaf_v1,
};

#[test]
fn capture_byte_writers_have_one_fallible_output_and_one_bounded_identity_allocation() {
  for (algorithm, profile) in PROFILES {
    for (kind, identity_bytes) in [
      ("semantic-source-capture", 24),
      ("semantic-source-node", algorithm.hash_length()),
      ("semantic-source-node-internal", algorithm.hash_length()),
    ] {
      let expected = control(profile, kind);
      let manifest = (kind == "semantic-source-capture").then(|| decode_semantic_source_capture_v1(&expected, algorithm).unwrap());
      let node = (kind != "semantic-source-capture").then(|| decode_semantic_source_node_v1(&expected, algorithm).unwrap());
      let rows = node.and_then(|node| node.leaf_entries()).map(|rows| rows.collect::<Result<Vec<_>, _>>().unwrap());
      let children = node.and_then(|node| node.children()).map(|rows| rows.collect::<Result<Vec<_>, _>>().unwrap());
      let encode = || {
        if let Some(manifest) = &manifest {
          encode_semantic_source_capture_v1(manifest, algorithm)
        } else if let Some(rows) = &rows {
          encode_semantic_source_leaf_v1(node.unwrap().database_id(), rows, algorithm)
        } else {
          encode_semantic_source_internal_v1(node.unwrap().database_id(), children.as_ref().unwrap(), algorithm)
        }
      };
      let (result, allocations) = measure(0, encode);
      assert_eq!(result.unwrap(), expected);
      assert_eq!(allocations.total, expected.len() + identity_bytes, "{kind}: {allocations:?}");
      assert_eq!(allocations.maximum, expected.len());
      for (size, code) in [(expected.len(), "system_control_output_allocation"), (identity_bytes, "semantic_capture_identity_allocation")] {
        let (result, allocations) = measure(size, encode);
        assert!(allocations.injected_failure, "{kind}: {allocations:?}");
        let error = result.unwrap_err();
        assert!(error.is_allocation_failure());
        assert_eq!(error.code(), code);
        assert_eq!(encode().unwrap(), expected);
      }
    }
  }
}

#[test]
fn capture_node_writer_capacity_refuses_before_large_output_allocation() {
  use aeordb::engine::v4::semantic_source_capture::{SemanticSourceChildV1, SemanticSourceLeafEntryV1};
  for (algorithm, _) in PROFILES {
    let paths: Vec<_> = (0..16).map(|index| format!("/{index:03}/{}", "x".repeat(65_530))).collect();
    let hashes: Vec<_> = (1..=17).map(|index| vec![index as u8; algorithm.hash_length()]).collect();
    let rows: Vec<_> = paths.iter().map(|path| SemanticSourceLeafEntryV1 { path, file_record_id: None }).collect();
    let mut children = vec![SemanticSourceChildV1 { separator: None, node_id: &hashes[0] }];
    for (index, path) in paths.iter().enumerate() {
      children.push(SemanticSourceChildV1 { separator: Some(path), node_id: &hashes[index + 1] });
    }
    for internal in [false, true] {
      let (result, allocations) = measure(0, || {
        if internal {
          encode_semantic_source_internal_v1(&[1; 16], &children, algorithm)
        } else {
          encode_semantic_source_leaf_v1(&[1; 16], &rows, algorithm)
        }
      });
      assert!(result.is_err());
      assert!(allocations.maximum <= 512, "{allocations:?}");
      let (result, allocations) = measure(0, || {
        if internal {
          encode_semantic_source_internal_v1(&[1; 16], &children[..16], algorithm)
        } else {
          encode_semantic_source_leaf_v1(&[1; 16], &rows[..15], algorithm)
        }
      });
      let output = result.unwrap();
      assert_eq!(allocations.maximum, output.len());
      assert_eq!(allocations.total, output.len() + algorithm.hash_length());
    }
  }
}

#[test]
fn capture_manifest_claimed_graph_counts_do_not_allocate_a_collection() {
  for (algorithm, profile) in PROFILES {
    let bytes = control(profile, "semantic-source-capture");
    let mut input = decode_semantic_source_capture_v1(&bytes, algorithm).unwrap();
    input.protected_path_count = u64::MAX / 2;
    input.base_catalog_node_count = u64::MAX - 2;
    input.requested_catalog_node_count = u64::MAX - 2;
    let (result, allocations) = measure(0, || encode_semantic_source_capture_v1(&input, algorithm));
    let encoded = result.unwrap();
    assert_eq!(allocations.maximum, bytes.len());
    assert_eq!(allocations.total, bytes.len() + 24);
    let decoded = decode_semantic_source_capture_v1(&encoded, algorithm).unwrap();
    assert_eq!(decoded.protected_path_count, input.protected_path_count);
    assert_eq!(decoded.base_catalog_node_count, input.base_catalog_node_count);
    assert_eq!(decoded.requested_catalog_node_count, input.requested_catalog_node_count);
    // These are structurally legal claims, never proof of a real graph.
  }
}

#[test]
fn capture_node_invalid_requests_refuse_before_allocating_the_output() {
  use aeordb::engine::v4::semantic_source_capture::{SemanticSourceChildV1, SemanticSourceLeafEntryV1};
  for (algorithm, _) in PROFILES {
    let first = vec![1; algorithm.hash_length()];
    let second = vec![2; algorithm.hash_length()];
    let zero = vec![0; algorithm.hash_length()];
    let short = vec![3; algorithm.hash_length() - 1];
    let long_path = format!("/{}", "x".repeat(65_534));
    let valid = SemanticSourceLeafEntryV1 { path: &long_path, file_record_id: Some(&first) };
    for case in 0..8 {
      let mut rows = [valid, valid];
      rows[1].path = "/z";
      let mut children = [
        SemanticSourceChildV1 { separator: None, node_id: &first },
        SemanticSourceChildV1 { separator: Some(&long_path), node_id: &second },
      ];
      match case {
        0 => rows[1].file_record_id = Some(&zero),
        1 => rows[1].file_record_id = Some(&short),
        2 => rows[1].path = "/a/../b",
        3 => rows[1].path = &long_path,
        4 => children[1].node_id = &first,
        5 => children[1].separator = None,
        6 => children[0].separator = Some("/a"),
        7 => children[1].node_id = &short,
        _ => unreachable!(),
      }
      let (result, allocations) = measure(0, || {
        if case < 4 {
          encode_semantic_source_leaf_v1(&[1; 16], &rows, algorithm)
        } else {
          encode_semantic_source_internal_v1(&[1; 16], &children, algorithm)
        }
      });
      assert!(result.is_err());
      assert!(allocations.maximum <= 512, "case{case}: {allocations:?}");
    }
  }
}
