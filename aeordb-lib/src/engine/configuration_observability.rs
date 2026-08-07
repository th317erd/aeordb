use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use base64::Engine as _;
use serde_json::{json, Map, Value};

use crate::engine::config_resolver::{ConfigDocumentStatus, ConfigIssue, ConfigResolution, ConfigSource, ConfigValue, ConfigurationFamily};
use crate::engine::configuration_authority::ConfigurationAuthoritySnapshot;
use crate::engine::v4::configuration_controls::{ConfigurationControlCapability, ConfigurationControlFamilyStatus};
use crate::engine::v4::contract_generated::{ConfigProperty, CONFIGURATION_PROPERTIES};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigurationVisibility {
  Root,
  Redacted,
}

pub fn configuration_envelope(
  snapshot: &ConfigurationAuthoritySnapshot,
  family: ConfigurationFamily,
  visibility: ConfigurationVisibility,
) -> Value {
  let desired_resolution = snapshot.desired.resolution.as_ref();
  let active = family_properties(family)
    .map(|property| {
      let active = snapshot.active_properties.get(property.path);
      (property.path.to_string(), active.and_then(|active| active.value.as_ref()))
    })
    .collect::<BTreeMap<_, _>>();
  let desired = family_properties(family)
    .map(|property| {
      let value =
        desired_resolution.and_then(|resolution| resolution.properties.get(property.path)).and_then(|property| property.value.as_ref());
      (property.path.to_string(), value)
    })
    .collect::<BTreeMap<_, _>>();
  let active_sources = family_properties(family)
    .map(|property| {
      let source = snapshot.active_properties.get(property.path).and_then(|active| active.source);
      (property.path.to_string(), source.map_or(Value::Null, |source| Value::String(source_name(source).to_string())))
    })
    .collect::<Map<_, _>>();
  let desired_sources = family_properties(family)
    .map(|property| {
      let source = desired_resolution.and_then(|resolution| resolution.properties.get(property.path)).and_then(|property| property.source);
      (property.path.to_string(), source.map_or(Value::Null, |source| Value::String(source_name(source).to_string())))
    })
    .collect::<Map<_, _>>();
  let pending_restart = pending_values(&snapshot.pending_restart, desired_resolution, family, visibility);
  let pending_convergence = pending_values(&snapshot.pending_convergence, desired_resolution, family, visibility);
  let issues = desired_resolution.map_or_else(Vec::new, |resolution| family_issues(resolution, family, visibility));
  let control = snapshot.control_status(family);
  let stored = desired_resolution
    .map(|resolution| match family {
      ConfigurationFamily::Runtime => &resolution.runtime_status,
      ConfigurationFamily::Lifecycle => &resolution.lifecycle_status,
    })
    .map_or_else(|| json!({"state": "unavailable"}), document_status);
  let effective_valid = active.values().all(|value| value.is_some()) && !issues.iter().any(|issue| issue["blocking"] == true);
  let desired_valid = desired.values().all(|value| value.is_some()) && !issues.iter().any(|issue| issue["blocking"] == true);
  let mut disabled_capabilities = desired_resolution.map_or_else(Vec::new, |resolution| disabled_owners(resolution, family));
  if control.capability == ConfigurationControlCapability::UnavailableNoDatabaseIdentity {
    disabled_capabilities.push("configuration_recovery_controls".to_string());
  }
  let convergence_errors = snapshot
    .convergence_errors
    .iter()
    .filter(|(path, _)| property(path).is_some_and(|property| family.contains(property)))
    .map(|(path, error)| {
      let value = if property_is_redacted(path, visibility) { redacted_value() } else { Value::String(error.clone()) };
      (path.clone(), value)
    })
    .collect::<Map<_, _>>();
  let degraded = matches!(
    desired_resolution.map(|resolution| match family {
      ConfigurationFamily::Runtime => &resolution.runtime_status,
      ConfigurationFamily::Lifecycle => &resolution.lifecycle_status,
    }),
    Some(ConfigDocumentStatus::Invalid { .. })
  ) || !issues.is_empty()
    || !disabled_capabilities.is_empty()
    || !convergence_errors.is_empty()
    || control.capability == ConfigurationControlCapability::Degraded
    || control.redundancy_degraded;
  let fallback_identities = desired_resolution.map_or_else(Vec::new, |resolution| {
    resolution.fallback_identities.iter().filter(|identity| identity.contains(family.name())).cloned().collect::<Vec<_>>()
  });
  let deprecated_aliases = desired_resolution.map_or_else(Vec::new, |resolution| {
    resolution
      .deprecated_aliases
      .iter()
      .filter(|alias| {
        family_properties(family)
          .any(|property| crate::engine::config_resolver::legacy_environment_alias(property.path) == Some(alias.as_str()))
      })
      .cloned()
      .collect::<Vec<_>>()
  });
  let gc_mode = if family == ConfigurationFamily::Lifecycle {
    Some(if disabled_capabilities.iter().any(|capability| capability == "gc_runtime") { "disabled_configuration" } else { "enabled" })
  } else {
    None
  };

  let mut status = json!({
    "generation": snapshot.generation,
    "valid": effective_valid,
    "effective_valid": effective_valid,
    "desired_valid": desired_valid,
    "degraded": degraded,
    "stored": stored,
    "sources": active_sources,
    "desired_config": configuration_document(family, &desired, visibility),
    "desired_sources": desired_sources,
    "pending_restart": pending_restart,
    "pending_convergence": pending_convergence,
    "convergence_errors": convergence_errors,
    "control": control_status(control, family, visibility),
    "fallback_identities": fallback_identities,
    "deprecated_aliases": deprecated_aliases,
    "disabled_capabilities": disabled_capabilities,
    "issues": issues,
  });
  if let Some(gc_mode) = gc_mode {
    status.as_object_mut().expect("status is an object").insert("gc_mode".to_string(), Value::String(gc_mode.to_string()));
  }
  json!({
    "config": configuration_document(family, &active, visibility),
    "invariants": match family {
      ConfigurationFamily::Runtime => json!({}),
      ConfigurationFamily::Lifecycle => json!({"required_complete_marks": 2}),
    },
    "status": status,
  })
}

fn configuration_document(
  family: ConfigurationFamily,
  values: &BTreeMap<String, Option<&ConfigValue>>,
  visibility: ConfigurationVisibility,
) -> Value {
  match family {
    ConfigurationFamily::Runtime => {
      let mut root = Map::new();
      root.insert("schema_version".to_string(), Value::from(1));
      for property in family_properties(family) {
        let (group, field) = property.path.split_once('.').expect("runtime property has group and field");
        let group = root.entry(group.to_string()).or_insert_with(|| Value::Object(Map::new()));
        let value = visible_config_value(property, values.get(property.path).and_then(|value| *value), visibility);
        group.as_object_mut().expect("runtime group is an object").insert(field.to_string(), value);
      }
      Value::Object(root)
    }
    ConfigurationFamily::Lifecycle => json!({
      "schema_version": 1,
      "snapshot_writes_enabled": value_at(values, "lifecycle.snapshot_writes_enabled", visibility),
      "snapshot_retention": {
        "auto_months": value_at(values, "lifecycle.snapshot_retention_auto_months", visibility),
        "manual_months": value_at(values, "lifecycle.snapshot_retention_manual_months", visibility),
      },
      "garbage_collection": {
        "pending_delete_grace_seconds": value_at(
          values,
          "lifecycle.garbage_collection_pending_delete_grace_seconds",
          visibility,
        ),
      },
    }),
  }
}

fn value_at(values: &BTreeMap<String, Option<&ConfigValue>>, path: &str, visibility: ConfigurationVisibility) -> Value {
  let property = property(path).expect("configuration document path is registered");
  visible_config_value(property, values.get(path).and_then(|value| *value), visibility)
}

fn visible_config_value(property: &ConfigProperty, value: Option<&ConfigValue>, visibility: ConfigurationVisibility) -> Value {
  if property.redaction == Some("root_only_path") && visibility == ConfigurationVisibility::Redacted {
    return redacted_value();
  }
  value.map_or(Value::Null, config_value)
}

fn config_value(value: &ConfigValue) -> Value {
  match value {
    ConfigValue::Unsigned(value) => Value::from(*value),
    ConfigValue::Boolean(value) => Value::from(*value),
    ConfigValue::OptionalBytes(value) => value.map_or(Value::Null, Value::from),
    ConfigValue::Path(value) => path_value(value),
  }
}

fn path_value(path: &Path) -> Value {
  if let Some(path) = path.to_str() {
    return Value::String(path.to_string());
  }
  #[cfg(unix)]
  {
    use std::os::unix::ffi::OsStrExt;
    return json!({
      "encoding": "unix_bytes_base64",
      "bytes": base64::engine::general_purpose::STANDARD_NO_PAD.encode(path.as_os_str().as_bytes()),
    });
  }
  #[cfg(windows)]
  {
    use std::os::windows::ffi::OsStrExt;
    let bytes = path.as_os_str().encode_wide().flat_map(u16::to_le_bytes).collect::<Vec<_>>();
    return json!({
      "encoding": "windows_utf16le_base64",
      "bytes": base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes),
    });
  }
  #[allow(unreachable_code)]
  Value::String(path.to_string_lossy().into_owned())
}

fn pending_values(
  pending: &BTreeSet<String>,
  resolution: Option<&ConfigResolution>,
  family: ConfigurationFamily,
  visibility: ConfigurationVisibility,
) -> Value {
  let values = pending
    .iter()
    .filter(|path| property(path).is_some_and(|property| family.contains(property)))
    .map(|path| {
      let value = resolution.and_then(|resolution| resolution.properties.get(path)).and_then(|property| property.value.as_ref());
      let property = property(path).expect("pending configuration path is registered");
      (path.clone(), visible_config_value(property, value, visibility))
    })
    .collect::<Map<_, _>>();
  Value::Object(values)
}

fn family_issues(resolution: &ConfigResolution, family: ConfigurationFamily, visibility: ConfigurationVisibility) -> Vec<Value> {
  resolution
    .issues
    .iter()
    .filter(|issue| issue_belongs_to_family(issue, family))
    .map(|issue| {
      let message = if issue.property.as_deref().is_some_and(|path| property_is_redacted(path, visibility)) {
        Value::String("<redacted>".to_string())
      } else {
        Value::String(issue.message.clone())
      };
      json!({
        "property": issue.property,
        "source": issue.source.map(source_name),
        "blocking": issue.blocking,
        "message": message,
      })
    })
    .collect()
}

fn issue_belongs_to_family(issue: &ConfigIssue, family: ConfigurationFamily) -> bool {
  issue.property.as_deref().and_then(property).is_some_and(|property| family.contains(property))
    || (issue.property.is_none() && issue.message.starts_with(family.name()))
}

fn disabled_owners(resolution: &ConfigResolution, family: ConfigurationFamily) -> Vec<String> {
  let owners = family_properties(family).map(|property| property.owner).collect::<BTreeSet<_>>();
  owners
    .into_iter()
    .filter(|owner| {
      !resolution.owner_ready(owner) || crate::engine::configuration_authority::unavailable_configuration_owner_reason(owner).is_some()
    })
    .map(str::to_string)
    .collect()
}

fn document_status(status: &ConfigDocumentStatus) -> Value {
  match status {
    ConfigDocumentStatus::Missing => json!({"state": "missing"}),
    ConfigDocumentStatus::Valid { schema_version } => json!({"state": "valid", "schema_version": schema_version}),
    ConfigDocumentStatus::Invalid { message } => json!({"state": "invalid", "message": message}),
  }
}

fn control_status(status: &ConfigurationControlFamilyStatus, family: ConfigurationFamily, visibility: ConfigurationVisibility) -> Value {
  let database_id = status.database_id.map(hex::encode);
  let lkg = status.lkg_sequence.map(|sequence| {
    let activated_at_ms = status.lkg_activated_at_ms;
    json!({
      "identity": format!("{}-config-lkg:sequence={sequence}", family.name()),
      "sequence": sequence,
      "activated_at_ms": activated_at_ms,
      "age_ms": activated_at_ms.map(|activated| chrono::Utc::now().timestamp_millis().saturating_sub(activated)),
    })
  });
  let errors = if visibility == ConfigurationVisibility::Root || status.errors.is_empty() {
    status.errors.clone()
  } else {
    vec!["<redacted>".to_string()]
  };
  json!({
    "capability": match status.capability {
      ConfigurationControlCapability::UnavailableNoDatabaseIdentity => "unavailable_no_database_identity",
      ConfigurationControlCapability::Available => "available",
      ConfigurationControlCapability::Degraded => "degraded",
    },
    "database_id": database_id,
    "lkg": lkg,
    "diagnostics_sequence": status.diagnostics_sequence,
    "redundancy_degraded": status.redundancy_degraded,
    "errors": errors,
  })
}

fn source_name(source: ConfigSource) -> &'static str {
  match source {
    ConfigSource::Default => "default",
    ConfigSource::StoredRuntimeV1 => "stored_runtime_v1",
    ConfigSource::StoredLifecycleV0 => "stored_lifecycle_v0",
    ConfigSource::StoredLifecycleV1 => "stored_lifecycle_v1",
    ConfigSource::Environment => "environment",
    ConfigSource::DeprecatedEnvironment => "deprecated_environment",
    ConfigSource::CommandLine => "command_line",
    ConfigSource::LastKnownGood => "last_known_good",
    ConfigSource::AppendHistory => "append_history",
  }
}

fn family_properties(family: ConfigurationFamily) -> impl Iterator<Item = &'static ConfigProperty> {
  CONFIGURATION_PROPERTIES.iter().filter(move |property| family.contains(property))
}

fn property(path: &str) -> Option<&'static ConfigProperty> {
  CONFIGURATION_PROPERTIES.iter().find(|property| property.path == path)
}

fn property_is_redacted(path: &str, visibility: ConfigurationVisibility) -> bool {
  visibility == ConfigurationVisibility::Redacted && property(path).is_some_and(|property| property.redaction == Some("root_only_path"))
}

fn redacted_value() -> Value {
  json!({"redacted": true})
}
