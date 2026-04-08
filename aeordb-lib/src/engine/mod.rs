pub mod append_writer;
pub mod backup;
pub mod btree;
pub mod compression;
pub mod content_type;
pub mod deletion_record;
pub mod directory_entry;
pub mod directory_ops;
pub mod engine_chunk_storage;
pub mod engine_event;
pub mod entry_header;
pub mod entry_scanner;
pub mod entry_type;
pub mod errors;
pub mod event_bus;
pub mod heartbeat;
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
pub mod request_context;
pub mod scalar_converter;
pub mod source_resolver;
pub mod storage_engine;
pub mod system_tables;
pub mod user;
pub mod tree_walker;
pub mod version_manager;
pub mod void_manager;
pub mod wasm_converter;
pub mod webhook;

pub use append_writer::AppendWriter;
pub use compression::{CompressionAlgorithm, compress, decompress, should_compress};
pub use content_type::detect_content_type;
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
pub use query_engine::{QueryOp, FieldQuery, QueryNode, QueryStrategy, Query, QueryResult, QueryEngine, QueryBuilder, FieldQueryBuilder, should_use_bitmap_compositing, FuzzyOptions, Fuzziness, FuzzyAlgorithm, SortField, SortDirection, PaginatedResult, DEFAULT_QUERY_LIMIT, AggregateQuery, AggregateResult, GroupResult, bytes_to_f64, bytes_to_json_value, is_numeric_type, ExplainMode, ExplainResult};
pub use tree_walker::{walk_version_tree, diff_trees, VersionTree, TreeDiff};
pub use backup::{export_version, export_snapshot, ExportResult, create_patch, create_patch_from_snapshots, PatchResult, import_backup, ImportResult};
pub use version_manager::{VersionManager, SnapshotInfo, ForkInfo};
pub use wasm_converter::{WasmConverter, WasmBatchConverter};
pub use user::{User, ROOT_USER_ID, validate_user_id, is_root, SAFE_QUERY_FIELDS};
pub use group::Group;
pub use group_cache::GroupCache;
pub use permission_resolver::{CrudlifyOp, PermissionResolver, path_levels};
pub use permissions::{PathPermissions, PermissionLink, parse_crudlify_flags, merge_flags};
pub use permissions_cache::PermissionsCache;
pub use engine_event::{
    EngineEvent, EntryEventData, VersionEventData, UserEventData,
    PermissionChangeData, ImportEventData, IndexEventData, ErrorEventData,
    TokenEventData, ApiKeyEventData, PluginEventData, HeartbeatData,
    EVENT_ENTRIES_CREATED, EVENT_ENTRIES_UPDATED, EVENT_ENTRIES_DELETED,
    EVENT_VERSIONS_CREATED, EVENT_VERSIONS_DELETED, EVENT_VERSIONS_PROMOTED, EVENT_VERSIONS_RESTORED,
    EVENT_USERS_CREATED, EVENT_USERS_ACTIVATED, EVENT_USERS_DEACTIVATED,
    EVENT_PERMISSIONS_CHANGED, EVENT_IMPORTS_COMPLETED, EVENT_INDEXES_UPDATED, EVENT_ERRORS,
    EVENT_TOKENS_EXCHANGED, EVENT_API_KEYS_CREATED, EVENT_API_KEYS_REVOKED,
    EVENT_PLUGINS_DEPLOYED, EVENT_PLUGINS_REMOVED, EVENT_HEARTBEAT,
};
pub use event_bus::EventBus;
pub use heartbeat::spawn_heartbeat;
pub use request_context::RequestContext;
pub use webhook::{spawn_webhook_dispatcher, load_webhook_config, compute_signature, WebhookConfig, WebhookRegistry};
pub use btree::{
    BTreeNode, LeafNode, InternalNode,
    BTREE_MAX_LEAF_ENTRIES, BTREE_MIN_LEAF_ENTRIES,
    BTREE_MAX_INTERNAL_KEYS, BTREE_MIN_INTERNAL_KEYS,
    BTREE_CONVERSION_THRESHOLD, is_btree_format,
    btree_insert, btree_lookup, btree_list, btree_list_from_node,
    btree_delete, btree_from_entries, store_btree_node,
};
