use std::process;

use aeordb::engine::StorageEngine;
use aeordb::engine::verify;

pub fn run(database: &str, repair: bool) {
    println!("AeorDB Integrity Check");
    println!("=======================");
    println!();

    let engine = match StorageEngine::open(database) {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("Error opening database: {}", e);
            process::exit(1);
        }
    };

    let report = if repair {
        println!("Running with --repair...");
        println!();
        verify::verify_and_repair(&engine, database)
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
            println!("  Run with --repair to auto-fix recoverable issues.");
        }
        process::exit(2);
    } else {
        println!("Status: OK");
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
