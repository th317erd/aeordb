//! Test-only whole-set Patricia oracle; writes independent frozen bytes.
use std::collections::BTreeMap;
use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::namespace::EncodedSemanticObjectV1;
use sha2::Digest;

#[derive(Clone, Debug)]
pub struct Binding {
  pub kind: u16,
  pub owner: Vec<u8>,
  pub semantic: Vec<u8>,
  pub definition: Vec<u8>,
}

pub fn digest(algorithm: HashAlgorithm, bytes: &[u8]) -> Vec<u8> {
  match algorithm {
    HashAlgorithm::Blake3_256 => blake3::hash(bytes).as_bytes().to_vec(),
    HashAlgorithm::Sha256 => sha2::Sha256::digest(bytes).to_vec(),
    HashAlgorithm::Sha512 => sha2::Sha512::digest(bytes).to_vec(),
    HashAlgorithm::Sha3_256 => sha3::Sha3_256::digest(bytes).to_vec(),
    HashAlgorithm::Sha3_512 => sha3::Sha3_512::digest(bytes).to_vec(),
  }
}

pub fn lookup(algorithm: HashAlgorithm, binding: &Binding) -> Vec<u8> {
  digest(algorithm, &[b"aeordb.semantic-catalog-key.v1\0".as_slice(), &binding.kind.to_le_bytes(), &binding.owner].concat())
}

fn envelope(algorithm: HashAlgorithm, kind: u16, count: u64, body: Vec<u8>) -> EncodedSemanticObjectV1 {
  let mut bytes = vec![0; 32];
  bytes[..4].copy_from_slice(b"ASEM");
  bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
  bytes[6..8].copy_from_slice(&kind.to_le_bytes());
  bytes[8..10].copy_from_slice(&32u16.to_le_bytes());
  bytes[12..16].copy_from_slice(&((36 + body.len()) as u32).to_le_bytes());
  bytes[16..20].copy_from_slice(&(body.len() as u32).to_le_bytes());
  bytes[20..28].copy_from_slice(&count.to_le_bytes());
  bytes.extend_from_slice(&body);
  bytes.extend_from_slice(&crc32fast::hash(&bytes).to_le_bytes());
  let object_id = digest(algorithm, &[b"aeordb.semantic-object.immutable.v1\0".as_slice(), &kind.to_le_bytes(), &bytes].concat());
  EncodedSemanticObjectV1 { object_id, value: bytes }
}

pub fn oracle(algorithm: HashAlgorithm, bindings: &[Binding], depth: usize) -> (EncodedSemanticObjectV1, u64) {
  assert!(!bindings.is_empty());
  let width = algorithm.hash_length();
  if bindings.len() == 1 {
    let binding = &bindings[0];
    let record_length = 8 + 2 * width + binding.owner.len();
    let mut body = vec![0; 16 + width];
    body[4..8].copy_from_slice(&1u32.to_le_bytes());
    body[8..8 + width].copy_from_slice(&lookup(algorithm, binding));
    body[8 + width..12 + width].copy_from_slice(&(record_length as u32).to_le_bytes());
    body.extend_from_slice(&binding.kind.to_le_bytes());
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(&(binding.owner.len() as u32).to_le_bytes());
    body.extend_from_slice(&binding.semantic);
    body.extend_from_slice(&binding.definition);
    body.extend_from_slice(&binding.owner);
    return (envelope(algorithm, 2, 1, body), 1);
  }
  let first = lookup(algorithm, &bindings[0]);
  let branch =
    (depth..width).find(|position| bindings.iter().any(|binding| lookup(algorithm, binding)[*position] != first[*position])).unwrap();
  let mut groups: BTreeMap<u8, Vec<Binding>> = BTreeMap::new();
  for binding in bindings {
    groups.entry(lookup(algorithm, binding)[branch]).or_default().push(binding.clone());
  }
  let mut body = vec![0; 20];
  body[4..6].copy_from_slice(&(depth as u16).to_le_bytes());
  body[6..8].copy_from_slice(&((branch - depth) as u16).to_le_bytes());
  body[8..10].copy_from_slice(&(groups.len() as u16).to_le_bytes());
  body[12..20].copy_from_slice(&(bindings.len() as u64).to_le_bytes());
  body.extend_from_slice(&first[depth..branch]);
  let children = groups.len();
  let mut nodes = 1;
  for (edge, group) in groups {
    let (child, child_nodes) = oracle(algorithm, &group, branch + 1);
    nodes += child_nodes;
    body.extend_from_slice(&[edge, 0, 0, 0]);
    body.extend_from_slice(&(group.len() as u64).to_le_bytes());
    body.extend_from_slice(&child.object_id);
  }
  (envelope(algorithm, 3, children as u64, body), nodes)
}
