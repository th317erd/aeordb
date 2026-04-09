use std::process;

use aeordb::engine::{RequestContext, StorageEngine};
use aeordb::engine::gc::run_gc;

pub fn run(database: &str, dry_run: bool) {
    if dry_run {
        println!("AeorDB Garbage Collection [DRY RUN]");
    } else {
        println!("AeorDB Garbage Collection");
    }
    println!("Database: {}", database);
    println!();

    let engine = match StorageEngine::open(database) {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("Error opening database: {}", e);
            process::exit(1);
        }
    };

    let ctx = RequestContext::system();

    match run_gc(&engine, &ctx, dry_run) {
        Ok(result) => {
            if result.dry_run {
                println!("[DRY RUN] Would collect {} garbage entries ({})",
                    result.garbage_entries,
                    format_bytes(result.reclaimed_bytes),
                );
            } else {
                println!("Versions scanned: {}", result.versions_scanned);
                println!("Live entries:     {}", result.live_entries);
                println!("Garbage entries:  {}", result.garbage_entries);
                println!("Reclaimed:        {}", format_bytes(result.reclaimed_bytes));
                println!("Duration:         {:.1}s", result.duration_ms as f64 / 1000.0);
            }
        }
        Err(e) => {
            eprintln!("GC failed: {}", e);
            process::exit(1);
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
