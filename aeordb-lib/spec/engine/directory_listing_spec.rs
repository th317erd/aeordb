use aeordb::engine::directory_listing::list_directory_recursive;
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::entry_type::EntryType;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::RequestContext;

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
    let ctx = RequestContext::system();
    let path = dir.path().join("test.aeor");
    let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
    let ops = DirectoryOps::new(&engine);
    ops.ensure_root_directory(&ctx).unwrap();
    engine
}

fn store_file(engine: &StorageEngine, path: &str, content: &[u8]) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_file(&ctx, path, content, None).unwrap();
}

#[test]
fn test_list_immediate_children() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_file(&engine, "/a.txt", b"aaa");
    store_file(&engine, "/b.txt", b"bbb");
    store_file(&engine, "/sub/c.txt", b"ccc");

    let entries = list_directory_recursive(&engine, "/", 0, None).unwrap();
    assert_eq!(entries.len(), 3); // a.txt, b.txt, sub (directory)

    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"a.txt"));
    assert!(names.contains(&"b.txt"));
    assert!(names.contains(&"sub"));
}

#[test]
fn test_list_depth_1() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_file(&engine, "/a.txt", b"aaa");
    store_file(&engine, "/sub/c.txt", b"ccc");

    let entries = list_directory_recursive(&engine, "/", 1, None).unwrap();
    // depth=1: returns files only (recursive mode). a.txt at root + c.txt inside /sub
    assert_eq!(entries.len(), 2);

    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"a.txt"));
    assert!(names.contains(&"c.txt"));
}

#[test]
fn test_list_unlimited_depth() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_file(&engine, "/a.txt", b"aaa");
    store_file(&engine, "/d1/b.txt", b"bbb");
    store_file(&engine, "/d1/d2/c.txt", b"ccc");
    store_file(&engine, "/d1/d2/d3/d.txt", b"ddd");

    let entries = list_directory_recursive(&engine, "/", -1, None).unwrap();
    assert_eq!(entries.len(), 4);

    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"a.txt"));
    assert!(names.contains(&"b.txt"));
    assert!(names.contains(&"c.txt"));
    assert!(names.contains(&"d.txt"));
}

#[test]
fn test_list_glob_filter() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_file(&engine, "/a.txt", b"aaa");
    store_file(&engine, "/b.psd", b"bbb");
    store_file(&engine, "/c.txt", b"ccc");

    let entries = list_directory_recursive(&engine, "/", 0, Some("*.txt")).unwrap();
    assert_eq!(entries.len(), 2);

    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"a.txt"));
    assert!(names.contains(&"c.txt"));
}

#[test]
fn test_list_glob_with_depth() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_file(&engine, "/a.txt", b"aaa");
    store_file(&engine, "/sub/b.txt", b"bbb");
    store_file(&engine, "/sub/c.psd", b"ccc");

    let entries = list_directory_recursive(&engine, "/", -1, Some("*.txt")).unwrap();
    assert_eq!(entries.len(), 2);

    let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    assert!(paths.contains(&"/a.txt"));
    assert!(paths.contains(&"/sub/b.txt"));
}

#[test]
fn test_list_empty_directory() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    let entries = list_directory_recursive(&engine, "/", 0, None).unwrap();
    assert!(entries.is_empty());
}

#[test]
fn test_list_nonexistent_directory() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    let result = list_directory_recursive(&engine, "/nonexistent", 0, None);
    assert!(result.is_err());
}

#[test]
fn test_list_no_glob_matches() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_file(&engine, "/a.txt", b"aaa");
    store_file(&engine, "/b.txt", b"bbb");

    let entries = list_directory_recursive(&engine, "/", 0, Some("*.xyz")).unwrap();
    assert!(entries.is_empty());
}

#[test]
fn test_list_depth_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_file(&engine, "/a.txt", b"aaa");
    store_file(&engine, "/d1/b.txt", b"bbb");
    store_file(&engine, "/d1/d2/c.txt", b"ccc");
    store_file(&engine, "/d1/d2/d3/d.txt", b"ddd");

    // depth=2: recursive mode (files only). Recurses 2 levels deep from root.
    // root(depth=2) -> d1(depth=1) -> d2(depth=0, no further recursion)
    // Files found: a.txt (root), b.txt (d1), c.txt (d2). d.txt is in d3 which is beyond depth.
    let entries = list_directory_recursive(&engine, "/", 2, None).unwrap();

    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"a.txt"), "should contain a.txt, got: {:?}", names);
    assert!(names.contains(&"b.txt"), "should contain b.txt, got: {:?}", names);
    assert!(names.contains(&"c.txt"), "should contain c.txt, got: {:?}", names);
    assert!(!names.contains(&"d.txt"), "should NOT contain d.txt, got: {:?}", names);

    // In recursive mode (depth > 0), only files should be returned
    for entry in &entries {
        assert_eq!(entry.entry_type, EntryType::FileRecord.to_u8());
    }
}

#[test]
fn test_list_files_only_recursive() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_file(&engine, "/a.txt", b"aaa");
    store_file(&engine, "/sub/b.txt", b"bbb");

    let entries = list_directory_recursive(&engine, "/", -1, None).unwrap();
    assert_eq!(entries.len(), 2);

    for entry in &entries {
        assert_eq!(entry.entry_type, EntryType::FileRecord.to_u8());
    }
}

#[test]
fn test_list_includes_content_hash() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_file(&engine, "/a.txt", b"hello world");

    let entries = list_directory_recursive(&engine, "/", 0, None).unwrap();
    assert_eq!(entries.len(), 1);
    assert!(!entries[0].hash.is_empty());
}
