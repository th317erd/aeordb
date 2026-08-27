use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use aeordb_plugin_sdk::root::{
  PluginItemsResponseV1, PluginNamespaceReadInvocationV1, PluginResultsResponseV1, PluginRootMetadataV1, PluginRootSelectorV1,
  PluginRootStateV1,
};

fn manifest() -> &'static Path {
  Path::new(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn invocation_selector_is_flattened_bounded_and_mutually_exclusive() {
  let current = PluginNamespaceReadInvocationV1::current();
  assert_eq!(serde_json::to_value(&current).unwrap(), serde_json::json!({}));
  assert_eq!(current.validate().unwrap().selector(), &PluginRootSelectorV1::CurrentHead);

  for (selector, expected) in [
    (PluginNamespaceReadInvocationV1::root_hash("AB".repeat(32)).unwrap(), serde_json::json!({"root_hash": "ab".repeat(32)})),
    (PluginNamespaceReadInvocationV1::snapshot("release-1").unwrap(), serde_json::json!({"snapshot": "release-1"})),
    (PluginNamespaceReadInvocationV1::version("CD".repeat(64)).unwrap(), serde_json::json!({"version": "cd".repeat(64)})),
  ] {
    assert_eq!(serde_json::to_value(&selector).unwrap(), expected);
    assert_eq!(serde_json::from_value::<PluginNamespaceReadInvocationV1>(expected).unwrap().validate().unwrap(), selector);
  }

  for malformed in [
    serde_json::json!({"root_hash": "aa", "snapshot": "release-1"}),
    serde_json::json!({"snapshot": "release-1", "version": "ab".repeat(32)}),
    serde_json::json!({"root_hash": "ab".repeat(32), "version": "cd".repeat(32)}),
    serde_json::json!({"root_hash": "ab".repeat(32), "snapshot": "release-1", "version": "cd".repeat(32)}),
    serde_json::json!({"root_hash": ""}),
    serde_json::json!({"root_hash": "00".repeat(32)}),
    serde_json::json!({"root_hash": "gg".repeat(32)}),
    serde_json::json!({"snapshot": ""}),
    serde_json::json!({"snapshot": "release\n1"}),
    serde_json::json!({"snapshot": "s".repeat(4097)}),
    serde_json::json!({"version": "not-hex"}),
    serde_json::json!({"unknown": true}),
  ] {
    let result = serde_json::from_value::<PluginNamespaceReadInvocationV1>(malformed)
      .and_then(|value| value.validate().map_err(serde::de::Error::custom));
    assert!(result.is_err());
  }

  let duplicate_root_hash = format!(r#"{{"root_hash":"{}","root_hash":"{}"}}"#, "ab".repeat(32), "cd".repeat(32));
  assert!(serde_json::from_str::<PluginNamespaceReadInvocationV1>(&duplicate_root_hash).is_err());
}

#[test]
fn exact_root_metadata_rejects_noncanonical_hashes_and_inconsistent_expiry() {
  for root in [
    PluginRootMetadataV1 { hash: "ef".repeat(32), state: PluginRootStateV1::Live, expires_at: None },
    PluginRootMetadataV1 { hash: "ef".repeat(64), state: PluginRootStateV1::Retained, expires_at: None },
    PluginRootMetadataV1 { hash: "ef".repeat(32), state: PluginRootStateV1::PendingDelete, expires_at: Some(1_700_086_400_000) },
  ] {
    root.validate().unwrap();
  }

  for malformed in [
    PluginRootMetadataV1 { hash: "EF".repeat(32), state: PluginRootStateV1::Live, expires_at: None },
    PluginRootMetadataV1 { hash: "00".repeat(32), state: PluginRootStateV1::Live, expires_at: None },
    PluginRootMetadataV1 { hash: "ef".repeat(31), state: PluginRootStateV1::Live, expires_at: None },
    PluginRootMetadataV1 { hash: "ef".repeat(32), state: PluginRootStateV1::PendingDelete, expires_at: None },
    PluginRootMetadataV1 { hash: "ef".repeat(32), state: PluginRootStateV1::Live, expires_at: Some(1) },
    PluginRootMetadataV1 { hash: "ef".repeat(32), state: PluginRootStateV1::Retained, expires_at: Some(1) },
  ] {
    assert!(malformed.validate().is_err());
  }

  assert!(serde_json::from_value::<PluginRootMetadataV1>(serde_json::json!({
    "hash": "ef".repeat(32),
    "state": "live",
    "expires_at": null,
    "unknown": true,
  }))
  .is_err());
}

#[test]
fn exact_root_and_collection_envelopes_preserve_public_shapes() {
  let root = PluginRootMetadataV1 { hash: "ef".repeat(32), state: PluginRootStateV1::Retained, expires_at: None };
  root.validate().unwrap();

  let items = PluginItemsResponseV1::new(root.clone(), vec!["alpha"], true, None);
  assert_eq!(
    serde_json::to_value(&items).unwrap(),
    serde_json::json!({
      "root": {"hash": "ef".repeat(32), "state": "retained", "expires_at": null},
      "items": ["alpha"],
      "has_more": true,
    })
  );

  let results = PluginResultsResponseV1::new(root, vec!["beta"], false, Some(1));
  let encoded = serde_json::to_value(&results).unwrap();
  assert_eq!(encoded["results"], serde_json::json!(["beta"]));
  assert_eq!(encoded["total"], 1);
  assert_eq!(encoded["has_more"], false);
  for forbidden in ["stable_key", "replacement", "physical", "offset", "wal", "engine", "handler"] {
    assert!(!encoded.to_string().contains(forbidden));
  }
}

#[test]
fn plugin_import_registries_remain_exactly_eight_sdk_and_nine_runtime_symbols() {
  let sdk = fs::read_to_string(manifest().join("src/context.rs")).unwrap();
  let extern_block = sdk.split("extern \"C\" {").nth(1).unwrap().split('}').next().unwrap();
  let sdk_imports = extern_block
    .lines()
    .filter_map(|line| line.trim().strip_prefix("fn "))
    .filter_map(|line| line.split('(').next())
    .map(str::to_string)
    .collect::<Vec<_>>();
  assert_eq!(sdk_imports.len(), 8);
  let sdk_imports = sdk_imports.into_iter().collect::<BTreeSet<_>>();
  assert_eq!(
    sdk_imports,
    BTreeSet::from([
      "aeordb_aggregate".to_string(),
      "aeordb_delete_file".to_string(),
      "aeordb_extract_file".to_string(),
      "aeordb_file_metadata".to_string(),
      "aeordb_list_directory".to_string(),
      "aeordb_query".to_string(),
      "aeordb_read_file".to_string(),
      "aeordb_write_file".to_string(),
    ])
  );

  let runtime = fs::read_to_string(manifest().join("../aeordb-lib/src/plugins/wasm_runtime.rs")).unwrap();
  let marker = ".func_wrap(\"aeordb\", \"";
  let runtime_imports = runtime
    .match_indices(marker)
    .map(|(start, _)| &runtime[start + marker.len()..])
    .map(|line| line.split('"').next().unwrap().to_string())
    .collect::<Vec<_>>();
  assert_eq!(runtime_imports.len(), 9);
  let runtime_imports = runtime_imports.into_iter().collect::<BTreeSet<_>>();
  assert_eq!(runtime_imports.len(), 9);
  assert!(runtime_imports.contains("log_message"));
  assert!(sdk_imports.is_subset(&runtime_imports));

  assert_eq!(
    sdk.matches("let args = self.rooted_arguments(").count(),
    6,
    "all six direct context imports must carry the invocation selector"
  );
  assert_eq!(sdk.matches("with_root_invocation(path, self.root_invocation.clone())").count(), 2);
  let builders = fs::read_to_string(manifest().join("src/query_builder.rs")).unwrap();
  assert_eq!(builders.matches("append_root_invocation(&mut query, &self.root_invocation);").count(), 2);
}
