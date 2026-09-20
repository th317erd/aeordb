//! Independent frozen framing and physical locator boundary cases.
use super::*;
use crate::engine::v4::entity::decode_whole_entity_header_v1;
use crate::engine::v4::reader::MalformedInputClass;

#[test]
fn native_task_metadata_inventory_shared_header_preserves_frozen_full_decoder_diagnostics() {
  for (algorithm, bytes) in [
    (HashAlgorithm::Blake3_256, include_bytes!("../fixtures/v4/whole-entity-v1/entity-blake3-256-directory-root-valid.bin").as_slice()),
    (HashAlgorithm::Sha512, include_bytes!("../fixtures/v4/whole-entity-v1/entity-sha512-directory-root-valid.bin").as_slice()),
  ] {
    let header_length = 77 + algorithm.hash_length();
    let header = decode_whole_entity_header_v1(&bytes[..header_length], algorithm, u64::MAX, bytes.len()).unwrap();
    let whole = decode_whole_entity(bytes, algorithm, u64::MAX).unwrap();
    assert_eq!(header.entry_type, EntryTypeV4::DirectoryIndex);
    assert_eq!(header.entity_version, 1);
    assert_eq!(header.header_length, header_length);
    assert_eq!(header.key_length, algorithm.hash_length());
    assert_eq!(header.header_length + header.key_length + header.value_length, bytes.len());
    assert_eq!(header.integrity_hash, whole.integrity_hash);
    assert_eq!(header.write_sequence, whole.write_sequence);
    for length in 0..bytes.len() {
      let error = decode_whole_entity(&bytes[..length], algorithm, u64::MAX).unwrap_err();
      let (code, context) = if length < 12 {
        ("truncated_entity_prefix", "need 12-byte entity prefix".to_owned())
      } else if length < header_length {
        ("header_length", format!("expected {header_length}, declared {header_length}, input {length}"))
      } else {
        ("total_length", format!("declared {}, input {length}", bytes.len()))
      };
      assert_eq!(error.code(), code);
      assert_eq!(error.context(), context);
      assert_eq!(error.class(), MalformedInputClass::TruncationOrTrailingBytes);
      if length < header_length {
        assert_eq!(decode_whole_entity_header_v1(&bytes[..length], algorithm, u64::MAX, bytes.len()).unwrap_err(), error);
      }
    }
    let mut damaged = bytes.to_vec();
    *damaged.last_mut().unwrap() ^= 1;
    assert!(decode_whole_entity_header_v1(&damaged[..header_length], algorithm, u64::MAX, damaged.len()).is_ok());
    assert_eq!(decode_whole_entity(&damaged, algorithm, u64::MAX).unwrap_err().code(), "integrity_hash_mismatch");
    assert_eq!(
      decode_whole_entity_header_v1(&bytes[..header_length], algorithm, u64::MAX, bytes.len() - 1).unwrap_err().code(),
      "total_length"
    );
  }
}

#[test]
fn native_task_metadata_inventory_rejects_every_truncated_prefix_and_invalid_captured_extent() {
  with_fixture(|publisher, memory, path, algorithm| {
    let key = publish_payload(publisher);
    let locator = publisher.locator(&key).unwrap().unwrap();
    let header = publisher.observe().unwrap().selected.header;
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(memory, &cancellation).unwrap();
    let before = fs::read(path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    for length in 0..77 + 2 * algorithm.hash_length() {
      let mut truncated = locator.clone();
      truncated.total_length = length as u32;
      publisher.lock_kv().unwrap().insert(truncated).unwrap();
      {
        let capture = protection.capture_semantic_mutation_inventory(bounds(), memory, &cancellation).unwrap();
        assert!(capture.visit_metadata(|_| Ok(true)).is_err(), "truncated prefix length {length} became complete");
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    }
    for offset in [0, header.kv_block_offset, header.hot_tail_offset, u64::MAX - 1] {
      let mut misplaced = locator.clone();
      misplaced.offset = offset;
      publisher.lock_kv().unwrap().insert(misplaced).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(bounds(), memory, &cancellation).unwrap();
      assert_eq!(capture.visit_metadata(|_| Ok(true)).unwrap_err().code(), "semantic_task_inventory_extent");
    }
    publisher.lock_kv().unwrap().insert(locator).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(bounds(), memory, &cancellation).unwrap();
    assert_eq!(capture.visit_metadata(|_| Ok(true)).unwrap().tasks, 1);
    assert_eq!(fs::read(path).unwrap(), before);
  });
}
