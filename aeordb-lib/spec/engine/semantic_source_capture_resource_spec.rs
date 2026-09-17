#[path = "semantic_source_aliases_resource_spec.rs"]
mod aliases;
#[path = "semantic_source_catalog_build_resource_spec.rs"]
mod assembly;
#[path = "semantic_source_writer_resource_spec.rs"]
mod writers;
use super::{fixture, measure, measure_nth};
use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::semantic_source_capture::{
  decode_semantic_source_capture_binding_v1, decode_semantic_source_capture_v1, decode_semantic_source_node_v1,
};
use aeordb::engine::v4::system_control::decode_system_control;

const PROFILES: [(HashAlgorithm, &str); 2] = [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")];

fn control(profile: &str, kind: &str) -> Vec<u8> {
  fixture("system-control-v1", &format!("control-{profile}-{kind}-valid"))
}

fn reseal(bytes: &mut [u8]) {
  let end = bytes.len() - 4;
  let checksum = crc32fast::hash(&bytes[..end]);
  bytes[end..].copy_from_slice(&checksum.to_le_bytes());
}

#[test]
fn source_capture_identity_refusal_is_operational_and_retry_succeeds() {
  for (algorithm, profile) in PROFILES {
    for (kind, allocation) in [
      ("semantic-source-capture", 24),
      ("semantic-source-node", algorithm.hash_length()),
      ("semantic-source-node-internal", algorithm.hash_length()),
    ] {
      let bytes = control(profile, kind);
      let (result, allocations) = measure(allocation, || decode_system_control(&bytes, algorithm));
      assert!(allocations.injected_failure);
      let error = result.unwrap_err();
      assert!(error.is_allocation_failure());
      assert_eq!(error.code(), "semantic_capture_identity_allocation");
      let (result, allocations) = measure(0, || decode_system_control(&bytes, algorithm));
      assert!(result.is_ok());
      assert_eq!(allocations.total, allocation);
      assert_eq!(allocations.maximum, allocation);
    }
  }
}

#[test]
fn source_capture_binding_each_allocation_refuses_then_retries_without_copying_inputs() {
  for (algorithm, profile) in PROFILES {
    let capture = control(profile, "semantic-source-capture");
    let checkpoint = control(profile, "semantic-mutation-checkpoint");
    for (size, occurrence, code) in [
      (24, 1, "semantic_capture_identity_allocation"),
      (24, 2, "semantic_task_identity_allocation"),
      (algorithm.hash_length(), 1, "semantic_capture_digest_allocation"),
    ] {
      let (result, allocations) =
        measure_nth(size, occurrence, || decode_semantic_source_capture_binding_v1(&capture, &checkpoint, algorithm));
      assert!(allocations.injected_failure);
      let error = result.unwrap_err();
      assert!(error.is_allocation_failure());
      assert_eq!(error.code(), code);
      let (result, allocations) = measure(0, || decode_semantic_source_capture_binding_v1(&capture, &checkpoint, algorithm));
      assert!(result.is_ok(), "{result:?}");
      assert_eq!(allocations.total, 48 + algorithm.hash_length());
      assert_eq!(allocations.maximum, algorithm.hash_length());
    }
    let (result, allocations) = measure(0, || decode_semantic_source_capture_v1(&capture, algorithm));
    assert!(result.is_ok());
    assert_eq!(allocations.total, 24);
  }
}

#[test]
fn source_catalog_maximum_fanout_and_long_paths_allocate_only_digest_and_borrow_iteration() {
  for (algorithm, profile) in PROFILES {
    for (internal, count, path_length) in [(false, 256, 12), (true, 128, 12), (false, 15, 65_535), (true, 15, 65_535)] {
      let mut bytes = control(profile, "semantic-source-node");
      bytes.truncate(64);
      bytes[48..50].copy_from_slice(&(if internal { 2u16 } else { 1u16 }).to_le_bytes());
      bytes[52..56].copy_from_slice(&(count as u32).to_le_bytes());
      if internal {
        bytes.resize(bytes.len() + algorithm.hash_length(), 0xfe);
      }
      for index in 0..count {
        let path = format!("/{index:03}/{}", "x".repeat(path_length - 5));
        bytes.extend_from_slice(&(path.len() as u32).to_le_bytes());
        bytes.extend_from_slice(path.as_bytes());
        bytes.resize(bytes.len() + algorithm.hash_length(), (index + 1) as u8);
      }
      let payload_length = (bytes.len() - 64) as u32;
      let body_length = payload_length + 32;
      bytes[56..60].copy_from_slice(&payload_length.to_le_bytes());
      bytes[24..28].copy_from_slice(&body_length.to_le_bytes());
      bytes[8..12].copy_from_slice(&(body_length + 36).to_le_bytes());
      bytes.extend_from_slice(&[0; 4]);
      reseal(&mut bytes);
      let (result, allocations) = measure(0, || decode_semantic_source_node_v1(&bytes, algorithm));
      let node = result.unwrap();
      assert_eq!(allocations.total, algorithm.hash_length());
      assert_eq!(allocations.maximum, algorithm.hash_length());
      let (observed, allocations) = measure(0, || {
        if internal {
          node.children().unwrap().map(|child| child.unwrap().node_id.len()).sum::<usize>() / algorithm.hash_length()
        } else {
          node
            .leaf_entries()
            .unwrap()
            .map(|entry| {
              assert_eq!(entry.unwrap().path.len(), path_length);
              1
            })
            .sum::<usize>()
        }
      });
      assert_eq!(observed, count + usize::from(internal));
      assert_eq!(allocations.total, 0);
    }
  }
}

#[test]
fn source_capture_bad_counts_do_not_allocate_claimed_collections() {
  for (algorithm, profile) in PROFILES {
    for (kind, offset, length) in [
      ("semantic-source-node", 52, 4),
      ("semantic-source-node", 56, 4),
      ("semantic-source-node", 64, 4),
      ("semantic-source-capture", 120, 8),
    ] {
      let mut bytes = control(profile, kind);
      bytes[offset..offset + length].fill(0xff);
      reseal(&mut bytes);
      let (result, allocations) = measure(0, || decode_system_control(&bytes, algorithm));
      let error = result.unwrap_err();
      assert!(!error.is_allocation_failure());
      assert!(allocations.total <= 512, "{allocations:?}");
      assert!(allocations.maximum <= 256, "{allocations:?}");
    }
  }
}
