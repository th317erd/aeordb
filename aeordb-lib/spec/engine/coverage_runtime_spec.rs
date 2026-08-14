use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::coverage_runtime::{
  CoverageAuthorityV1, CoverageBoundaryV1, CoverageControlIdentityV1, CoverageGapReasonV1, CoverageMutationV1, CoverageObservationV1,
  CoverageReconciliationV1, CoverageRuntimeErrorV1, CoverageTrackerV1, SoftMutationAdmissionV1, SoftMutationHubOptionsV1,
  SoftMutationHubV1, SoftMutationLossReasonV1,
};
use aeordb::engine::v4::index_manifest::CoverageVersionV1;
use aeordb::engine::namespace_mutation::{NamespaceMutationAcknowledgement, NamespaceMutationKind, NamespaceMutationSourceIdentity};
use uuid::Uuid;

fn hash(byte: u8) -> Vec<u8> {
  vec![byte; HashAlgorithm::Blake3_256.hash_length()]
}

fn authority(root: u8, controls: &[(u16, u8)]) -> CoverageAuthorityV1 {
  CoverageAuthorityV1::new(
    HashAlgorithm::Blake3_256,
    hash(root),
    controls.iter().map(|(domain, identity)| CoverageControlIdentityV1 { domain: *domain, identity: hash(*identity) }).collect(),
  )
  .unwrap()
}

fn mutation(id: u8, sequence: u64, before: CoverageAuthorityV1, after: CoverageAuthorityV1) -> CoverageMutationV1 {
  CoverageMutationV1::new([id; 16], sequence, before, after).unwrap()
}

fn acknowledgement(sequence: u64, path: &str) -> NamespaceMutationAcknowledgement {
  NamespaceMutationAcknowledgement {
    operation_id: Uuid::from_bytes([sequence as u8; 16]),
    kind: NamespaceMutationKind::FileWrite,
    publication_sequence: sequence,
    previous_root_hash: hash(sequence as u8),
    root_hash: hash(sequence.saturating_add(1) as u8),
    source_identities: vec![NamespaceMutationSourceIdentity {
      path: path.to_string(),
      entry_type: Some(1),
      previous_identity: Some(hash(sequence as u8)),
      new_identity: Some(hash(sequence.saturating_add(1) as u8)),
    }],
    locator_replacements: Vec::new(),
  }
}

#[test]
fn manifest_coverage_uses_portable_epoch_and_publication_names_without_changing_shape() {
  let root = hash(1);
  let epoch = [2u8; 16];
  let coverage = CoverageVersionV1 { source_namespace_root: &root, coverage_epoch_id: &epoch, coverage_publication_sequence: 41 };

  assert_eq!(coverage.source_namespace_root, root);
  assert_eq!(coverage.coverage_epoch_id, epoch);
  assert_eq!(coverage.coverage_publication_sequence, 41);
}

#[test]
fn ordered_mutations_advance_exact_authority_across_unrelated_global_sequence_gaps() {
  let a = authority(1, &[(1, 11)]);
  let b = authority(2, &[(1, 11)]);
  let c = authority(3, &[(1, 12)]);
  let mut tracker = CoverageTrackerV1::new([7; 16], CoverageBoundaryV1::new(a.clone(), 10).unwrap()).unwrap();

  assert!(matches!(
    tracker.observe(mutation(1, 14, a, b.clone())),
    CoverageObservationV1::Applied(CoverageBoundaryV1 { publication_sequence: 14, .. })
  ));
  assert!(matches!(
    tracker.observe(mutation(2, 29, b, c.clone())),
    CoverageObservationV1::Applied(CoverageBoundaryV1 { publication_sequence: 29, .. })
  ));
  assert_eq!(tracker.covered().authority, c);
  assert_eq!(tracker.covered().publication_sequence, 29);
  assert!(!tracker.requires_reconciliation());
}

#[test]
fn duplicate_delivery_is_idempotent_but_loss_reordering_and_branching_latch_reconciliation() {
  let a = authority(1, &[]);
  let b = authority(2, &[]);
  let c = authority(3, &[]);
  let first = mutation(1, 11, a.clone(), b.clone());
  let mut tracker = CoverageTrackerV1::new([7; 16], CoverageBoundaryV1::new(a.clone(), 10).unwrap()).unwrap();

  assert!(matches!(tracker.observe(first.clone()), CoverageObservationV1::Applied(_)));
  assert_eq!(tracker.observe(first), CoverageObservationV1::Duplicate);

  let missing_middle = mutation(3, 13, c.clone(), authority(4, &[]));
  assert_eq!(tracker.observe(missing_middle), CoverageObservationV1::ReconciliationRequired(CoverageGapReasonV1::AuthorityDiscontinuity));
  assert!(tracker.requires_reconciliation());

  assert_eq!(tracker.observe(mutation(2, 12, b, c)), CoverageObservationV1::ReconciliationRequired(CoverageGapReasonV1::AlreadyLatched));
}

#[test]
fn a_reused_mutation_id_with_different_authority_is_not_accepted_as_a_duplicate() {
  let a = authority(1, &[]);
  let b = authority(2, &[]);
  let c = authority(3, &[]);
  let first = mutation(1, 11, a.clone(), b.clone());
  let mut tracker = CoverageTrackerV1::new([7; 16], CoverageBoundaryV1::new(a.clone(), 10).unwrap()).unwrap();

  assert!(matches!(tracker.observe(first), CoverageObservationV1::Applied(_)));
  assert_eq!(
    tracker.observe(mutation(1, 11, a, c)),
    CoverageObservationV1::ReconciliationRequired(CoverageGapReasonV1::ConflictingDuplicate)
  );
  assert_eq!(tracker.covered().authority, b);
}

#[test]
fn queue_loss_and_restart_compare_exact_roots_and_controls_instead_of_inventing_empty_delta() {
  let covered = authority(1, &[(1, 11), (2, 21)]);
  let changed_control = authority(1, &[(1, 11), (2, 22)]);
  let boundary = CoverageBoundaryV1::new(covered.clone(), 20).unwrap();
  let mut tracker = CoverageTrackerV1::new([9; 16], boundary.clone()).unwrap();

  tracker.mark_soft_state_lost(31);
  assert_eq!(
    tracker.reconcile_against(&changed_control, 31).unwrap(),
    CoverageReconciliationV1::BoundedDiffRequired { from: boundary.clone(), to: changed_control.clone(), authority_sequence: 31 }
  );

  let restarted = CoverageTrackerV1::new([9; 16], boundary.clone()).unwrap();
  assert_eq!(
    restarted.reconcile_against(&covered, 44).unwrap(),
    CoverageReconciliationV1::AlreadyExact { covered: boundary, authority_sequence: 44 }
  );
  assert!(matches!(restarted.reconcile_against(&changed_control, 44).unwrap(), CoverageReconciliationV1::BoundedDiffRequired { .. }));
}

#[test]
fn deterministic_model_agrees_for_long_chains_duplicates_and_one_removed_transition() {
  let start = authority(1, &[(1, 10)]);
  let mut tracker = CoverageTrackerV1::new([5; 16], CoverageBoundaryV1::new(start.clone(), 1).unwrap()).unwrap();
  let mut model_authority = start;
  let mut model_sequence = 1u64;

  for step in 2u8..=96 {
    let next = authority(step, &[(1, step.wrapping_add(10))]);
    let sequence = model_sequence + u64::from(step % 5) + 1;
    let notice = mutation(step, sequence, model_authority.clone(), next.clone());
    assert!(matches!(tracker.observe(notice.clone()), CoverageObservationV1::Applied(_)));
    assert_eq!(tracker.observe(notice), CoverageObservationV1::Duplicate);
    model_authority = next;
    model_sequence = sequence;
  }
  assert_eq!(tracker.covered(), &CoverageBoundaryV1::new(model_authority.clone(), model_sequence).unwrap());

  let skipped_target = authority(98, &[(1, 108)]);
  let after_skipped = authority(99, &[(1, 109)]);
  let skipped_sequence = model_sequence + 3;
  let after_sequence = skipped_sequence + 4;
  assert_eq!(
    tracker.observe(mutation(99, after_sequence, skipped_target.clone(), after_skipped.clone())),
    CoverageObservationV1::ReconciliationRequired(CoverageGapReasonV1::AuthorityDiscontinuity)
  );
  assert_eq!(
    tracker.reconcile_against(&after_skipped, after_sequence).unwrap(),
    CoverageReconciliationV1::BoundedDiffRequired {
      from: CoverageBoundaryV1::new(model_authority, model_sequence).unwrap(),
      to: after_skipped,
      authority_sequence: after_sequence,
    }
  );
}

#[test]
fn malformed_or_ambiguous_boundaries_fail_before_becoming_coverage_authority() {
  let zero_root = CoverageAuthorityV1::new(HashAlgorithm::Blake3_256, vec![0; 32], vec![]).unwrap_err();
  assert_eq!(zero_root, CoverageRuntimeErrorV1::ZeroNamespaceRoot);

  let wrong_width = CoverageAuthorityV1::new(HashAlgorithm::Blake3_256, vec![1; 31], vec![]).unwrap_err();
  assert_eq!(wrong_width, CoverageRuntimeErrorV1::InvalidNamespaceRootWidth { expected: 32, actual: 31 });

  let unordered = CoverageAuthorityV1::new(
    HashAlgorithm::Blake3_256,
    hash(1),
    vec![CoverageControlIdentityV1 { domain: 2, identity: hash(2) }, CoverageControlIdentityV1 { domain: 1, identity: hash(1) }],
  )
  .unwrap_err();
  assert_eq!(unordered, CoverageRuntimeErrorV1::ControlIdentitiesNotStrictlyOrdered);

  assert_eq!(CoverageBoundaryV1::new(authority(1, &[]), 0).unwrap_err(), CoverageRuntimeErrorV1::ZeroPublicationSequence);
  assert_eq!(
    CoverageTrackerV1::new([0; 16], CoverageBoundaryV1::new(authority(1, &[]), 1).unwrap()).unwrap_err(),
    CoverageRuntimeErrorV1::ZeroCoverageEpoch
  );
  assert_eq!(
    CoverageMutationV1::new([0; 16], 2, authority(1, &[]), authority(2, &[])).unwrap_err(),
    CoverageRuntimeErrorV1::ZeroMutationId
  );
}

#[test]
fn bounded_soft_hub_preserves_exact_notice_data_and_drain_accounting() {
  let hub = SoftMutationHubV1::new(SoftMutationHubOptionsV1::new(4, 8_192, 4_096).unwrap()).unwrap();
  let first = acknowledgement(11, "/docs/first.txt");
  let second = acknowledgement(14, "/docs/second.txt");

  assert_eq!(hub.offer_acknowledgement(&first), SoftMutationAdmissionV1::Accepted);
  assert_eq!(hub.offer_acknowledgement(&second), SoftMutationAdmissionV1::Accepted);
  let before = hub.snapshot().unwrap();
  assert_eq!(before.queued_notices, 2);
  assert!(before.retained_bytes > 0);
  assert!(!before.reconciliation_required);

  let drained = hub.try_drain(4, 8_192).unwrap();
  assert_eq!(drained.notices.len(), 2);
  assert_eq!(drained.notices[0].operation_id, *first.operation_id.as_bytes());
  assert_eq!(drained.notices[0].source_identities, first.source_identities);
  assert_eq!(drained.notices[1].publication_sequence, second.publication_sequence);
  assert_eq!(drained.retained_bytes, before.retained_bytes);
  assert_eq!(hub.snapshot().unwrap().queued_notices, 0);
}

#[test]
fn queue_capacity_and_oversized_notices_latch_reconciliation_without_partial_admission() {
  let full = SoftMutationHubV1::new(SoftMutationHubOptionsV1::new(1, 8_192, 4_096).unwrap()).unwrap();
  assert_eq!(full.offer_acknowledgement(&acknowledgement(20, "/a")), SoftMutationAdmissionV1::Accepted);
  assert_eq!(
    full.offer_acknowledgement(&acknowledgement(21, "/b")),
    SoftMutationAdmissionV1::ReconciliationRequired(SoftMutationLossReasonV1::QueueFull)
  );
  let full_snapshot = full.snapshot().unwrap();
  assert_eq!(full_snapshot.queued_notices, 1);
  assert_eq!(full_snapshot.lost_through_sequence, Some(21));
  assert_eq!(full_snapshot.dropped_notices, 1);

  let tiny = SoftMutationHubV1::new(SoftMutationHubOptionsV1::new(2, 256, 128).unwrap()).unwrap();
  assert_eq!(
    tiny.offer_acknowledgement(&acknowledgement(31, &format!("/{}", "x".repeat(256)))),
    SoftMutationAdmissionV1::ReconciliationRequired(SoftMutationLossReasonV1::NoticeTooLarge)
  );
  let tiny_snapshot = tiny.snapshot().unwrap();
  assert_eq!(tiny_snapshot.queued_notices, 0);
  assert_eq!(tiny_snapshot.lost_through_sequence, Some(31));
  assert!(tiny_snapshot.loss_reasons.contains(&SoftMutationLossReasonV1::NoticeTooLarge));

  assert_eq!(tiny.offer_acknowledgement(&acknowledgement(35, "/later")), SoftMutationAdmissionV1::ReconciliationAlreadyRequired);
  assert_eq!(tiny.snapshot().unwrap().lost_through_sequence, Some(35));
}

#[test]
fn soft_hub_options_and_drain_limits_reject_ambiguous_or_unbounded_requests() {
  assert!(SoftMutationHubOptionsV1::new(0, 1, 1).is_err());
  assert!(SoftMutationHubOptionsV1::new(1, 0, 1).is_err());
  assert!(SoftMutationHubOptionsV1::new(1, 8, 9).is_err());

  let hub = SoftMutationHubV1::new(SoftMutationHubOptionsV1::new(2, 8_192, 4_096).unwrap()).unwrap();
  assert!(hub.try_drain(0, 8_192).is_err());
  assert!(hub.try_drain(1, 0).is_err());

  assert_eq!(
    hub.offer_acknowledgement(&acknowledgement(0, "/invalid-sequence")),
    SoftMutationAdmissionV1::ReconciliationRequired(SoftMutationLossReasonV1::InvalidNotice)
  );
  let malformed = hub.snapshot().unwrap();
  assert!(malformed.reconciliation_required);
  assert_eq!(malformed.lost_through_sequence, None);
  assert!(malformed.loss_reasons.contains(&SoftMutationLossReasonV1::InvalidNotice));

  let drain_hub = SoftMutationHubV1::new(SoftMutationHubOptionsV1::new(2, 8_192, 4_096).unwrap()).unwrap();
  assert_eq!(drain_hub.offer_acknowledgement(&acknowledgement(41, "/drain-limit")), SoftMutationAdmissionV1::Accepted);
  assert!(drain_hub.try_drain(1, 1).is_err());
  assert_eq!(drain_hub.snapshot().unwrap().queued_notices, 1);
}
