use std::process;

use aeordb::engine::StorageEngine;
use aeordb::engine::verify;
use aeordb::logging::{LogConfig, LogFormat, initialize_logging};
use crate::utils::format_bytes;

pub fn run(database: &str, repair: bool, force_fix_in_place: bool) {
    // Initialize logging so debug/trace output works with AEORDB_LOG env var.
    initialize_logging(&LogConfig {
        format: LogFormat::Pretty,
        level: "warn".to_string(),
        ..LogConfig::default()
    });

    println!("AeorDB Integrity Check");
    println!("=======================");
    println!();

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
                        println!(
                            "  hot_tail_offset {} > file size {} (off by {} bytes)",
                            m.recorded_offset, m.actual_file_size, m.bytes_past_eof,
                        );
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

    let report = if repair {
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
            let needed_stage = aeordb::engine::kv_pages::stage_for_count(
                initial.valid_entries as usize, hash_length,
            );
            let (needed_size, _) = aeordb::engine::kv_stages::stage_params(needed_stage, psize);
            let current_size = engine.writer_read_lock()
                .map(|w| w.file_header().kv_block_length)
                .unwrap_or(0);

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
    println!("  Unlisted files:     {:>8}", report.unlisted_files.len());
    for uf in &report.unlisted_files {
        println!("    - {}", uf);
    }
    println!();

    println!("KV Index:");
    println!("  KV entries:         {:>8}", report.kv_entries);
    println!("  Stale entries:      {:>8}", report.stale_kv_entries);
    println!("  Missing entries:    {:>8}", report.missing_kv_entries);
    println!();

    println!("Snapshot Integrity:");
    println!("  Snapshots checked:  {:>8}", report.snapshots_checked);
    println!("  Broken snapshots:   {:>8}", report.broken_snapshots.len());
    for bs in &report.broken_snapshots {
        println!("    - {}", bs);
    }
    println!();

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

