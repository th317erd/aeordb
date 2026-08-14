use std::fs;
use std::path::PathBuf;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::index_artifact::{IndexManifestWriteV1, decode_index_manifest, encode_index_manifest};
use aeordb::engine::v4::index_manifest::{FieldIndexManifestBodyV1, FieldNvtManifestBodyV1, IndexManifestBodyV1};
use aeordb::engine::v4::index_nvt::{
  ImmutableIndexPathV1, NvtBasisStatusV1, NvtEntryWriteV1, NvtFallbackReasonV1, NvtFallbackV1, NvtHealingDispositionV1, NvtHealingLimitsV1,
  NvtLookupAttemptV1, NvtLookupRequestV1, NvtLookupSourceV1, NvtPostingPageSampleV1, NvtTileWriteV1, SparseNvtBuildLimitsV1,
  SparseNvtBuildRequestV1, SparseNvtLookupLimitsV1, build_sparse_nvt_tiles_v1, coordinate_cell, decode_nvt_tile,
  default_nvt_healing_limits_v1, default_sparse_nvt_lookup_limits_v1, encode_nvt_tile, exact_posting_predecessor_v1, pin_field_index_v1,
  resolve_nvt_lookup_v1, select_nvt_predecessor_hint_v1, validate_field_nvt_basis_v1, validate_nvt_page_hint_v1,
};
use aeordb::engine::v4::index_page::{
  ArtifactDirectoryEntryWriteV1, ArtifactDirectoryWriteV1, OrderedIndexRoleV1, OrderedPageWriteV1, PhysicalHintV1, PostingRecordV1,
  decode_artifact_directory, decode_ordered_page, decode_ordered_record, encode_artifact_directory, encode_ordered_page,
  encode_posting_record, ordered_record_order_key,
};
use aeordb::engine::v4::reader::MalformedInputClass;

fn fixture_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/index-artifact-v1")
}

fn fixture_name(hash_algorithm: HashAlgorithm) -> &'static str {
  match hash_algorithm {
    HashAlgorithm::Blake3_256 => "aidx-blake3-256-nvt-tile-valid.bin",
    HashAlgorithm::Sha512 => "aidx-sha512-nvt-tile-valid.bin",
    _ => panic!("fixture test uses only the two frozen v4 hash profiles"),
  }
}

fn coordinate_for_cell(cell: u64, resolution: u64) -> u64 {
  assert!(resolution != 0 && cell < resolution);
  if cell == 0 {
    return 0;
  }
  u64::try_from((u128::from(cell) << 64).div_ceil(u128::from(resolution))).unwrap()
}

fn build_limits() -> SparseNvtBuildLimitsV1 {
  SparseNvtBuildLimitsV1 { maximum_page_samples: 1_024, maximum_tiles: 64, maximum_output_bytes: 4 * 1_024 * 1_024 }
}

fn lookup_limits() -> SparseNvtLookupLimitsV1 {
  default_sparse_nvt_lookup_limits_v1()
}

fn fixture_bytes(name: &str) -> Vec<u8> {
  fs::read(fixture_root().join(name)).unwrap()
}

fn posting_record(coordinate: u64, document_ordinal: u64) -> Vec<u8> {
  encode_posting_record(&PostingRecordV1 {
    tombstone: false,
    coordinate,
    document_ordinal,
    source_value_ordinal: 0,
    expansion_ordinal: 0,
    posting_key: b"k",
  })
  .unwrap()
}

fn posting_order_key(hash_algorithm: HashAlgorithm, record: &[u8]) -> Vec<u8> {
  let decoded = decode_ordered_record(record, hash_algorithm, OrderedIndexRoleV1::Posting).unwrap();
  ordered_record_order_key(&decoded).unwrap()
}

struct LookupGraph {
  hash_algorithm: HashAlgorithm,
  field_manifest: Vec<u8>,
  nvt_manifest: Vec<u8>,
  posting_directory: Vec<u8>,
  posting_pages: Vec<Vec<u8>>,
  tile_directory: Vec<u8>,
  tiles: Vec<Vec<u8>>,
  target_coordinate: u64,
  target_key: Vec<u8>,
}

fn build_lookup_graph(hash_algorithm: HashAlgorithm) -> LookupGraph {
  let profile = match hash_algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => panic!("lookup fixture uses a frozen v4 hash profile"),
  };
  let seed = fixture_bytes(&format!("aidx-{profile}-field-index-manifest-populated.bin"));
  let seed = decode_index_manifest(&seed, hash_algorithm).unwrap();
  let IndexManifestBodyV1::FieldIndex(seed_body) = &seed.details else {
    panic!("seed fixture is not a FieldIndex manifest");
  };
  let resolution = 65_536;
  let first_coordinate = coordinate_for_cell(10, resolution);
  let second_coordinate = coordinate_for_cell(4_200, resolution);
  let target_coordinate = coordinate_for_cell(4_096, resolution);
  let first_record = posting_record(first_coordinate, 1);
  let second_record = posting_record(second_coordinate, 2);
  let first_records = [first_record.as_slice()];
  let second_records = [second_record.as_slice()];
  let first_page = encode_ordered_page(&OrderedPageWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id: seed.owner_id,
    generation: seed.generation,
    page_id: 11,
    previous_page_id: 0,
    next_page_id: 12,
    records: &first_records,
  })
  .unwrap();
  let second_page = encode_ordered_page(&OrderedPageWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id: seed.owner_id,
    generation: seed.generation,
    page_id: 12,
    previous_page_id: 11,
    next_page_id: 0,
    records: &second_records,
  })
  .unwrap();
  let first = decode_ordered_page(&first_page.value, hash_algorithm).unwrap();
  let second = decode_ordered_page(&second_page.value, hash_algorithm).unwrap();
  let posting_entries = [
    ArtifactDirectoryEntryWriteV1 {
      lower_fence: first.lower_fence,
      upper_fence: first.upper_fence,
      child_hash: &first_page.key,
      child_generation: first.generation,
      live_count: u64::from(first.live_count),
      tombstone_count: u64::from(first.tombstone_count),
      page_count: 1,
      logical_bytes: first.logical_live_bytes,
      minimum_page_id: first.page_id,
      maximum_page_id: first.page_id,
      physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    },
    ArtifactDirectoryEntryWriteV1 {
      lower_fence: second.lower_fence,
      upper_fence: second.upper_fence,
      child_hash: &second_page.key,
      child_generation: second.generation,
      live_count: u64::from(second.live_count),
      tombstone_count: u64::from(second.tombstone_count),
      page_count: 1,
      logical_bytes: second.logical_live_bytes,
      minimum_page_id: second.page_id,
      maximum_page_id: second.page_id,
      physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    },
  ];
  let posting_directory = encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id: seed.owner_id,
    generation: seed.generation,
    level: 0,
    entries: &posting_entries,
  })
  .unwrap();
  let posting_directory_decoded = decode_artifact_directory(&posting_directory.value, hash_algorithm).unwrap();
  let field_manifest = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm,
    generation: seed.generation,
    owner_id: seed.owner_id,
    body: IndexManifestBodyV1::FieldIndex(FieldIndexManifestBodyV1 {
      required_reader_capabilities: seed_body.required_reader_capabilities,
      coverage: seed_body.coverage.clone(),
      value_store_manifest: seed_body.value_store_manifest,
      posting_directory_root: Some(&posting_directory.key),
      document_state_directory_root: None,
      first_page_id: first.page_id,
      last_page_id: second.page_id,
      next_page_id: 13,
      posting_page_count: posting_directory_decoded.page_count,
      state_page_count: 0,
      live_posting_count: posting_directory_decoded.live_count,
      posting_tombstone_count: posting_directory_decoded.tombstone_count,
      posting_document_count: 2,
      unindexable_document_count: 0,
      state_tombstone_count: 0,
      live_canonical_posting_bytes: posting_directory_decoded.logical_bytes,
      field_index_definition: seed_body.field_index_definition,
    }),
  })
  .unwrap();

  let samples = [
    NvtPostingPageSampleV1 {
      page_id: first.page_id,
      minimum_coordinate: first.minimum_coordinate,
      maximum_coordinate: first.maximum_coordinate,
      live_postings: u64::from(first.live_count),
    },
    NvtPostingPageSampleV1 {
      page_id: second.page_id,
      minimum_coordinate: second.minimum_coordinate,
      maximum_coordinate: second.maximum_coordinate,
      live_postings: u64::from(second.live_count),
    },
  ];
  let nvt_generation = seed.generation + 1;
  let plan = build_sparse_nvt_tiles_v1(&SparseNvtBuildRequestV1 {
    hash_algorithm,
    owner_id: seed.owner_id,
    generation: nvt_generation,
    resolution,
    tile_cell_count: 1_024,
    basis_posting_generation: seed.generation,
    pages: &samples,
    limits: build_limits(),
  })
  .unwrap();
  assert_eq!(plan.tiles.len(), 2);
  let decoded_tiles = plan.tiles.iter().map(|tile| decode_nvt_tile(&tile.value, hash_algorithm).unwrap()).collect::<Vec<_>>();
  let tile_fences = decoded_tiles.iter().map(|tile| tile.tile_start_cell.to_le_bytes()).collect::<Vec<_>>();
  let tile_entries = decoded_tiles
    .iter()
    .enumerate()
    .map(|(index, tile)| ArtifactDirectoryEntryWriteV1 {
      lower_fence: &tile_fences[index],
      upper_fence: &tile_fences[index],
      child_hash: &plan.tiles[index].key,
      child_generation: tile.generation,
      live_count: u64::try_from(tile.entries.len()).unwrap(),
      tombstone_count: 0,
      page_count: 1,
      logical_bytes: u64::try_from(plan.tiles[index].value.len()).unwrap(),
      minimum_page_id: 0,
      maximum_page_id: 0,
      physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    })
    .collect::<Vec<_>>();
  let tile_directory = encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::NvtTile,
    owner_id: seed.owner_id,
    generation: nvt_generation,
    level: 0,
    entries: &tile_entries,
  })
  .unwrap();
  let tile_directory_decoded = decode_artifact_directory(&tile_directory.value, hash_algorithm).unwrap();
  let nvt_manifest = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm,
    generation: nvt_generation,
    owner_id: seed.owner_id,
    body: IndexManifestBodyV1::FieldNvt(FieldNvtManifestBodyV1 {
      required_reader_capabilities: [0; 32],
      tile_cells: 1_024,
      resolution,
      basis_posting_generation: seed.generation,
      basis_source_head_hash: seed_body.coverage.source_namespace_root,
      tile_directory_root: Some(&tile_directory.key),
      tile_count: tile_directory_decoded.page_count,
      populated_cell_count: tile_directory_decoded.live_count,
      approximate_live_posting_count: plan.approximate_live_posting_count,
    }),
  })
  .unwrap();
  let target_record = posting_record(target_coordinate, 1);

  LookupGraph {
    hash_algorithm,
    field_manifest: field_manifest.value,
    nvt_manifest: nvt_manifest.value,
    posting_directory: posting_directory.value,
    posting_pages: vec![first_page.value, second_page.value],
    tile_directory: tile_directory.value,
    tiles: plan.tiles.into_iter().map(|tile| tile.value).collect(),
    target_coordinate,
    target_key: posting_order_key(hash_algorithm, &target_record),
  }
}

fn wrap_posting_directory(graph: &LookupGraph) -> (Vec<u8>, Vec<u8>) {
  let field = decode_index_manifest(&graph.field_manifest, graph.hash_algorithm).unwrap();
  let IndexManifestBodyV1::FieldIndex(body) = &field.details else {
    panic!("lookup graph field manifest has the wrong kind");
  };
  let leaf = decode_artifact_directory(&graph.posting_directory, graph.hash_algorithm).unwrap();
  let entries = [ArtifactDirectoryEntryWriteV1 {
    lower_fence: leaf.lower_fence,
    upper_fence: leaf.upper_fence,
    child_hash: &leaf.key,
    child_generation: leaf.generation,
    live_count: leaf.live_count,
    tombstone_count: leaf.tombstone_count,
    page_count: leaf.page_count,
    logical_bytes: leaf.logical_bytes,
    minimum_page_id: leaf.minimum_page_id,
    maximum_page_id: leaf.maximum_page_id,
    physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
  }];
  let root = encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm: graph.hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id: field.owner_id,
    generation: field.generation,
    level: 1,
    entries: &entries,
  })
  .unwrap();
  let manifest = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm: graph.hash_algorithm,
    generation: field.generation,
    owner_id: field.owner_id,
    body: IndexManifestBodyV1::FieldIndex(FieldIndexManifestBodyV1 { posting_directory_root: Some(&root.key), ..body.clone() }),
  })
  .unwrap();
  (manifest.value, root.value)
}

fn next_deterministic_u64(state: &mut u64) -> u64 {
  *state ^= *state << 13;
  *state ^= *state >> 7;
  *state ^= *state << 17;
  *state
}

fn prove_randomized_lookup_model(hash_algorithm: HashAlgorithm, resolution: u64, tile_cell_count: u32) {
  let profile = match hash_algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => panic!("randomized lookup model uses a frozen v4 hash profile"),
  };
  let seed = fixture_bytes(&format!("aidx-{profile}-field-index-manifest-populated.bin"));
  let seed = decode_index_manifest(&seed, hash_algorithm).unwrap();
  let IndexManifestBodyV1::FieldIndex(seed_body) = &seed.details else {
    panic!("randomized lookup seed is not a FieldIndex manifest");
  };
  let page_count = 64usize;
  let mut random_state = 0x4e56_542d_7631_2026 ^ resolution ^ u64::from(tile_cell_count);
  let mut cells = Vec::with_capacity(page_count);
  let mut cell = 17u64;
  for _ in 0..page_count {
    cell += 3 + next_deterministic_u64(&mut random_state) % (resolution / 96);
    assert!(cell < resolution);
    cells.push(cell);
  }
  let page_ids = (0..page_count).map(|index| u64::try_from((index * 37) % page_count + 1).unwrap()).collect::<Vec<_>>();
  let records = cells
    .iter()
    .enumerate()
    .map(|(index, cell)| posting_record(coordinate_for_cell(*cell, resolution), u64::try_from(index + 1).unwrap()))
    .collect::<Vec<_>>();
  let posting_pages = records
    .iter()
    .enumerate()
    .map(|(index, record)| {
      let records = [record.as_slice()];
      encode_ordered_page(&OrderedPageWriteV1 {
        hash_algorithm,
        role: OrderedIndexRoleV1::Posting,
        owner_id: seed.owner_id,
        generation: seed.generation,
        page_id: page_ids[index],
        previous_page_id: if index == 0 { 0 } else { page_ids[index - 1] },
        next_page_id: if index + 1 == page_count { 0 } else { page_ids[index + 1] },
        records: &records,
      })
      .unwrap()
    })
    .collect::<Vec<_>>();
  let decoded_pages = posting_pages.iter().map(|page| decode_ordered_page(&page.value, hash_algorithm).unwrap()).collect::<Vec<_>>();
  let posting_entries = decoded_pages
    .iter()
    .enumerate()
    .map(|(index, page)| ArtifactDirectoryEntryWriteV1 {
      lower_fence: page.lower_fence,
      upper_fence: page.upper_fence,
      child_hash: &posting_pages[index].key,
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
  let posting_directory = encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id: seed.owner_id,
    generation: seed.generation,
    level: 0,
    entries: &posting_entries,
  })
  .unwrap();
  let posting_directory_decoded = decode_artifact_directory(&posting_directory.value, hash_algorithm).unwrap();
  let field_manifest = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm,
    generation: seed.generation,
    owner_id: seed.owner_id,
    body: IndexManifestBodyV1::FieldIndex(FieldIndexManifestBodyV1 {
      required_reader_capabilities: seed_body.required_reader_capabilities,
      coverage: seed_body.coverage.clone(),
      value_store_manifest: seed_body.value_store_manifest,
      posting_directory_root: Some(&posting_directory.key),
      document_state_directory_root: None,
      first_page_id: page_ids[0],
      last_page_id: page_ids[page_count - 1],
      next_page_id: u64::try_from(page_count + 1).unwrap(),
      posting_page_count: posting_directory_decoded.page_count,
      state_page_count: 0,
      live_posting_count: posting_directory_decoded.live_count,
      posting_tombstone_count: posting_directory_decoded.tombstone_count,
      posting_document_count: u64::try_from(page_count).unwrap(),
      unindexable_document_count: 0,
      state_tombstone_count: 0,
      live_canonical_posting_bytes: posting_directory_decoded.logical_bytes,
      field_index_definition: seed_body.field_index_definition,
    }),
  })
  .unwrap();
  let samples = decoded_pages
    .iter()
    .map(|page| NvtPostingPageSampleV1 {
      page_id: page.page_id,
      minimum_coordinate: page.minimum_coordinate,
      maximum_coordinate: page.maximum_coordinate,
      live_postings: u64::from(page.live_count),
    })
    .collect::<Vec<_>>();
  let build_request = SparseNvtBuildRequestV1 {
    hash_algorithm,
    owner_id: seed.owner_id,
    generation: seed.generation + 1,
    resolution,
    tile_cell_count,
    basis_posting_generation: seed.generation,
    pages: &samples,
    limits: build_limits(),
  };
  let plan = build_sparse_nvt_tiles_v1(&build_request).unwrap();
  assert_eq!(plan, build_sparse_nvt_tiles_v1(&build_request).unwrap());
  let decoded_tiles = plan.tiles.iter().map(|tile| decode_nvt_tile(&tile.value, hash_algorithm).unwrap()).collect::<Vec<_>>();
  let tile_fences = decoded_tiles.iter().map(|tile| tile.tile_start_cell.to_le_bytes()).collect::<Vec<_>>();
  let tile_entries = decoded_tiles
    .iter()
    .enumerate()
    .map(|(index, tile)| ArtifactDirectoryEntryWriteV1 {
      lower_fence: &tile_fences[index],
      upper_fence: &tile_fences[index],
      child_hash: &plan.tiles[index].key,
      child_generation: tile.generation,
      live_count: u64::try_from(tile.entries.len()).unwrap(),
      tombstone_count: 0,
      page_count: 1,
      logical_bytes: u64::try_from(plan.tiles[index].value.len()).unwrap(),
      minimum_page_id: 0,
      maximum_page_id: 0,
      physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    })
    .collect::<Vec<_>>();
  let tile_directory = encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::NvtTile,
    owner_id: seed.owner_id,
    generation: seed.generation + 1,
    level: 0,
    entries: &tile_entries,
  })
  .unwrap();
  let tile_directory_decoded = decode_artifact_directory(&tile_directory.value, hash_algorithm).unwrap();
  let nvt_manifest = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm,
    generation: seed.generation + 1,
    owner_id: seed.owner_id,
    body: IndexManifestBodyV1::FieldNvt(FieldNvtManifestBodyV1 {
      required_reader_capabilities: [0; 32],
      tile_cells: tile_cell_count,
      resolution,
      basis_posting_generation: seed.generation,
      basis_source_head_hash: seed_body.coverage.source_namespace_root,
      tile_directory_root: Some(&tile_directory.key),
      tile_count: tile_directory_decoded.page_count,
      populated_cell_count: tile_directory_decoded.live_count,
      approximate_live_posting_count: plan.approximate_live_posting_count,
    }),
  })
  .unwrap();
  let field = pin_field_index_v1(&field_manifest.value, hash_algorithm).unwrap();
  let NvtBasisStatusV1::Usable(basis) = validate_field_nvt_basis_v1(&field, Some(&nvt_manifest.value)) else {
    panic!("randomized NVT basis must close to its FieldIndex");
  };
  let posting_directories = [posting_directory.value.as_slice()];
  let tile_directories = [tile_directory.value.as_slice()];
  let mut hint_count = 0usize;
  let mut fallback_count = 0usize;
  for query_index in 0..256usize {
    let target_cell = match query_index {
      0 => 0,
      1 => cells[0],
      _ => next_deterministic_u64(&mut random_state) % resolution,
    };
    let target_coordinate = coordinate_for_cell(target_cell, resolution);
    let target_record = posting_record(target_coordinate, u64::MAX);
    let target_key = posting_order_key(hash_algorithm, &target_record);
    let predecessor_count = cells.partition_point(|cell| *cell <= target_cell);
    let exact_index = predecessor_count.saturating_sub(1);
    let exact_path = ImmutableIndexPathV1 { directories: &posting_directories, leaf: &posting_pages[exact_index].value };
    let exact = exact_posting_predecessor_v1(&field, &target_key, Some(&exact_path), lookup_limits()).unwrap().unwrap();
    assert_eq!(exact.page_id, page_ids[exact_index]);

    let target_tile_start = target_cell / u64::from(tile_cell_count) * u64::from(tile_cell_count);
    let tile_end = decoded_tiles.partition_point(|tile| tile.tile_start_cell <= target_tile_start);
    let mut candidates = Vec::new();
    if tile_end != 0 {
      let current = tile_end - 1;
      candidates.push(ImmutableIndexPathV1 { directories: &tile_directories, leaf: &plan.tiles[current].value });
      if current != 0 {
        candidates.push(ImmutableIndexPathV1 { directories: &tile_directories, leaf: &plan.tiles[current - 1].value });
      }
    }
    let selection = select_nvt_predecessor_hint_v1(&basis, target_coordinate, &candidates, lookup_limits()).unwrap();
    let resolved = if let Some(hint) = selection.hint {
      hint_count += 1;
      let hint_index = page_ids.iter().position(|page_id| *page_id == hint.page_id).unwrap();
      let hint_path = ImmutableIndexPathV1 { directories: &posting_directories, leaf: &posting_pages[hint_index].value };
      let resolved = resolve_nvt_lookup_v1(&NvtLookupRequestV1 {
        field: &field,
        target_coordinate,
        target_posting_position: &target_key,
        attempt: NvtLookupAttemptV1::Hint { basis: &basis, hint, posting_path: Some(&hint_path) },
        exact_posting_path: Some(&exact_path),
        lookup_limits: lookup_limits(),
        healing_limits: default_nvt_healing_limits_v1(),
      })
      .unwrap();
      assert_eq!(resolved.source, NvtLookupSourceV1::Hint);
      assert!(hint_index <= exact_index);
      for page_index in hint_index..=exact_index {
        let page = &decoded_pages[page_index];
        let expected_next = if page_index + 1 == page_count { 0 } else { page_ids[page_index + 1] };
        assert_eq!(page.next_page_id, expected_next);
      }
      resolved
    } else {
      fallback_count += 1;
      let cause = selection.fallback.as_ref().unwrap();
      resolve_nvt_lookup_v1(&NvtLookupRequestV1 {
        field: &field,
        target_coordinate,
        target_posting_position: &target_key,
        attempt: NvtLookupAttemptV1::Fallback { basis: Some(&basis), cause },
        exact_posting_path: Some(&exact_path),
        lookup_limits: lookup_limits(),
        healing_limits: default_nvt_healing_limits_v1(),
      })
      .unwrap()
    };
    let resolved_index = page_ids.iter().position(|page_id| *page_id == resolved.anchor.as_ref().unwrap().page_id).unwrap();
    assert!(resolved_index <= exact_index);
  }
  assert!(hint_count > 0);
  assert!(fallback_count > 0);
}

#[test]
fn nvt_tile_writer_matches_both_independent_fixtures_exactly() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let expected = fs::read(fixture_root().join(fixture_name(hash_algorithm))).unwrap();
    let tile = decode_nvt_tile(&expected, hash_algorithm).unwrap();
    let entries = tile
      .entries
      .iter()
      .map(|entry| {
        let entry = entry.unwrap();
        NvtEntryWriteV1 {
          relative_cell: entry.relative_cell,
          predecessor_page_id: entry.predecessor_page_id,
          successor_page_id: entry.successor_page_id,
          approximate_live_postings: entry.approximate_live_postings,
          sample_coordinate: entry.sample_coordinate,
        }
      })
      .collect::<Vec<_>>();
    let encoded = encode_nvt_tile(&NvtTileWriteV1 {
      hash_algorithm,
      owner_id: tile.owner_id,
      generation: tile.generation,
      resolution: tile.resolution,
      tile_start_cell: tile.tile_start_cell,
      tile_cell_count: tile.tile_cell_count,
      basis_posting_generation: tile.basis_posting_generation,
      entries: &entries,
    })
    .unwrap();
    assert_eq!(encoded.value, expected);
    assert_eq!(encoded.key, tile.key);
  }
}

#[test]
fn sparse_builder_coalesces_same_cell_page_starts_and_preserves_gaps_without_sorting() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = vec![0x31; hash_algorithm.hash_length()];
  let resolution = 65_536;
  let pages = [
    NvtPostingPageSampleV1 {
      page_id: 41,
      minimum_coordinate: coordinate_for_cell(10, resolution),
      maximum_coordinate: coordinate_for_cell(10, resolution),
      live_postings: 7,
    },
    NvtPostingPageSampleV1 {
      page_id: 99,
      minimum_coordinate: coordinate_for_cell(10, resolution),
      maximum_coordinate: coordinate_for_cell(12, resolution),
      live_postings: 11,
    },
    NvtPostingPageSampleV1 {
      page_id: 55,
      minimum_coordinate: coordinate_for_cell(100, resolution),
      maximum_coordinate: coordinate_for_cell(120, resolution),
      live_postings: 5,
    },
    NvtPostingPageSampleV1 {
      page_id: 7,
      minimum_coordinate: coordinate_for_cell(4_200, resolution),
      maximum_coordinate: coordinate_for_cell(4_300, resolution),
      live_postings: 13,
    },
  ];
  let first = build_sparse_nvt_tiles_v1(&SparseNvtBuildRequestV1 {
    hash_algorithm,
    owner_id: &owner_id,
    generation: 8,
    resolution,
    tile_cell_count: 1_024,
    basis_posting_generation: 7,
    pages: &pages,
    limits: build_limits(),
  })
  .unwrap();
  let second = build_sparse_nvt_tiles_v1(&SparseNvtBuildRequestV1 {
    hash_algorithm,
    owner_id: &owner_id,
    generation: 8,
    resolution,
    tile_cell_count: 1_024,
    basis_posting_generation: 7,
    pages: &pages,
    limits: build_limits(),
  })
  .unwrap();
  assert_eq!(first, second);
  assert_eq!(first.tiles.len(), 2);
  assert_eq!(first.populated_cell_count, 3);
  assert_eq!(first.approximate_live_posting_count, 36);

  let first_tile = decode_nvt_tile(&first.tiles[0].value, hash_algorithm).unwrap();
  let first_entry = first_tile.entries.entry_at(0).unwrap();
  assert_eq!(first_tile.tile_start_cell, 0);
  assert_eq!(first_entry.relative_cell, 10);
  assert_eq!(first_entry.predecessor_page_id, Some(41));
  assert_eq!(first_entry.successor_page_id, Some(55));
  assert_eq!(first_entry.approximate_live_postings, 18);
  assert_eq!(coordinate_cell(first_entry.sample_coordinate, resolution), Some(10));
  let same_tile_successor = first_tile.entries.entry_at(1).unwrap();
  assert_eq!(same_tile_successor.relative_cell, 100);
  assert_eq!(same_tile_successor.predecessor_page_id, Some(55));
  assert_eq!(same_tile_successor.successor_page_id, Some(7));

  let second_tile = decode_nvt_tile(&first.tiles[1].value, hash_algorithm).unwrap();
  assert_eq!(second_tile.tile_start_cell, 4_096);
  assert_eq!(second_tile.entries.entry_at(0).unwrap().predecessor_page_id, Some(7));
}

#[test]
fn sparse_builder_accepts_each_ratified_tile_cell_count() {
  let hash_algorithm = HashAlgorithm::Sha512;
  let owner_id = vec![0x71; hash_algorithm.hash_length()];
  let resolution = 65_536;
  let pages = [NvtPostingPageSampleV1 {
    page_id: 1,
    minimum_coordinate: coordinate_for_cell(33_000, resolution),
    maximum_coordinate: coordinate_for_cell(33_100, resolution),
    live_postings: 1,
  }];
  for tile_cell_count in [256, 1_024, 4_096] {
    let plan = build_sparse_nvt_tiles_v1(&SparseNvtBuildRequestV1 {
      hash_algorithm,
      owner_id: &owner_id,
      generation: 2,
      resolution,
      tile_cell_count,
      basis_posting_generation: 1,
      pages: &pages,
      limits: build_limits(),
    })
    .unwrap();
    let tile = decode_nvt_tile(&plan.tiles[0].value, hash_algorithm).unwrap();
    assert_eq!(tile.tile_cell_count, tile_cell_count);
    assert_eq!(tile.tile_start_cell, 33_000 / u64::from(tile_cell_count) * u64::from(tile_cell_count));
  }
}

#[test]
fn sparse_builder_represents_an_empty_index_without_tiles() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = vec![0x21; hash_algorithm.hash_length()];
  let plan = build_sparse_nvt_tiles_v1(&SparseNvtBuildRequestV1 {
    hash_algorithm,
    owner_id: &owner_id,
    generation: 2,
    resolution: 65_536,
    tile_cell_count: 1_024,
    basis_posting_generation: 1,
    pages: &[],
    limits: build_limits(),
  })
  .unwrap();
  assert!(plan.tiles.is_empty());
  assert_eq!(plan.populated_cell_count, 0);
  assert_eq!(plan.approximate_live_posting_count, 0);
  assert_eq!(plan.retained_encoded_bytes, 0);
}

#[test]
fn tile_writer_and_sparse_builder_reject_malformed_or_amplifying_input() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = vec![0x41; hash_algorithm.hash_length()];
  let resolution = 65_536;
  let entries = [
    NvtEntryWriteV1 {
      relative_cell: 9,
      predecessor_page_id: Some(1),
      successor_page_id: None,
      approximate_live_postings: 1,
      sample_coordinate: coordinate_for_cell(9, resolution),
    },
    NvtEntryWriteV1 {
      relative_cell: 8,
      predecessor_page_id: Some(2),
      successor_page_id: None,
      approximate_live_postings: 1,
      sample_coordinate: coordinate_for_cell(8, resolution),
    },
  ];
  assert_eq!(
    encode_nvt_tile(&NvtTileWriteV1 {
      hash_algorithm,
      owner_id: &owner_id,
      generation: 2,
      resolution,
      tile_start_cell: 0,
      tile_cell_count: 1_024,
      basis_posting_generation: 1,
      entries: &entries,
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );

  let reversed_pages = [
    NvtPostingPageSampleV1 {
      page_id: 2,
      minimum_coordinate: coordinate_for_cell(20, resolution),
      maximum_coordinate: coordinate_for_cell(21, resolution),
      live_postings: 1,
    },
    NvtPostingPageSampleV1 {
      page_id: 1,
      minimum_coordinate: coordinate_for_cell(10, resolution),
      maximum_coordinate: coordinate_for_cell(11, resolution),
      live_postings: 1,
    },
  ];
  let request = SparseNvtBuildRequestV1 {
    hash_algorithm,
    owner_id: &owner_id,
    generation: 2,
    resolution,
    tile_cell_count: 1_024,
    basis_posting_generation: 1,
    pages: &reversed_pages,
    limits: build_limits(),
  };
  assert_eq!(build_sparse_nvt_tiles_v1(&request).unwrap_err().class(), MalformedInputClass::NoncanonicalOrderOrDuplicate);

  let one_page = [NvtPostingPageSampleV1 { page_id: 1, minimum_coordinate: 0, maximum_coordinate: 1, live_postings: 1 }];
  let tiny = SparseNvtBuildRequestV1 {
    pages: &one_page,
    limits: SparseNvtBuildLimitsV1 { maximum_page_samples: 1, maximum_tiles: 1, maximum_output_bytes: 1 },
    ..request
  };
  assert_eq!(build_sparse_nvt_tiles_v1(&tiny).unwrap_err().class(), MalformedInputClass::AllocationAmplification);
}

#[test]
fn tile_writer_rejects_identity_geometry_presence_sample_and_aggregate_corruption() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = vec![0x51; hash_algorithm.hash_length()];
  let resolution = 65_536;
  let valid = NvtEntryWriteV1 {
    relative_cell: 3,
    predecessor_page_id: Some(1),
    successor_page_id: Some(2),
    approximate_live_postings: 1,
    sample_coordinate: coordinate_for_cell(3, resolution),
  };
  let valid_request = NvtTileWriteV1 {
    hash_algorithm,
    owner_id: &owner_id,
    generation: 2,
    resolution,
    tile_start_cell: 0,
    tile_cell_count: 1_024,
    basis_posting_generation: 1,
    entries: std::slice::from_ref(&valid),
  };

  let zero_owner = vec![0; hash_algorithm.hash_length()];
  assert_eq!(
    encode_nvt_tile(&NvtTileWriteV1 { owner_id: &zero_owner, ..valid_request }).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  assert_eq!(
    encode_nvt_tile(&NvtTileWriteV1 { owner_id: &owner_id[..owner_id.len() - 1], ..valid_request }).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  assert_eq!(
    encode_nvt_tile(&NvtTileWriteV1 { generation: 0, ..valid_request }).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  assert_eq!(
    encode_nvt_tile(&NvtTileWriteV1 { basis_posting_generation: 0, ..valid_request }).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  assert_eq!(
    encode_nvt_tile(&NvtTileWriteV1 { resolution: 65_535, ..valid_request }).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  assert_eq!(
    encode_nvt_tile(&NvtTileWriteV1 { tile_cell_count: 3, ..valid_request }).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  assert_eq!(
    encode_nvt_tile(&NvtTileWriteV1 { tile_start_cell: 1, ..valid_request }).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  assert_eq!(
    encode_nvt_tile(&NvtTileWriteV1 { entries: &[], ..valid_request }).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let zero_page = NvtEntryWriteV1 { predecessor_page_id: Some(0), ..valid };
  assert_eq!(
    encode_nvt_tile(&NvtTileWriteV1 { entries: std::slice::from_ref(&zero_page), ..valid_request }).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  let zero_successor = NvtEntryWriteV1 { successor_page_id: Some(0), ..valid };
  assert_eq!(
    encode_nvt_tile(&NvtTileWriteV1 { entries: std::slice::from_ref(&zero_successor), ..valid_request }).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  let wrong_sample = NvtEntryWriteV1 { sample_coordinate: coordinate_for_cell(4, resolution), ..valid };
  assert_eq!(
    encode_nvt_tile(&NvtTileWriteV1 { entries: std::slice::from_ref(&wrong_sample), ..valid_request }).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  let overflow = [
    NvtEntryWriteV1 { relative_cell: 3, approximate_live_postings: u64::MAX, ..valid },
    NvtEntryWriteV1 {
      relative_cell: 4,
      predecessor_page_id: Some(2),
      successor_page_id: None,
      approximate_live_postings: 1,
      sample_coordinate: coordinate_for_cell(4, resolution),
    },
  ];
  assert_eq!(
    encode_nvt_tile(&NvtTileWriteV1 { entries: &overflow, ..valid_request }).unwrap_err().class(),
    MalformedInputClass::LengthCountOrArithmeticOverflow
  );
}

#[test]
fn sparse_builder_rejects_bad_pages_and_each_resource_limit() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = vec![0x61; hash_algorithm.hash_length()];
  let resolution = 65_536;
  let pages = [
    NvtPostingPageSampleV1 {
      page_id: 1,
      minimum_coordinate: coordinate_for_cell(1, resolution),
      maximum_coordinate: coordinate_for_cell(2, resolution),
      live_postings: 1,
    },
    NvtPostingPageSampleV1 {
      page_id: 2,
      minimum_coordinate: coordinate_for_cell(2, resolution),
      maximum_coordinate: coordinate_for_cell(3, resolution),
      live_postings: 1,
    },
  ];
  let valid = SparseNvtBuildRequestV1 {
    hash_algorithm,
    owner_id: &owner_id,
    generation: 2,
    resolution,
    tile_cell_count: 1_024,
    basis_posting_generation: 1,
    pages: &pages,
    limits: build_limits(),
  };

  assert_eq!(
    build_sparse_nvt_tiles_v1(&SparseNvtBuildRequestV1 { owner_id: &owner_id[..owner_id.len() - 1], ..valid }).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  assert_eq!(
    build_sparse_nvt_tiles_v1(&SparseNvtBuildRequestV1 { generation: 0, ..valid }).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  assert_eq!(
    build_sparse_nvt_tiles_v1(&SparseNvtBuildRequestV1 { basis_posting_generation: 0, ..valid }).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  assert_eq!(
    build_sparse_nvt_tiles_v1(&SparseNvtBuildRequestV1 { tile_cell_count: 3, ..valid }).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  for limits in [
    SparseNvtBuildLimitsV1 { maximum_page_samples: 0, ..build_limits() },
    SparseNvtBuildLimitsV1 { maximum_tiles: 0, ..build_limits() },
    SparseNvtBuildLimitsV1 { maximum_output_bytes: 0, ..build_limits() },
  ] {
    assert_eq!(
      build_sparse_nvt_tiles_v1(&SparseNvtBuildRequestV1 { limits, ..valid }).unwrap_err().class(),
      MalformedInputClass::AllocationAmplification
    );
  }

  let zero_id = [NvtPostingPageSampleV1 { page_id: 0, ..pages[0] }];
  assert_eq!(
    build_sparse_nvt_tiles_v1(&SparseNvtBuildRequestV1 { pages: &zero_id, ..valid }).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  let inverted = [NvtPostingPageSampleV1 { minimum_coordinate: 2, maximum_coordinate: 1, ..pages[0] }];
  assert_eq!(
    build_sparse_nvt_tiles_v1(&SparseNvtBuildRequestV1 { pages: &inverted, ..valid }).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  let overlapping = [pages[0], NvtPostingPageSampleV1 { minimum_coordinate: pages[0].minimum_coordinate, ..pages[1] }];
  assert_eq!(
    build_sparse_nvt_tiles_v1(&SparseNvtBuildRequestV1 { pages: &overlapping, ..valid }).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );
  assert_eq!(
    build_sparse_nvt_tiles_v1(&SparseNvtBuildRequestV1 {
      limits: SparseNvtBuildLimitsV1 { maximum_page_samples: 1, ..build_limits() },
      ..valid
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::AllocationAmplification
  );

  let separate_tiles = [
    pages[0],
    NvtPostingPageSampleV1 {
      page_id: 2,
      minimum_coordinate: coordinate_for_cell(2_000, resolution),
      maximum_coordinate: coordinate_for_cell(2_001, resolution),
      live_postings: 1,
    },
  ];
  assert_eq!(
    build_sparse_nvt_tiles_v1(&SparseNvtBuildRequestV1 {
      pages: &separate_tiles,
      limits: SparseNvtBuildLimitsV1 { maximum_tiles: 1, ..build_limits() },
      ..valid
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::AllocationAmplification
  );

  let overflowing_postings = [
    NvtPostingPageSampleV1 { live_postings: u64::MAX, ..pages[0] },
    NvtPostingPageSampleV1 {
      page_id: 2,
      minimum_coordinate: pages[0].maximum_coordinate,
      maximum_coordinate: pages[0].maximum_coordinate,
      live_postings: 1,
    },
  ];
  assert_eq!(
    build_sparse_nvt_tiles_v1(&SparseNvtBuildRequestV1 { pages: &overflowing_postings, ..valid }).unwrap_err().class(),
    MalformedInputClass::LengthCountOrArithmeticOverflow
  );
}

#[test]
fn pinned_basis_crosses_to_the_previous_sparse_tile_and_matches_exact_fallback() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let graph = build_lookup_graph(hash_algorithm);
    let field = pin_field_index_v1(&graph.field_manifest, hash_algorithm).unwrap();
    let NvtBasisStatusV1::Usable(basis) = validate_field_nvt_basis_v1(&field, Some(&graph.nvt_manifest)) else {
      panic!("matching FieldNvt basis must be usable");
    };
    let tile_directories = [graph.tile_directory.as_slice()];
    let candidates = [
      ImmutableIndexPathV1 { directories: &tile_directories, leaf: &graph.tiles[1] },
      ImmutableIndexPathV1 { directories: &tile_directories, leaf: &graph.tiles[0] },
    ];
    let hint = select_nvt_predecessor_hint_v1(&basis, graph.target_coordinate, &candidates, lookup_limits()).unwrap().hint.unwrap();
    assert_eq!(hint.page_id, 11);
    assert_eq!(hint.tile_start_cell, 0);

    let posting_directories = [graph.posting_directory.as_slice()];
    let hinted_path = ImmutableIndexPathV1 { directories: &posting_directories, leaf: &graph.posting_pages[0] };
    let hinted = validate_nvt_page_hint_v1(&field, &graph.target_key, hint.page_id, Some(&hinted_path), lookup_limits()).unwrap().unwrap();
    assert_eq!(hinted.page_id, 11);
    assert_eq!(hinted.page_artifact_hash, decode_ordered_page(&graph.posting_pages[0], hash_algorithm).unwrap().key);
    let exact_path = ImmutableIndexPathV1 { directories: &posting_directories, leaf: &graph.posting_pages[0] };
    let exact = exact_posting_predecessor_v1(&field, &graph.target_key, Some(&exact_path), lookup_limits()).unwrap().unwrap();
    assert_eq!(exact.page_id, 11);
    assert_eq!(exact.page_artifact_hash, hinted.page_artifact_hash);
  }
}

#[test]
fn absent_stale_and_corrupt_nvt_evidence_degrades_to_the_exact_posting_directory() {
  let graph = build_lookup_graph(HashAlgorithm::Blake3_256);
  let field = pin_field_index_v1(&graph.field_manifest, graph.hash_algorithm).unwrap();
  let NvtBasisStatusV1::Unavailable(absent) = validate_field_nvt_basis_v1(&field, None) else {
    panic!("absent FieldNvt must be unavailable");
  };
  assert_eq!(absent.reason, NvtFallbackReasonV1::Absent);
  assert!(absent.diagnostic.is_none());

  let nvt = decode_index_manifest(&graph.nvt_manifest, graph.hash_algorithm).unwrap();
  let IndexManifestBodyV1::FieldNvt(body) = &nvt.details else {
    panic!("fixture is not an NVT manifest");
  };
  let stale = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm: graph.hash_algorithm,
    generation: nvt.generation,
    owner_id: nvt.owner_id,
    body: IndexManifestBodyV1::FieldNvt(FieldNvtManifestBodyV1 {
      basis_posting_generation: body.basis_posting_generation + 1,
      ..body.clone()
    }),
  })
  .unwrap();
  let NvtBasisStatusV1::Unavailable(stale) = validate_field_nvt_basis_v1(&field, Some(&stale.value)) else {
    panic!("stale FieldNvt must be unavailable");
  };
  assert_eq!(stale.reason, NvtFallbackReasonV1::StalePostingGeneration);
  assert!(stale.diagnostic.is_none());

  let mut corrupt = graph.nvt_manifest.clone();
  let corrupt_offset = corrupt.len() / 2;
  corrupt[corrupt_offset] ^= 0x80;
  let NvtBasisStatusV1::Unavailable(corrupt) = validate_field_nvt_basis_v1(&field, Some(&corrupt)) else {
    panic!("corrupt FieldNvt must be unavailable");
  };
  assert_eq!(corrupt.reason, NvtFallbackReasonV1::Corrupt);
  assert_eq!(corrupt.diagnostic.as_ref().unwrap().class(), MalformedInputClass::ChecksumOrIntegrityMismatch);

  let posting_directories = [graph.posting_directory.as_slice()];
  let exact_path = ImmutableIndexPathV1 { directories: &posting_directories, leaf: &graph.posting_pages[0] };
  assert_eq!(exact_posting_predecessor_v1(&field, &graph.target_key, Some(&exact_path), lookup_limits()).unwrap().unwrap().page_id, 11);
  let resolved = resolve_nvt_lookup_v1(&NvtLookupRequestV1 {
    field: &field,
    target_coordinate: graph.target_coordinate,
    target_posting_position: &graph.target_key,
    attempt: NvtLookupAttemptV1::Fallback { basis: None, cause: &corrupt },
    exact_posting_path: Some(&exact_path),
    lookup_limits: lookup_limits(),
    healing_limits: default_nvt_healing_limits_v1(),
  })
  .unwrap();
  let NvtHealingDispositionV1::Proposed(proposal) = resolved.healing else {
    panic!("corrupt NVT evidence should produce a bounded rebuild proposal after exact fallback");
  };
  let diagnostic = proposal.diagnostic.unwrap();
  assert_eq!(diagnostic.class, MalformedInputClass::ChecksumOrIntegrityMismatch);
  assert_eq!(diagnostic.code, corrupt.diagnostic.as_ref().unwrap().code());
}

#[test]
fn stale_page_id_falls_back_but_authoritative_directory_corruption_fails_closed() {
  let graph = build_lookup_graph(HashAlgorithm::Blake3_256);
  let field = pin_field_index_v1(&graph.field_manifest, graph.hash_algorithm).unwrap();
  let posting_directories = [graph.posting_directory.as_slice()];
  let hinted_path = ImmutableIndexPathV1 { directories: &posting_directories, leaf: &graph.posting_pages[0] };
  assert_eq!(validate_nvt_page_hint_v1(&field, &graph.target_key, 999, Some(&hinted_path), lookup_limits()).unwrap(), None);
  let exact_path = ImmutableIndexPathV1 { directories: &posting_directories, leaf: &graph.posting_pages[0] };
  assert_eq!(exact_posting_predecessor_v1(&field, &graph.target_key, Some(&exact_path), lookup_limits()).unwrap().unwrap().page_id, 11);
  let forward_path = ImmutableIndexPathV1 { directories: &posting_directories, leaf: &graph.posting_pages[1] };
  assert!(validate_nvt_page_hint_v1(&field, &graph.target_key, 12, Some(&forward_path), lookup_limits()).unwrap().is_none());

  let mut corrupt_directory = graph.posting_directory.clone();
  let corrupt_offset = corrupt_directory.len() / 2;
  corrupt_directory[corrupt_offset] ^= 0x40;
  let corrupt_directories = [corrupt_directory.as_slice()];
  let corrupt_path = ImmutableIndexPathV1 { directories: &corrupt_directories, leaf: &graph.posting_pages[0] };
  assert_eq!(
    exact_posting_predecessor_v1(&field, &graph.target_key, Some(&corrupt_path), lookup_limits()).unwrap_err().class(),
    MalformedInputClass::ChecksumOrIntegrityMismatch
  );

  let mut corrupt_page = graph.posting_pages[0].clone();
  let corrupt_offset = corrupt_page.len() / 2;
  corrupt_page[corrupt_offset] ^= 0x20;
  let valid_directories = [graph.posting_directory.as_slice()];
  let corrupt_path = ImmutableIndexPathV1 { directories: &valid_directories, leaf: &corrupt_page };
  assert_eq!(
    exact_posting_predecessor_v1(&field, &graph.target_key, Some(&corrupt_path), lookup_limits()).unwrap_err().class(),
    MalformedInputClass::ChecksumOrIntegrityMismatch
  );
}

#[test]
fn multi_level_posting_paths_close_every_parent_descriptor() {
  let graph = build_lookup_graph(HashAlgorithm::Sha512);
  let (field_manifest, root_directory) = wrap_posting_directory(&graph);
  let field = pin_field_index_v1(&field_manifest, graph.hash_algorithm).unwrap();
  let directories = [root_directory.as_slice(), graph.posting_directory.as_slice()];
  let path = ImmutableIndexPathV1 { directories: &directories, leaf: &graph.posting_pages[0] };
  assert_eq!(exact_posting_predecessor_v1(&field, &graph.target_key, Some(&path), lookup_limits()).unwrap().unwrap().page_id, 11);
  assert_eq!(validate_nvt_page_hint_v1(&field, &graph.target_key, 11, Some(&path), lookup_limits()).unwrap().unwrap().page_id, 11);

  let detached_directories = [root_directory.as_slice()];
  let detached = ImmutableIndexPathV1 { directories: &detached_directories, leaf: &graph.posting_pages[0] };
  assert_eq!(
    exact_posting_predecessor_v1(&field, &graph.target_key, Some(&detached), lookup_limits()).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
}

#[test]
fn basis_checks_source_owner_and_preserves_corrupt_tile_diagnostics() {
  let graph = build_lookup_graph(HashAlgorithm::Blake3_256);
  let field = pin_field_index_v1(&graph.field_manifest, graph.hash_algorithm).unwrap();
  let nvt = decode_index_manifest(&graph.nvt_manifest, graph.hash_algorithm).unwrap();
  let IndexManifestBodyV1::FieldNvt(body) = &nvt.details else {
    panic!("fixture is not an NVT manifest");
  };
  let stale_source_hash = vec![0xf1; graph.hash_algorithm.hash_length()];
  let stale_source = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm: graph.hash_algorithm,
    generation: nvt.generation,
    owner_id: nvt.owner_id,
    body: IndexManifestBodyV1::FieldNvt(FieldNvtManifestBodyV1 { basis_source_head_hash: &stale_source_hash, ..body.clone() }),
  })
  .unwrap();
  let NvtBasisStatusV1::Unavailable(stale_source) = validate_field_nvt_basis_v1(&field, Some(&stale_source.value)) else {
    panic!("source-stale NVT must be unavailable");
  };
  assert_eq!(stale_source.reason, NvtFallbackReasonV1::StaleSourceHead);

  let foreign_owner = vec![0xa7; graph.hash_algorithm.hash_length()];
  let foreign = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm: graph.hash_algorithm,
    generation: nvt.generation,
    owner_id: &foreign_owner,
    body: IndexManifestBodyV1::FieldNvt(body.clone()),
  })
  .unwrap();
  let NvtBasisStatusV1::Unavailable(foreign) = validate_field_nvt_basis_v1(&field, Some(&foreign.value)) else {
    panic!("foreign-owner NVT must be unavailable");
  };
  assert_eq!(foreign.reason, NvtFallbackReasonV1::IncompatibleOwner);

  let NvtBasisStatusV1::Usable(basis) = validate_field_nvt_basis_v1(&field, Some(&graph.nvt_manifest)) else {
    panic!("matching NVT must be usable");
  };
  let mut corrupt_tile = graph.tiles[1].clone();
  let corrupt_offset = corrupt_tile.len() / 2;
  corrupt_tile[corrupt_offset] ^= 0x20;
  let tile_directories = [graph.tile_directory.as_slice()];
  let candidates = [ImmutableIndexPathV1 { directories: &tile_directories, leaf: &corrupt_tile }];
  let selection = select_nvt_predecessor_hint_v1(&basis, graph.target_coordinate, &candidates, lookup_limits()).unwrap();
  assert!(selection.hint.is_none());
  let fallback = selection.fallback.unwrap();
  assert_eq!(fallback.reason, NvtFallbackReasonV1::Corrupt);
  assert_eq!(fallback.diagnostic.unwrap().class(), MalformedInputClass::ChecksumOrIntegrityMismatch);
}

#[test]
fn missing_and_mismatched_hints_empty_indexes_and_lookup_limits_have_explicit_results() {
  let graph = build_lookup_graph(HashAlgorithm::Blake3_256);
  let field = pin_field_index_v1(&graph.field_manifest, graph.hash_algorithm).unwrap();
  let NvtBasisStatusV1::Usable(basis) = validate_field_nvt_basis_v1(&field, Some(&graph.nvt_manifest)) else {
    panic!("matching NVT must be usable");
  };
  let tile_directories = [graph.tile_directory.as_slice()];
  let mismatched_path = [ImmutableIndexPathV1 { directories: &tile_directories, leaf: &graph.tiles[1] }];
  let selection = select_nvt_predecessor_hint_v1(&basis, 0, &mismatched_path, lookup_limits()).unwrap();
  assert!(selection.hint.is_none());
  let fallback = selection.fallback.unwrap();
  assert_eq!(fallback.reason, NvtFallbackReasonV1::Corrupt);
  assert!(fallback.diagnostic.is_some());
  let selection = select_nvt_predecessor_hint_v1(&basis, 0, &[], lookup_limits()).unwrap();
  assert_eq!(selection.fallback.unwrap().reason, NvtFallbackReasonV1::MissingPredecessor);

  let profile = "blake3-256";
  let empty_manifest = fixture_bytes(&format!("aidx-{profile}-field-index-manifest-empty.bin"));
  let empty = pin_field_index_v1(&empty_manifest, graph.hash_algorithm).unwrap();
  assert!(exact_posting_predecessor_v1(&empty, &graph.target_key, None, lookup_limits()).unwrap().is_none());
  let absent = NvtFallbackV1 { reason: NvtFallbackReasonV1::Absent, diagnostic: None };
  let empty_resolution = resolve_nvt_lookup_v1(&NvtLookupRequestV1 {
    field: &empty,
    target_coordinate: graph.target_coordinate,
    target_posting_position: &graph.target_key,
    attempt: NvtLookupAttemptV1::Fallback { basis: None, cause: &absent },
    exact_posting_path: None,
    lookup_limits: lookup_limits(),
    healing_limits: default_nvt_healing_limits_v1(),
  })
  .unwrap();
  assert!(empty_resolution.anchor.is_none());
  assert_eq!(empty_resolution.healing, NvtHealingDispositionV1::NotNeeded);

  let candidates = [
    ImmutableIndexPathV1 { directories: &tile_directories, leaf: &graph.tiles[1] },
    ImmutableIndexPathV1 { directories: &tile_directories, leaf: &graph.tiles[0] },
  ];
  let one_candidate = SparseNvtLookupLimitsV1 { maximum_tile_candidates: 1, ..lookup_limits() };
  let selection = select_nvt_predecessor_hint_v1(&basis, graph.target_coordinate, &candidates, one_candidate).unwrap();
  let fallback = selection.fallback.unwrap();
  assert_eq!(fallback.reason, NvtFallbackReasonV1::ResourceLimit);
  assert_eq!(fallback.diagnostic.unwrap().class(), MalformedInputClass::AllocationAmplification);
  let one_byte = SparseNvtLookupLimitsV1 { maximum_input_bytes: 1, ..lookup_limits() };
  let selection = select_nvt_predecessor_hint_v1(&basis, graph.target_coordinate, &candidates[..1], one_byte).unwrap();
  let fallback = selection.fallback.unwrap();
  assert_eq!(fallback.reason, NvtFallbackReasonV1::ResourceLimit);
  assert_eq!(fallback.diagnostic.unwrap().class(), MalformedInputClass::AllocationAmplification);
  let invalid_depth = SparseNvtLookupLimitsV1 { maximum_directory_depth: 17, ..lookup_limits() };
  assert_eq!(
    select_nvt_predecessor_hint_v1(&basis, graph.target_coordinate, &[], invalid_depth).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );
  let unbounded_candidates = SparseNvtLookupLimitsV1 { maximum_tile_candidates: 17, ..lookup_limits() };
  assert_eq!(
    select_nvt_predecessor_hint_v1(&basis, graph.target_coordinate, &[], unbounded_candidates).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );
  let unbounded_bytes = SparseNvtLookupLimitsV1 { maximum_input_bytes: 64 * 1_024 * 1_024 + 1, ..lookup_limits() };
  assert_eq!(
    select_nvt_predecessor_hint_v1(&basis, graph.target_coordinate, &[], unbounded_bytes).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );
}

#[test]
fn completed_lookup_uses_valid_hints_and_proposes_bounded_healing_only_after_exact_fallback() {
  let graph = build_lookup_graph(HashAlgorithm::Blake3_256);
  let field = pin_field_index_v1(&graph.field_manifest, graph.hash_algorithm).unwrap();
  let NvtBasisStatusV1::Usable(basis) = validate_field_nvt_basis_v1(&field, Some(&graph.nvt_manifest)) else {
    panic!("matching NVT must be usable");
  };
  let tile_directories = [graph.tile_directory.as_slice()];
  let candidates = [
    ImmutableIndexPathV1 { directories: &tile_directories, leaf: &graph.tiles[1] },
    ImmutableIndexPathV1 { directories: &tile_directories, leaf: &graph.tiles[0] },
  ];
  let hint = select_nvt_predecessor_hint_v1(&basis, graph.target_coordinate, &candidates, lookup_limits()).unwrap().hint.unwrap();
  let posting_directories = [graph.posting_directory.as_slice()];
  let posting_path = ImmutableIndexPathV1 { directories: &posting_directories, leaf: &graph.posting_pages[0] };
  let hinted = resolve_nvt_lookup_v1(&NvtLookupRequestV1 {
    field: &field,
    target_coordinate: graph.target_coordinate,
    target_posting_position: &graph.target_key,
    attempt: NvtLookupAttemptV1::Hint { basis: &basis, hint, posting_path: Some(&posting_path) },
    exact_posting_path: Some(&posting_path),
    lookup_limits: lookup_limits(),
    healing_limits: default_nvt_healing_limits_v1(),
  })
  .unwrap();
  assert_eq!(hinted.source, NvtLookupSourceV1::Hint);
  assert_eq!(hinted.anchor.as_ref().unwrap().page_id, 11);
  assert_eq!(hinted.healing, NvtHealingDispositionV1::NotNeeded);

  let fallback = NvtFallbackV1 { reason: NvtFallbackReasonV1::MissingPredecessor, diagnostic: None };
  let exact = resolve_nvt_lookup_v1(&NvtLookupRequestV1 {
    field: &field,
    target_coordinate: graph.target_coordinate,
    target_posting_position: &graph.target_key,
    attempt: NvtLookupAttemptV1::Fallback { basis: Some(&basis), cause: &fallback },
    exact_posting_path: Some(&posting_path),
    lookup_limits: lookup_limits(),
    healing_limits: default_nvt_healing_limits_v1(),
  })
  .unwrap();
  assert_eq!(exact.source, NvtLookupSourceV1::ExactFallback);
  let anchor = exact.anchor.as_ref().unwrap();
  assert_eq!(anchor.page_id, 11);
  let NvtHealingDispositionV1::Proposed(proposal) = exact.healing else {
    panic!("successful exact fallback must produce one bounded healing proposal");
  };
  assert_eq!(proposal.field_index_manifest_key, field.manifest_key);
  assert_eq!(proposal.observed_nvt_manifest_key, Some(basis.manifest_key));
  assert_eq!(proposal.owner_id, field.owner_id);
  assert_eq!(proposal.posting_generation, field.generation);
  assert_eq!(proposal.source_head_hash, field.source_head_hash);
  assert_eq!(proposal.target_coordinate, graph.target_coordinate);
  assert_eq!(proposal.exact_page_id, anchor.page_id);
  assert_eq!(proposal.exact_page_generation, anchor.generation);
  assert_eq!(proposal.exact_page_artifact_hash, anchor.page_artifact_hash);
  assert_eq!(proposal.reason, NvtFallbackReasonV1::MissingPredecessor);
  assert!(proposal.diagnostic.is_none());
  assert!(proposal.retained_bytes <= default_nvt_healing_limits_v1().maximum_proposal_bytes);
}

#[test]
fn healing_resource_pressure_never_changes_the_exact_lookup_decision() {
  let graph = build_lookup_graph(HashAlgorithm::Sha512);
  let field = pin_field_index_v1(&graph.field_manifest, graph.hash_algorithm).unwrap();
  let posting_directories = [graph.posting_directory.as_slice()];
  let posting_path = ImmutableIndexPathV1 { directories: &posting_directories, leaf: &graph.posting_pages[0] };
  let fallback = NvtFallbackV1 { reason: NvtFallbackReasonV1::Absent, diagnostic: None };
  let unrestricted = resolve_nvt_lookup_v1(&NvtLookupRequestV1 {
    field: &field,
    target_coordinate: graph.target_coordinate,
    target_posting_position: &graph.target_key,
    attempt: NvtLookupAttemptV1::Fallback { basis: None, cause: &fallback },
    exact_posting_path: Some(&posting_path),
    lookup_limits: lookup_limits(),
    healing_limits: default_nvt_healing_limits_v1(),
  })
  .unwrap();
  let bounded = resolve_nvt_lookup_v1(&NvtLookupRequestV1 {
    field: &field,
    target_coordinate: graph.target_coordinate,
    target_posting_position: &graph.target_key,
    attempt: NvtLookupAttemptV1::Fallback { basis: None, cause: &fallback },
    exact_posting_path: Some(&posting_path),
    lookup_limits: lookup_limits(),
    healing_limits: NvtHealingLimitsV1 { maximum_proposal_bytes: 1 },
  })
  .unwrap();
  assert_eq!(bounded.anchor, unrestricted.anchor);
  assert_eq!(bounded.source, NvtLookupSourceV1::ExactFallback);
  let NvtHealingDispositionV1::Skipped(error) = bounded.healing else {
    panic!("healing pressure must be reported without replacing the exact result");
  };
  assert_eq!(error.class(), MalformedInputClass::AllocationAmplification);
}

#[test]
fn invalid_hints_and_nonrepairable_pressure_fall_back_without_weakening_authoritative_errors() {
  let graph = build_lookup_graph(HashAlgorithm::Blake3_256);
  let field = pin_field_index_v1(&graph.field_manifest, graph.hash_algorithm).unwrap();
  let NvtBasisStatusV1::Usable(basis) = validate_field_nvt_basis_v1(&field, Some(&graph.nvt_manifest)) else {
    panic!("matching NVT must be usable");
  };
  let posting_directories = [graph.posting_directory.as_slice()];
  let posting_path = ImmutableIndexPathV1 { directories: &posting_directories, leaf: &graph.posting_pages[0] };
  let invalid_hint = aeordb::engine::v4::index_nvt::NvtPageHintV1 { page_id: 11, tile_start_cell: u64::MAX, sample_coordinate: u64::MAX };
  let resolved = resolve_nvt_lookup_v1(&NvtLookupRequestV1 {
    field: &field,
    target_coordinate: graph.target_coordinate,
    target_posting_position: &graph.target_key,
    attempt: NvtLookupAttemptV1::Hint { basis: &basis, hint: invalid_hint, posting_path: Some(&posting_path) },
    exact_posting_path: Some(&posting_path),
    lookup_limits: lookup_limits(),
    healing_limits: default_nvt_healing_limits_v1(),
  })
  .unwrap();
  assert_eq!(resolved.source, NvtLookupSourceV1::ExactFallback);
  let NvtHealingDispositionV1::Proposed(proposal) = resolved.healing else {
    panic!("invalid hint must propose repair after the exact result is known");
  };
  assert_eq!(proposal.reason, NvtFallbackReasonV1::StalePageHint);

  let pressure = NvtFallbackV1 {
    reason: NvtFallbackReasonV1::ResourceLimit,
    diagnostic: Some(aeordb::engine::v4::reader::FormatError::new(
      MalformedInputClass::AllocationAmplification,
      "test_nvt_pressure",
      "candidate limit",
    )),
  };
  let resolved = resolve_nvt_lookup_v1(&NvtLookupRequestV1 {
    field: &field,
    target_coordinate: graph.target_coordinate,
    target_posting_position: &graph.target_key,
    attempt: NvtLookupAttemptV1::Fallback { basis: Some(&basis), cause: &pressure },
    exact_posting_path: Some(&posting_path),
    lookup_limits: lookup_limits(),
    healing_limits: default_nvt_healing_limits_v1(),
  })
  .unwrap();
  assert_eq!(resolved.anchor.unwrap().page_id, 11);
  let NvtHealingDispositionV1::Skipped(error) = resolved.healing else {
    panic!("query-local resource pressure is not a repair signal");
  };
  assert_eq!(error.class(), MalformedInputClass::AllocationAmplification);

  let mut corrupt_directory = graph.posting_directory.clone();
  let corrupt_offset = corrupt_directory.len() / 2;
  corrupt_directory[corrupt_offset] ^= 0x08;
  let corrupt_directories = [corrupt_directory.as_slice()];
  let corrupt_path = ImmutableIndexPathV1 { directories: &corrupt_directories, leaf: &graph.posting_pages[0] };
  let fallback = NvtFallbackV1 { reason: NvtFallbackReasonV1::Absent, diagnostic: None };
  assert_eq!(
    resolve_nvt_lookup_v1(&NvtLookupRequestV1 {
      field: &field,
      target_coordinate: graph.target_coordinate,
      target_posting_position: &graph.target_key,
      attempt: NvtLookupAttemptV1::Fallback { basis: None, cause: &fallback },
      exact_posting_path: Some(&corrupt_path),
      lookup_limits: lookup_limits(),
      healing_limits: default_nvt_healing_limits_v1(),
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::ChecksumOrIntegrityMismatch
  );
}

#[test]
fn deterministic_randomized_sparse_lookup_never_starts_after_the_independent_exact_predecessor() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for resolution in [65_536, 131_072] {
      for tile_cell_count in [256, 1_024, 4_096] {
        prove_randomized_lookup_model(hash_algorithm, resolution, tile_cell_count);
      }
    }
  }
}
