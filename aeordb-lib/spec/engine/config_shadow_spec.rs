use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

use aeordb::engine::config_resolver::{
  CommandLineConfigOverrides, ConfigDocumentStatus, ConfigSource, ConfigValue, ConfigurationFamily, MAX_CONFIG_DOCUMENT_BYTES,
  RUNTIME_CONFIG_PATH, preopen_emergency_spill_locations,
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
  let query_runtime = engine.query_runtime_snapshot().unwrap();
  assert!(query_runtime.policy.is_none());
  assert!(query_runtime.disabled_reason.as_deref().is_some_and(|reason| reason.contains("unresolved")));
  let durability_grouping = engine.durability_group_policy().unwrap_err();
  assert!(durability_grouping.to_string().contains("group_commit_max_bytes is unresolved"), "{durability_grouping}");

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
fn startup_bound_memory_policy_activates_only_after_restart() {
  let directory = tempfile::tempdir().unwrap();
  let path = database_path(&directory);
  let engine = create_engine(&directory);
  let before = engine.memory_coordinator_snapshot().unwrap().policy.unwrap();
  let updated = engine
    .replace_configuration_document(
      ConfigurationFamily::Runtime,
      br#"{"schema_version":1,"memory":{"soft_limit_bytes":2147483648,"hard_limit_bytes":3221225472,"emergency_reserve_bytes":268435456}}"#,
    )
    .unwrap();

  assert_eq!(engine.memory_coordinator_snapshot().unwrap().policy.unwrap(), before);
  assert!(updated.pending_restart.contains("memory.soft_limit_bytes"));
  assert!(updated.pending_restart.contains("memory.hard_limit_bytes"));
  assert!(updated.pending_restart.contains("memory.emergency_reserve_bytes"));
  engine.shutdown().unwrap();
  drop(engine);

  let reopened = StorageEngine::open(&path).unwrap();
  let active = reopened.configuration_snapshot();
  let policy = reopened.memory_coordinator_snapshot().unwrap().policy.unwrap();
  assert_eq!(policy.soft_limit_bytes, 2_147_483_648);
  assert_eq!(policy.hard_limit_bytes, 3_221_225_472);
  assert_eq!(policy.emergency_reserve_bytes, 268_435_456);
  assert!(active.pending_restart.is_empty());
}

#[test]
#[serial]
fn preopen_scan_reads_stored_startup_spill_root_without_mutating_the_database() {
  let directory = tempfile::tempdir().unwrap();
  let path = database_path(&directory);
  let configured_spill = directory.path().join("configured-spill");
  let engine = create_engine(&directory);
  let previous_spill = engine.configuration_snapshot().resolved_path("recovery.emergency_spill_dir").unwrap().to_path_buf();
  let document = serde_json::to_vec(&serde_json::json!({
    "schema_version": 1,
    "recovery": {"emergency_spill_dir": configured_spill},
  }))
  .unwrap();
  let pending = engine.replace_configuration_document(ConfigurationFamily::Runtime, &document).unwrap();
  assert_eq!(pending.resolved_path("recovery.emergency_spill_dir"), Some(previous_spill.as_path()));
  assert!(pending.pending_restart.contains("recovery.emergency_spill_dir"));
  engine.shutdown().unwrap();
  drop(engine);

  let before = std::fs::read(&path).unwrap();
  let locations = preopen_emergency_spill_locations(&path, &CommandLineConfigOverrides::default()).unwrap();
  let after = std::fs::read(&path).unwrap();
  assert_eq!(after, before, "pre-open configuration bootstrap must remain read-only");
  assert!(locations.iter().any(|location| location.path == configured_spill));

  let reopened = StorageEngine::open(&path).unwrap();
  let active = reopened.configuration_snapshot();
  assert_eq!(active.resolved_path("recovery.emergency_spill_dir"), Some(configured_spill.as_path()));
  assert!(!active.pending_restart.contains("recovery.emergency_spill_dir"));
}

#[test]
#[serial]
fn preopen_scan_accepts_a_new_database_path_without_creating_it() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("new.aeordb");
  assert!(!path.exists());

  let locations = preopen_emergency_spill_locations(&path, &CommandLineConfigOverrides::default()).unwrap();

  assert!(!locations.is_empty());
  assert!(!path.exists(), "pre-open spill discovery must not create the database");
}

#[test]
#[serial]
fn dynamic_replacement_converges_memory_and_index_owners_before_reporting_active() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let before_memory = engine.memory_coordinator_snapshot().unwrap().policy.unwrap();
  let before_index = engine.index_buffer_stats().unwrap();
  let host_floor = if before_memory.host_available_floor_bytes == 512 * 1024 * 1024 { 768 * 1024 * 1024 } else { 512 * 1024 * 1024 };
  let clean_max = if before_index.max_bytes == 64 * 1024 * 1024 { 96 * 1024 * 1024 } else { 64 * 1024 * 1024 };
  let mutation_max = if before_index.mutation_max_bytes == 32 * 1024 * 1024 { 48 * 1024 * 1024 } else { 32 * 1024 * 1024 };
  let publication_max = if before_index.publication_batch_max_bytes == 16 * 1024 * 1024 { 24 * 1024 * 1024 } else { 16 * 1024 * 1024 };
  let document = format!(
    r#"{{"schema_version":1,"memory":{{"host_available_floor_bytes":{host_floor}}},"cache":{{"index_clean_max_bytes":{clean_max},"index_clean_ttl_seconds":17}},"index":{{"mutation_buffer_max_bytes":{mutation_max},"flush_after_mutations":1234,"flush_after_seconds":19,"publication_batch_max_bytes":{publication_max}}}}}"#
  );

  let updated = engine.replace_configuration_document(ConfigurationFamily::Runtime, document.as_bytes()).unwrap();
  let after_memory = engine.memory_coordinator_snapshot().unwrap().policy.unwrap();
  let after_index = engine.index_buffer_stats().unwrap();

  assert_eq!(updated.resolved_unsigned("memory.host_available_floor_bytes"), Some(host_floor));
  assert_eq!(after_memory.host_available_floor_bytes, host_floor);
  assert_eq!(updated.resolved_unsigned("cache.index_clean_max_bytes"), Some(clean_max));
  assert_eq!(after_index.max_bytes, clean_max);
  assert_eq!(updated.resolved_unsigned("cache.index_clean_ttl_seconds"), Some(17));
  assert_eq!(after_index.clean_ttl_ms, 17_000);
  assert_eq!(updated.resolved_unsigned("index.mutation_buffer_max_bytes"), Some(mutation_max));
  assert_eq!(after_index.mutation_max_bytes, mutation_max);
  assert_eq!(updated.resolved_unsigned("index.publication_batch_max_bytes"), Some(publication_max));
  assert_eq!(after_index.publication_batch_max_bytes, publication_max);
  assert_eq!(updated.resolved_unsigned("index.flush_after_mutations"), Some(1234));
  assert_eq!(after_index.flush_after_mutations, 1234);
  assert_eq!(updated.resolved_unsigned("index.flush_after_seconds"), Some(19));
  assert_eq!(after_index.flush_after_ms, 19_000);
  assert!(updated.pending_convergence.is_empty());
}

#[test]
#[serial]
fn dynamic_replacement_converges_directory_kv_and_durability_owners() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let document = br#"{"schema_version":1,"cache":{"directory_max_bytes":33554432,"kv_resident_max_bytes":67108864},"durability":{"group_commit_max_bytes":2097152,"group_commit_max_delay_ms":37}}"#;

  let updated = engine.replace_configuration_document(ConfigurationFamily::Runtime, document).unwrap();
  let memory = engine.memory_stats().unwrap();
  let kv = engine.kv_page_provider_stats().unwrap().unwrap();
  let durability = engine.durability_group_policy().unwrap();

  assert_eq!(updated.resolved_unsigned("cache.directory_max_bytes"), Some(33_554_432));
  assert_eq!(memory.directory_cache.max_bytes, Some(33_554_432));
  assert_eq!(updated.resolved_unsigned("cache.kv_resident_max_bytes"), Some(67_108_864));
  assert_eq!(kv.max_resident_bytes, 67_108_864);
  assert_eq!(updated.resolved_unsigned("durability.group_commit_max_bytes"), Some(2_097_152));
  assert_eq!(durability.max_bytes(), 2_097_152);
  assert_eq!(updated.resolved_unsigned("durability.group_commit_max_delay_ms"), Some(37));
  assert_eq!(durability.max_delay(), std::time::Duration::from_millis(37));
  assert!(updated.pending_convergence.is_empty());
}

#[test]
#[serial]
fn dynamic_replacement_converges_query_runtime_before_reporting_active() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let previous_plan_cache = engine.configuration_snapshot().resolved_unsigned("cache.query_plan_max_bytes").unwrap();
  let document = br#"{"schema_version":1,"cache":{"query_plan_max_bytes":8388608},"query":{"per_request_memory_bytes":16777216,"global_memory_bytes":67108864,"position_scan_buffer_bytes":2097152}}"#;

  let updated = engine.replace_configuration_document(ConfigurationFamily::Runtime, document).unwrap();
  let runtime = engine.query_runtime_snapshot().unwrap();
  let policy = runtime.policy.expect("valid query configuration activates the runtime owner");

  assert_eq!(updated.resolved_unsigned("cache.query_plan_max_bytes"), Some(previous_plan_cache));
  assert_eq!(
    updated.desired.resolution.as_ref().unwrap().properties["cache.query_plan_max_bytes"].value,
    Some(aeordb::engine::config_resolver::ConfigValue::Unsigned(8_388_608))
  );
  assert_eq!(updated.resolved_unsigned("query.per_request_memory_bytes"), Some(16_777_216));
  assert_eq!(policy.per_request_memory_bytes, 16_777_216);
  assert_eq!(updated.resolved_unsigned("query.global_memory_bytes"), Some(67_108_864));
  assert_eq!(policy.global_memory_bytes, 67_108_864);
  assert_eq!(updated.resolved_unsigned("query.position_scan_buffer_bytes"), Some(2_097_152));
  assert_eq!(policy.position_scan_buffer_bytes, 2_097_152);
  assert_eq!(updated.pending_convergence, std::collections::BTreeSet::from(["cache.query_plan_max_bytes".to_string()]));
  assert!(updated.convergence_errors["cache.query_plan_max_bytes"].contains("query-plan cache owner is not implemented"));
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
fn serving_engine_command_line_override_wins_environment_and_remains_ephemeral() {
  let _environment = EnvironmentGuard::set("AEORDB_MEMORY_HARD_LIMIT_BYTES", "3GiB");
  let directory = tempfile::tempdir().unwrap();
  let path = database_path(&directory);
  let overrides =
    CommandLineConfigOverrides::from_registered(BTreeMap::from([("--memory-hard-limit-bytes".to_string(), OsString::from("4GiB"))]))
      .unwrap();
  let engine = StorageEngine::create_with_hot_dir_and_configuration_overrides(&path, None, overrides).unwrap();
  DirectoryOps::new(&engine).ensure_root_directory(&RequestContext::system()).unwrap();
  let resolution = engine.configuration_shadow().resolution.as_ref().unwrap().clone();

  assert_eq!(resolution.property("memory.hard_limit_bytes").unwrap().value, Some(ConfigValue::Unsigned(4 * 1024 * 1024 * 1024)));
  assert_eq!(resolution.property("memory.hard_limit_bytes").unwrap().source, Some(ConfigSource::CommandLine));
  assert!(DirectoryOps::new(&engine).get_metadata(RUNTIME_CONFIG_PATH).unwrap().is_none());
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
