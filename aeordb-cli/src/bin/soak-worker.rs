//! 12-hour soak worker for steady-state durability testing (S1 / S2).
//!
//! Walks a source corpus once at startup, then runs a mixed write/read/delete
//! workload against an AeorDB database for a configurable duration. Emits a
//! metrics line every minute and appends every successful commit to a
//! checkpoint file so a parent orchestrator can verify recovery after a
//! SIGKILL.
//!
//! Usage:
//! ```
//! soak-worker --database <path> --source-dir <path> --duration-hours <N>
//!             [--checkpoint <path>] [--metrics <path>]
//!             [--workload <W:R:D>] [--max-db-size-gb <N>]
//!             [--snapshot-interval-secs <N>] [--gc-interval-secs <N>]
//!
//! # Summarize a finished run:
//! soak-worker --summarize <metrics-tsv>
//! ```
//!
//! Symlinks in the source directory are skipped entirely (they may point
//! outside the source root, which would silently expand the corpus).

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aeordb::engine::{gc, DirectoryOps, RequestContext, StorageEngine, VersionManager};

const METRICS_HEADER: &str = "iso_time\telapsed_secs\twrites\treads\tdeletes\trss_kb\tvm_data_kb\tvm_size_kb\tvm_hwm_kb\tfd_count\tdb_size_bytes\twal_bytes\tvoid_bytes\tentry_count\tcache_perms\tcache_index\tcache_dir\tlast_action";

struct Config {
  database: String,
  source_dir: String,
  duration: Duration,
  checkpoint: String,
  metrics: String,
  // workload mix percentages, must sum to 100
  pct_write: u8,
  pct_read: u8,
  pct_delete: u8,
  snapshot_interval: Duration,
  gc_interval: Duration,
  max_db_size_bytes: Option<u64>,
}

fn parse_args() -> Result<Mode, String> {
  let args: Vec<String> = std::env::args().collect();
  let mut database: Option<String> = None;
  let mut source_dir: Option<String> = None;
  let mut duration_hours: f64 = 12.0;
  let mut checkpoint: Option<String> = None;
  let mut metrics: Option<String> = None;
  let mut workload: String = "60:30:10".to_string();
  let mut snapshot_secs: u64 = 300;
  let mut gc_secs: u64 = 1800;
  let mut max_db_size_gb: Option<f64> = None;
  let mut summarize: Option<String> = None;

  let mut i = 1;
  while i < args.len() {
    let arg = args[i].as_str();
    let value = || args.get(i + 1).cloned();
    match arg {
      "--database"               => { database = value(); i += 2; }
      "--source-dir"             => { source_dir = value(); i += 2; }
      "--duration-hours"         => { duration_hours = value().and_then(|v| v.parse().ok()).unwrap_or(12.0); i += 2; }
      "--checkpoint"             => { checkpoint = value(); i += 2; }
      "--metrics"                => { metrics = value(); i += 2; }
      "--workload"               => { workload = value().unwrap_or(workload); i += 2; }
      "--snapshot-interval-secs" => { snapshot_secs = value().and_then(|v| v.parse().ok()).unwrap_or(snapshot_secs); i += 2; }
      "--gc-interval-secs"       => { gc_secs = value().and_then(|v| v.parse().ok()).unwrap_or(gc_secs); i += 2; }
      "--max-db-size-gb"         => { max_db_size_gb = value().and_then(|v| v.parse().ok()); i += 2; }
      "--summarize"              => { summarize = value(); i += 2; }
      _ => { i += 1; }
    }
  }

  if let Some(metrics_path) = summarize {
    return Ok(Mode::Summarize(metrics_path));
  }

  let database = database.ok_or("--database required".to_string())?;
  let source_dir = source_dir.ok_or("--source-dir required".to_string())?;
  let checkpoint = checkpoint.unwrap_or_else(|| format!("{}.checkpoint.tsv", database));
  let metrics_path = metrics.unwrap_or_else(|| format!("{}.metrics.tsv", database));

  let mix: Vec<u8> = workload.split(':').filter_map(|s| s.parse().ok()).collect();
  if mix.len() != 3 || mix.iter().sum::<u8>() != 100 {
    return Err(format!("--workload must be W:R:D summing to 100, got {}", workload));
  }

  Ok(Mode::Run(Config {
    database,
    source_dir,
    duration: Duration::from_secs((duration_hours * 3600.0) as u64),
    checkpoint,
    metrics: metrics_path,
    pct_write: mix[0],
    pct_read: mix[1],
    pct_delete: mix[2],
    snapshot_interval: Duration::from_secs(snapshot_secs),
    gc_interval: Duration::from_secs(gc_secs),
    max_db_size_bytes: max_db_size_gb.map(|gb| (gb * 1_073_741_824.0) as u64),
  }))
}

enum Mode {
  Run(Config),
  Summarize(String),
}

fn main() {
  match parse_args() {
    Ok(Mode::Run(config)) => {
      if let Err(error) = run(config) {
        eprintln!("soak failed: {}", error);
        process::exit(1);
      }
    }
    Ok(Mode::Summarize(path)) => {
      if let Err(error) = summarize(&path) {
        eprintln!("summarize failed: {}", error);
        process::exit(1);
      }
    }
    Err(message) => {
      eprintln!("{}", message);
      process::exit(2);
    }
  }
}

fn run(config: Config) -> Result<(), String> {
  println!("== AeorDB soak ==");
  println!("database:         {}", config.database);
  println!("source corpus:    {}", config.source_dir);
  println!("duration:         {:.2}h", config.duration.as_secs_f64() / 3600.0);
  println!("workload (W:R:D): {}:{}:{}", config.pct_write, config.pct_read, config.pct_delete);
  println!("checkpoint:       {}", config.checkpoint);
  println!("metrics:          {}", config.metrics);

  // 1. Build the source corpus list (walk once, skip symlinks).
  print!("walking source corpus... ");
  std::io::stdout().flush().ok();
  let walk_start = Instant::now();
  let corpus = build_corpus(&config.source_dir);
  println!("{} files in {:.1}s", corpus.len(), walk_start.elapsed().as_secs_f64());
  if corpus.is_empty() {
    return Err("source corpus is empty — nothing to do".to_string());
  }

  // 2. Open or create the database.
  let engine = if Path::new(&config.database).exists() {
    println!("opening existing database");
    Arc::new(StorageEngine::open(&config.database).map_err(|e| format!("open: {}", e))?)
  } else {
    if let Some(parent) = Path::new(&config.database).parent() {
      std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {}", parent.display(), e))?;
    }
    println!("creating fresh database");
    Arc::new(StorageEngine::create(&config.database).map_err(|e| format!("create: {}", e))?)
  };

  // 3. Load any existing checkpoint so reads/deletes can target previously
  //    committed paths after a crash-restart.
  let mut committed: HashSet<String> = load_checkpoint(&config.checkpoint);
  println!("loaded {} previously-committed paths from checkpoint", committed.len());

  let mut checkpoint_file = OpenOptions::new()
    .create(true).append(true).open(&config.checkpoint)
    .map_err(|e| format!("open checkpoint: {}", e))?;
  let mut metrics_file = open_metrics(&config.metrics)?;

  // Counters shared with the metrics-flush thread.
  let writes = Arc::new(AtomicU64::new(0));
  let reads = Arc::new(AtomicU64::new(0));
  let deletes = Arc::new(AtomicU64::new(0));
  let last_action: Arc<std::sync::Mutex<String>> = Arc::new(std::sync::Mutex::new("startup".to_string()));
  let stop_flag = Arc::new(AtomicBool::new(false));

  // Metrics thread: sample every 60s.
  let metrics_handle = {
    let writes = Arc::clone(&writes);
    let reads = Arc::clone(&reads);
    let deletes = Arc::clone(&deletes);
    let last_action = Arc::clone(&last_action);
    let stop_flag = Arc::clone(&stop_flag);
    let engine = Arc::clone(&engine);
    let database = config.database.clone();
    let start = Instant::now();
    std::thread::spawn(move || {
      let mut next_tick = Instant::now();
      while !stop_flag.load(Ordering::Relaxed) {
        // Emit a row every 60s, sleeping in 1s slices so we shut down quickly.
        if Instant::now() >= next_tick {
          let elapsed = start.elapsed().as_secs();
          let mem = read_self_memory_stats().unwrap_or_default();
          let fd_count = count_fds().unwrap_or(0);
          let db_size = std::fs::metadata(&database).map(|m| m.len()).unwrap_or(0);
          let counters = engine.counters().snapshot();
          let wal_bytes = counters.write_buffer_depth;
          let void_bytes = counters.void_space;
          // No single "live entries" counter — sum the per-type counters.
          let entry_count = counters.files + counters.directories + counters.symlinks
            + counters.chunks + counters.snapshots + counters.forks;
          let (cache_perms, cache_index, cache_dir) = engine.engine_cache_sizes();
          let action = last_action.lock().map(|g| g.clone()).unwrap_or_default();
          let line = format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            chrono::Utc::now().to_rfc3339(),
            elapsed,
            writes.load(Ordering::Relaxed),
            reads.load(Ordering::Relaxed),
            deletes.load(Ordering::Relaxed),
            mem.rss_kb,
            mem.data_kb,
            mem.size_kb,
            mem.hwm_kb,
            fd_count,
            db_size,
            wal_bytes,
            void_bytes,
            entry_count,
            cache_perms,
            cache_index,
            cache_dir,
            action,
          );
          if let Err(error) = writeln!(metrics_file, "{}", line) {
            eprintln!("metrics write failed: {}", error);
          }
          let _ = metrics_file.flush();
          next_tick += Duration::from_secs(60);
        }
        std::thread::sleep(Duration::from_secs(1));
      }
    })
  };

  // Wide-cadence RSS sampler: 50 ms polling, peak-per-second row out.
  // Catches transient spikes that the 60 s metrics cadence misses entirely.
  // Output: <db>.wide_rss.tsv with iso_time, peak_rss_kb, cur_rss_kb, hwm_kb.
  let wide_handle = {
    let stop_flag = Arc::clone(&stop_flag);
    let wide_path = format!("{}.wide_rss.tsv", config.database);
    std::thread::spawn(move || {
      let mut file = match std::fs::File::create(&wide_path) {
        Ok(f) => f,
        Err(e) => { eprintln!("wide_rss create failed: {e}"); return; }
      };
      let _ = writeln!(file, "iso_time\tpeak_rss_kb\tcur_rss_kb\thwm_kb");
      let mut bucket_start = Instant::now();
      let mut bucket_peak_kb: u64 = 0;
      while !stop_flag.load(Ordering::Relaxed) {
        let mem = read_self_memory_stats().unwrap_or_default();
        if mem.rss_kb > bucket_peak_kb { bucket_peak_kb = mem.rss_kb; }
        if bucket_start.elapsed() >= Duration::from_secs(1) {
          let _ = writeln!(file, "{}\t{}\t{}\t{}",
            chrono::Utc::now().to_rfc3339(),
            bucket_peak_kb, mem.rss_kb, mem.hwm_kb,
          );
          let _ = file.flush();
          bucket_start = Instant::now();
          bucket_peak_kb = 0;
        }
        std::thread::sleep(Duration::from_millis(50));
      }
    })
  };

  // 4. Main workload loop. Each iteration picks an action and executes it
  //    synchronously. The committed-paths set stays in-memory authoritative;
  //    the checkpoint file is the recovery oracle.
  let start = Instant::now();
  let mut last_snapshot = Instant::now();
  let mut last_gc = Instant::now();
  let mut size_capped_logged = false;
  let mut rng_state: u64 = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;

  println!("starting workload loop");
  while start.elapsed() < config.duration {
    let pick = next_u32(&mut rng_state) % 100;
    let action: char = if pick < config.pct_write as u32 {
      'W'
    } else if pick < (config.pct_write + config.pct_read) as u32 {
      'R'
    } else {
      'D'
    };

    // Honor the DB size cap by demoting writes to reads when we're past it.
    let size_now = std::fs::metadata(&config.database).map(|m| m.len()).unwrap_or(0);
    let action = match config.max_db_size_bytes {
      Some(cap) if size_now >= cap => {
        if !size_capped_logged {
          eprintln!("DB size {} ≥ cap {} — demoting future writes to reads", size_now, cap);
          size_capped_logged = true;
        }
        if action == 'W' { 'R' } else { action }
      }
      _ => action,
    };

    match action {
      'W' => {
        let source = &corpus[next_u32(&mut rng_state) as usize % corpus.len()];
        match do_write(&engine, source, &config.source_dir) {
          Ok(stored_path) => {
            writeln!(checkpoint_file, "+\t{}", stored_path).ok();
            checkpoint_file.flush().ok();
            committed.insert(stored_path.clone());
            writes.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut s) = last_action.lock() { *s = format!("W {}", stored_path); }
          }
          Err(e) => {
            // Source read errors (e.g. permission denied on .gnupg, broken
            // symlink target) are expected — log + continue, don't fail.
            if let Ok(mut s) = last_action.lock() { *s = format!("W FAIL {}: {}", source.display(), e); }
          }
        }
      }
      'R' => {
        if committed.is_empty() {
          // Nothing to read yet — fall through to write.
          continue;
        }
        let path = pick_random(&committed, &mut rng_state);
        let ops = DirectoryOps::new(&engine);
        // Stream the file. We don't actually care about the content — only
        // that the read path exercises chunk fetch + decompress + iterate.
        // Memory stays bounded to one chunk regardless of file size.
        match ops.read_file_streaming(&path) {
          Ok(stream) => {
            let mut total: u64 = 0;
            let mut failure: Option<String> = None;
            for chunk_result in stream {
              match chunk_result {
                Ok(chunk) => { total += chunk.len() as u64; }
                Err(e) => { failure = Some(format!("{}", e)); break; }
              }
            }
            if let Some(err) = failure {
              eprintln!("read stream failed for {}: {}", path, err);
              if let Ok(mut s) = last_action.lock() { *s = format!("R FAIL {}: {}", path, err); }
            } else {
              reads.fetch_add(1, Ordering::Relaxed);
              if let Ok(mut s) = last_action.lock() { *s = format!("R {} ({} bytes)", path, total); }
            }
          }
          Err(e) => {
            eprintln!("read failed for {}: {}", path, e);
            if let Ok(mut s) = last_action.lock() { *s = format!("R FAIL {}: {}", path, e); }
          }
        }
      }
      'D' => {
        if committed.is_empty() { continue; }
        let path = pick_random(&committed, &mut rng_state);
        let ctx = RequestContext::system();
        let ops = DirectoryOps::new(&engine);
        match ops.delete_file(&ctx, &path) {
          Ok(_) => {
            writeln!(checkpoint_file, "-\t{}", path).ok();
            checkpoint_file.flush().ok();
            committed.remove(&path);
            deletes.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut s) = last_action.lock() { *s = format!("D {}", path); }
          }
          Err(e) => {
            if let Ok(mut s) = last_action.lock() { *s = format!("D FAIL {}: {}", path, e); }
          }
        }
      }
      _ => unreachable!(),
    }

    // Periodic snapshot.
    if last_snapshot.elapsed() >= config.snapshot_interval {
      let ctx = RequestContext::system();
      let vm = VersionManager::new(&engine);
      let name = format!("soak-{}", chrono::Utc::now().timestamp());
      if let Err(e) = vm.create_snapshot(&ctx, &name, std::collections::HashMap::new()) {
        eprintln!("snapshot failed: {}", e);
      } else if let Ok(mut s) = last_action.lock() {
        *s = format!("SNAPSHOT {}", name);
      }
      last_snapshot = Instant::now();
    }

    // Periodic GC.
    if last_gc.elapsed() >= config.gc_interval {
      let ctx = RequestContext::system();
      match gc::run_gc(&engine, &ctx, false) {
        Ok(result) => {
          if let Ok(mut s) = last_action.lock() {
            *s = format!("GC reclaimed={}b swept={}", result.reclaimed_bytes, result.garbage_entries);
          }
        }
        Err(e) => eprintln!("gc failed: {}", e),
      }
      last_gc = Instant::now();
    }
  }

  println!("duration reached, shutting down");
  stop_flag.store(true, Ordering::Relaxed);
  let _ = metrics_handle.join();
  let _ = wide_handle.join();

  // Final flush of the engine so any in-memory state is durable.
  if let Err(e) = engine.shutdown() {
    eprintln!("shutdown returned error: {}", e);
  }

  println!("done. Writes={} Reads={} Deletes={}",
    writes.load(Ordering::Relaxed),
    reads.load(Ordering::Relaxed),
    deletes.load(Ordering::Relaxed),
  );
  println!("metrics: {}", config.metrics);
  println!("Run `soak-worker --summarize {}` for a pass/fail report.", config.metrics);
  Ok(())
}

// ---------------------------------------------------------------------------
// Workload helpers
// ---------------------------------------------------------------------------

fn do_write(engine: &StorageEngine, source: &Path, source_root: &str) -> Result<String, String> {
  // Map the source path into the soak namespace by replacing the source root
  // with /soak. So `/media/Data/.../Pictures/foo.jpg` →
  // `/soak/Pictures/foo.jpg`. Overwriting the same path exercises the
  // overwrite-then-version-cleanup paths over time.
  let trimmed_root = source_root.trim_end_matches('/');
  let rel = source.to_string_lossy();
  let rel = rel.strip_prefix(trimmed_root).unwrap_or(&rel);
  let aeordb_path = format!("/soak{}", rel);

  // Stream the source file directly into the engine — no full-file buffer.
  // store_file_from_reader chunks at 256 KB regardless of file size, so a
  // 4 GB MP4 uses the same peak memory as a 4 KB text file.
  let file = File::open(source).map_err(|e| format!("open source: {}", e))?;
  let reader = std::io::BufReader::with_capacity(262_144, file);

  let content_type = guess_content_type(source);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(engine);
  ops.store_file_from_reader(&ctx, &aeordb_path, reader, Some(&content_type))
    .map_err(|e| format!("store_file_from_reader: {}", e))?;

  Ok(aeordb_path)
}

fn guess_content_type(p: &Path) -> String {
  match p.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase()).as_deref() {
    Some("jpg") | Some("jpeg") => "image/jpeg".to_string(),
    Some("png") => "image/png".to_string(),
    Some("gif") => "image/gif".to_string(),
    Some("pdf") => "application/pdf".to_string(),
    Some("mp3") => "audio/mpeg".to_string(),
    Some("mp4") => "video/mp4".to_string(),
    Some("txt") | Some("md") => "text/plain".to_string(),
    Some("json") => "application/json".to_string(),
    Some("html") => "text/html".to_string(),
    _ => "application/octet-stream".to_string(),
  }
}

// ---------------------------------------------------------------------------
// Corpus walk
// ---------------------------------------------------------------------------

fn build_corpus(root: &str) -> Vec<PathBuf> {
  let mut out = Vec::new();
  let mut stack = vec![PathBuf::from(root)];
  while let Some(dir) = stack.pop() {
    let entries = match std::fs::read_dir(&dir) {
      Ok(entries) => entries,
      Err(_) => continue,
    };
    for entry_result in entries {
      let entry = match entry_result {
        Ok(e) => e,
        Err(_) => continue,
      };
      // Skip symlinks unconditionally — they can point outside the source
      // root or form cycles. We only want regular files / regular dirs.
      let meta = match entry.path().symlink_metadata() {
        Ok(m) => m,
        Err(_) => continue,
      };
      let file_type = meta.file_type();
      if file_type.is_symlink() {
        continue;
      } else if file_type.is_dir() {
        stack.push(entry.path());
      } else if file_type.is_file() {
        out.push(entry.path());
      }
    }
  }
  out
}

// ---------------------------------------------------------------------------
// Checkpoint
// ---------------------------------------------------------------------------

fn load_checkpoint(path: &str) -> HashSet<String> {
  let file = match File::open(path) {
    Ok(f) => f,
    Err(_) => return HashSet::new(),
  };
  let mut set = HashSet::new();
  for line in BufReader::new(file).lines().map_while(Result::ok) {
    if let Some(rest) = line.strip_prefix("+\t") {
      set.insert(rest.to_string());
    } else if let Some(rest) = line.strip_prefix("-\t") {
      set.remove(rest);
    }
  }
  set
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

fn open_metrics(path: &str) -> Result<BufWriter<File>, String> {
  let needs_header = !Path::new(path).exists();
  let file = OpenOptions::new().create(true).append(true).open(path)
    .map_err(|e| format!("open metrics: {}", e))?;
  let mut writer = BufWriter::new(file);
  if needs_header {
    writeln!(writer, "{}", METRICS_HEADER).map_err(|e| format!("write header: {}", e))?;
    writer.flush().ok();
  }
  Ok(writer)
}

#[derive(Default)]
struct MemoryStats {
  rss_kb: u64,
  data_kb: u64,
  size_kb: u64,
  hwm_kb: u64,
}

fn read_self_memory_stats() -> Option<MemoryStats> {
  // /proc/self/status carries VmRSS/VmData/VmSize/VmHWM in kB. Distinguish:
  //   VmRSS  — resident set size right now
  //   VmHWM  — peak RSS ever (monotonic; useful for trend detection)
  //   VmData — heap + data segment (where leaks show up)
  //   VmSize — total virtual address space (mapped, not necessarily resident)
  let status = std::fs::read_to_string("/proc/self/status").ok()?;
  let mut stats = MemoryStats::default();
  for line in status.lines() {
    let parse_kb = |prefix: &str| -> Option<u64> {
      line.strip_prefix(prefix)?.split_whitespace().next()?.parse().ok()
    };
    if let Some(v) = parse_kb("VmRSS:")  { stats.rss_kb = v; }
    if let Some(v) = parse_kb("VmData:") { stats.data_kb = v; }
    if let Some(v) = parse_kb("VmSize:") { stats.size_kb = v; }
    if let Some(v) = parse_kb("VmHWM:")  { stats.hwm_kb = v; }
  }
  Some(stats)
}

fn count_fds() -> Option<usize> {
  std::fs::read_dir("/proc/self/fd").ok().map(|iter| iter.count())
}

// ---------------------------------------------------------------------------
// Summarize
// ---------------------------------------------------------------------------

fn summarize(path: &str) -> Result<(), String> {
  let file = File::open(path).map_err(|e| format!("open {}: {}", path, e))?;
  let mut rows: Vec<Row> = Vec::new();
  for (i, line) in BufReader::new(file).lines().map_while(Result::ok).enumerate() {
    if i == 0 || line.starts_with("iso_time") { continue; }
    if let Some(row) = parse_row(&line) {
      rows.push(row);
    }
  }
  if rows.is_empty() {
    return Err("no metric rows parsed".to_string());
  }

  let first = &rows[0];
  let last = &rows[rows.len() - 1];
  // "Warmup baseline" = ~first hour. Find first row at elapsed >= 3600s
  // (or the latest available if the run didn't reach an hour).
  let baseline = rows.iter().rev().find(|r| r.elapsed_secs <= 3600).unwrap_or(first);
  let baseline_warm = rows.iter().find(|r| r.elapsed_secs >= 3600).unwrap_or(baseline);

  println!("== AeorDB soak summary ==");
  println!("rows:               {}", rows.len());
  println!("duration logged:    {:.2}h", last.elapsed_secs as f64 / 3600.0);
  println!();
  println!("counters @ end:     writes={}  reads={}  deletes={}", last.writes, last.reads, last.deletes);
  println!("entry_count @ end:  {}", last.entry_count);
  println!();
  println!("RSS:");
  println!("  T+0:              {} MB", first.rss_kb / 1024);
  println!("  T+1h (warmup):    {} MB", baseline_warm.rss_kb / 1024);
  println!("  T+end:            {} MB", last.rss_kb / 1024);
  let rss_growth_pct = if baseline_warm.rss_kb > 0 {
    100.0 * (last.rss_kb as f64 - baseline_warm.rss_kb as f64) / baseline_warm.rss_kb as f64
  } else { 0.0 };
  println!("  growth T+1h→end:  {:+.1}%", rss_growth_pct);
  println!();
  println!("VmData (heap+data) — leaks show up here, not in RSS:");
  println!("  T+0:              {} MB", first.data_kb / 1024);
  println!("  T+1h:             {} MB", baseline_warm.data_kb / 1024);
  println!("  T+end:            {} MB", last.data_kb / 1024);
  let data_growth_pct = if baseline_warm.data_kb > 0 {
    100.0 * (last.data_kb as f64 - baseline_warm.data_kb as f64) / baseline_warm.data_kb as f64
  } else { 0.0 };
  println!("  growth T+1h→end:  {:+.1}%", data_growth_pct);
  println!();
  println!("VmHWM (peak RSS ever):  {} MB", last.hwm_kb / 1024);
  println!();
  println!("Engine caches (entry counts):");
  println!("  permissions: T+0={}  T+end={}", first.cache_perms, last.cache_perms);
  println!("  index:       T+0={}  T+end={}", first.cache_index, last.cache_index);
  println!("  dir_content: T+0={}  T+end={}", first.cache_dir, last.cache_dir);
  println!();
  println!("FD count:");
  println!("  min:              {}", rows.iter().map(|r| r.fd_count).min().unwrap_or(0));
  println!("  max:              {}", rows.iter().map(|r| r.fd_count).max().unwrap_or(0));
  println!();
  println!("DB size:");
  println!("  T+0:              {:.2} MB", first.db_size_bytes as f64 / 1_048_576.0);
  println!("  T+end:            {:.2} MB", last.db_size_bytes as f64 / 1_048_576.0);
  println!("  growth:           {:+.2} MB", (last.db_size_bytes as f64 - first.db_size_bytes as f64) / 1_048_576.0);
  println!();

  let mut pass = true;
  let mut report = |label: &str, ok: bool, detail: String| {
    println!("  [{}] {}: {}", if ok { " OK " } else { "FAIL" }, label, detail);
    if !ok { pass = false; }
  };

  let rss_ok = rss_growth_pct <= 30.0;
  report("RSS growth ≤ 30% (T+1h→end)", rss_ok, format!("{:+.1}%", rss_growth_pct));

  let data_ok = data_growth_pct <= 30.0;
  report("VmData growth ≤ 30% (T+1h→end)", data_ok, format!("{:+.1}%", data_growth_pct));

  let fd_max = rows.iter().map(|r| r.fd_count).max().unwrap_or(0);
  let fd_ok = fd_max <= 500;
  report("FD count ≤ 500", fd_ok, format!("max={}", fd_max));

  println!();
  println!("verdict: {}", if pass { "PASS" } else { "FAIL" });
  if !pass { process::exit(1); }
  Ok(())
}

struct Row {
  elapsed_secs: u64,
  writes: u64,
  reads: u64,
  deletes: u64,
  rss_kb: u64,
  data_kb: u64,
  hwm_kb: u64,
  fd_count: usize,
  db_size_bytes: u64,
  entry_count: u64,
  cache_perms: u64,
  cache_index: u64,
  cache_dir: u64,
}

fn parse_row(line: &str) -> Option<Row> {
  let cols: Vec<&str> = line.split('\t').collect();
  // 18 columns in v2 header. Earlier 12-column files are tolerated by falling
  // back to v1 positions for the fields we know about.
  let v2 = cols.len() >= 18;
  if cols.len() < 11 { return None; }
  Some(Row {
    elapsed_secs:  cols[1].parse().ok()?,
    writes:        cols[2].parse().ok()?,
    reads:         cols[3].parse().ok()?,
    deletes:       cols[4].parse().ok()?,
    rss_kb:        cols[5].parse().ok()?,
    data_kb:       if v2 { cols[6].parse().ok()? } else { 0 },
    hwm_kb:        if v2 { cols[8].parse().ok()? } else { 0 },
    fd_count:      if v2 { cols[9].parse().ok()? } else { cols[6].parse().ok()? },
    db_size_bytes: if v2 { cols[10].parse().ok()? } else { cols[7].parse().ok()? },
    entry_count:   if v2 { cols[13].parse().ok()? } else { cols[10].parse().ok()? },
    cache_perms:   if v2 { cols[14].parse().ok()? } else { 0 },
    cache_index:   if v2 { cols[15].parse().ok()? } else { 0 },
    cache_dir:     if v2 { cols[16].parse().ok()? } else { 0 },
  })
}

// ---------------------------------------------------------------------------
// Misc
// ---------------------------------------------------------------------------

fn next_u32(state: &mut u64) -> u32 {
  // splitmix64 — fast non-crypto PRNG, deterministic given seed.
  *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
  let mut z = *state;
  z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
  z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
  z ^= z >> 31;
  (z & 0xFFFF_FFFF) as u32
}

fn pick_random(set: &HashSet<String>, rng: &mut u64) -> String {
  // HashSet doesn't have indexed access. For a soak we don't need uniform —
  // pick a random skip count and iterate. O(N) per pick, fine for our scale.
  let n = set.len();
  let skip = (next_u32(rng) as usize) % n;
  set.iter().nth(skip).cloned().unwrap_or_default()
}
