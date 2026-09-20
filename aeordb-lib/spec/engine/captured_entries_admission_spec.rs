//! Enclosing admission must see raw page work, not just live callbacks.
use super::*;
use std::cell::Cell;
use crate::engine::kv_pages::serialize_page;

#[derive(Debug)]
enum AdmissionFailure {
  Engine(EngineError),
  Work,
  Callback,
}
impl From<EngineError> for AdmissionFailure {
  fn from(error: EngineError) -> Self {
    Self::Engine(error)
  }
}

fn admitted_snapshot() -> ReadSnapshot {
  let mut entries: Vec<_> =
    (1u8..=4).map(|value| KVEntry { type_flags: 0, hash: vec![value; 32], offset: u64::from(value) * 100, total_length: 64 }).collect();
  entries[3].type_flags |= KV_FLAG_DELETED;
  let mut replacement = entries[0].clone();
  replacement.offset = 9999;
  let mut deleted = entries[1].clone();
  deleted.type_flags |= KV_FLAG_DELETED;
  let inserted = KVEntry { hash: vec![5; 32], ..replacement.clone() };
  let buffer = [replacement, deleted, inserted].into_iter().map(|entry| (entry.hash.clone(), entry)).collect();
  let pages = Arc::new(vec![Arc::<[u8]>::from(serialize_page(&entries, 32).into_boxed_slice())]);
  ReadSnapshot::new(buffer, Arc::new(KvNvt::new(1)), 1, HashAlgorithm::Blake3_256, 3, pages).unwrap()
}

#[test]
fn native_semantic_task_retention_kv_admission_counts_pages_tombstones_and_overrides() {
  let snapshot = admitted_snapshot();
  for allowance in [8, 7] {
    let charges = Cell::new(0);
    let mut callbacks = 0;
    let result = snapshot.visit_captured_entries_admitted::<AdmissionFailure>(
      &CancellationToken::new(),
      100,
      || {
        if charges.get() == allowance {
          return Err(AdmissionFailure::Work);
        }
        charges.set(charges.get() + 1);
        Ok(())
      },
      |_| {
        callbacks += 1;
        Ok(true)
      },
    );
    assert_eq!(charges.get(), allowance);
    if allowance == 8 {
      let summary = result.unwrap();
      assert!(summary.complete);
      assert_eq!((summary.scanned_pages, summary.scanned_entries, summary.visited_entries), (1, 7, 3));
      assert_eq!(callbacks, 3);
    } else {
      assert!(matches!(result, Err(AdmissionFailure::Work)));
    }
    assert_eq!(snapshot.buffer_len(), 3);
  }
}

#[test]
fn native_semantic_task_retention_kv_admission_preserves_existing_local_failure_order() {
  let snapshot = admitted_snapshot();
  let cancellation = CancellationToken::new();
  let mut admission_calls = 0;
  let result = snapshot.visit_captured_entries_admitted::<AdmissionFailure>(
    &cancellation,
    0,
    || {
      admission_calls += 1;
      Err(AdmissionFailure::Work)
    },
    |_| panic!("local refusal"),
  );
  assert!(matches!(result, Err(AdmissionFailure::Engine(EngineError::ResourceExhausted(_)))));
  assert_eq!(admission_calls, 0);
  cancellation.cancel();
  let result = snapshot.visit_captured_entries_admitted::<AdmissionFailure>(
    &cancellation,
    100,
    || {
      admission_calls += 1;
      Err(AdmissionFailure::Work)
    },
    |_| panic!("cancelled"),
  );
  assert!(matches!(result, Err(AdmissionFailure::Engine(EngineError::Cancelled(_)))));
  assert_eq!(admission_calls, 0);
}

#[test]
fn native_semantic_task_retention_kv_admission_retains_callback_error_and_early_stop() {
  let snapshot = admitted_snapshot();
  let cancellation = CancellationToken::new();
  let result = snapshot.visit_captured_entries_admitted::<AdmissionFailure>(
    &cancellation,
    100,
    || Ok(()),
    |_| {
      cancellation.cancel();
      Err(AdmissionFailure::Callback)
    },
  );
  assert!(matches!(result, Err(AdmissionFailure::Callback)));
  let result =
    snapshot.visit_captured_entries_admitted::<AdmissionFailure>(&CancellationToken::new(), 100, || Ok(()), |_| Ok(false)).unwrap();
  assert!(!result.complete);
  assert_eq!(result.visited_entries, 1);
}
