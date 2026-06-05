//! JSON Merge Patch (RFC 7396) with optional depth bound.
//!
//! Strict RFC 7396 is unconditionally recursive: an object value in the
//! patch is merged into the corresponding object in the target, all the
//! way down. We extend the spec with an optional signed depth bound:
//!
//!   * `MergeDepth::Unbounded` — strict RFC 7396 (no parameter, the default).
//!   * `MergeDepth::FullReplace` — wholesale `*target = patch` (PUT
//!     semantics; the `?depth=0` spelling resolves here).
//!   * `MergeDepth::ReplaceBeyond(n)` — `?depth=+n` — merge for `n`
//!     levels; deeper object values in the patch REPLACE the target's
//!     subtree wholesale.
//!   * `MergeDepth::PreserveBeyond(n)` — `?depth=-n` — merge for `n`
//!     levels; deeper object values in the patch are IGNORED (the
//!     target's existing subtree is left untouched).
//!
//! Scalars and `null` in the patch always behave the same regardless
//! of sign — `null` deletes the key, scalars insert/replace at the
//! current level. The signed-depth distinction only fires for an
//! *object* value at a depth past the budget: do we let the patch
//! win (replace) or let the target win (preserve)?
//!
//! `n = 0` for either signed variant is degenerate: with `Replace` it
//! means "no merge levels, just replace the whole document," which is
//! exactly `FullReplace`. With `Preserve` it means "no merge levels,
//! preserve everything" — a no-op. The handler maps the `?depth=0`
//! spelling onto `FullReplace`; the `n=0` cases of the signed variants
//! are still legal and handled here for completeness.

use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeDepth {
  Unbounded,
  FullReplace,
  ReplaceBeyond(u32),
  PreserveBeyond(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BeyondPolicy {
  Replace,
  Preserve,
}

#[derive(Debug, Clone, Copy)]
enum ActiveDepth {
  Unbounded,
  Bounded { levels: u32, beyond: BeyondPolicy },
}

impl ActiveDepth {
  fn descend(self) -> Self {
    match self {
      ActiveDepth::Unbounded => ActiveDepth::Unbounded,
      ActiveDepth::Bounded { levels, beyond } => ActiveDepth::Bounded { levels: levels.saturating_sub(1), beyond },
    }
  }

  fn allows_merge(self) -> bool {
    match self {
      ActiveDepth::Unbounded => true,
      ActiveDepth::Bounded { levels, .. } => levels > 0,
    }
  }

  fn beyond(self) -> BeyondPolicy {
    match self {
      ActiveDepth::Unbounded => BeyondPolicy::Replace,
      ActiveDepth::Bounded { beyond, .. } => beyond,
    }
  }
}

/// Apply `patch` to `target` per the rules above. Mutates `target` in
/// place when both are objects; otherwise replaces `*target = patch`.
pub fn apply_merge_patch(target: &mut Value, patch: Value, depth: MergeDepth) {
  // `FullReplace` is the PUT-via-PATCH path — overwrite the document.
  if matches!(depth, MergeDepth::FullReplace) {
    *target = patch;
    return;
  }

  let active = match depth {
    MergeDepth::Unbounded => ActiveDepth::Unbounded,
    MergeDepth::FullReplace => unreachable!("handled above"),
    MergeDepth::ReplaceBeyond(n) => ActiveDepth::Bounded { levels: n, beyond: BeyondPolicy::Replace },
    MergeDepth::PreserveBeyond(n) => ActiveDepth::Bounded { levels: n, beyond: BeyondPolicy::Preserve },
  };

  // Zero merge budget — degenerate cases. Replace policy = wholesale
  // document replace (same as FullReplace); Preserve policy = no-op.
  if !active.allows_merge() {
    match active.beyond() {
      BeyondPolicy::Replace => {
        *target = patch;
      }
      BeyondPolicy::Preserve => { /* leave target alone */ }
    }
    return;
  }

  let patch_obj = match patch {
    Value::Object(o) => o,
    other => {
      // Patch is non-object at the top — RFC 7396 says it replaces
      // the whole target. (For PreserveBeyond, we'd have already
      // bailed if budget=0; with budget≥1 we still honor the spec.)
      *target = other;
      return;
    }
  };

  // Patch is an object. Ensure target is an object too; if not, RFC 7396
  // says the patch wins (target becomes an empty object first). For
  // PreserveBeyond mode we still need a place to merge top-level keys.
  if !target.is_object() {
    *target = Value::Object(Map::new());
  }
  let target_obj = target.as_object_mut().expect("just set to object");

  apply_object_merge(target_obj, patch_obj, active);
}

fn apply_object_merge(target: &mut Map<String, Value>, patch: Map<String, Value>, depth: ActiveDepth) {
  // This call performs one level of merge. `next_depth` is what remains
  // for any recursion into object-valued children.
  let next_depth = depth.descend();
  for (key, patch_value) in patch {
    match patch_value {
      Value::Null => {
        // null deletes at the current merge level regardless of policy.
        target.remove(&key);
      }
      Value::Object(patch_obj) => {
        if next_depth.allows_merge() {
          let entry = target.entry(key.clone()).or_insert(Value::Object(Map::new()));
          if !entry.is_object() {
            *entry = Value::Object(Map::new());
          }
          let sub_target = entry.as_object_mut().expect("just set to object");
          apply_object_merge(sub_target, patch_obj, next_depth);
        } else {
          // Budget exhausted: object value at the boundary. Policy
          // controls what happens here.
          match depth.beyond() {
            BeyondPolicy::Replace => {
              target.insert(key, Value::Object(patch_obj));
            }
            BeyondPolicy::Preserve => {
              // Leave target[key] alone. If target has no such key,
              // we deliberately do NOT add it — the policy is
              // "don't touch the depths."
            }
          }
        }
      }
      other => {
        // Non-null scalars and arrays insert/replace at the merge
        // level regardless of policy — the sign-distinguished
        // behavior only applies to object values past the boundary.
        target.insert(key, other);
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  fn merged(target: Value, patch: Value, depth: MergeDepth) -> Value {
    let mut t = target;
    apply_merge_patch(&mut t, patch, depth);
    t
  }

  #[test]
  fn rfc7396_basic_object_merge() {
    let out = merged(json!({"a": 1, "b": 2}), json!({"b": 20, "c": 3}), MergeDepth::Unbounded);
    assert_eq!(out, json!({"a": 1, "b": 20, "c": 3}));
  }

  #[test]
  fn rfc7396_null_deletes_key() {
    let out = merged(json!({"a": 1, "b": 2}), json!({"b": null}), MergeDepth::Unbounded);
    assert_eq!(out, json!({"a": 1}));
  }

  #[test]
  fn rfc7396_recursive_object_merge() {
    let out = merged(
      json!({"user": {"name": "Alice", "prefs": {"theme": "dark", "lang": "en"}}}),
      json!({"user": {"prefs": {"theme": "light"}}}),
      MergeDepth::Unbounded,
    );
    assert_eq!(out, json!({"user": {"name": "Alice", "prefs": {"theme": "light", "lang": "en"}}}));
  }

  #[test]
  fn rfc7396_arrays_replace_wholesale() {
    let out = merged(json!({"tags": [1, 2, 3]}), json!({"tags": [4, 5]}), MergeDepth::Unbounded);
    assert_eq!(out, json!({"tags": [4, 5]}));
  }

  #[test]
  fn rfc7396_scalar_patch_replaces_whole_target() {
    let out = merged(json!({"a": 1}), json!("hello"), MergeDepth::Unbounded);
    assert_eq!(out, json!("hello"));
  }

  #[test]
  fn rfc7396_null_at_top_level_replaces_target() {
    let out = merged(json!({"a": 1}), Value::Null, MergeDepth::Unbounded);
    assert_eq!(out, Value::Null);
  }

  #[test]
  fn rfc7396_array_patch_replaces_target() {
    let out = merged(json!({"a": 1}), json!([1, 2, 3]), MergeDepth::Unbounded);
    assert_eq!(out, json!([1, 2, 3]));
  }

  #[test]
  fn rfc7396_target_non_object_becomes_object_when_patch_is_object() {
    let out = merged(json!(42), json!({"a": 1}), MergeDepth::Unbounded);
    assert_eq!(out, json!({"a": 1}));
  }

  #[test]
  fn replace_beyond_1_replaces_subtrees_wholesale() {
    // Top-level keys merge, but their object values replace.
    let out = merged(
      json!({"user": {"name": "Alice", "prefs": {"theme": "dark"}}, "session": "abc"}),
      json!({"user": {"prefs": {"theme": "light"}}}),
      MergeDepth::ReplaceBeyond(1),
    );
    // user is replaced wholesale (no name preservation), session preserved.
    assert_eq!(out, json!({"user": {"prefs": {"theme": "light"}}, "session": "abc"}));
  }

  #[test]
  fn replace_beyond_2_merges_top_then_replaces_at_level_3() {
    // depth=+2: 2 levels of merge. Top keys merge AND one recursion
    // into user. target.user.prefs is at level 3, beyond budget, so
    // prefs REPLACES (lang lost).
    let out = merged(
      json!({"user": {"name": "Alice", "prefs": {"theme": "dark", "lang": "en"}}, "session": "abc"}),
      json!({"user": {"prefs": {"theme": "light"}}}),
      MergeDepth::ReplaceBeyond(2),
    );
    assert_eq!(
      out,
      json!({
        "user": {"name": "Alice", "prefs": {"theme": "light"}},
        "session": "abc"
      })
    );
  }

  #[test]
  fn replace_beyond_3_merges_three_levels() {
    let out = merged(
      json!({"user": {"name": "Alice", "prefs": {"theme": "dark", "lang": "en"}}}),
      json!({"user": {"prefs": {"theme": "light"}}}),
      MergeDepth::ReplaceBeyond(3),
    );
    assert_eq!(out, json!({"user": {"name": "Alice", "prefs": {"theme": "light", "lang": "en"}}}));
  }

  #[test]
  fn replace_beyond_2_keep_lost() {
    let out = merged(
      json!({"a": {"b": {"c": {"d": "old", "keep": "yes"}}}}),
      json!({"a": {"b": {"c": {"d": "new"}}}}),
      MergeDepth::ReplaceBeyond(2),
    );
    assert_eq!(out, json!({"a": {"b": {"c": {"d": "new"}}}}));
  }

  #[test]
  fn full_replace_overwrites_document() {
    let out = merged(json!({"a": 1, "b": 2}), json!({"c": 3}), MergeDepth::FullReplace);
    assert_eq!(out, json!({"c": 3}));
  }

  #[test]
  fn full_replace_with_scalar_overwrites() {
    let out = merged(json!({"a": 1, "b": 2}), json!("hello"), MergeDepth::FullReplace);
    assert_eq!(out, json!("hello"));
  }

  #[test]
  fn null_deletion_works_within_depth_bound() {
    let out = merged(
      json!({"user": {"name": "Alice", "email": "a@x"}, "session": "abc"}),
      json!({"user": {"email": null}, "session": null}),
      MergeDepth::ReplaceBeyond(2),
    );
    assert_eq!(out, json!({"user": {"name": "Alice"}}));
  }

  #[test]
  fn null_deletion_replace_mode_keeps_null_in_subtree() {
    // At depth=+1, target.user REPLACES wholesale. A null inside the
    // replacement persists in the result (it doesn't delete a sibling
    // — the whole subtree is just the patch verbatim). Top-level null
    // DOES delete `session` because that key is at the merged level.
    let out = merged(
      json!({"user": {"name": "Alice", "email": "a@x"}, "session": "abc"}),
      json!({"user": {"email": null}, "session": null}),
      MergeDepth::ReplaceBeyond(1),
    );
    assert_eq!(out, json!({"user": {"email": null}}));
  }

  // ─────────────────────────────────────────────────────────────────────
  // PreserveBeyond — negative depth: object values beyond the budget
  // are LEFT ALONE (patch's deeper objects are ignored).
  // ─────────────────────────────────────────────────────────────────────

  #[test]
  fn preserve_beyond_1_leaves_nested_objects_alone() {
    // depth=-1: top keys merge, but object values at level 2 are
    // preserved. Patch's `user` object is IGNORED entirely; target.user
    // stays intact.
    let out = merged(
      json!({"user": {"name": "Alice", "prefs": {"theme": "dark"}}, "session": "abc"}),
      json!({"user": {"prefs": {"theme": "light"}}, "session": "xyz"}),
      MergeDepth::PreserveBeyond(1),
    );
    // user unchanged (object value at the boundary is preserved);
    // session is a scalar so it still updates.
    assert_eq!(
      out,
      json!({
        "user": {"name": "Alice", "prefs": {"theme": "dark"}},
        "session": "xyz",
      })
    );
  }

  #[test]
  fn preserve_beyond_2_merges_two_levels_then_preserves() {
    // depth=-2: 2 levels of merge happen (user keys merge), but
    // target.user.prefs is an object at level 3 → PRESERVE.
    let out = merged(
      json!({"user": {"name": "Alice", "prefs": {"theme": "dark", "lang": "en"}}}),
      json!({"user": {"name": "Bob", "prefs": {"theme": "light"}}}),
      MergeDepth::PreserveBeyond(2),
    );
    assert_eq!(
      out,
      json!({
        "user": {"name": "Bob", "prefs": {"theme": "dark", "lang": "en"}},
      })
    );
  }

  #[test]
  fn preserve_beyond_1_null_still_deletes_at_merge_level() {
    // null at the merge level deletes regardless of policy. Object
    // values at the boundary preserve.
    let out = merged(
      json!({"keep_me": {"a": 1}, "delete_me": "x", "scalar": 1}),
      json!({"keep_me": {"a": 99}, "delete_me": null, "scalar": 2}),
      MergeDepth::PreserveBeyond(1),
    );
    assert_eq!(
      out,
      json!({
        "keep_me": {"a": 1},
        "scalar": 2,
      })
    );
  }

  #[test]
  fn preserve_beyond_1_does_not_create_missing_subtrees() {
    // Patch tries to introduce a NEW object key beyond the boundary.
    // Preserve policy means we don't touch the depths — including not
    // creating new ones from patch.
    let out = merged(json!({"existing": "v"}), json!({"new_nested": {"a": 1}}), MergeDepth::PreserveBeyond(1));
    assert_eq!(out, json!({"existing": "v"}));
  }

  #[test]
  fn preserve_beyond_0_is_noop() {
    let out = merged(json!({"a": 1, "b": 2}), json!({"a": 99, "c": 3}), MergeDepth::PreserveBeyond(0));
    // Budget=0 with preserve = leave everything alone.
    assert_eq!(out, json!({"a": 1, "b": 2}));
  }

  #[test]
  fn replace_beyond_0_is_equivalent_to_full_replace() {
    // ReplaceBeyond(0) = "merge zero levels, then replace beyond." With
    // no merge happening at all, the semantics collapse to wholesale
    // document replace, identical to FullReplace. Existing target keys
    // not in the patch are LOST.
    let out = merged(json!({"a": 1, "b": 2}), json!({"a": 99, "c": 3}), MergeDepth::ReplaceBeyond(0));
    assert_eq!(out, json!({"a": 99, "c": 3}));
  }

  #[test]
  fn target_path_missing_creates_object() {
    let out = merged(json!({}), json!({"new": {"nested": {"key": "value"}}}), MergeDepth::Unbounded);
    assert_eq!(out, json!({"new": {"nested": {"key": "value"}}}));
  }
}
