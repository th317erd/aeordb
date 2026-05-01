//! KV block expansion: relocate WAL entries to make room for a larger KV block.
//!
//! Layout before:
//!   [Header 256B] [KV block (old_size)] [WAL entries...] [Hot tail]
//!
//! Layout after:
//!   [Header 256B] [KV block (new_size)] [WAL entries...] [Hot tail]
//!
//! The WAL entries are copied forward by (new_size - old_size) bytes.
//! All KV entry offsets are adjusted by the same delta.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};

use crate::engine::errors::EngineResult;
use crate::engine::file_header::FILE_HEADER_SIZE;
use crate::engine::kv_pages::page_size;
use crate::engine::kv_stages::stage_params;

/// Expand the KV block in-place by relocating WAL entries forward.
///
/// Returns the (new_kv_block_length, new_stage, delta) on success.
/// `delta` is the number of bytes WAL entries were shifted forward.
///
/// The caller must rebuild the KV index after this — all WAL offsets
/// have changed by `delta`.
pub fn expand_kv_block(
    db_path: &str,
    target_stage: usize,
    hash_length: usize,
) -> EngineResult<(u64, usize, u64)> {
    let psize = page_size(hash_length);
    let (new_block_size, _new_bucket_count) = stage_params(target_stage, psize);

    // Read current header
    let mut file = OpenOptions::new().read(true).write(true).open(db_path)?;
    let mut header_bytes = [0u8; FILE_HEADER_SIZE];
    file.read_exact(&mut header_bytes)?;
    let mut header = crate::engine::file_header::FileHeader::deserialize(&header_bytes)?;

    let old_kv_offset = header.kv_block_offset;
    let old_kv_length = header.kv_block_length;
    let old_hot_tail = header.hot_tail_offset;

    // WAL region: from (kv_offset + old_kv_length) to hot_tail_offset
    let wal_start = old_kv_offset + old_kv_length;
    let wal_end = old_hot_tail;
    let wal_size = wal_end.saturating_sub(wal_start);

    if new_block_size <= old_kv_length {
        tracing::info!("KV block already large enough ({} >= {})", old_kv_length, new_block_size);
        return Ok((old_kv_length, header.kv_block_stage as usize, 0));
    }

    let delta = new_block_size - old_kv_length;
    let new_wal_start = wal_start + delta;
    // hot_tail_offset == 0 means "no hot tail"; preserve that sentinel.
    let new_hot_tail = if old_hot_tail == 0 { 0 } else { wal_end + delta };

    tracing::info!(
        old_kv_length, new_block_size = new_block_size, delta, wal_size,
        "Expanding KV block: relocating {} bytes of WAL data forward by {} bytes",
        wal_size, delta,
    );

    // Relocate WAL entries: copy backwards from end to avoid overwriting
    // data we haven't copied yet.
    const CHUNK_SIZE: u64 = 64 * 1024 * 1024; // 64MB copy chunks
    let mut remaining = wal_size;
    let mut buf = vec![0u8; CHUNK_SIZE.min(remaining) as usize];

    while remaining > 0 {
        let chunk_len = CHUNK_SIZE.min(remaining) as usize;
        let src_offset = wal_start + remaining - chunk_len as u64;
        let dst_offset = src_offset + delta;

        file.seek(SeekFrom::Start(src_offset))?;
        file.read_exact(&mut buf[..chunk_len])?;
        file.seek(SeekFrom::Start(dst_offset))?;
        file.write_all(&buf[..chunk_len])?;

        remaining -= chunk_len as u64;
    }

    // Zero-fill the new KV block area (old KV area + expansion gap)
    let zero_buf = vec![0u8; 65536.min(new_block_size as usize)];
    let mut zeroed = 0u64;
    while zeroed < new_block_size {
        let chunk = zero_buf.len().min((new_block_size - zeroed) as usize);
        file.seek(SeekFrom::Start(old_kv_offset + zeroed))?;
        file.write_all(&zero_buf[..chunk])?;
        zeroed += chunk as u64;
    }

    // Update header
    header.kv_block_length = new_block_size;
    header.kv_block_stage = target_stage as u8;
    header.hot_tail_offset = new_hot_tail;
    let serialized = header.serialize();
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&serialized)?;

    file.sync_all()?;

    tracing::info!(
        new_block_size, new_hot_tail, target_stage,
        "KV block expansion complete"
    );

    Ok((new_block_size, target_stage, delta))
}
