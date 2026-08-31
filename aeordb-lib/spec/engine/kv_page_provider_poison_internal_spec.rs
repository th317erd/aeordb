use std::collections::HashMap;
use std::io::{Seek, SeekFrom, Write};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use super::*;
use crate::engine::kv_pages::{page_size, serialize_page};
use crate::engine::kv_store::{KVEntry, KV_TYPE_CHUNK};
use crate::engine::memory_coordinator::{HostMemorySample, MemoryPolicy};

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

fn page(hash_byte: u8) -> Vec<u8> {
  serialize_page(
    &[KVEntry { type_flags: KV_TYPE_CHUNK, hash: vec![hash_byte; 32], offset: u64::from(hash_byte) * 100, total_length: 64 }],
    32,
  )
}

fn provider_with_page(page: &[u8]) -> (tempfile::TempDir, File, KvPageProvider, MemoryCoordinator) {
  let directory = tempfile::tempdir().unwrap();
  let mut file = File::options().read(true).write(true).create_new(true).open(directory.path().join("page-provider-race.aeordb")).unwrap();
  file.write_all(page).unwrap();
  file.sync_all().unwrap();
  let coordinator = MemoryCoordinator::new(MemoryPolicy::new(64 * 1024, 128 * 1024, 16 * 1024, 16 * 1024).unwrap());
  coordinator.update_host_sample(HostMemorySample { rss_bytes: 0, host_available_bytes: Some(1024 * 1024), ..Default::default() }).unwrap();
  let provider =
    KvPageProvider::new(file.try_clone().unwrap(), 0, HashAlgorithm::Blake3_256, 1, page_size(32) as u64, Some(coordinator.clone()))
      .unwrap();
  (directory, file, provider, coordinator)
}

#[test]
fn reader_cannot_publish_pre_update_bytes_into_the_committed_generation_cache() {
  let original = page(1);
  let replacement = page(2);
  let (_directory, mut file, provider, coordinator) = provider_with_page(&original);
  let reader_snapshot = provider.snapshot().unwrap();
  {
    let mut state = provider.inner.state.lock().unwrap();
    state.preparing_update = true;
  }

  let (read_event_sender, read_event_receiver) = mpsc::sync_channel(2);
  let (read_resume_sender, read_resume_receiver) = mpsc::sync_channel(1);
  *provider.inner.page_read_test_hook.lock().unwrap() = Some(PageReadTestHook { events: read_event_sender, resume: read_resume_receiver });

  let (reader_result_sender, reader_result_receiver) = mpsc::sync_channel(1);
  let reader = std::thread::spawn(move || {
    let result = reader_snapshot.read_page(0);
    reader_result_sender.send(result).unwrap();
  });

  let first_read_event = read_event_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
  let reader_reached_cache_publish_during_preparation = first_read_event == PageReadTestEvent::BeforeCachePublish;
  let reservation = coordinator.reserve(MemoryOwner::KvSnapshotGenerations, original.len() as u64, AdmissionClass::Workload).unwrap();
  {
    let mut state = provider.inner.state.lock().unwrap();
    state.pending = Some(PendingUpdate {
      generation: 1,
      pages: HashMap::from([(
        0,
        PendingPage { old_generation: 0, data: Arc::<[u8]>::from(original.clone().into_boxed_slice()), reservation },
      )]),
      overwrite_started: true,
    });
  }
  file.seek(SeekFrom::Start(0)).unwrap();
  file.write_all(&replacement).unwrap();
  file.sync_data().unwrap();
  let update = KvPageUpdate { provider: provider.clone(), generation: 1, overwrite_started: true, completed: false };
  update.commit(vec![(0, Arc::<[u8]>::from(replacement.clone().into_boxed_slice()))]).unwrap();

  if reader_reached_cache_publish_during_preparation {
    read_resume_sender.send(()).unwrap();
  } else {
    assert_eq!(first_read_event, PageReadTestEvent::BlockedByUpdatePreparation);
    let hook = provider.inner.page_read_test_hook.lock().unwrap().take();
    assert!(hook.is_some(), "a preparation-blocked reader unexpectedly consumed the cache-publication hook");
  }
  let reader_result = reader_result_receiver.recv_timeout(Duration::from_secs(1)).unwrap().unwrap();
  reader.join().unwrap();
  let committed_page = provider.snapshot().unwrap().read_page(0).unwrap();

  assert_eq!(reader_result.as_ref(), original, "a read admitted before commit lost its exact generation bytes");
  assert_eq!(committed_page.as_ref(), replacement, "the committed generation cache retained pre-update bytes");
  assert!(!reader_reached_cache_publish_during_preparation, "a reader started a disk load while update preparation was unpublished");
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

#[test]
fn abandoned_overwrite_with_missing_pending_state_clears_preparation_and_poisons_publication() {
  let provider = provider();
  provider.inner.state.lock().unwrap().preparing_update = true;
  let update = KvPageUpdate { provider: provider.clone(), generation: 1, overwrite_started: true, completed: false };

  drop(update);

  let state = provider.inner.state.lock().unwrap();
  assert!(!state.preparing_update);
  assert!(state.poisoned_reason.as_deref().is_some_and(|reason| reason.contains("lost its pending state")));
}
