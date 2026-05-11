use aeordb::auth::{generate_api_key, hash_api_key, ApiKeyRecord};
use aeordb::engine::RequestContext;
use aeordb::engine::ROOT_USER_ID;
use aeordb::engine::system_store;
use aeordb::server::create_engine_for_storage;

/// Helper: bootstrap a root API key into a fresh engine so there is something
/// to revoke during emergency reset.
fn bootstrap_root_key(engine: &aeordb::engine::StorageEngine) -> String {

  let key_id = uuid::Uuid::new_v4();
  let plaintext_key = generate_api_key(key_id);
  let key_hash = hash_api_key(&plaintext_key).unwrap();
  let record = ApiKeyRecord {
    key_id,
    key_hash,
    user_id: Some(ROOT_USER_ID),
    created_at: chrono::Utc::now(),
    is_revoked: false,
    expires_at: chrono::Utc::now().timestamp_millis() + (365 * 86400 * 1000),
    label: Some("test-root-key".to_string()),
    rules: vec![],
  };
  system_store::store_api_key_for_bootstrap(engine, &RequestContext::system(), &record)
    .expect("failed to store root key");
  plaintext_key
}

#[test]
fn test_emergency_reset_generates_new_key() {
  let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
  let engine_path = temp_dir.path().join("test_reset.aeordb");
  let engine_path_str = engine_path.to_str().unwrap();

  // Create engine and bootstrap a root key.
  let engine = create_engine_for_storage(engine_path_str);
  let _old_key = bootstrap_root_key(&engine);

  // Verify the old key exists and is not revoked.

  let keys_before = system_store::list_api_keys(&engine).unwrap();
  assert_eq!(keys_before.len(), 1);
  assert!(!keys_before[0].is_revoked);
  assert_eq!(keys_before[0].user_id, Some(ROOT_USER_ID));


  drop(engine);

  // Run emergency reset (with --force to skip prompt).
  aeordb_cli::commands::emergency_reset::run(engine_path_str, true);

  // Re-open and verify.
  let engine = create_engine_for_storage(engine_path_str);

  let keys_after = system_store::list_api_keys(&engine).unwrap();

  // Should have 2 keys now: old (revoked) and new (active).
  assert_eq!(keys_after.len(), 2, "Should have old revoked + new active key");

  let revoked_keys: Vec<_> = keys_after.iter().filter(|k| k.is_revoked).collect();
  let active_keys: Vec<_> = keys_after.iter().filter(|k| !k.is_revoked).collect();

  assert_eq!(revoked_keys.len(), 1, "Should have exactly 1 revoked key");
  assert_eq!(active_keys.len(), 1, "Should have exactly 1 active key");
  assert_eq!(active_keys[0].user_id, Some(ROOT_USER_ID));
  assert_eq!(revoked_keys[0].user_id, Some(ROOT_USER_ID));
}

#[test]
fn test_emergency_reset_revokes_old_key() {
  let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
  let engine_path = temp_dir.path().join("test_revoke.aeordb");
  let engine_path_str = engine_path.to_str().unwrap();

  // Create engine and bootstrap two root keys.
  let engine = create_engine_for_storage(engine_path_str);
  let _key1 = bootstrap_root_key(&engine);
  let _key2 = bootstrap_root_key(&engine);

  let keys_before = system_store::list_api_keys(&engine).unwrap();
  let active_count_before = keys_before.iter().filter(|k| !k.is_revoked && k.user_id == Some(ROOT_USER_ID)).count();
  assert_eq!(active_count_before, 2, "Should have 2 active root keys before reset");


  drop(engine);

  // Run emergency reset.
  aeordb_cli::commands::emergency_reset::run(engine_path_str, true);

  // Verify all old root keys are revoked and one new one is active.
  let engine = create_engine_for_storage(engine_path_str);

  let keys_after = system_store::list_api_keys(&engine).unwrap();

  let active_root_keys: Vec<_> = keys_after
    .iter()
    .filter(|k| !k.is_revoked && k.user_id == Some(ROOT_USER_ID))
    .collect();
  let revoked_root_keys: Vec<_> = keys_after
    .iter()
    .filter(|k| k.is_revoked && k.user_id == Some(ROOT_USER_ID))
    .collect();

  assert_eq!(active_root_keys.len(), 1, "Should have exactly 1 active root key after reset");
  assert_eq!(revoked_root_keys.len(), 2, "Should have 2 revoked root keys after reset");
}

#[test]
fn test_emergency_reset_on_empty_database() {
  let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
  let engine_path = temp_dir.path().join("test_empty.aeordb");
  let engine_path_str = engine_path.to_str().unwrap();

  // Create engine with no keys at all.
  let engine = create_engine_for_storage(engine_path_str);
  drop(engine);

  // Run emergency reset -- should still create a new root key.
  aeordb_cli::commands::emergency_reset::run(engine_path_str, true);

  let engine = create_engine_for_storage(engine_path_str);

  let keys = system_store::list_api_keys(&engine).unwrap();

  let active_root_keys: Vec<_> = keys
    .iter()
    .filter(|k| !k.is_revoked && k.user_id == Some(ROOT_USER_ID))
    .collect();
  assert_eq!(active_root_keys.len(), 1, "Should have created a new root key");
}
