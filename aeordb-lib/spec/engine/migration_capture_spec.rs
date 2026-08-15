use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::migration_capture::{
  MIGRATION_CAPTURE_FLAG_NEEDS_FULL_RECONCILE, MIGRATION_CAPTURE_FLAG_OPTIONAL_CAPTURE_STOPPED, MigrationCaptureManifestStateV1,
  MigrationCaptureManifestWriteV1, decode_migration_capture_manifest, encode_migration_capture_manifest,
  migration_capture_manifest_identity,
};

fn id(first: u8) -> [u8; 16] {
  std::array::from_fn(|offset| first.wrapping_add(offset as u8))
}

fn hash(algorithm: HashAlgorithm, first: u8) -> Vec<u8> {
  (0..algorithm.hash_length()).map(|offset| first.wrapping_add(offset as u8)).collect()
}

fn manifest(algorithm: HashAlgorithm) -> MigrationCaptureManifestWriteV1 {
  MigrationCaptureManifestWriteV1 {
    database_id: id(0x10),
    migration_id: id(0x20),
    source_physical_instance_id: id(0x30),
    destination_physical_instance_id: id(0x40),
    fencing_token: 9,
    capture_generation: 2,
    checkpoint_sequence: 3,
    state: MigrationCaptureManifestStateV1::Capturing,
    flags: 0,
    created_at_ms: 1_700_000_000_000,
    updated_at_ms: 1_700_000_001_000,
    captured_through_publication_sequence: 110,
    observed_through_publication_sequence: 110,
    first_segment_ordinal: 4,
    last_segment_ordinal: 5,
    segment_count: 2,
    segment_stored_bytes: 2_048,
    source_root_before: hash(algorithm, 0x50),
    source_root_after: hash(algorithm, 0x60),
    segment_head: hash(algorithm, 0x70),
    previous_manifest: hash(algorithm, 0x80),
    effective_config_fingerprint: hash(algorithm, 0x90),
    system_family_registry_fingerprint: hash(algorithm, 0xa0),
    failure_evidence: vec![0; algorithm.hash_length()],
    source_authority_digest: std::array::from_fn(|offset| 0xb0u8.wrapping_add(offset as u8)),
  }
}

fn fixture(name: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/migration-capture-v1/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

#[test]
fn manifest_codec_matches_the_independent_fixed_size_fixture() {
  let algorithm = HashAlgorithm::Blake3_256;
  let expected = fixture("amcm-blake3-256-capturing-valid.bin");
  let encoded = encode_migration_capture_manifest(&manifest(algorithm), algorithm).unwrap();
  assert_eq!(encoded, expected);

  let decoded = decode_migration_capture_manifest(&expected, algorithm).unwrap();
  assert_eq!(decoded, manifest(algorithm));
  assert_eq!(
    migration_capture_manifest_identity(&expected, algorithm),
    hex::decode("ea8e29ddfbfa642d4511b5957b6cfa50a38a764d11bdff709fe2937f25473238").unwrap()
  );
}

#[test]
fn manifest_codec_covers_the_widest_hash_profile_without_a_growing_descriptor_table() {
  let algorithm = HashAlgorithm::Sha512;
  let expected = fixture("amcm-sha512-capturing-valid.bin");
  let encoded = encode_migration_capture_manifest(&manifest(algorithm), algorithm).unwrap();
  assert_eq!(encoded, expected);
  assert_eq!(decode_migration_capture_manifest(&expected, algorithm).unwrap(), manifest(algorithm));
  assert_eq!(expected.len(), 724);
}

#[test]
fn empty_initial_checkpoint_accepts_the_boot_local_zero_publication_sentinel() {
  let algorithm = HashAlgorithm::Blake3_256;
  let mut request = manifest(algorithm);
  request.checkpoint_sequence = 1;
  request.captured_through_publication_sequence = 0;
  request.observed_through_publication_sequence = 0;
  request.first_segment_ordinal = 0;
  request.last_segment_ordinal = 0;
  request.segment_count = 0;
  request.segment_stored_bytes = 0;
  request.source_root_after.clone_from(&request.source_root_before);
  request.segment_head.fill(0);
  request.previous_manifest.fill(0);

  let expected = fixture("amcm-blake3-256-initial-empty-valid.bin");
  assert_eq!(encode_migration_capture_manifest(&request, algorithm).unwrap(), expected);
  assert_eq!(decode_migration_capture_manifest(&expected, algorithm).unwrap(), request);
}

#[test]
fn manifest_requires_exact_identity_time_sequence_segment_and_hash_closure() {
  let algorithm = HashAlgorithm::Blake3_256;
  let mut request = manifest(algorithm);
  request.database_id = [0; 16];
  assert_eq!(encode_migration_capture_manifest(&request, algorithm).unwrap_err().code(), "migration_capture_identity");

  let mut request = manifest(algorithm);
  request.destination_physical_instance_id = request.source_physical_instance_id;
  assert_eq!(encode_migration_capture_manifest(&request, algorithm).unwrap_err().code(), "migration_capture_identity");

  let mut request = manifest(algorithm);
  request.updated_at_ms = request.created_at_ms - 1;
  assert_eq!(encode_migration_capture_manifest(&request, algorithm).unwrap_err().code(), "migration_capture_time");

  let mut request = manifest(algorithm);
  request.observed_through_publication_sequence = 109;
  assert_eq!(encode_migration_capture_manifest(&request, algorithm).unwrap_err().code(), "migration_capture_sequence");

  let mut request = manifest(algorithm);
  request.segment_count = 3;
  assert_eq!(encode_migration_capture_manifest(&request, algorithm).unwrap_err().code(), "migration_capture_segment_closure");

  let mut request = manifest(algorithm);
  request.system_family_registry_fingerprint.pop();
  assert_eq!(encode_migration_capture_manifest(&request, algorithm).unwrap_err().code(), "migration_capture_hash_width");
}

#[test]
fn full_reconcile_and_terminal_states_are_explicit_and_failure_latched() {
  let algorithm = HashAlgorithm::Blake3_256;
  let mut request = manifest(algorithm);
  request.state = MigrationCaptureManifestStateV1::NeedsFullReconcile;
  request.flags = MIGRATION_CAPTURE_FLAG_NEEDS_FULL_RECONCILE | MIGRATION_CAPTURE_FLAG_OPTIONAL_CAPTURE_STOPPED;
  request.observed_through_publication_sequence += 5;
  request.failure_evidence = hash(algorithm, 0xd0);
  let encoded = encode_migration_capture_manifest(&request, algorithm).unwrap();
  assert_eq!(decode_migration_capture_manifest(&encoded, algorithm).unwrap(), request);

  let mut missing_latch = request.clone();
  missing_latch.flags = MIGRATION_CAPTURE_FLAG_OPTIONAL_CAPTURE_STOPPED;
  assert_eq!(encode_migration_capture_manifest(&missing_latch, algorithm).unwrap_err().code(), "migration_capture_state_flags");

  let mut cleared_evidence = request;
  cleared_evidence.failure_evidence.fill(0);
  assert_eq!(encode_migration_capture_manifest(&cleared_evidence, algorithm).unwrap_err().code(), "migration_capture_failure_evidence");
}

#[test]
fn decoder_rejects_crc_reserved_algorithm_and_length_corruption() {
  let algorithm = HashAlgorithm::Blake3_256;
  let encoded = encode_migration_capture_manifest(&manifest(algorithm), algorithm).unwrap();

  let mut corrupted = encoded.clone();
  corrupted[20] ^= 1;
  let crc_offset = corrupted.len() - 4;
  let crc = crc32fast::hash(&corrupted[..crc_offset]);
  corrupted[crc_offset..].copy_from_slice(&crc.to_le_bytes());
  assert_eq!(decode_migration_capture_manifest(&corrupted, algorithm).unwrap_err().code(), "migration_capture_reserved");

  let mut corrupted = encoded.clone();
  corrupted[6] ^= 1;
  assert!(decode_migration_capture_manifest(&corrupted, algorithm).is_err());

  let mut corrupted = encoded.clone();
  corrupted.pop();
  assert_eq!(decode_migration_capture_manifest(&corrupted, algorithm).unwrap_err().code(), "migration_capture_length");

  let mut corrupted = encoded;
  *corrupted.last_mut().unwrap() ^= 1;
  assert_eq!(decode_migration_capture_manifest(&corrupted, algorithm).unwrap_err().code(), "migration_capture_crc");
}
