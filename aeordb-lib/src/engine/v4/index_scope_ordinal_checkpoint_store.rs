//! Scope-local ordinal authority backed by one selected index checkpoint.
//!
//! This adapter deliberately composes the existing recovery store and
//! selector-last checkpoint publisher. It does not create another selector or
//! persistence format. One instance owns one scope/checkpoint owner; a later
//! runtime owner may multiplex instances for overlapping scopes.

use std::cmp::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio_util::sync::CancellationToken;

use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};
use crate::engine::{HashAlgorithm, VirtualClock};

use super::index_artifact::{IndexManifestBodyV1, decode_index_manifest};
use super::index_coordinator_recovery::{
  IndexCheckpointRootV1, IndexRecoveryErrorV1, IndexRecoveryOptionsV1, IndexRecoveryOutcomeV1, IndexRecoveryOwnerV1,
  IndexRecoveryPublicationRequestV1, IndexRecoveryStoreErrorV1, IndexRecoveryStoreV1, publish_index_recovery_checkpoint_v1,
  recover_index_checkpoint_v1,
};
use super::index_page::{ArtifactDirectoryNodeV1, OrderedIndexRoleV1, OrderedPageV1, decode_artifact_directory, decode_ordered_page};
use super::index_scope_ordinal_authority::{
  IndexScopeOrdinalDurableClaimV1, IndexScopeOrdinalPublishOutcomeV1, IndexScopeOrdinalPublishRequestV1,
  IndexScopeOrdinalSelectedObservationV1, IndexScopeOrdinalStateStoreErrorV1, IndexScopeOrdinalStateStoreV1,
  IndexScopeOrdinalStoreObservationRequestV1,
};
use super::index_scope_ordinal_checkpoint::{
  MAX_SCOPE_ORDINAL_PENDING_CLAIMS_V1, ScopeOrdinalPendingClaimWriteV1, decode_scope_ordinal_claim_resume_v1,
  encode_scope_ordinal_claim_resume_v1,
};
use super::index_task::{
  ExternalWorkspaceDescriptorWriteV1, IndexTaskAttachmentRoleV1, IndexTaskAttachmentWriteV1, IndexTaskCheckpointV1,
  IndexTaskCheckpointWriteV1, decode_index_task_checkpoint, encode_index_task_checkpoint,
};

const MAXIMUM_INDEX_ARTIFACT_BYTES: u64 = 4 * 1_024 * 1_024;

pub struct RecoveryIndexScopeOrdinalStateStoreV1<Store> {
  hash_algorithm: HashAlgorithm,
  owner: IndexRecoveryOwnerV1,
  recovery_options: IndexRecoveryOptionsV1,
  memory: Arc<MemoryCoordinator>,
  cancellation: CancellationToken,
  clock: Arc<dyn VirtualClock>,
  store: Mutex<Store>,
}

impl<Store> RecoveryIndexScopeOrdinalStateStoreV1<Store>
where
  Store: IndexRecoveryStoreV1 + Send,
{
  pub fn new(
    hash_algorithm: HashAlgorithm,
    owner: IndexRecoveryOwnerV1,
    recovery_options: IndexRecoveryOptionsV1,
    memory: Arc<MemoryCoordinator>,
    cancellation: CancellationToken,
    clock: Arc<dyn VirtualClock>,
    store: Store,
  ) -> Result<Self, IndexScopeOrdinalStateStoreErrorV1> {
    if owner.index_id().len() != hash_algorithm.hash_length() || owner.index_id().iter().all(|byte| *byte == 0) {
      return Err(corrupt("scope_ordinal_store_owner", "checkpoint owner does not identify one scope in the database hash profile"));
    }
    Ok(Self { hash_algorithm, owner, recovery_options, memory, cancellation, clock, store: Mutex::new(store) })
  }

  fn lock_store(&self) -> Result<MutexGuard<'_, Store>, IndexScopeOrdinalStateStoreErrorV1> {
    self.store.lock().map_err(|error| corrupt("scope_ordinal_store_poisoned", format!("checkpoint store lock was poisoned: {error}")))
  }

  pub fn recover_selected_checkpoint(&self) -> Result<IndexRecoveryOutcomeV1, IndexRecoveryErrorV1> {
    let mut store = self.store.lock().map_err(|error| {
      IndexRecoveryErrorV1::Store(IndexRecoveryStoreErrorV1::new(
        "scope_ordinal_store_poisoned",
        format!("checkpoint store lock was poisoned during runtime recovery: {error}"),
      ))
    })?;
    recover_index_checkpoint_v1(&mut *store, self.hash_algorithm, &self.owner, self.recovery_options, &self.memory, &self.cancellation)
  }

  fn load_selected_checkpoint(&self, store: &mut Store) -> Result<SelectedCheckpointV1, SelectedLoadErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(SelectedLoadErrorV1::State(retryable(
        "scope_ordinal_store_cancelled",
        "checkpoint recovery was cancelled before selection",
      )));
    }
    let recovered =
      recover_index_checkpoint_v1(store, self.hash_algorithm, &self.owner, self.recovery_options, &self.memory, &self.cancellation)
        .map_err(|error| SelectedLoadErrorV1::State(map_recovery_error(error)))?;
    let recovered = match recovered {
      IndexRecoveryOutcomeV1::Resumable(recovered) => recovered,
      IndexRecoveryOutcomeV1::ReconciliationRequired { reason, evidence } => {
        return Err(SelectedLoadErrorV1::State(corrupt(
          "scope_ordinal_checkpoint_reconciliation",
          format!("selected checkpoint requires {reason:?} reconciliation: {evidence:?}"),
        )));
      }
      IndexRecoveryOutcomeV1::Canceled => {
        return Err(SelectedLoadErrorV1::State(retryable("scope_ordinal_store_cancelled", "checkpoint recovery was cancelled")));
      }
    };
    let root = IndexCheckpointRootV1::new(recovered.checkpoint_sequence, recovered.checkpoint_key.clone())
      .map_err(|error| SelectedLoadErrorV1::State(map_recovery_error(error)))?;
    let loaded = self.load_required(store, &root.checkpoint_key, "selected checkpoint")?;
    let checkpoint = decode_index_task_checkpoint(&loaded.bytes, self.hash_algorithm)
      .map_err(|error| SelectedLoadErrorV1::State(corrupt("scope_ordinal_checkpoint_format", error.to_string())))?;
    if checkpoint.key != root.checkpoint_key || checkpoint.checkpoint_sequence != root.checkpoint_sequence {
      return Err(SelectedLoadErrorV1::State(corrupt(
        "scope_ordinal_checkpoint_identity",
        "recovered checkpoint bytes disagree with the selected root",
      )));
    }
    let current =
      store.load_selected(&self.owner).map_err(|error| SelectedLoadErrorV1::State(retryable(error.code(), error.to_string())))?;
    if current.as_ref() != Some(&root) {
      return Err(SelectedLoadErrorV1::SelectionChanged);
    }
    Ok(SelectedCheckpointV1 { root, semantic_state_root: recovered.journal.semantic_state_root, loaded })
  }

  fn load_required(&self, store: &mut Store, key: &[u8], label: &'static str) -> Result<ReservedArtifactV1, SelectedLoadErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(SelectedLoadErrorV1::State(retryable("scope_ordinal_store_cancelled", format!("{label} load was cancelled"))));
    }
    let length = store
      .immutable_length(key)
      .map_err(|error| SelectedLoadErrorV1::State(retryable(error.code(), error.to_string())))?
      .ok_or_else(|| SelectedLoadErrorV1::State(corrupt("scope_ordinal_artifact_missing", format!("{label} is missing"))))?;
    if length == 0 || length > MAXIMUM_INDEX_ARTIFACT_BYTES {
      return Err(SelectedLoadErrorV1::State(corrupt(
        "scope_ordinal_artifact_bound",
        format!("{label} length {length} is outside 1..={MAXIMUM_INDEX_ARTIFACT_BYTES}"),
      )));
    }
    let reservation = self
      .memory
      .reserve(MemoryOwner::IndexDirtyBuffers, length, AdmissionClass::Maintenance)
      .map_err(|error| SelectedLoadErrorV1::State(retryable("scope_ordinal_memory_pressure", error.to_string())))?;
    let bytes = store
      .load_immutable(key, length)
      .map_err(|error| SelectedLoadErrorV1::State(retryable(error.code(), error.to_string())))?
      .ok_or_else(|| {
        SelectedLoadErrorV1::State(corrupt(
          "scope_ordinal_artifact_changed",
          format!("{label} disappeared or changed after its length probe"),
        ))
      })?;
    let actual = u64::try_from(bytes.len()).map_err(|error| {
      SelectedLoadErrorV1::State(corrupt("scope_ordinal_artifact_length", format!("{label} byte length cannot be represented: {error}")))
    })?;
    if actual != length {
      return Err(SelectedLoadErrorV1::State(corrupt(
        "scope_ordinal_artifact_changed",
        format!("{label} returned {actual} bytes after a {length}-byte probe"),
      )));
    }
    Ok(ReservedArtifactV1 { bytes, _reservation: reservation })
  }

  fn resolve_snapshot(
    &self,
    store: &mut Store,
    selected: &SelectedCheckpointV1,
    request: ScopeSnapshotRequestV1<'_>,
  ) -> Result<ScopeSnapshotV1, SelectedLoadErrorV1> {
    if request.scope_id != self.owner.index_id() {
      return Err(SelectedLoadErrorV1::State(corrupt("scope_ordinal_scope_owner", "requested scope does not match this checkpoint owner")));
    }
    if request.semantic_state_root != selected.semantic_state_root {
      return Err(SelectedLoadErrorV1::State(corrupt(
        "scope_ordinal_semantic_root",
        "selected journal semantic-state root disagrees with the requested semantic root",
      )));
    }
    let checkpoint = decode_index_task_checkpoint(&selected.loaded.bytes, self.hash_algorithm)
      .map_err(|error| SelectedLoadErrorV1::State(corrupt("scope_ordinal_checkpoint_format", error.to_string())))?;
    if checkpoint.primary_id != request.scope_id || checkpoint.next_document_ordinal == 0 {
      return Err(SelectedLoadErrorV1::State(corrupt(
        "scope_ordinal_checkpoint_scope",
        "selected checkpoint primary scope or next-document ordinal is invalid",
      )));
    }
    let resume = decode_scope_ordinal_claim_resume_v1(checkpoint.resume_key, self.hash_algorithm)
      .map_err(|error| SelectedLoadErrorV1::State(corrupt("scope_ordinal_resume_format", error.to_string())))?;
    let attachments = owned_attachments(&checkpoint)?;
    let manifest_attachment = one_role_attachment(&attachments, IndexTaskAttachmentRoleV1::CandidateScopeManifest, Some(request.scope_id))?
      .ok_or_else(|| {
        SelectedLoadErrorV1::State(corrupt("scope_ordinal_manifest_attachment", "selected checkpoint has no scope manifest attachment"))
      })?;
    let manifest = self.load_scope_manifest(store, &manifest_attachment, request.scope_id, checkpoint.generation)?;
    if resume.applied_through_sequence != manifest.coverage_publication_sequence {
      return Err(SelectedLoadErrorV1::State(corrupt(
        "scope_ordinal_resume_watermark",
        "pending-claim watermark disagrees with the selected scope manifest coverage",
      )));
    }
    if manifest.next_document_ordinal > checkpoint.next_document_ordinal {
      return Err(SelectedLoadErrorV1::State(corrupt(
        "scope_ordinal_manifest_high_water",
        "selected scope manifest advances beyond the checkpoint next-document ordinal",
      )));
    }
    self.validate_manifest_roots(store, &attachments, request.scope_id, &manifest)?;

    let before_live_ordinal = self.lookup_optional_reverse(store, request.scope_id, &manifest, request.before_file_key)?;
    let after_live_ordinal = if request.before_file_key.is_some() && request.before_file_key == request.after_file_key {
      before_live_ordinal
    } else {
      self.lookup_optional_reverse(store, request.scope_id, &manifest, request.after_file_key)?
    };
    let mut claims = Vec::new();
    claims
      .try_reserve_exact(resume.claims.len())
      .map_err(|error| SelectedLoadErrorV1::State(retryable("scope_ordinal_claim_allocation", error.to_string())))?;
    let mut prior_operation_claim = None;
    for claim in resume.claims {
      if claim.document_ordinal >= checkpoint.next_document_ordinal {
        return Err(SelectedLoadErrorV1::State(corrupt(
          "scope_ordinal_claim_high_water",
          "pending claim reaches or exceeds the selected next-document ordinal",
        )));
      }
      let owned = OwnedPendingClaimV1 {
        operation_id: claim.operation_id,
        request_fingerprint: claim.request_fingerprint.to_vec(),
        document_ordinal: claim.document_ordinal,
        source_publication_sequence: claim.source_publication_sequence,
      };
      if claim.operation_id == request.operation_id {
        prior_operation_claim = Some(IndexScopeOrdinalDurableClaimV1 {
          request_fingerprint: owned.request_fingerprint.clone(),
          document_ordinal: owned.document_ordinal,
          source_publication_sequence: owned.source_publication_sequence,
        });
      }
      claims.push(owned);
    }
    let pending_claim_count = u32::try_from(claims.len()).map_err(|error| {
      SelectedLoadErrorV1::State(corrupt("scope_ordinal_claim_count", format!("pending claim count cannot be represented: {error}")))
    })?;
    Ok(ScopeSnapshotV1 {
      observation: IndexScopeOrdinalSelectedObservationV1 {
        checkpoint_sequence: selected.root.checkpoint_sequence,
        checkpoint_key: selected.root.checkpoint_key.clone(),
        generation: checkpoint.generation,
        scope_id: request.scope_id.to_vec(),
        semantic_state_root: request.semantic_state_root.to_vec(),
        next_document_ordinal: checkpoint.next_document_ordinal,
        prior_operation_claim,
        before_live_ordinal,
        after_live_ordinal,
        pending_claim_count,
      },
      applied_through_sequence: resume.applied_through_sequence,
      claims,
      attachments,
    })
  }

  fn load_scope_manifest(
    &self,
    store: &mut Store,
    attachment: &OwnedAttachmentV1,
    scope_id: &[u8],
    checkpoint_generation: u64,
  ) -> Result<ScopeManifestSnapshotV1, SelectedLoadErrorV1> {
    let loaded = self.load_required(store, &attachment.artifact_hash, "scope manifest")?;
    let manifest = decode_index_manifest(&loaded.bytes, self.hash_algorithm)
      .map_err(|error| SelectedLoadErrorV1::State(corrupt("scope_ordinal_manifest_format", error.to_string())))?;
    let IndexManifestBodyV1::ScopeCatalog(body) = manifest.details else {
      return Err(SelectedLoadErrorV1::State(corrupt(
        "scope_ordinal_manifest_kind",
        "candidate scope attachment is not a ScopeCatalog manifest",
      )));
    };
    if manifest.key != attachment.artifact_hash
      || manifest.owner_id != scope_id
      || manifest.generation != attachment.birth_generation
      || manifest.generation > checkpoint_generation
    {
      return Err(SelectedLoadErrorV1::State(corrupt(
        "scope_ordinal_manifest_identity",
        "scope manifest identity, owner, or generation disagrees with its selected attachment",
      )));
    }
    Ok(ScopeManifestSnapshotV1 {
      generation: manifest.generation,
      coverage_publication_sequence: body.coverage.coverage_publication_sequence,
      next_document_ordinal: body.next_document_ordinal,
      ordinal_directory_root: body.ordinal_directory_root.map(<[u8]>::to_vec),
      reverse_directory_root: body.reverse_directory_root.map(<[u8]>::to_vec),
      live_document_count: body.live_document_count,
      retained_tombstone_count: body.retained_tombstone_count,
      ordinal_page_count: body.ordinal_page_count,
      reverse_page_count: body.reverse_page_count,
    })
  }

  fn validate_manifest_roots(
    &self,
    store: &mut Store,
    attachments: &[OwnedAttachmentV1],
    scope_id: &[u8],
    manifest: &ScopeManifestSnapshotV1,
  ) -> Result<(), SelectedLoadErrorV1> {
    let ordinal_attachment = one_role_attachment(attachments, IndexTaskAttachmentRoleV1::ScopeOrdinalDirectoryRoot, Some(scope_id))?;
    let reverse_attachment = one_role_attachment(attachments, IndexTaskAttachmentRoleV1::ScopeReverseDirectoryRoot, Some(scope_id))?;
    match (&manifest.ordinal_directory_root, ordinal_attachment) {
      (Some(root), Some(attachment)) if root == &attachment.artifact_hash => {
        let directory = self.load_directory_root(store, &attachment, scope_id, OrderedIndexRoleV1::ScopeOrdinal, manifest.generation)?;
        if directory.live_count != manifest.live_document_count
          || directory.tombstone_count != manifest.retained_tombstone_count
          || directory.page_count != manifest.ordinal_page_count
        {
          return Err(SelectedLoadErrorV1::State(corrupt(
            "scope_ordinal_ordinal_root_aggregate",
            "scope ordinal root aggregates disagree with the selected manifest",
          )));
        }
      }
      (None, None) => {}
      _ => {
        return Err(SelectedLoadErrorV1::State(corrupt(
          "scope_ordinal_ordinal_root_attachment",
          "scope ordinal root and selected attachment disagree",
        )));
      }
    }
    match (&manifest.reverse_directory_root, reverse_attachment) {
      (Some(root), Some(attachment)) if root == &attachment.artifact_hash => {
        let directory = self.load_directory_root(store, &attachment, scope_id, OrderedIndexRoleV1::ScopeReverse, manifest.generation)?;
        if directory.live_count != manifest.live_document_count
          || directory.tombstone_count != 0
          || directory.page_count != manifest.reverse_page_count
        {
          return Err(SelectedLoadErrorV1::State(corrupt(
            "scope_ordinal_reverse_root_aggregate",
            "scope reverse root aggregates disagree with the selected manifest",
          )));
        }
      }
      (None, None) => {}
      _ => {
        return Err(SelectedLoadErrorV1::State(corrupt(
          "scope_ordinal_reverse_root_attachment",
          "scope reverse root and selected attachment disagree",
        )));
      }
    }
    Ok(())
  }

  fn load_directory_root(
    &self,
    store: &mut Store,
    attachment: &OwnedAttachmentV1,
    scope_id: &[u8],
    role: OrderedIndexRoleV1,
    maximum_generation: u64,
  ) -> Result<OwnedDirectorySummaryV1, SelectedLoadErrorV1> {
    let loaded = self.load_required(store, &attachment.artifact_hash, "scope directory root")?;
    let directory = decode_artifact_directory(&loaded.bytes, self.hash_algorithm)
      .map_err(|error| SelectedLoadErrorV1::State(corrupt("scope_ordinal_directory_format", error.to_string())))?;
    validate_root_directory(&directory, attachment, scope_id, role, maximum_generation)?;
    Ok(OwnedDirectorySummaryV1::from_directory(&directory))
  }

  fn lookup_optional_reverse(
    &self,
    store: &mut Store,
    scope_id: &[u8],
    manifest: &ScopeManifestSnapshotV1,
    file_key: Option<&[u8]>,
  ) -> Result<Option<u64>, SelectedLoadErrorV1> {
    let Some(file_key) = file_key else {
      return Ok(None);
    };
    if file_key.len() != self.hash_algorithm.hash_length() || file_key.iter().all(|byte| *byte == 0) {
      return Err(SelectedLoadErrorV1::State(corrupt(
        "scope_ordinal_file_key",
        "reverse lookup FileKey is zero or has the wrong hash width",
      )));
    }
    let Some(root) = manifest.reverse_directory_root.as_deref() else {
      return Ok(None);
    };
    self.lookup_reverse(store, scope_id, manifest.generation, root, file_key)
  }

  fn lookup_reverse(
    &self,
    store: &mut Store,
    scope_id: &[u8],
    maximum_generation: u64,
    root: &[u8],
    file_key: &[u8],
  ) -> Result<Option<u64>, SelectedLoadErrorV1> {
    let mut expected = ChildExpectationV1::root(root.to_vec(), maximum_generation);
    loop {
      let loaded = self.load_required(store, &expected.child_hash, "scope reverse directory path")?;
      let directory = decode_artifact_directory(&loaded.bytes, self.hash_algorithm)
        .map_err(|error| SelectedLoadErrorV1::State(corrupt("scope_ordinal_directory_format", error.to_string())))?;
      validate_directory_path_node(&directory, scope_id, &expected)?;
      let Some(entry) = select_directory_entry(&directory, file_key) else {
        return Ok(None);
      };
      let child = ChildExpectationV1::from_entry(&directory, entry);
      if directory.level != 0 {
        expected = child;
        continue;
      }
      drop(directory);
      drop(loaded);
      let page_loaded = self.load_required(store, &child.child_hash, "scope reverse page")?;
      let page = decode_ordered_page(&page_loaded.bytes, self.hash_algorithm)
        .map_err(|error| SelectedLoadErrorV1::State(corrupt("scope_ordinal_reverse_page_format", error.to_string())))?;
      validate_reverse_page(&page, scope_id, &child)?;
      for record in page.records.iter() {
        let record =
          record.map_err(|error| SelectedLoadErrorV1::State(corrupt("scope_ordinal_reverse_record_format", error.to_string())))?;
        let Some(record_file_key) = record.file_key else {
          return Err(SelectedLoadErrorV1::State(corrupt(
            "scope_ordinal_reverse_record_identity",
            "decoded scope reverse record has no FileKey",
          )));
        };
        match record_file_key.cmp(file_key) {
          Ordering::Less => {}
          Ordering::Equal => return Ok(Some(record.document_ordinal)),
          Ordering::Greater => return Ok(None),
        }
      }
      return Ok(None);
    }
  }

  fn publish_claim(
    &self,
    store: &mut Store,
    request: IndexScopeOrdinalPublishRequestV1<'_>,
  ) -> Result<IndexScopeOrdinalPublishOutcomeV1, IndexScopeOrdinalStateStoreErrorV1> {
    let selected = match self.load_selected_checkpoint(store) {
      Ok(selected) => selected,
      Err(SelectedLoadErrorV1::SelectionChanged) => return Ok(IndexScopeOrdinalPublishOutcomeV1::SelectionChanged),
      Err(SelectedLoadErrorV1::State(error)) => return Err(error),
    };
    if selected.root.checkpoint_sequence != request.expected_checkpoint_sequence
      || selected.root.checkpoint_key != request.expected_checkpoint_key
    {
      return Ok(IndexScopeOrdinalPublishOutcomeV1::SelectionChanged);
    }
    let snapshot = self
      .resolve_snapshot(
        store,
        &selected,
        ScopeSnapshotRequestV1 {
          scope_id: request.scope_id,
          semantic_state_root: request.semantic_state_root,
          operation_id: request.operation_id,
          before_file_key: None,
          after_file_key: None,
        },
      )
      .map_err(map_selected_state_error)?;
    if snapshot.observation.generation != request.generation {
      return Err(corrupt("scope_ordinal_publish_generation", "publish request generation disagrees with the selected checkpoint"));
    }
    if let Some(prior) = &snapshot.observation.prior_operation_claim {
      if prior.request_fingerprint == request.request_fingerprint
        && prior.document_ordinal == request.document_ordinal
        && prior.source_publication_sequence == request.source_publication_sequence
        && snapshot.observation.next_document_ordinal >= request.next_document_ordinal
      {
        return Ok(IndexScopeOrdinalPublishOutcomeV1::Committed);
      }
      return Err(corrupt(
        "scope_ordinal_publish_operation_conflict",
        "selected checkpoint already contains a different claim for this operation",
      ));
    }
    validate_publish_advance(&snapshot, request)?;
    let maximum = usize::try_from(MAX_SCOPE_ORDINAL_PENDING_CLAIMS_V1)
      .map_err(|error| corrupt("scope_ordinal_claim_count", format!("pending claim limit conversion failed: {error}")))?;
    if snapshot.claims.len() >= maximum {
      return Err(retryable("scope_ordinal_claim_pressure", "selected checkpoint reached the hard pending-claim limit"));
    }
    let mut claims = snapshot.claims;
    let insert_at = match claims.binary_search_by_key(&request.operation_id, |claim| claim.operation_id) {
      Ok(_) => {
        return Err(corrupt(
          "scope_ordinal_publish_operation_duplicate",
          "selected pending claims contain an operation that was not resolved as the prior claim",
        ));
      }
      Err(index) => index,
    };
    claims.insert(
      insert_at,
      OwnedPendingClaimV1 {
        operation_id: request.operation_id,
        request_fingerprint: request.request_fingerprint.to_vec(),
        document_ordinal: request.document_ordinal,
        source_publication_sequence: request.source_publication_sequence,
      },
    );
    let claim_writes = claims.iter().map(OwnedPendingClaimV1::as_write).collect::<Vec<_>>();
    let resume = encode_scope_ordinal_claim_resume_v1(self.hash_algorithm, snapshot.applied_through_sequence, &claim_writes)
      .map_err(|error| corrupt("scope_ordinal_resume_encode", error.to_string()))?;

    let checkpoint = decode_index_task_checkpoint(&selected.loaded.bytes, self.hash_algorithm)
      .map_err(|error| corrupt("scope_ordinal_checkpoint_format", error.to_string()))?;
    let next_sequence = checkpoint
      .checkpoint_sequence
      .checked_add(1)
      .ok_or_else(|| corrupt("scope_ordinal_checkpoint_sequence", "checkpoint sequence is exhausted"))?;
    let mut capabilities = [0; 32];
    capabilities.copy_from_slice(checkpoint.required_capabilities);
    let attachment_writes = snapshot.attachments.iter().map(OwnedAttachmentV1::as_write).collect::<Vec<_>>();
    let external = checkpoint.external.map(|external| ExternalWorkspaceDescriptorWriteV1 {
      workspace_id: external.workspace_id,
      manifest_digest: external.manifest_digest,
      durable_sequence: external.durable_sequence,
      durable_bytes: external.durable_bytes,
      path: external.path,
    });
    let updated_at_ms = checkpoint.updated_at_ms.max(self.clock.now_ms());
    let next = encode_index_task_checkpoint(&IndexTaskCheckpointWriteV1 {
      hash_algorithm: self.hash_algorithm,
      task_id: checkpoint.task_id,
      checkpoint_sequence: next_sequence,
      generation: checkpoint.generation,
      task_kind: checkpoint.task_kind,
      state: checkpoint.state,
      phase: checkpoint.phase,
      required_capabilities: &capabilities,
      started_at_ms: checkpoint.started_at_ms,
      updated_at_ms,
      source_root: checkpoint.source_root,
      target_root: optional_hash(checkpoint.target_root),
      primary_id: optional_hash(checkpoint.primary_id),
      journal_head: optional_hash(checkpoint.journal_head),
      journal_floor_sequence: checkpoint.journal_floor_sequence,
      journal_audited_through: checkpoint.journal_audited_through,
      next_document_ordinal: request.next_document_ordinal,
      completed_work: checkpoint.completed_work,
      total_work_hint: checkpoint.total_work_hint,
      resume_key: &resume,
      attachments: &attachment_writes,
      external,
    })
    .map_err(|error| corrupt("scope_ordinal_checkpoint_encode", error.to_string()))?;
    let next_root = IndexCheckpointRootV1::new(next_sequence, next.key.clone()).map_err(map_recovery_error)?;
    let publication = publish_index_recovery_checkpoint_v1(
      store,
      IndexRecoveryPublicationRequestV1 {
        hash_algorithm: self.hash_algorithm,
        owner: &self.owner,
        expected: Some(&selected.root),
        checkpoint: &next,
        dependencies: &[],
        options: self.recovery_options,
        memory: &self.memory,
        cancellation: &self.cancellation,
      },
    );
    match publication {
      Ok(_) => Ok(IndexScopeOrdinalPublishOutcomeV1::Committed),
      Err(publication_error) => self.resolve_publication_uncertainty(store, request, &selected.root, &next_root, publication_error),
    }
  }

  fn resolve_publication_uncertainty(
    &self,
    store: &mut Store,
    request: IndexScopeOrdinalPublishRequestV1<'_>,
    expected: &IndexCheckpointRootV1,
    next: &IndexCheckpointRootV1,
    publication_error: IndexRecoveryErrorV1,
  ) -> Result<IndexScopeOrdinalPublishOutcomeV1, IndexScopeOrdinalStateStoreErrorV1> {
    let reopened = match self.load_selected_checkpoint(store) {
      Ok(reopened) => reopened,
      Err(SelectedLoadErrorV1::SelectionChanged) => return Ok(IndexScopeOrdinalPublishOutcomeV1::SelectionChanged),
      Err(SelectedLoadErrorV1::State(reopen_error)) => {
        return Err(retryable(
          "scope_ordinal_commit_unknown",
          format!("checkpoint publication failed ({publication_error}); selected-state reopen also failed ({reopen_error:?})"),
        ));
      }
    };
    if reopened.root == *expected {
      return Err(map_recovery_error(publication_error));
    }
    if reopened.root != *next {
      return Ok(IndexScopeOrdinalPublishOutcomeV1::SelectionChanged);
    }
    let snapshot = self
      .resolve_snapshot(
        store,
        &reopened,
        ScopeSnapshotRequestV1 {
          scope_id: request.scope_id,
          semantic_state_root: request.semantic_state_root,
          operation_id: request.operation_id,
          before_file_key: None,
          after_file_key: None,
        },
      )
      .map_err(map_selected_state_error)?;
    let Some(claim) = snapshot.observation.prior_operation_claim else {
      return Err(corrupt(
        "scope_ordinal_commit_unknown_claim",
        "commit-unknown successor was selected without the requested pending claim",
      ));
    };
    if claim.request_fingerprint != request.request_fingerprint
      || claim.document_ordinal != request.document_ordinal
      || claim.source_publication_sequence != request.source_publication_sequence
      || snapshot.observation.next_document_ordinal < request.next_document_ordinal
    {
      return Err(corrupt(
        "scope_ordinal_commit_unknown_claim",
        "commit-unknown successor selected a claim that disagrees with the request",
      ));
    }
    Ok(IndexScopeOrdinalPublishOutcomeV1::Committed)
  }
}

impl<Store> IndexScopeOrdinalStateStoreV1 for RecoveryIndexScopeOrdinalStateStoreV1<Store>
where
  Store: IndexRecoveryStoreV1 + Send,
{
  fn observe_selected(
    &self,
    request: IndexScopeOrdinalStoreObservationRequestV1<'_>,
  ) -> Result<IndexScopeOrdinalSelectedObservationV1, IndexScopeOrdinalStateStoreErrorV1> {
    validate_observation_request(self.hash_algorithm, &self.owner, request)?;
    let mut store = self.lock_store()?;
    let selected = match self.load_selected_checkpoint(&mut *store) {
      Ok(selected) => selected,
      Err(SelectedLoadErrorV1::SelectionChanged) => {
        return Err(retryable("scope_ordinal_selection_changed", "selected checkpoint changed while it was being observed"));
      }
      Err(SelectedLoadErrorV1::State(error)) => return Err(error),
    };
    self
      .resolve_snapshot(
        &mut *store,
        &selected,
        ScopeSnapshotRequestV1 {
          scope_id: request.scope_id,
          semantic_state_root: request.semantic_state_root,
          operation_id: request.operation_id,
          before_file_key: request.before_file_key,
          after_file_key: request.after_file_key,
        },
      )
      .map(|snapshot| snapshot.observation)
      .map_err(map_selected_state_error)
  }

  fn publish_selected_synced(
    &self,
    request: IndexScopeOrdinalPublishRequestV1<'_>,
  ) -> Result<IndexScopeOrdinalPublishOutcomeV1, IndexScopeOrdinalStateStoreErrorV1> {
    validate_publish_request(self.hash_algorithm, &self.owner, request)?;
    let mut store = self.lock_store()?;
    self.publish_claim(&mut *store, request)
  }
}

struct ReservedArtifactV1 {
  bytes: Vec<u8>,
  _reservation: MemoryReservation,
}

struct SelectedCheckpointV1 {
  root: IndexCheckpointRootV1,
  semantic_state_root: Vec<u8>,
  loaded: ReservedArtifactV1,
}

enum SelectedLoadErrorV1 {
  SelectionChanged,
  State(IndexScopeOrdinalStateStoreErrorV1),
}

#[derive(Clone)]
struct OwnedAttachmentV1 {
  role: IndexTaskAttachmentRoleV1,
  owner_id: Vec<u8>,
  artifact_hash: Vec<u8>,
  birth_generation: u64,
}

impl OwnedAttachmentV1 {
  fn as_write(&self) -> IndexTaskAttachmentWriteV1<'_> {
    IndexTaskAttachmentWriteV1 {
      role: self.role,
      owner_id: &self.owner_id,
      artifact_hash: &self.artifact_hash,
      birth_generation: self.birth_generation,
    }
  }
}

struct OwnedPendingClaimV1 {
  operation_id: [u8; 16],
  request_fingerprint: Vec<u8>,
  document_ordinal: u64,
  source_publication_sequence: u64,
}

impl OwnedPendingClaimV1 {
  fn as_write(&self) -> ScopeOrdinalPendingClaimWriteV1<'_> {
    ScopeOrdinalPendingClaimWriteV1 {
      operation_id: self.operation_id,
      request_fingerprint: &self.request_fingerprint,
      document_ordinal: self.document_ordinal,
      source_publication_sequence: self.source_publication_sequence,
    }
  }
}

struct ScopeSnapshotV1 {
  observation: IndexScopeOrdinalSelectedObservationV1,
  applied_through_sequence: u64,
  claims: Vec<OwnedPendingClaimV1>,
  attachments: Vec<OwnedAttachmentV1>,
}

#[derive(Clone, Copy)]
struct ScopeSnapshotRequestV1<'a> {
  scope_id: &'a [u8],
  semantic_state_root: &'a [u8],
  operation_id: [u8; 16],
  before_file_key: Option<&'a [u8]>,
  after_file_key: Option<&'a [u8]>,
}

struct ScopeManifestSnapshotV1 {
  generation: u64,
  coverage_publication_sequence: u64,
  next_document_ordinal: u64,
  ordinal_directory_root: Option<Vec<u8>>,
  reverse_directory_root: Option<Vec<u8>>,
  live_document_count: u64,
  retained_tombstone_count: u64,
  ordinal_page_count: u64,
  reverse_page_count: u64,
}

struct OwnedDirectorySummaryV1 {
  live_count: u64,
  tombstone_count: u64,
  page_count: u64,
}

impl OwnedDirectorySummaryV1 {
  fn from_directory(directory: &ArtifactDirectoryNodeV1<'_>) -> Self {
    Self { live_count: directory.live_count, tombstone_count: directory.tombstone_count, page_count: directory.page_count }
  }
}

struct ChildExpectationV1 {
  child_hash: Vec<u8>,
  child_generation: u64,
  exact_generation: bool,
  child_level: Option<u16>,
  lower_fence: Option<Vec<u8>>,
  upper_fence: Option<Vec<u8>>,
  live_count: Option<u64>,
  tombstone_count: Option<u64>,
  page_count: Option<u64>,
  logical_bytes: Option<u64>,
  minimum_page_id: Option<u64>,
  maximum_page_id: Option<u64>,
}

impl ChildExpectationV1 {
  fn root(child_hash: Vec<u8>, maximum_generation: u64) -> Self {
    Self {
      child_hash,
      child_generation: maximum_generation,
      exact_generation: false,
      child_level: None,
      lower_fence: None,
      upper_fence: None,
      live_count: None,
      tombstone_count: None,
      page_count: None,
      logical_bytes: None,
      minimum_page_id: None,
      maximum_page_id: None,
    }
  }

  fn from_entry(directory: &ArtifactDirectoryNodeV1<'_>, entry: &super::index_page::ArtifactDirectoryEntryV1<'_>) -> Self {
    Self {
      child_hash: entry.child_hash.to_vec(),
      child_generation: entry.child_generation,
      exact_generation: true,
      child_level: directory.level.checked_sub(1),
      lower_fence: Some(entry.lower_fence.to_vec()),
      upper_fence: Some(entry.upper_fence.to_vec()),
      live_count: Some(entry.live_count),
      tombstone_count: Some(entry.tombstone_count),
      page_count: Some(entry.page_count),
      logical_bytes: Some(entry.logical_bytes),
      minimum_page_id: Some(entry.minimum_page_id),
      maximum_page_id: Some(entry.maximum_page_id),
    }
  }
}

fn owned_attachments(checkpoint: &IndexTaskCheckpointV1<'_>) -> Result<Vec<OwnedAttachmentV1>, SelectedLoadErrorV1> {
  let mut attachments = Vec::new();
  attachments
    .try_reserve_exact(checkpoint.attachments.len())
    .map_err(|error| SelectedLoadErrorV1::State(retryable("scope_ordinal_attachment_allocation", error.to_string())))?;
  for attachment in checkpoint.attachments.iter() {
    let attachment =
      attachment.map_err(|error| SelectedLoadErrorV1::State(corrupt("scope_ordinal_attachment_format", error.to_string())))?;
    attachments.push(OwnedAttachmentV1 {
      role: attachment.role,
      owner_id: attachment.owner_id.to_vec(),
      artifact_hash: attachment.artifact_hash.to_vec(),
      birth_generation: attachment.birth_generation,
    });
  }
  Ok(attachments)
}

fn one_role_attachment(
  attachments: &[OwnedAttachmentV1],
  role: IndexTaskAttachmentRoleV1,
  expected_owner: Option<&[u8]>,
) -> Result<Option<OwnedAttachmentV1>, SelectedLoadErrorV1> {
  let mut selected = None;
  for attachment in attachments.iter().filter(|attachment| attachment.role == role) {
    if expected_owner.is_some_and(|owner| attachment.owner_id != owner) || selected.is_some() {
      return Err(SelectedLoadErrorV1::State(corrupt(
        "scope_ordinal_attachment_closure",
        format!("checkpoint {} attachment cardinality or owner is invalid", role.name()),
      )));
    }
    selected = Some(attachment.clone());
  }
  Ok(selected)
}

fn validate_root_directory(
  directory: &ArtifactDirectoryNodeV1<'_>,
  attachment: &OwnedAttachmentV1,
  scope_id: &[u8],
  role: OrderedIndexRoleV1,
  maximum_generation: u64,
) -> Result<(), SelectedLoadErrorV1> {
  if directory.key != attachment.artifact_hash
    || directory.owner_id != scope_id
    || directory.role != role
    || directory.generation != attachment.birth_generation
    || directory.generation > maximum_generation
  {
    return Err(SelectedLoadErrorV1::State(corrupt(
      "scope_ordinal_directory_identity",
      "scope directory root identity, owner, role, or generation disagrees with its attachment",
    )));
  }
  Ok(())
}

fn validate_directory_path_node(
  directory: &ArtifactDirectoryNodeV1<'_>,
  scope_id: &[u8],
  expected: &ChildExpectationV1,
) -> Result<(), SelectedLoadErrorV1> {
  if directory.key != expected.child_hash
    || directory.owner_id != scope_id
    || directory.role != OrderedIndexRoleV1::ScopeReverse
    || expected.exact_generation && directory.generation != expected.child_generation
    || !expected.exact_generation && directory.generation > expected.child_generation
    || expected.child_level.is_some_and(|level| directory.level != level)
    || expected.lower_fence.as_deref().is_some_and(|fence| directory.lower_fence != fence)
    || expected.upper_fence.as_deref().is_some_and(|fence| directory.upper_fence != fence)
    || expected.live_count.is_some_and(|count| directory.live_count != count)
    || expected.tombstone_count.is_some_and(|count| directory.tombstone_count != count)
    || expected.page_count.is_some_and(|count| directory.page_count != count)
    || expected.logical_bytes.is_some_and(|bytes| directory.logical_bytes != bytes)
    || expected.minimum_page_id.is_some_and(|page| directory.minimum_page_id != page)
    || expected.maximum_page_id.is_some_and(|page| directory.maximum_page_id != page)
  {
    return Err(SelectedLoadErrorV1::State(corrupt(
      "scope_ordinal_directory_child_closure",
      "scope reverse directory child disagrees with its exact parent descriptor",
    )));
  }
  Ok(())
}

fn select_directory_entry<'a>(
  directory: &'a ArtifactDirectoryNodeV1<'a>,
  file_key: &[u8],
) -> Option<&'a super::index_page::ArtifactDirectoryEntryV1<'a>> {
  directory.entries.iter().find(|entry| entry.lower_fence <= file_key && file_key <= entry.upper_fence)
}

fn validate_reverse_page(page: &OrderedPageV1<'_>, scope_id: &[u8], expected: &ChildExpectationV1) -> Result<(), SelectedLoadErrorV1> {
  if page.key != expected.child_hash
    || page.owner_id != scope_id
    || page.role != OrderedIndexRoleV1::ScopeReverse
    || page.generation != expected.child_generation
    || expected.lower_fence.as_deref() != Some(page.lower_fence)
    || expected.upper_fence.as_deref() != Some(page.upper_fence)
    || expected.live_count != Some(u64::from(page.live_count))
    || expected.tombstone_count != Some(u64::from(page.tombstone_count))
    || expected.page_count != Some(1)
    || expected.logical_bytes != Some(page.logical_live_bytes)
    || expected.minimum_page_id != Some(page.page_id)
    || expected.maximum_page_id != Some(page.page_id)
  {
    return Err(SelectedLoadErrorV1::State(corrupt(
      "scope_ordinal_reverse_page_closure",
      "scope reverse page disagrees with its exact leaf descriptor",
    )));
  }
  Ok(())
}

fn validate_publish_advance(
  snapshot: &ScopeSnapshotV1,
  request: IndexScopeOrdinalPublishRequestV1<'_>,
) -> Result<(), IndexScopeOrdinalStateStoreErrorV1> {
  let current = snapshot.observation.next_document_ordinal;
  let valid = if request.document_ordinal < current {
    request.next_document_ordinal == current
  } else if request.document_ordinal == current {
    current.checked_add(1) == Some(request.next_document_ordinal)
  } else {
    false
  };
  if !valid || request.source_publication_sequence <= snapshot.applied_through_sequence {
    return Err(corrupt(
      "scope_ordinal_publish_advance",
      "publish request ordinal, high-water, or source sequence is not the exact selected successor",
    ));
  }
  Ok(())
}

fn validate_observation_request(
  hash_algorithm: HashAlgorithm,
  owner: &IndexRecoveryOwnerV1,
  request: IndexScopeOrdinalStoreObservationRequestV1<'_>,
) -> Result<(), IndexScopeOrdinalStateStoreErrorV1> {
  let width = hash_algorithm.hash_length();
  if request.scope_id != owner.index_id()
    || !valid_hash(request.scope_id, width)
    || !valid_hash(request.semantic_state_root, width)
    || request.operation_id.iter().all(|byte| *byte == 0)
    || request.before_file_key.is_some_and(|key| !valid_hash(key, width))
    || request.after_file_key.is_some_and(|key| !valid_hash(key, width))
  {
    return Err(corrupt(
      "scope_ordinal_observation_identity",
      "observation scope, semantic root, operation, or FileKey is zero, foreign, or has the wrong hash width",
    ));
  }
  Ok(())
}

fn validate_publish_request(
  hash_algorithm: HashAlgorithm,
  owner: &IndexRecoveryOwnerV1,
  request: IndexScopeOrdinalPublishRequestV1<'_>,
) -> Result<(), IndexScopeOrdinalStateStoreErrorV1> {
  let width = hash_algorithm.hash_length();
  if request.expected_checkpoint_sequence == 0
    || !valid_hash(request.expected_checkpoint_key, width)
    || request.generation == 0
    || request.scope_id != owner.index_id()
    || !valid_hash(request.scope_id, width)
    || !valid_hash(request.semantic_state_root, width)
    || request.operation_id.iter().all(|byte| *byte == 0)
    || !valid_hash(request.request_fingerprint, width)
    || request.document_ordinal == 0
    || request.next_document_ordinal <= request.document_ordinal
    || request.source_publication_sequence == 0
  {
    return Err(corrupt(
      "scope_ordinal_publish_identity",
      "publish root, scope, generation, operation, fingerprint, ordinal, or source sequence is zero, foreign, or has the wrong width",
    ));
  }
  Ok(())
}

fn valid_hash(hash: &[u8], width: usize) -> bool {
  hash.len() == width && hash.iter().any(|byte| *byte != 0)
}

fn optional_hash(hash: &[u8]) -> Option<&[u8]> {
  if hash.iter().all(|byte| *byte == 0) {
    None
  } else {
    Some(hash)
  }
}

fn map_selected_state_error(error: SelectedLoadErrorV1) -> IndexScopeOrdinalStateStoreErrorV1 {
  match error {
    SelectedLoadErrorV1::SelectionChanged => {
      retryable("scope_ordinal_selection_changed", "selected checkpoint changed while its scope state was being resolved")
    }
    SelectedLoadErrorV1::State(error) => error,
  }
}

fn map_recovery_error(error: IndexRecoveryErrorV1) -> IndexScopeOrdinalStateStoreErrorV1 {
  let context = error.to_string();
  match error {
    IndexRecoveryErrorV1::Canceled => retryable("scope_ordinal_store_cancelled", context),
    IndexRecoveryErrorV1::Memory(_) | IndexRecoveryErrorV1::Store(_) => retryable("scope_ordinal_recovery_retryable", context),
    IndexRecoveryErrorV1::Invalid(_)
    | IndexRecoveryErrorV1::Arithmetic(_)
    | IndexRecoveryErrorV1::ReconciliationRequired { .. }
    | IndexRecoveryErrorV1::Format(_)
    | IndexRecoveryErrorV1::Coverage(_) => corrupt("scope_ordinal_recovery_corrupt", context),
  }
}

fn retryable(code: &'static str, context: impl Into<String>) -> IndexScopeOrdinalStateStoreErrorV1 {
  IndexScopeOrdinalStateStoreErrorV1::retryable(code, context)
}

fn corrupt(code: &'static str, context: impl Into<String>) -> IndexScopeOrdinalStateStoreErrorV1 {
  IndexScopeOrdinalStateStoreErrorV1::corrupt(code, context)
}
