use crate::engine::HashAlgorithm;
use crate::engine::path_utils::{glob_matches, parent_path};

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

#[derive(Debug, Clone, Copy)]
pub struct EffectiveScopeCandidateV1<'a> {
  pub scope_id: &'a [u8],
  pub encoded_definition: &'a [u8],
}

pub struct EffectiveScopeResolverV1<'definition> {
  scopes: Vec<(usize, ScopeDefinitionV1<'definition>)>,
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

pub fn scope_matches_path(scope: &ScopeDefinitionV1<'_>, path: &str) -> FormatResult<bool> {
  validate_canonical_absolute_path(path)?;
  match scope.mode {
    ScopeMatchingMode::DirectChildren => Ok(parent_path(path).as_deref() == Some(scope.owner_path)),
    ScopeMatchingMode::RelativePathGlob => {
      let glob = scope.glob.ok_or_else(|| {
        error(MalformedInputClass::CrossRecordClosureMismatch, "scope_glob_missing", "relative-glob scope has no decoded glob")
      })?;
      let relative = if scope.owner_path == "/" {
        path.strip_prefix('/')
      } else {
        path.strip_prefix(scope.owner_path).and_then(|suffix| suffix.strip_prefix('/'))
      };
      Ok(relative.is_some_and(|relative| glob_matches(glob, relative)))
    }
  }
}

pub fn scope_owner_overlaps_query_path(scope: &ScopeDefinitionV1<'_>, query_path: &str) -> FormatResult<bool> {
  validate_canonical_absolute_path(scope.owner_path)?;
  validate_canonical_absolute_path(query_path)?;
  Ok(canonical_path_contains(scope.owner_path, query_path) || canonical_path_contains(query_path, scope.owner_path))
}

/// Resolve the sole effective index-configuration scope for one regular file.
///
/// Direct-child and relative-glob membership is defined by each canonical
/// ScopeDefinition. When multiple definitions match, the nearest owner wins.
/// Two matching definitions at the same owner violate the frozen one-config-
/// per-directory invariant and are corruption rather than an order-dependent
/// tie. Engine-owned index paths never belong to an effective scope.
impl<'definition> EffectiveScopeResolverV1<'definition> {
  pub fn from_encoded(hash_algorithm: HashAlgorithm, candidates: &[EffectiveScopeCandidateV1<'definition>]) -> FormatResult<Self> {
    let mut scopes = Vec::new();
    scopes.try_reserve_exact(candidates.len()).map_err(|source| {
      error(
        MalformedInputClass::AllocationAmplification,
        "scope_resolver_allocation",
        format!("cannot prepare {} effective-scope candidates: {source}", candidates.len()),
      )
    })?;
    for (index, candidate) in candidates.iter().enumerate() {
      let scope = decode_scope_definition(candidate.encoded_definition, hash_algorithm)?;
      if scope.scope_id != candidate.scope_id {
        return Err(error(
          MalformedInputClass::IdentityKeyOrGenerationMismatch,
          "scope_candidate_identity",
          "effective-scope candidate identity does not match its canonical definition",
        ));
      }
      scopes.push((index, scope));
    }
    Ok(Self { scopes })
  }

  pub fn resolve(&self, path: &str) -> FormatResult<Option<usize>> {
    validate_canonical_absolute_path(path)?;
    if path == "/" {
      return Err(error(
        MalformedInputClass::InvalidUtf8PathGlobOrNativePath,
        "scope_file_path_root",
        "an effective scope can only be resolved for a regular-file path",
      ));
    }
    if is_internal_index_path_v1(path) {
      return Ok(None);
    }

    let mut winner: Option<(usize, usize, &str)> = None;
    for (index, scope) in &self.scopes {
      if !scope_matches_path(scope, path)? {
        continue;
      }
      let owner_depth = canonical_path_depth(scope.owner_path);
      match winner {
        None => winner = Some((*index, owner_depth, scope.owner_path)),
        Some((_, depth, _)) if owner_depth > depth => winner = Some((*index, owner_depth, scope.owner_path)),
        Some((_, depth, prior_owner)) if owner_depth == depth => {
          let context = if prior_owner == scope.owner_path {
            format!("selected semantic catalog contains multiple matching scopes owned by {}", scope.owner_path)
          } else {
            format!("matching scope owners {prior_owner:?} and {:?} have the same canonical depth", scope.owner_path)
          };
          return Err(error(MalformedInputClass::NoncanonicalOrderOrDuplicate, "scope_winner_owner_duplicate", context));
        }
        Some(_) => {}
      }
    }

    Ok(winner.map(|(index, _, _)| index))
  }
}

pub fn is_internal_index_path_v1(path: &str) -> bool {
  path.split('/').filter(|segment| !segment.is_empty()).enumerate().any(|(index, segment)| {
    (index == 0 && segment == ".aeordb-system") || matches!(segment, ".aeordb-config" | ".aeordb-indexes" | ".aeordb-logs")
  })
}

fn canonical_path_contains(parent: &str, child: &str) -> bool {
  parent == "/" || parent == child || child.strip_prefix(parent).is_some_and(|suffix| suffix.starts_with('/'))
}

fn canonical_path_depth(path: &str) -> usize {
  path.split('/').filter(|segment| !segment.is_empty()).count()
}

pub(crate) fn validate_canonical_absolute_path(path: &str) -> FormatResult<()> {
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
