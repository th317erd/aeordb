use aeordb::engine::fuzzy::{extract_trigrams, trigram_similarity, auto_fuzziness};
use aeordb::engine::scalar_converter::{
  ScalarConverter, TrigramConverter, CONVERTER_TYPE_TRIGRAM,
  serialize_converter, deserialize_converter,
};
use aeordb::engine::index_config::{IndexFieldConfig, create_converter_from_config};
use aeordb::engine::index_store::FieldIndex;

// ============================================================================
// Trigram extraction tests
// ============================================================================

#[test]
fn test_extract_trigrams_basic() {
  // "hello" -> pad "  hello " -> ["  h", " he", "hel", "ell", "llo", "lo "]
  let trigrams = extract_trigrams("hello");
  let strs: Vec<String> = trigrams.iter().map(|t| String::from_utf8(t.clone()).unwrap()).collect();
  assert_eq!(strs, vec!["  h", " he", "hel", "ell", "llo", "lo "]);
}

#[test]
fn test_extract_trigrams_short_string() {
  // "ab" -> pad "  ab " -> ["  a", " ab", "ab "]
  let trigrams = extract_trigrams("ab");
  let strs: Vec<String> = trigrams.iter().map(|t| String::from_utf8(t.clone()).unwrap()).collect();
  assert_eq!(strs, vec!["  a", " ab", "ab "]);
}

#[test]
fn test_extract_trigrams_single_char() {
  // "a" -> pad "  a " -> ["  a", " a "]
  let trigrams = extract_trigrams("a");
  let strs: Vec<String> = trigrams.iter().map(|t| String::from_utf8(t.clone()).unwrap()).collect();
  assert_eq!(strs, vec!["  a", " a "]);
}

#[test]
fn test_extract_trigrams_empty() {
  let trigrams = extract_trigrams("");
  assert!(trigrams.is_empty());
}

#[test]
fn test_extract_trigrams_case_insensitive() {
  let upper = extract_trigrams("Hello");
  let lower = extract_trigrams("hello");
  assert_eq!(upper, lower);
}

#[test]
fn test_extract_trigrams_word_boundaries() {
  // "foo-bar" splits into ["foo", "bar"]
  let trigrams = extract_trigrams("foo-bar");
  let strs: Vec<String> = trigrams.iter().map(|t| String::from_utf8(t.clone()).unwrap()).collect();

  // Should contain trigrams from "foo" and "bar"
  assert!(strs.contains(&"  f".to_string()));
  assert!(strs.contains(&" fo".to_string()));
  assert!(strs.contains(&"foo".to_string()));
  assert!(strs.contains(&"oo ".to_string()));
  assert!(strs.contains(&"  b".to_string()));
  assert!(strs.contains(&" ba".to_string()));
  assert!(strs.contains(&"bar".to_string()));
  assert!(strs.contains(&"ar ".to_string()));
}

#[test]
fn test_extract_trigrams_unicode() {
  // "cafe" with accent: operates on codepoints
  let trigrams = extract_trigrams("cafe\u{0301}");
  // After lowercase, "cafe\u{0301}" stays the same (already lowercase)
  // Non-alphanumeric split: combining accent is not alphanumeric, so splits into ["caf", "\u{0301}"]
  // Actually, let's just verify we get trigrams and they are valid UTF-8
  assert!(!trigrams.is_empty());
  for t in &trigrams {
    assert!(std::str::from_utf8(t).is_ok(), "trigram should be valid UTF-8");
  }

  // Test with pre-composed character
  let trigrams2 = extract_trigrams("\u{00e9}toile");
  assert!(!trigrams2.is_empty());
  for t in &trigrams2 {
    assert!(std::str::from_utf8(t).is_ok(), "trigram should be valid UTF-8");
  }
}

#[test]
fn test_extract_trigrams_deduplication() {
  // "aa aa" -> two words "aa" and "aa" -> same trigrams, should be deduplicated
  let trigrams = extract_trigrams("aa aa");
  let strs: Vec<String> = trigrams.iter().map(|t| String::from_utf8(t.clone()).unwrap()).collect();

  // Count occurrences of each trigram - all should appear exactly once
  let unique: std::collections::HashSet<&String> = strs.iter().collect();
  assert_eq!(strs.len(), unique.len(), "trigrams should be deduplicated");
}

#[test]
fn test_extract_trigrams_only_spaces() {
  // Whitespace-only: no alphanumeric words
  let trigrams = extract_trigrams("   ");
  assert!(trigrams.is_empty());
}

#[test]
fn test_extract_trigrams_numbers() {
  // "abc123" is one word (all alphanumeric)
  let trigrams = extract_trigrams("abc123");
  let strs: Vec<String> = trigrams.iter().map(|t| String::from_utf8(t.clone()).unwrap()).collect();
  assert!(strs.contains(&"  a".to_string()));
  assert!(strs.contains(&" ab".to_string()));
  assert!(strs.contains(&"abc".to_string()));
  assert!(strs.contains(&"bc1".to_string()));
  assert!(strs.contains(&"c12".to_string()));
  assert!(strs.contains(&"123".to_string()));
  assert!(strs.contains(&"23 ".to_string()));
}

// ============================================================================
// Similarity tests
// ============================================================================

#[test]
fn test_trigram_similarity_identical() {
  let sim = trigram_similarity("hello", "hello");
  assert!((sim - 1.0).abs() < f64::EPSILON, "identical strings should have similarity 1.0, got {}", sim);
}

#[test]
fn test_trigram_similarity_completely_different() {
  let sim = trigram_similarity("xyz", "abc");
  // These share "  " prefix trigrams but the full trigrams differ
  // "xyz" -> ["  x", " xy", "xyz", "yz "]
  // "abc" -> ["  a", " ab", "abc", "bc "]
  // No overlap
  assert_eq!(sim, 0.0, "completely different strings should have similarity 0.0, got {}", sim);
}

#[test]
fn test_trigram_similarity_partial() {
  let sim = trigram_similarity("hello", "help");
  assert!(sim > 0.0, "partial overlap should be > 0");
  assert!(sim < 1.0, "partial overlap should be < 1");
}

#[test]
fn test_trigram_similarity_case_insensitive() {
  let sim = trigram_similarity("Hello", "hello");
  assert!((sim - 1.0).abs() < f64::EPSILON, "case-insensitive should match, got {}", sim);
}

#[test]
fn test_trigram_similarity_empty_strings() {
  let sim = trigram_similarity("", "");
  assert_eq!(sim, 0.0);
}

#[test]
fn test_trigram_similarity_one_empty() {
  let sim = trigram_similarity("hello", "");
  assert_eq!(sim, 0.0);
  let sim2 = trigram_similarity("", "hello");
  assert_eq!(sim2, 0.0);
}

#[test]
fn test_trigram_similarity_symmetric() {
  let ab = trigram_similarity("hello", "world");
  let ba = trigram_similarity("world", "hello");
  assert!((ab - ba).abs() < f64::EPSILON, "similarity should be symmetric: {} vs {}", ab, ba);

  let ab2 = trigram_similarity("testing", "test");
  let ba2 = trigram_similarity("test", "testing");
  assert!((ab2 - ba2).abs() < f64::EPSILON, "similarity should be symmetric: {} vs {}", ab2, ba2);
}

#[test]
fn test_trigram_similarity_known_value() {
  // "hello" trigrams: ["  h", " he", "hel", "ell", "llo", "lo "] = 6
  // "help" trigrams:  ["  h", " he", "hel", "elp", "lp "] = 5
  // Intersection: {"  h", " he", "hel"} = 3
  // Dice: 2*3 / (6+5) = 6/11
  let sim = trigram_similarity("hello", "help");
  let expected = 6.0 / 11.0;
  assert!(
    (sim - expected).abs() < 1e-10,
    "expected {}, got {}",
    expected,
    sim
  );
}

// ============================================================================
// Auto fuzziness tests
// ============================================================================

#[test]
fn test_auto_fuzziness_short() {
  assert_eq!(auto_fuzziness(0), 0);
  assert_eq!(auto_fuzziness(1), 0);
  assert_eq!(auto_fuzziness(2), 0);
}

#[test]
fn test_auto_fuzziness_medium() {
  assert_eq!(auto_fuzziness(3), 1);
  assert_eq!(auto_fuzziness(4), 1);
  assert_eq!(auto_fuzziness(5), 1);
}

#[test]
fn test_auto_fuzziness_long() {
  assert_eq!(auto_fuzziness(6), 2);
  assert_eq!(auto_fuzziness(7), 2);
  assert_eq!(auto_fuzziness(100), 2);
}

// ============================================================================
// Converter tests
// ============================================================================

#[test]
fn test_trigram_converter_expand_value() {
  let converter = TrigramConverter;
  let expanded = converter.expand_value(b"hello");
  assert!(!expanded.is_empty(), "expand_value should produce trigrams");

  let strs: Vec<String> = expanded.iter().map(|t| String::from_utf8(t.clone()).unwrap()).collect();
  assert!(strs.contains(&"  h".to_string()));
  assert!(strs.contains(&" he".to_string()));
  assert!(strs.contains(&"hel".to_string()));
  assert!(strs.contains(&"ell".to_string()));
  assert!(strs.contains(&"llo".to_string()));
  assert!(strs.contains(&"lo ".to_string()));
}

#[test]
fn test_trigram_converter_to_scalar_range() {
  let converter = TrigramConverter;

  // Test with various trigram values
  for trigram in &["  h", " he", "hel", "ell", "llo", "lo ", "abc", "xyz"] {
    let scalar = converter.to_scalar(trigram.as_bytes());
    assert!(
      scalar >= 0.0 && scalar <= 1.0,
      "scalar {} out of [0,1] for trigram '{}'",
      scalar,
      trigram
    );
  }
}

#[test]
fn test_trigram_converter_to_scalar_deterministic() {
  let converter = TrigramConverter;
  let s1 = converter.to_scalar(b"hel");
  let s2 = converter.to_scalar(b"hel");
  assert_eq!(s1, s2, "same input should produce same scalar");
}

#[test]
fn test_trigram_converter_to_scalar_different_inputs() {
  let converter = TrigramConverter;
  let s1 = converter.to_scalar(b"hel");
  let s2 = converter.to_scalar(b"xyz");
  // Extremely unlikely to collide with blake3
  assert_ne!(s1, s2, "different inputs should produce different scalars");
}

#[test]
fn test_trigram_converter_strategy() {
  let converter = TrigramConverter;
  assert_eq!(converter.strategy(), "trigram");
}

#[test]
fn test_trigram_converter_serialize_deserialize() {
  let converter = TrigramConverter;
  let data = serialize_converter(&converter);
  assert_eq!(data, vec![CONVERTER_TYPE_TRIGRAM]);

  let deserialized = deserialize_converter(&data).expect("should deserialize");
  assert_eq!(deserialized.name(), "trigram");
  assert_eq!(deserialized.type_tag(), CONVERTER_TYPE_TRIGRAM);
  assert_eq!(deserialized.strategy(), "trigram");
  assert!(!deserialized.is_order_preserving());

  // Verify round-trip produces same scalar
  let original_scalar = converter.to_scalar(b"test");
  let deserialized_scalar = deserialized.to_scalar(b"test");
  assert_eq!(original_scalar, deserialized_scalar);
}

#[test]
fn test_trigram_converter_recommended_buckets() {
  let converter = TrigramConverter;
  assert_eq!(converter.recommended_bucket_count(), 4096);
}

#[test]
fn test_trigram_converter_not_order_preserving() {
  let converter = TrigramConverter;
  assert!(!converter.is_order_preserving());
}

#[test]
fn test_create_converter_from_config_trigram() {
  let config = IndexFieldConfig {
    field_name: "name".to_string(),
    converter_type: "trigram".to_string(),
    min: None,
    max: None,
  };
  let converter = create_converter_from_config(&config).expect("should create trigram converter");
  assert_eq!(converter.name(), "trigram");
  assert_eq!(converter.strategy(), "trigram");
  assert_eq!(converter.type_tag(), CONVERTER_TYPE_TRIGRAM);
}

// ============================================================================
// Integration tests (using actual engine structures)
// ============================================================================

#[test]
fn test_trigram_index_store_and_load() {
  let converter: Box<dyn ScalarConverter> = Box::new(TrigramConverter);
  let mut index = FieldIndex::new("name".to_string(), converter);

  assert!(index.is_empty());

  // Insert a value using insert_expanded
  let file_hash = vec![0xAA; 32];
  index.insert_expanded(b"hello", file_hash.clone());

  // "hello" should produce 6 trigrams, so 6 entries
  assert_eq!(
    index.len(),
    6,
    "expected 6 entries (one per trigram), got {}",
    index.len()
  );

  // All entries should reference the same file hash
  for entry in &index.entries {
    assert_eq!(entry.file_hash, file_hash);
    assert!(
      entry.scalar >= 0.0 && entry.scalar <= 1.0,
      "scalar {} out of range",
      entry.scalar
    );
  }

  // Serialize and deserialize round-trip
  let hash_length = 32;
  let data = index.serialize(hash_length);
  let restored = FieldIndex::deserialize(&data, hash_length).expect("should deserialize");
  assert_eq!(restored.len(), 6);
  assert_eq!(restored.field_name, "name");
  assert_eq!(restored.converter.name(), "trigram");
}

#[test]
fn test_trigram_index_multiple_documents() {
  let converter: Box<dyn ScalarConverter> = Box::new(TrigramConverter);
  let mut index = FieldIndex::new("title".to_string(), converter);

  let hash1 = vec![0x01; 32];
  let hash2 = vec![0x02; 32];
  let hash3 = vec![0x03; 32];

  index.insert_expanded(b"hello", hash1.clone());
  index.insert_expanded(b"help", hash2.clone());
  index.insert_expanded(b"world", hash3.clone());

  // "hello" = 6 trigrams, "help" = 5 trigrams, "world" = 7 trigrams = 18 total
  // But some trigrams from "hello" and "help" overlap ("  h", " he", "hel")
  // However insert_expanded inserts every trigram including duplicates across documents
  // so we should get 6 + 5 + 7 = 18 entries
  // Wait - "world" -> pad "  world " -> ["  w", " wo", "wor", "orl", "rld", "ld "] = 6
  // So total = 6 + 5 + 6 = 17
  let expected_hello = 6; // "  h", " he", "hel", "ell", "llo", "lo "
  let expected_help = 5; // "  h", " he", "hel", "elp", "lp "
  let expected_world = 6; // "  w", " wo", "wor", "orl", "rld", "ld "
  let expected_total = expected_hello + expected_help + expected_world;

  assert_eq!(
    index.len(),
    expected_total,
    "expected {} entries, got {}",
    expected_total,
    index.len()
  );

  // Verify entries reference correct hashes
  let hash1_entries: Vec<_> = index.entries.iter().filter(|e| e.file_hash == hash1).collect();
  assert_eq!(hash1_entries.len(), expected_hello);

  let hash2_entries: Vec<_> = index.entries.iter().filter(|e| e.file_hash == hash2).collect();
  assert_eq!(hash2_entries.len(), expected_help);

  let hash3_entries: Vec<_> = index.entries.iter().filter(|e| e.file_hash == hash3).collect();
  assert_eq!(hash3_entries.len(), expected_world);
}

// ============================================================================
// Edge case / failure path tests
// ============================================================================

#[test]
fn test_trigram_converter_expand_empty_value() {
  let converter = TrigramConverter;
  let expanded = converter.expand_value(b"");
  assert!(expanded.is_empty(), "empty value should produce no trigrams");
}

#[test]
fn test_trigram_converter_expand_invalid_utf8() {
  let converter = TrigramConverter;
  // Invalid UTF-8 bytes - should gracefully fall back to empty string
  let expanded = converter.expand_value(&[0xFF, 0xFE, 0xFD]);
  assert!(expanded.is_empty(), "invalid UTF-8 should produce no trigrams");
}

#[test]
fn test_trigram_converter_to_scalar_empty_input() {
  let converter = TrigramConverter;
  // blake3 can hash empty input - should still produce a valid scalar
  let scalar = converter.to_scalar(b"");
  assert!(scalar >= 0.0 && scalar <= 1.0, "empty input scalar {} out of range", scalar);
}

#[test]
fn test_trigram_index_remove_document() {
  let converter: Box<dyn ScalarConverter> = Box::new(TrigramConverter);
  let mut index = FieldIndex::new("name".to_string(), converter);

  let hash1 = vec![0x01; 32];
  let hash2 = vec![0x02; 32];

  index.insert_expanded(b"hello", hash1.clone());
  index.insert_expanded(b"world", hash2.clone());

  let total_before = index.len();
  assert!(total_before > 0);

  // Remove hash1
  index.remove(&hash1);
  assert_eq!(index.len(), total_before - 6, "should have removed 6 entries for 'hello'");

  // All remaining should be hash2
  for entry in &index.entries {
    assert_eq!(entry.file_hash, hash2);
  }
}

#[test]
fn test_trigram_index_rebuild_nvt() {
  let converter: Box<dyn ScalarConverter> = Box::new(TrigramConverter);
  let mut index = FieldIndex::new("name".to_string(), converter);

  index.insert_expanded(b"hello world", vec![0xAA; 32]);
  assert!(index.is_dirty());

  index.ensure_nvt_current();
  assert!(!index.is_dirty());
}

#[test]
fn test_deserialize_converter_unknown_type() {
  let data = vec![0xFF]; // Unknown type tag
  let result = deserialize_converter(&data);
  assert!(result.is_err(), "unknown type tag should produce error");
}

#[test]
fn test_deserialize_converter_empty_data() {
  let result = deserialize_converter(&[]);
  assert!(result.is_err(), "empty data should produce error");
}

#[test]
fn test_extract_trigrams_special_characters_only() {
  // All non-alphanumeric -> no words -> empty
  let trigrams = extract_trigrams("---!!!@@@");
  assert!(trigrams.is_empty());
}

#[test]
fn test_extract_trigrams_mixed_word_separators() {
  // Multiple separator types
  let trigrams = extract_trigrams("a.b-c_d");
  let strs: Vec<String> = trigrams.iter().map(|t| String::from_utf8(t.clone()).unwrap()).collect();
  // Each single char produces 2 trigrams: "  x" and " x "
  // 4 words * 2 trigrams each, but "  a" type trigrams are unique per letter
  assert_eq!(strs.len(), 8, "4 single-char words x 2 trigrams each");
}

#[test]
fn test_trigram_similarity_near_match() {
  // "test" vs "tset" (transposition)
  let sim = trigram_similarity("test", "tset");
  assert!(sim > 0.0, "transposition should have some similarity");
  assert!(sim < 1.0, "transposition should not be identical");
}

#[test]
fn test_trigram_similarity_substring() {
  // "test" is a substring of "testing"
  let sim = trigram_similarity("test", "testing");
  assert!(sim > 0.5, "substring should have high similarity, got {}", sim);
}
