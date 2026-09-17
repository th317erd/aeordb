//! Structural Round9 metadata only; never deployment or execution authority.
use super::dependency::{is_canonical_dependency_id, is_canonical_semver};
use super::reader::{FormatError, FormatResult, MalformedInputClass};

const ALIAS_HEADER_LENGTH: usize = 128;
pub(crate) const ALIAS_MAX_LENGTH: usize = 16_772;
const MANIFEST_HEADER_LENGTH: usize = 64;
const MANIFEST_MAX_LENGTH: usize = 12_672;
const ALIAS_PREFIX: &str = "/.aeordb-system/plugin-aliases/";

/// Canonical lookup path for an exact alias name, not a live alias resolution.
pub(crate) fn plugin_alias_path_v1(alias: &str) -> FormatResult<String> {
  if alias.is_empty() || alias.len() > 4096 || alias.chars().any(char::is_control) {
    return Err(error(
      MalformedInputClass::InvalidUtf8PathGlobOrNativePath,
      "plugin_alias_name",
      "alias requires 1..4096 UTF-8 bytes without controls",
    ));
  }
  plugin_identity_path_v1(ALIAS_PREFIX, blake3::hash(alias.as_bytes()).as_bytes())
}

pub(super) fn plugin_identity_path_v1(prefix: &str, fingerprint: &[u8; 32]) -> FormatResult<String> {
  let mut path = String::new();
  path
    .try_reserve_exact(prefix.len() + 64)
    .map_err(|source| error(MalformedInputClass::AllocationAmplification, "plugin_identity_path_allocation", source.to_string()))?;
  path.push_str(prefix);
  let hex = b"0123456789abcdef";
  for byte in fingerprint {
    path.push(char::from(hex[usize::from(byte >> 4)]));
    path.push(char::from(hex[usize::from(byte & 15)]));
  }
  Ok(path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginAliasRecordV1<'a> {
  pub flags: u32,
  pub alias: &'a str,
  pub plugin_id: &'a str,
  pub name: &'a str,
  pub version: Option<&'a str>,
  pub author: Option<&'a str>,
  pub artifact_fingerprint: &'a [u8; 32],
  pub artifact_length: u64,
  pub created_at_ms: i64,
  pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginManifestRoleV1 {
  pub role: u16,
  pub abi: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AeorPluginManifestV1<'a> {
  pub plugin_id: &'a str,
  pub name: &'a str,
  pub version: &'a str,
  pub author: Option<&'a str>,
  roles: &'a [u8],
}

impl AeorPluginManifestV1<'_> {
  pub fn roles(&self) -> impl ExactSizeIterator<Item = PluginManifestRoleV1> + '_ {
    self
      .roles
      .chunks_exact(8)
      .map(|bytes| PluginManifestRoleV1 { role: u16::from_le_bytes([bytes[0], bytes[1]]), abi: u16::from_le_bytes([bytes[2], bytes[3]]) })
  }
}

/// Validate a borrowed APAL body and its exact protected name key. This does
/// not resolve or verify the referenced module, or admit a corrected executor.
pub fn decode_plugin_alias_v1<'a>(value: &'a [u8], canonical_path: &str) -> FormatResult<PluginAliasRecordV1<'a>> {
  envelope(value, b"APAL", ALIAS_HEADER_LENGTH, ALIAS_MAX_LENGTH, 4)?;
  let flags = u32_at(value, 12);
  if flags & !7 != 0 || u16_at(value, 36) != 1 || u16_at(value, 38) != 1 {
    return Err(error(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "plugin_alias_flags_kind",
      "unknown alias flags or artifact/plugin kind",
    ));
  }
  if value[96..128].iter().any(|byte| *byte != 0) {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "plugin_alias_reserved", "alias reserve is nonzero"));
  }
  let lengths = [
    u32_at(value, 16) as usize,
    u32_at(value, 20) as usize,
    u32_at(value, 24) as usize,
    u32_at(value, 28) as usize,
    u32_at(value, 32) as usize,
  ];
  if lengths[..3].iter().any(|length| !(1..=4096).contains(length)) || lengths[3] > 256 || lengths[4] > 4096 {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "plugin_alias_component_length",
      "alias metadata exceeds its field bound",
    ));
  }
  exact_components(value, ALIAS_HEADER_LENGTH + 4, &lengths)?;
  let checksum_offset = value.len() - 4;
  if crc32fast::hash(&value[..checksum_offset]) != u32_at(value, checksum_offset) {
    return Err(error(MalformedInputClass::ChecksumOrIntegrityMismatch, "plugin_alias_crc", "alias checksum mismatch"));
  }
  let mut cursor = ALIAS_HEADER_LENGTH;
  let alias = text_at(value, &mut cursor, lengths[0])?;
  let plugin_id = text_at(value, &mut cursor, lengths[1])?;
  let name = text_at(value, &mut cursor, lengths[2])?;
  let version = text_at(value, &mut cursor, lengths[3])?;
  let author = text_at(value, &mut cursor, lengths[4])?;
  if alias.chars().any(char::is_control) || (flags & 4 == 0 && !is_canonical_dependency_id(plugin_id)) {
    return Err(error(
      MalformedInputClass::InvalidUtf8PathGlobOrNativePath,
      "plugin_alias_identity",
      "invalid alias name or canonical plugin ID",
    ));
  }
  if (flags & 1 != 0) != version.is_empty() || (flags & 2 != 0) != author.is_empty() {
    return Err(error(
      MalformedInputClass::NoncanonicalBooleanOrOptionalPresence,
      "plugin_alias_presence",
      "alias metadata presence disagrees with flags",
    ));
  }
  if !version.is_empty() && !is_canonical_semver(version) {
    return Err(error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, "plugin_alias_version", "version is not canonical SemVer"));
  }
  let artifact_length = u64_at(value, 72);
  if !(1..=(64 << 20)).contains(&artifact_length) {
    return Err(error(MalformedInputClass::AllocationAmplification, "plugin_alias_artifact_length", "raw module must contain 1..64 MiB"));
  }
  validate_alias_key(canonical_path, alias)?;
  let artifact_fingerprint = value[40..72].try_into().map_err(|source: std::array::TryFromSliceError| {
    error(MalformedInputClass::TruncationOrTrailingBytes, "plugin_alias_fingerprint", source.to_string())
  })?;
  Ok(PluginAliasRecordV1 {
    flags,
    alias,
    plugin_id,
    name,
    version: (!version.is_empty()).then_some(version),
    author: (!author.is_empty()).then_some(author),
    artifact_fingerprint,
    artifact_length,
    created_at_ms: i64::from_le_bytes(u64_at(value, 80).to_le_bytes()),
    updated_at_ms: i64::from_le_bytes(u64_at(value, 88).to_le_bytes()),
  })
}

/// Decode only the named custom section's payload. The module owner must prove
/// section uniqueness, complete WASM framing and current executor availability.
pub fn decode_plugin_manifest_payload_v1(value: &[u8]) -> FormatResult<AeorPluginManifestV1<'_>> {
  envelope(value, b"APWM", MANIFEST_HEADER_LENGTH, MANIFEST_MAX_LENGTH, 0)?;
  if u32_at(value, 12) != 0 || value[34..64].iter().any(|byte| *byte != 0) {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "plugin_manifest_reserved", "manifest flags or reserve are nonzero"));
  }
  let lengths = [u32_at(value, 16) as usize, u32_at(value, 20) as usize, u32_at(value, 24) as usize, u32_at(value, 28) as usize];
  let role_count = usize::from(u16_at(value, 32));
  if !(1..=4096).contains(&lengths[0])
    || !(1..=4096).contains(&lengths[1])
    || !(1..=256).contains(&lengths[2])
    || lengths[3] > 4096
    || !(1..=8).contains(&role_count)
  {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "plugin_manifest_component_length",
      "manifest field or role count exceeds its bound",
    ));
  }
  // The role count is admitted above before multiplying or slicing its bytes.
  exact_components(value, MANIFEST_HEADER_LENGTH + 8 * role_count, &lengths)?;
  let mut cursor = MANIFEST_HEADER_LENGTH;
  let plugin_id = text_at(value, &mut cursor, lengths[0])?;
  let name = text_at(value, &mut cursor, lengths[1])?;
  let version = text_at(value, &mut cursor, lengths[2])?;
  let author = text_at(value, &mut cursor, lengths[3])?;
  if !is_canonical_dependency_id(plugin_id) || !is_canonical_semver(version) {
    return Err(error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, "plugin_manifest_identity", "invalid canonical ID or SemVer"));
  }
  let roles = &value[cursor..];
  let mut previous = None;
  for bytes in roles.chunks_exact(8) {
    let pair = (u16_at(bytes, 0), u16_at(bytes, 2));
    if !matches!(pair, (1, 3) | (2, 4)) || u32_at(bytes, 4) != 0 {
      return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "plugin_manifest_role", "unknown role/ABI pair or role flags"));
    }
    if previous.is_some_and(|previous| previous >= pair) {
      return Err(error(MalformedInputClass::NoncanonicalOrderOrDuplicate, "plugin_manifest_order", "roles are not strictly increasing"));
    }
    previous = Some(pair);
  }
  Ok(AeorPluginManifestV1 { plugin_id, name, version, author: (!author.is_empty()).then_some(author), roles })
}

fn envelope(value: &[u8], magic: &[u8; 4], header_length: usize, maximum_length: usize, trailer: usize) -> FormatResult<()> {
  if value.len() > maximum_length {
    return Err(error(MalformedInputClass::AllocationAmplification, "plugin_identity_length", "metadata exceeds its format byte ceiling"));
  }
  if value.len() < header_length + trailer {
    return Err(truncated());
  }
  if &value[..4] != magic || u16_at(value, 4) != 1 || usize::from(u16_at(value, 6)) != header_length {
    return Err(error(
      MalformedInputClass::UnknownMagicOrVersion,
      "plugin_identity_envelope",
      "wrong metadata magic, version or header length",
    ));
  }
  if u32_at(value, 8) as usize != value.len() {
    return Err(truncated());
  }
  Ok(())
}

fn exact_components(value: &[u8], fixed: usize, lengths: &[usize]) -> FormatResult<()> {
  let mut expected = fixed;
  for length in lengths {
    expected = expected.checked_add(*length).ok_or_else(|| {
      error(MalformedInputClass::LengthCountOrArithmeticOverflow, "plugin_identity_components", "metadata length overflow")
    })?;
  }
  if expected != value.len() {
    return Err(truncated());
  }
  Ok(())
}

fn validate_alias_key(path: &str, alias: &str) -> FormatResult<()> {
  let invalid = || {
    error(MalformedInputClass::IdentityKeyOrGenerationMismatch, "plugin_alias_key", "protected key differs from embedded alias identity")
  };
  let suffix = path.strip_prefix(ALIAS_PREFIX).ok_or_else(invalid)?;
  if suffix.len() != 64 {
    return Err(invalid());
  }
  let fingerprint = blake3::hash(alias.as_bytes());
  let suffix = suffix.as_bytes();
  let hex = b"0123456789abcdef";
  for (index, byte) in fingerprint.as_bytes().iter().enumerate() {
    if suffix[2 * index] != hex[usize::from(byte >> 4)] || suffix[2 * index + 1] != hex[usize::from(byte & 15)] {
      return Err(invalid());
    }
  }
  Ok(())
}

fn text_at<'a>(value: &'a [u8], cursor: &mut usize, length: usize) -> FormatResult<&'a str> {
  let end = cursor.checked_add(length).ok_or_else(truncated)?;
  let bytes = value.get(*cursor..end).ok_or_else(truncated)?;
  *cursor = end;
  std::str::from_utf8(bytes)
    .map_err(|source| error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, "plugin_identity_utf8", source.to_string()))
}

fn u16_at(value: &[u8], offset: usize) -> u16 {
  u16::from_le_bytes([value[offset], value[offset + 1]])
}

fn u32_at(value: &[u8], offset: usize) -> u32 {
  u32::from_le_bytes([value[offset], value[offset + 1], value[offset + 2], value[offset + 3]])
}

fn u64_at(value: &[u8], offset: usize) -> u64 {
  u64::from_le_bytes([
    value[offset],
    value[offset + 1],
    value[offset + 2],
    value[offset + 3],
    value[offset + 4],
    value[offset + 5],
    value[offset + 6],
    value[offset + 7],
  ])
}

fn truncated() -> FormatError {
  error(
    MalformedInputClass::TruncationOrTrailingBytes,
    "plugin_identity_truncated",
    "metadata length, component lengths or trailing bytes disagree",
  )
}

fn error(class: MalformedInputClass, code: &'static str, message: impl Into<String>) -> FormatError {
  FormatError::new(class, code, message)
}
