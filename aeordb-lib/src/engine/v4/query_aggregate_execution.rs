//! Bounded selected-root input authority for incremental query aggregation.

use std::cmp::Ordering;
use std::fmt;
use std::mem::size_of;

use tokio_util::sync::CancellationToken;

use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

use super::position::{PositionComparatorV1, PositionComponentStateV1};
use super::position_order::{
  LogicalNumericValueV1, LogicalOrderComponentOwnedV1, PositionOrderErrorClassV1, compare_logical_order_components_v1,
  decode_logical_numeric_component_v1, validate_logical_order_component_v1,
};
use super::query_executor::{
  QueryExecutionFieldStateV1, QueryExecutionMatchRefV1, QueryExecutionMatchSinkV1, QueryExecutionSinkBatchReceiptV1,
  QueryExecutionSinkBatchV1, QueryExecutionSinkErrorClassV1, QueryExecutionSinkErrorV1, QueryExecutionSourceErrorClassV1,
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
  present_value_count: u64,
  numeric: Option<QueryNumericAccumulatorV1>,
  minimum: Option<LogicalOrderComponentOwnedV1>,
  maximum: Option<LogicalOrderComponentOwnedV1>,
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
    self.present_value_count
  }

  pub fn value(&self, kind: QueryAggregateKindV1) -> Option<QueryAggregateReducedValueRefV1<'_>> {
    if !self.operations.contains(&kind) {
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

  fn has_operation(&self, kind: QueryAggregateKindV1) -> bool {
    self.operations.contains(&kind)
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
      let numeric = if operations.contains(&QueryAggregateKindV1::Sum) || operations.contains(&QueryAggregateKindV1::Average) {
        Some(QueryNumericAccumulatorV1::new(compiled.comparator)?)
      } else {
        None
      };
      fields.push(QueryUngroupedAggregateFieldV1 {
        field_name: try_clone_string(&compiled.field_name, "aggregate reducer field name")?,
        comparator: compiled.comparator,
        operations,
        present_value_count: 0,
        numeric,
        minimum: None,
        maximum: None,
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
    if value.state != PositionComponentStateV1::Present {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::CorruptSource,
        "query_aggregate_reducer_value",
        format!("aggregate field {} contains an unexpected non-present value", field.field_name),
      ));
    }
    field.present_value_count = field.present_value_count.checked_add(1).ok_or_else(|| {
      sink_error(
        QueryExecutionSinkErrorClassV1::ResourceLimit,
        "query_aggregate_reducer_count",
        format!("aggregate field {} value count overflowed", field.field_name),
      )
    })?;
    if let Some(numeric) = &mut field.numeric {
      numeric.add(field.comparator, value, &field.field_name)?;
    }
    let replace_minimum = if field.has_operation(QueryAggregateKindV1::Minimum) {
      match &field.minimum {
        Some(current) => {
          compare_logical_order_components_v1(field.comparator, value, current, &field.field_name).map_err(map_position_error)?
            == Ordering::Less
        }
        None => true,
      }
    } else {
      false
    };
    let replace_maximum = if field.has_operation(QueryAggregateKindV1::Maximum) {
      match &field.maximum {
        Some(current) => {
          compare_logical_order_components_v1(field.comparator, value, current, &field.field_name).map_err(map_position_error)?
            == Ordering::Greater
        }
        None => true,
      }
    } else {
      false
    };
    if replace_minimum {
      self.replace_extreme(field_index, value, true)?;
    }
    if replace_maximum {
      self.replace_extreme(field_index, value, false)?;
    }
    Ok(())
  }

  fn replace_extreme(
    &mut self,
    field_index: usize,
    value: &LogicalOrderComponentOwnedV1,
    minimum: bool,
  ) -> Result<(), QueryExecutionSinkErrorV1> {
    let cloned = try_clone_component(value)?;
    let new_bytes = component_dynamic_bytes(&cloned)?;
    let transient = self.retained_bytes.checked_add(new_bytes).ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_reducer_bytes", "aggregate retained bytes overflowed")
    })?;
    require_sink_retained_limit(transient, self.limits.maximum_retained_bytes)?;
    let field = &mut self.fields[field_index];
    let replaced = if minimum { field.minimum.replace(cloned) } else { field.maximum.replace(cloned) };
    let replaced_bytes = match &replaced {
      Some(component) => component_dynamic_bytes(component)?,
      None => 0,
    };
    self.retained_bytes = transient.checked_sub(replaced_bytes).ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::Internal, "query_aggregate_reducer_bytes", "aggregate replacement accounting underflowed")
    })?;
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
    if let Some(minimum) = &field.minimum {
      bytes = bytes.checked_add(component_dynamic_bytes(minimum)?).ok_or_else(|| {
        sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_aggregate_reducer_bytes", "aggregate minimum bytes overflowed")
      })?;
    }
    if let Some(maximum) = &field.maximum {
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
