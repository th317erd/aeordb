//! Independent fixture construction. Never import the AeorDB compiler/codecs.
//! Inputs and normalized recipes are authored separately; production output is
//! compared with these bytes, never used to update them.
use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::Digest;

pub const FILES: [&str; 4] = ["SPEC.md", "invalid.bin", "properties.json", "vectors.bin"];
pub const DOMAIN: &[u8] = b"aeordb.semantic-compiler-profile.v1\0";
pub const CONVERTERS: [&str; 12] = [
  "typed_exact_blake3_v1",
  "bytes_binary_order_v1",
  "utf8_binary_order_v1",
  "u64_order_v1",
  "i64_order_v1",
  "f64_finite_order_v1",
  "timestamp_ms_order_v1",
  "bool_order_v1",
  "unicode_trigram_v1",
  "soundex_ascii_v1",
  "double_metaphone_primary_ascii_v1",
  "double_metaphone_alt_ascii_v1",
];
pub const POLICY_NAMES: [&str; 14] = [
  "max_request_bytes",
  "max_response_bytes",
  "max_linear_memory_bytes",
  "max_fuel",
  "max_table_elements",
  "max_structure_nodes",
  "max_scalar_bytes",
  "max_structure_depth",
  "max_container_members",
  "max_wasm_instances",
  "max_wasm_memories",
  "max_wasm_tables",
  "max_value_stack_height",
  "max_recursion_depth",
];
pub const SOURCE_NAMES: [&str; 5] = [
  "max_source_values_per_document",
  "max_canonical_source_bytes_per_document",
  "max_document_input_bytes",
  "max_selector_work_items_per_document",
  "max_selector_examined_bytes_per_document",
];
pub const CONVERTER_NAMES: [&str; 4] = ["max_input_bytes", "max_output_values", "max_output_value_bytes", "max_total_output_bytes"];
pub const FIELD_NAMES: [&str; 4] =
  ["max_terms_per_document", "max_postings_per_document", "max_canonical_posting_bytes_per_document", "max_query_recheck_value_bytes"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Object {
  pub class: u16,
  pub identity: String,
  pub bytes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Field {
  pub name: String,
  pub value: Object,
  pub indexes: Vec<Object>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Expected {
  pub algorithm: u16,
  pub registry: Object,
  pub scope: Option<Object>,
  pub projection: Option<Object>,
  pub fields: Vec<Field>,
  pub dependencies: Vec<Object>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Case {
  pub name: String,
  pub owner: String,
  pub registry: Option<String>,
  pub source: Option<String>,
  pub fingerprint: u8,
  pub expected: Vec<Expected>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvalidCase {
  pub name: String,
  pub registry: bool,
  pub source: String,
  pub outcome: String,
}

pub fn digest(algorithm: u16, bytes: &[u8]) -> Vec<u8> {
  match algorithm {
    1 => blake3::hash(bytes).as_bytes().to_vec(),
    2 => sha2::Sha256::digest(bytes).to_vec(),
    3 => sha2::Sha512::digest(bytes).to_vec(),
    4 => sha3::Sha3_256::digest(bytes).to_vec(),
    5 => sha3::Sha3_512::digest(bytes).to_vec(),
    _ => panic!("unregistered fixture algorithm"),
  }
}

pub fn fingerprint(algorithm: u16, files: &[Vec<u8>; 4]) -> String {
  let mut bytes = DOMAIN.to_vec();
  for file in files {
    bytes.extend_from_slice(&(file.len() as u64).to_le_bytes());
    bytes.extend_from_slice(file);
  }
  hex::encode(digest(algorithm, &bytes))
}

pub fn packet<T: Serialize>(magic: &[u8; 4], cases: &[T]) -> Vec<u8> {
  let mut bytes = magic.to_vec();
  bytes.extend_from_slice(&(cases.len() as u32).to_le_bytes());
  for case in cases {
    let record = serde_json::to_vec(case).unwrap();
    bytes.extend_from_slice(&(record.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&record);
  }
  bytes
}

pub fn unpack<T: serde::de::DeserializeOwned>(magic: &[u8; 4], bytes: &[u8]) -> Vec<T> {
  assert_eq!(&bytes[..4], magic);
  let count = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
  let mut position = 8;
  let mut cases = Vec::new();
  for _ in 0..count {
    let length = u32::from_le_bytes(bytes[position..position + 4].try_into().unwrap()) as usize;
    position += 4;
    cases.push(serde_json::from_slice(&bytes[position..position + length]).unwrap());
    position += length;
  }
  assert_eq!(position, bytes.len());
  cases
}

fn put16(bytes: &mut [u8], offset: usize, value: u16) {
  bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}
fn put32(bytes: &mut [u8], offset: usize, value: usize) {
  bytes[offset..offset + 4].copy_from_slice(&u32::try_from(value).unwrap().to_le_bytes());
}
fn put64(bytes: &mut [u8], offset: usize, value: u64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn envelope(magic: &[u8; 4], length: usize, header: u16) -> Vec<u8> {
  let mut bytes = vec![0; length];
  bytes[..4].copy_from_slice(magic);
  put16(&mut bytes, 4, 1);
  put16(&mut bytes, 6, header);
  put32(&mut bytes, 8, length);
  bytes
}

fn object(algorithm: u16, class: u16, bytes: Vec<u8>) -> Object {
  let domain: &[u8] = match class {
    1 => b"aeordb.semantic.effective-index-config-projection.v1\0",
    2 => b"aeordb.semantic.parser-registry-projection.v1\0",
    3 => b"aeordb.index.scope-definition.v1\0",
    4 => b"aeordb.index.value-store-definition.v1\0",
    5 => b"aeordb.index.field-definition.v1\0",
    6 => b"aeordb.semantic.executable-dependency-definition.v1\0",
    7 => b"aeordb.semantic.native-dependency-definition.v1\0",
    _ => panic!("unknown class"),
  };
  Object { class, identity: hex::encode(digest(algorithm, &[domain, &bytes].concat())), bytes: hex::encode(bytes) }
}

fn frame(tag: u8, payload: &[u8]) -> Vec<u8> {
  let mut bytes = vec![tag];
  bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
  bytes.extend_from_slice(payload);
  bytes
}

fn map(members: BTreeMap<String, Vec<u8>>) -> Vec<u8> {
  let mut bytes = (members.len() as u32).to_le_bytes().to_vec();
  for (key, value) in members {
    bytes.extend_from_slice(&(key.len() as u32).to_le_bytes());
    bytes.extend_from_slice(key.as_bytes());
    bytes.extend_from_slice(&value);
  }
  frame(10, &bytes)
}

fn array(values: &[Vec<u8>]) -> Vec<u8> {
  let mut bytes = (values.len() as u32).to_le_bytes().to_vec();
  for value in values {
    bytes.extend_from_slice(value);
  }
  frame(9, &bytes)
}

pub fn policy(wasm: bool) -> [u64; 14] {
  if wasm {
    [64 << 20, 16 << 20, 64 << 20, 10_000_000, 65536, 65536, 1 << 20, 32, 65535, 1, 1, 1, 4096, 256]
  } else {
    [0, 16 << 20, 0, 0, 0, 65536, 1 << 20, 32, 65535, 0, 0, 0, 4096, 256]
  }
}

fn policy_bytes(values: &[u64; 14], wasm: bool) -> Vec<u8> {
  let mut bytes = envelope(b"AIVP", 128, 128);
  for (offset, value) in [(16, if wasm { 2 } else { 1 }), (18, u16::from(wasm)), (20, 1), (22, 1)] {
    put16(&mut bytes, offset, value);
  }
  for (index, value) in values.iter().enumerate() {
    if index < 7 {
      put64(&mut bytes, 24 + 8 * index, *value);
    } else {
      put32(&mut bytes, 80 + 4 * (index - 7), *value as usize);
    }
  }
  bytes
}

#[derive(Clone)]
struct Dependency {
  kind: u16,
  role: u16,
  name: &'static str,
  fingerprint: [u8; 32],
}

fn wasm(role: u16, fingerprint: u8) -> Dependency {
  Dependency { kind: 1, role, name: "/org/example/shared", fingerprint: [fingerprint; 32] }
}

fn natives() -> Vec<Dependency> {
  [
    (1, "/org/aeordev/aeordb/native/native-suite-v1", "ed952f3f8514d9bfb6f6879348e9b3d243cc9714c583ce20fbe59cf24121d8bb"),
    (1, "/org/aeordev/aeordb/native/raw-json-v1", "25a57279983b4dbc4ffd4dc6c3576623d5d2f9fd0e1bf3a9c8709a80ae304a68"),
    (3, "/org/aeordev/aeordb/native/mime-router-v1", "c72d35d74d0bcadc28ace70b6392d1699f9a803a14928852bf6e6b9cb290f729"),
    (4, "/org/aeordev/aeordb/native/aeor-regex-v1", "eae50800c658f804c4bda0d0b20331399db77f078545759f2f20f52b88fe34d6"),
  ]
  .into_iter()
  .map(|(role, name, fingerprint)| Dependency { kind: 2, role, name, fingerprint: hex::decode(fingerprint).unwrap().try_into().unwrap() })
  .collect()
}

fn dependency_bytes(dependency: &Dependency) -> Vec<u8> {
  let executable = dependency.kind == 1;
  let version = if executable { "1.2.3" } else { "1.0.0" };
  let mut bytes = vec![0; 96];
  put32(&mut bytes, 0, 96 + dependency.name.len() + version.len());
  for (offset, value) in [
    (4, dependency.kind),
    (6, dependency.role),
    (12, if executable { dependency.role + 2 } else { 0 }),
    (14, if executable { 2 } else { 1 }),
    (16, if executable { 1 } else { 2 }),
    (18, u16::from(executable)),
  ] {
    put16(&mut bytes, offset, value);
  }
  put32(&mut bytes, 8, if executable { 4 } else { 0 });
  put32(&mut bytes, 20, dependency.name.len());
  put32(&mut bytes, 24, version.len());
  put64(&mut bytes, 32, if executable { 123 } else { 0 });
  bytes[40..72].copy_from_slice(&dependency.fingerprint);
  bytes.extend_from_slice(dependency.name.as_bytes());
  bytes.extend_from_slice(version.as_bytes());
  bytes
}

#[derive(Clone)]
pub enum Selector {
  Metadata(u16),
  Path(Vec<(u8, u8, Vec<u8>)>),
  Mapper(Vec<u8>),
}

pub fn key(value: &str) -> (u8, u8, Vec<u8>) {
  (1, 0, value.as_bytes().to_vec())
}

#[derive(Clone)]
pub struct Recipe {
  pub name: String,
  pub selector: Selector,
  pub converters: Vec<u16>,
  pub source_limits: [u64; 5],
  pub converter_limits: [u64; 4],
  pub field_limits: [u64; 4],
  pub policies: [[u64; 14]; 4],
  pub explicit: bool,
}

pub fn recipe(name: &str, converters: &[u16]) -> Recipe {
  let metadata = ["@path", "@filename", "@extension", "@content_type", "@size", "@created_at", "@updated_at", "@hash"]
    .iter()
    .position(|value| *value == name);
  Recipe {
    name: name.into(),
    selector: match metadata {
      Some(index) => Selector::Metadata(index as u16 + 1),
      None => Selector::Path(vec![key(name)]),
    },
    converters: converters.to_vec(),
    source_limits: if metadata.is_some() { [1024, 8 << 20, 0, 0, 0] } else { [1024, 8 << 20, 64 << 20, 1_000_000, 64 << 20] },
    converter_limits: [1 << 20, 65536, 1 << 20, 4 << 20],
    field_limits: [65536, 65536, 8 << 20, 8 << 20],
    policies: [policy(true), policy(false), policy(false), policy(true)],
    explicit: false,
  }
}

pub fn mapper(recipe: &mut Recipe, arguments: Vec<u8>) {
  recipe.selector = Selector::Mapper(arguments);
  recipe.source_limits[3] = 0;
  recipe.source_limits[4] = 0;
}

pub fn null() -> Vec<u8> {
  frame(1, &[])
}

pub fn arguments(value: &serde_json::Value) -> Vec<u8> {
  use serde_json::Value;
  match value {
    Value::Null => null(),
    Value::Bool(false) => frame(2, &[]),
    Value::Bool(true) => frame(3, &[]),
    Value::Number(number) => {
      if let Some(value) = number.as_i64() {
        return frame(4, &value.to_le_bytes());
      }
      if let Some(value) = number.as_u64() {
        return frame(5, &value.to_le_bytes());
      }
      let value = number.as_f64().unwrap();
      frame(6, &if value == 0.0 { 0u64 } else { value.to_bits() }.to_le_bytes())
    }
    Value::String(value) => frame(7, value.as_bytes()),
    Value::Array(values) => array(&values.iter().map(arguments).collect::<Vec<_>>()),
    Value::Object(values) => map(values.iter().map(|(key, value)| (key.clone(), arguments(value))).collect()),
  }
}

fn context(recipe: &Recipe, registry: &[(&str, u8)], fingerprint: u8) -> (Vec<u8>, Vec<u8>, Vec<Dependency>, u32) {
  let metadata = matches!(recipe.selector, Selector::Metadata(_));
  let mut dependencies = Vec::new();
  if !metadata {
    if recipe.explicit {
      dependencies.push(wasm(1, fingerprint));
    } else {
      for (_, fingerprint) in registry {
        dependencies.push(wasm(1, *fingerprint));
      }
      dependencies.extend(natives().into_iter().take(3));
    }
    match recipe.selector {
      Selector::Mapper(_) => dependencies.push(wasm(2, fingerprint)),
      Selector::Path(_) => dependencies.push(natives().pop().unwrap()),
      Selector::Metadata(_) => unreachable!(),
    }
  }
  dependencies.sort_by(|left, right| {
    (left.kind, left.role, left.name, left.fingerprint).cmp(&(right.kind, right.role, right.name, right.fingerprint))
  });
  dependencies.dedup_by(|left, right| dependency_bytes(left) == dependency_bytes(right));
  let ordinal = |dependency: &Dependency| {
    (dependencies.iter().position(|candidate| dependency_bytes(candidate) == dependency_bytes(dependency)).unwrap() + 1) as u32
  };
  let selector_ordinal = match recipe.selector {
    Selector::Metadata(_) => 0,
    Selector::Path(_) => ordinal(&natives().pop().unwrap()),
    Selector::Mapper(_) => ordinal(&wasm(2, fingerprint)),
  };
  let mut table = envelope(b"ADPT", 32, 32);
  put32(&mut table, 16, dependencies.len());
  for dependency in &dependencies {
    table.extend_from_slice(&dependency_bytes(dependency));
  }
  let table_length = table.len();
  put32(&mut table, 8, table_length);
  put32(&mut table, 20, table_length - 32);
  let kind = if metadata {
    1
  } else if recipe.explicit {
    2
  } else {
    3
  };
  let mut plan = envelope(b"APRP", 48, 48);
  for (offset, value) in [(16, kind), (18, u16::from(!metadata)), (20, u16::from(kind == 3)), (22, u16::from(kind == 3))] {
    put16(&mut plan, offset, value);
  }
  let mut candidates = Vec::new();
  if recipe.explicit && !metadata {
    candidates.push((1, ordinal(&wasm(1, fingerprint)), "", 0));
  } else if !metadata {
    for (essence, fingerprint) in registry {
      candidates.push((2, ordinal(&wasm(1, *fingerprint)), *essence, 0));
    }
    let native = natives();
    candidates.push((3, ordinal(&native[1]), "", 1));
    candidates.push((4, ordinal(&native[0]), "", 2));
    put32(&mut plan, 28, ordinal(&native[2]) as usize);
  }
  put32(&mut plan, 24, candidates.len());
  for (kind, ordinal, essence, policy_index) in candidates {
    let mut candidate = vec![0; 32];
    put32(&mut candidate, 0, 160 + essence.len());
    put16(&mut candidate, 4, kind);
    put16(&mut candidate, 6, u16::from(kind == 2));
    put32(&mut candidate, 8, ordinal as usize);
    put32(&mut candidate, 12, 128);
    put32(&mut candidate, 16, essence.len());
    plan.extend_from_slice(&candidate);
    plan.extend_from_slice(essence.as_bytes());
    plan.extend_from_slice(&policy_bytes(&recipe.policies[policy_index], policy_index == 0));
  }
  let plan_length = plan.len();
  put32(&mut plan, 8, plan_length);
  (plan, table, dependencies, selector_ordinal)
}

fn selector(recipe: &Recipe, ordinal: u32) -> Vec<u8> {
  let mut bytes = vec![0; 32];
  put16(&mut bytes, 0, 1);
  match &recipe.selector {
    Selector::Metadata(id) => {
      put16(&mut bytes, 2, 1);
      bytes.resize(40, 0);
      put16(&mut bytes, 32, *id);
    }
    Selector::Path(segments) => {
      put16(&mut bytes, 2, 2);
      put32(&mut bytes, 12, segments.len());
      put16(&mut bytes, 16, 1);
      for (tag, flags, payload) in segments {
        let mut segment = vec![0; 8];
        segment[0] = *tag;
        segment[1] = *flags;
        put32(&mut segment, 4, payload.len());
        bytes.extend_from_slice(&segment);
        bytes.extend_from_slice(payload);
      }
    }
    Selector::Mapper(arguments) => {
      put16(&mut bytes, 2, 3);
      put16(&mut bytes, 18, 2);
      bytes.resize(48, 0);
      put32(&mut bytes, 32, ordinal as usize);
      put32(&mut bytes, 36, arguments.len());
      put32(&mut bytes, 40, 128);
      bytes.extend_from_slice(arguments);
      bytes.extend_from_slice(&policy_bytes(&recipe.policies[3], true));
    }
  }
  let length = bytes.len();
  put32(&mut bytes, 4, length);
  bytes
}

fn frozen(root: &Path, family: &str, prefix: &str, name: &str, width: usize) -> Vec<u8> {
  let profile = if width == 64 { "sha512" } else { "blake3-256" };
  std::fs::read(root.join(format!("{family}/{prefix}-{profile}-{name}-valid.bin"))).unwrap()
}

pub fn expected(
  root: &Path,
  algorithm: u16,
  owner: &str,
  glob: Option<&str>,
  registry: &[(&str, u8)],
  recipes: Option<&[Recipe]>,
  fingerprint: u8,
) -> Expected {
  assert!(registry.windows(2).all(|pair| pair[0].0 < pair[1].0));
  let registry_projection = object(
    algorithm,
    2,
    map(registry.iter().map(|(essence, fingerprint)| ((*essence).into(), frame(8, &dependency_bytes(&wasm(1, *fingerprint))))).collect()),
  );
  let mut output =
    Expected { algorithm, registry: registry_projection, scope: None, projection: None, fields: Vec::new(), dependencies: Vec::new() };
  let Some(recipes) = recipes else { return output };
  let glob = glob.unwrap_or("");
  let mut scope = envelope(b"ASCP", 64 + owner.len() + glob.len(), 32);
  put32(&mut scope, 32, owner.len());
  put32(&mut scope, 36, glob.len());
  for offset in [40, 42, 44, 46, 48, 50, 52, 54] {
    put16(&mut scope, offset, 1);
  }
  put16(&mut scope, 42, if glob.is_empty() { 1 } else { 2 });
  scope[64..64 + owner.len()].copy_from_slice(owner.as_bytes());
  scope[64 + owner.len()..].copy_from_slice(glob.as_bytes());
  let scope = object(algorithm, 3, scope);
  let scope_id = hex::decode(&scope.identity).unwrap();
  let width = scope_id.len();
  let mut dependencies = BTreeMap::new();
  for recipe in recipes {
    let (plan, table, records, ordinal) = context(recipe, registry, fingerprint);
    let selector = selector(recipe, ordinal);
    let fixed = 32 + width;
    let children = [recipe.name.as_bytes(), &selector, &plan, &table];
    let mut value = envelope(b"AVST", fixed + 80 + children.iter().map(|child| child.len()).sum::<usize>(), 32);
    value[32..fixed].copy_from_slice(&scope_id);
    for (index, child) in children.iter().enumerate() {
      put32(&mut value, fixed + 4 * index, child.len());
    }
    for offset in [16, 18, 20, 22, 24, 26, 28, 30, 32, 34] {
      put16(&mut value, fixed + offset, 1);
    }
    put16(&mut value, fixed + 18, u16::from(matches!(recipe.selector, Selector::Metadata(_))));
    put32(&mut value, fixed + 36, recipe.source_limits[0] as usize);
    for (index, limit) in recipe.source_limits[1..].iter().enumerate() {
      put64(&mut value, fixed + 48 + 8 * index, *limit);
    }
    let mut cursor = fixed + 80;
    for child in children {
      value[cursor..cursor + child.len()].copy_from_slice(child);
      cursor += child.len();
    }
    let value = object(algorithm, 4, value);
    let value_id = hex::decode(&value.identity).unwrap();
    let mut indexes = Vec::new();
    for converter_id in &recipe.converters {
      let name = CONVERTERS[*converter_id as usize - 1];
      let mut converter = frozen(root, "converter-definition-v1", "acnv", name, width);
      put64(&mut converter, 64, recipe.converter_limits[0]);
      put32(&mut converter, 72, recipe.converter_limits[1] as usize);
      put32(&mut converter, 76, recipe.converter_limits[2] as usize);
      put64(&mut converter, 80, recipe.converter_limits[3]);
      let mut field = frozen(root, "field-index-definition-v1", "afix", name, width);
      field[32..fixed].copy_from_slice(&value_id);
      put32(&mut field, fixed + 44, recipe.field_limits[0] as usize);
      put32(&mut field, fixed + 48, recipe.field_limits[1] as usize);
      put64(&mut field, fixed + 56, recipe.field_limits[2]);
      put64(&mut field, fixed + 64, recipe.field_limits[3]);
      let start = field.len() - converter.len();
      field[start..].copy_from_slice(&converter);
      indexes.push(object(algorithm, 5, field));
    }
    indexes.sort_by(|left, right| left.identity.cmp(&right.identity));
    indexes.dedup();
    output.fields.push(Field { name: recipe.name.clone(), value, indexes });
    for record in records {
      let class = if record.kind == 1 { 6 } else { 7 };
      let record = object(algorithm, class, dependency_bytes(&record));
      dependencies.insert((class, record.identity.clone()), record);
    }
  }
  output.fields.sort_by(|left, right| left.name.cmp(&right.name));
  let fields = output
    .fields
    .iter()
    .map(|field| {
      (
        field.name.clone(),
        map(BTreeMap::from([
          (
            "indexes".into(),
            array(&field.indexes.iter().map(|index| frame(8, &hex::decode(&index.identity).unwrap())).collect::<Vec<_>>()),
          ),
          ("value_store_id".into(), frame(8, &hex::decode(&field.value.identity).unwrap())),
        ])),
      )
    })
    .collect();
  output.projection =
    Some(object(algorithm, 1, map(BTreeMap::from([("fields".into(), map(fields)), ("scope_id".into(), frame(8, &scope_id))]))));
  output.scope = Some(scope);
  output.dependencies = dependencies.into_values().collect();
  output
}
