use std::collections::BTreeSet;

use tokio_util::sync::CancellationToken;

use crate::engine::permission_resolver::{CrudlifyOp, normalize_permission_path};

use super::database_header::SelectedDatabaseHeaderV4;
use super::read_view::{
  CurrentReadAuthorizationV1, LoadedReadAuthorityV1, ReadViewAuthorizationErrorV1, ReadViewAuthorizationFailureV1, ReadViewAuthorizerV1,
};

#[derive(Clone, Debug, PartialEq, Eq)]
enum PathAuthorizationScopeV1 {
  Direct,
  AncestorNavigation(BTreeSet<String>),
}

/// A nonempty authorization decision for one path and operation.
///
/// Direct authority covers the requested path. Ancestor navigation covers
/// only the named immediate children needed to reach deeper grants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathAuthorizationDecisionV1 {
  scope: PathAuthorizationScopeV1,
}

impl PathAuthorizationDecisionV1 {
  pub const fn direct() -> Self {
    Self { scope: PathAuthorizationScopeV1::Direct }
  }

  pub fn ancestor_navigation(allowed_children: BTreeSet<String>) -> Option<Self> {
    if allowed_children.is_empty() {
      None
    } else {
      Some(Self { scope: PathAuthorizationScopeV1::AncestorNavigation(allowed_children) })
    }
  }

  pub const fn is_direct(&self) -> bool {
    matches!(self.scope, PathAuthorizationScopeV1::Direct)
  }

  pub const fn allowed_children(&self) -> Option<&BTreeSet<String>> {
    match &self.scope {
      PathAuthorizationScopeV1::Direct => None,
      PathAuthorizationScopeV1::AncestorNavigation(children) => Some(children),
    }
  }

  /// Intersect current authority with a selected-root restriction.
  ///
  /// Returning `None` means the intersection contains no authorized path.
  pub fn intersect(&self, restriction: &Self) -> Option<Self> {
    match (&self.scope, &restriction.scope) {
      (PathAuthorizationScopeV1::Direct, PathAuthorizationScopeV1::Direct) => Some(Self::direct()),
      (PathAuthorizationScopeV1::Direct, PathAuthorizationScopeV1::AncestorNavigation(children))
      | (PathAuthorizationScopeV1::AncestorNavigation(children), PathAuthorizationScopeV1::Direct) => {
        Self::ancestor_navigation(children.clone())
      }
      (PathAuthorizationScopeV1::AncestorNavigation(current), PathAuthorizationScopeV1::AncestorNavigation(selected)) => {
        Self::ancestor_navigation(current.intersection(selected).cloned().collect())
      }
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectedRootRestrictionV1 {
  PermissionDocuments,
  RootCurrentPolicy,
  ShareCurrentPolicy,
}

/// Current authorization captured before any selected-root authority is read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentPathAuthorizationV1 {
  path: String,
  operation: CrudlifyOp,
  current_groups: Vec<String>,
  decision: PathAuthorizationDecisionV1,
  selected_root_restriction: SelectedRootRestrictionV1,
}

impl CurrentPathAuthorizationV1 {
  fn new(
    path: impl AsRef<str>,
    operation: CrudlifyOp,
    current_groups: Vec<String>,
    decision: PathAuthorizationDecisionV1,
    selected_root_restriction: SelectedRootRestrictionV1,
  ) -> Self {
    Self { path: normalize_permission_path(path.as_ref()), operation, current_groups, decision, selected_root_restriction }
  }

  pub fn for_user(
    path: impl AsRef<str>,
    operation: CrudlifyOp,
    current_groups: Vec<String>,
    decision: PathAuthorizationDecisionV1,
  ) -> Self {
    Self::new(path, operation, current_groups, decision, SelectedRootRestrictionV1::PermissionDocuments)
  }

  pub fn for_root(path: impl AsRef<str>, operation: CrudlifyOp) -> Self {
    Self::new(path, operation, Vec::new(), PathAuthorizationDecisionV1::direct(), SelectedRootRestrictionV1::RootCurrentPolicy)
  }

  pub fn for_share_current_head(path: impl AsRef<str>, operation: CrudlifyOp, decision: PathAuthorizationDecisionV1) -> Self {
    Self::new(path, operation, Vec::new(), decision, SelectedRootRestrictionV1::ShareCurrentPolicy)
  }

  pub fn path(&self) -> &str {
    &self.path
  }

  pub const fn operation(&self) -> CrudlifyOp {
    self.operation
  }

  pub fn current_groups(&self) -> &[String] {
    &self.current_groups
  }

  pub const fn decision(&self) -> &PathAuthorizationDecisionV1 {
    &self.decision
  }

  pub const fn selected_root_restriction(&self) -> SelectedRootRestrictionV1 {
    self.selected_root_restriction
  }
}

pub trait CurrentPathAuthorizationSourceV1: Send + Sync {
  fn authorize_current(
    &self,
    cancellation: &CancellationToken,
  ) -> Result<CurrentReadAuthorizationV1<CurrentPathAuthorizationV1>, ReadViewAuthorizationErrorV1>;
}

/// A request-owned current authorization result captured by the existing
/// account/key/share/permission authority before selected-root resolution.
#[derive(Clone, Debug)]
pub struct CapturedCurrentPathAuthorizationSourceV1 {
  result: Result<CurrentReadAuthorizationV1<CurrentPathAuthorizationV1>, ReadViewAuthorizationErrorV1>,
}

impl CapturedCurrentPathAuthorizationSourceV1 {
  pub const fn new(result: Result<CurrentReadAuthorizationV1<CurrentPathAuthorizationV1>, ReadViewAuthorizationErrorV1>) -> Self {
    Self { result }
  }
}

impl CurrentPathAuthorizationSourceV1 for CapturedCurrentPathAuthorizationSourceV1 {
  fn authorize_current(
    &self,
    _cancellation: &CancellationToken,
  ) -> Result<CurrentReadAuthorizationV1<CurrentPathAuthorizationV1>, ReadViewAuthorizationErrorV1> {
    self.result.clone()
  }
}

#[derive(Clone, Copy, Debug)]
pub struct SelectedRootPermissionRequestV1<'a> {
  path: &'a str,
  operation: CrudlifyOp,
  current_groups: &'a [String],
}

impl<'a> SelectedRootPermissionRequestV1<'a> {
  pub const fn path(self) -> &'a str {
    self.path
  }

  pub const fn operation(self) -> CrudlifyOp {
    self.operation
  }

  pub const fn current_groups(self) -> &'a [String] {
    self.current_groups
  }
}

/// Supplies one selected-root decision from captured immutable authority.
///
/// Returning `Ok(None)` is an ordinary permission denial. The authorizer owns
/// intersection so a source implementation cannot expand current authority.
pub trait SelectedRootPermissionSourceV1: Send + Sync {
  fn authorize_selected_root(
    &self,
    header: &SelectedDatabaseHeaderV4,
    authority: &LoadedReadAuthorityV1,
    request: SelectedRootPermissionRequestV1<'_>,
    cancellation: &CancellationToken,
  ) -> Result<Option<PathAuthorizationDecisionV1>, ReadViewAuthorizationFailureV1>;
}

pub struct ReadViewPermissionAuthorizerV1<C, S> {
  current: C,
  selected: S,
}

impl<C, S> ReadViewPermissionAuthorizerV1<C, S> {
  pub const fn new(current: C, selected: S) -> Self {
    Self { current, selected }
  }

  pub const fn current_source(&self) -> &C {
    &self.current
  }

  pub const fn selected_source(&self) -> &S {
    &self.selected
  }
}

impl<C, S> ReadViewAuthorizerV1 for ReadViewPermissionAuthorizerV1<C, S>
where
  C: CurrentPathAuthorizationSourceV1,
  S: SelectedRootPermissionSourceV1,
{
  type CurrentAuthorization = CurrentPathAuthorizationV1;
  type ResolvedAuthorization = PathAuthorizationDecisionV1;

  fn authorize_current(
    &self,
    cancellation: &CancellationToken,
  ) -> Result<CurrentReadAuthorizationV1<Self::CurrentAuthorization>, ReadViewAuthorizationErrorV1> {
    let current = self.current.authorize_current(cancellation)?;
    let credential_matches = match current.authorization().selected_root_restriction() {
      SelectedRootRestrictionV1::ShareCurrentPolicy => current.credential_kind() == super::read_view::ReadViewCredentialKindV1::Share,
      SelectedRootRestrictionV1::PermissionDocuments | SelectedRootRestrictionV1::RootCurrentPolicy => {
        current.credential_kind() == super::read_view::ReadViewCredentialKindV1::Ordinary
      }
    };
    if !credential_matches {
      return Err(ReadViewAuthorizationErrorV1::corrupt(
        current.concealment(),
        "current path authorization policy disagrees with the credential kind",
      ));
    }
    Ok(current)
  }

  fn restrict_to_selected_root(
    &self,
    current: &Self::CurrentAuthorization,
    header: &SelectedDatabaseHeaderV4,
    authority: &LoadedReadAuthorityV1,
    cancellation: &CancellationToken,
  ) -> Result<Self::ResolvedAuthorization, ReadViewAuthorizationFailureV1> {
    if cancellation.is_cancelled() {
      return Err(ReadViewAuthorizationFailureV1::Canceled);
    }
    if matches!(
      current.selected_root_restriction(),
      SelectedRootRestrictionV1::RootCurrentPolicy | SelectedRootRestrictionV1::ShareCurrentPolicy
    ) {
      return Ok(current.decision().clone());
    }

    let request =
      SelectedRootPermissionRequestV1 { path: current.path(), operation: current.operation(), current_groups: current.current_groups() };
    let selected = self.selected.authorize_selected_root(header, authority, request, cancellation)?;
    if cancellation.is_cancelled() {
      return Err(ReadViewAuthorizationFailureV1::Canceled);
    }
    let Some(selected) = selected else {
      return Err(ReadViewAuthorizationFailureV1::Denied);
    };
    current.decision().intersect(&selected).ok_or(ReadViewAuthorizationFailureV1::Denied)
  }
}
