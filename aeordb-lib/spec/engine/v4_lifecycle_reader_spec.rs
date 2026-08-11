use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::gc_state::{
  GcDirectoryRoleV1, GcStateArtifactV1, RootExpiryStateV1, decode_gc_state_artifact, decode_root_candidate_record_v1,
  decode_root_expiry_record_v1,
};
use aeordb::engine::v4::system_control::{TaskPinStateV1, decode_task_pin_v1};

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4")
}

fn fixture(relative_path: &str) -> Vec<u8> {
  fs::read(fixture_root().join(relative_path)).unwrap()
}

fn algorithm_name(algorithm: HashAlgorithm) -> &'static str {
  match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => unreachable!("lifecycle fixtures cover the two frozen hash widths"),
  }
}

#[test]
fn root_candidate_rows_expose_exact_pending_state_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let bytes = fixture(&format!("gc-artifact-v1/agca-{}-root-candidate-page-valid.bin", algorithm_name(algorithm)));
    let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(&bytes, algorithm).unwrap() else {
      panic!("fixture must decode as a root-candidate page");
    };
    assert_eq!(page.role, GcDirectoryRoleV1::RootCandidates);
    assert_eq!(page.record_count, 1);

    let record = decode_root_candidate_record_v1(page.records, algorithm).unwrap();
    assert_eq!(record.namespace_root_hash.len(), algorithm.hash_length());
    assert_eq!(record.namespace_root_hash, page.lower_fence);
    assert_eq!(record.namespace_root_hash, page.upper_fence);
    assert_eq!(record.reason, 1);
    assert!(record.pending_since_ms > 0);
    assert!(record.first_unreachable_generation > 0);
    assert!(record.last_confirmed_unreachable_generation >= record.first_unreachable_generation);
    assert!(record.grace_at_pending_ms > 0);
    assert_eq!(record.authority_root_set_digest.len(), algorithm.hash_length());
    assert_eq!(record.admission_commit_payload_hash.len(), algorithm.hash_length());
  }
}

#[test]
fn root_expiry_rows_distinguish_logical_retirement_from_physical_reclaim() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let bytes = fixture(&format!("gc-artifact-v1/agca-{}-root-expiry-page-valid.bin", algorithm_name(algorithm)));
    let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(&bytes, algorithm).unwrap() else {
      panic!("fixture must decode as a root-expiry page");
    };
    assert_eq!(page.role, GcDirectoryRoleV1::RootExpiry);
    let row_length = 40 + 3 * algorithm.hash_length();
    let records: Vec<_> = page.records.chunks_exact(row_length).map(|row| decode_root_expiry_record_v1(row, algorithm).unwrap()).collect();
    assert_eq!(records.len(), 2);

    assert_eq!(records[0].state, RootExpiryStateV1::LogicallyRetired);
    assert!(records[0].root_object_reclaim_proof_hash.is_none());
    assert!(records[0].evidence_expires_at_ms.is_none());
    assert_eq!(records[1].state, RootExpiryStateV1::PhysicallyReclaimed);
    assert_eq!(records[1].root_object_reclaim_proof_hash.unwrap().len(), algorithm.hash_length());
    assert!(records[1].evidence_expires_at_ms.is_some_and(|expires_at| expires_at >= records[1].retired_at_ms));
  }
}

#[test]
fn lifecycle_rows_reject_unknown_retirement_reasons_instead_of_treating_them_as_absence() {
  let algorithm = HashAlgorithm::Blake3_256;
  let candidate_bytes = fixture("gc-artifact-v1/agca-blake3-256-root-candidate-page-valid.bin");
  let GcStateArtifactV1::Page(candidate_page) = decode_gc_state_artifact(&candidate_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  let mut candidate = candidate_page.records.to_vec();
  let reason_offset = algorithm.hash_length() + 2;
  candidate[reason_offset..reason_offset + 2].copy_from_slice(&3u16.to_le_bytes());
  assert_eq!(decode_root_candidate_record_v1(&candidate, algorithm).unwrap_err().code(), "root_candidate_reason");

  let expiry_bytes = fixture("gc-artifact-v1/agca-blake3-256-root-expiry-page-valid.bin");
  let GcStateArtifactV1::Page(expiry_page) = decode_gc_state_artifact(&expiry_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  let row_length = 40 + 3 * algorithm.hash_length();
  let mut expiry = expiry_page.records[..row_length].to_vec();
  let reason_offset = algorithm.hash_length() + 24;
  expiry[reason_offset..reason_offset + 2].copy_from_slice(&3u16.to_le_bytes());
  assert_eq!(decode_root_expiry_record_v1(&expiry, algorithm).unwrap_err().code(), "root_expiry_reason");
}

#[test]
fn durable_task_pin_reader_exposes_bounded_roots_and_artifacts_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let bytes = fixture(&format!("system-control-v1/control-{}-task-pin-valid.bin", algorithm_name(algorithm)));
    let pin = decode_task_pin_v1(&bytes, algorithm).unwrap();
    assert_eq!(pin.control_sequence, 7);
    assert_eq!(pin.database_id, std::array::from_fn(|index| 0x10 + index as u8));
    assert_eq!(pin.task_id, std::array::from_fn(|index| 0x20 + index as u8));
    assert_eq!(pin.task_kind, 8);
    assert_eq!(pin.state, TaskPinStateV1::Active);
    assert!(pin.created_at_ms > 0);
    assert!(pin.renewed_at_ms >= pin.created_at_ms);
    assert!(pin.expires_at_ms.is_some_and(|expires_at| expires_at > pin.renewed_at_ms));
    assert_eq!(pin.fencing_token, 9);
    assert_eq!(pin.root_hashes.len(), 2);
    assert_eq!(pin.artifact_hashes.len(), 2);
    assert!(pin.root_hashes.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(pin.artifact_hashes.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(pin.root_hashes.iter().all(|hash| hash.len() == algorithm.hash_length()));
    assert!(pin.artifact_hashes.iter().all(|hash| hash.len() == algorithm.hash_length()));
  }
}

#[test]
fn durable_task_pin_reader_rejects_corruption_before_allocating_hash_vectors() {
  let algorithm = HashAlgorithm::Blake3_256;
  let mut bytes = fixture("system-control-v1/control-blake3-256-task-pin-valid.bin");
  bytes[32 + 34..32 + 36].copy_from_slice(&4u16.to_le_bytes());
  let crc_offset = bytes.len() - 4;
  let crc = crc32fast::hash(&bytes[..crc_offset]);
  bytes[crc_offset..].copy_from_slice(&crc.to_le_bytes());
  assert_eq!(decode_task_pin_v1(&bytes, algorithm).unwrap_err().code(), "task_pin_kind");

  let mut bytes = fixture("system-control-v1/control-blake3-256-task-pin-valid.bin");
  bytes[32 + 68..32 + 72].copy_from_slice(&4_097u32.to_le_bytes());
  let crc_offset = bytes.len() - 4;
  let crc = crc32fast::hash(&bytes[..crc_offset]);
  bytes[crc_offset..].copy_from_slice(&crc.to_le_bytes());
  assert_eq!(decode_task_pin_v1(&bytes, algorithm).unwrap_err().code(), "task_pin_count");
}
