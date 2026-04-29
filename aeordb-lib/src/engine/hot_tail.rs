use crate::engine::errors::EngineResult;
use crate::engine::kv_store::KVEntry;
use std::io::{Read, Seek, SeekFrom, Write};

/// Magic bytes for the hot tail: 0xAE017DB100C (5 bytes).
/// "AE01 7DB 100C" — the database is running hot.
pub const HOT_TAIL_MAGIC: [u8; 5] = [0xAE, 0x01, 0x7D, 0xB1, 0x0C];

/// Total header size: magic(5) + entry_count(4) + crc32(4) = 13 bytes.
pub const HOT_TAIL_HEADER_SIZE: usize = 13;

/// Serialize hot tail entries to bytes.
/// Format: magic(5) + entry_count(4) + crc32_of_count(4) + entries.
pub fn serialize_hot_tail(entries: &[KVEntry], hash_length: usize) -> Vec<u8> {
    let entry_size = hash_length + 1 + 8; // hash + type_flags + offset
    let count = entries.len() as u32;
    let count_bytes = count.to_le_bytes();
    let crc = crc32fast::hash(&count_bytes);

    let mut buf = Vec::with_capacity(HOT_TAIL_HEADER_SIZE + entries.len() * entry_size);
    buf.extend_from_slice(&HOT_TAIL_MAGIC);
    buf.extend_from_slice(&count_bytes);
    buf.extend_from_slice(&crc.to_le_bytes());

    for entry in entries {
        let hash_bytes = &entry.hash;
        let copy_len = hash_length.min(hash_bytes.len());
        buf.extend_from_slice(&hash_bytes[..copy_len]);
        // Pad if hash is shorter than hash_length
        if hash_bytes.len() < hash_length {
            buf.resize(buf.len() + (hash_length - hash_bytes.len()), 0);
        }
        buf.push(entry.type_flags);
        buf.extend_from_slice(&entry.offset.to_le_bytes());
    }

    buf
}

/// Deserialize hot tail entries from bytes.
/// Returns None if magic or CRC doesn't match.
pub fn deserialize_hot_tail(data: &[u8], hash_length: usize) -> Option<Vec<KVEntry>> {
    if data.len() < HOT_TAIL_HEADER_SIZE {
        return None;
    }

    // Verify magic
    if data[..5] != HOT_TAIL_MAGIC {
        return None;
    }

    // Read and verify count
    let count_bytes: [u8; 4] = data[5..9].try_into().ok()?;
    let count = u32::from_le_bytes(count_bytes);
    let expected_crc = u32::from_le_bytes(data[9..13].try_into().ok()?);
    let actual_crc = crc32fast::hash(&count_bytes);
    if expected_crc != actual_crc {
        return None;
    }

    let entry_size = hash_length + 1 + 8;
    let expected_len = HOT_TAIL_HEADER_SIZE + (count as usize) * entry_size;
    if data.len() < expected_len {
        return None;
    }

    let mut entries = Vec::with_capacity(count as usize);
    let mut offset = HOT_TAIL_HEADER_SIZE;

    for _ in 0..count {
        let hash = data[offset..offset + hash_length].to_vec();
        offset += hash_length;
        let type_flags = data[offset];
        offset += 1;
        let entry_offset = u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
        offset += 8;

        entries.push(KVEntry {
            hash,
            offset: entry_offset,
            type_flags,
        });
    }

    Some(entries)
}

/// Write hot tail to a file at the given offset. Truncates the file after.
pub fn write_hot_tail<W: Write + Seek>(
    writer: &mut W,
    offset: u64,
    entries: &[KVEntry],
    hash_length: usize,
) -> EngineResult<u64> {
    let data = serialize_hot_tail(entries, hash_length);
    writer.seek(SeekFrom::Start(offset))?;
    writer.write_all(&data)?;
    let end = offset + data.len() as u64;
    Ok(end)
}

/// Read hot tail from a file at the given offset.
/// Returns None if the hot tail is corrupt or missing.
pub fn read_hot_tail<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    hash_length: usize,
) -> Option<Vec<KVEntry>> {
    if reader.seek(SeekFrom::Start(offset)).is_err() {
        return None;
    }

    // Read header first to get count
    let mut header = [0u8; HOT_TAIL_HEADER_SIZE];
    if reader.read_exact(&mut header).is_err() {
        return None;
    }

    if header[..5] != HOT_TAIL_MAGIC {
        return None;
    }

    let count_bytes: [u8; 4] = header[5..9].try_into().ok()?;
    let count = u32::from_le_bytes(count_bytes);
    let expected_crc = u32::from_le_bytes(header[9..13].try_into().ok()?);
    if crc32fast::hash(&count_bytes) != expected_crc {
        return None;
    }

    let entry_size = hash_length + 1 + 8;
    let entries_len = (count as usize) * entry_size;
    let mut entries_data = vec![0u8; entries_len];
    if reader.read_exact(&mut entries_data).is_err() {
        return None;
    }

    // Reassemble for deserialization
    let mut full = Vec::with_capacity(HOT_TAIL_HEADER_SIZE + entries_len);
    full.extend_from_slice(&header);
    full.extend_from_slice(&entries_data);
    deserialize_hot_tail(&full, hash_length)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(hash_val: u8, type_flags: u8, offset: u64) -> KVEntry {
        KVEntry {
            hash: vec![hash_val; 32],
            type_flags,
            offset,
        }
    }

    #[test]
    fn serialize_deserialize_roundtrip() {
        let entries = vec![
            make_entry(0xAA, 0x01, 100),
            make_entry(0xBB, 0x02, 200),
            make_entry(0xCC, 0x03, 300),
            make_entry(0xDD, 0x04, 400),
            make_entry(0xEE, 0x05, 500),
        ];
        let data = serialize_hot_tail(&entries, 32);
        let result = deserialize_hot_tail(&data, 32).unwrap();
        assert_eq!(result.len(), 5);
        for (orig, deser) in entries.iter().zip(result.iter()) {
            assert_eq!(orig.hash, deser.hash);
            assert_eq!(orig.type_flags, deser.type_flags);
            assert_eq!(orig.offset, deser.offset);
        }
    }

    #[test]
    fn empty_hot_tail() {
        let data = serialize_hot_tail(&[], 32);
        let result = deserialize_hot_tail(&data, 32).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn corrupt_magic_returns_none() {
        let mut data = serialize_hot_tail(&[make_entry(0xAA, 0x01, 100)], 32);
        data[0] = 0xFF; // corrupt magic
        assert!(deserialize_hot_tail(&data, 32).is_none());
    }

    #[test]
    fn corrupt_crc_returns_none() {
        let mut data = serialize_hot_tail(&[make_entry(0xAA, 0x01, 100)], 32);
        // Change entry_count without updating CRC
        data[5] = 99;
        assert!(deserialize_hot_tail(&data, 32).is_none());
    }

    #[test]
    fn truncated_data_returns_none() {
        let data = serialize_hot_tail(&[make_entry(0xAA, 0x01, 100)], 32);
        let truncated = &data[..data.len() / 2];
        assert!(deserialize_hot_tail(truncated, 32).is_none());
    }

    #[test]
    fn write_read_file_roundtrip() {
        let entries = vec![
            make_entry(0x11, 0x02, 1000),
            make_entry(0x22, 0x03, 2000),
        ];

        let mut cursor = std::io::Cursor::new(Vec::new());
        let end = write_hot_tail(&mut cursor, 0, &entries, 32).unwrap();
        assert!(end > 0);

        let result = read_hot_tail(&mut cursor, 0, 32).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].hash, vec![0x11; 32]);
        assert_eq!(result[1].offset, 2000);
    }

    #[test]
    fn write_at_nonzero_offset() {
        let entries = vec![make_entry(0xFF, 0x01, 999)];

        // Write some junk first, then hot tail at offset 100
        let mut cursor = std::io::Cursor::new(vec![0u8; 200]);
        let end = write_hot_tail(&mut cursor, 100, &entries, 32).unwrap();
        assert!(end > 100);

        let result = read_hot_tail(&mut cursor, 100, 32).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].offset, 999);
    }
}
