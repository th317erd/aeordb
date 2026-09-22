//! Deterministic interruption tests sharing the private buffered owner loop.
use super::*;
use crate::engine::memory_coordinator::MemoryPolicy;

struct NoSink;

impl RetirementJournalDurableSinkV1 for NoSink {
  fn publish_synced(
    &mut self,
    _: &PreparedRetirementJournalSegmentV1<'_>,
  ) -> Result<RetirementJournalDurabilityReceiptV1, RetirementJournalSinkErrorV1> {
    panic!("buffered preparation must not invoke the durable sink");
  }
}

fn incarnation(algorithm: HashAlgorithm, key: u8, replacement: bool) -> Vec<u8> {
  let width = algorithm.hash_length();
  let mut bytes = vec![key; width];
  bytes.extend(std::iter::repeat_n(if replacement { 0x60 + key } else { 0x40 + key }, width));
  let offset = 10_000 + u64::from(key) * 1_000 + if replacement { 500 } else { 0 };
  let sequence = u64::from(key) + if replacement { 200 } else { 100 };
  let length: u32 = if replacement { 320 } else { 300 };
  bytes.extend_from_slice(&offset.to_le_bytes());
  bytes.extend_from_slice(&sequence.to_le_bytes());
  bytes.extend_from_slice(&length.to_le_bytes());
  bytes.extend_from_slice(&[2, 1, 0, 0]);
  bytes
}

fn batch<'a>(records: &'a [RetirementJournalReplacementV1<'a>]) -> RetirementJournalReplacementBatchV1<'a> {
  RetirementJournalReplacementBatchV1 { replacement_publication_sequence: 9_000, retired_at_ms: 1_700_000_000_000, replacements: records }
}

fn interrupted_after(cutoff: usize, cancel: bool) {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let cancellation = CancellationToken::new();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(64 << 20, 96 << 20, 1, 8 << 20).unwrap());
    let mut owner = RetirementJournalOwnerV1::new_chain(
      algorithm,
      [0x31; 16],
      1,
      1,
      RetirementJournalBufferOptionsV1::new(1, 1 << 20, 30_000),
      &cancellation,
      &memory,
    )
    .unwrap();
    let pairs: Vec<_> = (1..=4).map(|key| (incarnation(algorithm, key, false), incarnation(algorithm, key, true))).collect();
    let records: Vec<_> = pairs
      .iter()
      .map(|(old, replacement)| RetirementJournalReplacementV1 {
        reason: RetirementReasonV1::PointerOrControlReplace,
        old_incarnation: old,
        replacement_incarnation: replacement,
      })
      .collect();
    let mut sink = NoSink;
    let prefix =
      RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink).prepare_buffered_single(batch(&records[..1]), 10).unwrap();
    let outcome = prefix.activate(|_| -> Result<_, std::io::Error> { Ok(()) }).unwrap();
    assert!(matches!(outcome.journal_state, RetirementJournalActivationJournalStateV1::Buffered));
    let before = owner.soft_state();
    let before_bytes = owner.records.clone();
    let before_status = owner.status();
    let mut observed = Vec::new();
    let result = RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink).prepare_buffered_batch_observed(
      batch(&records[1..]),
      20,
      |completed| {
        observed.push(completed);
        if completed == cutoff {
          if cancel {
            cancellation.cancel();
          } else {
            memory.reconfigure_policy(MemoryPolicy::new(1, 2, 1, 1).unwrap()).unwrap();
          }
        }
      },
    );
    let error = result.expect_err("interruption before returning a permit must roll back the entire incoming suffix");
    assert_eq!(error.code(), if cancel { "retirement_journal_cancelled" } else { "retirement_journal_memory" });
    assert_eq!(error.admitted_records(), 0);
    assert_eq!(observed, (1..=cutoff).collect::<Vec<_>>());
    assert_eq!(owner.soft_state(), before);
    assert_eq!(owner.records, before_bytes);
    assert_eq!(owner.status(), before_status);
    assert!(!owner.failed);
    drop(owner);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn buffered_batch_interruptions_during_append_restore_the_exact_prior_state() {
  for cutoff in [1, 2] {
    for cancel in [false, true] {
      interrupted_after(cutoff, cancel);
    }
  }
}

#[test]
fn buffered_batch_final_cancellation_refuses_without_leaving_new_records() {
  interrupted_after(3, true);
}

#[test]
fn buffered_batch_final_pressure_refuses_without_leaving_new_records() {
  interrupted_after(3, false);
}
