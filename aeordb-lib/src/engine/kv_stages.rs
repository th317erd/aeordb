/// KV block stage table with tiered growth.
///
/// Stages 0-2: 8x growth (cheap to relocate at small sizes)
/// Stages 3-4: 4x growth
/// Stages 5+: 2x growth (conservative at large sizes)
///
/// Each entry: (block_size_bytes, nvt_bucket_count)
pub const KV_STAGES: &[(u64, usize)] = &[
    (64 * 1024,                  1_024),   // Stage 0: 64KB, 1K buckets
    (512 * 1024,                 4_096),   // Stage 1: 512KB, 4K buckets
    (4 * 1024 * 1024,            8_192),   // Stage 2: 4MB, 8K buckets
    (32 * 1024 * 1024,          16_384),   // Stage 3: 32MB, 16K buckets
    (128 * 1024 * 1024,         32_768),   // Stage 4: 128MB, 32K buckets
    (512 * 1024 * 1024,         65_536),   // Stage 5: 512MB, 64K buckets
    (1024 * 1024 * 1024,        65_536),   // Stage 6: 1GB, 64K buckets
    (2 * 1024 * 1024 * 1024,   131_072),   // Stage 7: 2GB, 128K buckets
    (4 * 1024 * 1024 * 1024,   131_072),   // Stage 8: 4GB, 128K buckets
    (8 * 1024 * 1024 * 1024,   262_144),   // Stage 9: 8GB, 256K buckets
];

/// Get block size and bucket count for a stage.
/// For stages beyond the table, extrapolates with 2x growth.
pub fn stage_params(stage: usize) -> (u64, usize) {
    if stage < KV_STAGES.len() {
        KV_STAGES[stage]
    } else {
        let (last_size, last_buckets) = KV_STAGES[KV_STAGES.len() - 1];
        let extra = stage - (KV_STAGES.len() - 1);
        (last_size * (1u64 << extra), last_buckets * (1 << extra.min(4)))
    }
}

/// Get the initial stage (stage 0) block size.
pub fn initial_block_size() -> u64 {
    KV_STAGES[0].0
}

/// Get the initial bucket count.
pub fn initial_bucket_count() -> usize {
    KV_STAGES[0].1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_0_is_64kb() {
        let (size, buckets) = stage_params(0);
        assert_eq!(size, 64 * 1024);
        assert_eq!(buckets, 1_024);
    }

    #[test]
    fn tiered_growth() {
        // 8x growth for stages 0-2
        assert_eq!(stage_params(1).0, 512 * 1024);
        assert_eq!(stage_params(2).0, 4 * 1024 * 1024);
        // 8x for stage 3
        assert_eq!(stage_params(3).0, 32 * 1024 * 1024);
        // 4x for stage 4
        assert_eq!(stage_params(4).0, 128 * 1024 * 1024);
    }

    #[test]
    fn extrapolation_beyond_table() {
        let (s9_size, _) = stage_params(9);
        let (s10_size, _) = stage_params(10);
        assert_eq!(s10_size, s9_size * 2); // 2x beyond table
    }
}
