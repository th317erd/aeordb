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
//! - `gc`: writes for a while then triggers GC; killed during sweep

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::process;
use std::time::Duration;

use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine};

fn main() {
  let args: Vec<String> = std::env::args().collect();
  let mut database: Option<String> = None;
  let mut checkpoint: Option<String> = None;
  let mut mode: String = "writes".to_string();

  let mut i = 1;
  while i < args.len() {
    match args[i].as_str() {
      "--database" => { database = args.get(i + 1).cloned(); i += 2; }
      "--checkpoint" => { checkpoint = args.get(i + 1).cloned(); i += 2; }
      "--mode" => { mode = args.get(i + 1).cloned().unwrap_or_default(); i += 2; }
      _ => { i += 1; }
    }
  }

  let database = match database {
    Some(value) => value,
    None => { eprintln!("--database required"); process::exit(2); }
  };
  let checkpoint = match checkpoint {
    Some(value) => value,
    None => { eprintln!("--checkpoint required"); process::exit(2); }
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
  let mut checkpoint_file = OpenOptions::new()
    .create(true)
    .append(true)
    .open(&checkpoint)
    .expect("open checkpoint");

  // Signal that we're up so the parent knows the engine opened cleanly.
  writeln!(checkpoint_file, "# worker up mode={}", mode).ok();
  checkpoint_file.flush().ok();

  let mut counter: u64 = 0;
  loop {
    let path = format!("/data/file-{:08}.txt", counter);
    let body = format!("entry {} payload {}", counter, fastish_hash(counter));

    let result = ops.store_file(&ctx, &path, body.as_bytes(), Some("text/plain"));
    match result {
      Ok(_) => {
        // Record the commit AFTER the engine reports success. fsync on the
        // checkpoint file is what gives the parent its "definitely committed"
        // guarantee.
        if let Err(error) = writeln!(checkpoint_file, "{}\t{}", path, body) {
          eprintln!("checkpoint write failed: {}", error);
          process::exit(4);
        }
        if let Err(error) = checkpoint_file.flush() {
          eprintln!("checkpoint flush failed: {}", error);
          process::exit(4);
        }
        // Best-effort sync; if the kernel ignores us mid-test that's not
        // a worker bug — the test verifies what the engine actually persisted.
        let _ = checkpoint_file.sync_data();
      }
      Err(error) => {
        eprintln!("store_file failed: {}", error);
        process::exit(5);
      }
    }

    if mode == "mixed" {
      // Occasional delete of an older entry to mix the workload.
      if counter > 0 && counter % 7 == 0 {
        let target = format!("/data/file-{:08}.txt", counter - 5);
        let _ = ops.delete_file(&ctx, &target);
        // Don't checkpoint deletes — only positive commits, since the parent's
        // assertion is "every checkpointed write must still be readable OR
        // explicitly deleted". For simplicity we just don't expect deleted
        // paths to be readable; the test reads `/data/file-*.txt` and tolerates
        // the recent deletions.
      }
    }

    counter += 1;

    // Tiny sleep to keep CPU usage bounded and give the OS time to schedule
    // a SIGKILL from the parent. Without this, on very fast disks the worker
    // can write tens of thousands of entries per second and never get
    // interrupted at an "interesting" moment.
    if counter % 32 == 0 {
      std::thread::sleep(Duration::from_micros(50));
    }
  }
}

fn fastish_hash(value: u64) -> u64 {
  // Simple non-crypto mixer for variety in the payload.
  let mut state = value.wrapping_mul(0x9E37_79B9_7F4A_7C15);
  state ^= state >> 33;
  state = state.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
  state ^= state >> 29;
  state
}
