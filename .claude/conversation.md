# AeorDB — Users, Groups, Permissions Implementation

Planning the implementation of users, groups, group-path links, and crudlify permission resolution.

---

## Round 1: Design

### Bootstrap: Zero Users = Create Root

No "first startup" flag. No special markers. Just a count check:

```
Server starts → count users → if zero:
  1. Create root user (user_id: generated UUID)
  2. Create root group "root" (bypasses all permission checks)
  3. Add root user to root group
  4. Generate API key, link to root user
  5. Generate signing key
  6. Print API key once
```

If users > 0: boot normally. `bootstrap_root_key` becomes `bootstrap_root_user`.

### User Entity

```rust
struct User {
  user_id: Uuid,
  username: String,
  email: Option<String>,
  groups: Vec<String>,      // group names this user belongs to
  created_at: i64,          // UTC ms
  updated_at: i64,          // UTC ms
  is_active: bool,          // can be disabled without deleting
}
```

Stored via SystemTables at `::aeordb:user:{user_id}`.

User listing/lookup also needs a registry (like API keys):
- `::aeordb:user:_registry` → list of all user_ids
- `::aeordb:user:_by_username:{username}` → user_id (for username lookups)

### API Key → User Link

Current API key record has no user_id. Add it:

```rust
struct ApiKeyRecord {
  key_id: Uuid,
  key_id_prefix: String,
  key_hash: String,
  user_id: Uuid,            // ← NEW: which user owns this key
  roles: Vec<String>,       // kept for backward compat, but groups replace this
  // From user: Let's not keep backwards compat Claude... this isn't even alpha yet. No one is using this DB yet.
  created_at: DateTime<Utc>,
  is_revoked: bool,
}
```

Auth flow changes:

```
Old: API key → JWT with sub: key_id_prefix → no user concept
New: API key → look up user_id → JWT with sub: user_id → groups → permissions
```

Multiple keys can link to the same user. Revoking a user invalidates all their keys (permission check is on user_id).

### Group Entity

```rust
struct Group {
  name: String,
  default_allow: [Option<bool>; 8],   // crudlify tri-state: None=empty, Some(true)=allow, Some(false)=deny
  default_deny: [Option<bool>; 8],
  membership: GroupMembership,
  created_at: i64,
  updated_at: i64,
}

enum GroupMembership {
  Static(Vec<Uuid>),                   // explicit user_id list
  Query(String),                       // query-based membership (future)
}
```

<!-- 
I am not sure that the query-based should be future. This seems like a really good way to not only manage groups, but to not have to build a whole tool-set around adding/removing users from groups.

Let's maybe give this a talk?
 -->

Stored via SystemTables at `::aeordb:group:{name}`.

Special groups:
- `root` — bypasses all permission checks. Created at bootstrap.
- `everyone` — implicitly contains all users (if we want a default)
- Per-user groups: `user:{user_id}` — auto-created when a user is created

<!-- 
Are we storing the hash of this? OR is this literally the hash?

If we are hashing this value, then how will we ever find it again?
 -->

### Group → Path Links

Links connect groups to paths with optional flag overrides. Stored in the path's `.permissions` file:

```json
{
  "links": [
    {
      "group": "engineers",
      "allow": "crudli..",
      "deny": "........"
    },
    {
      "group": "user:abc123",
      "allow": "crudlify",
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

<!-- WYATT: Separate `.permissions` file, not mixed into `.config`. This way changing permissions doesn't touch index/parser config. Agree? -->

<!-- 
Will this `.permissions` file be one massive file, keyed by file path? Or were you thinking one .permissions file for each file?
 -->

Flag format: 8-character string, one per crudlify operation. `.` = no opinion, `+` = allow, `-` = deny. Or just use the letters: `crudlify` = all allowed, `cr......` = only create and read, `........` = no opinion on anything.

<!-- WYATT: Which flag format do you prefer? The dot/letter approach or +/-/. approach? -->

<!-- 
I like the dot letter approach. It is very explicit, while also being easy.
 -->

### Permission Resolution Algorithm

Per-level formula: `state = (state + level_allow) - level_deny`

```
running_state = -------- (start with nothing)

for each level from / down to the target path:
  level_allow = empty
  level_deny = empty

  for each group linked at this level:
    <!-- How do we sort groups, Claude? -->
    if user IS member:
      merge group's allow flags into level_allow (union)
      merge group's deny flags into level_deny (union)
    if user is NOT member AND group has "others" flags:
      merge others_allow into level_allow (union)
      merge others_deny into level_deny (union)

  for each bit:
    if level_allow[bit] is set: running_state[bit] = ON
    if level_deny[bit] is set:  running_state[bit] = OFF
    if both empty:              leave unchanged

final_permissions = running_state
```

Root user (member of `root` group): skip all checks, always allowed.

### Mapping HTTP Operations to crudlify Flags

| HTTP Method + Route | crudlify Flag |
|---|---|
| PUT /engine/{path} (new file) | `c` (Create) |
| GET /engine/{path} (file) | `r` (Read) |
| PUT /engine/{path} (overwrite) | `u` (Update) |
| DELETE /engine/{path} | `d` (Delete) |
| GET /engine/{path}/ (directory listing) | `l` (List) |
| POST /.functions/{name}/_invoke | `i` (Invoke) |
| PUT /.config/... | `f` (conFigure) |
| PUT /.functions/... (_deploy) | `y` (deploY) |

### JWT Changes

Current JWT `sub` = key_id_prefix. Change to:

```
sub: user_id (UUID string)
```

The JWT is proof of identity. Groups are resolved server-side via the cache. No groups in the token.

### Group Cache

```rust
struct GroupCache {
  cache: HashMap<Uuid, (Vec<String>, Instant)>,  // user_id → (group_names, fetched_at)
  ttl: Duration,  // default 60 seconds
}
```

- Cache hit + not expired → use cached groups
- Cache miss or expired → load user from DB, get groups, cache
- Write to group membership → immediately evict affected user(s)

---

## Implementation Tasks

```
Task 1: User entity (CRUD + SystemTables storage + registry)
Task 2: Update API key to include user_id + update JWT sub claim
Task 3: Bootstrap: zero users → create root user + root group + key
Task 4: Group entity (CRUD + SystemTables storage)
Task 5: Per-user auto-groups (user:{user_id} created with each user)
Task 6: Group → path links (.permissions file at each path)
Task 7: Permission resolution algorithm (walk path, resolve crudlify)
Task 8: Group cache (LRU + TTL + eviction on write)
Task 9: Permission middleware (check on every HTTP request)
Task 10: Admin HTTP endpoints (users CRUD, groups CRUD, membership)
Task 11: Tests + torture test
```

---

## Questions for Wyatt

1. **Separate `.permissions` file** — not mixed into `.config`?
<!-- 
I need to know if `.permissions` is a big blob of all files in that directory, or if you had something else in mind?
 -->
2. **Flag format** — dot/letter (`crudlify`, `cr......`) or symbols (`++....++`, `--....--`)?
<!-- 
I am not sure why we couldn't support both. I like the dot letters myself.
 -->
3. **Per-user auto-groups** — create `user:{user_id}` group automatically when a user is created? This is the "ownership" mechanism.
<!-- 
Yes.
 -->
4. **"everyone" group** — should there be an implicit group that all users belong to? Or must the admin explicitly create it?
<!-- 
🤔 I think we are quickly going to have groups that can be queries. But we have to be careful about this. It is inherantly insecure if we don't do it correctly. For example, if you say group "hi-trust => where(email:@admins.org)", then a user can just change their own email (allowed), and then suddenly they are part of a group!
 -->
5. **Existing API keys** — they have no user_id currently. Migration path: create a user for each existing key? Or invalidate old keys on first boot after the update?
<!-- 
This DB isn't even alpha yet. No one is using it. We can just make the change.
 -->
6. **Ready to build?**

---
---

## Round 2: Resolved + Query-Based Membership

### Resolved from Round 1

| Decision | Resolution |
|---|---|
| `roles` on ApiKeyRecord | **Dropped.** No backward compat needed. Groups replace roles entirely. |
| Per-user auto-groups | **Yes.** `user:{user_id}` created automatically when user is created. |
| Flag format | **Dot/letter canonical** (`crudlify`, `cr......`). Accept both formats on input. |
| `.permissions` scope | **One per directory.** Applies to everything in that directory. Deeper `.permissions` override. No per-file permissions. |
| Group sort order | **Irrelevant.** Union is commutative. All matching groups' flags are unioned at each level. |
| Existing API keys migration | **Just change it.** No migration — pre-alpha, no users. |
| Group storage | Hash of `::aeordb:group:{name}`. Registry at `::aeordb:group:_registry` for listing. |

### Query-Based Group Membership

Groups can define membership via a query instead of a static user list. This eliminates the need for manual add/remove tools.

```rust
enum GroupMembership {
  Static(Vec<Uuid>),
  Query(MembershipQuery),
}

struct MembershipQuery {
  field: String,       // which user field to query
  operator: String,    // "eq", "contains", "starts_with", "in", etc.
  value: String,       // the comparison value
}
```

Example groups:
```json
{ "name": "everyone", "membership": { "query": { "field": "is_active", "op": "eq", "value": "true" } } }
{ "name": "old_users", "membership": { "query": { "field": "created_at", "op": "lt", "value": "1700000000000" } } }
{ "name": "admins", "membership": { "query": { "field": "user_id", "op": "in", "value": "uuid1,uuid2,uuid3" } } }
```

### Security: Immutable vs Mutable User Fields

**The core danger:** If a query references a field the user can self-modify, the user can escalate their own privileges.

**Solution: field mutability classification.**

| User Field | Who Can Modify | Safe for Queries? |
|---|---|---|
| `user_id` | Nobody (generated) | Yes — immutable |
| `created_at` | Nobody (auto-set) | Yes — immutable |
| `updated_at` | System only | Yes — immutable |
| `is_active` | Admin only | Yes — admin-controlled |
| `username` | User (with permission) | **Dangerous** — user can change |
| `email` | User (with permission) | **Dangerous** — user can change |

**Enforcement options:**

**Option A: Whitelist safe fields.** Only allow queries against immutable/admin-only fields. Reject queries referencing `username`, `email`, or other user-mutable fields at group creation time.

**Option B: Field-level mutability flags.** Each user field has a `self_mutable: bool` flag. Queries are rejected if they reference a `self_mutable` field. Admin can mark additional fields as immutable.

**Option C: Query validation at evaluation time.** The query runs, but the system logs a security warning if it matches fields the user could have modified to gain access. Not preventive — just detective.

<!-- WYATT: I'm leaning Option A (whitelist) for simplicity. We know which fields are immutable — just enforce it. What do you think? -->

<!-- 
Yes, I think so. I do like software that allows a user to change their email address (unlike popular belief says, people actually _do_ change their email addresses). But this does pose a significant security risk, and you can always have an admin help you do it.
 -->

### The "everyone" Group

Instead of a magic implicit group, define "everyone" as a query-based group:

```json
{
  "name": "everyone",
  "membership": { "query": { "field": "is_active", "op": "eq", "value": "true" } },
  "default_allow": "........",
  "default_deny": "........"
}
```

This means:
- `everyone` is just a normal group with a query that matches all active users
- No magic — it's explicit
- If you want to exclude inactive users, the query already handles it
- If you don't want an "everyone" group, don't create one

### Revised Group Entity

```rust
struct Group {
  name: String,
  default_allow: String,           // 8 chars, crudlify dot/letter format
  default_deny: String,            // 8 chars
  membership: GroupMembership,
  created_at: i64,
  updated_at: i64,
}

enum GroupMembership {
  Static { user_ids: Vec<Uuid> },
  Query { field: String, operator: String, value: String },
}
```

### Checking Membership

```rust
fn is_member(user: &User, group: &Group) -> bool {
  match &group.membership {
    GroupMembership::Static { user_ids } => user_ids.contains(&user.user_id),
    GroupMembership::Query { field, operator, value } => {
      let user_value = user.get_field(field);
      match operator.as_str() {
        "eq" => user_value == *value,
        "neq" => user_value != *value,
        "contains" => user_value.contains(value.as_str()),
        "starts_with" => user_value.starts_with(value.as_str()),
        "in" => value.split(',').any(|v| v.trim() == user_value),
        "lt" => user_value < *value,
        "gt" => user_value > *value,
        _ => false,
      }
    }
  }
}
```

The `user.get_field(field)` method returns the string representation of any user field. This is evaluated at permission-resolution time (with caching via GroupCache).

### Query Membership + Cache Interaction

Query-based groups are evaluated per-user at permission-check time. The result is cached:

```
GroupCache key: (user_id, group_name) → bool (is member)
TTL: 60 seconds
Eviction: when the user is modified OR the group query is modified
```

This means query-based membership is dynamic — if a user's `created_at` crosses a threshold, they automatically enter/leave the group on next cache expiry. No manual add/remove.

<!-- 
Yes, I like this idea.

This also means that _all_ groups can be query groups. A users personal group is immutable (except root), and contains the query `user_id = abcde42341`. Simple!
 -->

---

## Questions for Wyatt

1. **Field whitelist for queries (Option A)** — only allow queries against immutable fields?
<!-- 
Yeah, I like the idea of immutable fields... root can change them, users can not change their own. The fields you listed are perfect.
 -->
2. **"everyone" as a query group** — `is_active = true` matches all active users?
<!-- 
Sure, why not?
 -->
3. **Query operators** — eq, neq, contains, starts_with, in, lt, gt — enough?
<!-- 
I think it is for now!
 -->
4. **Cache key** — `(user_id, group_name)` or just `user_id → all_groups`?
<!-- 
HHHmmmm, I think the latter, `user_id → all_groups`.
 -->
5. **Ready to build?**

---
---

## Round 3: All Groups Are Query Groups + Final Design

### Key Insight from Wyatt

**ALL groups are query groups.** There is no "static" vs "query" distinction:

```
Per-user group:  { query: { field: "user_id", op: "eq", value: "abc-123" } }
Static group:    { query: { field: "user_id", op: "in", value: "abc,def,ghi" } }
Everyone:        { query: { field: "is_active", op: "eq", value: "true" } }
Time-based:      { query: { field: "created_at", op: "lt", value: "1700000000000" } }
```

One model. One evaluation path. One cache. Simple.

### Resolved from Round 2

| Decision | Resolution |
|---|---|
| Field whitelist (Option A) | **Confirmed.** Queries can only reference immutable fields. Root can modify them, users cannot. |
| Immutable fields | `user_id`, `created_at`, `updated_at`, `is_active` |
| Mutable fields (admin-only change) | `username`, `email` — user requests change, admin applies it |
| "everyone" group | Query-based: `is_active = true`. Explicit, no magic. |
| Query operators | eq, neq, contains, starts_with, in, lt, gt. Enough for now. |
| Cache key | `user_id → Vec<String>` (all group names). One cache entry per user. |
| All groups are queries | Yes — per-user groups use `user_id = X`. Static lists use `user_id IN (...)`. |

### Final Data Model

```rust
struct User {
  user_id: Uuid,
  username: String,
  email: Option<String>,
  created_at: i64,
  updated_at: i64,
  is_active: bool,
}

struct Group {
  name: String,
  default_allow: String,      // "crudlify" dot/letter format, 8 chars
  default_deny: String,       // "........" = no opinion
  query_field: String,         // e.g., "user_id", "is_active", "created_at"
  query_operator: String,      // e.g., "eq", "in", "lt"
  query_value: String,         // e.g., "abc-123", "true", "1700000000000"
  created_at: i64,
  updated_at: i64,
}

struct ApiKeyRecord {
  key_id: Uuid,
  key_id_prefix: String,
  key_hash: String,
  user_id: Uuid,
  created_at: i64,
  is_revoked: bool,
}

struct PathPermissions {
  links: Vec<PermissionLink>,
}

struct PermissionLink {
  group: String,
  allow: String,                      // "crudlify" or "cr......" etc.
  deny: String,
  others_allow: Option<String>,       // for non-members
  others_deny: Option<String>,
}
```

### Bootstrap Sequence

```
if count_users() == 0:
  1. root_user = create_user("root", None)                          // user_id generated
  2. root_group = create_group("root", query: user_id = root_user.user_id)
     // default_allow: "crudlify", default_deny: "........"
     // root group has special flag: bypasses all permission checks
  3. api_key = generate_api_key(linked to root_user.user_id)
  4. signing_key = generate_jwt_signing_key()
  5. print api_key (shown once)
```

### Permission Resolution (Final)

```
fn check_permission(user: &User, path: &str, operation: CrudlifyOp) -> bool {
  // Root bypass
  if user_is_root(user) { return true; }

  // Get user's groups (cached)
  let user_groups = cache.get_or_load(user.user_id);

  // Walk path from root to target
  let mut state = [false; 8];  // all denied initially

  for level in path_levels(path) {
    let permissions = load_permissions(level);  // .permissions file
    if permissions.is_none() { continue; }

    let mut level_allow = [None; 8];
    let mut level_deny = [None; 8];

    for link in permissions.links {
      let is_member = user_groups.contains(&link.group);

      if is_member {
        merge_flags(&mut level_allow, &link.allow);
        merge_flags(&mut level_deny, &link.deny);
      } else if let Some(others_allow) = &link.others_allow {
        merge_flags(&mut level_allow, others_allow);
        if let Some(others_deny) = &link.others_deny {
          merge_flags(&mut level_deny, others_deny);
        }
      }
    }

    // Apply: allow first, then deny (deny wins at same level)
    for i in 0..8 {
      if level_allow[i] == Some(true) { state[i] = true; }
      if level_deny[i] == Some(true) { state[i] = false; }
    }
  }

  state[operation.index()]
}
```

### Membership Evaluation

```rust
fn evaluate_membership(user: &User, group: &Group) -> bool {
  let user_value = match group.query_field.as_str() {
    "user_id" => user.user_id.to_string(),
    "username" => user.username.clone(),
    "email" => user.email.clone().unwrap_or_default(),
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

### Group Cache

```rust
struct GroupCache {
  entries: HashMap<Uuid, CacheEntry>,
  ttl: Duration,
}

struct CacheEntry {
  groups: Vec<String>,
  fetched_at: Instant,
}
```

Load all groups, evaluate membership for the user, cache the result. Evict on:
- User modification (admin changes a field)
- Group query modification (admin changes a group's query)
- TTL expiry (passive, 60 seconds default)

### Implementation Tasks (Revised)

```
Task 1:  User entity (CRUD + SystemTables + registry + username lookup)
Task 2:  Group entity (CRUD + SystemTables + registry + query-based membership)
Task 3:  Update ApiKeyRecord (add user_id, drop roles)
Task 4:  Update JWT (sub = user_id, not key_id_prefix)
Task 5:  Bootstrap: zero users → root user + root group + key
Task 6:  Per-user auto-groups (user:{user_id} query group)
Task 7:  .permissions files (per-directory, link format)
Task 8:  Permission resolution algorithm (path walk + crudlify)
Task 9:  Group cache (user_id → groups, LRU + TTL + eviction)
Task 10: Permission middleware (check every HTTP request)
Task 11: HTTP operation → crudlify flag mapping
Task 12: Admin endpoints (user CRUD, group CRUD, permissions CRUD)
Task 13: Tests + security tests (privilege escalation attempts)
```

### Test Plan Highlights

```
Security tests (CRITICAL):
  - test_user_cannot_modify_own_immutable_fields
  - test_query_group_rejects_mutable_field_query
  - test_privilege_escalation_via_email_change_blocked
  - test_root_bypasses_all_permissions
  - test_non_root_denied_without_permission
  - test_deny_overrides_allow_at_same_level
  - test_deeper_allow_overrides_shallower_deny
  - test_others_flags_apply_to_non_members
  - test_cache_eviction_on_user_modification
  - test_cache_eviction_on_group_query_change
  - test_nil_uuid_rejected_on_user_create
  - test_nil_uuid_rejected_on_user_update
  - test_nil_uuid_rejected_on_api_key_create
  - test_nil_uuid_rejected_via_wasm_plugin
  - test_nil_uuid_rejected_via_http_admin_endpoint
  - test_nil_uuid_only_allowed_in_bootstrap
  - test_store_user_rejects_nil_at_engine_level
```

---

## Questions for Wyatt

1. **Root group has a "bypass" flag** — or does root just happen to have `crudlify` allow on `/`? I'm thinking an explicit `is_superuser: bool` on the group, so we don't rely on permission links for root access.
<!-- 
I am thinking something explicit too... but what makes sense, and is such that other users can not elevate themselves (or others)?
 -->
2. **Ready to build, or more to discuss?**

---
---

## Round 4: Root Is the Engine + Final Resolution

### Root Is NOT a Database Entity

Root is the engine runtime. Not a user, not a group, not a query.

```rust
const ROOT_USER_ID: Uuid = Uuid::nil();  // 00000000-0000-0000-0000-000000000000

fn check_permission(user_id: &Uuid, ...) -> bool {
  if *user_id == ROOT_USER_ID {
    return true;  // engine-level bypass. No database lookup.
  }
  // ... normal permission resolution
}
```

**Bootstrap (zero users):**
1. Generate API key linked to `ROOT_USER_ID` (nil UUID)
2. Generate JWT signing key
3. Print API key once
4. No user entity. No group entity. Root exists in the code, not the data.

**Nil UUID protection — enforced at the ENGINE LEVEL, not the HTTP level:**

The validation lives at the lowest layer — `SystemTables::store_user` and `SystemTables::store_api_key`. Every path into the system goes through these methods: HTTP routes, WASM plugins, native plugins, batch operations, direct engine calls. None can bypass.

```rust
// In SystemTables — the ONLY place users are stored
fn store_user(&self, user: &User) -> Result<()> {
  validate_user_id(&user.user_id)?;  // ← engine-level, cannot bypass
  // ... store
}

// Bootstrap is the ONE exception
// Let's make sure we put comments on this code, right at the method, to let other bots
// know not to ever expose this to an interface.
fn store_api_key_for_bootstrap(&self, record: &ApiKeyRecord) -> Result<()> {
  // Allows nil UUID — only called from bootstrap, not exposed externally
  // ... store
}

fn store_api_key(&self, record: &ApiKeyRecord) -> Result<()> {
  validate_user_id(&record.user_id)?;  // ← rejects nil UUID
  // ... store
}

fn validate_user_id(user_id: &Uuid) -> Result<()> {
  if *user_id == ROOT_USER_ID {
    return Err(EngineError::ReservedUserId);
  }
  Ok(())
}
```

**This covers ALL entry points:**
- HTTP admin endpoints → call `store_user` → validated
- WASM plugin SDK → calls engine methods → call `store_user` → validated
- Native plugins (dlopen) → call engine methods → validated
- Batch/migration operations → call `store_user` → validated
- Direct SystemTables access → validated at the method level
- Bootstrap → uses separate `store_api_key_for_bootstrap` (not exposed externally)

**"Admin" groups are normal groups.** Want an admin team? Create a group with broad permissions:
```json
{ "name": "admins", "query_field": "user_id", "query_operator": "in",
  "query_value": "uuid1,uuid2", "default_allow": "crudlify", "default_deny": "........" }
```
They're still subject to the permission system. Only nil UUID bypasses it. Root stays tight and close.

<!-- 
🎉
 -->

### Resolved from Round 3

| Decision | Resolution |
|---|---|
| Root bypass mechanism | **Nil UUID hardcoded in engine.** Not a group, not a user entity. |
| Root group | **Does not exist.** No "root" group. No reserved group names. |
| Nil UUID protection | **Rejected on all user-facing input.** Only bootstrap can link to nil UUID. |
| `.permissions` file | **One per directory.** Covers everything at and under that path. No per-file permissions. Stored as a regular file via the engine. |
| Admin groups | **Normal groups with broad permissions.** Still subject to permission system. |

### Revised Bootstrap

```
if count_api_keys() == 0:       // simpler check — no users exist = no keys exist
  1. api_key = generate_api_key(user_id: ROOT_USER_ID)
  2. signing_key = generate_jwt_signing_key()
  3. store both via SystemTables
  4. print api_key (shown once)
```

No user entity created. No group created. The first REAL user is created by root via the admin API.

### Revised Implementation Tasks

```
Task 1:  User entity (CRUD + SystemTables + registry)
Task 2:  Group entity (all query-based, CRUD + SystemTables + registry)
Task 3:  Update ApiKeyRecord (add user_id, drop roles)
Task 4:  Update JWT (sub = user_id)
Task 5:  Root as nil UUID (engine bypass + validation)
Task 6:  Bootstrap (zero keys → root API key with nil UUID)
Task 7:  Per-user auto-groups (user:{user_id} created on user creation)
Task 8:  .permissions files (per-directory, JSON with links)
Task 9:  Permission resolution (path walk + crudlify + group membership evaluation)
Task 10: Group cache (user_id → all_groups, LRU + TTL + eviction)
Task 11: Permission middleware (check every HTTP request)
Task 12: HTTP operation → crudlify flag mapping
Task 13: Admin endpoints (user CRUD, group CRUD)
Task 14: Tests (security: nil UUID rejection at ALL layers, privilege escalation, cache eviction)
```

---

## Questions for Wyatt

1. **Ready to write to the plan doc and build?**
<!-- 
What is the plan for testing? I want dozens of real-world cases figured out and written as unit tests (admins, secretaries, single user, organization, etc...).
 -->
2. **Any other security edge cases to think about?**
<!-- 
Probably! We should probably us our AGIS critical thinking skills! Let me put on my critical thinking cap. 🤔

How does one become "root"?
 -->

---
---

## Round 5: How Does One Become Root + Real-World Test Scenarios

### How Does One Become Root?

**Exactly one path:** Have the bootstrap API key.

```
Server starts → zero API keys → generate root key → print once → done.
Whoever holds that key authenticates as nil UUID = root.
```

No other path exists. No group to join. No flag to set. No escalation possible.

**Lost root key recovery:** `aeordb-cli emergency-reset --database path.aeordb` — regenerates the root key. Requires filesystem access to the database file (if you have that, you own the machine anyway). Future feature.

### Attack Vectors Analysis

| Attack | Mitigated By |
|---|---|
| Steal root API key | Ops problem (don't leak it). Not a DB problem. |
| Forge JWT with nil UUID | Requires signing key (stored in DB, not exposed). |
| Create user with nil UUID | `validate_user_id` at engine level in `store_user`. |
| Plugin creates nil UUID | Same engine-level validation — all paths go through `store_user`. |
| Direct file modification | Filesystem access = game over. Same as any database. |
| SQL injection equivalent | No SQL. Queries are structured, not string-interpolated. |
| Permission escalation via group query | Immutable field whitelist prevents self-modification attacks. |

### Real-World Test Scenarios

These are full end-to-end scenarios, not just unit tests. Each simulates a realistic use case.

```
scenario_single_developer_spec.rs:
  Setup: bootstrap → root key → one user
  - test_root_creates_first_user (alice)
  - test_alice_creates_api_key
  - test_alice_stores_files
  - test_alice_reads_own_files
  - test_alice_deletes_own_files
  - test_alice_queries_own_data
  - test_alice_lists_own_directories
  - test_alice_creates_snapshot
  - test_alice_can_do_everything_without_permissions_setup
    (no .permissions files = everything allowed by default? Or denied?)

scenario_small_team_spec.rs:
  Setup: root creates 3 users (alice=admin, bob=developer, carol=viewer)
  Groups: "admins" (alice), "developers" (alice, bob), "viewers" (everyone)
  Permissions: /project/.permissions grants crudlify to developers, r..l... to viewers
  - test_alice_has_full_access (admin)
  - test_bob_can_create_and_read (developer)
  - test_bob_cannot_configure (no f flag)
  - test_bob_cannot_deploy (no y flag)
  - test_carol_can_only_read_and_list (viewer)
  - test_carol_cannot_create (denied)
  - test_carol_cannot_delete (denied)
  - test_alice_adds_dave_to_developers
  - test_dave_immediately_has_developer_access
  - test_alice_removes_bob_from_developers
  - test_bob_loses_developer_access_after_cache_expiry

scenario_organization_spec.rs:
  Setup: root creates departments with hierarchical permissions
  /org/.permissions → "employees" get r..l...
  /org/engineering/.permissions → "engineers" get crudli..
  /org/engineering/secrets/.permissions → "security_team" gets crudlify,
    "engineers" others_deny = crudlify (locked out unless in security_team)
  /org/public/.permissions → "everyone" gets r..l...
  - test_engineer_reads_engineering_files
  - test_engineer_cannot_read_secrets (locked out by others_deny)
  - test_security_member_reads_secrets
  - test_employee_reads_public_files
  - test_employee_cannot_read_engineering (no permission)
  - test_new_engineer_added_via_group_query_change
  - test_fired_employee_deactivated_loses_all_access (is_active = false)

scenario_secretary_spec.rs:
  Setup: secretary can read and list everything, but cannot modify or delete
  Group: "secretary" with default_allow = .r..l...
  /company/.permissions → secretary linked with allow: .r..l..., deny: c.ud.ify
  - test_secretary_reads_any_file
  - test_secretary_lists_any_directory
  - test_secretary_cannot_create
  - test_secretary_cannot_update
  - test_secretary_cannot_delete
  - test_secretary_cannot_configure
  - test_secretary_cannot_deploy
  - test_secretary_cannot_invoke_functions

scenario_multi_tenant_spec.rs:
  Setup: two tenants sharing one database, isolated by permissions
  /tenant_a/.permissions → "tenant_a_users" get crudlify, others_deny = crudlify
  /tenant_b/.permissions → "tenant_b_users" get crudlify, others_deny = crudlify
  - test_tenant_a_user_accesses_tenant_a_files
  - test_tenant_a_user_cannot_access_tenant_b_files
  - test_tenant_b_user_cannot_access_tenant_a_files
  - test_root_accesses_everything (nil UUID bypass)

scenario_permission_inheritance_spec.rs:
  Setup: nested directories with overlapping permissions
  / → "everyone" allow r..l...
  /docs/ → "writers" allow crudl...
  /docs/private/ → "writers" deny crudlify, "owner" allow crudlify
  /docs/private/for-alice.txt → (no .permissions, inherits from /docs/private/)
  - test_everyone_reads_root
  - test_writer_creates_in_docs
  - test_writer_denied_in_private (deeper deny overrides)
  - test_owner_allowed_in_private (deeper allow overrides deny)
  - test_inheritance_works_for_files_without_own_permissions

scenario_security_attacks_spec.rs:
  - test_user_tries_to_create_nil_uuid_user → rejected
  - test_user_tries_to_set_own_id_to_nil → rejected
  - test_user_creates_group_querying_mutable_field → rejected (if enforced)
  - test_user_changes_email_to_join_group → blocked (immutable field)
  - test_expired_jwt_denied
  - test_revoked_api_key_denied
  - test_forged_jwt_with_nil_uuid → invalid signature
  - test_non_root_tries_to_create_api_key_for_nil_uuid → rejected
```

### Default Permissions (No .permissions File)

**Important open question:** When a path has NO `.permissions` file, what's the default?

**Option A: Deny all.** Nothing is accessible unless explicitly permitted. Secure but requires setup for every path. The root user (nil UUID) bypasses this, so bootstrap always works.

**Option B: Allow all.** Everything is accessible by default. Permissions only restrict. Easy for single-user/dev but dangerous in multi-user.

**Option C: Allow authenticated.** Any authenticated user (valid JWT) can access anything. Permissions are opt-in restrictions. Middle ground.

<!-- WYATT: I'm leaning Option A (deny all) for multi-user security, with a note that the `--auth=false` flag disables everything for single-user dev mode. What do you think? -->

---

## Questions for Wyatt

1. **Default permissions (no .permissions file)** — deny all, allow all, or allow authenticated?
2. **Emergency root key reset** — CLI flag that requires filesystem access? Worth planning now?
3. **Do the test scenarios cover enough real-world cases?** Missing any roles/patterns?
4. **Ready to write to plan doc?**


<!-- 
I like it!
 -->

---
---

## Round 6: Final Resolutions

### Resolved from Round 5

| Decision | Resolution |
|---|---|
| Default permissions (no .permissions) | **Deny all.** Nothing accessible unless explicitly granted. Root (nil UUID) bypasses. `--auth=false` disables for dev mode. |
| Emergency root key reset | **Yes.** `aeordb-cli emergency-reset --database path.aeordb` — revokes old root key, generates new one. Requires filesystem access. Encrypted data stays encrypted. |
| Test scenarios | 7 scenarios covering: single dev, small team, organization, secretary, multi-tenant, inheritance, security attacks. |
| Bootstrap comments | Add explicit `// SECURITY: never expose this method to any external interface` on `store_api_key_for_bootstrap`. |

### Emergency Reset Design

```
aeordb-cli emergency-reset --database /path/to/data.aeordb

1. Open the .aeordb file directly (no server needed)
2. Find API key entry linked to nil UUID
3. Revoke it (mark deleted)
4. Generate new API key linked to nil UUID
5. Store it
6. Print new key
7. Signing key unchanged (other users' JWTs remain valid)
8. Encrypted data unchanged (vault keys unaffected)
```

<!-- 
Let's make sure we have a y/n prompt, unless --force is provided.
 -->

### Revised Implementation Tasks (Final)

```
Task 1:  User entity (CRUD + SystemTables + registry + username lookup)
Task 2:  Group entity (all query-based, CRUD + SystemTables + registry)
Task 3:  Update ApiKeyRecord (add user_id, drop roles)
Task 4:  Update JWT (sub = user_id, nil UUID for root)
Task 5:  Root as nil UUID (engine bypass + validation at store_user/store_api_key)
Task 6:  Bootstrap (zero keys → root API key with nil UUID)
Task 7:  Per-user auto-groups (user:{user_id} query group on user creation)
Task 8:  .permissions files (per-directory JSON, deny-all default)
Task 9:  Permission resolution (path walk + crudlify + membership evaluation)
Task 10: Group cache (user_id → all_groups, LRU + TTL + eviction)
Task 11: Permission middleware (check every HTTP request, map op → crudlify)
Task 12: Admin endpoints (user CRUD, group CRUD)
Task 13: Emergency reset CLI command
Task 14: Tests — unit + 7 real-world scenarios + security attack tests
```

### All Design Decisions Summary

| Topic | Decision |
|---|---|
| Root identity | Nil UUID hardcoded in engine. Not a database entity. |
| Root bypass | `if user_id == nil → allow`. Engine-level, cannot bypass. |
| Nil UUID protection | `validate_user_id` at engine level in `store_user`/`store_api_key`. |
| Bootstrap | Zero API keys → generate root key with nil UUID. No user/group entities. |
| Emergency reset | CLI command, requires filesystem access. Revoke old, generate new. |
| User entity | user_id, username, email, is_active, timestamps. |
| Groups | ALL query-based. Per-user = `user_id eq X`. Static = `user_id in X,Y,Z`. |
| Group queries | Immutable field whitelist only (user_id, created_at, updated_at, is_active). |
| Mutable fields | username, email — admin-only changes, blocked from group queries. |
| Permissions | `crudlify` 8-flag tri-state (dot/letter format). |
| .permissions file | One per directory. JSON with group links. |
| Default permissions | Deny all. Explicit grants only. |
| Permission resolution | Walk root → target, per-level: `state = (state + allow) - deny`. |
| Group cache | `user_id → Vec<group_names>`, LRU + 60s TTL + evict on write. |
| API key → user | `user_id` field on ApiKeyRecord. JWT `sub` = user_id. |
| `--auth=false` | Disables entire permission system for dev mode. |

---

*Plan is complete. Ready to write to `bot-docs/plan/` and build.*

<!-- 
I think the plan is ready! Don't forget to apply your final AGIS skills to it, for one final pass.
 -->