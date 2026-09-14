use super::*;
use crate::engine::v4::index_converter::IndexSemanticErrorV1;
use crate::engine::v4::index_definition_runtime::IndexDefinitionErrorV1;

#[test]
fn definition_host_failure_is_operational_in_query_planning() {
  for class in [IndexDefinitionErrorClassV1::HostFailure, IndexDefinitionErrorClassV1::ResourceLimit] {
    let error = map_definition_error(IndexDefinitionErrorV1::new(class, "allocation_refused", "host pressure"));
    assert_eq!(error.class(), QueryPlanningErrorClassV1::ResourceLimit);
    assert_eq!(error.code(), "allocation_refused");
  }
  let error = map_definition_error(IndexDefinitionErrorV1::new(IndexDefinitionErrorClassV1::InvalidSourceValue, "invalid", "bad value"));
  assert_eq!(error.class(), QueryPlanningErrorClassV1::InvalidRequest);
}

#[test]
fn converter_host_failure_is_operational_in_query_planning() {
  let error = map_semantic_error(IndexSemanticErrorV1::new(IndexSemanticErrorClassV1::HostFailure, "allocation_refused", "host pressure"));
  assert_eq!(error.class(), QueryPlanningErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "allocation_refused");
}
