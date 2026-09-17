//! Protected raw inputs read from the same capture as semantic task inventory.
//! Neither a source observation nor its bytes grant durable retention or resume.
use super::*;
#[path = "semantic_source_staging.rs"]
mod staging;
pub use staging::NativeSemanticSourcePublicationErrorV1;
use crate::engine::v4::hash::{IncrementalDigestV1, try_digest_parts};
use crate::engine::v4::scope::validate_canonical_absolute_path;
use crate::engine::v4::system_family::SystemFamilyPolicyDecisionV1;

const MAXIMUM_FILE_RECORD_BYTES: usize = 4 << 20;
const MAXIMUM_SOURCE_BODY_BYTES: usize = 64 << 20;
const MAXIMUM_CHUNK_ENTITY_BYTES: usize = MAXIMUM_ENTITY_BYTES;
const SOURCE_SCRATCH_BYTES: u64 = 64 << 10;
const DECOMPRESSION_SCRATCH_BYTES: u64 = 1 << 20;

#[derive(Clone, Copy, Debug)]
pub struct NativeSemanticSourceReadBoundsV1 {
  pub maximum_body_bytes: usize,
  pub maximum_chunk_entity_bytes: usize,
  pub maximum_chunks: u64,
  pub maximum_read_bytes: u64,
}

pub struct NativeProtectedSemanticSourceV1<'a> {
  _capture: &'a NativeSemanticMutationInventoryV1<'a>,
  record: FileRecord,
  encoded_record: Vec<u8>,
  body: Vec<u8>,
  revision: Vec<u8>,
  entity_version: u8,
  flags: u8,
  chunk_representation_fingerprint: [u8; 32],
  _memory: MemoryReservation,
}

impl NativeProtectedSemanticSourceV1<'_> {
  pub fn record(&self) -> &FileRecord {
    &self.record
  }

  pub fn encoded_record(&self) -> &[u8] {
    &self.encoded_record
  }

  pub fn body(&self) -> &[u8] {
    &self.body
  }

  pub fn revision(&self) -> &[u8] {
    &self.revision
  }

  pub fn entity_version(&self) -> u8 {
    self.entity_version
  }

  pub fn flags(&self) -> u8 {
    self.flags
  }
}

impl NativeSemanticMutationInventoryV1<'_> {
  /// Read one exact retained FileRecord revision, never its current path alias.
  pub fn read_retained_protected_source(
    &self,
    path: &str,
    revision: &[u8],
    bounds: NativeSemanticSourceReadBoundsV1,
  ) -> Result<NativeProtectedSemanticSourceV1<'_>, SemanticMutationObservationErrorV1> {
    self
      .read_source_from_lookup(path, Some(revision), bounds, &self.source_lookup(bounds), || {})?
      .ok_or_else(|| invalid("semantic_source_retained_missing", "retained protected source is absent from the captured snapshot"))
  }

  /// Read a protected non-HEAD input from this inventory's settled snapshot.
  /// An absent current path is explicit absence; an absent dependency is an
  /// error. The returned source keeps both capture protection and accounting.
  pub fn read_protected_source(
    &self,
    path: &str,
    bounds: NativeSemanticSourceReadBoundsV1,
  ) -> Result<Option<NativeProtectedSemanticSourceV1<'_>>, SemanticMutationObservationErrorV1> {
    self.read_protected_source_with_observer(path, bounds, || {})
  }

  pub(crate) fn read_protected_source_with_observer(
    &self,
    path: &str,
    bounds: NativeSemanticSourceReadBoundsV1,
    after_chunk: impl FnMut(),
  ) -> Result<Option<NativeProtectedSemanticSourceV1<'_>>, SemanticMutationObservationErrorV1> {
    self.read_source_from_lookup(path, None, bounds, &self.source_lookup(bounds), after_chunk)
  }

  fn source_lookup(&self, bounds: NativeSemanticSourceReadBoundsV1) -> CapturedEntityLookupV1<'_> {
    CapturedEntityLookupV1 {
      snapshot: &self.snapshot,
      header: &self.header.selected.header,
      bounds: NativeSemanticMutationInventoryBoundsV1 {
        maximum_work: bounds.maximum_chunks,
        maximum_entity_bytes: MAXIMUM_FILE_RECORD_BYTES.max(bounds.maximum_chunk_entity_bytes),
        maximum_read_bytes: bounds.maximum_read_bytes,
      },
      cancellation: &self.cancellation,
      remaining_read_bytes: Cell::new(bounds.maximum_read_bytes),
    }
  }

  // Catalog traversal supplies its single cumulative lookup; it must not
  // instantiate a fresh read budget for every referenced source.
  pub(super) fn read_source_from_lookup(
    &self,
    path: &str,
    retained_revision: Option<&[u8]>,
    bounds: NativeSemanticSourceReadBoundsV1,
    lookup: &impl FirstAuthorityEntityLookupV1,
    mut after_chunk: impl FnMut(),
  ) -> Result<Option<NativeProtectedSemanticSourceV1<'_>>, SemanticMutationObservationErrorV1> {
    check_cancelled(&self.cancellation)?;
    self._memory.check_admission()?;
    if bounds.maximum_body_bytes > MAXIMUM_SOURCE_BODY_BYTES
      || bounds.maximum_chunk_entity_bytes == 0
      || bounds.maximum_chunk_entity_bytes > MAXIMUM_CHUNK_ENTITY_BYTES
      || bounds.maximum_chunks == 0
      || bounds.maximum_read_bytes == 0
    {
      return Err(invalid("semantic_source_bounds", "protected source read requires valid bounded work and byte limits"));
    }
    let header = &self.header.selected.header;
    let algorithm = header.hash_algorithm;
    validate_source_path(path, algorithm)?;
    if retained_revision.is_some_and(|revision| revision.len() != algorithm.hash_length() || revision.iter().all(|byte| *byte == 0)) {
      return Err(invalid("semantic_source_retained_identity", "retained source requires a nonzero selected-width revision"));
    }
    let mut memory = self.memory.reserve(MemoryOwner::Task, SOURCE_SCRATCH_BYTES, AdmissionClass::Maintenance)?;
    let current_key;
    let key = match retained_revision {
      Some(revision) => revision,
      None => {
        current_key = source_digest(algorithm, &[b"file:", path.as_bytes()])?;
        &current_key
      }
    };
    let Some(locator) = lookup.get(key).map_err(FirstAuthorityPublicationErrorV1::from)? else {
      check_cancelled(&self.cancellation)?;
      memory.check_admission()?;
      if retained_revision.is_some() {
        return Err(invalid("semantic_source_retained_missing", "retained protected source is absent from the captured snapshot"));
      }
      return Ok(None);
    };
    if locator.type_flags != KV_TYPE_FILE_RECORD {
      return Err(invalid("semantic_source_record_role", "protected source key resolves to another KV role"));
    }
    let record_length = locator.total_length as usize;
    if record_length > MAXIMUM_FILE_RECORD_BYTES {
      return Err(resource("semantic_source_record_bound", "protected source FileRecord exceeds the operational entity bound"));
    }
    // Original entity, existing owned decoder, exact body copy and its bounded
    // per-chunk vector overhead. No allocation scales with an unchecked count.
    memory.grow((record_length as u64) * 4)?;
    let bytes =
      read_entity_bounded(&self._protection.publisher().file, lookup, key, MAXIMUM_FILE_RECORD_BYTES, header.write_sequence_high_water)
        .map_err(map_source_read_error)?
        .ok_or_else(|| invalid("semantic_source_record_missing", "captured protected source disappeared from the same snapshot"))?;
    let entity = decode_whole_entity(&bytes, algorithm, header.write_sequence_high_water)?;
    if entity.entry_type != EntryTypeV4::FileRecord
      || !matches!(entity.entity_version, 0 | 1)
      || entity.compression_algorithm != CompressionAlgorithm::None
    {
      return Err(invalid("semantic_source_record_representation", "protected source FileRecord representation is invalid"));
    }
    let checked_revision = if let Some(expected) = retained_revision {
      let actual = source_digest(algorithm, &[b"filec:", entity.stored_value])?;
      if actual != expected {
        return Err(invalid("semantic_source_retained_identity", "retained FileRecord bytes disagree with their content revision"));
      }
      Some(actual)
    } else {
      None
    };
    let record = FileRecord::deserialize(entity.stored_value, algorithm.hash_length(), entity.entity_version)
      .map_err(FirstAuthorityPublicationErrorV1::from)?;
    if record.path != path {
      return Err(invalid("semantic_source_record_path", "captured protected source names another path"));
    }
    if record.total_size > bounds.maximum_body_bytes as u64 || record.chunk_hashes.len() as u64 > bounds.maximum_chunks {
      return Err(resource("semantic_source_body_bound", "protected source exceeds admitted body or chunk work limits"));
    }
    let output_length = record.total_size as usize;
    memory.grow(record.total_size)?;
    let mut body = Vec::new();
    body
      .try_reserve_exact(output_length)
      .map_err(|source| SemanticMutationObservationErrorV1::Allocation { code: "semantic_source_body_allocation", source })?;
    // Decoders write only within this admitted output, including compressed
    // chunks. Infallible growth cannot follow an understated FileRecord size.
    body.resize(output_length, 0);
    let mut written = 0usize;
    let mut content = IncrementalDigestV1::new(algorithm);
    let mut representations = staging::chunk_representation_fingerprint(record.chunk_hashes.len());
    for chunk_key in &record.chunk_hashes {
      check_cancelled(&self.cancellation)?;
      memory.check_admission()?;
      let count = self.read_source_chunk(lookup, chunk_key, &mut body[written..], bounds, &mut representations)?;
      content.update(&body[written..written + count]);
      written += count;
      after_chunk();
      check_cancelled(&self.cancellation)?;
      memory.check_admission()?;
    }
    if written != output_length {
      return Err(invalid("semantic_source_content_length", "protected source chunks disagree with its declared length"));
    }
    if entity.entity_version == 1
      && content
        .try_finalize()
        .map_err(|source| SemanticMutationObservationErrorV1::Allocation { code: "semantic_source_digest_allocation", source })?
        != record.content_hash
    {
      return Err(invalid("semantic_source_content_identity", "protected source whole-content identity is invalid"));
    }
    let revision = match checked_revision {
      Some(revision) => revision,
      None => source_digest(algorithm, &[b"filec:", entity.stored_value])?,
    };
    let mut encoded_record = Vec::new();
    encoded_record
      .try_reserve_exact(entity.stored_value.len())
      .map_err(|source| SemanticMutationObservationErrorV1::Allocation { code: "semantic_source_record_allocation", source })?;
    encoded_record.extend_from_slice(entity.stored_value);
    check_cancelled(&self.cancellation)?;
    memory.check_admission()?;
    Ok(Some(NativeProtectedSemanticSourceV1 {
      _capture: self,
      record,
      encoded_record,
      body,
      revision,
      entity_version: entity.entity_version,
      flags: entity.flags,
      chunk_representation_fingerprint: *representations.finalize().as_bytes(),
      _memory: memory,
    }))
  }

  fn read_source_chunk(
    &self,
    lookup: &impl FirstAuthorityEntityLookupV1,
    key: &[u8],
    output: &mut [u8],
    bounds: NativeSemanticSourceReadBoundsV1,
    representations: &mut blake3::Hasher,
  ) -> Result<usize, SemanticMutationObservationErrorV1> {
    let locator = lookup
      .get(key)
      .map_err(FirstAuthorityPublicationErrorV1::from)?
      .ok_or_else(|| invalid("semantic_source_chunk_missing", "protected source chunk is absent from the captured snapshot"))?;
    if locator.type_flags != KV_TYPE_CHUNK {
      return Err(invalid("semantic_source_chunk_role", "protected source chunk key resolves to another KV role"));
    }
    if locator.total_length as usize > bounds.maximum_chunk_entity_bytes {
      return Err(resource("semantic_source_chunk_bound", "protected source chunk exceeds the operational entity bound"));
    }
    let memory =
      self.memory.reserve(MemoryOwner::Task, u64::from(locator.total_length) + DECOMPRESSION_SCRATCH_BYTES, AdmissionClass::Maintenance)?;
    let header = &self.header.selected.header;
    let bytes = read_entity_bounded(
      &self._protection.publisher().file,
      lookup,
      key,
      bounds.maximum_chunk_entity_bytes,
      header.write_sequence_high_water,
    )
    .map_err(map_source_read_error)?
    .ok_or_else(|| invalid("semantic_source_chunk_missing", "protected source chunk disappeared from the same snapshot"))?;
    let entity = decode_whole_entity(&bytes, header.hash_algorithm, header.write_sequence_high_water)?;
    if entity.entry_type != EntryTypeV4::Chunk || entity.entity_version != 0 {
      return Err(invalid("semantic_source_chunk_representation", "protected source chunk representation is invalid"));
    }
    let written = match entity.compression_algorithm {
      CompressionAlgorithm::None => {
        if entity.stored_value.len() > output.len() {
          return Err(invalid("semantic_source_content_length", "protected source chunk exceeds the remaining declared body"));
        }
        output[..entity.stored_value.len()].copy_from_slice(entity.stored_value);
        entity.stored_value.len()
      }
      CompressionAlgorithm::Zstd => {
        // The in-memory decoder uses caller-owned output for its history, not
        // a streaming window sized by an untrusted compressed-frame header.
        let mut decoder = zstd::zstd_safe::DCtx::try_create()
          .ok_or_else(|| resource("semantic_source_decoder_allocation", "protected source decoder allocation failed"))?;
        if decoder.sizeof() as u64 > DECOMPRESSION_SCRATCH_BYTES {
          return Err(resource("semantic_source_decoder_bound", "protected source decoder exceeds its admitted workspace"));
        }
        decoder.decompress(output, entity.stored_value).map_err(|status| SemanticMutationObservationErrorV1::Compression { status })?
      }
    };
    let content = &output[..written];
    let ordinary = source_digest(header.hash_algorithm, &[b"chunk:", content])?;
    if ordinary != key
      && (entity.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM || source_digest(header.hash_algorithm, &[b"system::", content])? != key)
    {
      return Err(invalid("semantic_source_chunk_identity", "protected source chunk content identity is invalid"));
    }
    check_cancelled(&self.cancellation)?;
    memory.check_admission()?;
    staging::fingerprint_chunk_representation(representations, &entity);
    Ok(written)
  }
}

pub(super) fn validate_source_path(path: &str, algorithm: HashAlgorithm) -> Result<(), SemanticMutationObservationErrorV1> {
  validate_canonical_absolute_path(path)?;
  match SystemFamilyPolicyResolverV1::embedded(algorithm)?.policy(SystemFamilySubjectV1::Path(path), "captured semantic source")? {
    SystemFamilyPolicyDecisionV1::Known { family_id: 0x0001 | 0x0003 | 0x0031 | 0x0032, .. } => Ok(()),
    _ => Err(invalid("semantic_source_family", "path is not a protected non-HEAD semantic input")),
  }
}

fn resource(code: &'static str, message: &'static str) -> SemanticMutationObservationErrorV1 {
  SemanticMutationObservationErrorV1::Resource { code, message }
}

fn source_digest(algorithm: HashAlgorithm, parts: &[&[u8]]) -> Result<Vec<u8>, SemanticMutationObservationErrorV1> {
  try_digest_parts(algorithm, parts)
    .map_err(|source| SemanticMutationObservationErrorV1::Allocation { code: "semantic_source_digest_allocation", source })
}

fn map_source_read_error(error: FirstAuthorityPublicationErrorV1) -> SemanticMutationObservationErrorV1 {
  let code = match error.code() {
    "semantic_task_inventory_read_bound" => "semantic_source_read_bound",
    "semantic_task_inventory_entity_bound" | "first_authority_locator_exceeds_cap" => "semantic_source_entity_bound",
    "first_authority_readback_allocation" => "semantic_source_entity_allocation",
    _ => return error.into(),
  };
  SemanticMutationObservationErrorV1::ResourceRead { code, source: error }
}
