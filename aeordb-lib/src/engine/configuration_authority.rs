use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;

use crate::engine::config_resolver::{
  ConfigDocumentInput, ConfigResolutionInputs, ConfigResolver, ConfigShadowReport, ConfigSource, ConfigValue, ConfigurationFamily,
  StartupConfigurationState,
};
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::merge_patch::{apply_merge_patch, MergeDepth};
use crate::engine::v4::config_value::{CanonicalValueBounds, canonical_value_to_json, canonicalize_json};
use crate::engine::v4::configuration_controls::ConfigurationControlFamilyStatus;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveConfigProperty {
  pub id: u16,
  pub path: String,
  pub owner: String,
  pub activation: String,
  pub value: Option<ConfigValue>,
  pub source: Option<ConfigSource>,
  pub activated_generation: u64,
}

#[derive(Clone, Debug)]
pub struct ConfigurationAuthoritySnapshot {
  pub generation: u64,
  pub startup: Arc<ConfigShadowReport>,
  pub desired: Arc<ConfigShadowReport>,
  pub active_properties: BTreeMap<String, ActiveConfigProperty>,
  pub pending_restart: BTreeSet<String>,
  pub pending_convergence: BTreeSet<String>,
  pub control_statuses: BTreeMap<ConfigurationFamily, ConfigurationControlFamilyStatus>,
}

impl ConfigurationAuthoritySnapshot {
  pub fn resolved_unsigned(&self, path: &str) -> Option<u64> {
    self.active_properties.get(path)?.value.as_ref().and_then(|value| match value {
      ConfigValue::Unsigned(value) => Some(*value),
      _ => None,
    })
  }

  pub fn resolved_boolean(&self, path: &str) -> Option<bool> {
    self.active_properties.get(path)?.value.as_ref().and_then(|value| match value {
      ConfigValue::Boolean(value) => Some(*value),
      _ => None,
    })
  }

  pub fn control_status(&self, family: ConfigurationFamily) -> &ConfigurationControlFamilyStatus {
    self.control_statuses.get(&family).expect("both frozen configuration families have control status")
  }
}

pub struct ConfigurationAuthority {
  startup: Arc<ConfigShadowReport>,
  current: ArcSwap<ConfigurationAuthoritySnapshot>,
  inputs: Mutex<ConfigResolutionInputs>,
}

impl ConfigurationAuthority {
  pub(crate) fn new(
    startup_state: StartupConfigurationState,
    control_statuses: BTreeMap<ConfigurationFamily, ConfigurationControlFamilyStatus>,
  ) -> Self {
    let startup = Arc::new(startup_state.report);
    let active_properties = startup
      .resolution
      .as_ref()
      .map(|resolution| {
        resolution
          .properties
          .values()
          .map(|property| {
            let activation = crate::engine::v4::contract_generated::CONFIGURATION_PROPERTIES
              .iter()
              .find(|registered| registered.path == property.path)
              .map_or("unknown", |registered| registered.activation);
            (
              property.path.clone(),
              ActiveConfigProperty {
                id: property.id,
                path: property.path.clone(),
                owner: property.owner.clone(),
                activation: activation.to_string(),
                value: property.value.clone(),
                source: property.source,
                activated_generation: 1,
              },
            )
          })
          .collect()
      })
      .unwrap_or_default();
    let initial = ConfigurationAuthoritySnapshot {
      generation: 1,
      startup: Arc::clone(&startup),
      desired: Arc::clone(&startup),
      active_properties,
      pending_restart: BTreeSet::new(),
      pending_convergence: BTreeSet::new(),
      control_statuses,
    };
    Self { startup, current: ArcSwap::from_pointee(initial), inputs: Mutex::new(startup_state.inputs) }
  }

  pub fn startup_report(&self) -> Arc<ConfigShadowReport> {
    Arc::clone(&self.startup)
  }

  pub fn snapshot(&self) -> Arc<ConfigurationAuthoritySnapshot> {
    self.current.load_full()
  }

  pub(crate) fn replace_document<F>(
    &self,
    family: ConfigurationFamily,
    bytes: &[u8],
    publish: F,
  ) -> EngineResult<Arc<ConfigurationAuthoritySnapshot>>
  where
    F: FnOnce(&[u8], u16, &ConfigurationAuthoritySnapshot) -> EngineResult<ConfigurationControlFamilyStatus>,
  {
    let mut inputs = self.inputs.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    self.replace_document_locked(family, bytes, &mut inputs, publish)
  }

  pub(crate) fn patch_document<F>(
    &self,
    family: ConfigurationFamily,
    patch_bytes: &[u8],
    publish: F,
  ) -> EngineResult<Arc<ConfigurationAuthoritySnapshot>>
  where
    F: FnOnce(&[u8], u16, &ConfigurationAuthoritySnapshot) -> EngineResult<ConfigurationControlFamilyStatus>,
  {
    let canonical_patch = canonicalize_json(patch_bytes, CanonicalValueBounds::AUDIT_VALUE)
      .map_err(|error| EngineError::InvalidInput(format!("invalid {} configuration patch: {error}", family.name())))?;
    let normalized_patch = canonical_value_to_json(
      &canonical_patch,
      CanonicalValueBounds::AUDIT_VALUE,
      crate::engine::config_resolver::MAX_CONFIG_DOCUMENT_BYTES,
    )
    .map_err(|error| EngineError::InvalidInput(format!("invalid {} configuration patch: {error}", family.name())))?;
    let patch: serde_json::Value = serde_json::from_slice(&normalized_patch)
      .map_err(|error| EngineError::InvalidInput(format!("invalid {} configuration patch: {error}", family.name())))?;

    let mut inputs = self.inputs.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    let context = self.startup.context.clone().ok_or_else(|| {
      EngineError::InvalidInput(self.startup.context_error.clone().unwrap_or_else(|| "configuration context is unavailable".to_string()))
    })?;
    let resolver = ConfigResolver::new(context);
    let base = patch_base(family, &inputs, &resolver)?;
    let mut merged: serde_json::Value = serde_json::from_slice(&base)
      .map_err(|error| EngineError::InvalidInput(format!("invalid {} configuration patch base: {error}", family.name())))?;
    apply_merge_patch(&mut merged, patch, MergeDepth::Unbounded);
    let root = merged
      .as_object_mut()
      .ok_or_else(|| EngineError::InvalidInput(format!("{} configuration patch must result in a JSON object", family.name())))?;
    root.insert("schema_version".to_string(), serde_json::Value::from(1));
    let merged_bytes = serde_json::to_vec(&merged)
      .map_err(|error| EngineError::InvalidInput(format!("cannot serialize {} configuration patch result: {error}", family.name())))?;
    self.replace_document_locked(family, &merged_bytes, &mut inputs, publish)
  }

  fn replace_document_locked<F>(
    &self,
    family: ConfigurationFamily,
    bytes: &[u8],
    inputs: &mut ConfigResolutionInputs,
    publish: F,
  ) -> EngineResult<Arc<ConfigurationAuthoritySnapshot>>
  where
    F: FnOnce(&[u8], u16, &ConfigurationAuthoritySnapshot) -> EngineResult<ConfigurationControlFamilyStatus>,
  {
    let context = self.startup.context.clone().ok_or_else(|| {
      EngineError::InvalidInput(self.startup.context_error.clone().unwrap_or_else(|| "configuration context is unavailable".to_string()))
    })?;
    let resolver = ConfigResolver::new(context.clone());
    let schema_version = resolver
      .validate_document(family, bytes)
      .map_err(|message| EngineError::InvalidInput(format!("invalid {} configuration: {message}", family.name())))?;
    let schema_version = u16::try_from(schema_version)
      .map_err(|_| EngineError::InvalidInput(format!("{} configuration schema exceeds u16", family.name())))?;

    let mut candidate_inputs = inputs.clone();
    match family {
      ConfigurationFamily::Runtime => candidate_inputs.runtime = ConfigDocumentInput::Bytes(bytes.to_vec()),
      ConfigurationFamily::Lifecycle => candidate_inputs.lifecycle = ConfigDocumentInput::Bytes(bytes.to_vec()),
    }
    let resolution = resolver.resolve(candidate_inputs.clone());
    if let Some(issue) = resolution.issues.iter().find(|issue| {
      issue.blocking
        && issue
          .property
          .as_deref()
          .and_then(|path| crate::engine::v4::contract_generated::CONFIGURATION_PROPERTIES.iter().find(|property| property.path == path))
          .is_some_and(|property| family.contains(property))
    }) {
      return Err(EngineError::InvalidInput(format!("invalid {} configuration: {}", family.name(), issue.message)));
    }
    let desired = Arc::new(ConfigShadowReport { context: Some(context), resolution: Some(resolution), context_error: None });
    let previous = self.current.load_full();
    let generation = previous
      .generation
      .checked_add(1)
      .ok_or_else(|| EngineError::InvalidInput("configuration authority generation exhausted".to_string()))?;

    let mut active_properties = previous.active_properties.clone();
    let mut pending_restart = previous.pending_restart.clone();
    let mut pending_convergence = previous.pending_convergence.clone();
    for property in desired.resolution.as_ref().expect("candidate resolution exists").properties.values() {
      let Some(registered) =
        crate::engine::v4::contract_generated::CONFIGURATION_PROPERTIES.iter().find(|registered| registered.path == property.path)
      else {
        continue;
      };
      if !family.contains(registered) {
        continue;
      }
      let active = active_properties.get(property.path.as_str());
      let changed = active.is_none_or(|active| active.value != property.value || active.source != property.source);
      if !changed {
        pending_restart.remove(&property.path);
        pending_convergence.remove(&property.path);
        continue;
      }
      if registered.activation == "startup_bound" {
        pending_restart.insert(property.path.clone());
        continue;
      }
      active_properties.insert(
        property.path.clone(),
        ActiveConfigProperty {
          id: property.id,
          path: property.path.clone(),
          owner: property.owner.clone(),
          activation: registered.activation.to_string(),
          value: property.value.clone(),
          source: property.source,
          activated_generation: generation,
        },
      );
      pending_restart.remove(&property.path);
      pending_convergence.remove(&property.path);
    }
    let mut next = ConfigurationAuthoritySnapshot {
      generation,
      startup: Arc::clone(&self.startup),
      desired,
      active_properties,
      pending_restart,
      pending_convergence,
      control_statuses: previous.control_statuses.clone(),
    };
    let control_status = publish(bytes, schema_version, &next)?;
    next.control_statuses.insert(family, control_status);

    *inputs = candidate_inputs;
    let next = Arc::new(next);
    self.current.store(Arc::clone(&next));
    Ok(next)
  }
}

fn patch_base(family: ConfigurationFamily, inputs: &ConfigResolutionInputs, resolver: &ConfigResolver) -> EngineResult<Vec<u8>> {
  let (current, last_known_good) = match family {
    ConfigurationFamily::Runtime => (&inputs.runtime, inputs.runtime_lkg.as_ref()),
    ConfigurationFamily::Lifecycle => (&inputs.lifecycle, inputs.lifecycle_lkg.as_ref()),
  };
  match current {
    ConfigDocumentInput::Bytes(bytes) if resolver.validate_document(family, bytes).is_ok() => return Ok(bytes.clone()),
    ConfigDocumentInput::Missing => {
      return Err(EngineError::InvalidInput(format!(
        "{} configuration PATCH has no current document; submit a complete PUT first",
        family.name()
      )));
    }
    ConfigDocumentInput::Bytes(_) | ConfigDocumentInput::Unreadable(_) => {}
  }
  if let Some(fallback) = last_known_good {
    resolver
      .validate_document(family, &fallback.bytes)
      .map_err(|message| EngineError::InvalidInput(format!("{} configuration PATCH has no valid base: {message}", family.name())))?;
    return Ok(fallback.bytes.clone());
  }
  Err(EngineError::InvalidInput(format!(
    "{} configuration PATCH has no valid current or last-known-good base; submit a complete PUT first",
    family.name()
  )))
}
