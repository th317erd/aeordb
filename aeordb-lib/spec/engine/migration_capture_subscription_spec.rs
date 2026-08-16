use std::sync::Arc;

use aeordb::engine::namespace_mutation::{NamespaceMutationAcknowledgement, NamespaceMutationKind, NamespaceMutationSourceIdentity};
use aeordb::engine::v4::coverage_runtime::{SoftMutationAdmissionV1, SoftMutationHubOptionsV1, SoftMutationHubV1, SoftMutationLossReasonV1};
use aeordb::engine::v4::migration_capture_subscription::{
  MigrationCaptureOfferV1, MigrationCaptureSubscriptionBoundaryV1, MigrationCaptureSubscriptionErrorV1,
  MigrationCaptureSubscriptionIdentityV1, MigrationCaptureSubscriptionOwnerV1, MigrationCaptureSubscriptionV1,
};
use aeordb::engine::StorageEngine;

fn identity(seed: u8, fencing_token: u64) -> MigrationCaptureSubscriptionIdentityV1 {
  MigrationCaptureSubscriptionIdentityV1::new([seed; 16], [seed.wrapping_add(1); 16], fencing_token, [seed.wrapping_add(2); 16]).unwrap()
}

fn hub(maximum_notices: usize, maximum_retained_bytes: usize, maximum_notice_bytes: usize) -> Arc<SoftMutationHubV1> {
  Arc::new(
    SoftMutationHubV1::new(SoftMutationHubOptionsV1::new(maximum_notices, maximum_retained_bytes, maximum_notice_bytes).unwrap()).unwrap(),
  )
}

fn acknowledgement(sequence: u64, previous: u8, current: u8, path: &str) -> NamespaceMutationAcknowledgement {
  NamespaceMutationAcknowledgement {
    operation_id: uuid::Uuid::from_bytes([sequence as u8; 16]),
    kind: NamespaceMutationKind::FileWrite,
    publication_sequence: sequence,
    previous_root_hash: vec![previous; 32],
    root_hash: vec![current; 32],
    source_identities: vec![NamespaceMutationSourceIdentity {
      path: path.to_string(),
      entry_type: Some(1),
      previous_identity: Some(vec![previous; 32]),
      new_identity: Some(vec![current; 32]),
    }],
    locator_replacements: Vec::new(),
  }
}

#[test]
fn subscription_ignores_only_the_registered_boundary_and_queues_future_publications() {
  let boundary = MigrationCaptureSubscriptionBoundaryV1 { source_namespace_root: vec![7; 32], publication_sequence: 41 };
  let subscription = MigrationCaptureSubscriptionV1::new(identity(1, 9), boundary.clone(), hub(4, 8_192, 4_096)).unwrap();

  assert_eq!(
    subscription.offer_acknowledgement(&acknowledgement(40, 5, 6, "/late-before")),
    MigrationCaptureOfferV1::IgnoredAtOrBeforeBoundary
  );
  assert_eq!(
    subscription.offer_acknowledgement(&acknowledgement(41, 6, 7, "/late-at")),
    MigrationCaptureOfferV1::IgnoredAtOrBeforeBoundary
  );
  assert_eq!(
    subscription.offer_acknowledgement(&acknowledgement(42, 7, 8, "/future")),
    MigrationCaptureOfferV1::Offered(SoftMutationAdmissionV1::Accepted)
  );

  assert_eq!(subscription.boundary(), &boundary);
  let snapshot = subscription.snapshot().unwrap();
  assert_eq!(snapshot.queued_notices, 1);
  assert_eq!(snapshot.latest_queued_publication_sequence, Some(42));
  assert_eq!(snapshot.dropped_notices, 0);
}

#[test]
fn subscription_pressure_latches_only_its_own_reconciliation_state() {
  let subscription = MigrationCaptureSubscriptionV1::new(
    identity(2, 10),
    MigrationCaptureSubscriptionBoundaryV1 { source_namespace_root: Vec::new(), publication_sequence: 0 },
    hub(1, 8_192, 4_096),
  )
  .unwrap();

  assert_eq!(
    subscription.offer_acknowledgement(&acknowledgement(1, 1, 2, "/first")),
    MigrationCaptureOfferV1::Offered(SoftMutationAdmissionV1::Accepted)
  );
  assert_eq!(
    subscription.offer_acknowledgement(&acknowledgement(2, 2, 3, "/second")),
    MigrationCaptureOfferV1::Offered(SoftMutationAdmissionV1::ReconciliationRequired(SoftMutationLossReasonV1::QueueFull))
  );
  assert_eq!(
    subscription.offer_acknowledgement(&acknowledgement(3, 3, 4, "/third")),
    MigrationCaptureOfferV1::Offered(SoftMutationAdmissionV1::ReconciliationAlreadyRequired)
  );

  let snapshot = subscription.snapshot().unwrap();
  assert_eq!(snapshot.queued_notices, 1);
  assert_eq!(snapshot.lost_through_sequence, Some(3));
  assert_eq!(snapshot.dropped_notices, 2);
  assert!(snapshot.loss_reasons.contains(&SoftMutationLossReasonV1::QueueFull));
}

#[test]
fn zero_sequence_is_invalid_even_when_the_registration_frontier_is_zero() {
  let subscription = MigrationCaptureSubscriptionV1::new(
    identity(12, 20),
    MigrationCaptureSubscriptionBoundaryV1 { source_namespace_root: Vec::new(), publication_sequence: 0 },
    hub(2, 8_192, 4_096),
  )
  .unwrap();

  assert_eq!(
    subscription.offer_acknowledgement(&acknowledgement(0, 1, 2, "/invalid-zero")),
    MigrationCaptureOfferV1::Offered(SoftMutationAdmissionV1::ReconciliationRequired(SoftMutationLossReasonV1::InvalidNotice))
  );
  let snapshot = subscription.snapshot().unwrap();
  assert!(snapshot.reconciliation_required);
  assert!(snapshot.loss_reasons.contains(&SoftMutationLossReasonV1::InvalidNotice));
}

#[test]
fn identity_and_initial_hub_state_fail_closed() {
  assert_eq!(
    MigrationCaptureSubscriptionIdentityV1::new([0; 16], [2; 16], 1, [3; 16]).unwrap_err().code(),
    "migration_capture_subscription_identity"
  );
  assert_eq!(
    MigrationCaptureSubscriptionIdentityV1::new([1; 16], [0; 16], 1, [3; 16]).unwrap_err().code(),
    "migration_capture_subscription_identity"
  );
  assert_eq!(
    MigrationCaptureSubscriptionIdentityV1::new([1; 16], [2; 16], 0, [3; 16]).unwrap_err().code(),
    "migration_capture_subscription_fence"
  );
  assert_eq!(
    MigrationCaptureSubscriptionIdentityV1::new([1; 16], [2; 16], 1, [0; 16]).unwrap_err().code(),
    "migration_capture_subscription_identity"
  );

  let used_hub = hub(2, 8_192, 4_096);
  assert_eq!(used_hub.offer_acknowledgement(&acknowledgement(1, 1, 2, "/already-used")), SoftMutationAdmissionV1::Accepted);
  let error = MigrationCaptureSubscriptionV1::new(
    identity(3, 11),
    MigrationCaptureSubscriptionBoundaryV1 { source_namespace_root: vec![1; 32], publication_sequence: 1 },
    used_hub,
  )
  .unwrap_err();
  assert!(matches!(error, MigrationCaptureSubscriptionErrorV1::HubNotPristine));
  assert_eq!(error.code(), "migration_capture_subscription_hub_state");

  let shared_hub = hub(2, 8_192, 4_096);
  let retained_clone = Arc::clone(&shared_hub);
  let error = MigrationCaptureSubscriptionV1::new(
    identity(13, 21),
    MigrationCaptureSubscriptionBoundaryV1 { source_namespace_root: vec![1; 32], publication_sequence: 1 },
    shared_hub,
  )
  .unwrap_err();
  assert!(matches!(error, MigrationCaptureSubscriptionErrorV1::HubNotExclusive));
  assert_eq!(error.code(), "migration_capture_subscription_hub_ownership");
  drop(retained_clone);
}

#[test]
fn subscription_module_remains_disconnected_from_service_and_persistence_authority() {
  let package = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
  let source = std::fs::read_to_string(package.join("src/engine/v4/migration_capture_subscription.rs")).unwrap();
  for forbidden in [
    "axum",
    "server::",
    "DirectoryOps",
    "MigrationCaptureWorkspaceWriterV1",
    "MigrationStateOwnerV1",
    "V4FirstAuthorityPublisher",
    "std::fs",
  ] {
    assert!(!source.contains(forbidden), "subscription boundary must not own {forbidden}");
  }
}

#[test]
fn owner_registers_and_removes_the_exact_engine_subscription_on_a_real_file() {
  let temporary = tempfile::tempdir().unwrap();
  let database = temporary.path().join("subscription.aeordb");
  let engine = StorageEngine::create(database.to_str().unwrap()).unwrap();
  let selected = identity(9, 17);

  let (owner, subscription) = MigrationCaptureSubscriptionOwnerV1::register(&engine, selected, hub(2, 8_192, 4_096)).unwrap();
  assert_eq!(owner.identity(), selected);
  assert_eq!(subscription.boundary().source_namespace_root, engine.head_hash().unwrap());
  assert_eq!(subscription.boundary().publication_sequence, engine.durability_snapshot().unwrap().hard_frontier);

  let duplicate = MigrationCaptureSubscriptionOwnerV1::register(&engine, identity(10, 18), hub(2, 8_192, 4_096)).unwrap_err();
  assert_eq!(duplicate.code(), "migration_capture_subscription_already_registered");

  let retired = owner.unregister(&engine).unwrap();
  assert!(Arc::ptr_eq(retired.subscription(), &subscription));
  assert!(retired.close_error().is_none());
  assert!(retired.subscription().snapshot().unwrap().admission_closed);

  let dirty = hub(2, 8_192, 4_096);
  assert_eq!(dirty.offer_acknowledgement(&acknowledgement(1, 1, 2, "/dirty")), SoftMutationAdmissionV1::Accepted);
  let dirty_error = MigrationCaptureSubscriptionOwnerV1::register(&engine, identity(11, 19), dirty).unwrap_err();
  assert_eq!(dirty_error.code(), "migration_capture_subscription_hub_state");
}
