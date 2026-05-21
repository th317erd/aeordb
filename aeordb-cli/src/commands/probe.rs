// Diagnostic helper: list /.aeordb-system/ contents in a database file.
pub fn run(path: &str, probe_path: Option<&str>) {
    let engine = match aeordb::engine::StorageEngine::open(path) {
        Ok(e) => e,
        Err(e) => { eprintln!("Open failed: {}", e); std::process::exit(1); }
    };
    let ops = aeordb::engine::DirectoryOps::new(&engine);

    if probe_path == Some("--list-files") {
        // Enumerate every FileRecord — try a couple of entry_version
        // settings so we cover whichever the DB was written with.
        use aeordb::engine::file_record::FileRecord;
        let hash_length = engine.hash_algo().hash_length();
        let entries = engine.entries_by_type(aeordb::engine::KV_TYPE_FILE_RECORD).unwrap_or_default();
        println!("FileRecord entries: {}", entries.len());
        let mut seen_paths = std::collections::HashSet::new();
        for (hash, value) in &entries {
            // Read entry header to get correct entry_version.
            let entry = match engine.get_entry_including_deleted(hash) {
                Ok(Some(e)) => e,
                _ => continue,
            };
            let version = entry.0.entry_version;
            match FileRecord::deserialize(value, hash_length, version) {
                Ok(record) => {
                    if seen_paths.insert(record.path.clone()) {
                        println!("  {} ({}B)", record.path, record.total_size);
                    }
                }
                Err(e) => {
                    println!("  [deser err: {} hash={} version={}]", e, hex::encode(&hash[..8.min(hash.len())]), version);
                }
            }
        }
        return;
    }

    if probe_path == Some("--wal-dump") {
        // Dump every DirectoryIndex entry — hash, length, first bytes
        // so we can see what /.aeordb-system content looks like across
        // its multiple versions.
        let algo = engine.hash_algo();
        let aeordb_system_dir_key = aeordb::engine::directory_path_hash("/.aeordb-system", &algo).unwrap();
        println!("/.aeordb-system dir_key = {}", hex::encode(&aeordb_system_dir_key));

        let dir_entries = engine.entries_by_type(aeordb::engine::KV_TYPE_DIRECTORY).unwrap_or_default();
        println!("DirectoryIndex entries (live KV): {}", dir_entries.len());
        for (hash, value) in &dir_entries {
            let hl = if value.len() == algo.hash_length() {
                format!(" hard-link → {}", hex::encode(&value[..8.min(value.len())]))
            } else {
                String::new()
            };
            let is_aeordb_sys = hash == &aeordb_system_dir_key;
            println!("  {} len={}{}{}",
                hex::encode(&hash[..8.min(hash.len())]),
                value.len(),
                hl,
                if is_aeordb_sys { "  ← /.aeordb-system dir_key" } else { "" },
            );
        }
        return;
    }

    if let Some(p) = probe_path {
        let algo = engine.hash_algo();
        let normalized = aeordb::engine::path_utils::normalize_path(p);
        println!("=== Probe path: {} ===", p);
        println!("Normalized:    {}", normalized);
        let dir_key = aeordb::engine::directory_path_hash(&normalized, &algo).unwrap();
        let file_key = aeordb::engine::file_path_hash(&normalized, &algo).unwrap();
        let dir_present = engine.has_entry(&dir_key).unwrap_or(false);
        let file_present = engine.has_entry(&file_key).unwrap_or(false);
        println!("dir:{} → hash {} → {}",  normalized, hex::encode(&dir_key), if dir_present { "PRESENT" } else { "MISSING" });
        println!("file:{} → hash {} → {}", normalized, hex::encode(&file_key), if file_present { "PRESENT" } else { "MISSING" });

        if dir_present {
            if let Ok(Some((header, _key, value))) = engine.get_entry_including_deleted(&dir_key) {
                println!("Dir entry (incl deleted): flags={:#x}, type={:?}, value len={}", header.flags, header.entry_type, value.len());
                if value.len() == algo.hash_length() {
                    println!("  hard-link → {}", hex::encode(&value));
                    if let Ok(Some((_h, _k, real))) = engine.get_entry_including_deleted(&value) {
                        println!("  target len: {}", real.len());
                    } else {
                        println!("  target MISSING (dangling hard-link)");
                    }
                }
            }
            // Now try the non-deleted variant — this is what list_directory uses.
            match engine.get_entry(&dir_key) {
                Ok(Some((header, _k, value))) => {
                    println!("Dir entry (LIVE): flags={:#x}, type={:?}, value len={}", header.flags, header.entry_type, value.len());
                    if value.len() == algo.hash_length() {
                        println!("  hard-link → {}", hex::encode(&value));
                        match engine.get_entry(&value) {
                            Ok(Some((_h, _k, real))) => println!("  target LIVE len: {}", real.len()),
                            Ok(None) => println!("  target MISSING from LIVE (would 404 in list_directory)"),
                            Err(e) => println!("  target lookup error: {}", e),
                        }
                    }
                }
                Ok(None) => println!("Dir entry NOT LIVE (marked deleted?) — list_directory will 404"),
                Err(e) => println!("Dir entry lookup error: {}", e),
            }
        }

        // Also try probing the parent's listing to see how the child appears
        if let Some(parent) = aeordb::engine::path_utils::parent_path(&normalized) {
            println!("--- Parent listing of {} ---", parent);
            match ops.list_directory(&parent) {
                Ok(entries) => {
                    let name = aeordb::engine::path_utils::file_name(&normalized).unwrap_or("");
                    for e in &entries {
                        if e.name == name {
                            println!("  ChildEntry: name={} type={} hash={}", e.name, e.entry_type, hex::encode(&e.hash));
                            // Check if the ChildEntry.hash itself is live and content-addressable
                            let child_hash = e.hash.clone();
                            match engine.get_entry(&child_hash) {
                                Ok(Some((h, _k, v))) => println!("    ChildEntry.hash LIVE: flags={:#x}, len={}", h.flags, v.len()),
                                Ok(None) => println!("    ChildEntry.hash NOT LIVE"),
                                Err(e) => println!("    ChildEntry.hash error: {}", e),
                            }
                            match engine.get_entry_including_deleted(&child_hash) {
                                Ok(Some((h, _k, v))) => println!("    ChildEntry.hash incl-deleted: flags={:#x}, len={}", h.flags, v.len()),
                                Ok(None) => println!("    ChildEntry.hash NOT FOUND in incl-deleted either"),
                                Err(e) => println!("    ChildEntry.hash incl-del error: {}", e),
                            }
                        }
                    }
                }
                Err(e) => println!("  ERROR: {}", e),
            }
        }
        // List snapshots via raw KV scan (the /.aeordb-system/snapshots
        // dir_key may itself be desynced, so prefer entries_by_type).
        println!("--- Raw snapshot KV records ---");
        match engine.entries_by_type(aeordb::engine::KV_TYPE_SNAPSHOT) {
            Ok(entries) => {
                println!("  count: {}", entries.len());
                for (key, value) in entries.iter().take(5) {
                    println!("    snapshot hash={} ({}b)", hex::encode(&key[..8.min(key.len())]), value.len());
                }
            }
            Err(e) => println!("  ERROR: {}", e),
        }
        return;
    }

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
