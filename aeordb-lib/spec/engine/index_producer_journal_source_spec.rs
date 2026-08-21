use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::index_producer_journal_source::{
  IndexProducerJournalReadErrorClassV1, IndexProducerJournalReadRequestV1, IndexProducerJournalReadV1,
};
use aeordb::engine::v4::index_task::{
  JournalOwnerKindV1, MutationJournalWriteV1, MutationKindV1, MutationRecordWriteV1, MutationSideWriteV1, decode_mutation_journal,
  encode_mutation_journal,
};

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(6 * 1_024 * 1_024, 8 * 1_024 * 1_024, 1, 1 * 1_024 * 1_024).unwrap())
}

fn encoded_journal(discriminator: u8) -> Vec<u8> {
  let previous = [0; 32];
  let semantic = [0x31; 32];
  let mutation = [discriminator; 32];
  let before = [0x41; 32];
  let after = [0x42; 32];
  let revision = [0x43; 32];
  encode_mutation_journal(&MutationJournalWriteV1 {
    hash_algorithm: ALGORITHM,
    owner_id: [0x21; 16],
    owner_kind: JournalOwnerKindV1::Task,
    generation: 1,
    segment_ordinal: 0,
    chain_reset: true,
    previous_segment: &previous,
    semantic_state_root: &semantic,
    runtime_boot_id: [0x22; 16],
    records: &[MutationRecordWriteV1 {
      kind: MutationKindV1::Create,
      sequence: 7,
      mutation_id: &mutation,
      batch_ordinal: 0,
      batch_count: 1,
      root_before: &before,
      root_after: &after,
      before: None,
      after: Some(MutationSideWriteV1 { path: "/doc.json", revision: &revision }),
      committed_at_ms: 100,
    }],
  })
  .unwrap()
  .value
}

fn head(encoded: &[u8]) -> Vec<u8> {
  decode_mutation_journal(encoded, ALGORITHM).unwrap().key.to_vec()
}

fn reserved(memory: &MemoryCoordinator, owner: MemoryOwner) -> u64 {
  memory.snapshot().unwrap().owner(owner).map_or(0, |state| state.reserved_bytes)
}

#[test]
fn exact_journal_read_retains_task_memory_until_drop() {
  let memory = memory();
  let encoded = encoded_journal(0x51);
  let journal_head = head(&encoded);
  let request = IndexProducerJournalReadRequestV1 { hash_algorithm: ALGORITHM, journal_head: &journal_head, is_cancelled: &|| false };
  let reservation = memory.reserve(MemoryOwner::Task, 1 * 1_024 * 1_024, AdmissionClass::Workload).unwrap();

  let read = IndexProducerJournalReadV1::new(&request, encoded.clone(), reservation).unwrap();

  assert_eq!(read.encoded(), encoded);
  assert_eq!(read.decode_journal(ALGORITHM, &journal_head).unwrap().key, journal_head);
  assert_eq!(read.decode_journal(ALGORITHM, &[0x11; 31]).err().unwrap().code(), "journal_request_identity");
  assert_eq!(read.reserved_bytes(), 1 * 1_024 * 1_024);
  assert_eq!(reserved(&memory, MemoryOwner::Task), 1 * 1_024 * 1_024);
  drop(read);
  assert_eq!(reserved(&memory, MemoryOwner::Task), 0);
}

#[test]
fn journal_read_rejects_cancellation_and_absent_or_wrong_width_identity() {
  let encoded = encoded_journal(0x52);
  let journal_head = head(&encoded);
  for (candidate, cancelled, expected_code) in [
    (journal_head.clone(), true, "journal_cancelled"),
    (vec![0; 32], false, "journal_request_identity"),
    (vec![0x11; 31], false, "journal_request_identity"),
  ] {
    let memory = memory();
    let request = IndexProducerJournalReadRequestV1 { hash_algorithm: ALGORITHM, journal_head: &candidate, is_cancelled: &|| cancelled };
    let reservation = memory.reserve(MemoryOwner::Task, 1 * 1_024 * 1_024, AdmissionClass::Workload).unwrap();
    let error = IndexProducerJournalReadV1::new(&request, encoded.clone(), reservation).err().unwrap();
    assert_eq!(error.code(), expected_code);
    assert_eq!(reserved(&memory, MemoryOwner::Task), 0);
  }
}

#[test]
fn journal_read_rejects_wrong_memory_owner_and_undersized_reservation() {
  let encoded = encoded_journal(0x53);
  let journal_head = head(&encoded);
  let request = IndexProducerJournalReadRequestV1 { hash_algorithm: ALGORITHM, journal_head: &journal_head, is_cancelled: &|| false };

  let wrong_owner_memory = memory();
  let wrong_owner = wrong_owner_memory.reserve(MemoryOwner::Query, 1 * 1_024 * 1_024, AdmissionClass::Workload).unwrap();
  let error = IndexProducerJournalReadV1::new(&request, encoded.clone(), wrong_owner).err().unwrap();
  assert_eq!(error.code(), "journal_memory_owner");
  assert_eq!(reserved(&wrong_owner_memory, MemoryOwner::Query), 0);

  let short_memory = memory();
  let short = short_memory.reserve(MemoryOwner::Task, 1, AdmissionClass::Workload).unwrap();
  let error = IndexProducerJournalReadV1::new(&request, encoded, short).err().unwrap();
  assert_eq!(error.code(), "journal_memory_reservation");
  assert_eq!(reserved(&short_memory, MemoryOwner::Task), 0);
}

#[test]
fn journal_read_rejects_malformed_and_wrong_identity_content() {
  let expected = encoded_journal(0x54);
  let expected_head = head(&expected);
  let request = IndexProducerJournalReadRequestV1 { hash_algorithm: ALGORITHM, journal_head: &expected_head, is_cancelled: &|| false };

  for (encoded, expected_code) in [(vec![0x99; 128], "journal_format"), (encoded_journal(0x55), "journal_identity")] {
    let memory = memory();
    let reservation = memory.reserve(MemoryOwner::Task, 1 * 1_024 * 1_024, AdmissionClass::Workload).unwrap();
    let read = IndexProducerJournalReadV1::new(&request, encoded, reservation).unwrap();
    let error = read.decode_journal(ALGORITHM, &expected_head).err().unwrap();
    assert_eq!(error.class(), IndexProducerJournalReadErrorClassV1::Corrupt);
    assert_eq!(error.code(), expected_code);
    drop(read);
    assert_eq!(reserved(&memory, MemoryOwner::Task), 0);
  }
}
