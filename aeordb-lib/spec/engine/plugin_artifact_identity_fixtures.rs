//! Test-only independent raw-module and alias constructors.
pub(super) fn leb(mut value: usize, output: &mut Vec<u8>) {
  loop {
    let byte = (value & 127) as u8;
    value >>= 7;
    output.push(byte | if value == 0 { 0 } else { 128 });
    if value == 0 {
      break;
    }
  }
}

pub(super) fn custom(name: &str, payload: &[u8], module: &mut Vec<u8>) {
  let mut section = Vec::new();
  leb(name.len(), &mut section);
  section.extend_from_slice(name.as_bytes());
  section.extend_from_slice(payload);
  module.push(0);
  leb(section.len(), module);
  module.extend_from_slice(&section);
}

pub(super) fn manifest(role: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/plugin-manifest-v1/apwm-blake3-256-{role}.bin", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

pub(super) fn module(role: &str) -> Vec<u8> {
  let mut module = wat::parse_str(
    r#"(module
    (memory (export "memory") 1)
    (func (export "aeordb_alloc_v1") (param i32) (result i32) i32.const 0)
    (func (export "aeordb_handle_v1") (param i32 i32) (result i64) i64.const 0))"#,
  )
  .unwrap();
  custom("aeordb.plugin.v1", &manifest(role), &mut module);
  // An independent engine confirms fixture framing/type validity. This does
  // not make the inspected identity an admitted/executable runtime capability.
  wasmi::Module::validate(&wasmi::Engine::default(), &module).unwrap();
  module
}

pub(super) fn alias(bytes: &[u8], role: &str) -> Vec<u8> {
  let (id, version, author) = match role {
    "parser" => ("/org/example/parser", "1.0.0", ""),
    "mapper" => ("/org/example/mapper", "1.0.0", "Author"),
    "both" => ("/org/example/both", "1.0.0-rc.1+02", "Author"),
    _ => panic!("unknown fixture role"),
  };
  let mut alias = vec![0; 128];
  alias[..8].copy_from_slice(b"APAL\x01\x00\x80\x00");
  alias[12] = if author.is_empty() { 2 } else { 0 };
  for (offset, text) in [(16, "parse"), (20, id), (24, "Fixture"), (28, version), (32, author)] {
    alias[offset..offset + 4].copy_from_slice(&(text.len() as u32).to_le_bytes());
    alias.extend_from_slice(text.as_bytes());
  }
  alias[36] = 1;
  alias[38] = 1;
  alias[40..72].copy_from_slice(blake3::hash(bytes).as_bytes());
  alias[72..80].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
  let total = (alias.len() + 4) as u32;
  alias[8..12].copy_from_slice(&total.to_le_bytes());
  alias.extend_from_slice(&[0; 4]);
  seal(&mut alias);
  alias
}

pub(super) fn seal(bytes: &mut [u8]) {
  let end = bytes.len() - 4;
  let checksum = crc32fast::hash(&bytes[..end]);
  bytes[end..].copy_from_slice(&checksum.to_le_bytes());
}

pub(super) fn alias_path() -> String {
  format!("/.aeordb-system/plugin-aliases/{}", blake3::hash(b"parse").to_hex())
}

pub(super) fn artifact_path(bytes: &[u8]) -> String {
  format!("/.aeordb-system/plugin-artifacts/blake3/{}", blake3::hash(bytes).to_hex())
}
