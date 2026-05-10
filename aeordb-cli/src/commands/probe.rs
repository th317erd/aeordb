// Diagnostic helper: list /.aeordb-system/ contents in a database file.
pub fn run(path: &str) {
    let engine = match aeordb::engine::StorageEngine::open(path) {
        Ok(e) => e,
        Err(e) => { eprintln!("Open failed: {}", e); std::process::exit(1); }
    };
    let ops = aeordb::engine::DirectoryOps::new(&engine);

    println!("--- /.aeordb-system/ ---");
    match ops.list_directory("/.aeordb-system") {
        Ok(items) => {
            if items.is_empty() { println!("  (empty)"); }
            for e in items { println!("  {} (type={})", e.name, e.entry_type); }
        }
        Err(e) => println!("  ERROR: {}", e),
    }

    println!("--- /.aeordb-system/api-keys/ ---");
    match ops.list_directory("/.aeordb-system/api-keys") {
        Ok(items) => {
            if items.is_empty() { println!("  (empty)"); }
            for e in items { println!("  {} ({}b)", e.name, e.total_size); }
        }
        Err(e) => println!("  ERROR: {}", e),
    }

    println!("--- /.aeordb-system/users/ ---");
    match ops.list_directory("/.aeordb-system/users") {
        Ok(items) => {
            if items.is_empty() { println!("  (empty)"); }
            for e in items { println!("  {} ({}b)", e.name, e.total_size); }
        }
        Err(e) => println!("  ERROR: {}", e),
    }

    println!("--- /.aeordb-system/snapshots/ ---");
    match ops.list_directory("/.aeordb-system/snapshots") {
        Ok(items) => {
            println!("  count: {}", items.len());
        }
        Err(e) => println!("  ERROR: {}", e),
    }

    println!("--- /.aeordb-system/groups/ ---");
    match ops.list_directory("/.aeordb-system/groups") {
        Ok(items) => println!("  count: {}", items.len()),
        Err(e) => println!("  ERROR: {}", e),
    }

    // Count FLAG_SYSTEM entries by type
    use aeordb::engine::entry_header::FLAG_SYSTEM;
    let mut sys_files = 0u32;
    let mut sys_dirs = 0u32;
    let mut total_files = 0u32;
    let mut total_dirs = 0u32;
    let file_entries = engine.entries_by_type(aeordb::engine::KV_TYPE_FILE_RECORD).unwrap_or_default();
    for (hash, _value) in &file_entries {
        total_files += 1;
        if let Ok(Some((header, _key, _value))) = engine.get_entry_including_deleted(hash) {
            if header.flags & FLAG_SYSTEM != 0 { sys_files += 1; }
        }
    }
    let dir_entries = engine.entries_by_type(aeordb::engine::KV_TYPE_DIRECTORY).unwrap_or_default();
    for (hash, _value) in &dir_entries {
        total_dirs += 1;
        if let Ok(Some((header, _key, _value))) = engine.get_entry_including_deleted(hash) {
            if header.flags & FLAG_SYSTEM != 0 { sys_dirs += 1; }
        }
    }
    println!("--- FLAG_SYSTEM counts ---");
    println!("  FileRecords:     {} of {} have FLAG_SYSTEM", sys_files, total_files);
    println!("  DirectoryIndex:  {} of {} have FLAG_SYSTEM", sys_dirs, total_dirs);

    // Check if specific api keys exist by path hash
    println!("--- Direct lookups ---");
    let algo = engine.hash_algo();
    for uuid in &[
        "83120afe-eb67-435e-9021-7544a54e0c86",
        "edd1c91d-c5c7-490a-b490-3c46b135ea72",
        "10fae062-d2ed-4f2e-b742-4abc48088fd2",
        "cafc6f96-e263-4199-818a-b0090b206317",
    ] {
        let path = format!("/.aeordb-system/api-keys/{}", uuid);
        let path_key = aeordb::engine::file_path_hash(&path, &algo).unwrap();
        let exists = engine.has_entry(&path_key).unwrap_or(false);
        println!("  {}: {}", uuid, if exists { "PRESENT" } else { "missing" });
    }

    // Read the api-keys directory data raw
    let dir_path = "/.aeordb-system/api-keys";
    let dir_key = aeordb::engine::directory_path_hash(dir_path, &algo).unwrap();
    println!("--- Raw dir entry at {} ---", dir_path);
    if let Ok(Some((header, _key, value))) = engine.get_entry_including_deleted(&dir_key) {
        println!("  flags: {:#x}, value len: {}", header.flags, value.len());
        if value.len() == algo.hash_length() {
            println!("  hard link → {}", hex::encode(&value));
            // Follow the link
            if let Ok(Some((_h, _k, real_value))) = engine.get_entry_including_deleted(&value) {
                println!("  target len: {}", real_value.len());
            }
        }
    } else {
        println!("  not found");
    }
}
