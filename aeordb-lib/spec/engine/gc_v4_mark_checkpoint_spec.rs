use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::gc::{GcActiveControlWriteV1, GcArtifactKindV1, encode_gc_active_control};
use aeordb::engine::v4::gc_mark::{
  GcMarkArtifactV1, MarkResumeContextV1, MarkRunCheckpointWriteV1, decode_gc_mark_artifact, decode_mark_workspace_manifest,
  encode_mark_run_checkpoint, validate_mark_resume_context,
};

const DEFAULT_WORKSPACE_PATH: &str = "/srv/data/.taraani.aeordb-gc-3132333435363738393a3b3c3d3e3f40-5152535455565758595a5b5c5d5e5f60";

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4")
}

fn fixture_label(algorithm: HashAlgorithm) -> &'static str {
  match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => panic!("checkpoint fixture test uses only frozen hash profiles"),
  }
}

fn sequence<const N: usize>(start: u8) -> [u8; N] {
  let mut bytes = [0u8; N];
  for (index, byte) in bytes.iter_mut().enumerate() {
    *byte = start.wrapping_add(u8::try_from(index).unwrap());
  }
  bytes
}

fn sequence_vec(start: u8, length: usize) -> Vec<u8> {
  (0..length).map(|index| start.wrapping_add(u8::try_from(index).unwrap())).collect()
}

fn capabilities() -> [u8; 32] {
  let mut capabilities = [0u8; 32];
  for bit in [12usize, 13, 14, 15, 17] {
    capabilities[bit / 8] |= 1 << (bit % 8);
  }
  capabilities
}

struct FixtureBasis {
  database_id: [u8; 16],
  run_id: [u8; 16],
  workspace_id: [u8; 16],
  authority_root_set_digest: Vec<u8>,
  semantic_state_digest: Vec<u8>,
  kv_layout_fingerprint: Vec<u8>,
  effective_policy_fingerprint: [u8; 32],
  system_family_registry_fingerprint: [u8; 32],
  manifest_digest: [u8; 32],
  mutation_journal_head: Vec<u8>,
  manifest: Vec<u8>,
}

fn fixture_basis(algorithm: HashAlgorithm) -> FixtureBasis {
  let label = fixture_label(algorithm);
  let manifest =
    fs::read(fixture_root().join("gc-mark-workspace-manifest-v1").join(format!("agcw-{label}-mark-workspace-manifest.bin"))).unwrap();
  let journal = fs::read(fixture_root().join("gc-artifact-v1").join(format!("agca-{label}-mark-mutation-journal-reset.bin"))).unwrap();
  let GcMarkArtifactV1::MutationJournal(journal) = decode_gc_mark_artifact(&journal, algorithm).unwrap() else {
    panic!("expected mutation journal fixture");
  };
  FixtureBasis {
    database_id: sequence(0x31),
    run_id: sequence(0x51),
    workspace_id: sequence(0x71),
    authority_root_set_digest: sequence_vec(0x11, algorithm.hash_length()),
    semantic_state_digest: sequence_vec(0x31, algorithm.hash_length()),
    kv_layout_fingerprint: sequence_vec(0x51, algorithm.hash_length()),
    effective_policy_fingerprint: sequence(0x71),
    system_family_registry_fingerprint: sequence(0x91),
    manifest_digest: *blake3::hash(&manifest).as_bytes(),
    mutation_journal_head: journal.key,
    manifest,
  }
}

fn checkpoint_request<'a>(algorithm: HashAlgorithm, basis: &'a FixtureBasis, canceled: bool) -> MarkRunCheckpointWriteV1<'a> {
  MarkRunCheckpointWriteV1 {
    hash_algorithm: algorithm,
    database_id: &basis.database_id,
    run_id: &basis.run_id,
    generation: 77,
    checkpoint_sequence: 7,
    state: if canceled { 4 } else { 1 },
    phase: if canceled { 6 } else { 1 },
    resumable: true,
    canceled,
    capabilities: capabilities(),
    started_at_ms: 1_700_000_100_000,
    updated_at_ms: 1_700_000_100_500,
    authority_root_set_digest: &basis.authority_root_set_digest,
    semantic_state_digest: &basis.semantic_state_digest,
    kv_layout_fingerprint: &basis.kv_layout_fingerprint,
    effective_policy_fingerprint: basis.effective_policy_fingerprint,
    system_family_registry_fingerprint: basis.system_family_registry_fingerprint,
    captured_header_sequence: 17,
    captured_write_high_water: 900,
    reconciled_through_sequence: 801,
    active_bitmap_bit_count: 512,
    kv_bucket_count: 8,
    kv_slots_per_bucket: 64,
    workspace_path: if canceled { "C:/AeorDB/gc/31323334/51525354" } else { DEFAULT_WORKSPACE_PATH },
    workspace_id: basis.workspace_id,
    workspace_manifest_digest: basis.manifest_digest,
    mutation_journal_head: &basis.mutation_journal_head,
    checkpoint_logical_work: 16 * 1024 * 1024,
    total_logical_work_hint: 64 * 1024 * 1024,
  }
}

#[test]
fn checkpoint_writer_matches_independent_both_width_fixtures_and_exposes_every_resume_field() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let basis = fixture_basis(algorithm);
    for (canceled, suffix) in [(false, "embedded"), (true, "external-canceled")] {
      let encoded = encode_mark_run_checkpoint(&checkpoint_request(algorithm, &basis, canceled)).unwrap();
      let fixture =
        fs::read(fixture_root().join("gc-artifact-v1").join(format!("agca-{}-mark-run-checkpoint-{suffix}.bin", fixture_label(algorithm))))
          .unwrap();
      assert_eq!(encoded.value, fixture);
      let GcMarkArtifactV1::Checkpoint(decoded) = decode_gc_mark_artifact(&encoded.value, algorithm).unwrap() else {
        panic!("expected checkpoint");
      };
      assert_eq!(decoded.capabilities, capabilities());
      assert_eq!(decoded.started_at_ms, 1_700_000_100_000);
      assert_eq!(decoded.updated_at_ms, 1_700_000_100_500);
      assert_eq!(decoded.authority_root_set_digest, basis.authority_root_set_digest);
      assert_eq!(decoded.semantic_state_digest, basis.semantic_state_digest);
      assert_eq!(decoded.kv_layout_fingerprint, basis.kv_layout_fingerprint);
      assert_eq!(decoded.effective_policy_fingerprint, basis.effective_policy_fingerprint);
      assert_eq!(decoded.system_family_registry_fingerprint, basis.system_family_registry_fingerprint);
      assert_eq!(decoded.captured_header_sequence, 17);
      assert_eq!(decoded.captured_write_high_water, 900);
      assert_eq!(decoded.reconciled_through_sequence, 801);
      assert_eq!(decoded.active_bitmap_bit_count, 512);
      assert_eq!(decoded.kv_bucket_count, 8);
      assert_eq!(decoded.kv_slots_per_bucket, 64);
      assert_eq!(decoded.workspace_id, basis.workspace_id);
      assert_eq!(decoded.checkpoint_logical_work, 16 * 1024 * 1024);
      assert_eq!(decoded.total_logical_work_hint, 64 * 1024 * 1024);
    }
  }
}

#[test]
fn active_control_writer_matches_independent_both_width_fixtures() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let target_manifest_hash = sequence_vec(0x82, algorithm.hash_length());
    for (slot, suffix) in [(0u8, "a"), (1, "b")] {
      let encoded = encode_gc_active_control(&GcActiveControlWriteV1 {
        kind: GcArtifactKindV1::MarkRunActiveControl,
        hash_algorithm: algorithm,
        database_id: &sequence(0x31),
        slot,
        sequence: if slot == 0 { 1 } else { u64::MAX },
        generation: 10_002,
        target_manifest_hash: &target_manifest_hash,
      })
      .unwrap();
      let fixture =
        fs::read(fixture_root().join("gc-artifact-v1").join(format!("agca-{}-mark-run-control-{suffix}.bin", fixture_label(algorithm))))
          .unwrap();
      assert_eq!(encoded.value, fixture);
    }
  }
}

fn resume_context<'a>(algorithm: HashAlgorithm, basis: &'a FixtureBasis) -> MarkResumeContextV1<'a> {
  MarkResumeContextV1 {
    hash_algorithm: algorithm,
    database_id: &basis.database_id,
    run_id: &basis.run_id,
    generation: 77,
    checkpoint_sequence: 7,
    workspace_path: DEFAULT_WORKSPACE_PATH,
    workspace_id: &basis.workspace_id,
    authority_root_set_digest: &basis.authority_root_set_digest,
    semantic_state_digest: &basis.semantic_state_digest,
    kv_layout_fingerprint: &basis.kv_layout_fingerprint,
    effective_policy_fingerprint: &basis.effective_policy_fingerprint,
    system_family_registry_fingerprint: &basis.system_family_registry_fingerprint,
    captured_header_sequence: 17,
    captured_write_high_water: 900,
    reconciled_through_sequence: 801,
    active_bitmap_bit_count: 512,
    kv_bucket_count: 8,
    kv_slots_per_bucket: 64,
  }
}

fn assert_resume_rejected(
  checkpoint: &aeordb::engine::v4::gc_mark::MarkRunCheckpointV1<'_>,
  manifest: &aeordb::engine::v4::gc_mark::MarkWorkspaceManifestV1<'_>,
  manifest_bytes: &[u8],
  context: MarkResumeContextV1<'_>,
) {
  assert!(validate_mark_resume_context(checkpoint, manifest, manifest_bytes, &context).is_err());
}

#[test]
fn resume_requires_complete_checkpoint_manifest_and_context_closure() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let basis = fixture_basis(algorithm);
    let encoded = encode_mark_run_checkpoint(&checkpoint_request(algorithm, &basis, false)).unwrap();
    let GcMarkArtifactV1::Checkpoint(checkpoint) = decode_gc_mark_artifact(&encoded.value, algorithm).unwrap() else {
      panic!("expected checkpoint");
    };
    let manifest = decode_mark_workspace_manifest(&basis.manifest, algorithm).unwrap();
    let context = resume_context(algorithm, &basis);
    validate_mark_resume_context(&checkpoint, &manifest, &basis.manifest, &context).unwrap();

    let wrong_database = sequence::<16>(0xd1);
    let wrong_run = sequence::<16>(0xe1);
    let wrong_workspace = sequence::<16>(0xf1);
    let wrong_hash = sequence_vec(0xa1, algorithm.hash_length());
    let wrong_policy = sequence::<32>(0xb1);
    let wrong_registry = sequence::<32>(0xc1);
    let mut mismatch = context;
    mismatch.database_id = &wrong_database;
    assert_resume_rejected(&checkpoint, &manifest, &basis.manifest, mismatch);
    mismatch = context;
    mismatch.run_id = &wrong_run;
    assert_resume_rejected(&checkpoint, &manifest, &basis.manifest, mismatch);
    mismatch = context;
    mismatch.generation += 1;
    assert_resume_rejected(&checkpoint, &manifest, &basis.manifest, mismatch);
    mismatch = context;
    mismatch.checkpoint_sequence += 1;
    assert_resume_rejected(&checkpoint, &manifest, &basis.manifest, mismatch);
    mismatch = context;
    mismatch.workspace_path = "/srv/data/wrong-workspace";
    assert_resume_rejected(&checkpoint, &manifest, &basis.manifest, mismatch);
    mismatch = context;
    mismatch.workspace_id = &wrong_workspace;
    assert_resume_rejected(&checkpoint, &manifest, &basis.manifest, mismatch);
    mismatch = context;
    mismatch.authority_root_set_digest = &wrong_hash;
    assert_resume_rejected(&checkpoint, &manifest, &basis.manifest, mismatch);
    mismatch = context;
    mismatch.semantic_state_digest = &wrong_hash;
    assert_resume_rejected(&checkpoint, &manifest, &basis.manifest, mismatch);
    mismatch = context;
    mismatch.kv_layout_fingerprint = &wrong_hash;
    assert_resume_rejected(&checkpoint, &manifest, &basis.manifest, mismatch);
    mismatch = context;
    mismatch.effective_policy_fingerprint = &wrong_policy;
    assert_resume_rejected(&checkpoint, &manifest, &basis.manifest, mismatch);
    mismatch = context;
    mismatch.system_family_registry_fingerprint = &wrong_registry;
    assert_resume_rejected(&checkpoint, &manifest, &basis.manifest, mismatch);
    mismatch = context;
    mismatch.captured_header_sequence += 1;
    assert_resume_rejected(&checkpoint, &manifest, &basis.manifest, mismatch);
    mismatch = context;
    mismatch.captured_write_high_water += 1;
    assert_resume_rejected(&checkpoint, &manifest, &basis.manifest, mismatch);
    mismatch = context;
    mismatch.reconciled_through_sequence += 1;
    assert_resume_rejected(&checkpoint, &manifest, &basis.manifest, mismatch);
    mismatch = context;
    mismatch.active_bitmap_bit_count += 1;
    assert_resume_rejected(&checkpoint, &manifest, &basis.manifest, mismatch);
    mismatch = context;
    mismatch.kv_bucket_count += 1;
    assert_resume_rejected(&checkpoint, &manifest, &basis.manifest, mismatch);
    mismatch = context;
    mismatch.kv_slots_per_bucket += 1;
    assert_resume_rejected(&checkpoint, &manifest, &basis.manifest, mismatch);
    mismatch = context;
    mismatch.hash_algorithm = match algorithm {
      HashAlgorithm::Blake3_256 => HashAlgorithm::Sha512,
      HashAlgorithm::Sha512 => HashAlgorithm::Blake3_256,
      _ => unreachable!(),
    };
    assert_resume_rejected(&checkpoint, &manifest, &basis.manifest, mismatch);

    let mut tampered_manifest = basis.manifest.clone();
    tampered_manifest[88] ^= 1;
    assert_resume_rejected(&checkpoint, &manifest, &tampered_manifest, context);
  }
}

#[test]
fn checkpoint_and_control_writers_reject_malformed_or_dishonest_requests() {
  let algorithm = HashAlgorithm::Blake3_256;
  let basis = fixture_basis(algorithm);
  let mut request = checkpoint_request(algorithm, &basis, false);
  request.kv_slots_per_bucket = 63;
  assert!(encode_mark_run_checkpoint(&request).is_err());
  request = checkpoint_request(algorithm, &basis, false);
  request.capabilities[0] = 1;
  assert!(encode_mark_run_checkpoint(&request).is_err());
  request = checkpoint_request(algorithm, &basis, false);
  request.workspace_path = "relative/workspace";
  assert!(encode_mark_run_checkpoint(&request).is_err());

  assert!(encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::MarkRunCheckpoint,
    hash_algorithm: algorithm,
    database_id: &basis.database_id,
    slot: 0,
    sequence: 1,
    generation: 77,
    target_manifest_hash: &basis.mutation_journal_head,
  })
  .is_err());
}

#[test]
fn checkpoint_codecs_remain_disconnected_from_live_gc_service_and_v3_control_storage() {
  let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  for relative in
    ["engine/gc.rs", "engine/storage_engine.rs", "engine/task_worker.rs", "engine/v4/control_store.rs", "server/mod.rs", "server/routes.rs"]
  {
    let source = fs::read_to_string(root.join(relative)).unwrap();
    assert!(!source.contains("encode_mark_run_checkpoint"), "checkpoint encoder escaped into {relative}");
    assert!(!source.contains("encode_gc_active_control"), "GC active-control encoder escaped into {relative}");
    assert!(!source.contains("validate_mark_resume_context"), "resume validator escaped into {relative}");
  }
}
