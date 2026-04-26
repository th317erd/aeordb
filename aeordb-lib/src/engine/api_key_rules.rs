use serde::{Deserialize, Serialize};

/// A single path-to-permission rule for an API key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyRule {
    pub glob: String,
    pub permitted: String,
}

/// Flag positions in the permitted string: c-r-u-d-l-i-f-y
pub const FLAG_CREATE: usize = 0;
pub const FLAG_READ: usize = 1;
pub const FLAG_UPDATE: usize = 2;
pub const FLAG_DELETE: usize = 3;
pub const FLAG_LIST: usize = 4;
pub const FLAG_INVOKE: usize = 5;
pub const FLAG_FUNCTIONS: usize = 6;
pub const FLAG_CONFIGURE: usize = 7;
pub const FLAGS_LENGTH: usize = 8;
pub const FLAGS_FULL: &str = "crudlify";
pub const FLAGS_NONE: &str = "--------";

/// The expected character at each flag position (when enabled).
const FLAG_CHARS: [char; FLAGS_LENGTH] = ['c', 'r', 'u', 'd', 'l', 'i', 'f', 'y'];

/// Parse rules from JSON array format: [{ "/path/**": "flags" }, ...]
pub fn parse_rules_from_json(value: &serde_json::Value) -> Result<Vec<KeyRule>, String> {
    let array = value
        .as_array()
        .ok_or_else(|| "Rules must be a JSON array".to_string())?;

    let mut rules = Vec::with_capacity(array.len());

    for (index, element) in array.iter().enumerate() {
        let object = element
            .as_object()
            .ok_or_else(|| format!("Rule at index {} must be a JSON object", index))?;

        if object.len() != 1 {
            return Err(format!(
                "Rule at index {} must have exactly one key (the glob pattern), found {}",
                index,
                object.len()
            ));
        }

        let (glob, flags_value) = object.iter().next().unwrap();

        if glob.is_empty() {
            return Err(format!("Rule at index {} has an empty glob pattern", index));
        }

        let flags = flags_value
            .as_str()
            .ok_or_else(|| format!("Rule at index {}: flags value must be a string", index))?;

        validate_flags(flags).map_err(|err| format!("Rule at index {}: {}", index, err))?;

        rules.push(KeyRule {
            glob: glob.clone(),
            permitted: flags.to_string(),
        });
    }

    Ok(rules)
}

/// Validate a permission flags string (must be exactly 8 chars, each position
/// is the expected letter or '-').
pub fn validate_flags(flags: &str) -> Result<(), String> {
    if flags.len() != FLAGS_LENGTH {
        return Err(format!(
            "Flags string must be exactly {} characters, got {}",
            FLAGS_LENGTH,
            flags.len()
        ));
    }

    for (position, character) in flags.chars().enumerate() {
        let expected = FLAG_CHARS[position];
        if character != expected && character != '-' {
            return Err(format!(
                "Invalid character '{}' at position {}: expected '{}' or '-'",
                character, position, expected
            ));
        }
    }

    Ok(())
}

/// Maximum allowed length for a single glob pattern (bytes).
const MAX_GLOB_PATTERN_LENGTH: usize = 1024;

/// Validate a list of rules (all globs non-empty, within length limits, all flags valid).
pub fn validate_rules(rules: &[KeyRule]) -> Result<(), String> {
    for rule in rules {
        if rule.glob.is_empty() {
            return Err("Rule glob pattern cannot be empty".to_string());
        }
        if rule.glob.len() > MAX_GLOB_PATTERN_LENGTH {
            return Err(format!(
                "Glob pattern too long: {} chars (max {})",
                rule.glob.len(),
                MAX_GLOB_PATTERN_LENGTH
            ));
        }
        validate_flags(&rule.permitted)?;
    }
    Ok(())
}

/// Match a path against an ordered list of rules. First match wins.
/// Returns None if no rule matches.
pub fn match_rules<'a>(rules: &'a [KeyRule], path: &str) -> Option<&'a KeyRule> {
    for rule in rules {
        if glob_match::glob_match(&rule.glob, path) {
            return Some(rule);
        }
    }
    None
}

/// Check if `path` is an ancestor directory of any rule's target path.
///
/// Example: rules contain `/projects/alpha/docs/report.pdf`.
/// Path `/projects/alpha/` is an ancestor → returns true.
/// Path `/photos/` is NOT an ancestor → returns false.
///
/// This enables scoped keys to navigate the directory tree down to their
/// target paths without granting access to sibling directories.
pub fn is_ancestor_of_any_rule(rules: &[KeyRule], path: &str) -> bool {
    let normalized = path.trim_end_matches('/');
    for rule in rules {
        // Skip the deny-all fallback
        if rule.glob == "**" { continue; }
        let target = rule.glob.trim_end_matches('/').trim_end_matches("**").trim_end_matches('/');
        // Check if the target path starts with the request path
        if target.starts_with(normalized) && (target.len() == normalized.len() || target.as_bytes().get(normalized.len()) == Some(&b'/')) {
            return true;
        }
    }
    false
}

/// Check if a listing item is on the path to any rule target.
///
/// Used by directory listing filters to show only items that lead to
/// scoped paths — no sibling exposure.
///
/// Example: rules contain `/projects/alpha/docs/report.pdf`.
/// Item `/projects/alpha/docs` → YES (on the path)
/// Item `/projects/beta` → NO (sibling, not on the path)
/// Item `/projects/alpha/docs/report.pdf` → YES (exact target)
pub fn is_item_on_shared_path(rules: &[KeyRule], item_path: &str) -> bool {
    let normalized_item = item_path.trim_end_matches('/');
    for rule in rules {
        if rule.glob == "**" { continue; }
        let target = rule.glob.trim_end_matches('/').trim_end_matches("**").trim_end_matches('/');
        // Item is the target itself or a descendant of the target
        if glob_match::glob_match(&rule.glob, item_path) {
            return true;
        }
        // Item is an ancestor of the target (on the path to the shared item)
        if target.starts_with(normalized_item) && (target.len() == normalized_item.len() || target.as_bytes().get(normalized_item.len()) == Some(&b'/')) {
            return true;
        }
    }
    false
}

/// Check if a specific operation is permitted by a flags string.
pub fn check_operation_permitted(permitted: &str, operation: char) -> bool {
    let index = match operation {
        'c' => FLAG_CREATE,
        'r' => FLAG_READ,
        'u' => FLAG_UPDATE,
        'd' => FLAG_DELETE,
        'l' => FLAG_LIST,
        'i' => FLAG_INVOKE,
        'f' => FLAG_FUNCTIONS,
        'y' => FLAG_CONFIGURE,
        _ => return false,
    };
    permitted
        .chars()
        .nth(index)
        .map(|ch| ch != '-')
        .unwrap_or(false)
}

/// Map a CrudlifyOp to its flag character.
pub fn operation_to_flag_char(op: &crate::engine::permission_resolver::CrudlifyOp) -> char {
    use crate::engine::permission_resolver::CrudlifyOp;
    match op {
        CrudlifyOp::Create => 'c',
        CrudlifyOp::Read => 'r',
        CrudlifyOp::Update => 'u',
        CrudlifyOp::Delete => 'd',
        CrudlifyOp::List => 'l',
        CrudlifyOp::Invoke => 'i',
        CrudlifyOp::Configure => 'y',
        CrudlifyOp::Deploy => 'f',
    }
}

#[cfg(test)]
mod ancestor_tests {
    use super::*;

    #[test]
    fn test_is_ancestor_of_file_rule() {
        let rules = vec![
            KeyRule { glob: "/test/subdir/deep/file.txt".to_string(), permitted: "-r--l---".to_string() },
            KeyRule { glob: "**".to_string(), permitted: "--------".to_string() },
        ];
        assert!(is_ancestor_of_any_rule(&rules, "/test/subdir/deep/"), "immediate parent");
        assert!(is_ancestor_of_any_rule(&rules, "/test/subdir/"), "grandparent");
        assert!(is_ancestor_of_any_rule(&rules, "/test/"), "great-grandparent");
        assert!(is_ancestor_of_any_rule(&rules, "/"), "root");
        assert!(!is_ancestor_of_any_rule(&rules, "/other/"), "sibling dir");
        // The exact target also counts as "on the path" (starts_with self)
        assert!(is_ancestor_of_any_rule(&rules, "/test/subdir/deep/file.txt"), "exact target is on path");
    }

    #[test]
    fn test_is_item_on_path() {
        let rules = vec![
            KeyRule { glob: "/test/subdir/deep/file.txt".to_string(), permitted: "-r--l---".to_string() },
            KeyRule { glob: "**".to_string(), permitted: "--------".to_string() },
        ];
        assert!(is_item_on_shared_path(&rules, "/test/subdir/deep"), "dir on path");
        assert!(is_item_on_shared_path(&rules, "/test/subdir"), "parent dir on path");
        assert!(is_item_on_shared_path(&rules, "/test"), "grandparent on path");
        assert!(!is_item_on_shared_path(&rules, "/other"), "sibling NOT on path");
        assert!(!is_item_on_shared_path(&rules, "/test/other"), "sibling subdir NOT on path");
        assert!(is_item_on_shared_path(&rules, "/test/subdir/deep/file.txt"), "exact target");
    }
}
