use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aeordb::engine::directory_entry::{ChildEntry, deserialize_child_entries, serialize_child_entries};
use aeordb::engine::entry_header::FLAG_SYSTEM;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy, MemoryPressure};
use aeordb::engine::native_durability::PlatformFileIdentityDescriptorV1;
use aeordb::engine::v4::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use aeordb::engine::v4::contract_generated::CONTRACT_REGISTRY_SHA256;
use aeordb::engine::v4::entity::{EntryTypeV4, WholeEntityV1, decode_whole_entity};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::migration_base_clone_execution::{
  MigrationBaseCloneEntrySourceV1, MigrationBaseCloneExecutionRequestV1, MigrationBaseCloneSeedKindV1, MigrationBaseCloneSeedResultSinkV1,
  MigrationBaseCloneSeedSourceV1, MigrationBaseCloneSeedV1, MigrationBaseCloneStreamClosureV1, execute_migration_base_clone_v1,
};
use aeordb::engine::v4::migration_destination::{
  InitializedMigrationDestinationV1, MigrationDestinationInitializationRequestV1, MigrationDestinationPathObservationV1,
  initialize_migration_destination_v1, observe_migration_destination_path_v1,
};
use aeordb::engine::v4::migration_preflight::{
  AuthorityInventoryCountsV1, CapacityRoleV1, MigrationBinaryEvidenceV1, MigrationCapacityObservationV1, MigrationConfigurationEvidenceV1,
  MigrationIdentityEvidenceV1, MigrationMemoryEvidenceV1, MigrationNativeEvidenceV1, MigrationPreflightPermitV1,
  MigrationPreflightRequestV1, MigrationRecoveryEvidenceV1, MigrationSourceEvidenceV1, NativeCutoverCapabilitiesV1,
  SourceAuthorityInventoryV1, StrictVerificationEvidenceV1, StrictVerificationStateV1, admit_migration_preflight_v1,
};
use aeordb::engine::v4::system_family::embedded_system_family_registry;
use aeordb::engine::storage_engine::EntryData;
use aeordb::engine::{
  CompressionAlgorithm, DirectoryOps, EngineError, EngineResult, EntryHeader, EntryType, FileRecord, HashAlgorithm, RequestContext,
  StorageEngine, SymlinkRecord, compress,
};
use aeordb::engine::{btree::BTreeNode, btree::InternalNode, btree::LeafNode};
use tokio_util::sync::CancellationToken;

const GIB: u64 = 1024 * 1024 * 1024;
const DATABASE_ID: [u8; 16] = [0x10; 16];
const MIGRATION_ID: [u8; 16] = [0x20; 16];
const SOURCE_PHYSICAL_ID: [u8; 16] = [0x30; 16];
const DESTINATION_PHYSICAL_ID: [u8; 16] = [0x40; 16];

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

fn permit(
  algorithm: HashAlgorithm,
  source_identity: PlatformFileIdentityDescriptorV1,
  source_head: Vec<u8>,
  destination: &MigrationDestinationPathObservationV1,
  counts: AuthorityInventoryCountsV1,
) -> MigrationPreflightPermitV1 {
  let baseline = CapabilitySetV1::v4_baseline();
  let registry = embedded_system_family_registry(algorithm).unwrap();
  let source_checksum = digest(0x71);
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
      complete_file_checksum: source_checksum,
      selected_header_slot: 1,
      selected_header_sequence: 41,
      selected_header_digest: digest(0x80),
      head_hash: source_head,
    },
    verification: StrictVerificationEvidenceV1 {
      state: StrictVerificationStateV1::CompleteClean,
      source_file_size: 4 * GIB,
      source_header_sequence: 41,
      source_complete_file_checksum: source_checksum,
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
      counts,
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

fn counts(roots: u64, snapshots: u64) -> AuthorityInventoryCountsV1 {
  AuthorityInventoryCountsV1 {
    protected_families: 46,
    modules: 0,
    snapshots,
    forks: 0,
    symlinks: 1,
    history_roots: snapshots,
    peers: 0,
    sync_states: 0,
    tasks: 0,
    plugins: 0,
    roots,
  }
}

fn initialize_destination(
  directory: &tempfile::TempDir,
  algorithm: HashAlgorithm,
  source_identity: PlatformFileIdentityDescriptorV1,
  source_head: Vec<u8>,
  counts: AuthorityInventoryCountsV1,
) -> (MigrationPreflightPermitV1, InitializedMigrationDestinationV1) {
  let path = directory.path().join("shadow.aeordb");
  let destination = observe_migration_destination_path_v1(&path).unwrap();
  let permit = permit(algorithm, source_identity, source_head, &destination, counts);
  let initialized = initialize_migration_destination_v1(MigrationDestinationInitializationRequestV1 {
    permit: &permit,
    destination: &destination,
    created_at_ms: 1_700_000_000_000,
    writer_fence_epoch: 7,
    cancellation: &CancellationToken::new(),
  })
  .unwrap();
  (permit, initialized)
}

fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(128 << 20, 256 << 20, 1, 16 << 20).unwrap())
}

#[derive(Clone)]
struct Stream {
  seeds: VecDeque<MigrationBaseCloneSeedV1>,
  closure: MigrationBaseCloneStreamClosureV1,
  fail_next: bool,
  finished: bool,
}

impl Stream {
  fn new(permit: &MigrationPreflightPermitV1, seeds: Vec<MigrationBaseCloneSeedV1>) -> Self {
    Self {
      seeds: seeds.into(),
      closure: MigrationBaseCloneStreamClosureV1 {
        database_id: permit.database_id(),
        source_physical_instance_id: permit.source_physical_instance_id(),
        source_header_sequence: permit.source_header_sequence(),
        source_capture_head: permit.source_capture_head().to_vec(),
        source_authority_digest: permit.source_authority_digest(),
        source_authority_counts: permit.source_authority_counts(),
      },
      fail_next: false,
      finished: false,
    }
  }
}

impl MigrationBaseCloneSeedSourceV1 for Stream {
  fn next_seed(&mut self) -> EngineResult<Option<MigrationBaseCloneSeedV1>> {
    if self.fail_next {
      return Err(EngineError::IoError(std::io::Error::other("injected seed stream failure")));
    }
    Ok(self.seeds.pop_front())
  }

  fn finish(&mut self) -> EngineResult<MigrationBaseCloneStreamClosureV1> {
    self.finished = true;
    Ok(self.closure.clone())
  }
}

#[derive(Default)]
struct SeedResultSink {
  fail: bool,
}

impl MigrationBaseCloneSeedResultSinkV1 for SeedResultSink {
  fn record_seed_result(&mut self, _seed: &MigrationBaseCloneSeedV1, _destination_hash: Option<&[u8]>) -> EngineResult<()> {
    if self.fail {
      return Err(EngineError::IoError(std::io::Error::other("injected seed result failure")));
    }
    Ok(())
  }
}

#[derive(Clone)]
struct FakeSource {
  algorithm: HashAlgorithm,
  identity: PlatformFileIdentityDescriptorV1,
  entries: HashMap<Vec<u8>, EntryData>,
  wrong_returned_key: Option<Vec<u8>>,
  returned_header_override: Option<EntryHeader>,
  cancel_after_file_read: Option<CancellationToken>,
  chunk_reads: Arc<AtomicUsize>,
}

impl FakeSource {
  fn new(algorithm: HashAlgorithm, identity: PlatformFileIdentityDescriptorV1) -> Self {
    Self {
      algorithm,
      identity,
      entries: HashMap::new(),
      wrong_returned_key: None,
      returned_header_override: None,
      cancel_after_file_read: None,
      chunk_reads: Arc::new(AtomicUsize::new(0)),
    }
  }

  fn insert(&mut self, entry_type: EntryType, entry_version: u8, compression: CompressionAlgorithm, key: Vec<u8>, value: Vec<u8>) {
    self.insert_with_flags(entry_type, entry_version, compression, 0, key, value);
  }

  fn insert_with_flags(
    &mut self,
    entry_type: EntryType,
    entry_version: u8,
    compression: CompressionAlgorithm,
    flags: u8,
    key: Vec<u8>,
    value: Vec<u8>,
  ) {
    let total_length = EntryHeader::compute_total_length(self.algorithm, key.len(), value.len()).unwrap();
    let header = EntryHeader {
      entry_version,
      entry_type,
      flags,
      hash_algo: self.algorithm,
      compression_algo: compression,
      encryption_algo: 0,
      key_length: key.len() as u32,
      value_length: value.len() as u32,
      timestamp: 1_700_000_000_001,
      total_length,
      hash: digest_parts(self.algorithm, &[b"synthetic verified entry", &key, &value]),
    };
    self.entries.insert(key.clone(), (header, key, value));
  }

  fn replace_value(&mut self, key: &[u8], value: Vec<u8>) {
    let (header, stored_key, _) = self.entries.get(key).cloned().expect("synthetic entry to replace");
    self.insert_with_flags(header.entry_type, header.entry_version, header.compression_algo, header.flags, stored_key, value);
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
      return Err(EngineError::ResourceExhausted("synthetic bounded source refusal".to_string()));
    }
    if header.entry_type == EntryType::FileRecord {
      if let Some(cancellation) = &self.cancel_after_file_read {
        cancellation.cancel();
      }
    } else if header.entry_type == EntryType::Chunk {
      self.chunk_reads.fetch_add(1, Ordering::Relaxed);
    }
    Ok(Some((
      self.returned_header_override.clone().unwrap_or_else(|| header.clone()),
      self.wrong_returned_key.clone().unwrap_or_else(|| key.clone()),
      value.clone(),
    )))
  }
}

fn content_key(algorithm: HashAlgorithm, entry_type: EntryType, value: &[u8]) -> Vec<u8> {
  let domain: &[u8] = match entry_type {
    EntryType::Chunk => b"chunk:",
    EntryType::FileRecord => b"filec:",
    EntryType::DirectoryIndex if aeordb::engine::btree::is_btree_format(value) => b"btree:",
    EntryType::DirectoryIndex => b"dirc:",
    EntryType::Symlink => b"symlinkc:",
    other => panic!("no content domain for {other:?}"),
  };
  digest_parts(algorithm, &[domain, value])
}

fn file_identity_key(algorithm: HashAlgorithm, record: &FileRecord) -> Vec<u8> {
  let chunk_bytes = record.chunk_hashes.iter().flat_map(|hash| hash.iter().copied()).collect::<Vec<_>>();
  digest_parts(
    algorithm,
    &[b"fileid:", record.path.as_bytes(), &[0], record.content_type.as_deref().unwrap_or("").as_bytes(), &[0], &chunk_bytes],
  )
}

fn child(name: &str, entry_type: EntryType, hash: Vec<u8>, total_size: u64) -> ChildEntry {
  ChildEntry {
    entry_type: entry_type.to_u8(),
    hash,
    total_size,
    created_at: 1_700_000_000_001,
    updated_at: 1_700_000_000_001,
    name: name.to_string(),
    content_type: None,
    virtual_time: 1,
    node_id: 1,
  }
}

struct SyntheticGraph {
  source: FakeSource,
  head: Vec<u8>,
  expected_keys: Vec<Vec<u8>>,
  omitted_key: Vec<u8>,
  decoded_chunk: Vec<u8>,
}

struct SingleFileGraph {
  source: FakeSource,
  head: Vec<u8>,
  file_identity: Vec<u8>,
  source_file_content: Vec<u8>,
  file_content: Vec<u8>,
  source_chunk: Vec<u8>,
  destination_chunk: Vec<u8>,
  record: FileRecord,
}

fn single_file_graph(
  algorithm: HashAlgorithm,
  source_identity: PlatformFileIdentityDescriptorV1,
  path: &str,
  payload: &[u8],
  system: bool,
) -> SingleFileGraph {
  single_file_graph_with_legacy_system_chunk(algorithm, source_identity, path, payload, system, false)
}

fn single_file_graph_with_legacy_system_chunk(
  algorithm: HashAlgorithm,
  source_identity: PlatformFileIdentityDescriptorV1,
  path: &str,
  payload: &[u8],
  system: bool,
  legacy_system_chunk: bool,
) -> SingleFileGraph {
  let mut source = FakeSource::new(algorithm, source_identity);
  let destination_chunk = digest_parts(algorithm, &[b"chunk:", payload]);
  let source_chunk = if legacy_system_chunk { digest_parts(algorithm, &[b"system::", payload]) } else { destination_chunk.clone() };
  source.insert_with_flags(
    EntryType::Chunk,
    0,
    CompressionAlgorithm::None,
    if system { FLAG_SYSTEM } else { 0 },
    source_chunk.clone(),
    payload.to_vec(),
  );
  let record = FileRecord {
    path: path.to_string(),
    content_type: Some("application/octet-stream".to_string()),
    total_size: payload.len() as u64,
    created_at: 1_700_000_000_001,
    updated_at: 1_700_000_000_001,
    metadata: Vec::new(),
    content_hash: digest_parts(algorithm, &[payload]),
    chunk_hashes: vec![source_chunk.clone()],
  };
  let file_value = record.serialize(algorithm.hash_length()).unwrap();
  let source_file_content = content_key(algorithm, EntryType::FileRecord, &file_value);
  let mut destination_record = record.clone();
  destination_record.chunk_hashes = vec![destination_chunk.clone()];
  let file_content = content_key(algorithm, EntryType::FileRecord, &destination_record.serialize(algorithm.hash_length()).unwrap());
  let file_identity = file_identity_key(algorithm, &record);
  source.insert_with_flags(
    EntryType::FileRecord,
    1,
    CompressionAlgorithm::None,
    if system { FLAG_SYSTEM } else { 0 },
    file_identity.clone(),
    file_value,
  );
  let root_value = if system {
    Vec::new()
  } else {
    serialize_child_entries(
      &[child(path.trim_start_matches('/'), EntryType::FileRecord, file_identity.clone(), payload.len() as u64)],
      algorithm.hash_length(),
    )
    .unwrap()
  };
  let head = content_key(algorithm, EntryType::DirectoryIndex, &root_value);
  source.insert(EntryType::DirectoryIndex, 0, CompressionAlgorithm::None, head.clone(), root_value);
  SingleFileGraph { source, head, file_identity, source_file_content, file_content, source_chunk, destination_chunk, record }
}

fn synthetic_graph(algorithm: HashAlgorithm, source_identity: PlatformFileIdentityDescriptorV1) -> SyntheticGraph {
  let mut source = FakeSource::new(algorithm, source_identity);
  let decoded_chunk = vec![0x5a; 128 * 1024];
  let chunk_key = content_key(algorithm, EntryType::Chunk, &decoded_chunk);
  source.insert(
    EntryType::Chunk,
    0,
    CompressionAlgorithm::Zstd,
    chunk_key.clone(),
    compress(&decoded_chunk, CompressionAlgorithm::Zstd).unwrap(),
  );

  let make_file = |path: &str| FileRecord {
    path: path.to_string(),
    content_type: Some("application/octet-stream".to_string()),
    total_size: decoded_chunk.len() as u64,
    created_at: 1_700_000_000_001,
    updated_at: 1_700_000_000_001,
    metadata: Vec::new(),
    content_hash: digest_parts(algorithm, &[&decoded_chunk]),
    chunk_hashes: vec![chunk_key.clone()],
  };
  let plain_value = make_file("/plain.bin").serialize(algorithm.hash_length()).unwrap();
  let plain_key = content_key(algorithm, EntryType::FileRecord, &plain_value);
  source.insert(EntryType::FileRecord, 1, CompressionAlgorithm::None, plain_key.clone(), plain_value);
  let left_value = make_file("/media/a.bin").serialize(algorithm.hash_length()).unwrap();
  let left_key = content_key(algorithm, EntryType::FileRecord, &left_value);
  source.insert(EntryType::FileRecord, 1, CompressionAlgorithm::None, left_key.clone(), left_value);
  let right_value = make_file("/media/z.bin").serialize(algorithm.hash_length()).unwrap();
  let right_key = content_key(algorithm, EntryType::FileRecord, &right_value);
  source.insert(EntryType::FileRecord, 1, CompressionAlgorithm::None, right_key.clone(), right_value);

  let symlink_value = SymlinkRecord {
    path: "/plain-link".to_string(),
    target: "/plain.bin".to_string(),
    created_at: 1_700_000_000_001,
    updated_at: 1_700_000_000_001,
  }
  .serialize()
  .unwrap();
  let symlink_key = content_key(algorithm, EntryType::Symlink, &symlink_value);
  source.insert(EntryType::Symlink, 0, CompressionAlgorithm::None, symlink_key.clone(), symlink_value);

  let left_leaf =
    BTreeNode::Leaf(LeafNode { entries: vec![child("a.bin", EntryType::FileRecord, left_key.clone(), decoded_chunk.len() as u64)] });
  let left_leaf_value = left_leaf.serialize(algorithm.hash_length()).unwrap();
  let left_leaf_key = content_key(algorithm, EntryType::DirectoryIndex, &left_leaf_value);
  source.insert(EntryType::DirectoryIndex, 0, CompressionAlgorithm::None, left_leaf_key.clone(), left_leaf_value);
  let right_leaf =
    BTreeNode::Leaf(LeafNode { entries: vec![child("z.bin", EntryType::FileRecord, right_key.clone(), decoded_chunk.len() as u64)] });
  let right_leaf_value = right_leaf.serialize(algorithm.hash_length()).unwrap();
  let right_leaf_key = content_key(algorithm, EntryType::DirectoryIndex, &right_leaf_value);
  source.insert(EntryType::DirectoryIndex, 0, CompressionAlgorithm::None, right_leaf_key.clone(), right_leaf_value);
  let media_root =
    BTreeNode::Internal(InternalNode { keys: vec!["z.bin".to_string()], children: vec![left_leaf_key.clone(), right_leaf_key.clone()] });
  let media_value = media_root.serialize(algorithm.hash_length()).unwrap();
  let media_key = content_key(algorithm, EntryType::DirectoryIndex, &media_value);
  source.insert(EntryType::DirectoryIndex, 0, CompressionAlgorithm::None, media_key.clone(), media_value);

  let omitted_key = vec![0x77; algorithm.hash_length()];
  let legacy_index_value =
    serialize_child_entries(&[child("text.idx", EntryType::FileRecord, omitted_key.clone(), 99)], algorithm.hash_length()).unwrap();
  let legacy_index_key = content_key(algorithm, EntryType::DirectoryIndex, &legacy_index_value);
  source.insert(EntryType::DirectoryIndex, 0, CompressionAlgorithm::None, legacy_index_key.clone(), legacy_index_value);

  let root_value = serialize_child_entries(
    &[
      child(".aeordb-indexes", EntryType::DirectoryIndex, legacy_index_key, 0),
      child("media", EntryType::DirectoryIndex, media_key.clone(), 0),
      child("plain-link", EntryType::Symlink, symlink_key.clone(), 0),
      child("plain.bin", EntryType::FileRecord, plain_key.clone(), decoded_chunk.len() as u64),
    ],
    algorithm.hash_length(),
  )
  .unwrap();
  let head = content_key(algorithm, EntryType::DirectoryIndex, &root_value);
  source.insert(EntryType::DirectoryIndex, 0, CompressionAlgorithm::None, head.clone(), root_value);
  SyntheticGraph {
    source,
    head: head.clone(),
    expected_keys: vec![head, media_key, left_leaf_key, right_leaf_key, plain_key, left_key, right_key, symlink_key, chunk_key],
    omitted_key,
    decoded_chunk,
  }
}

fn seeds(head: &[u8], with_snapshot: bool) -> Vec<MigrationBaseCloneSeedV1> {
  let mut seeds = vec![MigrationBaseCloneSeedV1 {
    kind: MigrationBaseCloneSeedKindV1::CurrentHead,
    path: "/".to_string(),
    entry_type: EntryType::DirectoryIndex,
    hash: head.to_vec(),
  }];
  if with_snapshot {
    seeds.push(MigrationBaseCloneSeedV1 {
      kind: MigrationBaseCloneSeedKindV1::Snapshot,
      path: "/".to_string(),
      entry_type: EntryType::DirectoryIndex,
      hash: head.to_vec(),
    });
  }
  seeds
}

fn execute(
  permit: &MigrationPreflightPermitV1,
  source: &dyn MigrationBaseCloneEntrySourceV1,
  stream: &mut dyn MigrationBaseCloneSeedSourceV1,
  destination: &InitializedMigrationDestinationV1,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
  maximum_memory_bytes: u64,
) -> Result<
  aeordb::engine::v4::migration_base_clone_execution::MigrationBaseCloneExecutionReceiptV1,
  aeordb::engine::v4::migration_base_clone_execution::MigrationBaseCloneExecutionErrorV1,
> {
  execute_with_limits(permit, source, stream, destination, memory, cancellation, 1_000_000, maximum_memory_bytes, 1024 * 1024, 1_000)
}

#[allow(clippy::too_many_arguments)]
fn execute_with_limits(
  permit: &MigrationPreflightPermitV1,
  source: &dyn MigrationBaseCloneEntrySourceV1,
  stream: &mut dyn MigrationBaseCloneSeedSourceV1,
  destination: &InitializedMigrationDestinationV1,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
  maximum_work_items: u64,
  maximum_memory_bytes: u64,
  maximum_decoded_chunk_bytes: usize,
  maximum_directory_depth: usize,
) -> Result<
  aeordb::engine::v4::migration_base_clone_execution::MigrationBaseCloneExecutionReceiptV1,
  aeordb::engine::v4::migration_base_clone_execution::MigrationBaseCloneExecutionErrorV1,
> {
  let mut seed_results = SeedResultSink::default();
  execute_migration_base_clone_v1(MigrationBaseCloneExecutionRequestV1 {
    permit,
    source,
    seeds: stream,
    seed_results: &mut seed_results,
    destination: destination.publisher(),
    memory,
    cancellation,
    publication_timestamp_ms: 1_700_000_000_100,
    maximum_work_items,
    maximum_memory_bytes,
    maximum_decoded_chunk_bytes,
    maximum_directory_depth,
  })
}

fn read_destination_entity<'a>(
  destination: &'a InitializedMigrationDestinationV1,
  key: &[u8],
  bytes: &'a mut Vec<u8>,
) -> WholeEntityV1<'a> {
  let locator = destination.publisher().locator(key).unwrap().expect("destination entity locator");
  let mut file = OpenOptions::new().read(true).open(destination.path()).unwrap();
  file.seek(SeekFrom::Start(locator.offset)).unwrap();
  bytes.resize(locator.total_length as usize, 0);
  file.read_exact(bytes).unwrap();
  let high_water = destination.publisher().observe().unwrap().selected.header.write_sequence_high_water;
  decode_whole_entity(bytes, destination.publisher().observe().unwrap().selected.header.hash_algorithm, high_water).unwrap()
}

fn assert_destination_directory_closure(destination: &InitializedMigrationDestinationV1, root: &[u8]) {
  let algorithm = destination.publisher().observe().unwrap().selected.header.hash_algorithm;
  let mut pending = vec![root.to_vec()];
  let mut visited = std::collections::HashSet::new();
  while let Some(directory_hash) = pending.pop() {
    if !visited.insert(directory_hash.clone()) {
      continue;
    }
    let mut bytes = Vec::new();
    let entity = read_destination_entity(destination, &directory_hash, &mut bytes);
    assert_eq!(entity.entry_type, EntryTypeV4::DirectoryIndex);
    if aeordb::engine::btree::is_btree_format(entity.stored_value) {
      match BTreeNode::deserialize(entity.stored_value, algorithm.hash_length(), entity.entity_version).unwrap() {
        BTreeNode::Leaf(leaf) => {
          for child in leaf.entries {
            assert!(destination.publisher().locator(&child.hash).unwrap().is_some(), "dangling child {}", child.name);
            if child.entry_type == EntryType::DirectoryIndex.to_u8() {
              pending.push(child.hash);
            }
          }
        }
        BTreeNode::Internal(internal) => {
          for child in internal.children {
            assert!(destination.publisher().locator(&child).unwrap().is_some(), "dangling B-tree child");
            pending.push(child);
          }
        }
      }
    } else {
      for child in deserialize_child_entries(entity.stored_value, algorithm.hash_length(), entity.entity_version).unwrap() {
        assert!(destination.publisher().locator(&child.hash).unwrap().is_some(), "dangling child {}", child.name);
        if child.entry_type == EntryType::DirectoryIndex.to_u8() {
          pending.push(child.hash);
        }
      }
    }
  }
}

fn assert_clone_error(source: &FakeSource, head: &[u8], seed_rows: Vec<MigrationBaseCloneSeedV1>, expected: &str) {
  let destination_directory = tempfile::tempdir().unwrap();
  let (permit, destination) =
    initialize_destination(&destination_directory, source.algorithm, source.identity, head.to_vec(), counts(1, 0));
  let before = destination.publisher().observe().unwrap();
  let mut stream = Stream::new(&permit, seed_rows);
  let error = execute(&permit, source, &mut stream, &destination, &memory(), &CancellationToken::new(), 96 << 20).unwrap_err();
  assert_eq!(error.code(), expected, "unexpected clone failure: {error}");
  assert_eq!(destination.publisher().observe().unwrap(), before, "small failed clone published destination authority");
}

#[test]
fn base_clone_streams_widest_hash_flat_btree_compressed_and_policy_state_without_moving_head() {
  let algorithm = HashAlgorithm::Sha512;
  let source_identity = identity(0x50, 0x70);
  let graph = synthetic_graph(algorithm, source_identity);
  let destination_directory = tempfile::tempdir().unwrap();
  let (permit, destination) = initialize_destination(&destination_directory, algorithm, source_identity, graph.head.clone(), counts(2, 1));
  let initial_head = destination.publisher().observe().unwrap().selected.header.head_hash;
  let mut stream = Stream::new(&permit, seeds(&graph.head, true));
  let memory = memory();

  let receipt = execute(&permit, &graph.source, &mut stream, &destination, &memory, &CancellationToken::new(), 96 << 20).unwrap();

  assert!(stream.finished);
  assert_eq!(receipt.processed_seeds, 2);
  assert!(receipt.loaded_entities >= graph.expected_keys.len() as u64);
  assert!(receipt.plan.structural_containers >= 2);
  assert!(receipt.plan.rebuild_items >= 2);
  assert!(receipt.duplicate_batch_entities > 0);
  assert!(receipt.maximum_batch_entities <= 511);
  assert!(receipt.maximum_batch_encoded_bytes <= 64 * 1024 * 1024);
  assert!(receipt.peak_accounted_memory_bytes <= 96 << 20);
  assert!(receipt.maximum_btree_depth >= 2);
  assert_eq!(destination.publisher().observe().unwrap().selected.header.head_hash, initial_head);
  assert_ne!(receipt.destination_head_tree, graph.head, "policy pruning must rebuild the namespace root");
  assert!(destination.publisher().locator(&receipt.destination_head_tree).unwrap().is_some());
  assert!(destination.publisher().locator(&graph.head).unwrap().is_none());
  assert_destination_directory_closure(&destination, &receipt.destination_head_tree);
  for key in &graph.expected_keys[1..4] {
    assert!(destination.publisher().locator(key).unwrap().is_none(), "stale source directory identity was copied {}", hex::encode(key));
  }
  for key in &graph.expected_keys[4..] {
    assert!(destination.publisher().locator(key).unwrap().is_some(), "missing copied identity {}", hex::encode(key));
  }
  assert!(destination.publisher().locator(&graph.omitted_key).unwrap().is_none());
  let chunk_key = graph.expected_keys.last().unwrap();
  let mut bytes = Vec::new();
  let chunk = read_destination_entity(&destination, chunk_key, &mut bytes);
  assert_eq!(chunk.entry_type, EntryTypeV4::Chunk);
  assert_eq!(chunk.compression_algorithm, CompressionAlgorithm::None);
  assert_eq!(chunk.stored_value, graph.decoded_chunk);

  let mut retry_stream = Stream::new(&permit, seeds(&graph.head, true));
  let retry = execute(&permit, &graph.source, &mut retry_stream, &destination, &memory, &CancellationToken::new(), 96 << 20).unwrap();
  assert!(retry.idempotent_entities > 0);
  assert_eq!(retry.destination_head_tree, receipt.destination_head_tree);
  assert_eq!(retry.destination_write_sequence, receipt.destination_write_sequence);
}

#[test]
fn real_v3_clone_preserves_source_bytes_and_copies_a_compressed_file_through_short_lived_reads() {
  let source_directory = tempfile::tempdir().unwrap();
  let source_path = source_directory.path().join("source.aeordb");
  let source = StorageEngine::create(source_path.to_str().unwrap()).unwrap();
  let payload = vec![0x42; 700_000];
  DirectoryOps::new(&source)
    .store_file_compressed(
      &RequestContext::system(),
      "/archive/data.bin",
      &payload,
      Some("application/octet-stream"),
      CompressionAlgorithm::Zstd,
    )
    .unwrap();
  source.sync_writer().unwrap();
  let source_head = source.head_hash().unwrap();
  let source_identity = aeordb::engine::native_durability::platform_file_identity(&source_path).unwrap();
  let source_before = fs::read(&source_path).unwrap();
  let destination_directory = tempfile::tempdir().unwrap();
  let (permit, destination) =
    initialize_destination(&destination_directory, HashAlgorithm::Blake3_256, source_identity, source_head.clone(), counts(1, 0));
  let mut stream = Stream::new(&permit, seeds(&source_head, false));
  let memory = memory();

  let first = execute(&permit, &source, &mut stream, &destination, &memory, &CancellationToken::new(), 96 << 20).unwrap();

  assert_eq!(fs::read(&source_path).unwrap(), source_before);
  assert!(first.copied_chunk_bytes >= payload.len() as u64);
  let (source_file_identity, record) = aeordb::engine::resolve_file_at_version(&source, &source_head, "/archive/data.bin").unwrap();
  let file_key = content_key(
    HashAlgorithm::Blake3_256,
    EntryType::FileRecord,
    &record.serialize_for_version(HashAlgorithm::Blake3_256.hash_length(), aeordb::engine::CURRENT_FILE_RECORD_VERSION).unwrap(),
  );
  assert!(destination.publisher().locator(&file_key).unwrap().is_some());
  assert_ne!(source_file_identity, file_key, "real v3 namespace should exercise identity-to-content translation");
  assert!(destination.publisher().locator(&source_file_identity).unwrap().is_none());
  for chunk_hash in &record.chunk_hashes {
    let mut bytes = Vec::new();
    let chunk = read_destination_entity(&destination, chunk_hash, &mut bytes);
    assert_eq!(chunk.compression_algorithm, CompressionAlgorithm::None);
  }
  assert_destination_directory_closure(&destination, &first.destination_head_tree);

  let mut retry_stream = Stream::new(&permit, seeds(&source_head, false));
  let retry = execute(&permit, &source, &mut retry_stream, &destination, &memory, &CancellationToken::new(), 96 << 20).unwrap();
  assert!(retry.idempotent_entities > 0);
  assert_eq!(fs::read(&source_path).unwrap(), source_before);
  source.shutdown().unwrap();
}

#[test]
fn base_clone_refuses_identity_seed_closure_source_and_resource_failures_without_guessing() {
  let algorithm = HashAlgorithm::Blake3_256;
  let source_identity = identity(0x50, 0x70);
  let graph = synthetic_graph(algorithm, source_identity);

  let canceled_directory = tempfile::tempdir().unwrap();
  let (permit, destination) = initialize_destination(&canceled_directory, algorithm, source_identity, graph.head.clone(), counts(1, 0));
  let before = destination.publisher().observe().unwrap();
  let mut stream = Stream::new(&permit, seeds(&graph.head, false));
  let canceled = CancellationToken::new();
  canceled.cancel();
  assert_eq!(
    execute(&permit, &graph.source, &mut stream, &destination, &memory(), &canceled, 96 << 20).unwrap_err().code(),
    "migration_clone_canceled"
  );
  assert_eq!(destination.publisher().observe().unwrap(), before);

  let intra_file_graph = single_file_graph(algorithm, source_identity, "/cancel.bin", b"payload", false);
  let intra_file_directory = tempfile::tempdir().unwrap();
  let (permit, destination) =
    initialize_destination(&intra_file_directory, algorithm, source_identity, intra_file_graph.head.clone(), counts(1, 0));
  let cancellation = CancellationToken::new();
  let mut source = intra_file_graph.source.clone();
  source.cancel_after_file_read = Some(cancellation.clone());
  let chunk_reads = source.chunk_reads.clone();
  let mut stream = Stream::new(&permit, seeds(&intra_file_graph.head, false));
  assert_eq!(
    execute(&permit, &source, &mut stream, &destination, &memory(), &cancellation, 96 << 20).unwrap_err().code(),
    "migration_clone_canceled"
  );
  assert_eq!(chunk_reads.load(Ordering::Relaxed), 0, "canceled FileRecord traversal still loaded chunks");

  let identity_directory = tempfile::tempdir().unwrap();
  let (permit, destination) = initialize_destination(&identity_directory, algorithm, source_identity, graph.head.clone(), counts(1, 0));
  let mut foreign = graph.source.clone();
  foreign.identity = identity(0x99, 0x70);
  let mut stream = Stream::new(&permit, seeds(&graph.head, false));
  assert_eq!(
    execute(&permit, &foreign, &mut stream, &destination, &memory(), &CancellationToken::new(), 96 << 20).unwrap_err().code(),
    "migration_clone_source_identity"
  );

  let algorithm_directory = tempfile::tempdir().unwrap();
  let (permit, destination) = initialize_destination(&algorithm_directory, algorithm, source_identity, graph.head.clone(), counts(1, 0));
  let mut wrong_algorithm = graph.source.clone();
  wrong_algorithm.algorithm = HashAlgorithm::Sha256;
  let mut stream = Stream::new(&permit, seeds(&graph.head, false));
  assert_eq!(
    execute(&permit, &wrong_algorithm, &mut stream, &destination, &memory(), &CancellationToken::new(), 96 << 20).unwrap_err().code(),
    "migration_clone_hash_algorithm"
  );

  let seed_directory = tempfile::tempdir().unwrap();
  let (permit, destination) = initialize_destination(&seed_directory, algorithm, source_identity, graph.head.clone(), counts(1, 0));
  let mut malformed = seeds(&graph.head, false);
  malformed[0].path = "/wrong".to_string();
  let mut stream = Stream::new(&permit, malformed);
  assert_eq!(
    execute(&permit, &graph.source, &mut stream, &destination, &memory(), &CancellationToken::new(), 96 << 20).unwrap_err().code(),
    "migration_clone_head_seed"
  );

  let key_directory = tempfile::tempdir().unwrap();
  let (permit, destination) = initialize_destination(&key_directory, algorithm, source_identity, graph.head.clone(), counts(1, 0));
  let mut wrong_key = graph.source.clone();
  wrong_key.wrong_returned_key = Some(vec![0x66; algorithm.hash_length()]);
  let mut stream = Stream::new(&permit, seeds(&graph.head, false));
  assert_eq!(
    execute(&permit, &wrong_key, &mut stream, &destination, &memory(), &CancellationToken::new(), 96 << 20).unwrap_err().code(),
    "migration_clone_source_key_mismatch"
  );

  let changed_directory = tempfile::tempdir().unwrap();
  let (permit, destination) = initialize_destination(&changed_directory, algorithm, source_identity, graph.head.clone(), counts(1, 0));
  let mut changed = graph.source.clone();
  let mut changed_header = changed.entries.get(&graph.head).unwrap().0.clone();
  changed_header.timestamp += 1;
  changed.returned_header_override = Some(changed_header);
  let mut stream = Stream::new(&permit, seeds(&graph.head, false));
  assert_eq!(
    execute(&permit, &changed, &mut stream, &destination, &memory(), &CancellationToken::new(), 96 << 20).unwrap_err().code(),
    "migration_clone_source_changed"
  );

  let memory_directory = tempfile::tempdir().unwrap();
  let (permit, destination) = initialize_destination(&memory_directory, algorithm, source_identity, graph.head.clone(), counts(1, 0));
  let mut stream = Stream::new(&permit, seeds(&graph.head, false));
  assert_eq!(
    execute(&permit, &graph.source, &mut stream, &destination, &memory(), &CancellationToken::new(), 128).unwrap_err().code(),
    "migration_clone_memory_limit"
  );

  let closure_directory = tempfile::tempdir().unwrap();
  let (permit, destination) = initialize_destination(&closure_directory, algorithm, source_identity, graph.head.clone(), counts(1, 0));
  let before = destination.publisher().observe().unwrap();
  let mut stream = Stream::new(&permit, seeds(&graph.head, false));
  stream.closure.source_header_sequence += 1;
  assert_eq!(
    execute(&permit, &graph.source, &mut stream, &destination, &memory(), &CancellationToken::new(), 96 << 20).unwrap_err().code(),
    "migration_clone_source_basis_mismatch"
  );
  assert_eq!(destination.publisher().observe().unwrap(), before, "small clone published before closing its source inventory");

  let sink_directory = tempfile::tempdir().unwrap();
  let (permit, destination) = initialize_destination(&sink_directory, algorithm, source_identity, graph.head.clone(), counts(1, 0));
  let initial_head = destination.publisher().observe().unwrap().selected.header.head_hash;
  let mut stream = Stream::new(&permit, seeds(&graph.head, false));
  let mut seed_results = SeedResultSink { fail: true };
  let error = execute_migration_base_clone_v1(MigrationBaseCloneExecutionRequestV1 {
    permit: &permit,
    source: &graph.source,
    seeds: &mut stream,
    seed_results: &mut seed_results,
    destination: destination.publisher(),
    memory: &memory(),
    cancellation: &CancellationToken::new(),
    publication_timestamp_ms: 1_700_000_000_100,
    maximum_work_items: 1_000_000,
    maximum_memory_bytes: 96 << 20,
    maximum_decoded_chunk_bytes: 1024 * 1024,
    maximum_directory_depth: 1_000,
  })
  .unwrap_err();
  assert_eq!(error.code(), "migration_clone_seed_result_error");
  assert_eq!(destination.publisher().observe().unwrap().selected.header.head_hash, initial_head);
}

#[test]
fn base_clone_translates_required_copy_system_chunks_without_preserving_v3_physical_flags() {
  let algorithm = HashAlgorithm::Blake3_256;
  let source_identity = identity(0x50, 0x70);
  let graph = single_file_graph(algorithm, source_identity, "/.aeordb-config/indexes.json", br#"{"fields":["title"]}"#, true);
  let destination_directory = tempfile::tempdir().unwrap();
  let (permit, destination) = initialize_destination(&destination_directory, algorithm, source_identity, graph.head.clone(), counts(1, 0));
  let mut seed_rows = seeds(&graph.head, false);
  seed_rows.push(MigrationBaseCloneSeedV1 {
    kind: MigrationBaseCloneSeedKindV1::DetachedProtectedPath,
    path: graph.record.path.clone(),
    entry_type: EntryType::FileRecord,
    hash: graph.file_identity.clone(),
  });
  let mut stream = Stream::new(&permit, seed_rows);

  let receipt = execute(&permit, &graph.source, &mut stream, &destination, &memory(), &CancellationToken::new(), 96 << 20).unwrap();

  assert!(receipt.plan.copy_items >= 2);
  assert!(destination.publisher().locator(&graph.file_content).unwrap().is_some());
  assert!(destination.publisher().locator(&graph.destination_chunk).unwrap().is_some());
  assert_eq!(graph.source_chunk, graph.destination_chunk, "current v3 writers use the ordinary chunk identity with a system flag");
  assert!(destination.publisher().locator(&graph.file_identity).unwrap().is_none());
  assert_eq!(graph.source_file_content, graph.file_content);
  let mut bytes = Vec::new();
  let stored = read_destination_entity(&destination, &graph.file_content, &mut bytes);
  assert_eq!(stored.flags, 0);
  let decoded = FileRecord::deserialize(stored.stored_value, algorithm.hash_length(), stored.entity_version).unwrap();
  assert_eq!(decoded.chunk_hashes, vec![graph.destination_chunk]);
  assert_eq!(decoded.content_hash, graph.record.content_hash);

  let legacy = single_file_graph_with_legacy_system_chunk(
    algorithm,
    source_identity,
    "/.aeordb-config/indexes.json",
    br#"{"legacy":true}"#,
    true,
    true,
  );
  let legacy_directory = tempfile::tempdir().unwrap();
  let (permit, destination) = initialize_destination(&legacy_directory, algorithm, source_identity, legacy.head.clone(), counts(1, 0));
  let mut seed_rows = seeds(&legacy.head, false);
  seed_rows.push(MigrationBaseCloneSeedV1 {
    kind: MigrationBaseCloneSeedKindV1::DetachedProtectedPath,
    path: legacy.record.path.clone(),
    entry_type: EntryType::FileRecord,
    hash: legacy.file_identity.clone(),
  });
  let mut stream = Stream::new(&permit, seed_rows);
  execute(&permit, &legacy.source, &mut stream, &destination, &memory(), &CancellationToken::new(), 96 << 20).unwrap();
  assert_ne!(legacy.source_chunk, legacy.destination_chunk);
  assert_ne!(legacy.source_file_content, legacy.file_content);
  assert!(destination.publisher().locator(&legacy.destination_chunk).unwrap().is_some());
  assert!(destination.publisher().locator(&legacy.source_chunk).unwrap().is_none());
  assert!(destination.publisher().locator(&legacy.file_content).unwrap().is_some());
  assert!(destination.publisher().locator(&legacy.source_file_content).unwrap().is_none());
}

#[test]
fn base_clone_preserves_a_valid_v0_file_record_without_inventing_v1_fields() {
  let algorithm = HashAlgorithm::Blake3_256;
  let source_identity = identity(0x50, 0x70);
  let mut graph = single_file_graph(algorithm, source_identity, "/legacy.bin", b"legacy payload", false);
  let v0_value = graph.record.serialize_v0(algorithm.hash_length()).unwrap();
  let destination_file = content_key(algorithm, EntryType::FileRecord, &v0_value);
  graph.source.replace_value(&graph.file_identity, v0_value.clone());
  graph.source.entries.get_mut(&graph.file_identity).unwrap().0.entry_version = 0;
  let destination_directory = tempfile::tempdir().unwrap();
  let (permit, destination) = initialize_destination(&destination_directory, algorithm, source_identity, graph.head.clone(), counts(1, 0));
  let mut stream = Stream::new(&permit, seeds(&graph.head, false));

  execute(&permit, &graph.source, &mut stream, &destination, &memory(), &CancellationToken::new(), 96 << 20).unwrap();

  let mut bytes = Vec::new();
  let stored = read_destination_entity(&destination, &destination_file, &mut bytes);
  assert_eq!(stored.entity_version, 0);
  assert_eq!(stored.stored_value, v0_value);
  let decoded = FileRecord::deserialize(stored.stored_value, algorithm.hash_length(), stored.entity_version).unwrap();
  assert!(decoded.content_hash.is_empty());
  assert_eq!(decoded.chunk_hashes, graph.record.chunk_hashes);
}

#[test]
fn base_clone_rebuilds_parent_metadata_from_translated_children() {
  let algorithm = HashAlgorithm::Blake3_256;
  let source_identity = identity(0x50, 0x70);
  let mut source = FakeSource::new(algorithm, source_identity);
  let empty_content_hash = digest_parts(algorithm, &[b""]);
  let file = FileRecord {
    path: "/file.txt".to_string(),
    content_type: Some("text/plain".to_string()),
    total_size: 0,
    created_at: 111,
    updated_at: 222,
    metadata: Vec::new(),
    content_hash: empty_content_hash,
    chunk_hashes: Vec::new(),
  };
  let file_value = file.serialize(algorithm.hash_length()).unwrap();
  let file_identity = file_identity_key(algorithm, &file);
  let destination_file = content_key(algorithm, EntryType::FileRecord, &file_value);
  source.insert(EntryType::FileRecord, 1, CompressionAlgorithm::None, file_identity.clone(), file_value);

  let symlink = SymlinkRecord { path: "/link".to_string(), target: "/file.txt".to_string(), created_at: 333, updated_at: 444 };
  let symlink_value = symlink.serialize().unwrap();
  let symlink_identity = digest_parts(algorithm, &[b"symlinkid:", symlink.path.as_bytes(), &[0], symlink.target.as_bytes()]);
  let destination_symlink = content_key(algorithm, EntryType::Symlink, &symlink_value);
  source.insert(EntryType::Symlink, 0, CompressionAlgorithm::None, symlink_identity.clone(), symlink_value);

  let empty_directory = content_key(algorithm, EntryType::DirectoryIndex, &[]);
  source.insert(EntryType::DirectoryIndex, 0, CompressionAlgorithm::None, empty_directory.clone(), Vec::new());
  let mut directory_child = child("dir", EntryType::DirectoryIndex, empty_directory.clone(), 999);
  directory_child.content_type = Some("wrong/directory".to_string());
  let mut file_child = child("file.txt", EntryType::FileRecord, file_identity, 999);
  file_child.content_type = Some("wrong/file".to_string());
  file_child.created_at = -1;
  file_child.updated_at = -2;
  let mut symlink_child = child("link", EntryType::Symlink, symlink_identity, 999);
  symlink_child.content_type = Some("wrong/link".to_string());
  symlink_child.created_at = -3;
  symlink_child.updated_at = -4;
  let root_value = serialize_child_entries(&[directory_child, file_child, symlink_child], algorithm.hash_length()).unwrap();
  let head = content_key(algorithm, EntryType::DirectoryIndex, &root_value);
  source.insert(EntryType::DirectoryIndex, 0, CompressionAlgorithm::None, head.clone(), root_value);
  let destination_directory = tempfile::tempdir().unwrap();
  let (permit, destination) = initialize_destination(&destination_directory, algorithm, source_identity, head.clone(), counts(1, 0));
  let mut stream = Stream::new(&permit, seeds(&head, false));

  let receipt = execute(&permit, &source, &mut stream, &destination, &memory(), &CancellationToken::new(), 96 << 20).unwrap();

  let mut bytes = Vec::new();
  let root = read_destination_entity(&destination, &receipt.destination_head_tree, &mut bytes);
  let children = deserialize_child_entries(root.stored_value, algorithm.hash_length(), root.entity_version).unwrap();
  assert_eq!(children.len(), 3);
  assert_eq!(
    (children[0].name.as_str(), children[0].hash.as_slice(), children[0].total_size, children[0].content_type.as_deref()),
    ("dir", empty_directory.as_slice(), 0, None)
  );
  assert_eq!((children[1].name.as_str(), children[1].hash.as_slice()), ("file.txt", destination_file.as_slice()));
  assert_eq!(
    (children[1].total_size, children[1].content_type.as_deref(), children[1].created_at, children[1].updated_at),
    (0, Some("text/plain"), 111, 222)
  );
  assert_eq!((children[2].name.as_str(), children[2].hash.as_slice()), ("link", destination_symlink.as_slice()));
  assert_eq!(
    (children[2].total_size, children[2].content_type.as_deref(), children[2].created_at, children[2].updated_at),
    (0, None, 333, 444)
  );
}

#[test]
fn base_clone_rejects_file_and_chunk_integrity_drift_before_publication() {
  let algorithm = HashAlgorithm::Blake3_256;
  let source_identity = identity(0x50, 0x70);

  let mut graph = single_file_graph(algorithm, source_identity, "/data.bin", b"payload", false);
  graph.record.total_size += 1;
  graph.source.replace_value(&graph.file_identity, graph.record.serialize(algorithm.hash_length()).unwrap());
  assert_clone_error(&graph.source, &graph.head, seeds(&graph.head, false), "migration_clone_file_size");

  let mut graph = single_file_graph(algorithm, source_identity, "/data.bin", b"payload", false);
  graph.record.content_hash.fill(0x77);
  graph.source.replace_value(&graph.file_identity, graph.record.serialize(algorithm.hash_length()).unwrap());
  assert_clone_error(&graph.source, &graph.head, seeds(&graph.head, false), "migration_clone_file_content_hash");

  let mut graph = single_file_graph(algorithm, source_identity, "/data.bin", b"payload", false);
  graph.source.entries.remove(&graph.source_chunk);
  assert_clone_error(&graph.source, &graph.head, seeds(&graph.head, false), "migration_clone_missing_chunk");

  let mut graph = single_file_graph(algorithm, source_identity, "/data.bin", b"payload", false);
  graph.source.entries.get_mut(&graph.source_chunk).unwrap().0.flags = 0x02;
  assert_clone_error(&graph.source, &graph.head, seeds(&graph.head, false), "migration_clone_source_representation");

  let mut graph = single_file_graph(algorithm, source_identity, "/data.bin", b"payload", false);
  graph.source.replace_value(&graph.source_chunk, b"tampered".to_vec());
  assert_clone_error(&graph.source, &graph.head, seeds(&graph.head, false), "migration_clone_chunk_identity");

  let mut graph = single_file_graph(algorithm, source_identity, "/data.bin", b"payload", false);
  let mut trailing = graph.record.serialize(algorithm.hash_length()).unwrap();
  trailing.push(0xaa);
  graph.source.replace_value(&graph.file_identity, trailing);
  assert_clone_error(&graph.source, &graph.head, seeds(&graph.head, false), "migration_clone_file_record_trailing");

  let mut graph = single_file_graph(algorithm, source_identity, "/data.bin", b"payload", false);
  graph.source.entries.get_mut(&graph.file_identity).unwrap().0.entry_version = 2;
  assert_clone_error(&graph.source, &graph.head, seeds(&graph.head, false), "migration_clone_file_record_version");

  let mut graph = single_file_graph(algorithm, source_identity, "/data.bin", b"payload", false);
  graph.source.entries.get_mut(&graph.source_chunk).unwrap().0.value_length = u32::MAX;
  assert_clone_error(&graph.source, &graph.head, seeds(&graph.head, false), "migration_clone_chunk_stored_bound");

  let mut graph = single_file_graph(algorithm, source_identity, "/data.bin", b"payload", false);
  graph.source.entries.get_mut(&graph.source_chunk).unwrap().0.entry_version = 1;
  assert_clone_error(&graph.source, &graph.head, seeds(&graph.head, false), "migration_clone_chunk_version");
}

#[test]
fn base_clone_rejects_malformed_directory_and_btree_structures_without_scanning_forever() {
  let algorithm = HashAlgorithm::Blake3_256;
  let source_identity = identity(0x50, 0x70);

  let mut graph = synthetic_graph(algorithm, source_identity);
  graph.source.entries.get_mut(&graph.head).unwrap().0.entry_type = EntryType::FileRecord;
  assert_clone_error(&graph.source, &graph.head, seeds(&graph.head, false), "migration_clone_source_type_mismatch");

  let mut graph = synthetic_graph(algorithm, source_identity);
  graph.source.entries.get_mut(&graph.head).unwrap().0.flags = FLAG_SYSTEM;
  assert_clone_error(&graph.source, &graph.head, seeds(&graph.head, false), "migration_clone_source_flags");

  let mut graph = synthetic_graph(algorithm, source_identity);
  graph.source.entries.get_mut(&graph.head).unwrap().0.compression_algo = CompressionAlgorithm::Zstd;
  assert_clone_error(&graph.source, &graph.head, seeds(&graph.head, false), "migration_clone_directory_compression");

  let empty_v1_head = content_key(algorithm, EntryType::DirectoryIndex, &[]);
  let mut empty_v1_source = FakeSource::new(algorithm, source_identity);
  empty_v1_source.insert(EntryType::DirectoryIndex, 1, CompressionAlgorithm::None, empty_v1_head.clone(), Vec::new());
  assert_clone_error(&empty_v1_source, &empty_v1_head, seeds(&empty_v1_head, false), "migration_clone_directory_version");

  let mut graph = synthetic_graph(algorithm, source_identity);
  let stale_value =
    serialize_child_entries(&[child("missing", EntryType::FileRecord, vec![0x55; algorithm.hash_length()], 1)], algorithm.hash_length())
      .unwrap();
  graph.source.replace_value(&graph.head, stale_value);
  assert_clone_error(&graph.source, &graph.head, seeds(&graph.head, false), "migration_clone_directory_identity");

  let mut trailing_source = FakeSource::new(algorithm, source_identity);
  let mut trailing = BTreeNode::Internal(InternalNode { keys: Vec::new(), children: vec![vec![0x22; algorithm.hash_length()]] })
    .serialize(algorithm.hash_length())
    .unwrap();
  trailing.push(0xee);
  let trailing_head = content_key(algorithm, EntryType::DirectoryIndex, &trailing);
  trailing_source.insert(EntryType::DirectoryIndex, 0, CompressionAlgorithm::None, trailing_head.clone(), trailing);
  assert_clone_error(&trailing_source, &trailing_head, seeds(&trailing_head, false), "migration_clone_btree_noncanonical");

  let mut unordered_source = FakeSource::new(algorithm, source_identity);
  let unordered = BTreeNode::Leaf(LeafNode {
    entries: vec![
      child("z", EntryType::FileRecord, vec![0x31; algorithm.hash_length()], 1),
      child("a", EntryType::FileRecord, vec![0x32; algorithm.hash_length()], 1),
    ],
  })
  .serialize(algorithm.hash_length())
  .unwrap();
  let unordered_head = content_key(algorithm, EntryType::DirectoryIndex, &unordered);
  unordered_source.insert(EntryType::DirectoryIndex, 0, CompressionAlgorithm::None, unordered_head.clone(), unordered);
  assert_clone_error(&unordered_source, &unordered_head, seeds(&unordered_head, false), "migration_clone_btree_leaf");

  let mut unordered_flat_source = FakeSource::new(algorithm, source_identity);
  let empty_content_hash = digest_parts(algorithm, &[b""]);
  let mut unordered_flat_children = Vec::new();
  for name in ["z", "a"] {
    let path = format!("/{name}");
    let record = FileRecord {
      path: path.clone(),
      content_type: Some("application/octet-stream".to_string()),
      total_size: 0,
      created_at: 1_700_000_000_001,
      updated_at: 1_700_000_000_001,
      metadata: Vec::new(),
      content_hash: empty_content_hash.clone(),
      chunk_hashes: Vec::new(),
    };
    let value = record.serialize(algorithm.hash_length()).unwrap();
    let key = file_identity_key(algorithm, &record);
    unordered_flat_source.insert(EntryType::FileRecord, 1, CompressionAlgorithm::None, key.clone(), value);
    unordered_flat_children.push(child(name, EntryType::FileRecord, key, 0));
  }
  let unordered_flat = serialize_child_entries(&unordered_flat_children, algorithm.hash_length()).unwrap();
  let unordered_flat_head = content_key(algorithm, EntryType::DirectoryIndex, &unordered_flat);
  unordered_flat_source.insert(EntryType::DirectoryIndex, 0, CompressionAlgorithm::None, unordered_flat_head.clone(), unordered_flat);
  let unordered_flat_directory = tempfile::tempdir().unwrap();
  let (permit, destination) =
    initialize_destination(&unordered_flat_directory, algorithm, source_identity, unordered_flat_head.clone(), counts(1, 0));
  let mut stream = Stream::new(&permit, seeds(&unordered_flat_head, false));
  let receipt =
    execute(&permit, &unordered_flat_source, &mut stream, &destination, &memory(), &CancellationToken::new(), 96 << 20).unwrap();
  let mut bytes = Vec::new();
  let root = read_destination_entity(&destination, &receipt.destination_head_tree, &mut bytes);
  let children = deserialize_child_entries(root.stored_value, algorithm.hash_length(), root.entity_version).unwrap();
  assert_eq!(children.iter().map(|child| child.name.as_str()).collect::<Vec<_>>(), ["a", "z"]);

  let mut duplicate_flat_source = unordered_flat_source.clone();
  let duplicate_flat = serialize_child_entries(
    &[
      unordered_flat_children[0].clone(),
      ChildEntry { name: unordered_flat_children[0].name.clone(), ..unordered_flat_children[1].clone() },
    ],
    algorithm.hash_length(),
  )
  .unwrap();
  let duplicate_flat_head = content_key(algorithm, EntryType::DirectoryIndex, &duplicate_flat);
  duplicate_flat_source.insert(EntryType::DirectoryIndex, 0, CompressionAlgorithm::None, duplicate_flat_head.clone(), duplicate_flat);
  assert_clone_error(
    &duplicate_flat_source,
    &duplicate_flat_head,
    seeds(&duplicate_flat_head, false),
    "migration_clone_flat_directory_duplicate",
  );

  let mut duplicate_child_source = FakeSource::new(algorithm, source_identity);
  let empty_leaf = BTreeNode::Leaf(LeafNode { entries: Vec::new() }).serialize(algorithm.hash_length()).unwrap();
  let empty_leaf_key = content_key(algorithm, EntryType::DirectoryIndex, &empty_leaf);
  duplicate_child_source.insert(EntryType::DirectoryIndex, 0, CompressionAlgorithm::None, empty_leaf_key.clone(), empty_leaf);
  let duplicate_child_root =
    BTreeNode::Internal(InternalNode { keys: vec!["m".to_string()], children: vec![empty_leaf_key.clone(), empty_leaf_key] })
      .serialize(algorithm.hash_length())
      .unwrap();
  let duplicate_child_head = content_key(algorithm, EntryType::DirectoryIndex, &duplicate_child_root);
  duplicate_child_source.insert(
    EntryType::DirectoryIndex,
    0,
    CompressionAlgorithm::None,
    duplicate_child_head.clone(),
    duplicate_child_root,
  );
  assert_clone_error(
    &duplicate_child_source,
    &duplicate_child_head,
    seeds(&duplicate_child_head, false),
    "migration_clone_btree_duplicate_child",
  );

  let mut range_source = FakeSource::new(algorithm, source_identity);
  let left = BTreeNode::Leaf(LeafNode { entries: vec![child("z", EntryType::FileRecord, vec![0x41; algorithm.hash_length()], 1)] })
    .serialize(algorithm.hash_length())
    .unwrap();
  let left_key = content_key(algorithm, EntryType::DirectoryIndex, &left);
  range_source.insert(EntryType::DirectoryIndex, 0, CompressionAlgorithm::None, left_key.clone(), left);
  let right = BTreeNode::Leaf(LeafNode { entries: vec![child("n", EntryType::FileRecord, vec![0x42; algorithm.hash_length()], 1)] })
    .serialize(algorithm.hash_length())
    .unwrap();
  let right_key = content_key(algorithm, EntryType::DirectoryIndex, &right);
  range_source.insert(EntryType::DirectoryIndex, 0, CompressionAlgorithm::None, right_key.clone(), right);
  let range_root = BTreeNode::Internal(InternalNode { keys: vec!["m".to_string()], children: vec![left_key, right_key] })
    .serialize(algorithm.hash_length())
    .unwrap();
  let range_head = content_key(algorithm, EntryType::DirectoryIndex, &range_root);
  range_source.insert(EntryType::DirectoryIndex, 0, CompressionAlgorithm::None, range_head.clone(), range_root);
  assert_clone_error(&range_source, &range_head, seeds(&range_head, false), "migration_clone_btree_range");

  let cycle_head = digest_parts(algorithm, &[b"dir:/"]);
  let cycle_value =
    BTreeNode::Internal(InternalNode { keys: Vec::new(), children: vec![cycle_head.clone()] }).serialize(algorithm.hash_length()).unwrap();
  let mut cycle_source = FakeSource::new(algorithm, source_identity);
  cycle_source.insert(EntryType::DirectoryIndex, 0, CompressionAlgorithm::None, cycle_head.clone(), cycle_value);
  assert_clone_error(&cycle_source, &cycle_head, seeds(&cycle_head, false), "migration_clone_btree_cycle_or_depth");
}

#[test]
fn base_clone_validates_seed_stream_and_execution_limits_before_guessing() {
  let algorithm = HashAlgorithm::Blake3_256;
  let source_identity = identity(0x50, 0x70);
  let graph = synthetic_graph(algorithm, source_identity);

  assert_clone_error(&graph.source, &graph.head, Vec::new(), "migration_clone_head_missing");
  let mut duplicate = seeds(&graph.head, false);
  duplicate.extend(seeds(&graph.head, false));
  assert_clone_error(&graph.source, &graph.head, duplicate, "migration_clone_head_duplicate");
  let before_head = vec![MigrationBaseCloneSeedV1 {
    kind: MigrationBaseCloneSeedKindV1::Snapshot,
    path: "/".to_string(),
    entry_type: EntryType::DirectoryIndex,
    hash: graph.head.clone(),
  }];
  assert_clone_error(&graph.source, &graph.head, before_head, "migration_clone_retained_root_seed");
  let mut root_detached = seeds(&graph.head, false);
  root_detached.push(MigrationBaseCloneSeedV1 {
    kind: MigrationBaseCloneSeedKindV1::DetachedProtectedPath,
    path: "/".to_string(),
    entry_type: EntryType::DirectoryIndex,
    hash: graph.head.clone(),
  });
  assert_clone_error(&graph.source, &graph.head, root_detached, "migration_clone_detached_seed");

  let stream_directory = tempfile::tempdir().unwrap();
  let (permit, destination) = initialize_destination(&stream_directory, algorithm, source_identity, graph.head.clone(), counts(1, 0));
  let mut failed_stream = Stream::new(&permit, seeds(&graph.head, false));
  failed_stream.fail_next = true;
  assert_eq!(
    execute(&permit, &graph.source, &mut failed_stream, &destination, &memory(), &CancellationToken::new(), 96 << 20).unwrap_err().code(),
    "migration_clone_source_error"
  );

  let limit_directory = tempfile::tempdir().unwrap();
  let (permit, destination) = initialize_destination(&limit_directory, algorithm, source_identity, graph.head.clone(), counts(1, 0));
  let mut stream = Stream::new(&permit, seeds(&graph.head, false));
  assert_eq!(
    execute_with_limits(
      &permit,
      &graph.source,
      &mut stream,
      &destination,
      &memory(),
      &CancellationToken::new(),
      1,
      96 << 20,
      1024 * 1024,
      1_000,
    )
    .unwrap_err()
    .code(),
    "migration_clone_work_limit"
  );

  let invalid_directory = tempfile::tempdir().unwrap();
  let (permit, destination) = initialize_destination(&invalid_directory, algorithm, source_identity, graph.head.clone(), counts(1, 0));
  let mut stream = Stream::new(&permit, seeds(&graph.head, false));
  assert_eq!(
    execute_with_limits(
      &permit,
      &graph.source,
      &mut stream,
      &destination,
      &memory(),
      &CancellationToken::new(),
      0,
      96 << 20,
      1024 * 1024,
      1_000,
    )
    .unwrap_err()
    .code(),
    "migration_clone_limits"
  );
}

#[test]
fn base_clone_rejects_an_oversized_decoded_chunk_bound_before_source_reads_or_destination_mutation() {
  let algorithm = HashAlgorithm::Blake3_256;
  let source_identity = identity(0x50, 0x70);
  let graph = single_file_graph(algorithm, source_identity, "/bounded.bin", b"payload", false);
  let destination_directory = tempfile::tempdir().unwrap();
  let (permit, destination) = initialize_destination(&destination_directory, algorithm, source_identity, graph.head.clone(), counts(1, 0));
  let before = destination.publisher().observe().unwrap();
  let chunk_reads = graph.source.chunk_reads.clone();
  let mut stream = Stream::new(&permit, seeds(&graph.head, false));

  let error = execute_with_limits(
    &permit,
    &graph.source,
    &mut stream,
    &destination,
    &memory(),
    &CancellationToken::new(),
    1_000_000,
    96 << 20,
    (64 * 1024 * 1024) + 1,
    1_000,
  )
  .unwrap_err();

  assert_eq!(error.code(), "migration_clone_limits");
  assert_eq!(chunk_reads.load(Ordering::Relaxed), 0);
  assert_eq!(destination.publisher().observe().unwrap(), before);
}

#[test]
fn base_clone_flushes_more_than_511_entities_in_bounded_idempotent_batches() {
  let algorithm = HashAlgorithm::Blake3_256;
  let source_identity = identity(0x50, 0x70);
  let mut source = FakeSource::new(algorithm, source_identity);
  let head = content_key(algorithm, EntryType::DirectoryIndex, &[]);
  source.insert(EntryType::DirectoryIndex, 0, CompressionAlgorithm::None, head.clone(), Vec::new());
  let mut seed_rows = seeds(&head, false);
  let empty_hash = digest_parts(algorithm, &[b""]);
  for index in 0..512 {
    let path = format!("/bulk-{index:04}.json");
    let record = FileRecord {
      path: path.clone(),
      content_type: Some("application/json".to_string()),
      total_size: 0,
      created_at: 1_700_000_000_001,
      updated_at: 1_700_000_000_001,
      metadata: Vec::new(),
      content_hash: empty_hash.clone(),
      chunk_hashes: Vec::new(),
    };
    let value = record.serialize(algorithm.hash_length()).unwrap();
    let identity = file_identity_key(algorithm, &record);
    source.insert(EntryType::FileRecord, 1, CompressionAlgorithm::None, identity.clone(), value);
    seed_rows.push(MigrationBaseCloneSeedV1 {
      kind: MigrationBaseCloneSeedKindV1::DetachedProtectedPath,
      path,
      entry_type: EntryType::FileRecord,
      hash: identity,
    });
  }
  let destination_directory = tempfile::tempdir().unwrap();
  let (permit, destination) = initialize_destination(&destination_directory, algorithm, source_identity, head, counts(1, 0));
  let initial_head = destination.publisher().observe().unwrap().selected.header.head_hash;
  let mut stream = Stream::new(&permit, seed_rows);

  let receipt = execute(&permit, &source, &mut stream, &destination, &memory(), &CancellationToken::new(), 96 << 20).unwrap();

  assert_eq!(receipt.published_entities, 513);
  assert_eq!(receipt.maximum_batch_entities, 511);
  assert!(receipt.destination_header_sequence >= 3);
  assert_eq!(destination.publisher().observe().unwrap().selected.header.head_hash, initial_head);
}

#[test]
fn base_clone_execution_remains_disconnected_from_whole_tree_snapshots_and_runtime_activation() {
  let package = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
  let source = fs::read_to_string(package.join("src/engine/v4/migration_base_clone_execution.rs")).unwrap();
  let source_adapter = fs::read_to_string(package.join("src/engine/v4/migration_base_clone_source.rs")).unwrap();
  for required in [
    "MigrationBaseClonePlannerV1",
    "publish_immutable_entity_batch",
    "historical_entry_verified_bounded",
    "visit_bounded_child_entries",
    "decompress_bounded",
    "MemoryOwner::Migration",
  ] {
    assert!(source.contains(required), "base clone omitted shared owner {required}");
  }
  for forbidden in [
    "walk_version_tree",
    "VersionTree",
    "ReadSnapshot",
    "kv_snapshot",
    "entries_by_type",
    "HashSet",
    "HashMap",
    "DirectoryOps",
    "store_entry(",
    "flush_batch(",
    "publish_successor_authority",
    "crate::server",
    "axum",
    "tokio::spawn",
    "remove_file(",
    "rename(",
  ] {
    assert!(!source.contains(forbidden), "base clone gained forbidden whole-tree/runtime authority {forbidden}");
  }
  let planner = fs::read_to_string(package.join("src/engine/v4/migration_clone.rs")).unwrap();
  assert!(!planner.contains("migration_base_clone_execution"), "pure policy planner depends on execution I/O");
  assert!(source_adapter.contains("impl MigrationBaseCloneEntrySourceV1 for StorageEngine"));
  for forbidden in ["V4FirstAuthorityPublisher", "publish_immutable_entity_batch", "crate::server", "axum", "tokio::spawn"] {
    assert!(!source_adapter.contains(forbidden), "source adapter gained destination or runtime activation token {forbidden}");
  }
}
