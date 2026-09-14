//! Independent envelope-size checks; opaque payloads only characterize the
//! generic structural reader, not validity as compiled semantic projections.
use super::{CoreFormat, HashProfile, observe};

fn structural_definition(profile: HashProfile, total_length: usize) -> Vec<u8> {
  let width = profile.width();
  let payload_length = total_length - 52 - width;
  let mut bytes = vec![0; total_length];
  bytes[..4].copy_from_slice(b"ASEM");
  bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
  bytes[6..8].copy_from_slice(&4u16.to_le_bytes());
  bytes[8..10].copy_from_slice(&32u16.to_le_bytes());
  bytes[12..16].copy_from_slice(&(total_length as u32).to_le_bytes());
  bytes[16..20].copy_from_slice(&((total_length - 36) as u32).to_le_bytes());
  bytes[20..28].copy_from_slice(&1u64.to_le_bytes());
  bytes[32..34].copy_from_slice(&2u16.to_le_bytes());
  bytes[34..36].copy_from_slice(&1u16.to_le_bytes());
  bytes[40..40 + width].fill(0x43);
  bytes[40 + width..44 + width].copy_from_slice(&(payload_length as u32).to_le_bytes());
  let checksum = crc32fast::hash(&bytes[..total_length - 4]);
  bytes[total_length - 4..].copy_from_slice(&checksum.to_le_bytes());
  bytes
}

#[test]
fn reference_semantic_envelope_preserves_the_exact_one_mebibyte_structural_boundary() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    let bytes = structural_definition(profile, 1_048_576);
    assert_eq!(observe(CoreFormat::SemanticObjectV1, profile, &bytes).0, "semantic:definition:class=2");
  }
}

#[test]
fn reference_semantic_envelope_rejects_oversized_structurally_complete_definitions() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for length in [1_048_577, 1_114_112] {
      let bytes = structural_definition(profile, length);
      assert!(observe(CoreFormat::SemanticObjectV1, profile, &bytes).0.starts_with("error:"), "{profile:?}, {length}");
    }
  }
}

#[test]
fn reference_semantic_envelope_admits_size_before_crc_or_identity_processing() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    let mut bytes = structural_definition(profile, 1_048_577);
    bytes[0] ^= 1;
    assert_eq!(observe(CoreFormat::SemanticObjectV1, profile, &bytes).0, "error:semantic_object_exceeds_cap");
  }
}
