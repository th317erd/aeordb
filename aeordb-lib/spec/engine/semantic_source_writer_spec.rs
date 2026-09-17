//! Independent byte and resource regressions for source-capture writers.
use super::*;
use aeordb::engine::v4::semantic_source_capture::{
  encode_semantic_source_capture_v1, encode_semantic_source_internal_v1, encode_semantic_source_leaf_v1, SemanticSourceCaptureV1,
  SemanticSourceChildV1, SemanticSourceLeafEntryV1,
};

#[test]
fn source_capture_writer_matches_independent_manifest_at_every_hash_width() {
  for algorithm in ALGORITHMS {
    let expected = envelope(b"ASCM", 1, &manifest_body(algorithm));
    let input = decode_semantic_source_capture_v1(&expected, algorithm).unwrap();
    let actual = encode_semantic_source_capture_v1(&input, algorithm).unwrap();
    assert_eq!(actual, expected);
    let checkpoint = envelope(b"ASMC", 1, &checkpoint_body(algorithm, 1));
    assert!(decode_semantic_source_capture_binding_v1(&actual, &checkpoint, algorithm).is_ok());
  }
}

#[test]
fn source_capture_leaf_writer_matches_independent_absent_and_present_rows() {
  for algorithm in ALGORITHMS {
    let expected = envelope(b"ASCN", 1, &node_body(algorithm, false));
    let node = decode_semantic_source_node_v1(&expected, algorithm).unwrap();
    let rows = node.leaf_entries().unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(encode_semantic_source_leaf_v1(node.database_id(), &rows, algorithm).unwrap(), expected);
  }
}

#[test]
fn source_capture_internal_writer_matches_independent_children() {
  for algorithm in ALGORITHMS {
    let expected = envelope(b"ASCN", 1, &node_body(algorithm, true));
    let node = decode_semantic_source_node_v1(&expected, algorithm).unwrap();
    let children = node.children().unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(encode_semantic_source_internal_v1(node.database_id(), &children, algorithm).unwrap(), expected);
  }
}

fn set_capture_field<'a>(input: &mut SemanticSourceCaptureV1<'a>, index: usize, value: &'a [u8]) {
  match index {
    0 => input.database_id = value,
    1 => input.task_id = value,
    2 => input.physical_instance_id = value,
    3 => input.base_namespace_root = value,
    4 => input.staged_directory_root = value,
    5 => input.base_source_catalog = value,
    6 => input.requested_source_catalog = value,
    7 => input.source_identity_fingerprint = value,
    8 => input.checkpoint_payload_hash = value,
    _ => unreachable!(),
  }
}

#[test]
fn source_capture_manifest_writer_rejects_every_invalid_width_and_zero_identity() {
  for algorithm in ALGORITHMS {
    let expected = envelope(b"ASCM", 1, &manifest_body(algorithm));
    let input = decode_semantic_source_capture_v1(&expected, algorithm).unwrap();
    for index in 0..9 {
      let width = if index < 3 { 16 } else { algorithm.hash_length() };
      for invalid in [vec![0; width], vec![1; width - 1], vec![1; width + 1]] {
        let mut bad = input;
        set_capture_field(&mut bad, index, &invalid);
        assert!(encode_semantic_source_capture_v1(&bad, algorithm).is_err(), "field{index}");
      }
    }
  }
}

#[test]
fn source_capture_manifest_writer_preserves_scalar_and_count_refusals() {
  for algorithm in ALGORITHMS {
    let expected = envelope(b"ASCM", 1, &manifest_body(algorithm));
    let input = decode_semantic_source_capture_v1(&expected, algorithm).unwrap();
    for case in 0..12 {
      let mut bad = input;
      match case {
        0 => bad.checkpoint_sequence = 0,
        1 => bad.writer_fence_epoch = 0,
        2 => bad.semantic_generation = 0,
        3 => bad.header_sequence = 0,
        4 => bad.captured_at_ms = -1,
        5 => bad.protected_path_count = 0,
        6 => bad.protected_path_count = u64::MAX,
        7 => bad.base_catalog_node_count = 0,
        8 => bad.requested_catalog_node_count = 0,
        9 => bad.base_catalog_node_count = 4,
        10 => bad.requested_catalog_node_count = 4,
        11 => bad.protected_path_count = 1,
        _ => unreachable!(),
      }
      if case == 11 {
        bad.base_catalog_node_count = 2;
      }
      assert!(encode_semantic_source_capture_v1(&bad, algorithm).is_err(), "case{case}");
    }
  }
}

#[test]
fn source_capture_leaf_writer_refuses_bad_paths_order_counts_and_present_zero() {
  for algorithm in ALGORITHMS {
    let hash = vec![7; algorithm.hash_length()];
    let zero = vec![0; algorithm.hash_length()];
    let short = vec![7; algorithm.hash_length() - 1];
    let long = vec![7; algorithm.hash_length() + 1];
    let valid = SemanticSourceLeafEntryV1 { path: "/a", file_record_id: Some(&hash) };
    for identity in [&zero, &short, &long] {
      assert!(encode_semantic_source_leaf_v1(
        &[1; 16],
        &[SemanticSourceLeafEntryV1 { file_record_id: Some(identity), ..valid }],
        algorithm
      )
      .is_err());
    }
    for path in ["", "relative", "/a/../b", "/a//b", "/a/"] {
      assert!(encode_semantic_source_leaf_v1(&[1; 16], &[SemanticSourceLeafEntryV1 { path, ..valid }], algorithm).is_err(), "{path}");
    }
    assert!(encode_semantic_source_leaf_v1(&[0; 16], &[valid], algorithm).is_err());
    assert!(encode_semantic_source_leaf_v1(&[1; 15], &[valid], algorithm).is_err());
    assert!(encode_semantic_source_leaf_v1(&[1; 16], &[], algorithm).is_err());
    assert!(encode_semantic_source_leaf_v1(&[1; 16], &vec![valid; 257], algorithm).is_err());
    assert!(encode_semantic_source_leaf_v1(&[1; 16], &[valid, valid], algorithm).is_err());
    assert!(encode_semantic_source_leaf_v1(&[1; 16], &[SemanticSourceLeafEntryV1 { path: "/z", ..valid }, valid], algorithm).is_err());
    let oversized = format!("/{}", "x".repeat(65_535));
    assert!(encode_semantic_source_leaf_v1(&[1; 16], &[SemanticSourceLeafEntryV1 { path: &oversized, ..valid }], algorithm).is_err());
  }
}

#[test]
fn source_capture_internal_writer_refuses_bad_children_and_separators() {
  for algorithm in ALGORITHMS {
    let first = vec![1; algorithm.hash_length()];
    let second = vec![2; algorithm.hash_length()];
    let zero = vec![0; algorithm.hash_length()];
    let short = vec![1; algorithm.hash_length() - 1];
    let valid =
      [SemanticSourceChildV1 { separator: None, node_id: &first }, SemanticSourceChildV1 { separator: Some("/a"), node_id: &second }];
    assert!(encode_semantic_source_internal_v1(&[1; 16], &[], algorithm).is_err());
    assert!(encode_semantic_source_internal_v1(&[1; 16], &valid[..1], algorithm).is_err());
    for case in 0..7 {
      let mut bad = valid;
      match case {
        0 => bad[0].separator = Some("/first"),
        1 => bad[1].separator = None,
        2 => bad[1].separator = Some("relative"),
        3 => bad[1].node_id = &first,
        4 => bad[0].node_id = &zero,
        5 => bad[1].node_id = &zero,
        6 => bad[1].node_id = &short,
        _ => unreachable!(),
      }
      assert!(encode_semantic_source_internal_v1(&[1; 16], &bad, algorithm).is_err(), "case{case}");
    }
    assert!(encode_semantic_source_internal_v1(&[1; 16], &vec![valid[0]; 130], algorithm).is_err());
  }
}

#[test]
fn source_capture_node_writers_match_independent_maximum_fanout_and_long_paths() {
  for algorithm in ALGORITHMS {
    let width = algorithm.hash_length();
    for (internal, count, path_length) in [(false, 256usize, 12usize), (true, 128, 12), (false, 15, 65_535), (true, 15, 65_535)] {
      let paths: Vec<_> = (0..count).map(|index| format!("/{index:03}/{}", "x".repeat(path_length - 5))).collect();
      let hashes: Vec<_> = (0..=count)
        .map(|index| {
          let mut hash = vec![0; width];
          hash[..8].copy_from_slice(&(index as u64 + 1).to_le_bytes());
          hash
        })
        .collect();
      let mut expected_body = vec![0; 32];
      expected_body[..16].fill(1);
      word(&mut expected_body, 16, if internal { 2 } else { 1 });
      expected_body[20..24].copy_from_slice(&(count as u32).to_le_bytes());
      if internal {
        expected_body.extend_from_slice(&hashes[0]);
      }
      for (index, path) in paths.iter().enumerate() {
        expected_body.extend_from_slice(&(path.len() as u32).to_le_bytes());
        expected_body.extend_from_slice(path.as_bytes());
        expected_body.extend_from_slice(&hashes[index + usize::from(internal)]);
      }
      let payload_length = (expected_body.len() - 32) as u32;
      expected_body[24..28].copy_from_slice(&payload_length.to_le_bytes());
      let actual = if internal {
        let mut children = vec![SemanticSourceChildV1 { separator: None, node_id: &hashes[0] }];
        for (index, path) in paths.iter().enumerate() {
          children.push(SemanticSourceChildV1 { separator: Some(path), node_id: &hashes[index + 1] });
        }
        encode_semantic_source_internal_v1(&[1; 16], &children, algorithm).unwrap()
      } else {
        let rows: Vec<_> =
          paths.iter().enumerate().map(|(index, path)| SemanticSourceLeafEntryV1 { path, file_record_id: Some(&hashes[index]) }).collect();
        encode_semantic_source_leaf_v1(&[1; 16], &rows, algorithm).unwrap()
      };
      assert_eq!(actual, envelope(b"ASCN", 1, &expected_body));
      let mut preimage = b"aeordb.semantic-source-node.v1\0".to_vec();
      preimage.extend_from_slice(&expected_body);
      assert_eq!(decode_system_control(&actual, algorithm).unwrap().identity, independent_digest(algorithm, &preimage));
    }
  }
}

#[test]
fn source_capture_node_writers_accept_exact_body_cap_and_refuse_the_next_byte() {
  for algorithm in ALGORITHMS {
    let width = algorithm.hash_length();
    for internal in [false, true] {
      let count = 16;
      let last_length = (1 << 20) - 32 - usize::from(internal) * width - count * (4 + width) - 15 * 65_535;
      let mut paths: Vec<_> = (0..count).map(|index| format!("/{index:03}/{}", "x".repeat(65_530))).collect();
      paths[15].truncate(last_length);
      let hashes: Vec<_> = (1..=17).map(|index| vec![index; width]).collect();
      for extra in [false, true] {
        if extra {
          paths[15].push('x');
        }
        let rows: Vec<_> =
          paths.iter().enumerate().map(|(index, path)| SemanticSourceLeafEntryV1 { path, file_record_id: Some(&hashes[index]) }).collect();
        let mut children = vec![SemanticSourceChildV1 { separator: None, node_id: &hashes[0] }];
        for (index, path) in paths.iter().enumerate() {
          children.push(SemanticSourceChildV1 { separator: Some(path), node_id: &hashes[index + 1] });
        }
        let result = if internal {
          encode_semantic_source_internal_v1(&[1; 16], &children, algorithm)
        } else {
          encode_semantic_source_leaf_v1(&[1; 16], &rows, algorithm)
        };
        if extra {
          assert_eq!(result.unwrap_err().code(), "semantic_source_node_cap");
          continue;
        }
        let actual = result.unwrap();
        let mut body = vec![0; 32];
        body[..16].fill(1);
        word(&mut body, 16, if internal { 2 } else { 1 });
        body[20..24].copy_from_slice(&16u32.to_le_bytes());
        body[24..28].copy_from_slice(&((1u32 << 20) - 32).to_le_bytes());
        if internal {
          body.extend_from_slice(&hashes[0]);
        }
        for (index, path) in paths.iter().enumerate() {
          body.extend_from_slice(&(path.len() as u32).to_le_bytes());
          body.extend_from_slice(path.as_bytes());
          body.extend_from_slice(&hashes[index + usize::from(internal)]);
        }
        assert_eq!(body.len(), 1 << 20);
        assert_eq!(actual, envelope(b"ASCN", 1, &body));
      }
    }
  }
}

#[test]
fn source_capture_internal_writer_validates_late_edges_and_database_widths() {
  for algorithm in ALGORITHMS {
    let hashes: Vec<_> = (1..=3).map(|index| vec![index; algorithm.hash_length()]).collect();
    let long = vec![4; algorithm.hash_length() + 1];
    let oversized = format!("/{}", "x".repeat(65_535));
    let valid = [
      SemanticSourceChildV1 { separator: None, node_id: &hashes[0] },
      SemanticSourceChildV1 { separator: Some("/a"), node_id: &hashes[1] },
      SemanticSourceChildV1 { separator: Some("/b"), node_id: &hashes[2] },
    ];
    for database in [vec![0; 16], vec![1; 15], vec![1; 17]] {
      assert!(encode_semantic_source_internal_v1(&database, &valid, algorithm).is_err());
      assert!(
        encode_semantic_source_leaf_v1(&database, &[SemanticSourceLeafEntryV1 { path: "/", file_record_id: None }], algorithm).is_err()
      );
    }
    for case in 0..7 {
      let mut bad = valid;
      match case {
        0 => bad[2].separator = Some("/a"),
        1 => bad[2].separator = Some("/0"),
        2 => bad[2].separator = Some(&oversized),
        3 => bad[2].node_id = &hashes[0],
        4 => bad[2].node_id = &hashes[1],
        5 => bad[2].node_id = &long,
        6 => bad[0].node_id = &long,
        _ => unreachable!(),
      }
      assert!(encode_semantic_source_internal_v1(&[1; 16], &bad, algorithm).is_err(), "case{case}");
    }
    assert!(encode_semantic_source_internal_v1(&[1; 16], &valid, algorithm).is_ok());
  }
}
