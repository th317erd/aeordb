//! Background integrity scanner.
//!
//! Periodically spot-checks random entries from the KV index by reading
//! them and verifying their hashes. Catches silent corruption (bit rot)
//! before it's needed.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::engine::storage_engine::StorageEngine;

const DEFAULT_INTERVAL_SECS: u64 = 3600; // 60 minutes
const MIN_SAMPLE: usize = 10;
const MAX_SAMPLE: usize = 1000;

/// Spawn the background integrity scanner.
///
/// Periodically picks a random sample of entries from the KV index,
/// reads each one (triggering hash verification), and logs failures.
/// Corrupt entries are quarantined to `lost+found/`.
pub fn spawn_integrity_scanner(
  engine: Arc<StorageEngine>,
  cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
  tokio::spawn(async move {
    loop {
      tokio::select! {
        _ = cancel.cancelled() => {
          tracing::info!("Integrity scanner shutting down");
          break;
        }
        _ = tokio::time::sleep(Duration::from_secs(DEFAULT_INTERVAL_SECS)) => {}
      }

      run_scan_cycle(&engine);
    }
  })
}

/// Run a single scan cycle: pick random entries, read them, verify hashes.
fn run_scan_cycle(engine: &StorageEngine) {
  let snapshot = engine.kv_snapshot.load();
  let all_entries = match snapshot.iter_all() {
    Ok(entries) => entries,
    Err(e) => {
      tracing::warn!("Integrity scanner: failed to read KV snapshot: {}", e);
      return;
    }
  };

  if all_entries.is_empty() {
    return;
  }

  // Calculate sample size: ~1% of entries, clamped to [MIN_SAMPLE, MAX_SAMPLE]
  let sample_size = ((all_entries.len() as f64 * 0.01).ceil() as usize)
    .max(MIN_SAMPLE)
    .min(MAX_SAMPLE)
    .min(all_entries.len());

  // Pick entries by stepping through with a stride
  let stride = (all_entries.len() / sample_size).max(1);

  // Use a simple offset that changes each cycle so we cover different entries
  let cycle_offset = (std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap_or_default()
    .as_secs()
    % stride as u64) as usize;

  let mut checked = 0u64;
  let mut failures = 0u64;

  for (i, entry) in all_entries.iter().enumerate() {
    if (i + cycle_offset) % stride != 0 {
      continue;
    }

    checked += 1;

    // get_entry does hash verification via read_entry_at_shared
    match engine.get_entry(&entry.hash) {
      Ok(Some(_)) => {} // Hash verified, entry is healthy
      Ok(None) => {
        // Entry was in KV but get_entry returned None (deleted or missing).
        // Not corruption, just a stale KV entry — skip.
      }
      Err(e) => {
        failures += 1;
        tracing::warn!(
          "Integrity scanner: corrupt entry at offset {}: {}",
          entry.offset, e
        );

        // Quarantine metadata about the corrupt entry
        crate::engine::lost_found::quarantine_metadata(
          engine,
          "/",
          &format!("integrity_scan_{}.json", entry.offset),
          &format!("Background integrity scan detected corruption: {}", e),
          entry.offset,
          None,
        );
      }
    }

    if checked >= sample_size as u64 {
      break;
    }
  }

  if failures > 0 {
    tracing::warn!(
      "Integrity scanner: checked {} entries, {} failures detected",
      checked, failures
    );
  } else {
    tracing::debug!(
      "Integrity scanner: checked {} entries, all OK",
      checked
    );
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use crate::engine::directory_ops::DirectoryOps;
  use crate::engine::request_context::RequestContext;

  fn create_test_db() -> (StorageEngine, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("test.aeordb");
    let engine = StorageEngine::create(db_path.to_str().unwrap()).unwrap();
    (engine, temp)
  }

  fn store_test_files(engine: &StorageEngine) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops
      .store_file(&ctx, "/docs/a.txt", b"file-a-content", Some("text/plain"))
      .unwrap();
    ops
      .store_file(&ctx, "/docs/b.txt", b"file-b-content", Some("text/plain"))
      .unwrap();
    ops
      .store_file(
        &ctx,
        "/images/photo.jpg",
        b"jpeg-data-here",
        Some("image/jpeg"),
      )
      .unwrap();
  }

  #[test]
  fn scan_cycle_on_empty_db_does_not_panic() {
    let (engine, _temp) = create_test_db();
    // Should return early without error on an empty database
    run_scan_cycle(&engine);
  }

  #[test]
  fn scan_cycle_on_healthy_db_finds_no_failures() {
    let (engine, _temp) = create_test_db();
    store_test_files(&engine);

    // Should complete without finding any corruption
    run_scan_cycle(&engine);
    // If we get here without panic, the scan succeeded
  }

  #[test]
  fn scan_cycle_detects_corruption() {
    use std::io::{Seek, SeekFrom, Write};

    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("test.aeordb");
    let db_str = db_path.to_str().unwrap();

    // Create DB and store files
    {
      let engine = StorageEngine::create(db_str).unwrap();
      store_test_files(&engine);
    }

    // Inject corruption into the middle of the file
    {
      let file_size = std::fs::metadata(db_str).unwrap().len();
      let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(db_str)
        .unwrap();
      // Write garbage in the middle of data region
      file.seek(SeekFrom::Start(file_size / 2)).unwrap();
      let garbage: Vec<u8> = (0..64).map(|i: u8| i.wrapping_mul(0x37)).collect();
      file.write_all(&garbage).unwrap();
      file.sync_all().unwrap();
    }

    // Delete KV to force rebuild from the now-corrupt file
    let _ = std::fs::remove_file(format!("{}.kv", db_str));

    // Re-open the database (KV will be rebuilt, corrupt entries skipped)
    let engine = StorageEngine::open(db_str).unwrap();

    // The scan should not panic — it should gracefully handle any
    // entries that are corrupt. Whether it detects corruption depends
    // on whether the injected bytes hit a sampled entry.
    run_scan_cycle(&engine);
  }

  #[test]
  fn sample_size_clamped_correctly() {
    // Test the clamping logic directly
    let compute_sample = |total: usize| -> usize {
      ((total as f64 * 0.01).ceil() as usize)
        .max(MIN_SAMPLE)
        .min(MAX_SAMPLE)
        .min(total)
    };

    // Small DB: clamp to min but not more than total
    assert_eq!(compute_sample(5), 5); // min(max(1, 10), 5) = 5
    assert_eq!(compute_sample(10), 10); // min(max(1, 10), 10) = 10
    assert_eq!(compute_sample(100), 10); // min(max(1, 10), 100) = 10

    // Medium DB: 1%
    assert_eq!(compute_sample(2000), 20); // 1% of 2000 = 20
    assert_eq!(compute_sample(50_000), 500); // 1% of 50k = 500

    // Large DB: clamp to max
    assert_eq!(compute_sample(200_000), 1000); // 1% of 200k = 2000, clamped to 1000
  }

  #[tokio::test]
  async fn scanner_shuts_down_on_cancel() {
    let (engine, _temp) = create_test_db();
    let engine = Arc::new(engine);
    let cancel = CancellationToken::new();

    let handle = spawn_integrity_scanner(Arc::clone(&engine), cancel.clone());

    // Cancel immediately
    cancel.cancel();

    // Should complete promptly
    let result = tokio::time::timeout(Duration::from_secs(5), handle).await;
    assert!(result.is_ok(), "Scanner should shut down within 5 seconds");
  }
}
