//! Shared authoritative ValueStore source evaluation.

use std::mem::size_of;

use thiserror::Error;

use crate::engine::HashAlgorithm;
use crate::engine::file_record::FileRecord;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};

use super::config_value::CANONICAL_CONFIG_VALUE_MAX_RETAINED_BYTES_PER_NODE_V1;
use super::index_producer_collector::{
  IndexParserDeterministicFailureV1, IndexParserExecutionErrorClassV1, IndexParserExecutionErrorV1, IndexParserExecutionRequestV1,
  IndexParserExecutorV1, IndexParserOutcomeV1,
};
use super::index_source::{PluginMapperExecutorV1, SourceDocumentV1, SourceExtractionV1, SourceOperationalErrorV1, ValueStoreRuntimeV1};
use super::parser_plan::{ParserPlanKind, ParserResolutionPlanV1};
use super::value_store::ValueStoreDefinitionV1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthoritativeSourceMemoryPolicyV1 {
  parser_owner: MemoryOwner,
  parser_admission: AdmissionClass,
  source_owner: MemoryOwner,
  source_admission: AdmissionClass,
}

impl AuthoritativeSourceMemoryPolicyV1 {
  pub const fn producer() -> Self {
    Self {
      parser_owner: MemoryOwner::ParserPlugin,
      parser_admission: AdmissionClass::Maintenance,
      source_owner: MemoryOwner::Task,
      source_admission: AdmissionClass::Workload,
    }
  }

  pub const fn selected_query() -> Self {
    Self {
      parser_owner: MemoryOwner::Query,
      parser_admission: AdmissionClass::Workload,
      source_owner: MemoryOwner::Query,
      source_admission: AdmissionClass::Workload,
    }
  }
}

#[derive(Clone, Copy)]
pub struct AuthoritativeSourceDocumentV1<'a> {
  pub namespace_root: &'a [u8],
  pub record_revision_hash: &'a [u8],
  pub file_record: &'a FileRecord,
}

pub enum AuthoritativeSourceEvaluationV1 {
  Missing,
  Values { values: Vec<Vec<u8>>, retained_bytes: u64, reservation: MemoryReservation },
  ParserUnindexable { failure: IndexParserDeterministicFailureV1, retained_bytes: u64, reservation: MemoryReservation },
  SourceUnindexable { code: &'static str, context: String, retained_bytes: u64, reservation: MemoryReservation },
}

impl AuthoritativeSourceEvaluationV1 {
  pub const fn retained_bytes(&self) -> u64 {
    match self {
      Self::Missing => 0,
      Self::Values { retained_bytes, .. }
      | Self::ParserUnindexable { retained_bytes, .. }
      | Self::SourceUnindexable { retained_bytes, .. } => *retained_bytes,
    }
  }
}

#[derive(Debug, Error)]
pub enum AuthoritativeSourceEvaluationErrorV1 {
  #[error("invalid authoritative source evaluation: {code}: {context}")]
  InvalidConfiguration { code: &'static str, context: String },
  #[error("authoritative source evaluation was cancelled")]
  Cancelled,
  #[error("authoritative source evaluation is under resource pressure: {0}")]
  ResourcePressure(String),
  #[error("authoritative parser failed: {0:?}")]
  Parser(IndexParserExecutionErrorV1),
  #[error("authoritative selector failed: {0}")]
  Source(#[source] SourceOperationalErrorV1),
}

pub struct AuthoritativeSourceEvaluatorV1<'definition> {
  runtime: ValueStoreRuntimeV1<'definition>,
  memory: MemoryCoordinator,
  policy: AuthoritativeSourceMemoryPolicyV1,
  parser_maximum_retained_bytes: u64,
  source_maximum_retained_bytes: u64,
}

impl<'definition> AuthoritativeSourceEvaluatorV1<'definition> {
  pub fn from_encoded(
    encoded_definition: &'definition [u8],
    hash_algorithm: HashAlgorithm,
    expected_scope_id: &[u8],
    expected_value_store_id: &[u8],
    memory: MemoryCoordinator,
    policy: AuthoritativeSourceMemoryPolicyV1,
  ) -> Result<Self, AuthoritativeSourceEvaluationErrorV1> {
    let runtime = ValueStoreRuntimeV1::from_encoded(encoded_definition, hash_algorithm).map_err(|error| {
      AuthoritativeSourceEvaluationErrorV1::InvalidConfiguration { code: error.code(), context: error.context().to_string() }
    })?;
    if runtime.definition().scope_id != expected_scope_id || runtime.definition().value_store_id != expected_value_store_id {
      return Err(AuthoritativeSourceEvaluationErrorV1::InvalidConfiguration {
        code: "authoritative_source_identity",
        context: "ValueStore definition does not match its selected scope and identity".to_string(),
      });
    }
    let parser_maximum_retained_bytes = parser_transient_bytes(&runtime.definition().parser_plan)?;
    let source_maximum_retained_bytes = source_transient_bytes(runtime.definition())?;
    Ok(Self { runtime, memory, policy, parser_maximum_retained_bytes, source_maximum_retained_bytes })
  }

  pub fn definition(&self) -> &ValueStoreDefinitionV1<'definition> {
    self.runtime.definition()
  }

  pub fn maximum_outcome_retained_bytes(&self) -> u64 {
    self.parser_maximum_retained_bytes.max(self.source_maximum_retained_bytes)
  }

  pub fn evaluate(
    &self,
    document: AuthoritativeSourceDocumentV1<'_>,
    parser: &dyn IndexParserExecutorV1,
    mapper: Option<&dyn PluginMapperExecutorV1>,
    is_cancelled: &dyn Fn() -> bool,
  ) -> Result<AuthoritativeSourceEvaluationV1, AuthoritativeSourceEvaluationErrorV1> {
    if is_cancelled() {
      return Err(AuthoritativeSourceEvaluationErrorV1::Cancelled);
    }
    let parser_memory = if self.definition().parser_plan.kind == ParserPlanKind::None { None } else { Some(self.reserve_parser()?) };
    let parsed = if self.definition().parser_plan.kind == ParserPlanKind::None {
      None
    } else {
      match parser.parse(IndexParserExecutionRequestV1::new(
        document.namespace_root,
        document.record_revision_hash,
        document.file_record,
        &self.definition().parser_plan,
        &self.definition().dependencies,
        self.definition().max_document_input_bytes,
        is_cancelled,
      )) {
        Ok(IndexParserOutcomeV1::Parsed(value)) => Some(value),
        Ok(IndexParserOutcomeV1::NotApplicable) => return Ok(AuthoritativeSourceEvaluationV1::Missing),
        Ok(IndexParserOutcomeV1::DeterministicUnindexable(failure)) => {
          let mut reservation = parser_memory.ok_or_else(|| AuthoritativeSourceEvaluationErrorV1::InvalidConfiguration {
            code: "authoritative_source_parser_plan",
            context: "parser produced output for a parser-free ValueStore".to_string(),
          })?;
          let retained_bytes =
            u64::try_from(failure.evidence_capacity()).map_err(|error| invalid("authoritative_parser_evidence", error))?;
          shrink_reservation(&mut reservation, retained_bytes)?;
          return Ok(AuthoritativeSourceEvaluationV1::ParserUnindexable { failure, retained_bytes, reservation });
        }
        Err(error) if error.class() == IndexParserExecutionErrorClassV1::Cancelled => {
          return Err(AuthoritativeSourceEvaluationErrorV1::Cancelled);
        }
        Err(error) => return Err(AuthoritativeSourceEvaluationErrorV1::Parser(error)),
      }
    };
    if is_cancelled() {
      return Err(AuthoritativeSourceEvaluationErrorV1::Cancelled);
    }
    let mut source_memory = self.reserve_source()?;
    let extracted = self
      .runtime
      .extract(SourceDocumentV1 { file_record: document.file_record, parsed_value: parsed.as_ref() }, mapper, is_cancelled)
      .map_err(|error| {
        if error.class() == super::index_source::SourceOperationalErrorClassV1::Cancelled {
          AuthoritativeSourceEvaluationErrorV1::Cancelled
        } else {
          AuthoritativeSourceEvaluationErrorV1::Source(error)
        }
      })?;
    match extracted {
      SourceExtractionV1::Missing => Ok(AuthoritativeSourceEvaluationV1::Missing),
      SourceExtractionV1::Values(values) => {
        let retained_bytes = retained_value_bytes(&values, values.capacity())?;
        shrink_reservation(&mut source_memory, retained_bytes)?;
        Ok(AuthoritativeSourceEvaluationV1::Values { values, retained_bytes, reservation: source_memory })
      }
      SourceExtractionV1::DeterministicUnindexable { code, context } => {
        let retained_bytes = u64::try_from(context.capacity()).map_err(|error| invalid("authoritative_source_context", error))?;
        shrink_reservation(&mut source_memory, retained_bytes)?;
        Ok(AuthoritativeSourceEvaluationV1::SourceUnindexable { code, context, retained_bytes, reservation: source_memory })
      }
    }
  }

  fn reserve_parser(&self) -> Result<MemoryReservation, AuthoritativeSourceEvaluationErrorV1> {
    self
      .memory
      .reserve(self.policy.parser_owner, self.parser_maximum_retained_bytes.max(1), self.policy.parser_admission)
      .map_err(|error| AuthoritativeSourceEvaluationErrorV1::ResourcePressure(error.to_string()))
  }

  fn reserve_source(&self) -> Result<MemoryReservation, AuthoritativeSourceEvaluationErrorV1> {
    self
      .memory
      .reserve(self.policy.source_owner, self.source_maximum_retained_bytes.max(1), self.policy.source_admission)
      .map_err(|error| AuthoritativeSourceEvaluationErrorV1::ResourcePressure(error.to_string()))
  }
}

fn source_transient_bytes(definition: &ValueStoreDefinitionV1<'_>) -> Result<u64, AuthoritativeSourceEvaluationErrorV1> {
  let vector_bytes = u64::from(definition.max_source_values_per_document)
    .checked_mul(2)
    .and_then(|count| count.checked_mul(size_of::<Vec<u8>>() as u64))
    .ok_or_else(|| invalid("authoritative_source_accounting", "source vector bytes overflow"))?;
  definition
    .max_canonical_source_bytes_per_document
    .checked_add(vector_bytes)
    .ok_or_else(|| invalid("authoritative_source_accounting", "source retained bytes overflow"))
}

fn parser_transient_bytes(plan: &ParserResolutionPlanV1<'_>) -> Result<u64, AuthoritativeSourceEvaluationErrorV1> {
  if plan.kind == ParserPlanKind::None {
    return Ok(0);
  }
  let mut maximum = None;
  for candidate in &plan.candidates {
    let structure_bytes = candidate
      .policy
      .max_structure_nodes
      .checked_mul(CANONICAL_CONFIG_VALUE_MAX_RETAINED_BYTES_PER_NODE_V1)
      .ok_or_else(|| invalid("authoritative_parser_accounting", "parser structure bytes overflow"))?;
    let bytes = candidate
      .policy
      .max_response_bytes
      .checked_add(structure_bytes)
      .ok_or_else(|| invalid("authoritative_parser_accounting", "parser retained bytes overflow"))?;
    maximum = Some(maximum.map_or(bytes, |current: u64| current.max(bytes)));
  }
  maximum.ok_or_else(|| invalid("authoritative_parser_plan", "non-none parser plan has no candidates"))
}

fn retained_value_bytes(values: &[Vec<u8>], outer_capacity: usize) -> Result<u64, AuthoritativeSourceEvaluationErrorV1> {
  let outer = outer_capacity
    .checked_mul(size_of::<Vec<u8>>())
    .ok_or_else(|| invalid("authoritative_source_accounting", "source value-vector capacity overflow"))?;
  let outer = u64::try_from(outer).map_err(|error| invalid("authoritative_source_accounting", error))?;
  values.iter().try_fold(outer, |total, value| {
    let capacity = u64::try_from(value.capacity()).map_err(|error| invalid("authoritative_source_accounting", error))?;
    total.checked_add(capacity).ok_or_else(|| invalid("authoritative_source_accounting", "source value capacity overflow"))
  })
}

fn shrink_reservation(reservation: &mut MemoryReservation, retained_bytes: u64) -> Result<(), AuthoritativeSourceEvaluationErrorV1> {
  if retained_bytes > reservation.bytes() {
    reservation
      .grow(retained_bytes - reservation.bytes())
      .map_err(|error| AuthoritativeSourceEvaluationErrorV1::ResourcePressure(error.to_string()))?;
  } else {
    reservation.shrink(reservation.bytes() - retained_bytes).map_err(|error| invalid("authoritative_source_accounting", error))?;
  }
  Ok(())
}

fn invalid(code: &'static str, context: impl ToString) -> AuthoritativeSourceEvaluationErrorV1 {
  AuthoritativeSourceEvaluationErrorV1::InvalidConfiguration { code, context: context.to_string() }
}
