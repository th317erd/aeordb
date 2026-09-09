use super::*;
use crate::engine::memory_coordinator::{HostMemorySample, MemoryPolicy};
use std::collections::VecDeque;
use std::io::Write;

fn assert_order(provider: &KvPageProvider, expected: &VecDeque<usize>) {
  let state = provider.lock().unwrap();
  assert_eq!(state.pages.len(), expected.len());
  assert_eq!(state.oldest, expected.front().copied());
  assert_eq!(state.newest, expected.back().copied());
  let mut current = state.oldest;
  let mut previous = None;
  for &bucket in expected {
    assert_eq!(current, Some(bucket));
    let page = &state.pages[&bucket];
    assert_eq!(page.previous, previous);
    previous = current;
    current = page.next;
  }
  assert_eq!(current, None, "LRU contains a cycle or an orphaned entry");
}

#[test]
fn lru_links_remain_bounded_and_exact_across_hits_removals_and_refills() {
  let directory = tempfile::tempdir().unwrap();
  // Windows positioned reads reopen ordinary shared database handles;
  // anonymous tempfile handles deliberately disallow that sharing mode.
  let mut file = File::options().read(true).write(true).create_new(true).open(directory.path().join("lru-pages.aeordb")).unwrap();
  let page = crate::engine::kv_pages::serialize_page(&[], 32);
  for _ in 0..32 {
    file.write_all(&page).unwrap();
  }
  file.sync_all().unwrap();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(1 << 20, 2 << 20, 65536, 65536).unwrap());
  memory.update_host_sample(HostMemorySample { rss_bytes: 0, host_available_bytes: Some(8 << 20), ..Default::default() }).unwrap();
  let provider = KvPageProvider::new(file, 0, HashAlgorithm::Blake3_256, 32, (page.len() * 16) as u64, Some(memory)).unwrap();
  let mut expected = VecDeque::new();
  let mut random = 3u64;
  for step in 0..10000 {
    random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
    let bucket = (random >> 32) as usize % 32;
    if step % 5 == 0 {
      let position = expected.iter().position(|resident| *resident == bucket);
      let mut state = provider.lock().unwrap();
      assert_eq!(remove_cached_page(&mut state, bucket), position.is_some());
      if let Some(position) = position {
        expected.remove(position);
      }
    } else {
      provider.read_page(bucket).unwrap();
      if let Some(position) = expected.iter().position(|resident| *resident == bucket) {
        expected.remove(position);
      } else if expected.len() == 16 {
        expected.pop_front();
      }
      expected.push_back(bucket);
    }
    assert_order(&provider, &expected);
  }
  let bucket = *expected.back().unwrap();
  let before = provider.stats().unwrap();
  for _ in 0..100000 {
    provider.read_page(bucket).unwrap();
  }
  assert_order(&provider, &expected);
  let after = provider.stats().unwrap();
  assert_eq!(before.resident_bytes, after.resident_bytes);
  assert_eq!(before.resident_pages, after.resident_pages);
  assert_eq!(before.eviction_candidates, after.eviction_candidates);
  while let Some(bucket) = expected.pop_front() {
    assert!(remove_cached_page(&mut provider.lock().unwrap(), bucket));
    assert_order(&provider, &expected);
  }
  assert!(!evict_oldest_page(&mut provider.lock().unwrap()));
  provider.read_page(0).unwrap();
  assert_order(&provider, &VecDeque::from([0]));
}
