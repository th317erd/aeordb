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
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aeordb::engine::{gc, DirectoryOps, RequestContext, StorageEngine, VersionManager};

const METRICS_HEADER: &str = "iso_time\telapsed_secs\twrites\treads\tdeletes\trss_kb\tvm_data_kb\tvm_size_kb\tvm_hwm_kb\tfd_count\tdb_size_bytes\twal_bytes\tvoid_bytes\tentry_count\tcache_perms\tcache_index\tcache_dir\tlast_action";
const CHECKPOINT_PAYLOAD_MAX_BYTES: usize = u16::MAX as usize + 2;
// The bounded payload above plus optional CRLF.
const CHECKPOINT_RECORD_MAX_BYTES: usize = CHECKPOINT_PAYLOAD_MAX_BYTES + 2;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkloadAction {
  Write,
  Read,
  Delete,
}

fn argument_value<'a>(arguments: &'a [String], index: usize, flag: &str) -> Result<&'a str, String> {
  arguments.get(index + 1).map(String::as_str).ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_f64_argument(arguments: &[String], index: usize, flag: &str) -> Result<f64, String> {
  let raw = argument_value(arguments, index, flag)?;
  let value = raw.parse::<f64>().map_err(|_| format!("{flag} expects a number, got {raw:?}"))?;
  if !value.is_finite() || value < 0.0 {
    return Err(format!("{flag} expects a finite non-negative number, got {raw:?}"));
  }
  Ok(value)
}

fn parse_u64_argument(arguments: &[String], index: usize, flag: &str) -> Result<u64, String> {
  let raw = argument_value(arguments, index, flag)?;
  raw.parse::<u64>().map_err(|_| format!("{flag} expects an unsigned integer, got {raw:?}"))
}

fn duration_from_hours(hours: f64) -> Result<Duration, String> {
  let seconds = hours * 3600.0;
  if seconds > u64::MAX as f64 {
    return Err("--duration-hours exceeds the supported duration".to_string());
  }
  Ok(Duration::from_secs(seconds as u64))
}

fn gibibytes_to_bytes(gibibytes: f64) -> Result<u64, String> {
  let bytes = gibibytes * 1_073_741_824.0;
  if bytes > u64::MAX as f64 {
    return Err("--max-db-size-gb exceeds the supported database size".to_string());
  }
  Ok(bytes as u64)
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
    match arg {
      "--database" => {
        database = Some(argument_value(&args, i, arg)?.to_string());
        i += 2;
      }
      "--source-dir" => {
        source_dir = Some(argument_value(&args, i, arg)?.to_string());
        i += 2;
      }
      "--duration-hours" => {
        duration_hours = parse_f64_argument(&args, i, arg)?;
        i += 2;
      }
      "--checkpoint" => {
        checkpoint = Some(argument_value(&args, i, arg)?.to_string());
        i += 2;
      }
      "--metrics" => {
        metrics = Some(argument_value(&args, i, arg)?.to_string());
        i += 2;
      }
      "--workload" => {
        workload = argument_value(&args, i, arg)?.to_string();
        i += 2;
      }
      "--snapshot-interval-secs" => {
        snapshot_secs = parse_u64_argument(&args, i, arg)?;
        i += 2;
      }
      "--gc-interval-secs" => {
        gc_secs = parse_u64_argument(&args, i, arg)?;
        i += 2;
      }
      "--max-db-size-gb" => {
        max_db_size_gb = Some(parse_f64_argument(&args, i, arg)?);
        i += 2;
      }
      "--summarize" => {
        summarize = Some(argument_value(&args, i, arg)?.to_string());
        i += 2;
      }
      _ => return Err(format!("unknown argument {arg:?}")),
    }
  }

  if let Some(metrics_path) = summarize {
    return Ok(Mode::Summarize(metrics_path));
  }

  let database = database.ok_or("--database required".to_string())?;
  let source_dir = source_dir.ok_or("--source-dir required".to_string())?;
  let checkpoint = checkpoint.unwrap_or_else(|| format!("{}.checkpoint.tsv", database));
  let metrics_path = metrics.unwrap_or_else(|| format!("{}.metrics.tsv", database));

  let mix = workload
    .split(':')
    .map(|value| value.parse::<u8>().map_err(|_| format!("--workload expects unsigned W:R:D percentages, got {workload:?}")))
    .collect::<Result<Vec<_>, _>>()?;
  if mix.len() != 3 || mix.iter().sum::<u8>() != 100 {
    return Err(format!("--workload must be W:R:D summing to 100, got {}", workload));
  }

  Ok(Mode::Run(Config {
    database,
    source_dir,
    duration: duration_from_hours(duration_hours)?,
    checkpoint,
    metrics: metrics_path,
    pct_write: mix[0],
    pct_read: mix[1],
    pct_delete: mix[2],
    snapshot_interval: Duration::from_secs(snapshot_secs),
    gc_interval: Duration::from_secs(gc_secs),
    max_db_size_bytes: max_db_size_gb.map(gibibytes_to_bytes).transpose()?,
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
  std::io::stdout().flush().map_err(|error| format!("flush source-corpus status: {error}"))?;
  let walk_start = Instant::now();
  let corpus = build_corpus(&config.source_dir)?;
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
  let mut committed: HashSet<String> = load_checkpoint(Path::new(&config.checkpoint))?;
  println!("loaded {} previously-committed paths from checkpoint", committed.len());

  let mut checkpoint_file =
    OpenOptions::new().create(true).append(true).open(&config.checkpoint).map_err(|e| format!("open checkpoint: {}", e))?;
  let mut metrics_file = open_metrics(&config.metrics)?;

  // Counters shared with the metrics-flush thread.
  let writes = Arc::new(AtomicU64::new(0));
  let reads = Arc::new(AtomicU64::new(0));
  let deletes = Arc::new(AtomicU64::new(0));
  let last_action = Arc::new(Mutex::new("startup".to_string()));
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
    std::thread::spawn(move || -> Result<(), String> {
      let mut next_tick = Instant::now();
      while !stop_flag.load(Ordering::Relaxed) {
        // Emit a row every 60s, sleeping in 1s slices so we shut down quickly.
        if Instant::now() >= next_tick {
          let elapsed = start.elapsed().as_secs();
          let mem = read_self_memory_stats()?;
          let fd_count = count_fds().map(|value| value.to_string()).unwrap_or_else(|| "unavailable".to_string());
          let db_size = database_size_bytes(Path::new(&database))?;
          let counters = engine.counters().snapshot();
          let wal_bytes = counters.write_buffer_depth;
          let void_bytes = counters.void_space;
          // No single "live entries" counter — sum the per-type counters.
          let entry_count =
            counters.files + counters.directories + counters.symlinks + counters.chunks + counters.snapshots + counters.forks;
          let (cache_perms, cache_index, cache_dir) = engine.engine_cache_sizes();
          let action = read_last_action(&last_action)?;
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
          writeln!(metrics_file, "{}", line).map_err(|error| format!("metrics write failed: {error}"))?;
          metrics_file.flush().map_err(|error| format!("metrics flush failed: {error}"))?;
          next_tick += Duration::from_secs(60);
        }
        std::thread::sleep(Duration::from_secs(1));
      }
      Ok(())
    })
  };

  // Wide-cadence RSS sampler: 50 ms polling, peak-per-second row out.
  // Catches transient spikes that the 60 s metrics cadence misses entirely.
  // Output: <db>.wide_rss.tsv with iso_time, peak_rss_kb, cur_rss_kb, hwm_kb.
  let wide_handle = {
    let stop_flag = Arc::clone(&stop_flag);
    let wide_path = format!("{}.wide_rss.tsv", config.database);
    std::thread::spawn(move || -> Result<(), String> {
      let mut file = std::fs::File::create(&wide_path).map_err(|error| format!("wide RSS create failed for {wide_path}: {error}"))?;
      writeln!(file, "iso_time\tpeak_rss_kb\tcur_rss_kb\thwm_kb")
        .map_err(|error| format!("wide RSS header write failed for {wide_path}: {error}"))?;
      let mut bucket_start = Instant::now();
      let mut bucket_peak_kb: u64 = 0;
      while !stop_flag.load(Ordering::Relaxed) {
        let mem = read_self_memory_stats()?;
        if mem.rss_kb > bucket_peak_kb {
          bucket_peak_kb = mem.rss_kb;
        }
        if bucket_start.elapsed() >= Duration::from_secs(1) {
          writeln!(file, "{}\t{}\t{}\t{}", chrono::Utc::now().to_rfc3339(), bucket_peak_kb, mem.rss_kb, mem.hwm_kb,)
            .map_err(|error| format!("wide RSS write failed for {wide_path}: {error}"))?;
          file.flush().map_err(|error| format!("wide RSS flush failed for {wide_path}: {error}"))?;
          bucket_start = Instant::now();
          bucket_peak_kb = 0;
        }
        std::thread::sleep(Duration::from_millis(50));
      }
      Ok(())
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
  let mut workload_result = Ok(());

  println!("starting workload loop");
  'workload: while start.elapsed() < config.duration {
    let pick = next_u32(&mut rng_state) % 100;
    let action = if pick < config.pct_write as u32 {
      WorkloadAction::Write
    } else if pick < (config.pct_write + config.pct_read) as u32 {
      WorkloadAction::Read
    } else {
      WorkloadAction::Delete
    };

    // Honor the DB size cap by demoting writes to reads when we're past it.
    let size_now = match database_size_bytes(Path::new(&config.database)) {
      Ok(size) => size,
      Err(error) => {
        workload_result = Err(error);
        break 'workload;
      }
    };
    let action = match config.max_db_size_bytes {
      Some(cap) if size_now >= cap => {
        if !size_capped_logged {
          eprintln!("DB size {} ≥ cap {} — demoting future writes to reads", size_now, cap);
          size_capped_logged = true;
        }
        if action == WorkloadAction::Write {
          WorkloadAction::Read
        } else {
          action
        }
      }
      _ => action,
    };

    match action {
      WorkloadAction::Write => {
        let source = &corpus[next_u32(&mut rng_state) as usize % corpus.len()];
        match do_write(&engine, source, &config.source_dir) {
          Ok(stored_path) => {
            if let Err(error) = append_checkpoint(&mut checkpoint_file, '+', &stored_path) {
              workload_result = Err(error);
              break 'workload;
            }
            committed.insert(stored_path.clone());
            writes.fetch_add(1, Ordering::Relaxed);
            if let Err(error) = set_last_action(&last_action, format!("W {}", stored_path)) {
              workload_result = Err(error);
              break 'workload;
            }
          }
          Err(e) => {
            workload_result = Err(record_workload_failure(&last_action, format!("write {} failed: {e}", source.display())));
            break 'workload;
          }
        }
      }
      WorkloadAction::Read => {
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
                Ok(chunk) => {
                  total += chunk.len() as u64;
                }
                Err(e) => {
                  failure = Some(format!("{}", e));
                  break;
                }
              }
            }
            if let Some(err) = failure {
              workload_result = Err(record_workload_failure(&last_action, format!("read stream {path} failed: {err}")));
              break 'workload;
            } else {
              reads.fetch_add(1, Ordering::Relaxed);
              if let Err(error) = set_last_action(&last_action, format!("R {} ({} bytes)", path, total)) {
                workload_result = Err(error);
                break 'workload;
              }
            }
          }
          Err(e) => {
            workload_result = Err(record_workload_failure(&last_action, format!("read {path} failed: {e}")));
            break 'workload;
          }
        }
      }
      WorkloadAction::Delete => {
        if committed.is_empty() {
          continue;
        }
        let path = pick_random(&committed, &mut rng_state);
        let ctx = RequestContext::system();
        let ops = DirectoryOps::new(&engine);
        // Exclude the path from the external must-survive oracle before the
        // database delete can commit. A crash after this `?` record may leave
        // an extra file, but can never fabricate a missing acknowledged file.
        if let Err(error) = append_checkpoint(&mut checkpoint_file, '?', &path) {
          workload_result = Err(error);
          break 'workload;
        }
        match ops.delete_file(&ctx, &path) {
          Ok(_) => {
            if let Err(error) = append_checkpoint(&mut checkpoint_file, '-', &path) {
              workload_result = Err(error);
              break 'workload;
            }
            committed.remove(&path);
            deletes.fetch_add(1, Ordering::Relaxed);
            if let Err(error) = set_last_action(&last_action, format!("D {}", path)) {
              workload_result = Err(error);
              break 'workload;
            }
          }
          Err(e) => {
            workload_result = Err(record_workload_failure(&last_action, format!("delete {path} failed: {e}")));
            break 'workload;
          }
        }
      }
    }

    // Periodic snapshot.
    if last_snapshot.elapsed() >= config.snapshot_interval {
      let ctx = RequestContext::system();
      let vm = VersionManager::new(&engine);
      let name = format!("soak-{}", chrono::Utc::now().timestamp());
      match vm.create_snapshot(&ctx, &name, std::collections::HashMap::new()) {
        Ok(_) => {
          if let Err(error) = set_last_action(&last_action, format!("SNAPSHOT {}", name)) {
            workload_result = Err(error);
            break 'workload;
          }
        }
        Err(error) => {
          workload_result = Err(record_workload_failure(&last_action, format!("snapshot {name} failed: {error}")));
          break 'workload;
        }
      }
      last_snapshot = Instant::now();
    }

    // Periodic GC.
    if last_gc.elapsed() >= config.gc_interval {
      let ctx = RequestContext::system();
      match gc::run_gc(&engine, &ctx, false) {
        Ok(result) => {
          if let Err(error) =
            set_last_action(&last_action, format!("GC reclaimed={}b swept={}", result.reclaimed_bytes, result.garbage_entries))
          {
            workload_result = Err(error);
            break 'workload;
          }
        }
        Err(error) => {
          workload_result = Err(record_workload_failure(&last_action, format!("GC failed: {error}")));
          break 'workload;
        }
      }
      last_gc = Instant::now();
    }
  }

  println!("duration reached, shutting down");
  stop_flag.store(true, Ordering::Relaxed);
  let metrics_result = metrics_handle.join().map_err(|_| "metrics worker panicked".to_string()).and_then(|result| result);
  let wide_result = wide_handle.join().map_err(|_| "wide RSS worker panicked".to_string()).and_then(|result| result);
  let diagnostics_result = combine_diagnostic_worker_results(metrics_result, wide_result);

  // Final flush of the engine so any in-memory state is durable.
  let shutdown_result = engine.shutdown().map_err(|error| format!("engine shutdown failed: {error}"));
  let cleanup_result = combine_soak_shutdown_results(diagnostics_result, shutdown_result);
  combine_workload_cleanup_results(workload_result, cleanup_result)?;

  println!(
    "done. Writes={} Reads={} Deletes={}",
    writes.load(Ordering::Relaxed),
    reads.load(Ordering::Relaxed),
    deletes.load(Ordering::Relaxed),
  );
  println!("metrics: {}", config.metrics);
  println!("Run `soak-worker --summarize {}` for a pass/fail report.", config.metrics);
  Ok(())
}

fn combine_diagnostic_worker_results(metrics_result: Result<(), String>, wide_result: Result<(), String>) -> Result<(), String> {
  match (metrics_result, wide_result) {
    (Ok(()), Ok(())) => {}
    (Err(metrics_error), Ok(())) => return Err(metrics_error),
    (Ok(()), Err(wide_error)) => return Err(wide_error),
    (Err(metrics_error), Err(wide_error)) => {
      return Err(format!("diagnostic workers failed: metrics: {metrics_error}; wide RSS: {wide_error}"));
    }
  }
  Ok(())
}

fn combine_soak_shutdown_results(diagnostics_result: Result<(), String>, shutdown_result: Result<(), String>) -> Result<(), String> {
  match (diagnostics_result, shutdown_result) {
    (Ok(()), Ok(())) => {}
    (Err(diagnostics_error), Ok(())) => return Err(diagnostics_error),
    (Ok(()), Err(shutdown_error)) => return Err(shutdown_error),
    (Err(diagnostics_error), Err(shutdown_error)) => {
      return Err(format!("soak shutdown failed: diagnostics: {diagnostics_error}; engine: {shutdown_error}"));
    }
  }
  Ok(())
}

fn combine_workload_cleanup_results(workload_result: Result<(), String>, cleanup_result: Result<(), String>) -> Result<(), String> {
  match (workload_result, cleanup_result) {
    (Ok(()), Ok(())) => {}
    (Err(workload_error), Ok(())) => return Err(workload_error),
    (Ok(()), Err(cleanup_error)) => return Err(cleanup_error),
    (Err(workload_error), Err(cleanup_error)) => {
      return Err(format!("soak workload and cleanup failed: workload: {workload_error}; cleanup: {cleanup_error}"));
    }
  }
  Ok(())
}

fn append_checkpoint(writer: &mut File, operation: char, path: &str) -> Result<(), String> {
  append_checkpoint_line(writer, operation, path)?;
  aeordb::engine::native_durability::sync_file_data_native(writer)
    .map_err(|error| format!("checkpoint durability barrier failed for {operation} {path}: {error}"))
}

fn append_checkpoint_line(writer: &mut impl Write, operation: char, path: &str) -> Result<(), String> {
  writeln!(writer, "{operation}\t{path}").map_err(|error| format!("checkpoint write failed for {operation} {path}: {error}"))?;
  writer.flush().map_err(|error| format!("checkpoint flush failed for {operation} {path}: {error}"))
}

#[cfg(test)]
#[path = "../../spec/soak_worker_internal_spec.rs"]
mod soak_worker_internal_spec;

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
  ops.store_file_from_reader(&ctx, &aeordb_path, reader, Some(&content_type)).map_err(|e| format!("store_file_from_reader: {}", e))?;

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

fn build_corpus(root: &str) -> Result<Vec<PathBuf>, String> {
  let mut out = Vec::new();
  let mut stack = vec![PathBuf::from(root)];
  while let Some(dir) = stack.pop() {
    let entries = std::fs::read_dir(&dir).map_err(|error| format!("read source corpus directory {}: {error}", dir.display()))?;
    for entry_result in entries {
      let entry = entry_result.map_err(|error| format!("enumerate source corpus directory {}: {error}", dir.display()))?;
      // Skip symlinks unconditionally — they can point outside the source
      // root or form cycles. We only want regular files / regular dirs.
      let entry_path = entry.path();
      let meta = entry_path.symlink_metadata().map_err(|error| format!("inspect source corpus entry {}: {error}", entry_path.display()))?;
      let file_type = meta.file_type();
      if file_type.is_symlink() {
        continue;
      } else if file_type.is_dir() {
        stack.push(entry_path);
      } else if file_type.is_file() {
        out.push(entry_path);
      }
    }
  }
  Ok(out)
}

// ---------------------------------------------------------------------------
// Checkpoint
// ---------------------------------------------------------------------------

fn load_checkpoint(path: &Path) -> Result<HashSet<String>, String> {
  let mut file = match OpenOptions::new().read(true).write(true).open(path) {
    Ok(file) => file,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
    Err(error) => return Err(format!("open checkpoint {}: {error}", path.display())),
  };
  let file_length = file.metadata().map_err(|error| format!("inspect checkpoint {}: {error}", path.display()))?.len();
  let mut reader = BufReader::new(&mut file);
  let mut set = HashSet::new();
  let mut line_bytes = Vec::with_capacity(CHECKPOINT_RECORD_MAX_BYTES.min(8 * 1_024));
  let mut valid_byte_frontier = 0u64;
  let mut line_number = 0usize;
  let mut incomplete_tail = false;

  loop {
    line_bytes.clear();
    let bytes_read = Read::by_ref(&mut reader)
      .take((CHECKPOINT_RECORD_MAX_BYTES + 1) as u64)
      .read_until(b'\n', &mut line_bytes)
      .map_err(|error| format!("read checkpoint {} line {}: {error}", path.display(), line_number + 1))?;
    if bytes_read == 0 {
      break;
    }
    line_number += 1;

    let bytes_read =
      u64::try_from(bytes_read).map_err(|error| format!("checkpoint {} line {line_number} length overflow: {error}", path.display()))?;
    let observed_byte_frontier = valid_byte_frontier
      .checked_add(bytes_read)
      .ok_or_else(|| format!("checkpoint {} byte frontier overflow at line {line_number}", path.display()))?;
    if !line_bytes.ends_with(b"\n") {
      if observed_byte_frontier < file_length {
        return Err(format!(
          "checkpoint {} line {line_number} exceeds the {CHECKPOINT_RECORD_MAX_BYTES}-byte record limit",
          path.display()
        ));
      }
      incomplete_tail = true;
      break;
    }
    if line_bytes.len() > CHECKPOINT_RECORD_MAX_BYTES {
      return Err(format!("checkpoint {} line {line_number} exceeds the {CHECKPOINT_RECORD_MAX_BYTES}-byte record limit", path.display()));
    }

    line_bytes.pop();
    if line_bytes.ends_with(b"\r") {
      line_bytes.pop();
    }
    if line_bytes.len() > CHECKPOINT_PAYLOAD_MAX_BYTES {
      return Err(format!(
        "checkpoint {} line {line_number} exceeds the {CHECKPOINT_PAYLOAD_MAX_BYTES}-byte payload limit",
        path.display()
      ));
    }
    let line = std::str::from_utf8(&line_bytes)
      .map_err(|error| format!("read checkpoint {} line {line_number}: invalid UTF-8: {error}", path.display()))?;
    if let Some(rest) = line.strip_prefix("+\t") {
      set.insert(rest.to_string());
    } else if let Some(rest) = line.strip_prefix("!\t") {
      set.remove(rest);
    } else if let Some(rest) = line.strip_prefix("?\t") {
      set.remove(rest);
    } else if let Some(rest) = line.strip_prefix("-\t") {
      set.remove(rest);
    } else {
      return Err(format!("malformed checkpoint {} line {line_number}: expected +, !, ?, or - operation", path.display()));
    }
    valid_byte_frontier = observed_byte_frontier;
  }
  drop(reader);

  if incomplete_tail {
    file
      .set_len(valid_byte_frontier)
      .map_err(|error| format!("truncate incomplete checkpoint tail {} at byte {valid_byte_frontier}: {error}", path.display()))?;
    aeordb::engine::native_durability::sync_file_data_native(&file)
      .map_err(|error| format!("checkpoint tail truncation durability barrier failed for {}: {error}", path.display()))?;
  }
  Ok(set)
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

fn open_metrics(path: &str) -> Result<BufWriter<File>, String> {
  let needs_header = !Path::new(path).exists();
  let file = OpenOptions::new().create(true).append(true).open(path).map_err(|e| format!("open metrics: {}", e))?;
  let mut writer = BufWriter::new(file);
  if needs_header {
    writeln!(writer, "{}", METRICS_HEADER).map_err(|e| format!("write header: {}", e))?;
    writer.flush().map_err(|error| format!("flush metrics header: {error}"))?;
  }
  Ok(writer)
}

struct MemoryStats {
  rss_kb: u64,
  data_kb: u64,
  size_kb: u64,
  hwm_kb: u64,
}

fn read_self_memory_stats() -> Result<MemoryStats, String> {
  // Cross-platform process memory via the engine's rss_sampler. The
  // sampler reads /proc/self/status on Linux and Mach task_info on macOS;
  // both report bytes the kernel attributes to the current process. Values
  // it can't observe (VmData on macOS) come back as 0.
  let m = aeordb::engine::rss_sampler::try_read_process_memory().map_err(|error| format!("process memory sample failed: {error}"))?;
  Ok(MemoryStats { rss_kb: m.resident_kb, data_kb: m.data_kb, size_kb: m.virtual_kb, hwm_kb: m.peak_resident_kb })
}

fn count_fds() -> Option<usize> {
  // /proc/self/fd on Linux, /dev/fd on macOS — both yield one dirent per
  // open file descriptor in the current process. /dev/fd actually works on
  // Linux too (it's a /proc/self/fd symlink there) but we keep the explicit
  // branch so the intent is obvious.
  let path = if cfg!(target_os = "linux") { "/proc/self/fd" } else { "/dev/fd" };
  std::fs::read_dir(path).ok().map(|iter| iter.count())
}

fn database_size_bytes(path: &Path) -> Result<u64, String> {
  std::fs::metadata(path).map(|metadata| metadata.len()).map_err(|error| format!("database metadata {}: {error}", path.display()))
}

fn set_last_action(last_action: &Mutex<String>, value: impl Into<String>) -> Result<(), String> {
  let mut action = last_action.lock().map_err(|_| "last-action diagnostics lock is poisoned".to_string())?;
  *action = value.into();
  Ok(())
}

fn read_last_action(last_action: &Mutex<String>) -> Result<String, String> {
  last_action.lock().map(|action| action.clone()).map_err(|_| "last-action diagnostics lock is poisoned".to_string())
}

fn record_workload_failure(last_action: &Mutex<String>, primary_error: String) -> String {
  match set_last_action(last_action, format!("FAIL {primary_error}")) {
    Ok(()) => primary_error,
    Err(diagnostic_error) => format!("{primary_error}; additionally failed to record diagnostic state: {diagnostic_error}"),
  }
}

// ---------------------------------------------------------------------------
// Summarize
// ---------------------------------------------------------------------------

fn summarize(path: &str) -> Result<(), String> {
  let file = File::open(path).map_err(|e| format!("open {}: {}", path, e))?;
  let mut rows: Vec<Row> = Vec::new();
  for (i, line_result) in BufReader::new(file).lines().enumerate() {
    let line = line_result.map_err(|error| format!("read metrics row {} from {}: {error}", i + 1, path))?;
    if i == 0 || line.starts_with("iso_time") {
      continue;
    }
    rows.push(parse_row(&line).map_err(|error| format!("malformed metrics row {}: {error}", i + 1))?);
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
  let rss_growth_pct =
    if baseline_warm.rss_kb > 0 { 100.0 * (last.rss_kb as f64 - baseline_warm.rss_kb as f64) / baseline_warm.rss_kb as f64 } else { 0.0 };
  println!("  growth T+1h→end:  {:+.1}%", rss_growth_pct);
  println!();
  println!("VmData (heap+data) — leaks show up here, not in RSS:");
  println!("  T+0:              {} MB", first.data_kb / 1024);
  println!("  T+1h:             {} MB", baseline_warm.data_kb / 1024);
  println!("  T+end:            {} MB", last.data_kb / 1024);
  let data_growth_pct = if baseline_warm.data_kb > 0 {
    100.0 * (last.data_kb as f64 - baseline_warm.data_kb as f64) / baseline_warm.data_kb as f64
  } else {
    0.0
  };
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
  let fd_min = rows.iter().filter_map(|row| row.fd_count).min();
  let fd_max = rows.iter().filter_map(|row| row.fd_count).max();
  println!("  min:              {}", fd_min.map(|value| value.to_string()).unwrap_or_else(|| "unavailable".to_string()));
  println!("  max:              {}", fd_max.map(|value| value.to_string()).unwrap_or_else(|| "unavailable".to_string()));
  println!();
  println!("DB size:");
  println!("  T+0:              {:.2} MB", first.db_size_bytes as f64 / 1_048_576.0);
  println!("  T+end:            {:.2} MB", last.db_size_bytes as f64 / 1_048_576.0);
  println!("  growth:           {:+.2} MB", (last.db_size_bytes as f64 - first.db_size_bytes as f64) / 1_048_576.0);
  println!();

  let mut pass = true;
  let mut report = |label: &str, ok: bool, detail: String| {
    println!("  [{}] {}: {}", if ok { " OK " } else { "FAIL" }, label, detail);
    if !ok {
      pass = false;
    }
  };

  let rss_ok = rss_growth_pct <= 30.0;
  report("RSS growth ≤ 30% (T+1h→end)", rss_ok, format!("{:+.1}%", rss_growth_pct));

  let data_ok = data_growth_pct <= 30.0;
  report("VmData growth ≤ 30% (T+1h→end)", data_ok, format!("{:+.1}%", data_growth_pct));

  match fd_max {
    Some(fd_max) => report("FD count ≤ 500", fd_max <= 500, format!("max={fd_max}")),
    None => report("FD count ≤ 500", false, "unavailable".to_string()),
  }

  println!();
  println!("verdict: {}", if pass { "PASS" } else { "FAIL" });
  if !pass {
    process::exit(1);
  }
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
  fd_count: Option<usize>,
  db_size_bytes: u64,
  entry_count: u64,
  cache_perms: u64,
  cache_index: u64,
  cache_dir: u64,
}

fn parse_row(line: &str) -> Result<Row, String> {
  let cols: Vec<&str> = line.split('\t').collect();
  // 18 columns in v2 header. Earlier 12-column files are tolerated by falling
  // back to v1 positions for the fields we know about.
  let v2 = cols.len() >= 18;
  if cols.len() < 11 {
    return Err(format!("expected at least 11 columns, found {}", cols.len()));
  }
  let parse_u64 = |index: usize, name: &str| {
    cols[index].parse::<u64>().map_err(|_| format!("{name} column {} is not an unsigned integer: {:?}", index + 1, cols[index]))
  };
  let fd_count = if v2 {
    if cols[9] == "unavailable" {
      None
    } else {
      Some(cols[9].parse::<usize>().map_err(|_| format!("fd_count column 10 is not an unsigned integer or unavailable: {:?}", cols[9]))?)
    }
  } else {
    Some(cols[6].parse::<usize>().map_err(|_| format!("fd_count column 7 is not an unsigned integer: {:?}", cols[6]))?)
  };
  Ok(Row {
    elapsed_secs: parse_u64(1, "elapsed_secs")?,
    writes: parse_u64(2, "writes")?,
    reads: parse_u64(3, "reads")?,
    deletes: parse_u64(4, "deletes")?,
    rss_kb: parse_u64(5, "rss_kb")?,
    data_kb: if v2 { parse_u64(6, "vm_data_kb")? } else { 0 },
    hwm_kb: if v2 { parse_u64(8, "vm_hwm_kb")? } else { 0 },
    fd_count,
    db_size_bytes: if v2 { parse_u64(10, "db_size_bytes")? } else { parse_u64(7, "db_size_bytes")? },
    entry_count: if v2 { parse_u64(13, "entry_count")? } else { parse_u64(10, "entry_count")? },
    cache_perms: if v2 { parse_u64(14, "cache_perms")? } else { 0 },
    cache_index: if v2 { parse_u64(15, "cache_index")? } else { 0 },
    cache_dir: if v2 { parse_u64(16, "cache_dir")? } else { 0 },
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
