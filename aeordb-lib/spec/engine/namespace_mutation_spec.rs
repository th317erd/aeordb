use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use aeordb::engine::entry_type::EntryType;
use aeordb::engine::memory_coordinator::{AdmissionClass, CriticalMemoryPurpose, MemoryOwner};
use aeordb::engine::namespace_mutation::{
  NamespaceMutationAcknowledgement, NamespaceMutationBatch, NamespaceMutationCoordinator, NamespaceMutationFanout, NamespaceMutationKind,
  NamespaceMutationSourceIdentity, publish_namespace_root, publish_namespace_root_from, publish_namespace_root_with_fanout,
};
use aeordb::auth::api_key::ApiKeyRecord;
use aeordb::engine::{DirectoryOps, EngineError, RequestContext, directory_content_hash, file_path_hash};
use aeordb::engine::system_store;
use aeordb::engine::storage_engine::TransactionGuard;
use aeordb::engine::StorageEngine;
use aeordb::server::create_temp_engine_for_tests;

#[derive(Default)]
struct RecordingFanout {
  acknowledgements: Mutex<Vec<NamespaceMutationAcknowledgement>>,
}

impl RecordingFanout {
  fn acknowledgements(&self) -> Vec<NamespaceMutationAcknowledgement> {
    self.acknowledgements.lock().unwrap().clone()
  }
}

impl NamespaceMutationFanout for RecordingFanout {
  fn publish(&self, acknowledgement: &NamespaceMutationAcknowledgement) {
    self.acknowledgements.lock().unwrap().push(acknowledgement.clone());
  }
}

struct AuthorityCheckingFanout {
  engine: Arc<StorageEngine>,
  stable_key: Vec<u8>,
  calls: Mutex<Vec<NamespaceMutationAcknowledgement>>,
}

impl NamespaceMutationFanout for AuthorityCheckingFanout {
  fn publish(&self, acknowledgement: &NamespaceMutationAcknowledgement) {
    let durability = self.engine.durability_snapshot().unwrap();
    assert!(durability.hard_frontier >= acknowledgement.publication_sequence);
    let published = self.engine.get_kv_entry(&self.stable_key).unwrap().expect("stable locator must exist before fanout");
    assert_eq!(acknowledgement.locator_replacements[0].new_incarnation.as_ref().unwrap().offset, published.offset);
    self.calls.lock().unwrap().push(acknowledgement.clone());
  }
}

struct PanickingFanout;

impl NamespaceMutationFanout for PanickingFanout {
  fn publish(&self, _acknowledgement: &NamespaceMutationAcknowledgement) {
    panic!("injected soft-fanout panic");
  }
}

#[test]
fn whole_root_publication_rejects_a_changed_expected_head() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let expected_root = engine.head_hash().unwrap();
  DirectoryOps::new(&engine).create_directory(&RequestContext::system(), "/concurrent").unwrap();
  let concurrent_root = engine.head_hash().unwrap();

  let error = publish_namespace_root_from(&engine, &expected_root, &expected_root, NamespaceMutationKind::Import).unwrap_err();

  assert!(matches!(error, EngineError::AlreadyExists(message) if message.contains("HEAD changed")));
  assert_eq!(engine.head_hash().unwrap(), concurrent_root);
}

#[test]
fn whole_root_publication_reconciles_live_namespace_counters_after_acknowledgement() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let operations = DirectoryOps::new(&engine);
  operations.store_file_buffered(&context, "/selected.txt", b"selected", Some("text/plain")).unwrap();
  operations.store_symlink(&context, "/selected-link", "/selected.txt").unwrap();
  let selected_root = engine.head_hash().unwrap();
  let expected = engine.counters().snapshot();
  operations.store_file_buffered(&context, "/later.txt", b"later", Some("text/plain")).unwrap();
  operations.create_directory(&context, "/later-directory").unwrap();

  publish_namespace_root(&engine, &selected_root, NamespaceMutationKind::Promote).unwrap();
  let actual = engine.counters().snapshot();

  assert_eq!(actual.files, expected.files);
  assert_eq!(actual.directories, expected.directories);
  assert_eq!(actual.symlinks, expected.symlinks);
  assert_eq!(actual.logical_data_size, expected.logical_data_size);
}

#[test]
fn whole_root_publication_invalidates_every_engine_owned_authority_cache() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let selected_root = engine.head_hash().unwrap();
  DirectoryOps::new(&engine).create_directory(&RequestContext::system(), "/later").unwrap();

  engine.permissions_cache.get(&"/".to_string(), &engine).unwrap();
  engine.index_config_cache.get(&"/".to_string(), &engine).unwrap();
  engine.grants_index_cache.get(&(), &engine).unwrap();
  engine.group_cache.get(&uuid::Uuid::new_v4(), &engine).unwrap();
  engine.api_key_cache.get(&uuid::Uuid::new_v4().to_string(), &engine).unwrap();
  assert_eq!(engine.permissions_cache.len(), 1);
  assert_eq!(engine.index_config_cache.len(), 1);
  assert_eq!(engine.grants_index_cache.len(), 1);
  assert_eq!(engine.group_cache.len(), 1);
  assert_eq!(engine.api_key_cache.len(), 1);
  assert!(engine.engine_cache_sizes().2 > 0);

  publish_namespace_root(&engine, &selected_root, NamespaceMutationKind::Promote).unwrap().expect("root should change");

  assert_eq!(engine.permissions_cache.len(), 0);
  assert_eq!(engine.index_config_cache.len(), 0);
  assert_eq!(engine.grants_index_cache.len(), 0);
  assert_eq!(engine.group_cache.len(), 0);
  assert_eq!(engine.api_key_cache.len(), 0);
  assert_eq!(engine.engine_cache_sizes().2, 0);
}

#[test]
fn whole_root_publication_requires_an_exact_root_source_transition() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let selected_root = engine.head_hash().unwrap();
  DirectoryOps::new(&engine).create_directory(&RequestContext::system(), "/later").unwrap();
  let current_root = engine.head_hash().unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let fanout = Arc::new(RecordingFanout::default());
  let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::Import);
  batch.set_whole_root_hash(selected_root.clone());
  batch
    .add_source_identity(NamespaceMutationSourceIdentity {
      path: "/not-the-root".to_string(),
      entry_type: Some(EntryType::DirectoryIndex.to_u8()),
      previous_identity: Some(current_root.clone()),
      new_identity: Some(selected_root),
    })
    .unwrap();

  let error = NamespaceMutationCoordinator::with_fanout(&engine, fanout.clone()).execute(batch).unwrap_err();

  assert!(matches!(error, EngineError::InvalidInput(message) if message.contains("canonical '/' DirectoryIndex")));
  assert_eq!(engine.head_hash().unwrap(), current_root);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert!(fanout.acknowledgements().is_empty());
}

#[test]
fn custom_whole_root_fanout_cannot_bypass_engine_owned_authority_cache_invalidation() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let selected_root = engine.head_hash().unwrap();
  let selected_counts = engine.counters().snapshot();
  DirectoryOps::new(&engine).create_directory(&RequestContext::system(), "/later").unwrap();

  engine.permissions_cache.get(&"/".to_string(), &engine).unwrap();
  engine.index_config_cache.get(&"/".to_string(), &engine).unwrap();
  engine.grants_index_cache.get(&(), &engine).unwrap();
  engine.group_cache.get(&uuid::Uuid::new_v4(), &engine).unwrap();
  engine.api_key_cache.get(&uuid::Uuid::new_v4().to_string(), &engine).unwrap();
  let fanout = Arc::new(RecordingFanout::default());

  let soft_before = engine.soft_mutation_runtime_snapshot().unwrap();
  let acknowledgement = publish_namespace_root_with_fanout(&engine, &selected_root, NamespaceMutationKind::Import, fanout.clone())
    .unwrap()
    .expect("root should change");

  assert_eq!(fanout.acknowledgements().len(), 1);
  assert_eq!(engine.permissions_cache.len(), 0);
  assert_eq!(engine.index_config_cache.len(), 0);
  assert_eq!(engine.grants_index_cache.len(), 0);
  assert_eq!(engine.group_cache.len(), 0);
  assert_eq!(engine.api_key_cache.len(), 0);
  let published_counts = engine.counters().snapshot();
  assert_eq!(published_counts.files, selected_counts.files);
  assert_eq!(published_counts.directories, selected_counts.directories);
  assert_eq!(published_counts.symlinks, selected_counts.symlinks);
  assert_eq!(published_counts.logical_data_size, selected_counts.logical_data_size);
  let soft_after = engine.soft_mutation_runtime_snapshot().unwrap();
  assert_eq!(soft_after.queued_notices, soft_before.queued_notices + 1);
  assert_eq!(soft_after.latest_queued_publication_sequence, Some(acknowledgement.publication_sequence));
}

#[test]
fn custom_locator_fanout_cannot_bypass_path_derived_authority_cache_invalidation() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let record = ApiKeyRecord {
    key_id: uuid::Uuid::new_v4(),
    key_hash: "namespace-cache-invalidation".to_string(),
    user_id: Some(uuid::Uuid::nil()),
    created_at: chrono::Utc::now(),
    is_revoked: false,
    expires_at: i64::MAX,
    label: None,
    rules: Vec::new(),
  };
  system_store::store_api_key_for_bootstrap(&engine, &context, &record).unwrap();
  engine.api_key_cache.get(&record.key_id.to_string(), &engine).unwrap();
  assert_eq!(engine.api_key_cache.len(), 1);

  let path = format!("/.aeordb-system/api-keys/{}", record.key_id);
  let locator_key = file_path_hash(&path, &engine.hash_algo()).unwrap();
  let (header, stored_key, stored_value) = engine.get_entry_verified(&locator_key).unwrap().expect("API-key FileRecord locator");
  assert_eq!(stored_key, locator_key);
  let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::SystemWrite);
  batch.replace_locator_with_version(header.entry_type, locator_key.clone(), stored_value, header.flags, header.entry_version).unwrap();
  batch
    .add_source_identity(NamespaceMutationSourceIdentity {
      path,
      entry_type: Some(EntryType::FileRecord.to_u8()),
      previous_identity: Some(locator_key.clone()),
      new_identity: Some(locator_key),
    })
    .unwrap();
  let fanout = Arc::new(RecordingFanout::default());

  NamespaceMutationCoordinator::with_fanout(&engine, fanout.clone()).execute(batch).unwrap();

  assert_eq!(fanout.acknowledgements().len(), 1);
  assert_eq!(engine.api_key_cache.len(), 0);
}

#[test]
fn whole_root_publication_rejects_a_noncanonical_directory_key() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let original_head = engine.head_hash().unwrap();
  let arbitrary_key = vec![0xC4; engine.hash_algo().hash_length()];
  engine.store_entry(EntryType::DirectoryIndex, &arbitrary_key, &[]).unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;

  let error = publish_namespace_root(&engine, &arbitrary_key, NamespaceMutationKind::Promote).unwrap_err();

  assert!(matches!(error, EngineError::CorruptEntry { .. }), "unexpected error: {error}");
  assert_eq!(engine.head_hash().unwrap(), original_head);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
}

#[test]
fn whole_root_publication_rejects_malformed_flat_directory_content() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let original_head = engine.head_hash().unwrap();
  let malformed = b"malformed directory";
  let malformed_hash = directory_content_hash(malformed, &engine.hash_algo()).unwrap();
  engine.store_entry(EntryType::DirectoryIndex, &malformed_hash, malformed).unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;

  let error = publish_namespace_root(&engine, &malformed_hash, NamespaceMutationKind::Promote).unwrap_err();

  assert!(matches!(error, EngineError::UnexpectedEof | EngineError::CorruptEntry { .. }), "unexpected error: {error}");
  assert_eq!(engine.head_hash().unwrap(), original_head);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
}

#[test]
fn same_batch_head_dependency_must_be_a_canonical_directory_root() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let original_head = engine.head_hash().unwrap();
  let arbitrary_key = vec![0xC7; engine.hash_algo().hash_length()];
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::Restore);
  batch.store_dependency(EntryType::DirectoryIndex, arbitrary_key.clone(), Vec::new(), 0).unwrap();
  batch.set_whole_root_hash(arbitrary_key.clone());
  batch
    .add_source_identity(NamespaceMutationSourceIdentity {
      path: "/".to_string(),
      entry_type: Some(EntryType::DirectoryIndex.to_u8()),
      previous_identity: Some(original_head.clone()),
      new_identity: Some(arbitrary_key.clone()),
    })
    .unwrap();

  let error = NamespaceMutationCoordinator::new(&engine).execute(batch).unwrap_err();

  assert!(matches!(error, EngineError::CorruptEntry { .. }), "unexpected error: {error}");
  assert_eq!(engine.head_hash().unwrap(), original_head);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert!(engine.get_kv_entry(&arbitrary_key).unwrap().is_none());
}

#[test]
fn same_batch_head_dependency_must_have_well_formed_directory_content() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let original_head = engine.head_hash().unwrap();
  let malformed = b"malformed same-batch directory";
  let malformed_hash = directory_content_hash(malformed, &engine.hash_algo()).unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::Restore);
  batch.store_dependency(EntryType::DirectoryIndex, malformed_hash.clone(), malformed.to_vec(), 0).unwrap();
  batch.set_whole_root_hash(malformed_hash.clone());
  batch
    .add_source_identity(NamespaceMutationSourceIdentity {
      path: "/".to_string(),
      entry_type: Some(EntryType::DirectoryIndex.to_u8()),
      previous_identity: Some(original_head.clone()),
      new_identity: Some(malformed_hash.clone()),
    })
    .unwrap();

  let error = NamespaceMutationCoordinator::new(&engine).execute(batch).unwrap_err();

  assert!(matches!(error, EngineError::CorruptEntry { .. }), "unexpected error: {error}");
  assert_eq!(engine.head_hash().unwrap(), original_head);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert!(engine.get_kv_entry(&malformed_hash).unwrap().is_none());
}

fn replacement_batch(stable_key: &[u8], value: &[u8]) -> NamespaceMutationBatch {
  let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::FileWrite);
  batch.replace_locator(EntryType::FileRecord, stable_key.to_vec(), value.to_vec(), 0).unwrap();
  batch
    .add_source_identity(NamespaceMutationSourceIdentity {
      path: format!("/spec/file-{:02x}", stable_key[0]),
      entry_type: Some(EntryType::FileRecord.to_u8()),
      previous_identity: None,
      new_identity: Some(stable_key.to_vec()),
    })
    .unwrap();
  batch
}

#[test]
fn successful_mutation_reuses_the_hard_publication_sequence_and_fans_out_once_after_commit() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let stable_key = vec![0xA1; engine.hash_algo().hash_length()];
  let fanout =
    Arc::new(AuthorityCheckingFanout { engine: Arc::clone(&engine), stable_key: stable_key.clone(), calls: Mutex::new(Vec::new()) });
  let coordinator = NamespaceMutationCoordinator::with_fanout(&engine, fanout.clone());
  let soft_before = engine.soft_mutation_runtime_snapshot().unwrap();

  let acknowledgement = coordinator.execute(replacement_batch(&stable_key, b"first locator value")).unwrap();

  assert!(!acknowledgement.operation_id.is_nil());
  assert!(acknowledgement.publication_sequence > 0);
  assert_eq!(acknowledgement.kind, NamespaceMutationKind::FileWrite);
  assert_eq!(fanout.calls.lock().unwrap().as_slice(), &[acknowledgement.clone()]);
  assert_eq!(acknowledgement.locator_replacements.len(), 1);
  assert!(acknowledgement.locator_replacements[0].old_incarnation.is_none());
  assert_eq!(acknowledgement.locator_replacements[0].ordinal, 0);
  assert_eq!(acknowledgement.source_identities.len(), 1);
  let incarnation = acknowledgement.locator_replacements[0].new_incarnation.as_ref().unwrap();
  let published = engine.get_kv_entry(&stable_key).unwrap().unwrap();
  assert_eq!(incarnation.type_flags, published.type_flags);
  assert_eq!(incarnation.total_length, published.total_length);
  let soft_after = engine.soft_mutation_runtime_snapshot().unwrap();
  assert_eq!(soft_after.queued_notices, soft_before.queued_notices + 1);
  assert_eq!(soft_after.latest_queued_publication_sequence, Some(acknowledgement.publication_sequence));
}

#[test]
fn concurrent_mutations_receive_unique_operation_ids_and_hard_publication_sequences() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let soft_before = engine.soft_mutation_runtime_snapshot().unwrap();
  let acknowledgements = std::thread::scope(|scope| {
    let handles: Vec<_> = (0u8..16)
      .map(|index| {
        let engine = Arc::clone(&engine);
        scope.spawn(move || {
          let key = vec![index.saturating_add(1); engine.hash_algo().hash_length()];
          NamespaceMutationCoordinator::new(&engine).execute(replacement_batch(&key, &[index]))
        })
      })
      .collect();
    handles.into_iter().map(|handle| handle.join().unwrap().unwrap()).collect::<Vec<_>>()
  });

  let operation_ids = acknowledgements.iter().map(|acknowledgement| acknowledgement.operation_id).collect::<HashSet<_>>();
  let sequences = acknowledgements.iter().map(|acknowledgement| acknowledgement.publication_sequence).collect::<HashSet<_>>();
  assert_eq!(operation_ids.len(), acknowledgements.len());
  assert_eq!(sequences.len(), acknowledgements.len());
  assert!(sequences.iter().all(|sequence| *sequence > 0));
  let soft = engine.soft_mutation_runtime_snapshot().unwrap();
  let admitted = soft.queued_notices - soft_before.queued_notices;
  let dropped = soft.dropped_notices - soft_before.dropped_notices;
  assert_eq!(admitted as u64 + dropped, acknowledgements.len() as u64);
  assert_eq!(soft.reconciliation_required, soft_before.reconciliation_required || dropped != 0);
}

#[test]
fn concurrent_prepare_and_execute_closures_plan_against_serialized_namespace_authority() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let stable_key = vec![0xA7; engine.hash_algo().hash_length()];

  std::thread::scope(|scope| {
    let handles: Vec<_> = (0..16)
      .map(|_| {
        let engine = Arc::clone(&engine);
        let stable_key = stable_key.clone();
        scope.spawn(move || {
          NamespaceMutationCoordinator::new(&engine).prepare_and_execute(|planning_engine| {
            let previous = planning_engine
              .get_entry(&stable_key)?
              .map(|(_header, _key, value)| u64::from_le_bytes(value.try_into().unwrap()))
              .unwrap_or_default();
            let next = previous + 1;
            let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::FileWrite);
            batch.replace_locator(EntryType::FileRecord, stable_key.clone(), next.to_le_bytes().to_vec(), 0)?;
            batch.add_source_identity(NamespaceMutationSourceIdentity {
              path: "/spec/serialized-plan".to_string(),
              entry_type: Some(EntryType::FileRecord.to_u8()),
              previous_identity: Some(stable_key.clone()),
              new_identity: Some(stable_key.clone()),
            })?;
            Ok((batch, next))
          })
        })
      })
      .collect();

    let mut planned_values = handles.into_iter().map(|handle| handle.join().unwrap().unwrap().1).collect::<Vec<_>>();
    planned_values.sort_unstable();
    assert_eq!(planned_values, (1..=16).collect::<Vec<_>>());
  });

  let (_header, _key, value) = engine.get_entry(&stable_key).unwrap().unwrap();
  assert_eq!(u64::from_le_bytes(value.try_into().unwrap()), 16);
}

#[test]
fn version_aware_plans_preserve_dependency_and_locator_entry_versions() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let dependency_key = vec![0xA8; engine.hash_algo().hash_length()];
  let locator_key = vec![0xA9; engine.hash_algo().hash_length()];
  let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::FileWrite);
  batch.store_dependency_with_version(EntryType::FileRecord, dependency_key.clone(), b"versioned dependency".to_vec(), 0, 1).unwrap();
  batch.replace_locator_with_version(EntryType::FileRecord, locator_key.clone(), b"versioned locator".to_vec(), 0, 1).unwrap();
  batch
    .add_source_identity(NamespaceMutationSourceIdentity {
      path: "/spec/versioned-file".to_string(),
      entry_type: Some(EntryType::FileRecord.to_u8()),
      previous_identity: None,
      new_identity: Some(locator_key.clone()),
    })
    .unwrap();

  NamespaceMutationCoordinator::new(&engine).execute(batch).unwrap();

  assert_eq!(engine.get_entry(&dependency_key).unwrap().unwrap().0.entry_version, 1);
  assert_eq!(engine.get_entry(&locator_key).unwrap().unwrap().0.entry_version, 1);
}

#[test]
fn replacement_and_retirement_preserve_ordered_old_and_new_physical_incarnations() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let coordinator = NamespaceMutationCoordinator::new(&engine);
  let first_key = vec![0xB1; engine.hash_algo().hash_length()];
  let second_key = vec![0xB2; engine.hash_algo().hash_length()];

  let first = coordinator.execute(replacement_batch(&first_key, b"first")).unwrap();
  let first_incarnation = first.locator_replacements[0].new_incarnation.clone().unwrap();

  let mut second_batch = NamespaceMutationBatch::new(NamespaceMutationKind::BatchWrite);
  second_batch.replace_locator(EntryType::FileRecord, second_key.clone(), b"second-key".to_vec(), 0).unwrap();
  second_batch.replace_locator(EntryType::FileRecord, first_key.clone(), b"second-version".to_vec(), 0).unwrap();
  second_batch
    .add_source_identity(NamespaceMutationSourceIdentity {
      path: "/spec/second".to_string(),
      entry_type: Some(EntryType::FileRecord.to_u8()),
      previous_identity: None,
      new_identity: Some(second_key.clone()),
    })
    .unwrap();
  second_batch
    .add_source_identity(NamespaceMutationSourceIdentity {
      path: "/spec/first".to_string(),
      entry_type: Some(EntryType::FileRecord.to_u8()),
      previous_identity: Some(first_key.clone()),
      new_identity: Some(first_key.clone()),
    })
    .unwrap();
  let second = coordinator.execute(second_batch).unwrap();

  assert_ne!(first.operation_id, second.operation_id);
  assert!(second.publication_sequence > first.publication_sequence);
  assert_eq!(second.locator_replacements.iter().map(|replacement| replacement.ordinal).collect::<Vec<_>>(), vec![0, 1]);
  assert_eq!(second.locator_replacements[1].old_incarnation.as_ref(), Some(&first_incarnation));
  assert_ne!(second.locator_replacements[1].new_incarnation.as_ref().unwrap().offset, first_incarnation.offset);

  let mut retire_batch = NamespaceMutationBatch::new(NamespaceMutationKind::FileDelete);
  retire_batch.retire_locator(first_key.clone()).unwrap();
  retire_batch
    .add_source_identity(NamespaceMutationSourceIdentity {
      path: "/spec/first".to_string(),
      entry_type: Some(EntryType::FileRecord.to_u8()),
      previous_identity: Some(first_key.clone()),
      new_identity: None,
    })
    .unwrap();
  let retired = coordinator.execute(retire_batch).unwrap();
  assert_eq!(retired.locator_replacements[0].old_incarnation.as_ref(), second.locator_replacements[1].new_incarnation.as_ref());
  assert!(retired.locator_replacements[0].new_incarnation.is_none());
  assert!(engine.get_kv_entry(&first_key).unwrap().is_none());
}

#[test]
fn malformed_or_duplicate_plans_fail_before_transaction_or_locator_mutation() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let fanout = Arc::new(RecordingFanout::default());
  let coordinator = NamespaceMutationCoordinator::with_fanout(&engine, fanout.clone());
  let stable_key = vec![0xC1; engine.hash_algo().hash_length()];
  let initial_sequence = engine.durability_snapshot().unwrap().next_sequence;

  let mut missing_source = NamespaceMutationBatch::new(NamespaceMutationKind::FileWrite);
  missing_source.replace_locator(EntryType::FileRecord, stable_key.clone(), b"missing source identity".to_vec(), 0).unwrap();
  assert!(coordinator.execute(missing_source).is_err());
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, initial_sequence);
  assert!(engine.get_kv_entry(&stable_key).unwrap().is_none());

  let identity_free_key = vec![0xC4; engine.hash_algo().hash_length()];
  let mut identity_free_source = NamespaceMutationBatch::new(NamespaceMutationKind::FileWrite);
  identity_free_source.replace_locator(EntryType::FileRecord, identity_free_key.clone(), b"identity-free source".to_vec(), 0).unwrap();
  assert!(identity_free_source
    .add_source_identity(NamespaceMutationSourceIdentity {
      path: "/spec/identity-free".to_string(),
      entry_type: Some(EntryType::FileRecord.to_u8()),
      previous_identity: None,
      new_identity: None,
    })
    .is_err());
  assert!(coordinator.execute(identity_free_source).is_err());
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, initial_sequence);
  assert!(engine.get_kv_entry(&identity_free_key).unwrap().is_none());

  let invalid_type_key = vec![0xC7; engine.hash_algo().hash_length()];
  let mut invalid_source_type = NamespaceMutationBatch::new(NamespaceMutationKind::FileWrite);
  invalid_source_type.replace_locator(EntryType::FileRecord, invalid_type_key.clone(), b"invalid source type".to_vec(), 0).unwrap();
  invalid_source_type
    .add_source_identity(NamespaceMutationSourceIdentity {
      path: "/spec/invalid-source-type".to_string(),
      entry_type: Some(0xFF),
      previous_identity: None,
      new_identity: Some(invalid_type_key.clone()),
    })
    .unwrap();
  assert!(coordinator.execute(invalid_source_type).is_err());
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, initial_sequence);
  assert!(engine.get_kv_entry(&invalid_type_key).unwrap().is_none());

  let mut duplicate = NamespaceMutationBatch::new(NamespaceMutationKind::FileWrite);
  duplicate.replace_locator(EntryType::FileRecord, stable_key.clone(), b"first".to_vec(), 0).unwrap();
  assert!(duplicate.replace_locator(EntryType::FileRecord, stable_key.clone(), b"duplicate".to_vec(), 0).is_err());

  let mut malformed_head = NamespaceMutationBatch::new(NamespaceMutationKind::FileWrite);
  malformed_head.set_whole_root_hash(vec![0x01]);
  malformed_head
    .add_source_identity(NamespaceMutationSourceIdentity {
      path: "/spec/malformed-head".to_string(),
      entry_type: None,
      previous_identity: Some(stable_key.clone()),
      new_identity: Some(stable_key.clone()),
    })
    .unwrap();
  assert!(coordinator.execute(malformed_head).is_err());
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, initial_sequence);
  assert!(engine.get_kv_entry(&stable_key).unwrap().is_none());
  assert!(fanout.acknowledgements().is_empty());
}

#[test]
fn locator_replacement_cannot_overwrite_an_immutable_payload_key() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let stable_key = vec![0xC9; engine.hash_algo().hash_length()];
  engine.store_entry(EntryType::Chunk, &stable_key, b"immutable payload").unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let batch = replacement_batch(&stable_key, b"mutable locator");

  let error = NamespaceMutationCoordinator::new(&engine).execute(batch).unwrap_err();

  assert!(matches!(error, EngineError::InvalidInput(_)), "unexpected error: {error}");
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  let (header, stored_key, value) = engine.get_entry_verified(&stable_key).unwrap().unwrap();
  assert_eq!(header.entry_type, EntryType::Chunk);
  assert_eq!(stored_key, stable_key);
  assert_eq!(value, b"immutable payload");
}

#[test]
fn dependency_preflight_rejects_an_existing_wrong_type_before_authority_changes() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let dependency_key = vec![0xCA; engine.hash_algo().hash_length()];
  let locator_key = vec![0xCB; engine.hash_algo().hash_length()];
  engine.store_entry(EntryType::Chunk, &dependency_key, b"occupied by a chunk").unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::SyncApply);
  batch.store_dependency(EntryType::FileRecord, dependency_key.clone(), b"retained file version".to_vec(), 0).unwrap();
  batch.replace_locator(EntryType::FileRecord, locator_key.clone(), b"published file".to_vec(), 0).unwrap();
  batch
    .add_source_identity(NamespaceMutationSourceIdentity {
      path: "/spec/dependency-collision".to_string(),
      entry_type: Some(EntryType::FileRecord.to_u8()),
      previous_identity: None,
      new_identity: Some(locator_key.clone()),
    })
    .unwrap();

  let error = NamespaceMutationCoordinator::new(&engine).execute(batch).unwrap_err();

  assert!(matches!(error, EngineError::CorruptEntry { .. }), "unexpected error: {error}");
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert!(engine.get_kv_entry(&locator_key).unwrap().is_none());
  let (header, stored_key, value) = engine.get_entry_verified(&dependency_key).unwrap().unwrap();
  assert_eq!(header.entry_type, EntryType::Chunk);
  assert_eq!(stored_key, dependency_key);
  assert_eq!(value, b"occupied by a chunk");
}

#[test]
fn typed_identity_dependencies_may_replace_prior_file_and_symlink_serializations() {
  for (entry_type, seed) in [(EntryType::FileRecord, 0xCC), (EntryType::Symlink, 0xCE)] {
    let (engine, _temporary) = create_temp_engine_for_tests();
    let dependency_key = vec![seed; engine.hash_algo().hash_length()];
    let locator_key = vec![seed + 1; engine.hash_algo().hash_length()];
    engine.store_entry_with_version(entry_type, &dependency_key, b"older timestamp serialization", 0).unwrap();
    let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::SyncApply);
    batch.store_dependency_with_version(entry_type, dependency_key.clone(), b"newer timestamp serialization".to_vec(), 0, 1).unwrap();
    batch.replace_locator(entry_type, locator_key.clone(), b"published locator".to_vec(), 0).unwrap();
    batch
      .add_source_identity(NamespaceMutationSourceIdentity {
        path: format!("/spec/typed-identity-{}", entry_type.to_u8()),
        entry_type: Some(entry_type.to_u8()),
        previous_identity: None,
        new_identity: Some(locator_key),
      })
      .unwrap();

    NamespaceMutationCoordinator::new(&engine).execute(batch).unwrap();

    let (header, stored_key, value) = engine.get_entry_verified(&dependency_key).unwrap().unwrap();
    assert_eq!(header.entry_type, entry_type);
    assert_eq!(header.entry_version, 1);
    assert_eq!(stored_key, dependency_key);
    assert_eq!(value, b"newer timestamp serialization");
  }
}

#[test]
fn no_op_batches_and_dependency_locator_aliases_are_rejected_before_admission() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let fanout = Arc::new(RecordingFanout::default());
  let coordinator = NamespaceMutationCoordinator::with_fanout(&engine, fanout.clone());
  let stable_key = vec![0xC2; engine.hash_algo().hash_length()];
  let initial_sequence = engine.durability_snapshot().unwrap().next_sequence;
  let soft_before = engine.soft_mutation_runtime_snapshot().unwrap();

  assert!(coordinator.execute(NamespaceMutationBatch::new(NamespaceMutationKind::FileWrite)).is_err());

  let mut unchanged_head = NamespaceMutationBatch::new(NamespaceMutationKind::Restore);
  unchanged_head.set_whole_root_hash(engine.head_hash().unwrap());
  unchanged_head
    .add_source_identity(NamespaceMutationSourceIdentity {
      path: "/".to_string(),
      entry_type: None,
      previous_identity: Some(engine.head_hash().unwrap()),
      new_identity: Some(engine.head_hash().unwrap()),
    })
    .unwrap();
  assert!(coordinator.execute(unchanged_head).is_err());

  let mut aliased = NamespaceMutationBatch::new(NamespaceMutationKind::FileWrite);
  aliased.store_dependency(EntryType::FileRecord, stable_key.clone(), b"dependency".to_vec(), 0).unwrap();
  assert!(aliased.replace_locator(EntryType::FileRecord, stable_key.clone(), b"locator".to_vec(), 0).is_err());
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, initial_sequence);
  assert!(engine.get_kv_entry(&stable_key).unwrap().is_none());
  assert!(fanout.acknowledgements().is_empty());
  assert_eq!(engine.soft_mutation_runtime_snapshot().unwrap(), soft_before);
}

#[test]
fn head_transition_rejects_a_missing_or_wrong_type_root_before_admission() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let fanout = Arc::new(RecordingFanout::default());
  let coordinator = NamespaceMutationCoordinator::with_fanout(&engine, fanout.clone());
  let original_head = engine.head_hash().unwrap();
  let initial_sequence = engine.durability_snapshot().unwrap().next_sequence;

  let missing_root = vec![0xD4; engine.hash_algo().hash_length()];
  let mut missing = NamespaceMutationBatch::new(NamespaceMutationKind::Restore);
  missing.set_whole_root_hash(missing_root.clone());
  missing
    .add_source_identity(NamespaceMutationSourceIdentity {
      path: "/".to_string(),
      entry_type: Some(EntryType::DirectoryIndex.to_u8()),
      previous_identity: Some(original_head.clone()),
      new_identity: Some(missing_root),
    })
    .unwrap();
  assert!(coordinator.execute(missing).is_err());

  let wrong_type_root = vec![0xD5; engine.hash_algo().hash_length()];
  engine.store_entry(EntryType::FileRecord, &wrong_type_root, b"not a directory root").unwrap();
  let mut wrong_type = NamespaceMutationBatch::new(NamespaceMutationKind::Restore);
  wrong_type.set_whole_root_hash(wrong_type_root.clone());
  wrong_type
    .add_source_identity(NamespaceMutationSourceIdentity {
      path: "/".to_string(),
      entry_type: Some(EntryType::DirectoryIndex.to_u8()),
      previous_identity: Some(original_head.clone()),
      new_identity: Some(wrong_type_root),
    })
    .unwrap();
  assert!(coordinator.execute(wrong_type).is_err());

  assert_eq!(engine.head_hash().unwrap(), original_head);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, initial_sequence);
  assert!(fanout.acknowledgements().is_empty());
}

#[test]
fn dependency_only_batches_and_non_namespace_locator_types_are_rejected_before_admission() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let fanout = Arc::new(RecordingFanout::default());
  let coordinator = NamespaceMutationCoordinator::with_fanout(&engine, fanout.clone());
  let dependency_key = vec![0xC5; engine.hash_algo().hash_length()];
  let locator_key = vec![0xC6; engine.hash_algo().hash_length()];
  let initial_sequence = engine.durability_snapshot().unwrap().next_sequence;

  let mut dependency_only = NamespaceMutationBatch::new(NamespaceMutationKind::FileWrite);
  dependency_only.store_dependency(EntryType::Chunk, dependency_key.clone(), b"unselected dependency".to_vec(), 0).unwrap();
  dependency_only
    .add_source_identity(NamespaceMutationSourceIdentity {
      path: "/spec/dependency-only".to_string(),
      entry_type: Some(EntryType::Chunk.to_u8()),
      previous_identity: None,
      new_identity: Some(dependency_key.clone()),
    })
    .unwrap();
  assert!(coordinator.execute(dependency_only).is_err());

  let mut chunk_locator = NamespaceMutationBatch::new(NamespaceMutationKind::FileWrite);
  assert!(chunk_locator.replace_locator(EntryType::Chunk, locator_key.clone(), b"not a mutable locator".to_vec(), 0).is_err());
  chunk_locator
    .add_source_identity(NamespaceMutationSourceIdentity {
      path: "/spec/chunk-locator".to_string(),
      entry_type: Some(EntryType::Chunk.to_u8()),
      previous_identity: None,
      new_identity: Some(locator_key.clone()),
    })
    .unwrap();
  assert!(coordinator.execute(chunk_locator).is_err());

  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, initial_sequence);
  assert!(engine.get_kv_entry(&dependency_key).unwrap().is_none());
  assert!(engine.get_kv_entry(&locator_key).unwrap().is_none());
  assert!(fanout.acknowledgements().is_empty());
}

#[test]
fn durability_waiter_pressure_refuses_before_mutation_and_authority_remains_reusable() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let fanout = Arc::new(RecordingFanout::default());
  let coordinator = NamespaceMutationCoordinator::with_fanout(&engine, fanout.clone());
  let stable_key = vec![0xC3; engine.hash_algo().hash_length()];
  let memory = engine.memory_coordinator();
  let snapshot = memory.snapshot().unwrap();
  let remaining = snapshot.policy.unwrap().emergency_reserve_bytes - snapshot.critical_reserved_bytes;
  let pressure = memory
    .reserve(MemoryOwner::DurabilityWaiters, remaining.saturating_sub(1), AdmissionClass::Critical(CriticalMemoryPurpose::DurableWrite))
    .unwrap();
  let initial_sequence = engine.durability_snapshot().unwrap().next_sequence;

  assert!(coordinator.execute(replacement_batch(&stable_key, b"must not be written")).is_err());
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, initial_sequence);
  assert!(engine.get_kv_entry(&stable_key).unwrap().is_none());
  assert!(fanout.acknowledgements().is_empty());

  drop(pressure);
  let acknowledgement = coordinator.execute(replacement_batch(&stable_key, b"written after pressure release")).unwrap();
  assert_eq!(fanout.acknowledgements(), vec![acknowledgement]);
}

#[test]
fn nested_top_level_mutation_is_refused_before_writes_and_authority_remains_reusable() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let fanout = Arc::new(RecordingFanout::default());
  let coordinator = NamespaceMutationCoordinator::with_fanout(&engine, fanout.clone());
  let stable_key = vec![0xD1; engine.hash_algo().hash_length()];
  let outer_transaction = TransactionGuard::new(&engine).unwrap();

  assert!(coordinator.execute(replacement_batch(&stable_key, b"must not be written")).is_err());
  assert!(engine.get_kv_entry(&stable_key).unwrap().is_none());
  assert!(fanout.acknowledgements().is_empty());
  outer_transaction.commit().unwrap();

  let acknowledgement = coordinator.execute(replacement_batch(&stable_key, b"written after reuse")).unwrap();
  assert_eq!(fanout.acknowledgements(), vec![acknowledgement]);
}

#[test]
fn soft_fanout_panic_cannot_turn_durably_committed_success_into_failure() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let stable_key = vec![0xE1; engine.hash_algo().hash_length()];
  let coordinator = NamespaceMutationCoordinator::with_fanout(&engine, Arc::new(PanickingFanout));

  let soft_before = engine.soft_mutation_runtime_snapshot().unwrap();
  let result = coordinator.execute(replacement_batch(&stable_key, b"committed despite soft panic"));

  assert!(result.is_ok());
  assert!(engine.get_kv_entry(&stable_key).unwrap().is_some());
  let soft_after = engine.soft_mutation_runtime_snapshot().unwrap();
  assert_eq!(soft_after.queued_notices, soft_before.queued_notices + 1);
  assert_eq!(soft_after.latest_queued_publication_sequence, Some(result.unwrap().publication_sequence));
}

#[test]
fn oversized_engine_soft_notice_latches_reconciliation_without_failing_hard_commit_or_caller_fanout() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let stable_key = vec![0xE2; engine.hash_algo().hash_length()];
  let path = format!("/{}", "x".repeat(300 * 1_024));
  let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::BatchWrite);
  batch.replace_locator(EntryType::FileRecord, stable_key.clone(), b"durable oversized notice".to_vec(), 0).unwrap();
  batch
    .add_source_identity(NamespaceMutationSourceIdentity {
      path,
      entry_type: Some(EntryType::FileRecord.to_u8()),
      previous_identity: None,
      new_identity: Some(stable_key.clone()),
    })
    .unwrap();
  let fanout = Arc::new(RecordingFanout::default());
  let before = engine.soft_mutation_runtime_snapshot().unwrap();

  let acknowledgement = NamespaceMutationCoordinator::with_fanout(&engine, fanout.clone()).execute(batch).unwrap();

  assert!(engine.get_kv_entry(&stable_key).unwrap().is_some());
  assert_eq!(fanout.acknowledgements(), vec![acknowledgement.clone()]);
  let after = engine.soft_mutation_runtime_snapshot().unwrap();
  assert_eq!(after.queued_notices, before.queued_notices);
  assert_eq!(after.lost_through_sequence, Some(acknowledgement.publication_sequence));
  assert!(after.loss_reasons.contains(&aeordb::engine::v4::coverage_runtime::SoftMutationLossReasonV1::NoticeTooLarge));
}

#[test]
fn converged_waves_activate_only_characterized_namespace_producers() {
  fn visit(directory: &std::path::Path, violations: &mut Vec<String>) {
    for entry in std::fs::read_dir(directory).unwrap() {
      let path = entry.unwrap().path();
      if path.is_dir() {
        visit(&path, violations);
      } else if path.extension().and_then(|value| value.to_str()) == Some("rs")
        && path.file_name().and_then(|value| value.to_str()) != Some("namespace_mutation.rs")
        && !path.ends_with("engine/mod.rs")
        && path.file_name().and_then(|value| value.to_str()) != Some("directory_ops.rs")
        && path.file_name().and_then(|value| value.to_str()) != Some("version_manager.rs")
        && path.file_name().and_then(|value| value.to_str()) != Some("backup.rs")
        && path.file_name().and_then(|value| value.to_str()) != Some("task_queue.rs")
      {
        let source = std::fs::read_to_string(&path).unwrap();
        if source.contains("NamespaceMutationCoordinator") || source.contains("LocatorReplacementCoordinator") {
          violations.push(path.display().to_string());
        }
      }
    }
  }

  let package = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
  let facade = std::fs::read_to_string(package.join("src/engine/namespace_mutation.rs")).unwrap();
  assert!(facade.contains("pub struct NamespaceMutationCoordinator"));
  assert!(facade.contains("pub struct LocatorReplacementCoordinator"));
  assert!(facade.contains("commit_top_level_after"));
  assert_eq!(
    facade.matches("offer_soft_namespace_mutation(&acknowledgement)").count(),
    1,
    "every producer must share one engine-owned post-authority soft handoff"
  );

  let mut violations = Vec::new();
  visit(&package.join("src"), &mut violations);
  assert!(violations.is_empty(), "P2e must not activate uncharacterized later-wave producers: {}", violations.join(", "));

  let directory_ops = std::fs::read_to_string(package.join("src/engine/directory_ops.rs")).unwrap();
  assert!(directory_ops.contains("NamespaceMutationCoordinator"), "the characterized Wave 1 producer must use the shared facade");
  let version_manager = std::fs::read_to_string(package.join("src/engine/version_manager.rs")).unwrap();
  assert!(version_manager.contains("NamespaceMutationCoordinator"), "the characterized Wave 3 version producer must use the shared facade");
  let backup = std::fs::read_to_string(package.join("src/engine/backup.rs")).unwrap();
  assert!(backup.contains("NamespaceMutationCoordinator"), "the characterized Wave 3 backup producer must use the shared facade");
  let task_queue = std::fs::read_to_string(package.join("src/engine/task_queue.rs")).unwrap();
  assert!(task_queue.contains("NamespaceMutationCoordinator"), "the characterized Wave 4 task producer must use the shared facade");

  let storage_engine = std::fs::read_to_string(package.join("src/engine/storage_engine.rs")).unwrap();
  assert_eq!(storage_engine.matches("offer_acknowledgement(acknowledgement)").count(), 1);
  let mut duplicate_soft_producers = Vec::new();
  for entry in std::fs::read_dir(package.join("src/engine")).unwrap() {
    let path = entry.unwrap().path();
    if path.extension().and_then(|value| value.to_str()) != Some("rs")
      || matches!(path.file_name().and_then(|value| value.to_str()), Some("namespace_mutation.rs" | "storage_engine.rs"))
    {
      continue;
    }
    let source = std::fs::read_to_string(&path).unwrap();
    if source.contains("offer_soft_namespace_mutation") || source.contains("offer_acknowledgement(acknowledgement)") {
      duplicate_soft_producers.push(path.display().to_string());
    }
  }
  assert!(duplicate_soft_producers.is_empty(), "soft mutation handoff bypasses exist: {duplicate_soft_producers:?}");
}

#[test]
fn wave_four_task_storage_cannot_bypass_shared_locator_authority() {
  let package = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
  let source = std::fs::read_to_string(package.join("src/engine/task_queue.rs")).unwrap();

  assert!(source.contains("NamespaceMutationCoordinator"), "task persistence must use the shared hard-authority coordinator");
  assert!(source.contains("NamespaceMutationBatch"), "compound task transitions must be planned as one locator batch");
  assert!(!source.contains("self.engine.store_entry("), "task persistence must not append raw detached rows");
  assert!(!source.contains("self.engine.mark_entry_deleted("), "task pruning must not retire detached rows outside the shared batch");
}

#[test]
fn wave_one_entrypoints_cannot_reintroduce_split_namespace_authority() {
  fn method_source(source: &str, name: &str) -> String {
    let marker = format!("fn {name}");
    let mut collecting = false;
    let mut lines = Vec::new();
    for line in source.lines() {
      if !collecting {
        if line.contains(&marker) {
          collecting = true;
          lines.push(line);
        }
        continue;
      }
      if line.starts_with("  fn ") || line.starts_with("  pub fn ") || line.starts_with("  pub(crate) fn ") {
        break;
      }
      lines.push(line);
    }
    assert!(collecting, "missing DirectoryOps method {name}");
    lines.join("\n")
  }

  let package = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
  let source = std::fs::read_to_string(package.join("src/engine/directory_ops.rs")).unwrap();
  let entrypoints = [
    ("finalize_file_with_content_hash", "execute_file_publication"),
    ("store_file_internal_inner", "execute_file_publication"),
    ("delete_file", "delete_files_batch_with_kind"),
    ("delete_files_batch_with_kind", "execute_optional_namespace_mutation"),
    ("delete_directory", "execute_namespace_mutation"),
    ("create_directory", "execute_namespace_mutation"),
    ("migrate_file_record_to_current_version_inner", "execute_optional_namespace_mutation"),
    ("ensure_root_directory", "execute_optional_namespace_mutation"),
    ("rebuild_directory_tree", "store_rebuilt_directory"),
    ("repair_directory_index_from_path_records", "store_rebuilt_directory"),
    ("repair_stale_dir_key", "execute_optional_namespace_mutation"),
    ("store_rebuilt_directory", "execute_namespace_mutation"),
    ("delete_file_with_indexing", "delete_file("),
    ("store_symlink", "execute_namespace_mutation"),
    ("delete_symlink", "execute_namespace_mutation"),
  ];
  let forbidden = [
    "direct_hard_authority_guard",
    "TransactionGuard::new",
    ".flush_batch",
    ".update_head",
    ".mark_entry_deleted",
    "self.update_parent_directories(",
    "self.remove_from_parent_directory(",
  ];

  for (name, required) in entrypoints {
    let method = method_source(&source, name);
    assert!(method.contains(required), "Wave 1 entrypoint {name} must route through {required}");
    for bypass in forbidden {
      assert!(!method.contains(bypass), "Wave 1 entrypoint {name} reintroduced split authority through {bypass}");
    }
    if name != "store_file_internal_inner" {
      assert!(!method.contains(".store_entry"), "Wave 1 entrypoint {name} bypassed dependency planning with a direct entry write");
    }
  }

  let stale_locator_repair = method_source(&source, "repair_stale_dir_key");
  assert!(
    !stale_locator_repair.contains("self.engine.store_entry("),
    "stale directory-locator repair must not bypass shared maintenance authority"
  );
}

#[test]
fn wave_three_version_entrypoints_cannot_reintroduce_split_namespace_authority() {
  fn method_source(source: &str, name: &str) -> String {
    let marker = format!("fn {name}");
    let mut collecting = false;
    let mut lines = Vec::new();
    for line in source.lines() {
      if !collecting {
        if line.contains(&marker) {
          collecting = true;
          lines.push(line);
        }
        continue;
      }
      if line.starts_with("  fn ") || line.starts_with("  pub fn ") || line.starts_with("  pub(crate) fn ") {
        break;
      }
      lines.push(line);
    }
    assert!(collecting, "missing VersionManager method {name}");
    lines.join("\n")
  }

  let package = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
  let source = std::fs::read_to_string(package.join("src/engine/version_manager.rs")).unwrap();
  let entrypoints = [
    "create_snapshot",
    "restore_snapshot",
    "delete_snapshot",
    "rename_snapshot",
    "create_fork",
    "promote_fork",
    "abandon_fork",
    "update_fork_hash",
  ];
  let forbidden = [
    "direct_hard_authority_guard",
    ".update_head",
    ".store_entry",
    ".store_entry_typed",
    ".mark_entry_deleted",
    "persist_deletion",
    "self.abandon_fork",
  ];

  for name in entrypoints {
    let method = method_source(&source, name);
    assert!(method.contains("execute_version_mutation"), "Wave 3 version entrypoint {name} must use the shared namespace authority");
    for bypass in forbidden {
      assert!(!method.contains(bypass), "Wave 3 version entrypoint {name} reintroduced split authority through {bypass}");
    }
  }
}

#[test]
fn wave_three_backup_import_and_promotion_cannot_reintroduce_split_authority() {
  let backup = include_str!("../../src/engine/backup.rs");
  let mode_start = backup.find("impl TransferDestinationMode").unwrap();
  let mode_end = backup[mode_start..].find("struct ImportLocatorBatch").unwrap() + mode_start;
  let mode_source = &backup[mode_start..mode_end];
  assert!(
    mode_source.contains("matches!(self, Self::FullImport | Self::SparseImport)"),
    "only active full/sparse imports may coordinate current path locators"
  );

  let locator_batch_start = backup.find("struct ImportLocatorBatch").unwrap();
  let locator_batch_end = backup[locator_batch_start..].find("fn import_locator_charge").unwrap() + locator_batch_start;
  let locator_batch_source = &backup[locator_batch_start..locator_batch_end];
  assert!(
    locator_batch_source.contains("NamespaceMutationBatch::new(NamespaceMutationKind::MaintenanceRepair)"),
    "derived import path locators must retain their maintenance-reconciliation classification"
  );
  assert!(locator_batch_source.contains("NamespaceMutationCoordinator::new(self.output).execute(batch)"));
  assert!(!locator_batch_source.contains("let _ ="), "locator-batch cleanup errors must not be silently discarded");

  let tree_writer_start = backup.find("fn write_tree_to_engine").unwrap();
  let tree_writer_end = backup[tree_writer_start..].find("fn write_transfer_directories").unwrap() + tree_writer_start;
  let tree_writer_source = &backup[tree_writer_start..tree_writer_end];
  assert!(tree_writer_source.contains("destination_mode.coordinates_active_locators()"));
  assert!(tree_writer_source.contains("active FileRecord import is missing its locator publication batch"));
  assert!(tree_writer_source.contains("active symlink import is missing its locator publication batch"));
  assert!(tree_writer_source.contains(".replace(EntryType::FileRecord"));
  assert!(tree_writer_source.contains(".replace(EntryType::Symlink"));
  assert!(!tree_writer_source.contains("locator_batch.as_mut().expect("));
  assert!(tree_writer_source.contains("locator_batch.flush()?"));

  let alias_policy_start = backup.find("fn should_store_transfer_alias").unwrap();
  let alias_policy_end = backup[alias_policy_start..].find("fn collect_transfer_btree_entries").unwrap() + alias_policy_start;
  let alias_policy_source = &backup[alias_policy_start..alias_policy_end];
  assert!(
    alias_policy_source.contains("TransferDestinationMode::HistoricalImport => return Ok(false)"),
    "historical snapshot imports must remain immutable-content-only and never publish current path locators"
  );

  let import_start = backup.find("pub fn import_backup(").unwrap();
  let import_source = &backup[import_start..];
  for forbidden in [
    "target.update_head(",
    "target.mark_entry_deleted(",
    "target.store_entry_with_version(EntryType::Snapshot",
    "target.store_entry_with_flags_and_version(EntryType::Snapshot",
  ] {
    assert!(!import_source.contains(forbidden), "active backup import retained split-authority token: {forbidden}");
  }
  assert!(import_source.contains("ImportLocatorBatch::new(target)"));
  assert!(backup.contains("publish_namespace_root_from_with_fanout"));
  assert!(import_source.contains("NamespaceMutationCoordinator::new(target).prepare_and_maybe_execute"));
  assert!(import_source.contains("deletion.previous_identity"), "sparse deletion must carry the selected prior entity identity");

  let server_route = include_str!("../../src/server/backup_routes.rs");
  let promote_start = server_route.find("pub async fn promote_head(").unwrap();
  let promote_source = &server_route[promote_start..];
  assert!(promote_source.contains("publish_namespace_root"));
  assert!(!promote_source.contains(".update_head("));

  let cli = include_str!("../../../aeordb-cli/src/commands/promote.rs");
  assert!(cli.contains("publish_namespace_root"));
  assert!(!cli.contains(".update_head("));
}

#[test]
fn wave_five_exit_allows_reviewed_p3c_contracts_but_keeps_migration_runtime_unactivated() {
  fn visit_rust_sources(directory: &std::path::Path, sources: &mut Vec<(std::path::PathBuf, String)>) {
    for entry in std::fs::read_dir(directory).unwrap() {
      let path = entry.unwrap().path();
      if path.is_dir() {
        visit_rust_sources(&path, sources);
      } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
        sources.push((path.clone(), std::fs::read_to_string(path).unwrap()));
      }
    }
  }

  let package = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
  let mut sources = Vec::new();
  visit_rust_sources(&package.join("src"), &mut sources);

  let capture_callers = sources
    .iter()
    .flat_map(|(path, source)| source.match_indices("capture_migration_run_configuration(").map(move |_| path))
    .collect::<Vec<_>>();
  assert_eq!(capture_callers.len(), 1, "migration configuration capture acquired a premature runtime caller: {capture_callers:?}");
  assert_eq!(capture_callers[0].file_name().and_then(|value| value.to_str()), Some("run_configuration.rs"));

  let run_configuration = sources
    .iter()
    .find(|(path, _)| path.file_name().and_then(|value| value.to_str()) == Some("run_configuration.rs"))
    .map(|(_, source)| source)
    .unwrap();
  assert!(run_configuration.contains("Called when the P3c migration state owner is activated"));
  assert!(run_configuration.contains("#[allow(dead_code)]"));

  let migration_control_sources = sources
    .iter()
    .filter(|(path, source)| {
      path.file_name().and_then(|value| value.to_str()) != Some("system_control.rs")
        && (source.contains("SystemControlKindV1::MigrationLease") || source.contains("SystemControlKindV1::MigrationProgress"))
    })
    .map(|(path, source)| (path, source))
    .collect::<Vec<_>>();
  assert_eq!(
    migration_control_sources.len(),
    2,
    "migration controls escaped their reviewed codec and state owner: {migration_control_sources:?}"
  );
  let codec = migration_control_sources
    .iter()
    .find(|(path, _)| path.file_name().and_then(|value| value.to_str()) == Some("migration_control.rs"))
    .map(|(_, source)| *source)
    .expect("reviewed migration codec");
  let owner = migration_control_sources
    .iter()
    .find(|(path, _)| path.file_name().and_then(|value| value.to_str()) == Some("migration_owner.rs"))
    .map(|(_, source)| *source)
    .expect("reviewed migration state owner");
  for forbidden in [
    "StorageEngine",
    "DirectoryOps",
    "V4ControlStore",
    "V3TransitionControlStore",
    "FirstAuthority",
    "std::fs",
    "publish_mutable",
    "capture_migration_run_configuration",
  ] {
    assert!(!codec.contains(forbidden), "disconnected migration codec acquired premature runtime dependency {forbidden}");
  }
  assert!(owner.contains("V4FirstAuthorityPublisher"));
  assert!(owner.contains("MigrationPreflightPermitV1"));
  for forbidden in [
    "StorageEngine",
    "DirectoryOps",
    "V4ControlStore",
    "V3TransitionControlStore",
    "std::fs",
    "server::",
    "axum",
    "task_worker",
    "GarbageCollector",
    "capture_migration_run_configuration",
  ] {
    assert!(!owner.contains(forbidden), "disconnected migration owner acquired premature runtime dependency {forbidden}");
  }

  let v4_module = std::fs::read_to_string(package.join("src/engine/v4/mod.rs")).unwrap();
  assert!(v4_module.contains("pub mod migration_control;"), "ratified P3c migration codec is not exported");
  assert!(v4_module.contains("pub mod migration_owner;"), "ratified P3c migration state owner is not exported");
  assert!(v4_module.contains("pub mod migration_preflight;"), "ratified P3c preflight contract is not exported");
  for forbidden in ["pub mod migration;", "pub mod migration_runtime;", "pub mod migration_capture;"] {
    assert!(!v4_module.contains(forbidden), "migration runtime module was activated before its owning phase: {forbidden}");
  }
  assert!(
    !sources.iter().any(|(path, _)| {
      matches!(path.file_name().and_then(|value| value.to_str()), Some("migration.rs" | "migration_runtime.rs" | "migration_capture.rs"))
    }),
    "migration runtime source exists without its authority review"
  );
}

#[test]
fn wave_three_restore_entrypoints_cannot_reintroduce_split_namespace_authority() {
  fn method_source(source: &str, name: &str) -> String {
    let marker = format!("fn {name}");
    let mut collecting = false;
    let mut lines = Vec::new();
    for line in source.lines() {
      if !collecting {
        if line.contains(&marker) {
          collecting = true;
          lines.push(line);
        }
        continue;
      }
      if line.starts_with("  fn ") || line.starts_with("  pub fn ") || line.starts_with("  pub(crate) fn ") {
        break;
      }
      lines.push(line);
    }
    assert!(collecting, "missing DirectoryOps method {name}");
    lines.join("\n")
  }

  let source = include_str!("../../src/engine/directory_ops.rs");
  for entrypoint in ["restore_file_from_record", "restore_deleted_file"] {
    let method = method_source(source, entrypoint);
    assert!(method.contains("execute_file_record_restore"), "Wave 3 restore entrypoint {entrypoint} must use the shared restore planner");
    for forbidden in ["TransactionGuard::new", "namespace_write_guard", "materialize_file_record_entries", "update_parent_directories"] {
      assert!(!method.contains(forbidden), "Wave 3 restore entrypoint {entrypoint} retained split authority through {forbidden}");
    }
  }

  let helper = method_source(source, "execute_file_record_restore");
  assert!(helper.contains("execute_file_publication"));
  for forbidden in ["TransactionGuard::new", "namespace_write_guard", "materialize_file_record_entries", "update_parent_directories"] {
    assert!(!helper.contains(forbidden), "shared restore planner retained split authority through {forbidden}");
  }
}
