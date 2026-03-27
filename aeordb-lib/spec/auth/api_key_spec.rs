use aeordb::auth::api_key::{generate_api_key, hash_api_key, verify_api_key};

#[test]
fn test_generate_api_key_has_prefix() {
  let key = generate_api_key();
  assert!(
    key.starts_with("aeor_k_"),
    "API key should start with aeor_k_ prefix, got: {}",
    key
  );
}

#[test]
fn test_generate_api_key_correct_length() {
  let key = generate_api_key();
  // "aeor_k_" (7 chars) + 64 hex chars (32 bytes * 2) = 71 chars
  assert_eq!(
    key.len(),
    71,
    "API key should be 71 chars (7 prefix + 64 hex), got: {} (len {})",
    key,
    key.len()
  );
}

#[test]
fn test_generate_api_key_is_unique() {
  let key_a = generate_api_key();
  let key_b = generate_api_key();
  assert_ne!(key_a, key_b, "Two generated keys should not be identical");
}

#[test]
fn test_api_key_hash_and_verify() {
  let key = generate_api_key();
  let hash = hash_api_key(&key).expect("should hash");
  let verified = verify_api_key(&key, &hash).expect("should verify");
  assert!(verified, "correct key should verify against its own hash");
}

#[test]
fn test_invalid_api_key_rejected() {
  let key = generate_api_key();
  let hash = hash_api_key(&key).expect("should hash");

  let wrong_key = "aeor_k_0000000000000000000000000000000000000000000000000000000000000000";
  let verified = verify_api_key(wrong_key, &hash).expect("should not error");
  assert!(!verified, "wrong key should not verify");
}

#[test]
fn test_wrong_api_key_rejected() {
  let key_a = generate_api_key();
  let key_b = generate_api_key();
  let hash_a = hash_api_key(&key_a).expect("should hash");

  let verified = verify_api_key(&key_b, &hash_a).expect("should not error");
  assert!(!verified, "key_b should not verify against key_a's hash");
}

#[test]
fn test_hash_is_not_plaintext() {
  let key = generate_api_key();
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
  let key = generate_api_key();
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
  let key = generate_api_key();
  let result = verify_api_key(&key, "not-a-valid-hash");
  assert!(result.is_err(), "corrupt hash should return error");
}
