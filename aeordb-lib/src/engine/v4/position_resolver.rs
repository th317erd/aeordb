use std::error::Error;
use std::fmt;

use tokio_util::sync::CancellationToken;

use super::hash::digest_parts;
use super::position::{
  CompiledRouteOrderV1, LogicalPositionV1, PositionComparatorV1, PositionComponentStateV1, PositionComponentV1, PositionRouteV1,
  decode_logical_position,
};
use super::position_order::{LogicalOrderRowOwnedV1, logical_order_row_retained_bytes_v1, validate_logical_order_row_v1};
use super::read_view::ResolvedReadViewV1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PositionResolutionLimitsV1 {
  maximum_row_bytes: u64,
}

impl PositionResolutionLimitsV1 {
  pub fn new(maximum_row_bytes: u64) -> PositionResolutionResultV1<Self> {
    if maximum_row_bytes == 0 {
      return Err(PositionResolutionErrorV1::resource(
        "position_resolution_limit",
        "maximum recomputed position-row bytes must be nonzero",
      ));
    }
    Ok(Self { maximum_row_bytes })
  }

  pub const fn maximum_row_bytes(self) -> u64 {
    self.maximum_row_bytes
  }
}

#[derive(Clone, Copy, Debug)]
pub struct PositionUniverseLookupRequestV1<'a> {
  database_id: [u8; 16],
  physical_instance_id: [u8; 16],
  selected_root: &'a [u8],
  order: &'a CompiledRouteOrderV1,
  file_key_tie: &'a [u8],
  record_revision_tie: &'a [u8],
  maximum_row_bytes: u64,
}

impl<'a> PositionUniverseLookupRequestV1<'a> {
  pub const fn database_id(self) -> [u8; 16] {
    self.database_id
  }

  pub const fn physical_instance_id(self) -> [u8; 16] {
    self.physical_instance_id
  }

  pub const fn selected_root(self) -> &'a [u8] {
    self.selected_root
  }

  pub const fn route(self) -> PositionRouteV1 {
    self.order.route()
  }

  pub const fn order(self) -> &'a CompiledRouteOrderV1 {
    self.order
  }

  pub fn order_fingerprint(self) -> &'a [u8] {
    self.order.fingerprint()
  }

  pub const fn file_key_tie(self) -> &'a [u8] {
    self.file_key_tie
  }

  pub const fn record_revision_tie(self) -> &'a [u8] {
    self.record_revision_tie
  }

  pub const fn maximum_row_bytes(self) -> u64 {
    self.maximum_row_bytes
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PositionUniverseLookupResultV1 {
  Found(LogicalOrderRowOwnedV1),
  Absent,
}

pub trait PositionUniverseSourceV1 {
  /// Resolve the immutable identity inside the exact selected-root result
  /// universe and recompute every logical-order component. Implementations may
  /// not use token tuple bytes as row or membership authority.
  fn resolve_position(
    &mut self,
    request: PositionUniverseLookupRequestV1<'_>,
    cancellation: &CancellationToken,
  ) -> Result<PositionUniverseLookupResultV1, PositionUniverseSourceErrorV1>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PositionUniverseSourceErrorV1 {
  Unavailable(String),
  ResourceLimit(String),
  Corrupt(String),
  Cancelled,
}

impl PositionUniverseSourceErrorV1 {
  pub fn unavailable(context: impl Into<String>) -> Self {
    Self::Unavailable(context.into())
  }

  pub fn resource(context: impl Into<String>) -> Self {
    Self::ResourceLimit(context.into())
  }

  pub fn corrupt(context: impl Into<String>) -> Self {
    Self::Corrupt(context.into())
  }

  pub const fn cancelled() -> Self {
    Self::Cancelled
  }
}

impl fmt::Display for PositionUniverseSourceErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Unavailable(context) => write!(formatter, "position universe is unavailable: {context}"),
      Self::ResourceLimit(context) => write!(formatter, "position universe resource limit: {context}"),
      Self::Corrupt(context) => write!(formatter, "position universe is corrupt: {context}"),
      Self::Cancelled => formatter.write_str("position universe lookup was cancelled"),
    }
  }
}

impl Error for PositionUniverseSourceErrorV1 {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPositionBoundV1 {
  row: LogicalOrderRowOwnedV1,
}

impl ResolvedPositionBoundV1 {
  pub const fn row(&self) -> &LogicalOrderRowOwnedV1 {
    &self.row
  }

  pub fn into_row(self) -> LogicalOrderRowOwnedV1 {
    self.row
  }
}

pub fn resolve_position_bound_v1<A>(
  token: &[u8],
  order: &CompiledRouteOrderV1,
  view: &ResolvedReadViewV1<A>,
  limits: PositionResolutionLimitsV1,
  source: &mut dyn PositionUniverseSourceV1,
) -> PositionResolutionResultV1<ResolvedPositionBoundV1> {
  check_cancelled(view.cancellation())?;
  if !view.is_explicit_root() {
    return Err(PositionResolutionErrorV1::invalid(
      "invalid_position_cursor",
      "after/before position resolution requires an explicit selected root",
    ));
  }
  if order.hash_algorithm() != view.hash_algorithm() {
    return Err(PositionResolutionErrorV1::corrupt("database_corruption", "compiled route order uses another database hash algorithm"));
  }

  let position = match decode_logical_position(token, view.hash_algorithm()) {
    Ok(position) => position,
    Err(source) => {
      return Err(PositionResolutionErrorV1::invalid("invalid_position_cursor", format!("logical position is malformed: {source}")));
    }
  };
  if position.route != order.route() {
    return Err(PositionResolutionErrorV1::invalid(
      "invalid_position_cursor",
      "logical position route does not match the requested result universe",
    ));
  }
  if position.namespace_root() != view.root_metadata().hash.as_slice() {
    return Err(PositionResolutionErrorV1::root_mismatch(
      "position_root_mismatch",
      "logical position root does not match the authorized selected root",
    ));
  }
  if position.order_fingerprint() != order.fingerprint() {
    return Err(PositionResolutionErrorV1::order_mismatch(
      "position_order_mismatch",
      "logical position order does not match the requested route order",
    ));
  }
  if usize::from(position.component_count) != order.component_count() {
    return Err(PositionResolutionErrorV1::invalid(
      "invalid_position_cursor",
      "logical position component count differs from its compiled route order",
    ));
  }
  validate_token_identity(&position, view.root_metadata().hash.as_slice(), order)?;
  check_cancelled(view.cancellation())?;

  let request = PositionUniverseLookupRequestV1 {
    database_id: view.database_id(),
    physical_instance_id: view.physical_instance_id(),
    selected_root: &view.root_metadata().hash,
    order,
    file_key_tie: position.file_key_tie(),
    record_revision_tie: position.record_revision_tie(),
    maximum_row_bytes: limits.maximum_row_bytes,
  };
  let lookup = match source.resolve_position(request, view.cancellation()) {
    Ok(result) => result,
    Err(PositionUniverseSourceErrorV1::Unavailable(context)) => {
      return Err(PositionResolutionErrorV1::unavailable("historical_view_unavailable", context));
    }
    Err(PositionUniverseSourceErrorV1::ResourceLimit(context)) => {
      return Err(PositionResolutionErrorV1::resource("position_resolution_resource", context));
    }
    Err(PositionUniverseSourceErrorV1::Corrupt(context)) => {
      return Err(PositionResolutionErrorV1::corrupt("database_corruption", context));
    }
    Err(PositionUniverseSourceErrorV1::Cancelled) => {
      return Err(PositionResolutionErrorV1::cancelled("position_resolution_cancelled", "position-universe lookup was cancelled"));
    }
  };
  check_cancelled(view.cancellation())?;
  let row = match lookup {
    PositionUniverseLookupResultV1::Found(row) => row,
    PositionUniverseLookupResultV1::Absent => {
      return Err(PositionResolutionErrorV1::invalid(
        "invalid_position_cursor",
        "logical position identity is absent from the selected result universe",
      ));
    }
  };

  if let Err(source) = validate_logical_order_row_v1(order, row.as_borrowed()) {
    return Err(PositionResolutionErrorV1::corrupt("database_corruption", format!("position universe returned a malformed row: {source}")));
  }
  let retained_bytes = match logical_order_row_retained_bytes_v1(row.as_borrowed()) {
    Ok(bytes) => bytes,
    Err(source) => {
      return Err(PositionResolutionErrorV1::resource(
        "position_resolution_resource",
        format!("cannot account recomputed position row: {source}"),
      ));
    }
  };
  if retained_bytes > limits.maximum_row_bytes {
    return Err(PositionResolutionErrorV1::resource(
      "position_resolution_resource",
      format!("recomputed position row requires {retained_bytes} bytes, cap is {}", limits.maximum_row_bytes),
    ));
  }
  if row.file_key_tie.as_slice() != position.file_key_tie() || row.record_revision_tie.as_slice() != position.record_revision_tie() {
    return Err(PositionResolutionErrorV1::corrupt(
      "database_corruption",
      "position universe returned an identity other than the requested immutable ties",
    ));
  }
  validate_recomputed_identity(&row, view.root_metadata().hash.as_slice(), order)?;
  validate_recomputed_components(&position, &row)?;
  check_cancelled(view.cancellation())?;
  Ok(ResolvedPositionBoundV1 { row })
}

fn validate_token_identity(
  position: &LogicalPositionV1,
  selected_root: &[u8],
  order: &CompiledRouteOrderV1,
) -> PositionResolutionResultV1<()> {
  let final_component = final_position_component(position)?;
  let valid = match position.route {
    PositionRouteV1::AggregateGroups => {
      final_component.comparator == Some(PositionComparatorV1::BytesBinary)
        && final_component.state == PositionComponentStateV1::Present
        && digest_parts(order.hash_algorithm(), &[final_component.payload]) == position.file_key_tie()
        && position.record_revision_tie() == selected_root
    }
    PositionRouteV1::DirectoryListing | PositionRouteV1::Query | PositionRouteV1::GlobalSearch => {
      final_component.comparator == Some(PositionComparatorV1::Utf8Binary)
        && final_component.state == PositionComponentStateV1::Present
        && digest_parts(order.hash_algorithm(), &[b"file:", final_component.payload]) == position.file_key_tie()
    }
  };
  if !valid {
    return Err(PositionResolutionErrorV1::invalid(
      "invalid_position_cursor",
      "logical position has invalid canonical path/group identity ties",
    ));
  }
  Ok(())
}

fn final_position_component(position: &LogicalPositionV1) -> PositionResolutionResultV1<PositionComponentV1<'_>> {
  let mut final_component = None;
  for component in position.components() {
    let component = match component {
      Ok(component) => component,
      Err(source) => {
        return Err(PositionResolutionErrorV1::invalid(
          "invalid_position_cursor",
          format!("logical position component is malformed: {source}"),
        ));
      }
    };
    final_component = Some(component);
  }
  match final_component {
    Some(component) => Ok(component),
    None => Err(PositionResolutionErrorV1::invalid("invalid_position_cursor", "logical position has no canonical path/group component")),
  }
}

fn validate_recomputed_identity(
  row: &LogicalOrderRowOwnedV1,
  selected_root: &[u8],
  order: &CompiledRouteOrderV1,
) -> PositionResolutionResultV1<()> {
  let Some(final_component) = row.components.last() else {
    return Err(PositionResolutionErrorV1::corrupt("database_corruption", "recomputed position row has no canonical path/group component"));
  };
  let valid = match row.route {
    PositionRouteV1::AggregateGroups => {
      digest_parts(order.hash_algorithm(), &[&final_component.payload]) == row.file_key_tie && row.record_revision_tie == selected_root
    }
    PositionRouteV1::DirectoryListing | PositionRouteV1::Query | PositionRouteV1::GlobalSearch => {
      digest_parts(order.hash_algorithm(), &[b"file:", &final_component.payload]) == row.file_key_tie
    }
  };
  if !valid {
    return Err(PositionResolutionErrorV1::corrupt(
      "database_corruption",
      "position universe returned invalid canonical path/group identity ties",
    ));
  }
  Ok(())
}

fn validate_recomputed_components(position: &LogicalPositionV1, row: &LogicalOrderRowOwnedV1) -> PositionResolutionResultV1<()> {
  let mut decoded = position.components();
  for (index, recomputed) in row.components.iter().enumerate() {
    let component = match decoded.next() {
      Some(Ok(component)) => component,
      Some(Err(source)) => {
        return Err(PositionResolutionErrorV1::invalid(
          "invalid_position_cursor",
          format!("logical position component {index} is malformed: {source}"),
        ));
      }
      None => {
        return Err(PositionResolutionErrorV1::invalid(
          "invalid_position_cursor",
          "logical position has fewer components than its recomputed row",
        ));
      }
    };
    if component.comparator != recomputed.comparator || component.state != recomputed.state || component.payload != recomputed.payload {
      return Err(PositionResolutionErrorV1::invalid(
        "invalid_position_cursor",
        format!("logical position component {index} differs from the recomputed selected-root row"),
      ));
    }
  }
  if decoded.next().is_some() {
    return Err(PositionResolutionErrorV1::invalid(
      "invalid_position_cursor",
      "logical position has more components than its recomputed row",
    ));
  }
  Ok(())
}

fn check_cancelled(cancellation: &CancellationToken) -> PositionResolutionResultV1<()> {
  if cancellation.is_cancelled() {
    return Err(PositionResolutionErrorV1::cancelled("position_resolution_cancelled", "position resolution was cancelled"));
  }
  Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionResolutionErrorClassV1 {
  InvalidPosition,
  RootMismatch,
  OrderMismatch,
  Unavailable,
  ResourceLimit,
  Corrupt,
  Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PositionResolutionErrorV1 {
  class: PositionResolutionErrorClassV1,
  code: &'static str,
  context: String,
}

impl PositionResolutionErrorV1 {
  fn new(class: PositionResolutionErrorClassV1, code: &'static str, context: impl Into<String>) -> Self {
    Self { class, code, context: context.into() }
  }

  pub fn invalid(code: &'static str, context: impl Into<String>) -> Self {
    Self::new(PositionResolutionErrorClassV1::InvalidPosition, code, context)
  }

  pub fn root_mismatch(code: &'static str, context: impl Into<String>) -> Self {
    Self::new(PositionResolutionErrorClassV1::RootMismatch, code, context)
  }

  pub fn order_mismatch(code: &'static str, context: impl Into<String>) -> Self {
    Self::new(PositionResolutionErrorClassV1::OrderMismatch, code, context)
  }

  pub fn unavailable(code: &'static str, context: impl Into<String>) -> Self {
    Self::new(PositionResolutionErrorClassV1::Unavailable, code, context)
  }

  pub fn resource(code: &'static str, context: impl Into<String>) -> Self {
    Self::new(PositionResolutionErrorClassV1::ResourceLimit, code, context)
  }

  pub fn corrupt(code: &'static str, context: impl Into<String>) -> Self {
    Self::new(PositionResolutionErrorClassV1::Corrupt, code, context)
  }

  pub fn cancelled(code: &'static str, context: impl Into<String>) -> Self {
    Self::new(PositionResolutionErrorClassV1::Cancelled, code, context)
  }

  pub const fn class(&self) -> PositionResolutionErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

impl fmt::Display for PositionResolutionErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.context)
  }
}

impl Error for PositionResolutionErrorV1 {}

pub type PositionResolutionResultV1<T> = Result<T, PositionResolutionErrorV1>;
