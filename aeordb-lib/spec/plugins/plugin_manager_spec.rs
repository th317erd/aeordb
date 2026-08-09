use std::sync::{Arc, Barrier};

use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine};
use aeordb::plugins::plugin_manager::{PLUGIN_BINARY_MAX_BYTES, PluginManager, PluginManagerError};
use aeordb::plugins::types::PluginType;
use aeordb::server::create_temp_engine_for_tests;
use aeordb::engine::memory_coordinator::{AdmissionClass, HostMemorySample, MemoryOwner};

fn raw_plugin_path(key: &str) -> String {
  format!("/.aeordb-system/plugins/{}", key.replace('/', "::"))
}

fn read_raw_plugin(engine: &StorageEngine, key: &str) -> Vec<u8> {
  DirectoryOps::new(engine).read_file_buffered(&raw_plugin_path(key)).expect("stored plugin record")
}

fn store_raw_plugin(engine: &StorageEngine, key: &str, encoded: &[u8]) {
  DirectoryOps::new(engine)
    .store_file_buffered(&RequestContext::system(), &raw_plugin_path(key), encoded, Some("application/octet-stream"))
    .expect("store raw plugin fixture");
}

/// Compile a minimal valid WASM module for testing.
fn minimal_wasm_bytes() -> Vec<u8> {
  let wat = r#"
  (module
    (memory (export "memory") 1)
    (func (export "handle") (param $request_ptr i32) (param $request_len i32) (result i64)
      (i64.or
        (i64.shl
          (i64.extend_i32_u (local.get $request_ptr))
          (i64.const 32)
        )
        (i64.extend_i32_u (local.get $request_len))
      )
    )
  )
  "#;
  wat::parse_str(wat).expect("WAT should be valid")
}

fn trapping_wasm_bytes() -> Vec<u8> {
  wat::parse_str(
    r#"
    (module
      (memory (export "memory") 1)
      (func (export "handle") (param i32) (param i32) (result i64)
        (unreachable)
      )
    )
    "#,
  )
  .expect("WAT should be valid")
}

fn fuel_exhausting_wasm_bytes() -> Vec<u8> {
  wat::parse_str(
    r#"
    (module
      (memory (export "memory") 1)
      (func (export "handle") (param i32) (param i32) (result i64)
        (loop $forever
          (br $forever)
        )
        (i64.const 0)
      )
    )
    "#,
  )
  .expect("WAT should be valid")
}

fn constant_wasm_bytes(value: u8) -> Vec<u8> {
  wat::parse_str(format!(
    r#"
    (module
      (memory (export "memory") 1)
      (data (i32.const 4096) "\{value:02x}")
      (func (export "handle") (param i32) (param i32) (result i64)
        (i64.or
          (i64.shl (i64.const 4096) (i64.const 32))
          (i64.const 1)
        )
      )
    )
    "#,
  ))
  .expect("WAT should be valid")
}

#[test]
fn plugin_manager_construction_does_not_resample_or_mutate_host_memory() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let sentinel = HostMemorySample {
    rss_bytes: 17,
    private_bytes: Some(19),
    mapped_bytes: Some(23),
    allocator_bytes: Some(29),
    host_available_bytes: Some(u64::MAX),
  };
  engine.memory_coordinator().update_host_sample(sentinel).unwrap();

  let _manager = PluginManager::new(engine.clone());

  assert_eq!(engine.memory_coordinator().snapshot().unwrap().host, sentinel);
}

/// Create a fresh PluginManager backed by a temp engine.
fn test_manager() -> (PluginManager, tempfile::TempDir) {
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let manager = PluginManager::new(engine);
  (manager, temp_dir)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_deploy_plugin_stores_in_database() {
  let (manager, _temp_dir) = test_manager();
  let wasm_bytes = minimal_wasm_bytes();

  let record = manager.deploy_plugin("my_plugin", "db/schema/table", PluginType::Wasm, wasm_bytes.clone()).expect("deploy should succeed");

  assert_eq!(record.name, "my_plugin");
  assert_eq!(record.path, "db/schema/table");
  assert_eq!(record.plugin_type, PluginType::Wasm);
  assert!(!record.wasm_bytes.is_empty());
  assert!(record.version.is_none());
  assert!(record.author.is_none());
  assert!(record.checksum.starts_with("blake3:"));
  assert_eq!(record.checksum.len(), "blake3:".len() + 64);
  assert!(record.updated_at >= record.created_at);
}

#[test]
fn test_deploy_plugin_with_metadata_stores_rich_metadata() {
  let (manager, _temp_dir) = test_manager();
  let wasm_bytes = minimal_wasm_bytes();

  let record = manager
    .deploy_plugin_with_metadata(
      "my_plugin",
      "db/schema/table",
      PluginType::Wasm,
      wasm_bytes,
      Some("1.2.3".to_string()),
      Some("Test Author".to_string()),
    )
    .expect("deploy should succeed");

  assert_eq!(record.version.as_deref(), Some("1.2.3"));
  assert_eq!(record.author.as_deref(), Some("Test Author"));
  assert!(record.checksum.starts_with("blake3:"));

  let metadata = record.to_metadata();
  assert_eq!(metadata.version.as_deref(), Some("1.2.3"));
  assert_eq!(metadata.author.as_deref(), Some("Test Author"));
  assert_eq!(metadata.checksum, record.checksum);
  assert_eq!(metadata.updated_at, record.updated_at);
}

#[test]
fn test_get_deployed_plugin() {
  let (manager, _temp_dir) = test_manager();
  let wasm_bytes = minimal_wasm_bytes();

  manager.deploy_plugin("my_plugin", "db/schema/table", PluginType::Wasm, wasm_bytes.clone()).expect("deploy should succeed");

  let retrieved = manager.get_plugin("db/schema/table").expect("get should not error").expect("plugin should exist");

  assert_eq!(retrieved.name, "my_plugin");
  assert_eq!(retrieved.path, "db/schema/table");
  assert_eq!(retrieved.wasm_bytes, wasm_bytes);
  assert!(retrieved.checksum.starts_with("blake3:"));
}

#[test]
fn test_list_deployed_plugins() {
  let (manager, _temp_dir) = test_manager();
  let wasm_bytes = minimal_wasm_bytes();

  manager.deploy_plugin("plugin_a", "db/schema/alpha", PluginType::Wasm, wasm_bytes.clone()).expect("deploy alpha");
  manager.deploy_plugin("plugin_b", "db/schema/beta", PluginType::Wasm, wasm_bytes.clone()).expect("deploy beta");

  let plugins = manager.list_plugins().expect("list should succeed");
  assert_eq!(plugins.len(), 2);

  let names: Vec<&str> = plugins.iter().map(|p| p.name.as_str()).collect();
  assert!(names.contains(&"plugin_a"));
  assert!(names.contains(&"plugin_b"));
  assert!(plugins.iter().all(|p| p.checksum.starts_with("blake3:")));
}

#[test]
fn test_remove_deployed_plugin() {
  let _ctx = RequestContext::system();
  let (manager, _temp_dir) = test_manager();
  let wasm_bytes = minimal_wasm_bytes();

  manager.deploy_plugin("doomed", "db/schema/doomed", PluginType::Wasm, wasm_bytes).expect("deploy");

  let removed = manager.remove_plugin("db/schema/doomed").expect("remove should not error");
  assert!(removed, "should return true when plugin existed");

  let after = manager.get_plugin("db/schema/doomed").expect("get should not error");
  assert!(after.is_none(), "plugin should no longer exist");
}

#[test]
fn test_deploy_duplicate_path_overwrites() {
  let (manager, _temp_dir) = test_manager();
  let wasm_bytes = minimal_wasm_bytes();

  let first = manager.deploy_plugin("v1", "db/schema/func", PluginType::Wasm, wasm_bytes.clone()).expect("first deploy");

  let second = manager.deploy_plugin("v2", "db/schema/func", PluginType::Wasm, wasm_bytes.clone()).expect("second deploy");

  // Should reuse the same plugin_id.
  assert_eq!(first.plugin_id, second.plugin_id);
  // But the name should be updated.
  assert_eq!(second.name, "v2");
  assert!(second.updated_at >= first.updated_at);

  // Only one plugin should exist.
  let plugins = manager.list_plugins().expect("list");
  assert_eq!(plugins.len(), 1);
  assert_eq!(plugins[0].name, "v2");
}

#[test]
fn test_get_nonexistent_plugin_returns_none() {
  let (manager, _temp_dir) = test_manager();

  let result = manager.get_plugin("nonexistent/path").expect("get should not error");
  assert!(result.is_none());
}

#[test]
fn test_remove_nonexistent_plugin_returns_false() {
  let _ctx = RequestContext::system();
  let (manager, _temp_dir) = test_manager();

  let removed = manager.remove_plugin("nonexistent/path").expect("remove should not error");
  assert!(!removed, "should return false when plugin did not exist");
}

#[test]
fn test_list_empty_returns_empty_vec() {
  let (manager, _temp_dir) = test_manager();

  let plugins = manager.list_plugins().expect("list should succeed");
  assert!(plugins.is_empty());
}

#[test]
fn test_deploy_invalid_wasm_rejected() {
  let (manager, _temp_dir) = test_manager();
  let garbage = vec![0x00, 0x61, 0x73, 0x6d, 0xFF, 0xFF, 0xFF, 0xFF];

  let result = manager.deploy_plugin("bad", "db/schema/bad", PluginType::Wasm, garbage);
  assert!(result.is_err(), "should reject invalid WASM");
  match result.unwrap_err() {
    PluginManagerError::InvalidPlugin(_) => {}
    other => panic!("expected InvalidPlugin, got: {:?}", other),
  }
}

#[test]
fn test_invoke_wasm_plugin() {
  let (manager, _temp_dir) = test_manager();
  let wasm_bytes = minimal_wasm_bytes();

  manager.deploy_plugin("echo", "db/schema/echo", PluginType::Wasm, wasm_bytes).expect("deploy");

  let response = manager.invoke_wasm_plugin("db/schema/echo", b"hello").expect("invoke should succeed");

  assert_eq!(response, b"hello");
}

#[test]
fn plugin_invocation_refuses_before_guest_memory_growth_and_retries_cleanly() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let manager = PluginManager::new(engine.clone());
  manager.deploy_plugin("echo", "memory/echo", PluginType::Wasm, minimal_wasm_bytes()).expect("deploy");

  let coordinator = engine.memory_coordinator();
  let before = engine.memory_coordinator_snapshot().unwrap();
  let available = before.policy.unwrap().ordinary_limit_bytes().saturating_sub(before.accounted_bytes);
  let remaining = 1024 * 1024;
  assert!(available > remaining);
  let pressure = coordinator.reserve(MemoryOwner::Task, available - remaining, AdmissionClass::Workload).unwrap();

  let error = manager.invoke_wasm_plugin("memory/echo", b"hello").unwrap_err();
  assert!(error.to_string().contains("resource exhausted"), "unexpected refusal: {error}");
  let refused = engine.memory_coordinator_snapshot().unwrap();
  assert_eq!(refused.owner(MemoryOwner::ParserPlugin).unwrap().reserved_bytes, 0);
  assert_eq!(refused.owner(MemoryOwner::ParserPlugin).unwrap().active_reservations, 0);
  drop(pressure);

  assert_eq!(manager.invoke_wasm_plugin("memory/echo", b"hello").unwrap(), b"hello");
}

#[test]
fn compiled_plugin_cache_is_accounted_and_invalidation_releases_it() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let manager = PluginManager::new(engine.clone());
  manager.deploy_plugin("echo", "cache/echo", PluginType::Wasm, minimal_wasm_bytes()).expect("deploy");

  assert_eq!(manager.invoke_wasm_plugin("cache/echo", b"hello").unwrap(), b"hello");
  let cached = engine.memory_coordinator_snapshot().unwrap();
  assert!(cached.owner(MemoryOwner::ParserPlugin).unwrap().reserved_bytes > 0, "compiled runtime cache is not accounted");

  assert!(manager.remove_plugin("cache/echo").unwrap());
  let invalidated = engine.memory_coordinator_snapshot().unwrap();
  assert_eq!(invalidated.owner(MemoryOwner::ParserPlugin).unwrap().reserved_bytes, 0);
  assert_eq!(invalidated.owner(MemoryOwner::ParserPlugin).unwrap().active_reservations, 0);
}

#[test]
fn compiled_plugin_cache_identity_tracks_acknowledged_bytes_across_managers() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let manager_a = PluginManager::new(engine.clone());
  let manager_b = PluginManager::new(engine.clone());
  manager_a.deploy_plugin("old", "cache/shared", PluginType::Wasm, constant_wasm_bytes(b'A')).unwrap();

  assert_eq!(manager_b.invoke_wasm_plugin("cache/shared", b"request").unwrap(), b"A");
  manager_a.deploy_plugin("new", "cache/shared", PluginType::Wasm, constant_wasm_bytes(b'B')).unwrap();

  assert_eq!(
    manager_b.invoke_wasm_plugin("cache/shared", b"request").unwrap(),
    b"B",
    "an acknowledged replacement must not reuse another manager's path-keyed stale runtime",
  );
  assert!(manager_b.remove_plugin("cache/shared").unwrap());
  let invalidated = engine.memory_coordinator_snapshot().unwrap();
  assert_eq!(invalidated.owner(MemoryOwner::ParserPlugin).unwrap().reserved_bytes, 0);
}

#[test]
fn concurrent_first_deploys_reuse_one_persistent_plugin_id() {
  const WRITERS: usize = 16;
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let barrier = Arc::new(Barrier::new(WRITERS));
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let mut handles = Vec::with_capacity(WRITERS);

  for writer in 0..WRITERS {
    let engine = engine.clone();
    let barrier = barrier.clone();
    handles.push(std::thread::spawn(move || {
      let manager = PluginManager::new(engine);
      barrier.wait();
      manager.deploy_plugin(&format!("writer-{writer}"), "race/identity", PluginType::Wasm, minimal_wasm_bytes()).unwrap()
    }));
  }

  let records: Vec<_> = handles.into_iter().map(|handle| handle.join().unwrap()).collect();
  let expected_id = &records[0].plugin_id;
  assert!(records.iter().all(|record| &record.plugin_id == expected_id), "every acknowledged replacement must retain one stable ID");
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before + WRITERS as u64);
}

#[test]
fn stored_plugin_checksum_mismatch_fails_closed() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let manager = PluginManager::new(engine.clone());
  manager.deploy_plugin("checksum", "strict/checksum", PluginType::Wasm, minimal_wasm_bytes()).unwrap();
  let encoded = read_raw_plugin(&engine, "strict/checksum");
  let mut record: aeordb::plugins::PluginRecord = serde_json::from_slice(&encoded).unwrap();
  record.checksum = format!("blake3:{}", "0".repeat(64));
  store_raw_plugin(&engine, "strict/checksum", &serde_json::to_vec(&record).unwrap());

  let error = manager.get_plugin("strict/checksum").unwrap_err();
  assert!(error.to_string().contains("checksum"), "unexpected checksum-corruption error: {error}");
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let error = manager.deploy_plugin("replacement", "strict/checksum", PluginType::Wasm, minimal_wasm_bytes()).unwrap_err();
  assert!(error.to_string().contains("checksum"), "unexpected replacement error: {error}");
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
}

#[test]
fn ambiguous_plugin_key_is_rejected_before_publication() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let manager = PluginManager::new(engine.clone());
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;

  let error = manager.deploy_plugin("ambiguous", "aliases::plugin", PluginType::Wasm, minimal_wasm_bytes()).unwrap_err();

  assert!(matches!(error, PluginManagerError::InvalidPlugin(_)), "unexpected ambiguous-key error: {error}");
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert!(manager.get_plugin("aliases/plugin").unwrap().is_none());
}

#[test]
fn legacy_empty_checksum_is_derived_without_rewriting_the_record() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let manager = PluginManager::new(engine.clone());
  manager.deploy_plugin("legacy", "strict/legacy", PluginType::Wasm, minimal_wasm_bytes()).unwrap();
  let encoded = read_raw_plugin(&engine, "strict/legacy");
  let mut record: aeordb::plugins::PluginRecord = serde_json::from_slice(&encoded).unwrap();
  record.checksum.clear();
  let legacy_encoded = serde_json::to_vec(&record).unwrap();
  store_raw_plugin(&engine, "strict/legacy", &legacy_encoded);
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;

  let loaded = manager.get_plugin("strict/legacy").unwrap().unwrap();

  assert_eq!(loaded.checksum, format!("blake3:{}", blake3::hash(&loaded.wasm_bytes).to_hex()));
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert_eq!(read_raw_plugin(&engine, "strict/legacy"), legacy_encoded);
}

#[test]
fn oversized_plugin_and_metadata_are_rejected_before_publication() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let manager = PluginManager::new(engine.clone());
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;

  let oversized = manager.deploy_plugin("oversized", "limits/body", PluginType::Native, vec![0x5a; PLUGIN_BINARY_MAX_BYTES + 1]);
  assert!(matches!(oversized, Err(PluginManagerError::ResourceExhausted(_))));
  let metadata = manager.deploy_plugin(&"n".repeat(4097), "limits/name", PluginType::Wasm, minimal_wasm_bytes());
  assert!(matches!(metadata, Err(PluginManagerError::InvalidPlugin(_))));
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert!(manager.get_plugin("limits/body").unwrap().is_none());
  assert!(manager.get_plugin("limits/name").unwrap().is_none());
}

#[test]
fn native_deployment_refuses_pressure_before_publication_and_retries_cleanly() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let manager = PluginManager::new(engine.clone());
  let snapshot = engine.memory_coordinator_snapshot().unwrap();
  let available = snapshot.policy.unwrap().ordinary_limit_bytes().saturating_sub(snapshot.accounted_bytes);
  let remaining = 512 * 1024;
  assert!(available > remaining);
  let pressure = engine.memory_coordinator().reserve(MemoryOwner::Task, available - remaining, AdmissionClass::Workload).unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let native_bytes = vec![0x31; 256 * 1024];

  let error = manager.deploy_plugin("native", "pressure/native", PluginType::Native, native_bytes.clone()).unwrap_err();

  assert!(matches!(error, PluginManagerError::ResourceExhausted(_)), "unexpected pressure error: {error}");
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert!(manager.get_plugin("pressure/native").unwrap().is_none());
  drop(pressure);
  assert_eq!(manager.deploy_plugin("native", "pressure/native", PluginType::Native, native_bytes).unwrap().name, "native");
}

#[test]
fn concurrent_remove_and_deploy_are_linearizable_with_a_cached_observer() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let setup_manager = PluginManager::new(engine.clone());
  let observer = PluginManager::new(engine.clone());
  setup_manager.deploy_plugin("old", "race/remove", PluginType::Wasm, constant_wasm_bytes(b'A')).unwrap();
  assert_eq!(observer.invoke_wasm_plugin("race/remove", b"request").unwrap(), b"A");
  let barrier = Arc::new(Barrier::new(2));
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;

  let remove_handle = {
    let engine = engine.clone();
    let barrier = barrier.clone();
    std::thread::spawn(move || {
      let manager = PluginManager::new(engine);
      barrier.wait();
      manager.remove_plugin("race/remove").unwrap()
    })
  };
  let deploy_handle = {
    let engine = engine.clone();
    std::thread::spawn(move || {
      let manager = PluginManager::new(engine);
      barrier.wait();
      manager.deploy_plugin("new", "race/remove", PluginType::Wasm, constant_wasm_bytes(b'B')).unwrap()
    })
  };

  assert!(remove_handle.join().unwrap());
  let _deployed = deploy_handle.join().unwrap();
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before + 2);
  match observer.get_plugin("race/remove").unwrap() {
    Some(record) => {
      assert_eq!(record.name, "new");
      assert_eq!(observer.invoke_wasm_plugin("race/remove", b"request").unwrap(), b"B");
    }
    None => assert!(matches!(observer.invoke_wasm_plugin("race/remove", b"request"), Err(PluginManagerError::NotFound(_)))),
  }
}

#[test]
fn trap_and_fuel_exhaustion_release_every_invocation_reservation() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let manager = PluginManager::new(engine.clone());
  manager.deploy_plugin("trap", "failure/trap", PluginType::Wasm, trapping_wasm_bytes()).expect("deploy trap");
  manager.deploy_plugin("fuel", "failure/fuel", PluginType::Wasm, fuel_exhausting_wasm_bytes()).expect("deploy fuel exhaustion");

  for path in ["failure/trap", "failure/fuel"] {
    let error = manager.invoke_wasm_plugin_with_limits(path, b"request", 64 * 1024).unwrap_err();
    assert!(matches!(error, PluginManagerError::ExecutionFailed(_)), "unexpected {path} error: {error}");
    let snapshot = engine.memory_coordinator_snapshot().unwrap();
    let parser = snapshot.owner(MemoryOwner::ParserPlugin).unwrap();
    assert_eq!(parser.reserved_bytes, 0, "{path} leaked reserved bytes");
    assert_eq!(parser.active_reservations, 0, "{path} leaked an active reservation");
  }
}

#[test]
fn test_invoke_nonexistent_plugin_returns_not_found() {
  let (manager, _temp_dir) = test_manager();

  let result = manager.invoke_wasm_plugin("missing/path", b"data");
  assert!(result.is_err());
  match result.unwrap_err() {
    PluginManagerError::NotFound(_) => {}
    other => panic!("expected NotFound, got: {:?}", other),
  }
}

#[test]
fn test_deploy_native_plugin_skips_wasm_validation() {
  let (manager, _temp_dir) = test_manager();
  // For a native plugin, the bytes are just stored as-is (no WASM validation).
  let dummy_bytes = b"not real wasm but that is fine for native".to_vec();

  let record = manager
    .deploy_plugin("native_func", "db/schema/native", PluginType::Native, dummy_bytes.clone())
    .expect("deploy native should succeed");

  assert_eq!(record.plugin_type, PluginType::Native);
  assert_eq!(record.wasm_bytes, dummy_bytes);
}
