//! Buffered authority admission regression tests.
use super::*;

fn activate_buffered(prepared: aeordb::engine::v4::gc_retirement::PreparedRetirementJournalReplacementV1) {
  let outcome = prepared.activate(|_| -> Result<_, InjectedFailure> { Ok(()) }).unwrap();
  assert!(matches!(outcome.journal_state, RetirementJournalActivationJournalStateV1::Buffered));
}

fn batch_algorithms() -> [HashAlgorithm; 5] {
  [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
}

#[test]
fn authority_buffered_batch_defers_every_sink_call_and_preserves_each_record() {
  for algorithm in batch_algorithms() {
    let cancellation = CancellationToken::new();
    let memory = memory_coordinator();
    let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::new(1, 1 << 20, 30_000), &memory);
    let mut sink = RecordingSink::default();
    let reasons = [
      RetirementReasonV1::StableKeyReplace,
      RetirementReasonV1::Relocation,
      RetirementReasonV1::Repair,
      RetirementReasonV1::Migration,
      RetirementReasonV1::PointerOrControlReplace,
    ];
    let pairs: Vec<_> = reasons.iter().enumerate().map(|(index, reason)| replacement_pair(algorithm, index as u8 + 1, *reason)).collect();
    let records = replacements(&pairs);
    let prepared =
      RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink).prepare_buffered_batch(batch(&records), 10).unwrap();
    assert_eq!(sink.attempts, 0, "the authority-held path must never recursively publish a journal segment");
    assert_eq!(owner.status().pending_records, 5);
    assert_eq!(prepared.permit().replacement_count(), 5);
    for reason in reasons {
      assert_eq!(prepared.permit().reason_count(reason), 1);
    }
    let outcome = prepared.activate(|_| -> Result<_, InjectedFailure> { Ok(5) }).unwrap();
    assert_eq!(outcome.output, 5);
    assert!(matches!(outcome.journal_state, RetirementJournalActivationJournalStateV1::Buffered));
    assert!(owner.flush(&mut sink).unwrap());
    assert_eq!(sink.attempts, 1);
    let segment = decode_retirement_journal_segment_v1(&sink.publications[0], algorithm).unwrap();
    let decoded: Vec<_> = retirement_journal_records_v1(&segment, algorithm).unwrap().map(Result::unwrap).collect();
    assert_eq!(decoded.len(), 5);
    // Literal, independently built incarnation bytes; do not derive expected
    // records by calling the production retirement-record encoder.
    for (record, (old, replacement, reason)) in decoded.iter().zip(&pairs) {
      let incarnation_length = 24 + 2 * algorithm.hash_length();
      assert_eq!(&record.encoded[24..24 + incarnation_length], old);
      assert_eq!(&record.encoded[24 + incarnation_length..], replacement);
      assert_eq!(record.replacement_publication_sequence, 9_000);
      assert_eq!(record.retired_at_ms, 1_700_000_000_000);
      assert_eq!(record.reason, *reason);
    }
    drop(owner);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn authority_buffered_batch_checks_whole_capacity_before_changing_prefix_or_clock() {
  for algorithm in batch_algorithms() {
    let width = algorithm.hash_length();
    // Frozen retirement segment/record widths, not a production sizing helper.
    let exact = 92 + width + 3 * (72 + 4 * width);
    for available in [exact - 1, exact] {
      let cancellation = CancellationToken::new();
      let memory = memory_coordinator();
      let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::new(1, available, 30_000), &memory);
      let mut sink = RecordingSink::default();
      let pairs: Vec<_> =
        (1..=3).map(|ordinal| replacement_pair(algorithm, ordinal, RetirementReasonV1::PointerOrControlReplace)).collect();
      let records = replacements(&pairs);
      let prefix =
        RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink).prepare_buffered_single(batch(&records[..1]), 10).unwrap();
      activate_buffered(prefix);
      let before = owner.status();
      let prepared = RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink).prepare_buffered_batch(batch(&records[1..]), 20);
      if available == exact {
        activate_buffered(prepared.unwrap());
        assert_eq!(owner.status().pending_records, 3);
      } else {
        let error = prepared.unwrap_err();
        assert_eq!(error.admitted_records(), 0);
        assert_eq!(owner.status(), before);
        assert_eq!(sink.attempts, 0);
        // Rejected work must not impose its later clock on a valid retry.
        let retry =
          RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink).prepare_buffered_single(batch(&records[1..2]), 15).unwrap();
        activate_buffered(retry);
        assert_eq!(owner.status().pending_records, 2);
      }
      assert_eq!(sink.attempts, 0);
      owner.flush(&mut sink).unwrap();
      let segment = decode_retirement_journal_segment_v1(&sink.publications[0], algorithm).unwrap();
      let count = retirement_journal_records_v1(&segment, algorithm).unwrap().map(Result::unwrap).count();
      assert_eq!(count, if available == exact { 3 } else { 2 });
    }
  }
}

#[test]
fn authority_buffered_batch_rejects_a_malformed_last_record_without_admitting_the_first() {
  for algorithm in batch_algorithms() {
    let cancellation = CancellationToken::new();
    let memory = memory_coordinator();
    let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::default(), &memory);
    let mut sink = RecordingSink::default();
    let mut pairs: Vec<_> =
      (1..=3).map(|ordinal| replacement_pair(algorithm, ordinal, RetirementReasonV1::PointerOrControlReplace)).collect();
    pairs[2].0.pop();
    let records = replacements(&pairs);
    let before = owner.status();
    let error =
      RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink).prepare_buffered_batch(batch(&records), 20).unwrap_err();
    assert_eq!(error.admitted_records(), 0);
    assert_eq!(owner.status(), before);
    assert_eq!(sink.attempts, 0);
    assert!(!owner.poll(10, &mut sink).unwrap());
  }
}

#[test]
fn authority_buffered_batch_rollback_restores_the_exact_prior_pending_records() {
  for algorithm in batch_algorithms() {
    let cancellation = CancellationToken::new();
    let memory = memory_coordinator();
    let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::new(1, 1 << 20, 30_000), &memory);
    let mut sink = RecordingSink::default();
    let pairs: Vec<_> = (1..=3).map(|ordinal| replacement_pair(algorithm, ordinal, RetirementReasonV1::PointerOrControlReplace)).collect();
    let records = replacements(&pairs);
    let prefix =
      RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink).prepare_buffered_single(batch(&records[..1]), 10).unwrap();
    activate_buffered(prefix);
    let before = owner.status();
    let prepared =
      RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink).prepare_buffered_batch(batch(&records[1..]), 20).unwrap();
    let failure = prepared.activate(|_| -> Result<(), InjectedFailure> { Err(InjectedFailure) }).unwrap_err();
    let (_, prepared) = failure.into_activation_failure().unwrap();
    prepared.discard_buffered(&mut owner).unwrap();
    assert_eq!(owner.status(), before);
    assert_eq!(sink.attempts, 0);
    assert!(!owner.poll(15, &mut sink).unwrap());
    owner.flush(&mut sink).unwrap();
    let segment = decode_retirement_journal_segment_v1(&sink.publications[0], algorithm).unwrap();
    let decoded: Vec<_> = retirement_journal_records_v1(&segment, algorithm).unwrap().map(Result::unwrap).collect();
    assert_eq!(decoded.len(), 1);
    let incarnation_length = 24 + 2 * algorithm.hash_length();
    assert_eq!(&decoded[0].encoded[24..24 + incarnation_length], pairs[0].0);
    assert_eq!(&decoded[0].encoded[24 + incarnation_length..], pairs[0].1);
  }
}

#[test]
fn failed_single_buffer_capacity_admission_does_not_advance_the_owner_clock() {
  let algorithm = HashAlgorithm::Blake3_256;
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let width = algorithm.hash_length();
  let exact_one = 92 + width + 72 + 4 * width;
  let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::new(1, exact_one, 30_000), &memory);
  let mut sink = RecordingSink::default();
  let pairs: Vec<_> = (1..=2).map(|ordinal| replacement_pair(algorithm, ordinal, RetirementReasonV1::PointerOrControlReplace)).collect();
  let records = replacements(&pairs);
  let prefix =
    RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink).prepare_buffered_single(batch(&records[..1]), 10).unwrap();
  activate_buffered(prefix);
  let before = owner.status();
  let error =
    RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink).prepare_buffered_single(batch(&records[1..]), 20).unwrap_err();
  assert_eq!(error.admitted_records(), 0);
  assert_eq!(owner.status(), before);
  assert_eq!(sink.attempts, 0);
  assert!(!owner.poll(15, &mut sink).expect("refused admission must preserve the previous monotonic clock"));
}

#[test]
fn authority_buffered_batch_refusals_preserve_the_prefix_and_chronology() {
  for algorithm in batch_algorithms() {
    for variant in 0..8 {
      let cancellation = CancellationToken::new();
      let memory = memory_coordinator();
      let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::default(), &memory);
      let mut sink = RecordingSink::default();
      let prefix_pair = [replacement_pair(algorithm, 1, RetirementReasonV1::PointerOrControlReplace)];
      let prefix_records = replacements(&prefix_pair);
      let prepared =
        RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink).prepare_buffered_single(batch(&prefix_records), 10).unwrap();
      activate_buffered(prepared);
      let before = owner.status();
      let mut pairs: Vec<_> =
        (2..=3).map(|ordinal| replacement_pair(algorithm, ordinal, RetirementReasonV1::PointerOrControlReplace)).collect();
      match variant {
        0 => pairs.swap(0, 1),
        1 => pairs[1] = pairs[0].clone(),
        2 => pairs[1].1[0] ^= 0x40,
        3 => pairs.clear(),
        _ => {}
      }
      let records = replacements(&pairs);
      let mut request = batch(&records);
      match variant {
        4 => request.replacement_publication_sequence = 8_999,
        5 => request.replacement_publication_sequence = 0,
        6 => request.retired_at_ms = 0,
        _ => {}
      }
      let clock = if variant == 7 { 9 } else { 20 };
      let error = RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink).prepare_buffered_batch(request, clock).unwrap_err();
      assert_eq!(error.admitted_records(), 0, "variant {variant}");
      assert_eq!(owner.status(), before, "variant {variant}");
      assert_eq!(sink.attempts, 0);
      assert!(!owner.poll(15, &mut sink).unwrap(), "variant {variant}");
      owner.flush(&mut sink).unwrap();
      let segment = decode_retirement_journal_segment_v1(&sink.publications[0], algorithm).unwrap();
      let decoded: Vec<_> = retirement_journal_records_v1(&segment, algorithm).unwrap().map(Result::unwrap).collect();
      assert_eq!(decoded.len(), 1);
      let incarnation_length = 24 + 2 * algorithm.hash_length();
      assert_eq!(&decoded[0].encoded[24..24 + incarnation_length], prefix_pair[0].0);
    }
  }
}

#[test]
fn authority_buffered_batch_cancellation_and_pressure_refuse_without_partial_admission() {
  for algorithm in batch_algorithms() {
    for canceled in [false, true] {
      let cancellation = CancellationToken::new();
      let memory = memory_coordinator();
      let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::default(), &memory);
      let mut sink = RecordingSink::default();
      let pairs: Vec<_> =
        (1..=3).map(|ordinal| replacement_pair(algorithm, ordinal, RetirementReasonV1::PointerOrControlReplace)).collect();
      let records = replacements(&pairs);
      let prepared =
        RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink).prepare_buffered_single(batch(&records[..1]), 10).unwrap();
      activate_buffered(prepared);
      let before = owner.status();
      if canceled {
        cancellation.cancel();
      } else {
        memory.reconfigure_policy(MemoryPolicy::new(1, 2, 1, 1).unwrap()).unwrap();
      }
      let error =
        RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink).prepare_buffered_batch(batch(&records[1..]), 20).unwrap_err();
      assert_eq!(error.code(), if canceled { "retirement_journal_cancelled" } else { "retirement_journal_memory" });
      assert_eq!(error.admitted_records(), 0);
      assert_eq!(owner.status(), before);
      assert_eq!(sink.attempts, 0);
      if !canceled {
        memory.reconfigure_policy(MemoryPolicy::new(64 * 1024 * 1024, 96 * 1024 * 1024, 1, 8 * 1024 * 1024).unwrap()).unwrap();
        assert!(!owner.poll(15, &mut sink).unwrap());
        owner.flush(&mut sink).unwrap();
        let segment = decode_retirement_journal_segment_v1(&sink.publications[0], algorithm).unwrap();
        assert_eq!(retirement_journal_records_v1(&segment, algorithm).unwrap().map(Result::unwrap).count(), 1);
      }
      drop(owner);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    }
  }
}

#[test]
fn authority_buffered_batch_discard_is_bound_to_the_exact_owner_and_pending_suffix() {
  for algorithm in batch_algorithms() {
    let cancellation = CancellationToken::new();
    let memory = memory_coordinator();
    let options = RetirementJournalBufferOptionsV1::default();
    let mut first_owner = owner(algorithm, &cancellation, options, &memory);
    let mut other_owner = owner(algorithm, &cancellation, options, &memory);
    let mut sink = RecordingSink::default();
    let pairs: Vec<_> = (1..=3).map(|ordinal| replacement_pair(algorithm, ordinal, RetirementReasonV1::PointerOrControlReplace)).collect();
    let records = replacements(&pairs);
    let prepared =
      RetirementJournalReplacementCoordinatorV1::new(&mut first_owner, &mut sink).prepare_buffered_batch(batch(&records[..2]), 10).unwrap();
    let wrong_owner = prepared.discard_buffered(&mut other_owner).unwrap_err();
    assert_eq!(wrong_owner.code(), "retirement_journal_buffered_rollback_owner");
    assert!(!other_owner.status().failed);
    assert_eq!(other_owner.status().pending_records, 0);
    let (_, prepared) = wrong_owner.into_parts();
    (*prepared).discard_buffered(&mut first_owner).unwrap();
    assert_eq!(first_owner.status().pending_records, 0);
    assert!(!first_owner.poll(5, &mut sink).unwrap());
    let older =
      RetirementJournalReplacementCoordinatorV1::new(&mut first_owner, &mut sink).prepare_buffered_batch(batch(&records[..2]), 10).unwrap();
    let newer = RetirementJournalReplacementCoordinatorV1::new(&mut first_owner, &mut sink)
      .prepare_buffered_single(batch(&records[2..]), 10)
      .unwrap();
    let changed_owner = older.discard_buffered(&mut first_owner).unwrap_err();
    assert_eq!(changed_owner.code(), "retirement_journal_buffered_rollback_state");
    assert!(first_owner.status().failed);
    assert_eq!(first_owner.status().pending_records, 3);
    assert_eq!(sink.attempts, 0);
    drop(newer);
  }
}
