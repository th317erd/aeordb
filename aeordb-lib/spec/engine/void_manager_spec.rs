use aeordb::engine::hash_algorithm::HashAlgorithm;
use aeordb::engine::void_manager::{VoidManager, MINIMUM_VOID_SIZE};

#[test]
fn test_register_and_find_void() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  manager.register_void(1000, 5000);

  let result = manager.find_void(1000);
  assert!(result.is_some());

  let (offset, size) = result.unwrap();
  assert_eq!(offset, 5000);
  assert_eq!(size, 1000);
}

#[test]
fn test_find_void_best_fit() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  // Register three voids of different sizes
  manager.register_void(500, 1000);
  manager.register_void(200, 2000);
  manager.register_void(800, 3000);

  // Request 150 bytes — should find the 200-byte void (smallest fit)
  let result = manager.find_void(150);
  assert!(result.is_some());

  let (offset, size) = result.unwrap();
  assert_eq!(offset, 2000);
  assert_eq!(size, 200);
}

#[test]
fn test_find_void_with_splitting() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  // Register a large void
  manager.register_void(500, 1000);

  // Request a smaller amount — should split the void
  let needed = 100;
  let result = manager.find_void(needed);
  assert!(result.is_some());

  let (offset, size) = result.unwrap();
  assert_eq!(offset, 1000);
  assert_eq!(size, 500);

  // The remainder (400 bytes at offset 1100) should now be registered
  let min_size = manager.minimum_void_size();
  if 400 >= min_size {
    let remainder = manager.find_void(400);
    assert!(remainder.is_some());
    let (rem_offset, rem_size) = remainder.unwrap();
    assert_eq!(rem_offset, 1000 + needed as u64);
    assert_eq!(rem_size, 400);
  }
}

#[test]
fn test_find_void_returns_none_when_empty() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  let result = manager.find_void(100);
  assert!(result.is_none());
}

#[test]
fn test_find_void_returns_none_when_too_small() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  manager.register_void(100, 5000);

  // Request more than available
  let result = manager.find_void(200);
  assert!(result.is_none());

  // Original void should still be there
  assert_eq!(manager.void_count(), 1);
}

#[test]
fn test_find_void_minimum_size() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);
  let min_size = manager.minimum_void_size();

  // Trying to register a void smaller than minimum should be ignored
  manager.register_void(min_size - 1, 1000);
  assert_eq!(manager.void_count(), 0);

  // Registering at exactly minimum should work
  manager.register_void(min_size, 2000);
  assert_eq!(manager.void_count(), 1);
}

#[test]
fn test_find_void_remainder_below_minimum_is_abandoned() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);
  let min_size = manager.minimum_void_size();

  // Create a void that, when split, leaves a remainder below minimum
  let void_size = min_size + (min_size - 2); // remainder will be min_size - 2
  manager.register_void(void_size, 1000);

  let result = manager.find_void(min_size);
  assert!(result.is_some());

  // The remainder is below minimum, so it should NOT be registered
  assert_eq!(manager.void_count(), 0);
}

#[test]
fn test_remove_void() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  manager.register_void(500, 1000);
  manager.register_void(500, 2000);
  assert_eq!(manager.void_count(), 2);

  manager.remove_void(500, 1000);
  assert_eq!(manager.void_count(), 1);

  // The remaining void should be at offset 2000
  let result = manager.find_void(500);
  assert!(result.is_some());
  assert_eq!(result.unwrap().0, 2000);
}

#[test]
fn test_remove_void_nonexistent() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  // Removing a void that doesn't exist should be a no-op
  manager.remove_void(500, 1000);
  assert_eq!(manager.void_count(), 0);
}

#[test]
fn test_total_void_space() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  manager.register_void(500, 1000);
  manager.register_void(300, 2000);
  manager.register_void(200, 3000);

  assert_eq!(manager.total_void_space(), 1000);
}

#[test]
fn test_void_count() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  assert_eq!(manager.void_count(), 0);

  manager.register_void(500, 1000);
  assert_eq!(manager.void_count(), 1);

  manager.register_void(500, 2000); // same size, different offset
  assert_eq!(manager.void_count(), 2);

  manager.register_void(300, 3000);
  assert_eq!(manager.void_count(), 3);
}

#[test]
fn test_void_hash_deterministic() {
  let hash_a = VoidManager::void_hash(1024);
  let hash_b = VoidManager::void_hash(1024);
  let hash_c = VoidManager::void_hash(2048);

  // Same input produces same output
  assert_eq!(hash_a, hash_b);
  // Different inputs produce different outputs
  assert_ne!(hash_a, hash_c);
  // Hash should be 32 bytes (BLAKE3)
  assert_eq!(hash_a.len(), 32);
}

#[test]
fn test_void_hash_matches_expected_format() {
  // The hash should be BLAKE3("::aeordb:void:1024")
  let expected = blake3::hash(b"::aeordb:void:1024").as_bytes().to_vec();
  let actual = VoidManager::void_hash(1024);
  assert_eq!(actual, expected);
}

#[test]
fn test_minimum_void_size_matches_entry_header() {
  let manager = VoidManager::new(HashAlgorithm::Blake3_256);
  let min = manager.minimum_void_size();

  // For BLAKE3_256: fixed_header(29) + hash(32) + key(0) + value(0) = 61
  assert_eq!(min, 61);

  // The constant should match for BLAKE3
  assert_eq!(MINIMUM_VOID_SIZE, 61);
}

#[test]
fn test_multiple_voids_same_size() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  manager.register_void(500, 1000);
  manager.register_void(500, 2000);
  manager.register_void(500, 3000);

  assert_eq!(manager.void_count(), 3);
  assert_eq!(manager.total_void_space(), 1500);

  // Finding voids should return them one at a time (LIFO from the Vec)
  let first = manager.find_void(500).unwrap();
  assert_eq!(first.1, 500);

  let second = manager.find_void(500).unwrap();
  assert_eq!(second.1, 500);
  assert_ne!(first.0, second.0);

  let third = manager.find_void(500).unwrap();
  assert_eq!(third.1, 500);

  // All consumed
  assert!(manager.find_void(500).is_none());
  assert_eq!(manager.void_count(), 0);
}

#[test]
fn test_total_void_space_after_consumption() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  manager.register_void(500, 1000);
  manager.register_void(300, 2000);
  assert_eq!(manager.total_void_space(), 800);

  manager.find_void(500);
  assert_eq!(manager.total_void_space(), 300);
}
