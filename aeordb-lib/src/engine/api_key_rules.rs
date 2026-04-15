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

/// Validate a list of rules (all globs non-empty, all flags valid).
pub fn validate_rules(rules: &[KeyRule]) -> Result<(), String> {
    for rule in rules {
        if rule.glob.is_empty() {
            return Err("Rule glob pattern cannot be empty".to_string());
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
