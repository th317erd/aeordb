# Users, Groups, and Permissions

**Parent:** [Master Plan](./master-plan.md)
**Status:** Designed, ready for implementation

---

## Core Design

- **Root = nil UUID**, hardcoded in engine. Not a database entity.
- **All groups are query groups.** Static membership is `user_id IN (...)`.
- **Permissions are `crudlify`** — 8 operations, tri-state flags, per-directory.
- **Default deny all.** Nothing accessible unless explicitly granted.
- **Immutable field whitelist** for group query security.

---

## Root Identity

Root is the engine runtime, not a database entity.

```rust
const ROOT_USER_ID: Uuid = Uuid::nil();  // 00000000-0000-0000-0000-000000000000

fn check_permission(user_id: &Uuid, ...) -> bool {
  if *user_id == ROOT_USER_ID { return true; }
  // ... normal permission resolution
}
```

### Nil UUID Protection

Enforced at the engine level — every code path goes through `store_user`/`store_api_key`:

```rust
/// SECURITY: This method MUST validate user_id. All user creation/modification
/// goes through this method — HTTP, WASM plugins, native plugins, batch ops.
fn store_user(&self, user: &User) -> Result<()> {
  validate_user_id(&user.user_id)?;
  // ... store
}

/// SECURITY: NEVER expose this method to any external interface.
/// Only called from bootstrap, never from HTTP/plugin/admin paths.
fn store_api_key_for_bootstrap(&self, record: &ApiKeyRecord) -> Result<()> {
  // Allows nil UUID — bootstrap only
  // ... store
}

fn store_api_key(&self, record: &ApiKeyRecord) -> Result<()> {
  validate_user_id(&record.user_id)?;
  // ... store
}

fn validate_user_id(user_id: &Uuid) -> Result<()> {
  if *user_id == ROOT_USER_ID {
    return Err(EngineError::ReservedUserId);
  }
  Ok(())
}
```

---

## Bootstrap

```
Server starts → count API keys → if zero:
  1. Generate API key linked to ROOT_USER_ID (nil UUID)
  2. Generate JWT signing key
  3. Store both via SystemTables (using store_api_key_for_bootstrap)
  4. Print API key once (shown only at first startup)
```

No user entity. No group entity. Root exists in the code, not the data.

### Emergency Reset

```
aeordb-cli emergency-reset --database /path/to/data.aeordb [--force]

1. Prompt y/n (unless --force)
2. Open .aeordb file directly (no server needed)
3. Find API key linked to nil UUID → revoke it
4. Generate new API key linked to nil UUID → store it
5. Print new key
6. Signing key unchanged (other users' JWTs still valid)
7. Encrypted data unchanged (vault keys unaffected)
```

---

## User Entity

```rust
struct User {
  user_id: Uuid,          // auto-generated, validated != nil
  username: String,
  email: Option<String>,
  is_active: bool,
  created_at: i64,        // UTC ms, immutable after creation
  updated_at: i64,        // UTC ms, system-managed
}
```

Stored at `::aeordb:user:{user_id}`.
Registry at `::aeordb:user:_registry` (list of all user_ids).
Username lookup at `::aeordb:user:_by_username:{username}`.

### Field Mutability

| Field | Mutable By | Safe for Group Queries |
|---|---|---|
| `user_id` | Nobody | Yes |
| `created_at` | Nobody | Yes |
| `updated_at` | System | Yes |
| `is_active` | Admin only | Yes |
| `username` | Admin only | No (could change to match a query) |
| `email` | Admin only | No (could change to match a query) |

Users CANNOT change their own immutable fields. Admin changes username/email on the user's behalf.

---

## API Key → User Link

```rust
struct ApiKeyRecord {
  key_id: Uuid,
  key_id_prefix: String,
  key_hash: String,
  user_id: Uuid,          // which user owns this key
  created_at: i64,
  is_revoked: bool,
}
```

No `roles` field. Groups replace roles entirely.

### Auth Flow

```
1. Client sends API key → POST /auth/token
2. Server looks up key → finds ApiKeyRecord → gets user_id
3. JWT created with sub: user_id (not key_id)
4. Client uses JWT for all requests
5. Server extracts user_id → resolves groups → checks permissions
```

Multiple keys can link to the same user. Revoking a key doesn't revoke the user. Revoking a user (is_active = false) effectively invalidates all their keys.

---

## Group Entity

ALL groups are query-based. No separate "static" vs "query" types.

```rust
struct Group {
  name: String,
  default_allow: String,      // "crudlify" dot/letter format, 8 chars
  default_deny: String,       // "........" = no opinion
  query_field: String,        // e.g., "user_id", "is_active"
  query_operator: String,     // "eq", "neq", "in", "lt", "gt", "contains", "starts_with"
  query_value: String,        // e.g., "abc-123", "true"
  created_at: i64,
  updated_at: i64,
}
```

Stored at `::aeordb:group:{name}`.
Registry at `::aeordb:group:_registry`.

### Group Examples

```
Per-user group:  { query_field: "user_id", query_operator: "eq", query_value: "abc-123" }
Static list:     { query_field: "user_id", query_operator: "in", query_value: "abc,def,ghi" }
Everyone:        { query_field: "is_active", query_operator: "eq", query_value: "true" }
Time-based:      { query_field: "created_at", query_operator: "lt", query_value: "1700000000000" }
```

### Query Security

Queries can ONLY reference immutable/admin-only fields:
- `user_id` — immutable
- `created_at` — immutable
- `updated_at` — system-managed
- `is_active` — admin-only

Queries referencing `username` or `email` are **rejected** at group creation time (prevents privilege escalation via self-modification).

### Membership Evaluation

```rust
fn evaluate_membership(user: &User, group: &Group) -> bool {
  let user_value = match group.query_field.as_str() {
    "user_id" => user.user_id.to_string(),
    "is_active" => user.is_active.to_string(),
    "created_at" => user.created_at.to_string(),
    "updated_at" => user.updated_at.to_string(),
    _ => return false,
  };

  match group.query_operator.as_str() {
    "eq" => user_value == group.query_value,
    "neq" => user_value != group.query_value,
    "contains" => user_value.contains(&group.query_value),
    "starts_with" => user_value.starts_with(&group.query_value),
    "in" => group.query_value.split(',').any(|v| v.trim() == user_value),
    "lt" => user_value < group.query_value,
    "gt" => user_value > group.query_value,
    _ => false,
  }
}
```

### Per-User Auto-Groups

When a user is created, automatically create a group `user:{user_id}`:

```json
{ "name": "user:abc-123", "query_field": "user_id", "query_operator": "eq", "query_value": "abc-123" }
```

This is the ownership mechanism — linking `user:abc-123` to a path with full permissions = ownership.

---

## Permissions

### crudlify Operations

| Position | Flag | Operation | HTTP Mapping |
|---|---|---|---|
| 0 | `c` | Create | PUT (new file) |
| 1 | `r` | Read | GET (file) |
| 2 | `u` | Update | PUT (overwrite) |
| 3 | `d` | Delete | DELETE |
| 4 | `l` | List | GET (directory) |
| 5 | `i` | Invoke | POST /_invoke |
| 6 | `f` | conFigure | PUT /.config, .permissions |
| 7 | `y` | deploY | PUT /.functions |

### Flag Format

Dot/letter, 8 characters. Canonical format:

```
"crudlify" = all operations allowed
"cr......"  = create and read only
"........"  = no opinion (empty)
".r..l..."  = read and list only
```

`.` = no opinion. Letter = allow. Both formats accepted on input.

### .permissions File

One per directory. Stored as a regular file via the engine. JSON format:

```json
{
  "links": [
    {
      "group": "engineers",
      "allow": "crudli..",
      "deny": "........"
    },
    {
      "group": "security_team",
      "allow": "crudlify",
      "deny": "........",
      "others_allow": "........",
      "others_deny": "crudlify"
    }
  ]
}
```

- `allow` / `deny`: flags for group MEMBERS (uses group defaults if omitted)
- `others_allow` / `others_deny`: flags for NON-MEMBERS (optional)

### Default: Deny All

When a path has NO `.permissions` file: deny everything. Only root (nil UUID) bypasses.

`--auth=false` disables the entire permission system for dev mode.

### Resolution Algorithm

Walk from root to target path. At each level, apply allow then deny:

```
running_state = [false; 8]  // start with everything denied

for level in path_levels(path):
  permissions = load_permissions(level)  // .permissions file, cached
  if permissions is None: continue       // no file = no change

  level_allow = [None; 8]
  level_deny = [None; 8]

  for link in permissions.links:
    is_member = user_groups.contains(link.group)

    if is_member:
      merge(level_allow, link.allow)     // union
      merge(level_deny, link.deny)       // union
    else if link.others_allow or link.others_deny:
      merge(level_allow, link.others_allow)
      merge(level_deny, link.others_deny)

  // Apply: allow adds, deny removes. Deny wins at same level.
  for i in 0..8:
    if level_allow[i] is set: state[i] = true
    if level_deny[i] is set:  state[i] = false

return state[operation.index()]
```

---

## Caching

### Group Cache

```rust
struct GroupCache {
  entries: HashMap<Uuid, CacheEntry>,
  ttl: Duration,  // default 60 seconds
}

struct CacheEntry {
  groups: Vec<String>,   // all group names this user belongs to
  fetched_at: Instant,
}
```

- **Populate:** Load all groups, evaluate membership for the user, cache result
- **Evict on user modification:** admin changes a user field → evict that user's cache
- **Evict on group query change:** admin changes any group's query → flush ALL cache entries
- **TTL expiry:** passive, 60 seconds default

### Permissions Cache

Cache `.permissions` files in memory:

```rust
struct PermissionsCache {
  entries: HashMap<String, (PathPermissions, Instant)>,  // path → (permissions, fetched_at)
  ttl: Duration,
}
```

Evict on `.permissions` file write at that path.

---

## Admin API Endpoints

```
POST   /admin/users                           → create user
GET    /admin/users                           → list users
GET    /admin/users/{user_id}                 → get user
PATCH  /admin/users/{user_id}                 → update user (admin-only fields)
DELETE /admin/users/{user_id}                 → deactivate user

POST   /admin/groups                          → create group
GET    /admin/groups                          → list groups
GET    /admin/groups/{name}                   → get group
PATCH  /admin/groups/{name}                   → update group query/flags
DELETE /admin/groups/{name}                   → delete group

PUT    /engine/{path}/.permissions            → set permissions for a path
GET    /engine/{path}/.permissions            → read permissions for a path
DELETE /engine/{path}/.permissions            → remove permissions (revert to deny-all)
```

All admin endpoints require authentication. User/group CRUD requires root or a user with `f` (configure) permission.

---

## Implementation Tasks

```
Task 1:  User entity (CRUD + SystemTables + registry + username lookup)
Task 2:  Group entity (all query-based, CRUD + SystemTables + registry)
Task 3:  Update ApiKeyRecord (add user_id, drop roles)
Task 4:  Update JWT (sub = user_id, nil UUID for root)
Task 5:  Root as nil UUID (engine bypass + validation at store_user/store_api_key)
Task 6:  Bootstrap (zero keys → root API key with nil UUID)
Task 7:  Per-user auto-groups (user:{user_id} created on user creation)
Task 8:  .permissions files (per-directory JSON, deny-all default)
Task 9:  Permission resolution (path walk + crudlify + membership evaluation)
Task 10: Group cache (user_id → all_groups, LRU + TTL + eviction)
Task 11: Permissions cache (.permissions file cache + eviction)
Task 12: Permission middleware (check every HTTP request, map op → crudlify)
Task 13: Admin endpoints (user CRUD, group CRUD)
Task 14: Emergency reset CLI command (with --force flag)
Task 15: Tests — unit + 7 real-world scenarios + security attack tests
```

---

## Test Plan

### Real-World Scenarios

1. **Single developer** — root creates one user, stores/reads/deletes files
2. **Small team** — admin/developer/viewer roles with group-based access
3. **Organization** — departments, hierarchical permissions, secrets area
4. **Secretary** — read-only access across everything
5. **Multi-tenant** — two tenants isolated by permissions on shared database
6. **Permission inheritance** — nested directories with overlapping grants/denies
7. **Security attacks** — nil UUID injection, privilege escalation, forgery

### Security Tests (Critical)

```
- test_nil_uuid_rejected_on_user_create
- test_nil_uuid_rejected_on_user_update
- test_nil_uuid_rejected_on_api_key_create
- test_nil_uuid_rejected_via_wasm_plugin
- test_nil_uuid_rejected_via_http_admin_endpoint
- test_nil_uuid_only_allowed_in_bootstrap
- test_store_user_rejects_nil_at_engine_level
- test_user_cannot_modify_own_immutable_fields
- test_query_group_rejects_mutable_field_query
- test_privilege_escalation_via_email_change_blocked
- test_root_bypasses_all_permissions
- test_default_deny_blocks_unauthenticated
- test_default_deny_blocks_authenticated_without_permissions
- test_deny_overrides_allow_at_same_level
- test_deeper_allow_overrides_shallower_deny
- test_others_flags_apply_to_non_members
- test_cache_eviction_on_user_modification
- test_cache_eviction_on_group_query_change
- test_emergency_reset_generates_new_key
- test_emergency_reset_revokes_old_key
```
