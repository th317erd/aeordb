//! Bounded selected-root input authority for incremental query aggregation.

use std::mem::size_of;

use tokio_util::sync::CancellationToken;

use super::position::{PositionComparatorV1, PositionComponentStateV1};
use super::position_order::{LogicalOrderComponentOwnedV1, validate_logical_order_component_v1};
use super::query_executor::{
  QueryExecutionFieldStateV1, QueryExecutionSinkErrorClassV1, QueryExecutionSinkErrorV1, QueryExecutionSourceErrorClassV1,
  QueryExecutionSourceErrorV1,
};
use super::query_planner::{CompiledQueryAuxiliaryOperationV1, CompiledRootAwareQueryPlanV1, QueryAggregateKindV1};

const ALLOCATION_OVERHEAD_BYTES: u64 = 16;
const MAXIMUM_IDENTITY_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryAggregateInputLimitsV1 {
  maximum_fields: usize,
  maximum_values_per_field: u64,
  maximum_total_values: u64,
  maximum_row_bytes: u64,
}

impl QueryAggregateInputLimitsV1 {
  pub fn new(
    maximum_fields: usize,
    maximum_values_per_field: u64,
    maximum_total_values: u64,
    maximum_row_bytes: u64,
  ) -> Result<Self, QueryExecutionSinkErrorV1> {
    if maximum_fields == 0 || maximum_values_per_field == 0 || maximum_total_values == 0 || maximum_row_bytes == 0 {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::ResourceLimit,
        "query_aggregate_input_limits",
        "aggregate field, value, and row-byte limits must be nonzero",
      ));
    }
    Ok(Self { maximum_fields, maximum_values_per_field, maximum_total_values, maximum_row_bytes })
  }

  pub const fn maximum_fields(self) -> usize {
    self.maximum_fields
  }

  pub const fn maximum_values_per_field(self) -> u64 {
    self.maximum_values_per_field
  }

  pub const fn maximum_total_values(self) -> u64 {
    self.maximum_total_values
  }

  pub const fn maximum_row_bytes(self) -> u64 {
    self.maximum_row_bytes
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledQueryAggregateInputFieldV1 {
  field_name: String,
  comparator: PositionComparatorV1,
  comparison_semantics: u16,
  collation_semantics: u16,
  behavior_fingerprint: [u8; 32],
  scope_ids: Vec<Vec<u8>>,
  operations: Vec<CompiledQueryAuxiliaryOperationV1>,
}

impl CompiledQueryAggregateInputFieldV1 {
  pub fn field_name(&self) -> &str {
    &self.field_name
  }

  pub const fn comparator(&self) -> PositionComparatorV1 {
    self.comparator
  }

  pub const fn comparison_semantics(&self) -> u16 {
    self.comparison_semantics
  }

  pub const fn collation_semantics(&self) -> u16 {
    self.collation_semantics
  }

  pub const fn behavior_fingerprint(&self) -> &[u8; 32] {
    &self.behavior_fingerprint
  }

  pub fn scope_ids(&self) -> &[Vec<u8>] {
    &self.scope_ids
  }

  pub fn operations(&self) -> &[CompiledQueryAuxiliaryOperationV1] {
    &self.operations
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledQueryAggregateInputV1 {
  database_id: [u8; 16],
  physical_instance_id: [u8; 16],
  selected_namespace_root: Vec<u8>,
  query_path: String,
  fields: Vec<CompiledQueryAggregateInputFieldV1>,
  limits: QueryAggregateInputLimitsV1,
}

impl CompiledQueryAggregateInputV1 {
  pub fn from_plan(plan: &CompiledRootAwareQueryPlanV1, limits: QueryAggregateInputLimitsV1) -> Result<Self, QueryExecutionSinkErrorV1> {
    let hash_width = plan.hash_algorithm().hash_length();
    if plan.selected_namespace_root().len() != hash_width
      || plan.selected_namespace_root().len() > MAXIMUM_IDENTITY_BYTES
      || plan.selected_namespace_root().iter().all(|byte| *byte == 0)
    {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::CorruptSource,
        "query_aggregate_input_plan",
        "compiled query plan has an invalid selected-root identity",
      ));
    }

    let aggregate_count =
      plan.auxiliary_fields().iter().filter(|field| !matches!(field.operation(), CompiledQueryAuxiliaryOperationV1::Sort(_))).count();
    if aggregate_count == 0 {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::CorruptSource,
        "query_aggregate_input_plan",
        "compiled query plan has no aggregate or group operation",
      ));
    }
    let mut fields: Vec<CompiledQueryAggregateInputFieldV1> = Vec::new();
    fields.try_reserve_exact(aggregate_count.min(limits.maximum_fields)).map_err(|source| {
      sink_error(
        QueryExecutionSinkErrorClassV1::ResourceLimit,
        "query_aggregate_input_reserve",
        format!("cannot reserve bounded aggregate input fields: {source}"),
      )
    })?;
    for auxiliary in plan.auxiliary_fields() {
      let operation = auxiliary.operation();
      if matches!(operation, CompiledQueryAuxiliaryOperationV1::Sort(_)) {
        continue;
      }
      if let Some(existing) = fields.iter_mut().find(|field| field.field_name == auxiliary.field_name()) {
        require_duplicate_semantics(existing, auxiliary, hash_width)?;
        if existing.operations.contains(&operation) {
          return Err(sink_error(
            QueryExecutionSinkErrorClassV1::CorruptSource,
            "query_aggregate_input_duplicate",
            format!("compiled query repeats aggregate operation {operation:?} for {}", auxiliary.field_name()),
          ));
        }
        existing.operations.try_reserve(1).map_err(|source| {
          sink_error(
            QueryExecutionSinkErrorClassV1::ResourceLimit,
            "query_aggregate_input_operation_reserve",
            format!("cannot reserve aggregate field operation: {source}"),
          )
        })?;
        existing.operations.push(operation);
        continue;
      }
      if fields.len() >= limits.maximum_fields {
        return Err(sink_error(
          QueryExecutionSinkErrorClassV1::ResourceLimit,
          "query_aggregate_input_field_limit",
          "compiled aggregate input exceeds its field limit",
        ));
      }
      let semantics = auxiliary.order_semantics();
      validate_aggregate_operation(operation, semantics.comparator(), auxiliary.field_name())?;
      let mut scope_ids = Vec::new();
      scope_ids.try_reserve_exact(auxiliary.scopes().len()).map_err(|source| {
        sink_error(
          QueryExecutionSinkErrorClassV1::ResourceLimit,
          "query_aggregate_input_scope_reserve",
          format!("cannot reserve aggregate field scopes: {source}"),
        )
      })?;
      for scope in auxiliary.scopes() {
        if scope.scope_id().len() != hash_width || scope.scope_id().iter().all(|byte| *byte == 0) {
          return Err(sink_error(
            QueryExecutionSinkErrorClassV1::CorruptSource,
            "query_aggregate_input_scope",
            format!("compiled aggregate field {} has an invalid ScopeId", auxiliary.field_name()),
          ));
        }
        let mut scope_id = Vec::new();
        scope_id.try_reserve_exact(scope.scope_id().len()).map_err(|source| {
          sink_error(
            QueryExecutionSinkErrorClassV1::ResourceLimit,
            "query_aggregate_input_scope_reserve",
            format!("cannot reserve aggregate ScopeId: {source}"),
          )
        })?;
        scope_id.extend_from_slice(scope.scope_id());
        scope_ids.push(scope_id);
      }
      if scope_ids.is_empty() {
        return Err(sink_error(
          QueryExecutionSinkErrorClassV1::CorruptSource,
          "query_aggregate_input_scope",
          format!("compiled aggregate field {} has no effective scope", auxiliary.field_name()),
        ));
      }
      let mut field_name = String::new();
      field_name.try_reserve_exact(auxiliary.field_name().len()).map_err(|source| {
        sink_error(
          QueryExecutionSinkErrorClassV1::ResourceLimit,
          "query_aggregate_input_name_reserve",
          format!("cannot reserve aggregate field name: {source}"),
        )
      })?;
      field_name.push_str(auxiliary.field_name());
      let mut operations = Vec::new();
      operations.try_reserve_exact(1).map_err(|source| {
        sink_error(
          QueryExecutionSinkErrorClassV1::ResourceLimit,
          "query_aggregate_input_operation_reserve",
          format!("cannot reserve aggregate field operation: {source}"),
        )
      })?;
      operations.push(operation);
      fields.push(CompiledQueryAggregateInputFieldV1 {
        field_name,
        comparator: semantics.comparator(),
        comparison_semantics: semantics.comparison_semantics(),
        collation_semantics: semantics.collation_semantics(),
        behavior_fingerprint: *semantics.behavior_fingerprint(),
        scope_ids,
        operations,
      });
    }

    Ok(Self {
      database_id: plan.database_id(),
      physical_instance_id: plan.physical_instance_id(),
      selected_namespace_root: try_clone_bytes(plan.selected_namespace_root(), "aggregate selected root")?,
      query_path: try_clone_string(plan.query_path(), "aggregate query path")?,
      fields,
      limits,
    })
  }

  pub const fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  pub const fn physical_instance_id(&self) -> [u8; 16] {
    self.physical_instance_id
  }

  pub fn selected_namespace_root(&self) -> &[u8] {
    &self.selected_namespace_root
  }

  pub fn query_path(&self) -> &str {
    &self.query_path
  }

  pub fn fields(&self) -> &[CompiledQueryAggregateInputFieldV1] {
    &self.fields
  }

  pub const fn limits(&self) -> QueryAggregateInputLimitsV1 {
    self.limits
  }
}

#[derive(Clone, Copy, Debug)]
pub struct QueryAggregateInputLookupRequestV1<'a> {
  input: &'a CompiledQueryAggregateInputV1,
  file_key: &'a [u8],
  record_revision: &'a [u8],
}

impl<'a> QueryAggregateInputLookupRequestV1<'a> {
  pub const fn new(input: &'a CompiledQueryAggregateInputV1, file_key: &'a [u8], record_revision: &'a [u8]) -> Self {
    Self { input, file_key, record_revision }
  }

  pub const fn database_id(self) -> [u8; 16] {
    self.input.database_id
  }

  pub const fn physical_instance_id(self) -> [u8; 16] {
    self.input.physical_instance_id
  }

  pub fn selected_namespace_root(self) -> &'a [u8] {
    &self.input.selected_namespace_root
  }

  pub fn query_path(self) -> &'a str {
    &self.input.query_path
  }

  pub fn fields(self) -> &'a [CompiledQueryAggregateInputFieldV1] {
    &self.input.fields
  }

  pub const fn file_key(self) -> &'a [u8] {
    self.file_key
  }

  pub const fn record_revision(self) -> &'a [u8] {
    self.record_revision
  }

  pub const fn limits(self) -> QueryAggregateInputLimitsV1 {
    self.input.limits
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryAggregateInputFieldV1 {
  pub field_name: String,
  pub scope_id: Vec<u8>,
  pub state: QueryExecutionFieldStateV1,
  pub values: Vec<LogicalOrderComponentOwnedV1>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryAggregateInputRowV1 {
  pub selected_namespace_root: Vec<u8>,
  pub file_key: Vec<u8>,
  pub record_revision: Vec<u8>,
  pub fields: Vec<QueryAggregateInputFieldV1>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryAggregateInputLookupResultV1 {
  Found(QueryAggregateInputRowV1),
  Absent,
}

pub trait QueryAggregateInputSourceV1 {
  /// Recompute every requested field from the exact selected-root immutable
  /// document. Returned values use the compiler-approved comparator payloads
  /// and preserve source-value ordinal order and duplicates.
  fn resolve_aggregate_input(
    &mut self,
    request: QueryAggregateInputLookupRequestV1<'_>,
    cancellation: &CancellationToken,
  ) -> Result<QueryAggregateInputLookupResultV1, QueryExecutionSourceErrorV1>;
}

pub fn resolve_query_aggregate_input_v1(
  request: QueryAggregateInputLookupRequestV1<'_>,
  source: &mut dyn QueryAggregateInputSourceV1,
  cancellation: &CancellationToken,
) -> Result<QueryAggregateInputLookupResultV1, QueryExecutionSourceErrorV1> {
  require_not_cancelled(cancellation)?;
  validate_lookup_request(request)?;
  let lookup = source.resolve_aggregate_input(request, cancellation)?;
  require_not_cancelled(cancellation)?;
  let QueryAggregateInputLookupResultV1::Found(row) = lookup else {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "query_aggregate_input_absent",
      "an exact query match is absent from the selected-root aggregate input",
    ));
  };
  validate_lookup_row(request, &row)?;
  require_not_cancelled(cancellation)?;
  Ok(QueryAggregateInputLookupResultV1::Found(row))
}

fn validate_lookup_request(request: QueryAggregateInputLookupRequestV1<'_>) -> Result<(), QueryExecutionSourceErrorV1> {
  let hash_width = request.selected_namespace_root().len();
  if hash_width == 0
    || hash_width > MAXIMUM_IDENTITY_BYTES
    || request.file_key().len() != hash_width
    || request.record_revision().len() != hash_width
    || request.file_key().iter().all(|byte| *byte == 0)
    || request.record_revision().iter().all(|byte| *byte == 0)
    || request.fields().is_empty()
    || request.fields().len() > request.limits().maximum_fields
  {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "query_aggregate_input_request",
      "aggregate input lookup carries an invalid identity or field set",
    ));
  }
  Ok(())
}

fn validate_lookup_row(
  request: QueryAggregateInputLookupRequestV1<'_>,
  row: &QueryAggregateInputRowV1,
) -> Result<(), QueryExecutionSourceErrorV1> {
  if row.selected_namespace_root != request.selected_namespace_root()
    || row.file_key != request.file_key()
    || row.record_revision != request.record_revision()
    || row.fields.len() != request.fields().len()
  {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "query_aggregate_input_identity",
      "aggregate input row disagrees with the requested root, immutable identity, or field count",
    ));
  }
  let mut total_values = 0u64;
  for (expected, field) in request.fields().iter().zip(&row.fields) {
    if field.field_name != expected.field_name
      || !expected.scope_ids.contains(&field.scope_id)
      || field.scope_id.len() != request.selected_namespace_root().len()
    {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::Corrupt,
        "query_aggregate_input_field",
        format!("aggregate input field {:?} has a foreign name or ScopeId", field.field_name),
      ));
    }
    let value_count = u64::try_from(field.values.len()).map_err(|source| {
      source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "query_aggregate_input_value_count", source.to_string())
    })?;
    if value_count > request.limits().maximum_values_per_field {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "query_aggregate_input_value_limit",
        format!("aggregate field {} exceeds its value-count limit", field.field_name),
      ));
    }
    total_values = total_values.checked_add(value_count).ok_or_else(|| {
      source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "query_aggregate_input_value_count", "aggregate value count overflowed")
    })?;
    if total_values > request.limits().maximum_total_values {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "query_aggregate_input_total_value_limit",
        "aggregate input exceeds its total value-count limit",
      ));
    }
    match field.state {
      QueryExecutionFieldStateV1::Values if !field.values.is_empty() => {}
      QueryExecutionFieldStateV1::Missing | QueryExecutionFieldStateV1::DeterministicUnindexable if field.values.is_empty() => continue,
      _ => {
        return Err(source_error(
          QueryExecutionSourceErrorClassV1::Corrupt,
          "query_aggregate_input_state",
          format!("aggregate field {} state disagrees with its values", field.field_name),
        ));
      }
    }
    for value in &field.values {
      if value.state == PositionComponentStateV1::Missing {
        return Err(source_error(
          QueryExecutionSourceErrorClassV1::Corrupt,
          "query_aggregate_input_value_state",
          format!("aggregate field {} contains a per-value missing marker", field.field_name),
        ));
      }
      validate_logical_order_component_v1(expected.comparator, value, &field.field_name).map_err(|source| {
        source_error(
          QueryExecutionSourceErrorClassV1::Corrupt,
          "query_aggregate_input_value",
          format!("aggregate field {} returned a malformed value: {source}", field.field_name),
        )
      })?;
    }
  }
  let row_bytes = aggregate_input_row_allocated_bytes(row)?;
  if row_bytes > request.limits().maximum_row_bytes {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "query_aggregate_input_row_limit",
      format!("aggregate input row requires {row_bytes} bytes, cap is {}", request.limits().maximum_row_bytes),
    ));
  }
  Ok(())
}

fn require_duplicate_semantics(
  existing: &CompiledQueryAggregateInputFieldV1,
  auxiliary: &super::query_planner::CompiledQueryAuxiliaryFieldPlanV1,
  hash_width: usize,
) -> Result<(), QueryExecutionSinkErrorV1> {
  let semantics = auxiliary.order_semantics();
  let scopes_match = existing.scope_ids.len() == auxiliary.scopes().len()
    && existing
      .scope_ids
      .iter()
      .zip(auxiliary.scopes())
      .all(|(expected, actual)| actual.scope_id().len() == hash_width && expected == actual.scope_id());
  if existing.comparator != semantics.comparator()
    || existing.comparison_semantics != semantics.comparison_semantics()
    || existing.collation_semantics != semantics.collation_semantics()
    || existing.behavior_fingerprint != *semantics.behavior_fingerprint()
    || !scopes_match
  {
    return Err(sink_error(
      QueryExecutionSinkErrorClassV1::HistoricalViewUnavailable,
      "query_aggregate_input_semantics",
      format!("aggregate operations for {} disagree on selected-root semantics or scopes", auxiliary.field_name()),
    ));
  }
  validate_aggregate_operation(auxiliary.operation(), existing.comparator, auxiliary.field_name())
}

fn validate_aggregate_operation(
  operation: CompiledQueryAuxiliaryOperationV1,
  comparator: PositionComparatorV1,
  field_name: &str,
) -> Result<(), QueryExecutionSinkErrorV1> {
  if matches!(operation, CompiledQueryAuxiliaryOperationV1::Aggregate(QueryAggregateKindV1::Sum | QueryAggregateKindV1::Average))
    && !matches!(comparator, PositionComparatorV1::U64 | PositionComparatorV1::I64 | PositionComparatorV1::FiniteF64)
  {
    return Err(sink_error(
      QueryExecutionSinkErrorClassV1::CorruptSource,
      "query_aggregate_numeric_required",
      format!("sum/average field {field_name} does not use a numeric comparator"),
    ));
  }
  Ok(())
}

fn aggregate_input_row_allocated_bytes(row: &QueryAggregateInputRowV1) -> Result<u64, QueryExecutionSourceErrorV1> {
  let mut bytes = u64::try_from(size_of::<QueryAggregateInputRowV1>()).map_err(|source| {
    source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "query_aggregate_input_row_bytes", source.to_string())
  })?;
  bytes = add_capacity(bytes, row.selected_namespace_root.capacity(), "selected root")?;
  bytes = add_capacity(bytes, row.file_key.capacity(), "FileKey")?;
  bytes = add_capacity(bytes, row.record_revision.capacity(), "record revision")?;
  bytes = add_slots::<QueryAggregateInputFieldV1>(bytes, row.fields.capacity(), "field slots")?;
  for field in &row.fields {
    bytes = add_capacity(bytes, field.field_name.capacity(), "field name")?;
    bytes = add_capacity(bytes, field.scope_id.capacity(), "ScopeId")?;
    bytes = add_slots::<LogicalOrderComponentOwnedV1>(bytes, field.values.capacity(), "value slots")?;
    for value in &field.values {
      bytes = add_capacity(bytes, value.payload.capacity(), "value payload")?;
    }
  }
  Ok(bytes)
}

fn add_slots<T>(bytes: u64, capacity: usize, label: &str) -> Result<u64, QueryExecutionSourceErrorV1> {
  let slot_bytes = capacity.checked_mul(size_of::<T>()).ok_or_else(|| {
    source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "query_aggregate_input_row_bytes", format!("{label} overflowed"))
  })?;
  add_capacity(bytes, slot_bytes, label)
}

fn add_capacity(bytes: u64, capacity: usize, label: &str) -> Result<u64, QueryExecutionSourceErrorV1> {
  let capacity = u64::try_from(capacity).map_err(|source| {
    source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "query_aggregate_input_row_bytes", format!("{label}: {source}"))
  })?;
  bytes.checked_add(ALLOCATION_OVERHEAD_BYTES).and_then(|value| value.checked_add(capacity)).ok_or_else(|| {
    source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "query_aggregate_input_row_bytes", format!("{label} overflowed"))
  })
}

fn try_clone_bytes(value: &[u8], label: &str) -> Result<Vec<u8>, QueryExecutionSinkErrorV1> {
  let mut output = Vec::new();
  output.try_reserve_exact(value.len()).map_err(|source| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_input_reserve", format!("cannot reserve {label}: {source}"))
  })?;
  output.extend_from_slice(value);
  Ok(output)
}

fn try_clone_string(value: &str, label: &str) -> Result<String, QueryExecutionSinkErrorV1> {
  let mut output = String::new();
  output.try_reserve_exact(value.len()).map_err(|source| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_input_reserve", format!("cannot reserve {label}: {source}"))
  })?;
  output.push_str(value);
  Ok(output)
}

fn require_not_cancelled(cancellation: &CancellationToken) -> Result<(), QueryExecutionSourceErrorV1> {
  if cancellation.is_cancelled() {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::Cancelled,
      "query_aggregate_input_cancelled",
      "aggregate input lookup was cancelled",
    ));
  }
  Ok(())
}

fn source_error(class: QueryExecutionSourceErrorClassV1, code: &'static str, context: impl Into<String>) -> QueryExecutionSourceErrorV1 {
  QueryExecutionSourceErrorV1::new(class, code, context)
}

fn sink_error(class: QueryExecutionSinkErrorClassV1, code: &'static str, context: impl Into<String>) -> QueryExecutionSinkErrorV1 {
  QueryExecutionSinkErrorV1::new(class, code, context)
}
