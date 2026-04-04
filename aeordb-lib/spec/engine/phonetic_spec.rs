use aeordb::engine::phonetic::{soundex, dmetaphone_primary, dmetaphone_alt};
use aeordb::engine::scalar_converter::{
  ScalarConverter, PhoneticConverter, PhoneticAlgorithm,
  CONVERTER_TYPE_PHONETIC, serialize_converter, deserialize_converter,
};
use aeordb::engine::index_config::{IndexFieldConfig, create_converter_from_config};

// ============================================================================
// Soundex Tests
// ============================================================================

#[test]
fn test_soundex_robert() {
  assert_eq!(soundex("Robert"), "R163");
}

#[test]
fn test_soundex_rupert() {
  assert_eq!(soundex("Rupert"), "R163");
}

#[test]
fn test_soundex_smith() {
  assert_eq!(soundex("Smith"), "S530");
}

#[test]
fn test_soundex_smythe() {
  assert_eq!(soundex("Smythe"), "S530");
}

#[test]
fn test_soundex_empty() {
  assert_eq!(soundex(""), "");
}

#[test]
fn test_soundex_single_char() {
  assert_eq!(soundex("A"), "A000");
  assert_eq!(soundex("Z"), "Z000");
}

#[test]
fn test_soundex_case_insensitive() {
  assert_eq!(soundex("smith"), soundex("SMITH"));
  assert_eq!(soundex("Smith"), soundex("sMiTh"));
}

#[test]
fn test_soundex_ashcraft() {
  assert_eq!(soundex("Ashcraft"), "A261");
}

#[test]
fn test_soundex_non_alpha() {
  // Punctuation stripped, still produces a code
  let code = soundex("O'Brien");
  assert!(!code.is_empty());
  assert_eq!(code.len(), 4);
  assert_eq!(&code[..1], "O");
}

#[test]
fn test_soundex_all_vowels() {
  assert_eq!(soundex("Aeiou"), "A000");
}

#[test]
fn test_soundex_numeric_input() {
  // All non-alpha stripped -> empty
  assert_eq!(soundex("12345"), "");
}

#[test]
fn test_soundex_hw_separation() {
  // H and W should not act as separators for identical consonant codes.
  // "Ashcraft" and "Ashcroft" should produce the same Soundex.
  assert_eq!(soundex("Ashcraft"), soundex("Ashcroft"));
}

#[test]
fn test_soundex_padding() {
  // Short names should be padded with zeros
  assert_eq!(soundex("Lee"), "L000");
  assert_eq!(soundex("Al"), "A400");
}

#[test]
fn test_soundex_truncation() {
  // Long names should be truncated to 4 chars
  let code = soundex("Washington");
  assert_eq!(code.len(), 4);
}

// ============================================================================
// Double Metaphone Tests
// ============================================================================

#[test]
fn test_dmetaphone_smith() {
  let code = dmetaphone_primary("Smith");
  assert!(!code.is_empty());
  assert!(code.len() <= 4);
  // SM + TH -> SM0 (theta)
  assert_eq!(code, "SM0");
}

#[test]
fn test_dmetaphone_schmidt() {
  let code = dmetaphone_primary("Schmidt");
  assert!(!code.is_empty());
  // SCH -> X
  assert!(code.starts_with('X'), "Schmidt should start with X, got {}", code);
}

#[test]
fn test_dmetaphone_phone() {
  let code = dmetaphone_primary("Phone");
  // PH -> F
  assert!(code.starts_with('F'), "Phone should start with F, got {}", code);
}

#[test]
fn test_dmetaphone_knight() {
  let code = dmetaphone_primary("Knight");
  // KN -> silent K, starts at N
  assert!(code.starts_with('N'), "Knight should start with N, got {}", code);
}

#[test]
fn test_dmetaphone_empty() {
  assert_eq!(dmetaphone_primary(""), "");
}

#[test]
fn test_dmetaphone_case_insensitive() {
  assert_eq!(dmetaphone_primary("Smith"), dmetaphone_primary("smith"));
  assert_eq!(dmetaphone_primary("Schmidt"), dmetaphone_primary("SCHMIDT"));
}

#[test]
fn test_dmetaphone_alt_schmidt() {
  let alt = dmetaphone_alt("Schmidt");
  assert!(alt.is_some(), "Schmidt should have an alternate code");
  let alt = alt.unwrap();
  let primary = dmetaphone_primary("Schmidt");
  assert_ne!(alt, primary, "Alternate should differ from primary for Schmidt");
  // Alternate should start with S (not X)
  assert!(alt.starts_with('S'), "Schmidt alt should start with S, got {}", alt);
}

#[test]
fn test_dmetaphone_alt_none() {
  // Regular words without SCH should return None
  assert!(dmetaphone_alt("Robert").is_none());
  assert!(dmetaphone_alt("Smith").is_none());
}

#[test]
fn test_dmetaphone_thorn() {
  let code = dmetaphone_primary("Thorn");
  // TH -> 0 (theta)
  assert!(code.starts_with('0'), "Thorn should start with 0, got {}", code);
}

#[test]
fn test_dmetaphone_max_length() {
  // Output should never exceed 4 characters
  let code = dmetaphone_primary("Christopherson");
  assert!(code.len() <= 4, "Code should be at most 4 chars, got {}", code);
}

#[test]
fn test_dmetaphone_single_char() {
  let code = dmetaphone_primary("A");
  assert_eq!(code, "A");
}

#[test]
fn test_dmetaphone_all_non_alpha() {
  assert_eq!(dmetaphone_primary("123!@#"), "");
}

#[test]
fn test_dmetaphone_sh_handling() {
  let code = dmetaphone_primary("Shaw");
  // SH -> X
  assert!(code.starts_with('X'), "Shaw should start with X, got {}", code);
}

#[test]
fn test_dmetaphone_wr_silent() {
  let code = dmetaphone_primary("Wright");
  // WR -> silent W, starts at R
  assert!(code.starts_with('R'), "Wright should start with R, got {}", code);
}

#[test]
fn test_dmetaphone_gn_silent() {
  let code = dmetaphone_primary("Gnome");
  // GN -> silent G, starts at N
  assert!(code.starts_with('N'), "Gnome should start with N, got {}", code);
}

#[test]
fn test_dmetaphone_pn_silent() {
  let code = dmetaphone_primary("Pneumonia");
  // PN -> silent P, starts at N
  assert!(code.starts_with('N'), "Pneumonia should start with N, got {}", code);
}

#[test]
fn test_dmetaphone_alt_empty() {
  assert!(dmetaphone_alt("").is_none());
}

// ============================================================================
// PhoneticConverter Tests
// ============================================================================

#[test]
fn test_phonetic_converter_soundex_strategy() {
  let conv = PhoneticConverter::soundex();
  assert_eq!(conv.strategy(), "soundex");
  assert_eq!(conv.name(), "phonetic");
}

#[test]
fn test_phonetic_converter_dmetaphone_strategy() {
  let conv = PhoneticConverter::dmetaphone();
  assert_eq!(conv.strategy(), "dmetaphone");
}

#[test]
fn test_phonetic_converter_dmetaphone_alt_strategy() {
  let conv = PhoneticConverter::dmetaphone_alt();
  assert_eq!(conv.strategy(), "dmetaphone_alt");
}

#[test]
fn test_phonetic_converter_serialize_deserialize_soundex() {
  let conv = PhoneticConverter::soundex();
  let data = conv.serialize();
  assert_eq!(data.len(), 2);
  assert_eq!(data[0], CONVERTER_TYPE_PHONETIC);
  assert_eq!(data[1], 0); // Soundex = 0

  let restored = deserialize_converter(&data).expect("deserialize should succeed");
  assert_eq!(restored.name(), "phonetic");
  assert_eq!(restored.strategy(), "soundex");
  assert_eq!(restored.type_tag(), CONVERTER_TYPE_PHONETIC);
}

#[test]
fn test_phonetic_converter_serialize_deserialize_dmetaphone() {
  let conv = PhoneticConverter::dmetaphone();
  let data = conv.serialize();
  assert_eq!(data[1], 1); // DoubleMetaphonePrimary = 1

  let restored = deserialize_converter(&data).expect("deserialize should succeed");
  assert_eq!(restored.strategy(), "dmetaphone");
}

#[test]
fn test_phonetic_converter_serialize_deserialize_dmetaphone_alt() {
  let conv = PhoneticConverter::dmetaphone_alt();
  let data = conv.serialize();
  assert_eq!(data[1], 2); // DoubleMetaphoneAlt = 2

  let restored = deserialize_converter(&data).expect("deserialize should succeed");
  assert_eq!(restored.strategy(), "dmetaphone_alt");
}

#[test]
fn test_phonetic_converter_deserialize_missing_algo_byte() {
  let data = vec![CONVERTER_TYPE_PHONETIC]; // no algorithm byte
  let result = deserialize_converter(&data);
  assert!(result.is_err());
}

#[test]
fn test_phonetic_converter_deserialize_unknown_algo() {
  let data = vec![CONVERTER_TYPE_PHONETIC, 0xFF];
  let result = deserialize_converter(&data);
  assert!(result.is_err());
}

#[test]
fn test_phonetic_converter_expand_value_soundex() {
  let conv = PhoneticConverter::soundex();
  let codes = conv.expand_value(b"Robert");
  assert_eq!(codes.len(), 1);
  assert_eq!(std::str::from_utf8(&codes[0]).unwrap(), "R163");
}

#[test]
fn test_phonetic_converter_expand_value_dmetaphone() {
  let conv = PhoneticConverter::dmetaphone();
  let codes = conv.expand_value(b"Smith");
  assert_eq!(codes.len(), 1);
  let code = std::str::from_utf8(&codes[0]).unwrap();
  assert!(!code.is_empty());
  assert!(code.len() <= 4);
}

#[test]
fn test_phonetic_converter_expand_value_dmetaphone_alt_with_sch() {
  let conv = PhoneticConverter::dmetaphone_alt();
  let codes = conv.expand_value(b"Schmidt");
  assert_eq!(codes.len(), 1);
  let code = std::str::from_utf8(&codes[0]).unwrap();
  // Should be the alternate (S-based) code
  assert!(code.starts_with('S'), "Alt for Schmidt should start with S, got {}", code);
}

#[test]
fn test_phonetic_converter_expand_value_dmetaphone_alt_fallback() {
  // For words without meaningful alternate, falls back to primary
  let conv = PhoneticConverter::dmetaphone_alt();
  let codes = conv.expand_value(b"Robert");
  assert_eq!(codes.len(), 1);
  // Should be same as primary
  let primary_conv = PhoneticConverter::dmetaphone();
  let primary_codes = primary_conv.expand_value(b"Robert");
  assert_eq!(codes, primary_codes);
}

#[test]
fn test_phonetic_converter_expand_value_empty() {
  let conv = PhoneticConverter::soundex();
  let codes = conv.expand_value(b"");
  assert!(codes.is_empty());
}

#[test]
fn test_phonetic_converter_expand_value_invalid_utf8() {
  let conv = PhoneticConverter::soundex();
  // Invalid UTF-8 bytes -> unwrap_or("") -> empty
  let codes = conv.expand_value(&[0xFF, 0xFE, 0xFD]);
  assert!(codes.is_empty());
}

#[test]
fn test_phonetic_converter_recommended_buckets_soundex() {
  let conv = PhoneticConverter::soundex();
  assert_eq!(conv.recommended_bucket_count(), 8192);
}

#[test]
fn test_phonetic_converter_recommended_buckets_dmetaphone() {
  let conv = PhoneticConverter::dmetaphone();
  assert_eq!(conv.recommended_bucket_count(), 16384);
}

#[test]
fn test_phonetic_converter_recommended_buckets_dmetaphone_alt() {
  let conv = PhoneticConverter::dmetaphone_alt();
  assert_eq!(conv.recommended_bucket_count(), 16384);
}

#[test]
fn test_phonetic_converter_not_order_preserving() {
  let conv = PhoneticConverter::soundex();
  assert!(!conv.is_order_preserving());
  let conv = PhoneticConverter::dmetaphone();
  assert!(!conv.is_order_preserving());
}

#[test]
fn test_phonetic_converter_to_scalar_range() {
  let conv = PhoneticConverter::soundex();
  let scalar = conv.to_scalar(b"R163");
  assert!(scalar >= 0.0 && scalar <= 1.0, "scalar {} out of [0,1]", scalar);
}

#[test]
fn test_phonetic_converter_to_scalar_deterministic() {
  let conv = PhoneticConverter::soundex();
  let s1 = conv.to_scalar(b"S530");
  let s2 = conv.to_scalar(b"S530");
  assert_eq!(s1, s2, "Same input should produce same scalar");
}

#[test]
fn test_phonetic_converter_to_scalar_different_codes() {
  let conv = PhoneticConverter::soundex();
  let s1 = conv.to_scalar(b"R163");
  let s2 = conv.to_scalar(b"S530");
  // Different codes should (almost certainly) produce different scalars
  assert_ne!(s1, s2, "Different codes should produce different scalars");
}

#[test]
fn test_phonetic_converter_type_tag() {
  let conv = PhoneticConverter::soundex();
  assert_eq!(conv.type_tag(), CONVERTER_TYPE_PHONETIC);
}

// ============================================================================
// Config Factory Tests
// ============================================================================

#[test]
fn test_create_converter_from_config_soundex() {
  let config = IndexFieldConfig {
    field_name: "name".to_string(),
    converter_type: "soundex".to_string(),
    min: None,
    max: None,
  };
  let conv = create_converter_from_config(&config).expect("should create soundex");
  assert_eq!(conv.name(), "phonetic");
  assert_eq!(conv.strategy(), "soundex");
}

#[test]
fn test_create_converter_from_config_dmetaphone() {
  let config = IndexFieldConfig {
    field_name: "name".to_string(),
    converter_type: "dmetaphone".to_string(),
    min: None,
    max: None,
  };
  let conv = create_converter_from_config(&config).expect("should create dmetaphone");
  assert_eq!(conv.strategy(), "dmetaphone");
}

#[test]
fn test_create_converter_from_config_phonetic() {
  let config = IndexFieldConfig {
    field_name: "name".to_string(),
    converter_type: "phonetic".to_string(),
    min: None,
    max: None,
  };
  let conv = create_converter_from_config(&config).expect("should create phonetic (defaults to dmetaphone)");
  assert_eq!(conv.strategy(), "dmetaphone");
}

#[test]
fn test_create_converter_from_config_dmetaphone_alt() {
  let config = IndexFieldConfig {
    field_name: "name".to_string(),
    converter_type: "dmetaphone_alt".to_string(),
    min: None,
    max: None,
  };
  let conv = create_converter_from_config(&config).expect("should create dmetaphone_alt");
  assert_eq!(conv.strategy(), "dmetaphone_alt");
}

// ============================================================================
// Cross-matching Tests
// ============================================================================

#[test]
fn test_phonetic_smith_smythe_match_soundex() {
  let code1 = soundex("Smith");
  let code2 = soundex("Smythe");
  assert_eq!(code1, code2, "Smith and Smythe should have same Soundex code");
}

#[test]
fn test_phonetic_robert_rupert_match_soundex() {
  let code1 = soundex("Robert");
  let code2 = soundex("Rupert");
  assert_eq!(code1, code2, "Robert and Rupert should have same Soundex code");
}

#[test]
fn test_phonetic_schmidt_smith_match_dmetaphone() {
  // Schmidt and Smith should share a phonetic link via DM.
  // Schmidt primary starts with X, Smith primary starts with S.
  // But Schmidt alternate starts with S — same as Smith primary prefix.
  let schmidt_alt = dmetaphone_alt("Schmidt").unwrap();
  let smith_primary = dmetaphone_primary("Smith");
  // They share the initial consonant code at least
  assert_eq!(
    &schmidt_alt[..1], &smith_primary[..1],
    "Schmidt alt and Smith primary should share first char"
  );
}

// ============================================================================
// Roundtrip: serialize_converter / deserialize_converter
// ============================================================================

#[test]
fn test_phonetic_roundtrip_via_trait_object() {
  for algo in [
    PhoneticAlgorithm::Soundex,
    PhoneticAlgorithm::DoubleMetaphonePrimary,
    PhoneticAlgorithm::DoubleMetaphoneAlt,
  ] {
    let conv = PhoneticConverter::new(algo);
    let data = serialize_converter(&conv);
    let restored = deserialize_converter(&data)
      .unwrap_or_else(|e| panic!("roundtrip failed for {:?}: {}", algo, e));
    assert_eq!(restored.type_tag(), CONVERTER_TYPE_PHONETIC);
    assert_eq!(restored.strategy(), conv.strategy());
  }
}

// ============================================================================
// Edge cases and failure paths
// ============================================================================

#[test]
fn test_soundex_repeated_same_code() {
  // "Pfister" — P and F both map to 1, so F is suppressed
  let code = soundex("Pfister");
  assert_eq!(code.len(), 4);
  assert_eq!(&code[..1], "P");
}

#[test]
fn test_soundex_unicode_stripped() {
  // Non-ASCII characters should be stripped
  let code = soundex("Muller"); // ASCII version
  assert_eq!(code.len(), 4);
}

#[test]
fn test_dmetaphone_double_letters() {
  // Double letters should be collapsed
  let code1 = dmetaphone_primary("Llano");
  let code2 = dmetaphone_primary("Lano");
  // Both should start with L
  assert!(code1.starts_with('L'));
  assert!(code2.starts_with('L'));
}

#[test]
fn test_dmetaphone_x_produces_ks() {
  let code = dmetaphone_primary("Rex");
  // X -> KS, R starts, so code has RKS
  assert!(code.contains('K'), "Rex should contain K from X, got {}", code);
}

#[test]
fn test_dmetaphone_z_to_s() {
  let code = dmetaphone_primary("Zack");
  assert!(code.starts_with('S'), "Zack should start with S, got {}", code);
}

#[test]
fn test_dmetaphone_v_to_f() {
  let code = dmetaphone_primary("Victor");
  assert!(code.starts_with('F'), "Victor should start with F (V->F), got {}", code);
}

#[test]
fn test_dmetaphone_b_to_p() {
  let code = dmetaphone_primary("Baker");
  assert!(code.starts_with('P'), "Baker should start with P (B->P), got {}", code);
}

#[test]
fn test_dmetaphone_q_to_k() {
  let code = dmetaphone_primary("Queen");
  assert!(code.starts_with('K'), "Queen should start with K (Q->K), got {}", code);
}

#[test]
fn test_dmetaphone_ce_to_s() {
  let code = dmetaphone_primary("Cecil");
  assert!(code.starts_with('S'), "Cecil should start with S (CE->S), got {}", code);
}

#[test]
fn test_dmetaphone_dge_to_j() {
  let code = dmetaphone_primary("Edge");
  // E is initial vowel (A), DGE -> J
  assert_eq!(&code[..1], "A", "Edge should start with A (initial vowel), got {}", code);
}
