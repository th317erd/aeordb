pub mod api_key_rules;
pub mod append_writer;
pub mod binary_utils;
#[cfg(feature = "auto_heal_unimplemented")]
pub mod auto_heal;
pub mod cache;
pub mod cache_loaders;
pub mod cluster_join;
pub mod backup;
pub mod batch_commit;
pub mod btree;
pub mod compression;
pub mod content_type;
pub mod cron_scheduler;
pub mod disk_kv_store;
pub mod deletion_record;
pub mod email_config;
pub mod email_sender;
pub mod email_template;
pub mod directory_entry;
pub mod directory_listing;
pub mod directory_ops;
pub mod engine_counters;
pub mod engine_event;
pub mod entry_header;
pub mod entry_scanner;
pub mod entry_type;
pub mod errors;
pub mod event_bus;
pub mod heartbeat;
pub mod metrics_pulse;
pub mod file_header;
pub mod header_repair;
pub mod file_record;
pub mod fuzzy;
pub mod gc;
pub mod hot_tail;
pub mod group;
pub mod health;
pub mod phonetic;
pub mod hash_algorithm;
pub mod index_cleanup;
pub mod index_config;
pub mod index_store;
pub mod indexing_pipeline;
pub mod integrity_scanner;
pub mod native_parsers;
pub mod json_parser;
pub mod kv_pages;
pub mod kv_stages;
pub mod kv_expand;
pub mod kv_resize;
pub mod kv_store;
pub mod kv_snapshot;
pub mod lost_found;
pub mod nvt;
pub mod nvt_ops;
pub mod path_utils;
pub mod permission_resolver;
pub mod permissions;
pub mod query_engine;
pub mod rate_tracker;
pub mod request_context;
pub mod scalar_converter;
pub mod search;
pub mod source_resolver;
pub mod storage_engine;
pub mod symlink_record;
pub mod symlink_resolver;
pub mod system_store;
pub mod task_queue;
pub mod task_worker;
pub mod user;
pub mod verify;
pub mod conflict_store;
pub mod merge;
pub mod sync_apply;
pub mod sync_api;
pub mod sync_engine;
pub mod tree_walker;
pub mod version_access;
pub mod version_manager;
pub mod peer_connection;
pub mod virtual_clock;
pub mod void_manager;
pub mod webhook;

pub use cache::{Cache, CacheLoader};
pub use cache_loaders::{PermissionsLoader, GroupLoader, ApiKeyLoader, IndexConfigLoader};
pub use engine_counters::{EngineCounters, CountersSnapshot};
pub use api_key_rules::{KeyRule, match_rules, check_operation_permitted, validate_rules, parse_rules_from_json, operation_to_flag_char};
pub use batch_commit::{commit_files, CommitFile, CommitResult, CommittedFile};
pub use append_writer::AppendWriter;
pub use compression::{CompressionAlgorithm, compress, decompress, should_compress};
pub use content_type::detect_content_type;
pub use deletion_record::DeletionRecord;
pub use directory_entry::{ChildEntry, serialize_child_entries, deserialize_child_entries};
pub use directory_listing::{ListingEntry, list_directory_recursive};
pub use entry_header::{EntryHeader, ENTRY_MAGIC, CURRENT_ENTRY_VERSION, FLAG_SYSTEM};
pub use entry_scanner::{EntryScanner, ScannedEntry};
pub use entry_type::EntryType;
pub use errors::{EngineError, EngineResult};
pub use file_header::{FileHeader, FILE_HEADER_SIZE, FILE_MAGIC};
pub use header_repair::{inspect_header, repair_header_in_place, HeaderRepairReport, HotTailMismatch};
pub use file_record::FileRecord;
pub use hash_algorithm::HashAlgorithm;
pub use disk_kv_store::DiskKVStore;
pub use kv_pages::{
  KV_STAGE_SIZES, MAX_ENTRIES_PER_PAGE,
  page_size, bucket_page_offset, serialize_page, deserialize_page,
  find_in_page, upsert_in_page, stage_for_count,
};
pub use kv_snapshot::ReadSnapshot;
pub use kv_store::{
  KVEntry, KVStore,
  KV_TYPE_CHUNK, KV_TYPE_FILE_RECORD, KV_TYPE_DIRECTORY, KV_TYPE_DELETION,
  KV_TYPE_SNAPSHOT, KV_TYPE_VOID, KV_TYPE_HEAD, KV_TYPE_FORK, KV_TYPE_VERSION, KV_TYPE_SYMLINK,
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
  CONVERTER_TYPE_TRIGRAM, CONVERTER_TYPE_PHONETIC,
};
pub use fuzzy::{extract_trigrams, extract_trigrams_no_pad, trigram_similarity, auto_fuzziness, damerau_levenshtein, jaro_winkler};
pub use phonetic::{soundex, dmetaphone_primary, dmetaphone_alt};
pub use index_config::{IndexFieldConfig, PathIndexConfig, create_converter_from_config};
pub use index_cleanup::{IndexCleanupSender, spawn_index_cleanup_worker};
pub use index_store::{IndexEntry, FieldIndex, IndexManager};
pub use json_parser::parse_json_fields;
pub use search::{SearchResult, SearchResults, global_search};
pub use source_resolver::{resolve_source, resolve_sources, walk_path, walk_paths};
pub use symlink_record::{SymlinkRecord, symlink_path_hash, symlink_content_hash};
pub use symlink_resolver::{resolve_symlink, ResolvedTarget, MAX_SYMLINK_DEPTH};
pub use kv_resize::KVResizeManager;
pub use path_utils::{normalize_path, parent_path, file_name, path_segments};
pub use void_manager::{VoidManager, MINIMUM_VOID_SIZE};
pub use storage_engine::{StorageEngine, WriteBatch};
pub use directory_ops::{DirectoryOps, EngineFileStream, directory_content_hash, directory_path_hash, file_path_hash, file_content_hash, file_identity_hash, symlink_identity_hash, chunk_content_hash, system_chunk_hash, system_file_identity_hash, is_system_path, DEFAULT_CHUNK_SIZE};
pub use indexing_pipeline::IndexingPipeline;
pub use task_queue::{TaskQueue, TaskRecord, TaskStatus, ProgressInfo};
pub use query_engine::{QueryOp, FieldQuery, QueryNode, QueryStrategy, Query, QueryResult, QueryEngine, QueryBuilder, FieldQueryBuilder, should_use_bitmap_compositing, FuzzyOptions, Fuzziness, FuzzyAlgorithm, SortField, SortDirection, PaginatedResult, QueryMeta, DEFAULT_QUERY_LIMIT, AggregateQuery, AggregateResult, GroupResult, bytes_to_f64, bytes_to_json_value, is_numeric_type, ExplainMode, ExplainResult};
pub use gc::{gc_mark, gc_sweep, run_gc, GcResult};
pub use health::{HealthStatus, HealthReport, HealthChecks, EngineHealth, DiskHealth, SyncHealth, AuthHealth, check_engine, check_disk, check_sync, check_auth, compute_overall_status, full_health_check};
pub use merge::{three_way_merge, MergeResult, MergeOp, ConflictEntry, ConflictType, ConflictVersion};
pub use sync_apply::apply_merge_operations;
pub use sync_api::{
    compute_sync_diff, get_needed_chunks, apply_sync_chunks,
    list_conflicts_typed, file_history, file_restore_from_version,
    SyncDiff, SyncFileEntry, SyncSymlinkEntry, SyncDeletedEntry,
    ChunkData, ConflictRecord, ConflictVersionInfo, FileHistoryEntry,
};
pub use sync_engine::{SyncEngine, SyncConfig, PeerSyncState, SyncCycleResult, spawn_sync_loop};
pub use tree_walker::{walk_version_tree, walk_subtree, diff_trees, VersionTree, TreeDiff};
pub use version_access::{resolve_file_at_version, read_file_at_version};
pub use backup::{export_version, export_snapshot, export_full, backup_contains_system_data, ExportResult, create_patch, create_patch_from_snapshots, PatchResult, import_backup, ImportResult};
pub use version_manager::{VersionManager, SnapshotInfo, ForkInfo};
pub use cluster_join::{has_signing_key, is_ready_for_traffic, get_cluster_mode};
pub use peer_connection::{PeerConnection, PeerConfig, PeerManager, ConnectionState, SyncStatus};
pub use virtual_clock::{VirtualClock, SystemClock, MockClock, PeerClockTracker, PeerClockStats};
pub use user::{User, ROOT_USER_ID, validate_user_id, is_root, SAFE_QUERY_FIELDS};
pub use group::Group;
pub use permission_resolver::{CrudlifyOp, PermissionResolver, path_levels};
pub use permissions::{PathPermissions, PermissionLink, parse_crudlify_flags, merge_flags};
pub use engine_event::{
    EngineEvent, EntryEventData, VersionEventData, UserEventData,
    PermissionChangeData, ImportEventData, IndexEventData, ErrorEventData,
    TokenEventData, ApiKeyEventData, PluginEventData, HeartbeatData,
    EVENT_ENTRIES_CREATED, EVENT_ENTRIES_UPDATED, EVENT_ENTRIES_DELETED,
    EVENT_VERSIONS_CREATED, EVENT_VERSIONS_DELETED, EVENT_VERSIONS_PROMOTED, EVENT_VERSIONS_RESTORED,
    EVENT_USERS_CREATED, EVENT_USERS_ACTIVATED, EVENT_USERS_DEACTIVATED,
    EVENT_PERMISSIONS_CHANGED, EVENT_IMPORTS_COMPLETED, EVENT_INDEXES_UPDATED, EVENT_ERRORS,
    EVENT_TOKENS_EXCHANGED, EVENT_API_KEYS_CREATED, EVENT_API_KEYS_REVOKED,
    EVENT_PLUGINS_DEPLOYED, EVENT_PLUGINS_REMOVED, EVENT_HEARTBEAT, EVENT_FILES_SHARED,
    EVENT_GC_STARTED, EVENT_GC_COMPLETED, EVENT_METRICS, GcEventData,
    EVENT_TASKS_CREATED, EVENT_TASKS_STARTED, EVENT_TASKS_COMPLETED,
    EVENT_TASKS_FAILED, EVENT_TASKS_CANCELLED,
    EVENT_SYNCS_COMPLETED, EVENT_SYNCS_FAILED,
};
pub use event_bus::EventBus;
pub use heartbeat::spawn_heartbeat;
pub use integrity_scanner::spawn_integrity_scanner;
pub use metrics_pulse::{spawn_metrics_pulse, spawn_rate_sampler};
pub use task_worker::{spawn_task_worker, process_next_task};
pub use cron_scheduler::{CronSchedule, CronConfig, spawn_cron_scheduler, load_cron_config, save_cron_config, seed_default_cron_if_missing, validate_cron_expression};
pub use request_context::RequestContext;
pub use rate_tracker::{RateTracker, RateSnapshot, RateTrackerSet, RateSetSnapshot};
pub use webhook::{spawn_webhook_dispatcher, load_webhook_config, compute_signature, WebhookConfig, WebhookRegistry};
pub use btree::{
    BTreeNode, LeafNode, InternalNode,
    BTREE_MAX_LEAF_ENTRIES, BTREE_MIN_LEAF_ENTRIES,
    BTREE_MAX_INTERNAL_KEYS, BTREE_MIN_INTERNAL_KEYS,
    BTREE_CONVERSION_THRESHOLD, is_btree_format,
    btree_insert, btree_insert_with_data, btree_insert_batched,
    btree_lookup, btree_list, btree_list_from_node,
    btree_delete, btree_from_entries, store_btree_node,
};
