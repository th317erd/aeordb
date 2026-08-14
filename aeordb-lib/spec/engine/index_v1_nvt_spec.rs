use std::fs;
use std::path::PathBuf;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::index_nvt::{
  NvtEntryWriteV1, NvtPostingPageSampleV1, NvtTileWriteV1, SparseNvtBuildLimitsV1, SparseNvtBuildRequestV1, build_sparse_nvt_tiles_v1,
  coordinate_cell, decode_nvt_tile, encode_nvt_tile,
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
