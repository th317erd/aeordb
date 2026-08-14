use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

use aeordb::engine::directory_ops::{DirectoryOps, file_path_hash, whole_file_content_hash};
use aeordb::engine::entry_header::FLAG_SYSTEM;
use aeordb::engine::file_record::{CURRENT_FILE_RECORD_VERSION, FileRecord};
use aeordb::engine::memory_coordinator::{AdmissionClass, MemoryOwner};
use aeordb::engine::v4::namespace::decode_semantic_object;
use aeordb::engine::v4::semantic_store::{SEMANTIC_OBJECT_CONTENT_TYPE, V4SemanticObjectStore, semantic_object_path};
use aeordb::engine::{EntryType, HashAlgorithm, RequestContext, StorageEngine};

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/semantic-object-v1")
}

fn fixture(algorithm: HashAlgorithm, name: &str) -> Vec<u8> {
  let algorithm_name = match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    other => panic!("fixture helper does not support {other:?}"),
  };
  fs::read(fixture_root().join(format!("asem-{algorithm_name}-{name}.bin"))).unwrap()
}

fn blake3_fixtures() -> Vec<Vec<u8>> {
  ["state-complete", "state-content-only", "catalog-leaf-valid", "catalog-internal-valid", "definition-valid"]
    .into_iter()
    .map(|name| fixture(HashAlgorithm::Blake3_256, name))
    .collect()
}

fn stored_file_record(engine: &StorageEngine, path: &str) -> FileRecord {
  let path_key = file_path_hash(path, &engine.hash_algo()).unwrap();
  let (header, _, value) = engine.get_entry(&path_key).unwrap().expect("semantic object FileRecord");
  assert_eq!(header.entry_type, EntryType::FileRecord);
  assert_eq!(header.entry_version, CURRENT_FILE_RECORD_VERSION);
  assert_ne!(header.flags & FLAG_SYSTEM, 0);
  FileRecord::deserialize(&value, engine.hash_algo().hash_length(), header.entry_version).unwrap()
}

fn assert_current_semantic_file(engine: &StorageEngine, path: &str, expected: &[u8]) {
  let record = stored_file_record(engine, path);
  assert_eq!(record.path, path);
  assert_eq!(record.content_type.as_deref(), Some(SEMANTIC_OBJECT_CONTENT_TYPE));
  assert!(record.metadata.is_empty());
  assert_eq!(record.total_size, expected.len() as u64);
  assert_eq!(record.content_hash, whole_file_content_hash(expected, &engine.hash_algo()).unwrap());
  assert_eq!(DirectoryOps::new(engine).read_file_buffered(path).unwrap(), expected);
}

#[test]
fn semantic_object_paths_are_exact_for_every_kind_and_hash_width() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for name in ["state-complete", "state-content-only", "catalog-leaf-valid", "catalog-internal-valid", "definition-valid"] {
      let bytes = fixture(algorithm, name);
      let object = decode_semantic_object(&bytes, algorithm).unwrap();
      assert_eq!(
        semantic_object_path(algorithm, object.kind_id, &object.object_id).unwrap(),
        format!("/.aeordb-system/semantic-objects/{:04x}/{:04x}/{}", algorithm.to_u16(), object.kind_id, hex::encode(&object.object_id))
      );
    }

    let hash_width = algorithm.hash_length();
    assert!(semantic_object_path(algorithm, 1, &vec![1; hash_width - 1]).is_err());
    assert!(semantic_object_path(algorithm, 1, &vec![0; hash_width]).is_err());
    assert!(semantic_object_path(algorithm, 0, &vec![1; hash_width]).is_err());
    assert!(semantic_object_path(algorithm, 5, &vec![1; hash_width]).is_err());
  }
}

#[test]
fn semantic_store_publishes_every_kind_as_an_exact_v1_system_file() {
  let temporary = tempfile::tempdir().unwrap();
  let engine = StorageEngine::create(temporary.path().join("all-kinds.aeordb").to_str().unwrap()).unwrap();
  let store = V4SemanticObjectStore::new(&engine);

  for bytes in blake3_fixtures() {
    let object = decode_semantic_object(&bytes, engine.hash_algo()).unwrap();
    let head_before = engine.head_hash().unwrap();
    let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
    let loaded = store.publish(&object.object_id, &bytes).unwrap();
    assert_eq!(loaded.object, object);
    assert_eq!(loaded.bytes, bytes);
    assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before + 1);
    assert_eq!(engine.head_hash().unwrap(), head_before, "semantic-object storage must not select namespace authority");
    assert_current_semantic_file(&engine, &loaded.path, &loaded.bytes);
  }
}

#[test]
fn semantic_store_concurrent_creation_is_write_once_idempotent_and_restart_safe() {
  const PUBLISHERS: usize = 16;
  let temporary = tempfile::tempdir().unwrap();
  let database_path = temporary.path().join("concurrent.aeordb");
  let engine = Arc::new(StorageEngine::create(database_path.to_str().unwrap()).unwrap());
  let bytes = Arc::new(fixture(HashAlgorithm::Blake3_256, "state-complete"));
  let object = decode_semantic_object(&bytes, engine.hash_algo()).unwrap();
  let object_id = Arc::new(object.object_id.clone());
  let barrier = Arc::new(Barrier::new(PUBLISHERS));
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let threads: Vec<_> = (0..PUBLISHERS)
    .map(|_| {
      let engine = Arc::clone(&engine);
      let bytes = Arc::clone(&bytes);
      let object_id = Arc::clone(&object_id);
      let barrier = Arc::clone(&barrier);
      std::thread::spawn(move || {
        barrier.wait();
        V4SemanticObjectStore::new(&engine).publish(object_id.as_slice(), bytes.as_slice())
      })
    })
    .collect();
  let results: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();

  assert!(results.iter().all(Result::is_ok), "exact concurrent retries must all resolve the same object: {results:?}");
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before + 1);
  let loaded = results[0].as_ref().unwrap();
  assert_current_semantic_file(&engine, &loaded.path, bytes.as_slice());
  let sequence_before_retry = engine.durability_snapshot().unwrap().next_sequence;
  let retry = V4SemanticObjectStore::new(&engine).publish(object_id.as_slice(), bytes.as_slice()).unwrap();
  assert_eq!(retry.bytes, bytes.as_slice());
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before_retry);

  let path = loaded.path.clone();
  engine.shutdown().unwrap();
  drop(results);
  drop(engine);
  let reopened = StorageEngine::open(database_path.to_str().unwrap()).unwrap();
  let loaded = V4SemanticObjectStore::new(&reopened).load(object.kind_id, &object.object_id).unwrap().unwrap();
  assert_eq!(loaded.path, path);
  assert_eq!(loaded.bytes, bytes.as_slice());
  reopened.shutdown().unwrap();
}

#[test]
fn semantic_store_rejects_identity_disagreement_and_existing_byte_collision_without_overwrite() {
  let temporary = tempfile::tempdir().unwrap();
  let engine = StorageEngine::create(temporary.path().join("collision.aeordb").to_str().unwrap()).unwrap();
  let expected = fixture(HashAlgorithm::Blake3_256, "state-complete");
  let expected_object = decode_semantic_object(&expected, engine.hash_algo()).unwrap();
  let other = fixture(HashAlgorithm::Blake3_256, "definition-valid");
  let path = semantic_object_path(engine.hash_algo(), expected_object.kind_id, &expected_object.object_id).unwrap();

  let mut wrong_id = expected_object.object_id.clone();
  wrong_id[0] ^= 1;
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  assert!(V4SemanticObjectStore::new(&engine).publish(&wrong_id, &expected).is_err());
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert!(V4SemanticObjectStore::new(&engine).load(expected_object.kind_id, &expected_object.object_id).unwrap().is_none());

  DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), &path, &other, Some(SEMANTIC_OBJECT_CONTENT_TYPE)).unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let error = V4SemanticObjectStore::new(&engine).publish(&expected_object.object_id, &expected).unwrap_err();
  assert!(error.to_string().contains("identity") || error.to_string().contains("different"), "unexpected collision error: {error}");
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert_eq!(DirectoryOps::new(&engine).read_file_buffered(&path).unwrap(), other);
}

#[test]
fn semantic_store_enforces_the_per_kind_cap_before_decoding() {
  let temporary = tempfile::tempdir().unwrap();
  let engine = StorageEngine::create(temporary.path().join("kind-cap.aeordb").to_str().unwrap()).unwrap();
  let mut bytes = fixture(HashAlgorithm::Blake3_256, "state-complete");
  let object = decode_semantic_object(&bytes, engine.hash_algo()).unwrap();
  bytes.resize(4_097, 0);
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;

  let error = V4SemanticObjectStore::new(&engine).publish(&object.object_id, &bytes).unwrap_err();

  assert!(error.to_string().contains("exceeds cap 4096"), "{error}");
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert!(V4SemanticObjectStore::new(&engine).load(object.kind_id, &object.object_id).unwrap().is_none());
}

fn modified_wrapper_error<F>(name: &str, flags: u8, entry_version: u8, modify: F) -> String
where
  F: FnOnce(&mut FileRecord),
{
  let temporary = tempfile::tempdir().unwrap();
  let engine = StorageEngine::create(temporary.path().join(format!("{name}.aeordb")).to_str().unwrap()).unwrap();
  let bytes = fixture(HashAlgorithm::Blake3_256, "state-complete");
  let object = decode_semantic_object(&bytes, engine.hash_algo()).unwrap();
  let path = semantic_object_path(engine.hash_algo(), object.kind_id, &object.object_id).unwrap();
  DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), &path, &bytes, Some(SEMANTIC_OBJECT_CONTENT_TYPE)).unwrap();
  let path_key = file_path_hash(&path, &engine.hash_algo()).unwrap();
  let (header, _, value) = engine.get_entry(&path_key).unwrap().unwrap();
  let mut record = FileRecord::deserialize(&value, engine.hash_algo().hash_length(), header.entry_version).unwrap();
  modify(&mut record);
  let serialized = record.serialize_for_version(engine.hash_algo().hash_length(), entry_version.min(CURRENT_FILE_RECORD_VERSION)).unwrap();
  engine.store_entry_with_flags_and_version(EntryType::FileRecord, &path_key, &serialized, flags, entry_version).unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let error = V4SemanticObjectStore::new(&engine).load(object.kind_id, &object.object_id).unwrap_err().to_string();
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  error
}

#[test]
fn semantic_store_rejects_every_noncanonical_file_record_wrapper() {
  let no_system_flag = modified_wrapper_error("no-system", 0, CURRENT_FILE_RECORD_VERSION, |_| {});
  assert!(no_system_flag.contains("system-flagged"), "{no_system_flag}");
  let legacy_version = modified_wrapper_error("legacy", FLAG_SYSTEM, 0, |_| {});
  assert!(legacy_version.contains("FileRecord v1"), "{legacy_version}");
  let unknown_version = modified_wrapper_error("unknown-version", FLAG_SYSTEM, 2, |_| {});
  assert!(unknown_version.contains("FileRecord v1"), "{unknown_version}");
  let wrong_path = modified_wrapper_error("wrong-path", FLAG_SYSTEM, CURRENT_FILE_RECORD_VERSION, |record| {
    record.path = "/.aeordb-system/semantic-objects/wrong".to_string();
  });
  assert!(wrong_path.contains("path-key mismatch"), "{wrong_path}");
  let wrong_content_type = modified_wrapper_error("content-type", FLAG_SYSTEM, CURRENT_FILE_RECORD_VERSION, |record| {
    record.content_type = Some("application/octet-stream".to_string());
  });
  assert!(wrong_content_type.contains("content type"), "{wrong_content_type}");
  let metadata = modified_wrapper_error("metadata", FLAG_SYSTEM, CURRENT_FILE_RECORD_VERSION, |record| {
    record.metadata = b"forbidden".to_vec();
  });
  assert!(metadata.contains("metadata must be empty"), "{metadata}");
  let content_hash = modified_wrapper_error("content-hash", FLAG_SYSTEM, CURRENT_FILE_RECORD_VERSION, |record| {
    record.content_hash[0] ^= 1;
  });
  assert!(content_hash.contains("content hash"), "{content_hash}");
  let declared_oversize = modified_wrapper_error("declared-oversize", FLAG_SYSTEM, CURRENT_FILE_RECORD_VERSION, |record| {
    record.total_size = 1_048_577;
  });
  assert!(declared_oversize.contains("exceeds"), "{declared_oversize}");
  let missing_chunk = modified_wrapper_error("missing-chunk", FLAG_SYSTEM, CURRENT_FILE_RECORD_VERSION, |record| {
    record.chunk_hashes[0] = vec![0x99; HashAlgorithm::Blake3_256.hash_length()];
  });
  assert!(missing_chunk.contains("Chunk not found"), "{missing_chunk}");

  let temporary = tempfile::tempdir().unwrap();
  let engine = StorageEngine::create(temporary.path().join("wrong-type.aeordb").to_str().unwrap()).unwrap();
  let bytes = fixture(HashAlgorithm::Blake3_256, "state-complete");
  let object = decode_semantic_object(&bytes, engine.hash_algo()).unwrap();
  let path = semantic_object_path(engine.hash_algo(), object.kind_id, &object.object_id).unwrap();
  let path_key = file_path_hash(&path, &engine.hash_algo()).unwrap();
  engine.store_entry(EntryType::DirectoryIndex, &path_key, b"wrong type").unwrap();
  let error = V4SemanticObjectStore::new(&engine).load(object.kind_id, &object.object_id).unwrap_err();
  assert!(error.to_string().contains("does not resolve to a FileRecord"), "{error}");
}

#[test]
fn semantic_store_memory_refusal_cannot_publish_a_path_or_root() {
  let temporary = tempfile::tempdir().unwrap();
  let engine = StorageEngine::create(temporary.path().join("memory.aeordb").to_str().unwrap()).unwrap();
  let bytes = fixture(HashAlgorithm::Blake3_256, "catalog-leaf-valid");
  let object = decode_semantic_object(&bytes, engine.hash_algo()).unwrap();
  let path = semantic_object_path(engine.hash_algo(), object.kind_id, &object.object_id).unwrap();
  let head_before = engine.head_hash().unwrap();
  let coordinator = engine.memory_coordinator();
  let snapshot = coordinator.snapshot().unwrap();
  let policy = snapshot.policy.unwrap();
  let available = policy.ordinary_limit_bytes().saturating_sub(snapshot.accounted_bytes);
  assert!(available > 64);
  let pressure = coordinator.reserve(MemoryOwner::Query, available - 64, AdmissionClass::Workload).unwrap();

  let result = V4SemanticObjectStore::new(&engine).publish(&object.object_id, &bytes);

  assert!(matches!(result, Err(aeordb::engine::EngineError::ResourceExhausted(_))), "unexpected pressure result: {result:?}");
  assert_eq!(engine.head_hash().unwrap(), head_before);
  let path_key = file_path_hash(&path, &engine.hash_algo()).unwrap();
  assert!(engine.get_entry(&path_key).unwrap().is_none());
  drop(pressure);
}

#[test]
fn semantic_store_has_only_the_bounded_read_source_and_no_service_or_string_path_publication_caller() {
  let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
  let semantic_store = fs::read_to_string(root.join("aeordb-lib/src/engine/v4/semantic_store.rs")).unwrap();
  for forbidden in ["server::", "update_head", "admit_v4_header", "V4ControlStore", "publish_namespace_root"] {
    assert!(!semantic_store.contains(forbidden), "semantic store unexpectedly contains {forbidden}");
  }
  let production = read_rust_sources(&root.join("aeordb-lib/src"));
  assert_eq!(
    production.matches("V4SemanticObjectStore::new").count(),
    1,
    "semantic store gained a production caller beyond the disconnected bounded read source"
  );
  let semantic_source = fs::read_to_string(root.join("aeordb-lib/src/engine/v4/index_semantic_source.rs")).unwrap();
  assert!(semantic_source.contains("V4SemanticObjectStore::new(self.engine)"));
  assert!(semantic_source.contains(".load(kind_id, object_id)"));
  assert!(!semantic_source.contains(".publish("), "the disconnected semantic source must remain read-only");
  assert_eq!(production.matches("store_semantic_file_record_v1").count(), 2, "semantic publication must have one owner and one adapter");
}

fn read_rust_sources(path: &Path) -> String {
  let mut source = String::new();
  for entry in fs::read_dir(path).unwrap() {
    let path = entry.unwrap().path();
    if path.is_dir() {
      source.push_str(&read_rust_sources(&path));
    } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
      source.push_str(&fs::read_to_string(path).unwrap());
    }
  }
  source
}
