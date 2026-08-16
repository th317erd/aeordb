use std::sync::Arc;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::namespace_mutation::{NamespaceMutationAcknowledgement, NamespaceMutationKind, NamespaceMutationSourceIdentity};
use aeordb::engine::v4::coverage_runtime::{SoftMutationAdmissionV1, SoftMutationHubOptionsV1, SoftMutationHubV1};
use aeordb::engine::v4::index_task::{JournalOwnerKindV1, decode_mutation_journal};
use aeordb::engine::v4::migration_capture_runtime::{
  MigrationCaptureDrainOutcomeV1, MigrationCaptureDrainPlanV1, MigrationCaptureInexactReasonV1, MigrationCaptureRuntimeClockV1,
  MigrationCaptureRuntimeOptionsV1, prepare_migration_capture_drain,
};
use aeordb::engine::v4::migration_capture_workspace::MigrationCaptureWorkspaceOptionsV1;

fn acknowledgement(operation_id: [u8; 16], sequence: u64, previous_root: u8, root: u8, path: &str) -> NamespaceMutationAcknowledgement {
  acknowledgement_with_width(operation_id, sequence, previous_root, root, path, 32)
}

fn acknowledgement_with_width(
  operation_id: [u8; 16],
  sequence: u64,
  previous_root: u8,
  root: u8,
  path: &str,
  hash_width: usize,
) -> NamespaceMutationAcknowledgement {
  NamespaceMutationAcknowledgement {
    operation_id: uuid::Uuid::from_bytes(operation_id),
    kind: NamespaceMutationKind::FileWrite,
    publication_sequence: sequence,
    previous_root_hash: vec![previous_root; hash_width],
    root_hash: vec![root; hash_width],
    source_identities: vec![NamespaceMutationSourceIdentity {
      path: path.to_string(),
      entry_type: Some(1),
      previous_identity: Some(vec![previous_root; hash_width]),
      new_identity: Some(vec![root; hash_width]),
    }],
    locator_replacements: Vec::new(),
  }
}

fn hub() -> Arc<SoftMutationHubV1> {
  Arc::new(SoftMutationHubV1::new(SoftMutationHubOptionsV1::new(16, 64 * 1_024, 16 * 1_024).unwrap()).unwrap())
}

fn plan(covered_sequence: u64, covered_root: u8) -> MigrationCaptureDrainPlanV1 {
  plan_for(HashAlgorithm::Blake3_256, covered_sequence, covered_root)
}

fn plan_for(hash_algorithm: HashAlgorithm, covered_sequence: u64, covered_root: u8) -> MigrationCaptureDrainPlanV1 {
  let hash_width = hash_algorithm.hash_length();
  MigrationCaptureDrainPlanV1::new(
    hash_algorithm,
    [0x51; 16],
    7,
    1,
    [0x61; 16],
    covered_sequence,
    vec![covered_root; hash_width],
    vec![0; hash_width],
    16,
    64 * 1_024,
  )
  .unwrap()
}

#[test]
fn unordered_duplicate_notices_become_one_exact_task_owned_ainx_segment() {
  let hub = hub();
  let first = acknowledgement([1; 16], 1, 1, 2, "/first.json");
  let second = acknowledgement([2; 16], 2, 2, 3, "/second.json");
  assert_eq!(hub.offer_acknowledgement(&second), SoftMutationAdmissionV1::Accepted);
  assert_eq!(hub.offer_acknowledgement(&first), SoftMutationAdmissionV1::Accepted);
  assert_eq!(hub.offer_acknowledgement(&first), SoftMutationAdmissionV1::Accepted);

  let drain = hub.try_drain(16, 64 * 1_024).unwrap();
  let MigrationCaptureDrainOutcomeV1::Exact(segment) = prepare_migration_capture_drain(drain, &plan(0, 1)).unwrap() else {
    panic!("unordered duplicate notices must retain exact capture");
  };
  assert_eq!(segment.captured_through_publication_sequence(), 2);
  assert_eq!(segment.source_root_after(), &[3; 32]);

  let journal = decode_mutation_journal(segment.bytes(), HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(journal.owner_kind, JournalOwnerKindV1::Task);
  assert_eq!(journal.owner_id, [0x51; 16]);
  assert_eq!(journal.segment_ordinal, 1);
  assert_eq!(journal.first_sequence, 1);
  assert_eq!(journal.last_sequence, 2);
  assert_eq!(journal.records.len(), 2);
}

#[test]
fn widest_hash_profile_produces_the_same_task_owned_segment_contract() {
  let hub = hub();
  assert_eq!(hub.offer_acknowledgement(&acknowledgement_with_width([9; 16], 1, 1, 2, "/wide.json", 64)), SoftMutationAdmissionV1::Accepted);
  let drain = hub.try_drain(16, 64 * 1_024).unwrap();
  let MigrationCaptureDrainOutcomeV1::Exact(segment) =
    prepare_migration_capture_drain(drain, &plan_for(HashAlgorithm::Sha512, 0, 1)).unwrap()
  else {
    panic!("widest profile must retain exact capture");
  };
  let journal = decode_mutation_journal(segment.bytes(), HashAlgorithm::Sha512).unwrap();
  assert_eq!(journal.owner_kind, JournalOwnerKindV1::Task);
  assert_eq!(journal.owner_id, [0x51; 16]);
  assert_eq!(journal.source_root_after, &[2; 64]);
}

#[test]
fn publication_gap_produces_no_artifact_and_requires_full_reconciliation() {
  let hub = hub();
  assert_eq!(hub.offer_acknowledgement(&acknowledgement([1; 16], 1, 1, 2, "/first.json")), SoftMutationAdmissionV1::Accepted);
  assert_eq!(hub.offer_acknowledgement(&acknowledgement([3; 16], 3, 2, 3, "/third.json")), SoftMutationAdmissionV1::Accepted);

  let drain = hub.try_drain(16, 64 * 1_024).unwrap();
  assert_eq!(
    prepare_migration_capture_drain(drain, &plan(0, 1)).unwrap(),
    MigrationCaptureDrainOutcomeV1::FullReconciliationRequired(MigrationCaptureInexactReasonV1::PublicationGap)
  );
}

#[test]
fn conflicting_operation_identity_produces_no_artifact() {
  let hub = hub();
  assert_eq!(hub.offer_acknowledgement(&acknowledgement([7; 16], 1, 1, 2, "/same.json")), SoftMutationAdmissionV1::Accepted);
  assert_eq!(hub.offer_acknowledgement(&acknowledgement([7; 16], 1, 1, 4, "/same.json")), SoftMutationAdmissionV1::Accepted);

  let drain = hub.try_drain(16, 64 * 1_024).unwrap();
  assert_eq!(
    prepare_migration_capture_drain(drain, &plan(0, 1)).unwrap(),
    MigrationCaptureDrainOutcomeV1::FullReconciliationRequired(MigrationCaptureInexactReasonV1::ConflictingOperation)
  );
}

#[test]
fn empty_drain_is_a_noop() {
  let drain = hub().try_drain(16, 64 * 1_024).unwrap();
  assert_eq!(prepare_migration_capture_drain(drain, &plan(0, 1)).unwrap(), MigrationCaptureDrainOutcomeV1::Empty);
}

#[test]
fn malformed_plan_notice_authority_and_window_fail_closed() {
  let invalid_plan =
    MigrationCaptureDrainPlanV1::new(HashAlgorithm::Blake3_256, [0; 16], 0, 0, [0; 16], 0, vec![0; 32], vec![0; 31], 0, 0).unwrap_err();
  assert_eq!(invalid_plan.code(), "migration_capture_runtime_plan");

  let malformed_hub = hub();
  assert_eq!(
    malformed_hub.offer_acknowledgement(&acknowledgement_with_width([4; 16], 1, 1, 2, "/wrong-width.json", 31)),
    SoftMutationAdmissionV1::Accepted
  );
  assert_eq!(
    prepare_migration_capture_drain(malformed_hub.try_drain(16, 64 * 1_024).unwrap(), &plan(0, 1)).unwrap(),
    MigrationCaptureDrainOutcomeV1::FullReconciliationRequired(MigrationCaptureInexactReasonV1::InvalidNotice)
  );

  let disconnected_hub = hub();
  assert_eq!(
    disconnected_hub.offer_acknowledgement(&acknowledgement([5; 16], 1, 7, 8, "/disconnected.json")),
    SoftMutationAdmissionV1::Accepted
  );
  assert_eq!(
    prepare_migration_capture_drain(disconnected_hub.try_drain(16, 64 * 1_024).unwrap(), &plan(0, 1)).unwrap(),
    MigrationCaptureDrainOutcomeV1::FullReconciliationRequired(MigrationCaptureInexactReasonV1::AuthorityDiscontinuity)
  );

  let limited_hub = hub();
  assert_eq!(limited_hub.offer_acknowledgement(&acknowledgement([6; 16], 1, 1, 2, "/one.json")), SoftMutationAdmissionV1::Accepted);
  assert_eq!(limited_hub.offer_acknowledgement(&acknowledgement([7; 16], 2, 2, 3, "/two.json")), SoftMutationAdmissionV1::Accepted);
  let limited_plan =
    MigrationCaptureDrainPlanV1::new(HashAlgorithm::Blake3_256, [0x51; 16], 7, 1, [0x61; 16], 0, vec![1; 32], vec![0; 32], 1, 64 * 1_024)
      .unwrap();
  assert_eq!(
    prepare_migration_capture_drain(limited_hub.try_drain(16, 64 * 1_024).unwrap(), &limited_plan).unwrap(),
    MigrationCaptureDrainOutcomeV1::FullReconciliationRequired(MigrationCaptureInexactReasonV1::WindowLimitExceeded)
  );
}

#[test]
fn runtime_clock_and_resource_limits_reject_every_unbounded_or_zero_shape() {
  for (updated_at_ms, publication_timestamp_ms, monotonic_now_ms) in [(-1, 1, 1), (0, 0, 1), (0, u64::MAX, 1), (0, 1, 0), (0, 1, u64::MAX)]
  {
    assert_eq!(
      MigrationCaptureRuntimeClockV1::new(updated_at_ms, publication_timestamp_ms, monotonic_now_ms).unwrap_err().code(),
      "migration_capture_runtime_clock"
    );
  }
  let scratch = tempfile::tempdir().unwrap();
  let workspace = || MigrationCaptureWorkspaceOptionsV1::new(Some(scratch.path().to_path_buf()), 1 << 20, 0).unwrap();
  let hub = SoftMutationHubOptionsV1::new(8, 64 * 1_024, 16 * 1_024).unwrap();
  for result in [
    MigrationCaptureRuntimeOptionsV1::new(0, [1; 16], hub, 8, 64 * 1_024, 1, workspace()),
    MigrationCaptureRuntimeOptionsV1::new(1, [0; 16], hub, 8, 64 * 1_024, 1, workspace()),
    MigrationCaptureRuntimeOptionsV1::new(1, [1; 16], hub, 0, 64 * 1_024, 1, workspace()),
    MigrationCaptureRuntimeOptionsV1::new(1, [1; 16], hub, 9, 64 * 1_024, 1, workspace()),
    MigrationCaptureRuntimeOptionsV1::new(1, [1; 16], hub, 8, 0, 1, workspace()),
    MigrationCaptureRuntimeOptionsV1::new(1, [1; 16], hub, 8, 64 * 1_024 + 1, 1, workspace()),
    MigrationCaptureRuntimeOptionsV1::new(1, [1; 16], hub, 8, 64 * 1_024, 0, workspace()),
    MigrationCaptureRuntimeOptionsV1::new(1, [1; 16], hub, 8, 64 * 1_024, 300_001, workspace()),
  ] {
    assert_eq!(result.unwrap_err().code(), "migration_capture_runtime_options");
  }
  MigrationCaptureRuntimeOptionsV1::new(1, [1; 16], hub, 8, 64 * 1_024, 300_000, workspace()).unwrap();
}
