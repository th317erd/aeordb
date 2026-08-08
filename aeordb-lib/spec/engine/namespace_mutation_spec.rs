use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use aeordb::engine::entry_type::EntryType;
use aeordb::engine::memory_coordinator::{AdmissionClass, CriticalMemoryPurpose, MemoryOwner};
use aeordb::engine::namespace_mutation::{
  NamespaceMutationAcknowledgement, NamespaceMutationBatch, NamespaceMutationCoordinator, NamespaceMutationFanout, NamespaceMutationKind,
  NamespaceMutationSourceIdentity,
};
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
}

#[test]
fn concurrent_mutations_receive_unique_operation_ids_and_hard_publication_sequences() {
  let (engine, _temporary) = create_temp_engine_for_tests();
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
  malformed_head.set_head_hash(vec![0x01]);
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
fn no_op_batches_and_dependency_locator_aliases_are_rejected_before_admission() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let fanout = Arc::new(RecordingFanout::default());
  let coordinator = NamespaceMutationCoordinator::with_fanout(&engine, fanout.clone());
  let stable_key = vec![0xC2; engine.hash_algo().hash_length()];
  let initial_sequence = engine.durability_snapshot().unwrap().next_sequence;

  assert!(coordinator.execute(NamespaceMutationBatch::new(NamespaceMutationKind::FileWrite)).is_err());

  let mut unchanged_head = NamespaceMutationBatch::new(NamespaceMutationKind::Restore);
  unchanged_head.set_head_hash(engine.head_hash().unwrap());
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

  let result = coordinator.execute(replacement_batch(&stable_key, b"committed despite soft panic"));

  assert!(result.is_ok());
  assert!(engine.get_kv_entry(&stable_key).unwrap().is_some());
}

#[test]
fn facade_boundary_is_present_but_production_producers_remain_unmigrated_until_p2e() {
  fn visit(directory: &std::path::Path, violations: &mut Vec<String>) {
    for entry in std::fs::read_dir(directory).unwrap() {
      let path = entry.unwrap().path();
      if path.is_dir() {
        visit(&path, violations);
      } else if path.extension().and_then(|value| value.to_str()) == Some("rs")
        && path.file_name().and_then(|value| value.to_str()) != Some("namespace_mutation.rs")
        && !path.ends_with("engine/mod.rs")
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

  let mut violations = Vec::new();
  visit(&package.join("src"), &mut violations);
  assert!(violations.is_empty(), "P2d must not falsely activate producers before their characterized P2e waves: {}", violations.join(", "));
}
