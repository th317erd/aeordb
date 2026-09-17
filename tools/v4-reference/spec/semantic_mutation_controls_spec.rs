use super::*;

#[test]
fn sequential_round17_oracle_closes_each_new_body_at_both_widths() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for (kind, identity_length, length) in [(0x0044, 16, 112 + profile.width()), (0x0045, 24, 168 + 9 * profile.width()), (0x0046, 0, 16)] {
      let bytes = body(profile, kind);
      assert_eq!(bytes.len(), length);
      let identity = validate(profile, kind, &bytes).unwrap();
      assert_eq!(identity.len(), identity_length);
      for length in 0..bytes.len() {
        assert!(validate(profile, kind, &bytes[..length]).is_err());
      }
      let mut extra = bytes.clone();
      extra.push(0);
      assert!(validate(profile, kind, &extra).is_err());
      let mut zero_database = bytes;
      zero_database[..16].fill(0);
      assert!(validate(profile, kind, &zero_database).is_err());
    }
  }
}

#[test]
fn oracle_capability_mask_preserves_the_historical_gap() {
  for bit in 0..256 {
    let mut bytes = [0; 32];
    bytes[bit / 8] |= 1 << (bit % 8);
    assert_eq!(crate::core::capabilities_are_known(&bytes), bit < 24 || bit == 25 || bit == 27);
  }
  for length in [0, 3, 4, 31, 33, 64] {
    assert!(!crate::core::capabilities_are_known(&vec![0; length]));
  }
}

#[test]
fn independent_oracle_rejects_semantic_fields_with_valid_lengths() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for (offset, length) in [(16, 16), (32, 16), (48, 16), (64, 8), (72, 8), (96, 2), (100, 8), (112, profile.width())] {
      let mut task = body(profile, 0x0044);
      task[offset..offset + length].fill(0);
      assert!(validate(profile, 0x0044, &task).is_err(), "task field {offset}");
    }
    for offset in [98, 108] {
      let mut task = body(profile, 0x0044);
      task[offset] = 1;
      assert!(validate(profile, 0x0044, &task).is_err());
    }
    for (offset, length) in [(16, 16), (32, 8), (40, 16), (56, 8), (64, 8), (72, 8), (88, 2), (104, 8), (112, 8), (120, 8), (136, 8)] {
      let mut checkpoint = body(profile, 0x0045);
      checkpoint[offset..offset + length].fill(0);
      assert!(validate(profile, 0x0045, &checkpoint).is_err(), "checkpoint field {offset}");
    }
    for slot in [0, 1, 2, 4, 5, 6, 7, 8] {
      let mut checkpoint = body(profile, 0x0045);
      let start = 168 + slot * profile.width();
      checkpoint[start..start + profile.width()].fill(0);
      assert!(validate(profile, 0x0045, &checkpoint).is_err(), "checkpoint hash {slot}");
    }
    let mut overflow = body(profile, 0x0045);
    overflow[112..120].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(validate(profile, 0x0045, &overflow).is_err());
  }
}
