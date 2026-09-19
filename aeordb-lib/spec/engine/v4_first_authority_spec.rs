use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::sync::Arc;

use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::first_authority::{FirstAuthorityPublicationRequestV1, PreparedNamespaceTreeV0, V4FirstAuthorityPublisher};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, encode_semantic_state_object};
use aeordb::engine::v4::root_authority::decode_root_admission_commit;
use aeordb::engine::{DiskKVStore, HashAlgorithm};

fn initial_header(algorithm: HashAlgorithm, kv_block_length: u64) -> DatabaseHeaderV4 {
  let hash_width = algorithm.hash_length();
  DatabaseHeaderV4 {
    hash_algorithm: algorithm,
    slot_sequence: 1,
    created_at_ms: 1_700_000_000_000,
    updated_at_ms: 1_700_000_000_000,
    database_id: [0x31; 16],
    write_sequence_high_water: 1,
    required_reader_capabilities: [0; 32],
    kv_block_offset: DATABASE_HEADER_V4_DATA_OFFSET,
    kv_block_length,
    kv_block_version: DiskKVStore::CURRENT_KV_BLOCK_VERSION,
    kv_block_stage: 0,
    resize_in_progress: false,
    resize_target_stage: 0,
    nvt_offset: DATABASE_HEADER_V4_DATA_OFFSET + kv_block_length,
    nvt_length: 0,
    nvt_version: 1,
    backup_type: 0,
    hot_tail_offset: DATABASE_HEADER_V4_DATA_OFFSET + kv_block_length,
    buffer_kvs_offset: 0,
    buffer_nvt_offset: 0,
    entry_count: 0,
    head_hash: vec![0; hash_width],
    base_hash: vec![0; hash_width],
    target_hash: vec![0; hash_width],
    required_writer_capabilities: [0; 32],
    system_family_registry_version: 1,
    system_family_registry_fingerprint: vec![0x41; hash_width],
    writer_fence_epoch: 1,
    physical_instance_id: [0x51; 16],
  }
}

fn publisher(algorithm: HashAlgorithm) -> (tempfile::TempDir, Arc<DurabilityCoordinator>, V4FirstAuthorityPublisher) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("first-authority.aeordb");
  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(path).unwrap();
  let kv_block_length = initial_block_size();
  let header = initial_header(algorithm, kv_block_length);
  let slot = encode_database_header_slot(&header).unwrap();
  file.seek(SeekFrom::Start(0)).unwrap();
  file.write_all(&slot).unwrap();
  file.write_all(&slot).unwrap();
  let coordinator = Arc::new(DurabilityCoordinator::new());
  let kv = DiskKVStore::create_with_coordinator(
    file.try_clone().unwrap(),
    algorithm,
    header.kv_block_offset,
    header.hot_tail_offset,
    0,
    coordinator.clone(),
  )
  .unwrap();
  file.sync_all().unwrap();
  let publisher = V4FirstAuthorityPublisher::new(kv, coordinator.clone()).unwrap();
  (directory, coordinator, publisher)
}

fn request(algorithm: HashAlgorithm) -> FirstAuthorityPublicationRequestV1 {
  let semantic_state = encode_semantic_state_object(
    &SemanticStateWriteV1 {
      required_capabilities: [0; 32],
      availability: SemanticAvailabilityV1::ContentOnly {
        reason: aeordb::engine::v4::namespace::SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured,
      },
    },
    algorithm,
  )
  .unwrap();
  let namespace_tree_root = digest_parts(algorithm, &[b"dirc:"]);
  FirstAuthorityPublicationRequestV1 {
    database_id: [0x31; 16],
    transaction_id: [0x61; 16],
    created_at_ms: 1_700_000_000_100,
    namespace_tree: PreparedNamespaceTreeV0 { root_hash: namespace_tree_root, stored_value: Vec::new() },
    semantic_state,
    required_capabilities: [0; 32],
    typed_closure_digest: digest_parts(algorithm, &[b"typed test closure"]),
    authority_identity: b"HEAD".to_vec(),
  }
}

#[test]
fn first_authority_publishes_one_exact_root_and_witness_at_the_header_boundary() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, coordinator, publisher) = publisher(algorithm);
  let request = request(algorithm);
  let before = publisher.observe().unwrap();
  let expected_publication_sequence = coordinator.snapshot().unwrap().next_sequence;

  let receipt = publisher.publish(&request).unwrap();

  assert!(!receipt.idempotent);
  assert_eq!(receipt.publication_sequence, expected_publication_sequence);
  assert_eq!(receipt.observation.selected.header.head_hash, receipt.namespace_root.root_hash);
  assert_eq!(receipt.observation.selected.header.slot_sequence, before.selected.header.slot_sequence + 1);
  assert_eq!(receipt.observation.selected.header.write_sequence_high_water, before.selected.header.write_sequence_high_water + 8);
  assert_eq!(receipt.observation.selected.header.entry_count, before.selected.header.entry_count + 8);
  assert_eq!(receipt.observation.selected.header.required_reader_capabilities, before.selected.header.required_reader_capabilities);
  assert_eq!(receipt.observation.selected.header.required_writer_capabilities, before.selected.header.required_writer_capabilities);
  let admission = decode_root_admission_commit(&receipt.admission_control, algorithm).unwrap();
  assert_eq!(admission.namespace_root, receipt.namespace_root.root_hash);
  assert_eq!(admission.publication_sequence, receipt.publication_sequence);
  assert_eq!(admission.selected_header_slot_sequence, receipt.observation.selected.header.slot_sequence);
  assert!(publisher.locator(&receipt.namespace_root.root_hash).unwrap().is_some());
  assert!(publisher.admission_locator(&receipt.namespace_root.root_hash).unwrap().is_some());
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, receipt.publication_sequence);
}

#[test]
fn exact_retry_returns_the_selected_first_authority_without_another_publication() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, coordinator, publisher) = publisher(algorithm);
  let request = request(algorithm);
  let first = publisher.publish(&request).unwrap();
  let frontier = coordinator.snapshot().unwrap().hard_frontier;

  let retry = publisher.publish(&request).unwrap();

  assert!(retry.idempotent);
  assert_eq!(retry.namespace_root, first.namespace_root);
  assert_eq!(retry.admission_control, first.admission_control);
  assert_eq!(retry.publication_sequence, first.publication_sequence);
  assert_eq!(retry.observation, first.observation);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, frontier);
}

#[test]
fn first_authority_supports_the_frozen_sha512_identity_width() {
  let algorithm = HashAlgorithm::Sha512;
  let (_directory, coordinator, publisher) = publisher(algorithm);
  let request = request(algorithm);

  let receipt = publisher.publish(&request).unwrap();
  let admission = decode_root_admission_commit(&receipt.admission_control, algorithm).unwrap();

  assert_eq!(receipt.namespace_root.root_hash.len(), algorithm.hash_length());
  assert_eq!(receipt.observation.selected.header.head_hash, receipt.namespace_root.root_hash);
  assert_eq!(admission.namespace_root, receipt.namespace_root.root_hash);
  assert_eq!(admission.publication_sequence, receipt.publication_sequence);
  assert!(publisher.publish(&request).unwrap().idempotent);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, receipt.publication_sequence);
}

#[test]
fn first_authority_allows_only_reviewed_owners_and_exclusively_owns_atomic_root_publication() {
  fn collect_rust_files(directory: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(directory).unwrap() {
      let path = entry.unwrap().path();
      if path.is_dir() {
        collect_rust_files(&path, files);
      } else if path.extension().is_some_and(|extension| extension == "rs") {
        files.push(path);
      }
    }
  }

  let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let first_authority_path = source_root.join("engine/v4/first_authority.rs");
  let index_artifact_native_path = source_root.join("engine/v4/index_artifact_native.rs");
  let index_coverage_registry_path = source_root.join("engine/v4/index_coverage_registry.rs");
  let index_coverage_runtime_path = source_root.join("engine/v4/index_coverage_runtime.rs");
  let index_generation_authority_path = source_root.join("engine/v4/index_generation_authority.rs");
  let index_native_compaction_path = source_root.join("engine/v4/index_native_compaction.rs");
  let index_native_journal_source_path = source_root.join("engine/v4/index_native_journal_source.rs");
  let index_native_semantic_source_path = source_root.join("engine/v4/index_native_semantic_source.rs");
  let index_recovery_store_path = source_root.join("engine/v4/index_recovery_store.rs");
  let index_runtime_installation_path = source_root.join("engine/v4/index_runtime_installation.rs");
  let migration_base_clone_execution_path = source_root.join("engine/v4/migration_base_clone_execution.rs");
  let migration_capture_replay_path = source_root.join("engine/v4/migration_capture_replay.rs");
  let migration_cutover_rehearsal_path = source_root.join("engine/v4/migration_cutover_rehearsal.rs");
  let migration_destination_path = source_root.join("engine/v4/migration_destination.rs");
  let migration_final_authority_reconciliation_path = source_root.join("engine/v4/migration_final_authority_reconciliation.rs");
  let migration_final_reconciliation_path = source_root.join("engine/v4/migration_final_reconciliation.rs");
  let migration_offline_run_path = source_root.join("engine/v4/migration_offline_run.rs");
  let migration_owner_path = source_root.join("engine/v4/migration_owner.rs");
  let migration_root_map_owner_path = source_root.join("engine/v4/migration_root_map_owner.rs");
  let read_view_native_path = source_root.join("engine/v4/read_view_native.rs");
  let semantic_catalog_native_path = source_root.join("engine/v4/semantic_catalog_native.rs");
  let semantic_mutation_observation_path = source_root.join("engine/v4/semantic_mutation_observation.rs");
  let semantic_mutation_inventory_path = source_root.join("engine/v4/semantic_mutation_inventory.rs");
  let semantic_source_native_path = source_root.join("engine/v4/semantic_source_native.rs");
  let staging_protection_path = source_root.join("engine/v4/staging_protection.rs");
  let disk_kv_path = source_root.join("engine/disk_kv_store.rs");
  let header_publication_path = source_root.join("engine/v4/header_publication.rs");
  let mut files = Vec::new();
  collect_rust_files(&source_root, &mut files);

  let mut staging_consumers: Vec<_> = files
    .iter()
    .filter(|path| *path != &first_authority_path)
    .filter(|path| std::fs::read_to_string(path).unwrap().contains("NativeStagingProtectionV1"))
    .collect();
  staging_consumers.sort();
  assert_eq!(staging_consumers, [&semantic_catalog_native_path, &semantic_mutation_inventory_path, &staging_protection_path]);
  let inventory_source = std::fs::read_to_string(&semantic_mutation_inventory_path).unwrap();
  let inventory: String = inventory_source.split_whitespace().collect();
  for required in [
    "_protection:&'aNativeStagingProtectionV1<'a>",
    "snapshot:Arc<ReadSnapshot>",
    "_memory:MemoryReservation",
    "scan_scratch_bytes:u64",
    "self.memory.reserve(MemoryOwner::Task,self.scan_scratch_bytes,AdmissionClass::Maintenance)",
    ".visit_captured_entries(",
    ".capture_settled_snapshot(",
    "read_entity_bounded(",
    "load_canonical_system_file_at_path(",
    "select_available_mutable_control_slots(",
    "complete_semantic_mutation_observation(",
    "remaining_read_bytes:Cell<u64>",
  ] {
    assert!(inventory.contains(required), "captured task inventory lost shared ownership/validation: {required}");
  }
  for forbidden in [
    "StorageEngine",
    "DiskKVStore",
    "OpenOptions",
    "File::open",
    "File::create",
    "write_file",
    "sync_file",
    ".flush(",
    ".publish(",
    "publish_successor",
    "RootReadAdmission",
    "implCloneforNativeSemanticMutationInventoryV1",
    "HashSet",
    "iter_all(",
  ] {
    assert!(!inventory.contains(forbidden), "captured task inventory gained another authority/unbounded collection: {forbidden}");
  }
  let authority_source = std::fs::read_to_string(&first_authority_path).unwrap();
  let source_catalog_build = std::fs::read_to_string(source_root.join("engine/v4/semantic_source_catalog_build.rs")).unwrap();
  let assembly: String = source_catalog_build.split_whitespace().collect();
  for required in [
    "build_semantic_source_catalog_pair_v1",
    "encode_semantic_source_leaf_v1(",
    "encode_semantic_source_internal_v1(",
    "decode_system_control(",
    "try_reserve_exact(",
    "MemoryReservation",
    "row.path.capacity()>self.request.maximum_path_bytes",
    "identity.capacity()>width",
    "maximum_node_pairs",
    "maximum_output_bytes",
    "builder.check()?;letnext=rows.next();builder.check()?;",
    "(self.emit)(base,requested)?;self.check()?;",
    "root.paths!=self.path_count||root.nodes!=self.node_count",
  ] {
    assert!(assembly.contains(required), "source catalog assembly lost its bounded shared-encoder boundary: {required}");
  }
  for forbidden in [
    "HashMap",
    "HashSet",
    "BTreeMap",
    "BTreeSet",
    "StorageEngine",
    "V4FirstAuthorityPublisher",
    "OpenOptions",
    "File::",
    "write_file",
    "publish_",
    "serialize",
    "unsafe",
    "unwrap(",
    "expect(",
  ] {
    assert!(!assembly.contains(forbidden), "source catalog assembly gained another owner or unchecked collection: {forbidden}");
  }
  let alias_source = std::fs::read_to_string(source_root.join("engine/v4/semantic_source_aliases.rs")).unwrap();
  let aliases: String = alias_source.split_whitespace().collect();
  for required in [
    "parse_registry_source(request.source)?",
    "index_configuration_source::parse(bytes)?",
    "source.used_parser_alias()",
    "registry_source_workspace_bytes(source_length)?",
    "configuration_source_workspace_bytes(source_length,0)?",
    "maximum_alias_occurrences",
    "maximum_source_bytes",
    "MemoryReservation",
    "visitor(role,alias)?;check(&reservation,is_cancelled)?;",
  ] {
    assert!(aliases.contains(required), "source alias discovery lost shared schema/admission: {required}");
  }
  for forbidden in [
    "serde_json",
    "Deserialize",
    "HashMap",
    "HashSet",
    "BTreeMap",
    "BTreeSet",
    "StorageEngine",
    "V4FirstAuthorityPublisher",
    "OpenOptions",
    "File::",
    "publish_",
    "unsafe",
    "unwrap(",
    "expect(",
  ] {
    assert!(!aliases.contains(forbidden), "source alias discovery gained another parser/owner: {forbidden}");
  }
  let plugin_pair_source = std::fs::read_to_string(source_root.join("engine/v4/semantic_plugin_source_native.rs")).unwrap();
  let plugin_pair: String = plugin_pair_source.split_whitespace().collect();
  for required in [
    "letlookup=self.source_lookup(plugin_source_read_bounds(bounds));",
    "decode_plugin_alias_v1(alias_source.body(),&alias_path)",
    "inspect_plugin_artifact_identity_v1(",
    "encode_dependency_record(&DependencyRecordV1",
    "plugin_alias_path_v1(alias)",
    "plugin_artifact_path_v1(alias_record.artifact_fingerprint)",
    "maximum_read_bytes:bounds.maximum_read_bytes",
    "PAIR_WORKSPACE_BYTES+IDENTITY_WORKSPACE_BYTES",
    "before_complete();check_cancelled(&self.cancellation)?;",
    "MemoryReservation",
  ] {
    assert!(plugin_pair.contains(required), "native plugin pair lost its single captured source owner: {required}");
  }
  assert_eq!(plugin_pair.matches("self.source_lookup(").count(), 1);
  assert_eq!(plugin_pair.matches("self.read_source_from_lookup(").count(), 2);
  for forbidden in [
    "StorageEngine",
    "V4FirstAuthorityPublisher",
    "OpenOptions",
    "File::",
    "publish_",
    "write_file",
    "lock_kv",
    "capture_semantic_mutation_inventory(",
    "read_protected_source(",
    "HashMap",
    "BTreeMap",
    "unsafe",
    "unwrap(",
    "expect(",
  ] {
    assert!(!plugin_pair.contains(forbidden), "native plugin pair gained another source/authority owner: {forbidden}");
  }
  let prepared_source = std::fs::read_to_string(source_root.join("engine/v4/semantic_alias_snapshot_native.rs")).unwrap();
  let prepared: String = prepared_source.split_whitespace().collect();
  for required in [
    "visit_semantic_source_aliases_v1(",
    "aliases.try_reserve_exact(count)",
    "aliases.sort_unstable_by(",
    "aliases.dedup_by(",
    "retained.requested_roles|=removed.requested_roles;",
    "letlookup=self.source_lookup(plugin_source_read_bounds(request.plugins));foraliasin&mutaliases{",
    "self.read_protected_plugin_sources_from_lookup(&alias.alias,request.plugins,&lookup,||{})?",
    "maximum_snapshot_bytes",
    "binary_search_by(",
    "bytes.map(decode_dependency_record_bytes)",
    "before_complete();check_snapshot(self,&reservation)?;",
    "MemoryReservation",
    "implParserAliasSnapshotV1forNativeSemanticAliasSnapshotV1",
    "implIndexConfigurationAliasSnapshotV1forNativeSemanticAliasSnapshotV1",
  ] {
    assert!(prepared.contains(required), "prepared native aliases lost their bounded captured owner: {required}");
  }
  assert_eq!(prepared.matches("visit_semantic_source_aliases_v1(").count(), 2);
  assert_eq!(prepared.matches("self.source_lookup(").count(), 1);
  assert_eq!(prepared.matches("self.read_protected_plugin_sources_from_lookup(").count(), 1);
  for forbidden in [
    "StorageEngine",
    "V4FirstAuthorityPublisher",
    "OpenOptions",
    "File::",
    "publish_",
    "write_file",
    "lock_kv",
    "capture_semantic_mutation_inventory(",
    "read_protected_source(",
    "read_protected_plugin_sources(",
    "HashMap",
    "BTreeMap",
    "serde_json",
    "unsafe",
    "unwrap(",
    "expect(",
  ] {
    assert!(!prepared.contains(forbidden), "prepared native aliases gained another schema/source/authority owner: {forbidden}");
  }
  let source_staging_path = source_root.join("engine/v4/semantic_source_staging.rs");
  let source_staging = std::fs::read_to_string(&source_staging_path).unwrap();
  let staging: String = source_staging.split_whitespace().collect();
  for required in [
    "implNativeProtectedSemanticSourceV1<'_>",
    "letcapture=self._capture;",
    "capture._protection.capture_semantic_mutation_inventory(",
    "fresh_header.slot_sequence<old_header.slot_sequence",
    "fresh_header.write_sequence_high_water<old_header.write_sequence_high_water",
    "self.validate_live_chunks(&fresh,bounds)?;",
    "publisher.root_state.lock()",
    "current.selected.header!=*fresh_header",
    "load_exact_immutable_entity(",
    "ImmutableEntityValidationV1::CapturedProtectedSource",
    ".publish_immutable_entity_batch_with_validation_locked(",
    "key:&self.revision",
    "stored_value:&self.encoded_record",
    "entity_version:self.entity_version",
    "flags:self.flags",
  ] {
    assert!(staging.contains(required), "source staging lost its guarded shared-owner boundary: {required}");
  }
  for forbidden in [
    "OpenOptions",
    "File::open",
    "File::create",
    "write_file",
    "sync_file",
    ".flush(",
    "FileRecord::serialize",
    "FileRecord::deserialize",
    "encode_whole_entity",
    "RootReadAdmission",
    "publish_mutable",
    "publish_successor",
    "HashMap",
    "HashSet",
    "unsafe",
  ] {
    assert!(!staging.contains(forbidden), "source staging gained another writer/parser/authority path: {forbidden}");
  }
  let captured_source_publishers: Vec<_> = files
    .iter()
    .filter(|path| std::fs::read_to_string(path).unwrap().contains("ImmutableEntityValidationV1::CapturedProtectedSource"))
    .collect();
  assert_eq!(captured_source_publishers, [&source_staging_path]);
  let catalog_source = std::fs::read_to_string(source_root.join("engine/v4/semantic_source_catalog_native.rs")).unwrap();
  let catalog: String = catalog_source.split_whitespace().collect();
  for required in [
    "capture:&'aNativeSemanticMutationInventoryV1<'a>",
    "snapshot:&capture.snapshot",
    "remaining_work:Cell<u64>",
    "load_immutable_system_control_file(",
    "decode_semantic_source_capture_binding_v1(",
    "seek_namespace_child_v1(",
    ".read_source_from_lookup(",
  ] {
    assert!(catalog.contains(required), "native source catalog lost captured/shared ownership: {required}");
  }
  let cursor_source = std::fs::read_to_string(source_root.join("engine/v4/semantic_source_catalog_cursor.rs")).unwrap();
  let cursor: String = cursor_source.split_whitespace().collect();
  for required in
    ["decode_semantic_source_node_v1(", "child_bounds(", "_memory:MemoryReservation", "stack:Vec<SourceCatalogFrameV1>", "leaf:Option<"]
  {
    assert!(cursor.contains(required), "source catalog lost bounded node/cursor ownership: {required}");
  }
  for source in [&catalog, &cursor] {
    for forbidden in [
      "StorageEngine",
      "DiskKVStore",
      "OpenOptions",
      "File::open",
      "write_file",
      "sync_file",
      "publish_",
      "RootReadAdmission",
      "capture_settled_snapshot",
      "lock_kv(",
      "HashMap",
      "HashSet",
      "FileRecord::deserialize",
      "FileRecord::serialize",
      "next_namespace_child_by_path_v1",
      "node.serialize(",
    ] {
      assert!(!source.contains(forbidden), "read-only source catalog gained another authority/decoder/unbounded path: {forbidden}");
    }
  }
  let source_reader = std::fs::read_to_string(&semantic_source_native_path).unwrap();
  let source_reader: String = source_reader.split_whitespace().collect();
  let namespace_source = std::fs::read_to_string(source_root.join("engine/v4/semantic_namespace_source_native.rs")).unwrap();
  let namespace_source: String = namespace_source.split_whitespace().collect();
  for required in [
    "snapshot:&capture.snapshot",
    "read_entity_bounded(",
    "read_decoded_source_from_lookup(",
    "SemanticSourceKindV1::Namespace",
    "next_namespace_child_by_path_v1(",
    "seek_namespace_child_v1(",
    "namespace_seek_workspace_bytes_v1(",
    "decode_validated_selected_directory_node(",
    "validate_selected_directory_entity(",
    "validate_selected_file_record_metadata(",
    "join_selected_path(",
    "remaining_work:Cell<u64>",
    "remaining_read_bytes:Cell::new(bounds.sources.maximum_read_bytes)",
    "before_complete();operation.check()?;",
    "value:DecodedSemanticSourceV1",
  ] {
    assert!(namespace_source.contains(required), "namespace sources lost shared captured readers: {required}");
  }
  for forbidden in [
    "NativeProtectedSemanticSourceV1",
    "stage_retained_copy",
    "StorageEngine",
    "DiskKVStore",
    "OpenOptions",
    "File::",
    "publish_",
    "write_file",
    "lock_kv(",
    "capture_settled_snapshot(",
    "capture_semantic_mutation_inventory(",
    "FileRecord::deserialize",
    "FileRecord::serialize",
    "node.serialize(",
    "HashMap",
    "HashSet",
    "BTreeMap",
    "unsafe",
    "unwrap(",
    "expect(",
  ] {
    assert!(!namespace_source.contains(forbidden), "namespace sources gained an independent owner or staging API: {forbidden}");
  }
  assert_eq!(source_reader.matches("FileRecord::deserialize(").count(), 1);
  assert!(source_reader.contains("SemanticSourceKindV1::Protected=>validate_source_path(path,algorithm)?"));
  assert_eq!(source_reader.matches("kind==SemanticSourceKindV1::Namespace&&entity.flags!=0").count(), 2);
  for required in [
    "_capture:&'aNativeSemanticMutationInventoryV1<'a>",
    "_memory:MemoryReservation",
    "snapshot:&self.snapshot",
    "FileRecord::deserialize(",
    "read_entity_bounded(",
    "IncrementalDigestV1::new(",
    "decoder.decompress(output,entity.stored_value)",
  ] {
    assert!(source_reader.contains(required), "protected source reader lost shared capture/validation: {required}");
  }
  for forbidden in [
    "StorageEngine",
    "DiskKVStore",
    "OpenOptions",
    "File::open",
    "File::create",
    "write_file",
    "sync_file",
    ".flush(",
    "publish_",
    "RootReadAdmission",
    "capture_settled_snapshot",
    "lock_kv(",
    "implCloneforNativeProtectedSemanticSourceV1",
    "decompress_bounded(",
    "FileRecord::serialize",
    "unsafe",
  ] {
    assert!(!source_reader.contains(forbidden), "protected source reader gained another authority or decoder: {forbidden}");
  }
  assert_eq!(
    authority_source.matches("kv.admit_read(&locator)?").count(),
    1,
    "captured read limits must live in the shared physical reader"
  );

  let mut publisher_callers: Vec<_> = files
    .iter()
    .filter(|path| *path != &first_authority_path)
    .filter(|path| std::fs::read_to_string(path).unwrap().contains("V4FirstAuthorityPublisher"))
    .collect();
  publisher_callers.sort();
  assert_eq!(
    publisher_callers,
    vec![
      &index_artifact_native_path,
      &index_coverage_registry_path,
      &index_coverage_runtime_path,
      &index_generation_authority_path,
      &index_native_compaction_path,
      &index_native_journal_source_path,
      &index_native_semantic_source_path,
      &index_recovery_store_path,
      &index_runtime_installation_path,
      &migration_base_clone_execution_path,
      &migration_capture_replay_path,
      &migration_cutover_rehearsal_path,
      &migration_destination_path,
      &migration_final_authority_reconciliation_path,
      &migration_final_reconciliation_path,
      &migration_offline_run_path,
      &migration_owner_path,
      &migration_root_map_owner_path,
      &read_view_native_path,
      &semantic_catalog_native_path,
      &semantic_mutation_observation_path,
      &staging_protection_path,
    ],
    "first-authority publisher escaped the reviewed owners: {publisher_callers:?}"
  );
  for owner_path in [
    &index_artifact_native_path,
    &index_coverage_registry_path,
    &index_coverage_runtime_path,
    &index_generation_authority_path,
    &index_native_compaction_path,
    &index_native_journal_source_path,
    &index_native_semantic_source_path,
    &index_recovery_store_path,
    &index_runtime_installation_path,
    &migration_base_clone_execution_path,
    &migration_capture_replay_path,
    &migration_cutover_rehearsal_path,
    &migration_destination_path,
    &migration_final_authority_reconciliation_path,
    &migration_final_reconciliation_path,
    &migration_offline_run_path,
    &migration_owner_path,
    &migration_root_map_owner_path,
    &read_view_native_path,
    &semantic_catalog_native_path,
    &semantic_mutation_observation_path,
    &staging_protection_path,
  ] {
    let owner_source = std::fs::read_to_string(owner_path).unwrap();
    for forbidden in ["DirectoryOps", "crate::server", "tokio::spawn"] {
      assert!(!owner_source.contains(forbidden), "disconnected owner {owner_path:?} gained live activation token {forbidden}");
    }
    if owner_path == &migration_final_reconciliation_path {
      assert!(owner_source.contains("MigrationSourceWriteFreezeV1"));
      assert!(owner_source.contains("StorageEngine"));
    } else if owner_path == &migration_offline_run_path {
      assert!(owner_source.contains("StorageEngine::open_for_offline_migration_inspection"));
      assert!(!owner_source.contains("StorageEngine::open("));
    } else if owner_path == &index_runtime_installation_path {
      assert!(owner_source.contains("StorageEngine"));
      assert!(owner_source.contains("begin_index_runtime_installation_v1"));
      assert!(owner_source.contains("load_selected_semantic_authority"));
      assert!(!owner_source.contains("request.publisher.publish"));
    } else if owner_path == &index_artifact_native_path {
      assert_eq!(owner_source.matches(".load_index_artifact_at_captured_header(").count(), 1);
      for forbidden in [".publish_index_artifacts(", ".publish_successor_authority(", ".publish("] {
        assert!(!owner_source.contains(forbidden), "captured artifact reader gained first-authority writer {forbidden}");
      }
    } else if owner_path == &semantic_catalog_native_path {
      // The staging adapter borrows the sole physical writer but can only
      // observe, read back, and publish immutable, unselected semantic objects.
      // Review any new publisher call instead of admitting authority selection
      // merely because this file is already in the owner inventory.
      let compact: String = owner_source.split_whitespace().collect();
      let calls: Vec<_> = compact.split(".publisher.").skip(1).map(|call| call.split('(').next().unwrap()).collect();
      assert_eq!(calls, ["observe", "load_semantic_object_at_captured_header", "publish_immutable_semantic_objects"]);
      assert!(compact.contains("_protection:&'aNativeStagingProtectionV1<'a>"));
      assert!(compact.contains("letpublisher=protection.publisher();"));
      for forbidden in ["StorageEngine", "DiskKVStore", "FirstAuthorityPublicationRequestV1", "publish_successor_authority", ".publish("] {
        assert!(!compact.contains(forbidden), "semantic staging gained authority or physical ownership: {forbidden}");
      }
    } else if owner_path == &semantic_mutation_observation_path {
      // This private child implements a read-only operation on the existing
      // owner; inventory it without allowing another writer or authority.
      let compact: String = owner_source.split_whitespace().collect();
      let calls: Vec<_> = compact.split("self.").skip(1).map(|call| call.split('(').next().unwrap()).collect();
      let owner_calls: Vec<_> = calls.into_iter().filter(|call| !call.contains('.') && !call.contains(':')).collect();
      for required in ["selected_semantic_authority_guard", "observe", "lock_kv"] {
        assert!(owner_calls.contains(&required), "task observation lost its shared native boundary: {required}");
      }
      for forbidden in ["StorageEngine", "DiskKVStore", "publish_", "write_file", "sync_file", "implCloneforSemanticMutationObservationV1"]
      {
        assert!(!compact.contains(forbidden), "read-only task observation gained authority or detached ownership: {forbidden}");
      }
      assert!(compact.contains("_memory:MemoryReservation"));
      assert!(compact.contains("decode_semantic_mutation_selection("));
    } else if owner_path == &staging_protection_path {
      let compact: String = owner_source.split_whitespace().collect();
      assert!(compact.contains("_memory:MemoryReservation"));
      assert!(compact.contains("drop(authority);Ok(NativeStagingProtectionV1"));
      assert!(compact.contains("implDropforNativeStagingProtectionV1"));
      for forbidden in ["implCloneforNativeStagingProtectionV1", "RootReadAdmission", ".file", ".kv", "publish_", "std::fs"] {
        assert!(!compact.contains(forbidden), "staging protection gained detached or physical authority: {forbidden}");
      }
      let authority = std::fs::read_to_string(&first_authority_path).unwrap();
      assert_eq!(authority.matches(".ensure_no_staging_protection()?").count(), 4);
      for method in [
        "fn publish_physical_quarantine_excluded(",
        "fn publish_root_retirement_excluded(",
        "fn publish_root_reclaim_excluded(",
        "pub fn execute_sweep_locator_removals(",
      ] {
        let start = authority.find(method).unwrap();
        let body = &authority[start..];
        let lock = body.find("self.root_state.lock()").unwrap();
        let gate = body.find(".ensure_no_staging_protection()?").unwrap();
        let observation = body.find("let observation = self.observe()?").unwrap();
        assert!(lock < gate && gate < observation, "{method} must recheck staging while holding the authority boundary");
      }
    } else {
      assert!(!owner_source.contains("StorageEngine"), "disconnected owner {owner_path:?} gained direct v3 engine ownership");
    }
  }

  for method in ["begin_atomic_visibility_batch", "publish_atomic_visibility_after_authority", "admit_inactive_slot_with_dependency_bytes"]
  {
    let owners: Vec<_> = files
      .iter()
      .filter(|path| *path != &disk_kv_path && *path != &header_publication_path)
      .filter(|path| std::fs::read_to_string(path).unwrap().contains(method))
      .collect();
    assert_eq!(owners, vec![&first_authority_path], "{method} escaped first-authority ownership: {owners:?}");
  }
}
