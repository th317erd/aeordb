//! Shared root-operation contracts for HTTP and trusted embedded callers.
//!
//! This module classifies execution authority; it does not parse selectors,
//! resolve roots, authorize paths, access storage, or activate v4 reads.

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RootOperationClassV1 {
  SingleRootNamespace,
  MultiRoot,
  ContentStaging,
  HashRetrieval,
  OperationalSystem,
  Mutation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootOperationProofV1 {
  ResolvedReadView,
  MultiRootResolver,
  ContentTransport,
  MutationRejectsGenericRoot,
  NoNamespace,
  PluginHost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RootOperationAdapterV1 {
  ResolveSingleRoot,
  ResolveMultipleRoots,
  TransportContent,
  RetrieveHashFromSelectedRoot,
  ExecuteOperational,
  PublishCurrentMutation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootServiceModeV1 {
  LegacyV3Compatibility,
}

/// The shared HTTP/embedded root-operation activation boundary.
///
/// P7 exposes only the inactive-v4 compatibility mode. The private field
/// prevents callers from manufacturing another mode before P8 adds a
/// migration-qualified constructor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootServiceActivationV1 {
  mode: RootServiceModeV1,
}

impl RootServiceActivationV1 {
  pub const fn inactive_v4() -> Self {
    Self { mode: RootServiceModeV1::LegacyV3Compatibility }
  }

  pub const fn mode(self) -> RootServiceModeV1 {
    self.mode
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootOperationPlanErrorV1 {
  ClassProofMismatch { class: RootOperationClassV1, proof: RootOperationProofV1 },
}

pub fn adapt_root_operation_v1(
  class: RootOperationClassV1,
  proof: RootOperationProofV1,
  activation: RootServiceActivationV1,
) -> Result<(RootOperationAdapterV1, RootServiceModeV1), RootOperationPlanErrorV1> {
  let adapter = match (class, proof) {
    (RootOperationClassV1::SingleRootNamespace, RootOperationProofV1::ResolvedReadView) => RootOperationAdapterV1::ResolveSingleRoot,
    (RootOperationClassV1::MultiRoot, RootOperationProofV1::MultiRootResolver) => RootOperationAdapterV1::ResolveMultipleRoots,
    (RootOperationClassV1::ContentStaging, RootOperationProofV1::ContentTransport) => RootOperationAdapterV1::TransportContent,
    (RootOperationClassV1::HashRetrieval, RootOperationProofV1::ResolvedReadView) => RootOperationAdapterV1::RetrieveHashFromSelectedRoot,
    (RootOperationClassV1::OperationalSystem, RootOperationProofV1::NoNamespace | RootOperationProofV1::PluginHost) => {
      RootOperationAdapterV1::ExecuteOperational
    }
    (RootOperationClassV1::Mutation, RootOperationProofV1::MutationRejectsGenericRoot) => RootOperationAdapterV1::PublishCurrentMutation,
    (class, proof) => return Err(RootOperationPlanErrorV1::ClassProofMismatch { class, proof }),
  };
  Ok((adapter, activation.mode()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EmbeddedOperationOwnerV1 {
  StorageEngine,
  DirectoryOps,
  QueryEngine,
  QueryBuilder,
  FieldQueryBuilder,
  PluginManager,
  WasmPluginRuntime,
  PluginHostImport,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbeddedOperationDispositionV1 {
  RootOperation { class: RootOperationClassV1, proof: RootOperationProofV1 },
  LocalOnly,
  InternalMaintenance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmbeddedRootOperationGroupV1 {
  pub owner: EmbeddedOperationOwnerV1,
  pub symbols: &'static [&'static str],
  pub disposition: EmbeddedOperationDispositionV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbeddedOperationExecutionV1 {
  RootOperation { adapter: RootOperationAdapterV1, service_mode: RootServiceModeV1 },
  LocalOnly,
  InternalMaintenance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmbeddedRootOperationPlanV1 {
  pub owner: EmbeddedOperationOwnerV1,
  pub symbol: &'static str,
  pub execution: EmbeddedOperationExecutionV1,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmbeddedRootOperationErrorV1 {
  UnknownOperation { owner: EmbeddedOperationOwnerV1, symbol: String },
  InvalidRootOperation(RootOperationPlanErrorV1),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmbeddedRootOperationRouterV1 {
  activation: RootServiceActivationV1,
}

impl EmbeddedRootOperationRouterV1 {
  pub const fn inactive_v4() -> Self {
    Self { activation: RootServiceActivationV1::inactive_v4() }
  }

  pub fn plan(self, owner: EmbeddedOperationOwnerV1, symbol: &str) -> Result<EmbeddedRootOperationPlanV1, EmbeddedRootOperationErrorV1> {
    let (group, symbol) = EMBEDDED_ROOT_OPERATION_GROUPS_V1
      .iter()
      .find_map(|group| {
        if group.owner != owner {
          return None;
        }
        group.symbols.iter().find(|candidate| **candidate == symbol).map(|selected| (group, *selected))
      })
      .ok_or_else(|| EmbeddedRootOperationErrorV1::UnknownOperation { owner, symbol: symbol.to_string() })?;
    let execution = match group.disposition {
      EmbeddedOperationDispositionV1::RootOperation { class, proof } => {
        let (adapter, service_mode) =
          adapt_root_operation_v1(class, proof, self.activation).map_err(EmbeddedRootOperationErrorV1::InvalidRootOperation)?;
        EmbeddedOperationExecutionV1::RootOperation { adapter, service_mode }
      }
      EmbeddedOperationDispositionV1::LocalOnly => EmbeddedOperationExecutionV1::LocalOnly,
      EmbeddedOperationDispositionV1::InternalMaintenance => EmbeddedOperationExecutionV1::InternalMaintenance,
    };
    Ok(EmbeddedRootOperationPlanV1 { owner, symbol, execution })
  }
}

const STORAGE_ENGINE_INTERNAL: &[&str] = &[
  "configuration_shadow",
  "configuration_snapshot",
  "replace_configuration_document",
  "patch_configuration_document",
  "query_runtime_snapshot",
  "soft_mutation_runtime_snapshot",
  "index_runtime_snapshot_v1",
  "index_coverage_registry_snapshot_v1",
  "index_runtime_coverage_snapshot_v1",
  "admit_index_maintenance_task_v1",
  "admit_index_maintenance_tasks_v1",
  "repair_kv_and_admit_index_maintenance_v1",
  "memory_coordinator",
  "durability_group_policy",
  "durability_group_policy_snapshot",
  "runtime_observability_snapshot",
  "gc_run_status",
  "gc_run_status_for_task",
  "kv_page_provider_stats",
  "memory_coordinator_snapshot",
  "begin_shutdown",
  "durability_failure",
  "durability_failure_state",
  "persistent_durability_recovery",
  "begin_explicit_durability_repair",
  "seed_durability_recovery_from_spills",
  "durability_snapshot",
  "emergency_spill_report",
  "wait_for_active_operations",
  "active_operations_snapshot",
  "create",
  "create_with_hot_dir",
  "create_with_hot_dir_and_configuration_overrides",
  "recover_after_emergency_spill_replay",
  "flush_index_buffer_if_due",
  "flush_index_runtime_if_due_v1",
  "flush_index_buffer",
  "index_buffer_stats",
  "evict_clean_index_cache",
  "open",
  "open_with_hot_dir",
  "open_with_hot_dir_and_progress",
  "open_with_hot_dir_progress_and_configuration_overrides",
  "open_for_import",
  "store_entry",
  "store_entry_with_flags",
  "store_entry_with_version",
  "store_entry_with_flags_and_version",
  "store_entry_compressed",
  "store_entry_compressed_with_flags",
  "begin_gc_recheck",
  "take_gc_recheck",
  "gc_recheck_contains",
  "end_gc_recheck",
  "get_entry",
  "get_entry_header",
  "get_entry_header_including_deleted",
  "get_entry_including_deleted",
  "get_entry_including_deleted_bounded",
  "get_entry_verified",
  "get_entry_verified_bounded",
  "get_entry_verified_including_deleted",
  "read_chunk",
  "get_chunk_metadata",
  "read_chunk_span_verified",
  "read_chunk_including_deleted",
  "read_chunk_verified",
  "read_chunk_verified_bounded",
  "read_chunk_verified_including_deleted",
  "has_entry",
  "writer_read_lock",
  "hash_algo",
  "compute_hash",
  "counters",
  "reconcile_counters_from_kv",
  "update_head",
  "head_hash",
  "backup_info",
  "set_backup_info",
  "store_entry_typed",
  "flush_batch",
  "flush_batch_and_update_head",
  "clear_dir_content_cache",
  "engine_cache_sizes",
  "memory_stats",
  "kv_observability_metrics",
  "kv_layout_metrics",
  "expand_kv_block_online",
  "is_entry_deleted",
  "mark_entry_deleted",
  "read_entry_header_at",
  "write_deletion_at",
  "write_void_at",
  "write_deletion_at_nosync",
  "write_void_at_nosync",
  "sync_writer",
  "remove_kv_entries_batch",
  "remove_kv_entry",
  "iter_kv_entries",
  "kv_entry_count",
  "get_kv_entry",
  "entries_by_type",
  "stats",
  "rebuild_kv",
  "rebuild_kv_with_progress",
  "end_transaction",
  "try_flush_hot_buffer",
  "shutdown",
];

const DIRECTORY_LOCAL: &[&str] = &["new"];
const DIRECTORY_INTERNAL: &[&str] = &[
  "migrate_file_record_to_current_version",
  "auto_snapshot_before_restore",
  "ensure_root_directory",
  "rebuild_directory_tree",
  "repair_directory_index_from_path_records",
  "canonical_directory_content_hash",
  "repair_stale_dir_key",
];
const DIRECTORY_SINGLE_ROOT: &[&str] = &[
  "read_file_streaming",
  "read_file_buffered",
  "list_directory",
  "list_directory_strict",
  "list_directory_with_traversal",
  "list_directory_with_btree_warnings",
  "list_directory_window",
  "list_directory_window_strict",
  "get_metadata",
  "exists",
  "get_symlink",
];
const DIRECTORY_CONTENT: &[&str] = &["store_chunk"];
const DIRECTORY_OPERATIONAL: &[&str] = &["list_deleted"];
const DIRECTORY_MUTATION: &[&str] = &[
  "apply_sync_merge",
  "store_file_buffered",
  "store_files_buffered_batch",
  "merge_json_file",
  "merge_json_files_batch",
  "store_file_from_reader",
  "finalize_file",
  "store_file_compressed",
  "restore_file_from_record",
  "delete_file",
  "delete_directory",
  "create_directory",
  "restore_deleted_file",
  "store_file_with_indexing",
  "store_file_with_full_pipeline",
  "delete_file_with_indexing",
  "store_symlink",
  "delete_symlink",
  "rename_file",
  "copy_file",
  "copy_path",
  "copy_paths",
  "rename_symlink",
];

const QUERY_ENGINE_LOCAL: &[&str] = &["new"];
const QUERY_ENGINE_SINGLE_ROOT: &[&str] = &[
  "execute",
  "execute_with_cancellation",
  "execute_paginated",
  "execute_paginated_filtered",
  "execute_paginated_with_cancellation",
  "execute_explain",
  "execute_explain_filtered",
  "execute_explain_with_cancellation",
  "execute_aggregate",
  "execute_aggregate_filtered",
  "execute_aggregate_with_cancellation",
];

const QUERY_BUILDER_LOCAL: &[&str] = &[
  "new",
  "field",
  "limit",
  "strategy",
  "order_by",
  "offset",
  "after",
  "before",
  "include_total",
  "cancellation_token",
  "and",
  "or",
  "not",
];
const QUERY_BUILDER_SINGLE_ROOT: &[&str] = &["all", "first", "count", "execute_paginated"];
const FIELD_QUERY_BUILDER_LOCAL: &[&str] = &[
  "eq",
  "gt",
  "lt",
  "between",
  "in_values",
  "eq_u64",
  "gt_u64",
  "lt_u64",
  "eq_i64",
  "gt_i64",
  "lt_i64",
  "eq_f64",
  "gt_f64",
  "lt_f64",
  "eq_str",
  "gt_str",
  "lt_str",
  "eq_bool",
  "between_u64",
  "between_str",
  "in_u64",
  "in_str",
  "contains",
  "similar",
  "phonetic",
  "fuzzy",
  "fuzzy_with",
  "match_query",
];

const PLUGIN_MANAGER_LOCAL: &[&str] = &["new"];
const PLUGIN_MANAGER_INTERNAL: &[&str] = &["install_bundled_plugins"];
const PLUGIN_MANAGER_MUTATION: &[&str] = &["deploy_plugin", "deploy_plugin_with_metadata", "remove_plugin"];
const PLUGIN_MANAGER_OPERATIONAL: &[&str] = &["get_plugin", "list_plugins", "invoke_wasm_plugin", "invoke_wasm_plugin_with_limits"];
const PLUGIN_MANAGER_HOST: &[&str] =
  &["invoke_wasm_plugin_with_context", "invoke_wasm_plugin_with_auth", "invoke_wasm_plugin_with_authority_engines"];

const WASM_RUNTIME_LOCAL: &[&str] = &["new", "with_limits", "call_handle"];
const WASM_RUNTIME_HOST: &[&str] = &["call_handle_with_context", "call_handle_with_authority_engines"];

const PLUGIN_HOST_SINGLE_ROOT: &[&str] =
  &["aeordb_read_file", "aeordb_extract_file", "aeordb_file_metadata", "aeordb_list_directory", "aeordb_query", "aeordb_aggregate"];
const PLUGIN_HOST_MUTATION: &[&str] = &["aeordb_write_file", "aeordb_delete_file"];
const PLUGIN_HOST_OPERATIONAL: &[&str] = &["log_message"];

const EMBEDDED_ROOT_OPERATION_GROUPS_V1: &[EmbeddedRootOperationGroupV1] = &[
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::StorageEngine,
    symbols: STORAGE_ENGINE_INTERNAL,
    disposition: EmbeddedOperationDispositionV1::InternalMaintenance,
  },
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::DirectoryOps,
    symbols: DIRECTORY_LOCAL,
    disposition: EmbeddedOperationDispositionV1::LocalOnly,
  },
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::DirectoryOps,
    symbols: DIRECTORY_INTERNAL,
    disposition: EmbeddedOperationDispositionV1::InternalMaintenance,
  },
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::DirectoryOps,
    symbols: DIRECTORY_SINGLE_ROOT,
    disposition: EmbeddedOperationDispositionV1::RootOperation {
      class: RootOperationClassV1::SingleRootNamespace,
      proof: RootOperationProofV1::ResolvedReadView,
    },
  },
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::DirectoryOps,
    symbols: DIRECTORY_CONTENT,
    disposition: EmbeddedOperationDispositionV1::RootOperation {
      class: RootOperationClassV1::ContentStaging,
      proof: RootOperationProofV1::ContentTransport,
    },
  },
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::DirectoryOps,
    symbols: DIRECTORY_OPERATIONAL,
    disposition: EmbeddedOperationDispositionV1::RootOperation {
      class: RootOperationClassV1::OperationalSystem,
      proof: RootOperationProofV1::NoNamespace,
    },
  },
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::DirectoryOps,
    symbols: DIRECTORY_MUTATION,
    disposition: EmbeddedOperationDispositionV1::RootOperation {
      class: RootOperationClassV1::Mutation,
      proof: RootOperationProofV1::MutationRejectsGenericRoot,
    },
  },
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::QueryEngine,
    symbols: QUERY_ENGINE_LOCAL,
    disposition: EmbeddedOperationDispositionV1::LocalOnly,
  },
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::QueryEngine,
    symbols: QUERY_ENGINE_SINGLE_ROOT,
    disposition: EmbeddedOperationDispositionV1::RootOperation {
      class: RootOperationClassV1::SingleRootNamespace,
      proof: RootOperationProofV1::ResolvedReadView,
    },
  },
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::QueryBuilder,
    symbols: QUERY_BUILDER_LOCAL,
    disposition: EmbeddedOperationDispositionV1::LocalOnly,
  },
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::QueryBuilder,
    symbols: QUERY_BUILDER_SINGLE_ROOT,
    disposition: EmbeddedOperationDispositionV1::RootOperation {
      class: RootOperationClassV1::SingleRootNamespace,
      proof: RootOperationProofV1::ResolvedReadView,
    },
  },
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::FieldQueryBuilder,
    symbols: FIELD_QUERY_BUILDER_LOCAL,
    disposition: EmbeddedOperationDispositionV1::LocalOnly,
  },
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::PluginManager,
    symbols: PLUGIN_MANAGER_LOCAL,
    disposition: EmbeddedOperationDispositionV1::LocalOnly,
  },
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::PluginManager,
    symbols: PLUGIN_MANAGER_INTERNAL,
    disposition: EmbeddedOperationDispositionV1::InternalMaintenance,
  },
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::PluginManager,
    symbols: PLUGIN_MANAGER_MUTATION,
    disposition: EmbeddedOperationDispositionV1::RootOperation {
      class: RootOperationClassV1::Mutation,
      proof: RootOperationProofV1::MutationRejectsGenericRoot,
    },
  },
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::PluginManager,
    symbols: PLUGIN_MANAGER_OPERATIONAL,
    disposition: EmbeddedOperationDispositionV1::RootOperation {
      class: RootOperationClassV1::OperationalSystem,
      proof: RootOperationProofV1::NoNamespace,
    },
  },
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::PluginManager,
    symbols: PLUGIN_MANAGER_HOST,
    disposition: EmbeddedOperationDispositionV1::RootOperation {
      class: RootOperationClassV1::OperationalSystem,
      proof: RootOperationProofV1::PluginHost,
    },
  },
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::WasmPluginRuntime,
    symbols: WASM_RUNTIME_LOCAL,
    disposition: EmbeddedOperationDispositionV1::LocalOnly,
  },
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::WasmPluginRuntime,
    symbols: WASM_RUNTIME_HOST,
    disposition: EmbeddedOperationDispositionV1::RootOperation {
      class: RootOperationClassV1::OperationalSystem,
      proof: RootOperationProofV1::PluginHost,
    },
  },
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::PluginHostImport,
    symbols: PLUGIN_HOST_SINGLE_ROOT,
    disposition: EmbeddedOperationDispositionV1::RootOperation {
      class: RootOperationClassV1::SingleRootNamespace,
      proof: RootOperationProofV1::ResolvedReadView,
    },
  },
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::PluginHostImport,
    symbols: PLUGIN_HOST_MUTATION,
    disposition: EmbeddedOperationDispositionV1::RootOperation {
      class: RootOperationClassV1::Mutation,
      proof: RootOperationProofV1::MutationRejectsGenericRoot,
    },
  },
  EmbeddedRootOperationGroupV1 {
    owner: EmbeddedOperationOwnerV1::PluginHostImport,
    symbols: PLUGIN_HOST_OPERATIONAL,
    disposition: EmbeddedOperationDispositionV1::RootOperation {
      class: RootOperationClassV1::OperationalSystem,
      proof: RootOperationProofV1::NoNamespace,
    },
  },
];

pub fn embedded_root_operation_groups_v1() -> &'static [EmbeddedRootOperationGroupV1] {
  EMBEDDED_ROOT_OPERATION_GROUPS_V1
}
