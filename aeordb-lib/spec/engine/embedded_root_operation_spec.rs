use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use aeordb::engine::root_operation::{
  EmbeddedOperationDispositionV1, EmbeddedOperationExecutionV1, EmbeddedOperationOwnerV1, EmbeddedRootOperationErrorV1,
  EmbeddedRootOperationRouterV1, RootOperationAdapterV1, RootOperationClassV1, RootOperationProofV1, RootServiceModeV1,
  embedded_root_operation_groups_v1,
};
use aeordb::server::root_api::{ReadViewProofV1, RootRequestAdapterV1, RootRouteClassV1};
use syn::{ImplItem, Item, Type, Visibility};

fn manifest() -> &'static Path {
  Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn public_methods(relative_path: &str, type_name: &str) -> BTreeSet<String> {
  let source = fs::read_to_string(manifest().join(relative_path)).unwrap();
  let file = syn::parse_file(&source).unwrap();
  let mut methods = BTreeSet::new();
  for item in file.items {
    let Item::Impl(item_impl) = item else {
      continue;
    };
    let Type::Path(self_type) = item_impl.self_ty.as_ref() else {
      continue;
    };
    if self_type.path.segments.last().map(|segment| segment.ident.to_string()).as_deref() != Some(type_name) {
      continue;
    }
    for item in item_impl.items {
      if let ImplItem::Fn(method) = item {
        if matches!(method.vis, Visibility::Public(_)) {
          assert!(methods.insert(method.sig.ident.to_string()), "duplicate public method on {type_name}");
        }
      }
    }
  }
  methods
}

fn plugin_host_imports() -> BTreeSet<String> {
  let source = fs::read_to_string(manifest().join("src/plugins/wasm_runtime.rs")).unwrap();
  let mut imports = BTreeSet::new();
  let marker = ".func_wrap(\"aeordb\", \"";
  for line in source.lines() {
    let Some(start) = line.find(marker) else {
      continue;
    };
    let remainder = &line[start + marker.len()..];
    let end = remainder.find('"').unwrap();
    assert!(imports.insert(remainder[..end].to_string()), "duplicate WASM host import");
  }
  imports
}

fn source_inventory() -> BTreeMap<EmbeddedOperationOwnerV1, BTreeSet<String>> {
  BTreeMap::from([
    (EmbeddedOperationOwnerV1::StorageEngine, public_methods("src/engine/storage_engine.rs", "StorageEngine")),
    (EmbeddedOperationOwnerV1::DirectoryOps, public_methods("src/engine/directory_ops.rs", "DirectoryOps")),
    (EmbeddedOperationOwnerV1::QueryEngine, public_methods("src/engine/query_engine.rs", "QueryEngine")),
    (EmbeddedOperationOwnerV1::QueryBuilder, public_methods("src/engine/query_engine.rs", "QueryBuilder")),
    (EmbeddedOperationOwnerV1::FieldQueryBuilder, public_methods("src/engine/query_engine.rs", "FieldQueryBuilder")),
    (EmbeddedOperationOwnerV1::PluginManager, public_methods("src/plugins/plugin_manager.rs", "PluginManager")),
    (EmbeddedOperationOwnerV1::WasmPluginRuntime, public_methods("src/plugins/wasm_runtime.rs", "WasmPluginRuntime")),
    (EmbeddedOperationOwnerV1::PluginHostImport, plugin_host_imports()),
  ])
}

fn registry_inventory() -> BTreeMap<EmbeddedOperationOwnerV1, BTreeSet<String>> {
  let mut inventory = BTreeMap::<EmbeddedOperationOwnerV1, BTreeSet<String>>::new();
  for group in embedded_root_operation_groups_v1() {
    assert!(!group.symbols.is_empty(), "embedded operation group is empty");
    for symbol in group.symbols {
      assert!(inventory.entry(group.owner).or_default().insert((*symbol).to_string()), "duplicate embedded operation contract");
    }
  }
  inventory
}

#[test]
fn registry_exactly_covers_every_selected_public_method_and_plugin_host_import() {
  let source = source_inventory();
  assert_eq!(source[&EmbeddedOperationOwnerV1::StorageEngine].len(), 108);
  assert_eq!(source[&EmbeddedOperationOwnerV1::DirectoryOps].len(), 44);
  assert_eq!(source[&EmbeddedOperationOwnerV1::QueryEngine].len(), 12);
  assert_eq!(source[&EmbeddedOperationOwnerV1::QueryBuilder].len(), 17);
  assert_eq!(source[&EmbeddedOperationOwnerV1::FieldQueryBuilder].len(), 28);
  assert_eq!(source[&EmbeddedOperationOwnerV1::PluginManager].len(), 12);
  assert_eq!(source[&EmbeddedOperationOwnerV1::WasmPluginRuntime].len(), 5);
  assert_eq!(source[&EmbeddedOperationOwnerV1::PluginHostImport].len(), 9);
  assert_eq!(registry_inventory(), source);
}

#[test]
fn root_operations_use_only_represented_shared_adapters_while_local_and_maintenance_stay_unrouted() {
  let router = EmbeddedRootOperationRouterV1::inactive_v4();
  let mut adapters = BTreeSet::new();
  let mut saw_local = false;
  let mut saw_maintenance = false;

  for group in embedded_root_operation_groups_v1() {
    for symbol in group.symbols {
      let plan = router.plan(group.owner, symbol).unwrap();
      assert_eq!(plan.owner, group.owner);
      assert_eq!(plan.symbol, *symbol);
      match (group.disposition, plan.execution) {
        (EmbeddedOperationDispositionV1::RootOperation { .. }, EmbeddedOperationExecutionV1::RootOperation { adapter, service_mode }) => {
          assert_eq!(service_mode, RootServiceModeV1::LegacyV3Compatibility);
          adapters.insert(adapter);
        }
        (EmbeddedOperationDispositionV1::LocalOnly, EmbeddedOperationExecutionV1::LocalOnly) => saw_local = true,
        (EmbeddedOperationDispositionV1::InternalMaintenance, EmbeddedOperationExecutionV1::InternalMaintenance) => {
          saw_maintenance = true;
        }
        (expected, actual) => panic!("embedded disposition changed during planning: expected {expected:?}, got {actual:?}"),
      }
    }
  }

  assert_eq!(
    adapters,
    BTreeSet::from([
      RootOperationAdapterV1::ResolveSingleRoot,
      RootOperationAdapterV1::TransportContent,
      RootOperationAdapterV1::ExecuteOperational,
      RootOperationAdapterV1::PublishCurrentMutation,
    ])
  );
  assert!(!adapters.contains(&RootOperationAdapterV1::ResolveMultipleRoots));
  assert!(!adapters.contains(&RootOperationAdapterV1::RetrieveHashFromSelectedRoot));
  assert!(saw_local);
  assert!(saw_maintenance);
}

#[test]
fn high_risk_embedded_operations_have_explicit_nonoverlapping_authority() {
  let router = EmbeddedRootOperationRouterV1::inactive_v4();
  let cases = [
    (EmbeddedOperationOwnerV1::StorageEngine, "get_entry", EmbeddedOperationExecutionV1::InternalMaintenance),
    (
      EmbeddedOperationOwnerV1::DirectoryOps,
      "read_file_streaming",
      EmbeddedOperationExecutionV1::RootOperation {
        adapter: RootOperationAdapterV1::ResolveSingleRoot,
        service_mode: RootServiceModeV1::LegacyV3Compatibility,
      },
    ),
    (
      EmbeddedOperationOwnerV1::DirectoryOps,
      "store_chunk",
      EmbeddedOperationExecutionV1::RootOperation {
        adapter: RootOperationAdapterV1::TransportContent,
        service_mode: RootServiceModeV1::LegacyV3Compatibility,
      },
    ),
    (EmbeddedOperationOwnerV1::DirectoryOps, "repair_stale_dir_key", EmbeddedOperationExecutionV1::InternalMaintenance),
    (EmbeddedOperationOwnerV1::QueryBuilder, "field", EmbeddedOperationExecutionV1::LocalOnly),
    (
      EmbeddedOperationOwnerV1::QueryBuilder,
      "execute_paginated",
      EmbeddedOperationExecutionV1::RootOperation {
        adapter: RootOperationAdapterV1::ResolveSingleRoot,
        service_mode: RootServiceModeV1::LegacyV3Compatibility,
      },
    ),
    (
      EmbeddedOperationOwnerV1::PluginHostImport,
      "aeordb_write_file",
      EmbeddedOperationExecutionV1::RootOperation {
        adapter: RootOperationAdapterV1::PublishCurrentMutation,
        service_mode: RootServiceModeV1::LegacyV3Compatibility,
      },
    ),
    (
      EmbeddedOperationOwnerV1::PluginHostImport,
      "aeordb_query",
      EmbeddedOperationExecutionV1::RootOperation {
        adapter: RootOperationAdapterV1::ResolveSingleRoot,
        service_mode: RootServiceModeV1::LegacyV3Compatibility,
      },
    ),
  ];

  for (owner, symbol, expected) in cases {
    assert_eq!(router.plan(owner, symbol).unwrap().execution, expected);
  }
}

#[test]
fn unknown_embedded_symbols_fail_closed_and_server_types_are_shared_aliases() {
  let router = EmbeddedRootOperationRouterV1::inactive_v4();
  assert_eq!(
    router.plan(EmbeddedOperationOwnerV1::DirectoryOps, "not_a_real_method"),
    Err(EmbeddedRootOperationErrorV1::UnknownOperation {
      owner: EmbeddedOperationOwnerV1::DirectoryOps,
      symbol: "not_a_real_method".to_string(),
    })
  );

  let class: RootOperationClassV1 = RootRouteClassV1::SingleRootNamespace;
  let proof: RootOperationProofV1 = ReadViewProofV1::ResolvedReadView;
  let adapter: RootOperationAdapterV1 = RootRequestAdapterV1::ResolveSingleRoot;
  assert_eq!(class, RootOperationClassV1::SingleRootNamespace);
  assert_eq!(proof, RootOperationProofV1::ResolvedReadView);
  assert_eq!(adapter, RootOperationAdapterV1::ResolveSingleRoot);

  let root_source = fs::read_to_string(manifest().join("src/server/root_api.rs")).unwrap();
  assert!(!root_source.contains("pub enum RootRouteClassV1"));
  assert!(!root_source.contains("pub enum ReadViewProofV1"));
  assert!(root_source.contains("adapt_root_operation_v1"));
  assert!(!root_source.contains("fn active_v4"));
}
