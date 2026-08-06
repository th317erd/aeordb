use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::sync::{Arc, Barrier};

use aeordb::engine::hash_algorithm::HashAlgorithm;
use aeordb::engine::kv_page_provider::KvPageProvider;
use aeordb::engine::kv_pages::{bucket_page_offset, page_size, serialize_page};
use aeordb::engine::kv_store::{KVEntry, KV_TYPE_CHUNK};
use aeordb::engine::memory_coordinator::{HostMemorySample, MemoryCoordinator, MemoryOwner, MemoryPolicy};

const KV_OFFSET: u64 = 256;
const BUCKETS: usize = 4;

fn entry(hash_byte: u8) -> KVEntry {
  KVEntry { type_flags: KV_TYPE_CHUNK, hash: vec![hash_byte; 32], offset: u64::from(hash_byte) * 100, total_length: 64 }
}

fn pages() -> Vec<Vec<u8>> {
  vec![serialize_page(&[entry(1)], 32), serialize_page(&[entry(2)], 32), serialize_page(&[entry(3)], 32), serialize_page(&[entry(4)], 32)]
}

fn write_pages(path: &std::path::Path, page_data: &[Vec<u8>]) -> File {
  let mut file = OpenOptions::new().read(true).write(true).create_new(true).open(path).unwrap();
  file.write_all(&vec![0u8; KV_OFFSET as usize]).unwrap();
  for page in page_data {
    file.write_all(page).unwrap();
  }
  file.sync_all().unwrap();
  file
}

fn coordinator() -> MemoryCoordinator {
  let coordinator = MemoryCoordinator::new(MemoryPolicy::new(64 * 1024, 128 * 1024, 16 * 1024, 16 * 1024).unwrap());
  coordinator.update_host_sample(HostMemorySample { rss_bytes: 0, host_available_bytes: Some(1024 * 1024), ..Default::default() }).unwrap();
  coordinator
}

fn provider(file: &File, max_resident_bytes: u64, coordinator: MemoryCoordinator) -> KvPageProvider {
  KvPageProvider::new(file.try_clone().unwrap(), KV_OFFSET, HashAlgorithm::Blake3_256, BUCKETS, max_resident_bytes, Some(coordinator))
    .unwrap()
}

fn overwrite_page(file: &File, bucket: usize, page: &[u8]) {
  let mut writer = file.try_clone().unwrap();
  writer.seek(SeekFrom::Start(KV_OFFSET + bucket_page_offset(bucket, 32))).unwrap();
  writer.write_all(page).unwrap();
  writer.sync_data().unwrap();
}

#[test]
fn positioned_reads_are_exact_and_lru_residency_never_exceeds_the_byte_cap() {
  let directory = tempfile::tempdir().unwrap();
  let expected = pages();
  let file = write_pages(&directory.path().join("pages.aeordb"), &expected);
  let page_bytes = page_size(32) as u64;
  let coordinator = coordinator();
  let provider = provider(&file, page_bytes * 2, coordinator.clone());

  assert_eq!(provider.read_page(0).unwrap().as_ref(), expected[0]);
  assert_eq!(provider.read_page(1).unwrap().as_ref(), expected[1]);
  assert_eq!(provider.read_page(0).unwrap().as_ref(), expected[0]);
  assert_eq!(provider.read_page(2).unwrap().as_ref(), expected[2]);

  let stats = provider.stats().unwrap();
  assert_eq!(stats.resident_pages, 2);
  assert!(stats.resident_bytes <= page_bytes * 2);
  assert_eq!(stats.hits, 1);
  assert_eq!(stats.misses, 3);
  assert_eq!(stats.evictions, 1);
  let owner = coordinator.snapshot().unwrap().owner(MemoryOwner::KvResidentPages).unwrap().clone();
  assert_eq!(owner.reserved_bytes, stats.resident_bytes);
  assert_eq!(owner.active_reservations, stats.resident_pages);
}

#[test]
fn concurrent_misses_for_one_bucket_are_coalesced_to_one_positioned_read() {
  let directory = tempfile::tempdir().unwrap();
  let expected = pages();
  let file = write_pages(&directory.path().join("coalesced.aeordb"), &expected);
  let provider = Arc::new(provider(&file, page_size(32) as u64 * 2, coordinator()));
  let start = Arc::new(Barrier::new(17));
  let mut threads = Vec::new();

  for _ in 0..16 {
    let provider = Arc::clone(&provider);
    let start = Arc::clone(&start);
    let expected = expected[3].clone();
    threads.push(std::thread::spawn(move || {
      start.wait();
      assert_eq!(provider.read_page(3).unwrap().as_ref(), expected);
    }));
  }
  start.wait();
  for thread in threads {
    thread.join().unwrap();
  }

  let stats = provider.stats().unwrap();
  assert_eq!(stats.disk_reads, 1);
  assert_eq!(stats.misses, 1);
  assert_eq!(stats.hits, 15);
}

#[test]
fn malformed_and_truncated_pages_fail_without_poisoning_or_reserving_cache_state() {
  let directory = tempfile::tempdir().unwrap();
  let mut malformed = pages();
  malformed[1][20] ^= 0x80;
  let path = directory.path().join("malformed.aeordb");
  let mut file = write_pages(&path, &malformed);
  let coordinator = coordinator();
  let provider = provider(&file, page_size(32) as u64 * 2, coordinator.clone());

  assert!(provider.read_page(1).is_err());
  let stats = provider.stats().unwrap();
  assert_eq!(stats.resident_pages, 0);
  assert_eq!(stats.read_failures, 1);
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::KvResidentPages).unwrap().reserved_bytes, 0);

  file.seek(SeekFrom::Start(KV_OFFSET + bucket_page_offset(2, 32))).unwrap();
  file.set_len(KV_OFFSET + bucket_page_offset(2, 32) + 7).unwrap();
  assert!(provider.read_page(2).is_err());
  assert_eq!(provider.stats().unwrap().read_failures, 2);
}

#[test]
fn pressure_or_a_zero_local_cap_returns_exact_transient_pages_without_retention() {
  let directory = tempfile::tempdir().unwrap();
  let expected = pages();
  let file = write_pages(&directory.path().join("pressure.aeordb"), &expected);
  let coordinator = coordinator();
  coordinator
    .update_host_sample(HostMemorySample { rss_bytes: 128 * 1024, host_available_bytes: Some(1024 * 1024), ..Default::default() })
    .unwrap();
  let provider = provider(&file, 0, coordinator.clone());

  assert_eq!(provider.read_page(0).unwrap().as_ref(), expected[0]);
  assert_eq!(provider.read_page(0).unwrap().as_ref(), expected[0]);
  let stats = provider.stats().unwrap();
  assert_eq!(stats.resident_pages, 0);
  assert_eq!(stats.disk_reads, 2);
  assert_eq!(stats.cache_deferrals, 2);
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::KvResidentPages).unwrap().reserved_bytes, 0);
}

#[test]
fn out_of_range_buckets_are_typed_failures_and_perform_no_io() {
  let directory = tempfile::tempdir().unwrap();
  let expected = pages();
  let file = write_pages(&directory.path().join("bounds.aeordb"), &expected);
  let provider = provider(&file, page_size(32) as u64, coordinator());

  assert!(provider.read_page(BUCKETS).is_err());
  let stats = provider.stats().unwrap();
  assert_eq!(stats.disk_reads, 0);
  assert_eq!(stats.read_failures, 1);
}

#[test]
fn committed_updates_preserve_old_snapshot_bytes_until_the_last_old_view_drops() {
  let directory = tempfile::tempdir().unwrap();
  let original = pages();
  let file = write_pages(&directory.path().join("generations.aeordb"), &original);
  let coordinator = coordinator();
  let provider = provider(&file, page_size(32) as u64 * 2, coordinator.clone());
  let old_view = provider.snapshot().unwrap();
  let replacement = serialize_page(&[entry(99)], 32);

  let mut update = provider.begin_update(&[0]).unwrap();
  update.mark_overwrite_started().unwrap();
  overwrite_page(&file, 0, &replacement);
  assert_eq!(old_view.read_page(0).unwrap().as_ref(), original[0], "an in-flight overwrite must not leak into the old view");
  update.commit(vec![(0, Arc::<[u8]>::from(replacement.clone().into_boxed_slice()))]).unwrap();

  let new_view = provider.snapshot().unwrap();
  assert_eq!(new_view.read_page(0).unwrap().as_ref(), replacement);
  assert_eq!(old_view.read_page(0).unwrap().as_ref(), original[0]);
  let retained = provider.stats().unwrap();
  assert_eq!(retained.historical_pages, 1);
  assert_eq!(retained.historical_bytes, page_size(32) as u64);
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::KvSnapshotGenerations).unwrap().reserved_bytes, page_size(32) as u64);

  drop(old_view);
  let pruned = provider.stats().unwrap();
  assert_eq!(pruned.historical_pages, 0);
  assert_eq!(pruned.historical_bytes, 0);
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::KvSnapshotGenerations).unwrap().reserved_bytes, 0);
}

#[test]
fn history_reservation_failure_happens_before_overwrite_and_leaves_the_file_unchanged() {
  let directory = tempfile::tempdir().unwrap();
  let original = pages();
  let file = write_pages(&directory.path().join("history-budget.aeordb"), &original);
  let constrained = MemoryCoordinator::new(MemoryPolicy::new(900, 1_500, 200, 500).unwrap());
  constrained.update_host_sample(HostMemorySample { rss_bytes: 0, host_available_bytes: Some(1024 * 1024), ..Default::default() }).unwrap();
  let provider = provider(&file, page_size(32) as u64 * 2, constrained);
  let old_view = provider.snapshot().unwrap();

  assert!(provider.begin_update(&[0]).is_err());
  assert_eq!(old_view.read_page(0).unwrap().as_ref(), original[0]);
  assert!(!provider.is_poisoned().unwrap());
}

#[test]
fn dropping_an_update_before_overwrite_aborts_cleanly_but_after_overwrite_poisons_publication() {
  let directory = tempfile::tempdir().unwrap();
  let original = pages();
  let file = write_pages(&directory.path().join("update-state.aeordb"), &original);
  let provider = provider(&file, page_size(32) as u64 * 2, coordinator());
  let old_view = provider.snapshot().unwrap();

  provider.begin_update(&[1]).unwrap().abort_before_overwrite().unwrap();
  assert!(!provider.is_poisoned().unwrap());
  let mut failed = provider.begin_update(&[1]).unwrap();
  failed.mark_overwrite_started().unwrap();
  overwrite_page(&file, 1, &serialize_page(&[entry(88)], 32));
  drop(failed);

  assert!(provider.is_poisoned().unwrap());
  assert!(provider.snapshot().is_err(), "a new generation cannot be exposed after an uncommitted overwrite");
  assert_eq!(old_view.read_page(1).unwrap().as_ref(), original[1], "the admitted old view must retain exact pre-overwrite bytes");
  assert!(provider.begin_update(&[2]).is_err());
}

#[test]
fn update_sets_are_unique_bounded_and_single_writer() {
  let directory = tempfile::tempdir().unwrap();
  let original = pages();
  let file = write_pages(&directory.path().join("update-bounds.aeordb"), &original);
  let provider = provider(&file, page_size(32) as u64 * 2, coordinator());

  assert!(provider.begin_update(&[]).is_err());
  assert!(provider.begin_update(&[0, 0]).is_err());
  assert!(provider.begin_update(&[BUCKETS]).is_err());
  let update = provider.begin_update(&[0, 1]).unwrap();
  assert!(provider.begin_update(&[2]).is_err());
  update.abort_before_overwrite().unwrap();
  assert!(provider.begin_update(&[2]).is_ok());
}

#[test]
fn snapshot_drain_waits_for_live_views_and_completes_after_release() {
  let directory = tempfile::tempdir().unwrap();
  let original = pages();
  let file = write_pages(&directory.path().join("snapshot-drain.aeordb"), &original);
  let provider = provider(&file, page_size(32) as u64 * 2, coordinator());
  let view = provider.snapshot().unwrap();

  assert!(!provider.wait_for_no_snapshots(std::time::Duration::from_millis(5)).unwrap());
  drop(view);
  assert!(provider.wait_for_no_snapshots(std::time::Duration::from_millis(50)).unwrap());
}

#[test]
fn update_validation_failures_release_pre_overwrite_state_but_poison_after_overwrite_admission() {
  let directory = tempfile::tempdir().unwrap();
  let original = pages();
  let file = write_pages(&directory.path().join("update-validation.aeordb"), &original);
  let coordinator = coordinator();
  let provider = provider(&file, page_size(32) as u64 * 2, coordinator.clone());

  let update = provider.begin_update(&[0]).unwrap();
  assert!(update.commit(vec![(0, Arc::<[u8]>::from(original[0].clone().into_boxed_slice()))]).is_err());
  assert!(!provider.is_poisoned().unwrap());
  assert_eq!(provider.stats().unwrap().pending_pages, 0);
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::KvSnapshotGenerations).unwrap().reserved_bytes, 0);

  let mut update = provider.begin_update(&[0, 1]).unwrap();
  update.mark_overwrite_started().unwrap();
  assert!(update.commit(vec![(0, Arc::<[u8]>::from(original[0].clone().into_boxed_slice()))]).is_err());
  assert!(provider.is_poisoned().unwrap());
  assert_eq!(provider.stats().unwrap().pending_pages, 2);
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::KvSnapshotGenerations).unwrap().reserved_bytes, page_size(32) as u64 * 2);
}

#[test]
fn each_live_snapshot_generation_reads_its_exact_page_across_multiple_commits() {
  let directory = tempfile::tempdir().unwrap();
  let original = pages();
  let file = write_pages(&directory.path().join("multi-generation.aeordb"), &original);
  let coordinator = coordinator();
  let provider = provider(&file, page_size(32) as u64 * 2, coordinator.clone());
  let generation_zero = provider.snapshot().unwrap();
  let generation_one_page = serialize_page(&[entry(77)], 32);
  let generation_two_page = serialize_page(&[entry(88)], 32);

  let mut first = provider.begin_update(&[0]).unwrap();
  first.mark_overwrite_started().unwrap();
  overwrite_page(&file, 0, &generation_one_page);
  first.commit(vec![(0, Arc::<[u8]>::from(generation_one_page.clone().into_boxed_slice()))]).unwrap();
  let generation_one = provider.snapshot().unwrap();

  let mut second = provider.begin_update(&[0]).unwrap();
  second.mark_overwrite_started().unwrap();
  overwrite_page(&file, 0, &generation_two_page);
  second.commit(vec![(0, Arc::<[u8]>::from(generation_two_page.clone().into_boxed_slice()))]).unwrap();
  let generation_two = provider.snapshot().unwrap();

  assert_eq!(generation_zero.generation(), 0);
  assert_eq!(generation_one.generation(), 1);
  assert_eq!(generation_two.generation(), 2);
  assert_eq!(generation_zero.read_page(0).unwrap().as_ref(), original[0]);
  assert_eq!(generation_one.read_page(0).unwrap().as_ref(), generation_one_page);
  assert_eq!(generation_two.read_page(0).unwrap().as_ref(), generation_two_page);
  assert_eq!(provider.stats().unwrap().historical_pages, 2);

  drop(generation_zero);
  assert_eq!(provider.stats().unwrap().historical_pages, 1);
  drop(generation_one);
  assert_eq!(provider.stats().unwrap().historical_pages, 0);
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::KvSnapshotGenerations).unwrap().reserved_bytes, 0);
}

#[test]
fn cloned_snapshots_share_one_generation_lease() {
  let directory = tempfile::tempdir().unwrap();
  let original = pages();
  let file = write_pages(&directory.path().join("clone-lease.aeordb"), &original);
  let provider = provider(&file, page_size(32) as u64, coordinator());

  let first = provider.snapshot().unwrap();
  let second = first.clone();
  assert_eq!(provider.stats().unwrap().active_snapshots, 1);
  drop(first);
  assert_eq!(provider.stats().unwrap().active_snapshots, 1);
  drop(second);
  assert_eq!(provider.stats().unwrap().active_snapshots, 0);
}
