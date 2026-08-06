use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::durability_coordinator::{
  CommitClass, DurabilityCommitPlan, DurabilityCoordinator, DurabilityCoordinatorError, DurabilityFailureDisposition,
  DurabilityGroupExecutor, DurabilityOperation, DurabilityTicket, DurabilityWaiterState, classify_native_durability_error,
};
use crate::engine::native_durability::{
  NativeDurabilityError, NativeDurabilityOperation, sync_file_data_native, verify_file_bytes_native, write_file_at_native,
};

/// Size of a single header slot. The on-disk layout has TWO of these (slot A
/// at byte 0, slot B at byte FILE_HEADER_SIZE). Most callsites that need
/// "size of a header buffer" want this constant; sites that need "where the
/// data region starts" want [`HEADER_REGION_SIZE`].
pub const FILE_HEADER_SIZE: usize = 256;

/// Total size of the header region — both slots combined. Data (KV block,
/// WAL) starts at this offset.
pub const HEADER_REGION_SIZE: usize = FILE_HEADER_SIZE * 2;

pub const FILE_MAGIC: &[u8; 4] = b"AEOR";

/// CRC32 size in bytes, at the tail of each slot. Bytes [FILE_HEADER_SIZE-4
/// .. FILE_HEADER_SIZE] hold the CRC32 computed over the first 252 bytes of
/// the slot.
const HEADER_CRC_SIZE: usize = 4;

/// Header format version this build understands. Bumping this is a
/// commitment: every future change to the on-disk header layout must increment
/// and provide a clear error to readers of an unknown version.
///
/// v1 (legacy): single 256-byte header, no CRC. No DBs in the wild.
/// v2: single 256-byte header with CRC32 in the last 4 bytes. Catches torn
///     writes that pass magic+version but leave later bytes garbled.
/// v3: two 256-byte slots at bytes 0 and 256, data starts at byte 512. Each
///     slot carries a u64 sequence number + CRC32. Writes alternate slots —
///     a torn write to one slot leaves the other intact. Readers pick the
///     highest sequence with a valid CRC. The CRC field added in v2 is the
///     prerequisite for picking the live slot.
pub const SUPPORTED_HEADER_VERSION: u8 = 3;

#[derive(Debug, Clone)]
pub struct FileHeader {
  pub header_version: u8,
  pub hash_algo: HashAlgorithm,
  /// Monotonically-increasing sequence number for A/B slot selection.
  /// Every `update_header` increments this by 1. On read the slot with the
  /// higher valid sequence wins.
  pub sequence: u64,
  pub created_at: i64,
  pub updated_at: i64,
  pub kv_block_offset: u64,
  pub kv_block_length: u64,
  pub kv_block_version: u8,
  pub nvt_offset: u64,
  pub nvt_length: u64,
  pub nvt_version: u8,
  pub head_hash: Vec<u8>,
  pub entry_count: u64,
  pub resize_in_progress: bool,
  pub buffer_kvs_offset: u64,
  pub buffer_nvt_offset: u64,
  pub hot_tail_offset: u64,
  pub kv_block_stage: u8,
  pub resize_target_stage: u8,
  pub backup_type: u8,      // 0=normal, 1=full export, 2=patch
  pub base_hash: Vec<u8>,   // source version hash
  pub target_hash: Vec<u8>, // destination version hash
}

impl FileHeader {
  pub fn new(hash_algo: HashAlgorithm) -> Self {
    let now = chrono::Utc::now().timestamp_millis();

    let hash_length = hash_algo.hash_length();

    FileHeader {
      header_version: SUPPORTED_HEADER_VERSION,
      hash_algo,
      sequence: 0,
      created_at: now,
      updated_at: now,
      kv_block_offset: 0,
      kv_block_length: 0,
      kv_block_version: 1,
      nvt_offset: 0,
      nvt_length: 0,
      nvt_version: 1,
      head_hash: vec![0u8; hash_length],
      entry_count: 0,
      resize_in_progress: false,
      buffer_kvs_offset: 0,
      buffer_nvt_offset: 0,
      hot_tail_offset: 0,
      kv_block_stage: 0,
      resize_target_stage: 0,
      backup_type: 0,
      base_hash: vec![0u8; hash_length],
      target_hash: vec![0u8; hash_length],
    }
  }

  pub fn serialize(&self) -> [u8; FILE_HEADER_SIZE] {
    let mut buffer = [0u8; FILE_HEADER_SIZE];
    let mut offset = 0;

    // magic: 4 bytes
    buffer[offset..offset + 4].copy_from_slice(FILE_MAGIC);
    offset += 4;

    // header_version: 1 byte
    buffer[offset] = self.header_version;
    offset += 1;

    // hash_algo: 2 bytes
    buffer[offset..offset + 2].copy_from_slice(&self.hash_algo.to_u16().to_le_bytes());
    offset += 2;

    // sequence: 8 bytes — v3 A/B slot selector
    buffer[offset..offset + 8].copy_from_slice(&self.sequence.to_le_bytes());
    offset += 8;

    // created_at: 8 bytes
    buffer[offset..offset + 8].copy_from_slice(&self.created_at.to_le_bytes());
    offset += 8;

    // updated_at: 8 bytes
    buffer[offset..offset + 8].copy_from_slice(&self.updated_at.to_le_bytes());
    offset += 8;

    // kv_block_offset: 8 bytes
    buffer[offset..offset + 8].copy_from_slice(&self.kv_block_offset.to_le_bytes());
    offset += 8;

    // kv_block_length: 8 bytes
    buffer[offset..offset + 8].copy_from_slice(&self.kv_block_length.to_le_bytes());
    offset += 8;

    // kv_block_version: 1 byte
    buffer[offset] = self.kv_block_version;
    offset += 1;

    // nvt_offset: 8 bytes
    buffer[offset..offset + 8].copy_from_slice(&self.nvt_offset.to_le_bytes());
    offset += 8;

    // nvt_length: 8 bytes
    buffer[offset..offset + 8].copy_from_slice(&self.nvt_length.to_le_bytes());
    offset += 8;

    // nvt_version: 1 byte
    buffer[offset] = self.nvt_version;
    offset += 1;

    // head_hash: dynamic length (hash_algo.hash_length() bytes)
    let hash_length = self.hash_algo.hash_length();
    let copy_length = hash_length.min(self.head_hash.len());
    buffer[offset..offset + copy_length].copy_from_slice(&self.head_hash[..copy_length]);
    offset += hash_length;

    // entry_count: 8 bytes
    buffer[offset..offset + 8].copy_from_slice(&self.entry_count.to_le_bytes());
    offset += 8;

    // resize_in_progress: 1 byte
    buffer[offset] = if self.resize_in_progress { 1 } else { 0 };
    offset += 1;

    // buffer_kvs_offset: 8 bytes
    buffer[offset..offset + 8].copy_from_slice(&self.buffer_kvs_offset.to_le_bytes());
    offset += 8;

    // buffer_nvt_offset: 8 bytes
    buffer[offset..offset + 8].copy_from_slice(&self.buffer_nvt_offset.to_le_bytes());
    offset += 8;

    // hot_tail_offset: 8 bytes
    buffer[offset..offset + 8].copy_from_slice(&self.hot_tail_offset.to_le_bytes());
    offset += 8;

    // kv_block_stage: 1 byte
    buffer[offset] = self.kv_block_stage;
    offset += 1;

    // resize_target_stage: 1 byte
    buffer[offset] = self.resize_target_stage;
    offset += 1;

    // backup_type: 1 byte
    buffer[offset] = self.backup_type;
    offset += 1;

    // base_hash: hash_length bytes
    let copy_len = hash_length.min(self.base_hash.len());
    buffer[offset..offset + copy_len].copy_from_slice(&self.base_hash[..copy_len]);
    offset += hash_length;

    // target_hash: hash_length bytes
    let copy_len = hash_length.min(self.target_hash.len());
    buffer[offset..offset + copy_len].copy_from_slice(&self.target_hash[..copy_len]);
    let _ = offset + hash_length; // suppress unused warning

    // CRC32 over the first 252 bytes (all fields + padding zeros). The last
    // 4 bytes hold the CRC itself. A torn write that lands magic + version
    // but garbles a later byte (e.g. hot_tail_offset) will fail this check
    // and trigger dirty startup instead of silently corrupting in-memory state.
    let crc = crc32fast::hash(&buffer[..FILE_HEADER_SIZE - HEADER_CRC_SIZE]);
    buffer[FILE_HEADER_SIZE - HEADER_CRC_SIZE..].copy_from_slice(&crc.to_le_bytes());

    buffer
  }

  pub fn deserialize(bytes: &[u8; FILE_HEADER_SIZE]) -> EngineResult<Self> {
    let mut offset = 0;

    // magic: 4 bytes
    if &bytes[offset..offset + 4] != FILE_MAGIC {
      return Err(EngineError::InvalidMagic);
    }
    offset += 4;

    // header_version: 1 byte. Reject unknown versions with a clear message so
    // future format changes have a clean error story instead of silent corruption.
    let header_version = bytes[offset];
    offset += 1;
    if header_version != SUPPORTED_HEADER_VERSION {
      return Err(EngineError::InvalidEntryVersion(header_version));
    }

    // CRC32 verification — must come BEFORE we interpret any later field so
    // a torn write doesn't bleed garbled data into the in-memory header.
    let stored_crc = u32::from_le_bytes(bytes[FILE_HEADER_SIZE - HEADER_CRC_SIZE..].try_into().unwrap());
    let computed_crc = crc32fast::hash(&bytes[..FILE_HEADER_SIZE - HEADER_CRC_SIZE]);
    if stored_crc != computed_crc {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("file header CRC mismatch (stored {:08x}, computed {:08x})", stored_crc, computed_crc),
      });
    }

    // hash_algo: 2 bytes
    let hash_algo_raw = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
    let hash_algo = HashAlgorithm::from_u16(hash_algo_raw).ok_or(EngineError::InvalidHashAlgorithm(hash_algo_raw))?;
    offset += 2;

    // sequence: 8 bytes — v3 A/B slot selector
    let sequence = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // created_at: 8 bytes
    let created_at = i64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // updated_at: 8 bytes
    let updated_at = i64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // kv_block_offset: 8 bytes
    let kv_block_offset = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // kv_block_length: 8 bytes
    let kv_block_length = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // kv_block_version: 1 byte
    let kv_block_version = bytes[offset];
    offset += 1;

    // nvt_offset: 8 bytes
    let nvt_offset = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // nvt_length: 8 bytes
    let nvt_length = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // nvt_version: 1 byte
    let nvt_version = bytes[offset];
    offset += 1;

    // head_hash: dynamic length
    let hash_length = hash_algo.hash_length();
    let head_hash = bytes[offset..offset + hash_length].to_vec();
    offset += hash_length;

    // entry_count: 8 bytes
    let entry_count = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // resize_in_progress: 1 byte
    let resize_in_progress = bytes[offset] != 0;
    offset += 1;

    // buffer_kvs_offset: 8 bytes
    let buffer_kvs_offset = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // buffer_nvt_offset: 8 bytes
    let buffer_nvt_offset = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // hot_tail_offset: 8 bytes
    let hot_tail_offset = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // kv_block_stage: 1 byte
    let kv_block_stage = bytes[offset];
    offset += 1;

    // resize_target_stage: 1 byte
    let resize_target_stage = bytes[offset];
    offset += 1;

    // backup_type: 1 byte
    let backup_type = bytes[offset];
    offset += 1;

    // base_hash: hash_length bytes
    let base_hash = bytes[offset..offset + hash_length].to_vec();
    offset += hash_length;

    // target_hash: hash_length bytes
    let target_hash = bytes[offset..offset + hash_length].to_vec();
    let _ = offset + hash_length; // suppress unused warning

    Ok(FileHeader {
      header_version,
      hash_algo,
      sequence,
      created_at,
      updated_at,
      kv_block_offset,
      kv_block_length,
      kv_block_version,
      nvt_offset,
      nvt_length,
      nvt_version,
      head_hash,
      entry_count,
      resize_in_progress,
      buffer_kvs_offset,
      buffer_nvt_offset,
      hot_tail_offset,
      kv_block_stage,
      resize_target_stage,
      backup_type,
      base_hash,
      target_hash,
    })
  }
}

// ---------------------------------------------------------------------------
// A/B slot read / write
// ---------------------------------------------------------------------------

/// Read both header slots and return the one with the higher valid sequence.
///
/// On a freshly-created database, slot A has sequence 0 and slot B is all
/// zeros (CRC fails). The function therefore accepts a single valid slot as
/// authoritative. If BOTH slots are valid (the steady state after the first
/// few writes), the higher-sequence one wins.
///
/// Returns the active header along with the slot index (0 or 1) it came from
/// so the caller can write the NEXT update to the OTHER slot.
pub fn read_active_header(file: &mut File) -> EngineResult<(FileHeader, usize)> {
  let mut region = [0u8; HEADER_REGION_SIZE];
  file.seek(SeekFrom::Start(0))?;
  file.read_exact(&mut region)?;
  decode_active_header_region(&region)
}

/// Decode the complete legacy v3 A/B header region without performing I/O.
///
/// This preserves the v3 selector exactly, including its historical choice of
/// slot A when two valid slots have the same sequence. V4 uses a distinct
/// fail-ambiguous selector; changing v3 behavior belongs to migration policy,
/// not format dispatch.
pub fn decode_active_header_region(region: &[u8]) -> EngineResult<(FileHeader, usize)> {
  if region.len() != HEADER_REGION_SIZE {
    return Err(EngineError::InvalidInput(format!("v3 header region must be {HEADER_REGION_SIZE} bytes, got {}", region.len())));
  }

  let slot_a: &[u8; FILE_HEADER_SIZE] = region[..FILE_HEADER_SIZE].try_into().expect("checked v3 slot A width");
  let slot_b: &[u8; FILE_HEADER_SIZE] = region[FILE_HEADER_SIZE..].try_into().expect("checked v3 slot B width");

  let parsed_a = FileHeader::deserialize(slot_a);
  let parsed_b = FileHeader::deserialize(slot_b);

  match (parsed_a, parsed_b) {
    (Ok(a), Ok(b)) => {
      // Both valid — pick higher sequence
      if a.sequence >= b.sequence {
        Ok((a, 0))
      } else {
        Ok((b, 1))
      }
    }
    (Ok(a), Err(_)) => Ok((a, 0)),
    (Err(_), Ok(b)) => Ok((b, 1)),
    (Err(error), Err(_)) => Err(error),
  }
}

/// Write `header` to the slot OPPOSITE the `active_slot` (the slot the most
/// recent read came from). Increments the sequence number first so the new
/// slot wins on the next read.
///
/// CRITICAL ordering for crash safety:
///   1. fsync the file so any prior writes are durable
///   2. write the new header bytes to the inactive slot
///   3. fsync again so the new header is durable
///
/// If we crash between steps 2 and 3, the OLD active slot still wins on the
/// next read (we wrote the new slot to the INACTIVE one), so the database
/// rolls back cleanly to the previous consistent state.
pub fn write_header_to_inactive_slot(file: &mut File, header: &mut FileHeader, active_slot: usize) -> EngineResult<()> {
  write_header_to_inactive_slot_coordinated(file, header, active_slot, &DurabilityCoordinator::new())
}

pub fn write_header_to_inactive_slot_coordinated(
  file: &mut File,
  header: &mut FileHeader,
  active_slot: usize,
  coordinator: &DurabilityCoordinator,
) -> EngineResult<()> {
  if active_slot > 1 {
    return Err(EngineError::InvalidInput(format!("v3 active header slot must be 0 or 1, got {active_slot}")));
  }
  // Increment sequence — the new slot must win on next read.
  header.sequence = header.sequence.wrapping_add(1);

  let bytes = header.serialize();
  let target_slot = 1 - active_slot;
  let slot_offset = (target_slot * FILE_HEADER_SIZE) as u64;
  publish_v3_header_slot(file, slot_offset, &bytes, V3HeaderDependency::None, coordinator)
}

pub(crate) fn write_header_to_inactive_slot_with_dependency<F>(
  file: &mut File,
  header: &mut FileHeader,
  active_slot: usize,
  coordinator: &DurabilityCoordinator,
  estimated_dependency_bytes: u64,
  mut dependency: F,
) -> EngineResult<()>
where
  F: FnMut() -> std::io::Result<()>,
{
  if active_slot > 1 {
    return Err(EngineError::InvalidInput(format!("v3 active header slot must be 0 or 1, got {active_slot}")));
  }
  header.sequence = header.sequence.wrapping_add(1);

  let bytes = header.serialize();
  let target_slot = 1 - active_slot;
  let slot_offset = (target_slot * FILE_HEADER_SIZE) as u64;
  publish_v3_header_slot(
    file,
    slot_offset,
    &bytes,
    V3HeaderDependency::Callback { estimated_bytes: estimated_dependency_bytes, action: &mut dependency },
    coordinator,
  )
}

pub(crate) fn write_header_group_to_inactive_slot_with_dependency<F>(
  file: &mut File,
  header: &mut FileHeader,
  active_slot: usize,
  coordinator: &DurabilityCoordinator,
  tickets: &[DurabilityTicket],
  mut dependency: F,
) -> EngineResult<()>
where
  F: FnMut() -> std::io::Result<()>,
{
  if active_slot > 1 {
    return Err(EngineError::InvalidInput(format!("v3 active header slot must be 0 or 1, got {active_slot}")));
  }
  header.sequence = header.sequence.wrapping_add(1);

  let bytes = header.serialize();
  let target_slot = 1 - active_slot;
  let slot_offset = (target_slot * FILE_HEADER_SIZE) as u64;
  let mut executor = V3HeaderPublicationExecutor {
    file,
    slot_offset,
    bytes: &bytes,
    dependency: V3HeaderDependency::Callback { estimated_bytes: 0, action: &mut dependency },
  };
  coordinator.execute_group(tickets, &mut executor).map_err(coordinator_error)
}

pub(crate) fn v3_header_commit_plan() -> Result<DurabilityCommitPlan, DurabilityCoordinatorError> {
  DurabilityCommitPlan::new(
    CommitClass::HardAuthority,
    vec![
      DurabilityOperation::DependencyAppend,
      DurabilityOperation::DataBarrier,
      DurabilityOperation::AuthorityWrite,
      DurabilityOperation::HeaderAb,
      DurabilityOperation::AuthorityBarrier,
      DurabilityOperation::AuthorityReadback,
    ],
  )
}

fn publish_v3_header_slot<'a>(
  file: &'a File,
  slot_offset: u64,
  bytes: &'a [u8; FILE_HEADER_SIZE],
  dependency: V3HeaderDependency<'a>,
  coordinator: &DurabilityCoordinator,
) -> EngineResult<()> {
  let plan = v3_header_commit_plan().map_err(|error| EngineError::InvalidInput(error.to_string()))?;
  let estimated_bytes = FILE_HEADER_SIZE as u64 + dependency.estimated_bytes();
  let ticket = coordinator.admit_sized(plan, estimated_bytes).map_err(coordinator_error)?;
  let mut executor = V3HeaderPublicationExecutor { file, slot_offset, bytes, dependency };
  coordinator.execute_group(&[ticket], &mut executor).map_err(coordinator_error)?;
  match coordinator.take_waiter_state(ticket).map_err(coordinator_error)? {
    DurabilityWaiterState::Succeeded(_) => Ok(()),
    DurabilityWaiterState::Failed(failure) => Err(EngineError::DurabilityFailure(failure.message)),
    DurabilityWaiterState::Pending => {
      Err(EngineError::DurabilityFailure("v3 header publication remained pending after execution".to_string()))
    }
  }
}

enum V3HeaderDependency<'a> {
  None,
  Positional { offset: u64, bytes: &'a [u8] },
  Callback { estimated_bytes: u64, action: &'a mut dyn FnMut() -> std::io::Result<()> },
}

impl V3HeaderDependency<'_> {
  fn estimated_bytes(&self) -> u64 {
    match self {
      Self::None => 0,
      Self::Positional { bytes, .. } => bytes.len() as u64,
      Self::Callback { estimated_bytes, .. } => *estimated_bytes,
    }
  }
}

fn coordinator_error(error: crate::engine::durability_coordinator::DurabilityCoordinatorError) -> EngineError {
  match error {
    DurabilityCoordinatorError::ResourceExhausted(message) => EngineError::ResourceExhausted(message),
    other => EngineError::DurabilityFailure(other.to_string()),
  }
}

struct V3HeaderPublicationExecutor<'a> {
  file: &'a File,
  slot_offset: u64,
  bytes: &'a [u8; FILE_HEADER_SIZE],
  dependency: V3HeaderDependency<'a>,
}

impl DurabilityGroupExecutor for V3HeaderPublicationExecutor<'_> {
  type Error = NativeDurabilityError;

  fn execute_group(&mut self, _sequences: &[u64], operation: DurabilityOperation) -> Result<(), Self::Error> {
    match operation {
      DurabilityOperation::DependencyAppend => match &mut self.dependency {
        V3HeaderDependency::None => Ok(()),
        V3HeaderDependency::Positional { offset, bytes } => write_file_at_native(self.file, *offset, bytes),
        V3HeaderDependency::Callback { action, .. } => {
          action().map_err(|error| NativeDurabilityError::operation_io(NativeDurabilityOperation::WriteAt, error))
        }
      },
      DurabilityOperation::HeaderAb => Ok(()),
      DurabilityOperation::DataBarrier | DurabilityOperation::AuthorityBarrier => sync_file_data_native(self.file),
      DurabilityOperation::AuthorityWrite => write_file_at_native(self.file, self.slot_offset, self.bytes),
      DurabilityOperation::AuthorityReadback => verify_file_bytes_native(self.file, self.slot_offset, self.bytes),
      _ => Err(NativeDurabilityError::invalid(
        NativeDurabilityOperation::WriteAt,
        format!("unsupported operation in v3 header publication plan: {operation:?}"),
      )),
    }
  }

  fn classify_error(&self, _operation: DurabilityOperation, error: &Self::Error, mutation_started: bool) -> DurabilityFailureDisposition {
    classify_native_durability_error(error, mutation_started)
  }
}

/// Write the initial header to slot A only (slot B left zeroed). Used by
/// `create` since there's no "previous" state to preserve.
pub fn write_initial_header(file: &mut File, header: &mut FileHeader) -> EngineResult<()> {
  write_initial_header_coordinated(file, header, &DurabilityCoordinator::new())
}

pub fn write_initial_header_coordinated(file: &mut File, header: &mut FileHeader, coordinator: &DurabilityCoordinator) -> EngineResult<()> {
  // Zero slot B explicitly so a torn write doesn't accidentally produce a
  // valid-looking slot B at a higher sequence.
  let zero = [0u8; FILE_HEADER_SIZE];
  header.sequence = 1; // start at 1 so any "all zeros" slot reads as older
  let bytes = header.serialize();
  publish_v3_header_slot(file, 0, &bytes, V3HeaderDependency::Positional { offset: FILE_HEADER_SIZE as u64, bytes: &zero }, coordinator)
}
