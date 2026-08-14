use std::collections::BTreeMap;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::index_artifact::EncodedImmutableIndexArtifactV1;
use aeordb::engine::v4::index_coordinator_recovery::{
  IndexCheckpointRootV1, IndexRecoveryErrorV1, IndexRecoveryOptionsV1, IndexRecoveryOutcomeV1, IndexRecoveryOwnerV1,
  IndexRecoveryPublicationRequestV1, IndexRecoveryReasonV1, IndexRecoveryStoreErrorV1, IndexRecoveryStoreV1,
  publish_index_recovery_checkpoint_v1, recover_index_checkpoint_v1,
};
use aeordb::engine::v4::index_task::{
  IndexTaskAttachmentRoleV1, IndexTaskAttachmentWriteV1, IndexTaskCheckpointWriteV1, IndexTaskKindV1, IndexTaskStateV1, JournalOwnerKindV1,
  MutationJournalWriteV1, MutationKindV1, MutationRecordWriteV1, MutationSideWriteV1, encode_index_task_checkpoint,
  encode_mutation_journal,
};
use tokio_util::sync::CancellationToken;

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;
const SYSTEM_JOURNAL_OWNER: [u8; 16] = *b"AEORIDXJOURNALV1";

fn hash(byte: u8) -> Vec<u8> {
  hash_for(ALGORITHM, byte)
}

fn hash_for(algorithm: HashAlgorithm, byte: u8) -> Vec<u8> {
  vec![byte; algorithm.hash_length()]
}

fn memory(limit: u64) -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(limit - 1, limit, 1, 1).unwrap())
}

fn options() -> IndexRecoveryOptionsV1 {
  IndexRecoveryOptionsV1::new(32, 64 * 1_024 * 1_024, 32, 64 * 1_024 * 1_024).unwrap()
}

fn owner() -> IndexRecoveryOwnerV1 {
  IndexRecoveryOwnerV1::new([1; 16], hash(2), [3; 16]).unwrap()
}

fn journal(previous: &[u8], ordinal: u64, generation: u64, sequence: u64, before: &[u8], after: &[u8]) -> EncodedImmutableIndexArtifactV1 {
  journal_for(ALGORITHM, previous, ordinal, generation, sequence, before, after)
}

fn journal_for(
  algorithm: HashAlgorithm,
  previous: &[u8],
  ordinal: u64,
  generation: u64,
  sequence: u64,
  before: &[u8],
  after: &[u8],
) -> EncodedImmutableIndexArtifactV1 {
  let mutation_id = hash_for(algorithm, sequence as u8);
  let revision = hash_for(algorithm, 0x41);
  let record = MutationRecordWriteV1 {
    kind: MutationKindV1::Create,
    sequence,
    mutation_id: &mutation_id,
    batch_ordinal: 0,
    batch_count: 1,
    root_before: before,
    root_after: after,
    before: None,
    after: Some(MutationSideWriteV1 { path: "/docs/a.txt", revision: &revision }),
    committed_at_ms: 1_000 + sequence,
  };
  encode_mutation_journal(&MutationJournalWriteV1 {
    hash_algorithm: algorithm,
    owner_id: SYSTEM_JOURNAL_OWNER,
    owner_kind: JournalOwnerKindV1::System,
    generation,
    segment_ordinal: ordinal,
    chain_reset: previous.iter().all(|byte| *byte == 0),
    previous_segment: previous,
    semantic_state_root: &hash_for(algorithm, 0x51),
    runtime_boot_id: [0x61; 16],
    records: &[record],
  })
  .unwrap()
}

fn checkpoint(
  owner: &IndexRecoveryOwnerV1,
  journal: &EncodedImmutableIndexArtifactV1,
  sequence: u64,
  generation: u64,
  source_root: &[u8],
  target_root: &[u8],
  floor: u64,
  audited: u64,
) -> EncodedImmutableIndexArtifactV1 {
  checkpoint_for(ALGORITHM, owner, journal, sequence, generation, source_root, target_root, floor, audited)
}

fn checkpoint_for(
  algorithm: HashAlgorithm,
  owner: &IndexRecoveryOwnerV1,
  journal: &EncodedImmutableIndexArtifactV1,
  sequence: u64,
  generation: u64,
  source_root: &[u8],
  target_root: &[u8],
  floor: u64,
  audited: u64,
) -> EncodedImmutableIndexArtifactV1 {
  let attachment_owner = hash_for(algorithm, 0x71);
  let attachments = [IndexTaskAttachmentWriteV1 {
    role: IndexTaskAttachmentRoleV1::MutationJournalHead,
    owner_id: &attachment_owner,
    artifact_hash: &journal.key,
    birth_generation: generation,
  }];
  encode_index_task_checkpoint(&IndexTaskCheckpointWriteV1 {
    hash_algorithm: algorithm,
    task_id: owner.operation_id(),
    checkpoint_sequence: sequence,
    generation,
    task_kind: IndexTaskKindV1::Reconcile,
    state: IndexTaskStateV1::Running,
    phase: 2,
    required_capabilities: &[0; 32],
    started_at_ms: 1_000,
    updated_at_ms: 2_000,
    source_root,
    target_root: Some(target_root),
    primary_id: Some(owner.index_id()),
    journal_head: Some(&journal.key),
    journal_floor_sequence: floor,
    journal_audited_through: audited,
    next_document_ordinal: 1,
    completed_work: 1,
    total_work_hint: 2,
    resume_key: b"resume",
    attachments: &attachments,
    external: None,
  })
  .unwrap()
}

#[derive(Default)]
struct RecordingStore {
  artifacts: BTreeMap<Vec<u8>, Vec<u8>>,
  selected: BTreeMap<(Vec<u8>, [u8; 16]), IndexCheckpointRootV1>,
  events: Vec<String>,
  fail_event: Option<String>,
  fail_after_publish: bool,
  cancel_on_event: Option<(String, CancellationToken)>,
  change_on_load: Option<Vec<u8>>,
}

impl RecordingStore {
  fn fail_at(mut self, event: &str) -> Self {
    self.fail_event = Some(event.to_string());
    self
  }

  fn event(&mut self, name: String) -> Result<(), IndexRecoveryStoreErrorV1> {
    self.events.push(name.clone());
    if let Some((event, cancellation)) = &self.cancel_on_event {
      if event == &name {
        cancellation.cancel();
      }
    }
    if self.fail_event.as_deref() == Some(name.as_str()) {
      return Err(IndexRecoveryStoreErrorV1::new("injected_store_failure", name));
    }
    Ok(())
  }
}

fn reserved_index_bytes(memory: &MemoryCoordinator) -> u64 {
  memory.snapshot().unwrap().owner(aeordb::engine::memory_coordinator::MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes
}

fn assert_reconciliation(outcome: IndexRecoveryOutcomeV1, expected: IndexRecoveryReasonV1) -> Option<String> {
  let IndexRecoveryOutcomeV1::ReconciliationRequired { reason, evidence } = outcome else {
    panic!("expected reconciliation for {expected:?}, received {outcome:?}");
  };
  assert_eq!(reason, expected);
  evidence
}

impl IndexRecoveryStoreV1 for RecordingStore {
  fn immutable_length(&mut self, key: &[u8]) -> Result<Option<u64>, IndexRecoveryStoreErrorV1> {
    self.event(format!("length:{}", hex::encode(key)))?;
    Ok(self.artifacts.get(key).map(|value| value.len() as u64))
  }

  fn load_immutable(&mut self, key: &[u8], expected_length: u64) -> Result<Option<Vec<u8>>, IndexRecoveryStoreErrorV1> {
    self.event(format!("load:{}", hex::encode(key)))?;
    let Some(value) = self.artifacts.get(key) else {
      return Ok(None);
    };
    if value.len() as u64 != expected_length {
      return Ok(None);
    }
    let mut loaded = value.clone();
    if self.change_on_load.as_deref() == Some(key) {
      loaded.pop();
    }
    Ok(Some(loaded))
  }

  fn put_immutable(&mut self, artifact: &EncodedImmutableIndexArtifactV1) -> Result<(), IndexRecoveryStoreErrorV1> {
    self.event(format!("put:{}", hex::encode(&artifact.key)))?;
    if let Some(existing) = self.artifacts.get(&artifact.key) {
      if existing != &artifact.value {
        return Err(IndexRecoveryStoreErrorV1::new("immutable_collision", "same key has different bytes"));
      }
      return Ok(());
    }
    self.artifacts.insert(artifact.key.clone(), artifact.value.clone());
    Ok(())
  }

  fn sync_immutable(&mut self) -> Result<(), IndexRecoveryStoreErrorV1> {
    self.event("sync".to_string())
  }

  fn load_selected(&mut self, owner: &IndexRecoveryOwnerV1) -> Result<Option<IndexCheckpointRootV1>, IndexRecoveryStoreErrorV1> {
    self.event("load-selected".to_string())?;
    Ok(self.selected.get(&(owner.index_id().to_vec(), owner.operation_id())).cloned())
  }

  fn publish_selected_synced(
    &mut self,
    owner: &IndexRecoveryOwnerV1,
    expected: Option<&IndexCheckpointRootV1>,
    next: &IndexCheckpointRootV1,
  ) -> Result<(), IndexRecoveryStoreErrorV1> {
    self.event("publish-selected".to_string())?;
    let key = (owner.index_id().to_vec(), owner.operation_id());
    if self.selected.get(&key) != expected {
      return Err(IndexRecoveryStoreErrorV1::new("selection_conflict", "selected checkpoint changed"));
    }
    self.selected.insert(key, next.clone());
    if self.fail_after_publish {
      return Err(IndexRecoveryStoreErrorV1::new("injected_commit_unknown", "selector write completed before error"));
    }
    Ok(())
  }
}

#[test]
fn dependencies_and_checkpoint_are_synced_before_selector_and_recover_exactly() {
  let owner = owner();
  let source = hash(0x11);
  let target = hash(0x12);
  let zero = vec![0; ALGORITHM.hash_length()];
  let journal = journal(&zero, 1, 7, 11, &source, &target);
  let checkpoint_artifact = checkpoint(&owner, &journal, 1, 7, &source, &target, 10, 11);
  let mut store = RecordingStore::default();
  let receipt = publish_index_recovery_checkpoint_v1(
    &mut store,
    IndexRecoveryPublicationRequestV1 {
      hash_algorithm: ALGORITHM,
      owner: &owner,
      expected: None,
      checkpoint: &checkpoint_artifact,
      dependencies: &[&journal],
      options: options(),
      memory: &memory(128 * 1_024 * 1_024),
      cancellation: &CancellationToken::new(),
    },
  )
  .unwrap();
  assert!(!receipt.idempotent);
  let sync = store.events.iter().position(|event| event == "sync").unwrap();
  let publish = store.events.iter().position(|event| event == "publish-selected").unwrap();
  assert!(sync < publish);
  let journal_load = format!("load:{}", hex::encode(&journal.key));
  assert_eq!(store.events.iter().filter(|event| *event == &journal_load).count(), 1);

  let recovered =
    recover_index_checkpoint_v1(&mut store, ALGORITHM, &owner, options(), &memory(128 * 1_024 * 1_024), &CancellationToken::new()).unwrap();
  let IndexRecoveryOutcomeV1::Resumable(state) = recovered else {
    panic!("expected resumable checkpoint");
  };
  assert_eq!(state.checkpoint_sequence, 1);
  assert_eq!(state.journal.last_sequence, 11);
  assert_eq!(state.journal.source_root_before, source);
  assert_eq!(state.journal.source_root_after, target);
  assert_eq!(state.rooted_artifact_count, 1);
}

#[test]
fn publication_and_restart_recovery_support_every_database_hash_profile() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let owner = IndexRecoveryOwnerV1::new([1; 16], hash_for(algorithm, 2), [3; 16]).unwrap();
    let source = hash_for(algorithm, 0x11);
    let target = hash_for(algorithm, 0x12);
    let zero = vec![0; algorithm.hash_length()];
    let journal = journal_for(algorithm, &zero, 1, 7, 11, &source, &target);
    let checkpoint = checkpoint_for(algorithm, &owner, &journal, 1, 7, &source, &target, 10, 11);
    let mut store = RecordingStore::default();
    let receipt = publish_index_recovery_checkpoint_v1(
      &mut store,
      IndexRecoveryPublicationRequestV1 {
        hash_algorithm: algorithm,
        owner: &owner,
        expected: None,
        checkpoint: &checkpoint,
        dependencies: &[&journal],
        options: options(),
        memory: &memory(128 * 1_024 * 1_024),
        cancellation: &CancellationToken::new(),
      },
    )
    .unwrap();
    assert_eq!(receipt.selected.checkpoint_key.len(), algorithm.hash_length());
    let recovered =
      recover_index_checkpoint_v1(&mut store, algorithm, &owner, options(), &memory(128 * 1_024 * 1_024), &CancellationToken::new())
        .unwrap();
    let IndexRecoveryOutcomeV1::Resumable(state) = recovered else {
      panic!("{algorithm:?} checkpoint was not resumable: {recovered:?}");
    };
    assert_eq!(state.journal.source_root_before, source);
    assert_eq!(state.journal.source_root_after, target);
  }
}

#[test]
fn absent_missing_corrupt_and_discontinuous_recovery_never_become_empty_success() {
  let owner = owner();
  let mut absent = RecordingStore::default();
  assert_reconciliation(
    recover_index_checkpoint_v1(&mut absent, ALGORITHM, &owner, options(), &memory(128 * 1_024 * 1_024), &CancellationToken::new())
      .unwrap(),
    IndexRecoveryReasonV1::CheckpointSelectionMissing,
  );

  let source = hash(0x11);
  let target = hash(0x12);
  let zero = vec![0; ALGORITHM.hash_length()];
  let journal = journal(&zero, 1, 7, 11, &source, &target);
  let checkpoint_artifact = checkpoint(&owner, &journal, 1, 7, &source, &target, 10, 11);
  for expected in [IndexRecoveryReasonV1::CheckpointMissing, IndexRecoveryReasonV1::JournalMissing, IndexRecoveryReasonV1::JournalCorrupt] {
    let mut store = RecordingStore::default();
    let root = IndexCheckpointRootV1::new(1, checkpoint_artifact.key.clone()).unwrap();
    store.selected.insert((owner.index_id().to_vec(), owner.operation_id()), root);
    match expected {
      IndexRecoveryReasonV1::CheckpointMissing => {}
      IndexRecoveryReasonV1::JournalMissing => {
        store.artifacts.insert(checkpoint_artifact.key.clone(), checkpoint_artifact.value.clone());
      }
      IndexRecoveryReasonV1::JournalCorrupt => {
        store.artifacts.insert(checkpoint_artifact.key.clone(), checkpoint_artifact.value.clone());
        store.artifacts.insert(journal.key.clone(), b"corrupt".to_vec());
      }
      _ => unreachable!(),
    }
    assert_reconciliation(
      recover_index_checkpoint_v1(&mut store, ALGORITHM, &owner, options(), &memory(128 * 1_024 * 1_024), &CancellationToken::new())
        .unwrap(),
      expected,
    );
  }

  let wrong_target = checkpoint(&owner, &journal, 1, 7, &source, &hash(0x13), 10, 11);
  let mut store = RecordingStore::default();
  store.artifacts.insert(journal.key.clone(), journal.value.clone());
  store.artifacts.insert(wrong_target.key.clone(), wrong_target.value.clone());
  store
    .selected
    .insert((owner.index_id().to_vec(), owner.operation_id()), IndexCheckpointRootV1::new(1, wrong_target.key.clone()).unwrap());
  assert_reconciliation(
    recover_index_checkpoint_v1(&mut store, ALGORITHM, &owner, options(), &memory(128 * 1_024 * 1_024), &CancellationToken::new()).unwrap(),
    IndexRecoveryReasonV1::JournalChainDiscontinuous,
  );
}

#[test]
fn publication_failure_or_incomplete_closure_never_advances_the_old_selector() {
  let owner = owner();
  let source = hash(0x11);
  let target = hash(0x12);
  let zero = vec![0; ALGORITHM.hash_length()];
  let journal = journal(&zero, 1, 7, 11, &source, &target);
  let initial_checkpoint = checkpoint(&owner, &journal, 1, 7, &source, &target, 10, 11);

  let mut no_dependency = RecordingStore::default();
  let error = publish_index_recovery_checkpoint_v1(
    &mut no_dependency,
    IndexRecoveryPublicationRequestV1 {
      hash_algorithm: ALGORITHM,
      owner: &owner,
      expected: None,
      checkpoint: &initial_checkpoint,
      dependencies: &[],
      options: options(),
      memory: &memory(128 * 1_024 * 1_024),
      cancellation: &CancellationToken::new(),
    },
  )
  .unwrap_err();
  assert!(matches!(error, IndexRecoveryErrorV1::ReconciliationRequired { reason: IndexRecoveryReasonV1::JournalMissing, .. }));
  assert!(no_dependency.selected.is_empty());

  let old = IndexCheckpointRootV1::new(9, hash(0x91)).unwrap();
  let next_checkpoint = checkpoint(&owner, &journal, 10, 7, &source, &target, 10, 11);
  for event in ["sync", "publish-selected"] {
    let mut store = RecordingStore::default().fail_at(event);
    store.selected.insert((owner.index_id().to_vec(), owner.operation_id()), old.clone());
    let result = publish_index_recovery_checkpoint_v1(
      &mut store,
      IndexRecoveryPublicationRequestV1 {
        hash_algorithm: ALGORITHM,
        owner: &owner,
        expected: Some(&old),
        checkpoint: &next_checkpoint,
        dependencies: &[&journal],
        options: options(),
        memory: &memory(128 * 1_024 * 1_024),
        cancellation: &CancellationToken::new(),
      },
    );
    assert!(result.is_err());
    assert!(store.events.iter().any(|observed| observed == event), "injected fault was not reached: {event}");
    assert_eq!(store.selected.get(&(owner.index_id().to_vec(), owner.operation_id())), Some(&old));
  }
}

#[test]
fn cancellation_and_memory_pressure_fail_before_selection_changes() {
  let owner = owner();
  let source = hash(0x11);
  let target = hash(0x12);
  let zero = vec![0; ALGORITHM.hash_length()];
  let journal = journal(&zero, 1, 7, 11, &source, &target);
  let checkpoint = checkpoint(&owner, &journal, 1, 7, &source, &target, 10, 11);
  let cancellation = CancellationToken::new();
  cancellation.cancel();
  let mut store = RecordingStore::default();
  assert!(matches!(
    publish_index_recovery_checkpoint_v1(
      &mut store,
      IndexRecoveryPublicationRequestV1 {
        hash_algorithm: ALGORITHM,
        owner: &owner,
        expected: None,
        checkpoint: &checkpoint,
        dependencies: &[&journal],
        options: options(),
        memory: &memory(128 * 1_024 * 1_024),
        cancellation: &cancellation,
      },
    ),
    Err(IndexRecoveryErrorV1::Canceled)
  ));
  assert!(store.events.is_empty());

  store.artifacts.insert(journal.key.clone(), journal.value.clone());
  store.artifacts.insert(checkpoint.key.clone(), checkpoint.value.clone());
  store.selected.insert((owner.index_id().to_vec(), owner.operation_id()), IndexCheckpointRootV1::new(1, checkpoint.key.clone()).unwrap());
  assert!(matches!(
    recover_index_checkpoint_v1(&mut store, ALGORITHM, &owner, options(), &memory(128), &CancellationToken::new()),
    Err(IndexRecoveryErrorV1::Memory(_))
  ));
}

#[test]
fn malformed_recovery_contracts_fail_before_storage_and_bad_selected_roots_require_reconciliation() {
  assert!(IndexRecoveryOptionsV1::new(0, 1, 1, 1).is_err());
  assert!(IndexRecoveryOptionsV1::new(1, 0, 1, 1).is_err());
  assert!(IndexRecoveryOptionsV1::new(1, 1, 0, 1).is_err());
  assert!(IndexRecoveryOptionsV1::new(1, 1, 1, 0).is_err());
  assert!(IndexRecoveryOwnerV1::new([0; 16], hash(2), [3; 16]).is_err());
  assert!(IndexRecoveryOwnerV1::new([1; 16], Vec::new(), [3; 16]).is_err());
  assert!(IndexRecoveryOwnerV1::new([1; 16], hash(2), [0; 16]).is_err());
  assert!(IndexCheckpointRootV1::new(0, hash(1)).is_err());
  assert!(IndexCheckpointRootV1::new(1, vec![0; ALGORITHM.hash_length()]).is_err());

  let wrong_width_owner = IndexRecoveryOwnerV1::new([1; 16], vec![2; ALGORITHM.hash_length() - 1], [3; 16]).unwrap();
  let mut untouched = RecordingStore::default();
  assert!(matches!(
    recover_index_checkpoint_v1(
      &mut untouched,
      ALGORITHM,
      &wrong_width_owner,
      options(),
      &memory(128 * 1_024 * 1_024),
      &CancellationToken::new(),
    ),
    Err(IndexRecoveryErrorV1::Invalid(_))
  ));
  assert!(untouched.events.is_empty());

  let owner = owner();
  let mut malformed_selected = RecordingStore::default();
  malformed_selected.selected.insert(
    (owner.index_id().to_vec(), owner.operation_id()),
    IndexCheckpointRootV1::new(1, vec![0x55; ALGORITHM.hash_length() - 1]).unwrap(),
  );
  assert_reconciliation(
    recover_index_checkpoint_v1(
      &mut malformed_selected,
      ALGORITHM,
      &owner,
      options(),
      &memory(128 * 1_024 * 1_024),
      &CancellationToken::new(),
    )
    .unwrap(),
    IndexRecoveryReasonV1::CheckpointCorrupt,
  );

  let source = hash(0x11);
  let target = hash(0x12);
  let zero = vec![0; ALGORITHM.hash_length()];
  let journal = journal(&zero, 1, 7, 11, &source, &target);
  let second = checkpoint(&owner, &journal, 2, 7, &source, &target, 10, 11);
  let mut invalid_first = RecordingStore::default();
  let result = publish_index_recovery_checkpoint_v1(
    &mut invalid_first,
    IndexRecoveryPublicationRequestV1 {
      hash_algorithm: ALGORITHM,
      owner: &owner,
      expected: None,
      checkpoint: &second,
      dependencies: &[&journal],
      options: options(),
      memory: &memory(128 * 1_024 * 1_024),
      cancellation: &CancellationToken::new(),
    },
  );
  assert!(matches!(result, Err(IndexRecoveryErrorV1::Invalid(_))));
  assert!(invalid_first.events.iter().all(|event| !event.starts_with("put:")));

  let first = checkpoint(&owner, &journal, 1, 7, &source, &target, 10, 11);
  for dependencies in [vec![&journal, &journal], vec![&first]] {
    let mut invalid_dependencies = RecordingStore::default();
    let result = publish_index_recovery_checkpoint_v1(
      &mut invalid_dependencies,
      IndexRecoveryPublicationRequestV1 {
        hash_algorithm: ALGORITHM,
        owner: &owner,
        expected: None,
        checkpoint: &first,
        dependencies: &dependencies,
        options: options(),
        memory: &memory(128 * 1_024 * 1_024),
        cancellation: &CancellationToken::new(),
      },
    );
    assert!(matches!(result, Err(IndexRecoveryErrorV1::Invalid(_))));
    assert!(invalid_dependencies.events.is_empty());
  }
}

#[test]
fn every_store_failure_is_preserved_and_never_advances_selection() {
  let owner = owner();
  let source = hash(0x51);
  let target = hash(0x52);
  let zero = vec![0; ALGORITHM.hash_length()];
  let journal = journal(&zero, 1, 7, 11, &source, &target);
  let checkpoint = checkpoint(&owner, &journal, 1, 7, &source, &target, 10, 11);
  let publication_failures = [
    "load-selected".to_string(),
    format!("put:{}", hex::encode(&journal.key)),
    format!("put:{}", hex::encode(&checkpoint.key)),
    "sync".to_string(),
    format!("length:{}", hex::encode(&checkpoint.key)),
    format!("load:{}", hex::encode(&checkpoint.key)),
    format!("length:{}", hex::encode(&journal.key)),
    format!("load:{}", hex::encode(&journal.key)),
    "publish-selected".to_string(),
  ];
  for event in publication_failures {
    let memory = memory(128 * 1_024 * 1_024);
    let mut store = RecordingStore::default().fail_at(&event);
    let result = publish_index_recovery_checkpoint_v1(
      &mut store,
      IndexRecoveryPublicationRequestV1 {
        hash_algorithm: ALGORITHM,
        owner: &owner,
        expected: None,
        checkpoint: &checkpoint,
        dependencies: &[&journal],
        options: options(),
        memory: &memory,
        cancellation: &CancellationToken::new(),
      },
    );
    let Err(IndexRecoveryErrorV1::Store(error)) = result else {
      panic!("store failure at {event} was not preserved: {result:?}");
    };
    assert_eq!(error.code(), "injected_store_failure");
    assert!(store.events.iter().any(|observed| observed == &event), "injected fault was not reached: {event}");
    assert!(store.selected.is_empty(), "selector changed after {event}");
    assert_eq!(reserved_index_bytes(&memory), 0, "memory remained reserved after {event}");
  }

  let recovery_failures = [
    "load-selected".to_string(),
    format!("length:{}", hex::encode(&checkpoint.key)),
    format!("load:{}", hex::encode(&checkpoint.key)),
    format!("length:{}", hex::encode(&journal.key)),
    format!("load:{}", hex::encode(&journal.key)),
  ];
  for event in recovery_failures {
    let memory = memory(128 * 1_024 * 1_024);
    let mut store = RecordingStore::default().fail_at(&event);
    store.artifacts.insert(journal.key.clone(), journal.value.clone());
    store.artifacts.insert(checkpoint.key.clone(), checkpoint.value.clone());
    let selected = IndexCheckpointRootV1::new(1, checkpoint.key.clone()).unwrap();
    store.selected.insert((owner.index_id().to_vec(), owner.operation_id()), selected.clone());
    let result = recover_index_checkpoint_v1(&mut store, ALGORITHM, &owner, options(), &memory, &CancellationToken::new());
    let Err(IndexRecoveryErrorV1::Store(error)) = result else {
      panic!("recovery store failure at {event} was not preserved: {result:?}");
    };
    assert_eq!(error.code(), "injected_store_failure");
    assert_eq!(store.selected.get(&(owner.index_id().to_vec(), owner.operation_id())), Some(&selected));
    assert_eq!(reserved_index_bytes(&memory), 0, "recovery memory remained reserved after {event}");
  }
}

#[test]
fn cancellation_during_publication_or_restart_releases_memory_and_preserves_selection() {
  let owner = owner();
  let source = hash(0x61);
  let target = hash(0x62);
  let zero = vec![0; ALGORITHM.hash_length()];
  let journal = journal(&zero, 1, 7, 11, &source, &target);
  let checkpoint = checkpoint(&owner, &journal, 1, 7, &source, &target, 10, 11);

  let publication_cancellation = CancellationToken::new();
  let mut publishing =
    RecordingStore { cancel_on_event: Some(("sync".to_string(), publication_cancellation.clone())), ..RecordingStore::default() };
  let publication_memory = memory(128 * 1_024 * 1_024);
  assert!(matches!(
    publish_index_recovery_checkpoint_v1(
      &mut publishing,
      IndexRecoveryPublicationRequestV1 {
        hash_algorithm: ALGORITHM,
        owner: &owner,
        expected: None,
        checkpoint: &checkpoint,
        dependencies: &[&journal],
        options: options(),
        memory: &publication_memory,
        cancellation: &publication_cancellation,
      },
    ),
    Err(IndexRecoveryErrorV1::Canceled)
  ));
  assert!(publishing.selected.is_empty());
  assert_eq!(reserved_index_bytes(&publication_memory), 0);

  let recovery_cancellation = CancellationToken::new();
  let mut recovering = RecordingStore {
    cancel_on_event: Some((format!("load:{}", hex::encode(&checkpoint.key)), recovery_cancellation.clone())),
    ..RecordingStore::default()
  };
  recovering.artifacts.insert(journal.key.clone(), journal.value.clone());
  recovering.artifacts.insert(checkpoint.key.clone(), checkpoint.value.clone());
  recovering
    .selected
    .insert((owner.index_id().to_vec(), owner.operation_id()), IndexCheckpointRootV1::new(1, checkpoint.key.clone()).unwrap());
  let recovery_memory = memory(128 * 1_024 * 1_024);
  assert_eq!(
    recover_index_checkpoint_v1(&mut recovering, ALGORITHM, &owner, options(), &recovery_memory, &recovery_cancellation).unwrap(),
    IndexRecoveryOutcomeV1::Canceled
  );
  assert_eq!(reserved_index_bytes(&recovery_memory), 0);
}

#[test]
fn two_segment_restart_requires_the_complete_chain_and_obeys_replay_bounds() {
  let owner = owner();
  let source = hash(0x21);
  let middle = hash(0x22);
  let target = hash(0x23);
  let zero = vec![0; ALGORITHM.hash_length()];
  let first = journal(&zero, 1, 7, 11, &source, &middle);
  let second = journal(&first.key, 2, 7, 12, &middle, &target);
  let checkpoint = checkpoint(&owner, &second, 1, 7, &source, &target, 10, 12);
  let selected = IndexCheckpointRootV1::new(1, checkpoint.key.clone()).unwrap();
  let mut store = RecordingStore::default();
  for artifact in [&first, &second, &checkpoint] {
    store.artifacts.insert(artifact.key.clone(), artifact.value.clone());
  }
  store.selected.insert((owner.index_id().to_vec(), owner.operation_id()), selected);

  let recovered =
    recover_index_checkpoint_v1(&mut store, ALGORITHM, &owner, options(), &memory(128 * 1_024 * 1_024), &CancellationToken::new()).unwrap();
  let IndexRecoveryOutcomeV1::Resumable(state) = recovered else {
    panic!("complete two-segment chain did not resume");
  };
  assert_eq!(state.journal.segment_count, 2);
  assert_eq!(state.journal.record_count, 2);
  assert_eq!(state.journal.last_sequence, 12);

  store.artifacts.remove(&first.key);
  assert_reconciliation(
    recover_index_checkpoint_v1(&mut store, ALGORITHM, &owner, options(), &memory(128 * 1_024 * 1_024), &CancellationToken::new()).unwrap(),
    IndexRecoveryReasonV1::JournalMissing,
  );
  store.artifacts.insert(first.key.clone(), first.value.clone());
  let tight = IndexRecoveryOptionsV1::new(32, 64 * 1_024 * 1_024, 1, 64 * 1_024 * 1_024).unwrap();
  assert_reconciliation(
    recover_index_checkpoint_v1(&mut store, ALGORITHM, &owner, tight, &memory(128 * 1_024 * 1_024), &CancellationToken::new()).unwrap(),
    IndexRecoveryReasonV1::RecoveryLimitExceeded,
  );
}

#[test]
fn idempotent_retry_revalidates_bytes_without_republishing_the_selector() {
  let owner = owner();
  let source = hash(0x31);
  let target = hash(0x32);
  let zero = vec![0; ALGORITHM.hash_length()];
  let journal = journal(&zero, 1, 7, 11, &source, &target);
  let checkpoint = checkpoint(&owner, &journal, 1, 7, &source, &target, 10, 11);
  let memory = memory(128 * 1_024 * 1_024);
  let cancellation = CancellationToken::new();
  let mut store = RecordingStore::default();
  let first = publish_index_recovery_checkpoint_v1(
    &mut store,
    IndexRecoveryPublicationRequestV1 {
      hash_algorithm: ALGORITHM,
      owner: &owner,
      expected: None,
      checkpoint: &checkpoint,
      dependencies: &[&journal],
      options: options(),
      memory: &memory,
      cancellation: &cancellation,
    },
  )
  .unwrap();
  let published_before = store.events.iter().filter(|event| event.as_str() == "publish-selected").count();
  let second = publish_index_recovery_checkpoint_v1(
    &mut store,
    IndexRecoveryPublicationRequestV1 {
      hash_algorithm: ALGORITHM,
      owner: &owner,
      expected: Some(&first.selected),
      checkpoint: &checkpoint,
      dependencies: &[&journal],
      options: options(),
      memory: &memory,
      cancellation: &cancellation,
    },
  )
  .unwrap();
  assert!(second.idempotent);
  assert_eq!(store.events.iter().filter(|event| event.as_str() == "publish-selected").count(), published_before);
}

#[test]
fn commit_unknown_selector_error_reopens_as_exact_new_and_retries_idempotently() {
  let owner = owner();
  let source = hash(0x71);
  let target = hash(0x72);
  let zero = vec![0; ALGORITHM.hash_length()];
  let journal = journal(&zero, 1, 7, 11, &source, &target);
  let checkpoint = checkpoint(&owner, &journal, 1, 7, &source, &target, 10, 11);
  let memory = memory(128 * 1_024 * 1_024);
  let cancellation = CancellationToken::new();
  let mut store = RecordingStore { fail_after_publish: true, ..RecordingStore::default() };
  let result = publish_index_recovery_checkpoint_v1(
    &mut store,
    IndexRecoveryPublicationRequestV1 {
      hash_algorithm: ALGORITHM,
      owner: &owner,
      expected: None,
      checkpoint: &checkpoint,
      dependencies: &[&journal],
      options: options(),
      memory: &memory,
      cancellation: &cancellation,
    },
  );
  assert!(matches!(result, Err(IndexRecoveryErrorV1::Store(ref error)) if error.code() == "injected_commit_unknown"));
  let selected = store.selected.get(&(owner.index_id().to_vec(), owner.operation_id())).cloned().unwrap();
  let recovered = recover_index_checkpoint_v1(&mut store, ALGORITHM, &owner, options(), &memory, &cancellation).unwrap();
  assert!(matches!(recovered, IndexRecoveryOutcomeV1::Resumable(_)));

  store.fail_after_publish = false;
  let retried = publish_index_recovery_checkpoint_v1(
    &mut store,
    IndexRecoveryPublicationRequestV1 {
      hash_algorithm: ALGORITHM,
      owner: &owner,
      expected: Some(&selected),
      checkpoint: &checkpoint,
      dependencies: &[&journal],
      options: options(),
      memory: &memory,
      cancellation: &cancellation,
    },
  )
  .unwrap();
  assert!(retried.idempotent);
  assert_eq!(store.events.iter().filter(|event| event.as_str() == "publish-selected").count(), 1);
  assert_eq!(reserved_index_bytes(&memory), 0);
}

#[test]
fn stale_selection_and_changed_after_probe_bytes_fail_closed() {
  let owner = owner();
  let source = hash(0x41);
  let target = hash(0x42);
  let zero = vec![0; ALGORITHM.hash_length()];
  let journal = journal(&zero, 1, 7, 11, &source, &target);
  let first_checkpoint = checkpoint(&owner, &journal, 1, 7, &source, &target, 10, 11);
  let second_checkpoint = checkpoint(&owner, &journal, 2, 7, &source, &target, 10, 11);
  let current = IndexCheckpointRootV1::new(1, first_checkpoint.key.clone()).unwrap();
  let mut stale = RecordingStore::default();
  stale.selected.insert((owner.index_id().to_vec(), owner.operation_id()), current.clone());
  assert!(matches!(
    publish_index_recovery_checkpoint_v1(
      &mut stale,
      IndexRecoveryPublicationRequestV1 {
        hash_algorithm: ALGORITHM,
        owner: &owner,
        expected: None,
        checkpoint: &second_checkpoint,
        dependencies: &[&journal],
        options: options(),
        memory: &memory(128 * 1_024 * 1_024),
        cancellation: &CancellationToken::new(),
      },
    ),
    Err(IndexRecoveryErrorV1::Invalid(_))
  ));
  assert!(stale.events.iter().all(|event| !event.starts_with("put:")));

  let mut changed = RecordingStore::default();
  changed.artifacts.insert(journal.key.clone(), journal.value.clone());
  changed.artifacts.insert(first_checkpoint.key.clone(), first_checkpoint.value.clone());
  changed.selected.insert((owner.index_id().to_vec(), owner.operation_id()), current);
  changed.change_on_load = Some(first_checkpoint.key.clone());
  let evidence = assert_reconciliation(
    recover_index_checkpoint_v1(&mut changed, ALGORITHM, &owner, options(), &memory(128 * 1_024 * 1_024), &CancellationToken::new())
      .unwrap(),
    IndexRecoveryReasonV1::CheckpointCorrupt,
  );
  assert!(evidence.is_some(), "changed checkpoint bytes lost their decoder evidence");
}

#[test]
fn recovery_runtime_remains_disconnected_until_the_p6_activation_owner_lands() {
  let source = std::fs::read_to_string("src/engine/v4/index_coordinator_recovery.rs").unwrap();
  for forbidden in ["StorageEngine", "DirectoryOps", "V4ControlStore", "V4FirstAuthorityPublisher", "std::fs", "tokio::spawn"] {
    assert!(!source.contains(forbidden), "recovery runtime activated a forbidden dependency: {forbidden}");
  }
}
