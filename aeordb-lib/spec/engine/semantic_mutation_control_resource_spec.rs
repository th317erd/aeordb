use super::{fixture, measure};
use aeordb::engine::v4::reader::MalformedInputClass;
use aeordb::engine::v4::semantic_mutation_control::{decode_semantic_mutation_checkpoint, decode_semantic_mutation_selection};
use aeordb::engine::v4::system_control::decode_system_control;
use aeordb::engine::HashAlgorithm;
use sha2::Digest;

fn reseal(bytes: &mut [u8]) {
  let end = bytes.len() - 4;
  let checksum = crc32fast::hash(&bytes[..end]);
  bytes[end..].copy_from_slice(&checksum.to_le_bytes());
}

#[test]
fn semantic_task_envelope_identity_allocations_fail_with_typed_errors() {
  for (slug, length) in [("task", 16), ("checkpoint", 24)] {
    let bytes = fixture("system-control-v1", &format!("control-blake3-256-semantic-mutation-{slug}-valid"));
    let (result, allocations) = measure(length, || decode_system_control(&bytes, HashAlgorithm::Blake3_256));
    assert!(allocations.injected_failure);
    let error = result.unwrap_err();
    assert_eq!(error.code(), "semantic_task_identity_allocation");
    assert_eq!(error.class(), MalformedInputClass::AllocationAmplification);
    assert!(error.is_allocation_failure());
  }
}

#[test]
fn semantic_checkpoint_maximum_cursor_is_borrowed_with_only_fixed_identity_allocation() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let width = algorithm.hash_length();
    let mut bytes = fixture("system-control-v1", &format!("control-{profile}-semantic-mutation-checkpoint-valid"));
    bytes.truncate(bytes.len() - 4);
    bytes[120..122].copy_from_slice(&2u16.to_le_bytes());
    bytes[122..124].copy_from_slice(&1u16.to_le_bytes());
    bytes[124..128].copy_from_slice(&65_535u32.to_le_bytes());
    bytes[200 + 4 * width..200 + 6 * width].fill(0);
    bytes.push(b'/');
    bytes.resize(bytes.len() + 65_534, b'x');
    let body_length = (bytes.len() - 32) as u32;
    bytes[24..28].copy_from_slice(&body_length.to_le_bytes());
    bytes[8..12].copy_from_slice(&(body_length + 36).to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    reseal(&mut bytes);
    let (result, allocations) = measure(0, || decode_semantic_mutation_checkpoint(&bytes, algorithm));
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(allocations.total, 24);
    assert_eq!(allocations.maximum, 24);
    let checkpoint = result.unwrap();
    let (edges, allocations) = measure(0, || checkpoint.references().count());
    assert_eq!(edges, 3);
    assert_eq!(allocations.total, 0);
  }
}

#[test]
fn semantic_task_selected_digest_allocation_failure_is_not_corruption_or_abort() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let checkpoint = fixture("system-control-v1", &format!("control-{profile}-semantic-mutation-checkpoint-valid"));
    let mut task = fixture("system-control-v1", &format!("control-{profile}-semantic-mutation-task-valid"));
    task[128..130].copy_from_slice(&4u16.to_le_bytes());
    let digest = if algorithm == HashAlgorithm::Blake3_256 {
      blake3::hash(&checkpoint).as_bytes().to_vec()
    } else {
      sha2::Sha512::digest(&checkpoint).to_vec()
    };
    task[144..144 + algorithm.hash_length()].copy_from_slice(&digest);
    reseal(&mut task);
    let (result, allocations) = measure(0, || decode_semantic_mutation_selection(&task, &checkpoint, algorithm));
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(allocations.total, 16 + 24 + algorithm.hash_length());
    let (result, allocations) = measure(algorithm.hash_length(), || decode_semantic_mutation_selection(&task, &checkpoint, algorithm));
    assert!(allocations.injected_failure);
    let error = result.unwrap_err();
    assert_eq!(error.code(), "semantic_task_digest_allocation");
    assert!(error.is_allocation_failure());
  }
}
