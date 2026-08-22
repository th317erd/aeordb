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
