//! Compatibility facade for the historical public NVT API.
//!
//! KV and v0 field indexes use focused wrappers. New v1 field indexes must use
//! the sparse fixed-point implementation introduced by Child 05 instead.

pub use crate::engine::legacy_nvt_v1::{LegacyNvtV1 as NormalizedVectorTable, NVTBucket};
