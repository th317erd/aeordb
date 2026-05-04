use std::collections::HashMap;

use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::index_store::IndexManager;
use crate::engine::query_engine::{
  FieldQuery, Query, QueryEngine, QueryNode, QueryOp, QueryResult,
  ExplainMode, QueryStrategy,
  DEFAULT_QUERY_LIMIT,
};
use crate::engine::storage_engine::StorageEngine;

/// A single result from a global search, enriched with source metadata.
#[derive(Debug)]
pub struct SearchResult {
  /// Full file path.
  pub path: String,
  /// Relevance score (higher is better).
  pub score: f64,
  /// Names of the indexes/strategies that produced this match.
  pub matched_by: Vec<String>,
  /// Directory where the matching index lives.
  pub source_dir: String,
  /// File size in bytes.
  pub size: u64,
  /// MIME content type, if known.
  pub content_type: Option<String>,
  /// Creation timestamp (millis since epoch).
  pub created_at: i64,
  /// Last-updated timestamp (millis since epoch).
  pub updated_at: i64,
}

/// Paginated container for search results.
#[derive(Debug)]
pub struct SearchResults {
  /// The current page of results, sorted by score descending.
  pub results: Vec<SearchResult>,
  /// True when more results exist beyond this page.
  pub has_more: bool,
  /// Total matching count (populated only when computable cheaply).
  pub total_count: Option<usize>,
}

/// Perform a global search across all indexed directories under `base_path`.
///
/// Two modes are supported:
///
/// 1. **Broad / fuzzy search** (`query` is `Some`):
///    Discovers every indexed directory, loads fuzzy-capable indexes
///    (trigram, soundex, dmetaphone), and searches each for candidates.
///    Results are scored via trigram similarity + phonetic matching and
///    fused across directories.
///
/// 2. **Structured search** (`where_clause` is `Some`):
///    Delegates to the existing `QueryEngine` for each discovered
///    directory that has the requested field indexed.
///
/// Results from all directories are merged by score, deduplicated by
/// path, and paginated according to `limit` and `offset`.
pub fn global_search(
  engine: &StorageEngine,
  base_path: &str,
  query: Option<&str>,
  where_clause: Option<&FieldQuery>,
  limit: Option<usize>,
  offset: Option<usize>,
) -> EngineResult<SearchResults> {
  let index_manager = IndexManager::new(engine);

  // Discover all directories that have indexes.
  let indexed_dirs = index_manager.discover_indexed_directories(base_path)?;

  if indexed_dirs.is_empty() {
    return Ok(SearchResults {
      results: Vec::new(),
      has_more: false,
      total_count: Some(0),
    });
  }

  // Collect raw results across every directory.
  let mut all_results: Vec<SearchResult> = Vec::new();

  if let Some(query_str) = query {
    // Broad search: search fuzzy-capable indexes in every directory.
    broad_search(engine, &index_manager, &indexed_dirs, query_str, &mut all_results)?;
  } else if let Some(field_query) = where_clause {
    // Structured search: delegate to QueryEngine per directory.
    structured_search(engine, &index_manager, &indexed_dirs, field_query, &mut all_results)?;
  } else {
    // Neither query nor where_clause provided -- nothing to search.
    return Ok(SearchResults {
      results: Vec::new(),
      has_more: false,
      total_count: Some(0),
    });
  }

  // Deduplicate by path, keeping the highest score for each.
  deduplicate_by_path(&mut all_results);

  // Sort by score descending (ties broken by path for determinism).
  all_results.sort_by(|a, b| {
    b.score
      .partial_cmp(&a.score)
      .unwrap_or(std::cmp::Ordering::Equal)
      .then_with(|| a.path.cmp(&b.path))
  });

  let total_count = all_results.len();
  let effective_offset = offset.unwrap_or(0);
  let effective_limit = limit.unwrap_or(DEFAULT_QUERY_LIMIT);

  let page: Vec<SearchResult> = all_results
    .into_iter()
    .skip(effective_offset)
    .take(effective_limit + 1) // fetch one extra to detect has_more
    .collect();

  let has_more = page.len() > effective_limit;
  let results: Vec<SearchResult> = page.into_iter().take(effective_limit).collect();

  Ok(SearchResults {
    results,
    has_more,
    total_count: Some(total_count),
  })
}

// ---------------------------------------------------------------------------
// Broad (fuzzy) search
// ---------------------------------------------------------------------------

/// Search all fuzzy-capable indexes (trigram, soundex, dmetaphone) in every
/// discovered directory.  For each directory we:
///
/// 1. List its indexes and pick those ending in `.trigram`, `.soundex`,
///    `.dmetaphone`, or `.dmetaphone_alt`.
/// 2. Use the existing `QueryOp::Match` operation via `QueryEngine` which
///    already fuses trigram + phonetic + exact strategies and assigns scores.
///
/// This re-uses the battle-tested scoring path in QueryEngine rather than
/// re-implementing trigram/phonetic lookup from scratch.
fn broad_search(
  engine: &StorageEngine,
  index_manager: &IndexManager,
  indexed_dirs: &[String],
  query_str: &str,
  out: &mut Vec<SearchResult>,
) -> EngineResult<()> {
  let query_engine = QueryEngine::new(engine);

  for dir in indexed_dirs {
    let indexes = index_manager.list_indexes(dir)?;
    if indexes.is_empty() {
      continue;
    }

    // Identify fields that have fuzzy-capable indexes.
    let fuzzy_fields = discover_fuzzy_fields(&indexes);
    if fuzzy_fields.is_empty() {
      continue;
    }

    // For each fuzzy-capable field, build a Match query and execute it.
    for field_name in &fuzzy_fields {
      let q = Query {
        path: dir.clone(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
          field_name: field_name.clone(),
          operation: QueryOp::Match(query_str.to_string()),
        })),
        limit: None, // no per-directory limit; we paginate globally
        offset: None,
        order_by: vec![],
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Auto,
        aggregate: None,
        explain: ExplainMode::Off,
      };

      match query_engine.execute(&q) {
        Ok(qr_results) => {
          for qr in qr_results {
            out.push(query_result_to_search_result(qr, dir, field_name));
          }
        }
        Err(EngineError::NotFound(_)) => {
          // Index missing for this field/directory -- skip silently.
          continue;
        }
        Err(e) => return Err(e),
      }
    }
  }

  Ok(())
}

// ---------------------------------------------------------------------------
// Structured search
// ---------------------------------------------------------------------------

/// Run a structured `FieldQuery` in every directory that has the requested
/// field indexed.
fn structured_search(
  engine: &StorageEngine,
  index_manager: &IndexManager,
  indexed_dirs: &[String],
  field_query: &FieldQuery,
  out: &mut Vec<SearchResult>,
) -> EngineResult<()> {
  let query_engine = QueryEngine::new(engine);

  for dir in indexed_dirs {
    // Only search directories that actually index the requested field.
    let indexes = index_manager.list_indexes(dir)?;
    let has_field = indexes.iter().any(|name| {
      name == &field_query.field_name
        || name.starts_with(&format!("{}.", field_query.field_name))
    });
    if !has_field {
      continue;
    }

    let q = Query {
      path: dir.clone(),
      field_queries: vec![],
      node: Some(QueryNode::Field(field_query.clone())),
      limit: None,
      offset: None,
      order_by: vec![],
      after: None,
      before: None,
      include_total: false,
      strategy: QueryStrategy::Auto,
      aggregate: None,
      explain: ExplainMode::Off,
    };

    match query_engine.execute(&q) {
      Ok(qr_results) => {
        for qr in qr_results {
          out.push(query_result_to_search_result(qr, dir, &field_query.field_name));
        }
      }
      Err(EngineError::NotFound(_)) => continue,
      Err(e) => return Err(e),
    }
  }

  Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract field names that have at least one fuzzy-capable index.
/// Index names are "field.strategy" -- we look for strategies:
/// `trigram`, `soundex`, `dmetaphone`, `dmetaphone_alt`.
fn discover_fuzzy_fields(index_names: &[String]) -> Vec<String> {
  const FUZZY_STRATEGIES: &[&str] = &["trigram", "soundex", "dmetaphone", "dmetaphone_alt"];

  let mut fields: Vec<String> = Vec::new();
  for name in index_names {
    // name is "field.strategy" or "field" (legacy)
    if let Some(dot_pos) = name.find('.') {
      let strategy = &name[dot_pos + 1..];
      if FUZZY_STRATEGIES.contains(&strategy) {
        let field = name[..dot_pos].to_string();
        if !fields.contains(&field) {
          fields.push(field);
        }
      }
    }
  }
  fields
}

/// Convert a `QueryResult` from the query engine into a `SearchResult`.
fn query_result_to_search_result(
  qr: QueryResult,
  source_dir: &str,
  matched_field: &str,
) -> SearchResult {
  let mut matched_by = qr.matched_by;
  if matched_by.is_empty() {
    matched_by.push(matched_field.to_string());
  }

  SearchResult {
    path: qr.file_record.path,
    score: qr.score,
    matched_by,
    source_dir: source_dir.to_string(),
    size: qr.file_record.total_size,
    content_type: qr.file_record.content_type,
    created_at: qr.file_record.created_at,
    updated_at: qr.file_record.updated_at,
  }
}

/// Deduplicate results by path, keeping the entry with the highest score.
fn deduplicate_by_path(results: &mut Vec<SearchResult>) {
  let mut best: HashMap<String, usize> = HashMap::new();
  let mut to_remove = Vec::new();

  for (i, result) in results.iter().enumerate() {
    match best.get(&result.path) {
      Some(&existing_idx) => {
        if result.score > results[existing_idx].score {
          to_remove.push(existing_idx);
          best.insert(result.path.clone(), i);
        } else {
          to_remove.push(i);
        }
      }
      None => {
        best.insert(result.path.clone(), i);
      }
    }
  }

  // Sort removal indices in reverse so removals don't shift earlier indices.
  to_remove.sort_unstable();
  to_remove.dedup();
  for idx in to_remove.into_iter().rev() {
    results.swap_remove(idx);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_discover_fuzzy_fields_basic() {
    let names = vec![
      "name.trigram".to_string(),
      "name.string".to_string(),
      "email.soundex".to_string(),
      "email.dmetaphone".to_string(),
      "age.u64".to_string(),
    ];
    let fields = discover_fuzzy_fields(&names);
    assert_eq!(fields, vec!["name".to_string(), "email".to_string()]);
  }

  #[test]
  fn test_discover_fuzzy_fields_empty() {
    let names = vec!["age.u64".to_string(), "score.f64".to_string()];
    let fields = discover_fuzzy_fields(&names);
    assert!(fields.is_empty());
  }

  #[test]
  fn test_discover_fuzzy_fields_no_duplicates() {
    let names = vec![
      "name.trigram".to_string(),
      "name.soundex".to_string(),
      "name.dmetaphone".to_string(),
      "name.dmetaphone_alt".to_string(),
    ];
    let fields = discover_fuzzy_fields(&names);
    assert_eq!(fields, vec!["name".to_string()]);
  }

  #[test]
  fn test_discover_fuzzy_fields_legacy_format() {
    // Legacy format "field" (no dot) should not be treated as fuzzy.
    let names = vec!["name".to_string()];
    let fields = discover_fuzzy_fields(&names);
    assert!(fields.is_empty());
  }

  #[test]
  fn test_deduplicate_by_path_keeps_highest_score() {
    let mut results = vec![
      SearchResult {
        path: "/a".to_string(),
        score: 0.5,
        matched_by: vec!["trigram".to_string()],
        source_dir: "/d1".to_string(),
        size: 10,
        content_type: None,
        created_at: 0,
        updated_at: 0,
      },
      SearchResult {
        path: "/a".to_string(),
        score: 0.9,
        matched_by: vec!["soundex".to_string()],
        source_dir: "/d2".to_string(),
        size: 10,
        content_type: None,
        created_at: 0,
        updated_at: 0,
      },
      SearchResult {
        path: "/b".to_string(),
        score: 0.7,
        matched_by: vec!["trigram".to_string()],
        source_dir: "/d1".to_string(),
        size: 20,
        content_type: None,
        created_at: 0,
        updated_at: 0,
      },
    ];
    deduplicate_by_path(&mut results);
    assert_eq!(results.len(), 2);
    // The "/a" entry with score 0.9 should survive.
    let a_result = results.iter().find(|r| r.path == "/a").unwrap();
    assert!((a_result.score - 0.9).abs() < f64::EPSILON);
  }

  #[test]
  fn test_deduplicate_by_path_no_duplicates() {
    let mut results = vec![
      SearchResult {
        path: "/x".to_string(),
        score: 1.0,
        matched_by: vec![],
        source_dir: "/d".to_string(),
        size: 0,
        content_type: None,
        created_at: 0,
        updated_at: 0,
      },
    ];
    deduplicate_by_path(&mut results);
    assert_eq!(results.len(), 1);
  }

  #[test]
  fn test_deduplicate_by_path_empty() {
    let mut results: Vec<SearchResult> = vec![];
    deduplicate_by_path(&mut results);
    assert!(results.is_empty());
  }
}
