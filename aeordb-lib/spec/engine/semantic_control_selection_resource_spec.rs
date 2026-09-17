//! An operational decode failure cannot turn a current slot into stale authority.
use super::{fixture, measure_nth};
use aeordb::engine::v4::system_control::{select_system_control_pair, SystemControlSlotV1};
use aeordb::engine::HashAlgorithm;

fn task(algorithm: HashAlgorithm, sequence: u64) -> Vec<u8> {
  let profile = if algorithm.hash_length() == 32 { "blake3-256" } else { "sha512" };
  let mut bytes = fixture("system-control-v1", &format!("control-{profile}-semantic-mutation-task-valid"));
  bytes[16..24].copy_from_slice(&sequence.to_le_bytes());
  let crc_offset = bytes.len() - 4;
  let crc = crc32fast::hash(&bytes[..crc_offset]);
  bytes[crc_offset..].copy_from_slice(&crc.to_le_bytes());
  bytes
}

#[test]
fn control_selection_never_falls_back_on_either_slot_identity_allocation_failure() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for newer_in_a in [true, false] {
      let a = task(algorithm, if newer_in_a { 9 } else { 8 });
      let b = task(algorithm, if newer_in_a { 8 } else { 9 });
      for occurrence in [1, 2] {
        let (selected, measured) = measure_nth(16, occurrence, || select_system_control_pair(algorithm, &a, &b));
        assert!(measured.injected_failure);
        let error = match selected {
          Err(error) => error,
          Ok(value) => panic!("allocation refusal incorrectly selected sequence {}", value.control.sequence),
        };
        assert!(error.is_allocation_failure());
        assert_eq!(error.code(), "semantic_task_identity_allocation");
      }
      let retry = select_system_control_pair(algorithm, &a, &b).unwrap();
      assert_eq!(retry.control.sequence, 9);
      assert_eq!(retry.selected_slot, if newer_in_a { SystemControlSlotV1::A } else { SystemControlSlotV1::B });
      assert!(!retry.redundancy_degraded);
    }
  }
}

#[test]
fn control_selection_preserves_resource_failure_beside_corruption_and_genuine_torn_fallback() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let valid = task(algorithm, 8);
    let mut torn = task(algorithm, 9);
    let last = torn.len() - 1;
    torn[last] ^= 1;
    for torn_in_a in [false, true] {
      let (a, b) = if torn_in_a { (&torn, &valid) } else { (&valid, &torn) };
      let (selected, measured) = measure_nth(16, 1, || select_system_control_pair(algorithm, a, b));
      assert!(measured.injected_failure);
      let error = selected.unwrap_err();
      assert!(error.is_allocation_failure());
      assert_eq!(error.code(), "semantic_task_identity_allocation");
      let retry = select_system_control_pair(algorithm, a, b).unwrap();
      assert_eq!(retry.control.sequence, 8);
      assert!(retry.redundancy_degraded);
    }
    let no_valid = select_system_control_pair(algorithm, &torn, &torn).unwrap_err();
    assert!(!no_valid.is_allocation_failure());
    assert_eq!(no_valid.code(), "system_control_no_valid_slot");
  }
}
