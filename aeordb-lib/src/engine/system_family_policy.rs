use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::path_utils::normalize_path;
use crate::engine::v4::reader::FormatError;
use crate::engine::v4::system_family::{
  IndexPolicyV1, StorageDomainV1, SystemFamilyClassificationV1, SystemFamilyMatchKindV1, SystemFamilyPolicyDecisionV1,
  SystemFamilyPolicyResolverV1, SystemFamilyPolicyV1, SystemFamilySubjectV1, SystemFamilyTransferOperationV1, TransferPolicyV1,
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

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct AbsoluteTransferPath {
  pub path: String,
  pub is_prefix: bool,
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
    self
      .inner
      .transfer_policy(SystemFamilySubjectV1::Path(&normalized), operation)
      .map_err(|error| system_family_path_error(error, &normalized, operation.name()))
  }

  /// Decide whether a broad transfer operation traverses one path.
  ///
  /// `NamedSubsetOnly` is deliberately excluded here: those families require
  /// an operation-specific named selector and must never leak into a broad
  /// traversal merely because their structural parent was selected.
  pub fn transfer_path_is_included(self, path: &str, operation: SystemFamilyTransferOperationV1) -> EngineResult<bool> {
    match self.transfer_policy_for_path(path, operation)? {
      SystemFamilyPolicyDecisionV1::Ordinary | SystemFamilyPolicyDecisionV1::StructuralContainer => Ok(true),
      SystemFamilyPolicyDecisionV1::Known { policy: TransferPolicyV1::RequiredInclude | TransferPolicyV1::OptionalValidated, .. } => {
        Ok(true)
      }
      SystemFamilyPolicyDecisionV1::Known {
        policy:
          TransferPolicyV1::OmitDeclared | TransferPolicyV1::NodeLocal | TransferPolicyV1::RedactOmit | TransferPolicyV1::NamedSubsetOnly,
        ..
      } => Ok(false),
      SystemFamilyPolicyDecisionV1::Known { family_id, policy: TransferPolicyV1::FailUnknown } => Err(EngineError::SystemFamilyPolicy {
        code: "system_family_transfer_refused",
        reason: format!("{} refuses family 0x{family_id:04x}", operation.name()),
      }),
    }
  }

  /// Absolute registry paths that a broad transfer must discover even when
  /// they are intentionally detached from the ordinary namespace root.
  pub(crate) fn included_absolute_paths(self, operation: SystemFamilyTransferOperationV1) -> EngineResult<Vec<AbsoluteTransferPath>> {
    let mut paths = Vec::new();
    for descriptor in self.inner.registry().iter() {
      let descriptor = descriptor.map_err(system_family_error)?;
      if descriptor.domain != StorageDomainV1::Path
        || !matches!(descriptor.match_kind, SystemFamilyMatchKindV1::AbsolutePathExact | SystemFamilyMatchKindV1::AbsolutePathPrefix)
        || !matches!(descriptor.policy.transfer_policy(operation), TransferPolicyV1::RequiredInclude | TransferPolicyV1::OptionalValidated)
      {
        continue;
      }
      let path = std::str::from_utf8(descriptor.matcher).map_err(|error| EngineError::SystemFamilyPolicy {
        code: "system_family_matcher_path_utf8",
        reason: format!("embedded absolute path matcher is not UTF-8: {error}"),
      })?;
      paths.push(AbsoluteTransferPath {
        path: path.strip_suffix('/').unwrap_or(path).to_string(),
        is_prefix: descriptor.match_kind == SystemFamilyMatchKindV1::AbsolutePathPrefix,
      });
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
  }

  pub fn index_policy_for_path(self, path: &str) -> EngineResult<SystemFamilyPolicyDecisionV1<IndexPolicyV1>> {
    let normalized = normalize_path(path);
    self.inner.index_policy(SystemFamilySubjectV1::Path(&normalized)).map_err(system_family_error)
  }
}

fn system_family_error(error: FormatError) -> EngineError {
  EngineError::SystemFamilyPolicy { code: error.code(), reason: error.to_string() }
}

fn system_family_path_error(error: FormatError, path: &str, operation: &str) -> EngineError {
  EngineError::SystemFamilyPolicy { code: error.code(), reason: format!("{operation} path '{path}': {error}") }
}
