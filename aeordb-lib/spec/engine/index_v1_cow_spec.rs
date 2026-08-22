use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_copy_on_write::{
  ArtifactDirectoryMutationRequestV1, ArtifactDirectoryPathV1, IndexCopyOnWriteClosureRequestV1, OrderedPageCompactionWindowRequestV1,
  OrderedPageBatchMutationRequestV1, OrderedPageMutationKindV1, OrderedPageMutationRequestV1, TombstoneDropProofV1,
  compact_ordered_page_window_v1, default_index_directory_layout_v1, default_index_page_layout_v1, mutate_ordered_page_batch_v1,
  mutate_ordered_page_v1, rewrite_artifact_directory_paths_v1, validate_index_copy_on_write_closure_v1,
};
use aeordb::engine::v4::index_page::{
  ArtifactDirectoryEntryWriteV1, ArtifactDirectoryWriteV1, OrderedIndexRoleV1, OrderedPageWriteV1, PhysicalHintV1, PostingRecordV1,
  checked_ordered_record_order_key_length, decode_artifact_directory, decode_ordered_page, decode_ordered_record, decode_posting_record,
  encode_artifact_directory, encode_ordered_page, encode_posting_record, ordered_record_order_key, validate_posting_page_link,
};
use aeordb::engine::v4::index_record::{ScopeDocumentRecordV1, encode_scope_document_record};
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

fn scope_document_record(hash_algorithm: HashAlgorithm, document_ordinal: u64, path: &str) -> Vec<u8> {
  let file_key = digest_parts(hash_algorithm, &[b"file:", path.as_bytes()]);
  let record_revision_hash = digest_parts(hash_algorithm, &[b"revision:", path.as_bytes()]);
  encode_scope_document_record(
    &ScopeDocumentRecordV1 { tombstone: false, document_ordinal, file_key: &file_key, record_revision_hash: &record_revision_hash, path },
    hash_algorithm,
  )
  .unwrap()
}

fn scope_page(hash_algorithm: HashAlgorithm, owner_id: &[u8], generation: u64, records: &[Vec<u8>]) -> Vec<u8> {
  let record_slices = records.iter().map(Vec::as_slice).collect::<Vec<_>>();
  encode_ordered_page(&OrderedPageWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::ScopeOrdinal,
    owner_id,
    generation,
    page_id: 0,
    previous_page_id: 0,
    next_page_id: 0,
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

#[test]
fn nonallocating_order_key_lengths_match_contiguous_and_posting_materialization() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  for (role, encoded) in [
    (OrderedIndexRoleV1::ScopeOrdinal, scope_document_record(hash_algorithm, 7, "/docs/readme.md")),
    (OrderedIndexRoleV1::Posting, posting_record(17, 7, 64, false)),
  ] {
    let decoded = decode_ordered_record(&encoded, hash_algorithm, role).unwrap();
    assert_eq!(checked_ordered_record_order_key_length(&decoded).unwrap(), ordered_record_order_key(&decoded).unwrap().len());
  }
}

fn leaf_directory(
  hash_algorithm: HashAlgorithm,
  owner_id: &[u8],
  generation: u64,
  pages: &[&[u8]],
  physical_hint: PhysicalHintV1,
) -> Vec<u8> {
  leaf_directory_for_role(hash_algorithm, OrderedIndexRoleV1::Posting, owner_id, generation, pages, physical_hint)
}

fn leaf_directory_for_role(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
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
  encode_artifact_directory(&ArtifactDirectoryWriteV1 { hash_algorithm, role, owner_id, generation, level: 0, entries: &entries })
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
fn cow_batch_applies_sorted_mutations_once_and_relinks_a_split_neighbor() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let owner_id = owner(hash_algorithm);
    let source_records = vec![posting_record(10, 10, 900, false), posting_record(50, 50, 900, false)];
    let source = posting_page(hash_algorithm, &owner_id, 7, 11, 0, 12, &source_records);
    let next_records = vec![posting_record(90, 90, 64, false)];
    let next = posting_page(hash_algorithm, &owner_id, 7, 12, 11, 0, &next_records);
    let inserted = [posting_record(20, 20, 900, false), posting_record(30, 30, 900, false), posting_record(40, 40, 900, false)];
    let mutations = inserted.iter().map(|record| OrderedPageMutationKindV1::UpsertLive(record)).collect::<Vec<_>>();
    let layout = aeordb::engine::v4::index_copy_on_write::IndexPageLayoutV1 {
      target_bytes: 4 * 1_024,
      split_above_bytes: 5 * 1_024,
      merge_below_bytes: 1_024,
      ..default_index_page_layout_v1()
    };

    let plan = mutate_ordered_page_batch_v1(&OrderedPageBatchMutationRequestV1 {
      hash_algorithm,
      source_page: &source,
      next_posting_page: Some(&next),
      generation: 8,
      next_page_id: 13,
      mutations: &mutations,
      layout,
    })
    .unwrap();

    assert_eq!(plan.replacements.len(), 2);
    let emitted = &plan.replacements[0].artifacts;
    assert!(emitted.len() > 1);
    assert_eq!(plan.allocated_page_ids.len(), emitted.len() - 1);
    assert_eq!(plan.next_page_id, 13 + u64::try_from(plan.allocated_page_ids.len()).unwrap());
    assert_eq!(
      emitted.iter().flat_map(|artifact| decoded_coordinates(&artifact.value, hash_algorithm)).collect::<Vec<_>>(),
      vec![(10, false), (20, false), (30, false), (40, false), (50, false)]
    );
    let rewritten_next = decode_ordered_page(&plan.replacements[1].artifacts[0].value, hash_algorithm).unwrap();
    assert_eq!(rewritten_next.page_id, 12);
    let last_emitted = decode_ordered_page(&emitted.last().unwrap().value, hash_algorithm).unwrap();
    assert_eq!(rewritten_next.previous_page_id, last_emitted.page_id);
    validate_posting_page_link(&last_emitted, &rewritten_next, hash_algorithm).unwrap();
    assert_eq!(source, posting_page(hash_algorithm, &owner_id, 7, 11, 0, 12, &source_records));
    assert_eq!(next, posting_page(hash_algorithm, &owner_id, 7, 12, 11, 0, &next_records));
  }
}

#[test]
fn cow_batch_rejects_empty_unsorted_duplicate_and_missing_tombstone_inputs() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let source = posting_page(hash_algorithm, &owner_id, 4, 1, 0, 0, &[posting_record(10, 10, 32, false)]);
  let high = posting_record(30, 30, 32, false);
  let low = posting_record(20, 20, 32, false);
  let duplicate = posting_record(20, 20, 32, true);
  let missing = posting_record(99, 99, 32, true);
  let empty = [];
  let unordered = [OrderedPageMutationKindV1::UpsertLive(&high), OrderedPageMutationKindV1::UpsertLive(&low)];
  let repeated = [OrderedPageMutationKindV1::UpsertLive(&low), OrderedPageMutationKindV1::TombstoneExisting(&duplicate)];
  let missing_tombstone = [OrderedPageMutationKindV1::TombstoneExisting(&missing)];
  let request = |mutations| OrderedPageBatchMutationRequestV1 {
    hash_algorithm,
    source_page: source.as_slice(),
    next_posting_page: None,
    generation: 5,
    next_page_id: 2,
    mutations,
    layout: default_index_page_layout_v1(),
  };

  assert_eq!(mutate_ordered_page_batch_v1(&request(&empty)).unwrap_err().code(), "index_cow_batch_empty");
  assert_eq!(mutate_ordered_page_batch_v1(&request(&unordered)).unwrap_err().code(), "index_cow_batch_order");
  assert_eq!(mutate_ordered_page_batch_v1(&request(&repeated)).unwrap_err().code(), "index_cow_batch_order");
  assert_eq!(mutate_ordered_page_batch_v1(&request(&missing_tombstone)).unwrap_err().code(), "index_cow_tombstone_missing");
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
  assert_eq!(plan.root_key, Some(rewritten_root.key.clone()));
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
    assert_eq!(plan.root_key, Some(root.key));
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
  assert_eq!(plan.root_key, Some(plan.artifacts.last().unwrap().key.clone()));
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

#[test]
fn cow_merge_retains_the_lower_right_page_id_and_relinks_only_the_previous_neighbor() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let previous_page = posting_page(hash_algorithm, &owner_id, 7, 5, 0, 30, &[posting_record(1, 1, 16, false)]);
  let left_page = posting_page(hash_algorithm, &owner_id, 7, 30, 5, 10, &[posting_record(2, 2, 16, false)]);
  let right_page = posting_page(hash_algorithm, &owner_id, 7, 10, 30, 40, &[posting_record(3, 3, 16, false)]);
  let next_page = posting_page(hash_algorithm, &owner_id, 7, 40, 10, 0, &[posting_record(4, 4, 16, false)]);
  let source_pages = [left_page.as_slice(), right_page.as_slice()];

  let request = OrderedPageCompactionWindowRequestV1 {
    hash_algorithm,
    source_pages: &source_pages,
    previous_posting_page: Some(&previous_page),
    next_posting_page: Some(&next_page),
    generation: 8,
    next_page_id: 50,
    tombstone_drop_proof: None,
    layout: default_index_page_layout_v1(),
  };
  let plan = compact_ordered_page_window_v1(&request).unwrap();
  assert_eq!(compact_ordered_page_window_v1(&request).unwrap(), plan);

  assert_eq!(plan.next_page_id, 50);
  assert!(plan.allocated_page_ids.is_empty());
  assert_eq!(plan.retired_page_ids, vec![30]);
  assert_eq!(plan.replacements.len(), 3);
  let retained = plan.replacements.iter().find(|replacement| replacement.source_page_id == 10).unwrap();
  let retired = plan.replacements.iter().find(|replacement| replacement.source_page_id == 30).unwrap();
  let rewritten_previous = plan.replacements.iter().find(|replacement| replacement.source_page_id == 5).unwrap();
  assert!(retired.artifacts.is_empty());
  assert_eq!(decoded_coordinates(&retained.artifacts[0].value, hash_algorithm), vec![(2, false), (3, false)]);
  let retained_page = decode_ordered_page(&retained.artifacts[0].value, hash_algorithm).unwrap();
  assert_eq!((retained_page.page_id, retained_page.previous_page_id, retained_page.next_page_id), (10, 5, 40));
  let previous = decode_ordered_page(&rewritten_previous.artifacts[0].value, hash_algorithm).unwrap();
  assert_eq!((previous.previous_page_id, previous.next_page_id), (0, 10));
  assert!(!plan.replacements.iter().any(|replacement| replacement.source_page_id == 40));
}

#[test]
fn cow_merge_retains_the_lower_left_page_id_and_relinks_only_the_next_neighbor() {
  let hash_algorithm = HashAlgorithm::Sha512;
  let owner_id = owner(hash_algorithm);
  let previous_page = posting_page(hash_algorithm, &owner_id, 7, 5, 0, 10, &[posting_record(1, 1, 16, false)]);
  let left_page = posting_page(hash_algorithm, &owner_id, 7, 10, 5, 30, &[posting_record(2, 2, 16, false)]);
  let right_page = posting_page(hash_algorithm, &owner_id, 7, 30, 10, 40, &[posting_record(3, 3, 16, false)]);
  let next_page = posting_page(hash_algorithm, &owner_id, 7, 40, 30, 0, &[posting_record(4, 4, 16, false)]);
  let source_pages = [left_page.as_slice(), right_page.as_slice()];

  let plan = compact_ordered_page_window_v1(&OrderedPageCompactionWindowRequestV1 {
    hash_algorithm,
    source_pages: &source_pages,
    previous_posting_page: Some(&previous_page),
    next_posting_page: Some(&next_page),
    generation: 8,
    next_page_id: 50,
    tombstone_drop_proof: None,
    layout: default_index_page_layout_v1(),
  })
  .unwrap();

  assert_eq!(plan.retired_page_ids, vec![30]);
  let retained = plan.replacements.iter().find(|replacement| replacement.source_page_id == 10).unwrap();
  let rewritten_next = plan.replacements.iter().find(|replacement| replacement.source_page_id == 40).unwrap();
  let retained_page = decode_ordered_page(&retained.artifacts[0].value, hash_algorithm).unwrap();
  assert_eq!((retained_page.page_id, retained_page.previous_page_id, retained_page.next_page_id), (10, 5, 40));
  let next = decode_ordered_page(&rewritten_next.artifacts[0].value, hash_algorithm).unwrap();
  assert_eq!((next.previous_page_id, next.next_page_id), (10, 0));
  assert!(!plan.replacements.iter().any(|replacement| replacement.source_page_id == 5));
}

#[test]
fn cow_compaction_preserves_tombstones_without_proof_and_drops_only_an_exact_proven_set() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let left_page = posting_page(hash_algorithm, &owner_id, 7, 10, 0, 30, &[posting_record(1, 1, 16, true), posting_record(2, 2, 16, false)]);
  let right_page = posting_page(hash_algorithm, &owner_id, 7, 30, 10, 0, &[posting_record(3, 3, 16, false)]);
  let source_pages = [left_page.as_slice(), right_page.as_slice()];
  let retained_plan = compact_ordered_page_window_v1(&OrderedPageCompactionWindowRequestV1 {
    hash_algorithm,
    source_pages: &source_pages,
    previous_posting_page: None,
    next_posting_page: None,
    generation: 8,
    next_page_id: 40,
    tombstone_drop_proof: None,
    layout: default_index_page_layout_v1(),
  })
  .unwrap();
  let retained_page = retained_plan.replacements.iter().find(|replacement| !replacement.artifacts.is_empty()).unwrap();
  assert_eq!(decoded_coordinates(&retained_page.artifacts[0].value, hash_algorithm), vec![(1, true), (2, false), (3, false)]);

  let left = decode_ordered_page(&left_page, hash_algorithm).unwrap();
  let right = decode_ordered_page(&right_page, hash_algorithm).unwrap();
  let proof_page_keys = [left.key.as_slice(), right.key.as_slice()];
  let proof = TombstoneDropProofV1 {
    owner_id: &owner_id,
    source_page_keys: &proof_page_keys,
    coverage_epoch_id: 9,
    covered_through_sequence: 100,
    journal_contiguous_through_sequence: 100,
    pin_safe_through_generation: 7,
  };
  let compacted_plan = compact_ordered_page_window_v1(&OrderedPageCompactionWindowRequestV1 {
    tombstone_drop_proof: Some(&proof),
    ..OrderedPageCompactionWindowRequestV1 {
      hash_algorithm,
      source_pages: &source_pages,
      previous_posting_page: None,
      next_posting_page: None,
      generation: 8,
      next_page_id: 40,
      tombstone_drop_proof: None,
      layout: default_index_page_layout_v1(),
    }
  })
  .unwrap();
  let compacted_page = compacted_plan.replacements.iter().find(|replacement| !replacement.artifacts.is_empty()).unwrap();
  assert_eq!(decoded_coordinates(&compacted_page.artifacts[0].value, hash_algorithm), vec![(2, false), (3, false)]);
}

#[test]
fn cow_compaction_rejects_detached_or_incomplete_tombstone_proof_before_output() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let left_page = posting_page(hash_algorithm, &owner_id, 7, 10, 0, 30, &[posting_record(1, 1, 16, true)]);
  let right_page = posting_page(hash_algorithm, &owner_id, 7, 30, 10, 0, &[posting_record(2, 2, 16, false)]);
  let source_pages = [left_page.as_slice(), right_page.as_slice()];
  let left = decode_ordered_page(&left_page, hash_algorithm).unwrap();
  let right = decode_ordered_page(&right_page, hash_algorithm).unwrap();
  let reversed_page_keys = [right.key.as_slice(), left.key.as_slice()];
  let detached = TombstoneDropProofV1 {
    owner_id: &owner_id,
    source_page_keys: &reversed_page_keys,
    coverage_epoch_id: 9,
    covered_through_sequence: 100,
    journal_contiguous_through_sequence: 99,
    pin_safe_through_generation: 6,
  };

  let error = compact_ordered_page_window_v1(&OrderedPageCompactionWindowRequestV1 {
    hash_algorithm,
    source_pages: &source_pages,
    previous_posting_page: None,
    next_posting_page: None,
    generation: 8,
    next_page_id: 40,
    tombstone_drop_proof: Some(&detached),
    layout: default_index_page_layout_v1(),
  })
  .unwrap_err();
  assert_eq!(error.code(), "index_cow_tombstone_proof_pages");
  assert_eq!(error.class(), MalformedInputClass::CrossRecordClosureMismatch);

  let exact_page_keys = [left.key.as_slice(), right.key.as_slice()];
  let other_owner = vec![0xabu8; hash_algorithm.hash_length()];
  let proofs = [
    (
      TombstoneDropProofV1 {
        owner_id: &other_owner,
        source_page_keys: &exact_page_keys,
        coverage_epoch_id: 9,
        covered_through_sequence: 100,
        journal_contiguous_through_sequence: 100,
        pin_safe_through_generation: 7,
      },
      "index_cow_tombstone_proof_owner",
    ),
    (
      TombstoneDropProofV1 {
        owner_id: &owner_id,
        source_page_keys: &exact_page_keys,
        coverage_epoch_id: 0,
        covered_through_sequence: 100,
        journal_contiguous_through_sequence: 100,
        pin_safe_through_generation: 7,
      },
      "index_cow_tombstone_proof_coverage",
    ),
    (
      TombstoneDropProofV1 {
        owner_id: &owner_id,
        source_page_keys: &exact_page_keys,
        coverage_epoch_id: 9,
        covered_through_sequence: 100,
        journal_contiguous_through_sequence: 99,
        pin_safe_through_generation: 7,
      },
      "index_cow_tombstone_proof_coverage",
    ),
    (
      TombstoneDropProofV1 {
        owner_id: &owner_id,
        source_page_keys: &exact_page_keys,
        coverage_epoch_id: 9,
        covered_through_sequence: 100,
        journal_contiguous_through_sequence: 100,
        pin_safe_through_generation: 6,
      },
      "index_cow_tombstone_proof_pins",
    ),
    (
      TombstoneDropProofV1 {
        owner_id: &owner_id,
        source_page_keys: &exact_page_keys,
        coverage_epoch_id: 9,
        covered_through_sequence: 100,
        journal_contiguous_through_sequence: 100,
        pin_safe_through_generation: 8,
      },
      "index_cow_tombstone_proof_pins",
    ),
  ];
  for (proof, expected_code) in proofs {
    let error = compact_ordered_page_window_v1(&OrderedPageCompactionWindowRequestV1 {
      hash_algorithm,
      source_pages: &source_pages,
      previous_posting_page: None,
      next_posting_page: None,
      generation: 8,
      next_page_id: 40,
      tombstone_drop_proof: Some(&proof),
      layout: default_index_page_layout_v1(),
    })
    .unwrap_err();
    assert_eq!(error.code(), expected_code);
  }
}

#[test]
fn cow_compaction_can_retire_a_fully_proven_tombstone_window_and_its_directory_root() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let source_page = posting_page(hash_algorithm, &owner_id, 7, 10, 0, 0, &[posting_record(1, 1, 16, true)]);
  let source = decode_ordered_page(&source_page, hash_algorithm).unwrap();
  let proof_page_keys = [source.key.as_slice()];
  let source_pages = [source_page.as_slice()];
  let proof = TombstoneDropProofV1 {
    owner_id: &owner_id,
    source_page_keys: &proof_page_keys,
    coverage_epoch_id: 9,
    covered_through_sequence: 100,
    journal_contiguous_through_sequence: 100,
    pin_safe_through_generation: 7,
  };
  let page_plan = compact_ordered_page_window_v1(&OrderedPageCompactionWindowRequestV1 {
    hash_algorithm,
    source_pages: &source_pages,
    previous_posting_page: None,
    next_posting_page: None,
    generation: 8,
    next_page_id: 40,
    tombstone_drop_proof: Some(&proof),
    layout: default_index_page_layout_v1(),
  })
  .unwrap();
  assert_eq!(page_plan.retired_page_ids, vec![10]);
  assert!(page_plan.replacements[0].artifacts.is_empty());

  let source_root =
    leaf_directory(hash_algorithm, &owner_id, 7, &[&source_page], PhysicalHintV1 { wal_offset: 5, total_length: 6, write_sequence: 7 });
  let path_nodes = [&source_root[..]];
  let paths = [ArtifactDirectoryPathV1 { source_page_key: &source.key, directories: &path_nodes }];
  let directory_plan = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
    hash_algorithm,
    generation: 8,
    page_plan: &page_plan,
    paths: &paths,
    layout: default_index_directory_layout_v1(),
  })
  .unwrap();
  assert_eq!(directory_plan.root_key, None);
  assert_eq!((directory_plan.live_count, directory_plan.tombstone_count, directory_plan.page_count), (0, 0, 0));
  assert!(directory_plan.artifacts.is_empty());
  let summary = validate_index_copy_on_write_closure_v1(&IndexCopyOnWriteClosureRequestV1 {
    hash_algorithm,
    generation: 8,
    initial_next_page_id: 40,
    source_pages: &source_pages,
    paths: &paths,
    page_plan: &page_plan,
    directory_plan: &directory_plan,
    page_layout: default_index_page_layout_v1(),
    directory_layout: default_index_directory_layout_v1(),
  })
  .unwrap();
  assert_eq!(summary.root_key, None);
  assert_eq!((summary.live_count, summary.tombstone_count, summary.page_count), (0, 0, 0));
}

#[test]
fn cow_compaction_returns_a_noop_for_a_pair_outside_the_local_merge_window() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let left_page = posting_page(hash_algorithm, &owner_id, 7, 10, 0, 30, &[posting_record(1, 1, 20_000, false)]);
  let right_page = posting_page(hash_algorithm, &owner_id, 7, 30, 10, 0, &[posting_record(2, 2, 20_000, false)]);
  let source_pages = [left_page.as_slice(), right_page.as_slice()];
  let plan = compact_ordered_page_window_v1(&OrderedPageCompactionWindowRequestV1 {
    hash_algorithm,
    source_pages: &source_pages,
    previous_posting_page: None,
    next_posting_page: None,
    generation: 8,
    next_page_id: 40,
    tombstone_drop_proof: None,
    layout: default_index_page_layout_v1(),
  })
  .unwrap();
  assert!(plan.is_unchanged());
}

#[test]
fn cow_compaction_rejects_malformed_windows_and_missing_required_neighbors() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let previous_page = posting_page(hash_algorithm, &owner_id, 7, 5, 0, 30, &[posting_record(1, 1, 16, false)]);
  let left_page = posting_page(hash_algorithm, &owner_id, 7, 30, 5, 10, &[posting_record(2, 2, 16, false)]);
  let right_page = posting_page(hash_algorithm, &owner_id, 7, 10, 30, 0, &[posting_record(3, 3, 16, false)]);
  let source_pages = [left_page.as_slice(), right_page.as_slice()];

  let missing_previous = compact_ordered_page_window_v1(&OrderedPageCompactionWindowRequestV1 {
    hash_algorithm,
    source_pages: &source_pages,
    previous_posting_page: None,
    next_posting_page: None,
    generation: 8,
    next_page_id: 40,
    tombstone_drop_proof: None,
    layout: default_index_page_layout_v1(),
  })
  .unwrap_err();
  assert_eq!(missing_previous.code(), "index_cow_compaction_previous_missing");

  let left_low_page = posting_page(hash_algorithm, &owner_id, 7, 10, 0, 30, &[posting_record(2, 2, 16, false)]);
  let right_high_page = posting_page(hash_algorithm, &owner_id, 7, 30, 10, 40, &[posting_record(3, 3, 16, false)]);
  let low_high_sources = [left_low_page.as_slice(), right_high_page.as_slice()];
  let missing_next = compact_ordered_page_window_v1(&OrderedPageCompactionWindowRequestV1 {
    hash_algorithm,
    source_pages: &low_high_sources,
    previous_posting_page: None,
    next_posting_page: None,
    generation: 8,
    next_page_id: 50,
    tombstone_drop_proof: None,
    layout: default_index_page_layout_v1(),
  })
  .unwrap_err();
  assert_eq!(missing_next.code(), "index_cow_compaction_next_missing");

  let no_pages: [&[u8]; 0] = [];
  let empty = compact_ordered_page_window_v1(&OrderedPageCompactionWindowRequestV1 {
    source_pages: &no_pages,
    ..OrderedPageCompactionWindowRequestV1 {
      hash_algorithm,
      source_pages: &source_pages,
      previous_posting_page: Some(&previous_page),
      next_posting_page: None,
      generation: 8,
      next_page_id: 40,
      tombstone_drop_proof: None,
      layout: default_index_page_layout_v1(),
    }
  })
  .unwrap_err();
  assert_eq!(empty.code(), "index_cow_compaction_window");

  let three_pages = [previous_page.as_slice(), left_page.as_slice(), right_page.as_slice()];
  let oversized = compact_ordered_page_window_v1(&OrderedPageCompactionWindowRequestV1 {
    source_pages: &three_pages,
    ..OrderedPageCompactionWindowRequestV1 {
      hash_algorithm,
      source_pages: &source_pages,
      previous_posting_page: Some(&previous_page),
      next_posting_page: None,
      generation: 8,
      next_page_id: 40,
      tombstone_drop_proof: None,
      layout: default_index_page_layout_v1(),
    }
  })
  .unwrap_err();
  assert_eq!(oversized.code(), "index_cow_compaction_window");

  let stale_generation = compact_ordered_page_window_v1(&OrderedPageCompactionWindowRequestV1 {
    generation: 7,
    previous_posting_page: Some(&previous_page),
    ..OrderedPageCompactionWindowRequestV1 {
      hash_algorithm,
      source_pages: &source_pages,
      previous_posting_page: None,
      next_posting_page: None,
      generation: 8,
      next_page_id: 40,
      tombstone_drop_proof: None,
      layout: default_index_page_layout_v1(),
    }
  })
  .unwrap_err();
  assert_eq!(stale_generation.code(), "index_cow_compaction_generation");

  let stale_high_water = compact_ordered_page_window_v1(&OrderedPageCompactionWindowRequestV1 {
    next_page_id: 30,
    previous_posting_page: Some(&previous_page),
    ..OrderedPageCompactionWindowRequestV1 {
      hash_algorithm,
      source_pages: &source_pages,
      previous_posting_page: None,
      next_posting_page: None,
      generation: 8,
      next_page_id: 40,
      tombstone_drop_proof: None,
      layout: default_index_page_layout_v1(),
    }
  })
  .unwrap_err();
  assert_eq!(stale_high_water.code(), "index_cow_compaction_page_id_high_water");

  let mut corrupt_left_page = left_page.clone();
  *corrupt_left_page.last_mut().unwrap() ^= 0x80;
  let corrupt_sources = [corrupt_left_page.as_slice(), right_page.as_slice()];
  let corrupt = compact_ordered_page_window_v1(&OrderedPageCompactionWindowRequestV1 {
    source_pages: &corrupt_sources,
    previous_posting_page: Some(&previous_page),
    ..OrderedPageCompactionWindowRequestV1 {
      hash_algorithm,
      source_pages: &source_pages,
      previous_posting_page: None,
      next_posting_page: None,
      generation: 8,
      next_page_id: 40,
      tombstone_drop_proof: None,
      layout: default_index_page_layout_v1(),
    }
  })
  .unwrap_err();
  assert_eq!(corrupt.class(), MalformedInputClass::ChecksumOrIntegrityMismatch);
}

#[test]
fn cow_compaction_retires_an_empty_middle_window_relinks_both_neighbors_and_rewrites_four_directory_paths() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let previous_page = posting_page(hash_algorithm, &owner_id, 7, 5, 0, 10, &[posting_record(1, 1, 16, false)]);
  let left_page = posting_page(hash_algorithm, &owner_id, 7, 10, 5, 30, &[posting_record(2, 2, 16, true)]);
  let right_page = posting_page(hash_algorithm, &owner_id, 7, 30, 10, 40, &[posting_record(3, 3, 16, true)]);
  let next_page = posting_page(hash_algorithm, &owner_id, 7, 40, 30, 0, &[posting_record(4, 4, 16, false)]);
  let left = decode_ordered_page(&left_page, hash_algorithm).unwrap();
  let right = decode_ordered_page(&right_page, hash_algorithm).unwrap();
  let proof_page_keys = [left.key.as_slice(), right.key.as_slice()];
  let source_pages = [left_page.as_slice(), right_page.as_slice()];
  let proof = TombstoneDropProofV1 {
    owner_id: &owner_id,
    source_page_keys: &proof_page_keys,
    coverage_epoch_id: 9,
    covered_through_sequence: 100,
    journal_contiguous_through_sequence: 100,
    pin_safe_through_generation: 7,
  };
  let page_plan = compact_ordered_page_window_v1(&OrderedPageCompactionWindowRequestV1 {
    hash_algorithm,
    source_pages: &source_pages,
    previous_posting_page: Some(&previous_page),
    next_posting_page: Some(&next_page),
    generation: 8,
    next_page_id: 50,
    tombstone_drop_proof: Some(&proof),
    layout: default_index_page_layout_v1(),
  })
  .unwrap();
  assert_eq!(page_plan.retired_page_ids, vec![10, 30]);
  assert_eq!(page_plan.replacements.len(), 4);
  let rewritten_previous = page_plan.replacements.iter().find(|replacement| replacement.source_page_id == 5).unwrap();
  let rewritten_next = page_plan.replacements.iter().find(|replacement| replacement.source_page_id == 40).unwrap();
  let previous = decode_ordered_page(&rewritten_previous.artifacts[0].value, hash_algorithm).unwrap();
  let next = decode_ordered_page(&rewritten_next.artifacts[0].value, hash_algorithm).unwrap();
  assert_eq!((previous.previous_page_id, previous.next_page_id), (0, 40));
  assert_eq!((next.previous_page_id, next.next_page_id), (5, 0));

  let source_root = leaf_directory(
    hash_algorithm,
    &owner_id,
    7,
    &[&previous_page, &left_page, &right_page, &next_page],
    PhysicalHintV1 { wal_offset: 5, total_length: 6, write_sequence: 7 },
  );
  let previous = decode_ordered_page(&previous_page, hash_algorithm).unwrap();
  let next = decode_ordered_page(&next_page, hash_algorithm).unwrap();
  let path_nodes = [&source_root[..]];
  let paths = [
    ArtifactDirectoryPathV1 { source_page_key: &left.key, directories: &path_nodes },
    ArtifactDirectoryPathV1 { source_page_key: &right.key, directories: &path_nodes },
    ArtifactDirectoryPathV1 { source_page_key: &previous.key, directories: &path_nodes },
    ArtifactDirectoryPathV1 { source_page_key: &next.key, directories: &path_nodes },
  ];
  let directory_plan = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
    hash_algorithm,
    generation: 8,
    page_plan: &page_plan,
    paths: &paths,
    layout: default_index_directory_layout_v1(),
  })
  .unwrap();
  assert_eq!((directory_plan.live_count, directory_plan.tombstone_count, directory_plan.page_count), (2, 0, 2));
  let root = decode_artifact_directory(directory_plan.artifacts.last().unwrap().value.as_slice(), hash_algorithm).unwrap();
  assert_eq!(directory_plan.root_key, Some(root.key.clone()));
  assert_eq!(root.entries.len(), 2);
}

#[test]
fn cow_compaction_counts_retained_source_summaries_in_its_peak_workspace() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let previous_page = posting_page(hash_algorithm, &owner_id, 7, 5, 0, 10, &[posting_record(1, 1, 900_000, false)]);
  let left_page = posting_page(hash_algorithm, &owner_id, 7, 10, 5, 30, &[posting_record(2, 2, 16, true)]);
  let right_page = posting_page(hash_algorithm, &owner_id, 7, 30, 10, 40, &[posting_record(3, 3, 16, true)]);
  let next_page = posting_page(hash_algorithm, &owner_id, 7, 40, 30, 0, &[posting_record(4, 4, 900_000, false)]);
  let left = decode_ordered_page(&left_page, hash_algorithm).unwrap();
  let right = decode_ordered_page(&right_page, hash_algorithm).unwrap();
  let proof_page_keys = [left.key.as_slice(), right.key.as_slice()];
  let source_pages = [left_page.as_slice(), right_page.as_slice()];
  let proof = TombstoneDropProofV1 {
    owner_id: &owner_id,
    source_page_keys: &proof_page_keys,
    coverage_epoch_id: 9,
    covered_through_sequence: 100,
    journal_contiguous_through_sequence: 100,
    pin_safe_through_generation: 7,
  };
  let layout = aeordb::engine::v4::index_copy_on_write::IndexPageLayoutV1 {
    maximum_workspace_bytes: 8 * 1_024 * 1_024,
    ..default_index_page_layout_v1()
  };

  let error = compact_ordered_page_window_v1(&OrderedPageCompactionWindowRequestV1 {
    hash_algorithm,
    source_pages: &source_pages,
    previous_posting_page: Some(&previous_page),
    next_posting_page: Some(&next_page),
    generation: 8,
    next_page_id: 50,
    tombstone_drop_proof: Some(&proof),
    layout,
  })
  .unwrap_err();
  assert_eq!(error.code(), "index_cow_compaction_workspace_exceeded");
  assert_eq!(error.class(), MalformedInputClass::AllocationAmplification);
}

#[test]
fn cow_whole_plan_validator_closes_source_pages_directory_and_root_summary() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let owner_id = owner(hash_algorithm);
    let source_records = vec![posting_record(1, 1, 16, false), posting_record(3, 3, 16, false)];
    let source_page = posting_page(hash_algorithm, &owner_id, 7, 10, 0, 0, &source_records);
    let source_root = leaf_directory(
      hash_algorithm,
      &owner_id,
      7,
      &[&source_page],
      PhysicalHintV1 { wal_offset: 400, total_length: 500, write_sequence: 6 },
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
    let source_key = decode_ordered_page(&source_page, hash_algorithm).unwrap().key;
    let directories = [&source_root[..]];
    let paths = [ArtifactDirectoryPathV1 { source_page_key: &source_key, directories: &directories }];
    let directory_plan = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
      hash_algorithm,
      generation: 8,
      page_plan: &page_plan,
      paths: &paths,
      layout: default_index_directory_layout_v1(),
    })
    .unwrap();
    let source_pages = [&source_page[..]];
    let request = IndexCopyOnWriteClosureRequestV1 {
      hash_algorithm,
      generation: 8,
      initial_next_page_id: 40,
      source_pages: &source_pages,
      paths: &paths,
      page_plan: &page_plan,
      directory_plan: &directory_plan,
      page_layout: default_index_page_layout_v1(),
      directory_layout: default_index_directory_layout_v1(),
    };

    let summary = validate_index_copy_on_write_closure_v1(&request).unwrap();
    assert_eq!(summary.owner_id, owner_id);
    assert_eq!(summary.role, OrderedIndexRoleV1::Posting);
    assert_eq!(summary.generation, 8);
    assert_eq!(summary.root_key, directory_plan.root_key);
    assert_eq!(summary.live_count, 3);
    assert_eq!(summary.tombstone_count, 0);
    assert_eq!(summary.page_count, 1);
    assert_eq!(summary.next_page_id, 40);
    assert_eq!(summary.page_artifact_count, 1);
    assert_eq!(summary.directory_artifact_count, 1);
    assert_eq!(summary.source_page_bytes, source_page.len());
    assert_eq!(summary.directory_path_bytes, source_root.len());
    assert_eq!(summary.page_artifact_bytes, page_plan.replacements[0].artifacts[0].value.len());
    assert_eq!(summary.directory_artifact_bytes, directory_plan.artifacts[0].value.len());
    assert_eq!(
      summary.retained_encoded_bytes,
      summary.source_page_bytes + summary.directory_path_bytes + summary.page_artifact_bytes + summary.directory_artifact_bytes
    );
  }
}

#[test]
fn cow_whole_plan_validator_closes_scope_pages_without_page_id_state() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let owner_id = owner(hash_algorithm);
    let source_page = scope_page(
      hash_algorithm,
      &owner_id,
      7,
      &[scope_document_record(hash_algorithm, 1, "/a"), scope_document_record(hash_algorithm, 3, "/c")],
    );
    let source_root = leaf_directory_for_role(
      hash_algorithm,
      OrderedIndexRoleV1::ScopeOrdinal,
      &owner_id,
      7,
      &[&source_page],
      PhysicalHintV1 { wal_offset: 400, total_length: 500, write_sequence: 6 },
    );
    let inserted = scope_document_record(hash_algorithm, 2, "/b");
    let page_plan = mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
      hash_algorithm,
      source_page: &source_page,
      next_posting_page: None,
      generation: 8,
      next_page_id: 0,
      mutation: OrderedPageMutationKindV1::UpsertLive(&inserted),
      layout: default_index_page_layout_v1(),
    })
    .unwrap();
    assert_eq!(page_plan.next_page_id, 0);
    assert!(page_plan.allocated_page_ids.is_empty());
    assert!(page_plan.retired_page_ids.is_empty());

    let source_key = decode_ordered_page(&source_page, hash_algorithm).unwrap().key;
    let directories = [&source_root[..]];
    let paths = [ArtifactDirectoryPathV1 { source_page_key: &source_key, directories: &directories }];
    let directory_plan = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
      hash_algorithm,
      generation: 8,
      page_plan: &page_plan,
      paths: &paths,
      layout: default_index_directory_layout_v1(),
    })
    .unwrap();
    let source_pages = [&source_page[..]];
    let summary = validate_index_copy_on_write_closure_v1(&IndexCopyOnWriteClosureRequestV1 {
      hash_algorithm,
      generation: 8,
      initial_next_page_id: 0,
      source_pages: &source_pages,
      paths: &paths,
      page_plan: &page_plan,
      directory_plan: &directory_plan,
      page_layout: default_index_page_layout_v1(),
      directory_layout: default_index_directory_layout_v1(),
    })
    .unwrap();

    assert_eq!(summary.owner_id, owner_id);
    assert_eq!(summary.role, OrderedIndexRoleV1::ScopeOrdinal);
    assert_eq!(summary.live_count, 3);
    assert_eq!(summary.tombstone_count, 0);
    assert_eq!(summary.page_count, 1);
    assert_eq!(summary.minimum_page_id, 0);
    assert_eq!(summary.maximum_page_id, 0);
    assert_eq!(summary.next_page_id, 0);
  }
}

#[test]
fn cow_whole_plan_validator_rejects_mutated_page_and_root_authority() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let source_page =
    posting_page(hash_algorithm, &owner_id, 7, 10, 0, 0, &[posting_record(1, 1, 16, false), posting_record(3, 3, 16, false)]);
  let source_root =
    leaf_directory(hash_algorithm, &owner_id, 7, &[&source_page], PhysicalHintV1 { wal_offset: 400, total_length: 500, write_sequence: 6 });
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
  let source_key = decode_ordered_page(&source_page, hash_algorithm).unwrap().key;
  let path_nodes = [&source_root[..]];
  let paths = [ArtifactDirectoryPathV1 { source_page_key: &source_key, directories: &path_nodes }];
  let directory_plan = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
    hash_algorithm,
    generation: 8,
    page_plan: &page_plan,
    paths: &paths,
    layout: default_index_directory_layout_v1(),
  })
  .unwrap();
  let source_pages = [&source_page[..]];
  let validate = |candidate_page_plan, candidate_directory_plan| {
    validate_index_copy_on_write_closure_v1(&IndexCopyOnWriteClosureRequestV1 {
      hash_algorithm,
      generation: 8,
      initial_next_page_id: 40,
      source_pages: &source_pages,
      paths: &paths,
      page_plan: candidate_page_plan,
      directory_plan: candidate_directory_plan,
      page_layout: default_index_page_layout_v1(),
      directory_layout: default_index_directory_layout_v1(),
    })
  };

  let mut missing_allocation = page_plan.clone();
  missing_allocation.next_page_id = 41;
  assert_eq!(validate(&missing_allocation, &directory_plan).unwrap_err().code(), "index_cow_closure_allocation_range");

  let mut forged_retirement = page_plan.clone();
  forged_retirement.retired_page_ids.push(10);
  assert_eq!(validate(&forged_retirement, &directory_plan).unwrap_err().code(), "index_cow_closure_source_retirement");

  let mut forged_page_key = page_plan.clone();
  forged_page_key.replacements[0].artifacts[0].key[0] ^= 0x80;
  assert_eq!(validate(&forged_page_key, &directory_plan).unwrap_err().code(), "index_cow_closure_page_output");

  let mut transferred_stable_id = page_plan.clone();
  let output = decode_ordered_page(&transferred_stable_id.replacements[0].artifacts[0].value, hash_algorithm).unwrap();
  let output_records = output.records.iter().map(|record| record.unwrap()).collect::<Vec<_>>();
  let output_record_bytes = output_records.iter().map(|record| record.encoded).collect::<Vec<_>>();
  transferred_stable_id.replacements[0].artifacts[0] = encode_ordered_page(&OrderedPageWriteV1 {
    hash_algorithm,
    role: output.role,
    owner_id: output.owner_id,
    generation: output.generation,
    page_id: 40,
    previous_page_id: 0,
    next_page_id: 0,
    records: &output_record_bytes,
  })
  .unwrap();
  transferred_stable_id.allocated_page_ids = vec![40];
  transferred_stable_id.next_page_id = 41;
  let transferred_directory = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
    hash_algorithm,
    generation: 8,
    page_plan: &transferred_stable_id,
    paths: &paths,
    layout: default_index_directory_layout_v1(),
  })
  .unwrap();
  assert_eq!(validate(&transferred_stable_id, &transferred_directory).unwrap_err().code(), "index_cow_closure_source_retirement");

  let mut forged_root_key = directory_plan.clone();
  forged_root_key.root_key.as_mut().unwrap()[0] ^= 0x80;
  assert_eq!(validate(&page_plan, &forged_root_key).unwrap_err().code(), "index_cow_closure_root_missing");

  let mut forged_root_count = directory_plan.clone();
  forged_root_count.live_count += 1;
  assert_eq!(validate(&page_plan, &forged_root_count).unwrap_err().code(), "index_cow_closure_root_summary");

  let mut duplicate_directory = directory_plan.clone();
  duplicate_directory.artifacts.push(duplicate_directory.artifacts[0].clone());
  assert_eq!(validate(&page_plan, &duplicate_directory).unwrap_err().code(), "index_cow_closure_directory_output");
}

#[test]
fn cow_whole_plan_validator_requires_dependency_order_for_rewritten_directories() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let source_page = posting_page(hash_algorithm, &owner_id, 7, 10, 0, 0, &[posting_record(1, 1, 16, false)]);
  let source_leaf =
    leaf_directory(hash_algorithm, &owner_id, 7, &[&source_page], PhysicalHintV1 { wal_offset: 1, total_length: 2, write_sequence: 3 });
  let source_root =
    internal_directory(hash_algorithm, &owner_id, 7, &[&source_leaf], PhysicalHintV1 { wal_offset: 4, total_length: 5, write_sequence: 6 });
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
  let source_key = decode_ordered_page(&source_page, hash_algorithm).unwrap().key;
  let path_nodes = [&source_root[..], &source_leaf[..]];
  let paths = [ArtifactDirectoryPathV1 { source_page_key: &source_key, directories: &path_nodes }];
  let directory_plan = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
    hash_algorithm,
    generation: 8,
    page_plan: &page_plan,
    paths: &paths,
    layout: default_index_directory_layout_v1(),
  })
  .unwrap();
  assert_eq!(directory_plan.artifacts.len(), 2);
  let source_pages = [&source_page[..]];
  validate_index_copy_on_write_closure_v1(&IndexCopyOnWriteClosureRequestV1 {
    hash_algorithm,
    generation: 8,
    initial_next_page_id: 40,
    source_pages: &source_pages,
    paths: &paths,
    page_plan: &page_plan,
    directory_plan: &directory_plan,
    page_layout: default_index_page_layout_v1(),
    directory_layout: default_index_directory_layout_v1(),
  })
  .unwrap();

  let mut reversed = directory_plan.clone();
  reversed.artifacts.reverse();
  let error = validate_index_copy_on_write_closure_v1(&IndexCopyOnWriteClosureRequestV1 {
    hash_algorithm,
    generation: 8,
    initial_next_page_id: 40,
    source_pages: &source_pages,
    paths: &paths,
    page_plan: &page_plan,
    directory_plan: &reversed,
    page_layout: default_index_page_layout_v1(),
    directory_layout: default_index_directory_layout_v1(),
  })
  .unwrap_err();
  assert_eq!(error.code(), "index_cow_closure_unknown_child");
}

#[test]
fn cow_whole_plan_validator_binds_source_order_and_outward_page_id_high_water() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let source_page = posting_page(hash_algorithm, &owner_id, 7, 10, 0, 30, &[posting_record(1, 1, 60_000, false)]);
  let next_page = posting_page(hash_algorithm, &owner_id, 7, 30, 10, 60, &[posting_record(100, 100, 16, false)]);
  let inserted = posting_record(2, 2, 60_000, false);
  let page_plan = mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
    hash_algorithm,
    source_page: &source_page,
    next_posting_page: Some(&next_page),
    generation: 8,
    next_page_id: 70,
    mutation: OrderedPageMutationKindV1::UpsertLive(&inserted),
    layout: default_index_page_layout_v1(),
  })
  .unwrap();
  assert_eq!(page_plan.replacements.len(), 2);
  let source_root = leaf_directory(
    hash_algorithm,
    &owner_id,
    7,
    &[&source_page, &next_page],
    PhysicalHintV1 { wal_offset: 1, total_length: 2, write_sequence: 3 },
  );
  let path_nodes = [&source_root[..]];
  let source_key = decode_ordered_page(&source_page, hash_algorithm).unwrap().key;
  let next_key = decode_ordered_page(&next_page, hash_algorithm).unwrap().key;
  let paths = [
    ArtifactDirectoryPathV1 { source_page_key: &source_key, directories: &path_nodes },
    ArtifactDirectoryPathV1 { source_page_key: &next_key, directories: &path_nodes },
  ];
  let directory_plan = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
    hash_algorithm,
    generation: 8,
    page_plan: &page_plan,
    paths: &paths,
    layout: default_index_directory_layout_v1(),
  })
  .unwrap();
  let source_pages = [source_page.as_slice(), next_page.as_slice()];
  let request = |source_pages: &[&[u8]], initial_next_page_id| {
    validate_index_copy_on_write_closure_v1(&IndexCopyOnWriteClosureRequestV1 {
      hash_algorithm,
      generation: 8,
      initial_next_page_id,
      source_pages,
      paths: &paths,
      page_plan: &page_plan,
      directory_plan: &directory_plan,
      page_layout: default_index_page_layout_v1(),
      directory_layout: default_index_directory_layout_v1(),
    })
  };
  request(&source_pages, 70).unwrap();

  let reversed_sources = [next_page.as_slice(), source_page.as_slice()];
  assert_eq!(request(&reversed_sources, 70).unwrap_err().code(), "index_cow_closure_source_order");
  assert_eq!(request(&source_pages, 50).unwrap_err().code(), "index_cow_closure_initial_page_id");
}

#[test]
fn directory_cow_rejects_aggregate_path_input_amplification_before_decoding() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let source_page = posting_page(hash_algorithm, &owner_id, 7, 10, 0, 0, &[posting_record(1, 1, 16, false)]);
  let inserted = posting_record(2, 2, 100_000, false);
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
  assert_eq!(page_plan.replacements.len(), 1);
  let oversized_directory = vec![0u8; 4 * 1_024 * 1_024 + 1];
  let path_nodes = [oversized_directory.as_slice(); 16];
  let source_key = decode_ordered_page(&source_page, hash_algorithm).unwrap().key;
  let paths = [ArtifactDirectoryPathV1 { source_page_key: &source_key, directories: &path_nodes }];

  let error = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
    hash_algorithm,
    generation: 8,
    page_plan: &page_plan,
    paths: &paths,
    layout: default_index_directory_layout_v1(),
  })
  .unwrap_err();
  assert_eq!(error.code(), "index_cow_directory_path_workspace");
  assert_eq!(error.class(), MalformedInputClass::AllocationAmplification);
}

fn property_page_layout() -> aeordb::engine::v4::index_copy_on_write::IndexPageLayoutV1 {
  aeordb::engine::v4::index_copy_on_write::IndexPageLayoutV1 {
    target_bytes: 4 * 1_024,
    split_above_bytes: 6 * 1_024,
    merge_below_bytes: 3_584,
    ..default_index_page_layout_v1()
  }
}

fn property_directory_layout() -> aeordb::engine::v4::index_copy_on_write::IndexDirectoryLayoutV1 {
  aeordb::engine::v4::index_copy_on_write::IndexDirectoryLayoutV1 { target_bytes: 4 * 1_024, ..default_index_directory_layout_v1() }
}

fn property_page_index(pages: &[Vec<u8>], hash_algorithm: HashAlgorithm, coordinate: u64) -> usize {
  pages
    .iter()
    .position(|page| decoded_coordinates(page, hash_algorithm).last().is_some_and(|record| record.0 >= coordinate))
    .unwrap_or(pages.len() - 1)
}

fn property_assert_model(pages: &[Vec<u8>], hash_algorithm: HashAlgorithm, expected_live: &std::collections::BTreeSet<u64>) {
  let mut observed_live = std::collections::BTreeSet::new();
  let mut previous_coordinate = None;
  for (index, bytes) in pages.iter().enumerate() {
    let page = decode_ordered_page(bytes, hash_algorithm).unwrap();
    assert_eq!(
      page.previous_page_id,
      index.checked_sub(1).map_or(0, |previous| { decode_ordered_page(&pages[previous], hash_algorithm).unwrap().page_id })
    );
    assert_eq!(page.next_page_id, pages.get(index + 1).map_or(0, |next| decode_ordered_page(next, hash_algorithm).unwrap().page_id));
    for (coordinate, tombstone) in decoded_coordinates(bytes, hash_algorithm) {
      assert!(previous_coordinate.is_none_or(|previous| previous < coordinate));
      previous_coordinate = Some(coordinate);
      if !tombstone {
        assert!(observed_live.insert(coordinate));
      }
    }
  }
  assert_eq!(&observed_live, expected_live);
}

fn property_apply_plan(
  hash_algorithm: HashAlgorithm,
  generation: u64,
  initial_next_page_id: u64,
  page_layout: aeordb::engine::v4::index_copy_on_write::IndexPageLayoutV1,
  page_plan: &aeordb::engine::v4::index_copy_on_write::OrderedPageMutationPlanV1,
  pages: &mut Vec<Vec<u8>>,
  directory_artifacts: &mut std::collections::BTreeMap<Vec<u8>, Vec<u8>>,
  root_key: &mut Vec<u8>,
) {
  let source_keys = page_plan.replacements.iter().map(|replacement| replacement.source_key.clone()).collect::<Vec<_>>();
  let source_pages = source_keys
    .iter()
    .map(|source_key| {
      pages.iter().find(|page| decode_ordered_page(page, hash_algorithm).unwrap().key == *source_key).map(Vec::as_slice).unwrap()
    })
    .collect::<Vec<_>>();
  let path_values = source_keys
    .iter()
    .map(|source_key| property_directory_path(hash_algorithm, directory_artifacts, root_key, source_key).unwrap())
    .collect::<Vec<_>>();
  let path_node_references = path_values.iter().map(|nodes| nodes.iter().map(Vec::as_slice).collect::<Vec<_>>()).collect::<Vec<_>>();
  let paths = source_keys
    .iter()
    .zip(&path_node_references)
    .map(|(source_key, directories)| ArtifactDirectoryPathV1 { source_page_key: source_key, directories })
    .collect::<Vec<_>>();
  let directory_layout = property_directory_layout();
  let directory_plan = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
    hash_algorithm,
    generation,
    page_plan,
    paths: &paths,
    layout: directory_layout,
  })
  .unwrap();
  let summary = validate_index_copy_on_write_closure_v1(&IndexCopyOnWriteClosureRequestV1 {
    hash_algorithm,
    generation,
    initial_next_page_id,
    source_pages: &source_pages,
    paths: &paths,
    page_plan,
    directory_plan: &directory_plan,
    page_layout,
    directory_layout,
  })
  .unwrap();
  assert_eq!(
    summary.page_count,
    u64::try_from(pages.len()).unwrap() + u64::try_from(page_plan.allocated_page_ids.len()).unwrap()
      - u64::try_from(page_plan.retired_page_ids.len()).unwrap()
  );

  let replacements = page_plan
    .replacements
    .iter()
    .map(|replacement| {
      (replacement.source_key.clone(), replacement.artifacts.iter().map(|artifact| artifact.value.clone()).collect::<Vec<_>>())
    })
    .collect::<std::collections::BTreeMap<_, _>>();
  let mut rewritten_pages = Vec::new();
  for page in pages.iter() {
    let key = decode_ordered_page(page, hash_algorithm).unwrap().key;
    if let Some(outputs) = replacements.get(&key) {
      rewritten_pages.extend(outputs.iter().cloned());
    } else {
      rewritten_pages.push(page.clone());
    }
  }
  rewritten_pages.sort_by_key(|page| decoded_coordinates(page, hash_algorithm)[0].0);
  *pages = rewritten_pages;
  for artifact in &directory_plan.artifacts {
    directory_artifacts.insert(artifact.key.clone(), artifact.value.clone());
  }
  *root_key = directory_plan.root_key.unwrap();
}

fn property_directory_path(
  hash_algorithm: HashAlgorithm,
  directory_artifacts: &std::collections::BTreeMap<Vec<u8>, Vec<u8>>,
  current_key: &[u8],
  source_page_key: &[u8],
) -> Option<Vec<Vec<u8>>> {
  let current = directory_artifacts.get(current_key)?;
  let directory = decode_artifact_directory(current, hash_algorithm).ok()?;
  if directory.level == 0 {
    if directory.entries.iter().any(|entry| entry.child_hash == source_page_key) {
      return Some(vec![current.clone()]);
    }
    return None;
  }
  for entry in &directory.entries {
    if let Some(mut suffix) = property_directory_path(hash_algorithm, directory_artifacts, entry.child_hash, source_page_key) {
      let mut path = Vec::with_capacity(suffix.len() + 1);
      path.push(current.clone());
      path.append(&mut suffix);
      return Some(path);
    }
  }
  None
}

fn property_next_random(state: &mut u64) -> u64 {
  *state ^= *state << 13;
  *state ^= *state >> 7;
  *state ^= *state << 17;
  *state
}

#[test]
fn cow_randomized_mutations_splits_compaction_and_merges_match_an_independent_model() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let owner_id = owner(hash_algorithm);
    let page_layout = property_page_layout();
    let mut generation = 2u64;
    let mut next_page_id = 2u64;
    let initial_record = posting_record(1, 1, 512, false);
    let initial_page = posting_page(hash_algorithm, &owner_id, 1, 1, 0, 0, &[initial_record]);
    let mut pages = vec![initial_page];
    let initial_root = leaf_directory(
      hash_algorithm,
      &owner_id,
      1,
      &[pages[0].as_slice()],
      PhysicalHintV1 { wal_offset: 1, total_length: 2, write_sequence: 3 },
    );
    let mut root_key = decode_artifact_directory(&initial_root, hash_algorithm).unwrap().key;
    let mut directory_artifacts = std::collections::BTreeMap::from([(root_key.clone(), initial_root)]);
    let mut expected_live = std::collections::BTreeSet::from([1u64]);
    let mut random_state = 0x9e37_79b9_7f4a_7c15u64 ^ u64::try_from(hash_algorithm.hash_length()).unwrap();
    let mut coordinates = (2u64..=56).collect::<Vec<_>>();
    for index in (1..coordinates.len()).rev() {
      let swap_index = usize::try_from(property_next_random(&mut random_state) % u64::try_from(index + 1).unwrap()).unwrap();
      coordinates.swap(index, swap_index);
    }
    let mut saw_split = false;
    for coordinate in coordinates {
      let page_index = property_page_index(&pages, hash_algorithm, coordinate);
      let inserted = posting_record(coordinate, coordinate, 512, false);
      let next_page = pages.get(page_index + 1).map(Vec::as_slice);
      let page_plan = mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
        hash_algorithm,
        source_page: &pages[page_index],
        next_posting_page: next_page,
        generation,
        next_page_id,
        mutation: OrderedPageMutationKindV1::UpsertLive(&inserted),
        layout: page_layout,
      })
      .unwrap();
      saw_split |= !page_plan.allocated_page_ids.is_empty();
      property_apply_plan(
        hash_algorithm,
        generation,
        next_page_id,
        page_layout,
        &page_plan,
        &mut pages,
        &mut directory_artifacts,
        &mut root_key,
      );
      next_page_id = page_plan.next_page_id;
      expected_live.insert(coordinate);
      property_assert_model(&pages, hash_algorithm, &expected_live);
      generation += 1;
    }
    assert!(saw_split);
    assert!(decode_artifact_directory(directory_artifacts.get(&root_key).unwrap(), hash_algorithm).unwrap().level > 0);

    for _ in 0..32 {
      let coordinate = 2 + property_next_random(&mut random_state) % 127;
      let delete = property_next_random(&mut random_state) & 3 == 0 && expected_live.contains(&coordinate);
      let page_index = property_page_index(&pages, hash_algorithm, coordinate);
      let record = posting_record(coordinate, coordinate, 512, delete);
      let page_plan = mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
        hash_algorithm,
        source_page: &pages[page_index],
        next_posting_page: pages.get(page_index + 1).map(Vec::as_slice),
        generation,
        next_page_id,
        mutation: if delete {
          OrderedPageMutationKindV1::TombstoneExisting(&record)
        } else {
          OrderedPageMutationKindV1::UpsertLive(&record)
        },
        layout: page_layout,
      })
      .unwrap();
      if page_plan.is_unchanged() {
        assert!(!delete && expected_live.contains(&coordinate));
      } else {
        property_apply_plan(
          hash_algorithm,
          generation,
          next_page_id,
          page_layout,
          &page_plan,
          &mut pages,
          &mut directory_artifacts,
          &mut root_key,
        );
        next_page_id = page_plan.next_page_id;
      }
      if delete {
        expected_live.remove(&coordinate);
      } else {
        expected_live.insert(coordinate);
      }
      property_assert_model(&pages, hash_algorithm, &expected_live);
      generation += 1;
    }

    let retirement_coordinates = pages
      .iter()
      .map(|page| decoded_coordinates(page, hash_algorithm))
      .find(|records| !records.iter().any(|record| record.0 == 1) && records.iter().any(|record| !record.1))
      .unwrap()
      .into_iter()
      .filter_map(|record| (!record.1).then_some(record.0))
      .collect::<Vec<_>>();
    for coordinate in retirement_coordinates {
      let page_index = property_page_index(&pages, hash_algorithm, coordinate);
      let tombstone = posting_record(coordinate, coordinate, 512, true);
      let page_plan = mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
        hash_algorithm,
        source_page: &pages[page_index],
        next_posting_page: pages.get(page_index + 1).map(Vec::as_slice),
        generation,
        next_page_id,
        mutation: OrderedPageMutationKindV1::TombstoneExisting(&tombstone),
        layout: page_layout,
      })
      .unwrap();
      property_apply_plan(
        hash_algorithm,
        generation,
        next_page_id,
        page_layout,
        &page_plan,
        &mut pages,
        &mut directory_artifacts,
        &mut root_key,
      );
      next_page_id = page_plan.next_page_id;
      expected_live.remove(&coordinate);
      property_assert_model(&pages, hash_algorithm, &expected_live);
      generation += 1;
    }

    let mut saw_retirement = false;
    loop {
      let Some(page_index) = pages.iter().position(|page| decode_ordered_page(page, hash_algorithm).unwrap().tombstone_count > 0) else {
        break;
      };
      let source = decode_ordered_page(&pages[page_index], hash_algorithm).unwrap();
      let proof_page_keys = [source.key.as_slice()];
      let source_pages = [pages[page_index].as_slice()];
      let proof = TombstoneDropProofV1 {
        owner_id: &owner_id,
        source_page_keys: &proof_page_keys,
        coverage_epoch_id: 1,
        covered_through_sequence: generation,
        journal_contiguous_through_sequence: generation,
        pin_safe_through_generation: source.generation,
      };
      let page_plan = compact_ordered_page_window_v1(&OrderedPageCompactionWindowRequestV1 {
        hash_algorithm,
        source_pages: &source_pages,
        previous_posting_page: page_index.checked_sub(1).map(|index| pages[index].as_slice()),
        next_posting_page: pages.get(page_index + 1).map(Vec::as_slice),
        generation,
        next_page_id,
        tombstone_drop_proof: Some(&proof),
        layout: page_layout,
      })
      .unwrap();
      assert!(!page_plan.is_unchanged());
      saw_retirement |= !page_plan.retired_page_ids.is_empty();
      property_apply_plan(
        hash_algorithm,
        generation,
        next_page_id,
        page_layout,
        &page_plan,
        &mut pages,
        &mut directory_artifacts,
        &mut root_key,
      );
      next_page_id = page_plan.next_page_id;
      property_assert_model(&pages, hash_algorithm, &expected_live);
      generation += 1;
    }
    assert!(saw_retirement);

    let merge_trim_coordinates = pages
      .iter()
      .take(2)
      .flat_map(|page| {
        decoded_coordinates(page, hash_algorithm)
          .into_iter()
          .filter_map(|record| (!record.1).then_some(record.0))
          .skip(1)
          .collect::<Vec<_>>()
      })
      .collect::<Vec<_>>();
    for coordinate in merge_trim_coordinates {
      let page_index = property_page_index(&pages, hash_algorithm, coordinate);
      let tombstone = posting_record(coordinate, coordinate, 512, true);
      let page_plan = mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
        hash_algorithm,
        source_page: &pages[page_index],
        next_posting_page: pages.get(page_index + 1).map(Vec::as_slice),
        generation,
        next_page_id,
        mutation: OrderedPageMutationKindV1::TombstoneExisting(&tombstone),
        layout: page_layout,
      })
      .unwrap();
      property_apply_plan(
        hash_algorithm,
        generation,
        next_page_id,
        page_layout,
        &page_plan,
        &mut pages,
        &mut directory_artifacts,
        &mut root_key,
      );
      next_page_id = page_plan.next_page_id;
      expected_live.remove(&coordinate);
      property_assert_model(&pages, hash_algorithm, &expected_live);
      generation += 1;
    }
    loop {
      let Some(page_index) = pages.iter().position(|page| decode_ordered_page(page, hash_algorithm).unwrap().tombstone_count > 0) else {
        break;
      };
      let source = decode_ordered_page(&pages[page_index], hash_algorithm).unwrap();
      let proof_page_keys = [source.key.as_slice()];
      let source_pages = [pages[page_index].as_slice()];
      let proof = TombstoneDropProofV1 {
        owner_id: &owner_id,
        source_page_keys: &proof_page_keys,
        coverage_epoch_id: 1,
        covered_through_sequence: generation,
        journal_contiguous_through_sequence: generation,
        pin_safe_through_generation: source.generation,
      };
      let page_plan = compact_ordered_page_window_v1(&OrderedPageCompactionWindowRequestV1 {
        hash_algorithm,
        source_pages: &source_pages,
        previous_posting_page: page_index.checked_sub(1).map(|index| pages[index].as_slice()),
        next_posting_page: pages.get(page_index + 1).map(Vec::as_slice),
        generation,
        next_page_id,
        tombstone_drop_proof: Some(&proof),
        layout: page_layout,
      })
      .unwrap();
      property_apply_plan(
        hash_algorithm,
        generation,
        next_page_id,
        page_layout,
        &page_plan,
        &mut pages,
        &mut directory_artifacts,
        &mut root_key,
      );
      next_page_id = page_plan.next_page_id;
      property_assert_model(&pages, hash_algorithm, &expected_live);
      generation += 1;
    }

    let mut saw_merge = false;
    'merge_passes: loop {
      for page_index in 0..pages.len().saturating_sub(1) {
        let source_pages = [pages[page_index].as_slice(), pages[page_index + 1].as_slice()];
        let page_plan = compact_ordered_page_window_v1(&OrderedPageCompactionWindowRequestV1 {
          hash_algorithm,
          source_pages: &source_pages,
          previous_posting_page: page_index.checked_sub(1).map(|index| pages[index].as_slice()),
          next_posting_page: pages.get(page_index + 2).map(Vec::as_slice),
          generation,
          next_page_id,
          tombstone_drop_proof: None,
          layout: page_layout,
        })
        .unwrap();
        if page_plan.is_unchanged() {
          continue;
        }
        saw_merge = true;
        property_apply_plan(
          hash_algorithm,
          generation,
          next_page_id,
          page_layout,
          &page_plan,
          &mut pages,
          &mut directory_artifacts,
          &mut root_key,
        );
        next_page_id = page_plan.next_page_id;
        property_assert_model(&pages, hash_algorithm, &expected_live);
        generation += 1;
        continue 'merge_passes;
      }
      break;
    }
    assert!(saw_merge);
    property_assert_model(&pages, hash_algorithm, &expected_live);
  }
}
