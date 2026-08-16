//! Live v3 source adapter for the disconnected base-clone executor.

use super::migration_base_clone_execution::MigrationBaseCloneEntrySourceV1;
use crate::engine::entry_header::EntryHeader;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::native_durability::{PlatformFileIdentityDescriptorV1, platform_file_identity};
use crate::engine::storage_engine::{EntryData, StorageEngine};
use crate::engine::HashAlgorithm;

impl MigrationBaseCloneEntrySourceV1 for StorageEngine {
  fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algo()
  }

  fn physical_identity(&self) -> EngineResult<PlatformFileIdentityDescriptorV1> {
    platform_file_identity(self.database_path()).map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))
  }

  fn historical_entry_header(&self, hash: &[u8]) -> EngineResult<Option<EntryHeader>> {
    self.get_entry_header_including_deleted(hash)
  }

  fn historical_entry_verified_bounded(&self, hash: &[u8], maximum_value_length: u32) -> EngineResult<Option<EntryData>> {
    self.get_entry_including_deleted_verified_bounded(hash, maximum_value_length)
  }
}
