use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::engine::config_resolver::{ConfigShadowReport, ConfigSource, ConfigValue};

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
}

impl ConfigurationAuthority {
  pub fn new(startup: Arc<ConfigShadowReport>) -> Self {
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
    Self { startup, current: ArcSwap::from_pointee(initial) }
  }

  pub fn startup_report(&self) -> Arc<ConfigShadowReport> {
    Arc::clone(&self.startup)
  }

  pub fn snapshot(&self) -> Arc<ConfigurationAuthoritySnapshot> {
    self.current.load_full()
  }
}
