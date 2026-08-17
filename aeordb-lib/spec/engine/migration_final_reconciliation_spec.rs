use std::collections::HashMap;
use std::fs::{self, File};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use aeordb::engine::btree::{BTREE_CONVERSION_THRESHOLD, BTreeNode, InternalNode, LeafNode, btree_plan_from_entries};
use aeordb::engine::directory_entry::{ChildEntry, deserialize_child_entries, serialize_child_entries};
use aeordb::engine::file_header::read_active_header;
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
use aeordb::engine::v4::migration_base_clone_execution::MigrationBaseCloneEntrySourceV1;
use aeordb::engine::v4::migration_capture_replay::MigrationCaptureReplayAuthorityTemplateV1;
use aeordb::engine::v4::migration_destination::{
  InitializedMigrationDestinationV1, MigrationDestinationInitializationRequestV1, initialize_migration_destination_v1,
  observe_migration_destination_path_v1,
};
use aeordb::engine::v4::migration_final_reconciliation::{
  MigrationFinalNamespaceReconciliationRequestV1, MigrationMerkleChangeKindV1, MigrationMerkleChangeV1, MigrationMerkleDiffRequestV1,
  MigrationMerkleDiffSinkV1, MigrationSourceWriteFreezeRequestV1, MigrationSourceWriteFreezeV1, acquire_migration_source_write_freeze_v1,
  execute_final_namespace_reconciliation_v1, stream_strict_migration_merkle_diff_v1,
};
use aeordb::engine::v4::migration_preflight::{
  AuthorityInventoryCountsV1, CapacityRoleV1, MigrationBinaryEvidenceV1, MigrationCapacityObservationV1, MigrationConfigurationEvidenceV1,
  MigrationIdentityEvidenceV1, MigrationMemoryEvidenceV1, MigrationNativeEvidenceV1, MigrationPreflightPermitV1,
  MigrationPreflightRequestV1, MigrationRecoveryEvidenceV1, MigrationSourceEvidenceV1, NativeCutoverCapabilitiesV1,
  SourceAuthorityInventoryV1, StrictVerificationEvidenceV1, StrictVerificationStateV1, admit_migration_preflight_v1,
};
use aeordb::engine::v4::namespace::{
  NamespaceRootWriteV1, SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, encode_namespace_root,
  encode_semantic_state_object, decode_namespace_root,
};
use aeordb::engine::v4::system_family::embedded_system_family_registry;
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
