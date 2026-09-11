use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// Bounds memory used for any checkpoint record produced by the qualification
/// workers. Their generated payloads are far smaller than this ceiling.
pub const SOAK_CHECKPOINT_RECORD_MAXIMUM_BYTES: usize = 4 * 1_024 * 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoakCheckpointRecord<'a> {
  Comment { text: &'a str },
  Committed { path: &'a str, body: Option<&'a str> },
  PendingWrite { path: &'a str },
  PendingDelete { path: &'a str },
  Deleted { path: &'a str },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoakCheckpointReadSummary {
  pub complete_lines: u64,
  pub ignored_incomplete_tail: bool,
}

pub fn visit_soak_checkpoint_records<F>(path: &Path, visit: F) -> Result<SoakCheckpointReadSummary, String>
where
  F: for<'record> FnMut(usize, SoakCheckpointRecord<'record>) -> Result<(), String>,
{
  let file = File::open(path).map_err(|error| format!("open checkpoint {}: {error}", path.display()))?;
  let (summary, _complete_byte_frontier) = visit_checkpoint_reader(file, path, visit)?;
  Ok(summary)
}

/// Admit a worker-owned, read/write checkpoint before writing its next record.
/// Readers ignore an unterminated final record, but an appending worker must
/// durably remove that fragment so a later newline cannot turn it into a record.
/// Completed malformed or oversized records remain fatal and are never changed.
/// The caller must exclusively own this checkpoint (and its database) throughout
/// admission and subsequent writes. Report-only readers must not call this.
/// Open with write access, not only append access: Windows requires it for
/// truncation. On success the handle is positioned at the complete-prefix end.
pub fn prepare_soak_checkpoint_append(file: &mut File, path: &Path) -> Result<(), String> {
  prepare_checkpoint_append_with_barrier(file, path, aeordb::engine::native_durability::sync_file_data_native)
}

fn prepare_checkpoint_append_with_barrier(
  file: &mut File,
  path: &Path,
  barrier: impl FnOnce(&File) -> aeordb::engine::native_durability::NativeDurabilityResult<()>,
) -> Result<(), String> {
  file.seek(SeekFrom::Start(0)).map_err(|error| format!("seek checkpoint {}: {error}", path.display()))?;
  let (summary, complete_byte_frontier) = visit_checkpoint_reader(&mut *file, path, |_line, _record| Ok(()))?;
  if summary.ignored_incomplete_tail {
    file
      .set_len(complete_byte_frontier)
      .map_err(|error| format!("truncate incomplete checkpoint tail {} at byte {complete_byte_frontier}: {error}", path.display()))?;
    barrier(file).map_err(|error| format!("checkpoint tail truncation durability barrier failed for {}: {error}", path.display()))?;
  }
  file.seek(SeekFrom::End(0)).map_err(|error| format!("seek checkpoint append frontier {}: {error}", path.display()))?;
  Ok(())
}

fn visit_checkpoint_reader<R: Read, F>(file: R, path: &Path, mut visit: F) -> Result<(SoakCheckpointReadSummary, u64), String>
where
  F: for<'record> FnMut(usize, SoakCheckpointRecord<'record>) -> Result<(), String>,
{
  let mut reader = BufReader::new(file);
  let mut line_bytes = Vec::new();
  let mut complete_lines = 0u64;
  let mut line_number = 0usize;
  let mut ignored_incomplete_tail = false;
  let mut complete_byte_frontier = 0u64;

  loop {
    line_bytes.clear();
    let bytes_read = Read::by_ref(&mut reader)
      .take((SOAK_CHECKPOINT_RECORD_MAXIMUM_BYTES + 1) as u64)
      .read_until(b'\n', &mut line_bytes)
      .map_err(|error| format!("read checkpoint {} line {}: {error}", path.display(), line_number + 1))?;
    if bytes_read == 0 {
      break;
    }
    line_number += 1;

    if line_bytes.len() > SOAK_CHECKPOINT_RECORD_MAXIMUM_BYTES {
      return Err(format!(
        "checkpoint {} line {line_number} exceeds the {SOAK_CHECKPOINT_RECORD_MAXIMUM_BYTES}-byte record limit",
        path.display()
      ));
    }
    if !line_bytes.ends_with(b"\n") {
      ignored_incomplete_tail = true;
      break;
    }
    line_bytes.pop();
    if line_bytes.ends_with(b"\r") {
      line_bytes.pop();
    }

    let line = std::str::from_utf8(&line_bytes)
      .map_err(|error| format!("read checkpoint {} line {line_number}: invalid UTF-8: {error}", path.display()))?;
    let record =
      parse_soak_checkpoint_record(line).map_err(|error| format!("malformed checkpoint {} line {line_number}: {error}", path.display()))?;
    visit(line_number, record).map_err(|error| format!("checkpoint {} line {line_number} rejected: {error}", path.display()))?;
    complete_byte_frontier = complete_byte_frontier
      .checked_add(bytes_read as u64)
      .ok_or_else(|| format!("checkpoint {} byte frontier overflow at line {line_number}", path.display()))?;
    complete_lines += 1;
  }

  Ok((SoakCheckpointReadSummary { complete_lines, ignored_incomplete_tail }, complete_byte_frontier))
}

fn parse_soak_checkpoint_record(line: &str) -> Result<SoakCheckpointRecord<'_>, &'static str> {
  if line.is_empty() || line.starts_with('#') {
    return Ok(SoakCheckpointRecord::Comment { text: line });
  }
  if let Some(rest) = line.strip_prefix("+\t") {
    let (path, body) = match rest.split_once('\t') {
      Some((path, body)) => (path, Some(body)),
      None => (rest, None),
    };
    require_checkpoint_path(path)?;
    return Ok(SoakCheckpointRecord::Committed { path, body });
  }
  if let Some(path) = line.strip_prefix("!\t") {
    require_checkpoint_operation_path(path)?;
    return Ok(SoakCheckpointRecord::PendingWrite { path });
  }
  if let Some(path) = line.strip_prefix("?\t") {
    require_checkpoint_operation_path(path)?;
    return Ok(SoakCheckpointRecord::PendingDelete { path });
  }
  if let Some(path) = line.strip_prefix("-\t") {
    require_checkpoint_operation_path(path)?;
    return Ok(SoakCheckpointRecord::Deleted { path });
  }
  if let Some((path, body)) = line.split_once('\t') {
    require_checkpoint_path(path)?;
    return Ok(SoakCheckpointRecord::Committed { path, body: Some(body) });
  }
  Err("expected a comment, committed record, pending intent, or delete record")
}

fn require_checkpoint_path(path: &str) -> Result<(), &'static str> {
  if path.is_empty() {
    return Err("checkpoint path cannot be empty");
  }
  Ok(())
}

fn require_checkpoint_operation_path(path: &str) -> Result<(), &'static str> {
  require_checkpoint_path(path)?;
  if path.contains('\t') {
    return Err("checkpoint operation path cannot contain a tab");
  }
  Ok(())
}

#[cfg(test)]
#[path = "../spec/soak_checkpoint_internal_spec.rs"]
mod soak_checkpoint_internal_spec;
