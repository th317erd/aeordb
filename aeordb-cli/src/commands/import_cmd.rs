use std::process;

pub fn run(database: &str, file: &str, force: bool, promote: bool) {
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

    let ctx = aeordb::engine::RequestContext::system();
    match aeordb::engine::backup::import_backup(&ctx, &target, file, force, promote) {
        Ok(result) => println!("\n{}", result),
        Err(e) => {
            eprintln!("Import failed: {}", e);
            process::exit(1);
        }
    }
}
