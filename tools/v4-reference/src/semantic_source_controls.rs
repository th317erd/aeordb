//! Independent sequential oracle; no AeorDB codec, hash helper or registry imports.
use crate::core::HashProfile;

#[cfg(test)]
#[path = "../spec/semantic_source_controls_spec.rs"]
mod semantic_source_controls_spec;

struct Input<'a>(&'a [u8]);

impl<'a> Input<'a> {
  fn take(&mut self, length: usize) -> Result<&'a [u8], &'static str> {
    let result = self.0.get(..length).ok_or("semantic_source_length")?;
    self.0 = &self.0[length..];
    Ok(result)
  }

  fn number<const N: usize>(&mut self) -> Result<[u8; N], &'static str> {
    self.take(N)?.try_into().map_err(|_| "semantic_source_length")
  }

  fn u64(&mut self) -> Result<u64, &'static str> {
    Ok(u64::from_le_bytes(self.number()?))
  }

  fn path(&mut self) -> Result<&'a str, &'static str> {
    let length = u32::from_le_bytes(self.number()?) as usize;
    require((1..=65_535).contains(&length))?;
    let path = std::str::from_utf8(self.take(length)?).map_err(|_| "semantic_source_path")?;
    require(path.starts_with('/') && !path.contains('\0') && path.trim() == path)?;
    require(path == "/" || path[1..].split('/').all(|part| !matches!(part, "" | "." | "..")))?;
    Ok(path)
  }
}

fn present(bytes: &[u8]) -> bool {
  bytes.iter().any(|byte| *byte != 0)
}

fn require(condition: bool) -> Result<(), &'static str> {
  if condition {
    Ok(())
  } else {
    Err("semantic_source_fields")
  }
}

pub(super) fn validate(profile: HashProfile, kind: u16, bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
  require(bytes.len() <= 1_048_576)?;
  let mut input = Input(bytes);
  require(present(input.take(16)?))?;
  if kind == 0x0048 {
    let task = input.take(16)?;
    let sequence = input.u64()?;
    require(present(task) && sequence != 0 && present(input.take(16)?))?;
    for _ in 0..3 {
      require(input.u64()? != 0)?;
    }
    require(input.u64()? <= i64::MAX as u64)?;
    let paths = input.u64()?;
    let maximum = paths.checked_mul(2).and_then(|number| number.checked_sub(1)).ok_or("semantic_source_counts")?;
    for _ in 0..2 {
      let nodes = input.u64()?;
      require(nodes != 0 && nodes <= maximum)?;
    }
    for _ in 0..6 {
      require(present(input.take(profile.width())?))?;
    }
    require(input.0.is_empty())?;
    let mut identity = task.to_vec();
    identity.extend_from_slice(&sequence.to_le_bytes());
    return Ok(identity);
  }
  require(kind == 0x0049)?;
  let node_kind = u16::from_le_bytes(input.number()?);
  require(matches!(node_kind, 1 | 2) && input.number::<2>()? == [0; 2])?;
  let count = u32::from_le_bytes(input.number()?) as usize;
  require(count != 0 && count <= if node_kind == 1 { 256 } else { 128 })?;
  let payload_length = u32::from_le_bytes(input.number()?) as usize;
  require(input.number::<4>()? == [0; 4] && payload_length == input.0.len())?;
  let mut children = std::collections::BTreeSet::new();
  if node_kind == 2 {
    let first = input.take(profile.width())?;
    require(present(first))?;
    children.insert(first);
  }
  let mut previous: Option<&str> = None;
  for _ in 0..count {
    let path = input.path()?;
    require(previous.is_none_or(|prior| prior < path))?;
    previous = Some(path);
    let identity = input.take(profile.width())?;
    if node_kind == 2 {
      require(present(identity) && children.insert(identity))?;
    }
  }
  require(input.0.is_empty())?;
  let mut preimage = b"aeordb.semantic-source-node.v1\0".to_vec();
  preimage.extend_from_slice(bytes);
  Ok(profile.digest(&preimage))
}

pub(super) fn body(profile: HashProfile, kind: u16) -> Vec<u8> {
  if kind == 0x0049 {
    return node_body(profile, false);
  }
  assert_eq!(kind, 0x0048);
  let checkpoint_body = super::semantic_mutation_controls::body(profile, 0x0045);
  let checkpoint = super::build_control(super::ControlKind::SemanticMutationCheckpoint, 1, &checkpoint_body);
  let mut bytes = checkpoint_body[..88].to_vec();
  for value in [2u64, 1, 1] {
    bytes.extend_from_slice(&value.to_le_bytes());
  }
  for fill in [1u8, 2, 0x31, 0x41, 9] {
    bytes.resize(bytes.len() + profile.width(), fill);
  }
  bytes.extend_from_slice(&profile.digest(&checkpoint));
  assert_eq!(bytes.len(), 112 + 6 * profile.width());
  bytes
}

pub(super) fn node_body(profile: HashProfile, internal: bool) -> Vec<u8> {
  let mut bytes = vec![1; 16];
  bytes.extend_from_slice(&(if internal { 2u16 } else { 1u16 }).to_le_bytes());
  bytes.extend_from_slice(&0u16.to_le_bytes());
  bytes.extend_from_slice(&(if internal { 1u32 } else { 2u32 }).to_le_bytes());
  bytes.extend_from_slice(&[0; 8]);
  if internal {
    bytes.resize(bytes.len() + profile.width(), 0x31);
  }
  let rows: &[(&str, u8)] = if internal {
    &[("/.aeordb-config/parsers.json", 0x41)]
  } else {
    &[("/.aeordb-config/indexes.json", 0), ("/.aeordb-config/parsers.json", 0x51)]
  };
  for (path, fill) in rows {
    bytes.extend_from_slice(&(path.len() as u32).to_le_bytes());
    bytes.extend_from_slice(path.as_bytes());
    bytes.resize(bytes.len() + profile.width(), *fill);
  }
  let payload_length = (bytes.len() - 32) as u32;
  bytes[24..28].copy_from_slice(&payload_length.to_le_bytes());
  bytes
}
