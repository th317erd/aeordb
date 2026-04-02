use std::collections::HashSet;

use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::index_store::{FieldIndex, IndexManager};
use crate::engine::nvt_ops::NVTMask;
use crate::engine::storage_engine::StorageEngine;

/// A query operation on a field.
#[derive(Debug, Clone)]
pub enum QueryOp {
  Eq(Vec<u8>),
  Gt(Vec<u8>),
  Lt(Vec<u8>),
  Between(Vec<u8>, Vec<u8>),
  In(Vec<Vec<u8>>),
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

/// A complete query: path + query node tree + optional limit + strategy.
#[derive(Debug, Clone)]
pub struct Query {
  pub path: String,
  pub field_queries: Vec<FieldQuery>,
  pub node: Option<QueryNode>,
  pub limit: Option<usize>,
  pub strategy: QueryStrategy,
}

/// A single query result.
#[derive(Debug)]
pub struct QueryResult {
  pub file_hash: Vec<u8>,
  pub file_record: FileRecord,
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
  pub fn execute(&self, query: &Query) -> EngineResult<Vec<QueryResult>> {
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
          results.push(QueryResult { file_hash, file_record });
        }
        Ok(None) => continue, // stale index entry, skip
        Err(error) => return Err(error),
      }
    }

    // Apply limit.
    if let Some(limit) = query.limit {
      results.truncate(limit);
    }

    Ok(results)
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
}

/// Chainable query builder.
pub struct QueryBuilder<'a> {
  engine: &'a StorageEngine,
  path: String,
  nodes: Vec<QueryNode>,
  limit_value: Option<usize>,
  strategy_value: QueryStrategy,
}

impl<'a> QueryBuilder<'a> {
  pub fn new(engine: &'a StorageEngine, path: &str) -> Self {
    QueryBuilder {
      engine,
      path: path.to_string(),
      nodes: Vec::new(),
      limit_value: None,
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
      strategy: self.strategy_value.clone(),
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
}
