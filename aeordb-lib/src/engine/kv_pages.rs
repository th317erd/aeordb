use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::kv_store::{KVEntry, KV_FLAG_DELETED};

/// Stage table for KV block growth.
/// Each stage: (max_file_size_bytes, nvt_bucket_count)
pub const KV_STAGES: &[(u64, usize)] = &[
    (64 * 1024,              1_024),   // Stage 0: 64KB, 1K buckets
    (256 * 1024,             4_096),   // Stage 1: 256KB, 4K buckets
    (1024 * 1024,            8_192),   // Stage 2: 1MB, 8K buckets
    (4 * 1024 * 1024,       16_384),   // Stage 3: 4MB, 16K buckets
    (16 * 1024 * 1024,      32_768),   // Stage 4: 16MB, 32K buckets
    (64 * 1024 * 1024,      65_536),   // Stage 5: 64MB, 64K buckets
    (256 * 1024 * 1024,     65_536),   // Stage 6: 256MB, 64K buckets
    (1024 * 1024 * 1024,   131_072),   // Stage 7: 1GB, 128K buckets
];

/// Maximum entries per bucket page.
pub const MAX_ENTRIES_PER_PAGE: usize = 32;

/// Compute the byte size of one bucket page for a given hash length.
/// Layout: entry_count(u16) + MAX_ENTRIES_PER_PAGE * (hash + type_flags + offset)
pub fn page_size(hash_length: usize) -> usize {
    2 + MAX_ENTRIES_PER_PAGE * (hash_length + 1 + 8)
}

/// Compute the file offset of bucket N's page within the KV file.
pub fn bucket_page_offset(bucket_index: usize, hash_length: usize) -> u64 {
    (bucket_index * page_size(hash_length)) as u64
}

/// Serialize a list of KV entries into a fixed-size bucket page.
/// The page is always `page_size(hash_length)` bytes, zero-padded.
pub fn serialize_page(entries: &[KVEntry], hash_length: usize) -> Vec<u8> {
    let psize = page_size(hash_length);
    let mut buffer = vec![0u8; psize];
    let count = entries.len().min(MAX_ENTRIES_PER_PAGE);

    buffer[0..2].copy_from_slice(&(count as u16).to_le_bytes());

    let entry_size = hash_length + 1 + 8;
    for (i, entry) in entries.iter().take(count).enumerate() {
        let offset = 2 + i * entry_size;
        let hash_len = entry.hash.len().min(hash_length);
        buffer[offset..offset + hash_len].copy_from_slice(&entry.hash[..hash_len]);
        buffer[offset + hash_length] = entry.type_flags;
        buffer[offset + hash_length + 1..offset + hash_length + 9]
            .copy_from_slice(&entry.offset.to_le_bytes());
    }

    buffer
}

/// Deserialize a bucket page from raw bytes into KV entries.
pub fn deserialize_page(data: &[u8], hash_length: usize) -> EngineResult<Vec<KVEntry>> {
    if data.len() < 2 {
        return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: "Page data too short for header".to_string(),
        });
    }

    let count = u16::from_le_bytes([data[0], data[1]]) as usize;
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
    let required = 2 + count * entry_size;
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
        let offset = 2 + i * entry_size;
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
pub fn stage_for_count(entry_count: usize, _hash_length: usize) -> usize {
    for (stage, (_block_size, buckets)) in KV_STAGES.iter().enumerate() {
        let capacity = buckets * MAX_ENTRIES_PER_PAGE;
        if entry_count < capacity {
            return stage;
        }
    }
    KV_STAGES.len() - 1
}
