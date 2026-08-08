use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::entry_type::EntryType;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransferPathSelection {
  Include,
  Omit,
  StructuralContainer,
}

/// Visibility of one namespace path through ordinary data APIs.
///
/// This deliberately preserves structural containers as a distinct outcome:
/// callers may traverse them to reach selected children, but may not expose or
/// mutate them as ordinary leaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenericDataPathSelection {
  Include,
  Conceal,
  StructuralContainer,
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

  pub fn transfer_policy_for_entry_type(
    self,
    entry_type: EntryType,
    operation: SystemFamilyTransferOperationV1,
  ) -> EngineResult<SystemFamilyPolicyDecisionV1<TransferPolicyV1>> {
    self.inner.transfer_policy(SystemFamilySubjectV1::EntryType(u16::from(entry_type.to_u8())), operation).map_err(|error| {
      EngineError::SystemFamilyPolicy { code: error.code(), reason: format!("{} entry type {:?}: {error}", operation.name(), entry_type) }
    })
  }

  /// Decide whether a broad transfer operation traverses one path.
  ///
  /// `NamedSubsetOnly` is deliberately excluded here: those families require
  /// an operation-specific named selector and must never leak into a broad
  /// traversal merely because their structural parent was selected.
  pub(crate) fn transfer_path_selection(
    self,
    path: &str,
    operation: SystemFamilyTransferOperationV1,
  ) -> EngineResult<TransferPathSelection> {
    transfer_selection(self.transfer_policy_for_path(path, operation)?, operation)
  }

  pub(crate) fn transfer_entry_type_selection(
    self,
    entry_type: EntryType,
    operation: SystemFamilyTransferOperationV1,
  ) -> EngineResult<TransferPathSelection> {
    transfer_selection(self.transfer_policy_for_entry_type(entry_type, operation)?, operation)
  }

  pub fn transfer_path_is_included(self, path: &str, operation: SystemFamilyTransferOperationV1) -> EngineResult<bool> {
    Ok(!matches!(self.transfer_path_selection(path, operation)?, TransferPathSelection::Omit))
  }

  /// Select one path for an ordinary data API using the registry's frozen
  /// `DataExport` policy. Unknown protected families remain typed errors.
  pub fn generic_data_path_selection(self, path: &str) -> EngineResult<GenericDataPathSelection> {
    match self.transfer_path_selection(path, SystemFamilyTransferOperationV1::DataExport)? {
      TransferPathSelection::Include => Ok(GenericDataPathSelection::Include),
      TransferPathSelection::Omit => Ok(GenericDataPathSelection::Conceal),
      TransferPathSelection::StructuralContainer => Ok(GenericDataPathSelection::StructuralContainer),
    }
  }

  pub fn generic_data_path_is_visible(self, path: &str) -> EngineResult<bool> {
    Ok(matches!(self.generic_data_path_selection(path)?, GenericDataPathSelection::Include))
  }

  /// Require a concrete path to be legal through an ordinary data API.
  pub fn require_generic_data_leaf_path(self, path: &str) -> EngineResult<()> {
    match self.generic_data_path_selection(path)? {
      GenericDataPathSelection::Include => Ok(()),
      GenericDataPathSelection::Conceal => Err(EngineError::SystemFamilyPolicy {
        code: "system_family_generic_data_concealed",
        reason: format!("generic data API conceals path '{}'", normalize_path(path)),
      }),
      GenericDataPathSelection::StructuralContainer => Err(EngineError::SystemFamilyPolicy {
        code: "system_family_structural_leaf",
        reason: format!("generic data API uses structural container '{}' as a leaf", normalize_path(path)),
      }),
    }
  }

  /// Require a path to be legal as a concrete file, symlink, or deletion in a
  /// transfer payload. Structural containers are valid traversal waypoints but
  /// are never valid leaves.
  pub fn require_transfer_leaf_path(self, path: &str, operation: SystemFamilyTransferOperationV1) -> EngineResult<()> {
    match self.transfer_path_selection(path, operation)? {
      TransferPathSelection::Include => Ok(()),
      TransferPathSelection::Omit => Err(EngineError::SystemFamilyPolicy {
        code: "system_family_transfer_omitted",
        reason: format!("{} payload contains omitted path '{}'", operation.name(), normalize_path(path)),
      }),
      TransferPathSelection::StructuralContainer => Err(EngineError::SystemFamilyPolicy {
        code: "system_family_structural_leaf",
        reason: format!("{} payload uses structural container '{}' as a leaf", operation.name(), normalize_path(path)),
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

fn transfer_selection(
  decision: SystemFamilyPolicyDecisionV1<TransferPolicyV1>,
  operation: SystemFamilyTransferOperationV1,
) -> EngineResult<TransferPathSelection> {
  match decision {
    SystemFamilyPolicyDecisionV1::Ordinary => Ok(TransferPathSelection::Include),
    SystemFamilyPolicyDecisionV1::StructuralContainer => Ok(TransferPathSelection::StructuralContainer),
    SystemFamilyPolicyDecisionV1::Known { policy: TransferPolicyV1::RequiredInclude | TransferPolicyV1::OptionalValidated, .. } => {
      Ok(TransferPathSelection::Include)
    }
    SystemFamilyPolicyDecisionV1::Known {
      policy:
        TransferPolicyV1::OmitDeclared | TransferPolicyV1::NodeLocal | TransferPolicyV1::RedactOmit | TransferPolicyV1::NamedSubsetOnly,
      ..
    } => Ok(TransferPathSelection::Omit),
    SystemFamilyPolicyDecisionV1::Known { family_id, policy: TransferPolicyV1::FailUnknown } => Err(EngineError::SystemFamilyPolicy {
      code: "system_family_transfer_refused",
      reason: format!("{} refuses family 0x{family_id:04x}", operation.name()),
    }),
  }
}

fn system_family_error(error: FormatError) -> EngineError {
  EngineError::SystemFamilyPolicy { code: error.code(), reason: error.to_string() }
}

fn system_family_path_error(error: FormatError, path: &str, operation: &str) -> EngineError {
  EngineError::SystemFamilyPolicy { code: error.code(), reason: format!("{operation} path '{path}': {error}") }
}
