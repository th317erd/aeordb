use std::sync::{Arc, Barrier};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::namespace_mutation::{NamespaceMutationAcknowledgement, NamespaceMutationKind, NamespaceMutationSourceIdentity};
use aeordb::engine::v4::coverage_journal::{
  CoverageAuthorityReconciliationOutcomeV1, CoverageAuthoritySelectionV1, CoverageControlDomainV1, CoverageJournalEncodeOptionsV1,
  CoverageJournalReplayExpectationV1, CoverageJournalReplayOptionsV1, CoverageJournalReplayOutcomeV1, CoverageJournalWindowOptionsV1,
  CoverageJournalWindowOutcomeV1, CoverageRebuildReasonV1, build_coverage_authority, encode_soft_mutation_journal_segment,
  order_soft_mutation_window, reconcile_authority_selection, replay_system_journal_chain,
};
use aeordb::engine::v4::coverage_runtime::{
  CoverageAuthorityV1, CoverageBoundaryV1, CoverageReconciliationV1, CoverageTrackerV1, SoftMutationAdmissionV1, SoftMutationHubOptionsV1,
  SoftMutationHubV1, SoftMutationReconciliationClearV1,
};
use aeordb::engine::v4::index_task::{JournalOwnerKindV1, MutationKindV1, decode_mutation_journal};
use aeordb::engine::v4::namespace::{NamespaceRootV1, SemanticAvailabilityV1, SemanticStateV1};
use aeordb::engine::v4::system_family::embedded_system_family_registry;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn hash(byte: u8) -> Vec<u8> {
  vec![byte; HashAlgorithm::Blake3_256.hash_length()]
}

fn semantic_state(byte: u8) -> SemanticStateV1 {
  SemanticStateV1 {
    object_id: hash(byte),
    required_capabilities: [0; 32],
    semantic_catalog_codec: 1,
    semantic_definition_codec: 1,
    compiler_profile_version: 1,
    availability: SemanticAvailabilityV1::ContentOnly {
      reason: aeordb::engine::v4::namespace::SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured,
    },
  }
}

fn namespace_root(root: u8, tree: u8, semantic: u8) -> NamespaceRootV1 {
  NamespaceRootV1 {
    root_hash: hash(root),
    required_capabilities: [0; 32],
    namespace_tree_codec: 1,
    semantic_state_codec: 1,
    namespace_tree_root: hash(tree),
    semantic_state_root: hash(semantic),
  }
}

fn acknowledgement(sequence: u64, before: u8, after: u8, path: &str) -> NamespaceMutationAcknowledgement {
  NamespaceMutationAcknowledgement {
    operation_id: Uuid::from_bytes([sequence as u8; 16]),
    kind: NamespaceMutationKind::FileWrite,
    publication_sequence: sequence,
    previous_root_hash: hash(before),
    root_hash: hash(after),
    source_identities: vec![NamespaceMutationSourceIdentity {
      path: path.to_string(),
      entry_type: Some(1),
      previous_identity: Some(hash(before)),
      new_identity: Some(hash(after)),
    }],
    locator_replacements: Vec::new(),
  }
}

fn drain_notices(acknowledgements: &[NamespaceMutationAcknowledgement]) -> Vec<aeordb::engine::v4::coverage_runtime::SoftMutationNoticeV1> {
  let hub = SoftMutationHubV1::new(SoftMutationHubOptionsV1::new(32, 64 * 1_024, 8 * 1_024).unwrap()).unwrap();
  for acknowledgement in acknowledgements {
    assert_eq!(hub.offer_acknowledgement(acknowledgement), SoftMutationAdmissionV1::Accepted);
  }
  hub.try_drain(32, 64 * 1_024).unwrap().notices
}

#[test]
fn coverage_authority_is_derived_from_exact_root_semantic_and_system_family_identity() {
  let root = namespace_root(1, 2, 3);
  let semantic = semantic_state(3);
  let registry = embedded_system_family_registry(HashAlgorithm::Blake3_256).unwrap();
  let authority = build_coverage_authority(HashAlgorithm::Blake3_256, &root, &semantic, registry).unwrap();

  assert_eq!(authority.source_namespace_root, hash(1));
  assert_eq!(authority.control_identities.len(), 2);
  assert_eq!(authority.control_identities[0].domain, CoverageControlDomainV1::SemanticStateRoot as u16);
  assert_eq!(authority.control_identities[0].identity, hash(3));
  assert_eq!(authority.control_identities[1].domain, CoverageControlDomainV1::SystemFamilySemanticProjection as u16);
  assert_eq!(authority.control_identities[1].identity, registry.semantic_projection_fingerprint);

  let mismatched = semantic_state(4);
  assert!(build_coverage_authority(HashAlgorithm::Blake3_256, &root, &mismatched, registry).is_err());

  let sha_registry = embedded_system_family_registry(HashAlgorithm::Sha512).unwrap();
  let sha_semantic = SemanticStateV1 { object_id: vec![3; HashAlgorithm::Sha512.hash_length()], ..semantic_state(3) };
  let sha_root = NamespaceRootV1 {
    root_hash: vec![1; HashAlgorithm::Sha512.hash_length()],
    namespace_tree_root: vec![2; HashAlgorithm::Sha512.hash_length()],
    semantic_state_root: sha_semantic.object_id.clone(),
    ..namespace_root(1, 2, 3)
  };
  let sha_authority = build_coverage_authority(HashAlgorithm::Sha512, &sha_root, &sha_semantic, sha_registry).unwrap();
  assert_eq!(sha_authority.source_namespace_root.len(), 64);
  assert!(sha_authority.control_identities.iter().all(|identity| identity.identity.len() == 64));
}

#[test]
fn authority_selection_never_turns_missing_ambiguous_or_corrupt_state_into_an_empty_delta() {
  let registry = embedded_system_family_registry(HashAlgorithm::Blake3_256).unwrap();
  let root = namespace_root(1, 2, 3);
  let authority = build_coverage_authority(HashAlgorithm::Blake3_256, &root, &semantic_state(3), registry).unwrap();
  let boundary = CoverageBoundaryV1::new(authority, 10).unwrap();
  let tracker = CoverageTrackerV1::new([7; 16], boundary).unwrap();

  for (selection, expected) in [
    (CoverageAuthoritySelectionV1::Missing, CoverageRebuildReasonV1::AuthorityMissing),
    (CoverageAuthoritySelectionV1::Ambiguous, CoverageRebuildReasonV1::AuthorityAmbiguous),
    (CoverageAuthoritySelectionV1::Corrupt, CoverageRebuildReasonV1::AuthorityCorrupt),
  ] {
    assert_eq!(reconcile_authority_selection(&tracker, selection), CoverageAuthorityReconciliationOutcomeV1::rebuild(expected));
  }
  assert_eq!(
    reconcile_authority_selection(&tracker, CoverageAuthoritySelectionV1::Canceled),
    CoverageAuthorityReconciliationOutcomeV1::Canceled
  );
}

#[test]
fn bounded_window_reorders_postcommit_delivery_by_publication_sequence_and_root_chain() {
  let registry = embedded_system_family_registry(HashAlgorithm::Blake3_256).unwrap();
  let before_root = namespace_root(1, 9, 3);
  let after_root = namespace_root(4, 10, 3);
  let before = build_coverage_authority(HashAlgorithm::Blake3_256, &before_root, &semantic_state(3), registry).unwrap();
  let after = build_coverage_authority(HashAlgorithm::Blake3_256, &after_root, &semantic_state(3), registry).unwrap();
  let covered = CoverageBoundaryV1::new(before, 5).unwrap();
  let selected = CoverageBoundaryV1::new(after, 22).unwrap();
  let notices = drain_notices(&[
    acknowledgement(22, 3, 4, "/docs/c.txt"),
    acknowledgement(9, 1, 2, "/docs/a.txt"),
    acknowledgement(17, 2, 3, "/docs/b.txt"),
  ]);

  let outcome = order_soft_mutation_window(
    HashAlgorithm::Blake3_256,
    notices,
    &covered,
    &selected,
    CoverageJournalWindowOptionsV1::new(8, 64 * 1_024).unwrap(),
  );
  let CoverageJournalWindowOutcomeV1::Exact(window) = outcome else {
    panic!("expected an exact reordered window");
  };
  assert_eq!(window.notices().iter().map(|notice| notice.publication_sequence).collect::<Vec<_>>(), vec![9, 17, 22]);
  assert_eq!(window.root_before(), hash(1));
  assert_eq!(window.root_after(), hash(4));
}

#[test]
fn namespace_notices_cannot_claim_exact_coverage_across_a_semantic_control_change() {
  let registry = embedded_system_family_registry(HashAlgorithm::Blake3_256).unwrap();
  let before = build_coverage_authority(HashAlgorithm::Blake3_256, &namespace_root(1, 9, 3), &semantic_state(3), registry).unwrap();
  let after = build_coverage_authority(HashAlgorithm::Blake3_256, &namespace_root(2, 10, 4), &semantic_state(4), registry).unwrap();
  let covered = CoverageBoundaryV1::new(before, 5).unwrap();
  let selected = CoverageBoundaryV1::new(after, 9).unwrap();

  assert_eq!(
    order_soft_mutation_window(
      HashAlgorithm::Blake3_256,
      drain_notices(&[acknowledgement(9, 1, 2, "/docs/a.txt")]),
      &covered,
      &selected,
      CoverageJournalWindowOptionsV1::new(8, 64 * 1_024).unwrap(),
    ),
    CoverageJournalWindowOutcomeV1::BoundedDiffRequired { reason: CoverageRebuildReasonV1::AuthorityDiscontinuity }
  );
}

#[test]
fn bounded_window_requests_reconciliation_for_gaps_conflicts_and_pressure() {
  let registry = embedded_system_family_registry(HashAlgorithm::Blake3_256).unwrap();
  let before = build_coverage_authority(HashAlgorithm::Blake3_256, &namespace_root(1, 9, 3), &semantic_state(3), registry).unwrap();
  let after = build_coverage_authority(HashAlgorithm::Blake3_256, &namespace_root(4, 10, 3), &semantic_state(3), registry).unwrap();
  let covered = CoverageBoundaryV1::new(before, 5).unwrap();
  let selected = CoverageBoundaryV1::new(after, 22).unwrap();

  let missing = drain_notices(&[acknowledgement(22, 3, 4, "/docs/c.txt")]);
  assert!(matches!(
    order_soft_mutation_window(
      HashAlgorithm::Blake3_256,
      missing,
      &covered,
      &selected,
      CoverageJournalWindowOptionsV1::new(8, 64 * 1_024).unwrap(),
    ),
    CoverageJournalWindowOutcomeV1::BoundedDiffRequired { .. }
  ));

  let over_count = drain_notices(&[acknowledgement(9, 1, 2, "/docs/a.txt"), acknowledgement(17, 2, 3, "/docs/b.txt")]);
  assert_eq!(
    order_soft_mutation_window(
      HashAlgorithm::Blake3_256,
      over_count,
      &covered,
      &selected,
      CoverageJournalWindowOptionsV1::new(1, 64 * 1_024).unwrap(),
    ),
    CoverageJournalWindowOutcomeV1::RebuildRequired(CoverageRebuildReasonV1::WindowLimitExceeded)
  );

  let duplicate = acknowledgement(9, 1, 2, "/docs/a.txt");
  let duplicated = drain_notices(&[duplicate.clone(), duplicate]);
  let duplicate_selected = CoverageBoundaryV1::new(
    build_coverage_authority(HashAlgorithm::Blake3_256, &namespace_root(2, 10, 3), &semantic_state(3), registry).unwrap(),
    9,
  )
  .unwrap();
  let CoverageJournalWindowOutcomeV1::Exact(deduplicated) = order_soft_mutation_window(
    HashAlgorithm::Blake3_256,
    duplicated,
    &covered,
    &duplicate_selected,
    CoverageJournalWindowOptionsV1::new(8, 64 * 1_024).unwrap(),
  ) else {
    panic!("expected exact duplicate delivery to collapse");
  };
  assert_eq!(deduplicated.notices().len(), 1);

  let first = acknowledgement(9, 1, 2, "/docs/a.txt");
  let mut conflict = acknowledgement(17, 2, 3, "/docs/b.txt");
  conflict.operation_id = first.operation_id;
  assert_eq!(
    order_soft_mutation_window(
      HashAlgorithm::Blake3_256,
      drain_notices(&[first, conflict]),
      &covered,
      &selected,
      CoverageJournalWindowOptionsV1::new(8, 64 * 1_024).unwrap(),
    ),
    CoverageJournalWindowOutcomeV1::BoundedDiffRequired { reason: CoverageRebuildReasonV1::ConflictingMutation }
  );
}

#[test]
fn exact_window_encodes_the_frozen_system_journal_without_uuid_or_batch_ambiguity() {
  let registry = embedded_system_family_registry(HashAlgorithm::Blake3_256).unwrap();
  let before = build_coverage_authority(HashAlgorithm::Blake3_256, &namespace_root(1, 9, 3), &semantic_state(3), registry).unwrap();
  let after = build_coverage_authority(HashAlgorithm::Blake3_256, &namespace_root(2, 10, 3), &semantic_state(3), registry).unwrap();
  let covered = CoverageBoundaryV1::new(before, 5).unwrap();
  let selected = CoverageBoundaryV1::new(after, 9).unwrap();
  let mut acknowledgement = acknowledgement(9, 1, 2, "/docs/a.txt");
  acknowledgement.source_identities.push(NamespaceMutationSourceIdentity {
    path: "/docs/b.txt".to_string(),
    entry_type: Some(1),
    previous_identity: None,
    new_identity: Some(hash(7)),
  });
  let notices = drain_notices(&[acknowledgement]);
  let CoverageJournalWindowOutcomeV1::Exact(window) = order_soft_mutation_window(
    HashAlgorithm::Blake3_256,
    notices,
    &covered,
    &selected,
    CoverageJournalWindowOptionsV1::new(8, 64 * 1_024).unwrap(),
  ) else {
    panic!("expected exact window");
  };
  let encoded = encode_soft_mutation_journal_segment(
    HashAlgorithm::Blake3_256,
    &window,
    CoverageJournalEncodeOptionsV1 { generation: 7, segment_ordinal: 0, previous_segment: vec![0; 32], runtime_boot_id: [8; 16] },
  )
  .unwrap();
  let decoded = decode_mutation_journal(&encoded.value, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(decoded.owner_kind, JournalOwnerKindV1::System);
  assert_eq!(decoded.records.len(), 2);
  let records = decoded.records.iter().collect::<Result<Vec<_>, _>>().unwrap();
  assert_eq!(records[0].batch_count, 2);
  assert_eq!(records[0].kind, MutationKindV1::Update);
  assert_eq!(records[1].kind, MutationKindV1::Create);
  assert_eq!(records[0].committed_at_ms, window.notices()[0].committed_at_ms);
  assert_eq!(records[0].mutation_id.len(), HashAlgorithm::Blake3_256.hash_length());
}

#[test]
fn journal_encoding_requires_the_semantic_identity_from_the_selected_authority() {
  let covered = CoverageBoundaryV1::new(CoverageAuthorityV1::new(HashAlgorithm::Blake3_256, hash(1), vec![]).unwrap(), 5).unwrap();
  let selected = CoverageBoundaryV1::new(CoverageAuthorityV1::new(HashAlgorithm::Blake3_256, hash(2), vec![]).unwrap(), 9).unwrap();
  let CoverageJournalWindowOutcomeV1::Exact(window) = order_soft_mutation_window(
    HashAlgorithm::Blake3_256,
    drain_notices(&[acknowledgement(9, 1, 2, "/docs/a.txt")]),
    &covered,
    &selected,
    CoverageJournalWindowOptionsV1::new(8, 64 * 1_024).unwrap(),
  ) else {
    panic!("expected exact root ordering");
  };

  assert!(encode_soft_mutation_journal_segment(
    HashAlgorithm::Blake3_256,
    &window,
    CoverageJournalEncodeOptionsV1 { generation: 7, segment_ordinal: 0, previous_segment: vec![0; 32], runtime_boot_id: [8; 16] },
  )
  .is_err());
}

#[test]
fn whole_root_notice_requires_authoritative_diff_instead_of_a_synthetic_file_record() {
  let mut whole_root = acknowledgement(9, 1, 2, "/unused");
  whole_root.kind = NamespaceMutationKind::Promote;
  whole_root.source_identities.clear();
  let notices = drain_notices(&[whole_root]);
  let registry = embedded_system_family_registry(HashAlgorithm::Blake3_256).unwrap();
  let before = build_coverage_authority(HashAlgorithm::Blake3_256, &namespace_root(1, 9, 3), &semantic_state(3), registry).unwrap();
  let after = build_coverage_authority(HashAlgorithm::Blake3_256, &namespace_root(2, 10, 3), &semantic_state(3), registry).unwrap();
  assert_eq!(
    order_soft_mutation_window(
      HashAlgorithm::Blake3_256,
      notices,
      &CoverageBoundaryV1::new(before, 5).unwrap(),
      &CoverageBoundaryV1::new(after, 9).unwrap(),
      CoverageJournalWindowOptionsV1::new(8, 64 * 1_024).unwrap(),
    ),
    CoverageJournalWindowOutcomeV1::BoundedDiffRequired { reason: CoverageRebuildReasonV1::WholeRootTransition }
  );
}

#[test]
fn replay_is_bounded_chain_checked_and_never_treats_missing_or_corrupt_bytes_as_empty() {
  let cancellation = CancellationToken::new();
  let options = CoverageJournalReplayOptionsV1::new(8, 1_024 * 1_024).unwrap();
  let expectation = CoverageJournalReplayExpectationV1 {
    generation: 7,
    first_segment_ordinal: 0,
    previous_segment: vec![0; 32],
    source_root_before: hash(1),
  };
  assert_eq!(
    replay_system_journal_chain(HashAlgorithm::Blake3_256, &[], &expectation, options, &cancellation),
    CoverageJournalReplayOutcomeV1::rebuild(CoverageRebuildReasonV1::JournalMissing)
  );
  assert!(matches!(
    replay_system_journal_chain(HashAlgorithm::Blake3_256, &[vec![0xde, 0xad]], &expectation, options, &cancellation),
    CoverageJournalReplayOutcomeV1::RebuildRequired { reason: CoverageRebuildReasonV1::JournalCorrupt, evidence: Some(_) }
  ));

  let registry = embedded_system_family_registry(HashAlgorithm::Blake3_256).unwrap();
  let root_one = build_coverage_authority(HashAlgorithm::Blake3_256, &namespace_root(1, 9, 3), &semantic_state(3), registry).unwrap();
  let root_two = build_coverage_authority(HashAlgorithm::Blake3_256, &namespace_root(2, 10, 3), &semantic_state(3), registry).unwrap();
  let root_three = build_coverage_authority(HashAlgorithm::Blake3_256, &namespace_root(3, 11, 3), &semantic_state(3), registry).unwrap();
  let CoverageJournalWindowOutcomeV1::Exact(first_window) = order_soft_mutation_window(
    HashAlgorithm::Blake3_256,
    drain_notices(&[acknowledgement(9, 1, 2, "/docs/a.txt")]),
    &CoverageBoundaryV1::new(root_one, 5).unwrap(),
    &CoverageBoundaryV1::new(root_two.clone(), 9).unwrap(),
    CoverageJournalWindowOptionsV1::new(8, 64 * 1_024).unwrap(),
  ) else {
    panic!("expected first exact window");
  };
  let first = encode_soft_mutation_journal_segment(
    HashAlgorithm::Blake3_256,
    &first_window,
    CoverageJournalEncodeOptionsV1 { generation: 7, segment_ordinal: 0, previous_segment: vec![0; 32], runtime_boot_id: [8; 16] },
  )
  .unwrap();
  let CoverageJournalWindowOutcomeV1::Exact(second_window) = order_soft_mutation_window(
    HashAlgorithm::Blake3_256,
    drain_notices(&[acknowledgement(17, 2, 3, "/docs/b.txt")]),
    &CoverageBoundaryV1::new(root_two, 9).unwrap(),
    &CoverageBoundaryV1::new(root_three, 17).unwrap(),
    CoverageJournalWindowOptionsV1::new(8, 64 * 1_024).unwrap(),
  ) else {
    panic!("expected second exact window");
  };
  let second = encode_soft_mutation_journal_segment(
    HashAlgorithm::Blake3_256,
    &second_window,
    CoverageJournalEncodeOptionsV1 { generation: 7, segment_ordinal: 1, previous_segment: first.key.clone(), runtime_boot_id: [8; 16] },
  )
  .unwrap();
  let chain = vec![first.value, second.value];
  let CoverageJournalReplayOutcomeV1::Verified(replayed) =
    replay_system_journal_chain(HashAlgorithm::Blake3_256, &chain, &expectation, options, &cancellation)
  else {
    panic!("expected an exact replay");
  };
  assert_eq!(replayed.segment_count, 2);
  assert_eq!(replayed.record_count, 2);
  assert_eq!(replayed.first_sequence, 9);
  assert_eq!(replayed.last_sequence, 17);
  assert_eq!(replayed.source_root_after, hash(3));

  assert_eq!(
    replay_system_journal_chain(
      HashAlgorithm::Blake3_256,
      &chain,
      &expectation,
      CoverageJournalReplayOptionsV1::new(1, 1_024 * 1_024).unwrap(),
      &cancellation,
    ),
    CoverageJournalReplayOutcomeV1::rebuild(CoverageRebuildReasonV1::JournalLimitExceeded)
  );

  let wrong_generation = CoverageJournalReplayExpectationV1 { generation: 8, ..expectation.clone() };
  assert_eq!(
    replay_system_journal_chain(HashAlgorithm::Blake3_256, &chain, &wrong_generation, options, &cancellation),
    CoverageJournalReplayOutcomeV1::rebuild(CoverageRebuildReasonV1::JournalChainDiscontinuous)
  );
  cancellation.cancel();
  assert_eq!(
    replay_system_journal_chain(HashAlgorithm::Blake3_256, &[vec![0xde, 0xad]], &expectation, options, &cancellation),
    CoverageJournalReplayOutcomeV1::Canceled
  );
}

#[test]
fn loss_clear_is_generation_checked_and_a_new_drop_cannot_be_erased_by_an_older_reconciliation() {
  let hub = SoftMutationHubV1::new(SoftMutationHubOptionsV1::new(1, 8 * 1_024, 4 * 1_024).unwrap()).unwrap();
  assert_eq!(hub.offer_acknowledgement(&acknowledgement(10, 1, 2, "/a")), SoftMutationAdmissionV1::Accepted);
  assert!(matches!(hub.offer_acknowledgement(&acknowledgement(11, 2, 3, "/b")), SoftMutationAdmissionV1::ReconciliationRequired(_)));
  let stale = hub.reconciliation_token();
  assert_eq!(hub.offer_acknowledgement(&acknowledgement(12, 3, 4, "/c")), SoftMutationAdmissionV1::ReconciliationAlreadyRequired);
  let registry = embedded_system_family_registry(HashAlgorithm::Blake3_256).unwrap();
  let authority = build_coverage_authority(HashAlgorithm::Blake3_256, &namespace_root(4, 9, 3), &semantic_state(3), registry).unwrap();
  let mut behind_tracker = CoverageTrackerV1::new([7; 16], CoverageBoundaryV1::new(authority.clone(), 10).unwrap()).unwrap();
  let behind_proof = behind_tracker.accept_reconciled(CoverageBoundaryV1::new(authority.clone(), 11).unwrap()).unwrap();
  let mut current_tracker = CoverageTrackerV1::new([7; 16], CoverageBoundaryV1::new(authority.clone(), 10).unwrap()).unwrap();
  let current_proof = current_tracker.accept_reconciled(CoverageBoundaryV1::new(authority, 12).unwrap()).unwrap();

  assert_eq!(hub.try_clear_reconciliation(stale, &current_proof), SoftMutationReconciliationClearV1::Stale);
  assert!(hub.snapshot().unwrap().reconciliation_required);

  let current = hub.reconciliation_token();
  assert_eq!(hub.try_clear_reconciliation(current, &behind_proof), SoftMutationReconciliationClearV1::BoundaryBehind);
  assert_eq!(hub.try_clear_reconciliation(current, &current_proof), SoftMutationReconciliationClearV1::Cleared);
  assert!(!hub.snapshot().unwrap().reconciliation_required);
  assert_eq!(hub.offer_acknowledgement(&acknowledgement(13, 4, 5, "/d")), SoftMutationAdmissionV1::Accepted);
}

#[test]
fn concurrent_drops_advance_loss_generation_and_leave_no_inflight_evidence() {
  let hub = Arc::new(SoftMutationHubV1::new(SoftMutationHubOptionsV1::new(1, 8 * 1_024, 4 * 1_024).unwrap()).unwrap());
  assert_eq!(hub.offer_acknowledgement(&acknowledgement(10, 1, 2, "/a")), SoftMutationAdmissionV1::Accepted);
  assert!(matches!(hub.offer_acknowledgement(&acknowledgement(11, 2, 3, "/b")), SoftMutationAdmissionV1::ReconciliationRequired(_)));
  let before = hub.snapshot().unwrap();
  let barrier = Arc::new(Barrier::new(9));
  let mut threads = Vec::new();
  for sequence in 12..20 {
    let hub = Arc::clone(&hub);
    let barrier = Arc::clone(&barrier);
    threads.push(std::thread::spawn(move || {
      barrier.wait();
      assert_eq!(
        hub.offer_acknowledgement(&acknowledgement(sequence, sequence as u8, sequence as u8 + 1, "/concurrent")),
        SoftMutationAdmissionV1::ReconciliationAlreadyRequired
      );
    }));
  }
  barrier.wait();
  for thread in threads {
    thread.join().unwrap();
  }
  let after = hub.snapshot().unwrap();
  assert!(after.loss_epoch >= before.loss_epoch + 8);
  assert_eq!(after.losses_in_flight, 0);
  assert_eq!(after.lost_through_sequence, Some(19));
  assert!(after.reconciliation_required);
}

#[test]
fn selected_exact_authority_preserves_tracker_reconciliation_semantics() {
  let registry = embedded_system_family_registry(HashAlgorithm::Blake3_256).unwrap();
  let root = namespace_root(1, 2, 3);
  let authority = build_coverage_authority(HashAlgorithm::Blake3_256, &root, &semantic_state(3), registry).unwrap();
  let boundary = CoverageBoundaryV1::new(authority.clone(), 10).unwrap();
  let tracker = CoverageTrackerV1::new([7; 16], boundary.clone()).unwrap();
  assert_eq!(
    reconcile_authority_selection(&tracker, CoverageAuthoritySelectionV1::Selected(CoverageBoundaryV1::new(authority, 14).unwrap())),
    CoverageAuthorityReconciliationOutcomeV1::Verified(CoverageReconciliationV1::AlreadyExact {
      covered: boundary,
      authority_sequence: 14,
    })
  );
}
