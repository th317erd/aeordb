use std::fs::File;
use std::time::Instant;

use aeordb::engine::file_record::FileRecord;
use aeordb::engine::{DirectoryOps, EngineFileStream, EntryType, StorageEngine};

pub struct ProbeConfig<'a> {
  pub database: &'a str,
  pub path: Option<&'a str>,
  pub http_path: Option<&'a str>,
  pub route_prefix: &'a str,
  pub read: bool,
  pub chunks: bool,
  pub list_files: bool,
  pub growth_stats: bool,
  pub wal_dump: bool,
  pub wal_tail_bytes: Option<usize>,
  pub diff_checkpoint: Option<&'a str>,
  pub path_history: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpPathMapping {
  pub request_path: String,
  pub route_prefix: String,
  pub db_path: String,
  pub prefix_matched: bool,
}

// Diagnostic helper: list /.aeordb-system/ contents in a database file.
pub fn run(config: ProbeConfig<'_>) {
  let path = config.database;
  let probe_path = config.path;

  // Subcommands that don't need to open the engine — handle first.
  if let Some(n) = config.wal_tail_bytes {
    return dump_wal_tail(path, n);
  }
  if let Some(arg) = probe_path {
    if let Some(n_str) = arg.strip_prefix("--wal-tail-bytes=") {
      let n: usize = match n_str.parse() {
        Ok(v) => v,
        Err(_) => {
          eprintln!("--wal-tail-bytes requires a usize, got: {}", n_str);
          std::process::exit(2);
        }
      };
      return dump_wal_tail(path, n);
    }
  }

  let engine = match StorageEngine::open(path) {
    Ok(e) => e,
    Err(e) => {
      eprintln!("Open failed: {}", e);
      std::process::exit(1);
    }
  };
  let ops = DirectoryOps::new(&engine);

  if config.growth_stats || probe_path == Some("--growth-stats") {
    return print_growth_stats(&engine);
  }

  if let Some(tsv_path) = config.diff_checkpoint {
    return diff_checkpoint(&engine, tsv_path);
  }
  if let Some(arg) = probe_path {
    if let Some(tsv_path) = arg.strip_prefix("--diff-checkpoint=") {
      return diff_checkpoint(&engine, tsv_path);
    }
  }

  if config.list_files || probe_path == Some("--list-files") {
    // Enumerate every FileRecord — try a couple of entry_version
    // settings so we cover whichever the DB was written with.
    use aeordb::engine::file_record::FileRecord;
    let hash_length = engine.hash_algo().hash_length();
    let entries = engine.entries_by_type(aeordb::engine::KV_TYPE_FILE_RECORD).unwrap_or_default();
    println!("FileRecord entries: {}", entries.len());
    let mut seen_paths = std::collections::HashSet::new();
    for (hash, value) in &entries {
      // Read entry header to get correct entry_version.
      let entry = match engine.get_entry_including_deleted(hash) {
        Ok(Some(e)) => e,
        _ => continue,
      };
      let version = entry.0.entry_version;
      match FileRecord::deserialize(value, hash_length, version) {
        Ok(record) => {
          if seen_paths.insert(record.path.clone()) {
            println!("  {} ({}B)", record.path, record.total_size);
          }
        }
        Err(e) => {
          println!("  [deser err: {} hash={} version={}]", e, hex::encode(&hash[..8.min(hash.len())]), version);
        }
      }
    }
    return;
  }

  if config.wal_dump || probe_path == Some("--wal-dump") {
    // Dump every DirectoryIndex entry — hash, length, first bytes
    // so we can see what /.aeordb-system content looks like across
    // its multiple versions.
    let algo = engine.hash_algo();
    let aeordb_system_dir_key = aeordb::engine::directory_path_hash("/.aeordb-system", &algo).unwrap();
    println!("/.aeordb-system dir_key = {}", hex::encode(&aeordb_system_dir_key));

    let dir_entries = engine.entries_by_type(aeordb::engine::KV_TYPE_DIRECTORY).unwrap_or_default();
    println!("DirectoryIndex entries (live KV): {}", dir_entries.len());
    for (hash, value) in &dir_entries {
      let hl = if value.len() == algo.hash_length() {
        format!(" hard-link → {}", hex::encode(&value[..8.min(value.len())]))
      } else {
        String::new()
      };
      let is_aeordb_sys = hash == &aeordb_system_dir_key;
      println!(
        "  {} len={}{}{}",
        hex::encode(&hash[..8.min(hash.len())]),
        value.len(),
        hl,
        if is_aeordb_sys { "  ← /.aeordb-system dir_key" } else { "" },
      );
    }
    return;
  }

  if config.path_history {
    return print_path_history(&engine, &config);
  }

  if config.http_path.is_some() || probe_path.is_some() {
    return print_path_probe(&engine, &ops, &config);
  }

  println!("--- /.aeordb-system/ ---");
  match ops.list_directory("/.aeordb-system") {
    Ok(items) => {
      if items.is_empty() {
        println!("  (empty)");
      }
      for e in items {
        println!("  {} (type={})", e.name, e.entry_type);
      }
    }
    Err(e) => println!("  ERROR: {}", e),
  }

  println!("--- /.aeordb-system/api-keys/ ---");
  match ops.list_directory("/.aeordb-system/api-keys") {
    Ok(items) => {
      if items.is_empty() {
        println!("  (empty)");
      }
      for e in items {
        println!("  {} ({}b)", e.name, e.total_size);
      }
    }
    Err(e) => println!("  ERROR: {}", e),
  }

  println!("--- /.aeordb-system/users/ ---");
  match ops.list_directory("/.aeordb-system/users") {
    Ok(items) => {
      if items.is_empty() {
        println!("  (empty)");
      }
      for e in items {
        println!("  {} ({}b)", e.name, e.total_size);
      }
    }
    Err(e) => println!("  ERROR: {}", e),
  }

  println!("--- /.aeordb-system/snapshots/ ---");
  match ops.list_directory("/.aeordb-system/snapshots") {
    Ok(items) => {
      println!("  count: {}", items.len());
    }
    Err(e) => println!("  ERROR: {}", e),
  }

  println!("--- /.aeordb-system/groups/ ---");
  match ops.list_directory("/.aeordb-system/groups") {
    Ok(items) => println!("  count: {}", items.len()),
    Err(e) => println!("  ERROR: {}", e),
  }

  // Count FLAG_SYSTEM entries by type
  use aeordb::engine::entry_header::FLAG_SYSTEM;
  let mut sys_files = 0u32;
  let mut sys_dirs = 0u32;
  let mut total_files = 0u32;
  let mut total_dirs = 0u32;
  let file_entries = engine.entries_by_type(aeordb::engine::KV_TYPE_FILE_RECORD).unwrap_or_default();
  for (hash, _value) in &file_entries {
    total_files += 1;
    if let Ok(Some((header, _key, _value))) = engine.get_entry_including_deleted(hash) {
      if header.flags & FLAG_SYSTEM != 0 {
        sys_files += 1;
      }
    }
  }
  let dir_entries = engine.entries_by_type(aeordb::engine::KV_TYPE_DIRECTORY).unwrap_or_default();
  for (hash, _value) in &dir_entries {
    total_dirs += 1;
    if let Ok(Some((header, _key, _value))) = engine.get_entry_including_deleted(hash) {
      if header.flags & FLAG_SYSTEM != 0 {
        sys_dirs += 1;
      }
    }
  }
  println!("--- FLAG_SYSTEM counts ---");
  println!("  FileRecords:     {} of {} have FLAG_SYSTEM", sys_files, total_files);
  println!("  DirectoryIndex:  {} of {} have FLAG_SYSTEM", sys_dirs, total_dirs);

  // Check if specific api keys exist by path hash
  println!("--- Direct lookups ---");
  let algo = engine.hash_algo();
  for uuid in &[
    "83120afe-eb67-435e-9021-7544a54e0c86",
    "edd1c91d-c5c7-490a-b490-3c46b135ea72",
    "10fae062-d2ed-4f2e-b742-4abc48088fd2",
    "cafc6f96-e263-4199-818a-b0090b206317",
  ] {
    let path = format!("/.aeordb-system/api-keys/{}", uuid);
    let path_key = aeordb::engine::file_path_hash(&path, &algo).unwrap();
    let exists = engine.has_entry(&path_key).unwrap_or(false);
    println!("  {}: {}", uuid, if exists { "PRESENT" } else { "missing" });
  }

  // Read the api-keys directory data raw
  let dir_path = "/.aeordb-system/api-keys";
  let dir_key = aeordb::engine::directory_path_hash(dir_path, &algo).unwrap();
  println!("--- Raw dir entry at {} ---", dir_path);
  if let Ok(Some((header, _key, value))) = engine.get_entry_including_deleted(&dir_key) {
    println!("  flags: {:#x}, value len: {}", header.flags, value.len());
    if value.len() == algo.hash_length() {
      println!("  hard link → {}", hex::encode(&value));
      // Follow the link
      if let Ok(Some((_h, _k, real_value))) = engine.get_entry_including_deleted(&value) {
        println!("  target len: {}", real_value.len());
      }
    }
  } else {
    println!("  not found");
  }
}

pub fn db_path_from_http_path(http_path: &str, route_prefix: &str) -> HttpPathMapping {
  let request_path = ensure_leading_slash(http_path.split('?').next().unwrap_or(http_path));
  let route_prefix = normalize_route_prefix(route_prefix);
  let prefix_with_slash = format!("{}/", route_prefix.trim_end_matches('/'));

  if request_path == route_prefix {
    return HttpPathMapping { request_path, route_prefix, db_path: "/".to_string(), prefix_matched: true };
  }

  if let Some(rest) = request_path.strip_prefix(&prefix_with_slash) {
    let db_path = aeordb::engine::path_utils::normalize_path(&format!("/{}", rest));
    return HttpPathMapping { request_path, route_prefix, db_path, prefix_matched: true };
  }

  HttpPathMapping { db_path: aeordb::engine::path_utils::normalize_path(&request_path), request_path, route_prefix, prefix_matched: false }
}

fn print_path_probe(engine: &StorageEngine, ops: &DirectoryOps<'_>, config: &ProbeConfig<'_>) {
  let normalized = if let Some(http_path) = config.http_path {
    let mapping = db_path_from_http_path(http_path, config.route_prefix);
    println!("=== HTTP path mapping ===");
    println!("Request path:  {}", mapping.request_path);
    println!("Route prefix:  {}", mapping.route_prefix);
    println!("DB path:       {}", mapping.db_path);
    println!("Prefix match:  {}", if mapping.prefix_matched { "yes" } else { "no" });
    if !mapping.prefix_matched {
      println!("WARNING: HTTP path did not start with route prefix; probing normalized request path as DB path.");
    }
    mapping.db_path
  } else {
    let p = config.path.expect("path probe requires --path or --http-path");
    let normalized = aeordb::engine::path_utils::normalize_path(p);
    println!("=== Probe path ===");
    println!("Input path:    {}", p);
    println!("DB path:       {}", normalized);
    normalized
  };

  let algo = engine.hash_algo();
  let dir_key = aeordb::engine::directory_path_hash(&normalized, &algo).unwrap();
  let file_key = aeordb::engine::file_path_hash(&normalized, &algo).unwrap();

  println!("--- Path keys ---");
  println!("dir path key:  {}", kv_summary(engine, &dir_key));
  println!("file path key: {}", kv_summary(engine, &file_key));

  print_dir_entry_diagnostics(engine, &dir_key, algo.hash_length());
  print_file_record_diagnostics(engine, ops, &normalized, config);
  print_parent_listing(engine, ops, &normalized);
  print_snapshot_records(engine);
}

fn print_path_history(engine: &StorageEngine, config: &ProbeConfig<'_>) {
  let Some(path) = config.path else {
    eprintln!("--path-history requires --path");
    std::process::exit(2);
  };

  let normalized = aeordb::engine::path_utils::normalize_path(path);
  let algo = engine.hash_algo();
  let hash_length = algo.hash_length();
  let dir_key = aeordb::engine::directory_path_hash(&normalized, &algo).unwrap();
  let file_key = aeordb::engine::file_path_hash(&normalized, &algo).unwrap();

  println!("=== Path history ===");
  println!("Input path:    {}", path);
  println!("DB path:       {}", normalized);
  println!("dir path key:  {}", hex::encode(&dir_key));
  println!("file path key: {}", hex::encode(&file_key));
  println!();
  println!("--- Current KV ---");
  println!("dir path key:  {}", kv_summary(engine, &dir_key));
  println!("file path key: {}", kv_summary(engine, &file_key));
  println!();
  println!("--- WAL matches ---");
  let hot_tail_voids = load_hot_tail_voids(config.database);

  let writer = match engine.writer_read_lock() {
    Ok(writer) => writer,
    Err(error) => {
      eprintln!("writer lock: {}", error);
      std::process::exit(1);
    }
  };
  let mut scanner = match writer.scan_entries_reporting() {
    Ok(scanner) => scanner,
    Err(error) => {
      eprintln!("scan entries: {}", error);
      std::process::exit(1);
    }
  };

  let mut matches = 0usize;
  for result in scanner.by_ref() {
    let scanned = match result {
      Ok(scanned) => scanned,
      Err(_) => continue,
    };

    let mut reasons = Vec::new();
    if scanned.key == dir_key {
      reasons.push("dir_path_key");
    }
    if scanned.key == file_key {
      reasons.push("file_path_key");
    }

    let mut file_record_summary = None;
    if scanned.header.entry_type == EntryType::FileRecord {
      if let Ok(record) = FileRecord::deserialize(&scanned.value, hash_length, scanned.header.entry_version) {
        if record.path == normalized {
          reasons.push("file_record_value_path");
          file_record_summary = Some(format!(
            " path={} size={} chunks={} updated_at={}",
            record.path,
            record.total_size,
            record.chunk_hashes.len(),
            format_timestamp_millis(record.updated_at)
          ));
        }
      }
    }

    if reasons.is_empty() {
      continue;
    }

    matches += 1;
    let live = match engine.get_kv_entry(&scanned.key) {
      Some(entry) if entry.offset == scanned.offset && entry.total_length == scanned.header.total_length => "LIVE exact".to_string(),
      Some(entry) => format!("not live; current offset={} len={} deleted={}", entry.offset, entry.total_length, entry.is_deleted()),
      None => "not live; key missing from KV".to_string(),
    };
    let value_summary = if scanned.value.len() == hash_length {
      format!(" hard-link={}", hex::encode(&scanned.value))
    } else {
      file_record_summary.unwrap_or_default()
    };
    let void_summary = match first_overlapping_void(scanned.offset, scanned.header.total_length, &hot_tail_voids) {
      Some((offset, size)) => format!(" voided_by={}..{}", offset, offset.saturating_add(size as u64)),
      None => String::new(),
    };
    println!(
      "offset={} len={} ts={} type={:?} key={} reasons={} status={}{}{}",
      scanned.offset,
      scanned.header.total_length,
      format_timestamp_millis(scanned.header.timestamp),
      scanned.header.entry_type,
      hex::encode(&scanned.key[..8.min(scanned.key.len())]),
      reasons.join(","),
      live,
      value_summary,
      void_summary,
    );
  }

  println!();
  println!("matches: {}", matches);
  if !scanner.skipped_regions.is_empty() {
    println!("skipped regions: {}", scanner.skipped_regions.len());
  }
}

fn load_hot_tail_voids(database: &str) -> Vec<aeordb::engine::hot_tail::VoidRecord> {
  let mut file = match File::open(database) {
    Ok(file) => file,
    Err(_) => return Vec::new(),
  };
  let Ok((header, _slot)) = aeordb::engine::file_header::read_active_header(&mut file) else {
    return Vec::new();
  };
  if header.hot_tail_offset == 0 {
    return Vec::new();
  }
  aeordb::engine::hot_tail::read_hot_tail(&mut file, header.hot_tail_offset, header.hash_algo.hash_length())
    .map(|payload| payload.voids)
    .unwrap_or_default()
}

fn first_overlapping_void(offset: u64, length: u32, voids: &[aeordb::engine::hot_tail::VoidRecord]) -> Option<(u64, u32)> {
  let end = offset.saturating_add(length as u64);
  voids
    .iter()
    .find(|void| {
      let void_end = void.offset.saturating_add(void.size as u64);
      offset < void_end && void.offset < end
    })
    .map(|void| (void.offset, void.size))
}

fn print_dir_entry_diagnostics(engine: &StorageEngine, dir_key: &[u8], hash_length: usize) {
  match engine.get_entry_including_deleted(dir_key) {
    Ok(Some((header, _key, value))) => {
      println!("Dir entry (incl deleted): flags={:#x}, type={:?}, value len={}", header.flags, header.entry_type, value.len());
      if value.len() == hash_length {
        println!("  hard-link: {}", hex::encode(&value));
        match engine.get_entry_including_deleted(&value) {
          Ok(Some((_h, _k, real))) => println!("  target len: {}", real.len()),
          Ok(None) => println!("  target MISSING (dangling hard-link)"),
          Err(e) => println!("  target lookup error: {}", e),
        }
      }
    }
    Ok(None) => println!("Dir entry (incl deleted): MISSING"),
    Err(e) => println!("Dir entry (incl deleted): ERROR {}", e),
  }

  match engine.get_entry(dir_key) {
    Ok(Some((header, _k, value))) => {
      println!("Dir entry (LIVE): flags={:#x}, type={:?}, value len={}", header.flags, header.entry_type, value.len());
      if value.len() == hash_length {
        println!("  hard-link: {}", hex::encode(&value));
        match engine.get_entry(&value) {
          Ok(Some((_h, _k, real))) => println!("  target LIVE len: {}", real.len()),
          Ok(None) => println!("  target MISSING from LIVE (would 404 in list_directory)"),
          Err(e) => println!("  target lookup error: {}", e),
        }
      }
    }
    Ok(None) => println!("Dir entry (LIVE): MISSING"),
    Err(e) => println!("Dir entry (LIVE): ERROR {}", e),
  }
}

fn print_file_record_diagnostics(engine: &StorageEngine, ops: &DirectoryOps<'_>, normalized: &str, config: &ProbeConfig<'_>) {
  println!("--- FileRecord ---");
  match ops.get_metadata(normalized) {
    Ok(Some(record)) => {
      println!("FileRecord: PRESENT");
      println!("  path: {}", record.path);
      println!("  content_type: {}", record.content_type.as_deref().unwrap_or("(none)"));
      println!("  total_size: {}", record.total_size);
      println!("  created_at: {}", format_timestamp_millis(record.created_at));
      println!("  updated_at: {}", format_timestamp_millis(record.updated_at));
      println!("  metadata_bytes: {}", record.metadata.len());
      println!("  chunk_count: {}", record.chunk_hashes.len());

      if config.chunks {
        print_chunk_diagnostics(engine, &record);
      }
    }
    Ok(None) => println!("FileRecord: MISSING"),
    Err(e) => println!("FileRecord: ERROR {}", e),
  }

  if config.read {
    print_stream_read(ops, normalized);
  }
}

fn print_parent_listing(engine: &StorageEngine, ops: &DirectoryOps<'_>, normalized: &str) {
  if let Some(parent) = aeordb::engine::path_utils::parent_path(normalized) {
    println!("--- Parent listing of {} ---", parent);
    match ops.list_directory(&parent) {
      Ok(entries) => {
        let name = aeordb::engine::path_utils::file_name(normalized).unwrap_or("");
        let mut found = false;
        for e in &entries {
          if e.name == name {
            found = true;
            println!("  ChildEntry: name={} type={} hash={}", e.name, e.entry_type, hex::encode(&e.hash));
            let child_hash = e.hash.clone();
            match engine.get_entry(&child_hash) {
              Ok(Some((h, _k, v))) => println!("    ChildEntry.hash LIVE: flags={:#x}, len={}", h.flags, v.len()),
              Ok(None) => println!("    ChildEntry.hash NOT LIVE"),
              Err(e) => println!("    ChildEntry.hash error: {}", e),
            }
            match engine.get_entry_including_deleted(&child_hash) {
              Ok(Some((h, _k, v))) => println!("    ChildEntry.hash incl-deleted: flags={:#x}, len={}", h.flags, v.len()),
              Ok(None) => println!("    ChildEntry.hash NOT FOUND in incl-deleted either"),
              Err(e) => println!("    ChildEntry.hash incl-del error: {}", e),
            }
          }
        }
        if !found {
          println!("  ChildEntry: MISSING");
        }
      }
      Err(e) => println!("  ERROR: {}", e),
    }
  }
}

fn print_snapshot_records(engine: &StorageEngine) {
  println!("--- Raw snapshot KV records ---");
  match engine.entries_by_type(aeordb::engine::KV_TYPE_SNAPSHOT) {
    Ok(entries) => {
      println!("  count: {}", entries.len());
      for (key, value) in entries.iter().take(5) {
        println!("    snapshot hash={} ({}b)", hex::encode(&key[..8.min(key.len())]), value.len());
      }
    }
    Err(e) => println!("  ERROR: {}", e),
  }
}

fn print_chunk_diagnostics(engine: &StorageEngine, record: &FileRecord) {
  println!("--- Chunks ---");
  if record.chunk_hashes.is_empty() {
    println!("  (no chunks)");
    return;
  }

  let display_limit = 20usize;
  let start = Instant::now();
  let mut kv_missing = 0usize;
  let mut verify_ok = 0usize;
  let mut verify_bytes = 0u64;
  let mut verify_errors = Vec::new();

  for (index, hash) in record.chunk_hashes.iter().enumerate() {
    if engine.get_kv_entry(hash).is_none() {
      kv_missing += 1;
    }

    let chunk_start = Instant::now();
    let verified = verify_single_chunk(engine, hash);
    match &verified {
      Ok(bytes) => {
        verify_ok += 1;
        verify_bytes += *bytes as u64;
      }
      Err(error) => {
        verify_errors.push(format!("chunk[{}] {}", index, error));
      }
    }

    if index < display_limit {
      match verified {
        Ok(bytes) => println!(
          "  chunk[{}]: {} verified_bytes={} verify_ms={:.3}",
          index,
          kv_summary(engine, hash),
          bytes,
          chunk_start.elapsed().as_secs_f64() * 1000.0,
        ),
        Err(error) => println!(
          "  chunk[{}]: {} verify_error={} verify_ms={:.3}",
          index,
          kv_summary(engine, hash),
          error,
          chunk_start.elapsed().as_secs_f64() * 1000.0,
        ),
      }
    }
  }

  if record.chunk_hashes.len() > display_limit {
    println!("  ... {} more chunks omitted from per-chunk output", record.chunk_hashes.len() - display_limit,);
  }

  println!(
    "  chunk summary: total={} kv_missing={} verified_ok={} verified_bytes={} errors={} elapsed_ms={:.3}",
    record.chunk_hashes.len(),
    kv_missing,
    verify_ok,
    verify_bytes,
    verify_errors.len(),
    start.elapsed().as_secs_f64() * 1000.0,
  );

  for error in verify_errors.iter().take(10) {
    println!("    verify error: {}", error);
  }
  if verify_errors.len() > 10 {
    println!("    ... {} more verify errors", verify_errors.len() - 10);
  }
}

fn verify_single_chunk(engine: &StorageEngine, hash: &[u8]) -> Result<usize, String> {
  let mut stream = EngineFileStream::from_chunk_hashes(vec![hash.to_vec()], engine).map_err(|error| error.to_string())?;
  match stream.next() {
    Some(Ok(bytes)) => Ok(bytes.len()),
    Some(Err(error)) => Err(error.to_string()),
    None => Ok(0),
  }
}

fn print_stream_read(ops: &DirectoryOps<'_>, normalized: &str) {
  println!("--- Verified read ---");
  let start = Instant::now();
  match ops.read_file_streaming(normalized) {
    Ok(stream) => {
      let expected_chunks = stream.chunk_count();
      let mut chunks = 0usize;
      let mut bytes = 0u64;
      for item in stream {
        match item {
          Ok(chunk) => {
            chunks += 1;
            bytes += chunk.len() as u64;
          }
          Err(error) => {
            println!(
              "verified stream read: ERROR after_chunks={} bytes={} error={} elapsed_ms={:.3}",
              chunks,
              bytes,
              error,
              start.elapsed().as_secs_f64() * 1000.0,
            );
            return;
          }
        }
      }
      let elapsed = start.elapsed().as_secs_f64();
      println!(
        "verified stream read: OK bytes={} chunks={}/{} elapsed_ms={:.3} throughput_mib_s={:.3}",
        bytes,
        chunks,
        expected_chunks,
        elapsed * 1000.0,
        throughput_mib_s(bytes, elapsed),
      );
    }
    Err(error) => println!("verified stream read: ERROR {} elapsed_ms={:.3}", error, start.elapsed().as_secs_f64() * 1000.0,),
  }
}

fn kv_summary(engine: &StorageEngine, hash: &[u8]) -> String {
  match engine.get_kv_entry(hash) {
    Some(entry) => format!(
      "PRESENT hash={} type={} flags={:#04x} offset={} len={} pending={} deleted={}",
      hex::encode(hash),
      entry.entry_type(),
      entry.flags(),
      entry.offset,
      entry.total_length,
      entry.is_pending(),
      entry.is_deleted(),
    ),
    None => format!("MISSING hash={}", hex::encode(hash)),
  }
}

fn normalize_route_prefix(route_prefix: &str) -> String {
  let with_slash = ensure_leading_slash(route_prefix);
  let trimmed = with_slash.trim_end_matches('/');
  if trimmed.is_empty() {
    "/".to_string()
  } else {
    trimmed.to_string()
  }
}

fn ensure_leading_slash(path: &str) -> String {
  if path.starts_with('/') {
    path.to_string()
  } else {
    format!("/{}", path)
  }
}

fn format_timestamp_millis(value: i64) -> String {
  match chrono::DateTime::<chrono::Utc>::from_timestamp_millis(value) {
    Some(timestamp) => format!("{} ({})", value, timestamp.to_rfc3339()),
    None => value.to_string(),
  }
}

fn throughput_mib_s(bytes: u64, elapsed_seconds: f64) -> f64 {
  if elapsed_seconds <= 0.0 {
    return 0.0;
  }
  bytes as f64 / (1024.0 * 1024.0) / elapsed_seconds
}

// ---------------------------------------------------------------------------
// --growth-stats
// ---------------------------------------------------------------------------
// Quick "is this DB actually growing / is recovery clean" report. Pulls
// `engine.stats()` for counts + reads the writer offset to compare against
// the on-disk file size. Surfaces three signals soak verify doesn't:
//   1. live KV entries by type (sanity for "is the worker doing work")
//   2. WAL frontier vs file size (catches "scanner stopped early; bytes
//      past frontier are not in any KV pointer")
//   3. void count + bytes (GC reclaim that may or may not be progressing)
// ---------------------------------------------------------------------------
fn print_growth_stats(engine: &aeordb::engine::StorageEngine) {
  let stats = engine.stats();
  let writer = match engine.writer_read_lock() {
    Ok(w) => w,
    Err(e) => {
      eprintln!("writer lock: {}", e);
      std::process::exit(1);
    }
  };
  let wal_end = writer.current_offset();
  let file_size = stats.db_file_size_bytes;
  let tail_gap: i128 = file_size as i128 - wal_end as i128;

  println!("=== growth-stats ===");
  println!("file size:           {} bytes ({:.2} GiB)", file_size, file_size as f64 / (1024.0 * 1024.0 * 1024.0));
  println!("wal end (writer):    {} bytes", wal_end);
  println!(
    "tail gap:            {} bytes ({})",
    tail_gap,
    if tail_gap == 0 {
      "clean"
    } else if tail_gap > 0 {
      "BYTES PAST FRONTIER — unrecovered tail"
    } else {
      "writer ahead of file (impossible?)"
    }
  );
  println!();
  println!("entries (total appended): {}", stats.entry_count);
  println!("kv entries (live):        {}", stats.kv_entries);
  println!("kv size bytes:            {}", stats.kv_size_bytes);
  println!();
  println!("by type:");
  println!("  file records:    {}", stats.file_count);
  println!("  directories:     {}", stats.directory_count);
  println!("  chunks:          {}", stats.chunk_count);
  println!("  snapshots:       {}", stats.snapshot_count);
  println!("  forks:           {}", stats.fork_count);
  println!("  voids:           {}  ({} bytes)", stats.void_count, stats.void_space_bytes);
  println!();
  println!("created_at: {}", stats.created_at);
  println!("updated_at: {}", stats.updated_at);
}

// ---------------------------------------------------------------------------
// --diff-checkpoint=<tsv>
// ---------------------------------------------------------------------------
// Differential check the soak-worker's checkpoint TSV against the DB. The
// worker writes `+\t<path>` for every successful store and `-\t<path>` for
// every successful delete, flushing after each line. After a SIGKILL, the
// checkpoint is the *intent*; the DB is the *outcome*. If a path is in the
// reconstructed "committed" set but missing from the DB, that's silent
// data loss the engine claimed to deliver.
//
// Reports four counts:
//   total_committed         — set after replaying + and - lines (intent)
//   present_in_db           — committed paths actually findable by file_path_hash
//   missing_in_db           — IN INTENT BUT NOT IN DB. This is the bug signal.
//   present_not_checkpointed — sanity; "I see file X but the checkpoint
//                              never mentioned it" — generally means an
//                              in-flight write before the checkpoint line
//                              flushed, or a system/internal file.
// ---------------------------------------------------------------------------
fn diff_checkpoint(engine: &aeordb::engine::StorageEngine, tsv_path: &str) {
  use std::collections::HashSet;
  use std::io::{BufRead, BufReader};
  use std::fs::File;

  let file = match File::open(tsv_path) {
    Ok(f) => f,
    Err(e) => {
      eprintln!("open checkpoint {}: {}", tsv_path, e);
      std::process::exit(1);
    }
  };

  // Reconstruct the worker's view: + adds, - removes. Match `load_checkpoint`
  // in aeordb-cli/src/bin/soak-worker.rs.
  let mut committed: HashSet<String> = HashSet::new();
  let mut lines = 0u64;
  let mut adds = 0u64;
  let mut dels = 0u64;
  for line in BufReader::new(file).lines().map_while(Result::ok) {
    lines += 1;
    if let Some(rest) = line.strip_prefix("+\t") {
      let path = rest.split_once('\t').map(|(path, _)| path).unwrap_or(rest);
      committed.insert(path.to_string());
      adds += 1;
    } else if let Some(rest) = line.strip_prefix("-\t") {
      committed.remove(rest);
      dels += 1;
    } else if let Some((path, _body)) = line.split_once('\t') {
      // crash-soak-worker records committed content as path<TAB>body.
      committed.insert(path.to_string());
      adds += 1;
    }
  }

  let algo = engine.hash_algo();
  let mut present = 0u64;
  let mut missing: Vec<String> = Vec::new();
  for path in &committed {
    let hash = match aeordb::engine::file_path_hash(path, &algo) {
      Ok(h) => h,
      Err(_) => {
        missing.push(path.clone());
        continue;
      }
    };
    match engine.has_entry(&hash) {
      Ok(true) => present += 1,
      Ok(false) => missing.push(path.clone()),
      Err(_) => missing.push(path.clone()),
    }
  }

  // Also gather "present-in-DB but not-in-checkpoint" — a weaker signal,
  // but useful for spotting in-flight writes that landed before the
  // checkpoint flush.
  let mut extras: Vec<String> = Vec::new();
  let entries = engine.entries_by_type(aeordb::engine::KV_TYPE_FILE_RECORD).unwrap_or_default();
  let hash_length = engine.hash_algo().hash_length();
  let mut db_paths = HashSet::new();
  for (hash, value) in &entries {
    let version = match engine.get_entry_including_deleted(hash) {
      Ok(Some(e)) => e.0.entry_version,
      _ => continue,
    };
    if let Ok(record) = aeordb::engine::file_record::FileRecord::deserialize(value, hash_length, version) {
      db_paths.insert(record.path);
    }
  }
  for p in &db_paths {
    if !committed.contains(p) && !p.starts_with("/.aeordb-system") {
      extras.push(p.clone());
    }
  }

  println!("=== diff-checkpoint ===");
  println!("checkpoint:              {}", tsv_path);
  println!("lines parsed:            {}  (+: {}, -: {})", lines, adds, dels);
  println!("committed intent (net):  {}", committed.len());
  println!("present in db:           {}", present);
  println!("MISSING from db:         {}  {}", missing.len(), if missing.is_empty() { "" } else { "← SILENT DATA LOSS" });
  println!("extras in db (not in checkpoint, non-system): {}", extras.len());

  if !missing.is_empty() {
    println!();
    println!("Missing paths (up to 30):");
    for p in missing.iter().take(30) {
      println!("  {}", p);
    }
    if missing.len() > 30 {
      println!("  ... ({} more)", missing.len() - 30);
    }
    std::process::exit(3); // distinct exit code for soak orchestrators
  }
}

// ---------------------------------------------------------------------------
// --wal-tail-bytes=<N>
// ---------------------------------------------------------------------------
// Hex-dump the last N bytes of the .aeordb file as-is, no engine open. For
// the rare case where the engine refuses to open (bad header) or you want
// to eyeball whatever the kernel actually persisted past the recovery
// frontier. Reads raw bytes — no parsing.
// ---------------------------------------------------------------------------
fn dump_wal_tail(path: &str, n: usize) {
  use std::io::{Read, Seek, SeekFrom};
  let mut file = match std::fs::File::open(path) {
    Ok(f) => f,
    Err(e) => {
      eprintln!("open {}: {}", path, e);
      std::process::exit(1);
    }
  };
  let total = match file.metadata().map(|m| m.len()) {
    Ok(v) => v,
    Err(e) => {
      eprintln!("metadata: {}", e);
      std::process::exit(1);
    }
  };
  let from = total.saturating_sub(n as u64);
  if let Err(e) = file.seek(SeekFrom::Start(from)) {
    eprintln!("seek: {}", e);
    std::process::exit(1);
  }
  let mut buf = vec![0u8; n];
  let read = file.read(&mut buf).unwrap_or(0);
  buf.truncate(read);

  println!("=== wal-tail-bytes (last {} of {}, starting offset {}) ===", read, total, from);
  // 16 bytes per row, offset | hex | ascii
  for (i, chunk) in buf.chunks(16).enumerate() {
    let off = from + (i as u64 * 16);
    let hex: String = chunk.iter().map(|b| format!("{:02x} ", b)).collect();
    let ascii: String = chunk.iter().map(|&b| if (32..127).contains(&b) { b as char } else { '.' }).collect();
    println!("{:012x}  {:<48}  |{}|", off, hex, ascii);
  }
}
