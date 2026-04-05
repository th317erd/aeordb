use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::fuzzy::{damerau_levenshtein, jaro_winkler};
use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use aeordb::engine::query_engine::{
  FuzzyAlgorithm, FuzzyOptions, Fuzziness, QueryBuilder,
};
use aeordb::engine::storage_engine::StorageEngine;

// =============================================================================
// Helpers
// =============================================================================

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
  let path = dir.path().join("test.aeor");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  let ops = DirectoryOps::new(&engine);
  ops.ensure_root_directory().unwrap();
  engine
}

fn store_index_config(engine: &StorageEngine, parent_path: &str, config: &PathIndexConfig) {
  let ops = DirectoryOps::new(engine);
  let config_path = if parent_path.ends_with('/') {
    format!("{}.config/indexes.json", parent_path)
  } else {
    format!("{}/.config/indexes.json", parent_path)
  };
  let config_data = config.serialize();
  ops
    .store_file(&config_path, &config_data, Some("application/json"))
    .unwrap();
}

fn make_name_json(name: &str) -> Vec<u8> {
  format!(r#"{{"name":"{}"}}"#, name).into_bytes()
}

/// Set up an engine with trigram + soundex + dmetaphone indexes on "name" field.
fn setup_fuzzy_engine(dir: &tempfile::TempDir, names: &[(&str, &str)]) -> StorageEngine {
  let engine = create_engine(dir);
  let ops = DirectoryOps::new(&engine);

  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    indexes: vec![
      IndexFieldConfig {
        name: "name".to_string(),
        index_type: "trigram".to_string(),
        source: None,
        min: None,
        max: None,
      },
      IndexFieldConfig {
        name: "name".to_string(),
        index_type: "soundex".to_string(),
        source: None,
        min: None,
        max: None,
      },
      IndexFieldConfig {
        name: "name".to_string(),
        index_type: "dmetaphone".to_string(),
        source: None,
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/data", &config);

  for (filename, name) in names {
    ops
      .store_file_with_indexing(
        &format!("/data/{}", filename),
        &make_name_json(name),
        Some("application/json"),
      )
      .unwrap();
  }

  engine
}

// =============================================================================
// Task 1: Damerau-Levenshtein algorithm tests
// =============================================================================

#[test]
fn test_damerau_levenshtein_identical() {
  assert_eq!(damerau_levenshtein("hello", "hello"), 0);
}

#[test]
fn test_damerau_levenshtein_one_insert() {
  assert_eq!(damerau_levenshtein("cat", "cats"), 1);
}

#[test]
fn test_damerau_levenshtein_one_delete() {
  assert_eq!(damerau_levenshtein("cats", "cat"), 1);
}

#[test]
fn test_damerau_levenshtein_one_substitute() {
  assert_eq!(damerau_levenshtein("cat", "car"), 1);
}

#[test]
fn test_damerau_levenshtein_one_transpose() {
  assert_eq!(damerau_levenshtein("ab", "ba"), 1);
}

#[test]
fn test_damerau_levenshtein_kitten_sitting() {
  assert_eq!(damerau_levenshtein("kitten", "sitting"), 3);
}

#[test]
fn test_damerau_levenshtein_empty() {
  assert_eq!(damerau_levenshtein("", "abc"), 3);
}

#[test]
fn test_damerau_levenshtein_both_empty() {
  assert_eq!(damerau_levenshtein("", ""), 0);
}

#[test]
fn test_damerau_levenshtein_symmetric() {
  // Distance should be the same in both directions
  assert_eq!(
    damerau_levenshtein("foo", "bar"),
    damerau_levenshtein("bar", "foo")
  );
}

#[test]
fn test_damerau_levenshtein_single_char() {
  assert_eq!(damerau_levenshtein("a", "b"), 1);
  assert_eq!(damerau_levenshtein("a", "a"), 0);
  assert_eq!(damerau_levenshtein("a", ""), 1);
}

#[test]
fn test_damerau_levenshtein_transpose_vs_two_subs() {
  // "ab" → "ba" should be 1 (transpose), not 2 (two substitutions)
  assert_eq!(damerau_levenshtein("ab", "ba"), 1);
  // "abcd" → "badc" should be 2 (two transpositions)
  assert_eq!(damerau_levenshtein("abcd", "badc"), 2);
}

// =============================================================================
// Jaro-Winkler algorithm tests
// =============================================================================

#[test]
fn test_jaro_winkler_identical() {
  let score = jaro_winkler("hello", "hello");
  assert!((score - 1.0).abs() < f64::EPSILON, "Expected 1.0, got {}", score);
}

#[test]
fn test_jaro_winkler_completely_different() {
  let score = jaro_winkler("abc", "xyz");
  assert!(score < 0.5, "Expected close to 0.0, got {}", score);
}

#[test]
fn test_jaro_winkler_martha_marhta() {
  let score = jaro_winkler("MARTHA", "MARHTA");
  assert!(
    (score - 0.961).abs() < 0.01,
    "Expected ~0.961, got {}",
    score
  );
}

#[test]
fn test_jaro_winkler_empty() {
  assert!((jaro_winkler("", "abc") - 0.0).abs() < f64::EPSILON);
  assert!((jaro_winkler("abc", "") - 0.0).abs() < f64::EPSILON);
}

#[test]
fn test_jaro_winkler_both_empty() {
  assert!((jaro_winkler("", "") - 1.0).abs() < f64::EPSILON);
}

#[test]
fn test_jaro_winkler_prefix_bonus() {
  // Two pairs with same Jaro score but different prefix lengths
  // "DWAYNE" vs "DUANE" should get prefix bonus from shared "D"
  let score = jaro_winkler("DWAYNE", "DUANE");
  assert!(score > 0.8, "Expected > 0.8, got {}", score);
}

#[test]
fn test_jaro_winkler_range() {
  // Score must always be in [0.0, 1.0]
  let pairs = [
    ("a", "b"),
    ("abc", "def"),
    ("hello", "world"),
    ("test", "testing"),
    ("", "nonempty"),
    ("same", "same"),
  ];
  for (a, b) in &pairs {
    let score = jaro_winkler(a, b);
    assert!(
      (0.0..=1.0).contains(&score),
      "Score out of range for ({}, {}): {}",
      a,
      b,
      score
    );
  }
}

#[test]
fn test_jaro_winkler_single_char() {
  assert!((jaro_winkler("a", "a") - 1.0).abs() < f64::EPSILON);
  // "a" vs "b" — no matches, should be 0.0
  assert!((jaro_winkler("a", "b") - 0.0).abs() < f64::EPSILON);
}

// =============================================================================
// Integration tests: Contains query
// =============================================================================

#[test]
fn test_contains_query() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_fuzzy_engine(
    &dir,
    &[("hello.json", "hello world"), ("other.json", "goodbye")],
  );

  let results = QueryBuilder::new(&engine, "/data")
    .field("name")
    .contains("world")
    .all()
    .unwrap();

  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_record.path, "/data/hello.json");
  assert!((results[0].score - 1.0).abs() < f64::EPSILON);
}

#[test]
fn test_contains_query_no_match() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_fuzzy_engine(
    &dir,
    &[("hello.json", "hello world"), ("other.json", "goodbye")],
  );

  let results = QueryBuilder::new(&engine, "/data")
    .field("name")
    .contains("xyz")
    .all()
    .unwrap();

  assert_eq!(results.len(), 0);
}

#[test]
fn test_contains_case_insensitive() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_fuzzy_engine(&dir, &[("hello.json", "hello world")]);

  let results = QueryBuilder::new(&engine, "/data")
    .field("name")
    .contains("WORLD")
    .all()
    .unwrap();

  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_record.path, "/data/hello.json");
}

// =============================================================================
// Integration tests: Similar query (trigram similarity)
// =============================================================================

#[test]
fn test_similar_query() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_fuzzy_engine(
    &dir,
    &[("smith.json", "Smith"), ("jones.json", "Jones")],
  );

  let results = QueryBuilder::new(&engine, "/data")
    .field("name")
    .similar("Smyth", 0.2)
    .all()
    .unwrap();

  // "Smith" and "Smyth" share many trigrams, should match
  assert!(!results.is_empty(), "Expected matches for 'Smyth' vs 'Smith'");
  assert!(results[0].score > 0.0);
}

#[test]
fn test_similar_query_below_threshold() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_fuzzy_engine(
    &dir,
    &[("smith.json", "Smith"), ("jones.json", "Jones")],
  );

  let results = QueryBuilder::new(&engine, "/data")
    .field("name")
    .similar("zzzzz", 0.9)
    .all()
    .unwrap();

  assert_eq!(results.len(), 0);
}

// =============================================================================
// Integration tests: Fuzzy query (edit distance / Jaro-Winkler)
// =============================================================================

#[test]
fn test_fuzzy_query_dl() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_fuzzy_engine(&dir, &[("rest.json", "restaurant")]);

  let results = QueryBuilder::new(&engine, "/data")
    .field("name")
    .fuzzy("restarant")
    .all()
    .unwrap();

  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_record.path, "/data/rest.json");
  assert!(results[0].score > 0.5);
}

#[test]
fn test_fuzzy_query_jw() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_fuzzy_engine(&dir, &[("martha.json", "Martha")]);

  let options = FuzzyOptions {
    fuzziness: Fuzziness::Auto,
    algorithm: FuzzyAlgorithm::JaroWinkler,
  };

  let results = QueryBuilder::new(&engine, "/data")
    .field("name")
    .fuzzy_with("Marhta", options)
    .all()
    .unwrap();

  assert_eq!(results.len(), 1);
  assert!(results[0].score > 0.9, "Expected high JW score, got {}", results[0].score);
}

#[test]
fn test_fuzzy_scoring_order() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_fuzzy_engine(
    &dir,
    &[
      ("smith.json", "Smith"),
      ("smythe.json", "Smythe"),
      ("smithson.json", "Smithson"),
    ],
  );

  let results = QueryBuilder::new(&engine, "/data")
    .field("name")
    .similar("Smith", 0.1)
    .all()
    .unwrap();

  // Should have results, sorted by score descending
  assert!(!results.is_empty());
  for i in 1..results.len() {
    assert!(
      results[i - 1].score >= results[i].score,
      "Results not sorted by score: {} < {} at index {}",
      results[i - 1].score,
      results[i].score,
      i,
    );
  }
}

// =============================================================================
// Integration tests: Phonetic query
// =============================================================================

#[test]
fn test_phonetic_query() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_fuzzy_engine(
    &dir,
    &[
      ("smith.json", "Smith"),
      ("schmidt.json", "Schmidt"),
      ("jones.json", "Jones"),
    ],
  );

  let results = QueryBuilder::new(&engine, "/data")
    .field("name")
    .phonetic("Smith")
    .all()
    .unwrap();

  // At minimum "Smith" should match itself via soundex (S530)
  assert!(!results.is_empty(), "Expected phonetic matches for 'Smith'");

  let paths: Vec<&str> = results.iter().map(|r| r.file_record.path.as_str()).collect();
  assert!(
    paths.contains(&"/data/smith.json"),
    "Expected Smith to match itself phonetically"
  );
}

// =============================================================================
// Integration tests: Limit
// =============================================================================

#[test]
fn test_fuzzy_query_with_limit() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_fuzzy_engine(
    &dir,
    &[
      ("a.json", "Smith"),
      ("b.json", "Smyth"),
      ("c.json", "Smithson"),
      ("d.json", "Smithers"),
    ],
  );

  let results = QueryBuilder::new(&engine, "/data")
    .field("name")
    .similar("Smith", 0.1)
    .limit(2)
    .all()
    .unwrap();

  assert!(results.len() <= 2, "Expected at most 2 results, got {}", results.len());
}

// =============================================================================
// Edge cases
// =============================================================================

#[test]
fn test_fuzzy_query_missing_trigram_index() {
  // Engine with no trigram index should return an error for Contains
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Only string index, no trigram
  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    indexes: vec![IndexFieldConfig {
      name: "name".to_string(),
      index_type: "string".to_string(),
        source: None,
      min: None,
      max: None,
    }],
  };
  store_index_config(&engine, "/data", &config);
  ops
    .store_file_with_indexing(
      "/data/test.json",
      &make_name_json("hello"),
      Some("application/json"),
    )
    .unwrap();

  let result = QueryBuilder::new(&engine, "/data")
    .field("name")
    .contains("hello")
    .all();

  assert!(result.is_err(), "Expected error when trigram index is missing");
}

#[test]
fn test_phonetic_query_missing_index() {
  // Engine with no phonetic index should return an error for Phonetic
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    indexes: vec![IndexFieldConfig {
      name: "name".to_string(),
      index_type: "string".to_string(),
        source: None,
      min: None,
      max: None,
    }],
  };
  store_index_config(&engine, "/data", &config);
  ops
    .store_file_with_indexing(
      "/data/test.json",
      &make_name_json("hello"),
      Some("application/json"),
    )
    .unwrap();

  let result = QueryBuilder::new(&engine, "/data")
    .field("name")
    .phonetic("hello")
    .all();

  assert!(result.is_err(), "Expected error when phonetic index is missing");
}

#[test]
fn test_fuzzy_query_empty_results() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_fuzzy_engine(&dir, &[("a.json", "hello")]);

  let options = FuzzyOptions {
    fuzziness: Fuzziness::Fixed(0),
    algorithm: FuzzyAlgorithm::DamerauLevenshtein,
  };

  let results = QueryBuilder::new(&engine, "/data")
    .field("name")
    .fuzzy_with("zzzzz", options)
    .all()
    .unwrap();

  assert_eq!(results.len(), 0);
}

#[test]
fn test_contains_empty_query_string() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_fuzzy_engine(&dir, &[("a.json", "hello world")]);

  // Empty string should match everything (since "" is a substring of any string)
  // but no trigrams are extracted from "", so candidates will be empty
  let results = QueryBuilder::new(&engine, "/data")
    .field("name")
    .contains("")
    .all()
    .unwrap();

  // Empty trigram set → no candidates → no results
  assert_eq!(results.len(), 0);
}

#[test]
fn test_fuzzy_query_exact_match_scores_one() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_fuzzy_engine(&dir, &[("a.json", "hello")]);

  let options = FuzzyOptions {
    fuzziness: Fuzziness::Auto,
    algorithm: FuzzyAlgorithm::DamerauLevenshtein,
  };

  let results = QueryBuilder::new(&engine, "/data")
    .field("name")
    .fuzzy_with("hello", options)
    .all()
    .unwrap();

  assert_eq!(results.len(), 1);
  assert!(
    (results[0].score - 1.0).abs() < f64::EPSILON,
    "Expected score 1.0 for exact match, got {}",
    results[0].score
  );
}

#[test]
fn test_jaro_winkler_exact_match_scores_one() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_fuzzy_engine(&dir, &[("a.json", "hello")]);

  let options = FuzzyOptions {
    fuzziness: Fuzziness::Auto,
    algorithm: FuzzyAlgorithm::JaroWinkler,
  };

  let results = QueryBuilder::new(&engine, "/data")
    .field("name")
    .fuzzy_with("hello", options)
    .all()
    .unwrap();

  assert_eq!(results.len(), 1);
  assert!(
    (results[0].score - 1.0).abs() < f64::EPSILON,
    "Expected score 1.0 for exact match, got {}",
    results[0].score
  );
}

#[test]
fn test_fuzzy_fixed_fuzziness() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_fuzzy_engine(&dir, &[("a.json", "cat"), ("b.json", "car"), ("c.json", "dog")]);

  // Fixed fuzziness of 1 should match "cat" → "car" (1 sub) but not "dog" (3 edits)
  let options = FuzzyOptions {
    fuzziness: Fuzziness::Fixed(1),
    algorithm: FuzzyAlgorithm::DamerauLevenshtein,
  };

  let results = QueryBuilder::new(&engine, "/data")
    .field("name")
    .fuzzy_with("cat", options)
    .all()
    .unwrap();

  let paths: Vec<&str> = results.iter().map(|r| r.file_record.path.as_str()).collect();
  assert!(paths.contains(&"/data/a.json"), "Expected 'cat' exact match");
  assert!(paths.contains(&"/data/b.json"), "Expected 'car' to match within 1 edit");
  assert!(!paths.contains(&"/data/c.json"), "Expected 'dog' NOT to match within 1 edit");
}

#[test]
fn test_phonetic_score_is_one() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_fuzzy_engine(&dir, &[("smith.json", "Smith")]);

  let results = QueryBuilder::new(&engine, "/data")
    .field("name")
    .phonetic("Smith")
    .all()
    .unwrap();

  assert_eq!(results.len(), 1);
  assert!(
    (results[0].score - 1.0).abs() < f64::EPSILON,
    "Expected phonetic match score 1.0, got {}",
    results[0].score
  );
}
