//! V3-compatible publication and recovery for runtime/lifecycle controls.

use std::collections::BTreeMap;

use crate::engine::config_resolver::{ConfigFallback, ConfigSource, ConfigValue, ConfigurationFamily, MAX_CONFIG_DOCUMENT_BYTES};
use crate::engine::configuration_authority::{ActiveConfigProperty, ConfigurationAuthoritySnapshot};
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;

use super::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, canonical_value_to_json, canonicalize_json, decode_canonical_value};
use super::control_store::V3TransitionControlStore;
use super::system_control::{
  ConfigDiagnosticsBodyV1, ConfigLKGBodyV1, ConfigurationKindV1, SystemControlKindV1, decode_config_diagnostics_body,
  decode_config_lkg_body, decode_system_control, encode_config_diagnostics_control, encode_config_lkg_control,
};

const POLICY_FINGERPRINT_DOMAIN: &[u8] = b"aeordb.configuration-policy.v1\0";
const EFFECTIVE_FINGERPRINT_DOMAIN: &[u8] = b"aeordb.effective-configuration.v1\0";
const MAX_CONTROL_ERRORS: usize = 8;
const MAX_CONTROL_ERROR_CHARS: usize = 1_024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigurationControlCapability {
  UnavailableNoDatabaseIdentity,
  Available,
  Degraded,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ConfigurationControlFamilyStatus {
  pub capability: ConfigurationControlCapability,
  pub database_id: Option<[u8; 16]>,
  pub lkg_sequence: Option<u64>,
  pub lkg_activated_at_ms: Option<i64>,
  pub diagnostics_sequence: Option<u64>,
  pub redundancy_degraded: bool,
  pub errors: Vec<String>,
}

impl ConfigurationControlFamilyStatus {
  fn identityless() -> Self {
    Self {
      capability: ConfigurationControlCapability::UnavailableNoDatabaseIdentity,
      database_id: None,
      lkg_sequence: None,
      lkg_activated_at_ms: None,
      diagnostics_sequence: None,
      redundancy_degraded: false,
      errors: Vec::new(),
    }
  }

  fn available(database_id: [u8; 16]) -> Self {
    Self {
      capability: ConfigurationControlCapability::Available,
      database_id: Some(database_id),
      lkg_sequence: None,
      lkg_activated_at_ms: None,
      diagnostics_sequence: None,
      redundancy_degraded: false,
      errors: Vec::new(),
    }
  }

  fn record_error(&mut self, context: &str, error: impl std::fmt::Display) {
    self.capability = ConfigurationControlCapability::Degraded;
    if self.errors.len() >= MAX_CONTROL_ERRORS {
      return;
    }
    let mut message = format!("{context}: {error}");
    if message.chars().count() > MAX_CONTROL_ERROR_CHARS {
      message = message.chars().take(MAX_CONTROL_ERROR_CHARS).collect();
    }
    tracing::error!(family = context, error = %message, "Configuration transition control is degraded");
    self.errors.push(message);
  }
}

#[derive(Clone, Debug)]
pub struct LoadedConfigurationControls {
  pub statuses: BTreeMap<ConfigurationFamily, ConfigurationControlFamilyStatus>,
  pub runtime_lkg: Option<ConfigFallback>,
  pub lifecycle_lkg: Option<ConfigFallback>,
}

pub fn load_configuration_controls(engine: &StorageEngine) -> LoadedConfigurationControls {
  let Some(database_id) = engine.persistent_durability_recovery().map(|state| state.database_id) else {
    return LoadedConfigurationControls {
      statuses: ConfigurationFamily::ALL.into_iter().map(|family| (family, ConfigurationControlFamilyStatus::identityless())).collect(),
      runtime_lkg: None,
      lifecycle_lkg: None,
    };
  };

  let mut statuses = BTreeMap::new();
  let mut runtime_lkg = None;
  let mut lifecycle_lkg = None;
  for family in ConfigurationFamily::ALL {
    let (status, fallback) = load_family(engine, database_id, family);
    statuses.insert(family, status);
    match family {
      ConfigurationFamily::Runtime => runtime_lkg = fallback,
      ConfigurationFamily::Lifecycle => lifecycle_lkg = fallback,
    }
  }
  LoadedConfigurationControls { statuses, runtime_lkg, lifecycle_lkg }
}

fn load_family(
  engine: &StorageEngine,
  database_id: [u8; 16],
  family: ConfigurationFamily,
) -> (ConfigurationControlFamilyStatus, Option<ConfigFallback>) {
  let store = V3TransitionControlStore::new(engine);
  let mut status = ConfigurationControlFamilyStatus::available(database_id);
  let mut fallback = None;
  match store.load_mutable(lkg_kind(family), database_id, &[]) {
    Ok(Some(selected)) => match decode_lkg_fallback(engine, family, &selected.bytes) {
      Ok(decoded) => {
        status.lkg_sequence = Some(selected.sequence);
        status.lkg_activated_at_ms = Some(decoded.recorded_at_ms);
        status.redundancy_degraded |= selected.redundancy_degraded;
        fallback = Some(decoded);
      }
      Err(error) => status.record_error(family.name(), error),
    },
    Ok(None) => {}
    Err(error) => status.record_error(family.name(), error),
  }
  match store.load_mutable(diagnostics_kind(family), database_id, &[]) {
    Ok(Some(selected)) => {
      match decode_system_control(&selected.bytes, engine.hash_algo())
        .map_err(format_error)
        .and_then(|control| decode_config_diagnostics_body(control.body, engine.hash_algo()).map_err(format_error))
      {
        Ok(_) => {
          status.diagnostics_sequence = Some(selected.sequence);
          status.redundancy_degraded |= selected.redundancy_degraded;
        }
        Err(error) => status.record_error(family.name(), error),
      }
    }
    Ok(None) => {}
    Err(error) => status.record_error(family.name(), error),
  }
  (status, fallback)
}

fn decode_lkg_fallback(engine: &StorageEngine, family: ConfigurationFamily, bytes: &[u8]) -> EngineResult<ConfigFallback> {
  let control = decode_system_control(bytes, engine.hash_algo()).map_err(format_error)?;
  let body = decode_config_lkg_body(control.body, engine.hash_algo()).map_err(format_error)?;
  if body.configuration_kind != configuration_kind(family) {
    return Err(EngineError::InvalidInput("configuration LKG kind does not match requested family".to_string()));
  }
  let expected_fingerprint = policy_fingerprint(body.configuration_kind, body.configuration_schema, &body.canonical_config);
  if body.policy_fingerprint != expected_fingerprint {
    return Err(EngineError::InvalidInput("configuration LKG policy fingerprint mismatch".to_string()));
  }
  let canonical = decode_canonical_value(&body.canonical_config, CanonicalValueBounds::AUDIT_VALUE).map_err(format_error)?;
  if canonical_schema(&canonical) != Some(u64::from(body.configuration_schema)) {
    return Err(EngineError::InvalidInput("configuration LKG schema disagrees with canonical policy".to_string()));
  }
  let json =
    canonical_value_to_json(&body.canonical_config, CanonicalValueBounds::AUDIT_VALUE, MAX_CONFIG_DOCUMENT_BYTES).map_err(format_error)?;
  Ok(ConfigFallback {
    bytes: json,
    identity: format!("{}:sequence={}", lkg_kind(family).slug(), control.sequence),
    recorded_at_ms: body.activated_at_ms,
  })
}

pub fn publish_configuration_document(
  engine: &StorageEngine,
  family: ConfigurationFamily,
  bytes: &[u8],
  schema_version: u16,
  prospective: &ConfigurationAuthoritySnapshot,
) -> EngineResult<ConfigurationControlFamilyStatus> {
  if schema_version != 1 {
    return Err(EngineError::InvalidInput(format!("new {} configuration writes require schema_version 1", family.name())));
  }
  let configuration_kind = configuration_kind(family);
  let canonical = canonicalize_json(bytes, CanonicalValueBounds::AUDIT_VALUE).map_err(format_error)?;
  if canonical_schema(&decode_canonical_value(&canonical, CanonicalValueBounds::AUDIT_VALUE).map_err(format_error)?)
    != Some(u64::from(schema_version))
  {
    return Err(EngineError::InvalidInput("validated configuration schema disagrees with canonical document".to_string()));
  }
  let policy_fingerprint = policy_fingerprint(configuration_kind, schema_version, &canonical);
  let effective_policy_fingerprint = effective_policy_fingerprint(family, &prospective.active_properties);
  let detail = canonicalize_json(b"{}", CanonicalValueBounds::AUDIT_VALUE).map_err(format_error)?;
  let source_row_count =
    u16::try_from(prospective.active_properties.values().filter(|property| family_path(family, &property.path)).count())
      .map_err(|_| EngineError::InvalidInput("configuration source row count exceeds u16".to_string()))?;
  let database_id = engine.persistent_durability_recovery().map(|state| state.database_id);

  let record = DirectoryOps::new(engine).store_file_buffered(&RequestContext::system(), family.path(), bytes, Some("application/json"))?;
  let Some(database_id) = database_id else {
    return Ok(ConfigurationControlFamilyStatus::identityless());
  };

  let mut status = ConfigurationControlFamilyStatus::available(database_id);
  let source_namespace_root = match engine.head_hash() {
    Ok(root) => root,
    Err(error) => {
      status.record_error(family.name(), error);
      return Ok(status);
    }
  };
  let observed_at_ms = chrono::Utc::now().timestamp_millis();
  let store = V3TransitionControlStore::new(engine);
  let lkg_body = ConfigLKGBodyV1 {
    database_id,
    configuration_kind,
    configuration_schema: schema_version,
    activated_at_ms: observed_at_ms,
    source_namespace_root: source_namespace_root.clone(),
    source_file_content_hash: record.content_hash.clone(),
    policy_fingerprint,
    canonical_config: canonical,
  };
  match publish_lkg(&store, engine, family, database_id, &lkg_body) {
    Ok(selected) => {
      status.lkg_sequence = Some(selected.sequence);
      status.lkg_activated_at_ms = Some(observed_at_ms);
      status.redundancy_degraded |= selected.redundancy_degraded;
    }
    Err(error) => status.record_error(family.name(), error),
  }

  let diagnostics_body = ConfigDiagnosticsBodyV1 {
    database_id,
    configuration_kind,
    aggregate_state: 1,
    observed_at_ms,
    current_file_root: source_namespace_root,
    current_file_content_hash: record.content_hash,
    effective_policy_fingerprint,
    source_row_count,
    disabled_capability_count: 0,
    detail,
  };
  match publish_diagnostics(&store, engine, family, database_id, &diagnostics_body) {
    Ok(selected) => {
      status.diagnostics_sequence = Some(selected.sequence);
      status.redundancy_degraded |= selected.redundancy_degraded;
    }
    Err(error) => status.record_error(family.name(), error),
  }
  Ok(status)
}

fn publish_lkg(
  store: &V3TransitionControlStore<'_>,
  engine: &StorageEngine,
  family: ConfigurationFamily,
  database_id: [u8; 16],
  body: &ConfigLKGBodyV1,
) -> EngineResult<super::control_store::LoadedMutableControlV1> {
  let kind = lkg_kind(family);
  let sequence = next_sequence(store, kind, database_id)?;
  let bytes = encode_config_lkg_control(sequence, body, engine.hash_algo()).map_err(format_error)?;
  store.publish_mutable(kind, database_id, &[], &bytes)
}

fn publish_diagnostics(
  store: &V3TransitionControlStore<'_>,
  engine: &StorageEngine,
  family: ConfigurationFamily,
  database_id: [u8; 16],
  body: &ConfigDiagnosticsBodyV1,
) -> EngineResult<super::control_store::LoadedMutableControlV1> {
  let kind = diagnostics_kind(family);
  let sequence = next_sequence(store, kind, database_id)?;
  let bytes = encode_config_diagnostics_control(sequence, body, engine.hash_algo()).map_err(format_error)?;
  store.publish_mutable(kind, database_id, &[], &bytes)
}

fn next_sequence(store: &V3TransitionControlStore<'_>, kind: SystemControlKindV1, database_id: [u8; 16]) -> EngineResult<u64> {
  store.load_mutable(kind, database_id, &[])?.map_or(Ok(1), |selected| {
    selected.sequence.checked_add(1).ok_or_else(|| EngineError::InvalidInput("control sequence exhausted".to_string()))
  })
}

fn policy_fingerprint(kind: ConfigurationKindV1, schema: u16, canonical: &[u8]) -> [u8; 32] {
  let mut hasher = blake3::Hasher::new();
  hasher.update(POLICY_FINGERPRINT_DOMAIN);
  hasher.update(&(kind as u16).to_le_bytes());
  hasher.update(&schema.to_le_bytes());
  hasher.update(canonical);
  *hasher.finalize().as_bytes()
}

fn effective_policy_fingerprint(family: ConfigurationFamily, properties: &BTreeMap<String, ActiveConfigProperty>) -> [u8; 32] {
  let mut hasher = blake3::Hasher::new();
  hasher.update(EFFECTIVE_FINGERPRINT_DOMAIN);
  hasher.update(&(configuration_kind(family) as u16).to_le_bytes());
  for property in properties.values().filter(|property| family_path(family, &property.path)) {
    hasher.update(&property.id.to_le_bytes());
    hash_length_prefixed(&mut hasher, property.path.as_bytes());
    hasher.update(&[property.source.map_or(0, config_source_id)]);
    hash_config_value(&mut hasher, property.value.as_ref());
  }
  *hasher.finalize().as_bytes()
}

fn hash_config_value(hasher: &mut blake3::Hasher, value: Option<&ConfigValue>) {
  match value {
    None => {
      hasher.update(&[0]);
    }
    Some(ConfigValue::Unsigned(value)) => {
      hasher.update(&[1]);
      hasher.update(&value.to_le_bytes());
    }
    Some(ConfigValue::Boolean(value)) => {
      hasher.update(&[2, u8::from(*value)]);
    }
    Some(ConfigValue::OptionalBytes(value)) => {
      hasher.update(&[3, u8::from(value.is_some())]);
      hasher.update(&value.unwrap_or(0).to_le_bytes());
    }
    Some(ConfigValue::Path(value)) => {
      hasher.update(&[4]);
      hash_length_prefixed(hasher, &native_path_bytes(value));
    }
  }
}

fn hash_length_prefixed(hasher: &mut blake3::Hasher, value: &[u8]) {
  hasher.update(&(value.len() as u64).to_le_bytes());
  hasher.update(value);
}

#[cfg(unix)]
fn native_path_bytes(path: &std::path::Path) -> Vec<u8> {
  use std::os::unix::ffi::OsStrExt;
  path.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
fn native_path_bytes(path: &std::path::Path) -> Vec<u8> {
  use std::os::windows::ffi::OsStrExt;
  path.as_os_str().encode_wide().flat_map(u16::to_le_bytes).collect()
}

fn config_source_id(source: ConfigSource) -> u8 {
  match source {
    ConfigSource::Default => 1,
    ConfigSource::StoredRuntimeV1 => 2,
    ConfigSource::StoredLifecycleV0 => 3,
    ConfigSource::StoredLifecycleV1 => 4,
    ConfigSource::Environment => 5,
    ConfigSource::DeprecatedEnvironment => 6,
    ConfigSource::CommandLine => 7,
    ConfigSource::LastKnownGood => 8,
    ConfigSource::AppendHistory => 9,
  }
}

fn canonical_schema(value: &CanonicalConfigValueV1) -> Option<u64> {
  let CanonicalConfigValueV1::Map(values) = value else {
    return None;
  };
  match values.get("schema_version") {
    Some(CanonicalConfigValueV1::Signed(value)) if *value >= 0 => Some(*value as u64),
    Some(CanonicalConfigValueV1::Unsigned(value)) => Some(*value),
    _ => None,
  }
}

fn configuration_kind(family: ConfigurationFamily) -> ConfigurationKindV1 {
  match family {
    ConfigurationFamily::Runtime => ConfigurationKindV1::Runtime,
    ConfigurationFamily::Lifecycle => ConfigurationKindV1::Lifecycle,
  }
}

fn lkg_kind(family: ConfigurationFamily) -> SystemControlKindV1 {
  match family {
    ConfigurationFamily::Runtime => SystemControlKindV1::RuntimeLastKnownGood,
    ConfigurationFamily::Lifecycle => SystemControlKindV1::LifecycleLastKnownGood,
  }
}

fn diagnostics_kind(family: ConfigurationFamily) -> SystemControlKindV1 {
  match family {
    ConfigurationFamily::Runtime => SystemControlKindV1::RuntimeDiagnostics,
    ConfigurationFamily::Lifecycle => SystemControlKindV1::LifecycleDiagnostics,
  }
}

fn family_path(family: ConfigurationFamily, path: &str) -> bool {
  match family {
    ConfigurationFamily::Runtime => !path.starts_with("lifecycle."),
    ConfigurationFamily::Lifecycle => path.starts_with("lifecycle."),
  }
}

fn format_error(error: super::reader::FormatError) -> EngineError {
  EngineError::InvalidInput(format!("invalid configuration transition control: {error}"))
}
