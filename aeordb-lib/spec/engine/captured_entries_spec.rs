use super::*;
use aeordb::engine::EngineError;
use tokio_util::sync::CancellationToken;
use aeordb::engine::kv_pages::serialize_page;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use std::io::Write;

#[path = "../support/allocation_probe.rs"]
mod allocation_probe;

fn buffered(entry_count: usize, wrong_key: bool) -> ReadSnapshot {
  let entry = make_entry(41, 100);
  let key = if wrong_key { make_hash(42) } else { entry.hash.clone() };
  ReadSnapshot::new(HashMap::from([(key, entry)]), make_nvt(1), 1, HashAlgorithm::Blake3_256, entry_count, empty_pages(1, 32)).unwrap()
}

#[test]
fn captured_entries_refuse_mismatched_live_count() {
  let snapshot = buffered(2, false);
  let error = snapshot.visit_captured_entries(&CancellationToken::new(), 100, |_| Ok(true)).unwrap_err();
  assert!(matches!(error, EngineError::CorruptEntry { .. }));
  assert!(error.to_string().contains("count"));
}

#[test]
fn captured_entries_recheck_cancellation_after_the_final_callback() {
  let snapshot = buffered(1, false);
  let cancellation = CancellationToken::new();
  let mut callbacks = 0;
  let error = snapshot
    .visit_captured_entries(&cancellation, 100, |_| {
      callbacks += 1;
      cancellation.cancel();
      Ok(true)
    })
    .unwrap_err();
  assert!(matches!(error, EngineError::Cancelled(_)));
  assert_eq!(callbacks, 1);
}

#[test]
fn captured_entries_refuse_mismatched_buffer_map_keys() {
  let snapshot = buffered(1, true);
  let error = snapshot.visit_captured_entries(&CancellationToken::new(), 100, |_| Ok(true)).unwrap_err();
  assert!(matches!(error, EngineError::CorruptEntry { .. }));
  assert!(error.to_string().contains("buffer"));
}

fn profile_entry(algorithm: HashAlgorithm, seed: u8) -> KVEntry {
  KVEntry { type_flags: KV_TYPE_CHUNK, hash: vec![seed; algorithm.hash_length()], offset: u64::from(seed) * 100, total_length: 64 }
}

fn one_page(algorithm: HashAlgorithm, entries: &[KVEntry], buffer: HashMap<Vec<u8>, KVEntry>, count: usize) -> ReadSnapshot {
  ReadSnapshot::new(buffer, make_nvt(1), 1, algorithm, count, Arc::new(vec![page_arc(serialize_page(entries, algorithm.hash_length()))]))
    .unwrap()
}

#[test]
fn captured_entries_count_all_work_but_emit_only_effective_live_entries_at_both_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let mut entries: Vec<_> = (1..=4).map(|seed| profile_entry(algorithm, seed)).collect();
    entries[3].type_flags |= KV_FLAG_DELETED;
    let mut replacement = entries[0].clone();
    replacement.offset = 9999;
    replacement.type_flags = KV_TYPE_FILE_RECORD;
    let mut removed = entries[1].clone();
    removed.type_flags |= KV_FLAG_DELETED;
    let inserted = profile_entry(algorithm, 5);
    let buffer = [replacement.clone(), removed, inserted.clone()].into_iter().map(|entry| (entry.hash.clone(), entry)).collect();
    let snapshot = one_page(algorithm, &entries, buffer, 3);
    let mut observed = Vec::new();
    let summary = snapshot
      .visit_captured_entries(&CancellationToken::new(), 8, |entry| {
        observed.push(entry.clone());
        Ok(true)
      })
      .unwrap();
    assert!(summary.complete);
    assert_eq!((summary.scanned_pages, summary.scanned_entries, summary.visited_entries), (1, 7, 3));
    assert_eq!(observed.len(), 3);
    assert!(observed.contains(&replacement));
    assert!(observed.contains(&entries[2]));
    assert!(observed.contains(&inserted));
    assert_eq!(snapshot.buffer_len(), 3, "scanning must not flush the captured buffer");
    assert!(snapshot.visit_captured_slots(&CancellationToken::new(), |_, _| Ok(true)).is_err(), "no fabricated physical slots");
    assert!(matches!(snapshot.visit_captured_entries(&CancellationToken::new(), 7, |_| Ok(true)), Err(EngineError::ResourceExhausted(_))));
  }
}

#[test]
fn captured_entries_preserve_early_stop_and_callback_error_without_claiming_closure() {
  let snapshot = buffered(99, false);
  let summary = snapshot.visit_captured_entries(&CancellationToken::new(), 100, |_| Ok(false)).unwrap();
  assert!(!summary.complete, "early stop does not validate the remaining snapshot count");
  assert_eq!(summary.visited_entries, 1);
  let mut callbacks = 0;
  let error = snapshot
    .visit_captured_entries(&CancellationToken::new(), 100, |_| {
      callbacks += 1;
      Err(EngineError::InvalidInput("captured callback failure".to_string()))
    })
    .unwrap_err();
  assert!(matches!(error, EngineError::InvalidInput(message) if message == "captured callback failure"));
  assert_eq!(callbacks, 1);
}

#[test]
fn captured_entries_cancellation_applies_to_empty_views_and_stopping_callbacks() {
  let empty = one_page(HashAlgorithm::Blake3_256, &[], HashMap::new(), 0);
  let cancellation = CancellationToken::new();
  cancellation.cancel();
  assert!(matches!(empty.visit_captured_entries(&cancellation, 100, |_| panic!("empty")), Err(EngineError::Cancelled(_))));
  let complete = empty.visit_captured_entries(&CancellationToken::new(), 1, |_| panic!("empty")).unwrap();
  assert!(complete.complete);
  assert_eq!((complete.scanned_pages, complete.scanned_entries, complete.visited_entries), (1, 0, 0));
  let snapshot = buffered(1, false);
  let cancellation = CancellationToken::new();
  assert!(matches!(
    snapshot.visit_captured_entries(&cancellation, 100, |_| {
      cancellation.cancel();
      Ok(false)
    }),
    Err(EngineError::Cancelled(_))
  ));
}

#[test]
fn captured_entries_refuse_duplicate_or_wrong_bucket_pages_even_when_overridden() {
  let algorithm = HashAlgorithm::Blake3_256;
  let entry = profile_entry(algorithm, 1);
  let buffer = HashMap::from([(entry.hash.clone(), entry.clone())]);
  let duplicate = one_page(algorithm, &[entry.clone(), entry.clone()], buffer.clone(), 1);
  let mut calls = 0;
  let error = duplicate
    .visit_captured_entries(&CancellationToken::new(), 100, |_| {
      calls += 1;
      Ok(true)
    })
    .unwrap_err();
  assert!(error.to_string().contains("duplicate"));
  assert_eq!(calls, 0);
  let table = make_nvt(2);
  let actual_bucket = table.bucket_for_value(&entry.hash);
  let mut pages = vec![page_arc(serialize_page(&[], 32)); 2];
  pages[1 - actual_bucket] = page_arc(serialize_page(std::slice::from_ref(&entry), 32));
  let wrong = ReadSnapshot::new(buffer, table, 2, algorithm, 1, Arc::new(pages)).unwrap();
  let error = wrong.visit_captured_entries(&CancellationToken::new(), 100, |_| Ok(true)).unwrap_err();
  assert!(error.to_string().contains("bucket"));
}

#[test]
fn captured_entries_refuse_layout_and_buffer_width_mismatches() {
  let algorithm = HashAlgorithm::Blake3_256;
  let wrong_nvt = ReadSnapshot::new(HashMap::new(), make_nvt(2), 1, algorithm, 0, empty_pages(1, 32)).unwrap();
  assert!(wrong_nvt.visit_captured_entries(&CancellationToken::new(), 100, |_| Ok(true)).unwrap_err().to_string().contains("bucket"));
  let wrong_pages = ReadSnapshot::new(HashMap::new(), make_nvt(1), 1, algorithm, 0, empty_pages(2, 32)).unwrap();
  assert!(wrong_pages.visit_captured_entries(&CancellationToken::new(), 100, |_| Ok(true)).is_err());
  for width in [0, 31, 33, 64] {
    let entry = KVEntry { hash: vec![1; width], ..profile_entry(algorithm, 1) };
    let snapshot = one_page(algorithm, &[], HashMap::from([(entry.hash.clone(), entry)]), 1);
    assert!(snapshot.visit_captured_entries(&CancellationToken::new(), 100, |_| Ok(true)).unwrap_err().to_string().contains("buffer"));
  }
}

#[test]
fn captured_entries_do_not_visit_past_work_limits_or_hide_tombstone_work() {
  let algorithm = HashAlgorithm::Blake3_256;
  let first = profile_entry(algorithm, 1);
  let second = profile_entry(algorithm, 2);
  let snapshot = one_page(algorithm, &[first.clone(), second], HashMap::new(), 2);
  for budget in 0..=2 {
    let mut calls = 0;
    let error = snapshot
      .visit_captured_entries(&CancellationToken::new(), budget, |_| {
        calls += 1;
        Ok(true)
      })
      .unwrap_err();
    assert!(matches!(error, EngineError::ResourceExhausted(_)));
    assert_eq!(calls, budget.saturating_sub(1));
  }
  let tombstone = KVEntry { type_flags: KV_TYPE_CHUNK | KV_FLAG_DELETED, ..first };
  let snapshot = one_page(algorithm, std::slice::from_ref(&tombstone), HashMap::from([(tombstone.hash.clone(), tombstone.clone())]), 0);
  assert!(matches!(
    snapshot.visit_captured_entries(&CancellationToken::new(), 2, |_| panic!("deleted")),
    Err(EngineError::ResourceExhausted(_))
  ));
  assert!(snapshot.visit_captured_entries(&CancellationToken::new(), 3, |_| panic!("deleted")).unwrap().complete);
}

#[test]
fn captured_entries_propagate_late_page_damage_after_partial_callbacks() {
  let algorithm = HashAlgorithm::Blake3_256;
  let table = make_nvt(2);
  let first = (0u8..=255).map(|seed| profile_entry(algorithm, seed)).find(|entry| table.bucket_for_value(&entry.hash) == 0).unwrap();
  let snapshot = ReadSnapshot::new_with_page_type_counts(
    HashMap::new(),
    table,
    2,
    algorithm,
    1,
    Arc::new(vec![page_arc(serialize_page(&[first], 32)), page_arc(vec![0xff; page_size(32)])]),
    [0; 16],
  );
  let mut calls = 0;
  let error = snapshot
    .visit_captured_entries(&CancellationToken::new(), 100, |_| {
      calls += 1;
      Ok(true)
    })
    .unwrap_err();
  assert!(matches!(error, EngineError::CorruptEntry { .. }));
  assert_eq!(calls, 1, "a partial callback stream cannot imply completeness");
}

#[test]
fn captured_entries_check_limits_before_native_page_io_and_preserve_read_failure() {
  let directory = tempdir().unwrap();
  let file = OpenOptions::new().read(true).write(true).create_new(true).open(directory.path().join("truncated.aeordb")).unwrap();
  file.set_len(256).unwrap();
  let provider = KvPageProvider::new(file, 256, HashAlgorithm::Blake3_256, 1, 0, None).unwrap();
  let snapshot = ReadSnapshot::from_bounded_pages_with_type_counts(
    HashMap::new(),
    make_nvt(1),
    1,
    HashAlgorithm::Blake3_256,
    0,
    provider.snapshot().unwrap(),
    [0; 16],
  );
  assert!(matches!(snapshot.visit_captured_entries(&CancellationToken::new(), 0, |_| Ok(true)), Err(EngineError::ResourceExhausted(_))));
  assert_eq!(provider.stats().unwrap().read_failures, 0);
  let cancellation = CancellationToken::new();
  cancellation.cancel();
  assert!(matches!(snapshot.visit_captured_entries(&cancellation, 1, |_| Ok(true)), Err(EngineError::Cancelled(_))));
  assert_eq!(provider.stats().unwrap().read_failures, 0);
  assert!(snapshot.visit_captured_entries(&CancellationToken::new(), 1, |_| Ok(true)).is_err());
  assert_eq!(provider.stats().unwrap().read_failures, 1);
}

#[test]
fn captured_entries_retain_native_page_generations_and_allow_callback_writes() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let directory = tempdir().unwrap();
    let path = directory.path().join("captured-generations.aeordb");
    let mut file = OpenOptions::new().read(true).write(true).create_new(true).open(&path).unwrap();
    let first = profile_entry(algorithm, 1);
    let original = serialize_page(std::slice::from_ref(&first), algorithm.hash_length());
    file.write_all(&[0; 256]).unwrap();
    file.write_all(&original).unwrap();
    file.sync_all().unwrap();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(1 << 20, 2 << 20, 1, 1 << 18).unwrap());
    let provider = KvPageProvider::new(file.try_clone().unwrap(), 256, algorithm, 1, 0, Some(memory.clone())).unwrap();
    let old = ReadSnapshot::from_bounded_pages(HashMap::new(), make_nvt(1), 1, algorithm, 1, provider.snapshot().unwrap()).unwrap();
    let replacement = KVEntry { offset: 9999, ..first.clone() };
    let replacement_page = serialize_page(std::slice::from_ref(&replacement), algorithm.hash_length());
    let mut calls = 0;
    old
      .visit_captured_entries(&CancellationToken::new(), 2, |entry| {
        assert_eq!(entry, &first);
        calls += 1;
        let mut update = provider.begin_update(&[0]).unwrap();
        update.mark_overwrite_started().unwrap();
        file.seek(SeekFrom::Start(256)).unwrap();
        file.write_all(&replacement_page).unwrap();
        file.sync_all().unwrap();
        update.commit(vec![(0, page_arc(replacement_page.clone()))]).unwrap();
        Ok(true)
      })
      .unwrap();
    assert_eq!(calls, 1);
    let current = ReadSnapshot::from_bounded_pages(HashMap::new(), make_nvt(1), 1, algorithm, 1, provider.snapshot().unwrap()).unwrap();
    for (snapshot, expected) in [(&old, &first), (&current, &replacement)] {
      let mut calls = 0;
      assert!(
        snapshot
          .visit_captured_entries(&CancellationToken::new(), 2, |entry| {
            assert_eq!(entry, expected);
            calls += 1;
            Ok(true)
          })
          .unwrap()
          .complete
      );
      assert_eq!(calls, 1);
    }
    assert_eq!(provider.stats().unwrap().historical_pages, 1);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::KvSnapshotGenerations).unwrap().reserved_bytes, original.len() as u64);
    drop(old);
    assert_eq!(provider.stats().unwrap().historical_pages, 0);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::KvSnapshotGenerations).unwrap().reserved_bytes, 0);
  }
}

#[test]
fn captured_entries_never_allocate_a_world_sized_collection() {
  for count in [32usize, 8192] {
    let buckets = count / 8;
    let table = make_nvt(buckets);
    let mut entries = vec![Vec::new(); buckets];
    for seed in 0..count {
      let hash = blake3::hash(&(seed as u64).to_le_bytes()).as_bytes().to_vec();
      let bucket = table.bucket_for_value(&hash);
      entries[bucket].push(KVEntry { type_flags: KV_TYPE_CHUNK, hash, offset: seed as u64 * 100, total_length: 64 });
      assert!(entries[bucket].len() <= 32);
    }
    let pages = Arc::new(entries.iter().map(|entries| page_arc(serialize_page(entries, 32))).collect());
    let snapshot = ReadSnapshot::new(HashMap::new(), table, buckets, HashAlgorithm::Blake3_256, count, pages).unwrap();
    let cancellation = CancellationToken::new();
    let (summary, measured) =
      allocation_probe::measure(0, || snapshot.visit_captured_entries(&cancellation, (count + buckets) as u64, |_| Ok(true)));
    assert_eq!(summary.unwrap().visited_entries, count as u64);
    assert!(measured.maximum <= 4096, "largest allocation {} grew with {count} entries", measured.maximum);
  }
}
