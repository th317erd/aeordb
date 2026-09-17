//! Round 17 byte-only encoders. No capture, publication, or retention authority.
use super::{
  CHECKPOINT_FIXED_BYTES, CHECKPOINT_HASH_COUNT, CURSOR_CAP, SemanticMutationCheckpointV1, SemanticMutationCursorV1, SemanticMutationTaskV1,
};
use super::super::reader::{FormatError, FormatResult, MalformedInputClass};
use super::super::scope::validate_canonical_absolute_path;
use super::super::system_control::{SystemControlKindV1, encode_system_control_with_body};
use crate::engine::HashAlgorithm;

/// Encode the requested sequence exactly; selecting or advancing it belongs to
/// the fenced task owner. The checkpoint payload binding is checked separately.
pub fn encode_semantic_mutation_task(task: &SemanticMutationTaskV1<'_>, algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  let identities = [task.database_id, task.task_id, task.physical_instance_id, task.holder_boot_id];
  for identity in identities {
    require_field(identity, 16)?;
  }
  require_field(task.checkpoint_payload_hash, algorithm.hash_length())?;
  encode_system_control_with_body(
    SystemControlKindV1::SemanticMutationTask,
    task.control_sequence,
    112 + algorithm.hash_length(),
    algorithm,
    |body| {
      for (index, identity) in identities.into_iter().enumerate() {
        body[index * 16..(index + 1) * 16].copy_from_slice(identity);
      }
      put_u64(body, 64, task.fencing_token);
      put_u64(body, 72, task.writer_fence_epoch);
      body[80..88].copy_from_slice(&task.created_at_ms.to_le_bytes());
      body[88..96].copy_from_slice(&task.updated_at_ms.to_le_bytes());
      body[96..98].copy_from_slice(&(task.state as u16).to_le_bytes());
      body[98..100].copy_from_slice(&u16::from(task.pins_released).to_le_bytes());
      put_u64(body, 100, task.checkpoint_sequence);
      // The shared zero-initialized output retains the four reserved bytes.
      body[112..].copy_from_slice(task.checkpoint_payload_hash);
    },
  )
}

/// Serialize an immutable checkpoint with envelope sequence one. Claimed counts
/// and hashes must satisfy the byte contract, but are not compiled admission.
pub fn encode_semantic_mutation_checkpoint(
  checkpoint: &SemanticMutationCheckpointV1<'_>,
  algorithm: HashAlgorithm,
) -> FormatResult<Vec<u8>> {
  let width = algorithm.hash_length();
  for identity in [checkpoint.database_id, checkpoint.task_id, checkpoint.physical_instance_id] {
    require_field(identity, 16)?;
  }
  let hashes = [
    Some(checkpoint.base_namespace_root),
    Some(checkpoint.staged_directory_root),
    checkpoint.catalog_root,
    checkpoint.pruning_catalog_root,
    checkpoint.semantic_state,
    checkpoint.candidate_namespace_root,
    Some(checkpoint.compiler_fingerprint),
    Some(checkpoint.semantic_registry_fingerprint),
    Some(checkpoint.source_identity_fingerprint),
  ];
  for hash in hashes.into_iter().flatten() {
    // Some(zero) is not silently normalized into None by the encoder.
    require_field(hash, width)?;
  }
  let (cursor_kind, cursor): (u16, &[u8]) = match checkpoint.cursor {
    SemanticMutationCursorV1::None => (0, &[]),
    SemanticMutationCursorV1::ConfigurationOwner(path) => (1, path.as_bytes()),
    SemanticMutationCursorV1::DependencyID(identity) => (2, identity),
  };
  if cursor.len() > CURSOR_CAP {
    return Err(FormatError::new(
      MalformedInputClass::AllocationAmplification,
      "semantic_task_cursor_cap",
      "checkpoint cursor exceeds 65,535 bytes",
    ));
  }
  match checkpoint.cursor {
    SemanticMutationCursorV1::ConfigurationOwner(path) => validate_canonical_absolute_path(path)?,
    SemanticMutationCursorV1::DependencyID(identity) => require_field(identity, width)?,
    SemanticMutationCursorV1::None => {}
  }
  let cursor_offset = CHECKPOINT_FIXED_BYTES + CHECKPOINT_HASH_COUNT * width;
  encode_system_control_with_body(SystemControlKindV1::SemanticMutationCheckpoint, 1, cursor_offset + cursor.len(), algorithm, |body| {
    body[..16].copy_from_slice(checkpoint.database_id);
    body[16..32].copy_from_slice(checkpoint.task_id);
    body[40..56].copy_from_slice(checkpoint.physical_instance_id);
    for (offset, value) in [
      (32, checkpoint.checkpoint_sequence),
      (56, checkpoint.writer_fence_epoch),
      (64, checkpoint.semantic_generation),
      (72, checkpoint.header_sequence),
      (96, checkpoint.expected_configuration_count),
      (104, checkpoint.configuration_count),
      (112, checkpoint.record_count),
      (120, checkpoint.node_count),
      (128, checkpoint.dependency_count),
      (136, checkpoint.mutation_count),
      (144, checkpoint.activation_generation),
      (152, checkpoint.pruning_record_count),
      (160, checkpoint.pruning_node_count),
    ] {
      put_u64(body, offset, value);
    }
    body[80..88].copy_from_slice(&checkpoint.captured_at_ms.to_le_bytes());
    body[88..90].copy_from_slice(&(checkpoint.phase as u16).to_le_bytes());
    body[90..92].copy_from_slice(&cursor_kind.to_le_bytes());
    body[92..96].copy_from_slice(&(cursor.len() as u32).to_le_bytes());
    for (index, hash) in hashes.into_iter().enumerate() {
      if let Some(hash) = hash {
        let start = CHECKPOINT_FIXED_BYTES + index * width;
        body[start..start + width].copy_from_slice(hash);
      }
    }
    body[cursor_offset..].copy_from_slice(cursor);
  })
}

/// Sequence is the generation itself. This does not increment or publish it.
pub fn encode_semantic_mutation_generation(database_id: &[u8], sequence: u64, algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  require_field(database_id, 16)?;
  encode_system_control_with_body(SystemControlKindV1::SemanticMutationGeneration, sequence, 16, algorithm, |body| {
    body.copy_from_slice(database_id);
  })
}

fn require_field(bytes: &[u8], width: usize) -> FormatResult<()> {
  if bytes.len() != width {
    return Err(FormatError::new(
      MalformedInputClass::LengthCountOrArithmeticOverflow,
      "semantic_task_width",
      "semantic mutation field has the wrong width",
    ));
  }
  super::nonzero(bytes)
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
