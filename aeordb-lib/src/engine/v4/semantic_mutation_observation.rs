//! A coherent read of durable task controls, never a resume or retention permit.
#[path = "semantic_mutation_inventory.rs"]
mod inventory;
pub use inventory::{NativeSemanticSourceUnionValidationBoundsV1, SemanticSourceUnionValidationSummaryV1};
pub use inventory::{NativeSemanticCompilerProgressBoundsV1, NativeSemanticCompilerProgressV1, SemanticCompilerConstructionModeV1};
pub use inventory::{NativeSemanticTaskGraphBoundsV1, SemanticTaskGraphErrorV1, SemanticTaskGraphSummaryV1};
pub use inventory::{NativeSemanticSourceUnionStagingRequestV1, NativeStagedSemanticSourceUnionV1, SemanticSourceUnionStagingSummaryV1};
pub use inventory::{NativeSemanticSourceControlPublicationErrorV1, NativeSemanticSourceNodeStagingRequestV1};
pub use inventory::NativeSemanticNamespaceSourceCursorV1;
pub use inventory::{
  NativeSemanticSourceReplacementV1, NativeSemanticSourceUnionBoundsV1, NativeSemanticSourceUnionErrorV1,
  NativeSemanticSourceUnionRequestV1, NativeSemanticSourceUnionV1,
};
pub use inventory::{
  NativeSemanticNamespaceSourceBoundsV1, NativeSemanticNamespaceSourceErrorV1, NativeSemanticNamespaceSourceRequestV1,
  NativeSemanticNamespaceSourceSummaryV1, NativeSemanticNamespaceSourceV1,
};
pub use inventory::{NativeSemanticAliasSnapshotRequestV1, NativeSemanticAliasSnapshotV1};
pub use inventory::{NativeSemanticPluginSourceBoundsV1, NativeSemanticPluginSourceErrorV1, NativeSemanticPluginSourcesV1};
pub use inventory::{
  NativeSemanticSourcePublicationErrorV1, SemanticSourceLookupDispositionV1, NativeSemanticSourceCatalogBoundsV1,
  NativeSemanticSourceLookupV1, SemanticSourceCatalogSideV1, SemanticSourceCatalogSummaryV1, NativeProtectedSemanticSourceV1,
  NativeSemanticMutationInventoryBoundsV1, NativeSemanticMutationInventoryV1, NativeSemanticSourceReadBoundsV1,
  SemanticMutationInventorySummaryV1,
};
use super::*;
use super::super::semantic_mutation_control::{
  SemanticMutationCheckpointV1, SemanticMutationTaskV1, decode_semantic_mutation_checkpoint, decode_semantic_mutation_selection,
  decode_semantic_mutation_task,
};

pub struct SemanticMutationObservationRequestV1<'a> {
  pub database_id: &'a [u8; 16],
  pub task_id: &'a [u8; 16],
  pub memory: &'a MemoryCoordinator,
  pub cancellation: &'a CancellationToken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticMutationObservationDispositionV1 {
  /// This requested task's checked A/B paths are absent, not a global task inventory.
  Absent,
  /// The selected terminal task declares release; its checkpoint is not loaded.
  ReleasedTerminal,
  /// The task declares unreleased pins and its checkpoint binding is valid.
  /// This does not prove retained graph closure, current ownership or resume rights.
  CheckpointHeld,
}

pub struct SemanticMutationObservationV1 {
  header: DatabaseHeaderObservationV4,
  disposition: SemanticMutationObservationDispositionV1,
  task: Option<LoadedMutableSystemControlV1>,
  generation: Option<LoadedMutableSystemControlV1>,
  checkpoint: Option<LoadedImmutableSystemControlV1>,
  _memory: MemoryReservation,
}

impl fmt::Debug for SemanticMutationObservationV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("SemanticMutationObservationV1")
      .field("disposition", &self.disposition)
      .field("header_sequence", &self.header.selected.header.slot_sequence)
      .field("generation", &self.generation())
      .finish_non_exhaustive()
  }
}

impl SemanticMutationObservationV1 {
  pub fn header(&self) -> &DatabaseHeaderObservationV4 {
    &self.header
  }

  pub fn disposition(&self) -> SemanticMutationObservationDispositionV1 {
    self.disposition
  }

  pub fn generation(&self) -> Option<u64> {
    self.generation.as_ref().map(|selected| selected.control_sequence)
  }

  pub fn task_selection(&self) -> Option<&LoadedMutableSystemControlV1> {
    self.task.as_ref()
  }

  pub fn generation_selection(&self) -> Option<&LoadedMutableSystemControlV1> {
    self.generation.as_ref()
  }

  pub fn task(&self) -> Result<Option<SemanticMutationTaskV1<'_>>, FormatError> {
    self
      .task
      .as_ref()
      .map(|selected| decode_semantic_mutation_task(&selected.bytes, self.header.selected.header.hash_algorithm))
      .transpose()
  }

  pub fn checkpoint(&self) -> Result<Option<SemanticMutationCheckpointV1<'_>>, FormatError> {
    self
      .checkpoint
      .as_ref()
      .map(|selected| decode_semantic_mutation_checkpoint(&selected.bytes, self.header.selected.header.hash_algorithm))
      .transpose()
  }
}

#[derive(Debug)]
pub enum SemanticMutationObservationErrorV1 {
  Invalid { code: &'static str, message: &'static str },
  Resource { code: &'static str, message: &'static str },
  Allocation { code: &'static str, source: std::collections::TryReserveError },
  ResourceRead { code: &'static str, source: FirstAuthorityPublicationErrorV1 },
  Compression { status: usize },
  Authority(FirstAuthorityPublicationErrorV1),
  Memory(MemoryCoordinatorError),
}

impl SemanticMutationObservationErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Invalid { code, .. } | Self::Resource { code, .. } => code,
      Self::Allocation { code, .. } | Self::ResourceRead { code, .. } => code,
      Self::Compression { .. } => "semantic_source_chunk_compression",
      Self::Authority(source) => source.code(),
      Self::Memory(_) => "semantic_task_observation_memory",
    }
  }
}

impl Display for SemanticMutationObservationErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Invalid { code, message } | Self::Resource { code, message } => write!(formatter, "{code}: {message}"),
      Self::Allocation { code, source } => write!(formatter, "{code}: {source}"),
      Self::ResourceRead { code, source } => write!(formatter, "{code}: {source}"),
      Self::Compression { status } => {
        write!(formatter, "semantic_source_chunk_compression: {} ({status})", zstd::zstd_safe::get_error_name(*status))
      }
      Self::Authority(source) => write!(formatter, "semantic task observation: {source}"),
      Self::Memory(source) => write!(formatter, "semantic task observation memory: {source}"),
    }
  }
}

impl Error for SemanticMutationObservationErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Invalid { .. } | Self::Resource { .. } => None,
      Self::Allocation { source, .. } => Some(source),
      Self::ResourceRead { source, .. } => Some(source),
      Self::Compression { .. } => None,
      Self::Authority(source) => Some(source),
      Self::Memory(source) => Some(source),
    }
  }
}

impl From<FirstAuthorityPublicationErrorV1> for SemanticMutationObservationErrorV1 {
  fn from(source: FirstAuthorityPublicationErrorV1) -> Self {
    Self::Authority(source)
  }
}

impl From<FormatError> for SemanticMutationObservationErrorV1 {
  fn from(source: FormatError) -> Self {
    Self::Authority(FirstAuthorityPublicationErrorV1::Format(source))
  }
}

impl From<MemoryCoordinatorError> for SemanticMutationObservationErrorV1 {
  fn from(source: MemoryCoordinatorError) -> Self {
    Self::Memory(source)
  }
}

impl V4FirstAuthorityPublisher {
  /// Observe task, generation and retained checkpoint under one physical authority
  /// boundary. The returned bytes remain memory-accounted, but confer no runtime
  /// ownership, reachability or activation permission.
  pub fn observe_semantic_mutation_task(
    &self,
    request: SemanticMutationObservationRequestV1<'_>,
  ) -> Result<SemanticMutationObservationV1, SemanticMutationObservationErrorV1> {
    self.observe_semantic_mutation_task_with_observer(request, || {})
  }

  pub(super) fn observe_semantic_mutation_task_with_observer(
    &self,
    request: SemanticMutationObservationRequestV1<'_>,
    after_task: impl FnOnce(),
  ) -> Result<SemanticMutationObservationV1, SemanticMutationObservationErrorV1> {
    check_cancelled(request.cancellation)?;
    if request.database_id.iter().all(|byte| *byte == 0) || request.task_id.iter().all(|byte| *byte == 0) {
      return Err(SemanticMutationObservationErrorV1::Invalid {
        code: "semantic_task_observation_identity",
        message: "database and task identities must be nonzero",
      });
    }

    let memory = reserve_observation_memory(request.memory)?;
    let _authority = self.selected_semantic_authority_guard()?;
    check_cancelled(request.cancellation)?;
    let observation = self.observe()?;
    let header = &observation.selected.header;
    validate_mutable_system_control_identity(header, SystemControlKindV1::SemanticMutationTask, request.database_id, request.task_id)?;
    let kv = self.lock_kv()?;
    validate_kv_header_alignment(&kv, header)?;
    let task =
      load_mutable_system_control_pair(&self.file, &kv, header, SystemControlKindV1::SemanticMutationTask, request.task_id)?.selected;
    after_task();
    check_cancelled(request.cancellation)?;
    complete_semantic_mutation_observation(&self.file, &kv, observation, task, request, memory)
  }
}

fn reserve_observation_memory(memory: &MemoryCoordinator) -> Result<MemoryReservation, SemanticMutationObservationErrorV1> {
  // Bound the shared loaders' simultaneously live slot bodies, selected
  // copies, FileRecord entities/fields and small header/hash/path buffers.
  // The existing KV cache retains its own accounting. This reservation is
  // not a claim that every transitive helper allocation can recover host OOM.
  let largest_control_cap = SystemControlKindV1::SemanticMutationTask
    .encoded_cap()
    .max(SystemControlKindV1::SemanticMutationCheckpoint.encoded_cap())
    .max(SystemControlKindV1::SemanticMutationGeneration.encoded_cap()) as u64;
  let record_cap = FIRST_AUTHORITY_CONTROL_ENTITY_CAP as u64;
  let reserved_bytes = largest_control_cap
    .checked_add(record_cap)
    .and_then(|bytes| bytes.checked_mul(8))
    .and_then(|bytes| bytes.checked_add(record_cap))
    .ok_or(SemanticMutationObservationErrorV1::Invalid {
      code: "semantic_task_observation_memory_bound",
      message: "native observation memory bound overflowed",
    })?;
  Ok(memory.reserve(MemoryOwner::Task, reserved_bytes, AdmissionClass::Maintenance)?)
}

fn complete_semantic_mutation_observation(
  file: &File,
  kv: &impl FirstAuthorityEntityLookupV1,
  observation: DatabaseHeaderObservationV4,
  task: Option<LoadedMutableSystemControlV1>,
  request: SemanticMutationObservationRequestV1<'_>,
  memory: MemoryReservation,
) -> Result<SemanticMutationObservationV1, SemanticMutationObservationErrorV1> {
  let header = &observation.selected.header;
  let Some(selected_task) = task.as_ref() else {
    return Ok(SemanticMutationObservationV1 {
      header: observation,
      disposition: SemanticMutationObservationDispositionV1::Absent,
      task: None,
      generation: None,
      checkpoint: None,
      _memory: memory,
    });
  };

  // Known format support is not ordinary-service capability advertisement.
  // A physical file containing these controls must still declare both bits.
  let bit = super::super::contract_generated::capability_bit::SEMANTIC_MUTATION_TASK_V1;
  let index = usize::from(bit / 8);
  let mask = 1u8 << (bit % 8);
  if header.required_reader_capabilities[index] & mask == 0 || header.required_writer_capabilities[index] & mask == 0 {
    return Err(SemanticMutationObservationErrorV1::Invalid {
      code: "semantic_task_observation_capability",
      message: "present semantic task controls require reader and writer capability declarations",
    });
  }
  let generation = load_mutable_system_control_pair(file, kv, header, SystemControlKindV1::SemanticMutationGeneration, &[])?
    .selected
    .ok_or(SemanticMutationObservationErrorV1::Invalid {
      code: "semantic_task_observation_generation_missing",
      message: "an existing semantic task has no selected generation control",
    })?;
  check_cancelled(request.cancellation)?;
  let decoded_task = decode_semantic_mutation_task(&selected_task.bytes, header.hash_algorithm)?;
  let (disposition, checkpoint) = if decoded_task.pins_released {
    // Released terminal summaries no longer promise checkpoint retention.
    (SemanticMutationObservationDispositionV1::ReleasedTerminal, None)
  } else {
    let mut checkpoint_identity = [0; 24];
    checkpoint_identity[..16].copy_from_slice(request.task_id);
    checkpoint_identity[16..].copy_from_slice(&decoded_task.checkpoint_sequence.to_le_bytes());
    let checkpoint =
      load_immutable_system_control_file(file, kv, header, SystemControlKindV1::SemanticMutationCheckpoint, &checkpoint_identity)?.ok_or(
        SemanticMutationObservationErrorV1::Invalid {
          code: "semantic_task_observation_checkpoint_missing",
          message: "an unreleased semantic task has no selected immutable checkpoint",
        },
      )?;
    check_cancelled(request.cancellation)?;
    decode_semantic_mutation_selection(&selected_task.bytes, &checkpoint.bytes, header.hash_algorithm)?;
    (SemanticMutationObservationDispositionV1::CheckpointHeld, Some(checkpoint))
  };
  check_cancelled(request.cancellation)?;
  Ok(SemanticMutationObservationV1 { header: observation, disposition, task, generation: Some(generation), checkpoint, _memory: memory })
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), SemanticMutationObservationErrorV1> {
  if cancellation.is_cancelled() {
    return Err(SemanticMutationObservationErrorV1::Invalid {
      code: "semantic_task_observation_cancelled",
      message: "native semantic task observation was cancelled",
    });
  }
  Ok(())
}
