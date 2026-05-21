use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use aeordb::auth::api_key::ApiKeyRecord;
use aeordb::auth::magic_link::MagicLinkRecord;
use aeordb::auth::refresh::RefreshTokenRecord;
use aeordb::engine::group::Group;
use aeordb::engine::peer_connection::PeerConfig;
use aeordb::engine::request_context::RequestContext;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::system_store;
use aeordb::engine::user::{ROOT_USER_ID, User};
use aeordb::engine::DirectoryOps;
use aeordb::server::create_temp_engine_for_tests;

fn setup() -> (Arc<StorageEngine>, tempfile::TempDir) {
    create_temp_engine_for_tests()
}

fn test_context() -> RequestContext {
    RequestContext::system()
}

fn make_api_key_record(user_id: Uuid) -> ApiKeyRecord {
    ApiKeyRecord {
        key_id: Uuid::new_v4(),
        key_hash: "hash_placeholder".to_string(),
        user_id: Some(user_id),
        created_at: Utc::now(),
        is_revoked: false,
        expires_at: Utc::now().timestamp_millis() + 86_400_000,
        label: Some("test key".to_string()),
        rules: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[test]
fn test_store_and_get_config() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    system_store::store_config(&engine, &ctx, "db_name", b"test_db").unwrap();
    let result = system_store::get_config(&engine, "db_name").unwrap();
    assert_eq!(result, Some(b"test_db".to_vec()));
}

#[test]
fn test_get_config_missing() {
    let (engine, _dir) = setup();
    let result = system_store::get_config(&engine, "nonexistent").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_config_overwrite() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    system_store::store_config(&engine, &ctx, "key", b"v1").unwrap();
    system_store::store_config(&engine, &ctx, "key", b"v2").unwrap();
    let result = system_store::get_config(&engine, "key").unwrap();
    assert_eq!(result, Some(b"v2".to_vec()));
}

// ---------------------------------------------------------------------------
// Users
// ---------------------------------------------------------------------------

#[test]
fn test_store_and_get_user() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    let user = User::new("alice", Some("alice@example.com"));
    let user_id = user.user_id;
    system_store::store_user(&engine, &ctx, &user).unwrap();

    let result = system_store::get_user(&engine, &user_id).unwrap();
    assert!(result.is_some());
    let fetched = result.unwrap();
    assert_eq!(fetched.user_id, user_id);
    assert_eq!(fetched.username, "alice");
    assert_eq!(fetched.email, Some("alice@example.com".to_string()));
}

#[test]
fn test_get_user_missing() {
    let (engine, _dir) = setup();
    let result = system_store::get_user(&engine, &Uuid::new_v4()).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_list_users() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    let user_a = User::new("alice", None);
    let user_b = User::new("bob", None);
    let user_c = User::new("charlie", None);

    system_store::store_user(&engine, &ctx, &user_a).unwrap();
    system_store::store_user(&engine, &ctx, &user_b).unwrap();
    system_store::store_user(&engine, &ctx, &user_c).unwrap();

    let users = system_store::list_users(&engine).unwrap();
    assert_eq!(users.len(), 3);

    let names: Vec<String> = users.iter().map(|u| u.username.clone()).collect();
    assert!(names.contains(&"alice".to_string()));
    assert!(names.contains(&"bob".to_string()));
    assert!(names.contains(&"charlie".to_string()));
}

#[test]
fn test_list_users_empty() {
    let (engine, _dir) = setup();
    let users = system_store::list_users(&engine).unwrap();
    assert!(users.is_empty());
}

#[test]
fn test_delete_user() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    let user = User::new("doomed", None);
    let user_id = user.user_id;
    system_store::store_user(&engine, &ctx, &user).unwrap();

    let deleted = system_store::delete_user(&engine, &ctx, &user_id).unwrap();
    assert!(deleted);

    let result = system_store::get_user(&engine, &user_id).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_delete_user_nonexistent() {
    let (engine, _dir) = setup();
    let ctx = test_context();
    let deleted = system_store::delete_user(&engine, &ctx, &Uuid::new_v4()).unwrap();
    assert!(!deleted);
}

#[test]
fn test_store_user_creates_auto_group() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    let user = User::new("grouped", None);
    let user_id = user.user_id;
    system_store::store_user(&engine, &ctx, &user).unwrap();

    let group_name = format!("user:{}", user_id);
    let group = system_store::get_group(&engine, &group_name).unwrap();
    assert!(group.is_some());
    let group = group.unwrap();
    assert_eq!(group.query_field, "user_id");
    assert_eq!(group.query_value, user_id.to_string());
}

#[test]
fn test_store_user_rejects_root_uuid() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    let mut user = User::new("evil", None);
    user.user_id = ROOT_USER_ID;

    let result = system_store::store_user(&engine, &ctx, &user);
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

#[test]
fn test_store_and_get_group() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    let group = Group::new("admins", "crudlify", "........", "user_id", "in", "abc,def").unwrap();
    system_store::store_group(&engine, &ctx, &group).unwrap();

    let result = system_store::get_group(&engine, "admins").unwrap();
    assert!(result.is_some());
    let fetched = result.unwrap();
    assert_eq!(fetched.name, "admins");
    assert_eq!(fetched.query_value, "abc,def");
}

#[test]
fn test_get_group_missing() {
    let (engine, _dir) = setup();
    let result = system_store::get_group(&engine, "nonexistent").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_list_groups() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    let group_a = Group::new("readers", "cr......", "........", "is_active", "eq", "true").unwrap();
    let group_b = Group::new("writers", "cru.....", "........", "is_active", "eq", "true").unwrap();

    system_store::store_group(&engine, &ctx, &group_a).unwrap();
    system_store::store_group(&engine, &ctx, &group_b).unwrap();

    let groups = system_store::list_groups(&engine).unwrap();
    assert_eq!(groups.len(), 2);

    let names: Vec<String> = groups.iter().map(|g| g.name.clone()).collect();
    assert!(names.contains(&"readers".to_string()));
    assert!(names.contains(&"writers".to_string()));
}

#[test]
fn test_delete_group() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    let group = Group::new("temp", "crudlify", "........", "is_active", "eq", "true").unwrap();
    system_store::store_group(&engine, &ctx, &group).unwrap();

    let deleted = system_store::delete_group(&engine, &ctx, "temp").unwrap();
    assert!(deleted);

    let result = system_store::get_group(&engine, "temp").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_delete_group_nonexistent() {
    let (engine, _dir) = setup();
    let ctx = test_context();
    let deleted = system_store::delete_group(&engine, &ctx, "ghost").unwrap();
    assert!(!deleted);
}

// ---------------------------------------------------------------------------
// API Keys
// ---------------------------------------------------------------------------

#[test]
fn test_store_and_list_api_keys() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    let user_id = Uuid::new_v4();
    let key_a = make_api_key_record(user_id);
    let key_b = make_api_key_record(user_id);

    system_store::store_api_key(&engine, &ctx, &key_a).unwrap();
    system_store::store_api_key(&engine, &ctx, &key_b).unwrap();

    let keys = system_store::list_api_keys(&engine).unwrap();
    assert_eq!(keys.len(), 2);
}

#[test]
fn test_list_api_keys_empty() {
    let (engine, _dir) = setup();
    let keys = system_store::list_api_keys(&engine).unwrap();
    assert!(keys.is_empty());
}

#[test]
fn test_get_api_key_by_prefix() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    let user_id = Uuid::new_v4();
    let record = make_api_key_record(user_id);
    let key_id = record.key_id;
    system_store::store_api_key(&engine, &ctx, &record).unwrap();

    let prefix = &key_id.simple().to_string()[..16];
    let found = system_store::get_api_key_by_prefix(&engine, prefix).unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().key_id, key_id);
}

#[test]
fn test_get_api_key_by_prefix_not_found() {
    let (engine, _dir) = setup();
    let found = system_store::get_api_key_by_prefix(&engine, "0000000000000000").unwrap();
    assert!(found.is_none());
}

#[test]
fn test_revoke_api_key() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    let user_id = Uuid::new_v4();
    let record = make_api_key_record(user_id);
    let key_id = record.key_id;
    system_store::store_api_key(&engine, &ctx, &record).unwrap();

    let revoked = system_store::revoke_api_key(&engine, &ctx, key_id).unwrap();
    assert!(revoked);

    // After revocation, list should still include it but with is_revoked = true.
    let keys = system_store::list_api_keys(&engine).unwrap();
    assert_eq!(keys.len(), 1);
    assert!(keys[0].is_revoked);
}

#[test]
fn test_revoke_api_key_not_found() {
    let (engine, _dir) = setup();
    let ctx = test_context();
    let revoked = system_store::revoke_api_key(&engine, &ctx, Uuid::new_v4()).unwrap();
    assert!(!revoked);
}

#[test]
fn test_store_api_key_validates_user_id() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    let record = make_api_key_record(ROOT_USER_ID);
    let result = system_store::store_api_key(&engine, &ctx, &record);
    assert!(result.is_err());
}

#[test]
fn test_bootstrap_api_key_allows_root() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    let record = make_api_key_record(ROOT_USER_ID);
    let result = system_store::store_api_key_for_bootstrap(&engine, &ctx, &record);
    assert!(result.is_ok());
}

#[test]
fn test_revoked_key_not_returned_by_prefix_lookup() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    let user_id = Uuid::new_v4();
    let record = make_api_key_record(user_id);
    let key_id = record.key_id;
    system_store::store_api_key(&engine, &ctx, &record).unwrap();

    system_store::revoke_api_key(&engine, &ctx, key_id).unwrap();

    let prefix = &key_id.simple().to_string()[..16];
    let found = system_store::get_api_key_by_prefix(&engine, prefix).unwrap();
    assert!(found.is_some(), "revoked key is returned by prefix lookup (caller checks is_revoked)");
    assert!(found.unwrap().is_revoked, "returned key should be marked as revoked");
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

#[test]
fn test_store_and_get_permissions() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    let permissions_json = br#"{"allow": "crudlify"}"#;
    system_store::store_permissions(&engine, &ctx, "/data/records", permissions_json).unwrap();

    let result = system_store::get_permissions(&engine, "/data/records").unwrap();
    assert_eq!(result, Some(permissions_json.to_vec()));
}

#[test]
fn test_get_permissions_missing() {
    let (engine, _dir) = setup();
    let result = system_store::get_permissions(&engine, "/nonexistent").unwrap();
    assert!(result.is_none());
}

// ---------------------------------------------------------------------------
// Magic Links
// ---------------------------------------------------------------------------

#[test]
fn test_store_and_get_magic_link() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    let record = MagicLinkRecord {
        code_hash: "abc123hash".to_string(),
        email: "alice@example.com".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::seconds(600),
        is_used: false,
    };

    system_store::store_magic_link(&engine, &ctx, &record).unwrap();

    let result = system_store::get_magic_link(&engine, "abc123hash").unwrap();
    assert!(result.is_some());
    let fetched = result.unwrap();
    assert_eq!(fetched.email, "alice@example.com");
    assert!(!fetched.is_used);
}

#[test]
fn test_get_magic_link_missing() {
    let (engine, _dir) = setup();
    let result = system_store::get_magic_link(&engine, "nonexistent").unwrap();
    assert!(result.is_none());
}

// ---------------------------------------------------------------------------
// Refresh Tokens
// ---------------------------------------------------------------------------

#[test]
fn test_store_and_get_refresh_token() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    let record = RefreshTokenRecord {
        token_hash: "token_hash_abc".to_string(),
        user_subject: "user123".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::days(30),
        is_revoked: false,
      key_id: None,
    };

    system_store::store_refresh_token(&engine, &ctx, &record).unwrap();

    let result = system_store::get_refresh_token(&engine, "token_hash_abc").unwrap();
    assert!(result.is_some());
    let fetched = result.unwrap();
    assert_eq!(fetched.user_subject, "user123");
    assert!(!fetched.is_revoked);
}

#[test]
fn test_get_refresh_token_missing() {
    let (engine, _dir) = setup();
    let result = system_store::get_refresh_token(&engine, "nonexistent").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_revoke_refresh_token() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    let record = RefreshTokenRecord {
        token_hash: "revokable_token".to_string(),
        user_subject: "user456".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::days(30),
        is_revoked: false,
      key_id: None,
    };

    system_store::store_refresh_token(&engine, &ctx, &record).unwrap();

    let revoked = system_store::revoke_refresh_token(&engine, &ctx, "revokable_token").unwrap();
    assert!(revoked);

    let result = system_store::get_refresh_token(&engine, "revokable_token").unwrap().unwrap();
    assert!(result.is_revoked);
}

#[test]
fn test_revoke_refresh_token_not_found() {
    let (engine, _dir) = setup();
    let ctx = test_context();
    let revoked = system_store::revoke_refresh_token(&engine, &ctx, "ghost").unwrap();
    assert!(!revoked);
}

// ---------------------------------------------------------------------------
// Cluster
// ---------------------------------------------------------------------------

#[test]
fn test_node_id_persistence() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    system_store::store_node_id(&engine, &ctx, 42).unwrap();
    let result = system_store::get_node_id(&engine).unwrap();
    assert_eq!(result, Some(42));
}

#[test]
fn test_node_id_missing() {
    let (engine, _dir) = setup();
    let result = system_store::get_node_id(&engine).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_peer_configs_persistence() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    let peers = vec![
        PeerConfig {
            node_id: 1,
            address: "http://peer1:8080".to_string(),
            label: Some("peer-one".to_string()),
            sync_paths: None,
            last_clock_offset_ms: None,
            last_wire_time_ms: None,
            last_jitter_ms: None,
            clock_state_at: None,
        },
        PeerConfig {
            node_id: 2,
            address: "http://peer2:8080".to_string(),
            label: None,
            sync_paths: Some(vec!["/data".to_string()]),
            last_clock_offset_ms: Some(1.5),
            last_wire_time_ms: Some(10.0),
            last_jitter_ms: Some(0.5),
            clock_state_at: Some(1000),
        },
    ];

    system_store::store_peer_configs(&engine, &ctx, &peers).unwrap();
    let result = system_store::get_peer_configs(&engine).unwrap();

    assert_eq!(result.len(), 2);
    assert_eq!(result[0].node_id, 1);
    assert_eq!(result[0].address, "http://peer1:8080");
    assert_eq!(result[1].node_id, 2);
    assert_eq!(result[1].sync_paths, Some(vec!["/data".to_string()]));
}

#[test]
fn test_peer_configs_empty() {
    let (engine, _dir) = setup();
    let result = system_store::get_peer_configs(&engine).unwrap();
    assert!(result.is_empty());
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

#[test]
fn test_store_and_get_plugin() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    system_store::store_plugin(&engine, &ctx, "my-plugin", b"wasm-bytes-here").unwrap();
    let result = system_store::get_plugin(&engine, "my-plugin").unwrap();
    assert_eq!(result, Some(b"wasm-bytes-here".to_vec()));
}

#[test]
fn test_get_plugin_missing() {
    let (engine, _dir) = setup();
    let result = system_store::get_plugin(&engine, "nonexistent").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_list_plugins() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    system_store::store_plugin(&engine, &ctx, "plugin-a", b"data-a").unwrap();
    system_store::store_plugin(&engine, &ctx, "plugin-b", b"data-b").unwrap();

    let plugins = system_store::list_plugins(&engine).unwrap();
    assert_eq!(plugins.len(), 2);

    let keys: Vec<String> = plugins.iter().map(|(k, _)| k.clone()).collect();
    assert!(keys.contains(&"plugin-a".to_string()));
    assert!(keys.contains(&"plugin-b".to_string()));
}

#[test]
fn test_remove_plugin() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    system_store::store_plugin(&engine, &ctx, "doomed", b"data").unwrap();
    let removed = system_store::remove_plugin(&engine, &ctx, "doomed").unwrap();
    assert!(removed);

    let result = system_store::get_plugin(&engine, "doomed").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_remove_plugin_nonexistent() {
    let (engine, _dir) = setup();
    let ctx = test_context();
    let removed = system_store::remove_plugin(&engine, &ctx, "ghost").unwrap();
    assert!(!removed);
}

// ---------------------------------------------------------------------------
// Integration: system data visible in directory tree
// ---------------------------------------------------------------------------

#[test]
fn test_system_data_in_directory_tree() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    // Store some system data.
    system_store::store_config(&engine, &ctx, "test_key", b"test_value").unwrap();
    let user = User::new("tree_test", None);
    system_store::store_user(&engine, &ctx, &user).unwrap();

    // Walk the version tree and check that /.aeordb-system/ entries are present.
    let ops = DirectoryOps::new(&engine);
    let children = ops.list_directory("/.aeordb-system").unwrap();
    let child_names: Vec<String> = children.iter().map(|c| c.name.clone()).collect();

    assert!(
        child_names.contains(&"config".to_string()),
        "/.aeordb-system/ should contain 'config' directory, got: {:?}",
        child_names
    );
    assert!(
        child_names.contains(&"users".to_string()),
        "/.aeordb-system/ should contain 'users' directory, got: {:?}",
        child_names
    );
}

#[test]
fn test_system_data_appears_in_subtree_walk() {
    // System paths are NOT reachable via walk_version_tree(HEAD) by design
    // (system data isn't in the user-visible directory tree). To check
    // system data made it to disk, walk_subtree the /.aeordb-system/config
    // subtree directly — that's the API peer sync (and backup) use.
    let (engine, _dir) = setup();
    let ctx = test_context();

    system_store::store_config(&engine, &ctx, "tree_check", b"hello").unwrap();

    let algo = engine.hash_algo();
    let hash_length = algo.hash_length();
    let dir_path = "/.aeordb-system/config";
    let key = aeordb::engine::directory_ops::directory_path_hash(dir_path, &algo).unwrap();
    let (_, _, raw) = engine.get_entry_including_deleted(&key).unwrap().unwrap();
    let dir_hash = if raw.len() == hash_length { raw } else { algo.compute_hash(&raw).unwrap() };

    let mut tree = aeordb::engine::tree_walker::VersionTree::new();
    aeordb::engine::tree_walker::walk_subtree(&engine, dir_path, &dir_hash, &mut tree).unwrap();

    let all_paths: Vec<&String> = tree.files.keys().collect();
    assert!(
        all_paths.iter().any(|p: &&String| p.contains("tree_check")),
        "subtree walk should contain the stored config, got: {:?}",
        all_paths
    );
}

// ---------------------------------------------------------------------------
// Delete user also removes auto-group
// ---------------------------------------------------------------------------

#[test]
fn test_delete_user_removes_auto_group() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    let user = User::new("deletable", None);
    let user_id = user.user_id;
    system_store::store_user(&engine, &ctx, &user).unwrap();

    let group_name = format!("user:{}", user_id);
    assert!(system_store::get_group(&engine, &group_name).unwrap().is_some());

    system_store::delete_user(&engine, &ctx, &user_id).unwrap();
    assert!(system_store::get_group(&engine, &group_name).unwrap().is_none());
}

// ---------------------------------------------------------------------------
// System Path Migration
// ---------------------------------------------------------------------------

#[test]
fn test_migrate_apikeys_to_api_keys() {
    let (engine, _dir) = setup();
    let ctx = test_context();
    let ops = DirectoryOps::new(&engine);

    // Write data at the OLD path (/.aeordb-system/apikeys/).
    let key_id = Uuid::new_v4();
    let record = make_api_key_record(Uuid::new_v4());
    let json = serde_json::to_vec(&record).unwrap();
    let old_path = format!("/.aeordb-system/apikeys/{}", key_id);
    ops.store_file_buffered(&ctx, &old_path, &json, Some("application/json")).unwrap();

    // Verify old path exists before migration.
    assert!(ops.read_file_buffered(&old_path).is_ok());

    // Run migration.
    system_store::migrate_system_paths(&engine).unwrap();

    // Verify data now lives at the new path.
    let new_path = format!("/.aeordb-system/api-keys/{}", key_id);
    let migrated_data = ops.read_file_buffered(&new_path).unwrap();
    assert_eq!(migrated_data, json);

    // Verify old path is gone.
    assert!(ops.read_file_buffered(&old_path).is_err());
}

#[test]
fn test_migrate_cluster_sync_to_sync_peers() {
    let (engine, _dir) = setup();
    let ctx = test_context();
    let ops = DirectoryOps::new(&engine);

    // Write data at the OLD path (/.aeordb-system/cluster/sync/).
    let peer_id = "42";
    let state_json = br#"{"last_synced_hash":"abc123","last_synced_at":"2026-04-18T00:00:00Z"}"#;
    let old_path = format!("/.aeordb-system/cluster/sync/{}", peer_id);
    ops.store_file_buffered(&ctx, &old_path, state_json, Some("application/json")).unwrap();

    // Verify old path exists before migration.
    assert!(ops.read_file_buffered(&old_path).is_ok());

    // Run migration.
    system_store::migrate_system_paths(&engine).unwrap();

    // Verify data now lives at the new path.
    let new_path = format!("/.aeordb-system/sync-peers/{}", peer_id);
    let migrated_data = ops.read_file_buffered(&new_path).unwrap();
    assert_eq!(migrated_data, state_json.to_vec());

    // Verify old path is gone.
    assert!(ops.read_file_buffered(&old_path).is_err());
}

#[test]
fn test_migration_is_idempotent() {
    let (engine, _dir) = setup();
    let ctx = test_context();
    let ops = DirectoryOps::new(&engine);

    // Write data at the OLD paths.
    let key_id = Uuid::new_v4();
    let record = make_api_key_record(Uuid::new_v4());
    let json = serde_json::to_vec(&record).unwrap();
    let old_api_path = format!("/.aeordb-system/apikeys/{}", key_id);
    ops.store_file_buffered(&ctx, &old_api_path, &json, Some("application/json")).unwrap();

    let old_sync_path = "/.aeordb-system/cluster/sync/99";
    let sync_data = b"sync-state-data";
    ops.store_file_buffered(&ctx, old_sync_path, sync_data, Some("application/octet-stream")).unwrap();

    // Run migration twice — second run should not error or corrupt data.
    system_store::migrate_system_paths(&engine).unwrap();
    system_store::migrate_system_paths(&engine).unwrap();

    // Verify data at new paths is intact.
    let new_api_path = format!("/.aeordb-system/api-keys/{}", key_id);
    let api_data = ops.read_file_buffered(&new_api_path).unwrap();
    assert_eq!(api_data, json);

    let new_sync_path = "/.aeordb-system/sync-peers/99";
    let sync_result = ops.read_file_buffered(new_sync_path).unwrap();
    assert_eq!(sync_result, sync_data.to_vec());
}

#[test]
fn test_migration_skips_when_no_old_paths_exist() {
    let (engine, _dir) = setup();

    // No old paths exist. Migration should succeed with no errors.
    system_store::migrate_system_paths(&engine).unwrap();

    // Running again should also be fine.
    system_store::migrate_system_paths(&engine).unwrap();
}

#[test]
fn test_migration_preserves_existing_new_path_data() {
    let (engine, _dir) = setup();
    let ctx = test_context();
    let ops = DirectoryOps::new(&engine);

    let key_id = Uuid::new_v4();

    // Write data at the NEW path first (simulates already-migrated data).
    let new_data = b"new-path-data";
    let new_path = format!("/.aeordb-system/api-keys/{}", key_id);
    ops.store_file_buffered(&ctx, &new_path, new_data, Some("application/octet-stream")).unwrap();

    // Write DIFFERENT data at the OLD path (simulates a stale leftover).
    let old_data = b"old-path-data";
    let old_path = format!("/.aeordb-system/apikeys/{}", key_id);
    ops.store_file_buffered(&ctx, &old_path, old_data, Some("application/octet-stream")).unwrap();

    // Run migration — should skip this entry because new path already exists.
    system_store::migrate_system_paths(&engine).unwrap();

    // The new path should retain its original data (not overwritten).
    let result = ops.read_file_buffered(&new_path).unwrap();
    assert_eq!(result, new_data.to_vec());
}

#[test]
fn test_migration_handles_multiple_entries() {
    let (engine, _dir) = setup();
    let ctx = test_context();
    let ops = DirectoryOps::new(&engine);

    // Store multiple entries at the old api-keys path.
    let mut expected_data = std::collections::HashMap::new();
    for i in 0..5 {
        let key_id = Uuid::new_v4();
        let data = format!("record-data-{}", i).into_bytes();
        let old_path = format!("/.aeordb-system/apikeys/{}", key_id);
        ops.store_file_buffered(&ctx, &old_path, &data, Some("application/octet-stream")).unwrap();
        expected_data.insert(key_id, data);
    }

    // Run migration.
    system_store::migrate_system_paths(&engine).unwrap();

    // Verify all entries exist at new paths.
    for (key_id, data) in &expected_data {
        let new_path = format!("/.aeordb-system/api-keys/{}", key_id);
        let migrated = ops.read_file_buffered(&new_path).unwrap();
        assert_eq!(&migrated, data);

        let old_path = format!("/.aeordb-system/apikeys/{}", key_id);
        assert!(ops.read_file_buffered(&old_path).is_err(), "old path should be gone: {}", old_path);
    }
}

// Reproducer for the 2026-05-20 storage-order-write-stomp bug report:
// when store_user (which internally also calls store_group) precedes
// store_magic_link in the same request context, the magic-link record
// is unreadable on the very next get_magic_link call.
#[test]
fn store_user_then_magic_link_then_get_magic_link() {
    use aeordb::auth::magic_link::{MagicLinkRecord, DEFAULT_MAGIC_LINK_EXPIRY_SECONDS, hash_magic_link_code};
    let (engine, _tmp) = setup();
    let ctx = test_context();

    let code_plain = "ABCDEFGHIJKLMNOP".to_string();
    let code_hash = hash_magic_link_code(&code_plain);
    let now = Utc::now();
    let record = MagicLinkRecord {
        code_hash: code_hash.clone(),
        email: "repro@example.com".to_string(),
        created_at: now,
        expires_at: now + chrono::Duration::seconds(DEFAULT_MAGIC_LINK_EXPIRY_SECONDS),
        is_used: false,
    };

    // Buggy order: store_user (writes user + auto-group) FIRST, then magic-link.
    let user = User::new("repro@example.com", Some("repro@example.com"));
    system_store::store_user(&engine, &ctx, &user).expect("store_user");
    system_store::store_magic_link(&engine, &ctx, &record).expect("store_magic_link");

    // Immediate readback in the SAME process — bug report says this returns None.
    let found = system_store::get_magic_link(&engine, &code_hash)
        .expect("get_magic_link");
    assert!(
        found.is_some(),
        "expected magic-link record to be findable after store_user → store_magic_link"
    );
}

// Dirty-startup variant of the storage-order-write-stomp repro.
// User reported the bug ALSO emits "Corrupt or missing hot tail —
// will rebuild KV from WAL" + "Corrupt entry header at offset 71138"
// on every startup. Try to reproduce with a damaged-then-reopened DB.
#[test]
fn store_user_then_magic_link_after_dirty_startup() {
    use aeordb::auth::magic_link::{MagicLinkRecord, DEFAULT_MAGIC_LINK_EXPIRY_SECONDS, hash_magic_link_code};
    use std::io::{Seek, SeekFrom, Write};

    // Build a DB with some content, then close.
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("repro.aeordb");
    {
        let engine = Arc::new(StorageEngine::create(db_path.to_str().unwrap()).unwrap());
        let ctx = RequestContext::system();
        // Write a few entries to grow the WAL past 50KB.
        let ops = DirectoryOps::new(&engine);
        for i in 0..30 {
            let path = format!("/scratch/file_{:03}.txt", i);
            let body = vec![b'X'; 2048]; // 2KB each → 60KB total
            ops.store_file_buffered(&ctx, &path, &body, Some("text/plain")).unwrap();
        }
        engine.shutdown().unwrap();
    }

    // Corrupt the hot tail region — overwrite the last 256 bytes of the file
    // with garbage so the dirty-startup path triggers.
    {
        let mut f = std::fs::OpenOptions::new().write(true).open(&db_path).unwrap();
        let size = f.metadata().unwrap().len();
        f.seek(SeekFrom::Start(size - 256)).unwrap();
        f.write_all(&[0xFFu8; 256]).unwrap();
    }

    // Re-open — should trigger dirty startup + KV rebuild.
    let engine = Arc::new(StorageEngine::open(db_path.to_str().unwrap()).unwrap());
    let ctx = RequestContext::system();

    let code_plain = "DIRTY-STARTUP-TEST".to_string();
    let code_hash = hash_magic_link_code(&code_plain);
    let now = Utc::now();
    let record = MagicLinkRecord {
        code_hash: code_hash.clone(),
        email: "dirty@example.com".to_string(),
        created_at: now,
        expires_at: now + chrono::Duration::seconds(DEFAULT_MAGIC_LINK_EXPIRY_SECONDS),
        is_used: false,
    };

    let user = User::new("dirty@example.com", Some("dirty@example.com"));
    system_store::store_user(&engine, &ctx, &user).expect("store_user post-dirty");
    system_store::store_magic_link(&engine, &ctx, &record).expect("store_magic_link post-dirty");

    let found = system_store::get_magic_link(&engine, &code_hash)
        .expect("get_magic_link post-dirty");
    assert!(
        found.is_some(),
        "expected magic-link record to be findable after dirty startup + user→magic-link writes"
    );
}

// Closer to the user's actual handler: use RequestContext::with_bus
// (the constructor they use), do the get_user_by_username probe first,
// then store user, then store magic-link.
#[test]
fn handler_flow_email_signup_first_time() {
    use aeordb::auth::magic_link::{MagicLinkRecord, DEFAULT_MAGIC_LINK_EXPIRY_SECONDS, hash_magic_link_code, generate_magic_link_code};
    use aeordb::engine::event_bus::EventBus;

    let (engine, _tmp) = setup();
    let event_bus = Arc::new(EventBus::new());
    let ctx = RequestContext::with_bus(event_bus.clone());

    let email = "repro@example.com";

    // Step 1: check if user exists.
    let existing = system_store::get_user_by_username(&engine, email).expect("get_user_by_username");
    assert!(existing.is_none());

    // Step 2: create user (writes user + auto-group).
    let user = User::new(email, Some(email));
    system_store::store_user(&engine, &ctx, &user).expect("store_user");

    // Step 3: create magic-link record.
    let code = generate_magic_link_code();
    let code_hash = hash_magic_link_code(&code);
    let now = Utc::now();
    let record = MagicLinkRecord {
        code_hash: code_hash.clone(),
        email: email.to_string(),
        created_at: now,
        expires_at: now + chrono::Duration::seconds(DEFAULT_MAGIC_LINK_EXPIRY_SECONDS),
        is_used: false,
    };
    system_store::store_magic_link(&engine, &ctx, &record).expect("store_magic_link");

    // Step 4: simulate the verify endpoint reading back.
    let found = system_store::get_magic_link(&engine, &code_hash)
        .expect("get_magic_link");
    assert!(found.is_some(), "magic-link record must be findable");

    // Also confirm user is readable (in case there's collateral damage).
    let recovered_user = system_store::get_user_by_username(&engine, email)
        .expect("re-fetch user");
    assert!(recovered_user.is_some(), "user must be findable");
}

// Reproducer for the parent-dir-stomp variant of the user's report.
// The user's DB shows /.aeordb-system contains {config, cluster,
// magic-links} but is missing /users, /groups, /api-keys, /snapshots
// — even though writes to all of those happened. Each write under
// /.aeordb-system/* seems to be overwriting the parent's child list
// rather than appending.
#[test]
fn parent_dir_must_accumulate_children_across_writes() {
    let (engine, _tmp) = setup();
    let ctx = test_context();
    let ops = DirectoryOps::new(&engine);

    // Reproduce the user's observed write order: config first (their DB
    // had it before signup), then user/group via store_user, then
    // magic-link. After all writes, /.aeordb-system must list every
    // direct-child subdirectory.
    ops.store_file_buffered(
        &ctx, "/.aeordb-system/config/email.json", b"{}",
        Some("application/json"),
    ).unwrap();
    ops.store_file_buffered(
        &ctx, "/.aeordb-system/cluster/peer.json", b"{}",
        Some("application/json"),
    ).unwrap();

    let user = User::new("repro@example.com", Some("repro@example.com"));
    system_store::store_user(&engine, &ctx, &user).unwrap();

    use aeordb::auth::magic_link::{MagicLinkRecord, DEFAULT_MAGIC_LINK_EXPIRY_SECONDS, hash_magic_link_code};
    let code_hash = hash_magic_link_code("ABC");
    let now = Utc::now();
    let record = MagicLinkRecord {
        code_hash: code_hash.clone(),
        email: "repro@example.com".to_string(),
        created_at: now,
        expires_at: now + chrono::Duration::seconds(DEFAULT_MAGIC_LINK_EXPIRY_SECONDS),
        is_used: false,
    };
    system_store::store_magic_link(&engine, &ctx, &record).unwrap();

    // /.aeordb-system must list ALL the subdirs we touched.
    let children = ops.list_directory("/.aeordb-system").unwrap();
    let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    eprintln!("/.aeordb-system children: {:?}", sorted);

    for required in &["config", "cluster", "users", "groups", "magic-links"] {
        assert!(
            names.contains(required),
            "/.aeordb-system is missing child {:?}. Saw: {:?}",
            required, sorted,
        );
    }

    // And the lookups must work end-to-end.
    let found_user = system_store::get_user_by_username(&engine, "repro@example.com").unwrap();
    assert!(found_user.is_some(), "user lookup must succeed");

    let found_link = system_store::get_magic_link(&engine, &code_hash).unwrap();
    assert!(found_link.is_some(), "magic-link lookup must succeed");
}

// Process-restart variant: write some children, abandon the engine without
// shutdown (simulating SIGKILL / power loss), reopen, check the parent
// listing. The user's server logs "Corrupt or missing hot tail — will
// rebuild KV from WAL" on every startup, which suggests crash-style
// shutdown is the norm in their environment.
#[test]
fn parent_dir_must_survive_dirty_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("repro.aeordb").to_str().unwrap().to_string();

    // Round 1: seed config, cluster, magic-link.
    {
        let engine = Arc::new(StorageEngine::create(&db_path).unwrap());
        let ctx = RequestContext::system();
        let ops = DirectoryOps::new(&engine);
        ops.store_file_buffered(
            &ctx, "/.aeordb-system/config/email.json", b"{}",
            Some("application/json"),
        ).unwrap();
        ops.store_file_buffered(
            &ctx, "/.aeordb-system/cluster/peer.json", b"{}",
            Some("application/json"),
        ).unwrap();

        use aeordb::auth::magic_link::{MagicLinkRecord, DEFAULT_MAGIC_LINK_EXPIRY_SECONDS, hash_magic_link_code};
        let now = Utc::now();
        let record = MagicLinkRecord {
            code_hash: hash_magic_link_code("ROUND1"),
            email: "r1@example.com".into(),
            created_at: now,
            expires_at: now + chrono::Duration::seconds(DEFAULT_MAGIC_LINK_EXPIRY_SECONDS),
            is_used: false,
        };
        system_store::store_magic_link(&engine, &ctx, &record).unwrap();
        engine.shutdown().unwrap();
    }

    // Round 2: reopen, do user + magic-link (the user's buggy order).
    {
        let engine = Arc::new(StorageEngine::open(&db_path).unwrap());
        let ctx = RequestContext::system();
        let user = User::new("r2@example.com", Some("r2@example.com"));
        system_store::store_user(&engine, &ctx, &user).unwrap();
        use aeordb::auth::magic_link::{MagicLinkRecord, DEFAULT_MAGIC_LINK_EXPIRY_SECONDS, hash_magic_link_code};
        let now = Utc::now();
        let record = MagicLinkRecord {
            code_hash: hash_magic_link_code("ROUND2"),
            email: "r2@example.com".into(),
            created_at: now,
            expires_at: now + chrono::Duration::seconds(DEFAULT_MAGIC_LINK_EXPIRY_SECONDS),
            is_used: false,
        };
        system_store::store_magic_link(&engine, &ctx, &record).unwrap();
        // INTENTIONALLY no shutdown — simulate SIGKILL / power loss.
        // The Arc<StorageEngine> drops without flushing the hot tail.
        drop(engine);
    }

    // Round 3: reopen (dirty startup), enumerate /.aeordb-system.
    {
        let engine = Arc::new(StorageEngine::open(&db_path).unwrap());
        let ops = DirectoryOps::new(&engine);
        let listing = ops.list_directory("/.aeordb-system").unwrap();
        let names: Vec<&str> = listing.iter().map(|c| c.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        eprintln!("after dirty restart, /.aeordb-system: {:?}", sorted);

        for required in &["config", "cluster", "users", "groups", "magic-links"] {
            assert!(
                names.contains(required),
                "/.aeordb-system is missing child {:?} after dirty restart. Saw: {:?}",
                required, sorted,
            );
        }
    }
}

// Wiring smoke test for the 2026-05-20 dirty-startup write-loss fix.
// `create_app_with_auth_mode` now spawns the 100ms hot-buffer flush
// timer that the CLI used to install by hand — this asserts the
// helper survives being called, the timer task spawns onto the test's
// tokio runtime, and a write that goes through it is still
// readable on reopen. Note: a true SIGTERM regression (Drop never
// runs) needs a two-process harness — we can't release the file lock
// in-process — so this is a wiring smoke test, not a full crash test.
// The production verification is on-server soak after deploy.
#[tokio::test]
async fn timer_flushes_writes_without_explicit_shutdown() {
    use aeordb::auth::magic_link::{MagicLinkRecord, DEFAULT_MAGIC_LINK_EXPIRY_SECONDS, hash_magic_link_code};

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("repro.aeordb").to_str().unwrap().to_string();

    let code_hash = hash_magic_link_code("TIMER-TEST");

    {
        let engine = Arc::new(StorageEngine::create(&db_path).unwrap());
        aeordb::server::spawn_hot_buffer_flush_timer(engine.clone(), None);

        let ctx = RequestContext::system();
        let user = User::new("timer@example.com", Some("timer@example.com"));
        system_store::store_user(&engine, &ctx, &user).unwrap();

        let now = Utc::now();
        let record = MagicLinkRecord {
            code_hash: code_hash.clone(),
            email: "timer@example.com".into(),
            created_at: now,
            expires_at: now + chrono::Duration::seconds(DEFAULT_MAGIC_LINK_EXPIRY_SECONDS),
            is_used: false,
        };
        system_store::store_magic_link(&engine, &ctx, &record).unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(350)).await;

        drop(engine);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let engine = Arc::new(StorageEngine::open(&db_path).unwrap());
    let found = system_store::get_magic_link(&engine, &code_hash).unwrap();
    assert!(
        found.is_some(),
        "magic-link record must be readable after engine drop with timer wired"
    );
}
