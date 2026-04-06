use std::process;

pub fn run(database: &str, output: &str, from: &str, to: Option<&str>) {
    println!("AeorDB Diff");
    println!("Source: {}", database);
    println!("Output: {}", output);
    println!("From: {}", from);
    println!("To: {}", to.unwrap_or("HEAD"));

    if std::path::Path::new(output).exists() {
        eprintln!("Error: output file '{}' already exists.", output);
        process::exit(1);
    }

    let source = match aeordb::engine::StorageEngine::open(database) {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("Error opening database: {}", e);
            process::exit(1);
        }
    };

    // Try snapshot names first, fall back to raw hashes
    let result = aeordb::engine::backup::create_patch_from_snapshots(&source, from, to, output)
        .or_else(|_| {
            // Try as raw hashes
            let from_bytes = hex::decode(from).map_err(|e| {
                aeordb::engine::EngineError::NotFound(format!("Invalid 'from' hash: {}", e))
            })?;
            let to_bytes = match to {
                Some(h) => hex::decode(h).map_err(|e| {
                    aeordb::engine::EngineError::NotFound(format!("Invalid 'to' hash: {}", e))
                })?,
                None => source.head_hash()?,
            };
            aeordb::engine::backup::create_patch(&source, &from_bytes, &to_bytes, output)
        });

    match result {
        Ok(result) => println!("\n{}", result),
        Err(e) => {
            eprintln!("Diff failed: {}", e);
            let _ = std::fs::remove_file(output);
            process::exit(1);
        }
    }
}
