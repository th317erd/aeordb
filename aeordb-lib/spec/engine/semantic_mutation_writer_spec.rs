//! Writer targets use the reader suite's already-frozen independent bodies.
//! This child shares the independently hand-built reader fixtures, not encoders.
use super::{ALGORITHMS, checkpoint_body, count, envelope, task_body, word};
use aeordb::engine::v4::semantic_mutation_control::{
  SemanticMutationCheckpointV1, SemanticMutationCursorV1, SemanticMutationTaskV1, decode_semantic_mutation_checkpoint,
  decode_semantic_mutation_selection, decode_semantic_mutation_task, encode_semantic_mutation_checkpoint,
  encode_semantic_mutation_generation, encode_semantic_mutation_task,
};
use aeordb::engine::v4::system_control::{SystemControlSlotV1, decode_system_control, select_system_control_pair};
use aeordb::engine::HashAlgorithm;

#[test]
fn byte_writers_match_all_six_frozen_reference_fixtures() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    for kind in ["task", "checkpoint", "generation"] {
      let expected = std::fs::read(format!(
        "{}/spec/fixtures/v4/system-control-v1/control-{profile}-semantic-mutation-{kind}-valid.bin",
        env!("CARGO_MANIFEST_DIR"),
      ))
      .unwrap();
      let encoded = match kind {
        "task" => encode_semantic_mutation_task(&decode_semantic_mutation_task(&expected, algorithm).unwrap(), algorithm),
        "checkpoint" => encode_semantic_mutation_checkpoint(&decode_semantic_mutation_checkpoint(&expected, algorithm).unwrap(), algorithm),
        "generation" => {
          let control = decode_system_control(&expected, algorithm).unwrap();
          encode_semantic_mutation_generation(control.database_id, control.sequence, algorithm)
        }
        _ => unreachable!(),
      }
      .unwrap();
      assert_eq!(encoded, expected, "{profile}/{kind}");
    }
  }
}

#[test]
fn every_task_state_and_sequence_encodes_exact_independent_bytes() {
  for algorithm in ALGORITHMS {
    for state in 1..=9 {
      for sequence in [1, 0x1234_5678_9abc_def0, u64::MAX] {
        let mut body = task_body(algorithm);
        word(&mut body, 96, state);
        word(&mut body, 98, u16::from(state >= 6));
        count(&mut body, 64, 0x1234_5678_9abc_def0);
        count(&mut body, 72, u64::MAX);
        count(&mut body, 80, 0);
        count(&mut body, 88, i64::MAX as u64);
        count(&mut body, 100, 0xfedc_ba98_7654_3210);
        let expected = envelope(b"ASMT", sequence, &body);
        let input = decode_semantic_mutation_task(&expected, algorithm).unwrap();
        assert_eq!(encode_semantic_mutation_task(&input, algorithm).unwrap(), expected);
      }
    }
  }
}

#[test]
fn every_checkpoint_phase_and_optional_hash_encodes_exact_independent_bytes() {
  for algorithm in ALGORITHMS {
    for phase in 1..=5 {
      let mut body = checkpoint_body(algorithm, phase);
      for offset in [32, 56, 72, 136] {
        count(&mut body, offset, 0x1234_5678_9abc_def0 + offset as u64);
      }
      count(&mut body, 64, u64::MAX - 1);
      count(&mut body, 80, i64::MAX as u64);
      if phase == 5 {
        count(&mut body, 144, u64::MAX);
      }
      if phase == 3 {
        let width = algorithm.hash_length();
        body[168 + 3 * width..168 + 4 * width].fill(0x94);
        count(&mut body, 152, 2);
        count(&mut body, 160, 3);
      }
      let expected = envelope(b"ASMC", 1, &body);
      let input = decode_semantic_mutation_checkpoint(&expected, algorithm).unwrap();
      let actual = encode_semantic_mutation_checkpoint(&input, algorithm).unwrap();
      assert_eq!(actual, expected, "{algorithm:?}/{phase}");
      assert_eq!(decode_system_control(&actual, algorithm).unwrap().sequence, 1);
    }
  }
}

#[test]
fn both_cursor_types_keep_their_exact_utf8_or_selected_hash_bytes() {
  let maximum_path = format!("/{}", "x".repeat(65_534));
  for algorithm in ALGORITHMS {
    let dependency = vec![0x92; algorithm.hash_length()];
    for (phase, kind, cursor) in
      [(2, 1, "/".as_bytes()), (2, 1, "/config/é.json".as_bytes()), (2, 1, maximum_path.as_bytes()), (3, 2, dependency.as_slice())]
    {
      let mut body = checkpoint_body(algorithm, phase);
      word(&mut body, 90, kind);
      body[92..96].copy_from_slice(&(cursor.len() as u32).to_le_bytes());
      body.extend_from_slice(cursor);
      let expected = envelope(b"ASMC", 1, &body);
      let input = decode_semantic_mutation_checkpoint(&expected, algorithm).unwrap();
      assert_eq!(encode_semantic_mutation_checkpoint(&input, algorithm).unwrap(), expected);
    }
  }
}

#[test]
fn task_writer_rejects_invalid_widths_zero_identities_and_state_closure() {
  for algorithm in ALGORITHMS {
    let bytes = envelope(b"ASMT", 1, &task_body(algorithm));
    let input = decode_semantic_mutation_task(&bytes, algorithm).unwrap();
    for field in 0..5 {
      let width = if field == 4 { algorithm.hash_length() } else { 16 };
      for invalid in [vec![], vec![1; width - 1], vec![1; width + 1], vec![0; width]] {
        let mut changed = input;
        match field {
          0 => changed.database_id = &invalid,
          1 => changed.task_id = &invalid,
          2 => changed.physical_instance_id = &invalid,
          3 => changed.holder_boot_id = &invalid,
          4 => changed.checkpoint_payload_hash = &invalid,
          _ => unreachable!(),
        }
        assert!(encode_semantic_mutation_task(&changed, algorithm).is_err(), "field {field}, length {}", invalid.len());
      }
    }
    for invalid in [
      SemanticMutationTaskV1 { control_sequence: 0, ..input },
      SemanticMutationTaskV1 { fencing_token: 0, ..input },
      SemanticMutationTaskV1 { writer_fence_epoch: 0, ..input },
      SemanticMutationTaskV1 { checkpoint_sequence: 0, ..input },
      SemanticMutationTaskV1 { created_at_ms: -1, ..input },
      SemanticMutationTaskV1 { created_at_ms: i64::MAX, ..input },
      SemanticMutationTaskV1 { updated_at_ms: -1, ..input },
      SemanticMutationTaskV1 { updated_at_ms: 99, ..input },
      SemanticMutationTaskV1 { pins_released: true, ..input },
    ] {
      assert!(encode_semantic_mutation_task(&invalid, algorithm).is_err());
    }
  }
}

#[test]
fn checkpoint_writer_rejects_every_wrong_width_and_present_zero_hash() {
  for algorithm in ALGORITHMS {
    let bytes = envelope(b"ASMC", 1, &checkpoint_body(algorithm, 2));
    let input = decode_semantic_mutation_checkpoint(&bytes, algorithm).unwrap();
    for field in 0..12 {
      let width = if field < 3 { 16 } else { algorithm.hash_length() };
      for invalid in [vec![], vec![1; width - 1], vec![1; width + 1], vec![0; width]] {
        let mut changed = input;
        match field {
          0 => changed.database_id = &invalid,
          1 => changed.task_id = &invalid,
          2 => changed.physical_instance_id = &invalid,
          3 => changed.base_namespace_root = &invalid,
          4 => changed.staged_directory_root = &invalid,
          5 => changed.catalog_root = Some(&invalid),
          6 => changed.pruning_catalog_root = Some(&invalid),
          7 => changed.semantic_state = Some(&invalid),
          8 => changed.candidate_namespace_root = Some(&invalid),
          9 => changed.compiler_fingerprint = &invalid,
          10 => changed.semantic_registry_fingerprint = &invalid,
          11 => changed.source_identity_fingerprint = &invalid,
          _ => unreachable!(),
        }
        assert!(encode_semantic_mutation_checkpoint(&changed, algorithm).is_err(), "field {field}, length {}", invalid.len());
      }
    }
  }
}

#[test]
fn checkpoint_writer_rejects_invalid_counters_cursors_and_ready_closure() {
  for algorithm in ALGORITHMS {
    let bytes = envelope(b"ASMC", 1, &checkpoint_body(algorithm, 4));
    let input = decode_semantic_mutation_checkpoint(&bytes, algorithm).unwrap();
    for invalid in [
      SemanticMutationCheckpointV1 { checkpoint_sequence: 0, ..input },
      SemanticMutationCheckpointV1 { writer_fence_epoch: 0, ..input },
      SemanticMutationCheckpointV1 { semantic_generation: 0, ..input },
      SemanticMutationCheckpointV1 { header_sequence: 0, ..input },
      SemanticMutationCheckpointV1 { mutation_count: 0, ..input },
      SemanticMutationCheckpointV1 { captured_at_ms: -1, ..input },
      SemanticMutationCheckpointV1 { configuration_count: 1, ..input },
      SemanticMutationCheckpointV1 { record_count: u64::MAX, ..input },
      SemanticMutationCheckpointV1 { node_count: 6, ..input },
      SemanticMutationCheckpointV1 { dependency_count: 4, ..input },
      SemanticMutationCheckpointV1 { activation_generation: 1, ..input },
      SemanticMutationCheckpointV1 { pruning_record_count: 1, ..input },
      SemanticMutationCheckpointV1 { pruning_node_count: 1, ..input },
      SemanticMutationCheckpointV1 { catalog_root: None, ..input },
      SemanticMutationCheckpointV1 { semantic_state: None, ..input },
      SemanticMutationCheckpointV1 { candidate_namespace_root: None, ..input },
      SemanticMutationCheckpointV1 { cursor: SemanticMutationCursorV1::ConfigurationOwner("/unfinished"), ..input },
    ] {
      assert!(encode_semantic_mutation_checkpoint(&invalid, algorithm).is_err());
    }
    let compiling = envelope(b"ASMC", 1, &checkpoint_body(algorithm, 2));
    let input = decode_semantic_mutation_checkpoint(&compiling, algorithm).unwrap();
    let overlong = format!("/{}", "x".repeat(65_535));
    let zero_dependency = vec![0; algorithm.hash_length()];
    for cursor in [
      SemanticMutationCursorV1::ConfigurationOwner(""),
      SemanticMutationCursorV1::ConfigurationOwner("relative"),
      SemanticMutationCursorV1::ConfigurationOwner("/a/../b"),
      SemanticMutationCursorV1::ConfigurationOwner("/a\0b"),
      SemanticMutationCursorV1::ConfigurationOwner(&overlong),
      SemanticMutationCursorV1::DependencyID(&zero_dependency),
    ] {
      assert!(encode_semantic_mutation_checkpoint(&SemanticMutationCheckpointV1 { cursor, ..input }, algorithm).is_err());
    }
    let pruning = envelope(b"ASMC", 1, &checkpoint_body(algorithm, 3));
    let input = decode_semantic_mutation_checkpoint(&pruning, algorithm).unwrap();
    let width = algorithm.hash_length();
    for invalid in [vec![], vec![1; width - 1], vec![1; width + 1], vec![0; width]] {
      let cursor = SemanticMutationCursorV1::DependencyID(&invalid);
      assert!(encode_semantic_mutation_checkpoint(&SemanticMutationCheckpointV1 { cursor, ..input }, algorithm).is_err());
    }
    let activated = envelope(b"ASMC", 1, &checkpoint_body(algorithm, 5));
    let input = decode_semantic_mutation_checkpoint(&activated, algorithm).unwrap();
    let overflow = SemanticMutationCheckpointV1 { semantic_generation: u64::MAX, activation_generation: 0, ..input };
    assert!(encode_semantic_mutation_checkpoint(&overflow, algorithm).is_err());
  }
}

#[test]
fn generation_writers_preserve_selection_exhaustion_and_input_rejection() {
  for algorithm in ALGORITHMS {
    for sequence in [1, 7, u64::MAX] {
      assert_eq!(encode_semantic_mutation_generation(&[1; 16], sequence, algorithm).unwrap(), envelope(b"ASMG", sequence, &[1; 16]));
    }
    for invalid in [vec![], vec![1; 15], vec![1; 17], vec![0; 16]] {
      assert!(encode_semantic_mutation_generation(&invalid, 1, algorithm).is_err());
    }
    assert!(encode_semantic_mutation_generation(&[1; 16], 0, algorithm).is_err());
    let older = encode_semantic_mutation_generation(&[1; 16], u64::MAX - 1, algorithm).unwrap();
    let last = encode_semantic_mutation_generation(&[1; 16], u64::MAX, algorithm).unwrap();
    let selected = select_system_control_pair(algorithm, &older, &last).unwrap();
    assert_eq!(selected.selected_slot, SystemControlSlotV1::B);
    assert_eq!(selected.control.sequence.checked_add(1), None);
  }
}

#[test]
fn encoded_task_binds_its_exact_checkpoint_envelope_and_state() {
  use aeordb::engine::v4::hash::digest_parts;
  use aeordb::engine::v4::semantic_mutation_control::SemanticMutationTaskStateV1;
  for algorithm in ALGORITHMS {
    let expected_checkpoint = envelope(b"ASMC", 1, &checkpoint_body(algorithm, 4));
    let checkpoint_input = decode_semantic_mutation_checkpoint(&expected_checkpoint, algorithm).unwrap();
    let checkpoint = encode_semantic_mutation_checkpoint(&checkpoint_input, algorithm).unwrap();
    let payload_hash = digest_parts(algorithm, &[&checkpoint]);
    let task_fixture = envelope(b"ASMT", 7, &task_body(algorithm));
    let task_input = SemanticMutationTaskV1 {
      checkpoint_payload_hash: &payload_hash,
      state: SemanticMutationTaskStateV1::ReadyToActivate,
      ..decode_semantic_mutation_task(&task_fixture, algorithm).unwrap()
    };
    let task = encode_semantic_mutation_task(&task_input, algorithm).unwrap();
    assert!(decode_semantic_mutation_selection(&task, &checkpoint, algorithm).is_ok());
    let next_checkpoint =
      encode_semantic_mutation_checkpoint(&SemanticMutationCheckpointV1 { checkpoint_sequence: 2, ..checkpoint_input }, algorithm).unwrap();
    assert_eq!(
      decode_semantic_mutation_selection(&task, &next_checkpoint, algorithm).unwrap_err().code(),
      "semantic_task_checkpoint_digest"
    );
  }
}
