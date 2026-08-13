use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::index_copy_on_write::{
  OrderedPageMutationKindV1, OrderedPageMutationRequestV1, default_index_page_layout_v1, mutate_ordered_page_v1,
};
use aeordb::engine::v4::index_page::{
  OrderedIndexRoleV1, OrderedPageWriteV1, PostingRecordV1, decode_ordered_page, decode_posting_record, encode_ordered_page,
  encode_posting_record, validate_posting_page_link,
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
