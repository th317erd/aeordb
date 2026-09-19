//! Captured base/generation binding; not a task, retention or activation permit.
use super::*;

pub(in crate::engine::v4::first_authority) struct NativeSemanticSourceBaseV1<'a> {
  _capture: &'a NativeSemanticMutationInventoryV1<'a>,
  pub(in crate::engine::v4::first_authority) authority: SelectedSemanticAuthorityV1,
  pub(in crate::engine::v4::first_authority) generation: LoadedMutableSystemControlV1,
  _memory: MemoryReservation,
}

impl NativeSemanticMutationInventoryV1<'_> {
  #[cfg(test)]
  pub(in crate::engine::v4::first_authority) fn with_namespace_authority_for_test(
    &self,
    root_hash: &[u8],
    maximum_read_bytes: u64,
    inspect: impl FnOnce(Option<&ImmutableNamespaceAuthorityV1>),
  ) -> Result<(), SemanticMutationObservationErrorV1> {
    check_cancelled(&self.cancellation)?;
    self._memory.check_admission()?;
    validate_captured_authority_header(&self.header.selected, &self.header.selected, root_hash)?;
    if maximum_read_bytes == 0 {
      return Err(invalid("captured_namespace_read_bound", "captured namespace read allowance must be positive"));
    }
    // Scoped test harness only. Production retention owns this admission in its
    // enclosing graph operation, not through a second public read capability.
    let scratch = (self.bounds.maximum_entity_bytes as u64)
      .checked_mul(8)
      .and_then(|bytes| bytes.checked_add(CAPTURE_SCRATCH_BYTES + 16 * FIRST_AUTHORITY_CONTROL_ENTITY_CAP as u64))
      .ok_or_else(|| invalid("captured_namespace_memory_bound", "namespace reader scratch overflowed"))?;
    let memory = self.memory.reserve(MemoryOwner::Task, scratch, AdmissionClass::Maintenance)?;
    let lookup = CapturedEntityLookupV1 {
      snapshot: &self.snapshot,
      header: &self.header.selected.header,
      bounds: self.bounds,
      cancellation: &self.cancellation,
      remaining_read_bytes: Cell::new(maximum_read_bytes.min(self.bounds.maximum_read_bytes)),
    };
    let authority = load_namespace_authority_from_lookup(
      &self._protection.publisher().file,
      &lookup,
      &self.header.selected,
      root_hash,
      &self.cancellation,
    )?;
    check_cancelled(&self.cancellation)?;
    memory.check_admission()?;
    inspect(authority.as_ref());
    check_cancelled(&self.cancellation)?;
    memory.check_admission()?;
    Ok(())
  }

  pub(in crate::engine::v4::first_authority) fn read_source_base_from_lookup(
    &self,
    expected_root: &[u8],
    lookup: &impl FirstAuthorityEntityLookupV1,
    before_complete: impl FnOnce(),
  ) -> Result<NativeSemanticSourceBaseV1<'_>, SemanticMutationObservationErrorV1> {
    check_cancelled(&self.cancellation)?;
    self._memory.check_admission()?;
    let header = selected_semantic_authority_header(&self.header)?;
    if expected_root != header.head_hash {
      return Err(invalid("semantic_source_base_root_mismatch", "requested base differs from the captured HEAD"));
    }
    // Canonical controls may reach their framing cap before typed decoding.
    // Account overlapping slot bodies/copies and FileRecord scratch, not just
    // the much smaller valid generation payload. Shared loaders retain their
    // existing transitive allocator limitations; this is admission accounting.
    let control_cap =
      SystemControlKindV1::RootAdmissionCommit.encoded_cap().max(SystemControlKindV1::SemanticMutationGeneration.encoded_cap()) as u64;
    let metadata = 4
      * (FIRST_AUTHORITY_NAMESPACE_ROOT_ENTITY_CAP + super::super::super::super::semantic_store::semantic_object_cap(1)?) as u64
      + CAPTURE_SCRATCH_BYTES;
    let scratch = control_cap
      .checked_add(FIRST_AUTHORITY_CONTROL_ENTITY_CAP as u64)
      .and_then(|bytes| bytes.checked_mul(8))
      .and_then(|bytes| bytes.checked_add(metadata))
      .ok_or_else(|| invalid("semantic_source_base_memory_bound", "captured base memory accounting overflowed"))?;
    let mut memory = self.memory.reserve(MemoryOwner::Task, scratch, AdmissionClass::Maintenance)?;
    let file = &self._protection.publisher().file;
    let authority = load_selected_semantic_authority_from_lookup(file, lookup, &self.header)?;
    check_cancelled(&self.cancellation)?;
    memory.check_admission()?;
    let generation = load_mutable_system_control_pair(file, lookup, header, SystemControlKindV1::SemanticMutationGeneration, &[])?
      .selected
      .ok_or_else(|| invalid("semantic_source_base_generation_missing", "captured source base has no selected semantic generation"))?;
    // Root/state identity buffers are bounded by their encoded caps. Preserve
    // the selected slot/digest/bytes for later exact expectations, not only its
    // integer sequence; release all temporary control-loader workspace now.
    let retained = metadata + generation.bytes.capacity() as u64 + generation.control_digest.capacity() as u64;
    let release = scratch
      .checked_sub(retained)
      .ok_or_else(|| invalid("semantic_source_base_memory_bound", "captured base exceeded its reserved workspace"))?;
    memory.shrink(release)?;
    before_complete();
    check_cancelled(&self.cancellation)?;
    self._memory.check_admission()?;
    memory.check_admission()?;
    Ok(NativeSemanticSourceBaseV1 { _capture: self, authority, generation, _memory: memory })
  }

  #[cfg(test)]
  pub(in crate::engine::v4::first_authority) fn read_source_base_for_test(
    &self,
    expected_root: &[u8],
    maximum_read_bytes: u64,
    before_complete: impl FnOnce(),
  ) -> Result<NativeSemanticSourceBaseV1<'_>, SemanticMutationObservationErrorV1> {
    let lookup = CapturedEntityLookupV1 {
      snapshot: &self.snapshot,
      header: &self.header.selected.header,
      bounds: self.bounds,
      cancellation: &self.cancellation,
      remaining_read_bytes: Cell::new(maximum_read_bytes.min(self.bounds.maximum_read_bytes)),
    };
    self.read_source_base_from_lookup(expected_root, &lookup, before_complete)
  }
}
