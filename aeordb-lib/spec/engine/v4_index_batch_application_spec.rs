use std::collections::BTreeMap;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::index_artifact::EncodedImmutableIndexArtifactV1;
use aeordb::engine::v4::index_batch_application::{
  INDEX_BATCH_PATH_MAXIMUM_INPUT_BYTES_V1, IndexBatchApplicationErrorV1, IndexBatchArtifactOverlayLimitsV1, IndexBatchArtifactReadErrorV1,
  IndexBatchArtifactSourceV1, OrderedPagePathLookupLimitsV1, OrderedPagePathLookupRequestV1, SparseIndexArtifactOverlayV1,
  load_ordered_page_path_v1,
};
use aeordb::engine::v4::index_page::{
  ArtifactDirectoryEntryWriteV1, ArtifactDirectoryWriteV1, OrderedIndexRoleV1, OrderedPageWriteV1, PhysicalHintV1, PostingRecordV1,
  decode_artifact_directory, decode_ordered_page, decode_ordered_record, encode_artifact_directory, encode_ordered_page,
  encode_posting_record, ordered_record_order_key,
};

fn owner(hash_algorithm: HashAlgorithm) -> Vec<u8> {
  vec![0x71; hash_algorithm.hash_length()]
}

fn posting_record(coordinate: u64) -> Vec<u8> {
  encode_posting_record(&PostingRecordV1 {
    tombstone: false,
    coordinate,
    document_ordinal: coordinate,
    source_value_ordinal: 0,
    expansion_ordinal: 0,
    posting_key: &coordinate.to_le_bytes(),
  })
  .unwrap()
}

fn posting_order_key(hash_algorithm: HashAlgorithm, coordinate: u64) -> Vec<u8> {
  let record = posting_record(coordinate);
  ordered_record_order_key(&decode_ordered_record(&record, hash_algorithm, OrderedIndexRoleV1::Posting).unwrap()).unwrap()
}

fn posting_page(
  hash_algorithm: HashAlgorithm,
  owner_id: &[u8],
  coordinate: u64,
  page_id: u64,
  previous: u64,
  next: u64,
) -> EncodedImmutableIndexArtifactV1 {
  let record = posting_record(coordinate);
  encode_ordered_page(&OrderedPageWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id,
    generation: 7,
    page_id,
    previous_page_id: previous,
    next_page_id: next,
    records: &[&record],
  })
  .unwrap()
}

fn leaf_directory(
  hash_algorithm: HashAlgorithm,
  owner_id: &[u8],
  pages: &[&EncodedImmutableIndexArtifactV1],
) -> EncodedImmutableIndexArtifactV1 {
  let pages = pages.iter().map(|artifact| decode_ordered_page(&artifact.value, hash_algorithm).unwrap()).collect::<Vec<_>>();
  let entries = pages
    .iter()
    .map(|page| ArtifactDirectoryEntryWriteV1 {
      lower_fence: page.lower_fence,
      upper_fence: page.upper_fence,
      child_hash: &page.key,
      child_generation: page.generation,
      live_count: u64::from(page.live_count),
      tombstone_count: u64::from(page.tombstone_count),
      page_count: 1,
      logical_bytes: page.logical_live_bytes,
      minimum_page_id: page.page_id,
      maximum_page_id: page.page_id,
      physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    })
    .collect::<Vec<_>>();
  encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id,
    generation: 7,
    level: 0,
    entries: &entries,
  })
  .unwrap()
}

fn parent_directory(
  hash_algorithm: HashAlgorithm,
  owner_id: &[u8],
  child: &EncodedImmutableIndexArtifactV1,
  live_count_delta: u64,
) -> EncodedImmutableIndexArtifactV1 {
  internal_directory(hash_algorithm, owner_id, &[(child, live_count_delta)])
}

fn internal_directory(
  hash_algorithm: HashAlgorithm,
  owner_id: &[u8],
  children: &[(&EncodedImmutableIndexArtifactV1, u64)],
) -> EncodedImmutableIndexArtifactV1 {
  let children = children
    .iter()
    .map(|(artifact, live_count_delta)| (decode_artifact_directory(&artifact.value, hash_algorithm).unwrap(), *live_count_delta))
    .collect::<Vec<_>>();
  let entries = children
    .iter()
    .map(|(child, live_count_delta)| ArtifactDirectoryEntryWriteV1 {
      lower_fence: child.lower_fence,
      upper_fence: child.upper_fence,
      child_hash: &child.key,
      child_generation: child.generation,
      live_count: child.live_count + live_count_delta,
      tombstone_count: child.tombstone_count,
      page_count: child.page_count,
      logical_bytes: child.logical_bytes,
      minimum_page_id: child.minimum_page_id,
      maximum_page_id: child.maximum_page_id,
      physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    })
    .collect::<Vec<_>>();
  encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id,
    generation: 8,
    level: children[0].0.level + 1,
    entries: &entries,
  })
  .unwrap()
}

#[derive(Default)]
struct CountingSource {
  values: BTreeMap<Vec<u8>, Vec<u8>>,
  reads: Vec<Vec<u8>>,
  failure: Option<IndexBatchArtifactReadErrorV1>,
}

impl CountingSource {
  fn insert(&mut self, artifact: &EncodedImmutableIndexArtifactV1) {
    self.values.insert(artifact.key.clone(), artifact.value.clone());
  }
}

impl IndexBatchArtifactSourceV1 for CountingSource {
  fn read_immutable_artifact(&mut self, key: &[u8], maximum_bytes: usize) -> Result<Vec<u8>, IndexBatchArtifactReadErrorV1> {
    self.reads.push(key.to_vec());
    if let Some(error) = self.failure.take() {
      return Err(error);
    }
    let value = self.values.get(key).ok_or(IndexBatchArtifactReadErrorV1::Missing)?;
    if value.len() > maximum_bytes {
      return Err(IndexBatchArtifactReadErrorV1::ResourcePressure("source value exceeds the supplied read bound".to_string()));
    }
    Ok(value.clone())
  }
}

#[test]
fn sparse_lookup_loads_only_the_selected_path_and_required_posting_successor() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let owner_id = owner(hash_algorithm);
    let first = posting_page(hash_algorithm, &owner_id, 10, 1, 0, 2);
    let second = posting_page(hash_algorithm, &owner_id, 20, 2, 1, 3);
    let third = posting_page(hash_algorithm, &owner_id, 30, 3, 2, 0);
    let root = leaf_directory(hash_algorithm, &owner_id, &[&first, &second, &third]);
    let mut source = CountingSource::default();
    for artifact in [&root, &first, &second, &third] {
      source.insert(artifact);
    }
    let overlay = SparseIndexArtifactOverlayV1::new(hash_algorithm, IndexBatchArtifactOverlayLimitsV1::default()).unwrap();

    let loaded = load_ordered_page_path_v1(
      &OrderedPagePathLookupRequestV1 {
        hash_algorithm,
        root_key: &root.key,
        owner_id: &owner_id,
        role: OrderedIndexRoleV1::Posting,
        order_key: &posting_order_key(hash_algorithm, 10),
        load_posting_successor: true,
        limits: OrderedPagePathLookupLimitsV1::default(),
      },
      &overlay,
      &mut source,
      &|| false,
    )
    .unwrap();

    assert_eq!(loaded.directory_count(), 1);
    assert_eq!(decode_ordered_page(loaded.page(), hash_algorithm).unwrap().page_id, 1);
    assert_eq!(decode_ordered_page(loaded.next_posting_page().unwrap(), hash_algorithm).unwrap().page_id, 2);
    assert_eq!(loaded.next_directory_count(), 1);
    assert_eq!(source.reads, vec![root.key.clone(), first.key.clone(), second.key.clone()]);
    assert!(!source.reads.contains(&third.key));

    source.reads.clear();
    let without_successor = load_ordered_page_path_v1(
      &OrderedPagePathLookupRequestV1 {
        hash_algorithm,
        root_key: &root.key,
        owner_id: &owner_id,
        role: OrderedIndexRoleV1::Posting,
        order_key: &posting_order_key(hash_algorithm, 10),
        load_posting_successor: false,
        limits: OrderedPagePathLookupLimitsV1::default(),
      },
      &overlay,
      &mut source,
      &|| false,
    )
    .unwrap();
    assert!(without_successor.next_posting_page().is_none());
    assert_eq!(source.reads, vec![root.key.clone(), first.key.clone()]);
  }
}

#[test]
fn sparse_lookup_reads_successor_artifacts_from_the_overlay_without_source_io() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let first = posting_page(hash_algorithm, &owner_id, 10, 1, 0, 2);
  let second = posting_page(hash_algorithm, &owner_id, 20, 2, 1, 0);
  let root = leaf_directory(hash_algorithm, &owner_id, &[&first, &second]);
  let mut overlay = SparseIndexArtifactOverlayV1::new(hash_algorithm, IndexBatchArtifactOverlayLimitsV1::default()).unwrap();
  for artifact in [first, second, root.clone()] {
    assert!(overlay.insert(artifact).unwrap());
  }
  let mut source = CountingSource::default();

  let loaded = load_ordered_page_path_v1(
    &OrderedPagePathLookupRequestV1 {
      hash_algorithm,
      root_key: &root.key,
      owner_id: &owner_id,
      role: OrderedIndexRoleV1::Posting,
      order_key: &posting_order_key(hash_algorithm, 10),
      load_posting_successor: true,
      limits: OrderedPagePathLookupLimitsV1::default(),
    },
    &overlay,
    &mut source,
    &|| false,
  )
  .unwrap();

  assert_eq!(decode_ordered_page(loaded.page(), hash_algorithm).unwrap().page_id, 1);
  assert_eq!(decode_ordered_page(loaded.next_posting_page().unwrap(), hash_algorithm).unwrap().page_id, 2);
  assert!(source.reads.is_empty());
  assert_eq!(overlay.artifact_count(), 3);
  assert_eq!(overlay.prepared_artifacts().count(), 3);
}

#[test]
fn sparse_lookup_finds_a_posting_successor_across_internal_directory_branches() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let first = posting_page(hash_algorithm, &owner_id, 10, 1, 0, 2);
  let second = posting_page(hash_algorithm, &owner_id, 20, 2, 1, 0);
  let left = leaf_directory(hash_algorithm, &owner_id, &[&first]);
  let right = leaf_directory(hash_algorithm, &owner_id, &[&second]);
  let root = internal_directory(hash_algorithm, &owner_id, &[(&left, 0), (&right, 0)]);
  let mut source = CountingSource::default();
  for artifact in [&root, &left, &right, &first, &second] {
    source.insert(artifact);
  }
  let overlay = SparseIndexArtifactOverlayV1::new(hash_algorithm, IndexBatchArtifactOverlayLimitsV1::default()).unwrap();

  let loaded = load_ordered_page_path_v1(
    &OrderedPagePathLookupRequestV1 {
      hash_algorithm,
      root_key: &root.key,
      owner_id: &owner_id,
      role: OrderedIndexRoleV1::Posting,
      order_key: &posting_order_key(hash_algorithm, 10),
      load_posting_successor: true,
      limits: OrderedPagePathLookupLimitsV1::default(),
    },
    &overlay,
    &mut source,
    &|| false,
  )
  .unwrap();

  assert_eq!(loaded.directory_count(), 2);
  assert_eq!(loaded.next_directory_count(), 2);
  assert_eq!(decode_ordered_page(loaded.next_posting_page().unwrap(), hash_algorithm).unwrap().page_id, 2);
  assert_eq!(source.reads, vec![root.key.clone(), left.key.clone(), first.key.clone(), right.key.clone(), second.key.clone()]);
}

#[test]
fn sparse_lookup_and_overlay_fail_closed_on_missing_pressure_corruption_cancellation_and_caps() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let page = posting_page(hash_algorithm, &owner_id, 10, 1, 0, 0);
  let root = leaf_directory(hash_algorithm, &owner_id, &[&page]);
  let request = OrderedPagePathLookupRequestV1 {
    hash_algorithm,
    root_key: &root.key,
    owner_id: &owner_id,
    role: OrderedIndexRoleV1::Posting,
    order_key: &posting_order_key(hash_algorithm, 10),
    load_posting_successor: true,
    limits: OrderedPagePathLookupLimitsV1::default(),
  };
  let overlay = SparseIndexArtifactOverlayV1::new(hash_algorithm, IndexBatchArtifactOverlayLimitsV1::default()).unwrap();

  let missing = load_ordered_page_path_v1(&request, &overlay, &mut CountingSource::default(), &|| false).unwrap_err();
  assert_eq!(missing.code(), "index_batch_artifact_missing");

  let mut pressure =
    CountingSource { failure: Some(IndexBatchArtifactReadErrorV1::ResourcePressure("budget".to_string())), ..Default::default() };
  assert_eq!(load_ordered_page_path_v1(&request, &overlay, &mut pressure, &|| false).unwrap_err().code(), "index_batch_source_pressure");

  let mut corrupt = CountingSource::default();
  corrupt.values.insert(root.key.clone(), page.value.clone());
  assert!(matches!(
    load_ordered_page_path_v1(&request, &overlay, &mut corrupt, &|| false).unwrap_err(),
    IndexBatchApplicationErrorV1::Malformed(_)
  ));

  let mismatched_parent = parent_directory(hash_algorithm, &owner_id, &root, 1);
  let mut mismatched_source = CountingSource::default();
  for artifact in [&mismatched_parent, &root, &page] {
    mismatched_source.insert(artifact);
  }
  let mismatched_request = OrderedPagePathLookupRequestV1 { root_key: &mismatched_parent.key, ..request };
  assert_eq!(
    load_ordered_page_path_v1(&mismatched_request, &overlay, &mut mismatched_source, &|| false).unwrap_err().code(),
    "index_batch_path_closure"
  );
  assert_eq!(
    load_ordered_page_path_v1(&request, &overlay, &mut CountingSource::default(), &|| true).unwrap_err().code(),
    "index_batch_cancelled"
  );

  let limits = IndexBatchArtifactOverlayLimitsV1::new(1, 4 * 1_024 * 1_024).unwrap();
  let mut bounded = SparseIndexArtifactOverlayV1::new(hash_algorithm, limits).unwrap();
  assert!(bounded.insert(page).unwrap());
  assert_eq!(bounded.insert(root).unwrap_err().code(), "index_batch_overlay_count");
  assert_eq!(bounded.artifact_count(), 1);
}

#[test]
fn sparse_lookup_rejects_source_classes_link_corruption_depth_and_byte_limits() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let terminal = posting_page(hash_algorithm, &owner_id, 10, 1, 0, 0);
  let unexpected = posting_page(hash_algorithm, &owner_id, 20, 2, 1, 0);
  let terminal_root = leaf_directory(hash_algorithm, &owner_id, &[&terminal, &unexpected]);
  let order_key = posting_order_key(hash_algorithm, 10);
  let request = OrderedPagePathLookupRequestV1 {
    hash_algorithm,
    root_key: &terminal_root.key,
    owner_id: &owner_id,
    role: OrderedIndexRoleV1::Posting,
    order_key: &order_key,
    load_posting_successor: true,
    limits: OrderedPagePathLookupLimitsV1::default(),
  };
  let overlay = SparseIndexArtifactOverlayV1::new(hash_algorithm, IndexBatchArtifactOverlayLimitsV1::default()).unwrap();

  for (failure, code) in [
    (IndexBatchArtifactReadErrorV1::Cancelled, "index_batch_cancelled"),
    (IndexBatchArtifactReadErrorV1::Operational("disk".to_string()), "index_batch_source_operational"),
  ] {
    let mut source = CountingSource { failure: Some(failure), ..Default::default() };
    assert_eq!(load_ordered_page_path_v1(&request, &overlay, &mut source, &|| false).unwrap_err().code(), code);
  }

  let mut terminal_source = CountingSource::default();
  for artifact in [&terminal_root, &terminal, &unexpected] {
    terminal_source.insert(artifact);
  }
  assert_eq!(
    load_ordered_page_path_v1(&request, &overlay, &mut terminal_source, &|| false).unwrap_err().code(),
    "index_batch_path_closure"
  );

  let linked = posting_page(hash_algorithm, &owner_id, 10, 1, 0, 2);
  let linked_leaf = leaf_directory(hash_algorithm, &owner_id, &[&linked]);
  let linked_root = parent_directory(hash_algorithm, &owner_id, &linked_leaf, 0);
  let mut linked_source = CountingSource::default();
  for artifact in [&linked_root, &linked_leaf, &linked] {
    linked_source.insert(artifact);
  }
  let shallow = OrderedPagePathLookupRequestV1 {
    root_key: &linked_root.key,
    limits: OrderedPagePathLookupLimitsV1::new(1, INDEX_BATCH_PATH_MAXIMUM_INPUT_BYTES_V1).unwrap(),
    ..request
  };
  assert_eq!(load_ordered_page_path_v1(&shallow, &overlay, &mut linked_source, &|| false).unwrap_err().code(), "index_batch_path_depth");

  let tiny_input = OrderedPagePathLookupRequestV1 { limits: OrderedPagePathLookupLimitsV1::new(16, 1).unwrap(), ..request };
  let mut tiny_source = CountingSource::default();
  tiny_source.insert(&terminal_root);
  assert_eq!(
    load_ordered_page_path_v1(&tiny_input, &overlay, &mut tiny_source, &|| false).unwrap_err().code(),
    "index_batch_source_pressure"
  );
}

#[test]
fn sparse_overlay_validates_limits_identity_idempotence_and_retained_bytes() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let page = posting_page(hash_algorithm, &owner_id, 10, 1, 0, 0);
  assert_eq!(IndexBatchArtifactOverlayLimitsV1::new(0, 1).unwrap_err().code(), "index_batch_invalid_limits");
  assert_eq!(OrderedPagePathLookupLimitsV1::new(0, 1).unwrap_err().code(), "index_batch_invalid_limits");

  let too_small = IndexBatchArtifactOverlayLimitsV1::new(1, page.value.len()).unwrap();
  let mut bounded = SparseIndexArtifactOverlayV1::new(hash_algorithm, too_small).unwrap();
  assert_eq!(bounded.insert(page.clone()).unwrap_err().code(), "index_batch_overlay_bytes");
  assert_eq!(bounded.artifact_count(), 0);

  let mut overlay = SparseIndexArtifactOverlayV1::new(hash_algorithm, IndexBatchArtifactOverlayLimitsV1::default()).unwrap();
  assert!(overlay.insert(page.clone()).unwrap());
  assert!(!overlay.insert(page.clone()).unwrap());
  assert_eq!(overlay.artifact_count(), 1);

  let mut forged = page;
  forged.key[0] ^= 0xff;
  assert!(matches!(overlay.insert(forged).unwrap_err(), IndexBatchApplicationErrorV1::Malformed(_)));
}
