//! Source text and normalized expected recipes are deliberately separate.
use std::path::Path;
use serde_json::{Value, json};
use crate::oracle::{self, Case, InvalidCase, Recipe, Selector};

struct Sample {
  case: Case,
  canonical_owner: String,
  glob: Option<String>,
  pins: Vec<(String, u8)>,
  recipes: Option<Vec<Recipe>>,
}

fn sample(name: &str, source: Option<Value>, recipes: Option<Vec<Recipe>>) -> Sample {
  Sample {
    case: Case {
      name: name.into(),
      owner: "/".into(),
      registry: None,
      source: source.map(|value| value.to_string()),
      fingerprint: 0x42,
      expected: Vec::new(),
    },
    canonical_owner: "/".into(),
    glob: None,
    pins: Vec::new(),
    recipes,
  }
}

fn configuration(rows: Value) -> Value {
  json!({"$v":1,"indexes":rows})
}
fn row(name: &str, converters: &[u16]) -> Value {
  json!({"name":name,"type":converters.iter().map(|id|oracle::CONVERTERS[*id as usize-1]).collect::<Vec<_>>()})
}

pub fn valid(root: &Path) -> Vec<Case> {
  let mut samples =
    vec![sample("absent-registry", None, None), sample("empty-configuration", Some(configuration(json!([]))), Some(Vec::new()))];
  let mut empty = sample("explicit-empty-registry", None, None);
  empty.case.registry = Some(r#"{"$v":1,"parsers":{}}"#.into());
  samples.push(empty);
  for (name, source) in [
    ("registry-canonical", r#"{"$v":1,"parsers":{"application/pdf":"p","text/plain":"second"}}"#),
    ("registry-case-order-alias", r#"{ "parsers": {"TEXT/PLAIN":"second", "APPLICATION/PDF":"renamed"}, "$v":1 }"#),
  ] {
    let mut registry = sample(name, None, None);
    registry.case.registry = Some(source.into());
    registry.pins = vec![("application/pdf".into(), 0x42), ("text/plain".into(), 0x43)];
    samples.push(registry);
  }
  let defaults = [
    ("@content_type", vec![1]),
    ("@created_at", vec![7]),
    ("@extension", vec![3]),
    ("@filename", vec![3, 9, 10, 11, 12]),
    ("@hash", vec![1]),
    ("@path", vec![3, 9]),
    ("@size", vec![4]),
    ("@updated_at", vec![7]),
    ("metadata.duration", vec![6]),
    ("metadata.format", vec![3]),
    ("text", vec![9]),
    ("title", vec![3, 9]),
  ];
  let mut rows = Vec::new();
  let mut recipes = Vec::new();
  for (name, converters) in defaults {
    let mut row = row(name, &converters);
    let mut recipe = oracle::recipe(name, &converters);
    if name.starts_with("metadata.") {
      let child = if name == "metadata.duration" { "duration_seconds" } else { "format" };
      row["source"] = json!(["metadata", child]);
      recipe.selector = Selector::Path(vec![oracle::key("metadata"), oracle::key(child)]);
    }
    rows.push(row);
    recipes.push(recipe);
  }
  let mut source = configuration(json!(rows));
  source["glob"] = json!("**/*");
  let mut default = sample("complete-bootstrap", Some(source), Some(recipes));
  default.glob = Some("**/*".into());
  samples.push(default);
  for converter in 1..=12 {
    samples.push(sample(
      &format!("converter-{converter}"),
      Some(configuration(json!([row("x", &[converter])]))),
      Some(vec![oracle::recipe("x", &[converter])]),
    ));
  }
  for (name, raw_owner, glob, expected_glob) in
    [("scope-normalized", "  /data//./child/../items\0 ", None, None), ("scope-recursive", "/data/items", Some("//**///x"), Some("**/x"))]
  {
    let mut source = configuration(json!([]));
    if let Some(glob) = glob {
      source["glob"] = json!(glob);
    }
    let mut value = sample(name, Some(source), Some(vec![]));
    value.case.owner = raw_owner.into();
    value.canonical_owner = "/data/items".into();
    value.glob = expected_glob.map(str::to_string);
    samples.push(value);
  }
  let mut alias = sample(
    "metadata-alias-set-duplicates",
    Some(configuration(json!([
      {"name":"@file_name","type":["unicode_trigram_v1","utf8_binary_order_v1","utf8_binary_order_v1"]},
      {"name":"@filename","type":"unicode_trigram_v1"},
    ]))),
    Some(vec![oracle::recipe("@filename", &[3, 9])]),
  );
  let mut source: Value = serde_json::from_str(alias.case.source.as_ref().unwrap()).unwrap();
  source["logging"] = json!(true);
  source["compression"] = json!("zstd");
  alias.case.source = Some(source.to_string());
  samples.push(alias);
  for (name, source, selector) in [
    ("source-root", json!([]), Selector::Path(vec![])),
    (
      "source-key-index-fanout-regex",
      json!(["items", u64::MAX, "", "/^name$/i"]),
      Selector::Path(vec![oracle::key("items"), (2, 0, u64::MAX.to_le_bytes().to_vec()), (3, 0, vec![]), (4, 1, b"^name$".to_vec())]),
    ),
    ("source-invalid-regex-literal", json!(["/[/"]), Selector::Path(vec![oracle::key("/[/")])),
    ("source-explicit-field", json!(["x"]), Selector::Path(vec![oracle::key("x")])),
  ] {
    let mut row = row("x", &[1]);
    row["source"] = source;
    let mut recipe = oracle::recipe("x", &[1]);
    recipe.selector = selector;
    samples.push(sample(name, Some(configuration(json!([row]))), Some(vec![recipe])));
  }
  for (name, explicit, mapper, registry) in [
    ("explicit-parser", true, false, false),
    ("automatic-mapper", false, true, false),
    ("same-module-two-roles", true, true, false),
    ("automatic-registry", false, false, true),
    ("registry-mapper", false, true, true),
  ] {
    let mut row = row("x", &[1]);
    let mut recipe = oracle::recipe("x", &[1]);
    recipe.explicit = explicit;
    if mapper {
      row["source"] = json!({"plugin":"m"});
      oracle::mapper(&mut recipe, oracle::null());
    }
    let mut source = configuration(json!([row]));
    if explicit {
      source["parser"] = json!("p");
    }
    let mut value = sample(name, Some(source), Some(vec![recipe]));
    if registry {
      value.case.registry = Some(r#"{"$v":1,"parsers":{"text/plain":"second","application/pdf":"p"}}"#.into());
      value.pins = vec![("application/pdf".into(), 0x42), ("text/plain".into(), 0x43)];
    }
    samples.push(value);
  }
  for (name, arguments) in [
    ("mapper-explicit-null", json!(null)),
    ("mapper-empty-object", json!({})),
    ("mapper-ordered-array", json!([true, false, -1, 1, u64::MAX, 1.5, "é", null])),
    ("mapper-reordered-array", json!([null, "é", 1.5, u64::MAX, 1, -1, false, true])),
    ("mapper-canonical-map", json!({"z":[2,1],"a":{"é":null,"x":true}})),
  ] {
    let mut input = row("x", &[1]);
    input["source"] = json!({"plugin":"m","args":arguments});
    let mut recipe = oracle::recipe("x", &[1]);
    oracle::mapper(&mut recipe, oracle::arguments(&arguments));
    samples.push(sample(name, Some(configuration(json!([input]))), Some(vec![recipe])));
  }
  for (name, rows, recipes) in [
    ("unused-explicit-parser-empty", json!([]), vec![]),
    ("unused-explicit-parser-metadata", json!([row("@hash", &[1])]), vec![oracle::recipe("@hash", &[1])]),
  ] {
    let mut source = configuration(rows);
    source["parser"] = json!("missing");
    samples.push(sample(name, Some(source), Some(recipes)));
  }
  let mut explicit_defaults = configuration(json!([row("x", &[1])]));
  let default_recipe = oracle::recipe("x", &[1]);
  for (group, names, values) in [
    ("source_limits", oracle::SOURCE_NAMES.as_slice(), default_recipe.source_limits.as_slice()),
    ("converter_limits", oracle::CONVERTER_NAMES.as_slice(), default_recipe.converter_limits.as_slice()),
    ("field_limits", oracle::FIELD_NAMES.as_slice(), default_recipe.field_limits.as_slice()),
  ] {
    for (name, value) in names.iter().zip(values) {
      explicit_defaults["indexes"][0][group][*name] = json!(value);
    }
  }
  for (tier, values) in [("wasm", oracle::policy(true)), ("raw_json", oracle::policy(false)), ("native_suite", oracle::policy(false))] {
    for (name, value) in oracle::POLICY_NAMES.iter().zip(values) {
      explicit_defaults["parser_policies"][tier][*name] = json!(value);
    }
  }
  samples.push(sample("explicit-all-defaults", Some(explicit_defaults), Some(vec![default_recipe])));
  let mut pinned =
    sample("changed-artifact", Some(json!({"$v":1,"parser":"p","indexes":[row("x",&[1])]})), Some(vec![oracle::recipe("x", &[1])]));
  pinned.case.fingerprint = 0x51;
  pinned.recipes.as_mut().unwrap()[0].explicit = true;
  samples.push(pinned);
  // Every property gets a distinct non-default number so swapping equal-default
  // fields cannot satisfy this corpus. Expected offsets are assigned by index,
  // not looked up from the source schema or production typed definitions.
  for (group, names, values) in [
    ("source_limits", oracle::SOURCE_NAMES.as_slice(), vec![513, 4_000_003, 32_000_007, 500_009, 16_000_011]),
    ("converter_limits", oracle::CONVERTER_NAMES.as_slice(), vec![500_001, 32003, 250_007, 2_000_009]),
    ("field_limits", oracle::FIELD_NAMES.as_slice(), vec![31001, 32003, 4_000_007, 3_000_011]),
  ] {
    for (index, name) in names.iter().enumerate() {
      let mut row = row("x", &[1]);
      row[group] = json!({*name:values[index]});
      let mut recipe = oracle::recipe("x", &[1]);
      match group {
        "source_limits" => recipe.source_limits[index] = values[index],
        "converter_limits" => recipe.converter_limits[index] = values[index],
        _ => recipe.field_limits[index] = values[index],
      }
      samples.push(sample(&format!("{group}-{name}"), Some(configuration(json!([row]))), Some(vec![recipe])));
    }
  }
  let changes = [32 << 20, 8_000_003, 32 << 20, 9_000_007, 32009, 32011, 500_013, 17, 32017, 2, 3, 4, 2017, 127];
  for (tier, policy_index) in [("wasm", 0), ("raw_json", 1), ("native_suite", 2), ("mapper", 3), ("registry", 0)] {
    for (index, name) in oracle::POLICY_NAMES.iter().enumerate() {
      let is_wasm = matches!(tier, "wasm" | "mapper" | "registry");
      if !is_wasm && oracle::policy(false)[index] == 0 {
        continue;
      }
      let mut row = row("x", &[1]);
      let mut recipe = oracle::recipe("x", &[1]);
      recipe.explicit = tier == "wasm";
      let mut source = configuration(json!([]));
      if tier == "mapper" {
        row["source"] = json!({"plugin":"m","policy":{*name:changes[index]}});
        oracle::mapper(&mut recipe, oracle::null());
      } else {
        source["parser_policies"][if tier == "registry" { "wasm" } else { tier }] = json!({*name:changes[index]});
      }
      if tier == "wasm" {
        source["parser"] = json!("p");
      }
      source["indexes"] = json!([row]);
      recipe.policies[policy_index][index] = changes[index];
      let mut value = sample(&format!("policy-{tier}-{name}"), Some(source), Some(vec![recipe]));
      if tier == "registry" {
        value.case.registry = Some(r#"{"$v":1,"parsers":{"text/plain":"p"}}"#.into());
        value.pins = vec![("text/plain".into(), 0x42)];
      }
      samples.push(value);
    }
  }
  let mut memory_alias = sample(
    "parser-memory-alias",
    Some(json!({"$v":1,"parser":"p","parser_memory_limit":" 32 MB ","indexes":[row("x",&[1])]})),
    Some(vec![oracle::recipe("x", &[1])]),
  );
  let recipe = &mut memory_alias.recipes.as_mut().unwrap()[0];
  recipe.explicit = true;
  recipe.policies[0][2] = 32 << 20;
  samples.push(memory_alias);
  samples
    .into_iter()
    .map(|mut sample| {
      let pins: Vec<_> = sample.pins.iter().map(|(name, fingerprint)| (name.as_str(), *fingerprint)).collect();
      for algorithm in 1..=5 {
        sample.case.expected.push(oracle::expected(
          root,
          algorithm,
          &sample.canonical_owner,
          sample.glob.as_deref(),
          &pins,
          sample.recipes.as_deref(),
          sample.case.fingerprint,
        ));
      }
      sample.case
    })
    .collect()
}

pub fn invalid() -> Vec<InvalidCase> {
  let mut cases = Vec::new();
  for (registry, sources) in [
    (
      false,
      vec![
        "",
        "null",
        "[]",
        "{}",
        r#"{"indexes":[]}"#,
        r#"{"$v":0,"indexes":[]}"#,
        r#"{"$v":1.0,"indexes":[]}"#,
        r#"{"$v":1,"$v":1,"indexes":[]}"#,
        r#"{"$v":1,"indexes":[],"indexes":[]}"#,
        r#"{"$v":1,"indexes":null}"#,
        r#"{"$v":1,"indexes":[],"unknown":1}"#,
        r#"{"$v":1,"indexes":[],"glob":null}"#,
        r#"{"$v":1,"indexes":[],"glob":"a/../b"}"#,
        r#"{"$v":1,"indexes":[],"parser":null}"#,
        r#"{"$v":1,"indexes":[],"parser":""}"#,
        r#"{"$v":1,"indexes":[],"logging":1}"#,
        r#"{"$v":1,"indexes":[],"compression":false}"#,
        r#"{"$v":1,"indexes":[{"name":"x","type":"hash"}]}"#,
        r#"{"$v":1,"indexes":[{"name":"x","type":[]}]}"#,
        r#"{"$v":1,"indexes":[{"name":"x","type":"typed_exact_blake3_v1","min":0}]}"#,
        r#"{"$v":1,"indexes":[{"name":"@hash","type":"typed_exact_blake3_v1","source":[]}]}"#,
        r#"{"$v":1,"indexes":[{"name":"x","type":"typed_exact_blake3_v1","source":{"plugin":"m","args":{"a":1,"a":2}}}]}"#,
        r#"{"$v":1,"indexes":[]} {}"#,
      ],
    ),
    (
      true,
      vec![
        "",
        "null",
        "[]",
        "{}",
        r#"{"parsers":{}}"#,
        r#"{"$v":0,"parsers":{}}"#,
        r#"{"$v":1,"parsers":null}"#,
        r#"{"$v":1,"parsers":{"text/plain":"p","TEXT/PLAIN":"m"}}"#,
        r#"{"$v":1,"parsers":{"text/plain; charset=utf8":"p"}}"#,
        r#"{"$v":1,"parsers":{"text/plain":null}}"#,
        r#"{"$v":1,"parsers":{"bad":"p"}}"#,
        r#"{"$v":1,"parsers":{},"unknown":1}"#,
      ],
    ),
  ] {
    for (index, source) in sources.into_iter().enumerate() {
      cases.push(InvalidCase {
        name: format!("schema-{registry}-{index}"),
        registry,
        source: source.into(),
        outcome: "InvalidSource".into(),
      });
    }
  }
  for (group, names) in [
    ("source_limits", oracle::SOURCE_NAMES.as_slice()),
    ("converter_limits", oracle::CONVERTER_NAMES.as_slice()),
    ("field_limits", oracle::FIELD_NAMES.as_slice()),
  ] {
    for name in names {
      for value in [json!(null), json!(-1), json!(1.0), json!("1"), json!(0), json!(u64::MAX)] {
        let mut row = row("x", &[1]);
        row[group] = json!({*name:value});
        cases.push(InvalidCase {
          name: format!("{group}-{name}-{value}"),
          registry: false,
          source: configuration(json!([row])).to_string(),
          outcome: "InvalidSource".into(),
        });
      }
    }
  }
  for tier in ["wasm", "raw_json", "native_suite", "mapper"] {
    for name in oracle::POLICY_NAMES {
      for value in [json!(null), json!(-1), json!(1.0), json!(u64::MAX)] {
        let mut row = row("x", &[1]);
        let mut source = configuration(json!([]));
        if tier == "mapper" {
          row["source"] = json!({"plugin":"m","policy":{name:value}});
        } else {
          source["parser_policies"][tier] = json!({name:value});
        }
        source["indexes"] = json!([row]);
        cases.push(InvalidCase {
          name: format!("policy-{tier}-{name}-{value}"),
          registry: false,
          source: source.to_string(),
          outcome: "InvalidSource".into(),
        });
      }
    }
  }
  for (registry, source) in [
    (true, r#"{"$v":1,"parsers":{"text/plain":"missing"}}"#),
    (false, r#"{"$v":1,"parser":"missing","indexes":[{"name":"x","type":"typed_exact_blake3_v1"}]}"#),
    (false, r#"{"$v":1,"indexes":[{"name":"x","type":"typed_exact_blake3_v1","source":{"plugin":"missing"}}]}"#),
  ] {
    cases.push(InvalidCase {
      name: format!("missing-{}", cases.len()),
      registry,
      source: source.into(),
      outcome: "DependencyUnavailable".into(),
    });
  }
  cases
}
