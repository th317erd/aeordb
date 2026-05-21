use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;

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
  pub backup_type: u8,        // 0=normal, 1=full export, 2=patch
  pub base_hash: Vec<u8>,     // source version hash
  pub target_hash: Vec<u8>,   // destination version hash
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
    let stored_crc = u32::from_le_bytes(
      bytes[FILE_HEADER_SIZE - HEADER_CRC_SIZE..].try_into().unwrap(),
    );
    let computed_crc = crc32fast::hash(&bytes[..FILE_HEADER_SIZE - HEADER_CRC_SIZE]);
    if stored_crc != computed_crc {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!(
          "file header CRC mismatch (stored {:08x}, computed {:08x})",
          stored_crc, computed_crc
        ),
      });
    }

    // hash_algo: 2 bytes
    let hash_algo_raw = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
    let hash_algo = HashAlgorithm::from_u16(hash_algo_raw)
      .ok_or(EngineError::InvalidHashAlgorithm(hash_algo_raw))?;
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
  let mut slot_a = [0u8; FILE_HEADER_SIZE];
  let mut slot_b = [0u8; FILE_HEADER_SIZE];

  file.seek(SeekFrom::Start(0))?;
  file.read_exact(&mut slot_a)?;
  file.seek(SeekFrom::Start(FILE_HEADER_SIZE as u64))?;
  file.read_exact(&mut slot_b)?;

  let parsed_a = FileHeader::deserialize(&slot_a);
  let parsed_b = FileHeader::deserialize(&slot_b);

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
pub fn write_header_to_inactive_slot(
  file: &mut File,
  header: &mut FileHeader,
  active_slot: usize,
) -> EngineResult<()> {
  // Increment sequence — the new slot must win on next read.
  header.sequence = header.sequence.wrapping_add(1);

  let bytes = header.serialize();
  let target_slot = 1 - active_slot;
  let slot_offset = (target_slot * FILE_HEADER_SIZE) as u64;

  // Barrier: durable-ize prior writes first.
  file.sync_data()?;

  file.seek(SeekFrom::Start(slot_offset))?;
  file.write_all(&bytes)?;
  file.sync_data()?;

  Ok(())
}

/// Write the initial header to slot A only (slot B left zeroed). Used by
/// `create` since there's no "previous" state to preserve.
pub fn write_initial_header(file: &mut File, header: &mut FileHeader) -> EngineResult<()> {
  // Zero slot B explicitly so a torn write doesn't accidentally produce a
  // valid-looking slot B at a higher sequence.
  let zero = [0u8; FILE_HEADER_SIZE];
  file.seek(SeekFrom::Start(FILE_HEADER_SIZE as u64))?;
  file.write_all(&zero)?;

  header.sequence = 1; // start at 1 so any "all zeros" slot reads as older
  let bytes = header.serialize();
  file.seek(SeekFrom::Start(0))?;
  file.write_all(&bytes)?;
  file.sync_data()?;
  Ok(())
}
