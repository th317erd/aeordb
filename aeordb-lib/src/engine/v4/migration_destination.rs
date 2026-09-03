//! Destination-first initialization for one preflight-admitted v4 shadow.
//!
//! This owner creates a separate physical file and composes the existing KV,
//! header, durability, and first-authority owners. It never opens the v3 source.

use std::error::Error;
use std::fmt::{self, Debug, Display, Formatter};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DATABASE_HEADER_V4_SLOT_LENGTH, DatabaseHeaderV4, encode_database_header_slot};
use super::first_authority::{
  FirstAuthorityPublicationReceiptV1, FirstAuthorityPublicationRequestV1, PreparedNamespaceTreeV0, V4FirstAuthorityPublisher,
};
use super::private_workspace::is_canonical_lexical_absolute_utf8_path;
use super::hash::digest_parts;
use super::migration_preflight::MigrationPreflightPermitV1;
use super::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, encode_semantic_state_object};
use super::system_family::embedded_system_family_registry;
use crate::engine::disk_kv_store::DiskKVStore;
use crate::engine::durability_coordinator::DurabilityCoordinator;
use crate::engine::emergency_spill::create_new_regular_file_read_write_no_follow;
use crate::engine::kv_stages::initial_block_size;
use crate::engine::native_durability::{
  PlatformFileIdentityDescriptorV1, platform_file_identity, platform_file_identity_from_file, sync_directory_native, sync_file_all_native,
  verify_file_bytes_native, write_file_at_native,
};

const INITIAL_HEADER_SEQUENCE: u64 = 1;
const INITIAL_WRITE_SEQUENCE: u64 = 1;
const SYSTEM_FAMILY_REGISTRY_VERSION: u16 = 1;
const NVT_VERSION: u8 = 1;
const PATH_DIGEST_DOMAIN: &[u8] = b"aeordb.migration-destination-path.v1\0";
const CLOSURE_DOMAIN: &[u8] = b"aeordb.migration-destination-closure.v1\0";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationDestinationPathObservationV1 {
  path: PathBuf,
  parent: PathBuf,
  path_digest: [u8; 32],
  parent_identity: PlatformFileIdentityDescriptorV1,
}

impl MigrationDestinationPathObservationV1 {
  pub fn path(&self) -> &Path {
    &self.path
  }

  pub fn parent(&self) -> &Path {
    &self.parent
  }

  pub const fn path_digest(&self) -> [u8; 32] {
    self.path_digest
  }

  pub const fn parent_identity(&self) -> PlatformFileIdentityDescriptorV1 {
    self.parent_identity
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationDestinationArtifactV1 {
  path: PathBuf,
  path_digest: [u8; 32],
  expected_database_id: [u8; 16],
  expected_physical_instance_id: [u8; 16],
  file_identity: Option<PlatformFileIdentityDescriptorV1>,
  file_identity_error: Option<String>,
  first_authority: Option<FirstAuthorityPublicationReceiptV1>,
}

impl MigrationDestinationArtifactV1 {
  pub fn path(&self) -> &Path {
    &self.path
  }

  pub const fn path_digest(&self) -> [u8; 32] {
    self.path_digest
  }

  pub const fn expected_database_id(&self) -> [u8; 16] {
    self.expected_database_id
  }

  pub const fn expected_physical_instance_id(&self) -> [u8; 16] {
    self.expected_physical_instance_id
  }

  pub const fn file_identity(&self) -> Option<PlatformFileIdentityDescriptorV1> {
    self.file_identity
  }

  pub fn file_identity_error(&self) -> Option<&str> {
    self.file_identity_error.as_deref()
  }

  pub const fn first_authority(&self) -> Option<&FirstAuthorityPublicationReceiptV1> {
    self.first_authority.as_ref()
  }
}

#[derive(Debug)]
pub struct MigrationDestinationInitializationErrorV1 {
  code: &'static str,
  message: String,
  artifact: Option<Box<MigrationDestinationArtifactV1>>,
}

impl MigrationDestinationInitializationErrorV1 {
  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub const fn artifact(&self) -> Option<&MigrationDestinationArtifactV1> {
    match self.artifact.as_ref() {
      Some(artifact) => Some(artifact),
      None => None,
    }
  }

  fn before(code: &'static str, message: impl Into<String>) -> Self {
    Self { code, message: message.into(), artifact: None }
  }

  fn after(
    code: &'static str,
    message: impl Into<String>,
    destination: &MigrationDestinationPathObservationV1,
    permit: &MigrationPreflightPermitV1,
    file_identity: Result<PlatformFileIdentityDescriptorV1, String>,
    first_authority: Option<FirstAuthorityPublicationReceiptV1>,
  ) -> Self {
    let (file_identity, file_identity_error) = match file_identity {
      Ok(identity) => (Some(identity), None),
      Err(error) => (None, Some(error)),
    };
    Self {
      code,
      message: message.into(),
      artifact: Some(Box::new(MigrationDestinationArtifactV1 {
        path: destination.path.clone(),
        path_digest: destination.path_digest,
        expected_database_id: permit.database_id(),
        expected_physical_instance_id: permit.destination_physical_instance_id(),
        file_identity,
        file_identity_error,
        first_authority,
      })),
    }
  }
}

impl Display for MigrationDestinationInitializationErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.message)
  }
}

impl Error for MigrationDestinationInitializationErrorV1 {}

pub struct MigrationDestinationInitializationRequestV1<'a> {
  pub permit: &'a MigrationPreflightPermitV1,
  pub destination: &'a MigrationDestinationPathObservationV1,
  pub created_at_ms: u64,
  pub writer_fence_epoch: u64,
  pub cancellation: &'a CancellationToken,
}

pub struct InitializedMigrationDestinationV1 {
  destination: MigrationDestinationPathObservationV1,
  file_identity: PlatformFileIdentityDescriptorV1,
  first_authority: FirstAuthorityPublicationReceiptV1,
  publisher: Arc<V4FirstAuthorityPublisher>,
}

impl Debug for InitializedMigrationDestinationV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("InitializedMigrationDestinationV1")
      .field("destination", &self.destination)
      .field("file_identity", &self.file_identity)
      .field("first_authority", &self.first_authority)
      .finish_non_exhaustive()
  }
}

impl InitializedMigrationDestinationV1 {
  pub fn path(&self) -> &Path {
    self.destination.path()
  }

  pub const fn path_digest(&self) -> [u8; 32] {
    self.destination.path_digest()
  }

  pub const fn parent_identity(&self) -> PlatformFileIdentityDescriptorV1 {
    self.destination.parent_identity()
  }

  pub const fn file_identity(&self) -> PlatformFileIdentityDescriptorV1 {
    self.file_identity
  }

  pub const fn first_authority(&self) -> &FirstAuthorityPublicationReceiptV1 {
    &self.first_authority
  }

  pub fn publisher(&self) -> &V4FirstAuthorityPublisher {
    &self.publisher
  }

  pub fn shared_publisher(&self) -> Arc<V4FirstAuthorityPublisher> {
    self.publisher.clone()
  }
}

pub fn observe_migration_destination_path_v1(
  path: impl AsRef<Path>,
) -> Result<MigrationDestinationPathObservationV1, MigrationDestinationInitializationErrorV1> {
  let path = path.as_ref();
  if !is_canonical_lexical_absolute_utf8_path(path) {
    return Err(MigrationDestinationInitializationErrorV1::before(
      "migration_destination_path_noncanonical",
      "destination must be an absolute path without current- or parent-directory components",
    ));
  }
  let file_name = path.file_name().ok_or_else(|| {
    MigrationDestinationInitializationErrorV1::before("migration_destination_path_noncanonical", "destination has no file name")
  })?;
  match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.file_type().is_symlink() => {
      return Err(MigrationDestinationInitializationErrorV1::before("migration_destination_symlink", "destination is an existing symlink"));
    }
    Ok(_) => {
      return Err(MigrationDestinationInitializationErrorV1::before("migration_destination_exists", "destination already exists"));
    }
    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
    Err(error) => {
      return Err(MigrationDestinationInitializationErrorV1::before(
        "migration_destination_path_io",
        format!("destination metadata failed: {error}"),
      ));
    }
  }
  let parent = path.parent().ok_or_else(|| {
    MigrationDestinationInitializationErrorV1::before("migration_destination_path_noncanonical", "destination has no parent directory")
  })?;
  let canonical_parent = fs::canonicalize(parent).map_err(|error| {
    MigrationDestinationInitializationErrorV1::before(
      "migration_destination_parent",
      format!("destination parent canonicalization failed: {error}"),
    )
  })?;
  let parent_metadata = fs::metadata(&canonical_parent).map_err(|error| {
    MigrationDestinationInitializationErrorV1::before(
      "migration_destination_parent",
      format!("destination parent metadata failed: {error}"),
    )
  })?;
  if !parent_metadata.is_dir() {
    return Err(MigrationDestinationInitializationErrorV1::before("migration_destination_parent", "destination parent is not a directory"));
  }
  let canonical_path = canonical_parent.join(file_name);
  let parent_identity = platform_file_identity(&canonical_parent).map_err(|error| {
    MigrationDestinationInitializationErrorV1::before(
      "migration_destination_parent_identity",
      format!("destination parent identity failed: {error}"),
    )
  })?;
  Ok(MigrationDestinationPathObservationV1 {
    path_digest: migration_native_path_digest_v1(&canonical_path),
    path: canonical_path,
    parent: canonical_parent,
    parent_identity,
  })
}

pub fn initialize_migration_destination_v1(
  request: MigrationDestinationInitializationRequestV1<'_>,
) -> Result<InitializedMigrationDestinationV1, MigrationDestinationInitializationErrorV1> {
  initialize_migration_destination_with_observer_v1(request, &mut NoopMigrationDestinationInitializationObserverV1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
enum MigrationDestinationInitializationPhaseV1 {
  Created = 1,
  KvInitialized = 2,
  HeaderSlotA = 3,
  HeaderSlotB = 4,
  HeadersSynced = 5,
  HeadersVerified = 6,
  FirstAuthorityPublished = 7,
  FileSynced = 8,
  ParentSynced = 9,
}

impl MigrationDestinationInitializationPhaseV1 {
  #[cfg(test)]
  const ALL: [Self; 9] = [
    Self::Created,
    Self::KvInitialized,
    Self::HeaderSlotA,
    Self::HeaderSlotB,
    Self::HeadersSynced,
    Self::HeadersVerified,
    Self::FirstAuthorityPublished,
    Self::FileSynced,
    Self::ParentSynced,
  ];
}

trait MigrationDestinationInitializationObserverV1 {
  fn phase_completed(&mut self, phase: MigrationDestinationInitializationPhaseV1) -> Result<(), String>;
}

struct NoopMigrationDestinationInitializationObserverV1;

impl MigrationDestinationInitializationObserverV1 for NoopMigrationDestinationInitializationObserverV1 {
  fn phase_completed(&mut self, _phase: MigrationDestinationInitializationPhaseV1) -> Result<(), String> {
    Ok(())
  }
}

fn initialize_migration_destination_with_observer_v1(
  request: MigrationDestinationInitializationRequestV1<'_>,
  observer: &mut impl MigrationDestinationInitializationObserverV1,
) -> Result<InitializedMigrationDestinationV1, MigrationDestinationInitializationErrorV1> {
  validate_before_create(&request)?;
  let prepared = prepare_before_create(&request)?;
  revalidate_destination_absent(request.destination)?;
  if request.cancellation.is_cancelled() {
    return Err(MigrationDestinationInitializationErrorV1::before(
      "migration_destination_canceled",
      "migration was canceled before destination creation",
    ));
  }

  let file = create_new_regular_file_read_write_no_follow(request.destination.path()).map_err(|error| {
    MigrationDestinationInitializationErrorV1::before("migration_destination_create", format!("destination creation failed: {error}"))
  })?;
  let initial_identity = platform_file_identity_from_file(&file).map_err(|error| {
    MigrationDestinationInitializationErrorV1::after(
      "migration_destination_file_identity",
      format!("created destination identity failed: {error}"),
      request.destination,
      request.permit,
      Err(error.to_string()),
      None,
    )
  })?;
  let created_error = |code, message| {
    MigrationDestinationInitializationErrorV1::after(code, message, request.destination, request.permit, Ok(initial_identity), None)
  };
  revalidate_parent_after_create(request.destination, &created_error)?;
  revalidate_destination_file_after_create(request.destination, initial_identity, &created_error)?;
  observe_initialization_phase(observer, MigrationDestinationInitializationPhaseV1::Created, &created_error)?;
  if request.cancellation.is_cancelled() {
    return Err(created_error("migration_destination_canceled", "migration canceled after destination creation".to_string()));
  }

  let coordinator = Arc::new(DurabilityCoordinator::new());
  let kv = DiskKVStore::create_with_coordinator(
    file.try_clone().map_err(|error| created_error("migration_destination_file_clone", error.to_string()))?,
    request.permit.hash_algorithm(),
    prepared.header.kv_block_offset,
    prepared.header.hot_tail_offset,
    0,
    coordinator.clone(),
  )
  .map_err(|error| created_error("migration_destination_kv", error.to_string()))?;
  observe_initialization_phase(observer, MigrationDestinationInitializationPhaseV1::KvInitialized, &created_error)?;
  if request.cancellation.is_cancelled() {
    return Err(created_error("migration_destination_canceled", "migration canceled after destination KV initialization".to_string()));
  }

  write_file_at_native(&file, 0, &prepared.header_slot)
    .map_err(|error| created_error("migration_destination_header_write", error.to_string()))?;
  observe_initialization_phase(observer, MigrationDestinationInitializationPhaseV1::HeaderSlotA, &created_error)?;
  write_file_at_native(&file, DATABASE_HEADER_V4_SLOT_LENGTH as u64, &prepared.header_slot)
    .map_err(|error| created_error("migration_destination_header_write", error.to_string()))?;
  observe_initialization_phase(observer, MigrationDestinationInitializationPhaseV1::HeaderSlotB, &created_error)?;
  sync_file_all_native(&file).map_err(|error| created_error("migration_destination_header_sync", error.to_string()))?;
  observe_initialization_phase(observer, MigrationDestinationInitializationPhaseV1::HeadersSynced, &created_error)?;
  verify_file_bytes_native(&file, 0, &prepared.header_slot)
    .map_err(|error| created_error("migration_destination_header_readback", error.to_string()))?;
  verify_file_bytes_native(&file, DATABASE_HEADER_V4_SLOT_LENGTH as u64, &prepared.header_slot)
    .map_err(|error| created_error("migration_destination_header_readback", error.to_string()))?;
  observe_initialization_phase(observer, MigrationDestinationInitializationPhaseV1::HeadersVerified, &created_error)?;
  if request.cancellation.is_cancelled() {
    return Err(created_error("migration_destination_canceled", "migration canceled before first shadow authority".to_string()));
  }

  let publisher = Arc::new(
    V4FirstAuthorityPublisher::new(kv, coordinator).map_err(|error| created_error("migration_destination_publisher", error.to_string()))?,
  );
  let first_authority = match publisher.publish(&prepared.first_authority) {
    Ok(receipt) => receipt,
    Err(error) => {
      let committed_receipt = error.committed_receipt().cloned();
      return Err(MigrationDestinationInitializationErrorV1::after(
        error.code(),
        error.to_string(),
        request.destination,
        request.permit,
        Ok(initial_identity),
        committed_receipt,
      ));
    }
  };
  let published_error = |code, message| {
    MigrationDestinationInitializationErrorV1::after(
      code,
      message,
      request.destination,
      request.permit,
      Ok(initial_identity),
      Some(first_authority.clone()),
    )
  };
  observe_initialization_phase(observer, MigrationDestinationInitializationPhaseV1::FirstAuthorityPublished, &published_error)?;
  sync_file_all_native(&file).map_err(|error| published_error("migration_destination_final_sync", error.to_string()))?;
  observe_initialization_phase(observer, MigrationDestinationInitializationPhaseV1::FileSynced, &published_error)?;
  revalidate_parent_after_create(request.destination, &published_error)?;
  revalidate_destination_file_after_create(request.destination, initial_identity, &published_error)?;
  sync_directory_native(request.destination.parent())
    .map_err(|error| published_error("migration_destination_parent_sync", error.to_string()))?;
  revalidate_parent_after_create(request.destination, &published_error)?;
  revalidate_destination_file_after_create(request.destination, initial_identity, &published_error)?;
  observe_initialization_phase(observer, MigrationDestinationInitializationPhaseV1::ParentSynced, &published_error)?;
  let final_identity = platform_file_identity_from_file(&file)
    .map_err(|error| published_error("migration_destination_file_identity", format!("final destination identity failed: {error}")))?;
  if !initial_identity.represents_same_physical_file_as(final_identity) {
    return Err(published_error(
      "migration_destination_file_replaced",
      "destination physical identity changed during initialization".to_string(),
    ));
  }
  revalidate_destination_file_after_create(request.destination, initial_identity, &published_error)?;

  let observation = publisher.observe().map_err(|error| published_error("migration_destination_final_observation", error.to_string()))?;
  let header = &observation.selected.header;
  if observation.selected.redundancy_degraded
    || header.database_id != request.permit.database_id()
    || header.physical_instance_id != request.permit.destination_physical_instance_id()
    || header.writer_fence_epoch != request.writer_fence_epoch
    || header.head_hash != first_authority.namespace_root.root_hash
  {
    return Err(published_error(
      "migration_destination_final_mismatch",
      "selected destination authority does not match the admitted shadow".to_string(),
    ));
  }

  Ok(InitializedMigrationDestinationV1 {
    destination: request.destination.clone(),
    file_identity: final_identity,
    first_authority,
    publisher,
  })
}

fn observe_initialization_phase(
  observer: &mut impl MigrationDestinationInitializationObserverV1,
  phase: MigrationDestinationInitializationPhaseV1,
  created_error: &impl Fn(&'static str, String) -> MigrationDestinationInitializationErrorV1,
) -> Result<(), MigrationDestinationInitializationErrorV1> {
  observer.phase_completed(phase).map_err(|message| created_error("migration_destination_fault_injected", message))
}

fn revalidate_parent_after_create(
  destination: &MigrationDestinationPathObservationV1,
  created_error: &impl Fn(&'static str, String) -> MigrationDestinationInitializationErrorV1,
) -> Result<(), MigrationDestinationInitializationErrorV1> {
  let parent_identity = platform_file_identity(destination.parent())
    .map_err(|error| created_error("migration_destination_parent_identity", format!("destination parent identity failed: {error}")))?;
  if parent_identity != destination.parent_identity {
    return Err(created_error(
      "migration_destination_parent_replaced",
      "destination parent identity changed during initialization".to_string(),
    ));
  }
  Ok(())
}

fn revalidate_destination_file_after_create(
  destination: &MigrationDestinationPathObservationV1,
  expected_identity: PlatformFileIdentityDescriptorV1,
  created_error: &impl Fn(&'static str, String) -> MigrationDestinationInitializationErrorV1,
) -> Result<(), MigrationDestinationInitializationErrorV1> {
  let path_identity = platform_file_identity(destination.path())
    .map_err(|error| created_error("migration_destination_file_identity", format!("destination path identity failed: {error}")))?;
  if !expected_identity.represents_same_physical_file_as(path_identity) {
    return Err(created_error(
      "migration_destination_file_replaced",
      "destination path no longer identifies the created physical file".to_string(),
    ));
  }
  Ok(())
}

struct PreparedMigrationDestinationV1 {
  header: DatabaseHeaderV4,
  header_slot: [u8; DATABASE_HEADER_V4_SLOT_LENGTH],
  first_authority: FirstAuthorityPublicationRequestV1,
}

fn validate_before_create(
  request: &MigrationDestinationInitializationRequestV1<'_>,
) -> Result<(), MigrationDestinationInitializationErrorV1> {
  if request.created_at_ms == 0 || request.created_at_ms > i64::MAX as u64 {
    return Err(MigrationDestinationInitializationErrorV1::before(
      "migration_destination_time",
      "destination creation time must fit the signed persistent timestamp range",
    ));
  }
  if request.writer_fence_epoch == 0 {
    return Err(MigrationDestinationInitializationErrorV1::before(
      "migration_destination_fence",
      "destination writer fence must be nonzero",
    ));
  }
  if request.cancellation.is_cancelled() {
    return Err(MigrationDestinationInitializationErrorV1::before("migration_destination_canceled", "migration is canceled"));
  }
  if request.destination.path_digest != request.permit.destination_path_digest()
    || request.destination.parent_identity != request.permit.destination_parent_identity()
  {
    return Err(MigrationDestinationInitializationErrorV1::before(
      "migration_destination_identity",
      "destination observation does not match the admitted path and parent identity",
    ));
  }
  let parent_identity = platform_file_identity(request.destination.parent()).map_err(|error| {
    MigrationDestinationInitializationErrorV1::before(
      "migration_destination_parent_identity",
      format!("destination parent identity recheck failed: {error}"),
    )
  })?;
  if parent_identity != request.destination.parent_identity {
    return Err(MigrationDestinationInitializationErrorV1::before(
      "migration_destination_parent_replaced",
      "destination parent identity changed after preflight",
    ));
  }
  Ok(())
}

fn revalidate_destination_absent(
  destination: &MigrationDestinationPathObservationV1,
) -> Result<(), MigrationDestinationInitializationErrorV1> {
  match fs::symlink_metadata(destination.path()) {
    Ok(metadata) if metadata.file_type().is_symlink() => Err(MigrationDestinationInitializationErrorV1::before(
      "migration_destination_symlink",
      "destination became a symlink after preflight",
    )),
    Ok(_) => Err(MigrationDestinationInitializationErrorV1::before("migration_destination_exists", "destination appeared after preflight")),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
    Err(error) => Err(MigrationDestinationInitializationErrorV1::before(
      "migration_destination_path_io",
      format!("destination metadata recheck failed: {error}"),
    )),
  }
}

fn prepare_before_create(
  request: &MigrationDestinationInitializationRequestV1<'_>,
) -> Result<PreparedMigrationDestinationV1, MigrationDestinationInitializationErrorV1> {
  let algorithm = request.permit.hash_algorithm();
  let registry = embedded_system_family_registry(algorithm)
    .map_err(|error| MigrationDestinationInitializationErrorV1::before("migration_destination_registry", error.to_string()))?;
  if registry.operational_fingerprint != request.permit.system_family_registry_fingerprint() {
    return Err(MigrationDestinationInitializationErrorV1::before(
      "migration_destination_registry",
      "embedded SystemFamily registry differs from preflight",
    ));
  }
  let kv_block_length = initial_block_size();
  let hot_tail_offset = DATABASE_HEADER_V4_DATA_OFFSET.checked_add(kv_block_length).ok_or_else(|| {
    MigrationDestinationInitializationErrorV1::before("migration_destination_layout", "initial destination layout overflowed")
  })?;
  let hash_width = algorithm.hash_length();
  let required_reader_capabilities = request.permit.required_reader_capabilities().into_bytes();
  let required_writer_capabilities = request.permit.required_writer_capabilities().into_bytes();
  let header = DatabaseHeaderV4 {
    hash_algorithm: algorithm,
    slot_sequence: INITIAL_HEADER_SEQUENCE,
    created_at_ms: request.created_at_ms,
    updated_at_ms: request.created_at_ms,
    database_id: request.permit.database_id(),
    write_sequence_high_water: INITIAL_WRITE_SEQUENCE,
    required_reader_capabilities,
    kv_block_offset: DATABASE_HEADER_V4_DATA_OFFSET,
    kv_block_length,
    kv_block_version: DiskKVStore::CURRENT_KV_BLOCK_VERSION,
    kv_block_stage: 0,
    resize_in_progress: false,
    resize_target_stage: 0,
    nvt_offset: hot_tail_offset,
    nvt_length: 0,
    nvt_version: NVT_VERSION,
    backup_type: 0,
    hot_tail_offset,
    buffer_kvs_offset: 0,
    buffer_nvt_offset: 0,
    entry_count: 0,
    head_hash: vec![0; hash_width],
    base_hash: vec![0; hash_width],
    target_hash: vec![0; hash_width],
    required_writer_capabilities,
    system_family_registry_version: SYSTEM_FAMILY_REGISTRY_VERSION,
    system_family_registry_fingerprint: registry.operational_fingerprint.clone(),
    writer_fence_epoch: request.writer_fence_epoch,
    physical_instance_id: request.permit.destination_physical_instance_id(),
  };
  let header_slot = encode_database_header_slot(&header)
    .map_err(|error| MigrationDestinationInitializationErrorV1::before("migration_destination_header", error.to_string()))?;
  let semantic_state = encode_semantic_state_object(
    &SemanticStateWriteV1 {
      required_capabilities: required_reader_capabilities,
      availability: SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured },
    },
    algorithm,
  )
  .map_err(|error| MigrationDestinationInitializationErrorV1::before("migration_destination_semantic", error.to_string()))?;
  let parent_identity_bytes = request.destination.parent_identity.to_bytes();
  let evidence_fingerprint = request.permit.evidence_fingerprint();
  let path_digest = request.destination.path_digest;
  let typed_closure_digest = digest_parts(
    algorithm,
    &[
      CLOSURE_DOMAIN,
      &evidence_fingerprint,
      &path_digest,
      &parent_identity_bytes,
      &required_reader_capabilities,
      &required_writer_capabilities,
    ],
  );
  let first_authority = FirstAuthorityPublicationRequestV1 {
    database_id: request.permit.database_id(),
    transaction_id: request.permit.migration_id(),
    created_at_ms: request.created_at_ms,
    namespace_tree: PreparedNamespaceTreeV0 { root_hash: digest_parts(algorithm, &[b"dirc:"]), stored_value: Vec::new() },
    semantic_state,
    required_capabilities: required_reader_capabilities,
    typed_closure_digest,
    authority_identity: b"HEAD".to_vec(),
  };
  Ok(PreparedMigrationDestinationV1 { header, header_slot, first_authority })
}

pub(super) fn migration_native_path_digest_v1(path: &Path) -> [u8; 32] {
  let mut hasher = blake3::Hasher::new();
  hasher.update(PATH_DIGEST_DOMAIN);
  #[cfg(unix)]
  {
    use std::os::unix::ffi::OsStrExt;
    hasher.update(&[1]);
    hasher.update(path.as_os_str().as_bytes());
  }
  #[cfg(windows)]
  {
    use std::os::windows::ffi::OsStrExt;
    hasher.update(&[2]);
    for unit in path.as_os_str().encode_wide() {
      hasher.update(&unit.to_le_bytes());
    }
  }
  *hasher.finalize().as_bytes()
}

#[cfg(test)]
#[path = "../../../spec/engine/migration_destination_internal_spec.rs"]
mod migration_destination_internal_spec;
