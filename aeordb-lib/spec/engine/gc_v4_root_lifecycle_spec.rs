use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::gc::{GcArtifactKindV1, immutable_gc_artifact_key};
use aeordb::engine::v4::gc_lifecycle::{
  RootCandidateRecordWriteV1, RootExpiryManifestWriteV1, RootExpiryRecordWriteV1, RootLifecycleManifestWriteV1,
  RootLifecycleReferenceModelV1, RootObjectReclaimProofWriteV1, RootRetirementCommitWriteV1, decode_root_expiry_manifest_v1,
  decode_root_lifecycle_manifest_v1, decode_root_object_reclaim_proof_v1, decode_root_retirement_commit_v1,
  encode_root_candidate_record_v1, encode_root_expiry_manifest_v1, encode_root_expiry_record_v1, encode_root_lifecycle_manifest_v1,
  encode_root_object_reclaim_proof_v1, encode_root_retirement_commit_v1, validate_root_expiry_manifest_directory,
  validate_root_expiry_reclaim_proof, validate_root_expiry_retirement_commit, validate_root_lifecycle_candidate_directory,
  validate_root_lifecycle_expiry_manifest,
};
use aeordb::engine::v4::gc_state::{
  GcStateArtifactV1, RootExpiryStateV1, decode_gc_state_artifact, decode_root_candidate_record_v1, decode_root_expiry_record_v1,
  validate_gc_directory_page,
};
use tokio_util::sync::CancellationToken;

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1")
}

fn fixture(algorithm: HashAlgorithm, name: &str) -> Vec<u8> {
  fs::read(fixture_root().join(format!("agca-{}-{name}.bin", algorithm_name(algorithm)))).unwrap()
}

fn algorithm_name(algorithm: HashAlgorithm) -> &'static str {
  match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => unreachable!("root lifecycle fixtures cover both frozen hash widths"),
  }
}

fn expected_sequence(length: usize, start: u8) -> Vec<u8> {
  (0..length).map(|index| start.wrapping_add(index as u8)).collect()
}

fn fixture_key(algorithm: HashAlgorithm, kind: GcArtifactKindV1, name: &str) -> Vec<u8> {
  immutable_gc_artifact_key(algorithm, kind, &fixture(algorithm, name))
}

#[test]
fn complete_lifecycle_readers_expose_every_frozen_graph_edge_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let hash_width = algorithm.hash_length();
    let lifecycle_bytes = fixture(algorithm, "root-lifecycle-manifest-populated");
    let lifecycle = decode_root_lifecycle_manifest_v1(&lifecycle_bytes, algorithm).unwrap();
    assert_eq!(lifecycle.generation, 612);
    assert_eq!(lifecycle.published_at_ms, 1_700_000_070_001);
    assert_eq!(lifecycle.source_complete_mark_generation, 602);
    assert_eq!(lifecycle.authority_root_set_digest, expected_sequence(hash_width, 0xe1));
    assert_eq!(
      lifecycle.candidate_directory_hash,
      Some(fixture_key(algorithm, GcArtifactKindV1::GcArtifactDirectoryNode, "root-candidates-directory-valid").as_slice()),
    );
    assert_eq!(
      lifecycle.root_expiry_manifest_hash,
      Some(fixture_key(algorithm, GcArtifactKindV1::RootExpiryCatalogManifest, "root-expiry-catalog-manifest-populated").as_slice()),
    );
    assert_eq!((lifecycle.candidate_count, lifecycle.pending_count, lifecycle.retired_evidence_count), (1, 1, 2));

    let expiry_bytes = fixture(algorithm, "root-expiry-catalog-manifest-populated");
    let expiry = decode_root_expiry_manifest_v1(&expiry_bytes, algorithm).unwrap();
    assert_eq!(expiry.retention_ms, 90 * 24 * 60 * 60 * 1_000);
    assert_eq!(expiry.optional_byte_budget, 64 * 1024 * 1024);
    assert_eq!(
      expiry.directory_root_hash,
      Some(fixture_key(algorithm, GcArtifactKindV1::GcArtifactDirectoryNode, "root-expiry-directory-valid").as_slice()),
    );
    assert_eq!((expiry.record_count, expiry.mandatory_count, expiry.optional_count), (2, 1, 1));
    assert_eq!((expiry.oldest_retired_at_ms, expiry.newest_retired_at_ms), (Some(1_700_000_010_001), Some(1_700_000_010_002)));

    let retirement_bytes = fixture(algorithm, "root-retirement-commit-valid");
    let retirement = decode_root_retirement_commit_v1(&retirement_bytes, algorithm).unwrap();
    assert_eq!(retirement.committed_at_ms, 1_700_000_080_000);
    assert_eq!(retirement.pending_since_ms, 1_700_000_060_000);
    assert_eq!(retirement.grace_at_pending_ms, 10_000);
    assert_eq!(retirement.final_mark_generation, 501);
    assert_eq!(
      retirement.prior_lifecycle_manifest_hash,
      fixture_key(algorithm, GcArtifactKindV1::RootLifecycleManifest, "root-lifecycle-manifest-empty"),
    );
    assert_eq!(retirement.authority_root_set_digest, expected_sequence(hash_width, 0xe1));
    assert_eq!(retirement.admission_commit_payload_hash, expected_sequence(hash_width, 0xd1));

    let proof_bytes = fixture(algorithm, "root-object-reclaim-proof-valid");
    let proof = decode_root_object_reclaim_proof_v1(&proof_bytes, algorithm).unwrap();
    assert_eq!(proof.reclaimed_at_ms, 1_700_000_090_000);
    assert_eq!(
      proof.retirement_commit_hash,
      fixture_key(algorithm, GcArtifactKindV1::RootRetirementCommit, "root-retirement-commit-valid")
    );
    assert_eq!(
      proof.physical_inventory_manifest_hash,
      fixture_key(algorithm, GcArtifactKindV1::PhysicalInventoryManifest, "physical-inventory-manifest-populated"),
    );
    assert_eq!((proof.root_object_incarnation_count, proof.sweep_receipt_count), (1, 1));
  }
}

#[test]
fn lifecycle_encoders_match_the_independent_fixtures_exactly() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let candidate_page = fixture(algorithm, "root-candidate-page-valid");
    let GcStateArtifactV1::Page(candidate_page) = decode_gc_state_artifact(&candidate_page, algorithm).unwrap() else {
      unreachable!();
    };
    let candidate = decode_root_candidate_record_v1(candidate_page.records, algorithm).unwrap();
    assert_eq!(
      encode_root_candidate_record_v1(&RootCandidateRecordWriteV1 {
        hash_algorithm: algorithm,
        namespace_root_hash: candidate.namespace_root_hash,
        reason: candidate.reason,
        pending_since_ms: candidate.pending_since_ms,
        first_unreachable_generation: candidate.first_unreachable_generation,
        last_confirmed_unreachable_generation: candidate.last_confirmed_unreachable_generation,
        grace_at_pending_ms: candidate.grace_at_pending_ms,
        authority_root_set_digest: candidate.authority_root_set_digest,
        admission_commit_payload_hash: candidate.admission_commit_payload_hash,
      })
      .unwrap(),
      candidate_page.records,
    );

    let expiry_page = fixture(algorithm, "root-expiry-page-valid");
    let GcStateArtifactV1::Page(expiry_page) = decode_gc_state_artifact(&expiry_page, algorithm).unwrap() else {
      unreachable!();
    };
    let expiry_row_length = 40 + 3 * algorithm.hash_length();
    for row in expiry_page.records.chunks_exact(expiry_row_length) {
      let expiry = decode_root_expiry_record_v1(row, algorithm).unwrap();
      assert_eq!(
        encode_root_expiry_record_v1(&RootExpiryRecordWriteV1 {
          hash_algorithm: algorithm,
          namespace_root_hash: expiry.namespace_root_hash,
          retired_at_ms: expiry.retired_at_ms,
          last_pending_since_ms: expiry.last_pending_since_ms,
          final_mark_generation: expiry.final_mark_generation,
          reason: expiry.reason,
          state: expiry.state,
          retirement_commit_hash: expiry.retirement_commit_hash,
          root_object_reclaim_proof_hash: expiry.root_object_reclaim_proof_hash,
          evidence_expires_at_ms: expiry.evidence_expires_at_ms,
        })
        .unwrap(),
        row,
      );
    }

    let expiry_bytes = fixture(algorithm, "root-expiry-catalog-manifest-populated");
    let expiry = decode_root_expiry_manifest_v1(&expiry_bytes, algorithm).unwrap();
    assert_eq!(encode_root_expiry_manifest_v1(&RootExpiryManifestWriteV1::from_decoded(algorithm, &expiry)).unwrap().value, expiry_bytes,);

    let lifecycle_bytes = fixture(algorithm, "root-lifecycle-manifest-populated");
    let lifecycle = decode_root_lifecycle_manifest_v1(&lifecycle_bytes, algorithm).unwrap();
    assert_eq!(
      encode_root_lifecycle_manifest_v1(&RootLifecycleManifestWriteV1::from_decoded(algorithm, &lifecycle)).unwrap().value,
      lifecycle_bytes,
    );

    let retirement_bytes = fixture(algorithm, "root-retirement-commit-valid");
    let retirement = decode_root_retirement_commit_v1(&retirement_bytes, algorithm).unwrap();
    assert_eq!(
      encode_root_retirement_commit_v1(&RootRetirementCommitWriteV1::from_decoded(algorithm, &retirement)).unwrap().value,
      retirement_bytes,
    );

    let proof_bytes = fixture(algorithm, "root-object-reclaim-proof-valid");
    let proof = decode_root_object_reclaim_proof_v1(&proof_bytes, algorithm).unwrap();
    assert_eq!(
      encode_root_object_reclaim_proof_v1(&RootObjectReclaimProofWriteV1::from_decoded(algorithm, &proof)).unwrap().value,
      proof_bytes,
    );

    let empty_expiry_bytes = fixture(algorithm, "root-expiry-catalog-manifest-empty");
    let empty_expiry = decode_root_expiry_manifest_v1(&empty_expiry_bytes, algorithm).unwrap();
    assert_eq!(
      encode_root_expiry_manifest_v1(&RootExpiryManifestWriteV1::from_decoded(algorithm, &empty_expiry)).unwrap().value,
      empty_expiry_bytes,
    );

    let empty_lifecycle_bytes = fixture(algorithm, "root-lifecycle-manifest-empty");
    let empty_lifecycle = decode_root_lifecycle_manifest_v1(&empty_lifecycle_bytes, algorithm).unwrap();
    assert_eq!(
      encode_root_lifecycle_manifest_v1(&RootLifecycleManifestWriteV1::from_decoded(algorithm, &empty_lifecycle)).unwrap().value,
      empty_lifecycle_bytes,
    );
  }
}

#[test]
fn lifecycle_encoders_reject_inconsistent_state_instead_of_emitting_unreadable_bytes() {
  let algorithm = HashAlgorithm::Blake3_256;
  let expiry_bytes = fixture(algorithm, "root-expiry-catalog-manifest-populated");
  let expiry = decode_root_expiry_manifest_v1(&expiry_bytes, algorithm).unwrap();
  let mut invalid_expiry = RootExpiryManifestWriteV1::from_decoded(algorithm, &expiry);
  invalid_expiry.mandatory_count = invalid_expiry.record_count;
  assert_eq!(encode_root_expiry_manifest_v1(&invalid_expiry).unwrap_err().code(), "root_expiry_manifest_state");

  let retirement_bytes = fixture(algorithm, "root-retirement-commit-valid");
  let retirement = decode_root_retirement_commit_v1(&retirement_bytes, algorithm).unwrap();
  let mut invalid_retirement = RootRetirementCommitWriteV1::from_decoded(algorithm, &retirement);
  invalid_retirement.committed_at_ms = invalid_retirement.pending_since_ms;
  assert_eq!(encode_root_retirement_commit_v1(&invalid_retirement).unwrap_err().code(), "root_retirement_fields");

  let proof_bytes = fixture(algorithm, "root-object-reclaim-proof-valid");
  let proof = decode_root_object_reclaim_proof_v1(&proof_bytes, algorithm).unwrap();
  let mut invalid_proof = RootObjectReclaimProofWriteV1::from_decoded(algorithm, &proof);
  invalid_proof.sweep_receipt_count = 0;
  assert_eq!(encode_root_object_reclaim_proof_v1(&invalid_proof).unwrap_err().code(), "root_reclaim_proof_fields");

  let expiry_page = fixture(algorithm, "root-expiry-page-valid");
  let GcStateArtifactV1::Page(expiry_page) = decode_gc_state_artifact(&expiry_page, algorithm).unwrap() else {
    unreachable!();
  };
  let row_length = 40 + 3 * algorithm.hash_length();
  let expiry = decode_root_expiry_record_v1(&expiry_page.records[..row_length], algorithm).unwrap();
  let mut invalid_row = RootExpiryRecordWriteV1 {
    hash_algorithm: algorithm,
    namespace_root_hash: expiry.namespace_root_hash,
    retired_at_ms: expiry.retired_at_ms,
    last_pending_since_ms: expiry.last_pending_since_ms,
    final_mark_generation: expiry.final_mark_generation,
    reason: expiry.reason,
    state: RootExpiryStateV1::LogicallyRetired,
    retirement_commit_hash: expiry.retirement_commit_hash,
    root_object_reclaim_proof_hash: Some(&[7; 32]),
    evidence_expires_at_ms: None,
  };
  assert_eq!(encode_root_expiry_record_v1(&invalid_row).unwrap_err().code(), "root_expiry_row_state");
  invalid_row.root_object_reclaim_proof_hash = None;
  invalid_row.evidence_expires_at_ms = Some(expiry.retired_at_ms);
  assert_eq!(encode_root_expiry_record_v1(&invalid_row).unwrap_err().code(), "root_expiry_row_state");
}

#[test]
fn lifecycle_closure_model_streams_pages_and_fails_closed_on_mismatch_or_cancellation() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let lifecycle_bytes = fixture(algorithm, "root-lifecycle-manifest-populated");
    let lifecycle = decode_root_lifecycle_manifest_v1(&lifecycle_bytes, algorithm).unwrap();
    let expiry_manifest_bytes = fixture(algorithm, "root-expiry-catalog-manifest-populated");
    let expiry_manifest = decode_root_expiry_manifest_v1(&expiry_manifest_bytes, algorithm).unwrap();

    let candidate_directory_bytes = fixture(algorithm, "root-candidates-directory-valid");
    let GcStateArtifactV1::Directory(candidate_directory) = decode_gc_state_artifact(&candidate_directory_bytes, algorithm).unwrap() else {
      unreachable!();
    };
    let candidate_page_bytes = fixture(algorithm, "root-candidate-page-valid");
    let GcStateArtifactV1::Page(candidate_page) = decode_gc_state_artifact(&candidate_page_bytes, algorithm).unwrap() else {
      unreachable!();
    };
    validate_root_lifecycle_candidate_directory(&lifecycle, &candidate_directory).unwrap();
    validate_gc_directory_page(&candidate_directory, &candidate_page).unwrap();

    let expiry_directory_bytes = fixture(algorithm, "root-expiry-directory-valid");
    let GcStateArtifactV1::Directory(expiry_directory) = decode_gc_state_artifact(&expiry_directory_bytes, algorithm).unwrap() else {
      unreachable!();
    };
    let expiry_page_bytes = fixture(algorithm, "root-expiry-page-valid");
    let GcStateArtifactV1::Page(expiry_page) = decode_gc_state_artifact(&expiry_page_bytes, algorithm).unwrap() else {
      unreachable!();
    };
    validate_root_lifecycle_expiry_manifest(&lifecycle, &expiry_manifest).unwrap();
    validate_root_expiry_manifest_directory(&expiry_manifest, &expiry_directory).unwrap();
    validate_gc_directory_page(&expiry_directory, &expiry_page).unwrap();

    let cancellation = CancellationToken::new();
    let mut model = RootLifecycleReferenceModelV1::new(&lifecycle, Some(&expiry_manifest), algorithm, &cancellation, 1, 2).unwrap();
    model.observe_candidate_page(&candidate_page).unwrap();
    model.observe_expiry_page(&expiry_page).unwrap();
    let summary = model.finish().unwrap();
    assert_eq!((summary.candidate_page_count, summary.candidate_count), (1, 1));
    assert_eq!((summary.expiry_page_count, summary.expiry_count), (1, 2));
    assert_eq!((summary.mandatory_expiry_count, summary.optional_expiry_count), (1, 1));

    let row_length = 40 + 3 * algorithm.hash_length();
    let reclaimed = decode_root_expiry_record_v1(&expiry_page.records[row_length..], algorithm).unwrap();
    let retirement_bytes = fixture(algorithm, "root-retirement-commit-valid");
    let retirement = decode_root_retirement_commit_v1(&retirement_bytes, algorithm).unwrap();
    let proof_bytes = fixture(algorithm, "root-object-reclaim-proof-valid");
    let proof = decode_root_object_reclaim_proof_v1(&proof_bytes, algorithm).unwrap();
    validate_root_expiry_retirement_commit(&reclaimed, &retirement).unwrap();
    validate_root_expiry_reclaim_proof(&reclaimed, &proof).unwrap();

    let mut wrong_directory = candidate_directory.clone();
    wrong_directory.key[0] ^= 1;
    assert_eq!(
      validate_root_lifecycle_candidate_directory(&lifecycle, &wrong_directory).unwrap_err().code(),
      "root_lifecycle_candidate_directory",
    );

    let mut wrong_lifecycle = lifecycle.clone();
    wrong_lifecycle.candidate_count += 1;
    wrong_lifecycle.pending_count += 1;
    let mut model = RootLifecycleReferenceModelV1::new(&wrong_lifecycle, Some(&expiry_manifest), algorithm, &cancellation, 2, 2).unwrap();
    model.observe_candidate_page(&candidate_page).unwrap();
    model.observe_expiry_page(&expiry_page).unwrap();
    assert_eq!(model.finish().unwrap_err().code(), "root_lifecycle_manifest_aggregate");

    let duplicate_page_cancellation = CancellationToken::new();
    let mut duplicate_page_model =
      RootLifecycleReferenceModelV1::new(&lifecycle, Some(&expiry_manifest), algorithm, &duplicate_page_cancellation, 2, 2).unwrap();
    duplicate_page_model.observe_candidate_page(&candidate_page).unwrap();
    assert_eq!(duplicate_page_model.observe_candidate_page(&candidate_page).unwrap_err().code(), "root_lifecycle_record_order");
    assert_eq!(duplicate_page_model.observe_expiry_page(&expiry_page).unwrap_err().code(), "root_lifecycle_failed");

    let mid_traversal_cancellation = CancellationToken::new();
    let mut canceled_model =
      RootLifecycleReferenceModelV1::new(&lifecycle, Some(&expiry_manifest), algorithm, &mid_traversal_cancellation, 1, 2).unwrap();
    mid_traversal_cancellation.cancel();
    assert_eq!(canceled_model.observe_candidate_page(&candidate_page).unwrap_err().code(), "root_lifecycle_canceled");
    assert_eq!(canceled_model.observe_candidate_page(&candidate_page).unwrap_err().code(), "root_lifecycle_failed");

    let canceled = CancellationToken::new();
    canceled.cancel();
    assert_eq!(
      RootLifecycleReferenceModelV1::new(&lifecycle, Some(&expiry_manifest), algorithm, &canceled, 1, 2).unwrap_err().code(),
      "root_lifecycle_canceled",
    );
  }
}
