use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::gc_state::{
  GcDirectoryRoleV1, GcStateArtifactV1, GcStateDirectoryEntryWriteV1, GcStateDirectoryWriteV1, GcStatePageWriteV1,
  decode_gc_state_artifact, encode_gc_state_directory_v1, encode_gc_state_page_v1, validate_gc_directory_child,
};
use aeordb::engine::v4::gc_void::{
  SweepVoidArtifactV1, VoidCatalogManifestWriteV1, VoidClaimSettlementWriteV1, VoidClaimWriteV1, VoidExtentPageWriteV1,
  decode_sweep_void_artifact, encode_void_catalog_manifest_v1, encode_void_claim_settlement_v1, encode_void_claim_v1,
  encode_void_extent_page_v1, validate_void_manifest_root,
};
use aeordb::engine::v4::reader::MalformedInputClass;

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1")
}

fn fixture(profile: &str, name: &str) -> Vec<u8> {
  fs::read(fixture_root().join(format!("agca-{profile}-{name}.bin"))).unwrap()
}

fn algorithm(profile: &str) -> HashAlgorithm {
  match profile {
    "blake3-256" => HashAlgorithm::Blake3_256,
    "sha512" => HashAlgorithm::Sha512,
    _ => panic!("unexpected profile {profile}"),
  }
}

#[test]
fn every_frozen_void_writer_matches_both_independent_hash_widths() {
  for profile in ["blake3-256", "sha512"] {
    let algorithm = algorithm(profile);

    let source_page_bytes = fixture(profile, "void-extent-page-source");
    let SweepVoidArtifactV1::VoidExtentPage(source_page) = decode_sweep_void_artifact(&source_page_bytes, algorithm).unwrap() else {
      panic!("expected source Void extent page")
    };
    let extents = source_page.extent_records().unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    let database_id: [u8; 16] = source_page.database_id.try_into().unwrap();
    let catalog_id: [u8; 16] = source_page.catalog_id.try_into().unwrap();
    let encoded_page = encode_void_extent_page_v1(&VoidExtentPageWriteV1 {
      hash_algorithm: algorithm,
      database_id: &database_id,
      catalog_id: &catalog_id,
      generation: source_page.generation,
      page_id: source_page.page_id,
      extents: &extents,
    })
    .unwrap();
    assert_eq!(encoded_page.value, source_page_bytes, "{profile} extent page");
    assert_extent_page_fixture(profile, "void-extent-page-remaining", algorithm);

    assert_directory_fixture(profile, "void-free-directory-source", algorithm);
    assert_directory_fixture(profile, "void-free-directory-remaining", algorithm);

    let source_manifest_bytes = fixture(profile, "void-catalog-source");
    let SweepVoidArtifactV1::VoidCatalog(source_manifest) = decode_sweep_void_artifact(&source_manifest_bytes, algorithm).unwrap() else {
      panic!("expected source Void catalog")
    };
    let encoded_manifest = encode_void_catalog_manifest_v1(&VoidCatalogManifestWriteV1 {
      hash_algorithm: algorithm,
      database_id: &database_id,
      generation: source_manifest.generation,
      published_at_ms: source_manifest.published_at_ms,
      free_root: Some(source_manifest.free_root),
      claim_root: None,
      next_page_id: source_manifest.next_page_id,
      free_count: source_manifest.free_count,
      free_bytes: source_manifest.free_bytes,
      claim_count: source_manifest.claim_count,
      claimed_bytes: source_manifest.claimed_bytes,
      previous_control_sequence: source_manifest.previous_control_sequence,
    })
    .unwrap();
    assert_eq!(encoded_manifest.value, source_manifest_bytes, "{profile} source manifest");
    for name in ["void-catalog-empty", "void-catalog-outstanding", "void-catalog-settled"] {
      assert_manifest_fixture(profile, name, algorithm);
    }

    let claim_bytes = fixture(profile, "void-claim");
    let SweepVoidArtifactV1::VoidClaim(claim) = decode_sweep_void_artifact(&claim_bytes, algorithm).unwrap() else {
      panic!("expected Void claim")
    };
    let claim_id: [u8; 16] = claim.claim_id.try_into().unwrap();
    let requesting_boot_id: [u8; 16] = claim.requesting_boot_id.try_into().unwrap();
    let requesting_task_or_batch_id: [u8; 16] = claim.requesting_task_or_batch_id.try_into().unwrap();
    let claimed_extents = claim.extent_records().unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    let encoded_claim = encode_void_claim_v1(&VoidClaimWriteV1 {
      hash_algorithm: algorithm,
      database_id: &database_id,
      claim_id: &claim_id,
      generation: claim.generation,
      created_at_ms: claim.created_at_ms,
      requesting_boot_id: &requesting_boot_id,
      requesting_task_or_batch_id: &requesting_task_or_batch_id,
      source_manifest_hash: claim.source_manifest_hash,
      extents: &claimed_extents,
    })
    .unwrap();
    assert_eq!(encoded_claim.value, claim_bytes, "{profile} claim");

    assert_directory_fixture(profile, "void-claims-directory", algorithm);

    let settlement_bytes = fixture(profile, "void-claim-settlement");
    let SweepVoidArtifactV1::VoidClaimSettlement(settlement) = decode_sweep_void_artifact(&settlement_bytes, algorithm).unwrap() else {
      panic!("expected Void claim settlement")
    };
    let encoded_settlement = encode_void_claim_settlement_v1(&VoidClaimSettlementWriteV1 {
      hash_algorithm: algorithm,
      database_id: &database_id,
      claim_id: &claim_id,
      generation: settlement.generation,
      outcome: settlement.outcome,
      settled_at_ms: settlement.settled_at_ms,
      source_manifest_hash: settlement.source_manifest_hash,
      result_manifest_hash: settlement.result_manifest_hash,
      used_count: settlement.used_count,
      unused_count: settlement.unused_count,
      used_bytes: settlement.used_bytes,
      returned_bytes: settlement.returned_bytes,
      evidence_digest: settlement.evidence_digest,
    })
    .unwrap();
    assert_eq!(encoded_settlement.value, settlement_bytes, "{profile} settlement");
  }
}

fn assert_extent_page_fixture(profile: &str, name: &str, algorithm: HashAlgorithm) {
  let bytes = fixture(profile, name);
  let SweepVoidArtifactV1::VoidExtentPage(page) = decode_sweep_void_artifact(&bytes, algorithm).unwrap() else {
    panic!("expected Void extent page")
  };
  let extents = page.extent_records().unwrap().collect::<Result<Vec<_>, _>>().unwrap();
  let database_id: [u8; 16] = page.database_id.try_into().unwrap();
  let catalog_id: [u8; 16] = page.catalog_id.try_into().unwrap();
  let encoded = encode_void_extent_page_v1(&VoidExtentPageWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    catalog_id: &catalog_id,
    generation: page.generation,
    page_id: page.page_id,
    extents: &extents,
  })
  .unwrap();
  assert_eq!(encoded.value, bytes, "{profile} {name}");
}

fn assert_manifest_fixture(profile: &str, name: &str, algorithm: HashAlgorithm) {
  let bytes = fixture(profile, name);
  let SweepVoidArtifactV1::VoidCatalog(manifest) = decode_sweep_void_artifact(&bytes, algorithm).unwrap() else {
    panic!("expected Void catalog")
  };
  let database_id: [u8; 16] = manifest.database_id.try_into().unwrap();
  let encoded = encode_void_catalog_manifest_v1(&VoidCatalogManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    generation: manifest.generation,
    published_at_ms: manifest.published_at_ms,
    free_root: (!manifest.free_root.iter().all(|byte| *byte == 0)).then_some(manifest.free_root),
    claim_root: (!manifest.claim_root.iter().all(|byte| *byte == 0)).then_some(manifest.claim_root),
    next_page_id: manifest.next_page_id,
    free_count: manifest.free_count,
    free_bytes: manifest.free_bytes,
    claim_count: manifest.claim_count,
    claimed_bytes: manifest.claimed_bytes,
    previous_control_sequence: manifest.previous_control_sequence,
  })
  .unwrap();
  assert_eq!(encoded.value, bytes, "{profile} {name}");
}

fn assert_directory_fixture(profile: &str, name: &str, algorithm: HashAlgorithm) {
  let bytes = fixture(profile, name);
  let GcStateArtifactV1::Directory(directory) = decode_gc_state_artifact(&bytes, algorithm).unwrap() else {
    panic!("expected shared GC directory")
  };
  assert!(matches!(directory.role, GcDirectoryRoleV1::FreeExtents | GcDirectoryRoleV1::Claims));
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
      physical_hint: entry.physical_hint,
    })
    .collect::<Vec<_>>();
  let encoded = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role: directory.role,
    database_id: directory.database_id,
    catalog_id: directory.catalog_id,
    generation: directory.generation,
    level: directory.level,
    entries: &entries,
  })
  .unwrap();
  assert_eq!(encoded.value, bytes, "{profile} {name}");
}

#[test]
fn void_manifest_accepts_older_copy_on_write_roots_but_rejects_future_roots() {
  let algorithm = HashAlgorithm::Blake3_256;
  let manifest_bytes = fixture("blake3-256", "void-catalog-source");
  let directory_bytes = fixture("blake3-256", "void-free-directory-source");
  let SweepVoidArtifactV1::VoidCatalog(source_manifest) = decode_sweep_void_artifact(&manifest_bytes, algorithm).unwrap() else {
    panic!("expected source Void catalog")
  };
  let GcStateArtifactV1::Directory(source_directory) = decode_gc_state_artifact(&directory_bytes, algorithm).unwrap() else {
    panic!("expected source Void directory")
  };
  let database_id: [u8; 16] = source_manifest.database_id.try_into().unwrap();

  let manifest_generation = source_manifest.generation + 1;
  for (directory_generation, should_pass) in [(source_directory.generation, true), (manifest_generation + 1, false)] {
    let entries = source_directory
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
        physical_hint: entry.physical_hint,
      })
      .collect::<Vec<_>>();
    let directory = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
      hash_algorithm: algorithm,
      role: source_directory.role,
      database_id: source_directory.database_id,
      catalog_id: source_directory.catalog_id,
      generation: directory_generation,
      level: source_directory.level,
      entries: &entries,
    })
    .unwrap();
    let manifest = encode_void_catalog_manifest_v1(&VoidCatalogManifestWriteV1 {
      hash_algorithm: algorithm,
      database_id: &database_id,
      generation: manifest_generation,
      published_at_ms: source_manifest.published_at_ms,
      free_root: Some(&directory.key),
      claim_root: None,
      next_page_id: source_manifest.next_page_id,
      free_count: source_manifest.free_count,
      free_bytes: source_manifest.free_bytes,
      claim_count: 0,
      claimed_bytes: 0,
      previous_control_sequence: 1,
    })
    .unwrap();
    let decoded_manifest = decode_sweep_void_artifact(&manifest.value, algorithm).unwrap();
    let decoded_directory = decode_sweep_void_artifact(&directory.value, algorithm).unwrap();
    assert_eq!(validate_void_manifest_root(&decoded_manifest, &decoded_directory).is_ok(), should_pass);
  }
}

#[test]
fn shared_void_directory_supports_multiple_free_pages_and_multiple_claims() {
  for (role, page_backed) in [(GcDirectoryRoleV1::FreeExtents, true), (GcDirectoryRoleV1::Claims, false)] {
    let hash_a = [0x31; 32];
    let hash_b = [0x32; 32];
    let lower_a = if page_backed { 4_096u64.to_le_bytes().to_vec() } else { vec![0x11; 16] };
    let upper_a = if page_backed { 8_192u64.to_le_bytes().to_vec() } else { vec![0x22; 16] };
    let lower_b = if page_backed { 12_288u64.to_le_bytes().to_vec() } else { vec![0x33; 16] };
    let upper_b = if page_backed { 16_384u64.to_le_bytes().to_vec() } else { vec![0x44; 16] };
    let entries = [
      directory_entry(&lower_a, &upper_a, &hash_a, page_backed.then_some(41)),
      directory_entry(&lower_b, &upper_b, &hash_b, page_backed.then_some(42)),
    ];
    let encoded = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
      hash_algorithm: HashAlgorithm::Blake3_256,
      role,
      database_id: &[0x11; 16],
      catalog_id: &[0x22; 16],
      generation: 7,
      level: 0,
      entries: &entries,
    })
    .unwrap();
    let GcStateArtifactV1::Directory(decoded) = decode_gc_state_artifact(&encoded.value, HashAlgorithm::Blake3_256).unwrap() else {
      panic!("expected shared GC directory")
    };
    assert_eq!(decoded.entries.len(), 2);
    assert_eq!(decoded.page_count, if page_backed { 2 } else { 0 });
  }
}

#[test]
fn shared_void_directory_supports_nested_trees_for_both_roles() {
  for (role, page_backed) in [(GcDirectoryRoleV1::FreeExtents, true), (GcDirectoryRoleV1::Claims, false)] {
    let hash_a = [0x31; 32];
    let hash_b = [0x32; 32];
    let fences =
      if page_backed { [4_096u64.to_le_bytes().to_vec(), 8_192u64.to_le_bytes().to_vec()] } else { [vec![0x11; 16], vec![0x22; 16]] };
    let child_entries = [
      directory_entry(&fences[0], &fences[0], &hash_a, page_backed.then_some(41)),
      directory_entry(&fences[1], &fences[1], &hash_b, page_backed.then_some(42)),
    ];
    let child_encoded = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
      hash_algorithm: HashAlgorithm::Blake3_256,
      role,
      database_id: &[0x11; 16],
      catalog_id: &[0x22; 16],
      generation: 7,
      level: 0,
      entries: &child_entries,
    })
    .unwrap();
    let GcStateArtifactV1::Directory(child) = decode_gc_state_artifact(&child_encoded.value, HashAlgorithm::Blake3_256).unwrap() else {
      panic!("expected shared child directory")
    };
    let root_entries = [GcStateDirectoryEntryWriteV1 {
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
      physical_hint: aeordb::engine::v4::gc_state::GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    }];
    let root_encoded = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
      hash_algorithm: HashAlgorithm::Blake3_256,
      role,
      database_id: &[0x11; 16],
      catalog_id: &[0x22; 16],
      generation: 8,
      level: 1,
      entries: &root_entries,
    })
    .unwrap();
    let GcStateArtifactV1::Directory(root) = decode_gc_state_artifact(&root_encoded.value, HashAlgorithm::Blake3_256).unwrap() else {
      panic!("expected shared root directory")
    };
    validate_gc_directory_child(&root, &child).unwrap();
    assert_eq!(root.page_count, if page_backed { 2 } else { 0 });
    assert_eq!(root.live_count, 2);
  }
}

#[test]
fn specialized_void_roles_reject_generic_pages_and_wrong_directory_rank_shape() {
  for role in [GcDirectoryRoleV1::FreeExtents, GcDirectoryRoleV1::Claims] {
    let error = encode_gc_state_page_v1(&GcStatePageWriteV1 {
      hash_algorithm: HashAlgorithm::Blake3_256,
      role,
      database_id: &[0x11; 16],
      catalog_id: &[0x22; 16],
      generation: 7,
      page_id: 1,
      records: &[&[0u8; 1]],
    })
    .unwrap_err();
    assert_eq!(error.class(), MalformedInputClass::UnknownTypeKindOrEnum);
  }

  let hash = [0x31; 32];
  let free_fence = 4_096u64.to_le_bytes();
  let invalid_free = [directory_entry(&free_fence, &free_fence, &hash, None)];
  assert!(encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    role: GcDirectoryRoleV1::FreeExtents,
    database_id: &[0x11; 16],
    catalog_id: &[0x22; 16],
    generation: 7,
    level: 0,
    entries: &invalid_free,
  })
  .is_err());
  let claim_fence = [0x11; 16];
  let invalid_claim = [directory_entry(&claim_fence, &claim_fence, &hash, Some(41))];
  assert!(encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    role: GcDirectoryRoleV1::Claims,
    database_id: &[0x11; 16],
    catalog_id: &[0x22; 16],
    generation: 7,
    level: 0,
    entries: &invalid_claim,
  })
  .is_err());
}

#[test]
fn void_iterators_latch_malformed_rows_and_writers_reject_unsafe_shapes() {
  let algorithm = HashAlgorithm::Blake3_256;
  let page_bytes = fixture("blake3-256", "void-extent-page-source");
  let SweepVoidArtifactV1::VoidExtentPage(page) = decode_sweep_void_artifact(&page_bytes, algorithm).unwrap() else {
    panic!("expected Void extent page")
  };
  let mut malformed_page_records = page.records.to_vec();
  malformed_page_records[12] = 1;
  let malformed_page = aeordb::engine::v4::gc_void::VoidExtentPageV1 { records: &malformed_page_records, ..page.clone() };
  let mut page_records = malformed_page.extent_records().unwrap();
  assert_eq!(page_records.next().unwrap().unwrap_err().class(), MalformedInputClass::NonzeroReservedOrPadding);
  assert!(page_records.next().is_none());

  let claim_bytes = fixture("blake3-256", "void-claim");
  let SweepVoidArtifactV1::VoidClaim(claim) = decode_sweep_void_artifact(&claim_bytes, algorithm).unwrap() else {
    panic!("expected Void claim")
  };
  let mut malformed_claim_records = claim.extents.to_vec();
  malformed_claim_records[12] = 1;
  let malformed_claim = aeordb::engine::v4::gc_void::VoidClaimV1 { extents: &malformed_claim_records, ..claim.clone() };
  let mut claim_records = malformed_claim.extent_records().unwrap();
  assert_eq!(claim_records.next().unwrap().unwrap_err().class(), MalformedInputClass::NonzeroReservedOrPadding);
  assert!(claim_records.next().is_none());

  let extents = page.extent_records().unwrap().collect::<Result<Vec<_>, _>>().unwrap();
  let duplicate_extents = [extents[0], extents[0]];
  let database_id: [u8; 16] = page.database_id.try_into().unwrap();
  let catalog_id: [u8; 16] = page.catalog_id.try_into().unwrap();
  assert_eq!(
    encode_void_extent_page_v1(&VoidExtentPageWriteV1 {
      hash_algorithm: algorithm,
      database_id: &database_id,
      catalog_id: &catalog_id,
      generation: page.generation,
      page_id: page.page_id,
      extents: &duplicate_extents,
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );

  let zero_boot_id = [0u8; 16];
  let claim_id: [u8; 16] = claim.claim_id.try_into().unwrap();
  let task_id: [u8; 16] = claim.requesting_task_or_batch_id.try_into().unwrap();
  let claim_extents = claim.extent_records().unwrap().collect::<Result<Vec<_>, _>>().unwrap();
  assert_eq!(
    encode_void_claim_v1(&VoidClaimWriteV1 {
      hash_algorithm: algorithm,
      database_id: &database_id,
      claim_id: &claim_id,
      generation: claim.generation,
      created_at_ms: claim.created_at_ms,
      requesting_boot_id: &zero_boot_id,
      requesting_task_or_batch_id: &task_id,
      source_manifest_hash: claim.source_manifest_hash,
      extents: &claim_extents,
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );

  let catalog_bytes = fixture("blake3-256", "void-catalog-source");
  let SweepVoidArtifactV1::VoidCatalog(catalog) = decode_sweep_void_artifact(&catalog_bytes, algorithm).unwrap() else {
    panic!("expected Void catalog")
  };
  assert_eq!(
    encode_void_catalog_manifest_v1(&VoidCatalogManifestWriteV1 {
      hash_algorithm: algorithm,
      database_id: &database_id,
      generation: catalog.generation,
      published_at_ms: catalog.published_at_ms,
      free_root: Some(catalog.free_root),
      claim_root: None,
      next_page_id: catalog.next_page_id,
      free_count: 0,
      free_bytes: 0,
      claim_count: 0,
      claimed_bytes: 0,
      previous_control_sequence: catalog.previous_control_sequence,
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let settlement_bytes = fixture("blake3-256", "void-claim-settlement");
  let SweepVoidArtifactV1::VoidClaimSettlement(settlement) = decode_sweep_void_artifact(&settlement_bytes, algorithm).unwrap() else {
    panic!("expected Void claim settlement")
  };
  assert_eq!(
    encode_void_claim_settlement_v1(&VoidClaimSettlementWriteV1 {
      hash_algorithm: algorithm,
      database_id: &database_id,
      claim_id: &claim_id,
      generation: settlement.generation,
      outcome: settlement.outcome,
      settled_at_ms: settlement.settled_at_ms,
      source_manifest_hash: settlement.source_manifest_hash,
      result_manifest_hash: settlement.result_manifest_hash,
      used_count: 0,
      unused_count: settlement.unused_count,
      used_bytes: settlement.used_bytes,
      returned_bytes: settlement.returned_bytes,
      evidence_digest: settlement.evidence_digest,
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
}

fn directory_entry<'a>(
  lower_fence: &'a [u8],
  upper_fence: &'a [u8],
  child_hash: &'a [u8],
  page_id: Option<u64>,
) -> GcStateDirectoryEntryWriteV1<'a> {
  GcStateDirectoryEntryWriteV1 {
    lower_fence,
    upper_fence,
    child_hash,
    child_generation: 7,
    live_count: 1,
    tombstone_count: 0,
    page_count: u64::from(page_id.is_some()),
    logical_bytes: 512,
    minimum_page_id: page_id.unwrap_or(0),
    maximum_page_id: page_id.unwrap_or(0),
    physical_hint: aeordb::engine::v4::gc_state::GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
  }
}

#[test]
fn void_codec_architecture_has_one_directory_path_and_no_allocator_caller() {
  let void_source = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine/v4/gc_void.rs")).unwrap();
  assert!(!void_source.contains("fn decode_void_directory"));
  assert!(!void_source.contains("pub struct VoidDirectoryV1"));
  for forbidden in ["VoidManager", "StorageEngine", "replace_all", "find_void", "server::", "run_gc"] {
    assert!(!void_source.contains(forbidden), "P4-7a must not activate {forbidden}");
  }
}
