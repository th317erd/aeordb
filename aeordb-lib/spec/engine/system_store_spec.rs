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
use aeordb::engine::tree_walker::walk_version_tree;
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
        user_id,
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
    assert!(found.is_none(), "revoked key should not be returned by prefix lookup");
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

    // Walk the version tree and check that /.system/ entries are present.
    let ops = DirectoryOps::new(&engine);
    let children = ops.list_directory("/.system").unwrap();
    let child_names: Vec<String> = children.iter().map(|c| c.name.clone()).collect();

    assert!(
        child_names.contains(&"config".to_string()),
        "/.system/ should contain 'config' directory, got: {:?}",
        child_names
    );
    assert!(
        child_names.contains(&"users".to_string()),
        "/.system/ should contain 'users' directory, got: {:?}",
        child_names
    );
}

#[test]
fn test_system_data_appears_in_version_tree() {
    let (engine, _dir) = setup();
    let ctx = test_context();

    system_store::store_config(&engine, &ctx, "tree_check", b"hello").unwrap();

    let head = engine.head_hash().unwrap();
    let tree = walk_version_tree(&engine, &head).unwrap();
    let all_paths: Vec<&String> = tree.files.keys().collect();

    assert!(
        all_paths.iter().any(|p| p.contains("/.system/")),
        "version tree should contain /.system/ paths, got: {:?}",
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
