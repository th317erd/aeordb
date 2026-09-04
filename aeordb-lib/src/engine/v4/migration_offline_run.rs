//! One offline, source-invariant v3-to-v4 shadow migration run.
//!
//! This is the sole orchestration adapter over the migration owners. It keeps
//! the v3 engine in read-only inspection mode, publishes only to a separately
//! identified v4 destination, and does not expose cutover or service-write
//! authority.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs::{self, File};
use std::io::Read;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::first_authority::{PreparedNamespaceTreeV0, V4FirstAuthorityPublisher};
use super::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1};
use super::hash::digest_parts;
use super::migration_base_clone_execution::{MigrationBaseCloneExecutionRequestV1, execute_migration_base_clone_v1};
use super::migration_capture_replay::{
  MigrationCaptureReplayAuthorityTemplateV1, MigrationSuccessorProjectionRequestV1, load_destination_tree, namespace_root_for_tree,
  publish_migration_successor_v1,
};
use super::migration_control::{MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED, MigrationPhaseV1, MigrationProgressStateV1};
use super::migration_destination::{
  MigrationDestinationInitializationRequestV1, initialize_migration_destination_for_offline_run_v1, observe_migration_destination_path_v1,
};
use super::migration_final_authority_reconciliation::{
  MigrationFinalAuthorityReconciliationRequestV1, MigrationFinalRootMappingClosureV1, MigrationFinalRootMappingSinkV1,
  MigrationFinalRootMappingV1, execute_final_authority_reconciliation_v1,
};
use super::migration_final_reconciliation::{
  MigrationFinalNamespaceReconciliationRequestV1, MigrationSourceWriteFreezeRequestV1, acquire_migration_source_write_freeze_v1,
  execute_final_namespace_reconciliation_v1,
};
use super::migration_offline_preflight::{OfflineMigrationPreflightRequestV1, collect_offline_migration_preflight_v1};
use super::migration_owner::{
  MigrationAcquisitionRequestV1, MigrationDestinationVerificationCompletionRequestV1, MigrationDestinationVerificationRequestV1,
  MigrationFinalFreezeCompletionRequestV1, MigrationProgressTransitionRequestV1, MigrationStateOwnerV1,
};
use super::migration_preflight::MigrationPreflightPermitV1;
use super::migration_root_map::LegacyRootMapRowV1;
use super::migration_root_map_owner::{
  LegacyRootMapOwnerV1, LegacyRootMapProducerSinkV1, LegacyRootMapPublicationRequestV1, LegacyRootMapStagedPriorLookupV1,
  LegacyRootMapStagingWorkspaceV1, LegacyRootMapWorkspaceIdentityV1, LegacyRootMapWorkspaceOptionsV1,
  LegacyRootMapWorkspaceReopenOptionsV1, VerifiedLegacyRootMapReaderV1,
};
use super::migration_run_manifest::{
  MigrationRunBoundsV1, MigrationRunManifestCreateRequestV1, MigrationRunManifestV1, create_migration_run_manifest_v1,
  open_migration_run_manifest_v1,
};
use super::migration_source_gc::{MigrationSourceGcSuspensionOwnerV1, MigrationSourceGcSuspensionRequestV1};
use super::migration_v3_authority_inventory::{
  V3MigrationAuthorityInventoryLimitsV1, V3MigrationAuthorityInventoryRequestV1, collect_v3_migration_authority_inventory_v1,
};
use super::namespace::{
  SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, decode_namespace_root, encode_semantic_state_object,
};
use super::root_authority::decode_root_admission_commit;
use super::system_family::{MigrationPolicyV1, SystemFamilyPolicyDecisionV1, SystemFamilyPolicyResolverV1, SystemFamilySubjectV1};
use crate::engine::btree::{BTreeNode, is_btree_format};
use crate::engine::compression::decompress_bounded;
use crate::engine::config_resolver::CommandLineConfigOverrides;
use crate::engine::directory_entry::{ChildEntry, deserialize_child_entries};
use crate::engine::file_record::FileRecord;
use crate::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use crate::engine::v4::entity::EntryTypeV4;
use crate::engine::{CompressionAlgorithm, EntryType, StorageEngine};

const RETIREMENT_SEGMENT_BYTES: usize = 1024 * 1024;
const SOURCE_CHECKSUM_BUFFER_BYTES: usize = 1024 * 1024;
const CLOCK_STEP_MS: u64 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OfflineMigrationRunIdentityV1 {
  pub database_id: [u8; 16],
  pub migration_id: [u8; 16],
  pub source_physical_instance_id: [u8; 16],
  pub destination_physical_instance_id: [u8; 16],
  pub holder_boot_id: [u8; 16],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OfflineMigrationRunClockV1 {
  pub wall_time_ms: u64,
  pub monotonic_time_ms: u64,
}

pub struct OfflineMigrationRunRequestV1<'a> {
  pub source: &'a Path,
  pub destination: &'a Path,
  pub workspace: &'a Path,
  pub executable: &'a Path,
  pub source_commit: [u8; 20],
  pub identity: OfflineMigrationRunIdentityV1,
  pub configuration_overrides: CommandLineConfigOverrides,
  pub bounds: MigrationRunBoundsV1,
  pub acquisition_timeout: Duration,
  pub clock: OfflineMigrationRunClockV1,
  pub cancellation: &'a CancellationToken,
  pub resume: bool,
  pub milestone_observer: Option<&'a mut dyn OfflineMigrationRunMilestoneObserverV1>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OfflineMigrationRunMilestoneV1 {
  ManifestDurable,
  DestinationInitialized,
  MigrationControlsAcquired,
  SourceGcSuspended,
  PreflightRunning,
  PreflightComplete,
  CopyPending,
  CopyRunning,
  BaseCloneStaged,
  BaseSuccessorPublished,
  CopyComplete,
  ReconcilePending,
  ReconcileRunning,
  ReconcileComplete,
  FinalFreezePending,
  FinalFreezeRunning,
  FinalNamespaceReconciled,
  FinalAuthorityStaged,
  FinalFreezeComplete,
  RootMapPublished,
  DestinationVerificationPending,
  DestinationVerificationRunning,
  DestinationVerificationComplete,
}

pub trait OfflineMigrationRunMilestoneObserverV1 {
  /// Return true to pause immediately after this durable milestone.
  fn should_pause_after(&mut self, milestone: OfflineMigrationRunMilestoneV1) -> bool;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfflineMigrationRunReceiptV1 {
  pub phase: MigrationPhaseV1,
  pub state: MigrationProgressStateV1,
  pub destination_full_verified: bool,
  pub source_complete_file_checksum: [u8; 32],
  pub destination_header_sequence: u64,
  pub copied_entity_count: u64,
  pub copied_content_bytes: u64,
  pub verified_root_count: u64,
  pub verified_entity_count: u64,
  pub verified_content_bytes: u64,
}

struct OfflineBaseCopyOutcomeV1 {
  destination_head_tree: Vec<u8>,
  destination_header_sequence: u64,
  processed_seeds: u64,
  published_entities: u64,
  copied_chunk_bytes: u64,
}

#[derive(Debug)]
pub struct OfflineMigrationRunErrorV1 {
  code: &'static str,
  message: String,
}

impl OfflineMigrationRunErrorV1 {
  pub const fn code(&self) -> &'static str {
    self.code
  }

  fn new(code: &'static str, message: impl Into<String>) -> Self {
    Self { code, message: message.into() }
  }

  fn owned(error: impl Display) -> Self {
    Self::new("offline_migration_run", error.to_string())
  }
}

impl Display for OfflineMigrationRunErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.message)
  }
}

impl Error for OfflineMigrationRunErrorV1 {}

struct RunClockV1 {
  wall: u64,
  monotonic: u64,
}

impl RunClockV1 {
  fn new(clock: OfflineMigrationRunClockV1) -> Result<Self, OfflineMigrationRunErrorV1> {
    if clock.wall_time_ms == 0 || clock.wall_time_ms > i64::MAX as u64 || clock.monotonic_time_ms == 0 {
      return Err(OfflineMigrationRunErrorV1::new("offline_migration_clock", "migration clock values must be nonzero and persistable"));
    }
    Ok(Self { wall: clock.wall_time_ms, monotonic: clock.monotonic_time_ms })
  }

  fn next(&mut self) -> Result<(i64, u64, u64), OfflineMigrationRunErrorV1> {
    self.wall = self
      .wall
      .checked_add(CLOCK_STEP_MS)
      .ok_or_else(|| OfflineMigrationRunErrorV1::new("offline_migration_clock", "wall clock overflowed"))?;
    self.monotonic = self
      .monotonic
      .checked_add(CLOCK_STEP_MS)
      .ok_or_else(|| OfflineMigrationRunErrorV1::new("offline_migration_clock", "monotonic clock overflowed"))?;
    let updated =
      i64::try_from(self.wall).map_err(|error| OfflineMigrationRunErrorV1::new("offline_migration_clock", error.to_string()))?;
    Ok((updated, self.wall, self.monotonic))
  }
}

pub fn execute_offline_migration_v1(
  mut request: OfflineMigrationRunRequestV1<'_>,
) -> Result<OfflineMigrationRunReceiptV1, OfflineMigrationRunErrorV1> {
  let mut milestone_observer = request.milestone_observer.take();
  validate_identity(request.identity)?;
  let mut clock = RunClockV1::new(request.clock)?;
  let manifest = if request.resume {
    Some(
      open_migration_run_manifest_v1(request.workspace, request.cancellation)
        .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?,
    )
  } else {
    None
  };
  let preflight = collect_offline_migration_preflight_v1(OfflineMigrationPreflightRequestV1 {
    source: request.source,
    destination: request.destination,
    workspace: request.workspace,
    executable: request.executable,
    source_commit: request.source_commit,
    database_id: request.identity.database_id,
    migration_id: request.identity.migration_id,
    source_physical_instance_id: request.identity.source_physical_instance_id,
    destination_physical_instance_id: request.identity.destination_physical_instance_id,
    configuration_overrides: request.configuration_overrides.clone(),
    bounds: request.bounds,
    acquisition_timeout: request.acquisition_timeout,
    cancellation: request.cancellation,
    resume_manifest: manifest.as_ref(),
  })
  .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  let verified_source_entries = preflight.verified_source_entries();
  let permit = preflight.permit().clone();
  let source_path =
    request.source.to_str().ok_or_else(|| OfflineMigrationRunErrorV1::new("offline_migration_source_path", "source path is not UTF-8"))?;
  let source = Arc::new(
    StorageEngine::open_for_offline_migration_inspection(source_path, request.configuration_overrides.clone())
      .map_err(OfflineMigrationRunErrorV1::owned)?,
  );
  let result = (|| {
    if let Some(manifest) = manifest.as_ref() {
      if request.destination.exists() {
        let destination = Arc::new(V4FirstAuthorityPublisher::open(request.destination).map_err(OfflineMigrationRunErrorV1::owned)?);
        validate_reopened_destination(&permit, &destination)?;
        if MigrationStateOwnerV1::observe_completed_destination_verification_if_present(&destination, &permit)
          .map_err(OfflineMigrationRunErrorV1::owned)?
          .is_some()
        {
          execute_completed_resume(&request, &permit, manifest, &source, &destination, &mut clock)
        } else {
          execute_admitted_run(&request, &permit, &source, &destination, &mut clock, &mut milestone_observer)
        }
      } else {
        let destination_observation = observe_migration_destination_path_v1(request.destination)
          .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
        let destination = initialize_migration_destination_for_offline_run_v1(
          MigrationDestinationInitializationRequestV1 {
            permit: &permit,
            destination: &destination_observation,
            created_at_ms: manifest.created_at_ms(),
            writer_fence_epoch: 1,
            cancellation: request.cancellation,
          },
          verified_source_entries,
        )
        .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
        pause_after(&mut milestone_observer, OfflineMigrationRunMilestoneV1::DestinationInitialized)?;
        execute_admitted_run(&request, &permit, &source, &destination.shared_publisher(), &mut clock, &mut milestone_observer)
      }
    } else {
      create_migration_run_manifest_v1(MigrationRunManifestCreateRequestV1 {
        workspace: request.workspace,
        source: request.source,
        destination: request.destination,
        permit: &permit,
        holder_boot_id: request.identity.holder_boot_id,
        created_at_ms: request.clock.wall_time_ms,
        bounds: request.bounds,
        cancellation: request.cancellation,
      })
      .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
      pause_after(&mut milestone_observer, OfflineMigrationRunMilestoneV1::ManifestDurable)?;
      let destination_observation = observe_migration_destination_path_v1(request.destination)
        .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
      let destination = initialize_migration_destination_for_offline_run_v1(
        MigrationDestinationInitializationRequestV1 {
          permit: &permit,
          destination: &destination_observation,
          created_at_ms: request.clock.wall_time_ms,
          writer_fence_epoch: 1,
          cancellation: request.cancellation,
        },
        verified_source_entries,
      )
      .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
      pause_after(&mut milestone_observer, OfflineMigrationRunMilestoneV1::DestinationInitialized)?;
      execute_admitted_run(&request, &permit, &source, &destination.shared_publisher(), &mut clock, &mut milestone_observer)
    }
  })();
  let shutdown = source.shutdown().map_err(OfflineMigrationRunErrorV1::owned);
  let result = result?;
  shutdown?;
  Ok(result)
}

fn pause_after(
  observer: &mut Option<&mut dyn OfflineMigrationRunMilestoneObserverV1>,
  milestone: OfflineMigrationRunMilestoneV1,
) -> Result<(), OfflineMigrationRunErrorV1> {
  if observer.as_deref_mut().is_some_and(|observer| observer.should_pause_after(milestone)) {
    return Err(OfflineMigrationRunErrorV1::new(
      "offline_migration_milestone_pause",
      format!("offline migration paused after durable milestone {milestone:?}"),
    ));
  }
  Ok(())
}

fn validate_reopened_destination(
  permit: &MigrationPreflightPermitV1,
  destination: &V4FirstAuthorityPublisher,
) -> Result<(), OfflineMigrationRunErrorV1> {
  let observation = destination.observe().map_err(OfflineMigrationRunErrorV1::owned)?;
  let header = &observation.selected.header;
  if observation.selected.redundancy_degraded
    || header.database_id != permit.database_id()
    || header.physical_instance_id != permit.destination_physical_instance_id()
    || header.hash_algorithm != permit.hash_algorithm()
    || header.writer_fence_epoch != 1
    || header.required_reader_capabilities != permit.required_reader_capabilities().into_bytes()
    || header.required_writer_capabilities != permit.required_writer_capabilities().into_bytes()
    || header.system_family_registry_fingerprint != permit.system_family_registry_fingerprint()
    || header.head_hash.iter().all(|byte| *byte == 0)
  {
    return Err(OfflineMigrationRunErrorV1::new(
      "offline_migration_resume_destination",
      "reopened destination differs from the immutable migration identity, capability, fence, or registry binding",
    ));
  }
  Ok(())
}

fn execute_completed_resume(
  request: &OfflineMigrationRunRequestV1<'_>,
  permit: &MigrationPreflightPermitV1,
  _manifest: &MigrationRunManifestV1,
  source: &Arc<StorageEngine>,
  destination: &Arc<V4FirstAuthorityPublisher>,
  clock: &mut RunClockV1,
) -> Result<OfflineMigrationRunReceiptV1, OfflineMigrationRunErrorV1> {
  let progress =
    MigrationStateOwnerV1::observe_completed_destination_verification(destination, permit).map_err(OfflineMigrationRunErrorV1::owned)?;
  let memory = migration_memory(request.bounds)?;
  let mut prior_workspace = reopen_root_workspace(request, permit, &memory)?;
  let mut prior = LegacyRootMapStagedPriorLookupV1::snapshot(
    &mut prior_workspace,
    &memory,
    request.bounds.prior_lookup_maximum_memory_bytes,
    request.bounds.maximum_authority_records,
  )
  .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  drop(prior_workspace);
  let root_workspace = reopen_root_workspace(request, permit, &memory)?;
  let root_map = open_root_map(destination, permit, request.cancellation, &memory)?;
  let selected = destination.load_selected_semantic_authority().map_err(OfflineMigrationRunErrorV1::owned)?;
  let authority = authority_template(permit, &initial_destination_root(permit), request.clock)?;
  let final_inventory = collect_inventory(request, permit, source)?;
  let verification_inventory = collect_inventory(request, permit, source)?;
  let mut final_stream = final_inventory.into_final_authority_stream();
  let freeze = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
    permit,
    source,
    cancellation: request.cancellation,
    acquisition_timeout: request.acquisition_timeout,
  })
  .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  let namespace = execute_final_namespace_reconciliation_v1(MigrationFinalNamespaceReconciliationRequestV1 {
    permit,
    freeze: &freeze,
    destination,
    last_reconciled_source_root: permit.source_capture_head(),
    current_destination_tree_root: &selected.namespace_tree_root,
    authority: &authority,
    memory: &memory,
    cancellation: request.cancellation,
    publication_timestamp_ms: publication_time(clock)?,
    maximum_diff_memory_bytes: request.bounds.maximum_memory_bytes,
    maximum_diff_work_items: request.bounds.maximum_work_items,
    maximum_subtree_memory_bytes: request.bounds.maximum_memory_bytes,
    maximum_subtree_work_items: request.bounds.maximum_work_items,
    maximum_total_subtree_work_items: request.bounds.maximum_work_items,
    maximum_decoded_chunk_bytes: to_usize(request.bounds.maximum_decoded_chunk_bytes, "decoded chunk bound")?,
    maximum_directory_depth: request.bounds.maximum_directory_depth as usize,
  })
  .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  let selected_sink = SelectedRootMapValidationSinkV1::new(&root_map, &root_workspace, request.cancellation);
  let mut final_sink =
    VerificationMappingCaptureSinkV1::new(selected_sink, request.bounds.maximum_authority_records, request.bounds.maximum_memory_bytes);
  let final_authority = execute_final_authority_reconciliation_v1(MigrationFinalAuthorityReconciliationRequestV1 {
    permit,
    namespace: &namespace,
    inventory: &mut final_stream,
    prior_mappings: &mut prior,
    root_sink: &mut final_sink,
    destination,
    authority: &authority,
    memory: &memory,
    cancellation: request.cancellation,
    publication_timestamp_ms: publication_time(clock)?,
    maximum_memory_bytes: request.bounds.maximum_memory_bytes,
    maximum_work_items: request.bounds.maximum_work_items,
    maximum_subtree_memory_bytes: request.bounds.maximum_memory_bytes,
    maximum_subtree_work_items: request.bounds.maximum_work_items,
    maximum_total_subtree_work_items: request.bounds.maximum_work_items,
    maximum_decoded_chunk_bytes: to_usize(request.bounds.maximum_decoded_chunk_bytes, "decoded chunk bound")?,
    maximum_destination_entity_bytes: to_usize(request.bounds.maximum_decoded_chunk_bytes, "destination entity bound")?,
    maximum_directory_depth: request.bounds.maximum_directory_depth as usize,
  })
  .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  let detached_mappings = final_sink.into_mappings()?;
  let mut verification_stream = verification_inventory.into_final_authority_stream();
  let verification = verify_destination(
    permit,
    source,
    destination,
    &root_map,
    &mut verification_stream,
    &final_authority.mapping_closure,
    &detached_mappings,
    request.bounds,
    request.cancellation,
    &memory,
  )?;
  freeze.validate_unchanged().map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  let checksum = file_blake3(request.source, request.cancellation)?;
  if checksum != permit.source_complete_file_checksum() {
    return Err(OfflineMigrationRunErrorV1::new(
      "offline_migration_source_changed",
      "source checksum changed between manifest preflight and resumed verification",
    ));
  }
  let final_header = destination.observe().map_err(OfflineMigrationRunErrorV1::owned)?.selected.header;
  Ok(OfflineMigrationRunReceiptV1 {
    phase: progress.phase,
    state: progress.state,
    destination_full_verified: true,
    source_complete_file_checksum: checksum,
    destination_header_sequence: final_header.slot_sequence,
    copied_entity_count: progress.entity_count,
    copied_content_bytes: progress.copied_bytes,
    verified_root_count: verification.roots,
    verified_entity_count: verification.entities,
    verified_content_bytes: verification.content_bytes,
  })
}

fn execute_admitted_run(
  request: &OfflineMigrationRunRequestV1<'_>,
  permit: &MigrationPreflightPermitV1,
  source: &Arc<StorageEngine>,
  destination: &Arc<V4FirstAuthorityPublisher>,
  clock: &mut RunClockV1,
  milestone_observer: &mut Option<&mut dyn OfflineMigrationRunMilestoneObserverV1>,
) -> Result<OfflineMigrationRunReceiptV1, OfflineMigrationRunErrorV1> {
  let memory = migration_memory(request.bounds)?;
  let retirement_options = RetirementJournalBufferOptionsV1::new(1, RETIREMENT_SEGMENT_BYTES, 30_000);
  let retirement_summary = destination
    .reconstruct_retirement_journal_summary(
      request.cancellation,
      &memory,
      request.bounds.maximum_work_items,
      request.bounds.maximum_work_items,
      request.bounds.maximum_work_items,
      request.bounds.maximum_memory_bytes,
    )
    .map_err(OfflineMigrationRunErrorV1::owned)?;
  let mut retirement = match retirement_summary {
    Some(summary) => RetirementJournalOwnerV1::resume_chain(
      permit.hash_algorithm(),
      permit.database_id(),
      &summary,
      retirement_options,
      request.cancellation,
      &memory,
    ),
    None => RetirementJournalOwnerV1::new_chain(
      permit.hash_algorithm(),
      permit.database_id(),
      1,
      1,
      retirement_options,
      request.cancellation,
      &memory,
    ),
  }
  .map_err(OfflineMigrationRunErrorV1::owned)?;
  let (updated, publication, monotonic) = clock.next()?;
  let acquisition = MigrationAcquisitionRequestV1 {
    holder_boot_id: request.identity.holder_boot_id,
    acquired_at_ms: updated,
    lease_duration_ms: i64::try_from(request.bounds.lease_duration_ms)
      .map_err(|error| OfflineMigrationRunErrorV1::new("offline_migration_lease", error.to_string()))?,
    publication_timestamp_ms: publication,
    monotonic_now_ms: monotonic,
  };
  let owner = if request.resume {
    MigrationStateOwnerV1::acquire_or_takeover_for_restart(destination.clone(), permit.clone(), acquisition, &mut retirement)
  } else {
    MigrationStateOwnerV1::acquire(destination.clone(), permit.clone(), acquisition, &mut retirement).map(|(owner, _)| owner)
  }
  .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  pause_after(milestone_observer, OfflineMigrationRunMilestoneV1::MigrationControlsAcquired)?;
  let (updated, publication, monotonic) = clock.next()?;
  let (_source_gc, _) = MigrationSourceGcSuspensionOwnerV1::suspend(
    source,
    &owner,
    MigrationSourceGcSuspensionRequestV1 { suspended_at_ms: updated, publication_timestamp_ms: publication, monotonic_now_ms: monotonic },
    &mut retirement,
  )
  .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  pause_after(milestone_observer, OfflineMigrationRunMilestoneV1::SourceGcSuspended)?;

  transition(&owner, &mut retirement, clock, permit, MigrationPhaseV1::Preflight, MigrationProgressStateV1::Running, 0, 0, 0, 0, 0)?;
  pause_after(milestone_observer, OfflineMigrationRunMilestoneV1::PreflightRunning)?;
  transition(&owner, &mut retirement, clock, permit, MigrationPhaseV1::Preflight, MigrationProgressStateV1::Complete, 0, 0, 0, 0, 0)?;
  pause_after(milestone_observer, OfflineMigrationRunMilestoneV1::PreflightComplete)?;
  transition(&owner, &mut retirement, clock, permit, MigrationPhaseV1::Copy, MigrationProgressStateV1::Pending, 0, 0, 0, 0, 0)?;
  pause_after(milestone_observer, OfflineMigrationRunMilestoneV1::CopyPending)?;
  transition(&owner, &mut retirement, clock, permit, MigrationPhaseV1::Copy, MigrationProgressStateV1::Running, 0, 0, 0, 0, 0)?;
  pause_after(milestone_observer, OfflineMigrationRunMilestoneV1::CopyRunning)?;

  let source_basis = source.frozen_source_authority_snapshot().map_err(OfflineMigrationRunErrorV1::owned)?;
  let mut root_workspace = open_or_create_root_workspace(request, permit, &memory, publication_time(clock)?)?;
  let workspace_timestamp_ms = root_workspace.publication_timestamp_ms();
  let base_entity_timestamp_ms = checked_clock_offset(workspace_timestamp_ms, 1)?;
  let base_successor_timestamp_ms = checked_clock_offset(workspace_timestamp_ms, 2)?;
  let final_namespace_timestamp_ms = checked_clock_offset(workspace_timestamp_ms, 3)?;
  let final_authority_timestamp_ms = checked_clock_offset(workspace_timestamp_ms, 4)?;
  let initial_tree_root = initial_destination_root(permit);
  let mut authority = authority_template(
    permit,
    &initial_tree_root,
    OfflineMigrationRunClockV1 { wall_time_ms: workspace_timestamp_ms, monotonic_time_ms: request.clock.monotonic_time_ms },
  )?;
  authority.base_predecessor_head = namespace_root_for_tree(
    permit.hash_algorithm(),
    &PreparedNamespaceTreeV0 { root_hash: initial_tree_root, stored_value: Vec::new() },
    &authority,
  )
  .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  publication_time(clock)?;
  let base = if root_workspace.is_sealed() {
    let now_ms =
      i64::try_from(clock.wall).map_err(|error| OfflineMigrationRunErrorV1::new("offline_migration_clock", error.to_string()))?;
    let progress =
      owner.observe_owned_progress_for_restart(now_ms).map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
    let resumable_progress = matches!(
      (progress.phase, progress.state),
      (MigrationPhaseV1::FinalFreeze, MigrationProgressStateV1::Running | MigrationProgressStateV1::Complete)
        | (MigrationPhaseV1::DestinationVerify, MigrationProgressStateV1::Pending | MigrationProgressStateV1::Running)
    );
    if !resumable_progress
      || progress.destination_header_sequence == 0
      || progress.copied_through_write_sequence != source_basis.hard_publication_frontier
    {
      return Err(OfflineMigrationRunErrorV1::new(
        "offline_migration_sealed_progress",
        "a sealed root-map workspace requires matching active final-freeze or destination-verification progress",
      ));
    }
    let mut staged = LegacyRootMapStagedPriorLookupV1::snapshot(
      &mut root_workspace,
      &memory,
      request.bounds.prior_lookup_maximum_memory_bytes,
      request.bounds.maximum_authority_records,
    )
    .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
    let destination_head_tree = staged
      .lookup_destination_tree_by_legacy_root(permit.source_capture_head())
      .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?
      .ok_or_else(|| {
        OfflineMigrationRunErrorV1::new(
          "offline_migration_sealed_base_mapping",
          "sealed root-map workspace has no base mapping for the admitted source capture HEAD",
        )
      })?;
    OfflineBaseCopyOutcomeV1 {
      destination_head_tree,
      destination_header_sequence: progress.destination_header_sequence,
      processed_seeds: progress.namespace_count,
      published_entities: progress.entity_count,
      copied_chunk_bytes: progress.copied_bytes,
    }
  } else {
    let inventory = collect_inventory(request, permit, source)?;
    let mut seeds = inventory.into_base_clone_stream();
    let mut root_sink = LegacyRootMapProducerSinkV1::new(&mut root_workspace, &authority, source_basis.hard_publication_frontier)
      .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
    let clone = execute_migration_base_clone_v1(MigrationBaseCloneExecutionRequestV1 {
      permit,
      source: source.as_ref(),
      seeds: &mut seeds,
      seed_results: &mut root_sink,
      destination,
      memory: &memory,
      cancellation: request.cancellation,
      publication_timestamp_ms: base_entity_timestamp_ms,
      maximum_work_items: request.bounds.maximum_work_items,
      maximum_memory_bytes: request.bounds.maximum_memory_bytes,
      maximum_decoded_chunk_bytes: to_usize(request.bounds.maximum_decoded_chunk_bytes, "decoded chunk bound")?,
      maximum_directory_depth: request.bounds.maximum_directory_depth as usize,
    })
    .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
    drop(root_sink);
    pause_after(milestone_observer, OfflineMigrationRunMilestoneV1::BaseCloneStaged)?;

    let base_tree = load_destination_tree(destination, &clone.destination_head_tree)
      .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
    let base_successor = publish_migration_successor_v1(MigrationSuccessorProjectionRequestV1 {
      permit,
      destination,
      authority: &authority,
      source_sequence: source_basis.hard_publication_frontier,
      source_root: &source_basis.namespace_root,
      expected_head_hash: &authority.base_predecessor_head,
      tree: base_tree,
      semantic_timestamp_ms: base_successor_timestamp_ms,
      transaction_domain: b"aeordb.offline-migration.base.transaction.v1\0",
      closure_domain: b"aeordb.offline-migration.base.closure.v1\0",
    })
    .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
    pause_after(milestone_observer, OfflineMigrationRunMilestoneV1::BaseSuccessorPublished)?;
    OfflineBaseCopyOutcomeV1 {
      destination_head_tree: clone.destination_head_tree,
      destination_header_sequence: decode_root_admission_commit(&base_successor.admission_control, permit.hash_algorithm())
        .map_err(OfflineMigrationRunErrorV1::owned)?
        .selected_header_slot_sequence,
      processed_seeds: clone.processed_seeds,
      published_entities: clone.published_entities,
      copied_chunk_bytes: clone.copied_chunk_bytes,
    }
  };
  publication_time(clock)?;

  let destination_sequence = base.destination_header_sequence;
  transition(
    &owner,
    &mut retirement,
    clock,
    permit,
    MigrationPhaseV1::Copy,
    MigrationProgressStateV1::Complete,
    destination_sequence,
    source_basis.hard_publication_frontier,
    base.processed_seeds,
    base.published_entities,
    base.copied_chunk_bytes,
  )?;
  pause_after(milestone_observer, OfflineMigrationRunMilestoneV1::CopyComplete)?;
  transition(
    &owner,
    &mut retirement,
    clock,
    permit,
    MigrationPhaseV1::Reconcile,
    MigrationProgressStateV1::Pending,
    destination_sequence,
    source_basis.hard_publication_frontier,
    base.processed_seeds,
    base.published_entities,
    base.copied_chunk_bytes,
  )?;
  pause_after(milestone_observer, OfflineMigrationRunMilestoneV1::ReconcilePending)?;
  transition(
    &owner,
    &mut retirement,
    clock,
    permit,
    MigrationPhaseV1::Reconcile,
    MigrationProgressStateV1::Running,
    destination_sequence,
    source_basis.hard_publication_frontier,
    base.processed_seeds,
    base.published_entities,
    base.copied_chunk_bytes,
  )?;
  pause_after(milestone_observer, OfflineMigrationRunMilestoneV1::ReconcileRunning)?;
  transition(
    &owner,
    &mut retirement,
    clock,
    permit,
    MigrationPhaseV1::Reconcile,
    MigrationProgressStateV1::Complete,
    destination_sequence,
    source_basis.hard_publication_frontier,
    base.processed_seeds,
    base.published_entities,
    base.copied_chunk_bytes,
  )?;
  pause_after(milestone_observer, OfflineMigrationRunMilestoneV1::ReconcileComplete)?;
  transition(
    &owner,
    &mut retirement,
    clock,
    permit,
    MigrationPhaseV1::FinalFreeze,
    MigrationProgressStateV1::Pending,
    destination_sequence,
    source_basis.hard_publication_frontier,
    base.processed_seeds,
    base.published_entities,
    base.copied_chunk_bytes,
  )?;
  pause_after(milestone_observer, OfflineMigrationRunMilestoneV1::FinalFreezePending)?;
  transition(
    &owner,
    &mut retirement,
    clock,
    permit,
    MigrationPhaseV1::FinalFreeze,
    MigrationProgressStateV1::Running,
    destination_sequence,
    source_basis.hard_publication_frontier,
    base.processed_seeds,
    base.published_entities,
    base.copied_chunk_bytes,
  )?;
  pause_after(milestone_observer, OfflineMigrationRunMilestoneV1::FinalFreezeRunning)?;

  let final_inventory = collect_inventory(request, permit, source)?;
  // Collect an independent verification stream before taking the final
  // exclusive freeze. Both streams are later bound to the exact frozen
  // authority; nesting another maintenance acquisition under that freeze is
  // deliberately forbidden by StorageEngine.
  let verification_inventory = collect_inventory(request, permit, source)?;
  let mut final_stream = final_inventory.into_final_authority_stream();
  let freeze = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
    permit,
    source,
    cancellation: request.cancellation,
    acquisition_timeout: request.acquisition_timeout,
  })
  .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  publication_time(clock)?;
  let namespace = execute_final_namespace_reconciliation_v1(MigrationFinalNamespaceReconciliationRequestV1 {
    permit,
    freeze: &freeze,
    destination,
    last_reconciled_source_root: permit.source_capture_head(),
    current_destination_tree_root: &base.destination_head_tree,
    authority: &authority,
    memory: &memory,
    cancellation: request.cancellation,
    publication_timestamp_ms: final_namespace_timestamp_ms,
    maximum_diff_memory_bytes: request.bounds.maximum_memory_bytes,
    maximum_diff_work_items: request.bounds.maximum_work_items,
    maximum_subtree_memory_bytes: request.bounds.maximum_memory_bytes,
    maximum_subtree_work_items: request.bounds.maximum_work_items,
    maximum_total_subtree_work_items: request.bounds.maximum_work_items,
    maximum_decoded_chunk_bytes: to_usize(request.bounds.maximum_decoded_chunk_bytes, "decoded chunk bound")?,
    maximum_directory_depth: request.bounds.maximum_directory_depth as usize,
  })
  .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  pause_after(milestone_observer, OfflineMigrationRunMilestoneV1::FinalNamespaceReconciled)?;
  let mut prior = LegacyRootMapStagedPriorLookupV1::snapshot(
    &mut root_workspace,
    &memory,
    request.bounds.prior_lookup_maximum_memory_bytes,
    request.bounds.maximum_authority_records,
  )
  .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  let final_root_sink = if root_workspace.is_sealed() {
    OfflineFinalRootMapSinkV1::Validate(SealedRootMapClosureValidationSinkV1::new(&root_workspace))
  } else {
    OfflineFinalRootMapSinkV1::Append(
      LegacyRootMapProducerSinkV1::new(&mut root_workspace, &authority, source_basis.hard_publication_frontier)
        .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?,
    )
  };
  let mut final_sink =
    VerificationMappingCaptureSinkV1::new(final_root_sink, request.bounds.maximum_authority_records, request.bounds.maximum_memory_bytes);
  publication_time(clock)?;
  let final_authority = execute_final_authority_reconciliation_v1(MigrationFinalAuthorityReconciliationRequestV1 {
    permit,
    namespace: &namespace,
    inventory: &mut final_stream,
    prior_mappings: &mut prior,
    root_sink: &mut final_sink,
    destination,
    authority: &authority,
    memory: &memory,
    cancellation: request.cancellation,
    publication_timestamp_ms: final_authority_timestamp_ms,
    maximum_memory_bytes: request.bounds.maximum_memory_bytes,
    maximum_work_items: request.bounds.maximum_work_items,
    maximum_subtree_memory_bytes: request.bounds.maximum_memory_bytes,
    maximum_subtree_work_items: request.bounds.maximum_work_items,
    maximum_total_subtree_work_items: request.bounds.maximum_work_items,
    maximum_decoded_chunk_bytes: to_usize(request.bounds.maximum_decoded_chunk_bytes, "decoded chunk bound")?,
    maximum_destination_entity_bytes: to_usize(request.bounds.maximum_decoded_chunk_bytes, "destination entity bound")?,
    maximum_directory_depth: request.bounds.maximum_directory_depth as usize,
  })
  .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  pause_after(milestone_observer, OfflineMigrationRunMilestoneV1::FinalAuthorityStaged)?;
  drop(final_stream);
  drop(prior);
  let detached_mappings = final_sink.into_mappings()?;
  let (updated, publication, monotonic) = clock.next()?;
  let progress =
    owner.observe_owned_progress_for_restart(updated).map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  if progress.phase == MigrationPhaseV1::FinalFreeze {
    owner
      .complete_final_freeze(
        MigrationFinalFreezeCompletionRequestV1 {
          proof: final_authority.proof(),
          updated_at_ms: updated,
          publication_timestamp_ms: publication,
          monotonic_now_ms: monotonic,
        },
        &mut retirement,
      )
      .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  } else if progress.phase != MigrationPhaseV1::DestinationVerify
    || !matches!(progress.state, MigrationProgressStateV1::Pending | MigrationProgressStateV1::Running)
  {
    return Err(OfflineMigrationRunErrorV1::new(
      "offline_migration_final_freeze_restart",
      "restart progress neither requires nor covers final-freeze completion",
    ));
  }
  pause_after(milestone_observer, OfflineMigrationRunMilestoneV1::FinalFreezeComplete)?;
  LegacyRootMapOwnerV1::new(destination)
    .publish(
      LegacyRootMapPublicationRequestV1 {
        workspace: root_workspace,
        retirement_owner: &mut retirement,
        cancellation: request.cancellation,
        monotonic_now_ms: publication_time(clock)?,
      },
      &memory,
    )
    .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  pause_after(milestone_observer, OfflineMigrationRunMilestoneV1::RootMapPublished)?;

  let root_map = open_root_map(destination, permit, request.cancellation, &memory)?;
  let (updated, publication, monotonic) = clock.next()?;
  let progress =
    owner.observe_owned_progress_for_restart(updated).map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  if progress.phase == MigrationPhaseV1::FinalFreeze
    || (progress.phase == MigrationPhaseV1::DestinationVerify && progress.state == MigrationProgressStateV1::Pending)
  {
    owner
      .begin_destination_verification(
        MigrationDestinationVerificationRequestV1 {
          proof: final_authority.proof(),
          root_map: &root_map,
          cancellation: request.cancellation,
          expected_map_generation: 1,
          updated_at_ms: updated,
          publication_timestamp_ms: publication,
          monotonic_now_ms: monotonic,
        },
        &mut retirement,
      )
      .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  } else if progress.phase != MigrationPhaseV1::DestinationVerify || progress.state != MigrationProgressStateV1::Running {
    return Err(OfflineMigrationRunErrorV1::new(
      "offline_migration_destination_verification_restart",
      "restart progress neither requires nor covers pending destination verification",
    ));
  }
  pause_after(milestone_observer, OfflineMigrationRunMilestoneV1::DestinationVerificationPending)?;
  drop(root_map);
  let root_map = open_root_map(destination, permit, request.cancellation, &memory)?;
  let (updated, publication, monotonic) = clock.next()?;
  owner
    .start_destination_full_verification(
      MigrationDestinationVerificationCompletionRequestV1 {
        proof: final_authority.proof(),
        root_map: &root_map,
        cancellation: request.cancellation,
        expected_map_generation: 1,
        updated_at_ms: updated,
        publication_timestamp_ms: publication,
        monotonic_now_ms: monotonic,
      },
      &mut retirement,
    )
    .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  pause_after(milestone_observer, OfflineMigrationRunMilestoneV1::DestinationVerificationRunning)?;
  drop(root_map);

  let mut verification_stream = verification_inventory.into_final_authority_stream();
  let root_map = open_root_map(destination, permit, request.cancellation, &memory)?;
  let verification = verify_destination(
    permit,
    source,
    destination,
    &root_map,
    &mut verification_stream,
    &final_authority.mapping_closure,
    &detached_mappings,
    request.bounds,
    request.cancellation,
    &memory,
  )?;
  let (updated, publication, monotonic) = clock.next()?;
  owner
    .complete_destination_verification(
      MigrationDestinationVerificationCompletionRequestV1 {
        proof: final_authority.proof(),
        root_map: &root_map,
        cancellation: request.cancellation,
        expected_map_generation: 1,
        updated_at_ms: updated,
        publication_timestamp_ms: publication,
        monotonic_now_ms: monotonic,
      },
      &mut retirement,
    )
    .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  pause_after(milestone_observer, OfflineMigrationRunMilestoneV1::DestinationVerificationComplete)?;
  freeze.validate_unchanged().map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  let checksum = file_blake3(request.source, request.cancellation)?;
  if checksum != permit.source_complete_file_checksum() {
    return Err(OfflineMigrationRunErrorV1::new(
      "offline_migration_source_changed",
      "source checksum changed between preflight and verified shadow completion",
    ));
  }
  let final_header = destination.observe().map_err(OfflineMigrationRunErrorV1::owned)?.selected.header;
  Ok(OfflineMigrationRunReceiptV1 {
    phase: MigrationPhaseV1::DestinationVerify,
    state: MigrationProgressStateV1::Complete,
    destination_full_verified: true,
    source_complete_file_checksum: checksum,
    destination_header_sequence: final_header.slot_sequence,
    copied_entity_count: base.published_entities,
    copied_content_bytes: base.copied_chunk_bytes,
    verified_root_count: verification.roots,
    verified_entity_count: verification.entities,
    verified_content_bytes: verification.content_bytes,
  })
}

#[allow(clippy::too_many_arguments)]
fn transition(
  owner: &MigrationStateOwnerV1,
  retirement: &mut RetirementJournalOwnerV1,
  clock: &mut RunClockV1,
  permit: &MigrationPreflightPermitV1,
  phase: MigrationPhaseV1,
  state: MigrationProgressStateV1,
  destination_header_sequence: u64,
  copied_through_write_sequence: u64,
  namespace_count: u64,
  entity_count: u64,
  copied_bytes: u64,
) -> Result<(), OfflineMigrationRunErrorV1> {
  let (updated, publication, monotonic) = clock.next()?;
  owner
    .transition_progress_after_restart(
      MigrationProgressTransitionRequestV1 {
        phase,
        state,
        flags: MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
        destination_header_sequence,
        copied_through_write_sequence,
        reconciled_through_publication_sequence: if matches!(
          phase,
          MigrationPhaseV1::Reconcile
            | MigrationPhaseV1::FinalFreeze
            | MigrationPhaseV1::DestinationVerify
            | MigrationPhaseV1::Cutover
            | MigrationPhaseV1::ReadOnlyValidation
            | MigrationPhaseV1::OperatorAcceptance
        ) {
          copied_through_write_sequence
        } else {
          0
        },
        namespace_count,
        entity_count,
        copied_bytes,
        updated_at_ms: updated,
        legacy_root_map_control_payload_hash: vec![0; permit.hash_algorithm().hash_length()],
        last_error_evidence: vec![0; permit.hash_algorithm().hash_length()],
        publication_timestamp_ms: publication,
        monotonic_now_ms: monotonic,
      },
      retirement,
    )
    .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  Ok(())
}

fn collect_inventory(
  request: &OfflineMigrationRunRequestV1<'_>,
  permit: &MigrationPreflightPermitV1,
  source: &Arc<StorageEngine>,
) -> Result<super::migration_v3_authority_inventory::V3MigrationAuthorityInventoryV1, OfflineMigrationRunErrorV1> {
  collect_v3_migration_authority_inventory_v1(V3MigrationAuthorityInventoryRequestV1 {
    source,
    database_id: permit.database_id(),
    source_physical_instance_id: permit.source_physical_instance_id(),
    cancellation: request.cancellation,
    acquisition_timeout: request.acquisition_timeout,
    limits: V3MigrationAuthorityInventoryLimitsV1 {
      maximum_roots: request.bounds.maximum_authority_roots,
      maximum_authority_records: request.bounds.maximum_authority_records,
      maximum_peers: request.bounds.maximum_authority_records,
      maximum_tasks: request.bounds.maximum_authority_records,
      maximum_plugins: request.bounds.maximum_authority_records,
      maximum_namespace_memory_bytes: request.bounds.maximum_memory_bytes,
      maximum_namespace_work_items: request.bounds.maximum_work_items,
      maximum_directory_depth: request.bounds.maximum_directory_depth as usize,
    },
  })
  .map_err(OfflineMigrationRunErrorV1::owned)
}

fn create_root_workspace(
  request: &OfflineMigrationRunRequestV1<'_>,
  permit: &MigrationPreflightPermitV1,
  memory: &MemoryCoordinator,
  publication_timestamp_ms: u64,
) -> Result<LegacyRootMapStagingWorkspaceV1, OfflineMigrationRunErrorV1> {
  let identity = LegacyRootMapWorkspaceIdentityV1::new(
    permit.database_id(),
    permit.migration_id(),
    permit.database_id(),
    permit.source_physical_instance_id(),
    permit.destination_physical_instance_id(),
    1,
    permit.hash_algorithm(),
  )
  .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  let options = LegacyRootMapWorkspaceOptionsV1::new(
    Some(request.workspace.to_path_buf()),
    request.bounds.root_map_maximum_stored_bytes,
    request.bounds.root_map_maximum_staged_rows,
    request.bounds.root_map_minimum_free_bytes,
    request.bounds.root_map_maximum_sort_memory_bytes,
    request.bounds.root_map_maximum_open_runs as usize,
    request.bounds.root_map_maximum_page_rows as usize,
    to_usize(request.bounds.root_map_maximum_publication_batch_bytes, "root-map publication bound")?,
  )
  .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  LegacyRootMapStagingWorkspaceV1::create(
    request.destination,
    identity,
    publication_timestamp_ms,
    options,
    request.cancellation.clone(),
    memory,
  )
  .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))
}

fn open_or_create_root_workspace(
  request: &OfflineMigrationRunRequestV1<'_>,
  permit: &MigrationPreflightPermitV1,
  memory: &MemoryCoordinator,
  publication_timestamp_ms: u64,
) -> Result<LegacyRootMapStagingWorkspaceV1, OfflineMigrationRunErrorV1> {
  if request.resume {
    match fs::symlink_metadata(root_workspace_path(request, permit)) {
      Ok(_) => return reopen_root_workspace(request, permit, memory),
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
      Err(error) => {
        return Err(OfflineMigrationRunErrorV1::new(
          "offline_migration_root_workspace_path",
          format!("failed to inspect the manifest-bound root-map workspace: {error}"),
        ));
      }
    }
  }
  create_root_workspace(request, permit, memory, publication_timestamp_ms)
}

fn reopen_root_workspace(
  request: &OfflineMigrationRunRequestV1<'_>,
  permit: &MigrationPreflightPermitV1,
  memory: &MemoryCoordinator,
) -> Result<LegacyRootMapStagingWorkspaceV1, OfflineMigrationRunErrorV1> {
  let identity = LegacyRootMapWorkspaceIdentityV1::new(
    permit.database_id(),
    permit.migration_id(),
    permit.database_id(),
    permit.source_physical_instance_id(),
    permit.destination_physical_instance_id(),
    1,
    permit.hash_algorithm(),
  )
  .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  let options = LegacyRootMapWorkspaceReopenOptionsV1::new(
    request.bounds.root_map_maximum_stored_bytes,
    request.bounds.root_map_maximum_staged_rows,
    request.bounds.root_map_minimum_free_bytes,
    request.bounds.root_map_maximum_sort_memory_bytes,
    request.bounds.root_map_maximum_open_runs as usize,
    request.bounds.root_map_maximum_page_rows as usize,
    to_usize(request.bounds.root_map_maximum_publication_batch_bytes, "root-map publication bound")?,
  )
  .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))?;
  let path = root_workspace_path(request, permit);
  LegacyRootMapStagingWorkspaceV1::reopen(&path, identity, options, request.cancellation.clone(), memory)
    .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))
}

fn root_workspace_path(request: &OfflineMigrationRunRequestV1<'_>, permit: &MigrationPreflightPermitV1) -> PathBuf {
  request.workspace.join(hex::encode(permit.database_id())).join(hex::encode(permit.migration_id())).join("root-map-0000000000000001")
}

fn initial_destination_root(permit: &MigrationPreflightPermitV1) -> Vec<u8> {
  digest_parts(permit.hash_algorithm(), &[b"dirc:"])
}

fn authority_template(
  permit: &MigrationPreflightPermitV1,
  base_predecessor_head: &[u8],
  clock: OfflineMigrationRunClockV1,
) -> Result<MigrationCaptureReplayAuthorityTemplateV1, OfflineMigrationRunErrorV1> {
  let required_capabilities = permit.required_reader_capabilities().into_bytes();
  let semantic_state = encode_semantic_state_object(
    &SemanticStateWriteV1 {
      required_capabilities,
      availability: SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured },
    },
    permit.hash_algorithm(),
  )
  .map_err(OfflineMigrationRunErrorV1::owned)?;
  Ok(MigrationCaptureReplayAuthorityTemplateV1 {
    base_predecessor_head: base_predecessor_head.to_vec(),
    semantic_state,
    required_capabilities,
    typed_closure_context: b"offline v3-to-v4 shadow migration".to_vec(),
    authority_identity: b"HEAD".to_vec(),
    publication_timestamp_floor_ms: clock.wall_time_ms,
    monotonic_timestamp_floor_ms: clock.monotonic_time_ms,
  })
}

fn open_root_map<'a>(
  destination: &'a V4FirstAuthorityPublisher,
  permit: &MigrationPreflightPermitV1,
  cancellation: &CancellationToken,
  memory: &MemoryCoordinator,
) -> Result<VerifiedLegacyRootMapReaderV1<'a>, OfflineMigrationRunErrorV1> {
  VerifiedLegacyRootMapReaderV1::open(destination, permit.database_id(), permit.migration_id(), cancellation, memory)
    .map_err(|error| OfflineMigrationRunErrorV1::new(error.code(), error.to_string()))
}

fn migration_memory(bounds: MigrationRunBoundsV1) -> Result<MemoryCoordinator, OfflineMigrationRunErrorV1> {
  // `maximum_memory_bytes` is the strict budget handed to each migration
  // algorithm.  The coordinator also retains small owner/root-map/journal
  // reservations while one such algorithm is active, so its aggregate soft
  // envelope must sit above a complete algorithm budget.
  let soft = bounds
    .maximum_memory_bytes
    .checked_mul(2)
    .ok_or_else(|| OfflineMigrationRunErrorV1::new("offline_migration_memory", "memory policy overflowed"))?;
  let hard = bounds
    .maximum_memory_bytes
    .checked_mul(3)
    .ok_or_else(|| OfflineMigrationRunErrorV1::new("offline_migration_memory", "memory policy overflowed"))?;
  let emergency = (bounds.maximum_memory_bytes / 4).max(1);
  MemoryPolicy::new(soft, hard, 1, emergency)
    .map(MemoryCoordinator::new)
    .map_err(|error| OfflineMigrationRunErrorV1::new("offline_migration_memory", error.to_string()))
}

fn publication_time(clock: &mut RunClockV1) -> Result<u64, OfflineMigrationRunErrorV1> {
  clock.next().map(|(_, publication, _)| publication)
}

fn checked_clock_offset(base_timestamp_ms: u64, steps: u64) -> Result<u64, OfflineMigrationRunErrorV1> {
  CLOCK_STEP_MS
    .checked_mul(steps)
    .and_then(|offset| base_timestamp_ms.checked_add(offset))
    .filter(|timestamp| *timestamp <= i64::MAX as u64)
    .ok_or_else(|| OfflineMigrationRunErrorV1::new("offline_migration_clock", "durable migration timestamp anchor overflowed"))
}

fn validate_identity(identity: OfflineMigrationRunIdentityV1) -> Result<(), OfflineMigrationRunErrorV1> {
  if [
    identity.database_id,
    identity.migration_id,
    identity.source_physical_instance_id,
    identity.destination_physical_instance_id,
    identity.holder_boot_id,
  ]
  .iter()
  .any(|value| value.iter().all(|byte| *byte == 0))
    || identity.source_physical_instance_id == identity.destination_physical_instance_id
  {
    return Err(OfflineMigrationRunErrorV1::new(
      "offline_migration_identity",
      "offline migration identities must be nonzero and physical incarnations must be distinct",
    ));
  }
  Ok(())
}

#[derive(Default)]
struct VerificationReceiptV1 {
  roots: u64,
  entities: u64,
  content_bytes: u64,
}

#[derive(Debug)]
struct DetachedVerificationMappingV1 {
  authority_identity: Vec<u8>,
  source_path: String,
  source_entry_type: EntryType,
  source_root: Vec<u8>,
  system_family_id: u16,
  destination_entity: Option<Vec<u8>>,
}

struct SealedRootMapClosureValidationSinkV1<'a> {
  workspace: &'a LegacyRootMapStagingWorkspaceV1,
  observed_mappings: u64,
  finished: bool,
}

impl<'a> SealedRootMapClosureValidationSinkV1<'a> {
  const fn new(workspace: &'a LegacyRootMapStagingWorkspaceV1) -> Self {
    Self { workspace, observed_mappings: 0, finished: false }
  }
}

impl MigrationFinalRootMappingSinkV1 for SealedRootMapClosureValidationSinkV1<'_> {
  fn record_root_mapping(&mut self, _mapping: &MigrationFinalRootMappingV1) -> crate::engine::EngineResult<()> {
    if self.finished {
      return Err(crate::engine::EngineError::InvalidInput("sealed root-map validation received a mapping after closure".to_string()));
    }
    self.observed_mappings = self
      .observed_mappings
      .checked_add(1)
      .ok_or_else(|| crate::engine::EngineError::ResourceExhausted("sealed root-map validation mapping count overflowed".to_string()))?;
    Ok(())
  }

  fn finish_root_mappings(&mut self, closure: &MigrationFinalRootMappingClosureV1) -> crate::engine::EngineResult<()> {
    if self.finished || self.observed_mappings != closure.mapping_count {
      return Err(crate::engine::EngineError::InvalidInput("sealed root-map validation closure is duplicate or incomplete".to_string()));
    }
    self.workspace.validate_sealed_final_closure(closure).map_err(|error| crate::engine::EngineError::InvalidInput(error.to_string()))?;
    self.finished = true;
    Ok(())
  }
}

enum OfflineFinalRootMapSinkV1<'a> {
  Append(LegacyRootMapProducerSinkV1<'a>),
  Validate(SealedRootMapClosureValidationSinkV1<'a>),
}

impl MigrationFinalRootMappingSinkV1 for OfflineFinalRootMapSinkV1<'_> {
  fn record_root_mapping(&mut self, mapping: &MigrationFinalRootMappingV1) -> crate::engine::EngineResult<()> {
    match self {
      Self::Append(sink) => sink.record_root_mapping(mapping),
      Self::Validate(sink) => sink.record_root_mapping(mapping),
    }
  }

  fn finish_root_mappings(&mut self, closure: &MigrationFinalRootMappingClosureV1) -> crate::engine::EngineResult<()> {
    match self {
      Self::Append(sink) => sink.finish_root_mappings(closure),
      Self::Validate(sink) => sink.finish_root_mappings(closure),
    }
  }
}

struct SelectedRootMapValidationSinkV1<'a, 'destination> {
  root_map: &'a VerifiedLegacyRootMapReaderV1<'destination>,
  workspace: &'a LegacyRootMapStagingWorkspaceV1,
  cancellation: &'a CancellationToken,
  observed_roots: HashSet<Vec<u8>>,
  finished: bool,
}

impl<'a, 'destination> SelectedRootMapValidationSinkV1<'a, 'destination> {
  fn new(
    root_map: &'a VerifiedLegacyRootMapReaderV1<'destination>,
    workspace: &'a LegacyRootMapStagingWorkspaceV1,
    cancellation: &'a CancellationToken,
  ) -> Self {
    Self { root_map, workspace, cancellation, observed_roots: HashSet::new(), finished: false }
  }
}

impl MigrationFinalRootMappingSinkV1 for SelectedRootMapValidationSinkV1<'_, '_> {
  fn record_root_mapping(&mut self, mapping: &MigrationFinalRootMappingV1) -> crate::engine::EngineResult<()> {
    if self.finished {
      return Err(crate::engine::EngineError::InvalidInput("selected root-map validation received a mapping after closure".to_string()));
    }
    if mapping.kind == super::migration_base_clone_execution::MigrationBaseCloneSeedKindV1::DetachedProtectedPath {
      if mapping.destination_namespace_root.is_some() || mapping.destination_tree_root.is_some() {
        return Err(crate::engine::EngineError::InvalidInput(
          "detached protected mapping unexpectedly identifies a NamespaceRoot".to_string(),
        ));
      }
      return Ok(());
    }
    let expected_namespace = mapping
      .destination_namespace_root
      .as_deref()
      .ok_or_else(|| crate::engine::EngineError::InvalidInput("resumed final mapping has no destination NamespaceRoot".to_string()))?;
    let selected = self
      .root_map
      .lookup(&mapping.source_root, self.cancellation)
      .map_err(|error| crate::engine::EngineError::InvalidInput(error.to_string()))?
      .ok_or_else(|| crate::engine::EngineError::InvalidInput("resumed final mapping is absent from the selected root map".to_string()))?;
    if selected.namespace_root_v1_hash != expected_namespace || selected.captured_source_write_sequence != mapping.source_write_sequence {
      return Err(crate::engine::EngineError::InvalidInput("resumed final mapping differs from the selected root map".to_string()));
    }
    self.observed_roots.insert(mapping.source_root.clone());
    Ok(())
  }

  fn finish_root_mappings(&mut self, closure: &MigrationFinalRootMappingClosureV1) -> crate::engine::EngineResult<()> {
    if self.finished || self.observed_roots.len() != self.root_map.record_count() as usize {
      return Err(crate::engine::EngineError::InvalidInput("selected root-map validation closure is duplicate or incomplete".to_string()));
    }
    self.workspace.validate_sealed_final_closure(closure).map_err(|error| crate::engine::EngineError::InvalidInput(error.to_string()))?;
    if closure.destination_namespace_root != self.root_map.destination_head() {
      return Err(crate::engine::EngineError::InvalidInput(
        "reproduced final closure differs from the selected destination HEAD".to_string(),
      ));
    }
    self.root_map.validate_selected_unchanged().map_err(|error| crate::engine::EngineError::InvalidInput(error.to_string()))?;
    self.finished = true;
    Ok(())
  }
}

struct VerificationMappingCaptureSinkV1<D> {
  delegate: D,
  mappings: Vec<DetachedVerificationMappingV1>,
  maximum_records: u64,
  maximum_memory_bytes: u64,
  used_memory_bytes: u64,
  finished: bool,
}

impl<D> VerificationMappingCaptureSinkV1<D> {
  fn new(delegate: D, maximum_records: u64, maximum_memory_bytes: u64) -> Self {
    Self { delegate, mappings: Vec::new(), maximum_records, maximum_memory_bytes, used_memory_bytes: 0, finished: false }
  }

  fn into_mappings(self) -> Result<Vec<DetachedVerificationMappingV1>, OfflineMigrationRunErrorV1> {
    if !self.finished {
      return Err(OfflineMigrationRunErrorV1::new(
        "offline_migration_detached_mapping_incomplete",
        "final reconciliation did not finish the detached verification mapping stream",
      ));
    }
    Ok(self.mappings)
  }
}

impl<D: MigrationFinalRootMappingSinkV1> MigrationFinalRootMappingSinkV1 for VerificationMappingCaptureSinkV1<D> {
  fn record_root_mapping(&mut self, mapping: &MigrationFinalRootMappingV1) -> crate::engine::EngineResult<()> {
    if mapping.kind == super::migration_base_clone_execution::MigrationBaseCloneSeedKindV1::DetachedProtectedPath {
      let family_id = mapping.system_family_id.ok_or_else(|| {
        crate::engine::EngineError::InvalidInput("detached verification mapping has no SystemFamily identity".to_string())
      })?;
      let next_count = (self.mappings.len() as u64)
        .checked_add(1)
        .ok_or_else(|| crate::engine::EngineError::ResourceExhausted("detached verification mapping count overflowed".to_string()))?;
      if next_count > self.maximum_records {
        return Err(crate::engine::EngineError::ResourceExhausted(format!(
          "detached verification mapping count exceeds configured maximum {}",
          self.maximum_records
        )));
      }
      let charge = size_of::<DetachedVerificationMappingV1>()
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(mapping.authority_identity.len()))
        .and_then(|bytes| bytes.checked_add(mapping.source_path.len()))
        .and_then(|bytes| bytes.checked_add(mapping.source_root.len()))
        .and_then(|bytes| bytes.checked_add(mapping.destination_entity.as_ref().map_or(0, Vec::len)))
        .and_then(|bytes| bytes.checked_add(4 * 64))
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or_else(|| {
          crate::engine::EngineError::ResourceExhausted("detached verification mapping memory estimate overflowed".to_string())
        })?;
      let next_memory = self.used_memory_bytes.checked_add(charge).ok_or_else(|| {
        crate::engine::EngineError::ResourceExhausted("detached verification mapping memory accounting overflowed".to_string())
      })?;
      if next_memory > self.maximum_memory_bytes {
        return Err(crate::engine::EngineError::ResourceExhausted(format!(
          "detached verification mappings require {next_memory} bytes but their bound is {}",
          self.maximum_memory_bytes
        )));
      }
      self.mappings.try_reserve(1).map_err(|error| {
        crate::engine::EngineError::ResourceExhausted(format!("detached verification mapping allocation failed: {error}"))
      })?;
      self.mappings.push(DetachedVerificationMappingV1 {
        authority_identity: mapping.authority_identity.clone(),
        source_path: mapping.source_path.clone(),
        source_entry_type: mapping.source_entry_type,
        source_root: mapping.source_root.clone(),
        system_family_id: family_id,
        destination_entity: mapping.destination_entity.clone(),
      });
      self.used_memory_bytes = next_memory;
    }
    self.delegate.record_root_mapping(mapping)
  }

  fn finish_root_mappings(&mut self, closure: &MigrationFinalRootMappingClosureV1) -> crate::engine::EngineResult<()> {
    if self.finished || self.mappings.len() as u64 != closure.seed_counts.detached_protected {
      return Err(crate::engine::EngineError::InvalidInput("detached verification mapping closure is duplicate or incomplete".to_string()));
    }
    self.delegate.finish_root_mappings(closure)?;
    self.finished = true;
    Ok(())
  }
}

struct DestinationVerifierV1<'a> {
  permit: &'a MigrationPreflightPermitV1,
  source: &'a StorageEngine,
  destination: &'a V4FirstAuthorityPublisher,
  policy: SystemFamilyPolicyResolverV1,
  cancellation: &'a CancellationToken,
  maximum_work_items: u64,
  maximum_entity_bytes: usize,
  maximum_decoded_chunk_bytes: usize,
  maximum_directory_depth: usize,
  work_items: u64,
  visited_destination: HashSet<Vec<u8>>,
  counted_destination_files: HashSet<Vec<u8>>,
  active_source_directories: HashSet<Vec<u8>>,
  active_destination_directories: HashSet<Vec<u8>>,
  content_bytes: u64,
}

#[allow(clippy::too_many_arguments)]
fn verify_destination(
  permit: &MigrationPreflightPermitV1,
  source: &StorageEngine,
  destination: &V4FirstAuthorityPublisher,
  root_map: &VerifiedLegacyRootMapReaderV1<'_>,
  inventory: &mut dyn super::migration_final_authority_reconciliation::MigrationFinalAuthorityInventorySourceV1,
  closure: &MigrationFinalRootMappingClosureV1,
  detached_mappings: &[DetachedVerificationMappingV1],
  bounds: MigrationRunBoundsV1,
  cancellation: &CancellationToken,
  memory: &MemoryCoordinator,
) -> Result<VerificationReceiptV1, OfflineMigrationRunErrorV1> {
  let _reservation = memory
    .reserve(
      crate::engine::memory_coordinator::MemoryOwner::Migration,
      bounds.maximum_memory_bytes,
      crate::engine::memory_coordinator::AdmissionClass::Maintenance,
    )
    .map_err(OfflineMigrationRunErrorV1::owned)?;
  let observation = destination.observe().map_err(OfflineMigrationRunErrorV1::owned)?;
  if observation.selected.redundancy_degraded
    || observation.selected.header.database_id != permit.database_id()
    || observation.selected.header.physical_instance_id != permit.destination_physical_instance_id()
    || observation.selected.header.head_hash != closure.destination_namespace_root
    || observation.selected.header.slot_sequence < closure.destination_header_sequence
  {
    return Err(OfflineMigrationRunErrorV1::new(
      "offline_migration_verify_destination",
      "destination header identity, redundancy, sequence, or HEAD differs from the final reconciliation closure",
    ));
  }
  let mut verifier = DestinationVerifierV1 {
    permit,
    source,
    destination,
    policy: SystemFamilyPolicyResolverV1::embedded(permit.hash_algorithm()).map_err(OfflineMigrationRunErrorV1::owned)?,
    cancellation,
    maximum_work_items: bounds.maximum_work_items,
    maximum_entity_bytes: to_usize(bounds.maximum_decoded_chunk_bytes, "verification entity bound")?,
    maximum_decoded_chunk_bytes: to_usize(bounds.maximum_decoded_chunk_bytes, "verification chunk bound")?,
    maximum_directory_depth: bounds.maximum_directory_depth as usize,
    work_items: 0,
    visited_destination: HashSet::new(),
    counted_destination_files: HashSet::new(),
    active_source_directories: HashSet::new(),
    active_destination_directories: HashSet::new(),
    content_bytes: 0,
  };
  let mut roots = HashSet::new();
  let mut detached = detached_mappings.iter();
  while let Some(seed) = inventory.next_seed().map_err(OfflineMigrationRunErrorV1::owned)? {
    verifier.work()?;
    if seed.seed.kind == super::migration_base_clone_execution::MigrationBaseCloneSeedKindV1::DetachedProtectedPath {
      if root_map.lookup(&seed.seed.hash, cancellation).map_err(OfflineMigrationRunErrorV1::owned)?.is_some() {
        return Err(OfflineMigrationRunErrorV1::new(
          "offline_migration_verify_detached_mapping",
          "detached protected source state unexpectedly has a legacy root mapping",
        ));
      }
      let mapping = detached.next().ok_or_else(|| {
        OfflineMigrationRunErrorV1::new(
          "offline_migration_verify_detached_mapping",
          "fresh detached authority has no final reconciliation mapping",
        )
      })?;
      verifier.verify_detached(&seed, mapping)?;
      continue;
    }
    let row = root_map.lookup(&seed.seed.hash, cancellation).map_err(OfflineMigrationRunErrorV1::owned)?.ok_or_else(|| {
      OfflineMigrationRunErrorV1::new(
        "offline_migration_verify_root_missing",
        format!("retained source root {} has no selected mapping", hex::encode(&seed.seed.hash)),
      )
    })?;
    if roots.insert(seed.seed.hash.clone()) {
      verifier.verify_root(&seed.seed.hash, &row)?;
    }
  }
  if detached.next().is_some() {
    return Err(OfflineMigrationRunErrorV1::new(
      "offline_migration_verify_detached_mapping",
      "final reconciliation retained a detached mapping absent from fresh authority",
    ));
  }
  let inventory_closure = inventory.finish().map_err(OfflineMigrationRunErrorV1::owned)?;
  if inventory_closure.database_id != closure.database_id
    || inventory_closure.source_physical_instance_id != closure.source_physical_instance_id
    || inventory_closure.source_header_sequence != closure.source_header_sequence
    || inventory_closure.frozen_source_root != closure.frozen_source_root
    || inventory_closure.frozen_source_publication_sequence != closure.frozen_source_publication_sequence
    || inventory_closure.authority_digest != closure.authority_digest
    || inventory_closure.system_family_registry_fingerprint != closure.system_family_registry_fingerprint
    || root_map.record_count() as usize != roots.len()
  {
    return Err(OfflineMigrationRunErrorV1::new(
      "offline_migration_verify_authority",
      "fresh source authority, final reconciliation closure, and selected root map do not identify one complete root set",
    ));
  }
  root_map.validate_selected_unchanged().map_err(OfflineMigrationRunErrorV1::owned)?;
  Ok(VerificationReceiptV1 {
    roots: roots.len() as u64,
    entities: verifier.visited_destination.len() as u64,
    content_bytes: verifier.content_bytes,
  })
}

impl DestinationVerifierV1<'_> {
  fn work(&mut self) -> Result<(), OfflineMigrationRunErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(OfflineMigrationRunErrorV1::new("offline_migration_verify_cancelled", "destination verification was cancelled"));
    }
    self.work_items = self
      .work_items
      .checked_add(1)
      .ok_or_else(|| OfflineMigrationRunErrorV1::new("offline_migration_verify_work", "verification work counter overflowed"))?;
    if self.work_items > self.maximum_work_items {
      return Err(OfflineMigrationRunErrorV1::new(
        "offline_migration_verify_work",
        "destination verification exceeded its work-item bound",
      ));
    }
    Ok(())
  }

  fn verify_root(&mut self, source_root: &[u8], row: &LegacyRootMapRowV1) -> Result<(), OfflineMigrationRunErrorV1> {
    let namespace = self.load_destination(&row.namespace_root_v1_hash, Some(EntryTypeV4::DirectoryIndex))?;
    if namespace.entry_type != EntryTypeV4::DirectoryIndex {
      return Err(OfflineMigrationRunErrorV1::new(
        "offline_migration_verify_namespace",
        "legacy root mapping does not resolve to a NamespaceRoot entity",
      ));
    }
    let decoded =
      decode_namespace_root(&namespace.stored_value, self.permit.hash_algorithm()).map_err(OfflineMigrationRunErrorV1::owned)?;
    if decoded.root_hash != row.namespace_root_v1_hash {
      return Err(OfflineMigrationRunErrorV1::new(
        "offline_migration_verify_namespace",
        "mapped NamespaceRoot bytes differ from their selected identity",
      ));
    }
    self.verify_directory("/", source_root, &decoded.namespace_tree_root, 0, None)
  }

  fn verify_detached(
    &mut self,
    seed: &super::migration_final_authority_reconciliation::MigrationFinalAuthoritySeedV1,
    mapping: &DetachedVerificationMappingV1,
  ) -> Result<(), OfflineMigrationRunErrorV1> {
    if mapping.authority_identity != seed.authority_identity
      || mapping.source_path != seed.seed.path
      || mapping.source_entry_type != seed.seed.entry_type
      || mapping.source_root != seed.seed.hash
      || Some(mapping.system_family_id) != seed.system_family_id
    {
      return Err(OfflineMigrationRunErrorV1::new(
        "offline_migration_verify_detached_mapping",
        "fresh detached authority differs from its final reconciliation mapping",
      ));
    }
    let policy = self
      .policy
      .policy(SystemFamilySubjectV1::Path(&seed.seed.path), "migration detached destination verification")
      .map_err(OfflineMigrationRunErrorV1::owned)?;
    let migration_policy = match policy {
      SystemFamilyPolicyDecisionV1::Known { family_id, policy } if family_id == mapping.system_family_id => policy.migration_policy,
      _ => {
        return Err(OfflineMigrationRunErrorV1::new(
          "offline_migration_verify_detached_family",
          "detached verification mapping no longer selects its recorded SystemFamily",
        ));
      }
    };
    if migration_policy != MigrationPolicyV1::RequiredCopy {
      if mapping.destination_entity.is_some() {
        return Err(OfflineMigrationRunErrorV1::new(
          "offline_migration_verify_detached_omission",
          "policy-omitted detached authority unexpectedly selected a destination entity",
        ));
      }
      return Ok(());
    }
    let destination = mapping.destination_entity.as_deref().ok_or_else(|| {
      OfflineMigrationRunErrorV1::new(
        "offline_migration_verify_detached_missing",
        format!("required detached path '{}' has no destination entity", seed.seed.path),
      )
    })?;
    match seed.seed.entry_type {
      EntryType::DirectoryIndex => self.verify_directory(&seed.seed.path, &seed.seed.hash, destination, 0, None),
      EntryType::FileRecord => self.verify_file(&seed.seed.path, &seed.seed.hash, destination),
      EntryType::Symlink => self.verify_leaf(&seed.seed.path, &seed.seed.hash, destination, EntryTypeV4::Symlink),
      other => Err(OfflineMigrationRunErrorV1::new(
        "offline_migration_verify_detached_type",
        format!("unsupported detached entry {other:?} at '{}'", seed.seed.path),
      )),
    }
  }

  fn verify_directory(
    &mut self,
    path: &str,
    source_hash: &[u8],
    destination_hash: &[u8],
    depth: usize,
    declared_destination_size: Option<u64>,
  ) -> Result<(), OfflineMigrationRunErrorV1> {
    self.work()?;
    if depth > self.maximum_directory_depth
      || !self.active_source_directories.insert(source_hash.to_vec())
      || !self.active_destination_directories.insert(destination_hash.to_vec())
    {
      return Err(OfflineMigrationRunErrorV1::new(
        "offline_migration_verify_directory",
        format!("directory cycle or depth limit at {path}"),
      ));
    }
    let source_children = self.source_children(source_hash)?;
    let (destination_children, destination_encoded_size) = self.destination_children(destination_hash)?;
    if declared_destination_size.is_some_and(|declared| declared != destination_encoded_size) {
      return Err(OfflineMigrationRunErrorV1::new(
        "offline_migration_verify_metadata",
        format!("destination directory size metadata differs from its encoded body at {path}"),
      ));
    }
    let destination_by_name: HashMap<&str, &ChildEntry> = destination_children.iter().map(|child| (child.name.as_str(), child)).collect();
    if destination_by_name.len() != destination_children.len() {
      return Err(OfflineMigrationRunErrorV1::new(
        "offline_migration_verify_directory",
        format!("destination directory {path} contains duplicate child names"),
      ));
    }
    let mut expected_names = Vec::new();
    for source_child in &source_children {
      self.work()?;
      let child_path = if path == "/" { format!("/{}", source_child.name) } else { format!("{path}/{}", source_child.name) };
      let policy = self
        .policy
        .policy(SystemFamilySubjectV1::Path(&child_path), "migration destination verification")
        .map_err(OfflineMigrationRunErrorV1::owned)?;
      let copied = matches!(
        policy,
        SystemFamilyPolicyDecisionV1::Ordinary
          | SystemFamilyPolicyDecisionV1::Known {
            policy: super::system_family::SystemFamilyPolicyV1 { migration_policy: MigrationPolicyV1::RequiredCopy, .. },
            ..
          }
      );
      let structural = matches!(policy, SystemFamilyPolicyDecisionV1::StructuralContainer);
      if !copied && !structural {
        if destination_by_name.contains_key(source_child.name.as_str()) {
          return Err(OfflineMigrationRunErrorV1::new(
            "offline_migration_verify_policy",
            format!("destination retained policy-omitted path {child_path}"),
          ));
        }
        continue;
      }
      if structural && source_child.entry_type != EntryType::DirectoryIndex.to_u8() {
        return Err(OfflineMigrationRunErrorV1::new(
          "offline_migration_verify_policy",
          format!("structural source path {child_path} is not a directory"),
        ));
      }
      let Some(destination_child) = destination_by_name.get(source_child.name.as_str()).copied() else {
        if structural {
          self.verify_absent_structural_directory(&child_path, &source_child.hash, depth + 1)?;
          continue;
        }
        return Err(OfflineMigrationRunErrorV1::new(
          "offline_migration_verify_child_missing",
          format!("copied source path {child_path} is absent from the destination"),
        ));
      };
      expected_names.push(source_child.name.as_str());
      let entry_type = EntryType::from_u8(source_child.entry_type).map_err(OfflineMigrationRunErrorV1::owned)?;
      self.compare_child_metadata(&child_path, source_child, destination_child, entry_type)?;
      match entry_type {
        EntryType::DirectoryIndex => {
          self.verify_directory(&child_path, &source_child.hash, &destination_child.hash, depth + 1, Some(destination_child.total_size))?
        }
        EntryType::FileRecord => self.verify_file(&child_path, &source_child.hash, &destination_child.hash)?,
        EntryType::Symlink => self.verify_leaf(&child_path, &source_child.hash, &destination_child.hash, EntryTypeV4::Symlink)?,
        other => {
          return Err(OfflineMigrationRunErrorV1::new(
            "offline_migration_verify_type",
            format!("unsupported namespace entry {other:?} at {child_path}"),
          ));
        }
      }
    }
    if expected_names.len() != destination_children.len() {
      return Err(OfflineMigrationRunErrorV1::new(
        "offline_migration_verify_extra_child",
        format!("destination directory {path} contains an unexplained child"),
      ));
    }
    self.active_source_directories.remove(source_hash);
    self.active_destination_directories.remove(destination_hash);
    Ok(())
  }

  fn verify_absent_structural_directory(&mut self, path: &str, source_hash: &[u8], depth: usize) -> Result<(), OfflineMigrationRunErrorV1> {
    self.work()?;
    if depth > self.maximum_directory_depth || !self.active_source_directories.insert(source_hash.to_vec()) {
      return Err(OfflineMigrationRunErrorV1::new(
        "offline_migration_verify_directory",
        format!("source structural directory cycle or depth limit at {path}"),
      ));
    }
    for child in self.source_children(source_hash)? {
      self.work()?;
      let child_path = format!("{path}/{}", child.name);
      let policy = self
        .policy
        .policy(SystemFamilySubjectV1::Path(&child_path), "migration absent structural verification")
        .map_err(OfflineMigrationRunErrorV1::owned)?;
      match policy {
        SystemFamilyPolicyDecisionV1::Ordinary
        | SystemFamilyPolicyDecisionV1::Known {
          policy: super::system_family::SystemFamilyPolicyV1 { migration_policy: MigrationPolicyV1::RequiredCopy, .. },
          ..
        } => {
          return Err(OfflineMigrationRunErrorV1::new(
            "offline_migration_verify_child_missing",
            format!("absent structural destination path {path} contains required descendant {child_path}"),
          ));
        }
        SystemFamilyPolicyDecisionV1::StructuralContainer => {
          if child.entry_type != EntryType::DirectoryIndex.to_u8() {
            return Err(OfflineMigrationRunErrorV1::new(
              "offline_migration_verify_policy",
              format!("structural source path {child_path} is not a directory"),
            ));
          }
          self.verify_absent_structural_directory(&child_path, &child.hash, depth + 1)?;
        }
        SystemFamilyPolicyDecisionV1::Known { .. } => {}
      }
    }
    self.active_source_directories.remove(source_hash);
    Ok(())
  }

  fn compare_child_metadata(
    &self,
    path: &str,
    source: &ChildEntry,
    destination: &ChildEntry,
    entry_type: EntryType,
  ) -> Result<(), OfflineMigrationRunErrorV1> {
    let translated_metadata_matches = if entry_type == EntryType::DirectoryIndex {
      source.content_type.is_none() && destination.content_type.is_none()
    } else {
      source.total_size == destination.total_size && source.content_type == destination.content_type
    };
    if source.entry_type != destination.entry_type
      || source.created_at != destination.created_at
      || source.updated_at != destination.updated_at
      || source.name != destination.name
      || source.virtual_time != destination.virtual_time
      || source.node_id != destination.node_id
      || !translated_metadata_matches
    {
      return Err(OfflineMigrationRunErrorV1::new(
        "offline_migration_verify_metadata",
        format!("source and destination metadata differ at {path}"),
      ));
    }
    Ok(())
  }

  fn verify_file(&mut self, path: &str, source_hash: &[u8], destination_hash: &[u8]) -> Result<(), OfflineMigrationRunErrorV1> {
    let (source_header, source_key, source_value) = self.load_source(source_hash, EntryType::FileRecord)?;
    let destination = self.load_destination(destination_hash, Some(EntryTypeV4::FileRecord))?;
    let source_record = FileRecord::deserialize(&source_value, self.permit.hash_algorithm().hash_length(), source_header.entry_version)
      .map_err(OfflineMigrationRunErrorV1::owned)?;
    let destination_record =
      FileRecord::deserialize(&destination.stored_value, self.permit.hash_algorithm().hash_length(), destination.entity_version)
        .map_err(OfflineMigrationRunErrorV1::owned)?;
    if source_key != source_hash
      || source_record.path != path
      || source_record.path != destination_record.path
      || source_record.content_type != destination_record.content_type
      || source_record.total_size != destination_record.total_size
      || source_record.created_at != destination_record.created_at
      || source_record.updated_at != destination_record.updated_at
      || source_record.metadata != destination_record.metadata
      || source_record.content_hash != destination_record.content_hash
      || source_record.chunk_hashes.len() != destination_record.chunk_hashes.len()
    {
      return Err(OfflineMigrationRunErrorV1::new(
        "offline_migration_verify_file",
        format!("source and destination FileRecord differ at {path}"),
      ));
    }
    let first_visit = self.counted_destination_files.insert(destination_hash.to_vec());
    let mut total = 0u64;
    let mut content = self.permit.hash_algorithm().incremental_hasher().map_err(OfflineMigrationRunErrorV1::owned)?;
    for (source_chunk, destination_chunk) in source_record.chunk_hashes.iter().zip(&destination_record.chunk_hashes) {
      self.work()?;
      let (source_header, _, stored) = self.load_source(source_chunk, EntryType::Chunk)?;
      let decoded = decompress_bounded(&stored, source_header.compression_algo, self.maximum_decoded_chunk_bytes)
        .map_err(OfflineMigrationRunErrorV1::owned)?;
      let destination = self.load_destination(destination_chunk, Some(EntryTypeV4::Chunk))?;
      if destination.compression_algorithm != CompressionAlgorithm::None || destination.stored_value != decoded {
        return Err(OfflineMigrationRunErrorV1::new(
          "offline_migration_verify_chunk",
          format!("source and destination chunk bytes differ at {path}"),
        ));
      }
      content.update(&decoded);
      total = total
        .checked_add(decoded.len() as u64)
        .ok_or_else(|| OfflineMigrationRunErrorV1::new("offline_migration_verify_size", "verified file size overflowed"))?;
    }
    let content_hash = content.finalize();
    if total != source_record.total_size || (!source_record.content_hash.is_empty() && source_record.content_hash != content_hash) {
      return Err(OfflineMigrationRunErrorV1::new(
        "offline_migration_verify_content",
        format!("verified file size or content hash differs at {path}"),
      ));
    }
    if first_visit {
      self.content_bytes = self
        .content_bytes
        .checked_add(total)
        .ok_or_else(|| OfflineMigrationRunErrorV1::new("offline_migration_verify_size", "verified content byte count overflowed"))?;
    }
    Ok(())
  }

  fn verify_leaf(
    &mut self,
    path: &str,
    source_hash: &[u8],
    destination_hash: &[u8],
    expected: EntryTypeV4,
  ) -> Result<(), OfflineMigrationRunErrorV1> {
    let (_, _, source_value) = self.load_source(source_hash, EntryType::Symlink)?;
    let destination = self.load_destination(destination_hash, Some(expected))?;
    if source_value != destination.stored_value {
      return Err(OfflineMigrationRunErrorV1::new(
        "offline_migration_verify_leaf",
        format!("source and destination leaf bytes differ at {path}"),
      ));
    }
    Ok(())
  }

  fn source_children(&mut self, root: &[u8]) -> Result<Vec<ChildEntry>, OfflineMigrationRunErrorV1> {
    let (header, _, value) = self.load_source(root, EntryType::DirectoryIndex)?;
    self.flatten_source_directory(&value, header.entry_version)
  }

  fn flatten_source_directory(&mut self, value: &[u8], version: u8) -> Result<Vec<ChildEntry>, OfflineMigrationRunErrorV1> {
    if !is_btree_format(value) {
      return deserialize_child_entries(value, self.permit.hash_algorithm().hash_length(), version)
        .map_err(OfflineMigrationRunErrorV1::owned);
    }
    let mut pending = vec![(value.to_vec(), version)];
    let mut children = Vec::new();
    while let Some((value, version)) = pending.pop() {
      self.work()?;
      match BTreeNode::deserialize(&value, self.permit.hash_algorithm().hash_length(), version)
        .map_err(OfflineMigrationRunErrorV1::owned)?
      {
        BTreeNode::Leaf(leaf) => children.extend(leaf.entries),
        BTreeNode::Internal(internal) => {
          for hash in internal.children.into_iter().rev() {
            let (header, _, value) = self.load_source(&hash, EntryType::DirectoryIndex)?;
            pending.push((value, header.entry_version));
          }
        }
      }
      if children.len() as u64 > self.maximum_work_items {
        return Err(OfflineMigrationRunErrorV1::new("offline_migration_verify_work", "source directory exceeds work bound"));
      }
    }
    Ok(children)
  }

  fn destination_children(&mut self, root: &[u8]) -> Result<(Vec<ChildEntry>, u64), OfflineMigrationRunErrorV1> {
    let entity = self.load_destination(root, Some(EntryTypeV4::DirectoryIndex))?;
    let encoded_size = u64::try_from(entity.stored_value.len())
      .map_err(|error| OfflineMigrationRunErrorV1::new("offline_migration_verify_size", error.to_string()))?;
    if !is_btree_format(&entity.stored_value) {
      let children = deserialize_child_entries(&entity.stored_value, self.permit.hash_algorithm().hash_length(), entity.entity_version)
        .map_err(OfflineMigrationRunErrorV1::owned)?;
      return Ok((children, encoded_size));
    }
    let mut pending = vec![(entity.stored_value, entity.entity_version)];
    let mut children = Vec::new();
    while let Some((value, version)) = pending.pop() {
      self.work()?;
      match BTreeNode::deserialize(&value, self.permit.hash_algorithm().hash_length(), version)
        .map_err(OfflineMigrationRunErrorV1::owned)?
      {
        BTreeNode::Leaf(leaf) => children.extend(leaf.entries),
        BTreeNode::Internal(internal) => {
          for hash in internal.children.into_iter().rev() {
            let entity = self.load_destination(&hash, Some(EntryTypeV4::DirectoryIndex))?;
            pending.push((entity.stored_value, entity.entity_version));
          }
        }
      }
      if children.len() as u64 > self.maximum_work_items {
        return Err(OfflineMigrationRunErrorV1::new("offline_migration_verify_work", "destination directory exceeds work bound"));
      }
    }
    Ok((children, encoded_size))
  }

  fn load_source(
    &mut self,
    hash: &[u8],
    expected: EntryType,
  ) -> Result<crate::engine::storage_engine::EntryData, OfflineMigrationRunErrorV1> {
    self.work()?;
    let maximum = u32::try_from(self.maximum_entity_bytes.min(u32::MAX as usize))
      .map_err(|error| OfflineMigrationRunErrorV1::new("offline_migration_verify_bound", error.to_string()))?;
    let entry =
      self.source.get_entry_including_deleted_verified_bounded(hash, maximum).map_err(OfflineMigrationRunErrorV1::owned)?.ok_or_else(
        || {
          OfflineMigrationRunErrorV1::new(
            "offline_migration_verify_source_missing",
            format!("source entity {} is missing", hex::encode(hash)),
          )
        },
      )?;
    if entry.0.entry_type != expected || entry.1 != hash {
      return Err(OfflineMigrationRunErrorV1::new(
        "offline_migration_verify_source_entity",
        format!("source entity {} has unexpected type or key", hex::encode(hash)),
      ));
    }
    Ok(entry)
  }

  fn load_destination(
    &mut self,
    hash: &[u8],
    expected: Option<EntryTypeV4>,
  ) -> Result<super::first_authority::LoadedImmutableEntityV1, OfflineMigrationRunErrorV1> {
    self.work()?;
    let entity = self
      .destination
      .load_immutable_entity_bounded(hash, self.maximum_entity_bytes)
      .map_err(OfflineMigrationRunErrorV1::owned)?
      .ok_or_else(|| {
        OfflineMigrationRunErrorV1::new(
          "offline_migration_verify_destination_missing",
          format!("destination entity {} is missing", hex::encode(hash)),
        )
      })?;
    if entity.key != hash || expected.is_some_and(|expected| entity.entry_type != expected) {
      return Err(OfflineMigrationRunErrorV1::new(
        "offline_migration_verify_destination_entity",
        format!("destination entity {} has unexpected type or key", hex::encode(hash)),
      ));
    }
    self.visited_destination.insert(hash.to_vec());
    Ok(entity)
  }
}

fn file_blake3(path: &Path, cancellation: &CancellationToken) -> Result<[u8; 32], OfflineMigrationRunErrorV1> {
  let metadata =
    fs::symlink_metadata(path).map_err(|error| OfflineMigrationRunErrorV1::new("offline_migration_source_changed", error.to_string()))?;
  if metadata.file_type().is_symlink() || !metadata.is_file() {
    return Err(OfflineMigrationRunErrorV1::new("offline_migration_source_changed", "source is no longer a no-follow regular file"));
  }
  let mut file =
    File::open(path).map_err(|error| OfflineMigrationRunErrorV1::new("offline_migration_source_changed", error.to_string()))?;
  let mut hasher = blake3::Hasher::new();
  let mut buffer = [0u8; SOURCE_CHECKSUM_BUFFER_BYTES];
  loop {
    if cancellation.is_cancelled() {
      return Err(OfflineMigrationRunErrorV1::new("offline_migration_cancelled", "offline migration was cancelled"));
    }
    let read =
      file.read(&mut buffer).map_err(|error| OfflineMigrationRunErrorV1::new("offline_migration_source_changed", error.to_string()))?;
    if read == 0 {
      return Ok(*hasher.finalize().as_bytes());
    }
    hasher.update(&buffer[..read]);
  }
}

fn to_usize(value: u64, label: &'static str) -> Result<usize, OfflineMigrationRunErrorV1> {
  usize::try_from(value).map_err(|error| OfflineMigrationRunErrorV1::new("offline_migration_bound", format!("{label}: {error}")))
}
