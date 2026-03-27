use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::EncodePrivateKey;
use jsonwebtoken::{
  Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode,
};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};

/// Default token expiry in seconds (1 hour).
pub const DEFAULT_EXPIRY_SECONDS: i64 = 3600;

/// JWT claims payload for aeordb tokens.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TokenClaims {
  pub sub: String,
  pub iss: String,
  pub iat: i64,
  pub exp: i64,
  pub roles: Vec<String>,
}

/// Manages JWT signing and verification using Ed25519 (EdDSA).
pub struct JwtManager {
  encoding_key: EncodingKey,
  decoding_key: DecodingKey,
}

impl JwtManager {
  /// Generate a new Ed25519 keypair and return a JwtManager.
  pub fn generate() -> Self {
    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);

    // jsonwebtoken (ring-backed) expects PKCS#8 DER for the signing key
    let pkcs8_der = signing_key
      .to_pkcs8_der()
      .expect("failed to encode Ed25519 key to PKCS#8 DER");
    let encoding_key = EncodingKey::from_ed_der(pkcs8_der.as_bytes());

    // The verifying (public) key is raw 32 bytes for ring
    let verifying_key = signing_key.verifying_key();
    let decoding_key = DecodingKey::from_ed_der(verifying_key.as_bytes());

    Self {
      encoding_key,
      decoding_key,
    }
  }

  /// Create a signed JWT from the given claims.
  pub fn create_token(&self, claims: &TokenClaims) -> Result<String, jsonwebtoken::errors::Error> {
    let header = Header::new(Algorithm::EdDSA);
    encode(&header, claims, &self.encoding_key)
  }

  /// Verify and decode a JWT, returning the claims if valid.
  pub fn verify_token(&self, token: &str) -> Result<TokenClaims, jsonwebtoken::errors::Error> {
    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.set_issuer(&["aeordb"]);
    validation.set_required_spec_claims(&["exp", "iat", "iss", "sub"]);

    let token_data = decode::<TokenClaims>(token, &self.decoding_key, &validation)?;
    Ok(token_data.claims)
  }
}
