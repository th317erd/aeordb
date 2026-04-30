use std::process;

use aeordb::engine::StorageEngine;
use aeordb::engine::verify;
use aeordb::logging::{LogConfig, LogFormat, initialize_logging};

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
        Err(e) => {
            eprintln!("Error opening database: {}", e);
            process::exit(1);
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

                let report = verify::verify_and_repair(&engine2, &work_path);
                report
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

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} bytes", bytes)
    }
}
