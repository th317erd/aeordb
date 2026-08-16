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

fn digest_sha2<D: Digest + Default>(parts: &[&[u8]]) -> Vec<u8> {
  let mut hasher = D::default();
  for part in parts {
    hasher.update(part);
  }
  hasher.finalize().to_vec()
}
