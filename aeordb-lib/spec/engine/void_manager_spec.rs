use aeordb::engine::hash_algorithm::HashAlgorithm;
use aeordb::engine::void_manager::{VoidManager, MINIMUM_USEFUL_VOID_SIZE};

#[test]
fn test_register_and_find_void() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  manager.register_void(5000, 1000);

  let result = manager.find_void(1000);
  assert!(result.is_some());

  let (offset, size) = result.unwrap();
  assert_eq!(offset, 5000);
  assert_eq!(size, 1000);
}

#[test]
fn test_find_void_best_fit() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  // Register three voids of different sizes.
  manager.register_void(1000, 500);
  manager.register_void(2000, 200);
  manager.register_void(3000, 800);

  // Request 150 bytes — should find the 200-byte void (smallest fit).
  let result = manager.find_void(150);
  assert!(result.is_some());

  let (offset, size) = result.unwrap();
  assert_eq!(offset, 2000);
  assert_eq!(size, 200);
}

#[test]
fn test_find_void_with_splitting() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  // Register a large void.
  manager.register_void(1000, 500);

  let needed = 100;
  let result = manager.find_void(needed);
  assert!(result.is_some());

  let (offset, size) = result.unwrap();
  assert_eq!(offset, 1000);
  assert_eq!(size, 500);

  // Remainder (400 bytes at offset 1100) should be re-registered if >= useful min.
  let min_useful = manager.minimum_useful_void_size();
  if 400 >= min_useful {
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

  manager.register_void(5000, 100);

  let result = manager.find_void(200);
  assert!(result.is_none());

  // Original void is still tracked.
  assert_eq!(manager.void_count(), 1);
}

#[test]
fn test_register_tracks_all_sizes_including_below_useful_min() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);
  let min_useful = manager.minimum_useful_void_size();

  // Sub-minimum voids ARE tracked now — useful for fragmentation metrics.
  manager.register_void(1000, min_useful - 1);
  assert_eq!(manager.void_count(), 1);

  manager.register_void(2000, min_useful);
  assert_eq!(manager.void_count(), 2);
}

#[test]
fn test_find_void_remainder_below_minimum_is_abandoned() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);
  let min_useful = manager.minimum_useful_void_size();

  // Create a void whose remainder after splitting is below the useful min.
  let void_size = min_useful + (min_useful - 2);
  manager.register_void(1000, void_size);

  let result = manager.find_void(min_useful);
  assert!(result.is_some());

  // Remainder is below useful min, so it should NOT be re-registered.
  assert_eq!(manager.void_count(), 0);
}

#[test]
fn test_remove_void() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  manager.register_void(1000, 500);
  manager.register_void(2000, 500);
  assert_eq!(manager.void_count(), 2);

  let removed = manager.remove_void(1000);
  assert_eq!(removed, Some(500));
  assert_eq!(manager.void_count(), 1);

  // Remaining void at offset 2000.
  let result = manager.find_void(500);
  assert!(result.is_some());
  assert_eq!(result.unwrap().0, 2000);
}

#[test]
fn test_remove_void_nonexistent() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  let removed = manager.remove_void(1000);
  assert!(removed.is_none());
  assert_eq!(manager.void_count(), 0);
}

#[test]
fn test_register_dedup_same_offset() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  manager.register_void(1000, 500);
  manager.register_void(1000, 500); // exact dup
  assert_eq!(manager.void_count(), 1);

  // Re-registering same offset with different size updates it.
  manager.register_void(1000, 600);
  assert_eq!(manager.void_count(), 1);
  assert_eq!(manager.total_void_space(), 600);
}

#[test]
fn test_total_void_space() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  manager.register_void(1000, 500);
  manager.register_void(2000, 300);
  manager.register_void(3000, 200);

  assert_eq!(manager.total_void_space(), 1000);
}

#[test]
fn test_void_count() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  assert_eq!(manager.void_count(), 0);

  manager.register_void(1000, 500);
  assert_eq!(manager.void_count(), 1);

  manager.register_void(2000, 500);
  assert_eq!(manager.void_count(), 2);

  manager.register_void(3000, 300);
  assert_eq!(manager.void_count(), 3);
}

#[test]
fn test_iter_in_offset_order() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  manager.register_void(3000, 200);
  manager.register_void(1000, 500);
  manager.register_void(2000, 300);

  let collected: Vec<_> = manager.iter().collect();
  assert_eq!(collected, vec![(1000, 500), (2000, 300), (3000, 200)]);
}

#[test]
fn test_replace_all_bulk_repopulate() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);
  manager.register_void(1000, 500);

  manager.replace_all(vec![(5000, 800), (7000, 1200)]);
  assert_eq!(manager.void_count(), 2);
  assert!(manager.find_void(1200).is_some());
  // The original void at 1000 was cleared out.
  let (offset, _) = manager.find_void(500).unwrap();
  assert_eq!(offset, 5000); // remainder of the 800-byte void from offset 5000
}

#[test]
fn test_void_hash_deterministic() {
  let hash_a = VoidManager::void_hash(1024);
  let hash_b = VoidManager::void_hash(1024);
  let hash_c = VoidManager::void_hash(2048);

  assert_eq!(hash_a, hash_b);
  assert_ne!(hash_a, hash_c);
  assert_eq!(hash_a.len(), 32);
}

#[test]
fn test_void_hash_matches_expected_format() {
  let expected = blake3::hash(b"::aeordb:void:1024").as_bytes().to_vec();
  let actual = VoidManager::void_hash(1024);
  assert_eq!(actual, expected);
}

#[test]
fn test_minimum_useful_void_size_matches_entry_header() {
  let manager = VoidManager::new(HashAlgorithm::Blake3_256);
  let min = manager.minimum_useful_void_size();

  // BLAKE3_256: fixed_header(31) + hash(32) + key(0) + value(0) = 63
  assert_eq!(min, 63);
  assert_eq!(MINIMUM_USEFUL_VOID_SIZE, 63);
}

#[test]
fn test_multiple_voids_same_size() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  manager.register_void(1000, 500);
  manager.register_void(2000, 500);
  manager.register_void(3000, 500);

  assert_eq!(manager.void_count(), 3);
  assert_eq!(manager.total_void_space(), 1500);

  // Each find_void consumes one, in offset order (BTreeSet).
  let first = manager.find_void(500).unwrap();
  assert_eq!(first, (1000, 500));

  let second = manager.find_void(500).unwrap();
  assert_eq!(second, (2000, 500));

  let third = manager.find_void(500).unwrap();
  assert_eq!(third, (3000, 500));

  assert!(manager.find_void(500).is_none());
  assert_eq!(manager.void_count(), 0);
}

#[test]
fn test_total_void_space_after_consumption() {
  let mut manager = VoidManager::new(HashAlgorithm::Blake3_256);

  manager.register_void(1000, 500);
  manager.register_void(2000, 300);
  assert_eq!(manager.total_void_space(), 800);

  manager.find_void(500);
  assert_eq!(manager.total_void_space(), 300);
}
