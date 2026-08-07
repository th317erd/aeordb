use aeordb::engine::HashAlgorithm;
use aeordb::engine::config_resolver::{ConfigDocumentStatus, ConfigSource, ConfigValue, ConfigurationFamily, RUNTIME_CONFIG_PATH};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::durability_coordinator::{DurabilityOperation, OsErrorClass};
use aeordb::engine::lifecycle_config::{LIFECYCLE_CONFIG_PATH, load_lifecycle_config};
use aeordb::engine::v4::configuration_controls::ConfigurationControlCapability;
use aeordb::engine::v4::config_value::{CanonicalValueBounds, canonicalize_json};
use aeordb::engine::v4::control_store::V3TransitionControlStore;
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::system_control::{
  ConfigLKGBodyV1, ConfigurationKindV1, DurabilityLatchBodyV1, EmergencySpillCatalogBodyV1, EmergencySpillCatalogRowV1,
  SystemControlKindV1, SystemControlSlotV1, decode_config_diagnostics_body, decode_config_lkg_body, decode_system_control,
  encode_config_lkg_control, encode_durability_latch_control, encode_emergency_spill_catalog_control, system_control_path,
};
use aeordb::engine::{RequestContext, StorageEngine};

fn create_engine(directory: &tempfile::TempDir) -> (String, StorageEngine) {
  let path = directory.path().join("config-controls.aeordb").to_string_lossy().into_owned();
  let engine = StorageEngine::create(&path).unwrap();
  DirectoryOps::new(&engine).ensure_root_directory(&RequestContext::system()).unwrap();
  (path, engine)
}

fn canonical_utf8(value: &str) -> Vec<u8> {
  let mut encoded = Vec::with_capacity(5 + value.len());
  encoded.push(0x07);
  encoded.extend_from_slice(&(value.len() as u32).to_le_bytes());
  encoded.extend_from_slice(value.as_bytes());
  encoded
}

fn publish_cleared_identity(engine: &StorageEngine, database_id: [u8; 16]) {
  let algorithm = HashAlgorithm::Blake3_256;
  let catalog = EmergencySpillCatalogBodyV1 {
    database_id,
    catalog_generation: 1,
    discovered_at_ms: 1_725_000_000_000,
    state: 3,
    flags: 0,
    repair_receipt_hash: vec![0x51; 32],
    rows: vec![EmergencySpillCatalogRowV1 {
      source_location_class: 1,
      replay_state: 2,
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
    state: 3,
    failed_operation: DurabilityOperation::AuthorityBarrier.stable_id(),
    os_error_class: OsErrorClass::MediaIo.stable_id(),
    os_error_code: 5,
    flags: 1,
    last_selected_header_sequence: 7,
    last_durable_write_sequence: 8,
    last_durable_publication_sequence: 8,
    emergency_spill_catalog_payload_hash: catalog_payload_hash,
    evidence_digest,
    diagnostic: canonical_utf8("cleared durability recovery identity"),
  };
  let latch_bytes = encode_durability_latch_control(1, &latch, algorithm).unwrap();
  let store = V3TransitionControlStore::new(engine);
  store.publish_mutable(SystemControlKindV1::EmergencySpillCatalog, database_id, &[], &catalog_bytes).unwrap();
  store.publish_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[], &latch_bytes).unwrap();
}

fn reopen(engine: StorageEngine, path: &str) -> StorageEngine {
  engine.shutdown().unwrap();
  drop(engine);
  StorageEngine::open(path).unwrap()
}

#[test]
fn identityless_v3_updates_remain_valid_and_report_control_capability_unavailable() {
  let directory = tempfile::tempdir().unwrap();
  let (_, engine) = create_engine(&directory);
  let document = br#"{"schema_version":1,"snapshot_writes_enabled":false}"#;

  let updated = engine.replace_configuration_document(ConfigurationFamily::Lifecycle, document).unwrap();

  let status = updated.control_status(ConfigurationFamily::Lifecycle);
  assert_eq!(status.capability, ConfigurationControlCapability::UnavailableNoDatabaseIdentity);
  assert_eq!(status.lkg_sequence, None);
  assert_eq!(status.diagnostics_sequence, None);
  assert!(status.errors.is_empty());
  assert!(V3TransitionControlStore::new(&engine).discover_mutable(SystemControlKindV1::LifecycleLastKnownGood, &[]).unwrap().is_none());

  let legacy = br#"{"snapshot_writes_enabled":true}"#;
  let error = engine.replace_configuration_document(ConfigurationFamily::Lifecycle, legacy).unwrap_err();
  assert!(error.to_string().contains("schema"), "{error}");
  assert_eq!(DirectoryOps::new(&engine).read_file_buffered(LIFECYCLE_CONFIG_PATH).unwrap(), document);
}

#[test]
fn stable_transition_identity_publishes_controls_and_recovers_lkg_after_current_corruption() {
  let directory = tempfile::tempdir().unwrap();
  let (path, engine) = create_engine(&directory);
  let database_id = [0x61; 16];
  publish_cleared_identity(&engine, database_id);
  let engine = reopen(engine, &path);
  let document = br#"{"schema_version":1,"snapshot_writes_enabled":false,"snapshot_retention":{"auto_months":2,"manual_months":9}}"#;

  let updated = engine.replace_configuration_document(ConfigurationFamily::Lifecycle, document).unwrap();

  let status = updated.control_status(ConfigurationFamily::Lifecycle);
  assert_eq!(status.capability, ConfigurationControlCapability::Available);
  assert_eq!(status.database_id, Some(database_id));
  assert_eq!(status.lkg_sequence, Some(1));
  assert_eq!(status.diagnostics_sequence, Some(1));
  assert!(status.errors.is_empty());
  let store = V3TransitionControlStore::new(&engine);
  let lkg = store.load_mutable(SystemControlKindV1::LifecycleLastKnownGood, database_id, &[]).unwrap().unwrap();
  let lkg_control = decode_system_control(&lkg.bytes, engine.hash_algo()).unwrap();
  let lkg_body = decode_config_lkg_body(lkg_control.body, engine.hash_algo()).unwrap();
  assert_eq!(lkg_body.configuration_schema, 1);
  assert_eq!(
    lkg_body.source_file_content_hash,
    DirectoryOps::new(&engine).get_metadata(LIFECYCLE_CONFIG_PATH).unwrap().unwrap().content_hash
  );
  let diagnostics = store.load_mutable(SystemControlKindV1::LifecycleDiagnostics, database_id, &[]).unwrap().unwrap();
  let diagnostics_control = decode_system_control(&diagnostics.bytes, engine.hash_algo()).unwrap();
  let diagnostics_body = decode_config_diagnostics_body(diagnostics_control.body, engine.hash_algo()).unwrap();
  assert_eq!(diagnostics_body.aggregate_state, 1);
  assert_eq!(diagnostics_body.disabled_capability_count, 0);

  let newest = br#"{"schema_version":1,"snapshot_writes_enabled":false,"snapshot_retention":{"auto_months":3,"manual_months":10}}"#;
  let advanced = engine.replace_configuration_document(ConfigurationFamily::Lifecycle, newest).unwrap();
  assert_eq!(advanced.control_status(ConfigurationFamily::Lifecycle).lkg_sequence, Some(2));
  assert_eq!(advanced.control_status(ConfigurationFamily::Lifecycle).diagnostics_sequence, Some(2));

  for slot in [SystemControlSlotV1::A, SystemControlSlotV1::B] {
    let diagnostics_path = system_control_path(SystemControlKindV1::LifecycleDiagnostics, &[], slot).unwrap();
    DirectoryOps::new(&engine)
      .store_file_buffered(&RequestContext::system(), &diagnostics_path, b"broken", Some("application/octet-stream"))
      .unwrap();
  }
  DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), LIFECYCLE_CONFIG_PATH, b"{", Some("application/json")).unwrap();
  let engine = reopen(engine, &path);
  let snapshot = engine.configuration_snapshot();
  let resolution = snapshot.desired.resolution.as_ref().unwrap();
  assert!(matches!(resolution.lifecycle_status, ConfigDocumentStatus::Invalid { .. }));
  assert_eq!(resolution.property("lifecycle.snapshot_writes_enabled").unwrap().source, Some(ConfigSource::LastKnownGood));
  let recovered = load_lifecycle_config(&engine);
  assert!(!recovered.snapshot_writes_enabled);
  assert_eq!(recovered.snapshot_retention.auto_months, 3);
  assert_eq!(recovered.snapshot_retention.manual_months, 10);
  let status = snapshot.control_status(ConfigurationFamily::Lifecycle);
  assert_eq!(status.capability, ConfigurationControlCapability::Degraded);
  assert_eq!(status.lkg_sequence, Some(2));
  assert_eq!(status.diagnostics_sequence, None);
  assert!(!status.errors.is_empty());
}

#[test]
fn corrupt_lkg_never_blocks_a_valid_current_configuration() {
  let directory = tempfile::tempdir().unwrap();
  let (path, engine) = create_engine(&directory);
  let database_id = [0x62; 16];
  publish_cleared_identity(&engine, database_id);
  let engine = reopen(engine, &path);
  let document = br#"{"schema_version":1,"snapshot_writes_enabled":false}"#;
  engine.replace_configuration_document(ConfigurationFamily::Lifecycle, document).unwrap();
  let lkg_path = system_control_path(SystemControlKindV1::LifecycleLastKnownGood, &[], SystemControlSlotV1::A).unwrap();
  DirectoryOps::new(&engine)
    .store_file_buffered(&RequestContext::system(), &lkg_path, b"broken", Some("application/octet-stream"))
    .unwrap();

  let engine = reopen(engine, &path);

  assert_eq!(DirectoryOps::new(&engine).read_file_buffered(LIFECYCLE_CONFIG_PATH).unwrap(), document);
  assert!(!load_lifecycle_config(&engine).snapshot_writes_enabled);
  let status = engine.configuration_snapshot().control_status(ConfigurationFamily::Lifecycle).clone();
  assert_eq!(status.capability, ConfigurationControlCapability::Degraded);
  assert!(!status.errors.is_empty());
}

#[test]
fn post_file_control_failure_activates_durable_current_policy_and_surfaces_degradation() {
  let directory = tempfile::tempdir().unwrap();
  let (path, engine) = create_engine(&directory);
  let database_id = [0x63; 16];
  publish_cleared_identity(&engine, database_id);
  let engine = reopen(engine, &path);
  let lkg_path = system_control_path(SystemControlKindV1::LifecycleLastKnownGood, &[], SystemControlSlotV1::A).unwrap();
  DirectoryOps::new(&engine)
    .store_file_buffered(&RequestContext::system(), &lkg_path, b"broken", Some("application/octet-stream"))
    .unwrap();
  let document = br#"{"schema_version":1,"snapshot_writes_enabled":false}"#;

  let updated = engine.replace_configuration_document(ConfigurationFamily::Lifecycle, document).unwrap();

  assert!(!updated.resolved_boolean("lifecycle.snapshot_writes_enabled").unwrap());
  assert_eq!(DirectoryOps::new(&engine).read_file_buffered(LIFECYCLE_CONFIG_PATH).unwrap(), document);
  let status = updated.control_status(ConfigurationFamily::Lifecycle);
  assert_eq!(status.capability, ConfigurationControlCapability::Degraded);
  assert!(!status.errors.is_empty());
  let reopened = reopen(engine, &path);
  assert!(!load_lifecycle_config(&reopened).snapshot_writes_enabled);
}

#[test]
fn runtime_family_uses_its_distinct_controls_and_recovers_after_current_corruption() {
  let directory = tempfile::tempdir().unwrap();
  let (path, engine) = create_engine(&directory);
  let database_id = [0x64; 16];
  publish_cleared_identity(&engine, database_id);
  let engine = reopen(engine, &path);
  let document = br#"{"schema_version":1,"index":{"flush_after_seconds":45}}"#;
  let updated = engine.replace_configuration_document(ConfigurationFamily::Runtime, document).unwrap();
  let status = updated.control_status(ConfigurationFamily::Runtime);
  assert_eq!(status.lkg_sequence, Some(1));
  assert_eq!(status.diagnostics_sequence, Some(1));
  let store = V3TransitionControlStore::new(&engine);
  assert!(store.load_mutable(SystemControlKindV1::RuntimeLastKnownGood, database_id, &[]).unwrap().is_some());
  assert!(store.load_mutable(SystemControlKindV1::RuntimeDiagnostics, database_id, &[]).unwrap().is_some());

  DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), RUNTIME_CONFIG_PATH, b"{", Some("application/json")).unwrap();
  let reopened = reopen(engine, &path);
  let resolution = reopened.configuration_snapshot().desired.resolution.as_ref().unwrap().clone();
  assert!(matches!(resolution.runtime_status, ConfigDocumentStatus::Invalid { .. }));
  assert_eq!(resolution.property("index.flush_after_seconds").unwrap().value, Some(ConfigValue::Unsigned(45)));
  assert_eq!(resolution.property("index.flush_after_seconds").unwrap().source, Some(ConfigSource::LastKnownGood));
}

#[test]
fn crc_valid_lkg_with_wrong_policy_fingerprint_is_never_used_as_fallback() {
  let directory = tempfile::tempdir().unwrap();
  let (path, engine) = create_engine(&directory);
  let database_id = [0x65; 16];
  publish_cleared_identity(&engine, database_id);
  let engine = reopen(engine, &path);
  let canonical = canonicalize_json(br#"{"schema_version":1,"snapshot_writes_enabled":true}"#, CanonicalValueBounds::AUDIT_VALUE).unwrap();
  let forged = ConfigLKGBodyV1 {
    database_id,
    configuration_kind: ConfigurationKindV1::Lifecycle,
    configuration_schema: 1,
    activated_at_ms: 1_725_000_010_000,
    source_namespace_root: vec![0x41; 32],
    source_file_content_hash: vec![0x42; 32],
    policy_fingerprint: [0x43; 32],
    canonical_config: canonical,
  };
  let bytes = encode_config_lkg_control(1, &forged, engine.hash_algo()).unwrap();
  V3TransitionControlStore::new(&engine).publish_mutable(SystemControlKindV1::LifecycleLastKnownGood, database_id, &[], &bytes).unwrap();
  DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), LIFECYCLE_CONFIG_PATH, b"{", Some("application/json")).unwrap();

  let reopened = reopen(engine, &path);

  assert!(!reopened.configuration_snapshot().resolved_boolean("lifecycle.snapshot_writes_enabled").unwrap_or(false));
  let status = reopened.configuration_snapshot().control_status(ConfigurationFamily::Lifecycle).clone();
  assert_eq!(status.capability, ConfigurationControlCapability::Degraded);
  assert!(status.errors.iter().any(|error| error.contains("fingerprint")), "{:?}", status.errors);
}
