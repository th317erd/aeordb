//! Storage-neutral ordering contract for one immutable index-generation publication.

use std::collections::HashSet;

use crate::engine::HashAlgorithm;
use crate::engine::durability_coordinator::{CommitClass, DurabilityCommitReceipt};

use super::index_artifact::{
  ActivePointerKindV1, ActivePointerRewritePlanV1, EncodedActivePointerV1, EncodedImmutableIndexArtifactV1, ImmutableIndexArtifactKindV1,
  IndexManifestKindV1, decode_active_pointer, decode_immutable_index_artifact, decode_index_manifest,
};
use super::reader::{FormatError, FormatResult, MalformedInputClass};

pub const INDEX_GENERATION_DEPENDENCY_HARD_CAP_V1: usize = 4_095;
pub const INDEX_GENERATION_TOTAL_BYTES_HARD_CAP_V1: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexGenerationPublicationModeV1 {
  Soft,
  Hard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexGenerationBarrierStageV1 {
  ImmutableClosure,
  Pointer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexGenerationPublicationFailureBoundaryV1 {
  PriorAuthorityRetained,
  PointerCommitUnknown,
  SuccessorPointerVisible,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexGenerationPublicationLimitsV1 {
  maximum_dependencies: usize,
  maximum_total_bytes: usize,
}

impl IndexGenerationPublicationLimitsV1 {
  pub fn new(maximum_dependencies: usize, maximum_total_bytes: usize) -> FormatResult<Self> {
    if maximum_dependencies > INDEX_GENERATION_DEPENDENCY_HARD_CAP_V1 {
      return Err(amplification_error(format!(
        "dependency limit {maximum_dependencies} exceeds hard cap {INDEX_GENERATION_DEPENDENCY_HARD_CAP_V1}"
      )));
    }
    if maximum_total_bytes == 0 || maximum_total_bytes > INDEX_GENERATION_TOTAL_BYTES_HARD_CAP_V1 {
      return Err(amplification_error(format!(
        "byte limit {maximum_total_bytes} is outside 1..={INDEX_GENERATION_TOTAL_BYTES_HARD_CAP_V1}"
      )));
    }
    Ok(Self { maximum_dependencies, maximum_total_bytes })
  }

  pub fn maximum_dependencies(self) -> usize {
    self.maximum_dependencies
  }

  pub fn maximum_total_bytes(self) -> usize {
    self.maximum_total_bytes
  }
}

#[derive(Clone, Copy, Debug)]
pub struct IndexGenerationPublicationRequestV1<'a> {
  pub mode: IndexGenerationPublicationModeV1,
  pub hash_algorithm: HashAlgorithm,
  pub dependencies: &'a [&'a EncodedImmutableIndexArtifactV1],
  pub manifest: &'a EncodedImmutableIndexArtifactV1,
  pub pointer: &'a EncodedActivePointerV1,
  pub rewrite_plan: ActivePointerRewritePlanV1<'a>,
  pub limits: IndexGenerationPublicationLimitsV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexGenerationPublicationActionV1<'a> {
  PublishDependency { ordinal: usize, artifact: &'a EncodedImmutableIndexArtifactV1 },
  PublishManifest { artifact: &'a EncodedImmutableIndexArtifactV1 },
  DurabilityBarrier { stage: IndexGenerationBarrierStageV1 },
  PublishPointer { pointer: &'a EncodedActivePointerV1 },
  ValidateSelectedClosure { manifest: &'a EncodedImmutableIndexArtifactV1, pointer: &'a EncodedActivePointerV1, pointer_sequence: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexGenerationPublicationStepReceiptV1<'a> {
  ImmutablePublished {
    artifact_key: &'a [u8],
    stored_length: usize,
  },
  DurabilityBarrierCompleted {
    stage: IndexGenerationBarrierStageV1,
    receipt: DurabilityCommitReceipt,
  },
  ActivePointerPublished {
    pointer_key: &'a [u8],
    stored_length: usize,
    pointer_sequence: u64,
    generation: u64,
    target_manifest_hash: &'a [u8],
  },
  SelectedClosureValidated {
    pointer_key: &'a [u8],
    manifest_key: &'a [u8],
    generation: u64,
    pointer_sequence: u64,
  },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexGenerationPublicationReceiptV1<'a> {
  Soft {
    dependency_count: usize,
    total_bytes: usize,
    manifest_key: &'a [u8],
    pointer_key: &'a [u8],
    pointer_sequence: u64,
  },
  Hard {
    dependency_count: usize,
    total_bytes: usize,
    manifest_key: &'a [u8],
    pointer_key: &'a [u8],
    pointer_sequence: u64,
    immutable_barrier_sequence: u64,
    pointer_barrier_sequence: u64,
  },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IndexGenerationPublicationStateV1 {
  Dependency(usize),
  Manifest,
  ImmutableBarrier,
  Pointer,
  PointerBarrier,
  Closure,
  Complete,
}

#[derive(Debug)]
pub struct IndexGenerationPublicationMachineV1<'a> {
  request: IndexGenerationPublicationRequestV1<'a>,
  state: IndexGenerationPublicationStateV1,
  pointer_sequence: u64,
  generation: u64,
  total_bytes: usize,
  immutable_barrier_sequence: Option<u64>,
  pointer_barrier_sequence: Option<u64>,
}

impl<'a> IndexGenerationPublicationMachineV1<'a> {
  pub fn new(request: IndexGenerationPublicationRequestV1<'a>) -> FormatResult<Self> {
    if request.dependencies.len() > request.limits.maximum_dependencies() {
      return Err(amplification_error(format!(
        "{} dependencies exceed configured limit {}",
        request.dependencies.len(),
        request.limits.maximum_dependencies()
      )));
    }
    let total_bytes = checked_publication_bytes(&request)?;
    validate_publication_closure(&request)?;
    let pointer = decode_active_pointer(&request.pointer.value, request.hash_algorithm)?;
    let state = if request.dependencies.is_empty() {
      IndexGenerationPublicationStateV1::Manifest
    } else {
      IndexGenerationPublicationStateV1::Dependency(0)
    };
    Ok(Self {
      request,
      state,
      pointer_sequence: pointer.sequence,
      generation: pointer.generation,
      total_bytes,
      immutable_barrier_sequence: None,
      pointer_barrier_sequence: None,
    })
  }

  pub fn next_action(&self) -> Option<IndexGenerationPublicationActionV1<'a>> {
    match self.state {
      IndexGenerationPublicationStateV1::Dependency(ordinal) => {
        Some(IndexGenerationPublicationActionV1::PublishDependency { ordinal, artifact: self.request.dependencies[ordinal] })
      }
      IndexGenerationPublicationStateV1::Manifest => {
        Some(IndexGenerationPublicationActionV1::PublishManifest { artifact: self.request.manifest })
      }
      IndexGenerationPublicationStateV1::ImmutableBarrier => {
        Some(IndexGenerationPublicationActionV1::DurabilityBarrier { stage: IndexGenerationBarrierStageV1::ImmutableClosure })
      }
      IndexGenerationPublicationStateV1::Pointer => {
        Some(IndexGenerationPublicationActionV1::PublishPointer { pointer: self.request.pointer })
      }
      IndexGenerationPublicationStateV1::PointerBarrier => {
        Some(IndexGenerationPublicationActionV1::DurabilityBarrier { stage: IndexGenerationBarrierStageV1::Pointer })
      }
      IndexGenerationPublicationStateV1::Closure => Some(IndexGenerationPublicationActionV1::ValidateSelectedClosure {
        manifest: self.request.manifest,
        pointer: self.request.pointer,
        pointer_sequence: self.pointer_sequence,
      }),
      IndexGenerationPublicationStateV1::Complete => None,
    }
  }

  pub fn failure_boundary(&self) -> IndexGenerationPublicationFailureBoundaryV1 {
    match self.state {
      IndexGenerationPublicationStateV1::Dependency(_)
      | IndexGenerationPublicationStateV1::Manifest
      | IndexGenerationPublicationStateV1::ImmutableBarrier => IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained,
      IndexGenerationPublicationStateV1::Pointer => IndexGenerationPublicationFailureBoundaryV1::PointerCommitUnknown,
      IndexGenerationPublicationStateV1::PointerBarrier
      | IndexGenerationPublicationStateV1::Closure
      | IndexGenerationPublicationStateV1::Complete => IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible,
    }
  }

  pub fn acknowledge<'receipt>(
    &mut self,
    receipt: IndexGenerationPublicationStepReceiptV1<'receipt>,
  ) -> FormatResult<Option<IndexGenerationPublicationReceiptV1<'a>>> {
    match self.state {
      IndexGenerationPublicationStateV1::Dependency(ordinal) => {
        validate_immutable_receipt(receipt, self.request.dependencies[ordinal])?;
        self.state = if ordinal + 1 == self.request.dependencies.len() {
          IndexGenerationPublicationStateV1::Manifest
        } else {
          IndexGenerationPublicationStateV1::Dependency(ordinal + 1)
        };
      }
      IndexGenerationPublicationStateV1::Manifest => {
        validate_immutable_receipt(receipt, self.request.manifest)?;
        self.state = match self.request.mode {
          IndexGenerationPublicationModeV1::Soft => IndexGenerationPublicationStateV1::Pointer,
          IndexGenerationPublicationModeV1::Hard => IndexGenerationPublicationStateV1::ImmutableBarrier,
        };
      }
      IndexGenerationPublicationStateV1::ImmutableBarrier => {
        let sequence = validate_barrier_receipt(receipt, IndexGenerationBarrierStageV1::ImmutableClosure, None)?;
        self.immutable_barrier_sequence = Some(sequence);
        self.state = IndexGenerationPublicationStateV1::Pointer;
      }
      IndexGenerationPublicationStateV1::Pointer => {
        validate_pointer_receipt(receipt, self.request.pointer, self.pointer_sequence, self.generation, &self.request.manifest.key)?;
        self.state = match self.request.mode {
          IndexGenerationPublicationModeV1::Soft => IndexGenerationPublicationStateV1::Closure,
          IndexGenerationPublicationModeV1::Hard => IndexGenerationPublicationStateV1::PointerBarrier,
        };
      }
      IndexGenerationPublicationStateV1::PointerBarrier => {
        let first = self.immutable_barrier_sequence.ok_or_else(|| order_error("pointer barrier has no immutable barrier predecessor"))?;
        let sequence = validate_barrier_receipt(receipt, IndexGenerationBarrierStageV1::Pointer, Some(first))?;
        self.pointer_barrier_sequence = Some(sequence);
        self.state = IndexGenerationPublicationStateV1::Closure;
      }
      IndexGenerationPublicationStateV1::Closure => {
        validate_closure_receipt(receipt, self.request.manifest, self.request.pointer, self.generation, self.pointer_sequence)?;
        self.state = IndexGenerationPublicationStateV1::Complete;
        return Ok(Some(self.complete_receipt()?));
      }
      IndexGenerationPublicationStateV1::Complete => {
        return Err(order_error("publication is already complete"));
      }
    }
    Ok(None)
  }

  fn complete_receipt(&self) -> FormatResult<IndexGenerationPublicationReceiptV1<'a>> {
    let common = (
      self.request.dependencies.len(),
      self.total_bytes,
      self.request.manifest.key.as_slice(),
      self.request.pointer.key.as_slice(),
      self.pointer_sequence,
    );
    match self.request.mode {
      IndexGenerationPublicationModeV1::Soft => Ok(IndexGenerationPublicationReceiptV1::Soft {
        dependency_count: common.0,
        total_bytes: common.1,
        manifest_key: common.2,
        pointer_key: common.3,
        pointer_sequence: common.4,
      }),
      IndexGenerationPublicationModeV1::Hard => Ok(IndexGenerationPublicationReceiptV1::Hard {
        dependency_count: common.0,
        total_bytes: common.1,
        manifest_key: common.2,
        pointer_key: common.3,
        pointer_sequence: common.4,
        immutable_barrier_sequence: self
          .immutable_barrier_sequence
          .ok_or_else(|| order_error("hard publication completed without its immutable barrier"))?,
        pointer_barrier_sequence: self
          .pointer_barrier_sequence
          .ok_or_else(|| order_error("hard publication completed without its pointer barrier"))?,
      }),
    }
  }
}

fn checked_publication_bytes(request: &IndexGenerationPublicationRequestV1<'_>) -> FormatResult<usize> {
  let mut total = request
    .manifest
    .value
    .len()
    .checked_add(request.pointer.value.len())
    .ok_or_else(|| overflow_error("manifest plus pointer byte length overflowed"))?;
  for dependency in request.dependencies {
    total = total.checked_add(dependency.value.len()).ok_or_else(|| overflow_error("dependency byte length overflowed"))?;
  }
  if total > request.limits.maximum_total_bytes() {
    return Err(amplification_error(format!(
      "publication has {total} bytes, exceeding configured limit {}",
      request.limits.maximum_total_bytes()
    )));
  }
  Ok(total)
}

fn validate_publication_closure(request: &IndexGenerationPublicationRequestV1<'_>) -> FormatResult<()> {
  let manifest = decode_index_manifest(&request.manifest.value, request.hash_algorithm)?;
  if manifest.key != request.manifest.key {
    return Err(identity_error("prepared manifest key disagrees with its encoded bytes"));
  }
  let pointer = decode_active_pointer(&request.pointer.value, request.hash_algorithm)?;
  if pointer.key != request.pointer.key {
    return Err(identity_error("prepared pointer key disagrees with its encoded bytes"));
  }
  if target_manifest_kind(pointer.kind) != manifest.kind
    || pointer.owner_id != manifest.owner_id
    || pointer.generation != manifest.generation
    || pointer.target_manifest_hash != request.manifest.key
  {
    return Err(closure_error("active pointer does not select the supplied manifest kind, owner, generation, and key"));
  }
  if pointer.kind != request.rewrite_plan.expected_kind()
    || pointer.owner_id != request.rewrite_plan.expected_owner_id()
    || pointer.slot != request.rewrite_plan.write_slot()
    || pointer.sequence != request.rewrite_plan.next_sequence()
  {
    return Err(closure_error("active pointer kind, owner, slot, or sequence disagrees with the deterministic rewrite plan"));
  }

  let mut unique = HashSet::new();
  unique.try_reserve(request.dependencies.len() + 1).map_err(|source| {
    amplification_error(format!("publication identity-set allocation failed for {} entries: {source}", request.dependencies.len() + 1))
  })?;
  unique.insert(request.manifest.key.as_slice());
  for dependency in request.dependencies {
    validate_prepared_immutable(dependency, request.hash_algorithm)?;
    if !unique.insert(dependency.key.as_slice()) {
      return Err(order_error("publication contains a duplicate immutable artifact key"));
    }
  }
  if unique.contains(request.pointer.key.as_slice()) {
    return Err(order_error("active pointer key collides with an immutable publication key"));
  }
  Ok(())
}

fn validate_prepared_immutable(artifact: &EncodedImmutableIndexArtifactV1, algorithm: HashAlgorithm) -> FormatResult<()> {
  let decoded = decode_immutable_index_artifact(
    &artifact.value,
    algorithm,
    ImmutableIndexArtifactKindV1::MutationJournalSegment.maximum_encoded_length(),
  )?;
  let kind = ImmutableIndexArtifactKindV1::from_u16(decoded.kind)
    .ok_or_else(|| closure_error("dependency is not a registered immutable IndexArtifact kind"))?;
  if artifact.value.len() > kind.maximum_encoded_length() {
    return Err(amplification_error(format!(
      "{kind:?} dependency has {} bytes, exceeding its {}-byte kind cap",
      artifact.value.len(),
      kind.maximum_encoded_length()
    )));
  }
  if decoded.key != artifact.key {
    return Err(identity_error("prepared dependency key disagrees with its encoded bytes"));
  }
  Ok(())
}

fn validate_immutable_receipt(
  receipt: IndexGenerationPublicationStepReceiptV1<'_>,
  expected: &EncodedImmutableIndexArtifactV1,
) -> FormatResult<()> {
  let IndexGenerationPublicationStepReceiptV1::ImmutablePublished { artifact_key, stored_length } = receipt else {
    return Err(closure_error("publication step receipt is not an immutable-artifact receipt"));
  };
  if artifact_key != expected.key || stored_length != expected.value.len() {
    return Err(closure_error("immutable publication receipt does not bind the expected key and stored length"));
  }
  Ok(())
}

fn validate_pointer_receipt(
  receipt: IndexGenerationPublicationStepReceiptV1<'_>,
  expected: &EncodedActivePointerV1,
  expected_sequence: u64,
  expected_generation: u64,
  expected_target_manifest_hash: &[u8],
) -> FormatResult<()> {
  let IndexGenerationPublicationStepReceiptV1::ActivePointerPublished {
    pointer_key,
    stored_length,
    pointer_sequence,
    generation,
    target_manifest_hash,
  } = receipt
  else {
    return Err(closure_error("publication step receipt is not an active-pointer receipt"));
  };
  if pointer_key != expected.key
    || stored_length != expected.value.len()
    || pointer_sequence != expected_sequence
    || generation != expected_generation
    || target_manifest_hash != expected_target_manifest_hash
  {
    return Err(closure_error(
      "active-pointer publication receipt does not bind the expected key, length, sequence, generation, and target",
    ));
  }
  Ok(())
}

fn validate_barrier_receipt(
  receipt: IndexGenerationPublicationStepReceiptV1<'_>,
  expected_stage: IndexGenerationBarrierStageV1,
  predecessor_sequence: Option<u64>,
) -> FormatResult<u64> {
  let IndexGenerationPublicationStepReceiptV1::DurabilityBarrierCompleted { stage, receipt } = receipt else {
    return Err(closure_error("publication step receipt is not a durability-barrier receipt"));
  };
  if stage != expected_stage
    || receipt.class != CommitClass::HardAuthority
    || receipt.sequence == 0
    || receipt.hard_frontier < receipt.sequence
  {
    return Err(closure_error("durability receipt does not prove the expected hard-authority barrier"));
  }
  if predecessor_sequence.is_some_and(|predecessor| receipt.sequence <= predecessor) {
    return Err(order_error("pointer barrier sequence does not follow the immutable barrier sequence"));
  }
  Ok(receipt.sequence)
}

fn validate_closure_receipt(
  receipt: IndexGenerationPublicationStepReceiptV1<'_>,
  manifest: &EncodedImmutableIndexArtifactV1,
  pointer: &EncodedActivePointerV1,
  expected_generation: u64,
  expected_pointer_sequence: u64,
) -> FormatResult<()> {
  let IndexGenerationPublicationStepReceiptV1::SelectedClosureValidated { pointer_key, manifest_key, generation, pointer_sequence } =
    receipt
  else {
    return Err(closure_error("publication step receipt is not a selected-closure receipt"));
  };
  if pointer_key != pointer.key
    || manifest_key != manifest.key
    || generation != expected_generation
    || pointer_sequence != expected_pointer_sequence
  {
    return Err(closure_error("selected-closure receipt does not bind the expected pointer, manifest, generation, and sequence"));
  }
  Ok(())
}

fn target_manifest_kind(kind: ActivePointerKindV1) -> IndexManifestKindV1 {
  match kind {
    ActivePointerKindV1::FieldIndex => IndexManifestKindV1::FieldIndex,
    ActivePointerKindV1::FieldNvt => IndexManifestKindV1::FieldNvt,
    ActivePointerKindV1::ScopeCatalog => IndexManifestKindV1::ScopeCatalog,
  }
}

fn amplification_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::AllocationAmplification, "index_generation_publication_bounds", context)
}

fn overflow_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::LengthCountOrArithmeticOverflow, "index_generation_publication_overflow", context)
}

fn identity_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::IdentityKeyOrGenerationMismatch, "index_generation_publication_identity", context)
}

fn order_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::NoncanonicalOrderOrDuplicate, "index_generation_publication_order", context)
}

fn closure_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::CrossRecordClosureMismatch, "index_generation_publication_closure", context)
}
