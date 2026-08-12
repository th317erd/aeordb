use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::gc_state::{
  GcDirectoryRoleV1, GcPhysicalHintV1, GcStateArtifactV1, GcStateDirectoryEntryWriteV1, GcStateDirectoryWriteV1, GcStatePageWriteV1,
  decode_gc_state_artifact, encode_gc_state_directory_v1, encode_gc_state_page_v1, validate_gc_directory_child,
};

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1")
}

fn algorithm_name(algorithm: HashAlgorithm) -> &'static str {
  match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => unreachable!("GC state writer fixtures cover both frozen hash widths"),
  }
}

fn fixture(algorithm: HashAlgorithm, name: &str) -> Vec<u8> {
  fs::read(fixture_root().join(format!("agca-{}-{name}.bin", algorithm_name(algorithm)))).unwrap()
}

fn page_and_directory_names(role: GcDirectoryRoleV1) -> (&'static str, &'static str) {
  match role {
    GcDirectoryRoleV1::Candidates => ("candidate-page-valid", "candidates-directory-valid"),
    GcDirectoryRoleV1::PhysicalInventory => ("physical-inventory-page-valid", "physical-inventory-directory-valid"),
    GcDirectoryRoleV1::RootCandidates => ("root-candidate-page-valid", "root-candidates-directory-valid"),
    GcDirectoryRoleV1::RootExpiry => ("root-expiry-page-valid", "root-expiry-directory-valid"),
    GcDirectoryRoleV1::FreeExtents | GcDirectoryRoleV1::Claims => panic!("specialized Void roles do not use generic GC pages"),
  }
}

fn row_length(algorithm: HashAlgorithm, role: GcDirectoryRoleV1) -> usize {
  match role {
    GcDirectoryRoleV1::Candidates => 52 + 2 * algorithm.hash_length(),
    GcDirectoryRoleV1::PhysicalInventory => 68 + 5 * algorithm.hash_length(),
    GcDirectoryRoleV1::RootCandidates => 36 + 3 * algorithm.hash_length(),
    GcDirectoryRoleV1::RootExpiry => 40 + 3 * algorithm.hash_length(),
    GcDirectoryRoleV1::FreeExtents | GcDirectoryRoleV1::Claims => panic!("specialized Void roles do not use generic GC rows"),
  }
}

#[test]
fn exact_gc_page_and_directory_writers_match_independent_lifecycle_fixtures() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for role in [
      GcDirectoryRoleV1::Candidates,
      GcDirectoryRoleV1::RootExpiry,
      GcDirectoryRoleV1::PhysicalInventory,
      GcDirectoryRoleV1::RootCandidates,
    ] {
      let (page_name, directory_name) = page_and_directory_names(role);
      let expected_page = fixture(algorithm, page_name);
      let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(&expected_page, algorithm).unwrap() else {
        unreachable!();
      };
      let records = page.records.chunks_exact(row_length(algorithm, role)).collect::<Vec<_>>();
      let encoded_page = encode_gc_state_page_v1(&GcStatePageWriteV1 {
        hash_algorithm: algorithm,
        role,
        database_id: page.database_id,
        catalog_id: page.catalog_id,
        generation: page.generation,
        page_id: page.page_id,
        records: &records,
      })
      .unwrap();
      assert_eq!(encoded_page.value, expected_page);

      let expected_directory = fixture(algorithm, directory_name);
      let GcStateArtifactV1::Directory(directory) = decode_gc_state_artifact(&expected_directory, algorithm).unwrap() else {
        unreachable!();
      };
      let entries = directory
        .entries
        .iter()
        .map(|entry| GcStateDirectoryEntryWriteV1 {
          lower_fence: entry.lower_fence,
          upper_fence: entry.upper_fence,
          child_hash: entry.child_hash,
          child_generation: entry.child_generation,
          live_count: entry.live_count,
          tombstone_count: entry.tombstone_count,
          page_count: entry.page_count,
          logical_bytes: entry.logical_bytes,
          minimum_page_id: entry.minimum_page_id,
          maximum_page_id: entry.maximum_page_id,
          physical_hint: GcPhysicalHintV1 {
            wal_offset: entry.physical_hint.wal_offset,
            total_length: entry.physical_hint.total_length,
            write_sequence: entry.physical_hint.write_sequence,
          },
        })
        .collect::<Vec<_>>();
      let encoded_directory = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
        hash_algorithm: algorithm,
        role,
        database_id: directory.database_id,
        catalog_id: directory.catalog_id,
        generation: directory.generation,
        level: directory.level,
        entries: &entries,
      })
      .unwrap();
      assert_eq!(encoded_directory.value, expected_directory);
    }
  }
}

#[test]
fn page_writer_rejects_empty_unsorted_and_cross_role_rows() {
  let algorithm = HashAlgorithm::Blake3_256;
  let expected_page = fixture(algorithm, "root-expiry-page-valid");
  let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(&expected_page, algorithm).unwrap() else {
    unreachable!();
  };
  let records = page.records.chunks_exact(row_length(algorithm, page.role)).collect::<Vec<_>>();
  let empty = encode_gc_state_page_v1(&GcStatePageWriteV1 {
    hash_algorithm: algorithm,
    role: page.role,
    database_id: page.database_id,
    catalog_id: page.catalog_id,
    generation: page.generation,
    page_id: page.page_id,
    records: &[],
  })
  .unwrap_err();
  assert_eq!(empty.code(), "gc_page_records");

  let reversed = [records[1], records[0]];
  let unsorted = encode_gc_state_page_v1(&GcStatePageWriteV1 {
    hash_algorithm: algorithm,
    role: page.role,
    database_id: page.database_id,
    catalog_id: page.catalog_id,
    generation: page.generation,
    page_id: page.page_id,
    records: &reversed,
  })
  .unwrap_err();
  assert_eq!(unsorted.code(), "gc_page_record_order");

  let candidate_page = fixture(algorithm, "root-candidate-page-valid");
  let GcStateArtifactV1::Page(candidate_page) = decode_gc_state_artifact(&candidate_page, algorithm).unwrap() else {
    unreachable!();
  };
  let cross_role = [candidate_page.records];
  let error = encode_gc_state_page_v1(&GcStatePageWriteV1 {
    hash_algorithm: algorithm,
    role: GcDirectoryRoleV1::RootExpiry,
    database_id: candidate_page.database_id,
    catalog_id: candidate_page.catalog_id,
    generation: candidate_page.generation,
    page_id: candidate_page.page_id,
    records: &cross_role,
  })
  .unwrap_err();
  assert_eq!(error.code(), "root_expiry_row");
}

#[test]
fn directory_writer_rejects_empty_overlapping_and_malformed_children() {
  let algorithm = HashAlgorithm::Blake3_256;
  let expected_directory = fixture(algorithm, "root-expiry-directory-valid");
  let GcStateArtifactV1::Directory(directory) = decode_gc_state_artifact(&expected_directory, algorithm).unwrap() else {
    unreachable!();
  };
  let empty = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role: directory.role,
    database_id: directory.database_id,
    catalog_id: directory.catalog_id,
    generation: directory.generation,
    level: directory.level,
    entries: &[],
  })
  .unwrap_err();
  assert_eq!(empty.code(), "gc_directory_entries");

  let source = &directory.entries[0];
  let valid_entry = || GcStateDirectoryEntryWriteV1 {
    lower_fence: source.lower_fence,
    upper_fence: source.upper_fence,
    child_hash: source.child_hash,
    child_generation: source.child_generation,
    live_count: source.live_count,
    tombstone_count: source.tombstone_count,
    page_count: source.page_count,
    logical_bytes: source.logical_bytes,
    minimum_page_id: source.minimum_page_id,
    maximum_page_id: source.maximum_page_id,
    physical_hint: GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
  };
  let overlapping = [valid_entry(), valid_entry()];
  let error = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role: directory.role,
    database_id: directory.database_id,
    catalog_id: directory.catalog_id,
    generation: directory.generation,
    level: directory.level,
    entries: &overlapping,
  })
  .unwrap_err();
  assert_eq!(error.code(), "gc_directory_child_order");

  let zero_hash = [0u8; 32];
  let malformed = [GcStateDirectoryEntryWriteV1 { child_hash: &zero_hash, ..valid_entry() }];
  let error = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role: directory.role,
    database_id: directory.database_id,
    catalog_id: directory.catalog_id,
    generation: directory.generation,
    level: directory.level,
    entries: &malformed,
  })
  .unwrap_err();
  assert_eq!(error.code(), "gc_directory_leaf");
}

#[test]
fn directory_writer_builds_a_multi_child_internal_tree_without_flattening_pages() {
  let algorithm = HashAlgorithm::Blake3_256;
  let source_page = fixture(algorithm, "root-candidate-page-valid");
  let GcStateArtifactV1::Page(source_page) = decode_gc_state_artifact(&source_page, algorithm).unwrap() else {
    unreachable!();
  };
  let first_row = source_page.records.to_vec();
  let mut second_row = first_row.clone();
  second_row[..algorithm.hash_length()].fill(0xf1);

  let mut leaf_directories = Vec::new();
  for (page_id, row) in [(41, first_row.as_slice()), (42, second_row.as_slice())] {
    let rows = [row];
    let page = encode_gc_state_page_v1(&GcStatePageWriteV1 {
      hash_algorithm: algorithm,
      role: GcDirectoryRoleV1::RootCandidates,
      database_id: source_page.database_id,
      catalog_id: source_page.catalog_id,
      generation: source_page.generation,
      page_id,
      records: &rows,
    })
    .unwrap();
    let GcStateArtifactV1::Page(decoded_page) = decode_gc_state_artifact(&page.value, algorithm).unwrap() else {
      unreachable!();
    };
    let leaf_entries = [GcStateDirectoryEntryWriteV1 {
      lower_fence: decoded_page.lower_fence,
      upper_fence: decoded_page.upper_fence,
      child_hash: &page.key,
      child_generation: decoded_page.generation,
      live_count: u64::from(decoded_page.record_count),
      tombstone_count: 0,
      page_count: 1,
      logical_bytes: decoded_page.logical_bytes,
      minimum_page_id: decoded_page.page_id,
      maximum_page_id: decoded_page.page_id,
      physical_hint: GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    }];
    leaf_directories.push(
      encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
        hash_algorithm: algorithm,
        role: decoded_page.role,
        database_id: decoded_page.database_id,
        catalog_id: decoded_page.catalog_id,
        generation: decoded_page.generation + 10,
        level: 0,
        entries: &leaf_entries,
      })
      .unwrap(),
    );
  }

  let decoded_leaf_directories = leaf_directories
    .iter()
    .map(|encoded| match decode_gc_state_artifact(&encoded.value, algorithm).unwrap() {
      GcStateArtifactV1::Directory(directory) => directory,
      _ => unreachable!(),
    })
    .collect::<Vec<_>>();
  let internal_entries = decoded_leaf_directories
    .iter()
    .zip(&leaf_directories)
    .map(|(directory, encoded)| GcStateDirectoryEntryWriteV1 {
      lower_fence: directory.lower_fence,
      upper_fence: directory.upper_fence,
      child_hash: &encoded.key,
      child_generation: directory.generation,
      live_count: directory.live_count,
      tombstone_count: directory.tombstone_count,
      page_count: directory.page_count,
      logical_bytes: directory.logical_bytes,
      minimum_page_id: directory.minimum_page_id,
      maximum_page_id: directory.maximum_page_id,
      physical_hint: GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    })
    .collect::<Vec<_>>();
  let root = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role: GcDirectoryRoleV1::RootCandidates,
    database_id: source_page.database_id,
    catalog_id: source_page.catalog_id,
    generation: source_page.generation + 20,
    level: 1,
    entries: &internal_entries,
  })
  .unwrap();
  let GcStateArtifactV1::Directory(root) = decode_gc_state_artifact(&root.value, algorithm).unwrap() else {
    unreachable!();
  };
  assert_eq!((root.level, root.entries.len(), root.page_count, root.live_count), (1, 2, 2, 2));
  for child in &decoded_leaf_directories {
    validate_gc_directory_child(&root, child).unwrap();
  }
}
