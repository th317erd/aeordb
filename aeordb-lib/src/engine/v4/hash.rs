use sha2::{Digest, Sha256, Sha512};
use sha3::{Sha3_256, Sha3_512};

use crate::engine::HashAlgorithm;

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
