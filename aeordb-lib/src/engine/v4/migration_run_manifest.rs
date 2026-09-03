//! Immutable same-host identity and evidence anchor for one offline migration.
//!
//! The manifest is created after preflight admission and before destination
//! creation. It is private, checksummed, read back, and parent-synced so a
//! restart reuses the exact admitted identities and execution bounds instead
//! of inventing a second logical run. It owns no source or destination writes.

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use super::migration_destination::{migration_native_path_digest_v1, observe_migration_destination_path_v1};
use super::migration_preflight::{AuthorityInventoryCountsV1, MigrationPreflightPermitV1};
use super::private_workspace::{
  create_private_directory_synced, create_private_regular_file, validate_existing_directory, validate_private_directory_readonly,
  is_canonical_lexical_absolute_utf8_path, validate_private_regular_file,
};
use crate::engine::emergency_spill::open_regular_file_no_follow;
use crate::engine::native_durability::{PlatformFileIdentityDescriptorV1, platform_file_identity, sync_directory_native, sync_file_all_native};
use crate::engine::HashAlgorithm;

pub const MIGRATION_RUN_MANIFEST_FILE_NAME: &str = "migration-run-v1.json";

const FORMAT_NAME: &str = "aeordb-offline-migration-run";
const FORMAT_VERSION: u16 = 1;
const INTEGRITY_DOMAIN: &[u8] = b"aeordb.offline-migration-run-manifest.v1\0";
const MAXIMUM_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAXIMUM_MEMORY_BYTES: u64 = 1024 * 1024 * 1024;
const MAXIMUM_WORK_ITEMS: u64 = 1 << 40;
const MAXIMUM_DECODED_CHUNK_BYTES: u64 = 64 * 1024 * 1024;
const MAXIMUM_DIRECTORY_DEPTH: u32 = 1_000;
const MAXIMUM_AUTHORITY_RECORDS: u64 = 1_000_000;
const MAXIMUM_ROOT_MAP_BYTES: u64 = 4 * 1024 * 1024 * 1024 * 1024;
const MAXIMUM_ROOT_MAP_OPEN_RUNS: u32 = 64;
const MAXIMUM_ROOT_MAP_PAGE_ROWS: u32 = 1_000_000;
const MAXIMUM_ROOT_MAP_PUBLICATION_BYTES: u64 = 64 * 1024 * 1024;
const MAXIMUM_LEASE_DURATION_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationRunBoundsV1 {
  pub maximum_memory_bytes: u64,
  pub maximum_work_items: u64,
  pub maximum_decoded_chunk_bytes: u64,
  pub maximum_directory_depth: u32,
  pub maximum_authority_roots: u64,
  pub maximum_authority_records: u64,
  pub root_map_maximum_stored_bytes: u64,
  pub root_map_maximum_staged_rows: u64,
  pub root_map_minimum_free_bytes: u64,
  pub root_map_maximum_sort_memory_bytes: u64,
  pub root_map_maximum_open_runs: u32,
  pub root_map_maximum_page_rows: u32,
  pub root_map_maximum_publication_batch_bytes: u64,
  pub prior_lookup_maximum_memory_bytes: u64,
  pub lease_duration_ms: u64,
}

impl MigrationRunBoundsV1 {
  fn validate(self) -> Result<(), MigrationRunManifestErrorV1> {
    if self.maximum_memory_bytes == 0
      || self.maximum_memory_bytes > MAXIMUM_MEMORY_BYTES
      || self.maximum_work_items == 0
      || self.maximum_work_items > MAXIMUM_WORK_ITEMS
      || self.maximum_decoded_chunk_bytes == 0
      || self.maximum_decoded_chunk_bytes > MAXIMUM_DECODED_CHUNK_BYTES
      || self.maximum_directory_depth == 0
      || self.maximum_directory_depth > MAXIMUM_DIRECTORY_DEPTH
      || self.maximum_authority_roots == 0
      || self.maximum_authority_roots > MAXIMUM_AUTHORITY_RECORDS
      || self.maximum_authority_records == 0
      || self.maximum_authority_records > MAXIMUM_AUTHORITY_RECORDS
      || self.root_map_maximum_stored_bytes == 0
      || self.root_map_maximum_stored_bytes > MAXIMUM_ROOT_MAP_BYTES
      || self.root_map_maximum_staged_rows == 0
      || self.root_map_maximum_staged_rows > MAXIMUM_AUTHORITY_RECORDS
      || self.root_map_minimum_free_bytes > MAXIMUM_ROOT_MAP_BYTES
      || self.root_map_maximum_sort_memory_bytes == 0
      || self.root_map_maximum_sort_memory_bytes > MAXIMUM_MEMORY_BYTES
      || !(2..=MAXIMUM_ROOT_MAP_OPEN_RUNS).contains(&self.root_map_maximum_open_runs)
      || self.root_map_maximum_page_rows == 0
      || self.root_map_maximum_page_rows > MAXIMUM_ROOT_MAP_PAGE_ROWS
      || self.root_map_maximum_publication_batch_bytes == 0
      || self.root_map_maximum_publication_batch_bytes > MAXIMUM_ROOT_MAP_PUBLICATION_BYTES
      || self.prior_lookup_maximum_memory_bytes == 0
      || self.prior_lookup_maximum_memory_bytes > MAXIMUM_MEMORY_BYTES
      || self.lease_duration_ms == 0
      || self.lease_duration_ms > MAXIMUM_LEASE_DURATION_MS
    {
      return Err(MigrationRunManifestErrorV1::invalid(
        "migration_run_manifest_bounds",
        "offline migration run bounds are zero, inconsistent, or exceed the supported limits",
      ));
    }
    if self.maximum_authority_roots > self.maximum_authority_records
      || self.root_map_maximum_staged_rows < self.maximum_authority_roots
      || self.maximum_decoded_chunk_bytes > self.maximum_memory_bytes
      || self.root_map_maximum_sort_memory_bytes > self.maximum_memory_bytes
      || self.root_map_maximum_publication_batch_bytes > self.maximum_memory_bytes
      || self.prior_lookup_maximum_memory_bytes > self.maximum_memory_bytes
      || self.maximum_authority_records > self.maximum_work_items
    {
      return Err(MigrationRunManifestErrorV1::invalid(
        "migration_run_manifest_bounds",
        "offline migration aggregate bounds do not cover their required subordinate work",
      ));
    }
    Ok(())
  }

  fn validate_authority_counts(self, counts: AuthorityInventoryCountsV1) -> Result<(), MigrationRunManifestErrorV1> {
    let values = [
      counts.protected_families,
      counts.modules,
      counts.snapshots,
      counts.forks,
      counts.symlinks,
      counts.history_roots,
      counts.peers,
      counts.sync_states,
      counts.tasks,
      counts.plugins,
      counts.roots,
    ];
    let total = values.iter().try_fold(0u64, |total, value| total.checked_add(*value));
    if counts.roots > self.maximum_authority_roots
      || values.iter().any(|value| *value > self.maximum_authority_records)
      || total.is_none_or(|total| total > self.maximum_authority_records)
    {
      return Err(MigrationRunManifestErrorV1::invalid(
        "migration_run_manifest_bounds",
        "persisted source authority counts exceed the admitted run bounds",
      ));
    }
    Ok(())
  }
}

pub struct MigrationRunManifestCreateRequestV1<'a> {
  pub workspace: &'a Path,
  pub source: &'a Path,
  pub destination: &'a Path,
  pub permit: &'a MigrationPreflightPermitV1,
  pub holder_boot_id: [u8; 16],
  pub created_at_ms: u64,
  pub bounds: MigrationRunBoundsV1,
  pub cancellation: &'a CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationRunManifestV1 {
  path: PathBuf,
  workspace: PathBuf,
  source: PathBuf,
  destination: PathBuf,
  created_at_ms: u64,
  database_id: [u8; 16],
  migration_id: [u8; 16],
  source_physical_instance_id: [u8; 16],
  destination_physical_instance_id: [u8; 16],
  holder_boot_id: [u8; 16],
  source_path_digest: [u8; 32],
  destination_path_digest: [u8; 32],
  source_file_identity: PlatformFileIdentityDescriptorV1,
  destination_parent_identity: PlatformFileIdentityDescriptorV1,
  hash_algorithm: HashAlgorithm,
  source_file_size: u64,
  source_complete_file_checksum: [u8; 32],
  source_header_sequence: u64,
  source_selected_header_digest: [u8; 32],
  source_capture_head: Vec<u8>,
  source_authority_digest: [u8; 32],
  source_authority_counts: AuthorityInventoryCountsV1,
  configuration_generation: u64,
  effective_configuration_fingerprint: Vec<u8>,
  system_family_registry_fingerprint: Vec<u8>,
  capability_profile: BinaryCapabilityProfileV1,
  required_reader_capabilities: CapabilitySetV1,
  required_writer_capabilities: CapabilitySetV1,
  binary_source_commit: [u8; 20],
  binary_executable_sha256: [u8; 32],
  source_native_qualification_digest: [u8; 32],
  destination_native_qualification_digest: [u8; 32],
  capture_max_bytes: u64,
  capture_free_reserve_bytes: u64,
  checkpoint_after_seconds: u64,
  preflight_evidence_fingerprint: [u8; 32],
  bounds: MigrationRunBoundsV1,
}

impl MigrationRunManifestV1 {
  pub fn path(&self) -> &Path {
    &self.path
  }

  pub fn workspace(&self) -> &Path {
    &self.workspace
  }

  pub fn source(&self) -> &Path {
    &self.source
  }

  pub fn destination(&self) -> &Path {
    &self.destination
  }

  pub const fn created_at_ms(&self) -> u64 {
    self.created_at_ms
  }

  pub const fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  pub const fn migration_id(&self) -> [u8; 16] {
    self.migration_id
  }

  pub const fn source_physical_instance_id(&self) -> [u8; 16] {
    self.source_physical_instance_id
  }

  pub const fn destination_physical_instance_id(&self) -> [u8; 16] {
    self.destination_physical_instance_id
  }

  pub const fn holder_boot_id(&self) -> [u8; 16] {
    self.holder_boot_id
  }

  pub const fn source_file_identity(&self) -> PlatformFileIdentityDescriptorV1 {
    self.source_file_identity
  }

  pub const fn destination_parent_identity(&self) -> PlatformFileIdentityDescriptorV1 {
    self.destination_parent_identity
  }

  pub const fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }

  pub const fn source_file_size(&self) -> u64 {
    self.source_file_size
  }

  pub const fn source_complete_file_checksum(&self) -> [u8; 32] {
    self.source_complete_file_checksum
  }

  pub const fn source_header_sequence(&self) -> u64 {
    self.source_header_sequence
  }

  pub fn source_capture_head(&self) -> &[u8] {
    &self.source_capture_head
  }

  pub const fn source_authority_digest(&self) -> [u8; 32] {
    self.source_authority_digest
  }

  pub const fn source_authority_counts(&self) -> AuthorityInventoryCountsV1 {
    self.source_authority_counts
  }

  pub const fn configuration_generation(&self) -> u64 {
    self.configuration_generation
  }

  pub fn effective_configuration_fingerprint(&self) -> &[u8] {
    &self.effective_configuration_fingerprint
  }

  pub fn system_family_registry_fingerprint(&self) -> &[u8] {
    &self.system_family_registry_fingerprint
  }

  pub const fn capability_profile(&self) -> BinaryCapabilityProfileV1 {
    self.capability_profile
  }

  pub const fn required_reader_capabilities(&self) -> CapabilitySetV1 {
    self.required_reader_capabilities
  }

  pub const fn required_writer_capabilities(&self) -> CapabilitySetV1 {
    self.required_writer_capabilities
  }

  pub const fn binary_source_commit(&self) -> [u8; 20] {
    self.binary_source_commit
  }

  pub const fn binary_executable_sha256(&self) -> [u8; 32] {
    self.binary_executable_sha256
  }

  pub const fn source_native_qualification_digest(&self) -> [u8; 32] {
    self.source_native_qualification_digest
  }

  pub const fn destination_native_qualification_digest(&self) -> [u8; 32] {
    self.destination_native_qualification_digest
  }

  pub const fn capture_max_bytes(&self) -> u64 {
    self.capture_max_bytes
  }

  pub const fn capture_free_reserve_bytes(&self) -> u64 {
    self.capture_free_reserve_bytes
  }

  pub const fn checkpoint_after_seconds(&self) -> u64 {
    self.checkpoint_after_seconds
  }

  pub const fn preflight_evidence_fingerprint(&self) -> [u8; 32] {
    self.preflight_evidence_fingerprint
  }

  pub const fn bounds(&self) -> MigrationRunBoundsV1 {
    self.bounds
  }

  pub fn validate_permit(&self, permit: &MigrationPreflightPermitV1) -> Result<(), MigrationRunManifestErrorV1> {
    if self.database_id != permit.database_id()
      || self.migration_id != permit.migration_id()
      || self.source_physical_instance_id != permit.source_physical_instance_id()
      || self.destination_physical_instance_id != permit.destination_physical_instance_id()
      || self.source_path_digest != permit.source_path_digest()
      || self.destination_path_digest != permit.destination_path_digest()
      || self.source_file_identity != permit.source_file_identity()
      || self.destination_parent_identity != permit.destination_parent_identity()
      || self.hash_algorithm != permit.hash_algorithm()
      || self.source_file_size != permit.source_file_size()
      || self.source_complete_file_checksum != permit.source_complete_file_checksum()
      || self.source_header_sequence != permit.source_header_sequence()
      || self.source_selected_header_digest != permit.source_selected_header_digest()
      || self.source_capture_head != permit.source_capture_head()
      || self.source_authority_digest != permit.source_authority_digest()
      || self.source_authority_counts != permit.source_authority_counts()
      || self.configuration_generation != permit.configuration_generation()
      || self.effective_configuration_fingerprint != permit.effective_configuration_fingerprint()
      || self.system_family_registry_fingerprint != permit.system_family_registry_fingerprint()
      || self.capability_profile != permit.capability_profile()
      || self.required_reader_capabilities != permit.required_reader_capabilities()
      || self.required_writer_capabilities != permit.required_writer_capabilities()
      || self.binary_source_commit != permit.binary_source_commit()
      || self.binary_executable_sha256 != permit.binary_executable_sha256()
      || self.source_native_qualification_digest != permit.source_native_qualification_digest()
      || self.destination_native_qualification_digest != permit.destination_native_qualification_digest()
      || self.capture_max_bytes != permit.capture_max_bytes()
      || self.capture_free_reserve_bytes != permit.capture_free_reserve_bytes()
      || self.checkpoint_after_seconds != permit.checkpoint_after_seconds()
    {
      return Err(MigrationRunManifestErrorV1::invalid(
        "migration_run_manifest_permit",
        "reopened migration permit differs from the immutable run manifest",
      ));
    }
    Ok(())
  }
}

#[derive(Debug)]
pub struct MigrationRunManifestErrorV1 {
  code: &'static str,
  message: String,
}

impl MigrationRunManifestErrorV1 {
  pub const fn code(&self) -> &'static str {
    self.code
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self { code, message: message.into() }
  }
}

impl Display for MigrationRunManifestErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.message)
  }
}

impl Error for MigrationRunManifestErrorV1 {}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedManifestEnvelopeV1 {
  format: String,
  version: u16,
  body: PersistedManifestBodyV1,
  body_blake3: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedManifestBodyV1 {
  created_at_ms: u64,
  database_id: String,
  migration_id: String,
  source_physical_instance_id: String,
  destination_physical_instance_id: String,
  holder_boot_id: String,
  workspace: String,
  source: String,
  destination: String,
  source_path_digest: String,
  destination_path_digest: String,
  source_file_identity: PersistedFileIdentityV1,
  destination_parent_identity: PersistedFileIdentityV1,
  hash_algorithm: u16,
  source_file_size: u64,
  source_complete_file_checksum: String,
  source_header_sequence: u64,
  source_selected_header_digest: String,
  source_capture_head: String,
  source_authority_digest: String,
  source_authority_counts: PersistedAuthorityCountsV1,
  configuration_generation: u64,
  effective_configuration_fingerprint: String,
  system_family_registry_fingerprint: String,
  supported_reader_capabilities: String,
  supported_writer_capabilities: String,
  required_reader_capabilities: String,
  required_writer_capabilities: String,
  binary_source_commit: String,
  binary_executable_sha256: String,
  source_native_qualification_digest: String,
  destination_native_qualification_digest: String,
  capture_max_bytes: u64,
  capture_free_reserve_bytes: u64,
  checkpoint_after_seconds: u64,
  preflight_evidence_fingerprint: String,
  bounds: MigrationRunBoundsV1,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedFileIdentityV1 {
  platform: u16,
  schema: u16,
  flags: u32,
  volume_identity: String,
  file_identity: String,
  birth_identity: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedAuthorityCountsV1 {
  protected_families: u64,
  modules: u64,
  snapshots: u64,
  forks: u64,
  symlinks: u64,
  history_roots: u64,
  peers: u64,
  sync_states: u64,
  tasks: u64,
  plugins: u64,
  roots: u64,
}

pub fn create_migration_run_manifest_v1(
  request: MigrationRunManifestCreateRequestV1<'_>,
) -> Result<MigrationRunManifestV1, MigrationRunManifestErrorV1> {
  check_cancelled(request.cancellation)?;
  request.bounds.validate()?;
  if request.holder_boot_id.iter().all(|byte| *byte == 0) || request.created_at_ms == 0 || request.created_at_ms > i64::MAX as u64 {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_identity",
      "holder identity and creation time must be nonzero and persistable",
    ));
  }
  let source = canonical_existing_file(request.source, "migration source")?;
  let destination = observe_migration_destination_path_v1(request.destination)
    .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_destination", error.to_string()))?;
  if destination.path() == source {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_paths",
      "migration source and destination paths must be distinct",
    ));
  }
  if migration_native_path_digest_v1(&source) != request.permit.source_path_digest()
    || destination.path_digest() != request.permit.destination_path_digest()
    || destination.parent_identity() != request.permit.destination_parent_identity()
  {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_paths",
      "canonical source or destination path differs from the admitted preflight path",
    ));
  }
  let source_identity = platform_file_identity(&source)
    .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_source_identity", error.to_string()))?;
  let source_size = fs::metadata(&source)
    .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_source_metadata", error.to_string()))?
    .len();
  if source_identity != request.permit.source_file_identity() || source_size != request.permit.source_file_size() {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_source_identity",
      "migration source physical identity or size changed after preflight",
    ));
  }
  check_cancelled(request.cancellation)?;
  let workspace = prepare_workspace(request.workspace)?;
  let body = body_from_request(&request, &workspace, &source, destination.path())?;
  let bytes = encode_manifest(&body)?;
  if bytes.len() as u64 > MAXIMUM_MANIFEST_BYTES {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_size",
      "encoded migration run manifest exceeds its one-megabyte bound",
    ));
  }
  check_cancelled(request.cancellation)?;
  let path = workspace.join(MIGRATION_RUN_MANIFEST_FILE_NAME);
  let mut file = create_private_regular_file(&path, "migration run manifest")
    .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_create", error.to_string()))?;
  file.write_all(&bytes).map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_write", error.to_string()))?;
  sync_file_all_native(&file).map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_sync", error.to_string()))?;
  let readback = read_bounded_manifest(&path, None)?;
  if readback != bytes {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_readback",
      "migration run manifest read-back differs from the bytes written",
    ));
  }
  sync_directory_native(&workspace)
    .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_parent_sync", error.to_string()))?;
  let manifest = decode_manifest(&readback, path)?;
  manifest.validate_permit(request.permit)?;
  revalidate_host_bindings(&manifest)?;
  Ok(manifest)
}

pub fn open_migration_run_manifest_v1(
  workspace: impl AsRef<Path>,
  cancellation: &CancellationToken,
) -> Result<MigrationRunManifestV1, MigrationRunManifestErrorV1> {
  check_cancelled(cancellation)?;
  let workspace = canonical_existing_directory(workspace.as_ref(), "migration run workspace")?;
  validate_private_directory_readonly(&workspace, "migration run workspace")
    .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_workspace", error.to_string()))?;
  let path = workspace.join(MIGRATION_RUN_MANIFEST_FILE_NAME);
  let bytes = read_bounded_manifest(&path, Some(cancellation))?;
  let manifest = decode_manifest(&bytes, path)?;
  if manifest.workspace != workspace {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_workspace",
      "manifest workspace differs from its canonical containing directory",
    ));
  }
  revalidate_host_bindings(&manifest)?;
  check_cancelled(cancellation)?;
  Ok(manifest)
}

fn body_from_request(
  request: &MigrationRunManifestCreateRequestV1<'_>,
  workspace: &Path,
  source: &Path,
  destination: &Path,
) -> Result<PersistedManifestBodyV1, MigrationRunManifestErrorV1> {
  let permit = request.permit;
  Ok(PersistedManifestBodyV1 {
    created_at_ms: request.created_at_ms,
    database_id: hex::encode(permit.database_id()),
    migration_id: hex::encode(permit.migration_id()),
    source_physical_instance_id: hex::encode(permit.source_physical_instance_id()),
    destination_physical_instance_id: hex::encode(permit.destination_physical_instance_id()),
    holder_boot_id: hex::encode(request.holder_boot_id),
    workspace: utf8_path(workspace, "workspace")?,
    source: utf8_path(source, "source")?,
    destination: utf8_path(destination, "destination")?,
    source_path_digest: hex::encode(permit.source_path_digest()),
    destination_path_digest: hex::encode(permit.destination_path_digest()),
    source_file_identity: permit.source_file_identity().into(),
    destination_parent_identity: permit.destination_parent_identity().into(),
    hash_algorithm: permit.hash_algorithm().to_u16(),
    source_file_size: permit.source_file_size(),
    source_complete_file_checksum: hex::encode(permit.source_complete_file_checksum()),
    source_header_sequence: permit.source_header_sequence(),
    source_selected_header_digest: hex::encode(permit.source_selected_header_digest()),
    source_capture_head: hex::encode(permit.source_capture_head()),
    source_authority_digest: hex::encode(permit.source_authority_digest()),
    source_authority_counts: permit.source_authority_counts().into(),
    configuration_generation: permit.configuration_generation(),
    effective_configuration_fingerprint: hex::encode(permit.effective_configuration_fingerprint()),
    system_family_registry_fingerprint: hex::encode(permit.system_family_registry_fingerprint()),
    supported_reader_capabilities: hex::encode(permit.capability_profile().supported_reader_capabilities.into_bytes()),
    supported_writer_capabilities: hex::encode(permit.capability_profile().supported_writer_capabilities.into_bytes()),
    required_reader_capabilities: hex::encode(permit.required_reader_capabilities().into_bytes()),
    required_writer_capabilities: hex::encode(permit.required_writer_capabilities().into_bytes()),
    binary_source_commit: hex::encode(permit.binary_source_commit()),
    binary_executable_sha256: hex::encode(permit.binary_executable_sha256()),
    source_native_qualification_digest: hex::encode(permit.source_native_qualification_digest()),
    destination_native_qualification_digest: hex::encode(permit.destination_native_qualification_digest()),
    capture_max_bytes: permit.capture_max_bytes(),
    capture_free_reserve_bytes: permit.capture_free_reserve_bytes(),
    checkpoint_after_seconds: permit.checkpoint_after_seconds(),
    preflight_evidence_fingerprint: hex::encode(permit.evidence_fingerprint()),
    bounds: request.bounds,
  })
}

fn encode_manifest(body: &PersistedManifestBodyV1) -> Result<Vec<u8>, MigrationRunManifestErrorV1> {
  let body_bytes =
    serde_json::to_vec(body).map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_encode", error.to_string()))?;
  let envelope = PersistedManifestEnvelopeV1 {
    format: FORMAT_NAME.to_string(),
    version: FORMAT_VERSION,
    body: body.clone(),
    body_blake3: hex::encode(manifest_body_digest(&body_bytes)),
  };
  serde_json::to_vec_pretty(&envelope)
    .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_encode", error.to_string()))
}

fn decode_manifest(bytes: &[u8], path: PathBuf) -> Result<MigrationRunManifestV1, MigrationRunManifestErrorV1> {
  let envelope: PersistedManifestEnvelopeV1 = serde_json::from_slice(bytes)
    .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_decode", error.to_string()))?;
  if envelope.format != FORMAT_NAME || envelope.version != FORMAT_VERSION {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_version",
      "migration run manifest magic or version is unsupported",
    ));
  }
  let body_bytes = serde_json::to_vec(&envelope.body)
    .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_encode", error.to_string()))?;
  let expected_digest = decode_array::<32>(&envelope.body_blake3, "body checksum")?;
  if manifest_body_digest(&body_bytes) != expected_digest {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_checksum",
      "migration run manifest body checksum does not match",
    ));
  }
  let body = envelope.body;
  let algorithm = HashAlgorithm::from_u16(body.hash_algorithm)
    .ok_or_else(|| MigrationRunManifestErrorV1::invalid("migration_run_manifest_hash", "unknown migration hash algorithm"))?;
  let hash_width = algorithm.hash_length();
  let supported_readers = capability_set(&body.supported_reader_capabilities, "supported reader capabilities")?;
  let supported_writers = capability_set(&body.supported_writer_capabilities, "supported writer capabilities")?;
  let manifest = MigrationRunManifestV1 {
    path,
    workspace: PathBuf::from(body.workspace),
    source: PathBuf::from(body.source),
    destination: PathBuf::from(body.destination),
    created_at_ms: body.created_at_ms,
    database_id: decode_array(&body.database_id, "database identity")?,
    migration_id: decode_array(&body.migration_id, "migration identity")?,
    source_physical_instance_id: decode_array(&body.source_physical_instance_id, "source physical identity")?,
    destination_physical_instance_id: decode_array(&body.destination_physical_instance_id, "destination physical identity")?,
    holder_boot_id: decode_array(&body.holder_boot_id, "holder boot identity")?,
    source_path_digest: decode_array(&body.source_path_digest, "source path digest")?,
    destination_path_digest: decode_array(&body.destination_path_digest, "destination path digest")?,
    source_file_identity: body.source_file_identity.try_into()?,
    destination_parent_identity: body.destination_parent_identity.try_into()?,
    hash_algorithm: algorithm,
    source_file_size: body.source_file_size,
    source_complete_file_checksum: decode_array(&body.source_complete_file_checksum, "source checksum")?,
    source_header_sequence: body.source_header_sequence,
    source_selected_header_digest: decode_array(&body.source_selected_header_digest, "source header digest")?,
    source_capture_head: decode_vec(&body.source_capture_head, hash_width, "source capture HEAD")?,
    source_authority_digest: decode_array(&body.source_authority_digest, "source authority digest")?,
    source_authority_counts: body.source_authority_counts.into(),
    configuration_generation: body.configuration_generation,
    effective_configuration_fingerprint: decode_vec(
      &body.effective_configuration_fingerprint,
      hash_width,
      "effective configuration fingerprint",
    )?,
    system_family_registry_fingerprint: decode_vec(
      &body.system_family_registry_fingerprint,
      hash_width,
      "SystemFamily registry fingerprint",
    )?,
    capability_profile: BinaryCapabilityProfileV1::new(supported_readers, supported_writers),
    required_reader_capabilities: capability_set(&body.required_reader_capabilities, "required reader capabilities")?,
    required_writer_capabilities: capability_set(&body.required_writer_capabilities, "required writer capabilities")?,
    binary_source_commit: decode_array(&body.binary_source_commit, "binary source commit")?,
    binary_executable_sha256: decode_array(&body.binary_executable_sha256, "binary executable SHA-256")?,
    source_native_qualification_digest: decode_array(&body.source_native_qualification_digest, "source native qualification digest")?,
    destination_native_qualification_digest: decode_array(
      &body.destination_native_qualification_digest,
      "destination native qualification digest",
    )?,
    capture_max_bytes: body.capture_max_bytes,
    capture_free_reserve_bytes: body.capture_free_reserve_bytes,
    checkpoint_after_seconds: body.checkpoint_after_seconds,
    preflight_evidence_fingerprint: decode_array(&body.preflight_evidence_fingerprint, "preflight evidence fingerprint")?,
    bounds: body.bounds,
  };
  validate_manifest(&manifest)?;
  Ok(manifest)
}

fn validate_manifest(manifest: &MigrationRunManifestV1) -> Result<(), MigrationRunManifestErrorV1> {
  manifest.bounds.validate()?;
  manifest.bounds.validate_authority_counts(manifest.source_authority_counts)?;
  if manifest.created_at_ms == 0
    || manifest.created_at_ms > i64::MAX as u64
    || manifest.source_file_size == 0
    || manifest.source_header_sequence == 0
    || [
      manifest.database_id,
      manifest.migration_id,
      manifest.source_physical_instance_id,
      manifest.destination_physical_instance_id,
      manifest.holder_boot_id,
    ]
    .iter()
    .any(|value| value.iter().all(|byte| *byte == 0))
    || manifest.source_physical_instance_id == manifest.destination_physical_instance_id
    || manifest.source_path_digest.iter().all(|byte| *byte == 0)
    || manifest.destination_path_digest.iter().all(|byte| *byte == 0)
    || manifest.source_path_digest == manifest.destination_path_digest
    || manifest.source_complete_file_checksum.iter().all(|byte| *byte == 0)
    || manifest.source_selected_header_digest.iter().all(|byte| *byte == 0)
    || manifest.source_capture_head.iter().all(|byte| *byte == 0)
    || manifest.source_authority_digest.iter().all(|byte| *byte == 0)
    || manifest.effective_configuration_fingerprint.iter().all(|byte| *byte == 0)
    || manifest.system_family_registry_fingerprint.iter().all(|byte| *byte == 0)
    || manifest.binary_source_commit.iter().all(|byte| *byte == 0)
    || manifest.binary_executable_sha256.iter().all(|byte| *byte == 0)
    || manifest.source_native_qualification_digest.iter().all(|byte| *byte == 0)
    || manifest.destination_native_qualification_digest.iter().all(|byte| *byte == 0)
    || manifest.capture_max_bytes == 0
    || manifest.capture_free_reserve_bytes == 0
    || manifest.checkpoint_after_seconds == 0
    || manifest.preflight_evidence_fingerprint.iter().all(|byte| *byte == 0)
    || manifest.required_reader_capabilities.is_empty()
    || manifest.required_writer_capabilities.is_empty()
    || !manifest.required_reader_capabilities.difference(manifest.capability_profile.supported_reader_capabilities).is_empty()
    || !manifest.required_writer_capabilities.difference(manifest.capability_profile.supported_writer_capabilities).is_empty()
    || manifest.source_authority_counts.roots == 0
  {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_fields",
      "migration run manifest contains invalid identity, evidence, or capability fields",
    ));
  }
  for path in [&manifest.workspace, &manifest.source, &manifest.destination] {
    if !canonical_lexical_path(path) {
      return Err(MigrationRunManifestErrorV1::invalid(
        "migration_run_manifest_paths",
        "migration run manifest paths must be absolute, canonical UTF-8 paths",
      ));
    }
  }
  if manifest.source == manifest.destination
    || migration_native_path_digest_v1(&manifest.source) != manifest.source_path_digest
    || migration_native_path_digest_v1(&manifest.destination) != manifest.destination_path_digest
  {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_paths",
      "migration run manifest path bindings are inconsistent",
    ));
  }
  if !valid_platform_identity(manifest.source_file_identity) || !valid_platform_identity(manifest.destination_parent_identity) {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_file_identity",
      "migration run manifest contains an invalid platform file identity",
    ));
  }
  Ok(())
}

fn revalidate_host_bindings(manifest: &MigrationRunManifestV1) -> Result<(), MigrationRunManifestErrorV1> {
  let workspace = canonical_existing_directory(&manifest.workspace, "migration run workspace")?;
  if workspace != manifest.workspace {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_workspace",
      "migration run workspace no longer resolves to its recorded path",
    ));
  }
  validate_private_directory_readonly(&workspace, "migration run workspace")
    .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_workspace", error.to_string()))?;
  let source = canonical_existing_file(&manifest.source, "migration source")?;
  let source_identity = platform_file_identity(&source)
    .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_source_identity", error.to_string()))?;
  let source_size = fs::metadata(&source)
    .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_source_metadata", error.to_string()))?
    .len();
  if source_identity != manifest.source_file_identity || source_size != manifest.source_file_size {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_source_identity",
      "migration source physical identity or size differs from the immutable manifest",
    ));
  }
  let destination_parent = manifest.destination.parent().ok_or_else(|| {
    MigrationRunManifestErrorV1::invalid("migration_run_manifest_destination", "migration destination has no parent directory")
  })?;
  let destination_parent = canonical_existing_directory(destination_parent, "migration destination parent")?;
  let destination_parent_identity = platform_file_identity(&destination_parent)
    .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_destination_identity", error.to_string()))?;
  if destination_parent_identity != manifest.destination_parent_identity {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_destination_identity",
      "migration destination parent identity differs from the immutable manifest",
    ));
  }
  match fs::symlink_metadata(&manifest.destination) {
    Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
      return Err(MigrationRunManifestErrorV1::invalid(
        "migration_run_manifest_destination",
        "existing migration destination is not a no-follow regular file",
      ));
    }
    Ok(_) => {
      let destination_identity = platform_file_identity(&manifest.destination)
        .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_destination_identity", error.to_string()))?;
      if destination_identity.represents_same_physical_file_as(manifest.source_file_identity) {
        return Err(MigrationRunManifestErrorV1::invalid(
          "migration_run_manifest_destination_identity",
          "migration destination aliases the immutable source file",
        ));
      }
    }
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
    Err(error) => {
      return Err(MigrationRunManifestErrorV1::invalid("migration_run_manifest_destination", error.to_string()));
    }
  }
  Ok(())
}

fn prepare_workspace(path: &Path) -> Result<PathBuf, MigrationRunManifestErrorV1> {
  if !canonical_lexical_path(path) {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_workspace",
      "migration run workspace must be an absolute canonical UTF-8 path",
    ));
  }
  match fs::symlink_metadata(path) {
    Ok(_) => {
      let canonical = canonical_existing_directory(path, "migration run workspace")?;
      validate_private_directory_readonly(&canonical, "migration run workspace")
        .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_workspace", error.to_string()))?;
      let mut entries = fs::read_dir(&canonical)
        .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_workspace_state", error.to_string()))?;
      if entries
        .next()
        .transpose()
        .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_workspace_state", error.to_string()))?
        .is_some()
      {
        return Err(MigrationRunManifestErrorV1::invalid(
          "migration_run_manifest_workspace_state",
          "existing migration run workspace must be empty before manifest creation",
        ));
      }
      Ok(canonical)
    }
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
      let parent = path
        .parent()
        .ok_or_else(|| MigrationRunManifestErrorV1::invalid("migration_run_manifest_workspace", "migration run workspace has no parent"))?;
      let canonical_parent = canonical_existing_directory(parent, "migration run workspace parent")?;
      let name = path.file_name().ok_or_else(|| {
        MigrationRunManifestErrorV1::invalid("migration_run_manifest_workspace", "migration run workspace has no final component")
      })?;
      let canonical = canonical_parent.join(name);
      if canonical != path {
        return Err(MigrationRunManifestErrorV1::invalid(
          "migration_run_manifest_workspace",
          "migration run workspace parent does not resolve to the supplied path",
        ));
      }
      create_private_directory_synced(&canonical, &canonical_parent)
        .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_workspace_create", error.to_string()))?;
      Ok(canonical)
    }
    Err(error) => Err(MigrationRunManifestErrorV1::invalid("migration_run_manifest_workspace", error.to_string())),
  }
}

fn canonical_existing_file(path: &Path, role: &str) -> Result<PathBuf, MigrationRunManifestErrorV1> {
  if !canonical_lexical_path(path) {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_paths",
      format!("{role} must be an absolute canonical UTF-8 path"),
    ));
  }
  let metadata = fs::symlink_metadata(path)
    .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_source_metadata", error.to_string()))?;
  if metadata.file_type().is_symlink() || !metadata.is_file() {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_source_type",
      format!("{role} is not a no-follow regular file"),
    ));
  }
  let canonical = fs::canonicalize(path)
    .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_source_canonical", error.to_string()))?;
  if canonical != path {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_paths",
      format!("{role} does not resolve to the supplied canonical path"),
    ));
  }
  Ok(canonical)
}

fn canonical_existing_directory(path: &Path, role: &str) -> Result<PathBuf, MigrationRunManifestErrorV1> {
  if !canonical_lexical_path(path) {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_paths",
      format!("{role} must be an absolute canonical UTF-8 path"),
    ));
  }
  validate_existing_directory(path, role)
    .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_directory", error.to_string()))?;
  let canonical =
    fs::canonicalize(path).map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_directory", error.to_string()))?;
  if canonical != path {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_paths",
      format!("{role} does not resolve to the supplied canonical path"),
    ));
  }
  Ok(canonical)
}

fn canonical_lexical_path(path: &Path) -> bool {
  is_canonical_lexical_absolute_utf8_path(path)
}

fn read_bounded_manifest(path: &Path, cancellation: Option<&CancellationToken>) -> Result<Vec<u8>, MigrationRunManifestErrorV1> {
  if let Some(cancellation) = cancellation {
    check_cancelled(cancellation)?;
  }
  let mut file = open_regular_file_no_follow(path)
    .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_open", error.to_string()))?;
  validate_private_regular_file(path, &file, "migration run manifest")
    .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_permissions", error.to_string()))?;
  let length =
    file.metadata().map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_metadata", error.to_string()))?.len();
  if length == 0 || length > MAXIMUM_MANIFEST_BYTES {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_size",
      "migration run manifest length is zero or exceeds its one-megabyte bound",
    ));
  }
  let length =
    usize::try_from(length).map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_size", error.to_string()))?;
  let mut bytes = Vec::new();
  bytes
    .try_reserve_exact(length)
    .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_allocation", error.to_string()))?;
  file.read_to_end(&mut bytes).map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_read", error.to_string()))?;
  if bytes.len() != length {
    return Err(MigrationRunManifestErrorV1::invalid(
      "migration_run_manifest_size",
      "migration run manifest length changed while it was read",
    ));
  }
  if let Some(cancellation) = cancellation {
    check_cancelled(cancellation)?;
  }
  Ok(bytes)
}

fn manifest_body_digest(body: &[u8]) -> [u8; 32] {
  let mut hasher = blake3::Hasher::new();
  hasher.update(INTEGRITY_DOMAIN);
  hasher.update(body);
  *hasher.finalize().as_bytes()
}

fn utf8_path(path: &Path, role: &str) -> Result<String, MigrationRunManifestErrorV1> {
  path
    .to_str()
    .map(str::to_string)
    .ok_or_else(|| MigrationRunManifestErrorV1::invalid("migration_run_manifest_paths", format!("{role} path is not UTF-8")))
}

fn decode_array<const N: usize>(encoded: &str, role: &str) -> Result<[u8; N], MigrationRunManifestErrorV1> {
  let bytes = decode_vec(encoded, N, role)?;
  bytes.try_into().map_err(|_| MigrationRunManifestErrorV1::invalid("migration_run_manifest_hex", format!("{role} has the wrong width")))
}

fn decode_vec(encoded: &str, expected: usize, role: &str) -> Result<Vec<u8>, MigrationRunManifestErrorV1> {
  if encoded.len() != expected.saturating_mul(2) {
    return Err(MigrationRunManifestErrorV1::invalid("migration_run_manifest_hex", format!("{role} has the wrong encoded width")));
  }
  let bytes =
    hex::decode(encoded).map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_hex", format!("{role}: {error}")))?;
  if hex::encode(&bytes) != encoded {
    return Err(MigrationRunManifestErrorV1::invalid("migration_run_manifest_hex", format!("{role} is not canonical lowercase hex")));
  }
  Ok(bytes)
}

fn capability_set(encoded: &str, role: &str) -> Result<CapabilitySetV1, MigrationRunManifestErrorV1> {
  CapabilitySetV1::from_bytes(decode_array(encoded, role)?)
    .map_err(|error| MigrationRunManifestErrorV1::invalid("migration_run_manifest_capabilities", error.to_string()))
}

fn valid_platform_identity(identity: PlatformFileIdentityDescriptorV1) -> bool {
  matches!(identity.platform, 1 | 2)
    && identity.schema == 1
    && identity.flags & !(1 << 1) == 0
    && identity.volume_identity.iter().any(|byte| *byte != 0)
    && identity.file_identity.iter().any(|byte| *byte != 0)
    && if identity.flags & (1 << 1) != 0 {
      identity.birth_identity.iter().any(|byte| *byte != 0)
    } else {
      identity.birth_identity.iter().all(|byte| *byte == 0)
    }
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), MigrationRunManifestErrorV1> {
  if cancellation.is_cancelled() {
    Err(MigrationRunManifestErrorV1::invalid("migration_run_manifest_canceled", "offline migration run was canceled"))
  } else {
    Ok(())
  }
}

impl From<PlatformFileIdentityDescriptorV1> for PersistedFileIdentityV1 {
  fn from(identity: PlatformFileIdentityDescriptorV1) -> Self {
    Self {
      platform: identity.platform,
      schema: identity.schema,
      flags: identity.flags,
      volume_identity: hex::encode(identity.volume_identity),
      file_identity: hex::encode(identity.file_identity),
      birth_identity: hex::encode(identity.birth_identity),
    }
  }
}

impl TryFrom<PersistedFileIdentityV1> for PlatformFileIdentityDescriptorV1 {
  type Error = MigrationRunManifestErrorV1;

  fn try_from(identity: PersistedFileIdentityV1) -> Result<Self, Self::Error> {
    Ok(Self {
      platform: identity.platform,
      schema: identity.schema,
      flags: identity.flags,
      volume_identity: decode_array(&identity.volume_identity, "volume identity")?,
      file_identity: decode_array(&identity.file_identity, "file identity")?,
      birth_identity: decode_array(&identity.birth_identity, "birth identity")?,
    })
  }
}

impl From<AuthorityInventoryCountsV1> for PersistedAuthorityCountsV1 {
  fn from(counts: AuthorityInventoryCountsV1) -> Self {
    Self {
      protected_families: counts.protected_families,
      modules: counts.modules,
      snapshots: counts.snapshots,
      forks: counts.forks,
      symlinks: counts.symlinks,
      history_roots: counts.history_roots,
      peers: counts.peers,
      sync_states: counts.sync_states,
      tasks: counts.tasks,
      plugins: counts.plugins,
      roots: counts.roots,
    }
  }
}

impl From<PersistedAuthorityCountsV1> for AuthorityInventoryCountsV1 {
  fn from(counts: PersistedAuthorityCountsV1) -> Self {
    Self {
      protected_families: counts.protected_families,
      modules: counts.modules,
      snapshots: counts.snapshots,
      forks: counts.forks,
      symlinks: counts.symlinks,
      history_roots: counts.history_roots,
      peers: counts.peers,
      sync_states: counts.sync_states,
      tasks: counts.tasks,
      plugins: counts.plugins,
      roots: counts.roots,
    }
  }
}
