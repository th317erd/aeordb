//! Crash-injection soak worker.
//!
//! Spawned as a subprocess by the crash-injection test suite. Runs a workload
//! against an AeorDB database, appending every successfully-committed key to
//! a checkpoint file so the parent test can verify recovery afterwards.
//!
//! The worker is expected to be SIGKILL'd at a random moment; nothing graceful
//! happens on shutdown.
//!
//! ```
//! crash-soak-worker --database <path> --checkpoint <path> [--mode <kind>]
//! ```
//!
//! Modes:
//! - `writes` (default): tight loop of `store_file` on fresh paths
//! - `mixed`: writes + deletes + occasional snapshots
//! - `gc`: overwrites a bounded file set and periodically triggers GC
//! - `stress`: tiny JSON batch writes, JSON merges, snapshots, and GC

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process;
use std::time::Duration;

use aeordb::engine::{gc, BufferedFile, DirectoryOps, JsonMergeFilePatch, MergeDepth, RequestContext, StorageEngine, VersionManager};
use serde_json::json;

fn main() {
  let args: Vec<String> = std::env::args().collect();
  let mut database: Option<String> = None;
  let mut checkpoint: Option<String> = None;
  let mut mode: String = "writes".to_string();

  let mut i = 1;
  while i < args.len() {
    match args[i].as_str() {
      "--database" => {
        database = args.get(i + 1).cloned();
        i += 2;
      }
      "--checkpoint" => {
        checkpoint = args.get(i + 1).cloned();
        i += 2;
      }
      "--mode" => {
        mode = args.get(i + 1).cloned().unwrap_or_default();
        i += 2;
      }
      _ => {
        i += 1;
      }
    }
  }

  let database = match database {
    Some(value) => value,
    None => {
      eprintln!("--database required");
      process::exit(2);
    }
  };
  let checkpoint = match checkpoint {
    Some(value) => value,
    None => {
      eprintln!("--checkpoint required");
      process::exit(2);
    }
  };

  let engine = match StorageEngine::open(&database) {
    Ok(engine) => engine,
    Err(error) => {
      if !Path::new(&database).exists() {
        match StorageEngine::create(&database) {
          Ok(engine) => engine,
          Err(create_error) => {
            eprintln!("create failed: {}", create_error);
            process::exit(3);
          }
        }
      } else {
        eprintln!("open failed: {}", error);
        process::exit(3);
      }
    }
  };

  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();
  let mut checkpoint_file = OpenOptions::new().create(true).append(true).open(&checkpoint).expect("open checkpoint");

  // Signal that we're up so the parent knows the engine opened cleanly.
  writeln!(checkpoint_file, "# worker up mode={}", mode).ok();
  checkpoint_file.flush().ok();

  let mut counter: u64 = 0;
  loop {
    if mode == "stress" {
      if let Err(error) = run_stress_iteration(&engine, &ops, &ctx, &mut checkpoint_file, counter) {
        eprintln!("stress iteration failed: {}", error);
        process::exit(5);
      }
      counter += 1;
      if counter.is_multiple_of(32) {
        std::thread::sleep(Duration::from_micros(50));
      }
      continue;
    }

    let (path, body, content_type) = if mode == "gc" {
      let path = format!("/gc/file-{:04}.json", counter % 256);
      let body = json!({
        "counter": counter,
        "slot": counter % 256,
        "payload": fastish_hash(counter),
      })
      .to_string();
      (path, body, "application/json")
    } else {
      let path = format!("/data/file-{:08}.txt", counter);
      let body = format!("entry {} payload {}", counter, fastish_hash(counter));
      (path, body, "text/plain")
    };

    let result = ops.store_file_buffered(&ctx, &path, body.as_bytes(), Some(content_type));
    match result {
      Ok(_) => {
        // Record the commit AFTER the engine reports success. fsync on the
        // checkpoint file is what gives the parent its "definitely committed"
        // guarantee.
        if let Err(error) = checkpoint_committed(&mut checkpoint_file, &path, &body) {
          eprintln!("{}", error);
          process::exit(4);
        }
      }
      Err(error) => {
        eprintln!("store_file failed: {}", error);
        process::exit(5);
      }
    }

    if mode == "mixed" {
      // Occasional delete of an older entry to mix the workload.
      if counter > 0 && counter.is_multiple_of(7) {
        let target = format!("/data/file-{:08}.txt", counter - 5);
        if ops.delete_file(&ctx, &target).is_ok() {
          if let Err(error) = checkpoint_deleted(&mut checkpoint_file, &target) {
            eprintln!("{}", error);
            process::exit(4);
          }
        }
      }
    }

    if mode == "gc" && counter > 0 && counter.is_multiple_of(64) {
      if let Err(error) = gc::run_gc(&engine, &ctx, false) {
        eprintln!("gc failed: {}", error);
        process::exit(6);
      }
    }

    counter += 1;

    // Tiny sleep to keep CPU usage bounded and give the OS time to schedule
    // a SIGKILL from the parent. Without this, on very fast disks the worker
    // can write tens of thousands of entries per second and never get
    // interrupted at an "interesting" moment.
    if counter.is_multiple_of(32) {
      std::thread::sleep(Duration::from_micros(50));
    }
  }
}

fn checkpoint_committed(checkpoint_file: &mut File, path: &str, body: &str) -> Result<(), String> {
  writeln!(checkpoint_file, "{}\t{}", path, body).map_err(|error| format!("checkpoint write failed: {}", error))?;
  checkpoint_file.flush().map_err(|error| format!("checkpoint flush failed: {}", error))?;
  // Best-effort sync; if the kernel ignores us mid-test that's not a worker
  // bug. The parent verifies what the engine actually persisted.
  let _ = checkpoint_file.sync_data();
  Ok(())
}

fn checkpoint_deleted(checkpoint_file: &mut File, path: &str) -> Result<(), String> {
  writeln!(checkpoint_file, "-\t{}", path).map_err(|error| format!("checkpoint delete write failed: {}", error))?;
  checkpoint_file.flush().map_err(|error| format!("checkpoint delete flush failed: {}", error))?;
  let _ = checkpoint_file.sync_data();
  Ok(())
}

fn run_stress_iteration(
  engine: &StorageEngine,
  ops: &DirectoryOps<'_>,
  ctx: &RequestContext,
  checkpoint_file: &mut File,
  counter: u64,
) -> Result<(), String> {
  match counter % 10 {
    0 => {
      let mut files = Vec::with_capacity(16);
      let mut committed = Vec::with_capacity(16);
      for item in 0..16u64 {
        let path = format!("/stress/batches/{:04}/file-{:08}-{:02}.json", counter % 512, counter, item);
        let body = json!({
          "counter": counter,
          "item": item,
          "hash": fastish_hash(counter ^ item),
        })
        .to_string();
        files.push(BufferedFile { path: path.clone(), data: body.as_bytes().to_vec(), content_type: Some("application/json".to_string()) });
        committed.push((path, body));
      }
      ops.store_files_buffered_batch(ctx, files).map_err(|error| format!("batch store: {}", error))?;
      for (path, body) in committed {
        checkpoint_committed(checkpoint_file, &path, &body)?;
      }
    }
    1 | 2 | 3 | 4 => {
      let path = format!("/stress/state/doc-{:03}.json", counter % 64);
      ops
        .merge_json_file(ctx, &path, stress_patch(counter, counter % 64), MergeDepth::Unbounded)
        .map_err(|error| format!("merge_json_file {}: {}", path, error))?;
      checkpoint_current_json(ops, checkpoint_file, &path)?;
    }
    5 | 6 => {
      let mut patches = Vec::with_capacity(8);
      let mut paths = Vec::with_capacity(8);
      for item in 0..8u64 {
        let path = format!("/stress/batch-merge/doc-{:03}.json", (counter + item) % 96);
        patches.push(JsonMergeFilePatch { path: path.clone(), patch: stress_patch(counter, item), depth: MergeDepth::Unbounded });
        paths.push(path);
      }
      ops.merge_json_files_batch(ctx, patches).map_err(|error| format!("merge_json_files_batch: {}", error))?;
      for path in paths {
        checkpoint_current_json(ops, checkpoint_file, &path)?;
      }
    }
    7 | 8 => {
      let path = format!("/stress/overwrites/file-{:03}.json", counter % 128);
      let body = json!({
        "counter": counter,
        "slot": counter % 128,
        "overwritten": true,
        "hash": fastish_hash(counter),
      })
      .to_string();
      ops
        .store_file_buffered(ctx, &path, body.as_bytes(), Some("application/json"))
        .map_err(|error| format!("overwrite {}: {}", path, error))?;
      checkpoint_committed(checkpoint_file, &path, &body)?;
    }
    _ => {
      let path = format!("/stress/tiny/file-{:08}.json", counter);
      let body = json!({
        "counter": counter,
        "tiny": true,
        "hash": fastish_hash(counter),
      })
      .to_string();
      ops
        .store_file_buffered(ctx, &path, body.as_bytes(), Some("application/json"))
        .map_err(|error| format!("tiny store {}: {}", path, error))?;
      checkpoint_committed(checkpoint_file, &path, &body)?;
    }
  }

  if counter > 0 && counter.is_multiple_of(97) {
    gc::run_gc(engine, ctx, false).map_err(|error| format!("stress gc: {}", error))?;
  }

  if counter > 0 && counter.is_multiple_of(131) {
    let version_manager = VersionManager::new(engine);
    let snapshot_name = format!("stress-{}-{}", process::id(), counter);
    version_manager
      .create_snapshot(ctx, &snapshot_name, std::collections::HashMap::new())
      .map_err(|error| format!("stress snapshot {}: {}", snapshot_name, error))?;
  }

  Ok(())
}

fn stress_patch(counter: u64, slot: u64) -> serde_json::Value {
  let mut updates = serde_json::Map::new();
  updates.insert(format!("{:08}", counter), json!({ "slot": slot, "hash": fastish_hash(counter ^ slot) }));
  json!({
    "last_counter": counter,
    "last_slot": slot,
    "updates": updates,
  })
}

fn checkpoint_current_json(ops: &DirectoryOps<'_>, checkpoint_file: &mut File, path: &str) -> Result<(), String> {
  let body = ops.read_file_buffered(path).map_err(|error| format!("read merged {}: {}", path, error))?;
  let body = String::from_utf8(body).map_err(|error| format!("merged file {} was not UTF-8: {}", path, error))?;
  checkpoint_committed(checkpoint_file, path, &body)
}

fn fastish_hash(value: u64) -> u64 {
  // Simple non-crypto mixer for variety in the payload.
  let mut state = value.wrapping_mul(0x9E37_79B9_7F4A_7C15);
  state ^= state >> 33;
  state = state.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
  state ^= state >> 29;
  state
}
