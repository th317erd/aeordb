//! Guarded immutable source staging; never durable task retention or activation.
use super::*;
use crate::engine::v4::entity::WholeEntityV1;

#[derive(Debug, thiserror::Error)]
pub enum NativeSemanticSourcePublicationErrorV1 {
  #[error(transparent)]
  Observation(#[from] SemanticMutationObservationErrorV1),
  #[error(transparent)]
  Publication(#[from] ImmutableEntityBatchPublicationErrorV1),
}

impl NativeSemanticSourcePublicationErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Observation(source) => source.code(),
      Self::Publication(source) => source.code(),
    }
  }

  pub fn committed_receipt(&self) -> Option<&ImmutableEntityBatchPublicationReceiptV1> {
    match self {
      Self::Publication(source) => source.committed_receipt(),
      Self::Observation(_) => None,
    }
  }
}

// Ephemeral evidence only. Frame every representation field and body length;
// omit physical publication metadata so exact relocation remains admissible.
pub(super) fn chunk_representation_fingerprint(count: usize) -> blake3::Hasher {
  let mut digest = blake3::Hasher::new();
  digest.update(b"aeordb.captured-source-chunk-representations.v1\0");
  digest.update(&(count as u64).to_le_bytes());
  digest
}

pub(super) fn fingerprint_chunk_representation(digest: &mut blake3::Hasher, entity: &WholeEntityV1<'_>) {
  digest.update(&[entity.entry_type.to_u8(), entity.entity_version, entity.flags, entity.compression_algorithm.to_u8()]);
  digest.update(&(entity.key.len() as u64).to_le_bytes());
  digest.update(entity.key);
  digest.update(&(entity.stored_value.len() as u64).to_le_bytes());
  digest.update(entity.stored_value);
}

impl NativeProtectedSemanticSourceV1<'_> {
  /// Stage this exact original FileRecord under its content identity, sharing
  /// the original typed chunks. The source's live staging guard must remain
  /// held until a durable task takes ownership or the staged work is discarded.
  /// No task/control/capability/HEAD/current-path publication is performed here.
  /// Read-byte limits cover the live chunk-validation pass; the sole physical
  /// publisher separately bounds its one-record transaction and exact readback.
  pub fn stage_retained_copy(
    &self,
    bounds: NativeSemanticSourceReadBoundsV1,
    publication_timestamp_ms: u64,
  ) -> Result<ImmutableEntityBatchPublicationReceiptV1, NativeSemanticSourcePublicationErrorV1> {
    self.stage_retained_copy_observed(bounds, publication_timestamp_ms, || {}, &mut NoopFirstAuthorityDependencyObserverV1)
  }

  pub(in crate::engine::v4::first_authority) fn stage_retained_copy_observed(
    &self,
    bounds: NativeSemanticSourceReadBoundsV1,
    publication_timestamp_ms: u64,
    after_validation: impl FnOnce(),
    observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<ImmutableEntityBatchPublicationReceiptV1, NativeSemanticSourcePublicationErrorV1> {
    let capture = self._capture;
    check_cancelled(&capture.cancellation)?;
    self._memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    if bounds.maximum_body_bytes > MAXIMUM_SOURCE_BODY_BYTES
      || bounds.maximum_chunk_entity_bytes == 0
      || bounds.maximum_chunk_entity_bytes > MAXIMUM_CHUNK_ENTITY_BYTES
      || bounds.maximum_chunks == 0
      || bounds.maximum_read_bytes == 0
    {
      return Err(invalid("semantic_source_bounds", "protected source staging requires valid bounded validation limits").into());
    }
    if self.record.total_size > bounds.maximum_body_bytes as u64 || self.record.chunk_hashes.len() as u64 > bounds.maximum_chunks {
      return Err(resource("semantic_source_body_bound", "protected source exceeds admitted body or chunk work limits").into());
    }
    if publication_timestamp_ms == 0 || publication_timestamp_ms > i64::MAX as u64 {
      return Err(invalid("semantic_source_stage_time", "source staging time must fit the signed persistent timestamp range").into());
    }
    let fresh = capture._protection.capture_semantic_mutation_inventory(capture.bounds, &capture.memory, &capture.cancellation)?;
    let old_header = &capture.header.selected.header;
    let fresh_header = &fresh.header.selected.header;
    if old_header.database_id != fresh_header.database_id
      || old_header.hash_algorithm != fresh_header.hash_algorithm
      || old_header.physical_instance_id != fresh_header.physical_instance_id
      || old_header.writer_fence_epoch != fresh_header.writer_fence_epoch
    {
      return Err(invalid("semantic_source_stage_owner", "source staging cannot cross a database, physical owner or writer fence").into());
    }
    if fresh_header.slot_sequence < old_header.slot_sequence
      || fresh_header.write_sequence_high_water < old_header.write_sequence_high_water
    {
      return Err(invalid("semantic_source_stage_regression", "source staging authority regressed behind its originating capture").into());
    }
    self.validate_live_chunks(&fresh, bounds)?;
    after_validation();
    check_cancelled(&capture.cancellation)?;
    // The original reader caps the FileRecord at4MiB. Charge the bounded
    // transaction's encoding/readback and fixed receipt/header workspace;
    // raw chunk buffers have already been released outside the root guard.
    let memory = capture
      .memory
      .reserve(MemoryOwner::Task, self.encoded_record.len() as u64 * 4 + 128 * 1024, AdmissionClass::Maintenance)
      .map_err(SemanticMutationObservationErrorV1::from)?;
    let entities = [ImmutableEntityWriteV1 {
      entity_version: self.entity_version,
      entry_type: EntryTypeV4::FileRecord,
      flags: self.flags,
      key: &self.revision,
      stored_value: &self.encoded_record,
    }];
    let request =
      ImmutableEntityBatchPublicationRequestV1 { database_id: &old_header.database_id, entities: &entities, publication_timestamp_ms };
    validate_immutable_entity_batch_request(&request)?;
    let publisher = capture._protection.publisher();
    let authority = publisher.root_state.lock().map_err(|poisoned| {
      drop(poisoned);
      SemanticMutationObservationErrorV1::from(FirstAuthorityPublicationErrorV1::StateLockPoisoned)
    })?;
    check_cancelled(&capture.cancellation)?;
    memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    if authority.staging_accounting_failed || authority.active_staging_protections == 0 {
      return Err(invalid("semantic_source_stage_protection", "source staging lost healthy process-local protection").into());
    }
    let current = publisher.observe().map_err(SemanticMutationObservationErrorV1::from)?;
    if current.selected.redundancy_degraded || current.selected.header != *fresh_header {
      return Err(
        resource("semantic_source_stage_changed", "physical authority changed after chunk validation; retry with a fresh pass").into(),
      );
    }
    // Check before entering the generic transaction: its baseline KV flush
    // may physically rewrite pages/hot-tail bytes even for an exact repeat.
    // Reuse the sole owner's exact reader and keep the root guard across both
    // this check and any required write. A collision also refuses pre-flush.
    {
      let kv = publisher.lock_kv().map_err(SemanticMutationObservationErrorV1::from)?;
      validate_kv_header_alignment(&kv, &current.selected.header).map_err(SemanticMutationObservationErrorV1::from)?;
      if let Some(write_sequence) =
        load_exact_immutable_entity(&publisher.file, &kv, &current.selected.header, entities[0], KV_TYPE_FILE_RECORD)?
      {
        let mut key = Vec::new();
        key
          .try_reserve_exact(self.revision.len())
          .map_err(|source| SemanticMutationObservationErrorV1::Allocation { code: "semantic_source_stage_allocation", source })?;
        key.extend_from_slice(&self.revision);
        let mut receipts = Vec::new();
        receipts
          .try_reserve_exact(1)
          .map_err(|source| SemanticMutationObservationErrorV1::Allocation { code: "semantic_source_stage_allocation", source })?;
        receipts.push(ImmutableEntityPublicationReceiptV1 { key, write_sequence, idempotent: true });
        check_cancelled(&capture.cancellation)?;
        memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
        return Ok(ImmutableEntityBatchPublicationReceiptV1 { entities: receipts, observation: current, idempotent: true });
      }
    }
    // The private source object, not an arbitrary descriptor, authorizes this
    // narrowly reviewed representation. Generic SYSTEM refusal is unchanged.
    // Once commit begins, report its actual receipt even if cancellation wins
    // later; never reinterpret a durable publication as an uncommitted refusal.
    publisher
      .publish_immutable_entity_batch_with_validation_locked(request, observer, ImmutableEntityValidationV1::CapturedProtectedSource)
      .map_err(NativeSemanticSourcePublicationErrorV1::from)
  }

  fn validate_live_chunks(
    &self,
    fresh: &NativeSemanticMutationInventoryV1<'_>,
    bounds: NativeSemanticSourceReadBoundsV1,
  ) -> Result<(), SemanticMutationObservationErrorV1> {
    let lookup = fresh.source_lookup(bounds);
    let header = &fresh.header.selected.header;
    let mut fingerprint = chunk_representation_fingerprint(self.record.chunk_hashes.len());
    for key in &self.record.chunk_hashes {
      check_cancelled(&fresh.cancellation)?;
      self._memory.check_admission()?;
      let locator = lookup
        .get(key)
        .map_err(FirstAuthorityPublicationErrorV1::from)?
        .ok_or_else(|| invalid("semantic_source_chunk_missing", "live protected source chunk is missing"))?;
      if locator.type_flags != KV_TYPE_CHUNK {
        return Err(invalid("semantic_source_chunk_role", "live protected source chunk resolves to another KV role"));
      }
      if locator.total_length as usize > bounds.maximum_chunk_entity_bytes {
        return Err(resource("semantic_source_chunk_bound", "live protected source chunk exceeds the validation entity bound"));
      }
      let memory = fresh.memory.reserve(MemoryOwner::Task, u64::from(locator.total_length), AdmissionClass::Maintenance)?;
      let bytes = read_entity_bounded(
        &fresh._protection.publisher().file,
        &lookup,
        key,
        bounds.maximum_chunk_entity_bytes,
        header.write_sequence_high_water,
      )
      .map_err(map_source_read_error)?
      .ok_or_else(|| invalid("semantic_source_chunk_missing", "live protected source chunk disappeared from its settled snapshot"))?;
      let entity = decode_whole_entity(&bytes, header.hash_algorithm, header.write_sequence_high_water)?;
      if entity.entry_type != EntryTypeV4::Chunk || entity.entity_version != 0 {
        return Err(invalid("semantic_source_chunk_representation", "live protected source chunk representation is invalid"));
      }
      fingerprint_chunk_representation(&mut fingerprint, &entity);
      check_cancelled(&fresh.cancellation)?;
      memory.check_admission()?;
    }
    if fingerprint.finalize().as_bytes() != &self.chunk_representation_fingerprint {
      return Err(invalid("semantic_source_stage_chunk_changed", "live chunk representations differ from the validated captured source"));
    }
    check_cancelled(&fresh.cancellation)?;
    self._memory.check_admission()?;
    Ok(())
  }
}
