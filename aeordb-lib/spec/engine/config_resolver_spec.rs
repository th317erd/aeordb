use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use aeordb::engine::config_resolver::{
  ConfigDocumentInput, ConfigFallback, ConfigResolutionContext, ConfigResolutionInputs, ConfigResolver, ConfigSource, ConfigValue,
};
use aeordb::engine::v4::contract_generated::CONFIGURATION_PROPERTIES;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

#[cfg(unix)]
fn native_paths() -> (PathBuf, PathBuf, PathBuf) {
  ("/srv/aeordb/data.aeordb".into(), "/srv/aeordb/.data.aeordb-gc".into(), "/var/lib/aeordb/spill".into())
}

#[cfg(windows)]
fn native_paths() -> (PathBuf, PathBuf, PathBuf) {
  (r"C:\srv\aeordb\data.aeordb".into(), r"C:\srv\aeordb\.data.aeordb-gc".into(), r"C:\var\lib\aeordb\spill".into())
}

#[cfg(unix)]
fn alternate_spill_paths() -> (&'static OsStr, PathBuf) {
  (OsStr::new("/var//tmp/aeordb-spill/"), "/var/tmp/aeordb-spill".into())
}

#[cfg(windows)]
fn alternate_spill_paths() -> (&'static OsStr, PathBuf) {
  (OsStr::new(r"C:\var\\tmp\aeordb-spill\"), r"C:\var\tmp\aeordb-spill".into())
}

fn context() -> ConfigResolutionContext {
  let (database_path, default_gc_workspace_root, default_emergency_spill_dir) = native_paths();
  ConfigResolutionContext {
    physical_memory_bytes: 16 * GIB,
    logical_cpu_count: 8,
    filesystem_capacity_bytes: 2 * 1024 * GIB,
    chunk_size_bytes: 256 * 1024,
    database_path,
    default_gc_workspace_root: Some(default_gc_workspace_root),
    default_emergency_spill_dir: Some(default_emergency_spill_dir),
  }
}

fn resolve(inputs: ConfigResolutionInputs) -> aeordb::engine::config_resolver::ConfigResolution {
  ConfigResolver::new(context()).resolve(inputs)
}

fn runtime(bytes: &str) -> ConfigDocumentInput {
  ConfigDocumentInput::Bytes(bytes.as_bytes().to_vec())
}

fn lifecycle(bytes: &str) -> ConfigDocumentInput {
  ConfigDocumentInput::Bytes(bytes.as_bytes().to_vec())
}

fn u64_value(resolution: &aeordb::engine::config_resolver::ConfigResolution, path: &str) -> u64 {
  match resolution.property(path).unwrap().value.as_ref().unwrap() {
    ConfigValue::Unsigned(value) => *value,
    value => panic!("expected unsigned value for {path}, got {value:?}"),
  }
}

#[test]
fn frozen_registry_is_complete_and_machine_names_remain_mechanical() {
  assert_eq!(CONFIGURATION_PROPERTIES.len(), 41);
  for (index, property) in CONFIGURATION_PROPERTIES.iter().enumerate() {
    assert_eq!(property.id as usize, index + 1);
    assert!(!property.default.is_empty(), "{} has no default", property.path);
    assert!(!property.constraint.is_empty(), "{} has no constraint", property.path);
    assert!(!property.owner.is_empty(), "{} has no owner", property.path);
    assert_eq!(property.environment, format!("AEORDB_{}", property.path.replace('.', "_").to_ascii_uppercase()));
    assert_eq!(property.cli, format!("--{}", property.path.replace(['.', '_'], "-")));
  }
}

#[test]
fn missing_documents_resolve_all_defaults_for_the_reference_host() {
  let (_, default_gc_workspace_root, default_emergency_spill_dir) = native_paths();
  let resolution = resolve(ConfigResolutionInputs::default());
  assert!(resolution.complete(), "{:?}", resolution.issues);
  assert!(!resolution.degraded());
  assert_eq!(resolution.properties.len(), 41);
  let expected = [
    ("memory.soft_limit_bytes", ConfigValue::Unsigned(6 * GIB)),
    ("memory.hard_limit_bytes", ConfigValue::Unsigned(8 * GIB)),
    ("memory.host_available_floor_bytes", ConfigValue::Unsigned(2 * GIB)),
    ("memory.emergency_reserve_bytes", ConfigValue::Unsigned(256 * MIB)),
    ("cache.index_clean_max_bytes", ConfigValue::Unsigned(2 * GIB)),
    ("cache.index_clean_ttl_seconds", ConfigValue::Unsigned(300)),
    ("cache.directory_max_bytes", ConfigValue::Unsigned(512 * MIB)),
    ("cache.kv_resident_max_bytes", ConfigValue::Unsigned(2 * GIB)),
    ("cache.query_plan_max_bytes", ConfigValue::Unsigned(256 * MIB)),
    ("index.mutation_buffer_max_bytes", ConfigValue::Unsigned(GIB)),
    ("index.flush_after_mutations", ConfigValue::Unsigned(262_144)),
    ("index.flush_after_seconds", ConfigValue::Unsigned(30)),
    ("index.publication_batch_max_bytes", ConfigValue::Unsigned(256 * MIB)),
    ("garbage_collection.mark_memory_preferred_bytes", ConfigValue::Unsigned(128 * MIB)),
    ("garbage_collection.mark_memory_minimum_bytes", ConfigValue::Unsigned(64 * MIB)),
    ("garbage_collection.mark_scratch_free_reserve_bytes", ConfigValue::Unsigned(2 * 1024 * GIB / 50)),
    ("garbage_collection.mark_scratch_max_bytes", ConfigValue::OptionalBytes(None)),
    ("garbage_collection.checkpoint_after_seconds", ConfigValue::Unsigned(300)),
    ("garbage_collection.checkpoint_after_dirty_bytes", ConfigValue::Unsigned(GIB)),
    ("garbage_collection.mark_workspace_root", ConfigValue::Path(default_gc_workspace_root)),
    ("io.read_prefetch_bytes", ConfigValue::Unsigned(2_621_440)),
    ("io.read_coalesce_max_bytes", ConfigValue::Unsigned(16 * MIB)),
    ("query.per_request_memory_bytes", ConfigValue::Unsigned(128 * MIB)),
    ("query.global_memory_bytes", ConfigValue::Unsigned(GIB)),
    ("query.position_scan_buffer_bytes", ConfigValue::Unsigned(8 * MIB)),
    ("durability.group_commit_max_bytes", ConfigValue::Unsigned(64 * MIB)),
    ("durability.group_commit_max_delay_ms", ConfigValue::Unsigned(100)),
    ("maintenance.max_concurrent_tasks", ConfigValue::Unsigned(2)),
    ("recovery.emergency_spill_dir", ConfigValue::Path(default_emergency_spill_dir)),
    ("recovery.emergency_spill_max_bytes", ConfigValue::Unsigned(4 * GIB)),
    ("shutdown.operation_wait_seconds", ConfigValue::Unsigned(600)),
    ("migration.capture_max_bytes", ConfigValue::Unsigned(64 * GIB)),
    ("migration.capture_free_reserve_bytes", ConfigValue::Unsigned(2 * 1024 * GIB / 20)),
    ("migration.checkpoint_after_seconds", ConfigValue::Unsigned(300)),
    ("garbage_collection.root_expiry_retention_seconds", ConfigValue::Unsigned(2_592_000)),
    ("garbage_collection.root_expiry_max_bytes", ConfigValue::Unsigned(256 * MIB)),
    ("garbage_collection.root_lifecycle_hard_max_bytes", ConfigValue::Unsigned(GIB)),
    ("lifecycle.snapshot_writes_enabled", ConfigValue::Boolean(true)),
    ("lifecycle.snapshot_retention_auto_months", ConfigValue::Unsigned(0)),
    ("lifecycle.snapshot_retention_manual_months", ConfigValue::Unsigned(0)),
    ("lifecycle.garbage_collection_pending_delete_grace_seconds", ConfigValue::Unsigned(86_400)),
  ];
  assert_eq!(expected.len(), CONFIGURATION_PROPERTIES.len());
  for (path, expected_value) in expected {
    let property = resolution.property(path).unwrap();
    assert_eq!(property.value, Some(expected_value), "{path}");
    assert_eq!(property.source, Some(ConfigSource::Default), "{path}");
  }
}

#[test]
fn runtime_v1_is_duplicate_aware_and_strict_at_every_object_level() {
  let valid = resolve(ConfigResolutionInputs {
    runtime: runtime(r#"{"schema_version":1,"memory":{"hard_limit_bytes":7516192768}}"#),
    ..Default::default()
  });
  assert!(valid.complete(), "{:?}", valid.issues);
  assert_eq!(u64_value(&valid, "memory.hard_limit_bytes"), 7 * GIB);
  assert_eq!(valid.property("memory.hard_limit_bytes").unwrap().source, Some(ConfigSource::StoredRuntimeV1));

  for invalid in [
    r#"{"schema_version":1,"schema_version":1}"#,
    r#"{"schema_version":1,"memory":{},"memory":{}}"#,
    r#"{"schema_version":1,"memory":{"hard_limit_bytes":4294967296,"hard_limit_bytes":4294967296}}"#,
    r#"{"schema_version":1,"unknown":{}}"#,
    r#"{"schema_version":1,"memory":{"unknown":1}}"#,
    r#"{"schema_version":2}"#,
    r#"{"schema_version":1,"memory":{"hard_limit_bytes":-1}}"#,
    r#"{"schema_version":1} trailing"#,
  ] {
    let resolution = resolve(ConfigResolutionInputs { runtime: runtime(invalid), ..Default::default() });
    assert!(resolution.degraded(), "document should be degraded: {invalid}");
    assert!(!resolution.complete(), "invalid document must not silently default: {invalid}");
  }
}

#[test]
fn lifecycle_v0_and_v1_decode_explicitly_while_v1_rejects_unknowns() {
  let legacy = resolve(ConfigResolutionInputs {
    lifecycle: lifecycle(r#"{"snapshot_writes_enabled":false,"snapshot_retention":{"auto_months":2}}"#),
    ..Default::default()
  });
  assert!(legacy.complete(), "{:?}", legacy.issues);
  assert_eq!(legacy.property("lifecycle.snapshot_writes_enabled").unwrap().value, Some(ConfigValue::Boolean(false)));
  assert_eq!(legacy.property("lifecycle.snapshot_writes_enabled").unwrap().source, Some(ConfigSource::StoredLifecycleV0));
  assert_eq!(u64_value(&legacy, "lifecycle.snapshot_retention_auto_months"), 2);

  let current = resolve(ConfigResolutionInputs {
    lifecycle: lifecycle(
      r#"{"schema_version":1,"snapshot_writes_enabled":true,"snapshot_retention":{"auto_months":1,"manual_months":12},"garbage_collection":{"pending_delete_grace_seconds":0}}"#,
    ),
    ..Default::default()
  });
  assert!(current.complete(), "{:?}", current.issues);
  assert_eq!(u64_value(&current, "lifecycle.garbage_collection_pending_delete_grace_seconds"), 0);

  for invalid in [
    r#"{"schema_version":1,"unknown":true}"#,
    r#"{"schema_version":1,"garbage_collection":{"pending_delete_grace_seconds":1,"pending_delete_grace_seconds":2}}"#,
    r#"{"schema_version":1,"snapshot_retention":{"auto_months":4294967296}}"#,
    r#"{"schema_version":-1}"#,
  ] {
    let resolution = resolve(ConfigResolutionInputs { lifecycle: lifecycle(invalid), ..Default::default() });
    assert!(resolution.degraded(), "{invalid}");
    assert!(!resolution.complete(), "{invalid}");
  }
}

#[test]
fn precedence_is_per_property_and_valid_higher_sources_survive_broken_storage() {
  let mut inputs =
    ConfigResolutionInputs { runtime: runtime(r#"{"schema_version":1,"memory":{"hard_limit_bytes":7516192768}}"#), ..Default::default() };
  inputs.environment.insert("AEORDB_MEMORY_HARD_LIMIT_BYTES".into(), OsString::from("7680MiB"));
  inputs.cli.insert("--memory-hard-limit-bytes".into(), OsString::from("8GiB"));
  let resolution = resolve(inputs);
  assert!(resolution.complete(), "{:?}", resolution.issues);
  assert_eq!(u64_value(&resolution, "memory.hard_limit_bytes"), 8 * GIB);
  assert_eq!(resolution.property("memory.hard_limit_bytes").unwrap().source, Some(ConfigSource::CommandLine));

  let mut broken = ConfigResolutionInputs { runtime: runtime("not-json"), ..Default::default() };
  broken.environment.insert("AEORDB_MEMORY_HARD_LIMIT_BYTES".into(), OsString::from("7GiB"));
  let resolution = resolve(broken);
  assert!(resolution.degraded());
  assert_eq!(u64_value(&resolution, "memory.hard_limit_bytes"), 7 * GIB);
  assert_eq!(resolution.property("memory.hard_limit_bytes").unwrap().source, Some(ConfigSource::Environment));
  assert!(resolution.property("memory.soft_limit_bytes").unwrap().value.is_none());
  assert!(!resolution.owner_ready("index_cache"));
}

#[test]
fn derived_defaults_follow_the_effective_hard_limit_from_every_configuration_source() {
  let mut environment = ConfigResolutionInputs::default();
  environment.environment.insert("AEORDB_MEMORY_HARD_LIMIT_BYTES".into(), OsString::from("4GiB"));
  let environment_resolution = resolve(environment);
  assert!(environment_resolution.complete(), "{:?}", environment_resolution.issues);
  assert_eq!(u64_value(&environment_resolution, "memory.hard_limit_bytes"), 4 * GIB);
  assert_eq!(u64_value(&environment_resolution, "memory.soft_limit_bytes"), 3 * GIB);
  assert_eq!(u64_value(&environment_resolution, "cache.index_clean_max_bytes"), GIB);
  assert_eq!(u64_value(&environment_resolution, "cache.directory_max_bytes"), 256 * MIB);
  assert_eq!(u64_value(&environment_resolution, "cache.kv_resident_max_bytes"), GIB);
  assert_eq!(u64_value(&environment_resolution, "index.mutation_buffer_max_bytes"), 512 * MIB);
  assert_eq!(u64_value(&environment_resolution, "query.per_request_memory_bytes"), 64 * MIB);
  assert_eq!(u64_value(&environment_resolution, "query.global_memory_bytes"), 512 * MIB);
  for path in [
    "memory.soft_limit_bytes",
    "cache.index_clean_max_bytes",
    "cache.directory_max_bytes",
    "cache.kv_resident_max_bytes",
    "index.mutation_buffer_max_bytes",
    "query.per_request_memory_bytes",
    "query.global_memory_bytes",
  ] {
    assert_eq!(environment_resolution.property(path).unwrap().source, Some(ConfigSource::Default), "{path}");
  }

  let stored = resolve(ConfigResolutionInputs {
    runtime: runtime(r#"{"schema_version":1,"memory":{"hard_limit_bytes":4294967296}}"#),
    ..Default::default()
  });
  assert!(stored.complete(), "{:?}", stored.issues);
  assert_eq!(u64_value(&stored, "memory.hard_limit_bytes"), 4 * GIB);
  assert_eq!(stored.property("memory.hard_limit_bytes").unwrap().source, Some(ConfigSource::StoredRuntimeV1));
  assert_eq!(u64_value(&stored, "memory.soft_limit_bytes"), 3 * GIB);
  assert_eq!(u64_value(&stored, "cache.index_clean_max_bytes"), GIB);

  let mut command_line = ConfigResolutionInputs::default();
  command_line.cli.insert("--memory-hard-limit-bytes".into(), OsString::from("2GiB"));
  let command_line_resolution = resolve(command_line);
  assert!(command_line_resolution.complete(), "{:?}", command_line_resolution.issues);
  assert_eq!(u64_value(&command_line_resolution, "memory.soft_limit_bytes"), 1536 * MIB);
  assert_eq!(u64_value(&command_line_resolution, "cache.index_clean_max_bytes"), 512 * MIB);
  assert_eq!(u64_value(&command_line_resolution, "query.per_request_memory_bytes"), 32 * MIB);

  let last_known_good = resolve(ConfigResolutionInputs {
    runtime: runtime("{"),
    runtime_lkg: Some(ConfigFallback {
      bytes: br#"{"schema_version":1,"memory":{"hard_limit_bytes":4294967296}}"#.to_vec(),
      identity: "runtime-lkg-4g".into(),
      recorded_at_ms: 100,
    }),
    ..Default::default()
  });
  assert_eq!(u64_value(&last_known_good, "memory.soft_limit_bytes"), 3 * GIB);
  assert_eq!(u64_value(&last_known_good, "cache.index_clean_max_bytes"), GIB);

  let mut explicit_soft = ConfigResolutionInputs {
    runtime: runtime(r#"{"schema_version":1,"memory":{"soft_limit_bytes":2684354560,"hard_limit_bytes":4294967296}}"#),
    ..Default::default()
  };
  explicit_soft.cli.insert("--memory-hard-limit-bytes".into(), OsString::from("5GiB"));
  let explicit_soft_resolution = resolve(explicit_soft);
  assert!(explicit_soft_resolution.complete(), "{:?}", explicit_soft_resolution.issues);
  assert_eq!(u64_value(&explicit_soft_resolution, "memory.soft_limit_bytes"), 2560 * MIB);
  assert_eq!(explicit_soft_resolution.property("memory.soft_limit_bytes").unwrap().source, Some(ConfigSource::StoredRuntimeV1));
}

#[test]
fn invalid_winning_source_never_falls_through_but_invalid_lower_source_is_visible_only() {
  let mut invalid_environment = ConfigResolutionInputs::default();
  invalid_environment.environment.insert("AEORDB_MEMORY_HARD_LIMIT_BYTES".into(), OsString::from("4GB"));
  let resolution = resolve(invalid_environment);
  assert!(resolution.property("memory.hard_limit_bytes").unwrap().value.is_none());
  assert!(!resolution.complete());

  let mut valid_cli = ConfigResolutionInputs::default();
  valid_cli.environment.insert("AEORDB_MEMORY_HARD_LIMIT_BYTES".into(), OsString::from("4GB"));
  valid_cli.cli.insert("--memory-hard-limit-bytes".into(), OsString::from("4GiB"));
  let resolution = resolve(valid_cli);
  assert_eq!(u64_value(&resolution, "memory.hard_limit_bytes"), 4 * GIB);
  assert!(resolution.degraded(), "invalid lower environment must remain visible");
}

#[test]
fn invalid_current_document_uses_lkg_then_newest_valid_history_but_missing_uses_defaults() {
  let lkg = ConfigFallback {
    bytes: br#"{"schema_version":1,"memory":{"soft_limit_bytes":3221225472,"hard_limit_bytes":4294967296}}"#.to_vec(),
    identity: "lkg-1".into(),
    recorded_at_ms: 100,
  };
  let resolution = resolve(ConfigResolutionInputs { runtime: runtime("{"), runtime_lkg: Some(lkg), ..Default::default() });
  assert_eq!(u64_value(&resolution, "memory.hard_limit_bytes"), 4 * GIB);
  assert_eq!(resolution.property("memory.hard_limit_bytes").unwrap().source, Some(ConfigSource::LastKnownGood));
  assert_eq!(resolution.property("cache.index_clean_max_bytes").unwrap().source, Some(ConfigSource::Default));

  let history = vec![
    ConfigFallback { bytes: b"bad".to_vec(), identity: "newest-bad".into(), recorded_at_ms: 300 },
    ConfigFallback {
      bytes: br#"{"schema_version":1,"memory":{"soft_limit_bytes":4294967296,"hard_limit_bytes":5368709120}}"#.to_vec(),
      identity: "older-good".into(),
      recorded_at_ms: 200,
    },
  ];
  let resolution = resolve(ConfigResolutionInputs { runtime: runtime("{"), runtime_history: history, ..Default::default() });
  assert_eq!(u64_value(&resolution, "memory.hard_limit_bytes"), 5 * GIB);
  assert_eq!(resolution.property("memory.hard_limit_bytes").unwrap().source, Some(ConfigSource::AppendHistory));

  let missing = resolve(ConfigResolutionInputs {
    runtime: ConfigDocumentInput::Missing,
    runtime_lkg: Some(ConfigFallback {
      bytes: br#"{"schema_version":1,"memory":{"hard_limit_bytes":4294967296}}"#.to_vec(),
      identity: "stale".into(),
      recorded_at_ms: 1,
    }),
    ..Default::default()
  });
  assert_eq!(u64_value(&missing, "memory.hard_limit_bytes"), 8 * GIB);
  assert_eq!(missing.property("memory.hard_limit_bytes").unwrap().source, Some(ConfigSource::Default));
}

#[test]
fn cross_invalid_current_and_lkg_documents_are_rejected_as_complete_layers() {
  let cross_invalid = r#"{"schema_version":1,"memory":{"soft_limit_bytes":6442450944,"hard_limit_bytes":4294967296}}"#;
  let current = resolve(ConfigResolutionInputs { runtime: runtime(cross_invalid), ..Default::default() });
  assert!(current.degraded());
  assert!(current.property("memory.hard_limit_bytes").unwrap().value.is_none());
  assert!(current.issues.iter().any(|issue| issue.message.contains("soft_limit_bytes")));

  let resolution = resolve(ConfigResolutionInputs {
    runtime: runtime("{"),
    runtime_lkg: Some(ConfigFallback { bytes: cross_invalid.as_bytes().to_vec(), identity: "cross-invalid".into(), recorded_at_ms: 300 }),
    runtime_history: vec![ConfigFallback {
      bytes: br#"{"schema_version":1,"memory":{"soft_limit_bytes":4294967296,"hard_limit_bytes":5368709120}}"#.to_vec(),
      identity: "history-valid".into(),
      recorded_at_ms: 200,
    }],
    ..Default::default()
  });
  assert_eq!(u64_value(&resolution, "memory.hard_limit_bytes"), 5 * GIB);
  assert_eq!(resolution.property("memory.hard_limit_bytes").unwrap().source, Some(ConfigSource::AppendHistory));
  assert!(resolution.fallback_identities.iter().any(|identity| identity == "history-valid"));
}

#[test]
fn legacy_aliases_are_visible_and_conflicts_fail_closed() {
  let mut alias = ConfigResolutionInputs::default();
  alias.environment.insert("AEORDB_INDEX_CACHE_MAX_BYTES".into(), OsString::from("512MiB"));
  let resolution = resolve(alias);
  assert_eq!(u64_value(&resolution, "cache.index_clean_max_bytes"), 512 * MIB);
  assert_eq!(resolution.property("cache.index_clean_max_bytes").unwrap().source, Some(ConfigSource::DeprecatedEnvironment));
  assert!(resolution.deprecated_aliases.iter().any(|name| name == "AEORDB_INDEX_CACHE_MAX_BYTES"));

  let mut conflict = ConfigResolutionInputs::default();
  conflict.environment.insert("AEORDB_INDEX_CACHE_MAX_BYTES".into(), OsString::from("512MiB"));
  conflict.environment.insert("AEORDB_CACHE_INDEX_CLEAN_MAX_BYTES".into(), OsString::from("1GiB"));
  let resolution = resolve(conflict);
  assert!(resolution.property("cache.index_clean_max_bytes").unwrap().value.is_none());
  assert!(!resolution.complete());
}

#[test]
fn every_transitional_environment_alias_maps_to_exactly_one_frozen_property() {
  let (spill_input, spill_expected) = alternate_spill_paths();
  let cases = [
    ("AEORDB_INDEX_CACHE_MAX_BYTES", "512MiB", "cache.index_clean_max_bytes", ConfigValue::Unsigned(512 * MIB)),
    ("AEORDB_INDEX_CACHE_CLEAN_TTL_SECS", "45", "cache.index_clean_ttl_seconds", ConfigValue::Unsigned(45)),
    ("AEORDB_EMERGENCY_SPILL_DIR", spill_input.to_str().unwrap(), "recovery.emergency_spill_dir", ConfigValue::Path(spill_expected)),
    ("AEORDB_EMERGENCY_WAL_SPILL_MAX_BYTES", "128MiB", "recovery.emergency_spill_max_bytes", ConfigValue::Unsigned(128 * MIB)),
    ("AEORDB_SHUTDOWN_OPERATION_WAIT_SECS", "42", "shutdown.operation_wait_seconds", ConfigValue::Unsigned(42)),
  ];
  for (alias, raw, path, expected) in cases {
    let mut inputs = ConfigResolutionInputs::default();
    inputs.environment.insert(alias.into(), OsString::from(raw));
    let resolution = resolve(inputs);
    assert_eq!(resolution.property(path).unwrap().value, Some(expected), "{alias}");
    assert_eq!(resolution.property(path).unwrap().source, Some(ConfigSource::DeprecatedEnvironment), "{alias}");
    assert!(resolution.deprecated_aliases.iter().any(|name| name == alias), "{alias}");
  }
}

#[test]
fn stored_optional_auto_and_wrong_structural_kinds_are_handled_strictly() {
  let (_, default_gc_workspace_root, default_emergency_spill_dir) = native_paths();
  let valid = resolve(ConfigResolutionInputs {
    runtime: runtime(
      r#"{"schema_version":1,"garbage_collection":{"mark_scratch_max_bytes":null,"mark_workspace_root":"auto"},"recovery":{"emergency_spill_dir":"auto"}}"#,
    ),
    ..Default::default()
  });
  assert!(valid.complete(), "{:?}", valid.issues);
  assert_eq!(valid.property("garbage_collection.mark_scratch_max_bytes").unwrap().value, Some(ConfigValue::OptionalBytes(None)));
  assert_eq!(valid.property("garbage_collection.mark_workspace_root").unwrap().value, Some(ConfigValue::Path(default_gc_workspace_root)));
  assert_eq!(valid.property("recovery.emergency_spill_dir").unwrap().value, Some(ConfigValue::Path(default_emergency_spill_dir)));

  for invalid in [
    r#"{"schema_version":1,"memory":[]}"#,
    r#"{"schema_version":1,"memory":{"hard_limit_bytes":true}}"#,
    r#"{"schema_version":1,"garbage_collection":{"mark_scratch_max_bytes":"unbounded"}}"#,
    r#"{"schema_version":1,"recovery":{"emergency_spill_dir":"relative/path"}}"#,
  ] {
    let resolution = resolve(ConfigResolutionInputs { runtime: runtime(invalid), ..Default::default() });
    assert!(resolution.degraded(), "{invalid}");
    assert!(!resolution.complete(), "{invalid}");
  }
}

#[test]
fn accepted_paths_are_returned_in_normalized_form() {
  let (spill_input, spill_expected) = alternate_spill_paths();
  let mut inputs = ConfigResolutionInputs::default();
  inputs.environment.insert("AEORDB_RECOVERY_EMERGENCY_SPILL_DIR".into(), spill_input.into());
  let resolution = resolve(inputs);
  assert!(resolution.complete(), "{:?}", resolution.issues);
  assert_eq!(resolution.property("recovery.emergency_spill_dir").unwrap().value, Some(ConfigValue::Path(spill_expected.clone())));
  let ConfigValue::Path(path) = resolution.property("recovery.emergency_spill_dir").unwrap().value.as_ref().unwrap() else {
    panic!("expected normalized spill path");
  };
  assert_eq!(path.as_os_str(), spill_expected.as_os_str());
}

#[test]
fn checked_quantities_paths_and_cross_property_constraints_reject_ambiguous_values() {
  for value in ["1.5GiB", "1GB", "-1", "18446744073709551615TiB"] {
    let mut inputs = ConfigResolutionInputs::default();
    inputs.environment.insert("AEORDB_CACHE_INDEX_CLEAN_MAX_BYTES".into(), OsString::from(value));
    assert!(resolve(inputs).property("cache.index_clean_max_bytes").unwrap().value.is_none(), "{value}");
  }

  let mut relative_path = ConfigResolutionInputs::default();
  relative_path.environment.insert("AEORDB_RECOVERY_EMERGENCY_SPILL_DIR".into(), OsString::from("relative/path"));
  assert!(resolve(relative_path).property("recovery.emergency_spill_dir").unwrap().value.is_none());

  let mut crossed = ConfigResolutionInputs::default();
  crossed.environment.insert("AEORDB_MEMORY_HARD_LIMIT_BYTES".into(), OsString::from("2GiB"));
  crossed.environment.insert("AEORDB_MEMORY_SOFT_LIMIT_BYTES".into(), OsString::from("1900MiB"));
  crossed.environment.insert("AEORDB_MEMORY_EMERGENCY_RESERVE_BYTES".into(), OsString::from("256MiB"));
  let resolution = resolve(crossed);
  assert!(!resolution.complete());
  assert!(resolution.issues.iter().any(|issue| issue.message.contains("soft_limit_bytes")));
}

#[test]
fn unreadable_current_document_does_not_apply_defaults_without_a_valid_fallback() {
  let resolution =
    resolve(ConfigResolutionInputs { lifecycle: ConfigDocumentInput::Unreadable("permission denied".into()), ..Default::default() });
  assert!(resolution.degraded());
  assert!(resolution.property("lifecycle.snapshot_writes_enabled").unwrap().value.is_none());
  assert!(!resolution.owner_ready("lifecycle_runtime"));
  assert!(resolution.owner_ready("read_runtime"));
}

#[test]
fn valid_higher_override_supplies_an_unavailable_auto_path_without_hiding_degradation() {
  let mut unavailable = context();
  unavailable.default_emergency_spill_dir = None;
  let mut inputs = ConfigResolutionInputs::default();
  let (_, explicit_spill_path) = alternate_spill_paths();
  inputs.environment.insert("AEORDB_RECOVERY_EMERGENCY_SPILL_DIR".into(), explicit_spill_path.as_os_str().into());

  let resolution = ConfigResolver::new(unavailable).resolve(inputs);
  assert!(resolution.complete(), "{:?}", resolution.issues);
  assert!(resolution.degraded(), "the unavailable lower auto default must remain visible");
  assert_eq!(resolution.property("recovery.emergency_spill_dir").unwrap().value, Some(ConfigValue::Path(explicit_spill_path)));
  assert_eq!(resolution.property("recovery.emergency_spill_dir").unwrap().source, Some(ConfigSource::Environment));
}
