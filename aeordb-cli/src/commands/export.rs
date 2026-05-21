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

    // Write to a `.part` sibling and rename at the end. A killed export
    // would otherwise leave a partial file at the final path with a valid
    // magic + backup_info header — indistinguishable from a successful
    // export, and `import_backup` would happily iterate the partial entries.
    let part_path = format!("{}.part", output);
    if std::path::Path::new(&part_path).exists() {
        // Clean up a stale `.part` from a previous crashed export.
        if let Err(e) = std::fs::remove_file(&part_path) {
            eprintln!(
                "Error: failed to remove stale '{}': {}. Remove it manually and retry.",
                part_path, e
            );
            process::exit(1);
        }
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

    // Determine export mode — write into the .part path.
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
        aeordb::engine::backup::export_version(&source, &hash_bytes, &part_path, include_system)
    } else if let Some(name) = snapshot {
        // Specific snapshot — single-version export
        println!("Exporting snapshot: {}", name);
        aeordb::engine::backup::export_snapshot(&source, Some(name), &part_path, include_system)
    } else if include_system {
        // Full backup mode — HEAD + all snapshots + system data
        println!("Exporting full database (HEAD + all snapshots + system data)");
        aeordb::engine::backup::export_full(&source, &part_path, true)
    } else {
        // Default: HEAD only, user data only
        println!("Exporting HEAD (user data only)");
        aeordb::engine::backup::export_snapshot(&source, None, &part_path, false)
    };

    match result {
        Ok(result) => {
            // Atomic rename: only after the export wrote successfully do we
            // expose the file at its final path. A killed export leaves
            // `.part` on disk — easy for an operator to identify and clean
            // up, and `import_backup` won't accidentally read it.
            if let Err(e) = std::fs::rename(&part_path, output) {
                eprintln!(
                    "Export wrote successfully but final rename failed: {}\n\
                     Partial output is at: {}",
                    e, part_path
                );
                process::exit(1);
            }
            println!("\n{}", result);
        }
        Err(e) => {
            eprintln!("Export failed: {}", e);
            // Clean up the partial .part file — it has no use.
            let _ = std::fs::remove_file(&part_path);
            process::exit(1);
        }
    }
}
