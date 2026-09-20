//! Target-bound task contribution, never complete GC or mutation authority.
use super::*;
use crate::engine::v4::contract_generated::capability_bit;
use crate::engine::v4::database_header::DATABASE_HEADER_V4_REGION_LENGTH;
use crate::engine::v4::gc::PhysicalIncarnationV1;
use crate::engine::v4::gc_quarantine::{
  PhysicalQuarantineCandidateV1, QuarantineEffectiveClosureLimitsV1, QuarantineEffectiveClosureRequestV1, QuarantineEffectiveClosureV1,
  decode_quarantine_manifest_v1,
};

#[derive(Clone, Copy, Debug)]
pub struct NativeSemanticTaskPhysicalExclusionBoundsV1 {
  /// Bound on the validated immutable candidate support graph.
  pub maximum_support_artifacts: u64,
  /// Input rows plus identity comparisons; sweep charges one per candidate.
  pub maximum_work: u64,
  /// Support bodies, candidate prefixes and logical captured-slot page reads.
  pub maximum_read_bytes: u64,
}

/// A target-specific task predicate, not complete mark or physical integrity
/// authority. It retains no snapshot or bitmap after qualification.
pub struct NativeSemanticTaskPhysicalExclusionV1<'publisher> {
  publisher: &'publisher V4FirstAuthorityPublisher,
  frontier: [u8; DATABASE_HEADER_V4_REGION_LENGTH],
  kind: GcArtifactKindV1,
  target_key: Vec<u8>,
  cancellation: CancellationToken,
  _memory: MemoryReservation,
}

impl fmt::Debug for NativeSemanticTaskPhysicalExclusionV1<'_> {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("NativeSemanticTaskPhysicalExclusionV1").finish_non_exhaustive()
  }
}

impl V4FirstAuthorityPublisher {
  pub fn qualify_semantic_task_quarantine_exclusion(
    &self,
    mark: &NativeSemanticTaskMarkV1<'_, '_>,
    artifact: &EncodedImmutableGcArtifactV1,
    bounds: NativeSemanticTaskPhysicalExclusionBoundsV1,
  ) -> Result<NativeSemanticTaskPhysicalExclusionV1<'_>, PhysicalQuarantinePublicationErrorV1> {
    let proof = self.begin_physical_task_exclusion(mark, GcArtifactKindV1::QuarantineManifest, &artifact.key, bounds)?;
    let header = &mark.capture.header.selected.header;
    let manifest = decode_quarantine_manifest_v1(&artifact.value, header.hash_algorithm)?;
    if manifest.key != artifact.key || manifest.database_id != header.database_id {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "semantic_task_physical_exclusion_identity",
        "quarantine target differs from its bytes or captured database",
      ));
    }
    let lookup = mark.capture.physical_exclusion_lookup(bounds);
    let reader = PhysicalQuarantineSupportReadContextV1 { file: &self.file, kv: &lookup, header, memory: &mark.capture.memory };
    let lifecycle_entity = reader.load_entity(manifest.captured_root_lifecycle_manifest, GcArtifactKindV1::RootLifecycleManifest)?;
    let lifecycle_whole = decode_whole_entity(&lifecycle_entity.bytes, header.hash_algorithm, header.write_sequence_high_water)?;
    let GcStateArtifactV1::Manifest(lifecycle) = decode_gc_state_artifact(lifecycle_whole.stored_value, header.hash_algorithm)? else {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid("quarantine_support_lifecycle_kind", "expected root-lifecycle manifest"));
    };
    let root_entity =
      manifest.candidate_directory_root.map(|key| reader.load_entity(key, GcArtifactKindV1::GcArtifactDirectoryNode)).transpose()?;
    let directory = match &root_entity {
      Some(entity) => {
        let whole = decode_whole_entity(&entity.bytes, header.hash_algorithm, header.write_sequence_high_water)?;
        let GcStateArtifactV1::Directory(directory) = decode_gc_state_artifact(whole.stored_value, header.hash_algorithm)? else {
          return Err(PhysicalQuarantinePublicationErrorV1::invalid("quarantine_support_kind", "expected candidate directory"));
        };
        Some(directory)
      }
      None => None,
    };
    // The frozen manifest caps this list at 256. Charge both descriptor arrays
    // before allocating; every raw entity separately owns its buffer charge.
    let descriptor_bytes = (manifest.delta_count as u64)
      * (std::mem::size_of::<ChargedPhysicalQuarantineSupportEntityV1>() + std::mem::size_of::<&[u8]>()) as u64
      + 1024;
    let descriptors = mark
      .capture
      .memory
      .reserve(MemoryOwner::GarbageCollection, descriptor_bytes, AdmissionClass::Maintenance)
      .map_err(SemanticTaskMarkErrorV1::from)?;
    let mut deltas = Vec::new();
    deltas.try_reserve_exact(manifest.delta_count as usize).map_err(physical_exclusion_allocation)?;
    let mut total_delta_bytes = 0u64;
    for key in manifest.delta_hashes.chunks_exact(header.hash_algorithm.hash_length()) {
      mark.capture.check_mark_admission()?;
      descriptors.check_admission().map_err(SemanticTaskMarkErrorV1::from)?;
      let entity = reader.load_entity(key, GcArtifactKindV1::CandidateDelta)?;
      let whole = decode_whole_entity(&entity.bytes, header.hash_algorithm, header.write_sequence_high_water)?;
      total_delta_bytes =
        total_delta_bytes.checked_add(whole.stored_value.len() as u64).filter(|bytes| *bytes <= 64 << 20).ok_or_else(|| {
          PhysicalQuarantinePublicationErrorV1::invalid("quarantine_delta_bytes", "delta bodies exceed their frozen byte bound")
        })?;
      deltas.push(entity);
    }
    let mut values = Vec::new();
    values.try_reserve_exact(deltas.len()).map_err(physical_exclusion_allocation)?;
    for delta in &deltas {
      let whole = decode_whole_entity(&delta.bytes, header.hash_algorithm, header.write_sequence_high_water)?;
      values.push(whole.stored_value);
    }
    let mut effective = QuarantineEffectiveClosureV1::new(
      QuarantineEffectiveClosureRequestV1 {
        manifest: &manifest,
        directory: directory.as_ref(),
        lifecycle: &lifecycle,
        delta_values: &values,
        hash_algorithm: header.hash_algorithm,
        limits: QuarantineEffectiveClosureLimitsV1 {
          maximum_support_artifacts: bounds.maximum_support_artifacts,
          maximum_work: bounds.maximum_work,
        },
      },
      mark.capture.cancellation.clone(),
      &mark.capture.memory,
    )?;
    let physical_length = self.file.metadata().map_err(EngineError::from).map_err(FirstAuthorityPublicationErrorV1::from)?.len();
    let mut visitor = |candidate: PhysicalQuarantineCandidateV1<'_>| {
      proof._memory.check_admission().map_err(SemanticTaskMarkErrorV1::from)?;
      self.check_task_physical_incarnation(mark, &lookup, physical_length, candidate.incarnation)
    };
    if let Some(directory) = &directory {
      reader.revalidate_subtree(directory, 0, &mut EffectivePhysicalTaskObserverV1 { effective: &mut effective, visitor: &mut visitor })?;
    }
    effective.finish(&mut visitor)?;
    mark.capture.check_mark_admission()?;
    descriptors.check_admission().map_err(SemanticTaskMarkErrorV1::from)?;
    proof._memory.check_admission().map_err(SemanticTaskMarkErrorV1::from)?;
    Ok(proof)
  }

  pub fn qualify_semantic_task_sweep_exclusion(
    &self,
    mark: &NativeSemanticTaskMarkV1<'_, '_>,
    artifact: &EncodedImmutableGcArtifactV1,
    bounds: NativeSemanticTaskPhysicalExclusionBoundsV1,
  ) -> Result<NativeSemanticTaskPhysicalExclusionV1<'_>, SweepLocatorRemovalErrorV1> {
    let proof = self.begin_physical_task_exclusion(mark, GcArtifactKindV1::SweepProposal, &artifact.key, bounds)?;
    let header = &mark.capture.header.selected.header;
    let SweepVoidArtifactV1::SweepProposal(proposal) = decode_sweep_void_artifact(&artifact.value, header.hash_algorithm)? else {
      return Err(SweepLocatorRemovalErrorV1::invalid("semantic_task_physical_exclusion_identity", "expected sweep proposal"));
    };
    if proposal.key != artifact.key
      || proposal.database_id != header.database_id
      || u64::from(proposal.candidate_count) > bounds.maximum_work
    {
      return Err(SweepLocatorRemovalErrorV1::invalid(
        "semantic_task_physical_exclusion_identity",
        "proposal identity or candidate bound disagrees",
      ));
    }
    let lookup = mark.capture.physical_exclusion_lookup(bounds);
    let physical_length = self.file.metadata().map_err(EngineError::from).map_err(FirstAuthorityPublicationErrorV1::from)?.len();
    for candidate in proposal.candidate_records(header.hash_algorithm)? {
      self.check_task_physical_incarnation(mark, &lookup, physical_length, candidate?)?;
    }
    mark.capture.check_mark_admission().map_err(PhysicalQuarantinePublicationErrorV1::from)?;
    proof._memory.check_admission().map_err(SemanticTaskMarkErrorV1::from).map_err(PhysicalQuarantinePublicationErrorV1::from)?;
    Ok(proof)
  }

  fn begin_physical_task_exclusion(
    &self,
    mark: &NativeSemanticTaskMarkV1<'_, '_>,
    kind: GcArtifactKindV1,
    key: &[u8],
    bounds: NativeSemanticTaskPhysicalExclusionBoundsV1,
  ) -> Result<NativeSemanticTaskPhysicalExclusionV1<'_>, PhysicalQuarantinePublicationErrorV1> {
    mark.capture.check_mark_admission()?;
    if !std::ptr::eq(self, mark.capture._protection.publisher()) {
      return Err(
        SemanticTaskMarkErrorV1::from(invalid("semantic_task_physical_exclusion_owner", "task mark belongs to another physical publisher"))
          .into(),
      );
    }
    if bounds.maximum_support_artifacts == 0
      || bounds.maximum_work == 0
      || bounds.maximum_read_bytes == 0
      || key.len() != mark.capture.header.selected.header.hash_algorithm.hash_length()
      || key.iter().all(|byte| *byte == 0)
    {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "semantic_task_physical_exclusion_bounds",
        "positive bounds and an exact target key are required",
      ));
    }
    let bytes = std::mem::size_of::<NativeSemanticTaskPhysicalExclusionV1<'_>>() as u64 + key.len() as u64 + 1024;
    let memory = mark
      .capture
      .memory
      .reserve(MemoryOwner::GarbageCollection, bytes, AdmissionClass::Maintenance)
      .map_err(SemanticTaskMarkErrorV1::from)?;
    let mut target_key = Vec::new();
    target_key.try_reserve_exact(key.len()).map_err(physical_exclusion_allocation)?;
    target_key.extend_from_slice(key);
    Ok(NativeSemanticTaskPhysicalExclusionV1 {
      publisher: self,
      frontier: mark.capture.header.region,
      kind,
      target_key,
      cancellation: mark.capture.cancellation.clone(),
      _memory: memory,
    })
  }

  fn check_task_physical_incarnation(
    &self,
    mark: &NativeSemanticTaskMarkV1<'_, '_>,
    lookup: &CapturedEntityLookupV1<'_>,
    physical_length: u64,
    candidate: PhysicalIncarnationV1<'_>,
  ) -> Result<(), PhysicalQuarantinePublicationErrorV1> {
    mark.capture.check_mark_admission()?;
    let kind = EntryTypeV4::from_u8(candidate.entry_type)?;
    let mut hash = Vec::new();
    hash.try_reserve_exact(candidate.logical_key.len()).map_err(physical_exclusion_allocation)?;
    hash.extend_from_slice(candidate.logical_key);
    let locator = KVEntry { type_flags: inventory_kv_tag(kind), hash, offset: candidate.wal_offset, total_length: candidate.entity_length };
    read_locator_metadata(&self.file, lookup, &locator, lookup.header.write_sequence_high_water, physical_length, |header| {
      if header.entry_type != kind
        || header.entity_version != candidate.entity_version
        || header.write_sequence != candidate.write_sequence
        || header.integrity_hash != candidate.integrity_or_legacy_digest
      {
        return Err(FirstAuthorityPublicationErrorV1::invalid(
          "semantic_task_physical_incarnation",
          "candidate differs from its checked physical incarnation",
        ));
      }
      Ok(())
    })?;
    lookup.charge_read_bytes(page_size(lookup.header.hash_algorithm.hash_length()) as u64)?;
    if mark.is_captured_locator_marked(&locator)? {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "semantic_task_physical_retained",
        "captured semantic tasks retain this exact physical incarnation",
      ));
    }
    mark.capture.check_mark_admission()?;
    Ok(())
  }

  pub(in crate::engine::v4::first_authority) fn validate_semantic_task_physical_exclusion_locked(
    &self,
    _authority: &MutexGuard<'_, FirstAuthorityRootStateV1>,
    proof: Option<&NativeSemanticTaskPhysicalExclusionV1<'_>>,
    observation: &DatabaseHeaderObservationV4,
    kind: GcArtifactKindV1,
    key: &[u8],
  ) -> Result<(), PhysicalQuarantinePublicationErrorV1> {
    let bit = capability_bit::SEMANTIC_MUTATION_TASK_V1;
    let index = usize::from(bit / 8);
    let mask = 1u8 << (bit % 8);
    let header = &observation.selected.header;
    let required = header.required_reader_capabilities[index] & mask != 0 || header.required_writer_capabilities[index] & mask != 0;
    let Some(proof) = proof else {
      return if required {
        Err(PhysicalQuarantinePublicationErrorV1::invalid(
          "semantic_task_physical_exclusion_required",
          "task-capable quarantine/sweep requires native captured exclusion",
        ))
      } else {
        Ok(())
      };
    };
    check_cancelled(&proof.cancellation).map_err(SemanticTaskMarkErrorV1::from)?;
    proof._memory.check_admission().map_err(SemanticTaskMarkErrorV1::from)?;
    if !std::ptr::eq(self, proof.publisher) {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "semantic_task_physical_exclusion_owner",
        "proof belongs to another physical publisher",
      ));
    }
    if proof.kind != kind || proof.target_key != key {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "semantic_task_physical_exclusion_target",
        "proof names another artifact kind or target",
      ));
    }
    if observation.selected.redundancy_degraded || proof.frontier != observation.region {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "semantic_task_physical_exclusion_stale",
        "physical authority changed after task capture; capture again",
      ));
    }
    Ok(())
  }
}

impl NativeSemanticMutationInventoryV1<'_> {
  fn physical_exclusion_lookup(&self, bounds: NativeSemanticTaskPhysicalExclusionBoundsV1) -> CapturedEntityLookupV1<'_> {
    CapturedEntityLookupV1 {
      snapshot: &self.snapshot,
      header: &self.header.selected.header,
      bounds: self.bounds,
      cancellation: &self.cancellation,
      remaining_read_bytes: Cell::new(bounds.maximum_read_bytes.min(self.bounds.maximum_read_bytes)),
    }
  }
}

struct EffectivePhysicalTaskObserverV1<'a, 'manifest, F> {
  effective: &'a mut QuarantineEffectiveClosureV1<'manifest>,
  visitor: &'a mut F,
}

impl<F: FnMut(PhysicalQuarantineCandidateV1<'_>) -> Result<(), PhysicalQuarantinePublicationErrorV1>> PhysicalQuarantineBaseObserverV1
  for EffectivePhysicalTaskObserverV1<'_, '_, F>
{
  fn observe_page(&mut self, page: &crate::engine::v4::gc_state::GcStatePageV1<'_>) -> Result<(), PhysicalQuarantinePublicationErrorV1> {
    self.effective.observe_base_page(page, self.visitor)
  }
  fn observe_directory(
    &mut self,
    directory: &crate::engine::v4::gc_state::GcStateDirectoryV1<'_>,
  ) -> Result<(), PhysicalQuarantinePublicationErrorV1> {
    self.effective.observe_base_directory(directory).map_err(Into::into)
  }
}

fn physical_exclusion_allocation(source: std::collections::TryReserveError) -> PhysicalQuarantinePublicationErrorV1 {
  SemanticTaskMarkErrorV1::from(SemanticMutationObservationErrorV1::Allocation {
    code: "semantic_task_physical_exclusion_allocation",
    source,
  })
  .into()
}
