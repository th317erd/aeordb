use std::process;

pub fn run(database: &str, file: &str, force: bool, promote: bool, root_key: Option<&str>) {
    println!("AeorDB Import");
    println!("Target: {}", database);
    println!("File: {}", file);

    if !std::path::Path::new(file).exists() {
        eprintln!("Error: backup file '{}' not found.", file);
        process::exit(1);
    }

    let target = match aeordb::engine::StorageEngine::open(database) {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("Error opening target database: {}", e);
            process::exit(1);
        }
    };

    // Detect whether the backup file contains system data
    let backup_for_inspect = match aeordb::engine::StorageEngine::open_for_import(file) {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("Error inspecting backup file: {}", e);
            process::exit(1);
        }
    };
    let backup_has_system = match aeordb::engine::backup::backup_contains_system_data(&backup_for_inspect) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Error inspecting backup file: {}", e);
            process::exit(1);
        }
    };
    drop(backup_for_inspect);

    // Resolve root key from arg or env var
    let provided_key = root_key
        .map(|s| s.to_string())
        .or_else(|| std::env::var("AEORDB_ROOT_KEY").ok());

    // Determine include_system: requires both a system-bearing backup AND a valid root key
    let include_system = if backup_has_system {
        match provided_key {
            Some(key) => match aeordb::auth::validate_root_key(&target, &key) {
                Ok(true) => {
                    println!("Root key validated — system data will be imported.");
                    true
                }
                Ok(false) => {
                    eprintln!("Error: backup contains system data, but provided key is not a valid root key for the target database.");
                    eprintln!("       Either provide the correct root key for the target, or import into a fresh database.");
                    process::exit(1);
                }
                Err(e) => {
                    eprintln!("Error validating root key: {}", e);
                    process::exit(1);
                }
            },
            None => {
                eprintln!("Note: backup contains system data (users, groups, keys), but no root key provided.");
                eprintln!("      System data will be SKIPPED. Provide --root-key <key> or set AEORDB_ROOT_KEY to import system data.");
                false
            }
        }
    } else {
        if provided_key.is_some() {
            println!("Note: root key provided but backup contains no system data — proceeding with user-data-only import.");
        }
        false
    };

    let ctx = aeordb::engine::RequestContext::system();
    match aeordb::engine::backup::import_backup(&ctx, &target, file, force, promote, include_system) {
        Ok(result) => println!("\n{}", result),
        Err(e) => {
            eprintln!("Import failed: {}", e);
            process::exit(1);
        }
    }
}
