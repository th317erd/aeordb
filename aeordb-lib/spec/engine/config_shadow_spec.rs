use std::ffi::{OsStr, OsString};

use aeordb::engine::config_resolver::{ConfigDocumentStatus, ConfigSource, ConfigValue, MAX_CONFIG_DOCUMENT_BYTES, RUNTIME_CONFIG_PATH};
use aeordb::engine::compression::{CompressionAlgorithm, compress};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::entry_type::EntryType;
use aeordb::engine::lifecycle_config::{LIFECYCLE_CONFIG_PATH, load_lifecycle_config};
use aeordb::engine::{RequestContext, StorageEngine};
use serial_test::serial;

fn database_path(directory: &tempfile::TempDir) -> String {
  directory.path().join("shadow.aeordb").to_string_lossy().into_owned()
}

fn create_engine(directory: &tempfile::TempDir) -> StorageEngine {
  let engine = StorageEngine::create(&database_path(directory)).unwrap();
  DirectoryOps::new(&engine).ensure_root_directory(&RequestContext::system()).unwrap();
  engine
}

fn store_config(engine: &StorageEngine, path: &str, bytes: &[u8]) {
  DirectoryOps::new(engine).store_file_buffered(&RequestContext::system(), path, bytes, Some("application/json")).unwrap();
}

fn reopen(engine: StorageEngine, path: &str) -> StorageEngine {
  engine.shutdown().unwrap();
  drop(engine);
  StorageEngine::open(path).unwrap()
}

struct EnvironmentGuard {
  name: &'static str,
  previous: Option<OsString>,
}

impl EnvironmentGuard {
  fn set(name: &'static str, value: impl AsRef<OsStr>) -> Self {
    let previous = std::env::var_os(name);
    unsafe { std::env::set_var(name, value) };
    Self { name, previous }
  }
}

impl Drop for EnvironmentGuard {
  fn drop(&mut self) {
    unsafe {
      match &self.previous {
        Some(value) => std::env::set_var(self.name, value),
        None => std::env::remove_var(self.name),
      }
    }
  }
}

#[test]
#[serial]
fn new_engine_captures_one_complete_default_shadow_without_writing_config() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let report = engine.configuration_shadow();
  let resolution = report.resolution.as_ref().expect("host context should be available");

  assert!(resolution.complete(), "{:?}", resolution.issues);
  assert_eq!(resolution.runtime_status, ConfigDocumentStatus::Missing);
  assert_eq!(resolution.lifecycle_status, ConfigDocumentStatus::Missing);
  assert_eq!(resolution.properties.len(), 41);
  assert!(DirectoryOps::new(&engine).get_metadata(RUNTIME_CONFIG_PATH).unwrap().is_none());
  assert!(DirectoryOps::new(&engine).get_metadata(LIFECYCLE_CONFIG_PATH).unwrap().is_none());
}

#[test]
#[serial]
fn reopen_shadow_reads_valid_runtime_and_lifecycle_documents() {
  let directory = tempfile::tempdir().unwrap();
  let path = database_path(&directory);
  let engine = create_engine(&directory);
  store_config(&engine, RUNTIME_CONFIG_PATH, br#"{"schema_version":1,"index":{"flush_after_seconds":45}}"#);
  store_config(
    &engine,
    LIFECYCLE_CONFIG_PATH,
    br#"{"schema_version":1,"snapshot_writes_enabled":false,"garbage_collection":{"pending_delete_grace_seconds":0}}"#,
  );

  let engine = reopen(engine, &path);
  let report = engine.configuration_shadow();
  let resolution = report.resolution.as_ref().unwrap();
  assert!(resolution.complete(), "{:?}", resolution.issues);
  assert_eq!(resolution.runtime_status, ConfigDocumentStatus::Valid { schema_version: 1 });
  assert_eq!(resolution.lifecycle_status, ConfigDocumentStatus::Valid { schema_version: 1 });
  assert_eq!(resolution.property("index.flush_after_seconds").unwrap().value, Some(ConfigValue::Unsigned(45)));
  assert_eq!(resolution.property("index.flush_after_seconds").unwrap().source, Some(ConfigSource::StoredRuntimeV1));
  assert_eq!(resolution.property("lifecycle.snapshot_writes_enabled").unwrap().value, Some(ConfigValue::Boolean(false)));
}

#[test]
#[serial]
fn malformed_shadow_is_visible_but_does_not_activate_or_disable_live_legacy_owners() {
  let directory = tempfile::tempdir().unwrap();
  let path = database_path(&directory);
  let engine = create_engine(&directory);
  store_config(&engine, RUNTIME_CONFIG_PATH, b"{");
  store_config(&engine, LIFECYCLE_CONFIG_PATH, b"{");

  let engine = reopen(engine, &path);
  let report = engine.configuration_shadow();
  let resolution = report.resolution.as_ref().unwrap();
  assert!(matches!(resolution.runtime_status, ConfigDocumentStatus::Invalid { .. }));
  assert!(matches!(resolution.lifecycle_status, ConfigDocumentStatus::Invalid { .. }));
  assert!(!resolution.complete());

  assert!(load_lifecycle_config(&engine).snapshot_writes_enabled);
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&RequestContext::system(), "/still-writable.txt", b"ok", Some("text/plain")).unwrap();
  assert_eq!(ops.read_file_buffered("/still-writable.txt").unwrap(), b"ok");
}

#[test]
#[serial]
fn oversized_config_is_rejected_from_metadata_before_unbounded_collection() {
  let directory = tempfile::tempdir().unwrap();
  let path = database_path(&directory);
  let engine = create_engine(&directory);
  store_config(&engine, RUNTIME_CONFIG_PATH, &vec![b' '; MAX_CONFIG_DOCUMENT_BYTES + 1]);

  let engine = reopen(engine, &path);
  let resolution = engine.configuration_shadow().resolution.as_ref().unwrap().clone();
  let ConfigDocumentStatus::Invalid { ref message } = resolution.runtime_status else {
    panic!("oversized runtime document must be invalid");
  };
  assert!(message.contains("exceeds"), "{message}");
  assert!(!resolution.complete());
}

#[test]
#[serial]
fn dishonest_compressed_config_chunk_is_rejected_at_remaining_file_bound() {
  let directory = tempfile::tempdir().unwrap();
  let path = database_path(&directory);
  let engine = create_engine(&directory);
  let document = br#"{"schema_version":1}"#;
  store_config(&engine, RUNTIME_CONFIG_PATH, document);
  let record = DirectoryOps::new(&engine).get_metadata(RUNTIME_CONFIG_PATH).unwrap().unwrap();
  assert_eq!(record.chunk_hashes.len(), 1);

  let expansion = vec![b'x'; MAX_CONFIG_DOCUMENT_BYTES + 1];
  let compressed = compress(&expansion, CompressionAlgorithm::Zstd).unwrap();
  engine.store_entry_compressed(EntryType::Chunk, &record.chunk_hashes[0], &compressed, CompressionAlgorithm::Zstd).unwrap();

  let engine = reopen(engine, &path);
  let resolution = engine.configuration_shadow().resolution.as_ref().unwrap().clone();
  let ConfigDocumentStatus::Invalid { ref message } = resolution.runtime_status else {
    panic!("dishonest compressed runtime document must be invalid");
  };
  assert!(message.contains(&format!("exceeds caller bound {}", document.len())), "{message}");
  assert!(!resolution.complete());
}

#[test]
#[serial]
fn missing_config_chunk_is_visible_as_unreadable_without_blocking_legacy_writes() {
  let directory = tempfile::tempdir().unwrap();
  let path = database_path(&directory);
  let engine = create_engine(&directory);
  store_config(&engine, RUNTIME_CONFIG_PATH, br#"{"schema_version":1}"#);
  let record = DirectoryOps::new(&engine).get_metadata(RUNTIME_CONFIG_PATH).unwrap().unwrap();
  engine.mark_entry_deleted(&record.chunk_hashes[0]).unwrap();

  let engine = reopen(engine, &path);
  let resolution = engine.configuration_shadow().resolution.as_ref().unwrap().clone();
  let ConfigDocumentStatus::Invalid { ref message } = resolution.runtime_status else {
    panic!("runtime document with a missing chunk must be invalid");
  };
  assert!(message.contains("missing chunk"), "{message}");

  DirectoryOps::new(&engine)
    .store_file_buffered(&RequestContext::system(), "/legacy-owner-remains-active.txt", b"ok", Some("text/plain"))
    .unwrap();
}

#[test]
#[serial]
fn startup_shadow_is_immutable_after_runtime_file_changes() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let initial = engine.configuration_shadow();
  assert_eq!(initial.resolution.as_ref().unwrap().runtime_status, ConfigDocumentStatus::Missing);

  store_config(&engine, RUNTIME_CONFIG_PATH, br#"{"schema_version":1}"#);
  let after_write = engine.configuration_shadow();
  assert!(std::sync::Arc::ptr_eq(&initial, &after_write));
  assert_eq!(after_write.resolution.as_ref().unwrap().runtime_status, ConfigDocumentStatus::Missing);
}

#[test]
#[serial]
fn startup_shadow_collects_registered_environment_override() {
  let _environment = EnvironmentGuard::set("AEORDB_INDEX_FLUSH_AFTER_SECONDS", "45");
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let resolution = engine.configuration_shadow().resolution.as_ref().unwrap().clone();

  assert_eq!(resolution.property("index.flush_after_seconds").unwrap().value, Some(ConfigValue::Unsigned(45)));
  assert_eq!(resolution.property("index.flush_after_seconds").unwrap().source, Some(ConfigSource::Environment));
}

#[test]
#[serial]
fn startup_shadow_ignores_unregistered_environment_names() {
  let _environment = EnvironmentGuard::set("AEORDB_INDEX_FLUSH_AFTER_SECONDZ", "45");
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let resolution = engine.configuration_shadow().resolution.as_ref().unwrap().clone();

  assert_eq!(resolution.property("index.flush_after_seconds").unwrap().value, Some(ConfigValue::Unsigned(30)));
  assert_eq!(resolution.property("index.flush_after_seconds").unwrap().source, Some(ConfigSource::Default));
}
