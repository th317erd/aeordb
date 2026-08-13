use std::ops::{Deref, DerefMut};

use crate::engine::errors::EngineResult;
use crate::engine::legacy_nvt_v1::LegacyNvtV1;
use crate::engine::scalar_converter::ScalarConverter;

/// V0 field-index compatibility wrapper around the byte-frozen NVT v1 core.
#[derive(Debug, Clone)]
pub struct FieldIndexV0Nvt {
  legacy: LegacyNvtV1,
}

impl FieldIndexV0Nvt {
  pub fn new(converter: Box<dyn ScalarConverter>, bucket_count: usize) -> Self {
    Self { legacy: LegacyNvtV1::new(converter, bucket_count) }
  }

  pub fn deserialize(data: &[u8]) -> EngineResult<Self> {
    LegacyNvtV1::deserialize(data).map(|legacy| Self { legacy })
  }

  pub fn serialize(&self) -> Vec<u8> {
    self.legacy.serialize()
  }
}

impl AsRef<LegacyNvtV1> for FieldIndexV0Nvt {
  fn as_ref(&self) -> &LegacyNvtV1 {
    &self.legacy
  }
}

impl Deref for FieldIndexV0Nvt {
  type Target = LegacyNvtV1;

  fn deref(&self) -> &Self::Target {
    &self.legacy
  }
}

impl DerefMut for FieldIndexV0Nvt {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut self.legacy
  }
}
