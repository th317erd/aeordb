use std::sync::Arc;

use aeordb::engine::{
  RequestContext,
  StorageEngine, User, ROOT_USER_ID,
  validate_user_id, is_root, SAFE_QUERY_FIELDS,
};
use aeordb::engine::system_store;
use aeordb::server::create_temp_engine_for_tests;

fn setup() -> (Arc<StorageEngine>, tempfile::TempDir) {
  create_temp_engine_for_tests()
}

// ---------------------------------------------------------------------------
// User entity tests
// ---------------------------------------------------------------------------

#[test]
fn test_create_user() {
  let user = User::new("alice", Some("alice@example.com"));
  assert_eq!(user.username, "alice");
  assert_eq!(user.email, Some("alice@example.com".to_string()));
  assert!(user.is_active);
}

#[test]
fn test_create_user_auto_generates_uuid() {
  let user_a = User::new("alice", None);
  let user_b = User::new("bob", None);
  assert_ne!(user_a.user_id, user_b.user_id);
  assert_ne!(user_a.user_id, uuid::Uuid::nil());
  assert_ne!(user_b.user_id, uuid::Uuid::nil());
}

#[test]
fn test_create_user_sets_timestamps() {
  let before = chrono::Utc::now().timestamp_millis();
  let user = User::new("timestamped", None);
  let after = chrono::Utc::now().timestamp_millis();

  assert!(user.created_at >= before);
  assert!(user.created_at <= after);
  assert!(user.updated_at >= before);
  assert!(user.updated_at <= after);
}

#[test]
fn test_user_serialize_deserialize() {
  let user = User::new("serialize_test", Some("test@example.com"));
  let serialized = user.serialize();
  let deserialized = User::deserialize(&serialized).expect("should deserialize");

  assert_eq!(deserialized.user_id, user.user_id);
  assert_eq!(deserialized.username, user.username);
  assert_eq!(deserialized.email, user.email);
  assert_eq!(deserialized.is_active, user.is_active);
  assert_eq!(deserialized.created_at, user.created_at);
  assert_eq!(deserialized.updated_at, user.updated_at);
}

#[test]
fn test_user_deserialize_invalid_data() {
  let result = User::deserialize(b"not json");
  assert!(result.is_err());
}

#[test]
fn test_user_get_field() {
  let user = User::new("fieldtest", Some("field@example.com"));

  assert_eq!(user.get_field("user_id"), user.user_id.to_string());
  assert_eq!(user.get_field("username"), "fieldtest");
  assert_eq!(user.get_field("email"), "field@example.com");
  assert_eq!(user.get_field("is_active"), "true");
  assert_eq!(user.get_field("created_at"), user.created_at.to_string());
  assert_eq!(user.get_field("updated_at"), user.updated_at.to_string());
  assert_eq!(user.get_field("nonexistent"), "");
}

#[test]
fn test_user_get_field_email_none() {
  let user = User::new("noemail", None);
  assert_eq!(user.get_field("email"), "");
}

// ---------------------------------------------------------------------------
// Root user identity tests
// ---------------------------------------------------------------------------

#[test]
fn test_root_user_id_is_nil() {
  assert_eq!(ROOT_USER_ID, uuid::Uuid::nil());
}

#[test]
fn test_validate_user_id_rejects_nil() {
  let result = validate_user_id(&uuid::Uuid::nil());
  assert!(result.is_err());
}

#[test]
fn test_validate_user_id_accepts_random() {
  let result = validate_user_id(&uuid::Uuid::new_v4());
  assert!(result.is_ok());
}

#[test]
fn test_is_root_nil_uuid() {
  assert!(is_root(&uuid::Uuid::nil()));
}

#[test]
fn test_is_root_random_uuid() {
  assert!(!is_root(&uuid::Uuid::new_v4()));
}

#[test]
fn test_safe_query_fields() {
  assert!(SAFE_QUERY_FIELDS.contains(&"user_id"));
  assert!(SAFE_QUERY_FIELDS.contains(&"created_at"));
  assert!(SAFE_QUERY_FIELDS.contains(&"updated_at"));
  assert!(SAFE_QUERY_FIELDS.contains(&"is_active"));
  assert!(!SAFE_QUERY_FIELDS.contains(&"username"));
  assert!(!SAFE_QUERY_FIELDS.contains(&"email"));
}

// ---------------------------------------------------------------------------
// system_store user CRUD tests
// ---------------------------------------------------------------------------

#[test]
fn test_store_and_get_user() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();

  let user = User::new("alice", Some("alice@example.com"));
  system_store::store_user(&engine, &ctx, &user).expect("store user");

  let retrieved = system_store::get_user(&engine, &user.user_id)
    .expect("get user")
    .expect("user should exist");

  assert_eq!(retrieved.user_id, user.user_id);
  assert_eq!(retrieved.username, "alice");
  assert_eq!(retrieved.email, Some("alice@example.com".to_string()));
}

#[test]
fn test_get_user_not_found() {
  let (engine, _temp_dir) = setup();

  let result = system_store::get_user(&engine, &uuid::Uuid::new_v4())
    .expect("get user should not error");
  assert!(result.is_none());
}

#[test]
fn test_get_user_by_username() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();

  let user = User::new("lookup_user", Some("lookup@example.com"));
  system_store::store_user(&engine, &ctx, &user).expect("store user");

  let retrieved = system_store::get_user_by_username(&engine, "lookup_user")
    .expect("get by username")
    .expect("user should exist");

  assert_eq!(retrieved.user_id, user.user_id);
  assert_eq!(retrieved.username, "lookup_user");
}

#[test]
fn test_get_user_by_username_not_found() {
  let (engine, _temp_dir) = setup();

  let result = system_store::get_user_by_username(&engine, "nonexistent")
    .expect("should not error");
  assert!(result.is_none());
}

#[test]
fn test_list_users() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();

  let user_a = User::new("user_a", None);
  let user_b = User::new("user_b", None);
  system_store::store_user(&engine, &ctx, &user_a).expect("store user a");
  system_store::store_user(&engine, &ctx, &user_b).expect("store user b");

  let users = system_store::list_users(&engine).expect("list users");
  assert_eq!(users.len(), 2);

  let usernames: Vec<String> = users.iter().map(|u| u.username.clone()).collect();
  assert!(usernames.contains(&"user_a".to_string()));
  assert!(usernames.contains(&"user_b".to_string()));
}

#[test]
fn test_list_users_empty() {
  let (engine, _temp_dir) = setup();

  let users = system_store::list_users(&engine).expect("list users");
  assert!(users.is_empty());
}

#[test]
fn test_update_user() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();

  let mut user = User::new("original", Some("original@example.com"));
  system_store::store_user(&engine, &ctx, &user).expect("store user");

  user.username = "updated".to_string();
  user.email = Some("updated@example.com".to_string());
  user.updated_at = chrono::Utc::now().timestamp_millis();
  system_store::update_user(&engine, &ctx, &user).expect("update user");

  let retrieved = system_store::get_user(&engine, &user.user_id)
    .expect("get user")
    .expect("user should exist");

  assert_eq!(retrieved.username, "updated");
  assert_eq!(retrieved.email, Some("updated@example.com".to_string()));
}

#[test]
fn test_delete_user() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();

  let user = User::new("deleteme", None);
  system_store::store_user(&engine, &ctx, &user).expect("store user");

  system_store::delete_user(&engine, &ctx, &user.user_id).expect("delete user");

  let result = system_store::get_user(&engine, &user.user_id)
    .expect("get user should not error");
  assert!(result.is_none());
}

#[test]
fn test_delete_user_removes_from_list() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();

  let user = User::new("listdelete", None);
  system_store::store_user(&engine, &ctx, &user).expect("store user");
  assert_eq!(system_store::list_users(&engine).unwrap().len(), 1);

  system_store::delete_user(&engine, &ctx, &user.user_id).expect("delete user");
  assert_eq!(system_store::list_users(&engine).unwrap().len(), 0);
}

#[test]
fn test_delete_user_removes_username_lookup() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();

  let user = User::new("lookupdelete", None);
  system_store::store_user(&engine, &ctx, &user).expect("store user");

  system_store::delete_user(&engine, &ctx, &user.user_id).expect("delete user");

  let result = system_store::get_user_by_username(&engine, "lookupdelete")
    .expect("should not error");
  assert!(result.is_none());
}

#[test]
fn test_nil_uuid_rejected_on_store() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();

  let mut user = User::new("root_impersonator", None);
  user.user_id = uuid::Uuid::nil();

  let result = system_store::store_user(&engine, &ctx, &user);
  assert!(result.is_err(), "nil UUID should be rejected on store");
}

#[test]
fn test_nil_uuid_rejected_on_update() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();

  let mut user = User::new("root_impersonator", None);
  user.user_id = uuid::Uuid::nil();

  let result = system_store::update_user(&engine, &ctx, &user);
  assert!(result.is_err(), "nil UUID should be rejected on update");
}

#[test]
fn test_count_users() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();

  assert_eq!(system_store::count_users(&engine).unwrap(), 0);

  let user_a = User::new("count_a", None);
  let user_b = User::new("count_b", None);
  system_store::store_user(&engine, &ctx, &user_a).expect("store user a");
  system_store::store_user(&engine, &ctx, &user_b).expect("store user b");

  assert_eq!(system_store::count_users(&engine).unwrap(), 2);

  system_store::delete_user(&engine, &ctx, &user_a.user_id).expect("delete user a");
  assert_eq!(system_store::count_users(&engine).unwrap(), 1);
}

// ---------------------------------------------------------------------------
// Auto-group tests
// ---------------------------------------------------------------------------

#[test]
fn test_auto_group_created_on_user_creation() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();

  let user = User::new("autogroup_user", None);
  system_store::store_user(&engine, &ctx, &user).expect("store user");

  let group_name = format!("user:{}", user.user_id);
  let group = system_store::get_group(&engine, &group_name)
    .expect("get group")
    .expect("auto-group should exist");

  assert_eq!(group.name, group_name);
  assert_eq!(group.query_field, "user_id");
  assert_eq!(group.query_operator, "eq");
  assert_eq!(group.query_value, user.user_id.to_string());
  assert_eq!(group.default_allow, "crudlify");
  assert_eq!(group.default_deny, "........");
}

#[test]
fn test_auto_group_deleted_on_user_deletion() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();

  let user = User::new("autogroup_delete", None);
  system_store::store_user(&engine, &ctx, &user).expect("store user");

  let group_name = format!("user:{}", user.user_id);
  assert!(
    system_store::get_group(&engine, &group_name).unwrap().is_some(),
    "auto-group should exist before deletion"
  );

  system_store::delete_user(&engine, &ctx, &user.user_id).expect("delete user");

  assert!(
    system_store::get_group(&engine, &group_name).unwrap().is_none(),
    "auto-group should be deleted when user is deleted"
  );
}

#[test]
fn test_auto_group_membership() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();

  let user = User::new("membership_test", None);
  system_store::store_user(&engine, &ctx, &user).expect("store user");

  let group_name = format!("user:{}", user.user_id);
  let group = system_store::get_group(&engine, &group_name)
    .unwrap()
    .expect("auto-group should exist");

  // The user should be a member of their own auto-group.
  assert!(group.evaluate_membership(&user));

  // A different user should NOT be a member.
  let other_user = User::new("other", None);
  assert!(!group.evaluate_membership(&other_user));
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[test]
fn test_store_user_with_no_email() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();

  let user = User::new("noemail", None);
  system_store::store_user(&engine, &ctx, &user).expect("store user");

  let retrieved = system_store::get_user(&engine, &user.user_id)
    .unwrap()
    .expect("user should exist");
  assert_eq!(retrieved.email, None);
}

#[test]
fn test_store_duplicate_user_overwrites() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();

  let user = User::new("dup_test", None);
  system_store::store_user(&engine, &ctx, &user).expect("store first time");
  system_store::store_user(&engine, &ctx, &user).expect("store second time");

  // Should still count as 1 user (registry deduplication).
  assert_eq!(system_store::count_users(&engine).unwrap(), 1);
}

#[test]
fn test_delete_nonexistent_user_does_not_error() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();

  let result = system_store::delete_user(&engine, &ctx, &uuid::Uuid::new_v4());
  assert!(result.is_ok());
}
