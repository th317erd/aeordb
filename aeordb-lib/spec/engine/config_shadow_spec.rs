use std::ffi::{OsStr, OsString};

use aeordb::engine::config_resolver::{
  ConfigDocumentStatus, ConfigSource, ConfigValue, ConfigurationFamily, MAX_CONFIG_DOCUMENT_BYTES, RUNTIME_CONFIG_PATH,
};
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
fn malformed_lifecycle_state_is_visible_and_fails_snapshot_writes_closed() {
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

  assert!(!load_lifecycle_config(&engine).snapshot_writes_enabled);
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
fn engine_configuration_authority_retains_startup_evidence_and_one_coherent_active_generation() {
  let directory = tempfile::tempdir().unwrap();
  let engine = std::sync::Arc::new(create_engine(&directory));
  let startup = engine.configuration_shadow();
  let current = engine.configuration_snapshot();

  assert_eq!(current.generation, 1);
  assert!(std::sync::Arc::ptr_eq(&startup, &current.startup));
  assert_eq!(current.active_properties.len(), 41);
  assert_eq!(current.resolved_unsigned("index.flush_after_seconds"), Some(30));
  assert!(current.pending_restart.is_empty());
  assert!(current.pending_convergence.is_empty());

  std::thread::scope(|scope| {
    let mut readers = Vec::new();
    for _ in 0..16 {
      let engine = std::sync::Arc::clone(&engine);
      readers.push(scope.spawn(move || engine.configuration_snapshot()));
    }
    for reader in readers {
      let observed = reader.join().unwrap();
      assert_eq!(observed.generation, 1);
      assert!(std::sync::Arc::ptr_eq(&current, &observed));
    }
  });
}

#[test]
#[serial]
fn strict_replacement_is_durable_before_activation_and_preserves_startup_evidence() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let startup = engine.configuration_shadow();
  let lifecycle = br#"{"schema_version":1,"snapshot_writes_enabled":false}"#;

  let updated = engine.replace_configuration_document(ConfigurationFamily::Lifecycle, lifecycle).unwrap();

  assert_eq!(updated.generation, 2);
  assert_eq!(updated.resolved_boolean("lifecycle.snapshot_writes_enabled"), Some(false));
  assert!(updated.pending_restart.is_empty());
  assert!(updated.pending_convergence.is_empty());
  assert_eq!(updated.active_properties["index.flush_after_seconds"].activated_generation, 1);
  assert_eq!(DirectoryOps::new(&engine).read_file_buffered(LIFECYCLE_CONFIG_PATH).unwrap(), lifecycle);
  assert!(std::sync::Arc::ptr_eq(&startup, &engine.configuration_shadow()));
  assert_eq!(startup.resolution.as_ref().unwrap().lifecycle_status, ConfigDocumentStatus::Missing);
}

#[test]
#[serial]
fn replacement_activates_dynamic_values_but_stages_startup_bound_values_for_restart() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let before = engine.configuration_snapshot();
  let original_hard_limit = before.resolved_unsigned("memory.hard_limit_bytes").unwrap();
  let runtime = br#"{"schema_version":1,"memory":{"hard_limit_bytes":4294967296},"index":{"flush_after_seconds":45}}"#;

  let updated = engine.replace_configuration_document(ConfigurationFamily::Runtime, runtime).unwrap();

  assert_eq!(updated.generation, 2);
  assert_eq!(updated.resolved_unsigned("index.flush_after_seconds"), Some(45));
  assert_eq!(updated.resolved_unsigned("memory.hard_limit_bytes"), Some(original_hard_limit));
  assert_eq!(
    updated.desired.resolution.as_ref().unwrap().property("memory.hard_limit_bytes").unwrap().value,
    Some(ConfigValue::Unsigned(4_294_967_296))
  );
  assert!(updated.pending_restart.contains("memory.hard_limit_bytes"));
  assert!(!updated.pending_restart.contains("index.flush_after_seconds"));
}

#[test]
#[serial]
fn rejected_replacement_leaves_stored_bytes_generation_and_active_values_unchanged() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let valid = br#"{"schema_version":1,"snapshot_writes_enabled":false}"#;
  let current = engine.replace_configuration_document(ConfigurationFamily::Lifecycle, valid).unwrap();

  let error = engine
    .replace_configuration_document(
      ConfigurationFamily::Lifecycle,
      br#"{"schema_version":1,"snapshot_writes_enabled":true,"snapshot_writes_enabled":false}"#,
    )
    .unwrap_err();

  assert!(error.to_string().contains("duplicate"), "{error}");
  let after = engine.configuration_snapshot();
  assert_eq!(after.generation, current.generation);
  assert_eq!(after.resolved_boolean("lifecycle.snapshot_writes_enabled"), Some(false));
  assert_eq!(DirectoryOps::new(&engine).read_file_buffered(LIFECYCLE_CONFIG_PATH).unwrap(), valid);
}

#[test]
#[serial]
fn failed_durable_publication_leaves_generation_active_policy_and_stored_bytes_unchanged() {
  let directory = tempfile::tempdir().unwrap();
  let path = database_path(&directory);
  let engine = create_engine(&directory);
  let valid = br#"{"schema_version":1,"snapshot_writes_enabled":false}"#;
  let current = engine.replace_configuration_document(ConfigurationFamily::Lifecycle, valid).unwrap();
  engine.shutdown().unwrap();

  let error = engine
    .replace_configuration_document(ConfigurationFamily::Lifecycle, br#"{"schema_version":1,"snapshot_writes_enabled":true}"#)
    .unwrap_err();

  assert!(error.to_string().contains("shutting down"), "{error}");
  let after = engine.configuration_snapshot();
  assert_eq!(after.generation, current.generation);
  assert_eq!(after.resolved_boolean("lifecycle.snapshot_writes_enabled"), Some(false));
  drop(engine);

  let reopened = StorageEngine::open(&path).unwrap();
  assert_eq!(DirectoryOps::new(&reopened).read_file_buffered(LIFECYCLE_CONFIG_PATH).unwrap(), valid);
  assert_eq!(reopened.configuration_snapshot().resolved_boolean("lifecycle.snapshot_writes_enabled"), Some(false));
}

#[test]
#[serial]
fn concurrent_replacements_serialize_file_bytes_and_authority_generations() {
  let directory = tempfile::tempdir().unwrap();
  let engine = std::sync::Arc::new(create_engine(&directory));
  let documents: [&[u8]; 2] =
    [br#"{"schema_version":1,"index":{"flush_after_seconds":45}}"#, br#"{"schema_version":1,"index":{"flush_after_seconds":60}}"#];

  let returned = std::thread::scope(|scope| {
    let handles = documents.map(|document| {
      let engine = std::sync::Arc::clone(&engine);
      scope.spawn(move || engine.replace_configuration_document(ConfigurationFamily::Runtime, document).unwrap())
    });
    handles.map(|handle| handle.join().unwrap())
  });

  let final_snapshot = engine.configuration_snapshot();
  assert_eq!(final_snapshot.generation, 3);
  assert_eq!(returned.iter().map(|snapshot| snapshot.generation).collect::<std::collections::BTreeSet<_>>(), [2, 3].into());
  let final_seconds = final_snapshot.resolved_unsigned("index.flush_after_seconds").unwrap();
  let final_bytes = DirectoryOps::new(&engine).read_file_buffered(RUNTIME_CONFIG_PATH).unwrap();
  let expected = if final_seconds == 45 { documents[0] } else { documents[1] };
  assert_eq!(final_bytes, expected);
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
fn replacement_never_persists_effective_environment_overrides() {
  let _environment = EnvironmentGuard::set("AEORDB_INDEX_FLUSH_AFTER_SECONDS", "45");
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let document = br#"{"schema_version":1,"index":{"flush_after_seconds":60}}"#;

  let snapshot = engine.replace_configuration_document(ConfigurationFamily::Runtime, document).unwrap();

  let active = snapshot.active_properties.get("index.flush_after_seconds").unwrap();
  assert_eq!(active.value, Some(ConfigValue::Unsigned(45)));
  assert_eq!(active.source, Some(ConfigSource::Environment));
  let desired = snapshot.desired.resolution.as_ref().unwrap().property("index.flush_after_seconds").unwrap();
  assert_eq!(desired.value, Some(ConfigValue::Unsigned(45)));
  assert_eq!(desired.source, Some(ConfigSource::Environment));
  assert_eq!(DirectoryOps::new(&engine).read_file_buffered(RUNTIME_CONFIG_PATH).unwrap(), document);
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
