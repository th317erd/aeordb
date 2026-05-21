//! GC benchmark / debugging tool.
//!
//! Subcommands:
//!   build  — populate a fresh database with deterministic synthetic data
//!            (random adds + deletes + snapshots).
//!   run    — open an existing database and time a single GC cycle.
//!            Set AEORDB_GC_TIMING=1 to enable per-phase timing on stderr.
//!
//! Workflow:
//!   ./gc-bench build /tmp/test.aeordb --files 5000 --seed 42
//!   cp /tmp/test.aeordb /tmp/working.aeordb
//!   AEORDB_GC_TIMING=1 ./gc-bench run /tmp/working.aeordb

use std::collections::HashSet;
use std::time::Instant;

use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::request_context::RequestContext;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::version_manager::VersionManager;

fn print_usage() {
  eprintln!("Usage:");
  eprintln!("  gc-bench build <db-path> [--files N] [--seed N] [--snapshot-every N] [--delete-pct N]");
  eprintln!("  gc-bench run   <db-path> [--dry-run]");
  std::process::exit(2);
}

fn parse_uint(args: &mut Vec<String>, flag: &str, default: u64) -> u64 {
  if let Some(idx) = args.iter().position(|a| a == flag) {
    let value = args.get(idx + 1)
      .unwrap_or_else(|| { eprintln!("error: {} requires a value", flag); std::process::exit(2); })
      .parse::<u64>()
      .unwrap_or_else(|_| { eprintln!("error: {} expects an integer", flag); std::process::exit(2); });
    args.drain(idx..=idx + 1);
    value
  } else { default }
}

fn parse_flag(args: &mut Vec<String>, flag: &str) -> bool {
  if let Some(idx) = args.iter().position(|a| a == flag) {
    args.remove(idx);
    true
  } else { false }
}

// SplitMix64 — same PRNG used in soak-worker; tiny, deterministic.
fn splitmix(state: &mut u64) -> u64 {
  *state = state.wrapping_add(0x9E3779B97F4A7C15);
  let mut z = *state;
  z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
  z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
  z ^ (z >> 31)
}

fn cmd_build(args: &mut Vec<String>) -> Result<(), String> {
  let target_files = parse_uint(args, "--files", 5000) as usize;
  let seed = parse_uint(args, "--seed", 42);
  let snapshot_every = parse_uint(args, "--snapshot-every", 500) as usize;
  let delete_pct = parse_uint(args, "--delete-pct", 15) as usize;
  let db_path = args.first()
    .ok_or_else(|| "build: missing <db-path>".to_string())?
    .clone();

  if std::path::Path::new(&db_path).exists() {
    return Err(format!("refusing to overwrite existing file: {}", db_path));
  }

  let start = Instant::now();
  println!("== building test database ==");
  println!("  path:           {}", db_path);
  println!("  target files:   {}", target_files);
  println!("  seed:           {}", seed);
  println!("  snapshot every: {} files", snapshot_every);
  println!("  delete %:       {}", delete_pct);
  println!();

  let engine = StorageEngine::create(&db_path)
    .map_err(|e| format!("create engine: {}", e))?;
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.ensure_root_directory(&ctx)
    .map_err(|e| format!("ensure root: {}", e))?;

  let vm = VersionManager::new(&engine);
  let mut rng = seed;
  let mut committed: Vec<String> = Vec::with_capacity(target_files);
  let mut total_bytes: u64 = 0;
  let mut deletes_done: usize = 0;
  let mut snapshots_done: usize = 0;

  for i in 0..target_files {
    // Random nested path: /dir{a}/dir{b}/file-{i}.bin
    let a = splitmix(&mut rng) % 20;
    let b = splitmix(&mut rng) % 20;
    let path = format!("/dir{}/dir{}/file-{:06}.bin", a, b, i);

    // Random size 1 KB .. ~3 MB, biased small so we get many small files
    // and occasional large ones (exercises chunking and dedup).
    let size_pick = splitmix(&mut rng) % 100;
    let size = if size_pick < 80 {
      (splitmix(&mut rng) % (256 * 1024)) as usize + 1024  // 1-256 KB
    } else if size_pick < 95 {
      (splitmix(&mut rng) % (1024 * 1024)) as usize + 256 * 1024  // 256 KB-1.25 MB
    } else {
      (splitmix(&mut rng) % (2 * 1024 * 1024)) as usize + 1024 * 1024  // 1-3 MB
    };

    // Content: a chunk-size-aligned-ish pattern derived from the seed.
    // Predictable but each file is different.
    let mut buf = vec![0u8; size];
    let mut filler = splitmix(&mut rng);
    for chunk in buf.chunks_mut(8) {
      let bytes = filler.to_le_bytes();
      let n = chunk.len().min(8);
      chunk[..n].copy_from_slice(&bytes[..n]);
      filler = filler.wrapping_add(0x9E3779B97F4A7C15);
    }

    ops.store_file_buffered(&ctx, &path, &buf, Some("application/octet-stream"))
      .map_err(|e| format!("store {} ({}b): {}", path, size, e))?;
    total_bytes += size as u64;
    committed.push(path);

    // Random delete: maintains a steady-state churn so we accumulate
    // garbage entries (deleted files' chunks/file-records).
    if !committed.is_empty() && (splitmix(&mut rng) % 100) < delete_pct as u64 {
      let victim_idx = (splitmix(&mut rng) as usize) % committed.len();
      let victim = committed.swap_remove(victim_idx);
      ops.delete_file(&ctx, &victim)
        .map_err(|e| format!("delete {}: {}", victim, e))?;
      deletes_done += 1;
    }

    // Periodic snapshot — creates multi-root mark workload for the GC.
    if i > 0 && i % snapshot_every == 0 {
      let name = format!("bench-{:04}", snapshots_done);
      vm.create_snapshot(&ctx, &name, std::collections::HashMap::new())
        .map_err(|e| format!("snapshot {}: {}", name, e))?;
      snapshots_done += 1;
    }

    if i > 0 && i % 500 == 0 {
      let counters = engine.counters().snapshot();
      println!("  [{:>6}/{}] writes={} deletes={} snaps={} entries={} bytes={:.1}MB elapsed={:.1}s",
        i, target_files, i + 1, deletes_done, snapshots_done,
        counters.files + counters.directories + counters.chunks + counters.snapshots,
        total_bytes as f64 / (1024.0 * 1024.0),
        start.elapsed().as_secs_f64(),
      );
    }
  }

  let counters = engine.counters().snapshot();
  let live = committed.len();
  let unique_paths: HashSet<&str> = committed.iter().map(|s| s.as_str()).collect();

  println!();
  println!("== build complete ==");
  println!("  elapsed:        {:.1}s", start.elapsed().as_secs_f64());
  println!("  files written:  {}", target_files);
  println!("  files deleted:  {}", deletes_done);
  println!("  files live:     {} ({} unique paths)", live, unique_paths.len());
  println!("  snapshots:      {}", snapshots_done);
  println!("  KV entries:     files={} dirs={} chunks={} snaps={}",
    counters.files, counters.directories, counters.chunks, counters.snapshots);
  println!("  data bytes:     {:.1} MB", total_bytes as f64 / (1024.0 * 1024.0));
  println!("  db file size:   {:.1} MB",
    std::fs::metadata(&db_path).map(|m| m.len() as f64 / (1024.0 * 1024.0)).unwrap_or(0.0));

  Ok(())
}

fn cmd_run(args: &mut Vec<String>) -> Result<(), String> {
  let dry_run = parse_flag(args, "--dry-run");
  let db_path = args.first()
    .ok_or_else(|| "run: missing <db-path>".to_string())?
    .clone();

  println!("== gc run ==");
  println!("  path:    {}", db_path);
  println!("  dry_run: {}", dry_run);
  println!("  timing:  {}",
    if std::env::var("AEORDB_GC_TIMING").is_ok() { "ENABLED" } else { "disabled (set AEORDB_GC_TIMING=1)" });
  println!();

  let open_start = Instant::now();
  let engine = StorageEngine::open(&db_path)
    .map_err(|e| format!("open engine: {}", e))?;
  println!("  open: {:?}", open_start.elapsed());

  let counters_before = engine.counters().snapshot();
  println!("  entries before: files={} dirs={} chunks={} snaps={} (logical_bytes={:.1}MB)",
    counters_before.files, counters_before.directories, counters_before.chunks,
    counters_before.snapshots, counters_before.logical_data_size as f64 / (1024.0 * 1024.0));

  let ctx = RequestContext::system();
  let gc_start = Instant::now();
  let result = aeordb::engine::gc::run_gc(&engine, &ctx, dry_run)
    .map_err(|e| format!("run_gc: {}", e))?;
  let gc_elapsed = gc_start.elapsed();

  println!();
  println!("== gc result ==");
  println!("  duration:        {:?} ({:.3}s)", gc_elapsed, gc_elapsed.as_secs_f64());
  println!("  versions:        {}", result.versions_scanned);
  println!("  live entries:    {}", result.live_entries);
  println!("  garbage entries: {}", result.garbage_entries);
  println!("  reclaimed bytes: {:.1} MB", result.reclaimed_bytes as f64 / (1024.0 * 1024.0));
  println!("  dry_run:         {}", result.dry_run);

  Ok(())
}

fn main() {
  let mut args: Vec<String> = std::env::args().skip(1).collect();
  if args.is_empty() {
    print_usage();
  }
  let cmd = args.remove(0);
  let result = match cmd.as_str() {
    "build" => cmd_build(&mut args),
    "run"   => cmd_run(&mut args),
    "-h" | "--help" | "help" => { print_usage(); Ok(()) }
    other => Err(format!("unknown subcommand: {}", other)),
  };

  if let Err(e) = result {
    eprintln!("error: {}", e);
    std::process::exit(1);
  }
}
