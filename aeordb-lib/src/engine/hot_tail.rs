use crate::engine::errors::EngineResult;
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
fn write_record_size(hash_length: usize) -> usize { 1 + hash_length + 1 + 8 + 4 }

/// Per-void-record size: version(1) + offset(8) + size(4) = 13 bytes.
const VOID_RECORD_SIZE: usize = 1 + 8 + 4;

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

/// Serialize the hot tail payload (writes + voids) into a single byte buffer.
pub fn serialize_hot_tail(payload: &HotTailPayload, hash_length: usize) -> Vec<u8> {
  let wsize = write_record_size(hash_length);
  let total = HOT_TAIL_HEADER_SIZE
    + payload.writes.len() * wsize
    + payload.voids.len() * VOID_RECORD_SIZE;

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
  if data.len() < HOT_TAIL_HEADER_SIZE { return None; }
  if data[..5] != HOT_TAIL_MAGIC { return None; }
  let format_version = data[5];
  if format_version != HOT_TAIL_FORMAT_VERSION { return None; }

  let write_count = u32::from_le_bytes(data[6..10].try_into().ok()?) as usize;
  let void_count  = u32::from_le_bytes(data[10..14].try_into().ok()?) as usize;
  let stored_crc  = u32::from_le_bytes(data[14..18].try_into().ok()?);
  let actual_crc  = crc32fast::hash(&data[..14]);
  if stored_crc != actual_crc { return None; }

  let wsize = write_record_size(hash_length);
  let expected_len = HOT_TAIL_HEADER_SIZE + write_count * wsize + void_count * VOID_RECORD_SIZE;
  if data.len() < expected_len { return None; }

  let mut writes = Vec::with_capacity(write_count);
  let mut cursor = HOT_TAIL_HEADER_SIZE;

  for _ in 0..write_count {
    // Per-record version is currently always WRITE_RECORD_VERSION; future
    // record-format changes branch here.
    let _record_version = data[cursor];
    cursor += 1;
    let hash = data[cursor..cursor + hash_length].to_vec();
    cursor += hash_length;
    let type_flags = data[cursor];
    cursor += 1;
    let offset = u64::from_le_bytes(data[cursor..cursor + 8].try_into().ok()?);
    cursor += 8;
    let total_length = u32::from_le_bytes(data[cursor..cursor + 4].try_into().ok()?);
    cursor += 4;
    writes.push(KVEntry { hash, type_flags, offset, total_length });
  }

  let mut voids = Vec::with_capacity(void_count);
  for _ in 0..void_count {
    let _record_version = data[cursor];
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
pub fn write_hot_tail<W: Write + Seek>(
  writer: &mut W,
  offset: u64,
  payload: &HotTailPayload,
  hash_length: usize,
) -> EngineResult<u64> {
  let data = serialize_hot_tail(payload, hash_length);
  writer.seek(SeekFrom::Start(offset))?;
  writer.write_all(&data)?;
  Ok(offset + data.len() as u64)
}

/// Read the hot tail payload from a file at the given offset.
/// Returns `None` for any failure (missing, torn, wrong version, etc.).
pub fn read_hot_tail<R: Read + Seek>(
  reader: &mut R,
  offset: u64,
  hash_length: usize,
) -> Option<HotTailPayload> {
  reader.seek(SeekFrom::Start(offset)).ok()?;

  let mut header = [0u8; HOT_TAIL_HEADER_SIZE];
  reader.read_exact(&mut header).ok()?;

  if header[..5] != HOT_TAIL_MAGIC { return None; }
  if header[5] != HOT_TAIL_FORMAT_VERSION { return None; }

  let write_count = u32::from_le_bytes(header[6..10].try_into().ok()?) as usize;
  let void_count  = u32::from_le_bytes(header[10..14].try_into().ok()?) as usize;
  let stored_crc  = u32::from_le_bytes(header[14..18].try_into().ok()?);
  if crc32fast::hash(&header[..14]) != stored_crc { return None; }

  let wsize = write_record_size(hash_length);
  let body_len = write_count * wsize + void_count * VOID_RECORD_SIZE;
  let mut body = vec![0u8; body_len];
  reader.read_exact(&mut body).ok()?;

  let mut full = Vec::with_capacity(HOT_TAIL_HEADER_SIZE + body_len);
  full.extend_from_slice(&header);
  full.extend_from_slice(&body);
  deserialize_hot_tail(&full, hash_length)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn make_entry(hash_val: u8, type_flags: u8, offset: u64, total_length: u32) -> KVEntry {
    KVEntry { hash: vec![hash_val; 32], type_flags, offset, total_length }
  }

  #[test]
  fn writes_only_roundtrip() {
    let p = HotTailPayload {
      writes: vec![
        make_entry(0xAA, 0x01, 100, 128),
        make_entry(0xBB, 0x02, 200, 256),
      ],
      voids: vec![],
    };
    let data = serialize_hot_tail(&p, 32);
    let got = deserialize_hot_tail(&data, 32).unwrap();
    assert_eq!(got.writes.len(), 2);
    assert_eq!(got.voids.len(), 0);
    assert_eq!(got.writes[0].total_length, 128);
    assert_eq!(got.writes[1].offset, 200);
  }

  #[test]
  fn voids_only_roundtrip() {
    let p = HotTailPayload {
      writes: vec![],
      voids: vec![VoidRecord { offset: 1000, size: 500 }, VoidRecord { offset: 5000, size: 256 }],
    };
    let data = serialize_hot_tail(&p, 32);
    let got = deserialize_hot_tail(&data, 32).unwrap();
    assert_eq!(got.writes.len(), 0);
    assert_eq!(got.voids, p.voids);
  }

  #[test]
  fn mixed_roundtrip() {
    let p = HotTailPayload {
      writes: vec![make_entry(0xCC, 0x03, 300, 512)],
      voids: vec![VoidRecord { offset: 2000, size: 128 }],
    };
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
  fn corrupt_magic_returns_none() {
    let p = HotTailPayload {
      writes: vec![make_entry(0xAA, 0x01, 100, 64)],
      voids: vec![],
    };
    let mut data = serialize_hot_tail(&p, 32);
    data[0] = 0xFF;
    assert!(deserialize_hot_tail(&data, 32).is_none());
  }

  #[test]
  fn corrupt_crc_returns_none() {
    let p = HotTailPayload {
      writes: vec![make_entry(0xAA, 0x01, 100, 64)],
      voids: vec![],
    };
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
    let p = HotTailPayload {
      writes: vec![make_entry(0xAA, 0x01, 100, 64)],
      voids: vec![VoidRecord { offset: 5, size: 9 }],
    };
    let data = serialize_hot_tail(&p, 32);
    let truncated = &data[..data.len() - 4];
    assert!(deserialize_hot_tail(truncated, 32).is_none());
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
    let p = HotTailPayload {
      writes: vec![make_entry(0xFF, 0x01, 999, 100)],
      voids: vec![],
    };
    let mut cursor = std::io::Cursor::new(vec![0u8; 1024]);
    let end = write_hot_tail(&mut cursor, 256, &p, 32).unwrap();
    assert!(end > 256);

    let got = read_hot_tail(&mut cursor, 256, 32).unwrap();
    assert_eq!(got.writes[0].offset, 999);
  }
}
