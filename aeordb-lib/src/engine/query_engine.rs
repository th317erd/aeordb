use std::collections::{HashMap, HashSet};

use base64::Engine as _;
use serde::Serialize;

use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::index_store::{FieldIndex, IndexManager};
use crate::engine::json_parser::parse_json_fields;
use crate::engine::nvt_ops::NVTMask;
use crate::engine::scalar_converter::{
    ScalarConverter, TrigramConverter,
    CONVERTER_TYPE_U8, CONVERTER_TYPE_U16, CONVERTER_TYPE_U32, CONVERTER_TYPE_U64,
    CONVERTER_TYPE_I64, CONVERTER_TYPE_F64, CONVERTER_TYPE_STRING, CONVERTER_TYPE_TIMESTAMP,
};
use crate::engine::storage_engine::StorageEngine;

/// A query operation on a field.
#[derive(Debug, Clone)]
pub enum QueryOp {
  Eq(Vec<u8>),
  Gt(Vec<u8>),
  Lt(Vec<u8>),
  Between(Vec<u8>, Vec<u8>),
  In(Vec<Vec<u8>>),
  // Fuzzy search operations
  /// Substring match via trigram AND + recheck
  Contains(String),
  /// Trigram similarity with threshold (Dice coefficient)
  Similar(String, f64),
  /// Phonetic code match (soundex / double metaphone)
  Phonetic(String),
  /// Edit distance or Jaro-Winkler fuzzy match
  Fuzzy(String, FuzzyOptions),
  /// Composite: run all matching indexes, score-fuse
  Match(String),
}

/// Options for the Fuzzy query operation.
#[derive(Debug, Clone)]
pub struct FuzzyOptions {
  pub fuzziness: Fuzziness,
  pub algorithm: FuzzyAlgorithm,
}

/// How many edits to allow.
#[derive(Debug, Clone)]
pub enum Fuzziness {
  /// Automatically determined by term length (0-2: 0, 3-5: 1, 6+: 2)
  Auto,
  /// Fixed edit distance
  Fixed(usize),
}

/// Which fuzzy matching algorithm to use.
#[derive(Debug, Clone)]
pub enum FuzzyAlgorithm {
  DamerauLevenshtein,
  JaroWinkler,
}

impl Default for FuzzyOptions {
  fn default() -> Self {
    FuzzyOptions {
      fuzziness: Fuzziness::Auto,
      algorithm: FuzzyAlgorithm::DamerauLevenshtein,
    }
  }
}

/// Sort direction for ORDER BY.
#[derive(Debug, Clone)]
pub enum SortDirection {
    Asc,
    Desc,
}

/// A single sort field in an ORDER BY clause.
#[derive(Debug, Clone)]
pub struct SortField {
    pub field: String,
    pub direction: SortDirection,
}

/// Default limit applied when no explicit limit is provided.
pub const DEFAULT_QUERY_LIMIT: usize = 20;

/// Paginated query response wrapping results with metadata.
#[derive(Debug)]
pub struct PaginatedResult {
    pub results: Vec<QueryResult>,
    pub total_count: Option<u64>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub prev_cursor: Option<String>,
    pub default_limit_hit: bool,
}

/// A query on a single field.
#[derive(Debug, Clone)]
pub struct FieldQuery {
  pub field_name: String,
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
#[derive(Debug, Clone, PartialEq)]
pub enum ExplainMode {
  Off,
  Plan,     // plan only, no execution
  Analyze,  // plan + execution + results
}

impl Default for ExplainMode {
  fn default() -> Self { ExplainMode::Off }
}

/// Result of an EXPLAIN query.
#[derive(Debug, Clone, Serialize)]
pub struct ExplainResult {
  pub plan: serde_json::Value,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub execution: Option<serde_json::Value>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub results: Option<serde_json::Value>,
}

/// A complete query: path + query node tree + optional limit + strategy.
#[derive(Debug, Clone)]
pub struct Query {
  pub path: String,
  pub field_queries: Vec<FieldQuery>,
  pub node: Option<QueryNode>,
  pub limit: Option<usize>,
  pub offset: Option<usize>,
  pub order_by: Vec<SortField>,
  pub after: Option<String>,
  pub before: Option<String>,
  pub include_total: bool,
  pub strategy: QueryStrategy,
  pub aggregate: Option<AggregateQuery>,
  pub explain: ExplainMode,
}

/// Aggregation query -- what statistics to compute over the result set.
#[derive(Debug, Clone, Default)]
pub struct AggregateQuery {
    pub count: bool,
    pub sum: Vec<String>,
    pub avg: Vec<String>,
    pub min: Vec<String>,
    pub max: Vec<String>,
    pub group_by: Vec<String>,
}

/// Result of an aggregation query.
#[derive(Debug, Clone, Serialize)]
pub struct AggregateResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub sum: HashMap<String, f64>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub avg: HashMap<String, f64>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub min: HashMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub max: HashMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<GroupResult>>,
    pub has_more: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub default_limit_hit: bool,
}

/// A single group in a GROUP BY result.
#[derive(Debug, Clone, Serialize)]
pub struct GroupResult {
    pub key: HashMap<String, serde_json::Value>,
    pub count: u64,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub sum: HashMap<String, f64>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub avg: HashMap<String, f64>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub min: HashMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub max: HashMap<String, serde_json::Value>,
}

/// A single query result.
#[derive(Debug)]
pub struct QueryResult {
  pub file_hash: Vec<u8>,
  pub file_record: FileRecord,
  pub score: f64,
  pub matched_by: Vec<String>,
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
fn field_query_to_mask(
  field_index: &mut FieldIndex,
  query: &FieldQuery,
  bucket_count: usize,
) -> EngineResult<NVTMask> {
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
      Err(EngineError::NotFound(
        "Fuzzy operations do not support NVT mask generation".to_string(),
      ))
    }
  }
}

/// Walk the QueryNode tree bottom-up, producing an NVTMask at each level.
fn evaluate_node_as_mask(
  node: &QueryNode,
  path: &str,
  index_manager: &IndexManager,
  bucket_count: usize,
) -> EngineResult<NVTMask> {
  match node {
    QueryNode::Field(field_query) => {
      let loaded = index_manager.load_index(path, &field_query.field_name)?;
      let mut index = match loaded {
        Some(index) => index,
        None => {
          return Err(EngineError::NotFound(format!(
            "Index not found for field '{}' at path '{}'",
            field_query.field_name, path,
          )));
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

fn encode_cursor(
    result: &QueryResult,
    order_by: &[SortField],
    version_hash: &[u8],
) -> String {
    let mut cursor = serde_json::Map::new();

    for sf in order_by {
        if sf.field.starts_with('@') {
            let value = match sf.field.as_str() {
                "@score" => serde_json::json!(result.score),
                "@path" => serde_json::json!(result.file_record.path),
                "@size" => serde_json::json!(result.file_record.total_size),
                "@created_at" => serde_json::json!(result.file_record.created_at),
                "@updated_at" => serde_json::json!(result.file_record.updated_at),
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
    let bytes = base64::engine::general_purpose::STANDARD.decode(cursor)
        .map_err(|e| EngineError::JsonParseError(format!("Invalid cursor: {}", e)))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| EngineError::JsonParseError(format!("Invalid cursor JSON: {}", e)))
}

/// Executes queries against the index system.
pub struct QueryEngine<'a> {
  engine: &'a StorageEngine,
}

impl<'a> QueryEngine<'a> {
  pub fn new(engine: &'a StorageEngine) -> Self {
    QueryEngine { engine }
  }

  /// Execute a query and return matching file records.
  /// Uses a two-tier approach:
  ///   Tier 1: flat AND of field queries → direct scalar lookups (HashSet intersection).
  ///   Tier 2: complex boolean logic (OR, NOT) → NVTMask bitmap compositing.
  /// Fuzzy queries (Contains, Similar, Phonetic, Fuzzy) use a separate path
  /// with index-based candidate generation followed by a recheck phase.
  pub fn execute(&self, query: &Query) -> EngineResult<Vec<QueryResult>> {
    let mut results = self.execute_internal(query)?;

    // Apply limit (use DEFAULT_QUERY_LIMIT when no explicit limit).
    let effective_limit = query.limit.unwrap_or(DEFAULT_QUERY_LIMIT);
    results.truncate(effective_limit);

    Ok(results)
  }

  /// Execute a query with pagination support.
  /// Applies default limit, sorting, cursor-based pagination, offset, and builds pagination metadata.
  pub fn execute_paginated(&self, query: &Query) -> EngineResult<PaginatedResult> {
    let explicit_limit = query.limit.is_some();
    let effective_limit = query.limit.unwrap_or(DEFAULT_QUERY_LIMIT);

    // Get all results (without limit)
    let mut all_results = self.execute_internal(query)?;

    // Sort if order_by specified
    if !query.order_by.is_empty() {
      self.sort_results(&mut all_results, &query.order_by, &query.path)?;
    }

    // Count total before pagination
    let total_count = if query.include_total {
      Some(all_results.len() as u64)
    } else {
      None
    };

    // Apply cursor-based pagination (after sorting, before offset)
    if let Some(ref cursor_str) = query.after {
      let cursor_data = decode_cursor(cursor_str)?;
      let cursor_hash = cursor_data.get("_hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| EngineError::JsonParseError("Cursor missing _hash".to_string()))?;
      let cursor_hash_bytes = hex::decode(cursor_hash)
        .map_err(|e| EngineError::JsonParseError(format!("Invalid cursor hash: {}", e)))?;

      if let Some(pos) = all_results.iter().position(|r| r.file_hash == cursor_hash_bytes) {
        all_results = all_results.into_iter().skip(pos + 1).collect();
      }
    }

    if let Some(ref cursor_str) = query.before {
      let cursor_data = decode_cursor(cursor_str)?;
      let cursor_hash = cursor_data.get("_hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| EngineError::JsonParseError("Cursor missing _hash".to_string()))?;
      let cursor_hash_bytes = hex::decode(cursor_hash)
        .map_err(|e| EngineError::JsonParseError(format!("Invalid cursor hash: {}", e)))?;

      if let Some(pos) = all_results.iter().position(|r| r.file_hash == cursor_hash_bytes) {
        all_results.truncate(pos);
      }
    }

    // Apply offset
    let offset = query.offset.unwrap_or(0);
    if offset > 0 {
      if offset < all_results.len() {
        all_results = all_results.into_iter().skip(offset).collect();
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
    let version_hash = self.engine.head_hash().unwrap_or_default();

    let next_cursor = if has_more {
      all_results.last().map(|last| {
        encode_cursor(last, &query.order_by, &version_hash)
      })
    } else {
      None
    };

    let prev_cursor = if offset > 0 || query.after.is_some() {
      all_results.first().map(|first| {
        encode_cursor(first, &query.order_by, &version_hash)
      })
    } else {
      None
    };

    Ok(PaginatedResult {
      results: all_results,
      total_count,
      has_more,
      next_cursor,
      prev_cursor,
      default_limit_hit,
    })
  }

  /// Execute an EXPLAIN query, returning plan info and optionally execution metrics + results.
  pub fn execute_explain(&self, query: &Query) -> EngineResult<ExplainResult> {
    let index_manager = IndexManager::new(self.engine);

    // Build the plan by analyzing the query structure
    let plan = self.build_plan(query, &index_manager)?;

    if query.explain == ExplainMode::Plan {
      return Ok(ExplainResult {
        plan,
        execution: None,
        results: None,
      });
    }

    // Analyze mode: execute and time it
    let start = std::time::Instant::now();

    let (results_json, candidate_count, result_count) = if query.aggregate.is_some() {
      let agg_result = self.execute_aggregate(query)?;
      let count = agg_result.count.unwrap_or(0);
      (Some(serde_json::to_value(&agg_result).unwrap_or_default()), count as usize, count as usize)
    } else {
      let paginated = self.execute_paginated(query)?;
      let total = paginated.total_count.unwrap_or(paginated.results.len() as u64);
      let returned = paginated.results.len();
      let results_value = serde_json::json!({
        "results": paginated.results.iter().map(|r| {
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

    Ok(ExplainResult {
      plan,
      execution: Some(execution),
      results: results_json,
    })
  }

  /// Build a query execution plan without running the query.
  fn build_plan(&self, query: &Query, index_manager: &IndexManager) -> EngineResult<serde_json::Value> {
    let mut plan = serde_json::Map::new();

    // Analyze the query node tree
    if let Some(ref node) = query.node {
      plan.insert("query_tree".to_string(), self.explain_node(node, &query.path, index_manager)?);
      plan.insert("bitmap_compositing".to_string(),
        serde_json::json!(should_use_bitmap_compositing(node)));
    }

    if !query.order_by.is_empty() {
      let sort_fields: Vec<serde_json::Value> = query.order_by.iter().map(|sf| {
        serde_json::json!({
          "field": sf.field,
          "direction": match sf.direction { SortDirection::Asc => "asc", SortDirection::Desc => "desc" },
        })
      }).collect();
      plan.insert("order_by".to_string(), serde_json::json!(sort_fields));
    }

    if let Some(ref agg) = query.aggregate {
      plan.insert("aggregate".to_string(), serde_json::json!({
        "count": agg.count,
        "sum": agg.sum,
        "avg": agg.avg,
        "min": agg.min,
        "max": agg.max,
        "group_by": agg.group_by,
      }));
    }

    plan.insert("limit".to_string(), serde_json::json!(query.limit.unwrap_or(DEFAULT_QUERY_LIMIT)));
    if let Some(offset) = query.offset {
      plan.insert("offset".to_string(), serde_json::json!(offset));
    }

    Ok(serde_json::Value::Object(plan))
  }

  /// Explain a single query node, showing field info, operation, and index details.
  fn explain_node(&self, node: &QueryNode, path: &str, index_manager: &IndexManager) -> EngineResult<serde_json::Value> {
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

        let indexes = index_manager.load_indexes_for_field(path, &fq.field_name)
          .unwrap_or_default();
        let index_info: Vec<serde_json::Value> = indexes.iter().map(|idx| {
          serde_json::json!({
            "strategy": idx.converter.strategy(),
            "type": idx.converter.name(),
            "entries": idx.entries.len(),
            "order_preserving": idx.converter.is_order_preserving(),
            "values_stored": idx.values.len(),
          })
        }).collect();

        let needs_recheck = matches!(&fq.operation,
          QueryOp::Contains(_) | QueryOp::Similar(_, _) |
          QueryOp::Phonetic(_) | QueryOp::Fuzzy(_, _) | QueryOp::Match(_));

        Ok(serde_json::json!({
          "type": "field",
          "field": fq.field_name,
          "operation": op_name,
          "indexes": index_info,
          "recheck": needs_recheck,
        }))
      }
      QueryNode::And(children) => {
        let child_plans: Vec<serde_json::Value> = children.iter()
          .map(|c| self.explain_node(c, path, index_manager))
          .collect::<EngineResult<Vec<_>>>()?;
        Ok(serde_json::json!({"type": "and", "children": child_plans}))
      }
      QueryNode::Or(children) => {
        let child_plans: Vec<serde_json::Value> = children.iter()
          .map(|c| self.explain_node(c, path, index_manager))
          .collect::<EngineResult<Vec<_>>>()?;
        Ok(serde_json::json!({"type": "or", "children": child_plans}))
      }
      QueryNode::Not(child) => {
        let child_plan = self.explain_node(child, path, index_manager)?;
        Ok(serde_json::json!({"type": "not", "child": child_plan}))
      }
    }
  }

  /// Internal query execution that returns all matching results without applying limit.
  /// Both `execute()` and `execute_paginated()` delegate to this.
  fn execute_internal(&self, query: &Query) -> EngineResult<Vec<QueryResult>> {
    // Determine which node tree to evaluate.
    let effective_node = if let Some(ref node) = query.node {
      node.clone()
    } else if query.field_queries.is_empty() {
      return Ok(Vec::new());
    } else {
      // Legacy path: wrap flat field_queries as an implicit AND.
      let leaves: Vec<QueryNode> = query.field_queries.iter()
        .map(|fq| QueryNode::Field(fq.clone()))
        .collect();
      if leaves.len() == 1 {
        leaves.into_iter().next().unwrap()
      } else {
        QueryNode::And(leaves)
      }
    };

    // Check for fuzzy operations — these need the recheck path.
    if self.node_has_fuzzy_ops(&effective_node) {
      return self.execute_with_recheck_internal(query, &effective_node);
    }

    let index_manager = IndexManager::new(self.engine);

    let result_hashes = if should_use_bitmap_compositing(&effective_node) {
      self.execute_tier2(&effective_node, &query.path, &index_manager)?
    } else {
      self.evaluate_node(&effective_node, &query.path, &index_manager)?
    };

    // Load FileRecords for candidates.
    let hash_length = self.engine.hash_algo().hash_length();
    let mut results = Vec::new();

    for file_hash in result_hashes {
      match self.engine.get_entry(&file_hash) {
        Ok(Some((_header, _key, value))) => {
          let file_record = FileRecord::deserialize(&value, hash_length)?;
          results.push(QueryResult { file_hash, file_record, score: 1.0, matched_by: vec![] });
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
      if sf.field.starts_with('@') {
        sort_fields.push(SortData {
          values: HashMap::new(),
          is_virtual: true,
          field: sf.field.clone(),
          direction: sf.direction.clone(),
        });
      } else {
        let indexes = index_manager.load_indexes_for_field(path, &sf.field)?;
        let index = indexes.into_iter()
          .find(|idx| idx.converter.is_order_preserving())
          .ok_or_else(|| {
            EngineError::NotFound(format!(
              "Cannot sort by field '{}' — no order-preserving index found. \
               Use a string, numeric, or timestamp index type.",
              sf.field
            ))
          })?;

        sort_fields.push(SortData {
          values: index.values,
          is_virtual: false,
          field: sf.field.clone(),
          direction: sf.direction.clone(),
        });
      }
    }

    results.sort_by(|a, b| {
      for sd in &sort_fields {
        let cmp = if sd.is_virtual {
          match sd.field.as_str() {
            "@score" => a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal),
            "@path" => a.file_record.path.cmp(&b.file_record.path),
            "@size" => a.file_record.total_size.cmp(&b.file_record.total_size),
            "@created_at" => a.file_record.created_at.cmp(&b.file_record.created_at),
            "@updated_at" => a.file_record.updated_at.cmp(&b.file_record.updated_at),
            _ => std::cmp::Ordering::Equal,
          }
        } else {
          let va = sd.values.get(&a.file_hash).cloned().unwrap_or_default();
          let vb = sd.values.get(&b.file_hash).cloned().unwrap_or_default();
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

    Ok(())
  }

  /// Tier 2: NVTMask bitmap compositing for complex queries with OR/NOT.
  /// Builds a bitmap mask via the QueryNode tree, then uses the precise
  /// set-based evaluation for final result collection. The mask is computed
  /// (and can be used for early pruning in future large-dataset optimizations),
  /// but correctness is guaranteed by the set-based verify pass.
  fn execute_tier2(
    &self,
    node: &QueryNode,
    path: &str,
    index_manager: &IndexManager,
  ) -> EngineResult<HashSet<Vec<u8>>> {
    // Build the bitmap mask for analysis / future optimization.
    let bucket_count = 1024;
    let _mask = evaluate_node_as_mask(node, path, index_manager, bucket_count)?;

    // For correctness (especially with NOT, which requires the full universe),
    // use the precise set-based evaluation.
    self.evaluate_node(node, path, index_manager)
  }

  /// Tier 1: Recursively evaluate a QueryNode tree using direct scalar lookups,
  /// returning matching file hashes.
  fn evaluate_node(
    &self,
    node: &QueryNode,
    path: &str,
    index_manager: &IndexManager,
  ) -> EngineResult<HashSet<Vec<u8>>> {
    match node {
      QueryNode::Field(field_query) => {
        self.evaluate_field_query(field_query, path, index_manager)
      }
      QueryNode::And(children) => {
        if children.is_empty() {
          return Ok(HashSet::new());
        }
        let mut result = self.evaluate_node(&children[0], path, index_manager)?;
        for child in &children[1..] {
          let child_set = self.evaluate_node(child, path, index_manager)?;
          result = result.intersection(&child_set).cloned().collect();
        }
        Ok(result)
      }
      QueryNode::Or(children) => {
        let mut result = HashSet::new();
        for child in children {
          let child_set = self.evaluate_node(child, path, index_manager)?;
          result = result.union(&child_set).cloned().collect();
        }
        Ok(result)
      }
      QueryNode::Not(child) => {
        // NOT requires knowing the universe of all file hashes.
        let child_set = self.evaluate_node(child, path, index_manager)?;
        let all_hashes = self.collect_all_hashes(path, index_manager)?;
        Ok(all_hashes.difference(&child_set).cloned().collect())
      }
    }
  }

  /// Evaluate a single FieldQuery leaf against the index.
  fn evaluate_field_query(
    &self,
    field_query: &FieldQuery,
    path: &str,
    index_manager: &IndexManager,
  ) -> EngineResult<HashSet<Vec<u8>>> {
    let index = index_manager.load_index(path, &field_query.field_name)?;
    let mut index = match index {
      Some(index) => index,
      None => {
        return Err(EngineError::NotFound(format!(
          "Index not found for field '{}' at path '{}'",
          field_query.field_name, path,
        )));
      }
    };

    let matching_entries = match &field_query.operation {
      QueryOp::Eq(value) => {
        index.lookup_exact(value)
          .into_iter()
          .map(|entry| entry.file_hash.clone())
          .collect::<HashSet<Vec<u8>>>()
      }
      QueryOp::Gt(value) => {
        index.lookup_gt(value)?
          .into_iter()
          .map(|entry| entry.file_hash.clone())
          .collect::<HashSet<Vec<u8>>>()
      }
      QueryOp::Lt(value) => {
        index.lookup_lt(value)?
          .into_iter()
          .map(|entry| entry.file_hash.clone())
          .collect::<HashSet<Vec<u8>>>()
      }
      QueryOp::Between(min, max) => {
        index.lookup_range(min, max)?
          .into_iter()
          .map(|entry| entry.file_hash.clone())
          .collect::<HashSet<Vec<u8>>>()
      }
      QueryOp::In(values) => {
        let mut result = HashSet::new();
        for value in values {
          for entry in index.lookup_exact(value) {
            result.insert(entry.file_hash.clone());
          }
        }
        result
      }
      // Fuzzy ops are handled by execute_with_recheck, not here.
      QueryOp::Contains(_) | QueryOp::Similar(_, _) | QueryOp::Phonetic(_) | QueryOp::Fuzzy(_, _) | QueryOp::Match(_) => {
        return Err(EngineError::NotFound(
          "Fuzzy operations should use the recheck execution path".to_string(),
        ));
      }
    };

    Ok(matching_entries)
  }

  /// Collect all file hashes from all indexed fields at a path.
  /// Used as the "universe" for NOT operations.
  fn collect_all_hashes(
    &self,
    path: &str,
    index_manager: &IndexManager,
  ) -> EngineResult<HashSet<Vec<u8>>> {
    let field_names = index_manager.list_indexes(path)?;
    let mut all_hashes = HashSet::new();
    for field_name in &field_names {
      if let Some(index) = index_manager.load_index(path, field_name)? {
        for entry in &index.entries {
          all_hashes.insert(entry.file_hash.clone());
        }
      }
    }
    Ok(all_hashes)
  }

  /// Check if a QueryNode tree contains any fuzzy operations.
  fn node_has_fuzzy_ops(&self, node: &QueryNode) -> bool {
    match node {
      QueryNode::Field(fq) => matches!(
        fq.operation,
        QueryOp::Contains(_) | QueryOp::Similar(_, _) | QueryOp::Phonetic(_) | QueryOp::Fuzzy(_, _) | QueryOp::Match(_)
      ),
      QueryNode::And(children) | QueryNode::Or(children) => {
        children.iter().any(|c| self.node_has_fuzzy_ops(c))
      }
      QueryNode::Not(child) => self.node_has_fuzzy_ops(child),
    }
  }

  /// Execute a query containing fuzzy operations with a recheck phase.
  /// Currently supports single-field fuzzy queries (the common case).
  /// Values are loaded from the index's values map instead of re-reading files.
  fn execute_with_recheck_internal(
    &self,
    query: &Query,
    effective_node: &QueryNode,
  ) -> EngineResult<Vec<QueryResult>> {
    let index_manager = IndexManager::new(self.engine);
    let hash_length = self.engine.hash_algo().hash_length();
    let ops = DirectoryOps::new(self.engine);

    // Extract the single fuzzy field query
    let field_query = match effective_node {
      QueryNode::Field(fq) => fq,
      _ => {
        return Err(EngineError::NotFound(
          "Fuzzy operations currently support single-field queries only".to_string(),
        ));
      }
    };

    // Get candidates AND values from the appropriate index
    let (candidates, candidate_values) = self.get_fuzzy_candidates_with_values(
      field_query, &query.path, &index_manager,
    )?;

    // Recheck phase: get field value from index, compute score
    let mut results = Vec::new();

    for file_hash in candidates {
      // Try to get value from index first (works for parser-indexed files)
      let field_value = if let Some(value_bytes) = candidate_values.get(&file_hash) {
        String::from_utf8_lossy(value_bytes).to_string()
      } else {
        // Fallback: load file and parse as JSON (for native JSON files without values in index)
        let (_file_record, file_data) = match self.load_file_with_data(&file_hash, hash_length, &ops)? {
          Some(pair) => pair,
          None => continue,
        };
        match self.extract_field_value(&file_data, &field_query.field_name) {
          Some(v) => v,
          None => continue,
        }
      };

      // Load the FileRecord for the result
      let file_record = match self.engine.get_entry(&file_hash) {
        Ok(Some((_header, _key, value))) => {
          FileRecord::deserialize(&value, hash_length)?
        }
        _ => continue,
      };

      // Compute score based on operation
      let (score, strategy) = self.compute_score(&field_query.operation, &field_value)?;

      if score > 0.0 {
        results.push(QueryResult {
          file_hash,
          file_record,
          score,
          matched_by: strategy.split(',').filter(|s| !s.is_empty()).map(String::from).collect(),
        });
      }
    }

    // Sort by score descending
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    Ok(results)
  }

  /// Get candidate file hashes and their stored values from the appropriate index for a fuzzy query.
  fn get_fuzzy_candidates_with_values(
    &self,
    field_query: &FieldQuery,
    path: &str,
    index_manager: &IndexManager,
  ) -> EngineResult<(HashSet<Vec<u8>>, HashMap<Vec<u8>, Vec<u8>>)> {
    let mut all_values: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();

    match &field_query.operation {
      QueryOp::Contains(query_str) | QueryOp::Similar(query_str, _) | QueryOp::Fuzzy(query_str, _) => {
        // Use trigram index for candidates
        let mut index = match index_manager.load_index_by_strategy(path, &field_query.field_name, "trigram")? {
          Some(idx) => idx,
          None => {
            return Err(EngineError::NotFound(format!(
              "Trigram index not found for field '{}' at '{}'",
              field_query.field_name, path,
            )));
          }
        };

        // For Contains (substring), use unpadded trigrams to avoid
        // word-boundary padding mismatches. For similarity/fuzzy, use
        // the standard padded trigrams.
        let trigrams = if matches!(&field_query.operation, QueryOp::Contains(_)) {
          crate::engine::fuzzy::extract_trigrams_no_pad(query_str)
        } else {
          crate::engine::fuzzy::extract_trigrams(query_str)
        };
        let converter = TrigramConverter;

        let mut candidates = HashSet::new();

        if matches!(&field_query.operation, QueryOp::Contains(_)) {
          // AND: intersect all trigram lookups for substring matching
          let mut first = true;
          for trigram in &trigrams {
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
        } else {
          // OR: union all trigram lookups (broader candidates for similarity/fuzzy)
          for trigram in &trigrams {
            let scalar = converter.to_scalar(trigram);
            let entries = index.lookup_by_scalar(scalar);
            for entry in entries {
              candidates.insert(entry.file_hash.clone());
            }
          }
        }

        // Collect values from this index
        all_values.extend(index.values.drain());

        Ok((candidates, all_values))
      }
      QueryOp::Phonetic(query_str) => {
        // Use phonetic indexes for candidates
        // Tokenize query on whitespace — match any word's phonetic code
        let query_words: Vec<&str> = query_str.split_whitespace()
          .filter(|w| w.chars().any(|c| c.is_alphabetic()))
          .collect();

        let mut candidates = HashSet::new();
        let strategies = ["soundex", "dmetaphone", "dmetaphone_alt"];
        let mut found_any_index = false;

        for strategy in &strategies {
          if let Some(mut index) = index_manager.load_index_by_strategy(path, &field_query.field_name, strategy)? {
            found_any_index = true;

            for word in &query_words {
              let code = match *strategy {
                "soundex" => crate::engine::phonetic::soundex(word),
                "dmetaphone" => crate::engine::phonetic::dmetaphone_primary(word),
                "dmetaphone_alt" => crate::engine::phonetic::dmetaphone_alt(word)
                  .unwrap_or_else(|| crate::engine::phonetic::dmetaphone_primary(word)),
                _ => continue,
              };

              if code.is_empty() {
                continue;
              }

              let scalar = index.converter.to_scalar(code.as_bytes());
              let entries = index.lookup_by_scalar(scalar);
              for entry in entries {
                candidates.insert(entry.file_hash.clone());
              }
            }

            // Collect values from each phonetic index
            all_values.extend(index.values.drain());
          }
        }

        if !found_any_index {
          return Err(EngineError::NotFound(format!(
            "No phonetic index found for field '{}' at '{}'",
            field_query.field_name, path,
          )));
        }

        Ok((candidates, all_values))
      }
      QueryOp::Match(query_str) => {
        let mut candidates = HashSet::new();

        // Try trigram index
        if let Some(mut index) = index_manager.load_index_by_strategy(path, &field_query.field_name, "trigram")? {
          let trigrams = crate::engine::fuzzy::extract_trigrams(query_str);
          let converter = TrigramConverter;
          for trigram in &trigrams {
            let scalar = converter.to_scalar(trigram);
            let entries = index.lookup_by_scalar(scalar);
            for entry in entries {
              candidates.insert(entry.file_hash.clone());
            }
          }
          all_values.extend(index.values.drain());
        }

        // Try phonetic indexes (tokenize query on whitespace)
        let query_words: Vec<&str> = query_str.split_whitespace()
          .filter(|w| w.chars().any(|c| c.is_alphabetic()))
          .collect();
        let phonetic_strategies = ["soundex", "dmetaphone", "dmetaphone_alt"];
        for strategy in &phonetic_strategies {
          if let Some(mut index) = index_manager.load_index_by_strategy(path, &field_query.field_name, strategy)? {
            for word in &query_words {
              let code = match *strategy {
                "soundex" => crate::engine::phonetic::soundex(word),
                "dmetaphone" => crate::engine::phonetic::dmetaphone_primary(word),
                "dmetaphone_alt" => crate::engine::phonetic::dmetaphone_alt(word)
                  .unwrap_or_else(|| crate::engine::phonetic::dmetaphone_primary(word)),
                _ => continue,
              };
              if code.is_empty() { continue; }
              let scalar = index.converter.to_scalar(code.as_bytes());
              let entries = index.lookup_by_scalar(scalar);
              for entry in entries {
                candidates.insert(entry.file_hash.clone());
              }
            }
            all_values.extend(index.values.drain());
          }
        }

        // Try exact match via string index
        if let Some(mut index) = index_manager.load_index_by_strategy(path, &field_query.field_name, "string")? {
          let entries = index.lookup_exact(query_str.as_bytes());
          for entry in entries {
            candidates.insert(entry.file_hash.clone());
          }
          all_values.extend(index.values.drain());
        }

        Ok((candidates, all_values))
      }
      _ => {
        Err(EngineError::NotFound("Not a fuzzy operation".to_string()))
      }
    }
  }

  /// Load a file's FileRecord and raw data from its hash.
  /// Used as a fallback for native JSON files whose values are not in the index.
  fn load_file_with_data(
    &self,
    file_hash: &[u8],
    hash_length: usize,
    ops: &DirectoryOps,
  ) -> EngineResult<Option<(FileRecord, Vec<u8>)>> {
    match self.engine.get_entry(file_hash) {
      Ok(Some((_header, _key, value))) => {
        let file_record = FileRecord::deserialize(&value, hash_length)?;

        match ops.read_file(&file_record.path) {
          Ok(data) => Ok(Some((file_record, data))),
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
  fn compute_score(&self, op: &QueryOp, field_value: &str) -> EngineResult<(f64, String)> {
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
        let q_words: Vec<&str> = query_str.split_whitespace()
          .filter(|w| w.chars().any(|c| c.is_alphabetic()))
          .collect();
        let v_words: Vec<&str> = field_value.split_whitespace()
          .filter(|w| w.chars().any(|c| c.is_alphabetic()))
          .collect();

        let mut strategies = Vec::new();

        for qw in &q_words {
          for vw in &v_words {
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
              if !q_alt.is_empty() && (q_alt == &v_dm || Some(q_alt) == v_dm_alt.as_ref()) && !strategies.contains(&"dmetaphone_alt".to_string()) {
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
      QueryOp::Fuzzy(query_str, options) => {
        match options.algorithm {
          FuzzyAlgorithm::DamerauLevenshtein => {
            let distance = crate::engine::fuzzy::damerau_levenshtein(query_str, field_value);
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
        }
      }
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
          if trig_score > max_score { max_score = trig_score; }
          strategies.push("trigram".to_string());
        }

        // Phonetic matching (tokenize both sides)
        let q_words: Vec<&str> = query_str.split_whitespace()
          .filter(|w| w.chars().any(|c| c.is_alphabetic()))
          .collect();
        let v_words: Vec<&str> = field_value.split_whitespace()
          .filter(|w| w.chars().any(|c| c.is_alphabetic()))
          .collect();

        'soundex_check: for qw in &q_words {
          for vw in &v_words {
            let qs = crate::engine::phonetic::soundex(qw);
            let vs = crate::engine::phonetic::soundex(vw);
            if !qs.is_empty() && qs == vs {
              if 1.0 > max_score { max_score = 1.0; }
              strategies.push("soundex".to_string());
              break 'soundex_check;
            }
          }
        }

        'dm_check: for qw in &q_words {
          for vw in &v_words {
            let qd = crate::engine::phonetic::dmetaphone_primary(qw);
            let vd = crate::engine::phonetic::dmetaphone_primary(vw);
            let vda = crate::engine::phonetic::dmetaphone_alt(vw);
            if !qd.is_empty() && (qd == vd || Some(&qd) == vda.as_ref()) {
              if 1.0 > max_score { max_score = 1.0; }
              strategies.push("dmetaphone".to_string());
              break 'dm_check;
            }
          }
        }

        // Edit distance
        let distance = crate::engine::fuzzy::damerau_levenshtein(query_str, field_value);
        let max_edits = crate::engine::fuzzy::auto_fuzziness(query_str.len());
        if distance <= max_edits {
          let max_len = query_str.len().max(field_value.len()).max(1);
          let dl_score = 1.0 - (distance as f64 / max_len as f64);
          if dl_score > max_score { max_score = dl_score; }
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

  /// Execute an aggregation query.
  pub fn execute_aggregate(&self, query: &Query) -> EngineResult<AggregateResult> {
    let agg = query.aggregate.as_ref().ok_or_else(|| {
        EngineError::NotFound("No aggregate query specified".to_string())
    })?;

    // Run the filter to get matching file hashes
    let result_hashes = self.execute_internal(query)?;
    let result_hash_set: HashSet<Vec<u8>> = result_hashes.iter()
        .map(|r| r.file_hash.clone())
        .collect();

    let index_manager = IndexManager::new(self.engine);
    let effective_limit = query.limit.unwrap_or(DEFAULT_QUERY_LIMIT);
    let explicit_limit = query.limit.is_some();

    // COUNT
    let count = if agg.count {
        Some(result_hash_set.len() as u64)
    } else {
        None
    };

    // Collect all aggregate field names
    let mut agg_fields: HashSet<&str> = HashSet::new();
    for f in &agg.sum { agg_fields.insert(f); }
    for f in &agg.avg { agg_fields.insert(f); }
    for f in &agg.min { agg_fields.insert(f); }
    for f in &agg.max { agg_fields.insert(f); }

    // Load indexes for aggregate fields
    let mut field_indexes: HashMap<String, (HashMap<Vec<u8>, Vec<u8>>, u8)> = HashMap::new();
    for field_name in &agg_fields {
        let indexes = index_manager.load_indexes_for_field(&query.path, field_name)?;
        let index = indexes.into_iter().next().ok_or_else(|| {
            EngineError::NotFound(format!("No index found for aggregate field '{}'", field_name))
        })?;
        let type_tag = index.converter.type_tag();
        field_indexes.insert(field_name.to_string(), (index.values, type_tag));
    }

    // Validate SUM/AVG fields are numeric
    for field_name in &agg.sum {
        if let Some((_, type_tag)) = field_indexes.get(field_name.as_str()) {
            if !is_numeric_type(*type_tag) {
                return Err(EngineError::NotFound(format!(
                    "Cannot compute SUM on field '{}' -- requires numeric index type", field_name
                )));
            }
        }
    }
    for field_name in &agg.avg {
        if let Some((_, type_tag)) = field_indexes.get(field_name.as_str()) {
            if !is_numeric_type(*type_tag) {
                return Err(EngineError::NotFound(format!(
                    "Cannot compute AVG on field '{}' -- requires numeric index type", field_name
                )));
            }
        }
    }

    // If no GROUP BY, compute flat aggregates
    if agg.group_by.is_empty() {
        let (sum, avg, min, max) = compute_aggregates(
            &result_hash_set, agg, &field_indexes,
        );

        return Ok(AggregateResult {
            count,
            sum,
            avg,
            min,
            max,
            groups: None,
            has_more: false,
            default_limit_hit: false,
        });
    }

    // GROUP BY: load group field indexes
    let mut group_field_data: Vec<(String, HashMap<Vec<u8>, Vec<u8>>, u8)> = Vec::new();
    for gf in &agg.group_by {
        let indexes = index_manager.load_indexes_for_field(&query.path, gf)?;
        let index = indexes.into_iter().next().ok_or_else(|| {
            EngineError::NotFound(format!("No index found for group_by field '{}'", gf))
        })?;
        let type_tag = index.converter.type_tag();
        group_field_data.push((gf.clone(), index.values, type_tag));
    }

    // Bucket results by group key
    let mut groups: HashMap<String, (HashMap<String, serde_json::Value>, Vec<Vec<u8>>)> = HashMap::new();

    for file_hash in &result_hash_set {
        // Build group key from all group_by fields
        let mut key_map = HashMap::new();
        let mut key_parts: Vec<String> = Vec::new();

        for (field_name, values, type_tag) in &group_field_data {
            let value = values.get(file_hash.as_slice())
                .map(|bytes| bytes_to_json_value(bytes, *type_tag))
                .unwrap_or(serde_json::Value::Null);
            key_parts.push(format!("{}={}", field_name, value));
            key_map.insert(field_name.clone(), value);
        }

        let group_key = key_parts.join("|");
        groups.entry(group_key)
            .or_insert_with(|| (key_map, Vec::new()))
            .1.push(file_hash.clone());
    }

    // Compute aggregates per group
    let mut group_results: Vec<GroupResult> = Vec::new();

    for (_key_str, (key_map, group_hashes)) in &groups {
        let group_hash_set: HashSet<Vec<u8>> = group_hashes.iter().cloned().collect();
        let (sum, avg, min, max) = compute_aggregates(&group_hash_set, agg, &field_indexes);

        group_results.push(GroupResult {
            key: key_map.clone(),
            count: group_hashes.len() as u64,
            sum,
            avg,
            min,
            max,
        });
    }

    // Sort groups by count descending (most populated first)
    group_results.sort_by(|a, b| b.count.cmp(&a.count));

    // Apply limit to groups
    let has_more = group_results.len() > effective_limit;
    group_results.truncate(effective_limit);
    let default_limit_hit = !explicit_limit && has_more;

    Ok(AggregateResult {
        count,
        sum: HashMap::new(),
        avg: HashMap::new(),
        min: HashMap::new(),
        max: HashMap::new(),
        groups: Some(group_results),
        has_more,
        default_limit_hit,
    })
  }
}

// ---------------------------------------------------------------------------
// Aggregation helpers
// ---------------------------------------------------------------------------

/// Parse raw value bytes into a numeric f64, using the converter type to determine format.
pub fn bytes_to_f64(bytes: &[u8], type_tag: u8) -> Option<f64> {
    match type_tag {
        CONVERTER_TYPE_U8 => {
            if bytes.len() >= 1 { Some(bytes[0] as f64) }
            else { None }
        }
        CONVERTER_TYPE_U16 => {
            if bytes.len() >= 2 { Some(u16::from_be_bytes([bytes[0], bytes[1]]) as f64) }
            else { None }
        }
        CONVERTER_TYPE_U32 => {
            if bytes.len() >= 4 { Some(u32::from_be_bytes(bytes[..4].try_into().ok()?) as f64) }
            else { None }
        }
        CONVERTER_TYPE_U64 => {
            if bytes.len() >= 8 { Some(u64::from_be_bytes(bytes[..8].try_into().ok()?) as f64) }
            else { None }
        }
        CONVERTER_TYPE_I64 | CONVERTER_TYPE_TIMESTAMP => {
            if bytes.len() >= 8 { Some(i64::from_be_bytes(bytes[..8].try_into().ok()?) as f64) }
            else { None }
        }
        CONVERTER_TYPE_F64 => {
            if bytes.len() >= 8 { Some(f64::from_be_bytes(bytes[..8].try_into().ok()?)) }
            else { None }
        }
        _ => None,
    }
}

/// Parse raw value bytes into a JSON value for display (MIN/MAX, GROUP BY keys).
pub fn bytes_to_json_value(bytes: &[u8], type_tag: u8) -> serde_json::Value {
    match type_tag {
        CONVERTER_TYPE_U8 => {
            if bytes.len() >= 1 { serde_json::json!(bytes[0]) }
            else { serde_json::Value::Null }
        }
        CONVERTER_TYPE_U16 => {
            if bytes.len() >= 2 { serde_json::json!(u16::from_be_bytes([bytes[0], bytes[1]])) }
            else { serde_json::Value::Null }
        }
        CONVERTER_TYPE_U32 => {
            if bytes.len() >= 4 {
                serde_json::json!(u32::from_be_bytes(bytes[..4].try_into().unwrap()))
            } else { serde_json::Value::Null }
        }
        CONVERTER_TYPE_U64 => {
            if bytes.len() >= 8 {
                serde_json::json!(u64::from_be_bytes(bytes[..8].try_into().unwrap()))
            } else { serde_json::Value::Null }
        }
        CONVERTER_TYPE_I64 | CONVERTER_TYPE_TIMESTAMP => {
            if bytes.len() >= 8 {
                serde_json::json!(i64::from_be_bytes(bytes[..8].try_into().unwrap()))
            } else { serde_json::Value::Null }
        }
        CONVERTER_TYPE_F64 => {
            if bytes.len() >= 8 {
                serde_json::json!(f64::from_be_bytes(bytes[..8].try_into().unwrap()))
            } else { serde_json::Value::Null }
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
    matches!(type_tag,
        CONVERTER_TYPE_U8 | CONVERTER_TYPE_U16 | CONVERTER_TYPE_U32 | CONVERTER_TYPE_U64 |
        CONVERTER_TYPE_I64 | CONVERTER_TYPE_F64
    )
}

/// Shared aggregation computation: iterates the hash set, computes SUM, AVG, MIN, MAX.
fn compute_aggregates(
    hash_set: &HashSet<Vec<u8>>,
    agg: &AggregateQuery,
    field_indexes: &HashMap<String, (HashMap<Vec<u8>, Vec<u8>>, u8)>,
) -> (HashMap<String, f64>, HashMap<String, f64>, HashMap<String, serde_json::Value>, HashMap<String, serde_json::Value>) {
    let mut sum_map: HashMap<String, f64> = HashMap::new();
    let mut avg_counts: HashMap<String, (f64, u64)> = HashMap::new();
    let mut min_map: HashMap<String, (serde_json::Value, Vec<u8>)> = HashMap::new();
    let mut max_map: HashMap<String, (serde_json::Value, Vec<u8>)> = HashMap::new();

    for file_hash in hash_set {
        // SUM
        for field_name in &agg.sum {
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
            if let Some((values, type_tag)) = field_indexes.get(field_name.as_str()) {
                if let Some(bytes) = values.get(file_hash.as_slice()) {
                    let current = min_map.get(field_name.as_str());
                    if current.is_none() || bytes.as_slice() < current.unwrap().1.as_slice() {
                        min_map.insert(field_name.clone(), (bytes_to_json_value(bytes, *type_tag), bytes.clone()));
                    }
                }
            }
        }

        // MAX
        for field_name in &agg.max {
            if let Some((values, type_tag)) = field_indexes.get(field_name.as_str()) {
                if let Some(bytes) = values.get(file_hash.as_slice()) {
                    let current = max_map.get(field_name.as_str());
                    if current.is_none() || bytes.as_slice() > current.unwrap().1.as_slice() {
                        max_map.insert(field_name.clone(), (bytes_to_json_value(bytes, *type_tag), bytes.clone()));
                    }
                }
            }
        }
    }

    let avg_map: HashMap<String, f64> = avg_counts.into_iter()
        .map(|(k, (sum, count))| (k, if count > 0 { sum / count as f64 } else { 0.0 }))
        .collect();

    let min_display: HashMap<String, serde_json::Value> = min_map.into_iter()
        .map(|(k, (v, _))| (k, v))
        .collect();

    let max_display: HashMap<String, serde_json::Value> = max_map.into_iter()
        .map(|(k, (v, _))| (k, v))
        .collect();

    (sum_map, avg_map, min_display, max_display)
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
    }
  }

  /// Start building a field query.
  pub fn field(self, name: &str) -> FieldQueryBuilder<'a> {
    FieldQueryBuilder {
      parent: self,
      field_name: name.to_string(),
    }
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
    self.order_by_fields.push(SortField {
      field: field.to_string(),
      direction,
    });
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

  /// Add an explicit AND group via a sub-builder closure.
  pub fn and<F>(mut self, build_fn: F) -> Self
  where
    F: FnOnce(QueryBuilder<'a>) -> QueryBuilder<'a>,
  {
    let sub = QueryBuilder::new(self.engine, &self.path);
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
    let sub = QueryBuilder::new(self.engine, &self.path);
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
    let sub = QueryBuilder::new(self.engine, &self.path);
    let built = build_fn(sub);
    if !built.nodes.is_empty() {
      let inner = if built.nodes.len() == 1 {
        built.nodes.into_iter().next().unwrap()
      } else {
        QueryNode::And(built.nodes)
      };
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
    query_engine.execute(&query)
  }

  /// Execute and return the first matching result.
  pub fn first(&self) -> EngineResult<Option<QueryResult>> {
    let mut query = self.build_query();
    query.limit = Some(1);
    let query_engine = QueryEngine::new(self.engine);
    let mut results = query_engine.execute(&query)?;
    Ok(results.pop())
  }

  /// Execute and return only the count of matching results.
  pub fn count(&self) -> EngineResult<usize> {
    let query = self.build_query();
    let query_engine = QueryEngine::new(self.engine);
    let results = query_engine.execute(&query)?;
    Ok(results.len())
  }

  /// Execute with pagination support and return a PaginatedResult.
  pub fn execute_paginated(&self) -> EngineResult<PaginatedResult> {
    let query = self.build_query();
    let query_engine = QueryEngine::new(self.engine);
    query_engine.execute_paginated(&query)
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
    self.parent.nodes.push(QueryNode::Field(FieldQuery {
      field_name: self.field_name,
      operation: QueryOp::Eq(value.to_vec()),
    }));
    self.parent
  }

  /// Greater than (raw bytes).
  pub fn gt(mut self, value: &[u8]) -> QueryBuilder<'a> {
    self.parent.nodes.push(QueryNode::Field(FieldQuery {
      field_name: self.field_name,
      operation: QueryOp::Gt(value.to_vec()),
    }));
    self.parent
  }

  /// Less than (raw bytes).
  pub fn lt(mut self, value: &[u8]) -> QueryBuilder<'a> {
    self.parent.nodes.push(QueryNode::Field(FieldQuery {
      field_name: self.field_name,
      operation: QueryOp::Lt(value.to_vec()),
    }));
    self.parent
  }

  /// Range: between min and max (inclusive, raw bytes).
  pub fn between(mut self, min: &[u8], max: &[u8]) -> QueryBuilder<'a> {
    self.parent.nodes.push(QueryNode::Field(FieldQuery {
      field_name: self.field_name,
      operation: QueryOp::Between(min.to_vec(), max.to_vec()),
    }));
    self.parent
  }

  /// Match any of the given values (raw bytes).
  pub fn in_values(mut self, values: Vec<Vec<u8>>) -> QueryBuilder<'a> {
    self.parent.nodes.push(QueryNode::Field(FieldQuery {
      field_name: self.field_name,
      operation: QueryOp::In(values),
    }));
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
    let byte_values = values.iter()
      .map(|v| v.to_be_bytes().to_vec())
      .collect();
    self.in_values(byte_values)
  }

  /// Match any of the given string values.
  pub fn in_str(self, values: &[&str]) -> QueryBuilder<'a> {
    let byte_values = values.iter()
      .map(|v| v.as_bytes().to_vec())
      .collect();
    self.in_values(byte_values)
  }

  // --- Fuzzy search methods ---

  /// Substring match via trigram index + recheck.
  pub fn contains(mut self, value: &str) -> QueryBuilder<'a> {
    self.parent.nodes.push(QueryNode::Field(FieldQuery {
      field_name: self.field_name,
      operation: QueryOp::Contains(value.to_string()),
    }));
    self.parent
  }

  /// Trigram similarity match with threshold.
  pub fn similar(mut self, value: &str, threshold: f64) -> QueryBuilder<'a> {
    self.parent.nodes.push(QueryNode::Field(FieldQuery {
      field_name: self.field_name,
      operation: QueryOp::Similar(value.to_string(), threshold),
    }));
    self.parent
  }

  /// Phonetic code match (soundex / double metaphone).
  pub fn phonetic(mut self, value: &str) -> QueryBuilder<'a> {
    self.parent.nodes.push(QueryNode::Field(FieldQuery {
      field_name: self.field_name,
      operation: QueryOp::Phonetic(value.to_string()),
    }));
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
    self.parent.nodes.push(QueryNode::Field(FieldQuery {
      field_name: self.field_name,
      operation: QueryOp::Fuzzy(value.to_string(), options),
    }));
    self.parent
  }

  /// Composite match: run all matching indexes and score-fuse.
  pub fn match_query(mut self, value: &str) -> QueryBuilder<'a> {
    self.parent.nodes.push(QueryNode::Field(FieldQuery {
      field_name: self.field_name,
      operation: QueryOp::Match(value.to_string()),
    }));
    self.parent
  }
}
