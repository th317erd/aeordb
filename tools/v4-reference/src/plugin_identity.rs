//! Independent sequential Round9 metadata framing. No AeorDB codec imports.
use std::io::{Cursor, Read};
use crate::core::HashProfile;

#[derive(Clone, Copy)]
pub enum PluginIdentityFormat {
  Alias,
  Manifest,
}

impl PluginIdentityFormat {
  pub fn id(self) -> &'static str {
    match self {
      Self::Alias => "plugin-alias-record-v1",
      Self::Manifest => "plugin-manifest-v1",
    }
  }
  pub fn family(self) -> &'static str {
    match self {
      Self::Alias => "PluginAliasRecordV1",
      Self::Manifest => "AeorPluginManifestV1",
    }
  }
}

pub struct PluginIdentityFixtureCase {
  pub id: &'static str,
  pub format: PluginIdentityFormat,
  pub profile: HashProfile,
  pub expected: &'static str,
  pub canonical_key: Option<String>,
  pub bytes: Vec<u8>,
}

fn path(alias: &str) -> String {
  format!("/.aeordb-system/plugin-aliases/{}", blake3::hash(alias.as_bytes()).to_hex())
}

fn envelope(magic: &[u8], header: u16, total: usize, flags: u32) -> Vec<u8> {
  let mut bytes = magic.to_vec();
  bytes.extend_from_slice(&1u16.to_le_bytes());
  bytes.extend_from_slice(&header.to_le_bytes());
  bytes.extend_from_slice(&(total as u32).to_le_bytes());
  bytes.extend_from_slice(&flags.to_le_bytes());
  bytes
}

fn build_alias(fields: [&str; 5], flags: u32, artifact_length: u64) -> Vec<u8> {
  let mut bytes = envelope(b"APAL", 128, 132 + fields.iter().map(|field| field.len()).sum::<usize>(), flags);
  for field in fields {
    bytes.extend_from_slice(&(field.len() as u32).to_le_bytes());
  }
  bytes.extend_from_slice(&1u16.to_le_bytes());
  bytes.extend_from_slice(&1u16.to_le_bytes());
  // Payload fixtures attest only structural metadata, not a deployed module.
  bytes.extend_from_slice(blake3::hash(b"\0asm\x01\0\0\0").as_bytes());
  bytes.extend_from_slice(&artifact_length.to_le_bytes());
  bytes.extend_from_slice(&(-7i64).to_le_bytes());
  bytes.extend_from_slice(&13i64.to_le_bytes());
  bytes.extend_from_slice(&[0; 32]);
  assert_eq!(bytes.len(), 128);
  for field in fields {
    bytes.extend_from_slice(field.as_bytes());
  }
  bytes.extend_from_slice(&crc32fast::hash(&bytes).to_le_bytes());
  bytes
}

fn build_manifest(fields: [&str; 4], roles: &[(u16, u16)]) -> Vec<u8> {
  let mut bytes = envelope(b"APWM", 64, 64 + fields.iter().map(|field| field.len()).sum::<usize>() + roles.len() * 8, 0);
  for field in fields {
    bytes.extend_from_slice(&(field.len() as u32).to_le_bytes());
  }
  bytes.extend_from_slice(&(roles.len() as u16).to_le_bytes());
  bytes.extend_from_slice(&[0; 30]);
  assert_eq!(bytes.len(), 64);
  for field in fields {
    bytes.extend_from_slice(field.as_bytes());
  }
  for (role, abi) in roles {
    bytes.extend_from_slice(&role.to_le_bytes());
    bytes.extend_from_slice(&abi.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
  }
  bytes
}

pub fn fixture_cases() -> Vec<PluginIdentityFixtureCase> {
  use PluginIdentityFormat::{Alias, Manifest};
  let alias = "a".repeat(4096);
  let id = format!("/{}", "i".repeat(4095));
  let name = "n".repeat(4096);
  let version = format!("1.0.0+{}", "b".repeat(250));
  let author = "u".repeat(4096);
  let mut cases = Vec::new();
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    let names = match profile {
      HashProfile::Blake3_256 => [
        "apal-blake3-256-corrected",
        "apal-blake3-256-legacy",
        "apal-blake3-256-maximum",
        "apal-blake3-256-crc-invalid",
        "apwm-blake3-256-parser",
        "apwm-blake3-256-mapper",
        "apwm-blake3-256-both",
        "apwm-blake3-256-maximum",
        "apwm-blake3-256-role-invalid",
      ],
      HashProfile::Sha512 => [
        "apal-sha512-corrected",
        "apal-sha512-legacy",
        "apal-sha512-maximum",
        "apal-sha512-crc-invalid",
        "apwm-sha512-parser",
        "apwm-sha512-mapper",
        "apwm-sha512-both",
        "apwm-sha512-maximum",
        "apwm-sha512-role-invalid",
      ],
    };
    let corrected = build_alias(["parse/é", "/org/example/parser", "Fixture", "1.2.3-rc.1+build.02", "Author"], 0, 8);
    let mut bad_crc = corrected.clone();
    let last = bad_crc.len() - 1;
    bad_crc[last] ^= 1;
    let inputs = [
      (Alias, corrected, "plugin-alias:flags=0:artifact-bytes=8", Some(path("parse/é"))),
      (Alias, build_alias(["old", "legacy-id", "Legacy", "", ""], 7, 8), "plugin-alias:flags=7:artifact-bytes=8", Some(path("old"))),
      (
        Alias,
        build_alias([&alias, &id, &name, &version, &author], 0, 64 << 20),
        "plugin-alias:flags=0:artifact-bytes=67108864",
        Some(path(&alias)),
      ),
      (Alias, bad_crc, "error:plugin_alias_crc", None),
      (Manifest, build_manifest(["/org/example/parser", "Fixture", "1.0.0", ""], &[(1, 3)]), "plugin-manifest:roles=1", None),
      (Manifest, build_manifest(["/org/example/mapper", "Fixture", "1.0.0", "Author"], &[(2, 4)]), "plugin-manifest:roles=1", None),
      (
        Manifest,
        build_manifest(["/org/example/both", "Fixture", "1.0.0-rc.1+02", "Author"], &[(1, 3), (2, 4)]),
        "plugin-manifest:roles=2",
        None,
      ),
      (Manifest, build_manifest([&id, &name, &version, &author], &[(1, 3), (2, 4)]), "plugin-manifest:roles=2", None),
      (Manifest, build_manifest(["/i", "n", "1.0.0", ""], &[(1, 4)]), "error:plugin_manifest_role", None),
    ];
    for (id, (format, bytes, expected, canonical_key)) in names.into_iter().zip(inputs) {
      cases.push(PluginIdentityFixtureCase { id, format, profile, expected, canonical_key, bytes });
    }
  }
  cases
}

type Result<T> = std::result::Result<T, &'static str>;

struct Reader<'a>(Cursor<&'a [u8]>);
impl<'a> Reader<'a> {
  fn fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
    let mut bytes = [0; N];
    self.0.read_exact(&mut bytes).map_err(|_| "plugin_identity_truncated")?;
    Ok(bytes)
  }
  fn u16(&mut self) -> Result<u16> {
    Ok(u16::from_le_bytes(self.fixed()?))
  }
  fn u32(&mut self) -> Result<u32> {
    Ok(u32::from_le_bytes(self.fixed()?))
  }
  fn u64(&mut self) -> Result<u64> {
    Ok(u64::from_le_bytes(self.fixed()?))
  }
  fn text(&mut self, length: usize) -> Result<&'a str> {
    let start = self.0.position() as usize;
    let end = start.checked_add(length).ok_or("plugin_identity_length")?;
    let source = *self.0.get_ref();
    let bytes = source.get(start..end).ok_or("plugin_identity_truncated")?;
    self.0.set_position(end as u64);
    std::str::from_utf8(bytes).map_err(|_| "plugin_identity_utf8")
  }
}

fn header<'a>(bytes: &'a [u8], magic: &[u8; 4], length: usize, cap: usize) -> Result<(Reader<'a>, u32)> {
  if bytes.len() < length || bytes.len() > cap {
    return Err("plugin_identity_length");
  }
  let mut reader = Reader(Cursor::new(bytes));
  if reader.fixed::<4>()? != *magic || reader.u16()? != 1 || usize::from(reader.u16()?) != length {
    return Err("plugin_identity_envelope");
  }
  if reader.u32()? as usize != bytes.len() {
    return Err("plugin_identity_length");
  }
  let flags = reader.u32()?;
  Ok((reader, flags))
}

fn canonical_id(value: &str) -> bool {
  let Some(parts) = value.strip_prefix('/') else { return false };
  parts.split('/').all(|part| !part.is_empty() && part != "." && part != ".." && !part.chars().any(char::is_control))
}

fn canonical_version(value: &str) -> bool {
  semver::Version::parse(value).is_ok_and(|version| version.to_string() == value)
}

fn alias(bytes: &[u8]) -> Result<(String, String)> {
  let (mut reader, flags) = header(bytes, b"APAL", 128, 16_772)?;
  let lengths = [reader.u32()? as usize, reader.u32()? as usize, reader.u32()? as usize, reader.u32()? as usize, reader.u32()? as usize];
  if lengths[..3].iter().any(|length| *length == 0 || *length > 4096) || lengths[3] > 256 || lengths[4] > 4096 {
    return Err("plugin_alias_component_length");
  }
  if bytes.len() != 132 + lengths.iter().sum::<usize>() {
    return Err("plugin_identity_truncated");
  }
  if flags > 7 || reader.u16()? != 1 || reader.u16()? != 1 {
    return Err("plugin_alias_flags_kind");
  }
  reader.fixed::<32>()?;
  let artifact_length = reader.u64()?;
  if !(1..=(64 << 20)).contains(&artifact_length) {
    return Err("plugin_alias_artifact_length");
  }
  reader.fixed::<16>()?; // Signed timestamp bytes have no implicit normalization.
  if reader.fixed::<32>()? != [0; 32] {
    return Err("plugin_alias_reserved");
  }
  let name = reader.text(lengths[0])?;
  let id = reader.text(lengths[1])?;
  reader.text(lengths[2])?;
  let version = reader.text(lengths[3])?;
  let author = reader.text(lengths[4])?;
  if reader.u32()? != crc32fast::hash(&bytes[..bytes.len() - 4]) {
    return Err("plugin_alias_crc");
  }
  if name.chars().any(char::is_control) || (flags & 4 == 0 && !canonical_id(id)) {
    return Err("plugin_alias_identity");
  }
  if version.is_empty() != (flags & 1 != 0) || author.is_empty() != (flags & 2 != 0) {
    return Err("plugin_alias_presence");
  }
  if !version.is_empty() && !canonical_version(version) {
    return Err("plugin_alias_version");
  }
  Ok((format!("plugin-alias:flags={flags}:artifact-bytes={artifact_length}"), path(name)))
}

fn manifest(bytes: &[u8]) -> Result<String> {
  let (mut reader, flags) = header(bytes, b"APWM", 64, 12_672)?;
  let lengths = [reader.u32()? as usize, reader.u32()? as usize, reader.u32()? as usize, reader.u32()? as usize];
  let count = usize::from(reader.u16()?);
  if !(1..=4096).contains(&lengths[0])
    || !(1..=4096).contains(&lengths[1])
    || !(1..=256).contains(&lengths[2])
    || lengths[3] > 4096
    || !(1..=8).contains(&count)
  {
    return Err("plugin_manifest_component_length");
  }
  if bytes.len() != 64 + lengths.iter().sum::<usize>() + 8 * count {
    return Err("plugin_identity_truncated");
  }
  if flags != 0 || reader.fixed::<30>()? != [0; 30] {
    return Err("plugin_manifest_reserved");
  }
  let id = reader.text(lengths[0])?;
  reader.text(lengths[1])?;
  let version = reader.text(lengths[2])?;
  reader.text(lengths[3])?;
  if !canonical_id(id) || !canonical_version(version) {
    return Err("plugin_manifest_identity");
  }
  let mut previous = (0, 0);
  for _ in 0..count {
    let pair = (reader.u16()?, reader.u16()?);
    let flags = reader.u32()?;
    if ![(1, 3), (2, 4)].contains(&pair) || flags != 0 {
      return Err("plugin_manifest_role");
    }
    if pair <= previous {
      return Err("plugin_manifest_order");
    }
    previous = pair;
  }
  Ok(format!("plugin-manifest:roles={count}"))
}

pub fn observe(format: PluginIdentityFormat, bytes: &[u8]) -> (String, Option<String>) {
  let result = match format {
    PluginIdentityFormat::Alias => alias(bytes).map(|(result, path)| (result, Some(path))),
    PluginIdentityFormat::Manifest => manifest(bytes).map(|result| (result, None)),
  };
  result.unwrap_or_else(|error| (format!("error:{error}"), None))
}

pub fn annotation_lines(format: PluginIdentityFormat) -> Vec<String> {
  match format {
    PluginIdentityFormat::Alias => vec![
      "APAL: 128-byte header, alias/ID/name/version/author, trailing CRC32".into(),
      "raw-module fingerprint at40 is fixed BLAKE3-256, not a database-H FileRecord identity".into(),
    ],
    PluginIdentityFormat::Manifest => vec![
      "APWM: 64-byte header, ID/name/version/author, sorted 8-byte role records".into(),
      "custom-section payload only; no module-framing, archive or execution authority".into(),
    ],
  }
}

#[cfg(test)]
#[path = "../spec/plugin_identity_spec.rs"]
mod plugin_identity_spec;
