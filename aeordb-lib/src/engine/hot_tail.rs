use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::kv_store::KVEntry;
use std::io::{Read, Seek, SeekFrom, Write};

/// Magic bytes for the hot tail (5 bytes). Bumped to 0x..0D for the
/// versioned multi-section format (pending writes + voids). Older hot
/// tails (magic ending 0x..0C) fail the magic check → dirty rebuild on
/// first open with the new code, no compatibility concerns pre-beta.
pub const HOT_TAIL_MAGIC: [u8; 5] = [0xAE, 0x01, 0x7D, 0xB1, 0x0D];

/// Top-level hot-tail format version. Bumped when the section layout
/// changes (new section, reordering, etc.).
pub const HOT_TAIL_FORMAT_VERSION: u8 = 1;

/// Per-record versions inside the hot tail. Each section's records carry
/// their own version byte so individual record layouts can evolve without
/// requiring a full format bump.
pub const WRITE_RECORD_VERSION: u8 = 1;
pub const VOID_RECORD_VERSION: u8 = 1;

/// Header layout (21 bytes):
///   magic(5) + format_version(1) + write_count(u32) + void_count(u32) +
///   crc32_of_header(u32)
/// The CRC is computed over the preceding 17 bytes (magic + version + counts).
const HOT_TAIL_HEADER_SIZE: usize = 5 + 1 + 4 + 4 + 4;

/// Per-write-record size: version(1) + hash + type_flags(1) + offset(8) + total_length(4).
fn write_record_size(hash_length: usize) -> usize {
  checked_write_record_size(hash_length).expect("supported hash lengths fit the hot-tail record format")
}

fn checked_write_record_size(hash_length: usize) -> Option<usize> {
  hash_length.checked_add(1 + 1 + 8 + 4)
}

fn checked_body_size(write_count: usize, void_count: usize, hash_length: usize) -> Option<usize> {
  write_count.checked_mul(checked_write_record_size(hash_length)?)?.checked_add(void_count.checked_mul(VOID_RECORD_SIZE)?)
}

/// Per-void-record size: version(1) + offset(8) + size(4) = 13 bytes.
const VOID_RECORD_SIZE: usize = 1 + 8 + 4;

pub fn serialized_size(write_count: usize, void_count: usize, hash_length: usize) -> usize {
  HOT_TAIL_HEADER_SIZE + write_count * write_record_size(hash_length) + void_count * VOID_RECORD_SIZE
}

/// A descriptive void record carried in the hot tail. Pure data — its
/// existence in the hot tail tells the runtime that the bytes at
/// `(offset, offset + size)` are reclaimable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoidRecord {
  pub offset: u64,
  pub size: u32,
}

/// Combined hot tail payload: the in-flight KV writes that haven't been
/// flushed to bucket pages yet, plus the current `void_manager` snapshot.
#[derive(Debug, Default, Clone)]
pub struct HotTailPayload {
  pub writes: Vec<KVEntry>,
  pub voids: Vec<VoidRecord>,
}

/// Write the frozen hot-tail encoding without allocating a second buffer for
/// the complete payload. Emergency preservation uses this path when memory is
/// already under pressure.
pub fn write_hot_tail_payload<W: Write>(writer: &mut W, payload: &HotTailPayload, hash_length: usize) -> EngineResult<()> {
  let write_count = u32::try_from(payload.writes.len())
    .map_err(|_| EngineError::ResourceExhausted("hot-tail write count exceeds the v1 format limit".to_string()))?;
  let void_count = u32::try_from(payload.voids.len())
    .map_err(|_| EngineError::ResourceExhausted("hot-tail void count exceeds the v1 format limit".to_string()))?;
  checked_body_size(payload.writes.len(), payload.voids.len(), hash_length)
    .ok_or_else(|| EngineError::ResourceExhausted("hot-tail serialized size overflow".to_string()))?;

  let mut header = [0u8; HOT_TAIL_HEADER_SIZE];
  header[..5].copy_from_slice(&HOT_TAIL_MAGIC);
  header[5] = HOT_TAIL_FORMAT_VERSION;
  header[6..10].copy_from_slice(&write_count.to_le_bytes());
  header[10..14].copy_from_slice(&void_count.to_le_bytes());
  let header_crc = crc32fast::hash(&header[..14]);
  header[14..18].copy_from_slice(&header_crc.to_le_bytes());
  writer.write_all(&header)?;

  const ZERO_PADDING: [u8; 64] = [0; 64];
  for entry in &payload.writes {
    writer.write_all(&[WRITE_RECORD_VERSION])?;
    let copy_length = hash_length.min(entry.hash.len());
    writer.write_all(&entry.hash[..copy_length])?;
    let mut padding = hash_length - copy_length;
    while padding > 0 {
      let width = padding.min(ZERO_PADDING.len());
      writer.write_all(&ZERO_PADDING[..width])?;
      padding -= width;
    }
    writer.write_all(&[entry.type_flags])?;
    writer.write_all(&entry.offset.to_le_bytes())?;
    writer.write_all(&entry.total_length.to_le_bytes())?;
  }

  for void in &payload.voids {
    writer.write_all(&[VOID_RECORD_VERSION])?;
    writer.write_all(&void.offset.to_le_bytes())?;
    writer.write_all(&void.size.to_le_bytes())?;
  }
  Ok(())
}

/// Serialize the hot tail payload (writes + voids) into a single byte buffer.
pub fn serialize_hot_tail(payload: &HotTailPayload, hash_length: usize) -> Vec<u8> {
  let total = serialized_size(payload.writes.len(), payload.voids.len(), hash_length);

  let mut buf = Vec::with_capacity(total);
  buf.extend_from_slice(&HOT_TAIL_MAGIC);
  buf.push(HOT_TAIL_FORMAT_VERSION);
  buf.extend_from_slice(&(payload.writes.len() as u32).to_le_bytes());
  buf.extend_from_slice(&(payload.voids.len() as u32).to_le_bytes());

  // CRC over the 14-byte pre-CRC header (magic + version + writes_count + voids_count).
  let header_crc = crc32fast::hash(&buf[..14]);
  buf.extend_from_slice(&header_crc.to_le_bytes());

  // Write records.
  for entry in &payload.writes {
    buf.push(WRITE_RECORD_VERSION);
    let hash_bytes = &entry.hash;
    let copy_len = hash_length.min(hash_bytes.len());
    buf.extend_from_slice(&hash_bytes[..copy_len]);
    if hash_bytes.len() < hash_length {
      buf.resize(buf.len() + (hash_length - hash_bytes.len()), 0);
    }
    buf.push(entry.type_flags);
    buf.extend_from_slice(&entry.offset.to_le_bytes());
    buf.extend_from_slice(&entry.total_length.to_le_bytes());
  }

  // Void records.
  for v in &payload.voids {
    buf.push(VOID_RECORD_VERSION);
    buf.extend_from_slice(&v.offset.to_le_bytes());
    buf.extend_from_slice(&v.size.to_le_bytes());
  }

  buf
}

/// Deserialize a hot-tail payload from bytes. Returns `None` if magic
/// mismatches, CRC fails, format version is unrecognized, or the buffer
/// is truncated.
pub fn deserialize_hot_tail(data: &[u8], hash_length: usize) -> Option<HotTailPayload> {
  if data.len() < HOT_TAIL_HEADER_SIZE {
    return None;
  }
  if data[..5] != HOT_TAIL_MAGIC {
    return None;
  }
  let format_version = data[5];
  if format_version != HOT_TAIL_FORMAT_VERSION {
    return None;
  }

  let write_count = u32::from_le_bytes(data[6..10].try_into().ok()?) as usize;
  let void_count = u32::from_le_bytes(data[10..14].try_into().ok()?) as usize;
  let stored_crc = u32::from_le_bytes(data[14..18].try_into().ok()?);
  let actual_crc = crc32fast::hash(&data[..14]);
  if stored_crc != actual_crc {
    return None;
  }

  let expected_len = HOT_TAIL_HEADER_SIZE.checked_add(checked_body_size(write_count, void_count, hash_length)?)?;
  if data.len() < expected_len {
    return None;
  }

  let mut writes = Vec::new();
  writes.try_reserve_exact(write_count).ok()?;
  let mut cursor = HOT_TAIL_HEADER_SIZE;

  for _ in 0..write_count {
    if data[cursor] != WRITE_RECORD_VERSION {
      return None;
    }
    cursor += 1;
    let mut hash = Vec::new();
    hash.try_reserve_exact(hash_length).ok()?;
    hash.extend_from_slice(&data[cursor..cursor + hash_length]);
    cursor += hash_length;
    let type_flags = data[cursor];
    cursor += 1;
    let offset = u64::from_le_bytes(data[cursor..cursor + 8].try_into().ok()?);
    cursor += 8;
    let total_length = u32::from_le_bytes(data[cursor..cursor + 4].try_into().ok()?);
    cursor += 4;
    writes.push(KVEntry { hash, type_flags, offset, total_length });
  }

  let mut voids = Vec::new();
  voids.try_reserve_exact(void_count).ok()?;
  for _ in 0..void_count {
    if data[cursor] != VOID_RECORD_VERSION {
      return None;
    }
    cursor += 1;
    let offset = u64::from_le_bytes(data[cursor..cursor + 8].try_into().ok()?);
    cursor += 8;
    let size = u32::from_le_bytes(data[cursor..cursor + 4].try_into().ok()?);
    cursor += 4;
    voids.push(VoidRecord { offset, size });
  }

  Some(HotTailPayload { writes, voids })
}

/// Write the hot tail payload to a file at the given offset.
pub fn write_hot_tail<W: Write + Seek>(writer: &mut W, offset: u64, payload: &HotTailPayload, hash_length: usize) -> EngineResult<u64> {
  writer.seek(SeekFrom::Start(offset))?;
  write_hot_tail_payload(writer, payload, hash_length)?;
  let length = serialized_size(payload.writes.len(), payload.voids.len(), hash_length);
  offset.checked_add(length as u64).ok_or_else(|| EngineError::ResourceExhausted("hot-tail end offset overflow".to_string()))
}

/// Read the hot tail payload from a file at the given offset.
/// Returns `None` for any failure (missing, torn, wrong version, etc.).
pub fn read_hot_tail<R: Read + Seek>(reader: &mut R, offset: u64, hash_length: usize) -> Option<HotTailPayload> {
  reader.seek(SeekFrom::Start(offset)).ok()?;

  let mut header = [0u8; HOT_TAIL_HEADER_SIZE];
  reader.read_exact(&mut header).ok()?;

  if header[..5] != HOT_TAIL_MAGIC {
    return None;
  }
  if header[5] != HOT_TAIL_FORMAT_VERSION {
    return None;
  }

  let write_count = u32::from_le_bytes(header[6..10].try_into().ok()?) as usize;
  let void_count = u32::from_le_bytes(header[10..14].try_into().ok()?) as usize;
  let stored_crc = u32::from_le_bytes(header[14..18].try_into().ok()?);
  if crc32fast::hash(&header[..14]) != stored_crc {
    return None;
  }

  let body_len = checked_body_size(write_count, void_count, hash_length)?;
  let records_start = offset.checked_add(HOT_TAIL_HEADER_SIZE as u64)?;
  let required_end = records_start.checked_add(u64::try_from(body_len).ok()?)?;
  if required_end > reader.seek(SeekFrom::End(0)).ok()? {
    return None;
  }
  reader.seek(SeekFrom::Start(records_start)).ok()?;

  let write_size = checked_write_record_size(hash_length)?;
  let mut write_record = Vec::new();
  write_record.try_reserve_exact(write_size).ok()?;
  write_record.resize(write_size, 0);
  let mut writes = Vec::new();
  writes.try_reserve_exact(write_count).ok()?;
  for _ in 0..write_count {
    reader.read_exact(&mut write_record).ok()?;
    if write_record[0] != WRITE_RECORD_VERSION {
      return None;
    }
    let mut hash = Vec::new();
    hash.try_reserve_exact(hash_length).ok()?;
    hash.extend_from_slice(&write_record[1..1 + hash_length]);
    let type_flags = write_record[1 + hash_length];
    let number_start = 2 + hash_length;
    let entry_offset = u64::from_le_bytes(write_record[number_start..number_start + 8].try_into().ok()?);
    let total_length = u32::from_le_bytes(write_record[number_start + 8..number_start + 12].try_into().ok()?);
    writes.push(KVEntry { hash, type_flags, offset: entry_offset, total_length });
  }

  let mut voids = Vec::new();
  voids.try_reserve_exact(void_count).ok()?;
  let mut void_record = [0u8; VOID_RECORD_SIZE];
  for _ in 0..void_count {
    reader.read_exact(&mut void_record).ok()?;
    if void_record[0] != VOID_RECORD_VERSION {
      return None;
    }
    let void_offset = u64::from_le_bytes(void_record[1..9].try_into().ok()?);
    let size = u32::from_le_bytes(void_record[9..13].try_into().ok()?);
    voids.push(VoidRecord { offset: void_offset, size });
  }

  Some(HotTailPayload { writes, voids })
}

/// Validate a hot tail and visit its void records without allocating arrays
/// from its on-disk counts. Verification and recovery diagnostics use this
/// path because those counts are untrusted until every fixed-size record has
/// been read successfully.
pub(crate) fn visit_hot_tail_voids<R, F>(
  reader: &mut R,
  offset: u64,
  hash_length: usize,
  cancellation: Option<&std::sync::atomic::AtomicBool>,
  mut visitor: F,
) -> EngineResult<(u32, u32)>
where
  R: Read + Seek,
  F: FnMut(u32, VoidRecord) -> EngineResult<()>,
{
  reader.seek(SeekFrom::Start(offset))?;
  let mut header = [0u8; HOT_TAIL_HEADER_SIZE];
  reader.read_exact(&mut header)?;
  if header[..5] != HOT_TAIL_MAGIC {
    return Err(EngineError::InvalidMagic);
  }
  if header[5] != HOT_TAIL_FORMAT_VERSION {
    return Err(EngineError::InvalidEntryVersion(header[5]));
  }
  let write_count = u32::from_le_bytes(header[6..10].try_into().map_err(|_| EngineError::UnexpectedEof)?);
  let void_count = u32::from_le_bytes(header[10..14].try_into().map_err(|_| EngineError::UnexpectedEof)?);
  let stored_crc = u32::from_le_bytes(header[14..18].try_into().map_err(|_| EngineError::UnexpectedEof)?);
  if crc32fast::hash(&header[..14]) != stored_crc {
    return Err(EngineError::CorruptEntry { offset, reason: "hot-tail header checksum mismatch".to_string() });
  }

  let write_record_length = write_record_size(hash_length);
  let records_start = offset
    .checked_add(HOT_TAIL_HEADER_SIZE as u64)
    .ok_or_else(|| EngineError::CorruptEntry { offset, reason: "hot-tail record offset overflow".to_string() })?;
  let records_bytes = u64::from(write_count)
    .checked_mul(
      u64::try_from(write_record_length)
        .map_err(|_| EngineError::CorruptEntry { offset, reason: "hot-tail write record length exceeds u64".to_string() })?,
    )
    .and_then(|bytes| bytes.checked_add(u64::from(void_count).checked_mul(VOID_RECORD_SIZE as u64)?))
    .ok_or_else(|| EngineError::CorruptEntry { offset, reason: "hot-tail record span overflow".to_string() })?;
  let required_end = records_start
    .checked_add(records_bytes)
    .ok_or_else(|| EngineError::CorruptEntry { offset, reason: "hot-tail end offset overflow".to_string() })?;
  let physical_end = reader.seek(SeekFrom::End(0))?;
  if required_end > physical_end {
    return Err(EngineError::CorruptEntry {
      offset,
      reason: format!("hot-tail record counts require end {required_end}, beyond file length {physical_end}"),
    });
  }
  reader.seek(SeekFrom::Start(records_start))?;

  let mut write_record = Vec::new();
  write_record
    .try_reserve_exact(write_record_length)
    .map_err(|error| EngineError::ResourceExhausted(format!("hot-tail verification buffer allocation failed: {error}")))?;
  write_record.resize(write_record_length, 0);
  for index in 0..write_count {
    if index % 4_096 == 0 && cancellation.is_some_and(|cancelled| cancelled.load(std::sync::atomic::Ordering::Acquire)) {
      return Err(EngineError::ShuttingDown);
    }
    reader.read_exact(&mut write_record).map_err(|error| hot_tail_record_error(offset, "write", index, error))?;
    if write_record[0] != WRITE_RECORD_VERSION {
      return Err(EngineError::CorruptEntry {
        offset,
        reason: format!("hot-tail write record {index} has unsupported version {}", write_record[0]),
      });
    }
  }

  let mut void_record = [0u8; VOID_RECORD_SIZE];
  for index in 0..void_count {
    if index % 4_096 == 0 && cancellation.is_some_and(|cancelled| cancelled.load(std::sync::atomic::Ordering::Acquire)) {
      return Err(EngineError::ShuttingDown);
    }
    reader.read_exact(&mut void_record).map_err(|error| hot_tail_record_error(offset, "void", index, error))?;
    if void_record[0] != VOID_RECORD_VERSION {
      return Err(EngineError::CorruptEntry {
        offset,
        reason: format!("hot-tail void record {index} has unsupported version {}", void_record[0]),
      });
    }
    let void = VoidRecord {
      offset: u64::from_le_bytes(void_record[1..9].try_into().map_err(|_| EngineError::UnexpectedEof)?),
      size: u32::from_le_bytes(void_record[9..13].try_into().map_err(|_| EngineError::UnexpectedEof)?),
    };
    visitor(index, void)?;
  }
  Ok((write_count, void_count))
}

fn hot_tail_record_error(offset: u64, section: &str, index: u32, error: std::io::Error) -> EngineError {
  if error.kind() == std::io::ErrorKind::UnexpectedEof {
    EngineError::CorruptEntry { offset, reason: format!("hot-tail {section} record {index} is truncated: {error}") }
  } else {
    EngineError::IoError(error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  struct FailAfterCursor {
    inner: std::io::Cursor<Vec<u8>>,
    fail_at: u64,
  }

  struct FailAfterWriter {
    written: usize,
    fail_at: usize,
  }

  impl std::io::Write for FailAfterWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
      if self.written >= self.fail_at {
        return Err(std::io::Error::new(std::io::ErrorKind::StorageFull, "injected hot-tail write failure"));
      }
      let width = buffer.len().min(self.fail_at - self.written);
      self.written += width;
      Ok(width)
    }

    fn flush(&mut self) -> std::io::Result<()> {
      Ok(())
    }
  }

  impl std::io::Read for FailAfterCursor {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
      if self.inner.position() >= self.fail_at {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "injected hot-tail read failure"));
      }
      let remaining = usize::try_from(self.fail_at - self.inner.position()).unwrap_or(usize::MAX);
      let width = buffer.len().min(remaining);
      std::io::Read::read(&mut self.inner, &mut buffer[..width])
    }
  }

  impl std::io::Seek for FailAfterCursor {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
      std::io::Seek::seek(&mut self.inner, position)
    }
  }

  fn make_entry(hash_val: u8, type_flags: u8, offset: u64, total_length: u32) -> KVEntry {
    KVEntry { hash: vec![hash_val; 32], type_flags, offset, total_length }
  }

  #[test]
  fn writes_only_roundtrip() {
    let p = HotTailPayload { writes: vec![make_entry(0xAA, 0x01, 100, 128), make_entry(0xBB, 0x02, 200, 256)], voids: vec![] };
    let data = serialize_hot_tail(&p, 32);
    let got = deserialize_hot_tail(&data, 32).unwrap();
    assert_eq!(got.writes.len(), 2);
    assert_eq!(got.voids.len(), 0);
    assert_eq!(got.writes[0].total_length, 128);
    assert_eq!(got.writes[1].offset, 200);
  }

  #[test]
  fn voids_only_roundtrip() {
    let p = HotTailPayload { writes: vec![], voids: vec![VoidRecord { offset: 1000, size: 500 }, VoidRecord { offset: 5000, size: 256 }] };
    let data = serialize_hot_tail(&p, 32);
    let got = deserialize_hot_tail(&data, 32).unwrap();
    assert_eq!(got.writes.len(), 0);
    assert_eq!(got.voids, p.voids);
  }

  #[test]
  fn mixed_roundtrip() {
    let p = HotTailPayload { writes: vec![make_entry(0xCC, 0x03, 300, 512)], voids: vec![VoidRecord { offset: 2000, size: 128 }] };
    let data = serialize_hot_tail(&p, 32);
    let got = deserialize_hot_tail(&data, 32).unwrap();
    assert_eq!(got.writes.len(), 1);
    assert_eq!(got.voids.len(), 1);
  }

  #[test]
  fn empty_roundtrip() {
    let p = HotTailPayload::default();
    let data = serialize_hot_tail(&p, 32);
    let got = deserialize_hot_tail(&data, 32).unwrap();
    assert!(got.writes.is_empty());
    assert!(got.voids.is_empty());
  }

  #[test]
  fn streaming_writer_matches_the_frozen_hot_tail_encoding() {
    let payload = HotTailPayload {
      writes: vec![make_entry(0x11, 0x02, 1000, 80), make_entry(0x22, 0x03, 2000, 200)],
      voids: vec![VoidRecord { offset: 8888, size: 64 }],
    };
    let expected = serialize_hot_tail(&payload, 32);
    let mut streamed = Vec::new();

    write_hot_tail_payload(&mut streamed, &payload, 32).unwrap();

    assert_eq!(streamed, expected);
  }

  #[test]
  fn streaming_writer_surfaces_destination_failure() {
    let payload = HotTailPayload {
      writes: vec![make_entry(0x11, 0x02, 1000, 80), make_entry(0x22, 0x03, 2000, 200)],
      voids: vec![VoidRecord { offset: 8888, size: 64 }],
    };
    let mut writer = FailAfterWriter { written: 0, fail_at: HOT_TAIL_HEADER_SIZE + 5 };

    let error = write_hot_tail_payload(&mut writer, &payload, 32).unwrap_err();

    assert!(matches!(error, EngineError::IoError(ref source) if source.kind() == std::io::ErrorKind::StorageFull));
  }

  #[test]
  fn corrupt_magic_returns_none() {
    let p = HotTailPayload { writes: vec![make_entry(0xAA, 0x01, 100, 64)], voids: vec![] };
    let mut data = serialize_hot_tail(&p, 32);
    data[0] = 0xFF;
    assert!(deserialize_hot_tail(&data, 32).is_none());
  }

  #[test]
  fn corrupt_crc_returns_none() {
    let p = HotTailPayload { writes: vec![make_entry(0xAA, 0x01, 100, 64)], voids: vec![] };
    let mut data = serialize_hot_tail(&p, 32);
    // Tamper a count byte without updating the CRC.
    data[6] = 99;
    assert!(deserialize_hot_tail(&data, 32).is_none());
  }

  #[test]
  fn unknown_format_version_returns_none() {
    let p = HotTailPayload::default();
    let mut data = serialize_hot_tail(&p, 32);
    data[5] = 99; // Unknown format version
                  // CRC must still match the (now-tampered) bytes for the version check to be the rejector.
    let new_crc = crc32fast::hash(&data[..14]);
    data[14..18].copy_from_slice(&new_crc.to_le_bytes());
    assert!(deserialize_hot_tail(&data, 32).is_none());
  }

  #[test]
  fn truncated_returns_none() {
    let p = HotTailPayload { writes: vec![make_entry(0xAA, 0x01, 100, 64)], voids: vec![VoidRecord { offset: 5, size: 9 }] };
    let data = serialize_hot_tail(&p, 32);
    let truncated = &data[..data.len() - 4];
    assert!(deserialize_hot_tail(truncated, 32).is_none());
  }

  #[test]
  fn readers_reject_unknown_record_versions() {
    let payload = HotTailPayload { writes: vec![make_entry(0xAA, 0x01, 100, 64)], voids: vec![] };
    let mut bytes = serialize_hot_tail(&payload, 32);
    bytes[HOT_TAIL_HEADER_SIZE] = WRITE_RECORD_VERSION + 1;

    assert!(deserialize_hot_tail(&bytes, 32).is_none());
    assert!(read_hot_tail(&mut std::io::Cursor::new(bytes), 0, 32).is_none());
  }

  #[test]
  fn file_reader_preflights_untrusted_counts_against_physical_extent() {
    let mut bytes = serialize_hot_tail(&HotTailPayload::default(), 32);
    bytes[6..10].copy_from_slice(&u32::MAX.to_le_bytes());
    let crc = crc32fast::hash(&bytes[..14]);
    bytes[14..18].copy_from_slice(&crc.to_le_bytes());

    assert!(read_hot_tail(&mut std::io::Cursor::new(bytes), 0, 32).is_none());
  }

  #[test]
  fn write_read_file_roundtrip() {
    let p = HotTailPayload {
      writes: vec![make_entry(0x11, 0x02, 1000, 80), make_entry(0x22, 0x03, 2000, 200)],
      voids: vec![VoidRecord { offset: 8888, size: 64 }],
    };
    let mut cursor = std::io::Cursor::new(Vec::new());
    let end = write_hot_tail(&mut cursor, 0, &p, 32).unwrap();
    assert!(end > 0);

    let got = read_hot_tail(&mut cursor, 0, 32).unwrap();
    assert_eq!(got.writes.len(), 2);
    assert_eq!(got.voids[0].offset, 8888);
  }

  #[test]
  fn write_at_nonzero_offset() {
    let p = HotTailPayload { writes: vec![make_entry(0xFF, 0x01, 999, 100)], voids: vec![] };
    let mut cursor = std::io::Cursor::new(vec![0u8; 1024]);
    let end = write_hot_tail(&mut cursor, 256, &p, 32).unwrap();
    assert!(end > 256);

    let got = read_hot_tail(&mut cursor, 256, 32).unwrap();
    assert_eq!(got.writes[0].offset, 999);
  }

  #[test]
  fn bounded_void_visitor_rejects_truncated_untrusted_counts() {
    let payload = HotTailPayload { writes: vec![], voids: vec![VoidRecord { offset: 100, size: 20 }] };
    let mut bytes = serialize_hot_tail(&payload, 32);
    bytes[10..14].copy_from_slice(&u32::MAX.to_le_bytes());
    let crc = crc32fast::hash(&bytes[..14]);
    bytes[14..18].copy_from_slice(&crc.to_le_bytes());
    let error = visit_hot_tail_voids(&mut std::io::Cursor::new(bytes), 0, 32, None, |_index, _void| Ok(())).unwrap_err();
    assert!(matches!(error, EngineError::CorruptEntry { .. }));
  }

  #[test]
  fn bounded_void_visitor_rejects_unknown_record_versions() {
    let payload = HotTailPayload { writes: vec![], voids: vec![VoidRecord { offset: 100, size: 20 }] };
    let mut bytes = serialize_hot_tail(&payload, 32);
    bytes[HOT_TAIL_HEADER_SIZE] = VOID_RECORD_VERSION + 1;
    let error = visit_hot_tail_voids(&mut std::io::Cursor::new(bytes), 0, 32, None, |_index, _void| Ok(())).unwrap_err();
    assert!(matches!(error, EngineError::CorruptEntry { reason, .. } if reason.contains("unsupported version")));
  }

  #[test]
  fn bounded_void_visitor_preserves_non_eof_io_failures() {
    let payload = HotTailPayload { writes: vec![], voids: vec![VoidRecord { offset: 100, size: 20 }] };
    let bytes = serialize_hot_tail(&payload, 32);
    let mut reader = FailAfterCursor { inner: std::io::Cursor::new(bytes), fail_at: HOT_TAIL_HEADER_SIZE as u64 };

    let error = visit_hot_tail_voids(&mut reader, 0, 32, None, |_index, _void| Ok(())).unwrap_err();

    assert!(matches!(error, EngineError::IoError(ref io) if io.kind() == std::io::ErrorKind::PermissionDenied));
  }

  #[test]
  fn bounded_void_visitor_honors_cancellation_before_record_reads() {
    let payload = HotTailPayload { writes: vec![], voids: vec![VoidRecord { offset: 100, size: 20 }] };
    let bytes = serialize_hot_tail(&payload, 32);
    let cancelled = std::sync::atomic::AtomicBool::new(true);

    let error = visit_hot_tail_voids(&mut std::io::Cursor::new(bytes), 0, 32, Some(&cancelled), |_index, _void| Ok(())).unwrap_err();

    assert!(matches!(error, EngineError::ShuttingDown));
  }
}
