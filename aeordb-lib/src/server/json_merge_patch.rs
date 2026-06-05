//! Compatibility re-export for the JSON Merge Patch primitive.
//!
//! The implementation lives in `engine::merge_patch` so embedded callers do
//! not have to depend on server-layer modules.

pub use crate::engine::merge_patch::{apply_merge_patch, MergeDepth};
