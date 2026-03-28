# Permissions System

**Parent:** [Master Plan](./master-plan.md)
**Status:** In Design

---

## Core Philosophy

Unix permissions, evolved. The filesystem metaphor carries through: paths, ownership, groups — but with finer-grained operations, multi-group assignment, tri-state flags, and proximity-ordered resolution.

**Design principles:**
- Permissions are declarations of intent — if a flag is set, someone meant it
- Empty flags mean "no opinion" — inherit from parent state
- Deny always wins over allow at the same level
- Deeper levels can override shallower levels (both restrict AND grant back)
- Everything is a group — users are just groups with one member
- Groups link to paths — the link carries the permission flags

---

## Operations (Permission Flags)

Eight operations, ordered as `crudlify`:

| Position | Flag | Operation | Description |
|---|---|---|---|
| 0 | `c` | Create | Store a new document at a path |
| 1 | `r` | Read | Retrieve a document's content |
| 2 | `u` | Update | Modify an existing document's content |
| 3 | `d` | Delete | Delete a document (recoverable via version restore) |
| 4 | `l` | List | List documents at a path |
| 5 | `i` | Invoke | Execute a deployed function/plugin |
| 6 | `f` | conFigure | Write to `.config` (parsers, indexes, validators, permissions), chown, chgrp |
| 7 | `y` | deploY | Deploy plugins/functions at a path |

**No soft-delete in the engine.** Delete means delete. Recovery is via versioning — the content-addressed chunk store preserves all previous states. Soft-delete is an application-level concern; users who want it can add their own `is_deleted` field and index it.

**No undelete operation.** Restoring deleted data is a version restore operation, not a permission flag.

**No purge operation.** Garbage collection of old versions/chunks is an administrative function, not a per-document permission.

---

## Tri-State Flags

Each flag per operation has three possible values:

| Value | Meaning |
|---|---|
| allow (`+`) | Explicitly grant this operation |
| deny (`-`) | Explicitly deny this operation |
| empty (null/`.`) | No opinion — don't touch the inherited state |

**Flags are declarations of intent.** If set, someone meant it. If empty, nobody has an opinion at this level — the inherited state stands.

This is fundamentally different from Unix, where every file must have a complete `rwxrwxrwx`. In aeordb, a permission link can say "I only care about denying delete" and be silent on everything else.

---

## Groups

A group is:
- A name
- Default permission flags (allow + deny, tri-state)
- A membership definition (user list, query, or pattern)

```
{
  name: "engineers",
  default_allow: "crudli..",
  default_deny:  "........",
  members: { query: "role = 'engineer'" }
}

{
  name: "user:alice",
  default_allow: "crudlify",
  default_deny:  "........",
  members: { users: ["alice_uuid"] }
}

{
  name: "guests",
  default_allow: ".r..l...",
  default_deny:  "........",
  members: { query: "role = 'guest'" }
}
```

### Key Properties

- **Every user has their own group** — a group with one member (the user). This is the "ownership" group.
- **Groups are always what link to paths** — never bare users. The user's own group is the mechanism for individual assignment.
- **Membership can be defined by query/pattern** — not just static user lists. "All users where first_name = 'Karen'" is a valid group membership definition.
- **Groups have default flags** — used when a link doesn't specify its own flags.

---

## Links

A link connects a group to a path. The link optionally carries its own permission flags that override the group's defaults.

```
{
  group: "engineers",
  path: "/myapp/users/",
  allow: null,          // null = use group's default_allow
  deny: null,           // null = use group's default_deny
}

{
  group: "engineers",
  path: "/myapp/secrets/",
  allow: ".r..l...",    // override: read and list only
  deny:  "c.ud.ify",   // override: deny everything except read and list
}
```

### "Others" on Links

A link can also specify what happens to users who are NOT in the group:

```
{
  group: "security_team",
  path: "/myapp/secrets/",
  allow: "crudlify",            // members get full access
  deny:  "........",
  others_allow: "........",     // non-members get nothing
  others_deny:  "crudlify",    // non-members explicitly denied everything
}
```

This replaces Unix's "other" permission column. The `others_allow` and `others_deny` apply to every user who is NOT a member of this group.

---

## Resolution Algorithm

When a user accesses a path, permissions are resolved by walking from root to the target path, accumulating state at each level.

### Per-Level Merge Formula

```
state = (state + level_allow) - level_deny
```

At each level:
1. Collect all groups the user is a member of that have links at this level
2. Also collect "others" flags from groups the user is NOT a member of
3. Union all applicable allow flags (any allow = allow for that bit)
4. Union all applicable deny flags (any deny = deny for that bit)
5. Apply allow first (turn bits ON), then deny (turn bits OFF)

Deny applied after allow means **deny wins at the same level**. But a deeper level's allow can override a shallower level's deny.

Empty/null flags are no-ops — they do not affect the running state.

### Full Walk

```
running_state = -------- (start with nothing)

for each level from / down to the target path:
  level_allow = empty
  level_deny = empty

  for each group linked at this level:
    if user IS member:
      merge group's allow flags into level_allow (union)
      merge group's deny flags into level_deny (union)
    if user is NOT member AND group has "others" flags:
      merge others_allow into level_allow (union)
      merge others_deny into level_deny (union)

  // Apply this level to running state
  for each bit:
    if level_allow[bit] is set: running_state[bit] = ON
    if level_deny[bit] is set:  running_state[bit] = OFF
    if both are empty:          leave unchanged

final_permissions = running_state
```

### Example Walkthrough

Setup:
```
Groups:
  "everyone":      default_allow: "crudlify", members: all users
  "myapp_team":    default_allow: "........", default_deny: ".....ify"
  "security_team": default_allow: "crudlify", others_deny: "crudlify"
  "user:alice":    default_allow: ".r..l...", members: [alice]

Links:
  / → "everyone" (no flag override)
  /myapp/ → "myapp_team" (no flag override)
  /myapp/secrets/ → "security_team" (no flag override)
  /myapp/secrets/for-alice.txt → "user:alice" (no flag override)
```

Alice accesses `/myapp/secrets/for-alice.txt`:

```
Level /
  Alice is in "everyone" → allow: crudlify, deny: ........
  State: ........ + crudlify = crudlify
  State: crudlify - ........ = crudlify
  → crudlify

Level /myapp/
  Alice is in "myapp_team" → allow: ........, deny: .....ify
  State: crudlify + ........ = crudlify
  State: crudlify - .....ify = crudl...
  → crudl...

Level /myapp/secrets/
  Alice is NOT in "security_team" → others_allow: ........, others_deny: crudlify
  State: crudl... + ........ = crudl...
  State: crudl... - crudlify = ........
  → ........  (LOCKED OUT)

Level /myapp/secrets/for-alice.txt
  Alice IS in "user:alice" → allow: .r..l..., deny: ........
  State: ........ + .r..l... = .r..l...
  State: .r..l... - ........ = .r..l...
  → .r..l...  (Alice can read and list this specific file)
```

---

## Ownership

Ownership is not a special privilege — it's a group relationship.

When a user creates a document:
1. The user's personal group (e.g., `user:alice`) is linked to the document
2. The link gets the default flags for that group (typically full access)
3. An admin can modify the link's flags to restrict the owner
4. Ownership transfer = change which user-group is linked

**Root/admin safety valve:**
- A `root` or `administrators` group always exists
- It's linked at `/` with `allow: crudlify, deny: ........`
- This link cannot be removed — it's the bootstrap guarantee
- No matter how badly permissions are misconfigured, root can fix it

---

## Mandatory Document Fields

With soft-delete removed from the engine (delete is real delete, recovery via versioning), the mandatory metadata fields are:

| Field | Type | Description |
|---|---|---|
| `document_id` | UUID v4 | Unique document identifier |
| `created_at` | Timestamp | When the document was created |
| `updated_at` | Timestamp | When the document was last modified |

These are managed by the engine. Users can provide their own values or let the engine auto-generate.

---

## Storage

Permissions (group definitions and links) are stored at `.config` at each path level, alongside parser and index configuration. They are:
- Chunked like everything else
- Versioned like everything else
- Replicated like everything else
- Modifiable by anyone with the `configure` (`f`) flag at that path

---

## Extensibility

The built-in permission system handles group/link/flag resolution. For cases that need more complex logic (time-based access, attribute-based policies, external auth checks), the existing **rule plugins** (WASM) can augment the built-in system.

The built-in system runs first (fast, no plugin overhead). If it allows the operation, rule plugins get a chance to further restrict. Rule plugins cannot GRANT access that the built-in system denied — they can only further restrict.

This keeps the common case fast (pure flag evaluation, no WASM) while allowing arbitrary complexity when needed.

---

## Open Questions

- [ ] Group membership query syntax and evaluation engine
- [ ] Performance of walking the path hierarchy for every operation — caching strategy?
- [ ] How are group definitions stored? At `/.system/groups/`?
- [ ] Maximum number of groups per link? Per path?
- [ ] Audit logging of permission changes
- [ ] CLI commands for permission management (chmod/chown/chgrp equivalents)
- [ ] How does version restore interact with permissions? (Restore old data but keep current permissions?)
