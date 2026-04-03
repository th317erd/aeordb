use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use aeordb::engine::{
  CrudlifyOp, DirectoryOps, GroupCache, PathPermissions, PermissionLink,
  PermissionResolver, PermissionsCache, StorageEngine, SystemTables,
  merge_flags, parse_crudlify_flags, path_levels,
};
use aeordb::engine::group::Group;
use aeordb::engine::user::{ROOT_USER_ID, User};
use aeordb::server::create_temp_engine_for_tests;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a fresh engine with a root directory.
fn test_engine() -> (Arc<StorageEngine>, tempfile::TempDir) {
  create_temp_engine_for_tests()
}

/// Create a test user in the system and return its user_id.
fn create_test_user(engine: &StorageEngine, username: &str) -> Uuid {
  let user = User::new(username, None);
  let user_id = user.user_id;
  let system_tables = SystemTables::new(engine);
  system_tables.store_user(&user).unwrap();
  user_id
}

/// Create a group by name with given query parameters.
fn create_test_group(
  engine: &StorageEngine,
  name: &str,
  query_field: &str,
  query_operator: &str,
  query_value: &str,
) {
  let group = Group::new(name, "........", "........", query_field, query_operator, query_value).unwrap();
  let system_tables = SystemTables::new(engine);
  system_tables.store_group(&group).unwrap();
}

/// Write a .permissions file at a given directory path.
fn write_permissions(engine: &StorageEngine, dir_path: &str, permissions: &PathPermissions) {
  let directory_ops = DirectoryOps::new(engine);
  let perm_path = if dir_path == "/" || dir_path.ends_with('/') {
    format!("{}.permissions", dir_path)
  } else {
    format!("{}/.permissions", dir_path)
  };
  let data = permissions.serialize();
  directory_ops.store_file(&perm_path, &data, Some("application/json")).unwrap();
}

/// Create a PermissionLink with member-only flags.
fn member_link(group: &str, allow: &str, deny: &str) -> PermissionLink {
  PermissionLink {
    group: group.to_string(),
    allow: allow.to_string(),
    deny: deny.to_string(),
    others_allow: None,
    others_deny: None,
  }
}

/// Create a PermissionLink with others flags.
fn link_with_others(
  group: &str,
  allow: &str,
  deny: &str,
  others_allow: &str,
  others_deny: &str,
) -> PermissionLink {
  PermissionLink {
    group: group.to_string(),
    allow: allow.to_string(),
    deny: deny.to_string(),
    others_allow: Some(others_allow.to_string()),
    others_deny: Some(others_deny.to_string()),
  }
}

// ---------------------------------------------------------------------------
// Task 8: parse_crudlify_flags
// ---------------------------------------------------------------------------

#[test]
fn test_parse_crudlify_flags_all_letters() {
  let flags = parse_crudlify_flags("crudlify");
  for index in 0..8 {
    assert_eq!(flags[index], Some(true), "Position {} should be Some(true)", index);
  }
}

#[test]
fn test_parse_crudlify_flags_all_dots() {
  let flags = parse_crudlify_flags("........");
  for index in 0..8 {
    assert_eq!(flags[index], None, "Position {} should be None", index);
  }
}

#[test]
fn test_parse_crudlify_flags_mixed() {
  let flags = parse_crudlify_flags("cr..l..y");
  assert_eq!(flags[0], Some(true)); // c
  assert_eq!(flags[1], Some(true)); // r
  assert_eq!(flags[2], None);       // u
  assert_eq!(flags[3], None);       // d
  assert_eq!(flags[4], Some(true)); // l
  assert_eq!(flags[5], None);       // i
  assert_eq!(flags[6], None);       // f
  assert_eq!(flags[7], Some(true)); // y
}

#[test]
fn test_parse_crudlify_flags_empty_string() {
  let flags = parse_crudlify_flags("");
  for flag in &flags {
    assert_eq!(*flag, None);
  }
}

#[test]
fn test_parse_crudlify_flags_wrong_letters() {
  // "xxxx...." should produce all None since the letters don't match positions
  let flags = parse_crudlify_flags("xxxx....");
  for flag in &flags {
    assert_eq!(*flag, None);
  }
}

#[test]
fn test_parse_crudlify_flags_partial() {
  let flags = parse_crudlify_flags("cr");
  assert_eq!(flags[0], Some(true)); // c
  assert_eq!(flags[1], Some(true)); // r
  for index in 2..8 {
    assert_eq!(flags[index], None);
  }
}

#[test]
fn test_parse_crudlify_read_only() {
  let flags = parse_crudlify_flags(".r..l...");
  assert_eq!(flags[0], None);
  assert_eq!(flags[1], Some(true)); // r
  assert_eq!(flags[2], None);
  assert_eq!(flags[3], None);
  assert_eq!(flags[4], Some(true)); // l
  assert_eq!(flags[5], None);
  assert_eq!(flags[6], None);
  assert_eq!(flags[7], None);
}

// ---------------------------------------------------------------------------
// Task 8: merge_flags
// ---------------------------------------------------------------------------

#[test]
fn test_merge_flags_basic_union() {
  let mut target = [None; 8];
  let source = [Some(true), None, Some(true), None, None, None, None, None];
  merge_flags(&mut target, &source);
  assert_eq!(target[0], Some(true));
  assert_eq!(target[1], None);
  assert_eq!(target[2], Some(true));
}

#[test]
fn test_merge_flags_preserves_existing() {
  let mut target = [Some(true), None, None, None, None, None, None, None];
  let source = [None, Some(true), None, None, None, None, None, None];
  merge_flags(&mut target, &source);
  assert_eq!(target[0], Some(true)); // Preserved from target
  assert_eq!(target[1], Some(true)); // Added from source
}

#[test]
fn test_merge_flags_all_none_no_change() {
  let mut target = [Some(true), None, Some(true), None, None, None, None, None];
  let source = [None; 8];
  merge_flags(&mut target, &source);
  assert_eq!(target[0], Some(true));
  assert_eq!(target[2], Some(true));
}

#[test]
fn test_merge_flags_full_union() {
  let mut target = [Some(true), None, None, None, Some(true), None, None, None];
  let source = [None, Some(true), None, None, None, Some(true), None, None];
  merge_flags(&mut target, &source);
  assert_eq!(target[0], Some(true));
  assert_eq!(target[1], Some(true));
  assert_eq!(target[4], Some(true));
  assert_eq!(target[5], Some(true));
}

// ---------------------------------------------------------------------------
// Task 8: PathPermissions serialize/deserialize
// ---------------------------------------------------------------------------

#[test]
fn test_path_permissions_serialize_deserialize() {
  let permissions = PathPermissions {
    links: vec![
      member_link("engineers", "crudli..", "........"),
      member_link("viewers", ".r..l...", "........"),
    ],
  };

  let bytes = permissions.serialize();
  let deserialized = PathPermissions::deserialize(&bytes).unwrap();
  assert_eq!(deserialized.links.len(), 2);
  assert_eq!(deserialized.links[0].group, "engineers");
  assert_eq!(deserialized.links[0].allow, "crudli..");
  assert_eq!(deserialized.links[1].group, "viewers");
  assert_eq!(deserialized.links[1].allow, ".r..l...");
}

#[test]
fn test_path_permissions_empty_links() {
  let permissions = PathPermissions { links: vec![] };
  let bytes = permissions.serialize();
  let deserialized = PathPermissions::deserialize(&bytes).unwrap();
  assert_eq!(deserialized.links.len(), 0);
}

#[test]
fn test_path_permissions_deserialize_invalid_json() {
  let result = PathPermissions::deserialize(b"not json");
  assert!(result.is_err());
}

#[test]
fn test_permission_link_with_others() {
  let permissions = PathPermissions {
    links: vec![
      link_with_others("security", "crudlify", "........", "........", "crudlify"),
    ],
  };

  let bytes = permissions.serialize();
  let deserialized = PathPermissions::deserialize(&bytes).unwrap();
  assert_eq!(deserialized.links[0].others_allow.as_deref(), Some("........"));
  assert_eq!(deserialized.links[0].others_deny.as_deref(), Some("crudlify"));
}

#[test]
fn test_permission_link_without_others_serializes_clean() {
  let permissions = PathPermissions {
    links: vec![
      member_link("team", "crudlify", "........"),
    ],
  };
  let bytes = permissions.serialize();
  let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
  // others_allow and others_deny should be absent (skip_serializing_if = None)
  assert!(json["links"][0].get("others_allow").is_none());
  assert!(json["links"][0].get("others_deny").is_none());
}

// ---------------------------------------------------------------------------
// Task 9: path_levels
// ---------------------------------------------------------------------------

#[test]
fn test_path_levels_root() {
  assert_eq!(path_levels("/"), vec!["/"]);
}

#[test]
fn test_path_levels_file_in_nested_dir() {
  let levels = path_levels("/myapp/users/alice.json");
  assert_eq!(levels, vec!["/", "/myapp", "/myapp/users"]);
}

#[test]
fn test_path_levels_directory_with_trailing_slash() {
  let levels = path_levels("/myapp/users/");
  assert_eq!(levels, vec!["/", "/myapp", "/myapp/users"]);
}

#[test]
fn test_path_levels_top_level_file() {
  let levels = path_levels("/file.json");
  assert_eq!(levels, vec!["/"]);
}

#[test]
fn test_path_levels_deeply_nested() {
  let levels = path_levels("/a/b/c/d/file.txt");
  assert_eq!(levels, vec!["/", "/a", "/a/b", "/a/b/c", "/a/b/c/d"]);
}

// ---------------------------------------------------------------------------
// Task 9: Permission resolution
// ---------------------------------------------------------------------------

#[test]
fn test_resolve_deny_all_default() {
  let (engine, _temp_dir) = test_engine();
  let user_id = create_test_user(&engine, "alice");

  let group_cache = GroupCache::new(Duration::from_secs(60));
  let permissions_cache = PermissionsCache::new(Duration::from_secs(60));
  let resolver = PermissionResolver::new(&engine, &group_cache, &permissions_cache);

  // No .permissions file anywhere -> deny all
  let allowed = resolver.check_permission(&user_id, "/myapp/data.json", CrudlifyOp::Read).unwrap();
  assert!(!allowed, "Default should be deny-all");
}

#[test]
fn test_resolve_root_bypasses_everything() {
  let (engine, _temp_dir) = test_engine();
  let group_cache = GroupCache::new(Duration::from_secs(60));
  let permissions_cache = PermissionsCache::new(Duration::from_secs(60));
  let resolver = PermissionResolver::new(&engine, &group_cache, &permissions_cache);

  // Root should bypass even with no .permissions files
  let allowed = resolver.check_permission(&ROOT_USER_ID, "/anything/at/all.txt", CrudlifyOp::Read).unwrap();
  assert!(allowed, "Root should bypass all permission checks");

  let allowed = resolver.check_permission(&ROOT_USER_ID, "/anything", CrudlifyOp::Delete).unwrap();
  assert!(allowed, "Root should bypass delete permission");

  let allowed = resolver.check_permission(&ROOT_USER_ID, "/", CrudlifyOp::Configure).unwrap();
  assert!(allowed, "Root should bypass configure permission");
}

#[test]
fn test_resolve_simple_allow() {
  let (engine, _temp_dir) = test_engine();
  let user_id = create_test_user(&engine, "bob");

  // Create a group that matches bob by user_id
  let user_group_name = format!("user:{}", user_id);
  // The auto-group was already created by store_user, so it exists.

  // Write .permissions at root granting read+list to user:bob
  let permissions = PathPermissions {
    links: vec![
      member_link(&user_group_name, ".r..l...", "........"),
    ],
  };
  write_permissions(&engine, "/", &permissions);

  let group_cache = GroupCache::new(Duration::from_secs(60));
  let permissions_cache = PermissionsCache::new(Duration::from_secs(60));
  let resolver = PermissionResolver::new(&engine, &group_cache, &permissions_cache);

  assert!(resolver.check_permission(&user_id, "/data.json", CrudlifyOp::Read).unwrap());
  assert!(resolver.check_permission(&user_id, "/data/", CrudlifyOp::List).unwrap());
  assert!(!resolver.check_permission(&user_id, "/data.json", CrudlifyOp::Create).unwrap());
  assert!(!resolver.check_permission(&user_id, "/data.json", CrudlifyOp::Delete).unwrap());
}

#[test]
fn test_resolve_deny_overrides_allow_same_level() {
  let (engine, _temp_dir) = test_engine();
  let user_id = create_test_user(&engine, "carol");
  let user_group = format!("user:{}", user_id);

  // Create a separate group that also matches carol (e.g., "everyone")
  create_test_group(&engine, "everyone", "is_active", "eq", "true");

  // At root: "everyone" allows crudlify, but user:carol denies delete
  let permissions = PathPermissions {
    links: vec![
      member_link("everyone", "crudlify", "........"),
      member_link(&user_group, "........", "...d...."),
    ],
  };
  write_permissions(&engine, "/", &permissions);

  let group_cache = GroupCache::new(Duration::from_secs(60));
  let permissions_cache = PermissionsCache::new(Duration::from_secs(60));
  let resolver = PermissionResolver::new(&engine, &group_cache, &permissions_cache);

  // Allow wins for read
  assert!(resolver.check_permission(&user_id, "/file.json", CrudlifyOp::Read).unwrap());
  // Deny overrides for delete
  assert!(!resolver.check_permission(&user_id, "/file.json", CrudlifyOp::Delete).unwrap());
  // Create is still allowed (no deny)
  assert!(resolver.check_permission(&user_id, "/file.json", CrudlifyOp::Create).unwrap());
}

#[test]
fn test_resolve_deeper_allow_overrides_shallower_deny() {
  let (engine, _temp_dir) = test_engine();
  let user_id = create_test_user(&engine, "dave");
  let user_group = format!("user:{}", user_id);

  // At root: deny everything
  let root_perms = PathPermissions {
    links: vec![
      member_link(&user_group, "........", "crudlify"),
    ],
  };
  write_permissions(&engine, "/", &root_perms);

  // At /myapp: allow read+list
  let app_perms = PathPermissions {
    links: vec![
      member_link(&user_group, ".r..l...", "........"),
    ],
  };
  write_permissions(&engine, "/myapp", &app_perms);

  let group_cache = GroupCache::new(Duration::from_secs(60));
  let permissions_cache = PermissionsCache::new(Duration::from_secs(60));
  let resolver = PermissionResolver::new(&engine, &group_cache, &permissions_cache);

  // Root denied everything, but /myapp re-allows read and list
  assert!(resolver.check_permission(&user_id, "/myapp/data.json", CrudlifyOp::Read).unwrap());
  assert!(resolver.check_permission(&user_id, "/myapp/", CrudlifyOp::List).unwrap());
  // Create is still denied (only read+list were re-allowed)
  assert!(!resolver.check_permission(&user_id, "/myapp/new.json", CrudlifyOp::Create).unwrap());
}

#[test]
fn test_resolve_others_flags() {
  let (engine, _temp_dir) = test_engine();
  let user_id = create_test_user(&engine, "eve");

  // Create a group "admins" that eve is NOT a member of
  create_test_group(&engine, "admins", "user_id", "eq", &Uuid::new_v4().to_string());

  // At root: admins get full access, others get read-only
  let permissions = PathPermissions {
    links: vec![
      link_with_others("admins", "crudlify", "........", ".r..l...", "........"),
    ],
  };
  write_permissions(&engine, "/", &permissions);

  let group_cache = GroupCache::new(Duration::from_secs(60));
  let permissions_cache = PermissionsCache::new(Duration::from_secs(60));
  let resolver = PermissionResolver::new(&engine, &group_cache, &permissions_cache);

  // Eve is not in admins, so others_allow applies: read+list only
  assert!(resolver.check_permission(&user_id, "/file.json", CrudlifyOp::Read).unwrap());
  assert!(resolver.check_permission(&user_id, "/dir/", CrudlifyOp::List).unwrap());
  assert!(!resolver.check_permission(&user_id, "/file.json", CrudlifyOp::Create).unwrap());
  assert!(!resolver.check_permission(&user_id, "/file.json", CrudlifyOp::Delete).unwrap());
}

#[test]
fn test_resolve_others_deny_flags() {
  let (engine, _temp_dir) = test_engine();
  let user_id = create_test_user(&engine, "frank");

  // Create a group that frank is NOT in
  create_test_group(&engine, "trusted", "user_id", "eq", &Uuid::new_v4().to_string());

  // At root: trusted members get full access, others are denied everything
  let permissions = PathPermissions {
    links: vec![
      link_with_others("trusted", "crudlify", "........", "........", "crudlify"),
    ],
  };
  write_permissions(&engine, "/", &permissions);

  let group_cache = GroupCache::new(Duration::from_secs(60));
  let permissions_cache = PermissionsCache::new(Duration::from_secs(60));
  let resolver = PermissionResolver::new(&engine, &group_cache, &permissions_cache);

  // Frank is not trusted, so others_deny applies
  assert!(!resolver.check_permission(&user_id, "/file.json", CrudlifyOp::Read).unwrap());
  assert!(!resolver.check_permission(&user_id, "/file.json", CrudlifyOp::Create).unwrap());
}

#[test]
fn test_resolve_multiple_groups_union() {
  let (engine, _temp_dir) = test_engine();
  let user_id = create_test_user(&engine, "grace");

  // Create two groups that grace is a member of
  create_test_group(&engine, "readers", "is_active", "eq", "true");
  create_test_group(&engine, "writers", "is_active", "eq", "true");

  // readers allow read+list, writers allow create+update
  let permissions = PathPermissions {
    links: vec![
      member_link("readers", ".r..l...", "........"),
      member_link("writers", "c.u.....", "........"),
    ],
  };
  write_permissions(&engine, "/", &permissions);

  let group_cache = GroupCache::new(Duration::from_secs(60));
  let permissions_cache = PermissionsCache::new(Duration::from_secs(60));
  let resolver = PermissionResolver::new(&engine, &group_cache, &permissions_cache);

  // Union of both groups: read, list, create, update all allowed
  assert!(resolver.check_permission(&user_id, "/data.json", CrudlifyOp::Read).unwrap());
  assert!(resolver.check_permission(&user_id, "/dir/", CrudlifyOp::List).unwrap());
  assert!(resolver.check_permission(&user_id, "/data.json", CrudlifyOp::Create).unwrap());
  assert!(resolver.check_permission(&user_id, "/data.json", CrudlifyOp::Update).unwrap());
  // Delete is still denied (neither group grants it)
  assert!(!resolver.check_permission(&user_id, "/data.json", CrudlifyOp::Delete).unwrap());
}

#[test]
fn test_resolve_nested_path_inheritance() {
  let (engine, _temp_dir) = test_engine();
  let user_id = create_test_user(&engine, "heidi");
  let user_group = format!("user:{}", user_id);

  // Root: allow read+list
  write_permissions(&engine, "/", &PathPermissions {
    links: vec![member_link(&user_group, ".r..l...", "........")],
  });

  // /app: allow create+update (inherits read+list from root)
  write_permissions(&engine, "/app", &PathPermissions {
    links: vec![member_link(&user_group, "c.u.....", "........")],
  });

  // /app/restricted: deny everything
  write_permissions(&engine, "/app/restricted", &PathPermissions {
    links: vec![member_link(&user_group, "........", "crudlify")],
  });

  let group_cache = GroupCache::new(Duration::from_secs(60));
  let permissions_cache = PermissionsCache::new(Duration::from_secs(60));
  let resolver = PermissionResolver::new(&engine, &group_cache, &permissions_cache);

  // At /app level: read+list (root) + create+update (/app)
  assert!(resolver.check_permission(&user_id, "/app/data.json", CrudlifyOp::Read).unwrap());
  assert!(resolver.check_permission(&user_id, "/app/data.json", CrudlifyOp::Create).unwrap());
  assert!(resolver.check_permission(&user_id, "/app/data.json", CrudlifyOp::Update).unwrap());

  // At /app/restricted level: all denied
  assert!(!resolver.check_permission(&user_id, "/app/restricted/secret.json", CrudlifyOp::Read).unwrap());
  assert!(!resolver.check_permission(&user_id, "/app/restricted/secret.json", CrudlifyOp::Create).unwrap());
}

#[test]
fn test_resolve_no_permissions_file_passes_through() {
  let (engine, _temp_dir) = test_engine();
  let user_id = create_test_user(&engine, "ivan");
  let user_group = format!("user:{}", user_id);

  // Only set permissions at root, skip /app level
  write_permissions(&engine, "/", &PathPermissions {
    links: vec![member_link(&user_group, ".r......", "........")],
  });

  let group_cache = GroupCache::new(Duration::from_secs(60));
  let permissions_cache = PermissionsCache::new(Duration::from_secs(60));
  let resolver = PermissionResolver::new(&engine, &group_cache, &permissions_cache);

  // No .permissions at /app means no change -- root's read grant still applies
  assert!(resolver.check_permission(&user_id, "/app/data.json", CrudlifyOp::Read).unwrap());
  assert!(!resolver.check_permission(&user_id, "/app/data.json", CrudlifyOp::Create).unwrap());
}

// ---------------------------------------------------------------------------
// Task 10: Group Cache
// ---------------------------------------------------------------------------

#[test]
fn test_group_cache_hit() {
  let (engine, _temp_dir) = test_engine();
  let user_id = create_test_user(&engine, "julia");

  let cache = GroupCache::new(Duration::from_secs(60));

  // First call loads from engine
  let groups_first = cache.get_groups(&user_id, &engine).unwrap();
  // Second call should hit cache (same result)
  let groups_second = cache.get_groups(&user_id, &engine).unwrap();

  assert_eq!(groups_first, groups_second);
  // User has auto-group "user:{user_id}"
  let expected_group = format!("user:{}", user_id);
  assert!(groups_first.contains(&expected_group));
}

#[test]
fn test_group_cache_miss_loads() {
  let (engine, _temp_dir) = test_engine();
  let user_id = create_test_user(&engine, "karl");

  // Create a group matching all active users
  create_test_group(&engine, "everyone", "is_active", "eq", "true");

  let cache = GroupCache::new(Duration::from_secs(60));
  let groups = cache.get_groups(&user_id, &engine).unwrap();

  let user_group = format!("user:{}", user_id);
  assert!(groups.contains(&user_group), "Should contain auto-group");
  assert!(groups.contains(&"everyone".to_string()), "Should contain 'everyone' group");
}

#[test]
fn test_group_cache_evict_user() {
  let (engine, _temp_dir) = test_engine();
  let user_id = create_test_user(&engine, "lily");

  let cache = GroupCache::new(Duration::from_secs(60));

  // Populate cache
  let groups_before = cache.get_groups(&user_id, &engine).unwrap();

  // Add a new group that matches lily
  create_test_group(&engine, "new_group", "is_active", "eq", "true");

  // Without eviction, cache still returns old result
  let groups_cached = cache.get_groups(&user_id, &engine).unwrap();
  assert_eq!(groups_before, groups_cached, "Cache should return stale data");

  // Evict and reload
  cache.evict_user(&user_id);
  let groups_after = cache.get_groups(&user_id, &engine).unwrap();
  assert!(groups_after.contains(&"new_group".to_string()), "Should see new group after eviction");
}

#[test]
fn test_group_cache_evict_all() {
  let (engine, _temp_dir) = test_engine();
  let user_id_a = create_test_user(&engine, "mike");
  let user_id_b = create_test_user(&engine, "nancy");

  let cache = GroupCache::new(Duration::from_secs(60));

  // Populate both
  cache.get_groups(&user_id_a, &engine).unwrap();
  cache.get_groups(&user_id_b, &engine).unwrap();

  // Add a new group
  create_test_group(&engine, "new_team", "is_active", "eq", "true");

  // Evict all
  cache.evict_all();

  // Both should now see the new group
  let groups_a = cache.get_groups(&user_id_a, &engine).unwrap();
  let groups_b = cache.get_groups(&user_id_b, &engine).unwrap();
  assert!(groups_a.contains(&"new_team".to_string()));
  assert!(groups_b.contains(&"new_team".to_string()));
}

#[test]
fn test_group_cache_nonexistent_user_returns_empty() {
  let (engine, _temp_dir) = test_engine();
  let fake_user_id = Uuid::new_v4();

  let cache = GroupCache::new(Duration::from_secs(60));
  let groups = cache.get_groups(&fake_user_id, &engine).unwrap();
  assert!(groups.is_empty(), "Nonexistent user should have no groups");
}

#[test]
fn test_group_cache_ttl_expiry() {
  let (engine, _temp_dir) = test_engine();
  let user_id = create_test_user(&engine, "oscar");

  // Use a very short TTL
  let cache = GroupCache::new(Duration::from_millis(1));

  // Populate
  cache.get_groups(&user_id, &engine).unwrap();

  // Wait for TTL to expire
  std::thread::sleep(Duration::from_millis(5));

  // Add a new group
  create_test_group(&engine, "ttl_test_group", "is_active", "eq", "true");

  // Should reload from engine (TTL expired)
  let groups = cache.get_groups(&user_id, &engine).unwrap();
  assert!(groups.contains(&"ttl_test_group".to_string()), "Should see new group after TTL expiry");
}

// ---------------------------------------------------------------------------
// Task 11: Permissions Cache
// ---------------------------------------------------------------------------

#[test]
fn test_permissions_cache_hit() {
  let (engine, _temp_dir) = test_engine();

  let permissions = PathPermissions {
    links: vec![member_link("team", "crudlify", "........")],
  };
  write_permissions(&engine, "/", &permissions);

  let cache = PermissionsCache::new(Duration::from_secs(60));

  // First call loads from engine
  let result_first = cache.get_permissions("/", &engine).unwrap();
  assert!(result_first.is_some());
  assert_eq!(result_first.as_ref().unwrap().links.len(), 1);

  // Second call should hit cache
  let result_second = cache.get_permissions("/", &engine).unwrap();
  assert!(result_second.is_some());
  assert_eq!(result_second.as_ref().unwrap().links[0].group, "team");
}

#[test]
fn test_permissions_cache_miss_returns_none() {
  let (engine, _temp_dir) = test_engine();
  let cache = PermissionsCache::new(Duration::from_secs(60));

  // No .permissions file at /nonexistent
  let result = cache.get_permissions("/nonexistent", &engine).unwrap();
  assert!(result.is_none(), "Should return None for nonexistent path");

  // Verify it caches the None (second call should also return None without hitting engine)
  let result_cached = cache.get_permissions("/nonexistent", &engine).unwrap();
  assert!(result_cached.is_none());
}

#[test]
fn test_permissions_cache_evict() {
  let (engine, _temp_dir) = test_engine();

  let permissions_v1 = PathPermissions {
    links: vec![member_link("team_v1", ".r......", "........")],
  };
  write_permissions(&engine, "/", &permissions_v1);

  let cache = PermissionsCache::new(Duration::from_secs(60));
  let result = cache.get_permissions("/", &engine).unwrap();
  assert_eq!(result.unwrap().links[0].group, "team_v1");

  // Write updated permissions
  let permissions_v2 = PathPermissions {
    links: vec![member_link("team_v2", "crudlify", "........")],
  };
  write_permissions(&engine, "/", &permissions_v2);

  // Without eviction, still returns stale
  let stale = cache.get_permissions("/", &engine).unwrap();
  assert_eq!(stale.unwrap().links[0].group, "team_v1");

  // Evict and reload
  cache.evict_path("/");
  let fresh = cache.get_permissions("/", &engine).unwrap();
  assert_eq!(fresh.unwrap().links[0].group, "team_v2");
}

#[test]
fn test_permissions_cache_evict_all() {
  let (engine, _temp_dir) = test_engine();

  write_permissions(&engine, "/", &PathPermissions {
    links: vec![member_link("root_team", "crudlify", "........")],
  });
  write_permissions(&engine, "/app", &PathPermissions {
    links: vec![member_link("app_team", ".r......", "........")],
  });

  let cache = PermissionsCache::new(Duration::from_secs(60));
  cache.get_permissions("/", &engine).unwrap();
  cache.get_permissions("/app", &engine).unwrap();

  cache.evict_all();

  // Both should reload on next access
  let root = cache.get_permissions("/", &engine).unwrap();
  let app = cache.get_permissions("/app", &engine).unwrap();
  assert!(root.is_some());
  assert!(app.is_some());
}

#[test]
fn test_permissions_cache_ttl_expiry() {
  let (engine, _temp_dir) = test_engine();

  write_permissions(&engine, "/", &PathPermissions {
    links: vec![member_link("original", ".r......", "........")],
  });

  let cache = PermissionsCache::new(Duration::from_millis(1));
  cache.get_permissions("/", &engine).unwrap();

  std::thread::sleep(Duration::from_millis(5));

  // Update permissions
  write_permissions(&engine, "/", &PathPermissions {
    links: vec![member_link("updated", "crudlify", "........")],
  });

  // TTL expired, should reload
  let result = cache.get_permissions("/", &engine).unwrap();
  assert_eq!(result.unwrap().links[0].group, "updated");
}

// ---------------------------------------------------------------------------
// Task 12: CrudlifyOp from HTTP method
// ---------------------------------------------------------------------------

#[test]
fn test_crudlify_op_from_http_method() {
  use axum::http::Method;
  use aeordb::auth::permission_middleware::http_to_crudlify;
  use aeordb::server::state::AppState;

  // We need an AppState to call http_to_crudlify. Build a minimal one.
  let (engine, _temp_dir) = test_engine();
  let jwt_manager = Arc::new(aeordb::auth::JwtManager::generate());
  let prometheus_handle = aeordb::metrics::initialize_metrics();
  let plugin_manager = Arc::new(aeordb::plugins::PluginManager::new(engine.clone()));
  let rate_limiter = Arc::new(aeordb::auth::RateLimiter::default_config());
  let group_cache = Arc::new(GroupCache::new(Duration::from_secs(60)));
  let permissions_cache = Arc::new(PermissionsCache::new(Duration::from_secs(60)));

  let auth_provider: Arc<dyn aeordb::auth::AuthProvider> = Arc::new(aeordb::auth::FileAuthProvider::new(engine.clone()));
  let state = AppState {
    jwt_manager,
    auth_provider,
    plugin_manager,
    rate_limiter,
    prometheus_handle,
    engine,
    group_cache,
    permissions_cache,
  };

  // GET file -> Read
  assert_eq!(http_to_crudlify(&Method::GET, "myapp/data.json", &state), CrudlifyOp::Read);
  // GET directory -> List
  assert_eq!(http_to_crudlify(&Method::GET, "myapp/data/", &state), CrudlifyOp::List);
  // DELETE -> Delete
  assert_eq!(http_to_crudlify(&Method::DELETE, "myapp/data.json", &state), CrudlifyOp::Delete);
  // HEAD -> Read
  assert_eq!(http_to_crudlify(&Method::HEAD, "myapp/data.json", &state), CrudlifyOp::Read);
  // PUT to .permissions -> Configure
  assert_eq!(http_to_crudlify(&Method::PUT, "myapp/.permissions", &state), CrudlifyOp::Configure);
  // PUT to .config -> Configure
  assert_eq!(http_to_crudlify(&Method::PUT, "myapp/.config", &state), CrudlifyOp::Configure);
  // PUT to .functions -> Deploy
  assert_eq!(http_to_crudlify(&Method::PUT, "myapp/.functions", &state), CrudlifyOp::Deploy);
  // PUT to new file -> Create (file doesn't exist)
  assert_eq!(http_to_crudlify(&Method::PUT, "myapp/newfile.json", &state), CrudlifyOp::Create);
  // POST to /_invoke -> Invoke
  assert_eq!(http_to_crudlify(&Method::POST, "myapp/func/_invoke", &state), CrudlifyOp::Invoke);
}

// ---------------------------------------------------------------------------
// Task 12: Permission middleware integration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_permission_middleware_allows_root() {
  use axum::body::Body;
  use axum::http::Request;
  use tower::ServiceExt;

  let (engine, _temp_dir) = test_engine();
  let jwt_manager = Arc::new(aeordb::auth::JwtManager::generate());
  let app = aeordb::server::create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone());

  // Create a token with the nil UUID (root)
  let now = chrono::Utc::now().timestamp();
  let claims = aeordb::auth::TokenClaims {
    sub: ROOT_USER_ID.to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + 3600,
    scope: None,
    permissions: None,
  };
  let token = jwt_manager.create_token(&claims).unwrap();
  let auth = format!("Bearer {}", token);

  // Store a file -- root should be allowed even with no .permissions
  let request = Request::builder()
    .method("PUT")
    .uri("/engine/root_test/file.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("root data"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), axum::http::StatusCode::CREATED);
}

#[tokio::test]
async fn test_permission_middleware_denies_without_permission() {
  use axum::body::Body;
  use axum::http::Request;
  use http_body_util::BodyExt;
  use tower::ServiceExt;

  let (engine, _temp_dir) = test_engine();
  let jwt_manager = Arc::new(aeordb::auth::JwtManager::generate());

  // Create a user
  let user_id = create_test_user(&engine, "restricted_user");

  let app = aeordb::server::create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone());

  // Create a token for this user
  let now = chrono::Utc::now().timestamp();
  let claims = aeordb::auth::TokenClaims {
    sub: user_id.to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + 3600,
    scope: None,
    permissions: None,
  };
  let token = jwt_manager.create_token(&claims).unwrap();
  let auth = format!("Bearer {}", token);

  // Try to store a file -- no .permissions anywhere, so default deny
  let request = Request::builder()
    .method("PUT")
    .uri("/engine/restricted/file.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("blocked data"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);

  // Verify the error message
  let body = response.into_body().collect().await.unwrap().to_bytes();
  let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
  assert_eq!(json["error"], "Permission denied");
}

#[tokio::test]
async fn test_permission_middleware_allows_with_permission() {
  use axum::body::Body;
  use axum::http::Request;
  use tower::ServiceExt;

  let (engine, _temp_dir) = test_engine();
  let jwt_manager = Arc::new(aeordb::auth::JwtManager::generate());

  // Create a user and set up permissions
  let user_id = create_test_user(&engine, "allowed_user");
  let user_group = format!("user:{}", user_id);

  // Write .permissions at root allowing this user full access
  write_permissions(&engine, "/", &PathPermissions {
    links: vec![member_link(&user_group, "crudlify", "........")],
  });

  let app = aeordb::server::create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone());

  // Create a token
  let now = chrono::Utc::now().timestamp();
  let claims = aeordb::auth::TokenClaims {
    sub: user_id.to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + 3600,
    scope: None,
    permissions: None,
  };
  let token = jwt_manager.create_token(&claims).unwrap();
  let auth = format!("Bearer {}", token);

  // Store a file -- should be allowed
  let request = Request::builder()
    .method("PUT")
    .uri("/engine/allowed/file.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("allowed data"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), axum::http::StatusCode::CREATED);
}

#[tokio::test]
async fn test_permission_middleware_skips_non_engine_routes() {
  use axum::body::Body;
  use axum::http::Request;
  use tower::ServiceExt;

  let (engine, _temp_dir) = test_engine();
  let jwt_manager = Arc::new(aeordb::auth::JwtManager::generate());
  let app = aeordb::server::create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone());

  // Health check should work without any permission
  let request = Request::builder()
    .method("GET")
    .uri("/admin/health")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), axum::http::StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Edge cases and error paths
// ---------------------------------------------------------------------------

#[test]
fn test_resolve_user_not_in_any_group() {
  let (engine, _temp_dir) = test_engine();
  let user_id = create_test_user(&engine, "orphan");

  // Create a group that this user is NOT in
  create_test_group(&engine, "exclusive", "user_id", "eq", &Uuid::new_v4().to_string());

  // Grant permissions only to the exclusive group
  write_permissions(&engine, "/", &PathPermissions {
    links: vec![member_link("exclusive", "crudlify", "........")],
  });

  let group_cache = GroupCache::new(Duration::from_secs(60));
  let permissions_cache = PermissionsCache::new(Duration::from_secs(60));
  let resolver = PermissionResolver::new(&engine, &group_cache, &permissions_cache);

  // orphan is not in the exclusive group, so no permissions
  // (their auto-group user:X exists but isn't linked in .permissions)
  assert!(!resolver.check_permission(&user_id, "/data.json", CrudlifyOp::Read).unwrap());
}

#[test]
fn test_resolve_empty_permissions_file() {
  let (engine, _temp_dir) = test_engine();
  let user_id = create_test_user(&engine, "empty_perm");

  // Write an empty links array
  write_permissions(&engine, "/", &PathPermissions { links: vec![] });

  let group_cache = GroupCache::new(Duration::from_secs(60));
  let permissions_cache = PermissionsCache::new(Duration::from_secs(60));
  let resolver = PermissionResolver::new(&engine, &group_cache, &permissions_cache);

  // Empty permissions = no changes from default deny
  assert!(!resolver.check_permission(&user_id, "/data.json", CrudlifyOp::Read).unwrap());
}

#[test]
fn test_resolve_all_eight_operations() {
  let (engine, _temp_dir) = test_engine();
  let user_id = create_test_user(&engine, "fullaccess");
  let user_group = format!("user:{}", user_id);

  write_permissions(&engine, "/", &PathPermissions {
    links: vec![member_link(&user_group, "crudlify", "........")],
  });

  let group_cache = GroupCache::new(Duration::from_secs(60));
  let permissions_cache = PermissionsCache::new(Duration::from_secs(60));
  let resolver = PermissionResolver::new(&engine, &group_cache, &permissions_cache);

  let operations = [
    CrudlifyOp::Create,
    CrudlifyOp::Read,
    CrudlifyOp::Update,
    CrudlifyOp::Delete,
    CrudlifyOp::List,
    CrudlifyOp::Invoke,
    CrudlifyOp::Configure,
    CrudlifyOp::Deploy,
  ];

  for op in &operations {
    assert!(
      resolver.check_permission(&user_id, "/data.json", *op).unwrap(),
      "Operation {:?} should be allowed with full crudlify",
      op
    );
  }
}

#[test]
fn test_resolve_deny_specific_operations() {
  let (engine, _temp_dir) = test_engine();
  let user_id = create_test_user(&engine, "selective");
  let user_group = format!("user:{}", user_id);

  // Allow everything, then deny configure and deploy
  write_permissions(&engine, "/", &PathPermissions {
    links: vec![member_link(&user_group, "crudlify", "......fy")],
  });

  let group_cache = GroupCache::new(Duration::from_secs(60));
  let permissions_cache = PermissionsCache::new(Duration::from_secs(60));
  let resolver = PermissionResolver::new(&engine, &group_cache, &permissions_cache);

  assert!(resolver.check_permission(&user_id, "/data.json", CrudlifyOp::Read).unwrap());
  assert!(resolver.check_permission(&user_id, "/data.json", CrudlifyOp::Create).unwrap());
  assert!(!resolver.check_permission(&user_id, "/data.json", CrudlifyOp::Configure).unwrap());
  assert!(!resolver.check_permission(&user_id, "/data.json", CrudlifyOp::Deploy).unwrap());
}

#[test]
fn test_resolve_multiple_levels_accumulate() {
  let (engine, _temp_dir) = test_engine();
  let user_id = create_test_user(&engine, "multilevel");
  let user_group = format!("user:{}", user_id);

  // Root: allow read
  write_permissions(&engine, "/", &PathPermissions {
    links: vec![member_link(&user_group, ".r......", "........")],
  });

  // /a: allow create
  write_permissions(&engine, "/a", &PathPermissions {
    links: vec![member_link(&user_group, "c.......", "........")],
  });

  // /a/b: allow update
  write_permissions(&engine, "/a/b", &PathPermissions {
    links: vec![member_link(&user_group, "..u.....", "........")],
  });

  let group_cache = GroupCache::new(Duration::from_secs(60));
  let permissions_cache = PermissionsCache::new(Duration::from_secs(60));
  let resolver = PermissionResolver::new(&engine, &group_cache, &permissions_cache);

  // At /a/b/file.txt: read (root) + create (/a) + update (/a/b)
  assert!(resolver.check_permission(&user_id, "/a/b/file.txt", CrudlifyOp::Read).unwrap());
  assert!(resolver.check_permission(&user_id, "/a/b/file.txt", CrudlifyOp::Create).unwrap());
  assert!(resolver.check_permission(&user_id, "/a/b/file.txt", CrudlifyOp::Update).unwrap());
  assert!(!resolver.check_permission(&user_id, "/a/b/file.txt", CrudlifyOp::Delete).unwrap());
}
