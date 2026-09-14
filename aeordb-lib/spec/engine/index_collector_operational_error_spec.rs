use super::*;

#[test]
fn constructor_failures_preserve_resource_cancellation_and_dependency_categories() {
  use super::super::index_source::SourceOperationalErrorV1;
  for error in [
    AuthoritativeSourceEvaluationErrorV1::ResourcePressure("admission refused".into()),
    AuthoritativeSourceEvaluationErrorV1::Source(SourceOperationalErrorV1::host_failure("reserve", "allocation refused")),
    AuthoritativeSourceEvaluationErrorV1::Parser(IndexParserExecutionErrorV1::host_failure("reserve", "allocation refused")),
  ] {
    assert!(matches!(source_constructor_retry_reason(error), Err(IndexProducerCollectorErrorV1::ResourcePressure(_))));
  }
  for error in [
    AuthoritativeSourceEvaluationErrorV1::Cancelled,
    AuthoritativeSourceEvaluationErrorV1::Parser(IndexParserExecutionErrorV1::cancelled("cancelled", "cancelled")),
  ] {
    assert!(matches!(source_constructor_retry_reason(error), Err(IndexProducerCollectorErrorV1::Cancelled)));
  }
  assert_eq!(
    source_constructor_retry_reason(AuthoritativeSourceEvaluationErrorV1::Parser(IndexParserExecutionErrorV1::dependency_unavailable(
      "missing",
      "dependency missing",
    )))
    .unwrap(),
    Some(stable_reason_v1::DEPENDENCY_UNAVAILABLE)
  );
  assert_eq!(
    source_constructor_retry_reason(AuthoritativeSourceEvaluationErrorV1::InvalidConfiguration {
      code: "malformed",
      context: "bad bytes".into()
    })
    .unwrap(),
    None
  );

  // The retained fixture's unknown selector fingerprint is structurally valid
  // but unavailable. It supplies real source errors without test-only APIs.
  use super::super::index_source::{SourceDocumentV1, ValueStoreRuntimeV1};
  let bytes = include_bytes!("../fixtures/v4/value-store-definition-v1/avst-blake3-256-json-corrected-valid.bin");
  let runtime = ValueStoreRuntimeV1::from_encoded(bytes, HashAlgorithm::Blake3_256).unwrap();
  let unavailable = runtime.ensure_selector_execution_supported().unwrap_err();
  assert_eq!(
    source_constructor_retry_reason(AuthoritativeSourceEvaluationErrorV1::Source(unavailable)).unwrap(),
    Some(stable_reason_v1::DEPENDENCY_UNAVAILABLE)
  );
  let record = FileRecord::new("/data".into(), None, 0, Vec::new());
  let cancelled = runtime.extract(SourceDocumentV1 { file_record: &record, parsed_value: None }, None, &|| true).unwrap_err();
  assert!(matches!(
    source_constructor_retry_reason(AuthoritativeSourceEvaluationErrorV1::Source(cancelled)),
    Err(IndexProducerCollectorErrorV1::Cancelled)
  ));
}

#[test]
fn host_failure_never_produces_a_frozen_document_state() {
  let result =
    field_state(IndexDefinitionErrorV1::new(IndexDefinitionErrorClassV1::HostFailure, "index_source_value_reserve", "host pressure"));
  assert!(matches!(result, Err(IndexProducerCollectorErrorV1::ResourcePressure(_))));
}

#[test]
fn deterministic_definition_limits_still_produce_their_frozen_reasons() {
  for (code, reason) in
    [("converter_input_limit", 0x000c), ("converter_output_count_limit", 0x000e), ("converter_total_output_limit", 0x000f)]
  {
    let state =
      field_state(IndexDefinitionErrorV1::new(IndexDefinitionErrorClassV1::ResourceLimit, code, "definition limit")).unwrap().unwrap();
    assert_eq!(state.reason, reason);
  }
  let state =
    field_state(IndexDefinitionErrorV1::new(IndexDefinitionErrorClassV1::InvalidSourceValue, "invalid", "wrong type")).unwrap().unwrap();
  assert_eq!(state.reason, 0x0009);
}
