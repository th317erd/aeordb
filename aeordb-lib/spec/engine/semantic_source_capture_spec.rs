//! Independent capture bodies, with no production encoder or identity helper.
#[path = "semantic_source_writer_spec.rs"]
mod writers;
use super::{ALGORITHMS, checkpoint_body, count, envelope, word};
use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use aeordb::engine::v4::system_control::{SystemControlKindV1, SystemControlSlotV1, decode_system_control};
use sha2::Digest;
use aeordb::engine::v4::semantic_source_capture::{
  decode_semantic_source_capture_binding_v1, decode_semantic_source_capture_v1, decode_semantic_source_node_v1,
};

fn independent_digest(algorithm: HashAlgorithm, bytes: &[u8]) -> Vec<u8> {
  match algorithm {
    HashAlgorithm::Blake3_256 => blake3::hash(bytes).as_bytes().to_vec(),
    HashAlgorithm::Sha256 => sha2::Sha256::digest(bytes).to_vec(),
    HashAlgorithm::Sha512 => sha2::Sha512::digest(bytes).to_vec(),
    HashAlgorithm::Sha3_256 => sha3::Sha3_256::digest(bytes).to_vec(),
    HashAlgorithm::Sha3_512 => sha3::Sha3_512::digest(bytes).to_vec(),
  }
}

fn manifest_body(algorithm: HashAlgorithm) -> Vec<u8> {
  let width = algorithm.hash_length();
  let checkpoint = checkpoint_body(algorithm, 1);
  let mut body = vec![0; 112 + 6 * width];
  body[..88].copy_from_slice(&checkpoint[..88]);
  count(&mut body, 88, 2);
  count(&mut body, 96, 1);
  count(&mut body, 104, 1);
  body[112..112 + 2 * width].copy_from_slice(&checkpoint[168..168 + 2 * width]);
  body[112 + 2 * width..112 + 3 * width].fill(0x31);
  body[112 + 3 * width..112 + 4 * width].fill(0x41);
  body[112 + 4 * width..112 + 5 * width].copy_from_slice(&checkpoint[168 + 8 * width..]);
  body[112 + 5 * width..].copy_from_slice(&independent_digest(algorithm, &envelope(b"ASMC", 1, &checkpoint)));
  body
}

fn node_body(algorithm: HashAlgorithm, internal: bool) -> Vec<u8> {
  let width = algorithm.hash_length();
  let mut body = vec![0; 32];
  body[..16].fill(1);
  word(&mut body, 16, if internal { 2 } else { 1 });
  let rows: &[(&str, u8)] = if internal {
    body.extend(vec![0x31; width]);
    &[("/.aeordb-config/parsers.json", 0x41)]
  } else {
    &[("/.aeordb-config/indexes.json", 0), ("/.aeordb-config/parsers.json", 0x51)]
  };
  body[20..24].copy_from_slice(&(rows.len() as u32).to_le_bytes());
  for (path, byte) in rows {
    body.extend_from_slice(&(path.len() as u32).to_le_bytes());
    body.extend_from_slice(path.as_bytes());
    body.extend(vec![*byte; width]);
  }
  let payload_length = (body.len() - 32) as u32;
  body[24..28].copy_from_slice(&payload_length.to_le_bytes());
  body
}

#[test]
fn independent_capture_manifest_uses_checkpoint_identity_at_every_hash_width() {
  for algorithm in ALGORITHMS {
    let body = manifest_body(algorithm);
    let bytes = envelope(b"ASCM", 1, &body);
    let control = decode_system_control(&bytes, algorithm).expect("capture companion must be recognized");
    assert_eq!(control.kind as u16, 0x0048);
    assert_eq!(control.identity, body[16..40]);
    assert_eq!(control.body, body);
    assert!(control.kind.is_immutable());
    assert!(control.canonical_path_for_slot(SystemControlSlotV1::Immutable).unwrap().contains("/0048/"));
    assert!(control.canonical_path_for_slot(SystemControlSlotV1::A).is_err());
    assert!(decode_system_control(&envelope(b"ASCM", 2, &body), algorithm).is_err());
  }
}

fn assert_independent_node(internal: bool) {
  for algorithm in ALGORITHMS {
    let body = node_body(algorithm, internal);
    let mut identity_preimage = b"aeordb.semantic-source-node.v1\0".to_vec();
    identity_preimage.extend_from_slice(&body);
    let bytes = envelope(b"ASCN", 1, &body);
    let control = decode_system_control(&bytes, algorithm).expect("source catalog node must be recognized");
    assert_eq!(control.kind as u16, 0x0049);
    assert_eq!(control.identity, independent_digest(algorithm, &identity_preimage));
    assert_eq!(control.body, body);
    assert!(control.kind.is_immutable());
    assert!(control.canonical_path_for_slot(SystemControlSlotV1::Immutable).unwrap().contains("/0049/"));
    assert!(control.canonical_path_for_slot(SystemControlSlotV1::B).is_err());
    assert!(decode_system_control(&envelope(b"ASCN", 2, &body), algorithm).is_err());
  }
}

#[test]
fn independent_source_leaf_preserves_present_and_absent_file_records() {
  assert_independent_node(false);
}

#[test]
fn independent_source_internal_has_typed_nonzero_child_identities() {
  assert_independent_node(true);
}

#[test]
fn capture_capability_is_assigned_but_not_advertised() {
  let capabilities = CapabilitySetV1::from_bits([25, 27]).expect("source capture assigns27 without repurposing26");
  assert_eq!(capabilities.bits(), [25, 27]);
  assert_eq!(CapabilitySetV1::from_bytes(capabilities.into_bytes()).unwrap(), capabilities);
  let current = BinaryCapabilityProfileV1::current();
  for bit in [25, 27] {
    assert!(!current.supported_reader_capabilities.contains(bit));
    assert!(!current.supported_writer_capabilities.contains(bit));
  }
}

#[test]
fn prior_unknown_formats_and_capabilities_keep_their_negative_meaning() {
  for kind in [0x0047, 0x004a, 0x004b] {
    assert!(SystemControlKindV1::from_u16(kind).is_none());
  }
  assert!(SystemControlKindV1::from_magic(b"ASCR").is_none());
  for bit in [24, 26, 28, 255] {
    assert!(CapabilitySetV1::from_bits([bit]).is_err());
  }
}

#[test]
fn capture_fields_and_catalog_entries_borrow_the_input() {
  for algorithm in ALGORITHMS {
    let width = algorithm.hash_length();
    let manifest = envelope(b"ASCM", 1, &manifest_body(algorithm));
    let capture = decode_semantic_source_capture_v1(&manifest, algorithm).unwrap();
    assert_eq!(capture.database_id.as_ptr(), manifest[32..].as_ptr());
    assert_eq!(capture.task_id.as_ptr(), manifest[48..].as_ptr());
    assert_eq!(capture.base_namespace_root.as_ptr(), manifest[144..].as_ptr());
    assert_eq!(capture.protected_path_count, 2);
    assert_eq!(capture.base_catalog_node_count, 1);
    assert_eq!(capture.requested_catalog_node_count, 1);
    let leaf_bytes = envelope(b"ASCN", 1, &node_body(algorithm, false));
    let leaf = decode_semantic_source_node_v1(&leaf_bytes, algorithm).unwrap();
    assert_eq!(leaf.database_id().as_ptr(), leaf_bytes[32..].as_ptr());
    assert!(leaf.children().is_none());
    let entries = leaf.leaf_entries().unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].path, "/.aeordb-config/indexes.json");
    assert_eq!(entries[0].path.as_ptr(), leaf_bytes[68..].as_ptr());
    assert_eq!(entries[0].file_record_id, None);
    assert_eq!(entries[1].file_record_id.unwrap(), vec![0x51; width]);
    let internal_bytes = envelope(b"ASCN", 1, &node_body(algorithm, true));
    let internal = decode_semantic_source_node_v1(&internal_bytes, algorithm).unwrap();
    assert!(internal.leaf_entries().is_none());
    let children = internal.children().unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(children.len(), 2);
    assert_eq!(children[0].separator, None);
    assert_eq!(children[0].node_id, vec![0x31; width]);
    assert_eq!(children[1].separator, Some("/.aeordb-config/parsers.json"));
    assert_eq!(children[1].node_id, vec![0x41; width]);
  }
}

#[test]
fn companion_binds_the_entire_original_checkpoint_and_duplicated_context() {
  for algorithm in ALGORITHMS {
    let checkpoint_body = checkpoint_body(algorithm, 1);
    let checkpoint = envelope(b"ASMC", 1, &checkpoint_body);
    let manifest = manifest_body(algorithm);
    assert!(decode_semantic_source_capture_binding_v1(&envelope(b"ASCM", 1, &manifest), &checkpoint, algorithm).is_ok());
    // Change a field that is NOT separately copied into the companion.
    let mut other_body = checkpoint_body.clone();
    other_body[168 + 6 * algorithm.hash_length()] ^= 0x40;
    let other = envelope(b"ASMC", 1, &other_body);
    assert!(decode_semantic_source_capture_binding_v1(&envelope(b"ASCM", 1, &manifest), &other, algorithm).is_err());
    // A matching complete payload digest does not excuse disagreeing copied fields.
    for offset in [0, 16, 32, 40, 56, 64, 72, 80, 112, 112 + algorithm.hash_length(), 112 + 4 * algorithm.hash_length()] {
      let mut changed = manifest.clone();
      changed[offset] ^= 0x40;
      assert!(decode_semantic_source_capture_binding_v1(&envelope(b"ASCM", 1, &changed), &checkpoint, algorithm).is_err(), "{offset}");
    }
  }
}

#[test]
fn capture_manifest_rejects_invalid_scalars_hashes_counts_and_lengths() {
  for algorithm in ALGORITHMS {
    let valid = manifest_body(algorithm);
    for (offset, length) in [(0, 16), (16, 16), (32, 8), (40, 16), (56, 8), (64, 8), (72, 8), (88, 8), (96, 8), (104, 8)] {
      let mut body = valid.clone();
      body[offset..offset + length].fill(0);
      assert!(decode_system_control(&envelope(b"ASCM", 1, &body), algorithm).is_err(), "zero {offset}");
    }
    for slot in 0..6 {
      let mut body = valid.clone();
      body[112 + slot * algorithm.hash_length()..112 + (slot + 1) * algorithm.hash_length()].fill(0);
      assert!(decode_system_control(&envelope(b"ASCM", 1, &body), algorithm).is_err());
    }
    for (offset, value) in [(80, u64::MAX), (88, u64::MAX), (96, 4), (104, 4)] {
      let mut body = valid.clone();
      count(&mut body, offset, value);
      assert!(decode_system_control(&envelope(b"ASCM", 1, &body), algorithm).is_err(), "field {offset}");
    }
    let mut longer = valid.clone();
    longer.push(0);
    for body in [&valid[..valid.len() - 1], &longer] {
      assert!(decode_system_control(&envelope(b"ASCM", 1, body), algorithm).is_err());
    }
  }
}

#[test]
fn source_nodes_reject_malformed_counts_reserves_paths_and_child_identities() {
  for algorithm in ALGORITHMS {
    for internal in [false, true] {
      let valid = node_body(algorithm, internal);
      for (offset, value) in [(16, 0u32), (16, 3), (18, 1), (20, 0), (20, u32::MAX), (24, 0), (24, u32::MAX), (28, 1)] {
        let mut body = valid.clone();
        if offset == 16 || offset == 18 {
          word(&mut body, offset, value as u16);
        } else {
          body[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        assert!(decode_system_control(&envelope(b"ASCN", 1, &body), algorithm).is_err(), "field {offset}");
      }
      let path_offset = if internal { 32 + algorithm.hash_length() } else { 32 };
      for length in [0u32, 65_536, u32::MAX] {
        let mut body = valid.clone();
        body[path_offset..path_offset + 4].copy_from_slice(&length.to_le_bytes());
        assert!(decode_system_control(&envelope(b"ASCN", 1, &body), algorithm).is_err());
      }
      for byte in [0, b'x', 0xff] {
        let mut body = valid.clone();
        body[path_offset + 4] = byte;
        assert!(decode_system_control(&envelope(b"ASCN", 1, &body), algorithm).is_err());
      }
      if internal {
        for byte in [0, 0x31] {
          let mut body = valid.clone();
          let last_child = body.len() - algorithm.hash_length();
          body[last_child..].fill(byte);
          assert!(decode_system_control(&envelope(b"ASCN", 1, &body), algorithm).is_err());
        }
      }
    }
  }
}

#[test]
fn every_capture_prefix_and_outer_checksum_mutation_is_rejected() {
  for algorithm in ALGORITHMS {
    for (magic, body) in
      [(b"ASCM", manifest_body(algorithm)), (b"ASCN", node_body(algorithm, false)), (b"ASCN", node_body(algorithm, true))]
    {
      let bytes = envelope(magic, 1, &body);
      for end in 0..bytes.len() {
        assert!(decode_system_control(&bytes[..end], algorithm).is_err(), "prefix {end}");
      }
      for offset in 0..bytes.len() {
        let mut changed = bytes.clone();
        changed[offset] ^= 1;
        assert!(decode_system_control(&changed, algorithm).is_err(), "CRC mutation {offset}");
      }
    }
  }
}

fn node_with_paths(algorithm: HashAlgorithm, internal: bool, paths: &[&[u8]]) -> Vec<u8> {
  let mut bytes = vec![0; 32];
  bytes[..16].fill(1);
  word(&mut bytes, 16, if internal { 2 } else { 1 });
  bytes[20..24].copy_from_slice(&(paths.len() as u32).to_le_bytes());
  if internal {
    bytes.extend(vec![0xfe; algorithm.hash_length()]);
  }
  for (index, path) in paths.iter().enumerate() {
    bytes.extend_from_slice(&(path.len() as u32).to_le_bytes());
    bytes.extend_from_slice(path);
    bytes.extend(vec![(index + 1) as u8; algorithm.hash_length()]);
  }
  let length = (bytes.len() - 32) as u32;
  bytes[24..28].copy_from_slice(&length.to_le_bytes());
  bytes
}

#[test]
fn source_nodes_accept_exact_path_fanout_and_body_boundaries() {
  for algorithm in ALGORITHMS {
    for (internal, maximum) in [(false, 256), (true, 128)] {
      let paths: Vec<_> = (0..=maximum).map(|index| format!("/p/{index:03}")).collect();
      let borrowed: Vec<_> = paths.iter().map(|path| path.as_bytes()).collect();
      let maximum_body = node_with_paths(algorithm, internal, &borrowed[..maximum]);
      let encoded = envelope(b"ASCN", 1, &maximum_body);
      let node = decode_semantic_source_node_v1(&encoded, algorithm).unwrap();
      if internal {
        assert_eq!(node.children().unwrap().collect::<Result<Vec<_>, _>>().unwrap().len(), maximum + 1);
      } else {
        assert_eq!(node.leaf_entries().unwrap().collect::<Result<Vec<_>, _>>().unwrap().len(), maximum);
      }
      assert!(decode_system_control(&envelope(b"ASCN", 1, &node_with_paths(algorithm, internal, &borrowed)), algorithm).is_err());
    }
    for (length, valid) in [(65_535, true), (65_536, false)] {
      let path = format!("/{}", "x".repeat(length - 1));
      let body = node_with_paths(algorithm, false, &[path.as_bytes()]);
      assert_eq!(decode_system_control(&envelope(b"ASCN", 1, &body), algorithm).is_ok(), valid);
    }
    // Sixteen legal paths close the exact 1MiB cap, without relying on padding.
    let total_path_bytes = (1 << 20) - 32 - 16 * (4 + algorithm.hash_length());
    let paths: Vec<_> = (0..16)
      .map(|index| {
        let length = if index < 15 { 65_535 } else { total_path_bytes - 15 * 65_535 };
        format!("/{index:02}/{}", "x".repeat(length - 4))
      })
      .collect();
    let borrowed: Vec<_> = paths.iter().map(|path| path.as_bytes()).collect();
    let maximum = node_with_paths(algorithm, false, &borrowed);
    assert_eq!(maximum.len(), 1 << 20);
    assert!(decode_system_control(&envelope(b"ASCN", 1, &maximum), algorithm).is_ok());
    let mut larger = paths;
    larger.last_mut().unwrap().push('x');
    let borrowed: Vec<_> = larger.iter().map(|path| path.as_bytes()).collect();
    let oversized = node_with_paths(algorithm, false, &borrowed);
    assert_eq!(oversized.len(), (1 << 20) + 1);
    assert!(decode_system_control(&envelope(b"ASCN", 1, &oversized), algorithm).is_err());
  }
}

#[test]
fn source_nodes_reject_noncanonical_and_unordered_paths_in_both_kinds() {
  use aeordb::engine::v4::reader::MalformedInputClass;
  for algorithm in ALGORITHMS {
    for internal in [false, true] {
      for path in [b"".as_slice(), b"relative", b"/a/", b"/a//b", b"/a/./b", b"/a/../b", b" /a", b"/a ", b"/a\0b", b"/\xff"] {
        assert!(decode_system_control(&envelope(b"ASCN", 1, &node_with_paths(algorithm, internal, &[path])), algorithm).is_err());
      }
      for paths in [[b"/a".as_slice(), b"/a"], [b"/z", b"/a"]] {
        assert_eq!(
          decode_system_control(&envelope(b"ASCN", 1, &node_with_paths(algorithm, internal, &paths)), algorithm).unwrap_err().class(),
          MalformedInputClass::NoncanonicalOrderOrDuplicate
        );
      }
      let canonical = node_with_paths(algorithm, internal, &[b"/", b"/a b", "/é".as_bytes()]);
      assert!(decode_system_control(&envelope(b"ASCN", 1, &canonical), algorithm).is_ok());
      let mut zero_database = canonical.clone();
      zero_database[..16].fill(0);
      assert!(decode_system_control(&envelope(b"ASCN", 1, &zero_database), algorithm).is_err());
      if internal {
        let mut zero_first = canonical;
        zero_first[32..32 + algorithm.hash_length()].fill(0);
        assert!(decode_system_control(&envelope(b"ASCN", 1, &zero_first), algorithm).is_err());
      }
    }
  }
}

#[test]
fn capture_typed_readers_refuse_wrong_kinds_and_bind_every_checkpoint_phase() {
  for algorithm in ALGORITHMS {
    let manifest = envelope(b"ASCM", 1, &manifest_body(algorithm));
    let node = envelope(b"ASCN", 1, &node_body(algorithm, false));
    assert!(decode_semantic_source_capture_v1(&node, algorithm).is_err());
    assert!(decode_semantic_source_node_v1(&manifest, algorithm).is_err());
    for phase in 1..=5 {
      let checkpoint = envelope(b"ASMC", 1, &checkpoint_body(algorithm, phase));
      let mut capture = manifest_body(algorithm);
      capture[112 + 5 * algorithm.hash_length()..].copy_from_slice(&independent_digest(algorithm, &checkpoint));
      let encoded = envelope(b"ASCM", 1, &capture);
      assert!(decode_semantic_source_capture_binding_v1(&encoded, &checkpoint, algorithm).is_ok(), "phase{phase}");
      capture[112 + 5 * algorithm.hash_length()] ^= 1;
      assert!(decode_semantic_source_capture_binding_v1(&envelope(b"ASCM", 1, &capture), &checkpoint, algorithm).is_err());
    }
    let mut maximum = manifest_body(algorithm);
    count(&mut maximum, 80, 0);
    count(&mut maximum, 88, u64::MAX / 2);
    count(&mut maximum, 96, u64::MAX - 2);
    count(&mut maximum, 104, u64::MAX - 2);
    assert!(decode_system_control(&envelope(b"ASCM", 1, &maximum), algorithm).is_ok());
  }
}

#[test]
fn source_capture_paths_are_strict_node_local_controls_not_logical_transfer_inputs() {
  use aeordb::engine::v4::system_family::{
    SystemFamilyClassificationV1, SystemFamilySubjectV1, TransferPolicyV1, VerifyPolicyV1, classify_system_family,
    embedded_system_family_registry,
  };
  for algorithm in ALGORITHMS {
    let registry = embedded_system_family_registry(algorithm).unwrap();
    for (magic, body) in
      [(b"ASCM", manifest_body(algorithm)), (b"ASCN", node_body(algorithm, false)), (b"ASCN", node_body(algorithm, true))]
    {
      let bytes = envelope(magic, 1, &body);
      let control = decode_system_control(&bytes, algorithm).unwrap();
      let path = control.canonical_path_for_slot(SystemControlSlotV1::Immutable).unwrap();
      let SystemFamilyClassificationV1::Known(family) = classify_system_family(registry, SystemFamilySubjectV1::Path(&path)).unwrap()
      else {
        panic!("capture must be protected");
      };
      assert_eq!(family.family_id, 0x0043);
      assert_eq!(family.policy.physical_copy_policy, TransferPolicyV1::RequiredInclude);
      assert_eq!(family.policy.logical_backup_policy, TransferPolicyV1::OmitDeclared);
      assert_eq!(family.policy.data_export_policy, TransferPolicyV1::OmitDeclared);
      assert_eq!(family.policy.peer_replication_policy, TransferPolicyV1::NodeLocal);
      assert_eq!(family.policy.cluster_join_policy, TransferPolicyV1::OmitDeclared);
      assert_eq!(family.policy.client_sync_policy, TransferPolicyV1::OmitDeclared);
      assert_eq!(family.policy.import_policy, TransferPolicyV1::NodeLocal);
      assert_eq!(family.policy.verify_policy, VerifyPolicyV1::StrictRequired);
    }
  }
}

#[test]
fn capture_records_refuse_wrong_width_and_mutable_pair_selection() {
  use aeordb::engine::v4::system_control::select_system_control_pair;
  for (algorithm, other) in [(HashAlgorithm::Blake3_256, HashAlgorithm::Sha512), (HashAlgorithm::Sha512, HashAlgorithm::Blake3_256)] {
    for (magic, body) in
      [(b"ASCM", manifest_body(algorithm)), (b"ASCN", node_body(algorithm, false)), (b"ASCN", node_body(algorithm, true))]
    {
      let bytes = envelope(magic, 1, &body);
      assert!(decode_system_control(&bytes, other).is_err());
      assert!(select_system_control_pair(algorithm, &bytes, &bytes).is_err());
    }
  }
}
