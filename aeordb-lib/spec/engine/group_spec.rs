use std::sync::Arc;

use aeordb::engine::{
  RequestContext,
  Group, StorageEngine, SystemTables, User, SAFE_QUERY_FIELDS,
};
use aeordb::server::create_temp_engine_for_tests;

fn setup() -> (Arc<StorageEngine>, tempfile::TempDir) {
  create_temp_engine_for_tests()
}

// ---------------------------------------------------------------------------
// Group entity tests
// ---------------------------------------------------------------------------

#[test]
fn test_create_group() {
  let group = Group::new("engineers", "crudli..", "........", "is_active", "eq", "true")
    .expect("should create group");
  assert_eq!(group.name, "engineers");
  assert_eq!(group.default_allow, "crudli..");
  assert_eq!(group.default_deny, "........");
  assert_eq!(group.query_field, "is_active");
  assert_eq!(group.query_operator, "eq");
  assert_eq!(group.query_value, "true");
}

#[test]
fn test_create_group_rejects_unsafe_query_field() {
  let result = Group::new("bad", "crudlify", "........", "username", "eq", "admin");
  assert!(result.is_err(), "username should be rejected as unsafe query field");

  let result = Group::new("bad2", "crudlify", "........", "email", "eq", "admin@example.com");
  assert!(result.is_err(), "email should be rejected as unsafe query field");
}

#[test]
fn test_create_group_accepts_safe_fields() {
  for field in SAFE_QUERY_FIELDS {
    let result = Group::new("test", "crudlify", "........", field, "eq", "value");
    assert!(result.is_ok(), "field '{}' should be accepted", field);
  }
}

#[test]
fn test_create_group_rejects_arbitrary_field() {
  let result = Group::new("bad", "crudlify", "........", "arbitrary_field", "eq", "value");
  assert!(result.is_err());
}

#[test]
fn test_group_serialize_deserialize() {
  let group = Group::new("roundtrip", "crudlify", "........", "is_active", "eq", "true")
    .expect("create group");
  let serialized = group.serialize();
  let deserialized = Group::deserialize(&serialized).expect("should deserialize");

  assert_eq!(deserialized.name, group.name);
  assert_eq!(deserialized.default_allow, group.default_allow);
  assert_eq!(deserialized.default_deny, group.default_deny);
  assert_eq!(deserialized.query_field, group.query_field);
  assert_eq!(deserialized.query_operator, group.query_operator);
  assert_eq!(deserialized.query_value, group.query_value);
  assert_eq!(deserialized.created_at, group.created_at);
  assert_eq!(deserialized.updated_at, group.updated_at);
}

#[test]
fn test_group_deserialize_invalid_data() {
  let result = Group::deserialize(b"not json at all");
  assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Membership evaluation tests
// ---------------------------------------------------------------------------

#[test]
fn test_evaluate_membership_eq() {
  let user = User::new("testuser", None);
  let group = Group::new(
    "specific_user",
    "crudlify",
    "........",
    "user_id",
    "eq",
    &user.user_id.to_string(),
  )
  .unwrap();

  assert!(group.evaluate_membership(&user));
}

#[test]
fn test_evaluate_membership_neq() {
  let user = User::new("testuser", None);
  let group = Group::new(
    "not_this_user",
    "crudlify",
    "........",
    "user_id",
    "neq",
    "00000000-0000-0000-0000-000000000099",
  )
  .unwrap();

  assert!(group.evaluate_membership(&user));
}

#[test]
fn test_evaluate_membership_in() {
  let user = User::new("intest", None);
  let user_id_string = user.user_id.to_string();
  let other_id = uuid::Uuid::new_v4().to_string();
  let value = format!("{},{}", user_id_string, other_id);

  let group = Group::new("in_group", "crudlify", "........", "user_id", "in", &value).unwrap();
  assert!(group.evaluate_membership(&user));
}

#[test]
fn test_evaluate_membership_in_with_spaces() {
  let user = User::new("intest2", None);
  let user_id_string = user.user_id.to_string();
  let other_id = uuid::Uuid::new_v4().to_string();
  let value = format!("{}, {}", other_id, user_id_string);

  let group = Group::new("in_group_spaces", "crudlify", "........", "user_id", "in", &value).unwrap();
  assert!(group.evaluate_membership(&user));
}

#[test]
fn test_evaluate_membership_lt() {
  let mut user = User::new("lttest", None);
  user.created_at = 1000;

  let group = Group::new(
    "early_users",
    "crudlify",
    "........",
    "created_at",
    "lt",
    "2000",
  )
  .unwrap();

  assert!(group.evaluate_membership(&user));
}

#[test]
fn test_evaluate_membership_gt() {
  let mut user = User::new("gttest", None);
  user.created_at = 3000;

  let group = Group::new(
    "late_users",
    "crudlify",
    "........",
    "created_at",
    "gt",
    "2000",
  )
  .unwrap();

  assert!(group.evaluate_membership(&user));
}

#[test]
fn test_evaluate_membership_no_match() {
  let user = User::new("nomatch", None);
  let group = Group::new(
    "exclusive",
    "crudlify",
    "........",
    "user_id",
    "eq",
    "00000000-0000-0000-0000-000000000099",
  )
  .unwrap();

  assert!(!group.evaluate_membership(&user));
}

#[test]
fn test_evaluate_membership_unknown_operator() {
  let user = User::new("unknownop", None);
  // Manually build a group with an unknown operator via deserialization.
  let group = Group {
    name: "bad_op".to_string(),
    default_allow: "crudlify".to_string(),
    default_deny: "........".to_string(),
    query_field: "is_active".to_string(),
    query_operator: "regex".to_string(),
    query_value: "true".to_string(),
    created_at: 0,
    updated_at: 0,
  };

  assert!(!group.evaluate_membership(&user));
}

#[test]
fn test_evaluate_membership_is_active_eq() {
  let user = User::new("active_test", None);
  assert!(user.is_active);

  let group = Group::new("active_users", "crudlify", "........", "is_active", "eq", "true")
    .unwrap();
  assert!(group.evaluate_membership(&user));

  let mut inactive_user = User::new("inactive", None);
  inactive_user.is_active = false;
  assert!(!group.evaluate_membership(&inactive_user));
}

#[test]
fn test_evaluate_membership_contains() {
  let user = User::new("containstest", None);
  let partial_id = &user.user_id.to_string()[..8];

  let group = Group::new(
    "contains_group",
    "crudlify",
    "........",
    "user_id",
    "contains",
    partial_id,
  )
  .unwrap();

  assert!(group.evaluate_membership(&user));
}

#[test]
fn test_evaluate_membership_starts_with() {
  let user = User::new("swtest", None);
  let prefix = &user.user_id.to_string()[..8];

  let group = Group::new(
    "sw_group",
    "crudlify",
    "........",
    "user_id",
    "starts_with",
    prefix,
  )
  .unwrap();

  assert!(group.evaluate_membership(&user));
}

#[test]
fn test_safe_query_fields_enforced() {
  // All safe fields should be accepted.
  for field in SAFE_QUERY_FIELDS {
    assert!(
      Group::new("test", "crudlify", "........", field, "eq", "v").is_ok(),
      "field '{}' should be safe",
      field
    );
  }

  // Mutable fields should be rejected.
  assert!(Group::new("test", "crudlify", "........", "username", "eq", "v").is_err());
  assert!(Group::new("test", "crudlify", "........", "email", "eq", "v").is_err());
  assert!(Group::new("test", "crudlify", "........", "password", "eq", "v").is_err());
}

// ---------------------------------------------------------------------------
// SystemTables group CRUD tests
// ---------------------------------------------------------------------------

#[test]
fn test_store_and_get_group() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();
  let system_tables = SystemTables::new(&engine);

  let group = Group::new("engineers", "crudli..", "........", "is_active", "eq", "true")
    .expect("create group");
  system_tables.store_group(&ctx, &group).expect("store group");

  let retrieved = system_tables
    .get_group("engineers")
    .expect("get group")
    .expect("group should exist");

  assert_eq!(retrieved.name, "engineers");
  assert_eq!(retrieved.query_field, "is_active");
}

#[test]
fn test_get_group_not_found() {
  let (engine, _temp_dir) = setup();
  let system_tables = SystemTables::new(&engine);

  let result = system_tables.get_group("nonexistent").expect("should not error");
  assert!(result.is_none());
}

#[test]
fn test_list_groups() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();
  let system_tables = SystemTables::new(&engine);

  let group_a = Group::new("alpha", "crudlify", "........", "is_active", "eq", "true").unwrap();
  let group_b = Group::new("beta", ".r..l...", "........", "is_active", "eq", "true").unwrap();
  system_tables.store_group(&ctx, &group_a).unwrap();
  system_tables.store_group(&ctx, &group_b).unwrap();

  let groups = system_tables.list_groups().expect("list groups");
  assert_eq!(groups.len(), 2);

  let names: Vec<String> = groups.iter().map(|g| g.name.clone()).collect();
  assert!(names.contains(&"alpha".to_string()));
  assert!(names.contains(&"beta".to_string()));
}

#[test]
fn test_list_groups_empty() {
  let (engine, _temp_dir) = setup();
  let system_tables = SystemTables::new(&engine);

  let groups = system_tables.list_groups().expect("list groups");
  assert!(groups.is_empty());
}

#[test]
fn test_update_group() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();
  let system_tables = SystemTables::new(&engine);

  let mut group = Group::new("mutable", "crudlify", "........", "is_active", "eq", "true").unwrap();
  system_tables.store_group(&ctx, &group).unwrap();

  group.default_allow = ".r..l...".to_string();
  group.updated_at = chrono::Utc::now().timestamp_millis();
  system_tables.update_group(&ctx, &group).expect("update group");

  let retrieved = system_tables
    .get_group("mutable")
    .unwrap()
    .expect("group should exist");
  assert_eq!(retrieved.default_allow, ".r..l...");
}

#[test]
fn test_delete_group() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();
  let system_tables = SystemTables::new(&engine);

  let group = Group::new("deleteme", "crudlify", "........", "is_active", "eq", "true").unwrap();
  system_tables.store_group(&ctx, &group).unwrap();

  system_tables.delete_group(&ctx, "deleteme").expect("delete group");

  let result = system_tables.get_group("deleteme").expect("should not error");
  assert!(result.is_none());
}

#[test]
fn test_delete_group_removes_from_list() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();
  let system_tables = SystemTables::new(&engine);

  let group = Group::new("listdelete", "crudlify", "........", "is_active", "eq", "true").unwrap();
  system_tables.store_group(&ctx, &group).unwrap();
  assert_eq!(system_tables.list_groups().unwrap().len(), 1);

  system_tables.delete_group(&ctx, "listdelete").unwrap();
  assert_eq!(system_tables.list_groups().unwrap().len(), 0);
}

#[test]
fn test_delete_nonexistent_group_does_not_error() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = setup();
  let system_tables = SystemTables::new(&engine);

  let result = system_tables.delete_group(&ctx, "does_not_exist");
  assert!(result.is_ok());
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[test]
fn test_group_with_empty_name() {
  let group = Group::new("", "crudlify", "........", "is_active", "eq", "true");
  assert!(group.is_ok(), "empty name should be allowed at this layer");
}

#[test]
fn test_group_in_operator_no_match() {
  let user = User::new("notinlist", None);
  let group = Group::new(
    "in_nomatch",
    "crudlify",
    "........",
    "user_id",
    "in",
    "aaa,bbb,ccc",
  )
  .unwrap();

  assert!(!group.evaluate_membership(&user));
}

#[test]
fn test_group_lt_boundary() {
  let mut user = User::new("boundary", None);
  user.created_at = 1000;

  // Exactly equal should not match lt.
  let group = Group::new("lt_boundary", "crudlify", "........", "created_at", "lt", "1000").unwrap();
  assert!(!group.evaluate_membership(&user));
}

#[test]
fn test_group_gt_boundary() {
  let mut user = User::new("boundary", None);
  user.created_at = 1000;

  // Exactly equal should not match gt.
  let group = Group::new("gt_boundary", "crudlify", "........", "created_at", "gt", "1000").unwrap();
  assert!(!group.evaluate_membership(&user));
}

#[test]
fn test_evaluate_membership_unknown_field() {
  // Build a group with a field that somehow got past validation (e.g., deserialized).
  let group = Group {
    name: "bad_field".to_string(),
    default_allow: "crudlify".to_string(),
    default_deny: "........".to_string(),
    query_field: "nonexistent_field".to_string(),
    query_operator: "eq".to_string(),
    query_value: "anything".to_string(),
    created_at: 0,
    updated_at: 0,
  };

  let user = User::new("test", None);
  // get_field returns "" for unknown fields, and "" != "anything"
  assert!(!group.evaluate_membership(&user));
}
