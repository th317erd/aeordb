//! Root-specific task exclusion, rechecked at the final physical authority gate.
use super::*;
use crate::engine::v4::contract_generated::capability_bit;
use crate::engine::v4::database_header::DATABASE_HEADER_V4_REGION_LENGTH;

/// Opaque root-specific evidence, bound to the physical publisher, not a bitmap.
pub struct NativeSemanticTaskRootExclusionV1<'publisher> {
  publisher: &'publisher V4FirstAuthorityPublisher,
  frontier: [u8; DATABASE_HEADER_V4_REGION_LENGTH],
  namespace_root_hash: Vec<u8>,
  cancellation: CancellationToken,
  _memory: MemoryReservation,
}

impl fmt::Debug for NativeSemanticTaskRootExclusionV1<'_> {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("NativeSemanticTaskRootExclusionV1").finish_non_exhaustive()
  }
}

impl V4FirstAuthorityPublisher {
  /// Qualify only this root against a complete captured task contribution. The
  /// result may outlive the mark/protection but never its physical publisher.
  /// Any intervening authority publication invalidates it. This does not grant
  /// global mark completion or replace the other retirement/reclaim predicates.
  pub fn qualify_semantic_task_root_exclusion(
    &self,
    mark: &NativeSemanticTaskMarkV1<'_, '_>,
    namespace_root_hash: &[u8],
  ) -> Result<NativeSemanticTaskRootExclusionV1<'_>, SemanticTaskMarkErrorV1> {
    mark.capture.check_mark_admission()?;
    if !std::ptr::eq(self, mark.capture._protection.publisher()) {
      return Err(invalid("semantic_task_root_exclusion_owner", "task contribution belongs to another physical publisher").into());
    }
    let header = &mark.capture.header.selected.header;
    if namespace_root_hash.len() != header.hash_algorithm.hash_length() || namespace_root_hash.iter().all(|byte| *byte == 0) {
      return Err(invalid("semantic_task_root_exclusion_identity", "root identity must be nonzero and database hash width").into());
    }
    let bytes = (std::mem::size_of::<NativeSemanticTaskRootExclusionV1<'_>>() as u64)
      .checked_add(namespace_root_hash.len() as u64)
      .and_then(|bytes| bytes.checked_add(256))
      .ok_or_else(|| invalid("semantic_task_root_exclusion_memory_bound", "root exclusion evidence exceeds its memory bound"))?;
    let memory = mark.capture.memory.reserve(MemoryOwner::GarbageCollection, bytes, AdmissionClass::Maintenance)?;
    let scratch = mark.capture.reserve_mark_slot_scratch()?;
    if let Some((position, locator)) = mark.capture.snapshot.find_captured_slot(namespace_root_hash)? {
      if locator.type_flags != kv_tag::DIRECTORY {
        return Err(invalid("semantic_task_root_exclusion_role", "captured root key resolves to another physical role").into());
      }
      if mark.bitmap.is_marked(position)? {
        return Err(invalid("semantic_task_root_retained", "captured semantic tasks retain this root").into());
      }
    }
    let mut root = Vec::new();
    root
      .try_reserve_exact(namespace_root_hash.len())
      .map_err(|source| SemanticMutationObservationErrorV1::Allocation { code: "semantic_task_root_exclusion_allocation", source })?;
    root.extend_from_slice(namespace_root_hash);
    mark.capture.check_mark_admission()?;
    scratch.check_admission()?;
    memory.check_admission()?;
    Ok(NativeSemanticTaskRootExclusionV1 {
      publisher: self,
      frontier: mark.capture.header.region,
      namespace_root_hash: root,
      cancellation: mark.capture.cancellation.clone(),
      _memory: memory,
    })
  }

  // The caller retains the existing root guard through this check and commit.
  // No recapture, root lock, bitmap transfer or publication occurs here.
  pub(in crate::engine::v4::first_authority) fn validate_semantic_task_root_exclusion_locked(
    &self,
    _authority: &MutexGuard<'_, FirstAuthorityRootStateV1>,
    evidence: Option<&NativeSemanticTaskRootExclusionV1<'_>>,
    observation: &DatabaseHeaderObservationV4,
    namespace_root_hash: &[u8],
  ) -> Result<(), SemanticTaskMarkErrorV1> {
    let bit = capability_bit::SEMANTIC_MUTATION_TASK_V1;
    let index = usize::from(bit / 8);
    let mask = 1u8 << (bit % 8);
    let header = &observation.selected.header;
    let required = header.required_reader_capabilities[index] & mask != 0 || header.required_writer_capabilities[index] & mask != 0;
    let Some(evidence) = evidence else {
      return if required {
        Err(
          invalid("semantic_task_root_exclusion_required", "task-capable root retirement/reclaim requires native captured exclusion")
            .into(),
        )
      } else {
        Ok(())
      };
    };
    check_cancelled(&evidence.cancellation)?;
    evidence._memory.check_admission()?;
    if !std::ptr::eq(self, evidence.publisher) {
      return Err(invalid("semantic_task_root_exclusion_owner", "root exclusion belongs to another physical publisher").into());
    }
    if evidence.namespace_root_hash != namespace_root_hash {
      return Err(invalid("semantic_task_root_exclusion_target", "root exclusion names another target").into());
    }
    if observation.selected.redundancy_degraded || evidence.frontier != observation.region {
      return Err(invalid("semantic_task_root_exclusion_stale", "physical authority changed after task capture; capture again").into());
    }
    Ok(())
  }
}
