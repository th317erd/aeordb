//! Storage-neutral correctness planning for selected immutable index coverage.

use std::error::Error;
use std::fmt;

use crate::engine::HashAlgorithm;

use super::namespace::SemanticUnavailableReasonV1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexCoverageGenerationHealthV1 {
  Healthy,
  Degraded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexCoverageGenerationV1<'a> {
  pub generation: u64,
  pub owner_id: &'a [u8],
  pub manifest_hash: &'a [u8],
  pub source_namespace_root: &'a [u8],
  pub coverage_epoch_id: &'a [u8],
  pub coverage_publication_sequence: u64,
  pub definition_fingerprint: &'a [u8],
  pub dependency_fingerprint: &'a [u8],
  pub health: IndexCoverageGenerationHealthV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExactIndexComplementProofV1<'a> {
  pub generation_manifest_hash: &'a [u8],
  pub source_namespace_root: &'a [u8],
  pub target_namespace_root: &'a [u8],
  pub coverage_epoch_id: &'a [u8],
  pub covered_through_publication_sequence: u64,
  pub target_publication_sequence: u64,
  pub changed_document_set_hash: &'a [u8],
  pub changed_document_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexSemanticQueryAvailabilityV1 {
  Complete,
  ContentOnly(SemanticUnavailableReasonV1),
  DependencyUnavailable,
}

#[derive(Clone, Copy, Debug)]
pub struct IndexCoveragePlanningRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub requested_namespace_root: &'a [u8],
  pub requested_publication_sequence: u64,
  pub required_owner_id: &'a [u8],
  pub required_definition_fingerprint: &'a [u8],
  pub required_dependency_fingerprint: &'a [u8],
  pub semantic_availability: IndexSemanticQueryAvailabilityV1,
  pub selected_generation: Option<IndexCoverageGenerationV1<'a>>,
  pub exact_complement: Option<ExactIndexComplementProofV1<'a>>,
  pub maximum_complement_documents: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexAuthoritativeFallbackReasonV1 {
  NoSelectedGeneration,
  IncompatibleOwner,
  IncompatibleDefinition,
  IncompatibleDependencies,
  ExactComplementUnavailable,
  DegradedPartialGeneration,
  ComplementWorkLimitExceeded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexHistoricalViewUnavailableReasonV1 {
  ContentOnly(SemanticUnavailableReasonV1),
  DependencyUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexCoveragePlanV1<'a> {
  Complete { generation: IndexCoverageGenerationV1<'a> },
  PartialExact { generation: IndexCoverageGenerationV1<'a>, complement: ExactIndexComplementProofV1<'a> },
  AuthoritativeOnly { reason: IndexAuthoritativeFallbackReasonV1 },
  HistoricalViewUnavailable { reason: IndexHistoricalViewUnavailableReasonV1 },
}

impl IndexCoveragePlanV1<'_> {
  pub const fn requires_authoritative_complement_scan(&self) -> bool {
    matches!(self, Self::PartialExact { .. })
  }

  pub const fn requires_candidate_recheck(&self) -> bool {
    matches!(self, Self::PartialExact { .. })
  }

  pub const fn requires_deduplication(&self) -> bool {
    matches!(self, Self::PartialExact { .. })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexCoveragePlanningErrorClassV1 {
  InvalidRequest,
  CorruptSelectedGeneration,
  InvalidComplementProof,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexCoveragePlanningErrorV1 {
  class: IndexCoveragePlanningErrorClassV1,
  code: &'static str,
  context: String,
}

impl IndexCoveragePlanningErrorV1 {
  pub const fn class(&self) -> IndexCoveragePlanningErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

impl fmt::Display for IndexCoveragePlanningErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.context)
  }
}

impl Error for IndexCoveragePlanningErrorV1 {}

pub fn plan_selected_index_coverage_v1<'a>(
  request: &IndexCoveragePlanningRequestV1<'a>,
) -> Result<IndexCoveragePlanV1<'a>, IndexCoveragePlanningErrorV1> {
  let hash_width = request.hash_algorithm.hash_length();
  validate_request(request, hash_width)?;

  match request.semantic_availability {
    IndexSemanticQueryAvailabilityV1::ContentOnly(reason) => {
      return Ok(IndexCoveragePlanV1::HistoricalViewUnavailable { reason: IndexHistoricalViewUnavailableReasonV1::ContentOnly(reason) });
    }
    IndexSemanticQueryAvailabilityV1::DependencyUnavailable => {
      return Ok(IndexCoveragePlanV1::HistoricalViewUnavailable { reason: IndexHistoricalViewUnavailableReasonV1::DependencyUnavailable });
    }
    IndexSemanticQueryAvailabilityV1::Complete => {}
  }

  let Some(generation) = request.selected_generation else {
    if request.exact_complement.is_some() {
      return Err(error(
        IndexCoveragePlanningErrorClassV1::InvalidComplementProof,
        "index_coverage_orphan_complement",
        "an exact complement cannot exist without a selected generation",
      ));
    }
    return Ok(IndexCoveragePlanV1::AuthoritativeOnly { reason: IndexAuthoritativeFallbackReasonV1::NoSelectedGeneration });
  };
  validate_generation(generation, hash_width)?;

  if generation.owner_id != request.required_owner_id {
    return Ok(IndexCoveragePlanV1::AuthoritativeOnly { reason: IndexAuthoritativeFallbackReasonV1::IncompatibleOwner });
  }
  if generation.definition_fingerprint != request.required_definition_fingerprint {
    return Ok(IndexCoveragePlanV1::AuthoritativeOnly { reason: IndexAuthoritativeFallbackReasonV1::IncompatibleDefinition });
  }
  if generation.dependency_fingerprint != request.required_dependency_fingerprint {
    return Ok(IndexCoveragePlanV1::AuthoritativeOnly { reason: IndexAuthoritativeFallbackReasonV1::IncompatibleDependencies });
  }

  if generation.source_namespace_root == request.requested_namespace_root {
    if request.exact_complement.is_some() {
      return Err(error(
        IndexCoveragePlanningErrorClassV1::InvalidComplementProof,
        "index_coverage_redundant_complement",
        "an exact-root generation must not carry a complement proof",
      ));
    }
    if generation.coverage_publication_sequence > request.requested_publication_sequence {
      return Err(error(
        IndexCoveragePlanningErrorClassV1::CorruptSelectedGeneration,
        "index_coverage_future_exact_generation",
        "an exact-root generation claims coverage after the requested root publication",
      ));
    }
    return Ok(IndexCoveragePlanV1::Complete { generation });
  }

  if generation.health == IndexCoverageGenerationHealthV1::Degraded {
    return Ok(IndexCoveragePlanV1::AuthoritativeOnly { reason: IndexAuthoritativeFallbackReasonV1::DegradedPartialGeneration });
  }
  let Some(complement) = request.exact_complement else {
    return Ok(IndexCoveragePlanV1::AuthoritativeOnly { reason: IndexAuthoritativeFallbackReasonV1::ExactComplementUnavailable });
  };
  validate_complement(request, generation, complement, hash_width)?;
  if complement.changed_document_count > request.maximum_complement_documents {
    return Ok(IndexCoveragePlanV1::AuthoritativeOnly { reason: IndexAuthoritativeFallbackReasonV1::ComplementWorkLimitExceeded });
  }

  Ok(IndexCoveragePlanV1::PartialExact { generation, complement })
}

fn validate_request(request: &IndexCoveragePlanningRequestV1<'_>, hash_width: usize) -> Result<(), IndexCoveragePlanningErrorV1> {
  for (label, value) in [
    ("requested namespace root", request.requested_namespace_root),
    ("required owner ID", request.required_owner_id),
    ("required definition fingerprint", request.required_definition_fingerprint),
    ("required dependency fingerprint", request.required_dependency_fingerprint),
  ] {
    validate_hash(value, hash_width, IndexCoveragePlanningErrorClassV1::InvalidRequest, "index_coverage_invalid_request", label)?;
  }
  if request.requested_publication_sequence == 0 || request.maximum_complement_documents == 0 {
    return Err(error(
      IndexCoveragePlanningErrorClassV1::InvalidRequest,
      "index_coverage_invalid_request",
      "requested publication sequence and complement-document bound must be nonzero",
    ));
  }
  Ok(())
}

fn validate_generation(generation: IndexCoverageGenerationV1<'_>, hash_width: usize) -> Result<(), IndexCoveragePlanningErrorV1> {
  for (label, value) in [
    ("selected owner ID", generation.owner_id),
    ("selected manifest hash", generation.manifest_hash),
    ("selected source namespace root", generation.source_namespace_root),
    ("selected definition fingerprint", generation.definition_fingerprint),
    ("selected dependency fingerprint", generation.dependency_fingerprint),
  ] {
    validate_hash(
      value,
      hash_width,
      IndexCoveragePlanningErrorClassV1::CorruptSelectedGeneration,
      "index_coverage_corrupt_generation",
      label,
    )?;
  }
  if generation.generation == 0
    || generation.coverage_publication_sequence == 0
    || generation.coverage_epoch_id.len() != 16
    || generation.coverage_epoch_id.iter().all(|byte| *byte == 0)
  {
    return Err(error(
      IndexCoveragePlanningErrorClassV1::CorruptSelectedGeneration,
      "index_coverage_corrupt_generation",
      "selected generation, coverage sequence, or coverage epoch is invalid",
    ));
  }
  Ok(())
}

fn validate_complement(
  request: &IndexCoveragePlanningRequestV1<'_>,
  generation: IndexCoverageGenerationV1<'_>,
  complement: ExactIndexComplementProofV1<'_>,
  hash_width: usize,
) -> Result<(), IndexCoveragePlanningErrorV1> {
  validate_hash(
    complement.changed_document_set_hash,
    hash_width,
    IndexCoveragePlanningErrorClassV1::InvalidComplementProof,
    "index_coverage_invalid_complement",
    "changed-document set hash",
  )?;
  if complement.generation_manifest_hash != generation.manifest_hash
    || complement.source_namespace_root != generation.source_namespace_root
    || complement.target_namespace_root != request.requested_namespace_root
    || complement.coverage_epoch_id != generation.coverage_epoch_id
    || complement.covered_through_publication_sequence != generation.coverage_publication_sequence
    || complement.target_publication_sequence != request.requested_publication_sequence
    || complement.covered_through_publication_sequence >= complement.target_publication_sequence
  {
    return Err(error(
      IndexCoveragePlanningErrorClassV1::InvalidComplementProof,
      "index_coverage_invalid_complement",
      "complement proof does not bind the exact selected generation and requested root interval",
    ));
  }
  Ok(())
}

fn validate_hash(
  value: &[u8],
  hash_width: usize,
  class: IndexCoveragePlanningErrorClassV1,
  code: &'static str,
  label: &'static str,
) -> Result<(), IndexCoveragePlanningErrorV1> {
  if value.len() != hash_width || value.iter().all(|byte| *byte == 0) {
    return Err(error(class, code, format!("{label} has the wrong width or is all zero")));
  }
  Ok(())
}

fn error(class: IndexCoveragePlanningErrorClassV1, code: &'static str, context: impl Into<String>) -> IndexCoveragePlanningErrorV1 {
  IndexCoveragePlanningErrorV1 { class, code, context: context.into() }
}
