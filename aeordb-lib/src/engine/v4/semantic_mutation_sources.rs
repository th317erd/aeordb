//! Ordered source-identity comparison; not capture, retention or root authority.
use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};
use super::super::hash::IncrementalDigestV1;
use super::super::parser_registry_compiler::SemanticCompilationErrorV1;
use super::super::scope::validate_canonical_absolute_path;

const ERROR_PATH: &str = "<semantic-mutation-sources>";
const DOMAIN: &[u8] = b"aeordb.semantic-mutation-sources.v1\0";

#[derive(Clone, Copy, Debug)]
pub struct SemanticMutationSourceFingerprintRequestV1 {
  pub hash_algorithm: HashAlgorithm,
  pub expected_record_count: u64,
  /// Bounds both path length and its owned allocation capacity.
  pub maximum_path_bytes: usize,
  pub maximum_workspace_bytes: usize,
}

#[derive(Clone, Debug)]
pub struct SemanticMutationSourceIdentityV1 {
  pub path: String,
  /// None is an explicitly captured absence, never a failed lookup. Present
  /// identities are nonzero FileRecord hashes, not file keys or content hashes.
  /// Its owned buffer must not retain capacity beyond the selected hash width.
  pub file_record_id: Option<Vec<u8>>,
}

pub struct SemanticMutationSourceFingerprintV1 {
  digest: Vec<u8>,
  record_count: u64,
  _memory: MemoryReservation,
}

impl SemanticMutationSourceFingerprintV1 {
  pub fn digest(&self) -> &[u8] {
    &self.digest
  }
  pub const fn record_count(&self) -> u64 {
    self.record_count
  }
}

/// Compare a counted, strictly path-ordered source stream. The source owner
/// must separately prove complete base/request control, alias and module
/// enumeration under pinned immutable trees. This digest neither acquires a
/// pin nor proves capture completeness or execution availability. The source
/// owner separately accounts for its iterator/backing storage. This helper
/// retains only the previous path, current row, hash state and final digest.
pub fn fingerprint_semantic_mutation_sources_v1(
  request: SemanticMutationSourceFingerprintRequestV1,
  sources: impl IntoIterator<Item = Result<SemanticMutationSourceIdentityV1, SemanticCompilationErrorV1>>,
  memory: &MemoryCoordinator,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<SemanticMutationSourceFingerprintV1, SemanticCompilationErrorV1> {
  if is_cancelled() {
    return Err(SemanticCompilationErrorV1::Cancelled);
  }
  if request.maximum_path_bytes == 0 {
    return Err(resource("path bound must be nonempty"));
  }
  u32::try_from(request.maximum_path_bytes).map_err(|error| resource(error.to_string()))?;
  let width = request.hash_algorithm.hash_length();
  let workspace = request
    .maximum_path_bytes
    .checked_mul(2)
    .and_then(|bytes| width.checked_mul(2).and_then(|identities| bytes.checked_add(identities)))
    .and_then(|bytes| bytes.checked_add(std::mem::size_of::<IncrementalDigestV1>()))
    .and_then(|bytes| bytes.checked_add(1024))
    .filter(|bytes| *bytes <= request.maximum_workspace_bytes)
    .ok_or_else(|| resource("source fingerprint workspace exceeds the caller ceiling"))?;
  let workspace = u64::try_from(workspace).map_err(|error| resource(error.to_string()))?;
  let mut reservation =
    memory.reserve(MemoryOwner::Task, workspace, AdmissionClass::Workload).map_err(|error| resource(error.to_string()))?;
  check(&reservation, is_cancelled)?;
  let mut digest = IncrementalDigestV1::new(request.hash_algorithm);
  digest.update(DOMAIN);
  let mut previous_path: Option<String> = None;
  let mut record_count = 0u64;
  let mut sources = sources.into_iter();
  loop {
    check(&reservation, is_cancelled)?;
    let next = sources.next();
    check(&reservation, is_cancelled)?;
    let Some(row) = next else { break };
    // A read error is not an absence or a count mismatch, including the final
    // read which establishes that the expected stream really ended.
    let row = row?;
    if record_count == request.expected_record_count {
      return Err(invalid("source stream has more records than captured"));
    }
    if row.path.len() > request.maximum_path_bytes || row.path.capacity() > request.maximum_path_bytes {
      return Err(resource("source path exceeds the admitted owned-buffer bound"));
    }
    validate_canonical_absolute_path(&row.path).map_err(|error| invalid(error.to_string()))?;
    if previous_path.as_ref().is_some_and(|previous| previous.as_bytes() >= row.path.as_bytes()) {
      return Err(invalid("source paths must be strictly increasing and unique"));
    }
    if let Some(identity) = &row.file_record_id {
      if identity.len() != width || identity.iter().all(|byte| *byte == 0) {
        return Err(invalid("present FileRecord identity must be nonzero and selected-hash width"));
      }
      if identity.capacity() > width {
        return Err(resource("source identity exceeds the admitted owned-buffer bound"));
      }
    }
    let path_length = u32::try_from(row.path.len()).map_err(|error| resource(error.to_string()))?;
    digest.update(&path_length.to_le_bytes());
    digest.update(row.path.as_bytes());
    if let Some(identity) = &row.file_record_id {
      digest.update(identity);
    } else {
      // Absence uses the selected algorithm's entire width, not a fixed-size
      // substitute or an additional marker in the ratified preimage.
      for _ in 0..width {
        digest.update(&[0]);
      }
    }
    previous_path = Some(row.path);
    record_count = record_count.checked_add(1).ok_or_else(|| invalid("source record count overflow"))?;
  }
  if record_count != request.expected_record_count {
    return Err(invalid("source stream ended before its captured record count"));
  }
  drop(sources);
  drop(previous_path);
  check(&reservation, is_cancelled)?;
  let digest = digest.try_finalize().map_err(|error| resource(error.to_string()))?;
  reservation.shrink(workspace - width as u64).map_err(|error| resource(error.to_string()))?;
  check(&reservation, is_cancelled)?;
  Ok(SemanticMutationSourceFingerprintV1 { digest, record_count, _memory: reservation })
}

fn check(reservation: &MemoryReservation, is_cancelled: &dyn Fn() -> bool) -> Result<(), SemanticCompilationErrorV1> {
  if is_cancelled() {
    return Err(SemanticCompilationErrorV1::Cancelled);
  }
  reservation.check_admission().map_err(|error| resource(error.to_string()))
}

fn resource(message: impl Into<String>) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::Resource { path: ERROR_PATH, message: message.into() }
}

fn invalid(message: impl Into<String>) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::InvalidSource { path: ERROR_PATH, message: message.into() }
}
