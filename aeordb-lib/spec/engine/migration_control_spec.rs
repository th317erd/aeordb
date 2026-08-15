use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::migration_control::{
  MIGRATION_PROGRESS_FLAG_DESTINATION_FULL_VERIFIED, MIGRATION_PROGRESS_FLAG_NEEDS_FULL_RECONCILE,
  MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED, MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD, MigrationLeaseBodyV1,
  MigrationLeaseStateV1, MigrationPhaseV1, MigrationProgressBodyV1, MigrationProgressStateV1, decode_migration_lease_control,
  decode_migration_progress_control, encode_migration_lease_control, encode_migration_progress_control,
};

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

fn bytes(first: u8, length: usize) -> Vec<u8> {
  (0..length).map(|offset| first.wrapping_add(offset as u8)).collect()
}

fn id(first: u8) -> [u8; 16] {
  bytes(first, 16).try_into().unwrap()
}

fn fixture(name: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/system-control-v1/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn lease() -> MigrationLeaseBodyV1 {
  MigrationLeaseBodyV1 {
    database_id: id(0x10),
    migration_id: id(0x20),
    source_physical_instance_id: id(0x30),
    destination_physical_instance_id: id(0x40),
    holder_boot_id: id(0x50),
    fencing_token: 9,
    acquired_at_ms: 1_700_000_008_000,
    renewed_at_ms: 1_700_000_009_000,
    expires_at_ms: 1_700_000_069_000,
    source_header_sequence: 12,
    state: MigrationLeaseStateV1::Held,
  }
}

fn progress() -> MigrationProgressBodyV1 {
  MigrationProgressBodyV1 {
    database_id: id(0x10),
    migration_id: id(0x20),
    source_physical_instance_id: id(0x30),
    destination_physical_instance_id: id(0x40),
    fencing_token: 9,
    phase: MigrationPhaseV1::Reconcile,
    state: MigrationProgressStateV1::Running,
    flags: MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
    source_header_sequence: 12,
    destination_header_sequence: 8,
    copied_through_write_sequence: 90,
    captured_through_publication_sequence: 88,
    reconciled_through_publication_sequence: 88,
    namespace_count: 500,
    entity_count: 5_000,
    copied_bytes: 1_048_576,
    updated_at_ms: 1_700_000_010_000,
    source_capture_head: bytes(0x50, 32),
    checkpoint_artifact: bytes(0x60, 32),
    legacy_root_map_control_payload_hash: bytes(0x70, 32),
    effective_config_fingerprint: bytes(0x80, 32),
    system_family_registry_fingerprint: bytes(0x90, 32),
    last_error_evidence: vec![0; 32],
  }
}

#[test]
fn migration_lease_codec_matches_the_independent_frozen_fixture() {
  let expected = fixture("control-blake3-256-migration-lease-valid.bin");
  assert_eq!(encode_migration_lease_control(7, &lease(), ALGORITHM).unwrap(), expected);
  let decoded = decode_migration_lease_control(&expected, ALGORITHM).unwrap();
  assert_eq!(decoded.sequence, 7);
  assert_eq!(decoded.body, lease());
}

#[test]
fn migration_progress_codec_matches_the_independent_frozen_fixture() {
  let expected = fixture("control-blake3-256-migration-progress-valid.bin");
  assert_eq!(encode_migration_progress_control(7, &progress(), ALGORITHM).unwrap(), expected);
  let decoded = decode_migration_progress_control(&expected, ALGORITHM).unwrap();
  assert_eq!(decoded.sequence, 7);
  assert_eq!(decoded.body, progress());
}

#[test]
fn typed_migration_decoders_reject_wrong_kinds_and_malformed_bodies() {
  let progress = fixture("control-blake3-256-migration-progress-valid.bin");
  assert_eq!(decode_migration_lease_control(&progress, ALGORITHM).unwrap_err().code(), "migration_lease_control_kind");

  let mut lease = fixture("control-blake3-256-migration-lease-valid.bin");
  lease.truncate(lease.len() - 1);
  assert!(decode_migration_lease_control(&lease, ALGORITHM).is_err());
}

#[test]
fn migration_codecs_cover_the_widest_supported_hash_profile() {
  let algorithm = HashAlgorithm::Sha512;
  let expected = fixture("control-sha512-migration-progress-valid.bin");
  let decoded = decode_migration_progress_control(&expected, algorithm).unwrap();
  assert_eq!(decoded.body.source_capture_head.len(), 64);
  assert_eq!(encode_migration_progress_control(decoded.sequence, &decoded.body, algorithm).unwrap(), expected);
}

#[test]
fn migration_enums_and_known_progress_flags_round_trip_exhaustively() {
  for state in
    [MigrationLeaseStateV1::Held, MigrationLeaseStateV1::Releasing, MigrationLeaseStateV1::Released, MigrationLeaseStateV1::Expired]
  {
    let mut body = lease();
    body.state = state;
    let encoded = encode_migration_lease_control(1, &body, ALGORITHM).unwrap();
    assert_eq!(decode_migration_lease_control(&encoded, ALGORITHM).unwrap().body, body);
  }

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
  let states = [
    MigrationProgressStateV1::Pending,
    MigrationProgressStateV1::Running,
    MigrationProgressStateV1::Paused,
    MigrationProgressStateV1::Complete,
    MigrationProgressStateV1::Failed,
    MigrationProgressStateV1::Canceled,
  ];
  for (index, phase) in phases.into_iter().enumerate() {
    let mut body = progress();
    body.phase = phase;
    body.state = states[index % states.len()];
    body.flags = MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED
      | MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD
      | MIGRATION_PROGRESS_FLAG_DESTINATION_FULL_VERIFIED
      | MIGRATION_PROGRESS_FLAG_NEEDS_FULL_RECONCILE;
    let encoded = encode_migration_progress_control(index as u64 + 1, &body, ALGORITHM).unwrap();
    assert_eq!(decode_migration_progress_control(&encoded, ALGORITHM).unwrap().body, body);
  }
}

#[test]
fn migration_lease_encoder_rejects_invalid_identity_fencing_and_times() {
  let mut body = lease();
  body.holder_boot_id = [0; 16];
  assert_eq!(encode_migration_lease_control(1, &body, ALGORITHM).unwrap_err().code(), "migration_lease_identity");

  let mut body = lease();
  body.fencing_token = 0;
  assert_eq!(encode_migration_lease_control(1, &body, ALGORITHM).unwrap_err().code(), "migration_lease_fencing");

  let mut body = lease();
  body.renewed_at_ms = body.expires_at_ms;
  assert_eq!(encode_migration_lease_control(1, &body, ALGORITHM).unwrap_err().code(), "migration_lease_times");
}

#[test]
fn migration_progress_encoder_rejects_invalid_identity_flags_time_and_hashes() {
  let mut body = progress();
  body.destination_physical_instance_id = [0; 16];
  assert_eq!(encode_migration_progress_control(1, &body, ALGORITHM).unwrap_err().code(), "migration_progress_identity");

  let mut body = progress();
  body.flags = 1 << 4;
  assert_eq!(encode_migration_progress_control(1, &body, ALGORITHM).unwrap_err().code(), "migration_progress_flags");

  let mut body = progress();
  body.updated_at_ms = -1;
  assert_eq!(encode_migration_progress_control(1, &body, ALGORITHM).unwrap_err().code(), "migration_progress_time");

  let mut body = progress();
  body.source_capture_head.pop();
  assert_eq!(encode_migration_progress_control(1, &body, ALGORITHM).unwrap_err().code(), "migration_control_length");

  let mut body = progress();
  body.effective_config_fingerprint.fill(0);
  assert_eq!(encode_migration_progress_control(1, &body, ALGORITHM).unwrap_err().code(), "migration_progress_required_hash");
}
