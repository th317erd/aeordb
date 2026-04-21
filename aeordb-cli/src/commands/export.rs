use std::process;

pub fn run(database: &str, output: &str, snapshot: Option<&str>, hash: Option<&str>) {
    println!("AeorDB Export");
    println!("Source: {}", database);
    println!("Output: {}", output);

    // Check for root key auth — future gating for .system/ data in exports
    let _include_system = if let Ok(_root_key) = std::env::var("AEORDB_ROOT_KEY") {
        // TODO: Validate the key against the database once encrypted exports land.
        // For now, CLI export with filesystem access includes everything since
        // the user already has the .aeordb file.
        eprintln!("Note: AEORDB_ROOT_KEY detected. System data inclusion will be gated by key validation in a future release.");
        true
    } else {
        eprintln!("Note: Set AEORDB_ROOT_KEY to include system data in exports.");
        false
    };

    // Check output doesn't already exist
    if std::path::Path::new(output).exists() {
        eprintln!(
            "Error: output file '{}' already exists. Remove it first or choose a different name.",
            output
        );
        process::exit(1);
    }

    // Open source database
    let source = match aeordb::engine::StorageEngine::open(database) {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("Error opening source database: {}", e);
            process::exit(1);
        }
    };

    // Determine version to export
    let result = if let Some(h) = hash {
        let hash_bytes = match hex::decode(h) {
            Ok(bytes) => bytes,
            Err(e) => {
                eprintln!("Error: invalid hash '{}': {}", h, e);
                process::exit(1);
            }
        };
        println!("Exporting hash: {}", h);
        aeordb::engine::backup::export_version(&source, &hash_bytes, output)
    } else {
        match snapshot {
            Some(name) => {
                println!("Exporting snapshot: {}", name);
            }
            None => {
                println!("Exporting HEAD");
            }
        }
        aeordb::engine::backup::export_snapshot(&source, snapshot, output)
    };

    match result {
        Ok(result) => {
            println!("\n{}", result);
        }
        Err(e) => {
            eprintln!("Export failed: {}", e);
            // Clean up partial output
            let _ = std::fs::remove_file(output);
            process::exit(1);
        }
    }
}
