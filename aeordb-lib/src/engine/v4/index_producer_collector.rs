use std::collections::BTreeMap;
use std::mem::size_of;

use thiserror::Error;

use crate::engine::HashAlgorithm;
use crate::engine::file_record::FileRecord;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};
use crate::engine::path_utils::normalize_path;

use super::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, decode_canonical_value, encode_canonical_value};
use super::contract_generated::stable_reason_v1;
use super::dependency::DependencyTableV1;
use super::field_definition::decode_field_index_definition;
use super::hash::digest_parts;
use super::index_definition_runtime::{CompiledIndexDocumentV1, IndexDefinitionErrorClassV1, IndexDefinitionErrorV1, IndexDefinitionRuntimeV1};
use super::index_page::{OrderedIndexRoleV1, PostingRecordV1, encode_posting_record};
use super::index_producer_coordinator::{
  IndexProducerFallbackModeV1, IndexProducerMutationV1, IndexProducerOwnerDispositionV1, IndexProducerOwnerOutcomeV1, IndexProducerReportV1,
};
use super::index_record::{
  CanonicalValueRecordV1, DocumentStateOwnerV1, DocumentStateRecordV1, ScopeDocumentRecordV1, ScopeReverseRecordV1,
  encode_canonical_value_record, encode_document_state_record, encode_scope_document_record, encode_scope_reverse_record,
};
use super::index_source::{
  PluginMapperExecutorV1, SourceDocumentV1, SourceExtractionV1, SourceOperationalErrorClassV1, SourceOperationalErrorV1,
  ValueStoreRuntimeV1,
};
use super::parser_plan::{ParserPlanKind, ParserResolutionPlanV1};
use super::scope::{ScopeDefinitionV1, decode_scope_definition, scope_matches_path};
use super::source_selector::SourceSelectorKind;
use super::value_store::{ValueStoreDefinitionV1, decode_value_store_definition};

const STATE_STAGE_PARSER: u8 = 1;
const STATE_STAGE_SELECTOR: u8 = 2;
const STATE_STAGE_MAPPER: u8 = 3;
const STATE_STAGE_CANONICAL_VALUE: u8 = 4;
const STATE_STAGE_CONVERTER: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexProducerCollectorOptionsV1 {
  pub max_scopes: u32,
  pub max_value_stores: u32,
  pub max_field_indexes: u32,
  pub max_definition_bytes: u64,
  pub max_mutations: u32,
  pub max_report_bytes: u64,
  pub retry_after_ms: u64,
}

impl IndexProducerCollectorOptionsV1 {
  pub fn new(
    max_scopes: u32,
    max_value_stores: u32,
    max_field_indexes: u32,
    max_definition_bytes: u64,
    max_mutations: u32,
    max_report_bytes: u64,
    retry_after_ms: u64,
  ) -> Result<Self, IndexProducerCollectorErrorV1> {
    if max_scopes == 0
      || max_value_stores == 0
      || max_field_indexes == 0
      || max_definition_bytes == 0
      || max_mutations == 0
      || max_report_bytes == 0
      || retry_after_ms == 0
    {
      return Err(IndexProducerCollectorErrorV1::InvalidOptions("all collector limits and retry delay must be nonzero".to_string()));
    }
    Ok(Self { max_scopes, max_value_stores, max_field_indexes, max_definition_bytes, max_mutations, max_report_bytes, retry_after_ms })
  }
}

#[derive(Debug, Clone)]
pub struct IndexCollectorFieldDefinitionV1<'a> {
  pub expected_index_id: &'a [u8],
  pub encoded_definition: &'a [u8],
}

#[derive(Debug, Clone)]
pub struct IndexCollectorValueStoreDefinitionV1<'a> {
  pub expected_value_store_id: &'a [u8],
  pub encoded_definition: &'a [u8],
  pub field_indexes: Vec<IndexCollectorFieldDefinitionV1<'a>>,
}

#[derive(Debug, Clone)]
pub struct IndexCollectorScopeDefinitionV1<'a> {
  pub expected_scope_id: &'a [u8],
  pub encoded_definition: &'a [u8],
  pub value_stores: Vec<IndexCollectorValueStoreDefinitionV1<'a>>,
}

#[derive(Debug, Clone, Copy)]
pub struct IndexCollectorDocumentV1<'a> {
  pub namespace_root: &'a [u8],
  pub record_revision_hash: &'a [u8],
  pub file_record: &'a FileRecord,
}

#[derive(Debug, Clone, Copy)]
pub struct IndexCollectorDocumentTransitionV1<'a> {
  pub document_ordinal: u64,
  pub before: Option<IndexCollectorDocumentV1<'a>>,
  pub after: Option<IndexCollectorDocumentV1<'a>>,
}

#[derive(Debug, Clone, Copy)]
pub struct IndexCollectorDocumentRevisionTransitionV1<'a> {
  pub before: Option<IndexCollectorDocumentV1<'a>>,
  pub after: Option<IndexCollectorDocumentV1<'a>>,
}

#[derive(Debug, Clone)]
pub struct IndexCollectorScopeWorkV1<'a> {
  pub document_ordinal: u64,
  pub scope_bundle: IndexCollectorScopeDefinitionV1<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexParserDeterministicFailureV1 {
  reason: u16,
  evidence: Vec<u8>,
  observed_value_count: u64,
  observed_canonical_bytes: u64,
  observed_work_units: u64,
  dependency_ordinal: u32,
}

impl IndexParserDeterministicFailureV1 {
  pub fn malformed_document(evidence: Vec<u8>, observed_work_units: u64) -> Self {
    Self { reason: 0x0001, evidence, observed_value_count: 0, observed_canonical_bytes: 0, observed_work_units, dependency_ordinal: 0 }
  }

  pub fn deterministic_plugin_rejection(evidence: Vec<u8>, observed_work_units: u64, dependency_ordinal: u32) -> Self {
    Self { reason: 0x0002, evidence, observed_value_count: 0, observed_canonical_bytes: 0, observed_work_units, dependency_ordinal }
  }

  pub fn parser_output_contract(evidence: Vec<u8>, observed_canonical_bytes: u64, observed_work_units: u64) -> Self {
    Self { reason: 0x0003, evidence, observed_value_count: 0, observed_canonical_bytes, observed_work_units, dependency_ordinal: 0 }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexParserOutcomeV1 {
  Parsed(CanonicalConfigValueV1),
  DeterministicUnindexable(IndexParserDeterministicFailureV1),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexParserExecutionErrorClassV1 {
  Cancelled,
  DependencyUnavailable,
  HostFailure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexParserExecutionErrorV1 {
  class: IndexParserExecutionErrorClassV1,
  code: &'static str,
  context: String,
}

impl IndexParserExecutionErrorV1 {
  pub fn cancelled(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexParserExecutionErrorClassV1::Cancelled, code, context: context.into() }
  }

  pub fn dependency_unavailable(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexParserExecutionErrorClassV1::DependencyUnavailable, code, context: context.into() }
  }

  pub fn host_failure(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexParserExecutionErrorClassV1::HostFailure, code, context: context.into() }
  }

  pub const fn class(&self) -> IndexParserExecutionErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

pub struct IndexParserExecutionRequestV1<'a> {
  namespace_root: &'a [u8],
  record_revision_hash: &'a [u8],
  file_record: &'a FileRecord,
  parser_plan: &'a ParserResolutionPlanV1<'a>,
  dependencies: &'a DependencyTableV1<'a>,
}

impl IndexParserExecutionRequestV1<'_> {
  pub fn namespace_root(&self) -> &[u8] {
    self.namespace_root
  }

  pub fn record_revision_hash(&self) -> &[u8] {
    self.record_revision_hash
  }

  pub fn path(&self) -> &str {
    &self.file_record.path
  }

  pub fn file_record(&self) -> &FileRecord {
    self.file_record
  }

  pub fn parser_plan(&self) -> &ParserResolutionPlanV1<'_> {
    self.parser_plan
  }

  pub fn dependencies(&self) -> &DependencyTableV1<'_> {
    self.dependencies
  }
}

/// Resolve and parse the exact record revision named by the request.
///
/// Implementations must enforce the frozen invocation-policy limits before
/// allocating parser output and must account file-body, WASM, and executor
/// memory under their dedicated memory owners. The collector reserves the
/// bounded canonical result retained across this interface; file bodies never
/// cross the interface.
pub trait IndexParserExecutorV1: Send + Sync {
  fn parse(&self, request: IndexParserExecutionRequestV1<'_>) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1>;
}

pub struct CollectedIndexProducerReportV1 {
  report: IndexProducerReportV1,
  _reservation: MemoryReservation,
}

impl CollectedIndexProducerReportV1 {
  pub fn report(&self) -> &IndexProducerReportV1 {
    &self.report
  }

  pub fn into_parts(self) -> (IndexProducerReportV1, MemoryReservation) {
    (self.report, self._reservation)
  }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IndexProducerCollectorErrorV1 {
  #[error("invalid index producer collector options: {0}")]
  InvalidOptions(String),
  #[error("invalid index producer collection request: {0}")]
  InvalidRequest(String),
  #[error("index producer collection was cancelled")]
  Cancelled,
  #[error("index producer collection resource pressure: {0}")]
  ResourcePressure(String),
  #[error("index producer collection encoding failed: {0}")]
  Encoding(String),
  #[error("index producer collection accounting overflow: {0}")]
  AccountingOverflow(&'static str),
}

pub struct IndexProducerCollectorV1 {
  hash_algorithm: HashAlgorithm,
  memory: MemoryCoordinator,
  options: IndexProducerCollectorOptionsV1,
}

impl IndexProducerCollectorV1 {
  pub fn new(
    hash_algorithm: HashAlgorithm,
    memory: MemoryCoordinator,
    options: IndexProducerCollectorOptionsV1,
  ) -> Result<Self, IndexProducerCollectorErrorV1> {
    Ok(Self { hash_algorithm, memory, options })
  }

  pub fn collect(
    &self,
    scope_bundle: IndexCollectorScopeDefinitionV1<'_>,
    transition: IndexCollectorDocumentTransitionV1<'_>,
    parser: &dyn IndexParserExecutorV1,
    mapper: Option<&dyn PluginMapperExecutorV1>,
    is_cancelled: &dyn Fn() -> bool,
  ) -> Result<CollectedIndexProducerReportV1, IndexProducerCollectorErrorV1> {
    self.collect_scopes(
      std::iter::once(IndexCollectorScopeWorkV1 { document_ordinal: transition.document_ordinal, scope_bundle }),
      IndexCollectorDocumentRevisionTransitionV1 { before: transition.before, after: transition.after },
      parser,
      mapper,
      is_cancelled,
    )
  }

  pub fn collect_scopes<'a>(
    &self,
    scope_work: impl IntoIterator<Item = IndexCollectorScopeWorkV1<'a>>,
    transition: IndexCollectorDocumentRevisionTransitionV1<'a>,
    parser: &dyn IndexParserExecutorV1,
    mapper: Option<&dyn PluginMapperExecutorV1>,
    is_cancelled: &dyn Fn() -> bool,
  ) -> Result<CollectedIndexProducerReportV1, IndexProducerCollectorErrorV1> {
    if is_cancelled() {
      return Err(IndexProducerCollectorErrorV1::Cancelled);
    }
    let mut report = ReportBuilderV1::new(self.memory.clone(), self.options)?;
    let mut scope_count = 0u32;
    for scope in scope_work {
      if is_cancelled() {
        return Err(IndexProducerCollectorErrorV1::Cancelled);
      }
      scope_count = scope_count.checked_add(1).ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("scope definition count"))?;
      if scope_count > self.options.max_scopes {
        return Err(IndexProducerCollectorErrorV1::InvalidRequest("scope definition count exceeds the collector bound".to_string()));
      }
      self.collect_scope(
        &mut report,
        &scope.scope_bundle,
        IndexCollectorDocumentTransitionV1 { document_ordinal: scope.document_ordinal, before: transition.before, after: transition.after },
        parser,
        mapper,
        is_cancelled,
      )?;
    }
    report.finish()
  }

  fn collect_scope(
    &self,
    report: &mut ReportBuilderV1,
    scope_bundle: &IndexCollectorScopeDefinitionV1<'_>,
    transition: IndexCollectorDocumentTransitionV1<'_>,
    parser: &dyn IndexParserExecutorV1,
    mapper: Option<&dyn PluginMapperExecutorV1>,
    is_cancelled: &dyn Fn() -> bool,
  ) -> Result<(), IndexProducerCollectorErrorV1> {
    let definition_bytes = self.validate_request(scope_bundle, transition)?;
    let _definition_memory = self.reserve_transient(
      definition_bytes.checked_mul(2).ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("definition decode bytes"))?,
    )?;
    let scope = match decode_scope_definition(scope_bundle.encoded_definition, self.hash_algorithm) {
      Ok(scope) if scope.scope_id == scope_bundle.expected_scope_id => scope,
      Ok(_) => {
        degrade_scope_bundle(report, scope_bundle)?;
        return Ok(());
      }
      Err(error) => {
        tracing::warn!(code = error.code(), context = %error.context(), "Index scope configuration is malformed");
        degrade_scope_bundle(report, scope_bundle)?;
        return Ok(());
      }
    };

    let path_match_bytes = transition.before.into_iter().chain(transition.after).try_fold(0u64, |bytes, document| {
      let document_bytes = (document.file_record.path.len() as u64)
        .checked_mul(2)
        .ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("scope path match bytes"))?;
      bytes.checked_add(document_bytes).ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("scope path match bytes"))
    })?;
    let _path_match_memory = self.reserve_transient(path_match_bytes.max(1))?;
    let before_in_scope = match transition.before {
      Some(document) => scope_matches(&scope, &document.file_record.path)?,
      None => false,
    };
    let after_in_scope = match transition.after {
      Some(document) => scope_matches(&scope, &document.file_record.path)?,
      None => false,
    };
    if !before_in_scope && !after_in_scope {
      return Ok(());
    }

    let mut scope_outcome = report.outcome(scope_bundle.expected_scope_id, IndexProducerOwnerDispositionV1::Ready)?;
    self.collect_scope_mutations(report, &mut scope_outcome, transition, before_in_scope, after_in_scope)?;
    report.push_outcome(scope_outcome)?;

    for value in &scope_bundle.value_stores {
      if is_cancelled() {
        return Err(IndexProducerCollectorErrorV1::Cancelled);
      }
      self.collect_value_store(report, &scope, value, transition, before_in_scope, after_in_scope, parser, mapper, is_cancelled)?;
    }
    Ok(())
  }

  fn validate_request(
    &self,
    scope: &IndexCollectorScopeDefinitionV1<'_>,
    transition: IndexCollectorDocumentTransitionV1<'_>,
  ) -> Result<u64, IndexProducerCollectorErrorV1> {
    if transition.document_ordinal == 0 || (transition.before.is_none() && transition.after.is_none()) {
      return Err(IndexProducerCollectorErrorV1::InvalidRequest(
        "document transition requires a nonzero ordinal and at least one side".to_string(),
      ));
    }
    if scope.value_stores.len() > self.options.max_value_stores as usize {
      return Err(IndexProducerCollectorErrorV1::InvalidRequest("ValueStore count exceeds the collector bound".to_string()));
    }
    let field_count = scope.value_stores.iter().try_fold(0usize, |count, value| count.checked_add(value.field_indexes.len()));
    let Some(field_count) = field_count else {
      return Err(IndexProducerCollectorErrorV1::AccountingOverflow("field definition count"));
    };
    if field_count > self.options.max_field_indexes as usize {
      return Err(IndexProducerCollectorErrorV1::InvalidRequest("FieldIndex count exceeds the collector bound".to_string()));
    }
    let definition_bytes = scope
      .value_stores
      .iter()
      .try_fold(scope.encoded_definition.len(), |total, value| {
        total
          .checked_add(value.encoded_definition.len())
          .and_then(|total| value.field_indexes.iter().try_fold(total, |total, field| total.checked_add(field.encoded_definition.len())))
      })
      .ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("definition bytes"))?;
    if definition_bytes as u64 > self.options.max_definition_bytes {
      return Err(IndexProducerCollectorErrorV1::InvalidRequest("definition bytes exceed the collector bound".to_string()));
    }

    let hash_width = self.hash_algorithm.hash_length();
    let owner_count = 1usize
      .checked_add(scope.value_stores.len())
      .and_then(|count| count.checked_add(field_count))
      .ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("owner count"))?;
    let owner_bytes =
      owner_count.checked_mul(size_of::<&[u8]>()).ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("owner validation bytes"))?;
    let _owner_memory = self.reserve_transient(owner_bytes as u64)?;
    let mut owners = Vec::new();
    owners.try_reserve_exact(owner_count).map_err(|error| IndexProducerCollectorErrorV1::ResourcePressure(error.to_string()))?;
    validate_owner(scope.expected_scope_id, hash_width, "ScopeId")?;
    owners.push(scope.expected_scope_id);
    for value in &scope.value_stores {
      validate_owner(value.expected_value_store_id, hash_width, "ValueStoreId")?;
      owners.push(value.expected_value_store_id);
      for field in &value.field_indexes {
        validate_owner(field.expected_index_id, hash_width, "IndexId")?;
        owners.push(field.expected_index_id);
      }
    }
    owners.sort_unstable();
    if owners.windows(2).any(|pair| pair[0] == pair[1]) {
      return Err(IndexProducerCollectorErrorV1::InvalidRequest("duplicate index owner identity".to_string()));
    }
    for document in transition.before.into_iter().chain(transition.after) {
      validate_hash(document.namespace_root, hash_width, "namespace root")?;
      validate_hash(document.record_revision_hash, hash_width, "record revision")?;
      if document.file_record.path.is_empty()
        || !document.file_record.path.starts_with('/')
        || normalize_path(&document.file_record.path) != document.file_record.path
      {
        return Err(IndexProducerCollectorErrorV1::InvalidRequest("FileRecord path is not canonical absolute".to_string()));
      }
    }
    Ok(definition_bytes as u64)
  }

  fn collect_scope_mutations(
    &self,
    report: &mut ReportBuilderV1,
    outcome: &mut IndexProducerOwnerOutcomeV1,
    transition: IndexCollectorDocumentTransitionV1<'_>,
    before_in_scope: bool,
    after_in_scope: bool,
  ) -> Result<(), IndexProducerCollectorErrorV1> {
    let document = if after_in_scope {
      transition
        .after
        .ok_or_else(|| IndexProducerCollectorErrorV1::InvalidRequest("in-scope transition has no after document".to_string()))?
    } else {
      transition
        .before
        .ok_or_else(|| IndexProducerCollectorErrorV1::InvalidRequest("in-scope transition has no before document".to_string()))?
    };
    let file_key = digest_parts(self.hash_algorithm, &[b"file:", document.file_record.path.as_bytes()]);
    let ordinal_admission = report.admit_mutation(
      outcome,
      checked_encoded_length(16 + 2 * self.hash_algorithm.hash_length(), document.file_record.path.len(), "scope document record")?,
    )?;
    let ordinal = encode_scope_document_record(
      &ScopeDocumentRecordV1 {
        tombstone: !after_in_scope,
        document_ordinal: transition.document_ordinal,
        file_key: &file_key,
        record_revision_hash: document.record_revision_hash,
        path: &document.file_record.path,
      },
      self.hash_algorithm,
    )
    .map_err(encoding)?;
    report.push_admitted_mutation(outcome, OrderedIndexRoleV1::ScopeOrdinal, ordinal_admission, ordinal)?;
    if after_in_scope {
      let reverse_admission = report.admit_mutation(outcome, 12 + self.hash_algorithm.hash_length())?;
      let reverse = encode_scope_reverse_record(
        &ScopeReverseRecordV1 { document_ordinal: transition.document_ordinal, file_key: &file_key },
        self.hash_algorithm,
      )
      .map_err(encoding)?;
      report.push_admitted_mutation(outcome, OrderedIndexRoleV1::ScopeReverse, reverse_admission, reverse)?;
    } else if !before_in_scope {
      return Err(IndexProducerCollectorErrorV1::InvalidRequest("scope mutation has no applicable side".to_string()));
    }
    Ok(())
  }

  #[allow(clippy::too_many_arguments)]
  fn collect_value_store(
    &self,
    report: &mut ReportBuilderV1,
    scope: &ScopeDefinitionV1<'_>,
    value: &IndexCollectorValueStoreDefinitionV1<'_>,
    transition: IndexCollectorDocumentTransitionV1<'_>,
    before_in_scope: bool,
    after_in_scope: bool,
    parser: &dyn IndexParserExecutorV1,
    mapper: Option<&dyn PluginMapperExecutorV1>,
    is_cancelled: &dyn Fn() -> bool,
  ) -> Result<(), IndexProducerCollectorErrorV1> {
    let definition = match decode_value_store_definition(value.encoded_definition, self.hash_algorithm) {
      Ok(definition) if definition.value_store_id == value.expected_value_store_id && definition.scope_id == scope.scope_id => definition,
      Ok(_) => {
        report.push_degraded(value.expected_value_store_id, stable_reason_v1::INVALID_CONFIGURATION)?;
        for field in &value.field_indexes {
          report.push_degraded(field.expected_index_id, stable_reason_v1::INVALID_CONFIGURATION)?;
        }
        return Ok(());
      }
      Err(error) => {
        tracing::warn!(code = error.code(), context = %error.context(), "ValueStore configuration is malformed");
        report.push_degraded(value.expected_value_store_id, stable_reason_v1::INVALID_CONFIGURATION)?;
        for field in &value.field_indexes {
          report.push_degraded(field.expected_index_id, stable_reason_v1::INVALID_CONFIGURATION)?;
        }
        return Ok(());
      }
    };
    let runtime = match ValueStoreRuntimeV1::from_encoded(value.encoded_definition, self.hash_algorithm) {
      Ok(runtime) => runtime,
      Err(error) => {
        tracing::warn!(code = error.code(), context = %error.context(), "ValueStore runtime rejected decoded configuration");
        report.push_degraded(value.expected_value_store_id, stable_reason_v1::INVALID_CONFIGURATION)?;
        for field in &value.field_indexes {
          report.push_degraded(field.expected_index_id, stable_reason_v1::INVALID_CONFIGURATION)?;
        }
        return Ok(());
      }
    };

    let before = if before_in_scope {
      let document = transition
        .before
        .ok_or_else(|| IndexProducerCollectorErrorV1::InvalidRequest("in-scope transition has no before document".to_string()))?;
      self.evaluate_source(&runtime, &definition, document, parser, mapper, is_cancelled)?
    } else {
      SourceEvaluationV1::Missing
    };
    let after = if after_in_scope {
      let document = transition
        .after
        .ok_or_else(|| IndexProducerCollectorErrorV1::InvalidRequest("in-scope transition has no after document".to_string()))?;
      self.evaluate_source(&runtime, &definition, document, parser, mapper, is_cancelled)?
    } else {
      SourceEvaluationV1::Missing
    };

    let _value_disposition_memory = if matches!(after, SourceEvaluationV1::Frozen(_)) {
      Some(self.reserve_transient(self.hash_algorithm.hash_length() as u64)?)
    } else {
      None
    };
    let value_disposition = source_disposition(&before, &after, self.hash_algorithm, self.options.retry_after_ms);
    let mut value_outcome = report.outcome(value.expected_value_store_id, value_disposition)?;
    self.collect_value_mutations(report, &mut value_outcome, transition, &before, &after)?;
    report.push_outcome(value_outcome)?;

    for field in &value.field_indexes {
      self.collect_field_index(report, value, field, transition, &before, &after, is_cancelled)?;
    }
    Ok(())
  }

  fn evaluate_source(
    &self,
    runtime: &ValueStoreRuntimeV1<'_>,
    definition: &ValueStoreDefinitionV1<'_>,
    document: IndexCollectorDocumentV1<'_>,
    parser: &dyn IndexParserExecutorV1,
    mapper: Option<&dyn PluginMapperExecutorV1>,
    is_cancelled: &dyn Fn() -> bool,
  ) -> Result<SourceEvaluationV1, IndexProducerCollectorErrorV1> {
    if is_cancelled() {
      return Err(IndexProducerCollectorErrorV1::Cancelled);
    }
    let parser_memory = if definition.parser_plan.kind == ParserPlanKind::None {
      None
    } else {
      Some(self.reserve_transient(parser_transient_bytes(&definition.parser_plan)?)?)
    };
    let parsed = if definition.parser_plan.kind == ParserPlanKind::None {
      None
    } else {
      match parser.parse(IndexParserExecutionRequestV1 {
        namespace_root: document.namespace_root,
        record_revision_hash: document.record_revision_hash,
        file_record: document.file_record,
        parser_plan: &definition.parser_plan,
        dependencies: &definition.dependencies,
      }) {
        Ok(IndexParserOutcomeV1::Parsed(value)) => Some(value),
        Ok(IndexParserOutcomeV1::DeterministicUnindexable(failure)) => {
          let reservation = parser_memory.ok_or_else(|| {
            IndexProducerCollectorErrorV1::InvalidRequest("parser produced output for a parser-free ValueStore".to_string())
          })?;
          return self.parser_failure(failure).map(|state| SourceEvaluationV1::Frozen(state.with_reservation(reservation)));
        }
        Err(error) => return parser_operational(error),
      }
    };
    if is_cancelled() {
      return Err(IndexProducerCollectorErrorV1::Cancelled);
    }
    let transient_bytes = source_transient_bytes(definition)?;
    let transient = self.reserve_transient(transient_bytes)?;
    match runtime.extract(SourceDocumentV1 { file_record: document.file_record, parsed_value: parsed.as_ref() }, mapper, is_cancelled) {
      Ok(SourceExtractionV1::Missing) => Ok(SourceEvaluationV1::Missing),
      Ok(SourceExtractionV1::Values(values)) => Ok(SourceEvaluationV1::Values { values, _reservation: transient }),
      Ok(SourceExtractionV1::DeterministicUnindexable { code, .. }) => Ok(match source_state(definition.selector.kind, code)? {
        Some(state) => SourceEvaluationV1::Frozen(state.with_reservation(transient)),
        None => SourceEvaluationV1::Degraded,
      }),
      Err(error) => source_operational(error),
    }
  }

  fn parser_failure(&self, failure: IndexParserDeterministicFailureV1) -> Result<DocumentStateV1, IndexProducerCollectorErrorV1> {
    if !matches!(failure.reason, 0x0001..=0x0003) {
      return Err(IndexProducerCollectorErrorV1::InvalidRequest("parser returned an unknown deterministic reason".to_string()));
    }
    validate_evidence(&failure.evidence)?;
    Ok(DocumentStateV1 {
      stage: STATE_STAGE_PARSER,
      reason: failure.reason,
      evidence: failure.evidence,
      observed_value_count: failure.observed_value_count,
      observed_canonical_bytes: failure.observed_canonical_bytes,
      observed_work_units: failure.observed_work_units,
      dependency_ordinal: failure.dependency_ordinal,
      _reservation: None,
    })
  }

  fn collect_value_mutations(
    &self,
    report: &mut ReportBuilderV1,
    outcome: &mut IndexProducerOwnerOutcomeV1,
    transition: IndexCollectorDocumentTransitionV1<'_>,
    before: &SourceEvaluationV1,
    after: &SourceEvaluationV1,
  ) -> Result<(), IndexProducerCollectorErrorV1> {
    if matches!(before, SourceEvaluationV1::Retryable(_) | SourceEvaluationV1::Degraded)
      || matches!(after, SourceEvaluationV1::Retryable(_) | SourceEvaluationV1::Degraded)
    {
      return Ok(());
    }
    let before_document = transition.before;
    let after_document = transition.after;
    let before_values = source_values(before);
    let after_values = source_values(after);
    if let (Some(values), Some(document)) = (after_values, after_document) {
      for (ordinal, value) in values.iter().enumerate() {
        let ordinal = u32::try_from(ordinal)
          .map_err(|error| IndexProducerCollectorErrorV1::InvalidRequest(format!("source value ordinal exceeds u32: {error}")))?;
        let admission = report.admit_mutation(
          outcome,
          checked_encoded_length(24 + self.hash_algorithm.hash_length(), value.len(), "canonical value record")?,
        )?;
        let encoded = encode_canonical_value_record(
          &CanonicalValueRecordV1 {
            tombstone: false,
            document_ordinal: transition.document_ordinal,
            source_value_ordinal: ordinal,
            record_revision_hash: document.record_revision_hash,
            canonical_value: Some(value),
          },
          self.hash_algorithm,
        )
        .map_err(encoding)?;
        report.push_admitted_mutation(outcome, OrderedIndexRoleV1::Value, admission, encoded)?;
      }
    }
    if let (Some(values), Some(document)) = (before_values, before_document) {
      let retained = after_values.map_or(0, |values| values.len());
      for (ordinal, _) in values.iter().enumerate().skip(retained) {
        let ordinal = u32::try_from(ordinal)
          .map_err(|error| IndexProducerCollectorErrorV1::InvalidRequest(format!("source value ordinal exceeds u32: {error}")))?;
        let admission = report.admit_mutation(outcome, 24 + self.hash_algorithm.hash_length())?;
        let encoded = encode_canonical_value_record(
          &CanonicalValueRecordV1 {
            tombstone: true,
            document_ordinal: transition.document_ordinal,
            source_value_ordinal: ordinal,
            record_revision_hash: document.record_revision_hash,
            canonical_value: None,
          },
          self.hash_algorithm,
        )
        .map_err(encoding)?;
        report.push_admitted_mutation(outcome, OrderedIndexRoleV1::Value, admission, encoded)?;
      }
    }
    match (source_state_ref(before), source_state_ref(after)) {
      (_, Some(state)) => {
        let document =
          after_document.ok_or_else(|| IndexProducerCollectorErrorV1::InvalidRequest("state has no after document".to_string()))?;
        self.push_state(report, outcome, DocumentStateOwnerV1::ValueStore, false, transition.document_ordinal, document, state)?;
      }
      (Some(state), None) => {
        let document =
          before_document.ok_or_else(|| IndexProducerCollectorErrorV1::InvalidRequest("state has no before document".to_string()))?;
        self.push_state(report, outcome, DocumentStateOwnerV1::ValueStore, true, transition.document_ordinal, document, state)?;
      }
      (None, None) => {}
    }
    Ok(())
  }

  fn collect_field_index(
    &self,
    report: &mut ReportBuilderV1,
    value: &IndexCollectorValueStoreDefinitionV1<'_>,
    field: &IndexCollectorFieldDefinitionV1<'_>,
    transition: IndexCollectorDocumentTransitionV1<'_>,
    before_source: &SourceEvaluationV1,
    after_source: &SourceEvaluationV1,
    is_cancelled: &dyn Fn() -> bool,
  ) -> Result<(), IndexProducerCollectorErrorV1> {
    if let SourceEvaluationV1::Retryable(reason) = before_source {
      report.push_retryable(field.expected_index_id, *reason, self.options.retry_after_ms)?;
      return Ok(());
    }
    if let SourceEvaluationV1::Retryable(reason) = after_source {
      report.push_retryable(field.expected_index_id, *reason, self.options.retry_after_ms)?;
      return Ok(());
    }
    if matches!(before_source, SourceEvaluationV1::Degraded) || matches!(after_source, SourceEvaluationV1::Degraded) {
      report.push_degraded(field.expected_index_id, stable_reason_v1::INVALID_CONFIGURATION)?;
      return Ok(());
    }
    let field_definition = match decode_field_index_definition(field.encoded_definition, self.hash_algorithm) {
      Ok(definition) if definition.index_id == field.expected_index_id && definition.value_store_id == value.expected_value_store_id => {
        definition
      }
      Ok(_) => {
        report.push_degraded(field.expected_index_id, stable_reason_v1::INVALID_CONFIGURATION)?;
        return Ok(());
      }
      Err(error) => {
        tracing::warn!(code = error.code(), context = %error.context(), "FieldIndex configuration is malformed");
        report.push_degraded(field.expected_index_id, stable_reason_v1::INVALID_CONFIGURATION)?;
        return Ok(());
      }
    };
    let runtime = match IndexDefinitionRuntimeV1::from_encoded(value.encoded_definition, field.encoded_definition, self.hash_algorithm) {
      Ok(runtime) => runtime,
      Err(error) => {
        tracing::warn!(code = error.code(), context = %error.context(), "FieldIndex runtime rejected decoded configuration");
        report.push_degraded(field.expected_index_id, stable_reason_v1::INVALID_CONFIGURATION)?;
        return Ok(());
      }
    };
    if is_cancelled() {
      return Err(IndexProducerCollectorErrorV1::Cancelled);
    }
    let before = self.compile_field_source(&runtime, &field_definition, source_values(before_source))?;
    let after = self.compile_field_source(&runtime, &field_definition, source_values(after_source))?;
    let _field_disposition_memory = if matches!(after, FieldEvaluationV1::Frozen(_)) {
      Some(self.reserve_transient(self.hash_algorithm.hash_length() as u64)?)
    } else {
      None
    };
    let disposition = field_disposition(&before, &after, self.hash_algorithm);
    let mut outcome = report.outcome(field.expected_index_id, disposition)?;
    self.collect_field_mutations(report, &mut outcome, transition, &before, &after)?;
    report.push_outcome(outcome)
  }

  fn compile_field_source(
    &self,
    runtime: &IndexDefinitionRuntimeV1<'_, '_>,
    definition: &super::field_definition::FieldIndexDefinitionV1<'_>,
    values: Option<&[Vec<u8>]>,
  ) -> Result<FieldEvaluationV1, IndexProducerCollectorErrorV1> {
    let Some(values) = values else {
      return Ok(FieldEvaluationV1::Missing);
    };
    let transient_bytes = field_transient_bytes(definition, values)?;
    let transient = self.reserve_transient(transient_bytes)?;
    match runtime.compile_source_values(values) {
      Ok(compiled) => Ok(FieldEvaluationV1::Values { compiled, _reservation: transient }),
      Err(error) => Ok(match field_state(error)? {
        Some(state) => FieldEvaluationV1::Frozen(state.with_reservation(transient)),
        None => FieldEvaluationV1::Degraded,
      }),
    }
  }

  fn collect_field_mutations(
    &self,
    report: &mut ReportBuilderV1,
    outcome: &mut IndexProducerOwnerOutcomeV1,
    transition: IndexCollectorDocumentTransitionV1<'_>,
    before: &FieldEvaluationV1,
    after: &FieldEvaluationV1,
  ) -> Result<(), IndexProducerCollectorErrorV1> {
    if matches!(before, FieldEvaluationV1::Degraded) || matches!(after, FieldEvaluationV1::Degraded) {
      return Ok(());
    }
    let (current_keys, _current_keys_reservation) = if let FieldEvaluationV1::Values { compiled, .. } = after {
      let bytes = posting_identity_ref_bytes(compiled)?;
      let reservation = self.reserve_transient(bytes)?;
      (compiled_posting_keys(compiled)?, Some(reservation))
    } else {
      (Vec::new(), None)
    };
    if let FieldEvaluationV1::Values { compiled, .. } = after {
      for value in &compiled.values {
        for posting in &value.postings {
          let admission = report.admit_mutation(outcome, checked_encoded_length(32, posting.posting_key.len(), "posting record")?)?;
          let encoded = encode_posting_record(&PostingRecordV1 {
            tombstone: false,
            coordinate: posting.coordinate,
            document_ordinal: transition.document_ordinal,
            source_value_ordinal: value.source_value_ordinal,
            expansion_ordinal: posting.expansion_ordinal,
            posting_key: &posting.posting_key,
          })
          .map_err(encoding)?;
          report.push_admitted_mutation(outcome, OrderedIndexRoleV1::Posting, admission, encoded)?;
        }
      }
    }
    if let FieldEvaluationV1::Values { compiled, .. } = before {
      for value in &compiled.values {
        for posting in &value.postings {
          let key = PostingIdentityRefV1 {
            coordinate: posting.coordinate,
            posting_key: &posting.posting_key,
            source_value_ordinal: value.source_value_ordinal,
            expansion_ordinal: posting.expansion_ordinal,
          };
          let key_index = current_keys.partition_point(|candidate| candidate < &key);
          if current_keys.get(key_index) == Some(&key) {
            continue;
          }
          let admission =
            report.admit_mutation(outcome, checked_encoded_length(32, posting.posting_key.len(), "posting tombstone record")?)?;
          let encoded = encode_posting_record(&PostingRecordV1 {
            tombstone: true,
            coordinate: posting.coordinate,
            document_ordinal: transition.document_ordinal,
            source_value_ordinal: value.source_value_ordinal,
            expansion_ordinal: posting.expansion_ordinal,
            posting_key: &posting.posting_key,
          })
          .map_err(encoding)?;
          report.push_admitted_mutation(outcome, OrderedIndexRoleV1::Posting, admission, encoded)?;
        }
      }
    }
    match (field_state_ref(before), field_state_ref(after)) {
      (_, Some(state)) => {
        let document =
          transition.after.ok_or_else(|| IndexProducerCollectorErrorV1::InvalidRequest("field state has no after document".to_string()))?;
        self.push_state(report, outcome, DocumentStateOwnerV1::FieldIndex, false, transition.document_ordinal, document, state)?;
      }
      (Some(state), None) => {
        let document = transition
          .before
          .ok_or_else(|| IndexProducerCollectorErrorV1::InvalidRequest("field state has no before document".to_string()))?;
        self.push_state(report, outcome, DocumentStateOwnerV1::FieldIndex, true, transition.document_ordinal, document, state)?;
      }
      (None, None) => {}
    }
    Ok(())
  }

  #[allow(clippy::too_many_arguments)]
  fn push_state(
    &self,
    report: &mut ReportBuilderV1,
    outcome: &mut IndexProducerOwnerOutcomeV1,
    owner: DocumentStateOwnerV1,
    tombstone: bool,
    document_ordinal: u64,
    document: IndexCollectorDocumentV1<'_>,
    state: &DocumentStateV1,
  ) -> Result<(), IndexProducerCollectorErrorV1> {
    let role = match owner {
      DocumentStateOwnerV1::ValueStore => OrderedIndexRoleV1::ValueDocumentState,
      DocumentStateOwnerV1::FieldIndex => OrderedIndexRoleV1::IndexDocumentState,
    };
    let admission = report.admit_mutation(
      outcome,
      checked_encoded_length(48 + self.hash_algorithm.hash_length(), state.evidence.len(), "document state record")?,
    )?;
    let encoded = encode_document_state_record(
      &DocumentStateRecordV1 {
        tombstone,
        stage: state.stage,
        reason: state.reason,
        document_ordinal,
        record_revision_hash: document.record_revision_hash,
        observed_value_count: state.observed_value_count,
        observed_canonical_bytes: state.observed_canonical_bytes,
        observed_work_units: state.observed_work_units,
        dependency_ordinal: state.dependency_ordinal,
        evidence: &state.evidence,
      },
      owner,
      self.hash_algorithm,
    )
    .map_err(encoding)?;
    report.push_admitted_mutation(outcome, role, admission, encoded)
  }

  fn reserve_transient(&self, bytes: u64) -> Result<MemoryReservation, IndexProducerCollectorErrorV1> {
    self
      .memory
      .reserve(MemoryOwner::Task, bytes, AdmissionClass::Workload)
      .map_err(|error| IndexProducerCollectorErrorV1::ResourcePressure(error.to_string()))
  }
}

enum SourceEvaluationV1 {
  Missing,
  Values { values: Vec<Vec<u8>>, _reservation: MemoryReservation },
  Frozen(DocumentStateV1),
  Retryable(u16),
  Degraded,
}

enum FieldEvaluationV1 {
  Missing,
  Values { compiled: CompiledIndexDocumentV1, _reservation: MemoryReservation },
  Frozen(DocumentStateV1),
  Degraded,
}

struct DocumentStateV1 {
  stage: u8,
  reason: u16,
  evidence: Vec<u8>,
  observed_value_count: u64,
  observed_canonical_bytes: u64,
  observed_work_units: u64,
  dependency_ordinal: u32,
  _reservation: Option<MemoryReservation>,
}

impl DocumentStateV1 {
  fn with_reservation(mut self, reservation: MemoryReservation) -> Self {
    self._reservation = Some(reservation);
    self
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PostingIdentityRefV1<'a> {
  coordinate: u64,
  posting_key: &'a [u8],
  source_value_ordinal: u32,
  expansion_ordinal: u32,
}

struct ReportBuilderV1 {
  options: IndexProducerCollectorOptionsV1,
  report: IndexProducerReportV1,
  mutation_count: u32,
  retained_bytes: u64,
  reservation: MemoryReservation,
}

struct MutationAdmissionV1 {
  encoded_length: usize,
}

impl ReportBuilderV1 {
  fn new(memory: MemoryCoordinator, options: IndexProducerCollectorOptionsV1) -> Result<Self, IndexProducerCollectorErrorV1> {
    let retained_bytes = size_of::<IndexProducerReportV1>() as u64;
    let reservation = memory
      .reserve(MemoryOwner::Task, retained_bytes, AdmissionClass::Workload)
      .map_err(|error| IndexProducerCollectorErrorV1::ResourcePressure(error.to_string()))?;
    Ok(Self { options, report: IndexProducerReportV1 { outcomes: Vec::new() }, mutation_count: 0, retained_bytes, reservation })
  }

  fn outcome(
    &mut self,
    owner_id: &[u8],
    disposition: IndexProducerOwnerDispositionV1,
  ) -> Result<IndexProducerOwnerOutcomeV1, IndexProducerCollectorErrorV1> {
    let evidence_bytes = match &disposition {
      IndexProducerOwnerDispositionV1::FrozenUnindexable { evidence_hash, .. }
      | IndexProducerOwnerDispositionV1::Retryable { evidence_hash, .. }
      | IndexProducerOwnerDispositionV1::Degraded { evidence_hash, .. } => evidence_hash.as_ref().map_or(0, Vec::len),
      IndexProducerOwnerDispositionV1::Ready => 0,
    };
    self.grow((size_of::<IndexProducerOwnerOutcomeV1>() + owner_id.len() + evidence_bytes) as u64)?;
    Ok(IndexProducerOwnerOutcomeV1 { owner_id: owner_id.to_vec(), disposition, mutations: Vec::new() })
  }

  fn push_outcome(&mut self, outcome: IndexProducerOwnerOutcomeV1) -> Result<(), IndexProducerCollectorErrorV1> {
    self.report.outcomes.try_reserve(1).map_err(|error| IndexProducerCollectorErrorV1::ResourcePressure(error.to_string()))?;
    self.report.outcomes.push(outcome);
    Ok(())
  }

  fn push_degraded(&mut self, owner_id: &[u8], reason: u16) -> Result<(), IndexProducerCollectorErrorV1> {
    let outcome = self.outcome(
      owner_id,
      IndexProducerOwnerDispositionV1::Degraded {
        stable_reason: reason,
        fallback_mode: IndexProducerFallbackModeV1::AuthoritativeScan,
        evidence_hash: None,
      },
    )?;
    self.push_outcome(outcome)
  }

  fn push_retryable(&mut self, owner_id: &[u8], reason: u16, retry_after_ms: u64) -> Result<(), IndexProducerCollectorErrorV1> {
    let outcome = self.outcome(
      owner_id,
      IndexProducerOwnerDispositionV1::Retryable {
        stable_reason: reason,
        retry_after_ms,
        fallback_mode: IndexProducerFallbackModeV1::AuthoritativeScan,
        evidence_hash: None,
      },
    )?;
    self.push_outcome(outcome)
  }

  fn admit_mutation(
    &mut self,
    outcome: &IndexProducerOwnerOutcomeV1,
    encoded_length: usize,
  ) -> Result<MutationAdmissionV1, IndexProducerCollectorErrorV1> {
    let next_count =
      self.mutation_count.checked_add(1).ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("report mutation count"))?;
    if next_count > self.options.max_mutations {
      return Err(IndexProducerCollectorErrorV1::ResourcePressure("collector mutation count exceeds its bound".to_string()));
    }
    let bytes = size_of::<IndexProducerMutationV1>()
      .checked_add(outcome.owner_id.len())
      .and_then(|bytes| bytes.checked_add(encoded_length))
      .ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("report mutation bytes"))?;
    self.grow(bytes as u64)?;
    self.mutation_count = next_count;
    Ok(MutationAdmissionV1 { encoded_length })
  }

  fn push_admitted_mutation(
    &mut self,
    outcome: &mut IndexProducerOwnerOutcomeV1,
    role: OrderedIndexRoleV1,
    admission: MutationAdmissionV1,
    encoded_record: Vec<u8>,
  ) -> Result<(), IndexProducerCollectorErrorV1> {
    if encoded_record.len() != admission.encoded_length {
      return Err(IndexProducerCollectorErrorV1::Encoding(format!(
        "encoded mutation length {} differs from admitted length {}",
        encoded_record.len(),
        admission.encoded_length
      )));
    }
    outcome.mutations.try_reserve(1).map_err(|error| IndexProducerCollectorErrorV1::ResourcePressure(error.to_string()))?;
    outcome.mutations.push(IndexProducerMutationV1 { owner_id: outcome.owner_id.clone(), role, encoded_record });
    Ok(())
  }

  fn grow(&mut self, bytes: u64) -> Result<(), IndexProducerCollectorErrorV1> {
    let next = self.retained_bytes.checked_add(bytes).ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("report retained bytes"))?;
    if next > self.options.max_report_bytes {
      return Err(IndexProducerCollectorErrorV1::ResourcePressure("collector report bytes exceed its bound".to_string()));
    }
    self.reservation.grow(bytes).map_err(|error| IndexProducerCollectorErrorV1::ResourcePressure(error.to_string()))?;
    self.retained_bytes = next;
    Ok(())
  }

  fn finish(mut self) -> Result<CollectedIndexProducerReportV1, IndexProducerCollectorErrorV1> {
    self.report.outcomes.sort_unstable_by(|left, right| left.owner_id.cmp(&right.owner_id));
    if self.report.outcomes.windows(2).any(|pair| pair[0].owner_id == pair[1].owner_id) {
      return Err(IndexProducerCollectorErrorV1::InvalidRequest("collector produced duplicate owner outcomes".to_string()));
    }
    Ok(CollectedIndexProducerReportV1 { report: self.report, _reservation: self.reservation })
  }
}

fn degrade_scope_bundle(
  report: &mut ReportBuilderV1,
  scope: &IndexCollectorScopeDefinitionV1<'_>,
) -> Result<(), IndexProducerCollectorErrorV1> {
  report.push_degraded(scope.expected_scope_id, stable_reason_v1::INVALID_CONFIGURATION)?;
  for value in &scope.value_stores {
    report.push_degraded(value.expected_value_store_id, stable_reason_v1::INVALID_CONFIGURATION)?;
    for field in &value.field_indexes {
      report.push_degraded(field.expected_index_id, stable_reason_v1::INVALID_CONFIGURATION)?;
    }
  }
  Ok(())
}

fn validate_owner(owner: &[u8], hash_width: usize, label: &str) -> Result<(), IndexProducerCollectorErrorV1> {
  validate_hash(owner, hash_width, label)?;
  Ok(())
}

fn validate_hash(value: &[u8], hash_width: usize, label: &str) -> Result<(), IndexProducerCollectorErrorV1> {
  if value.len() != hash_width || value.iter().all(|byte| *byte == 0) {
    return Err(IndexProducerCollectorErrorV1::InvalidRequest(format!("{label} must be a nonzero complete database hash")));
  }
  Ok(())
}

fn checked_encoded_length(base: usize, variable: usize, label: &'static str) -> Result<usize, IndexProducerCollectorErrorV1> {
  base.checked_add(variable).ok_or(IndexProducerCollectorErrorV1::AccountingOverflow(label))
}

fn scope_matches(scope: &ScopeDefinitionV1<'_>, path: &str) -> Result<bool, IndexProducerCollectorErrorV1> {
  scope_matches_path(scope, path).map_err(|error| {
    IndexProducerCollectorErrorV1::InvalidRequest(format!("scope membership failed ({}): {}", error.code(), error.context()))
  })
}

fn source_disposition(
  before: &SourceEvaluationV1,
  after: &SourceEvaluationV1,
  hash_algorithm: HashAlgorithm,
  retry_after_ms: u64,
) -> IndexProducerOwnerDispositionV1 {
  let retry = match (before, after) {
    (SourceEvaluationV1::Retryable(reason), _) | (_, SourceEvaluationV1::Retryable(reason)) => Some(*reason),
    _ => None,
  };
  if let Some(reason) = retry {
    return IndexProducerOwnerDispositionV1::Retryable {
      stable_reason: reason,
      retry_after_ms,
      fallback_mode: IndexProducerFallbackModeV1::AuthoritativeScan,
      evidence_hash: None,
    };
  }
  if matches!(before, SourceEvaluationV1::Degraded) || matches!(after, SourceEvaluationV1::Degraded) {
    return IndexProducerOwnerDispositionV1::Degraded {
      stable_reason: stable_reason_v1::INVALID_CONFIGURATION,
      fallback_mode: IndexProducerFallbackModeV1::AuthoritativeScan,
      evidence_hash: None,
    };
  }
  if let SourceEvaluationV1::Frozen(state) = after {
    return frozen_disposition(state, hash_algorithm);
  }
  IndexProducerOwnerDispositionV1::Ready
}

fn field_disposition(
  before: &FieldEvaluationV1,
  after: &FieldEvaluationV1,
  hash_algorithm: HashAlgorithm,
) -> IndexProducerOwnerDispositionV1 {
  if matches!(before, FieldEvaluationV1::Degraded) || matches!(after, FieldEvaluationV1::Degraded) {
    return IndexProducerOwnerDispositionV1::Degraded {
      stable_reason: stable_reason_v1::INVALID_CONFIGURATION,
      fallback_mode: IndexProducerFallbackModeV1::AuthoritativeScan,
      evidence_hash: None,
    };
  }
  if let FieldEvaluationV1::Frozen(state) = after {
    return frozen_disposition(state, hash_algorithm);
  }
  IndexProducerOwnerDispositionV1::Ready
}

fn frozen_disposition(state: &DocumentStateV1, hash_algorithm: HashAlgorithm) -> IndexProducerOwnerDispositionV1 {
  let evidence_hash = digest_parts(hash_algorithm, &[b"aeordb.index.document-state-evidence.v1\0", &state.evidence]);
  IndexProducerOwnerDispositionV1::FrozenUnindexable { stage: state.stage, reason: state.reason, evidence_hash: Some(evidence_hash) }
}

fn source_values(value: &SourceEvaluationV1) -> Option<&[Vec<u8>]> {
  match value {
    SourceEvaluationV1::Values { values, .. } => Some(values),
    _ => None,
  }
}

fn source_state_ref(value: &SourceEvaluationV1) -> Option<&DocumentStateV1> {
  match value {
    SourceEvaluationV1::Frozen(state) => Some(state),
    _ => None,
  }
}

fn field_state_ref(value: &FieldEvaluationV1) -> Option<&DocumentStateV1> {
  match value {
    FieldEvaluationV1::Frozen(state) => Some(state),
    _ => None,
  }
}

fn parser_operational(error: IndexParserExecutionErrorV1) -> Result<SourceEvaluationV1, IndexProducerCollectorErrorV1> {
  match error.class() {
    IndexParserExecutionErrorClassV1::Cancelled => Err(IndexProducerCollectorErrorV1::Cancelled),
    IndexParserExecutionErrorClassV1::DependencyUnavailable => Ok(SourceEvaluationV1::Retryable(stable_reason_v1::DEPENDENCY_UNAVAILABLE)),
    IndexParserExecutionErrorClassV1::HostFailure => Ok(SourceEvaluationV1::Retryable(stable_reason_v1::RETRYABLE_IO)),
  }
}

fn source_operational(error: SourceOperationalErrorV1) -> Result<SourceEvaluationV1, IndexProducerCollectorErrorV1> {
  match error.class() {
    SourceOperationalErrorClassV1::Cancelled => Err(IndexProducerCollectorErrorV1::Cancelled),
    SourceOperationalErrorClassV1::DependencyUnavailable => Ok(SourceEvaluationV1::Retryable(stable_reason_v1::DEPENDENCY_UNAVAILABLE)),
    SourceOperationalErrorClassV1::HostFailure => Ok(SourceEvaluationV1::Retryable(stable_reason_v1::RETRYABLE_IO)),
  }
}

fn source_state(kind: SourceSelectorKind, code: &'static str) -> Result<Option<DocumentStateV1>, IndexProducerCollectorErrorV1> {
  let (stage, reason) = match code {
    "selector_work_limit" | "selector_work_overflow" => (STATE_STAGE_SELECTOR, 0x0005),
    "selector_examined_bytes_limit" | "selector_examined_bytes_overflow" => (STATE_STAGE_SELECTOR, 0x0006),
    "source_value_count_limit" => (selector_stage(kind), 0x0007),
    "source_value_bytes_limit" | "source_document_input_limit" | "source_value_bytes_overflow" => (selector_stage(kind), 0x0008),
    "plugin_mapper_empty_values" | "plugin_mapper_invalid_value" => (STATE_STAGE_MAPPER, 0x0004),
    "source_value_encode" => (STATE_STAGE_CANONICAL_VALUE, 0x0009),
    _ => return Ok(None),
  };
  let evidence = stable_code_evidence(code)?;
  Ok(Some(DocumentStateV1 {
    stage,
    reason,
    evidence,
    observed_value_count: 0,
    observed_canonical_bytes: 0,
    observed_work_units: 0,
    dependency_ordinal: 0,
    _reservation: None,
  }))
}

fn selector_stage(kind: SourceSelectorKind) -> u8 {
  match kind {
    SourceSelectorKind::JsonPath => STATE_STAGE_SELECTOR,
    SourceSelectorKind::PluginMapper => STATE_STAGE_MAPPER,
    SourceSelectorKind::Metadata | SourceSelectorKind::AlwaysMissingV0 => STATE_STAGE_CANONICAL_VALUE,
  }
}

fn field_state(error: IndexDefinitionErrorV1) -> Result<Option<DocumentStateV1>, IndexProducerCollectorErrorV1> {
  let reason = match error.class() {
    IndexDefinitionErrorClassV1::IdentityMismatch
    | IndexDefinitionErrorClassV1::SemanticMismatch
    | IndexDefinitionErrorClassV1::UnsupportedDefinition => return Ok(None),
    IndexDefinitionErrorClassV1::InvalidSourceValue => {
      if error.code().contains("nonfinite") {
        0x000b
      } else if error.code().contains("numeric") || error.code().contains("timestamp") || error.code().contains("temporal") {
        0x000a
      } else {
        0x0009
      }
    }
    IndexDefinitionErrorClassV1::ResourceLimit => match error.code() {
      "converter_input_limit" => 0x000c,
      "index_source_value_count_limit" | "index_posting_count_limit" | "converter_output_count_limit" => 0x000e,
      _ => 0x000f,
    },
  };
  let evidence = stable_code_evidence(error.code())?;
  Ok(Some(DocumentStateV1 {
    stage: STATE_STAGE_CONVERTER,
    reason,
    evidence,
    observed_value_count: 0,
    observed_canonical_bytes: 0,
    observed_work_units: 0,
    dependency_ordinal: 0,
    _reservation: None,
  }))
}

fn stable_code_evidence(code: &'static str) -> Result<Vec<u8>, IndexProducerCollectorErrorV1> {
  encode_canonical_value(
    &CanonicalConfigValueV1::Map(BTreeMap::from([("code".to_string(), CanonicalConfigValueV1::String(code.to_string()))])),
    CanonicalValueBounds::CONFIG,
  )
  .map_err(encoding)
}

fn validate_evidence(evidence: &[u8]) -> Result<(), IndexProducerCollectorErrorV1> {
  decode_canonical_value(evidence, CanonicalValueBounds::CONFIG)
    .map(|_| ())
    .map_err(|error| IndexProducerCollectorErrorV1::InvalidRequest(format!("parser evidence is not canonical bounded config: {error}")))
}

fn source_transient_bytes(definition: &ValueStoreDefinitionV1<'_>) -> Result<u64, IndexProducerCollectorErrorV1> {
  let vector_bytes = u64::from(definition.max_source_values_per_document)
    .checked_mul(size_of::<Vec<u8>>() as u64)
    .ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("source transient vector bytes"))?;
  definition
    .max_canonical_source_bytes_per_document
    .checked_add(vector_bytes)
    .ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("source transient bytes"))
}

fn field_transient_bytes(
  definition: &super::field_definition::FieldIndexDefinitionV1<'_>,
  values: &[Vec<u8>],
) -> Result<u64, IndexProducerCollectorErrorV1> {
  let value_count = values.len() as u64;
  let value_structures = value_count
    .checked_mul(size_of::<super::index_definition_runtime::CompiledDocumentValueV1>() as u64)
    .ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("field value structures"))?;
  let posting_count = value_count
    .checked_mul(u64::from(definition.converter.max_output_values))
    .map(|count| count.min(u64::from(definition.max_postings_per_document)))
    .ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("field posting count"))?;
  let posting_structures = posting_count
    .checked_mul(size_of::<super::index_converter::CompiledPostingKeyV1>() as u64)
    .ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("field posting structures"))?;
  let posting_bytes = posting_count
    .checked_mul(u64::from(definition.converter.max_output_value_bytes))
    .map(|bytes| bytes.min(definition.max_canonical_posting_bytes_per_document))
    .ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("field posting bytes"))?;
  let source_bytes = values.iter().try_fold(0u64, |bytes, value| {
    bytes.checked_add(value.len() as u64).ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("field source bytes"))
  })?;
  posting_structures
    .checked_add(posting_bytes)
    .and_then(|bytes| bytes.checked_add(value_structures))
    .and_then(|bytes| bytes.checked_add(source_bytes))
    .ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("field transient bytes"))
}

fn parser_transient_bytes(plan: &ParserResolutionPlanV1<'_>) -> Result<u64, IndexProducerCollectorErrorV1> {
  let mut maximum = None;
  for candidate in &plan.candidates {
    let structure_bytes = candidate
      .policy
      .max_structure_nodes
      .checked_mul(size_of::<CanonicalConfigValueV1>() as u64)
      .ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("parser structure bytes"))?;
    let bytes = candidate
      .policy
      .max_response_bytes
      .checked_add(structure_bytes)
      .ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("parser transient bytes"))?;
    maximum = Some(maximum.map_or(bytes, |current: u64| current.max(bytes)));
  }
  maximum.ok_or_else(|| IndexProducerCollectorErrorV1::InvalidRequest("non-none parser plan has no candidates".to_string()))
}

fn posting_identity_ref_bytes(compiled: &CompiledIndexDocumentV1) -> Result<u64, IndexProducerCollectorErrorV1> {
  let count = compiled.values.iter().try_fold(0u64, |count, value| {
    count.checked_add(value.postings.len() as u64).ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("posting identity count"))
  })?;
  count
    .checked_mul(size_of::<PostingIdentityRefV1<'_>>() as u64)
    .ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("posting identity bytes"))
}

fn compiled_posting_keys(compiled: &CompiledIndexDocumentV1) -> Result<Vec<PostingIdentityRefV1<'_>>, IndexProducerCollectorErrorV1> {
  let count = compiled.values.iter().try_fold(0usize, |count, value| {
    count.checked_add(value.postings.len()).ok_or(IndexProducerCollectorErrorV1::AccountingOverflow("posting identity count"))
  })?;
  let mut keys = Vec::new();
  keys.try_reserve_exact(count).map_err(|error| IndexProducerCollectorErrorV1::ResourcePressure(error.to_string()))?;
  for value in &compiled.values {
    for posting in &value.postings {
      keys.push(PostingIdentityRefV1 {
        coordinate: posting.coordinate,
        posting_key: &posting.posting_key,
        source_value_ordinal: value.source_value_ordinal,
        expansion_ordinal: posting.expansion_ordinal,
      });
    }
  }
  keys.sort_unstable();
  keys.dedup();
  Ok(keys)
}

fn encoding(error: impl std::fmt::Display) -> IndexProducerCollectorErrorV1 {
  IndexProducerCollectorErrorV1::Encoding(error.to_string())
}
