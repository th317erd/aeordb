use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;

use crate::engine::config_resolver::{
  ConfigDocumentInput, ConfigResolutionInputs, ConfigResolver, ConfigShadowReport, ConfigSource, ConfigValue, ConfigurationFamily,
  StartupConfigurationState,
};
use crate::engine::errors::{EngineError, EngineResult};

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
}

pub struct ConfigurationAuthority {
  startup: Arc<ConfigShadowReport>,
  current: ArcSwap<ConfigurationAuthoritySnapshot>,
  inputs: Mutex<ConfigResolutionInputs>,
}

impl ConfigurationAuthority {
  pub(crate) fn new(startup_state: StartupConfigurationState) -> Self {
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
    F: FnOnce(&[u8]) -> EngineResult<()>,
  {
    let context = self.startup.context.clone().ok_or_else(|| {
      EngineError::InvalidInput(self.startup.context_error.clone().unwrap_or_else(|| "configuration context is unavailable".to_string()))
    })?;
    let resolver = ConfigResolver::new(context.clone());
    resolver
      .validate_document(family, bytes)
      .map_err(|message| EngineError::InvalidInput(format!("invalid {} configuration: {message}", family.name())))?;

    let mut inputs = self.inputs.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
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

    publish(bytes)?;

    *inputs = candidate_inputs;
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
    let next = Arc::new(ConfigurationAuthoritySnapshot {
      generation,
      startup: Arc::clone(&self.startup),
      desired,
      active_properties,
      pending_restart,
      pending_convergence,
    });
    self.current.store(Arc::clone(&next));
    Ok(next)
  }
}
