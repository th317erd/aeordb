// Storage
pub const CHUNKS_STORED_TOTAL: &str = "aeordb_chunks_stored_total";
pub const CHUNKS_READ_TOTAL: &str = "aeordb_chunks_read_total";
pub const CHUNKS_DEDUPLICATED_TOTAL: &str = "aeordb_chunks_deduplicated_total";
pub const CHUNK_STORE_BYTES: &str = "aeordb_chunk_store_bytes_total";
pub const CHUNK_STORE_COUNT: &str = "aeordb_chunk_store_count";
pub const CHUNK_WRITE_DURATION: &str = "aeordb_chunk_write_duration_seconds";
pub const CHUNK_READ_DURATION: &str = "aeordb_chunk_read_duration_seconds";

// Filesystem
pub const PATH_RESOLVE_DURATION: &str = "aeordb_path_resolve_duration_seconds";
pub const FILE_STORE_DURATION: &str = "aeordb_file_store_duration_seconds";
pub const FILE_READ_DURATION: &str = "aeordb_file_read_duration_seconds";
pub const FILE_DELETE_DURATION: &str = "aeordb_file_delete_duration_seconds";
pub const DIRECTORY_LIST_DURATION: &str = "aeordb_directory_list_duration_seconds";
pub const DIRECTORIES_CREATED_TOTAL: &str = "aeordb_directories_created_total";
pub const FILES_STORED_TOTAL: &str = "aeordb_files_stored_total";
pub const FILES_READ_TOTAL: &str = "aeordb_files_read_total";
pub const FILES_DELETED_TOTAL: &str = "aeordb_files_deleted_total";
pub const FILE_BYTES_STORED_TOTAL: &str = "aeordb_file_bytes_stored_total";
pub const FILE_BYTES_READ_TOTAL: &str = "aeordb_file_bytes_read_total";

// HTTP
pub const HTTP_REQUESTS_TOTAL: &str = "aeordb_http_requests_total";
pub const HTTP_REQUEST_DURATION: &str = "aeordb_http_request_duration_seconds";
pub const HTTP_REQUEST_BYTES: &str = "aeordb_http_request_bytes_total";
pub const HTTP_RESPONSE_BYTES: &str = "aeordb_http_response_bytes_total";

// Auth
pub const AUTH_VALIDATIONS_TOTAL: &str = "aeordb_auth_validations_total";
pub const AUTH_TOKEN_EXCHANGES_TOTAL: &str = "aeordb_auth_token_exchanges_total";
pub const AUTH_RATE_LIMIT_HITS_TOTAL: &str = "aeordb_auth_rate_limit_hits_total";

// Plugins
pub const PLUGIN_INVOCATIONS_TOTAL: &str = "aeordb_plugin_invocations_total";
pub const PLUGIN_DURATION: &str = "aeordb_plugin_duration_seconds";
pub const PLUGIN_ERRORS_TOTAL: &str = "aeordb_plugin_errors_total";

// Sync
pub const SYNC_CYCLES_TOTAL: &str = "aeordb_sync_cycles_total";
pub const SYNC_CONSECUTIVE_FAILURES: &str = "aeordb_sync_consecutive_failures";

// Cleanup
pub const CLEANUP_TOKENS_TOTAL: &str = "aeordb_cleanup_tokens_total";
pub const CLEANUP_LINKS_TOTAL: &str = "aeordb_cleanup_links_total";

// Query
pub const QUERY_DURATION: &str = "aeordb_query_duration_seconds";

// KV Store
pub const KV_FLUSH_DURATION: &str = "aeordb_kv_flush_duration_seconds";

// Memory
pub const PROCESS_RSS_BYTES: &str = "aeordb_process_rss_bytes";
pub const PROCESS_PEAK_RSS_BYTES: &str = "aeordb_process_peak_rss_bytes";
pub const PROCESS_VIRTUAL_BYTES: &str = "aeordb_process_virtual_bytes";
pub const PROCESS_DATA_BYTES: &str = "aeordb_process_data_bytes";
pub const PROCESS_SWAP_BYTES: &str = "aeordb_process_swap_bytes";
pub const PROCESS_THREAD_COUNT: &str = "aeordb_process_thread_count";
pub const PROCESS_FD_COUNT: &str = "aeordb_process_fd_count";
pub const PROCESS_PRIVATE_BYTES: &str = "aeordb_process_private_bytes";
pub const PROCESS_SHARED_BYTES: &str = "aeordb_process_shared_bytes";
pub const PROCESS_MAPPED_BYTES: &str = "aeordb_process_mapped_bytes";
pub const PROCESS_ALLOCATOR_BYTES: &str = "aeordb_process_allocator_bytes";
pub const MEMORY_OBSERVED_BYTES: &str = "aeordb_memory_observed_bytes";
pub const MEMORY_RESERVED_BYTES: &str = "aeordb_memory_reserved_bytes";
pub const MEMORY_CRITICAL_RESERVED_BYTES: &str = "aeordb_memory_critical_reserved_bytes";
pub const MEMORY_ACCOUNTED_BYTES: &str = "aeordb_memory_accounted_bytes";
pub const MEMORY_UNACCOUNTED_RSS_BYTES: &str = "aeordb_memory_unaccounted_rss_bytes";
pub const MEMORY_REJECTED_RESERVATIONS: &str = "aeordb_memory_rejected_reservations";
pub const MEMORY_DEFERRED_RESERVATIONS: &str = "aeordb_memory_deferred_reservations";
pub const MEMORY_PRESSURE: &str = "aeordb_memory_pressure";
pub const MEMORY_MAINTENANCE_PAUSED: &str = "aeordb_memory_maintenance_paused";
pub const MEMORY_OWNER_RESIDENT_BYTES: &str = "aeordb_memory_owner_resident_bytes";
pub const MEMORY_OWNER_CLEAN_BYTES: &str = "aeordb_memory_owner_clean_bytes";
pub const MEMORY_OWNER_DIRTY_BYTES: &str = "aeordb_memory_owner_dirty_bytes";
pub const MEMORY_OWNER_EVICTABLE_BYTES: &str = "aeordb_memory_owner_evictable_bytes";
pub const MEMORY_OWNER_PINNED_BYTES: &str = "aeordb_memory_owner_pinned_bytes";
pub const MEMORY_OWNER_SPILL_BYTES: &str = "aeordb_memory_owner_spill_bytes";
pub const MEMORY_OWNER_RESERVED_BYTES: &str = "aeordb_memory_owner_reserved_bytes";
pub const MEMORY_OWNER_CRITICAL_RESERVED_BYTES: &str = "aeordb_memory_owner_critical_reserved_bytes";
pub const MEMORY_OWNER_PEAK_RESERVED_BYTES: &str = "aeordb_memory_owner_peak_reserved_bytes";
pub const MEMORY_OWNER_ACTIVE_RESERVATIONS: &str = "aeordb_memory_owner_active_reservations";
pub const MEMORY_OWNER_ITEMS: &str = "aeordb_memory_owner_items";
pub const MEMORY_OWNER_HITS: &str = "aeordb_memory_owner_hits";
pub const MEMORY_OWNER_MISSES: &str = "aeordb_memory_owner_misses";
pub const MEMORY_OWNER_EVICTIONS: &str = "aeordb_memory_owner_evictions";
pub const MEMORY_OWNER_REJECTIONS: &str = "aeordb_memory_owner_rejections";
pub const MEMORY_OWNER_DEFERRALS: &str = "aeordb_memory_owner_deferrals";
pub const ENGINE_MEMORY_ESTIMATED_BYTES: &str = "aeordb_engine_memory_estimated_bytes";
pub const INDEX_CACHE_ESTIMATED_BYTES: &str = "aeordb_index_cache_estimated_bytes";
pub const INDEX_CACHE_ESTIMATED_CLEAN_BYTES: &str = "aeordb_index_cache_estimated_clean_bytes";
pub const INDEX_CACHE_ESTIMATED_DIRTY_BYTES: &str = "aeordb_index_cache_estimated_dirty_bytes";
pub const INDEX_CACHE_CLEAN_RESERVED_BYTES: &str = "aeordb_index_cache_clean_reserved_bytes";
pub const INDEX_CACHE_DIRTY_RESERVED_BYTES: &str = "aeordb_index_cache_dirty_reserved_bytes";
pub const INDEX_CACHE_FLUSH_RESERVED_BYTES: &str = "aeordb_index_cache_flush_reserved_bytes";
pub const INDEX_CACHE_CACHED_INDEXES: &str = "aeordb_index_cache_cached_indexes";
pub const INDEX_CACHE_DIRTY_INDEXES: &str = "aeordb_index_cache_dirty_indexes";
pub const INDEX_CACHE_FLUSHING_INDEXES: &str = "aeordb_index_cache_flushing_indexes";
pub const INDEX_CACHE_PENDING_MUTATIONS: &str = "aeordb_index_cache_pending_mutations";
pub const INDEX_CACHE_MAX_BYTES: &str = "aeordb_index_cache_max_bytes";
pub const INDEX_MUTATION_BUFFER_MAX_BYTES: &str = "aeordb_index_mutation_buffer_max_bytes";
pub const INDEX_PUBLICATION_BATCH_MAX_BYTES: &str = "aeordb_index_publication_batch_max_bytes";
pub const INDEX_CACHE_EVICTIONS: &str = "aeordb_index_cache_evictions";
pub const INDEX_CACHE_EVICTED_INDEXES: &str = "aeordb_index_cache_evicted_indexes";
pub const INDEX_CACHE_EVICTED_BYTES: &str = "aeordb_index_cache_evicted_bytes";
pub const INDEX_CACHE_ENTRIES: &str = "aeordb_index_cache_entries";
pub const INDEX_CACHE_VALUES: &str = "aeordb_index_cache_values";
pub const DIRECTORY_CACHE_ESTIMATED_BYTES: &str = "aeordb_directory_cache_estimated_bytes";
pub const DIRECTORY_CACHE_ENTRIES: &str = "aeordb_directory_cache_entries";

// Durability
pub const DURABILITY_HARD_FRONTIER: &str = "aeordb_durability_hard_frontier";
pub const DURABILITY_NEXT_SEQUENCE: &str = "aeordb_durability_next_sequence";
pub const DURABILITY_WAITER_DEPTH: &str = "aeordb_durability_waiter_depth";
pub const DURABILITY_PENDING_HARD: &str = "aeordb_durability_pending_hard";
pub const DURABILITY_OLDEST_WAITER_AGE_MS: &str = "aeordb_durability_oldest_waiter_age_ms";
pub const DURABILITY_LAST_BARRIER_LATENCY_MS: &str = "aeordb_durability_last_barrier_latency_ms";
pub const DURABILITY_LAST_BARRIER_SUCCESS: &str = "aeordb_durability_last_barrier_success";
pub const DURABILITY_GROUP_COMMIT_ENABLED: &str = "aeordb_durability_group_commit_enabled";
pub const DURABILITY_GROUP_COMMIT_MAX_BYTES: &str = "aeordb_durability_group_commit_max_bytes";
pub const DURABILITY_GROUP_COMMIT_MAX_DELAY_MS: &str = "aeordb_durability_group_commit_max_delay_ms";
pub const DURABILITY_READ_ONLY: &str = "aeordb_durability_read_only";
pub const DURABILITY_SPILL_COUNT: &str = "aeordb_durability_spill_count";
pub const DURABILITY_SPILL_BYTES: &str = "aeordb_durability_spill_bytes";
pub const DURABILITY_REPAIR_REQUIRED: &str = "aeordb_durability_repair_required";

// Configuration
pub const CONFIGURATION_FAMILY_VALID: &str = "aeordb_configuration_family_valid";
pub const CONFIGURATION_FAMILY_DEGRADED: &str = "aeordb_configuration_family_degraded";
pub const CONFIGURATION_PENDING_RESTART: &str = "aeordb_configuration_pending_restart";
pub const CONFIGURATION_PENDING_CONVERGENCE: &str = "aeordb_configuration_pending_convergence";
pub const CONFIGURATION_DISABLED_CAPABILITIES: &str = "aeordb_configuration_disabled_capabilities";
pub const CONFIGURATION_PROPERTY_ACTIVE: &str = "aeordb_configuration_property_active";

// Versions
pub const VERSION_SNAPSHOTS_TOTAL: &str = "aeordb_version_snapshots_total";
pub const VERSION_RESTORES_TOTAL: &str = "aeordb_version_restores_total";
pub const VERSION_SNAPSHOT_DURATION: &str = "aeordb_version_snapshot_duration_seconds";
pub const VERSION_RESTORE_DURATION: &str = "aeordb_version_restore_duration_seconds";
pub const VERSION_COUNT: &str = "aeordb_version_count";
