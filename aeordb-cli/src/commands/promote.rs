use std::process;

pub fn run(database: &str, hash: &str) {
    println!("AeorDB Promote");
    println!("Database: {}", database);
    println!("Hash: {}", hash);

    let hash_bytes = match hex::decode(hash) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("Error: invalid hash '{}': {}", hash, e);
            process::exit(1);
        }
    };

    let engine = match aeordb::engine::StorageEngine::open(database) {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("Error opening database: {}", e);
            process::exit(1);
        }
    };

    // Verify the hash exists (it should be a directory entry)
    match engine.has_entry(&hash_bytes) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("Error: version hash {} not found in database.", hash);
            process::exit(1);
        }
        Err(e) => {
            eprintln!("Error checking hash: {}", e);
            process::exit(1);
        }
    }

    match engine.update_head(&hash_bytes) {
        Ok(()) => println!("HEAD promoted to {}", hash),
        Err(e) => {
            eprintln!("Promote failed: {}", e);
            process::exit(1);
        }
    }
}
