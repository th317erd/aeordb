use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::path_utils::normalize_path;
use crate::engine::v4::reader::FormatError;
use crate::engine::v4::system_family::{
  IndexPolicyV1, SystemFamilyClassificationV1, SystemFamilyPolicyDecisionV1, SystemFamilyPolicyResolverV1, SystemFamilyPolicyV1,
  SystemFamilySubjectV1, SystemFamilyTransferOperationV1, TransferPolicyV1,
};
use crate::engine::HashAlgorithm;

/// Engine-facing authority for all path-based SystemFamily decisions.
///
/// The persistent v1 resolver requires canonical paths and reports bounded
/// format errors. This facade gives live consumers one normalization and error
/// boundary without reducing operation-specific policies to a system boolean.
#[derive(Clone, Copy, Debug)]
pub struct SystemFamilyPolicyResolver {
  inner: SystemFamilyPolicyResolverV1,
}

impl SystemFamilyPolicyResolver {
  pub fn new(algorithm: HashAlgorithm) -> EngineResult<Self> {
    let inner = SystemFamilyPolicyResolverV1::embedded(algorithm).map_err(system_family_error)?;
    Ok(Self { inner })
  }

  pub fn classify_path(self, path: &str) -> EngineResult<SystemFamilyClassificationV1> {
    let normalized = normalize_path(path);
    self.inner.classify(SystemFamilySubjectV1::Path(&normalized)).map_err(system_family_error)
  }

  pub fn policy_for_path(self, path: &str, operation: &'static str) -> EngineResult<SystemFamilyPolicyDecisionV1<SystemFamilyPolicyV1>> {
    let normalized = normalize_path(path);
    self.inner.policy(SystemFamilySubjectV1::Path(&normalized), operation).map_err(system_family_error)
  }

  pub fn transfer_policy_for_path(
    self,
    path: &str,
    operation: SystemFamilyTransferOperationV1,
  ) -> EngineResult<SystemFamilyPolicyDecisionV1<TransferPolicyV1>> {
    let normalized = normalize_path(path);
    self.inner.transfer_policy(SystemFamilySubjectV1::Path(&normalized), operation).map_err(system_family_error)
  }

  pub fn index_policy_for_path(self, path: &str) -> EngineResult<SystemFamilyPolicyDecisionV1<IndexPolicyV1>> {
    let normalized = normalize_path(path);
    self.inner.index_policy(SystemFamilySubjectV1::Path(&normalized)).map_err(system_family_error)
  }
}

fn system_family_error(error: FormatError) -> EngineError {
  EngineError::SystemFamilyPolicy { code: error.code(), reason: error.to_string() }
}
