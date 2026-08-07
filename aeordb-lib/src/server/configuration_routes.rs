use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header::CONTENT_TYPE, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::Extension;
use base64::Engine as _;
use serde_json::{json, Map, Value};

use crate::auth::TokenClaims;
use crate::engine::config_resolver::{ConfigDocumentStatus, ConfigIssue, ConfigResolution, ConfigSource, ConfigValue, ConfigurationFamily};
use crate::engine::configuration_authority::ConfigurationAuthoritySnapshot;
use crate::engine::v4::configuration_controls::{ConfigurationControlCapability, ConfigurationControlFamilyStatus};
use crate::engine::v4::contract_generated::{ConfigProperty, CONFIGURATION_PROPERTIES};
use crate::server::responses::{engine_error_response, require_root, ErrorResponse};
use crate::server::state::AppState;

pub async fn get_runtime(State(state): State<AppState>, Extension(claims): Extension<TokenClaims>) -> Response {
  get_configuration(state, claims, ConfigurationFamily::Runtime)
}

pub async fn put_runtime(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  headers: HeaderMap,
  body: Bytes,
) -> Response {
  if let Err(response) = require_root(&claims) {
    return response;
  }
  if let Err(response) = require_json_content_type(&headers, false) {
    return response;
  }
  put_configuration(state, ConfigurationFamily::Runtime, body)
}

pub async fn patch_runtime(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  headers: HeaderMap,
  body: Bytes,
) -> Response {
  if let Err(response) = require_root(&claims) {
    return response;
  }
  if let Err(response) = require_json_content_type(&headers, true) {
    return response;
  }
  patch_configuration(state, ConfigurationFamily::Runtime, body)
}

pub async fn get_lifecycle(State(state): State<AppState>, Extension(claims): Extension<TokenClaims>) -> Response {
  get_configuration(state, claims, ConfigurationFamily::Lifecycle)
}

pub async fn put_lifecycle(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  headers: HeaderMap,
  body: Bytes,
) -> Response {
  if let Err(response) = require_root(&claims) {
    return response;
  }
  if let Err(response) = require_json_content_type(&headers, false) {
    return response;
  }
  put_configuration(state, ConfigurationFamily::Lifecycle, body)
}

pub async fn patch_lifecycle(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  headers: HeaderMap,
  body: Bytes,
) -> Response {
  if let Err(response) = require_root(&claims) {
    return response;
  }
  if let Err(response) = require_json_content_type(&headers, true) {
    return response;
  }
  patch_configuration(state, ConfigurationFamily::Lifecycle, body)
}

fn require_json_content_type(headers: &HeaderMap, merge_patch: bool) -> Result<(), Response> {
  let media_type = headers.get(CONTENT_TYPE).and_then(|value| value.to_str().ok()).and_then(|value| value.split(';').next()).map(str::trim);
  let accepted = media_type.is_some_and(|media_type| {
    media_type.eq_ignore_ascii_case("application/json") || (merge_patch && media_type.eq_ignore_ascii_case("application/merge-patch+json"))
  });
  if accepted {
    return Ok(());
  }
  let expected = if merge_patch { "application/json or application/merge-patch+json" } else { "application/json" };
  Err(
    ErrorResponse::new(format!("configuration request Content-Type must be {expected}"))
      .with_status(StatusCode::UNSUPPORTED_MEDIA_TYPE)
      .into_response(),
  )
}

fn get_configuration(state: AppState, claims: TokenClaims, family: ConfigurationFamily) -> Response {
  if let Err(response) = require_root(&claims) {
    return response;
  }
  Json(configuration_envelope(&state.engine.configuration_snapshot(), family)).into_response()
}

fn put_configuration(state: AppState, family: ConfigurationFamily, body: Bytes) -> Response {
  match state.engine.replace_configuration_document(family, &body) {
    Ok(snapshot) => Json(configuration_envelope(&snapshot, family)).into_response(),
    Err(error) => engine_error_response(&format!("failed to replace {} configuration", family.name()), &error),
  }
}

fn patch_configuration(state: AppState, family: ConfigurationFamily, body: Bytes) -> Response {
  match state.engine.patch_configuration_document(family, &body) {
    Ok(snapshot) => Json(configuration_envelope(&snapshot, family)).into_response(),
    Err(error) => engine_error_response(&format!("failed to patch {} configuration", family.name()), &error),
  }
}

fn configuration_envelope(snapshot: &ConfigurationAuthoritySnapshot, family: ConfigurationFamily) -> Value {
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
  let pending_restart = pending_values(&snapshot.pending_restart, desired_resolution, family);
  let pending_convergence = pending_values(&snapshot.pending_convergence, desired_resolution, family);
  let issues = desired_resolution.map_or_else(Vec::new, |resolution| family_issues(resolution, family));
  let control = snapshot.control_status(family);
  let stored = desired_resolution
    .map(|resolution| match family {
      ConfigurationFamily::Runtime => &resolution.runtime_status,
      ConfigurationFamily::Lifecycle => &resolution.lifecycle_status,
    })
    .map_or_else(|| json!({"state": "unavailable"}), document_status);
  let effective_valid = active.values().all(|value| value.is_some()) && !issues.iter().any(|issue| issue["blocking"] == true);
  let desired_valid = desired.values().all(|value| value.is_some()) && !issues.iter().any(|issue| issue["blocking"] == true);
  let degraded = matches!(
    desired_resolution.map(|resolution| match family {
      ConfigurationFamily::Runtime => &resolution.runtime_status,
      ConfigurationFamily::Lifecycle => &resolution.lifecycle_status,
    }),
    Some(ConfigDocumentStatus::Invalid { .. })
  ) || !issues.is_empty()
    || control.capability == ConfigurationControlCapability::Degraded
    || control.redundancy_degraded;
  let mut disabled_capabilities = desired_resolution.map_or_else(Vec::new, |resolution| disabled_owners(resolution, family));
  if control.capability == ConfigurationControlCapability::UnavailableNoDatabaseIdentity {
    disabled_capabilities.push("configuration_recovery_controls".to_string());
  }
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
    "desired_config": configuration_document(family, &desired),
    "desired_sources": desired_sources,
    "pending_restart": pending_restart,
    "pending_convergence": pending_convergence,
    "control": control_status(control, family),
    "fallback_identities": fallback_identities,
    "deprecated_aliases": deprecated_aliases,
    "disabled_capabilities": disabled_capabilities,
    "issues": issues,
  });
  if let Some(gc_mode) = gc_mode {
    status.as_object_mut().expect("status is an object").insert("gc_mode".to_string(), Value::String(gc_mode.to_string()));
  }
  json!({
    "config": configuration_document(family, &active),
    "invariants": match family {
      ConfigurationFamily::Runtime => json!({}),
      ConfigurationFamily::Lifecycle => json!({"required_complete_marks": 2}),
    },
    "status": status,
  })
}

fn configuration_document(family: ConfigurationFamily, values: &BTreeMap<String, Option<&ConfigValue>>) -> Value {
  match family {
    ConfigurationFamily::Runtime => {
      let mut root = Map::new();
      root.insert("schema_version".to_string(), Value::from(1));
      for property in family_properties(family) {
        let (group, field) = property.path.split_once('.').expect("runtime property has group and field");
        let group = root.entry(group.to_string()).or_insert_with(|| Value::Object(Map::new()));
        group
          .as_object_mut()
          .expect("runtime group is an object")
          .insert(field.to_string(), values.get(property.path).and_then(|value| *value).map_or(Value::Null, config_value));
      }
      Value::Object(root)
    }
    ConfigurationFamily::Lifecycle => json!({
      "schema_version": 1,
      "snapshot_writes_enabled": value_at(values, "lifecycle.snapshot_writes_enabled"),
      "snapshot_retention": {
        "auto_months": value_at(values, "lifecycle.snapshot_retention_auto_months"),
        "manual_months": value_at(values, "lifecycle.snapshot_retention_manual_months"),
      },
      "garbage_collection": {
        "pending_delete_grace_seconds": value_at(values, "lifecycle.garbage_collection_pending_delete_grace_seconds"),
      },
    }),
  }
}

fn value_at(values: &BTreeMap<String, Option<&ConfigValue>>, path: &str) -> Value {
  values.get(path).and_then(|value| *value).map_or(Value::Null, config_value)
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

fn pending_values(pending: &BTreeSet<String>, resolution: Option<&ConfigResolution>, family: ConfigurationFamily) -> Value {
  let values = pending
    .iter()
    .filter(|path| property(path).is_some_and(|property| family.contains(property)))
    .map(|path| {
      let value = resolution
        .and_then(|resolution| resolution.properties.get(path))
        .and_then(|property| property.value.as_ref())
        .map_or(Value::Null, config_value);
      (path.clone(), value)
    })
    .collect::<Map<_, _>>();
  Value::Object(values)
}

fn family_issues(resolution: &ConfigResolution, family: ConfigurationFamily) -> Vec<Value> {
  resolution
    .issues
    .iter()
    .filter(|issue| issue_belongs_to_family(issue, family))
    .map(|issue| {
      json!({
        "property": issue.property,
        "source": issue.source.map(source_name),
        "blocking": issue.blocking,
        "message": issue.message,
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
  owners.into_iter().filter(|owner| !resolution.owner_ready(owner)).map(str::to_string).collect()
}

fn document_status(status: &ConfigDocumentStatus) -> Value {
  match status {
    ConfigDocumentStatus::Missing => json!({"state": "missing"}),
    ConfigDocumentStatus::Valid { schema_version } => json!({"state": "valid", "schema_version": schema_version}),
    ConfigDocumentStatus::Invalid { message } => json!({"state": "invalid", "message": message}),
  }
}

fn control_status(status: &ConfigurationControlFamilyStatus, family: ConfigurationFamily) -> Value {
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
    "errors": status.errors,
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
