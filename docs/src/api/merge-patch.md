# JSON Merge Patch

`PATCH /files/{path}` with `Content-Type: application/merge-patch+json` partially updates a JSON document **server-side** — the engine reads the stored file, merges the patch in, and writes back atomically. Clients send only the changed fields. No more read-modify-write round trips, no more lost-update races between the read and the put.

This document is the implementation guide for client SDKs and applications adopting the endpoint. For the underlying spec, see [RFC 7396](https://datatracker.ietf.org/doc/html/rfc7396).

> **Note on the dispatcher.** `PATCH /files/{path}` is overloaded by `Content-Type`: `application/merge-patch+json` lands here; `application/json` is the existing [rename endpoint](./files.md#patch-filespath). The content-type alone distinguishes them — a merge-patch body that happens to contain a `"to"` key will be merged, not renamed.

---

## Why this exists

Before merge-patch, updating a single field in a stored JSON document required three round trips:

1. `GET /files/record.json` — pull the current state.
2. Parse it, mutate locally.
3. `PUT /files/record.json` — push the full document back.

This is a problem because:

- **It's a lost-update race.** If another writer modifies the file between your GET and PUT, your PUT silently overwrites their change.
- **It's bandwidth-expensive.** A 5 MB record requires 10 MB of transfer to change a 12-byte field.
- **It's client-side complex.** Every client has to implement parse/mutate/serialize correctly per schema.

Server-side merge fixes all three: only the patch (typically tiny) crosses the wire, and the engine's write path is single-threaded so two concurrent patches to disjoint keys compose correctly.

---

## Wire protocol

```
PATCH /files/{path}
Authorization: Bearer <token>
Content-Type:  application/merge-patch+json

<JSON patch body>
```

Query parameters:

| Param   | Type             | Default | Meaning |
|---------|------------------|---------|---------|
| `depth` | signed integer   | unset   | Controls how deep the merge recurses. See [Depth bound](#depth-bound) below. Unset = strict RFC 7396 (unbounded). |

Status codes:

| Status | Meaning |
|--------|---------|
| `200 OK` | Existing file merged. |
| `201 Created` | File did not previously exist; patch became the new document. |
| `400 Bad Request` | Malformed query parameter. |
| `401 Unauthorized` | Missing/invalid token. |
| `403 Forbidden` | Caller lacks `update` permission on the path. |
| `404 Not Found` | Path is in `/.aeordb-system/...` (system data is never client-modifiable). |
| `413 Payload Too Large` | Patch body or stored file exceeds **10 MB**. |
| `415 Unsupported Media Type` | Patch body is not valid JSON, OR the stored file is not valid JSON. |

Successful responses have the same shape as `PUT /files/{path}`:

```json
{
  "path":         "/record.json",
  "content_type": "application/json",
  "size":         85,
  "created_at":   1779470049858,
  "updated_at":   1779470049903,
  "hash":         "bc14a77290fb594388efe43fbb4a0b31411cea40b6a725b5fcd3aa782a3cd4e4"
}
```

---

## Merge semantics

The merge is **recursive JSON object merge**, per RFC 7396:

1. **Patch is an object** → each key is merged into the target:
   - `null` value → **deletes** the key from the target.
   - Object value → **recursive merge** into the target's value at that key.
   - Anything else (scalar, array) → **replaces** the target's value at that key.
2. **Patch is anything else** (top-level scalar, array, or `null`) → the patch replaces the entire stored document.

### Arrays are replaced, not concatenated

This is a frequent surprise. RFC 7396 has no merge concept for arrays — they are always treated as opaque values.

```
target: {"tags": ["a", "b", "c"]}
patch:  {"tags": ["d"]}
result: {"tags": ["d"]}        // NOT ["a", "b", "c", "d"]
```

If you want to append to an array, you have to read it, append client-side, and PATCH the new array, or use a separate operation.

### `null` is delete, not "set to null"

```
target: {"name": "Alice", "email": "a@x"}
patch:  {"email": null}
result: {"name": "Alice"}
```

If you genuinely need to store a JSON `null` for a field, you can't do it with merge-patch. Use PUT with the full document instead.

### Missing file → 201 Created

```
GET /files/new.json    → 404
PATCH /files/new.json with {"a": 1}    → 201 Created
GET /files/new.json    → {"a": 1}
```

The missing file is treated as `{}` for merge purposes.

---

## Depth bound

By default the merge recurses to arbitrary depth. The `?depth=N` query parameter bounds that recursion. The sign is meaningful:

| `?depth=...` | Behavior |
|--------------|---------|
| (unset)      | Strict RFC 7396 — unbounded recursion. |
| `?depth=0`   | **Wholesale replace** — the patch overwrites the stored document. Functionally identical to a `PUT`. |
| `?depth=+N`  | Merge N levels deep. At the boundary, deeper object values in the patch **REPLACE** the target's subtree. |
| `?depth=-N`  | Merge N levels deep. At the boundary, deeper object values in the patch are **IGNORED** (target's subtree is preserved). |

The signed distinction only fires for **object values at the boundary**. Scalars and `null` always behave the same regardless of sign — `null` deletes at the current merge level, scalars insert/replace.

### Why positive vs negative?

The use cases are genuinely different.

**Positive depth** — "I want to update top-level fields and atomically swap a known subtree":

```
target: {"user": {"name": "Alice", "prefs": {"theme": "dark"}}, "session": "abc"}
patch:  {"user": {"prefs": {"theme": "light"}}}
?depth=1
result: {"user": {"prefs": {"theme": "light"}}, "session": "abc"}
                  ▲
              user is REPLACED — name is lost.
              "session" is preserved (not in the patch).
```

**Negative depth** — "I want to update top-level fields but leave nested state alone, even if the caller accidentally includes deeper data":

```
target: {"user": {"name": "Alice", "prefs": {"theme": "dark"}}, "scalar": "old"}
patch:  {"user": {"prefs": {"theme": "light"}}, "scalar": "new"}
?depth=-1
result: {"user": {"name": "Alice", "prefs": {"theme": "dark"}}, "scalar": "new"}
                  ▲
              user is PRESERVED — patch's user object is ignored entirely.
              "scalar" is updated (scalars always merge regardless of sign).
```

Negative depth is a defensive primitive: it lets a service that knows it should only update shallow fields enforce that on the server, so a buggy or malicious client can't accidentally rewrite a nested subtree.

### Counting levels

`depth=N` means **N levels of merge actually happen**. The outer merge of the patch into the target counts as 1.

- `depth=1`: only top-level keys merge. Their values replace (positive) or are preserved (negative).
- `depth=2`: top-level merges, plus one recursion into object values. Level-3 objects replace/preserve.
- `depth=3`: three levels of merging happen. Level-4 objects replace/preserve.
- `depth=0`: no levels of merging — the patch is written as the new document.

---

## Examples

All examples assume `BASE=http://localhost:6830` and `JWT=$(...)` set to a valid bearer token.

### Update a single field

```bash
# Stored: {"name": "Alice", "age": 30}
curl -X PATCH "$BASE/files/user.json" \
  -H "Authorization: Bearer $JWT" \
  -H "Content-Type: application/merge-patch+json" \
  -d '{"age": 31}'
# Result: {"name": "Alice", "age": 31}
```

### Delete a field

```bash
# Stored: {"name": "Alice", "email": "a@x", "phone": "555-1212"}
curl -X PATCH "$BASE/files/user.json" \
  -H "Authorization: Bearer $JWT" \
  -H "Content-Type: application/merge-patch+json" \
  -d '{"phone": null}'
# Result: {"name": "Alice", "email": "a@x"}
```

### Update a nested field without disturbing siblings

```bash
# Stored: {"user": {"name": "Alice", "prefs": {"theme": "dark", "lang": "en"}}}
curl -X PATCH "$BASE/files/user.json" \
  -H "Authorization: Bearer $JWT" \
  -H "Content-Type: application/merge-patch+json" \
  -d '{"user": {"prefs": {"theme": "light"}}}'
# Result: {"user": {"name": "Alice", "prefs": {"theme": "light", "lang": "en"}}}
```

### Swap a subtree atomically (`?depth=+1`)

When you want to replace `prefs` wholesale rather than merging:

```bash
# Stored: {"user": {"name": "Alice", "prefs": {"theme": "dark", "lang": "en"}}}
curl -X PATCH "$BASE/files/user.json?depth=1" \
  -H "Authorization: Bearer $JWT" \
  -H "Content-Type: application/merge-patch+json" \
  -d '{"user": {"prefs": {"theme": "light"}}}'
# Result: {"user": {"prefs": {"theme": "light"}}}
#                 ▲ user.name is gone (user object was replaced wholesale)
```

### Protect nested state from accidental writes (`?depth=-1`)

Useful in a service that exposes shallow user-profile updates but never wants its API to touch nested session/credential blobs even if a caller includes them by mistake:

```bash
# Stored: {"profile": {"name": "Alice"}, "credentials": {"token": "secret"}}
curl -X PATCH "$BASE/files/user.json?depth=-1" \
  -H "Authorization: Bearer $JWT" \
  -H "Content-Type: application/merge-patch+json" \
  -d '{"profile": {"name": "Bob"}, "credentials": {"token": "compromised"}}'
# Result: {"profile": {"name": "Alice"}, "credentials": {"token": "secret"}}
#         ▲ Both nested objects are preserved — the patch's depths are ignored.
```

Note that scalars at the top level still update — `?depth=-1` is "shallow merge that doesn't touch the depths," not "noop."

### Create a new document via PATCH

```bash
# /new.json does not exist
curl -X PATCH "$BASE/files/new.json" \
  -H "Authorization: Bearer $JWT" \
  -H "Content-Type: application/merge-patch+json" \
  -d '{"hello": "world"}'
# Status: 201 Created
# Stored: {"hello": "world"}
```

### Wholesale replace (`?depth=0` — equivalent to PUT)

```bash
curl -X PATCH "$BASE/files/doc.json?depth=0" \
  -H "Authorization: Bearer $JWT" \
  -H "Content-Type: application/merge-patch+json" \
  -d '{"replaced": true}'
# Stored: {"replaced": true}  (whatever was there before is gone)
```

In practice you should just use `PUT` for this. `?depth=0` exists for callers that compute the depth dynamically and want the boundary to degrade cleanly.

---

## Concurrency

### Safe by default

The engine's write path is single-threaded — writes are serialized through the write buffer and WAL. Two concurrent merge-patches to the same file are applied **one at a time** by the server. Each merge reads the latest persisted state (including the other writer's prior merge), applies its own patch on top, and writes.

This means:

- **Disjoint key updates compose correctly.** Writer A's `{"a": 1}` and Writer B's `{"b": 2}` arriving simultaneously yield `{"a": 1, "b": 2}` regardless of which runs first.
- **Overlapping key updates are last-writer-wins.** This is inherent to merge semantics — there's no per-key clock or vector to detect concurrency. If two writers both update `"theme"`, the second to be applied wins.

### Compare-and-swap is not currently supported

The endpoint does not yet honor `If-Match: <content-hash>`. If you have a strict ordering requirement (e.g., "fail if anyone modified this file since I last read it"), do a `GET` first, capture the `hash`, then `PUT` with the new document and compare. We may add an opt-in CAS header in a future revision; track [the followup issue] if you need it.

### When you should still GET first

- You need to make a decision based on the current state ("only set `published` to `true` if `draft_count >= 1`"). The server can't conditionally merge, only blindly merge.
- You need to read the result of the merge — the response body is metadata only, not the merged content. Do a `GET` after.

---

## Limits

| Limit | Value | Behavior on exceed |
|---|---|---|
| Patch body size | 10 MB | `413 Payload Too Large` |
| Stored file size (post-merge target must fit in memory) | 10 MB | `413 Payload Too Large` |
| Path is under `/.aeordb-system/` | — | `404 Not Found` |
| Patch body not valid JSON | — | `415 Unsupported Media Type` |
| Stored file present but not valid JSON | — | `415 Unsupported Media Type` |

These caps apply because the engine has to hold both the existing document and the patched result in memory simultaneously. If you have records larger than 10 MB, split them into multiple files and merge each independently.

---

## Migration from read-modify-write

If you have existing client code like this:

```javascript
// OLD: 3 round trips, race-prone
const resp = await fetch(`${base}/files/user.json`, { headers });
const doc = await resp.json();
doc.prefs.theme = 'light';
await fetch(`${base}/files/user.json`, {
  method: 'PUT',
  headers: { ...headers, 'Content-Type': 'application/json' },
  body: JSON.stringify(doc),
});
```

The migration is mechanical:

```javascript
// NEW: 1 round trip, race-safe under server-side merge
await fetch(`${base}/files/user.json`, {
  method: 'PATCH',
  headers: { ...headers, 'Content-Type': 'application/merge-patch+json' },
  body: JSON.stringify({ prefs: { theme: 'light' } }),
});
```

Pick a `depth` mode based on what your code path expects:

| Your code did this | Use this depth |
|---|---|
| Mutate a deeply-nested field | (unset) — strict RFC 7396 |
| Replace an entire subtree by value | `?depth=+N` matching the subtree depth |
| Update only shallow fields, never touch nested state | `?depth=-N` matching how far to allow merges |
| Replace the whole document | `?depth=0` (or just use `PUT`) |

---

## Implementation checklist for SDKs

A minimal client wrapper should:

- [ ] Expose `mergePatch(path, patch, { depth })` returning the response metadata.
- [ ] Set `Content-Type: application/merge-patch+json` (no other variant — the dispatcher uses this exact string to discriminate from rename).
- [ ] Encode `depth` as a signed query-string integer when provided. `depth=0` is legal; `depth=-1` is legal; omit the query param entirely for unbounded.
- [ ] Handle `201` and `200` as success. Both have the same body shape.
- [ ] Surface `413` and `415` distinctly from generic `4xx`; they typically indicate a configuration bug (wrong content-type on the stored file, oversize doc) rather than a transient failure.
- [ ] Do **not** retry on `4xx`. Merge patches are not idempotent in general — retrying a delete-via-null after a successful first call would re-apply against the now-changed state.
- [ ] Document explicitly that arrays replace wholesale and `null` deletes, so users don't get surprised.
