use std::ops::{Deref, DerefMut};

use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::legacy_nvt_v1::LegacyNvtV1;
use crate::engine::scalar_converter::{CONVERTER_TYPE_HASH, HashConverter};

/// KV-owned compatibility wrapper around the byte-frozen NVT v1 core.
#[derive(Debug, Clone)]
pub struct KvNvt {
  legacy: LegacyNvtV1,
}

impl KvNvt {
  pub fn new(bucket_count: usize) -> Self {
    Self { legacy: LegacyNvtV1::new(Box::new(HashConverter), bucket_count) }
  }

  pub fn deserialize(data: &[u8]) -> EngineResult<Self> {
    let legacy = LegacyNvtV1::deserialize(data)?;
    if legacy.converter().type_tag() != CONVERTER_TYPE_HASH {
      return Err(EngineError::CorruptEntry { offset: 0, reason: "KV NVT does not use the hash converter".to_string() });
    }
    Ok(Self { legacy })
  }

  pub fn serialize(&self) -> Vec<u8> {
    self.legacy.serialize()
  }
}

impl AsRef<LegacyNvtV1> for KvNvt {
  fn as_ref(&self) -> &LegacyNvtV1 {
    &self.legacy
  }
}

impl Deref for KvNvt {
  type Target = LegacyNvtV1;

  fn deref(&self) -> &Self::Target {
    &self.legacy
  }
}

impl DerefMut for KvNvt {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut self.legacy
  }
}
