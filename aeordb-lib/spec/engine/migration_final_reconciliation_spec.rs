use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use aeordb::engine::btree::{BTREE_CONVERSION_THRESHOLD, BTreeNode, InternalNode, LeafNode, btree_plan_from_entries};
use aeordb::engine::directory_entry::{ChildEntry, deserialize_child_entries, serialize_child_entries};
use aeordb::engine::file_header::read_active_header;
use aeordb::engine::v4::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1};
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy, MemoryPressure};
use aeordb::engine::native_durability::{PlatformFileIdentityDescriptorV1, platform_file_identity};
use aeordb::engine::storage_engine::EntryData;
use aeordb::engine::v4::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use aeordb::engine::v4::contract_generated::CONTRACT_REGISTRY_SHA256;
use aeordb::engine::v4::entity::EntryTypeV4;
use aeordb::engine::v4::first_authority::{
  ImmutableEntityBatchPublicationRequestV1, ImmutableEntityWriteV1, SuccessorAuthorityPublicationRequestV1, V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::migration_base_clone_execution::{
  MigrationBaseCloneEntrySourceV1, MigrationBaseCloneSeedKindV1, MigrationBaseCloneSeedV1,
};
use aeordb::engine::v4::migration_capture_replay::MigrationCaptureReplayAuthorityTemplateV1;
use aeordb::engine::v4::migration_destination::{
  InitializedMigrationDestinationV1, MigrationDestinationInitializationRequestV1, initialize_migration_destination_v1,
  observe_migration_destination_path_v1,
};
use aeordb::engine::v4::migration_final_reconciliation::{
  MigrationFinalNamespaceReconciliationReceiptV1, MigrationFinalNamespaceReconciliationRequestV1, MigrationMerkleChangeKindV1,
  MigrationMerkleChangeV1, MigrationMerkleDiffRequestV1, MigrationMerkleDiffSinkV1, MigrationSourceWriteFreezeRequestV1,
  MigrationSourceWriteFreezeV1, acquire_migration_source_write_freeze_v1, execute_final_namespace_reconciliation_v1,
  stream_strict_migration_merkle_diff_v1,
};
use aeordb::engine::v4::migration_control::{
  MIGRATION_PROGRESS_FLAG_DESTINATION_FULL_VERIFIED, MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
  MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD, MigrationPhaseV1, MigrationProgressStateV1, decode_migration_progress_control,
};
use aeordb::engine::v4::migration_owner::{
  MigrationAcquisitionRequestV1, MigrationDestinationVerificationCompletionRequestV1, MigrationDestinationVerificationRequestV1,
  MigrationFinalFreezeCompletionRequestV1, MigrationLeaseRenewalRequestV1, MigrationProgressTransitionRequestV1, MigrationStateOwnerV1,
};
use aeordb::engine::v4::migration_final_authority_reconciliation::{
  MigrationFinalAuthorityInventoryClosureV1, MigrationFinalAuthorityInventorySourceV1, MigrationFinalAuthorityReconciliationErrorV1,
  MigrationFinalAuthorityReconciliationRequestV1, MigrationFinalAuthoritySeedCountsV1, MigrationFinalAuthoritySeedV1,
  MigrationFinalPriorRootMappingLookupV1, MigrationFinalRootMappingClosureV1, MigrationFinalRootMappingSinkV1, MigrationFinalRootMappingV1,
  execute_final_authority_reconciliation_v1,
};
use aeordb::engine::v4::migration_preflight::{
  AuthorityInventoryCountsV1, CapacityRoleV1, MigrationBinaryEvidenceV1, MigrationCapacityObservationV1, MigrationConfigurationEvidenceV1,
  MigrationIdentityEvidenceV1, MigrationMemoryEvidenceV1, MigrationNativeEvidenceV1, MigrationPreflightPermitV1,
  MigrationPreflightRequestV1, MigrationRecoveryEvidenceV1, MigrationSourceEvidenceV1, NativeCutoverCapabilitiesV1,
  SourceAuthorityInventoryV1, StrictVerificationEvidenceV1, StrictVerificationStateV1, admit_migration_preflight_v1,
};
use aeordb::engine::v4::migration_root_map_owner::{
  LegacyRootMapOwnerV1, LegacyRootMapProducerSinkV1, LegacyRootMapPublicationRequestV1, LegacyRootMapStagingWorkspaceV1,
  LegacyRootMapWorkspaceIdentityV1, LegacyRootMapWorkspaceOptionsV1, VerifiedLegacyRootMapReaderV1,
};
use aeordb::engine::v4::migration_source_gc::{MigrationSourceGcSuspensionOwnerV1, MigrationSourceGcSuspensionRequestV1};
use aeordb::engine::v4::namespace::{
  NamespaceRootWriteV1, SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, encode_namespace_root,
  encode_semantic_state_object, decode_namespace_root,
};
use aeordb::engine::v4::system_family::embedded_system_family_registry;
use aeordb::engine::v4::system_control::SystemControlKindV1;
use aeordb::engine::{
  CompressionAlgorithm, EngineError, EngineResult, EntryHeader, EntryType, FileRecord, HashAlgorithm, NamespaceMutationBatch,
  NamespaceMutationCoordinator, NamespaceMutationKind, NamespaceMutationSourceIdentity, StorageEngine,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const GIB: u64 = 1024 * 1024 * 1024;
const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

fn id(first: u8) -> [u8; 16] {
  std::array::from_fn(|offset| first.wrapping_add(offset as u8))
}

fn digest(first: u8) -> [u8; 32] {
  std::array::from_fn(|offset| first.wrapping_add(offset as u8))
}

fn native(first: u8) -> NativeCutoverCapabilitiesV1 {
  NativeCutoverCapabilitiesV1 {
    data_barrier: true,
    file_barrier: true,
    parent_directory_sync: true,
    durable_replace: true,
    preallocation: true,
    stable_file_identity: true,
    read_back_verified: true,
    qualification_digest: digest(first),
  }
}

struct Fixture {
  _directory: TempDir,
  path: std::path::PathBuf,
  engine: Arc<StorageEngine>,
  permit: MigrationPreflightPermitV1,
}

impl Fixture {
  fn new() -> Self {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.aeordb");
    let engine = Arc::new(StorageEngine::create(path.to_str().unwrap()).unwrap());
    let permit = permit_for(&path, directory.path(), None);
    Self { _directory: directory, path, engine, permit }
  }
}

fn permit_for(
  source_path: &std::path::Path,
  destination_parent: &std::path::Path,
  source_identity_override: Option<PlatformFileIdentityDescriptorV1>,
) -> MigrationPreflightPermitV1 {
  permit_for_destination(source_path, platform_file_identity(destination_parent).unwrap(), digest(0x30), source_identity_override)
}

fn permit_for_destination(
  source_path: &std::path::Path,
  destination_parent_identity: PlatformFileIdentityDescriptorV1,
  destination_path_digest: [u8; 32],
  source_identity_override: Option<PlatformFileIdentityDescriptorV1>,
) -> MigrationPreflightPermitV1 {
  let mut file = File::open(source_path).unwrap();
  let (header, slot) = read_active_header(&mut file).unwrap();
  let source_identity = source_identity_override.unwrap_or_else(|| platform_file_identity(source_path).unwrap());
  let file_size = fs::metadata(source_path).unwrap().len();
  let registry = embedded_system_family_registry(header.hash_algo).unwrap();
  let baseline = CapabilitySetV1::v4_baseline();
  let checksum = digest(0x70);
  let capacity = |role| MigrationCapacityObservationV1 {
    role,
    volume_identity: destination_parent_identity.volume_identity,
    path_identity: destination_parent_identity,
    filesystem_capacity_bytes: 64 * GIB,
    available_bytes: 48 * GIB,
    required_bytes: if role == CapacityRoleV1::Capture { GIB } else { file_size.max(1) },
    minimum_remaining_bytes: GIB,
  };
  let request = MigrationPreflightRequestV1 {
    identity: MigrationIdentityEvidenceV1 {
      database_id: id(0x10),
      migration_id: id(0x20),
      source_physical_instance_id: id(0x30),
      destination_physical_instance_id: id(0x40),
      source_path_digest: digest(0x10),
      destination_path_digest,
      source_file_identity: source_identity,
      destination_parent_identity,
    },
    source: MigrationSourceEvidenceV1 {
      hash_algorithm: header.hash_algo,
      file_size,
      complete_file_checksum: checksum,
      selected_header_slot: slot as u8,
      selected_header_sequence: header.sequence,
      selected_header_digest: digest(0x80),
      head_hash: header.head_hash,
    },
    verification: StrictVerificationEvidenceV1 {
      state: StrictVerificationStateV1::CompleteClean,
      source_file_size: file_size,
      source_header_sequence: header.sequence,
      source_complete_file_checksum: checksum,
      issue_count: 0,
      evidence_digest: digest(0xa0),
    },
    recovery: MigrationRecoveryEvidenceV1 {
      inspection_complete: true,
      source_header_sequence: header.sequence,
      durability_latched: false,
      repair_active: false,
      external_spill_count: 0,
      repair_ticket_count: 0,
      path_latch_count: 0,
      evidence_digest: digest(0xb0),
    },
    inventory: SourceAuthorityInventoryV1 {
      complete: true,
      source_header_sequence: header.sequence,
      unresolved_family_count: 0,
      counts: AuthorityInventoryCountsV1 { protected_families: u64::from(registry.family_count), roots: 1, ..Default::default() },
      authority_digest: digest(0xc0),
      system_family_registry_fingerprint: registry.operational_fingerprint.clone(),
    },
    capacity: [
      capacity(CapacityRoleV1::Destination),
      capacity(CapacityRoleV1::Workspace),
      capacity(CapacityRoleV1::Backup),
      capacity(CapacityRoleV1::Capture),
    ],
    native: MigrationNativeEvidenceV1 { source: native(0xd0), destination: native(0xe0) },
    memory: MigrationMemoryEvidenceV1 {
      source_budget_bytes: GIB,
      destination_budget_bytes: GIB,
      coordinator_accounted_bytes: GIB,
      coordinator_ordinary_limit_bytes: 8 * GIB,
      host_available_bytes: 8 * GIB,
      host_available_floor_bytes: GIB,
      pressure: MemoryPressure::Normal,
      evidence_digest: digest(0xf0),
    },
    configuration: MigrationConfigurationEvidenceV1 {
      generation: 1,
      capture_max_bytes: GIB,
      capture_free_reserve_bytes: GIB,
      checkpoint_after_seconds: 30,
      effective_configuration_fingerprint: vec![0x17; header.hash_algo.hash_length()],
    },
    binary: MigrationBinaryEvidenceV1 {
      source_commit: [0x21; 20],
      executable_sha256: digest(0x31),
      contract_registry_sha256: hex::decode(CONTRACT_REGISTRY_SHA256).unwrap().try_into().unwrap(),
      capability_profile: BinaryCapabilityProfileV1::new(baseline, baseline),
      required_reader_capabilities: baseline,
      required_writer_capabilities: baseline,
      system_family_registry_fingerprint: registry.operational_fingerprint.clone(),
    },
  };
  admit_migration_preflight_v1(&request).unwrap().1
}

fn request<'a>(
  fixture: &'a Fixture,
  cancellation: &'a CancellationToken,
  acquisition_timeout: Duration,
) -> MigrationSourceWriteFreezeRequestV1<'a> {
  MigrationSourceWriteFreezeRequestV1 { permit: &fixture.permit, source: &fixture.engine, cancellation, acquisition_timeout }
}

#[derive(Clone)]
struct DiffSource {
  algorithm: HashAlgorithm,
  identity: PlatformFileIdentityDescriptorV1,
  entries: HashMap<Vec<u8>, EntryData>,
  reads: Arc<std::sync::atomic::AtomicU64>,
}

impl DiffSource {
  fn new(algorithm: HashAlgorithm) -> Self {
    Self {
      algorithm,
      identity: PlatformFileIdentityDescriptorV1 {
        platform: 1,
        schema: 1,
        flags: 0,
        volume_identity: id(0xa0),
        file_identity: id(0xb0),
        birth_identity: [0; 16],
      },
      entries: HashMap::new(),
      reads: Arc::new(std::sync::atomic::AtomicU64::new(0)),
    }
  }

  fn insert(&mut self, key: Vec<u8>, value: Vec<u8>) {
    let header = EntryHeader {
      entry_version: 0,
      entry_type: EntryType::DirectoryIndex,
      flags: 0,
      hash_algo: self.algorithm,
      compression_algo: CompressionAlgorithm::None,
      encryption_algo: 0,
      key_length: key.len() as u32,
      value_length: value.len() as u32,
      timestamp: 1_700_000_000_000,
      total_length: EntryHeader::compute_total_length(self.algorithm, key.len(), value.len()).unwrap(),
      hash: digest_parts(self.algorithm, &[b"diff source entry", &key, &value]),
    };
    self.entries.insert(key.clone(), (header, key, value));
  }
}

impl MigrationBaseCloneEntrySourceV1 for DiffSource {
  fn hash_algorithm(&self) -> HashAlgorithm {
    self.algorithm
  }

  fn physical_identity(&self) -> EngineResult<PlatformFileIdentityDescriptorV1> {
    Ok(self.identity)
  }

  fn historical_entry_header(&self, hash: &[u8]) -> EngineResult<Option<EntryHeader>> {
    self.reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(self.entries.get(hash).map(|entry| entry.0.clone()))
  }

  fn historical_entry_verified_bounded(&self, hash: &[u8], maximum_value_length: u32) -> EngineResult<Option<EntryData>> {
    self.reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let Some((header, key, value)) = self.entries.get(hash) else {
      return Ok(None);
    };
    if header.value_length > maximum_value_length {
      return Err(EngineError::ResourceExhausted("bounded diff source read refused".to_string()));
    }
    Ok(Some((header.clone(), key.clone(), value.clone())))
  }
}

fn diff_memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(64 << 20, 128 << 20, 1, 8 << 20).unwrap())
}

fn diff_child(name: &str, entry_type: EntryType, hash: Vec<u8>, updated_at: i64) -> ChildEntry {
  ChildEntry {
    entry_type: entry_type.to_u8(),
    hash,
    total_size: 17,
    created_at: 1_700_000_000_000,
    updated_at,
    name: name.to_string(),
    content_type: (entry_type == EntryType::FileRecord).then(|| "text/plain".to_string()),
    virtual_time: 7,
    node_id: 11,
  }
}

fn diff_directory(source: &mut DiffSource, entries: &[ChildEntry]) -> Vec<u8> {
  let value = serialize_child_entries(entries, source.algorithm.hash_length()).unwrap();
  let key = digest_parts(source.algorithm, &[b"dirc:", &value]);
  source.insert(key.clone(), value);
  key
}

fn diff_btree(source: &mut DiffSource, entries: Vec<ChildEntry>) -> Vec<u8> {
  let plan = btree_plan_from_entries(entries, source.algorithm.hash_length(), &source.algorithm).unwrap();
  let root = plan.root_hash().to_vec();
  for write in plan.node_writes() {
    source.insert(write.key.clone(), write.value.clone());
  }
  root
}

fn diff_btree_node(source: &mut DiffSource, node: BTreeNode) -> Vec<u8> {
  let value = node.serialize(source.algorithm.hash_length()).unwrap();
  let key = digest_parts(source.algorithm, &[b"btree:", &value]);
  source.insert(key.clone(), value);
  key
}

#[derive(Default)]
struct DiffSink {
  changes: Vec<(String, MigrationMerkleChangeKindV1)>,
  fail_after: Option<usize>,
}

impl MigrationMerkleDiffSinkV1 for DiffSink {
  fn record_change(&mut self, change: &MigrationMerkleChangeV1) -> EngineResult<()> {
    if self.fail_after == Some(self.changes.len()) {
      return Err(EngineError::ResourceExhausted("injected diff sink failure".to_string()));
    }
    self.changes.push((change.path.clone(), change.kind));
    Ok(())
  }
}

fn diff_request<'a>(
  source: &'a DiffSource,
  basis_root: &'a [u8],
  target_root: &'a [u8],
  memory: &'a MemoryCoordinator,
  cancellation: &'a CancellationToken,
) -> MigrationMerkleDiffRequestV1<'a> {
  MigrationMerkleDiffRequestV1 {
    source,
    basis_root,
    target_root,
    memory,
    cancellation,
    maximum_memory_bytes: 16 << 20,
    maximum_work_items: 100_000,
    maximum_directory_depth: 128,
  }
}

struct ProjectionFixture {
  _directory: TempDir,
  source_path: std::path::PathBuf,
  source: Arc<StorageEngine>,
  permit: MigrationPreflightPermitV1,
  destination: InitializedMigrationDestinationV1,
  authority: MigrationCaptureReplayAuthorityTemplateV1,
  memory: MemoryCoordinator,
  basis_file: Vec<u8>,
  basis_root: Vec<u8>,
  current_destination_tree_root: Vec<u8>,
  target_root: Vec<u8>,
}

#[derive(Clone, Copy)]
enum ProjectionTarget {
  ContentReplacement,
  MetadataOnly,
  Unchanged,
  RootAdded,
  RootRemoved,
}

impl ProjectionFixture {
  fn new() -> Self {
    Self::with_target(ProjectionTarget::ContentReplacement)
  }

  fn with_target(target: ProjectionTarget) -> Self {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("race-source.aeordb");
    let destination_path = directory.path().join("race-destination.aeordb");
    let source = Arc::new(StorageEngine::create(source_path.to_str().unwrap()).unwrap());
    let algorithm = source.hash_algo();
    let (old_chunk, old_file, old_file_value) = projection_file(algorithm, b"old", 1_700_000_000_001);
    let basis_is_empty = matches!(target, ProjectionTarget::RootAdded);
    let (stored_basis_root, basis_value) = projection_root(algorithm, old_file.clone(), 1_700_000_000_001);
    let basis_root = if basis_is_empty { vec![0; algorithm.hash_length()] } else { stored_basis_root.clone() };
    let (target_root, target_value, replacement) = match target {
      ProjectionTarget::ContentReplacement | ProjectionTarget::RootAdded => {
        let (new_chunk, new_file, new_file_value) = projection_file(algorithm, b"new", 1_700_000_000_002);
        let (root, value) = projection_root(algorithm, new_file.clone(), 1_700_000_000_002);
        (root, value, Some((new_chunk, new_file, new_file_value)))
      }
      ProjectionTarget::MetadataOnly => {
        let (root, value) = projection_root_with_virtual_time(algorithm, old_file.clone(), 1_700_000_000_001, 8);
        (root, value, None)
      }
      ProjectionTarget::Unchanged => (basis_root.clone(), basis_value.clone(), None),
      ProjectionTarget::RootRemoved => (vec![0; algorithm.hash_length()], Vec::new(), None),
    };
    if !basis_is_empty {
      for (entry_type, version, key, value) in [
        (EntryType::Chunk, 0, old_chunk.clone(), b"old".to_vec()),
        (EntryType::FileRecord, 1, old_file.clone(), old_file_value.clone()),
        (EntryType::DirectoryIndex, 0, basis_root.clone(), basis_value.clone()),
      ] {
        source.store_entry_with_version(entry_type, &key, &value, version).unwrap();
      }
      source.update_head(&basis_root).unwrap();
    }

    let destination_observation = observe_migration_destination_path_v1(&destination_path).unwrap();
    let permit =
      permit_for_destination(&source_path, destination_observation.parent_identity(), destination_observation.path_digest(), None);
    let destination = initialize_migration_destination_v1(MigrationDestinationInitializationRequestV1 {
      permit: &permit,
      destination: &destination_observation,
      created_at_ms: 1_700_000_000_100,
      writer_fence_epoch: 7,
      cancellation: &CancellationToken::new(),
    })
    .unwrap();
    let (current_destination_tree_root, current_destination_tree_value) = if basis_is_empty {
      let initial_namespace = decode_namespace_root(&destination.first_authority().namespace_root.value, algorithm).unwrap();
      let initial_tree =
        destination.publisher().load_immutable_entity_bounded(&initial_namespace.namespace_tree_root, 1 << 20).unwrap().unwrap();
      (initial_namespace.namespace_tree_root, initial_tree.stored_value)
    } else {
      publish_projection_entities(
        destination.publisher(),
        permit.database_id(),
        &[
          (0, EntryTypeV4::Chunk, old_chunk, b"old".to_vec()),
          (1, EntryTypeV4::FileRecord, old_file.clone(), old_file_value),
          (0, EntryTypeV4::DirectoryIndex, basis_root.clone(), basis_value.clone()),
        ],
      );
      (basis_root.clone(), basis_value.clone())
    };
    let required_capabilities = permit.required_reader_capabilities().into_bytes();
    let authority = MigrationCaptureReplayAuthorityTemplateV1 {
      base_predecessor_head: destination.first_authority().namespace_root.root_hash.clone(),
      semantic_state: encode_semantic_state_object(
        &SemanticStateWriteV1 {
          required_capabilities,
          availability: SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured },
        },
        algorithm,
      )
      .unwrap(),
      required_capabilities,
      typed_closure_context: b"final reconciliation race closure".to_vec(),
      authority_identity: b"HEAD".to_vec(),
      publication_timestamp_floor_ms: 1_700_000_001_000,
      monotonic_timestamp_floor_ms: 10_000,
    };
    let basis_namespace_root = encode_namespace_root(
      &NamespaceRootWriteV1 {
        required_capabilities,
        namespace_tree_root: current_destination_tree_root.clone(),
        semantic_state_root: authority.semantic_state.object_id.clone(),
      },
      algorithm,
    )
    .unwrap();
    if basis_namespace_root.root_hash != authority.base_predecessor_head {
      let publication = destination
        .publisher()
        .publish_successor_authority(&SuccessorAuthorityPublicationRequestV1 {
          database_id: permit.database_id(),
          transaction_id: id(0x91),
          created_at_ms: 1_700_000_001_100,
          expected_head_hash: authority.base_predecessor_head.clone(),
          namespace_tree: aeordb::engine::v4::first_authority::PreparedNamespaceTreeV0 {
            root_hash: current_destination_tree_root.clone(),
            stored_value: current_destination_tree_value,
          },
          semantic_state: authority.semantic_state.clone(),
          required_capabilities,
          typed_closure_digest: digest_parts(algorithm, &[b"projection basis closure"]),
          authority_identity: authority.authority_identity.clone(),
        })
        .unwrap();
      assert_eq!(publication.namespace_root.root_hash, basis_namespace_root.root_hash);
    } else {
      assert_eq!(destination.publisher().observe().unwrap().selected.header.head_hash, basis_namespace_root.root_hash);
    }

    if let Some((new_chunk, new_file, new_file_value)) = replacement {
      source.store_entry_with_version(EntryType::Chunk, &new_chunk, b"new", 0).unwrap();
      source.store_entry_with_version(EntryType::FileRecord, &new_file, &new_file_value, 1).unwrap();
    }
    if target_root != basis_root {
      if target_root.iter().any(|byte| *byte != 0) {
        source.store_entry_with_version(EntryType::DirectoryIndex, &target_root, &target_value, 0).unwrap();
      }
      source.update_head(&target_root).unwrap();
    }

    Self {
      _directory: directory,
      source_path,
      source,
      permit,
      destination,
      authority,
      memory: diff_memory(),
      basis_file: old_file,
      basis_root,
      current_destination_tree_root,
      target_root,
    }
  }
}

fn final_reconciliation_request<'request, 'source>(
  fixture: &'request ProjectionFixture,
  freeze: &'request MigrationSourceWriteFreezeV1<'source>,
  cancellation: &'request CancellationToken,
  last_reconciled_source_root: &'request [u8],
  current_destination_tree_root: &'request [u8],
) -> MigrationFinalNamespaceReconciliationRequestV1<'request, 'source> {
  MigrationFinalNamespaceReconciliationRequestV1 {
    permit: &fixture.permit,
    freeze,
    destination: fixture.destination.publisher(),
    last_reconciled_source_root,
    current_destination_tree_root,
    authority: &fixture.authority,
    memory: &fixture.memory,
    cancellation,
    publication_timestamp_ms: 1_700_000_002_000,
    maximum_diff_memory_bytes: 16 << 20,
    maximum_diff_work_items: 100_000,
    maximum_subtree_memory_bytes: 16 << 20,
    maximum_subtree_work_items: 100_000,
    maximum_total_subtree_work_items: 100_000,
    maximum_decoded_chunk_bytes: 1 << 20,
    maximum_directory_depth: 128,
  }
}

fn projection_file(algorithm: HashAlgorithm, bytes: &[u8], updated_at: i64) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
  let chunk = digest_parts(algorithm, &[b"chunk:", bytes]);
  let mut record = FileRecord::new("/a.txt".to_string(), Some("text/plain".to_string()), bytes.len() as u64, vec![chunk.clone()]);
  record.created_at = 1_700_000_000_000;
  record.updated_at = updated_at;
  record.content_hash = digest_parts(algorithm, &[bytes]);
  let value = record.serialize(algorithm.hash_length()).unwrap();
  let key = digest_parts(algorithm, &[b"filec:", &value]);
  (chunk, key, value)
}

fn projection_root(algorithm: HashAlgorithm, file_hash: Vec<u8>, updated_at: i64) -> (Vec<u8>, Vec<u8>) {
  projection_root_with_virtual_time(algorithm, file_hash, updated_at, 7)
}

fn projection_root_with_virtual_time(
  algorithm: HashAlgorithm,
  file_hash: Vec<u8>,
  updated_at: i64,
  virtual_time: u64,
) -> (Vec<u8>, Vec<u8>) {
  let mut child = diff_child("a.txt", EntryType::FileRecord, file_hash, updated_at);
  child.virtual_time = virtual_time;
  let value = serialize_child_entries(&[child], algorithm.hash_length()).unwrap();
  let key = digest_parts(algorithm, &[b"dirc:", &value]);
  (key, value)
}

fn publish_projection_entities(
  destination: &V4FirstAuthorityPublisher,
  database_id: [u8; 16],
  entities: &[(u8, EntryTypeV4, Vec<u8>, Vec<u8>)],
) {
  let writes = entities
    .iter()
    .map(|(version, entry_type, key, value)| ImmutableEntityWriteV1 {
      entity_version: *version,
      entry_type: *entry_type,
      flags: 0,
      key,
      stored_value: value,
    })
    .collect::<Vec<_>>();
  destination
    .publish_immutable_entity_batch(ImmutableEntityBatchPublicationRequestV1 {
      database_id: &database_id,
      entities: &writes,
      publication_timestamp_ms: 1_700_000_000_900,
    })
    .unwrap();
}

#[test]
fn final_freeze_captures_exact_live_authority_without_mutating_source() {
  let fixture = Fixture::new();
  let before = fs::read(&fixture.path).unwrap();
  let physical_identity = platform_file_identity(&fixture.path).unwrap();
  let mut file = File::open(&fixture.path).unwrap();
  let (header, _) = read_active_header(&mut file).unwrap();
  let frontier = fixture.engine.durability_snapshot().unwrap().hard_frontier;
  let cancellation = CancellationToken::new();

  let freeze = acquire_migration_source_write_freeze_v1(request(&fixture, &cancellation, Duration::from_secs(2))).unwrap();
  assert_eq!(freeze.authority().physical_identity, physical_identity);
  assert_eq!(freeze.authority().header_sequence, header.sequence);
  assert_eq!(freeze.authority().namespace_root, header.head_hash);
  assert_eq!(freeze.authority().hard_publication_frontier, frontier);
  assert_eq!(freeze.authority().hash_algorithm, ALGORITHM);
  assert_eq!(
    freeze.authority().system_family_registry_fingerprint,
    embedded_system_family_registry(ALGORITHM).unwrap().operational_fingerprint
  );
  freeze.validate_unchanged().unwrap();
  assert_eq!(fs::read(&fixture.path).unwrap(), before);
}

#[test]
fn final_freeze_allows_owner_reads_but_rejects_owner_writes() {
  let fixture = Fixture::new();
  let before = fs::read(&fixture.path).unwrap();
  let cancellation = CancellationToken::new();
  let freeze = acquire_migration_source_write_freeze_v1(request(&fixture, &cancellation, Duration::from_secs(2))).unwrap();

  assert_eq!(fixture.engine.head_hash().unwrap(), freeze.authority().namespace_root);
  let key = fixture.engine.compute_hash(b"owner write must be refused").unwrap();
  let error = fixture.engine.store_entry(EntryType::Chunk, &key, b"forbidden").unwrap_err();
  assert!(error.to_string().contains("writes are prohibited"), "{error}");
  let error = fixture.engine.flush_index_buffer().unwrap_err();
  assert!(error.to_string().contains("writes are prohibited"), "{error}");
  let error = fixture.engine.recover_after_emergency_spill_replay().unwrap_err();
  assert!(error.to_string().contains("writes are prohibited"), "{error}");
  freeze.validate_unchanged().unwrap();
  assert_eq!(fs::read(&fixture.path).unwrap(), before);
}

#[test]
fn final_freeze_blocks_raw_and_namespace_writers_until_release() {
  let fixture = Fixture::new();
  let cancellation = CancellationToken::new();
  let freeze = acquire_migration_source_write_freeze_v1(request(&fixture, &cancellation, Duration::from_secs(2))).unwrap();
  let (started_tx, started_rx) = mpsc::channel();
  let (result_tx, result_rx) = mpsc::channel();

  let raw_engine = Arc::clone(&fixture.engine);
  let raw_started = started_tx.clone();
  let raw_result = result_tx.clone();
  let raw = std::thread::spawn(move || {
    let key = raw_engine.compute_hash(b"blocked raw writer").unwrap();
    raw_started.send(()).unwrap();
    raw_result.send(raw_engine.store_entry(EntryType::Chunk, &key, b"raw")).unwrap();
  });

  let namespace_engine = Arc::clone(&fixture.engine);
  let namespace = std::thread::spawn(move || {
    let locator = namespace_engine.compute_hash(b"blocked namespace locator").unwrap();
    let mut record = FileRecord::new("/blocked-system-entry".to_string(), Some("application/json".to_string()), 0, Vec::new());
    record.content_hash = namespace_engine.compute_hash(b"").unwrap();
    let value = record.serialize(namespace_engine.hash_algo().hash_length()).unwrap();
    let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::SystemWrite);
    batch
      .replace_locator_with_version(
        EntryType::FileRecord,
        locator.clone(),
        value,
        0,
        aeordb::engine::file_record::CURRENT_FILE_RECORD_VERSION,
      )
      .unwrap();
    batch
      .add_source_identity(NamespaceMutationSourceIdentity {
        path: "/blocked-system-entry".to_string(),
        entry_type: Some(EntryType::FileRecord.to_u8()),
        previous_identity: None,
        new_identity: Some(locator),
      })
      .unwrap();
    started_tx.send(()).unwrap();
    result_tx.send(NamespaceMutationCoordinator::new(&namespace_engine).execute(batch).map(|_| 0)).unwrap();
  });

  started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
  started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
  assert!(result_rx.recv_timeout(Duration::from_millis(100)).is_err());
  drop(freeze);
  assert!(result_rx.recv_timeout(Duration::from_secs(2)).unwrap().is_ok());
  assert!(result_rx.recv_timeout(Duration::from_secs(2)).unwrap().is_ok());
  raw.join().unwrap();
  namespace.join().unwrap();
}

#[test]
fn invalid_requests_fail_before_source_bytes_change() {
  let fixture = Fixture::new();
  let before = fs::read(&fixture.path).unwrap();

  let canceled = CancellationToken::new();
  canceled.cancel();
  let error = acquire_migration_source_write_freeze_v1(request(&fixture, &canceled, Duration::from_secs(1))).err().unwrap();
  assert_eq!(error.code(), "migration_final_freeze_canceled");

  let active = CancellationToken::new();
  let error = acquire_migration_source_write_freeze_v1(request(&fixture, &active, Duration::ZERO)).err().unwrap();
  assert_eq!(error.code(), "migration_final_freeze_timeout");
  let error = acquire_migration_source_write_freeze_v1(request(&fixture, &active, Duration::from_secs(24 * 60 * 60 + 1))).err().unwrap();
  assert_eq!(error.code(), "migration_final_freeze_timeout");

  let mut wrong_identity = platform_file_identity(&fixture.path).unwrap();
  wrong_identity.file_identity[0] ^= 0x80;
  if wrong_identity.flags & (1 << 1) != 0 {
    wrong_identity.birth_identity[0] ^= 0x80;
  }
  let wrong_permit = permit_for(&fixture.path, fixture._directory.path(), Some(wrong_identity));
  let error = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
    permit: &wrong_permit,
    source: &fixture.engine,
    cancellation: &active,
    acquisition_timeout: Duration::from_secs(1),
  })
  .err()
  .unwrap();
  assert_eq!(error.code(), "migration_final_freeze_source_identity");
  assert_eq!(fs::read(&fixture.path).unwrap(), before);
}

#[test]
fn blocked_freeze_acquisition_honors_cancellation_and_timeout() {
  let fixture = Fixture::new();
  let first_cancellation = CancellationToken::new();
  let first = acquire_migration_source_write_freeze_v1(request(&fixture, &first_cancellation, Duration::from_secs(2))).unwrap();

  let engine = Arc::clone(&fixture.engine);
  let permit = fixture.permit.clone();
  let cancellation = CancellationToken::new();
  let thread_cancellation = cancellation.clone();
  let canceled = std::thread::spawn(move || {
    acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
      permit: &permit,
      source: &engine,
      cancellation: &thread_cancellation,
      acquisition_timeout: Duration::from_secs(2),
    })
    .err()
    .unwrap()
    .code()
  });
  std::thread::sleep(Duration::from_millis(100));
  cancellation.cancel();
  assert_eq!(canceled.join().unwrap(), "migration_final_freeze_canceled");

  let engine = Arc::clone(&fixture.engine);
  let permit = fixture.permit.clone();
  let timed_out = std::thread::spawn(move || {
    let cancellation = CancellationToken::new();
    acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
      permit: &permit,
      source: &engine,
      cancellation: &cancellation,
      acquisition_timeout: Duration::from_millis(100),
    })
    .err()
    .unwrap()
    .code()
  });
  assert_eq!(timed_out.join().unwrap(), "migration_final_freeze_timeout");
  first.validate_unchanged().unwrap();
}

#[test]
fn strict_merkle_diff_streams_flat_changes_and_metadata_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let mut source = DiffSource::new(algorithm);
    let same = digest_parts(algorithm, &[b"same"]);
    let old = digest_parts(algorithm, &[b"old"]);
    let new = digest_parts(algorithm, &[b"new"]);
    let removed = digest_parts(algorithm, &[b"removed"]);
    let added = digest_parts(algorithm, &[b"added"]);
    let basis_root = diff_directory(
      &mut source,
      &[
        diff_child("a.txt", EntryType::FileRecord, same.clone(), 1),
        diff_child("b.txt", EntryType::FileRecord, same.clone(), 1),
        diff_child("c.txt", EntryType::FileRecord, old, 1),
        diff_child("d.txt", EntryType::FileRecord, removed, 1),
      ],
    );
    let target_root = diff_directory(
      &mut source,
      &[
        diff_child("a.txt", EntryType::FileRecord, same.clone(), 1),
        diff_child("b.txt", EntryType::FileRecord, same, 2),
        diff_child("c.txt", EntryType::FileRecord, new, 2),
        diff_child("e.txt", EntryType::FileRecord, added, 2),
      ],
    );
    let memory = diff_memory();
    let cancellation = CancellationToken::new();
    let mut sink = DiffSink::default();

    let receipt =
      stream_strict_migration_merkle_diff_v1(diff_request(&source, &basis_root, &target_root, &memory, &cancellation), &mut sink).unwrap();

    assert_eq!(
      sink.changes,
      vec![
        ("/b.txt".to_string(), MigrationMerkleChangeKindV1::MetadataOnly),
        ("/c.txt".to_string(), MigrationMerkleChangeKindV1::Replaced),
        ("/d.txt".to_string(), MigrationMerkleChangeKindV1::Removed),
        ("/e.txt".to_string(), MigrationMerkleChangeKindV1::Added),
      ]
    );
    assert_eq!(receipt.changed_path_count, 4);
    assert_eq!(receipt.metadata_only_count, 1);
    assert_eq!(receipt.basis_root, basis_root);
    assert_eq!(receipt.target_root, target_root);
    assert!(receipt.maximum_memory_used_bytes <= 16 << 20);
  }
}

#[test]
fn strict_merkle_diff_canonicalizes_unsorted_legacy_flat_directories_but_refuses_duplicates() {
  let mut source = DiffSource::new(ALGORITHM);
  let shared = digest_parts(ALGORITHM, &[b"shared"]);
  let basis_root = diff_directory(
    &mut source,
    &[
      diff_child("z-removed.txt", EntryType::FileRecord, digest(0x51).to_vec(), 1),
      diff_child("a-shared.txt", EntryType::FileRecord, shared.clone(), 1),
    ],
  );
  let target_root = diff_directory(
    &mut source,
    &[
      diff_child("y-added.txt", EntryType::FileRecord, digest(0x52).to_vec(), 1),
      diff_child("a-shared.txt", EntryType::FileRecord, shared.clone(), 1),
    ],
  );
  let memory = diff_memory();
  let cancellation = CancellationToken::new();
  let mut sink = DiffSink::default();

  let receipt =
    stream_strict_migration_merkle_diff_v1(diff_request(&source, &basis_root, &target_root, &memory, &cancellation), &mut sink).unwrap();

  assert_eq!(
    sink.changes,
    vec![
      ("/y-added.txt".to_string(), MigrationMerkleChangeKindV1::Added),
      ("/z-removed.txt".to_string(), MigrationMerkleChangeKindV1::Removed),
    ]
  );
  assert_eq!(receipt.changed_path_count, 2);

  let duplicate_root = diff_directory(
    &mut source,
    &[
      diff_child("duplicate.txt", EntryType::FileRecord, digest(0x53).to_vec(), 1),
      diff_child("duplicate.txt", EntryType::FileRecord, digest(0x54).to_vec(), 2),
    ],
  );
  let error = stream_strict_migration_merkle_diff_v1(
    diff_request(&source, &basis_root, &duplicate_root, &memory, &cancellation),
    &mut DiffSink::default(),
  )
  .unwrap_err();
  assert_eq!(error.code(), "migration_final_diff_flat_duplicate");
}

#[test]
fn strict_merkle_diff_descends_changed_directories_and_btree_pages() {
  let algorithm = HashAlgorithm::Blake3_256;
  let mut source = DiffSource::new(algorithm);
  let old = digest_parts(algorithm, &[b"old nested file"]);
  let new = digest_parts(algorithm, &[b"new nested file"]);
  let mut basis_entries =
    (0..=256).map(|index| diff_child(&format!("file-{index:04}.txt"), EntryType::FileRecord, old.clone(), 1)).collect::<Vec<_>>();
  let mut target_entries = basis_entries.clone();
  target_entries[128] = diff_child("file-0128.txt", EntryType::FileRecord, new, 2);
  let basis_nested = diff_btree(&mut source, std::mem::take(&mut basis_entries));
  let target_nested = diff_btree(&mut source, target_entries);
  let basis_root = diff_directory(&mut source, &[diff_child("nested", EntryType::DirectoryIndex, basis_nested, 1)]);
  let target_root = diff_directory(&mut source, &[diff_child("nested", EntryType::DirectoryIndex, target_nested, 2)]);
  let memory = diff_memory();
  let cancellation = CancellationToken::new();
  let mut sink = DiffSink::default();

  let receipt =
    stream_strict_migration_merkle_diff_v1(diff_request(&source, &basis_root, &target_root, &memory, &cancellation), &mut sink).unwrap();

  assert_eq!(
    sink.changes,
    vec![
      ("/nested".to_string(), MigrationMerkleChangeKindV1::MetadataOnly),
      ("/nested/file-0128.txt".to_string(), MigrationMerkleChangeKindV1::Replaced),
    ]
  );
  assert_eq!(receipt.changed_path_count, 2);
  assert_eq!(receipt.metadata_only_count, 1);
  assert!(receipt.visited_directory_count >= 2);
  assert!(receipt.visited_btree_node_count >= 2);
}

#[test]
fn strict_merkle_diff_equal_roots_perform_no_source_reads() {
  let mut source = DiffSource::new(ALGORITHM);
  let root = diff_directory(&mut source, &[]);
  source.reads.store(0, std::sync::atomic::Ordering::Relaxed);
  let memory = diff_memory();
  let cancellation = CancellationToken::new();
  let mut sink = DiffSink::default();

  let receipt = stream_strict_migration_merkle_diff_v1(diff_request(&source, &root, &root, &memory, &cancellation), &mut sink).unwrap();

  assert!(sink.changes.is_empty());
  assert_eq!(receipt.changed_path_count, 0);
  assert_eq!(receipt.visited_entity_count, 0);
  assert_eq!(source.reads.load(std::sync::atomic::Ordering::Relaxed), 0);
}

#[test]
fn strict_merkle_diff_streams_zero_root_transitions_without_source_reads() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let mut source = DiffSource::new(algorithm);
    let root = diff_directory(&mut source, &[]);
    let zero = vec![0; algorithm.hash_length()];
    source.reads.store(0, std::sync::atomic::Ordering::Relaxed);
    let memory = diff_memory();
    let cancellation = CancellationToken::new();

    let mut added = DiffSink::default();
    let added_receipt =
      stream_strict_migration_merkle_diff_v1(diff_request(&source, &zero, &root, &memory, &cancellation), &mut added).unwrap();
    assert_eq!(added.changes, vec![("/".to_string(), MigrationMerkleChangeKindV1::Added)]);
    assert_eq!(added_receipt.changed_path_count, 1);

    let mut removed = DiffSink::default();
    let removed_receipt =
      stream_strict_migration_merkle_diff_v1(diff_request(&source, &root, &zero, &memory, &cancellation), &mut removed).unwrap();
    assert_eq!(removed.changes, vec![("/".to_string(), MigrationMerkleChangeKindV1::Removed)]);
    assert_eq!(removed_receipt.changed_path_count, 1);
    assert_eq!(source.reads.load(std::sync::atomic::Ordering::Relaxed), 0);
  }
}

#[test]
fn strict_merkle_diff_fails_closed_on_corruption_cancellation_bounds_and_sink_failure() {
  let mut source = DiffSource::new(ALGORITHM);
  let basis_root = diff_directory(&mut source, &[]);
  let entries = (0..=256)
    .map(|index| diff_child(&format!("file-{index:04}.txt"), EntryType::FileRecord, digest(index as u8).to_vec(), 1))
    .collect::<Vec<_>>();
  let valid_plan = btree_plan_from_entries(entries, ALGORITHM.hash_length(), &ALGORITHM).unwrap();
  let mut malformed = valid_plan.root_data().to_vec();
  malformed.pop();
  let malformed_root = digest_parts(ALGORITHM, &[b"btree:", &malformed]);
  source.insert(malformed_root.clone(), malformed);
  let memory = diff_memory();
  let active = CancellationToken::new();
  let mut sink = DiffSink::default();

  let error =
    stream_strict_migration_merkle_diff_v1(diff_request(&source, &basis_root, &malformed_root, &memory, &active), &mut sink).unwrap_err();
  assert!(error.code().starts_with("migration_final_diff_"), "{}: {error}", error.code());

  let target_root = diff_directory(&mut source, &[diff_child("a.txt", EntryType::FileRecord, digest(0x41).to_vec(), 1)]);
  let canceled = CancellationToken::new();
  canceled.cancel();
  let error =
    stream_strict_migration_merkle_diff_v1(diff_request(&source, &basis_root, &target_root, &memory, &canceled), &mut DiffSink::default())
      .unwrap_err();
  assert_eq!(error.code(), "migration_final_diff_cancelled");

  let mut bounded = diff_request(&source, &basis_root, &target_root, &memory, &active);
  bounded.maximum_work_items = 1;
  let error = stream_strict_migration_merkle_diff_v1(bounded, &mut DiffSink::default()).unwrap_err();
  assert_eq!(error.code(), "migration_final_diff_work_limit");

  let mut failing = DiffSink { changes: Vec::new(), fail_after: Some(0) };
  let error =
    stream_strict_migration_merkle_diff_v1(diff_request(&source, &basis_root, &target_root, &memory, &active), &mut failing).unwrap_err();
  assert_eq!(error.code(), "migration_final_diff_sink");
}

#[test]
fn strict_merkle_diff_refuses_noncanonical_identity_btree_ranges_and_resource_bounds() {
  let mut source = DiffSource::new(ALGORITHM);
  let basis_root = diff_directory(&mut source, &[]);
  let invalid_identity = digest_parts(ALGORITHM, &[b"not the canonical directory identity"]);
  source.insert(invalid_identity.clone(), Vec::new());
  let memory = diff_memory();
  let cancellation = CancellationToken::new();
  let error = stream_strict_migration_merkle_diff_v1(
    diff_request(&source, &basis_root, &invalid_identity, &memory, &cancellation),
    &mut DiffSink::default(),
  )
  .unwrap_err();
  assert_eq!(error.code(), "migration_final_diff_directory_identity");

  let left = diff_btree_node(
    &mut source,
    BTreeNode::Leaf(LeafNode { entries: vec![diff_child("z-left", EntryType::FileRecord, digest(0x51).to_vec(), 1)] }),
  );
  let right = diff_btree_node(
    &mut source,
    BTreeNode::Leaf(LeafNode { entries: vec![diff_child("z-right", EntryType::FileRecord, digest(0x52).to_vec(), 1)] }),
  );
  let invalid_range =
    diff_btree_node(&mut source, BTreeNode::Internal(InternalNode { keys: vec!["m".to_string()], children: vec![left, right] }));
  let error = stream_strict_migration_merkle_diff_v1(
    diff_request(&source, &basis_root, &invalid_range, &memory, &cancellation),
    &mut DiffSink::default(),
  )
  .unwrap_err();
  assert_eq!(error.code(), "migration_final_diff_btree_range");

  let nested_basis = diff_directory(&mut source, &[]);
  let nested_target = diff_directory(&mut source, &[diff_child("a.txt", EntryType::FileRecord, digest(0x53).to_vec(), 1)]);
  let depth_basis_root = diff_directory(&mut source, &[diff_child("nested", EntryType::DirectoryIndex, nested_basis, 1)]);
  let target_root = diff_directory(&mut source, &[diff_child("nested", EntryType::DirectoryIndex, nested_target, 1)]);
  let mut shallow = diff_request(&source, &depth_basis_root, &target_root, &memory, &cancellation);
  shallow.maximum_directory_depth = 1;
  let error = stream_strict_migration_merkle_diff_v1(shallow, &mut DiffSink::default()).unwrap_err();
  assert_eq!(error.code(), "migration_final_diff_directory_depth");

  let mut tiny_memory = diff_request(&source, &depth_basis_root, &target_root, &memory, &cancellation);
  tiny_memory.maximum_memory_bytes = 1;
  let error = stream_strict_migration_merkle_diff_v1(tiny_memory, &mut DiffSink::default()).unwrap_err();
  assert_eq!(error.code(), "migration_final_diff_memory_limit");

  let short_root = &basis_root[..basis_root.len() - 1];
  let error = stream_strict_migration_merkle_diff_v1(
    diff_request(&source, short_root, &target_root, &memory, &cancellation),
    &mut DiffSink::default(),
  )
  .unwrap_err();
  assert_eq!(error.code(), "migration_final_diff_root_width");

  let oversized_flat_entries = (0..=BTREE_CONVERSION_THRESHOLD)
    .map(|index| diff_child(&format!("flat-{index:04}.txt"), EntryType::FileRecord, digest(index as u8).to_vec(), 1))
    .collect::<Vec<_>>();
  let oversized_flat_root = diff_directory(&mut source, &oversized_flat_entries);
  let error = stream_strict_migration_merkle_diff_v1(
    diff_request(&source, &basis_root, &oversized_flat_root, &memory, &cancellation),
    &mut DiffSink::default(),
  )
  .unwrap_err();
  assert_eq!(error.code(), "migration_final_diff_flat_count");
}

#[test]
fn strict_merkle_diff_accounts_retained_flat_ordering_state() {
  let mut source = DiffSource::new(ALGORITHM);
  let basis_root = diff_directory(&mut source, &[]);
  let name = format!("{}.txt", "n".repeat(16 * 1024));
  let target_value =
    serialize_child_entries(&[diff_child(&name, EntryType::FileRecord, digest(0x61).to_vec(), 1)], ALGORITHM.hash_length()).unwrap();
  let target_root = digest_parts(ALGORITHM, &[b"dirc:", &target_value]);
  source.insert(target_root.clone(), target_value.clone());
  let memory = diff_memory();
  let cancellation = CancellationToken::new();

  let receipt = stream_strict_migration_merkle_diff_v1(
    diff_request(&source, &basis_root, &target_root, &memory, &cancellation),
    &mut DiffSink::default(),
  )
  .unwrap();

  let minimum_retained_bytes = u64::try_from(target_value.len() + (name.len() * 2)).unwrap();
  assert!(receipt.maximum_memory_used_bytes >= minimum_retained_bytes);
}

#[test]
fn final_reconciliation_projects_a_post_commit_capture_gap_under_the_live_freeze() {
  let fixture = ProjectionFixture::new();
  let source_before = fs::read(&fixture.source_path).unwrap();
  let cancellation = CancellationToken::new();
  let freeze = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
    permit: &fixture.permit,
    source: &fixture.source,
    cancellation: &cancellation,
    acquisition_timeout: Duration::from_secs(2),
  })
  .unwrap();
  assert_eq!(freeze.authority().namespace_root, fixture.target_root);

  let execute = || {
    execute_final_namespace_reconciliation_v1(final_reconciliation_request(
      &fixture,
      &freeze,
      &cancellation,
      &fixture.basis_root,
      &fixture.current_destination_tree_root,
    ))
  };
  let receipt = execute().unwrap();
  assert_eq!(receipt.frozen_source_root, fixture.target_root);
  assert_ne!(receipt.destination_tree_root, fixture.target_root);
  assert_eq!(receipt.diff.changed_path_count, 1);
  assert_eq!(receipt.translated_subtree_count, 1);
  assert!(receipt.translated_subtree_work_items > 0);
  assert_eq!(receipt.reused_destination_entity_count, 0);
  assert_eq!(receipt.destination_successor_count, 1);
  assert!(!receipt.idempotent);
  assert_eq!(fixture.destination.publisher().observe().unwrap().selected.header.head_hash, receipt.destination_namespace_root);
  assert!(fixture.destination.publisher().load_immutable_entity_bounded(&receipt.destination_tree_root, 1 << 20).unwrap().is_some());

  let retry = execute().unwrap();
  assert_eq!(retry.destination_namespace_root, receipt.destination_namespace_root);
  assert_eq!(retry.destination_tree_root, receipt.destination_tree_root);
  assert_eq!(retry.destination_successor_count, 0);
  assert!(retry.idempotent);
  freeze.validate_unchanged().unwrap();
  assert_eq!(fs::read(&fixture.source_path).unwrap(), source_before);
}

#[test]
fn final_reconciliation_reuses_destination_identity_for_metadata_only_changes() {
  let fixture = ProjectionFixture::with_target(ProjectionTarget::MetadataOnly);
  let source_before = fs::read(&fixture.source_path).unwrap();
  let cancellation = CancellationToken::new();
  let freeze = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
    permit: &fixture.permit,
    source: &fixture.source,
    cancellation: &cancellation,
    acquisition_timeout: Duration::from_secs(2),
  })
  .unwrap();

  let receipt = execute_final_namespace_reconciliation_v1(final_reconciliation_request(
    &fixture,
    &freeze,
    &cancellation,
    &fixture.basis_root,
    &fixture.current_destination_tree_root,
  ))
  .unwrap();

  assert_eq!(receipt.diff.changed_path_count, 1);
  assert_eq!(receipt.diff.metadata_only_count, 1);
  assert_eq!(receipt.translated_subtree_count, 0);
  assert_eq!(receipt.translated_subtree_work_items, 0);
  assert_eq!(receipt.reused_destination_entity_count, 1);
  assert_eq!(receipt.destination_successor_count, 1);
  let root = fixture.destination.publisher().load_immutable_entity_bounded(&receipt.destination_tree_root, 1 << 20).unwrap().unwrap();
  let children = deserialize_child_entries(&root.stored_value, ALGORITHM.hash_length(), root.entity_version).unwrap();
  assert_eq!(children.len(), 1);
  assert_eq!(children[0].hash, fixture.basis_file);
  assert_eq!(children[0].updated_at, 1_700_000_000_001);
  assert_eq!(children[0].virtual_time, 8);
  freeze.validate_unchanged().unwrap();
  assert_eq!(fs::read(&fixture.source_path).unwrap(), source_before);
}

#[test]
fn final_reconciliation_equal_root_is_an_exact_noop() {
  let fixture = ProjectionFixture::with_target(ProjectionTarget::Unchanged);
  let source_before = fs::read(&fixture.source_path).unwrap();
  let before = fixture.destination.publisher().observe().unwrap().selected.header.clone();
  let cancellation = CancellationToken::new();
  let freeze = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
    permit: &fixture.permit,
    source: &fixture.source,
    cancellation: &cancellation,
    acquisition_timeout: Duration::from_secs(2),
  })
  .unwrap();

  let receipt = execute_final_namespace_reconciliation_v1(final_reconciliation_request(
    &fixture,
    &freeze,
    &cancellation,
    &fixture.basis_root,
    &fixture.current_destination_tree_root,
  ))
  .unwrap();

  assert_eq!(receipt.diff.changed_path_count, 0);
  assert_eq!(receipt.translated_subtree_work_items, 0);
  assert_eq!(receipt.destination_successor_count, 0);
  assert!(receipt.idempotent);
  assert_eq!(receipt.destination_tree_root, fixture.current_destination_tree_root);
  assert_eq!(fixture.destination.publisher().observe().unwrap().selected.header, before);
  freeze.validate_unchanged().unwrap();
  assert_eq!(fs::read(&fixture.source_path).unwrap(), source_before);
}

#[test]
fn final_reconciliation_projects_added_and_removed_namespace_roots() {
  for target in [ProjectionTarget::RootAdded, ProjectionTarget::RootRemoved] {
    let fixture = ProjectionFixture::with_target(target);
    let source_before = fs::read(&fixture.source_path).unwrap();
    let cancellation = CancellationToken::new();
    let freeze = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
      permit: &fixture.permit,
      source: &fixture.source,
      cancellation: &cancellation,
      acquisition_timeout: Duration::from_secs(2),
    })
    .unwrap();

    let receipt = execute_final_namespace_reconciliation_v1(final_reconciliation_request(
      &fixture,
      &freeze,
      &cancellation,
      &fixture.basis_root,
      &fixture.current_destination_tree_root,
    ))
    .unwrap();

    assert_eq!(receipt.diff.changed_path_count, 1);
    assert_eq!(receipt.translated_subtree_count, 1);
    assert_eq!(receipt.destination_successor_count, 1);
    let root = fixture.destination.publisher().load_immutable_entity_bounded(&receipt.destination_tree_root, 1 << 20).unwrap().unwrap();
    let children = deserialize_child_entries(&root.stored_value, ALGORITHM.hash_length(), root.entity_version).unwrap();
    match target {
      ProjectionTarget::RootAdded => assert_eq!(children.len(), 1),
      ProjectionTarget::RootRemoved => assert!(children.is_empty()),
      _ => unreachable!(),
    }
    freeze.validate_unchanged().unwrap();
    assert_eq!(fs::read(&fixture.source_path).unwrap(), source_before);
  }
}

#[test]
fn final_reconciliation_refuses_invalid_or_cancelled_work_without_moving_authority() {
  let fixture = ProjectionFixture::new();
  let active = CancellationToken::new();
  let freeze = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
    permit: &fixture.permit,
    source: &fixture.source,
    cancellation: &active,
    acquisition_timeout: Duration::from_secs(2),
  })
  .unwrap();
  let before = fixture.destination.publisher().observe().unwrap().selected.header.clone();

  let canceled = CancellationToken::new();
  canceled.cancel();
  let error = execute_final_namespace_reconciliation_v1(final_reconciliation_request(
    &fixture,
    &freeze,
    &canceled,
    &fixture.basis_root,
    &fixture.current_destination_tree_root,
  ))
  .unwrap_err();
  assert_eq!(error.code(), "migration_final_diff_cancelled");
  assert_eq!(fixture.destination.publisher().observe().unwrap().selected.header, before);

  let mut unbounded = final_reconciliation_request(&fixture, &freeze, &active, &fixture.basis_root, &fixture.current_destination_tree_root);
  unbounded.maximum_diff_memory_bytes = 0;
  let error = execute_final_namespace_reconciliation_v1(unbounded).unwrap_err();
  assert_eq!(error.code(), "migration_final_diff_bounds");
  assert_eq!(fixture.destination.publisher().observe().unwrap().selected.header, before);

  let mut aggregate_work =
    final_reconciliation_request(&fixture, &freeze, &active, &fixture.basis_root, &fixture.current_destination_tree_root);
  aggregate_work.maximum_total_subtree_work_items = 1;
  let error = execute_final_namespace_reconciliation_v1(aggregate_work).unwrap_err();
  assert_eq!(error.code(), "migration_clone_work_limit");
  assert_eq!(fixture.destination.publisher().observe().unwrap().selected.header, before);

  let missing_tree = digest_parts(ALGORITHM, &[b"missing destination tree"]);
  let error =
    execute_final_namespace_reconciliation_v1(final_reconciliation_request(&fixture, &freeze, &active, &fixture.basis_root, &missing_tree))
      .unwrap_err();
  assert!(error.code().starts_with("migration_"), "{}: {error}", error.code());
  assert_eq!(fixture.destination.publisher().observe().unwrap().selected.header, before);
  freeze.validate_unchanged().unwrap();
}

struct FinalInventoryStream {
  seeds: VecDeque<MigrationFinalAuthoritySeedV1>,
  closure: MigrationFinalAuthorityInventoryClosureV1,
}

impl MigrationFinalAuthorityInventorySourceV1 for FinalInventoryStream {
  fn next_seed(&mut self) -> EngineResult<Option<MigrationFinalAuthoritySeedV1>> {
    Ok(self.seeds.pop_front())
  }

  fn finish(&mut self) -> EngineResult<MigrationFinalAuthorityInventoryClosureV1> {
    Ok(self.closure.clone())
  }
}

#[derive(Default)]
struct FinalPriorMappings {
  rows: HashMap<Vec<u8>, Vec<u8>>,
  lookups: u64,
}

impl MigrationFinalPriorRootMappingLookupV1 for FinalPriorMappings {
  fn lookup_destination_entity(&mut self, seed: &MigrationFinalAuthoritySeedV1) -> EngineResult<Option<Vec<u8>>> {
    self.lookups += 1;
    Ok(self.rows.get(&seed.seed.hash).cloned())
  }
}

#[derive(Default)]
struct FinalMappingSink {
  rows: Vec<MigrationFinalRootMappingV1>,
  closure: Option<MigrationFinalRootMappingClosureV1>,
  fail_record: bool,
  fail_finish: bool,
}

impl MigrationFinalRootMappingSinkV1 for FinalMappingSink {
  fn record_root_mapping(&mut self, mapping: &MigrationFinalRootMappingV1) -> EngineResult<()> {
    if self.fail_record {
      return Err(EngineError::ResourceExhausted("injected final mapping sink failure".to_string()));
    }
    self.rows.push(mapping.clone());
    Ok(())
  }

  fn finish_root_mappings(&mut self, closure: &MigrationFinalRootMappingClosureV1) -> EngineResult<()> {
    if self.fail_finish {
      return Err(EngineError::ResourceExhausted("injected final mapping closure failure".to_string()));
    }
    if let Some(existing) = self.closure.as_ref() {
      if existing != closure {
        return Err(EngineError::InvalidInput("final mapping closure changed across retry".to_string()));
      }
    } else {
      self.closure = Some(closure.clone());
    }
    Ok(())
  }
}

struct FailingFinalInventory {
  head: Option<MigrationFinalAuthoritySeedV1>,
  closure: MigrationFinalAuthorityInventoryClosureV1,
  fail_next: bool,
  fail_finish: bool,
}

impl MigrationFinalAuthorityInventorySourceV1 for FailingFinalInventory {
  fn next_seed(&mut self) -> EngineResult<Option<MigrationFinalAuthoritySeedV1>> {
    if self.fail_next {
      return Err(EngineError::ResourceExhausted("injected final inventory read failure".to_string()));
    }
    Ok(self.head.take())
  }

  fn finish(&mut self) -> EngineResult<MigrationFinalAuthorityInventoryClosureV1> {
    if self.fail_finish {
      return Err(EngineError::ResourceExhausted("injected final inventory closure failure".to_string()));
    }
    Ok(self.closure.clone())
  }
}

struct FailingFinalPriorMappings;

impl MigrationFinalPriorRootMappingLookupV1 for FailingFinalPriorMappings {
  fn lookup_destination_entity(&mut self, _seed: &MigrationFinalAuthoritySeedV1) -> EngineResult<Option<Vec<u8>>> {
    Err(EngineError::ResourceExhausted("injected final prior-map failure".to_string()))
  }
}

struct OneShotFinalPriorMapping {
  destination: Option<Vec<u8>>,
}

impl MigrationFinalPriorRootMappingLookupV1 for OneShotFinalPriorMapping {
  fn lookup_destination_entity(&mut self, _seed: &MigrationFinalAuthoritySeedV1) -> EngineResult<Option<Vec<u8>>> {
    Ok(self.destination.take())
  }
}

struct MovingFinalInventory {
  seeds: VecDeque<MigrationFinalAuthoritySeedV1>,
  closure: Option<MigrationFinalAuthorityInventoryClosureV1>,
}

impl MigrationFinalAuthorityInventorySourceV1 for MovingFinalInventory {
  fn next_seed(&mut self) -> EngineResult<Option<MigrationFinalAuthoritySeedV1>> {
    Ok(self.seeds.pop_front())
  }

  fn finish(&mut self) -> EngineResult<MigrationFinalAuthorityInventoryClosureV1> {
    self.closure.take().ok_or_else(|| EngineError::InvalidInput("final inventory closure already consumed".to_string()))
  }
}

struct MutatingFinalMappingSink {
  publisher: Arc<V4FirstAuthorityPublisher>,
  database_id: [u8; 16],
  rows: Vec<MigrationFinalRootMappingV1>,
}

impl MigrationFinalRootMappingSinkV1 for MutatingFinalMappingSink {
  fn record_root_mapping(&mut self, mapping: &MigrationFinalRootMappingV1) -> EngineResult<()> {
    self.rows.push(mapping.clone());
    Ok(())
  }

  fn finish_root_mappings(&mut self, _closure: &MigrationFinalRootMappingClosureV1) -> EngineResult<()> {
    let value = b"sink authority mutation".to_vec();
    let key = digest_parts(ALGORITHM, &[b"chunk:", &value]);
    publish_projection_entities(self.publisher.as_ref(), self.database_id, &[(0, EntryTypeV4::Chunk, key, value)]);
    Ok(())
  }
}

struct CancellingFinalInventory {
  head: Option<MigrationFinalAuthoritySeedV1>,
  closure: MigrationFinalAuthorityInventoryClosureV1,
  cancellation: CancellationToken,
}

impl MigrationFinalAuthorityInventorySourceV1 for CancellingFinalInventory {
  fn next_seed(&mut self) -> EngineResult<Option<MigrationFinalAuthoritySeedV1>> {
    Ok(self.head.take())
  }

  fn finish(&mut self) -> EngineResult<MigrationFinalAuthorityInventoryClosureV1> {
    self.cancellation.cancel();
    Ok(self.closure.clone())
  }
}

struct CancellingFinalMappingSink {
  cancellation: CancellationToken,
}

impl MigrationFinalRootMappingSinkV1 for CancellingFinalMappingSink {
  fn record_root_mapping(&mut self, _mapping: &MigrationFinalRootMappingV1) -> EngineResult<()> {
    Ok(())
  }

  fn finish_root_mappings(&mut self, _closure: &MigrationFinalRootMappingClosureV1) -> EngineResult<()> {
    self.cancellation.cancel();
    Ok(())
  }
}

fn final_authority_digest(rows: &[MigrationFinalAuthoritySeedV1], counts: AuthorityInventoryCountsV1) -> [u8; 32] {
  let mut hasher = blake3::Hasher::new();
  hasher.update(b"aeordb.migration-final-authority-inventory.v1\0");
  for row in rows {
    let kind = match row.seed.kind {
      MigrationBaseCloneSeedKindV1::CurrentHead => 0,
      MigrationBaseCloneSeedKindV1::Snapshot => 1,
      MigrationBaseCloneSeedKindV1::Fork => 2,
      MigrationBaseCloneSeedKindV1::SyncPin => 3,
      MigrationBaseCloneSeedKindV1::Maintenance => 4,
      MigrationBaseCloneSeedKindV1::DetachedProtectedPath => 5,
    };
    hasher.update(&[kind]);
    hasher.update(&(row.authority_identity.len() as u32).to_le_bytes());
    hasher.update(&row.authority_identity);
    hasher.update(&row.source_write_sequence.to_le_bytes());
    match row.system_family_id {
      Some(family_id) => {
        hasher.update(&[1]);
        hasher.update(&family_id.to_le_bytes());
      }
      None => {
        hasher.update(&[0]);
      }
    }
    hasher.update(&row.logical_bytes.to_le_bytes());
    hasher.update(&(row.seed.path.len() as u32).to_le_bytes());
    hasher.update(row.seed.path.as_bytes());
    hasher.update(&[row.seed.entry_type.to_u8()]);
    hasher.update(&(row.seed.hash.len() as u16).to_le_bytes());
    hasher.update(&row.seed.hash);
  }
  hasher.update(b"authority-counts\0");
  for value in [
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
  ] {
    hasher.update(&value.to_le_bytes());
  }
  *hasher.finalize().as_bytes()
}

fn final_inventory_closure(
  fixture: &ProjectionFixture,
  freeze: &MigrationSourceWriteFreezeV1<'_>,
  rows: &[MigrationFinalAuthoritySeedV1],
  seed_counts: MigrationFinalAuthoritySeedCountsV1,
) -> MigrationFinalAuthorityInventoryClosureV1 {
  let registry = embedded_system_family_registry(fixture.permit.hash_algorithm()).unwrap();
  let source_authority_counts = AuthorityInventoryCountsV1 {
    protected_families: u64::from(registry.family_count),
    roots: seed_counts.root_count().unwrap(),
    snapshots: seed_counts.snapshots,
    forks: seed_counts.forks,
    ..Default::default()
  };
  MigrationFinalAuthorityInventoryClosureV1 {
    complete: true,
    database_id: fixture.permit.database_id(),
    source_physical_instance_id: fixture.permit.source_physical_instance_id(),
    source_physical_identity: freeze.authority().physical_identity,
    source_header_sequence: freeze.authority().header_sequence,
    frozen_source_root: freeze.authority().namespace_root.clone(),
    frozen_source_publication_sequence: freeze.authority().hard_publication_frontier,
    unresolved_family_count: 0,
    source_authority_counts,
    seed_counts,
    seed_count: rows.len() as u64,
    authority_digest: final_authority_digest(rows, source_authority_counts),
    system_family_registry_fingerprint: registry.operational_fingerprint.clone(),
  }
}

fn final_head_rows(freeze: &MigrationSourceWriteFreezeV1<'_>) -> Vec<MigrationFinalAuthoritySeedV1> {
  vec![MigrationFinalAuthoritySeedV1 {
    authority_identity: Vec::new(),
    source_write_sequence: freeze.authority().hard_publication_frontier,
    system_family_id: None,
    logical_bytes: 0,
    seed: MigrationBaseCloneSeedV1 {
      kind: MigrationBaseCloneSeedKindV1::CurrentHead,
      path: "/".to_string(),
      entry_type: EntryType::DirectoryIndex,
      hash: freeze.authority().namespace_root.clone(),
    },
  }]
}

fn final_authority_request<'request, 'freeze, 'source>(
  fixture: &'request ProjectionFixture,
  namespace: &'request MigrationFinalNamespaceReconciliationReceiptV1<'freeze, 'source>,
  inventory: &'request mut dyn MigrationFinalAuthorityInventorySourceV1,
  prior_mappings: &'request mut dyn MigrationFinalPriorRootMappingLookupV1,
  root_sink: &'request mut dyn MigrationFinalRootMappingSinkV1,
  cancellation: &'request CancellationToken,
) -> MigrationFinalAuthorityReconciliationRequestV1<'request, 'freeze, 'source> {
  MigrationFinalAuthorityReconciliationRequestV1 {
    permit: &fixture.permit,
    namespace,
    inventory,
    prior_mappings,
    root_sink,
    destination: fixture.destination.publisher(),
    authority: &fixture.authority,
    memory: &fixture.memory,
    cancellation,
    publication_timestamp_ms: 1_700_000_003_000,
    maximum_memory_bytes: 16 << 20,
    maximum_work_items: 100_000,
    maximum_subtree_memory_bytes: 16 << 20,
    maximum_subtree_work_items: 100_000,
    maximum_total_subtree_work_items: 100_000,
    maximum_decoded_chunk_bytes: 1 << 20,
    maximum_destination_entity_bytes: 1 << 20,
    maximum_directory_depth: 128,
  }
}

fn final_authority_error(
  request: MigrationFinalAuthorityReconciliationRequestV1<'_, '_, '_>,
) -> MigrationFinalAuthorityReconciliationErrorV1 {
  match execute_final_authority_reconciliation_v1(request) {
    Ok(_) => panic!("final authority reconciliation unexpectedly succeeded"),
    Err(error) => error,
  }
}

fn final_retirement_owner(fixture: &ProjectionFixture, cancellation: &CancellationToken) -> RetirementJournalOwnerV1 {
  RetirementJournalOwnerV1::new_chain(
    fixture.permit.hash_algorithm(),
    fixture.permit.database_id(),
    1,
    901,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    cancellation,
    &fixture.memory,
  )
  .unwrap()
}

fn final_progress_transition(
  fixture: &ProjectionFixture,
  phase: MigrationPhaseV1,
  state: MigrationProgressStateV1,
  step: u64,
) -> MigrationProgressTransitionRequestV1 {
  let updated_at_ms = 1_700_000_001_000 + step as i64;
  MigrationProgressTransitionRequestV1 {
    phase,
    state,
    flags: MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
    destination_header_sequence: 0,
    copied_through_write_sequence: 0,
    reconciled_through_publication_sequence: 0,
    namespace_count: 0,
    entity_count: 0,
    copied_bytes: 0,
    updated_at_ms,
    legacy_root_map_control_payload_hash: vec![0; fixture.permit.hash_algorithm().hash_length()],
    last_error_evidence: vec![0; fixture.permit.hash_algorithm().hash_length()],
    publication_timestamp_ms: updated_at_ms as u64 + 10,
    monotonic_now_ms: 20_000 + step,
  }
}

fn advance_to_final_freeze_running(fixture: &ProjectionFixture, owner: &MigrationStateOwnerV1, retirement: &mut RetirementJournalOwnerV1) {
  for (phase, state, step) in [
    (MigrationPhaseV1::Preflight, MigrationProgressStateV1::Running, 100),
    (MigrationPhaseV1::Preflight, MigrationProgressStateV1::Complete, 200),
    (MigrationPhaseV1::Copy, MigrationProgressStateV1::Pending, 300),
    (MigrationPhaseV1::Copy, MigrationProgressStateV1::Running, 400),
    (MigrationPhaseV1::Copy, MigrationProgressStateV1::Complete, 500),
    (MigrationPhaseV1::Reconcile, MigrationProgressStateV1::Pending, 600),
    (MigrationPhaseV1::Reconcile, MigrationProgressStateV1::Running, 700),
    (MigrationPhaseV1::Reconcile, MigrationProgressStateV1::Complete, 800),
    (MigrationPhaseV1::FinalFreeze, MigrationProgressStateV1::Pending, 900),
    (MigrationPhaseV1::FinalFreeze, MigrationProgressStateV1::Running, 1_000),
  ] {
    owner.transition_progress(final_progress_transition(fixture, phase, state, step), retirement).unwrap();
  }
}

#[test]
fn final_authority_closes_the_live_head_mapping_without_source_mutation() {
  let fixture = ProjectionFixture::new();
  let cancellation = CancellationToken::new();
  let before = fs::read(&fixture.source_path).unwrap();
  let freeze = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
    permit: &fixture.permit,
    source: &fixture.source,
    cancellation: &cancellation,
    acquisition_timeout: Duration::from_secs(2),
  })
  .unwrap();
  let namespace = execute_final_namespace_reconciliation_v1(final_reconciliation_request(
    &fixture,
    &freeze,
    &cancellation,
    &fixture.basis_root,
    &fixture.current_destination_tree_root,
  ))
  .unwrap();
  let rows = vec![MigrationFinalAuthoritySeedV1 {
    authority_identity: Vec::new(),
    source_write_sequence: freeze.authority().hard_publication_frontier,
    system_family_id: None,
    logical_bytes: 0,
    seed: MigrationBaseCloneSeedV1 {
      kind: MigrationBaseCloneSeedKindV1::CurrentHead,
      path: "/".to_string(),
      entry_type: EntryType::DirectoryIndex,
      hash: freeze.authority().namespace_root.clone(),
    },
  }];
  let seed_counts = MigrationFinalAuthoritySeedCountsV1 { current_heads: 1, ..Default::default() };
  let mut inventory =
    FinalInventoryStream { seeds: rows.clone().into(), closure: final_inventory_closure(&fixture, &freeze, &rows, seed_counts) };
  let mut prior = FinalPriorMappings::default();
  let mut sink = FinalMappingSink::default();
  let destination_head_before = fixture.destination.publisher().observe().unwrap().selected.header.head_hash;

  let receipt = execute_final_authority_reconciliation_v1(MigrationFinalAuthorityReconciliationRequestV1 {
    permit: &fixture.permit,
    namespace: &namespace,
    inventory: &mut inventory,
    prior_mappings: &mut prior,
    root_sink: &mut sink,
    destination: fixture.destination.publisher(),
    authority: &fixture.authority,
    memory: &fixture.memory,
    cancellation: &cancellation,
    publication_timestamp_ms: 1_700_000_003_000,
    maximum_memory_bytes: 16 << 20,
    maximum_work_items: 100_000,
    maximum_subtree_memory_bytes: 16 << 20,
    maximum_subtree_work_items: 100_000,
    maximum_total_subtree_work_items: 100_000,
    maximum_decoded_chunk_bytes: 1 << 20,
    maximum_destination_entity_bytes: 1 << 20,
    maximum_directory_depth: 128,
  })
  .unwrap();

  assert_eq!(receipt.processed_seed_count, 1);
  assert_eq!(receipt.reused_mapping_count, 0);
  assert_eq!(receipt.translated_seed_count, 0);
  assert_eq!(receipt.omitted_seed_count, 0);
  assert_eq!(prior.lookups, 0, "the live HEAD mapping must come from the d2 receipt");
  assert_eq!(sink.rows.len(), 1);
  assert_eq!(sink.rows[0].source_root, freeze.authority().namespace_root);
  assert_eq!(sink.rows[0].destination_tree_root.as_deref(), Some(namespace.destination_tree_root.as_slice()));
  assert_eq!(sink.rows[0].destination_namespace_root.as_deref(), Some(namespace.destination_namespace_root.as_slice()));
  assert!(sink.closure.is_some());
  receipt.proof().validate_live(fixture.destination.publisher()).unwrap();
  assert_eq!(fixture.destination.publisher().observe().unwrap().selected.header.head_hash, destination_head_before);
  assert_eq!(fs::read(&fixture.source_path).unwrap(), before);
}

#[test]
fn only_live_final_authority_proof_can_complete_the_ampr_final_freeze() {
  let fixture = ProjectionFixture::new();
  let cancellation = CancellationToken::new();
  let mut retirement = final_retirement_owner(&fixture, &cancellation);
  let (owner, _) = MigrationStateOwnerV1::acquire(
    fixture.destination.shared_publisher(),
    fixture.permit.clone(),
    MigrationAcquisitionRequestV1 {
      holder_boot_id: id(0x61),
      acquired_at_ms: 1_700_000_000_200,
      lease_duration_ms: 60_000,
      publication_timestamp_ms: 1_700_000_000_300,
      monotonic_now_ms: 10_000,
    },
    &mut retirement,
  )
  .unwrap();
  let (_gc_owner, _) = MigrationSourceGcSuspensionOwnerV1::suspend(
    &fixture.source,
    &owner,
    MigrationSourceGcSuspensionRequestV1 {
      suspended_at_ms: 1_700_000_001_000,
      publication_timestamp_ms: 1_700_000_001_010,
      monotonic_now_ms: 20_000,
    },
    &mut retirement,
  )
  .unwrap();
  advance_to_final_freeze_running(&fixture, &owner, &mut retirement);

  let before = fs::read(&fixture.source_path).unwrap();
  let freeze = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
    permit: &fixture.permit,
    source: &fixture.source,
    cancellation: &cancellation,
    acquisition_timeout: Duration::from_secs(2),
  })
  .unwrap();
  let namespace = execute_final_namespace_reconciliation_v1(final_reconciliation_request(
    &fixture,
    &freeze,
    &cancellation,
    &fixture.basis_root,
    &fixture.current_destination_tree_root,
  ))
  .unwrap();
  let rows = vec![MigrationFinalAuthoritySeedV1 {
    authority_identity: Vec::new(),
    source_write_sequence: freeze.authority().hard_publication_frontier,
    system_family_id: None,
    logical_bytes: 0,
    seed: MigrationBaseCloneSeedV1 {
      kind: MigrationBaseCloneSeedKindV1::CurrentHead,
      path: "/".to_string(),
      entry_type: EntryType::DirectoryIndex,
      hash: freeze.authority().namespace_root.clone(),
    },
  }];
  let counts = MigrationFinalAuthoritySeedCountsV1 { current_heads: 1, ..Default::default() };
  let mut inventory =
    FinalInventoryStream { seeds: rows.clone().into(), closure: final_inventory_closure(&fixture, &freeze, &rows, counts) };
  let mut prior = FinalPriorMappings::default();
  let mut sink = FinalMappingSink::default();
  let final_authority = execute_final_authority_reconciliation_v1(MigrationFinalAuthorityReconciliationRequestV1 {
    permit: &fixture.permit,
    namespace: &namespace,
    inventory: &mut inventory,
    prior_mappings: &mut prior,
    root_sink: &mut sink,
    destination: fixture.destination.publisher(),
    authority: &fixture.authority,
    memory: &fixture.memory,
    cancellation: &cancellation,
    publication_timestamp_ms: 1_700_000_003_000,
    maximum_memory_bytes: 16 << 20,
    maximum_work_items: 100_000,
    maximum_subtree_memory_bytes: 16 << 20,
    maximum_subtree_work_items: 100_000,
    maximum_total_subtree_work_items: 100_000,
    maximum_decoded_chunk_bytes: 1 << 20,
    maximum_destination_entity_bytes: 1 << 20,
    maximum_directory_depth: 128,
  })
  .unwrap();
  let proof_destination_sequence = final_authority.mapping_closure.destination_header_sequence;

  let completion = owner
    .complete_final_freeze(
      MigrationFinalFreezeCompletionRequestV1 {
        proof: final_authority.proof(),
        updated_at_ms: 1_700_000_003_100,
        publication_timestamp_ms: 1_700_000_003_200,
        monotonic_now_ms: 40_000,
      },
      &mut retirement,
    )
    .unwrap();
  assert_eq!(completion.phase, MigrationPhaseV1::FinalFreeze);
  assert_eq!(completion.state, MigrationProgressStateV1::Complete);

  let progress = fixture
    .destination
    .publisher()
    .load_mutable_system_control(SystemControlKindV1::MigrationProgress, &fixture.permit.database_id(), &fixture.permit.migration_id())
    .unwrap()
    .unwrap();
  let progress = decode_migration_progress_control(&progress.bytes, fixture.permit.hash_algorithm()).unwrap();
  assert_eq!(progress.body.phase, MigrationPhaseV1::FinalFreeze);
  assert_eq!(progress.body.state, MigrationProgressStateV1::Complete);
  assert_ne!(progress.body.flags & MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD, 0);
  assert_eq!(progress.body.reconciled_through_publication_sequence, freeze.authority().hard_publication_frontier);
  assert_eq!(progress.body.destination_header_sequence, proof_destination_sequence);
  freeze.validate_unchanged().unwrap();
  assert_eq!(fs::read(&fixture.source_path).unwrap(), before);

  let completed_retry = owner
    .complete_final_freeze(
      MigrationFinalFreezeCompletionRequestV1 {
        proof: final_authority.proof(),
        updated_at_ms: 1_700_000_003_300,
        publication_timestamp_ms: 1_700_000_003_400,
        monotonic_now_ms: 41_000,
      },
      &mut retirement,
    )
    .unwrap();
  assert!(completed_retry.idempotent);

  let mut retry_inventory =
    FinalInventoryStream { seeds: rows.clone().into(), closure: final_inventory_closure(&fixture, &freeze, &rows, counts) };
  let mut retry_prior = FinalPriorMappings::default();
  let mut retry_sink = FinalMappingSink::default();
  let retry_authority = execute_final_authority_reconciliation_v1(final_authority_request(
    &fixture,
    &namespace,
    &mut retry_inventory,
    &mut retry_prior,
    &mut retry_sink,
    &cancellation,
  ))
  .unwrap();
  assert_eq!(retry_authority.mapping_closure.destination_header_sequence, proof_destination_sequence);
  let retry = owner
    .complete_final_freeze(
      MigrationFinalFreezeCompletionRequestV1 {
        proof: retry_authority.proof(),
        updated_at_ms: 1_700_000_003_500,
        publication_timestamp_ms: 1_700_000_003_600,
        monotonic_now_ms: 42_000,
      },
      &mut retirement,
    )
    .unwrap();
  assert!(retry.idempotent, "non-HEAD header history must not change the final namespace watermark");
}

#[test]
fn selected_root_map_and_live_frozen_source_begin_destination_verification_idempotently() {
  let fixture = ProjectionFixture::new();
  let cancellation = CancellationToken::new();
  let mut retirement = final_retirement_owner(&fixture, &cancellation);
  let (owner, _) = MigrationStateOwnerV1::acquire(
    fixture.destination.shared_publisher(),
    fixture.permit.clone(),
    MigrationAcquisitionRequestV1 {
      holder_boot_id: id(0x61),
      acquired_at_ms: 1_700_000_000_200,
      lease_duration_ms: 60_000,
      publication_timestamp_ms: 1_700_000_000_300,
      monotonic_now_ms: 10_000,
    },
    &mut retirement,
  )
  .unwrap();
  let (_gc_owner, _) = MigrationSourceGcSuspensionOwnerV1::suspend(
    &fixture.source,
    &owner,
    MigrationSourceGcSuspensionRequestV1 {
      suspended_at_ms: 1_700_000_001_000,
      publication_timestamp_ms: 1_700_000_001_010,
      monotonic_now_ms: 20_000,
    },
    &mut retirement,
  )
  .unwrap();
  advance_to_final_freeze_running(&fixture, &owner, &mut retirement);

  let source_before = fs::read(&fixture.source_path).unwrap();
  let freeze = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
    permit: &fixture.permit,
    source: &fixture.source,
    cancellation: &cancellation,
    acquisition_timeout: Duration::from_secs(2),
  })
  .unwrap();
  let namespace = execute_final_namespace_reconciliation_v1(final_reconciliation_request(
    &fixture,
    &freeze,
    &cancellation,
    &fixture.basis_root,
    &fixture.current_destination_tree_root,
  ))
  .unwrap();
  let rows = final_head_rows(&freeze);
  let counts = MigrationFinalAuthoritySeedCountsV1 { current_heads: 1, ..Default::default() };
  let mut inventory =
    FinalInventoryStream { seeds: rows.clone().into(), closure: final_inventory_closure(&fixture, &freeze, &rows, counts) };
  let mut prior = FinalPriorMappings::default();
  let scratch = fixture.destination.path().parent().unwrap().join("root-map-destination-verification");
  fs::create_dir(&scratch).unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700)).unwrap();
  }
  let mut workspace = LegacyRootMapStagingWorkspaceV1::create(
    fixture.destination.path(),
    LegacyRootMapWorkspaceIdentityV1::new(
      fixture.permit.database_id(),
      fixture.permit.migration_id(),
      fixture.permit.database_id(),
      fixture.permit.source_physical_instance_id(),
      fixture.permit.destination_physical_instance_id(),
      1,
      fixture.permit.hash_algorithm(),
    )
    .unwrap(),
    1_700_000_003_000,
    LegacyRootMapWorkspaceOptionsV1::new(Some(scratch), 64 << 20, 100, 0, 1 << 20, 2, 8, 2 << 20).unwrap(),
    cancellation.clone(),
    &fixture.memory,
  )
  .unwrap();
  let mut root_sink =
    LegacyRootMapProducerSinkV1::new(&mut workspace, &fixture.authority, freeze.authority().hard_publication_frontier).unwrap();
  let final_authority = execute_final_authority_reconciliation_v1(final_authority_request(
    &fixture,
    &namespace,
    &mut inventory,
    &mut prior,
    &mut root_sink,
    &cancellation,
  ))
  .unwrap();
  drop(root_sink);
  owner
    .complete_final_freeze(
      MigrationFinalFreezeCompletionRequestV1 {
        proof: final_authority.proof(),
        updated_at_ms: 1_700_000_003_100,
        publication_timestamp_ms: 1_700_000_003_200,
        monotonic_now_ms: 40_000,
      },
      &mut retirement,
    )
    .unwrap();
  let map_receipt = LegacyRootMapOwnerV1::new(fixture.destination.publisher())
    .publish(
      LegacyRootMapPublicationRequestV1 {
        workspace,
        retirement_owner: &mut retirement,
        cancellation: &cancellation,
        monotonic_now_ms: 41_000,
      },
      &fixture.memory,
    )
    .unwrap();
  let reader = VerifiedLegacyRootMapReaderV1::open(
    fixture.destination.publisher(),
    fixture.permit.database_id(),
    fixture.permit.migration_id(),
    &cancellation,
    &fixture.memory,
  )
  .unwrap();
  let prepublication_destination_sequence = reader.destination_header_sequence();
  let first = owner
    .begin_destination_verification(
      MigrationDestinationVerificationRequestV1 {
        proof: final_authority.proof(),
        root_map: &reader,
        cancellation: &cancellation,
        expected_map_generation: 1,
        updated_at_ms: 1_700_000_003_300,
        publication_timestamp_ms: 1_700_000_003_400,
        monotonic_now_ms: 42_000,
      },
      &mut retirement,
    )
    .unwrap();
  assert_eq!((first.phase, first.state, first.idempotent), (MigrationPhaseV1::DestinationVerify, MigrationProgressStateV1::Pending, false));

  let progress = fixture
    .destination
    .publisher()
    .load_mutable_system_control(SystemControlKindV1::MigrationProgress, &fixture.permit.database_id(), &fixture.permit.migration_id())
    .unwrap()
    .unwrap();
  let progress = decode_migration_progress_control(&progress.bytes, fixture.permit.hash_algorithm()).unwrap();
  assert_eq!(progress.body.destination_header_sequence, prepublication_destination_sequence);
  assert_eq!(progress.body.legacy_root_map_control_payload_hash, map_receipt.control_payload_hash);

  let reopened_reader = VerifiedLegacyRootMapReaderV1::open(
    fixture.destination.publisher(),
    fixture.permit.database_id(),
    fixture.permit.migration_id(),
    &cancellation,
    &fixture.memory,
  )
  .unwrap();
  let retry = owner
    .begin_destination_verification(
      MigrationDestinationVerificationRequestV1 {
        proof: final_authority.proof(),
        root_map: &reopened_reader,
        cancellation: &cancellation,
        expected_map_generation: 1,
        updated_at_ms: 1_700_000_003_500,
        publication_timestamp_ms: 1_700_000_003_600,
        monotonic_now_ms: 43_000,
      },
      &mut retirement,
    )
    .unwrap();
  assert!(retry.idempotent);
  freeze.validate_unchanged().unwrap();
  assert_eq!(fs::read(&fixture.source_path).unwrap(), source_before);

  let selected_progress = fixture
    .destination
    .publisher()
    .load_mutable_system_control(SystemControlKindV1::MigrationProgress, &fixture.permit.database_id(), &fixture.permit.migration_id())
    .unwrap()
    .unwrap();
  let error = owner
    .begin_destination_verification(
      MigrationDestinationVerificationRequestV1 {
        proof: final_authority.proof(),
        root_map: &reopened_reader,
        cancellation: &cancellation,
        expected_map_generation: 2,
        updated_at_ms: 1_700_000_003_700,
        publication_timestamp_ms: 1_700_000_003_800,
        monotonic_now_ms: 44_000,
      },
      &mut retirement,
    )
    .unwrap_err();
  assert_eq!(error.code(), "migration_destination_verification_map_binding");

  let canceled = CancellationToken::new();
  canceled.cancel();
  let error = owner
    .begin_destination_verification(
      MigrationDestinationVerificationRequestV1 {
        proof: final_authority.proof(),
        root_map: &reopened_reader,
        cancellation: &canceled,
        expected_map_generation: 1,
        updated_at_ms: 1_700_000_003_700,
        publication_timestamp_ms: 1_700_000_003_800,
        monotonic_now_ms: 44_000,
      },
      &mut retirement,
    )
    .unwrap_err();
  assert_eq!(error.code(), "migration_root_map_cancelled");
  assert_eq!(
    fixture
      .destination
      .publisher()
      .load_mutable_system_control(SystemControlKindV1::MigrationProgress, &fixture.permit.database_id(), &fixture.permit.migration_id())
      .unwrap()
      .unwrap(),
    selected_progress
  );

  owner
    .renew(
      MigrationLeaseRenewalRequestV1 {
        renewed_at_ms: 1_700_000_004_000,
        lease_duration_ms: 120_000,
        publication_timestamp_ms: 1_700_000_004_100,
        monotonic_now_ms: 45_000,
      },
      &mut retirement,
    )
    .unwrap();
  let error = owner
    .begin_destination_verification(
      MigrationDestinationVerificationRequestV1 {
        proof: final_authority.proof(),
        root_map: &reopened_reader,
        cancellation: &cancellation,
        expected_map_generation: 1,
        updated_at_ms: 1_700_000_004_200,
        publication_timestamp_ms: 1_700_000_004_300,
        monotonic_now_ms: 46_000,
      },
      &mut retirement,
    )
    .unwrap_err();
  assert_eq!(error.code(), "migration_root_map_selected_changed");
  let fresh_reader = VerifiedLegacyRootMapReaderV1::open(
    fixture.destination.publisher(),
    fixture.permit.database_id(),
    fixture.permit.migration_id(),
    &cancellation,
    &fixture.memory,
  )
  .unwrap();
  let recovered = owner
    .begin_destination_verification(
      MigrationDestinationVerificationRequestV1 {
        proof: final_authority.proof(),
        root_map: &fresh_reader,
        cancellation: &cancellation,
        expected_map_generation: 1,
        updated_at_ms: 1_700_000_004_400,
        publication_timestamp_ms: 1_700_000_004_500,
        monotonic_now_ms: 47_000,
      },
      &mut retirement,
    )
    .unwrap();
  assert!(recovered.idempotent);

  let completion_reader = VerifiedLegacyRootMapReaderV1::open(
    fixture.destination.publisher(),
    fixture.permit.database_id(),
    fixture.permit.migration_id(),
    &cancellation,
    &fixture.memory,
  )
  .unwrap();
  let started = owner
    .start_destination_full_verification(
      MigrationDestinationVerificationCompletionRequestV1 {
        proof: final_authority.proof(),
        root_map: &completion_reader,
        cancellation: &cancellation,
        expected_map_generation: 1,
        updated_at_ms: 1_700_000_004_600,
        publication_timestamp_ms: 1_700_000_004_700,
        monotonic_now_ms: 48_000,
      },
      &mut retirement,
    )
    .unwrap();
  assert_eq!(
    (started.phase, started.state, started.idempotent),
    (MigrationPhaseV1::DestinationVerify, MigrationProgressStateV1::Running, false)
  );
  let completion_reader = VerifiedLegacyRootMapReaderV1::open(
    fixture.destination.publisher(),
    fixture.permit.database_id(),
    fixture.permit.migration_id(),
    &cancellation,
    &fixture.memory,
  )
  .unwrap();
  let completion_destination_sequence = completion_reader.destination_header_sequence();
  let completed = owner
    .complete_destination_verification(
      MigrationDestinationVerificationCompletionRequestV1 {
        proof: final_authority.proof(),
        root_map: &completion_reader,
        cancellation: &cancellation,
        expected_map_generation: 1,
        updated_at_ms: 1_700_000_004_800,
        publication_timestamp_ms: 1_700_000_004_900,
        monotonic_now_ms: 49_000,
      },
      &mut retirement,
    )
    .unwrap();
  assert_eq!(
    (completed.phase, completed.state, completed.idempotent),
    (MigrationPhaseV1::DestinationVerify, MigrationProgressStateV1::Complete, false)
  );
  let selected = fixture
    .destination
    .publisher()
    .load_mutable_system_control(SystemControlKindV1::MigrationProgress, &fixture.permit.database_id(), &fixture.permit.migration_id())
    .unwrap()
    .unwrap();
  let selected = decode_migration_progress_control(&selected.bytes, fixture.permit.hash_algorithm()).unwrap();
  assert_eq!(selected.body.destination_header_sequence, completion_destination_sequence);
  assert_ne!(selected.body.flags & MIGRATION_PROGRESS_FLAG_DESTINATION_FULL_VERIFIED, 0);

  let retry_reader = VerifiedLegacyRootMapReaderV1::open(
    fixture.destination.publisher(),
    fixture.permit.database_id(),
    fixture.permit.migration_id(),
    &cancellation,
    &fixture.memory,
  )
  .unwrap();
  let retry = owner
    .complete_destination_verification(
      MigrationDestinationVerificationCompletionRequestV1 {
        proof: final_authority.proof(),
        root_map: &retry_reader,
        cancellation: &cancellation,
        expected_map_generation: 1,
        updated_at_ms: 1_700_000_005_000,
        publication_timestamp_ms: 1_700_000_005_100,
        monotonic_now_ms: 50_000,
      },
      &mut retirement,
    )
    .unwrap();
  assert!(retry.idempotent);
  freeze.validate_unchanged().unwrap();
  assert_eq!(fs::read(&fixture.source_path).unwrap(), source_before);
}

#[test]
fn final_authority_rejects_noncanonical_seeds_and_inexact_closures() {
  let fixture = ProjectionFixture::new();
  let cancellation = CancellationToken::new();
  let freeze = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
    permit: &fixture.permit,
    source: &fixture.source,
    cancellation: &cancellation,
    acquisition_timeout: Duration::from_secs(2),
  })
  .unwrap();
  let namespace = execute_final_namespace_reconciliation_v1(final_reconciliation_request(
    &fixture,
    &freeze,
    &cancellation,
    &fixture.basis_root,
    &fixture.current_destination_tree_root,
  ))
  .unwrap();
  let head = final_head_rows(&freeze).remove(0);
  let exact_rows = vec![head.clone()];
  let exact_counts = MigrationFinalAuthoritySeedCountsV1 { current_heads: 1, ..Default::default() };
  let exact_closure = final_inventory_closure(&fixture, &freeze, &exact_rows, exact_counts);
  let before = fixture.destination.publisher().observe().unwrap().selected.header.clone();

  let mut wrong_head = head.clone();
  wrong_head.seed.path = "/not-head".to_string();
  let mut invalid_hash = head.clone();
  invalid_hash.seed.hash = vec![0; fixture.permit.hash_algorithm().hash_length()];
  let snapshot_without_head = MigrationFinalAuthoritySeedV1 {
    authority_identity: b"snapshot-1".to_vec(),
    source_write_sequence: 1,
    system_family_id: None,
    logical_bytes: 17,
    seed: MigrationBaseCloneSeedV1 {
      kind: MigrationBaseCloneSeedKindV1::Snapshot,
      path: "/".to_string(),
      entry_type: EntryType::DirectoryIndex,
      hash: fixture.basis_root.clone(),
    },
  };
  let invalid_detached = MigrationFinalAuthoritySeedV1 {
    authority_identity: b"detached-1".to_vec(),
    source_write_sequence: 1,
    system_family_id: Some(0x0013),
    logical_bytes: 17,
    seed: MigrationBaseCloneSeedV1 {
      kind: MigrationBaseCloneSeedKindV1::DetachedProtectedPath,
      path: "/.aeordb-config/indexes.json".to_string(),
      entry_type: EntryType::FileRecord,
      hash: fixture.basis_file.clone(),
    },
  };
  for (rows, expected) in [
    (vec![head.clone(), head.clone()], "migration_final_authority_order"),
    (vec![wrong_head], "migration_final_authority_head"),
    (vec![invalid_hash], "migration_final_authority_seed"),
    (vec![snapshot_without_head], "migration_final_authority_retained_root"),
    (vec![head.clone(), invalid_detached], "migration_final_authority_family_binding"),
  ] {
    let mut inventory = FinalInventoryStream { seeds: rows.into(), closure: exact_closure.clone() };
    let mut prior = FinalPriorMappings::default();
    let mut sink = FinalMappingSink::default();
    let error = final_authority_error(final_authority_request(&fixture, &namespace, &mut inventory, &mut prior, &mut sink, &cancellation));
    assert_eq!(error.code(), expected);
    assert!(sink.closure.is_none());
    assert_eq!(fixture.destination.publisher().observe().unwrap().selected.header, before);
  }

  for mutation in 0..15 {
    let mut closure = exact_closure.clone();
    match mutation {
      0 => closure.complete = false,
      1 => closure.unresolved_family_count = 1,
      2 => closure.database_id[0] ^= 0xff,
      3 => closure.source_physical_instance_id[0] ^= 0xff,
      4 => closure.source_header_sequence += 1,
      5 => closure.frozen_source_root[0] ^= 0xff,
      6 => closure.frozen_source_publication_sequence += 1,
      7 => closure.seed_counts.current_heads = 0,
      8 => closure.seed_count += 1,
      9 => closure.authority_digest[0] ^= 0xff,
      10 => closure.system_family_registry_fingerprint[0] ^= 0xff,
      11 => closure.source_authority_counts.protected_families -= 1,
      12 => closure.source_authority_counts.roots += 1,
      13 => closure.source_authority_counts.snapshots += 1,
      14 => closure.source_authority_counts.modules += 1,
      _ => unreachable!(),
    }
    let mut inventory = FinalInventoryStream { seeds: exact_rows.clone().into(), closure };
    let mut prior = FinalPriorMappings::default();
    let mut sink = FinalMappingSink::default();
    let error = final_authority_error(final_authority_request(&fixture, &namespace, &mut inventory, &mut prior, &mut sink, &cancellation));
    assert_eq!(error.code(), "migration_final_authority_closure", "closure mutation {mutation}");
    assert!(sink.closure.is_none());
    assert_eq!(fixture.destination.publisher().observe().unwrap().selected.header, before);
  }
}

#[test]
fn final_authority_reuses_valid_roots_and_applies_detached_family_policy() {
  let fixture = ProjectionFixture::new();
  let cancellation = CancellationToken::new();
  let freeze = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
    permit: &fixture.permit,
    source: &fixture.source,
    cancellation: &cancellation,
    acquisition_timeout: Duration::from_secs(2),
  })
  .unwrap();
  let namespace = execute_final_namespace_reconciliation_v1(final_reconciliation_request(
    &fixture,
    &freeze,
    &cancellation,
    &fixture.basis_root,
    &fixture.current_destination_tree_root,
  ))
  .unwrap();
  let mut rows = final_head_rows(&freeze);
  rows.extend([
    MigrationFinalAuthoritySeedV1 {
      authority_identity: b"snapshot-1".to_vec(),
      source_write_sequence: 1,
      system_family_id: None,
      logical_bytes: 17,
      seed: MigrationBaseCloneSeedV1 {
        kind: MigrationBaseCloneSeedKindV1::Snapshot,
        path: "/".to_string(),
        entry_type: EntryType::DirectoryIndex,
        hash: fixture.basis_root.clone(),
      },
    },
    MigrationFinalAuthoritySeedV1 {
      authority_identity: b"01-detached-required".to_vec(),
      source_write_sequence: 1,
      system_family_id: Some(0x0001),
      logical_bytes: 17,
      seed: MigrationBaseCloneSeedV1 {
        kind: MigrationBaseCloneSeedKindV1::DetachedProtectedPath,
        path: "/.aeordb-config/indexes.json".to_string(),
        entry_type: EntryType::FileRecord,
        hash: fixture.basis_file.clone(),
      },
    },
    MigrationFinalAuthoritySeedV1 {
      authority_identity: b"02-detached-omit".to_vec(),
      source_write_sequence: 1,
      system_family_id: Some(0x0013),
      logical_bytes: 17,
      seed: MigrationBaseCloneSeedV1 {
        kind: MigrationBaseCloneSeedKindV1::DetachedProtectedPath,
        path: "/.aeordb-system/api-keys/key".to_string(),
        entry_type: EntryType::FileRecord,
        hash: fixture.basis_file.clone(),
      },
    },
  ]);
  let counts = MigrationFinalAuthoritySeedCountsV1 { current_heads: 1, snapshots: 1, detached_protected: 2, ..Default::default() };
  let mut inventory =
    FinalInventoryStream { seeds: rows.clone().into(), closure: final_inventory_closure(&fixture, &freeze, &rows, counts) };
  let mut prior = FinalPriorMappings::default();
  prior.rows.insert(fixture.basis_root.clone(), fixture.basis_root.clone());
  prior.rows.insert(fixture.basis_file.clone(), fixture.basis_file.clone());
  let mut sink = FinalMappingSink::default();

  let receipt = execute_final_authority_reconciliation_v1(final_authority_request(
    &fixture,
    &namespace,
    &mut inventory,
    &mut prior,
    &mut sink,
    &cancellation,
  ))
  .unwrap();

  assert_eq!(receipt.processed_seed_count, 4);
  assert_eq!(receipt.reused_mapping_count, 2);
  assert_eq!(receipt.translated_seed_count, 0);
  assert_eq!(receipt.omitted_seed_count, 1);
  assert_eq!(prior.lookups, 2, "HEAD and destination-local families must bypass prior-map lookup");
  assert_eq!(sink.rows.len(), 4);
  assert!(sink.rows[1].reused);
  assert!(sink.rows[1].destination_namespace_root.is_some());
  assert!(sink.rows[2].reused);
  assert_eq!(sink.rows[2].destination_entity, Some(fixture.basis_file.clone()));
  assert_eq!(sink.rows[3].destination_entity, None);
  assert_eq!(receipt.mapping_closure.omitted_mapping_count, 1);
  receipt.proof().validate_live(fixture.destination.publisher()).unwrap();
}

#[test]
fn final_authority_rejects_invalid_prior_mapping_entities() {
  let fixture = ProjectionFixture::new();
  let cancellation = CancellationToken::new();
  let freeze = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
    permit: &fixture.permit,
    source: &fixture.source,
    cancellation: &cancellation,
    acquisition_timeout: Duration::from_secs(2),
  })
  .unwrap();
  let namespace = execute_final_namespace_reconciliation_v1(final_reconciliation_request(
    &fixture,
    &freeze,
    &cancellation,
    &fixture.basis_root,
    &fixture.current_destination_tree_root,
  ))
  .unwrap();
  let mut rows = final_head_rows(&freeze);
  rows.push(MigrationFinalAuthoritySeedV1 {
    authority_identity: b"snapshot-1".to_vec(),
    source_write_sequence: 1,
    system_family_id: None,
    logical_bytes: 17,
    seed: MigrationBaseCloneSeedV1 {
      kind: MigrationBaseCloneSeedKindV1::Snapshot,
      path: "/".to_string(),
      entry_type: EntryType::DirectoryIndex,
      hash: fixture.basis_root.clone(),
    },
  });
  let counts = MigrationFinalAuthoritySeedCountsV1 { current_heads: 1, snapshots: 1, ..Default::default() };
  let closure = final_inventory_closure(&fixture, &freeze, &rows, counts);
  let wrong_type = digest_parts(ALGORITHM, &[b"chunk:", b"old"]);
  for (mapped, expected) in [
    (vec![0x44; ALGORITHM.hash_length() - 1], "migration_final_authority_prior_mapping_hash"),
    (digest_parts(ALGORITHM, &[b"missing prior mapping"]), "migration_final_authority_prior_mapping_missing"),
    (wrong_type, "migration_final_authority_prior_mapping_type"),
  ] {
    let mut inventory = FinalInventoryStream { seeds: rows.clone().into(), closure: closure.clone() };
    let mut prior = FinalPriorMappings::default();
    prior.rows.insert(fixture.basis_root.clone(), mapped);
    let mut sink = FinalMappingSink::default();
    let error = final_authority_error(final_authority_request(&fixture, &namespace, &mut inventory, &mut prior, &mut sink, &cancellation));
    assert_eq!(error.code(), expected);
    assert!(sink.closure.is_none());
  }
}

#[test]
fn final_authority_translates_a_new_retained_root_under_the_aggregate_work_bound() {
  let fixture = ProjectionFixture::new();
  let cancellation = CancellationToken::new();
  let (snapshot_chunk, snapshot_file, snapshot_file_value) = projection_file(ALGORITHM, b"new retained snapshot", 1_700_000_000_050);
  let (snapshot_root, snapshot_root_value) = projection_root(ALGORITHM, snapshot_file.clone(), 1_700_000_000_050);
  for (entry_type, version, key, value) in [
    (EntryType::Chunk, 0, snapshot_chunk, b"new retained snapshot".to_vec()),
    (EntryType::FileRecord, 1, snapshot_file, snapshot_file_value),
    (EntryType::DirectoryIndex, 0, snapshot_root.clone(), snapshot_root_value),
  ] {
    fixture.source.store_entry_with_version(entry_type, &key, &value, version).unwrap();
  }
  let before_source = fs::read(&fixture.source_path).unwrap();
  let freeze = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
    permit: &fixture.permit,
    source: &fixture.source,
    cancellation: &cancellation,
    acquisition_timeout: Duration::from_secs(2),
  })
  .unwrap();
  let namespace = execute_final_namespace_reconciliation_v1(final_reconciliation_request(
    &fixture,
    &freeze,
    &cancellation,
    &fixture.basis_root,
    &fixture.current_destination_tree_root,
  ))
  .unwrap();
  let mut rows = final_head_rows(&freeze);
  rows.push(MigrationFinalAuthoritySeedV1 {
    authority_identity: b"snapshot-new".to_vec(),
    source_write_sequence: freeze.authority().hard_publication_frontier,
    system_family_id: None,
    logical_bytes: 21,
    seed: MigrationBaseCloneSeedV1 {
      kind: MigrationBaseCloneSeedKindV1::Snapshot,
      path: "/".to_string(),
      entry_type: EntryType::DirectoryIndex,
      hash: snapshot_root,
    },
  });
  let counts = MigrationFinalAuthoritySeedCountsV1 { current_heads: 1, snapshots: 1, ..Default::default() };
  let closure = final_inventory_closure(&fixture, &freeze, &rows, counts);

  let mut bounded_inventory = FinalInventoryStream { seeds: rows.clone().into(), closure: closure.clone() };
  let mut bounded_prior = FinalPriorMappings::default();
  let mut bounded_sink = FinalMappingSink::default();
  let mut bounded =
    final_authority_request(&fixture, &namespace, &mut bounded_inventory, &mut bounded_prior, &mut bounded_sink, &cancellation);
  bounded.maximum_subtree_work_items = 1;
  bounded.maximum_total_subtree_work_items = 1;
  assert_eq!(final_authority_error(bounded).code(), "migration_clone_work_limit");
  assert!(bounded_sink.closure.is_none());

  let mut inventory = FinalInventoryStream { seeds: rows.into(), closure };
  let mut prior = FinalPriorMappings::default();
  let mut sink = FinalMappingSink::default();
  let receipt = execute_final_authority_reconciliation_v1(final_authority_request(
    &fixture,
    &namespace,
    &mut inventory,
    &mut prior,
    &mut sink,
    &cancellation,
  ))
  .unwrap();
  assert_eq!(receipt.translated_seed_count, 1);
  assert!(receipt.translated_subtree_work_items >= 3);
  assert_eq!(receipt.reused_mapping_count, 0);
  assert_eq!(prior.lookups, 1);
  let translated = sink.rows[1].destination_entity.as_ref().unwrap();
  let loaded = fixture.destination.publisher().load_immutable_entity_bounded(translated, 1 << 20).unwrap().unwrap();
  assert_eq!(loaded.entry_type, EntryTypeV4::DirectoryIndex);
  assert_eq!(fs::read(&fixture.source_path).unwrap(), before_source);
  receipt.proof().validate_live(fixture.destination.publisher()).unwrap();
}

#[test]
fn final_authority_bounds_cancellation_and_component_failures_never_issue_a_proof() {
  let fixture = ProjectionFixture::new();
  let cancellation = CancellationToken::new();
  let freeze = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
    permit: &fixture.permit,
    source: &fixture.source,
    cancellation: &cancellation,
    acquisition_timeout: Duration::from_secs(2),
  })
  .unwrap();
  let namespace = execute_final_namespace_reconciliation_v1(final_reconciliation_request(
    &fixture,
    &freeze,
    &cancellation,
    &fixture.basis_root,
    &fixture.current_destination_tree_root,
  ))
  .unwrap();
  let rows = final_head_rows(&freeze);
  let counts = MigrationFinalAuthoritySeedCountsV1 { current_heads: 1, ..Default::default() };
  let closure = final_inventory_closure(&fixture, &freeze, &rows, counts);
  let before = fixture.destination.publisher().observe().unwrap().selected.header.clone();

  let mut inventory = FinalInventoryStream { seeds: rows.clone().into(), closure: closure.clone() };
  let mut prior = FinalPriorMappings::default();
  let mut sink = FinalMappingSink::default();
  let mut invalid = final_authority_request(&fixture, &namespace, &mut inventory, &mut prior, &mut sink, &cancellation);
  invalid.maximum_memory_bytes = 0;
  assert_eq!(final_authority_error(invalid).code(), "migration_final_authority_bounds");

  let mut inventory = FinalInventoryStream { seeds: rows.clone().into(), closure: closure.clone() };
  let mut prior = FinalPriorMappings::default();
  let mut sink = FinalMappingSink::default();
  let mut invalid = final_authority_request(&fixture, &namespace, &mut inventory, &mut prior, &mut sink, &cancellation);
  invalid.maximum_directory_depth = 1_001;
  assert_eq!(final_authority_error(invalid).code(), "migration_final_authority_bounds");

  let canceled = CancellationToken::new();
  canceled.cancel();
  let mut inventory = FinalInventoryStream { seeds: rows.clone().into(), closure: closure.clone() };
  let mut prior = FinalPriorMappings::default();
  let mut sink = FinalMappingSink::default();
  assert_eq!(
    final_authority_error(final_authority_request(&fixture, &namespace, &mut inventory, &mut prior, &mut sink, &canceled)).code(),
    "migration_final_authority_cancelled"
  );

  let mut inventory = FinalInventoryStream { seeds: rows.clone().into(), closure: closure.clone() };
  let mut prior = FinalPriorMappings::default();
  let mut sink = FinalMappingSink::default();
  let mut bounded = final_authority_request(&fixture, &namespace, &mut inventory, &mut prior, &mut sink, &cancellation);
  bounded.maximum_memory_bytes = 1;
  assert_eq!(final_authority_error(bounded).code(), "migration_final_authority_memory_limit");

  let mut two_rows = rows.clone();
  two_rows.push(MigrationFinalAuthoritySeedV1 {
    authority_identity: b"snapshot-1".to_vec(),
    source_write_sequence: 1,
    system_family_id: None,
    logical_bytes: 17,
    seed: MigrationBaseCloneSeedV1 {
      kind: MigrationBaseCloneSeedKindV1::Snapshot,
      path: "/".to_string(),
      entry_type: EntryType::DirectoryIndex,
      hash: fixture.basis_root.clone(),
    },
  });
  let two_counts = MigrationFinalAuthoritySeedCountsV1 { current_heads: 1, snapshots: 1, ..Default::default() };
  let two_closure = final_inventory_closure(&fixture, &freeze, &two_rows, two_counts);
  let mut inventory = FinalInventoryStream { seeds: two_rows.clone().into(), closure: two_closure.clone() };
  let mut prior = FinalPriorMappings::default();
  let mut sink = FinalMappingSink::default();
  let mut bounded = final_authority_request(&fixture, &namespace, &mut inventory, &mut prior, &mut sink, &cancellation);
  bounded.maximum_work_items = 1;
  assert_eq!(final_authority_error(bounded).code(), "migration_final_authority_work_limit");

  let mut inventory = FailingFinalInventory { head: None, closure: closure.clone(), fail_next: true, fail_finish: false };
  let mut prior = FinalPriorMappings::default();
  let mut sink = FinalMappingSink::default();
  assert_eq!(
    final_authority_error(final_authority_request(&fixture, &namespace, &mut inventory, &mut prior, &mut sink, &cancellation)).code(),
    "migration_final_authority_inventory_source"
  );

  let mut inventory = FailingFinalInventory { head: Some(rows[0].clone()), closure: closure.clone(), fail_next: false, fail_finish: true };
  let mut prior = FinalPriorMappings::default();
  let mut sink = FinalMappingSink::default();
  assert_eq!(
    final_authority_error(final_authority_request(&fixture, &namespace, &mut inventory, &mut prior, &mut sink, &cancellation)).code(),
    "migration_final_authority_inventory_source"
  );

  let mut inventory = FinalInventoryStream { seeds: two_rows.into(), closure: two_closure };
  let mut prior = FailingFinalPriorMappings;
  let mut sink = FinalMappingSink::default();
  assert_eq!(
    final_authority_error(final_authority_request(&fixture, &namespace, &mut inventory, &mut prior, &mut sink, &cancellation)).code(),
    "migration_final_authority_prior_mapping"
  );

  for fail_finish in [false, true] {
    let mut inventory = FinalInventoryStream { seeds: rows.clone().into(), closure: closure.clone() };
    let mut prior = FinalPriorMappings::default();
    let mut sink = FinalMappingSink { fail_record: !fail_finish, fail_finish, ..Default::default() };
    assert_eq!(
      final_authority_error(final_authority_request(&fixture, &namespace, &mut inventory, &mut prior, &mut sink, &cancellation)).code(),
      "migration_final_authority_root_sink"
    );
    assert!(sink.closure.is_none());
  }
  assert_eq!(fixture.destination.publisher().observe().unwrap().selected.header, before);
  freeze.validate_unchanged().unwrap();
}

#[test]
fn final_authority_memory_bound_charges_excess_owned_seed_capacity() {
  let fixture = ProjectionFixture::new();
  let cancellation = CancellationToken::new();
  let freeze = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
    permit: &fixture.permit,
    source: &fixture.source,
    cancellation: &cancellation,
    acquisition_timeout: Duration::from_secs(2),
  })
  .unwrap();
  let namespace = execute_final_namespace_reconciliation_v1(final_reconciliation_request(
    &fixture,
    &freeze,
    &cancellation,
    &fixture.basis_root,
    &fixture.current_destination_tree_root,
  ))
  .unwrap();
  let mut rows = final_head_rows(&freeze);
  let mut oversized_identity = Vec::with_capacity(1 << 20);
  oversized_identity.clear();
  rows[0].authority_identity = oversized_identity;
  let counts = MigrationFinalAuthoritySeedCountsV1 { current_heads: 1, ..Default::default() };
  let closure = final_inventory_closure(&fixture, &freeze, &rows, counts);
  let mut inventory = FinalInventoryStream { seeds: rows.into(), closure };
  let mut prior = FinalPriorMappings::default();
  let mut sink = FinalMappingSink::default();
  let mut request = final_authority_request(&fixture, &namespace, &mut inventory, &mut prior, &mut sink, &cancellation);
  request.maximum_memory_bytes = 4 << 10;

  assert_eq!(final_authority_error(request).code(), "migration_final_authority_memory_limit");
  assert!(sink.closure.is_none());
}

#[test]
fn final_authority_memory_bound_charges_excess_prior_mapping_capacity() {
  let fixture = ProjectionFixture::new();
  let cancellation = CancellationToken::new();
  let freeze = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
    permit: &fixture.permit,
    source: &fixture.source,
    cancellation: &cancellation,
    acquisition_timeout: Duration::from_secs(2),
  })
  .unwrap();
  let namespace = execute_final_namespace_reconciliation_v1(final_reconciliation_request(
    &fixture,
    &freeze,
    &cancellation,
    &fixture.basis_root,
    &fixture.current_destination_tree_root,
  ))
  .unwrap();
  let mut rows = final_head_rows(&freeze);
  rows.push(MigrationFinalAuthoritySeedV1 {
    authority_identity: b"snapshot-1".to_vec(),
    source_write_sequence: 1,
    system_family_id: None,
    logical_bytes: 17,
    seed: MigrationBaseCloneSeedV1 {
      kind: MigrationBaseCloneSeedKindV1::Snapshot,
      path: "/".to_string(),
      entry_type: EntryType::DirectoryIndex,
      hash: fixture.basis_root.clone(),
    },
  });
  let counts = MigrationFinalAuthoritySeedCountsV1 { current_heads: 1, snapshots: 1, ..Default::default() };
  let closure = final_inventory_closure(&fixture, &freeze, &rows, counts);
  let mut oversized_mapping = Vec::with_capacity(1 << 20);
  oversized_mapping.extend_from_slice(&fixture.basis_root);
  let mut inventory = FinalInventoryStream { seeds: rows.into(), closure };
  let mut prior = OneShotFinalPriorMapping { destination: Some(oversized_mapping) };
  let mut sink = FinalMappingSink::default();
  let mut request = final_authority_request(&fixture, &namespace, &mut inventory, &mut prior, &mut sink, &cancellation);
  request.maximum_memory_bytes = 4 << 10;

  assert_eq!(final_authority_error(request).code(), "migration_final_authority_memory_limit");
  assert!(sink.closure.is_none());
}

#[test]
fn final_authority_memory_bound_charges_excess_inventory_closure_capacity() {
  let fixture = ProjectionFixture::new();
  let cancellation = CancellationToken::new();
  let freeze = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
    permit: &fixture.permit,
    source: &fixture.source,
    cancellation: &cancellation,
    acquisition_timeout: Duration::from_secs(2),
  })
  .unwrap();
  let namespace = execute_final_namespace_reconciliation_v1(final_reconciliation_request(
    &fixture,
    &freeze,
    &cancellation,
    &fixture.basis_root,
    &fixture.current_destination_tree_root,
  ))
  .unwrap();
  let rows = final_head_rows(&freeze);
  let counts = MigrationFinalAuthoritySeedCountsV1 { current_heads: 1, ..Default::default() };
  let mut closure = final_inventory_closure(&fixture, &freeze, &rows, counts);
  let fingerprint = closure.system_family_registry_fingerprint.clone();
  let mut oversized_fingerprint = Vec::with_capacity(1 << 20);
  oversized_fingerprint.extend_from_slice(&fingerprint);
  closure.system_family_registry_fingerprint = oversized_fingerprint;
  let mut inventory = MovingFinalInventory { seeds: rows.into(), closure: Some(closure) };
  let mut prior = FinalPriorMappings::default();
  let mut sink = FinalMappingSink::default();
  let mut request = final_authority_request(&fixture, &namespace, &mut inventory, &mut prior, &mut sink, &cancellation);
  request.maximum_memory_bytes = 4 << 10;

  assert_eq!(final_authority_error(request).code(), "migration_final_authority_memory_limit");
  assert!(sink.closure.is_none());
}

#[test]
fn final_authority_detects_a_mapping_sink_that_moves_destination_authority() {
  let fixture = ProjectionFixture::new();
  let cancellation = CancellationToken::new();
  let before_source = fs::read(&fixture.source_path).unwrap();
  let freeze = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
    permit: &fixture.permit,
    source: &fixture.source,
    cancellation: &cancellation,
    acquisition_timeout: Duration::from_secs(2),
  })
  .unwrap();
  let namespace = execute_final_namespace_reconciliation_v1(final_reconciliation_request(
    &fixture,
    &freeze,
    &cancellation,
    &fixture.basis_root,
    &fixture.current_destination_tree_root,
  ))
  .unwrap();
  let rows = final_head_rows(&freeze);
  let counts = MigrationFinalAuthoritySeedCountsV1 { current_heads: 1, ..Default::default() };
  let closure = final_inventory_closure(&fixture, &freeze, &rows, counts);
  let before = fixture.destination.publisher().observe().unwrap().selected.header.clone();
  let mut inventory = FinalInventoryStream { seeds: rows.into(), closure };
  let mut prior = FinalPriorMappings::default();
  let mut sink = MutatingFinalMappingSink {
    publisher: fixture.destination.shared_publisher(),
    database_id: fixture.permit.database_id(),
    rows: Vec::new(),
  };

  let error = final_authority_error(final_authority_request(&fixture, &namespace, &mut inventory, &mut prior, &mut sink, &cancellation));
  assert_eq!(error.code(), "migration_final_authority_sink_mutation");
  let after = fixture.destination.publisher().observe().unwrap().selected.header.clone();
  assert!(after.slot_sequence > before.slot_sequence);
  assert_eq!(after.head_hash, before.head_hash);
  assert_eq!(fs::read(&fixture.source_path).unwrap(), before_source);
  freeze.validate_unchanged().unwrap();
}

#[test]
fn final_authority_cancellation_at_stream_and_sink_closure_never_issues_a_proof() {
  let fixture = ProjectionFixture::new();
  let freeze_cancellation = CancellationToken::new();
  let freeze = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
    permit: &fixture.permit,
    source: &fixture.source,
    cancellation: &freeze_cancellation,
    acquisition_timeout: Duration::from_secs(2),
  })
  .unwrap();
  let namespace = execute_final_namespace_reconciliation_v1(final_reconciliation_request(
    &fixture,
    &freeze,
    &freeze_cancellation,
    &fixture.basis_root,
    &fixture.current_destination_tree_root,
  ))
  .unwrap();
  let rows = final_head_rows(&freeze);
  let counts = MigrationFinalAuthoritySeedCountsV1 { current_heads: 1, ..Default::default() };
  let closure = final_inventory_closure(&fixture, &freeze, &rows, counts);

  let cancellation = CancellationToken::new();
  let mut inventory =
    CancellingFinalInventory { head: Some(rows[0].clone()), closure: closure.clone(), cancellation: cancellation.clone() };
  let mut prior = FinalPriorMappings::default();
  let mut sink = FinalMappingSink::default();
  let error = final_authority_error(final_authority_request(&fixture, &namespace, &mut inventory, &mut prior, &mut sink, &cancellation));
  assert_eq!(error.code(), "migration_final_authority_cancelled");
  assert!(sink.closure.is_none());

  let cancellation = CancellationToken::new();
  let mut inventory = FinalInventoryStream { seeds: rows.into(), closure };
  let mut prior = FinalPriorMappings::default();
  let mut sink = CancellingFinalMappingSink { cancellation: cancellation.clone() };
  let error = final_authority_error(final_authority_request(&fixture, &namespace, &mut inventory, &mut prior, &mut sink, &cancellation));
  assert_eq!(error.code(), "migration_final_authority_cancelled");
  freeze.validate_unchanged().unwrap();
}
