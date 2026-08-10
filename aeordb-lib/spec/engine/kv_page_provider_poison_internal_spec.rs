use std::collections::HashMap;

use super::*;

fn provider() -> KvPageProvider {
  let file = tempfile::tempfile().unwrap();
  KvPageProvider::new(file, 0, HashAlgorithm::Blake3_256, 1, 1024 * 1024, None).unwrap()
}

fn poison_state(provider: &KvPageProvider) {
  let provider = provider.clone();
  let unwind = std::thread::spawn(move || {
    let _state = provider.inner.state.lock().unwrap();
    panic!("inject KV page state poison");
  })
  .join();
  assert!(unwind.is_err());
}

fn poisoned_state(provider: &KvPageProvider) -> std::sync::MutexGuard<'_, PageCacheState> {
  match provider.inner.state.lock() {
    Ok(_) => panic!("KV page provider unexpectedly lost its poison state"),
    Err(poisoned) => poisoned.into_inner(),
  }
}

#[test]
fn snapshot_lease_releases_its_generation_after_page_state_poison() {
  let provider = provider();
  let snapshot = provider.snapshot().unwrap();
  poison_state(&provider);

  drop(snapshot);

  let state = poisoned_state(&provider);
  assert!(state.active_generations.is_empty());
}

#[test]
fn abandoned_overwrite_records_poison_evidence_after_page_state_poison() {
  let provider = provider();
  {
    let mut state = provider.inner.state.lock().unwrap();
    state.preparing_update = true;
    state.pending = Some(PendingUpdate { generation: 1, pages: HashMap::new(), overwrite_started: true });
  }
  let update = KvPageUpdate { provider: provider.clone(), generation: 1, overwrite_started: true, completed: false };
  poison_state(&provider);

  drop(update);

  let state = poisoned_state(&provider);
  assert!(!state.preparing_update);
  assert!(state.poisoned_reason.as_deref().is_some_and(|reason| reason.contains("generation 1 was abandoned")));
}
