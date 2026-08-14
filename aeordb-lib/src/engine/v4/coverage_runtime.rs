use crate::engine::HashAlgorithm;

const MAX_CONTROL_IDENTITIES: usize = 4_096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageControlIdentityV1 {
  pub domain: u16,
  pub identity: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageAuthorityV1 {
  pub source_namespace_root: Vec<u8>,
  pub control_identities: Vec<CoverageControlIdentityV1>,
}

impl CoverageAuthorityV1 {
  pub fn new(
    hash_algorithm: HashAlgorithm,
    source_namespace_root: Vec<u8>,
    control_identities: Vec<CoverageControlIdentityV1>,
  ) -> Result<Self, CoverageRuntimeErrorV1> {
    let hash_width = hash_algorithm.hash_length();
    if source_namespace_root.len() != hash_width {
      return Err(CoverageRuntimeErrorV1::InvalidNamespaceRootWidth { expected: hash_width, actual: source_namespace_root.len() });
    }
    if source_namespace_root.iter().all(|byte| *byte == 0) {
      return Err(CoverageRuntimeErrorV1::ZeroNamespaceRoot);
    }
    if control_identities.len() > MAX_CONTROL_IDENTITIES {
      return Err(CoverageRuntimeErrorV1::TooManyControlIdentities { maximum: MAX_CONTROL_IDENTITIES, actual: control_identities.len() });
    }
    let mut previous_domain = None;
    for control in &control_identities {
      if control.domain == 0 || previous_domain.is_some_and(|previous| previous >= control.domain) {
        return Err(CoverageRuntimeErrorV1::ControlIdentitiesNotStrictlyOrdered);
      }
      if control.identity.len() != hash_width {
        return Err(CoverageRuntimeErrorV1::InvalidControlIdentityWidth {
          domain: control.domain,
          expected: hash_width,
          actual: control.identity.len(),
        });
      }
      if control.identity.iter().all(|byte| *byte == 0) {
        return Err(CoverageRuntimeErrorV1::ZeroControlIdentity { domain: control.domain });
      }
      previous_domain = Some(control.domain);
    }
    Ok(Self { source_namespace_root, control_identities })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageBoundaryV1 {
  pub authority: CoverageAuthorityV1,
  pub publication_sequence: u64,
}

impl CoverageBoundaryV1 {
  pub fn new(authority: CoverageAuthorityV1, publication_sequence: u64) -> Result<Self, CoverageRuntimeErrorV1> {
    if publication_sequence == 0 {
      return Err(CoverageRuntimeErrorV1::ZeroPublicationSequence);
    }
    Ok(Self { authority, publication_sequence })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageMutationV1 {
  pub mutation_id: [u8; 16],
  pub publication_sequence: u64,
  pub before: CoverageAuthorityV1,
  pub after: CoverageAuthorityV1,
}

impl CoverageMutationV1 {
  pub fn new(
    mutation_id: [u8; 16],
    publication_sequence: u64,
    before: CoverageAuthorityV1,
    after: CoverageAuthorityV1,
  ) -> Result<Self, CoverageRuntimeErrorV1> {
    if mutation_id.iter().all(|byte| *byte == 0) {
      return Err(CoverageRuntimeErrorV1::ZeroMutationId);
    }
    if publication_sequence == 0 {
      return Err(CoverageRuntimeErrorV1::ZeroPublicationSequence);
    }
    if before == after {
      return Err(CoverageRuntimeErrorV1::MutationDoesNotChangeAuthority);
    }
    Ok(Self { mutation_id, publication_sequence, before, after })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoverageGapReasonV1 {
  SoftStateLost,
  AuthorityDiscontinuity,
  NonMonotonicPublication,
  ConflictingDuplicate,
  AlreadyLatched,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoverageObservationV1 {
  Applied(CoverageBoundaryV1),
  Duplicate,
  ReconciliationRequired(CoverageGapReasonV1),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoverageReconciliationV1 {
  AlreadyExact { covered: CoverageBoundaryV1, authority_sequence: u64 },
  BoundedDiffRequired { from: CoverageBoundaryV1, to: CoverageAuthorityV1, authority_sequence: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageTrackerV1 {
  coverage_epoch_id: [u8; 16],
  covered: CoverageBoundaryV1,
  last_mutation: Option<CoverageMutationV1>,
  reconciliation_reason: Option<CoverageGapReasonV1>,
  lost_through_sequence: Option<u64>,
}

impl CoverageTrackerV1 {
  pub fn new(coverage_epoch_id: [u8; 16], covered: CoverageBoundaryV1) -> Result<Self, CoverageRuntimeErrorV1> {
    if coverage_epoch_id.iter().all(|byte| *byte == 0) {
      return Err(CoverageRuntimeErrorV1::ZeroCoverageEpoch);
    }
    Ok(Self { coverage_epoch_id, covered, last_mutation: None, reconciliation_reason: None, lost_through_sequence: None })
  }

  pub fn coverage_epoch_id(&self) -> [u8; 16] {
    self.coverage_epoch_id
  }

  pub fn covered(&self) -> &CoverageBoundaryV1 {
    &self.covered
  }

  pub fn requires_reconciliation(&self) -> bool {
    self.reconciliation_reason.is_some()
  }

  pub fn lost_through_sequence(&self) -> Option<u64> {
    self.lost_through_sequence
  }

  pub fn observe(&mut self, mutation: CoverageMutationV1) -> CoverageObservationV1 {
    if self.reconciliation_reason.is_some() {
      return CoverageObservationV1::ReconciliationRequired(CoverageGapReasonV1::AlreadyLatched);
    }
    if self.last_mutation.as_ref().is_some_and(|previous| previous.mutation_id == mutation.mutation_id) {
      if self.last_mutation.as_ref() == Some(&mutation) {
        return CoverageObservationV1::Duplicate;
      }
      return self.latch(CoverageGapReasonV1::ConflictingDuplicate, mutation.publication_sequence);
    }
    if mutation.publication_sequence <= self.covered.publication_sequence {
      return self.latch(CoverageGapReasonV1::NonMonotonicPublication, mutation.publication_sequence);
    }
    if mutation.before != self.covered.authority {
      return self.latch(CoverageGapReasonV1::AuthorityDiscontinuity, mutation.publication_sequence);
    }

    self.covered = CoverageBoundaryV1 { authority: mutation.after.clone(), publication_sequence: mutation.publication_sequence };
    self.last_mutation = Some(mutation);
    CoverageObservationV1::Applied(self.covered.clone())
  }

  pub fn mark_soft_state_lost(&mut self, observed_sequence: u64) {
    self.reconciliation_reason.get_or_insert(CoverageGapReasonV1::SoftStateLost);
    self.lost_through_sequence = Some(self.lost_through_sequence.map_or(observed_sequence, |current| current.max(observed_sequence)));
  }

  pub fn reconcile_against(
    &self,
    selected_authority: &CoverageAuthorityV1,
    authority_sequence: u64,
  ) -> Result<CoverageReconciliationV1, CoverageRuntimeErrorV1> {
    if authority_sequence < self.covered.publication_sequence {
      return Err(CoverageRuntimeErrorV1::AuthoritySequenceRegressed {
        covered: self.covered.publication_sequence,
        authority: authority_sequence,
      });
    }
    if selected_authority == &self.covered.authority {
      return Ok(CoverageReconciliationV1::AlreadyExact { covered: self.covered.clone(), authority_sequence });
    }
    Ok(CoverageReconciliationV1::BoundedDiffRequired { from: self.covered.clone(), to: selected_authority.clone(), authority_sequence })
  }

  pub fn accept_reconciled(&mut self, boundary: CoverageBoundaryV1) -> Result<(), CoverageRuntimeErrorV1> {
    if boundary.publication_sequence < self.covered.publication_sequence {
      return Err(CoverageRuntimeErrorV1::AuthoritySequenceRegressed {
        covered: self.covered.publication_sequence,
        authority: boundary.publication_sequence,
      });
    }
    self.covered = boundary;
    self.last_mutation = None;
    self.reconciliation_reason = None;
    self.lost_through_sequence = None;
    Ok(())
  }

  fn latch(&mut self, reason: CoverageGapReasonV1, observed_sequence: u64) -> CoverageObservationV1 {
    self.reconciliation_reason = Some(reason);
    self.lost_through_sequence = Some(self.lost_through_sequence.map_or(observed_sequence, |current| current.max(observed_sequence)));
    CoverageObservationV1::ReconciliationRequired(reason)
  }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CoverageRuntimeErrorV1 {
  #[error("coverage namespace root is zero")]
  ZeroNamespaceRoot,
  #[error("coverage namespace root has width {actual}, expected {expected}")]
  InvalidNamespaceRootWidth { expected: usize, actual: usize },
  #[error("coverage contains {actual} control identities, maximum {maximum}")]
  TooManyControlIdentities { maximum: usize, actual: usize },
  #[error("coverage control identities are not strictly ordered by nonzero domain")]
  ControlIdentitiesNotStrictlyOrdered,
  #[error("coverage control identity for domain {domain} is zero")]
  ZeroControlIdentity { domain: u16 },
  #[error("coverage control identity for domain {domain} has width {actual}, expected {expected}")]
  InvalidControlIdentityWidth { domain: u16, expected: usize, actual: usize },
  #[error("coverage publication sequence is zero")]
  ZeroPublicationSequence,
  #[error("coverage epoch is zero")]
  ZeroCoverageEpoch,
  #[error("coverage mutation ID is zero")]
  ZeroMutationId,
  #[error("coverage mutation does not change namespace or control authority")]
  MutationDoesNotChangeAuthority,
  #[error("authority sequence {authority} precedes covered sequence {covered}")]
  AuthoritySequenceRegressed { covered: u64, authority: u64 },
}
