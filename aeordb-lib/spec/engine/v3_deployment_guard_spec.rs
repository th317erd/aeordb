use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};

use aeordb::engine::durability_coordinator::{DurabilityOperation, OsErrorClass};
use aeordb::engine::v4::control_store::V3TransitionControlStore;
use aeordb::engine::v4::database_header::{ReadOnlyDatabaseHeader, read_database_header_read_only};
use aeordb::engine::v4::deployment_guard::{
  TRANSITION_RECOVERY_CAPABILITY_V1, current_deployment_capabilities, evaluate_deployment_candidate,
  inspect_deployment_transition_state_read_only, inspect_deployment_transition_state_with_spill_dirs_read_only,
};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::system_control::{
  DurabilityLatchBodyV1, EmergencySpillCatalogBodyV1, EmergencySpillCatalogRowV1, SystemControlKindV1, SystemControlSlotV1,
  decode_system_control, encode_durability_latch_control, encode_emergency_spill_catalog_control, system_control_path,
};
use aeordb::engine::{EntryType, HashAlgorithm, StorageEngine};
use aeordb::engine::directory_ops::file_path_hash;
use aeordb::engine::kv_pages::bucket_page_offset;
use aeordb::engine::kv_stages::stage_params;
use aeordb::engine::kv_nvt::KvNvt;

fn canonical_utf8(value: &str) -> Vec<u8> {
  let mut encoded = Vec::with_capacity(5 + value.len());
  encoded.push(0x07);
  encoded.extend_from_slice(&(value.len() as u32).to_le_bytes());
  encoded.extend_from_slice(value.as_bytes());
  encoded
}

fn recovery_controls(database_id: [u8; 16], latch_state: u16, catalog_state: u16) -> (Vec<u8>, Vec<u8>) {
  let algorithm = HashAlgorithm::Blake3_256;
  let catalog = EmergencySpillCatalogBodyV1 {
    database_id,
    catalog_generation: 1,
    discovered_at_ms: 1_725_000_000_000,
    state: catalog_state,
    flags: 0,
    repair_receipt_hash: if catalog_state == 3 { vec![0x51; 32] } else { vec![0; 32] },
    rows: vec![EmergencySpillCatalogRowV1 {
      source_location_class: 1,
      replay_state: if catalog_state == 3 { 2 } else { 1 },
      path_encoding: 1,
      flags: 0,
      created_at_ms: 1_725_000_000_000,
      creation_sequence: 9,
      file_length: 4_096,
      complete_file_digest: vec![0x31; 32],
      native_path: b"/var/lib/aeordb/spill/incident/manifest.json".to_vec(),
    }],
  };
  let catalog_bytes = encode_emergency_spill_catalog_control(1, &catalog, algorithm).unwrap();
  let catalog_body = decode_system_control(&catalog_bytes, algorithm).unwrap().body;
  let catalog_payload_hash = digest_parts(algorithm, &[b"aeordb.emergency-spill-catalog-payload.v1\0", catalog_body]);
  let latch = DurabilityLatchBodyV1 {
    database_id,
    latch_generation: 1,
    first_failure_at_ms: 1_725_000_000_000,
    latest_failure_at_ms: 1_725_000_000_001,
    severity: 1,
    state: latch_state,
    failed_operation: DurabilityOperation::AuthorityBarrier.stable_id(),
    os_error_class: OsErrorClass::MediaIo.stable_id(),
    os_error_code: 5,
    flags: 1,
    last_selected_header_sequence: 7,
    last_durable_write_sequence: 8,
    last_durable_publication_sequence: 8,
    emergency_spill_catalog_payload_hash: catalog_payload_hash,
    evidence_digest: digest_parts(algorithm, &[b"aeordb.durability-latch-evidence.v1\0", &database_id, catalog_body]),
    diagnostic: canonical_utf8("serious durability failure; details redacted"),
  };
  let latch_bytes = encode_durability_latch_control(1, &latch, algorithm).unwrap();
  (catalog_bytes, latch_bytes)
}

fn publish_recovery_controls(engine: &StorageEngine, database_id: [u8; 16], latch_state: u16, catalog_state: u16) {
  let (catalog, latch) = recovery_controls(database_id, latch_state, catalog_state);
  let store = V3TransitionControlStore::new(engine);
  store.publish_mutable(SystemControlKindV1::EmergencySpillCatalog, database_id, &[], &catalog).unwrap();
  store.publish_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[], &latch).unwrap();
}

#[test]
fn read_only_inspection_preserves_database_bytes_and_reports_no_transition_state() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("inactive.aeordb");
  StorageEngine::create(database.to_str().unwrap()).unwrap().shutdown().unwrap();
  let before = fs::read(&database).unwrap();

  let state = inspect_deployment_transition_state_read_only(&database).unwrap();

  assert!(!state.requires_transition_capability);
  assert!(state.persistent_recovery.is_none());
  assert_eq!(state.external_spill_count, 0);
  assert_eq!(fs::read(&database).unwrap(), before, "inspection must not write even a header or hot-tail byte");
}

#[test]
fn read_only_inspection_accepts_boundary_aligned_kv_expansion_slack() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("expanded.aeordb");
  let engine = StorageEngine::create(database.to_str().unwrap()).unwrap();
  engine.store_entry(EntryType::DirectoryIndex, &[0x71; 32], &vec![0x5a; 600 * 1024]).unwrap();
  engine.expand_kv_block_online(1).unwrap();
  engine.shutdown().unwrap();

  let header = {
    let mut file = fs::File::open(&database).unwrap();
    match read_database_header_read_only(&mut file).unwrap() {
      ReadOnlyDatabaseHeader::V3 { header, .. } => header,
      ReadOnlyDatabaseHeader::V4(_) => unreachable!(),
    }
  };
  let (minimum_length, _) =
    stage_params(header.kv_block_stage as usize, aeordb::engine::kv_pages::page_size(header.hash_algo.hash_length()));
  assert!(header.kv_block_length > minimum_length, "fixture must include legal boundary-alignment slack");

  let state = inspect_deployment_transition_state_read_only(&database).unwrap();
  assert!(!state.requires_transition_capability);
}

#[test]
fn read_only_inspection_detects_active_and_repairing_controls_from_the_hot_tail() {
  for (name, latch_state, catalog_state) in [("active", 1, 1), ("repairing", 2, 2)] {
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join(format!("{name}.aeordb"));
    let engine = StorageEngine::create(database.to_str().unwrap()).unwrap();
    publish_recovery_controls(&engine, [latch_state as u8; 16], latch_state, catalog_state);

    let state = inspect_deployment_transition_state_read_only(&database).unwrap();

    assert!(state.requires_transition_capability, "{name}");
    let recovery = state.persistent_recovery.expect("selected controls");
    assert!(recovery.blocks_writes, "{name}");
    assert_eq!(recovery.latch_state, Some(latch_state));
    assert_eq!(recovery.catalog_state, Some(catalog_state));
    engine.shutdown().unwrap();
  }
}

#[test]
fn read_only_inspection_treats_completed_controls_as_identity_not_an_active_downgrade_block() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("complete.aeordb");
  let engine = StorageEngine::create(database.to_str().unwrap()).unwrap();
  publish_recovery_controls(&engine, [0x43; 16], 3, 3);
  engine.shutdown().unwrap();

  let state = inspect_deployment_transition_state_read_only(&database).unwrap();

  assert!(!state.requires_transition_capability);
  assert!(!state.persistent_recovery.unwrap().blocks_writes);
}

#[test]
fn deployment_policy_allows_compatible_upgrades_but_refuses_incompatible_downgrades_only_when_state_is_active() {
  let mut active = aeordb::engine::v4::deployment_guard::DeploymentTransitionStateV1::inactive_v3();
  active.requires_transition_capability = true;
  active.reasons.push("active durability recovery".to_string());
  let inactive = aeordb::engine::v4::deployment_guard::DeploymentTransitionStateV1::inactive_v3();

  assert!(evaluate_deployment_candidate(&active, Some(TRANSITION_RECOVERY_CAPABILITY_V1)).allowed);
  let refused = evaluate_deployment_candidate(&active, None);
  assert!(!refused.allowed);
  assert!(refused.message.contains("active durability recovery"));
  assert!(evaluate_deployment_candidate(&inactive, None).allowed);
}

#[test]
fn current_binary_advertises_the_exact_versioned_transition_recovery_capability() {
  let report = current_deployment_capabilities();
  assert_eq!(report.protocol_version, 1);
  assert_eq!(report.product, "aeordb");
  assert_eq!(report.capabilities, vec![TRANSITION_RECOVERY_CAPABILITY_V1.to_string()]);
}

#[test]
fn corrupt_hot_tail_fails_closed_instead_of_claiming_the_database_is_inactive() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("corrupt-hot-tail.aeordb");
  StorageEngine::create(database.to_str().unwrap()).unwrap().shutdown().unwrap();
  let hot_tail_offset = {
    let mut file = fs::File::open(&database).unwrap();
    match read_database_header_read_only(&mut file).unwrap() {
      ReadOnlyDatabaseHeader::V3 { header, .. } => header.hot_tail_offset,
      ReadOnlyDatabaseHeader::V4(_) => unreachable!(),
    }
  };
  let mut file = fs::OpenOptions::new().write(true).open(&database).unwrap();
  file.seek(SeekFrom::Start(hot_tail_offset)).unwrap();
  file.write_all(b"BROKEN").unwrap();
  file.sync_all().unwrap();

  let error = inspect_deployment_transition_state_read_only(&database).unwrap_err();

  assert!(error.to_string().contains("hot tail"), "{error}");
}

#[test]
fn corrupt_control_kv_page_fails_closed_instead_of_skipping_persistent_authority() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("corrupt-kv.aeordb");
  StorageEngine::create(database.to_str().unwrap()).unwrap().shutdown().unwrap();
  let header = {
    let mut file = fs::File::open(&database).unwrap();
    match read_database_header_read_only(&mut file).unwrap() {
      ReadOnlyDatabaseHeader::V3 { header, .. } => header,
      ReadOnlyDatabaseHeader::V4(_) => unreachable!(),
    }
  };
  let hash_length = header.hash_algo.hash_length();
  let (_, bucket_count) = stage_params(header.kv_block_stage as usize, aeordb::engine::kv_pages::page_size(hash_length));
  let path = system_control_path(SystemControlKindV1::DurabilityLatch, &[], SystemControlSlotV1::A).unwrap();
  let path_key = file_path_hash(&path, &header.hash_algo).unwrap();
  let nvt = KvNvt::new(bucket_count);
  let page_offset = header.kv_block_offset + bucket_page_offset(nvt.bucket_for_value(&path_key), hash_length);
  let mut file = fs::OpenOptions::new().read(true).write(true).open(&database).unwrap();
  file.seek(SeekFrom::Start(page_offset)).unwrap();
  let mut first = [0u8; 1];
  file.read_exact(&mut first).unwrap();
  file.seek(SeekFrom::Start(page_offset)).unwrap();
  file.write_all(&[first[0] ^ 0xff]).unwrap();
  file.sync_all().unwrap();

  let error = inspect_deployment_transition_state_read_only(&database).unwrap_err();

  assert!(error.to_string().contains("KV") || error.to_string().contains("page"), "{error}");
}

#[test]
fn unapplied_external_spill_blocks_an_old_candidate_even_without_persistent_controls() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("external-spill.aeordb");
  StorageEngine::create(database.to_str().unwrap()).unwrap().shutdown().unwrap();
  let spill_root = temp.path().join("spill-root");
  let incident = spill_root.join("incident");
  fs::create_dir_all(&incident).unwrap();
  fs::write(
    incident.join("manifest.json"),
    serde_json::to_vec_pretty(&serde_json::json!({
      "format": "aeordb-emergency-spill-v1",
      "attempted_at": "2026-06-15T09:00:00Z",
      "db_path": database.display().to_string(),
      "hot_tail_writes": 1,
      "hot_tail_voids": 0,
      "wal_tail_bytes": 0
    }))
    .unwrap(),
  )
  .unwrap();

  let state = inspect_deployment_transition_state_with_spill_dirs_read_only(&database, &[spill_root]).unwrap();

  assert!(state.requires_transition_capability);
  assert_eq!(state.external_spill_count, 1);
  assert!(state.reasons[0].contains("external emergency spill"));
  assert!(!evaluate_deployment_candidate(&state, None).allowed);
}
