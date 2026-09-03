//! Real, bounded evidence collection for one offline v3-to-v4 migration run.
//!
//! This adapter composes the existing read-only header, recovery, verifier,
//! authority-inventory, configuration, memory, capacity, native-durability,
//! and binary-identity producers. It creates no workspace or destination and
//! does not activate migration, service, cutover, or source-write authority.

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use super::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use super::contract_generated::CONTRACT_REGISTRY_SHA256;
use super::database_header::{ReadOnlyDatabaseHeader, read_database_header_read_only};
use super::deployment_guard::inspect_deployment_transition_state_read_only;
use super::hash::digest_parts;
use super::migration_destination::{migration_native_path_digest_v1, observe_migration_destination_path_v1};
use super::migration_preflight::{
  CapacityRoleV1, MigrationBinaryEvidenceV1, MigrationCapacityObservationV1, MigrationConfigurationEvidenceV1, MigrationIdentityEvidenceV1,
  MigrationMemoryEvidenceV1, MigrationNativeEvidenceV1, MigrationPreflightPermitV1, MigrationPreflightReportV1,
  MigrationPreflightRequestV1, MigrationRecoveryEvidenceV1, MigrationSourceEvidenceV1, NativeCutoverCapabilitiesV1,
  StrictVerificationEvidenceV1, admit_migration_preflight_v1,
};
use super::migration_run_manifest::{MigrationRunBoundsV1, MigrationRunManifestV1};
use super::migration_v3_authority_inventory::{
  V3MigrationAuthorityInventoryLimitsV1, V3MigrationAuthorityInventoryRequestV1, collect_v3_migration_authority_inventory_v1,
};
use super::private_workspace::is_canonical_lexical_absolute_utf8_path;
use super::system_family::embedded_system_family_registry;
use crate::engine::config_resolver::CommandLineConfigOverrides;
use crate::engine::file_header::FILE_HEADER_SIZE;
use crate::engine::native_durability::{
  PlatformFileIdentityDescriptorV1, platform_file_identity, platform_file_identity_from_file, probe_native_durability,
};
use crate::engine::verify::verify_checked;
use crate::engine::StorageEngine;

const CHECKSUM_BUFFER_BYTES: usize = 1024 * 1024;
const CONFIGURATION_FINGERPRINT_DOMAIN_V1: &[u8] = b"aeordb.effective-migration-configuration.v1\0";

pub struct OfflineMigrationPreflightRequestV1<'a> {
  pub source: &'a Path,
  pub destination: &'a Path,
  pub workspace: &'a Path,
  pub executable: &'a Path,
  pub source_commit: [u8; 20],
  pub database_id: [u8; 16],
  pub migration_id: [u8; 16],
  pub source_physical_instance_id: [u8; 16],
  pub destination_physical_instance_id: [u8; 16],
  pub configuration_overrides: CommandLineConfigOverrides,
  pub bounds: MigrationRunBoundsV1,
  pub acquisition_timeout: Duration,
  pub cancellation: &'a CancellationToken,
  /// Present only after the immutable manifest has been opened and its live
  /// host bindings have been revalidated.
  pub resume_manifest: Option<&'a MigrationRunManifestV1>,
}

#[derive(Clone, Debug)]
pub struct OfflineMigrationPreflightV1 {
  report: MigrationPreflightReportV1,
  permit: MigrationPreflightPermitV1,
}

impl OfflineMigrationPreflightV1 {
  pub const fn report(&self) -> &MigrationPreflightReportV1 {
    &self.report
  }

  pub const fn permit(&self) -> &MigrationPreflightPermitV1 {
    &self.permit
  }
}

#[derive(Debug)]
pub struct OfflineMigrationPreflightErrorV1 {
  code: &'static str,
  message: String,
}

impl OfflineMigrationPreflightErrorV1 {
  pub const fn code(&self) -> &'static str {
    self.code
  }

  fn new(code: &'static str, message: impl Into<String>) -> Self {
    Self { code, message: message.into() }
  }
}

impl Display for OfflineMigrationPreflightErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.message)
  }
}

impl Error for OfflineMigrationPreflightErrorV1 {}

struct SourceObservationV1 {
  canonical_path: PathBuf,
  file_identity: PlatformFileIdentityDescriptorV1,
  evidence: MigrationSourceEvidenceV1,
}

struct DestinationBindingV1 {
  path_digest: [u8; 32],
  parent: PathBuf,
  parent_identity: PlatformFileIdentityDescriptorV1,
}

struct OpenSourceEvidenceV1 {
  verification: StrictVerificationEvidenceV1,
  inventory: super::migration_preflight::SourceAuthorityInventoryV1,
  memory: MigrationMemoryEvidenceV1,
  configuration: MigrationConfigurationEvidenceV1,
}

pub fn collect_offline_migration_preflight_v1(
  request: OfflineMigrationPreflightRequestV1<'_>,
) -> Result<OfflineMigrationPreflightV1, OfflineMigrationPreflightErrorV1> {
  check_cancelled(request.cancellation)?;
  validate_request_identity(&request)?;
  let source = observe_source(request.source, request.cancellation)?;
  let destination = observe_destination(&request, &source.canonical_path)?;
  let workspace_capacity_root = observe_workspace_capacity_root(request.workspace)?;
  let recovery_before = inspect_deployment_transition_state_read_only(&source.canonical_path)
    .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_recovery", error.to_string()))?;
  let source_parent = source.canonical_path.parent().expect("validated canonical source has a parent").to_path_buf();
  let source_native = NativeCutoverCapabilitiesV1::from_probe_report(
    &probe_native_durability(&source_parent)
      .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_source_native", error.to_string()))?,
  );
  let destination_native = NativeCutoverCapabilitiesV1::from_probe_report(
    &probe_native_durability(&destination.parent)
      .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_destination_native", error.to_string()))?,
  );
  let executable_sha256 = sha256_regular_file(request.executable, request.cancellation)?;

  let source_path = source
    .canonical_path
    .to_str()
    .ok_or_else(|| OfflineMigrationPreflightErrorV1::new("offline_migration_source_path", "source path is not UTF-8"))?;
  let engine = Arc::new(
    StorageEngine::open_for_offline_migration_inspection(source_path, request.configuration_overrides.clone())
      .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_source_open", error.to_string()))?,
  );
  let open_evidence = collect_open_source_evidence(&request, &source, &engine);
  let shutdown = engine.shutdown();
  let open_evidence = open_evidence?;
  shutdown.map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_source_shutdown", error.to_string()))?;
  drop(engine);

  let source_after = observe_source(&source.canonical_path, request.cancellation)?;
  if source.file_identity != source_after.file_identity || source.evidence != source_after.evidence {
    return Err(OfflineMigrationPreflightErrorV1::new(
      "offline_migration_source_changed",
      "migration source identity, bytes, selected header, or HEAD changed during preflight",
    ));
  }
  let recovery_after = inspect_deployment_transition_state_read_only(&source.canonical_path)
    .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_recovery", error.to_string()))?;
  if recovery_before != recovery_after {
    return Err(OfflineMigrationPreflightErrorV1::new(
      "offline_migration_source_changed",
      "migration source recovery authority changed during preflight",
    ));
  }
  let recovery = MigrationRecoveryEvidenceV1::from_deployment_state(&recovery_after, source.evidence.selected_header_sequence, 0, 0)
    .map_err(|error| OfflineMigrationPreflightErrorV1::new(error.code(), error.to_string()))?;
  let registry = embedded_system_family_registry(source.evidence.hash_algorithm)
    .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_system_family_registry", error.to_string()))?;
  let capacity = collect_capacity(
    &source,
    &destination,
    &source_parent,
    &workspace_capacity_root,
    &open_evidence.configuration,
    request.bounds,
    request.cancellation,
  )?;
  let baseline = CapabilitySetV1::v4_baseline();
  let resume_manifest = request.resume_manifest;
  let admission = MigrationPreflightRequestV1 {
    identity: MigrationIdentityEvidenceV1 {
      database_id: request.database_id,
      migration_id: request.migration_id,
      source_physical_instance_id: request.source_physical_instance_id,
      destination_physical_instance_id: request.destination_physical_instance_id,
      source_path_digest: migration_native_path_digest_v1(&source.canonical_path),
      destination_path_digest: destination.path_digest,
      source_file_identity: source.file_identity,
      destination_parent_identity: destination.parent_identity,
    },
    source: source.evidence,
    verification: open_evidence.verification,
    recovery,
    inventory: open_evidence.inventory,
    capacity,
    native: MigrationNativeEvidenceV1 { source: source_native, destination: destination_native },
    memory: open_evidence.memory,
    configuration: open_evidence.configuration,
    binary: MigrationBinaryEvidenceV1 {
      source_commit: request.source_commit,
      executable_sha256,
      contract_registry_sha256: contract_registry_sha256()?,
      capability_profile: BinaryCapabilityProfileV1::new(baseline, baseline),
      required_reader_capabilities: baseline,
      required_writer_capabilities: baseline,
      system_family_registry_fingerprint: registry.operational_fingerprint.clone(),
    },
  };
  let (report, permit) = admit_migration_preflight_v1(&admission)
    .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_preflight_refused", error.to_string()))?;
  if let Some(manifest) = resume_manifest {
    manifest.validate_permit(&permit).map_err(|error| OfflineMigrationPreflightErrorV1::new(error.code(), error.to_string()))?;
  }
  Ok(OfflineMigrationPreflightV1 { report, permit })
}

fn collect_open_source_evidence(
  request: &OfflineMigrationPreflightRequestV1<'_>,
  source: &SourceObservationV1,
  engine: &Arc<StorageEngine>,
) -> Result<OpenSourceEvidenceV1, OfflineMigrationPreflightErrorV1> {
  check_cancelled(request.cancellation)?;
  let verify_report = verify_checked(engine, source.canonical_path.to_str().expect("validated UTF-8 source path"))
    .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_verify", error.to_string()))?;
  let verification = StrictVerificationEvidenceV1::from_complete_report(
    &verify_report,
    source.evidence.selected_header_sequence,
    source.evidence.complete_file_checksum,
  );
  let limits = V3MigrationAuthorityInventoryLimitsV1 {
    maximum_roots: request.bounds.maximum_authority_roots,
    maximum_peers: request.bounds.maximum_authority_records,
    maximum_tasks: request.bounds.maximum_authority_records,
    maximum_plugins: request.bounds.maximum_authority_records,
    maximum_namespace_memory_bytes: request.bounds.maximum_memory_bytes,
    maximum_namespace_work_items: request.bounds.maximum_work_items,
    maximum_directory_depth: request.bounds.maximum_directory_depth as usize,
  };
  let inventory = collect_v3_migration_authority_inventory_v1(V3MigrationAuthorityInventoryRequestV1 {
    source: engine,
    database_id: request.database_id,
    source_physical_instance_id: request.source_physical_instance_id,
    cancellation: request.cancellation,
    acquisition_timeout: request.acquisition_timeout,
    limits,
  })?
  .preflight_evidence();
  let run_configuration = engine
    .capture_migration_run_configuration()
    .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_configuration", error.to_string()))?;
  let fingerprint = digest_parts(
    source.evidence.hash_algorithm,
    &[
      CONFIGURATION_FINGERPRINT_DOMAIN_V1,
      &run_configuration.generation.to_le_bytes(),
      &run_configuration.capture_max_bytes.to_le_bytes(),
      &run_configuration.capture_free_reserve_bytes.to_le_bytes(),
      &run_configuration.checkpoint_after_seconds.to_le_bytes(),
    ],
  );
  let configuration = MigrationConfigurationEvidenceV1::from_run_configuration(run_configuration, fingerprint);
  let memory_snapshot = engine
    .memory_coordinator_snapshot()
    .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_memory", error.to_string()))?;
  let memory =
    MigrationMemoryEvidenceV1::from_snapshot(&memory_snapshot, request.bounds.maximum_memory_bytes, request.bounds.maximum_memory_bytes)
      .map_err(|error| OfflineMigrationPreflightErrorV1::new(error.code(), error.to_string()))?;
  Ok(OpenSourceEvidenceV1 { verification, inventory, memory, configuration })
}

impl From<crate::engine::EngineError> for OfflineMigrationPreflightErrorV1 {
  fn from(error: crate::engine::EngineError) -> Self {
    Self::new("offline_migration_source_inventory", error.to_string())
  }
}

fn validate_request_identity(request: &OfflineMigrationPreflightRequestV1<'_>) -> Result<(), OfflineMigrationPreflightErrorV1> {
  if [request.database_id, request.migration_id, request.source_physical_instance_id, request.destination_physical_instance_id]
    .iter()
    .any(|value| value.iter().all(|byte| *byte == 0))
    || request.source_commit.iter().all(|byte| *byte == 0)
    || request.source_physical_instance_id == request.destination_physical_instance_id
    || request.acquisition_timeout.is_zero()
  {
    return Err(OfflineMigrationPreflightErrorV1::new(
      "offline_migration_identity",
      "offline migration identities, source commit, physical separation, or timeout are invalid",
    ));
  }
  if let Some(manifest) = request.resume_manifest {
    if manifest.source() != request.source
      || manifest.destination() != request.destination
      || manifest.workspace() != request.workspace
      || manifest.database_id() != request.database_id
      || manifest.migration_id() != request.migration_id
      || manifest.source_physical_instance_id() != request.source_physical_instance_id
      || manifest.destination_physical_instance_id() != request.destination_physical_instance_id
      || manifest.bounds() != request.bounds
    {
      return Err(OfflineMigrationPreflightErrorV1::new(
        "offline_migration_manifest_binding",
        "resume request differs from the opened immutable migration manifest",
      ));
    }
  }
  Ok(())
}

fn observe_source(path: &Path, cancellation: &CancellationToken) -> Result<SourceObservationV1, OfflineMigrationPreflightErrorV1> {
  check_cancelled(cancellation)?;
  if !is_canonical_lexical_absolute_utf8_path(path) {
    return Err(OfflineMigrationPreflightErrorV1::new(
      "offline_migration_source_path",
      "source path must be absolute, canonical, and UTF-8",
    ));
  }
  let metadata = fs::symlink_metadata(path)
    .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_source_path", error.to_string()))?;
  if metadata.file_type().is_symlink() || !metadata.is_file() {
    return Err(OfflineMigrationPreflightErrorV1::new("offline_migration_source_path", "source must be a no-follow regular file"));
  }
  let canonical_path =
    fs::canonicalize(path).map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_source_path", error.to_string()))?;
  if canonical_path != path {
    return Err(OfflineMigrationPreflightErrorV1::new("offline_migration_source_path", "source path does not equal its canonical path"));
  }
  let mut file = File::open(&canonical_path)
    .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_source_path", error.to_string()))?;
  let file_identity = platform_file_identity_from_file(&file)
    .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_source_identity", error.to_string()))?;
  let file_size =
    file.metadata().map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_source_path", error.to_string()))?.len();
  let (header, selected_slot) = match read_database_header_read_only(&mut file)
    .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_source_header", error.to_string()))?
  {
    ReadOnlyDatabaseHeader::V3 { header, selected_slot } => (header, selected_slot),
    ReadOnlyDatabaseHeader::V4(_) => {
      return Err(OfflineMigrationPreflightErrorV1::new(
        "offline_migration_source_header",
        "offline v3-to-v4 migration requires a selected v3 source header",
      ));
    }
  };
  let selected_offset = selected_slot
    .checked_mul(FILE_HEADER_SIZE)
    .ok_or_else(|| OfflineMigrationPreflightErrorV1::new("offline_migration_source_header", "selected header offset overflowed"))?;
  file
    .seek(SeekFrom::Start(selected_offset as u64))
    .and_then(|_| {
      let mut selected = [0u8; FILE_HEADER_SIZE];
      file.read_exact(&mut selected)?;
      Ok(selected)
    })
    .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_source_header", error.to_string()))
    .and_then(|selected| {
      let selected_header_digest = *blake3::hash(&selected).as_bytes();
      let complete_file_checksum = complete_file_blake3(&mut file, cancellation)?;
      let evidence =
        MigrationSourceEvidenceV1::from_v3_header(&header, selected_slot, file_size, complete_file_checksum, selected_header_digest)
          .map_err(|error| OfflineMigrationPreflightErrorV1::new(error.code(), error.to_string()))?;
      Ok(SourceObservationV1 { canonical_path, file_identity, evidence })
    })
}

fn complete_file_blake3(file: &mut File, cancellation: &CancellationToken) -> Result<[u8; 32], OfflineMigrationPreflightErrorV1> {
  file
    .seek(SeekFrom::Start(0))
    .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_source_checksum", error.to_string()))?;
  let mut hasher = blake3::Hasher::new();
  let mut buffer = [0u8; CHECKSUM_BUFFER_BYTES];
  loop {
    check_cancelled(cancellation)?;
    let read = file
      .read(&mut buffer)
      .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_source_checksum", error.to_string()))?;
    if read == 0 {
      return Ok(*hasher.finalize().as_bytes());
    }
    hasher.update(&buffer[..read]);
  }
}

fn observe_destination(
  request: &OfflineMigrationPreflightRequestV1<'_>,
  source: &Path,
) -> Result<DestinationBindingV1, OfflineMigrationPreflightErrorV1> {
  if let Some(manifest) = request.resume_manifest {
    if request.destination == source {
      return Err(OfflineMigrationPreflightErrorV1::new(
        "offline_migration_destination",
        "migration source and destination paths must be distinct",
      ));
    }
    let parent = request
      .destination
      .parent()
      .ok_or_else(|| OfflineMigrationPreflightErrorV1::new("offline_migration_destination", "destination has no parent"))?
      .to_path_buf();
    return Ok(DestinationBindingV1 {
      path_digest: migration_native_path_digest_v1(request.destination),
      parent,
      parent_identity: manifest.destination_parent_identity(),
    });
  }
  let observation = observe_migration_destination_path_v1(request.destination)
    .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_destination", error.to_string()))?;
  if observation.path() == source {
    return Err(OfflineMigrationPreflightErrorV1::new(
      "offline_migration_destination",
      "migration source and destination paths must be distinct",
    ));
  }
  Ok(DestinationBindingV1 {
    path_digest: observation.path_digest(),
    parent: observation.parent().to_path_buf(),
    parent_identity: observation.parent_identity(),
  })
}

fn observe_workspace_capacity_root(path: &Path) -> Result<PathBuf, OfflineMigrationPreflightErrorV1> {
  if !is_canonical_lexical_absolute_utf8_path(path) {
    return Err(OfflineMigrationPreflightErrorV1::new(
      "offline_migration_workspace_path",
      "workspace path must be absolute, canonical, and UTF-8",
    ));
  }
  match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
      Err(OfflineMigrationPreflightErrorV1::new("offline_migration_workspace_path", "existing workspace must be a no-follow directory"))
    }
    Ok(_) => {
      let canonical = fs::canonicalize(path)
        .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_workspace_path", error.to_string()))?;
      if canonical != path {
        return Err(OfflineMigrationPreflightErrorV1::new(
          "offline_migration_workspace_path",
          "existing workspace does not resolve to the supplied path",
        ));
      }
      Ok(canonical)
    }
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
      let parent = path
        .parent()
        .ok_or_else(|| OfflineMigrationPreflightErrorV1::new("offline_migration_workspace_path", "workspace has no parent"))?;
      let canonical_parent = fs::canonicalize(parent)
        .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_workspace_path", error.to_string()))?;
      if canonical_parent.join(path.file_name().expect("validated workspace path has a final component")) != path {
        return Err(OfflineMigrationPreflightErrorV1::new(
          "offline_migration_workspace_path",
          "workspace parent does not resolve to the supplied path",
        ));
      }
      Ok(canonical_parent)
    }
    Err(error) => Err(OfflineMigrationPreflightErrorV1::new("offline_migration_workspace_path", error.to_string())),
  }
}

fn collect_capacity(
  source: &SourceObservationV1,
  destination: &DestinationBindingV1,
  source_parent: &Path,
  workspace_root: &Path,
  configuration: &MigrationConfigurationEvidenceV1,
  bounds: MigrationRunBoundsV1,
  cancellation: &CancellationToken,
) -> Result<[MigrationCapacityObservationV1; 4], OfflineMigrationPreflightErrorV1> {
  check_cancelled(cancellation)?;
  let mut observations = [
    capacity_observation(CapacityRoleV1::Destination, &destination.parent, source.evidence.file_size, bounds.root_map_minimum_free_bytes)?,
    capacity_observation(
      CapacityRoleV1::Workspace,
      workspace_root,
      bounds.root_map_maximum_stored_bytes,
      bounds.root_map_minimum_free_bytes,
    )?,
    capacity_observation(CapacityRoleV1::Backup, source_parent, source.evidence.file_size, bounds.root_map_minimum_free_bytes)?,
    capacity_observation(
      CapacityRoleV1::Capture,
      workspace_root,
      configuration.capture_max_bytes,
      configuration.capture_free_reserve_bytes,
    )?,
  ];
  for index in 0..observations.len() {
    if let Some((filesystem_capacity_bytes, available_bytes)) = observations[..index]
      .iter()
      .find(|prior| prior.volume_identity == observations[index].volume_identity)
      .map(|prior| (prior.filesystem_capacity_bytes, prior.available_bytes))
    {
      observations[index].filesystem_capacity_bytes = filesystem_capacity_bytes;
      observations[index].available_bytes = available_bytes;
    }
  }
  Ok(observations)
}

fn capacity_observation(
  role: CapacityRoleV1,
  root: &Path,
  required_bytes: u64,
  minimum_remaining_bytes: u64,
) -> Result<MigrationCapacityObservationV1, OfflineMigrationPreflightErrorV1> {
  let path_identity = platform_file_identity(root)
    .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_capacity_identity", error.to_string()))?;
  let filesystem_capacity_bytes =
    fs2::total_space(root).map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_capacity", error.to_string()))?;
  let available_bytes =
    fs2::available_space(root).map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_capacity", error.to_string()))?;
  Ok(MigrationCapacityObservationV1 {
    role,
    volume_identity: path_identity.volume_identity,
    path_identity,
    filesystem_capacity_bytes,
    available_bytes,
    required_bytes,
    minimum_remaining_bytes,
  })
}

fn sha256_regular_file(path: &Path, cancellation: &CancellationToken) -> Result<[u8; 32], OfflineMigrationPreflightErrorV1> {
  if !is_canonical_lexical_absolute_utf8_path(path) {
    return Err(OfflineMigrationPreflightErrorV1::new(
      "offline_migration_binary_path",
      "executable path must be absolute, canonical, and UTF-8",
    ));
  }
  let metadata = fs::symlink_metadata(path)
    .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_binary_path", error.to_string()))?;
  if metadata.file_type().is_symlink() || !metadata.is_file() {
    return Err(OfflineMigrationPreflightErrorV1::new("offline_migration_binary_path", "executable must be a no-follow regular file"));
  }
  let mut file =
    File::open(path).map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_binary_path", error.to_string()))?;
  let mut hasher = Sha256::new();
  let mut buffer = [0u8; CHECKSUM_BUFFER_BYTES];
  loop {
    check_cancelled(cancellation)?;
    let read = file
      .read(&mut buffer)
      .map_err(|error| OfflineMigrationPreflightErrorV1::new("offline_migration_binary_checksum", error.to_string()))?;
    if read == 0 {
      return Ok(hasher.finalize().into());
    }
    hasher.update(&buffer[..read]);
  }
}

fn contract_registry_sha256() -> Result<[u8; 32], OfflineMigrationPreflightErrorV1> {
  hex::decode(CONTRACT_REGISTRY_SHA256)
    .ok()
    .and_then(|bytes| bytes.try_into().ok())
    .ok_or_else(|| OfflineMigrationPreflightErrorV1::new("offline_migration_contract_registry", "embedded contract digest is malformed"))
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), OfflineMigrationPreflightErrorV1> {
  if cancellation.is_cancelled() {
    Err(OfflineMigrationPreflightErrorV1::new("offline_migration_cancelled", "offline migration preflight was cancelled"))
  } else {
    Ok(())
  }
}
