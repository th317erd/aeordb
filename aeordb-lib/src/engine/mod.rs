pub mod append_writer;
pub mod backup;
pub mod compression;
pub mod deletion_record;
pub mod directory_entry;
pub mod directory_ops;
pub mod engine_chunk_storage;
pub mod entry_header;
pub mod entry_scanner;
pub mod entry_type;
pub mod errors;
pub mod file_header;
pub mod file_record;
pub mod fuzzy;
pub mod group;
pub mod phonetic;
pub mod group_cache;
pub mod hash_algorithm;
pub mod index_config;
pub mod index_store;
pub mod indexing_pipeline;
pub mod json_parser;
pub mod kv_resize;
pub mod kv_store;
pub mod nvt;
pub mod nvt_ops;
pub mod path_utils;
pub mod permission_resolver;
pub mod permissions;
pub mod permissions_cache;
pub mod query_engine;
pub mod scalar_converter;
pub mod source_resolver;
pub mod storage_engine;
pub mod system_tables;
pub mod user;
pub mod tree_walker;
pub mod version_manager;
pub mod void_manager;
pub mod wasm_converter;

pub use append_writer::AppendWriter;
pub use compression::{CompressionAlgorithm, compress, decompress, should_compress};
pub use deletion_record::DeletionRecord;
pub use directory_entry::{ChildEntry, serialize_child_entries, deserialize_child_entries};
pub use entry_header::{EntryHeader, ENTRY_MAGIC};
pub use entry_scanner::{EntryScanner, ScannedEntry};
pub use entry_type::EntryType;
pub use errors::{EngineError, EngineResult};
pub use file_header::{FileHeader, FILE_HEADER_SIZE, FILE_MAGIC};
pub use file_record::FileRecord;
pub use hash_algorithm::HashAlgorithm;
pub use kv_store::{
  KVEntry, KVStore,
  KV_TYPE_CHUNK, KV_TYPE_FILE_RECORD, KV_TYPE_DIRECTORY, KV_TYPE_DELETION,
  KV_TYPE_SNAPSHOT, KV_TYPE_VOID, KV_TYPE_HEAD, KV_TYPE_FORK, KV_TYPE_VERSION,
  KV_FLAG_PENDING, KV_FLAG_DELETED,
};
pub use nvt::{NVTBucket, NormalizedVectorTable};
pub use nvt_ops::NVTMask;
pub use scalar_converter::{
  ScalarConverter, HashConverter,
  U8Converter, U16Converter, U32Converter, U64Converter,
  I64Converter, F64Converter, StringConverter, TimestampConverter,
  TrigramConverter, PhoneticConverter, PhoneticAlgorithm,
  serialize_converter, deserialize_converter,
  CONVERTER_TYPE_WASM, CONVERTER_TYPE_TRIGRAM, CONVERTER_TYPE_PHONETIC,
};
pub use fuzzy::{extract_trigrams, extract_trigrams_no_pad, trigram_similarity, auto_fuzziness, damerau_levenshtein, jaro_winkler};
pub use phonetic::{soundex, dmetaphone_primary, dmetaphone_alt};
pub use index_config::{IndexFieldConfig, PathIndexConfig, create_converter_from_config};
pub use index_store::{IndexEntry, FieldIndex, IndexManager};
pub use json_parser::parse_json_fields;
pub use source_resolver::{resolve_source, walk_path};
pub use kv_resize::KVResizeManager;
pub use path_utils::{normalize_path, parent_path, file_name, path_segments};
pub use void_manager::{VoidManager, MINIMUM_VOID_SIZE};
pub use engine_chunk_storage::EngineChunkStorage;
pub use storage_engine::StorageEngine;
pub use directory_ops::{DirectoryOps, EngineFileStream, directory_content_hash, directory_path_hash, file_path_hash};
pub use indexing_pipeline::IndexingPipeline;
pub use system_tables::{SystemTables, SystemTableError};
pub use query_engine::{QueryOp, FieldQuery, QueryNode, QueryStrategy, Query, QueryResult, QueryEngine, QueryBuilder, FieldQueryBuilder, should_use_bitmap_compositing, FuzzyOptions, Fuzziness, FuzzyAlgorithm};
pub use tree_walker::{walk_version_tree, diff_trees, VersionTree, TreeDiff};
pub use backup::{export_version, export_snapshot, ExportResult, create_patch, create_patch_from_snapshots, PatchResult};
pub use version_manager::{VersionManager, SnapshotInfo, ForkInfo};
pub use wasm_converter::{WasmConverter, WasmBatchConverter};
pub use user::{User, ROOT_USER_ID, validate_user_id, is_root, SAFE_QUERY_FIELDS};
pub use group::Group;
pub use group_cache::GroupCache;
pub use permission_resolver::{CrudlifyOp, PermissionResolver, path_levels};
pub use permissions::{PathPermissions, PermissionLink, parse_crudlify_flags, merge_flags};
pub use permissions_cache::PermissionsCache;
