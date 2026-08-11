use thiserror::Error;

use super::namespace::{SemanticAvailabilityV1, SemanticUnavailableReasonV1};
use super::read_view::ResolvedReadViewV1;
use super::reader::FormatError;
use super::root_authority::{ImmutableNamespaceAuthorityV1, RootAuthorityReferenceRoleV1};
use super::system_family::{
  SystemFamilyPolicyDecisionV1, SystemFamilyPolicyResolverV1, SystemFamilySubjectV1, SystemFamilyTransferOperationV1, TransferPolicyV1,
};

const NAMESPACE_ROOT_EDGE: u8 = 0x01;
const NAMESPACE_TREE_EDGE: u8 = 0x02;
const SEMANTIC_STATE_EDGE: u8 = 0x04;
const ROOT_ADMISSION_EDGE: u8 = 0x08;
const REQUIRED_AUTHORITY_EDGES: u8 = NAMESPACE_ROOT_EDGE | NAMESPACE_TREE_EDGE | SEMANTIC_STATE_EDGE | ROOT_ADMISSION_EDGE;
const REQUIRED_AUTHORITY_EDGE_COUNT: u8 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferClosureItemV1<'a> {
  AuthorityEdge { role: RootAuthorityReferenceRoleV1, identity: &'a [u8] },
  StorageSubject(SystemFamilySubjectV1<'a>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferClosureDecisionV1 {
  RequiredAuthority { role: RootAuthorityReferenceRoleV1 },
  IncludeOrdinary,
  TraverseStructuralContainer,
  IncludeKnown { family_id: u16, policy: TransferPolicyV1 },
  OmitKnown { family_id: u16, policy: TransferPolicyV1 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferClosureCompletionV1 {
  Complete,
  DataOnly { reason: SemanticUnavailableReasonV1 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransferClosureSummaryV1 {
  pub operation: SystemFamilyTransferOperationV1,
  pub processed_items: u64,
  pub required_authority_edges: u8,
  pub included_items: u64,
  pub omitted_items: u64,
  pub structural_containers: u64,
  pub completion: TransferClosureCompletionV1,
}

#[derive(Debug, Error)]
pub enum TransferClosureErrorV1 {
  #[error("transfer closure classification was canceled")]
  Canceled,
  #[error("transfer closure item limit cannot fit the required authority prefix")]
  LimitTooSmall,
  #[error("transfer closure item limit was exhausted")]
  ItemLimit,
  #[error("transfer closure is missing one or more required authority edges")]
  AuthorityIncomplete,
  #[error("transfer closure authority edge {role:?} has the wrong identity")]
  AuthorityMismatch { role: RootAuthorityReferenceRoleV1 },
  #[error("transfer closure authority edge {role:?} is duplicated")]
  AuthorityDuplicate { role: RootAuthorityReferenceRoleV1 },
  #[error("transfer closure authority edge {role:?} appeared after payload classification began")]
  AuthorityAfterPayload { role: RootAuthorityReferenceRoleV1 },
  #[error("{operation:?} refuses protected family 0x{family_id:04x}")]
  TransferRefused { family_id: u16, operation: SystemFamilyTransferOperationV1 },
  #[error(transparent)]
  SystemFamily(#[from] FormatError),
  #[error("transfer closure classifier has already failed")]
  Failed,
}

impl TransferClosureErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Canceled => "transfer_closure_canceled",
      Self::LimitTooSmall => "transfer_closure_limit_too_small",
      Self::ItemLimit => "transfer_closure_item_limit",
      Self::AuthorityIncomplete => "transfer_closure_authority_incomplete",
      Self::AuthorityMismatch { .. } => "transfer_closure_authority_mismatch",
      Self::AuthorityDuplicate { .. } => "transfer_closure_authority_duplicate",
      Self::AuthorityAfterPayload { .. } => "transfer_closure_authority_after_payload",
      Self::TransferRefused { .. } => "system_family_transfer_refused",
      Self::SystemFamily(error) => error.code(),
      Self::Failed => "transfer_closure_failed",
    }
  }
}

/// Constant-state classifier for one pinned, authorized namespace root.
///
/// Consumers must retain the returned classifier until `finish` succeeds and
/// must not publish a complete transfer before that terminal summary. The
/// classifier borrows the resolved view so its request pin cannot be released
/// while the closure is being classified.
#[derive(Debug)]
pub struct TransferClosureClassifierV1<'a> {
  authority: &'a ImmutableNamespaceAuthorityV1,
  resolver: SystemFamilyPolicyResolverV1,
  cancellation: &'a tokio_util::sync::CancellationToken,
  operation: SystemFamilyTransferOperationV1,
  maximum_items: u64,
  processed_items: u64,
  seen_authority_edges: u8,
  included_items: u64,
  omitted_items: u64,
  structural_containers: u64,
  payload_started: bool,
  failed: bool,
  completion: TransferClosureCompletionV1,
}

impl<'a> TransferClosureClassifierV1<'a> {
  pub fn for_read_view<A>(
    view: &'a ResolvedReadViewV1<A>,
    operation: SystemFamilyTransferOperationV1,
    maximum_items: u64,
  ) -> Result<Self, TransferClosureErrorV1> {
    if maximum_items < u64::from(REQUIRED_AUTHORITY_EDGE_COUNT) {
      return Err(TransferClosureErrorV1::LimitTooSmall);
    }
    if view.cancellation().is_cancelled() {
      return Err(TransferClosureErrorV1::Canceled);
    }
    let completion = match &view.authority().semantic_state.availability {
      SemanticAvailabilityV1::Complete { .. } => TransferClosureCompletionV1::Complete,
      SemanticAvailabilityV1::ContentOnly { reason } => TransferClosureCompletionV1::DataOnly { reason: *reason },
    };
    Ok(Self {
      authority: view.authority(),
      resolver: SystemFamilyPolicyResolverV1::selected(view.system_family_registry()),
      cancellation: view.cancellation(),
      operation,
      maximum_items,
      processed_items: 0,
      seen_authority_edges: 0,
      included_items: 0,
      omitted_items: 0,
      structural_containers: 0,
      payload_started: false,
      failed: false,
      completion,
    })
  }

  pub fn classify(&mut self, item: TransferClosureItemV1<'_>) -> Result<TransferClosureDecisionV1, TransferClosureErrorV1> {
    if self.failed {
      return Err(TransferClosureErrorV1::Failed);
    }
    match self.classify_inner(item) {
      Ok(decision) => Ok(decision),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }

  pub fn finish(self) -> Result<TransferClosureSummaryV1, TransferClosureErrorV1> {
    if self.failed {
      return Err(TransferClosureErrorV1::Failed);
    }
    if self.cancellation.is_cancelled() {
      return Err(TransferClosureErrorV1::Canceled);
    }
    if self.seen_authority_edges != REQUIRED_AUTHORITY_EDGES {
      return Err(TransferClosureErrorV1::AuthorityIncomplete);
    }
    Ok(TransferClosureSummaryV1 {
      operation: self.operation,
      processed_items: self.processed_items,
      required_authority_edges: self.seen_authority_edges.count_ones() as u8,
      included_items: self.included_items,
      omitted_items: self.omitted_items,
      structural_containers: self.structural_containers,
      completion: self.completion,
    })
  }

  fn classify_inner(&mut self, item: TransferClosureItemV1<'_>) -> Result<TransferClosureDecisionV1, TransferClosureErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(TransferClosureErrorV1::Canceled);
    }
    if self.processed_items >= self.maximum_items {
      return Err(TransferClosureErrorV1::ItemLimit);
    }
    match item {
      TransferClosureItemV1::AuthorityEdge { role, identity } => self.classify_authority_edge(role, identity),
      TransferClosureItemV1::StorageSubject(subject) => self.classify_storage_subject(subject),
    }
  }

  fn classify_authority_edge(
    &mut self,
    role: RootAuthorityReferenceRoleV1,
    identity: &[u8],
  ) -> Result<TransferClosureDecisionV1, TransferClosureErrorV1> {
    if self.payload_started {
      return Err(TransferClosureErrorV1::AuthorityAfterPayload { role });
    }
    let bit = authority_edge_bit(role);
    if self.seen_authority_edges & bit != 0 {
      return Err(TransferClosureErrorV1::AuthorityDuplicate { role });
    }
    if identity != expected_authority_identity(self.authority, role) {
      return Err(TransferClosureErrorV1::AuthorityMismatch { role });
    }
    self.seen_authority_edges |= bit;
    self.processed_items += 1;
    Ok(TransferClosureDecisionV1::RequiredAuthority { role })
  }

  fn classify_storage_subject(&mut self, subject: SystemFamilySubjectV1<'_>) -> Result<TransferClosureDecisionV1, TransferClosureErrorV1> {
    if self.seen_authority_edges != REQUIRED_AUTHORITY_EDGES {
      return Err(TransferClosureErrorV1::AuthorityIncomplete);
    }
    let decision = match self.resolver.transfer_policy(subject, self.operation)? {
      SystemFamilyPolicyDecisionV1::Ordinary => {
        self.included_items += 1;
        TransferClosureDecisionV1::IncludeOrdinary
      }
      SystemFamilyPolicyDecisionV1::StructuralContainer => {
        self.structural_containers += 1;
        TransferClosureDecisionV1::TraverseStructuralContainer
      }
      SystemFamilyPolicyDecisionV1::Known {
        family_id,
        policy: policy @ (TransferPolicyV1::RequiredInclude | TransferPolicyV1::OptionalValidated),
      } => {
        self.included_items += 1;
        TransferClosureDecisionV1::IncludeKnown { family_id, policy }
      }
      SystemFamilyPolicyDecisionV1::Known {
        family_id,
        policy:
          policy @ (TransferPolicyV1::OmitDeclared
          | TransferPolicyV1::NodeLocal
          | TransferPolicyV1::RedactOmit
          | TransferPolicyV1::NamedSubsetOnly),
      } => {
        self.omitted_items += 1;
        TransferClosureDecisionV1::OmitKnown { family_id, policy }
      }
      SystemFamilyPolicyDecisionV1::Known { family_id, policy: TransferPolicyV1::FailUnknown } => {
        return Err(TransferClosureErrorV1::TransferRefused { family_id, operation: self.operation });
      }
    };
    self.payload_started = true;
    self.processed_items += 1;
    Ok(decision)
  }
}

const fn authority_edge_bit(role: RootAuthorityReferenceRoleV1) -> u8 {
  match role {
    RootAuthorityReferenceRoleV1::NamespaceRoot => NAMESPACE_ROOT_EDGE,
    RootAuthorityReferenceRoleV1::NamespaceTreeRoot => NAMESPACE_TREE_EDGE,
    RootAuthorityReferenceRoleV1::SemanticStateRoot => SEMANTIC_STATE_EDGE,
    RootAuthorityReferenceRoleV1::RootAdmissionCommit => ROOT_ADMISSION_EDGE,
  }
}

fn expected_authority_identity(authority: &ImmutableNamespaceAuthorityV1, role: RootAuthorityReferenceRoleV1) -> &[u8] {
  match role {
    RootAuthorityReferenceRoleV1::NamespaceRoot => &authority.root.root_hash,
    RootAuthorityReferenceRoleV1::NamespaceTreeRoot => &authority.root.namespace_tree_root,
    RootAuthorityReferenceRoleV1::SemanticStateRoot => &authority.root.semantic_state_root,
    // RootAdmissionCommit controls are keyed by the admitted NamespaceRoot.
    RootAuthorityReferenceRoleV1::RootAdmissionCommit => &authority.admission.namespace_root,
  }
}
