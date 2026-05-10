use std::process;

pub fn run(
    database: &str,
    output: &str,
    snapshot: Option<&str>,
    hash: Option<&str>,
    root_key: Option<&str>,
) {
    println!("AeorDB Export");
    println!("Source: {}", database);
    println!("Output: {}", output);

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

    // Resolve root key from arg or env var, then validate it.
    let provided_key = root_key
        .map(|s| s.to_string())
        .or_else(|| std::env::var("AEORDB_ROOT_KEY").ok());

    let include_system = match provided_key {
        Some(key) => {
            match aeordb::auth::validate_root_key(&source, &key) {
                Ok(true) => {
                    println!("Root key validated — full backup mode (includes system data and all snapshots).");
                    true
                }
                Ok(false) => {
                    eprintln!("Error: provided key is not a valid root key for this database.");
                    process::exit(1);
                }
                Err(e) => {
                    eprintln!("Error validating root key: {}", e);
                    process::exit(1);
                }
            }
        }
        None => {
            eprintln!("Note: no root key provided — exporting user data only (no system entries, no snapshots).");
            eprintln!("      Provide --root-key <key> or set AEORDB_ROOT_KEY for a full backup.");
            false
        }
    };

    // Determine export mode
    let result = if let Some(h) = hash {
        // Specific version hash — single-version export
        let hash_bytes = match hex::decode(h) {
            Ok(bytes) => bytes,
            Err(e) => {
                eprintln!("Error: invalid hash '{}': {}", h, e);
                process::exit(1);
            }
        };
        println!("Exporting hash: {}", h);
        aeordb::engine::backup::export_version(&source, &hash_bytes, output, include_system)
    } else if let Some(name) = snapshot {
        // Specific snapshot — single-version export
        println!("Exporting snapshot: {}", name);
        aeordb::engine::backup::export_snapshot(&source, Some(name), output, include_system)
    } else if include_system {
        // Full backup mode — HEAD + all snapshots + system data
        println!("Exporting full database (HEAD + all snapshots + system data)");
        aeordb::engine::backup::export_full(&source, output, true)
    } else {
        // Default: HEAD only, user data only
        println!("Exporting HEAD (user data only)");
        aeordb::engine::backup::export_snapshot(&source, None, output, false)
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
