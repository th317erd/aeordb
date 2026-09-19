use super::*;

#[derive(Debug)]
struct CorruptAfterRetirementReadback {
  publications: AtomicUsize,
  target_publication: usize,
  saved: Mutex<Option<[u8; DATABASE_HEADER_V4_REGION_LENGTH]>>,
}

impl HeaderPublicationIo for CorruptAfterRetirementReadback {
  fn read_observation(&self, file: &File) -> Result<DatabaseHeaderObservationV4, DatabaseHeaderPublicationErrorV4> {
    observe_database_header_v4(file)
  }

  fn data_barrier(&self, file: &File) -> Result<(), NativeDurabilityError> {
    self.publications.fetch_add(1, AtomicOrdering::SeqCst);
    sync_file_data_native(file)
  }

  fn write_slot(&self, file: &File, slot: usize, bytes: &[u8; DATABASE_HEADER_V4_SLOT_LENGTH]) -> Result<(), NativeDurabilityError> {
    write_file_at_native(file, (slot * DATABASE_HEADER_V4_SLOT_LENGTH) as u64, bytes)
  }

  fn full_barrier(&self, file: &File) -> Result<(), NativeDurabilityError> {
    sync_file_all_native(file)
  }

  fn verify_region(&self, file: &File, expected: &[u8; DATABASE_HEADER_V4_REGION_LENGTH]) -> Result<(), NativeDurabilityError> {
    verify_file_bytes_native(file, 0, expected)?;
    // Simulate a later header read failure only after the caller's exact
    // retirement publication passed native durability and readback.
    if self.publications.load(AtomicOrdering::SeqCst) == self.target_publication {
      *self.saved.lock().unwrap() = Some(*expected);
      write_file_at_native(file, 0, &[0; DATABASE_HEADER_V4_REGION_LENGTH])?;
    }
    Ok(())
  }
}

#[test]
fn mutable_control_observation_after_retirement_preserves_committed_receipt() {
  let (_directory, path, coordinator, mut publisher) = create_environment("mutable-control-retirement-observation", None);
  publisher.publish(&request()).unwrap();
  let index_id = vec![0xc1; HashAlgorithm::Blake3_256.hash_length()];
  let operation_id = [0xc4; 16];
  let identity = index_operation_control_identity(HashAlgorithm::Blake3_256, &index_id, &operation_id).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 * 1024 * 1024, 64 * 1024 * 1024, 1, 8 * 1024 * 1024).unwrap());
  let mut retirement_owner = index_operation_retirement_owner(&cancellation, &memory);
  let mut expected = None;
  for sequence in 1..=2 {
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
  let before = publisher.observe().unwrap();
  let io = Arc::new(CorruptAfterRetirementReadback { publications: AtomicUsize::new(0), target_publication: 3, saved: Mutex::new(None) });
  publisher.header_publisher = DatabaseHeaderPublisherV4::with_io(coordinator, io.clone());
  let encoded = index_operation_control(3, 0xd3);
  let error = publisher
    .publish_mutable_system_control(
      MutableSystemControlPublicationRequestV1 {
        database_id: &[0x31; 16],
        kind: SystemControlKindV1::IndexOperation,
        identity: &identity,
        expected,
        guards: &[],
        encoded_control: &encoded,
        publication_timestamp_ms: 1_700_000_000_203,
        monotonic_now_ms: 10_003,
      },
      &mut retirement_owner,
    )
    .unwrap_err();
  assert_eq!(io.publications.load(AtomicOrdering::SeqCst), 3);
  assert!(publisher.observe().is_err());
  let saved = io.saved.lock().unwrap().take().expect("fault must occur after the retirement header is verified");
  // Restore only the deliberately changed fixture header before inspecting
  // the independently committed task/control and hard-retirement evidence.
  write_file_at_native(&publisher.file, 0, &saved).unwrap();
  sync_file_all_native(&publisher.file).unwrap();
  let selected = publisher.observe().unwrap();
  let hard_sequence = retirement_owner.status().last_hard_publication_sequence;
  assert!(hard_sequence > 0);
  assert_eq!(retirement_owner.status().durable_records, 1);
  let committed = error.committed_receipt().expect("post-retirement observation failure must not erase the committed control receipt");
  assert_eq!(committed.control_sequence, 3);
  assert_eq!(committed.selected_slot, SystemControlSlotV1::A);
  assert!(committed.replaced_slot);
  assert!(!committed.idempotent);
  assert_eq!(committed.retirement_hard_publication_sequence, Some(hard_sequence));
  assert_eq!(committed.observation.selected.header.slot_sequence, before.selected.header.slot_sequence + 2);
  assert_eq!(selected.selected.header.slot_sequence, committed.observation.selected.header.slot_sequence + 1);
  assert_eq!(error.code(), "mutable_control_committed_readback");
  let loaded = publisher.load_mutable_system_control(SystemControlKindV1::IndexOperation, &[0x31; 16], &identity).unwrap().unwrap();
  assert_eq!(loaded.bytes, encoded);
  drop(publisher);
  let (_coordinator, reopened) = reopen(&path);
  assert_eq!(reopened.observe().unwrap(), selected);
  assert_eq!(
    reopened.load_mutable_system_control(SystemControlKindV1::IndexOperation, &[0x31; 16], &identity).unwrap().unwrap().bytes,
    encoded
  );
}

#[test]
fn active_pointer_observation_after_retirement_preserves_committed_receipt_blake3() {
  active_pointer_observation_after_retirement(HashAlgorithm::Blake3_256, "blake3-256");
}

#[test]
fn active_pointer_observation_after_retirement_preserves_committed_receipt_sha512() {
  active_pointer_observation_after_retirement(HashAlgorithm::Sha512, "sha512");
}

fn active_pointer_observation_after_retirement(algorithm: HashAlgorithm, fixture_algorithm: &str) {
  use crate::engine::v4::index_artifact::{ActivePointerKindV1, ActivePointerWriteV1, decode_index_manifest, encode_active_pointer};
  let (_directory, path, coordinator, mut publisher) =
    create_environment_for_algorithm_at_kv_stage("active-pointer-retirement-observation", None, [0x31; 16], algorithm, 0);
  publisher.publish(&request_for_database_and_algorithm([0x31; 16], algorithm)).unwrap();
  let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("spec/fixtures/v4/index-artifact-v1")
    .join(format!("aidx-{fixture_algorithm}-scope-catalog-manifest-empty.bin"));
  let value = fs::read(fixture).unwrap();
  let key = decode_index_manifest(&value, algorithm).unwrap().key;
  let manifest = EncodedImmutableIndexArtifactV1 { key, value };
  let view = decode_index_manifest(&manifest.value, algorithm).unwrap();
  publisher
    .publish_index_artifacts(IndexArtifactBatchPublicationRequestV1 {
      database_id: &[0x31; 16],
      artifacts: &[&manifest],
      publication_timestamp_ms: 1_700_000_000_200,
    })
    .unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 * 1024 * 1024, 64 * 1024 * 1024, 1, 8 * 1024 * 1024).unwrap());
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    [0x31; 16],
    1,
    901,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let pointer = |slot, sequence| {
    encode_active_pointer(&ActivePointerWriteV1 {
      kind: ActivePointerKindV1::ScopeCatalog,
      hash_algorithm: algorithm,
      generation: view.generation,
      owner_id: view.owner_id,
      slot,
      sequence,
      target_manifest_hash: &manifest.key,
    })
    .unwrap()
  };
  for sequence in 1..=2 {
    publisher
      .publish_index_active_pointer(
        IndexActivePointerPublicationRequestV1 {
          database_id: &[0x31; 16],
          pointer: &pointer((sequence - 1) as u8, sequence),
          publication_timestamp_ms: 1_700_000_000_300 + sequence,
          monotonic_now_ms: 10_000 + sequence,
        },
        &mut retirement_owner,
      )
      .unwrap();
  }
  let before = publisher.observe().unwrap();
  let io = Arc::new(CorruptAfterRetirementReadback { publications: AtomicUsize::new(0), target_publication: 2, saved: Mutex::new(None) });
  publisher.header_publisher = DatabaseHeaderPublisherV4::with_io(coordinator, io.clone());
  let replacement = pointer(0, 3);
  let error = publisher
    .publish_index_active_pointer(
      IndexActivePointerPublicationRequestV1 {
        database_id: &[0x31; 16],
        pointer: &replacement,
        publication_timestamp_ms: 1_700_000_000_303,
        monotonic_now_ms: 10_003,
      },
      &mut retirement_owner,
    )
    .unwrap_err();
  assert_eq!(io.publications.load(AtomicOrdering::SeqCst), 2);
  assert!(publisher.observe().is_err());
  let saved = io.saved.lock().unwrap().take().expect("fault must occur after the retirement header is verified");
  write_file_at_native(&publisher.file, 0, &saved).unwrap();
  sync_file_all_native(&publisher.file).unwrap();
  let selected = publisher.observe().unwrap();
  let hard_sequence = retirement_owner.status().last_hard_publication_sequence;
  assert!(hard_sequence > 0);
  assert_eq!(retirement_owner.status().durable_records, 1);
  let committed = error.committed_receipt().expect("post-retirement observation failure must retain the committed active-pointer receipt");
  assert_eq!(committed.pointer_sequence, 3);
  assert_eq!(committed.selected_slot, 0);
  assert_eq!(committed.target_manifest_hash, manifest.key);
  assert!(committed.replaced_slot);
  assert!(!committed.idempotent);
  assert_eq!(committed.retirement_hard_publication_sequence, Some(hard_sequence));
  assert_eq!(committed.observation.selected.header.slot_sequence, before.selected.header.slot_sequence + 1);
  assert_eq!(selected.selected.header.slot_sequence, committed.observation.selected.header.slot_sequence + 1);
  assert_eq!(error.code(), "index_active_pointer_committed_readback");
  let loaded = publisher.load_index_active_pointer_pair(&[0x31; 16], ActivePointerKindV1::ScopeCatalog, view.owner_id).unwrap();
  assert_eq!(loaded.selected.unwrap().bytes, replacement.value);
  drop(publisher);
  let (_coordinator, reopened) = reopen(&path);
  assert_eq!(reopened.observe().unwrap(), selected);
  assert_eq!(
    reopened.load_index_active_pointer_pair(&[0x31; 16], ActivePointerKindV1::ScopeCatalog, view.owner_id).unwrap().selected.unwrap().bytes,
    replacement.value
  );
}
