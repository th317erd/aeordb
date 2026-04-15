use aeordb::engine::api_key_rules::{
    check_operation_permitted, match_rules, operation_to_flag_char, parse_rules_from_json,
    validate_flags, validate_rules, KeyRule, FLAGS_FULL, FLAGS_NONE,
};
use aeordb::engine::permission_resolver::CrudlifyOp;

// ===========================================================================
// match_rules tests
// ===========================================================================

#[test]
fn test_first_match_wins() {
    let rules = vec![
        KeyRule {
            glob: "/a/**".to_string(),
            permitted: "cr------".to_string(),
        },
        KeyRule {
            glob: "**".to_string(),
            permitted: "--------".to_string(),
        },
    ];
    let matched = match_rules(&rules, "/a/b");
    assert!(matched.is_some());
    let rule = matched.unwrap();
    assert_eq!(rule.glob, "/a/**");
    assert_eq!(rule.permitted, "cr------");
}

#[test]
fn test_no_match_returns_none() {
    let rules = vec![KeyRule {
        glob: "/x/**".to_string(),
        permitted: "crudlify".to_string(),
    }];
    let matched = match_rules(&rules, "/y/z");
    assert!(matched.is_none());
}

#[test]
fn test_glob_star_star_matches_all() {
    let rules = vec![KeyRule {
        glob: "**".to_string(),
        permitted: "crudlify".to_string(),
    }];
    let matched = match_rules(&rules, "/any/thing");
    assert!(matched.is_some());
    assert_eq!(matched.unwrap().permitted, "crudlify");
}

#[test]
fn test_glob_specific_path() {
    let rules = vec![KeyRule {
        glob: "/assets/logo.psd".to_string(),
        permitted: "-r------".to_string(),
    }];
    let matched = match_rules(&rules, "/assets/logo.psd");
    assert!(matched.is_some());
    assert_eq!(matched.unwrap().permitted, "-r------");
}

#[test]
fn test_glob_wildcard_extension() {
    let rules = vec![KeyRule {
        glob: "/assets/*.psd".to_string(),
        permitted: "-r------".to_string(),
    }];
    // Should match .psd
    let matched = match_rules(&rules, "/assets/logo.psd");
    assert!(matched.is_some());
    assert_eq!(matched.unwrap().permitted, "-r------");

    // Should NOT match .png
    let no_match = match_rules(&rules, "/assets/logo.png");
    assert!(no_match.is_none());
}

#[test]
fn test_match_rules_empty_rules() {
    let rules: Vec<KeyRule> = vec![];
    assert!(match_rules(&rules, "/anything").is_none());
}

#[test]
fn test_match_rules_order_matters() {
    // More restrictive rule first, then catch-all
    let rules = vec![
        KeyRule {
            glob: "/admin/**".to_string(),
            permitted: "--------".to_string(),
        },
        KeyRule {
            glob: "/admin/public/**".to_string(),
            permitted: "crudlify".to_string(),
        },
        KeyRule {
            glob: "**".to_string(),
            permitted: "-r------".to_string(),
        },
    ];
    // /admin/public/x matches the first rule (deny-all) because first match wins
    let matched = match_rules(&rules, "/admin/public/x");
    assert!(matched.is_some());
    assert_eq!(matched.unwrap().permitted, "--------");
}

// ===========================================================================
// check_operation_permitted tests
// ===========================================================================

#[test]
fn test_check_operation_permitted_full() {
    let flags = FLAGS_FULL;
    for ch in ['c', 'r', 'u', 'd', 'l', 'i', 'f', 'y'] {
        assert!(
            check_operation_permitted(flags, ch),
            "Expected '{}' to be permitted with flags '{}'",
            ch,
            flags
        );
    }
}

#[test]
fn test_check_operation_denied_all() {
    let flags = FLAGS_NONE;
    for ch in ['c', 'r', 'u', 'd', 'l', 'i', 'f', 'y'] {
        assert!(
            !check_operation_permitted(flags, ch),
            "Expected '{}' to be denied with flags '{}'",
            ch,
            flags
        );
    }
}

#[test]
fn test_check_operation_partial() {
    let flags = "-r--l---";
    assert!(!check_operation_permitted(flags, 'c'));
    assert!(check_operation_permitted(flags, 'r'));
    assert!(!check_operation_permitted(flags, 'u'));
    assert!(!check_operation_permitted(flags, 'd'));
    assert!(check_operation_permitted(flags, 'l'));
    assert!(!check_operation_permitted(flags, 'i'));
    assert!(!check_operation_permitted(flags, 'f'));
    assert!(!check_operation_permitted(flags, 'y'));
}

#[test]
fn test_check_operation_unknown_char_returns_false() {
    assert!(!check_operation_permitted("crudlify", 'z'));
    assert!(!check_operation_permitted("crudlify", 'x'));
    assert!(!check_operation_permitted("crudlify", '!'));
}

#[test]
fn test_check_operation_short_string_returns_false() {
    // If the permitted string is too short, chars().nth() returns None -> false
    assert!(!check_operation_permitted("cr", 'y'));
}

// ===========================================================================
// parse_rules_from_json tests
// ===========================================================================

#[test]
fn test_parse_rules_from_json_valid() {
    let json = serde_json::json!([
        {"/assets/**": "-r--l---"},
        {"**": "--------"}
    ]);
    let rules = parse_rules_from_json(&json).unwrap();
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0].glob, "/assets/**");
    assert_eq!(rules[0].permitted, "-r--l---");
    assert_eq!(rules[1].glob, "**");
    assert_eq!(rules[1].permitted, "--------");
}

#[test]
fn test_parse_rules_from_json_single_rule() {
    let json = serde_json::json!([
        {"/data/**": "crudlify"}
    ]);
    let rules = parse_rules_from_json(&json).unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].glob, "/data/**");
    assert_eq!(rules[0].permitted, "crudlify");
}

#[test]
fn test_parse_rules_from_json_empty_array() {
    let json = serde_json::json!([]);
    let rules = parse_rules_from_json(&json).unwrap();
    assert!(rules.is_empty());
}

#[test]
fn test_parse_rules_from_json_invalid_flags() {
    let json = serde_json::json!([
        {"/path/**": "crud"}
    ]);
    let result = parse_rules_from_json(&json);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("8 characters"), "Error: {}", err);
}

#[test]
fn test_parse_rules_from_json_invalid_char() {
    let json = serde_json::json!([
        {"/path/**": "xr------"}
    ]);
    let result = parse_rules_from_json(&json);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.contains("Invalid character") && err.contains("position 0"),
        "Error: {}",
        err
    );
}

#[test]
fn test_parse_rules_empty_glob() {
    let json = serde_json::json!([
        {"": "crudlify"}
    ]);
    let result = parse_rules_from_json(&json);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("empty glob"), "Error: {}", err);
}

#[test]
fn test_parse_rules_from_json_not_array() {
    let json = serde_json::json!({"glob": "flags"});
    let result = parse_rules_from_json(&json);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("array"));
}

#[test]
fn test_parse_rules_from_json_element_not_object() {
    let json = serde_json::json!(["not an object"]);
    let result = parse_rules_from_json(&json);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("object"));
}

#[test]
fn test_parse_rules_from_json_multiple_keys_in_object() {
    let json = serde_json::json!([
        {"/a/**": "crudlify", "/b/**": "--------"}
    ]);
    let result = parse_rules_from_json(&json);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("exactly one key"));
}

#[test]
fn test_parse_rules_from_json_flags_not_string() {
    let json = serde_json::json!([
        {"/path/**": 12345}
    ]);
    let result = parse_rules_from_json(&json);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("string"));
}

// ===========================================================================
// validate_rules tests
// ===========================================================================

#[test]
fn test_validate_rules_valid() {
    let rules = vec![
        KeyRule {
            glob: "/a/**".to_string(),
            permitted: "crudlify".to_string(),
        },
        KeyRule {
            glob: "**".to_string(),
            permitted: "--------".to_string(),
        },
    ];
    assert!(validate_rules(&rules).is_ok());
}

#[test]
fn test_validate_rules_empty_is_valid() {
    let rules: Vec<KeyRule> = vec![];
    assert!(validate_rules(&rules).is_ok());
}

#[test]
fn test_validate_rules_empty_glob_fails() {
    let rules = vec![KeyRule {
        glob: "".to_string(),
        permitted: "crudlify".to_string(),
    }];
    let result = validate_rules(&rules);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("empty"));
}

#[test]
fn test_validate_rules_bad_flags_fails() {
    let rules = vec![KeyRule {
        glob: "/a/**".to_string(),
        permitted: "invalid!".to_string(),
    }];
    let result = validate_rules(&rules);
    assert!(result.is_err());
}

// ===========================================================================
// validate_flags tests
// ===========================================================================

#[test]
fn test_validate_flags_valid() {
    assert!(validate_flags("crudlify").is_ok());
    assert!(validate_flags("-r--l---").is_ok());
    assert!(validate_flags("--------").is_ok());
    assert!(validate_flags("c-------").is_ok());
    assert!(validate_flags("-------y").is_ok());
    assert!(validate_flags("cr-dl-f-").is_ok());
}

#[test]
fn test_validate_flags_wrong_length() {
    assert!(validate_flags("crud").is_err());
    assert!(validate_flags("").is_err());
    assert!(validate_flags("crudlifyz").is_err()); // 9 chars
    assert!(validate_flags("crudlif").is_err()); // 7 chars
}

#[test]
fn test_validate_flags_wrong_position_char() {
    // Position 0 expects 'c' or '-', not 'x'
    assert!(validate_flags("xrudlify").is_err());
    // Position 1 expects 'r' or '-', not 'c'
    assert!(validate_flags("ccudlify").is_err());
    // Position 7 expects 'y' or '-', not 'f'
    assert!(validate_flags("crudliff").is_err());
    // All wrong letters
    assert!(validate_flags("abcdefgh").is_err());
}

#[test]
fn test_validate_flags_each_position_independently() {
    // Each valid flag character at its correct position
    let valid_combos = [
        "c-------", "-r------", "--u-----", "---d----",
        "----l---", "-----i--", "------f-", "-------y",
    ];
    for combo in &valid_combos {
        assert!(validate_flags(combo).is_ok(), "Expected '{}' to be valid", combo);
    }

    // Wrong character at each position
    let invalid_combos = [
        ("r-------", 0, 'r'),
        ("-c------", 1, 'c'),
        ("--r-----", 2, 'r'),
        ("---c----", 3, 'c'),
        ("----c---", 4, 'c'),
        ("-----c--", 5, 'c'),
        ("------c-", 6, 'c'),
        ("-------c", 7, 'c'),
    ];
    for (flags, pos, ch) in &invalid_combos {
        let result = validate_flags(flags);
        assert!(
            result.is_err(),
            "Expected '{}' to be invalid (pos {}, char '{}')",
            flags, pos, ch
        );
    }
}

// ===========================================================================
// operation_to_flag_char tests
// ===========================================================================

#[test]
fn test_operation_to_flag_char_all_ops() {
    assert_eq!(operation_to_flag_char(&CrudlifyOp::Create), 'c');
    assert_eq!(operation_to_flag_char(&CrudlifyOp::Read), 'r');
    assert_eq!(operation_to_flag_char(&CrudlifyOp::Update), 'u');
    assert_eq!(operation_to_flag_char(&CrudlifyOp::Delete), 'd');
    assert_eq!(operation_to_flag_char(&CrudlifyOp::List), 'l');
    assert_eq!(operation_to_flag_char(&CrudlifyOp::Invoke), 'i');
    assert_eq!(operation_to_flag_char(&CrudlifyOp::Deploy), 'f');
    assert_eq!(operation_to_flag_char(&CrudlifyOp::Configure), 'y');
}

#[test]
fn test_operation_roundtrip() {
    // For every CrudlifyOp, converting to flag char and checking against
    // FLAGS_FULL should always be permitted.
    let ops = [
        CrudlifyOp::Create,
        CrudlifyOp::Read,
        CrudlifyOp::Update,
        CrudlifyOp::Delete,
        CrudlifyOp::List,
        CrudlifyOp::Invoke,
        CrudlifyOp::Deploy,
        CrudlifyOp::Configure,
    ];
    for op in &ops {
        let flag_char = operation_to_flag_char(op);
        assert!(
            check_operation_permitted(FLAGS_FULL, flag_char),
            "Op {:?} with flag '{}' should be permitted in full flags",
            op,
            flag_char
        );
    }
}

// ===========================================================================
// KeyRule serialization tests
// ===========================================================================

#[test]
fn test_key_rule_serialization_roundtrip() {
    let rule = KeyRule {
        glob: "/data/**".to_string(),
        permitted: "cr--l---".to_string(),
    };
    let json = serde_json::to_string(&rule).unwrap();
    let deserialized: KeyRule = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.glob, rule.glob);
    assert_eq!(deserialized.permitted, rule.permitted);
}

#[test]
fn test_key_rule_vec_serialization() {
    let rules = vec![
        KeyRule {
            glob: "/a/**".to_string(),
            permitted: "crudlify".to_string(),
        },
        KeyRule {
            glob: "**".to_string(),
            permitted: "--------".to_string(),
        },
    ];
    let json = serde_json::to_string(&rules).unwrap();
    let deserialized: Vec<KeyRule> = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.len(), 2);
    assert_eq!(deserialized[0].glob, "/a/**");
    assert_eq!(deserialized[1].permitted, "--------");
}

// ===========================================================================
// Integration-style: parse then match then check
// ===========================================================================

#[test]
fn test_full_pipeline_parse_match_check() {
    let json = serde_json::json!([
        {"/assets/**": "-r--l---"},
        {"/admin/**": "--------"},
        {"**": "-r------"}
    ]);
    let rules = parse_rules_from_json(&json).unwrap();

    // /assets/image.png -> first rule, read + list allowed
    let rule = match_rules(&rules, "/assets/image.png").unwrap();
    assert!(check_operation_permitted(&rule.permitted, 'r'));
    assert!(check_operation_permitted(&rule.permitted, 'l'));
    assert!(!check_operation_permitted(&rule.permitted, 'c'));
    assert!(!check_operation_permitted(&rule.permitted, 'd'));

    // /admin/settings -> second rule, nothing allowed
    let rule = match_rules(&rules, "/admin/settings").unwrap();
    assert!(!check_operation_permitted(&rule.permitted, 'r'));
    assert!(!check_operation_permitted(&rule.permitted, 'c'));

    // /other/path -> third rule, only read
    let rule = match_rules(&rules, "/other/path").unwrap();
    assert!(check_operation_permitted(&rule.permitted, 'r'));
    assert!(!check_operation_permitted(&rule.permitted, 'l'));
}
