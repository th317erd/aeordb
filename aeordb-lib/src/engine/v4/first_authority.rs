//! Atomic first-authority publication for a disconnected v4 database.

use std::collections::HashSet;
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs::File;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::engine::durability_coordinator::DurabilityCoordinator;
use crate::engine::errors::EngineError;
use crate::engine::file_record::FileRecord;
use crate::engine::kv_store::{KV_TYPE_CHUNK, KV_TYPE_DIRECTORY, KV_TYPE_FILE_RECORD, KVEntry};
use crate::engine::native_durability::{
  NativeDurabilityError, NativeDurabilityOperation, read_file_at_native, verify_file_bytes_native, write_file_at_native,
};
use crate::engine::{CompressionAlgorithm, DiskKVStore, HashAlgorithm};

use super::control_store::SYSTEM_CONTROL_CONTENT_TYPE;
use super::contract_generated::kv_tag;
use super::database_header::DatabaseHeaderV4;
use super::entity::{EntryTypeV4, WHOLE_ENTITY_V1_FLAG_SYSTEM, WholeEntityWriteV1, decode_whole_entity, encode_whole_entity};
use super::header_publication::{
  DatabaseHeaderObservationV4, DatabaseHeaderPublicationErrorV4, DatabaseHeaderPublisherV4, HeaderPublicationDependencyV4,
  observe_database_header_v4,
};
use super::hash::digest_parts;
use super::gc_retirement::{
  PreparedRetirementJournalSegmentV1, RetirementJournalDurabilityReceiptV1, RetirementJournalDurableSinkV1, RetirementJournalSinkErrorV1,
};
use super::gc_state::{decode_retirement_journal_segment_v1, retirement_journal_records_v1};
use super::namespace::{
  EncodedNamespaceRootV1, EncodedSemanticObjectV1, NamespaceRootWriteV1, SemanticObjectKind, decode_namespace_tree_root_v0,
  decode_semantic_object, encode_namespace_root,
};
use super::reader::FormatError;
use super::root_authority::{
  RootAdmissionCommitV1, RootAuthorityKindV1, RootPublicationPrepareV1, decode_root_admission_commit, encode_root_admission_commit_control,
  encode_root_publication_prepare_control,
};
use super::semantic_store::{SEMANTIC_OBJECT_CONTENT_TYPE, semantic_object_path};
use super::system_control::{SystemControlKindV1, SystemControlSlotV1, system_control_path};

const FIRST_AUTHORITY_ENTITY_COUNT: usize = 8;
const FIRST_AUTHORITY_NAMESPACE_TREE_CAP: usize = 48 * 1024 * 1024;
const FIRST_AUTHORITY_CONTROL_BODY_CAP: usize = 16 * 1024;
const FIRST_AUTHORITY_CONTROL_ENTITY_CAP: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedNamespaceTreeV0 {
  pub root_hash: Vec<u8>,
  pub stored_value: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirstAuthorityPublicationRequestV1 {
  pub database_id: [u8; 16],
  pub transaction_id: [u8; 16],
  pub created_at_ms: u64,
  pub namespace_tree: PreparedNamespaceTreeV0,
  pub semantic_state: EncodedSemanticObjectV1,
  pub required_capabilities: [u8; 32],
  pub typed_closure_digest: Vec<u8>,
  pub authority_identity: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirstAuthorityPublicationReceiptV1 {
  pub namespace_root: EncodedNamespaceRootV1,
  pub prepare_control: Vec<u8>,
  pub admission_control: Vec<u8>,
  pub publication_sequence: u64,
  pub observation: DatabaseHeaderObservationV4,
  pub idempotent: bool,
}

#[derive(Debug)]
pub enum FirstAuthorityPublicationErrorV1 {
  Invalid { code: &'static str, message: String },
  Committed { code: &'static str, message: String, receipt: Box<FirstAuthorityPublicationReceiptV1> },
  Format(FormatError),
  Engine(EngineError),
  Header(DatabaseHeaderPublicationErrorV4),
  StateLockPoisoned,
}

impl FirstAuthorityPublicationErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Invalid { code, .. } => code,
      Self::Committed { code, .. } => code,
      Self::Format(error) => error.code(),
      Self::Engine(_) => "engine_failure",
      Self::Header(error) => error.code(),
      Self::StateLockPoisoned => "first_authority_lock_poisoned",
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }

  fn committed(code: &'static str, message: impl Into<String>, receipt: FirstAuthorityPublicationReceiptV1) -> Self {
    Self::Committed { code, message: message.into(), receipt: Box::new(receipt) }
  }

  pub fn committed_receipt(&self) -> Option<&FirstAuthorityPublicationReceiptV1> {
    match self {
      Self::Committed { receipt, .. } => Some(receipt),
      Self::Invalid { .. } | Self::Format(_) | Self::Engine(_) | Self::Header(_) | Self::StateLockPoisoned => None,
    }
  }
}

impl Display for FirstAuthorityPublicationErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Invalid { code, message } => write!(formatter, "{code}: {message}"),
      Self::Committed { code, message, receipt } => write!(
        formatter,
        "{code}: authority publication {} committed, but post-commit handling failed: {message}",
        receipt.publication_sequence
      ),
      Self::Format(error) => write!(formatter, "first-authority format error: {error}"),
      Self::Engine(error) => write!(formatter, "first-authority storage error: {error}"),
      Self::Header(error) => write!(formatter, "first-authority header error: {error}"),
      Self::StateLockPoisoned => formatter.write_str("first-authority state lock is poisoned"),
    }
  }
}

impl Error for FirstAuthorityPublicationErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Format(error) => Some(error),
      Self::Engine(error) => Some(error),
      Self::Header(error) => Some(error),
      Self::Invalid { .. } | Self::Committed { .. } | Self::StateLockPoisoned => None,
    }
  }
}

impl From<FormatError> for FirstAuthorityPublicationErrorV1 {
  fn from(error: FormatError) -> Self {
    Self::Format(error)
  }
}

impl From<EngineError> for FirstAuthorityPublicationErrorV1 {
  fn from(error: EngineError) -> Self {
    Self::Engine(error)
  }
}

impl From<DatabaseHeaderPublicationErrorV4> for FirstAuthorityPublicationErrorV1 {
  fn from(error: DatabaseHeaderPublicationErrorV4) -> Self {
    Self::Header(error)
  }
}

#[derive(Clone)]
struct PreparedWholeEntityV1 {
  key: Vec<u8>,
  kv_type: u8,
  bytes: Vec<u8>,
}

struct FirstAuthorityPackageV1 {
  namespace_root: EncodedNamespaceRootV1,
  prepare_control: Vec<u8>,
  admission_control: Vec<u8>,
  entities: Vec<PreparedWholeEntityV1>,
  hot_tail_offset: u64,
  write_sequence_high_water: u64,
}

trait FirstAuthorityDependencyObserverV1 {
  fn before_entity(&mut self, _index: usize, _entity: &PreparedWholeEntityV1) -> Result<(), NativeDurabilityError> {
    Ok(())
  }

  fn entity_written(&mut self, _index: usize, _entity: &PreparedWholeEntityV1) -> Result<(), NativeDurabilityError> {
    Ok(())
  }

  fn entity_staged(&mut self, _index: usize, _entity: &PreparedWholeEntityV1) -> Result<(), NativeDurabilityError> {
    Ok(())
  }

  fn staged(&mut self, kv: &DiskKVStore, entities: &[PreparedWholeEntityV1]) -> Result<(), NativeDurabilityError>;

  fn authority_committed(
    &mut self,
    _kv: &DiskKVStore,
    _entities: &[PreparedWholeEntityV1],
  ) -> Result<(), FirstAuthorityPublicationErrorV1> {
    Ok(())
  }
}

struct NoopFirstAuthorityDependencyObserverV1;

impl FirstAuthorityDependencyObserverV1 for NoopFirstAuthorityDependencyObserverV1 {
  fn staged(&mut self, _kv: &DiskKVStore, _entities: &[PreparedWholeEntityV1]) -> Result<(), NativeDurabilityError> {
    Ok(())
  }
}

pub struct V4FirstAuthorityPublisher {
  file: File,
  kv: Mutex<DiskKVStore>,
  header_publisher: DatabaseHeaderPublisherV4,
  root_state: Mutex<()>,
}

impl V4FirstAuthorityPublisher {
  pub fn new(kv: DiskKVStore, coordinator: Arc<DurabilityCoordinator>) -> Result<Self, FirstAuthorityPublicationErrorV1> {
    if !kv.shares_durability_coordinator(&coordinator) {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_coordinator_mismatch",
        "the KV store and header publisher must share one durability coordinator",
      ));
    }
    let file = kv.clone_database_file()?;
    let observation = observe_database_header_v4(&file)?;
    validate_kv_header_alignment(&kv, &observation.selected.header)?;
    Ok(Self { file, kv: Mutex::new(kv), header_publisher: DatabaseHeaderPublisherV4::new(coordinator), root_state: Mutex::new(()) })
  }

  pub fn observe(&self) -> Result<DatabaseHeaderObservationV4, FirstAuthorityPublicationErrorV1> {
    observe_database_header_v4(&self.file).map_err(Into::into)
  }

  pub fn locator(&self, key: &[u8]) -> Result<Option<KVEntry>, FirstAuthorityPublicationErrorV1> {
    let kv = self.lock_kv()?;
    kv.get(key).map_err(Into::into)
  }

  pub fn admission_locator(&self, root_hash: &[u8]) -> Result<Option<KVEntry>, FirstAuthorityPublicationErrorV1> {
    let observation = self.observe()?;
    let path = system_control_path(SystemControlKindV1::RootAdmissionCommit, root_hash, SystemControlSlotV1::Immutable)?;
    let key = first_authority_file_path_hash(&path, observation.selected.header.hash_algorithm);
    self.locator(&key)
  }

  fn publish_retirement_journal_segment(
    &self,
    segment: &PreparedRetirementJournalSegmentV1<'_>,
    observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<RetirementJournalDurabilityReceiptV1, RetirementJournalSinkErrorV1> {
    let _authority = match self.root_state.lock() {
      Ok(authority) => authority,
      Err(poisoned) => {
        drop(poisoned);
        return Err(RetirementJournalSinkErrorV1::new(
          "retirement_journal_authority_lock",
          FirstAuthorityPublicationErrorV1::StateLockPoisoned,
        ));
      }
    };
    let observation = self.observe().map_err(retirement_sink_first_authority_error)?;
    let header = &observation.selected.header;
    if observation.selected.redundancy_degraded {
      return Err(retirement_sink_invalid(
        "retirement_journal_degraded_header",
        "retirement-journal publication requires two valid v4 header slots",
      ));
    }
    if header.head_hash.iter().all(|byte| *byte == 0) {
      return Err(retirement_sink_invalid(
        "retirement_journal_missing_authority",
        "retirement-journal publication requires selected first authority",
      ));
    }

    let decoded = decode_retirement_journal_segment_v1(segment.value, header.hash_algorithm).map_err(retirement_sink_format_error)?;
    if decoded.key != segment.artifact_key
      || decoded.segment_ordinal != segment.segment_ordinal
      || decoded.generation != segment.generation
      || decoded.first_replacement_sequence != segment.first_replacement_sequence
      || decoded.last_replacement_sequence != segment.last_replacement_sequence
      || decoded.record_count != segment.record_count
    {
      return Err(retirement_sink_invalid(
        "retirement_journal_prepared_mismatch",
        "prepared retirement-journal fields do not match the exact immutable artifact",
      ));
    }
    if decoded.database_id != header.database_id {
      return Err(retirement_sink_invalid(
        "retirement_journal_database_mismatch",
        "retirement-journal segment belongs to another logical database",
      ));
    }
    let mut segment_timestamp_ms = 0;
    for record in retirement_journal_records_v1(&decoded, header.hash_algorithm).map_err(retirement_sink_format_error)? {
      let record = record.map_err(retirement_sink_format_error)?;
      segment_timestamp_ms = segment_timestamp_ms.max(record.retired_at_ms);
    }
    let publication_timestamp_ms = header.updated_at_ms.max(segment_timestamp_ms);

    let mut kv = self.lock_kv().map_err(retirement_sink_first_authority_error)?;
    validate_kv_header_alignment(&kv, header).map_err(retirement_sink_first_authority_error)?;
    if let Some(locator) = kv.get(segment.artifact_key).map_err(retirement_sink_engine_error)? {
      if locator.type_flags != kv_tag::GC_ARTIFACT {
        return Err(retirement_sink_invalid(
          "retirement_journal_identity_collision",
          "retirement-journal artifact key resolves to another KV role",
        ));
      }
      let maximum_length =
        super::entity::checked_whole_entity_encoded_length(header.hash_algorithm, segment.artifact_key.len(), segment.value.len())
          .map_err(retirement_sink_format_error)?;
      let bytes = read_entity_bounded(&self.file, &kv, segment.artifact_key, maximum_length, header.write_sequence_high_water)
        .map_err(retirement_sink_first_authority_error)?
        .ok_or_else(|| retirement_sink_invalid("retirement_journal_readback_missing", "retirement-journal locator disappeared"))?;
      let entity =
        decode_whole_entity(&bytes, header.hash_algorithm, header.write_sequence_high_water).map_err(retirement_sink_format_error)?;
      if entity.entry_type != EntryTypeV4::GcArtifact
        || entity.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM
        || entity.compression_algorithm != CompressionAlgorithm::None
        || entity.key != segment.artifact_key
        || entity.stored_value != segment.value
        || entity.timestamp_ms < segment_timestamp_ms
      {
        return Err(retirement_sink_invalid(
          "retirement_journal_identity_collision",
          "existing retirement-journal entity differs from the exact immutable artifact representation",
        ));
      }
      return retirement_journal_receipt(segment, entity.write_sequence);
    }

    let write_sequence = header
      .write_sequence_high_water
      .checked_add(1)
      .ok_or_else(|| retirement_sink_invalid("retirement_journal_write_sequence_exhausted", "v4 write sequence is exhausted"))?;
    let entry_count = header
      .entry_count
      .checked_add(1)
      .ok_or_else(|| retirement_sink_invalid("retirement_journal_entry_count_overflow", "v4 entry count overflowed"))?;
    let entity_bytes = encode_entity(
      EntryTypeV4::GcArtifact,
      WHOLE_ENTITY_V1_FLAG_SYSTEM,
      header.hash_algorithm,
      publication_timestamp_ms,
      write_sequence,
      segment.artifact_key,
      segment.value,
    )
    .map_err(retirement_sink_first_authority_error)?;
    let entities = [PreparedWholeEntityV1 { key: segment.artifact_key.to_vec(), kv_type: kv_tag::GC_ARTIFACT, bytes: entity_bytes }];
    let dependency_bytes =
      entity_dependency_bytes(&entities, header.hash_algorithm.hash_length()).map_err(retirement_sink_first_authority_error)?;

    kv.flush().map_err(retirement_sink_engine_error)?;
    validate_kv_header_alignment(&kv, header).map_err(retirement_sink_first_authority_error)?;
    if kv.write_buffer_len() != 0 || kv.hot_buffer_len() != 0 {
      return Err(retirement_sink_invalid(
        "retirement_journal_baseline_not_flushed",
        "retirement-journal publication requires an empty KV write and hot-buffer baseline",
      ));
    }
    let append_start = self.file.metadata().map_err(|error| retirement_sink_engine_error(EngineError::IoError(error)))?.len();
    if append_start < header.hot_tail_offset {
      return Err(retirement_sink_invalid("retirement_journal_file_truncated", "database length precedes the selected v4 hot-tail offset"));
    }
    let expected_hot_tail_offset = append_start
      .checked_add(entities[0].bytes.len() as u64)
      .ok_or_else(|| retirement_sink_invalid("retirement_journal_wal_overflow", "retirement-journal WAL offset overflowed"))?;
    let mut candidate = header.clone();
    candidate.updated_at_ms = publication_timestamp_ms;
    candidate.write_sequence_high_water = write_sequence;
    candidate.hot_tail_offset = expected_hot_tail_offset;
    candidate.entry_count = entry_count;
    let admitted = self
      .header_publisher
      .admit_inactive_slot_with_dependency_bytes(&self.file, &observation, candidate, dependency_bytes)
      .map_err(retirement_sink_header_error)?;
    let authority_sequence = admitted.sequence();
    let batch = kv.begin_atomic_visibility_batch(1, authority_sequence).map_err(retirement_sink_engine_error)?;
    let (publication_result, append_completed) = {
      let mut dependency = FirstAuthorityDependencyV1 {
        file: &self.file,
        kv: &mut kv,
        batch,
        expected_publication_sequence: authority_sequence,
        entities: &entities,
        start_offset: append_start,
        expected_hot_tail_offset,
        append_completed: false,
        observer,
      };
      let publication_result = admitted.commit_with_dependency(&mut dependency);
      (publication_result, dependency.append_completed)
    };
    let publication = match publication_result {
      Ok(publication) => publication,
      Err(error) => {
        kv.abort_atomic_visibility_batch(batch).map_err(retirement_sink_engine_error)?;
        return Err(retirement_sink_header_error(error));
      }
    };
    if !append_completed {
      kv.abort_atomic_visibility_batch(batch).map_err(retirement_sink_engine_error)?;
      return Err(retirement_sink_invalid(
        "retirement_journal_dependency_missing",
        "header publication completed without the exact retirement-journal dependency append",
      ));
    }
    kv.complete_hot_tail_dependency();
    kv.publish_atomic_visibility_after_authority(batch, &publication.durability).map_err(retirement_sink_engine_error)?;
    observer
      .authority_committed(&kv, &entities)
      .map_err(|error| RetirementJournalSinkErrorV1::new("retirement_journal_committed_postcondition", error))?;

    let stored = read_entity_bounded(
      &self.file,
      &kv,
      segment.artifact_key,
      entities[0].bytes.len(),
      publication.observation.selected.header.write_sequence_high_water,
    )
    .map_err(retirement_sink_first_authority_error)?
    .ok_or_else(|| retirement_sink_invalid("retirement_journal_readback_missing", "published retirement-journal locator is absent"))?;
    if stored != entities[0].bytes {
      return Err(retirement_sink_invalid(
        "retirement_journal_readback_mismatch",
        "published retirement-journal entity differs from its exact prepared bytes",
      ));
    }
    retirement_journal_receipt(segment, write_sequence)
  }

  pub fn publish(
    &self,
    request: &FirstAuthorityPublicationRequestV1,
  ) -> Result<FirstAuthorityPublicationReceiptV1, FirstAuthorityPublicationErrorV1> {
    let mut observer = NoopFirstAuthorityDependencyObserverV1;
    self.publish_with_observer(request, &mut observer)
  }

  fn publish_with_observer(
    &self,
    request: &FirstAuthorityPublicationRequestV1,
    observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<FirstAuthorityPublicationReceiptV1, FirstAuthorityPublicationErrorV1> {
    let _root_state = match self.root_state.lock() {
      Ok(root_state) => root_state,
      Err(poisoned) => {
        drop(poisoned);
        return Err(FirstAuthorityPublicationErrorV1::StateLockPoisoned);
      }
    };
    let observation = observe_database_header_v4(&self.file)?;
    if observation.selected.redundancy_degraded {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_degraded_header",
        "first authority requires two valid v4 header slots",
      ));
    }
    let header = &observation.selected.header;
    if header.database_id != request.database_id {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_database_mismatch",
        "the request belongs to a different logical database",
      ));
    }
    let namespace_root = prepare_namespace_root(request, header.hash_algorithm, header.write_sequence_high_water)?;
    if header.head_hash.iter().any(|byte| *byte != 0) {
      return self.load_idempotent(request, namespace_root, observation);
    }

    let mut kv = self.lock_kv()?;
    validate_kv_header_alignment(&kv, header)?;
    let selected_header_slot_sequence = header
      .slot_sequence
      .checked_add(1)
      .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_header_sequence_exhausted", "header sequence exhausted"))?;
    let sizing_package = build_package(request, namespace_root.clone(), header, selected_header_slot_sequence, 1)?;
    refuse_existing_entities(&kv, &sizing_package.entities)?;
    let expected_hot_tail_offset = sizing_package.hot_tail_offset;
    let expected_write_sequence_high_water = sizing_package.write_sequence_high_water;
    let dependency_bytes = package_dependency_bytes(&sizing_package, header.hash_algorithm.hash_length())?;
    drop(sizing_package);
    let mut candidate = header.clone();
    candidate.updated_at_ms = candidate.updated_at_ms.max(request.created_at_ms);
    candidate.write_sequence_high_water = expected_write_sequence_high_water;
    candidate.hot_tail_offset = expected_hot_tail_offset;
    candidate.entry_count = candidate
      .entry_count
      .checked_add(FIRST_AUTHORITY_ENTITY_COUNT as u64)
      .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_entry_count_overflow", "header entry count overflow"))?;
    candidate.head_hash = namespace_root.root_hash.clone();

    let admitted =
      self.header_publisher.admit_inactive_slot_with_dependency_bytes(&self.file, &observation, candidate, dependency_bytes)?;
    let publication_sequence = admitted.sequence();
    let package = build_package(request, namespace_root, header, selected_header_slot_sequence, publication_sequence)?;
    if package.hot_tail_offset != expected_hot_tail_offset || package.write_sequence_high_water != expected_write_sequence_high_water {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_sizing_changed",
        "publication sequence changed the pre-admitted physical layout",
      ));
    }
    let batch = kv.begin_atomic_visibility_batch(FIRST_AUTHORITY_ENTITY_COUNT, publication_sequence)?;
    let (publication_result, append_completed) = {
      let mut dependency = FirstAuthorityDependencyV1 {
        file: &self.file,
        kv: &mut kv,
        batch,
        expected_publication_sequence: publication_sequence,
        entities: &package.entities,
        start_offset: header.hot_tail_offset,
        expected_hot_tail_offset: package.hot_tail_offset,
        append_completed: false,
        observer,
      };
      let publication_result = admitted.commit_with_dependency(&mut dependency);
      (publication_result, dependency.append_completed)
    };
    let publication = match publication_result {
      Ok(publication) => publication,
      Err(error) => {
        kv.abort_atomic_visibility_batch(batch)?;
        return Err(error.into());
      }
    };
    if !append_completed {
      kv.abort_atomic_visibility_batch(batch)?;
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_dependency_missing",
        "header publication completed without the exact root dependency append",
      ));
    }
    kv.complete_hot_tail_dependency();
    let receipt = FirstAuthorityPublicationReceiptV1 {
      namespace_root: package.namespace_root.clone(),
      prepare_control: package.prepare_control.clone(),
      admission_control: package.admission_control.clone(),
      publication_sequence,
      observation: publication.observation,
      idempotent: false,
    };
    match kv.publish_atomic_visibility_after_authority(batch, &publication.durability) {
      Ok(()) => {}
      Err(error) => {
        return Err(FirstAuthorityPublicationErrorV1::committed(
          "first_authority_committed_visibility_failure",
          error.to_string(),
          receipt,
        ));
      }
    }
    match observer.authority_committed(&kv, &package.entities) {
      Ok(()) => {}
      Err(error) => {
        return Err(FirstAuthorityPublicationErrorV1::committed(
          "first_authority_committed_postcondition_failure",
          error.to_string(),
          receipt,
        ));
      }
    }
    match verify_package_locators(&self.file, &kv, &package, &receipt.observation.selected.header) {
      Ok(()) => Ok(receipt),
      Err(error) => {
        Err(FirstAuthorityPublicationErrorV1::committed("first_authority_committed_readback_failure", error.to_string(), receipt))
      }
    }
  }

  fn load_idempotent(
    &self,
    request: &FirstAuthorityPublicationRequestV1,
    namespace_root: EncodedNamespaceRootV1,
    observation: DatabaseHeaderObservationV4,
  ) -> Result<FirstAuthorityPublicationReceiptV1, FirstAuthorityPublicationErrorV1> {
    if observation.selected.header.head_hash != namespace_root.root_hash {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_already_selected",
        "the database already selects a different first authority",
      ));
    }
    let kv = self.lock_kv()?;
    let admission_control =
      load_system_file(&self.file, &kv, &observation.selected.header, SystemControlKindV1::RootAdmissionCommit, &namespace_root.root_hash)?;
    let admission = decode_root_admission_commit(&admission_control, observation.selected.header.hash_algorithm)?;
    if admission.selected_header_slot_sequence != observation.selected.header.slot_sequence
      || admission.namespace_root != namespace_root.root_hash
      || admission.database_id != request.database_id
      || admission.transaction_id != request.transaction_id
      || admission.authority_kind != RootAuthorityKindV1::Head
    {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_witness_mismatch",
        "selected HEAD and first-admission witness do not describe the requested transaction",
      ));
    }
    let base_write_sequence =
      observation.selected.header.write_sequence_high_water.checked_sub(FIRST_AUTHORITY_ENTITY_COUNT as u64).ok_or_else(|| {
        FirstAuthorityPublicationErrorV1::invalid("first_authority_sequence_underflow", "selected write sequence is too small")
      })?;
    let mut source_header = observation.selected.header.clone();
    source_header.write_sequence_high_water = base_write_sequence;
    source_header.hot_tail_offset = package_start_offset(&self.file, &kv, &observation.selected.header, &request.namespace_tree.root_hash)?;
    let package =
      build_package(request, namespace_root, &source_header, observation.selected.header.slot_sequence, admission.publication_sequence)?;
    if package.admission_control != admission_control {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_retry_collision",
        "selected admission bytes differ from the exact retry request",
      ));
    }
    verify_package_locators(&self.file, &kv, &package, &observation.selected.header)?;
    Ok(FirstAuthorityPublicationReceiptV1 {
      namespace_root: package.namespace_root,
      prepare_control: package.prepare_control,
      admission_control: package.admission_control,
      publication_sequence: admission.publication_sequence,
      observation,
      idempotent: true,
    })
  }

  fn lock_kv(&self) -> Result<MutexGuard<'_, DiskKVStore>, FirstAuthorityPublicationErrorV1> {
    match self.kv.lock() {
      Ok(kv) => Ok(kv),
      Err(poisoned) => {
        drop(poisoned);
        Err(FirstAuthorityPublicationErrorV1::StateLockPoisoned)
      }
    }
  }
}

impl RetirementJournalDurableSinkV1 for V4FirstAuthorityPublisher {
  fn publish_synced(
    &mut self,
    segment: &PreparedRetirementJournalSegmentV1<'_>,
  ) -> Result<RetirementJournalDurabilityReceiptV1, RetirementJournalSinkErrorV1> {
    let mut observer = NoopFirstAuthorityDependencyObserverV1;
    self.publish_retirement_journal_segment(segment, &mut observer)
  }
}

fn prepare_namespace_root(
  request: &FirstAuthorityPublicationRequestV1,
  algorithm: HashAlgorithm,
  write_sequence_high_water: u64,
) -> Result<EncodedNamespaceRootV1, FirstAuthorityPublicationErrorV1> {
  if request.namespace_tree.stored_value.len() > FIRST_AUTHORITY_NAMESPACE_TREE_CAP {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_namespace_tree_exceeds_cap",
      format!(
        "namespace tree is {} bytes, exceeding the {FIRST_AUTHORITY_NAMESPACE_TREE_CAP}-byte first-authority cap",
        request.namespace_tree.stored_value.len()
      ),
    ));
  }
  let semantic = decode_semantic_object(&request.semantic_state.value, algorithm)?;
  let semantic_cap = super::semantic_store::semantic_object_cap(semantic.kind_id)?;
  if request.semantic_state.value.len() > semantic_cap {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_semantic_state_exceeds_cap",
      format!("semantic state is {} bytes, exceeding its {semantic_cap}-byte kind cap", request.semantic_state.value.len()),
    ));
  }
  if semantic.object_id != request.semantic_state.object_id || !matches!(semantic.kind, SemanticObjectKind::State { .. }) {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_semantic_state_mismatch",
      "semantic-state bytes do not match their state identity",
    ));
  }
  let tree_sequence = write_sequence_high_water
    .checked_add(1)
    .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_write_sequence_exhausted", "write sequence exhausted"))?;
  let tree_entity = encode_entity(
    EntryTypeV4::DirectoryIndex,
    0,
    algorithm,
    request.created_at_ms,
    tree_sequence,
    &request.namespace_tree.root_hash,
    &request.namespace_tree.stored_value,
  )?;
  decode_namespace_tree_root_v0(&tree_entity, &request.namespace_tree.root_hash, algorithm, tree_sequence)?;
  encode_namespace_root(
    &NamespaceRootWriteV1 {
      required_capabilities: request.required_capabilities,
      namespace_tree_root: request.namespace_tree.root_hash.clone(),
      semantic_state_root: request.semantic_state.object_id.clone(),
    },
    algorithm,
  )
  .map_err(Into::into)
}

fn refuse_existing_entities(kv: &DiskKVStore, entities: &[PreparedWholeEntityV1]) -> Result<(), FirstAuthorityPublicationErrorV1> {
  for entity in entities {
    if kv.get(&entity.key)?.is_some() {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_identity_collision",
        format!("first-authority identity {} already exists before root admission", hex::encode(&entity.key)),
      ));
    }
  }
  Ok(())
}

fn package_dependency_bytes(package: &FirstAuthorityPackageV1, hash_length: usize) -> Result<u64, FirstAuthorityPublicationErrorV1> {
  entity_dependency_bytes(&package.entities, hash_length)
}

fn entity_dependency_bytes(entities: &[PreparedWholeEntityV1], hash_length: usize) -> Result<u64, FirstAuthorityPublicationErrorV1> {
  let entity_bytes = entities.iter().try_fold(0u64, |total, entity| {
    let length = match u64::try_from(entity.bytes.len()) {
      Ok(length) => length,
      Err(error) => {
        return Err(FirstAuthorityPublicationErrorV1::invalid(
          "first_authority_package_size",
          format!("entity length exceeds u64: {error}"),
        ));
      }
    };
    total
      .checked_add(length)
      .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_package_size", "entity byte total overflowed"))
  })?;
  let hot_tail_bytes = crate::engine::hot_tail::serialized_size(entities.len(), 0, hash_length)?;
  let hot_tail_bytes = match u64::try_from(hot_tail_bytes) {
    Ok(hot_tail_bytes) => hot_tail_bytes,
    Err(error) => {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_package_size",
        format!("hot-tail length exceeds u64: {error}"),
      ));
    }
  };
  entity_bytes
    .checked_add(hot_tail_bytes)
    .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_package_size", "dependency byte total overflowed"))
}

fn retirement_journal_receipt(
  segment: &PreparedRetirementJournalSegmentV1<'_>,
  hard_publication_sequence: u64,
) -> Result<RetirementJournalDurabilityReceiptV1, RetirementJournalSinkErrorV1> {
  let stored_value_length =
    u32::try_from(segment.value.len()).map_err(|error| RetirementJournalSinkErrorV1::new("retirement_journal_value_length", error))?;
  if hard_publication_sequence == 0 {
    return Err(retirement_sink_invalid(
      "retirement_journal_publication_sequence",
      "retirement-journal entity has no durable v4 write sequence",
    ));
  }
  Ok(RetirementJournalDurabilityReceiptV1 { artifact_key: segment.artifact_key.to_vec(), stored_value_length, hard_publication_sequence })
}

fn retirement_sink_invalid(code: &'static str, message: &'static str) -> RetirementJournalSinkErrorV1 {
  RetirementJournalSinkErrorV1::new(code, FirstAuthorityPublicationErrorV1::invalid(code, message))
}

fn retirement_sink_format_error(error: FormatError) -> RetirementJournalSinkErrorV1 {
  RetirementJournalSinkErrorV1::new(error.code(), error)
}

fn retirement_sink_first_authority_error(error: FirstAuthorityPublicationErrorV1) -> RetirementJournalSinkErrorV1 {
  let code = error.code();
  RetirementJournalSinkErrorV1::new(code, error)
}

fn retirement_sink_header_error(error: DatabaseHeaderPublicationErrorV4) -> RetirementJournalSinkErrorV1 {
  let code = error.code();
  RetirementJournalSinkErrorV1::new(code, error)
}

fn retirement_sink_engine_error(error: EngineError) -> RetirementJournalSinkErrorV1 {
  RetirementJournalSinkErrorV1::new("retirement_journal_storage", error)
}

fn build_package(
  request: &FirstAuthorityPublicationRequestV1,
  namespace_root: EncodedNamespaceRootV1,
  source_header: &DatabaseHeaderV4,
  selected_header_slot_sequence: u64,
  publication_sequence: u64,
) -> Result<FirstAuthorityPackageV1, FirstAuthorityPublicationErrorV1> {
  let algorithm = source_header.hash_algorithm;
  let timestamp_i64 = match i64::try_from(request.created_at_ms) {
    Ok(timestamp) => timestamp,
    Err(error) => {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_timestamp_range",
        format!("timestamp exceeds signed v1 control range: {error}"),
      ));
    }
  };
  let authority_identity_digest = digest_parts(algorithm, &[&request.authority_identity]);
  let zero_hash = vec![0; algorithm.hash_length()];
  let prepare = RootPublicationPrepareV1 {
    database_id: request.database_id,
    transaction_id: request.transaction_id,
    created_at_ms: timestamp_i64,
    target_namespace_root: namespace_root.root_hash.clone(),
    target_semantic_state: request.semantic_state.object_id.clone(),
    typed_closure_digest: request.typed_closure_digest.clone(),
    authority_kind: RootAuthorityKindV1::Head,
    authority_identity: request.authority_identity.clone(),
    expected_authority_before: zero_hash,
    expected_authority_after: namespace_root.root_hash.clone(),
    intended_header_slot_sequence: selected_header_slot_sequence,
    intended_publication_sequence: publication_sequence,
  };
  let prepare_control = encode_root_publication_prepare_control(&prepare, algorithm)?;
  let admission = RootAdmissionCommitV1 {
    database_id: request.database_id,
    namespace_root: namespace_root.root_hash.clone(),
    transaction_id: request.transaction_id,
    publication_started_at_ms: timestamp_i64,
    authority_kind: RootAuthorityKindV1::Head,
    recovered_from_selected_authority: false,
    authority_identity_digest,
    authority_after: namespace_root.root_hash.clone(),
    selected_header_slot_sequence,
    publication_sequence,
    prepare_payload_hash: digest_parts(algorithm, &[&prepare_control]),
  };
  let admission_control = encode_root_admission_commit_control(&admission, algorithm)?;

  let mut next_sequence = source_header.write_sequence_high_water;
  let mut entities = Vec::with_capacity(FIRST_AUTHORITY_ENTITY_COUNT);
  next_sequence = append_entity(
    &mut entities,
    EntryTypeV4::DirectoryIndex,
    0,
    KV_TYPE_DIRECTORY,
    algorithm,
    request.created_at_ms,
    next_sequence,
    &request.namespace_tree.root_hash,
    &request.namespace_tree.stored_value,
  )?;
  next_sequence = append_system_file(
    &mut entities,
    semantic_object_path(algorithm, 1, &request.semantic_state.object_id)?,
    SEMANTIC_OBJECT_CONTENT_TYPE,
    &request.semantic_state.value,
    algorithm,
    request.created_at_ms,
    next_sequence,
  )?;
  next_sequence = append_entity(
    &mut entities,
    EntryTypeV4::DirectoryIndex,
    WHOLE_ENTITY_V1_FLAG_SYSTEM,
    KV_TYPE_DIRECTORY,
    algorithm,
    request.created_at_ms,
    next_sequence,
    &namespace_root.root_hash,
    &namespace_root.value,
  )?;
  let prepare_path =
    system_control_path(SystemControlKindV1::RootPublicationPrepare, &request.transaction_id, SystemControlSlotV1::Immutable)?;
  next_sequence = append_system_file(
    &mut entities,
    prepare_path,
    SYSTEM_CONTROL_CONTENT_TYPE,
    &prepare_control,
    algorithm,
    request.created_at_ms,
    next_sequence,
  )?;
  let admission_path =
    system_control_path(SystemControlKindV1::RootAdmissionCommit, &namespace_root.root_hash, SystemControlSlotV1::Immutable)?;
  next_sequence = append_system_file(
    &mut entities,
    admission_path,
    SYSTEM_CONTROL_CONTENT_TYPE,
    &admission_control,
    algorithm,
    request.created_at_ms,
    next_sequence,
  )?;
  if entities.len() != FIRST_AUTHORITY_ENTITY_COUNT {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_entity_count",
      format!("constructed {} entities, expected {FIRST_AUTHORITY_ENTITY_COUNT}", entities.len()),
    ));
  }
  let mut identities = HashSet::with_capacity(entities.len());
  if entities.iter().any(|entity| !identities.insert(entity.key.clone())) {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_duplicate_identity",
      "first-authority entities contain a duplicate KV identity",
    ));
  }
  let hot_tail_offset = entities.iter().try_fold(source_header.hot_tail_offset, |offset, entity| {
    offset
      .checked_add(entity.bytes.len() as u64)
      .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_wal_overflow", "WAL offset overflow"))
  })?;
  Ok(FirstAuthorityPackageV1 {
    namespace_root,
    prepare_control,
    admission_control,
    entities,
    hot_tail_offset,
    write_sequence_high_water: next_sequence,
  })
}

#[allow(clippy::too_many_arguments)]
fn append_entity(
  entities: &mut Vec<PreparedWholeEntityV1>,
  entry_type: EntryTypeV4,
  flags: u8,
  kv_type: u8,
  algorithm: HashAlgorithm,
  timestamp_ms: u64,
  previous_sequence: u64,
  key: &[u8],
  stored_value: &[u8],
) -> Result<u64, FirstAuthorityPublicationErrorV1> {
  let write_sequence = previous_sequence
    .checked_add(1)
    .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_write_sequence_exhausted", "write sequence exhausted"))?;
  let bytes = encode_entity(entry_type, flags, algorithm, timestamp_ms, write_sequence, key, stored_value)?;
  entities.push(PreparedWholeEntityV1 { key: key.to_vec(), kv_type, bytes });
  Ok(write_sequence)
}

#[allow(clippy::too_many_arguments)]
fn append_system_file(
  entities: &mut Vec<PreparedWholeEntityV1>,
  path: String,
  content_type: &str,
  body: &[u8],
  algorithm: HashAlgorithm,
  timestamp_ms: u64,
  previous_sequence: u64,
) -> Result<u64, FirstAuthorityPublicationErrorV1> {
  let chunk_key = first_authority_system_chunk_hash(body, algorithm);
  let sequence = append_entity(
    entities,
    EntryTypeV4::Chunk,
    WHOLE_ENTITY_V1_FLAG_SYSTEM,
    KV_TYPE_CHUNK,
    algorithm,
    timestamp_ms,
    previous_sequence,
    &chunk_key,
    body,
  )?;
  let timestamp_i64 = match i64::try_from(timestamp_ms) {
    Ok(timestamp) => timestamp,
    Err(error) => {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_timestamp_range",
        format!("timestamp exceeds FileRecord range: {error}"),
      ));
    }
  };
  let record = FileRecord {
    path: path.clone(),
    content_type: Some(content_type.to_string()),
    total_size: body.len() as u64,
    created_at: timestamp_i64,
    updated_at: timestamp_i64,
    metadata: Vec::new(),
    content_hash: first_authority_content_hash(body, algorithm),
    chunk_hashes: vec![chunk_key],
  };
  let value = record.serialize(algorithm.hash_length())?;
  let path_key = first_authority_file_path_hash(&path, algorithm);
  append_entity(
    entities,
    EntryTypeV4::FileRecord,
    WHOLE_ENTITY_V1_FLAG_SYSTEM,
    KV_TYPE_FILE_RECORD,
    algorithm,
    timestamp_ms,
    sequence,
    &path_key,
    &value,
  )
}

fn encode_entity(
  entry_type: EntryTypeV4,
  flags: u8,
  algorithm: HashAlgorithm,
  timestamp_ms: u64,
  write_sequence: u64,
  key: &[u8],
  stored_value: &[u8],
) -> Result<Vec<u8>, FirstAuthorityPublicationErrorV1> {
  encode_whole_entity(&WholeEntityWriteV1 {
    entry_type,
    flags,
    hash_algorithm: algorithm,
    compression_algorithm: CompressionAlgorithm::None,
    timestamp_ms,
    write_sequence,
    key,
    stored_value,
  })
  .map_err(Into::into)
}

fn first_authority_file_path_hash(path: &str, algorithm: HashAlgorithm) -> Vec<u8> {
  digest_parts(algorithm, &[b"file:", path.as_bytes()])
}

fn first_authority_system_chunk_hash(body: &[u8], algorithm: HashAlgorithm) -> Vec<u8> {
  digest_parts(algorithm, &[b"system::", body])
}

fn first_authority_content_hash(body: &[u8], algorithm: HashAlgorithm) -> Vec<u8> {
  digest_parts(algorithm, &[body])
}

struct FirstAuthorityDependencyV1<'a> {
  file: &'a File,
  kv: &'a mut DiskKVStore,
  batch: crate::engine::disk_kv_store::AtomicKvVisibilityBatch,
  expected_publication_sequence: u64,
  entities: &'a [PreparedWholeEntityV1],
  start_offset: u64,
  expected_hot_tail_offset: u64,
  append_completed: bool,
  observer: &'a mut dyn FirstAuthorityDependencyObserverV1,
}

impl HeaderPublicationDependencyV4 for FirstAuthorityDependencyV1<'_> {
  fn append_dependency(&mut self, publication_sequence: u64) -> Result<(), NativeDurabilityError> {
    if publication_sequence != self.expected_publication_sequence {
      return Err(NativeDurabilityError::invalid(
        NativeDurabilityOperation::WriteAt,
        "first-authority dependency received another publication sequence",
      ));
    }
    for entity in self.entities {
      if self.kv.get(&entity.key).map_err(native_engine_error)?.is_some() {
        return Err(NativeDurabilityError::invalid(
          NativeDurabilityOperation::WriteAt,
          format!("first-authority identity {} already exists", hex::encode(&entity.key)),
        ));
      }
    }

    let mut offset = self.start_offset;
    for (index, entity) in self.entities.iter().enumerate() {
      self.observer.before_entity(index, entity)?;
      write_file_at_native(self.file, offset, &entity.bytes)?;
      verify_file_bytes_native(self.file, offset, &entity.bytes)?;
      self.observer.entity_written(index, entity)?;
      let total_length = match u32::try_from(entity.bytes.len()) {
        Ok(total_length) => total_length,
        Err(error) => {
          return Err(NativeDurabilityError::invalid(
            NativeDurabilityOperation::WriteAt,
            format!("first-authority entity length exceeds u32: {error}"),
          ));
        }
      };
      self
        .kv
        .stage_atomic_visibility_entry(self.batch, KVEntry { type_flags: entity.kv_type, hash: entity.key.clone(), offset, total_length })
        .map_err(native_engine_error)?;
      self.observer.entity_staged(index, entity)?;
      offset = offset
        .checked_add(entity.bytes.len() as u64)
        .ok_or_else(|| NativeDurabilityError::invalid(NativeDurabilityOperation::WriteAt, "first-authority WAL offset overflow"))?;
    }
    if offset != self.expected_hot_tail_offset {
      return Err(NativeDurabilityError::invalid(
        NativeDurabilityOperation::WriteAt,
        "first-authority append ended at an unexpected hot-tail offset",
      ));
    }
    self.kv.set_hot_tail_offset(offset);
    let wrote_hot_tail = self
      .kv
      .prepare_hot_tail_dependency(true)
      .map_err(|error| NativeDurabilityError::from_io(NativeDurabilityOperation::WriteAt, error))?;
    if !wrote_hot_tail {
      return Err(NativeDurabilityError::invalid(
        NativeDurabilityOperation::WriteAt,
        "first-authority KV dependency did not write its hot tail",
      ));
    }
    self.observer.staged(self.kv, self.entities)?;
    self.append_completed = true;
    Ok(())
  }
}

fn native_engine_error(error: EngineError) -> NativeDurabilityError {
  NativeDurabilityError::invalid(NativeDurabilityOperation::WriteAt, error.to_string())
}

fn validate_kv_header_alignment(kv: &DiskKVStore, header: &DatabaseHeaderV4) -> Result<(), FirstAuthorityPublicationErrorV1> {
  if kv.hash_algo() != header.hash_algorithm
    || kv.kv_block_offset() != header.kv_block_offset
    || kv.kv_block_length() != header.kv_block_length
    || kv.stage() != header.kv_block_stage as usize
    || kv.hot_tail_offset() != header.hot_tail_offset
    || kv.len() as u64 != header.entry_count
  {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_kv_header_mismatch",
      "KV state does not match the selected v4 header",
    ));
  }
  Ok(())
}

fn verify_package_locators(
  file: &File,
  kv: &DiskKVStore,
  package: &FirstAuthorityPackageV1,
  header: &DatabaseHeaderV4,
) -> Result<(), FirstAuthorityPublicationErrorV1> {
  for entity in &package.entities {
    let stored = read_entity_bounded(file, kv, &entity.key, entity.bytes.len(), header.write_sequence_high_water)?
      .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_readback_missing", "published locator is absent"))?;
    if stored != entity.bytes {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_readback_mismatch",
        format!("published entity {} differs from its exact bytes", hex::encode(&entity.key)),
      ));
    }
  }
  Ok(())
}

fn read_entity_bounded(
  file: &File,
  kv: &DiskKVStore,
  key: &[u8],
  maximum_total_length: usize,
  write_sequence_high_water: u64,
) -> Result<Option<Vec<u8>>, FirstAuthorityPublicationErrorV1> {
  let Some(locator) = kv.get(key)? else {
    return Ok(None);
  };
  let length = match usize::try_from(locator.total_length) {
    Ok(length) => length,
    Err(error) => {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_locator_length",
        format!("locator length exceeds usize: {error}"),
      ));
    }
  };
  if length > maximum_total_length {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_locator_exceeds_cap",
      format!("locator length {length} exceeds its {maximum_total_length}-byte role cap"),
    ));
  }
  let mut bytes = vec![0; length];
  read_file_at_native(file, locator.offset, &mut bytes)
    .map_err(|error| FirstAuthorityPublicationErrorV1::invalid("first_authority_readback_io", error.to_string()))?;
  let entity = decode_whole_entity(&bytes, kv.hash_algo(), write_sequence_high_water)?;
  if entity.key != key {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_locator_identity",
      "KV locator resolves to another WholeEntity key",
    ));
  }
  Ok(Some(bytes))
}

fn load_system_file(
  file: &File,
  kv: &DiskKVStore,
  header: &DatabaseHeaderV4,
  kind: SystemControlKindV1,
  identity: &[u8],
) -> Result<Vec<u8>, FirstAuthorityPublicationErrorV1> {
  let path = system_control_path(kind, identity, SystemControlSlotV1::Immutable)?;
  let path_key = first_authority_file_path_hash(&path, header.hash_algorithm);
  let record_bytes = read_entity_bounded(file, kv, &path_key, FIRST_AUTHORITY_CONTROL_ENTITY_CAP, header.write_sequence_high_water)?
    .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_control_missing", format!("missing {path}")))?;
  let entity = decode_whole_entity(&record_bytes, header.hash_algorithm, header.write_sequence_high_water)?;
  if entity.entry_type != EntryTypeV4::FileRecord || entity.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_control_representation",
      format!("{path} is not a system FileRecord"),
    ));
  }
  let record = FileRecord::deserialize(entity.stored_value, header.hash_algorithm.hash_length(), 1)?;
  if record.path != path || record.content_type.as_deref() != Some(SYSTEM_CONTROL_CONTENT_TYPE) || !record.metadata.is_empty() {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_control_file_record",
      format!("{path} FileRecord metadata is not canonical"),
    ));
  }
  if record.chunk_hashes.len() != 1 || record.total_size > FIRST_AUTHORITY_CONTROL_BODY_CAP as u64 {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_control_file_record",
      format!("{path} must contain one bounded canonical control chunk"),
    ));
  }
  let mut body = Vec::with_capacity(record.total_size as usize);
  for chunk_key in &record.chunk_hashes {
    let chunk_bytes = read_entity_bounded(file, kv, chunk_key, FIRST_AUTHORITY_CONTROL_ENTITY_CAP, header.write_sequence_high_water)?
      .ok_or_else(|| {
        FirstAuthorityPublicationErrorV1::invalid("first_authority_control_chunk_missing", format!("missing chunk for {path}"))
      })?;
    let chunk = decode_whole_entity(&chunk_bytes, header.hash_algorithm, header.write_sequence_high_water)?;
    if chunk.entry_type != EntryTypeV4::Chunk || chunk.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_control_chunk_representation",
        format!("{path} references a non-system chunk"),
      ));
    }
    body.extend_from_slice(chunk.stored_value);
  }
  if body.len() as u64 != record.total_size || first_authority_content_hash(&body, header.hash_algorithm) != record.content_hash {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_control_content",
      format!("{path} content does not match its FileRecord"),
    ));
  }
  Ok(body)
}

fn package_start_offset(
  file: &File,
  kv: &DiskKVStore,
  header: &DatabaseHeaderV4,
  namespace_tree_root: &[u8],
) -> Result<u64, FirstAuthorityPublicationErrorV1> {
  let tree = kv.get(namespace_tree_root)?.ok_or_else(|| {
    FirstAuthorityPublicationErrorV1::invalid("first_authority_tree_missing", "selected namespace tree locator is absent")
  })?;
  let maximum_tree_entity_length = FIRST_AUTHORITY_NAMESPACE_TREE_CAP
    .checked_add(super::entity::WHOLE_ENTITY_V1_MAX_HEADER_LENGTH)
    .and_then(|length| length.checked_add(header.hash_algorithm.hash_length()))
    .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_tree_cap", "namespace-tree entity cap overflowed"))?;
  let tree_bytes = read_entity_bounded(file, kv, namespace_tree_root, maximum_tree_entity_length, header.write_sequence_high_water)?
    .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_tree_missing", "selected namespace tree entity is absent"))?;
  let tree_entity = decode_whole_entity(&tree_bytes, header.hash_algorithm, header.write_sequence_high_water)?;
  let package_high_water = tree_entity
    .write_sequence
    .checked_add(FIRST_AUTHORITY_ENTITY_COUNT as u64 - 1)
    .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_tree_sequence", "first package sequence overflows"))?;
  if package_high_water != header.write_sequence_high_water {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_tree_sequence",
      "selected namespace tree sequence does not begin the first-authority package",
    ));
  }
  Ok(tree.offset)
}

#[cfg(test)]
#[path = "../../../spec/engine/v4_first_authority_internal_spec.rs"]
mod v4_first_authority_internal_spec;
