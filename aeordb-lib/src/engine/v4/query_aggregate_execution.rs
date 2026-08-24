//! Bounded selected-root input authority for incremental query aggregation.

use std::cmp::Ordering;
use std::fmt;
use std::mem::size_of;

use tokio_util::sync::CancellationToken;

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

use super::hash::digest_parts;
use super::position::{
  CompiledRouteOrderV1, PositionComparatorV1, PositionComponentStateV1, PositionComponentWriteV1, PositionRouteV1,
  append_logical_position_component_v1, logical_position_component_encoded_length_v1,
};
use super::position_order::{
  LogicalNumericValueV1, LogicalOrderComponentOwnedV1, LogicalOrderRowOwnedV1, PositionOrderErrorClassV1,
  compare_logical_order_components_v1, compare_logical_order_rows_v1, compile_aggregate_group_order_v1,
  decode_logical_numeric_component_v1, logical_order_row_allocated_bytes_v1, validate_logical_order_component_v1,
};
use super::query_executor::{
  QueryExecutionFieldStateV1, QueryExecutionMatchRefV1, QueryExecutionMatchSinkV1, QueryExecutionSinkBatchReceiptV1,
  QueryExecutionSinkBatchV1, QueryExecutionSinkErrorClassV1, QueryExecutionSinkErrorV1, QueryExecutionSourceErrorClassV1,
  QueryExecutionSourceErrorV1,
};
use super::query_planner::{CompiledQueryAuxiliaryOperationV1, CompiledRootAwareQueryPlanV1, QueryAggregateKindV1};
use super::reader::MalformedInputClass;

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
  hash_algorithm: HashAlgorithm,
  selected_namespace_root: Vec<u8>,
  query_path: String,
  fields: Vec<CompiledQueryAggregateInputFieldV1>,
  group_field_indices: Vec<usize>,
  result_limit: usize,
  group_order: Option<CompiledRouteOrderV1>,
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
    if aggregate_count == 0 || plan.result_limit() == 0 {
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
    let group_count =
      plan.auxiliary_fields().iter().filter(|field| matches!(field.operation(), CompiledQueryAuxiliaryOperationV1::Group)).count();
    let mut group_field_indices = Vec::new();
    group_field_indices.try_reserve_exact(group_count).map_err(|source| {
      sink_error(
        QueryExecutionSinkErrorClassV1::ResourceLimit,
        "query_aggregate_input_group_reserve",
        format!("cannot reserve aggregate group-field indices: {source}"),
      )
    })?;
    for auxiliary in plan.auxiliary_fields() {
      let operation = auxiliary.operation();
      if matches!(operation, CompiledQueryAuxiliaryOperationV1::Sort(_)) {
        continue;
      }
      if let Some(existing_index) = fields.iter().position(|field| field.field_name == auxiliary.field_name()) {
        let existing = &mut fields[existing_index];
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
        if operation == CompiledQueryAuxiliaryOperationV1::Group {
          group_field_indices.push(existing_index);
        }
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
      if operation == CompiledQueryAuxiliaryOperationV1::Group {
        group_field_indices.push(fields.len() - 1);
      }
    }

    let group_order = if group_field_indices.is_empty() {
      None
    } else {
      Some(compile_aggregate_group_order_v1(plan.hash_algorithm(), &[]).map_err(map_group_order_format_error)?)
    };

    Ok(Self {
      database_id: plan.database_id(),
      physical_instance_id: plan.physical_instance_id(),
      hash_algorithm: plan.hash_algorithm(),
      selected_namespace_root: try_clone_bytes(plan.selected_namespace_root(), "aggregate selected root")?,
      query_path: try_clone_string(plan.query_path(), "aggregate query path")?,
      fields,
      group_field_indices,
      result_limit: plan.result_limit(),
      group_order,
      limits,
    })
  }

  pub const fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  pub const fn physical_instance_id(&self) -> [u8; 16] {
    self.physical_instance_id
  }

  pub const fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
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

  pub fn group_field_indices(&self) -> &[usize] {
    &self.group_field_indices
  }

  pub const fn result_limit(&self) -> usize {
    self.result_limit
  }

  pub const fn group_order(&self) -> Option<&CompiledRouteOrderV1> {
    self.group_order.as_ref()
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
  /// The effective scope only when it defines this field. `None` retains a
  /// document whose effective configuration omits the field as `Missing`.
  pub scope_id: Option<Vec<u8>>,
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

const UNGROUPED_FIXED_RETAINED_BYTES: u64 = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryUngroupedAggregateLimitsV1 {
  maximum_retained_bytes: u64,
}

impl QueryUngroupedAggregateLimitsV1 {
  pub fn new(maximum_retained_bytes: u64) -> Result<Self, QueryExecutionSinkErrorV1> {
    if maximum_retained_bytes == 0 {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::ResourceLimit,
        "query_aggregate_reducer_limits",
        "ungrouped aggregate retained-memory limit must be nonzero",
      ));
    }
    Ok(Self { maximum_retained_bytes })
  }

  pub const fn maximum_retained_bytes(self) -> u64 {
    self.maximum_retained_bytes
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryAggregateNumericV1 {
  UnsignedRatio { numerator: u128, denominator: u64 },
  SignedRatio { numerator: i128, denominator: u64 },
  FiniteF64Bits(u64),
}

impl QueryAggregateNumericV1 {
  pub fn finite_f64(self) -> Option<f64> {
    match self {
      Self::FiniteF64Bits(bits) => Some(f64::from_bits(bits)),
      Self::UnsignedRatio { .. } | Self::SignedRatio { .. } => None,
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryAggregateReducedValueRefV1<'a> {
  Count(u64),
  Numeric(QueryAggregateNumericV1),
  Ordered(&'a LogicalOrderComponentOwnedV1),
  Empty,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum QueryNumericAccumulatorV1 {
  U64(u128),
  I64(i128),
  FiniteF64 { sum_bits: u64, compensation_bits: u64 },
}

impl QueryNumericAccumulatorV1 {
  fn new(comparator: PositionComparatorV1) -> Result<Self, QueryExecutionSinkErrorV1> {
    match comparator {
      PositionComparatorV1::U64 => Ok(Self::U64(0)),
      PositionComparatorV1::I64 => Ok(Self::I64(0)),
      PositionComparatorV1::FiniteF64 => Ok(Self::FiniteF64 { sum_bits: 0, compensation_bits: 0 }),
      _ => Err(sink_error(
        QueryExecutionSinkErrorClassV1::CorruptSource,
        "query_aggregate_numeric_comparator",
        format!("aggregate numeric reducer cannot use comparator {comparator:?}"),
      )),
    }
  }

  fn reset(&mut self) {
    match self {
      Self::U64(value) => *value = 0,
      Self::I64(value) => *value = 0,
      Self::FiniteF64 { sum_bits, compensation_bits } => {
        *sum_bits = 0;
        *compensation_bits = 0;
      }
    }
  }

  fn add(
    &mut self,
    comparator: PositionComparatorV1,
    component: &LogicalOrderComponentOwnedV1,
    field: &str,
  ) -> Result<(), QueryExecutionSinkErrorV1> {
    let value = decode_logical_numeric_component_v1(comparator, component, field).map_err(map_position_error)?;
    match (self, value) {
      (Self::U64(sum), LogicalNumericValueV1::U64(value)) => {
        *sum = sum.checked_add(value as u128).ok_or_else(|| numeric_overflow(field))?;
      }
      (Self::I64(sum), LogicalNumericValueV1::I64(value)) => {
        *sum = sum.checked_add(value as i128).ok_or_else(|| numeric_overflow(field))?;
      }
      (Self::FiniteF64 { sum_bits, compensation_bits }, LogicalNumericValueV1::FiniteF64Bits(value_bits)) => {
        let sum = f64::from_bits(*sum_bits);
        let compensation = f64::from_bits(*compensation_bits);
        let value = f64::from_bits(value_bits);
        let next = sum + value;
        if !next.is_finite() {
          return Err(numeric_overflow(field));
        }
        let adjustment = if sum.abs() >= value.abs() { (sum - next) + value } else { (value - next) + sum };
        let next_compensation = compensation + adjustment;
        let combined = next + next_compensation;
        if !next_compensation.is_finite() || !combined.is_finite() {
          return Err(numeric_overflow(field));
        }
        *sum_bits = canonical_f64_bits(next);
        *compensation_bits = canonical_f64_bits(next_compensation);
      }
      _ => {
        return Err(sink_error(
          QueryExecutionSinkErrorClassV1::CorruptSource,
          "query_aggregate_numeric_type",
          format!("aggregate field {field} returned a numeric payload for the wrong comparator"),
        ));
      }
    }
    Ok(())
  }

  fn result(&self, denominator: u64) -> QueryAggregateNumericV1 {
    debug_assert_ne!(denominator, 0);
    match self {
      Self::U64(numerator) => QueryAggregateNumericV1::UnsignedRatio { numerator: *numerator, denominator },
      Self::I64(numerator) => QueryAggregateNumericV1::SignedRatio { numerator: *numerator, denominator },
      Self::FiniteF64 { sum_bits, compensation_bits } => {
        let total = (f64::from_bits(*sum_bits) + f64::from_bits(*compensation_bits)) / denominator as f64;
        QueryAggregateNumericV1::FiniteF64Bits(canonical_f64_bits(total))
      }
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct QueryAggregateAccumulatorV1 {
  present_value_count: u64,
  numeric: Option<QueryNumericAccumulatorV1>,
  minimum: Option<LogicalOrderComponentOwnedV1>,
  maximum: Option<LogicalOrderComponentOwnedV1>,
}

impl QueryAggregateAccumulatorV1 {
  fn new(comparator: PositionComparatorV1, operations: &[QueryAggregateKindV1]) -> Result<Self, QueryExecutionSinkErrorV1> {
    let numeric = if operations.contains(&QueryAggregateKindV1::Sum) || operations.contains(&QueryAggregateKindV1::Average) {
      Some(QueryNumericAccumulatorV1::new(comparator)?)
    } else {
      None
    };
    Ok(Self { present_value_count: 0, numeric, minimum: None, maximum: None })
  }

  fn value(&self, operations: &[QueryAggregateKindV1], kind: QueryAggregateKindV1) -> Option<QueryAggregateReducedValueRefV1<'_>> {
    if !operations.contains(&kind) {
      return None;
    }
    Some(match kind {
      QueryAggregateKindV1::Count => QueryAggregateReducedValueRefV1::Count(self.present_value_count),
      QueryAggregateKindV1::Sum => match (&self.numeric, self.present_value_count) {
        (Some(numeric), count) if count != 0 => QueryAggregateReducedValueRefV1::Numeric(numeric.result(1)),
        _ => QueryAggregateReducedValueRefV1::Empty,
      },
      QueryAggregateKindV1::Average => match (&self.numeric, self.present_value_count) {
        (Some(numeric), count) if count != 0 => QueryAggregateReducedValueRefV1::Numeric(numeric.result(count)),
        _ => QueryAggregateReducedValueRefV1::Empty,
      },
      QueryAggregateKindV1::Minimum => match &self.minimum {
        Some(value) => QueryAggregateReducedValueRefV1::Ordered(value),
        None => QueryAggregateReducedValueRefV1::Empty,
      },
      QueryAggregateKindV1::Maximum => match &self.maximum {
        Some(value) => QueryAggregateReducedValueRefV1::Ordered(value),
        None => QueryAggregateReducedValueRefV1::Empty,
      },
    })
  }

  fn reset(&mut self) {
    self.present_value_count = 0;
    if let Some(numeric) = &mut self.numeric {
      numeric.reset();
    }
    self.minimum = None;
    self.maximum = None;
  }
}

fn canonical_f64_bits(value: f64) -> u64 {
  if value == 0.0 {
    0
  } else {
    value.to_bits()
  }
}

fn numeric_overflow(field: &str) -> QueryExecutionSinkErrorV1 {
  sink_error(
    QueryExecutionSinkErrorClassV1::ResourceLimit,
    "query_aggregate_numeric_overflow",
    format!("aggregate numeric result for {field} exceeds its admitted representation"),
  )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryUngroupedAggregateFieldV1 {
  field_name: String,
  comparator: PositionComparatorV1,
  operations: Vec<QueryAggregateKindV1>,
  accumulator: QueryAggregateAccumulatorV1,
}

impl QueryUngroupedAggregateFieldV1 {
  pub fn field_name(&self) -> &str {
    &self.field_name
  }

  pub const fn comparator(&self) -> PositionComparatorV1 {
    self.comparator
  }

  pub fn operations(&self) -> &[QueryAggregateKindV1] {
    &self.operations
  }

  pub const fn present_value_count(&self) -> u64 {
    self.accumulator.present_value_count
  }

  pub fn value(&self, kind: QueryAggregateKindV1) -> Option<QueryAggregateReducedValueRefV1<'_>> {
    self.accumulator.value(&self.operations, kind)
  }

  fn reset(&mut self) {
    self.accumulator.reset();
  }
}

pub struct QueryUngroupedAggregateResultV1 {
  selected_namespace_root: [u8; MAXIMUM_IDENTITY_BYTES],
  selected_namespace_root_length: usize,
  scope_id: [u8; MAXIMUM_IDENTITY_BYTES],
  scope_id_length: usize,
  fields: Vec<QueryUngroupedAggregateFieldV1>,
  document_count: u64,
  examined_documents: u64,
  examined_field_values: u64,
  aggregate_values_examined: u64,
  retained_bytes: u64,
  _memory: MemoryReservation,
}

impl fmt::Debug for QueryUngroupedAggregateResultV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("QueryUngroupedAggregateResultV1")
      .field("selected_namespace_root", &hex::encode(self.selected_namespace_root()))
      .field("scope_id", &self.scope_id().map(hex::encode))
      .field("fields", &self.fields)
      .field("document_count", &self.document_count)
      .field("examined_documents", &self.examined_documents)
      .field("examined_field_values", &self.examined_field_values)
      .field("aggregate_values_examined", &self.aggregate_values_examined)
      .field("retained_bytes", &self.retained_bytes)
      .finish_non_exhaustive()
  }
}

impl QueryUngroupedAggregateResultV1 {
  pub fn selected_namespace_root(&self) -> &[u8] {
    &self.selected_namespace_root[..self.selected_namespace_root_length]
  }

  pub fn scope_id(&self) -> Option<&[u8]> {
    (self.scope_id_length != 0).then_some(&self.scope_id[..self.scope_id_length])
  }

  pub fn fields(&self) -> &[QueryUngroupedAggregateFieldV1] {
    &self.fields
  }

  pub fn field(&self, field_name: &str) -> Option<&QueryUngroupedAggregateFieldV1> {
    self.fields.iter().find(|field| field.field_name == field_name)
  }

  pub const fn document_count(&self) -> u64 {
    self.document_count
  }

  pub const fn examined_documents(&self) -> u64 {
    self.examined_documents
  }

  pub const fn examined_field_values(&self) -> u64 {
    self.examined_field_values
  }

  pub const fn aggregate_values_examined(&self) -> u64 {
    self.aggregate_values_examined
  }

  pub const fn retained_bytes(&self) -> u64 {
    self.retained_bytes
  }
}

pub struct QueryUngroupedAggregateSinkV1<'input, 'source, 'runtime> {
  input: &'input CompiledQueryAggregateInputV1,
  source: &'source mut dyn QueryAggregateInputSourceV1,
  cancellation: &'runtime CancellationToken,
  fields: Vec<QueryUngroupedAggregateFieldV1>,
  memory: MemoryReservation,
  limits: QueryUngroupedAggregateLimitsV1,
  base_retained_bytes: u64,
  retained_bytes: u64,
  selected_namespace_root: [u8; MAXIMUM_IDENTITY_BYTES],
  selected_namespace_root_length: usize,
  scope_id: [u8; MAXIMUM_IDENTITY_BYTES],
  scope_id_length: usize,
  maximum_matches: u64,
  document_count: u64,
  examined_documents: u64,
  examined_field_values: u64,
  aggregate_values_examined: u64,
  active: bool,
  failed: bool,
  committed: bool,
}

impl<'input, 'source, 'runtime> QueryUngroupedAggregateSinkV1<'input, 'source, 'runtime> {
  pub fn new(
    input: &'input CompiledQueryAggregateInputV1,
    source: &'source mut dyn QueryAggregateInputSourceV1,
    memory: &MemoryCoordinator,
    cancellation: &'runtime CancellationToken,
    limits: QueryUngroupedAggregateLimitsV1,
  ) -> Result<Self, QueryExecutionSinkErrorV1> {
    if input.fields.is_empty()
      || input.selected_namespace_root.is_empty()
      || input.selected_namespace_root.len() > MAXIMUM_IDENTITY_BYTES
      || input.selected_namespace_root.iter().all(|byte| *byte == 0)
    {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::CorruptSource,
        "query_aggregate_reducer_input",
        "ungrouped aggregate reducer received an invalid compiled input",
      ));
    }
    let reservation =
      memory.reserve(MemoryOwner::Query, limits.maximum_retained_bytes, AdmissionClass::Workload).map_err(map_memory_error)?;
    let mut fields = Vec::new();
    fields.try_reserve_exact(input.fields.len()).map_err(|source| {
      sink_error(
        QueryExecutionSinkErrorClassV1::ResourceLimit,
        "query_aggregate_reducer_reserve",
        format!("cannot reserve aggregate reducer fields: {source}"),
      )
    })?;
    for compiled in &input.fields {
      let mut operations = Vec::new();
      operations.try_reserve_exact(compiled.operations.len()).map_err(|source| {
        sink_error(
          QueryExecutionSinkErrorClassV1::ResourceLimit,
          "query_aggregate_reducer_reserve",
          format!("cannot reserve aggregate reducer operations: {source}"),
        )
      })?;
      for operation in &compiled.operations {
        let CompiledQueryAuxiliaryOperationV1::Aggregate(kind) = operation else {
          return Err(sink_error(
            QueryExecutionSinkErrorClassV1::CorruptSource,
            "query_aggregate_reducer_grouped",
            "ungrouped aggregate reducer cannot consume sort or group operations",
          ));
        };
        operations.push(*kind);
      }
      if operations.is_empty() {
        return Err(sink_error(
          QueryExecutionSinkErrorClassV1::CorruptSource,
          "query_aggregate_reducer_operations",
          format!("aggregate field {} has no reducer operation", compiled.field_name),
        ));
      }
      let accumulator = QueryAggregateAccumulatorV1::new(compiled.comparator, &operations)?;
      fields.push(QueryUngroupedAggregateFieldV1 {
        field_name: try_clone_string(&compiled.field_name, "aggregate reducer field name")?,
        comparator: compiled.comparator,
        operations,
        accumulator,
      });
    }
    let base_retained_bytes = ungrouped_retained_bytes(&fields, fields.capacity())?;
    require_sink_retained_limit(base_retained_bytes, limits.maximum_retained_bytes)?;
    let mut selected_namespace_root = [0u8; MAXIMUM_IDENTITY_BYTES];
    selected_namespace_root[..input.selected_namespace_root.len()].copy_from_slice(&input.selected_namespace_root);
    Ok(Self {
      input,
      source,
      cancellation,
      fields,
      memory: reservation,
      limits,
      base_retained_bytes,
      retained_bytes: base_retained_bytes,
      selected_namespace_root,
      selected_namespace_root_length: input.selected_namespace_root.len(),
      scope_id: [0u8; MAXIMUM_IDENTITY_BYTES],
      scope_id_length: 0,
      maximum_matches: 0,
      document_count: 0,
      examined_documents: 0,
      examined_field_values: 0,
      aggregate_values_examined: 0,
      active: false,
      failed: false,
      committed: false,
    })
  }

  pub fn finish(self) -> Result<QueryUngroupedAggregateResultV1, QueryExecutionSinkErrorV1> {
    if self.active || self.failed || !self.committed {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::Internal,
        "query_aggregate_reducer_state",
        "ungrouped aggregate result escaped without exactly one committed sink batch",
      ));
    }
    let Self {
      fields,
      memory,
      selected_namespace_root,
      selected_namespace_root_length,
      scope_id,
      scope_id_length,
      document_count,
      examined_documents,
      examined_field_values,
      aggregate_values_examined,
      retained_bytes,
      ..
    } = self;
    Ok(QueryUngroupedAggregateResultV1 {
      selected_namespace_root,
      selected_namespace_root_length,
      scope_id,
      scope_id_length,
      fields,
      document_count,
      examined_documents,
      examined_field_values,
      aggregate_values_examined,
      retained_bytes,
      _memory: memory,
    })
  }

  fn scope_id(&self) -> Option<&[u8]> {
    (self.scope_id_length != 0).then_some(&self.scope_id[..self.scope_id_length])
  }

  fn push_match_inner(&mut self, matched: QueryExecutionMatchRefV1<'_>) -> Result<(), QueryExecutionSinkErrorV1> {
    if self.document_count >= self.maximum_matches {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::ResourceLimit,
        "query_aggregate_reducer_matches",
        "ungrouped aggregate reducer exceeded its admitted match bound",
      ));
    }
    require_sink_not_cancelled(self.cancellation)?;
    let request = QueryAggregateInputLookupRequestV1::new(self.input, matched.file_key, matched.record_revision);
    let row = match resolve_query_aggregate_input_v1(request, self.source, self.cancellation).map_err(map_input_source_error)? {
      QueryAggregateInputLookupResultV1::Found(row) => row,
      QueryAggregateInputLookupResultV1::Absent => {
        return Err(sink_error(
          QueryExecutionSinkErrorClassV1::CorruptSource,
          "query_aggregate_reducer_absent",
          "exact aggregate input unexpectedly resolved as absent",
        ));
      }
    };
    for (index, field) in row.fields.iter().enumerate() {
      let value_count = u64::try_from(field.values.len()).map_err(|source| {
        sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_reducer_values", source.to_string())
      })?;
      self.aggregate_values_examined = self.aggregate_values_examined.checked_add(value_count).ok_or_else(|| {
        sink_error(
          QueryExecutionSinkErrorClassV1::ResourceLimit,
          "query_aggregate_reducer_values",
          "aggregate examined-value count overflowed",
        )
      })?;
      for value in &field.values {
        if value.state == PositionComponentStateV1::TypedNull {
          continue;
        }
        self.reduce_present_value(index, value)?;
      }
    }
    require_sink_not_cancelled(self.cancellation)?;
    self.document_count = self.document_count.checked_add(1).ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_reducer_matches", "aggregate document count overflowed")
    })?;
    Ok(())
  }

  fn reduce_present_value(&mut self, field_index: usize, value: &LogicalOrderComponentOwnedV1) -> Result<(), QueryExecutionSinkErrorV1> {
    let field = self.fields.get_mut(field_index).ok_or_else(|| {
      sink_error(
        QueryExecutionSinkErrorClassV1::Internal,
        "query_aggregate_reducer_field",
        "aggregate field index escaped its compiled set",
      )
    })?;
    self.retained_bytes = reduce_accumulator_present_value(
      &field.field_name,
      field.comparator,
      &field.operations,
      &mut field.accumulator,
      value,
      self.retained_bytes,
      self.limits.maximum_retained_bytes,
    )?;
    Ok(())
  }

  fn reset_transaction(&mut self) {
    for field in &mut self.fields {
      field.reset();
    }
    self.retained_bytes = self.base_retained_bytes;
    self.scope_id.fill(0);
    self.scope_id_length = 0;
    self.maximum_matches = 0;
    self.document_count = 0;
    self.examined_documents = 0;
    self.examined_field_values = 0;
    self.aggregate_values_examined = 0;
    self.active = false;
    self.failed = false;
  }
}

impl QueryExecutionMatchSinkV1 for QueryUngroupedAggregateSinkV1<'_, '_, '_> {
  fn begin_batch(&mut self, batch: QueryExecutionSinkBatchV1<'_>) -> Result<(), QueryExecutionSinkErrorV1> {
    if self.active
      || self.failed
      || self.committed
      || self.document_count != 0
      || batch.selected_namespace_root != self.input.selected_namespace_root
      || batch.maximum_matches == 0
      || batch.scope_id.is_some_and(|scope| scope.len() != self.selected_namespace_root_length || scope.iter().all(|byte| *byte == 0))
    {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::Internal,
        "query_aggregate_reducer_state",
        "ungrouped aggregate reducer received an invalid sink transaction",
      ));
    }
    require_sink_not_cancelled(self.cancellation)?;
    self.maximum_matches = batch.maximum_matches;
    self.scope_id.fill(0);
    self.scope_id_length = if let Some(scope) = batch.scope_id {
      self.scope_id[..scope.len()].copy_from_slice(scope);
      scope.len()
    } else {
      0
    };
    self.active = true;
    Ok(())
  }

  fn push_match(&mut self, matched: QueryExecutionMatchRefV1<'_>) -> Result<(), QueryExecutionSinkErrorV1> {
    if !self.active || self.failed || self.committed {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::Internal,
        "query_aggregate_reducer_state",
        "ungrouped aggregate reducer received a match outside an active transaction",
      ));
    }
    match self.push_match_inner(matched) {
      Ok(()) => Ok(()),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }

  fn commit_batch(&mut self, receipt: QueryExecutionSinkBatchReceiptV1<'_>) -> Result<(), QueryExecutionSinkErrorV1> {
    if !self.active
      || self.failed
      || self.committed
      || receipt.selected_namespace_root != self.input.selected_namespace_root
      || receipt.scope_id != self.scope_id()
      || receipt.match_count != self.document_count
    {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::Internal,
        "query_aggregate_reducer_receipt",
        "ungrouped aggregate reducer received an inconsistent commit receipt",
      ));
    }
    require_sink_not_cancelled(self.cancellation)?;
    let recomputed = ungrouped_retained_bytes(&self.fields, self.fields.capacity())?;
    if recomputed != self.retained_bytes {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::Internal,
        "query_aggregate_reducer_bytes",
        "aggregate incremental and recomputed memory accounting disagree",
      ));
    }
    let release = self.memory.bytes().checked_sub(recomputed).ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::Internal, "query_aggregate_reducer_bytes", "aggregate result exceeds its reservation")
    })?;
    self.memory.shrink(release).map_err(map_shrink_error)?;
    self.examined_documents = receipt.examined_documents;
    self.examined_field_values = receipt.examined_field_values;
    self.active = false;
    self.committed = true;
    Ok(())
  }

  fn rollback_batch(&mut self) {
    if self.committed {
      return;
    }
    self.reset_transaction();
  }
}

const GROUP_TUPLE_MAGIC_V1: &[u8; 4] = b"AGTP";
const GROUP_TUPLE_VERSION_V1: u16 = 1;
const GROUPED_FIXED_RETAINED_BYTES: u64 = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryGroupedAggregateLimitsV1 {
  maximum_groups: usize,
  maximum_group_tuple_bytes: u64,
  maximum_retained_bytes: u64,
}

impl QueryGroupedAggregateLimitsV1 {
  pub fn new(
    maximum_groups: usize,
    maximum_group_tuple_bytes: u64,
    maximum_retained_bytes: u64,
  ) -> Result<Self, QueryExecutionSinkErrorV1> {
    if maximum_groups == 0 || maximum_group_tuple_bytes == 0 || maximum_retained_bytes == 0 {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::ResourceLimit,
        "query_aggregate_group_limits",
        "group count, tuple bytes, and retained-memory limits must be nonzero",
      ));
    }
    Ok(Self { maximum_groups, maximum_group_tuple_bytes, maximum_retained_bytes })
  }

  pub const fn maximum_groups(self) -> usize {
    self.maximum_groups
  }

  pub const fn maximum_group_tuple_bytes(self) -> u64 {
    self.maximum_group_tuple_bytes
  }

  pub const fn maximum_retained_bytes(self) -> u64 {
    self.maximum_retained_bytes
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryGroupedAggregateFieldDefinitionV1 {
  field_name: String,
  comparator: PositionComparatorV1,
  operations: Vec<QueryAggregateKindV1>,
  input_field_index: usize,
}

impl QueryGroupedAggregateFieldDefinitionV1 {
  pub fn field_name(&self) -> &str {
    &self.field_name
  }

  pub const fn comparator(&self) -> PositionComparatorV1 {
    self.comparator
  }

  pub fn operations(&self) -> &[QueryAggregateKindV1] {
    &self.operations
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryAggregateGroupFieldDefinitionV1 {
  field_name: String,
  comparator: PositionComparatorV1,
}

impl QueryAggregateGroupFieldDefinitionV1 {
  pub fn field_name(&self) -> &str {
    &self.field_name
  }

  pub const fn comparator(&self) -> PositionComparatorV1 {
    self.comparator
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryAggregateGroupV1 {
  position_row: LogicalOrderRowOwnedV1,
  document_count: u64,
  aggregate_fields: Vec<QueryAggregateAccumulatorV1>,
}

impl QueryAggregateGroupV1 {
  pub const fn position_row(&self) -> &LogicalOrderRowOwnedV1 {
    &self.position_row
  }

  pub fn canonical_group_tuple(&self) -> &[u8] {
    match self.position_row.components.last() {
      Some(component) => &component.payload,
      None => &[],
    }
  }

  pub const fn document_count(&self) -> u64 {
    self.document_count
  }
}

pub struct QueryGroupedAggregateResultV1 {
  selected_namespace_root: [u8; MAXIMUM_IDENTITY_BYTES],
  selected_namespace_root_length: usize,
  scope_id: [u8; MAXIMUM_IDENTITY_BYTES],
  scope_id_length: usize,
  group_fields: Vec<QueryAggregateGroupFieldDefinitionV1>,
  aggregate_fields: Vec<QueryGroupedAggregateFieldDefinitionV1>,
  groups: Vec<QueryAggregateGroupV1>,
  total_document_count: u64,
  total_group_count: u64,
  examined_documents: u64,
  examined_field_values: u64,
  aggregate_values_examined: u64,
  retained_bytes: u64,
  _memory: MemoryReservation,
}

impl fmt::Debug for QueryGroupedAggregateResultV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("QueryGroupedAggregateResultV1")
      .field("selected_namespace_root", &hex::encode(self.selected_namespace_root()))
      .field("scope_id", &self.scope_id().map(hex::encode))
      .field("group_fields", &self.group_fields)
      .field("aggregate_fields", &self.aggregate_fields)
      .field("groups", &self.groups)
      .field("total_document_count", &self.total_document_count)
      .field("total_group_count", &self.total_group_count)
      .field("examined_documents", &self.examined_documents)
      .field("examined_field_values", &self.examined_field_values)
      .field("aggregate_values_examined", &self.aggregate_values_examined)
      .field("retained_bytes", &self.retained_bytes)
      .finish_non_exhaustive()
  }
}

impl QueryGroupedAggregateResultV1 {
  pub fn selected_namespace_root(&self) -> &[u8] {
    &self.selected_namespace_root[..self.selected_namespace_root_length]
  }

  pub fn scope_id(&self) -> Option<&[u8]> {
    (self.scope_id_length != 0).then_some(&self.scope_id[..self.scope_id_length])
  }

  pub fn group_fields(&self) -> &[QueryAggregateGroupFieldDefinitionV1] {
    &self.group_fields
  }

  pub fn aggregate_fields(&self) -> &[QueryGroupedAggregateFieldDefinitionV1] {
    &self.aggregate_fields
  }

  pub fn groups(&self) -> &[QueryAggregateGroupV1] {
    &self.groups
  }

  pub const fn total_document_count(&self) -> u64 {
    self.total_document_count
  }

  pub const fn total_group_count(&self) -> u64 {
    self.total_group_count
  }

  pub fn has_more(&self) -> bool {
    self.total_group_count > self.groups.len() as u64
  }

  pub const fn examined_documents(&self) -> u64 {
    self.examined_documents
  }

  pub const fn examined_field_values(&self) -> u64 {
    self.examined_field_values
  }

  pub const fn aggregate_values_examined(&self) -> u64 {
    self.aggregate_values_examined
  }

  pub const fn retained_bytes(&self) -> u64 {
    self.retained_bytes
  }

  pub fn group_value(
    &self,
    group_index: usize,
    field_name: &str,
    kind: QueryAggregateKindV1,
  ) -> Option<QueryAggregateReducedValueRefV1<'_>> {
    let group = self.groups.get(group_index)?;
    let field_index = self.aggregate_fields.iter().position(|field| field.field_name == field_name)?;
    let definition = &self.aggregate_fields[field_index];
    group.aggregate_fields.get(field_index)?.value(&definition.operations, kind)
  }

  pub fn group_value_by_tuple(
    &self,
    canonical_group_tuple: &[u8],
    field_name: &str,
    kind: QueryAggregateKindV1,
  ) -> Option<QueryAggregateReducedValueRefV1<'_>> {
    let group_index = self.groups.iter().position(|group| group.canonical_group_tuple() == canonical_group_tuple)?;
    self.group_value(group_index, field_name, kind)
  }
}

pub struct QueryGroupedAggregateSinkV1<'input, 'source, 'runtime> {
  input: &'input CompiledQueryAggregateInputV1,
  source: &'source mut dyn QueryAggregateInputSourceV1,
  cancellation: &'runtime CancellationToken,
  group_fields: Vec<QueryAggregateGroupFieldDefinitionV1>,
  aggregate_fields: Vec<QueryGroupedAggregateFieldDefinitionV1>,
  groups: Vec<QueryAggregateGroupV1>,
  result_groups: Vec<QueryAggregateGroupV1>,
  memory: MemoryReservation,
  limits: QueryGroupedAggregateLimitsV1,
  base_retained_bytes: u64,
  retained_bytes: u64,
  selected_namespace_root: [u8; MAXIMUM_IDENTITY_BYTES],
  selected_namespace_root_length: usize,
  scope_id: [u8; MAXIMUM_IDENTITY_BYTES],
  scope_id_length: usize,
  maximum_matches: u64,
  document_count: u64,
  total_group_count: u64,
  examined_documents: u64,
  examined_field_values: u64,
  aggregate_values_examined: u64,
  active: bool,
  failed: bool,
  committed: bool,
}

impl<'input, 'source, 'runtime> QueryGroupedAggregateSinkV1<'input, 'source, 'runtime> {
  pub fn new(
    input: &'input CompiledQueryAggregateInputV1,
    source: &'source mut dyn QueryAggregateInputSourceV1,
    memory: &MemoryCoordinator,
    cancellation: &'runtime CancellationToken,
    limits: QueryGroupedAggregateLimitsV1,
  ) -> Result<Self, QueryExecutionSinkErrorV1> {
    let group_order = input.group_order().ok_or_else(|| {
      sink_error(
        QueryExecutionSinkErrorClassV1::CorruptSource,
        "query_aggregate_group_input",
        "grouped aggregate reducer requires at least one compiled group field",
      )
    })?;
    if input.result_limit == 0
      || input.result_limit > limits.maximum_groups
      || input.selected_namespace_root.is_empty()
      || input.selected_namespace_root.len() > MAXIMUM_IDENTITY_BYTES
      || input.selected_namespace_root.iter().all(|byte| *byte == 0)
      || group_order.route() != PositionRouteV1::AggregateGroups
      || group_order.hash_algorithm() != input.hash_algorithm
      || group_order.component_count() != 2
    {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::CorruptSource,
        "query_aggregate_group_input",
        "grouped aggregate reducer received an invalid result bound, root, or permanent order",
      ));
    }

    let constructor_preflight = grouped_constructor_preflight_bytes(input, limits)?;
    require_sink_retained_limit(constructor_preflight, limits.maximum_retained_bytes)?;

    let reservation =
      memory.reserve(MemoryOwner::Query, limits.maximum_retained_bytes, AdmissionClass::Workload).map_err(map_memory_error)?;
    let group_fields = compile_group_field_definitions(input)?;
    let aggregate_fields = compile_grouped_aggregate_field_definitions(input)?;
    let mut groups = Vec::new();
    groups.try_reserve_exact(limits.maximum_groups).map_err(|source| {
      sink_error(
        QueryExecutionSinkErrorClassV1::ResourceLimit,
        "query_aggregate_group_reserve",
        format!("cannot reserve bounded aggregate groups: {source}"),
      )
    })?;
    let mut result_groups = Vec::new();
    result_groups.try_reserve_exact(input.result_limit).map_err(|source| {
      sink_error(
        QueryExecutionSinkErrorClassV1::ResourceLimit,
        "query_aggregate_group_reserve",
        format!("cannot reserve aggregate group top-K: {source}"),
      )
    })?;
    let memory_shape = GroupedAggregateMemoryShapeV1::new(
      &group_fields,
      group_fields.capacity(),
      &aggregate_fields,
      aggregate_fields.capacity(),
      result_groups.capacity(),
    )?;
    let base_retained_bytes = grouped_sink_base_retained_bytes(memory_shape, groups.capacity(), limits.maximum_group_tuple_bytes)?;
    require_sink_retained_limit(base_retained_bytes, limits.maximum_retained_bytes)?;
    let mut selected_namespace_root = [0u8; MAXIMUM_IDENTITY_BYTES];
    selected_namespace_root[..input.selected_namespace_root.len()].copy_from_slice(&input.selected_namespace_root);
    Ok(Self {
      input,
      source,
      cancellation,
      group_fields,
      aggregate_fields,
      groups,
      result_groups,
      memory: reservation,
      limits,
      base_retained_bytes,
      retained_bytes: base_retained_bytes,
      selected_namespace_root,
      selected_namespace_root_length: input.selected_namespace_root.len(),
      scope_id: [0u8; MAXIMUM_IDENTITY_BYTES],
      scope_id_length: 0,
      maximum_matches: 0,
      document_count: 0,
      total_group_count: 0,
      examined_documents: 0,
      examined_field_values: 0,
      aggregate_values_examined: 0,
      active: false,
      failed: false,
      committed: false,
    })
  }

  pub fn finish(self) -> Result<QueryGroupedAggregateResultV1, QueryExecutionSinkErrorV1> {
    if self.active || self.failed || !self.committed || !self.groups.is_empty() {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::Internal,
        "query_aggregate_group_state",
        "grouped aggregate result escaped without exactly one committed sink batch",
      ));
    }
    let Self {
      group_fields,
      aggregate_fields,
      result_groups,
      memory,
      selected_namespace_root,
      selected_namespace_root_length,
      scope_id,
      scope_id_length,
      document_count,
      total_group_count,
      examined_documents,
      examined_field_values,
      aggregate_values_examined,
      retained_bytes,
      ..
    } = self;
    Ok(QueryGroupedAggregateResultV1 {
      selected_namespace_root,
      selected_namespace_root_length,
      scope_id,
      scope_id_length,
      group_fields,
      aggregate_fields,
      groups: result_groups,
      total_document_count: document_count,
      total_group_count,
      examined_documents,
      examined_field_values,
      aggregate_values_examined,
      retained_bytes,
      _memory: memory,
    })
  }

  fn scope_id(&self) -> Option<&[u8]> {
    (self.scope_id_length != 0).then_some(&self.scope_id[..self.scope_id_length])
  }

  fn ensure_group_capacity(&mut self) -> Result<(), QueryExecutionSinkErrorV1> {
    if self.groups.capacity() >= self.limits.maximum_groups {
      return Ok(());
    }
    self.groups.try_reserve_exact(self.limits.maximum_groups).map_err(|source| {
      sink_error(
        QueryExecutionSinkErrorClassV1::ResourceLimit,
        "query_aggregate_group_reserve",
        format!("cannot restore aggregate group capacity after rollback: {source}"),
      )
    })
  }

  fn push_match_inner(&mut self, matched: QueryExecutionMatchRefV1<'_>) -> Result<(), QueryExecutionSinkErrorV1> {
    if self.document_count >= self.maximum_matches {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::ResourceLimit,
        "query_aggregate_group_matches",
        "grouped aggregate reducer exceeded its admitted match bound",
      ));
    }
    require_sink_not_cancelled(self.cancellation)?;
    let request = QueryAggregateInputLookupRequestV1::new(self.input, matched.file_key, matched.record_revision);
    let row = match resolve_query_aggregate_input_v1(request, self.source, self.cancellation).map_err(map_input_source_error)? {
      QueryAggregateInputLookupResultV1::Found(row) => row,
      QueryAggregateInputLookupResultV1::Absent => {
        return Err(sink_error(
          QueryExecutionSinkErrorClassV1::CorruptSource,
          "query_aggregate_group_absent",
          "exact grouped aggregate input unexpectedly resolved as absent",
        ));
      }
    };
    for field in &row.fields {
      let value_count = u64::try_from(field.values.len())
        .map_err(|source| sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_values", source.to_string()))?;
      self.aggregate_values_examined = self.aggregate_values_examined.checked_add(value_count).ok_or_else(|| {
        sink_error(
          QueryExecutionSinkErrorClassV1::ResourceLimit,
          "query_aggregate_group_values",
          "grouped aggregate examined-value count overflowed",
        )
      })?;
    }
    let tuple = encode_canonical_group_tuple_v1(self.input, &row, self.limits.maximum_group_tuple_bytes)?;
    match self.groups.binary_search_by(|group| group.canonical_group_tuple().cmp(&tuple)) {
      Ok(index) => self.reduce_existing_group(index, &row)?,
      Err(index) => self.insert_new_group(index, tuple, &row)?,
    }
    require_sink_not_cancelled(self.cancellation)?;
    self.document_count = self.document_count.checked_add(1).ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_matches", "grouped document count overflowed")
    })?;
    Ok(())
  }

  fn reduce_existing_group(&mut self, group_index: usize, row: &QueryAggregateInputRowV1) -> Result<(), QueryExecutionSinkErrorV1> {
    let group = self.groups.get_mut(group_index).ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::Internal, "query_aggregate_group_index", "aggregate group index escaped its set")
    })?;
    let mut retained_bytes = self.retained_bytes;
    for (field_index, definition) in self.aggregate_fields.iter().enumerate() {
      let input_field = &row.fields[definition.input_field_index];
      let accumulator = &mut group.aggregate_fields[field_index];
      for value in &input_field.values {
        if value.state == PositionComponentStateV1::TypedNull {
          continue;
        }
        retained_bytes = reduce_accumulator_present_value(
          &definition.field_name,
          definition.comparator,
          &definition.operations,
          accumulator,
          value,
          retained_bytes,
          self.limits.maximum_retained_bytes,
        )?;
      }
    }
    group.document_count = group.document_count.checked_add(1).ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_count", "aggregate group count overflowed")
    })?;
    set_group_count_component(group)?;
    self.retained_bytes = retained_bytes;
    Ok(())
  }

  fn insert_new_group(&mut self, index: usize, tuple: Vec<u8>, row: &QueryAggregateInputRowV1) -> Result<(), QueryExecutionSinkErrorV1> {
    if self.groups.len() >= self.limits.maximum_groups {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::ResourceLimit,
        "query_aggregate_group_limit",
        "grouped aggregation exceeded its distinct-group limit",
      ));
    }
    let preflight = new_group_dynamic_preflight_bytes(self.input, &self.aggregate_fields, row, tuple.len())?;
    let prospective = self.retained_bytes.checked_add(preflight).ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate group preflight overflowed")
    })?;
    require_sink_retained_limit(prospective, self.limits.maximum_retained_bytes)?;

    let (group, retained_bytes) =
      build_new_group_v1(self.input, &self.aggregate_fields, tuple, row, self.retained_bytes, self.limits.maximum_retained_bytes)?;
    let actual = group_dynamic_allocated_bytes(&group)?;
    if actual != preflight {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::Internal,
        "query_aggregate_group_bytes",
        format!("aggregate group preflight {preflight} differs from allocated bytes {actual}"),
      ));
    }
    self.groups.insert(index, group);
    self.retained_bytes = retained_bytes;
    Ok(())
  }

  fn reset_transaction(&mut self) {
    self.groups.clear();
    self.result_groups.clear();
    self.retained_bytes = self.base_retained_bytes;
    self.scope_id.fill(0);
    self.scope_id_length = 0;
    self.maximum_matches = 0;
    self.document_count = 0;
    self.total_group_count = 0;
    self.examined_documents = 0;
    self.examined_field_values = 0;
    self.aggregate_values_examined = 0;
    self.active = false;
    self.failed = false;
  }
}

impl QueryExecutionMatchSinkV1 for QueryGroupedAggregateSinkV1<'_, '_, '_> {
  fn begin_batch(&mut self, batch: QueryExecutionSinkBatchV1<'_>) -> Result<(), QueryExecutionSinkErrorV1> {
    if self.active
      || self.failed
      || self.committed
      || !self.groups.is_empty()
      || !self.result_groups.is_empty()
      || self.document_count != 0
      || batch.selected_namespace_root != self.input.selected_namespace_root
      || batch.maximum_matches == 0
      || batch.scope_id.is_some_and(|scope| scope.len() != self.selected_namespace_root_length || scope.iter().all(|byte| *byte == 0))
    {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::Internal,
        "query_aggregate_group_state",
        "grouped aggregate reducer received an invalid sink transaction",
      ));
    }
    require_sink_not_cancelled(self.cancellation)?;
    self.ensure_group_capacity()?;
    self.maximum_matches = batch.maximum_matches;
    self.scope_id.fill(0);
    self.scope_id_length = if let Some(scope) = batch.scope_id {
      self.scope_id[..scope.len()].copy_from_slice(scope);
      scope.len()
    } else {
      0
    };
    self.active = true;
    Ok(())
  }

  fn push_match(&mut self, matched: QueryExecutionMatchRefV1<'_>) -> Result<(), QueryExecutionSinkErrorV1> {
    if !self.active || self.failed || self.committed {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::Internal,
        "query_aggregate_group_state",
        "grouped aggregate reducer received a match outside an active transaction",
      ));
    }
    match self.push_match_inner(matched) {
      Ok(()) => Ok(()),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }

  fn commit_batch(&mut self, receipt: QueryExecutionSinkBatchReceiptV1<'_>) -> Result<(), QueryExecutionSinkErrorV1> {
    if !self.active
      || self.failed
      || self.committed
      || receipt.selected_namespace_root != self.input.selected_namespace_root
      || receipt.scope_id != self.scope_id()
      || receipt.match_count != self.document_count
    {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::Internal,
        "query_aggregate_group_receipt",
        "grouped aggregate reducer received an inconsistent commit receipt",
      ));
    }
    require_sink_not_cancelled(self.cancellation)?;
    let memory_shape = GroupedAggregateMemoryShapeV1::new(
      &self.group_fields,
      self.group_fields.capacity(),
      &self.aggregate_fields,
      self.aggregate_fields.capacity(),
      self.result_groups.capacity(),
    )?;
    let recomputed =
      grouped_sink_retained_bytes(memory_shape, &self.groups, self.groups.capacity(), self.limits.maximum_group_tuple_bytes)?;
    if recomputed != self.retained_bytes {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::Internal,
        "query_aggregate_group_bytes",
        "aggregate group incremental and recomputed memory accounting disagree",
      ));
    }
    let group_order = self.input.group_order().ok_or_else(|| {
      sink_error(
        QueryExecutionSinkErrorClassV1::Internal,
        "query_aggregate_group_order",
        "validated grouped aggregate input lost its permanent order",
      )
    })?;
    heap_sort_groups(group_order, &mut self.groups)?;
    self.total_group_count = u64::try_from(self.groups.len())
      .map_err(|source| sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_count", source.to_string()))?;
    let retained_groups = self.groups.len().min(self.input.result_limit);
    let groups = std::mem::take(&mut self.groups);
    for group in groups.into_iter().take(retained_groups) {
      self.result_groups.push(group);
    }
    let memory_shape = GroupedAggregateMemoryShapeV1::new(
      &self.group_fields,
      self.group_fields.capacity(),
      &self.aggregate_fields,
      self.aggregate_fields.capacity(),
      self.result_groups.capacity(),
    )?;
    let result_retained = grouped_result_retained_bytes(memory_shape, &self.result_groups)?;
    let release = self.memory.bytes().checked_sub(result_retained).ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::Internal, "query_aggregate_group_bytes", "aggregate group result exceeds its reservation")
    })?;
    self.memory.shrink(release).map_err(map_shrink_error)?;
    self.retained_bytes = result_retained;
    self.examined_documents = receipt.examined_documents;
    self.examined_field_values = receipt.examined_field_values;
    self.active = false;
    self.committed = true;
    Ok(())
  }

  fn rollback_batch(&mut self) {
    if self.committed {
      return;
    }
    self.reset_transaction();
  }
}

fn compile_group_field_definitions(
  input: &CompiledQueryAggregateInputV1,
) -> Result<Vec<QueryAggregateGroupFieldDefinitionV1>, QueryExecutionSinkErrorV1> {
  let mut output = Vec::new();
  output.try_reserve_exact(input.group_field_indices.len()).map_err(|source| {
    sink_error(
      QueryExecutionSinkErrorClassV1::ResourceLimit,
      "query_aggregate_group_reserve",
      format!("cannot reserve aggregate group-field definitions: {source}"),
    )
  })?;
  for index in &input.group_field_indices {
    let field = input.fields.get(*index).ok_or_else(|| {
      sink_error(
        QueryExecutionSinkErrorClassV1::CorruptSource,
        "query_aggregate_group_field",
        "compiled aggregate group-field index escaped its input",
      )
    })?;
    if !field.operations.contains(&CompiledQueryAuxiliaryOperationV1::Group) {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::CorruptSource,
        "query_aggregate_group_field",
        format!("compiled aggregate field {} is not a group field", field.field_name),
      ));
    }
    output.push(QueryAggregateGroupFieldDefinitionV1 {
      field_name: try_clone_string(&field.field_name, "aggregate group field name")?,
      comparator: field.comparator,
    });
  }
  Ok(output)
}

fn grouped_constructor_preflight_bytes(
  input: &CompiledQueryAggregateInputV1,
  limits: QueryGroupedAggregateLimitsV1,
) -> Result<u64, QueryExecutionSinkErrorV1> {
  let group_field_slots =
    input.group_field_indices.len().checked_mul(size_of::<QueryAggregateGroupFieldDefinitionV1>()).ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "group-field preflight overflowed")
    })?;
  let mut bytes = add_sink_capacity(GROUPED_FIXED_RETAINED_BYTES, group_field_slots, "aggregate group-field slots")?;
  for index in &input.group_field_indices {
    let field = input.fields.get(*index).ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::CorruptSource, "query_aggregate_group_field", "group-field preflight index escaped input")
    })?;
    bytes = add_sink_capacity(bytes, field.field_name.len(), "aggregate group-field name")?;
  }
  let aggregate_count = input
    .fields
    .iter()
    .filter(|field| field.operations.iter().any(|operation| matches!(operation, CompiledQueryAuxiliaryOperationV1::Aggregate(_))))
    .count();
  let aggregate_slots = aggregate_count.checked_mul(size_of::<QueryGroupedAggregateFieldDefinitionV1>()).ok_or_else(|| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate-field preflight overflowed")
  })?;
  bytes = add_sink_capacity(bytes, aggregate_slots, "grouped aggregate-field slots")?;
  for field in &input.fields {
    let operation_count =
      field.operations.iter().filter(|operation| matches!(operation, CompiledQueryAuxiliaryOperationV1::Aggregate(_))).count();
    if operation_count == 0 {
      continue;
    }
    bytes = add_sink_capacity(bytes, field.field_name.len(), "grouped aggregate-field name")?;
    let operation_slots = operation_count.checked_mul(size_of::<QueryAggregateKindV1>()).ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate operation preflight overflowed")
    })?;
    bytes = add_sink_capacity(bytes, operation_slots, "grouped aggregate operation slots")?;
  }
  let group_slots = limits.maximum_groups.checked_mul(size_of::<QueryAggregateGroupV1>()).ok_or_else(|| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate group preflight overflowed")
  })?;
  bytes = add_sink_capacity(bytes, group_slots, "aggregate group slots")?;
  let result_slots = input.result_limit.checked_mul(size_of::<QueryAggregateGroupV1>()).ok_or_else(|| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate result preflight overflowed")
  })?;
  bytes = add_sink_capacity(bytes, result_slots, "aggregate result-group slots")?;
  bytes.checked_add(ALLOCATION_OVERHEAD_BYTES).and_then(|bytes| bytes.checked_add(limits.maximum_group_tuple_bytes)).ok_or_else(|| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate tuple scratch preflight overflowed")
  })
}

fn compile_grouped_aggregate_field_definitions(
  input: &CompiledQueryAggregateInputV1,
) -> Result<Vec<QueryGroupedAggregateFieldDefinitionV1>, QueryExecutionSinkErrorV1> {
  let count = input
    .fields
    .iter()
    .filter(|field| field.operations.iter().any(|operation| matches!(operation, CompiledQueryAuxiliaryOperationV1::Aggregate(_))))
    .count();
  let mut output = Vec::new();
  output.try_reserve_exact(count).map_err(|source| {
    sink_error(
      QueryExecutionSinkErrorClassV1::ResourceLimit,
      "query_aggregate_group_reserve",
      format!("cannot reserve grouped aggregate field definitions: {source}"),
    )
  })?;
  for (input_field_index, field) in input.fields.iter().enumerate() {
    let mut operations = Vec::new();
    operations.try_reserve_exact(field.operations.len()).map_err(|source| {
      sink_error(
        QueryExecutionSinkErrorClassV1::ResourceLimit,
        "query_aggregate_group_reserve",
        format!("cannot reserve grouped aggregate operations: {source}"),
      )
    })?;
    for operation in &field.operations {
      if let CompiledQueryAuxiliaryOperationV1::Aggregate(kind) = operation {
        operations.push(*kind);
      }
    }
    if operations.is_empty() {
      continue;
    }
    output.push(QueryGroupedAggregateFieldDefinitionV1 {
      field_name: try_clone_string(&field.field_name, "grouped aggregate field name")?,
      comparator: field.comparator,
      operations,
      input_field_index,
    });
  }
  Ok(output)
}

fn canonical_group_tuple_length_v1(
  input: &CompiledQueryAggregateInputV1,
  row: &QueryAggregateInputRowV1,
) -> Result<usize, QueryExecutionSinkErrorV1> {
  let mut total = 8usize;
  let _field_count = u16::try_from(input.group_field_indices.len()).map_err(|source| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_tuple_length", source.to_string())
  })?;
  for index in &input.group_field_indices {
    let field = row.fields.get(*index).ok_or_else(|| {
      sink_error(
        QueryExecutionSinkErrorClassV1::CorruptSource,
        "query_aggregate_group_tuple_field",
        "group tuple field index escaped its selected-root row",
      )
    })?;
    let _value_count = u32::try_from(field.values.len()).map_err(|source| {
      sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_tuple_length", source.to_string())
    })?;
    let mut values_length = 0usize;
    for value in &field.values {
      let component = group_component_write_v1(value)?;
      values_length = values_length
        .checked_add(logical_position_component_encoded_length_v1(component).map_err(map_group_order_format_error)?)
        .ok_or_else(|| {
          sink_error(
            QueryExecutionSinkErrorClassV1::ResourceLimit,
            "query_aggregate_group_tuple_length",
            "canonical group value framing overflowed",
          )
        })?;
    }
    let _values_length = u32::try_from(values_length).map_err(|source| {
      sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_tuple_length", source.to_string())
    })?;
    total = total.checked_add(12).and_then(|bytes| bytes.checked_add(values_length)).ok_or_else(|| {
      sink_error(
        QueryExecutionSinkErrorClassV1::ResourceLimit,
        "query_aggregate_group_tuple_length",
        "canonical group tuple length overflowed",
      )
    })?;
  }
  Ok(total)
}

fn encode_canonical_group_tuple_v1(
  input: &CompiledQueryAggregateInputV1,
  row: &QueryAggregateInputRowV1,
  maximum_group_tuple_bytes: u64,
) -> Result<Vec<u8>, QueryExecutionSinkErrorV1> {
  let encoded_length = canonical_group_tuple_length_v1(input, row)?;
  let encoded_length_u64 = u64::try_from(encoded_length).map_err(|source| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_tuple_length", source.to_string())
  })?;
  if encoded_length_u64 > maximum_group_tuple_bytes {
    return Err(sink_error(
      QueryExecutionSinkErrorClassV1::ResourceLimit,
      "query_aggregate_group_tuple_limit",
      format!("canonical group tuple requires {encoded_length_u64} bytes, cap is {maximum_group_tuple_bytes}"),
    ));
  }
  let field_count = u16::try_from(input.group_field_indices.len()).map_err(|source| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_tuple_length", source.to_string())
  })?;
  let mut output = Vec::new();
  output.try_reserve_exact(encoded_length).map_err(|source| {
    sink_error(
      QueryExecutionSinkErrorClassV1::ResourceLimit,
      "query_aggregate_group_tuple_reserve",
      format!("cannot reserve canonical group tuple: {source}"),
    )
  })?;
  output.extend_from_slice(GROUP_TUPLE_MAGIC_V1);
  output.extend_from_slice(&GROUP_TUPLE_VERSION_V1.to_le_bytes());
  output.extend_from_slice(&field_count.to_le_bytes());
  for index in &input.group_field_indices {
    let field = &row.fields[*index];
    let state = match field.state {
      QueryExecutionFieldStateV1::Values => 0,
      QueryExecutionFieldStateV1::Missing => 1,
      QueryExecutionFieldStateV1::DeterministicUnindexable => 2,
    };
    let values_length = field.values.iter().try_fold(0usize, |bytes, value| {
      let component = group_component_write_v1(value)?;
      bytes.checked_add(logical_position_component_encoded_length_v1(component).map_err(map_group_order_format_error)?).ok_or_else(|| {
        sink_error(
          QueryExecutionSinkErrorClassV1::ResourceLimit,
          "query_aggregate_group_tuple_length",
          "canonical group value framing overflowed",
        )
      })
    })?;
    let value_count = u32::try_from(field.values.len()).map_err(|source| {
      sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_tuple_length", source.to_string())
    })?;
    let values_length = u32::try_from(values_length).map_err(|source| {
      sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_tuple_length", source.to_string())
    })?;
    output.push(state);
    output.extend_from_slice(&[0, 0, 0]);
    output.extend_from_slice(&value_count.to_le_bytes());
    output.extend_from_slice(&values_length.to_le_bytes());
    for value in &field.values {
      append_logical_position_component_v1(&mut output, group_component_write_v1(value)?).map_err(map_group_order_format_error)?;
    }
  }
  if output.len() != encoded_length {
    return Err(sink_error(
      QueryExecutionSinkErrorClassV1::Internal,
      "query_aggregate_group_tuple_length",
      "canonical group tuple preflight and encoder disagree",
    ));
  }
  Ok(output)
}

fn group_component_write_v1(value: &LogicalOrderComponentOwnedV1) -> Result<PositionComponentWriteV1<'_>, QueryExecutionSinkErrorV1> {
  if value.state == PositionComponentStateV1::Missing {
    return Err(sink_error(
      QueryExecutionSinkErrorClassV1::CorruptSource,
      "query_aggregate_group_tuple_value",
      "group value contains an illegal per-value missing state",
    ));
  }
  Ok(PositionComponentWriteV1 { comparator: value.comparator, state: value.state, payload: &value.payload })
}

fn new_group_dynamic_preflight_bytes(
  input: &CompiledQueryAggregateInputV1,
  aggregate_fields: &[QueryGroupedAggregateFieldDefinitionV1],
  row: &QueryAggregateInputRowV1,
  tuple_length: usize,
) -> Result<u64, QueryExecutionSinkErrorV1> {
  let hash_width = input.hash_algorithm.hash_length();
  let component_slots = 2usize.checked_mul(size_of::<LogicalOrderComponentOwnedV1>()).ok_or_else(|| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate group component slots overflowed")
  })?;
  let mut bytes = (ALLOCATION_OVERHEAD_BYTES * 5)
    .checked_add(
      u64::try_from(hash_width.checked_mul(2).ok_or_else(|| {
        sink_error(
          QueryExecutionSinkErrorClassV1::ResourceLimit,
          "query_aggregate_group_bytes",
          "aggregate group identity bytes overflowed",
        )
      })?)
      .map_err(|source| sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", source.to_string()))?,
    )
    .and_then(|bytes| bytes.checked_add(component_slots as u64))
    .and_then(|bytes| bytes.checked_add(8))
    .and_then(|bytes| bytes.checked_add(tuple_length as u64))
    .ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate group row preflight overflowed")
    })?;
  let accumulator_slots = aggregate_fields.len().checked_mul(size_of::<QueryAggregateAccumulatorV1>()).ok_or_else(|| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate state slots overflowed")
  })?;
  bytes = add_sink_capacity(bytes, accumulator_slots, "aggregate group state slots")?;
  for definition in aggregate_fields {
    let input_field = &row.fields[definition.input_field_index];
    if definition.operations.contains(&QueryAggregateKindV1::Minimum) {
      if let Some(value) = selected_extreme_value(input_field, definition, Ordering::Less)? {
        bytes = bytes.checked_add(component_retained_preflight_bytes(value)?).ok_or_else(|| {
          sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate minimum bytes overflowed")
        })?;
      }
    }
    if definition.operations.contains(&QueryAggregateKindV1::Maximum) {
      if let Some(value) = selected_extreme_value(input_field, definition, Ordering::Greater)? {
        bytes = bytes.checked_add(component_retained_preflight_bytes(value)?).ok_or_else(|| {
          sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate maximum bytes overflowed")
        })?;
      }
    }
  }
  Ok(bytes)
}

fn selected_extreme_value<'a>(
  input_field: &'a QueryAggregateInputFieldV1,
  definition: &QueryGroupedAggregateFieldDefinitionV1,
  direction: Ordering,
) -> Result<Option<&'a LogicalOrderComponentOwnedV1>, QueryExecutionSinkErrorV1> {
  let mut selected: Option<&LogicalOrderComponentOwnedV1> = None;
  for value in &input_field.values {
    if value.state != PositionComponentStateV1::Present {
      continue;
    }
    let replace = match selected {
      Some(current) => {
        compare_logical_order_components_v1(definition.comparator, value, current, &definition.field_name).map_err(map_position_error)?
          == direction
      }
      None => true,
    };
    if replace {
      selected = Some(value);
    }
  }
  Ok(selected)
}

fn component_retained_preflight_bytes(value: &LogicalOrderComponentOwnedV1) -> Result<u64, QueryExecutionSinkErrorV1> {
  u64::try_from(value.payload.len())
    .map_err(|source| sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", source.to_string()))?
    .checked_add(ALLOCATION_OVERHEAD_BYTES)
    .ok_or_else(|| sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "component bytes overflowed"))
}

fn build_new_group_v1(
  input: &CompiledQueryAggregateInputV1,
  aggregate_fields: &[QueryGroupedAggregateFieldDefinitionV1],
  tuple: Vec<u8>,
  row: &QueryAggregateInputRowV1,
  retained_bytes: u64,
  maximum_retained_bytes: u64,
) -> Result<(QueryAggregateGroupV1, u64), QueryExecutionSinkErrorV1> {
  let mut components = Vec::new();
  components.try_reserve_exact(2).map_err(|source| {
    sink_error(
      QueryExecutionSinkErrorClassV1::ResourceLimit,
      "query_aggregate_group_reserve",
      format!("cannot reserve aggregate group order components: {source}"),
    )
  })?;
  components.push(LogicalOrderComponentOwnedV1::present(
    PositionComparatorV1::U64,
    try_clone_bytes(&1u64.to_le_bytes(), "aggregate group count component")?,
  ));
  components.push(LogicalOrderComponentOwnedV1::present(PositionComparatorV1::BytesBinary, tuple));
  let position_row = LogicalOrderRowOwnedV1 {
    route: PositionRouteV1::AggregateGroups,
    file_key_tie: digest_parts(input.hash_algorithm, &[&components[1].payload]),
    record_revision_tie: try_clone_bytes(&input.selected_namespace_root, "aggregate group input root")?,
    components,
  };
  let mut states = Vec::new();
  states.try_reserve_exact(aggregate_fields.len()).map_err(|source| {
    sink_error(
      QueryExecutionSinkErrorClassV1::ResourceLimit,
      "query_aggregate_group_reserve",
      format!("cannot reserve aggregate group reducer states: {source}"),
    )
  })?;
  for definition in aggregate_fields {
    states.push(QueryAggregateAccumulatorV1::new(definition.comparator, &definition.operations)?);
  }
  let mut group = QueryAggregateGroupV1 { position_row, document_count: 1, aggregate_fields: states };
  let initial = group_dynamic_allocated_bytes(&group)?;
  let mut prospective = retained_bytes.checked_add(initial).ok_or_else(|| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate group bytes overflowed")
  })?;
  require_sink_retained_limit(prospective, maximum_retained_bytes)?;
  for (field_index, definition) in aggregate_fields.iter().enumerate() {
    let input_field = &row.fields[definition.input_field_index];
    for value in &input_field.values {
      if value.state == PositionComponentStateV1::TypedNull {
        continue;
      }
      prospective = reduce_accumulator_present_value(
        &definition.field_name,
        definition.comparator,
        &definition.operations,
        &mut group.aggregate_fields[field_index],
        value,
        prospective,
        maximum_retained_bytes,
      )?;
    }
  }
  Ok((group, prospective))
}

fn set_group_count_component(group: &mut QueryAggregateGroupV1) -> Result<(), QueryExecutionSinkErrorV1> {
  let component = group.position_row.components.first_mut().ok_or_else(|| {
    sink_error(QueryExecutionSinkErrorClassV1::Internal, "query_aggregate_group_count", "aggregate group has no count component")
  })?;
  if component.state != PositionComponentStateV1::Present
    || component.comparator != Some(PositionComparatorV1::U64)
    || component.payload.len() != 8
  {
    return Err(sink_error(
      QueryExecutionSinkErrorClassV1::Internal,
      "query_aggregate_group_count",
      "aggregate group count component is malformed",
    ));
  }
  component.payload.copy_from_slice(&group.document_count.to_le_bytes());
  Ok(())
}

fn group_dynamic_allocated_bytes(group: &QueryAggregateGroupV1) -> Result<u64, QueryExecutionSinkErrorV1> {
  let row = logical_order_row_allocated_bytes_v1(&group.position_row).map_err(map_position_error)?;
  let mut bytes = row.checked_sub(size_of::<LogicalOrderRowOwnedV1>() as u64).ok_or_else(|| {
    sink_error(QueryExecutionSinkErrorClassV1::Internal, "query_aggregate_group_bytes", "aggregate group row accounting underflowed")
  })?;
  let state_slots = group.aggregate_fields.capacity().checked_mul(size_of::<QueryAggregateAccumulatorV1>()).ok_or_else(|| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate group state capacity overflowed")
  })?;
  bytes = add_sink_capacity(bytes, state_slots, "aggregate group state slots")?;
  for state in &group.aggregate_fields {
    if let Some(minimum) = &state.minimum {
      bytes = bytes.checked_add(component_dynamic_bytes(minimum)?).ok_or_else(|| {
        sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate minimum bytes overflowed")
      })?;
    }
    if let Some(maximum) = &state.maximum {
      bytes = bytes.checked_add(component_dynamic_bytes(maximum)?).ok_or_else(|| {
        sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate maximum bytes overflowed")
      })?;
    }
  }
  Ok(bytes)
}

#[derive(Clone, Copy)]
struct GroupedAggregateMemoryShapeV1<'a> {
  group_fields: &'a [QueryAggregateGroupFieldDefinitionV1],
  group_field_capacity: usize,
  aggregate_fields: &'a [QueryGroupedAggregateFieldDefinitionV1],
  aggregate_field_capacity: usize,
  result_group_capacity: usize,
}

impl<'a> GroupedAggregateMemoryShapeV1<'a> {
  fn new(
    group_fields: &'a [QueryAggregateGroupFieldDefinitionV1],
    group_field_capacity: usize,
    aggregate_fields: &'a [QueryGroupedAggregateFieldDefinitionV1],
    aggregate_field_capacity: usize,
    result_group_capacity: usize,
  ) -> Result<Self, QueryExecutionSinkErrorV1> {
    if group_field_capacity < group_fields.len() || aggregate_field_capacity < aggregate_fields.len() {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::Internal,
        "query_aggregate_group_bytes",
        "aggregate memory shape capacity is smaller than its retained definitions",
      ));
    }
    Ok(Self { group_fields, group_field_capacity, aggregate_fields, aggregate_field_capacity, result_group_capacity })
  }
}

fn grouped_shared_retained_bytes(shape: GroupedAggregateMemoryShapeV1<'_>) -> Result<u64, QueryExecutionSinkErrorV1> {
  let GroupedAggregateMemoryShapeV1 {
    group_fields,
    group_field_capacity,
    aggregate_fields,
    aggregate_field_capacity,
    result_group_capacity,
  } = shape;
  let group_field_slots = group_field_capacity.checked_mul(size_of::<QueryAggregateGroupFieldDefinitionV1>()).ok_or_else(|| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "group-field slots overflowed")
  })?;
  let mut bytes = add_sink_capacity(GROUPED_FIXED_RETAINED_BYTES, group_field_slots, "aggregate group-field slots")?;
  for field in group_fields {
    bytes = add_sink_capacity(bytes, field.field_name.capacity(), "aggregate group-field name")?;
  }
  let aggregate_field_slots =
    aggregate_field_capacity.checked_mul(size_of::<QueryGroupedAggregateFieldDefinitionV1>()).ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate-field slots overflowed")
    })?;
  bytes = add_sink_capacity(bytes, aggregate_field_slots, "grouped aggregate-field slots")?;
  for field in aggregate_fields {
    bytes = add_sink_capacity(bytes, field.field_name.capacity(), "grouped aggregate-field name")?;
    let operation_slots = field.operations.capacity().checked_mul(size_of::<QueryAggregateKindV1>()).ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate operation slots overflowed")
    })?;
    bytes = add_sink_capacity(bytes, operation_slots, "grouped aggregate operation slots")?;
  }
  let result_slots = result_group_capacity.checked_mul(size_of::<QueryAggregateGroupV1>()).ok_or_else(|| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate result-group slots overflowed")
  })?;
  add_sink_capacity(bytes, result_slots, "aggregate result-group slots")
}

fn grouped_sink_base_retained_bytes(
  shape: GroupedAggregateMemoryShapeV1<'_>,
  group_capacity: usize,
  maximum_group_tuple_bytes: u64,
) -> Result<u64, QueryExecutionSinkErrorV1> {
  let mut bytes = grouped_shared_retained_bytes(shape)?;
  let group_slots = group_capacity.checked_mul(size_of::<QueryAggregateGroupV1>()).ok_or_else(|| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate group slots overflowed")
  })?;
  bytes = add_sink_capacity(bytes, group_slots, "aggregate group slots")?;
  bytes.checked_add(ALLOCATION_OVERHEAD_BYTES).and_then(|bytes| bytes.checked_add(maximum_group_tuple_bytes)).ok_or_else(|| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate tuple scratch bytes overflowed")
  })
}

fn grouped_sink_retained_bytes(
  shape: GroupedAggregateMemoryShapeV1<'_>,
  groups: &[QueryAggregateGroupV1],
  group_capacity: usize,
  maximum_group_tuple_bytes: u64,
) -> Result<u64, QueryExecutionSinkErrorV1> {
  groups.iter().try_fold(grouped_sink_base_retained_bytes(shape, group_capacity, maximum_group_tuple_bytes)?, |bytes, group| {
    bytes.checked_add(group_dynamic_allocated_bytes(group)?).ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate retained bytes overflowed")
    })
  })
}

fn grouped_result_retained_bytes(
  shape: GroupedAggregateMemoryShapeV1<'_>,
  groups: &[QueryAggregateGroupV1],
) -> Result<u64, QueryExecutionSinkErrorV1> {
  groups.iter().try_fold(grouped_shared_retained_bytes(shape)?, |bytes, group| {
    bytes.checked_add(group_dynamic_allocated_bytes(group)?).ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_group_bytes", "aggregate result bytes overflowed")
    })
  })
}

fn heap_sort_groups(order: &CompiledRouteOrderV1, groups: &mut [QueryAggregateGroupV1]) -> Result<(), QueryExecutionSinkErrorV1> {
  let length = groups.len();
  for start in (0..length / 2).rev() {
    sift_down_groups(order, groups, start, length)?;
  }
  for end in (1..length).rev() {
    groups.swap(0, end);
    sift_down_groups(order, groups, 0, end)?;
  }
  Ok(())
}

fn sift_down_groups(
  order: &CompiledRouteOrderV1,
  groups: &mut [QueryAggregateGroupV1],
  mut index: usize,
  end: usize,
) -> Result<(), QueryExecutionSinkErrorV1> {
  loop {
    let left = index.saturating_mul(2).saturating_add(1);
    if left >= end {
      return Ok(());
    }
    let right = left + 1;
    let child = if right < end
      && compare_logical_order_rows_v1(order, groups[right].position_row.as_borrowed(), groups[left].position_row.as_borrowed())
        .map_err(map_position_error)?
        == Ordering::Greater
    {
      right
    } else {
      left
    };
    if compare_logical_order_rows_v1(order, groups[child].position_row.as_borrowed(), groups[index].position_row.as_borrowed())
      .map_err(map_position_error)?
      != Ordering::Greater
    {
      return Ok(());
    }
    groups.swap(index, child);
    index = child;
  }
}

fn try_clone_component(value: &LogicalOrderComponentOwnedV1) -> Result<LogicalOrderComponentOwnedV1, QueryExecutionSinkErrorV1> {
  let mut payload = Vec::new();
  payload.try_reserve_exact(value.payload.len()).map_err(|source| {
    sink_error(
      QueryExecutionSinkErrorClassV1::ResourceLimit,
      "query_aggregate_reducer_reserve",
      format!("cannot reserve aggregate extrema payload: {source}"),
    )
  })?;
  payload.extend_from_slice(&value.payload);
  Ok(LogicalOrderComponentOwnedV1 { state: value.state, comparator: value.comparator, payload })
}

fn reduce_accumulator_present_value(
  field_name: &str,
  comparator: PositionComparatorV1,
  operations: &[QueryAggregateKindV1],
  accumulator: &mut QueryAggregateAccumulatorV1,
  value: &LogicalOrderComponentOwnedV1,
  retained_bytes: u64,
  maximum_retained_bytes: u64,
) -> Result<u64, QueryExecutionSinkErrorV1> {
  if value.state != PositionComponentStateV1::Present {
    return Err(sink_error(
      QueryExecutionSinkErrorClassV1::CorruptSource,
      "query_aggregate_reducer_value",
      format!("aggregate field {field_name} contains an unexpected non-present value"),
    ));
  }
  accumulator.present_value_count = accumulator.present_value_count.checked_add(1).ok_or_else(|| {
    sink_error(
      QueryExecutionSinkErrorClassV1::ResourceLimit,
      "query_aggregate_reducer_count",
      format!("aggregate field {field_name} value count overflowed"),
    )
  })?;
  if let Some(numeric) = &mut accumulator.numeric {
    numeric.add(comparator, value, field_name)?;
  }
  let replace_minimum = if operations.contains(&QueryAggregateKindV1::Minimum) {
    match &accumulator.minimum {
      Some(current) => {
        compare_logical_order_components_v1(comparator, value, current, field_name).map_err(map_position_error)? == Ordering::Less
      }
      None => true,
    }
  } else {
    false
  };
  let replace_maximum = if operations.contains(&QueryAggregateKindV1::Maximum) {
    match &accumulator.maximum {
      Some(current) => {
        compare_logical_order_components_v1(comparator, value, current, field_name).map_err(map_position_error)? == Ordering::Greater
      }
      None => true,
    }
  } else {
    false
  };
  let mut retained_bytes = retained_bytes;
  if replace_minimum {
    retained_bytes = replace_accumulator_extreme(&mut accumulator.minimum, value, retained_bytes, maximum_retained_bytes)?;
  }
  if replace_maximum {
    retained_bytes = replace_accumulator_extreme(&mut accumulator.maximum, value, retained_bytes, maximum_retained_bytes)?;
  }
  Ok(retained_bytes)
}

fn replace_accumulator_extreme(
  target: &mut Option<LogicalOrderComponentOwnedV1>,
  value: &LogicalOrderComponentOwnedV1,
  retained_bytes: u64,
  maximum_retained_bytes: u64,
) -> Result<u64, QueryExecutionSinkErrorV1> {
  let new_bytes = component_retained_preflight_bytes(value)?;
  let transient = retained_bytes.checked_add(new_bytes).ok_or_else(|| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_reducer_bytes", "aggregate retained bytes overflowed")
  })?;
  require_sink_retained_limit(transient, maximum_retained_bytes)?;
  let cloned = try_clone_component(value)?;
  if component_dynamic_bytes(&cloned)? != new_bytes {
    return Err(sink_error(
      QueryExecutionSinkErrorClassV1::Internal,
      "query_aggregate_reducer_bytes",
      "aggregate extrema preflight differs from allocated bytes",
    ));
  }
  let replaced = target.replace(cloned);
  let replaced_bytes = match &replaced {
    Some(component) => component_dynamic_bytes(component)?,
    None => 0,
  };
  transient.checked_sub(replaced_bytes).ok_or_else(|| {
    sink_error(QueryExecutionSinkErrorClassV1::Internal, "query_aggregate_reducer_bytes", "aggregate replacement accounting underflowed")
  })
}

fn ungrouped_retained_bytes(fields: &[QueryUngroupedAggregateFieldV1], field_capacity: usize) -> Result<u64, QueryExecutionSinkErrorV1> {
  let field_slots = field_capacity.checked_mul(size_of::<QueryUngroupedAggregateFieldV1>()).ok_or_else(|| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_reducer_bytes", "aggregate field capacity overflowed")
  })?;
  let mut bytes = add_sink_capacity(UNGROUPED_FIXED_RETAINED_BYTES, field_slots, "aggregate field slots")?;
  for field in fields {
    bytes = add_sink_capacity(bytes, field.field_name.capacity(), "aggregate field name")?;
    let operation_slots = field.operations.capacity().checked_mul(size_of::<QueryAggregateKindV1>()).ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_reducer_bytes", "aggregate operation capacity overflowed")
    })?;
    bytes = add_sink_capacity(bytes, operation_slots, "aggregate operation slots")?;
    if let Some(minimum) = &field.accumulator.minimum {
      bytes = bytes.checked_add(component_dynamic_bytes(minimum)?).ok_or_else(|| {
        sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_reducer_bytes", "aggregate minimum bytes overflowed")
      })?;
    }
    if let Some(maximum) = &field.accumulator.maximum {
      bytes = bytes.checked_add(component_dynamic_bytes(maximum)?).ok_or_else(|| {
        sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_reducer_bytes", "aggregate maximum bytes overflowed")
      })?;
    }
  }
  Ok(bytes)
}

fn component_dynamic_bytes(component: &LogicalOrderComponentOwnedV1) -> Result<u64, QueryExecutionSinkErrorV1> {
  let capacity = u64::try_from(component.payload.capacity()).map_err(|source| {
    sink_error(
      QueryExecutionSinkErrorClassV1::ResourceLimit,
      "query_aggregate_reducer_bytes",
      format!("aggregate component capacity is not representable: {source}"),
    )
  })?;
  capacity.checked_add(ALLOCATION_OVERHEAD_BYTES).ok_or_else(|| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_reducer_bytes", "aggregate component bytes overflowed")
  })
}

fn add_sink_capacity(bytes: u64, capacity: usize, label: &str) -> Result<u64, QueryExecutionSinkErrorV1> {
  let capacity = u64::try_from(capacity).map_err(|source| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_reducer_bytes", format!("{label}: {source}"))
  })?;
  bytes.checked_add(ALLOCATION_OVERHEAD_BYTES).and_then(|value| value.checked_add(capacity)).ok_or_else(|| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_reducer_bytes", format!("{label} overflowed"))
  })
}

fn require_sink_retained_limit(retained: u64, maximum: u64) -> Result<(), QueryExecutionSinkErrorV1> {
  if retained > maximum {
    return Err(sink_error(
      QueryExecutionSinkErrorClassV1::ResourceLimit,
      "query_aggregate_reducer_bytes",
      format!("aggregate reducer requires {retained} retained bytes, cap is {maximum}"),
    ));
  }
  Ok(())
}

fn require_sink_not_cancelled(cancellation: &CancellationToken) -> Result<(), QueryExecutionSinkErrorV1> {
  if cancellation.is_cancelled() {
    return Err(sink_error(
      QueryExecutionSinkErrorClassV1::Cancelled,
      "query_aggregate_reducer_cancelled",
      "ungrouped aggregation was cancelled",
    ));
  }
  Ok(())
}

fn map_input_source_error(error: QueryExecutionSourceErrorV1) -> QueryExecutionSinkErrorV1 {
  let class = match error.class() {
    QueryExecutionSourceErrorClassV1::Unavailable => QueryExecutionSinkErrorClassV1::HistoricalViewUnavailable,
    QueryExecutionSourceErrorClassV1::ResourceLimit => QueryExecutionSinkErrorClassV1::ResourceLimit,
    QueryExecutionSourceErrorClassV1::Corrupt => QueryExecutionSinkErrorClassV1::CorruptSource,
    QueryExecutionSourceErrorClassV1::Cancelled => QueryExecutionSinkErrorClassV1::Cancelled,
    QueryExecutionSourceErrorClassV1::Internal => QueryExecutionSinkErrorClassV1::Internal,
  };
  sink_error(class, error.code(), error.context())
}

fn map_position_error(error: super::position_order::PositionOrderErrorV1) -> QueryExecutionSinkErrorV1 {
  let class = match error.class() {
    PositionOrderErrorClassV1::ResourceLimit => QueryExecutionSinkErrorClassV1::ResourceLimit,
    PositionOrderErrorClassV1::Unavailable => QueryExecutionSinkErrorClassV1::HistoricalViewUnavailable,
    PositionOrderErrorClassV1::Cancelled => QueryExecutionSinkErrorClassV1::Cancelled,
    PositionOrderErrorClassV1::InvalidRequest | PositionOrderErrorClassV1::Corrupt => QueryExecutionSinkErrorClassV1::CorruptSource,
  };
  sink_error(class, error.code(), error.context())
}

fn map_group_order_format_error(error: super::reader::FormatError) -> QueryExecutionSinkErrorV1 {
  let class = match error.class() {
    MalformedInputClass::LengthCountOrArithmeticOverflow | MalformedInputClass::AllocationAmplification => {
      QueryExecutionSinkErrorClassV1::ResourceLimit
    }
    _ => QueryExecutionSinkErrorClassV1::CorruptSource,
  };
  sink_error(class, error.code(), error.context())
}

fn map_memory_error(error: MemoryCoordinatorError) -> QueryExecutionSinkErrorV1 {
  let class = match error {
    MemoryCoordinatorError::HardLimitExceeded { .. }
    | MemoryCoordinatorError::EmergencyReserveExceeded { .. }
    | MemoryCoordinatorError::SoftPressureDeferred { .. } => QueryExecutionSinkErrorClassV1::ResourceLimit,
    _ => QueryExecutionSinkErrorClassV1::Internal,
  };
  sink_error(class, "query_aggregate_reducer_memory", error.to_string())
}

fn map_shrink_error(error: MemoryCoordinatorError) -> QueryExecutionSinkErrorV1 {
  sink_error(QueryExecutionSinkErrorClassV1::Internal, "query_aggregate_reducer_memory", error.to_string())
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
    let configured_scope = field.scope_id.as_ref();
    if field.field_name != expected.field_name
      || configured_scope
        .is_some_and(|scope_id| !expected.scope_ids.contains(scope_id) || scope_id.len() != request.selected_namespace_root().len())
      || configured_scope.is_none() && (field.state != QueryExecutionFieldStateV1::Missing || !field.values.is_empty())
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
  let row_bytes = query_aggregate_input_row_allocated_bytes_v1(row)?;
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

pub(super) fn query_aggregate_input_row_allocated_bytes_v1(row: &QueryAggregateInputRowV1) -> Result<u64, QueryExecutionSourceErrorV1> {
  let mut bytes = u64::try_from(size_of::<QueryAggregateInputRowV1>()).map_err(|source| {
    source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "query_aggregate_input_row_bytes", source.to_string())
  })?;
  bytes = add_capacity(bytes, row.selected_namespace_root.capacity(), "selected root")?;
  bytes = add_capacity(bytes, row.file_key.capacity(), "FileKey")?;
  bytes = add_capacity(bytes, row.record_revision.capacity(), "record revision")?;
  bytes = add_slots::<QueryAggregateInputFieldV1>(bytes, row.fields.capacity(), "field slots")?;
  for field in &row.fields {
    bytes = add_capacity(bytes, field.field_name.capacity(), "field name")?;
    bytes = add_capacity(bytes, field.scope_id.as_ref().map_or(0, Vec::capacity), "ScopeId")?;
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
