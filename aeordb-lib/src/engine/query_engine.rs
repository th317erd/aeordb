use std::collections::HashSet;

use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::index_store::{FieldIndex, IndexManager};
use crate::engine::json_parser::parse_json_fields;
use crate::engine::nvt_ops::NVTMask;
use crate::engine::scalar_converter::{ScalarConverter, TrigramConverter};
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
      return self.execute_with_recheck(query, &effective_node);
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
  fn execute_with_recheck(
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

    // Get candidates from the appropriate index
    let candidates = self.get_fuzzy_candidates(field_query, &query.path, &index_manager)?;

    // Recheck phase: load each candidate, extract field value, compute score
    let mut results = Vec::new();

    for file_hash in candidates {
      // Load the FileRecord and file data
      let (file_record, file_data) = match self.load_file_with_data(&file_hash, hash_length, &ops)? {
        Some(pair) => pair,
        None => continue,
      };

      // Extract the field value from JSON
      let field_value = match self.extract_field_value(&file_data, &field_query.field_name) {
        Some(v) => v,
        None => continue,
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

    // Apply limit
    if let Some(limit) = query.limit {
      results.truncate(limit);
    }

    Ok(results)
  }

  /// Get candidate file hashes from the appropriate index for a fuzzy query.
  fn get_fuzzy_candidates(
    &self,
    field_query: &FieldQuery,
    path: &str,
    index_manager: &IndexManager,
  ) -> EngineResult<HashSet<Vec<u8>>> {
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

        Ok(candidates)
      }
      QueryOp::Phonetic(query_str) => {
        // Use phonetic indexes for candidates
        let mut candidates = HashSet::new();
        let strategies = ["soundex", "dmetaphone", "dmetaphone_alt"];
        let mut found_any_index = false;

        for strategy in &strategies {
          if let Some(mut index) = index_manager.load_index_by_strategy(path, &field_query.field_name, strategy)? {
            found_any_index = true;
            // Compute the phonetic code for the query value
            let code = match *strategy {
              "soundex" => crate::engine::phonetic::soundex(query_str),
              "dmetaphone" => crate::engine::phonetic::dmetaphone_primary(query_str),
              "dmetaphone_alt" => crate::engine::phonetic::dmetaphone_alt(query_str)
                .unwrap_or_else(|| crate::engine::phonetic::dmetaphone_primary(query_str)),
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
        }

        if !found_any_index {
          return Err(EngineError::NotFound(format!(
            "No phonetic index found for field '{}' at '{}'",
            field_query.field_name, path,
          )));
        }

        Ok(candidates)
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
        }

        // Try phonetic indexes
        let phonetic_strategies = ["soundex", "dmetaphone", "dmetaphone_alt"];
        for strategy in &phonetic_strategies {
          if let Some(mut index) = index_manager.load_index_by_strategy(path, &field_query.field_name, strategy)? {
            let code = match *strategy {
              "soundex" => crate::engine::phonetic::soundex(query_str),
              "dmetaphone" => crate::engine::phonetic::dmetaphone_primary(query_str),
              "dmetaphone_alt" => crate::engine::phonetic::dmetaphone_alt(query_str)
                .unwrap_or_else(|| crate::engine::phonetic::dmetaphone_primary(query_str)),
              _ => continue,
            };
            if code.is_empty() { continue; }
            let scalar = index.converter.to_scalar(code.as_bytes());
            let entries = index.lookup_by_scalar(scalar);
            for entry in entries {
              candidates.insert(entry.file_hash.clone());
            }
          }
        }

        // Try exact match via string index
        if let Some(mut index) = index_manager.load_index_by_strategy(path, &field_query.field_name, "string")? {
          let entries = index.lookup_exact(query_str.as_bytes());
          for entry in entries {
            candidates.insert(entry.file_hash.clone());
          }
        }

        Ok(candidates)
      }
      _ => {
        Err(EngineError::NotFound("Not a fuzzy operation".to_string()))
      }
    }
  }

  /// Load a file's FileRecord and raw data from its hash.
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
        let q_soundex = crate::engine::phonetic::soundex(query_str);
        let v_soundex = crate::engine::phonetic::soundex(field_value);

        let q_dm = crate::engine::phonetic::dmetaphone_primary(query_str);
        let v_dm = crate::engine::phonetic::dmetaphone_primary(field_value);

        let q_dm_alt = crate::engine::phonetic::dmetaphone_alt(query_str);
        let v_dm_alt = crate::engine::phonetic::dmetaphone_alt(field_value);

        let mut strategies = Vec::new();

        if !q_soundex.is_empty() && q_soundex == v_soundex {
          strategies.push("soundex".to_string());
        }
        if !q_dm.is_empty() && (q_dm == v_dm || Some(&q_dm) == v_dm_alt.as_ref()) {
          strategies.push("dmetaphone".to_string());
        }
        if let Some(ref q_alt) = q_dm_alt {
          if !q_alt.is_empty() && (q_alt == &v_dm || Some(q_alt) == v_dm_alt.as_ref()) {
            strategies.push("dmetaphone_alt".to_string());
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

        // Phonetic matching
        let q_soundex = crate::engine::phonetic::soundex(query_str);
        let v_soundex = crate::engine::phonetic::soundex(field_value);
        if !q_soundex.is_empty() && q_soundex == v_soundex {
          if 1.0 > max_score { max_score = 1.0; }
          strategies.push("soundex".to_string());
        }

        let q_dm = crate::engine::phonetic::dmetaphone_primary(query_str);
        let v_dm = crate::engine::phonetic::dmetaphone_primary(field_value);
        let v_dm_alt = crate::engine::phonetic::dmetaphone_alt(field_value);
        if !q_dm.is_empty() && (q_dm == v_dm || Some(&q_dm) == v_dm_alt.as_ref()) {
          if 1.0 > max_score { max_score = 1.0; }
          strategies.push("dmetaphone".to_string());
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
