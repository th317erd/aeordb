use crate::engine::HashAlgorithm;

use super::hash::digest_parts;
use super::reader::{FormatError, FormatResult, MalformedInputClass};

const DEFINITION_HEADER_LENGTH: usize = 32;
const SCOPE_FIXED_LENGTH: usize = 64;
const SCOPE_MAX_LENGTH: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeMatchingMode {
  DirectChildren,
  RelativePathGlob,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeDefinitionV1<'a> {
  pub scope_id: Vec<u8>,
  pub mode: ScopeMatchingMode,
  pub owner_path: &'a str,
  pub glob: Option<&'a str>,
}

pub fn decode_scope_definition(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<ScopeDefinitionV1<'_>> {
  if value.len() > SCOPE_MAX_LENGTH {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "scope_exceeds_cap",
      format!("{} bytes exceeds {SCOPE_MAX_LENGTH}", value.len()),
    ));
  }
  if value.len() < SCOPE_FIXED_LENGTH {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "scope_truncated",
      format!("{} bytes is shorter than {SCOPE_FIXED_LENGTH}", value.len()),
    ));
  }
  if &value[..4] != b"ASCP" || u16_at(value, 4)? != 1 || u16_at(value, 6)? as usize != DEFINITION_HEADER_LENGTH {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "scope_envelope", "expected ASCP v1 with 32-byte header"));
  }
  if u32_at(value, 8)? as usize != value.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "scope_total_length",
      format!("declared {}, got {}", u32_at(value, 8)?, value.len()),
    ));
  }
  if u32_at(value, 12)? != 0 || value[16..32].iter().any(|byte| *byte != 0) {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "scope_envelope_reserved", "flags or envelope reserve are nonzero"));
  }

  let owner_length = usize::try_from(u32_at(value, 32)?).map_err(|_| length_error("scope owner length does not fit usize"))?;
  let glob_length = usize::try_from(u32_at(value, 36)?).map_err(|_| length_error("scope glob length does not fit usize"))?;
  let variable_length = owner_length.checked_add(glob_length).ok_or_else(|| length_error("scope variable length overflow"))?;
  let expected_length = SCOPE_FIXED_LENGTH.checked_add(variable_length).ok_or_else(|| length_error("scope total length overflow"))?;
  if owner_length == 0 || expected_length != value.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "scope_body_length",
      format!("owner {owner_length}, glob {glob_length}, total {}", value.len()),
    ));
  }
  if u16_at(value, 40)? != 1 || [44, 46, 48, 50, 52, 54].into_iter().any(|offset| u16_at(value, offset).ok() != Some(1)) {
    return Err(error(
      MalformedInputClass::UnknownMagicOrVersion,
      "scope_semantic_versions",
      "membership and all semantic versions must be one",
    ));
  }
  if value[56..64].iter().any(|byte| *byte != 0) {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "scope_reserved", "scope reserve is nonzero"));
  }

  let mode = match u16_at(value, 42)? {
    1 if glob_length == 0 => ScopeMatchingMode::DirectChildren,
    2 if glob_length > 0 => ScopeMatchingMode::RelativePathGlob,
    1 | 2 => {
      return Err(error(
        MalformedInputClass::CrossRecordClosureMismatch,
        "scope_mode_length",
        "direct scopes must omit a glob and relative-glob scopes must include one",
      ));
    }
    mode => {
      return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "scope_matching_mode", format!("unknown mode {mode}")));
    }
  };

  let owner_end = SCOPE_FIXED_LENGTH.checked_add(owner_length).ok_or_else(|| length_error("scope owner end overflow"))?;
  let owner_path = utf8(&value[SCOPE_FIXED_LENGTH..owner_end], "scope_owner_utf8")?;
  validate_canonical_absolute_path(owner_path)?;
  let glob = if glob_length == 0 {
    None
  } else {
    let glob = utf8(&value[owner_end..], "scope_glob_utf8")?;
    validate_canonical_glob(glob)?;
    Some(glob)
  };

  let scope_id = digest_parts(hash_algorithm, &[b"aeordb.index.scope-definition.v1\0", value]);
  Ok(ScopeDefinitionV1 { scope_id, mode, owner_path, glob })
}

fn validate_canonical_absolute_path(path: &str) -> FormatResult<()> {
  if path == "/" {
    return Ok(());
  }
  if path.is_empty()
    || !path.starts_with('/')
    || path.ends_with('/')
    || path.trim() != path
    || path.as_bytes().contains(&0)
    || path[1..].split('/').any(|segment| segment.is_empty() || segment == "." || segment == "..")
  {
    return Err(error(
      MalformedInputClass::InvalidUtf8PathGlobOrNativePath,
      "scope_owner_noncanonical",
      "owner path is not its canonical absolute normalization",
    ));
  }
  Ok(())
}

fn validate_canonical_glob(glob: &str) -> FormatResult<()> {
  if glob.is_empty()
    || glob.as_bytes().contains(&0)
    || glob.starts_with('/')
    || glob.ends_with('/')
    || glob.split('/').any(|segment| segment.is_empty() || segment == "." || segment == "..")
  {
    return Err(error(
      MalformedInputClass::InvalidUtf8PathGlobOrNativePath,
      "scope_glob_noncanonical",
      "glob is not in canonical relative form",
    ));
  }
  Ok(())
}

fn utf8<'a>(bytes: &'a [u8], code: &'static str) -> FormatResult<&'a str> {
  std::str::from_utf8(bytes)
    .map_err(|source| error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, code, format!("invalid UTF-8: {source}")))
}

fn u16_at(bytes: &[u8], offset: usize) -> FormatResult<u16> {
  let raw = bytes
    .get(offset..offset + 2)
    .ok_or_else(|| error(MalformedInputClass::TruncationOrTrailingBytes, "scope_u16_truncated", format!("u16 at {offset}")))?;
  Ok(u16::from_le_bytes(raw.try_into().expect("checked scope u16 length")))
}

fn u32_at(bytes: &[u8], offset: usize) -> FormatResult<u32> {
  let raw = bytes
    .get(offset..offset + 4)
    .ok_or_else(|| error(MalformedInputClass::TruncationOrTrailingBytes, "scope_u32_truncated", format!("u32 at {offset}")))?;
  Ok(u32::from_le_bytes(raw.try_into().expect("checked scope u32 length")))
}

fn length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "scope_length_overflow", context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
