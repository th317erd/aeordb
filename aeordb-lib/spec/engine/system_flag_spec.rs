use std::sync::Arc;

use aeordb::engine::{
    FLAG_SYSTEM, StorageEngine,
    chunk_content_hash, system_chunk_hash, system_file_identity_hash,
    is_system_path, HashAlgorithm,
};
use aeordb::engine::entry_type::EntryType;
use aeordb::server::create_temp_engine_for_tests;

fn setup() -> (Arc<StorageEngine>, tempfile::TempDir) {
    create_temp_engine_for_tests()
}

// ─── 1. FLAG_SYSTEM constant ───────────────────────────────────────────────

#[test]
fn test_flag_system_constant() {
    assert_eq!(FLAG_SYSTEM, 0x01, "FLAG_SYSTEM must be 0x01");
}

// ─── 2. system_chunk_hash differs from user chunk_content_hash ──────────────

#[test]
fn test_system_chunk_hash_differs_from_user() {
    let algo = HashAlgorithm::Blake3_256;
    let data = b"some chunk data";

    let user_hash = chunk_content_hash(data, &algo).unwrap();
    let system_hash = system_chunk_hash(data, &algo).unwrap();

    assert_ne!(
        user_hash, system_hash,
        "System hash must differ from user hash for identical data (different domain prefix)"
    );
}

// ─── 3. system_chunk_hash is deterministic ──────────────────────────────────

#[test]
fn test_system_chunk_hash_deterministic() {
    let algo = HashAlgorithm::Blake3_256;
    let data = b"deterministic test data";

    let hash1 = system_chunk_hash(data, &algo).unwrap();
    let hash2 = system_chunk_hash(data, &algo).unwrap();

    assert_eq!(hash1, hash2, "Same input must produce same system hash");
}

// ─── 4. system_file_identity_hash produces valid non-empty hash ─────────────

#[test]
fn test_system_file_identity_hash() {
    let algo = HashAlgorithm::Blake3_256;
    let chunk_hash = system_chunk_hash(b"chunk data", &algo).unwrap();
    let chunk_hashes = vec![chunk_hash];

    let hash = system_file_identity_hash(
        "/.system/config/key",
        Some("application/json"),
        &chunk_hashes,
        &algo,
    ).unwrap();

    assert!(!hash.is_empty(), "System file identity hash must be non-empty");
    assert_eq!(hash.len(), algo.hash_length(), "Hash length must match algorithm");
}

// ─── 5. is_system_path ─────────────────────────────────────────────────────

#[test]
fn test_is_system_path() {
    // Positive cases
    assert!(is_system_path("/.system/config/key"), "/.system/config/key is a system path");
    assert!(is_system_path("/.system"), "/.system is a system path");
    assert!(is_system_path("/.system/"), "/.system/ is a system path");
    assert!(is_system_path("/.system/deeply/nested/path"), "deeply nested system path");

    // Negative cases
    assert!(!is_system_path("/regular/path"), "/regular/path is not a system path");
    assert!(!is_system_path("/.systems/"), "/.systems/ is not a system path (note the 's')");
    assert!(!is_system_path("/.systemic/data"), "/.systemic/data is not a system path");
    assert!(!is_system_path("/data/.system/nested"), "/data/.system/nested is not a system path");
    assert!(!is_system_path("/"), "root is not a system path");
    assert!(!is_system_path(""), "empty string is not a system path");
}

// ─── 6. store_entry_with_flags round-trip ───────────────────────────────────

#[test]
fn test_store_entry_with_flags() {
    let (engine, _temp) = setup();
    let key = b"test-key-with-flags";
    let value = b"test-value";

    let offset = engine.store_entry_with_flags(
        EntryType::Chunk,
        key,
        value,
        FLAG_SYSTEM,
    ).unwrap();

    assert!(offset > 0, "Offset should be positive (after file header)");

    // Read back and verify flags
    let result = engine.get_entry(key).unwrap();
    assert!(result.is_some(), "Entry must be retrievable after store");

    let (header, retrieved_key, retrieved_value) = result.unwrap();
    assert_eq!(retrieved_key, key, "Key must match");
    assert_eq!(retrieved_value, value, "Value must match");
    assert!(
        header.flags & FLAG_SYSTEM != 0,
        "FLAG_SYSTEM must be set on the retrieved entry header (flags=0x{:02X})",
        header.flags
    );
}

// ─── 7. is_system_entry ────────────────────────────────────────────────────

#[test]
fn test_is_system_entry() {
    let (engine, _temp) = setup();

    // Store entry WITH FLAG_SYSTEM
    let sys_key = b"system-entry-key";
    engine.store_entry_with_flags(EntryType::Chunk, sys_key, b"sys", FLAG_SYSTEM).unwrap();
    let (sys_header, _, _) = engine.get_entry(sys_key).unwrap().unwrap();
    assert!(sys_header.is_system_entry(), "Entry with FLAG_SYSTEM must return true for is_system_entry");

    // Store entry WITHOUT FLAG_SYSTEM
    let user_key = b"user-entry-key";
    engine.store_entry(EntryType::Chunk, user_key, b"user").unwrap();
    let (user_header, _, _) = engine.get_entry(user_key).unwrap().unwrap();
    assert!(!user_header.is_system_entry(), "Entry without FLAG_SYSTEM must return false for is_system_entry");
}

// ─── 8. user cannot forge system hash ──────────────────────────────────────

#[test]
fn test_user_cannot_forge_system_hash() {
    let algo = HashAlgorithm::Blake3_256;

    // The attacker's strategy: prefix user data with "system::" so that
    // the user chunk hash input becomes "chunk:system::payload".
    // The real system hash input is "system::payload".
    // These MUST differ because the domain prefixes are different.

    let payload = b"sensitive-system-data";

    // Real system chunk hash: hash("system::" + payload)
    let real_system_hash = system_chunk_hash(payload, &algo).unwrap();

    // Attacker tries to forge by passing "system::payload" as user chunk data.
    // User chunk hash: hash("chunk:" + "system::" + payload)
    let mut forged_data = Vec::new();
    forged_data.extend_from_slice(b"system::");
    forged_data.extend_from_slice(payload);
    let forged_hash = chunk_content_hash(&forged_data, &algo).unwrap();

    assert_ne!(
        real_system_hash, forged_hash,
        "User chunk hash with 'system::' prefix in data must NOT equal system chunk hash. \
         Domain separation prevents forgery."
    );

    // Also verify the reverse: attacker tries to create system hash that
    // collides with a user chunk hash.
    // User chunk hash: hash("chunk:" + payload)
    let user_hash = chunk_content_hash(payload, &algo).unwrap();
    // System hash: hash("system::" + payload)
    assert_ne!(
        user_hash, real_system_hash,
        "System and user hashes must never collide for same payload"
    );

    // Verify domain prefix lengths differ to make collision structurally impossible.
    // "chunk:" = 6 bytes, "system::" = 8 bytes — different lengths ensure
    // no alignment trick can produce identical hash inputs.
    let chunk_prefix = b"chunk:";
    let system_prefix = b"system::";
    assert_ne!(
        chunk_prefix.len(), system_prefix.len(),
        "Domain prefixes must have different lengths for structural separation"
    );
}

// ─── 9. store_entry_with_flags zero flags behaves like store_entry ──────────

#[test]
fn test_store_entry_with_flags_zero_flags() {
    let (engine, _temp) = setup();
    let key = b"zero-flags-key";
    let value = b"zero-flags-value";

    engine.store_entry_with_flags(EntryType::Chunk, key, value, 0).unwrap();

    let (header, _, _) = engine.get_entry(key).unwrap().unwrap();
    assert_eq!(header.flags, 0, "Flags should be 0 when stored with 0");
    assert!(!header.is_system_entry(), "is_system_entry must be false with flags=0");
}

// ─── 10. system_file_identity_hash differs from user file_identity_hash ─────

#[test]
fn test_system_file_identity_hash_differs_from_user() {
    use aeordb::engine::file_identity_hash;

    let algo = HashAlgorithm::Blake3_256;
    let path = "/.system/config/key";
    let content_type = Some("text/plain");
    let chunk_data = b"shared chunk";
    let chunk_hash = chunk_content_hash(chunk_data, &algo).unwrap();
    let chunk_hashes = vec![chunk_hash];

    let user_hash = file_identity_hash(path, content_type, &chunk_hashes, &algo).unwrap();
    let sys_hash = system_file_identity_hash(path, content_type, &chunk_hashes, &algo).unwrap();

    assert_ne!(
        user_hash, sys_hash,
        "System file identity hash must differ from user file identity hash (different domain prefix)"
    );
}

// ─── 11. is_system_path normalizes before checking ──────────────────────────

#[test]
fn test_is_system_path_normalization() {
    // Paths with redundant slashes or dots should still be detected
    assert!(is_system_path("/.system/./config"), "normalized dotted path should be system");
    assert!(is_system_path("/.system//double-slash"), "double-slash should normalize");
}

// ─── 12. FLAG_SYSTEM is only the lowest bit ─────────────────────────────────

#[test]
fn test_flag_system_is_lowest_bit() {
    // Ensure FLAG_SYSTEM only occupies bit 0, leaving bits 1-7 free for future flags
    assert_eq!(FLAG_SYSTEM & 0xFE, 0, "FLAG_SYSTEM must not set any bits above bit 0");
    assert_eq!(FLAG_SYSTEM.count_ones(), 1, "FLAG_SYSTEM must be a single-bit flag");
}
