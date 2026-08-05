use crate::core::HashProfile;

const DEFINITION_HEADER_LENGTH: usize = 32;
const SCOPE_FIXED_LENGTH: usize = 64;
const SCOPE_MAX_LENGTH: usize = 65_536;

#[derive(Clone, Copy)]
pub enum DefinitionFormat {
  ScopeDefinitionV1,
}

impl DefinitionFormat {
  pub fn id(self) -> &'static str {
    "scope-definition-v1"
  }

  pub fn family(self) -> &'static str {
    "ScopeDefinitionV1"
  }
}

#[derive(Clone)]
pub struct DefinitionFixtureCase {
  pub id: &'static str,
  pub format: DefinitionFormat,
  pub profile: HashProfile,
  pub expected: &'static str,
  pub relation: Option<&'static str>,
  pub canonical_key: Option<String>,
  pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MatchingMode {
  DirectChildren,
  RelativePathGlob,
}

impl MatchingMode {
  fn id(self) -> u16 {
    match self {
      Self::DirectChildren => 1,
      Self::RelativePathGlob => 2,
    }
  }

  fn name(self) -> &'static str {
    match self {
      Self::DirectChildren => "direct",
      Self::RelativePathGlob => "relative-glob",
    }
  }
}

struct DecodedScope<'a> {
  mode: MatchingMode,
  owner_path: &'a str,
  glob: Option<&'a str>,
}

pub fn fixture_cases() -> Vec<DefinitionFixtureCase> {
  let mut cases = Vec::with_capacity(6);
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    cases.push(scope_fixture(
      profile,
      match profile {
        HashProfile::Blake3_256 => "ascp-blake3-256-root-direct-valid",
        HashProfile::Sha512 => "ascp-sha512-root-direct-valid",
      },
      "/",
      None,
      "scope:direct:owner=/",
      Some("semantic-id:ScopeId"),
    ));
    cases.push(scope_fixture(
      profile,
      match profile {
        HashProfile::Blake3_256 => "ascp-blake3-256-normalized-glob-valid",
        HashProfile::Sha512 => "ascp-sha512-normalized-glob-valid",
      },
      " /workspace//wyatt/../docs/ ",
      Some("/guides//**/*.md/"),
      "scope:relative-glob:owner=/workspace/docs:glob=guides/**/*.md",
      Some("semantic-id:ScopeId"),
    ));

    let maximum_owner = format!("/{}", "a".repeat(65_470));
    cases.push(scope_fixture(
      profile,
      match profile {
        HashProfile::Blake3_256 => "ascp-blake3-256-maximum-length-valid",
        HashProfile::Sha512 => "ascp-sha512-maximum-length-valid",
      },
      &maximum_owner,
      Some("*"),
      "scope:relative-glob:maximum-length",
      Some("boundary:65536-bytes"),
    ));
  }
  cases
}

pub fn observe(profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  match decode_scope(bytes) {
    Ok(scope) => {
      let outcome = if bytes.len() == SCOPE_MAX_LENGTH {
        "scope:relative-glob:maximum-length".to_string()
      } else if let Some(glob) = scope.glob {
        format!("scope:{}:owner={}:glob={glob}", scope.mode.name(), scope.owner_path)
      } else {
        format!("scope:{}:owner={}", scope.mode.name(), scope.owner_path)
      };
      (outcome, Some(hex::encode(scope_id(profile, bytes))))
    }
    Err(error) => (format!("error:{error}"), None),
  }
}

pub fn annotation_lines(bytes: &[u8]) -> Vec<String> {
  let owner_length = read_u32(bytes, 32).unwrap_or(0) as usize;
  let glob_length = read_u32(bytes, 36).unwrap_or(0) as usize;
  vec![
    "definition +0x000 len 32: canonical definition envelope".to_string(),
    "definition magic: ASCP".to_string(),
    "body +0x020 len 4: owner_path_length".to_string(),
    "body +0x024 len 4: glob_length".to_string(),
    "body +0x028 len 16: membership and semantic-version enums".to_string(),
    "body +0x038 len 8: reserved zero".to_string(),
    format!("body +0x040 len {owner_length}: canonical owner_path"),
    format!("body +0x{:03x} len {glob_length}: canonical glob", SCOPE_FIXED_LENGTH + owner_length),
  ]
}

fn scope_fixture(
  profile: HashProfile,
  id: &'static str,
  owner_path: &str,
  glob: Option<&str>,
  expected: &'static str,
  relation: Option<&'static str>,
) -> DefinitionFixtureCase {
  let bytes = build_scope(owner_path, glob).expect("fixture source must be canonicalizable");
  DefinitionFixtureCase {
    id,
    format: DefinitionFormat::ScopeDefinitionV1,
    profile,
    expected,
    relation,
    canonical_key: Some(hex::encode(scope_id(profile, &bytes))),
    bytes,
  }
}

fn build_scope(owner_path: &str, glob: Option<&str>) -> Result<Vec<u8>, &'static str> {
  let owner_path = normalize_path(owner_path)?;
  let (mode, glob) = match glob {
    Some(glob) => (MatchingMode::RelativePathGlob, canonicalize_glob(glob)?),
    None => (MatchingMode::DirectChildren, String::new()),
  };
  let variable_length = owner_path.len().checked_add(glob.len()).ok_or("scope_length_overflow")?;
  let total_length = SCOPE_FIXED_LENGTH.checked_add(variable_length).ok_or("scope_length_overflow")?;
  if owner_path.is_empty() || total_length > SCOPE_MAX_LENGTH {
    return Err("scope_length");
  }

  let mut value = vec![0u8; total_length];
  value[0..4].copy_from_slice(b"ASCP");
  put_u16(&mut value, 4, 1);
  put_u16(&mut value, 6, DEFINITION_HEADER_LENGTH as u16);
  put_u32(&mut value, 8, total_length as u32);
  put_u32(&mut value, 32, owner_path.len() as u32);
  put_u32(&mut value, 36, glob.len() as u32);
  put_u16(&mut value, 40, 1);
  put_u16(&mut value, 42, mode.id());
  for offset in [44, 46, 48, 50, 52, 54] {
    put_u16(&mut value, offset, 1);
  }
  value[64..64 + owner_path.len()].copy_from_slice(owner_path.as_bytes());
  value[64 + owner_path.len()..].copy_from_slice(glob.as_bytes());
  Ok(value)
}

fn decode_scope(value: &[u8]) -> Result<DecodedScope<'_>, &'static str> {
  if value.len() < SCOPE_FIXED_LENGTH || value.len() > SCOPE_MAX_LENGTH {
    return Err("scope_length");
  }
  if &value[0..4] != b"ASCP" || read_u16(value, 4)? != 1 || read_u16(value, 6)? as usize != DEFINITION_HEADER_LENGTH {
    return Err("definition_envelope");
  }
  if read_u32(value, 8)? as usize != value.len() || read_u32(value, 12)? != 0 || value[16..32].iter().any(|byte| *byte != 0) {
    return Err("definition_metadata");
  }

  let owner_length = read_u32(value, 32)? as usize;
  let glob_length = read_u32(value, 36)? as usize;
  let variable_length = owner_length.checked_add(glob_length).ok_or("scope_length_overflow")?;
  let expected_length = SCOPE_FIXED_LENGTH.checked_add(variable_length).ok_or("scope_length_overflow")?;
  if owner_length == 0 || expected_length != value.len() {
    return Err("scope_body_length");
  }
  if read_u16(value, 40)? != 1 || [44, 46, 48, 50, 52, 54].iter().any(|offset| read_u16(value, *offset).ok() != Some(1)) {
    return Err("scope_semantics");
  }
  if value[56..64].iter().any(|byte| *byte != 0) {
    return Err("scope_reserved");
  }

  let mode = match read_u16(value, 42)? {
    1 if glob_length == 0 => MatchingMode::DirectChildren,
    2 if glob_length > 0 => MatchingMode::RelativePathGlob,
    1 | 2 => return Err("scope_mode_length"),
    _ => return Err("scope_matching_mode"),
  };
  let owner_end = SCOPE_FIXED_LENGTH.checked_add(owner_length).ok_or("scope_length_overflow")?;
  let owner_path = std::str::from_utf8(&value[SCOPE_FIXED_LENGTH..owner_end]).map_err(|_| "scope_owner_utf8")?;
  if normalize_path(owner_path)?.as_bytes() != owner_path.as_bytes() {
    return Err("scope_owner_noncanonical");
  }
  let glob = if glob_length == 0 {
    None
  } else {
    let glob = std::str::from_utf8(&value[owner_end..]).map_err(|_| "scope_glob_utf8")?;
    if canonicalize_glob(glob)?.as_bytes() != glob.as_bytes() {
      return Err("scope_glob_noncanonical");
    }
    Some(glob)
  };

  Ok(DecodedScope { mode, owner_path, glob })
}

pub(crate) fn scope_id(profile: HashProfile, bytes: &[u8]) -> Vec<u8> {
  let mut preimage = Vec::with_capacity(34 + bytes.len());
  preimage.extend_from_slice(b"aeordb.index.scope-definition.v1\0");
  preimage.extend_from_slice(bytes);
  profile.digest(&preimage)
}

pub(crate) fn validate_scope_definition(bytes: &[u8]) -> Result<(), &'static str> {
  decode_scope(bytes).map(|_| ())
}

pub(crate) fn sample_scope_definition() -> Vec<u8> {
  build_scope("/workspace/docs", Some("**/*.md")).expect("sample scope definition")
}

pub(crate) fn file_key(profile: HashProfile, path: &str) -> Result<Vec<u8>, &'static str> {
  let path = normalize_path(path)?;
  let mut preimage = Vec::with_capacity(5 + path.len());
  preimage.extend_from_slice(b"file:");
  preimage.extend_from_slice(path.as_bytes());
  Ok(profile.digest(&preimage))
}

pub(crate) fn is_canonical_absolute_path(path: &str) -> bool {
  path.starts_with('/') && normalize_path(path).is_ok_and(|canonical| canonical == path)
}

fn normalize_path(path: &str) -> Result<String, &'static str> {
  let without_nul: String = path.chars().filter(|character| *character != '\0').collect();
  let mut segments = Vec::new();
  for segment in without_nul.trim().split('/') {
    match segment {
      "" | "." => {}
      ".." => {
        segments.pop();
      }
      _ => segments.push(segment),
    }
  }
  if segments.is_empty() {
    Ok("/".to_string())
  } else {
    Ok(format!("/{}", segments.join("/")))
  }
}

fn canonicalize_glob(glob: &str) -> Result<String, &'static str> {
  if glob.as_bytes().contains(&0) {
    return Err("glob_nul");
  }
  let mut segments = Vec::new();
  for segment in glob.split('/') {
    match segment {
      "" => {}
      "." | ".." => return Err("glob_relative_segment"),
      _ => segments.push(segment),
    }
  }
  if segments.is_empty() {
    return Err("glob_empty");
  }
  Ok(segments.join("/"))
}

#[cfg(test)]
fn is_internal_path(path: &str) -> Result<bool, &'static str> {
  let path = normalize_path(path)?;
  let segments: Vec<&str> = path.split('/').filter(|segment| !segment.is_empty()).collect();
  Ok(
    segments.first() == Some(&".aeordb-system")
      || segments.iter().any(|segment| matches!(*segment, ".aeordb-config" | ".aeordb-indexes" | ".aeordb-logs")),
  )
}

#[cfg(test)]
fn glob_matches(pattern: &str, candidate: &str) -> Result<bool, &'static str> {
  let pattern = canonicalize_glob(pattern)?;
  let pattern: Vec<&[u8]> = pattern.split('/').map(str::as_bytes).collect();
  let candidate: Vec<&[u8]> = candidate.split('/').filter(|segment| !segment.is_empty()).map(str::as_bytes).collect();
  let mut memo = vec![vec![None; candidate.len() + 1]; pattern.len() + 1];
  Ok(match_segments(&pattern, &candidate, 0, 0, &mut memo))
}

#[cfg(test)]
fn match_segments(pattern: &[&[u8]], candidate: &[&[u8]], p: usize, c: usize, memo: &mut [Vec<Option<bool>>]) -> bool {
  if let Some(result) = memo[p][c] {
    return result;
  }
  let result = if p == pattern.len() {
    c == candidate.len()
  } else if pattern[p] == b"**" {
    match_segments(pattern, candidate, p + 1, c, memo) || (c < candidate.len() && match_segments(pattern, candidate, p, c + 1, memo))
  } else {
    c < candidate.len() && match_bytes(pattern[p], candidate[c]) && match_segments(pattern, candidate, p + 1, c + 1, memo)
  };
  memo[p][c] = Some(result);
  result
}

#[cfg(test)]
fn match_bytes(pattern: &[u8], candidate: &[u8]) -> bool {
  let mut previous = vec![false; candidate.len() + 1];
  previous[0] = true;
  for token in pattern {
    let mut current = vec![false; candidate.len() + 1];
    match token {
      b'*' => {
        current[0] = previous[0];
        for index in 1..=candidate.len() {
          current[index] = previous[index] || current[index - 1];
        }
      }
      b'?' => {
        current[1..].copy_from_slice(&previous[..candidate.len()]);
      }
      literal => {
        for index in 1..=candidate.len() {
          current[index] = previous[index - 1] && *literal == candidate[index - 1];
        }
      }
    }
    previous = current;
  }
  previous[candidate.len()]
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
  bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
  bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, &'static str> {
  let raw = bytes.get(offset..offset + 2).ok_or("truncated")?;
  Ok(u16::from_le_bytes(raw.try_into().map_err(|_| "truncated")?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, &'static str> {
  let raw = bytes.get(offset..offset + 4).ok_or("truncated")?;
  Ok(u32::from_le_bytes(raw.try_into().map_err(|_| "truncated")?))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn scope_fixtures_match_results_and_ids() {
    for case in fixture_cases() {
      let (observed, key) = observe(case.profile, &case.bytes);
      assert_eq!(observed, case.expected, "fixture {}", case.id);
      assert_eq!(key, case.canonical_key, "fixture {} key", case.id);
    }
  }

  #[test]
  fn scope_fixture_mutations_are_structurally_or_semantically_protected() {
    for case in fixture_cases() {
      let mutation_offsets: Vec<usize> = if case.bytes.len() <= 4_096 {
        (0..case.bytes.len()).collect()
      } else {
        let mut offsets: Vec<usize> = (0..SCOPE_FIXED_LENGTH).collect();
        offsets.extend((SCOPE_FIXED_LENGTH..case.bytes.len()).step_by(4_096));
        offsets.extend([case.bytes.len() / 2, case.bytes.len() - 2, case.bytes.len() - 1]);
        offsets.sort_unstable();
        offsets.dedup();
        offsets
      };
      for index in mutation_offsets {
        let mut mutated = case.bytes.clone();
        mutated[index] ^= 0x01;
        let (observed, key) = observe(case.profile, &mutated);
        assert!(observed.starts_with("error:") || key != case.canonical_key, "fixture {} byte {index} was not protected", case.id);
      }
    }
  }

  #[test]
  fn scope_decoder_rejects_envelope_length_reserve_and_mode_failures() {
    let canonical = build_scope("/docs", Some("**/*.md")).unwrap();
    for truncated_length in [0, 31, 32, 63, canonical.len() - 1] {
      assert!(decode_scope(&canonical[..truncated_length]).is_err());
    }

    let mut trailing = canonical.clone();
    trailing.push(0);
    assert_eq!(decode_scope(&trailing).err(), Some("definition_metadata"));

    let mut reserved = canonical.clone();
    reserved[16] = 1;
    assert_eq!(decode_scope(&reserved).err(), Some("definition_metadata"));

    let mut mode = canonical.clone();
    put_u16(&mut mode, 42, 1);
    assert_eq!(decode_scope(&mode).err(), Some("scope_mode_length"));
  }

  #[test]
  fn scope_decoder_rejects_noncanonical_and_unknown_semantics() {
    let canonical = build_scope("/docs", Some("**/*.md")).unwrap();

    let mut wrong_magic = canonical.clone();
    wrong_magic[0] = b'X';
    assert_eq!(decode_scope(&wrong_magic).err(), Some("definition_envelope"));

    let mut unknown_class = canonical.clone();
    put_u16(&mut unknown_class, 40, 2);
    assert_eq!(decode_scope(&unknown_class).err(), Some("scope_semantics"));

    let mut unknown_mode = canonical.clone();
    put_u16(&mut unknown_mode, 42, 3);
    assert_eq!(decode_scope(&unknown_mode).err(), Some("scope_matching_mode"));

    let mut unknown_semantics = canonical.clone();
    put_u16(&mut unknown_semantics, 48, 2);
    assert_eq!(decode_scope(&unknown_semantics).err(), Some("scope_semantics"));

    let mut impossible_owner_length = canonical.clone();
    put_u32(&mut impossible_owner_length, 32, u32::MAX);
    assert_eq!(decode_scope(&impossible_owner_length).err(), Some("scope_body_length"));

    let mut noncanonical_owner = canonical.clone();
    noncanonical_owner[64..69].copy_from_slice(b"docs/");
    assert_eq!(decode_scope(&noncanonical_owner).err(), Some("scope_owner_noncanonical"));

    let mut invalid_owner_utf8 = canonical.clone();
    invalid_owner_utf8[64] = 0xff;
    assert_eq!(decode_scope(&invalid_owner_utf8).err(), Some("scope_owner_utf8"));

    let owner_end = 64 + read_u32(&canonical, 32).unwrap() as usize;
    let mut noncanonical_glob = canonical.clone();
    noncanonical_glob[owner_end] = b'/';
    assert_eq!(decode_scope(&noncanonical_glob).err(), Some("scope_glob_noncanonical"));
  }

  #[test]
  fn scope_compilation_canonicalizes_paths_globs_and_ids() {
    let left = build_scope(" /alpha//beta/../docs/ ", Some("/guides//**/*.md/")).unwrap();
    let right = build_scope("/alpha/docs", Some("guides/**/*.md")).unwrap();
    assert_eq!(left, right);
    assert_eq!(scope_id(HashProfile::Blake3_256, &left), scope_id(HashProfile::Blake3_256, &right));
    assert_ne!(scope_id(HashProfile::Blake3_256, &left), scope_id(HashProfile::Sha512, &left));
  }

  #[test]
  fn file_key_and_internal_path_rules_match_the_frozen_contract() {
    assert_eq!(
      file_key(HashProfile::Blake3_256, "/docs/./guide.md").unwrap(),
      file_key(HashProfile::Blake3_256, "/docs/guide.md").unwrap()
    );
    assert!(is_internal_path("/.aeordb-system/controls/a.ctrl").unwrap());
    assert!(is_internal_path("/docs/.aeordb-indexes/title.idx").unwrap());
    assert!(!is_internal_path("/docs/.aeordb-permissions").unwrap());
  }

  #[test]
  fn byte_glob_rules_cover_double_star_star_question_and_non_ascii() {
    assert!(glob_matches("guides/**/*.md", "guides/setup/linux/readme.md").unwrap());
    assert!(glob_matches("guides/**/*.md", "guides/readme.md").unwrap());
    assert!(!glob_matches("a?", "a\u{00e9}").unwrap());
    assert!(glob_matches("a??", "a\u{00e9}").unwrap());
    assert!(!glob_matches("*.MD", "readme.md").unwrap());
  }

  #[test]
  fn scope_bounds_and_invalid_globs_fail_closed() {
    let maximum_owner = format!("/{}", "a".repeat(65_470));
    assert_eq!(build_scope(&maximum_owner, Some("*")).unwrap().len(), SCOPE_MAX_LENGTH);
    assert_eq!(build_scope(&format!("{maximum_owner}a"), Some("*")).err(), Some("scope_length"));
    assert_eq!(build_scope("/docs", Some("../*.md")).err(), Some("glob_relative_segment"));
    assert_eq!(build_scope("/docs", Some("///")).err(), Some("glob_empty"));
    assert_eq!(build_scope("/docs", Some("bad\0glob")).err(), Some("glob_nul"));
  }
}
