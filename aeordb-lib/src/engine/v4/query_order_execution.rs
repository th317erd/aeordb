use std::cmp::Ordering;
use std::fmt;
use std::mem::size_of;

use tokio_util::sync::CancellationToken;

use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

use super::position::PositionRouteV1;
use super::position_order::{
  LogicalOrderRowOwnedV1, PositionOrderErrorClassV1, compare_logical_order_rows_v1, logical_order_row_allocated_bytes_v1,
};
use super::position_resolver::{
  PositionUniverseLookupRequestV1, PositionUniverseLookupResultV1, PositionUniverseSourceErrorV1, PositionUniverseSourceV1,
  resolve_position_universe_row_v1,
};
use super::query_executor::{
  QueryExecutionMatchPathV1, QueryExecutionMatchRefV1, QueryExecutionMatchSinkV1, QueryExecutionSinkBatchReceiptV1,
  QueryExecutionSinkBatchV1, QueryExecutionSinkErrorClassV1, QueryExecutionSinkErrorV1,
};
use super::query_planner::CompiledRootAwareQueryPlanV1;

const MAXIMUM_IDENTITY_BYTES: usize = 64;
const TOP_K_FIXED_RETAINED_BYTES: u64 = 256;
const ALLOCATION_OVERHEAD_BYTES: u64 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryOrderedTopKLimitsV1 {
  maximum_row_bytes: u64,
  maximum_retained_bytes: u64,
}

impl QueryOrderedTopKLimitsV1 {
  pub fn new(maximum_row_bytes: u64, maximum_retained_bytes: u64) -> Result<Self, QueryExecutionSinkErrorV1> {
    if maximum_row_bytes == 0 || maximum_retained_bytes == 0 {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::ResourceLimit,
        "query_order_top_k_limits",
        "query top-K row and retained-memory limits must be nonzero",
      ));
    }
    Ok(Self { maximum_row_bytes, maximum_retained_bytes })
  }

  pub const fn maximum_row_bytes(self) -> u64 {
    self.maximum_row_bytes
  }

  pub const fn maximum_retained_bytes(self) -> u64 {
    self.maximum_retained_bytes
  }
}

pub struct QueryOrderedTopKResultV1 {
  selected_namespace_root: [u8; MAXIMUM_IDENTITY_BYTES],
  selected_namespace_root_length: usize,
  scope_id: [u8; MAXIMUM_IDENTITY_BYTES],
  scope_id_length: usize,
  rows: Vec<LogicalOrderRowOwnedV1>,
  total_match_count: u64,
  examined_documents: u64,
  examined_field_values: u64,
  retained_bytes: u64,
  _memory: MemoryReservation,
}

impl fmt::Debug for QueryOrderedTopKResultV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("QueryOrderedTopKResultV1")
      .field("selected_namespace_root", &hex::encode(self.selected_namespace_root()))
      .field("scope_id", &self.scope_id().map(hex::encode))
      .field("rows", &self.rows)
      .field("total_match_count", &self.total_match_count)
      .field("examined_documents", &self.examined_documents)
      .field("examined_field_values", &self.examined_field_values)
      .field("retained_bytes", &self.retained_bytes)
      .finish_non_exhaustive()
  }
}

impl QueryOrderedTopKResultV1 {
  pub fn selected_namespace_root(&self) -> &[u8] {
    &self.selected_namespace_root[..self.selected_namespace_root_length]
  }

  pub fn scope_id(&self) -> Option<&[u8]> {
    (self.scope_id_length != 0).then_some(&self.scope_id[..self.scope_id_length])
  }

  pub fn rows(&self) -> &[LogicalOrderRowOwnedV1] {
    &self.rows
  }

  pub const fn total_match_count(&self) -> u64 {
    self.total_match_count
  }

  pub fn has_more(&self) -> bool {
    self.total_match_count > self.rows.len() as u64
  }

  pub const fn examined_documents(&self) -> u64 {
    self.examined_documents
  }

  pub const fn examined_field_values(&self) -> u64 {
    self.examined_field_values
  }

  pub const fn retained_bytes(&self) -> u64 {
    self.retained_bytes
  }
}

pub struct QueryOrderedTopKSinkV1<'plan, 'source, 'runtime> {
  plan: &'plan CompiledRootAwareQueryPlanV1,
  source: &'source mut dyn PositionUniverseSourceV1,
  cancellation: &'runtime CancellationToken,
  rows: Vec<LogicalOrderRowOwnedV1>,
  memory: MemoryReservation,
  limits: QueryOrderedTopKLimitsV1,
  base_retained_bytes: u64,
  retained_bytes: u64,
  selected_namespace_root: [u8; MAXIMUM_IDENTITY_BYTES],
  selected_namespace_root_length: usize,
  scope_id: [u8; MAXIMUM_IDENTITY_BYTES],
  scope_id_length: usize,
  maximum_matches: u64,
  match_count: u64,
  examined_documents: u64,
  examined_field_values: u64,
  active: bool,
  committed: bool,
}

impl<'plan, 'source, 'runtime> QueryOrderedTopKSinkV1<'plan, 'source, 'runtime> {
  pub fn new(
    plan: &'plan CompiledRootAwareQueryPlanV1,
    source: &'source mut dyn PositionUniverseSourceV1,
    memory: &MemoryCoordinator,
    cancellation: &'runtime CancellationToken,
    limits: QueryOrderedTopKLimitsV1,
  ) -> Result<Self, QueryExecutionSinkErrorV1> {
    if plan.result_limit() == 0
      || plan.query_order().route() != PositionRouteV1::Query
      || plan.query_order().hash_algorithm() != plan.hash_algorithm()
      || plan.selected_namespace_root().len() != plan.hash_algorithm().hash_length()
      || plan.selected_namespace_root().len() > MAXIMUM_IDENTITY_BYTES
    {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::CorruptSource,
        "query_order_top_k_plan",
        "compiled query plan has an invalid result limit, route order, or selected-root identity",
      ));
    }

    let reservation =
      memory.reserve(MemoryOwner::Query, limits.maximum_retained_bytes, AdmissionClass::Workload).map_err(map_memory_error)?;
    let mut rows = Vec::new();
    rows.try_reserve_exact(plan.result_limit()).map_err(|error| {
      sink_error(
        QueryExecutionSinkErrorClassV1::ResourceLimit,
        "query_order_top_k_allocation",
        format!("cannot reserve bounded query top-K rows: {error}"),
      )
    })?;
    let base_retained_bytes = top_k_base_retained_bytes(rows.capacity())?;
    if base_retained_bytes > limits.maximum_retained_bytes {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::ResourceLimit,
        "query_order_top_k_capacity",
        "query top-K row capacity cannot fit the admitted retained-memory limit",
      ));
    }

    let mut selected_namespace_root = [0u8; MAXIMUM_IDENTITY_BYTES];
    selected_namespace_root[..plan.selected_namespace_root().len()].copy_from_slice(plan.selected_namespace_root());
    Ok(Self {
      plan,
      source,
      cancellation,
      rows,
      memory: reservation,
      limits,
      base_retained_bytes,
      retained_bytes: base_retained_bytes,
      selected_namespace_root,
      selected_namespace_root_length: plan.selected_namespace_root().len(),
      scope_id: [0u8; MAXIMUM_IDENTITY_BYTES],
      scope_id_length: 0,
      maximum_matches: 0,
      match_count: 0,
      examined_documents: 0,
      examined_field_values: 0,
      active: false,
      committed: false,
    })
  }

  pub fn finish(self) -> Result<QueryOrderedTopKResultV1, QueryExecutionSinkErrorV1> {
    if self.active || !self.committed {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::Internal,
        "query_order_top_k_state",
        "ordered query result escaped without exactly one committed sink batch",
      ));
    }
    let Self {
      rows,
      memory,
      retained_bytes,
      selected_namespace_root,
      selected_namespace_root_length,
      scope_id,
      scope_id_length,
      match_count,
      examined_documents,
      examined_field_values,
      ..
    } = self;
    Ok(QueryOrderedTopKResultV1 {
      selected_namespace_root,
      selected_namespace_root_length,
      scope_id,
      scope_id_length,
      rows,
      total_match_count: match_count,
      examined_documents,
      examined_field_values,
      retained_bytes,
      _memory: memory,
    })
  }

  fn resolve_match(&mut self, matched: QueryExecutionMatchRefV1<'_>) -> Result<LogicalOrderRowOwnedV1, QueryExecutionSinkErrorV1> {
    let request = PositionUniverseLookupRequestV1::new(
      self.plan.database_id(),
      self.plan.physical_instance_id(),
      self.plan.selected_namespace_root(),
      self.plan.query_order(),
      matched.file_key,
      matched.record_revision,
      self.limits.maximum_row_bytes,
    );
    let row = match resolve_position_universe_row_v1(request, self.source, self.cancellation).map_err(map_source_error)? {
      PositionUniverseLookupResultV1::Found(row) => row,
      PositionUniverseLookupResultV1::Absent => {
        return Err(sink_error(
          QueryExecutionSinkErrorClassV1::CorruptSource,
          "query_order_selected_root_identity",
          "an exact query match is absent from the selected-root position universe",
        ));
      }
    };
    if let QueryExecutionMatchPathV1::Canonical(path) = matched.path {
      let Some(component) = row.components.last() else {
        return Err(sink_error(
          QueryExecutionSinkErrorClassV1::CorruptSource,
          "query_order_selected_root_path",
          "selected-root query row has no canonical path component",
        ));
      };
      if component.payload.as_slice() != path.as_bytes() {
        return Err(sink_error(
          QueryExecutionSinkErrorClassV1::CorruptSource,
          "query_order_selected_root_path",
          "streamed canonical path differs from the selected-root query row",
        ));
      }
    }
    Ok(row)
  }

  fn retain_row(&mut self, row: LogicalOrderRowOwnedV1) -> Result<(), QueryExecutionSinkErrorV1> {
    let row_bytes = row_dynamic_allocated_bytes(&row)?;
    if self.rows.len() < self.plan.result_limit() {
      let prospective = self.retained_bytes.checked_add(row_bytes).ok_or_else(|| {
        sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_order_top_k_bytes", "query top-K retained bytes overflowed")
      })?;
      require_retained_limit(prospective, self.limits.maximum_retained_bytes)?;
      self.rows.push(row);
      self.retained_bytes = prospective;
      let index = self.rows.len() - 1;
      sift_up(self.plan.query_order(), &mut self.rows, index)?;
      return Ok(());
    }

    if compare_rows(self.plan.query_order(), &row, &self.rows[0])? != Ordering::Less {
      return Ok(());
    }
    let replaced_bytes = row_dynamic_allocated_bytes(&self.rows[0])?;
    let prospective = self.retained_bytes.checked_sub(replaced_bytes).and_then(|bytes| bytes.checked_add(row_bytes)).ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::Internal, "query_order_top_k_bytes", "query top-K replacement accounting underflowed")
    })?;
    require_retained_limit(prospective, self.limits.maximum_retained_bytes)?;
    self.rows[0] = row;
    self.retained_bytes = prospective;
    let length = self.rows.len();
    sift_down(self.plan.query_order(), &mut self.rows, 0, length)?;
    Ok(())
  }
}

impl QueryExecutionMatchSinkV1 for QueryOrderedTopKSinkV1<'_, '_, '_> {
  fn begin_batch(&mut self, batch: QueryExecutionSinkBatchV1<'_>) -> Result<(), QueryExecutionSinkErrorV1> {
    if self.active
      || self.committed
      || !self.rows.is_empty()
      || self.match_count != 0
      || batch.selected_namespace_root != self.plan.selected_namespace_root()
      || batch.maximum_matches < self.plan.result_limit() as u64
      || batch.scope_id.is_some_and(|scope| scope.is_empty() || scope.len() > MAXIMUM_IDENTITY_BYTES)
    {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::Internal,
        "query_order_top_k_state",
        "ordered query result received an invalid sink transaction",
      ));
    }
    if self.cancellation.is_cancelled() {
      return Err(sink_error(QueryExecutionSinkErrorClassV1::Cancelled, "query_order_top_k_cancelled", "query ordering was cancelled"));
    }
    self.maximum_matches = batch.maximum_matches;
    self.scope_id.fill(0);
    self.scope_id_length = if let Some(scope_id) = batch.scope_id {
      self.scope_id[..scope_id.len()].copy_from_slice(scope_id);
      scope_id.len()
    } else {
      0
    };
    self.active = true;
    Ok(())
  }

  fn push_match(&mut self, matched: QueryExecutionMatchRefV1<'_>) -> Result<(), QueryExecutionSinkErrorV1> {
    if !self.active || self.committed {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::Internal,
        "query_order_top_k_state",
        "ordered query result received a match outside an active transaction",
      ));
    }
    if self.match_count >= self.maximum_matches {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::ResourceLimit,
        "query_order_top_k_matches",
        "ordered query result exceeded its admitted match bound",
      ));
    }
    if self.cancellation.is_cancelled() {
      return Err(sink_error(QueryExecutionSinkErrorClassV1::Cancelled, "query_order_top_k_cancelled", "query ordering was cancelled"));
    }
    let row = self.resolve_match(matched)?;
    self.retain_row(row)?;
    if self.cancellation.is_cancelled() {
      return Err(sink_error(QueryExecutionSinkErrorClassV1::Cancelled, "query_order_top_k_cancelled", "query ordering was cancelled"));
    }
    self.match_count = self.match_count.checked_add(1).ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_order_top_k_matches", "query top-K match count overflowed")
    })?;
    Ok(())
  }

  fn commit_batch(&mut self, receipt: QueryExecutionSinkBatchReceiptV1<'_>) -> Result<(), QueryExecutionSinkErrorV1> {
    if !self.active
      || self.committed
      || receipt.selected_namespace_root != self.plan.selected_namespace_root()
      || receipt.scope_id != self.scope_id()
      || receipt.match_count != self.match_count
    {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::Internal,
        "query_order_top_k_receipt",
        "ordered query result received an inconsistent commit receipt",
      ));
    }
    if self.cancellation.is_cancelled() {
      return Err(sink_error(QueryExecutionSinkErrorClassV1::Cancelled, "query_order_top_k_cancelled", "query ordering was cancelled"));
    }
    heap_sort(self.plan.query_order(), &mut self.rows)?;
    let recomputed = top_k_retained_bytes(&self.rows, self.rows.capacity())?;
    if recomputed != self.retained_bytes {
      return Err(sink_error(
        QueryExecutionSinkErrorClassV1::Internal,
        "query_order_top_k_bytes",
        "query top-K incremental and recomputed memory accounting disagree",
      ));
    }
    let release = self.memory.bytes().checked_sub(recomputed).ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::Internal, "query_order_top_k_bytes", "query top-K result exceeds its reservation")
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
    self.rows.clear();
    self.retained_bytes = self.base_retained_bytes;
    self.scope_id.fill(0);
    self.scope_id_length = 0;
    self.maximum_matches = 0;
    self.match_count = 0;
    self.examined_documents = 0;
    self.examined_field_values = 0;
    self.active = false;
  }
}

impl QueryOrderedTopKSinkV1<'_, '_, '_> {
  fn scope_id(&self) -> Option<&[u8]> {
    (self.scope_id_length != 0).then_some(&self.scope_id[..self.scope_id_length])
  }
}

fn sift_up(
  order: &super::position::CompiledRouteOrderV1,
  rows: &mut [LogicalOrderRowOwnedV1],
  mut index: usize,
) -> Result<(), QueryExecutionSinkErrorV1> {
  while index != 0 {
    let parent = (index - 1) / 2;
    if compare_rows(order, &rows[index], &rows[parent])? != Ordering::Greater {
      break;
    }
    rows.swap(index, parent);
    index = parent;
  }
  Ok(())
}

fn sift_down(
  order: &super::position::CompiledRouteOrderV1,
  rows: &mut [LogicalOrderRowOwnedV1],
  mut index: usize,
  end: usize,
) -> Result<(), QueryExecutionSinkErrorV1> {
  loop {
    let left = index.saturating_mul(2).saturating_add(1);
    if left >= end {
      return Ok(());
    }
    let right = left + 1;
    let largest = if right < end && compare_rows(order, &rows[right], &rows[left])? == Ordering::Greater { right } else { left };
    if compare_rows(order, &rows[largest], &rows[index])? != Ordering::Greater {
      return Ok(());
    }
    rows.swap(index, largest);
    index = largest;
  }
}

fn heap_sort(order: &super::position::CompiledRouteOrderV1, rows: &mut [LogicalOrderRowOwnedV1]) -> Result<(), QueryExecutionSinkErrorV1> {
  for end in (1..rows.len()).rev() {
    rows.swap(0, end);
    sift_down(order, rows, 0, end)?;
  }
  Ok(())
}

fn compare_rows(
  order: &super::position::CompiledRouteOrderV1,
  left: &LogicalOrderRowOwnedV1,
  right: &LogicalOrderRowOwnedV1,
) -> Result<Ordering, QueryExecutionSinkErrorV1> {
  compare_logical_order_rows_v1(order, left.as_borrowed(), right.as_borrowed()).map_err(|error| {
    let class = match error.class() {
      PositionOrderErrorClassV1::ResourceLimit => QueryExecutionSinkErrorClassV1::ResourceLimit,
      PositionOrderErrorClassV1::Unavailable => QueryExecutionSinkErrorClassV1::HistoricalViewUnavailable,
      PositionOrderErrorClassV1::Cancelled => QueryExecutionSinkErrorClassV1::Cancelled,
      PositionOrderErrorClassV1::InvalidRequest | PositionOrderErrorClassV1::Corrupt => QueryExecutionSinkErrorClassV1::CorruptSource,
    };
    sink_error(class, error.code(), error.context())
  })
}

fn top_k_base_retained_bytes(capacity: usize) -> Result<u64, QueryExecutionSinkErrorV1> {
  let capacity_bytes = capacity.checked_mul(size_of::<LogicalOrderRowOwnedV1>()).ok_or_else(|| {
    sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_order_top_k_bytes", "query top-K row capacity overflowed")
  })?;
  TOP_K_FIXED_RETAINED_BYTES.checked_add(ALLOCATION_OVERHEAD_BYTES).and_then(|bytes| bytes.checked_add(capacity_bytes as u64)).ok_or_else(
    || sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_order_top_k_bytes", "query top-K base bytes overflowed"),
  )
}

fn top_k_retained_bytes(rows: &[LogicalOrderRowOwnedV1], capacity: usize) -> Result<u64, QueryExecutionSinkErrorV1> {
  rows.iter().try_fold(top_k_base_retained_bytes(capacity)?, |bytes, row| {
    bytes.checked_add(row_dynamic_allocated_bytes(row)?).ok_or_else(|| {
      sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_order_top_k_bytes", "query top-K retained bytes overflowed")
    })
  })
}

fn row_dynamic_allocated_bytes(row: &LogicalOrderRowOwnedV1) -> Result<u64, QueryExecutionSinkErrorV1> {
  logical_order_row_allocated_bytes_v1(row)
    .map_err(|error| sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, error.code(), error.context()))?
    .checked_sub(size_of::<LogicalOrderRowOwnedV1>() as u64)
    .ok_or_else(|| sink_error(QueryExecutionSinkErrorClassV1::Internal, "query_order_top_k_bytes", "query row accounting underflowed"))
}

fn require_retained_limit(retained: u64, maximum: u64) -> Result<(), QueryExecutionSinkErrorV1> {
  if retained > maximum {
    return Err(sink_error(
      QueryExecutionSinkErrorClassV1::ResourceLimit,
      "query_order_top_k_bytes",
      format!("query top-K requires {retained} retained bytes, cap is {maximum}"),
    ));
  }
  Ok(())
}

fn map_source_error(error: PositionUniverseSourceErrorV1) -> QueryExecutionSinkErrorV1 {
  match error {
    PositionUniverseSourceErrorV1::Unavailable(context) => {
      sink_error(QueryExecutionSinkErrorClassV1::HistoricalViewUnavailable, "query_order_selected_root_unavailable", context)
    }
    PositionUniverseSourceErrorV1::ResourceLimit(context) => {
      sink_error(QueryExecutionSinkErrorClassV1::ResourceLimit, "query_order_selected_root_resource", context)
    }
    PositionUniverseSourceErrorV1::Corrupt(context) => {
      sink_error(QueryExecutionSinkErrorClassV1::CorruptSource, "query_order_selected_root_corrupt", context)
    }
    PositionUniverseSourceErrorV1::Cancelled => {
      sink_error(QueryExecutionSinkErrorClassV1::Cancelled, "query_order_top_k_cancelled", "query ordering was cancelled")
    }
  }
}

fn map_memory_error(error: MemoryCoordinatorError) -> QueryExecutionSinkErrorV1 {
  let class = match error {
    MemoryCoordinatorError::HardLimitExceeded { .. }
    | MemoryCoordinatorError::EmergencyReserveExceeded { .. }
    | MemoryCoordinatorError::SoftPressureDeferred { .. } => QueryExecutionSinkErrorClassV1::ResourceLimit,
    _ => QueryExecutionSinkErrorClassV1::Internal,
  };
  sink_error(class, "query_order_top_k_memory", error.to_string())
}

fn map_shrink_error(error: MemoryCoordinatorError) -> QueryExecutionSinkErrorV1 {
  sink_error(QueryExecutionSinkErrorClassV1::Internal, "query_order_top_k_memory", error.to_string())
}

fn sink_error(class: QueryExecutionSinkErrorClassV1, code: &'static str, context: impl Into<String>) -> QueryExecutionSinkErrorV1 {
  QueryExecutionSinkErrorV1::new(class, code, context)
}
