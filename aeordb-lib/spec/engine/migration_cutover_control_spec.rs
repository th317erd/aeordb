use aeordb::engine::HashAlgorithm;
use aeordb::engine::native_durability::PlatformFileIdentityDescriptorV1;
use aeordb::engine::v4::migration_control::MigrationPhaseV1;
use aeordb::engine::v4::migration_cutover_control::{
  CutoverArtifactRoleV1, CutoverStableFileIdentityEvidenceV1, SideBySideCutoverBodyV1, cutover_path_identity_hash_v1,
  cutover_stable_file_identity_hash_v1, decode_side_by_side_cutover_control_v1, encode_side_by_side_cutover_control_v1,
};

fn bytes(first: u8, length: usize) -> Vec<u8> {
  (0..length).map(|offset| first.wrapping_add(offset as u8)).collect()
}

fn id(first: u8) -> [u8; 16] {
  bytes(first, 16).try_into().unwrap()
}

fn fixture(profile: &str) -> Vec<u8> {
  std::fs::read(format!(
    "{}/spec/fixtures/v4/system-control-v1/control-{profile}-side-by-side-cutover-valid.bin",
    env!("CARGO_MANIFEST_DIR")
  ))
  .unwrap()
}

fn body(hash_width: usize) -> SideBySideCutoverBodyV1 {
  SideBySideCutoverBodyV1 {
    database_id: id(0x10),
    migration_id: id(0x20),
    source_physical_instance_id: id(0x30),
    destination_physical_instance_id: id(0x40),
    holder_boot_id: id(0x50),
    fencing_token: 9,
    phase: MigrationPhaseV1::Reconcile,
    journal_sequence: 12,
    destination_header_sequence: 8,
    source_file_size: 2_000_000,
    destination_file_size: 2_100_000,
    updated_at_ms: 1_700_000_019_000,
    source_path_identity_hash: bytes(0x60, hash_width),
    destination_path_identity_hash: bytes(0x70, hash_width),
    source_stable_file_identity_hash: bytes(0x80, hash_width),
    destination_stable_file_identity_hash: bytes(0x90, hash_width),
    last_error_evidence: vec![0; hash_width],
  }
}

#[test]
fn typed_cutover_codec_matches_both_independent_frozen_fixtures() {
  for (profile, algorithm) in [("blake3-256", HashAlgorithm::Blake3_256), ("sha512", HashAlgorithm::Sha512)] {
    let expected = fixture(profile);
    let expected_body = body(algorithm.hash_length());
    assert_eq!(encode_side_by_side_cutover_control_v1(7, &expected_body, algorithm).unwrap(), expected);
    let decoded = decode_side_by_side_cutover_control_v1(&expected, algorithm).unwrap();
    assert_eq!(decoded.sequence, 7);
    assert_eq!(decoded.body, expected_body);
  }
}

#[test]
fn every_frozen_cutover_phase_round_trips_without_an_alias() {
  let phases = [
    MigrationPhaseV1::Preflight,
    MigrationPhaseV1::Copy,
    MigrationPhaseV1::Reconcile,
    MigrationPhaseV1::FinalFreeze,
    MigrationPhaseV1::DestinationVerify,
    MigrationPhaseV1::Cutover,
    MigrationPhaseV1::ReadOnlyValidation,
    MigrationPhaseV1::OperatorAcceptance,
  ];
  for (index, phase) in phases.into_iter().enumerate() {
    let mut expected = body(32);
    expected.phase = phase;
    expected.journal_sequence = index as u64 + 1;
    let encoded = encode_side_by_side_cutover_control_v1(index as u64 + 1, &expected, HashAlgorithm::Blake3_256).unwrap();
    assert_eq!(decode_side_by_side_cutover_control_v1(&encoded, HashAlgorithm::Blake3_256).unwrap().body, expected);
  }
}

#[test]
fn typed_cutover_encoder_rejects_every_invalid_field_class() {
  let mut cases = Vec::new();

  let mut invalid = body(32);
  invalid.holder_boot_id = [0; 16];
  cases.push((invalid, "cutover_control_identity"));

  let mut invalid = body(32);
  invalid.source_physical_instance_id = invalid.destination_physical_instance_id;
  cases.push((invalid, "cutover_control_identity"));

  let mut invalid = body(32);
  invalid.fencing_token = 0;
  cases.push((invalid, "cutover_control_scalars"));

  let mut invalid = body(32);
  invalid.source_file_size = 0;
  cases.push((invalid, "cutover_control_scalars"));

  let mut invalid = body(32);
  invalid.updated_at_ms = -1;
  cases.push((invalid, "cutover_control_time"));

  let mut invalid = body(32);
  invalid.source_path_identity_hash.pop();
  cases.push((invalid, "cutover_control_hash_length"));

  let mut invalid = body(32);
  invalid.destination_stable_file_identity_hash.fill(0);
  cases.push((invalid, "cutover_control_hash"));

  let mut invalid = body(32);
  invalid.destination_path_identity_hash = invalid.source_path_identity_hash.clone();
  cases.push((invalid, "cutover_control_hash"));

  for (invalid, code) in cases {
    assert_eq!(encode_side_by_side_cutover_control_v1(1, &invalid, HashAlgorithm::Blake3_256).unwrap_err().code(), code);
  }
}

#[test]
fn typed_cutover_decoder_rejects_wrong_kind_truncation_and_hash_profile() {
  let wrong_kind = std::fs::read(format!(
    "{}/spec/fixtures/v4/system-control-v1/control-blake3-256-migration-progress-valid.bin",
    env!("CARGO_MANIFEST_DIR")
  ))
  .unwrap();
  assert_eq!(decode_side_by_side_cutover_control_v1(&wrong_kind, HashAlgorithm::Blake3_256).unwrap_err().code(), "cutover_control_kind");

  let mut truncated = fixture("blake3-256");
  truncated.pop();
  assert!(decode_side_by_side_cutover_control_v1(&truncated, HashAlgorithm::Blake3_256).is_err());
  assert!(decode_side_by_side_cutover_control_v1(&fixture("blake3-256"), HashAlgorithm::Sha512).is_err());
}

fn independent_digest(algorithm: HashAlgorithm, parts: &[&[u8]]) -> Vec<u8> {
  match algorithm {
    HashAlgorithm::Blake3_256 => {
      let mut hasher = blake3::Hasher::new();
      for part in parts {
        hasher.update(part);
      }
      hasher.finalize().as_bytes().to_vec()
    }
    HashAlgorithm::Sha512 => {
      use sha2::{Digest, Sha512};
      let mut hasher = Sha512::new();
      for part in parts {
        hasher.update(part);
      }
      hasher.finalize().to_vec()
    }
    HashAlgorithm::Sha256 => {
      use sha2::{Digest, Sha256};
      let mut hasher = Sha256::new();
      for part in parts {
        hasher.update(part);
      }
      hasher.finalize().to_vec()
    }
    HashAlgorithm::Sha3_256 => {
      use sha3::{Digest, Sha3_256};
      let mut hasher = Sha3_256::new();
      for part in parts {
        hasher.update(part);
      }
      hasher.finalize().to_vec()
    }
    HashAlgorithm::Sha3_512 => {
      use sha3::{Digest, Sha3_512};
      let mut hasher = Sha3_512::new();
      for part in parts {
        hasher.update(part);
      }
      hasher.finalize().to_vec()
    }
  }
}

#[test]
fn typed_path_and_stable_file_hashes_match_the_independent_contract_recipe() {
  let path_digest = [0x31; 32];
  let identity = PlatformFileIdentityDescriptorV1 {
    platform: 1,
    schema: 1,
    flags: 0,
    volume_identity: [0x41; 16],
    file_identity: [0x51; 16],
    birth_identity: [0x61; 16],
  };
  let database_id = [0x11; 16];
  let physical_instance_id = [0x21; 16];
  let header_digest = [0x71; 32];
  let header_sequence = 8u64;
  let file_size = 2_100_000u64;
  let descriptor = identity.to_bytes();
  let format = 4u16;

  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let path_expected = independent_digest(algorithm, &[b"aeordb.side-by-side-cutover.path-identity.v1\0", &[2], &path_digest]);
    assert_eq!(cutover_path_identity_hash_v1(algorithm, CutoverArtifactRoleV1::Destination, path_digest).unwrap(), path_expected);

    let stable_expected = independent_digest(
      algorithm,
      &[
        b"aeordb.side-by-side-cutover.stable-file-identity.v1\0",
        &[2],
        &database_id,
        &physical_instance_id,
        &descriptor,
        &format.to_le_bytes(),
        &header_sequence.to_le_bytes(),
        &header_digest,
        &file_size.to_le_bytes(),
      ],
    );
    let evidence = CutoverStableFileIdentityEvidenceV1 {
      role: CutoverArtifactRoleV1::Destination,
      database_id,
      physical_instance_id,
      platform_file_identity: identity,
      format,
      selected_header_sequence: header_sequence,
      selected_header_blake3: header_digest,
      file_size,
    };
    assert_eq!(cutover_stable_file_identity_hash_v1(algorithm, &evidence).unwrap(), stable_expected);
  }
}

#[test]
fn stable_file_hash_refuses_zero_or_wrong_role_evidence() {
  let evidence = CutoverStableFileIdentityEvidenceV1 {
    role: CutoverArtifactRoleV1::Source,
    database_id: [0x11; 16],
    physical_instance_id: [0x21; 16],
    platform_file_identity: PlatformFileIdentityDescriptorV1 {
      platform: 1,
      schema: 1,
      flags: 0,
      volume_identity: [0x41; 16],
      file_identity: [0x51; 16],
      birth_identity: [0x61; 16],
    },
    format: 4,
    selected_header_sequence: 8,
    selected_header_blake3: [0x71; 32],
    file_size: 2_100_000,
  };
  assert_eq!(
    cutover_stable_file_identity_hash_v1(HashAlgorithm::Blake3_256, &evidence).unwrap_err().code(),
    "cutover_file_identity_format"
  );

  let mut invalid = evidence;
  invalid.format = 3;
  invalid.file_size = 0;
  assert_eq!(cutover_stable_file_identity_hash_v1(HashAlgorithm::Blake3_256, &invalid).unwrap_err().code(), "cutover_file_identity_scalar");
}
