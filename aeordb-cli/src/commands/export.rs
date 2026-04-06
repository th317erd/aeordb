use std::process;

pub fn run(database: &str, output: &str, snapshot: Option<&str>, hash: Option<&str>) {
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
