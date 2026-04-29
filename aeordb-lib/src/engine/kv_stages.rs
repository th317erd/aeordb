/// KV block stage table with tiered growth.
///
/// Stages 0-2: 8x growth (cheap to relocate at small sizes)
/// Stages 3-4: 4x growth
/// Stages 5+: 2x growth (conservative at large sizes)
///
/// Only block sizes are stored. Bucket counts are derived from block size
/// and page size to guarantee pages always fit within the block.
pub const KV_STAGE_SIZES: &[u64] = &[
    64 * 1024,                  // Stage 0: 64KB
    512 * 1024,                 // Stage 1: 512KB
    4 * 1024 * 1024,            // Stage 2: 4MB
    32 * 1024 * 1024,           // Stage 3: 32MB
    128 * 1024 * 1024,          // Stage 4: 128MB
    512 * 1024 * 1024,          // Stage 5: 512MB
    1024 * 1024 * 1024,         // Stage 6: 1GB
    2 * 1024 * 1024 * 1024,     // Stage 7: 2GB
    4 * 1024 * 1024 * 1024,     // Stage 8: 4GB
    8 * 1024 * 1024 * 1024,     // Stage 9: 8GB
];

/// Compute bucket count from block size and page size.
/// Ensures all buckets fit within the block.
pub fn buckets_for_block(block_size: u64, page_size: usize) -> usize {
    (block_size as usize) / page_size
}

/// Get block size and bucket count for a stage.
/// Bucket count is derived from block size and the given page size.
pub fn stage_params(stage: usize, page_size: usize) -> (u64, usize) {
    let block_size = if stage < KV_STAGE_SIZES.len() {
        KV_STAGE_SIZES[stage]
    } else {
        let last_size = KV_STAGE_SIZES[KV_STAGE_SIZES.len() - 1];
        let extra = (stage - (KV_STAGE_SIZES.len() - 1)).min(53);
        last_size.saturating_mul(1u64 << extra)
    };
    let buckets = buckets_for_block(block_size, page_size);
    (block_size, buckets)
}

/// Get the initial stage (stage 0) block size.
pub fn initial_block_size() -> u64 {
    KV_STAGE_SIZES[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    // BLAKE3_256 hash: page_size = 2 + 32*(32+1+8) = 1314 bytes
    const TEST_PAGE_SIZE: usize = 1314;

    #[test]
    fn stage_0_is_64kb() {
        let (size, buckets) = stage_params(0, TEST_PAGE_SIZE);
        assert_eq!(size, 64 * 1024);
        assert!(buckets > 0);
        assert!(buckets * TEST_PAGE_SIZE <= size as usize);
    }

    #[test]
    fn buckets_fit_in_block() {
        for stage in 0..10 {
            let (size, buckets) = stage_params(stage, TEST_PAGE_SIZE);
            assert!(
                buckets * TEST_PAGE_SIZE <= size as usize,
                "Stage {}: {} buckets * {} page_size = {} > {} block_size",
                stage, buckets, TEST_PAGE_SIZE, buckets * TEST_PAGE_SIZE, size,
            );
        }
    }

    #[test]
    fn tiered_growth() {
        assert_eq!(stage_params(1, TEST_PAGE_SIZE).0, 512 * 1024);
        assert_eq!(stage_params(2, TEST_PAGE_SIZE).0, 4 * 1024 * 1024);
        assert_eq!(stage_params(3, TEST_PAGE_SIZE).0, 32 * 1024 * 1024);
        assert_eq!(stage_params(4, TEST_PAGE_SIZE).0, 128 * 1024 * 1024);
    }

    #[test]
    fn extrapolation_beyond_table() {
        let (s9_size, _) = stage_params(9, TEST_PAGE_SIZE);
        let (s10_size, _) = stage_params(10, TEST_PAGE_SIZE);
        assert_eq!(s10_size, s9_size * 2);
    }
}
