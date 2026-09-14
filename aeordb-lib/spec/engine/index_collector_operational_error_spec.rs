use super::*;

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
