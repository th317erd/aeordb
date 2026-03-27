use aeordb::auth::api_key::{generate_api_key, hash_api_key, parse_api_key, verify_api_key};
use uuid::Uuid;

#[test]
fn test_generate_api_key_has_prefix() {
  let key_id = Uuid::new_v4();
  let key = generate_api_key(key_id);
  assert!(
    key.starts_with("aeor_k_"),
    "API key should start with aeor_k_ prefix, got: {}",
    key
  );
}

#[test]
fn test_generate_api_key_correct_format() {
  let key_id = Uuid::new_v4();
  let key = generate_api_key(key_id);
  // "aeor_k_" (7 chars) + 16 hex key_id + "_" (1 char) + 64 hex chars = 88 chars
  assert_eq!(
    key.len(),
    88,
    "API key should be 88 chars (7 prefix + 16 key_id + 1 sep + 64 hex), got: {} (len {})",
    key,
    key.len()
  );
}

#[test]
fn test_generate_api_key_embeds_key_id_prefix() {
  let key_id = Uuid::new_v4();
  let key = generate_api_key(key_id);
  let key_id_prefix = &key_id.simple().to_string()[..16];
  let expected_prefix = format!("aeor_k_{}_", key_id_prefix);
  assert!(
    key.starts_with(&expected_prefix),
    "API key should embed key_id prefix. Expected prefix: {}, got: {}",
    expected_prefix,
    key
  );
}

#[test]
fn test_generate_api_key_is_unique() {
  let key_a = generate_api_key(Uuid::new_v4());
  let key_b = generate_api_key(Uuid::new_v4());
  assert_ne!(key_a, key_b, "Two generated keys should not be identical");
}

#[test]
fn test_parse_api_key_extracts_key_id() {
  let key_id = Uuid::new_v4();
  let key = generate_api_key(key_id);
  let (parsed_prefix, parsed_full) = parse_api_key(&key).expect("should parse");

  let expected_prefix = &key_id.simple().to_string()[..16];
  assert_eq!(parsed_prefix, expected_prefix);
  assert_eq!(parsed_full, key);
}

#[test]
fn test_parse_api_key_rejects_missing_prefix() {
  let result = parse_api_key("invalid_key_format");
  assert!(result.is_err(), "should reject key without aeor_k_ prefix");
}

#[test]
fn test_parse_api_key_rejects_missing_separator() {
  let result = parse_api_key("aeor_k_noseparatorhere");
  assert!(result.is_err(), "should reject key without key_id separator");
}

#[test]
fn test_parse_api_key_rejects_short_key_id() {
  let result = parse_api_key("aeor_k_short_rest");
  assert!(result.is_err(), "should reject key with short key_id prefix");
}

#[test]
fn test_api_key_hash_and_verify() {
  let key = generate_api_key(Uuid::new_v4());
  let hash = hash_api_key(&key).expect("should hash");
  let verified = verify_api_key(&key, &hash).expect("should verify");
  assert!(verified, "correct key should verify against its own hash");
}

#[test]
fn test_invalid_api_key_rejected() {
  let key = generate_api_key(Uuid::new_v4());
  let hash = hash_api_key(&key).expect("should hash");

  let wrong_key = generate_api_key(Uuid::new_v4());
  let verified = verify_api_key(&wrong_key, &hash).expect("should not error");
  assert!(!verified, "wrong key should not verify");
}

#[test]
fn test_wrong_api_key_rejected() {
  let key_a = generate_api_key(Uuid::new_v4());
  let key_b = generate_api_key(Uuid::new_v4());
  let hash_a = hash_api_key(&key_a).expect("should hash");

  let verified = verify_api_key(&key_b, &hash_a).expect("should not error");
  assert!(!verified, "key_b should not verify against key_a's hash");
}

#[test]
fn test_hash_is_not_plaintext() {
  let key = generate_api_key(Uuid::new_v4());
  let hash = hash_api_key(&key).expect("should hash");
  assert_ne!(hash, key, "hash should not equal the plaintext key");
  assert!(
    hash.starts_with("$argon2"),
    "hash should be an argon2 formatted string, got: {}",
    hash
  );
}

#[test]
fn test_same_key_produces_different_hashes() {
  let key = generate_api_key(Uuid::new_v4());
  let hash_a = hash_api_key(&key).expect("should hash");
  let hash_b = hash_api_key(&key).expect("should hash");
  // Different salts mean different hashes
  assert_ne!(hash_a, hash_b, "same key should produce different hashes due to random salts");

  // But both should still verify
  assert!(verify_api_key(&key, &hash_a).unwrap());
  assert!(verify_api_key(&key, &hash_b).unwrap());
}

#[test]
fn test_verify_with_corrupt_hash_returns_error() {
  let key = generate_api_key(Uuid::new_v4());
  let result = verify_api_key(&key, "not-a-valid-hash");
  assert!(result.is_err(), "corrupt hash should return error");
}
