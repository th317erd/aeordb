// LEGACY: This entire module is superseded by the custom storage engine
// (src/engine/). It remains only because the HTTP /fs/ routes and 100+ tests
// still depend on it. Remove once /fs/ routes are migrated to the engine.

pub mod directory_entry;
pub mod path_resolver;
pub mod redb_directory;
pub mod version_manager;

pub use directory_entry::{DirectoryEntry, EntryType};
pub use path_resolver::{FileStream, PathError, PathResolver};
pub use redb_directory::RedbDirectory;
pub use version_manager::{VersionError, VersionInfo, VersionManager};
