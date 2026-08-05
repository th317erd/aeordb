use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

use aeordb::engine::directory_ops::{DirectoryOps, file_path_hash};
use aeordb::engine::durability_coordinator::{DurabilityOperation, OsErrorClass};
use aeordb::engine::entry_header::FLAG_SYSTEM;
use aeordb::engine::file_record::FileRecord;
use aeordb::engine::v4::control_store::V3TransitionControlStore;
use aeordb::engine::v4::database_header::{DatabaseHeaderVersion, read_database_header_read_only};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::system_control::{
  DurabilityLatchBodyV1, EmergencySpillCatalogBodyV1, EmergencySpillCatalogRowV1, SystemControlKindV1, SystemControlSlotV1,
  decode_system_control, encode_durability_latch_control, encode_emergency_spill_catalog_control, system_control_path,
};
use aeordb::engine::{EntryType, HashAlgorithm, RequestContext, StorageEngine};

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/system-control-v1")
}

fn fixture(name: &str) -> Vec<u8> {
  fs::read(fixture_root().join(name)).unwrap()
}

fn with_sequence(mut bytes: Vec<u8>, sequence: u64) -> Vec<u8> {
  bytes[16..24].copy_from_slice(&sequence.to_le_bytes());
  let crc_offset = bytes.len() - 4;
  let crc = crc32fast::hash(&bytes[..crc_offset]);
  bytes[crc_offset..].copy_from_slice(&crc.to_le_bytes());
  bytes
}

fn canonical_utf8(value: &str) -> Vec<u8> {
  let mut encoded = Vec::with_capacity(5 + value.len());
  encoded.push(0x07);
  encoded.extend_from_slice(&(value.len() as u32).to_le_bytes());
  encoded.extend_from_slice(value.as_bytes());
  encoded
}

fn recovery_controls(database_id: [u8; 16], active: bool) -> (Vec<u8>, Vec<u8>) {
  let algorithm = HashAlgorithm::Blake3_256;
  let catalog = EmergencySpillCatalogBodyV1 {
    database_id,
    catalog_generation: 1,
    discovered_at_ms: 1_725_000_000_000,
    state: if active { 1 } else { 3 },
    flags: 0,
    repair_receipt_hash: if active { vec![0; 32] } else { vec![0x51; 32] },
    rows: vec![EmergencySpillCatalogRowV1 {
      source_location_class: 1,
      replay_state: if active { 1 } else { 2 },
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
  let evidence_digest = digest_parts(algorithm, &[b"aeordb.durability-latch-evidence.v1\0", &database_id, catalog_body]);
  let latch = DurabilityLatchBodyV1 {
    database_id,
    latch_generation: 1,
    first_failure_at_ms: 1_725_000_000_000,
    latest_failure_at_ms: 1_725_000_000_001,
    severity: 1,
    state: if active { 1 } else { 3 },
    failed_operation: DurabilityOperation::AuthorityBarrier.stable_id(),
    os_error_class: OsErrorClass::MediaIo.stable_id(),
    os_error_code: 5,
    flags: 1,
    last_selected_header_sequence: 7,
    last_durable_write_sequence: 8,
    last_durable_publication_sequence: 8,
    emergency_spill_catalog_payload_hash: catalog_payload_hash,
    evidence_digest,
    diagnostic: canonical_utf8("serious durability failure; details redacted"),
  };
  let latch_bytes = encode_durability_latch_control(1, &latch, algorithm).unwrap();
  (catalog_bytes, latch_bytes)
}

fn publish_recovery_controls(engine: &StorageEngine, database_id: [u8; 16], active: bool) {
  let (catalog, latch) = recovery_controls(database_id, active);
  let store = V3TransitionControlStore::new(engine);
  store.publish_mutable(SystemControlKindV1::EmergencySpillCatalog, database_id, &[], &catalog).unwrap();
  store.publish_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[], &latch).unwrap();
}

#[test]
fn active_persistent_recovery_controls_restore_read_only_admission_after_reopen() {
  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("transition-controls-active.aeordb");
  let database_id = [0x42; 16];

  {
    let engine = StorageEngine::create(db_path.to_str().unwrap()).unwrap();
    DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), "/survives.txt", b"readable", Some("text/plain")).unwrap();
    publish_recovery_controls(&engine, database_id, true);
    engine.shutdown().unwrap();
  }

  let reopened = StorageEngine::open(db_path.to_str().unwrap()).unwrap();
  let recovery = reopened.persistent_durability_recovery().expect("active recovery authority restored");
  assert_eq!(recovery.database_id, database_id);
  assert!(recovery.blocks_writes);
  assert!(reopened.durability_failure().is_some(), "health/status must surface the persistent write block");
  assert_eq!(DirectoryOps::new(&reopened).read_file_buffered("/survives.txt").unwrap(), b"readable");
  let error = DirectoryOps::new(&reopened)
    .store_file_buffered(&RequestContext::system(), "/rejected.txt", b"must fail", Some("text/plain"))
    .unwrap_err();
  assert!(error.to_string().contains("explicit repair"), "{error}");
}

#[test]
fn completed_persistent_recovery_controls_preserve_identity_without_blocking_writes() {
  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("transition-controls-complete.aeordb");
  let database_id = [0x43; 16];

  {
    let engine = StorageEngine::create(db_path.to_str().unwrap()).unwrap();
    publish_recovery_controls(&engine, database_id, false);
    engine.shutdown().unwrap();
  }

  let reopened = StorageEngine::open(db_path.to_str().unwrap()).unwrap();
  let recovery = reopened.persistent_durability_recovery().expect("completed recovery identity retained");
  assert_eq!(recovery.database_id, database_id);
  assert!(!recovery.blocks_writes);
  DirectoryOps::new(&reopened).store_file_buffered(&RequestContext::system(), "/accepted.txt", b"accepted", Some("text/plain")).unwrap();
}

#[test]
fn contradictory_persistent_recovery_database_identities_fail_closed_before_writes() {
  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("transition-controls-mismatched.aeordb");
  {
    let engine = StorageEngine::create(db_path.to_str().unwrap()).unwrap();
    let (catalog, _) = recovery_controls([0x44; 16], true);
    let (_, latch) = recovery_controls([0x45; 16], true);
    let store = V3TransitionControlStore::new(&engine);
    store.publish_mutable(SystemControlKindV1::EmergencySpillCatalog, [0x44; 16], &[], &catalog).unwrap();
    store.publish_mutable(SystemControlKindV1::DurabilityLatch, [0x45; 16], &[], &latch).unwrap();
    engine.shutdown().unwrap();
  }

  let error = match StorageEngine::open(db_path.to_str().unwrap()) {
    Ok(_) => panic!("contradictory persistent recovery controls must fail closed"),
    Err(error) => error,
  };
  assert!(error.to_string().contains("different database identities"), "{error}");
}

#[test]
fn transition_control_store_publishes_v0_system_slots_and_selects_them_after_reopen() {
  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("transition-controls.aeordb");
  let initial = fixture("control-blake3-256-durability-latch-valid.bin");
  let initial_control = decode_system_control(&initial, HashAlgorithm::Blake3_256).unwrap();
  let database_id: [u8; 16] = initial_control.database_id.try_into().unwrap();

  {
    let engine = StorageEngine::create(db_path.to_str().unwrap()).unwrap();
    let store = V3TransitionControlStore::new(&engine);
    let first = store.publish_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[], &initial).unwrap();
    assert_eq!(first.selected_slot, SystemControlSlotV1::A);
    assert_eq!(first.sequence, 7);

    let a_path = system_control_path(SystemControlKindV1::DurabilityLatch, &[], SystemControlSlotV1::A).unwrap();
    let path_key = file_path_hash(&a_path, &HashAlgorithm::Blake3_256).unwrap();
    let (header, _, _) = engine.get_entry(&path_key).unwrap().expect("A slot FileRecord");
    assert_eq!(header.entry_version, 0);
    assert_ne!(header.flags & FLAG_SYSTEM, 0);
    assert_eq!(DirectoryOps::new(&engine).read_file_buffered(&a_path).unwrap(), initial);

    let second_bytes = with_sequence(fixture("control-blake3-256-durability-latch-valid.bin"), 8);
    let second = store.publish_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[], &second_bytes).unwrap();
    assert_eq!(second.selected_slot, SystemControlSlotV1::B);
    assert_eq!(second.sequence, 8);
    assert_eq!(store.load_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[]).unwrap().unwrap().bytes, second_bytes);
    assert_eq!(store.discover_mutable(SystemControlKindV1::DurabilityLatch, &[]).unwrap().unwrap().database_id, database_id);

    let stale = store.publish_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[], &initial).unwrap_err();
    assert!(stale.to_string().contains("expected next sequence 9"));
    engine.shutdown().unwrap();
  }

  let mut database_file = fs::File::open(&db_path).unwrap();
  assert_eq!(read_database_header_read_only(&mut database_file).unwrap().version(), DatabaseHeaderVersion::V3);

  let reopened = StorageEngine::open(db_path.to_str().unwrap()).unwrap();
  let selected =
    V3TransitionControlStore::new(&reopened).load_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[]).unwrap().unwrap();
  assert_eq!(selected.selected_slot, SystemControlSlotV1::B);
  assert_eq!(selected.sequence, 8);
  assert!(reopened.persistent_durability_recovery().unwrap().blocks_writes);
}

#[test]
fn transition_control_store_rejects_wrong_database_kind_and_unvalidated_bytes_without_writing() {
  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("transition-controls-reject.aeordb");
  let engine = StorageEngine::create(db_path.to_str().unwrap()).unwrap();
  let store = V3TransitionControlStore::new(&engine);
  let bytes = fixture("control-blake3-256-durability-latch-valid.bin");
  let decoded = decode_system_control(&bytes, HashAlgorithm::Blake3_256).unwrap();
  let database_id: [u8; 16] = decoded.database_id.try_into().unwrap();

  let mut wrong_database = database_id;
  wrong_database[0] ^= 1;
  assert!(store.publish_mutable(SystemControlKindV1::DurabilityLatch, wrong_database, &[], &bytes).is_err());
  assert!(store.publish_mutable(SystemControlKindV1::EmergencySpillCatalog, database_id, &[], &bytes).is_err());
  let mut torn = bytes.clone();
  torn[40] ^= 1;
  assert!(store.publish_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[], &torn).is_err());

  assert!(store.load_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[]).unwrap().is_none());
}

#[test]
fn transition_control_store_rejects_oversized_non_v0_and_wrong_type_slots_before_content_use() {
  let temp = tempfile::tempdir().unwrap();
  let database_id = [0x10; 16];
  let path = system_control_path(SystemControlKindV1::DurabilityLatch, &[], SystemControlSlotV1::A).unwrap();

  let oversized_path = temp.path().join("oversized.aeordb");
  let oversized = StorageEngine::create(oversized_path.to_str().unwrap()).unwrap();
  let path_key = file_path_hash(&path, &HashAlgorithm::Blake3_256).unwrap();
  let oversized_record = FileRecord {
    path: path.clone(),
    content_type: Some("application/octet-stream".to_string()),
    total_size: SystemControlKindV1::DurabilityLatch.encoded_cap() as u64 + 1,
    created_at: 1,
    updated_at: 1,
    metadata: Vec::new(),
    content_hash: Vec::new(),
    chunk_hashes: Vec::new(),
  };
  let value = oversized_record.serialize_v0(HashAlgorithm::Blake3_256.hash_length()).unwrap();
  oversized.store_entry_with_flags_and_version(EntryType::FileRecord, &path_key, &value, FLAG_SYSTEM, 0).unwrap();
  let error = V3TransitionControlStore::new(&oversized).load_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[]).unwrap_err();
  assert!(error.to_string().contains("exceeds cap"));

  let over_bound_path = temp.path().join("over-bound.aeordb");
  let over_bound = StorageEngine::create(over_bound_path.to_str().unwrap()).unwrap();
  let over_bound_record = FileRecord {
    path: path.clone(),
    content_type: Some("application/octet-stream".to_string()),
    total_size: 0,
    created_at: 1,
    updated_at: 1,
    metadata: vec![0x55; 5_000],
    content_hash: Vec::new(),
    chunk_hashes: Vec::new(),
  };
  let value = over_bound_record.serialize_v0(HashAlgorithm::Blake3_256.hash_length()).unwrap();
  over_bound.store_entry_with_flags_and_version(EntryType::FileRecord, &path_key, &value, FLAG_SYSTEM, 0).unwrap();
  let error = V3TransitionControlStore::new(&over_bound).load_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[]).unwrap_err();
  assert!(error.to_string().contains("exceeds caller bound 4096"));

  let non_system_path = temp.path().join("non-system.aeordb");
  let non_system = StorageEngine::create(non_system_path.to_str().unwrap()).unwrap();
  let value = oversized_record.serialize_v0(HashAlgorithm::Blake3_256.hash_length()).unwrap();
  non_system.store_entry_with_version(EntryType::FileRecord, &path_key, &value, 0).unwrap();
  let error = V3TransitionControlStore::new(&non_system).load_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[]).unwrap_err();
  assert!(error.to_string().contains("system-flagged FileRecord v0"));

  let v1_path = temp.path().join("v1.aeordb");
  let v1 = StorageEngine::create(v1_path.to_str().unwrap()).unwrap();
  DirectoryOps::new(&v1)
    .store_file_buffered(&RequestContext::system(), &path, b"not a transition v0 record", Some("application/octet-stream"))
    .unwrap();
  let error = V3TransitionControlStore::new(&v1).load_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[]).unwrap_err();
  assert!(error.to_string().contains("system-flagged FileRecord v0"));

  let wrong_type_path = temp.path().join("wrong-type.aeordb");
  let wrong_type = StorageEngine::create(wrong_type_path.to_str().unwrap()).unwrap();
  wrong_type.store_entry_with_flags(EntryType::Chunk, &path_key, b"wrong type", FLAG_SYSTEM).unwrap();
  let error = V3TransitionControlStore::new(&wrong_type).load_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[]).unwrap_err();
  assert!(error.to_string().contains("does not resolve to a FileRecord"));
}

#[test]
fn file_record_v0_serializer_round_trips_and_rejects_unknown_versions_and_bad_chunk_widths() {
  let record = FileRecord {
    path: "/.aeordb-system/controls/v1/test/a.ctrl".to_string(),
    content_type: Some("application/octet-stream".to_string()),
    total_size: 3,
    created_at: 10,
    updated_at: 11,
    metadata: vec![1, 2],
    content_hash: vec![0x33; 32],
    chunk_hashes: vec![vec![0x44; 32]],
  };
  let bytes = record.serialize_for_version(32, 0).unwrap();
  let decoded = FileRecord::deserialize(&bytes, 32, 0).unwrap();
  assert_eq!(decoded.path, record.path);
  assert_eq!(decoded.chunk_hashes, record.chunk_hashes);
  assert!(decoded.content_hash.is_empty());
  assert!(matches!(record.serialize_for_version(32, 2), Err(aeordb::engine::EngineError::InvalidEntryVersion(2))));

  let mut malformed = record;
  malformed.chunk_hashes[0].pop();
  assert!(malformed.serialize_for_version(32, 0).is_err());
  assert!(malformed.serialize_for_version(32, 1).is_err());

  let mut corrupt_count = bytes.clone();
  let count_offset = corrupt_count.len() - 32 - 4;
  corrupt_count[count_offset..count_offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
  assert!(FileRecord::deserialize(&corrupt_count, 32, 0).is_err());
  assert!(FileRecord::deserialize(&bytes, 0, 0).is_err());
}

#[test]
fn transition_control_store_rejects_sequence_exhaustion_without_replacing_selected_state() {
  let temp = tempfile::tempdir().unwrap();
  let engine = StorageEngine::create(temp.path().join("sequence-exhaustion.aeordb").to_str().unwrap()).unwrap();
  let bytes = with_sequence(fixture("control-blake3-256-durability-latch-valid.bin"), u64::MAX);
  let decoded = decode_system_control(&bytes, HashAlgorithm::Blake3_256).unwrap();
  let database_id: [u8; 16] = decoded.database_id.try_into().unwrap();
  let store = V3TransitionControlStore::new(&engine);

  let selected = store.publish_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[], &bytes).unwrap();
  assert_eq!(selected.sequence, u64::MAX);
  let error = store.publish_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[], &bytes).unwrap_err();
  assert!(error.to_string().contains("control sequence exhausted"));
  assert_eq!(store.load_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[]).unwrap().unwrap().bytes, bytes);
}

#[test]
fn transition_control_store_serializes_concurrent_publication_of_one_sequence() {
  const PUBLISHERS: usize = 16;
  let temp = tempfile::tempdir().unwrap();
  let engine = Arc::new(StorageEngine::create(temp.path().join("concurrent-publication.aeordb").to_str().unwrap()).unwrap());
  let bytes = Arc::new(fixture("control-blake3-256-durability-latch-valid.bin"));
  let decoded = decode_system_control(bytes.as_slice(), HashAlgorithm::Blake3_256).unwrap();
  let database_id: [u8; 16] = decoded.database_id.try_into().unwrap();
  let barrier = Arc::new(Barrier::new(PUBLISHERS));

  let threads: Vec<_> = (0..PUBLISHERS)
    .map(|_| {
      let engine = Arc::clone(&engine);
      let bytes = Arc::clone(&bytes);
      let barrier = Arc::clone(&barrier);
      std::thread::spawn(move || {
        barrier.wait();
        V3TransitionControlStore::new(&engine).publish_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[], bytes.as_slice())
      })
    })
    .collect();
  let results: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();
  let successes = results.iter().filter(|result| result.is_ok()).count();

  assert_eq!(successes, 1, "one sequence may have only one acknowledged publisher: {results:?}");
  assert_eq!(
    V3TransitionControlStore::new(&engine).load_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[]).unwrap().unwrap().bytes,
    bytes.as_slice()
  );
}

#[test]
fn transition_v0_writer_has_one_definition_and_control_store_is_its_only_caller() {
  fn visit(path: &Path, matches: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(path).unwrap() {
      let entry = entry.unwrap();
      let path = entry.path();
      if entry.file_type().unwrap().is_dir() {
        visit(&path, matches);
      } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
        let source = fs::read_to_string(&path).unwrap();
        if source.contains("store_transition_control_v0(") {
          matches.push(path);
        }
      }
    }
  }

  let mut matches = Vec::new();
  visit(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut matches);
  matches.sort();
  let relative: Vec<_> =
    matches.iter().map(|path| path.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap().to_string_lossy().replace('\\', "/")).collect();
  assert_eq!(relative, vec!["src/engine/directory_ops.rs", "src/engine/v4/control_store.rs"]);
}
