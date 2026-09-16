//! Independent Round 17 bodies: no production encoder supplies these bytes.
#[path = "semantic_mutation_sources_spec.rs"]
mod source_fingerprint;
use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use aeordb::engine::v4::semantic_mutation_control::{
  SemanticMutationCursorV1, SemanticMutationPhaseV1, SemanticMutationTaskStateV1, decode_semantic_mutation_checkpoint,
  decode_semantic_mutation_task, decode_semantic_mutation_selection, SemanticMutationReferenceRoleV1,
};
use aeordb::engine::v4::system_control::{SystemControlKindV1, SystemControlSlotV1, decode_system_control, select_system_control_pair};

const ALGORITHMS: [HashAlgorithm; 5] =
  [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512];

fn word(bytes: &mut [u8], offset: usize, value: u16) {
  bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn count(bytes: &mut [u8], offset: usize, value: u64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn envelope(magic: &[u8; 4], sequence: u64, body: &[u8]) -> Vec<u8> {
  let mut bytes = vec![0; 32];
  bytes[..4].copy_from_slice(magic);
  word(&mut bytes, 4, 1);
  word(&mut bytes, 6, 32);
  bytes[8..12].copy_from_slice(&((36 + body.len()) as u32).to_le_bytes());
  count(&mut bytes, 16, sequence);
  bytes[24..28].copy_from_slice(&(body.len() as u32).to_le_bytes());
  bytes.extend_from_slice(body);
  bytes.extend_from_slice(&crc32fast::hash(&bytes).to_le_bytes());
  bytes
}

fn task_body(algorithm: HashAlgorithm) -> Vec<u8> {
  let mut body = vec![0; 112 + algorithm.hash_length()];
  for index in 0..4 {
    body[index * 16..index * 16 + 16].fill(index as u8 + 1);
  }
  for offset in [64, 72, 100] {
    count(&mut body, offset, 1);
  }
  count(&mut body, 80, 100);
  count(&mut body, 88, 101);
  word(&mut body, 96, 1);
  body[112..].fill(0xa1);
  body
}

fn checkpoint_body(algorithm: HashAlgorithm, phase: u16) -> Vec<u8> {
  let width = algorithm.hash_length();
  let mut body = vec![0; 168 + 9 * width];
  body[..16].fill(1);
  body[16..32].fill(2);
  body[40..56].fill(3);
  for offset in [32, 56, 64, 72, 136] {
    count(&mut body, offset, 1);
  }
  count(&mut body, 80, 100);
  count(&mut body, 96, 2);
  word(&mut body, 88, phase);
  for slot in [0, 1, 6, 7, 8] {
    body[168 + slot * width..168 + (slot + 1) * width].fill(slot as u8 + 1);
  }
  if phase >= 2 {
    count(&mut body, 104, 2);
    count(&mut body, 112, 3);
    count(&mut body, 120, 5);
    count(&mut body, 128, 1);
    body[168 + 2 * width..168 + 3 * width].fill(3);
  }
  if phase >= 4 {
    body[168 + 4 * width..168 + 5 * width].fill(5);
    body[168 + 5 * width..168 + 6 * width].fill(6);
  }
  if phase == 5 {
    count(&mut body, 144, 2);
  }
  body
}

#[test]
fn independent_task_bodies_decode_at_every_registered_hash_width() {
  for algorithm in ALGORITHMS {
    for state in 1..=9 {
      let mut body = task_body(algorithm);
      word(&mut body, 96, state);
      let bytes = envelope(b"ASMT", 7, &body);
      let control = decode_system_control(&bytes, algorithm).expect("Round 17 task must be recognized");
      assert_eq!(control.kind as u16, 0x0044);
      assert_eq!(control.identity, [2; 16]);
      assert_eq!(control.sequence, 7);
      assert_eq!(control.body, body);
      assert!(!control.kind.is_immutable());
      assert!(control.canonical_path_for_slot(SystemControlSlotV1::A).unwrap().contains("/0044/"));
      assert!(control.canonical_path_for_slot(SystemControlSlotV1::Immutable).is_err());
    }
  }
}

#[test]
fn independent_checkpoint_phases_decode_without_fixed_hash_widths() {
  for algorithm in ALGORITHMS {
    for phase in 1..=5 {
      let body = checkpoint_body(algorithm, phase);
      let bytes = envelope(b"ASMC", 1, &body);
      let control = decode_system_control(&bytes, algorithm).expect("Round 17 checkpoint must be recognized");
      assert_eq!(control.kind as u16, 0x0045);
      assert_eq!(control.identity, [[2; 16].as_slice(), &1u64.to_le_bytes()].concat());
      assert!(control.kind.is_immutable());
      assert!(control.canonical_path_for_slot(SystemControlSlotV1::Immutable).unwrap().contains("/0045/"));
      assert!(control.canonical_path_for_slot(SystemControlSlotV1::A).is_err());
      assert!(decode_system_control(&envelope(b"ASMC", 2, &body), algorithm).is_err());
    }
  }
}

#[test]
fn generation_uses_the_existing_monotonic_selector_not_a_second_counter() {
  for algorithm in ALGORITHMS {
    let older = envelope(b"ASMG", 1, &[1; 16]);
    let newer = envelope(b"ASMG", 2, &[1; 16]);
    let selected = select_system_control_pair(algorithm, &older, &newer).expect("generation selection");
    assert_eq!(selected.control.kind as u16, 0x0046);
    assert!(selected.control.identity.is_empty());
    assert_eq!(selected.control.sequence, 2);
    assert_eq!(selected.selected_slot, SystemControlSlotV1::B);
    assert!(decode_system_control(&envelope(b"ASMG", 1, &[1; 17]), algorithm).is_err());
    assert!(decode_system_control(&envelope(b"ASMG", 0, &[1; 16]), algorithm).is_err());
  }
}

#[test]
fn new_capability_is_known_but_not_advertised_before_runtime_integration() {
  let assigned = CapabilitySetV1::from_bits([25]).expect("Round 17 assigns bit 25, preserving the bit24 corruption fixture");
  assert_eq!(assigned.bits(), vec![25]);
  assert_eq!(CapabilitySetV1::from_bytes(assigned.into_bytes()).unwrap(), assigned);
  let current = BinaryCapabilityProfileV1::current();
  assert!(!current.supported_reader_capabilities.contains(25));
  assert!(!current.supported_writer_capabilities.contains(25));
  for bit in 0..256 {
    let mut bytes = [0; 32];
    bytes[bit / 8] = 1 << (bit % 8);
    assert_eq!(CapabilitySetV1::from_bytes(bytes).is_ok(), bit < 24 || bit == 25, "bit {bit}");
  }
}

#[test]
fn malformed_task_bodies_fail_with_valid_outer_checksums() {
  for algorithm in ALGORITHMS {
    for (offset, length) in [(0, 16), (16, 16), (32, 16), (48, 16), (64, 8), (72, 8), (100, 8), (112, algorithm.hash_length())] {
      let mut body = task_body(algorithm);
      body[offset..offset + length].fill(0);
      assert!(decode_system_control(&envelope(b"ASMT", 1, &body), algorithm).is_err(), "zero field {offset}");
    }
    for (offset, value) in [(96, 0), (96, 10), (98, 1), (98, 2), (108, 1)] {
      let mut body = task_body(algorithm);
      word(&mut body, offset, value);
      assert!(decode_system_control(&envelope(b"ASMT", 1, &body), algorithm).is_err(), "field {offset} value {value}");
    }
    let mut body = task_body(algorithm);
    count(&mut body, 88, 99);
    assert!(decode_system_control(&envelope(b"ASMT", 1, &body), algorithm).is_err());
    count(&mut body, 80, u64::MAX);
    count(&mut body, 88, u64::MAX);
    assert!(decode_system_control(&envelope(b"ASMT", 1, &body), algorithm).is_err());
  }
}

#[test]
fn checkpoint_presence_counts_and_activation_arithmetic_fail_closed() {
  for algorithm in ALGORITHMS {
    for (offset, value) in
      [(32, 0), (56, 0), (64, 0), (72, 0), (136, 0), (104, 1), (112, 0), (120, 0), (120, 6), (128, 4), (144, 2), (152, 1), (160, 1)]
    {
      let mut body = checkpoint_body(algorithm, 4);
      count(&mut body, offset, value);
      assert!(decode_system_control(&envelope(b"ASMC", 1, &body), algorithm).is_err(), "field {offset} value {value}");
    }
    for slot in [0, 1, 2, 4, 5, 6, 7, 8] {
      let mut body = checkpoint_body(algorithm, 4);
      let width = algorithm.hash_length();
      body[168 + slot * width..168 + (slot + 1) * width].fill(0);
      assert!(decode_system_control(&envelope(b"ASMC", 1, &body), algorithm).is_err(), "hash slot {slot}");
    }
    let mut body = checkpoint_body(algorithm, 5);
    count(&mut body, 64, u64::MAX);
    count(&mut body, 144, 0);
    assert!(decode_system_control(&envelope(b"ASMC", 1, &body), algorithm).is_err());
  }
}

#[test]
fn every_new_envelope_truncation_and_trailing_byte_is_rejected() {
  for algorithm in ALGORITHMS {
    for bytes in
      [envelope(b"ASMT", 1, &task_body(algorithm)), envelope(b"ASMC", 1, &checkpoint_body(algorithm, 4)), envelope(b"ASMG", 1, &[1; 16])]
    {
      for length in 0..bytes.len() {
        assert!(decode_system_control(&bytes[..length], algorithm).is_err(), "prefix {length}");
      }
      let mut trailing = bytes;
      trailing.push(0);
      assert!(decode_system_control(&trailing, algorithm).is_err());
    }
  }
}

#[test]
fn legacy_task_pin_kind_registry_stays_closed_and_old_bytes_still_decode() {
  let bytes =
    std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/spec/fixtures/v4/system-control-v1/control-blake3-256-task-pin-valid.bin"))
      .unwrap();
  let old = decode_system_control(&bytes, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(old.kind, SystemControlKindV1::TaskPin);
  let mut body = old.body.to_vec();
  word(&mut body, 32, 12);
  assert!(decode_system_control(&envelope(b"ATPN", 7, &body), HashAlgorithm::Blake3_256).is_err());
}

#[test]
fn typed_readers_borrow_fields_and_refuse_other_control_kinds() {
  for algorithm in ALGORITHMS {
    let task = envelope(b"ASMT", 7, &task_body(algorithm));
    let checkpoint = envelope(b"ASMC", 1, &checkpoint_body(algorithm, 4));
    let decoded_task = decode_semantic_mutation_task(&task, algorithm).unwrap();
    assert_eq!(decoded_task.state, SemanticMutationTaskStateV1::Queued);
    assert_eq!(decoded_task.control_sequence, 7);
    assert_eq!(decoded_task.database_id.as_ptr(), task[32..].as_ptr());
    let decoded_checkpoint = decode_semantic_mutation_checkpoint(&checkpoint, algorithm).unwrap();
    assert_eq!(decoded_checkpoint.phase, SemanticMutationPhaseV1::Ready);
    assert_eq!(decoded_checkpoint.cursor, SemanticMutationCursorV1::None);
    assert_eq!(decoded_checkpoint.database_id.as_ptr(), checkpoint[32..].as_ptr());
    assert_eq!(decoded_checkpoint.compiler_fingerprint.len(), algorithm.hash_length());
    assert_eq!(decode_semantic_mutation_task(&checkpoint, algorithm).unwrap_err().code(), "semantic_task_kind");
    assert_eq!(decode_semantic_mutation_checkpoint(&task, algorithm).unwrap_err().code(), "semantic_task_kind");
  }
}

fn cursor_body(algorithm: HashAlgorithm, phase: u16, kind: u16, cursor: &[u8]) -> Vec<u8> {
  let mut body = checkpoint_body(algorithm, phase);
  word(&mut body, 90, kind);
  body[92..96].copy_from_slice(&(cursor.len() as u32).to_le_bytes());
  body.extend_from_slice(cursor);
  body
}

#[test]
fn typed_cursors_validate_phase_path_width_and_explicit_absence() {
  for algorithm in ALGORITHMS {
    for path in ["/", "/photos", "/資料/2026", "/back\\slash"] {
      let bytes = envelope(b"ASMC", 1, &cursor_body(algorithm, 2, 1, path.as_bytes()));
      assert_eq!(
        decode_semantic_mutation_checkpoint(&bytes, algorithm).unwrap().cursor,
        SemanticMutationCursorV1::ConfigurationOwner(path)
      );
    }
    let dependency = vec![0x42; algorithm.hash_length()];
    let bytes = envelope(b"ASMC", 1, &cursor_body(algorithm, 3, 2, &dependency));
    assert_eq!(decode_semantic_mutation_checkpoint(&bytes, algorithm).unwrap().cursor, SemanticMutationCursorV1::DependencyID(&dependency));
    for path in ["", "relative", "/trailing/", "/a//b", "/a/./b", "/a/../b", "/zero\0", "/trailing "] {
      assert!(decode_system_control(&envelope(b"ASMC", 1, &cursor_body(algorithm, 2, 1, path.as_bytes())), algorithm).is_err(), "{path:?}");
    }
    for (phase, kind, cursor) in [(1, 1, b"/path".as_slice()), (3, 1, b"/path"), (4, 1, b"/path"), (2, 0, b"/path"), (2, 3, b"")] {
      assert!(decode_system_control(&envelope(b"ASMC", 1, &cursor_body(algorithm, phase, kind, cursor)), algorithm).is_err());
    }
    for width in [0, algorithm.hash_length() - 1, algorithm.hash_length() + 1] {
      assert!(decode_system_control(&envelope(b"ASMC", 1, &cursor_body(algorithm, 3, 2, &vec![1; width])), algorithm).is_err());
    }
    assert!(
      decode_system_control(&envelope(b"ASMC", 1, &cursor_body(algorithm, 3, 2, &vec![0; algorithm.hash_length()])), algorithm).is_err()
    );
    assert!(decode_system_control(&envelope(b"ASMC", 1, &cursor_body(algorithm, 2, 1, &[0xff])), algorithm).is_err());
  }
}

#[test]
fn terminal_pin_release_is_explicit_and_never_inferred_from_state() {
  for state in 1..=9 {
    let mut body = task_body(HashAlgorithm::Blake3_256);
    word(&mut body, 96, state);
    let bytes = envelope(b"ASMT", 1, &body);
    assert!(!decode_semantic_mutation_task(&bytes, HashAlgorithm::Blake3_256).unwrap().pins_released);
    word(&mut body, 98, 1);
    let bytes = envelope(b"ASMT", 1, &body);
    assert_eq!(decode_semantic_mutation_task(&bytes, HashAlgorithm::Blake3_256).is_ok(), state >= 6);
  }
}

#[test]
fn bounded_cursor_limit_and_impossible_counts_are_rejected() {
  let algorithm = HashAlgorithm::Blake3_256;
  let mut cursor = vec![b'x'; 65_535];
  cursor[0] = b'/';
  assert!(decode_system_control(&envelope(b"ASMC", 1, &cursor_body(algorithm, 2, 1, &cursor)), algorithm).is_ok());
  cursor.push(b'x');
  assert_eq!(
    decode_system_control(&envelope(b"ASMC", 1, &cursor_body(algorithm, 2, 1, &cursor)), algorithm).unwrap_err().code(),
    "semantic_task_cursor_cap"
  );
  let mut body = checkpoint_body(algorithm, 2);
  count(&mut body, 112, u64::MAX);
  assert!(decode_system_control(&envelope(b"ASMC", 1, &body), algorithm).is_err());
  for phase in [0, 6, u16::MAX] {
    let mut body = checkpoint_body(algorithm, 2);
    word(&mut body, 88, phase);
    assert_eq!(decode_system_control(&envelope(b"ASMC", 1, &body), algorithm).unwrap_err().code(), "semantic_task_phase");
  }
}

#[test]
fn known_sparse_capability_reaches_header_admission_but_current_binary_refuses_it() {
  use aeordb::engine::v4::admission::{AdmissionModeV1, admit_v4_header};
  use aeordb::engine::v4::database_header::decode_header_region;
  let mut bytes =
    std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/spec/fixtures/v4/database-header-v4/header-blake3-256-valid-ab.bin")).unwrap();
  for slot in bytes.chunks_exact_mut(1_024) {
    slot[61] |= 2;
    slot[355] |= 2;
    let crc = crc32fast::hash(&slot[..1_020]);
    slot[1_020..].copy_from_slice(&crc.to_le_bytes());
  }
  let selected = decode_header_region(&bytes).unwrap();
  let error = admit_v4_header(&selected, AdmissionModeV1::SemanticReadOnly, BinaryCapabilityProfileV1::current(), None).unwrap_err();
  assert_eq!(error.code(), "missing_reader_capabilities");
  assert_eq!(error.capability_bits(), &[25]);
  for slot in bytes.chunks_exact_mut(1_024) {
    slot[61] |= 1;
    let crc = crc32fast::hash(&slot[..1_020]);
    slot[1_020..].copy_from_slice(&crc.to_le_bytes());
  }
  assert!(decode_header_region(&bytes).is_err(), "bit24 remains unassigned");
}

#[test]
fn new_control_paths_inherit_the_frozen_node_local_transfer_policy() {
  use aeordb::engine::v4::system_family::{
    SystemFamilyClassificationV1, SystemFamilySubjectV1, TransferPolicyV1, VerifyPolicyV1, classify_system_family,
    embedded_system_family_registry,
  };
  let registry = embedded_system_family_registry(HashAlgorithm::Blake3_256).unwrap();
  for bytes in [
    envelope(b"ASMT", 1, &task_body(HashAlgorithm::Blake3_256)),
    envelope(b"ASMC", 1, &checkpoint_body(HashAlgorithm::Blake3_256, 4)),
    envelope(b"ASMG", 1, &[1; 16]),
  ] {
    let control = decode_system_control(&bytes, HashAlgorithm::Blake3_256).unwrap();
    let path = control.canonical_path();
    let SystemFamilyClassificationV1::Known(family) = classify_system_family(registry, SystemFamilySubjectV1::Path(&path)).unwrap() else {
      panic!("new control path must retain its frozen protected family");
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

fn selected_pair(algorithm: HashAlgorithm, state: u16, checkpoint: &[u8]) -> Vec<u8> {
  use sha2::Digest;
  let digest = match algorithm {
    HashAlgorithm::Blake3_256 => blake3::hash(checkpoint).as_bytes().to_vec(),
    HashAlgorithm::Sha256 => sha2::Sha256::digest(checkpoint).to_vec(),
    HashAlgorithm::Sha512 => sha2::Sha512::digest(checkpoint).to_vec(),
    HashAlgorithm::Sha3_256 => sha3::Sha3_256::digest(checkpoint).to_vec(),
    HashAlgorithm::Sha3_512 => sha3::Sha3_512::digest(checkpoint).to_vec(),
  };
  let mut body = task_body(algorithm);
  word(&mut body, 96, state);
  body[112..].copy_from_slice(&digest);
  envelope(b"ASMT", 7, &body)
}

#[test]
fn selected_checkpoint_closure_exhausts_all_state_phase_pairs_at_every_hash_width() {
  for algorithm in ALGORITHMS {
    for state in 1..=9 {
      for phase in 1..=5 {
        let checkpoint = envelope(b"ASMC", 1, &checkpoint_body(algorithm, phase));
        let task = selected_pair(algorithm, state, &checkpoint);
        let expected = match state {
          1 | 2 => phase == 1,
          3 => phase == 2 || phase == 3,
          4 | 5 => phase == 4,
          6 => phase == 5,
          7..=9 => phase != 5,
          _ => unreachable!(),
        };
        assert_eq!(decode_semantic_mutation_selection(&task, &checkpoint, algorithm).is_ok(), expected, "{algorithm:?}/{state}/{phase}");
      }
    }
  }
}

#[test]
fn selected_checkpoint_digest_identity_fence_and_time_are_not_interchangeable() {
  for algorithm in ALGORITHMS {
    let checkpoint = envelope(b"ASMC", 1, &checkpoint_body(algorithm, 4));
    let task = selected_pair(algorithm, 4, &checkpoint);
    let mut later_epoch = decode_system_control(&task, algorithm).unwrap().body.to_vec();
    count(&mut later_epoch, 72, 2);
    assert!(decode_semantic_mutation_selection(&envelope(b"ASMT", 8, &later_epoch), &checkpoint, algorithm).is_ok());
    for offset in [0, 16, 32, 72, 80, 88, 100, 112] {
      let mut body = decode_system_control(&task, algorithm).unwrap().body.to_vec();
      if offset == 72 {
        // A new task epoch may take over an old capture, but an old epoch may
        // never own a newer capture. Raise the captured epoch below instead.
        let mut newer = checkpoint_body(algorithm, 4);
        count(&mut newer, 56, 2);
        let newer = envelope(b"ASMC", 1, &newer);
        let matching_digest = selected_pair(algorithm, 4, &newer);
        assert_eq!(
          decode_semantic_mutation_selection(&matching_digest, &newer, algorithm).unwrap_err().code(),
          "semantic_task_checkpoint_identity"
        );
        continue;
      }
      if offset == 80 {
        count(&mut body, 80, 101);
      } else if offset == 88 {
        count(&mut body, 80, 99);
        count(&mut body, 88, 99);
      } else {
        body[offset] ^= 0x40;
      }
      let changed = envelope(b"ASMT", 7, &body);
      let error = decode_semantic_mutation_selection(&changed, &checkpoint, algorithm).unwrap_err();
      let expected = match offset {
        112 => "semantic_task_checkpoint_digest",
        80 | 88 => "semantic_task_checkpoint_time",
        _ => "semantic_task_checkpoint_identity",
      };
      assert_eq!(error.code(), expected, "offset {offset}");
    }
  }
}

#[test]
fn typed_checkpoint_edges_exclude_fingerprints_and_never_admit_the_candidate() {
  use SemanticMutationReferenceRoleV1 as Role;
  for algorithm in ALGORITHMS {
    let captured = envelope(b"ASMC", 1, &checkpoint_body(algorithm, 1));
    let captured = decode_semantic_mutation_checkpoint(&captured, algorithm).unwrap();
    assert_eq!(
      captured.references().map(|edge| edge.0).collect::<Vec<_>>(),
      vec![Role::AdmittedBaseNamespaceRoot, Role::StagedDirectoryTree]
    );
    let ready = envelope(b"ASMC", 1, &checkpoint_body(algorithm, 4));
    let ready = decode_semantic_mutation_checkpoint(&ready, algorithm).unwrap();
    assert_eq!(
      ready.references().map(|edge| edge.0).collect::<Vec<_>>(),
      vec![
        Role::AdmittedBaseNamespaceRoot,
        Role::StagedDirectoryTree,
        Role::SemanticCatalog,
        Role::CompiledSemanticState,
        Role::StagedCandidateNamespaceRoot,
      ]
    );
    for (_, hash) in ready.references() {
      assert!(![ready.compiler_fingerprint, ready.semantic_registry_fingerprint, ready.source_identity_fingerprint].contains(&hash));
    }
    let mut pruning = checkpoint_body(algorithm, 3);
    let width = algorithm.hash_length();
    pruning[168 + 3 * width..168 + 4 * width].fill(4);
    count(&mut pruning, 152, 1);
    count(&mut pruning, 160, 1);
    let pruning = envelope(b"ASMC", 1, &pruning);
    let pruning = decode_semantic_mutation_checkpoint(&pruning, algorithm).unwrap();
    assert!(pruning.references().any(|edge| edge.0 == Role::PruningCandidateCatalog));
  }
}
