//! Independently assembled internal bytes, maximum paths and streaming bounds.
use super::*;

fn internal(left: &[u8], right: &[u8], separator: &str) -> Vec<u8> {
  let mut body = vec![0; 32];
  body[..16].fill(1);
  body[16..18].copy_from_slice(&2u16.to_le_bytes());
  body[20..24].copy_from_slice(&1u32.to_le_bytes());
  body.extend_from_slice(left);
  body.extend_from_slice(&(separator.len() as u32).to_le_bytes());
  body.extend_from_slice(separator.as_bytes());
  body.extend_from_slice(right);
  let length = (body.len() - 32) as u32;
  body[24..28].copy_from_slice(&length.to_le_bytes());
  envelope(b"ASCN", 1, &body)
}

#[test]
fn catalog_build_internal_bytes_match_independent_odd_forest_for_all_hashes() {
  for algorithm in ALGORITHMS {
    let rows: Vec<_> = (0..513).map(|index| row(index, algorithm)).collect();
    let mut expected = Vec::new();
    for requested in [false, true] {
      let first = leaf(algorithm, &rows[..256], requested);
      let second = leaf(algorithm, &rows[256..512], requested);
      let parent = internal(&identity(&first, algorithm), &identity(&second, algorithm), &rows[256].path);
      let third = leaf(algorithm, &rows[512..], requested);
      let root = internal(&identity(&parent, algorithm), &identity(&third, algorithm), &rows[512].path);
      expected.push(vec![first, second, parent, third, root]);
    }
    let mut position = 0;
    let result = build_semantic_source_catalog_pair_v1(
      request(algorithm, 513),
      rows.into_iter().map(Ok),
      &mut |base, requested| {
        assert_eq!(base, expected[0][position]);
        assert_eq!(requested, expected[1][position]);
        position += 1;
        Ok(())
      },
      &memory(),
      &|| false,
    )
    .unwrap();
    assert_eq!(position, 5);
    assert_eq!(result.base_root(), identity(&expected[0][4], algorithm));
    assert_eq!(result.requested_root(), identity(&expected[1][4], algorithm));
  }
}

#[test]
fn catalog_build_long_paths_split_at_body_bytes_not_just_item_count() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let rows: Vec<_> = (0..33)
      .map(|index| {
        let mut row = row(index, algorithm);
        let mut path = String::with_capacity(65_535);
        path.push_str(&row.path);
        path.extend(std::iter::repeat_n('x', 65_535 - path.len()));
        row.path = path;
        row
      })
      .collect();
    let mut base_nodes = BTreeMap::new();
    let mut leaf_sizes = Vec::new();
    let memory = memory();
    let result = build_semantic_source_catalog_pair_v1(
      request(algorithm, 33),
      rows.clone().into_iter().map(Ok),
      &mut |base, requested| {
        for bytes in [base, requested] {
          assert!(decode_system_control(bytes, algorithm).unwrap().body.len() <= 1 << 20);
        }
        let node = decode_semantic_source_node_v1(base, algorithm).unwrap();
        if let Some(entries) = node.leaf_entries() {
          leaf_sizes.push(entries.count());
        }
        base_nodes.insert(identity(base, algorithm), base.to_vec());
        Ok(())
      },
      &memory,
      &|| false,
    )
    .unwrap();
    assert_eq!(leaf_sizes, [15, 15, 3]);
    assert_eq!(result.node_count(), 5);
    let mut actual = Vec::new();
    collect_rows(result.base_root(), &base_nodes, algorithm, &mut actual, 0);
    assert_eq!(actual, rows.into_iter().map(|row| (row.path, row.base_file_record_id)).collect::<Vec<_>>());
    drop(result);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn catalog_build_bytes_do_not_depend_on_admitted_path_spare_capacity() {
  let algorithm = HashAlgorithm::Blake3_256;
  let mut first = None;
  for extra in [0, 64] {
    let rows = (0..1025).map(|index| {
      let mut row = row(index, algorithm);
      let mut path = String::with_capacity(row.path.len() + extra);
      path.push_str(&row.path);
      row.path = path;
      Ok(row)
    });
    let result = build_semantic_source_catalog_pair_v1(request(algorithm, 1025), rows, &mut |_, _| Ok(()), &memory(), &|| false).unwrap();
    let actual = (result.base_root().to_vec(), result.requested_root().to_vec(), result.node_count());
    match &first {
      Some(expected) => assert_eq!(&actual, expected),
      None => first = Some(actual),
    }
  }
}

#[test]
fn catalog_build_streams_ten_thousand_rows_with_bounded_memory_and_exact_emission_count() {
  let algorithm = HashAlgorithm::Blake3_256;
  let memory = memory();
  let mut input = request(algorithm, 10_000);
  input.maximum_path_bytes = 16;
  input.maximum_workspace_bytes = 3 << 20;
  let mut emissions = 0;
  let mut peak_reserved = 0;
  let start = std::time::Instant::now();
  let result = build_semantic_source_catalog_pair_v1(
    input,
    (0..10_000).map(|index| Ok(row(index, algorithm))),
    &mut |_, _| {
      emissions += 1;
      peak_reserved = peak_reserved.max(memory.snapshot().unwrap().reserved_bytes);
      Ok(())
    },
    &memory,
    &|| false,
  )
  .unwrap();
  assert_eq!(result.path_count(), 10_000);
  assert_eq!(result.node_count(), 79);
  assert_eq!(emissions, 79);
  assert!(peak_reserved <= 3 << 20);
  assert!(start.elapsed() < std::time::Duration::from_secs(10));
  drop(result);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}
