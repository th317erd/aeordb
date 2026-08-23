use std::collections::BTreeMap;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::index_artifact::EncodedImmutableIndexArtifactV1;
use aeordb::engine::v4::index_artifact_cursor::{
  ArtifactCursorReadErrorV1, ArtifactCursorSourceV1, ArtifactDirectoryRootSummaryV1, ArtifactPageCursorErrorV1, ArtifactPageCursorLimitsV1,
  ArtifactPageCursorRequestV1, ArtifactPageCursorRootV1, ArtifactPageNeighborModeV1, ArtifactPageSeekV1, RetainedArtifactBytesV1,
  load_artifact_page_cursor_v1,
};
use aeordb::engine::v4::index_page::{
  ArtifactDirectoryEntryWriteV1, ArtifactDirectoryWriteV1, OrderedIndexRoleV1, OrderedPageWriteV1, PhysicalHintV1, PostingRecordV1,
  decode_artifact_directory, decode_ordered_page, decode_ordered_record, encode_artifact_directory, encode_ordered_page,
  encode_posting_record, ordered_record_order_key,
};

fn owner(hash_algorithm: HashAlgorithm) -> Vec<u8> {
  vec![0x71; hash_algorithm.hash_length()]
}

fn posting_page(
  hash_algorithm: HashAlgorithm,
  owner_id: &[u8],
  coordinates: &[u64],
  page_id: u64,
  previous_page_id: u64,
  next_page_id: u64,
) -> EncodedImmutableIndexArtifactV1 {
  posting_page_with_states(
    hash_algorithm,
    owner_id,
    &coordinates.iter().map(|coordinate| (*coordinate, false)).collect::<Vec<_>>(),
    page_id,
    previous_page_id,
    next_page_id,
  )
}

fn posting_page_with_states(
  hash_algorithm: HashAlgorithm,
  owner_id: &[u8],
  coordinates: &[(u64, bool)],
  page_id: u64,
  previous_page_id: u64,
  next_page_id: u64,
) -> EncodedImmutableIndexArtifactV1 {
  let records = coordinates
    .iter()
    .map(|(coordinate, tombstone)| {
      encode_posting_record(&PostingRecordV1 {
        tombstone: *tombstone,
        coordinate: *coordinate,
        document_ordinal: *coordinate,
        source_value_ordinal: 0,
        expansion_ordinal: 0,
        posting_key: &coordinate.to_le_bytes(),
      })
      .unwrap()
    })
    .collect::<Vec<_>>();
  encode_ordered_page(&OrderedPageWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id,
    generation: 7,
    page_id,
    previous_page_id,
    next_page_id,
    records: &records.iter().map(Vec::as_slice).collect::<Vec<_>>(),
  })
  .unwrap()
}

fn posting_order_key(hash_algorithm: HashAlgorithm, coordinate: u64) -> Vec<u8> {
  let record = encode_posting_record(&PostingRecordV1 {
    tombstone: false,
    coordinate,
    document_ordinal: coordinate,
    source_value_ordinal: 0,
    expansion_ordinal: 0,
    posting_key: &coordinate.to_le_bytes(),
  })
  .unwrap();
  ordered_record_order_key(&decode_ordered_record(&record, hash_algorithm, OrderedIndexRoleV1::Posting).unwrap()).unwrap()
}

fn leaf_directory(
  hash_algorithm: HashAlgorithm,
  owner_id: &[u8],
  pages: &[&EncodedImmutableIndexArtifactV1],
) -> EncodedImmutableIndexArtifactV1 {
  let decoded = pages.iter().map(|page| decode_ordered_page(&page.value, hash_algorithm).unwrap()).collect::<Vec<_>>();
  let entries = decoded
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

fn internal_directory(
  hash_algorithm: HashAlgorithm,
  owner_id: &[u8],
  children: &[&EncodedImmutableIndexArtifactV1],
) -> EncodedImmutableIndexArtifactV1 {
  let decoded = children.iter().map(|directory| decode_artifact_directory(&directory.value, hash_algorithm).unwrap()).collect::<Vec<_>>();
  let entries = decoded
    .iter()
    .map(|directory| ArtifactDirectoryEntryWriteV1 {
      lower_fence: directory.lower_fence,
      upper_fence: directory.upper_fence,
      child_hash: &directory.key,
      child_generation: directory.generation,
      live_count: directory.live_count,
      tombstone_count: directory.tombstone_count,
      page_count: directory.page_count,
      logical_bytes: directory.logical_bytes,
      minimum_page_id: directory.minimum_page_id,
      maximum_page_id: directory.maximum_page_id,
      physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    })
    .collect::<Vec<_>>();
  encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id,
    generation: 8,
    level: decoded[0].level + 1,
    entries: &entries,
  })
  .unwrap()
}

#[derive(Default)]
struct Source {
  values: BTreeMap<Vec<u8>, Vec<u8>>,
  reads: Vec<Vec<u8>>,
  failure: Option<ArtifactCursorReadErrorV1>,
}

impl Source {
  fn insert(&mut self, artifact: &EncodedImmutableIndexArtifactV1) {
    self.values.insert(artifact.key.clone(), artifact.value.clone());
  }
}

impl ArtifactCursorSourceV1 for Source {
  fn read_immutable_artifact(&mut self, key: &[u8], maximum_bytes: usize) -> Result<RetainedArtifactBytesV1, ArtifactCursorReadErrorV1> {
    self.reads.push(key.to_vec());
    if let Some(error) = self.failure.take() {
      return Err(error);
    }
    let value = self.values.get(key).ok_or(ArtifactCursorReadErrorV1::Missing)?;
    if value.len() > maximum_bytes {
      return Err(ArtifactCursorReadErrorV1::ResourcePressure("artifact exceeds the supplied read ceiling".to_owned()));
    }
    Ok(RetainedArtifactBytesV1::from_bytes(value.clone()))
  }
}

#[test]
fn cursor_seeks_exact_page_and_live_ranks_across_directory_branches() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let owner_id = owner(hash_algorithm);
    let first = posting_page(hash_algorithm, &owner_id, &[10, 11], 1, 0, 2);
    let second = posting_page(hash_algorithm, &owner_id, &[20], 2, 1, 3);
    let third = posting_page(hash_algorithm, &owner_id, &[30, 31, 32], 3, 2, 0);
    let left = leaf_directory(hash_algorithm, &owner_id, &[&first]);
    let right = leaf_directory(hash_algorithm, &owner_id, &[&second, &third]);
    let root = internal_directory(hash_algorithm, &owner_id, &[&left, &right]);
    let decoded_root = decode_artifact_directory(&root.value, hash_algorithm).unwrap();
    let mut source = Source::default();
    for artifact in [&root, &left, &right, &first, &second, &third] {
      source.insert(artifact);
    }
    let root_authority = ArtifactPageCursorRootV1 {
      hash_algorithm,
      root_key: &root.key,
      owner_id: &owner_id,
      role: OrderedIndexRoleV1::Posting,
      maximum_generation: 8,
      expected_summary: Some(ArtifactDirectoryRootSummaryV1::from_directory(&decoded_root)),
    };
    let request = ArtifactPageCursorRequestV1 {
      root: root_authority,
      seek: ArtifactPageSeekV1::LiveRecordRank(2),
      neighbors: ArtifactPageNeighborModeV1::Both,
      limits: ArtifactPageCursorLimitsV1::default(),
    };
    let loaded = load_artifact_page_cursor_v1(&request, &mut source, &|| false).unwrap().unwrap();

    assert_eq!(loaded.page_ordinal(), 1);
    assert_eq!(loaded.live_rank_before_page(), 2);
    assert_eq!(loaded.live_rank_within_page(), Some(0));
    assert_eq!(loaded.record_index_within_page(), Some(0));
    assert_eq!(decode_ordered_page(loaded.page(), hash_algorithm).unwrap().page_id, 2);
    assert_eq!(decode_ordered_page(loaded.previous_page().unwrap(), hash_algorithm).unwrap().page_id, 1);
    assert_eq!(decode_ordered_page(loaded.next_page().unwrap(), hash_algorithm).unwrap().page_id, 3);
    assert_eq!(loaded.directory_count(), 2);
    assert_eq!(loaded.previous_directory_count(), 2);
    assert_eq!(loaded.next_directory_count(), 2);
    assert!(loaded.retained_input_bytes() <= request.limits.maximum_input_bytes());
  }
}

#[test]
fn cursor_resolves_order_keys_and_overlapping_page_id_ranges_without_trusting_range_order() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let owner_id = owner(hash_algorithm);
    let first = posting_page(hash_algorithm, &owner_id, &[10], 1, 0, 100);
    let second = posting_page(hash_algorithm, &owner_id, &[20], 100, 1, 50);
    let third = posting_page(hash_algorithm, &owner_id, &[30], 50, 100, 0);
    let left = leaf_directory(hash_algorithm, &owner_id, &[&first, &second]);
    let right = leaf_directory(hash_algorithm, &owner_id, &[&third]);
    let root = internal_directory(hash_algorithm, &owner_id, &[&left, &right]);
    let decoded_root = decode_artifact_directory(&root.value, hash_algorithm).unwrap();
    let root_authority = ArtifactPageCursorRootV1 {
      hash_algorithm,
      root_key: &root.key,
      owner_id: &owner_id,
      role: OrderedIndexRoleV1::Posting,
      maximum_generation: 8,
      expected_summary: Some(ArtifactDirectoryRootSummaryV1::from_directory(&decoded_root)),
    };
    let mut source = Source::default();
    for artifact in [&root, &left, &right, &first, &second, &third] {
      source.insert(artifact);
    }

    let third_key = decode_ordered_page(&third.value, hash_algorithm).unwrap().lower_fence.to_vec();
    let by_key = load_artifact_page_cursor_v1(
      &ArtifactPageCursorRequestV1 {
        root: root_authority,
        seek: ArtifactPageSeekV1::OrderLowerBound(&third_key),
        neighbors: ArtifactPageNeighborModeV1::None,
        limits: ArtifactPageCursorLimitsV1::default(),
      },
      &mut source,
      &|| false,
    )
    .unwrap()
    .unwrap();
    assert_eq!(decode_ordered_page(by_key.page(), hash_algorithm).unwrap().page_id, 50);
    assert_eq!(by_key.page_ordinal(), 2);
    assert_eq!(by_key.live_rank_before_page(), 2);

    source.reads.clear();
    let by_page_id = load_artifact_page_cursor_v1(
      &ArtifactPageCursorRequestV1 {
        root: root_authority,
        seek: ArtifactPageSeekV1::PageId(50),
        neighbors: ArtifactPageNeighborModeV1::Both,
        limits: ArtifactPageCursorLimitsV1::default(),
      },
      &mut source,
      &|| false,
    )
    .unwrap()
    .unwrap();
    assert_eq!(decode_ordered_page(by_page_id.page(), hash_algorithm).unwrap().page_id, 50);
    assert_eq!(by_page_id.page_ordinal(), 2);
    assert_eq!(decode_ordered_page(by_page_id.previous_page().unwrap(), hash_algorithm).unwrap().page_id, 100);
    assert!(by_page_id.next_page().is_none());
    assert!(source.reads.contains(&left.key), "overlapping PageId ranges were not searched exactly");
    assert!(source.reads.contains(&right.key));

    let absent = load_artifact_page_cursor_v1(
      &ArtifactPageCursorRequestV1 {
        root: root_authority,
        seek: ArtifactPageSeekV1::PageId(75),
        neighbors: ArtifactPageNeighborModeV1::None,
        limits: ArtifactPageCursorLimitsV1::default(),
      },
      &mut source,
      &|| false,
    )
    .unwrap();
    assert!(absent.is_none());
  }
}

#[test]
fn cursor_distinguishes_lower_bound_and_predecessor_seeks_across_fence_gaps() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let owner_id = owner(hash_algorithm);
    let first = posting_page(hash_algorithm, &owner_id, &[10], 1, 0, 2);
    let second = posting_page(hash_algorithm, &owner_id, &[30], 2, 1, 0);
    let root = leaf_directory(hash_algorithm, &owner_id, &[&first, &second]);
    let decoded_root = decode_artifact_directory(&root.value, hash_algorithm).unwrap();
    let root_authority = ArtifactPageCursorRootV1 {
      hash_algorithm,
      root_key: &root.key,
      owner_id: &owner_id,
      role: OrderedIndexRoleV1::Posting,
      maximum_generation: 7,
      expected_summary: Some(ArtifactDirectoryRootSummaryV1::from_directory(&decoded_root)),
    };
    let mut source = Source::default();
    for artifact in [&root, &first, &second] {
      source.insert(artifact);
    }
    let gap_key = posting_order_key(hash_algorithm, 20);
    let lower = load_artifact_page_cursor_v1(
      &ArtifactPageCursorRequestV1 {
        root: root_authority,
        seek: ArtifactPageSeekV1::OrderLowerBound(&gap_key),
        neighbors: ArtifactPageNeighborModeV1::None,
        limits: ArtifactPageCursorLimitsV1::default(),
      },
      &mut source,
      &|| false,
    )
    .unwrap()
    .unwrap();
    assert_eq!(decode_ordered_page(lower.page(), hash_algorithm).unwrap().page_id, 2);
    let predecessor = load_artifact_page_cursor_v1(
      &ArtifactPageCursorRequestV1 {
        root: root_authority,
        seek: ArtifactPageSeekV1::OrderPredecessor(&gap_key),
        neighbors: ArtifactPageNeighborModeV1::None,
        limits: ArtifactPageCursorLimitsV1::default(),
      },
      &mut source,
      &|| false,
    )
    .unwrap()
    .unwrap();
    assert_eq!(decode_ordered_page(predecessor.page(), hash_algorithm).unwrap().page_id, 1);
  }
}

#[test]
fn cursor_rejects_duplicate_page_ids_in_distinct_canonical_pages() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let first = posting_page(hash_algorithm, &owner_id, &[10], 1, 0, 0);
  let duplicate = posting_page(hash_algorithm, &owner_id, &[20], 1, 0, 0);
  let root = leaf_directory(hash_algorithm, &owner_id, &[&first, &duplicate]);
  let decoded_root = decode_artifact_directory(&root.value, hash_algorithm).unwrap();
  let mut source = Source::default();
  for artifact in [&root, &first, &duplicate] {
    source.insert(artifact);
  }
  let error = load_artifact_page_cursor_v1(
    &ArtifactPageCursorRequestV1 {
      root: ArtifactPageCursorRootV1 {
        hash_algorithm,
        root_key: &root.key,
        owner_id: &owner_id,
        role: OrderedIndexRoleV1::Posting,
        maximum_generation: 7,
        expected_summary: Some(ArtifactDirectoryRootSummaryV1::from_directory(&decoded_root)),
      },
      seek: ArtifactPageSeekV1::PageId(1),
      neighbors: ArtifactPageNeighborModeV1::None,
      limits: ArtifactPageCursorLimitsV1::default(),
    },
    &mut source,
    &|| false,
  )
  .unwrap_err();
  assert_eq!(error.code(), "artifact_cursor_order");
}

#[test]
fn cursor_reuses_retained_artifacts_when_corrupt_descriptors_repeat_a_key() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let first = posting_page(hash_algorithm, &owner_id, &[10], 1, 0, 2);
  let second = posting_page(hash_algorithm, &owner_id, &[20], 2, 1, 0);
  let first_page = decode_ordered_page(&first.value, hash_algorithm).unwrap();
  let second_page = decode_ordered_page(&second.value, hash_algorithm).unwrap();
  let entries = [
    ArtifactDirectoryEntryWriteV1 {
      lower_fence: first_page.lower_fence,
      upper_fence: first_page.upper_fence,
      child_hash: &first.key,
      child_generation: first_page.generation,
      live_count: u64::from(first_page.live_count),
      tombstone_count: u64::from(first_page.tombstone_count),
      page_count: 1,
      logical_bytes: first_page.logical_live_bytes,
      minimum_page_id: first_page.page_id,
      maximum_page_id: first_page.page_id,
      physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    },
    ArtifactDirectoryEntryWriteV1 {
      lower_fence: second_page.lower_fence,
      upper_fence: second_page.upper_fence,
      child_hash: &first.key,
      child_generation: second_page.generation,
      live_count: u64::from(second_page.live_count),
      tombstone_count: u64::from(second_page.tombstone_count),
      page_count: 1,
      logical_bytes: second_page.logical_live_bytes,
      minimum_page_id: second_page.page_id,
      maximum_page_id: second_page.page_id,
      physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    },
  ];
  let root = encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id: &owner_id,
    generation: 7,
    level: 0,
    entries: &entries,
  })
  .unwrap();
  let decoded_root = decode_artifact_directory(&root.value, hash_algorithm).unwrap();
  let mut source = Source::default();
  source.insert(&root);
  source.insert(&first);

  let error = load_artifact_page_cursor_v1(
    &ArtifactPageCursorRequestV1 {
      root: ArtifactPageCursorRootV1 {
        hash_algorithm,
        root_key: &root.key,
        owner_id: &owner_id,
        role: OrderedIndexRoleV1::Posting,
        maximum_generation: 7,
        expected_summary: Some(ArtifactDirectoryRootSummaryV1::from_directory(&decoded_root)),
      },
      seek: ArtifactPageSeekV1::PageOrdinal(0),
      neighbors: ArtifactPageNeighborModeV1::Next,
      limits: ArtifactPageCursorLimitsV1::default(),
    },
    &mut source,
    &|| false,
  )
  .unwrap_err();
  assert_eq!(error.code(), "artifact_cursor_closure");
  assert_eq!(source.reads.iter().filter(|key| key.as_slice() == first.key.as_slice()).count(), 1);
}

#[test]
fn live_rank_seek_skips_tombstones_and_returns_the_exact_record_index() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let page = posting_page_with_states(hash_algorithm, &owner_id, &[(10, true), (11, false), (12, true), (13, false)], 1, 0, 0);
  let root = leaf_directory(hash_algorithm, &owner_id, &[&page]);
  let decoded_root = decode_artifact_directory(&root.value, hash_algorithm).unwrap();
  let root_authority = ArtifactPageCursorRootV1 {
    hash_algorithm,
    root_key: &root.key,
    owner_id: &owner_id,
    role: OrderedIndexRoleV1::Posting,
    maximum_generation: 7,
    expected_summary: Some(ArtifactDirectoryRootSummaryV1::from_directory(&decoded_root)),
  };
  let mut source = Source::default();
  source.insert(&root);
  source.insert(&page);
  for (rank, record_index) in [(0, 1), (1, 3)] {
    let loaded = load_artifact_page_cursor_v1(
      &ArtifactPageCursorRequestV1 {
        root: root_authority,
        seek: ArtifactPageSeekV1::LiveRecordRank(rank),
        neighbors: ArtifactPageNeighborModeV1::None,
        limits: ArtifactPageCursorLimitsV1::default(),
      },
      &mut source,
      &|| false,
    )
    .unwrap()
    .unwrap();
    assert_eq!(loaded.live_rank_within_page(), Some(rank));
    assert_eq!(loaded.record_index_within_page(), Some(record_index));
  }
}

#[test]
fn cursor_fails_closed_on_authority_links_sources_limits_and_cancellation() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let first = posting_page(hash_algorithm, &owner_id, &[10], 1, 0, 99);
  let second = posting_page(hash_algorithm, &owner_id, &[20], 2, 1, 0);
  let root = leaf_directory(hash_algorithm, &owner_id, &[&first, &second]);
  let decoded_root = decode_artifact_directory(&root.value, hash_algorithm).unwrap();
  let summary = ArtifactDirectoryRootSummaryV1::from_directory(&decoded_root);
  let root_authority = ArtifactPageCursorRootV1 {
    hash_algorithm,
    root_key: &root.key,
    owner_id: &owner_id,
    role: OrderedIndexRoleV1::Posting,
    maximum_generation: 7,
    expected_summary: Some(summary),
  };
  let request = ArtifactPageCursorRequestV1 {
    root: root_authority,
    seek: ArtifactPageSeekV1::PageOrdinal(0),
    neighbors: ArtifactPageNeighborModeV1::Next,
    limits: ArtifactPageCursorLimitsV1::default(),
  };

  let mut source = Source::default();
  for artifact in [&root, &first, &second] {
    source.insert(artifact);
  }
  assert_eq!(load_artifact_page_cursor_v1(&request, &mut source, &|| false).unwrap_err().code(), "ordered_index_closure");

  let mut missing = Source::default();
  assert_eq!(load_artifact_page_cursor_v1(&request, &mut missing, &|| false).unwrap_err().code(), "artifact_cursor_missing");
  let mut pressure = Source { failure: Some(ArtifactCursorReadErrorV1::ResourcePressure("budget".to_owned())), ..Default::default() };
  assert_eq!(load_artifact_page_cursor_v1(&request, &mut pressure, &|| false).unwrap_err().code(), "artifact_cursor_source_pressure");
  let mut operational = Source { failure: Some(ArtifactCursorReadErrorV1::Operational("disk".to_owned())), ..Default::default() };
  assert_eq!(load_artifact_page_cursor_v1(&request, &mut operational, &|| false).unwrap_err().code(), "artifact_cursor_source_operational");
  assert_eq!(load_artifact_page_cursor_v1(&request, &mut Source::default(), &|| true).unwrap_err().code(), "artifact_cursor_cancelled");

  let mut wrong_summary = summary;
  wrong_summary.live_count += 1;
  let wrong_root = ArtifactPageCursorRootV1 { expected_summary: Some(wrong_summary), ..root_authority };
  let mut source = Source::default();
  source.insert(&root);
  assert_eq!(
    load_artifact_page_cursor_v1(
      &ArtifactPageCursorRequestV1 { root: wrong_root, neighbors: ArtifactPageNeighborModeV1::None, ..request },
      &mut source,
      &|| false,
    )
    .unwrap_err()
    .code(),
    "artifact_cursor_closure"
  );

  let mut source = Source::default();
  source.insert(&root);
  let tiny = ArtifactPageCursorRequestV1 {
    limits: ArtifactPageCursorLimitsV1::new(16, 1).unwrap(),
    neighbors: ArtifactPageNeighborModeV1::None,
    ..request
  };
  assert_eq!(load_artifact_page_cursor_v1(&tiny, &mut source, &|| false).unwrap_err().code(), "artifact_cursor_resource");
  let out_of_range =
    ArtifactPageCursorRequestV1 { seek: ArtifactPageSeekV1::PageOrdinal(2), neighbors: ArtifactPageNeighborModeV1::None, ..request };
  let mut source = Source::default();
  source.insert(&root);
  assert_eq!(load_artifact_page_cursor_v1(&out_of_range, &mut source, &|| false).unwrap_err().code(), "artifact_cursor_rank");
  assert_eq!(ArtifactPageCursorLimitsV1::new(0, 1).unwrap_err().code(), "artifact_cursor_invalid_limits");
  assert!(matches!(
    load_artifact_page_cursor_v1(
      &ArtifactPageCursorRequestV1 { seek: ArtifactPageSeekV1::PageId(0), ..request },
      &mut Source::default(),
      &|| false,
    )
    .unwrap_err(),
    ArtifactPageCursorErrorV1::Malformed(_)
  ));
}

#[test]
fn batch_and_compaction_use_the_shared_cursor_without_a_second_directory_walker() {
  let batch = include_str!("../../src/engine/v4/index_batch_application.rs");
  let compaction = include_str!("../../src/engine/v4/index_native_compaction.rs");
  let cursor = include_str!("../../src/engine/v4/index_artifact_cursor.rs");
  for removed in [
    "fn descend_to_order_key",
    "fn descend_to_page_ordinal",
    "fn locate_logical_predecessor",
    "fn locate_logical_successor",
    "fn validate_directory_child",
  ] {
    assert!(!batch.contains(removed), "batch application retained duplicate traversal owner {removed}");
  }
  assert_eq!(batch.matches("load_artifact_page_cursor_v1(").count(), 2);
  assert!(compaction.contains("load_ordered_page_ordinal_path_v1"));
  assert_eq!(cursor.matches("fn locate_neighbor(").count(), 1);
}
