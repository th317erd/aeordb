use super::*;

fn poison_registry(registry: &Mutex<HashSet<u64>>) {
  std::thread::scope(|scope| {
    let unwind = scope
      .spawn(|| {
        let _in_flight = registry.lock().unwrap();
        panic!("inject blob commit registry poison");
      })
      .join();
    assert!(unwind.is_err());
  });
}

#[test]
fn poisoned_blob_commit_registry_is_not_reported_as_a_duplicate() {
  let registry = Mutex::new(HashSet::new());
  poison_registry(&registry);

  let error = try_register_blob_commit(&registry, 41).unwrap_err();
  assert!(error.to_string().contains("blob commit registry lock poisoned"), "unexpected error: {error}");
}

#[test]
fn poisoned_blob_commit_registry_teardown_still_releases_its_signature() {
  let registry = Mutex::new(HashSet::from([42]));
  poison_registry(&registry);

  unregister_blob_commit(&registry, 42);

  let in_flight = registry.lock().unwrap_err().into_inner();
  assert!(!in_flight.contains(&42));
}
