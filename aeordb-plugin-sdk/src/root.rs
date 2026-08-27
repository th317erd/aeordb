//! Root-selection and exact-root response contracts shared by embedded callers,
//! guest plugins, and AeorDB's host-function bridge.

use std::error::Error;
use std::fmt;

use serde::de::Deserializer;
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};

const MAXIMUM_ROOT_ALIAS_BYTES_V1: usize = 4_096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginRootContractErrorV1 {
  context: String,
}

impl PluginRootContractErrorV1 {
  fn new(context: impl Into<String>) -> Self {
    Self { context: context.into() }
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

impl fmt::Display for PluginRootContractErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str(&self.context)
  }
}

impl Error for PluginRootContractErrorV1 {}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PluginRootSelectorV1 {
  #[default]
  CurrentHead,
  RootHash(String),
  Snapshot(String),
  Version(String),
}

impl PluginRootSelectorV1 {
  fn into_canonical(self) -> Result<Self, PluginRootContractErrorV1> {
    match self {
      Self::CurrentHead => Ok(Self::CurrentHead),
      Self::RootHash(value) => canonical_root_hash(value).map(Self::RootHash),
      Self::Snapshot(value) => {
        validate_alias(&value)?;
        Ok(Self::Snapshot(value))
      }
      Self::Version(value) => canonical_root_hash(value).map(Self::Version),
    }
  }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginNamespaceReadInvocationV1 {
  selector: PluginRootSelectorV1,
}

impl PluginNamespaceReadInvocationV1 {
  pub fn current() -> Self {
    Self { selector: PluginRootSelectorV1::CurrentHead }
  }

  pub fn root_hash(root_hash: impl Into<String>) -> Result<Self, PluginRootContractErrorV1> {
    Self::from_selector(PluginRootSelectorV1::RootHash(root_hash.into()))
  }

  pub fn snapshot(snapshot: impl Into<String>) -> Result<Self, PluginRootContractErrorV1> {
    Self::from_selector(PluginRootSelectorV1::Snapshot(snapshot.into()))
  }

  pub fn version(version: impl Into<String>) -> Result<Self, PluginRootContractErrorV1> {
    Self::from_selector(PluginRootSelectorV1::Version(version.into()))
  }

  pub fn from_selector(selector: PluginRootSelectorV1) -> Result<Self, PluginRootContractErrorV1> {
    Ok(Self { selector: selector.into_canonical()? })
  }

  pub fn validate(self) -> Result<Self, PluginRootContractErrorV1> {
    Self::from_selector(self.selector)
  }

  pub const fn selector(&self) -> &PluginRootSelectorV1 {
    &self.selector
  }

  pub const fn is_current(&self) -> bool {
    matches!(self.selector, PluginRootSelectorV1::CurrentHead)
  }

  pub(crate) fn insert_into_json_object(&self, target: &mut serde_json::Map<String, serde_json::Value>) {
    match &self.selector {
      PluginRootSelectorV1::CurrentHead => {}
      PluginRootSelectorV1::RootHash(value) => {
        target.insert("root_hash".to_string(), serde_json::Value::String(value.clone()));
      }
      PluginRootSelectorV1::Snapshot(value) => {
        target.insert("snapshot".to_string(), serde_json::Value::String(value.clone()));
      }
      PluginRootSelectorV1::Version(value) => {
        target.insert("version".to_string(), serde_json::Value::String(value.clone()));
      }
    }
  }
}

impl Serialize for PluginNamespaceReadInvocationV1 {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    let field_count = usize::from(!self.is_current());
    let mut map = serializer.serialize_map(Some(field_count))?;
    match &self.selector {
      PluginRootSelectorV1::CurrentHead => {}
      PluginRootSelectorV1::RootHash(value) => map.serialize_entry("root_hash", value)?,
      PluginRootSelectorV1::Snapshot(value) => map.serialize_entry("snapshot", value)?,
      PluginRootSelectorV1::Version(value) => map.serialize_entry("version", value)?,
    }
    map.end()
  }
}

impl<'de> Deserialize<'de> for PluginNamespaceReadInvocationV1 {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RawInvocationV1 {
      root_hash: Option<String>,
      snapshot: Option<String>,
      version: Option<String>,
    }

    let raw = RawInvocationV1::deserialize(deserializer)?;
    let present = usize::from(raw.root_hash.is_some()) + usize::from(raw.snapshot.is_some()) + usize::from(raw.version.is_some());
    if present > 1 {
      return Err(serde::de::Error::custom("root_hash, snapshot, and version are mutually exclusive"));
    }
    let selector = match (raw.root_hash, raw.snapshot, raw.version) {
      (Some(value), None, None) => PluginRootSelectorV1::RootHash(value),
      (None, Some(value), None) => PluginRootSelectorV1::Snapshot(value),
      (None, None, Some(value)) => PluginRootSelectorV1::Version(value),
      (None, None, None) => PluginRootSelectorV1::CurrentHead,
      _ => return Err(serde::de::Error::custom("root selectors are mutually exclusive")),
    };
    Self::from_selector(selector).map_err(serde::de::Error::custom)
  }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginRootStateV1 {
  Live,
  Retained,
  PendingDelete,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginRootMetadataV1 {
  pub hash: String,
  pub state: PluginRootStateV1,
  pub expires_at: Option<i64>,
}

impl PluginRootMetadataV1 {
  pub fn validate(&self) -> Result<(), PluginRootContractErrorV1> {
    let canonical_hash = canonical_root_hash(self.hash.clone())?;
    if canonical_hash != self.hash {
      return Err(PluginRootContractErrorV1::new("root metadata hash must be lowercase hexadecimal"));
    }
    match (self.state, self.expires_at) {
      (PluginRootStateV1::PendingDelete, Some(_)) | (PluginRootStateV1::Live | PluginRootStateV1::Retained, None) => Ok(()),
      (PluginRootStateV1::PendingDelete, None) => Err(PluginRootContractErrorV1::new("pending-delete root metadata requires expires_at")),
      (PluginRootStateV1::Live | PluginRootStateV1::Retained, Some(_)) => {
        Err(PluginRootContractErrorV1::new("live and retained root metadata cannot carry expires_at"))
      }
    }
  }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginItemsResponseV1<Items> {
  pub root: PluginRootMetadataV1,
  pub items: Items,
  pub has_more: bool,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub total: Option<u64>,
}

impl<Items> PluginItemsResponseV1<Items> {
  pub fn new(root: PluginRootMetadataV1, items: Items, has_more: bool, total: Option<u64>) -> Self {
    Self { root, items, has_more, total }
  }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginResultsResponseV1<Results> {
  pub root: PluginRootMetadataV1,
  pub results: Results,
  pub has_more: bool,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub total: Option<u64>,
}

impl<Results> PluginResultsResponseV1<Results> {
  pub fn new(root: PluginRootMetadataV1, results: Results, has_more: bool, total: Option<u64>) -> Self {
    Self { root, results, has_more, total }
  }
}

fn canonical_root_hash(value: String) -> Result<String, PluginRootContractErrorV1> {
  if !matches!(value.len(), 64 | 128) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
    return Err(PluginRootContractErrorV1::new("root hash must be 64 or 128 hexadecimal characters"));
  }
  if value.bytes().all(|byte| byte == b'0') {
    return Err(PluginRootContractErrorV1::new("root hash cannot be all zeroes"));
  }
  Ok(value.to_ascii_lowercase())
}

fn validate_alias(value: &str) -> Result<(), PluginRootContractErrorV1> {
  if value.is_empty() || value.len() > MAXIMUM_ROOT_ALIAS_BYTES_V1 || value.chars().any(char::is_control) {
    return Err(PluginRootContractErrorV1::new("snapshot alias is empty, oversized, or contains control characters"));
  }
  Ok(())
}
