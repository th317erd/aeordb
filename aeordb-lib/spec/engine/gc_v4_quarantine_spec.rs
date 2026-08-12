use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::gc::PhysicalIncarnationV1;
use aeordb::engine::v4::gc_quarantine::{
  CandidateDeltaOperationV1, CandidateDeltaRecordWriteV1, CandidateDeltaWriteV1, PhysicalQuarantineCandidateClassV1,
  PhysicalQuarantineCandidateWriteV1, QuarantineClosureLimitsV1, QuarantineClosureValidatorV1, QuarantineManifestV1,
  QuarantineManifestWriteV1, decode_candidate_delta_v1, decode_physical_quarantine_candidate_v1, decode_quarantine_manifest_v1,
  encode_candidate_delta_v1, encode_physical_quarantine_candidate_v1, encode_quarantine_manifest_v1, quarantine_candidate_records_v1,
};
use aeordb::engine::v4::gc_state::{
  GcPhysicalHintV1, GcStateArtifactV1, GcStateDirectoryEntryWriteV1, GcStateDirectoryV1, GcStateDirectoryWriteV1, GcStateManifestV1,
  GcStatePageWriteV1, decode_gc_state_artifact, encode_gc_state_directory_v1, encode_gc_state_page_v1,
};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::reader::MalformedInputClass;
use tokio_util::sync::CancellationToken;

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1")
}

fn fixture(name: &str) -> Vec<u8> {
  fs::read(fixture_root().join(name)).unwrap()
}

fn algorithm_name(algorithm: HashAlgorithm) -> &'static str {
  match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => unreachable!("quarantine fixtures cover the two frozen hash widths"),
  }
}

fn memory_coordinator() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(16 * 1024 * 1024, 32 * 1024 * 1024, 1, 1024 * 1024).unwrap())
}

const fn closure_limits() -> QuarantineClosureLimitsV1 {
  QuarantineClosureLimitsV1 { maximum_support_artifacts: 1024 }
}

fn closure_validator<'a>(
  manifest: &'a QuarantineManifestV1<'a>,
  directory: Option<&'a GcStateDirectoryV1<'a>>,
  lifecycle: &GcStateManifestV1<'_>,
  algorithm: HashAlgorithm,
) -> QuarantineClosureValidatorV1<'a> {
  QuarantineClosureValidatorV1::new(
    manifest,
    directory,
    lifecycle,
    algorithm,
    CancellationToken::new(),
    closure_limits(),
    &memory_coordinator(),
  )
  .unwrap()
}

#[test]
fn quarantine_candidate_delta_and_manifest_codecs_match_both_width_independent_fixtures() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let name = algorithm_name(algorithm);
    let page_bytes = fixture(&format!("agca-{name}-candidate-page-valid.bin"));
    let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(&page_bytes, algorithm).unwrap() else {
      panic!("candidate fixture must decode as a page");
    };
    let candidates: Vec<_> = quarantine_candidate_records_v1(&page, algorithm).unwrap().map(Result::unwrap).collect();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].class, PhysicalQuarantineCandidateClassV1::UnreachableActiveLocator);
    assert_eq!(candidates[1].class, PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation);
    for candidate in &candidates {
      let encoded = encode_physical_quarantine_candidate_v1(&PhysicalQuarantineCandidateWriteV1::from(candidate)).unwrap();
      assert_eq!(encoded, candidate.encoded);
    }

    let delta_bytes = fixture(&format!("agca-{name}-candidate-delta-valid.bin"));
    let delta = decode_candidate_delta_v1(&delta_bytes, algorithm).unwrap();
    let records: Vec<_> = delta.records().unwrap().map(Result::unwrap).collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].operation, CandidateDeltaOperationV1::Set);
    assert_eq!(records[1].operation, CandidateDeltaOperationV1::Clear);
    let record_writes: Vec<_> = records.iter().map(CandidateDeltaRecordWriteV1::from).collect();
    let encoded_delta = encode_candidate_delta_v1(&CandidateDeltaWriteV1 {
      hash_algorithm: algorithm,
      database_id: delta.database_id.try_into().unwrap(),
      mark_generation: delta.mark_generation,
      delta_ordinal: delta.delta_ordinal,
      previous_delta_hash: delta.previous_delta_hash,
      records: &record_writes,
    })
    .unwrap();
    assert_eq!(encoded_delta.value, delta_bytes);
    assert_eq!(encoded_delta.key, delta.key);

    let manifest_bytes = fixture(&format!("agca-{name}-quarantine-manifest-populated.bin"));
    let manifest = decode_quarantine_manifest_v1(&manifest_bytes, algorithm).unwrap();
    assert_eq!(manifest.candidate_count, 2);
    assert_eq!(manifest.eligible_count_hint, 2);
    assert_eq!(manifest.delta_hashes.len(), algorithm.hash_length());
    let encoded_manifest = encode_quarantine_manifest_v1(&QuarantineManifestWriteV1::from_decoded(&manifest).unwrap()).unwrap();
    assert_eq!(encoded_manifest.value, manifest_bytes);
    assert_eq!(encoded_manifest.key, manifest.key);
  }
}

#[test]
fn quarantine_closure_validates_exact_base_delta_and_lifecycle_edges_without_collecting_candidates() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let name = algorithm_name(algorithm);
    let manifest_bytes = fixture(&format!("agca-{name}-quarantine-manifest-populated.bin"));
    let manifest = decode_quarantine_manifest_v1(&manifest_bytes, algorithm).unwrap();
    let directory_bytes = fixture(&format!("agca-{name}-candidates-directory-valid.bin"));
    let GcStateArtifactV1::Directory(directory) = decode_gc_state_artifact(&directory_bytes, algorithm).unwrap() else {
      panic!("candidate directory fixture must decode as a directory");
    };
    let page_bytes = fixture(&format!("agca-{name}-candidate-page-valid.bin"));
    let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(&page_bytes, algorithm).unwrap() else {
      panic!("candidate fixture must decode as a page");
    };
    let lifecycle_bytes = fixture(&format!("agca-{name}-root-lifecycle-manifest-populated.bin"));
    let GcStateArtifactV1::Manifest(lifecycle) = decode_gc_state_artifact(&lifecycle_bytes, algorithm).unwrap() else {
      panic!("lifecycle fixture must decode as a manifest");
    };
    let delta_bytes = fixture(&format!("agca-{name}-candidate-delta-valid.bin"));

    let mut validator = closure_validator(&manifest, Some(&directory), &lifecycle, algorithm);
    validator.observe_base_page(&page).unwrap();
    validator.observe_delta(&delta_bytes).unwrap();
    let summary = validator.finish().unwrap();

    assert_eq!(summary.base_page_count, 1);
    assert_eq!(summary.base_record_count, 2);
    assert_eq!(summary.base_logical_bytes, page.logical_bytes);
    assert_eq!(summary.declared_candidate_count, manifest.candidate_count);
    assert_eq!(summary.declared_candidate_bytes, manifest.candidate_bytes);
    assert_eq!(summary.delta_count, 1);
    assert_eq!(summary.delta_record_count, 2);
  }
}

#[test]
fn quarantine_graph_closure_does_not_confuse_compacted_base_totals_with_delta_overlay_totals() {
  let algorithm = HashAlgorithm::Blake3_256;
  let manifest_bytes = fixture("agca-blake3-256-quarantine-manifest-populated.bin");
  let mut manifest = decode_quarantine_manifest_v1(&manifest_bytes, algorithm).unwrap();
  let directory_bytes = fixture("agca-blake3-256-candidates-directory-valid.bin");
  let GcStateArtifactV1::Directory(directory) = decode_gc_state_artifact(&directory_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  let page_bytes = fixture("agca-blake3-256-candidate-page-valid.bin");
  let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(&page_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  let lifecycle_bytes = fixture("agca-blake3-256-root-lifecycle-manifest-populated.bin");
  let GcStateArtifactV1::Manifest(lifecycle) = decode_gc_state_artifact(&lifecycle_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  let delta_bytes = fixture("agca-blake3-256-candidate-delta-valid.bin");

  manifest.candidate_count = directory.live_count + 1;
  manifest.candidate_bytes = directory.logical_bytes + (52 + 2 * algorithm.hash_length()) as u64;
  manifest.eligible_count_hint = manifest.candidate_count;
  manifest.eligible_bytes_hint = manifest.candidate_bytes;

  let mut validator = closure_validator(&manifest, Some(&directory), &lifecycle, algorithm);
  validator.observe_base_page(&page).unwrap();
  validator.observe_delta(&delta_bytes).unwrap();
  let summary = validator.finish().unwrap();

  assert_eq!(summary.base_record_count, directory.live_count);
  assert_eq!(summary.base_logical_bytes, directory.logical_bytes);
  assert_eq!(summary.declared_candidate_count, manifest.candidate_count);
  assert_eq!(summary.declared_candidate_bytes, manifest.candidate_bytes);
}

#[test]
fn quarantine_graph_closure_accepts_nested_directories_only_in_exact_postorder() {
  let algorithm = HashAlgorithm::Blake3_256;
  let manifest_bytes = fixture("agca-blake3-256-quarantine-manifest-populated.bin");
  let mut manifest = decode_quarantine_manifest_v1(&manifest_bytes, algorithm).unwrap();
  let page_bytes = fixture("agca-blake3-256-candidate-page-valid.bin");
  let GcStateArtifactV1::Page(fixture_page) = decode_gc_state_artifact(&page_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  let row_length = 52 + 2 * algorithm.hash_length();
  let fixture_candidate = decode_physical_quarantine_candidate_v1(&fixture_page.records[..row_length], algorithm, false).unwrap();
  let rows: Vec<_> = [255, 256]
    .into_iter()
    .map(|wal_offset| {
      let mut write = PhysicalQuarantineCandidateWriteV1::from(&fixture_candidate);
      write.incarnation = PhysicalIncarnationV1 { wal_offset, entity_length: 1, ..fixture_candidate.incarnation };
      encode_physical_quarantine_candidate_v1(&write).unwrap()
    })
    .collect();
  let encoded_pages: Vec<_> = rows
    .iter()
    .enumerate()
    .map(|(index, row)| {
      encode_gc_state_page_v1(&GcStatePageWriteV1 {
        hash_algorithm: algorithm,
        role: fixture_page.role,
        database_id: fixture_page.database_id,
        catalog_id: fixture_page.catalog_id,
        generation: fixture_page.generation,
        page_id: u64::try_from(index + 1).unwrap(),
        records: &[row],
      })
      .unwrap()
    })
    .collect();
  let decoded_pages: Vec<_> = encoded_pages
    .iter()
    .map(|encoded| match decode_gc_state_artifact(&encoded.value, algorithm).unwrap() {
      GcStateArtifactV1::Page(page) => page,
      _ => unreachable!(),
    })
    .collect();
  let physical_hint = GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 };
  let encoded_leaf_directories: Vec<_> = decoded_pages
    .iter()
    .map(|page| {
      let entry = GcStateDirectoryEntryWriteV1 {
        lower_fence: page.lower_fence,
        upper_fence: page.upper_fence,
        child_hash: &page.key,
        child_generation: page.generation,
        live_count: u64::from(page.record_count),
        tombstone_count: 0,
        page_count: 1,
        logical_bytes: page.logical_bytes,
        minimum_page_id: page.page_id,
        maximum_page_id: page.page_id,
        physical_hint,
      };
      encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
        hash_algorithm: algorithm,
        role: fixture_page.role,
        database_id: fixture_page.database_id,
        catalog_id: fixture_page.catalog_id,
        generation: fixture_page.generation,
        level: 0,
        entries: &[entry],
      })
      .unwrap()
    })
    .collect();
  let decoded_leaf_directories: Vec<_> = encoded_leaf_directories
    .iter()
    .map(|encoded| match decode_gc_state_artifact(&encoded.value, algorithm).unwrap() {
      GcStateArtifactV1::Directory(directory) => directory,
      _ => unreachable!(),
    })
    .collect();
  let root_entries: Vec<_> = decoded_leaf_directories
    .iter()
    .map(|directory| GcStateDirectoryEntryWriteV1 {
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
      physical_hint,
    })
    .collect();
  let encoded_root = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role: fixture_page.role,
    database_id: fixture_page.database_id,
    catalog_id: fixture_page.catalog_id,
    generation: fixture_page.generation,
    level: 1,
    entries: &root_entries,
  })
  .unwrap();
  let GcStateArtifactV1::Directory(root) = decode_gc_state_artifact(&encoded_root.value, algorithm).unwrap() else {
    unreachable!();
  };
  let lifecycle_bytes = fixture("agca-blake3-256-root-lifecycle-manifest-populated.bin");
  let GcStateArtifactV1::Manifest(lifecycle) = decode_gc_state_artifact(&lifecycle_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  manifest.candidate_directory_root = Some(&root.key);
  manifest.next_candidate_page_id = 3;

  let mut validator = closure_validator(&manifest, Some(&root), &lifecycle, algorithm);
  for (page, directory) in decoded_pages.iter().zip(&decoded_leaf_directories) {
    validator.observe_base_page(page).unwrap();
    validator.observe_base_directory(directory).unwrap();
  }
  validator.observe_delta(&fixture("agca-blake3-256-candidate-delta-valid.bin")).unwrap();
  let summary = validator.finish().unwrap();
  assert_eq!((summary.base_page_count, summary.base_record_count), (2, 2));

  let mut wrong_order = closure_validator(&manifest, Some(&root), &lifecycle, algorithm);
  assert_eq!(wrong_order.observe_base_directory(&decoded_leaf_directories[0]).unwrap_err().code(), "quarantine_base_directory_order");
}

#[test]
fn quarantine_codecs_reject_wrong_classes_clear_state_edges_and_delta_chains() {
  let algorithm = HashAlgorithm::Blake3_256;
  let page_bytes = fixture("agca-blake3-256-candidate-page-valid.bin");
  let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(&page_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  let row_length = 52 + 2 * algorithm.hash_length();
  let mut candidate = page.records[..row_length].to_vec();
  let class_offset = 24 + 2 * algorithm.hash_length();
  candidate[class_offset..class_offset + 2].copy_from_slice(&8u16.to_le_bytes());
  assert_eq!(
    decode_physical_quarantine_candidate_v1(&candidate, algorithm, false).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let mut clear = page.records[..row_length].to_vec();
  assert_eq!(
    decode_physical_quarantine_candidate_v1(&clear, algorithm, true).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  clear[class_offset + 4..].fill(0);
  let decoded_clear = decode_physical_quarantine_candidate_v1(&clear, algorithm, true).unwrap();
  assert_eq!(decoded_clear.pending_since_ms, 0);

  let decoded_set = decode_physical_quarantine_candidate_v1(&page.records[..row_length], algorithm, false).unwrap();
  for class in [
    PhysicalQuarantineCandidateClassV1::UnreachableActiveLocator,
    PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation,
    PhysicalQuarantineCandidateClassV1::OrphanUncommittedIncarnation,
    PhysicalQuarantineCandidateClassV1::ExpiredDerivedArtifact,
    PhysicalQuarantineCandidateClassV1::ExpiredGcAuditArtifact,
    PhysicalQuarantineCandidateClassV1::ExpiredNamespaceRootClosure,
    PhysicalQuarantineCandidateClassV1::UnexplainedGapInventoryCandidate,
  ] {
    let mut write = PhysicalQuarantineCandidateWriteV1::from(&decoded_set);
    write.class = class;
    let encoded = encode_physical_quarantine_candidate_v1(&write).unwrap();
    assert_eq!(decode_physical_quarantine_candidate_v1(&encoded, algorithm, false).unwrap().class, class);
  }

  let clear_with_set_state = CandidateDeltaRecordWriteV1 {
    operation: CandidateDeltaOperationV1::Clear,
    candidate: PhysicalQuarantineCandidateWriteV1::from(&decoded_set),
  };
  let clear_error = encode_candidate_delta_v1(&CandidateDeltaWriteV1 {
    hash_algorithm: algorithm,
    database_id: [7; 16],
    mark_generation: 2,
    delta_ordinal: 1,
    previous_delta_hash: None,
    records: &[clear_with_set_state],
  })
  .unwrap_err();
  assert_eq!(clear_error.code(), "candidate_row_state");

  let delta_bytes = fixture("agca-blake3-256-candidate-delta-valid.bin");
  let mut wrong_predecessor = delta_bytes.clone();
  let body = 32 + 28;
  wrong_predecessor[body + 16..body + 16 + algorithm.hash_length()].copy_from_slice(&digest_parts(algorithm, &[b"unexpected predecessor"]));
  let crc = crc32fast::hash(&wrong_predecessor[..wrong_predecessor.len() - 4]);
  let crc_offset = wrong_predecessor.len() - 4;
  wrong_predecessor[crc_offset..].copy_from_slice(&crc.to_le_bytes());
  let decoded = decode_candidate_delta_v1(&wrong_predecessor, algorithm).unwrap();
  assert!(decoded.previous_delta_hash.is_some());
  assert!(matches!(
    decode_gc_state_artifact(&wrong_predecessor, algorithm).unwrap(),
    GcStateArtifactV1::CandidateDelta { record_count: 2, .. }
  ));

  let manifest_bytes = fixture("agca-blake3-256-quarantine-manifest-populated.bin");
  let mut manifest = decode_quarantine_manifest_v1(&manifest_bytes, algorithm).unwrap();
  let directory_bytes = fixture("agca-blake3-256-candidates-directory-valid.bin");
  let GcStateArtifactV1::Directory(directory) = decode_gc_state_artifact(&directory_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  let lifecycle_bytes = fixture("agca-blake3-256-root-lifecycle-manifest-populated.bin");
  let GcStateArtifactV1::Manifest(lifecycle) = decode_gc_state_artifact(&lifecycle_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  manifest.delta_hashes = &decoded.key;
  let mut validator = closure_validator(&manifest, Some(&directory), &lifecycle, algorithm);
  assert_eq!(validator.observe_delta(&wrong_predecessor).unwrap_err().code(), "quarantine_delta_predecessor");
}

#[test]
fn empty_quarantine_manifest_requires_no_candidate_graph_and_preserves_nonzero_lifecycle_basis() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let name = algorithm_name(algorithm);
    let manifest_bytes = fixture(&format!("agca-{name}-quarantine-manifest-empty.bin"));
    let manifest = decode_quarantine_manifest_v1(&manifest_bytes, algorithm).unwrap();
    assert!(manifest.candidate_directory_root.is_none());
    assert_eq!(manifest.candidate_count, 0);
    assert_eq!(manifest.candidate_bytes, 0);
    assert_eq!(manifest.delta_count, 0);
    assert!(!manifest.captured_root_lifecycle_manifest.iter().all(|byte| *byte == 0));

    let lifecycle_bytes = fixture(&format!("agca-{name}-root-lifecycle-manifest-empty.bin"));
    let GcStateArtifactV1::Manifest(lifecycle) = decode_gc_state_artifact(&lifecycle_bytes, algorithm).unwrap() else {
      unreachable!();
    };
    let summary = closure_validator(&manifest, None, &lifecycle, algorithm).finish().unwrap();
    assert_eq!(summary.base_page_count, 0);
    assert_eq!(summary.delta_count, 0);
  }
}

#[test]
fn quarantine_delta_only_closure_is_valid_but_missing_extra_and_failed_chains_are_not() {
  let algorithm = HashAlgorithm::Blake3_256;
  let manifest_bytes = fixture("agca-blake3-256-quarantine-manifest-populated.bin");
  let mut manifest = decode_quarantine_manifest_v1(&manifest_bytes, algorithm).unwrap();
  manifest.candidate_directory_root = None;
  let lifecycle_bytes = fixture("agca-blake3-256-root-lifecycle-manifest-populated.bin");
  let GcStateArtifactV1::Manifest(lifecycle) = decode_gc_state_artifact(&lifecycle_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  let delta_bytes = fixture("agca-blake3-256-candidate-delta-valid.bin");

  let mut valid = closure_validator(&manifest, None, &lifecycle, algorithm);
  valid.observe_delta(&delta_bytes).unwrap();
  let summary = valid.finish().unwrap();
  assert_eq!((summary.base_record_count, summary.declared_candidate_count, summary.delta_count), (0, 2, 1));

  let missing = closure_validator(&manifest, None, &lifecycle, algorithm);
  assert_eq!(missing.finish().unwrap_err().code(), "quarantine_closure_totals");

  let mut extra = closure_validator(&manifest, None, &lifecycle, algorithm);
  extra.observe_delta(&delta_bytes).unwrap();
  assert_eq!(extra.observe_delta(&delta_bytes).unwrap_err().code(), "quarantine_delta_count");
  assert_eq!(extra.observe_delta(&delta_bytes).unwrap_err().code(), "quarantine_closure_failed");
}

#[test]
fn quarantine_closure_rejects_wrong_lifecycle_missing_base_children_and_wrong_page_roles() {
  let algorithm = HashAlgorithm::Blake3_256;
  let manifest_bytes = fixture("agca-blake3-256-quarantine-manifest-populated.bin");
  let manifest = decode_quarantine_manifest_v1(&manifest_bytes, algorithm).unwrap();
  let directory_bytes = fixture("agca-blake3-256-candidates-directory-valid.bin");
  let GcStateArtifactV1::Directory(directory) = decode_gc_state_artifact(&directory_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  let lifecycle_bytes = fixture("agca-blake3-256-root-lifecycle-manifest-populated.bin");
  let GcStateArtifactV1::Manifest(lifecycle) = decode_gc_state_artifact(&lifecycle_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  let wrong_lifecycle_bytes = fixture("agca-blake3-256-root-lifecycle-manifest-empty.bin");
  let GcStateArtifactV1::Manifest(wrong_lifecycle) = decode_gc_state_artifact(&wrong_lifecycle_bytes, algorithm).unwrap() else {
    unreachable!();
  };

  assert_eq!(
    QuarantineClosureValidatorV1::new(
      &manifest,
      Some(&directory),
      &wrong_lifecycle,
      algorithm,
      CancellationToken::new(),
      closure_limits(),
      &memory_coordinator(),
    )
    .unwrap_err()
    .code(),
    "quarantine_lifecycle_basis"
  );
  assert_eq!(
    closure_validator(&manifest, Some(&directory), &lifecycle, algorithm).finish().unwrap_err().code(),
    "quarantine_base_directory_order"
  );

  let wrong_page_bytes = fixture("agca-blake3-256-root-candidate-page-valid.bin");
  let GcStateArtifactV1::Page(wrong_page) = decode_gc_state_artifact(&wrong_page_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  let mut validator = closure_validator(&manifest, Some(&directory), &lifecycle, algorithm);
  assert_eq!(validator.observe_base_page(&wrong_page).unwrap_err().code(), "quarantine_base_page");
}

#[test]
fn quarantine_closure_attributes_memory_observes_cancellation_and_latches_failures() {
  let algorithm = HashAlgorithm::Blake3_256;
  let manifest_bytes = fixture("agca-blake3-256-quarantine-manifest-populated.bin");
  let manifest = decode_quarantine_manifest_v1(&manifest_bytes, algorithm).unwrap();
  let directory_bytes = fixture("agca-blake3-256-candidates-directory-valid.bin");
  let GcStateArtifactV1::Directory(directory) = decode_gc_state_artifact(&directory_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  let page_bytes = fixture("agca-blake3-256-candidate-page-valid.bin");
  let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(&page_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  let lifecycle_bytes = fixture("agca-blake3-256-root-lifecycle-manifest-populated.bin");
  let GcStateArtifactV1::Manifest(lifecycle) = decode_gc_state_artifact(&lifecycle_bytes, algorithm).unwrap() else {
    unreachable!();
  };

  assert_eq!(
    QuarantineClosureValidatorV1::new(
      &manifest,
      Some(&directory),
      &lifecycle,
      algorithm,
      CancellationToken::new(),
      QuarantineClosureLimitsV1 { maximum_support_artifacts: 0 },
      &memory_coordinator(),
    )
    .unwrap_err()
    .code(),
    "quarantine_closure_configuration"
  );

  let canceled = CancellationToken::new();
  canceled.cancel();
  assert_eq!(
    QuarantineClosureValidatorV1::new(
      &manifest,
      Some(&directory),
      &lifecycle,
      algorithm,
      canceled,
      closure_limits(),
      &memory_coordinator(),
    )
    .unwrap_err()
    .code(),
    "quarantine_closure_canceled"
  );

  let cancellation = CancellationToken::new();
  let mut canceled_mid_pass = QuarantineClosureValidatorV1::new(
    &manifest,
    Some(&directory),
    &lifecycle,
    algorithm,
    cancellation.clone(),
    closure_limits(),
    &memory_coordinator(),
  )
  .unwrap();
  cancellation.cancel();
  assert_eq!(canceled_mid_pass.observe_base_page(&page).unwrap_err().code(), "quarantine_closure_canceled");
  assert_eq!(canceled_mid_pass.observe_base_page(&page).unwrap_err().code(), "quarantine_closure_failed");

  let constrained_memory = MemoryCoordinator::new(MemoryPolicy::new(64, 128, 1, 16).unwrap());
  let mut memory_limited = QuarantineClosureValidatorV1::new(
    &manifest,
    Some(&directory),
    &lifecycle,
    algorithm,
    CancellationToken::new(),
    closure_limits(),
    &constrained_memory,
  )
  .unwrap();
  assert_eq!(memory_limited.observe_base_page(&page).unwrap_err().code(), "quarantine_closure_memory");
  assert_eq!(memory_limited.observe_base_page(&page).unwrap_err().code(), "quarantine_closure_failed");
  drop(memory_limited);
  let constrained_snapshot = constrained_memory.snapshot().unwrap();
  let constrained_gc = constrained_snapshot.owners.iter().find(|owner| owner.owner == MemoryOwner::GarbageCollection).unwrap();
  assert_eq!((constrained_gc.reserved_bytes, constrained_gc.active_reservations), (0, 0));

  let mut artifact_limited = QuarantineClosureValidatorV1::new(
    &manifest,
    Some(&directory),
    &lifecycle,
    algorithm,
    CancellationToken::new(),
    QuarantineClosureLimitsV1 { maximum_support_artifacts: 2 },
    &memory_coordinator(),
  )
  .unwrap();
  artifact_limited.observe_base_page(&page).unwrap();
  assert_eq!(
    artifact_limited.observe_delta(&fixture("agca-blake3-256-candidate-delta-valid.bin")).unwrap_err().code(),
    "quarantine_closure_artifact_limit"
  );

  let successful_memory = memory_coordinator();
  let mut successful = QuarantineClosureValidatorV1::new(
    &manifest,
    Some(&directory),
    &lifecycle,
    algorithm,
    CancellationToken::new(),
    closure_limits(),
    &successful_memory,
  )
  .unwrap();
  successful.observe_base_page(&page).unwrap();
  successful.observe_delta(&fixture("agca-blake3-256-candidate-delta-valid.bin")).unwrap();
  successful.finish().unwrap();
  let successful_snapshot = successful_memory.snapshot().unwrap();
  let successful_gc = successful_snapshot.owners.iter().find(|owner| owner.owner == MemoryOwner::GarbageCollection).unwrap();
  assert_eq!((successful_gc.reserved_bytes, successful_gc.active_reservations), (0, 0));
}

#[test]
fn quarantine_readers_reject_wrong_width_reserved_state_and_oversized_manifest_input() {
  let algorithm = HashAlgorithm::Blake3_256;
  let page_bytes = fixture("agca-blake3-256-candidate-page-valid.bin");
  let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(&page_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  let row_length = 52 + 2 * algorithm.hash_length();
  let row = &page.records[..row_length];
  assert_eq!(
    decode_physical_quarantine_candidate_v1(row, HashAlgorithm::Sha512, false).unwrap_err().class(),
    MalformedInputClass::TruncationOrTrailingBytes
  );

  let mut reserved = row.to_vec();
  let flags_offset = 24 + 2 * algorithm.hash_length() + 2;
  reserved[flags_offset] = 1;
  assert_eq!(
    decode_physical_quarantine_candidate_v1(&reserved, algorithm, false).unwrap_err().class(),
    MalformedInputClass::NonzeroReservedOrPadding
  );

  let oversized = vec![0; 1_024 * 1_024 + 1];
  assert_eq!(decode_quarantine_manifest_v1(&oversized, algorithm).unwrap_err().class(), MalformedInputClass::AllocationAmplification);
}
