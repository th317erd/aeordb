//! Linear, bounded namespace branch of the captured task reader.
use super::*;
use crate::engine::btree::BTreeNode;
use crate::engine::directory_entry::ChildEntry;
use crate::engine::v4::hash::IncrementalDigestV1;
use crate::engine::v4::namespace_seek::child_bounds;
use crate::engine::v4::read_view_native::{
  MAX_DIRECTORY_ENTITY_BYTES, decode_validated_selected_directory_node, join_selected_path, validate_selected_directory_entity,
  validate_selected_file_record_metadata,
};

struct DirectoryVisit<'a> {
  hash: &'a [u8],
  path: &'a str,
  lower: Option<&'a str>,
  upper: Option<&'a str>,
  btree_child: bool,
}

struct NamespaceCharge<'a> {
  retained: &'a Cell<u64>,
  bytes: u64,
  _memory: MemoryReservation,
}

impl Drop for NamespaceCharge<'_> {
  fn drop(&mut self) {
    self.retained.set(self.retained.get() - self.bytes);
  }
}

impl GraphOperation<'_, '_> {
  pub(super) fn walk_namespace(&self, root: &[u8], summary: &mut CheckpointGraphStatistics) -> Result<()> {
    let mut ancestry = Vec::new();
    ancestry.try_reserve_exact(self.bounds.maximum_depth).map_err(graph_allocation)?;
    self.directory(DirectoryVisit { hash: root, path: "/", lower: None, upper: None, btree_child: false }, &mut ancestry, summary)
  }

  fn directory(&self, visit: DirectoryVisit<'_>, ancestry: &mut Vec<Vec<u8>>, summary: &mut CheckpointGraphStatistics) -> Result<()> {
    self.check()?;
    if ancestry.len() >= self.bounds.maximum_depth {
      return Err(invalid("semantic_task_graph_namespace_depth", "namespace graph exceeds its admitted depth").into());
    }
    if ancestry.iter().any(|hash| hash == visit.hash) {
      return Err(invalid("semantic_task_graph_namespace_cycle", "namespace graph repeats an ancestor").into());
    }
    let locator =
      self.lookup.get(visit.hash)?.ok_or_else(|| invalid("semantic_task_graph_entity_missing", "namespace directory is absent"))?;
    // Same conservative decode/copy charge as the existing native namespace
    // source reader; retain it with the decoded ancestor, not only its I/O.
    let charge = 4 * u64::from(locator.total_length) + (64 << 10);
    let retained =
      self.namespace_bytes.get().checked_add(charge).filter(|bytes| *bytes <= self.bounds.maximum_namespace_workspace_bytes).ok_or(
        SemanticMutationObservationErrorV1::Resource {
          code: "semantic_task_graph_namespace_memory",
          message: "namespace ancestors exceed admitted workspace",
        },
      )?;
    let memory = self.capture.memory.reserve(MemoryOwner::Task, charge, AdmissionClass::Maintenance)?;
    self.namespace_bytes.set(retained);
    let _charge = NamespaceCharge { retained: &self.namespace_bytes, bytes: charge, _memory: memory };
    ancestry.push(copy_bytes(visit.hash)?);
    let bytes = self.raw(visit.hash, kv_tag::DIRECTORY, MAX_DIRECTORY_ENTITY_BYTES)?;
    let decoded = decode_whole_entity(&bytes, self.algorithm(), self.header().write_sequence_high_water)?;
    let entity = LoadedImmutableEntityV1 {
      entity_version: decoded.entity_version,
      entry_type: decoded.entry_type,
      flags: decoded.flags,
      compression_algorithm: decoded.compression_algorithm,
      timestamp_ms: decoded.timestamp_ms,
      write_sequence: decoded.write_sequence,
      key: copy_bytes(decoded.key)?,
      stored_value: copy_bytes(decoded.stored_value)?,
    };
    validate_selected_directory_entity(&entity, self.algorithm(), visit.hash).map_err(NativeSelectedNamespaceReadErrorV1::from)?;
    let node = decode_validated_selected_directory_node(
      &entity,
      self.algorithm().hash_length(),
      visit.path,
      visit.lower,
      visit.upper,
      visit.btree_child,
    )?;
    drop(entity);
    drop(bytes);
    if !visit.btree_child {
      increment(&mut summary.namespace_directories)?;
    }
    match node {
      BTreeNode::Leaf(leaf) => {
        self.lookup.step(leaf.entries.len() as u64)?;
        for child in &leaf.entries {
          self.check()?;
          let path = join_selected_path(visit.path, &child.name, self.bounds.maximum_path_bytes)?;
          match child.entry_type {
            value if value == EntryTypeV4::DirectoryIndex.to_u8() => {
              self.directory(
                DirectoryVisit { hash: &child.hash, path: &path, lower: None, upper: None, btree_child: false },
                ancestry,
                summary,
              )?;
            }
            value if value == EntryTypeV4::FileRecord.to_u8() => self.file_record(child, &path, summary)?,
            value if value == EntryTypeV4::Symlink.to_u8() => self.symlink(child, &path, summary)?,
            _ => return Err(invalid("semantic_task_graph_child_role", "namespace contains an unsupported child role").into()),
          }
        }
      }
      BTreeNode::Internal(internal) => {
        self.lookup.step(internal.children.len() as u64)?;
        for (index, hash) in internal.children.iter().enumerate() {
          let (lower, upper) = child_bounds(&internal, index, visit.lower.map(str::to_string), visit.upper.map(str::to_string))
            .map_err(NativeSelectedNamespaceReadErrorV1::from)?;
          self.directory(
            DirectoryVisit { hash, path: visit.path, lower: lower.as_deref(), upper: upper.as_deref(), btree_child: true },
            ancestry,
            summary,
          )?;
        }
      }
    }
    ancestry.pop();
    self.check()
  }

  fn file_record(&self, entry: &ChildEntry, path: &str, summary: &mut CheckpointGraphStatistics) -> Result<()> {
    let bytes = self.raw(&entry.hash, kv_tag::FILE_RECORD, 4 << 20)?;
    let entity = decode_whole_entity(&bytes, self.algorithm(), self.header().write_sequence_high_water)?;
    if entity.entry_type != EntryTypeV4::FileRecord
      || !matches!(entity.entity_version, 0 | 1)
      || entity.flags != 0
      || entity.compression_algorithm != CompressionAlgorithm::None
      || digest_parts(self.algorithm(), &[b"filec:", entity.stored_value]) != entry.hash
    {
      return Err(invalid("semantic_task_graph_file_identity", "ordinary FileRecord representation or content identity is invalid").into());
    }
    let record = FileRecord::deserialize(entity.stored_value, self.algorithm().hash_length(), entity.entity_version)?;
    validate_selected_file_record_metadata(&record, entry, path).map_err(NativeSelectedNamespaceReadErrorV1::from)?;
    if record.total_size > 0 && record.chunk_hashes.is_empty() {
      return Err(invalid("semantic_task_graph_file_length", "nonempty ordinary file has no chunk references").into());
    }
    if !self.inspect_ordinary_payloads {
      for key in &record.chunk_hashes {
        self.ordinary_chunk_reference(key, summary)?;
      }
      self.check()?;
      return increment(&mut summary.namespace_files);
    }
    let buffer_length = record.total_size.min(self.bounds.maximum_decoded_chunk_bytes as u64) as usize;
    let buffer_memory = self.capture.memory.reserve(MemoryOwner::Task, buffer_length as u64, AdmissionClass::Maintenance)?;
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(buffer_length).map_err(graph_allocation)?;
    buffer.resize(buffer_length, 0);
    let mut content = IncrementalDigestV1::new(self.algorithm());
    let mut representation = blake3::Hasher::new();
    let mut remaining = record.total_size;
    let chunk_bounds = NativeSemanticSourceReadBoundsV1 {
      maximum_body_bytes: buffer_length,
      maximum_chunk_entity_bytes: self.capture.bounds.maximum_entity_bytes,
      maximum_chunks: self.bounds.maximum_work,
      maximum_read_bytes: self.bounds.maximum_read_bytes,
    };
    for key in &record.chunk_hashes {
      self.check()?;
      buffer_memory.check_admission()?;
      let capacity = remaining.min(buffer_length as u64) as usize;
      let written = self.capture.read_source_chunk(
        &self.lookup,
        key,
        &mut buffer[..capacity],
        chunk_bounds,
        &mut representation,
        protected_sources::SemanticSourceKindV1::Namespace,
      )?;
      content.update(&buffer[..written]);
      remaining = remaining
        .checked_sub(written as u64)
        .ok_or_else(|| invalid("semantic_task_graph_file_length", "chunk exceeds declared file size"))?;
      increment(&mut summary.namespace_chunks)?;
    }
    if remaining != 0 {
      return Err(invalid("semantic_task_graph_file_length", "ordinary chunks do not fill the declared file size").into());
    }
    if entity.entity_version == 1 && content.try_finalize().map_err(graph_allocation)? != record.content_hash {
      return Err(invalid("semantic_task_graph_file_content", "ordinary file content digest differs from its record").into());
    }
    self.check()?;
    increment(&mut summary.namespace_files)
  }

  fn ordinary_chunk_reference(&self, key: &[u8], summary: &mut CheckpointGraphStatistics) -> Result<()> {
    self.lookup.step(1)?;
    if key.len() != self.algorithm().hash_length() || key.iter().all(|byte| *byte == 0) {
      return Err(invalid("semantic_task_graph_chunk_reference", "ordinary chunk reference must have a nonzero selected-width key").into());
    }
    let locator = self.lookup.get(key)?.ok_or_else(|| invalid("semantic_source_chunk_missing", "ordinary chunk locator is absent"))?;
    if locator.type_flags != kv_tag::CHUNK {
      return Err(invalid("semantic_task_graph_entity_role", "ordinary chunk reference resolves to another KV role").into());
    }
    let minimum = crate::engine::v4::entity::checked_whole_entity_encoded_length(self.algorithm(), key.len(), 0)?;
    if locator.hash != key || u64::from(locator.total_length) < minimum as u64 {
      return Err(invalid("semantic_task_graph_chunk_reference", "ordinary chunk locator identity or minimum geometry is invalid").into());
    }
    self.lookup.captured.validate_reference_extent(&locator)?;
    // This is an opaque leaf: neither read admission nor a payload decoder is
    // called. The captured KV identity/extent is retained, not certified healthy.
    // Returning the callback error directly preserves it over simultaneous
    // cancellation or host pressure, as the full-read bridge does.
    self.lookup.visitor.borrow_mut()(&locator)?;
    self.check()?;
    increment(&mut summary.namespace_chunks)?;
    increment(&mut summary.opaque_chunk_references)?;
    summary.opaque_chunk_bytes = summary
      .opaque_chunk_bytes
      .checked_add(u64::from(locator.total_length))
      .ok_or_else(|| invalid("semantic_task_graph_counts", "ordinary chunk reference byte count overflowed"))?;
    Ok(())
  }

  fn symlink(&self, entry: &ChildEntry, path: &str, summary: &mut CheckpointGraphStatistics) -> Result<()> {
    let bytes = self.raw(&entry.hash, kv_tag::SYMLINK, (2 << 16) + 4096)?;
    let entity = decode_whole_entity(&bytes, self.algorithm(), self.header().write_sequence_high_water)?;
    if entity.entry_type != EntryTypeV4::Symlink
      || entity.entity_version != 0
      || entity.flags != 0
      || entity.compression_algorithm != CompressionAlgorithm::None
      || digest_parts(self.algorithm(), &[b"symlinkc:", entity.stored_value]) != entry.hash
    {
      return Err(invalid("semantic_task_graph_symlink_identity", "symlink representation or content identity is invalid").into());
    }
    let record = crate::engine::symlink_record::SymlinkRecord::deserialize(entity.stored_value, entity.entity_version)?;
    if record.path != path
      || record.created_at != entry.created_at
      || record.updated_at != entry.updated_at
      || record.serialize()? != entity.stored_value
    {
      return Err(invalid("semantic_task_graph_symlink_metadata", "symlink metadata or canonical length differs").into());
    }
    // The target string is not a physical reference. Do not follow it.
    self.check()?;
    increment(&mut summary.namespace_symlinks)
  }
}

fn increment(value: &mut u64) -> Result<()> {
  *value = value.checked_add(1).ok_or_else(|| invalid("semantic_task_graph_counts", "namespace count overflowed"))?;
  Ok(())
}
