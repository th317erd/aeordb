//! Independent sequential oracle for the additive Round 17 control bodies.
use crate::core::HashProfile;

#[cfg(test)]
#[path = "../spec/semantic_mutation_controls_spec.rs"]
mod semantic_mutation_controls_spec;

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
  fn take(&mut self, length: usize) -> Result<&'a [u8], &'static str> {
    let bytes = self.0.get(..length).ok_or("semantic_task_length")?;
    self.0 = &self.0[length..];
    Ok(bytes)
  }

  fn number<const N: usize>(&mut self) -> Result<[u8; N], &'static str> {
    self.take(N)?.try_into().map_err(|_| "semantic_task_length")
  }

  fn u16(&mut self) -> Result<u16, &'static str> {
    Ok(u16::from_le_bytes(self.number()?))
  }

  fn u32(&mut self) -> Result<u32, &'static str> {
    Ok(u32::from_le_bytes(self.number()?))
  }

  fn u64(&mut self) -> Result<u64, &'static str> {
    Ok(u64::from_le_bytes(self.number()?))
  }
}

fn present(value: &[u8]) -> bool {
  value.iter().any(|byte| *byte != 0)
}

fn require(condition: bool) -> Result<(), &'static str> {
  if condition {
    Ok(())
  } else {
    Err("semantic_task_fields")
  }
}

fn tree(hash: &[u8], records: u64, nodes: u64) -> Result<(), &'static str> {
  if !present(hash) {
    return require(records == 0 && nodes == 0);
  }
  let maximum_nodes = records.checked_mul(2).and_then(|value| value.checked_sub(1)).ok_or("semantic_task_counts")?;
  require(records > 0 && nodes > 0 && nodes <= maximum_nodes)
}

pub(super) fn validate(profile: HashProfile, kind: u16, bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
  let mut input = Cursor(bytes);
  require(present(input.take(16)?))?;
  if kind == 0x0046 {
    require(input.0.is_empty())?;
    return Ok(Vec::new());
  }
  let task = input.take(16)?;
  require(present(task))?;
  if kind == 0x0044 {
    require(present(input.take(16)?) && present(input.take(16)?))?;
    require(input.u64()? != 0 && input.u64()? != 0)?;
    let created = input.u64()?;
    let updated = input.u64()?;
    require(created <= updated && updated <= i64::MAX as u64)?;
    let state = input.u16()?;
    let flags = input.u16()?;
    require((1..=9).contains(&state) && flags & !1 == 0 && (flags == 0 || state >= 6))?;
    require(input.u64()? != 0 && input.u32()? == 0 && present(input.take(profile.width())?))?;
    require(input.0.is_empty())?;
    return Ok(task.to_vec());
  }
  require(kind == 0x0045)?;
  let sequence = input.u64()?;
  require(sequence != 0 && present(input.take(16)?) && input.u64()? != 0)?;
  let generation = input.u64()?;
  require(generation != 0 && input.u64()? != 0 && input.u64()? <= i64::MAX as u64)?;
  let phase = input.u16()?;
  let cursor_kind = input.u16()?;
  let cursor_length = input.u32()? as usize;
  require((1..=5).contains(&phase) && cursor_length <= 65_535)?;
  let expected_configurations = input.u64()?;
  let configurations = input.u64()?;
  let records = input.u64()?;
  let nodes = input.u64()?;
  let dependencies = input.u64()?;
  require(input.u64()? != 0)?;
  let activation = input.u64()?;
  let pruning_records = input.u64()?;
  let pruning_nodes = input.u64()?;
  let mut hashes = [&[][..]; 9];
  for hash in &mut hashes {
    *hash = input.take(profile.width())?;
  }
  let cursor = input.take(cursor_length)?;
  require(input.0.is_empty())?;
  require([0, 1, 6, 7, 8].into_iter().all(|slot| present(hashes[slot])))?;
  tree(hashes[2], records, nodes)?;
  tree(hashes[3], pruning_records, pruning_nodes)?;
  require(dependencies <= records && configurations <= records)?;
  if phase == 1 {
    require(hashes[2..6].iter().all(|hash| !present(hash)) && cursor.is_empty())?;
  } else if phase < 4 {
    require(!present(hashes[4]) && !present(hashes[5]))?;
  } else {
    require(present(hashes[2]) && !present(hashes[3]) && present(hashes[4]) && present(hashes[5]))?;
    require(configurations == expected_configurations && cursor.is_empty())?;
  }
  require(if phase == 5 { generation.checked_add(1) == Some(activation) } else { activation == 0 })?;
  match cursor_kind {
    0 => require(cursor.is_empty())?,
    1 => {
      require(phase == 2 && !cursor.is_empty())?;
      let path = std::str::from_utf8(cursor).map_err(|_| "semantic_task_cursor")?;
      require(path.starts_with('/') && !path.contains('\0') && path.trim() == path && (path == "/" || !path.ends_with('/')))?;
      require(path == "/" || path[1..].split('/').all(|part| !matches!(part, "" | "." | "..")))?;
    }
    2 => require(phase == 3 && cursor.len() == profile.width() && present(cursor))?,
    _ => return Err("semantic_task_cursor"),
  }
  let mut identity = task.to_vec();
  identity.extend_from_slice(&sequence.to_le_bytes());
  Ok(identity)
}

pub(super) fn body(profile: HashProfile, kind: u16) -> Vec<u8> {
  let mut bytes = vec![1; 16];
  if kind == 0x0046 {
    return bytes;
  }
  bytes.extend_from_slice(&[2; 16]);
  if kind == 0x0044 {
    bytes.extend_from_slice(&[3; 16]);
    bytes.extend_from_slice(&[4; 16]);
    for value in [1u64, 1, 100, 101] {
      bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&1u64.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.resize(112 + profile.width(), 0xa1);
    return bytes;
  }
  assert_eq!(kind, 0x0045);
  bytes.extend_from_slice(&1u64.to_le_bytes());
  bytes.extend_from_slice(&[3; 16]);
  for value in [1u64, 1, 1, 100] {
    bytes.extend_from_slice(&value.to_le_bytes());
  }
  bytes.extend_from_slice(&4u16.to_le_bytes());
  bytes.extend_from_slice(&0u16.to_le_bytes());
  bytes.extend_from_slice(&0u32.to_le_bytes());
  for value in [2u64, 2, 3, 5, 1, 1, 0, 0, 0] {
    bytes.extend_from_slice(&value.to_le_bytes());
  }
  for fill in [1u8, 2, 3, 0, 5, 6, 7, 8, 9] {
    bytes.resize(bytes.len() + profile.width(), fill);
  }
  assert_eq!(bytes.len(), 168 + 9 * profile.width());
  bytes
}
