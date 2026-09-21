//! Guarded node persistence; no durable task, source-closure or activation permit.
use super::*;
use crate::engine::v4::contract_generated::capability_bit;
use crate::engine::v4::system_control::SystemControlV1;

#[derive(Clone, Copy, Debug)]
pub struct NativeSemanticSourceNodeStagingRequestV1<'a> {
  pub encoded_nodes: &'a [&'a [u8]],
  pub publication_timestamp_ms: u64,
  pub maximum_workspace_bytes: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum NativeSemanticSourceControlPublicationErrorV1 {
  #[error(transparent)]
  Observation(#[from] SemanticMutationObservationErrorV1),
  #[error(transparent)]
  Publication(#[from] ImmutableSystemControlPublicationErrorV1),
}

impl NativeSemanticSourceControlPublicationErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Observation(source) => source.code(),
      Self::Publication(source) => source.code(),
    }
  }

  pub fn committed_receipt(&self) -> Option<&ImmutableSystemControlBatchPublicationReceiptV1> {
    match self {
      Self::Observation(_) => None,
      Self::Publication(source) => source.committed_receipt(),
    }
  }
}

impl NativeSemanticMutationInventoryV1<'_> {
  /// Stage at most one paired node emission, without selecting task authority.
  /// The capture's protection remains required; these receipts prove neither
  /// complete catalog closure nor durable retention. Shared nodes preserve
  /// their existing canonical wrapper metadata across later captures.
  pub fn stage_semantic_source_nodes(
    &self,
    request: NativeSemanticSourceNodeStagingRequestV1<'_>,
  ) -> Result<ImmutableSystemControlBatchPublicationReceiptV1, NativeSemanticSourceControlPublicationErrorV1> {
    self.stage_semantic_source_nodes_observed(request, || {}, &mut NoopFirstAuthorityDependencyObserverV1)
  }

  pub(in crate::engine::v4::first_authority) fn stage_semantic_source_nodes_observed(
    &self,
    request: NativeSemanticSourceNodeStagingRequestV1<'_>,
    before_lock: impl FnOnce(),
    observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<ImmutableSystemControlBatchPublicationReceiptV1, NativeSemanticSourceControlPublicationErrorV1> {
    check_cancelled(&self.cancellation)?;
    self._memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    if request.encoded_nodes.is_empty() || request.encoded_nodes.len() > 2 {
      return Err(invalid("semantic_source_node_count", "source node staging requires one node or one paired emission").into());
    }
    if request.publication_timestamp_ms == 0 || request.publication_timestamp_ms > i64::MAX as u64 {
      return Err(invalid("semantic_source_node_time", "source node publication time must fit the signed persistent range").into());
    }
    let kind = SystemControlKindV1::SemanticSourceNode;
    let mut body_bytes = 0usize;
    for bytes in request.encoded_nodes {
      if bytes.len() > kind.encoded_cap() {
        return Err(invalid("semantic_source_node_size", "source node exceeds its encoded control cap").into());
      }
      body_bytes =
        body_bytes.checked_add(bytes.len()).ok_or_else(|| invalid("semantic_source_node_size", "source node byte count overflowed"))?;
    }
    // Inputs are caller-owned. Bound wrapper decoding, exact-body readback,
    // encoding, transaction and receipt scratch before any owned allocation.
    let workspace = body_bytes
      .checked_add(2 * FIRST_AUTHORITY_CONTROL_ENTITY_CAP)
      .and_then(|bytes| bytes.checked_mul(8))
      .and_then(|bytes| bytes.checked_add(128 << 10))
      .filter(|bytes| *bytes <= request.maximum_workspace_bytes)
      .ok_or(SemanticMutationObservationErrorV1::Resource {
        code: "semantic_source_node_workspace",
        message: "source node staging exceeds its admitted workspace",
      })?;
    let memory = self
      .memory
      .reserve(MemoryOwner::Task, workspace as u64, AdmissionClass::Maintenance)
      .map_err(SemanticMutationObservationErrorV1::from)?;
    let captured_header = &self.header.selected.header;
    let mut decoded = Vec::new();
    decoded.try_reserve_exact(request.encoded_nodes.len()).map_err(node_allocation)?;
    for bytes in request.encoded_nodes {
      check_cancelled(&self.cancellation)?;
      let control = decode_system_control(bytes, captured_header.hash_algorithm).map_err(SemanticMutationObservationErrorV1::from)?;
      if control.kind != kind || control.database_id != captured_header.database_id {
        return Err(invalid("semantic_source_node_identity", "staged control is not a source node for the captured database").into());
      }
      if let Some(index) = decoded.iter().position(|prior: &SystemControlV1<'_>| prior.identity == control.identity) {
        if request.encoded_nodes[index] != *bytes {
          return Err(invalid("semantic_source_node_collision", "paired node identity names different exact bytes").into());
        }
        continue;
      }
      decoded.push(control);
    }
    let mut controls = Vec::new();
    controls.try_reserve_exact(decoded.len()).map_err(node_allocation)?;
    for (index, control) in decoded.iter().enumerate() {
      controls.push(ImmutableSystemControlWriteV1 { kind, identity: &control.identity, encoded_control: request.encoded_nodes[index] });
    }
    self.stage_captured_source_controls(&controls, request.publication_timestamp_ms, &memory, before_lock, observer)
  }

  // Private composition point for source nodes and the derived initial pair.
  // Mutable tasks/generation and arbitrary immutable controls remain refused.
  pub(super) fn stage_captured_source_controls(
    &self,
    controls: &[ImmutableSystemControlWriteV1<'_>],
    publication_timestamp_ms: u64,
    memory: &MemoryReservation,
    before_lock: impl FnOnce(),
    observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<ImmutableSystemControlBatchPublicationReceiptV1, NativeSemanticSourceControlPublicationErrorV1> {
    self.stage_captured_source_controls_guarded(controls, publication_timestamp_ms, memory, (before_lock, observer), None)
  }

  pub(in crate::engine::v4::first_authority) fn stage_captured_work_controls(
    &self,
    controls: &[ImmutableSystemControlWriteV1<'_>],
    publication_timestamp_ms: u64,
    memory: &MemoryReservation,
    work: &NativeSemanticTaskWorkV1<'_>,
    hooks: (impl FnOnce(), &mut dyn FirstAuthorityDependencyObserverV1),
  ) -> Result<ImmutableSystemControlBatchPublicationReceiptV1, NativeSemanticSourceControlPublicationErrorV1> {
    self.stage_captured_source_controls_guarded(controls, publication_timestamp_ms, memory, hooks, Some(work))
  }

  fn stage_captured_source_controls_guarded(
    &self,
    controls: &[ImmutableSystemControlWriteV1<'_>],
    publication_timestamp_ms: u64,
    memory: &MemoryReservation,
    hooks: (impl FnOnce(), &mut dyn FirstAuthorityDependencyObserverV1),
    work: Option<&NativeSemanticTaskWorkV1<'_>>,
  ) -> Result<ImmutableSystemControlBatchPublicationReceiptV1, NativeSemanticSourceControlPublicationErrorV1> {
    let (before_lock, observer) = hooks;
    let nodes =
      !controls.is_empty() && controls.len() <= 2 && controls.iter().all(|control| control.kind == SystemControlKindV1::SemanticSourceNode);
    let pair = controls.len() == 2
      && controls[0].kind == SystemControlKindV1::SemanticMutationCheckpoint
      && controls[1].kind == SystemControlKindV1::SemanticSourceCapture;
    if (work.is_some() || !nodes) && !pair {
      return Err(
        invalid("semantic_source_staging_kind", "source staging accepts nodes or the derived initial checkpoint pair only").into(),
      );
    }
    let captured_header = &self.header.selected.header;
    before_lock();
    let publisher = self._protection.publisher();
    let authority = publisher.root_state.lock().map_err(|poisoned| {
      drop(poisoned);
      SemanticMutationObservationErrorV1::from(FirstAuthorityPublicationErrorV1::StateLockPoisoned)
    })?;
    check_cancelled(&self.cancellation)?;
    memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    if authority.staging_accounting_failed || authority.active_staging_protections == 0 {
      return Err(invalid("semantic_source_node_protection", "source node staging lost healthy process-local protection").into());
    }
    let observation = publisher.observe().map_err(SemanticMutationObservationErrorV1::from)?;
    let header = &observation.selected.header;
    if observation.selected.redundancy_degraded || header.head_hash.iter().all(|byte| *byte == 0) {
      return Err(invalid("semantic_source_node_authority", "source node staging requires a healthy selected HEAD").into());
    }
    if header.database_id != captured_header.database_id
      || header.hash_algorithm != captured_header.hash_algorithm
      || header.physical_instance_id != captured_header.physical_instance_id
      || header.writer_fence_epoch != captured_header.writer_fence_epoch
    {
      return Err(invalid("semantic_source_node_owner", "source node staging cannot cross physical ownership or writer fences").into());
    }
    if header.slot_sequence < captured_header.slot_sequence || header.write_sequence_high_water < captured_header.write_sequence_high_water
    {
      return Err(invalid("semantic_source_node_regression", "source node authority regressed behind its capture").into());
    }
    for bit in [capability_bit::SEMANTIC_MUTATION_TASK_V1, capability_bit::SEMANTIC_SOURCE_CAPTURE_V1] {
      let index = usize::from(bit / 8);
      let mask = 1u8 << (bit % 8);
      if header.required_reader_capabilities[index] & mask == 0
        || header.required_writer_capabilities[index] & mask == 0
        || captured_header.required_reader_capabilities[index] & mask == 0
        || captured_header.required_writer_capabilities[index] & mask == 0
      {
        return Err(
          invalid("semantic_source_node_capability", "captured and current authority must declare source/task capabilities").into(),
        );
      }
    }
    if let Some(work) = work {
      if observation.region != self.header.region {
        return Err(invalid("semantic_task_work_frontier", "authority changed before checkpoint pair staging").into());
      }
      // Exact task/generation admission precedes retry and the shared KV flush.
      work.validate_selected_work(publisher, &observation)?;
    }
    let mut prepared = Vec::new();
    prepared.try_reserve_exact(controls.len()).map_err(node_allocation)?;
    let mut existing_receipts = Vec::new();
    existing_receipts.try_reserve_exact(controls.len()).map_err(node_allocation)?;
    {
      let kv = publisher.lock_kv().map_err(SemanticMutationObservationErrorV1::from)?;
      validate_kv_header_alignment(&kv, header).map_err(SemanticMutationObservationErrorV1::from)?;
      for control in controls {
        check_cancelled(&self.cancellation)?;
        memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
        let path = system_control_path(control.kind, control.identity, SystemControlSlotV1::Immutable)
          .map_err(SemanticMutationObservationErrorV1::from)?;
        // Different body length cannot name the identical immutable node. The
        // existing canonical loader validates all framing/chunk/content checks.
        let existing = load_canonical_system_file_at_path(
          &publisher.file,
          &kv,
          header,
          &path,
          SYSTEM_CONTROL_CONTENT_TYPE,
          control.encoded_control.len(),
        )
        .map_err(SemanticMutationObservationErrorV1::from)?;
        let timestamp = publication_timestamp_ms as i64;
        let (created_at, updated_at) = match existing.as_ref() {
          Some(existing) => (existing.record.created_at, existing.record.updated_at),
          None => (timestamp, timestamp),
        };
        let item = prepare_system_control_file_record(path, control.encoded_control, header.hash_algorithm, created_at, updated_at)
          .map_err(SemanticMutationObservationErrorV1::from)?;
        if let Some(existing) = existing {
          let entity = decode_whole_entity(&existing.entity_bytes, header.hash_algorithm, header.write_sequence_high_water)
            .map_err(SemanticMutationObservationErrorV1::from)?;
          if existing.body != control.encoded_control || entity.stored_value != item.record_value {
            return Err(
              invalid("semantic_source_node_collision", "existing source node or wrapper differs from the exact canonical bytes").into(),
            );
          }
          let mut path_key = Vec::new();
          path_key.try_reserve_exact(existing.locator.hash.len()).map_err(node_allocation)?;
          path_key.extend_from_slice(&existing.locator.hash);
          existing_receipts.push(ImmutableSystemControlPublicationReceiptV1 {
            kind: control.kind,
            path_key,
            control_sequence: 1,
            write_sequence: existing.write_sequence,
            idempotent: true,
          });
        }
        prepared.push(item);
      }
    }
    check_cancelled(&self.cancellation)?;
    memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    // Do not reach the generic transaction's baseline KV flush for a fully
    // existing repeat: it must remain byte-stable at any later caller time.
    if existing_receipts.len() == controls.len() {
      return Ok(ImmutableSystemControlBatchPublicationReceiptV1 { controls: existing_receipts, observation, idempotent: true });
    }
    drop(existing_receipts);
    let mut entities = Vec::new();
    entities.try_reserve_exact(2 * controls.len()).map_err(node_allocation)?;
    for (control, prepared) in controls.iter().zip(&prepared) {
      entities.push(ImmutableEntityWriteV1 {
        entity_version: 0,
        entry_type: EntryTypeV4::Chunk,
        flags: WHOLE_ENTITY_V1_FLAG_SYSTEM,
        key: &prepared.chunk_key,
        stored_value: control.encoded_control,
      });
      entities.push(ImmutableEntityWriteV1 {
        entity_version: 1,
        entry_type: EntryTypeV4::FileRecord,
        flags: WHOLE_ENTITY_V1_FLAG_SYSTEM,
        key: &prepared.path_key,
        stored_value: &prepared.record_value,
      });
    }
    let mut translated = Vec::new();
    translated.try_reserve_exact(controls.len()).map_err(node_allocation)?;
    let publication =
      ImmutableEntityBatchPublicationRequestV1 { database_id: &header.database_id, entities: &entities, publication_timestamp_ms };
    check_cancelled(&self.cancellation)?;
    memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    // Keep the root guard through the sole publisher. The KV guard is released.
    // Once committed, report its receipt even if cancellation arrives afterward.
    let result = validate_immutable_entity_batch_request(&publication).and_then(|()| {
      publisher.publish_immutable_entity_batch_with_validation_locked(
        publication,
        observer,
        ImmutableEntityValidationV1::PrevalidatedSystemFiles,
      )
    });
    match result {
      Ok(receipt) => {
        let (receipt, complete) = translate_immutable_system_control_receipt(receipt, controls, translated);
        if complete {
          Ok(receipt)
        } else {
          Err(
            ImmutableSystemControlPublicationErrorV1::committed(
              "immutable_system_control_receipt_shape",
              "stable entity publication returned an invalid source-node receipt shape",
              receipt,
            )
            .into(),
          )
        }
      }
      Err(ImmutableEntityBatchPublicationErrorV1::Committed { code, message, receipt }) => {
        let (receipt, complete) = translate_immutable_system_control_receipt(*receipt, controls, translated);
        let message = if complete { message } else { format!("{message}; source-node receipt shape is incomplete") };
        Err(ImmutableSystemControlPublicationErrorV1::committed(code, message, receipt).into())
      }
      Err(ImmutableEntityBatchPublicationErrorV1::Invalid { code, message }) => {
        Err(ImmutableSystemControlPublicationErrorV1::Invalid { code, message }.into())
      }
      Err(ImmutableEntityBatchPublicationErrorV1::Authority(source)) => {
        Err(ImmutableSystemControlPublicationErrorV1::Authority(source).into())
      }
    }
  }
}

fn node_allocation(source: std::collections::TryReserveError) -> SemanticMutationObservationErrorV1 {
  SemanticMutationObservationErrorV1::Allocation { code: "semantic_source_node_allocation", source }
}
