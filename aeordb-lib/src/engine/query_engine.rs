use std::collections::HashSet;

use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::index_store::IndexManager;
use crate::engine::storage_engine::StorageEngine;

/// A query operation on a field.
#[derive(Debug, Clone)]
pub enum QueryOp {
  Eq(Vec<u8>),
  Gt(Vec<u8>),
  Lt(Vec<u8>),
  Between(Vec<u8>, Vec<u8>),
}

/// A query on a single field.
#[derive(Debug, Clone)]
pub struct FieldQuery {
  pub field_name: String,
  pub operation: QueryOp,
}

/// A complete query: path + field queries + optional limit.
#[derive(Debug, Clone)]
pub struct Query {
  pub path: String,
  pub field_queries: Vec<FieldQuery>,
  pub limit: Option<usize>,
}

/// A single query result.
#[derive(Debug)]
pub struct QueryResult {
  pub file_hash: Vec<u8>,
  pub file_record: FileRecord,
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
  pub fn execute(&self, query: &Query) -> EngineResult<Vec<QueryResult>> {
    if query.field_queries.is_empty() {
      return Ok(Vec::new());
    }

    let index_manager = IndexManager::new(self.engine);
    let mut candidate_sets: Vec<HashSet<Vec<u8>>> = Vec::new();

    for field_query in &query.field_queries {
      let index = index_manager.load_index(&query.path, &field_query.field_name)?;
      let mut index = match index {
        Some(index) => index,
        None => {
          return Err(EngineError::NotFound(format!(
            "Index not found for field '{}' at path '{}'",
            field_query.field_name, query.path,
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
      };

      candidate_sets.push(matching_entries);
    }

    // Intersect all candidate sets (AND logic)
    let mut result_hashes = candidate_sets[0].clone();
    for set in &candidate_sets[1..] {
      result_hashes = result_hashes.intersection(set).cloned().collect();
    }

    // Load FileRecords for candidates
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

    // Apply limit
    if let Some(limit) = query.limit {
      results.truncate(limit);
    }

    Ok(results)
  }
}

/// Chainable query builder.
pub struct QueryBuilder<'a> {
  engine: &'a StorageEngine,
  path: String,
  field_queries: Vec<FieldQuery>,
  limit_value: Option<usize>,
}

impl<'a> QueryBuilder<'a> {
  pub fn new(engine: &'a StorageEngine, path: &str) -> Self {
    QueryBuilder {
      engine,
      path: path.to_string(),
      field_queries: Vec::new(),
      limit_value: None,
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

  /// Execute and return all matching results.
  pub fn all(&self) -> EngineResult<Vec<QueryResult>> {
    let query = Query {
      path: self.path.clone(),
      field_queries: self.field_queries.clone(),
      limit: self.limit_value,
    };
    let query_engine = QueryEngine::new(self.engine);
    query_engine.execute(&query)
  }

  /// Execute and return the first matching result.
  pub fn first(&self) -> EngineResult<Option<QueryResult>> {
    let query = Query {
      path: self.path.clone(),
      field_queries: self.field_queries.clone(),
      limit: Some(1),
    };
    let query_engine = QueryEngine::new(self.engine);
    let mut results = query_engine.execute(&query)?;
    Ok(results.pop())
  }

  /// Execute and return only the count of matching results.
  pub fn count(&self) -> EngineResult<usize> {
    let query = Query {
      path: self.path.clone(),
      field_queries: self.field_queries.clone(),
      limit: self.limit_value,
    };
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
  /// Exact match.
  pub fn eq(mut self, value: &[u8]) -> QueryBuilder<'a> {
    self.parent.field_queries.push(FieldQuery {
      field_name: self.field_name,
      operation: QueryOp::Eq(value.to_vec()),
    });
    self.parent
  }

  /// Greater than.
  pub fn gt(mut self, value: &[u8]) -> QueryBuilder<'a> {
    self.parent.field_queries.push(FieldQuery {
      field_name: self.field_name,
      operation: QueryOp::Gt(value.to_vec()),
    });
    self.parent
  }

  /// Less than.
  pub fn lt(mut self, value: &[u8]) -> QueryBuilder<'a> {
    self.parent.field_queries.push(FieldQuery {
      field_name: self.field_name,
      operation: QueryOp::Lt(value.to_vec()),
    });
    self.parent
  }

  /// Range: between min and max (inclusive).
  pub fn between(mut self, min: &[u8], max: &[u8]) -> QueryBuilder<'a> {
    self.parent.field_queries.push(FieldQuery {
      field_name: self.field_name,
      operation: QueryOp::Between(min.to_vec(), max.to_vec()),
    });
    self.parent
  }
}
