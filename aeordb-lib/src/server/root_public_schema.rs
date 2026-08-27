//! Inactive shared public request and response schemas for the coordinated P7
//! namespace-read cutover.
//!
//! This module deliberately owns no handler or storage behavior. It bounds and
//! validates public JSON before the root-aware planner and provides route-
//! neutral wire types that HTTP, embedded, plugin, SSE, and client adapters can
//! share when the coordinated activation lands.

use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::engine::HashAlgorithm;
use crate::engine::path_utils::normalize_path;
use crate::engine::v4::config_value::CanonicalConfigValueV1;
use crate::engine::v4::position::{LogicalPositionV1, PositionRouteV1, decode_logical_position};
use crate::engine::v4::position_order::{PositionPaginationInputsV1, PositionWindowLimitsV1, PositionWindowPlanV1, plan_position_window_v1};
use crate::engine::v4::query_planner::{
  QUERY_MAXIMUM_AGGREGATE_FIELDS_V1, QUERY_MAXIMUM_EXPRESSION_DEPTH_V1, QUERY_MAXIMUM_EXPRESSION_NODES_V1,
  QUERY_MAXIMUM_FIELD_NAME_BYTES_V1, QUERY_MAXIMUM_FUZZY_EDITS_V1, QUERY_MAXIMUM_GROUP_FIELDS_V1, QUERY_MAXIMUM_IN_LITERALS_V1,
  QUERY_MAXIMUM_LITERAL_BYTES_V1, QUERY_MAXIMUM_PATH_BYTES_V1, QUERY_MAXIMUM_RETURNED_DOCUMENTS_V1, QUERY_MAXIMUM_SORT_FIELDS_V1,
  QUERY_MAXIMUM_TOTAL_LITERAL_BYTES_V1, QueryExpressionV1, QueryFuzzyAlgorithmV1, QueryPredicateOperationV1, QueryPredicateV1,
};

use super::root_api::{RequestedRootSelectorV1, RootApiErrorV1, RootResponseV1, RootSelectorFieldsV1, parse_root_selector_v1};

pub const PUBLIC_QUERY_MAXIMUM_REQUEST_BYTES_V1: usize = 16 * 1_048_576;
pub const PUBLIC_SEARCH_MAXIMUM_REQUEST_BYTES_V1: usize = 16 * 1_048_576;

const PUBLIC_QUERY_DEFAULT_LIMIT_V1: u64 = 100;
const PUBLIC_QUERY_MAXIMUM_WINDOW_BYTES_V1: u64 = 16 * 1_048_576;
const PUBLIC_QUERY_MAXIMUM_SELECTION_FIELDS_V1: usize = 256;
const PUBLIC_QUERY_MAXIMUM_LOCATOR_MATCHES_PER_RESULT_V1: usize = 1_024;
const PUBLIC_QUERY_MAXIMUM_LOCATOR_SCAN_BYTES_V1: u64 = 256 * 1_048_576;
const PUBLIC_QUERY_MAXIMUM_SNIPPET_CHARACTERS_V1: usize = 4_096;
const PUBLIC_QUERY_MAXIMUM_MATCH_CONTEXT_LINES_V1: u64 = 1_024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicSchemaErrorV1 {
  code: &'static str,
  context: String,
}

impl PublicSchemaErrorV1 {
  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }

  fn new(code: &'static str, context: impl Into<String>) -> Self {
    Self { code, context: context.into() }
  }
}

impl fmt::Display for PublicSchemaErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.context)
  }
}

impl Error for PublicSchemaErrorV1 {}

#[derive(Clone, Copy, Debug)]
pub struct PublicPositionContextV1<'a> {
  pub route: PositionRouteV1,
  pub order_fingerprint: &'a [u8],
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PublicSortDirectionV1 {
  Asc,
  Desc,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublicSortFieldV1 {
  pub field: String,
  #[serde(default = "default_sort_direction")]
  pub direction: PublicSortDirectionV1,
}

fn default_sort_direction() -> PublicSortDirectionV1 {
  PublicSortDirectionV1::Asc
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublicAggregateRequestV1 {
  #[serde(default)]
  pub count: bool,
  #[serde(default)]
  pub sum: Vec<String>,
  #[serde(default)]
  pub avg: Vec<String>,
  #[serde(default)]
  pub min: Vec<String>,
  #[serde(default)]
  pub max: Vec<String>,
  #[serde(default)]
  pub group_by: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PublicExplainModeV1 {
  #[default]
  Off,
  Plan,
  Analyze,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicLocatorRequestV1 {
  pub include_matches: bool,
  pub maximum_matches_per_result: usize,
  pub maximum_scan_bytes: u64,
  pub snippet_characters: usize,
  pub match_context_lines: u64,
}

#[derive(Debug)]
pub struct PublicQueryRequestV1 {
  pub path: String,
  pub selector: RequestedRootSelectorV1,
  pub expression: QueryExpressionV1,
  pub pagination: PositionWindowPlanV1,
  pub default_limit_applied: bool,
  pub position: Option<LogicalPositionV1>,
  pub order_by: Vec<PublicSortFieldV1>,
  pub include_total: bool,
  pub aggregate: Option<PublicAggregateRequestV1>,
  pub select: Vec<String>,
  pub explain: PublicExplainModeV1,
  pub locators: PublicLocatorRequestV1,
}

#[derive(Debug)]
pub struct AdmittedPublicQueryRequestV1 {
  pub path: String,
  pub selector: RequestedRootSelectorV1,
  pub expression: QueryExpressionV1,
  pub pagination: PositionWindowPlanV1,
  pub default_limit_applied: bool,
  pub order_by: Vec<PublicSortFieldV1>,
  pub include_total: bool,
  pub aggregate: Option<PublicAggregateRequestV1>,
  pub select: Vec<String>,
  pub explain: PublicExplainModeV1,
  pub locators: PublicLocatorRequestV1,
  position_token: Option<String>,
}

#[derive(Debug)]
pub struct PublicSearchRequestV1 {
  pub query: Option<String>,
  pub expression: Option<QueryExpressionV1>,
  pub path: String,
  pub selector: RequestedRootSelectorV1,
  pub pagination: PositionWindowPlanV1,
  pub position: Option<LogicalPositionV1>,
  pub locators: PublicLocatorRequestV1,
}

#[derive(Debug)]
pub struct AdmittedPublicSearchRequestV1 {
  pub query: Option<String>,
  pub expression: Option<QueryExpressionV1>,
  pub path: String,
  pub selector: RequestedRootSelectorV1,
  pub pagination: PositionWindowPlanV1,
  pub locators: PublicLocatorRequestV1,
  position_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPublicQueryRequestV1 {
  path: String,
  #[serde(rename = "where")]
  expression: serde_json::Value,
  root_hash: Option<String>,
  snapshot: Option<String>,
  version: Option<String>,
  page: Option<u64>,
  offset: Option<u64>,
  after: Option<String>,
  before: Option<String>,
  limit: Option<u64>,
  #[serde(default)]
  order_by: Vec<PublicSortFieldV1>,
  #[serde(default)]
  include_total: bool,
  aggregate: Option<PublicAggregateRequestV1>,
  #[serde(default)]
  select: Vec<String>,
  explain: Option<serde_json::Value>,
  #[serde(default)]
  include_matches: bool,
  max_matches_per_result: Option<usize>,
  max_locator_scan_bytes: Option<u64>,
  snippet_chars: Option<usize>,
  match_context_lines: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPublicSearchRequestV1 {
  query: Option<String>,
  #[serde(rename = "where")]
  expression: Option<serde_json::Value>,
  path: Option<String>,
  root_hash: Option<String>,
  snapshot: Option<String>,
  version: Option<String>,
  page: Option<u64>,
  offset: Option<u64>,
  after: Option<String>,
  before: Option<String>,
  limit: Option<u64>,
  #[serde(default)]
  include_matches: bool,
  max_matches_per_result: Option<usize>,
  max_locator_scan_bytes: Option<u64>,
  snippet_chars: Option<usize>,
  match_context_lines: Option<u64>,
}

pub fn admit_public_query_request_v1(
  bytes: &[u8],
  hash_algorithm: HashAlgorithm,
) -> Result<AdmittedPublicQueryRequestV1, PublicSchemaErrorV1> {
  if bytes.len() > PUBLIC_QUERY_MAXIMUM_REQUEST_BYTES_V1 {
    return Err(PublicSchemaErrorV1::new(
      "QUERY_REQUEST_TOO_LARGE",
      format!("query request is {} bytes; maximum is {PUBLIC_QUERY_MAXIMUM_REQUEST_BYTES_V1}", bytes.len()),
    ));
  }
  let raw = serde_json::from_slice::<RawPublicQueryRequestV1>(bytes)
    .map_err(|source| PublicSchemaErrorV1::new("INVALID_QUERY_REQUEST", source.to_string()))?;
  let path = validate_query_path(&raw.path)?;
  let selector = parse_root_selector_v1(
    &RootSelectorFieldsV1 { root_hash: raw.root_hash, snapshot: raw.snapshot, version: raw.version },
    hash_algorithm,
  )
  .map_err(public_root_error)?;

  let default_limit_applied = raw.limit.is_none();
  let admitted_pagination = admit_public_pagination_v1(
    RawPaginationV1 { page: raw.page, offset: raw.offset, after: raw.after, before: raw.before, limit: raw.limit },
    &selector,
  )?;
  let expression = parse_public_query_expression_v1(&raw.expression)?;
  validate_string_fields(&raw.order_by.iter().map(|field| field.field.as_str()).collect::<Vec<_>>(), QUERY_MAXIMUM_SORT_FIELDS_V1)?;
  validate_aggregate(raw.aggregate.as_ref())?;
  validate_string_fields(&raw.select.iter().map(String::as_str).collect::<Vec<_>>(), PUBLIC_QUERY_MAXIMUM_SELECTION_FIELDS_V1)?;
  let explain = parse_explain_mode(raw.explain.as_ref())?;
  let locators = validate_locator_request(
    raw.include_matches,
    raw.max_matches_per_result,
    raw.max_locator_scan_bytes,
    raw.snippet_chars,
    raw.match_context_lines,
  )?;

  Ok(AdmittedPublicQueryRequestV1 {
    path,
    selector,
    expression,
    pagination: admitted_pagination.pagination,
    default_limit_applied,
    order_by: raw.order_by,
    include_total: raw.include_total,
    aggregate: raw.aggregate,
    select: raw.select,
    explain,
    locators,
    position_token: admitted_pagination.position_token,
  })
}

pub fn finalize_public_query_request_v1(
  admitted: AdmittedPublicQueryRequestV1,
  hash_algorithm: HashAlgorithm,
  position_context: PublicPositionContextV1<'_>,
) -> Result<PublicQueryRequestV1, PublicSchemaErrorV1> {
  let expected_route = if admitted.aggregate.is_some() { PositionRouteV1::AggregateGroups } else { PositionRouteV1::Query };
  if position_context.route != expected_route {
    return Err(PublicSchemaErrorV1::new("INVALID_POSITION_CURSOR", "position route does not match request route"));
  }
  let position = finalize_public_position_v1(admitted.position_token, &admitted.selector, hash_algorithm, position_context)?;
  Ok(PublicQueryRequestV1 {
    path: admitted.path,
    selector: admitted.selector,
    expression: admitted.expression,
    pagination: admitted.pagination,
    default_limit_applied: admitted.default_limit_applied,
    position,
    order_by: admitted.order_by,
    include_total: admitted.include_total,
    aggregate: admitted.aggregate,
    select: admitted.select,
    explain: admitted.explain,
    locators: admitted.locators,
  })
}

pub fn parse_public_query_request_v1(
  bytes: &[u8],
  hash_algorithm: HashAlgorithm,
  position_context: PublicPositionContextV1<'_>,
) -> Result<PublicQueryRequestV1, PublicSchemaErrorV1> {
  let admitted = admit_public_query_request_v1(bytes, hash_algorithm)?;
  finalize_public_query_request_v1(admitted, hash_algorithm, position_context)
}

pub fn admit_public_search_request_v1(
  bytes: &[u8],
  hash_algorithm: HashAlgorithm,
) -> Result<AdmittedPublicSearchRequestV1, PublicSchemaErrorV1> {
  if bytes.len() > PUBLIC_SEARCH_MAXIMUM_REQUEST_BYTES_V1 {
    return Err(PublicSchemaErrorV1::new(
      "SEARCH_REQUEST_TOO_LARGE",
      format!("search request is {} bytes; maximum is {PUBLIC_SEARCH_MAXIMUM_REQUEST_BYTES_V1}", bytes.len()),
    ));
  }
  let raw = serde_json::from_slice::<RawPublicSearchRequestV1>(bytes)
    .map_err(|source| PublicSchemaErrorV1::new("INVALID_SEARCH_REQUEST", source.to_string()))?;
  if raw.query.is_none() && raw.expression.is_none() {
    return Err(PublicSchemaErrorV1::new("INVALID_SEARCH_REQUEST", "search requires query, where, or both"));
  }
  if raw.query.as_ref().is_some_and(|query| query.is_empty() || query.len() > QUERY_MAXIMUM_LITERAL_BYTES_V1) {
    return Err(PublicSchemaErrorV1::new("INVALID_SEARCH_REQUEST", "search query must be nonempty and bounded"));
  }
  let mut requested_path = "/";
  if let Some(path) = raw.path.as_deref() {
    requested_path = path;
  }
  let path =
    validate_query_path(requested_path).map_err(|error| PublicSchemaErrorV1::new("INVALID_SEARCH_REQUEST", error.context().to_string()))?;
  let selector = parse_root_selector_v1(
    &RootSelectorFieldsV1 { root_hash: raw.root_hash, snapshot: raw.snapshot, version: raw.version },
    hash_algorithm,
  )
  .map_err(public_root_error)?;
  let admitted_pagination = admit_public_pagination_v1(
    RawPaginationV1 { page: raw.page, offset: raw.offset, after: raw.after, before: raw.before, limit: raw.limit },
    &selector,
  )?;
  let expression = raw.expression.as_ref().map(parse_public_query_expression_v1).transpose()?;
  let locators = validate_locator_request(
    raw.include_matches,
    raw.max_matches_per_result,
    raw.max_locator_scan_bytes,
    raw.snippet_chars,
    raw.match_context_lines,
  )?;
  Ok(AdmittedPublicSearchRequestV1 {
    query: raw.query,
    expression,
    path,
    selector,
    pagination: admitted_pagination.pagination,
    locators,
    position_token: admitted_pagination.position_token,
  })
}

pub fn finalize_public_search_request_v1(
  admitted: AdmittedPublicSearchRequestV1,
  hash_algorithm: HashAlgorithm,
  position_context: PublicPositionContextV1<'_>,
) -> Result<PublicSearchRequestV1, PublicSchemaErrorV1> {
  if position_context.route != PositionRouteV1::GlobalSearch {
    return Err(PublicSchemaErrorV1::new("INVALID_POSITION_CURSOR", "position route does not match request route"));
  }
  let position = finalize_public_position_v1(admitted.position_token, &admitted.selector, hash_algorithm, position_context)?;
  Ok(PublicSearchRequestV1 {
    query: admitted.query,
    expression: admitted.expression,
    path: admitted.path,
    selector: admitted.selector,
    pagination: admitted.pagination,
    position,
    locators: admitted.locators,
  })
}

pub fn parse_public_search_request_v1(
  bytes: &[u8],
  hash_algorithm: HashAlgorithm,
  position_context: PublicPositionContextV1<'_>,
) -> Result<PublicSearchRequestV1, PublicSchemaErrorV1> {
  let admitted = admit_public_search_request_v1(bytes, hash_algorithm)?;
  finalize_public_search_request_v1(admitted, hash_algorithm, position_context)
}

#[derive(Debug)]
struct RawPaginationV1 {
  page: Option<u64>,
  offset: Option<u64>,
  after: Option<String>,
  before: Option<String>,
  limit: Option<u64>,
}

#[derive(Debug)]
struct AdmittedPublicPaginationV1 {
  pagination: PositionWindowPlanV1,
  position_token: Option<String>,
}

fn admit_public_pagination_v1(
  raw: RawPaginationV1,
  selector: &RequestedRootSelectorV1,
) -> Result<AdmittedPublicPaginationV1, PublicSchemaErrorV1> {
  let has_after = raw.after.is_some();
  let has_before = raw.before.is_some();
  let limits = PositionWindowLimitsV1::new(
    PUBLIC_QUERY_DEFAULT_LIMIT_V1,
    QUERY_MAXIMUM_RETURNED_DOCUMENTS_V1 as u64,
    PUBLIC_QUERY_MAXIMUM_WINDOW_BYTES_V1,
  )
  .map_err(|error| PublicSchemaErrorV1::new("INVALID_PAGINATION", error.to_string()))?;
  let pagination = plan_position_window_v1(
    PositionPaginationInputsV1 { page: raw.page, offset: raw.offset, after: has_after, before: has_before, limit: raw.limit },
    limits,
  )
  .map_err(|error| PublicSchemaErrorV1::new("INVALID_PAGINATION", error.to_string()))?;

  let token = match (raw.after, raw.before) {
    (Some(token), None) | (None, Some(token)) => token,
    (None, None) => return Ok(AdmittedPublicPaginationV1 { pagination, position_token: None }),
    (Some(_), Some(_)) => {
      return Err(PublicSchemaErrorV1::new("INVALID_PAGINATION", "after and before are mutually exclusive"));
    }
  };
  let RequestedRootSelectorV1::ExplicitRoot(selected_root) = selector else {
    return Err(PublicSchemaErrorV1::new("INVALID_PAGINATION", "after and before require an explicit root_hash selector"));
  };
  if selected_root.is_empty() {
    return Err(PublicSchemaErrorV1::new("INVALID_PAGINATION", "after and before require a nonempty explicit root_hash selector"));
  }
  Ok(AdmittedPublicPaginationV1 { pagination, position_token: Some(token) })
}

fn finalize_public_position_v1(
  position_token: Option<String>,
  selector: &RequestedRootSelectorV1,
  hash_algorithm: HashAlgorithm,
  position_context: PublicPositionContextV1<'_>,
) -> Result<Option<LogicalPositionV1>, PublicSchemaErrorV1> {
  let Some(token) = position_token else {
    return Ok(None);
  };
  let RequestedRootSelectorV1::ExplicitRoot(selected_root) = selector else {
    return Err(PublicSchemaErrorV1::new("INVALID_PAGINATION", "after and before require an explicit root_hash selector"));
  };
  let position = decode_logical_position(token.as_bytes(), hash_algorithm)
    .map_err(|error| PublicSchemaErrorV1::new("INVALID_POSITION_CURSOR", error.to_string()))?;
  if position.route != position_context.route {
    return Err(PublicSchemaErrorV1::new("INVALID_POSITION_CURSOR", "position route does not match request route"));
  }
  if position.namespace_root() != selected_root {
    return Err(PublicSchemaErrorV1::new("POSITION_ROOT_MISMATCH", "position root does not match selected root"));
  }
  if position.order_fingerprint() != position_context.order_fingerprint {
    return Err(PublicSchemaErrorV1::new("POSITION_ORDER_MISMATCH", "position order does not match request order"));
  }
  Ok(Some(position))
}

fn validate_query_path(path: &str) -> Result<String, PublicSchemaErrorV1> {
  if path.is_empty() || path.len() > QUERY_MAXIMUM_PATH_BYTES_V1 || path.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
    return Err(PublicSchemaErrorV1::new("INVALID_QUERY_REQUEST", "query path is empty, oversized, or contains controls"));
  }
  Ok(normalize_path(path))
}

fn validate_string_fields(fields: &[&str], maximum_fields: usize) -> Result<(), PublicSchemaErrorV1> {
  if fields.len() > maximum_fields {
    return Err(PublicSchemaErrorV1::new(
      "QUERY_EXPRESSION_LIMIT_EXCEEDED",
      format!("field list contains {} entries; maximum is {maximum_fields}", fields.len()),
    ));
  }
  if fields.iter().any(|field| field.is_empty() || field.len() > QUERY_MAXIMUM_FIELD_NAME_BYTES_V1) {
    return Err(PublicSchemaErrorV1::new("INVALID_QUERY_REQUEST", "query field names must be nonempty and bounded"));
  }
  Ok(())
}

fn validate_aggregate(aggregate: Option<&PublicAggregateRequestV1>) -> Result<(), PublicSchemaErrorV1> {
  let Some(aggregate) = aggregate else {
    return Ok(());
  };
  let aggregate_fields = aggregate
    .sum
    .len()
    .checked_add(aggregate.avg.len())
    .and_then(|count| count.checked_add(aggregate.min.len()))
    .and_then(|count| count.checked_add(aggregate.max.len()))
    .ok_or_else(|| PublicSchemaErrorV1::new("QUERY_EXPRESSION_LIMIT_EXCEEDED", "aggregate field count overflow"))?;
  if aggregate_fields > QUERY_MAXIMUM_AGGREGATE_FIELDS_V1 || aggregate.group_by.len() > QUERY_MAXIMUM_GROUP_FIELDS_V1 {
    return Err(PublicSchemaErrorV1::new("QUERY_EXPRESSION_LIMIT_EXCEEDED", "aggregate or group field count exceeds the protocol maximum"));
  }
  let fields = aggregate
    .sum
    .iter()
    .chain(&aggregate.avg)
    .chain(&aggregate.min)
    .chain(&aggregate.max)
    .chain(&aggregate.group_by)
    .map(String::as_str)
    .collect::<Vec<_>>();
  validate_string_fields(&fields, QUERY_MAXIMUM_AGGREGATE_FIELDS_V1 + QUERY_MAXIMUM_GROUP_FIELDS_V1)
}

fn parse_explain_mode(value: Option<&serde_json::Value>) -> Result<PublicExplainModeV1, PublicSchemaErrorV1> {
  match value {
    None | Some(serde_json::Value::Bool(false)) => Ok(PublicExplainModeV1::Off),
    Some(serde_json::Value::Bool(true)) => Ok(PublicExplainModeV1::Plan),
    Some(serde_json::Value::String(value)) if value == "plan" => Ok(PublicExplainModeV1::Plan),
    Some(serde_json::Value::String(value)) if value == "analyze" => Ok(PublicExplainModeV1::Analyze),
    Some(_) => Err(PublicSchemaErrorV1::new("INVALID_QUERY_REQUEST", "explain must be false, true, plan, or analyze")),
  }
}

fn validate_locator_request(
  include_matches: bool,
  requested_maximum_matches_per_result: Option<usize>,
  requested_maximum_scan_bytes: Option<u64>,
  requested_snippet_characters: Option<usize>,
  requested_match_context_lines: Option<u64>,
) -> Result<PublicLocatorRequestV1, PublicSchemaErrorV1> {
  let mut maximum_matches_per_result = 5;
  if let Some(value) = requested_maximum_matches_per_result {
    maximum_matches_per_result = value;
  }
  let mut maximum_scan_bytes = PUBLIC_QUERY_MAXIMUM_LOCATOR_SCAN_BYTES_V1;
  if let Some(value) = requested_maximum_scan_bytes {
    maximum_scan_bytes = value;
  }
  let mut snippet_characters = 160;
  if let Some(value) = requested_snippet_characters {
    snippet_characters = value;
  }
  let mut match_context_lines = 2;
  if let Some(value) = requested_match_context_lines {
    match_context_lines = value;
  }
  if maximum_matches_per_result == 0
    || maximum_matches_per_result > PUBLIC_QUERY_MAXIMUM_LOCATOR_MATCHES_PER_RESULT_V1
    || maximum_scan_bytes == 0
    || maximum_scan_bytes > PUBLIC_QUERY_MAXIMUM_LOCATOR_SCAN_BYTES_V1
    || snippet_characters == 0
    || snippet_characters > PUBLIC_QUERY_MAXIMUM_SNIPPET_CHARACTERS_V1
    || match_context_lines > PUBLIC_QUERY_MAXIMUM_MATCH_CONTEXT_LINES_V1
  {
    return Err(PublicSchemaErrorV1::new("INVALID_QUERY_REQUEST", "locator limits must be nonzero and remain within protocol maxima"));
  }
  Ok(PublicLocatorRequestV1 { include_matches, maximum_matches_per_result, maximum_scan_bytes, snippet_characters, match_context_lines })
}

#[derive(Default)]
struct QueryExpressionAdmissionV1 {
  nodes: usize,
  total_literal_bytes: usize,
}

fn parse_public_query_expression_v1(value: &serde_json::Value) -> Result<QueryExpressionV1, PublicSchemaErrorV1> {
  let mut admission = QueryExpressionAdmissionV1::default();
  parse_public_query_expression_inner_v1(value, 0, &mut admission)
}

fn parse_public_query_expression_inner_v1(
  value: &serde_json::Value,
  depth: usize,
  admission: &mut QueryExpressionAdmissionV1,
) -> Result<QueryExpressionV1, PublicSchemaErrorV1> {
  if depth > QUERY_MAXIMUM_EXPRESSION_DEPTH_V1 {
    return Err(expression_limit("query expression exceeds the maximum depth"));
  }
  admission.nodes = admission.nodes.checked_add(1).ok_or_else(|| expression_limit("query expression node count overflow"))?;
  if admission.nodes > QUERY_MAXIMUM_EXPRESSION_NODES_V1 {
    return Err(expression_limit("query expression exceeds the maximum node count"));
  }

  if let Some(array) = value.as_array() {
    return parse_boolean_children_v1(array, depth, admission).map(QueryExpressionV1::And);
  }
  let object = value
    .as_object()
    .ok_or_else(|| PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", "query expression must be an object or legacy array"))?;
  let boolean_keys =
    usize::from(object.contains_key("and")) + usize::from(object.contains_key("or")) + usize::from(object.contains_key("not"));
  if boolean_keys > 1 || (boolean_keys != 0 && object.contains_key("field")) {
    return Err(PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", "query expression cannot combine boolean and field forms"));
  }
  if let Some(children) = object.get("and") {
    ensure_only_keys(object, &["and"])?;
    let children = children.as_array().ok_or_else(|| PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", "and must be an array"))?;
    return parse_boolean_children_v1(children, depth, admission).map(QueryExpressionV1::And);
  }
  if let Some(children) = object.get("or") {
    ensure_only_keys(object, &["or"])?;
    let children = children.as_array().ok_or_else(|| PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", "or must be an array"))?;
    return parse_boolean_children_v1(children, depth, admission).map(QueryExpressionV1::Or);
  }
  if let Some(child) = object.get("not") {
    ensure_only_keys(object, &["not"])?;
    return parse_public_query_expression_inner_v1(child, depth + 1, admission).map(Box::new).map(QueryExpressionV1::Not);
  }
  parse_public_query_predicate_v1(object, admission).map(QueryExpressionV1::Field)
}

fn parse_boolean_children_v1(
  children: &[serde_json::Value],
  depth: usize,
  admission: &mut QueryExpressionAdmissionV1,
) -> Result<Vec<QueryExpressionV1>, PublicSchemaErrorV1> {
  let mut parsed = Vec::new();
  parsed
    .try_reserve_exact(children.len())
    .map_err(|source| expression_limit(format!("cannot reserve query expression children: {source}")))?;
  for child in children {
    parsed.push(parse_public_query_expression_inner_v1(child, depth + 1, admission)?);
  }
  Ok(parsed)
}

fn parse_public_query_predicate_v1(
  object: &serde_json::Map<String, serde_json::Value>,
  admission: &mut QueryExpressionAdmissionV1,
) -> Result<QueryPredicateV1, PublicSchemaErrorV1> {
  ensure_only_keys(object, &["field", "op", "value", "value2", "threshold", "algorithm", "fuzziness"])?;
  let field_name = object
    .get("field")
    .and_then(serde_json::Value::as_str)
    .ok_or_else(|| PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", "query predicate requires a string field"))?;
  if field_name.is_empty() || field_name.len() > QUERY_MAXIMUM_FIELD_NAME_BYTES_V1 {
    return Err(PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", "query predicate field is empty or oversized"));
  }
  let operation_name = object
    .get("op")
    .and_then(serde_json::Value::as_str)
    .ok_or_else(|| PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", "query predicate requires a string op"))?;
  ensure_predicate_keys(object, operation_name)?;
  let raw_value =
    object.get("value").ok_or_else(|| PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", "query predicate requires value"))?;
  let operation = match operation_name {
    "eq" => QueryPredicateOperationV1::Eq(admit_literal_v1(raw_value, admission)?),
    "in" => {
      let values =
        raw_value.as_array().ok_or_else(|| PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", "in requires an array value"))?;
      if values.len() > QUERY_MAXIMUM_IN_LITERALS_V1 {
        return Err(expression_limit("in literal count exceeds the protocol maximum"));
      }
      let mut admitted = Vec::new();
      admitted.try_reserve_exact(values.len()).map_err(|source| expression_limit(format!("cannot reserve in literals: {source}")))?;
      for value in values {
        admitted.push(admit_literal_v1(value, admission)?);
      }
      QueryPredicateOperationV1::In(admitted)
    }
    "gt" => QueryPredicateOperationV1::Gt(admit_literal_v1(raw_value, admission)?),
    "lt" => QueryPredicateOperationV1::Lt(admit_literal_v1(raw_value, admission)?),
    "between" => {
      let second = object.get("value2").ok_or_else(|| PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", "between requires value2"))?;
      QueryPredicateOperationV1::Between(admit_literal_v1(raw_value, admission)?, admit_literal_v1(second, admission)?)
    }
    "contains" => QueryPredicateOperationV1::Contains(admit_string_literal_v1(raw_value, admission)?),
    "similar" => {
      let threshold = match object.get("threshold") {
        None => 0.3,
        Some(value) => {
          value.as_f64().ok_or_else(|| PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", "similar threshold must be a number"))?
        }
      };
      if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
        return Err(PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", "similar threshold must be finite and within 0..=1"));
      }
      QueryPredicateOperationV1::Similar { value: admit_string_literal_v1(raw_value, admission)?, threshold }
    }
    "phonetic" => QueryPredicateOperationV1::Phonetic(admit_string_literal_v1(raw_value, admission)?),
    "fuzzy" => {
      let algorithm = match object.get("algorithm") {
        None => QueryFuzzyAlgorithmV1::DamerauLevenshtein,
        Some(serde_json::Value::String(value)) if value == "damerau_levenshtein" => QueryFuzzyAlgorithmV1::DamerauLevenshtein,
        Some(serde_json::Value::String(value)) if value == "jaro_winkler" => QueryFuzzyAlgorithmV1::JaroWinkler,
        Some(serde_json::Value::String(_)) => {
          return Err(PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", "unknown fuzzy algorithm"));
        }
        Some(_) => return Err(PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", "fuzzy algorithm must be a string")),
      };
      let edits = match object.get("fuzziness") {
        None => None,
        Some(serde_json::Value::String(value)) if value == "auto" => None,
        Some(value) => {
          let edits = value
            .as_u64()
            .ok_or_else(|| PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", "fuzziness must be auto or a nonnegative integer"))?;
          let edits = u8::try_from(edits).map_err(|source| {
            PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", format!("fuzziness exceeds the supported range: {source}"))
          })?;
          if edits > QUERY_MAXIMUM_FUZZY_EDITS_V1 {
            return Err(expression_limit("fuzziness exceeds the protocol maximum"));
          }
          Some(edits)
        }
      };
      QueryPredicateOperationV1::Fuzzy { value: admit_string_literal_v1(raw_value, admission)?, algorithm, edits }
    }
    "match" => QueryPredicateOperationV1::Match(admit_string_literal_v1(raw_value, admission)?),
    _ => return Err(PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", format!("unknown query operation {operation_name:?}"))),
  };
  Ok(QueryPredicateV1 { field_name: field_name.to_string(), operation })
}

fn ensure_only_keys(object: &serde_json::Map<String, serde_json::Value>, allowed: &[&str]) -> Result<(), PublicSchemaErrorV1> {
  if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
    return Err(PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", format!("unknown query expression field {key:?}")));
  }
  Ok(())
}

fn ensure_predicate_keys(object: &serde_json::Map<String, serde_json::Value>, operation: &str) -> Result<(), PublicSchemaErrorV1> {
  let allowed = match operation {
    "between" => &["field", "op", "value", "value2"][..],
    "similar" => &["field", "op", "value", "threshold"][..],
    "fuzzy" => &["field", "op", "value", "algorithm", "fuzziness"][..],
    _ => &["field", "op", "value"][..],
  };
  ensure_only_keys(object, allowed)
}

fn admit_string_literal_v1(
  value: &serde_json::Value,
  admission: &mut QueryExpressionAdmissionV1,
) -> Result<CanonicalConfigValueV1, PublicSchemaErrorV1> {
  if !value.is_string() {
    return Err(PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", "query operation requires a string literal"));
  }
  admit_literal_v1(value, admission)
}

fn admit_literal_v1(
  value: &serde_json::Value,
  admission: &mut QueryExpressionAdmissionV1,
) -> Result<CanonicalConfigValueV1, PublicSchemaErrorV1> {
  let encoded_length = serde_json::to_vec(value)
    .map_err(|source| PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", format!("cannot encode query literal: {source}")))?
    .len();
  if encoded_length > QUERY_MAXIMUM_LITERAL_BYTES_V1 {
    return Err(expression_limit("query literal exceeds the per-literal byte maximum"));
  }
  admission.total_literal_bytes =
    admission.total_literal_bytes.checked_add(encoded_length).ok_or_else(|| expression_limit("query literal byte count overflow"))?;
  if admission.total_literal_bytes > QUERY_MAXIMUM_TOTAL_LITERAL_BYTES_V1 {
    return Err(expression_limit("query literals exceed the request-total byte maximum"));
  }
  match value {
    serde_json::Value::Null => Ok(CanonicalConfigValueV1::Null),
    serde_json::Value::Bool(value) => Ok(CanonicalConfigValueV1::Boolean(*value)),
    serde_json::Value::Number(value) => {
      if let Some(value) = value.as_u64() {
        Ok(CanonicalConfigValueV1::Unsigned(value))
      } else if let Some(value) = value.as_i64() {
        Ok(CanonicalConfigValueV1::Signed(value))
      } else if let Some(value) = value.as_f64() {
        Ok(CanonicalConfigValueV1::FloatBits(value.to_bits()))
      } else {
        Err(PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", "query literal number is unsupported"))
      }
    }
    serde_json::Value::String(value) => Ok(CanonicalConfigValueV1::String(value.clone())),
    serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
      Err(PublicSchemaErrorV1::new("INVALID_QUERY_EXPRESSION", "query literals must be JSON scalars"))
    }
  }
}

fn expression_limit(context: impl Into<String>) -> PublicSchemaErrorV1 {
  PublicSchemaErrorV1::new("QUERY_EXPRESSION_LIMIT_EXCEEDED", context)
}

fn public_root_error(error: RootApiErrorV1) -> PublicSchemaErrorV1 {
  PublicSchemaErrorV1::new(error.code(), error.message())
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct PublicCollectionMetadataV1 {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub has_more: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub next_cursor: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub prev_cursor: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub total: Option<u64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub total_count: Option<u64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub limit: Option<u64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub offset: Option<u64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub default_limit_hit: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub default_limit: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PublicItemsResponseV1<Items> {
  pub root: RootResponseV1,
  pub items: Items,
  #[serde(flatten)]
  pub metadata: PublicCollectionMetadataV1,
}

impl<Items> PublicItemsResponseV1<Items> {
  pub fn new(root: RootResponseV1, items: Items, metadata: PublicCollectionMetadataV1) -> Self {
    Self { root, items, metadata }
  }
}

#[derive(Clone, Debug, Serialize)]
pub struct PublicResultsResponseV1<Results> {
  pub root: RootResponseV1,
  pub results: Results,
  #[serde(flatten)]
  pub metadata: PublicCollectionMetadataV1,
}

impl<Results> PublicResultsResponseV1<Results> {
  pub fn new(root: RootResponseV1, results: Results, metadata: PublicCollectionMetadataV1) -> Self {
    Self { root, results, metadata }
  }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PublicHalfOpenRangeV1 {
  pub start: u64,
  pub end: u64,
}

impl PublicHalfOpenRangeV1 {
  pub fn validate(self) -> Result<Self, PublicSchemaErrorV1> {
    if self.end < self.start {
      return Err(PublicSchemaErrorV1::new("INVALID_RANGE", "range end precedes range start"));
    }
    Ok(self)
  }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PublicLineColumnPointV1 {
  pub line: u64,
  pub column: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PublicLineColumnRangeV1 {
  pub start: PublicLineColumnPointV1,
  pub end: PublicLineColumnPointV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PublicLocatorMatchSemanticsV1 {
  ExactBytes,
  AsciiCaseInsensitiveBytes,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PublicLocatorContinuationV1 {
  pub next_candidate_byte: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PublicLocatorMatchV1 {
  pub path: String,
  pub file_key: String,
  pub record_revision: String,
  pub content_hash: String,
  pub updated_at: i64,
  pub matching_semantics: PublicLocatorMatchSemanticsV1,
  pub byte_range: PublicHalfOpenRangeV1,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub unicode_scalar_range: Option<PublicHalfOpenRangeV1>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub line_column_range: Option<PublicLineColumnRangeV1>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub continuation: Option<PublicLocatorContinuationV1>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "unit", rename_all = "snake_case")]
pub enum PublicRangeSelectionV1 {
  Bytes { start: u64, end: u64 },
  UnicodeScalars { start: u64, end: u64 },
  Lines { start: u64, end: u64 },
}

impl PublicRangeSelectionV1 {
  pub fn validate(self) -> Result<Self, PublicSchemaErrorV1> {
    let (start, end) = match self {
      Self::Bytes { start, end } | Self::UnicodeScalars { start, end } | Self::Lines { start, end } => (start, end),
    };
    PublicHalfOpenRangeV1 { start, end }.validate()?;
    Ok(self)
  }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PublicRangeContinuationV1 {
  pub remaining: PublicHalfOpenRangeV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PublicAffectedRelationshipChangeV1 {
  Created,
  Updated,
  Deleted,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PublicEntryTypeV1 {
  File,
  Directory,
  Symlink,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PublicAffectedRelationshipV1 {
  pub path: String,
  pub entry_type: Option<PublicEntryTypeV1>,
  pub change: PublicAffectedRelationshipChangeV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PublicMutationKindV1 {
  FileWrite,
  FileDelete,
  DirectoryCreate,
  DirectoryDelete,
  SymlinkWrite,
  SymlinkDelete,
  Copy,
  Rename,
  BatchWrite,
  Merge,
  Restore,
  Promote,
  Import,
  SyncApply,
  SystemWrite,
  PluginWrite,
  MaintenanceRepair,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PublicMutationEventMetadataV1 {
  pub operation_id: Uuid,
  pub mutation_kind: PublicMutationKindV1,
  pub publication_sequence: u64,
  pub previous_root_hash: String,
  pub root_hash: String,
  pub affected_relationships: Vec<PublicAffectedRelationshipV1>,
}
