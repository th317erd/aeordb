use std::collections::HashMap;

use aeordb::engine::btree::{BTreeNode, btree_plan_from_entries};
use aeordb::engine::directory_entry::{ChildEntry, serialize_child_entries};
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy, MemoryPressure};
use aeordb::engine::native_durability::PlatformFileIdentityDescriptorV1;
use aeordb::engine::storage_engine::EntryData;
use aeordb::engine::v4::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use aeordb::engine::v4::contract_generated::CONTRACT_REGISTRY_SHA256;
use aeordb::engine::v4::entity::EntryTypeV4;
use aeordb::engine::v4::first_authority::{ImmutableEntityBatchPublicationRequestV1, ImmutableEntityWriteV1, V4FirstAuthorityPublisher};
use aeordb::engine::v4::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_task::{
  JournalOwnerKindV1, MutationJournalWriteV1, MutationKindV1, MutationRecordWriteV1, MutationSideWriteV1, encode_mutation_journal,
};
use aeordb::engine::v4::migration_base_clone_execution::MigrationBaseCloneEntrySourceV1;
use aeordb::engine::v4::migration_capture_replay::{
  MigrationCaptureReplayAuthorityTemplateV1, MigrationCaptureReplayRequestV1, MigrationCaptureReplayRootSinkV1,
  execute_selected_migration_capture_replay_v1,
};
use aeordb::engine::v4::migration_capture_workspace::{
  DurableMigrationCaptureWorkspaceV1, MigrationCaptureWorkspaceBasisV1, MigrationCaptureWorkspaceIdentityV1,
  MigrationCaptureWorkspaceOptionsV1, MigrationCaptureWorkspaceReopenOptionsV1, ReopenedMigrationCaptureWorkspaceV1,
};
use aeordb::engine::v4::migration_destination::{
  InitializedMigrationDestinationV1, MigrationDestinationInitializationRequestV1, MigrationDestinationPathObservationV1,
  initialize_migration_destination_v1, observe_migration_destination_path_v1,
};
use aeordb::engine::v4::migration_owner::{
  MigrationAcquisitionRequestV1, MigrationCaptureCheckpointPublicationRequestV1, MigrationStateOwnerV1,
};
use aeordb::engine::v4::migration_preflight::{
  AuthorityInventoryCountsV1, CapacityRoleV1, MigrationBinaryEvidenceV1, MigrationCapacityObservationV1, MigrationConfigurationEvidenceV1,
  MigrationIdentityEvidenceV1, MigrationMemoryEvidenceV1, MigrationNativeEvidenceV1, MigrationPreflightPermitV1,
  MigrationPreflightRequestV1, MigrationRecoveryEvidenceV1, MigrationSourceEvidenceV1, NativeCutoverCapabilitiesV1,
  SourceAuthorityInventoryV1, StrictVerificationEvidenceV1, StrictVerificationStateV1, admit_migration_preflight_v1,
};
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, encode_semantic_state_object};
use aeordb::engine::v4::system_family::embedded_system_family_registry;
use aeordb::engine::{CompressionAlgorithm, EngineError, EngineResult, EntryHeader, EntryType, FileRecord, HashAlgorithm};
use tokio_util::sync::CancellationToken;

const GIB: u64 = 1024 * 1024 * 1024;
const DATABASE_ID: [u8; 16] = [0x11; 16];
const MIGRATION_ID: [u8; 16] = [0x22; 16];
const SOURCE_PHYSICAL_ID: [u8; 16] = [0x33; 16];
const DESTINATION_PHYSICAL_ID: [u8; 16] = [0x44; 16];
const HOLDER_BOOT_ID: [u8; 16] = [0x55; 16];
const RUNTIME_BOOT_ID: [u8; 16] = [0x66; 16];
const BASE_SEQUENCE: u64 = 10;
const REPLAY_SEQUENCE: u64 = 11;
const BASE_TIME: u64 = 1_700_000_000_000;

fn id(first: u8) -> [u8; 16] {
  std::array::from_fn(|offset| first.wrapping_add(offset as u8))
}

fn digest(first: u8) -> [u8; 32] {
  std::array::from_fn(|offset| first.wrapping_add(offset as u8))
}

fn identity(first: u8, volume: u8) -> PlatformFileIdentityDescriptorV1 {
  PlatformFileIdentityDescriptorV1 {
    platform: 1,
    schema: 1,
    flags: 1 << 1,
    volume_identity: id(volume),
    file_identity: id(first),
    birth_identity: id(first.wrapping_add(0x20)),
  }
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

fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(256 << 20, 512 << 20, 1, 16 << 20).unwrap())
}

fn counts() -> AuthorityInventoryCountsV1 {
  AuthorityInventoryCountsV1 {
    protected_families: 46,
    modules: 0,
    snapshots: 0,
    forks: 0,
    symlinks: 0,
    history_roots: 0,
    peers: 0,
    sync_states: 0,
    tasks: 0,
    plugins: 0,
    roots: 1,
  }
}

fn permit(
  algorithm: HashAlgorithm,
  source_identity: PlatformFileIdentityDescriptorV1,
  source_head: Vec<u8>,
  destination: &MigrationDestinationPathObservationV1,
) -> MigrationPreflightPermitV1 {
  let baseline = CapabilitySetV1::v4_baseline();
  let registry = embedded_system_family_registry(algorithm).unwrap();
  let checksum = digest(0x70);
  let mut capacities = std::array::from_fn(|index| {
    let role = [CapacityRoleV1::Destination, CapacityRoleV1::Workspace, CapacityRoleV1::Backup, CapacityRoleV1::Capture][index];
    MigrationCapacityObservationV1 {
      role,
      volume_identity: id(0x80 + index as u8),
      path_identity: identity(0x90 + index as u8, 0x80 + index as u8),
      filesystem_capacity_bytes: 256 * GIB,
      available_bytes: 192 * GIB,
      required_bytes: if role == CapacityRoleV1::Capture { 64 * GIB } else { 4 * GIB },
      minimum_remaining_bytes: 16 * GIB,
    }
  });
  capacities[0].volume_identity = destination.parent_identity().volume_identity;
  capacities[0].path_identity = destination.parent_identity();
  let request = MigrationPreflightRequestV1 {
    identity: MigrationIdentityEvidenceV1 {
      database_id: DATABASE_ID,
      migration_id: MIGRATION_ID,
      source_physical_instance_id: SOURCE_PHYSICAL_ID,
      destination_physical_instance_id: DESTINATION_PHYSICAL_ID,
      source_path_digest: digest(0x10),
      destination_path_digest: destination.path_digest(),
      source_file_identity: source_identity,
      destination_parent_identity: destination.parent_identity(),
    },
    source: MigrationSourceEvidenceV1 {
      hash_algorithm: algorithm,
      file_size: 4 * GIB,
      complete_file_checksum: checksum,
      selected_header_slot: 1,
      selected_header_sequence: 41,
      selected_header_digest: digest(0x80),
      head_hash: source_head,
    },
    verification: StrictVerificationEvidenceV1 {
      state: StrictVerificationStateV1::CompleteClean,
      source_file_size: 4 * GIB,
      source_header_sequence: 41,
      source_complete_file_checksum: checksum,
      issue_count: 0,
      evidence_digest: digest(0xa0),
    },
    recovery: MigrationRecoveryEvidenceV1 {
      inspection_complete: true,
      source_header_sequence: 41,
      durability_latched: false,
      repair_active: false,
      external_spill_count: 0,
      repair_ticket_count: 0,
      path_latch_count: 0,
      evidence_digest: digest(0xb0),
    },
    inventory: SourceAuthorityInventoryV1 {
      complete: true,
      source_header_sequence: 41,
      unresolved_family_count: 0,
      counts: counts(),
      authority_digest: digest(0xc0),
      system_family_registry_fingerprint: registry.operational_fingerprint.clone(),
    },
    capacity: capacities,
    native: MigrationNativeEvidenceV1 { source: native(0xd0), destination: native(0xe0) },
    memory: MigrationMemoryEvidenceV1 {
      source_budget_bytes: GIB,
      destination_budget_bytes: 2 * GIB,
      coordinator_accounted_bytes: GIB,
      coordinator_ordinary_limit_bytes: 12 * GIB,
      host_available_bytes: 12 * GIB,
      host_available_floor_bytes: GIB,
      pressure: MemoryPressure::Normal,
      evidence_digest: digest(0xf0),
    },
    configuration: MigrationConfigurationEvidenceV1 {
      generation: 7,
      capture_max_bytes: 64 * GIB,
      capture_free_reserve_bytes: 16 * GIB,
      checkpoint_after_seconds: 300,
      effective_configuration_fingerprint: vec![0x17; algorithm.hash_length()],
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

#[derive(Clone)]
struct FakeSource {
  algorithm: HashAlgorithm,
  identity: PlatformFileIdentityDescriptorV1,
  entries: HashMap<Vec<u8>, EntryData>,
}

impl FakeSource {
  fn new(algorithm: HashAlgorithm, identity: PlatformFileIdentityDescriptorV1) -> Self {
    Self { algorithm, identity, entries: HashMap::new() }
  }

  fn insert(&mut self, entry_type: EntryType, entry_version: u8, key: Vec<u8>, value: Vec<u8>) {
    let total_length = EntryHeader::compute_total_length(self.algorithm, key.len(), value.len()).unwrap();
    let header = EntryHeader {
      entry_version,
      entry_type,
      flags: 0,
      hash_algo: self.algorithm,
      compression_algo: CompressionAlgorithm::None,
      encryption_algo: 0,
      key_length: key.len() as u32,
      value_length: value.len() as u32,
      timestamp: BASE_TIME as i64,
      total_length,
      hash: digest_parts(self.algorithm, &[b"capture replay source", &key, &value]),
    };
    self.entries.insert(key.clone(), (header, key, value));
  }
}

impl MigrationBaseCloneEntrySourceV1 for FakeSource {
  fn hash_algorithm(&self) -> HashAlgorithm {
    self.algorithm
  }

  fn physical_identity(&self) -> EngineResult<PlatformFileIdentityDescriptorV1> {
    Ok(self.identity)
  }

  fn historical_entry_header(&self, hash: &[u8]) -> EngineResult<Option<EntryHeader>> {
    Ok(self.entries.get(hash).map(|entry| entry.0.clone()))
  }

  fn historical_entry_verified_bounded(&self, hash: &[u8], maximum_value_length: u32) -> EngineResult<Option<EntryData>> {
    let Some((header, key, value)) = self.entries.get(hash) else {
      return Ok(None);
    };
    if header.value_length > maximum_value_length {
      return Err(EngineError::ResourceExhausted("fake source bounded read refused".to_string()));
    }
    Ok(Some((header.clone(), key.clone(), value.clone())))
  }
}

type SourceFingerprint = Vec<(Vec<u8>, u8, u8, u8, u32, u32, i64, u32, Vec<u8>, Vec<u8>, Vec<u8>)>;

fn source_fingerprint(source: &FakeSource) -> SourceFingerprint {
  let mut entries = source
    .entries
    .iter()
    .map(|(map_key, (header, key, value))| {
      (
        map_key.clone(),
        header.entry_version,
        header.entry_type.to_u8(),
        header.flags,
        header.key_length,
        header.value_length,
        header.timestamp,
        header.total_length,
        header.hash.clone(),
        key.clone(),
        value.clone(),
      )
    })
    .collect::<Vec<_>>();
  entries.sort_by(|left, right| left.0.cmp(&right.0));
  entries
}

fn destination_btree_lookup(publisher: &V4FirstAuthorityPublisher, root_hash: &[u8], name: &str, hash_width: usize) -> Option<ChildEntry> {
  let mut current = root_hash.to_vec();
  for _ in 0..128 {
    let entity = publisher.load_immutable_entity_bounded(&current, 64 << 20).unwrap().unwrap();
    match BTreeNode::deserialize(&entity.stored_value, hash_width, entity.entity_version).unwrap() {
      BTreeNode::Leaf(leaf) => return leaf.find(name).cloned(),
      BTreeNode::Internal(internal) => current = internal.children[internal.find_child_index(name)].clone(),
    }
  }
  panic!("destination B-tree lookup exceeded the depth bound")
}

fn file_entity_at(algorithm: HashAlgorithm, path: &str, bytes: &[u8], updated_at: i64) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
  let chunk = digest_parts(algorithm, &[b"chunk:", bytes]);
  let mut record = FileRecord::new(path.to_string(), Some("text/plain".to_string()), bytes.len() as u64, vec![chunk.clone()]);
  record.created_at = BASE_TIME as i64;
  record.updated_at = updated_at;
  record.content_hash = digest_parts(algorithm, &[bytes]);
  let value = record.serialize(algorithm.hash_length()).unwrap();
  let key = digest_parts(algorithm, &[b"filec:", &value]);
  (chunk, key, value)
}

fn file_entity(algorithm: HashAlgorithm, bytes: &[u8], updated_at: i64) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
  file_entity_at(algorithm, "/a.txt", bytes, updated_at)
}

fn child(name: &str, entry_type: EntryType, hash: Vec<u8>, size: u64, updated_at: i64) -> ChildEntry {
  ChildEntry {
    entry_type: entry_type.to_u8(),
    hash,
    total_size: size,
    created_at: BASE_TIME as i64,
    updated_at,
    name: name.to_string(),
    content_type: (entry_type == EntryType::FileRecord).then(|| "text/plain".to_string()),
    virtual_time: 1,
    node_id: 1,
  }
}

fn directory_value(entries: &[ChildEntry], algorithm: HashAlgorithm) -> (Vec<u8>, Vec<u8>) {
  let value = serialize_child_entries(entries, algorithm.hash_length()).unwrap();
  let key = digest_parts(algorithm, &[b"dirc:", &value]);
  (key, value)
}

fn root_value(file_hash: Vec<u8>, size: u64, updated_at: i64, algorithm: HashAlgorithm) -> (Vec<u8>, Vec<u8>) {
  directory_value(&[child("a.txt", EntryType::FileRecord, file_hash, size, updated_at)], algorithm)
}

struct EntityFixture {
  version: u8,
  entry_type: EntryTypeV4,
  key: Vec<u8>,
  value: Vec<u8>,
}

struct MutationFixture {
  kind: MutationKindV1,
  mutation_id: Vec<u8>,
  before: Option<(String, Vec<u8>)>,
  after: Option<(String, Vec<u8>)>,
  committed_at_ms: u64,
}

struct ReplayGraph {
  source: FakeSource,
  base_root: Vec<u8>,
  source_after_root: Vec<u8>,
  expected_destination_root: Vec<u8>,
  base_entities: Vec<EntityFixture>,
  mutations: Vec<MutationFixture>,
}

fn publish_base(destination: &V4FirstAuthorityPublisher, entities: &[EntityFixture]) {
  let writes = entities
    .iter()
    .map(|entity| ImmutableEntityWriteV1 {
      entity_version: entity.version,
      entry_type: entity.entry_type,
      flags: 0,
      key: &entity.key,
      stored_value: &entity.value,
    })
    .collect::<Vec<_>>();
  destination
    .publish_immutable_entity_batch(ImmutableEntityBatchPublicationRequestV1 {
      database_id: &DATABASE_ID,
      entities: &writes,
      publication_timestamp_ms: BASE_TIME + 200,
    })
    .unwrap();
}

fn mutation_fixture(
  algorithm: HashAlgorithm,
  label: &[u8],
  kind: MutationKindV1,
  before: Option<(&str, &[u8])>,
  after: Option<(&str, &[u8])>,
) -> MutationFixture {
  MutationFixture {
    kind,
    mutation_id: digest_parts(algorithm, &[b"capture replay mutation", label]),
    before: before.map(|(path, revision)| (path.to_string(), revision.to_vec())),
    after: after.map(|(path, revision)| (path.to_string(), revision.to_vec())),
    committed_at_ms: BASE_TIME + 500,
  }
}

fn flat_graph(algorithm: HashAlgorithm, source_identity: PlatformFileIdentityDescriptorV1, scenario: CaptureScenario) -> ReplayGraph {
  let (old_chunk, old_file, old_file_value) = file_entity(algorithm, b"old", BASE_TIME as i64 + 1);
  let (new_chunk, new_file, new_file_value) = file_entity(algorithm, b"new", BASE_TIME as i64 + 2);
  let (base_root, base_value) = root_value(old_file.clone(), 3, BASE_TIME as i64 + 1, algorithm);
  let (after_root, after_value) = root_value(new_file.clone(), 3, BASE_TIME as i64 + 2, algorithm);
  let mut source = FakeSource::new(algorithm, source_identity);
  for (entry_type, version, key, value) in [
    (EntryType::Chunk, 0, old_chunk.clone(), b"old".to_vec()),
    (EntryType::Chunk, 0, new_chunk.clone(), b"new".to_vec()),
    (EntryType::FileRecord, 1, old_file.clone(), old_file_value.clone()),
    (EntryType::FileRecord, 1, new_file.clone(), new_file_value.clone()),
    (EntryType::DirectoryIndex, 0, base_root.clone(), base_value.clone()),
    (EntryType::DirectoryIndex, 0, after_root.clone(), after_value),
  ] {
    source.insert(entry_type, version, key, value);
  }
  let update = || {
    mutation_fixture(algorithm, b"a", MutationKindV1::Update, Some(("/a.txt", old_file.as_slice())), Some(("/a.txt", new_file.as_slice())))
  };
  let mut mutations = match scenario {
    CaptureScenario::Update | CaptureScenario::BasisMismatch => vec![update()],
    CaptureScenario::Transition => vec![mutation_fixture(
      algorithm,
      b"transition",
      MutationKindV1::Transition,
      Some(("/a.txt", old_file.as_slice())),
      Some(("/a.txt", new_file.as_slice())),
    )],
    CaptureScenario::NoPublications => Vec::new(),
    CaptureScenario::TwoBatchesOnePublication => vec![
      update(),
      mutation_fixture(
        algorithm,
        b"b",
        MutationKindV1::Update,
        Some(("/a.txt", old_file.as_slice())),
        Some(("/a.txt", new_file.as_slice())),
      ),
    ],
    CaptureScenario::SamePublicationTimeDivergence => {
      let mut second = mutation_fixture(
        algorithm,
        b"b",
        MutationKindV1::Update,
        Some(("/a.txt", old_file.as_slice())),
        Some(("/a.txt", new_file.as_slice())),
      );
      second.committed_at_ms += 1;
      vec![update(), second]
    }
    CaptureScenario::CreateOverExistingPath => {
      vec![mutation_fixture(algorithm, b"a", MutationKindV1::Create, None, Some(("/a.txt", new_file.as_slice())))]
    }
    CaptureScenario::DeleteLeavingPathPresent => {
      vec![mutation_fixture(algorithm, b"a", MutationKindV1::Delete, Some(("/a.txt", old_file.as_slice())), None)]
    }
    _ => unreachable!("non-flat replay scenario"),
  };
  mutations.sort_by(|left, right| left.mutation_id.cmp(&right.mutation_id));
  let mut graph = ReplayGraph {
    source,
    base_root: base_root.clone(),
    source_after_root: after_root.clone(),
    expected_destination_root: after_root,
    base_entities: vec![
      EntityFixture { version: 0, entry_type: EntryTypeV4::Chunk, key: old_chunk, value: b"old".to_vec() },
      EntityFixture { version: 1, entry_type: EntryTypeV4::FileRecord, key: old_file, value: old_file_value },
      EntityFixture { version: 0, entry_type: EntryTypeV4::DirectoryIndex, key: base_root, value: base_value },
    ],
    mutations,
  };
  if matches!(scenario, CaptureScenario::NoPublications) {
    graph.source_after_root = graph.base_root.clone();
    graph.expected_destination_root = graph.base_root.clone();
  }
  graph
}

fn nested_graph(algorithm: HashAlgorithm, source_identity: PlatformFileIdentityDescriptorV1) -> ReplayGraph {
  let (old_chunk, old_file, old_file_value) = file_entity_at(algorithm, "/nested/a.txt", b"old", BASE_TIME as i64 + 1);
  let (new_chunk, new_file, new_file_value) = file_entity_at(algorithm, "/nested/a.txt", b"new", BASE_TIME as i64 + 2);
  let (base_nested, base_nested_value) = root_value(old_file.clone(), 3, BASE_TIME as i64 + 1, algorithm);
  let (after_nested, after_nested_value) = root_value(new_file.clone(), 3, BASE_TIME as i64 + 2, algorithm);
  let (base_root, base_value) = directory_value(
    &[child("nested", EntryType::DirectoryIndex, base_nested.clone(), base_nested_value.len() as u64, BASE_TIME as i64 + 1)],
    algorithm,
  );
  let (after_root, after_value) = directory_value(
    &[child("nested", EntryType::DirectoryIndex, after_nested.clone(), after_nested_value.len() as u64, BASE_TIME as i64 + 2)],
    algorithm,
  );
  let mut source = FakeSource::new(algorithm, source_identity);
  for (entry_type, version, key, value) in [
    (EntryType::Chunk, 0, old_chunk.clone(), b"old".to_vec()),
    (EntryType::Chunk, 0, new_chunk, b"new".to_vec()),
    (EntryType::FileRecord, 1, old_file.clone(), old_file_value.clone()),
    (EntryType::FileRecord, 1, new_file.clone(), new_file_value),
    (EntryType::DirectoryIndex, 0, base_nested.clone(), base_nested_value.clone()),
    (EntryType::DirectoryIndex, 0, after_nested, after_nested_value),
    (EntryType::DirectoryIndex, 0, base_root.clone(), base_value.clone()),
    (EntryType::DirectoryIndex, 0, after_root.clone(), after_value),
  ] {
    source.insert(entry_type, version, key, value);
  }
  ReplayGraph {
    source,
    base_root: base_root.clone(),
    source_after_root: after_root.clone(),
    expected_destination_root: after_root,
    base_entities: vec![
      EntityFixture { version: 0, entry_type: EntryTypeV4::Chunk, key: old_chunk, value: b"old".to_vec() },
      EntityFixture { version: 1, entry_type: EntryTypeV4::FileRecord, key: old_file.clone(), value: old_file_value },
      EntityFixture { version: 0, entry_type: EntryTypeV4::DirectoryIndex, key: base_nested, value: base_nested_value },
      EntityFixture { version: 0, entry_type: EntryTypeV4::DirectoryIndex, key: base_root, value: base_value },
    ],
    mutations: vec![mutation_fixture(
      algorithm,
      b"nested",
      MutationKindV1::Update,
      Some(("/nested/a.txt", old_file.as_slice())),
      Some(("/nested/a.txt", new_file.as_slice())),
    )],
  }
}

fn btree_graph(algorithm: HashAlgorithm, source_identity: PlatformFileIdentityDescriptorV1) -> ReplayGraph {
  let target = "file-0128.txt";
  let path = format!("/{target}");
  let (old_chunk, old_file, old_file_value) = file_entity_at(algorithm, &path, b"old", BASE_TIME as i64 + 1);
  let (new_chunk, new_file, new_file_value) = file_entity_at(algorithm, &path, b"new", BASE_TIME as i64 + 2);
  let mut base_entries = (0..=256)
    .map(|index| child(&format!("file-{index:04}.txt"), EntryType::FileRecord, old_file.clone(), 3, BASE_TIME as i64 + 1))
    .collect::<Vec<_>>();
  let mut after_entries = base_entries.clone();
  let target_index = base_entries.binary_search_by(|entry| entry.name.as_str().cmp(target)).unwrap();
  after_entries[target_index] = child(target, EntryType::FileRecord, new_file.clone(), 3, BASE_TIME as i64 + 2);
  let base_plan = btree_plan_from_entries(std::mem::take(&mut base_entries), algorithm.hash_length(), &algorithm).unwrap();
  let after_plan = btree_plan_from_entries(after_entries, algorithm.hash_length(), &algorithm).unwrap();
  let base_root = base_plan.root_hash().to_vec();
  let after_root = after_plan.root_hash().to_vec();
  let mut source = FakeSource::new(algorithm, source_identity);
  source.insert(EntryType::Chunk, 0, old_chunk.clone(), b"old".to_vec());
  source.insert(EntryType::Chunk, 0, new_chunk, b"new".to_vec());
  source.insert(EntryType::FileRecord, 1, old_file.clone(), old_file_value.clone());
  source.insert(EntryType::FileRecord, 1, new_file.clone(), new_file_value);
  for write in base_plan.node_writes().chain(after_plan.node_writes()) {
    source.insert(EntryType::DirectoryIndex, 0, write.key.clone(), write.value.clone());
  }
  let mut base_entities = vec![
    EntityFixture { version: 0, entry_type: EntryTypeV4::Chunk, key: old_chunk, value: b"old".to_vec() },
    EntityFixture { version: 1, entry_type: EntryTypeV4::FileRecord, key: old_file.clone(), value: old_file_value },
  ];
  base_entities.extend(base_plan.node_writes().map(|write| EntityFixture {
    version: 0,
    entry_type: EntryTypeV4::DirectoryIndex,
    key: write.key.clone(),
    value: write.value.clone(),
  }));
  ReplayGraph {
    source,
    base_root,
    source_after_root: after_root.clone(),
    expected_destination_root: after_root,
    base_entities,
    mutations: vec![mutation_fixture(
      algorithm,
      b"btree",
      MutationKindV1::Update,
      Some((&path, old_file.as_slice())),
      Some((&path, new_file.as_slice())),
    )],
  }
}

fn omitted_system_graph(algorithm: HashAlgorithm, source_identity: PlatformFileIdentityDescriptorV1) -> ReplayGraph {
  let (base_root, base_value) = directory_value(&[], algorithm);
  let (after_root, after_value) = directory_value(
    &[child(".aeordb-indexes", EntryType::DirectoryIndex, base_root.clone(), base_value.len() as u64, BASE_TIME as i64 + 2)],
    algorithm,
  );
  let mut source = FakeSource::new(algorithm, source_identity);
  source.insert(EntryType::DirectoryIndex, 0, base_root.clone(), base_value.clone());
  source.insert(EntryType::DirectoryIndex, 0, after_root.clone(), after_value);
  ReplayGraph {
    source,
    base_root: base_root.clone(),
    source_after_root: after_root,
    expected_destination_root: base_root.clone(),
    base_entities: vec![EntityFixture { version: 0, entry_type: EntryTypeV4::DirectoryIndex, key: base_root.clone(), value: base_value }],
    mutations: vec![mutation_fixture(
      algorithm,
      b"omitted system family",
      MutationKindV1::Create,
      None,
      Some(("/.aeordb-indexes", base_root.as_slice())),
    )],
  }
}

fn create_or_delete_graph(
  algorithm: HashAlgorithm,
  source_identity: PlatformFileIdentityDescriptorV1,
  kind: MutationKindV1,
) -> ReplayGraph {
  let (chunk, file, file_value) = file_entity(algorithm, b"new", BASE_TIME as i64 + 2);
  let (empty_root, empty_value) = directory_value(&[], algorithm);
  let (file_root, file_root_value) = root_value(file.clone(), 3, BASE_TIME as i64 + 2, algorithm);
  let deleting = kind == MutationKindV1::Delete;
  let (base_root, base_value, after_root) = if deleting {
    (file_root.clone(), file_root_value.clone(), empty_root.clone())
  } else {
    (empty_root.clone(), empty_value.clone(), file_root.clone())
  };
  let mut source = FakeSource::new(algorithm, source_identity);
  source.insert(EntryType::Chunk, 0, chunk.clone(), b"new".to_vec());
  source.insert(EntryType::FileRecord, 1, file.clone(), file_value.clone());
  source.insert(EntryType::DirectoryIndex, 0, empty_root, empty_value);
  source.insert(EntryType::DirectoryIndex, 0, file_root, file_root_value);
  let mut base_entities =
    vec![EntityFixture { version: 0, entry_type: EntryTypeV4::DirectoryIndex, key: base_root.clone(), value: base_value }];
  if deleting {
    base_entities.insert(0, EntityFixture { version: 0, entry_type: EntryTypeV4::Chunk, key: chunk, value: b"new".to_vec() });
    base_entities.insert(1, EntityFixture { version: 1, entry_type: EntryTypeV4::FileRecord, key: file.clone(), value: file_value });
  }
  ReplayGraph {
    source,
    base_root,
    source_after_root: after_root.clone(),
    expected_destination_root: after_root,
    base_entities,
    mutations: vec![mutation_fixture(
      algorithm,
      b"create or delete",
      kind,
      deleting.then_some(("/a.txt", file.as_slice())),
      (!deleting).then_some(("/a.txt", file.as_slice())),
    )],
  }
}

fn move_graph(algorithm: HashAlgorithm, source_identity: PlatformFileIdentityDescriptorV1) -> ReplayGraph {
  let (chunk, old_file, old_value) = file_entity_at(algorithm, "/a.txt", b"same", BASE_TIME as i64 + 1);
  let (_, new_file, new_value) = file_entity_at(algorithm, "/b.txt", b"same", BASE_TIME as i64 + 2);
  let (base_root, base_value) =
    directory_value(&[child("a.txt", EntryType::FileRecord, old_file.clone(), 4, BASE_TIME as i64 + 1)], algorithm);
  let (after_root, after_value) =
    directory_value(&[child("b.txt", EntryType::FileRecord, new_file.clone(), 4, BASE_TIME as i64 + 2)], algorithm);
  let mut source = FakeSource::new(algorithm, source_identity);
  source.insert(EntryType::Chunk, 0, chunk.clone(), b"same".to_vec());
  source.insert(EntryType::FileRecord, 1, old_file.clone(), old_value.clone());
  source.insert(EntryType::FileRecord, 1, new_file.clone(), new_value);
  source.insert(EntryType::DirectoryIndex, 0, base_root.clone(), base_value.clone());
  source.insert(EntryType::DirectoryIndex, 0, after_root.clone(), after_value);
  ReplayGraph {
    source,
    base_root: base_root.clone(),
    source_after_root: after_root.clone(),
    expected_destination_root: after_root,
    base_entities: vec![
      EntityFixture { version: 0, entry_type: EntryTypeV4::Chunk, key: chunk, value: b"same".to_vec() },
      EntityFixture { version: 1, entry_type: EntryTypeV4::FileRecord, key: old_file.clone(), value: old_value },
      EntityFixture { version: 0, entry_type: EntryTypeV4::DirectoryIndex, key: base_root, value: base_value },
    ],
    mutations: vec![mutation_fixture(
      algorithm,
      b"move",
      MutationKindV1::Move,
      Some(("/a.txt", old_file.as_slice())),
      Some(("/b.txt", new_file.as_slice())),
    )],
  }
}

fn retirement_owner(algorithm: HashAlgorithm, cancellation: &CancellationToken, memory: &MemoryCoordinator) -> RetirementJournalOwnerV1 {
  RetirementJournalOwnerV1::new_chain(
    algorithm,
    DATABASE_ID,
    1,
    901,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    cancellation,
    memory,
  )
  .unwrap()
}

#[derive(Default)]
struct RootSink {
  rows: RootMappings,
}

type RootMappings = Vec<(u64, Vec<u8>, Vec<u8>, Vec<u8>)>;

impl MigrationCaptureReplayRootSinkV1 for RootSink {
  fn record_root_mapping(&mut self, sequence: u64, source_root: &[u8], namespace_root: &[u8], tree_root: &[u8]) -> EngineResult<()> {
    let row = (sequence, source_root.to_vec(), namespace_root.to_vec(), tree_root.to_vec());
    if !self.rows.contains(&row) {
      self.rows.push(row);
    }
    Ok(())
  }
}

struct FailingRootSink {
  fail_on_call: usize,
  calls: usize,
}

impl MigrationCaptureReplayRootSinkV1 for FailingRootSink {
  fn record_root_mapping(&mut self, _sequence: u64, _source_root: &[u8], _namespace_root: &[u8], _tree_root: &[u8]) -> EngineResult<()> {
    self.calls += 1;
    if self.calls == self.fail_on_call {
      return Err(EngineError::ResourceExhausted("injected root-map sink failure".to_string()));
    }
    Ok(())
  }
}

struct Harness {
  _directory: tempfile::TempDir,
  permit: MigrationPreflightPermitV1,
  initialized: InitializedMigrationDestinationV1,
  source: FakeSource,
  capture: ReopenedMigrationCaptureWorkspaceV1,
  owner: MigrationStateOwnerV1,
  retirement: RetirementJournalOwnerV1,
  memory: MemoryCoordinator,
  cancellation: CancellationToken,
  authority: MigrationCaptureReplayAuthorityTemplateV1,
  base_root: Vec<u8>,
  after_root: Vec<u8>,
  expected_destination_root: Vec<u8>,
}

#[derive(Clone, Copy)]
enum CaptureScenario {
  Update,
  TwoBatchesOnePublication,
  SamePublicationTimeDivergence,
  CreateOverExistingPath,
  DeleteLeavingPathPresent,
  NestedUpdate,
  BTreeUpdate,
  OmittedSystemFamily,
  BasisMismatch,
  NoPublications,
  Create,
  Delete,
  Copy,
  Restore,
  Move,
  Transition,
}

impl Harness {
  fn new(algorithm: HashAlgorithm) -> Self {
    Self::new_with_scenario(algorithm, CaptureScenario::Update)
  }

  fn new_with_scenario(algorithm: HashAlgorithm, scenario: CaptureScenario) -> Self {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("replay.aeordb");
    let destination = observe_migration_destination_path_v1(&path).unwrap();
    let source_identity = identity(0x42, 0x24);
    let graph = match scenario {
      CaptureScenario::Update
      | CaptureScenario::TwoBatchesOnePublication
      | CaptureScenario::SamePublicationTimeDivergence
      | CaptureScenario::CreateOverExistingPath
      | CaptureScenario::DeleteLeavingPathPresent
      | CaptureScenario::BasisMismatch
      | CaptureScenario::NoPublications
      | CaptureScenario::Transition => flat_graph(algorithm, source_identity, scenario),
      CaptureScenario::NestedUpdate => nested_graph(algorithm, source_identity),
      CaptureScenario::BTreeUpdate => btree_graph(algorithm, source_identity),
      CaptureScenario::OmittedSystemFamily => omitted_system_graph(algorithm, source_identity),
      CaptureScenario::Create => create_or_delete_graph(algorithm, source_identity, MutationKindV1::Create),
      CaptureScenario::Delete => create_or_delete_graph(algorithm, source_identity, MutationKindV1::Delete),
      CaptureScenario::Copy => create_or_delete_graph(algorithm, source_identity, MutationKindV1::Copy),
      CaptureScenario::Restore => create_or_delete_graph(algorithm, source_identity, MutationKindV1::Restore),
      CaptureScenario::Move => move_graph(algorithm, source_identity),
    };
    let permit_source_head = if matches!(scenario, CaptureScenario::BasisMismatch) {
      digest_parts(algorithm, &[b"divergent preflight source head"])
    } else {
      graph.base_root.clone()
    };
    let permit = permit(algorithm, source_identity, permit_source_head, &destination);
    let initialized = initialize_migration_destination_v1(MigrationDestinationInitializationRequestV1 {
      permit: &permit,
      destination: &destination,
      created_at_ms: BASE_TIME + 100,
      writer_fence_epoch: 7,
      cancellation: &CancellationToken::new(),
    })
    .unwrap();
    publish_base(initialized.publisher(), &graph.base_entities);

    let memory = memory();
    let cancellation = CancellationToken::new();
    let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
    let (owner, _) = MigrationStateOwnerV1::acquire(
      initialized.shared_publisher(),
      permit.clone(),
      MigrationAcquisitionRequestV1 {
        holder_boot_id: HOLDER_BOOT_ID,
        acquired_at_ms: BASE_TIME as i64 + 300,
        lease_duration_ms: 60_000,
        publication_timestamp_ms: BASE_TIME + 300,
        monotonic_now_ms: 10_000,
      },
      &mut retirement,
    )
    .unwrap();

    let capture_identity = MigrationCaptureWorkspaceIdentityV1::new(
      DATABASE_ID,
      MIGRATION_ID,
      SOURCE_PHYSICAL_ID,
      DESTINATION_PHYSICAL_ID,
      RUNTIME_BOOT_ID,
      1,
      1,
      algorithm,
    )
    .unwrap();
    let basis = MigrationCaptureWorkspaceBasisV1::new(
      BASE_TIME as i64 + 400,
      BASE_SEQUENCE,
      graph.base_root.clone(),
      permit.effective_configuration_fingerprint().to_vec(),
      permit.system_family_registry_fingerprint().to_vec(),
      permit.source_authority_digest(),
    )
    .unwrap();
    let mut workspace = DurableMigrationCaptureWorkspaceV1::create(
      initialized.path(),
      capture_identity,
      basis.clone(),
      MigrationCaptureWorkspaceOptionsV1::new(Some(directory.path().to_path_buf()), 16 << 20, 0).unwrap(),
      cancellation.clone(),
      &memory,
    )
    .unwrap();
    let records = graph
      .mutations
      .iter()
      .map(|mutation| MutationRecordWriteV1 {
        kind: mutation.kind,
        sequence: REPLAY_SEQUENCE,
        mutation_id: &mutation.mutation_id,
        batch_ordinal: 0,
        batch_count: 1,
        root_before: &graph.base_root,
        root_after: &graph.source_after_root,
        before: mutation.before.as_ref().map(|(path, revision)| MutationSideWriteV1 { path, revision }),
        after: mutation.after.as_ref().map(|(path, revision)| MutationSideWriteV1 { path, revision }),
        committed_at_ms: mutation.committed_at_ms,
      })
      .collect::<Vec<_>>();
    let captured_sequence = if records.is_empty() { BASE_SEQUENCE } else { REPLAY_SEQUENCE };
    if !records.is_empty() {
      let zero = vec![0; algorithm.hash_length()];
      let segment = encode_mutation_journal(&MutationJournalWriteV1 {
        hash_algorithm: algorithm,
        owner_id: MIGRATION_ID,
        owner_kind: JournalOwnerKindV1::Task,
        generation: 1,
        segment_ordinal: 1,
        chain_reset: true,
        previous_segment: &zero,
        semantic_state_root: &graph.source_after_root,
        runtime_boot_id: RUNTIME_BOOT_ID,
        records: &records,
      })
      .unwrap();
      workspace.append_segment(&segment.value).unwrap();
    }
    let checkpoint_request = workspace.prepare_capturing_checkpoint(BASE_TIME as i64 + 600).unwrap();
    let checkpoint = workspace.publish_checkpoint(&checkpoint_request).unwrap();
    owner
      .publish_capture_checkpoint(
        MigrationCaptureCheckpointPublicationRequestV1 {
          captured_through_publication_sequence: captured_sequence,
          checkpoint_artifact: checkpoint.manifest_identity().to_vec(),
          updated_at_ms: BASE_TIME as i64 + 600,
          publication_timestamp_ms: BASE_TIME + 600,
          monotonic_now_ms: 10_100,
        },
        &mut retirement,
      )
      .unwrap();
    let capture = ReopenedMigrationCaptureWorkspaceV1::open_selected(
      checkpoint.workspace_path(),
      checkpoint.manifest_identity(),
      capture_identity,
      basis,
      MigrationCaptureWorkspaceReopenOptionsV1::new(16 << 20).unwrap(),
      cancellation.clone(),
      &memory,
    )
    .unwrap();
    let required_capabilities = permit.required_reader_capabilities().into_bytes();
    let authority = MigrationCaptureReplayAuthorityTemplateV1 {
      base_predecessor_head: initialized.first_authority().namespace_root.root_hash.clone(),
      semantic_state: encode_semantic_state_object(
        &SemanticStateWriteV1 {
          required_capabilities,
          availability: SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured },
        },
        algorithm,
      )
      .unwrap(),
      required_capabilities,
      typed_closure_context: b"capture replay test closure".to_vec(),
      authority_identity: b"HEAD".to_vec(),
      publication_timestamp_floor_ms: BASE_TIME + 1_000,
      monotonic_timestamp_floor_ms: 10_200,
    };
    Self {
      _directory: directory,
      permit,
      initialized,
      source: graph.source,
      capture,
      owner,
      retirement,
      memory,
      cancellation,
      authority,
      base_root: graph.base_root,
      after_root: graph.source_after_root,
      expected_destination_root: graph.expected_destination_root,
    }
  }

  fn execute(
    &mut self,
    sink: &mut dyn MigrationCaptureReplayRootSinkV1,
  ) -> Result<aeordb::engine::v4::migration_capture_replay::MigrationCaptureReplayReceiptV1, String> {
    self.execute_with_replay_memory(sink, 32 << 20)
  }

  fn execute_with_replay_memory(
    &mut self,
    sink: &mut dyn MigrationCaptureReplayRootSinkV1,
    maximum_replay_memory_bytes: u64,
  ) -> Result<aeordb::engine::v4::migration_capture_replay::MigrationCaptureReplayReceiptV1, String> {
    self.execute_with_bounds(sink, maximum_replay_memory_bytes, 10_000, 10_000)
  }

  fn execute_with_bounds(
    &mut self,
    sink: &mut dyn MigrationCaptureReplayRootSinkV1,
    maximum_replay_memory_bytes: u64,
    maximum_records: u64,
    maximum_publications: u64,
  ) -> Result<aeordb::engine::v4::migration_capture_replay::MigrationCaptureReplayReceiptV1, String> {
    execute_selected_migration_capture_replay_v1(MigrationCaptureReplayRequestV1 {
      permit: &self.permit,
      capture: &self.capture,
      source: &self.source,
      destination: self.initialized.publisher(),
      state_owner: &self.owner,
      retirement_owner: &mut self.retirement,
      root_sink: sink,
      base_destination_tree_root: &self.base_root,
      authority: &self.authority,
      memory: &self.memory,
      cancellation: &self.cancellation,
      maximum_replay_memory_bytes,
      maximum_subtree_memory_bytes: 16 << 20,
      maximum_subtree_work_items: 100_000,
      maximum_decoded_chunk_bytes: 1 << 20,
      maximum_directory_depth: 128,
      maximum_records,
      maximum_publications,
    })
    .map_err(|error| format!("{}: {error}", error.code()))
  }
}

#[test]
fn selected_capture_replay_refuses_basis_and_selected_checkpoint_divergence_before_root_publication() {
  let mut basis = Harness::new_with_scenario(HashAlgorithm::Blake3_256, CaptureScenario::BasisMismatch);
  let before = basis.initialized.publisher().observe().unwrap().selected.header.head_hash;
  let error = basis.execute(&mut RootSink::default()).unwrap_err();
  assert!(error.contains("migration_replay_basis_divergence"), "{error}");
  assert_eq!(basis.initialized.publisher().observe().unwrap().selected.header.head_hash, before);

  let mut selected = Harness::new(HashAlgorithm::Blake3_256);
  selected
    .owner
    .publish_capture_checkpoint(
      MigrationCaptureCheckpointPublicationRequestV1 {
        captured_through_publication_sequence: REPLAY_SEQUENCE,
        checkpoint_artifact: digest_parts(HashAlgorithm::Blake3_256, &[b"different selected capture checkpoint"]),
        updated_at_ms: BASE_TIME as i64 + 700,
        publication_timestamp_ms: BASE_TIME + 700,
        monotonic_now_ms: 10_300,
      },
      &mut selected.retirement,
    )
    .unwrap();
  let before = selected.initialized.publisher().observe().unwrap().selected.header.head_hash;
  let error = selected.execute(&mut RootSink::default()).unwrap_err();
  assert!(error.contains("migration_replay_checkpoint_selection"), "{error}");
  assert_eq!(selected.initialized.publisher().observe().unwrap().selected.header.head_hash, before);
}

#[test]
fn selected_capture_replay_refuses_publication_metadata_and_work_bound_divergence() {
  let mut divergent = Harness::new_with_scenario(HashAlgorithm::Blake3_256, CaptureScenario::SamePublicationTimeDivergence);
  let error = divergent.execute(&mut RootSink::default()).unwrap_err();
  assert!(error.contains("migration_replay_publication_divergence"), "{error}");

  let mut bounded = Harness::new_with_scenario(HashAlgorithm::Blake3_256, CaptureScenario::TwoBatchesOnePublication);
  let error = bounded.execute_with_bounds(&mut RootSink::default(), 32 << 20, 1, 1).unwrap_err();
  assert!(error.contains("migration_replay_work_limit"), "{error}");

  let mut invalid = Harness::new(HashAlgorithm::Blake3_256);
  let before = invalid.initialized.publisher().observe().unwrap().selected.header.head_hash;
  let error = invalid.execute_with_bounds(&mut RootSink::default(), 32 << 20, 1, 0).unwrap_err();
  assert!(error.contains("migration_replay_bounds"), "{error}");
  assert_eq!(invalid.initialized.publisher().observe().unwrap().selected.header.head_hash, before);

  let mut clock = Harness::new(HashAlgorithm::Blake3_256);
  clock.authority.monotonic_timestamp_floor_ms = u64::MAX;
  let before = clock.initialized.publisher().observe().unwrap().selected.header.head_hash;
  let error = clock.execute(&mut RootSink::default()).unwrap_err();
  assert!(error.contains("migration_replay_monotonic_time"), "{error}");
  assert_eq!(clock.initialized.publisher().observe().unwrap().selected.header.head_hash, before);
}

#[test]
fn selected_capture_replay_recovers_a_committed_root_after_root_map_sink_failure() {
  let mut harness = Harness::new(HashAlgorithm::Blake3_256);
  let mut failing = FailingRootSink { fail_on_call: 2, calls: 0 };
  let error = harness.execute(&mut failing).unwrap_err();
  assert!(error.contains("injected root-map sink failure"), "{error}");
  let after_failure = harness.initialized.publisher().observe().unwrap().selected.header.clone();
  let committed_root_locator = harness.initialized.publisher().locator(&after_failure.head_hash).unwrap();
  let checkpoint = harness.owner.observe_capture_state(BASE_TIME as i64 + 2_000, BASE_TIME + 2_000, 10_300).unwrap();
  assert_eq!(checkpoint.reconciled_through_publication_sequence, BASE_SEQUENCE);

  let mut sink = RootSink::default();
  let receipt = harness.execute(&mut sink).unwrap();
  let after_retry = harness.initialized.publisher().observe().unwrap().selected.header;
  assert_eq!(receipt.destination_tree_root, harness.after_root);
  assert_eq!(receipt.destination_successor_count, 0);
  assert_eq!(after_retry.head_hash, after_failure.head_hash);
  assert_eq!(harness.initialized.publisher().locator(&after_retry.head_hash).unwrap(), committed_root_locator);
  assert_eq!(sink.rows.len(), 2);
}

#[test]
fn selected_capture_replay_refuses_an_understated_record_memory_budget_before_destination_change() {
  let mut harness = Harness::new(HashAlgorithm::Blake3_256);
  let before = harness.initialized.publisher().observe().unwrap().selected.header.head_hash;
  let error = harness.execute_with_replay_memory(&mut RootSink::default(), 4 << 20).unwrap_err();
  assert!(error.contains("migration_replay_bounds"), "{error}");
  assert_eq!(harness.initialized.publisher().observe().unwrap().selected.header.head_hash, before);
}

#[test]
fn selected_capture_replay_groups_atomic_batches_from_one_source_publication() {
  let mut harness = Harness::new_with_scenario(HashAlgorithm::Blake3_256, CaptureScenario::TwoBatchesOnePublication);
  let receipt = harness.execute(&mut RootSink::default()).unwrap();
  assert_eq!(receipt.publication_count, 1);
  assert_eq!(receipt.record_count, 2);
  assert_eq!(receipt.destination_tree_root, harness.after_root);
}

#[test]
fn selected_capture_replay_copy_on_writes_nested_directory_ancestors() {
  let mut harness = Harness::new_with_scenario(HashAlgorithm::Blake3_256, CaptureScenario::NestedUpdate);
  let source_before = source_fingerprint(&harness.source);
  let receipt = harness.execute(&mut RootSink::default()).unwrap();
  assert_eq!(receipt.collapsed_path_count, 1);
  assert_eq!(receipt.destination_successor_count, 2);
  assert_eq!(receipt.destination_tree_root, harness.expected_destination_root);
  assert_eq!(source_fingerprint(&harness.source), source_before);
}

#[test]
fn selected_capture_replay_uses_the_shared_btree_planner_for_large_directories() {
  let mut harness = Harness::new_with_scenario(HashAlgorithm::Blake3_256, CaptureScenario::BTreeUpdate);
  let receipt = harness.execute(&mut RootSink::default()).unwrap();
  let (_, expected_file, _) = file_entity_at(HashAlgorithm::Blake3_256, "/file-0128.txt", b"new", BASE_TIME as i64 + 2);
  let selected = destination_btree_lookup(
    harness.initialized.publisher(),
    &receipt.destination_tree_root,
    "file-0128.txt",
    HashAlgorithm::Blake3_256.hash_length(),
  )
  .unwrap();
  assert_eq!(selected.hash, expected_file);
  assert_ne!(receipt.destination_tree_root, harness.base_root);
  assert!(harness.initialized.publisher().locator(&receipt.destination_tree_root).unwrap().is_some());
}

#[test]
fn selected_capture_replay_checkpoints_an_omitted_system_family_without_fabricating_a_root() {
  let mut harness = Harness::new_with_scenario(HashAlgorithm::Blake3_256, CaptureScenario::OmittedSystemFamily);
  let receipt = harness.execute(&mut RootSink::default()).unwrap();
  assert_eq!(receipt.replayed_through_publication_sequence, REPLAY_SEQUENCE);
  assert_eq!(receipt.destination_tree_root, harness.expected_destination_root);
  assert_eq!(receipt.destination_successor_count, 0, "initial authority and omitted publication already have the desired root");
  assert_eq!(receipt.unchanged_destination_count, 1);
}

#[test]
fn selected_capture_replay_accepts_a_base_only_checkpoint_without_segments() {
  let mut harness = Harness::new_with_scenario(HashAlgorithm::Blake3_256, CaptureScenario::NoPublications);
  let receipt = harness.execute(&mut RootSink::default()).unwrap();
  assert_eq!(receipt.replayed_through_publication_sequence, BASE_SEQUENCE);
  assert_eq!(receipt.capture_segment_count, 0);
  assert_eq!(receipt.publication_count, 0);
  assert_eq!(receipt.record_count, 0);
  assert_eq!(receipt.destination_tree_root, harness.expected_destination_root);
}

#[test]
fn selected_capture_replay_applies_every_supported_mutation_kind() {
  for scenario in [
    CaptureScenario::Create,
    CaptureScenario::Delete,
    CaptureScenario::Move,
    CaptureScenario::Copy,
    CaptureScenario::Restore,
    CaptureScenario::Transition,
  ] {
    let mut harness = Harness::new_with_scenario(HashAlgorithm::Blake3_256, scenario);
    let receipt = harness.execute(&mut RootSink::default()).unwrap();
    assert_eq!(receipt.replayed_through_publication_sequence, REPLAY_SEQUENCE);
    assert_eq!(receipt.destination_tree_root, harness.expected_destination_root);
  }
}

#[test]
fn selected_capture_replay_requires_kind_specific_historical_absence() {
  for scenario in [CaptureScenario::CreateOverExistingPath, CaptureScenario::DeleteLeavingPathPresent] {
    let mut harness = Harness::new_with_scenario(HashAlgorithm::Blake3_256, scenario);
    let error = harness.execute(&mut RootSink::default()).unwrap_err();
    assert!(error.contains("migration_replay_presence_divergence"), "{error}");
  }
}

#[test]
fn selected_capture_replay_updates_real_destination_and_retries_idempotently() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let mut harness = Harness::new(algorithm);
    let mut sink = RootSink::default();
    let first = harness.execute(&mut sink).unwrap();
    assert_eq!(first.replayed_through_publication_sequence, REPLAY_SEQUENCE);
    assert_eq!(first.publication_count, 1);
    assert_eq!(first.record_count, 1);
    assert_eq!(first.collapsed_path_count, 1);
    assert_eq!(first.destination_successor_count, 2);
    assert_eq!(first.destination_tree_root, harness.after_root);
    assert_eq!(sink.rows.len(), 2);

    let selected_after_first = harness.initialized.publisher().observe().unwrap().selected.header.clone();
    let retry = harness.execute(&mut sink).unwrap();
    let selected_after_retry = harness.initialized.publisher().observe().unwrap().selected.header.clone();
    assert_eq!(retry.destination_tree_root, harness.after_root);
    assert_eq!(retry.destination_successor_count, 0);
    assert_eq!(selected_after_retry.head_hash, selected_after_first.head_hash);
    assert_eq!(selected_after_retry.slot_sequence, selected_after_first.slot_sequence);
    assert_eq!(sink.rows.len(), 2);
  }
}

#[test]
fn selected_capture_replay_refuses_cancellation_before_destination_change() {
  let mut harness = Harness::new(HashAlgorithm::Blake3_256);
  let before = harness.initialized.publisher().observe().unwrap().selected.header.head_hash;
  harness.cancellation.cancel();
  let error = harness.execute(&mut RootSink::default()).unwrap_err();
  assert!(error.contains("migration_capture_workspace_cancelled") || error.contains("migration_replay_cancelled"), "{error}");
  assert_eq!(harness.initialized.publisher().observe().unwrap().selected.header.head_hash, before);
}

#[test]
fn selected_capture_replay_refuses_historical_revision_divergence_without_advancing_checkpoint() {
  let mut harness = Harness::new(HashAlgorithm::Blake3_256);
  harness.source.entries.remove(&harness.after_root);
  let error = harness.execute(&mut RootSink::default()).unwrap_err();
  assert!(error.contains("migration_replay_missing_source_entity") || error.contains("migration_replay_revision_divergence"), "{error}");
  let capture = harness.owner.observe_capture_state(BASE_TIME as i64 + 2_000, BASE_TIME + 2_000, 10_300).unwrap();
  assert_eq!(capture.reconciled_through_publication_sequence, BASE_SEQUENCE);
}
