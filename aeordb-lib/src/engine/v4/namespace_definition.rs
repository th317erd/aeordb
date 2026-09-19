//! Validated kind-4 wrapping, separate from source-configuration compilation.

use super::{
  EncodedSemanticObjectV1, FormatResult, HashAlgorithm, MalformedInputClass, SEMANTIC_HARD_CAP, SEMANTIC_HEADER_LENGTH, checked_add,
  checked_u32, decode_dependency_record_bytes, digest_parts, error, immutable_id, put_u16, put_u32, put_u64, require_nonzero_hash,
  write_trailing_crc,
};
use super::super::config_value::{CanonicalValueBounds, validate_canonical_value};
use super::super::field_definition::decode_field_index_definition;
use super::super::scope::decode_scope_definition;
use super::super::value_store::decode_value_store_definition;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedSemanticDefinitionObjectV1 {
  pub semantic_id: Vec<u8>,
  pub object: EncodedSemanticObjectV1,
}

/// Wrap an already-canonical definition using its class-specific semantic ID.
/// Classes 1/2 require the owning semantic compiler's projection output, not
/// canonicalized raw source configuration. Their validation here proves only
/// structural CanonicalConfigValueV1 form and size; it is not compilation.
/// Other classes use their existing typed decoders. Dependency availability is
/// deliberately not required for structural retention of an exact definition.
pub fn encode_semantic_definition_object(
  class: u16,
  definition: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<EncodedSemanticDefinitionObjectV1> {
  let hash_width = hash_algorithm.hash_length();
  let body_prefix = checked_add(16, hash_width, "semantic definition prefix")?;
  let body_length = checked_add(body_prefix, definition.len(), "semantic definition body")?;
  let total_length = checked_add(36, body_length, "semantic definition envelope")?;
  if total_length > SEMANTIC_HARD_CAP {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "semantic_definition_exceeds_cap",
      "complete semantic definition object exceeds 1 MiB",
    ));
  }
  let semantic_id = validated_semantic_identity(class, definition, hash_algorithm)?;
  require_nonzero_hash(&semantic_id, hash_width, "semantic definition identity", "semantic_definition_zero_id")?;
  let total_field = checked_u32(total_length, "semantic definition complete length")?;
  let body_field = checked_u32(body_length, "semantic definition body length")?;
  let mut value = Vec::new();
  value.try_reserve_exact(total_length).map_err(|source| {
    error(
      MalformedInputClass::AllocationAmplification,
      "semantic_definition_writer_allocation",
      format!("cannot reserve {total_length} bytes: {source}"),
    )
  })?;
  value.resize(total_length, 0);
  value[..4].copy_from_slice(b"ASEM");
  put_u16(&mut value, 4, 1);
  put_u16(&mut value, 6, 4);
  put_u16(&mut value, 8, SEMANTIC_HEADER_LENGTH as u16);
  put_u32(&mut value, 12, total_field);
  put_u32(&mut value, 16, body_field);
  put_u64(&mut value, 20, 1);
  put_u16(&mut value, 32, class);
  put_u16(&mut value, 34, 1);
  value[40..40 + hash_width].copy_from_slice(&semantic_id);
  put_u32(&mut value, 40 + hash_width, definition.len() as u32);
  value[48 + hash_width..total_length - 4].copy_from_slice(definition);
  write_trailing_crc(&mut value);
  let object_id = immutable_id(hash_algorithm, b"aeordb.semantic-object.immutable.v1\0", 4, &value);
  Ok(EncodedSemanticDefinitionObjectV1 { semantic_id, object: EncodedSemanticObjectV1 { object_id, value } })
}

pub(crate) fn validated_semantic_identity(class: u16, definition: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  let domain: &[u8] = match class {
    1 | 2 => {
      validate_canonical_value(definition, CanonicalValueBounds::CONFIG)?;
      if class == 1 {
        b"aeordb.semantic.effective-index-config-projection.v1\0"
      } else {
        b"aeordb.semantic.parser-registry-projection.v1\0"
      }
    }
    3 => return Ok(decode_scope_definition(definition, hash_algorithm)?.scope_id),
    4 => return Ok(decode_value_store_definition(definition, hash_algorithm)?.value_store_id),
    5 => return Ok(decode_field_index_definition(definition, hash_algorithm)?.index_id),
    6 | 7 => {
      let dependency = decode_dependency_record_bytes(definition)?;
      let expected_kind = if class == 6 { 1 } else { 2 };
      if dependency.kind != expected_kind {
        return Err(error(
          MalformedInputClass::CrossRecordClosureMismatch,
          "semantic_dependency_definition_kind",
          "dependency kind disagrees with its semantic definition class",
        ));
      }
      if class == 6 {
        b"aeordb.semantic.executable-dependency-definition.v1\0"
      } else {
        b"aeordb.semantic.native-dependency-definition.v1\0"
      }
    }
    _ => {
      return Err(error(
        MalformedInputClass::UnknownTypeKindOrEnum,
        "semantic_definition_class",
        "semantic definition class is not registered",
      ));
    }
  };
  Ok(digest_parts(hash_algorithm, &[domain, definition]))
}
