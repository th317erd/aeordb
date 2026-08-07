//! Strict per-property operational configuration resolution.
//!
//! P2b initially uses this resolver in diagnostic shadow mode. Live owners keep
//! their existing behavior until their activation landing unit explicitly
//! adopts resolved values.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

use crate::engine::directory_ops::file_path_hash;
use crate::engine::entry_type::EntryType;
use crate::engine::file_record::FileRecord;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::v4::contract_generated::{CONFIGURATION_PROPERTIES, ConfigProperty};

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;
const TIB: u64 = 1024 * GIB;
const MAX_CONFIG_FILE_RECORD_BYTES: u32 = 64 * 1024;

pub const MAX_CONFIG_DOCUMENT_BYTES: usize = 1024 * 1024;
pub const RUNTIME_CONFIG_PATH: &str = "/.aeordb-config/runtime.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigValue {
  Unsigned(u64),
  Boolean(bool),
  OptionalBytes(Option<u64>),
  Path(PathBuf),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigSource {
  Default,
  StoredRuntimeV1,
  StoredLifecycleV0,
  StoredLifecycleV1,
  Environment,
  DeprecatedEnvironment,
  CommandLine,
  LastKnownGood,
  AppendHistory,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ConfigDocumentInput {
  #[default]
  Missing,
  Bytes(Vec<u8>),
  Unreadable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigFallback {
  pub bytes: Vec<u8>,
  pub identity: String,
  pub recorded_at_ms: i64,
}

#[derive(Clone, Debug)]
pub struct ConfigResolutionInputs {
  pub runtime: ConfigDocumentInput,
  pub lifecycle: ConfigDocumentInput,
  pub runtime_lkg: Option<ConfigFallback>,
  pub lifecycle_lkg: Option<ConfigFallback>,
  pub runtime_history: Vec<ConfigFallback>,
  pub lifecycle_history: Vec<ConfigFallback>,
  pub environment: BTreeMap<String, OsString>,
  pub cli: BTreeMap<String, OsString>,
}

impl Default for ConfigResolutionInputs {
  fn default() -> Self {
    Self {
      runtime: ConfigDocumentInput::Missing,
      lifecycle: ConfigDocumentInput::Missing,
      runtime_lkg: None,
      lifecycle_lkg: None,
      runtime_history: Vec::new(),
      lifecycle_history: Vec::new(),
      environment: BTreeMap::new(),
      cli: BTreeMap::new(),
    }
  }
}

#[derive(Clone, Debug)]
pub struct ConfigResolutionContext {
  pub physical_memory_bytes: u64,
  pub logical_cpu_count: u64,
  pub filesystem_capacity_bytes: u64,
  pub chunk_size_bytes: u64,
  pub database_path: PathBuf,
  pub default_gc_workspace_root: Option<PathBuf>,
  pub default_emergency_spill_dir: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct ConfigShadowReport {
  pub context: Option<ConfigResolutionContext>,
  pub resolution: Option<ConfigResolution>,
  pub context_error: Option<String>,
}

impl ConfigShadowReport {
  pub fn complete(&self) -> bool {
    self.resolution.as_ref().is_some_and(ConfigResolution::complete)
  }

  pub fn degraded(&self) -> bool {
    self.context_error.is_some() || self.resolution.as_ref().is_none_or(ConfigResolution::degraded)
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigDocumentStatus {
  Missing,
  Valid { schema_version: u64 },
  Invalid { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedConfigProperty {
  pub id: u16,
  pub path: String,
  pub owner: String,
  pub value: Option<ConfigValue>,
  pub source: Option<ConfigSource>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigIssue {
  pub property: Option<String>,
  pub source: Option<ConfigSource>,
  pub blocking: bool,
  pub message: String,
}

#[derive(Clone, Debug)]
pub struct ConfigResolution {
  pub properties: BTreeMap<String, ResolvedConfigProperty>,
  pub runtime_status: ConfigDocumentStatus,
  pub lifecycle_status: ConfigDocumentStatus,
  pub issues: Vec<ConfigIssue>,
  pub deprecated_aliases: Vec<String>,
  pub fallback_identities: Vec<String>,
}

impl ConfigResolution {
  pub fn property(&self, path: &str) -> Option<&ResolvedConfigProperty> {
    self.properties.get(path)
  }

  pub fn complete(&self) -> bool {
    self.properties.values().all(|property| property.value.is_some()) && !self.issues.iter().any(|issue| issue.blocking)
  }

  pub fn degraded(&self) -> bool {
    !self.issues.is_empty()
  }

  pub fn owner_ready(&self, owner: &str) -> bool {
    let owned = self.properties.values().filter(|property| property.owner == owner).collect::<Vec<_>>();
    !owned.is_empty()
      && owned.iter().all(|property| property.value.is_some())
      && !self.issues.iter().any(|issue| {
        issue.blocking
          && issue.property.as_deref().and_then(|path| self.properties.get(path)).is_some_and(|property| property.owner == owner)
      })
  }
}

pub struct ConfigResolver {
  context: ConfigResolutionContext,
}

impl ConfigResolver {
  pub fn new(context: ConfigResolutionContext) -> Self {
    Self { context }
  }

  pub fn resolve(&self, mut inputs: ConfigResolutionInputs) -> ConfigResolution {
    let mut resolution = ConfigResolution {
      properties: CONFIGURATION_PROPERTIES
        .iter()
        .map(|property| {
          (
            property.path.to_string(),
            ResolvedConfigProperty {
              id: property.id,
              path: property.path.to_string(),
              owner: property.owner.to_string(),
              value: None,
              source: None,
            },
          )
        })
        .collect(),
      runtime_status: ConfigDocumentStatus::Missing,
      lifecycle_status: ConfigDocumentStatus::Missing,
      issues: Vec::new(),
      deprecated_aliases: Vec::new(),
      fallback_identities: Vec::new(),
    };

    self.resolve_family(
      ConfigurationFamily::Runtime,
      &inputs.runtime,
      inputs.runtime_lkg.as_ref(),
      &mut inputs.runtime_history,
      &mut resolution,
    );
    self.resolve_family(
      ConfigurationFamily::Lifecycle,
      &inputs.lifecycle,
      inputs.lifecycle_lkg.as_ref(),
      &mut inputs.lifecycle_history,
      &mut resolution,
    );
    self.apply_overrides(&inputs.environment, &inputs.cli, &mut resolution);
    self.refresh_derived_defaults(&mut resolution);

    for property in CONFIGURATION_PROPERTIES {
      if resolution.properties[property.path].value.is_none()
        && !resolution.issues.iter().any(|issue| issue.blocking && issue.property.as_deref() == Some(property.path))
      {
        resolution.issues.push(ConfigIssue {
          property: Some(property.path.to_string()),
          source: None,
          blocking: true,
          message: format!("{} has no valid configuration source", property.path),
        });
      }
    }
    self.validate_cross_property_constraints(&mut resolution);
    if self.context.physical_memory_bytes < 2 * GIB {
      resolution.issues.push(ConfigIssue {
        property: Some("memory.hard_limit_bytes".to_string()),
        source: None,
        blocking: true,
        message: "writable startup requires at least 2 GiB of detected physical memory".to_string(),
      });
    }
    resolution.deprecated_aliases.sort();
    resolution.deprecated_aliases.dedup();
    resolution
  }

  pub fn validate_document(&self, family: ConfigurationFamily, bytes: &[u8]) -> Result<u64, String> {
    if bytes.len() > MAX_CONFIG_DOCUMENT_BYTES {
      return Err(format!("configuration document length {} exceeds {} bytes", bytes.len(), MAX_CONFIG_DOCUMENT_BYTES));
    }
    self.parse_document(family, bytes).map(|layer| layer.schema_version)
  }

  fn resolve_family(
    &self,
    family: ConfigurationFamily,
    current: &ConfigDocumentInput,
    lkg: Option<&ConfigFallback>,
    history: &mut [ConfigFallback],
    resolution: &mut ConfigResolution,
  ) {
    match current {
      ConfigDocumentInput::Missing => {
        self.set_document_status(family, ConfigDocumentStatus::Missing, resolution);
        self.apply_default_layer(family, ConfigSource::Default, resolution);
      }
      ConfigDocumentInput::Unreadable(message) => {
        self.set_document_status(family, ConfigDocumentStatus::Invalid { message: message.clone() }, resolution);
        resolution.issues.push(ConfigIssue {
          property: None,
          source: None,
          blocking: false,
          message: format!("{} configuration is unreadable: {message}", family.name()),
        });
        self.apply_fallback_layer(family, lkg, history, resolution);
      }
      ConfigDocumentInput::Bytes(bytes) => match self.parse_document(family, bytes) {
        Ok(layer) => {
          self.set_document_status(family, ConfigDocumentStatus::Valid { schema_version: layer.schema_version }, resolution);
          self.apply_current_layer(family, layer, resolution);
        }
        Err(message) => {
          self.set_document_status(family, ConfigDocumentStatus::Invalid { message: message.clone() }, resolution);
          resolution.issues.push(ConfigIssue {
            property: None,
            source: None,
            blocking: false,
            message: format!("{} configuration is invalid: {message}", family.name()),
          });
          self.apply_fallback_layer(family, lkg, history, resolution);
        }
      },
    }
  }

  fn set_document_status(&self, family: ConfigurationFamily, status: ConfigDocumentStatus, resolution: &mut ConfigResolution) {
    match family {
      ConfigurationFamily::Runtime => resolution.runtime_status = status,
      ConfigurationFamily::Lifecycle => resolution.lifecycle_status = status,
    }
  }

  fn apply_current_layer(&self, family: ConfigurationFamily, layer: ParsedLayer, resolution: &mut ConfigResolution) {
    let values = self
      .complete_family_values(family, &layer.values)
      .expect("a parsed configuration layer has already passed complete-value validation");
    for property in CONFIGURATION_PROPERTIES.iter().filter(|property| family.contains(property)) {
      let value = values.get(property.path).cloned();
      let source = Some(if layer.values.contains_key(property.path) { layer.source } else { ConfigSource::Default });
      let target = resolution.properties.get_mut(property.path).expect("frozen property exists");
      target.value = value;
      target.source = source;
    }
  }

  fn apply_default_layer(&self, family: ConfigurationFamily, source: ConfigSource, resolution: &mut ConfigResolution) {
    let mut values = BTreeMap::new();
    for property in CONFIGURATION_PROPERTIES.iter().filter(|property| family.contains(property)) {
      match self.default_value(property, &values) {
        Ok(value) => {
          values.insert(property.path.to_string(), value.clone());
          let target = resolution.properties.get_mut(property.path).expect("frozen property exists");
          target.value = Some(value);
          target.source = Some(source);
        }
        Err(message) => {
          resolution.issues.push(ConfigIssue { property: Some(property.path.to_string()), source: Some(source), blocking: true, message });
        }
      }
    }
  }

  fn apply_fallback_layer(
    &self,
    family: ConfigurationFamily,
    lkg: Option<&ConfigFallback>,
    history: &mut [ConfigFallback],
    resolution: &mut ConfigResolution,
  ) {
    if let Some(lkg) = lkg {
      match self.parse_document(family, &lkg.bytes) {
        Ok(layer) => {
          resolution.fallback_identities.push(lkg.identity.clone());
          self.apply_validated_fallback(family, layer, ConfigSource::LastKnownGood, resolution);
          return;
        }
        Err(message) => resolution.issues.push(ConfigIssue {
          property: None,
          source: Some(ConfigSource::LastKnownGood),
          blocking: false,
          message: format!("{} last-known-good {} is invalid: {message}", family.name(), lkg.identity),
        }),
      }
    }

    history.sort_by(|left, right| right.recorded_at_ms.cmp(&left.recorded_at_ms).then_with(|| right.identity.cmp(&left.identity)));
    for candidate in history {
      match self.parse_document(family, &candidate.bytes) {
        Ok(layer) => {
          resolution.fallback_identities.push(candidate.identity.clone());
          self.apply_validated_fallback(family, layer, ConfigSource::AppendHistory, resolution);
          return;
        }
        Err(message) => resolution.issues.push(ConfigIssue {
          property: None,
          source: Some(ConfigSource::AppendHistory),
          blocking: false,
          message: format!("{} history {} is invalid: {message}", family.name(), candidate.identity),
        }),
      }
    }
  }

  fn apply_validated_fallback(
    &self,
    family: ConfigurationFamily,
    layer: ParsedLayer,
    source: ConfigSource,
    resolution: &mut ConfigResolution,
  ) {
    let values =
      self.complete_family_values(family, &layer.values).expect("a parsed fallback layer has already passed complete-value validation");
    for property in CONFIGURATION_PROPERTIES.iter().filter(|property| family.contains(property)) {
      let value = values.get(property.path).cloned();
      let resolved_source = Some(if layer.values.contains_key(property.path) { source } else { ConfigSource::Default });
      let target = resolution.properties.get_mut(property.path).expect("frozen property exists");
      target.value = value;
      target.source = resolved_source;
    }
  }

  fn apply_overrides(&self, environment: &BTreeMap<String, OsString>, cli: &BTreeMap<String, OsString>, resolution: &mut ConfigResolution) {
    for property in CONFIGURATION_PROPERTIES {
      let environment_value = self.environment_override(property, environment, resolution);
      let cli_value = cli.get(property.cli).map(|raw| self.parse_override_value(property, raw, ConfigSource::CommandLine));

      match cli_value {
        Some(Ok(value)) => {
          if let Some(Err(message)) = environment_value {
            resolution.issues.push(ConfigIssue {
              property: Some(property.path.to_string()),
              source: Some(ConfigSource::Environment),
              blocking: false,
              message,
            });
          }
          Self::mark_lower_issues_superseded(property.path, &mut resolution.issues);
          let target = resolution.properties.get_mut(property.path).expect("frozen property exists");
          target.value = Some(value);
          target.source = Some(ConfigSource::CommandLine);
        }
        Some(Err(message)) => {
          let target = resolution.properties.get_mut(property.path).expect("frozen property exists");
          target.value = None;
          target.source = None;
          resolution.issues.push(ConfigIssue {
            property: Some(property.path.to_string()),
            source: Some(ConfigSource::CommandLine),
            blocking: true,
            message,
          });
        }
        None => match environment_value {
          Some(Ok((value, source))) => {
            Self::mark_lower_issues_superseded(property.path, &mut resolution.issues);
            let target = resolution.properties.get_mut(property.path).expect("frozen property exists");
            target.value = Some(value);
            target.source = Some(source);
          }
          Some(Err(message)) => {
            let target = resolution.properties.get_mut(property.path).expect("frozen property exists");
            target.value = None;
            target.source = None;
            resolution.issues.push(ConfigIssue {
              property: Some(property.path.to_string()),
              source: Some(ConfigSource::Environment),
              blocking: true,
              message,
            });
          }
          None => {}
        },
      }
    }
  }

  fn mark_lower_issues_superseded(path: &str, issues: &mut [ConfigIssue]) {
    for issue in issues.iter_mut().filter(|issue| issue.blocking && issue.property.as_deref() == Some(path)) {
      issue.blocking = false;
    }
  }

  fn refresh_derived_defaults(&self, resolution: &mut ConfigResolution) {
    let mut values = resolution
      .properties
      .iter()
      .filter_map(|(path, property)| property.value.clone().map(|value| (path.clone(), value)))
      .collect::<BTreeMap<_, _>>();
    for property in CONFIGURATION_PROPERTIES {
      let target = resolution.properties.get(property.path).expect("frozen property exists");
      if target.source != Some(ConfigSource::Default) {
        continue;
      }
      match self.default_value(property, &values) {
        Ok(value) => {
          values.insert(property.path.to_string(), value.clone());
          resolution.properties.get_mut(property.path).expect("frozen property exists").value = Some(value);
        }
        Err(message) => {
          resolution.properties.get_mut(property.path).expect("frozen property exists").value = None;
          resolution.issues.push(ConfigIssue {
            property: Some(property.path.to_string()),
            source: Some(ConfigSource::Default),
            blocking: true,
            message,
          });
        }
      }
    }
  }

  fn environment_override(
    &self,
    property: &ConfigProperty,
    environment: &BTreeMap<String, OsString>,
    resolution: &mut ConfigResolution,
  ) -> Option<Result<(ConfigValue, ConfigSource), String>> {
    let current = environment.get(property.environment);
    let legacy_name = legacy_environment_alias(property.path);
    let legacy = legacy_name.and_then(|name| environment.get(name).map(|value| (name, value)));
    if current.is_some() && legacy.is_some() {
      return Some(Err(format!(
        "{} and deprecated {} are both present for {}",
        property.environment,
        legacy_name.expect("legacy exists"),
        property.path
      )));
    }
    if let Some(value) = current {
      return Some(self.parse_override_value(property, value, ConfigSource::Environment).map(|value| (value, ConfigSource::Environment)));
    }
    legacy.map(|(name, value)| {
      resolution.deprecated_aliases.push(name.to_string());
      self
        .parse_override_value(property, value, ConfigSource::DeprecatedEnvironment)
        .map(|value| (value, ConfigSource::DeprecatedEnvironment))
    })
  }

  fn parse_override_value(&self, property: &ConfigProperty, raw: &OsStr, source: ConfigSource) -> Result<ConfigValue, String> {
    let value = match property.kind {
      "path_or_auto" => self.parse_path(property, raw)?,
      "boolean" => {
        let text = raw.to_str().ok_or_else(|| format!("{} {source:?} value is not UTF-8", property.path))?;
        match text {
          "true" => ConfigValue::Boolean(true),
          "false" => ConfigValue::Boolean(false),
          _ => return Err(format!("{} requires exactly true or false", property.path)),
        }
      }
      "optional_bytes" => {
        let text = raw.to_str().ok_or_else(|| format!("{} {source:?} value is not UTF-8", property.path))?;
        if text == "null" {
          ConfigValue::OptionalBytes(None)
        } else {
          ConfigValue::OptionalBytes(Some(parse_quantity(text, true)?))
        }
      }
      "bytes" => {
        let text = raw.to_str().ok_or_else(|| format!("{} {source:?} value is not UTF-8", property.path))?;
        ConfigValue::Unsigned(parse_quantity(text, true)?)
      }
      "seconds" | "milliseconds" | "months" | "count" => {
        let text = raw.to_str().ok_or_else(|| format!("{} {source:?} value is not UTF-8", property.path))?;
        ConfigValue::Unsigned(parse_quantity(text, false)?)
      }
      kind => return Err(format!("{} uses unsupported configuration kind {kind}", property.path)),
    };
    self.validate_individual(property, &value)?;
    Ok(value)
  }

  fn parse_document(&self, family: ConfigurationFamily, bytes: &[u8]) -> Result<ParsedLayer, String> {
    let value = parse_strict_json(bytes)?;
    let layer = match family {
      ConfigurationFamily::Runtime => self.parse_runtime_document(value),
      ConfigurationFamily::Lifecycle => self.parse_lifecycle_document(value),
    }?;
    self.validate_document_layer(family, &layer)?;
    Ok(layer)
  }

  fn validate_document_layer(&self, family: ConfigurationFamily, layer: &ParsedLayer) -> Result<(), String> {
    let values = self.complete_family_values(family, &layer.values)?;
    let failures = self.cross_property_failures(&values);
    if failures.is_empty() {
      Ok(())
    } else {
      Err(failures.into_iter().map(|(path, message)| format!("{path}: {message}")).collect::<Vec<_>>().join("; "))
    }
  }

  fn parse_runtime_document(&self, value: StrictValue) -> Result<ParsedLayer, String> {
    let mut root = into_object(value, "runtime root")?;
    let schema_version = take_u64(&mut root, "schema_version")?.ok_or_else(|| "runtime schema_version is required".to_string())?;
    if schema_version != 1 {
      return Err(format!("unsupported runtime schema_version {schema_version}"));
    }
    let mut values = BTreeMap::new();
    for (group, value) in root {
      let prefix = format!("{group}.");
      if !CONFIGURATION_PROPERTIES
        .iter()
        .any(|property| !ConfigurationFamily::Lifecycle.contains(property) && property.path.starts_with(&prefix))
      {
        return Err(format!("unknown runtime configuration group {group}"));
      }
      let fields = into_object(value, &format!("runtime.{group}"))?;
      for (field, value) in fields {
        let path = format!("{group}.{field}");
        let property = property_by_path(&path)
          .filter(|property| !ConfigurationFamily::Lifecycle.contains(property))
          .ok_or_else(|| format!("unknown runtime property {path}"))?;
        let parsed = self.parse_stored_value(property, value)?;
        values.insert(path, parsed);
      }
    }
    Ok(ParsedLayer { schema_version, source: ConfigSource::StoredRuntimeV1, values })
  }

  fn parse_lifecycle_document(&self, value: StrictValue) -> Result<ParsedLayer, String> {
    let mut root = into_object(value, "lifecycle root")?;
    let schema_version = take_u64(&mut root, "schema_version")?.unwrap_or(0);
    if schema_version > 1 {
      return Err(format!("unsupported lifecycle schema_version {schema_version}"));
    }
    let mut values = BTreeMap::new();
    if let Some(value) = root.remove("snapshot_writes_enabled") {
      self.insert_lifecycle_value(&mut values, "lifecycle.snapshot_writes_enabled", value)?;
    }
    if let Some(value) = root.remove("snapshot_retention") {
      let mut retention = into_object(value, "lifecycle.snapshot_retention")?;
      if let Some(value) = retention.remove("auto_months") {
        self.insert_lifecycle_value(&mut values, "lifecycle.snapshot_retention_auto_months", value)?;
      }
      if let Some(value) = retention.remove("manual_months") {
        self.insert_lifecycle_value(&mut values, "lifecycle.snapshot_retention_manual_months", value)?;
      }
      reject_unknown(retention, "lifecycle.snapshot_retention")?;
    }
    if let Some(value) = root.remove("garbage_collection") {
      if schema_version == 0 {
        return Err("legacy lifecycle v0 does not contain garbage_collection".to_string());
      }
      let mut gc = into_object(value, "lifecycle.garbage_collection")?;
      if let Some(value) = gc.remove("pending_delete_grace_seconds") {
        self.insert_lifecycle_value(&mut values, "lifecycle.garbage_collection_pending_delete_grace_seconds", value)?;
      }
      reject_unknown(gc, "lifecycle.garbage_collection")?;
    }
    reject_unknown(root, "lifecycle")?;
    Ok(ParsedLayer {
      schema_version,
      source: if schema_version == 0 { ConfigSource::StoredLifecycleV0 } else { ConfigSource::StoredLifecycleV1 },
      values,
    })
  }

  fn insert_lifecycle_value(&self, values: &mut BTreeMap<String, ConfigValue>, path: &str, value: StrictValue) -> Result<(), String> {
    let property = property_by_path(path).ok_or_else(|| format!("frozen lifecycle property {path} is missing"))?;
    values.insert(path.to_string(), self.parse_stored_value(property, value)?);
    Ok(())
  }

  fn parse_stored_value(&self, property: &ConfigProperty, value: StrictValue) -> Result<ConfigValue, String> {
    let value = match (property.kind, value) {
      ("boolean", StrictValue::Bool(value)) => ConfigValue::Boolean(value),
      ("optional_bytes", StrictValue::Null) => ConfigValue::OptionalBytes(None),
      ("optional_bytes", StrictValue::Number(StrictNumber::Unsigned(value))) => ConfigValue::OptionalBytes(Some(value)),
      ("path_or_auto", StrictValue::String(value)) => self.parse_path(property, OsStr::new(&value))?,
      ("bytes" | "seconds" | "milliseconds" | "months" | "count", StrictValue::Number(StrictNumber::Unsigned(value))) => {
        ConfigValue::Unsigned(value)
      }
      (_, StrictValue::Number(StrictNumber::Signed(value))) => return Err(format!("{} rejects signed value {value}", property.path)),
      (_, StrictValue::Number(StrictNumber::Float(value))) => return Err(format!("{} rejects non-integer value {value}", property.path)),
      (kind, value) => return Err(format!("{} expects {kind}, found {}", property.path, value.kind())),
    };
    self.validate_individual(property, &value)?;
    Ok(value)
  }

  fn parse_path(&self, property: &ConfigProperty, raw: &OsStr) -> Result<ConfigValue, String> {
    if raw == OsStr::new("auto") {
      return match property.id {
        20 => self
          .context
          .default_gc_workspace_root
          .as_deref()
          .ok_or_else(|| format!("{} automatic path is unavailable", property.path))
          .and_then(validate_absolute_path)
          .map(ConfigValue::Path),
        29 => self
          .context
          .default_emergency_spill_dir
          .as_deref()
          .ok_or_else(|| format!("{} OS user-data path is unavailable", property.path))
          .and_then(validate_absolute_path)
          .map(ConfigValue::Path),
        _ => Err(format!("{} does not define an automatic path", property.path)),
      };
    }
    validate_absolute_path(Path::new(raw)).map(ConfigValue::Path)
  }

  fn complete_family_values(
    &self,
    family: ConfigurationFamily,
    explicit: &BTreeMap<String, ConfigValue>,
  ) -> Result<BTreeMap<String, ConfigValue>, String> {
    let mut values = explicit.clone();
    for property in CONFIGURATION_PROPERTIES.iter().filter(|property| family.contains(property)) {
      if values.contains_key(property.path) {
        continue;
      }
      let value = self.default_value(property, &values)?;
      values.insert(property.path.to_string(), value);
    }
    Ok(values)
  }

  fn default_value(&self, property: &ConfigProperty, values: &BTreeMap<String, ConfigValue>) -> Result<ConfigValue, String> {
    let r = self.context.physical_memory_bytes;
    let h = values.get("memory.hard_limit_bytes").and_then(config_u64).unwrap_or_else(|| (r / 2).clamp(GIB, 8 * GIB));
    let value = match property.id {
      1 => ConfigValue::Unsigned((h * 3 / 4).max(768 * MIB)),
      2 => ConfigValue::Unsigned(h),
      3 => ConfigValue::Unsigned((r / 8).clamp(512 * MIB, 2 * GIB)),
      4 => ConfigValue::Unsigned((h / 8).min(256 * MIB)),
      5 => ConfigValue::Unsigned((h / 4).min(2 * GIB)),
      6 => ConfigValue::Unsigned(300),
      7 => ConfigValue::Unsigned((h / 16).min(512 * MIB)),
      8 => ConfigValue::Unsigned((h / 4).min(2 * GIB)),
      9 => ConfigValue::Unsigned((h / 32).min(256 * MIB)),
      10 => ConfigValue::Unsigned((h / 8).min(GIB)),
      11 => ConfigValue::Unsigned(262_144),
      12 => ConfigValue::Unsigned(30),
      13 => ConfigValue::Unsigned(256 * MIB),
      14 => ConfigValue::Unsigned(128 * MIB),
      15 => ConfigValue::Unsigned(64 * MIB),
      16 => ConfigValue::Unsigned((self.context.filesystem_capacity_bytes / 50).clamp(8 * GIB, 64 * GIB)),
      17 => ConfigValue::OptionalBytes(None),
      18 => ConfigValue::Unsigned(300),
      19 => ConfigValue::Unsigned(GIB),
      20 => self
        .context
        .default_gc_workspace_root
        .as_deref()
        .ok_or_else(|| format!("{} automatic path is unavailable", property.path))
        .and_then(validate_absolute_path)
        .map(ConfigValue::Path)?,
      21 => {
        ConfigValue::Unsigned(self.context.chunk_size_bytes.checked_mul(10).ok_or_else(|| "read prefetch default overflows".to_string())?)
      }
      22 => ConfigValue::Unsigned(16 * MIB),
      23 => ConfigValue::Unsigned((h / 64).min(128 * MIB)),
      24 => ConfigValue::Unsigned((h / 8).min(GIB)),
      25 => ConfigValue::Unsigned(8 * MIB),
      26 => ConfigValue::Unsigned(64 * MIB),
      27 => ConfigValue::Unsigned(100),
      28 => ConfigValue::Unsigned((self.context.logical_cpu_count / 4).clamp(1, 2)),
      29 => self
        .context
        .default_emergency_spill_dir
        .as_deref()
        .ok_or_else(|| format!("{} OS user-data path is unavailable", property.path))
        .and_then(validate_absolute_path)
        .map(ConfigValue::Path)?,
      30 => ConfigValue::Unsigned(4 * GIB),
      31 => ConfigValue::Unsigned(600),
      32 => ConfigValue::Unsigned(64 * GIB),
      33 => ConfigValue::Unsigned((self.context.filesystem_capacity_bytes / 20).clamp(16 * GIB, 128 * GIB)),
      34 => ConfigValue::Unsigned(300),
      35 => ConfigValue::Unsigned(2_592_000),
      36 => ConfigValue::Unsigned(256 * MIB),
      37 => ConfigValue::Unsigned(GIB),
      38 => ConfigValue::Boolean(true),
      39 | 40 => ConfigValue::Unsigned(0),
      41 => ConfigValue::Unsigned(86_400),
      id => return Err(format!("configuration property id {id} has no frozen default")),
    };
    self.validate_individual(property, &value)?;
    Ok(value)
  }

  fn validate_individual(&self, property: &ConfigProperty, value: &ConfigValue) -> Result<(), String> {
    if matches!(property.kind, "path_or_auto" | "boolean") {
      return Ok(());
    }
    if property.id == 17 && matches!(value, ConfigValue::OptionalBytes(None)) {
      return Ok(());
    }
    let Some(value) = config_u64(value) else {
      return Err(format!("{} requires an unsigned integer", property.path));
    };
    let range = match property.id {
      1 => Some((512 * MIB, u64::MAX)),
      2 => Some((GIB, (128 * GIB).min(self.context.physical_memory_bytes.saturating_sub(512 * MIB)))),
      3 => Some((256 * MIB, self.context.physical_memory_bytes / 2)),
      4 => Some((64 * MIB, u64::MAX)),
      5 | 7 | 9 => Some((0, u64::MAX)),
      6 => Some((1, 86_400)),
      8 => Some((64 * MIB, u64::MAX)),
      10 => Some((16 * MIB, u64::MAX)),
      11 => Some((1, 16_777_216)),
      12 => Some((1, 300)),
      13 => Some((MIB, GIB)),
      14 => Some((64 * MIB, u64::MAX)),
      15 => Some((32 * MIB, u64::MAX)),
      16 | 33 => Some((GIB, self.context.filesystem_capacity_bytes / 2)),
      17 => Some((GIB, u64::MAX)),
      18 | 34 => Some((30, 3_600)),
      19 => Some((64 * MIB, 64 * GIB)),
      21 => Some((self.context.chunk_size_bytes, 64 * MIB)),
      22 => Some((0, 256 * MIB)),
      23 => Some((8 * MIB, u64::MAX)),
      24 => Some((0, u64::MAX)),
      25 => Some((256 * KIB, 64 * MIB)),
      26 => Some((MIB, GIB)),
      27 => Some((0, 1_000)),
      28 => Some((1, 32)),
      30 => Some((64 * MIB, TIB)),
      31 => Some((0, 86_400)),
      32 => Some((GIB, 4 * TIB)),
      35 => Some((3_600, 315_576_000)),
      36 => Some((MIB, 16 * GIB)),
      37 => Some((64 * MIB, 64 * GIB)),
      39 | 40 => Some((0, u32::MAX as u64)),
      41 => Some((0, i64::MAX as u64 / 1_000)),
      _ => None,
    };
    if let Some((minimum, maximum)) = range {
      if !(minimum..=maximum).contains(&value) {
        return Err(format!("{} value {value} is outside {}", property.path, property.constraint));
      }
    }
    Ok(())
  }

  fn validate_cross_property_constraints(&self, resolution: &mut ConfigResolution) {
    let values = resolution
      .properties
      .iter()
      .filter_map(|(path, property)| property.value.clone().map(|value| (path.clone(), value)))
      .collect::<BTreeMap<_, _>>();
    for (path, message) in self.cross_property_failures(&values) {
      resolution.issues.push(ConfigIssue {
        property: Some(path.to_string()),
        source: resolution.property(path).and_then(|property| property.source),
        blocking: true,
        message: message.to_string(),
      });
    }
  }

  fn cross_property_failures(&self, values: &BTreeMap<String, ConfigValue>) -> Vec<(&'static str, &'static str)> {
    let value = |path: &str| values.get(path).and_then(config_u64);
    let mut failures = Vec::new();
    if let (Some(soft), Some(hard), Some(reserve)) =
      (value("memory.soft_limit_bytes"), value("memory.hard_limit_bytes"), value("memory.emergency_reserve_bytes"))
    {
      if soft > hard.saturating_sub(reserve) {
        failures
          .push(("memory.soft_limit_bytes", "memory.soft_limit_bytes must not exceed hard_limit_bytes minus emergency_reserve_bytes"));
      }
    }
    if let (Some(reserve), Some(hard)) = (value("memory.emergency_reserve_bytes"), value("memory.hard_limit_bytes")) {
      if reserve > hard / 4 {
        failures.push(("memory.emergency_reserve_bytes", "memory.emergency_reserve_bytes must not exceed one quarter of hard_limit_bytes"));
      }
    }
    for (path, divisor) in [
      ("cache.index_clean_max_bytes", 2),
      ("cache.directory_max_bytes", 4),
      ("cache.kv_resident_max_bytes", 2),
      ("cache.query_plan_max_bytes", 8),
      ("index.mutation_buffer_max_bytes", 4),
      ("garbage_collection.mark_memory_preferred_bytes", 4),
      ("query.per_request_memory_bytes", 8),
      ("query.global_memory_bytes", 4),
    ] {
      if let (Some(item), Some(hard)) = (value(path), value("memory.hard_limit_bytes")) {
        if item > hard / divisor {
          failures.push((path, "configured value exceeds its hard-memory share"));
        }
      }
    }
    if let (Some(minimum), Some(preferred)) =
      (value("garbage_collection.mark_memory_minimum_bytes"), value("garbage_collection.mark_memory_preferred_bytes"))
    {
      if minimum > preferred {
        failures.push(("garbage_collection.mark_memory_minimum_bytes", "GC minimum mark memory must not exceed preferred mark memory"));
      }
    }
    if let (Some(prefetch), Some(coalesce)) = (value("io.read_prefetch_bytes"), value("io.read_coalesce_max_bytes")) {
      if prefetch > coalesce {
        failures.push(("io.read_coalesce_max_bytes", "read coalesce maximum must be at least the prefetch size"));
      }
    }
    if let (Some(per_request), Some(global)) = (value("query.per_request_memory_bytes"), value("query.global_memory_bytes")) {
      if per_request > global {
        failures.push(("query.global_memory_bytes", "global query memory must be at least per-request query memory"));
      }
    }
    failures
  }
}

pub(crate) struct StartupConfigurationState {
  pub report: ConfigShadowReport,
  pub inputs: ConfigResolutionInputs,
}

pub(crate) fn build_startup_configuration(
  engine: &StorageEngine,
  database_path: &Path,
  chunk_size_bytes: u64,
) -> StartupConfigurationState {
  let context = match detect_context(database_path, chunk_size_bytes) {
    Ok(context) => context,
    Err(message) => {
      return StartupConfigurationState {
        report: ConfigShadowReport { context: None, resolution: None, context_error: Some(message) },
        inputs: ConfigResolutionInputs::default(),
      };
    }
  };
  let inputs = ConfigResolutionInputs {
    runtime: read_config_document(engine, RUNTIME_CONFIG_PATH),
    lifecycle: read_config_document(engine, crate::engine::lifecycle_config::LIFECYCLE_CONFIG_PATH),
    environment: collect_registered_environment(),
    ..Default::default()
  };
  let resolution = ConfigResolver::new(context.clone()).resolve(inputs.clone());
  StartupConfigurationState {
    report: ConfigShadowReport { context: Some(context), resolution: Some(resolution), context_error: None },
    inputs,
  }
}

fn detect_context(database_path: &Path, chunk_size_bytes: u64) -> Result<ConfigResolutionContext, String> {
  let database_path = std::fs::canonicalize(database_path)
    .map_err(|error| format!("cannot canonicalize database path {}: {error}", database_path.display()))?;
  let filesystem_capacity_bytes = fs2::total_space(&database_path)
    .map_err(|error| format!("cannot detect filesystem capacity for {}: {error}", database_path.display()))?;
  let logical_cpu_count =
    std::thread::available_parallelism().map_err(|error| format!("cannot detect logical CPU count: {error}"))?.get() as u64;
  let default_gc_workspace_root = private_gc_workspace_root(&database_path)?;
  Ok(ConfigResolutionContext {
    physical_memory_bytes: detect_physical_memory_bytes()?,
    logical_cpu_count,
    filesystem_capacity_bytes,
    chunk_size_bytes,
    database_path,
    default_gc_workspace_root: Some(default_gc_workspace_root),
    default_emergency_spill_dir: crate::engine::emergency_spill::os_user_data_emergency_spill_dir(),
  })
}

fn private_gc_workspace_root(database_path: &Path) -> Result<PathBuf, String> {
  let parent = database_path.parent().ok_or_else(|| format!("database path {} has no parent", database_path.display()))?;
  let file_name = database_path.file_name().ok_or_else(|| format!("database path {} has no file name", database_path.display()))?;
  let mut sibling_name = OsString::from(".");
  sibling_name.push(file_name);
  sibling_name.push("-gc");
  Ok(parent.join(sibling_name))
}

#[cfg(unix)]
fn detect_physical_memory_bytes() -> Result<u64, String> {
  let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
  let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
  if pages <= 0 || page_size <= 0 {
    return Err(format!("physical-memory sysconf returned pages={pages}, page_size={page_size}"));
  }
  (pages as u64).checked_mul(page_size as u64).ok_or_else(|| "detected physical-memory size overflows u64".to_string())
}

#[cfg(windows)]
fn detect_physical_memory_bytes() -> Result<u64, String> {
  use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

  let mut status = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
  if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
    return Err(format!("GlobalMemoryStatusEx failed: {}", std::io::Error::last_os_error()));
  }
  Ok(status.ullTotalPhys)
}

#[cfg(not(any(unix, windows)))]
fn detect_physical_memory_bytes() -> Result<u64, String> {
  Err("physical-memory detection is unsupported on this platform".to_string())
}

fn collect_registered_environment() -> BTreeMap<String, OsString> {
  let mut environment = BTreeMap::new();
  for property in CONFIGURATION_PROPERTIES {
    if let Some(value) = std::env::var_os(property.environment) {
      environment.insert(property.environment.to_string(), value);
    }
    if let Some(alias) = legacy_environment_alias(property.path) {
      if let Some(value) = std::env::var_os(alias) {
        environment.insert(alias.to_string(), value);
      }
    }
  }
  environment
}

fn read_config_document(engine: &StorageEngine, path: &str) -> ConfigDocumentInput {
  match read_config_document_inner(engine, path) {
    Ok(Some(bytes)) => ConfigDocumentInput::Bytes(bytes),
    Ok(None) => ConfigDocumentInput::Missing,
    Err(message) => ConfigDocumentInput::Unreadable(message),
  }
}

fn read_config_document_inner(engine: &StorageEngine, path: &str) -> Result<Option<Vec<u8>>, String> {
  let key = file_path_hash(path, &engine.hash_algo()).map_err(|error| error.to_string())?;
  let Some((header, _, value)) = engine
    .get_entry_verified_bounded(&key, MAX_CONFIG_FILE_RECORD_BYTES)
    .map_err(|error| format!("cannot read bounded FileRecord for {path}: {error}"))?
  else {
    return Ok(None);
  };
  if header.entry_type != EntryType::FileRecord {
    return Err(format!("{path} resolves to {:?}, not FileRecord", header.entry_type));
  }
  let record = FileRecord::deserialize(&value, engine.hash_algo().hash_length(), header.entry_version)
    .map_err(|error| format!("cannot decode FileRecord for {path}: {error}"))?;
  if record.path != path {
    return Err(format!("FileRecord path {} does not match requested configuration path {path}", record.path));
  }
  if record.total_size > MAX_CONFIG_DOCUMENT_BYTES as u64 {
    return Err(format!("configuration document length {} exceeds {} bytes", record.total_size, MAX_CONFIG_DOCUMENT_BYTES));
  }

  let mut bytes = Vec::with_capacity(record.total_size as usize);
  for chunk_hash in record.chunk_hashes {
    let remaining = (record.total_size as usize)
      .checked_sub(bytes.len())
      .ok_or_else(|| format!("configuration document {path} exceeded its declared length"))?;
    if remaining == 0 {
      return Err(format!("configuration document {path} contains chunks beyond its declared length"));
    }
    let chunk = engine
      .read_chunk_verified_bounded(&chunk_hash, remaining)
      .map_err(|error| format!("cannot read bounded configuration document {path}: {error}"))?
      .ok_or_else(|| format!("configuration document {path} references a missing chunk {}", hex::encode(chunk_hash)))?;
    bytes.extend_from_slice(&chunk);
  }
  if bytes.len() as u64 != record.total_size {
    return Err(format!("configuration document {path} expected {} bytes but read {}", record.total_size, bytes.len()));
  }
  Ok(Some(bytes))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigurationFamily {
  Runtime,
  Lifecycle,
}

impl ConfigurationFamily {
  pub fn contains(self, property: &ConfigProperty) -> bool {
    let lifecycle = property.path.starts_with("lifecycle.");
    match self {
      Self::Runtime => !lifecycle,
      Self::Lifecycle => lifecycle,
    }
  }

  pub fn name(self) -> &'static str {
    match self {
      Self::Runtime => "runtime",
      Self::Lifecycle => "lifecycle",
    }
  }

  pub fn path(self) -> &'static str {
    match self {
      Self::Runtime => RUNTIME_CONFIG_PATH,
      Self::Lifecycle => crate::engine::lifecycle_config::LIFECYCLE_CONFIG_PATH,
    }
  }
}

struct ParsedLayer {
  schema_version: u64,
  source: ConfigSource,
  values: BTreeMap<String, ConfigValue>,
}

#[derive(Clone, Debug)]
enum StrictValue {
  Null,
  Bool(bool),
  Number(StrictNumber),
  String(String),
  Array,
  Object(BTreeMap<String, StrictValue>),
}

impl StrictValue {
  fn kind(&self) -> &'static str {
    match self {
      Self::Null => "null",
      Self::Bool(_) => "boolean",
      Self::Number(_) => "number",
      Self::String(_) => "string",
      Self::Array => "array",
      Self::Object(_) => "object",
    }
  }
}

#[derive(Clone, Debug)]
enum StrictNumber {
  Unsigned(u64),
  Signed(i64),
  Float(f64),
}

impl<'de> Deserialize<'de> for StrictValue {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    deserializer.deserialize_any(StrictValueVisitor)
  }
}

struct StrictValueVisitor;

impl<'de> Visitor<'de> for StrictValueVisitor {
  type Value = StrictValue;

  fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str("a JSON value without duplicate object keys")
  }

  fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
    Ok(StrictValue::Null)
  }

  fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
    Ok(StrictValue::Null)
  }

  fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
    Ok(StrictValue::Bool(value))
  }

  fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
    Ok(StrictValue::Number(StrictNumber::Unsigned(value)))
  }

  fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
    Ok(StrictValue::Number(StrictNumber::Signed(value)))
  }

  fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
    Ok(StrictValue::Number(StrictNumber::Float(value)))
  }

  fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
    Ok(StrictValue::String(value.to_string()))
  }

  fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
    Ok(StrictValue::String(value))
  }

  fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
  where
    A: SeqAccess<'de>,
  {
    while sequence.next_element::<StrictValue>()?.is_some() {
      // Arrays are invalid for every v1 property, but consume them completely
      // so duplicate detection and trailing-input checks remain authoritative.
    }
    Ok(StrictValue::Array)
  }

  fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
  where
    A: MapAccess<'de>,
  {
    let mut values = BTreeMap::new();
    while let Some((key, value)) = map.next_entry::<String, StrictValue>()? {
      if values.insert(key.clone(), value).is_some() {
        return Err(de::Error::custom(format!("duplicate JSON property {key}")));
      }
    }
    Ok(StrictValue::Object(values))
  }
}

fn parse_strict_json(bytes: &[u8]) -> Result<StrictValue, String> {
  let mut deserializer = serde_json::Deserializer::from_slice(bytes);
  let value = StrictValue::deserialize(&mut deserializer).map_err(|error| error.to_string())?;
  deserializer.end().map_err(|error| error.to_string())?;
  Ok(value)
}

fn into_object(value: StrictValue, location: &str) -> Result<BTreeMap<String, StrictValue>, String> {
  match value {
    StrictValue::Object(value) => Ok(value),
    value => Err(format!("{location} must be an object, found {}", value.kind())),
  }
}

fn take_u64(root: &mut BTreeMap<String, StrictValue>, name: &str) -> Result<Option<u64>, String> {
  match root.remove(name) {
    None => Ok(None),
    Some(StrictValue::Number(StrictNumber::Unsigned(value))) => Ok(Some(value)),
    Some(value) => Err(format!("{name} must be an unsigned integer, found {}", value.kind())),
  }
}

fn reject_unknown(values: BTreeMap<String, StrictValue>, location: &str) -> Result<(), String> {
  if values.is_empty() {
    Ok(())
  } else {
    Err(format!("unknown {location} property {}", values.keys().next().expect("nonempty")))
  }
}

fn parse_quantity(value: &str, allow_binary_suffix: bool) -> Result<u64, String> {
  if value.is_empty() || value.starts_with('-') || value.starts_with('+') || value.contains('.') || value.chars().any(char::is_whitespace) {
    return Err(format!("invalid canonical quantity {value:?}"));
  }
  let (digits, multiplier) = if allow_binary_suffix {
    [("KiB", KIB), ("MiB", MIB), ("GiB", GIB), ("TiB", TIB)]
      .into_iter()
      .find_map(|(suffix, multiplier)| value.strip_suffix(suffix).map(|digits| (digits, multiplier)))
      .unwrap_or((value, 1))
  } else {
    (value, 1)
  };
  if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
    return Err(format!("invalid canonical quantity {value:?}"));
  }
  digits
    .parse::<u64>()
    .map_err(|_| format!("quantity {value:?} overflows u64"))?
    .checked_mul(multiplier)
    .ok_or_else(|| format!("quantity {value:?} overflows u64"))
}

fn validate_absolute_path(path: &Path) -> Result<PathBuf, String> {
  if !path.is_absolute() {
    return Err(format!("configuration path {} is not absolute", path.display()));
  }
  for component in path.components() {
    if matches!(component, Component::CurDir | Component::ParentDir) {
      return Err(format!("configuration path {} is not normalized", path.display()));
    }
  }
  Ok(path.components().collect())
}

fn property_by_path(path: &str) -> Option<&'static ConfigProperty> {
  CONFIGURATION_PROPERTIES.iter().find(|property| property.path == path)
}

fn config_u64(value: &ConfigValue) -> Option<u64> {
  match value {
    ConfigValue::Unsigned(value) | ConfigValue::OptionalBytes(Some(value)) => Some(*value),
    ConfigValue::Boolean(_) | ConfigValue::OptionalBytes(None) | ConfigValue::Path(_) => None,
  }
}

fn legacy_environment_alias(path: &str) -> Option<&'static str> {
  match path {
    "cache.index_clean_max_bytes" => Some("AEORDB_INDEX_CACHE_MAX_BYTES"),
    "cache.index_clean_ttl_seconds" => Some("AEORDB_INDEX_CACHE_CLEAN_TTL_SECS"),
    "recovery.emergency_spill_dir" => Some("AEORDB_EMERGENCY_SPILL_DIR"),
    "recovery.emergency_spill_max_bytes" => Some("AEORDB_EMERGENCY_WAL_SPILL_MAX_BYTES"),
    "shutdown.operation_wait_seconds" => Some("AEORDB_SHUTDOWN_OPERATION_WAIT_SECS"),
    _ => None,
  }
}
