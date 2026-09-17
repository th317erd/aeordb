use super::*;

#[test]
fn source_capture_reference_closes_all_shapes_and_every_prefix() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for (kind, bytes) in [(0x0048, body(profile, 0x0048)), (0x0049, node_body(profile, false)), (0x0049, node_body(profile, true))] {
      let identity = validate(profile, kind, &bytes).unwrap();
      assert_eq!(identity.len(), if kind == 0x0048 { 24 } else { profile.width() });
      for end in 0..bytes.len() {
        assert!(validate(profile, kind, &bytes[..end]).is_err(), "{kind:x} prefix{end}");
      }
      let mut trailing = bytes.clone();
      trailing.push(0);
      assert!(validate(profile, kind, &trailing).is_err());
    }
  }
}

#[test]
fn source_capture_reference_rejects_manifest_identity_counts_and_all_hashes() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    let baseline = body(profile, 0x0048);
    for (offset, length) in [(0, 16), (16, 16), (32, 8), (40, 16), (56, 8), (64, 8), (72, 8), (88, 8), (96, 8), (104, 8)] {
      let mut bytes = baseline.clone();
      bytes[offset..offset + length].fill(0);
      assert!(validate(profile, 0x0048, &bytes).is_err(), "zero{offset}");
    }
    for slot in 0..6 {
      let mut bytes = baseline.clone();
      bytes[112 + slot * profile.width()..112 + (slot + 1) * profile.width()].fill(0);
      assert!(validate(profile, 0x0048, &bytes).is_err());
    }
    for (offset, number) in [(80, u64::MAX), (88, u64::MAX), (96, 4), (104, 4)] {
      let mut bytes = baseline.clone();
      bytes[offset..offset + 8].copy_from_slice(&number.to_le_bytes());
      assert!(validate(profile, 0x0048, &bytes).is_err());
    }
  }
}

#[test]
fn source_node_reference_rejects_shape_order_path_and_repeated_children() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for internal in [false, true] {
      let baseline = node_body(profile, internal);
      for offset in [16, 18, 20, 24, 28] {
        let mut bytes = baseline.clone();
        bytes[offset] = 255;
        assert!(validate(profile, 0x0049, &bytes).is_err(), "field{offset}");
      }
      let start = 36 + if internal { profile.width() } else { 0 };
      for byte in [0, b'x', 255] {
        let mut bytes = baseline.clone();
        bytes[start] = byte;
        assert!(validate(profile, 0x0049, &bytes).is_err());
      }
      if internal {
        for fill in [0, 0x31] {
          let mut bytes = baseline.clone();
          let last = bytes.len() - profile.width();
          bytes[last..].fill(fill);
          assert!(validate(profile, 0x0049, &bytes).is_err());
        }
      } else {
        let mut bytes = baseline.clone();
        // Equal length path values: make the first sort after the second.
        bytes[start + "/.aeordb-config/".len()] = b'z';
        assert!(validate(profile, 0x0049, &bytes).is_err());
      }
    }
  }
}

#[test]
fn source_capture_reference_preserves_sparse_capability_meanings() {
  for bit in 0..256 {
    let mut bytes = [0; 32];
    bytes[bit / 8] = 1 << (bit % 8);
    assert_eq!(crate::core::capabilities_are_known(&bytes), bit < 24 || matches!(bit, 25 | 27));
  }
}
