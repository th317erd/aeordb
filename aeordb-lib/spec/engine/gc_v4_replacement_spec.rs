use std::cell::Cell;
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::gc_retirement::{
  PreparedRetirementJournalSegmentV1, RetirementJournalActivationJournalStateV1, RetirementJournalBufferOptionsV1,
  RetirementJournalDurabilityReceiptV1, RetirementJournalDurableSinkV1, RetirementJournalOwnerV1, RetirementJournalReplacementBatchV1,
  RetirementJournalReplacementCoordinatorV1, RetirementJournalReplacementV1, RetirementJournalSinkErrorV1,
  retirement_journal_replacement_batch_digest_v1,
};
use aeordb::engine::v4::gc_state::{RetirementReasonV1, decode_retirement_journal_segment_v1, retirement_journal_records_v1};
use tokio_util::sync::CancellationToken;

fn memory_coordinator() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(64 * 1024 * 1024, 96 * 1024 * 1024, 1, 8 * 1024 * 1024).unwrap())
}

fn owner<'a>(
  algorithm: HashAlgorithm,
  cancellation: &'a CancellationToken,
  options: RetirementJournalBufferOptionsV1,
  memory: &MemoryCoordinator,
) -> RetirementJournalOwnerV1<'a> {
  RetirementJournalOwnerV1::new_chain(algorithm, [0x31; 16], 1, 1, options, cancellation, memory).unwrap()
}

fn physical_incarnation(
  algorithm: HashAlgorithm,
  logical_key_byte: u8,
  digest_byte: u8,
  wal_offset: u64,
  write_sequence: u64,
  entity_length: u32,
  entry_type: u8,
  entity_version: u8,
) -> Vec<u8> {
  let hash_width = algorithm.hash_length();
  let mut bytes = Vec::with_capacity(24 + 2 * hash_width);
  bytes.extend_from_slice(&vec![logical_key_byte; hash_width]);
  bytes.extend_from_slice(&vec![digest_byte; hash_width]);
  bytes.extend_from_slice(&wal_offset.to_le_bytes());
  bytes.extend_from_slice(&write_sequence.to_le_bytes());
  bytes.extend_from_slice(&entity_length.to_le_bytes());
  bytes.push(entry_type);
  bytes.push(entity_version);
  bytes.extend_from_slice(&[0, 0]);
  bytes
}

fn replacement_pair(algorithm: HashAlgorithm, ordinal: u8, reason: RetirementReasonV1) -> (Vec<u8>, Vec<u8>, RetirementReasonV1) {
  let old_offset = 10_000 + u64::from(ordinal) * 1_000;
  let old = physical_incarnation(algorithm, ordinal, 0x40 + ordinal, old_offset, 100 + u64::from(ordinal), 300, 2, 1);
  let replacement = physical_incarnation(algorithm, ordinal, 0x60 + ordinal, old_offset + 500, 200 + u64::from(ordinal), 320, 2, 1);
  (old, replacement, reason)
}

fn replacements<'a>(pairs: &'a [(Vec<u8>, Vec<u8>, RetirementReasonV1)]) -> Vec<RetirementJournalReplacementV1<'a>> {
  pairs
    .iter()
    .map(|(old, replacement, reason)| RetirementJournalReplacementV1 {
      reason: *reason,
      old_incarnation: old,
      replacement_incarnation: replacement,
    })
    .collect()
}

fn batch<'a>(replacements: &'a [RetirementJournalReplacementV1<'a>]) -> RetirementJournalReplacementBatchV1<'a> {
  RetirementJournalReplacementBatchV1 { replacement_publication_sequence: 9_000, retired_at_ms: 1_700_000_000_000, replacements }
}

#[derive(Debug)]
struct InjectedFailure;

impl Display for InjectedFailure {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter.write_str("injected replacement test failure")
  }
}

impl Error for InjectedFailure {}

#[derive(Default)]
struct RecordingSink {
  attempts: usize,
  publications: Vec<Vec<u8>>,
  publication_count: Rc<Cell<usize>>,
  fail_attempt: Option<usize>,
  wrong_receipt: bool,
}

impl RetirementJournalDurableSinkV1 for RecordingSink {
  fn publish_synced(
    &mut self,
    segment: &PreparedRetirementJournalSegmentV1<'_>,
  ) -> Result<RetirementJournalDurabilityReceiptV1, RetirementJournalSinkErrorV1> {
    self.attempts += 1;
    if self.fail_attempt == Some(self.attempts) {
      return Err(RetirementJournalSinkErrorV1::new("replacement_test_sink", InjectedFailure));
    }
    self.publications.push(segment.value.to_vec());
    self.publication_count.set(self.publications.len());
    Ok(RetirementJournalDurabilityReceiptV1 {
      artifact_key: if self.wrong_receipt { vec![0xA5; segment.artifact_key.len()] } else { segment.artifact_key.to_vec() },
      stored_value_length: segment.value.len() as u32,
      hard_publication_sequence: self.attempts as u64,
    })
  }
}

#[test]
fn all_five_replacement_reasons_enter_one_owner_before_activation_at_both_hash_widths() {
  let reasons = [
    RetirementReasonV1::StableKeyReplace,
    RetirementReasonV1::Relocation,
    RetirementReasonV1::Repair,
    RetirementReasonV1::Migration,
    RetirementReasonV1::PointerOrControlReplace,
  ];
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let cancellation = CancellationToken::new();
    let memory = memory_coordinator();
    let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::new(5, 1024 * 1024, 30_000), &memory);
    let mut sink = RecordingSink::default();
    let pairs: Vec<_> = reasons.iter().enumerate().map(|(index, reason)| replacement_pair(algorithm, index as u8 + 1, *reason)).collect();
    let records = replacements(&pairs);
    let activated = Cell::new(false);
    let publication_count = Rc::clone(&sink.publication_count);
    let expected_batch_digest = retirement_journal_replacement_batch_digest_v1(&batch(&records), algorithm).unwrap();

    let result = RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink)
      .execute(batch(&records), 10, |permit| -> Result<_, InjectedFailure> {
        assert_eq!(publication_count.get(), 1, "journal publication must precede activation");
        assert_eq!(permit.hash_algorithm(), algorithm);
        assert_eq!(permit.batch_digest(), expected_batch_digest);
        assert_eq!(permit.replacement_count(), 5);
        for reason in reasons {
          assert_eq!(permit.reason_count(reason), 1);
        }
        activated.set(true);
        Ok("activated")
      })
      .unwrap();

    assert!(activated.get());
    assert_eq!(result.output, "activated");
    assert_eq!(result.journal_state.code(), "hard_published");
    let segment = decode_retirement_journal_segment_v1(&sink.publications[0], algorithm).unwrap();
    let observed: Vec<_> = retirement_journal_records_v1(&segment, algorithm).unwrap().map(|record| record.unwrap().reason).collect();
    assert_eq!(observed, reasons);
  }
}

#[test]
fn activation_permit_digest_binds_reason_and_both_incarnation_identities() {
  let algorithm = HashAlgorithm::Blake3_256;
  let base = [replacement_pair(algorithm, 1, RetirementReasonV1::StableKeyReplace)];
  let base_records = replacements(&base);
  let expected = retirement_journal_replacement_batch_digest_v1(&batch(&base_records), algorithm).unwrap();

  let mut changed_reason = base.clone();
  changed_reason[0].2 = RetirementReasonV1::Repair;
  let changed_reason_records = replacements(&changed_reason);
  assert_ne!(retirement_journal_replacement_batch_digest_v1(&batch(&changed_reason_records), algorithm).unwrap(), expected);

  let mut changed_old = base.clone();
  changed_old[0].0[0] ^= 1;
  let changed_old_records = replacements(&changed_old);
  assert_ne!(retirement_journal_replacement_batch_digest_v1(&batch(&changed_old_records), algorithm).unwrap(), expected);

  let mut changed_replacement = base;
  changed_replacement[0].1[algorithm.hash_length()] ^= 1;
  let changed_replacement_records = replacements(&changed_replacement);
  assert_ne!(retirement_journal_replacement_batch_digest_v1(&batch(&changed_replacement_records), algorithm).unwrap(), expected);
}

#[test]
fn buffered_admission_can_activate_without_a_soft_journal_sync() {
  let algorithm = HashAlgorithm::Blake3_256;
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::new(10, 1024 * 1024, 30_000), &memory);
  let mut sink = RecordingSink::default();
  let pairs = [replacement_pair(algorithm, 1, RetirementReasonV1::StableKeyReplace)];
  let records = replacements(&pairs);

  let result = RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink)
    .execute(batch(&records), 10, |_| -> Result<_, InjectedFailure> { Ok(7) })
    .unwrap();

  assert_eq!(result.output, 7);
  assert!(matches!(result.journal_state, RetirementJournalActivationJournalStateV1::Buffered));
  assert_eq!(sink.attempts, 0);
  assert_eq!(owner.status().pending_records, 1);
}

#[test]
fn authority_critical_single_replacement_buffers_even_when_the_normal_threshold_is_one() {
  let algorithm = HashAlgorithm::Blake3_256;
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000), &memory);
  let mut sink = RecordingSink::default();
  let pairs = [replacement_pair(algorithm, 1, RetirementReasonV1::PointerOrControlReplace)];
  let records = replacements(&pairs);

  let prepared =
    RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink).prepare_buffered_single(batch(&records), 10).unwrap();
  assert_eq!(sink.attempts, 0, "buffered admission must not recurse into the authority sink");
  assert_eq!(owner.status().pending_records, 1);
  let outcome = prepared.activate(|permit| -> Result<_, InjectedFailure> {
    assert_eq!(permit.reason_count(RetirementReasonV1::PointerOrControlReplace), 1);
    Ok("activated")
  });
  assert_eq!(outcome.unwrap().output, "activated");

  assert!(owner.flush(&mut sink).unwrap());
  assert_eq!(sink.attempts, 1);
  assert_eq!(owner.status().pending_records, 0);
}

#[test]
fn failed_buffered_activation_can_discard_only_its_exact_soft_record() {
  let algorithm = HashAlgorithm::Blake3_256;
  let pairs = [replacement_pair(algorithm, 1, RetirementReasonV1::PointerOrControlReplace)];
  let records = replacements(&pairs);
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::new(1, 1_048_576, 30_000), &memory);
  let mut sink = RecordingSink::default();
  let before = owner.status();
  let prepared =
    RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink).prepare_buffered_single(batch(&records), 10).unwrap();
  let error = prepared.activate(|_| -> Result<(), InjectedFailure> { Err(InjectedFailure) }).unwrap_err();
  let (_source, prepared) = error.into_activation_failure().unwrap();

  prepared.discard_buffered(&mut owner).unwrap();

  assert_eq!(owner.status(), before);
  assert_eq!(sink.attempts, 0);
}

#[test]
fn buffered_discard_refuses_wrong_or_changed_owners_without_guessing() {
  let algorithm = HashAlgorithm::Blake3_256;
  let pairs = [
    replacement_pair(algorithm, 1, RetirementReasonV1::PointerOrControlReplace),
    replacement_pair(algorithm, 2, RetirementReasonV1::PointerOrControlReplace),
  ];
  let records = replacements(&pairs);
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let options = RetirementJournalBufferOptionsV1::new(4, 1_048_576, 30_000);
  let mut first_owner = owner(algorithm, &cancellation, options, &memory);
  let mut other_owner = owner(algorithm, &cancellation, options, &memory);
  let mut sink = RecordingSink::default();
  let wrong_owner_prepared =
    RetirementJournalReplacementCoordinatorV1::new(&mut first_owner, &mut sink).prepare_buffered_single(batch(&records[..1]), 10).unwrap();

  let error = wrong_owner_prepared.discard_buffered(&mut other_owner).unwrap_err();

  assert_eq!(error.code(), "retirement_journal_buffered_rollback_owner");
  assert!(!other_owner.status().failed);
  let (_source, wrong_owner_prepared) = error.into_parts();
  (*wrong_owner_prepared).discard_buffered(&mut first_owner).unwrap();
  assert_eq!(first_owner.status().pending_records, 0);

  let mut changed_owner = owner(algorithm, &cancellation, options, &memory);
  let older = RetirementJournalReplacementCoordinatorV1::new(&mut changed_owner, &mut sink)
    .prepare_buffered_single(batch(&records[..1]), 10)
    .unwrap();
  let newer = RetirementJournalReplacementCoordinatorV1::new(&mut changed_owner, &mut sink)
    .prepare_buffered_single(batch(&records[1..]), 10)
    .unwrap();
  let error = older.discard_buffered(&mut changed_owner).unwrap_err();
  assert_eq!(error.code(), "retirement_journal_buffered_rollback_state");
  assert!(changed_owner.status().failed);
  drop(newer);
}

#[test]
fn buffered_authority_admission_rejects_plural_batches_without_partial_owner_mutation() {
  let algorithm = HashAlgorithm::Blake3_256;
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::default(), &memory);
  let mut sink = RecordingSink::default();
  let pairs = [
    replacement_pair(algorithm, 1, RetirementReasonV1::PointerOrControlReplace),
    replacement_pair(algorithm, 2, RetirementReasonV1::PointerOrControlReplace),
  ];
  let records = replacements(&pairs);

  let error =
    RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink).prepare_buffered_single(batch(&records), 10).unwrap_err();
  assert_eq!(error.code(), "retirement_replacement_preflight");
  assert_eq!(error.admitted_records(), 0);
  assert_eq!(owner.status().pending_records, 0);
  assert_eq!(sink.attempts, 0);
}

#[test]
fn a_final_retryable_sink_failure_retains_evidence_and_surfaces_the_deferred_failure() {
  let algorithm = HashAlgorithm::Blake3_256;
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000), &memory);
  let mut sink = RecordingSink { fail_attempt: Some(1), ..RecordingSink::default() };
  let pairs = [replacement_pair(algorithm, 1, RetirementReasonV1::StableKeyReplace)];
  let records = replacements(&pairs);

  let result = RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink)
    .execute(batch(&records), 10, |_| -> Result<_, InjectedFailure> { Ok(()) })
    .unwrap();

  assert_eq!(result.journal_state.code(), "buffered_after_sink_failure");
  assert_eq!(result.journal_state.deferred_sink_error().unwrap().code(), "replacement_test_sink");
  assert_eq!(owner.status().pending_records, 1);
  assert!(!owner.status().failed);
}

#[test]
fn a_partial_batch_or_dishonest_receipt_never_reaches_activation() {
  let algorithm = HashAlgorithm::Blake3_256;
  for wrong_receipt in [false, true] {
    let cancellation = CancellationToken::new();
    let memory = memory_coordinator();
    let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000), &memory);
    let mut sink = if wrong_receipt {
      RecordingSink { wrong_receipt: true, ..RecordingSink::default() }
    } else {
      RecordingSink { fail_attempt: Some(1), ..RecordingSink::default() }
    };
    let count = if wrong_receipt { 1 } else { 2 };
    let pairs: Vec<_> = (1..=count).map(|ordinal| replacement_pair(algorithm, ordinal, RetirementReasonV1::StableKeyReplace)).collect();
    let records = replacements(&pairs);
    let activated = Cell::new(false);

    let error = RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink)
      .execute(batch(&records), 10, |_| -> Result<(), InjectedFailure> {
        activated.set(true);
        Ok(())
      })
      .unwrap_err();

    assert!(!activated.get());
    assert_eq!(error.admitted_records(), 1);
    assert_eq!(error.code(), if wrong_receipt { "retirement_journal_receipt" } else { "retirement_journal_sink" });
    if wrong_receipt {
      assert!(owner.status().failed);
    }
  }
}

#[test]
fn activation_failure_returns_the_same_nonconstructible_permit_for_retry_without_reappend() {
  let algorithm = HashAlgorithm::Blake3_256;
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::new(10, 1024 * 1024, 30_000), &memory);
  let mut sink = RecordingSink::default();
  let pairs = [replacement_pair(algorithm, 1, RetirementReasonV1::Repair)];
  let records = replacements(&pairs);

  let error = RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink)
    .execute(batch(&records), 10, |_| -> Result<(), InjectedFailure> { Err(InjectedFailure) })
    .unwrap_err();
  let (source, prepared) = error.into_activation_failure().expect("activation failure must retain its permit");
  assert_eq!(source.to_string(), "injected replacement test failure");
  assert_eq!(owner.status().pending_records, 1);

  let result = prepared.activate(|permit| -> Result<_, InjectedFailure> {
    assert_eq!(permit.replacement_count(), 1);
    assert_eq!(permit.reason_count(RetirementReasonV1::Repair), 1);
    Ok("retried")
  });
  assert_eq!(result.unwrap().output, "retried");
  assert_eq!(owner.status().pending_records, 1, "activation retry must not append lineage twice");
  assert_eq!(sink.attempts, 0);
}

#[test]
fn complete_preflight_rejects_identity_sequence_extent_and_order_errors_without_mutation() {
  let algorithm = HashAlgorithm::Blake3_256;
  let valid = replacement_pair(algorithm, 1, RetirementReasonV1::StableKeyReplace);
  let mut cases = Vec::new();

  let mut wrong_key = valid.clone();
  wrong_key.1[0] = 0xFE;
  cases.push(wrong_key);

  let mut wrong_type = valid.clone();
  let type_offset = 2 * algorithm.hash_length() + 20;
  wrong_type.1[type_offset] = 3;
  cases.push(wrong_type);

  let mut backward_sequence = valid.clone();
  let sequence_offset = 2 * algorithm.hash_length() + 8;
  backward_sequence.1[sequence_offset..sequence_offset + 8].copy_from_slice(&50u64.to_le_bytes());
  cases.push(backward_sequence);

  let mut overlapping = valid.clone();
  let offset_start = 2 * algorithm.hash_length();
  let old_offset = u64::from_le_bytes(overlapping.0[offset_start..offset_start + 8].try_into().unwrap());
  overlapping.1[offset_start..offset_start + 8].copy_from_slice(&(old_offset + 100).to_le_bytes());
  cases.push(overlapping);

  for pair in cases {
    let cancellation = CancellationToken::new();
    let memory = memory_coordinator();
    let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::default(), &memory);
    let mut sink = RecordingSink::default();
    let pairs = [pair];
    let records = replacements(&pairs);
    let activated = Cell::new(false);
    let error = RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink)
      .execute(batch(&records), 10, |_| -> Result<(), InjectedFailure> {
        activated.set(true);
        Ok(())
      })
      .unwrap_err();
    assert_eq!(error.code(), "retirement_replacement_preflight");
    assert!(!activated.get());
    assert_eq!(owner.status().pending_records, 0);
    assert_eq!(sink.attempts, 0);
  }

  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::default(), &memory);
  let mut sink = RecordingSink::default();
  let pairs = [
    replacement_pair(algorithm, 2, RetirementReasonV1::StableKeyReplace),
    replacement_pair(algorithm, 1, RetirementReasonV1::StableKeyReplace),
  ];
  let records = replacements(&pairs);
  let error = RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink)
    .execute(batch(&records), 10, |_| -> Result<(), InjectedFailure> { Ok(()) })
    .unwrap_err();
  assert_eq!(error.code(), "retirement_replacement_preflight");
  assert_eq!(owner.status().pending_records, 0);
}

#[test]
fn empty_and_canceled_batches_refuse_before_activation() {
  let algorithm = HashAlgorithm::Blake3_256;
  for canceled in [false, true] {
    let cancellation = CancellationToken::new();
    if canceled {
      cancellation.cancel();
    }
    let memory = memory_coordinator();
    let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::default(), &memory);
    let mut sink = RecordingSink::default();
    let pairs = [replacement_pair(algorithm, 1, RetirementReasonV1::StableKeyReplace)];
    let records = replacements(&pairs);
    let selected = if canceled { records.as_slice() } else { &[] };
    let error = RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink)
      .execute(batch(selected), 10, |_| -> Result<(), InjectedFailure> { Ok(()) })
      .unwrap_err();
    assert_eq!(error.code(), if canceled { "retirement_journal_cancelled" } else { "retirement_replacement_preflight" });
    assert_eq!(error.admitted_records(), 0);
    assert_eq!(owner.status().pending_records, 0);
  }
}

#[test]
fn hard_memory_pressure_refuses_the_complete_batch_before_admission_or_activation() {
  let algorithm = HashAlgorithm::Blake3_256;
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::default(), &memory);
  memory.reconfigure_policy(MemoryPolicy::new(1, 2, 1, 1).unwrap()).unwrap();
  let mut sink = RecordingSink::default();
  let pairs = [replacement_pair(algorithm, 1, RetirementReasonV1::StableKeyReplace)];
  let records = replacements(&pairs);
  let activated = Cell::new(false);

  let error = RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink)
    .execute(batch(&records), 10, |_| -> Result<(), InjectedFailure> {
      activated.set(true);
      Ok(())
    })
    .unwrap_err();

  assert_eq!(error.code(), "retirement_journal_memory");
  assert_eq!(error.admitted_records(), 0);
  assert!(!activated.get());
  assert_eq!(owner.status().pending_records, 0);
  assert_eq!(sink.attempts, 0);
}

#[test]
fn a_legacy_incarnation_can_converge_forward_through_migration() {
  let algorithm = HashAlgorithm::Blake3_256;
  let old = physical_incarnation(algorithm, 1, 0x41, 10_000, 0, 300, 2, 0);
  let replacement = physical_incarnation(algorithm, 1, 0x61, 11_000, 1, 320, 2, 1);
  let pairs = [(old, replacement, RetirementReasonV1::Migration)];
  let records = replacements(&pairs);
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::default(), &memory);
  let mut sink = RecordingSink::default();

  let result =
    RetirementJournalReplacementCoordinatorV1::new(&mut owner, &mut sink)
      .execute(batch(&records), 10, |_| -> Result<_, InjectedFailure> { Ok(()) });

  assert!(result.is_ok());
  assert_eq!(owner.status().pending_records, 1);
}

fn rust_sources(path: PathBuf, sources: &mut Vec<PathBuf>) {
  for entry in fs::read_dir(path).unwrap() {
    let entry = entry.unwrap();
    if entry.file_type().unwrap().is_dir() {
      rust_sources(entry.path(), sources);
    } else if entry.path().extension().and_then(|extension| extension.to_str()) == Some("rs") {
      sources.push(entry.path());
    }
  }
}

#[test]
fn replacement_boundary_has_no_live_v3_service_or_control_store_activation() {
  let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let owner_path = source_root.join("engine/v4/gc_retirement.rs");
  let mut sources = Vec::new();
  rust_sources(source_root.clone(), &mut sources);
  let mut callers = Vec::new();
  for path in sources {
    if path == owner_path {
      continue;
    }
    let source = fs::read_to_string(&path).unwrap_or_default();
    if source.contains("RetirementJournalReplacementCoordinatorV1") {
      callers.push(path.strip_prefix(&source_root).unwrap().to_owned());
    }
  }
  callers.sort();
  assert_eq!(
    callers,
    [PathBuf::from("engine/v4/first_authority.rs")],
    "replacement boundary must remain confined to the disconnected P4 first-authority owner"
  );

  let v3 = fs::read_to_string(source_root.join("engine/namespace_mutation.rs")).unwrap();
  let controls = fs::read_to_string(source_root.join("engine/v4/control_store.rs")).unwrap();
  assert!(!v3.contains("PhysicalIncarnationV1"));
  assert!(!controls.contains("RetirementJournalReplacementCoordinatorV1"));
  assert!(controls.contains("store_control_file_record_v1"), "the named v4 ControlStore remains a v3 FileRecord transition writer");
}
