use std::collections::{BTreeSet, HashMap};

use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::path_utils::{normalize_path, parent_path};
use crate::engine::query_engine::{
  FieldQuery, Query, QueryEngine, QueryNode, QueryOp, QueryReadSourceV1, QueryResult, ExplainMode, QueryStrategy, DEFAULT_QUERY_LIMIT,
};
use crate::engine::query_runtime::QueryRequestBudget;
use crate::engine::storage_engine::StorageEngine;

/// A single result from a global search, enriched with source metadata.
#[derive(Debug)]
pub struct SearchResult {
  /// Exact selected FileRecord revision for root-aware pagination and fetch continuity.
  pub record_revision: Vec<u8>,
  /// Full file path.
  pub path: String,
  /// Relevance score (higher is better).
  pub score: f64,
  /// Names of the indexes/strategies that produced this match.
  pub matched_by: Vec<String>,
  /// Indexed field names that should be inspected for opt-in locator generation.
  pub matched_fields: Vec<String>,
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
  where_clause: Option<&QueryNode>,
  limit: Option<usize>,
  offset: Option<usize>,
) -> EngineResult<SearchResults> {
  let request_budget = engine.start_query_request_budget()?;
  global_search_with_budget(engine, base_path, query, where_clause, limit, offset, &request_budget)
}

pub(crate) fn global_search_with_budget(
  engine: &StorageEngine,
  base_path: &str,
  query: Option<&str>,
  where_clause: Option<&QueryNode>,
  limit: Option<usize>,
  offset: Option<usize>,
  request_budget: &QueryRequestBudget,
) -> EngineResult<SearchResults> {
  let query_engine = QueryEngine::with_request_budget(engine, request_budget.clone());
  global_search_with_query_engine(&query_engine, base_path, query, where_clause, limit, offset, &mut |_result| Ok(true))
}

pub(crate) fn global_search_all_with_source_and_budget<F>(
  engine: &StorageEngine,
  read_source: &dyn QueryReadSourceV1,
  base_path: &str,
  query: Option<&str>,
  where_clause: Option<&QueryNode>,
  request_budget: &QueryRequestBudget,
  mut filter: F,
) -> EngineResult<Vec<SearchResult>>
where
  F: FnMut(&SearchResult) -> EngineResult<bool>,
{
  let query_engine = QueryEngine::with_read_source_and_budget(engine, read_source, request_budget.clone());
  collect_global_search_results(&query_engine, base_path, query, where_clause, &mut filter)
}

fn global_search_with_query_engine(
  query_engine: &QueryEngine<'_>,
  base_path: &str,
  query: Option<&str>,
  where_clause: Option<&QueryNode>,
  limit: Option<usize>,
  offset: Option<usize>,
  filter: &mut dyn FnMut(&SearchResult) -> EngineResult<bool>,
) -> EngineResult<SearchResults> {
  let all_results = collect_global_search_results(query_engine, base_path, query, where_clause, filter)?;
  let total_count = all_results.len();
  let mut effective_offset = 0;
  if let Some(requested_offset) = offset {
    effective_offset = requested_offset;
  }
  let effective_limit = match limit {
    Some(limit) => limit,
    None => DEFAULT_QUERY_LIMIT,
  };

  let page: Vec<SearchResult> = all_results.into_iter().skip(effective_offset).take(effective_limit.saturating_add(1)).collect();

  let has_more = page.len() > effective_limit;
  let results: Vec<SearchResult> = page.into_iter().take(effective_limit).collect();

  Ok(SearchResults { results, has_more, total_count: Some(total_count) })
}

fn collect_global_search_results(
  query_engine: &QueryEngine<'_>,
  base_path: &str,
  query: Option<&str>,
  where_clause: Option<&QueryNode>,
  filter: &mut dyn FnMut(&SearchResult) -> EngineResult<bool>,
) -> EngineResult<Vec<SearchResult>> {
  // Discover all directories that have indexes. Include indexed ancestors so a
  // root glob index can satisfy a search scoped to `/some/subtree`.
  let indexed_dirs = discover_indexed_directories_for_base(query_engine, base_path)?;

  if indexed_dirs.is_empty() {
    return Ok(Vec::new());
  }

  // Collect raw results across every directory.
  let mut all_results: Vec<SearchResult> = Vec::new();

  if let Some(query_str) = query {
    // Broad search: search fuzzy-capable indexes in every directory.
    broad_search(query_engine, &indexed_dirs, query_str, &mut all_results)?;
    if let Some(query_node) = where_clause {
      let mut structured_results = Vec::new();
      structured_search(query_engine, &indexed_dirs, query_node, &mut structured_results)?;
      structured_results.sort_by(|left, right| left.path.cmp(&right.path));
      all_results.retain(|result| {
        let index = structured_results.partition_point(|structured| structured.path.as_str() < result.path.as_str());
        match structured_results.get(index) {
          Some(structured) => structured.path == result.path,
          None => false,
        }
      });
    }
  } else if let Some(query_node) = where_clause {
    // Structured search: delegate to QueryEngine per directory.
    structured_search(query_engine, &indexed_dirs, query_node, &mut all_results)?;
  } else {
    // Neither query nor where_clause provided -- nothing to search.
    return Ok(Vec::new());
  }

  let normalized_base_path = normalize_path(base_path);
  all_results.retain(|result| path_is_under_base(&result.path, &normalized_base_path));
  let mut filter_error = None;
  all_results.retain(|result| {
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
  if let Some(error) = filter_error {
    return Err(error);
  }
  validate_search_result_scores(&all_results)?;

  // Deduplicate by path, keeping the highest score for each.
  deduplicate_by_path(&mut all_results);

  // Sort by score descending (ties broken by path for determinism).
  all_results.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.path.cmp(&b.path)));

  Ok(all_results)
}

fn discover_indexed_directories_for_base(query_engine: &QueryEngine<'_>, base_path: &str) -> EngineResult<Vec<String>> {
  let mut dirs: BTreeSet<String> = query_engine.discover_source_indexed_directories(base_path)?.into_iter().collect();
  let normalized = normalize_path(base_path);
  // The base scope is always a legal authoritative-evaluation candidate. This
  // matters when a selected legacy root cannot retain a detached derived index
  // directory, and for virtual fields whose exact values live in FileRecords.
  dirs.insert(normalized.clone());
  let mut current = Some(normalized);

  while let Some(dir) = current {
    if !query_engine.list_source_indexes(&dir)?.is_empty() {
      dirs.insert(dir.clone());
    }
    if dir == "/" {
      break;
    }
    current = parent_path(&dir);
  }

  Ok(dirs.into_iter().collect())
}

fn path_is_under_base(path: &str, base_path: &str) -> bool {
  if base_path == "/" {
    return true;
  }

  let normalized_path = normalize_path(path);
  let normalized_base = normalize_path(base_path);
  if normalized_path == normalized_base {
    return true;
  }

  let prefix = format!("{}/", normalized_base.trim_end_matches('/'));
  normalized_path.starts_with(&prefix)
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
fn broad_search(query_engine: &QueryEngine<'_>, indexed_dirs: &[String], query_str: &str, out: &mut Vec<SearchResult>) -> EngineResult<()> {
  for dir in indexed_dirs {
    let indexes = query_engine.list_source_indexes(dir)?;
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
        node: Some(QueryNode::Field(FieldQuery { field_name: field_name.clone(), operation: QueryOp::Match(query_str.to_string()) })),
        // No per-directory cap — global_search paginates after every
        // directory has contributed all its hits. `limit: None` would
        // silently get rewritten to `DEFAULT_QUERY_LIMIT = 20` by
        // QueryEngine::execute, so an explicit usize::MAX is required
        // to actually mean "everything that matched".
        limit: Some(usize::MAX),
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
            out.push(query_result_to_search_result(qr, dir, std::slice::from_ref(field_name)));
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

/// Run a structured `QueryNode` in every discovered indexed directory.
fn structured_search(
  query_engine: &QueryEngine<'_>,
  indexed_dirs: &[String],
  query_node: &QueryNode,
  out: &mut Vec<SearchResult>,
) -> EngineResult<()> {
  let matched_fields = query_node_field_names(query_node);

  for dir in indexed_dirs {
    let q = Query {
      path: dir.clone(),
      field_queries: vec![],
      node: Some(query_node.clone()),
      // Same caveat as in broad_search: `None` would mean 20-result cap.
      limit: Some(usize::MAX),
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
          out.push(query_result_to_search_result(qr, dir, &matched_fields));
        }
      }
      Err(EngineError::NotFound(_)) => continue,
      Err(e) => return Err(e),
    }
  }

  Ok(())
}

fn query_node_field_names(node: &QueryNode) -> Vec<String> {
  let mut fields = BTreeSet::new();
  collect_query_node_field_names(node, &mut fields);
  fields.into_iter().collect()
}

fn collect_query_node_field_names(node: &QueryNode, out: &mut BTreeSet<String>) {
  match node {
    QueryNode::Field(field_query) => {
      out.insert(canonical_result_field_name(&field_query.field_name).to_string());
    }
    QueryNode::And(children) | QueryNode::Or(children) => {
      for child in children {
        collect_query_node_field_names(child, out);
      }
    }
    QueryNode::Not(child) => collect_query_node_field_names(child, out),
  }
}

fn canonical_result_field_name(field_name: &str) -> &str {
  match field_name {
    "@file_name" => "@filename",
    other => other,
  }
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
fn query_result_to_search_result(qr: QueryResult, source_dir: &str, fallback_matched_by: &[String]) -> SearchResult {
  let record_revision = qr.file_hash;
  let mut matched_by = qr.matched_by;
  if matched_by.is_empty() {
    if fallback_matched_by.is_empty() {
      matched_by.push("structured".to_string());
    } else {
      matched_by.extend_from_slice(fallback_matched_by);
    }
  }

  SearchResult {
    record_revision,
    path: qr.file_record.path,
    score: qr.score,
    matched_by,
    matched_fields: fallback_matched_by.to_vec(),
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

  for i in 0..results.len() {
    let path = results[i].path.clone();
    match best.get(&path).copied() {
      Some(existing_idx) => {
        let incoming_matched_by = results[i].matched_by.clone();
        let incoming_matched_fields = results[i].matched_fields.clone();
        let existing_matched_by = results[existing_idx].matched_by.clone();
        let existing_matched_fields = results[existing_idx].matched_fields.clone();
        if results[i].score > results[existing_idx].score {
          append_unique(&mut results[i].matched_by, existing_matched_by);
          append_unique(&mut results[i].matched_fields, existing_matched_fields);
          to_remove.push(existing_idx);
          best.insert(path, i);
        } else {
          append_unique(&mut results[existing_idx].matched_by, incoming_matched_by);
          append_unique(&mut results[existing_idx].matched_fields, incoming_matched_fields);
          to_remove.push(i);
        }
      }
      None => {
        best.insert(path, i);
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

fn validate_search_result_scores(results: &[SearchResult]) -> EngineResult<()> {
  if let Some(result) = results.iter().find(|result| !result.score.is_finite()) {
    return Err(EngineError::InvalidInput(format!("search score for '{}' is not finite", result.path)));
  }
  Ok(())
}

fn append_unique(target: &mut Vec<String>, values: Vec<String>) {
  for value in values {
    if !target.contains(&value) {
      target.push(value);
    }
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
    let names =
      vec!["name.trigram".to_string(), "name.soundex".to_string(), "name.dmetaphone".to_string(), "name.dmetaphone_alt".to_string()];
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
        record_revision: vec![1; 32],
        score: 0.5,
        matched_by: vec!["trigram".to_string()],
        matched_fields: vec!["name".to_string()],
        source_dir: "/d1".to_string(),
        size: 10,
        content_type: None,
        created_at: 0,
        updated_at: 0,
      },
      SearchResult {
        path: "/a".to_string(),
        record_revision: vec![2; 32],
        score: 0.9,
        matched_by: vec!["soundex".to_string()],
        matched_fields: vec!["name".to_string()],
        source_dir: "/d2".to_string(),
        size: 10,
        content_type: None,
        created_at: 0,
        updated_at: 0,
      },
      SearchResult {
        path: "/b".to_string(),
        record_revision: vec![3; 32],
        score: 0.7,
        matched_by: vec!["trigram".to_string()],
        matched_fields: vec!["title".to_string()],
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
    let mut results = vec![SearchResult {
      path: "/x".to_string(),
      record_revision: vec![4; 32],
      score: 1.0,
      matched_by: vec![],
      matched_fields: vec![],
      source_dir: "/d".to_string(),
      size: 0,
      content_type: None,
      created_at: 0,
      updated_at: 0,
    }];
    deduplicate_by_path(&mut results);
    assert_eq!(results.len(), 1);
  }

  #[test]
  fn test_deduplicate_by_path_empty() {
    let mut results: Vec<SearchResult> = vec![];
    deduplicate_by_path(&mut results);
    assert!(results.is_empty());
  }

  #[test]
  fn test_search_score_validation_rejects_non_finite_values() {
    let results = vec![SearchResult {
      path: "/invalid".to_string(),
      record_revision: vec![5; 32],
      score: f64::NAN,
      matched_by: vec![],
      matched_fields: vec![],
      source_dir: "/".to_string(),
      size: 0,
      content_type: None,
      created_at: 0,
      updated_at: 0,
    }];
    let error = validate_search_result_scores(&results).unwrap_err();
    assert!(matches!(error, EngineError::InvalidInput(message) if message.contains("finite")));
  }
}
