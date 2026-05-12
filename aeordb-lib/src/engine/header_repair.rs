//! Low-level header repair for databases that won't open cleanly.
//!
//! The normal [`StorageEngine::open`] path fails fast on:
//!   - unknown header version (we expect v2; v1 files refused)
//!   - header CRC mismatch (torn write)
//!   - hot_tail_offset past EOF (the fsync-ordering corruption that hit
//!     xenocept on 2026-05-11)
//!
//! For real-world recovery we need to make the file openable WITHOUT trusting
//! every header field. This module reads the raw header bytes, identifies
//! recoverable conditions, applies the minimal fixes needed to let
//! `StorageEngine::open` proceed (which then triggers dirty startup), and
//! writes a new v2 header back to disk.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};

use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_header::{
    FileHeader, FILE_HEADER_SIZE, FILE_MAGIC, SUPPORTED_HEADER_VERSION,
};
use crate::engine::hash_algorithm::HashAlgorithm;

/// What the inspection found wrong with the header.
#[derive(Debug, Clone, Default)]
pub struct HeaderRepairReport {
    /// The header was already valid for this build — no repair needed.
    pub already_ok: bool,
    /// File magic was wrong (not an AeorDB file).
    pub bad_magic: bool,
    /// Header version was older than this build supports.
    pub upgraded_version: Option<(u8, u8)>,
    /// hot_tail_offset pointed past the actual file size and was reset.
    pub hot_tail_past_eof: Option<HotTailMismatch>,
    /// Header CRC didn't match (v2 only).
    pub crc_failed: bool,
    /// The new header was written and fsynced.
    pub repaired: bool,
}

#[derive(Debug, Clone)]
pub struct HotTailMismatch {
    pub recorded_offset: u64,
    pub actual_file_size: u64,
    pub bytes_past_eof: u64,
}

impl HeaderRepairReport {
    pub fn needs_repair(&self) -> bool {
        self.upgraded_version.is_some()
            || self.hot_tail_past_eof.is_some()
            || self.crc_failed
    }
}

/// Inspect the header at `db_path` without going through `StorageEngine::open`.
/// Returns a description of what's wrong without modifying anything.
pub fn inspect_header(db_path: &str) -> EngineResult<HeaderRepairReport> {
    let mut report = HeaderRepairReport::default();

    let mut file = OpenOptions::new().read(true).open(db_path)?;
    let file_size = file.metadata()?.len();

    let mut bytes = [0u8; FILE_HEADER_SIZE];
    if let Err(error) = file.read_exact(&mut bytes) {
        return Err(EngineError::IoError(error));
    }

    if &bytes[0..4] != FILE_MAGIC {
        report.bad_magic = true;
        return Ok(report);
    }

    let header_version = bytes[4];
    if header_version != SUPPORTED_HEADER_VERSION {
        report.upgraded_version = Some((header_version, SUPPORTED_HEADER_VERSION));
    }

    // For v2+ check CRC; for v1 there's no CRC to check.
    if header_version == 2 {
        let stored = u32::from_le_bytes(
            bytes[FILE_HEADER_SIZE - 4..].try_into().unwrap(),
        );
        let computed = crc32fast::hash(&bytes[..FILE_HEADER_SIZE - 4]);
        if stored != computed {
            report.crc_failed = true;
        }
    }

    // Parse hot_tail_offset directly: it lives 114..122 in both v1 and v2
    // (the layout up to that point is identical).
    let hot_tail_offset = u64::from_le_bytes(bytes[114..122].try_into().unwrap());
    if hot_tail_offset > file_size {
        report.hot_tail_past_eof = Some(HotTailMismatch {
            recorded_offset: hot_tail_offset,
            actual_file_size: file_size,
            bytes_past_eof: hot_tail_offset - file_size,
        });
    }

    if !report.needs_repair() && !report.bad_magic {
        report.already_ok = true;
    }

    Ok(report)
}

/// Repair the header in place. Reads it as v1/v2, applies fixes, writes a
/// fresh v2 header back to disk with the safe fsync ordering. After this
/// returns successfully, `StorageEngine::open` on the same path will succeed
/// (triggering dirty startup to rebuild the KV from a full WAL scan).
///
/// Caller MUST close any open handle to the file before invoking this.
/// Returns `(report, repaired_header)` so the caller can decide what to do
/// next (e.g. trigger dirty startup, rebuild_kv, etc.).
pub fn repair_header_in_place(db_path: &str) -> EngineResult<HeaderRepairReport> {
    let mut report = inspect_header(db_path)?;

    if report.already_ok {
        return Ok(report);
    }
    if report.bad_magic {
        return Err(EngineError::InvalidMagic);
    }

    let mut file = OpenOptions::new().read(true).write(true).open(db_path)?;
    let file_size = file.metadata()?.len();

    let mut bytes = [0u8; FILE_HEADER_SIZE];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut bytes)?;

    // Parse the fields we trust into a fresh FileHeader. Layout for v1 and v2
    // is identical up to byte 252; v2 just adds CRC at 252..256. So we can
    // read v1 bytes into a header that we then re-serialize as v2 (the writer
    // adds the CRC).
    let hash_algo_raw = u16::from_le_bytes([bytes[5], bytes[6]]);
    let hash_algo = HashAlgorithm::from_u16(hash_algo_raw)
        .ok_or(EngineError::InvalidHashAlgorithm(hash_algo_raw))?;
    let hash_length = hash_algo.hash_length();

    let created_at = i64::from_le_bytes(bytes[7..15].try_into().unwrap());
    let updated_at = i64::from_le_bytes(bytes[15..23].try_into().unwrap());
    let kv_block_offset = u64::from_le_bytes(bytes[23..31].try_into().unwrap());
    let kv_block_length = u64::from_le_bytes(bytes[31..39].try_into().unwrap());
    let kv_block_version = bytes[39];
    let nvt_offset = u64::from_le_bytes(bytes[40..48].try_into().unwrap());
    let nvt_length = u64::from_le_bytes(bytes[48..56].try_into().unwrap());
    let nvt_version = bytes[56];

    let head_hash_start = 57;
    let head_hash_end = head_hash_start + hash_length;
    let head_hash = bytes[head_hash_start..head_hash_end].to_vec();

    let entry_count = u64::from_le_bytes(
        bytes[head_hash_end..head_hash_end + 8].try_into().unwrap(),
    );
    let resize_in_progress = bytes[head_hash_end + 8] != 0;
    let buffer_kvs_offset = u64::from_le_bytes(
        bytes[head_hash_end + 9..head_hash_end + 17].try_into().unwrap(),
    );
    let buffer_nvt_offset = u64::from_le_bytes(
        bytes[head_hash_end + 17..head_hash_end + 25].try_into().unwrap(),
    );
    let mut hot_tail_offset = u64::from_le_bytes(
        bytes[head_hash_end + 25..head_hash_end + 33].try_into().unwrap(),
    );
    let kv_block_stage = bytes[head_hash_end + 33];
    let resize_target_stage = bytes[head_hash_end + 34];
    let backup_type = bytes[head_hash_end + 35];

    let base_hash_start = head_hash_end + 36;
    let base_hash_end = base_hash_start + hash_length;
    let base_hash = bytes[base_hash_start..base_hash_end].to_vec();
    let target_hash_end = base_hash_end + hash_length;
    let target_hash = bytes[base_hash_end..target_hash_end].to_vec();

    // Apply the fix: if hot_tail_offset points past EOF, reset it to the
    // current file size. read_hot_tail at file size will fail to find a
    // valid hot-tail header (EOF before the 13-byte header is read), so
    // open's dirty-startup branch fires and rebuild_kv reconstructs the KV
    // from a full WAL scan. We keep the offset > kv_block_offset so the
    // kv_block_valid check still passes — that lets the KV pages on disk
    // be reused while only the missing tail entries are recovered.
    if let Some(ref mismatch) = report.hot_tail_past_eof {
        tracing::warn!(
            recorded = mismatch.recorded_offset,
            file_size = mismatch.actual_file_size,
            past_eof = mismatch.bytes_past_eof,
            "hot_tail_offset is past EOF — resetting to file size to trigger dirty startup"
        );
        hot_tail_offset = mismatch.actual_file_size;
    }

    let new_header = FileHeader {
        header_version: SUPPORTED_HEADER_VERSION,
        hash_algo,
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
    };

    let new_bytes = new_header.serialize();

    // Safe ordering: fsync before AND after writing the header.
    file.sync_all()?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&new_bytes)?;
    file.sync_all()?;

    let _ = file_size; // silence unused warning if no logging refs it

    report.repaired = true;
    Ok(report)
}
