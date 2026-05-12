use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::kv_store::{KVEntry, KV_FLAG_DELETED};

pub use crate::engine::kv_stages::KV_STAGE_SIZES;

/// Maximum entries per bucket page.
pub const MAX_ENTRIES_PER_PAGE: usize = 32;

/// Magic bytes prefixed to every bucket page. Distinct from the file-level
/// magic so corrupted file headers can't be mistaken for valid pages.
pub const PAGE_MAGIC: u32 = 0xAE0D_B905;

/// Page header size: u32 magic + u32 crc32 + u16 entry_count = 10 bytes.
pub const PAGE_HEADER_SIZE: usize = 4 + 4 + 2;

/// Compute the byte size of one bucket page for a given hash length.
/// Layout: magic(u32) + crc32(u32) + entry_count(u16) +
///         MAX_ENTRIES_PER_PAGE * (hash + type_flags + offset)
pub fn page_size(hash_length: usize) -> usize {
    PAGE_HEADER_SIZE + MAX_ENTRIES_PER_PAGE * (hash_length + 1 + 8)
}

/// A freshly-zeroed page (all bytes zero, including magic) is the "empty"
/// page state on disk before any entries land. Distinguish this from a
/// torn-write corruption by checking the magic field. Empty pages have
/// magic == 0 and entry_count == 0 — both checked together so a zero-magic
/// page with non-zero entry_count is still flagged as corrupt.
fn is_empty_page(data: &[u8]) -> bool {
    if data.len() < PAGE_HEADER_SIZE {
        return false;
    }
    let magic = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let count = u16::from_le_bytes([data[8], data[9]]);
    magic == 0 && count == 0
}

/// Compute the file offset of bucket N's page within the KV file.
pub fn bucket_page_offset(bucket_index: usize, hash_length: usize) -> u64 {
    (bucket_index * page_size(hash_length)) as u64
}

/// Serialize a list of KV entries into a fixed-size bucket page.
/// The page is always `page_size(hash_length)` bytes, zero-padded.
/// Layout: magic + crc32 + entry_count + entries. The CRC32 covers the
/// entire page with the crc32 field itself zeroed during computation.
pub fn serialize_page(entries: &[KVEntry], hash_length: usize) -> Vec<u8> {
    let psize = page_size(hash_length);
    let mut buffer = vec![0u8; psize];
    let count = entries.len().min(MAX_ENTRIES_PER_PAGE);

    // magic
    buffer[0..4].copy_from_slice(&PAGE_MAGIC.to_le_bytes());
    // crc32 left as 0; filled in at the end
    // entry_count
    buffer[8..10].copy_from_slice(&(count as u16).to_le_bytes());

    let entry_size = hash_length + 1 + 8;
    for (i, entry) in entries.iter().take(count).enumerate() {
        let offset = PAGE_HEADER_SIZE + i * entry_size;
        let hash_len = entry.hash.len().min(hash_length);
        buffer[offset..offset + hash_len].copy_from_slice(&entry.hash[..hash_len]);
        buffer[offset + hash_length] = entry.type_flags;
        buffer[offset + hash_length + 1..offset + hash_length + 9]
            .copy_from_slice(&entry.offset.to_le_bytes());
    }

    // Compute CRC32 over the full page with crc32 field zeroed.
    let crc = crc32fast::hash(&buffer);
    buffer[4..8].copy_from_slice(&crc.to_le_bytes());

    buffer
}

/// Deserialize a bucket page from raw bytes into KV entries.
///
/// Validates the page magic + CRC32 before interpreting any entries. A page
/// with magic == 0 and entry_count == 0 is treated as the "empty page" sentinel
/// (freshly zeroed on disk) rather than as corruption. Any other validation
/// failure returns `CorruptEntry`, and the caller can either fall back to a
/// per-bucket rebuild (see disk-resident-kvs.md §4) or escalate to dirty
/// startup.
pub fn deserialize_page(data: &[u8], hash_length: usize) -> EngineResult<Vec<KVEntry>> {
    if data.len() < PAGE_HEADER_SIZE {
        return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: "Page data too short for header".to_string(),
        });
    }

    if is_empty_page(data) {
        return Ok(Vec::new());
    }

    let magic = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    if magic != PAGE_MAGIC {
        return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: format!(
                "page magic mismatch (got 0x{:08x}, expected 0x{:08x})",
                magic, PAGE_MAGIC
            ),
        });
    }

    let stored_crc = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    // Compute CRC over the page with the crc32 field zeroed.
    let mut tmp = data.to_vec();
    tmp[4..8].fill(0);
    let computed_crc = crc32fast::hash(&tmp);
    if stored_crc != computed_crc {
        return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: format!(
                "page CRC mismatch (stored {:08x}, computed {:08x})",
                stored_crc, computed_crc
            ),
        });
    }

    let count = u16::from_le_bytes([data[8], data[9]]) as usize;
    if count > MAX_ENTRIES_PER_PAGE {
        return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: format!(
                "Page entry count {} exceeds maximum {}",
                count, MAX_ENTRIES_PER_PAGE
            ),
        });
    }

    let entry_size = hash_length + 1 + 8;
    let required = PAGE_HEADER_SIZE + count * entry_size;
    if data.len() < required {
        return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: format!(
                "Page data too short: need {} bytes for {} entries, got {}",
                required, count, data.len()
            ),
        });
    }

    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let offset = PAGE_HEADER_SIZE + i * entry_size;
        let hash = data[offset..offset + hash_length].to_vec();
        let type_flags = data[offset + hash_length];
        let file_offset = u64::from_le_bytes(
            data[offset + hash_length + 1..offset + hash_length + 9]
                .try_into()
                .unwrap(),
        );
        entries.push(KVEntry {
            type_flags,
            hash,
            offset: file_offset,
        });
    }

    Ok(entries)
}

/// Find an entry by hash within a deserialized page, skipping deleted entries.
pub fn find_in_page<'a>(entries: &'a [KVEntry], hash: &[u8]) -> Option<&'a KVEntry> {
    entries
        .iter()
        .find(|e| e.hash == hash && (e.type_flags & KV_FLAG_DELETED) == 0)
}

/// Insert or update an entry in a page's entry list.
/// Returns `true` if the operation succeeded (entry fit in page).
/// Returns `false` if the page is full and the entry is new.
pub fn upsert_in_page(entries: &mut Vec<KVEntry>, entry: KVEntry) -> bool {
    if let Some(existing) = entries.iter_mut().find(|e| e.hash == entry.hash) {
        *existing = entry;
        true
    } else if entries.len() < MAX_ENTRIES_PER_PAGE {
        entries.push(entry);
        true
    } else {
        false // page full
    }
}

/// Determine the appropriate stage index for a given total entry count.
/// Returns the lowest stage whose bucket_count * MAX_ENTRIES_PER_PAGE > entry_count.
pub fn stage_for_count(entry_count: usize, hash_length: usize) -> usize {
    let psize = page_size(hash_length);
    for (stage, &block_size) in KV_STAGE_SIZES.iter().enumerate() {
        let buckets = crate::engine::kv_stages::buckets_for_block(block_size, psize);
        let capacity = buckets * MAX_ENTRIES_PER_PAGE;
        if entry_count < capacity {
            return stage;
        }
    }
    KV_STAGE_SIZES.len() - 1
}
