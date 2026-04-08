use std::sync::Arc;

use aeordb::engine::btree::{
    BTreeNode, LeafNode, InternalNode,
    BTREE_MAX_LEAF_ENTRIES, BTREE_MAX_INTERNAL_KEYS,
    BTREE_LEAF_MARKER, BTREE_INTERNAL_MARKER,
    is_btree_format, btree_insert, btree_lookup, btree_list,
    btree_list_from_node, btree_delete, btree_from_entries,
    store_btree_node,
};
use aeordb::engine::directory_entry::ChildEntry;
use aeordb::engine::hash_algorithm::HashAlgorithm;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::server::create_temp_engine_for_tests;

fn make_entry(name: &str) -> ChildEntry {
    ChildEntry {
        entry_type: 0x02, // FileRecord type
        hash: vec![0u8; 32],
        total_size: 100,
        created_at: 1000,
        updated_at: 1000,
        name: name.to_string(),
        content_type: Some("application/json".to_string()),
    }
}

fn make_entry_with_hash(name: &str, hash_byte: u8) -> ChildEntry {
    ChildEntry {
        entry_type: 0x02,
        hash: vec![hash_byte; 32],
        total_size: 200,
        created_at: 2000,
        updated_at: 2000,
        name: name.to_string(),
        content_type: Some("text/plain".to_string()),
    }
}

const HASH_LENGTH: usize = 32;

// ─── Serialization tests ────────────────────────────────────────────────────

#[test]
fn test_leaf_serialize_deserialize_empty() {
    let leaf = BTreeNode::Leaf(LeafNode::new());
    let data = leaf.serialize(HASH_LENGTH);
    let deserialized = BTreeNode::deserialize(&data, HASH_LENGTH).unwrap();
    match deserialized {
        BTreeNode::Leaf(l) => assert!(l.entries.is_empty()),
        _ => panic!("Expected Leaf node"),
    }
}

#[test]
fn test_leaf_serialize_deserialize_with_entries() {
    let entries = vec![
        make_entry("alpha"),
        make_entry("bravo"),
        make_entry("charlie"),
        make_entry("delta"),
        make_entry("echo"),
    ];
    let leaf = BTreeNode::Leaf(LeafNode { entries: entries.clone() });
    let data = leaf.serialize(HASH_LENGTH);
    let deserialized = BTreeNode::deserialize(&data, HASH_LENGTH).unwrap();
    match deserialized {
        BTreeNode::Leaf(l) => {
            assert_eq!(l.entries.len(), 5);
            for (i, entry) in l.entries.iter().enumerate() {
                assert_eq!(entry.name, entries[i].name);
                assert_eq!(entry.entry_type, entries[i].entry_type);
                assert_eq!(entry.hash, entries[i].hash);
                assert_eq!(entry.total_size, entries[i].total_size);
                assert_eq!(entry.content_type, entries[i].content_type);
            }
        }
        _ => panic!("Expected Leaf node"),
    }
}

#[test]
fn test_internal_serialize_deserialize() {
    let keys = vec!["delta".to_string(), "mike".to_string(), "tango".to_string()];
    let children = vec![
        vec![1u8; 32],
        vec![2u8; 32],
        vec![3u8; 32],
        vec![4u8; 32],
    ];
    let internal = BTreeNode::Internal(InternalNode {
        keys: keys.clone(),
        children: children.clone(),
    });
    let data = internal.serialize(HASH_LENGTH);
    let deserialized = BTreeNode::deserialize(&data, HASH_LENGTH).unwrap();
    match deserialized {
        BTreeNode::Internal(n) => {
            assert_eq!(n.keys, keys);
            assert_eq!(n.children, children);
        }
        _ => panic!("Expected Internal node"),
    }
}

#[test]
fn test_content_hash_deterministic() {
    let algo = HashAlgorithm::Blake3_256;
    let leaf = BTreeNode::Leaf(LeafNode {
        entries: vec![make_entry("alpha"), make_entry("bravo")],
    });
    let hash1 = leaf.content_hash(HASH_LENGTH, &algo).unwrap();
    let hash2 = leaf.content_hash(HASH_LENGTH, &algo).unwrap();
    assert_eq!(hash1, hash2);
}

#[test]
fn test_content_hash_differs() {
    let algo = HashAlgorithm::Blake3_256;
    let leaf1 = BTreeNode::Leaf(LeafNode {
        entries: vec![make_entry("alpha")],
    });
    let leaf2 = BTreeNode::Leaf(LeafNode {
        entries: vec![make_entry("bravo")],
    });
    let hash1 = leaf1.content_hash(HASH_LENGTH, &algo).unwrap();
    let hash2 = leaf2.content_hash(HASH_LENGTH, &algo).unwrap();
    assert_ne!(hash1, hash2);
}

#[test]
fn test_is_btree_format_leaf() {
    let data = vec![BTREE_LEAF_MARKER, 0x00, 0x00];
    assert!(is_btree_format(&data));
}

#[test]
fn test_is_btree_format_internal() {
    let data = vec![BTREE_INTERNAL_MARKER, 0x00, 0x00];
    assert!(is_btree_format(&data));
}

#[test]
fn test_is_btree_format_flat() {
    // 0x02 is FileRecord entry_type — flat directory format
    let data = vec![0x02, 0x00, 0x00];
    assert!(!is_btree_format(&data));
    // Other high values
    let data2 = vec![0x03, 0xFF];
    assert!(!is_btree_format(&data2));
}

#[test]
fn test_is_btree_format_empty() {
    assert!(!is_btree_format(&[]));
}

// ─── Leaf operation tests ───────────────────────────────────────────────────

#[test]
fn test_leaf_find() {
    let mut leaf = LeafNode::new();
    leaf.upsert(make_entry("alpha"));
    leaf.upsert(make_entry("bravo"));
    leaf.upsert(make_entry("charlie"));

    let found = leaf.find("bravo");
    assert!(found.is_some());
    assert_eq!(found.unwrap().name, "bravo");
}

#[test]
fn test_leaf_find_missing() {
    let mut leaf = LeafNode::new();
    leaf.upsert(make_entry("alpha"));
    assert!(leaf.find("zulu").is_none());
}

#[test]
fn test_leaf_upsert_insert() {
    let mut leaf = LeafNode::new();
    let inserted = leaf.upsert(make_entry("alpha"));
    assert!(inserted); // was new
    assert_eq!(leaf.entries.len(), 1);
}

#[test]
fn test_leaf_upsert_update() {
    let mut leaf = LeafNode::new();
    leaf.upsert(make_entry("alpha"));
    let updated_entry = make_entry_with_hash("alpha", 0xFF);
    let inserted = leaf.upsert(updated_entry);
    assert!(!inserted); // was update
    assert_eq!(leaf.entries.len(), 1);
    assert_eq!(leaf.entries[0].hash, vec![0xFF; 32]);
}

#[test]
fn test_leaf_upsert_maintains_sort() {
    let mut leaf = LeafNode::new();
    leaf.upsert(make_entry("charlie"));
    leaf.upsert(make_entry("alpha"));
    leaf.upsert(make_entry("bravo"));
    leaf.upsert(make_entry("delta"));

    let names: Vec<&str> = leaf.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "bravo", "charlie", "delta"]);
}

#[test]
fn test_leaf_remove() {
    let mut leaf = LeafNode::new();
    leaf.upsert(make_entry("alpha"));
    leaf.upsert(make_entry("bravo"));
    let removed = leaf.remove("alpha");
    assert!(removed);
    assert_eq!(leaf.entries.len(), 1);
    assert_eq!(leaf.entries[0].name, "bravo");
}

#[test]
fn test_leaf_remove_missing() {
    let mut leaf = LeafNode::new();
    leaf.upsert(make_entry("alpha"));
    let removed = leaf.remove("zulu");
    assert!(!removed);
    assert_eq!(leaf.entries.len(), 1);
}

#[test]
fn test_leaf_is_full() {
    let mut leaf = LeafNode::new();
    for i in 0..BTREE_MAX_LEAF_ENTRIES {
        leaf.upsert(make_entry(&format!("entry_{:04}", i)));
    }
    assert!(leaf.is_full());
}

#[test]
fn test_leaf_split() {
    let mut leaf = LeafNode::new();
    for i in 0..BTREE_MAX_LEAF_ENTRIES {
        leaf.upsert(make_entry(&format!("entry_{:04}", i)));
    }
    let (left, split_key, right) = leaf.split();

    // Both halves should have entries
    assert!(!left.entries.is_empty());
    assert!(!right.entries.is_empty());
    // Total should equal original count
    assert_eq!(left.entries.len() + right.entries.len(), BTREE_MAX_LEAF_ENTRIES);
    // Split key should be the first entry in the right half
    assert_eq!(split_key, right.entries[0].name);
}

#[test]
fn test_leaf_split_both_sorted() {
    let mut leaf = LeafNode::new();
    for i in 0..BTREE_MAX_LEAF_ENTRIES {
        leaf.upsert(make_entry(&format!("entry_{:04}", i)));
    }
    let (left, _split_key, right) = leaf.split();

    // Both halves should be sorted
    for w in left.entries.windows(2) {
        assert!(w[0].name < w[1].name, "Left half not sorted: {} >= {}", w[0].name, w[1].name);
    }
    for w in right.entries.windows(2) {
        assert!(w[0].name < w[1].name, "Right half not sorted: {} >= {}", w[0].name, w[1].name);
    }
    // All left entries should be < split key (< all right entries)
    let last_left = &left.entries.last().unwrap().name;
    let first_right = &right.entries[0].name;
    assert!(last_left < first_right);
}

// ─── Internal operation tests ───────────────────────────────────────────────

#[test]
fn test_internal_find_child_index() {
    let internal = InternalNode {
        keys: vec!["delta".to_string(), "mike".to_string(), "tango".to_string()],
        children: vec![vec![1; 32], vec![2; 32], vec![3; 32], vec![4; 32]],
    };

    // Before first key
    assert_eq!(internal.find_child_index("alpha"), 0);
    // Exact match on first key -> goes right
    assert_eq!(internal.find_child_index("delta"), 1);
    // Between first and second key
    assert_eq!(internal.find_child_index("golf"), 1);
    // Exact match on second key -> goes right
    assert_eq!(internal.find_child_index("mike"), 2);
    // Between second and third key
    assert_eq!(internal.find_child_index("papa"), 2);
    // Exact match on third key -> goes right
    assert_eq!(internal.find_child_index("tango"), 3);
    // After last key
    assert_eq!(internal.find_child_index("zulu"), 3);
}

#[test]
fn test_internal_insert_key() {
    let mut internal = InternalNode {
        keys: vec!["delta".to_string(), "tango".to_string()],
        children: vec![vec![1; 32], vec![2; 32], vec![3; 32]],
    };
    internal.insert_key("mike".to_string(), vec![4; 32]);

    assert_eq!(internal.keys, vec!["delta", "mike", "tango"]);
    assert_eq!(internal.children.len(), 4);
    // The new child should be at index 2 (after "mike")
    assert_eq!(internal.children[2], vec![4; 32]);
}

#[test]
fn test_internal_split() {
    let keys: Vec<String> = (0..BTREE_MAX_INTERNAL_KEYS)
        .map(|i| format!("key_{:04}", i))
        .collect();
    let children: Vec<Vec<u8>> = (0..=BTREE_MAX_INTERNAL_KEYS)
        .map(|i| vec![i as u8; 32])
        .collect();

    let mut internal = InternalNode {
        keys: keys.clone(),
        children: children.clone(),
    };
    let (left, split_key, right) = internal.split();

    // Split key should not be in either half
    assert!(!left.keys.contains(&split_key));
    assert!(!right.keys.contains(&split_key));
    // Total keys = left.keys + 1 (split_key) + right.keys
    assert_eq!(left.keys.len() + 1 + right.keys.len(), BTREE_MAX_INTERNAL_KEYS);
    // Children count = left.children + right.children
    assert_eq!(left.children.len() + right.children.len(), BTREE_MAX_INTERNAL_KEYS + 1);
    // left.children.len() == left.keys.len() + 1
    assert_eq!(left.children.len(), left.keys.len() + 1);
    // right.children.len() == right.keys.len() + 1
    assert_eq!(right.children.len(), right.keys.len() + 1);
}

#[test]
fn test_internal_is_full() {
    let keys: Vec<String> = (0..BTREE_MAX_INTERNAL_KEYS)
        .map(|i| format!("key_{:04}", i))
        .collect();
    let children: Vec<Vec<u8>> = (0..=BTREE_MAX_INTERNAL_KEYS)
        .map(|i| vec![i as u8; 32])
        .collect();
    let internal = InternalNode { keys, children };
    assert!(internal.is_full());
}

// ─── Deserialization error tests ────────────────────────────────────────────

#[test]
fn test_deserialize_empty_data() {
    let result = BTreeNode::deserialize(&[], HASH_LENGTH);
    assert!(result.is_err());
}

#[test]
fn test_deserialize_unknown_marker() {
    let data = vec![0xFF, 0x00, 0x00];
    let result = BTreeNode::deserialize(&data, HASH_LENGTH);
    assert!(result.is_err());
}

#[test]
fn test_deserialize_truncated_leaf() {
    // Only marker byte, missing count
    let data = vec![BTREE_LEAF_MARKER];
    let result = BTreeNode::deserialize(&data, HASH_LENGTH);
    assert!(result.is_err());
}

#[test]
fn test_deserialize_truncated_internal() {
    // Only marker byte, missing count
    let data = vec![BTREE_INTERNAL_MARKER];
    let result = BTreeNode::deserialize(&data, HASH_LENGTH);
    assert!(result.is_err());
}

#[test]
fn test_deserialize_internal_truncated_key() {
    // Marker + key_count=1, but no key data
    let mut data = vec![BTREE_INTERNAL_MARKER];
    data.extend_from_slice(&1u16.to_le_bytes());
    let result = BTreeNode::deserialize(&data, HASH_LENGTH);
    assert!(result.is_err());
}

#[test]
fn test_deserialize_internal_truncated_child_hash() {
    // Marker + key_count=1 + one key, but missing child hashes
    let mut data = vec![BTREE_INTERNAL_MARKER];
    data.extend_from_slice(&1u16.to_le_bytes());
    let key = b"alpha";
    data.extend_from_slice(&(key.len() as u16).to_le_bytes());
    data.extend_from_slice(key);
    // Need 2 child hashes (key_count + 1), but provide none
    let result = BTreeNode::deserialize(&data, HASH_LENGTH);
    assert!(result.is_err());
}

// ─── Integration tests (with StorageEngine) ─────────────────────────────────

fn setup_engine() -> (Arc<StorageEngine>, tempfile::TempDir) {
    create_temp_engine_for_tests()
}

fn create_empty_root(engine: &StorageEngine) -> Vec<u8> {
    let algo = engine.hash_algo();
    let hash_length = algo.hash_length();
    let leaf = BTreeNode::Leaf(LeafNode::new());
    store_btree_node(engine, &leaf, hash_length, &algo).unwrap()
}

#[test]
fn test_btree_insert_single() {
    let (engine, _dir) = setup_engine();
    let algo = engine.hash_algo();
    let hl = algo.hash_length();

    let root = create_empty_root(&engine);
    let new_root = btree_insert(&engine, &root, make_entry("alpha"), hl, &algo).unwrap();

    let found = btree_lookup(&engine, &new_root, "alpha", hl).unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().name, "alpha");
}

#[test]
fn test_btree_insert_multiple() {
    let (engine, _dir) = setup_engine();
    let algo = engine.hash_algo();
    let hl = algo.hash_length();

    let mut root = create_empty_root(&engine);
    for i in 0..10 {
        root = btree_insert(&engine, &root, make_entry(&format!("item_{:02}", i)), hl, &algo).unwrap();
    }

    for i in 0..10 {
        let found = btree_lookup(&engine, &root, &format!("item_{:02}", i), hl).unwrap();
        assert!(found.is_some(), "Could not find item_{:02}", i);
    }
}

#[test]
fn test_btree_insert_sorted_order() {
    let (engine, _dir) = setup_engine();
    let algo = engine.hash_algo();
    let hl = algo.hash_length();

    let mut root = create_empty_root(&engine);
    // Insert in reverse order
    for i in (0..10).rev() {
        root = btree_insert(&engine, &root, make_entry(&format!("item_{:02}", i)), hl, &algo).unwrap();
    }

    let entries = btree_list(&engine, &root, hl).unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
}

#[test]
fn test_btree_insert_causes_split() {
    let (engine, _dir) = setup_engine();
    let algo = engine.hash_algo();
    let hl = algo.hash_length();

    let mut root = create_empty_root(&engine);
    // Insert enough to cause at least one split
    let count = BTREE_MAX_LEAF_ENTRIES + 5;
    for i in 0..count {
        root = btree_insert(&engine, &root, make_entry(&format!("entry_{:04}", i)), hl, &algo).unwrap();
    }

    // The root should now be an internal node (tree grew)
    let root_data = engine.get_entry(&root).unwrap().unwrap();
    let root_node = BTreeNode::deserialize(&root_data.2, hl).unwrap();
    assert!(!root_node.is_leaf(), "Root should be internal after split");

    // All entries should still be findable
    for i in 0..count {
        let found = btree_lookup(&engine, &root, &format!("entry_{:04}", i), hl).unwrap();
        assert!(found.is_some(), "Could not find entry_{:04} after split", i);
    }
}

#[test]
fn test_btree_insert_update() {
    let (engine, _dir) = setup_engine();
    let algo = engine.hash_algo();
    let hl = algo.hash_length();

    let mut root = create_empty_root(&engine);
    root = btree_insert(&engine, &root, make_entry("alpha"), hl, &algo).unwrap();
    root = btree_insert(&engine, &root, make_entry_with_hash("alpha", 0xFF), hl, &algo).unwrap();

    let found = btree_lookup(&engine, &root, "alpha", hl).unwrap().unwrap();
    assert_eq!(found.hash, vec![0xFF; 32]);

    // Should still be only one entry
    let entries = btree_list(&engine, &root, hl).unwrap();
    assert_eq!(entries.len(), 1);
}

#[test]
fn test_btree_lookup_missing() {
    let (engine, _dir) = setup_engine();
    let algo = engine.hash_algo();
    let hl = algo.hash_length();

    let mut root = create_empty_root(&engine);
    root = btree_insert(&engine, &root, make_entry("alpha"), hl, &algo).unwrap();

    let found = btree_lookup(&engine, &root, "zulu", hl).unwrap();
    assert!(found.is_none());
}

#[test]
fn test_btree_list_empty() {
    let (engine, _dir) = setup_engine();
    let algo = engine.hash_algo();
    let hl = algo.hash_length();

    let root = create_empty_root(&engine);
    let entries = btree_list(&engine, &root, hl).unwrap();
    assert!(entries.is_empty());
}

#[test]
fn test_btree_delete() {
    let (engine, _dir) = setup_engine();
    let algo = engine.hash_algo();
    let hl = algo.hash_length();

    let mut root = create_empty_root(&engine);
    root = btree_insert(&engine, &root, make_entry("alpha"), hl, &algo).unwrap();
    root = btree_insert(&engine, &root, make_entry("bravo"), hl, &algo).unwrap();

    let new_root = btree_delete(&engine, &root, "alpha", hl, &algo).unwrap();
    assert!(new_root.is_some());
    let new_root = new_root.unwrap();

    let found = btree_lookup(&engine, &new_root, "alpha", hl).unwrap();
    assert!(found.is_none());
    let found = btree_lookup(&engine, &new_root, "bravo", hl).unwrap();
    assert!(found.is_some());
}

#[test]
fn test_btree_delete_missing() {
    let (engine, _dir) = setup_engine();
    let algo = engine.hash_algo();
    let hl = algo.hash_length();

    let mut root = create_empty_root(&engine);
    root = btree_insert(&engine, &root, make_entry("alpha"), hl, &algo).unwrap();

    // Deleting a name that doesn't exist should not error
    let result = btree_delete(&engine, &root, "nonexistent", hl, &algo).unwrap();
    assert!(result.is_some());
    // Original entry should still be there
    let found = btree_lookup(&engine, &result.unwrap(), "alpha", hl).unwrap();
    assert!(found.is_some());
}

#[test]
fn test_btree_from_entries() {
    let (engine, _dir) = setup_engine();
    let algo = engine.hash_algo();
    let hl = algo.hash_length();

    let entries: Vec<ChildEntry> = (0..100)
        .map(|i| make_entry(&format!("entry_{:04}", i)))
        .collect();

    let root = btree_from_entries(&engine, entries, hl, &algo).unwrap();

    // All entries findable
    for i in 0..100 {
        let found = btree_lookup(&engine, &root, &format!("entry_{:04}", i), hl).unwrap();
        assert!(found.is_some(), "Could not find entry_{:04} after bulk build", i);
    }
}

#[test]
fn test_btree_from_entries_sorted() {
    let (engine, _dir) = setup_engine();
    let algo = engine.hash_algo();
    let hl = algo.hash_length();

    // Insert in random-ish order
    let entries: Vec<ChildEntry> = (0..50)
        .map(|i| make_entry(&format!("entry_{:04}", (i * 7) % 50)))
        .collect();

    let root = btree_from_entries(&engine, entries, hl, &algo).unwrap();
    let listed = btree_list(&engine, &root, hl).unwrap();

    for w in listed.windows(2) {
        assert!(w[0].name <= w[1].name, "List not sorted: {} > {}", w[0].name, w[1].name);
    }
}

#[test]
fn test_btree_large_directory() {
    let (engine, _dir) = setup_engine();
    let algo = engine.hash_algo();
    let hl = algo.hash_length();

    let mut root = create_empty_root(&engine);
    for i in 0..1000 {
        root = btree_insert(&engine, &root, make_entry(&format!("file_{:05}", i)), hl, &algo).unwrap();
    }

    // All findable
    for i in 0..1000 {
        let found = btree_lookup(&engine, &root, &format!("file_{:05}", i), hl).unwrap();
        assert!(found.is_some(), "Could not find file_{:05}", i);
    }

    // List returns correct count
    let entries = btree_list(&engine, &root, hl).unwrap();
    assert_eq!(entries.len(), 1000);

    // List is sorted
    for w in entries.windows(2) {
        assert!(w[0].name < w[1].name);
    }
}

#[test]
fn test_btree_structural_sharing() {
    let (engine, _dir) = setup_engine();
    let algo = engine.hash_algo();
    let hl = algo.hash_length();

    let mut root = create_empty_root(&engine);
    root = btree_insert(&engine, &root, make_entry("alpha"), hl, &algo).unwrap();
    let old_root = root.clone();

    // Insert another entry — should create a new root
    root = btree_insert(&engine, &root, make_entry("bravo"), hl, &algo).unwrap();

    // Old root should still be valid and contain only "alpha"
    let old_entries = btree_list(&engine, &old_root, hl).unwrap();
    assert_eq!(old_entries.len(), 1);
    assert_eq!(old_entries[0].name, "alpha");

    // New root should have both
    let new_entries = btree_list(&engine, &root, hl).unwrap();
    assert_eq!(new_entries.len(), 2);
}

#[test]
fn test_btree_content_hash_changes() {
    let (engine, _dir) = setup_engine();
    let algo = engine.hash_algo();
    let hl = algo.hash_length();

    let mut root = create_empty_root(&engine);
    root = btree_insert(&engine, &root, make_entry("alpha"), hl, &algo).unwrap();
    let hash_before = root.clone();

    root = btree_insert(&engine, &root, make_entry("bravo"), hl, &algo).unwrap();
    assert_ne!(hash_before, root, "Root hash should change after insert");
}

#[test]
fn test_btree_delete_to_empty() {
    let (engine, _dir) = setup_engine();
    let algo = engine.hash_algo();
    let hl = algo.hash_length();

    let mut root = create_empty_root(&engine);
    root = btree_insert(&engine, &root, make_entry("alpha"), hl, &algo).unwrap();
    root = btree_insert(&engine, &root, make_entry("bravo"), hl, &algo).unwrap();

    let result = btree_delete(&engine, &root, "alpha", hl, &algo).unwrap();
    assert!(result.is_some());
    let result = btree_delete(&engine, &result.unwrap(), "bravo", hl, &algo).unwrap();
    assert!(result.is_none(), "Deleting all entries should return None");
}

#[test]
fn test_btree_many_splits() {
    let (engine, _dir) = setup_engine();
    let algo = engine.hash_algo();
    let hl = algo.hash_length();

    let mut root = create_empty_root(&engine);
    for i in 0..500 {
        root = btree_insert(&engine, &root, make_entry(&format!("item_{:05}", i)), hl, &algo).unwrap();
    }

    // All findable
    for i in 0..500 {
        let found = btree_lookup(&engine, &root, &format!("item_{:05}", i), hl).unwrap();
        assert!(found.is_some(), "Could not find item_{:05} after 500 inserts", i);
    }

    let entries = btree_list(&engine, &root, hl).unwrap();
    assert_eq!(entries.len(), 500);
}

#[test]
fn test_btree_delete_from_multi_level() {
    let (engine, _dir) = setup_engine();
    let algo = engine.hash_algo();
    let hl = algo.hash_length();

    // Build a tree with enough entries to guarantee multiple levels
    let count = BTREE_MAX_LEAF_ENTRIES * 3;
    let mut root = create_empty_root(&engine);
    for i in 0..count {
        root = btree_insert(&engine, &root, make_entry(&format!("entry_{:04}", i)), hl, &algo).unwrap();
    }

    // Verify it's multi-level
    let root_data = engine.get_entry(&root).unwrap().unwrap();
    let root_node = BTreeNode::deserialize(&root_data.2, hl).unwrap();
    assert!(!root_node.is_leaf(), "Should be multi-level tree");

    // Delete entries from various positions
    let to_delete = vec![
        format!("entry_{:04}", 0),           // first
        format!("entry_{:04}", count / 2),    // middle
        format!("entry_{:04}", count - 1),    // last
    ];

    let mut current = root;
    for name in &to_delete {
        let result = btree_delete(&engine, &current, name, hl, &algo).unwrap();
        assert!(result.is_some());
        current = result.unwrap();
    }

    // Verify deleted entries are gone
    for name in &to_delete {
        let found = btree_lookup(&engine, &current, name, hl).unwrap();
        assert!(found.is_none(), "{} should have been deleted", name);
    }

    // Verify remaining entries are still present
    let entries = btree_list(&engine, &current, hl).unwrap();
    assert_eq!(entries.len(), count - to_delete.len());
}

// ─── btree_list_from_node tests ─────────────────────────────────────────────

#[test]
fn test_btree_list_from_node_leaf() {
    let leaf = BTreeNode::Leaf(LeafNode {
        entries: vec![make_entry("alpha"), make_entry("bravo")],
    });
    let data = leaf.serialize(HASH_LENGTH);

    // No engine needed for leaf-only case (but signature requires it)
    let (engine, _dir) = setup_engine();
    let entries = btree_list_from_node(&data, &engine, HASH_LENGTH).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "alpha");
    assert_eq!(entries[1].name, "bravo");
}

#[test]
fn test_btree_list_from_node_internal() {
    let (engine, _dir) = setup_engine();
    let algo = engine.hash_algo();
    let hl = algo.hash_length();

    // Build a multi-level tree via bulk build
    let entries: Vec<ChildEntry> = (0..100)
        .map(|i| make_entry(&format!("entry_{:04}", i)))
        .collect();
    let root_hash = btree_from_entries(&engine, entries, hl, &algo).unwrap();

    // Get the root node data
    let root_data = engine.get_entry(&root_hash).unwrap().unwrap();

    // btree_list_from_node should produce the same result as btree_list
    let from_node = btree_list_from_node(&root_data.2, &engine, hl).unwrap();
    let from_hash = btree_list(&engine, &root_hash, hl).unwrap();

    assert_eq!(from_node.len(), from_hash.len());
    for (a, b) in from_node.iter().zip(from_hash.iter()) {
        assert_eq!(a.name, b.name);
    }
}

// ─── Bulk build edge cases ──────────────────────────────────────────────────

#[test]
fn test_btree_from_entries_empty() {
    let (engine, _dir) = setup_engine();
    let algo = engine.hash_algo();
    let hl = algo.hash_length();

    let root = btree_from_entries(&engine, vec![], hl, &algo).unwrap();
    let entries = btree_list(&engine, &root, hl).unwrap();
    assert!(entries.is_empty());
}

#[test]
fn test_btree_from_entries_single() {
    let (engine, _dir) = setup_engine();
    let algo = engine.hash_algo();
    let hl = algo.hash_length();

    let root = btree_from_entries(&engine, vec![make_entry("only")], hl, &algo).unwrap();
    let entries = btree_list(&engine, &root, hl).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "only");
}

#[test]
fn test_btree_from_entries_exact_leaf_size() {
    let (engine, _dir) = setup_engine();
    let algo = engine.hash_algo();
    let hl = algo.hash_length();

    let entries: Vec<ChildEntry> = (0..BTREE_MAX_LEAF_ENTRIES)
        .map(|i| make_entry(&format!("entry_{:04}", i)))
        .collect();
    let root = btree_from_entries(&engine, entries, hl, &algo).unwrap();

    // Should be a single leaf (no internal nodes needed)
    let root_data = engine.get_entry(&root).unwrap().unwrap();
    let root_node = BTreeNode::deserialize(&root_data.2, hl).unwrap();
    assert!(root_node.is_leaf(), "Should be a single leaf for exactly MAX_LEAF entries");

    let listed = btree_list(&engine, &root, hl).unwrap();
    assert_eq!(listed.len(), BTREE_MAX_LEAF_ENTRIES);
}

// ─── Content hash with domain prefix ────────────────────────────────────────

#[test]
fn test_content_hash_domain_prefix() {
    let algo = HashAlgorithm::Blake3_256;
    let leaf = BTreeNode::Leaf(LeafNode {
        entries: vec![make_entry("alpha")],
    });

    // Compute hash manually with "btree:" prefix
    let serialized = leaf.serialize(HASH_LENGTH);
    let mut prefixed = Vec::with_capacity(6 + serialized.len());
    prefixed.extend_from_slice(b"btree:");
    prefixed.extend_from_slice(&serialized);
    let expected = algo.compute_hash(&prefixed).unwrap();

    let actual = leaf.content_hash(HASH_LENGTH, &algo).unwrap();
    assert_eq!(actual, expected);

    // Hash WITHOUT prefix should differ
    let no_prefix = algo.compute_hash(&serialized).unwrap();
    assert_ne!(actual, no_prefix, "Domain prefix should change the hash");
}
