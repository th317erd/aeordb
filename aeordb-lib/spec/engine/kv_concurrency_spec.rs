use std::sync::Arc;
use std::thread;

use aeordb::engine::{DirectoryOps, RequestContext};
use aeordb::server::create_temp_engine_for_tests;

// ─── Concurrent readers ─────────────────────────────────────────────────────

#[test]
fn test_concurrent_readers_dont_block() {
  let ctx = RequestContext::system();
  let (engine, _temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);

  // Seed with 20 files
  for i in 0..20 {
    let path = format!("/files/doc-{}.txt", i);
    let content = format!("content-for-file-{}", i);
    ops.store_file_buffered(&ctx, &path, content.as_bytes(), Some("text/plain")).unwrap();
  }

  // Spawn 8 reader threads, each reading all 20 files
  let mut handles = Vec::new();
  for thread_id in 0..8 {
    let engine_clone = Arc::clone(&engine);
    let handle = thread::spawn(move || {
      let ops = DirectoryOps::new(&engine_clone);
      for i in 0..20 {
        let path = format!("/files/doc-{}.txt", i);
        let data = ops.read_file_buffered(&path)
          .unwrap_or_else(|e| panic!("thread {} failed to read {}: {:?}", thread_id, path, e));
        let expected = format!("content-for-file-{}", i);
        assert_eq!(
          data, expected.as_bytes(),
          "thread {} got wrong content for {}", thread_id, path,
        );
      }
    });
    handles.push(handle);
  }

  for handle in handles {
    handle.join().expect("reader thread panicked");
  }
}

// ─── Readers and writer concurrent ──────────────────────────────────────────

#[test]
fn test_readers_and_writer_concurrent() {
  let ctx = RequestContext::system();
  let (engine, _temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);

  // Seed with 10 files
  for i in 0..10 {
    let path = format!("/seed/file-{}.txt", i);
    let content = format!("seed-content-{}", i);
    ops.store_file_buffered(&ctx, &path, content.as_bytes(), Some("text/plain")).unwrap();
  }

  let mut handles = Vec::new();

  // 4 reader threads, each reading seed files 50 times
  for thread_id in 0..4 {
    let engine_clone = Arc::clone(&engine);
    let handle = thread::spawn(move || {
      let ops = DirectoryOps::new(&engine_clone);
      for _iteration in 0..50 {
        for i in 0..10 {
          let path = format!("/seed/file-{}.txt", i);
          let data = ops.read_file_buffered(&path)
            .unwrap_or_else(|e| panic!("reader {} failed on {}: {:?}", thread_id, path, e));
          let expected = format!("seed-content-{}", i);
          assert_eq!(
            data, expected.as_bytes(),
            "reader {} got wrong content for {}", thread_id, path,
          );
        }
      }
    });
    handles.push(handle);
  }

  // 1 writer thread adding 50 new files
  {
    let engine_clone = Arc::clone(&engine);
    let handle = thread::spawn(move || {
      let ctx = RequestContext::system();
      let ops = DirectoryOps::new(&engine_clone);
      for i in 0..50 {
        let path = format!("/new/written-{}.txt", i);
        let content = format!("new-content-{}", i);
        ops.store_file_buffered(&ctx, &path, content.as_bytes(), Some("text/plain"))
          .unwrap_or_else(|e| panic!("writer failed on {}: {:?}", path, e));
      }
    });
    handles.push(handle);
  }

  for handle in handles {
    handle.join().expect("thread panicked");
  }

  // Verify all 50 new files exist and have correct content
  let ops = DirectoryOps::new(&engine);
  for i in 0..50 {
    let path = format!("/new/written-{}.txt", i);
    let data = ops.read_file_buffered(&path)
      .unwrap_or_else(|e| panic!("post-join read failed for {}: {:?}", path, e));
    let expected = format!("new-content-{}", i);
    assert_eq!(data, expected.as_bytes(), "wrong content for {}", path);
  }
}

// ─── Long reader doesn't block writer ───────────────────────────────────────

#[test]
fn test_long_reader_doesnt_block_writer() {
  let ctx = RequestContext::system();
  let (engine, _temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);

  // Seed with 100 files
  for i in 0..100 {
    let path = format!("/bulk/item-{}.txt", i);
    let content = format!("bulk-content-{}", i);
    ops.store_file_buffered(&ctx, &path, content.as_bytes(), Some("text/plain")).unwrap();
  }

  let mut handles = Vec::new();

  // Reader thread: call iter_kv_entries() 10 times (simulating GC mark scan)
  {
    let engine_clone = Arc::clone(&engine);
    let handle = thread::spawn(move || {
      for iteration in 0..10 {
        let entries = engine_clone.iter_kv_entries()
          .unwrap_or_else(|e| panic!("iter_kv_entries failed on iteration {}: {:?}", iteration, e));
        // Should have a reasonable number of entries from our 100 seed files
        assert!(
          !entries.is_empty(),
          "iter_kv_entries returned empty on iteration {}", iteration,
        );
      }
    });
    handles.push(handle);
  }

  // Writer thread: add 50 more files concurrently
  {
    let engine_clone = Arc::clone(&engine);
    let handle = thread::spawn(move || {
      let ctx = RequestContext::system();
      let ops = DirectoryOps::new(&engine_clone);
      for i in 0..50 {
        let path = format!("/extra/added-{}.txt", i);
        let content = format!("extra-content-{}", i);
        ops.store_file_buffered(&ctx, &path, content.as_bytes(), Some("text/plain"))
          .unwrap_or_else(|e| panic!("writer failed on {}: {:?}", path, e));
      }
    });
    handles.push(handle);
  }

  for handle in handles {
    handle.join().expect("thread panicked");
  }

  // Both completed without blocking -- verify the new files exist
  let ops = DirectoryOps::new(&engine);
  for i in 0..50 {
    let path = format!("/extra/added-{}.txt", i);
    let data = ops.read_file_buffered(&path)
      .unwrap_or_else(|e| panic!("post-join read failed for {}: {:?}", path, e));
    let expected = format!("extra-content-{}", i);
    assert_eq!(data, expected.as_bytes(), "wrong content for {}", path);
  }
}

// ─── Snapshot isolation during writes ───────────────────────────────────────

#[test]
fn test_snapshot_isolation_during_write() {
  let ctx = RequestContext::system();
  let (engine, _temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);

  // Store a file, verify it exists via has_entry
  ops.store_file_buffered(&ctx, "/alpha.txt", b"alpha-data", Some("text/plain")).unwrap();
  let alpha_hash = engine.compute_hash(b"file:/alpha.txt").unwrap();
  assert!(
    engine.has_entry(&alpha_hash).unwrap(),
    "alpha.txt should exist after store",
  );

  // Read it back to confirm data integrity
  let alpha_data = ops.read_file_buffered("/alpha.txt").unwrap();
  assert_eq!(alpha_data, b"alpha-data");

  // Store another file, verify both exist
  ops.store_file_buffered(&ctx, "/beta.txt", b"beta-data", Some("text/plain")).unwrap();
  let beta_hash = engine.compute_hash(b"file:/beta.txt").unwrap();
  assert!(
    engine.has_entry(&beta_hash).unwrap(),
    "beta.txt should exist after store",
  );
  assert!(
    engine.has_entry(&alpha_hash).unwrap(),
    "alpha.txt should still exist after storing beta.txt",
  );

  // Read both back
  let alpha_data = ops.read_file_buffered("/alpha.txt").unwrap();
  assert_eq!(alpha_data, b"alpha-data");
  let beta_data = ops.read_file_buffered("/beta.txt").unwrap();
  assert_eq!(beta_data, b"beta-data");
}

// ─── No data corruption under contention ────────────────────────────────────

#[test]
fn test_no_data_corruption_under_contention() {
  let ctx = RequestContext::system();
  let (engine, _temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);

  // Write 20 files with known content patterns
  for i in 0..20 {
    let path = format!("/verified/entry-{}.txt", i);
    let content = format!("exact-content-{}", i);
    ops.store_file_buffered(&ctx, &path, content.as_bytes(), Some("text/plain")).unwrap();
  }

  // Spawn 4 reader threads that verify content integrity 20 times each
  let mut handles = Vec::new();
  for thread_id in 0..4 {
    let engine_clone = Arc::clone(&engine);
    let handle = thread::spawn(move || {
      let ops = DirectoryOps::new(&engine_clone);
      for _iteration in 0..20 {
        for i in 0..20 {
          let path = format!("/verified/entry-{}.txt", i);
          let data = ops.read_file_buffered(&path)
            .unwrap_or_else(|e| panic!(
              "thread {} failed to read {} on iteration {}: {:?}",
              thread_id, path, _iteration, e,
            ));
          let expected = format!("exact-content-{}", i);
          assert_eq!(
            data, expected.as_bytes(),
            "DATA CORRUPTION: thread {} got wrong content for {} on iteration {}. \
             Expected {:?}, got {:?}",
            thread_id, path, _iteration,
            expected.as_bytes(), data,
          );
        }
      }
    });
    handles.push(handle);
  }

  for handle in handles {
    handle.join().expect("reader thread panicked -- possible data corruption");
  }
}

// ─── Edge cases ─────────────────────────────────────────────────────────────

#[test]
fn test_concurrent_readers_on_same_file() {
  let ctx = RequestContext::system();
  let (engine, _temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);

  let big_content = vec![0xABu8; 4096];
  ops.store_file_buffered(&ctx, "/shared/big.bin", &big_content, Some("application/octet-stream")).unwrap();

  // 8 threads all reading the exact same file 100 times
  let mut handles = Vec::new();
  for thread_id in 0..8 {
    let engine_clone = Arc::clone(&engine);
    let expected = big_content.clone();
    let handle = thread::spawn(move || {
      let ops = DirectoryOps::new(&engine_clone);
      for iteration in 0..100 {
        let data = ops.read_file_buffered("/shared/big.bin")
          .unwrap_or_else(|e| panic!(
            "thread {} failed read iteration {}: {:?}", thread_id, iteration, e,
          ));
        assert_eq!(
          data, expected,
          "thread {} got corrupted data on iteration {}", thread_id, iteration,
        );
      }
    });
    handles.push(handle);
  }

  for handle in handles {
    handle.join().expect("reader thread panicked");
  }
}

#[test]
fn test_writer_doesnt_corrupt_concurrent_reader_results() {
  let ctx = RequestContext::system();
  let (engine, _temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);

  // Seed a file that readers will continuously verify
  ops.store_file_buffered(&ctx, "/stable.txt", b"stable-content", Some("text/plain")).unwrap();

  let mut handles = Vec::new();

  // 4 readers verify the stable file doesn't get corrupted while writes happen
  for thread_id in 0..4 {
    let engine_clone = Arc::clone(&engine);
    let handle = thread::spawn(move || {
      let ops = DirectoryOps::new(&engine_clone);
      for iteration in 0..100 {
        let data = ops.read_file_buffered("/stable.txt")
          .unwrap_or_else(|e| panic!(
            "reader {} failed on iteration {}: {:?}", thread_id, iteration, e,
          ));
        assert_eq!(
          data, b"stable-content",
          "reader {} got wrong data on iteration {} while writer was active",
          thread_id, iteration,
        );
      }
    });
    handles.push(handle);
  }

  // 1 writer adding unrelated files concurrently
  {
    let engine_clone = Arc::clone(&engine);
    let handle = thread::spawn(move || {
      let ctx = RequestContext::system();
      let ops = DirectoryOps::new(&engine_clone);
      for i in 0..100 {
        let path = format!("/noise/file-{}.txt", i);
        ops.store_file_buffered(&ctx, &path, b"noise", Some("text/plain"))
          .unwrap_or_else(|e| panic!("writer failed on iteration {}: {:?}", i, e));
      }
    });
    handles.push(handle);
  }

  for handle in handles {
    handle.join().expect("thread panicked");
  }
}

#[test]
fn test_has_entry_concurrent_with_writes() {
  let ctx = RequestContext::system();
  let (engine, _temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);

  // Seed files and collect their hashes
  let mut hashes = Vec::new();
  for i in 0..10 {
    let path = format!("/check/item-{}.txt", i);
    ops.store_file_buffered(&ctx, &path, format!("data-{}", i).as_bytes(), Some("text/plain")).unwrap();
    let hash = engine.compute_hash(format!("file:/check/item-{}.txt", i).as_bytes()).unwrap();
    hashes.push(hash);
  }

  let mut handles = Vec::new();

  // Readers checking has_entry concurrently
  for thread_id in 0..4 {
    let engine_clone = Arc::clone(&engine);
    let hashes_clone = hashes.clone();
    let handle = thread::spawn(move || {
      for iteration in 0..50 {
        for (i, hash) in hashes_clone.iter().enumerate() {
          let exists = engine_clone.has_entry(hash)
            .unwrap_or_else(|e| panic!(
              "thread {} has_entry failed for item {} on iteration {}: {:?}",
              thread_id, i, iteration, e,
            ));
          assert!(
            exists,
            "thread {} found item {} missing on iteration {}",
            thread_id, i, iteration,
          );
        }
      }
    });
    handles.push(handle);
  }

  // Writer adding new files concurrently
  {
    let engine_clone = Arc::clone(&engine);
    let handle = thread::spawn(move || {
      let ctx = RequestContext::system();
      let ops = DirectoryOps::new(&engine_clone);
      for i in 0..50 {
        let path = format!("/other/new-{}.txt", i);
        ops.store_file_buffered(&ctx, &path, b"new", Some("text/plain"))
          .unwrap_or_else(|e| panic!("writer failed on {}: {:?}", path, e));
      }
    });
    handles.push(handle);
  }

  for handle in handles {
    handle.join().expect("thread panicked");
  }
}
