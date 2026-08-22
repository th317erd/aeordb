//! Crash-resumable first-authority publication of one frozen index application.

use std::collections::HashMap;
use std::error::Error;
use std::fmt::{self, Display, Formatter};

use crate::engine::HashAlgorithm;

use super::first_authority::{
  FirstAuthorityPublicationErrorV1, IndexActivePointerPublicationErrorV1, IndexActivePointerPublicationRequestV1,
  IndexArtifactBatchPublicationRequestV1, LoadedIndexActivePointerPairV1, V4FirstAuthorityPublisher,
};
use super::gc_retirement::RetirementJournalOwnerV1;
use super::index_artifact::{
  ActivePointerKindV1, ActivePointerSlotObservationV1, ActivePointerV1, ActivePointerWriteV1, EncodedActivePointerV1,
  EncodedImmutableIndexArtifactV1, IndexManifestBodyV1, IndexManifestKindV1, decode_active_pointer, decode_index_manifest,
  encode_active_pointer, plan_active_pointer_rewrite,
};
use super::index_batch_application::{FrozenIndexBatchApplicationPlanV1, FrozenIndexOwnerApplicationPlanV1};
use super::index_coordinator::IndexMembershipOwnerClassV1;
use super::index_generation_publication::{
  IndexGenerationBarrierStageV1, IndexGenerationPublicationFailureBoundaryV1, IndexGenerationPublicationLimitsV1,
  IndexGenerationPublicationMachineV1, IndexGenerationPublicationModeV1, IndexGenerationPublicationReceiptV1,
  IndexGenerationPublicationRequestV1, IndexGenerationPublicationStepReceiptV1,
};
use super::reader::{FormatError, MalformedInputClass};

#[derive(Clone, Copy, Debug)]
pub struct FrozenIndexGenerationPublicationRequestV1<'a> {
  pub database_id: &'a [u8; 16],
  pub hash_algorithm: HashAlgorithm,
  pub plan: &'a FrozenIndexBatchApplicationPlanV1,
  pub mode: IndexGenerationPublicationModeV1,
  pub limits: IndexGenerationPublicationLimitsV1,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenIndexPointerPublicationReceiptV1 {
  pub kind: ActivePointerKindV1,
  pub owner_id: Vec<u8>,
  pub manifest_key: Vec<u8>,
  pub pointer_key: Vec<u8>,
  pub pointer_bytes: Vec<u8>,
  pub pointer_sequence: u64,
  pub dependency_count: usize,
  pub total_bytes: usize,
  pub immutable_barrier_sequence: Option<u64>,
  pub pointer_barrier_sequence: Option<u64>,
  pub idempotent: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenIndexGenerationPublicationReceiptV1 {
  pub coordinator_id: [u8; 16],
  pub batch_id: u64,
  pub attempt_id: u64,
  pub generation: u64,
  pub artifact_count: usize,
  pub manifest_count: usize,
  pub idempotent_artifact_count: usize,
  pub idempotent_manifest_count: usize,
  pub immutable_barrier_sequence: Option<u64>,
  pub pointer_barrier_sequence: Option<u64>,
  pub pointer_receipts: Vec<FrozenIndexPointerPublicationReceiptV1>,
}

#[derive(Debug)]
pub enum FrozenIndexGenerationPublicationErrorV1 {
  Cancelled { boundary: IndexGenerationPublicationFailureBoundaryV1 },
  InvalidPlan { code: &'static str, message: String, boundary: IndexGenerationPublicationFailureBoundaryV1 },
  Format { source: FormatError, boundary: IndexGenerationPublicationFailureBoundaryV1 },
  Authority { source: FirstAuthorityPublicationErrorV1, boundary: IndexGenerationPublicationFailureBoundaryV1 },
  ActivePointer { source: IndexActivePointerPublicationErrorV1, boundary: IndexGenerationPublicationFailureBoundaryV1 },
}

impl FrozenIndexGenerationPublicationErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Cancelled { .. } => "index_generation_cancelled",
      Self::InvalidPlan { code, .. } => code,
      Self::Format { source, .. } => source.code(),
      Self::Authority { source, .. } => source.code(),
      Self::ActivePointer { source, .. } => source.code(),
    }
  }

  pub const fn failure_boundary(&self) -> IndexGenerationPublicationFailureBoundaryV1 {
    match self {
      Self::Cancelled { boundary }
      | Self::InvalidPlan { boundary, .. }
      | Self::Format { boundary, .. }
      | Self::Authority { boundary, .. }
      | Self::ActivePointer { boundary, .. } => *boundary,
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>, boundary: IndexGenerationPublicationFailureBoundaryV1) -> Self {
    Self::InvalidPlan { code, message: message.into(), boundary }
  }

  fn format(source: FormatError, boundary: IndexGenerationPublicationFailureBoundaryV1) -> Self {
    Self::Format { source, boundary }
  }

  fn authority(source: FirstAuthorityPublicationErrorV1, boundary: IndexGenerationPublicationFailureBoundaryV1) -> Self {
    Self::Authority { source, boundary }
  }

  fn at_boundary(self, boundary: IndexGenerationPublicationFailureBoundaryV1) -> Self {
    match self {
      Self::Cancelled { .. } => Self::Cancelled { boundary },
      Self::InvalidPlan { code, message, .. } => Self::InvalidPlan { code, message, boundary },
      Self::Format { source, .. } => Self::Format { source, boundary },
      Self::Authority { source, .. } => Self::Authority { source, boundary },
      Self::ActivePointer { source, .. } => Self::ActivePointer { source, boundary },
    }
  }
}

impl Display for FrozenIndexGenerationPublicationErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Cancelled { boundary } => write!(formatter, "index generation publication was cancelled at {boundary:?}"),
      Self::InvalidPlan { code, message, boundary } => write!(formatter, "{code} at {boundary:?}: {message}"),
      Self::Format { source, boundary } => write!(formatter, "index generation format failure at {boundary:?}: {source}"),
      Self::Authority { source, boundary } => write!(formatter, "index generation first-authority failure at {boundary:?}: {source}"),
      Self::ActivePointer { source, boundary } => write!(formatter, "index generation active-pointer failure at {boundary:?}: {source}"),
    }
  }
}

impl Error for FrozenIndexGenerationPublicationErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Format { source, .. } => Some(source),
      Self::Authority { source, .. } => Some(source),
      Self::ActivePointer { source, .. } => Some(source),
      Self::Cancelled { .. } | Self::InvalidPlan { .. } => None,
    }
  }
}

#[derive(Clone, Debug)]
struct OwnerManifestInfoV1 {
  owner_class: IndexMembershipOwnerClassV1,
  owner_id: Vec<u8>,
  parent_manifest_key: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
struct PreparedPointerOwnerV1 {
  owner_plan_index: usize,
  kind: ActivePointerKindV1,
  pointer: EncodedActivePointerV1,
  prior_pair: LoadedIndexActivePointerPairV1,
  dependency_artifact_indices: Vec<usize>,
  dependency_manifest_indices: Vec<usize>,
}

struct OwnerMachineContextV1<'plan, 'borrow> {
  hash_algorithm: HashAlgorithm,
  mode: IndexGenerationPublicationModeV1,
  limits: IndexGenerationPublicationLimitsV1,
  plan: &'plan FrozenIndexBatchApplicationPlanV1,
  artifacts: &'borrow [&'plan EncodedImmutableIndexArtifactV1],
  prepared: &'borrow PreparedPointerOwnerV1,
  failure_boundary: IndexGenerationPublicationFailureBoundaryV1,
}

pub fn publish_frozen_index_application_v1(
  publisher: &V4FirstAuthorityPublisher,
  retirement_owner: &mut RetirementJournalOwnerV1,
  request: FrozenIndexGenerationPublicationRequestV1<'_>,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<FrozenIndexGenerationPublicationReceiptV1, FrozenIndexGenerationPublicationErrorV1> {
  check_cancelled(is_cancelled, IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)?;
  let prepared_artifacts = request.plan.prepared_artifacts();
  let mut artifacts = Vec::new();
  artifacts.try_reserve_exact(prepared_artifacts.len()).map_err(|source| {
    allocation_error(
      "frozen artifact reference reservation",
      source.to_string(),
      IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained,
    )
  })?;
  artifacts.extend(prepared_artifacts);
  let (manifest_infos, prepared_pointers) = preflight_application(publisher, &request, &artifacts, is_cancelled)?;
  check_cancelled(is_cancelled, IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)?;

  let artifact_publication = if artifacts.is_empty() {
    None
  } else {
    Some(
      publisher
        .publish_index_artifacts(IndexArtifactBatchPublicationRequestV1 {
          database_id: request.database_id,
          artifacts: &artifacts,
          publication_timestamp_ms: request.publication_timestamp_ms,
        })
        .map_err(|source| {
          FrozenIndexGenerationPublicationErrorV1::authority(source, IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)
        })?,
    )
  };
  check_cancelled(is_cancelled, IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)?;

  let mut manifests = Vec::new();
  manifests.try_reserve_exact(request.plan.owner_plans().len()).map_err(|source| {
    allocation_error(
      "successor manifest reference reservation",
      source.to_string(),
      IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained,
    )
  })?;
  manifests.extend(request.plan.owner_plans().iter().map(FrozenIndexOwnerApplicationPlanV1::successor_manifest));
  let manifest_publication = publisher
    .publish_index_artifacts(IndexArtifactBatchPublicationRequestV1 {
      database_id: request.database_id,
      artifacts: &manifests,
      publication_timestamp_ms: request.publication_timestamp_ms,
    })
    .map_err(|source| {
      FrozenIndexGenerationPublicationErrorV1::authority(source, IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)
    })?;
  validate_immutable_batch_receipts(artifact_publication.as_ref().map(|receipt| receipt.artifacts.as_slice()), &artifacts)?;
  validate_immutable_batch_receipts(Some(&manifest_publication.artifacts), &manifests)?;
  check_cancelled(is_cancelled, IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)?;

  let immutable_barrier = if request.mode == IndexGenerationPublicationModeV1::Hard {
    let receipt = publisher.publish_index_hard_barrier(request.database_id, request.publication_timestamp_ms).map_err(|source| {
      FrozenIndexGenerationPublicationErrorV1::authority(source, IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)
    })?;
    check_cancelled(is_cancelled, IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)?;
    Some(receipt.durability)
  } else {
    None
  };

  let mut physical_pointer_receipts = Vec::new();
  physical_pointer_receipts.try_reserve_exact(prepared_pointers.len()).map_err(|source| {
    allocation_error(
      "physical active-pointer receipt reservation",
      source.to_string(),
      IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained,
    )
  })?;
  for prepared in &prepared_pointers {
    let boundary = if physical_pointer_receipts.is_empty() {
      IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained
    } else {
      IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible
    };
    check_cancelled(is_cancelled, boundary)?;
    let receipt = publisher
      .publish_index_active_pointer(
        IndexActivePointerPublicationRequestV1 {
          database_id: request.database_id,
          pointer: &prepared.pointer,
          publication_timestamp_ms: request.publication_timestamp_ms,
          monotonic_now_ms: request.monotonic_now_ms,
        },
        retirement_owner,
      )
      .map_err(|source| {
        let boundary = if !physical_pointer_receipts.is_empty() || source.committed_receipt().is_some() {
          IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible
        } else {
          IndexGenerationPublicationFailureBoundaryV1::PointerCommitUnknown
        };
        FrozenIndexGenerationPublicationErrorV1::ActivePointer { source, boundary }
      })?;
    physical_pointer_receipts.push(receipt);
    check_cancelled(is_cancelled, IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible)?;
  }

  let pointer_barrier = if request.mode == IndexGenerationPublicationModeV1::Hard {
    let receipt = publisher.publish_index_hard_barrier(request.database_id, request.publication_timestamp_ms).map_err(|source| {
      FrozenIndexGenerationPublicationErrorV1::authority(source, IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible)
    })?;
    check_cancelled(is_cancelled, IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible)?;
    Some(receipt.durability)
  } else {
    None
  };

  let mut pointer_receipts = Vec::new();
  pointer_receipts.try_reserve_exact(prepared_pointers.len()).map_err(|source| {
    allocation_error(
      "validated active-pointer receipt reservation",
      source.to_string(),
      IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible,
    )
  })?;
  for (prepared, physical) in prepared_pointers.iter().zip(&physical_pointer_receipts) {
    check_cancelled(is_cancelled, IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible)?;
    let selected = publisher
      .load_index_active_pointer_pair(request.database_id, prepared.kind, &manifest_infos[prepared.owner_plan_index].owner_id)
      .map_err(|source| {
        FrozenIndexGenerationPublicationErrorV1::authority(source, IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible)
      })?
      .selected
      .ok_or_else(|| {
        FrozenIndexGenerationPublicationErrorV1::invalid(
          "index_generation_selected_pointer_missing",
          "published active-pointer owner has no closure-valid selected member",
          IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible,
        )
      })?;
    if selected.bytes != prepared.pointer.value {
      return Err(FrozenIndexGenerationPublicationErrorV1::invalid(
        "index_generation_selected_pointer_mismatch",
        "selected active pointer does not byte-match the prepared successor",
        IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible,
      ));
    }
    pointer_receipts.push(validate_completed_owner_publication(
      request.hash_algorithm,
      request.mode,
      request.limits,
      request.plan,
      &artifacts,
      prepared,
      physical,
      immutable_barrier.as_ref(),
      pointer_barrier.as_ref(),
      &selected,
    )?);
  }

  Ok(FrozenIndexGenerationPublicationReceiptV1 {
    coordinator_id: request.plan.coordinator_id(),
    batch_id: request.plan.batch_id(),
    attempt_id: request.plan.attempt_id(),
    generation: request.plan.generation(),
    artifact_count: artifacts.len(),
    manifest_count: manifests.len(),
    idempotent_artifact_count: artifact_publication
      .as_ref()
      .map_or(0, |publication| publication.artifacts.iter().filter(|receipt| receipt.idempotent).count()),
    idempotent_manifest_count: manifest_publication.artifacts.iter().filter(|receipt| receipt.idempotent).count(),
    immutable_barrier_sequence: immutable_barrier.as_ref().map(|receipt| receipt.sequence),
    pointer_barrier_sequence: pointer_barrier.as_ref().map(|receipt| receipt.sequence),
    pointer_receipts,
  })
}

fn preflight_application(
  publisher: &V4FirstAuthorityPublisher,
  request: &FrozenIndexGenerationPublicationRequestV1<'_>,
  artifacts: &[&EncodedImmutableIndexArtifactV1],
  is_cancelled: &dyn Fn() -> bool,
) -> Result<(Vec<OwnerManifestInfoV1>, Vec<PreparedPointerOwnerV1>), FrozenIndexGenerationPublicationErrorV1> {
  if request.publication_timestamp_ms == 0 || request.monotonic_now_ms == 0 || request.plan.owner_plans().is_empty() {
    return Err(FrozenIndexGenerationPublicationErrorV1::invalid(
      "index_generation_publication_request",
      "generation publication requires nonzero timestamps and at least one owner plan",
      IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained,
    ));
  }
  let observation = publisher.observe().map_err(|source| {
    FrozenIndexGenerationPublicationErrorV1::authority(source, IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)
  })?;
  if observation.selected.header.database_id != *request.database_id || observation.selected.header.hash_algorithm != request.hash_algorithm
  {
    return Err(FrozenIndexGenerationPublicationErrorV1::invalid(
      "index_generation_database_identity",
      "generation application and selected first authority belong to different database identities or hash profiles",
      IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained,
    ));
  }
  if request.plan.coordinator_id() == [0; 16]
    || request.plan.batch_id() == 0
    || request.plan.attempt_id() == 0
    || request.plan.generation() == 0
  {
    return Err(FrozenIndexGenerationPublicationErrorV1::invalid(
      "index_generation_application_identity",
      "frozen application coordinator, batch, attempt, or generation identity is zero",
      IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained,
    ));
  }

  let mut manifest_infos = Vec::new();
  manifest_infos.try_reserve_exact(request.plan.owner_plans().len()).map_err(|source| {
    allocation_error("owner manifest preflight", source.to_string(), IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)
  })?;
  let mut manifest_indices = HashMap::new();
  manifest_indices.try_reserve(request.plan.owner_plans().len()).map_err(|source| {
    allocation_error("manifest identity map", source.to_string(), IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)
  })?;
  let mut artifact_owners = Vec::new();
  artifact_owners.try_reserve_exact(artifacts.len()).map_err(|source| {
    allocation_error("artifact owner map", source.to_string(), IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)
  })?;
  artifact_owners.resize(artifacts.len(), None);
  let mut previous_rank = 0u8;
  for (owner_index, owner) in request.plan.owner_plans().iter().enumerate() {
    check_cancelled(is_cancelled, IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)?;
    let manifest = decode_index_manifest(&owner.successor_manifest().value, request.hash_algorithm).map_err(|source| {
      FrozenIndexGenerationPublicationErrorV1::format(source, IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)
    })?;
    let rank = owner_class_rank(owner.owner_class());
    if owner_index > 0 && rank < previous_rank {
      return Err(plan_error("index generation owner plans are not in parent-before-child order"));
    }
    previous_rank = rank;
    if manifest.key != owner.successor_manifest().key
      || manifest.owner_id != owner.owner_id()
      || manifest.generation != request.plan.generation()
      || manifest_owner_class(manifest.kind) != Some(owner.owner_class())
    {
      return Err(plan_error("successor manifest identity, class, or generation disagrees with its owner plan"));
    }
    let manifest_key =
      clone_bytes(&manifest.key, "manifest identity", IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)?;
    if manifest_indices.insert(manifest_key, owner_index).is_some() {
      return Err(plan_error("two owner plans produce one successor manifest key"));
    }
    let dependency_range = owner.dependency_range();
    if dependency_range.start > dependency_range.end || dependency_range.end > artifacts.len() {
      return Err(plan_error("owner dependency range falls outside the frozen artifact overlay"));
    }
    for artifact_owner in &mut artifact_owners[dependency_range] {
      if artifact_owner.replace(owner_index).is_some() {
        return Err(plan_error("two owner plans claim one frozen artifact dependency"));
      }
    }
    manifest_infos.push(OwnerManifestInfoV1 {
      owner_class: owner.owner_class(),
      owner_id: clone_bytes(
        owner.owner_id(),
        "manifest owner identity",
        IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained,
      )?,
      parent_manifest_key: manifest_parent_key(&manifest.details)
        .map(|parent| clone_bytes(parent, "parent manifest identity", IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained))
        .transpose()?,
    });
  }
  if artifact_owners.iter().any(Option::is_none) {
    return Err(plan_error("frozen artifact overlay contains a dependency outside every owner plan"));
  }

  let mut value_referenced = Vec::new();
  value_referenced.try_reserve_exact(manifest_infos.len()).map_err(|source| {
    allocation_error("ValueStore reference map", source.to_string(), IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)
  })?;
  value_referenced.resize(manifest_infos.len(), false);
  let mut prepared = Vec::new();
  prepared.try_reserve_exact(manifest_infos.len()).map_err(|source| {
    allocation_error("active-pointer preflight", source.to_string(), IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)
  })?;
  for (owner_index, info) in manifest_infos.iter().enumerate() {
    let kind = match info.owner_class {
      IndexMembershipOwnerClassV1::ScopeCatalog => ActivePointerKindV1::ScopeCatalog,
      IndexMembershipOwnerClassV1::FieldIndex => ActivePointerKindV1::FieldIndex,
      IndexMembershipOwnerClassV1::ValueStore => continue,
    };
    let chain = owner_manifest_chain(owner_index, &manifest_infos, &manifest_indices, &mut value_referenced)?;
    let dependency_artifact_count = chain
      .iter()
      .try_fold(0usize, |count, chain_index| count.checked_add(request.plan.owner_plans()[*chain_index].dependency_range().len()));
    let dependency_artifact_count = dependency_artifact_count.ok_or_else(|| plan_error("owner dependency artifact count overflowed"))?;
    let dependency_manifest_count = chain.len().saturating_sub(1);
    let mut dependency_artifact_indices = Vec::new();
    dependency_artifact_indices.try_reserve_exact(dependency_artifact_count).map_err(|source| {
      allocation_error(
        "owner dependency artifact indices",
        source.to_string(),
        IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained,
      )
    })?;
    let mut dependency_manifest_indices = Vec::new();
    dependency_manifest_indices.try_reserve_exact(dependency_manifest_count).map_err(|source| {
      allocation_error(
        "owner dependency manifest indices",
        source.to_string(),
        IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained,
      )
    })?;
    for chain_index in &chain {
      dependency_artifact_indices.extend(request.plan.owner_plans()[*chain_index].dependency_range());
    }
    dependency_manifest_indices.extend(chain.iter().copied().filter(|chain_index| *chain_index != owner_index));
    let pair = publisher.load_index_active_pointer_pair(request.database_id, kind, &info.owner_id).map_err(|source| {
      FrozenIndexGenerationPublicationErrorV1::authority(source, IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)
    })?;
    let pointer = prepare_pointer(
      request.hash_algorithm,
      request.plan.generation(),
      kind,
      &info.owner_id,
      &pair,
      owner_manifest(request, owner_index),
    )?;
    let owner = PreparedPointerOwnerV1 {
      owner_plan_index: owner_index,
      kind,
      pointer,
      prior_pair: pair,
      dependency_artifact_indices,
      dependency_manifest_indices,
    };
    validate_owner_machine_request(request.hash_algorithm, request.mode, request.limits, request.plan, artifacts, &owner)?;
    prepared.push(owner);
  }
  for (index, info) in manifest_infos.iter().enumerate() {
    if info.owner_class == IndexMembershipOwnerClassV1::ValueStore && !value_referenced[index] {
      return Err(FrozenIndexGenerationPublicationErrorV1::invalid(
        "index_generation_unreferenced_value_store",
        format!("ValueStore successor {} is not selected through a same-batch FieldIndex", hex::encode(&info.owner_id)),
        IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained,
      ));
    }
  }
  if prepared.is_empty() {
    return Err(plan_error("frozen application has no pointer-bearing owner closure"));
  }
  Ok((manifest_infos, prepared))
}

fn owner_manifest_chain(
  owner_index: usize,
  infos: &[OwnerManifestInfoV1],
  manifest_indices: &HashMap<Vec<u8>, usize>,
  value_referenced: &mut [bool],
) -> Result<Vec<usize>, FrozenIndexGenerationPublicationErrorV1> {
  let mut reversed = Vec::new();
  reversed.try_reserve_exact(infos.len()).map_err(|source| {
    allocation_error("owner manifest chain", source.to_string(), IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)
  })?;
  reversed.push(owner_index);
  let mut current = owner_index;
  while let Some(parent_key) = infos[current].parent_manifest_key.as_ref() {
    let Some(parent_index) = manifest_indices.get(parent_key).copied() else {
      break;
    };
    let expected = match infos[current].owner_class {
      IndexMembershipOwnerClassV1::FieldIndex => IndexMembershipOwnerClassV1::ValueStore,
      IndexMembershipOwnerClassV1::ValueStore => IndexMembershipOwnerClassV1::ScopeCatalog,
      IndexMembershipOwnerClassV1::ScopeCatalog => return Err(plan_error("ScopeCatalog successor unexpectedly names a parent manifest")),
    };
    if infos[parent_index].owner_class != expected || parent_index >= current {
      return Err(plan_error("same-batch manifest parent has the wrong class or topological position"));
    }
    if expected == IndexMembershipOwnerClassV1::ValueStore {
      value_referenced[parent_index] = true;
    }
    if reversed.contains(&parent_index) {
      return Err(plan_error("same-batch manifest parent chain contains a cycle"));
    }
    reversed.push(parent_index);
    current = parent_index;
  }
  reversed.reverse();
  Ok(reversed)
}

fn prepare_pointer(
  hash_algorithm: HashAlgorithm,
  generation: u64,
  kind: ActivePointerKindV1,
  owner_id: &[u8],
  pair: &LoadedIndexActivePointerPairV1,
  manifest: &EncodedImmutableIndexArtifactV1,
) -> Result<EncodedActivePointerV1, FrozenIndexGenerationPublicationErrorV1> {
  if let Some(selected) = pair.selected.as_ref() {
    if selected.kind == kind
      && selected.owner_id == owner_id
      && selected.generation == generation
      && selected.target_manifest_hash == manifest.key
      && !pair.repair_required
    {
      let selected_pointer = decode_active_pointer(&selected.bytes, hash_algorithm).map_err(|source| {
        FrozenIndexGenerationPublicationErrorV1::format(source, IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)
      })?;
      let pointer = EncodedActivePointerV1 {
        key: clone_bytes(
          &selected_pointer.key,
          "selected active-pointer key",
          IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained,
        )?,
        value: clone_bytes(
          &selected.bytes,
          "selected active-pointer bytes",
          IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained,
        )?,
      };
      return Ok(pointer);
    }
  }
  let decoded_slots = decode_pointer_slots(pair, hash_algorithm)?;
  let observations = pointer_observations(pair, &decoded_slots, None);
  let rewrite = plan_active_pointer_rewrite(kind, owner_id, observations[0], observations[1]).map_err(|source| {
    FrozenIndexGenerationPublicationErrorV1::format(source, IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)
  })?;
  encode_active_pointer(&ActivePointerWriteV1 {
    kind,
    hash_algorithm,
    generation,
    owner_id,
    slot: rewrite.write_slot(),
    sequence: rewrite.next_sequence(),
    target_manifest_hash: &manifest.key,
  })
  .map_err(|source| {
    FrozenIndexGenerationPublicationErrorV1::format(source, IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)
  })
}

fn validate_owner_machine_request(
  hash_algorithm: HashAlgorithm,
  mode: IndexGenerationPublicationModeV1,
  limits: IndexGenerationPublicationLimitsV1,
  plan: &FrozenIndexBatchApplicationPlanV1,
  artifacts: &[&EncodedImmutableIndexArtifactV1],
  prepared: &PreparedPointerOwnerV1,
) -> Result<(), FrozenIndexGenerationPublicationErrorV1> {
  with_owner_machine(
    OwnerMachineContextV1 {
      hash_algorithm,
      mode,
      limits,
      plan,
      artifacts,
      prepared,
      failure_boundary: IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained,
    },
    |_machine, _dependencies| Ok(()),
  )
}

#[allow(clippy::too_many_arguments)]
fn validate_completed_owner_publication(
  hash_algorithm: HashAlgorithm,
  mode: IndexGenerationPublicationModeV1,
  limits: IndexGenerationPublicationLimitsV1,
  plan: &FrozenIndexBatchApplicationPlanV1,
  artifacts: &[&EncodedImmutableIndexArtifactV1],
  prepared: &PreparedPointerOwnerV1,
  physical: &super::first_authority::IndexActivePointerPublicationReceiptV1,
  immutable_barrier: Option<&crate::engine::durability_coordinator::DurabilityCommitReceipt>,
  pointer_barrier: Option<&crate::engine::durability_coordinator::DurabilityCommitReceipt>,
  selected: &super::first_authority::LoadedIndexActivePointerV1,
) -> Result<FrozenIndexPointerPublicationReceiptV1, FrozenIndexGenerationPublicationErrorV1> {
  with_owner_machine(
    OwnerMachineContextV1 {
      hash_algorithm,
      mode,
      limits,
      plan,
      artifacts,
      prepared,
      failure_boundary: IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible,
    },
    |machine, dependencies| {
      for dependency in dependencies {
        machine
          .acknowledge(IndexGenerationPublicationStepReceiptV1::ImmutablePublished {
            artifact_key: &dependency.key,
            stored_length: dependency.value.len(),
          })
          .map_err(successor_format_error)?;
      }
      let manifest = owner_manifest_from_plan(plan, prepared.owner_plan_index);
      machine
        .acknowledge(IndexGenerationPublicationStepReceiptV1::ImmutablePublished {
          artifact_key: &manifest.key,
          stored_length: manifest.value.len(),
        })
        .map_err(successor_format_error)?;
      if let Some(barrier) = immutable_barrier {
        machine
          .acknowledge(IndexGenerationPublicationStepReceiptV1::DurabilityBarrierCompleted {
            stage: IndexGenerationBarrierStageV1::ImmutableClosure,
            receipt: barrier.clone(),
          })
          .map_err(successor_format_error)?;
      }
      machine
        .acknowledge(IndexGenerationPublicationStepReceiptV1::ActivePointerPublished {
          pointer_key: &physical.pointer_key,
          stored_length: prepared.pointer.value.len(),
          pointer_sequence: physical.pointer_sequence,
          generation: physical.generation,
          target_manifest_hash: &physical.target_manifest_hash,
        })
        .map_err(successor_format_error)?;
      if let Some(barrier) = pointer_barrier {
        machine
          .acknowledge(IndexGenerationPublicationStepReceiptV1::DurabilityBarrierCompleted {
            stage: IndexGenerationBarrierStageV1::Pointer,
            receipt: barrier.clone(),
          })
          .map_err(successor_format_error)?;
      }
      let selected_pointer = decode_active_pointer(&selected.bytes, hash_algorithm).map_err(successor_format_error)?;
      let complete = machine
        .acknowledge(IndexGenerationPublicationStepReceiptV1::SelectedClosureValidated {
          pointer_key: &selected_pointer.key,
          manifest_key: &selected.target_manifest_hash,
          generation: selected.generation,
          pointer_sequence: selected.pointer_sequence,
        })
        .map_err(successor_format_error)?
        .ok_or_else(|| {
          successor_format_error(publication_format_error("index generation machine did not complete after selected-closure validation"))
        })?;
      let (dependency_count, total_bytes, immutable_barrier_sequence, pointer_barrier_sequence) = match complete {
        IndexGenerationPublicationReceiptV1::Soft { dependency_count, total_bytes, .. } => (dependency_count, total_bytes, None, None),
        IndexGenerationPublicationReceiptV1::Hard {
          dependency_count,
          total_bytes,
          immutable_barrier_sequence,
          pointer_barrier_sequence,
          ..
        } => (dependency_count, total_bytes, Some(immutable_barrier_sequence), Some(pointer_barrier_sequence)),
      };
      Ok(FrozenIndexPointerPublicationReceiptV1 {
        kind: prepared.kind,
        owner_id: clone_bytes(
          &selected.owner_id,
          "published pointer owner identity",
          IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible,
        )?,
        manifest_key: clone_bytes(
          &manifest.key,
          "published manifest identity",
          IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible,
        )?,
        pointer_key: clone_bytes(
          &physical.pointer_key,
          "published pointer identity",
          IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible,
        )?,
        pointer_bytes: clone_bytes(
          &selected.bytes,
          "published pointer bytes",
          IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible,
        )?,
        pointer_sequence: physical.pointer_sequence,
        dependency_count,
        total_bytes,
        immutable_barrier_sequence,
        pointer_barrier_sequence,
        idempotent: physical.idempotent,
      })
    },
  )
}

fn with_owner_machine<'plan, T>(
  context: OwnerMachineContextV1<'plan, '_>,
  operation: impl FnOnce(
    &mut IndexGenerationPublicationMachineV1<'_>,
    &[&EncodedImmutableIndexArtifactV1],
  ) -> Result<T, FrozenIndexGenerationPublicationErrorV1>,
) -> Result<T, FrozenIndexGenerationPublicationErrorV1> {
  let dependencies =
    owner_dependencies(context.plan, context.artifacts, context.prepared).map_err(|source| source.at_boundary(context.failure_boundary))?;
  let decoded_slots = decode_pointer_slots(&context.prepared.prior_pair, context.hash_algorithm)
    .map_err(|source| source.at_boundary(context.failure_boundary))?;
  let pointer = decode_active_pointer(&context.prepared.pointer.value, context.hash_algorithm)
    .map_err(|source| FrozenIndexGenerationPublicationErrorV1::format(source, context.failure_boundary))?;
  let ignored_slot = context
    .prepared
    .prior_pair
    .selected
    .as_ref()
    .filter(|selected| selected.bytes == context.prepared.pointer.value)
    .map(|selected| selected.selected_slot);
  let observations = pointer_observations(&context.prepared.prior_pair, &decoded_slots, ignored_slot);
  let rewrite = plan_active_pointer_rewrite(context.prepared.kind, pointer.owner_id, observations[0], observations[1])
    .map_err(|source| FrozenIndexGenerationPublicationErrorV1::format(source, context.failure_boundary))?;
  let manifest = owner_manifest_from_plan(context.plan, context.prepared.owner_plan_index);
  let mut machine = IndexGenerationPublicationMachineV1::new(IndexGenerationPublicationRequestV1 {
    mode: context.mode,
    hash_algorithm: context.hash_algorithm,
    dependencies: &dependencies,
    manifest,
    pointer: &context.prepared.pointer,
    rewrite_plan: rewrite,
    limits: context.limits,
  })
  .map_err(|source| FrozenIndexGenerationPublicationErrorV1::format(source, context.failure_boundary))?;
  operation(&mut machine, &dependencies)
}

fn owner_dependencies<'a>(
  plan: &'a FrozenIndexBatchApplicationPlanV1,
  artifacts: &[&'a EncodedImmutableIndexArtifactV1],
  prepared: &PreparedPointerOwnerV1,
) -> Result<Vec<&'a EncodedImmutableIndexArtifactV1>, FrozenIndexGenerationPublicationErrorV1> {
  let count = prepared
    .dependency_artifact_indices
    .len()
    .checked_add(prepared.dependency_manifest_indices.len())
    .ok_or_else(|| plan_error("owner dependency count overflowed"))?;
  let mut dependencies = Vec::new();
  dependencies.try_reserve_exact(count).map_err(|source| {
    allocation_error("owner dependency references", source.to_string(), IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)
  })?;
  for index in &prepared.dependency_artifact_indices {
    dependencies.push(*artifacts.get(*index).ok_or_else(|| plan_error("owner dependency artifact index is out of range"))?);
  }
  for index in &prepared.dependency_manifest_indices {
    dependencies.push(owner_manifest_from_plan(plan, *index));
  }
  Ok(dependencies)
}

fn decode_pointer_slots<'a>(
  pair: &'a LoadedIndexActivePointerPairV1,
  hash_algorithm: HashAlgorithm,
) -> Result<[Option<ActivePointerV1<'a>>; 2], FrozenIndexGenerationPublicationErrorV1> {
  let decode = |slot: &'a Option<super::first_authority::LoadedIndexActivePointerV1>| {
    slot.as_ref().map(|slot| decode_active_pointer(&slot.bytes, hash_algorithm)).transpose().map_err(|source| {
      FrozenIndexGenerationPublicationErrorV1::format(source, IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained)
    })
  };
  Ok([decode(&pair.slots[0])?, decode(&pair.slots[1])?])
}

fn pointer_observations<'a>(
  pair: &LoadedIndexActivePointerPairV1,
  decoded: &'a [Option<ActivePointerV1<'a>>; 2],
  ignored_slot: Option<u8>,
) -> [ActivePointerSlotObservationV1<'a>; 2] {
  std::array::from_fn(|index| {
    if ignored_slot == Some(index as u8) {
      ActivePointerSlotObservationV1::Missing
    } else if pair.structurally_invalid_slots[index] {
      ActivePointerSlotObservationV1::StructurallyInvalid
    } else if let Some(pointer) = decoded[index].as_ref() {
      ActivePointerSlotObservationV1::Structural { pointer, closure_valid: !pair.closure_invalid_slots[index] }
    } else {
      ActivePointerSlotObservationV1::Missing
    }
  })
}

fn validate_immutable_batch_receipts(
  receipts: Option<&[super::first_authority::IndexArtifactPublicationReceiptV1]>,
  artifacts: &[&EncodedImmutableIndexArtifactV1],
) -> Result<(), FrozenIndexGenerationPublicationErrorV1> {
  if artifacts.is_empty() {
    if receipts.is_some_and(|receipts| !receipts.is_empty()) {
      return Err(plan_error("empty immutable batch produced receipts"));
    }
    return Ok(());
  }
  let receipts = receipts.ok_or_else(|| plan_error("nonempty immutable batch produced no receipts"))?;
  if receipts.len() != artifacts.len() {
    return Err(plan_error("immutable batch receipt count disagrees with its request"));
  }
  if receipts.iter().zip(artifacts).any(|(receipt, artifact)| receipt.artifact_key != artifact.key || receipt.write_sequence == 0) {
    return Err(plan_error("immutable batch receipt does not bind its requested artifact order"));
  }
  Ok(())
}

fn owner_manifest<'a>(request: &FrozenIndexGenerationPublicationRequestV1<'a>, owner_index: usize) -> &'a EncodedImmutableIndexArtifactV1 {
  owner_manifest_from_plan(request.plan, owner_index)
}

fn owner_manifest_from_plan(plan: &FrozenIndexBatchApplicationPlanV1, owner_index: usize) -> &EncodedImmutableIndexArtifactV1 {
  plan.owner_plans()[owner_index].successor_manifest()
}

fn manifest_parent_key<'a>(body: &'a IndexManifestBodyV1<'a>) -> Option<&'a [u8]> {
  match body {
    IndexManifestBodyV1::ScopeCatalog(_) | IndexManifestBodyV1::FieldNvt(_) => None,
    IndexManifestBodyV1::ValueStore(body) => Some(body.scope_catalog_manifest),
    IndexManifestBodyV1::FieldIndex(body) => Some(body.value_store_manifest),
  }
}

fn manifest_owner_class(kind: IndexManifestKindV1) -> Option<IndexMembershipOwnerClassV1> {
  match kind {
    IndexManifestKindV1::ScopeCatalog => Some(IndexMembershipOwnerClassV1::ScopeCatalog),
    IndexManifestKindV1::ValueStore => Some(IndexMembershipOwnerClassV1::ValueStore),
    IndexManifestKindV1::FieldIndex => Some(IndexMembershipOwnerClassV1::FieldIndex),
    IndexManifestKindV1::FieldNvt => None,
  }
}

fn owner_class_rank(owner_class: IndexMembershipOwnerClassV1) -> u8 {
  match owner_class {
    IndexMembershipOwnerClassV1::ScopeCatalog => 0,
    IndexMembershipOwnerClassV1::ValueStore => 1,
    IndexMembershipOwnerClassV1::FieldIndex => 2,
  }
}

fn check_cancelled(
  is_cancelled: &dyn Fn() -> bool,
  boundary: IndexGenerationPublicationFailureBoundaryV1,
) -> Result<(), FrozenIndexGenerationPublicationErrorV1> {
  if is_cancelled() {
    Err(FrozenIndexGenerationPublicationErrorV1::Cancelled { boundary })
  } else {
    Ok(())
  }
}

fn plan_error(message: impl Into<String>) -> FrozenIndexGenerationPublicationErrorV1 {
  FrozenIndexGenerationPublicationErrorV1::invalid(
    "index_generation_application_plan",
    message,
    IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained,
  )
}

fn allocation_error(
  context: &'static str,
  message: String,
  boundary: IndexGenerationPublicationFailureBoundaryV1,
) -> FrozenIndexGenerationPublicationErrorV1 {
  FrozenIndexGenerationPublicationErrorV1::invalid("index_generation_allocation", format!("{context} failed: {message}"), boundary)
}

fn clone_bytes(
  value: &[u8],
  context: &'static str,
  boundary: IndexGenerationPublicationFailureBoundaryV1,
) -> Result<Vec<u8>, FrozenIndexGenerationPublicationErrorV1> {
  let mut cloned = Vec::new();
  cloned.try_reserve_exact(value.len()).map_err(|source| allocation_error(context, source.to_string(), boundary))?;
  cloned.extend_from_slice(value);
  Ok(cloned)
}

fn publication_format_error(message: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::CrossRecordClosureMismatch, "index_generation_authority_closure", message)
}

fn successor_format_error(source: FormatError) -> FrozenIndexGenerationPublicationErrorV1 {
  FrozenIndexGenerationPublicationErrorV1::format(source, IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible)
}
