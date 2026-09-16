use sha2::{Digest, Sha256, Sha512};
use sha3::{Sha3_256, Sha3_512};

use crate::engine::HashAlgorithm;

// Clone execution owns only one digest at a time; keeping it inline avoids an
// unaccounted heap allocation in the bounded migration path.
#[allow(clippy::large_enum_variant)]
pub(crate) enum IncrementalDigestV1 {
  Blake3(blake3::Hasher),
  Sha256(Sha256),
  Sha512(Sha512),
  Sha3_256(Sha3_256),
  Sha3_512(Sha3_512),
}

impl IncrementalDigestV1 {
  pub(crate) fn new(algorithm: HashAlgorithm) -> Self {
    match algorithm {
      HashAlgorithm::Blake3_256 => Self::Blake3(blake3::Hasher::new()),
      HashAlgorithm::Sha256 => Self::Sha256(Sha256::default()),
      HashAlgorithm::Sha512 => Self::Sha512(Sha512::default()),
      HashAlgorithm::Sha3_256 => Self::Sha3_256(Sha3_256::default()),
      HashAlgorithm::Sha3_512 => Self::Sha3_512(Sha3_512::default()),
    }
  }

  pub(crate) fn update(&mut self, bytes: &[u8]) {
    match self {
      Self::Blake3(hasher) => {
        hasher.update(bytes);
      }
      Self::Sha256(hasher) => Digest::update(hasher, bytes),
      Self::Sha512(hasher) => Digest::update(hasher, bytes),
      Self::Sha3_256(hasher) => Digest::update(hasher, bytes),
      Self::Sha3_512(hasher) => Digest::update(hasher, bytes),
    }
  }

  pub(crate) fn finalize(self) -> Vec<u8> {
    match self {
      Self::Blake3(hasher) => hasher.finalize().as_bytes().to_vec(),
      Self::Sha256(hasher) => hasher.finalize().to_vec(),
      Self::Sha512(hasher) => hasher.finalize().to_vec(),
      Self::Sha3_256(hasher) => hasher.finalize().to_vec(),
      Self::Sha3_512(hasher) => hasher.finalize().to_vec(),
    }
  }

  /// Finalize without an infallible allocation for the owned digest. Existing
  /// callers retain their original API; bounded owners can report refusal.
  pub(crate) fn try_finalize(self) -> Result<Vec<u8>, std::collections::TryReserveError> {
    fn copy_digest(bytes: &[u8]) -> Result<Vec<u8>, std::collections::TryReserveError> {
      let mut output = Vec::new();
      output.try_reserve_exact(bytes.len())?;
      output.extend_from_slice(bytes);
      Ok(output)
    }
    match self {
      Self::Blake3(hasher) => copy_digest(hasher.finalize().as_bytes()),
      Self::Sha256(hasher) => copy_digest(&hasher.finalize()),
      Self::Sha512(hasher) => copy_digest(&hasher.finalize()),
      Self::Sha3_256(hasher) => copy_digest(&hasher.finalize()),
      Self::Sha3_512(hasher) => copy_digest(&hasher.finalize()),
    }
  }
}

pub fn digest_parts(algorithm: HashAlgorithm, parts: &[&[u8]]) -> Vec<u8> {
  match algorithm {
    HashAlgorithm::Blake3_256 => {
      let mut hasher = blake3::Hasher::new();
      for part in parts {
        hasher.update(part);
      }
      hasher.finalize().as_bytes().to_vec()
    }
    HashAlgorithm::Sha256 => digest_sha2::<Sha256>(parts),
    HashAlgorithm::Sha512 => digest_sha2::<Sha512>(parts),
    HashAlgorithm::Sha3_256 => digest_sha2::<Sha3_256>(parts),
    HashAlgorithm::Sha3_512 => digest_sha2::<Sha3_512>(parts),
  }
}

/// Same selected-algorithm digest, with fallible ownership of its output.
/// The fixed-size hash state/final digest stay on the stack; the returned
/// vector reserves its exact registered width before any bytes are appended.
pub(crate) fn try_digest_parts(algorithm: HashAlgorithm, parts: &[&[u8]]) -> Result<Vec<u8>, std::collections::TryReserveError> {
  let mut output = Vec::new();
  output.try_reserve_exact(algorithm.hash_length())?;
  match algorithm {
    HashAlgorithm::Blake3_256 => {
      let mut hasher = blake3::Hasher::new();
      for part in parts {
        hasher.update(part);
      }
      output.extend_from_slice(hasher.finalize().as_bytes());
    }
    HashAlgorithm::Sha256 => append_digest::<Sha256>(parts, &mut output),
    HashAlgorithm::Sha512 => append_digest::<Sha512>(parts, &mut output),
    HashAlgorithm::Sha3_256 => append_digest::<Sha3_256>(parts, &mut output),
    HashAlgorithm::Sha3_512 => append_digest::<Sha3_512>(parts, &mut output),
  }
  Ok(output)
}

fn append_digest<D: Digest + Default>(parts: &[&[u8]], output: &mut Vec<u8>) {
  let mut hasher = D::default();
  for part in parts {
    hasher.update(part);
  }
  output.extend_from_slice(&hasher.finalize());
}

fn digest_sha2<D: Digest + Default>(parts: &[&[u8]]) -> Vec<u8> {
  let mut hasher = D::default();
  for part in parts {
    hasher.update(part);
  }
  hasher.finalize().to_vec()
}
