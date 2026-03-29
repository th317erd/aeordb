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

// Versions
pub const VERSION_SNAPSHOTS_TOTAL: &str = "aeordb_version_snapshots_total";
pub const VERSION_RESTORES_TOTAL: &str = "aeordb_version_restores_total";
pub const VERSION_SNAPSHOT_DURATION: &str = "aeordb_version_snapshot_duration_seconds";
pub const VERSION_RESTORE_DURATION: &str = "aeordb_version_restore_duration_seconds";
pub const VERSION_COUNT: &str = "aeordb_version_count";
