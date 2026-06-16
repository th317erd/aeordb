use std::process;
use std::io::{self, Write};

use aeordb::engine::emergency_spill::{self, EmergencySpillApplyReport, EmergencySpillArtifact};
use aeordb::engine::StorageEngine;
use aeordb::engine::verify;
use aeordb::logging::{LogConfig, LogFormat, initialize_logging};
use crate::utils::format_bytes;

pub fn run(database: &str, repair: bool, force_fix_in_place: bool, yes: bool) {
  // Initialize logging so debug/trace output works with AEORDB_LOG env var.
  initialize_logging(&LogConfig { format: LogFormat::Pretty, level: "warn".to_string(), ..LogConfig::default() });

  println!("AeorDB Integrity Check");
  println!("=======================");
  println!();

  let emergency_spills = match emergency_spill::scan_unapplied_for_database(database) {
    Ok(artifacts) => artifacts,
    Err(error) => {
      eprintln!("Error: failed to scan emergency spill locations: {}", error);
      process::exit(1);
    }
  };

  if !emergency_spills.is_empty() {
    if !repair {
      eprintln!("Fatal: unresolved emergency spill artifacts were found for this database.");
      print_emergency_spill_summary(&emergency_spills);
      eprintln!();
      eprintln!("Run repair before starting or verifying normally:");
      eprintln!("  aeordb verify --repair --force-fix-in-place -D {}", database);
      eprintln!("For unattended repair after reviewing the artifacts, add --yes.");
      process::exit(2);
    }
    if !force_fix_in_place {
      eprintln!("Fatal: emergency spill recovery must be applied in-place.");
      eprintln!("The spill artifacts are tied to the original database path and startup remains blocked until they are marked applied.");
      eprintln!();
      eprintln!("Run:");
      eprintln!("  aeordb verify --repair --force-fix-in-place -D {}", database);
      eprintln!("For unattended repair after reviewing the artifacts, add --yes.");
      process::exit(1);
    }
    print_emergency_spill_summary(&emergency_spills);
    if !yes && !confirm_emergency_spill_replay() {
      eprintln!("Aborted. No emergency spill artifacts were applied.");
      process::exit(1);
    }
    println!();
  }

  // If repairing without --force-fix-in-place, work on a copy.
  let work_path = if repair && !force_fix_in_place {
    let repaired_path = format!("{}.repaired", database);
    if std::path::Path::new(&repaired_path).exists() {
      eprintln!("Error: {} already exists.", repaired_path);
      eprintln!("Remove it first, or use --force-fix-in-place to repair the original.");
      process::exit(1);
    }
    // Flush the hot tail into the main file before copying, so the
    // repaired copy starts with a fully-consistent on-disk state.
    println!("Flushing hot tail before copy...");
    match StorageEngine::open(database) {
      Ok(flush_engine) => {
        if let Err(e) = flush_engine.shutdown() {
          eprintln!("Warning: hot-tail flush failed: {}", e);
        }
        drop(flush_engine);
      }
      Err(e) => {
        eprintln!("Warning: could not open database for hot-tail flush: {}", e);
      }
    }

    println!("Creating repaired copy: {}", repaired_path);
    if let Err(e) = std::fs::copy(database, &repaired_path) {
      eprintln!("Failed to copy database: {}", e);
      process::exit(1);
    }
    println!();
    repaired_path
  } else {
    database.to_string()
  };

  let spill_apply_report = if repair && !emergency_spills.is_empty() {
    println!("Applying emergency WAL-tail spill artifacts...");
    match emergency_spill::apply_wal_tails_to_database(&work_path, &emergency_spills) {
      Ok(report) => {
        println!(
          "  Applied {} artifact(s): {} WAL tail(s), {} already present, {} written.",
          report.artifact_count,
          report.wal_tails_seen,
          format_bytes(report.wal_tail_bytes_present),
          format_bytes(report.wal_tail_bytes_written)
        );
        Some(report)
      }
      Err(error) => {
        eprintln!("Emergency spill replay failed: {}", error);
        process::exit(1);
      }
    }
  } else {
    None
  };

  let engine = match StorageEngine::open(&work_path) {
    Ok(engine) => engine,
    Err(open_error) => {
      // Some databases can't even be opened — old format version, header
      // CRC failure, or a hot_tail_offset that points past EOF (the
      // 2026-05-11 xenocept corruption mode). When --repair is set, try
      // a low-level header repair first: rewrite the header in the
      // current format, reset hot_tail_offset if it's past EOF, then
      // reopen. StorageEngine::open's dirty-startup path rebuilds the
      // KV from a full WAL scan and recovers the data.
      if !repair {
        eprintln!("Error opening database: {}", open_error);
        eprintln!();
        eprintln!("  Run with --repair to attempt low-level header recovery:");
        eprintln!("    aeordb verify --repair -D {}", database);
        process::exit(1);
      }

      println!("Initial open failed: {}", open_error);
      println!("Attempting low-level header repair...");

      match aeordb::engine::inspect_header(&work_path) {
        Ok(inspect) => {
          if inspect.bad_magic {
            eprintln!("File is not an AeorDB database (bad magic). Refusing to touch it.");
            process::exit(1);
          }
          if let Some(ref m) = inspect.hot_tail_past_eof {
            println!("  hot_tail_offset {} > file size {} (off by {} bytes)", m.recorded_offset, m.actual_file_size, m.bytes_past_eof,);
          }
          if let Some((from, to)) = inspect.upgraded_version {
            println!("  header version v{} → v{}", from, to);
          }
          if inspect.crc_failed {
            println!("  header CRC mismatch");
          }
        }
        Err(error) => {
          eprintln!("Header inspect failed: {}", error);
          process::exit(1);
        }
      }

      match aeordb::engine::repair_header_in_place(&work_path) {
        Ok(report) if report.repaired => {
          println!("  Header repaired. Reopening to trigger dirty startup...");
        }
        Ok(_) => {
          println!("  Header was already clean — nothing to repair at the header level.");
        }
        Err(error) => {
          eprintln!("Header repair failed: {}", error);
          process::exit(1);
        }
      }

      match StorageEngine::open(&work_path) {
        Ok(engine) => {
          println!("  Reopened successfully (dirty startup rebuilt the KV index from WAL)");
          println!();
          engine
        }
        Err(error) => {
          eprintln!("Reopen after header repair still failed: {}", error);
          eprintln!("The damage extends beyond the header — escalate.");
          process::exit(1);
        }
      }
    }
  };

  if spill_apply_report.is_some() {
    println!("Forcing WAL rebuild and reusable-gap recovery after emergency spill replay...");
    if let Err(error) = engine.recover_after_emergency_spill_replay() {
      eprintln!("Emergency spill post-replay recovery failed: {}", error);
      process::exit(1);
    }
    println!("  WAL rebuild and hot-tail republish complete.");
    println!();
  }

  let mut report = if repair {
    if force_fix_in_place {
      println!("Running with --repair --force-fix-in-place...");
    } else {
      println!("Running with --repair (on copy)...");
    }
    println!();

    // Phase 1: Verify to determine what's needed
    let initial = verify::verify(&engine, &work_path);

    if initial.missing_kv_entries > 0 || initial.stale_kv_entries > 0 {
      // Check if KV expansion is needed
      let hash_length = engine.hash_algo().hash_length();
      let psize = aeordb::engine::kv_pages::page_size(hash_length);
      let needed_stage = aeordb::engine::kv_pages::stage_for_count(initial.valid_entries as usize, hash_length);
      let (needed_size, _) = aeordb::engine::kv_stages::stage_params(needed_stage, psize);
      let current_size = engine.writer_read_lock().map(|w| w.file_header().kv_block_length).unwrap_or(0);

      if needed_size > current_size && current_size > 0 {
        // Phase 2: Expand KV block (requires dropping the engine)
        println!("Expanding KV block: {} → {} bytes (stage {})", current_size, needed_size, needed_stage);
        engine.shutdown().ok();
        drop(engine);

        match aeordb::engine::kv_expand::expand_kv_block(&work_path, needed_stage, hash_length) {
          Ok((_size, _stage, delta)) => {
            println!("WAL entries relocated forward by {} bytes", delta);
          }
          Err(e) => {
            eprintln!("KV expansion failed: {}", e);
            process::exit(1);
          }
        }

        // Phase 3: Reopen and rebuild
        let engine2 = match StorageEngine::open(&work_path) {
          Ok(e) => e,
          Err(e) => {
            eprintln!("Failed to reopen after expansion: {}", e);
            process::exit(1);
          }
        };

        verify::verify_and_repair(&engine2, &work_path)
      } else {
        // No expansion needed — repair in place
        verify::verify_and_repair(&engine, &work_path)
      }
    } else {
      // No KV issues — just run repair for other issues
      verify::verify_and_repair(&engine, &work_path)
    }
  } else {
    verify::verify(&engine, database)
  };

  if let Some(apply_report) = &spill_apply_report {
    report.repairs.push(format_emergency_spill_repair_summary(apply_report));
  }

  if repair && !emergency_spills.is_empty() && !report.has_issues() {
    if let Err(error) = emergency_spill::mark_artifacts_applied(&work_path, &emergency_spills, spill_apply_report.as_ref().unwrap()) {
      eprintln!("Repair succeeded, but failed to mark emergency spill artifacts applied: {}", error);
      eprintln!("Startup will continue to refuse this database until the artifacts are resolved.");
      process::exit(1);
    }
    report.repairs.push(format!("Emergency spill artifacts marked applied: {}", emergency_spills.len()));
  }

  // Print report
  println!("Database: {}", report.db_path);
  println!("File size: {}", format_bytes(report.file_size));
  println!("Hash algorithm: {}", report.hash_algorithm);
  println!();

  println!("Entry Summary:");
  println!("  Total entries:      {:>8}", report.total_entries);
  println!("  Chunks:             {:>8}", report.chunks);
  println!("  File records:       {:>8}", report.file_records);
  println!("  Directory indexes:  {:>8}", report.directory_indexes);
  println!("  Symlinks:           {:>8}", report.symlinks);
  println!("  Snapshots:          {:>8}", report.snapshots);
  println!("  Deletion records:   {:>8}", report.deletion_records);
  println!("  Forks:              {:>8}", report.forks);
  println!("  Voids:              {:>8}  ({})", report.voids, format_bytes(report.void_bytes));
  println!();

  println!("Integrity:");
  println!("  Valid:              {:>8}", report.valid_entries);
  println!("  Corrupt hash:       {:>8}", report.corrupt_hash);
  println!("  Corrupt header:     {:>8}", report.corrupt_header);
  if !report.skipped_regions.is_empty() {
    for (offset, len) in &report.skipped_regions {
      println!("  Skipped region:     {} bytes at offset {}", len, offset);
    }
  }
  println!();

  println!("Storage:");
  println!("  Logical data:  {}", format_bytes(report.logical_data_size));
  println!("  Chunk data:    {}", format_bytes(report.chunk_data_size));
  println!("  Dedup savings: {}", format_bytes(report.dedup_savings));
  println!("  Void space:    {}", format_bytes(report.void_bytes));
  println!();

  println!("Directory Consistency:");
  println!("  Directories:        {:>8}", report.directories_checked);
  println!("  Missing children:   {:>8}", report.missing_children.len());
  for mc in &report.missing_children {
    println!("    - {}", mc);
  }
  println!("  Dangling records:   {:>8}", report.dangling_file_records.len());
  for dangling in &report.dangling_file_records {
    println!("    - {}", dangling);
  }
  println!("  Unlisted files:     {:>8}", report.unlisted_files.len());
  for uf in &report.unlisted_files {
    println!("    - {}", uf);
  }
  println!();

  println!("KV Index:");
  println!("  KV entries:         {:>8}", report.kv_entries);
  println!("  Stale entries:      {:>8}", report.stale_kv_entries);
  for stale in &report.stale_kv_details {
    println!("    - {}", stale);
  }
  println!("  Missing entries:    {:>8}", report.missing_kv_entries);
  for missing in &report.missing_kv_details {
    println!("    - {}", missing);
  }
  println!("  Invalid offsets:    {:>8}", report.invalid_kv_offsets.len());
  for invalid in &report.invalid_kv_offsets {
    println!("    - {}", invalid);
  }
  println!("  Invalid voids:      {:>8}", report.invalid_hot_tail_voids.len());
  for invalid in &report.invalid_hot_tail_voids {
    println!("    - {}", invalid);
  }
  println!();

  println!("Snapshot Integrity:");
  println!("  Snapshots checked:  {:>8}", report.snapshots_checked);
  println!("  Broken snapshots:   {:>8}", report.broken_snapshots.len());
  for bs in &report.broken_snapshots {
    println!("    - {}", bs);
  }
  println!();

  if !report.stale_dir_path_keys.is_empty() {
    println!("Stale dir_key entries ({} found — typically caused by snapshot_restore + GC):", report.stale_dir_path_keys.len());
    for p in report.stale_dir_path_keys.iter().take(10) {
      println!("  - {}", p);
    }
    if report.stale_dir_path_keys.len() > 10 {
      println!("  ... and {} more", report.stale_dir_path_keys.len() - 10);
    }
    println!();
  }

  if !report.repairs.is_empty() {
    println!("Repairs:");
    for r in &report.repairs {
      println!("  + {}", r);
    }
    println!();
  }

  if report.has_issues() {
    println!("Status: ISSUES FOUND");
    if !repair {
      println!();
      if report.missing_kv_entries > 0 {
        println!("  KV index is incomplete ({} entries missing from index).", report.missing_kv_entries);
        println!("  The data is in the WAL but the index doesn't point to it.");
        println!("  Repair will rebuild the KV index from the WAL.");
      }
      if report.stale_kv_entries > 0 {
        println!("  KV index has {} stale entries pointing to outdated data.", report.stale_kv_entries);
        println!("  Repair will rebuild the KV index from the WAL.");
      }
      if !report.invalid_kv_offsets.is_empty() {
        println!("  {} KV entries point outside the current WAL region.", report.invalid_kv_offsets.len());
        println!("  Repair should rebuild the KV index from the WAL.");
      }
      if !report.invalid_hot_tail_voids.is_empty() {
        println!("  {} hot-tail void records point outside the current WAL region.", report.invalid_hot_tail_voids.len());
        println!("  These voids will be ignored by newer runtimes and should be removed by repair.");
      }
      if report.corrupt_hash > 0 {
        println!("  {} entries have corrupt content (hash mismatch).", report.corrupt_hash);
        println!("  These entries may have been damaged by disk errors.");
      }
      if report.corrupt_header > 0 {
        println!("  {} entries have corrupt headers.", report.corrupt_header);
        println!("  These entries are unreadable and will be skipped.");
      }
      if !report.missing_children.is_empty() {
        println!("  {} files are listed in directories but can't be read.", report.missing_children.len());
      }
      if !report.dangling_file_records.is_empty() {
        println!("  {} live path-key FileRecords reference chunks that are not live.", report.dangling_file_records.len());
      }
      if !report.broken_snapshots.is_empty() {
        println!("  {} snapshots reference data that no longer exists.", report.broken_snapshots.len());
        println!("  This is typically caused by GC sweeping entries that snapshots");
        println!("  still reference (a bug fixed in this version). The snapshot");
        println!("  metadata is intact but the file data it points to is gone.");
        println!("  These snapshots can be deleted with: aeordb snapshot delete <name>");
      }
      println!();
      println!("  Run with --repair to auto-fix recoverable issues:");
      println!("    aeordb verify --repair -D {}", database);
    }
    process::exit(2);
  } else {
    println!("Status: OK");
    if repair && !force_fix_in_place {
      println!();
      println!("Repaired copy: {}", format!("{}.repaired", database));
      println!("To use it, replace the original:");
      println!("  mv {}.repaired {}", database, database);
    }
  }
}

fn print_emergency_spill_summary(artifacts: &[EmergencySpillArtifact]) {
  println!("Emergency spill artifacts found: {}", artifacts.len());
  println!("These will be applied oldest-first before normal repair:");
  for (index, artifact) in artifacts.iter().enumerate() {
    println!("  {}. {}", index + 1, artifact.attempted_at.as_deref().unwrap_or("unknown time"));
    println!("     directory: {}", artifact.directory.display());
    println!("     manifest:  {}", artifact.manifest_path.display());
    if let Some(context) = artifact.context.as_deref() {
      println!("     context:   {}", context);
    }
    if let Some(failure) = artifact.failure.as_deref() {
      println!("     failure:   {}", failure);
    }
    println!("     hot-tail:  {} writes, {} voids", artifact.hot_tail_writes, artifact.hot_tail_voids);
    if let Some(path) = artifact.hot_tail_path.as_ref() {
      println!("                {}", path.display());
    }
    if let Some(path) = artifact.wal_tail_path.as_ref() {
      println!(
        "     WAL tail:  {} bytes at {:?}..{:?}{}",
        format_bytes(artifact.wal_tail_bytes),
        artifact.wal_tail_copy_start,
        artifact.wal_tail_end,
        if artifact.wal_tail_truncated { " (truncated)" } else { "" }
      );
      println!("                {}", path.display());
    } else {
      println!("     WAL tail:  none recorded");
    }
  }
  println!();
  println!("Repair will copy matching WAL-tail bytes into the database, force a WAL-to-EOF KV rebuild, recover reusable gaps, and republish the hot tail.");
}

fn confirm_emergency_spill_replay() -> bool {
  print!("Proceed with emergency spill replay and repair? [y/N] ");
  let _ = io::stdout().flush();
  let mut answer = String::new();
  if io::stdin().read_line(&mut answer).is_err() {
    return false;
  }
  matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn format_emergency_spill_repair_summary(report: &EmergencySpillApplyReport) -> String {
  format!(
    "Emergency spill replay: {} artifact(s), {} WAL tail(s), {} already present, {} written; forced WAL rebuild and void gap recovery.",
    report.artifact_count,
    report.wal_tails_seen,
    format_bytes(report.wal_tail_bytes_present),
    format_bytes(report.wal_tail_bytes_written)
  )
}
