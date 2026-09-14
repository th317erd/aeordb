use super::*;
use crate::engine::v4::index_definition_runtime::IndexDefinitionErrorV1;

#[test]
fn definition_host_failure_is_operational_in_native_query_source() {
  let error = map_index_definition_error(IndexDefinitionErrorV1::new(
    IndexDefinitionErrorClassV1::HostFailure,
    "allocation_refused",
    "host pressure",
  ));
  assert_eq!(error.class(), QueryExecutionSourceErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "allocation_refused");
  let error =
    map_index_definition_error(IndexDefinitionErrorV1::new(IndexDefinitionErrorClassV1::InvalidSourceValue, "invalid", "bad value"));
  assert_eq!(error.class(), QueryExecutionSourceErrorClassV1::Corrupt);
}
