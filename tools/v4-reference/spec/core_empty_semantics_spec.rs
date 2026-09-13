use super::{HashProfile, decode_semantic_object};

#[test]
fn independent_reference_accepts_complete_empty_catalog_and_rejects_presence_count_disagreements() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    let fixture_path = format!(
      "{}/../../aeordb-lib/spec/fixtures/v4/semantic-object-v1/asem-{}-state-complete.bin",
      env!("CARGO_MANIFEST_DIR"),
      profile.label()
    );
    let original = std::fs::read(fixture_path).unwrap();
    for bits in 0u8..64 {
      let mut bytes = original.clone();
      let present = bits & 1 != 0;
      let root = bits & 2 != 0;
      let counts = [bits & 4 != 0, bits & 8 != 0, bits & 16 != 0, bits & 32 != 0];
      bytes[76] = u8::from(present);
      bytes[80 + 2 * profile.width()..80 + 3 * profile.width()].fill(u8::from(root));
      bytes[20..28].copy_from_slice(&u64::from(counts[0]).to_le_bytes());
      for (index, nonzero) in counts.into_iter().enumerate() {
        let offset = 80 + 3 * profile.width() + index * 8;
        bytes[offset..offset + 8].copy_from_slice(&u64::from(nonzero).to_le_bytes());
      }
      let checksum_offset = bytes.len() - 4;
      let checksum = crc32fast::hash(&bytes[..checksum_offset]);
      bytes[checksum_offset..].copy_from_slice(&checksum.to_le_bytes());
      let observed = decode_semantic_object(profile, &bytes);
      let valid = bits == 0 || (present && root && counts[0] && counts[1]);
      assert_eq!(observed.is_ok(), valid, "{profile:?} combination {bits:06b}");
      if bits == 0 {
        assert_eq!(observed.unwrap().0, "semantic:state:complete");
      }
    }
  }
}
