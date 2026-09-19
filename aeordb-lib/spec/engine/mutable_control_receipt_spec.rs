use super::*;

#[test]
fn mutable_control_readback_failure_preserves_committed_receipt() {
  let (_directory, path, _coordinator, publisher) = create_environment("mutable-control-readback-receipt", None);
  publisher.publish(&request()).unwrap();
  let before = publisher.observe().unwrap();
  let index_id = vec![0xc1; HashAlgorithm::Blake3_256.hash_length()];
  let operation_id = [0xc4; 16];
  let identity = index_operation_control_identity(HashAlgorithm::Blake3_256, &index_id, &operation_id).unwrap();
  let encoded = index_operation_control(1, 0xd1);
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 * 1024 * 1024, 64 * 1024 * 1024, 1, 8 * 1024 * 1024).unwrap());
  let mut retirement_owner = index_operation_retirement_owner(&cancellation, &memory);
  let mut observer = TruncatingPostCommitObserver { file: fs::OpenOptions::new().write(true).open(&path).unwrap() };
  let error = publisher
    .publish_mutable_system_control_with_observer(
      MutableSystemControlPublicationRequestV1 {
        database_id: &[0x31; 16],
        kind: SystemControlKindV1::IndexOperation,
        identity: &identity,
        expected: None,
        guards: &[],
        encoded_control: &encoded,
        publication_timestamp_ms: 1_700_000_000_201,
        monotonic_now_ms: 10_001,
      },
      None,
      &mut retirement_owner,
      &mut observer,
    )
    .unwrap_err();
  let selected = publisher.observe().unwrap();
  assert!(selected.selected.header.slot_sequence > before.selected.header.slot_sequence);
  assert!(fs::metadata(&path).unwrap().len() < selected.selected.header.hot_tail_offset);
  let committed = error.committed_receipt().expect("readback failure happened after authority committed; its receipt must survive");
  assert_eq!(committed.observation, selected);
  assert_eq!(committed.selected_slot, SystemControlSlotV1::A);
  assert_eq!(committed.control_sequence, 1);
  assert_eq!(committed.control_digest, mutable_system_control_digest(HashAlgorithm::Blake3_256, &encoded));
  assert!(!committed.replaced_slot);
  assert!(!committed.idempotent);
  assert_eq!(error.code(), "mutable_control_committed_readback");
}

#[test]
fn index_operation_readback_failure_preserves_committed_receipt() {
  let (_directory, path, _coordinator, publisher) = create_environment("index-control-readback-receipt", None);
  publisher.publish(&request()).unwrap();
  let before = publisher.observe().unwrap();
  let index_id = vec![0xc1; HashAlgorithm::Blake3_256.hash_length()];
  let operation_id = [0xc4; 16];
  let encoded = index_operation_control(1, 0xd1);
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 * 1024 * 1024, 64 * 1024 * 1024, 1, 8 * 1024 * 1024).unwrap());
  let mut retirement_owner = index_operation_retirement_owner(&cancellation, &memory);
  let mut observer = TruncatingPostCommitObserver { file: fs::OpenOptions::new().write(true).open(&path).unwrap() };
  let error = publisher
    .publish_index_operation_control_with_observer(
      IndexOperationControlPublicationRequestV1 {
        database_id: &[0x31; 16],
        index_id: &index_id,
        operation_id: &operation_id,
        expected: None,
        encoded_control: &encoded,
        publication_timestamp_ms: 1_700_000_000_201,
        monotonic_now_ms: 10_001,
      },
      &mut retirement_owner,
      &mut observer,
    )
    .unwrap_err();
  let selected = publisher.observe().unwrap();
  assert!(selected.selected.header.slot_sequence > before.selected.header.slot_sequence);
  assert!(fs::metadata(&path).unwrap().len() < selected.selected.header.hot_tail_offset);
  let committed = error.committed_receipt().expect("index adapter must preserve the committed receipt after readback failure");
  assert_eq!(committed.observation, selected);
  assert_eq!(committed.selected_slot, SystemControlSlotV1::A);
  assert_eq!(committed.control_sequence, 1);
  assert_eq!(committed.checkpoint_artifact, vec![0xd1; HashAlgorithm::Blake3_256.hash_length()]);
  assert!(!committed.replaced_slot);
  assert!(!committed.idempotent);
  assert_eq!(error.code(), "index_operation_committed_readback");
}

struct CorruptControlAfterCommit {
  file: File,
  saved: Option<(u64, Vec<u8>)>,
  return_error: bool,
}

impl FirstAuthorityDependencyObserverV1 for CorruptControlAfterCommit {
  fn staged(&mut self, _kv: &DiskKVStore, _entities: &[PreparedWholeEntityV1]) -> Result<(), NativeDurabilityError> {
    Ok(())
  }

  fn authority_committed(&mut self, kv: &DiskKVStore, entities: &[PreparedWholeEntityV1]) -> Result<(), FirstAuthorityPublicationErrorV1> {
    let entity = entities.last().expect("control publication includes its FileRecord");
    assert_eq!(entity.kv_type, kv_tag::FILE_RECORD);
    let locator = kv.get(&entity.key)?.unwrap();
    let mut damaged = entity.bytes.clone();
    *damaged.last_mut().unwrap() ^= 1;
    write_file_at_native(&self.file, locator.offset, &damaged).unwrap();
    self.saved = Some((locator.offset, entity.bytes.clone()));
    if self.return_error {
      Err(FirstAuthorityPublicationErrorV1::invalid("receipt_test_observer_failure", "original post-commit observer cause"))
    } else {
      Ok(())
    }
  }
}

#[test]
fn mutable_control_readback_error_keeps_retirement_priority_and_exact_restart_retry() {
  for replaced_slot in [false, true] {
    for original_failure in [false, true] {
      let (_directory, path, _coordinator, publisher) = create_environment("mutable-control-receipt-retry", None);
      publisher.publish(&request()).unwrap();
      let algorithm = HashAlgorithm::Blake3_256;
      let identity = index_operation_control_identity(algorithm, &vec![0xc1; algorithm.hash_length()], &[0xc4; 16]).unwrap();
      let cancellation = CancellationToken::new();
      let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
      let mut retirement_owner = index_operation_retirement_owner(&cancellation, &memory);
      let mut expected = None;
      let next_sequence = if replaced_slot { 3 } else { 1 };
      for sequence in 1..next_sequence {
        let encoded = index_operation_control(sequence, 0xd0 + sequence as u8);
        let receipt = publisher
          .publish_mutable_system_control(
            MutableSystemControlPublicationRequestV1 {
              database_id: &[0x31; 16],
              kind: SystemControlKindV1::IndexOperation,
              identity: &identity,
              expected,
              guards: &[],
              encoded_control: &encoded,
              publication_timestamp_ms: 1_700_000_000_200 + sequence,
              monotonic_now_ms: 10_000 + sequence,
            },
            &mut retirement_owner,
          )
          .unwrap();
        expected = Some(MutableSystemControlExpectationV1 {
          selected_slot: receipt.selected_slot,
          control_sequence: receipt.control_sequence,
          control_digest: receipt.control_digest,
        });
      }
      let encoded = index_operation_control(next_sequence, 0xd0 + next_sequence as u8);
      let mut observer = CorruptControlAfterCommit {
        file: fs::OpenOptions::new().write(true).open(&path).unwrap(),
        saved: None,
        return_error: original_failure,
      };
      let error = publisher
        .publish_mutable_system_control_with_observer(
          MutableSystemControlPublicationRequestV1 {
            database_id: &[0x31; 16],
            kind: SystemControlKindV1::IndexOperation,
            identity: &identity,
            expected,
            guards: &[],
            encoded_control: &encoded,
            publication_timestamp_ms: 1_700_000_000_200 + next_sequence,
            monotonic_now_ms: 10_000 + next_sequence,
          },
          None,
          &mut retirement_owner,
          &mut observer,
        )
        .unwrap_err();
      let (offset, bytes) = observer.saved.take().expect("fault must happen after authority publication");
      write_file_at_native(&observer.file, offset, &bytes).unwrap();
      sync_file_all_native(&observer.file).unwrap();
      let committed = error.committed_receipt().expect("control readback failure must retain the actual committed authority");
      assert_eq!(committed.control_sequence, next_sequence);
      assert_eq!(committed.replaced_slot, replaced_slot);
      assert_eq!(committed.selected_slot, SystemControlSlotV1::A);
      assert_eq!(committed.control_digest, mutable_system_control_digest(algorithm, &encoded));
      assert_eq!(committed.observation, publisher.observe().unwrap());
      assert_eq!(committed.retirement_hard_publication_sequence.is_some(), replaced_slot);
      assert_eq!(retirement_owner.status().durable_records, u64::from(replaced_slot));
      assert_eq!(
        error.code(),
        if original_failure { "mutable_control_committed_postcondition_failure" } else { "mutable_control_committed_readback" }
      );
      if original_failure {
        assert!(error.to_string().contains("original post-commit observer cause"));
      }
      let selected = committed.observation.clone();
      drop(publisher);
      let (_coordinator, reopened) = reopen(&path);
      assert_eq!(reopened.observe().unwrap(), selected);
      // Reopen replay may retain buffered KV metadata. Establish the public
      // publisher's flushed baseline before measuring exact retry bytes.
      reopened.lock_kv().unwrap().flush().unwrap();
      let before_retry = fs::read(&path).unwrap();
      let retry = reopened
        .publish_mutable_system_control(
          MutableSystemControlPublicationRequestV1 {
            database_id: &[0x31; 16],
            kind: SystemControlKindV1::IndexOperation,
            identity: &identity,
            expected: None,
            guards: &[],
            encoded_control: &encoded,
            publication_timestamp_ms: 1_700_000_000_210,
            monotonic_now_ms: 10_010,
          },
          &mut retirement_owner,
        )
        .unwrap();
      assert!(retry.idempotent);
      assert_eq!(retry.control_sequence, next_sequence);
      assert_eq!(retry.observation, selected);
      assert_eq!(fs::read(&path).unwrap(), before_retry);
    }
  }
}
