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

use crate::engine::durability_coordinator::{DurabilityCoordinator, NativeFileBarrierKind};
use crate::engine::entry_header::EntryHeader;
use crate::engine::errors::EngineError;
use crate::engine::errors::EngineResult;
use crate::engine::file_header::{HEADER_REGION_SIZE, read_active_header, write_header_to_inactive_slot_coordinated};
use crate::engine::hot_tail::{HotTailPayload, VoidRecord};
use crate::engine::kv_pages::page_size;
use crate::engine::kv_stages::{KV_STAGE_SIZES, stage_params};

/// Convert a sidecar-era no-KV database, or the short-lived post-WAL KV
/// repair layout, into the standard `[headers][KV][WAL][hot tail]` layout.
///
/// The complete source WAL is validated before the first header mutation.
/// The existing resize journal then owns crash recovery: its first selected
/// phase retains the original WAL, and its second selected phase retains the
/// relocated WAL until the new KV region has been zeroed and finalized.
pub(crate) fn bootstrap_initial_kv_block(db_path: &str, hash_length: usize) -> EngineResult<(u64, usize, u64)> {
  let mut file = OpenOptions::new().read(true).write(true).open(db_path)?;
  let coordinator = DurabilityCoordinator::new();
  let (mut header, active_slot) = read_active_header(&mut file)?;
  let header_end = HEADER_REGION_SIZE as u64;
  let physical_end = file.metadata()?.len();

  if hash_length != header.hash_algo.hash_length() {
    return Err(EngineError::InvalidInput(format!(
      "KV bootstrap hash length {hash_length} differs from database hash length {}",
      header.hash_algo.hash_length()
    )));
  }
  if header.resize_in_progress {
    return Err(EngineError::CorruptEntry {
      offset: 0,
      reason: "KV bootstrap cannot replace an existing resize recovery marker".to_string(),
    });
  }

  let source_wal_end = if header.kv_block_offset == 0 && header.kv_block_length == 0 {
    if header.hot_tail_offset != 0 {
      return Err(EngineError::CorruptEntry {
        offset: header.hot_tail_offset,
        reason: "no-KV layout unexpectedly advertises an in-file hot tail; its WAL boundary is ambiguous and requires explicit repair"
          .to_string(),
      });
    }
    physical_end
  } else if header.kv_block_offset > header_end && header.kv_block_length > 0 {
    // The old repair layout placed a disposable KV block after the WAL. Its
    // offset, rather than its often-inconsistent hot-tail field, is the exact
    // authoritative WAL frontier.
    let post_wal_kv_end = header.kv_block_offset.checked_add(header.kv_block_length).ok_or_else(|| EngineError::CorruptEntry {
      offset: header.kv_block_offset,
      reason: "post-WAL KV block end overflows u64".to_string(),
    })?;
    if post_wal_kv_end > physical_end {
      return Err(EngineError::CorruptEntry {
        offset: header.kv_block_offset,
        reason: format!("post-WAL KV block ends at {post_wal_kv_end}, beyond physical file end {physical_end}"),
      });
    }
    header.kv_block_offset
  } else {
    return Err(EngineError::CorruptEntry {
      offset: header.kv_block_offset,
      reason: format!(
        "KV bootstrap requires no KV block or a post-WAL KV block, got offset {} length {}",
        header.kv_block_offset, header.kv_block_length
      ),
    });
  };

  if source_wal_end < header_end || source_wal_end > physical_end {
    return Err(EngineError::CorruptEntry {
      offset: source_wal_end,
      reason: format!("legacy WAL frontier is outside {header_end}..={physical_end}"),
    });
  }
  let validated_end = strict_relocation_end(&mut file, header_end, source_wal_end, source_wal_end, header.hash_algo)?;
  if validated_end != source_wal_end {
    return Err(EngineError::CorruptEntry {
      offset: validated_end,
      reason: format!("legacy WAL validation stopped before its frontier {source_wal_end}"),
    });
  }

  header.kv_block_offset = header_end;
  header.kv_block_length = 0;
  header.kv_block_stage = 0;
  header.hot_tail_offset = source_wal_end;
  header.resize_in_progress = true;
  header.resize_target_stage = 0;
  write_header_to_inactive_slot_coordinated(&mut file, &mut header, active_slot, &coordinator)?;
  drop(file);

  expand_kv_block(db_path, 0, hash_length)
}

/// Expand the KV block in-place by relocating WAL entries forward.
///
/// Returns the (new_kv_block_length, new_stage, delta) on success.
/// `delta` is the number of bytes WAL entries were shifted forward.
///
/// The caller must rebuild the KV index after this — all WAL offsets
/// have changed by `delta`.
pub fn expand_kv_block(db_path: &str, target_stage: usize, hash_length: usize) -> EngineResult<(u64, usize, u64)> {
  if target_stage >= KV_STAGE_SIZES.len() {
    return Err(EngineError::InvalidInput(format!("KV target stage {target_stage} is outside the supported stage table")));
  }
  let psize = page_size(hash_length);
  let (minimum_block_size, _new_bucket_count) = stage_params(target_stage, psize);

  // Read the active header slot (v3 A/B layout).
  let mut file = OpenOptions::new().read(true).write(true).open(db_path)?;
  let coordinator = DurabilityCoordinator::new();
  let (mut header, mut active_slot) = read_active_header(&mut file)?;
  let bootstrap =
    header.resize_in_progress && target_stage == 0 && header.kv_block_stage == 0 && header.kv_block_offset == HEADER_REGION_SIZE as u64;

  if target_stage <= header.kv_block_stage as usize && !bootstrap {
    // Older recovery code cleared resize_target_stage but accidentally left
    // resize_in_progress selected. It had already published the final stage,
    // so clearing that stale boolean is the only mutation required.
    if header.resize_in_progress && target_stage == 0 && header.kv_block_stage > 0 {
      header.resize_in_progress = false;
      write_header_to_inactive_slot_coordinated(&mut file, &mut header, active_slot, &coordinator)?;
      return Ok((header.kv_block_length, header.kv_block_stage as usize, 0));
    }
    return Err(EngineError::CorruptEntry {
      offset: 0,
      reason: format!("KV expansion target stage {target_stage} does not advance current stage {}", header.kv_block_stage),
    });
  }

  let mut relocation_delta = 0u64;
  // A bootstrap's relocation-durable phase keeps resize_in_progress selected
  // while changing kv_block_length from zero to the complete new block. That
  // makes the phase distinguishable even though both source and target stage
  // are zero.
  if header.resize_in_progress && (!bootstrap || header.kv_block_length == 0) {
    let old_kv_offset = header.kv_block_offset;
    let old_kv_end = old_kv_offset
      .checked_add(header.kv_block_length)
      .ok_or_else(|| EngineError::CorruptEntry { offset: old_kv_offset, reason: "current KV block end overflows u64".to_string() })?;
    let minimum_kv_end = old_kv_offset
      .checked_add(minimum_block_size)
      .ok_or_else(|| EngineError::CorruptEntry { offset: old_kv_offset, reason: "expanded KV block end overflows u64".to_string() })?;
    let old_hot_tail = header.hot_tail_offset;
    if old_hot_tail < old_kv_end {
      return Err(EngineError::CorruptEntry {
        offset: old_hot_tail,
        reason: format!("hot tail begins before the current KV block ends at {old_kv_end}"),
      });
    }

    // Establish the exact complete-entry boundary before writing anything.
    let relocation_end = strict_relocation_end(&mut file, old_kv_end, minimum_kv_end, old_hot_tail, header.hash_algo)?;
    let new_kv_end = minimum_kv_end.max(relocation_end);
    let new_block_length = new_kv_end - old_kv_offset;
    let relocation_bytes = relocation_end - old_kv_end;
    let copy_destination = old_hot_tail.max(new_kv_end);
    let new_hot_tail = copy_destination.checked_add(relocation_bytes).ok_or_else(|| EngineError::CorruptEntry {
      offset: copy_destination,
      reason: "relocated hot-tail offset overflows u64".to_string(),
    })?;
    let raw_delta = i128::from(copy_destination) - i128::from(old_kv_end);
    let offset_delta = i64::try_from(raw_delta)
      .map_err(|_| EngineError::InvalidInput(format!("KV relocation delta {raw_delta} cannot be represented as i64")))?;
    relocation_delta = u64::try_from(raw_delta)
      .map_err(|_| EngineError::InvalidInput(format!("KV relocation delta {raw_delta} cannot be represented as u64")))?;

    let relocated_payload = if let Some(old_payload) = crate::engine::hot_tail::read_hot_tail(&mut file, old_hot_tail, hash_length) {
      relocate_hot_tail_payload(old_payload, old_kv_end, relocation_end, offset_delta, new_kv_end, new_hot_tail)?
    } else {
      // A previous pre-relocation attempt may have overwritten the selected
      // old hot tail while copying WAL bytes. Bytes at the future hot-tail
      // offset are not authority until phase 2 is selected, so never trust
      // them even when they happen to contain valid framing and checksums.
      // WAL bytes remain authoritative and the subsequent mandatory
      // rebuild/gap scan recovers KV and Void state.
      tracing::warn!(
        old_hot_tail,
        new_hot_tail,
        "No complete hot-tail copy survived pre-relocation recovery; rebuilding it from authoritative WAL"
      );
      HotTailPayload::default()
    };

    copy_nonoverlapping_region(&mut file, old_kv_end, copy_destination, relocation_bytes)?;
    let hot_end = crate::engine::hot_tail::write_hot_tail(&mut file, new_hot_tail, &relocated_payload, hash_length)?;
    file.set_len(hot_end)?;
    coordinator
      .execute_recoverable_file_barrier(
        &file,
        NativeFileBarrierKind::Full,
        relocation_bytes.saturating_add(hot_end.saturating_sub(new_hot_tail)),
      )
      .map_err(|error| EngineError::DurabilityFailure(error.to_string()))?;

    // Phase 2 is selected only after the relocated WAL and hot tail are
    // durable. From here, recovery must finalize/rebuild, never relocate again.
    header.kv_block_length = new_block_length;
    header.hot_tail_offset = new_hot_tail;
    header.resize_in_progress = bootstrap;
    write_header_to_inactive_slot_coordinated(&mut file, &mut header, active_slot, &coordinator)?;
    active_slot = 1 - active_slot;
  }

  // A selected relocation-durable marker already carries the exact block end
  // and hot-tail frontier. Repeating this zero/finalize phase is idempotent.
  if header.kv_block_length < minimum_block_size {
    return Err(EngineError::CorruptEntry {
      offset: header.kv_block_offset,
      reason: format!(
        "relocation-durable KV block length {} is smaller than the {minimum_block_size}-byte stage {target_stage} minimum",
        header.kv_block_length
      ),
    });
  }
  let new_kv_end = header.kv_block_offset.checked_add(header.kv_block_length).ok_or_else(|| EngineError::CorruptEntry {
    offset: header.kv_block_offset,
    reason: "relocation-durable KV block end overflows u64".to_string(),
  })?;
  if header.hot_tail_offset < new_kv_end {
    return Err(EngineError::CorruptEntry {
      offset: header.hot_tail_offset,
      reason: format!("relocation-durable hot tail begins before the expanded KV block ends at {new_kv_end}"),
    });
  }
  let validated_wal_end = strict_relocation_end(&mut file, new_kv_end, header.hot_tail_offset, header.hot_tail_offset, header.hash_algo)?;
  if validated_wal_end != header.hot_tail_offset {
    return Err(EngineError::CorruptEntry {
      offset: validated_wal_end,
      reason: format!("relocation-durable WAL validation stopped before hot-tail frontier {}", header.hot_tail_offset),
    });
  }

  zero_region(&mut file, header.kv_block_offset, header.kv_block_length)?;
  coordinator
    .execute_recoverable_file_barrier(&file, NativeFileBarrierKind::Full, header.kv_block_length)
    .map_err(|error| EngineError::DurabilityFailure(error.to_string()))?;

  header.kv_block_stage = target_stage as u8;
  header.resize_in_progress = false;
  header.resize_target_stage = 0;
  write_header_to_inactive_slot_coordinated(&mut file, &mut header, active_slot, &coordinator)?;

  tracing::info!(
    new_block_length = header.kv_block_length,
    new_hot_tail = header.hot_tail_offset,
    target_stage,
    relocation_delta,
    "Interrupted KV block expansion recovered"
  );

  Ok((header.kv_block_length, target_stage, relocation_delta))
}

fn strict_relocation_end(
  file: &mut std::fs::File,
  old_kv_end: u64,
  minimum_kv_end: u64,
  wal_end: u64,
  expected_hash_algo: crate::engine::hash_algorithm::HashAlgorithm,
) -> EngineResult<u64> {
  let overlap_end = minimum_kv_end.min(wal_end);
  let mut offset = old_kv_end;
  while offset < overlap_end {
    file.seek(SeekFrom::Start(offset))?;
    let header = EntryHeader::deserialize(file).map_err(|error| EngineError::CorruptEntry {
      offset,
      reason: format!("cannot establish interrupted KV expansion boundary: {error}"),
    })?;
    if header.hash_algo != expected_hash_algo {
      return Err(EngineError::CorruptEntry {
        offset,
        reason: format!("WAL entry hash algorithm {:?} differs from database algorithm {expected_hash_algo:?}", header.hash_algo),
      });
    }
    let expected_total = EntryHeader::compute_total_length(header.hash_algo, header.key_length as usize, header.value_length as usize)?;
    if header.total_length != expected_total {
      return Err(EngineError::CorruptEntry {
        offset,
        reason: format!("WAL entry length {} does not match encoded fields ({expected_total})", header.total_length),
      });
    }
    let entry_end = offset.checked_add(u64::from(header.total_length)).ok_or_else(|| EngineError::CorruptEntry {
      offset,
      reason: format!("WAL entry length {} overflows its file offset", header.total_length),
    })?;
    if entry_end > wal_end {
      return Err(EngineError::CorruptEntry {
        offset,
        reason: format!("WAL entry ends at {entry_end}, beyond the active WAL frontier {wal_end}"),
      });
    }
    offset = entry_end;
  }
  Ok(offset)
}

fn copy_nonoverlapping_region(file: &mut std::fs::File, source: u64, destination: u64, length: u64) -> EngineResult<()> {
  if length == 0 {
    return Ok(());
  }
  let source_end = source
    .checked_add(length)
    .ok_or_else(|| EngineError::CorruptEntry { offset: source, reason: "KV relocation source end overflows u64".to_string() })?;
  if destination < source_end {
    return Err(EngineError::InvalidInput(format!(
      "KV recovery copy destination {destination} overlaps source range {source}..{source_end}"
    )));
  }
  let mut buffer = vec![0u8; 1024 * 1024];
  let mut copied = 0u64;
  while copied < length {
    let chunk = usize::try_from((length - copied).min(buffer.len() as u64))
      .map_err(|_| EngineError::InvalidInput("KV relocation chunk cannot be represented as usize".to_string()))?;
    file.seek(SeekFrom::Start(source + copied))?;
    file.read_exact(&mut buffer[..chunk])?;
    file.seek(SeekFrom::Start(destination + copied))?;
    file.write_all(&buffer[..chunk])?;
    copied += chunk as u64;
  }
  Ok(())
}

fn relocate_hot_tail_payload(
  mut payload: HotTailPayload,
  old_kv_end: u64,
  relocation_end: u64,
  offset_delta: i64,
  new_wal_start: u64,
  new_wal_end: u64,
) -> EngineResult<HotTailPayload> {
  for entry in &mut payload.writes {
    if entry.offset >= old_kv_end && entry.offset < relocation_end {
      entry.offset = shifted_offset(entry.offset, offset_delta)?;
    }
    if entry.offset < new_wal_start || entry.offset >= new_wal_end {
      return Err(EngineError::CorruptEntry {
        offset: entry.offset,
        reason: format!("relocated hot-tail write falls outside WAL range {new_wal_start}..{new_wal_end}"),
      });
    }
  }

  let mut adjusted_voids = Vec::with_capacity(payload.voids.len());
  for mut void in payload.voids {
    let old_end = void
      .offset
      .checked_add(u64::from(void.size))
      .ok_or_else(|| EngineError::CorruptEntry { offset: void.offset, reason: "hot-tail void end overflows u64".to_string() })?;
    if void.offset >= old_kv_end && old_end <= relocation_end {
      void.offset = shifted_offset(void.offset, offset_delta)?;
    } else if void.offset < new_wal_start {
      continue;
    }
    let new_end = void
      .offset
      .checked_add(u64::from(void.size))
      .ok_or_else(|| EngineError::CorruptEntry { offset: void.offset, reason: "relocated hot-tail void end overflows u64".to_string() })?;
    if void.offset >= new_wal_start && new_end <= new_wal_end {
      adjusted_voids.push(VoidRecord { offset: void.offset, size: void.size });
    }
  }
  payload.voids = adjusted_voids;
  Ok(payload)
}

fn shifted_offset(offset: u64, delta: i64) -> EngineResult<u64> {
  let shifted = i128::from(offset) + i128::from(delta);
  u64::try_from(shifted).map_err(|_| EngineError::InvalidInput(format!("relocated offset {shifted} cannot be represented as u64")))
}

fn zero_region(file: &mut std::fs::File, offset: u64, length: u64) -> EngineResult<()> {
  let zeroes = vec![0u8; 1024 * 1024];
  let mut written = 0u64;
  while written < length {
    let chunk = usize::try_from((length - written).min(zeroes.len() as u64))
      .map_err(|_| EngineError::InvalidInput("KV zero-fill chunk cannot be represented as usize".to_string()))?;
    file.seek(SeekFrom::Start(offset + written))?;
    file.write_all(&zeroes[..chunk])?;
    written += chunk as u64;
  }
  Ok(())
}
