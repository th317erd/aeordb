use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::index_copy_on_write::{
  ArtifactDirectoryMutationRequestV1, ArtifactDirectoryPathV1, OrderedPageMutationKindV1, OrderedPageMutationRequestV1,
  default_index_directory_layout_v1, default_index_page_layout_v1, mutate_ordered_page_v1, rewrite_artifact_directory_paths_v1,
};
use aeordb::engine::v4::index_page::{
  ArtifactDirectoryEntryWriteV1, ArtifactDirectoryWriteV1, OrderedIndexRoleV1, OrderedPageWriteV1, PhysicalHintV1, PostingRecordV1,
  decode_artifact_directory, decode_ordered_page, decode_ordered_record, decode_posting_record, encode_artifact_directory,
  encode_ordered_page, encode_posting_record, ordered_record_order_key, validate_posting_page_link,
};
use aeordb::engine::v4::reader::MalformedInputClass;

fn owner(hash_algorithm: HashAlgorithm) -> Vec<u8> {
  (1..=hash_algorithm.hash_length()).map(|value| u8::try_from(value).unwrap()).collect()
}

fn posting_record(coordinate: u64, document_ordinal: u64, key_length: usize, tombstone: bool) -> Vec<u8> {
  let mut posting_key = vec![b'k'; key_length];
  posting_key[..8].copy_from_slice(&coordinate.to_le_bytes());
  encode_posting_record(&PostingRecordV1 {
    tombstone,
    coordinate,
    document_ordinal,
    source_value_ordinal: 0,
    expansion_ordinal: 0,
    posting_key: &posting_key,
  })
  .unwrap()
}

fn posting_page(
  hash_algorithm: HashAlgorithm,
  owner_id: &[u8],
  generation: u64,
  page_id: u64,
  previous_page_id: u64,
  next_page_id: u64,
  records: &[Vec<u8>],
) -> Vec<u8> {
  let record_slices = records.iter().map(Vec::as_slice).collect::<Vec<_>>();
  encode_ordered_page(&OrderedPageWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id,
    generation,
    page_id,
    previous_page_id,
    next_page_id,
    records: &record_slices,
  })
  .unwrap()
  .value
}

fn decoded_coordinates(page: &[u8], hash_algorithm: HashAlgorithm) -> Vec<(u64, bool)> {
  decode_ordered_page(page, hash_algorithm)
    .unwrap()
    .records
    .iter()
    .map(|record| {
      let record = record.unwrap();
      let posting = decode_posting_record(record.encoded).unwrap();
      (posting.coordinate, posting.tombstone)
    })
    .collect()
}

fn posting_fence(hash_algorithm: HashAlgorithm, coordinate: u64) -> Vec<u8> {
  let record = posting_record(coordinate, coordinate, 16, false);
  let decoded = decode_ordered_record(&record, hash_algorithm, OrderedIndexRoleV1::Posting).unwrap();
  ordered_record_order_key(&decoded).unwrap()
}

fn leaf_directory(
  hash_algorithm: HashAlgorithm,
  owner_id: &[u8],
  generation: u64,
  pages: &[&[u8]],
  physical_hint: PhysicalHintV1,
) -> Vec<u8> {
  let decoded_pages = pages.iter().map(|page| decode_ordered_page(page, hash_algorithm).unwrap()).collect::<Vec<_>>();
  let entries = decoded_pages
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
      physical_hint,
    })
    .collect::<Vec<_>>();
  encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id,
    generation,
    level: 0,
    entries: &entries,
  })
  .unwrap()
  .value
}

fn internal_directory(
  hash_algorithm: HashAlgorithm,
  owner_id: &[u8],
  generation: u64,
  children: &[&[u8]],
  physical_hint: PhysicalHintV1,
) -> Vec<u8> {
  let decoded_children = children.iter().map(|directory| decode_artifact_directory(directory, hash_algorithm).unwrap()).collect::<Vec<_>>();
  let level = decoded_children[0].level + 1;
  let entries = decoded_children
    .iter()
    .map(|child| ArtifactDirectoryEntryWriteV1 {
      lower_fence: child.lower_fence,
      upper_fence: child.upper_fence,
      child_hash: &child.key,
      child_generation: child.generation,
      live_count: child.live_count,
      tombstone_count: child.tombstone_count,
      page_count: child.page_count,
      logical_bytes: child.logical_bytes,
      minimum_page_id: child.minimum_page_id,
      maximum_page_id: child.maximum_page_id,
      physical_hint,
    })
    .collect::<Vec<_>>();
  encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id,
    generation,
    level,
    entries: &entries,
  })
  .unwrap()
  .value
}

#[test]
fn cow_upsert_is_deterministic_bounded_and_preserves_the_immutable_source() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let owner_id = owner(hash_algorithm);
    let source_records = vec![posting_record(1, 1, 16, false), posting_record(3, 3, 16, false)];
    let source_page = posting_page(hash_algorithm, &owner_id, 7, 10, 0, 0, &source_records);
    let source_before = source_page.clone();
    let inserted = posting_record(2, 2, 16, false);
    let request = OrderedPageMutationRequestV1 {
      hash_algorithm,
      source_page: &source_page,
      next_posting_page: None,
      generation: 8,
      next_page_id: 40,
      mutation: OrderedPageMutationKindV1::UpsertLive(&inserted),
      layout: default_index_page_layout_v1(),
    };

    let first = mutate_ordered_page_v1(&request).unwrap();
    let second = mutate_ordered_page_v1(&request).unwrap();
    assert_eq!(source_page, source_before);
    assert_eq!(first, second);
    assert_eq!(first.next_page_id, 40);
    assert!(first.allocated_page_ids.is_empty());
    assert!(first.retired_page_ids.is_empty());
    assert_eq!(first.replacements.len(), 1);
    assert_eq!(first.replacements[0].source_key, decode_ordered_page(&source_page, hash_algorithm).unwrap().key);
    assert_eq!(first.replacements[0].artifacts.len(), 1);

    let rewritten = decode_ordered_page(&first.replacements[0].artifacts[0].value, hash_algorithm).unwrap();
    assert_eq!(rewritten.generation, 8);
    assert_eq!(rewritten.page_id, 10);
    assert_eq!(rewritten.previous_page_id, 0);
    assert_eq!(rewritten.next_page_id, 0);
    assert_eq!(decoded_coordinates(&first.replacements[0].artifacts[0].value, hash_algorithm), vec![(1, false), (2, false), (3, false)]);
  }
}

#[test]
fn cow_upsert_of_identical_bytes_is_an_idempotent_noop() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let record = posting_record(1, 1, 16, false);
  let source_page = posting_page(hash_algorithm, &owner_id, 7, 10, 0, 0, std::slice::from_ref(&record));
  let plan = mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
    hash_algorithm,
    source_page: &source_page,
    next_posting_page: None,
    generation: 8,
    next_page_id: 40,
    mutation: OrderedPageMutationKindV1::UpsertLive(&record),
    layout: default_index_page_layout_v1(),
  })
  .unwrap();

  assert!(plan.is_unchanged());
  assert_eq!(plan.next_page_id, 40);
}

#[test]
fn cow_delete_requires_an_existing_exact_key_and_writes_a_tombstone() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let source_records = vec![posting_record(1, 1, 16, false), posting_record(2, 2, 16, false)];
  let source_page = posting_page(hash_algorithm, &owner_id, 7, 10, 0, 0, &source_records);
  let tombstone = posting_record(2, 2, 16, true);
  let plan = mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
    hash_algorithm,
    source_page: &source_page,
    next_posting_page: None,
    generation: 8,
    next_page_id: 40,
    mutation: OrderedPageMutationKindV1::TombstoneExisting(&tombstone),
    layout: default_index_page_layout_v1(),
  })
  .unwrap();
  let rewritten = &plan.replacements[0].artifacts[0].value;
  let page = decode_ordered_page(rewritten, hash_algorithm).unwrap();
  assert_eq!(page.live_count, 1);
  assert_eq!(page.tombstone_count, 1);
  assert_eq!(decoded_coordinates(rewritten, hash_algorithm), vec![(1, false), (2, true)]);

  let missing = posting_record(3, 3, 16, true);
  let error = mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
    mutation: OrderedPageMutationKindV1::TombstoneExisting(&missing),
    ..OrderedPageMutationRequestV1 {
      hash_algorithm,
      source_page: &source_page,
      next_posting_page: None,
      generation: 8,
      next_page_id: 40,
      mutation: OrderedPageMutationKindV1::TombstoneExisting(&tombstone),
      layout: default_index_page_layout_v1(),
    }
  })
  .unwrap_err();
  assert_eq!(error.code(), "index_cow_tombstone_missing");
  assert_eq!(error.class(), MalformedInputClass::CrossRecordClosureMismatch);

  let live_delete = posting_record(2, 2, 16, false);
  assert_eq!(
    mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
      hash_algorithm,
      source_page: &source_page,
      next_posting_page: None,
      generation: 8,
      next_page_id: 40,
      mutation: OrderedPageMutationKindV1::TombstoneExisting(&live_delete),
      layout: default_index_page_layout_v1(),
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
}

#[test]
fn tombstone_only_page_remains_representable_in_an_artifact_directory() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let tombstone = posting_record(1, 1, 16, true);
  let page_bytes = posting_page(hash_algorithm, &owner_id, 8, 10, 0, 0, &[tombstone]);
  let page = decode_ordered_page(&page_bytes, hash_algorithm).unwrap();
  assert_eq!(page.live_count, 0);
  assert_eq!(page.tombstone_count, 1);
  assert_eq!(page.logical_live_bytes, 0);

  let directory = encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm,
    role: page.role,
    owner_id: page.owner_id,
    generation: page.generation,
    level: 0,
    entries: &[ArtifactDirectoryEntryWriteV1 {
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
    }],
  })
  .unwrap();
  let decoded = decode_artifact_directory(&directory.value, hash_algorithm).unwrap();
  assert_eq!(decoded.live_count, 0);
  assert_eq!(decoded.tombstone_count, 1);
  assert_eq!(decoded.logical_bytes, 0);

  let inconsistent_entry = ArtifactDirectoryEntryWriteV1 {
    lower_fence: page.lower_fence,
    upper_fence: page.upper_fence,
    child_hash: &page.key,
    child_generation: page.generation,
    live_count: 0,
    tombstone_count: 1,
    page_count: 1,
    logical_bytes: 1,
    minimum_page_id: page.page_id,
    maximum_page_id: page.page_id,
    physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
  };
  let inconsistent = encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm,
    role: page.role,
    owner_id: page.owner_id,
    generation: page.generation,
    level: 0,
    entries: &[inconsistent_entry],
  })
  .unwrap_err();
  assert_eq!(inconsistent.class(), MalformedInputClass::CrossRecordClosureMismatch);
}

#[test]
fn cow_split_retains_the_left_id_allocates_the_right_and_relinks_the_next_page() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let source_records = (1..=23).map(|coordinate| posting_record(coordinate, coordinate, 3_800, false)).collect::<Vec<_>>();
  let source_page = posting_page(hash_algorithm, &owner_id, 7, 10, 0, 30, &source_records);
  assert!(source_page.len() <= default_index_page_layout_v1().split_above_bytes);
  let next_records = vec![posting_record(100, 100, 16, false)];
  let next_page = posting_page(hash_algorithm, &owner_id, 6, 30, 10, 0, &next_records);
  let inserted = posting_record(24, 24, 3_800, false);

  let plan = mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
    hash_algorithm,
    source_page: &source_page,
    next_posting_page: Some(&next_page),
    generation: 8,
    next_page_id: 40,
    mutation: OrderedPageMutationKindV1::UpsertLive(&inserted),
    layout: default_index_page_layout_v1(),
  })
  .unwrap();

  assert_eq!(plan.allocated_page_ids, vec![40]);
  assert_eq!(plan.next_page_id, 41);
  assert!(plan.retired_page_ids.is_empty());
  assert_eq!(plan.replacements.len(), 2);
  assert_eq!(plan.replacements[0].artifacts.len(), 2);
  assert_eq!(plan.replacements[1].artifacts.len(), 1);
  let left = decode_ordered_page(&plan.replacements[0].artifacts[0].value, hash_algorithm).unwrap();
  let right = decode_ordered_page(&plan.replacements[0].artifacts[1].value, hash_algorithm).unwrap();
  let rewritten_next = decode_ordered_page(&plan.replacements[1].artifacts[0].value, hash_algorithm).unwrap();
  assert_eq!(left.page_id, 10);
  assert_eq!(right.page_id, 40);
  assert_eq!(left.next_page_id, 40);
  assert_eq!(right.previous_page_id, 10);
  assert_eq!(right.next_page_id, 30);
  assert_eq!(rewritten_next.page_id, 30);
  assert_eq!(rewritten_next.previous_page_id, 40);
  assert_eq!(rewritten_next.generation, 8);
  validate_posting_page_link(&left, &right, hash_algorithm).unwrap();
  validate_posting_page_link(&right, &rewritten_next, hash_algorithm).unwrap();
  assert!(plan.replacements[0].artifacts.iter().all(|artifact| {
    let page = decode_ordered_page(&artifact.value, hash_algorithm).unwrap();
    artifact.value.len() <= default_index_page_layout_v1().target_bytes || page.records.len() == 1
  }));
  let coordinates =
    plan.replacements[0].artifacts.iter().flat_map(|artifact| decoded_coordinates(&artifact.value, hash_algorithm)).collect::<Vec<_>>();
  assert_eq!(coordinates, (1..=24).map(|coordinate| (coordinate, false)).collect::<Vec<_>>());
}

#[test]
fn directory_cow_rewrites_two_leaf_paths_deduplicates_the_root_and_clears_hints() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let source_records = (1..=23).map(|coordinate| posting_record(coordinate, coordinate, 3_800, false)).collect::<Vec<_>>();
  let source_page = posting_page(hash_algorithm, &owner_id, 7, 10, 0, 30, &source_records);
  let next_page = posting_page(hash_algorithm, &owner_id, 6, 30, 10, 0, &[posting_record(100, 100, 16, false)]);
  let source_leaf = leaf_directory(
    hash_algorithm,
    &owner_id,
    7,
    &[&source_page],
    PhysicalHintV1 { wal_offset: 100, total_length: 200, write_sequence: 300 },
  );
  let next_leaf =
    leaf_directory(hash_algorithm, &owner_id, 7, &[&next_page], PhysicalHintV1 { wal_offset: 400, total_length: 500, write_sequence: 600 });
  let root = internal_directory(
    hash_algorithm,
    &owner_id,
    7,
    &[&source_leaf, &next_leaf],
    PhysicalHintV1 { wal_offset: 700, total_length: 800, write_sequence: 900 },
  );
  let inserted = posting_record(24, 24, 3_800, false);
  let page_plan = mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
    hash_algorithm,
    source_page: &source_page,
    next_posting_page: Some(&next_page),
    generation: 8,
    next_page_id: 40,
    mutation: OrderedPageMutationKindV1::UpsertLive(&inserted),
    layout: default_index_page_layout_v1(),
  })
  .unwrap();
  let source_page_key = decode_ordered_page(&source_page, hash_algorithm).unwrap().key;
  let next_page_key = decode_ordered_page(&next_page, hash_algorithm).unwrap().key;
  let source_path_nodes = [&root[..], &source_leaf[..]];
  let next_path_nodes = [&root[..], &next_leaf[..]];
  let paths = [
    ArtifactDirectoryPathV1 { source_page_key: &source_page_key, directories: &source_path_nodes },
    ArtifactDirectoryPathV1 { source_page_key: &next_page_key, directories: &next_path_nodes },
  ];

  let plan = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
    hash_algorithm,
    generation: 8,
    page_plan: &page_plan,
    paths: &paths,
    layout: default_index_directory_layout_v1(),
  })
  .unwrap();
  let reversed_paths = [paths[1], paths[0]];
  let reversed_plan = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
    hash_algorithm,
    generation: 8,
    page_plan: &page_plan,
    paths: &reversed_paths,
    layout: default_index_directory_layout_v1(),
  })
  .unwrap();

  assert_eq!(plan, reversed_plan);
  assert_eq!(plan.source_root_key, decode_artifact_directory(&root, hash_algorithm).unwrap().key);
  assert_eq!(plan.artifacts.len(), 3);
  let first_leaf = decode_artifact_directory(&plan.artifacts[0].value, hash_algorithm).unwrap();
  let second_leaf = decode_artifact_directory(&plan.artifacts[1].value, hash_algorithm).unwrap();
  let rewritten_root = decode_artifact_directory(&plan.artifacts[2].value, hash_algorithm).unwrap();
  assert_eq!((first_leaf.level, second_leaf.level, rewritten_root.level), (0, 0, 1));
  assert_eq!(rewritten_root.entries.len(), 2);
  assert_eq!(plan.root_key, rewritten_root.key);
  assert_eq!(rewritten_root.generation, 8);
  assert!([first_leaf, second_leaf, rewritten_root].into_iter().flat_map(|directory| directory.entries).all(|entry| !entry
    .physical_hint
    .is_complete()
    && entry.physical_hint.wal_offset == 0
    && entry.physical_hint.write_sequence == 0));
}

#[test]
fn directory_cow_rewrites_a_single_leaf_at_both_hash_widths() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let owner_id = owner(hash_algorithm);
    let source_page = posting_page(hash_algorithm, &owner_id, 7, 10, 0, 0, &[posting_record(1, 1, 16, false)]);
    let source = decode_ordered_page(&source_page, hash_algorithm).unwrap();
    let source_leaf = leaf_directory(
      hash_algorithm,
      &owner_id,
      7,
      &[&source_page],
      PhysicalHintV1 { wal_offset: 10, total_length: 20, write_sequence: 30 },
    );
    let inserted = posting_record(2, 2, 16, false);
    let page_plan = mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
      hash_algorithm,
      source_page: &source_page,
      next_posting_page: None,
      generation: 8,
      next_page_id: 40,
      mutation: OrderedPageMutationKindV1::UpsertLive(&inserted),
      layout: default_index_page_layout_v1(),
    })
    .unwrap();
    let path_nodes = [&source_leaf[..]];
    let paths = [ArtifactDirectoryPathV1 { source_page_key: &source.key, directories: &path_nodes }];
    let plan = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
      hash_algorithm,
      generation: 8,
      page_plan: &page_plan,
      paths: &paths,
      layout: default_index_directory_layout_v1(),
    })
    .unwrap();
    assert_eq!(plan.artifacts.len(), 1);
    let root = decode_artifact_directory(&plan.artifacts[0].value, hash_algorithm).unwrap();
    assert_eq!(plan.root_key, root.key);
    assert_eq!(root.owner_id, owner_id);
    assert_eq!(root.entries.len(), 1);
    assert!(!root.entries[0].physical_hint.is_complete());
  }
}

#[test]
fn directory_cow_recursively_splits_a_leaf_and_parent_then_grows_a_new_root() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let source_records = (2_000..2_023).map(|coordinate| posting_record(coordinate, coordinate, 3_800, false)).collect::<Vec<_>>();
  let source_page = posting_page(hash_algorithm, &owner_id, 7, 5_000, 0, 0, &source_records);
  let source = decode_ordered_page(&source_page, hash_algorithm).unwrap();

  let leaf_fences = (1_000..1_292).map(|coordinate| posting_fence(hash_algorithm, coordinate)).collect::<Vec<_>>();
  let leaf_hashes = (1_000u64..1_292)
    .map(|coordinate| {
      let mut hash = vec![0x51; hash_algorithm.hash_length()];
      hash[..8].copy_from_slice(&coordinate.to_le_bytes());
      hash
    })
    .collect::<Vec<_>>();
  let mut leaf_entries = leaf_fences
    .iter()
    .zip(&leaf_hashes)
    .enumerate()
    .map(|(index, (fence, hash))| ArtifactDirectoryEntryWriteV1 {
      lower_fence: fence,
      upper_fence: fence,
      child_hash: hash,
      child_generation: 7,
      live_count: 1,
      tombstone_count: 0,
      page_count: 1,
      logical_bytes: 48,
      minimum_page_id: u64::try_from(index + 1).unwrap(),
      maximum_page_id: u64::try_from(index + 1).unwrap(),
      physical_hint: PhysicalHintV1 { wal_offset: 1, total_length: 2, write_sequence: 3 },
    })
    .collect::<Vec<_>>();
  leaf_entries.push(ArtifactDirectoryEntryWriteV1 {
    lower_fence: source.lower_fence,
    upper_fence: source.upper_fence,
    child_hash: &source.key,
    child_generation: source.generation,
    live_count: u64::from(source.live_count),
    tombstone_count: u64::from(source.tombstone_count),
    page_count: 1,
    logical_bytes: source.logical_live_bytes,
    minimum_page_id: source.page_id,
    maximum_page_id: source.page_id,
    physical_hint: PhysicalHintV1 { wal_offset: 4, total_length: 5, write_sequence: 6 },
  });
  let source_leaf = encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id: &owner_id,
    generation: 7,
    level: 0,
    entries: &leaf_entries,
  })
  .unwrap()
  .value;
  assert!(source_leaf.len() <= default_index_directory_layout_v1().target_bytes);
  let source_leaf_summary = decode_artifact_directory(&source_leaf, hash_algorithm).unwrap();

  let root_fences = (1..288).map(|coordinate| posting_fence(hash_algorithm, coordinate)).collect::<Vec<_>>();
  let root_hashes = (1u64..288)
    .map(|coordinate| {
      let mut hash = vec![0x71; hash_algorithm.hash_length()];
      hash[..8].copy_from_slice(&coordinate.to_le_bytes());
      hash
    })
    .collect::<Vec<_>>();
  let mut root_entries = root_fences
    .iter()
    .zip(&root_hashes)
    .enumerate()
    .map(|(index, (fence, hash))| ArtifactDirectoryEntryWriteV1 {
      lower_fence: fence,
      upper_fence: fence,
      child_hash: hash,
      child_generation: 7,
      live_count: 1,
      tombstone_count: 0,
      page_count: 1,
      logical_bytes: 48,
      minimum_page_id: u64::try_from(index + 1).unwrap(),
      maximum_page_id: u64::try_from(index + 1).unwrap(),
      physical_hint: PhysicalHintV1 { wal_offset: 7, total_length: 8, write_sequence: 9 },
    })
    .collect::<Vec<_>>();
  root_entries.push(ArtifactDirectoryEntryWriteV1 {
    lower_fence: source_leaf_summary.lower_fence,
    upper_fence: source_leaf_summary.upper_fence,
    child_hash: &source_leaf_summary.key,
    child_generation: source_leaf_summary.generation,
    live_count: source_leaf_summary.live_count,
    tombstone_count: source_leaf_summary.tombstone_count,
    page_count: source_leaf_summary.page_count,
    logical_bytes: source_leaf_summary.logical_bytes,
    minimum_page_id: source_leaf_summary.minimum_page_id,
    maximum_page_id: source_leaf_summary.maximum_page_id,
    physical_hint: PhysicalHintV1 { wal_offset: 10, total_length: 11, write_sequence: 12 },
  });
  let source_root = encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id: &owner_id,
    generation: 7,
    level: 1,
    entries: &root_entries,
  })
  .unwrap()
  .value;
  assert!(source_root.len() <= default_index_directory_layout_v1().target_bytes);

  let inserted = posting_record(2_023, 2_023, 3_800, false);
  let page_plan = mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
    hash_algorithm,
    source_page: &source_page,
    next_posting_page: None,
    generation: 8,
    next_page_id: 6_000,
    mutation: OrderedPageMutationKindV1::UpsertLive(&inserted),
    layout: default_index_page_layout_v1(),
  })
  .unwrap();
  assert_eq!(page_plan.replacements[0].artifacts.len(), 2);
  let path_nodes = [&source_root[..], &source_leaf[..]];
  let paths = [ArtifactDirectoryPathV1 { source_page_key: &source.key, directories: &path_nodes }];
  let plan = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
    hash_algorithm,
    generation: 8,
    page_plan: &page_plan,
    paths: &paths,
    layout: default_index_directory_layout_v1(),
  })
  .unwrap();

  let levels =
    plan.artifacts.iter().map(|artifact| decode_artifact_directory(&artifact.value, hash_algorithm).unwrap().level).collect::<Vec<_>>();
  assert_eq!(levels, vec![0, 0, 1, 1, 2]);
  assert_eq!(plan.root_level, 2);
  assert_eq!(plan.root_key, plan.artifacts.last().unwrap().key);
  assert!(plan.artifacts.iter().all(|artifact| artifact.value.len() <= default_index_directory_layout_v1().target_bytes));
}

#[test]
fn directory_cow_rejects_missing_corrupt_and_cross_owner_paths_before_output() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let source_page = posting_page(hash_algorithm, &owner_id, 7, 10, 0, 0, &[posting_record(1, 1, 16, false)]);
  let source = decode_ordered_page(&source_page, hash_algorithm).unwrap();
  let inserted = posting_record(2, 2, 16, false);
  let page_plan = mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
    hash_algorithm,
    source_page: &source_page,
    next_posting_page: None,
    generation: 8,
    next_page_id: 40,
    mutation: OrderedPageMutationKindV1::UpsertLive(&inserted),
    layout: default_index_page_layout_v1(),
  })
  .unwrap();
  let valid_leaf =
    leaf_directory(hash_algorithm, &owner_id, 7, &[&source_page], PhysicalHintV1 { wal_offset: 1, total_length: 2, write_sequence: 3 });

  let no_paths = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
    hash_algorithm,
    generation: 8,
    page_plan: &page_plan,
    paths: &[],
    layout: default_index_directory_layout_v1(),
  })
  .unwrap_err();
  assert_eq!(no_paths.code(), "index_cow_directory_path_count");

  let valid_leaf_summary = decode_artifact_directory(&valid_leaf, hash_algorithm).unwrap();
  let forged_root = encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id: &owner_id,
    generation: 7,
    level: 1,
    entries: &[ArtifactDirectoryEntryWriteV1 {
      lower_fence: valid_leaf_summary.lower_fence,
      upper_fence: valid_leaf_summary.upper_fence,
      child_hash: &valid_leaf_summary.key,
      child_generation: valid_leaf_summary.generation,
      live_count: valid_leaf_summary.live_count,
      tombstone_count: valid_leaf_summary.tombstone_count,
      page_count: valid_leaf_summary.page_count,
      logical_bytes: valid_leaf_summary.logical_bytes + 1,
      minimum_page_id: valid_leaf_summary.minimum_page_id,
      maximum_page_id: valid_leaf_summary.maximum_page_id,
      physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    }],
  })
  .unwrap()
  .value;
  let forged_nodes = [&forged_root[..], &valid_leaf[..]];
  let forged_paths = [ArtifactDirectoryPathV1 { source_page_key: &source.key, directories: &forged_nodes }];
  let forged = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
    hash_algorithm,
    generation: 8,
    page_plan: &page_plan,
    paths: &forged_paths,
    layout: default_index_directory_layout_v1(),
  })
  .unwrap_err();
  assert_eq!(forged.code(), "index_cow_directory_parent_child");
  assert_eq!(forged.class(), MalformedInputClass::CrossRecordClosureMismatch);

  let other_page = posting_page(hash_algorithm, &owner_id, 7, 20, 0, 0, &[posting_record(100, 100, 16, false)]);
  let missing_leaf =
    leaf_directory(hash_algorithm, &owner_id, 7, &[&other_page], PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 });
  let missing_nodes = [&missing_leaf[..]];
  let missing_paths = [ArtifactDirectoryPathV1 { source_page_key: &source.key, directories: &missing_nodes }];
  let missing = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
    hash_algorithm,
    generation: 8,
    page_plan: &page_plan,
    paths: &missing_paths,
    layout: default_index_directory_layout_v1(),
  })
  .unwrap_err();
  assert_eq!(missing.code(), "index_cow_directory_child_hash_missing");
  assert_eq!(missing.class(), MalformedInputClass::CrossRecordClosureMismatch);

  let mut corrupt_leaf = valid_leaf.clone();
  *corrupt_leaf.last_mut().unwrap() ^= 0x80;
  let corrupt_nodes = [&corrupt_leaf[..]];
  let corrupt_paths = [ArtifactDirectoryPathV1 { source_page_key: &source.key, directories: &corrupt_nodes }];
  let corrupt = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
    hash_algorithm,
    generation: 8,
    page_plan: &page_plan,
    paths: &corrupt_paths,
    layout: default_index_directory_layout_v1(),
  })
  .unwrap_err();
  assert_eq!(corrupt.class(), MalformedInputClass::ChecksumOrIntegrityMismatch);

  let mut other_owner = owner_id.clone();
  other_owner[0] ^= 0xff;
  let cross_owner_leaf =
    leaf_directory(hash_algorithm, &other_owner, 7, &[&source_page], PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 });
  let cross_owner_nodes = [&cross_owner_leaf[..]];
  let cross_owner_paths = [ArtifactDirectoryPathV1 { source_page_key: &source.key, directories: &cross_owner_nodes }];
  let cross_owner = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
    hash_algorithm,
    generation: 8,
    page_plan: &page_plan,
    paths: &cross_owner_paths,
    layout: default_index_directory_layout_v1(),
  })
  .unwrap_err();
  assert_eq!(cross_owner.code(), "index_cow_directory_path_identity");
  assert_eq!(cross_owner.class(), MalformedInputClass::CrossRecordClosureMismatch);
}

#[test]
fn cow_mutation_rejects_generation_regression_missing_neighbors_and_page_id_exhaustion() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let source_records = (1..=23).map(|coordinate| posting_record(coordinate, coordinate, 3_800, false)).collect::<Vec<_>>();
  let source_page = posting_page(hash_algorithm, &owner_id, 7, 10, 0, 30, &source_records);
  let inserted = posting_record(24, 24, 3_800, false);
  let request = OrderedPageMutationRequestV1 {
    hash_algorithm,
    source_page: &source_page,
    next_posting_page: None,
    generation: 8,
    next_page_id: 40,
    mutation: OrderedPageMutationKindV1::UpsertLive(&inserted),
    layout: default_index_page_layout_v1(),
  };

  assert_eq!(mutate_ordered_page_v1(&request).unwrap_err().code(), "index_cow_next_page_missing");
  assert_eq!(
    mutate_ordered_page_v1(&OrderedPageMutationRequestV1 { generation: 7, ..request }).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  assert_eq!(
    mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
      next_posting_page: Some(&posting_page(hash_algorithm, &owner_id, 6, 30, 10, 0, &[posting_record(100, 100, 16, false)],)),
      next_page_id: u64::MAX,
      ..request
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::LengthCountOrArithmeticOverflow
  );
}

#[test]
fn cow_mutation_rejects_a_page_id_high_water_that_does_not_cover_linked_neighbors() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let records = vec![posting_record(1, 1, 16, false)];
  let source_page = posting_page(hash_algorithm, &owner_id, 7, 10, 9, 30, &records);
  let inserted = posting_record(2, 2, 16, false);

  let error = mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
    hash_algorithm,
    source_page: &source_page,
    next_posting_page: None,
    generation: 8,
    next_page_id: 30,
    mutation: OrderedPageMutationKindV1::UpsertLive(&inserted),
    layout: default_index_page_layout_v1(),
  })
  .unwrap_err();
  assert_eq!(error.code(), "index_cow_page_id_high_water");
  assert_eq!(error.class(), MalformedInputClass::IdentityKeyOrGenerationMismatch);
}

#[test]
fn cow_split_rejects_a_page_id_high_water_below_the_next_neighbors_outward_link() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let source_records = (1..=23).map(|coordinate| posting_record(coordinate, coordinate, 3_800, false)).collect::<Vec<_>>();
  let source_page = posting_page(hash_algorithm, &owner_id, 7, 10, 0, 30, &source_records);
  let next_page = posting_page(hash_algorithm, &owner_id, 6, 30, 10, 90, &[posting_record(100, 100, 16, false)]);
  let inserted = posting_record(24, 24, 3_800, false);

  let error = mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
    hash_algorithm,
    source_page: &source_page,
    next_posting_page: Some(&next_page),
    generation: 8,
    next_page_id: 40,
    mutation: OrderedPageMutationKindV1::UpsertLive(&inserted),
    layout: default_index_page_layout_v1(),
  })
  .unwrap_err();
  assert_eq!(error.code(), "index_cow_neighbor_page_id");
  assert_eq!(error.class(), MalformedInputClass::IdentityKeyOrGenerationMismatch);
}

#[test]
fn cow_partitions_an_over_hard_cap_candidate_before_encoding() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let source_records = vec![posting_record(1, 1, 900_000, false), posting_record(2, 2, 900_000, false)];
  let source_page = posting_page(hash_algorithm, &owner_id, 7, 10, 0, 0, &source_records);
  let inserted = posting_record(3, 3, 900_000, false);

  let plan = mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
    hash_algorithm,
    source_page: &source_page,
    next_posting_page: None,
    generation: 8,
    next_page_id: 40,
    mutation: OrderedPageMutationKindV1::UpsertLive(&inserted),
    layout: default_index_page_layout_v1(),
  })
  .unwrap();

  assert_eq!(plan.replacements[0].artifacts.len(), 3);
  assert!(plan.replacements[0].artifacts.iter().all(|artifact| artifact.value.len() <= default_index_page_layout_v1().hard_artifact_bytes));
}

#[test]
fn cow_rejects_combined_record_and_output_workspace_amplification() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let source_records = vec![posting_record(1, 1, 600_000, false), posting_record(2, 2, 600_000, false)];
  let source_page = posting_page(hash_algorithm, &owner_id, 7, 10, 0, 0, &source_records);
  let inserted = posting_record(3, 3, 600_000, false);
  let layout = aeordb::engine::v4::index_copy_on_write::IndexPageLayoutV1 {
    maximum_workspace_bytes: 8 * 1_024 * 1_024,
    ..default_index_page_layout_v1()
  };

  let error = mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
    hash_algorithm,
    source_page: &source_page,
    next_posting_page: None,
    generation: 8,
    next_page_id: 40,
    mutation: OrderedPageMutationKindV1::UpsertLive(&inserted),
    layout,
  })
  .unwrap_err();
  assert_eq!(error.code(), "index_cow_peak_workspace_exceeded");
  assert_eq!(error.class(), MalformedInputClass::AllocationAmplification);
}

#[test]
fn cow_mutation_rejects_output_amplification_within_the_operation_workspace() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let source_records = (1..=5).map(|coordinate| posting_record(coordinate, coordinate, 500_000, false)).collect::<Vec<_>>();
  let source_page = posting_page(hash_algorithm, &owner_id, 7, 10, 0, 0, &source_records);
  let inserted = posting_record(6, 6, 500_000, false);
  let layout = aeordb::engine::v4::index_copy_on_write::IndexPageLayoutV1 {
    maximum_workspace_bytes: 8 * 1_024 * 1_024,
    ..default_index_page_layout_v1()
  };

  let error = mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
    hash_algorithm,
    source_page: &source_page,
    next_posting_page: None,
    generation: 8,
    next_page_id: 40,
    mutation: OrderedPageMutationKindV1::UpsertLive(&inserted),
    layout,
  })
  .unwrap_err();
  assert_eq!(error.code(), "index_cow_output_exceeds_workspace");
  assert_eq!(error.class(), MalformedInputClass::AllocationAmplification);
}
