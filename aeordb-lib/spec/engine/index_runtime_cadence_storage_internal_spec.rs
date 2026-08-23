use super::*;

#[test]
fn absent_v4_runtime_is_a_noop_for_the_shared_index_timer_tick() {
  let directory = tempfile::tempdir().unwrap();
  let engine = StorageEngine::create(directory.path().join("no-v4-cadence.aeordb").to_str().unwrap()).unwrap();

  assert_eq!(engine.flush_index_runtime_if_due_v1().unwrap(), None);
  assert!(engine.index_runtime_snapshot_v1().is_none());
}

#[test]
fn producer_service_and_due_flush_failures_are_resolved_without_hiding_either_attempt() {
  use crate::engine::v4::index_runtime_cadence::IndexRuntimeCadenceErrorV1;
  use crate::engine::v4::index_runtime_owner::{IndexRuntimeErrorV1, IndexRuntimeFlushOutcomeV1};

  assert_eq!(
    resolve_index_runtime_service_and_flush(Ok(3), Ok(IndexRuntimeFlushOutcomeV1::Idle)).unwrap(),
    IndexRuntimeFlushOutcomeV1::Idle
  );
  let service = IndexRuntimeCadenceErrorV1::NativeSource("injected source failure".to_string());
  assert_eq!(
    resolve_index_runtime_service_and_flush(Err(service), Ok(IndexRuntimeFlushOutcomeV1::Idle)).unwrap_err(),
    IndexRuntimeCadenceErrorV1::NativeSource("injected source failure".to_string())
  );
  let flush = IndexRuntimeCadenceErrorV1::Runtime(IndexRuntimeErrorV1::Mutations("injected flush failure".to_string()));
  assert_eq!(
    resolve_index_runtime_service_and_flush(Ok(0), Err(flush)).unwrap_err(),
    IndexRuntimeCadenceErrorV1::Runtime(IndexRuntimeErrorV1::Mutations("injected flush failure".to_string()))
  );

  let combined = resolve_index_runtime_service_and_flush(
    Err(IndexRuntimeCadenceErrorV1::NativeSource("injected source failure".to_string())),
    Err(IndexRuntimeCadenceErrorV1::Runtime(IndexRuntimeErrorV1::Mutations("injected flush failure".to_string()))),
  )
  .unwrap_err();
  assert!(matches!(combined, IndexRuntimeCadenceErrorV1::ServiceAndFlush { .. }));
  let rendered = combined.to_string();
  assert!(rendered.contains("injected source failure"));
  assert!(rendered.contains("injected flush failure"));

  assert_eq!(
    resolve_index_runtime_service_and_flush(
      Err(IndexRuntimeCadenceErrorV1::Runtime(IndexRuntimeErrorV1::Canceled)),
      Err(IndexRuntimeCadenceErrorV1::Runtime(IndexRuntimeErrorV1::Canceled)),
    )
    .unwrap_err(),
    IndexRuntimeCadenceErrorV1::Runtime(IndexRuntimeErrorV1::Canceled)
  );
}

#[test]
fn primary_cadence_and_coverage_failures_are_resolved_without_hiding_either_failure() {
  use crate::engine::v4::index_runtime_cadence::IndexRuntimeCadenceErrorV1;
  use crate::engine::v4::index_runtime_owner::IndexRuntimeFlushOutcomeV1;

  assert_eq!(
    resolve_index_runtime_outcome_and_coverage(Ok(IndexRuntimeFlushOutcomeV1::Idle), Ok(())).unwrap(),
    IndexRuntimeFlushOutcomeV1::Idle
  );
  assert_eq!(
    resolve_index_runtime_outcome_and_coverage::<IndexRuntimeFlushOutcomeV1>(
      Err(IndexRuntimeCadenceErrorV1::NativeSource("injected source failure".to_string())),
      Ok(()),
    )
    .unwrap_err(),
    IndexRuntimeCadenceErrorV1::NativeSource("injected source failure".to_string())
  );
  assert_eq!(
    resolve_index_runtime_outcome_and_coverage(
      Ok(IndexRuntimeFlushOutcomeV1::Idle),
      Err(IndexRuntimeCadenceErrorV1::Coverage("injected coverage failure".to_string())),
    )
    .unwrap_err(),
    IndexRuntimeCadenceErrorV1::Coverage("injected coverage failure".to_string())
  );

  let combined = resolve_index_runtime_outcome_and_coverage::<IndexRuntimeFlushOutcomeV1>(
    Err(IndexRuntimeCadenceErrorV1::NativeSource("injected source failure".to_string())),
    Err(IndexRuntimeCadenceErrorV1::Coverage("injected coverage failure".to_string())),
  )
  .unwrap_err();
  assert!(matches!(combined, IndexRuntimeCadenceErrorV1::RuntimeAndCoverage { .. }));
  let rendered = combined.to_string();
  assert!(rendered.contains("injected source failure"));
  assert!(rendered.contains("injected coverage failure"));
}

#[test]
fn selection_refresh_policy_is_conservative_for_uncertain_producer_and_flush_failures() {
  use crate::engine::v4::index_runtime_cadence::IndexRuntimeCadenceErrorV1;
  use crate::engine::v4::index_runtime_owner::IndexRuntimeFlushOutcomeV1;

  assert_eq!(classify_index_runtime_producer_service(Ok(0)), (Ok(0), false));
  assert_eq!(classify_index_runtime_producer_service(Ok(1)), (Ok(1), true));
  let (producer_result, producer_changed) =
    classify_index_runtime_producer_service(Err(IndexRuntimeCadenceErrorV1::NativeSource("commit outcome is unknown".to_string())));
  assert!(producer_changed);
  assert_eq!(
    producer_result.unwrap_err().to_string(),
    "index runtime native producer source construction failed: commit outcome is unknown"
  );

  assert_eq!(classify_index_runtime_flush(Ok(IndexRuntimeFlushOutcomeV1::Idle)), (Ok(IndexRuntimeFlushOutcomeV1::Idle), false));
  assert_eq!(
    classify_index_runtime_flush(Ok(IndexRuntimeFlushOutcomeV1::Deferred { retry_at_ms: 17 })),
    (Ok(IndexRuntimeFlushOutcomeV1::Deferred { retry_at_ms: 17 }), false)
  );
  let published = IndexRuntimeFlushOutcomeV1::Published { records: 1, publication_bytes: 2, checkpoint_sequence: 3 };
  assert_eq!(classify_index_runtime_flush(Ok(published.clone())), (Ok(published), true));
  let (flush_result, flush_changed) =
    classify_index_runtime_flush(Err(IndexRuntimeCadenceErrorV1::Coverage("commit outcome is unknown".to_string())));
  assert!(flush_changed);
  assert_eq!(flush_result.unwrap_err().to_string(), "index runtime coverage refresh failed: commit outcome is unknown");
}

#[test]
fn pending_coverage_refresh_runs_after_an_uncertain_primary_failure_and_preserves_both_errors() {
  use std::cell::Cell;

  use crate::engine::v4::index_runtime_cadence::IndexRuntimeCadenceErrorV1;
  use crate::engine::v4::index_runtime_owner::IndexRuntimeFlushOutcomeV1;

  let refreshed = Cell::new(false);
  let result = resolve_index_runtime_cadence_and_refresh::<IndexRuntimeFlushOutcomeV1, _>(
    Err(IndexRuntimeCadenceErrorV1::NativeSource("primary commit outcome is unknown".to_string())),
    Ok(()),
    || {
      refreshed.set(true);
      Err(IndexRuntimeCadenceErrorV1::Coverage("coverage source is unavailable".to_string()))
    },
  )
  .unwrap_err();
  assert!(refreshed.get());
  let rendered = result.to_string();
  assert!(rendered.contains("primary commit outcome is unknown"));
  assert!(rendered.contains("coverage source is unavailable"));

  let refreshed = Cell::new(false);
  let result = resolve_index_runtime_cadence_and_refresh(
    Ok(IndexRuntimeFlushOutcomeV1::Idle),
    Err(IndexRuntimeCadenceErrorV1::Coverage("pending-state lock failed".to_string())),
    || {
      refreshed.set(true);
      Ok(())
    },
  )
  .unwrap_err();
  assert!(!refreshed.get());
  assert!(result.to_string().contains("pending-state lock failed"));
}
