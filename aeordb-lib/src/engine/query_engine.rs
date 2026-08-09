use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use base64::Engine as _;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::engine::directory_listing::list_directory_recursive_strict;
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::index_store::{FieldIndex, IndexLoadMemoryAccount, IndexManager};
use crate::engine::json_parser::parse_json_fields;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::nvt_ops::NVTMask;
use crate::engine::path_utils::{normalize_path, parent_path};
use crate::engine::query_runtime::{QueryRequestBudget, QueryRuntimeReservation};
use crate::engine::scalar_converter::{
  ScalarConverter, TrigramConverter, CONVERTER_TYPE_U8, CONVERTER_TYPE_U16, CONVERTER_TYPE_U32, CONVERTER_TYPE_U64, CONVERTER_TYPE_I64,
  CONVERTER_TYPE_F64, CONVERTER_TYPE_STRING, CONVERTER_TYPE_TIMESTAMP,
};
use crate::engine::storage_engine::StorageEngine;

// ---------------------------------------------------------------------------
// Type aliases for the query + aggregation pipelines
// ---------------------------------------------------------------------------
//
// Most of the query engine works in terms of three things: file hashes, raw
// field-value bytes, and scalar type tags. The natural Rust spellings
// (`HashMap<Vec<u8>, Vec<u8>>`, `(HashMap<...>, u8)`, etc.) are mechanical to
// read once but very noisy when nested. These aliases name the things the
// engine actually traffics in.

/// One file's stored field value, expressed as raw bytes. The accompanying
/// type tag tells callers how to interpret them (see `bytes_to_f64`,
/// `bytes_to_json_value`).
type FieldValueBytes = HashMap<Vec<u8>, Vec<u8>>;

/// A loaded field index: the per-file value bytes plus the converter type
/// tag (u8) so callers know how to interpret them.
type FieldIndexData = (FieldValueBytes, u8);

/// Aggregate field indexes keyed by field name. Used during SUM/AVG/MIN/MAX
/// to look up each file's value for each requested aggregate field.
type FieldIndexMap = HashMap<String, FieldIndexData>;

/// GROUP BY field data: positional `Vec` of `(field_name, values, type_tag)`
/// preserving the order declared in the query.
type GroupFieldEntries = Vec<(String, FieldValueBytes, u8)>;

/// One bucket of a GROUP BY result: the group-key field values plus the
/// file hashes assigned to that group.
type GroupBucket = (HashMap<String, serde_json::Value>, Vec<Vec<u8>>);

/// All GROUP BY buckets keyed by composite group key string.
type GroupBuckets = HashMap<String, GroupBucket>;

/// Candidate set returned by fuzzy/trigram indexes: the set of matching file
/// hashes plus their raw stored values (used during the recheck phase to
/// filter false positives before returning to the caller).
type FuzzyCandidates = (HashSet<Vec<u8>>, FieldValueBytes);

/// Output of `compute_aggregates`: per-aggregate-field result maps.
struct ComputedAggregates {
  sum: HashMap<String, f64>,
  avg: HashMap<String, f64>,
  min: HashMap<String, serde_json::Value>,
  max: HashMap<String, serde_json::Value>,
}

/// A query operation on a single indexed field.
///
/// Scalar operations (`Eq`, `Gt`, `Lt`, `Between`, `In`) compare against
/// the field's NVT scalar values. Text operations (`Contains`, `Similar`,
/// `Phonetic`, `Fuzzy`, `Match`) use trigram, phonetic, and edit-distance
/// indexes with a recheck phase.
#[derive(Debug, Clone)]
pub enum QueryOp {
  /// Exact equality match on a scalar value.
  Eq(Vec<u8>),
  /// Greater-than comparison on a scalar value.
  Gt(Vec<u8>),
  /// Less-than comparison on a scalar value.
  Lt(Vec<u8>),
  /// Inclusive range match between two scalar values.
  Between(Vec<u8>, Vec<u8>),
  /// Match any of the given scalar values (set membership).
  In(Vec<Vec<u8>>),
  /// Substring match via trigram AND + recheck.
  Contains(String),
  /// Trigram similarity with threshold (Dice coefficient).
  Similar(String, f64),
  /// Phonetic code match (soundex / double metaphone).
  Phonetic(String),
  /// Edit distance or Jaro-Winkler fuzzy match.
  Fuzzy(String, FuzzyOptions),
  /// Composite: run all matching indexes and fuse scores.
  Match(String),
}

/// Options for the [`QueryOp::Fuzzy`] query operation.
#[derive(Debug, Clone)]
pub struct FuzzyOptions {
  /// How many edits to allow.
  pub fuzziness: Fuzziness,
  /// Which matching algorithm to use.
  pub algorithm: FuzzyAlgorithm,
}

/// Controls the allowed edit distance for fuzzy matching.
#[derive(Debug, Clone)]
pub enum Fuzziness {
  /// Automatically determined by term length (0-2 chars: 0, 3-5: 1, 6+: 2).
  Auto,
  /// Fixed edit distance.
  Fixed(usize),
}

/// Fuzzy matching algorithm selection.
#[derive(Debug, Clone)]
pub enum FuzzyAlgorithm {
  /// Damerau-Levenshtein edit distance (transpositions count as one edit).
  DamerauLevenshtein,
  /// Jaro-Winkler similarity (prefix-weighted).
  JaroWinkler,
}

impl Default for FuzzyOptions {
  fn default() -> Self {
    FuzzyOptions { fuzziness: Fuzziness::Auto, algorithm: FuzzyAlgorithm::DamerauLevenshtein }
  }
}

/// Sort direction for ORDER BY clauses.
#[derive(Debug, Clone)]
pub enum SortDirection {
  /// Ascending order (smallest first).
  Asc,
  /// Descending order (largest first).
  Desc,
}

/// A single sort field in an ORDER BY clause.
#[derive(Debug, Clone)]
pub struct SortField {
  /// Field name to sort by. Prefix with `@` for built-in fields (`@score`, `@path`, `@hash`, `@size`, `@created_at`, `@updated_at`).
  pub field: String,
  /// Sort direction.
  pub direction: SortDirection,
}

/// Default limit applied when no explicit limit is provided.
pub const DEFAULT_QUERY_LIMIT: usize = 20;

/// Metadata about active reindexing that may affect query freshness.
///
/// Included in paginated responses when a reindex task is in progress
/// for the queried path.
#[derive(Debug, Clone, Serialize)]
pub struct QueryMeta {
  /// Reindexing progress as a fraction (0.0 to 1.0).
  #[serde(skip_serializing_if = "Option::is_none")]
  pub reindexing: Option<f64>,
  /// Estimated time remaining in milliseconds.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub reindexing_eta: Option<i64>,
  /// Number of files indexed so far.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub reindexing_indexed: Option<usize>,
  /// Total number of files to index.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub reindexing_total: Option<usize>,
  /// Timestamp (ms since epoch) when the index became stale.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub reindexing_stale_since: Option<i64>,
}

/// Paginated query response wrapping results with cursor-based pagination metadata.
#[derive(Debug)]
pub struct PaginatedResult {
  /// Matching file results for this page.
  pub results: Vec<QueryResult>,
  /// Total number of matches across all pages (if `include_total` was set).
  pub total_count: Option<u64>,
  /// True if more results exist beyond this page.
  pub has_more: bool,
  /// Opaque cursor to pass as `after` to fetch the next page.
  pub next_cursor: Option<String>,
  /// Opaque cursor to pass as `before` to fetch the previous page.
  pub prev_cursor: Option<String>,
  /// True if the default limit was applied because no explicit limit was provided.
  pub default_limit_hit: bool,
  /// Reindexing status metadata, if applicable.
  pub meta: Option<QueryMeta>,
}

/// A query condition on a single indexed field.
#[derive(Debug, Clone)]
pub struct FieldQuery {
  /// Name of the indexed field to query.
  pub field_name: String,
  /// The comparison or search operation to apply.
  pub operation: QueryOp,
}

/// A tree node representing a boolean query expression.
#[derive(Debug, Clone)]
pub enum QueryNode {
  /// A leaf: single field operation.
  Field(FieldQuery),
  /// All children must match (intersection).
  And(Vec<QueryNode>),
  /// Any child matches (union).
  Or(Vec<QueryNode>),
  /// Invert child (complement).
  Not(Box<QueryNode>),
}

/// Maximum allowed nesting depth for JSON where-clause parsing.
pub const MAX_WHERE_CLAUSE_DEPTH: usize = 32;

/// Convert a query JSON value to the byte representation used by converters.
///
/// Numbers become big-endian u64 bytes, strings become UTF-8 bytes, and
/// booleans become a single byte. This is intentionally stricter than
/// `json_parser::json_value_to_bytes`, because query literals should fail fast
/// when the caller supplies unsupported types.
pub fn json_query_value_to_bytes(value: &serde_json::Value) -> Result<Vec<u8>, String> {
  match value {
    serde_json::Value::Number(number) => {
      if let Some(unsigned) = number.as_u64() {
        Ok(unsigned.to_be_bytes().to_vec())
      } else if let Some(signed) = number.as_i64() {
        Ok((signed as u64).to_be_bytes().to_vec())
      } else if let Some(float) = number.as_f64() {
        Ok((float as u64).to_be_bytes().to_vec())
      } else {
        Err("Unsupported number format".to_string())
      }
    }
    serde_json::Value::String(text) => Ok(text.as_bytes().to_vec()),
    serde_json::Value::Bool(flag) => Ok(vec![if *flag { 1 } else { 0 }]),
    other => Err(format!("Unsupported value type: {}", other)),
  }
}

/// Parse a single field-level where clause JSON object into a `QueryNode::Field`.
pub fn parse_single_field_query(value: &serde_json::Value) -> Result<QueryNode, String> {
  let field = value.get("field").and_then(|v| v.as_str()).ok_or_else(|| "Missing 'field' in where clause".to_string())?;
  let op = value.get("op").and_then(|v| v.as_str()).ok_or_else(|| format!("Missing 'op' in where clause for field '{}'", field))?;
  let raw_value = value.get("value").ok_or_else(|| format!("Missing 'value' in where clause for field '{}'", field))?;

  let operation = match op {
    "eq" => {
      let bytes = json_query_value_to_bytes(raw_value).map_err(|message| format!("Invalid value for field '{}': {}", field, message))?;
      QueryOp::Eq(bytes)
    }
    "gt" => {
      let bytes = json_query_value_to_bytes(raw_value).map_err(|message| format!("Invalid value for field '{}': {}", field, message))?;
      QueryOp::Gt(bytes)
    }
    "lt" => {
      let bytes = json_query_value_to_bytes(raw_value).map_err(|message| format!("Invalid value for field '{}': {}", field, message))?;
      QueryOp::Lt(bytes)
    }
    "between" => {
      let bytes = json_query_value_to_bytes(raw_value).map_err(|message| format!("Invalid value for field '{}': {}", field, message))?;
      let raw_value2 = value.get("value2").ok_or_else(|| format!("Missing value2 for 'between' operation on field '{}'", field))?;
      let bytes2 = json_query_value_to_bytes(raw_value2).map_err(|message| format!("Invalid value2 for field '{}': {}", field, message))?;
      QueryOp::Between(bytes, bytes2)
    }
    "in" => {
      let array = raw_value.as_array().ok_or_else(|| format!("'in' operation requires array value for field '{}'", field))?;
      let mut byte_values = Vec::with_capacity(array.len());
      for item in array {
        let bytes =
          json_query_value_to_bytes(item).map_err(|message| format!("Invalid value in 'in' array for field '{}': {}", field, message))?;
        byte_values.push(bytes);
      }
      QueryOp::In(byte_values)
    }
    "contains" => {
      let s = raw_value.as_str().ok_or_else(|| format!("'contains' requires string value for field '{}'", field))?;
      QueryOp::Contains(s.to_string())
    }
    "similar" => {
      let s = raw_value.as_str().ok_or_else(|| format!("'similar' requires string value for field '{}'", field))?;
      let threshold = value.get("threshold").and_then(|v| v.as_f64()).unwrap_or(0.3);
      QueryOp::Similar(s.to_string(), threshold)
    }
    "phonetic" => {
      let s = raw_value.as_str().ok_or_else(|| format!("'phonetic' requires string value for field '{}'", field))?;
      QueryOp::Phonetic(s.to_string())
    }
    "fuzzy" => {
      let s = raw_value.as_str().ok_or_else(|| format!("'fuzzy' requires string value for field '{}'", field))?;

      let fuzziness = match value.get("fuzziness") {
        Some(v) if v.is_string() && v.as_str() == Some("auto") => Fuzziness::Auto,
        Some(v) if v.is_u64() => Fuzziness::Fixed(v.as_u64().unwrap() as usize),
        Some(v) if v.is_i64() => Fuzziness::Fixed(v.as_i64().unwrap().max(0) as usize),
        _ => Fuzziness::Auto,
      };

      let algorithm = match value.get("algorithm").and_then(|v| v.as_str()) {
        Some("jaro_winkler") => FuzzyAlgorithm::JaroWinkler,
        _ => FuzzyAlgorithm::DamerauLevenshtein,
      };

      QueryOp::Fuzzy(s.to_string(), FuzzyOptions { fuzziness, algorithm })
    }
    "match" => {
      let s = raw_value.as_str().ok_or_else(|| format!("'match' requires string value for field '{}'", field))?;
      QueryOp::Match(s.to_string())
    }
    unknown => return Err(format!("Unknown operation: '{}'", unknown)),
  };

  Ok(QueryNode::Field(FieldQuery { field_name: field.to_string(), operation }))
}

/// Parse a JSON where clause into a boolean query tree.
pub fn parse_where_clause(value: &serde_json::Value) -> Result<QueryNode, String> {
  parse_where_clause_inner(value, 0)
}

fn parse_where_clause_inner(value: &serde_json::Value, depth: usize) -> Result<QueryNode, String> {
  if depth > MAX_WHERE_CLAUSE_DEPTH {
    return Err(format!("Query nesting too deep (max {} levels). Simplify the where clause", MAX_WHERE_CLAUSE_DEPTH));
  }

  if value.is_array() {
    let array = value.as_array().unwrap();
    let children: Result<Vec<QueryNode>, String> = array.iter().map(|v| parse_where_clause_inner(v, depth + 1)).collect();
    return Ok(QueryNode::And(children?));
  }

  if let Some(and_array) = value.get("and") {
    let array = and_array.as_array().ok_or_else(|| "'and' must be an array".to_string())?;
    let children: Result<Vec<QueryNode>, String> = array.iter().map(|v| parse_where_clause_inner(v, depth + 1)).collect();
    return Ok(QueryNode::And(children?));
  }

  if let Some(or_array) = value.get("or") {
    let array = or_array.as_array().ok_or_else(|| "'or' must be an array".to_string())?;
    let children: Result<Vec<QueryNode>, String> = array.iter().map(|v| parse_where_clause_inner(v, depth + 1)).collect();
    return Ok(QueryNode::Or(children?));
  }

  if let Some(not_value) = value.get("not") {
    let child = parse_where_clause_inner(not_value, depth + 1)?;
    return Ok(QueryNode::Not(Box::new(child)));
  }

  if value.get("field").is_some() {
    return parse_single_field_query(value);
  }

  Err(format!("Invalid where clause structure: {}", value))
}

/// Query execution strategy for NVTMask operations.
#[derive(Debug, Clone)]
pub enum QueryStrategy {
  /// Regular full scan of all buckets.
  Full,
  /// Check every Nth bucket, propagate to skipped buckets.
  Strided(usize),
  /// Rough pass at initial_stride, then precise on surviving regions.
  Progressive { initial_stride: usize },
  /// Engine picks based on index sizes.
  Auto,
}

/// EXPLAIN mode for query introspection.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum ExplainMode {
  #[default]
  Off,
  Plan,    // plan only, no execution
  Analyze, // plan + execution + results
}

/// Result of an EXPLAIN query.
#[derive(Debug, Clone, Serialize)]
pub struct ExplainResult {
  pub plan: serde_json::Value,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub execution: Option<serde_json::Value>,
  #[serde(skip_serializing_if = "Option::is_none", rename = "items")]
  pub results: Option<serde_json::Value>,
  #[serde(skip)]
  memory_lease: Option<Arc<QueryMemoryLease>>,
}

impl ExplainResult {
  pub fn new(plan: serde_json::Value, execution: Option<serde_json::Value>, results: Option<serde_json::Value>) -> Self {
    Self { plan, execution, results, memory_lease: None }
  }

  fn estimated_memory_bytes(&self) -> u64 {
    std::mem::size_of::<Self>()
      .saturating_add(estimate_json_value(&self.plan))
      .saturating_add(self.execution.as_ref().map_or(0, estimate_json_value))
      .saturating_add(self.results.as_ref().map_or(0, estimate_json_value)) as u64
  }
}

/// A complete query against files stored under a given path.
///
/// Combines field-level conditions, boolean logic, pagination, sorting,
/// aggregation, and query-planning hints into a single request object.
#[derive(Debug, Clone)]
pub struct Query {
  /// Directory path to query (e.g. `"/users"`).
  pub path: String,
  /// Flat list of field conditions (legacy; superseded by `node`).
  pub field_queries: Vec<FieldQuery>,
  /// Boolean expression tree of field conditions (AND/OR/NOT).
  pub node: Option<QueryNode>,
  /// Maximum number of results to return. Defaults to [`DEFAULT_QUERY_LIMIT`].
  pub limit: Option<usize>,
  /// Number of results to skip before returning.
  pub offset: Option<usize>,
  /// Sort order for results.
  pub order_by: Vec<SortField>,
  /// Cursor-based pagination: start after this opaque cursor token.
  pub after: Option<String>,
  /// Cursor-based pagination: end before this opaque cursor token.
  pub before: Option<String>,
  /// When true, include the total matching count in the response.
  pub include_total: bool,
  /// NVT mask evaluation strategy hint.
  pub strategy: QueryStrategy,
  /// Optional aggregation to compute over the result set.
  pub aggregate: Option<AggregateQuery>,
  /// EXPLAIN mode for query plan introspection.
  pub explain: ExplainMode,
}

/// Aggregation query specifying which statistics to compute over the result set.
#[derive(Debug, Clone, Default)]
pub struct AggregateQuery {
  /// Whether to count matching entries.
  pub count: bool,
  /// Fields to sum.
  pub sum: Vec<String>,
  /// Fields to average.
  pub avg: Vec<String>,
  /// Fields to find the minimum value of.
  pub min: Vec<String>,
  /// Fields to find the maximum value of.
  pub max: Vec<String>,
  /// Fields to group results by before aggregating.
  pub group_by: Vec<String>,
}

/// Result of an aggregation query, containing computed statistics and optional group-by results.
#[derive(Debug, Clone, Serialize)]
pub struct AggregateResult {
  /// Total count of matching entries (if `count` was requested).
  #[serde(skip_serializing_if = "Option::is_none")]
  pub count: Option<u64>,
  /// Sum of each requested field.
  #[serde(skip_serializing_if = "HashMap::is_empty")]
  pub sum: HashMap<String, f64>,
  /// Average of each requested field.
  #[serde(skip_serializing_if = "HashMap::is_empty")]
  pub avg: HashMap<String, f64>,
  /// Minimum value of each requested field.
  #[serde(skip_serializing_if = "HashMap::is_empty")]
  pub min: HashMap<String, serde_json::Value>,
  /// Maximum value of each requested field.
  #[serde(skip_serializing_if = "HashMap::is_empty")]
  pub max: HashMap<String, serde_json::Value>,
  /// Per-group aggregation results when `group_by` was specified.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub groups: Option<Vec<GroupResult>>,
  /// True if more groups exist beyond the limit.
  pub has_more: bool,
  /// True if the default limit was applied (no explicit limit provided).
  #[serde(skip_serializing_if = "std::ops::Not::not")]
  pub default_limit_hit: bool,
  #[serde(skip)]
  memory_lease: Option<Arc<QueryMemoryLease>>,
}

impl AggregateResult {
  pub fn new(
    count: Option<u64>,
    sum: HashMap<String, f64>,
    avg: HashMap<String, f64>,
    min: HashMap<String, serde_json::Value>,
    max: HashMap<String, serde_json::Value>,
    groups: Option<Vec<GroupResult>>,
    has_more: bool,
    default_limit_hit: bool,
  ) -> Self {
    Self { count, sum, avg, min, max, groups, has_more, default_limit_hit, memory_lease: None }
  }

  fn estimated_memory_bytes(&self) -> u64 {
    let mut bytes = std::mem::size_of::<Self>();
    bytes = bytes.saturating_add(estimate_f64_map(&self.sum));
    bytes = bytes.saturating_add(estimate_f64_map(&self.avg));
    bytes = bytes.saturating_add(estimate_json_map(&self.min));
    bytes = bytes.saturating_add(estimate_json_map(&self.max));
    if let Some(groups) = &self.groups {
      bytes = bytes.saturating_add(groups.capacity().saturating_mul(std::mem::size_of::<GroupResult>()));
      for group in groups {
        bytes = bytes
          .saturating_add(estimate_json_map(&group.key))
          .saturating_add(estimate_f64_map(&group.sum))
          .saturating_add(estimate_f64_map(&group.avg))
          .saturating_add(estimate_json_map(&group.min))
          .saturating_add(estimate_json_map(&group.max));
      }
    }
    bytes as u64
  }
}

fn estimate_f64_map(values: &HashMap<String, f64>) -> usize {
  values
    .capacity()
    .saturating_mul(std::mem::size_of::<(String, f64)>().saturating_add(2 * std::mem::size_of::<usize>()))
    .saturating_add(values.keys().fold(0usize, |total, key| total.saturating_add(key.capacity())))
}

fn estimate_json_map(values: &HashMap<String, serde_json::Value>) -> usize {
  values
    .capacity()
    .saturating_mul(std::mem::size_of::<(String, serde_json::Value)>().saturating_add(2 * std::mem::size_of::<usize>()))
    .saturating_add(
      values.iter().fold(0usize, |total, (key, value)| total.saturating_add(key.capacity()).saturating_add(estimate_json_value(value))),
    )
}

fn estimate_json_value(value: &serde_json::Value) -> usize {
  match value {
    serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => std::mem::size_of::<serde_json::Value>(),
    serde_json::Value::String(value) => std::mem::size_of::<serde_json::Value>().saturating_add(value.capacity()),
    serde_json::Value::Array(values) => values
      .capacity()
      .saturating_mul(std::mem::size_of::<serde_json::Value>())
      .saturating_add(values.iter().fold(0usize, |total, value| total.saturating_add(estimate_json_value(value)))),
    serde_json::Value::Object(values) => values.iter().fold(
      values.len().saturating_mul(std::mem::size_of::<(String, serde_json::Value)>().saturating_add(2 * std::mem::size_of::<usize>())),
      |total, (key, value)| total.saturating_add(key.capacity()).saturating_add(estimate_json_value(value)),
    ),
  }
}

/// A single group in a GROUP BY aggregation result.
#[derive(Debug, Clone, Serialize)]
pub struct GroupResult {
  /// Group key values (one entry per `group_by` field).
  pub key: HashMap<String, serde_json::Value>,
  /// Number of entries in this group.
  pub count: u64,
  /// Sum of requested fields within this group.
  #[serde(skip_serializing_if = "HashMap::is_empty")]
  pub sum: HashMap<String, f64>,
  /// Average of requested fields within this group.
  #[serde(skip_serializing_if = "HashMap::is_empty")]
  pub avg: HashMap<String, f64>,
  /// Minimum values of requested fields within this group.
  #[serde(skip_serializing_if = "HashMap::is_empty")]
  pub min: HashMap<String, serde_json::Value>,
  /// Maximum values of requested fields within this group.
  #[serde(skip_serializing_if = "HashMap::is_empty")]
  pub max: HashMap<String, serde_json::Value>,
}

/// A single query result containing the matched file and relevance metadata.
#[derive(Debug)]
pub struct QueryResult {
  /// Content-addressed hash of the matching file record.
  pub file_hash: Vec<u8>,
  /// The full file record (path, size, timestamps, chunk hashes).
  pub file_record: FileRecord,
  /// Relevance score (higher is better). Non-zero for fuzzy/text queries.
  pub score: f64,
  /// Names of the indexes or operations that produced this match.
  pub matched_by: Vec<String>,
  memory_lease: Option<Arc<QueryMemoryLease>>,
}

impl QueryResult {
  pub fn new(file_hash: Vec<u8>, file_record: FileRecord, score: f64, matched_by: Vec<String>) -> Self {
    Self { file_hash, file_record, score, matched_by, memory_lease: None }
  }

  fn estimated_memory_bytes(&self) -> u64 {
    let record = &self.file_record;
    let chunk_bytes = record.chunk_hashes.iter().fold(0usize, |total, hash| total.saturating_add(hash.capacity()));
    let chunk_slots = record.chunk_hashes.capacity().saturating_mul(std::mem::size_of::<Vec<u8>>());
    let matched_bytes = self.matched_by.iter().fold(0usize, |total, value| total.saturating_add(value.capacity()));
    let matched_slots = self.matched_by.capacity().saturating_mul(std::mem::size_of::<String>());
    std::mem::size_of::<Self>()
      .saturating_add(self.file_hash.capacity())
      .saturating_add(record.path.capacity())
      .saturating_add(record.content_type.as_ref().map_or(0, String::capacity))
      .saturating_add(record.metadata.capacity())
      .saturating_add(record.content_hash.capacity())
      .saturating_add(chunk_slots)
      .saturating_add(chunk_bytes)
      .saturating_add(matched_slots)
      .saturating_add(matched_bytes) as u64
  }
}

type QueryResultFilter<'a> = dyn FnMut(&QueryResult) -> EngineResult<bool> + 'a;

fn include_all_query_results(_result: &QueryResult) -> EngineResult<bool> {
  Ok(true)
}

fn retain_query_results(results: &mut Vec<QueryResult>, filter: &mut QueryResultFilter<'_>) -> EngineResult<()> {
  let mut filter_error = None;
  results.retain(|result| {
    if filter_error.is_some() {
      return true;
    }
    match filter(result) {
      Ok(include) => include,
      Err(error) => {
        filter_error = Some(error);
        true
      }
    }
  });
  match filter_error {
    Some(error) => Err(error),
    None => Ok(()),
  }
}

struct QueryMemoryLease {
  reservation: MemoryReservation,
  runtime_reservation: QueryRuntimeReservation,
}

impl std::fmt::Debug for QueryMemoryLease {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("QueryMemoryLease")
      .field("bytes", &self.reservation.bytes())
      .field("runtime_bytes", &self.runtime_reservation.bytes())
      .finish()
  }
}

struct QueryTemporaryMemoryLease {
  _reservation: MemoryReservation,
  _runtime_reservation: QueryRuntimeReservation,
}

struct QueryMemoryBudget {
  coordinator: Arc<MemoryCoordinator>,
  reservation: Option<MemoryReservation>,
  request_budget: QueryRequestBudget,
  runtime_reservation: Option<QueryRuntimeReservation>,
  cancellation: Option<CancellationToken>,
  work_since_cancellation_check: usize,
}

impl QueryMemoryBudget {
  const MINIMUM_WORKSPACE_BYTES: u64 = 4 * 1024;

  fn new_with_cancellation(
    engine: &StorageEngine,
    cancellation: Option<&CancellationToken>,
    request_budget: QueryRequestBudget,
  ) -> EngineResult<Self> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
      return Err(EngineError::Cancelled("query".to_string()));
    }
    let runtime_reservation = request_budget.reserve(Self::MINIMUM_WORKSPACE_BYTES)?;
    let coordinator = engine.memory_coordinator();
    let reservation = coordinator
      .reserve(MemoryOwner::Query, Self::MINIMUM_WORKSPACE_BYTES, AdmissionClass::Workload)
      .map_err(|error| query_memory_error("query workspace admission failed", error))?;
    Ok(Self {
      coordinator,
      reservation: Some(reservation),
      request_budget,
      runtime_reservation: Some(runtime_reservation),
      cancellation: cancellation.cloned(),
      work_since_cancellation_check: 0,
    })
  }

  fn check_cancellation(&self) -> EngineResult<()> {
    if self.cancellation.as_ref().is_some_and(CancellationToken::is_cancelled) {
      return Err(EngineError::Cancelled("query".to_string()));
    }
    Ok(())
  }

  fn record_work(&mut self, units: usize) -> EngineResult<()> {
    const CANCELLATION_QUANTUM: usize = 256;
    self.work_since_cancellation_check = self.work_since_cancellation_check.saturating_add(units);
    if self.work_since_cancellation_check >= CANCELLATION_QUANTUM {
      self.work_since_cancellation_check = 0;
      self.check_cancellation()?;
    }
    Ok(())
  }

  fn runtime_reservation_mut(&mut self) -> EngineResult<&mut QueryRuntimeReservation> {
    self
      .runtime_reservation
      .as_mut()
      .ok_or_else(|| EngineError::IoError(std::io::Error::other("query memory budget lost its runtime reservation")))
  }

  fn grow_pair(&mut self, bytes: u64, context: &str) -> EngineResult<()> {
    if bytes == 0 {
      return Ok(());
    }
    self.check_cancellation()?;
    self.runtime_reservation_mut()?.grow(bytes).map_err(|error| query_runtime_error(context, error))?;
    if let Err(error) = self.reservation_mut()?.grow(bytes) {
      self.runtime_reservation_mut()?.shrink(bytes).map_err(|rollback_error| {
        EngineError::IoError(std::io::Error::other(format!("{context}: runtime rollback failed: {rollback_error}")))
      })?;
      return Err(query_memory_error(context, error));
    }
    Ok(())
  }

  fn shrink_pair(&mut self, bytes: u64, context: &str) -> EngineResult<()> {
    if bytes == 0 {
      return Ok(());
    }
    self.reservation_mut()?.shrink(bytes).map_err(|error| query_memory_error(context, error))?;
    self.runtime_reservation_mut()?.shrink(bytes).map_err(|error| query_runtime_error(context, error))
  }

  fn resize_retained(&mut self, required: u64, admission_context: &str, accounting_context: &str) -> EngineResult<()> {
    let current = self.reservation_mut()?.bytes();
    if required > current {
      self.grow_pair(required - current, admission_context)
    } else {
      self.shrink_pair(current - required, accounting_context)
    }
  }

  fn take_lease(&mut self) -> EngineResult<Arc<QueryMemoryLease>> {
    Ok(Arc::new(QueryMemoryLease {
      reservation: self
        .reservation
        .take()
        .ok_or_else(|| EngineError::IoError(std::io::Error::other("query memory budget lost its final reservation")))?,
      runtime_reservation: self
        .runtime_reservation
        .take()
        .ok_or_else(|| EngineError::IoError(std::io::Error::other("query memory budget lost its final runtime reservation")))?,
    }))
  }

  fn retain_results(mut self, results: &mut [QueryResult]) -> EngineResult<()> {
    if results.is_empty() {
      return Ok(());
    }
    let required = results.iter().fold(0u64, |total, result| total.saturating_add(result.estimated_memory_bytes()));
    self.resize_retained(required, "query result admission failed", "query result accounting failed")?;
    let lease = self.take_lease()?;
    for result in results {
      result.memory_lease = Some(Arc::clone(&lease));
    }
    Ok(())
  }

  fn retain_aggregate(mut self, result: &mut AggregateResult) -> EngineResult<()> {
    let required = result.estimated_memory_bytes().max(1);
    self.resize_retained(required, "aggregate result admission failed", "aggregate result accounting failed")?;
    result.memory_lease = Some(self.take_lease()?);
    Ok(())
  }

  fn retain_explain(mut self, result: &mut ExplainResult) -> EngineResult<()> {
    let required = result.estimated_memory_bytes().max(1);
    self.resize_retained(required, "explain result admission failed", "explain result accounting failed")?;
    result.memory_lease = Some(self.take_lease()?);
    Ok(())
  }

  fn reservation_mut(&mut self) -> EngineResult<&mut MemoryReservation> {
    self.reservation.as_mut().ok_or_else(|| EngineError::IoError(std::io::Error::other("query memory budget lost its reservation")))
  }

  fn reserve_growth(&mut self, bytes: u64, context: &str) -> EngineResult<()> {
    self.grow_pair(bytes, context)
  }

  fn reserve_hash_work(&mut self, entries: usize, hash_length: usize, include_lookup_refs: bool) -> EngineResult<()> {
    let per_hash = std::mem::size_of::<Vec<u8>>()
      .checked_add(hash_length)
      .and_then(|bytes| bytes.checked_add(64))
      .ok_or_else(|| EngineError::ResourceExhausted("query hash work estimate overflow".to_string()))?;
    let per_entry = if include_lookup_refs {
      per_hash
        .checked_add(std::mem::size_of::<&crate::engine::index_store::IndexEntry>())
        .ok_or_else(|| EngineError::ResourceExhausted("query lookup estimate overflow".to_string()))?
    } else {
      per_hash
    };
    let bytes = entries
      .checked_mul(per_entry)
      .and_then(|bytes| u64::try_from(bytes).ok())
      .ok_or_else(|| EngineError::ResourceExhausted("query candidate estimate overflow".to_string()))?;
    self.reserve_growth(bytes, "query candidate admission failed")
  }

  fn reserve_result_slots(&mut self, entries: usize) -> EngineResult<()> {
    let bytes = entries
      .checked_mul(std::mem::size_of::<QueryResult>())
      .and_then(|bytes| u64::try_from(bytes).ok())
      .ok_or_else(|| EngineError::ResourceExhausted("query result slot estimate overflow".to_string()))?;
    self.reserve_growth(bytes, "query result slot admission failed")
  }

  fn reserve_file_record_load(&mut self, value_length: u32) -> EngineResult<()> {
    let bytes = u64::from(value_length)
      .checked_mul(3)
      .and_then(|bytes| bytes.checked_add(512))
      .ok_or_else(|| EngineError::ResourceExhausted("query FileRecord estimate overflow".to_string()))?;
    self.reserve_growth(bytes, "query FileRecord admission failed")
  }

  fn reserve_listing(&mut self, entries: u64) -> EngineResult<()> {
    let bytes = entries.checked_mul(512).ok_or_else(|| EngineError::ResourceExhausted("query listing estimate overflow".to_string()))?;
    self.reserve_growth(bytes, "query listing admission failed")
  }

  fn reserve_stable_sort<T>(&mut self, entries: usize) -> EngineResult<()> {
    let bytes = entries
      .checked_add(1)
      .and_then(|entries| entries.checked_div(2))
      .and_then(|entries| entries.checked_mul(std::mem::size_of::<T>()))
      .and_then(|bytes| u64::try_from(bytes).ok())
      .ok_or_else(|| EngineError::ResourceExhausted("query sort estimate overflow".to_string()))?;
    self.reserve_growth(bytes, "query sort admission failed")
  }

  fn reserve_field_values(&mut self, values: &FieldValueBytes) -> EngineResult<()> {
    let bytes = values.iter().try_fold(0u64, |total, (hash, value)| {
      let entry = std::mem::size_of::<(Vec<u8>, Vec<u8>)>()
        .checked_add(2 * std::mem::size_of::<usize>())
        .and_then(|bytes| bytes.checked_add(hash.len()))
        .and_then(|bytes| bytes.checked_add(value.len()))
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or_else(|| EngineError::ResourceExhausted("query field-value estimate overflow".to_string()))?;
      total.checked_add(entry).ok_or_else(|| EngineError::ResourceExhausted("query field-value estimate overflow".to_string()))
    })?;
    self.reserve_growth(bytes, "query field-value admission failed")
  }

  fn reserve_aggregate_groups(&mut self, entries: usize, group_fields: usize, hash_length: usize) -> EngineResult<()> {
    let key_bytes = group_fields
      .checked_mul(256)
      .and_then(|bytes| bytes.checked_add(256))
      .ok_or_else(|| EngineError::ResourceExhausted("aggregate group estimate overflow".to_string()))?;
    let per_entry = key_bytes
      .checked_add(std::mem::size_of::<Vec<u8>>())
      .and_then(|bytes| bytes.checked_add(hash_length))
      .and_then(|bytes| bytes.checked_add(64))
      .ok_or_else(|| EngineError::ResourceExhausted("aggregate group estimate overflow".to_string()))?;
    let bytes = entries
      .checked_mul(per_entry)
      .and_then(|bytes| u64::try_from(bytes).ok())
      .ok_or_else(|| EngineError::ResourceExhausted("aggregate group estimate overflow".to_string()))?;
    self.reserve_growth(bytes, "aggregate group admission failed")
  }

  fn reserve_group_results(&mut self, groups: usize, aggregate_fields: usize, group_fields: usize) -> EngineResult<()> {
    let dynamic_bytes = aggregate_fields
      .checked_add(group_fields)
      .and_then(|fields| fields.checked_mul(256))
      .ok_or_else(|| EngineError::ResourceExhausted("aggregate result estimate overflow".to_string()))?;
    let per_group = std::mem::size_of::<GroupResult>()
      .checked_add(dynamic_bytes)
      .and_then(|bytes| bytes.checked_add(256))
      .ok_or_else(|| EngineError::ResourceExhausted("aggregate result estimate overflow".to_string()))?;
    let bytes = groups
      .checked_mul(per_group)
      .and_then(|bytes| u64::try_from(bytes).ok())
      .ok_or_else(|| EngineError::ResourceExhausted("aggregate result estimate overflow".to_string()))?;
    self.reserve_growth(bytes, "aggregate result admission failed")
  }

  fn buffered_json_amplification_bytes(file_bytes: u64, record_bytes: u32) -> EngineResult<u64> {
    file_bytes
      .checked_mul(6)
      .and_then(|bytes| bytes.checked_add(u64::from(record_bytes).saturating_mul(2)))
      .and_then(|bytes| bytes.checked_add(1024 * 1024))
      .ok_or_else(|| EngineError::ResourceExhausted("query buffered JSON estimate overflow".to_string()))
  }

  fn reserve_temporary(&self, bytes: u64, context: &str) -> EngineResult<QueryTemporaryMemoryLease> {
    let bytes = bytes.max(1);
    let runtime_reservation = self.request_budget.reserve(bytes).map_err(|error| query_runtime_error(context, error))?;
    let reservation = match self.coordinator.reserve(MemoryOwner::Query, bytes, AdmissionClass::Workload) {
      Ok(reservation) => reservation,
      Err(error) => {
        drop(runtime_reservation);
        return Err(query_memory_error(context, error));
      }
    };
    Ok(QueryTemporaryMemoryLease { _reservation: reservation, _runtime_reservation: runtime_reservation })
  }

  fn reserve_fuzzy_score_scratch(&self, operation: &QueryOp, field_value_bytes: usize) -> EngineResult<QueryTemporaryMemoryLease> {
    let query_bytes = match operation {
      QueryOp::Contains(value)
      | QueryOp::Similar(value, _)
      | QueryOp::Phonetic(value)
      | QueryOp::Fuzzy(value, _)
      | QueryOp::Match(value) => value.len(),
      _ => 0,
    };
    // Scoring can simultaneously hold Unicode-normalized strings, trigram
    // hash sets, token vectors, phonetic codes, and the three OSA rows. The
    // deliberately conservative multiplier admits that transient work before
    // any of those algorithm-specific allocations begin.
    let bytes = query_bytes
      .checked_add(field_value_bytes)
      .and_then(|bytes| bytes.checked_mul(128))
      .and_then(|bytes| bytes.checked_add(64 * 1024))
      .and_then(|bytes| u64::try_from(bytes).ok())
      .ok_or_else(|| EngineError::ResourceExhausted("fuzzy score scratch estimate overflow".to_string()))?;
    self.reserve_temporary(bytes, "fuzzy score scratch admission failed")
  }
}

impl IndexLoadMemoryAccount for QueryMemoryBudget {
  fn grow_index_load(&mut self, bytes: u64, context: &str) -> EngineResult<()> {
    self.grow_pair(bytes, context)
  }

  fn shrink_index_load(&mut self, bytes: u64, context: &str) -> EngineResult<()> {
    self.shrink_pair(bytes, context)
  }
}

fn query_runtime_error(context: &str, error: EngineError) -> EngineError {
  match error {
    EngineError::ResourceExhausted(message) => EngineError::ResourceExhausted(format!("{context}: {message}")),
    other => other,
  }
}

fn query_memory_error(context: &str, error: MemoryCoordinatorError) -> EngineError {
  match error {
    MemoryCoordinatorError::HardLimitExceeded { .. }
    | MemoryCoordinatorError::SoftPressureDeferred { .. }
    | MemoryCoordinatorError::EmergencyReserveExceeded { .. }
    | MemoryCoordinatorError::PolicyUnavailable => EngineError::ResourceExhausted(format!("{context}: {error}")),
    _ => EngineError::IoError(std::io::Error::other(format!("{context}: {error}"))),
  }
}

/// Determine if a QueryNode tree requires bitmap compositing (Tier 2).
/// Returns true if the tree contains any Or or Not nodes.
/// A flat AND of Field leaves uses Tier 1 (direct scalar lookups).
pub fn should_use_bitmap_compositing(node: &QueryNode) -> bool {
  match node {
    QueryNode::Field(_) => false,
    QueryNode::And(children) => children.iter().any(should_use_bitmap_compositing),
    QueryNode::Or(_) => true,
    QueryNode::Not(_) => true,
  }
}

/// Create an NVTMask from a FieldQuery by mapping the query operation
/// onto the NVT bucket space.
/// Currently unused -- retained for future bitmap pruning optimization.
/// TODO: Wire into execute_tier2 when NVT-based pre-filtering is implemented.
#[allow(dead_code)]
fn field_query_to_mask(field_index: &mut FieldIndex, query: &FieldQuery, bucket_count: usize) -> EngineResult<NVTMask> {
  field_index.ensure_nvt_current();
  let converter = field_index.nvt.converter();
  match &query.operation {
    QueryOp::Eq(value) => {
      let scalar = converter.to_scalar(value);
      let bucket = (scalar * bucket_count as f64).min((bucket_count - 1) as f64) as usize;
      // Exclusive end, so bucket..bucket+1 sets one bit.
      Ok(NVTMask::from_range(bucket_count, bucket, bucket + 1))
    }
    QueryOp::Gt(value) => {
      let scalar = converter.to_scalar(value);
      let start_bucket = ((scalar * bucket_count as f64) as usize).min(bucket_count - 1);
      // Include the start bucket (may contain values > target within the bucket).
      Ok(NVTMask::from_range(bucket_count, start_bucket, bucket_count))
    }
    QueryOp::Lt(value) => {
      let scalar = converter.to_scalar(value);
      let end_bucket = ((scalar * bucket_count as f64) as usize).min(bucket_count);
      // Include the end bucket (may contain values < target within the bucket).
      Ok(NVTMask::from_range(bucket_count, 0, end_bucket + 1))
    }
    QueryOp::Between(min, max) => {
      let min_scalar = converter.to_scalar(min);
      let max_scalar = converter.to_scalar(max);
      let start = (min_scalar * bucket_count as f64).min((bucket_count - 1) as f64) as usize;
      let end = ((max_scalar * bucket_count as f64) as usize).min(bucket_count - 1);
      Ok(NVTMask::from_range(bucket_count, start, end + 1))
    }
    QueryOp::In(values) => {
      let mut mask = NVTMask::new(bucket_count);
      for value in values {
        let scalar = converter.to_scalar(value);
        let bucket = (scalar * bucket_count as f64).min((bucket_count - 1) as f64) as usize;
        mask.set_bit(bucket);
      }
      Ok(mask)
    }
    // Fuzzy ops are handled by the recheck path, not NVT masks.
    QueryOp::Contains(_) | QueryOp::Similar(_, _) | QueryOp::Phonetic(_) | QueryOp::Fuzzy(_, _) | QueryOp::Match(_) => {
      Err(EngineError::NotFound("Fuzzy operations do not support NVT mask generation".to_string()))
    }
  }
}

/// Walk the QueryNode tree bottom-up, producing an NVTMask at each level.
/// Currently unused -- retained for future bitmap pruning optimization in
/// execute_tier2. See the execute_tier2 doc comment.
/// TODO: Wire into execute_tier2 when NVT-based pre-filtering is implemented.
#[allow(dead_code)]
fn evaluate_node_as_mask(node: &QueryNode, path: &str, index_manager: &IndexManager, bucket_count: usize) -> EngineResult<NVTMask> {
  match node {
    QueryNode::Field(field_query) => {
      let loaded = index_manager.load_index(path, &field_query.field_name)?;
      let mut index = match loaded {
        Some(index) => index,
        None => {
          return Err(EngineError::NotFound(format!("Index not found for field '{}' at path '{}'", field_query.field_name, path,)));
        }
      };
      field_query_to_mask(&mut index, field_query, bucket_count)
    }
    QueryNode::And(children) => {
      if children.is_empty() {
        return Ok(NVTMask::new(bucket_count));
      }
      let first = evaluate_node_as_mask(&children[0], path, index_manager, bucket_count)?;
      let mut result = first;
      for child in &children[1..] {
        let child_mask = evaluate_node_as_mask(child, path, index_manager, bucket_count)?;
        result = result.and(&child_mask)?;
      }
      Ok(result)
    }
    QueryNode::Or(children) => {
      if children.is_empty() {
        return Ok(NVTMask::new(bucket_count));
      }
      let first = evaluate_node_as_mask(&children[0], path, index_manager, bucket_count)?;
      let mut result = first;
      for child in &children[1..] {
        let child_mask = evaluate_node_as_mask(child, path, index_manager, bucket_count)?;
        result = result.or(&child_mask)?;
      }
      Ok(result)
    }
    QueryNode::Not(child) => {
      let mask = evaluate_node_as_mask(child, path, index_manager, bucket_count)?;
      Ok(mask.not())
    }
  }
}

// ---------------------------------------------------------------------------
// Cursor encoding/decoding for cursor-based pagination
// ---------------------------------------------------------------------------

fn encode_cursor(result: &QueryResult, order_by: &[SortField], version_hash: &[u8]) -> String {
  let mut cursor = serde_json::Map::new();

  for sf in order_by {
    if sf.field.starts_with('@') {
      let value = match sf.field.as_str() {
        "@score" => serde_json::json!(result.score),
        "@path" => serde_json::json!(result.file_record.path),
        "@size" => serde_json::json!(result.file_record.total_size),
        "@created_at" => serde_json::json!(result.file_record.created_at),
        "@updated_at" => serde_json::json!(result.file_record.updated_at),
        "@hash" => serde_json::json!(result.file_record.content_hash_hex()),
        _ => serde_json::Value::Null,
      };
      cursor.insert(sf.field.clone(), value);
    }
  }

  cursor.insert("_hash".to_string(), serde_json::json!(hex::encode(&result.file_hash)));
  cursor.insert("_version".to_string(), serde_json::json!(hex::encode(version_hash)));

  let json = serde_json::Value::Object(cursor);
  base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&json).unwrap_or_default())
}

fn decode_cursor(cursor: &str) -> EngineResult<serde_json::Value> {
  let bytes =
    base64::engine::general_purpose::STANDARD.decode(cursor).map_err(|e| EngineError::JsonParseError(format!("Invalid cursor: {}", e)))?;
  serde_json::from_slice(&bytes).map_err(|e| EngineError::JsonParseError(format!("Invalid cursor JSON: {}", e)))
}

/// Executes queries against the index system.
///
/// Supports scalar lookups, boolean compositing (AND/OR/NOT), text search
/// (trigram, phonetic, fuzzy), cursor-based pagination, sorting, and aggregation.
pub struct QueryEngine<'a> {
  engine: &'a StorageEngine,
  request_budget: Option<QueryRequestBudget>,
}

fn decode_virtual_query_string<'a>(field_name: &str, query_bytes: &'a [u8]) -> EngineResult<&'a str> {
  std::str::from_utf8(query_bytes)
    .map_err(|error| EngineError::InvalidInput(format!("query value for virtual field {field_name} is not valid UTF-8: {error}")))
}

impl<'a> QueryEngine<'a> {
  pub fn new(engine: &'a StorageEngine) -> Self {
    QueryEngine { engine, request_budget: None }
  }

  pub(crate) fn with_request_budget(engine: &'a StorageEngine, request_budget: QueryRequestBudget) -> Self {
    QueryEngine { engine, request_budget: Some(request_budget) }
  }

  fn request_budget(&self) -> EngineResult<QueryRequestBudget> {
    match &self.request_budget {
      Some(request_budget) => Ok(request_budget.clone()),
      None => self.engine.start_query_request_budget(),
    }
  }

  /// Execute a query and return matching file records, applying the default limit.
  ///
  /// Uses a two-tier approach:
  ///   - **Tier 1**: flat AND of field queries via direct scalar lookups.
  ///   - **Tier 2**: complex boolean logic (OR, NOT) via NVTMask bitmap compositing.
  ///
  /// Fuzzy queries (Contains, Similar, Phonetic, Fuzzy) use index-based candidate
  /// generation followed by a recheck phase.
  pub fn execute(&self, query: &Query) -> EngineResult<Vec<QueryResult>> {
    self.execute_with_optional_cancellation(query, None)
  }

  /// Execute a query with cooperative cancellation checked between bounded
  /// units of query work. WASM callers remain independently bounded by fuel.
  pub fn execute_with_cancellation(&self, query: &Query, cancellation: &CancellationToken) -> EngineResult<Vec<QueryResult>> {
    self.execute_with_optional_cancellation(query, Some(cancellation))
  }

  fn execute_with_optional_cancellation(&self, query: &Query, cancellation: Option<&CancellationToken>) -> EngineResult<Vec<QueryResult>> {
    let _operation = self.engine.query_operation_guard()?;
    let timer_start = std::time::Instant::now();
    let mut budget = QueryMemoryBudget::new_with_cancellation(self.engine, cancellation, self.request_budget()?)?;
    let mut results = self.execute_internal(query, &mut budget)?;

    // Apply limit (use DEFAULT_QUERY_LIMIT when no explicit limit).
    let effective_limit = query.limit.unwrap_or(DEFAULT_QUERY_LIMIT);
    results.truncate(effective_limit);
    budget.retain_results(&mut results)?;

    let elapsed = timer_start.elapsed().as_secs_f64();
    metrics::histogram!(crate::metrics::definitions::QUERY_DURATION).record(elapsed);

    Ok(results)
  }

  /// Execute a query with full pagination support.
  ///
  /// Applies default limit, sorting, cursor-based pagination, offset, and
  /// builds pagination metadata including `next_cursor` and `prev_cursor`.
  ///
  /// PERF(M19): This fetches ALL matching results via execute_internal(), sorts them, then
  /// truncates to the requested page. For a query matching 100K files with limit=20, this
  /// materializes all 100K results in memory. Pushing limit/offset into the NVT scan or
  /// using a streaming/lazy iterator would avoid this, but requires significant refactoring
  /// of the index evaluation pipeline.
  pub fn execute_paginated(&self, query: &Query) -> EngineResult<PaginatedResult> {
    self.execute_paginated_with_optional_filter(query, None, &mut include_all_query_results)
  }

  /// Execute a paginated query after applying an authority-owned result filter.
  ///
  /// Filtering occurs before sorting, counts, cursors, offsets, and limits so
  /// callers cannot leak or paginate over rows outside their namespace.
  pub fn execute_paginated_filtered<F>(&self, query: &Query, mut filter: F) -> EngineResult<PaginatedResult>
  where
    F: FnMut(&QueryResult) -> EngineResult<bool>,
  {
    self.execute_paginated_with_optional_filter(query, None, &mut filter)
  }

  pub fn execute_paginated_with_cancellation(&self, query: &Query, cancellation: &CancellationToken) -> EngineResult<PaginatedResult> {
    self.execute_paginated_with_optional_filter(query, Some(cancellation), &mut include_all_query_results)
  }

  fn execute_paginated_with_optional_filter(
    &self,
    query: &Query,
    cancellation: Option<&CancellationToken>,
    filter: &mut QueryResultFilter<'_>,
  ) -> EngineResult<PaginatedResult> {
    let _operation = self.engine.query_operation_guard()?;
    let explicit_limit = query.limit.is_some();
    let effective_limit = query.limit.unwrap_or(DEFAULT_QUERY_LIMIT);
    let mut budget = QueryMemoryBudget::new_with_cancellation(self.engine, cancellation, self.request_budget()?)?;

    // Get all results (without limit)
    let mut all_results = self.execute_internal(query, &mut budget)?;
    retain_query_results(&mut all_results, filter)?;

    // Sort if order_by specified
    if !query.order_by.is_empty() {
      self.sort_results(&mut all_results, &query.order_by, &query.path, &mut budget)?;
    }

    // Count total before pagination
    let total_count = if query.include_total { Some(all_results.len() as u64) } else { None };

    // Apply cursor-based pagination (after sorting, before offset)
    if let Some(ref cursor_str) = query.after {
      let cursor_data = decode_cursor(cursor_str)?;
      let cursor_hash =
        cursor_data.get("_hash").and_then(|v| v.as_str()).ok_or_else(|| EngineError::JsonParseError("Cursor missing _hash".to_string()))?;
      let cursor_hash_bytes = hex::decode(cursor_hash).map_err(|e| EngineError::JsonParseError(format!("Invalid cursor hash: {}", e)))?;

      if let Some(pos) = all_results.iter().position(|r| r.file_hash == cursor_hash_bytes) {
        all_results.drain(..=pos);
      }
    }

    if let Some(ref cursor_str) = query.before {
      let cursor_data = decode_cursor(cursor_str)?;
      let cursor_hash =
        cursor_data.get("_hash").and_then(|v| v.as_str()).ok_or_else(|| EngineError::JsonParseError("Cursor missing _hash".to_string()))?;
      let cursor_hash_bytes = hex::decode(cursor_hash).map_err(|e| EngineError::JsonParseError(format!("Invalid cursor hash: {}", e)))?;

      if let Some(pos) = all_results.iter().position(|r| r.file_hash == cursor_hash_bytes) {
        all_results.truncate(pos);
      }
    }

    // Apply offset
    let offset = query.offset.unwrap_or(0);
    if offset > 0 {
      if offset < all_results.len() {
        all_results.drain(..offset);
      } else {
        all_results.clear();
      }
    }

    // Determine has_more
    let has_more = all_results.len() > effective_limit;

    // Apply limit
    all_results.truncate(effective_limit);

    let default_limit_hit = !explicit_limit && has_more;

    // Build cursors
    let version_hash = self.engine.head_hash()?;

    let next_cursor = if has_more { all_results.last().map(|last| encode_cursor(last, &query.order_by, &version_hash)) } else { None };

    let prev_cursor = if offset > 0 || query.after.is_some() {
      all_results.first().map(|first| encode_cursor(first, &query.order_by, &version_hash))
    } else {
      None
    };

    budget.retain_results(&mut all_results)?;

    Ok(PaginatedResult { results: all_results, total_count, has_more, next_cursor, prev_cursor, default_limit_hit, meta: None })
  }

  /// Execute an EXPLAIN query, returning plan info and optionally execution metrics + results.
  pub fn execute_explain(&self, query: &Query) -> EngineResult<ExplainResult> {
    self.execute_explain_with_optional_filter(query, None, &mut include_all_query_results)
  }

  /// Execute EXPLAIN/ANALYZE with the same authority filter used by ordinary
  /// pagination and aggregation. Plan-only requests do not enumerate rows.
  pub fn execute_explain_filtered<F>(&self, query: &Query, mut filter: F) -> EngineResult<ExplainResult>
  where
    F: FnMut(&QueryResult) -> EngineResult<bool>,
  {
    self.execute_explain_with_optional_filter(query, None, &mut filter)
  }

  pub fn execute_explain_with_cancellation(&self, query: &Query, cancellation: &CancellationToken) -> EngineResult<ExplainResult> {
    self.execute_explain_with_optional_filter(query, Some(cancellation), &mut include_all_query_results)
  }

  fn execute_explain_with_optional_filter(
    &self,
    query: &Query,
    cancellation: Option<&CancellationToken>,
    filter: &mut QueryResultFilter<'_>,
  ) -> EngineResult<ExplainResult> {
    let _operation = self.engine.query_operation_guard()?;
    let request_budget = self.request_budget()?;
    let mut budget = QueryMemoryBudget::new_with_cancellation(self.engine, cancellation, request_budget.clone())?;
    let index_manager = IndexManager::new(self.engine);

    // Build the plan by analyzing the query structure
    let plan = self.build_plan(query, &index_manager, &mut budget)?;

    if query.explain == ExplainMode::Plan {
      let mut result = ExplainResult::new(plan, None, None);
      budget.retain_explain(&mut result)?;
      return Ok(result);
    }

    // Analyze mode: execute and time it
    let start = std::time::Instant::now();

    let (results_json, candidate_count, result_count) = if query.aggregate.is_some() {
      let scoped_engine = QueryEngine::with_request_budget(self.engine, request_budget.clone());
      let agg_result = scoped_engine.execute_aggregate_with_optional_filter(query, cancellation, filter)?;
      let count = agg_result.count.unwrap_or(0);
      (Some(serde_json::to_value(&agg_result).unwrap_or_default()), count as usize, count as usize)
    } else {
      let scoped_engine = QueryEngine::with_request_budget(self.engine, request_budget.clone());
      let paginated = scoped_engine.execute_paginated_with_optional_filter(query, cancellation, filter)?;
      let total = paginated.total_count.unwrap_or(paginated.results.len() as u64);
      let returned = paginated.results.len();
      let results_value = serde_json::json!({
        "items": paginated.results.iter().map(|r| {
          serde_json::json!({
            "path": r.file_record.path,
            "score": r.score,
          })
        }).collect::<Vec<_>>(),
        "has_more": paginated.has_more,
      });
      (Some(results_value), total as usize, returned)
    };

    let duration = start.elapsed();

    let execution = serde_json::json!({
      "total_duration_ms": duration.as_secs_f64() * 1000.0,
      "candidates_generated": candidate_count,
      "results_returned": result_count,
    });

    let mut result = ExplainResult::new(plan, Some(execution), results_json);
    budget.retain_explain(&mut result)?;
    Ok(result)
  }

  /// Build a query execution plan without running the query.
  fn build_plan(&self, query: &Query, index_manager: &IndexManager, budget: &mut QueryMemoryBudget) -> EngineResult<serde_json::Value> {
    let mut plan = serde_json::Map::new();

    // Analyze the query node tree
    if let Some(ref node) = query.node {
      plan.insert("query_tree".to_string(), self.explain_node(node, &query.path, index_manager, budget)?);
      plan.insert("bitmap_compositing".to_string(), serde_json::json!(should_use_bitmap_compositing(node)));
    }

    if !query.order_by.is_empty() {
      let sort_fields: Vec<serde_json::Value> = query
        .order_by
        .iter()
        .map(|sf| {
          serde_json::json!({
            "field": sf.field,
            "direction": match sf.direction { SortDirection::Asc => "asc", SortDirection::Desc => "desc" },
          })
        })
        .collect();
      plan.insert("order_by".to_string(), serde_json::json!(sort_fields));
    }

    if let Some(ref agg) = query.aggregate {
      plan.insert(
        "aggregate".to_string(),
        serde_json::json!({
          "count": agg.count,
          "sum": agg.sum,
          "avg": agg.avg,
          "min": agg.min,
          "max": agg.max,
          "group_by": agg.group_by,
        }),
      );
    }

    plan.insert("limit".to_string(), serde_json::json!(query.limit.unwrap_or(DEFAULT_QUERY_LIMIT)));
    if let Some(offset) = query.offset {
      plan.insert("offset".to_string(), serde_json::json!(offset));
    }

    Ok(serde_json::Value::Object(plan))
  }

  /// Explain a single query node, showing field info, operation, and index details.
  fn explain_node(
    &self,
    node: &QueryNode,
    path: &str,
    index_manager: &IndexManager,
    budget: &mut QueryMemoryBudget,
  ) -> EngineResult<serde_json::Value> {
    match node {
      QueryNode::Field(fq) => {
        let op_name = match &fq.operation {
          QueryOp::Eq(_) => "eq",
          QueryOp::Gt(_) => "gt",
          QueryOp::Lt(_) => "lt",
          QueryOp::Between(_, _) => "between",
          QueryOp::In(_) => "in",
          QueryOp::Contains(_) => "contains",
          QueryOp::Similar(_, _) => "similar",
          QueryOp::Phonetic(_) => "phonetic",
          QueryOp::Fuzzy(_, _) => "fuzzy",
          QueryOp::Match(_) => "match",
        };

        let index_field_name = canonical_virtual_field_name(&fq.field_name).unwrap_or(fq.field_name.as_str());
        let mut index_source = path.to_string();
        let mut indexes = index_manager.load_indexes_for_field_with_memory_account(path, index_field_name, budget)?;
        if indexes.is_empty() && canonical_virtual_field_name(&fq.field_name).is_some() {
          for ancestor in virtual_index_ancestor_paths(path) {
            let ancestor_indexes = index_manager.load_indexes_for_field_with_memory_account(&ancestor, index_field_name, budget)?;
            if !ancestor_indexes.is_empty() {
              index_source = ancestor;
              indexes = ancestor_indexes;
              break;
            }
          }
        }
        let index_info: Vec<serde_json::Value> = indexes
          .iter()
          .map(|idx| {
            serde_json::json!({
              "strategy": idx.converter.strategy(),
              "type": idx.converter.name(),
              "entries": idx.entries.len(),
              "order_preserving": idx.converter.is_order_preserving(),
              "values_stored": idx.values.len(),
            })
          })
          .collect();

        let needs_recheck = matches!(
          &fq.operation,
          QueryOp::Contains(_) | QueryOp::Similar(_, _) | QueryOp::Phonetic(_) | QueryOp::Fuzzy(_, _) | QueryOp::Match(_)
        );

        Ok(serde_json::json!({
          "type": "field",
          "field": fq.field_name,
          "index_field": index_field_name,
          "index_source": index_source,
          "operation": op_name,
          "indexes": index_info,
          "recheck": needs_recheck,
        }))
      }
      QueryNode::And(children) => {
        let child_plans: Vec<serde_json::Value> =
          children.iter().map(|c| self.explain_node(c, path, index_manager, budget)).collect::<EngineResult<Vec<_>>>()?;
        Ok(serde_json::json!({"type": "and", "children": child_plans}))
      }
      QueryNode::Or(children) => {
        let child_plans: Vec<serde_json::Value> =
          children.iter().map(|c| self.explain_node(c, path, index_manager, budget)).collect::<EngineResult<Vec<_>>>()?;
        Ok(serde_json::json!({"type": "or", "children": child_plans}))
      }
      QueryNode::Not(child) => {
        let child_plan = self.explain_node(child, path, index_manager, budget)?;
        Ok(serde_json::json!({"type": "not", "child": child_plan}))
      }
    }
  }

  /// Internal query execution that returns all matching results without applying limit.
  /// Both `execute()` and `execute_paginated()` delegate to this.
  fn execute_internal(&self, query: &Query, budget: &mut QueryMemoryBudget) -> EngineResult<Vec<QueryResult>> {
    // Determine which node tree to evaluate.
    let effective_node = if let Some(ref node) = query.node {
      node.clone()
    } else if query.field_queries.is_empty() {
      return Ok(Vec::new());
    } else {
      // Legacy path: wrap flat field_queries as an implicit AND.
      let leaves: Vec<QueryNode> = query.field_queries.iter().map(|fq| QueryNode::Field(fq.clone())).collect();
      if leaves.len() == 1 {
        leaves.into_iter().next().unwrap()
      } else {
        QueryNode::And(leaves)
      }
    };

    let index_manager = IndexManager::new(self.engine);

    // Check for fuzzy operations that can use indexed recheck. Virtual fields
    // without an index keep their scan fallback for backwards compatibility.
    if self.should_use_recheck_path(&effective_node, &query.path, &index_manager, budget)? {
      return self.execute_with_recheck_internal(query, &effective_node, budget);
    }

    let result_hashes = if should_use_bitmap_compositing(&effective_node) {
      self.execute_tier2(&effective_node, &query.path, &index_manager, budget)?
    } else {
      self.evaluate_node(&effective_node, &query.path, &index_manager, budget)?
    };

    // Load FileRecords for candidates.
    let hash_length = self.engine.hash_algo().hash_length();
    budget.reserve_result_slots(result_hashes.len())?;
    let mut results = Vec::with_capacity(result_hashes.len());

    for file_hash in result_hashes {
      budget.record_work(1)?;
      let Some(preflight_header) = self.engine.get_entry_header(&file_hash)? else {
        continue;
      };
      budget.reserve_file_record_load(preflight_header.value_length)?;
      match self.engine.get_entry(&file_hash) {
        Ok(Some((header, _key, value))) => {
          let file_record = FileRecord::deserialize(&value, hash_length, header.entry_version)?;
          results.push(QueryResult::new(file_hash, file_record, 1.0, vec![]));
        }
        Ok(None) => continue, // stale index entry, skip
        Err(error) => return Err(error),
      }
    }

    Ok(results)
  }

  /// Sort results by the specified order_by fields.
  /// Supports virtual @fields (score, path, size, created_at, updated_at) and
  /// indexed fields with order-preserving converters.
  fn sort_results(
    &self,
    results: &mut Vec<QueryResult>,
    order_by: &[SortField],
    path: &str,
    budget: &mut QueryMemoryBudget,
  ) -> EngineResult<()> {
    if order_by.is_empty() || results.is_empty() {
      return Ok(());
    }

    let index_manager = IndexManager::new(self.engine);

    // For each sort field, prepare the sort data
    struct SortData {
      values: HashMap<Vec<u8>, Vec<u8>>,
      is_virtual: bool,
      field: String,
      direction: SortDirection,
    }

    let mut sort_fields: Vec<SortData> = Vec::new();

    for sf in order_by {
      budget.record_work(1)?;
      if sf.field.starts_with('@') {
        sort_fields.push(SortData { values: HashMap::new(), is_virtual: true, field: sf.field.clone(), direction: sf.direction.clone() });
      } else {
        let indexes = index_manager.load_indexes_for_field_with_memory_account(path, &sf.field, budget)?;
        let index = indexes.into_iter().find(|idx| idx.converter.is_order_preserving()).ok_or_else(|| {
          EngineError::NotFound(format!(
            "Cannot sort by field '{}' — no order-preserving index found. \
               Use a string, numeric, or timestamp index type.",
            sf.field
          ))
        })?;

        sort_fields.push(SortData { values: index.values, is_virtual: false, field: sf.field.clone(), direction: sf.direction.clone() });
      }
    }

    budget.reserve_stable_sort::<QueryResult>(results.len())?;

    results.sort_by(|a, b| {
      for sd in &sort_fields {
        let cmp = if sd.is_virtual {
          match sd.field.as_str() {
            "@score" => a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal),
            "@path" => a.file_record.path.cmp(&b.file_record.path),
            "@hash" => a.file_record.content_hash.cmp(&b.file_record.content_hash),
            "@size" => a.file_record.total_size.cmp(&b.file_record.total_size),
            "@created_at" => a.file_record.created_at.cmp(&b.file_record.created_at),
            "@updated_at" => a.file_record.updated_at.cmp(&b.file_record.updated_at),
            _ => std::cmp::Ordering::Equal,
          }
        } else {
          let va = sd.values.get(&a.file_hash).map(Vec::as_slice).unwrap_or_default();
          let vb = sd.values.get(&b.file_hash).map(Vec::as_slice).unwrap_or_default();
          va.cmp(&vb)
        };

        let cmp = match sd.direction {
          SortDirection::Asc => cmp,
          SortDirection::Desc => cmp.reverse(),
        };

        if cmp != std::cmp::Ordering::Equal {
          return cmp;
        }
      }
      std::cmp::Ordering::Equal
    });
    budget.check_cancellation()?;

    Ok(())
  }

  /// Tier 2: Complex queries with OR/NOT.
  /// Uses the precise set-based evaluation for correctness (especially with
  /// NOT, which requires the full universe). The bitmap mask computation was
  /// removed as it was unused -- when bitmap pruning is needed for large
  /// datasets, re-introduce evaluate_node_as_mask here as a pre-filter.
  fn execute_tier2(
    &self,
    node: &QueryNode,
    path: &str,
    index_manager: &IndexManager,
    budget: &mut QueryMemoryBudget,
  ) -> EngineResult<HashSet<Vec<u8>>> {
    self.evaluate_node(node, path, index_manager, budget)
  }

  /// Tier 1: Recursively evaluate a QueryNode tree using direct scalar lookups,
  /// returning matching file hashes.
  fn evaluate_node(
    &self,
    node: &QueryNode,
    path: &str,
    index_manager: &IndexManager,
    budget: &mut QueryMemoryBudget,
  ) -> EngineResult<HashSet<Vec<u8>>> {
    match node {
      QueryNode::Field(field_query) => self.evaluate_field_query(field_query, path, index_manager, budget),
      QueryNode::And(children) => {
        if children.is_empty() {
          return Ok(HashSet::new());
        }
        let mut result = self.evaluate_node(&children[0], path, index_manager, budget)?;
        for child in &children[1..] {
          budget.record_work(1)?;
          let child_set = self.evaluate_node(child, path, index_manager, budget)?;
          budget.reserve_hash_work(result.len().min(child_set.len()), self.engine.hash_algo().hash_length(), false)?;
          result = result.intersection(&child_set).cloned().collect();
        }
        Ok(result)
      }
      QueryNode::Or(children) => {
        let mut result = HashSet::new();
        for child in children {
          budget.record_work(1)?;
          let child_set = self.evaluate_node(child, path, index_manager, budget)?;
          budget.reserve_hash_work(result.len().saturating_add(child_set.len()), self.engine.hash_algo().hash_length(), false)?;
          result = result.union(&child_set).cloned().collect();
        }
        Ok(result)
      }
      QueryNode::Not(child) => {
        // NOT requires knowing the universe of all file hashes.
        //
        // PERF(M20): collect_all_hashes materializes every file hash in the directory
        // into a HashSet, then computes the set difference. For directories with many
        // thousands of files, this is expensive. A bitmap-based approach (already used
        // in the Tier 2 mask compositing path) avoids this allocation. The bitmap path
        // handles NOT via bitwise negation — this fallback is only hit for Tier 3
        // (non-indexed field queries).
        let child_set = self.evaluate_node(child, path, index_manager, budget)?;
        let all_hashes = self.collect_all_hashes(path, index_manager, budget)?;
        budget.reserve_hash_work(all_hashes.len(), self.engine.hash_algo().hash_length(), false)?;
        Ok(all_hashes.difference(&child_set).cloned().collect())
      }
    }
  }

  /// Evaluate a single FieldQuery leaf against the index (or virtual field scan).
  fn evaluate_field_query(
    &self,
    field_query: &FieldQuery,
    path: &str,
    index_manager: &IndexManager,
    budget: &mut QueryMemoryBudget,
  ) -> EngineResult<HashSet<Vec<u8>>> {
    // Virtual fields first try their configured indexes. If no matching index
    // exists, they fall back to scanning FileRecord metadata for compatibility.
    if canonical_virtual_field_name(&field_query.field_name).is_some() {
      return self.evaluate_virtual_field_query(field_query, path, index_manager, budget);
    }

    self.evaluate_indexed_field_query(field_query, path, index_manager, budget)
  }

  fn evaluate_indexed_field_query(
    &self,
    field_query: &FieldQuery,
    path: &str,
    index_manager: &IndexManager,
    budget: &mut QueryMemoryBudget,
  ) -> EngineResult<HashSet<Vec<u8>>> {
    match &field_query.operation {
      QueryOp::Eq(value) => {
        return self.evaluate_exact_field_values(&field_query.field_name, path, std::slice::from_ref(value), index_manager, budget)
      }
      QueryOp::In(values) => return self.evaluate_exact_field_values(&field_query.field_name, path, values, index_manager, budget),
      _ => {}
    }

    let index = index_manager.load_index_with_memory_account(path, &field_query.field_name, budget)?;
    let mut index = match index {
      Some(index) => index,
      None => {
        return Err(EngineError::NotFound(format!("Index not found for field '{}' at path '{}'", field_query.field_name, path,)));
      }
    };

    budget.reserve_hash_work(index.entries.len(), self.engine.hash_algo().hash_length(), true)?;

    let matching_entries = match &field_query.operation {
      QueryOp::Eq(value) => index.lookup_exact(value).into_iter().map(|entry| entry.file_hash.clone()).collect::<HashSet<Vec<u8>>>(),
      QueryOp::Gt(value) => index.lookup_gt(value)?.into_iter().map(|entry| entry.file_hash.clone()).collect::<HashSet<Vec<u8>>>(),
      QueryOp::Lt(value) => index.lookup_lt(value)?.into_iter().map(|entry| entry.file_hash.clone()).collect::<HashSet<Vec<u8>>>(),
      QueryOp::Between(min, max) => {
        index.lookup_range(min, max)?.into_iter().map(|entry| entry.file_hash.clone()).collect::<HashSet<Vec<u8>>>()
      }
      // Fuzzy ops are handled by execute_with_recheck, not here.
      QueryOp::In(_) | QueryOp::Contains(_) | QueryOp::Similar(_, _) | QueryOp::Phonetic(_) | QueryOp::Fuzzy(_, _) | QueryOp::Match(_) => {
        return Err(EngineError::NotFound("Fuzzy operations should use the recheck execution path".to_string()));
      }
    };

    Ok(matching_entries)
  }

  /// Evaluate exact equality or set-membership against every strategy for a field.
  ///
  /// Exact-capable scalar indexes (`string`, numeric, timestamp, etc.) remain
  /// the fast path. Tokenizing indexes (`trigram`, phonetic) are only scanned
  /// by stored raw value when no exact-capable strategy exists for the field.
  fn evaluate_exact_field_values(
    &self,
    field_name: &str,
    path: &str,
    values: &[Vec<u8>],
    index_manager: &IndexManager,
    budget: &mut QueryMemoryBudget,
  ) -> EngineResult<HashSet<Vec<u8>>> {
    let mut indexes = index_manager.load_indexes_for_field_with_memory_account(path, field_name, budget)?;
    if indexes.is_empty() {
      return Err(EngineError::NotFound(format!("Index not found for field '{}' at path '{}'", field_name, path,)));
    }

    let mut result = HashSet::new();
    let total_entries = indexes
      .iter()
      .try_fold(0usize, |total, index| total.checked_add(index.entries.len()))
      .ok_or_else(|| EngineError::ResourceExhausted("query exact candidate count overflow".to_string()))?;
    budget.reserve_hash_work(total_entries, self.engine.hash_algo().hash_length(), true)?;
    let has_exact_capable_index = indexes.iter().any(|index| index.supports_scalar_exact_lookup());

    if has_exact_capable_index {
      for index in indexes.iter_mut().filter(|index| index.supports_scalar_exact_lookup()) {
        budget.record_work(1)?;
        for value in values {
          budget.record_work(1)?;
          for entry in index.lookup_exact(value) {
            budget.record_work(1)?;
            result.insert(entry.file_hash.clone());
          }
        }
      }
      return Ok(result);
    }

    // No exact-capable index exists. Tokenizing indexes cannot answer exact
    // queries through scalar lookup, because their entries are expanded tokens.
    // Use persisted raw values when present; legacy tokenizing indexes without
    // values have no safe exact-match accelerator and return no matches.
    for index in indexes.iter().filter(|index| !index.values.is_empty()) {
      budget.record_work(1)?;
      for file_hash in index.lookup_stored_values_exact(values) {
        budget.record_work(1)?;
        result.insert(file_hash);
      }
    }

    Ok(result)
  }

  /// Collect all file hashes from all indexed fields at a path.
  /// Used as the "universe" for NOT operations.
  fn collect_all_hashes(&self, path: &str, index_manager: &IndexManager, budget: &mut QueryMemoryBudget) -> EngineResult<HashSet<Vec<u8>>> {
    let field_names = index_manager.list_indexes(path)?;
    let mut all_hashes = HashSet::new();
    for index_name in &field_names {
      budget.record_work(1)?;
      let loaded = if let Some((field_name, strategy)) = index_name.rsplit_once('.') {
        index_manager.load_index_by_strategy_with_memory_account(path, field_name, strategy, budget)?
      } else {
        index_manager.load_index_with_memory_account(path, index_name, budget)?
      };
      if let Some(index) = loaded {
        budget.reserve_hash_work(index.entries.len(), self.engine.hash_algo().hash_length(), false)?;
        for entry in &index.entries {
          budget.record_work(1)?;
          all_hashes.insert(entry.file_hash.clone());
        }
      }
    }
    Ok(all_hashes)
  }

  /// Evaluate a virtual field query, using an index when one is available and
  /// falling back to scanning all files under the path otherwise.
  ///
  /// Virtual fields (`@path`, `@filename`, `@extension`, `@content_type`,
  /// `@size`, `@created_at`, `@updated_at`, `@hash`) are derived from FileRecord
  /// metadata. The scan fallback is O(n) over files in the directory tree.
  fn evaluate_virtual_field_query(
    &self,
    field_query: &FieldQuery,
    path: &str,
    index_manager: &IndexManager,
    budget: &mut QueryMemoryBudget,
  ) -> EngineResult<HashSet<Vec<u8>>> {
    let Some(field_name) = canonical_virtual_field_name(&field_query.field_name) else {
      return Err(EngineError::InvalidInput(format!(
        "Unknown virtual field '{}'. Supported: @path, @filename, @file_name, @extension, \
         @content_type, @size, @created_at, @updated_at, @hash",
        field_query.field_name,
      )));
    };

    if matches!(field_query.operation, QueryOp::Eq(_) | QueryOp::In(_) | QueryOp::Gt(_) | QueryOp::Lt(_) | QueryOp::Between(_, _)) {
      match self.evaluate_indexed_virtual_field_query(field_query, field_name, path, index_manager, budget) {
        Ok(result) => return Ok(result),
        Err(EngineError::NotFound(_)) => {}
        Err(error) => return Err(error),
      }
    }

    if is_recheck_operation(&field_query.operation) {
      match self.evaluate_virtual_recheck_field_query(field_query, field_name, path, index_manager, budget) {
        Ok(result) => return Ok(result),
        Err(EngineError::NotFound(_)) => {}
        Err(error) => return Err(error),
      }
    }

    // Collect all files under the query path via recursive directory listing.
    budget.reserve_listing(self.engine.counters().snapshot().files)?;
    let listing = match list_directory_recursive_strict(self.engine, path, -1, None, None) {
      Ok(entries) => entries,
      Err(EngineError::NotFound(_)) => return Ok(HashSet::new()),
      Err(other) => return Err(other),
    };

    let hash_length = self.engine.hash_algo().hash_length();
    budget.reserve_hash_work(listing.len(), hash_length, false)?;
    let mut matching_hashes = HashSet::new();

    for entry in &listing {
      budget.record_work(1)?;
      // Only consider file entries, not directories.
      if entry.entry_type != EntryType::FileRecord.to_u8() {
        continue;
      }

      // Load the full FileRecord from the entry hash.
      let Some(preflight_header) = self.engine.get_entry_header(&entry.hash)? else {
        continue;
      };
      budget.reserve_file_record_load(preflight_header.value_length)?;
      let file_record = match self.engine.get_entry(&entry.hash) {
        Ok(Some((header, _key, value))) => FileRecord::deserialize(&value, hash_length, header.entry_version)?,
        Ok(None) => continue,
        Err(error) => return Err(error),
      };

      let matches = self.virtual_field_matches(field_name, &file_record, &field_query.operation)?;
      if matches {
        matching_hashes.insert(entry.hash.clone());
      }
    }

    Ok(matching_hashes)
  }

  fn evaluate_virtual_recheck_field_query(
    &self,
    field_query: &FieldQuery,
    field_name: &str,
    path: &str,
    index_manager: &IndexManager,
    budget: &mut QueryMemoryBudget,
  ) -> EngineResult<HashSet<Vec<u8>>> {
    let indexed_query = FieldQuery { field_name: field_name.to_string(), operation: field_query.operation.clone() };
    let (candidates, candidate_values) = self.get_fuzzy_candidates_with_values(&indexed_query, path, index_manager, budget)?;
    if candidates.is_empty() && candidate_values.is_empty() {
      return Err(EngineError::NotFound(format!("No recheck index found for virtual field '{}' at '{}'", field_name, path)));
    }

    let hash_length = self.engine.hash_algo().hash_length();
    let mut matching_hashes = HashSet::new();

    for file_hash in candidates {
      budget.record_work(1)?;
      let Some(preflight_header) = self.engine.get_entry_header(&file_hash)? else {
        continue;
      };
      budget.reserve_file_record_load(preflight_header.value_length)?;
      let Some((header, _key, value)) = self.engine.get_entry(&file_hash)? else {
        continue;
      };
      let file_record = FileRecord::deserialize(&value, hash_length, header.entry_version)?;
      let field_value = candidate_values
        .get(&file_hash)
        .map(|bytes| String::from_utf8_lossy(bytes).to_string())
        .or_else(|| self.virtual_field_value_string(field_name, &file_record));

      let Some(field_value) = field_value else {
        continue;
      };

      let _score_memory = budget.reserve_fuzzy_score_scratch(&field_query.operation, field_value.len())?;
      let (score, _strategy) = self.compute_score(&field_query.operation, &field_value, budget)?;
      if score > 0.0 {
        matching_hashes.insert(file_hash);
      }
    }

    Ok(matching_hashes)
  }

  fn evaluate_indexed_virtual_field_query(
    &self,
    field_query: &FieldQuery,
    field_name: &str,
    path: &str,
    index_manager: &IndexManager,
    budget: &mut QueryMemoryBudget,
  ) -> EngineResult<HashSet<Vec<u8>>> {
    let indexed_query = FieldQuery { field_name: field_name.to_string(), operation: field_query.operation.clone() };

    match self.evaluate_indexed_field_query(&indexed_query, path, index_manager, budget) {
      Ok(result) => return Ok(result),
      Err(EngineError::NotFound(_)) => {}
      Err(error) => return Err(error),
    }

    for ancestor in virtual_index_ancestor_paths(path) {
      budget.record_work(1)?;
      if !self.field_has_index(&ancestor, field_name, index_manager, budget)? {
        continue;
      }

      match self.evaluate_indexed_field_query(&indexed_query, &ancestor, index_manager, budget) {
        Ok(result) => return self.filter_hashes_to_query_path(result, path, budget),
        Err(EngineError::NotFound(_)) => continue,
        Err(error) => return Err(error),
      }
    }

    Err(EngineError::NotFound(format!("Index not found for virtual field '{}' at path '{}' or its ancestors", field_name, path)))
  }

  fn filter_hashes_to_query_path(
    &self,
    hashes: HashSet<Vec<u8>>,
    query_path: &str,
    budget: &mut QueryMemoryBudget,
  ) -> EngineResult<HashSet<Vec<u8>>> {
    let normalized_query_path = normalize_path(query_path);
    if normalized_query_path == "/" {
      return Ok(hashes);
    }

    let hash_length = self.engine.hash_algo().hash_length();
    budget.reserve_hash_work(hashes.len(), hash_length, false)?;
    let mut filtered = HashSet::new();
    for file_hash in hashes {
      budget.record_work(1)?;
      let Some(preflight_header) = self.engine.get_entry_header(&file_hash)? else {
        continue;
      };
      budget.reserve_file_record_load(preflight_header.value_length)?;
      let Some((header, _key, value)) = self.engine.get_entry(&file_hash)? else {
        continue;
      };
      let file_record = FileRecord::deserialize(&value, hash_length, header.entry_version)?;
      if path_is_under_query_path(&file_record.path, &normalized_query_path) {
        filtered.insert(file_hash);
      }
    }

    Ok(filtered)
  }

  /// Check whether a single FileRecord matches a virtual field operation.
  fn virtual_field_matches(&self, field_name: &str, file_record: &FileRecord, operation: &QueryOp) -> EngineResult<bool> {
    match field_name {
      "@path" => self.virtual_string_matches("@path", &file_record.path, operation),
      "@filename" => {
        let filename = file_record.path.rsplit('/').next().unwrap_or("");
        self.virtual_string_matches("@filename", filename, operation)
      }
      "@extension" => {
        let filename = file_record.path.rsplit('/').next().unwrap_or("");
        let extension = filename.rsplit('.').next().unwrap_or("");
        // If there's no dot, rsplit returns the full filename — treat as no extension.
        let extension = if extension == filename { "" } else { extension };
        self.virtual_string_matches("@extension", extension, operation)
      }
      "@content_type" => {
        let content_type = file_record.content_type.as_deref().unwrap_or("");
        self.virtual_string_matches("@content_type", content_type, operation)
      }
      "@hash" => self.virtual_string_matches("@hash", &file_record.content_hash_hex(), operation),
      "@size" => self.virtual_u64_matches(file_record.total_size, operation),
      "@created_at" => self.virtual_i64_matches(file_record.created_at, operation),
      "@updated_at" => self.virtual_i64_matches(file_record.updated_at, operation),
      _ => Ok(false),
    }
  }

  fn virtual_field_value_string(&self, field_name: &str, file_record: &FileRecord) -> Option<String> {
    match field_name {
      "@path" => Some(file_record.path.clone()),
      "@filename" => Some(file_record.path.rsplit('/').next().unwrap_or("").to_string()),
      "@extension" => {
        let filename = file_record.path.rsplit('/').next().unwrap_or("");
        let extension = filename.rsplit('.').next().unwrap_or("");
        Some(if extension == filename { "" } else { extension }.to_string())
      }
      "@content_type" => Some(file_record.content_type.as_deref().unwrap_or("").to_string()),
      "@hash" => Some(file_record.content_hash_hex()),
      "@size" => Some(file_record.total_size.to_string()),
      "@created_at" => Some(file_record.created_at.to_string()),
      "@updated_at" => Some(file_record.updated_at.to_string()),
      _ => None,
    }
  }

  /// Apply a query operation against a string value (for virtual fields).
  /// Supports Eq (exact match), Contains (substring), and In (set membership).
  fn virtual_string_matches(&self, field_name: &str, value: &str, operation: &QueryOp) -> EngineResult<bool> {
    match operation {
      QueryOp::Eq(query_bytes) => {
        let query_str = decode_virtual_query_string(field_name, query_bytes)?;
        Ok(value == query_str)
      }
      QueryOp::Contains(query_str) => Ok(value.contains(query_str.as_str())),
      QueryOp::In(values) => {
        for query_bytes in values {
          let query_str = decode_virtual_query_string(field_name, query_bytes)?;
          if value == query_str {
            return Ok(true);
          }
        }
        Ok(false)
      }
      QueryOp::Gt(query_bytes) => {
        let query_str = decode_virtual_query_string(field_name, query_bytes)?;
        Ok(value > query_str)
      }
      QueryOp::Lt(query_bytes) => {
        let query_str = decode_virtual_query_string(field_name, query_bytes)?;
        Ok(value < query_str)
      }
      QueryOp::Similar(query_str, threshold) => {
        let similarity = crate::engine::fuzzy::trigram_similarity(value, query_str);
        Ok(similarity >= *threshold)
      }
      QueryOp::Phonetic(query_str) => {
        let value_soundex = crate::engine::phonetic::soundex(value);
        let query_soundex = crate::engine::phonetic::soundex(query_str);
        if value_soundex == query_soundex {
          return Ok(true);
        }
        let value_dm = crate::engine::phonetic::dmetaphone_primary(value);
        let query_dm = crate::engine::phonetic::dmetaphone_primary(query_str);
        if value_dm == query_dm {
          return Ok(true);
        }
        if let Some(value_alt) = crate::engine::phonetic::dmetaphone_alt(value) {
          if value_alt == query_dm {
            return Ok(true);
          }
        }
        Ok(false)
      }
      QueryOp::Fuzzy(query_str, options) => {
        let max_distance = match &options.fuzziness {
          Fuzziness::Auto => crate::engine::fuzzy::auto_fuzziness(query_str.len()),
          Fuzziness::Fixed(d) => *d,
        };
        match &options.algorithm {
          FuzzyAlgorithm::DamerauLevenshtein => {
            let distance = crate::engine::fuzzy::damerau_levenshtein(value, query_str);
            Ok(distance <= max_distance)
          }
          FuzzyAlgorithm::JaroWinkler => {
            let similarity = crate::engine::fuzzy::jaro_winkler(value, query_str);
            // JW returns 0.0-1.0; convert max_distance to threshold
            let threshold = if max_distance == 0 { 1.0 } else { 1.0 - (max_distance as f64 * 0.1) };
            Ok(similarity >= threshold.max(0.0))
          }
        }
      }
      QueryOp::Match(query_str) => {
        // Match: fuse trigram similarity + substring check
        let contains = value.to_lowercase().contains(&query_str.to_lowercase());
        let similarity = crate::engine::fuzzy::trigram_similarity(value, query_str);
        Ok(contains || similarity >= 0.3)
      }
      other => Err(EngineError::InvalidInput(format!(
        "Operation '{:?}' is not supported for string virtual fields",
        std::mem::discriminant(other),
      ))),
    }
  }

  /// Apply a query operation against a u64 value (for virtual @size field).
  fn virtual_u64_matches(&self, value: u64, operation: &QueryOp) -> EngineResult<bool> {
    match operation {
      QueryOp::Eq(query_bytes) => {
        let query_value = bytes_to_u64(query_bytes);
        Ok(value == query_value)
      }
      QueryOp::Gt(query_bytes) => {
        let query_value = bytes_to_u64(query_bytes);
        Ok(value > query_value)
      }
      QueryOp::Lt(query_bytes) => {
        let query_value = bytes_to_u64(query_bytes);
        Ok(value < query_value)
      }
      QueryOp::Between(min_bytes, max_bytes) => {
        let min_value = bytes_to_u64(min_bytes);
        let max_value = bytes_to_u64(max_bytes);
        Ok(value >= min_value && value <= max_value)
      }
      QueryOp::In(values) => {
        for query_bytes in values {
          if value == bytes_to_u64(query_bytes) {
            return Ok(true);
          }
        }
        Ok(false)
      }
      other => Err(EngineError::InvalidInput(format!(
        "Operation '{:?}' is not supported for numeric virtual fields",
        std::mem::discriminant(other),
      ))),
    }
  }

  /// Apply a query operation against an i64 value (for virtual @created_at, @updated_at fields).
  fn virtual_i64_matches(&self, value: i64, operation: &QueryOp) -> EngineResult<bool> {
    match operation {
      QueryOp::Eq(query_bytes) => {
        let query_value = bytes_to_i64(query_bytes);
        Ok(value == query_value)
      }
      QueryOp::Gt(query_bytes) => {
        let query_value = bytes_to_i64(query_bytes);
        Ok(value > query_value)
      }
      QueryOp::Lt(query_bytes) => {
        let query_value = bytes_to_i64(query_bytes);
        Ok(value < query_value)
      }
      QueryOp::Between(min_bytes, max_bytes) => {
        let min_value = bytes_to_i64(min_bytes);
        let max_value = bytes_to_i64(max_bytes);
        Ok(value >= min_value && value <= max_value)
      }
      QueryOp::In(values) => {
        for query_bytes in values {
          if value == bytes_to_i64(query_bytes) {
            return Ok(true);
          }
        }
        Ok(false)
      }
      other => Err(EngineError::InvalidInput(format!(
        "Operation '{:?}' is not supported for numeric virtual fields",
        std::mem::discriminant(other),
      ))),
    }
  }

  fn should_use_recheck_path(
    &self,
    node: &QueryNode,
    path: &str,
    index_manager: &IndexManager,
    budget: &mut QueryMemoryBudget,
  ) -> EngineResult<bool> {
    match node {
      QueryNode::Field(fq) => {
        if !is_recheck_operation(&fq.operation) {
          return Ok(false);
        }
        match canonical_virtual_field_name(&fq.field_name) {
          Some(field_name) => self.field_has_index_at_path_or_ancestors(path, field_name, index_manager, budget),
          None => Ok(true),
        }
      }
      // Non-virtual fuzzy queries already require the recheck path. Virtual
      // fuzzy filters inside complex boolean nodes keep their scan fallback so
      // existing virtual-field boolean queries do not become single-field-only.
      QueryNode::And(children) | QueryNode::Or(children) => Ok(children.iter().any(|child| self.node_has_non_virtual_recheck_ops(child))),
      QueryNode::Not(child) => Ok(self.node_has_non_virtual_recheck_ops(child)),
    }
  }

  fn node_has_non_virtual_recheck_ops(&self, node: &QueryNode) -> bool {
    match node {
      QueryNode::Field(fq) => {
        if canonical_virtual_field_name(&fq.field_name).is_some() {
          return false;
        }
        is_recheck_operation(&fq.operation)
      }
      QueryNode::And(children) | QueryNode::Or(children) => children.iter().any(|child| self.node_has_non_virtual_recheck_ops(child)),
      QueryNode::Not(child) => self.node_has_non_virtual_recheck_ops(child),
    }
  }

  fn field_has_index(
    &self,
    path: &str,
    field_name: &str,
    index_manager: &IndexManager,
    budget: &mut QueryMemoryBudget,
  ) -> EngineResult<bool> {
    index_manager.load_indexes_for_field_with_memory_account(path, field_name, budget).map(|indexes| !indexes.is_empty())
  }

  fn field_has_index_at_path_or_ancestors(
    &self,
    path: &str,
    field_name: &str,
    index_manager: &IndexManager,
    budget: &mut QueryMemoryBudget,
  ) -> EngineResult<bool> {
    if self.field_has_index(path, field_name, index_manager, budget)? {
      return Ok(true);
    }

    for ancestor in virtual_index_ancestor_paths(path) {
      budget.record_work(1)?;
      if self.field_has_index(&ancestor, field_name, index_manager, budget)? {
        return Ok(true);
      }
    }
    Ok(false)
  }

  fn load_index_by_strategy_for_query(
    &self,
    path: &str,
    field_name: &str,
    strategy: &str,
    index_manager: &IndexManager,
    include_ancestors: bool,
    budget: &mut QueryMemoryBudget,
  ) -> EngineResult<Option<FieldIndex>> {
    if let Some(index) = index_manager.load_index_by_strategy_with_memory_account(path, field_name, strategy, budget)? {
      return Ok(Some(index));
    }

    if include_ancestors {
      for ancestor in virtual_index_ancestor_paths(path) {
        budget.record_work(1)?;
        if let Some(index) = index_manager.load_index_by_strategy_with_memory_account(&ancestor, field_name, strategy, budget)? {
          return Ok(Some(index));
        }
      }
    }

    Ok(None)
  }

  /// Execute a query containing fuzzy operations with a recheck phase.
  /// Currently supports single-field fuzzy queries (the common case).
  /// Values are loaded from the index's values map instead of re-reading files.
  fn execute_with_recheck_internal(
    &self,
    query: &Query,
    effective_node: &QueryNode,
    budget: &mut QueryMemoryBudget,
  ) -> EngineResult<Vec<QueryResult>> {
    let index_manager = IndexManager::new(self.engine);
    let hash_length = self.engine.hash_algo().hash_length();
    let ops = DirectoryOps::new(self.engine);

    // Extract the single fuzzy field query
    let field_query = match effective_node {
      QueryNode::Field(fq) => fq,
      _ => {
        return Err(EngineError::NotFound("Fuzzy operations currently support single-field queries only".to_string()));
      }
    };

    // Get candidates AND values from the appropriate index
    let (candidates, candidate_values) = self.get_fuzzy_candidates_with_values(field_query, &query.path, &index_manager, budget)?;

    // Recheck phase: get field value from index, compute score
    budget.reserve_result_slots(candidates.len())?;
    let mut results = Vec::with_capacity(candidates.len());

    for file_hash in candidates {
      budget.record_work(1)?;
      // Load the FileRecord for the result
      let Some(preflight_header) = self.engine.get_entry_header(&file_hash)? else {
        continue;
      };
      budget.reserve_file_record_load(preflight_header.value_length)?;
      let file_record = match self.engine.get_entry(&file_hash) {
        Ok(Some((header, _key, value))) => FileRecord::deserialize(&value, hash_length, header.entry_version)?,
        Ok(None) => continue,
        Err(error) => return Err(error),
      };

      // Try to get value from index first. Virtual fields can derive their
      // fallback directly from FileRecord metadata; normal fields fall back to
      // loading/parsing the file body when an old index has no values map.
      let mut fallback_memory = None;
      let field_value = if let Some(value_bytes) = candidate_values.get(&file_hash) {
        String::from_utf8_lossy(value_bytes).to_string()
      } else if let Some(field_name) = canonical_virtual_field_name(&field_query.field_name) {
        match self.virtual_field_value_string(field_name, &file_record) {
          Some(value) => value,
          None => continue,
        }
      } else {
        // Fallback: load file and parse as JSON (for native JSON files without values in index)
        let (_file_record, file_data, memory) = match self.load_file_with_data(&file_hash, hash_length, &ops, budget)? {
          Some(parts) => parts,
          None => continue,
        };
        fallback_memory = Some(memory);
        match self.extract_field_value(&file_data, &field_query.field_name) {
          Some(v) => v,
          None => continue,
        }
      };

      // Compute score based on operation
      let _score_memory = budget.reserve_fuzzy_score_scratch(&field_query.operation, field_value.len())?;
      let (score, strategy) = self.compute_score(&field_query.operation, &field_value, budget)?;
      drop(fallback_memory);

      if score > 0.0 {
        results.push(QueryResult::new(
          file_hash,
          file_record,
          score,
          strategy.split(',').filter(|s| !s.is_empty()).map(String::from).collect(),
        ));
      }
    }

    // Sort by score descending
    budget.reserve_stable_sort::<QueryResult>(results.len())?;
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    budget.check_cancellation()?;

    Ok(results)
  }

  /// Get candidate file hashes and their stored values from the appropriate index for a fuzzy query.
  fn get_fuzzy_candidates_with_values(
    &self,
    field_query: &FieldQuery,
    path: &str,
    index_manager: &IndexManager,
    budget: &mut QueryMemoryBudget,
  ) -> EngineResult<FuzzyCandidates> {
    let mut all_values: FieldValueBytes = HashMap::new();
    let index_field_name = canonical_virtual_field_name(&field_query.field_name).unwrap_or(field_query.field_name.as_str());
    let is_virtual_field = canonical_virtual_field_name(&field_query.field_name).is_some();

    match &field_query.operation {
      QueryOp::Contains(query_str) | QueryOp::Similar(query_str, _) | QueryOp::Fuzzy(query_str, _) => {
        // Use trigram index for candidates
        let mut index =
          match self.load_index_by_strategy_for_query(path, index_field_name, "trigram", index_manager, is_virtual_field, budget)? {
            Some(idx) => idx,
            None => {
              return Err(EngineError::NotFound(format!("Trigram index not found for field '{}' at '{}'", field_query.field_name, path,)));
            }
          };

        let converter = TrigramConverter;
        let mut candidates = HashSet::new();
        budget.reserve_hash_work(index.entries.len(), self.engine.hash_algo().hash_length(), true)?;
        budget.reserve_hash_work(index.entries.len(), self.engine.hash_algo().hash_length(), false)?;
        budget.reserve_field_values(&index.values)?;

        if matches!(&field_query.operation, QueryOp::Contains(_)) {
          // For Contains (substring) queries, we need trigrams that will match
          // regardless of word boundaries. The index stores padded per-word
          // trigrams (e.g. "Item 20" → "  i"," it","ite","tem","em ","  2"," 20","20 ").
          // If we use ALL padded trigrams from the query, boundary trigrams like
          // " 2 " (suffix-padded) would exclude "Item 20" (which has " 20", not " 2 ").
          //
          // Fix: only intersect trigrams that don't contain spaces. These are the
          // interior/core trigrams shared by any word regardless of boundaries.
          // The recheck phase verifies the actual substring match.
          let trigrams = crate::engine::fuzzy::extract_trigrams(query_str);
          let interior_trigrams: Vec<&Vec<u8>> = trigrams.iter().filter(|t| !t.contains(&b' ')).collect();

          // AND: intersect interior trigram lookups for substring matching
          let search_trigrams = if interior_trigrams.is_empty() {
            // Very short query words (1-2 chars each) — no interior trigrams.
            // Fall back to all trigrams for some filtering, even though
            // boundary trigrams may be too restrictive. The recheck will
            // correct any false negatives, but with padded trigrams we may
            // miss some candidates. Use all trigrams with OR (union) to
            // avoid excluding valid matches.
            let mut all_candidates = HashSet::new();
            for trigram in &trigrams {
              budget.record_work(1)?;
              let scalar = converter.to_scalar(trigram);
              let entries = index.lookup_by_scalar(scalar);
              for entry in entries {
                budget.record_work(1)?;
                all_candidates.insert(entry.file_hash.clone());
              }
            }
            candidates = all_candidates;
            Vec::new() // signal: already populated candidates
          } else {
            interior_trigrams.iter().map(|t| (*t).clone()).collect::<Vec<Vec<u8>>>()
          };

          if !search_trigrams.is_empty() {
            let mut first = true;
            for trigram in &search_trigrams {
              budget.record_work(1)?;
              let scalar = converter.to_scalar(trigram);
              let entries = index.lookup_by_scalar(scalar);
              let hashes: HashSet<Vec<u8>> = entries.iter().map(|e| e.file_hash.clone()).collect();
              if first {
                candidates = hashes;
                first = false;
              } else {
                candidates = candidates.intersection(&hashes).cloned().collect();
              }
            }
          }
        } else {
          // Similar/Fuzzy: use padded trigrams with OR (union) for broader candidates
          let trigrams = crate::engine::fuzzy::extract_trigrams(query_str);
          // OR: union all trigram lookups (broader candidates for similarity/fuzzy)
          for trigram in &trigrams {
            budget.record_work(1)?;
            let scalar = converter.to_scalar(trigram);
            let entries = index.lookup_by_scalar(scalar);
            for entry in entries {
              budget.record_work(1)?;
              candidates.insert(entry.file_hash.clone());
            }
          }
        }

        // Collect values from this index
        all_values.extend(index.values.drain());
        if is_virtual_field {
          candidates = self.filter_hashes_to_query_path(candidates, path, budget)?;
        }

        Ok((candidates, all_values))
      }
      QueryOp::Phonetic(query_str) => {
        // Use phonetic indexes for candidates
        // Tokenize query on whitespace — match any word's phonetic code
        let query_words: Vec<&str> = query_str.split_whitespace().filter(|w| w.chars().any(|c| c.is_alphabetic())).collect();

        let mut candidates = HashSet::new();
        let strategies = ["soundex", "dmetaphone", "dmetaphone_alt"];
        let mut found_any_index = false;

        for strategy in &strategies {
          budget.record_work(1)?;
          if let Some(mut index) =
            self.load_index_by_strategy_for_query(path, index_field_name, strategy, index_manager, is_virtual_field, budget)?
          {
            found_any_index = true;
            budget.reserve_hash_work(index.entries.len(), self.engine.hash_algo().hash_length(), true)?;
            budget.reserve_field_values(&index.values)?;

            for word in &query_words {
              budget.record_work(1)?;
              let code = match *strategy {
                "soundex" => crate::engine::phonetic::soundex(word),
                "dmetaphone" => crate::engine::phonetic::dmetaphone_primary(word),
                "dmetaphone_alt" => {
                  crate::engine::phonetic::dmetaphone_alt(word).unwrap_or_else(|| crate::engine::phonetic::dmetaphone_primary(word))
                }
                _ => continue,
              };

              if code.is_empty() {
                continue;
              }

              let scalar = index.converter.to_scalar(code.as_bytes());
              let entries = index.lookup_by_scalar(scalar);
              for entry in entries {
                budget.record_work(1)?;
                candidates.insert(entry.file_hash.clone());
              }
            }

            // Collect values from each phonetic index
            all_values.extend(index.values.drain());
          }
        }

        if !found_any_index {
          return Err(EngineError::NotFound(format!("No phonetic index found for field '{}' at '{}'", field_query.field_name, path,)));
        }
        if is_virtual_field {
          candidates = self.filter_hashes_to_query_path(candidates, path, budget)?;
        }

        Ok((candidates, all_values))
      }
      QueryOp::Match(query_str) => {
        let mut candidates = HashSet::new();

        // Try trigram index
        if let Some(mut index) =
          self.load_index_by_strategy_for_query(path, index_field_name, "trigram", index_manager, is_virtual_field, budget)?
        {
          budget.reserve_hash_work(index.entries.len(), self.engine.hash_algo().hash_length(), true)?;
          budget.reserve_field_values(&index.values)?;
          let trigrams = crate::engine::fuzzy::extract_trigrams(query_str);
          let converter = TrigramConverter;
          for trigram in &trigrams {
            budget.record_work(1)?;
            let scalar = converter.to_scalar(trigram);
            let entries = index.lookup_by_scalar(scalar);
            for entry in entries {
              budget.record_work(1)?;
              candidates.insert(entry.file_hash.clone());
            }
          }
          all_values.extend(index.values.drain());
        }

        // Try phonetic indexes (tokenize query on whitespace)
        let query_words: Vec<&str> = query_str.split_whitespace().filter(|w| w.chars().any(|c| c.is_alphabetic())).collect();
        let phonetic_strategies = ["soundex", "dmetaphone", "dmetaphone_alt"];
        for strategy in &phonetic_strategies {
          budget.record_work(1)?;
          if let Some(mut index) =
            self.load_index_by_strategy_for_query(path, index_field_name, strategy, index_manager, is_virtual_field, budget)?
          {
            budget.reserve_hash_work(index.entries.len(), self.engine.hash_algo().hash_length(), true)?;
            budget.reserve_field_values(&index.values)?;
            for word in &query_words {
              budget.record_work(1)?;
              let code = match *strategy {
                "soundex" => crate::engine::phonetic::soundex(word),
                "dmetaphone" => crate::engine::phonetic::dmetaphone_primary(word),
                "dmetaphone_alt" => {
                  crate::engine::phonetic::dmetaphone_alt(word).unwrap_or_else(|| crate::engine::phonetic::dmetaphone_primary(word))
                }
                _ => continue,
              };
              if code.is_empty() {
                continue;
              }
              let scalar = index.converter.to_scalar(code.as_bytes());
              let entries = index.lookup_by_scalar(scalar);
              for entry in entries {
                budget.record_work(1)?;
                candidates.insert(entry.file_hash.clone());
              }
            }
            all_values.extend(index.values.drain());
          }
        }

        // Try exact match via string index
        if let Some(mut index) =
          self.load_index_by_strategy_for_query(path, index_field_name, "string", index_manager, is_virtual_field, budget)?
        {
          budget.reserve_hash_work(index.entries.len(), self.engine.hash_algo().hash_length(), true)?;
          budget.reserve_field_values(&index.values)?;
          let entries = index.lookup_exact(query_str.as_bytes());
          for entry in entries {
            budget.record_work(1)?;
            candidates.insert(entry.file_hash.clone());
          }
          all_values.extend(index.values.drain());
        }
        if is_virtual_field {
          candidates = self.filter_hashes_to_query_path(candidates, path, budget)?;
        }

        Ok((candidates, all_values))
      }
      _ => Err(EngineError::NotFound("Not a fuzzy operation".to_string())),
    }
  }

  /// Load a file's FileRecord and raw data from its hash.
  /// Used as a fallback for native JSON files whose values are not in the index.
  fn load_file_with_data(
    &self,
    file_hash: &[u8],
    hash_length: usize,
    ops: &DirectoryOps,
    budget: &QueryMemoryBudget,
  ) -> EngineResult<Option<(FileRecord, Vec<u8>, QueryTemporaryMemoryLease)>> {
    let Some(preflight_header) = self.engine.get_entry_header(file_hash)? else {
      return Ok(None);
    };
    match self.engine.get_entry(file_hash) {
      Ok(Some((header, _key, value))) => {
        let file_record = FileRecord::deserialize(&value, hash_length, header.entry_version)?;
        let temporary_bytes = QueryMemoryBudget::buffered_json_amplification_bytes(file_record.total_size, preflight_header.value_length)?;
        let memory = budget.reserve_temporary(temporary_bytes, "query buffered JSON admission failed")?;

        match ops.read_file_buffered(&file_record.path) {
          Ok(data) => Ok(Some((file_record, data, memory))),
          Err(EngineError::NotFound(_)) => Ok(None), // file may have been deleted
          Err(e) => Err(e),
        }
      }
      Ok(None) => Ok(None),
      Err(e) => Err(e),
    }
  }

  /// Extract a field's string value from JSON file data.
  fn extract_field_value(&self, file_data: &[u8], field_name: &str) -> Option<String> {
    let fields = parse_json_fields(file_data, &[field_name]).ok()?;
    for (name, value) in fields {
      if name == field_name {
        return Some(String::from_utf8_lossy(&value).to_string());
      }
    }
    None
  }

  /// Compute a fuzzy score for a field value given the query operation.
  /// Returns (score, strategy_name). Score of 0.0 means no match.
  fn compute_score(&self, op: &QueryOp, field_value: &str, budget: &mut QueryMemoryBudget) -> EngineResult<(f64, String)> {
    budget.check_cancellation()?;
    match op {
      QueryOp::Contains(query_str) => {
        let query_lower = query_str.to_lowercase();
        let value_lower = field_value.to_lowercase();
        if value_lower.contains(&query_lower) {
          Ok((1.0, "trigram".to_string()))
        } else {
          Ok((0.0, "trigram".to_string()))
        }
      }
      QueryOp::Similar(query_str, threshold) => {
        let score = crate::engine::fuzzy::trigram_similarity(query_str, field_value);
        if score >= *threshold {
          Ok((score, "trigram".to_string()))
        } else {
          Ok((0.0, "trigram".to_string()))
        }
      }
      QueryOp::Phonetic(query_str) => {
        // Tokenize both query and field value — match if ANY word pair shares a code
        let q_words: Vec<&str> = query_str.split_whitespace().filter(|w| w.chars().any(|c| c.is_alphabetic())).collect();
        let v_words: Vec<&str> = field_value.split_whitespace().filter(|w| w.chars().any(|c| c.is_alphabetic())).collect();

        let mut strategies = Vec::new();

        for qw in &q_words {
          for vw in &v_words {
            budget.record_work(1)?;
            let q_soundex = crate::engine::phonetic::soundex(qw);
            let v_soundex = crate::engine::phonetic::soundex(vw);
            if !q_soundex.is_empty() && q_soundex == v_soundex && !strategies.contains(&"soundex".to_string()) {
              strategies.push("soundex".to_string());
            }

            let q_dm = crate::engine::phonetic::dmetaphone_primary(qw);
            let v_dm = crate::engine::phonetic::dmetaphone_primary(vw);
            let v_dm_alt = crate::engine::phonetic::dmetaphone_alt(vw);
            if !q_dm.is_empty() && (q_dm == v_dm || Some(&q_dm) == v_dm_alt.as_ref()) && !strategies.contains(&"dmetaphone".to_string()) {
              strategies.push("dmetaphone".to_string());
            }

            let q_dm_alt = crate::engine::phonetic::dmetaphone_alt(qw);
            if let Some(ref q_alt) = q_dm_alt {
              if !q_alt.is_empty()
                && (q_alt == &v_dm || Some(q_alt) == v_dm_alt.as_ref())
                && !strategies.contains(&"dmetaphone_alt".to_string())
              {
                strategies.push("dmetaphone_alt".to_string());
              }
            }
          }
        }

        if !strategies.is_empty() {
          Ok((1.0, strategies.join(",")))
        } else {
          Ok((0.0, String::new()))
        }
      }
      QueryOp::Fuzzy(query_str, options) => match options.algorithm {
        FuzzyAlgorithm::DamerauLevenshtein => {
          let distance = crate::engine::fuzzy::damerau_levenshtein_controlled(query_str, field_value, || budget.record_work(1))?;
          let max_edits = match options.fuzziness {
            Fuzziness::Auto => crate::engine::fuzzy::auto_fuzziness(query_str.len()),
            Fuzziness::Fixed(n) => n,
          };
          if distance <= max_edits {
            let max_len = query_str.len().max(field_value.len()).max(1);
            let score = 1.0 - (distance as f64 / max_len as f64);
            Ok((score, "trigram".to_string()))
          } else {
            Ok((0.0, "trigram".to_string()))
          }
        }
        FuzzyAlgorithm::JaroWinkler => {
          let score = crate::engine::fuzzy::jaro_winkler(query_str, field_value);
          let threshold = match options.fuzziness {
            Fuzziness::Auto => 0.8,
            Fuzziness::Fixed(n) => 1.0 - (n as f64 / query_str.len().max(1) as f64),
          };
          if score >= threshold {
            Ok((score, "trigram".to_string()))
          } else {
            Ok((0.0, "trigram".to_string()))
          }
        }
      },
      QueryOp::Match(query_str) => {
        let mut max_score = 0.0f64;
        let mut strategies = Vec::new();

        // Exact match
        if query_str.to_lowercase() == field_value.to_lowercase() {
          max_score = 1.0;
          strategies.push("exact".to_string());
        }

        // Trigram similarity
        let trig_score = crate::engine::fuzzy::trigram_similarity(query_str, field_value);
        if trig_score > 0.3 {
          if trig_score > max_score {
            max_score = trig_score;
          }
          strategies.push("trigram".to_string());
        }

        // Phonetic matching (tokenize both sides)
        let q_words: Vec<&str> = query_str.split_whitespace().filter(|w| w.chars().any(|c| c.is_alphabetic())).collect();
        let v_words: Vec<&str> = field_value.split_whitespace().filter(|w| w.chars().any(|c| c.is_alphabetic())).collect();

        'soundex_check: for qw in &q_words {
          for vw in &v_words {
            budget.record_work(1)?;
            let qs = crate::engine::phonetic::soundex(qw);
            let vs = crate::engine::phonetic::soundex(vw);
            if !qs.is_empty() && qs == vs {
              if 1.0 > max_score {
                max_score = 1.0;
              }
              strategies.push("soundex".to_string());
              break 'soundex_check;
            }
          }
        }

        'dm_check: for qw in &q_words {
          for vw in &v_words {
            budget.record_work(1)?;
            let qd = crate::engine::phonetic::dmetaphone_primary(qw);
            let vd = crate::engine::phonetic::dmetaphone_primary(vw);
            let vda = crate::engine::phonetic::dmetaphone_alt(vw);
            if !qd.is_empty() && (qd == vd || Some(&qd) == vda.as_ref()) {
              if 1.0 > max_score {
                max_score = 1.0;
              }
              strategies.push("dmetaphone".to_string());
              break 'dm_check;
            }
          }
        }

        // Edit distance
        let distance = crate::engine::fuzzy::damerau_levenshtein_controlled(query_str, field_value, || budget.record_work(1))?;
        let max_edits = crate::engine::fuzzy::auto_fuzziness(query_str.len());
        if distance <= max_edits {
          let max_len = query_str.len().max(field_value.len()).max(1);
          let dl_score = 1.0 - (distance as f64 / max_len as f64);
          if dl_score > max_score {
            max_score = dl_score;
          }
          strategies.push("fuzzy".to_string());
        }

        if max_score > 0.0 {
          Ok((max_score, strategies.join(",")))
        } else {
          Ok((0.0, String::new()))
        }
      }
      _ => Ok((1.0, String::new())), // Non-fuzzy ops always score 1.0
    }
  }

  /// Execute an aggregation query, computing statistics (count, sum, avg, min, max)
  /// over the matching result set, optionally grouped by one or more fields.
  pub fn execute_aggregate(&self, query: &Query) -> EngineResult<AggregateResult> {
    self.execute_aggregate_with_optional_filter(query, None, &mut include_all_query_results)
  }

  /// Execute aggregation after applying an authority-owned result filter.
  /// Counts, groups, and numeric aggregates are computed only from retained
  /// rows.
  pub fn execute_aggregate_filtered<F>(&self, query: &Query, mut filter: F) -> EngineResult<AggregateResult>
  where
    F: FnMut(&QueryResult) -> EngineResult<bool>,
  {
    self.execute_aggregate_with_optional_filter(query, None, &mut filter)
  }

  pub fn execute_aggregate_with_cancellation(&self, query: &Query, cancellation: &CancellationToken) -> EngineResult<AggregateResult> {
    self.execute_aggregate_with_optional_filter(query, Some(cancellation), &mut include_all_query_results)
  }

  fn execute_aggregate_with_optional_filter(
    &self,
    query: &Query,
    cancellation: Option<&CancellationToken>,
    filter: &mut QueryResultFilter<'_>,
  ) -> EngineResult<AggregateResult> {
    let _operation = self.engine.query_operation_guard()?;
    let agg = query.aggregate.as_ref().ok_or_else(|| EngineError::NotFound("No aggregate query specified".to_string()))?;
    let mut budget = QueryMemoryBudget::new_with_cancellation(self.engine, cancellation, self.request_budget()?)?;

    // Run the filter to get matching file hashes
    let mut result_hashes = self.execute_internal(query, &mut budget)?;
    retain_query_results(&mut result_hashes, filter)?;
    budget.reserve_hash_work(result_hashes.len(), self.engine.hash_algo().hash_length(), false)?;
    let result_hash_set: HashSet<Vec<u8>> = result_hashes.iter().map(|r| r.file_hash.clone()).collect();

    let index_manager = IndexManager::new(self.engine);
    let effective_limit = query.limit.unwrap_or(DEFAULT_QUERY_LIMIT);
    let explicit_limit = query.limit.is_some();

    // COUNT
    let count = if agg.count { Some(result_hash_set.len() as u64) } else { None };

    // Collect all aggregate field names
    let mut agg_fields: HashSet<&str> = HashSet::new();
    for f in &agg.sum {
      budget.record_work(1)?;
      agg_fields.insert(f);
    }
    for f in &agg.avg {
      budget.record_work(1)?;
      agg_fields.insert(f);
    }
    for f in &agg.min {
      budget.record_work(1)?;
      agg_fields.insert(f);
    }
    for f in &agg.max {
      budget.record_work(1)?;
      agg_fields.insert(f);
    }

    // Load indexes for aggregate fields
    let mut field_indexes: FieldIndexMap = HashMap::new();
    for field_name in &agg_fields {
      budget.record_work(1)?;
      let indexes = index_manager.load_indexes_for_field_with_memory_account(&query.path, field_name, &mut budget)?;
      let index =
        indexes.into_iter().next().ok_or_else(|| EngineError::NotFound(format!("No index found for aggregate field '{}'", field_name)))?;
      let type_tag = index.converter.type_tag();
      field_indexes.insert(field_name.to_string(), (index.values, type_tag));
    }

    // Validate SUM/AVG fields are numeric
    for field_name in &agg.sum {
      budget.record_work(1)?;
      if let Some((_, type_tag)) = field_indexes.get(field_name.as_str()) {
        if !is_numeric_type(*type_tag) {
          return Err(EngineError::NotFound(format!("Cannot compute SUM on field '{}' -- requires numeric index type", field_name)));
        }
      }
    }
    for field_name in &agg.avg {
      budget.record_work(1)?;
      if let Some((_, type_tag)) = field_indexes.get(field_name.as_str()) {
        if !is_numeric_type(*type_tag) {
          return Err(EngineError::NotFound(format!("Cannot compute AVG on field '{}' -- requires numeric index type", field_name)));
        }
      }
    }

    // If no GROUP BY, compute flat aggregates
    if agg.group_by.is_empty() {
      let ComputedAggregates { sum, avg, min, max } = compute_aggregates(&result_hash_set, agg, &field_indexes, &mut budget)?;
      let mut result = AggregateResult::new(count, sum, avg, min, max, None, false, false);
      drop(field_indexes);
      drop(result_hash_set);
      drop(result_hashes);
      budget.retain_aggregate(&mut result)?;
      return Ok(result);
    }

    // GROUP BY: load group field indexes
    let mut group_field_data: GroupFieldEntries = Vec::new();
    for gf in &agg.group_by {
      budget.record_work(1)?;
      let indexes = index_manager.load_indexes_for_field_with_memory_account(&query.path, gf, &mut budget)?;
      let index = indexes.into_iter().next().ok_or_else(|| EngineError::NotFound(format!("No index found for group_by field '{}'", gf)))?;
      let type_tag = index.converter.type_tag();
      group_field_data.push((gf.clone(), index.values, type_tag));
    }

    // Bucket results by group key
    budget.reserve_aggregate_groups(result_hash_set.len(), agg.group_by.len(), self.engine.hash_algo().hash_length())?;
    let mut groups: GroupBuckets = HashMap::new();

    for file_hash in &result_hash_set {
      budget.record_work(1)?;
      // Build group key from all group_by fields
      let mut key_map = HashMap::new();
      let mut key_parts: Vec<String> = Vec::new();

      for (field_name, values, type_tag) in &group_field_data {
        budget.record_work(1)?;
        let value = values.get(file_hash.as_slice()).map(|bytes| bytes_to_json_value(bytes, *type_tag)).unwrap_or(serde_json::Value::Null);
        key_parts.push(format!("{}={}", field_name, value));
        key_map.insert(field_name.clone(), value);
      }

      let group_key = key_parts.join("|");
      groups.entry(group_key).or_insert_with(|| (key_map, Vec::new())).1.push(file_hash.clone());
    }

    // Compute aggregates per group
    let aggregate_field_count = agg.sum.len().saturating_add(agg.avg.len()).saturating_add(agg.min.len()).saturating_add(agg.max.len());
    budget.reserve_group_results(groups.len(), aggregate_field_count, agg.group_by.len())?;
    let mut group_results: Vec<GroupResult> = Vec::with_capacity(groups.len());

    for (key_map, group_hashes) in groups.values() {
      budget.record_work(1)?;
      budget.reserve_hash_work(group_hashes.len(), self.engine.hash_algo().hash_length(), false)?;
      let group_hash_set: HashSet<Vec<u8>> = group_hashes.iter().cloned().collect();
      let ComputedAggregates { sum, avg, min, max } = compute_aggregates(&group_hash_set, agg, &field_indexes, &mut budget)?;

      group_results.push(GroupResult { key: key_map.clone(), count: group_hashes.len() as u64, sum, avg, min, max });
    }

    // Sort groups by count descending (most populated first)
    budget.reserve_stable_sort::<GroupResult>(group_results.len())?;
    group_results.sort_by(|a, b| b.count.cmp(&a.count));
    budget.check_cancellation()?;

    // Apply limit to groups
    let has_more = group_results.len() > effective_limit;
    group_results.truncate(effective_limit);
    let default_limit_hit = !explicit_limit && has_more;

    let mut result = AggregateResult::new(
      count,
      HashMap::new(),
      HashMap::new(),
      HashMap::new(),
      HashMap::new(),
      Some(group_results),
      has_more,
      default_limit_hit,
    );
    drop(groups);
    drop(group_field_data);
    drop(field_indexes);
    drop(result_hash_set);
    drop(result_hashes);
    budget.retain_aggregate(&mut result)?;
    Ok(result)
  }
}

// ---------------------------------------------------------------------------
// Aggregation helpers
// ---------------------------------------------------------------------------

/// Parse raw value bytes into a numeric f64, using the converter type to determine format.
pub fn bytes_to_f64(bytes: &[u8], type_tag: u8) -> Option<f64> {
  match type_tag {
    CONVERTER_TYPE_U8 => {
      if !bytes.is_empty() {
        Some(bytes[0] as f64)
      } else {
        None
      }
    }
    CONVERTER_TYPE_U16 => {
      if bytes.len() >= 2 {
        Some(u16::from_be_bytes([bytes[0], bytes[1]]) as f64)
      } else {
        None
      }
    }
    CONVERTER_TYPE_U32 => {
      if bytes.len() >= 4 {
        Some(u32::from_be_bytes(bytes[..4].try_into().ok()?) as f64)
      } else {
        None
      }
    }
    CONVERTER_TYPE_U64 => {
      if bytes.len() >= 8 {
        Some(u64::from_be_bytes(bytes[..8].try_into().ok()?) as f64)
      } else {
        None
      }
    }
    CONVERTER_TYPE_I64 | CONVERTER_TYPE_TIMESTAMP => {
      if bytes.len() >= 8 {
        Some(i64::from_be_bytes(bytes[..8].try_into().ok()?) as f64)
      } else {
        None
      }
    }
    CONVERTER_TYPE_F64 => {
      if bytes.len() >= 8 {
        Some(f64::from_be_bytes(bytes[..8].try_into().ok()?))
      } else {
        None
      }
    }
    _ => None,
  }
}

/// Parse raw value bytes into a JSON value for display (MIN/MAX, GROUP BY keys).
pub fn bytes_to_json_value(bytes: &[u8], type_tag: u8) -> serde_json::Value {
  match type_tag {
    CONVERTER_TYPE_U8 => {
      if !bytes.is_empty() {
        serde_json::json!(bytes[0])
      } else {
        serde_json::Value::Null
      }
    }
    CONVERTER_TYPE_U16 => {
      if bytes.len() >= 2 {
        serde_json::json!(u16::from_be_bytes([bytes[0], bytes[1]]))
      } else {
        serde_json::Value::Null
      }
    }
    CONVERTER_TYPE_U32 => {
      if bytes.len() >= 4 {
        serde_json::json!(u32::from_be_bytes(bytes[..4].try_into().unwrap()))
      } else {
        serde_json::Value::Null
      }
    }
    CONVERTER_TYPE_U64 => {
      if bytes.len() >= 8 {
        serde_json::json!(u64::from_be_bytes(bytes[..8].try_into().unwrap()))
      } else {
        serde_json::Value::Null
      }
    }
    CONVERTER_TYPE_I64 | CONVERTER_TYPE_TIMESTAMP => {
      if bytes.len() >= 8 {
        serde_json::json!(i64::from_be_bytes(bytes[..8].try_into().unwrap()))
      } else {
        serde_json::Value::Null
      }
    }
    CONVERTER_TYPE_F64 => {
      if bytes.len() >= 8 {
        serde_json::json!(f64::from_be_bytes(bytes[..8].try_into().unwrap()))
      } else {
        serde_json::Value::Null
      }
    }
    CONVERTER_TYPE_STRING => {
      serde_json::json!(String::from_utf8_lossy(bytes).to_string())
    }
    _ => {
      // Unknown type -- try as UTF-8 string, fall back to hex
      if let Ok(s) = std::str::from_utf8(bytes) {
        serde_json::json!(s)
      } else {
        serde_json::json!(hex::encode(bytes))
      }
    }
  }
}

/// Check if a converter type supports numeric aggregation (SUM/AVG).
pub fn is_numeric_type(type_tag: u8) -> bool {
  matches!(
    type_tag,
    CONVERTER_TYPE_U8 | CONVERTER_TYPE_U16 | CONVERTER_TYPE_U32 | CONVERTER_TYPE_U64 | CONVERTER_TYPE_I64 | CONVERTER_TYPE_F64
  )
}

/// Shared aggregation computation: iterates the hash set, computes SUM, AVG, MIN, MAX.
fn compute_aggregates(
  hash_set: &HashSet<Vec<u8>>,
  agg: &AggregateQuery,
  field_indexes: &FieldIndexMap,
  budget: &mut QueryMemoryBudget,
) -> EngineResult<ComputedAggregates> {
  let mut sum_map: HashMap<String, f64> = HashMap::new();
  let mut avg_counts: HashMap<String, (f64, u64)> = HashMap::new();
  let mut min_map: HashMap<String, (serde_json::Value, Vec<u8>)> = HashMap::new();
  let mut max_map: HashMap<String, (serde_json::Value, Vec<u8>)> = HashMap::new();

  for file_hash in hash_set {
    budget.record_work(1)?;
    // SUM
    for field_name in &agg.sum {
      budget.record_work(1)?;
      if let Some((values, type_tag)) = field_indexes.get(field_name.as_str()) {
        if let Some(bytes) = values.get(file_hash.as_slice()) {
          if let Some(num) = bytes_to_f64(bytes, *type_tag) {
            *sum_map.entry(field_name.clone()).or_insert(0.0) += num;
          }
        }
      }
    }

    // AVG (accumulate sum + count)
    for field_name in &agg.avg {
      budget.record_work(1)?;
      if let Some((values, type_tag)) = field_indexes.get(field_name.as_str()) {
        if let Some(bytes) = values.get(file_hash.as_slice()) {
          if let Some(num) = bytes_to_f64(bytes, *type_tag) {
            let entry = avg_counts.entry(field_name.clone()).or_insert((0.0, 0));
            entry.0 += num;
            entry.1 += 1;
          }
        }
      }
    }

    // MIN
    for field_name in &agg.min {
      budget.record_work(1)?;
      if let Some((values, type_tag)) = field_indexes.get(field_name.as_str()) {
        if let Some(bytes) = values.get(file_hash.as_slice()) {
          let should_replace = match min_map.get(field_name.as_str()) {
            None => true,
            Some((_, current_bytes)) => bytes.as_slice() < current_bytes.as_slice(),
          };
          if should_replace {
            min_map.insert(field_name.clone(), (bytes_to_json_value(bytes, *type_tag), bytes.clone()));
          }
        }
      }
    }

    // MAX
    for field_name in &agg.max {
      budget.record_work(1)?;
      if let Some((values, type_tag)) = field_indexes.get(field_name.as_str()) {
        if let Some(bytes) = values.get(file_hash.as_slice()) {
          let should_replace = match max_map.get(field_name.as_str()) {
            None => true,
            Some((_, current_bytes)) => bytes.as_slice() > current_bytes.as_slice(),
          };
          if should_replace {
            max_map.insert(field_name.clone(), (bytes_to_json_value(bytes, *type_tag), bytes.clone()));
          }
        }
      }
    }
  }

  let avg_map: HashMap<String, f64> =
    avg_counts.into_iter().map(|(k, (sum, count))| (k, if count > 0 { sum / count as f64 } else { 0.0 })).collect();

  let min_display: HashMap<String, serde_json::Value> = min_map.into_iter().map(|(k, (v, _))| (k, v)).collect();

  let max_display: HashMap<String, serde_json::Value> = max_map.into_iter().map(|(k, (v, _))| (k, v)).collect();

  Ok(ComputedAggregates { sum: sum_map, avg: avg_map, min: min_display, max: max_display })
}

/// Chainable query builder.
pub struct QueryBuilder<'a> {
  engine: &'a StorageEngine,
  path: String,
  nodes: Vec<QueryNode>,
  limit_value: Option<usize>,
  offset_value: Option<usize>,
  order_by_fields: Vec<SortField>,
  after_value: Option<String>,
  before_value: Option<String>,
  include_total_value: bool,
  strategy_value: QueryStrategy,
  cancellation: Option<CancellationToken>,
}

impl<'a> QueryBuilder<'a> {
  pub fn new(engine: &'a StorageEngine, path: &str) -> Self {
    QueryBuilder {
      engine,
      path: path.to_string(),
      nodes: Vec::new(),
      limit_value: None,
      offset_value: None,
      order_by_fields: Vec::new(),
      after_value: None,
      before_value: None,
      include_total_value: false,
      strategy_value: QueryStrategy::Full,
      cancellation: None,
    }
  }

  /// Start building a field query.
  pub fn field(self, name: &str) -> FieldQueryBuilder<'a> {
    FieldQueryBuilder { parent: self, field_name: name.to_string() }
  }

  /// Set a result limit.
  pub fn limit(mut self, count: usize) -> Self {
    self.limit_value = Some(count);
    self
  }

  /// Set the query execution strategy.
  pub fn strategy(mut self, strategy: QueryStrategy) -> Self {
    self.strategy_value = strategy;
    self
  }

  /// Add a sort field.
  pub fn order_by(mut self, field: &str, direction: SortDirection) -> Self {
    self.order_by_fields.push(SortField { field: field.to_string(), direction });
    self
  }

  /// Set an offset (skip N results).
  pub fn offset(mut self, offset: usize) -> Self {
    self.offset_value = Some(offset);
    self
  }

  /// Set an "after" cursor for cursor-based pagination.
  pub fn after(mut self, cursor: &str) -> Self {
    self.after_value = Some(cursor.to_string());
    self
  }

  /// Set a "before" cursor for cursor-based pagination.
  pub fn before(mut self, cursor: &str) -> Self {
    self.before_value = Some(cursor.to_string());
    self
  }

  /// Include total count in paginated results.
  pub fn include_total(mut self) -> Self {
    self.include_total_value = true;
    self
  }

  /// Cooperatively cancel this query between bounded units of query work.
  pub fn cancellation_token(mut self, cancellation: CancellationToken) -> Self {
    self.cancellation = Some(cancellation);
    self
  }

  /// Add an explicit AND group via a sub-builder closure.
  pub fn and<F>(mut self, build_fn: F) -> Self
  where
    F: FnOnce(QueryBuilder<'a>) -> QueryBuilder<'a>,
  {
    let mut sub = QueryBuilder::new(self.engine, &self.path);
    sub.cancellation = self.cancellation.clone();
    let built = build_fn(sub);
    if !built.nodes.is_empty() {
      self.nodes.push(QueryNode::And(built.nodes));
    }
    self
  }

  /// Add an OR group via a sub-builder closure.
  pub fn or<F>(mut self, build_fn: F) -> Self
  where
    F: FnOnce(QueryBuilder<'a>) -> QueryBuilder<'a>,
  {
    let mut sub = QueryBuilder::new(self.engine, &self.path);
    sub.cancellation = self.cancellation.clone();
    let built = build_fn(sub);
    if !built.nodes.is_empty() {
      self.nodes.push(QueryNode::Or(built.nodes));
    }
    self
  }

  /// Add a NOT group via a sub-builder closure.
  pub fn not<F>(mut self, build_fn: F) -> Self
  where
    F: FnOnce(QueryBuilder<'a>) -> QueryBuilder<'a>,
  {
    let mut sub = QueryBuilder::new(self.engine, &self.path);
    sub.cancellation = self.cancellation.clone();
    let built = build_fn(sub);
    if !built.nodes.is_empty() {
      let inner = if built.nodes.len() == 1 { built.nodes.into_iter().next().unwrap() } else { QueryNode::And(built.nodes) };
      self.nodes.push(QueryNode::Not(Box::new(inner)));
    }
    self
  }

  /// Build the QueryNode tree from the accumulated nodes.
  fn build_node(&self) -> Option<QueryNode> {
    if self.nodes.is_empty() {
      return None;
    }
    if self.nodes.len() == 1 {
      return Some(self.nodes[0].clone());
    }
    Some(QueryNode::And(self.nodes.clone()))
  }

  /// Build the Query struct from the builder state.
  fn build_query(&self) -> Query {
    Query {
      path: self.path.clone(),
      field_queries: Vec::new(),
      node: self.build_node(),
      limit: self.limit_value,
      offset: self.offset_value,
      order_by: self.order_by_fields.clone(),
      after: self.after_value.clone(),
      before: self.before_value.clone(),
      include_total: self.include_total_value,
      strategy: self.strategy_value.clone(),
      aggregate: None,
      explain: ExplainMode::Off,
    }
  }

  /// Execute and return all matching results.
  pub fn all(&self) -> EngineResult<Vec<QueryResult>> {
    let query = self.build_query();
    let query_engine = QueryEngine::new(self.engine);
    match self.cancellation.as_ref() {
      Some(cancellation) => query_engine.execute_with_cancellation(&query, cancellation),
      None => query_engine.execute(&query),
    }
  }

  /// Execute and return the first matching result.
  pub fn first(&self) -> EngineResult<Option<QueryResult>> {
    let mut query = self.build_query();
    query.limit = Some(1);
    let query_engine = QueryEngine::new(self.engine);
    let mut results = match self.cancellation.as_ref() {
      Some(cancellation) => query_engine.execute_with_cancellation(&query, cancellation)?,
      None => query_engine.execute(&query)?,
    };
    Ok(results.pop())
  }

  /// Execute and return only the count of matching results.
  pub fn count(&self) -> EngineResult<usize> {
    let query = self.build_query();
    let query_engine = QueryEngine::new(self.engine);
    let results = match self.cancellation.as_ref() {
      Some(cancellation) => query_engine.execute_with_cancellation(&query, cancellation)?,
      None => query_engine.execute(&query)?,
    };
    Ok(results.len())
  }

  /// Execute with pagination support and return a PaginatedResult.
  pub fn execute_paginated(&self) -> EngineResult<PaginatedResult> {
    let query = self.build_query();
    let query_engine = QueryEngine::new(self.engine);
    match self.cancellation.as_ref() {
      Some(cancellation) => query_engine.execute_paginated_with_cancellation(&query, cancellation),
      None => query_engine.execute_paginated(&query),
    }
  }
}

/// Builder for a single field's query operation.
pub struct FieldQueryBuilder<'a> {
  parent: QueryBuilder<'a>,
  field_name: String,
}

impl<'a> FieldQueryBuilder<'a> {
  /// Exact match (raw bytes).
  pub fn eq(mut self, value: &[u8]) -> QueryBuilder<'a> {
    self.parent.nodes.push(QueryNode::Field(FieldQuery { field_name: self.field_name, operation: QueryOp::Eq(value.to_vec()) }));
    self.parent
  }

  /// Greater than (raw bytes).
  pub fn gt(mut self, value: &[u8]) -> QueryBuilder<'a> {
    self.parent.nodes.push(QueryNode::Field(FieldQuery { field_name: self.field_name, operation: QueryOp::Gt(value.to_vec()) }));
    self.parent
  }

  /// Less than (raw bytes).
  pub fn lt(mut self, value: &[u8]) -> QueryBuilder<'a> {
    self.parent.nodes.push(QueryNode::Field(FieldQuery { field_name: self.field_name, operation: QueryOp::Lt(value.to_vec()) }));
    self.parent
  }

  /// Range: between min and max (inclusive, raw bytes).
  pub fn between(mut self, min: &[u8], max: &[u8]) -> QueryBuilder<'a> {
    self
      .parent
      .nodes
      .push(QueryNode::Field(FieldQuery { field_name: self.field_name, operation: QueryOp::Between(min.to_vec(), max.to_vec()) }));
    self.parent
  }

  /// Match any of the given values (raw bytes).
  pub fn in_values(mut self, values: Vec<Vec<u8>>) -> QueryBuilder<'a> {
    self.parent.nodes.push(QueryNode::Field(FieldQuery { field_name: self.field_name, operation: QueryOp::In(values) }));
    self.parent
  }

  // --- Typed convenience methods ---

  /// Exact match on u64.
  pub fn eq_u64(self, value: u64) -> QueryBuilder<'a> {
    self.eq(&value.to_be_bytes())
  }

  /// Greater than u64.
  pub fn gt_u64(self, value: u64) -> QueryBuilder<'a> {
    self.gt(&value.to_be_bytes())
  }

  /// Less than u64.
  pub fn lt_u64(self, value: u64) -> QueryBuilder<'a> {
    self.lt(&value.to_be_bytes())
  }

  /// Exact match on i64.
  pub fn eq_i64(self, value: i64) -> QueryBuilder<'a> {
    self.eq(&value.to_be_bytes())
  }

  /// Greater than i64.
  pub fn gt_i64(self, value: i64) -> QueryBuilder<'a> {
    self.gt(&value.to_be_bytes())
  }

  /// Less than i64.
  pub fn lt_i64(self, value: i64) -> QueryBuilder<'a> {
    self.lt(&value.to_be_bytes())
  }

  /// Exact match on f64.
  pub fn eq_f64(self, value: f64) -> QueryBuilder<'a> {
    self.eq(&value.to_be_bytes())
  }

  /// Greater than f64.
  pub fn gt_f64(self, value: f64) -> QueryBuilder<'a> {
    self.gt(&value.to_be_bytes())
  }

  /// Less than f64.
  pub fn lt_f64(self, value: f64) -> QueryBuilder<'a> {
    self.lt(&value.to_be_bytes())
  }

  /// Exact match on string.
  pub fn eq_str(self, value: &str) -> QueryBuilder<'a> {
    self.eq(value.as_bytes())
  }

  /// Greater than string.
  pub fn gt_str(self, value: &str) -> QueryBuilder<'a> {
    self.gt(value.as_bytes())
  }

  /// Less than string.
  pub fn lt_str(self, value: &str) -> QueryBuilder<'a> {
    self.lt(value.as_bytes())
  }

  /// Exact match on bool.
  pub fn eq_bool(self, value: bool) -> QueryBuilder<'a> {
    self.eq(&[if value { 1 } else { 0 }])
  }

  /// Range: between min and max u64 (inclusive).
  pub fn between_u64(self, min: u64, max: u64) -> QueryBuilder<'a> {
    self.between(&min.to_be_bytes(), &max.to_be_bytes())
  }

  /// Range: between min and max string (inclusive).
  pub fn between_str(self, min: &str, max: &str) -> QueryBuilder<'a> {
    self.between(min.as_bytes(), max.as_bytes())
  }

  /// Match any of the given u64 values.
  pub fn in_u64(self, values: &[u64]) -> QueryBuilder<'a> {
    let byte_values = values.iter().map(|v| v.to_be_bytes().to_vec()).collect();
    self.in_values(byte_values)
  }

  /// Match any of the given string values.
  pub fn in_str(self, values: &[&str]) -> QueryBuilder<'a> {
    let byte_values = values.iter().map(|v| v.as_bytes().to_vec()).collect();
    self.in_values(byte_values)
  }

  // --- Fuzzy search methods ---

  /// Substring match via trigram index + recheck.
  pub fn contains(mut self, value: &str) -> QueryBuilder<'a> {
    self.parent.nodes.push(QueryNode::Field(FieldQuery { field_name: self.field_name, operation: QueryOp::Contains(value.to_string()) }));
    self.parent
  }

  /// Trigram similarity match with threshold.
  pub fn similar(mut self, value: &str, threshold: f64) -> QueryBuilder<'a> {
    self
      .parent
      .nodes
      .push(QueryNode::Field(FieldQuery { field_name: self.field_name, operation: QueryOp::Similar(value.to_string(), threshold) }));
    self.parent
  }

  /// Phonetic code match (soundex / double metaphone).
  pub fn phonetic(mut self, value: &str) -> QueryBuilder<'a> {
    self.parent.nodes.push(QueryNode::Field(FieldQuery { field_name: self.field_name, operation: QueryOp::Phonetic(value.to_string()) }));
    self.parent
  }

  /// Fuzzy match with edit distance (Damerau-Levenshtein, auto fuzziness).
  pub fn fuzzy(mut self, value: &str) -> QueryBuilder<'a> {
    self.parent.nodes.push(QueryNode::Field(FieldQuery {
      field_name: self.field_name,
      operation: QueryOp::Fuzzy(value.to_string(), FuzzyOptions::default()),
    }));
    self.parent
  }

  /// Fuzzy match with custom options.
  pub fn fuzzy_with(mut self, value: &str, options: FuzzyOptions) -> QueryBuilder<'a> {
    self
      .parent
      .nodes
      .push(QueryNode::Field(FieldQuery { field_name: self.field_name, operation: QueryOp::Fuzzy(value.to_string(), options) }));
    self.parent
  }

  /// Composite match: run all matching indexes and score-fuse.
  pub fn match_query(mut self, value: &str) -> QueryBuilder<'a> {
    self.parent.nodes.push(QueryNode::Field(FieldQuery { field_name: self.field_name, operation: QueryOp::Match(value.to_string()) }));
    self.parent
  }
}

// ---------------------------------------------------------------------------
// Virtual field helpers
// ---------------------------------------------------------------------------

fn canonical_virtual_field_name(field_name: &str) -> Option<&'static str> {
  match field_name {
    "@path" => Some("@path"),
    "@filename" | "@file_name" => Some("@filename"),
    "@extension" => Some("@extension"),
    "@content_type" => Some("@content_type"),
    "@size" => Some("@size"),
    "@created_at" => Some("@created_at"),
    "@updated_at" => Some("@updated_at"),
    "@hash" => Some("@hash"),
    _ => None,
  }
}

fn is_recheck_operation(operation: &QueryOp) -> bool {
  matches!(operation, QueryOp::Contains(_) | QueryOp::Similar(_, _) | QueryOp::Phonetic(_) | QueryOp::Fuzzy(_, _) | QueryOp::Match(_))
}

fn virtual_index_ancestor_paths(path: &str) -> Vec<String> {
  let normalized = normalize_path(path);
  let mut ancestors = Vec::new();
  let mut current = parent_path(&normalized);

  while let Some(dir) = current {
    ancestors.push(dir.clone());
    if dir == "/" {
      break;
    }
    current = parent_path(&dir);
  }

  ancestors
}

fn path_is_under_query_path(path: &str, query_path: &str) -> bool {
  if query_path == "/" {
    return true;
  }

  let normalized_path = normalize_path(path);
  let normalized_query_path = normalize_path(query_path);
  if normalized_path == normalized_query_path {
    return true;
  }

  let prefix = format!("{}/", normalized_query_path.trim_end_matches('/'));
  normalized_path.starts_with(&prefix)
}

/// Decode a big-endian u64 from bytes (as produced by `json_value_to_bytes`).
/// Returns 0 if the byte slice is not exactly 8 bytes.
fn bytes_to_u64(bytes: &[u8]) -> u64 {
  if bytes.len() == 8 {
    u64::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]])
  } else {
    0
  }
}

/// Decode a big-endian i64 from bytes (as produced by `json_value_to_bytes`).
/// Returns 0 if the byte slice is not exactly 8 bytes.
fn bytes_to_i64(bytes: &[u8]) -> i64 {
  if bytes.len() == 8 {
    // json_value_to_bytes casts i64 as u64 before encoding to big-endian bytes,
    // so we decode as u64 and reinterpret as i64.
    u64::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]]) as i64
  } else {
    0
  }
}
