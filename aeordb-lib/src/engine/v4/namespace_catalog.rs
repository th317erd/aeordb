//! Bounded catalog wire encoders. Publication, definition closure and canonical
//! cross-node Patricia shape belong to the catalog COW/authority owners.

use super::{
  EncodedSemanticObjectV1, FormatResult, HashAlgorithm, MalformedInputClass, SEMANTIC_HARD_CAP, SEMANTIC_HEADER_LENGTH,
  SemanticCatalogChildV1, SemanticCatalogRecordV1, checked_add, checked_mul, checked_u32, digest_parts, error, immutable_id, length_error,
  put_u16, put_u32, put_u64, require_nonzero_hash, validate_catalog_owner_key, write_trailing_crc,
};

/// Encode a nonempty, strictly ordered collision bucket. Every binding must
/// have the same full lookup digest; inputs are never sorted or truncated.
/// Definition target existence and class-specific payload identity are checked
/// by the catalog closure owner, not inferred from caller-supplied hashes.
pub fn encode_semantic_catalog_leaf(
  records: &[SemanticCatalogRecordV1<'_>],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<EncodedSemanticObjectV1> {
  if records.is_empty() || records.len() > 4_096 {
    return Err(error(MalformedInputClass::AllocationAmplification, "catalog_leaf_count", "leaf requires 1..4096 records"));
  }
  let hash_width = hash_algorithm.hash_length();
  let record_prefix = checked_add(8, checked_mul(2, hash_width, "catalog writer record hashes")?, "catalog writer record prefix")?;
  let body_prefix = checked_add(16, hash_width, "catalog writer leaf prefix")?;
  let mut body_length = body_prefix;
  // Check the complete object bound before interpreting any variable fields,
  // hashing a key, or allocating an output buffer.
  for record in records {
    let record_length = checked_add(record_prefix, record.owner_key.len(), "catalog writer record length")?;
    body_length = checked_add(body_length, record_length, "catalog writer leaf body")?;
    if checked_add(body_length, 36, "catalog writer leaf envelope")? > SEMANTIC_HARD_CAP {
      return Err(error(MalformedInputClass::AllocationAmplification, "catalog_leaf_exceeds_cap", "complete catalog leaf exceeds 1 MiB"));
    }
  }

  let mut lookup_digest = Vec::new();
  let mut previous: Option<(u16, &[u8])> = None;
  for record in records {
    require_nonzero_hash(record.semantic_id, hash_width, "catalog semantic identity", "catalog_leaf_zero_hash")?;
    require_nonzero_hash(record.definition_object_id, hash_width, "catalog definition edge", "catalog_leaf_zero_hash")?;
    validate_catalog_owner_key(record.record_kind, record.owner_key, hash_width)?;
    if matches!(record.record_kind, 6 | 7) && record.owner_key != record.semantic_id {
      return Err(error(
        MalformedInputClass::IdentityKeyOrGenerationMismatch,
        "catalog_leaf_dependency_identity",
        "dependency owner key must equal its complete semantic definition ID",
      ));
    }
    if previous.is_some_and(|prior| (prior.0, prior.1) >= (record.record_kind, record.owner_key)) {
      return Err(error(MalformedInputClass::NoncanonicalOrderOrDuplicate, "catalog_leaf_order", "records are not strictly ordered"));
    }
    let lookup = digest_parts(hash_algorithm, &[b"aeordb.semantic-catalog-key.v1\0", &record.record_kind.to_le_bytes(), record.owner_key]);
    if previous.is_none() {
      lookup_digest = lookup;
    } else if lookup != lookup_digest {
      return Err(error(
        MalformedInputClass::IdentityKeyOrGenerationMismatch,
        "catalog_leaf_lookup_digest",
        "record does not belong to the leaf lookup digest",
      ));
    }
    previous = Some((record.record_kind, record.owner_key));
  }

  let mut value = allocate_catalog_object(2, records.len() as u64, body_length)?;
  let body = &mut value[SEMANTIC_HEADER_LENGTH..SEMANTIC_HEADER_LENGTH + body_length];
  put_u32(body, 4, records.len() as u32);
  body[8..8 + hash_width].copy_from_slice(&lookup_digest);
  put_u32(body, 8 + hash_width, (body_length - body_prefix) as u32);
  let mut cursor = body_prefix;
  for record in records {
    put_u16(body, cursor, record.record_kind);
    put_u32(body, cursor + 4, record.owner_key.len() as u32);
    body[cursor + 8..cursor + 8 + hash_width].copy_from_slice(record.semantic_id);
    body[cursor + 8 + hash_width..cursor + record_prefix].copy_from_slice(record.definition_object_id);
    let end = cursor + record_prefix + record.owner_key.len();
    body[cursor + record_prefix..end].copy_from_slice(record.owner_key);
    cursor = end;
  }
  Ok(finish_catalog_object(2, value, hash_algorithm))
}

/// Encode one structural internal node, preserving the supplied prefix and
/// child order. The COW owner must additionally prove maximal compression and
/// child-depth/prefix/count agreement against the captured child objects.
pub fn encode_semantic_catalog_internal(
  depth: u16,
  prefix: &[u8],
  children: &[SemanticCatalogChildV1<'_>],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<EncodedSemanticObjectV1> {
  let hash_width = hash_algorithm.hash_length();
  if !(2..=256).contains(&children.len()) || usize::from(depth).checked_add(prefix.len()).is_none_or(|end| end >= hash_width) {
    return Err(error(
      MalformedInputClass::InvalidGraphEdgeOrCycle,
      "catalog_internal_metadata",
      "internal node requires 2..256 children and a prefix ending before the last digest byte",
    ));
  }
  let child_length = checked_add(12, hash_width, "catalog writer child length")?;
  let children_length = checked_mul(children.len(), child_length, "catalog writer children")?;
  let body_prefix = checked_add(20, prefix.len(), "catalog writer internal prefix")?;
  let body_length = checked_add(body_prefix, children_length, "catalog writer internal body")?;
  if checked_add(body_length, 36, "catalog writer internal envelope")? > 65_536 {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "catalog_internal_exceeds_cap",
      "complete catalog internal exceeds 64 KiB",
    ));
  }
  let mut subtree_record_count = 0u64;
  let mut previous_edge = None;
  for child in children {
    if previous_edge.is_some_and(|edge| edge >= child.edge) {
      return Err(error(
        MalformedInputClass::NoncanonicalOrderOrDuplicate,
        "catalog_internal_child",
        "child edges are not strictly ordered",
      ));
    }
    require_nonzero_hash(child.object_id, hash_width, "catalog child edge", "catalog_internal_zero_child")?;
    if child.record_count == 0 {
      return Err(error(MalformedInputClass::InvalidGraphEdgeOrCycle, "catalog_internal_zero_count", "catalog child has no records"));
    }
    subtree_record_count =
      subtree_record_count.checked_add(child.record_count).ok_or_else(|| length_error("catalog internal subtree count overflow"))?;
    previous_edge = Some(child.edge);
  }

  let mut value = allocate_catalog_object(3, children.len() as u64, body_length)?;
  let body = &mut value[SEMANTIC_HEADER_LENGTH..SEMANTIC_HEADER_LENGTH + body_length];
  put_u16(body, 4, depth);
  put_u16(body, 6, prefix.len() as u16);
  put_u16(body, 8, children.len() as u16);
  put_u64(body, 12, subtree_record_count);
  body[20..body_prefix].copy_from_slice(prefix);
  for (index, child) in children.iter().enumerate() {
    let offset = body_prefix + index * child_length;
    body[offset] = child.edge;
    put_u64(body, offset + 4, child.record_count);
    body[offset + 12..offset + child_length].copy_from_slice(child.object_id);
  }
  Ok(finish_catalog_object(3, value, hash_algorithm))
}

fn allocate_catalog_object(kind: u16, item_count: u64, body_length: usize) -> FormatResult<Vec<u8>> {
  let total_length = checked_add(body_length, 36, "catalog writer complete length")?;
  let total_field = checked_u32(total_length, "catalog complete length")?;
  let body_field = checked_u32(body_length, "catalog body length")?;
  let mut value = Vec::new();
  value.try_reserve_exact(total_length).map_err(|source| {
    error(
      MalformedInputClass::AllocationAmplification,
      "catalog_writer_allocation",
      format!("cannot reserve {total_length} bytes: {source}"),
    )
  })?;
  value.resize(total_length, 0);
  value[..4].copy_from_slice(b"ASEM");
  put_u16(&mut value, 4, 1);
  put_u16(&mut value, 6, kind);
  put_u16(&mut value, 8, SEMANTIC_HEADER_LENGTH as u16);
  put_u32(&mut value, 12, total_field);
  put_u32(&mut value, 16, body_field);
  put_u64(&mut value, 20, item_count);
  Ok(value)
}

fn finish_catalog_object(kind: u16, mut value: Vec<u8>, hash_algorithm: HashAlgorithm) -> EncodedSemanticObjectV1 {
  write_trailing_crc(&mut value);
  let object_id = immutable_id(hash_algorithm, b"aeordb.semantic-object.immutable.v1\0", kind, &value);
  EncodedSemanticObjectV1 { object_id, value }
}
