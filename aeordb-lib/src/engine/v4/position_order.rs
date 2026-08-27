use std::cmp::Ordering;
use std::error::Error;
use std::fmt;
use std::mem::size_of;

use super::contract_generated;
use super::position::{
  CanonicalRouteOrderDefinitionV1, CompiledPositionComparatorV1, CompiledRouteOrderV1, LogicalPositionWriteV1, PositionComparatorV1,
  PositionComponentStateV1, PositionComponentWriteV1, PositionRouteV1, PositionSortDefinitionV1, PositionSortDirectionV1,
  compile_route_order_definition, encode_logical_position,
};
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use super::text_fold::AEOR_TEXT_FOLD_TABLE_FINGERPRINT_V1;
use crate::engine::HashAlgorithm;

const DIRECTORY_POLICY: &str = "always";
const DIRECTORY_COLLATION: &str = "aeor-text-fold-unicode-17-then-raw-utf8-v1";
const NULL_POLICY: &str = "present-null-missing-only-present-reverses";
const MULTI_VALUE_POLICY: &str = "minimum-ascending-maximum-descending";
const SCORE_POLICY: &str = "corrected-finite-score-v1";
const NOT_APPLICABLE: &str = "not-applicable";
const ALLOCATION_OVERHEAD: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryOrderFieldV1 {
  Name,
  Size,
  CreatedAt,
  UpdatedAt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PositionOrderFieldV1<'a> {
  pub field: &'a str,
  pub direction: PositionSortDirectionV1,
  pub comparator: PositionComparatorV1,
}

pub type AggregateOrderFieldV1<'a> = PositionOrderFieldV1<'a>;

pub fn compile_directory_listing_order_v1(
  hash_algorithm: HashAlgorithm,
  primary: DirectoryOrderFieldV1,
  direction: PositionSortDirectionV1,
) -> FormatResult<CompiledRouteOrderV1> {
  let (primary_field, primary_comparator) = match primary {
    DirectoryOrderFieldV1::Name => ("name_folded", PositionComparatorV1::Utf8Binary),
    DirectoryOrderFieldV1::Size => ("@size", PositionComparatorV1::U64),
    DirectoryOrderFieldV1::CreatedAt => ("@created_at", PositionComparatorV1::TimestampMs),
    DirectoryOrderFieldV1::UpdatedAt => ("@updated_at", PositionComparatorV1::TimestampMs),
  };
  let mut sort = vec![
    PositionSortDefinitionV1 {
      field: "category",
      direction: PositionSortDirectionV1::Ascending,
      comparator: PositionComparatorV1::U64.name(),
    },
    PositionSortDefinitionV1 { field: primary_field, direction, comparator: primary_comparator.name() },
  ];
  if primary != DirectoryOrderFieldV1::Name {
    sort.push(PositionSortDefinitionV1 {
      field: "name_folded",
      direction: PositionSortDirectionV1::Ascending,
      comparator: PositionComparatorV1::Utf8Binary.name(),
    });
  }
  sort.push(PositionSortDefinitionV1 {
    field: "name_raw",
    direction: PositionSortDirectionV1::Ascending,
    comparator: PositionComparatorV1::Utf8Binary.name(),
  });
  sort.push(PositionSortDefinitionV1 {
    field: "@path",
    direction: PositionSortDirectionV1::Ascending,
    comparator: PositionComparatorV1::Utf8Binary.name(),
  });
  compile_route_order(
    hash_algorithm,
    PositionRouteV1::DirectoryListing,
    &sort,
    DIRECTORY_POLICY,
    DIRECTORY_COLLATION,
    NOT_APPLICABLE,
    NOT_APPLICABLE,
    NOT_APPLICABLE,
  )
}

pub fn compile_query_order_v1(hash_algorithm: HashAlgorithm, fields: &[PositionOrderFieldV1<'_>]) -> FormatResult<CompiledRouteOrderV1> {
  let mut sort = checked_sort_capacity(fields.len(), 1)?;
  for field in fields {
    validate_order_field(field)?;
    sort.push(PositionSortDefinitionV1 { field: field.field, direction: field.direction, comparator: field.comparator.name() });
  }
  sort.push(PositionSortDefinitionV1 {
    field: "@path",
    direction: PositionSortDirectionV1::Ascending,
    comparator: PositionComparatorV1::Utf8Binary.name(),
  });
  compile_route_order(
    hash_algorithm,
    PositionRouteV1::Query,
    &sort,
    NOT_APPLICABLE,
    NOT_APPLICABLE,
    NULL_POLICY,
    MULTI_VALUE_POLICY,
    NOT_APPLICABLE,
  )
}

pub fn compile_global_search_order_v1(hash_algorithm: HashAlgorithm) -> FormatResult<CompiledRouteOrderV1> {
  let sort = [
    PositionSortDefinitionV1 {
      field: "@score",
      direction: PositionSortDirectionV1::Descending,
      comparator: PositionComparatorV1::FiniteF64.name(),
    },
    PositionSortDefinitionV1 {
      field: "@path",
      direction: PositionSortDirectionV1::Ascending,
      comparator: PositionComparatorV1::Utf8Binary.name(),
    },
  ];
  compile_route_order(
    hash_algorithm,
    PositionRouteV1::GlobalSearch,
    &sort,
    NOT_APPLICABLE,
    NOT_APPLICABLE,
    NULL_POLICY,
    MULTI_VALUE_POLICY,
    SCORE_POLICY,
  )
}

pub fn compile_aggregate_group_order_v1(
  hash_algorithm: HashAlgorithm,
  fields: &[AggregateOrderFieldV1<'_>],
) -> FormatResult<CompiledRouteOrderV1> {
  let mut sort = checked_sort_capacity(fields.len().max(1), 1)?;
  if fields.is_empty() {
    sort.push(PositionSortDefinitionV1 {
      field: "@count",
      direction: PositionSortDirectionV1::Descending,
      comparator: PositionComparatorV1::U64.name(),
    });
  } else {
    for field in fields {
      validate_order_field(field)?;
      sort.push(PositionSortDefinitionV1 { field: field.field, direction: field.direction, comparator: field.comparator.name() });
    }
  }
  sort.push(PositionSortDefinitionV1 {
    field: "group_tuple",
    direction: PositionSortDirectionV1::Ascending,
    comparator: PositionComparatorV1::BytesBinary.name(),
  });
  compile_route_order(
    hash_algorithm,
    PositionRouteV1::AggregateGroups,
    &sort,
    NOT_APPLICABLE,
    NOT_APPLICABLE,
    NULL_POLICY,
    MULTI_VALUE_POLICY,
    NOT_APPLICABLE,
  )
}

fn checked_sort_capacity<'a>(fields: usize, ties: usize) -> FormatResult<Vec<PositionSortDefinitionV1<'a>>> {
  let capacity = fields.checked_add(ties).ok_or_else(|| {
    FormatError::new(MalformedInputClass::LengthCountOrArithmeticOverflow, "invalid_position_order", "position sort capacity overflow")
  })?;
  let mut sort = Vec::new();
  sort.try_reserve_exact(capacity).map_err(|source| {
    FormatError::new(
      MalformedInputClass::AllocationAmplification,
      "invalid_position_order",
      format!("cannot reserve {capacity} position sort fields: {source}"),
    )
  })?;
  Ok(sort)
}

fn validate_order_field(field: &PositionOrderFieldV1<'_>) -> FormatResult<()> {
  if field.field.is_empty() {
    return Err(FormatError::new(
      MalformedInputClass::NoncanonicalOrderOrDuplicate,
      "invalid_position_order",
      "position order fields must be nonempty",
    ));
  }
  Ok(())
}

#[allow(clippy::too_many_arguments)]
fn compile_route_order(
  hash_algorithm: HashAlgorithm,
  route: PositionRouteV1,
  sort: &[PositionSortDefinitionV1<'_>],
  directories_first: &str,
  name_collation: &str,
  null_missing_policy: &str,
  multi_value_selector: &str,
  score_semantics: &str,
) -> FormatResult<CompiledRouteOrderV1> {
  let mut semantic_fingerprints = Vec::new();
  semantic_fingerprints.try_reserve_exact(sort.len()).map_err(|source| {
    FormatError::new(
      MalformedInputClass::AllocationAmplification,
      "invalid_position_order",
      format!("cannot reserve {} semantic fingerprints: {source}", sort.len()),
    )
  })?;
  for component in sort {
    let bundle = contract_generated::SEMANTIC_BUNDLES
      .iter()
      .find(|bundle| bundle.name == component.comparator && bundle.corrected)
      .ok_or_else(|| {
        FormatError::new(
          MalformedInputClass::CrossRecordClosureMismatch,
          "invalid_position_order",
          format!("permanent comparator {:?} has no corrected semantic bundle", component.comparator),
        )
      })?;
    let fingerprint = hex::encode(bundle.fingerprint_blake3);
    if !semantic_fingerprints.contains(&fingerprint) {
      semantic_fingerprints.push(fingerprint);
    }
  }
  if route == PositionRouteV1::DirectoryListing {
    let text_fold_fingerprint = hex::encode(AEOR_TEXT_FOLD_TABLE_FINGERPRINT_V1);
    if !semantic_fingerprints.contains(&text_fold_fingerprint) {
      semantic_fingerprints.push(text_fold_fingerprint);
    }
  }
  compile_route_order_definition(
    hash_algorithm,
    &CanonicalRouteOrderDefinitionV1 {
      route,
      sort,
      directories_first,
      multi_value_selector,
      name_collation,
      null_missing_policy,
      score_semantics,
      semantic_fingerprints,
    },
  )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogicalOrderComponentOwnedV1 {
  pub comparator: Option<PositionComparatorV1>,
  pub state: PositionComponentStateV1,
  pub payload: Vec<u8>,
}

impl LogicalOrderComponentOwnedV1 {
  pub fn present(comparator: PositionComparatorV1, payload: Vec<u8>) -> Self {
    Self { comparator: Some(comparator), state: PositionComponentStateV1::Present, payload }
  }

  pub const fn typed_null() -> Self {
    Self { comparator: None, state: PositionComponentStateV1::TypedNull, payload: Vec::new() }
  }

  pub const fn missing() -> Self {
    Self { comparator: None, state: PositionComponentStateV1::Missing, payload: Vec::new() }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogicalOrderRowOwnedV1 {
  pub route: PositionRouteV1,
  pub components: Vec<LogicalOrderComponentOwnedV1>,
  pub file_key_tie: Vec<u8>,
  pub record_revision_tie: Vec<u8>,
}

impl LogicalOrderRowOwnedV1 {
  pub fn as_borrowed(&self) -> LogicalOrderRowV1<'_> {
    LogicalOrderRowV1 {
      route: self.route,
      components: &self.components,
      file_key_tie: &self.file_key_tie,
      record_revision_tie: &self.record_revision_tie,
    }
  }
}

pub fn encode_logical_order_row_position_v1(
  order: &CompiledRouteOrderV1,
  namespace_root: &[u8],
  row: LogicalOrderRowV1<'_>,
) -> FormatResult<Vec<u8>> {
  validate_logical_order_row_v1(order, row)
    .map_err(|error| FormatError::new(MalformedInputClass::CrossRecordClosureMismatch, "invalid_position_cursor", error.to_string()))?;
  let components = row
    .components
    .iter()
    .map(|component| PositionComponentWriteV1 { comparator: component.comparator, state: component.state, payload: &component.payload })
    .collect::<Vec<_>>();
  encode_logical_position(&LogicalPositionWriteV1 {
    order,
    namespace_root,
    file_key_tie: row.file_key_tie,
    record_revision_tie: row.record_revision_tie,
    components: &components,
  })
}

#[derive(Clone, Copy, Debug)]
pub struct LogicalOrderRowV1<'a> {
  pub route: PositionRouteV1,
  pub components: &'a [LogicalOrderComponentOwnedV1],
  pub file_key_tie: &'a [u8],
  pub record_revision_tie: &'a [u8],
}

pub fn compare_logical_order_rows_v1(
  order: &CompiledRouteOrderV1,
  left: LogicalOrderRowV1<'_>,
  right: LogicalOrderRowV1<'_>,
) -> PositionOrderResultV1<Ordering> {
  validate_logical_order_row_v1(order, left)?;
  validate_logical_order_row_v1(order, right)?;
  for (index, definition) in order.sort().iter().enumerate() {
    let comparison = compare_components(definition.comparator, &left.components[index], &right.components[index])?;
    let comparison = if comparison != Ordering::Equal
      && left.components[index].state == PositionComponentStateV1::Present
      && right.components[index].state == PositionComponentStateV1::Present
      && definition.direction == PositionSortDirectionV1::Descending
    {
      comparison.reverse()
    } else {
      comparison
    };
    if comparison != Ordering::Equal {
      return Ok(comparison);
    }
  }
  Ok(left.file_key_tie.cmp(right.file_key_tie).then_with(|| left.record_revision_tie.cmp(right.record_revision_tie)))
}

pub fn validate_logical_order_row_v1(order: &CompiledRouteOrderV1, row: LogicalOrderRowV1<'_>) -> PositionOrderResultV1<()> {
  validate_route_order_contract(order)?;
  if row.route != order.route() || row.components.len() != order.component_count() {
    return Err(PositionOrderErrorV1::corrupt(
      "position_row_order_mismatch",
      format!(
        "row route/count {:?}/{} differs from order {:?}/{}",
        row.route,
        row.components.len(),
        order.route(),
        order.component_count()
      ),
    ));
  }
  let hash_width = order.hash_algorithm().hash_length();
  for (name, identity) in [("FileKey", row.file_key_tie), ("RecordRevision", row.record_revision_tie)] {
    if identity.len() != hash_width || identity.iter().all(|byte| *byte == 0) {
      return Err(PositionOrderErrorV1::corrupt(
        "position_row_identity",
        format!("row {name} tie must be a nonzero {hash_width}-byte identity"),
      ));
    }
  }
  for (definition, component) in order.sort().iter().zip(row.components) {
    validate_component(definition.comparator, component, &definition.field)?;
  }
  validate_route_row_contract(order, row)?;
  Ok(())
}

fn validate_route_order_contract(order: &CompiledRouteOrderV1) -> PositionOrderResultV1<()> {
  let sort = order.sort();
  let policies = order.policies();
  let valid = match order.route() {
    PositionRouteV1::DirectoryListing => {
      let name_order = sort.len() == 4
        && sort[0].field == "category"
        && sort[0].direction == PositionSortDirectionV1::Ascending
        && sort[0].comparator == CompiledPositionComparatorV1::Payload(PositionComparatorV1::U64)
        && sort[1].field == "name_folded"
        && sort[1].comparator == CompiledPositionComparatorV1::Payload(PositionComparatorV1::Utf8Binary)
        && sort[2].field == "name_raw"
        && sort[2].direction == PositionSortDirectionV1::Ascending
        && sort[2].comparator == CompiledPositionComparatorV1::Payload(PositionComparatorV1::Utf8Binary)
        && is_path_tie(&sort[3]);
      let metadata_order = sort.len() == 5
        && sort[0].field == "category"
        && sort[0].direction == PositionSortDirectionV1::Ascending
        && sort[0].comparator == CompiledPositionComparatorV1::Payload(PositionComparatorV1::U64)
        && matches!(
          (sort[1].field.as_str(), sort[1].comparator),
          ("@size", CompiledPositionComparatorV1::Payload(PositionComparatorV1::U64))
            | ("@created_at" | "@updated_at", CompiledPositionComparatorV1::Payload(PositionComparatorV1::TimestampMs))
        )
        && sort[2].field == "name_folded"
        && sort[2].direction == PositionSortDirectionV1::Ascending
        && sort[2].comparator == CompiledPositionComparatorV1::Payload(PositionComparatorV1::Utf8Binary)
        && sort[3].field == "name_raw"
        && sort[3].direction == PositionSortDirectionV1::Ascending
        && sort[3].comparator == CompiledPositionComparatorV1::Payload(PositionComparatorV1::Utf8Binary)
        && is_path_tie(&sort[4]);
      (name_order || metadata_order)
        && policies.directories_first == DIRECTORY_POLICY
        && policies.name_collation == DIRECTORY_COLLATION
        && policies.null_missing_policy == NOT_APPLICABLE
        && policies.multi_value_selector == NOT_APPLICABLE
        && policies.score_semantics == NOT_APPLICABLE
    }
    PositionRouteV1::Query => {
      sort.last().is_some_and(is_path_tie)
        && policies.directories_first == NOT_APPLICABLE
        && policies.name_collation == NOT_APPLICABLE
        && policies.null_missing_policy == NULL_POLICY
        && policies.multi_value_selector == MULTI_VALUE_POLICY
        && policies.score_semantics == NOT_APPLICABLE
    }
    PositionRouteV1::GlobalSearch => {
      sort.len() == 2
        && sort[0].field == "@score"
        && sort[0].direction == PositionSortDirectionV1::Descending
        && sort[0].comparator == CompiledPositionComparatorV1::Payload(PositionComparatorV1::FiniteF64)
        && is_path_tie(&sort[1])
        && policies.directories_first == NOT_APPLICABLE
        && policies.name_collation == NOT_APPLICABLE
        && policies.null_missing_policy == NULL_POLICY
        && policies.multi_value_selector == MULTI_VALUE_POLICY
        && policies.score_semantics == SCORE_POLICY
    }
    PositionRouteV1::AggregateGroups => {
      sort.len() >= 2
        && sort.last().is_some_and(|component| {
          component.field == "group_tuple"
            && component.direction == PositionSortDirectionV1::Ascending
            && component.comparator == CompiledPositionComparatorV1::Payload(PositionComparatorV1::BytesBinary)
        })
        && policies.directories_first == NOT_APPLICABLE
        && policies.name_collation == NOT_APPLICABLE
        && policies.null_missing_policy == NULL_POLICY
        && policies.multi_value_selector == MULTI_VALUE_POLICY
        && policies.score_semantics == NOT_APPLICABLE
    }
  };
  if !valid {
    return Err(PositionOrderErrorV1::corrupt(
      "position_route_order_contract",
      format!("compiled {:?} order differs from the permanent route contract", order.route()),
    ));
  }
  Ok(())
}

fn is_path_tie(component: &super::position::CompiledPositionSortV1) -> bool {
  component.field == "@path"
    && component.direction == PositionSortDirectionV1::Ascending
    && component.comparator == CompiledPositionComparatorV1::Payload(PositionComparatorV1::Utf8Binary)
}

fn validate_route_row_contract(order: &CompiledRouteOrderV1, row: LogicalOrderRowV1<'_>) -> PositionOrderResultV1<()> {
  let present = |component: &LogicalOrderComponentOwnedV1| component.state == PositionComponentStateV1::Present;
  let valid = match order.route() {
    PositionRouteV1::DirectoryListing => {
      let category = decode_u64_payload(&row.components[0].payload, "category")?;
      row.components.iter().all(present) && category <= 1
    }
    PositionRouteV1::Query => row.components.last().is_some_and(present),
    PositionRouteV1::GlobalSearch => row.components.iter().all(present),
    PositionRouteV1::AggregateGroups => row.components.last().is_some_and(present),
  };
  if !valid {
    return Err(PositionOrderErrorV1::corrupt(
      "position_route_row_contract",
      format!("row violates the permanent {:?} presence/category contract", order.route()),
    ));
  }
  Ok(())
}

fn validate_component(
  expected: CompiledPositionComparatorV1,
  component: &LogicalOrderComponentOwnedV1,
  field: &str,
) -> PositionOrderResultV1<()> {
  match component.state {
    PositionComponentStateV1::Present => {
      let CompiledPositionComparatorV1::Payload(expected) = expected else {
        return Err(PositionOrderErrorV1::corrupt(
          "position_row_fixture_comparator",
          format!("field {field:?} uses a framing-only null/missing comparator"),
        ));
      };
      if component.comparator != Some(expected) {
        return Err(PositionOrderErrorV1::corrupt(
          "position_row_comparator",
          format!("field {field:?} comparator {:?} differs from {expected:?}", component.comparator),
        ));
      }
      validate_present_payload(expected, &component.payload, field)?;
    }
    PositionComponentStateV1::TypedNull | PositionComponentStateV1::Missing => {
      if component.comparator.is_some() || !component.payload.is_empty() {
        return Err(PositionOrderErrorV1::corrupt(
          "position_row_presence",
          format!("field {field:?} null/missing state has a comparator or payload"),
        ));
      }
      if !matches!(expected, CompiledPositionComparatorV1::Payload(_)) {
        return Err(PositionOrderErrorV1::corrupt(
          "position_row_fixture_comparator",
          format!("field {field:?} uses a framing-only null/missing comparator"),
        ));
      }
    }
  }
  Ok(())
}

pub fn validate_logical_order_component_v1(
  comparator: PositionComparatorV1,
  component: &LogicalOrderComponentOwnedV1,
  field: &str,
) -> PositionOrderResultV1<()> {
  validate_component(CompiledPositionComparatorV1::Payload(comparator), component, field)
}

pub fn compare_logical_order_components_v1(
  comparator: PositionComparatorV1,
  left: &LogicalOrderComponentOwnedV1,
  right: &LogicalOrderComponentOwnedV1,
  field: &str,
) -> PositionOrderResultV1<Ordering> {
  validate_logical_order_component_v1(comparator, left, field)?;
  validate_logical_order_component_v1(comparator, right, field)?;
  compare_components(CompiledPositionComparatorV1::Payload(comparator), left, right)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogicalNumericValueV1 {
  U64(u64),
  I64(i64),
  FiniteF64Bits(u64),
}

pub fn decode_logical_numeric_component_v1(
  comparator: PositionComparatorV1,
  component: &LogicalOrderComponentOwnedV1,
  field: &str,
) -> PositionOrderResultV1<LogicalNumericValueV1> {
  validate_logical_order_component_v1(comparator, component, field)?;
  if component.state != PositionComponentStateV1::Present {
    return Err(PositionOrderErrorV1::corrupt(
      "position_numeric_presence",
      format!("field {field:?} numeric decoding requires a present value"),
    ));
  }
  match comparator {
    PositionComparatorV1::U64 => Ok(LogicalNumericValueV1::U64(decode_u64_payload(&component.payload, field)?)),
    PositionComparatorV1::I64 => Ok(LogicalNumericValueV1::I64(decode_i64_payload(&component.payload, field)?)),
    PositionComparatorV1::FiniteF64 => {
      let value = decode_f64_payload(&component.payload, field)?;
      Ok(LogicalNumericValueV1::FiniteF64Bits(value.to_bits()))
    }
    _ => {
      Err(PositionOrderErrorV1::corrupt("position_numeric_comparator", format!("field {field:?} comparator {comparator:?} is not numeric")))
    }
  }
}

fn validate_present_payload(comparator: PositionComparatorV1, payload: &[u8], field: &str) -> PositionOrderResultV1<()> {
  match comparator {
    PositionComparatorV1::BytesBinary => {}
    PositionComparatorV1::Utf8Binary => {
      if let Err(source) = std::str::from_utf8(payload) {
        return Err(PositionOrderErrorV1::corrupt("position_row_payload", format!("field {field:?} has invalid UTF-8: {source}")));
      }
    }
    PositionComparatorV1::U64 => {
      decode_u64_payload(payload, field)?;
    }
    PositionComparatorV1::I64 | PositionComparatorV1::TimestampMs => {
      decode_i64_payload(payload, field)?;
    }
    PositionComparatorV1::FiniteF64 => {
      let value = decode_f64_payload(payload, field)?;
      if !value.is_finite() || (value == 0.0 && value.to_bits() != 0) {
        return Err(PositionOrderErrorV1::corrupt(
          "position_row_payload",
          format!("field {field:?} has a noncanonical finite-f64 payload"),
        ));
      }
    }
    PositionComparatorV1::Boolean if payload == [0] || payload == [1] => {}
    PositionComparatorV1::Boolean => {
      return Err(PositionOrderErrorV1::corrupt("position_row_payload", format!("field {field:?} has a noncanonical Boolean payload")));
    }
  }
  Ok(())
}

fn fixed_width_payload(payload: &[u8], field: &str, comparator: &str) -> PositionOrderResultV1<[u8; 8]> {
  let bytes = match payload.try_into() {
    Ok(bytes) => bytes,
    Err(source) => {
      return Err(PositionOrderErrorV1::corrupt(
        "position_row_payload",
        format!("field {field:?} has invalid {comparator} width: {source}"),
      ));
    }
  };
  Ok(bytes)
}

fn decode_u64_payload(payload: &[u8], field: &str) -> PositionOrderResultV1<u64> {
  Ok(u64::from_le_bytes(fixed_width_payload(payload, field, "u64")?))
}

fn decode_i64_payload(payload: &[u8], field: &str) -> PositionOrderResultV1<i64> {
  Ok(i64::from_le_bytes(fixed_width_payload(payload, field, "i64")?))
}

fn decode_f64_payload(payload: &[u8], field: &str) -> PositionOrderResultV1<f64> {
  Ok(f64::from_le_bytes(fixed_width_payload(payload, field, "f64")?))
}

fn compare_components(
  expected: CompiledPositionComparatorV1,
  left: &LogicalOrderComponentOwnedV1,
  right: &LogicalOrderComponentOwnedV1,
) -> PositionOrderResultV1<Ordering> {
  let left_rank = presence_rank(left.state);
  let right_rank = presence_rank(right.state);
  if left_rank != right_rank {
    return Ok(left_rank.cmp(&right_rank));
  }
  if left.state != PositionComponentStateV1::Present {
    return Ok(Ordering::Equal);
  }
  let CompiledPositionComparatorV1::Payload(comparator) = expected else {
    return Err(PositionOrderErrorV1::corrupt(
      "position_row_fixture_comparator",
      "framing-only null/missing comparator cannot order logical rows",
    ));
  };
  let comparison = match comparator {
    PositionComparatorV1::BytesBinary | PositionComparatorV1::Utf8Binary => left.payload.cmp(&right.payload),
    PositionComparatorV1::U64 => {
      decode_u64_payload(&left.payload, "left order component")?.cmp(&decode_u64_payload(&right.payload, "right order component")?)
    }
    PositionComparatorV1::I64 | PositionComparatorV1::TimestampMs => {
      decode_i64_payload(&left.payload, "left order component")?.cmp(&decode_i64_payload(&right.payload, "right order component")?)
    }
    PositionComparatorV1::FiniteF64 => {
      let left = decode_f64_payload(&left.payload, "left order component")?;
      let right = decode_f64_payload(&right.payload, "right order component")?;
      left.total_cmp(&right)
    }
    PositionComparatorV1::Boolean => left.payload[0].cmp(&right.payload[0]),
  };
  Ok(comparison)
}

const fn presence_rank(state: PositionComponentStateV1) -> u8 {
  match state {
    PositionComponentStateV1::Present => 0,
    PositionComponentStateV1::TypedNull => 1,
    PositionComponentStateV1::Missing => 2,
  }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PositionPaginationInputsV1 {
  pub page: Option<u64>,
  pub offset: Option<u64>,
  pub after: bool,
  pub before: bool,
  pub limit: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PositionWindowLimitsV1 {
  default_limit: u64,
  maximum_limit: u64,
  maximum_buffer_bytes: u64,
}

impl PositionWindowLimitsV1 {
  pub fn new(default_limit: u64, maximum_limit: u64, maximum_buffer_bytes: u64) -> PositionOrderResultV1<Self> {
    if default_limit == 0 || maximum_limit < default_limit || maximum_buffer_bytes == 0 || maximum_limit == u64::MAX {
      return Err(PositionOrderErrorV1::invalid(
        "invalid_pagination_limits",
        "pagination limits require 1 <= default <= maximum < u64::MAX and a nonzero buffer",
      ));
    }
    Ok(Self { default_limit, maximum_limit, maximum_buffer_bytes })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionWindowOriginV1 {
  Start,
  AbsoluteRank(u64),
  After,
  Before,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PositionWindowPlanV1 {
  pub origin: PositionWindowOriginV1,
  pub limit: u64,
  maximum_buffer_bytes: u64,
}

pub fn plan_position_window_v1(
  inputs: PositionPaginationInputsV1,
  limits: PositionWindowLimitsV1,
) -> PositionOrderResultV1<PositionWindowPlanV1> {
  let origins =
    usize::from(inputs.page.is_some()) + usize::from(inputs.offset.is_some()) + usize::from(inputs.after) + usize::from(inputs.before);
  if origins > 1 {
    return Err(PositionOrderErrorV1::invalid(
      "invalid_pagination",
      "page, offset, after, and before are mutually exclusive window origins",
    ));
  }
  let limit = match inputs.limit {
    Some(limit) => limit,
    None => limits.default_limit,
  };
  if limit == 0 || limit > limits.maximum_limit {
    return Err(PositionOrderErrorV1::invalid(
      "invalid_pagination",
      format!("pagination limit {limit} is outside 1..={}", limits.maximum_limit),
    ));
  }
  let origin = if let Some(page) = inputs.page {
    if page == 0 {
      return Err(PositionOrderErrorV1::invalid("invalid_pagination", "page is one-based and cannot be zero"));
    }
    let rank = (page - 1)
      .checked_mul(limit)
      .ok_or_else(|| PositionOrderErrorV1::invalid("invalid_pagination", "page-to-rank arithmetic overflow"))?;
    PositionWindowOriginV1::AbsoluteRank(rank)
  } else if let Some(offset) = inputs.offset {
    PositionWindowOriginV1::AbsoluteRank(offset)
  } else if inputs.after {
    PositionWindowOriginV1::After
  } else if inputs.before {
    PositionWindowOriginV1::Before
  } else {
    PositionWindowOriginV1::Start
  };
  Ok(PositionWindowPlanV1 { origin, limit, maximum_buffer_bytes: limits.maximum_buffer_bytes })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionWindowDirectionV1 {
  Forward,
  Reverse,
}

#[derive(Clone, Copy, Debug)]
pub enum PositionWindowSeekV1<'a> {
  First,
  AbsoluteRank(u64),
  After(LogicalOrderRowV1<'a>),
  Before(LogicalOrderRowV1<'a>),
}

#[derive(Clone, Copy)]
pub struct PositionWindowScanRequestV1<'a> {
  pub order: &'a CompiledRouteOrderV1,
  pub seek: PositionWindowSeekV1<'a>,
  pub direction: PositionWindowDirectionV1,
  pub maximum_rows: u64,
  pub is_cancelled: &'a dyn Fn() -> bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PositionWindowScanReceiptV1 {
  pub emitted_rows: u64,
  pub exhausted: bool,
}

pub trait PositionWindowVisitorV1 {
  fn visit(&mut self, row: LogicalOrderRowV1<'_>) -> PositionOrderResultV1<()>;
}

pub trait PositionWindowSourceV1 {
  /// Seek directly to the requested absolute rank or exclusive logical bound
  /// and stream no more than `maximum_rows`. Implementations may use rank
  /// metadata or bounded index/authoritative scans, but must never satisfy a
  /// deep rank by materializing the skipped prefix.
  fn scan(
    &mut self,
    request: PositionWindowScanRequestV1<'_>,
    visitor: &mut dyn PositionWindowVisitorV1,
  ) -> PositionOrderResultV1<PositionWindowScanReceiptV1>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PositionWindowPageV1 {
  pub rows: Vec<LogicalOrderRowOwnedV1>,
  pub has_more: bool,
  pub source_emitted_rows: u64,
}

pub fn execute_position_window_v1(
  order: &CompiledRouteOrderV1,
  plan: PositionWindowPlanV1,
  bound: Option<LogicalOrderRowV1<'_>>,
  source: &mut dyn PositionWindowSourceV1,
  is_cancelled: &dyn Fn() -> bool,
) -> PositionOrderResultV1<PositionWindowPageV1> {
  check_cancelled(is_cancelled)?;
  let (seek, direction) = match (plan.origin, bound) {
    (PositionWindowOriginV1::Start, None) => (PositionWindowSeekV1::First, PositionWindowDirectionV1::Forward),
    (PositionWindowOriginV1::AbsoluteRank(rank), None) => (PositionWindowSeekV1::AbsoluteRank(rank), PositionWindowDirectionV1::Forward),
    (PositionWindowOriginV1::After, Some(bound)) => {
      validate_logical_order_row_v1(order, bound)?;
      (PositionWindowSeekV1::After(bound), PositionWindowDirectionV1::Forward)
    }
    (PositionWindowOriginV1::Before, Some(bound)) => {
      validate_logical_order_row_v1(order, bound)?;
      (PositionWindowSeekV1::Before(bound), PositionWindowDirectionV1::Reverse)
    }
    (PositionWindowOriginV1::After | PositionWindowOriginV1::Before, None) => {
      return Err(PositionOrderErrorV1::invalid("invalid_pagination", "after/before requires one resolved logical bound"));
    }
    (PositionWindowOriginV1::Start | PositionWindowOriginV1::AbsoluteRank(_), Some(_)) => {
      return Err(PositionOrderErrorV1::invalid("invalid_pagination", "start/page/offset cannot carry a logical bound"));
    }
  };
  let maximum_rows =
    plan.limit.checked_add(1).ok_or_else(|| PositionOrderErrorV1::invalid("invalid_pagination", "limit lookahead overflow"))?;
  let mut collector = PositionWindowCollectorV1 {
    order,
    direction,
    bound,
    maximum_rows,
    maximum_buffer_bytes: plan.maximum_buffer_bytes,
    retained_bytes: 0,
    rows: Vec::new(),
    is_cancelled,
  };
  let receipt = source.scan(PositionWindowScanRequestV1 { order, seek, direction, maximum_rows, is_cancelled }, &mut collector)?;
  check_cancelled(is_cancelled)?;
  let observed =
    u64::try_from(collector.rows.len()).map_err(|source| PositionOrderErrorV1::resource("position_window_count", source.to_string()))?;
  if receipt.emitted_rows != observed || observed > maximum_rows {
    return Err(PositionOrderErrorV1::corrupt(
      "position_window_receipt",
      format!("source receipt/emitted rows {}/{} disagree or exceed {maximum_rows}", receipt.emitted_rows, observed),
    ));
  }
  if observed < maximum_rows && !receipt.exhausted {
    return Err(PositionOrderErrorV1::corrupt(
      "position_window_incomplete",
      "source stopped before its row bound without reaching the result boundary",
    ));
  }
  let has_more = observed > plan.limit;
  if has_more {
    collector.rows.pop();
  }
  if direction == PositionWindowDirectionV1::Reverse {
    collector.rows.reverse();
  }
  Ok(PositionWindowPageV1 { rows: collector.rows, has_more, source_emitted_rows: observed })
}

struct PositionWindowCollectorV1<'a> {
  order: &'a CompiledRouteOrderV1,
  direction: PositionWindowDirectionV1,
  bound: Option<LogicalOrderRowV1<'a>>,
  maximum_rows: u64,
  maximum_buffer_bytes: u64,
  retained_bytes: u64,
  rows: Vec<LogicalOrderRowOwnedV1>,
  is_cancelled: &'a dyn Fn() -> bool,
}

impl PositionWindowVisitorV1 for PositionWindowCollectorV1<'_> {
  fn visit(&mut self, row: LogicalOrderRowV1<'_>) -> PositionOrderResultV1<()> {
    check_cancelled(self.is_cancelled)?;
    let observed =
      u64::try_from(self.rows.len()).map_err(|source| PositionOrderErrorV1::resource("position_window_count", source.to_string()))?;
    if observed >= self.maximum_rows {
      return Err(PositionOrderErrorV1::corrupt(
        "position_window_extra_row",
        format!("source emitted more than {} admitted rows", self.maximum_rows),
      ));
    }
    validate_logical_order_row_v1(self.order, row)?;
    if let Some(bound) = self.bound {
      let comparison = compare_logical_order_rows_v1(self.order, row, bound)?;
      let valid = match self.direction {
        PositionWindowDirectionV1::Forward => comparison == Ordering::Greater,
        PositionWindowDirectionV1::Reverse => comparison == Ordering::Less,
      };
      if !valid {
        return Err(PositionOrderErrorV1::corrupt(
          "position_window_bound",
          "source emitted a row on the wrong side of its exclusive logical bound",
        ));
      }
    }
    if let Some(previous) = self.rows.last() {
      let comparison = compare_logical_order_rows_v1(self.order, previous.as_borrowed(), row)?;
      let valid = match self.direction {
        PositionWindowDirectionV1::Forward => comparison == Ordering::Less,
        PositionWindowDirectionV1::Reverse => comparison == Ordering::Greater,
      };
      if !valid {
        return Err(PositionOrderErrorV1::corrupt(
          "position_window_order",
          "source rows are duplicate or not strictly ordered in scan direction",
        ));
      }
    }
    let row_bytes = logical_order_row_retained_bytes_v1(row)?;
    let next = self
      .retained_bytes
      .checked_add(row_bytes)
      .ok_or_else(|| PositionOrderErrorV1::resource("position_window_buffer", "position-window retained-byte counter overflow"))?;
    if next > self.maximum_buffer_bytes {
      return Err(PositionOrderErrorV1::resource(
        "position_window_buffer",
        format!("position window requires {next} bytes, configured bound is {}", self.maximum_buffer_bytes),
      ));
    }
    self.rows.try_reserve_exact(1).map_err(|source| PositionOrderErrorV1::resource("position_window_rows", source.to_string()))?;
    self.rows.push(clone_row(row)?);
    self.retained_bytes = next;
    Ok(())
  }
}

pub fn logical_order_row_retained_bytes_v1(row: LogicalOrderRowV1<'_>) -> PositionOrderResultV1<u64> {
  let mut total = size_of::<LogicalOrderRowOwnedV1>()
    .checked_add(ALLOCATION_OVERHEAD * 3)
    .and_then(|value| value.checked_add(row.file_key_tie.len()))
    .and_then(|value| value.checked_add(row.record_revision_tie.len()))
    .and_then(|value| value.checked_add(row.components.len().checked_mul(size_of::<LogicalOrderComponentOwnedV1>())?))
    .ok_or_else(|| PositionOrderErrorV1::resource("position_window_buffer", "position row retained-byte preflight overflow"))?;
  for component in row.components {
    total = total
      .checked_add(ALLOCATION_OVERHEAD)
      .and_then(|value| value.checked_add(component.payload.len()))
      .ok_or_else(|| PositionOrderErrorV1::resource("position_window_buffer", "position component retained-byte preflight overflow"))?;
  }
  u64::try_from(total).map_err(|source| PositionOrderErrorV1::resource("position_window_buffer", source.to_string()))
}

pub fn logical_order_row_allocated_bytes_v1(row: &LogicalOrderRowOwnedV1) -> PositionOrderResultV1<u64> {
  let mut total = size_of::<LogicalOrderRowOwnedV1>()
    .checked_add(ALLOCATION_OVERHEAD * 3)
    .and_then(|value| value.checked_add(row.file_key_tie.capacity()))
    .and_then(|value| value.checked_add(row.record_revision_tie.capacity()))
    .and_then(|value| value.checked_add(row.components.capacity().checked_mul(size_of::<LogicalOrderComponentOwnedV1>())?))
    .ok_or_else(|| PositionOrderErrorV1::resource("position_window_buffer", "owned position-row allocation accounting overflow"))?;
  for component in &row.components {
    total = total
      .checked_add(ALLOCATION_OVERHEAD)
      .and_then(|value| value.checked_add(component.payload.capacity()))
      .ok_or_else(|| PositionOrderErrorV1::resource("position_window_buffer", "owned position-component allocation accounting overflow"))?;
  }
  u64::try_from(total).map_err(|source| PositionOrderErrorV1::resource("position_window_buffer", source.to_string()))
}

fn clone_row(row: LogicalOrderRowV1<'_>) -> PositionOrderResultV1<LogicalOrderRowOwnedV1> {
  let mut components = Vec::new();
  components
    .try_reserve_exact(row.components.len())
    .map_err(|source| PositionOrderErrorV1::resource("position_window_components", source.to_string()))?;
  for component in row.components {
    let mut payload = Vec::new();
    payload
      .try_reserve_exact(component.payload.len())
      .map_err(|source| PositionOrderErrorV1::resource("position_window_payload", source.to_string()))?;
    payload.extend_from_slice(&component.payload);
    components.push(LogicalOrderComponentOwnedV1 { comparator: component.comparator, state: component.state, payload });
  }
  Ok(LogicalOrderRowOwnedV1 {
    route: row.route,
    components,
    file_key_tie: copy_bytes(row.file_key_tie, "position_window_file_key")?,
    record_revision_tie: copy_bytes(row.record_revision_tie, "position_window_revision")?,
  })
}

fn copy_bytes(bytes: &[u8], code: &'static str) -> PositionOrderResultV1<Vec<u8>> {
  let mut output = Vec::new();
  output.try_reserve_exact(bytes.len()).map_err(|source| PositionOrderErrorV1::resource(code, source.to_string()))?;
  output.extend_from_slice(bytes);
  Ok(output)
}

fn check_cancelled(is_cancelled: &dyn Fn() -> bool) -> PositionOrderResultV1<()> {
  if is_cancelled() {
    return Err(PositionOrderErrorV1::cancelled("position_window_cancelled", "position window was cancelled"));
  }
  Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionOrderErrorClassV1 {
  InvalidRequest,
  ResourceLimit,
  Unavailable,
  Corrupt,
  Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PositionOrderErrorV1 {
  class: PositionOrderErrorClassV1,
  code: &'static str,
  context: String,
}

impl PositionOrderErrorV1 {
  pub fn invalid(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: PositionOrderErrorClassV1::InvalidRequest, code, context: context.into() }
  }

  pub fn resource(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: PositionOrderErrorClassV1::ResourceLimit, code, context: context.into() }
  }

  pub fn unavailable(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: PositionOrderErrorClassV1::Unavailable, code, context: context.into() }
  }

  pub fn corrupt(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: PositionOrderErrorClassV1::Corrupt, code, context: context.into() }
  }

  pub fn cancelled(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: PositionOrderErrorClassV1::Cancelled, code, context: context.into() }
  }

  pub const fn class(&self) -> PositionOrderErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

impl fmt::Display for PositionOrderErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.context)
  }
}

impl Error for PositionOrderErrorV1 {}

pub type PositionOrderResultV1<T> = Result<T, PositionOrderErrorV1>;
