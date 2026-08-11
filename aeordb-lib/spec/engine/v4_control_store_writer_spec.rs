use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

use aeordb::engine::directory_ops::{DirectoryOps, file_path_hash, whole_file_content_hash};
use aeordb::engine::entry_header::FLAG_SYSTEM;
use aeordb::engine::file_record::{CURRENT_FILE_RECORD_VERSION, FileRecord};
use aeordb::engine::v4::control_store::{SYSTEM_CONTROL_CONTENT_TYPE, V3TransitionControlStore, V4ControlStore};
use aeordb::engine::v4::database_header::{DatabaseHeaderVersion, read_database_header_read_only};
use aeordb::engine::v4::system_control::{SystemControlKindV1, SystemControlSlotV1, decode_system_control, system_control_path};
use aeordb::engine::{EntryType, HashAlgorithm, RequestContext, StorageEngine};

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/system-control-v1")
}

fn fixture(kind: SystemControlKindV1) -> Vec<u8> {
  fs::read(fixture_root().join(format!("control-blake3-256-{}-valid.bin", kind.slug()))).unwrap()
}

fn with_sequence(mut bytes: Vec<u8>, sequence: u64) -> Vec<u8> {
  bytes[16..24].copy_from_slice(&sequence.to_le_bytes());
  let checksum_offset = bytes.len() - 4;
  let checksum = crc32fast::hash(&bytes[..checksum_offset]);
  bytes[checksum_offset..].copy_from_slice(&checksum.to_le_bytes());
  bytes
}

fn decoded_identity(bytes: &[u8]) -> ([u8; 16], Vec<u8>) {
  let control = decode_system_control(bytes, HashAlgorithm::Blake3_256).unwrap();
  (control.database_id.try_into().unwrap(), control.identity)
}

fn stored_file_record(engine: &StorageEngine, path: &str) -> FileRecord {
  let path_key = file_path_hash(path, &HashAlgorithm::Blake3_256).unwrap();
  let (header, _, value) = engine.get_entry(&path_key).unwrap().expect("ControlStore path FileRecord");
  assert_eq!(header.entry_type, EntryType::FileRecord);
  assert_eq!(header.entry_version, CURRENT_FILE_RECORD_VERSION);
  assert_ne!(header.flags & FLAG_SYSTEM, 0);
  FileRecord::deserialize(&value, HashAlgorithm::Blake3_256.hash_length(), header.entry_version).unwrap()
}

fn assert_current_control_file(engine: &StorageEngine, path: &str, expected: &[u8]) {
  let record = stored_file_record(engine, path);
  assert_eq!(record.path, path);
  assert_eq!(record.content_type.as_deref(), Some(SYSTEM_CONTROL_CONTENT_TYPE));
  assert_eq!(record.total_size, expected.len() as u64);
  assert_eq!(record.content_hash, whole_file_content_hash(expected, &HashAlgorithm::Blake3_256).unwrap());
  assert_eq!(DirectoryOps::new(engine).read_file_buffered(path).unwrap(), expected);
}

fn modified_v1_wrapper_error<F>(name: &str, flags: u8, entry_version: u8, modify: F) -> String
where
  F: FnOnce(&mut FileRecord),
{
  let temporary = tempfile::tempdir().unwrap();
  let engine = StorageEngine::create(temporary.path().join(format!("{name}.aeordb")).to_str().unwrap()).unwrap();
  let bytes = fixture(SystemControlKindV1::DurabilityLatch);
  let (database_id, identity) = decoded_identity(&bytes);
  let path = system_control_path(SystemControlKindV1::DurabilityLatch, &identity, SystemControlSlotV1::A).unwrap();
  DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), &path, &bytes, Some(SYSTEM_CONTROL_CONTENT_TYPE)).unwrap();
  let path_key = file_path_hash(&path, &HashAlgorithm::Blake3_256).unwrap();
  let (header, _, value) = engine.get_entry(&path_key).unwrap().unwrap();
  let mut record = FileRecord::deserialize(&value, HashAlgorithm::Blake3_256.hash_length(), header.entry_version).unwrap();
  modify(&mut record);
  let serialized = record.serialize(HashAlgorithm::Blake3_256.hash_length()).unwrap();
  engine.store_entry_with_flags_and_version(EntryType::FileRecord, &path_key, &serialized, flags, entry_version).unwrap();
  V4ControlStore::new(&engine).load_mutable(SystemControlKindV1::DurabilityLatch, database_id, &identity).unwrap_err().to_string()
}

#[test]
fn v4_control_store_publishes_every_control_kind_as_a_v1_system_file_record() {
  let temporary = tempfile::tempdir().unwrap();
  let database_path = temporary.path().join("all-control-kinds.aeordb");
  let engine = StorageEngine::create(database_path.to_str().unwrap()).unwrap();
  let store = V4ControlStore::new(&engine);

  for kind in SystemControlKindV1::ALL {
    let bytes = fixture(kind);
    let (database_id, identity) = decoded_identity(&bytes);
    let durability_before = engine.durability_snapshot().unwrap().next_sequence;
    let selected_slot = if kind.is_immutable() {
      let loaded = store.publish_immutable(kind, database_id, &identity, &bytes).unwrap();
      assert_eq!(loaded.bytes, bytes);
      SystemControlSlotV1::Immutable
    } else {
      let loaded = store.publish_mutable(kind, database_id, &identity, &bytes).unwrap();
      assert_eq!(loaded.bytes, bytes);
      assert_eq!(loaded.selected_slot, SystemControlSlotV1::A);
      SystemControlSlotV1::A
    };
    assert_eq!(engine.durability_snapshot().unwrap().next_sequence, durability_before + 1, "kind {kind:?}");
    let path = system_control_path(kind, &identity, selected_slot).unwrap();
    assert_current_control_file(&engine, &path, &bytes);
  }

  engine.shutdown().unwrap();
  drop(engine);
  let mut database_file = fs::File::open(&database_path).unwrap();
  assert_eq!(read_database_header_read_only(&mut database_file).unwrap().version(), DatabaseHeaderVersion::V3);
}

#[test]
fn v4_control_store_rolls_legacy_v0_slots_forward_without_changing_payload_bytes() {
  let temporary = tempfile::tempdir().unwrap();
  let engine = StorageEngine::create(temporary.path().join("legacy-roll-forward.aeordb").to_str().unwrap()).unwrap();
  let initial = fixture(SystemControlKindV1::DurabilityLatch);
  let (database_id, identity) = decoded_identity(&initial);
  V3TransitionControlStore::new(&engine).publish_mutable(SystemControlKindV1::DurabilityLatch, database_id, &identity, &initial).unwrap();
  let a_path = system_control_path(SystemControlKindV1::DurabilityLatch, &identity, SystemControlSlotV1::A).unwrap();
  let a_key = file_path_hash(&a_path, &HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(engine.get_entry(&a_key).unwrap().unwrap().0.entry_version, 0);

  let store = V4ControlStore::new(&engine);
  assert_eq!(store.load_mutable(SystemControlKindV1::DurabilityLatch, database_id, &identity).unwrap().unwrap().bytes, initial);
  let second = with_sequence(fixture(SystemControlKindV1::DurabilityLatch), 8);
  let selected = store.publish_mutable(SystemControlKindV1::DurabilityLatch, database_id, &identity, &second).unwrap();
  assert_eq!(selected.selected_slot, SystemControlSlotV1::B);
  let b_path = system_control_path(SystemControlKindV1::DurabilityLatch, &identity, SystemControlSlotV1::B).unwrap();
  assert_current_control_file(&engine, &b_path, &second);
  assert_eq!(DirectoryOps::new(&engine).read_file_buffered(&a_path).unwrap(), initial);

  let third = with_sequence(fixture(SystemControlKindV1::DurabilityLatch), 9);
  let selected = store.publish_mutable(SystemControlKindV1::DurabilityLatch, database_id, &identity, &third).unwrap();
  assert_eq!(selected.selected_slot, SystemControlSlotV1::A);
  assert_current_control_file(&engine, &a_path, &third);
  assert_current_control_file(&engine, &b_path, &second);
}

#[test]
fn v4_control_store_immutable_publication_is_byte_exact_and_idempotent() {
  let temporary = tempfile::tempdir().unwrap();
  let database_path = temporary.path().join("immutable-idempotent.aeordb");
  let engine = StorageEngine::create(database_path.to_str().unwrap()).unwrap();
  let bytes = fixture(SystemControlKindV1::RootAdmissionCommit);
  let (database_id, identity) = decoded_identity(&bytes);
  let store = V4ControlStore::new(&engine);
  let first = store.publish_immutable(SystemControlKindV1::RootAdmissionCommit, database_id, &identity, &bytes).unwrap();
  assert_eq!(first.bytes, bytes);
  let path = system_control_path(SystemControlKindV1::RootAdmissionCommit, &identity, SystemControlSlotV1::Immutable).unwrap();
  assert_current_control_file(&engine, &path, &bytes);
  let durability_before_retry = engine.durability_snapshot().unwrap().next_sequence;

  let second = store.publish_immutable(SystemControlKindV1::RootAdmissionCommit, database_id, &identity, &bytes).unwrap();

  assert_eq!(second.bytes, bytes);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, durability_before_retry);
  assert_current_control_file(&engine, &path, &bytes);

  drop(store);
  engine.shutdown().unwrap();
  drop(engine);
  let reopened = StorageEngine::open(database_path.to_str().unwrap()).unwrap();
  let loaded =
    V4ControlStore::new(&reopened).load_immutable(SystemControlKindV1::RootAdmissionCommit, database_id, &identity).unwrap().unwrap();
  assert_eq!(loaded.bytes, bytes);
}

#[test]
fn v4_control_store_rolls_an_exact_immutable_v0_wrapper_forward_once() {
  let temporary = tempfile::tempdir().unwrap();
  let engine = StorageEngine::create(temporary.path().join("immutable-v0-roll-forward.aeordb").to_str().unwrap()).unwrap();
  let bytes = fixture(SystemControlKindV1::RootAdmissionCommit);
  let (database_id, identity) = decoded_identity(&bytes);
  let path = system_control_path(SystemControlKindV1::RootAdmissionCommit, &identity, SystemControlSlotV1::Immutable).unwrap();
  DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), &path, &bytes, Some(SYSTEM_CONTROL_CONTENT_TYPE)).unwrap();
  let path_key = file_path_hash(&path, &HashAlgorithm::Blake3_256).unwrap();
  let (header, _, value) = engine.get_entry(&path_key).unwrap().unwrap();
  let record = FileRecord::deserialize(&value, HashAlgorithm::Blake3_256.hash_length(), header.entry_version).unwrap();
  engine
    .store_entry_with_flags_and_version(
      EntryType::FileRecord,
      &path_key,
      &record.serialize_v0(HashAlgorithm::Blake3_256.hash_length()).unwrap(),
      FLAG_SYSTEM,
      0,
    )
    .unwrap();
  let durability_before = engine.durability_snapshot().unwrap().next_sequence;

  let loaded =
    V4ControlStore::new(&engine).publish_immutable(SystemControlKindV1::RootAdmissionCommit, database_id, &identity, &bytes).unwrap();

  assert_eq!(loaded.bytes, bytes);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, durability_before + 1);
  assert_current_control_file(&engine, &path, &bytes);
}

#[test]
fn v4_control_store_rejects_invalid_authority_before_namespace_publication() {
  let temporary = tempfile::tempdir().unwrap();
  let engine = StorageEngine::create(temporary.path().join("invalid-authority.aeordb").to_str().unwrap()).unwrap();
  let bytes = fixture(SystemControlKindV1::DurabilityLatch);
  let (database_id, identity) = decoded_identity(&bytes);
  let store = V4ControlStore::new(&engine);
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let mut wrong_database = database_id;
  wrong_database[0] ^= 1;
  assert!(store.publish_mutable(SystemControlKindV1::DurabilityLatch, wrong_database, &identity, &bytes).is_err());
  assert!(store.publish_mutable(SystemControlKindV1::EmergencySpillCatalog, database_id, &identity, &bytes).is_err());
  assert!(store.publish_immutable(SystemControlKindV1::RootAdmissionCommit, database_id, &identity, &bytes).is_err());
  let mut torn = bytes.clone();
  torn[40] ^= 1;
  assert!(store.publish_mutable(SystemControlKindV1::DurabilityLatch, database_id, &identity, &torn).is_err());
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  let path = system_control_path(SystemControlKindV1::DurabilityLatch, &identity, SystemControlSlotV1::A).unwrap();
  let path_key = file_path_hash(&path, &HashAlgorithm::Blake3_256).unwrap();
  assert!(engine.get_entry(&path_key).unwrap().is_none());
}

#[test]
fn v4_control_store_serializes_mutable_sequence_selection() {
  const PUBLISHERS: usize = 16;
  let temporary = tempfile::tempdir().unwrap();
  let engine = Arc::new(StorageEngine::create(temporary.path().join("concurrent-v1.aeordb").to_str().unwrap()).unwrap());
  let bytes = Arc::new(fixture(SystemControlKindV1::DurabilityLatch));
  let (database_id, identity) = decoded_identity(bytes.as_slice());
  let identity = Arc::new(identity);
  let barrier = Arc::new(Barrier::new(PUBLISHERS));
  let threads: Vec<_> = (0..PUBLISHERS)
    .map(|_| {
      let engine = Arc::clone(&engine);
      let bytes = Arc::clone(&bytes);
      let identity = Arc::clone(&identity);
      let barrier = Arc::clone(&barrier);
      std::thread::spawn(move || {
        barrier.wait();
        V4ControlStore::new(&engine).publish_mutable(
          SystemControlKindV1::DurabilityLatch,
          database_id,
          identity.as_slice(),
          bytes.as_slice(),
        )
      })
    })
    .collect();
  let results: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();

  assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1, "one mutable sequence may have only one winner: {results:?}");
  assert_eq!(
    V4ControlStore::new(&engine)
      .load_mutable(SystemControlKindV1::DurabilityLatch, database_id, identity.as_slice())
      .unwrap()
      .unwrap()
      .bytes,
    bytes.as_slice(),
  );
}

#[test]
fn v4_control_store_rejects_noncanonical_v1_file_record_metadata() {
  let temporary = tempfile::tempdir().unwrap();
  let engine = StorageEngine::create(temporary.path().join("wrong-file-record-metadata.aeordb").to_str().unwrap()).unwrap();
  let bytes = fixture(SystemControlKindV1::DurabilityLatch);
  let (database_id, identity) = decoded_identity(&bytes);
  let path = system_control_path(SystemControlKindV1::DurabilityLatch, &identity, SystemControlSlotV1::A).unwrap();
  DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), &path, &bytes, Some("application/octet-stream")).unwrap();

  let error = V4ControlStore::new(&engine).load_mutable(SystemControlKindV1::DurabilityLatch, database_id, &identity).unwrap_err();

  assert!(error.to_string().contains("content type"), "{error}");
}

#[test]
fn v4_control_store_rejects_every_noncanonical_v1_wrapper_shape() {
  let no_system_flag = modified_v1_wrapper_error("no-system-flag", 0, CURRENT_FILE_RECORD_VERSION, |_| {});
  assert!(no_system_flag.contains("system-flagged"), "{no_system_flag}");

  let unknown_version = modified_v1_wrapper_error("unknown-version", FLAG_SYSTEM, 2, |_| {});
  assert!(unknown_version.contains("compatible FileRecord"), "{unknown_version}");

  let wrong_path = modified_v1_wrapper_error("wrong-path", FLAG_SYSTEM, CURRENT_FILE_RECORD_VERSION, |record| {
    record.path = "/.aeordb-system/controls/v1/wrong.ctrl".to_string();
  });
  assert!(wrong_path.contains("path-key mismatch"), "{wrong_path}");

  let metadata = modified_v1_wrapper_error("metadata", FLAG_SYSTEM, CURRENT_FILE_RECORD_VERSION, |record| {
    record.metadata = b"not permitted".to_vec();
  });
  assert!(metadata.contains("metadata must be empty"), "{metadata}");

  let content_hash = modified_v1_wrapper_error("content-hash", FLAG_SYSTEM, CURRENT_FILE_RECORD_VERSION, |record| {
    record.content_hash[0] ^= 1;
  });
  assert!(content_hash.contains("content hash"), "{content_hash}");

  let body_cap = SystemControlKindV1::DurabilityLatch.encoded_cap() as u64;
  let declared_oversize = modified_v1_wrapper_error("declared-oversize", FLAG_SYSTEM, CURRENT_FILE_RECORD_VERSION, |record| {
    record.total_size = body_cap + 1;
  });
  assert!(declared_oversize.contains("exceeds cap"), "{declared_oversize}");

  let wrapper_oversize = modified_v1_wrapper_error("wrapper-oversize", FLAG_SYSTEM, CURRENT_FILE_RECORD_VERSION, |record| {
    record.metadata = vec![0x55; 16_384];
  });
  assert!(wrapper_oversize.contains("exceeds caller bound"), "{wrapper_oversize}");

  let missing_chunk = modified_v1_wrapper_error("missing-chunk", FLAG_SYSTEM, CURRENT_FILE_RECORD_VERSION, |record| {
    record.chunk_hashes[0] = vec![0x99; HashAlgorithm::Blake3_256.hash_length()];
  });
  assert!(missing_chunk.contains("Chunk not found"), "{missing_chunk}");
}

#[test]
fn v4_control_store_rejects_a_torn_control_body_inside_a_canonical_v1_wrapper() {
  let temporary = tempfile::tempdir().unwrap();
  let engine = StorageEngine::create(temporary.path().join("torn-body.aeordb").to_str().unwrap()).unwrap();
  let bytes = fixture(SystemControlKindV1::DurabilityLatch);
  let (database_id, identity) = decoded_identity(&bytes);
  let mut torn = bytes.clone();
  torn[40] ^= 1;
  let path = system_control_path(SystemControlKindV1::DurabilityLatch, &identity, SystemControlSlotV1::A).unwrap();
  DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), &path, &torn, Some(SYSTEM_CONTROL_CONTENT_TYPE)).unwrap();

  let error = V4ControlStore::new(&engine).load_mutable(SystemControlKindV1::DurabilityLatch, database_id, &identity).unwrap_err();

  assert!(error.to_string().contains("CRC"), "{error}");
}

#[test]
fn v4_control_store_writer_has_no_production_caller_or_string_path_authority() {
  fn rust_sources(path: PathBuf) -> String {
    let mut source = String::new();
    for entry in fs::read_dir(path).unwrap() {
      let entry = entry.unwrap();
      if entry.file_type().unwrap().is_dir() {
        source.push_str(&rust_sources(entry.path()));
      } else if entry.path().extension().and_then(|extension| extension.to_str()) == Some("rs") {
        source.push_str(&fs::read_to_string(entry.path()).unwrap());
      }
    }
    source
  }

  let production = rust_sources(Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
  assert_eq!(production.matches("V4ControlStore::new(").count(), 0);
  assert_eq!(production.matches("store_control_file_record_v1(").count(), 2);
  let directory_source = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine/directory_ops.rs")).unwrap();
  let writer_start = directory_source.find("  pub(crate) fn store_control_file_record_v1(").unwrap();
  let writer_end = directory_source[writer_start..].find("\n  /// Store multiple small files").unwrap() + writer_start;
  let writer = &directory_source[writer_start..writer_end];
  assert!(writer.contains("publication: &V4ControlPublicationContextV1"));
  assert!(writer.contains("std::ptr::eq(self.engine, publication.engine())"));
  assert!(writer.contains("publication.target_path(target_slot)?"));
  assert!(!writer.contains("starts_with"));
}
