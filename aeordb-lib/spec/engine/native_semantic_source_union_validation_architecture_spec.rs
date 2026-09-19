//! Supplemental structural guards; native behavior tests remain the authority.
#[test]
fn retained_source_validation_reuses_captured_readers_and_keeps_metadata_distinct_from_tree_authority() {
  let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine/v4");
  let source = std::fs::read_to_string(root.join("semantic_source_union_validation.rs")).unwrap();
  let source: String = source.split_whitespace().collect();
  for required in [
    "load_companion_and_checkpoint(",
    "decode_semantic_source_capture_v1(",
    "decode_semantic_mutation_checkpoint(",
    "load_namespace_authority_parts_from_lookup(",
    "decode_namespace_semantic_binding(",
    "validate_captured_root_admission_sequence(",
    "catalog.visit_pairs(",
    "visit_semantic_source_aliases_v1(",
    "read_plugin_sources_with_selected_reader(",
    "self.manifest.base_source_catalog,self.manifest.requested_source_catalog",
    "SemanticSourceLookupDispositionV1::Unlisted",
    "SourceCatalogCursorV1::new(",
    "NamespaceSourcePairCursorV1::new(",
    "fingerprint_semantic_mutation_sources_v1(",
    "requested_configurations!=checkpoint.expected_configuration_count",
    "fingerprint.digest()!=manifest.source_identity_fingerprint",
    "before_complete();catalog.check()?;namespace.check()?;",
    "checked_add(1)",
    "maximum_alias_occurrences",
  ] {
    assert!(source.contains(required), "retained validation lost a shared owner/check: {required}");
  }
  assert_eq!(source.matches("CatalogReadOperationV1::new(").count(), 1);
  assert_eq!(source.matches("NamespaceSourceOperationV1::with_read_admission(").count(), 1);
  for forbidden in [
    "StorageEngine",
    "DiskKVStore",
    "OpenOptions",
    "File::",
    "lock_kv(",
    "root_state.lock(",
    "capture_settled_snapshot(",
    "capture_semantic_mutation_inventory(",
    "source_lookup(",
    "read_protected_source(",
    "load_namespace_authority_from_lookup(",
    "head_hash",
    "publish_",
    "stage_retained_copy(",
    "FileRecord::deserialize",
    "FileRecord::serialize",
    "decode_whole_entity(",
    "node.serialize(",
    "serde_json",
    "HashMap",
    "HashSet",
    "BTreeMap",
    "BTreeSet",
    "unsafe",
    "unwrap(",
    "expect(",
  ] {
    assert!(!source.contains(forbidden), "retained validation gained an independent decoder/authority/fallback: {forbidden}");
  }
  let metadata = std::fs::read_to_string(root.join("root_authority.rs")).unwrap();
  let metadata: String = metadata.split_whitespace().collect();
  assert!(metadata.contains("structNamespaceSemanticBindingV1{"));
  assert!(metadata.contains("structNamespaceSemanticBindingInputV1<'a>{"));
  assert_eq!(metadata.matches("decode_namespace_semantic_binding_with_tree(").count(), 2);
  assert_eq!(metadata.matches("decode_namespace_tree_root_v0(namespace_tree_bytes,").count(), 1);
  assert_eq!(metadata.matches("decode_root_admission_commit(admission_bytes,").count(), 1);
  assert_eq!(metadata.matches("decode_semantic_object(semantic_bytes,").count(), 1);
  let namespace = std::fs::read_to_string(root.join("semantic_namespace_source_native.rs")).unwrap();
  let namespace: String = namespace.split_whitespace().collect();
  assert!(namespace.contains("Self::with_read_admission(capture,request,())"));
  assert!(namespace.contains("self.charge_work(1)?;self.captured.admit_read(locator)?;self.additional_read_admission.admit(locator)"));
}
