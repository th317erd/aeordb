use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::errors::EngineError;
use aeordb::engine::kv_page_provider::KvPageProvider;
use aeordb::engine::kv_nvt::KvNvt;
use aeordb::engine::kv_pages::{MAX_ENTRIES_PER_PAGE, page_size, serialize_page};
use aeordb::engine::kv_snapshot::ReadSnapshot;
use aeordb::engine::kv_store::{KV_FLAG_DELETED, KV_TYPE_CHUNK, KVEntry};
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::gc_mark_runtime::{DenseMarkBitmapV1, MarkBitmapErrorV1, MarkSlotPositionV1};
use tokio_util::sync::CancellationToken;
use tempfile::tempdir;

fn memory_coordinator() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(64 * 1024 * 1024, 96 * 1024 * 1024, 1, 8 * 1024 * 1024).unwrap())
}

fn nvt(bucket_count: usize) -> Arc<KvNvt> {
  Arc::new(KvNvt::new(bucket_count))
}

fn entry_for_bucket(nvt: &KvNvt, bucket: usize, seed_start: u64, deleted: bool) -> KVEntry {
  for seed in seed_start..seed_start + 100_000 {
    let hash = blake3::hash(&seed.to_le_bytes()).as_bytes().to_vec();
    if nvt.bucket_for_value(&hash) == bucket {
      return KVEntry {
        type_flags: KV_TYPE_CHUNK | if deleted { KV_FLAG_DELETED } else { 0 },
        hash,
        offset: seed.saturating_mul(100),
        total_length: 64,
      };
    }
  }
  panic!("test could not find a hash for bucket {bucket}");
}

fn page_arc(entries: &[KVEntry], hash_length: usize) -> Arc<[u8]> {
  Arc::from(serialize_page(entries, hash_length).into_boxed_slice())
}

fn empty_page(hash_length: usize) -> Arc<[u8]> {
  Arc::from(vec![0; page_size(hash_length)].into_boxed_slice())
}

#[test]
fn dense_bitmap_reserves_exact_geometry_and_marks_idempotently() {
  let memory = memory_coordinator();
  let cancellation = CancellationToken::new();
  let mut bitmap = DenseMarkBitmapV1::new(4, MAX_ENTRIES_PER_PAGE as u32, cancellation, &memory).unwrap();

  assert_eq!(bitmap.bucket_count(), 4);
  assert_eq!(bitmap.slots_per_bucket(), MAX_ENTRIES_PER_PAGE as u32);
  assert_eq!(bitmap.bit_count(), 128);
  assert_eq!(bitmap.byte_count(), 16);
  assert_eq!(bitmap.marked_count(), 0);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes, 16);

  let first = MarkSlotPositionV1 { bucket_index: 0, slot_index: 0 };
  let last = MarkSlotPositionV1 { bucket_index: 3, slot_index: 31 };
  assert!(bitmap.mark(first).unwrap());
  assert!(!bitmap.mark(first).unwrap());
  assert!(bitmap.mark(last).unwrap());
  assert!(bitmap.is_marked(first).unwrap());
  assert!(bitmap.is_marked(last).unwrap());
  assert_eq!(bitmap.marked_count(), 2);
  assert_eq!(bitmap.bytes()[0], 1);
  assert_eq!(bitmap.bytes()[15], 0x80);

  assert_eq!(bitmap.mark(MarkSlotPositionV1 { bucket_index: 4, slot_index: 0 }).unwrap_err().code(), "mark_bitmap_position");
  assert_eq!(bitmap.mark(MarkSlotPositionV1 { bucket_index: 0, slot_index: 32 }).unwrap_err().code(), "mark_bitmap_position");

  drop(bitmap);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes, 0);
}

#[test]
fn bitmap_geometry_cancellation_and_memory_refusal_leave_no_accounting() {
  let memory = memory_coordinator();
  let cancellation = CancellationToken::new();
  for (buckets, slots) in [(0, 32), (1, 0), (1, 31), (u64::MAX, 32)] {
    assert_eq!(DenseMarkBitmapV1::new(buckets, slots, cancellation.clone(), &memory).unwrap_err().code(), "mark_bitmap_geometry");
  }
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes, 0);

  let canceled = CancellationToken::new();
  canceled.cancel();
  assert_eq!(DenseMarkBitmapV1::new(1, 32, canceled, &memory).unwrap_err().code(), "mark_bitmap_cancelled");
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes, 0);

  let constrained = MemoryCoordinator::new(MemoryPolicy::new(64, 128, 1, 16).unwrap());
  let error = DenseMarkBitmapV1::new(64, 32, CancellationToken::new(), &constrained).unwrap_err();
  assert!(matches!(error, MarkBitmapErrorV1::Memory(_)));
  let owner = constrained.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().clone();
  assert_eq!(owner.reserved_bytes, 0);
  assert_eq!(owner.active_reservations, 0);
}

#[test]
fn captured_slot_visitor_reports_exact_bucket_slot_identity_without_materializing() {
  let algorithm = HashAlgorithm::Blake3_256;
  let hash_length = algorithm.hash_length();
  let table = nvt(3);
  let first = entry_for_bucket(&table, 0, 1, false);
  let deleted = entry_for_bucket(&table, 0, 100_001, true);
  let second = entry_for_bucket(&table, 2, 200_001, false);
  let pages = Arc::new(vec![
    page_arc(&[first.clone(), deleted], hash_length),
    empty_page(hash_length),
    page_arc(std::slice::from_ref(&second), hash_length),
  ]);
  let snapshot = ReadSnapshot::new(HashMap::new(), table, 3, algorithm, 2, pages).unwrap();
  let cancellation = CancellationToken::new();
  let mut observed = Vec::new();

  let summary = snapshot
    .visit_captured_slots(&cancellation, |position, entry| {
      observed.push((position, entry.hash.clone()));
      Ok(true)
    })
    .unwrap();

  assert_eq!(summary.bucket_count, 3);
  assert_eq!(summary.slots_per_bucket, MAX_ENTRIES_PER_PAGE as u32);
  assert_eq!(summary.visited_slots, 2);
  assert_eq!(
    observed,
    vec![
      (MarkSlotPositionV1 { bucket_index: 0, slot_index: 0 }, first.hash.clone()),
      (MarkSlotPositionV1 { bucket_index: 2, slot_index: 0 }, second.hash.clone())
    ]
  );

  let (position, found) = snapshot.find_captured_slot(&second.hash).unwrap().unwrap();
  assert_eq!(position, MarkSlotPositionV1 { bucket_index: 2, slot_index: 0 });
  assert_eq!(found, second);
  assert!(snapshot.find_captured_slot(&[0xFF; 32]).unwrap().is_none());
}

#[test]
fn captured_slot_visitor_streams_file_backed_pages_with_a_one_page_cache_bound() {
  let algorithm = HashAlgorithm::Blake3_256;
  let hash_length = algorithm.hash_length();
  let table = nvt(4);
  let first = entry_for_bucket(&table, 0, 1, false);
  let second = entry_for_bucket(&table, 3, 100_001, false);
  let pages = [
    page_arc(std::slice::from_ref(&first), hash_length),
    empty_page(hash_length),
    empty_page(hash_length),
    page_arc(std::slice::from_ref(&second), hash_length),
  ];
  let directory = tempdir().unwrap();
  let path = directory.path().join("captured-pages.aeordb");
  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(&path).unwrap();
  file.write_all(&[0; 256]).unwrap();
  for page in pages {
    file.write_all(&page).unwrap();
  }
  file.sync_all().unwrap();

  let page_bytes = u64::try_from(page_size(hash_length)).unwrap();
  let provider = KvPageProvider::new(file, 256, algorithm, 4, page_bytes, None).unwrap();
  let snapshot = ReadSnapshot::from_bounded_pages(HashMap::new(), table, 4, algorithm, 2, provider.snapshot().unwrap()).unwrap();
  let reads_before_visit = provider.stats().unwrap().disk_reads;
  let mut observed = Vec::new();
  let summary = snapshot
    .visit_captured_slots(&CancellationToken::new(), |position, entry| {
      observed.push((position, entry.hash.clone()));
      Ok(true)
    })
    .unwrap();

  assert!(summary.complete);
  assert_eq!(summary.visited_slots, 2);
  assert_eq!(
    observed,
    vec![
      (MarkSlotPositionV1 { bucket_index: 0, slot_index: 0 }, first.hash),
      (MarkSlotPositionV1 { bucket_index: 3, slot_index: 0 }, second.hash),
    ]
  );
  let stats = provider.stats().unwrap();
  assert_eq!(stats.disk_reads - reads_before_visit, 4);
  assert!(stats.resident_pages <= 1);
  assert!(stats.resident_bytes <= page_bytes);
}

#[test]
fn captured_slot_scans_refuse_buffers_cancellation_and_wrong_bucket_entries() {
  let algorithm = HashAlgorithm::Blake3_256;
  let hash_length = algorithm.hash_length();
  let table = nvt(2);
  let entry = entry_for_bucket(&table, 0, 1, false);
  let pages = Arc::new(vec![page_arc(std::slice::from_ref(&entry), hash_length), empty_page(hash_length)]);

  let mut buffer = HashMap::new();
  buffer.insert(entry.hash.clone(), entry.clone());
  let buffered = ReadSnapshot::new(buffer, Arc::clone(&table), 2, algorithm, 1, Arc::clone(&pages)).unwrap();
  assert!(buffered.visit_captured_slots(&CancellationToken::new(), |_, _| Ok(true)).unwrap_err().to_string().contains("flushed"));
  assert!(buffered.find_captured_slot(&entry.hash).unwrap_err().to_string().contains("flushed"));

  let clean = ReadSnapshot::new(HashMap::new(), Arc::clone(&table), 2, algorithm, 1, pages).unwrap();
  let canceled = CancellationToken::new();
  canceled.cancel();
  let mut callbacks = 0;
  assert!(clean
    .visit_captured_slots(&canceled, |_, _| {
      callbacks += 1;
      Ok(true)
    })
    .unwrap_err()
    .to_string()
    .contains("cancel"));
  assert_eq!(callbacks, 0);

  let wrong_bucket_pages = Arc::new(vec![empty_page(hash_length), page_arc(std::slice::from_ref(&entry), hash_length)]);
  let wrong_bucket = ReadSnapshot::new(HashMap::new(), table, 2, algorithm, 1, wrong_bucket_pages).unwrap();
  assert!(wrong_bucket.visit_captured_slots(&CancellationToken::new(), |_, _| Ok(true)).unwrap_err().to_string().contains("bucket"));
  assert!(wrong_bucket.find_captured_slot(&entry.hash).unwrap().is_none());
}

#[test]
fn captured_slot_scans_preserve_stop_error_and_snapshot_closure_semantics() {
  let algorithm = HashAlgorithm::Blake3_256;
  let hash_length = algorithm.hash_length();
  let table = nvt(1);
  let first = entry_for_bucket(&table, 0, 1, false);
  let second = entry_for_bucket(&table, 0, 100_001, false);
  let pages = Arc::new(vec![page_arc(&[first.clone(), second.clone()], hash_length)]);
  let snapshot = ReadSnapshot::new(HashMap::new(), Arc::clone(&table), 1, algorithm, 2, Arc::clone(&pages)).unwrap();

  let stopped = snapshot.visit_captured_slots(&CancellationToken::new(), |_, _| Ok(false)).unwrap();
  assert!(!stopped.complete);
  assert_eq!(stopped.visited_slots, 1);

  let callback_error = snapshot
    .visit_captured_slots(&CancellationToken::new(), |_, _| Err(EngineError::InvalidInput("injected visitor failure".to_string())))
    .unwrap_err();
  assert!(callback_error.to_string().contains("injected visitor failure"));

  let cancellation = CancellationToken::new();
  let mut callbacks = 0;
  let error = snapshot
    .visit_captured_slots(&cancellation, |_, _| {
      callbacks += 1;
      cancellation.cancel();
      Ok(true)
    })
    .unwrap_err();
  assert!(error.to_string().contains("cancel"));
  assert_eq!(callbacks, 1);

  assert!(snapshot.find_captured_slot(&[0x11; 31]).unwrap_err().to_string().contains("width"));

  let wrong_count = ReadSnapshot::new(HashMap::new(), Arc::clone(&table), 1, algorithm, 3, Arc::clone(&pages)).unwrap();
  assert!(wrong_count.visit_captured_slots(&CancellationToken::new(), |_, _| Ok(true)).unwrap_err().to_string().contains("count"));

  let wrong_layout = ReadSnapshot::new(HashMap::new(), nvt(2), 1, algorithm, 2, pages).unwrap();
  assert!(wrong_layout.visit_captured_slots(&CancellationToken::new(), |_, _| Ok(true)).unwrap_err().to_string().contains("bucket count"));

  let duplicate_pages = Arc::new(vec![page_arc(&[first.clone(), first.clone()], hash_length)]);
  let duplicate = ReadSnapshot::new(HashMap::new(), table, 1, algorithm, 2, duplicate_pages).unwrap();
  assert!(duplicate.visit_captured_slots(&CancellationToken::new(), |_, _| Ok(true)).unwrap_err().to_string().contains("duplicate"));
  assert!(duplicate.find_captured_slot(&first.hash).unwrap_err().to_string().contains("duplicate"));
}

#[test]
fn canceled_bitmap_refuses_later_marks_without_changing_bits() {
  let memory = memory_coordinator();
  let cancellation = CancellationToken::new();
  let mut bitmap = DenseMarkBitmapV1::new(1, 32, cancellation.clone(), &memory).unwrap();
  cancellation.cancel();

  assert_eq!(bitmap.mark(MarkSlotPositionV1 { bucket_index: 0, slot_index: 0 }).unwrap_err().code(), "mark_bitmap_cancelled");
  assert_eq!(bitmap.marked_count(), 0);
  assert_eq!(bitmap.bytes(), &[0; 4]);
}

fn rust_sources(root: &Path, sources: &mut Vec<PathBuf>) {
  for entry in fs::read_dir(root).unwrap() {
    let path = entry.unwrap().path();
    if path.is_dir() {
      rust_sources(&path, sources);
    } else if path.extension().is_some_and(|extension| extension == "rs") {
      sources.push(path);
    }
  }
}

#[test]
fn mark_runtime_remains_disconnected_from_live_gc_service_and_control_paths() {
  let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let bitmap_path = source_root.join("engine/v4/gc_mark_runtime.rs");
  let snapshot_path = source_root.join("engine/kv_snapshot.rs");
  let mut sources = Vec::new();
  rust_sources(&source_root, &mut sources);

  let callers: Vec<_> = sources
    .into_iter()
    .filter(|path| path != &bitmap_path && path != &snapshot_path)
    .filter(|path| {
      let source = fs::read_to_string(path).unwrap_or_default();
      source.contains("DenseMarkBitmapV1") || source.contains("visit_captured_slots") || source.contains("find_captured_slot")
    })
    .map(|path| path.strip_prefix(&source_root).unwrap().to_owned())
    .collect();
  assert!(callers.is_empty(), "P4-3 mark runtime activated before its owner gate: {callers:?}");

  let bitmap_source = fs::read_to_string(bitmap_path).unwrap();
  for forbidden in ["engine::gc", "VoidManager", "V4ControlStore", "candidate", "sweep", "authorizes_reclaim"] {
    assert!(!bitmap_source.contains(forbidden), "mark runtime contains forbidden live/reclaim token {forbidden}");
  }
}
