use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};

fn make_claims(expiry_offset: i64) -> TokenClaims {
  let now = chrono::Utc::now().timestamp();
  TokenClaims {
    sub: "test-subject".to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + expiry_offset,
    scope: None,
    permissions: None,
    key_id: None,
  }
}

#[test]
fn test_generate_ed25519_keypair() {
  // Should not panic -- just verifying construction works.
  let _manager = JwtManager::generate();
}

#[test]
fn test_sign_and_verify_jwt() {
  let manager = JwtManager::generate();
  let claims = make_claims(3600);

  let token = manager.create_token(&claims).expect("should create token");
  let decoded = manager.verify_token(&token).expect("should verify token");

  assert_eq!(decoded.sub, claims.sub);
  assert_eq!(decoded.iss, claims.iss);
}

#[test]
fn test_expired_jwt_rejected() {
  let manager = JwtManager::generate();
  let claims = make_claims(-120); // Already expired 2 minutes ago (past default leeway)

  let token = manager.create_token(&claims).expect("should create token");
  let result = manager.verify_token(&token);

  assert!(result.is_err(), "expired token should be rejected");
}

#[test]
fn test_tampered_jwt_rejected() {
  let manager = JwtManager::generate();
  let claims = make_claims(3600);

  let token = manager.create_token(&claims).expect("should create token");

  // Tamper with the token by modifying a character in the signature
  let mut tampered = token.clone();
  let last_char = tampered.pop().unwrap();
  let replacement = if last_char == 'A' { 'B' } else { 'A' };
  tampered.push(replacement);

  let result = manager.verify_token(&tampered);
  assert!(result.is_err(), "tampered token should be rejected");
}

#[test]
fn test_wrong_issuer_rejected() {
  let manager = JwtManager::generate();
  let now = chrono::Utc::now().timestamp();

  // Create claims with wrong issuer
  let claims = TokenClaims {
    sub: "test-subject".to_string(),
    iss: "not-aeordb".to_string(),
    iat: now,
    exp: now + 3600,
    scope: None,
    permissions: None,
    key_id: None,
  };

  let token = manager.create_token(&claims).expect("should create token");
  let result = manager.verify_token(&token);
  assert!(result.is_err(), "wrong issuer should be rejected");
}

#[test]
fn test_jwt_contains_correct_claims() {
  let manager = JwtManager::generate();
  let now = chrono::Utc::now().timestamp();

  let claims = TokenClaims {
    sub: "user-42".to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + 3600,
    scope: None,
    permissions: None,
    key_id: None,
  };

  let token = manager.create_token(&claims).expect("should create token");
  let decoded = manager.verify_token(&token).expect("should verify token");

  assert_eq!(decoded.sub, "user-42");
  assert_eq!(decoded.iss, "aeordb");
  assert_eq!(decoded.iat, now);
  assert_eq!(decoded.exp, now + 3600);
  assert_eq!(decoded.scope, None);
  assert_eq!(decoded.permissions, None);
}

#[test]
fn test_jwt_default_expiry() {
  assert_eq!(DEFAULT_EXPIRY_SECONDS, 3600);
}

#[test]
fn test_different_keypairs_cannot_verify_each_others_tokens() {
  let manager_a = JwtManager::generate();
  let manager_b = JwtManager::generate();

  let claims = make_claims(3600);
  let token = manager_a.create_token(&claims).expect("should create token");

  let result = manager_b.verify_token(&token);
  assert!(result.is_err(), "token from manager_a should not verify with manager_b");
}

#[test]
fn test_empty_token_rejected() {
  let manager = JwtManager::generate();
  let result = manager.verify_token("");
  assert!(result.is_err(), "empty token should be rejected");
}

#[test]
fn test_garbage_token_rejected() {
  let manager = JwtManager::generate();
  let result = manager.verify_token("not.a.jwt.at.all");
  assert!(result.is_err(), "garbage token should be rejected");
}

#[test]
fn test_to_bytes_and_from_bytes_roundtrip() {
  let manager = JwtManager::generate();
  let claims = make_claims(3600);
  let token = manager.create_token(&claims).expect("should create token");

  let key_bytes = manager.to_bytes();
  let restored_manager = JwtManager::from_bytes(&key_bytes).expect("should restore from bytes");

  let decoded = restored_manager.verify_token(&token).expect("restored manager should verify token");
  assert_eq!(decoded.sub, claims.sub);
}

#[test]
fn test_from_bytes_with_invalid_length_fails() {
  let result = JwtManager::from_bytes(&[0u8; 16]);
  assert!(result.is_err(), "invalid key length should fail");
}

#[test]
fn test_scope_and_permissions_serialized_when_present() {
  let manager = JwtManager::generate();
  let now = chrono::Utc::now().timestamp();

  let claims = TokenClaims {
    sub: "scoped-user".to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + 3600,
    scope: Some("read write".to_string()),
    permissions: Some(vec!["docs:read".to_string(), "docs:write".to_string()]),
    key_id: None,
  };

  let token = manager.create_token(&claims).expect("should create token");
  let decoded = manager.verify_token(&token).expect("should verify token");

  assert_eq!(decoded.scope, Some("read write".to_string()));
  assert_eq!(
    decoded.permissions,
    Some(vec!["docs:read".to_string(), "docs:write".to_string()])
  );
}
